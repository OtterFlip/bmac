// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

//! The engine socket end to end: a real Unix socket in a per-test
//! directory, the server, and the front end's client.

use std::net::{IpAddr, Ipv4Addr};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use bmac_pmxcfs::{Pmxcfs, RealFs};
use bmac_proto::api::ErrorCode;
use bmac_proto::engine::{
    EngineCall, EngineOutcome, EngineReply, EngineRequest, Hello, MAX_REQUEST_FRAME,
};
use bmac_proto::stream::{StreamBody, StreamMessage};
use bmacd::auth::proxmox::ProxmoxTickets;
use bmacd::auth::revocations::Revocations;
use bmacd::auth::{Auth, AuthConfig, system_clock};
use bmacd::db::Database;
use bmacd::engine::server::{self, SOCKET_MODE};
use bmacd::engine::{Engine, Handler, Peer, Replies};
use bmacd::peers::AllowedPeers;
use bmacd::web::client::{ClientError, EngineClient};
use bmacd::web::{self, StartError};
use bmacd::wire;
use serde_json::json;
use tokio::io::{AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::sync::{Notify, oneshot};

const LOCAL: IpAddr = IpAddr::V4(Ipv4Addr::LOCALHOST);

fn me() -> u32 {
    rustix::process::getuid().as_raw()
}

struct NotifyOnDrop<'a>(&'a Notify);

impl Drop for NotifyOnDrop<'_> {
    fn drop(&mut self) {
        self.0.notify_one();
    }
}

/// Echoes each call's name, peer, and token. `cluster` waits for `release`;
/// `subscribe` streams three events, then runs until it is stopped.
#[derive(Default)]
struct Echo {
    release: Notify,
    left: Notify,
}

impl Handler for Echo {
    async fn handle(&self, peer: Peer, request: EngineRequest, replies: Replies) {
        match &request.call {
            EngineCall::Subscribe { .. } => {
                let _left = NotifyOnDrop(&self.left);
                for n in 0..3 {
                    let message = StreamMessage {
                        id: n.to_string(),
                        body: StreamBody::Quorum { quorate: true },
                    };
                    replies.event(message).await;
                }
                // The server aborts this task when the front end leaves.
                std::future::pending::<()>().await;
            }
            EngineCall::Cluster {} => self.release.notified().await,
            _ => {}
        }
        let body = json!({"name": request.call.name(), "uid": peer.uid, "token": request.token});
        replies.ok(body).await;
    }
}

struct Running {
    _dir: tempfile::TempDir,
    path: PathBuf,
    stop: Option<oneshot::Sender<()>>,
    done: tokio::task::JoinHandle<()>,
}

impl Running {
    async fn stop(mut self) {
        let _ = self.stop.take().unwrap().send(());
        self.done.await.unwrap();
    }
}

fn start<H: Handler>(handler: Arc<H>, allowed: AllowedPeers) -> Running {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("engine.sock");
    let listener = server::bind(&path, None).unwrap();
    let (stop, stopped) = oneshot::channel::<()>();
    let done = tokio::spawn(server::serve(listener, allowed, handler, async {
        let _ = stopped.await;
    }));
    Running {
        _dir: dir,
        path,
        stop: Some(stop),
        done,
    }
}

/// An engine whose login service is unreachable, with state in `dir`.
async fn engine(dir: &Path, version: &str) -> Engine {
    let pve = dir.join("pve");
    let config = AuthConfig {
        proxmox: ProxmoxTickets {
            address: "127.0.0.1:9".parse().unwrap(),
            certificates: Vec::new(),
        },
        user_cfg: dir.join("user.cfg"),
        revocations: Revocations::new(Pmxcfs::new(&pve, Arc::new(RealFs)), pve.join("revoked")),
        policy: Default::default(),
        limits: Default::default(),
        host: "test".into(),
        clock: system_clock(),
    };
    let db = Database::open(&dir.join("bmac.db")).unwrap();
    Engine::new(version, Arc::new(Auth::new(config, db).await.unwrap()))
}

fn every_call() -> Vec<EngineCall> {
    let job = "6f9619ff-8b86-4011-b42d-00c04fc964ff";
    [
        json!({"name": "hello"}),
        json!({"name": "login", "request": {"step": "password", "user": "alice@pve", "password": "pw"}}),
        json!({"name": "logout", "request": {"everywhere": false}}),
        json!({"name": "list", "collection": "hosts"}),
        json!({"name": "operations"}),
        json!({"name": "launch_job", "request": {"operation": "add_prod_vm", "parameters": {}, "idempotency_key": "k", "dry_run": true}}),
        json!({"name": "job", "job": job, "from_seq": 0}),
        json!({"name": "answer", "job": job, "request_id": "r1", "answer": {"kind": "acknowledge"}}),
        json!({"name": "cancel", "job": job, "request": {"force": true}}),
        json!({"name": "watch", "request": {"stream": "s1", "jobs": [job]}}),
    ]
    .into_iter()
    .map(|call| serde_json::from_value(call).unwrap())
    .collect()
}

#[tokio::test]
async fn every_unary_call_round_trips() {
    let engine = start(Arc::new(Echo::default()), AllowedPeers::new([me()]));
    let client = EngineClient::new(&engine.path);
    for call in every_call() {
        let name = call.name();
        let outcome = client.call(LOCAL, Some("t".into()), call).await.unwrap();
        assert_eq!(
            outcome,
            EngineOutcome::Ok {
                body: json!({"name": name, "uid": me(), "token": "t"})
            },
            "{name}"
        );
    }
    engine.stop().await;
}

#[tokio::test]
async fn streams_deliver_events_and_end_when_dropped() {
    let echo = Arc::new(Echo::default());
    let engine = start(echo.clone(), AllowedPeers::new([me()]));
    let client = EngineClient::new(&engine.path);
    let subscribe = EngineCall::Subscribe {
        last_event_id: Some("41".into()),
    };
    assert!(matches!(
        client.call(LOCAL, None, subscribe.clone()).await,
        Err(ClientError::StreamCall("subscribe"))
    ));
    assert!(matches!(
        client.stream(LOCAL, None, EngineCall::Operations {}).await,
        Err(ClientError::NotStream("operations"))
    ));
    let mut stream = client
        .stream(LOCAL, Some("t".into()), subscribe)
        .await
        .unwrap();
    for n in 0..3 {
        match stream.next().await {
            Some(EngineOutcome::Event { message }) => assert_eq!(message.id, n.to_string()),
            other => panic!("expected event {n}, got {other:?}"),
        }
    }
    let left = echo.left.notified();
    drop(stream);
    tokio::time::timeout(Duration::from_secs(5), left)
        .await
        .expect("the engine noticed the stream closed");
    engine.stop().await;
}

#[tokio::test]
async fn other_uids_are_refused() {
    let engine = start(
        Arc::new(Echo::default()),
        AllowedPeers::new([me().wrapping_add(1)]),
    );
    let client = EngineClient::new(&engine.path);
    let refused = client
        .call(LOCAL, Some("t".into()), EngineCall::Operations {})
        .await;
    assert!(
        matches!(refused, Err(ClientError::Disconnected)),
        "{refused:?}"
    );
    engine.stop().await;
}

#[tokio::test]
async fn a_slow_call_does_not_hold_up_others() {
    let echo = Arc::new(Echo::default());
    let engine = start(echo.clone(), AllowedPeers::new([me()]));
    let client = Arc::new(EngineClient::new(&engine.path));
    let slow = {
        let client = client.clone();
        tokio::spawn(async move {
            client
                .call(LOCAL, Some("t".into()), EngineCall::Cluster {})
                .await
        })
    };
    tokio::time::sleep(Duration::from_millis(50)).await;
    let fast = client
        .call(LOCAL, Some("t".into()), EngineCall::Operations {})
        .await
        .unwrap();
    assert!(matches!(fast, EngineOutcome::Ok { .. }));
    assert!(!slow.is_finished());
    echo.release.notify_one();
    assert!(matches!(
        slow.await.unwrap().unwrap(),
        EngineOutcome::Ok { .. }
    ));
    engine.stop().await;
}

async fn raw_exchange(path: &Path, bytes: &[u8]) -> (Option<EngineReply>, bool) {
    let mut stream = UnixStream::connect(path).await.unwrap();
    // The server may close before reading everything; that is the point.
    let _ = stream.write_all(bytes).await;
    let (read, _write) = stream.into_split();
    let mut reader = BufReader::new(read);
    let reply = wire::read_frame::<EngineReply, _>(&mut reader, 1 << 20)
        .await
        .unwrap();
    let closed = wire::read_line(&mut reader, 1 << 20)
        .await
        .unwrap()
        .is_none();
    (reply, closed)
}

#[tokio::test]
async fn bad_frames_get_an_error_and_a_closed_connection() {
    let engine = start(Arc::new(Echo::default()), AllowedPeers::new([me()]));
    let generic =
        b"{\"id\":1,\"client\":\"127.0.0.1\",\"call\":{\"name\":\"run\",\"argv\":[\"/bin/sh\"]}}\n";
    let (reply, closed) = raw_exchange(&engine.path, generic).await;
    let reply = reply.unwrap();
    assert_eq!(reply.id, 0);
    match reply.outcome {
        EngineOutcome::Err { error } => {
            assert_eq!(error.code, ErrorCode::Invalid);
            assert!(
                error
                    .message
                    .starts_with("the request is not one the engine accepts"),
                "{}",
                error.message
            );
        }
        other => panic!("{other:?}"),
    }
    assert!(closed);

    let huge = vec![b'x'; MAX_REQUEST_FRAME + 10];
    let (reply, closed) = raw_exchange(&engine.path, &huge).await;
    match reply.unwrap().outcome {
        EngineOutcome::Err { error } => assert_eq!(
            error.message,
            format!("frame longer than {MAX_REQUEST_FRAME} bytes")
        ),
        other => panic!("{other:?}"),
    }
    assert!(closed);
    engine.stop().await;
}

#[tokio::test]
async fn the_engine_needs_a_token_for_everything_but_hello_and_login() {
    let state = tempfile::tempdir().unwrap();
    let engine = start(
        Arc::new(engine(state.path(), "3.0.0-test").await),
        AllowedPeers::new([me()]),
    );
    let client = EngineClient::new(&engine.path);
    for call in every_call() {
        let name = call.name();
        let anonymous = call.is_anonymous();
        let outcome = client.call(LOCAL, None, call).await.unwrap();
        match (anonymous, outcome) {
            (true, _) => {}
            (false, EngineOutcome::Err { error }) => {
                assert_eq!(error.code, ErrorCode::Unauthenticated, "{name}");
                assert_eq!(error.message, "log in first");
            }
            (false, other) => panic!("{name} without a token gave {other:?}"),
        }
    }
    let empty = client
        .call(LOCAL, Some(String::new()), EngineCall::Operations {})
        .await
        .unwrap();
    assert!(
        matches!(empty, EngineOutcome::Err { error } if error.code == ErrorCode::Unauthenticated)
    );

    let hello = web::hello(&client, "3.0.0-test").await.unwrap();
    assert_eq!(
        hello,
        Hello {
            version: "3.0.0-test".into(),
            listen: Vec::new()
        }
    );
    match web::hello(&client, "3.0.1").await {
        Err(StartError::VersionMismatch { engine, front_end }) => assert_eq!(
            (engine.as_str(), front_end.as_str()),
            ("3.0.0-test", "3.0.1")
        ),
        other => panic!("{other:?}"),
    }
    engine.stop().await;
}

#[tokio::test]
async fn hello_names_the_current_listen_addresses() {
    let (tx, rx) = tokio::sync::watch::channel(vec!["100.101.2.3:8443".parse().unwrap()]);
    let state = tempfile::tempdir().unwrap();
    let auth = engine(state.path(), "v").await.auth().clone();
    let engine = start(
        Arc::new(Engine::with_listen("v", auth, rx)),
        AllowedPeers::new([me()]),
    );
    let client = EngineClient::new(&engine.path);
    let first = web::hello(&client, "v").await.unwrap();
    assert_eq!(first.listen, ["100.101.2.3:8443".parse().unwrap()]);
    tx.send(vec![
        "10.10.0.1:8443".parse().unwrap(),
        "100.101.2.3:8443".parse().unwrap(),
    ])
    .unwrap();
    let second = web::hello(&client, "v").await.unwrap();
    assert_eq!(second.listen.len(), 2);
    engine.stop().await;
}

#[tokio::test]
async fn the_client_reconnects_after_the_engine_restarts() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("engine.sock");
    let client = EngineClient::new(&path);
    assert!(matches!(
        web::wait_for_engine(&client, "v", 2).await,
        Err(StartError::Engine(ClientError::Connect { .. }))
    ));
    for _ in 0..2 {
        let listener = server::bind(&path, None).unwrap();
        let (stop, stopped) = oneshot::channel::<()>();
        let done = tokio::spawn(server::serve(
            listener,
            AllowedPeers::new([me()]),
            Arc::new(engine(dir.path(), "v").await),
            async {
                let _ = stopped.await;
            },
        ));
        web::wait_for_engine(&client, "v", 20).await.unwrap();
        stop.send(()).unwrap();
        done.await.unwrap();
    }
}

#[test]
fn bind_sets_the_mode_and_group_and_replaces_only_sockets() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("engine.sock");
    let gid = rustix::process::getgid().as_raw();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        drop(server::bind(&path, Some(gid)).unwrap());
        let meta = std::fs::metadata(&path).unwrap();
        assert_eq!(meta.permissions().mode() & 0o777, SOCKET_MODE);
        assert_eq!(meta.gid(), gid);
        // A stale socket from an earlier run is replaced.
        drop(server::bind(&path, None).unwrap());
    });
    let run = dir.path().join("run");
    std::fs::create_dir(&run).unwrap();
    server::prepare_directory(&run, gid).unwrap();
    let meta = std::fs::metadata(&run).unwrap();
    assert_eq!(
        (meta.permissions().mode() & 0o777, meta.gid()),
        (server::DIRECTORY_MODE, gid)
    );

    let file = dir.path().join("not-a-socket");
    std::fs::write(&file, "keep me").unwrap();
    let err = runtime.block_on(async { server::bind(&file, None).unwrap_err() });
    assert_eq!(err.kind(), std::io::ErrorKind::AlreadyExists);
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "keep me");
}
