// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

//! `bmacd engine` (root) and `bmacd web` (`bmac-web`), each run by its own
//! systemd unit.

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;

use bmac_core::HostSlot;
use bmac_exec::RealRunner;
use bmac_proto::engine::ENGINE_SOCKET;
use bmacd::auth::{self, Auth, AuthConfig};
use bmacd::db::{DATABASE, Database};
use bmacd::engine::{Engine, listen, server};
use bmacd::peers::{self, AllowedPeers, WEB_USER};
use bmacd::web::client::EngineClient;
use bmacd::{
    VERSION, certificate, cluster, identity, jobs, log, mesh, metrics, notify, operations, records,
    runner, schedules, settings, status, supervisor, systemd, web,
};

/// How often expired and revoked sessions are ended.
const SESSION_UPKEEP: std::time::Duration = std::time::Duration::from_secs(10);
/// How often, and for how long, a starting engine tries to resume the jobs
/// that restarted the host: pmxcfs refuses changes until quorum is back.
const RECOVER_EVERY: std::time::Duration = std::time::Duration::from_secs(5);
const RECOVER_WITHIN: std::time::Duration = std::time::Duration::from_secs(30 * 60);
/// How often each host re-pins the hosts' private addresses in `/etc/hosts`.
const HOST_PINS: std::time::Duration = std::time::Duration::from_secs(60);
const SETUP_DONE_POLL: std::time::Duration = std::time::Duration::from_secs(5);
const JOB_UPKEEP: std::time::Duration = std::time::Duration::from_secs(10);
/// How long after a certificate switch the front end restarts, so the
/// switch's reply reaches the browser over the old certificate first.
const WEB_RESTART_DELAY: std::time::Duration = std::time::Duration::from_secs(1);
const WEB_UNIT: &str = "bmacd-web.service";
/// How soon a settings change made on any host reaches this host's
/// rendered script config.
const SCRIPT_CONFIG_REFRESH: std::time::Duration = std::time::Duration::from_secs(10);

const USAGE: &str = "usage: bmacd engine [--socket PATH] [--web-user NAME]
       bmacd web [--socket PATH]
       bmacd render-config
       bmacd native OPERATION --json [ARGS...]
       bmacd --version";

struct Options {
    socket: PathBuf,
    web_user: String,
}

fn parse(args: &[String]) -> Result<Options, String> {
    let mut options = Options {
        socket: PathBuf::from(ENGINE_SOCKET),
        web_user: WEB_USER.to_string(),
    };
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        let mut value = || {
            args.next()
                .cloned()
                .ok_or_else(|| format!("{arg} needs a value"))
        };
        match arg.as_str() {
            "--socket" => options.socket = PathBuf::from(value()?),
            "--web-user" => options.web_user = value()?,
            other => return Err(format!("unknown option {other}")),
        }
    }
    Ok(options)
}

async fn terminated() {
    use tokio::signal::unix::{SignalKind, signal};
    let (Ok(mut term), Ok(mut int)) = (
        signal(SignalKind::terminate()),
        signal(SignalKind::interrupt()),
    ) else {
        std::future::pending::<()>().await;
        return;
    };
    tokio::select! {
        _ = term.recv() => {}
        _ = int.recv() => {}
    }
}

/// What a host runs is chosen at startup, and setup mode leaves out the
/// peer links, join bundles, and host pins. Once setup has finished and its
/// job's end is recorded, bmacd restarts as a cluster member. The restart is
/// queued without waiting, since the stop it asks for ends this process.
fn restart_after_setup(
    runner: Arc<runner::Runner>,
    paths: bmacd::native::setup::Paths,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(SETUP_DONE_POLL).await;
            if bmacd::native::setup::in_setup_mode(&paths) || !runner.is_idle() {
                continue;
            }
            log::info("setup is finished; restarting as a cluster member");
            let restarted = tokio::process::Command::new(bmac_adapters::programs::SYSTEMCTL)
                .args(["--no-block", "restart", "bmacd.service", WEB_UNIT])
                .status()
                .await;
            match restarted {
                Ok(status) if status.success() => return,
                Ok(status) => log::error(&format!("cannot restart bmacd: systemctl {status}")),
                Err(e) => log::error(&format!("cannot restart bmacd: {e}")),
            }
        }
    })
}

/// After a certificate switch: publish the new fingerprint, then restart
/// the front end, which reads its certificate only at startup.
fn after_switch() -> certificate::AfterSwitch {
    let runtime = tokio::runtime::Handle::current();
    Arc::new(move || {
        runtime.spawn(async {
            if let Ok(host) = listen::node_name().parse::<HostSlot>() {
                let now = auth::system_clock()();
                let published = tokio::task::spawn_blocking(move || {
                    identity::Identities::system().publish(
                        host,
                        identity::HTTPS_CERT.as_ref(),
                        identity::SSH_HOST_KEY.as_ref(),
                        None,
                        now,
                    )
                })
                .await;
                if let Ok(Err(e)) = published {
                    log::warning(&format!(
                        "cannot publish the new fingerprint: {}",
                        e.message
                    ));
                }
            }
            tokio::time::sleep(WEB_RESTART_DELAY).await;
            match bmac_adapters::systemd::Systemd::new(&RealRunner)
                .restart(WEB_UNIT)
                .await
            {
                Ok(()) => log::info("restarted the front end with the new certificate"),
                Err(e) => log::error(&format!("cannot restart {WEB_UNIT}: {e}")),
            }
        });
    })
}

async fn render_script_config() -> Result<(), String> {
    tokio::task::spawn_blocking(|| settings::Settings::system().render_for_scripts())
        .await
        .unwrap_or_else(|e| Err(e.to_string()))
}

/// Keeps the script config current. The lifecycle hook and the timers read
/// it too, so it is rendered once before the engine reports ready (the unit
/// orders guest starts after that) and then whenever the settings change.
async fn spawn_script_config() -> tokio::task::JoinHandle<()> {
    let mut failing = false;
    if let Err(e) = render_script_config().await {
        log::error(&format!("cannot render the script config: {e}"));
        failing = true;
    }
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(SCRIPT_CONFIG_REFRESH).await;
            match render_script_config().await {
                Ok(()) if failing => {
                    log::info("the script config is current again");
                    failing = false;
                }
                Err(e) if !failing => {
                    log::error(&format!("cannot render the script config: {e}"));
                    failing = true;
                }
                _ => {}
            }
        }
    })
}

async fn run_engine(options: Options) -> Result<(), String> {
    if !rustix::process::geteuid().is_root() {
        return Err("the engine runs as root".to_string());
    }
    let script_config = spawn_script_config().await;
    let web = peers::lookup_account("/etc/passwd".as_ref(), &options.web_user)
        .map_err(|e| e.to_string())?;
    if let Some(directory) = options.socket.parent() {
        server::prepare_directory(directory, web.gid)
            .map_err(|e| format!("cannot prepare {}: {e}", directory.display()))?;
    }
    let db =
        Database::open(DATABASE.as_ref()).map_err(|e| format!("cannot open {DATABASE}: {e}"))?;
    let metrics = metrics::Metrics::new(db.clone());
    let auth = Arc::new(
        Auth::new(AuthConfig::system(listen::node_name()), db)
            .await
            .map_err(|e| format!("cannot load sessions: {e}"))?,
    );
    let _log_writer = log::attach(auth.store().clone());
    let upkeep = auth::spawn_upkeep(auth.clone(), SESSION_UPKEEP);
    let listener = server::bind(&options.socket, Some(web.gid))
        .map_err(|e| format!("cannot open {}: {e}", options.socket.display()))?;
    log::info(&format!(
        "bmacd {VERSION} engine listening on {}",
        options.socket.display()
    ));
    let (listen_tx, listen_rx) = tokio::sync::watch::channel(Vec::new());
    let refresher = tokio::spawn(async move {
        let conf = PathBuf::from(listen::COROSYNC_CONF);
        loop {
            let addresses = listen::discover(&RealRunner, &conf).await;
            listen_tx.send_if_modified(|current| {
                let changed = *current != addresses;
                *current = addresses;
                changed
            });
            tokio::time::sleep(web::REFRESH).await;
        }
    });
    let certificates = certificate::Certificates::system(listen::node_name(), after_switch());
    match certificates.repair() {
        Ok(true) => log::warning("finished a certificate switch that was interrupted"),
        Ok(false) => {}
        Err(e) => log::error(&format!(
            "cannot finish an interrupted certificate switch: {e}"
        )),
    }
    systemd::notify_ready().map_err(|e| format!("cannot notify systemd: {e}"))?;
    let mut engine = Engine::with_listen(VERSION, auth, listen_rx.clone())
        .with_certificates(certificates)
        .with_setup(bmacd::native::setup::Paths::system());
    let mut tasks = Vec::new();
    let setup_paths = bmacd::native::setup::Paths::system();
    let started_in_setup = bmacd::native::setup::in_setup_mode(&setup_paths);
    if started_in_setup {
        tasks.push(bmacd::joiner::spawn(
            setup_paths.bootstrap.clone(),
            setup_paths.join_state.clone(),
            bmacd::joiner::system_identity,
            auth::system_clock(),
        ));
    } else {
        tasks.push(tokio::spawn(async {
            let settings = bmacd::settings::Settings::system();
            let mut failing = false;
            loop {
                let pinned = bmacd::native::cluster::pin_from_settings(Path::new("/"), &settings);
                match &pinned {
                    Ok(true) => log::info("pinned the hosts' private addresses in /etc/hosts"),
                    Ok(false) => {}
                    Err(e) if !failing => log::warning(&format!("cannot pin the hosts: {e}")),
                    Err(_) => {}
                }
                failing = pinned.is_err();
                tokio::time::sleep(HOST_PINS).await;
            }
        }));
    }
    let mut stopping: Option<Arc<supervisor::Supervisor>> = None;
    match listen::node_name().parse::<HostSlot>() {
        Ok(host) => {
            let registry = Arc::new(
                operations::Registry::builtin().map_err(|e| format!("operation registry: {e}"))?,
            );
            let config = jobs::JobsConfig::system(host, auth::system_clock());
            let cluster: Arc<dyn cluster::ClusterView> = Arc::new(cluster::PveCluster::system());
            let jobs = Arc::new(jobs::Jobs::new(config, registry.clone(), cluster.clone()));
            let projections = metrics::Projections::default();
            let source = status::StatusSource::new(
                Some(host),
                engine.hub().clone(),
                cluster.clone(),
                bmac_registry::Registry::system(),
                auth::system_clock(),
            )
            .with_projections(projections.clone());
            let source = Arc::new(source);
            engine = engine.with_status(source.clone());
            let health = source.clone();
            tasks.push(status::journal::spawn(status::journal::JournalConfig {
                runner: Arc::new(RealRunner),
                store: engine.auth().store().clone(),
                clock: auth::system_clock(),
                critical: Arc::new(move || {
                    let own = host.to_string();
                    health.hosts().conditions.iter().any(|c| {
                        c.severity == bmac_proto::status::ConditionSeverity::Critical
                            && (c.resource == own || c.resource.starts_with(&format!("{own}/")))
                    })
                }),
                every: status::journal::EVERY,
            }));
            let notifier = Arc::new(notify::Notifier::new(notify::NotifierConfig {
                host,
                cluster: cluster.clone(),
                conditions: Arc::new(move || {
                    let snapshot = source.cluster();
                    (snapshot.cluster.name, snapshot.conditions)
                }),
                hub: engine.hub().clone(),
                clock: auth::system_clock(),
                zone: jiff::tz::TimeZone::system(),
                store: notify::store::NotifyStore::system(),
                local: notify::store::NotifyStore::local(),
                connect: notify::https(),
            }));
            tasks.push(notifier.clone().spawn());
            engine = engine.with_notifier(notifier);
            let identities = identity::Identities::system();
            tasks.push(
                identities
                    .clone()
                    .spawn_publisher(host, auth::system_clock()),
            );
            engine = engine.with_settings(settings::Settings::system(), identities);
            tasks.push(metrics.clone().spawn(
                engine.hub().clone(),
                auth::system_clock(),
                projections,
            ));
            let mut collectors = status::CollectorConfig::system(
                host,
                VERSION,
                Arc::new(RealRunner),
                auth::system_clock(),
            );
            collectors.cluster = Some(cluster.clone());
            tasks.push(status::spawn_collectors(collectors, engine.hub().clone()));
            let (sink, changes) =
                runner::EngineSink::new(engine.hub().clone(), engine.auth().store().clone());
            let known = registry.clone();
            let mut scripts = supervisor::SupervisorConfig::system(
                auth::system_clock(),
                Arc::new(move |id: &str| known.get(id).is_some()),
            );
            let script_settings = settings::Settings::system();
            scripts.env.push(("BMAC_HOST".into(), host.to_string()));
            scripts.prepare = Some(Arc::new(move || {
                script_settings.render_to(std::path::Path::new(settings::RUN_CONFIG_DIR))?;
                script_settings.render_ssh_config(std::path::Path::new(settings::SSH_CONFIG))
            }));
            let supervisor = supervisor::Supervisor::new(scripts, sink.clone());
            stopping = Some(supervisor.clone());
            let runner = runner::Runner::new(
                jobs.clone(),
                supervisor,
                sink,
                runner::RunnerConfig::default(),
            );
            let mut peer_mesh = None;
            let runner = match mesh::tls::PeerTls::system(&listen::node_name()) {
                Ok(tls) => {
                    let mesh = mesh::Mesh::new(
                        mesh::MeshConfig::new(host, tls),
                        cluster,
                        engine.auth().store().clone(),
                        engine.hub().clone(),
                        Arc::new(mesh::tokens::Tokens::system(host, auth::system_clock())),
                        auth::system_clock(),
                    );
                    let runner = runner.with_relay(mesh.clone());
                    mesh.set_runner(runner.clone());
                    tasks.push(mesh.serve(listen_rx));
                    tasks.push(mesh.spawn_links());
                    tasks.push(mesh.spawn_catch_up());
                    peer_mesh = Some(mesh);
                    runner
                }
                Err(e) => {
                    log::warning(&format!(
                        "no links to other hosts, so answers reach only jobs running here: {e}"
                    ));
                    runner
                }
            };
            match bmacd::native::boot_id() {
                Ok(boot) => tasks.push(runner.spawn_recover(boot, RECOVER_EVERY, RECOVER_WITHIN)),
                Err(e) => log::warning(&format!("jobs that restart this host won't resume: {e}")),
            }
            tasks.push(runner.spawn(changes));
            if started_in_setup {
                tasks.push(restart_after_setup(runner.clone(), setup_paths.clone()));
            }
            tasks.push(
                records::task::RecordsTask {
                    host,
                    store: engine.auth().store().clone(),
                    mesh: peer_mesh.clone(),
                    hub: engine.hub().clone(),
                    clock: auth::system_clock(),
                    policy: records::retention::Policy::default(),
                }
                .spawn(),
            );
            if let Some(mesh) = peer_mesh {
                engine = engine.with_joins(Arc::new(bmacd::joins::Joins {
                    store: bmacd::joins::JoinStore::system(),
                    tokens: mesh.tokens().clone(),
                    settings: settings::Settings::system(),
                    identities: identity::Identities::system(),
                    runner: Arc::new(RealRunner),
                    clock: auth::system_clock(),
                    ca: std::path::PathBuf::from(bmacd::joins::CLUSTER_CA),
                }));
                engine = engine.with_mesh(mesh);
            }
            let scheduler = schedules::Scheduler::new(
                schedules::Schedules::system(),
                jobs,
                schedules::SchedulerConfig::system(auth::system_clock()),
            )
            .with_runner(runner.clone());
            tasks.push(Arc::new(scheduler).spawn());
            engine = engine.with_runner(runner);
        }
        Err(_) => log::warning(&format!(
            "{} is not a BMAC host name (mox1 to mox32), so this host runs no jobs",
            listen::node_name()
        )),
    }
    tasks.extend(engine.spawn_job_upkeep(JOB_UPKEEP));
    let allowed = AllowedPeers::front_end_and_root(web);
    let shutdown = async move {
        terminated().await;
        if let Some(supervisor) = stopping {
            supervisor.stop();
        }
    };
    server::serve(listener, allowed, Arc::new(engine), shutdown).await;
    refresher.abort();
    upkeep.abort();
    script_config.abort();
    for task in tasks {
        task.abort();
    }
    let _ = std::fs::remove_file(&options.socket);
    log::info("engine stopped");
    Ok(())
}

async fn run_web(options: Options) -> Result<(), String> {
    if rustix::process::geteuid().is_root() {
        return Err("the front end must not run as root".to_string());
    }
    let read = |name: &str| {
        let path = systemd::credential(name).map_err(|e| e.to_string())?;
        std::fs::read(&path).map_err(|e| format!("cannot read credential {name}: {e}"))
    };
    let (key, cert) = (read(web::KEY_CREDENTIAL)?, read(web::CERT_CREDENTIAL)?);
    let tls = web::tls::server_config(&cert, &key).map_err(|e| e.to_string())?;
    let fingerprint = web::tls::pem_fingerprint(&cert).map_err(|e| e.to_string())?;
    log::info(&format!(
        "https: certificate SHA-256 fingerprint {fingerprint}"
    ));

    let client = EngineClient::new(&options.socket);
    let hello = web::wait_for_engine(&client, VERSION, 40)
        .await
        .map_err(|e| e.to_string())?;
    log::info(&format!(
        "bmacd {VERSION} front end connected to the engine"
    ));
    let client = Arc::new(client);
    let app = web::app::router(Arc::new(web::assets::Assets::embedded()), client.clone());
    let server = web::https::HttpsServer::new(tls, app);
    systemd::notify_ready().map_err(|e| format!("cannot notify systemd: {e}"))?;
    web::run(&client, VERSION, server, hello, terminated())
        .await
        .map_err(|e| e.to_string())?;
    log::info("front end stopped");
    Ok(())
}

/// `bmacd import-settings DIR`, in lab builds only: loads v2-style
/// `cluster.conf` and `moxN.conf` files into the cluster settings, until
/// the Settings page (M3) and the Deployer (M7) set them.
#[cfg(feature = "lab")]
fn import_settings(args: &[String]) -> ExitCode {
    let [dir] = args else {
        eprintln!("usage: bmacd import-settings DIR");
        return ExitCode::from(2);
    };
    if !rustix::process::geteuid().is_root() {
        eprintln!("bmacd import-settings runs as root");
        return ExitCode::FAILURE;
    }
    match settings::Settings::system().import(
        std::path::Path::new(dir),
        "root@pam",
        auth::system_clock()(),
    ) {
        Ok(hosts) => {
            let names: Vec<String> = hosts.iter().map(ToString::to_string).collect();
            println!(
                "Imported the cluster settings and the facts of {}",
                if names.is_empty() {
                    "no host".to_string()
                } else {
                    names.join(", ")
                }
            );
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("bmacd import-settings: {}", error.message);
            ExitCode::FAILURE
        }
    }
}

/// `bmacd render-config`: renders the script config once, for the timer
/// units, which must not depend on the engine running.
fn render_config(args: &[String]) -> ExitCode {
    if !args.is_empty() {
        eprintln!("usage: bmacd render-config");
        return ExitCode::from(2);
    }
    if !rustix::process::geteuid().is_root() {
        eprintln!("bmacd render-config runs as root");
        return ExitCode::FAILURE;
    }
    match settings::Settings::system().render_for_scripts() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("bmacd render-config: {e}");
            ExitCode::FAILURE
        }
    }
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some(mode) = args.first().map(String::as_str) else {
        eprintln!("{USAGE}");
        return ExitCode::from(2);
    };
    if mode == "--version" {
        println!("bmacd {VERSION}");
        return ExitCode::SUCCESS;
    }
    if mode == "render-config" {
        return render_config(&args[1..]);
    }
    if mode == "native" {
        if !rustix::process::geteuid().is_root() {
            eprintln!("bmacd native runs as root, under the engine");
            return ExitCode::FAILURE;
        }
        return bmacd::native::main(&args[1..]);
    }
    #[cfg(feature = "lab")]
    if mode == "import-settings" {
        return import_settings(&args[1..]);
    }
    let options = match parse(&args[1..]) {
        Ok(options) => options,
        Err(message) => {
            eprintln!("bmacd: {message}\n{USAGE}");
            return ExitCode::from(2);
        }
    };
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(e) => {
            log::error(&format!("cannot start the runtime: {e}"));
            return ExitCode::FAILURE;
        }
    };
    let result = match mode {
        "engine" => runtime.block_on(run_engine(options)),
        "web" => runtime.block_on(run_web(options)),
        other => {
            eprintln!("bmacd: unknown mode {other}\n{USAGE}");
            return ExitCode::from(2);
        }
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            log::error(&format!("bmacd {mode}: {message}"));
            ExitCode::FAILURE
        }
    }
}
