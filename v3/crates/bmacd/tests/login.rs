// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

//! Logging in against a fake Proxmox ticket endpoint served over TLS, with
//! the engine's real session table, revocations, and account checks.

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, AtomicUsize, Ordering};
use std::time::Duration;

use axum::Router;
use axum::body::Bytes;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use bmac_pmxcfs::{Pmxcfs, RealFs};
use bmac_proto::api::{ErrorCode, LoginRequest, LoginResponse, Role, SecondFactor, Session};
use bmac_proto::engine::{EngineCall, EngineOutcome};
use bmac_proto::stream::StreamBody;
use bmacd::auth::limits::Window;
use bmacd::auth::proxmox::{ProxmoxTickets, TICKET_PATH};
use bmacd::auth::revocations::Revocations;
use bmacd::auth::sessions::SessionPolicy;
use bmacd::auth::{Auth, AuthConfig, CHALLENGE_TTL_MS, Clock, LoginLimits};
use bmacd::db::Database;
use bmacd::engine::{Engine, server};
use bmacd::peers::AllowedPeers;
use bmacd::web::client::EngineClient;
use bmacd::web::https::HttpsServer;
use bmacd::web::{app, assets::Assets, tls};
use serde_json::{Value, json};
use tokio::net::TcpListener;
use tokio::sync::oneshot;

const USER_CFG: &str = "\
user:root@pam:1:0::::::
user:alice@pve:1:0::::::
user:bob@pve:1:0::::::
user:carol@pve:0:0::::::
user:erin@pve:1:0::::::
user:wendy@pve:1:0::::::
group:bmac-admin:alice@pve::
group:bmac-operator:bob@pve,wendy@pve::
";

const ALICE_TICKET: &str =
    "PVE:!tfa!%7B%22totp%22%3Atrue%2C%22recovery%22%3A%5B0%2C1%5D%7D:6528ABCD::c2ln";
const WENDY_TICKET: &str =
    "PVE:!tfa!%7B%22webauthn%22%3A%7B%22challenge%22%3A%22x%22%7D%7D:6528ABCD::c2ln";

fn form(body: &[u8]) -> HashMap<String, String> {
    let decode = |s: &str| {
        let s = s.replace('+', " ");
        let bytes = s.as_bytes();
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
/// recovery keys, wendy only WebAuthn, carol is disabled, and erin is in no
/// BMAC group.
async fn fake_ticket(hits: Arc<AtomicUsize>, body: Bytes) -> Response {
    hits.fetch_add(1, Ordering::SeqCst);
    let form = form(&body);
    let field = |k: &str| form.get(k).map(String::as_str).unwrap_or_default();
    assert_eq!(field("new-format"), "1");
    let granted = |user: &str| {
        axum::Json(json!({"data": {"username": user, "ticket": format!("PVE:{user}:6528ABCD::sig"), "CSRFPreventionToken": "x"}}))
            .into_response()
    };
    let need_tfa = |user: &str, ticket: &str| {
        axum::Json(json!({"data": {"username": user, "ticket": ticket, "CSRFPreventionToken": "x", "NeedTFA": 1}}))
            .into_response()
    };
    let refused = (StatusCode::UNAUTHORIZED, axum::Json(json!({"data": null}))).into_response();
    match (field("username"), field("password"), field("tfa-challenge")) {
        ("bob@pve", "bob-pw", "") => granted("bob@pve"),
        ("erin@pve", "erin-pw", "") => granted("erin@pve"),
        ("root@pam", "root-pw", "") => granted("root@pam"),
        ("alice@pve", "alice-pw", "") => need_tfa("alice@pve", ALICE_TICKET),
        ("wendy@pve", "wendy-pw", "") => need_tfa("wendy@pve", WENDY_TICKET),
        ("alice@pve", "totp:123456" | "recovery:abcd-ef12", ALICE_TICKET) => granted("alice@pve"),
        _ => refused,
    }
}

struct Lab {
    dir: tempfile::TempDir,
    proxmox: SocketAddr,
    cert: PathBuf,
    hits: Arc<AtomicUsize>,
    time: Arc<AtomicI64>,
    _server: tokio::task::JoinHandle<()>,
}

impl Lab {
    async fn new() -> Lab {
        let dir = tempfile::tempdir().unwrap();
        let generated = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
        let cert = dir.path().join("pve-ssl.pem");
        std::fs::write(&cert, generated.cert.pem()).unwrap();
        let config = tls::server_config(
            generated.cert.pem().as_bytes(),
            generated.key_pair.serialize_pem().as_bytes(),
        )
        .unwrap();
        let hits = Arc::new(AtomicUsize::new(0));
        let counter = hits.clone();
        let router = Router::new().route(
            TICKET_PATH,
            post(move |body: Bytes| fake_ticket(counter.clone(), body)),
        );
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let proxmox = listener.local_addr().unwrap();
        let server = HttpsServer::new(config, router);
        let _server = tokio::spawn(async move { server.serve(listener).await });
        std::fs::write(dir.path().join("user.cfg"), USER_CFG).unwrap();
        Lab {
            dir,
            proxmox,
            cert,
            hits,
            time: Arc::new(AtomicI64::new(1_800_000_000_000)),
            _server,
        }
    }

    fn user_cfg(&self) -> PathBuf {
        self.dir.path().join("user.cfg")
    }

    fn advance(&self, ms: i64) {
        self.time.fetch_add(ms, Ordering::SeqCst);
    }

    fn config(&self, host: &str) -> AuthConfig {
        let time = self.time.clone();
        let clock: Clock = Arc::new(move || time.load(Ordering::SeqCst));
        let pve = self.dir.path().join("pve");
        AuthConfig {
            proxmox: ProxmoxTickets {
                address: self.proxmox,
                certificates: vec![self.dir.path().join("pveproxy-ssl.pem"), self.cert.clone()],
            },
            user_cfg: self.user_cfg(),
            revocations: Revocations::new(
                Pmxcfs::new(&pve, Arc::new(RealFs)),
                pve.join("priv/bmac/sessions/revoked"),
            ),
            policy: SessionPolicy::default(),
            limits: LoginLimits::default(),
            host: host.into(),
            clock,
        }
    }

    async fn auth_with(&self, host: &str, config: AuthConfig) -> Arc<Auth> {
        let db = Database::open(&self.dir.path().join(format!("{host}.db"))).unwrap();
        Arc::new(Auth::new(config, db).await.unwrap())
    }

    async fn auth(&self, host: &str) -> Arc<Auth> {
        self.auth_with(host, self.config(host)).await
    }
}

fn ip(text: &str) -> IpAddr {
    text.parse().unwrap()
}

fn password(user: &str, password: &str) -> LoginRequest {
    LoginRequest::Password {
        user: user.into(),
        password: password.into(),
    }
}

fn session(response: LoginResponse) -> Session {
    match response {
        LoginResponse::Session(session) => session,
        other => panic!("expected a session, got {other:?}"),
    }
}

async fn audit_kinds(auth: &Auth) -> Vec<String> {
    let mut kinds: Vec<String> = auth
        .audit()
        .recent(100)
        .await
        .into_iter()
        .map(|e| e.kind)
        .collect();
    kinds.reverse();
    kinds
}

#[tokio::test]
async fn a_password_alone_logs_in_a_user_without_a_second_factor() {
    let lab = Lab::new().await;
    let auth = lab.auth("mox1").await;
    let bob = session(
        auth.login(password("bob@pve", "bob-pw"), ip("100.64.0.9"))
            .await
            .unwrap(),
    );
    assert_eq!((bob.user.as_str(), bob.role), ("bob@pve", Role::Operator));
    assert_eq!(bob.idle_timeout_seconds, 30 * 60);
    assert_eq!(
        bob.expires_at_ms,
        lab.time.load(Ordering::SeqCst) + 12 * 3600 * 1000
    );
    let info = auth.authenticate(Some(&bob.token), true).await.unwrap();
    assert_eq!(info.client, ip("100.64.0.9"));

    let root = session(
        auth.login(password("root@pam", "root-pw"), ip("100.64.0.9"))
            .await
            .unwrap(),
    );
    assert_eq!(root.role, Role::Admin);
    let events = auth.audit().recent(10).await;
    assert_eq!(events[0].kind, "login");
    assert_eq!(events[0].host, "mox1");
    assert_eq!(events[0].client.as_deref(), Some("100.64.0.9"));
    assert!(events.iter().all(|e| !e.message.contains(&bob.token)));
}

#[tokio::test]
async fn totp_and_recovery_keys_answer_the_challenge() {
    let lab = Lab::new().await;
    let auth = lab.auth("mox1").await;
    let client = ip("100.64.0.9");
    let LoginResponse::SecondFactor { challenge, methods } = auth
        .login(password("alice@pve", "alice-pw"), client)
        .await
        .unwrap()
    else {
        panic!("expected a challenge");
    };
    assert_eq!(methods, [SecondFactor::Totp, SecondFactor::Recovery]);
    assert!(
        !challenge.contains("tfa"),
        "the Proxmox partial ticket stays in the engine"
    );

    let answer = |method, response: &str| LoginRequest::SecondFactor {
        challenge: challenge.clone(),
        method,
        response: response.into(),
    };
    let wrong = auth
        .login(answer(SecondFactor::Totp, "654321"), client)
        .await
        .unwrap_err();
    assert_eq!(
        (wrong.code, wrong.message.as_str()),
        (ErrorCode::Unauthenticated, "that code is wrong; try again")
    );
    let invalid = auth
        .login(answer(SecondFactor::Totp, "12ab"), client)
        .await
        .unwrap_err();
    assert_eq!(invalid.code, ErrorCode::Invalid);
    let elsewhere = auth
        .login(answer(SecondFactor::Totp, "123456"), ip("100.64.0.10"))
        .await
        .unwrap_err();
    assert_eq!(
        elsewhere.code,
        ErrorCode::Unauthenticated,
        "a challenge is bound to its address"
    );
    let alice = session(
        auth.login(answer(SecondFactor::Totp, "123 456"), client)
            .await
            .unwrap(),
    );
    assert_eq!(alice.role, Role::Admin);
    let reused = auth
        .login(answer(SecondFactor::Totp, "123456"), client)
        .await
        .unwrap_err();
    assert_eq!(
        reused.code,
        ErrorCode::Unauthenticated,
        "a challenge is single-use"
    );

    let LoginResponse::SecondFactor { challenge, .. } = auth
        .login(password("alice@pve", "alice-pw"), client)
        .await
        .unwrap()
    else {
        panic!("expected a challenge");
    };
    let recovery = LoginRequest::SecondFactor {
        challenge,
        method: SecondFactor::Recovery,
        response: "abcd-ef12".into(),
    };
    session(auth.login(recovery, client).await.unwrap());
    assert_eq!(audit_kinds(&auth).await, ["login_failed", "login", "login"]);
}

#[tokio::test]
async fn challenges_expire_and_wrong_codes_run_out() {
    let lab = Lab::new().await;
    let auth = lab.auth("mox1").await;
    let client = ip("10.10.0.5");
    let challenge = || async {
        match auth
            .login(password("alice@pve", "alice-pw"), client)
            .await
            .unwrap()
        {
            LoginResponse::SecondFactor { challenge, .. } => challenge,
            other => panic!("{other:?}"),
        }
    };
    let totp = |challenge: &str, code: &str| LoginRequest::SecondFactor {
        challenge: challenge.into(),
        method: SecondFactor::Totp,
        response: code.into(),
    };
    let late = challenge().await;
    lab.advance(CHALLENGE_TTL_MS);
    let error = auth.login(totp(&late, "123456"), client).await.unwrap_err();
    assert_eq!(
        error.message,
        "the login took too long; start again with your password"
    );

    let tries = challenge().await;
    for _ in 0..2 {
        assert_eq!(
            auth.login(totp(&tries, "000000"), client)
                .await
                .unwrap_err()
                .message,
            "that code is wrong; try again"
        );
    }
    let last = auth
        .login(totp(&tries, "000000"), client)
        .await
        .unwrap_err();
    assert_eq!(
        last.message,
        "that code is wrong; start again with your password"
    );
    assert_eq!(
        auth.login(totp(&tries, "123456"), client)
            .await
            .unwrap_err()
            .code,
        ErrorCode::Unauthenticated
    );
}

#[tokio::test]
async fn refusals_say_why_without_saying_too_much() {
    let lab = Lab::new().await;
    let auth = lab.auth("mox1").await;
    let client = ip("100.64.0.9");
    let wrong = auth
        .login(password("bob@pve", "nope"), client)
        .await
        .unwrap_err();
    let disabled = auth
        .login(password("carol@pve", "carol-pw"), client)
        .await
        .unwrap_err();
    for error in [&wrong, &disabled] {
        assert_eq!(error.code, ErrorCode::Unauthenticated);
        assert_eq!(
            error.message,
            "the user name or password is wrong, or the account is disabled"
        );
    }
    let no_group = auth
        .login(password("erin@pve", "erin-pw"), client)
        .await
        .unwrap_err();
    assert_eq!(no_group.code, ErrorCode::Forbidden);
    assert!(
        no_group
            .message
            .contains("bmac-viewer, bmac-operator, or bmac-admin"),
        "{}",
        no_group.message
    );
    let webauthn = auth
        .login(password("wendy@pve", "wendy-pw"), client)
        .await
        .unwrap_err();
    assert_eq!(webauthn.code, ErrorCode::Forbidden);
    assert!(
        webauthn.message.contains("add TOTP or recovery keys"),
        "{}",
        webauthn.message
    );
    let realmless = auth
        .login(password("bob", "bob-pw"), client)
        .await
        .unwrap_err();
    assert_eq!(realmless.code, ErrorCode::Invalid);
    assert_eq!(
        lab.hits.load(Ordering::SeqCst),
        4,
        "an invalid user name never reaches Proxmox"
    );
    assert_eq!(
        audit_kinds(&auth).await,
        [
            "login_failed",
            "login_failed",
            "login_refused",
            "login_refused"
        ]
    );
}

#[tokio::test]
async fn only_the_nodes_own_certificate_is_trusted() {
    let lab = Lab::new().await;
    let mut config = lab.config("mox1");
    let other = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
    let other_path = lab.dir.path().join("other.pem");
    std::fs::write(&other_path, other.cert.pem()).unwrap();
    config.proxmox.certificates = vec![other_path];
    let auth = lab.auth_with("mox1", config).await;
    let error = auth
        .login(password("bob@pve", "bob-pw"), ip("100.64.0.9"))
        .await
        .unwrap_err();
    assert_eq!(error.code, ErrorCode::Unavailable);
    assert_eq!(lab.hits.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn sessions_go_idle_and_expire() {
    let lab = Lab::new().await;
    let auth = lab.auth("mox1").await;
    let bob = session(
        auth.login(password("bob@pve", "bob-pw"), ip("100.64.0.9"))
            .await
            .unwrap(),
    );
    lab.advance(29 * 60 * 1000);
    auth.authenticate(Some(&bob.token), true).await.unwrap();
    lab.advance(29 * 60 * 1000);
    auth.authenticate(Some(&bob.token), false).await.unwrap();
    lab.advance(60 * 1000);
    let idle = auth.authenticate(Some(&bob.token), true).await.unwrap_err();
    assert_eq!(idle.code, ErrorCode::Unauthenticated);
    assert!(
        idle.message.contains("without activity"),
        "{}",
        idle.message
    );

    let bob = session(
        auth.login(password("bob@pve", "bob-pw"), ip("100.64.0.9"))
            .await
            .unwrap(),
    );
    for _ in 0..47 {
        lab.advance(15 * 60 * 1000);
        auth.authenticate(Some(&bob.token), true).await.unwrap();
    }
    lab.advance(15 * 60 * 1000 - 1);
    auth.authenticate(Some(&bob.token), true).await.unwrap();
    lab.advance(1);
    auth.maintain().await;
    let ended = auth.authenticate(Some(&bob.token), true).await.unwrap_err();
    assert_eq!(ended.message, "the session has ended; log in again");
    assert!(
        audit_kinds(&auth)
            .await
            .ends_with(&["session_ended".to_string()])
    );
}

#[tokio::test]
async fn sessions_survive_an_engine_restart() {
    let lab = Lab::new().await;
    let token = {
        let auth = lab.auth("mox1").await;
        session(
            auth.login(password("bob@pve", "bob-pw"), ip("100.64.0.9"))
                .await
                .unwrap(),
        )
        .token
    };
    let auth = lab.auth("mox1").await;
    assert_eq!(
        auth.authenticate(Some(&token), true).await.unwrap().user,
        "bob@pve"
    );
}

struct Socket {
    _dir: tempfile::TempDir,
    client: EngineClient,
    stop: Option<oneshot::Sender<()>>,
}

impl Drop for Socket {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
    }
}

fn serve(auth: Arc<Auth>) -> Socket {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("engine.sock");
    let listener = server::bind(&path, None).unwrap();
    let (stop, stopped) = oneshot::channel::<()>();
    let me = rustix::process::getuid().as_raw();
    tokio::spawn(server::serve(
        listener,
        AllowedPeers::new([me]),
        Arc::new(Engine::new("v", auth)),
        async {
            let _ = stopped.await;
        },
    ));
    Socket {
        client: EngineClient::new(&path),
        _dir: dir,
        stop: Some(stop),
    }
}

async fn ends_with(stream: &mut bmacd::web::client::EngineStream, needle: &str) {
    loop {
        match tokio::time::timeout(Duration::from_secs(5), stream.next()).await {
            Ok(Some(EngineOutcome::Event { message })) => match message.body {
                StreamBody::SessionEnded { message } => {
                    assert!(message.contains(needle), "{message}");
                    return;
                }
                StreamBody::Hello { .. } => {}
                other => panic!("unexpected {other:?}"),
            },
            other => panic!("expected the stream to end, got {other:?}"),
        }
    }
}

#[tokio::test]
async fn logging_out_everywhere_closes_streams_on_every_host() {
    let lab = Lab::new().await;
    let (mox1, mox2) = (lab.auth("mox1").await, lab.auth("mox2").await);
    let client = ip("100.64.0.9");
    let here = session(
        mox1.login(password("bob@pve", "bob-pw"), client)
            .await
            .unwrap(),
    );
    let other_tab = session(
        mox1.login(password("bob@pve", "bob-pw"), client)
            .await
            .unwrap(),
    );
    let there = session(
        mox2.login(password("bob@pve", "bob-pw"), client)
            .await
            .unwrap(),
    );
    let alice_tfa = mox2
        .login(password("alice@pve", "alice-pw"), client)
        .await
        .unwrap();
    let LoginResponse::SecondFactor { challenge, .. } = alice_tfa else {
        panic!()
    };
    let alice = session(
        mox2.login(
            LoginRequest::SecondFactor {
                challenge,
                method: SecondFactor::Totp,
                response: "123456".into(),
            },
            client,
        )
        .await
        .unwrap(),
    );

    let (socket1, socket2) = (serve(mox1.clone()), serve(mox2.clone()));
    let subscribe = || EngineCall::Subscribe {
        last_event_id: None,
    };
    let mut stream1 = socket1
        .client
        .stream(client, Some(other_tab.token.clone()), subscribe())
        .await
        .unwrap();
    let mut stream2 = socket2
        .client
        .stream(client, Some(there.token.clone()), subscribe())
        .await
        .unwrap();
    let mut alice_stream = socket2
        .client
        .stream(client, Some(alice.token.clone()), subscribe())
        .await
        .unwrap();
    assert!(matches!(
        alice_stream.next().await,
        Some(EngineOutcome::Event { message }) if matches!(message.body, StreamBody::Hello { .. })
    ));
    tokio::time::sleep(Duration::from_millis(50)).await;

    lab.advance(1);
    let logout = EngineCall::Logout {
        request: bmac_proto::api::LogoutRequest { everywhere: true },
    };
    let outcome = socket1
        .client
        .call(client, Some(here.token.clone()), logout)
        .await
        .unwrap();
    assert_eq!(outcome, EngineOutcome::Ok { body: json!({}) });
    ends_with(&mut stream1, "logging out everywhere").await;
    assert_eq!(
        mox2.authenticate(Some(&there.token), true)
            .await
            .unwrap()
            .user,
        "bob@pve",
        "until mox2 reads pmxcfs"
    );

    mox2.maintain().await;
    ends_with(&mut stream2, "logging out everywhere").await;
    assert!(mox2.authenticate(Some(&there.token), true).await.is_err());
    assert!(
        mox2.authenticate(Some(&alice.token), true).await.is_ok(),
        "only bob's sessions end"
    );
    assert!(
        tokio::time::timeout(Duration::from_millis(100), alice_stream.next())
            .await
            .is_err(),
        "alice's stream stays open"
    );

    lab.advance(1);
    let again = session(
        mox2.login(password("bob@pve", "bob-pw"), client)
            .await
            .unwrap(),
    );
    mox2.maintain().await;
    assert!(
        mox2.authenticate(Some(&again.token), true).await.is_ok(),
        "a later login is not revoked"
    );
    assert_eq!(
        audit_kinds(&mox1)
            .await
            .iter()
            .filter(|k| *k == "session_ended")
            .count(),
        2
    );
}

#[tokio::test]
async fn account_changes_end_sessions_at_the_recheck() {
    let lab = Lab::new().await;
    let auth = lab.auth("mox1").await;
    let client = ip("100.64.0.9");
    let bob = session(
        auth.login(password("bob@pve", "bob-pw"), client)
            .await
            .unwrap(),
    );
    let root = session(
        auth.login(password("root@pam", "root-pw"), client)
            .await
            .unwrap(),
    );
    let socket = serve(auth.clone());
    let mut stream = socket
        .client
        .stream(
            client,
            Some(bob.token.clone()),
            EngineCall::Subscribe {
                last_event_id: None,
            },
        )
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;

    std::fs::write(
        lab.user_cfg(),
        USER_CFG.replace("bob@pve,wendy@pve", "wendy@pve"),
    )
    .unwrap();
    lab.advance(4 * 60 * 1000);
    auth.maintain().await;
    assert!(
        auth.authenticate(Some(&bob.token), true).await.is_ok(),
        "not due yet"
    );
    lab.advance(60 * 1000);
    auth.maintain().await;
    ends_with(&mut stream, "in no BMAC group").await;
    assert!(auth.authenticate(Some(&root.token), true).await.is_ok());

    let refused = auth
        .login(password("bob@pve", "bob-pw"), client)
        .await
        .unwrap_err();
    assert_eq!(refused.code, ErrorCode::Forbidden);

    std::fs::write(
        lab.user_cfg(),
        USER_CFG.replace(
            "group:bmac-admin:alice@pve",
            "group:bmac-admin:alice@pve,bob@pve",
        ),
    )
    .unwrap();
    let promoted = session(
        auth.login(password("bob@pve", "bob-pw"), client)
            .await
            .unwrap(),
    );
    assert_eq!(promoted.role, Role::Admin);
    std::fs::write(
        lab.user_cfg(),
        USER_CFG.replace("user:bob@pve:1:", "user:bob@pve:0:"),
    )
    .unwrap();
    auth.recheck_accounts().await;
    let ended = auth
        .authenticate(Some(&promoted.token), true)
        .await
        .unwrap_err();
    assert_eq!(ended.message, "the session has ended; log in again");

    std::fs::remove_file(lab.user_cfg()).unwrap();
    auth.recheck_accounts().await;
    assert!(
        auth.authenticate(Some(&root.token), true).await.is_ok(),
        "an unreadable account list ends nothing"
    );
}

#[tokio::test]
async fn login_attempts_are_rate_limited() {
    let lab = Lab::new().await;
    let mut config = lab.config("mox1");
    config.limits = LoginLimits {
        per_address: Window {
            limit: 6,
            period_ms: 60_000,
        },
        per_user: Window {
            limit: 3,
            period_ms: 600_000,
        },
    };
    let auth = lab.auth_with("mox1", config).await;
    for n in 0..3 {
        let address = ip(&format!("100.64.0.{}", 10 + n));
        assert_eq!(
            auth.login(password("bob@pve", "wrong"), address)
                .await
                .unwrap_err()
                .code,
            ErrorCode::Unauthenticated
        );
    }
    let limited = auth
        .login(password("bob@pve", "bob-pw"), ip("100.64.0.20"))
        .await
        .unwrap_err();
    assert_eq!(limited.code, ErrorCode::RateLimited);
    assert_eq!(limited.retry_after_seconds, Some(600));
    assert!(
        auth.login(password("BOB@pve", "bob-pw"), ip("100.64.0.21"))
            .await
            .is_err(),
        "user names are compared without case"
    );
    session(
        auth.login(password("root@pam", "root-pw"), ip("100.64.0.20"))
            .await
            .unwrap(),
    );
    assert_eq!(
        lab.hits.load(Ordering::SeqCst),
        4,
        "limited attempts never reach Proxmox"
    );

    let busy = ip("100.64.0.30");
    for _ in 0..6 {
        session(
            auth.login(password("root@pam", "root-pw"), busy)
                .await
                .unwrap(),
        );
    }
    let limited = auth
        .login(password("root@pam", "root-pw"), busy)
        .await
        .unwrap_err();
    assert_eq!(
        (limited.code, limited.retry_after_seconds),
        (ErrorCode::RateLimited, Some(60))
    );
    lab.advance(60_000);
    session(
        auth.login(password("root@pam", "root-pw"), busy)
            .await
            .unwrap(),
    );
    let kinds = audit_kinds(&auth).await;
    assert_eq!(
        kinds.iter().filter(|k| *k == "login_rate_limited").count(),
        2,
        "{kinds:?}"
    );
}

async fn https_post(
    address: SocketAddr,
    der: &[u8],
    path: &str,
    headers: &[(&str, &str)],
    body: Value,
) -> (StatusCode, Value) {
    use http_body_util::{BodyExt, Full};
    use hyper_util::rt::TokioIo;
    let mut config = bmacd::pinned::Pinned::new(vec![der.to_vec()])
        .client_config(&[&rustls::version::TLS13])
        .unwrap();
    config.alpn_protocols = vec![b"http/1.1".to_vec()];
    let tcp = tokio::net::TcpStream::connect(address).await.unwrap();
    let name = rustls::pki_types::ServerName::try_from("localhost").unwrap();
    let tls = tokio_rustls::TlsConnector::from(Arc::new(config))
        .connect(name, tcp)
        .await
        .unwrap();
    let (mut sender, connection) = hyper::client::conn::http1::handshake(TokioIo::new(tls))
        .await
        .unwrap();
    tokio::spawn(connection);
    let mut request = hyper::Request::post(path).header("host", "localhost");
    for (name, value) in headers {
        request = request.header(*name, *value);
    }
    let request = request
        .body(Full::new(Bytes::from(body.to_string())))
        .unwrap();
    let response = sender.send_request(request).await.unwrap();
    let status = response.status();
    assert_eq!(response.headers()["cache-control"], "no-store");
    assert!(response.headers().get("set-cookie").is_none());
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (status, serde_json::from_slice(&bytes).unwrap())
}

#[tokio::test]
async fn the_browser_logs_in_and_out_through_the_front_end() {
    let lab = Lab::new().await;
    let auth = lab.auth("mox1").await;
    let socket = serve(auth.clone());
    let generated = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
    let config = tls::server_config(
        generated.cert.pem().as_bytes(),
        generated.key_pair.serialize_pem().as_bytes(),
    )
    .unwrap();
    let engine = Arc::new(EngineClient::new(socket_path(&socket)));
    let router = app::router(Arc::new(Assets::from_files(Vec::new())), engine);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = HttpsServer::new(config, router);
    tokio::spawn(async move { server.serve(listener).await });
    let der = generated.cert.der().to_vec();
    let json = ("content-type", "application/json");

    let (status, body) = https_post(
        address,
        &der,
        "/api/v1/login",
        &[json],
        json!({"step": "password", "user": "bob@pve", "password": "bob-pw"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        (body["step"].as_str(), body["role"].as_str()),
        (Some("session"), Some("operator"))
    );
    let token = body["token"].as_str().unwrap().to_string();
    assert_eq!(
        auth.audit().recent(1).await[0].client.as_deref(),
        Some("127.0.0.1")
    );

    let (status, body) = https_post(
        address,
        &der,
        "/api/v1/login",
        &[json],
        json!({"step": "password", "user": "bob@pve", "password": "x", "remember": true}),
    )
    .await;
    assert_eq!(
        (status, body["code"].as_str()),
        (StatusCode::UNPROCESSABLE_ENTITY, Some("invalid"))
    );
    let (status, _) = https_post(
        address,
        &der,
        "/api/v1/login",
        &[("content-type", "text/plain")],
        json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    let (status, body) = https_post(
        address,
        &der,
        "/api/v1/login",
        &[json, ("origin", "https://evil.example")],
        json!({"step": "password", "user": "bob@pve", "password": "bob-pw"}),
    )
    .await;
    assert_eq!(
        (status, body["code"].as_str()),
        (StatusCode::FORBIDDEN, Some("forbidden"))
    );
    let (status, _) = https_post(
        address,
        &der,
        "/api/v1/login",
        &[json, ("origin", "https://localhost")],
        json!({"step": "password", "user": "bob@pve", "password": "nope"}),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    let bearer = format!("Bearer {token}");
    let (status, _) = https_post(address, &der, "/api/v1/logout", &[json], json!({})).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let (status, body) = https_post(
        address,
        &der,
        "/api/v1/logout",
        &[json, ("authorization", &bearer)],
        json!({}),
    )
    .await;
    assert_eq!((status, body), (StatusCode::OK, json!({})));
    let (status, body) = https_post(
        address,
        &der,
        "/api/v1/logout",
        &[json, ("authorization", &bearer)],
        json!({}),
    )
    .await;
    assert_eq!(
        (status, body["message"].as_str()),
        (
            StatusCode::UNAUTHORIZED,
            Some("the session has ended; log in again")
        )
    );
}

fn socket_path(socket: &Socket) -> PathBuf {
    socket._dir.path().join("engine.sock")
}
