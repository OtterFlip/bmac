// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

//! The `bmac` commands against a real engine: login through a fake Proxmox
//! ticket endpoint, jobs run by the real runner and supervisor on the mock
//! scripts, and every command logging out when it ends.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Router;
use axum::body::Bytes;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use bmac_cli::args::RunArgs;
use bmac_cli::commands::{self, Context, TerminalEnv};
use bmac_cli::terminal::Scripted;
use bmac_core::{GuestName, HostSlot, JobId};
use bmac_pmxcfs::{Pmxcfs, RealFs};
use bmacd::auth::proxmox::{ProxmoxTickets, TICKET_PATH};
use bmacd::auth::revocations::Revocations;
use bmacd::auth::{Auth, AuthConfig, system_clock};
use bmacd::cluster::{ClusterView, Membership};
use bmacd::db::Database;
use bmacd::engine::{Engine, server};
use bmacd::jobs::{Jobs, JobsConfig};
use bmacd::operations::Registry;
use bmacd::peers::AllowedPeers;
use bmacd::runner::{EngineSink, Runner, RunnerConfig};
use bmacd::supervisor::{Supervisor, SupervisorConfig};
use bmacd::web::client::EngineClient;
use bmacd::web::https::HttpsServer;
use bmacd::web::tls;
use serde_json::{Value, json};
use tokio::net::TcpListener;
use tokio::sync::oneshot;

const USER_CFG: &str = "\
user:alice@pve:1:0::::::
user:bob@pve:1:0::::::
user:wendy@pve:1:0::::::
group:bmac-admin:alice@pve::
group:bmac-operator:bob@pve,wendy@pve::
";

const ALICE_TICKET: &str =
    "PVE:!tfa!%7B%22totp%22%3Atrue%2C%22recovery%22%3A%5B0%2C1%5D%7D:6528ABCD::c2ln";
const WENDY_TICKET: &str =
    "PVE:!tfa!%7B%22webauthn%22%3A%7B%22challenge%22%3A%22x%22%7D%7D:6528ABCD::c2ln";

const TERMINAL_SCRIPT: &str = "#!/usr/bin/env bash
# Copyright (c) 2026 BEENTHERE VENTURES, INC.
# SPDX-License-Identifier: GPL-3.0-only
set -euo pipefail
printf '%s\\n' \"$@\" > \"$BMAC_TEST_OUT\"
exit 3
";

fn form(body: &[u8]) -> HashMap<String, String> {
    let decode = |s: &str| {
        let bytes = s.replace('+', " ").into_bytes();
        let mut out = Vec::new();
        let mut i = 0;
        while i < bytes.len() {
            if bytes[i] == b'%' {
                out.push(
                    u8::from_str_radix(std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap(), 16)
                        .unwrap(),
                );
                i += 3;
            } else {
                out.push(bytes[i]);
                i += 1;
            }
        }
        String::from_utf8(out).unwrap()
    };
    std::str::from_utf8(body)
        .unwrap()
        .split('&')
        .filter_map(|pair| pair.split_once('='))
        .map(|(k, v)| (decode(k), decode(v)))
        .collect()
}

/// Proxmox's `access/ticket`: bob has no second factor, alice has TOTP and
/// recovery keys, and wendy only WebAuthn.
async fn fake_ticket(body: Bytes) -> Response {
    let form = form(&body);
    let field = |k: &str| form.get(k).map(String::as_str).unwrap_or_default();
    let granted = |user: &str| {
        axum::Json(json!({"data": {"username": user, "ticket": format!("PVE:{user}:6528ABCD::sig"), "CSRFPreventionToken": "x"}}))
            .into_response()
    };
    let need_tfa = |user: &str, ticket: &str| {
        axum::Json(json!({"data": {"username": user, "ticket": ticket, "CSRFPreventionToken": "x", "NeedTFA": 1}}))
            .into_response()
    };
    match (field("username"), field("password"), field("tfa-challenge")) {
        ("bob@pve", "bob-pw", "") => granted("bob@pve"),
        ("alice@pve", "alice-pw", "") => need_tfa("alice@pve", ALICE_TICKET),
        ("wendy@pve", "wendy-pw", "") => need_tfa("wendy@pve", WENDY_TICKET),
        ("alice@pve", "totp:123456", ALICE_TICKET) => granted("alice@pve"),
        _ => (StatusCode::UNAUTHORIZED, axum::Json(json!({"data": null}))).into_response(),
    }
}

struct OneHost;

impl ClusterView for OneHost {
    fn membership(&self) -> Result<Membership, String> {
        let me = HostSlot::new(1).unwrap();
        Ok(Membership {
            quorate: true,
            online: [me].into(),
            members: [(me, None)].into(),
        })
    }

    fn guest_owner(&self, _guest: GuestName) -> Result<Option<HostSlot>, String> {
        Ok(None)
    }

    fn placement(&self, _guest: GuestName) -> Result<Vec<HostSlot>, String> {
        Ok(Vec::new())
    }
}

fn operation(id: &str, script: &str, lock: &str, min_role: &str, params: Value) -> Value {
    json!({
        "id": id, "title": id, "summary": "s", "description": "d",
        "category": "diagnostics", "kind": "script", "script": script,
        "min_role": min_role, "mutating": true, "destructive": false,
        "params": params, "locks": [lock],
        "target": {"rule": "duty"}, "cancel": {"rule": "script"}, "resume": "never"
    })
}

fn copy_tree(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for entry in std::fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let target = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_tree(&entry.path(), &target);
        } else {
            std::fs::copy(entry.path(), target).unwrap();
        }
    }
}

struct Stack {
    dir: tempfile::TempDir,
    auth: Arc<Auth>,
    socket: PathBuf,
    legacy: PathBuf,
    registry: Arc<Registry>,
    prepared: Arc<Mutex<usize>>,
    tasks: Vec<tokio::task::JoinHandle<()>>,
    _stop: oneshot::Sender<()>,
}

impl Drop for Stack {
    fn drop(&mut self) {
        for task in &self.tasks {
            task.abort();
        }
    }
}

impl Stack {
    async fn new(version: &str) -> Stack {
        let dir = tempfile::tempdir().unwrap();
        let mut tasks = Vec::new();

        let generated = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
        let cert = dir.path().join("pve-ssl.pem");
        std::fs::write(&cert, generated.cert.pem()).unwrap();
        let config = tls::server_config(
            generated.cert.pem().as_bytes(),
            generated.key_pair.serialize_pem().as_bytes(),
        )
        .unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let proxmox = listener.local_addr().unwrap();
        let router = Router::new().route(TICKET_PATH, post(fake_ticket));
        let server = HttpsServer::new(config, router);
        tasks.push(tokio::spawn(async move { server.serve(listener).await }));

        std::fs::write(dir.path().join("user.cfg"), USER_CFG).unwrap();
        let pve = dir.path().join("pve");
        let auth = AuthConfig {
            proxmox: ProxmoxTickets {
                address: proxmox,
                certificates: vec![cert],
            },
            user_cfg: dir.path().join("user.cfg"),
            revocations: Revocations::new(
                Pmxcfs::new(&pve, Arc::new(RealFs)),
                pve.join("priv/bmac/sessions/revoked"),
            ),
            policy: Default::default(),
            limits: Default::default(),
            host: "mox1".into(),
            clock: system_clock(),
        };
        let db = Database::open(&dir.path().join("bmac.db")).unwrap();
        let auth = Arc::new(Auth::new(auth, db).await.unwrap());

        let legacy = dir.path().join("legacy");
        let bmacd = Path::new(env!("CARGO_MANIFEST_DIR")).join("../bmacd");
        for sub in ["scripts/lib", "scripts/utilities"] {
            copy_tree(&bmacd.join("../../legacy").join(sub), &legacy.join(sub));
        }
        copy_tree(&bmacd.join("tests/mock-scripts"), &legacy.join("mock"));
        std::fs::write(legacy.join("mock/terminal.sh"), TERMINAL_SCRIPT).unwrap();
        let operations = json!({
            "schema_version": 1,
            "operations": [
                operation("test.interactive", "mock/interactive.sh", "staging", "operator", json!([])),
                operation("test.long", "mock/long_running.sh", "capacity", "operator", json!([])),
                operation("test.terminal", "mock/terminal.sh", "host", "operator", json!([
                    {"id": "count", "label": "Count", "type": "integer", "flag": "--count", "required": true}
                ])),
                operation("test.admin", "mock/long_running.sh", "production", "admin", json!([])),
            ],
        });
        let registry = Arc::new(Registry::from_parts(&operations.to_string(), Vec::new()).unwrap());
        let jobs = Arc::new(Jobs::new(
            JobsConfig {
                dir: pve.join("priv/bmac/jobs"),
                pmxcfs: Pmxcfs::new(&pve, Arc::new(RealFs)),
                ..JobsConfig::system(HostSlot::new(1).unwrap(), system_clock())
            },
            registry.clone(),
            Arc::new(OneHost),
        ));
        let engine = Engine::new(version, auth.clone());
        let (sink, changes) = EngineSink::new(engine.hub().clone(), auth.store().clone());
        let mut scripts = SupervisorConfig::system(system_clock(), Arc::new(|_: &str| false));
        scripts.root = legacy.clone();
        scripts.bash = PathBuf::from("bash");
        scripts.work_root = dir.path().join("work");
        scripts.env = vec![(
            "PATH".into(),
            std::env::var("PATH").unwrap_or_else(|_| "/usr/bin:/bin".into()),
        )];
        let runner = Runner::new(
            jobs,
            Supervisor::new(scripts, sink.clone()),
            sink,
            RunnerConfig {
                poll_every: Duration::from_millis(100),
                renew_every: Duration::from_secs(5),
                cancel_check_every: Duration::from_millis(100),
            },
        );
        tasks.push(runner.spawn(changes));
        let engine = engine.with_runner(runner);

        let socket = dir.path().join("engine.sock");
        let listener = server::bind(&socket, None).unwrap();
        let (stop, stopped) = oneshot::channel::<()>();
        let me = rustix::process::getuid().as_raw();
        tokio::spawn(server::serve(
            listener,
            AllowedPeers::new([me]),
            Arc::new(engine),
            async {
                let _ = stopped.await;
            },
        ));
        Stack {
            dir,
            auth,
            socket,
            legacy,
            registry,
            prepared: Arc::default(),
            tasks,
            _stop: stop,
        }
    }

    fn out(&self) -> PathBuf {
        self.dir.path().join("terminal.out")
    }

    fn context(&self) -> Context {
        let prepared = self.prepared.clone();
        Context {
            client: Arc::new(EngineClient::new(&self.socket)),
            poll: Duration::from_millis(50),
            handle_interrupt: false,
            terminal: TerminalEnv {
                root: self.legacy.clone(),
                bash: PathBuf::from("bash"),
                env: vec![
                    (
                        "PATH".into(),
                        std::env::var("PATH").unwrap_or_else(|_| "/usr/bin:/bin".into()),
                    ),
                    ("BMAC_TEST_OUT".into(), self.out().display().to_string()),
                ],
                prepare: Box::new(move || {
                    *prepared.lock().unwrap() += 1;
                    Ok(())
                }),
                registry: self.registry.clone(),
            },
        }
    }

    fn assert_logged_out(&self) {
        let open: Vec<String> = self
            .auth
            .sessions()
            .all()
            .into_iter()
            .map(|s| s.user)
            .collect();
        assert!(open.is_empty(), "sessions left open: {open:?}");
    }
}

fn run(operation: &str, user: &str) -> RunArgs {
    RunArgs {
        operation: operation.into(),
        params: Vec::new(),
        dry_run: false,
        detach: false,
        terminal: false,
        user: Some(user.into()),
    }
}

fn admitted(prompter: &Scripted) -> JobId {
    let text = prompter.text();
    let line = text
        .lines()
        .find(|l| l.starts_with("Job ") && l.contains(" admitted"))
        .unwrap_or_else(|| panic!("no job admitted:\n{text}"));
    line.split_whitespace().nth(1).unwrap().parse().unwrap()
}

const ALICE: [&str; 3] = ["alice-pw", "", "123456"];

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_operator_runs_a_job_and_answers_its_requests_at_the_terminal() {
    let stack = Stack::new("v").await;
    let ctx = stack.context();
    let mut prompter = Scripted::new(["bob-pw", "3", "1,2", "4", "1,3", "GO", ""]);
    let shown = prompter.clone();
    let code = tokio::time::timeout(
        Duration::from_secs(20),
        commands::run(&ctx, &mut prompter, run("test.interactive", "bob@pve")),
    )
    .await
    .unwrap_or_else(|_| panic!("no end:\n{}", shown.text()))
    .unwrap();
    let text = prompter.text();
    assert_eq!(code, 0, "{text}");
    assert_eq!(prompter.remaining(), 0, "{text}");
    for expected in [
        "Password for bob@pve: <secret>",
        "VM resources",
        "  1) mox1",
        "Use an even number of cores.",
        "[WARNING] Create the VM?",
        "Type GO to confirm, or press Enter to decline: GO",
        "Unlock the disk",
        "cores=4 hosts=mox1,mox3",
    ] {
        assert!(text.contains(expected), "missing {expected:?} in:\n{text}");
    }
    assert!(!text.contains("bob-pw"), "the password is never shown");
    stack.assert_logged_out();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn second_factors_and_refused_logins() {
    let stack = Stack::new("v").await;
    let ctx = stack.context();
    let mut prompter = Scripted::new(ALICE);
    let code = commands::jobs(&ctx, &mut prompter, false, Some("alice@pve"))
        .await
        .unwrap();
    assert_eq!(code, 0);
    let text = prompter.text();
    assert!(
        text.contains("Second factor: [t]otp code or [r]ecovery key (t): ")
            && text.contains("Authenticator code: 123456")
            && text.contains("JOB"),
        "{text}"
    );

    let mut prompter = Scripted::new(["nope"]);
    let error = commands::jobs(&ctx, &mut prompter, false, Some("bob@pve"))
        .await
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("the user name or password is wrong"),
        "{error}"
    );

    let mut prompter = Scripted::new(["wendy-pw"]);
    let error = commands::jobs(&ctx, &mut prompter, false, Some("wendy@pve"))
        .await
        .unwrap_err()
        .to_string();
    assert!(error.contains("add TOTP or recovery keys"), "{error}");

    let mut prompter = Scripted::new(["bob-pw"]);
    let error = commands::run(&ctx, &mut prompter, run("test.admin", "bob@pve"))
        .await
        .unwrap_err()
        .to_string();
    assert_eq!(
        error,
        "test.admin needs the admin role; bob@pve is operator"
    );
    stack.assert_logged_out();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_detached_job_is_listed_cancelled_and_followed_to_the_end() {
    let stack = Stack::new("v").await;
    let ctx = stack.context();
    let mut prompter = Scripted::new(["bob-pw"]);
    let mut args = run("test.long", "bob@pve");
    args.detach = true;
    assert_eq!(commands::run(&ctx, &mut prompter, args).await.unwrap(), 0);
    let job = admitted(&prompter);
    assert!(prompter.text().contains(&format!("bmac jobs {job}")));

    let mut prompter = Scripted::new(["bob-pw"]);
    commands::jobs(&ctx, &mut prompter, true, Some("bob@pve"))
        .await
        .unwrap();
    let listed: Value =
        serde_json::from_str(prompter.shown.lock().unwrap().last().unwrap()).unwrap();
    assert_eq!(listed["jobs"][0]["id"], job.to_string());

    let mut prompter = Scripted::new(["bob-pw"]);
    commands::cancel(&ctx, &mut prompter, job, false, Some("bob@pve"))
        .await
        .unwrap();
    assert!(prompter.text().contains("A cancel requested for job"));

    let mut prompter = Scripted::new(["bob-pw"]);
    let code = tokio::time::timeout(
        Duration::from_secs(20),
        commands::follow_job(&ctx, &mut prompter, job, true, Some("bob@pve")),
    )
    .await
    .unwrap()
    .unwrap();
    let text = prompter.text();
    assert_eq!(code, 1, "{text}");
    assert!(text.contains("Finalizing receive"), "{text}");
    stack.assert_logged_out();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn running_in_the_terminal_is_guarded() {
    let stack = Stack::new(bmacd::VERSION).await;
    let ctx = stack.context();
    let terminal = |user: &str| RunArgs {
        params: vec![("count".into(), Some("3".into()))],
        terminal: true,
        ..run("test.terminal", user)
    };

    let mut prompter = Scripted::new(["bob-pw"]);
    let error = commands::run(&ctx, &mut prompter, terminal("bob@pve"))
        .await
        .unwrap_err()
        .to_string();
    assert_eq!(error, "bmac run --terminal is for administrators");

    let mut prompter = Scripted::new(["bob-pw"]);
    let mut args = run("test.long", "bob@pve");
    args.detach = true;
    commands::run(&ctx, &mut prompter, args).await.unwrap();
    let job = admitted(&prompter);
    let mut prompter = Scripted::new(ALICE);
    let error = commands::run(&ctx, &mut prompter, terminal("alice@pve"))
        .await
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("refuses while jobs are unfinished") && error.contains(&job.to_string()),
        "{error}"
    );
    let mut prompter = Scripted::new(["bob-pw"]);
    commands::cancel(&ctx, &mut prompter, job, false, Some("bob@pve"))
        .await
        .unwrap();
    let mut prompter = Scripted::new(["bob-pw"]);
    tokio::time::timeout(
        Duration::from_secs(20),
        commands::follow_job(&ctx, &mut prompter, job, false, Some("bob@pve")),
    )
    .await
    .unwrap()
    .unwrap();

    let mut prompter = Scripted::new(ALICE.into_iter().chain(["yes"]));
    let error = commands::run(&ctx, &mut prompter, terminal("alice@pve"))
        .await
        .unwrap_err()
        .to_string();
    assert_eq!(error, "not confirmed; nothing was run");
    assert_eq!(*stack.prepared.lock().unwrap(), 0);
    assert!(!stack.out().exists());

    let mut prompter = Scripted::new(ALICE.into_iter().chain(["test.terminal"]));
    let code = commands::run(&ctx, &mut prompter, terminal("alice@pve"))
        .await
        .unwrap();
    assert_eq!(code, 3, "the script's own exit status");
    assert_eq!(*stack.prepared.lock().unwrap(), 1);
    assert_eq!(
        std::fs::read_to_string(stack.out()).unwrap(),
        "--count\n3\n"
    );
    assert!(
        prompter
            .text()
            .contains("This runs mock/terminal.sh --count 3 in this terminal"),
        "{}",
        prompter.text()
    );
    stack.assert_logged_out();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn running_in_the_terminal_needs_the_same_engine_version() {
    let stack = Stack::new("0.0.1").await;
    let ctx = stack.context();
    let mut prompter = Scripted::new(ALICE);
    let args = RunArgs {
        params: vec![("count".into(), Some("3".into()))],
        terminal: true,
        ..run("test.terminal", "alice@pve")
    };
    let error = commands::run(&ctx, &mut prompter, args)
        .await
        .unwrap_err()
        .to_string();
    assert!(
        error.starts_with("bmacd is 0.0.1 and this bmac is ")
            && error.ends_with("update them together"),
        "{error}"
    );
    stack.assert_logged_out();
}
