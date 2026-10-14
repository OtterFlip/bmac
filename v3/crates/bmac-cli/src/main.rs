// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

//! The `bmac` command for hosts.

use std::path::Path;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use bmac_adapters::proxmox::Pvesh;
use bmac_adapters::qga::GuestAgent;
use bmac_cli::args::{self, Command};
use bmac_cli::commands::{self, Context, TerminalEnv};
use bmac_cli::guests::{self, GuestHostKey, GuestList, HOST_KEY_FILE};
use bmac_cli::session::{CliError, Result};
use bmac_cli::terminal::{self, Prompter, Tty};
use bmac_cli::{capacity, luks};
use bmac_core::HostSlot;
use bmac_exec::RealRunner;
use bmac_proto::engine::ENGINE_SOCKET;
use bmacd::cluster::{ClusterView, PveCluster};
use bmacd::engine::listen;
use bmacd::settings::{RUN_CONFIG_DIR, Settings};
use bmacd::supervisor::SupervisorConfig;
use bmacd::web::client::EngineClient;
use serde::Serialize;

fn print_json(value: &impl Serialize) {
    println!(
        "{}",
        serde_json::to_string_pretty(value).unwrap_or_else(|_| "null".to_string())
    );
}

fn require_root(what: &str) -> Result<()> {
    if rustix::process::geteuid().is_root() {
        Ok(())
    } else {
        Err(CliError::Message(format!("bmac {what} must run as root")))
    }
}

fn context() -> Result<Context> {
    let registry = bmacd::operations::Registry::builtin().map_err(CliError::Message)?;
    let scripts = SupervisorConfig::system(Arc::new(|| 0), Arc::new(|_: &str| true));
    let settings = Settings::system();
    Ok(Context {
        client: Arc::new(EngineClient::new(ENGINE_SOCKET)),
        poll: Duration::from_millis(500),
        handle_interrupt: true,
        terminal: TerminalEnv {
            root: scripts.root,
            bash: scripts.bash,
            env: scripts.env,
            prepare: Box::new(move || settings.render_to(Path::new(RUN_CONFIG_DIR))),
            registry: Arc::new(registry),
        },
    })
}

async fn guest_rows() -> Result<Vec<guests::GuestRow>> {
    let resources = bmac_registry::Registry::system()
        .resources()
        .map_err(|e| CliError::Message(format!("cannot read the registry: {e}")))?;
    let live = Pvesh::new(&RealRunner)
        .guests()
        .await
        .map_err(|e| CliError::Message(e.to_string()))?;
    Ok(guests::guest_rows(&resources, &live))
}

fn luks_enter(prompter: &mut Tty) -> Result<i32> {
    require_root("luks-enter")?;
    luks::at_console(|name| std::env::var(name).ok())?;
    if !terminal::stdin_is_terminal() {
        return Err(CliError::Message(
            "bmac luks-enter reads the passphrase from the console".to_string(),
        ));
    }
    let first = prompter.secret("LUKS passphrase: ")?.unwrap_or_default();
    let second = prompter.secret("Again: ")?.unwrap_or_default();
    if first != second {
        return Err(CliError::Message(
            "the passphrases differ; nothing was stored".to_string(),
        ));
    }
    luks::store(Path::new(luks::PASSPHRASE_FILE), first.as_bytes())?;
    prompter.say(
        "Stored for the job that asked for it, which deletes it once the disks are \
         prepared. Go back to the Dashboard and continue the job.",
    );
    Ok(0)
}

async fn execute(command: Command) -> Result<i32> {
    let mut tty = Tty;
    match command {
        Command::Version => {
            println!("bmac {}", bmacd::VERSION);
            Ok(0)
        }
        Command::Help => {
            println!("{}", args::USAGE);
            Ok(0)
        }
        Command::Status { json } => {
            let client = EngineClient::new(ENGINE_SOCKET);
            let status = commands::status_from(
                listen::node_name(),
                commands::engine_version(&client).await,
                PveCluster::system().membership(),
            );
            if json {
                print_json(&status);
            } else {
                print!("{}", commands::status_text(&status));
            }
            Ok(0)
        }
        Command::Jobs { json, user } => {
            require_root("jobs")?;
            commands::jobs(&context()?, &mut tty, json, user.as_deref()).await
        }
        Command::Follow { job, answer, user } => {
            require_root("jobs")?;
            commands::follow_job(&context()?, &mut tty, job, answer, user.as_deref()).await
        }
        Command::Cancel { job, force, user } => {
            require_root("cancel")?;
            commands::cancel(&context()?, &mut tty, job, force, user.as_deref()).await
        }
        Command::Run(run) => {
            require_root("run")?;
            commands::run(&context()?, &mut tty, run).await
        }
        Command::LuksEnter => luks_enter(&mut tty),
        Command::Guests { json } => {
            require_root("guests")?;
            let rows = guest_rows().await?;
            if json {
                let hosts = Settings::system()
                    .hosts()
                    .map_err(|e| CliError::Message(e.message))?;
                let staging_limits =
                    guests::staging_limits(hosts.iter().map(|h| (h.host, &h.values)));
                print_json(&GuestList {
                    guests: rows,
                    staging_limits,
                });
            } else {
                print!("{}", guests::table(&rows));
            }
            Ok(0)
        }
        Command::GuestHostKey { guest, json } => {
            require_root("guest-host-key")?;
            let rows = guest_rows().await?;
            let (row, node) = guests::running_guest(&rows, &guest)?;
            let status = GuestAgent::new(&RealRunner)
                .exec(node, row.vmid, &["/bin/cat", HOST_KEY_FILE])
                .await
                .map_err(|e| CliError::Message(format!("cannot read {guest}'s host key: {e}")))?;
            let (key, fingerprint) = guests::attest(&status)?;
            let attested = GuestHostKey {
                guest: row.name.clone(),
                vmid: row.vmid,
                node,
                key,
                fingerprint,
            };
            if json {
                print_json(&attested);
            } else {
                println!(
                    "{} (VMID {}, on {})",
                    attested.guest, attested.vmid, attested.node
                );
                println!("{}", attested.key);
                println!("{}", attested.fingerprint);
            }
            Ok(0)
        }
        Command::Capacity { json, pool, zvol } => {
            require_root("capacity")?;
            let host: HostSlot = listen::node_name().parse().map_err(|_| {
                CliError::Message(format!("{} is not a BMAC host", listen::node_name()))
            })?;
            let pool = pool
                .parse()
                .map_err(|_| CliError::Message(format!("{pool} is not a pool name")))?;
            let capacity = capacity::collect(&RealRunner, host, &pool, zvol).await?;
            if json {
                print_json(&capacity);
            } else {
                println!(
                    "{} {}: {} bytes total, {} available, {} reserved, {} headroom",
                    capacity.host,
                    capacity.pool,
                    capacity.total,
                    capacity.space.available,
                    capacity.reserve,
                    capacity.headroom
                );
                if let Some(fit) = &capacity.fit {
                    println!(
                        "{}: {}",
                        if fit.fits { "fits" } else { "does not fit" },
                        fit.explanation
                    );
                }
            }
            Ok(if capacity.fit.as_ref().is_some_and(|f| !f.fits) {
                3
            } else {
                0
            })
        }
        Command::Growth {
            allocation,
            volsize,
            block,
        } => {
            let mut input = String::new();
            std::io::Read::read_to_string(&mut std::io::stdin(), &mut input)
                .map_err(|e| CliError::Message(format!("cannot read standard input: {e}")))?;
            let growth =
                capacity::growth(&input, allocation, volsize, block).map_err(CliError::Message)?;
            print_json(&growth);
            Ok(0)
        }
        Command::BlockSizes { offered } => {
            let mut input = String::new();
            std::io::Read::read_to_string(&mut std::io::stdin(), &mut input)
                .map_err(|e| CliError::Message(format!("cannot read standard input: {e}")))?;
            let sizes = capacity::block_sizes(&input, &offered).map_err(CliError::Message)?;
            print_json(&sizes);
            Ok(0)
        }
    }
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let command = match args::parse(&args) {
        Ok(command) => command,
        Err(message) => {
            eprintln!("bmac: {message}\n\n{}", args::USAGE);
            return ExitCode::from(2);
        }
    };
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(e) => {
            eprintln!("bmac: cannot start: {e}");
            return ExitCode::FAILURE;
        }
    };
    let result = runtime.block_on(execute(command));
    terminal::restore();
    match result {
        Ok(code) => ExitCode::from(u8::try_from(code).unwrap_or(1)),
        Err(error) => {
            eprintln!("bmac: {error}");
            ExitCode::FAILURE
        }
    }
}
