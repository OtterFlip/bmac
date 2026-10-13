// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

//! The API end to end: HTTPS front end, engine socket, engine, and session
//! table, with sessions created directly (login itself is tested in
//! `login.rs`).

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use axum::body::Bytes;
use bmac_core::job::{EventBody, JobEvent, LogLevel};
use bmac_core::{GuestName, HostSlot, JobId};
use bmac_pmxcfs::{Pmxcfs, RealFs};
use bmac_proto::api::Role;
use bmac_proto::stream::StreamBody;
use bmacd::auth::proxmox::ProxmoxTickets;
use bmacd::auth::revocations::Revocations;
use bmacd::auth::{Auth, AuthConfig, system_clock};
use bmacd::certificate::Certificates;
use bmacd::cluster::{ClusterView, Membership};
use bmacd::db::Database;
use bmacd::engine::events::EventHub;
use bmacd::engine::{Engine, server};
use bmacd::identity::Identities;
use bmacd::jobs::{Jobs, JobsConfig};
use bmacd::notify::store::NotifyStore;
use bmacd::notify::{Notifier, NotifierConfig};
use bmacd::operations::Registry;
use bmacd::peers::AllowedPeers;
use bmacd::settings::Settings;
use bmacd::web::app::{self, AppState};
use bmacd::web::assets::Assets;
use bmacd::web::client::EngineClient;
use bmacd::web::https::HttpsServer;
use bmacd::web::tls;
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use hyper::{Response, StatusCode};
use hyper_util::rt::TokioIo;
use serde_json::{Value, json};
use tokio::net::TcpListener;
use tokio::sync::oneshot;

const JOB: &str = "6f9619ff-8b86-4011-b42d-00c04fc964ff";

/// Three online hosts, with prod1 on mox1.
struct OneCluster;

impl ClusterView for OneCluster {
    fn membership(&self) -> Result<Membership, String> {
        Ok(Membership {
            quorate: true,
            online: (1..=3).map(|n| HostSlot::new(n).unwrap()).collect(),
            members: (1..=3).map(|n| (HostSlot::new(n).unwrap(), None)).collect(),
        })
    }

    fn guest_owner(&self, guest: GuestName) -> Result<Option<HostSlot>, String> {
        Ok((guest.to_string() == "prod1").then(|| HostSlot::new(1).unwrap()))
    }

    fn placement(&self, _guest: GuestName) -> Result<Vec<HostSlot>, String> {
        Ok(Vec::new())
    }
}

struct Stack {
    _dir: tempfile::TempDir,
    address: SocketAddr,
    der: Vec<u8>,
    auth: Arc<Auth>,
    hub: Arc<EventHub>,
    tls_dir: std::path::PathBuf,
    switches: Arc<std::sync::atomic::AtomicUsize>,
    setup: bmacd::native::setup::Paths,
    _stop: oneshot::Sender<()>,
}

impl Stack {
    async fn new(keepalive: Duration) -> Stack {
        let dir = tempfile::tempdir().unwrap();
        let pve = dir.path().join("pve");
        let config = AuthConfig {
            proxmox: ProxmoxTickets {
                address: "127.0.0.1:9".parse().unwrap(),
                certificates: Vec::new(),
            },
            user_cfg: dir.path().join("user.cfg"),
            revocations: Revocations::new(Pmxcfs::new(&pve, Arc::new(RealFs)), pve.join("revoked")),
            policy: Default::default(),
            limits: Default::default(),
            host: "mox1".into(),
            clock: system_clock(),
        };
        let db = Database::open(&dir.path().join("bmac.db")).unwrap();
        let auth = Arc::new(Auth::new(config, db).await.unwrap());
        let jobs = JobsConfig {
            dir: pve.join("priv/bmac/jobs"),
            pmxcfs: Pmxcfs::new(&pve, Arc::new(RealFs)),
            ..JobsConfig::system(HostSlot::new(1).unwrap(), system_clock())
        };
        let jobs = Jobs::new(
            jobs,
            Arc::new(Registry::builtin().unwrap()),
            Arc::new(OneCluster),
        );
        let generated = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
        let tls_dir = dir.path().join("tls");
        std::fs::create_dir(&tls_dir).unwrap();
        std::fs::write(tls_dir.join("https.pem"), generated.cert.pem()).unwrap();
        std::fs::write(
            tls_dir.join("https.key"),
            generated.key_pair.serialize_pem(),
        )
        .unwrap();
        let switches = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter = switches.clone();
        let certificates = Certificates::new(
            &tls_dir,
            "mox1",
            Arc::new(move || {
                counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            }),
        );
        let setup = bmacd::native::setup::Paths {
            bootstrap: dir.path().join("bootstrap.json"),
            done: dir.path().join("setup-complete"),
            cluster_record: dir.path().join("cluster.json"),
            join_state: dir.path().join("join.json"),
        };
        let engine = Engine::new("v", auth.clone())
            .with_setup(setup.clone())
            .with_certificates(certificates)
            .with_jobs(Arc::new(jobs))
            .with_settings(
                Settings::new(
                    Pmxcfs::new(&pve, Arc::new(RealFs)),
                    pve.join("priv/bmac/config"),
                ),
                Identities::new(
                    Pmxcfs::new(&pve, Arc::new(RealFs)),
                    pve.join("priv/bmac/identity"),
                ),
            );
        let hub = engine.hub().clone();
        let notifier = Notifier::new(NotifierConfig {
            host: HostSlot::new(1).unwrap(),
            cluster: Arc::new(OneCluster),
            conditions: Arc::new(|| (Some("lab".into()), Vec::new())),
            hub: hub.clone(),
            clock: system_clock(),
            zone: jiff::tz::TimeZone::UTC,
            store: NotifyStore::new(
                Pmxcfs::new(&pve, Arc::new(RealFs)),
                pve.join("priv/bmac/notify"),
                pve.join("priv/bmac/secrets/postmark.token"),
            ),
            local: NotifyStore::new(
                Pmxcfs::new(dir.path().join("local"), Arc::new(RealFs)),
                dir.path().join("local/notify"),
                dir.path().join("local/postmark.token"),
            ),
            connect: Arc::new(|_| Err("no email from tests".to_string())),
        });
        let engine = engine.with_notifier(Arc::new(notifier));

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

        let tls = tls::server_config(
            generated.cert.pem().as_bytes(),
            generated.key_pair.serialize_pem().as_bytes(),
        )
        .unwrap();
        let state = AppState {
            assets: Arc::new(Assets::from_files(Vec::new())),
            engine: Arc::new(EngineClient::new(&socket)),
            keepalive,
        };
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = HttpsServer::new(tls, app::router_with(state));
        tokio::spawn(async move { server.serve(listener).await });
        Stack {
            _dir: dir,
            address,
            der: generated.cert.der().to_vec(),
            auth,
            hub,
            tls_dir,
            switches,
            setup,
            _stop: stop,
        }
    }

    async fn session(&self, user: &str) -> String {
        self.session_as(user, Role::Operator).await
    }

    async fn session_as(&self, user: &str, role: Role) -> String {
        let client = "127.0.0.1".parse().unwrap();
        self.auth
            .sessions()
            .create(user, role, client, now())
            .await
            .unwrap()
            .0
    }

    async fn send(
        &self,
        method: &str,
        path: &str,
        headers: &[(&str, &str)],
        body: &str,
    ) -> Response<Incoming> {
        let mut config = bmacd::pinned::Pinned::new(vec![self.der.clone()])
            .client_config(&[&rustls::version::TLS13])
            .unwrap();
        config.alpn_protocols = vec![b"http/1.1".to_vec()];
        let tcp = tokio::net::TcpStream::connect(self.address).await.unwrap();
        let name = rustls::pki_types::ServerName::try_from("localhost").unwrap();
        let tls = tokio_rustls::TlsConnector::from(Arc::new(config))
            .connect(name, tcp)
            .await
            .unwrap();
        let (mut sender, connection) = hyper::client::conn::http1::handshake(TokioIo::new(tls))
            .await
            .unwrap();
        tokio::spawn(connection);
        let mut request = hyper::Request::builder()
            .method(method)
            .uri(path)
            .header("host", "localhost");
        for (name, value) in headers {
            request = request.header(*name, *value);
        }
        sender
            .send_request(
                request
                    .body(Full::new(Bytes::from(body.to_string())))
                    .unwrap(),
            )
            .await
            .unwrap()
    }

    async fn json(
        &self,
        method: &str,
        path: &str,
        headers: &[(&str, &str)],
        body: &str,
    ) -> (StatusCode, Value) {
        let response = self.send(method, path, headers, body).await;
        let status = response.status();
        assert_eq!(
            response.headers()["cache-control"],
            "no-store",
            "{method} {path}"
        );
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        )
    }
}

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64
}

/// Every endpoint, with a body that fits it.
fn endpoints() -> Vec<(&'static str, String, String)> {
    let launch =
        json!({"operation": "prod.add", "parameters": {}, "idempotency_key": "k1"}).to_string();
    let watch = json!({"stream": "s", "jobs": [JOB]}).to_string();
    vec![
        ("POST", "/api/v1/logout".into(), "{}".into()),
        ("GET", "/api/v1/cluster".into(), String::new()),
        ("GET", "/api/v1/hosts".into(), String::new()),
        ("GET", "/api/v1/vms".into(), String::new()),
        ("GET", "/api/v1/storage".into(), String::new()),
        ("GET", "/api/v1/backups".into(), String::new()),
        ("GET", "/api/v1/alerts".into(), String::new()),
        (
            "POST",
            "/api/v1/alerts/0a1b2c3d4e5f/acknowledge".into(),
            String::new(),
        ),
        (
            "POST",
            "/api/v1/silences".into(),
            json!({"condition": "zfs.", "until_ms": 1, "reason": "work"}).to_string(),
        ),
        (
            "POST",
            "/api/v1/silences/0a1b2c3d4e5f/delete".into(),
            "{}".into(),
        ),
        (
            "PUT",
            "/api/v1/notify/settings".into(),
            json!({
                "settings": {
                    "sender": null,
                    "recipients": [],
                    "routes": [],
                    "throttle_minutes": 60,
                    "digest_hour": null,
                    "dashboard_url": null
                },
                "expected_revision": null
            })
            .to_string(),
        ),
        (
            "PUT",
            "/api/v1/notify/token".into(),
            json!({"token": ""}).to_string(),
        ),
        (
            "POST",
            "/api/v1/notify/test".into(),
            json!({"to": null}).to_string(),
        ),
        ("GET", "/api/v1/notify/templates".into(), String::new()),
        ("POST", "/api/v1/notify/templates".into(), String::new()),
        (
            "POST",
            "/api/v1/logs/search".into(),
            json!({"limit": 10}).to_string(),
        ),
        ("GET", "/api/v1/logs/replication".into(), String::new()),
        ("GET", "/api/v1/operations".into(), String::new()),
        ("GET", "/api/v1/setup".into(), String::new()),
        ("GET", "/api/v1/settings".into(), String::new()),
        (
            "PUT",
            "/api/v1/settings/cluster".into(),
            json!({"values": {}, "expected_revision": null}).to_string(),
        ),
        (
            "PUT",
            "/api/v1/settings/hosts/mox2".into(),
            json!({"values": {}, "expected_revision": null}).to_string(),
        ),
        ("GET", "/api/v1/certificate".into(), String::new()),
        ("POST", "/api/v1/certificate/rotate".into(), String::new()),
        (
            "POST",
            "/api/v1/certificate/switch".into(),
            json!({"fingerprint": "AB:CD"}).to_string(),
        ),
        ("POST", "/api/v1/certificate/discard".into(), "{}".into()),
        ("GET", "/api/v1/jobs".into(), String::new()),
        ("POST", "/api/v1/jobs".into(), launch),
        (
            "GET",
            format!("/api/v1/jobs/{JOB}?from_seq=3"),
            String::new(),
        ),
        (
            "POST",
            format!("/api/v1/jobs/{JOB}/requests/r1"),
            json!({"kind": "acknowledge"}).to_string(),
        ),
        ("POST", format!("/api/v1/jobs/{JOB}/cancel"), String::new()),
        ("POST", format!("/api/v1/jobs/{JOB}/resume"), String::new()),
        ("POST", format!("/api/v1/jobs/{JOB}/reconcile"), "{}".into()),
        ("GET", "/api/v1/events".into(), String::new()),
        ("POST", "/api/v1/events/watch".into(), watch),
    ]
}

#[tokio::test]
async fn every_endpoint_needs_a_live_session() {
    let stack = Stack::new(Duration::from_secs(15)).await;
    let json = ("content-type", "application/json");
    let ended = stack.session("bob@pve").await;
    let hash = bmacd::auth::sessions::hash_token(&ended);
    stack
        .auth
        .sessions()
        .end(&hash, bmacd::auth::sessions::EndReason::LoggedOut)
        .await
        .unwrap();
    let unknown = format!("Bearer {}", "A".repeat(43));
    let ended = format!("Bearer {ended}");
    for (method, path, body) in endpoints() {
        let (status, error) = stack.json(method, &path, &[json], &body).await;
        assert_eq!(
            (status, error["message"].as_str()),
            (StatusCode::UNAUTHORIZED, Some("log in first")),
            "{method} {path}"
        );
        let (status, _) = stack
            .json(
                method,
                &path,
                &[json, ("authorization", "Bearer short")],
                &body,
            )
            .await;
        assert_eq!(
            status,
            StatusCode::UNAUTHORIZED,
            "{method} {path}: a malformed token"
        );
        for token in [&unknown, &ended] {
            let (status, error) = stack
                .json(method, &path, &[json, ("authorization", token)], &body)
                .await;
            assert_eq!(status, StatusCode::UNAUTHORIZED, "{method} {path}");
            assert_eq!(
                error["message"], "the session has ended; log in again",
                "{method} {path}"
            );
        }
    }
}

#[tokio::test]
async fn requests_are_checked_before_they_reach_the_engine() {
    let stack = Stack::new(Duration::from_secs(15)).await;
    let bearer = format!("Bearer {}", stack.session("bob@pve").await);
    let auth = ("authorization", bearer.as_str());
    let json = ("content-type", "application/json");

    for (method, path, body) in endpoints() {
        let evil = ("origin", "https://evil.example");
        let (status, error) = stack.json(method, &path, &[json, auth, evil], &body).await;
        assert_eq!(
            (status, error["code"].as_str()),
            (StatusCode::FORBIDDEN, Some("forbidden")),
            "{method} {path}"
        );
    }
    let (status, _) = stack
        .json(
            "GET",
            "/api/v1/cluster",
            &[auth, ("origin", "https://localhost")],
            "",
        )
        .await;
    assert_eq!(
        status,
        StatusCode::SERVICE_UNAVAILABLE,
        "same origin passes; the engine has no cluster view yet"
    );

    type Case<'a> = (&'a str, &'a str, &'a [(&'a str, &'a str)], &'a str);
    let cases: [Case; 8] = [
        (
            "POST",
            "/api/v1/jobs",
            &[auth],
            r#"{"operation":"x","idempotency_key":"k"}"#,
        ),
        ("POST", "/api/v1/jobs", &[auth, json], "{not json"),
        (
            "POST",
            "/api/v1/jobs",
            &[auth, json],
            r#"{"operation":"x","idempotency_key":"k","argv":["sh"]}"#,
        ),
        ("GET", "/api/v1/jobs/not-a-job", &[auth], ""),
        (
            "GET",
            "/api/v1/jobs/6f9619ff-8b86-4011-b42d-00c04fc964ff?path=/etc",
            &[auth],
            "",
        ),
        (
            "POST",
            "/api/v1/jobs/6f9619ff-8b86-4011-b42d-00c04fc964ff/cancel",
            &[auth, json],
            r#"{"force":true,"signal":9}"#,
        ),
        (
            "POST",
            "/api/v1/events/watch",
            &[auth, json],
            r#"{"stream":"s"}"#,
        ),
        (
            "GET",
            "/api/v1/events",
            &[auth, ("last-event-id", "a b")],
            "",
        ),
    ];
    for (method, path, headers, body) in cases {
        let (status, error) = stack.json(method, path, headers, body).await;
        assert_eq!(
            (status, error["code"].as_str()),
            (StatusCode::UNPROCESSABLE_ENTITY, Some("invalid")),
            "{method} {path} {body}"
        );
    }

    let (status, error) = stack.json("DELETE", "/api/v1/jobs", &[auth], "").await;
    assert_eq!(
        (status, error["code"].as_str()),
        (StatusCode::METHOD_NOT_ALLOWED, Some("invalid"))
    );
    let (status, error) = stack.json("GET", "/api/v1/shell", &[auth], "").await;
    assert_eq!(
        (status, error["code"].as_str()),
        (StatusCode::NOT_FOUND, Some("not_found"))
    );
    let (status, error) = stack
        .json("POST", "/api/v1/watch", &[auth, json], "{}")
        .await;
    assert_eq!(
        (status, error["code"].as_str()),
        (StatusCode::NOT_FOUND, Some("not_found"))
    );
}

/// The required keys of v2's `cluster_dot_conf`.
fn cluster_settings() -> Value {
    json!({
        "PROXMOX_INTERNAL_DOMAIN": "pve.internal",
        "MAX_MOX_HOSTS": "10",
        "PRIVATE_SUBNET_CIDR": "10.213.0.0/24",
        "PROXMOX_MIGRATION_NETWORK": "10.213.0.240/28",
        "DATACENTER_PRIVATE_VLAN_GATEWAY": "10.213.0.1",
        "GUEST_EGRESS_VIP": "10.213.0.10/24",
        "MOX_IP_START": "10.213.0.11",
        "MOX_IP_END": "10.213.0.20",
        "MOX_REPLICATION_IP_START": "10.213.0.241",
        "MOX_REPLICATION_IP_END": "10.213.0.250",
        "HAPROXY_IP_START": "10.213.0.21",
        "HAPROXY_IP_END": "10.213.0.30",
        "PRODUCTION_IP_START": "10.213.0.31",
        "PRODUCTION_IP_END": "10.213.0.50",
        "STAGING_IP_START": "10.213.0.51",
        "STAGING_IP_END": "10.213.0.200",
    })
}

#[tokio::test]
async fn everyone_reads_settings_and_only_admins_save_them() {
    let stack = Stack::new(Duration::from_secs(15)).await;
    let client = "127.0.0.1".parse().unwrap();
    let create = |user: &'static str, role| {
        let auth = stack.auth.clone();
        async move {
            auth.sessions()
                .create(user, role, client, now())
                .await
                .unwrap()
                .0
        }
    };
    let viewer = format!("Bearer {}", create("vera@pve", Role::Viewer).await);
    let admin = format!("Bearer {}", create("root@pam", Role::Admin).await);
    let json = ("content-type", "application/json");

    let (status, view) = stack
        .json("GET", "/api/v1/settings", &[("authorization", &viewer)], "")
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(view["cluster"], Value::Null);
    assert_eq!(view["can_edit"], false);
    let key = |keys: &Value, name: &str| {
        keys.as_array()
            .unwrap()
            .iter()
            .find(|k| k["name"] == name)
            .cloned()
            .unwrap()
    };
    assert_eq!(
        key(&view["cluster_keys"], "MAX_MOX_HOSTS"),
        json!({"name": "MAX_MOX_HOSTS", "kind": "host_count", "choices": [], "required": true, "locked": true})
    );
    assert_eq!(
        key(&view["cluster_keys"], "PROD_GUEST_OS_INSTALL_MODE")["choices"],
        json!(["ubuntu-autoinstall", "manual"])
    );
    assert_eq!(key(&view["host_keys"], "PROXMOX_IP")["kind"], "ipv4");

    let save = json!({"values": cluster_settings(), "expected_revision": null}).to_string();
    let (status, error) = stack
        .json(
            "PUT",
            "/api/v1/settings/cluster",
            &[("authorization", &viewer), json],
            &save,
        )
        .await;
    assert_eq!(
        (status, error["code"].as_str()),
        (StatusCode::FORBIDDEN, Some("forbidden"))
    );

    let (status, saved) = stack
        .json(
            "PUT",
            "/api/v1/settings/cluster",
            &[("authorization", &admin), json],
            &save,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{saved}");
    assert_eq!(saved["updated_by"], "root@pam");
    let revision = saved["revision"].as_u64().unwrap();
    assert!(revision >= 1);

    let (status, error) = stack
        .json(
            "PUT",
            "/api/v1/settings/cluster",
            &[("authorization", &admin), json],
            &save,
        )
        .await;
    assert_eq!(
        (status, error["code"].as_str()),
        (StatusCode::CONFLICT, Some("conflict")),
        "created twice"
    );

    let mut values = cluster_settings();
    values["MOX_IP_START"] = json!("10.99.0.11");
    let bad = json!({"values": values, "expected_revision": revision}).to_string();
    let (status, error) = stack
        .json(
            "PUT",
            "/api/v1/settings/cluster",
            &[("authorization", &admin), json],
            &bad,
        )
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(
        error["field_errors"]["MOX_IP_START"]
            .as_str()
            .unwrap()
            .contains("10.213.0.0/24"),
        "{error}"
    );

    let (status, view) = stack
        .json("GET", "/api/v1/settings", &[("authorization", &admin)], "")
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(view["can_edit"], true);
    assert_eq!(view["cluster"]["revision"], revision);
    assert_eq!(view["cluster"]["values"]["MAX_MOX_HOSTS"], "10");

    let (status, error) = stack
        .json(
            "PUT",
            "/api/v1/settings/hosts/mox11",
            &[("authorization", &admin), json],
            &save,
        )
        .await;
    assert_eq!(
        (status, error["code"].as_str()),
        (StatusCode::UNPROCESSABLE_ENTITY, Some("invalid"))
    );

    let facts = json!({"values": {
        "PROXMOX_IP": "203.0.113.12",
        "PROXMOX_GATEWAY": "203.0.113.1",
        "PROXMOX_PREFIX": "24",
        "PROXMOX_PUBLIC_MAC": "aa:bb:cc:dd:ee:02",
        "PROXMOX_SECONDARY_MAC": "auto",
        "MAX_PROD_VM_COUNT_ON_THIS_HOST": "2",
        "MAX_STAGING_VM_COUNT_ON_THIS_HOST": "4",
        "NVME_MIRROR_0_SERIAL_1": "S6MOX2A0001",
        "NVME_MIRROR_0_SERIAL_2": "S6MOX2A0002",
        "NVME_MIRROR_0_CAPACITY_BYTES_1": "3840755982336",
        "NVME_MIRROR_0_CAPACITY_BYTES_2": "3840755982336",
    }, "expected_revision": null})
    .to_string();
    let (status, saved) = stack
        .json(
            "PUT",
            "/api/v1/settings/hosts/mox2",
            &[("authorization", &admin), json],
            &facts,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{saved}");
    assert_eq!(
        (&saved["host"], &saved["revision"]),
        (&json!("mox2"), &json!(1))
    );
    let (_, view) = stack
        .json("GET", "/api/v1/settings", &[("authorization", &viewer)], "")
        .await;
    let host = view["hosts"]
        .as_array()
        .unwrap()
        .iter()
        .find(|h| h["host"] == "mox2")
        .cloned()
        .unwrap();
    assert_eq!(host["values"]["PROXMOX_IP"], "203.0.113.12");

    let audit = stack.auth.audit().recent(20).await;
    let changes: Vec<_> = audit
        .iter()
        .filter(|e| e.kind == "settings_changed")
        .collect();
    assert_eq!(changes.len(), 2);
    assert!(
        changes
            .iter()
            .all(|c| c.user.as_deref() == Some("root@pam"))
    );
    assert_eq!(changes[0].message, "mox2's facts saved at revision 1");
    assert_eq!(
        changes[1].message,
        format!("cluster settings saved at revision {revision}")
    );
}

#[tokio::test]
async fn the_operation_list_shows_what_each_role_may_start() {
    let stack = Stack::new(Duration::from_secs(15)).await;
    let client = "127.0.0.1".parse().unwrap();
    let (viewer, _) = stack
        .auth
        .sessions()
        .create("vera@pve", Role::Viewer, client, now())
        .await
        .unwrap();
    let operator = stack.session("bob@pve").await;
    let allowed = |list: &Value, id: &str| {
        list["operations"]
            .as_array()
            .unwrap()
            .iter()
            .find(|o| o["id"] == id)
            .unwrap_or_else(|| panic!("{id} is listed"))["allowed"]
            .as_bool()
            .unwrap()
    };
    for (token, add, remove, health) in
        [(viewer, false, false, true), (operator, true, false, true)]
    {
        let bearer = format!("Bearer {token}");
        let (status, list) = stack
            .json(
                "GET",
                "/api/v1/operations",
                &[("authorization", &bearer)],
                "",
            )
            .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(allowed(&list, "prod.add"), add);
        assert_eq!(allowed(&list, "prod.remove"), remove);
        assert_eq!(allowed(&list, "diagnostics.cluster_health"), health);
        let text = list.to_string();
        assert!(
            !text.contains("scripts/") && !text.contains("--resource"),
            "scripts and flags stay on the server"
        );
        let remove = list["operations"]
            .as_array()
            .unwrap()
            .iter()
            .find(|o| o["id"] == "prod.remove")
            .unwrap();
        assert_eq!(remove["params"][0]["type"], "production");
        assert_eq!(
            remove["target"],
            json!({"rule": "owner", "param": "resource"})
        );
    }
}

/// Reads one SSE frame (comments included) from a streaming body.
struct Sse {
    body: Incoming,
    buffer: Vec<u8>,
}

impl Sse {
    fn new(response: Response<Incoming>) -> Sse {
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()["content-type"], "text/event-stream");
        assert_eq!(response.headers()["cache-control"], "no-store");
        Sse {
            body: response.into_body(),
            buffer: Vec::new(),
        }
    }

    async fn frame(&mut self) -> Option<String> {
        loop {
            if let Some(end) = self.buffer.windows(2).position(|w| w == b"\n\n") {
                let frame: Vec<u8> = self.buffer.drain(..end + 2).collect();
                return Some(String::from_utf8(frame).unwrap());
            }
            let next = tokio::time::timeout(Duration::from_secs(5), self.body.frame())
                .await
                .expect("a frame in time")?;
            if let Ok(data) = next.unwrap().into_data() {
                self.buffer.extend_from_slice(&data);
            }
        }
    }

    /// The next message, skipping keep-alives.
    async fn message(&mut self) -> Value {
        loop {
            let frame = self.frame().await.expect("the stream is open");
            if frame.starts_with(':') {
                continue;
            }
            let data = frame
                .lines()
                .find_map(|l| l.strip_prefix("data: "))
                .unwrap();
            let id = frame.lines().find_map(|l| l.strip_prefix("id: ")).unwrap();
            let message: Value = serde_json::from_str(data).unwrap();
            assert_eq!(message["id"], id);
            return message;
        }
    }
}

fn output(job: JobId, seq: u64, text: &str) -> StreamBody {
    StreamBody::JobEvent {
        event: JobEvent {
            job,
            seq,
            at_ms: 0,
            body: EventBody::Output {
                stream: None,
                level: LogLevel::Info,
                text: text.into(),
                source: None,
            },
        },
    }
}

#[tokio::test]
async fn the_event_stream_replays_watches_and_ends_with_the_session() {
    let stack = Stack::new(Duration::from_millis(300)).await;
    let bearer = format!("Bearer {}", stack.session("bob@pve").await);
    let auth = ("authorization", bearer.as_str());

    let mut events = Sse::new(stack.send("GET", "/api/v1/events", &[auth], "").await);
    let hello = events.message().await;
    assert_eq!(hello["type"], "hello");
    let stream = hello["stream"].as_str().unwrap().to_string();

    stack.hub.publish(StreamBody::Quorum { quorate: true });
    let first = events.message().await;
    assert_eq!(first["type"], "quorum");
    let keepalive = events.frame().await.unwrap();
    assert_eq!(keepalive, ": keep-alive\n\n");

    let watched: JobId = JOB.parse().unwrap();
    let other = JobId::random();
    let watch = json!({"stream": stream, "jobs": [JOB]}).to_string();
    let (status, _) = stack
        .json(
            "POST",
            "/api/v1/events/watch",
            &[auth, ("content-type", "application/json")],
            &watch,
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    stack.hub.publish(output(other, 1, "not watched"));
    stack.hub.publish(output(watched, 1, "watched"));
    let event = events.message().await;
    assert_eq!(
        (event["type"].as_str(), event["event"]["text"].as_str()),
        (Some("job_event"), Some("watched"))
    );

    let someone_else = format!("Bearer {}", stack.session("alice@pve").await);
    let (status, _) = stack
        .json(
            "POST",
            "/api/v1/events/watch",
            &[
                ("authorization", &someone_else),
                ("content-type", "application/json"),
            ],
            &watch,
        )
        .await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "another session can't steer this tab"
    );

    let last = event["id"].as_str().unwrap().to_string();
    drop(events);
    stack.hub.publish(StreamBody::Quorum { quorate: false });
    stack.hub.publish(StreamBody::Host {
        host: "mox2".parse().unwrap(),
        reachable: false,
        last_seen_ms: 1,
    });

    let mut events = Sse::new(
        stack
            .send(
                "GET",
                "/api/v1/events",
                &[auth, ("last-event-id", &last)],
                "",
            )
            .await,
    );
    assert_eq!(events.message().await["type"], "hello");
    let replayed = [events.message().await, events.message().await];
    assert_eq!(
        replayed.map(|m| m["type"].as_str().unwrap().to_string()),
        ["quorum", "host"]
    );

    let mut stale = Sse::new(
        stack
            .send(
                "GET",
                "/api/v1/events",
                &[auth, ("last-event-id", "0-1")],
                "",
            )
            .await,
    );
    assert_eq!(stale.message().await["type"], "hello");
    let resnapshot = stale.message().await;
    assert_eq!(resnapshot["type"], "resnapshot");
    assert!(resnapshot["reason"].as_str().unwrap().contains("restarted"));

    let (status, _) = stack
        .json(
            "POST",
            "/api/v1/logout",
            &[auth, ("content-type", "application/json")],
            "",
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    for stream in [&mut events, &mut stale] {
        let ended = stream.message().await;
        assert_eq!(
            (ended["type"].as_str(), ended["message"].as_str()),
            (Some("session_ended"), Some("you logged out"))
        );
        assert_eq!(
            stream.frame().await,
            None,
            "the stream closes after session_ended"
        );
    }
}

#[tokio::test]
async fn a_tab_that_never_reads_does_not_hold_up_others() {
    let stack = Stack::new(Duration::from_secs(15)).await;
    let bearer = format!("Bearer {}", stack.session("bob@pve").await);
    let auth = ("authorization", bearer.as_str());
    let mut stuck = Sse::new(stack.send("GET", "/api/v1/events", &[auth], "").await);
    assert_eq!(stuck.message().await["type"], "hello");
    let mut reader = Sse::new(stack.send("GET", "/api/v1/events", &[auth], "").await);
    assert_eq!(reader.message().await["type"], "hello");

    let text = "x".repeat(8000);
    let (batch, batches) = (200, 15);
    let (seen_tx, mut seen_rx) = tokio::sync::watch::channel(0usize);
    let reading = tokio::spawn(async move {
        let mut seen = 0;
        while seen < batch * batches {
            let message = reader.message().await;
            match message["type"].as_str() {
                Some("observation") => seen += 1,
                other => panic!("the reading tab got {other:?} after {seen}"),
            }
            seen_tx.send_replace(seen);
        }
    });
    for round in 0..batches {
        for seq in 0..batch {
            let data = json!({"seq": round * batch + seq, "pad": text});
            stack.hub.publish(StreamBody::Observation {
                host: "mox1".parse().unwrap(),
                collector: "test".into(),
                data,
            });
        }
        let target = (round + 1) * batch;
        tokio::time::timeout(
            Duration::from_secs(10),
            seen_rx.wait_for(|seen| *seen >= target),
        )
        .await
        .expect("the reading tab keeps up while another tab is stuck")
        .unwrap();
    }
    reading.await.unwrap();

    let mut kinds = Vec::new();
    while let Ok(Some(frame)) =
        tokio::time::timeout(Duration::from_millis(500), stuck.frame()).await
    {
        if frame.contains("\"type\":\"resnapshot\"") {
            kinds.push("resnapshot");
            break;
        }
    }
    assert_eq!(
        kinds,
        ["resnapshot"],
        "the stuck tab is told to fetch fresh snapshots"
    );
}

#[tokio::test]
async fn setup_mode_offers_only_the_setup_job_until_it_finishes() {
    let stack = Stack::new(Duration::from_secs(15)).await;
    let bearer = format!("Bearer {}", stack.session_as("root@pam", Role::Admin).await);
    let auth = ("authorization", bearer.as_str());
    let json = ("content-type", "application/json");
    let launch = |operation: &str, key: &str| {
        json!({"operation": operation, "parameters": {}, "idempotency_key": key}).to_string()
    };
    let offered = |list: &Value| -> Vec<String> {
        list["operations"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v["id"].as_str().unwrap().to_string())
            .collect()
    };

    let (status, view) = stack.json("GET", "/api/v1/setup", &[auth], "").await;
    assert_eq!(status, StatusCode::OK, "{view}");
    assert_eq!(view["active"], false);
    let (_, list) = stack.json("GET", "/api/v1/operations", &[auth], "").await;
    assert!(!offered(&list).contains(&"hosts.setup".to_string()));
    let (status, _) = stack
        .json(
            "POST",
            "/api/v1/jobs",
            &[auth, json],
            &launch("hosts.setup", "s0"),
        )
        .await;
    assert_eq!(
        status,
        StatusCode::CONFLICT,
        "no setup without a bootstrap file"
    );

    std::fs::write(&stack.setup.bootstrap, "{}").unwrap();
    let (_, view) = stack.json("GET", "/api/v1/setup", &[auth], "").await;
    assert_eq!(view["active"], true, "{view}");
    assert!(view.get("job").is_none());
    for path in ["/api/v1/cluster", "/api/v1/settings", "/api/v1/alerts"] {
        let (status, error) = stack.json("GET", path, &[auth], "").await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{path}: {error}");
    }
    let (_, list) = stack.json("GET", "/api/v1/operations", &[auth], "").await;
    assert_eq!(offered(&list), ["hosts.setup"]);
    let (status, _) = stack
        .json(
            "POST",
            "/api/v1/jobs",
            &[auth, json],
            &launch("prod.change_owner", "p1"),
        )
        .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    let (status, job) = stack
        .json(
            "POST",
            "/api/v1/jobs",
            &[auth, json],
            &launch("hosts.setup", "s1"),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{job}");
    assert_eq!(job["host"], "mox1");
    let (_, view) = stack.json("GET", "/api/v1/setup", &[auth], "").await;
    assert_eq!(view["job"]["id"], job["id"]);
    let (_, jobs) = stack.json("GET", "/api/v1/jobs", &[auth], "").await;
    assert_eq!(jobs["jobs"].as_array().unwrap().len(), 1);
    let path = format!("/api/v1/jobs/{}?from_seq=0", job["id"].as_str().unwrap());
    let (status, _) = stack.json("GET", &path, &[auth], "").await;
    assert_eq!(status, StatusCode::OK);

    std::fs::write(&stack.setup.done, "").unwrap();
    let (_, view) = stack.json("GET", "/api/v1/setup", &[auth], "").await;
    assert_eq!(view["active"], false);
    assert_eq!(
        view["job"]["id"], job["id"],
        "the finished setup job stays visible"
    );
    let (status, _) = stack.json("GET", "/api/v1/settings", &[auth], "").await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn jobs_are_launched_listed_and_cancelled() {
    let stack = Stack::new(Duration::from_secs(15)).await;
    let bearer = format!("Bearer {}", stack.session("bob@pve").await);
    let auth = ("authorization", bearer.as_str());
    let json = ("content-type", "application/json");
    let launch = json!({
        "operation": "prod.change_owner",
        "parameters": {"resource": "prod1"},
        "idempotency_key": "dialog-7"
    })
    .to_string();
    let (status, job) = stack
        .json("POST", "/api/v1/jobs", &[auth, json], &launch)
        .await;
    assert_eq!(status, StatusCode::OK, "{job}");
    assert_eq!(job["state"], "ADMITTED");
    assert_eq!(job["host"], "mox1");
    assert_eq!(job["user"], "bob@pve");
    let id = job["id"].as_str().unwrap().to_string();
    let (_, again) = stack
        .json("POST", "/api/v1/jobs", &[auth, json], &launch)
        .await;
    assert_eq!(
        again["id"],
        id.as_str(),
        "a repeated POST returns the same job"
    );

    let other = json!({
        "operation": "prod.extend_disk",
        "parameters": {"resource": "prod1"},
        "idempotency_key": "dialog-8"
    })
    .to_string();
    let (status, error) = stack
        .json("POST", "/api/v1/jobs", &[auth, json], &other)
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{error}");
    let bad = json!({
        "operation": "prod.extend_disk",
        "parameters": {"resource": "prod1; rm -rf /"},
        "idempotency_key": "dialog-9"
    })
    .to_string();
    let (status, error) = stack
        .json("POST", "/api/v1/jobs", &[auth, json], &bad)
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(error["field_errors"]["resource"], "expected prodN");
    let admin_only = json!({
        "operation": "prod.remove",
        "parameters": {"resource": "prod1"},
        "idempotency_key": "dialog-10"
    })
    .to_string();
    let (status, _) = stack
        .json("POST", "/api/v1/jobs", &[auth, json], &admin_only)
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    let (status, list) = stack.json("GET", "/api/v1/jobs", &[auth], "").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(list["jobs"].as_array().unwrap().len(), 1);
    let (status, view) = stack
        .json("GET", &format!("/api/v1/jobs/{id}"), &[auth], "")
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(view["parameters"], json!({"resource": "prod1"}));
    assert_eq!(view["locks"], json!(["production:prod1", "staging"]));
    let (status, _) = stack
        .json("GET", &format!("/api/v1/jobs/{JOB}"), &[auth], "")
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    let (status, cancelled) = stack
        .json(
            "POST",
            &format!("/api/v1/jobs/{id}/cancel"),
            &[auth, json],
            "{}",
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(cancelled["state"], "CANCELLED");
    let (status, error) = stack
        .json(
            "POST",
            &format!("/api/v1/jobs/{id}/resume"),
            &[auth, json],
            "{}",
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{error}");
    let (status, _) = stack
        .json(
            "POST",
            &format!("/api/v1/jobs/{id}/reconcile"),
            &[auth, json],
            r#"{"force":true}"#,
        )
        .await;
    assert_eq!(
        status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "reconcile takes no parameters"
    );

    let audit = stack.auth.audit().recent(10).await;
    let kinds: Vec<&str> = audit.iter().map(|e| e.kind.as_str()).collect();
    assert!(
        kinds.contains(&"job_launched") && kinds.contains(&"job_cancel_requested"),
        "{kinds:?}"
    );
    assert_eq!(
        kinds.iter().filter(|k| **k == "job_launched").count(),
        1,
        "a repeated POST is not a second launch"
    );
}

#[tokio::test]
async fn admins_rotate_the_certificate_after_seeing_the_new_fingerprint() {
    let stack = Stack::new(Duration::from_secs(15)).await;
    let client = "127.0.0.1".parse().unwrap();
    let create = |user: &'static str, role| {
        let auth = stack.auth.clone();
        async move {
            auth.sessions()
                .create(user, role, client, now())
                .await
                .unwrap()
                .0
        }
    };
    let viewer = format!("Bearer {}", create("vera@pve", Role::Viewer).await);
    let admin = format!("Bearer {}", create("root@pam", Role::Admin).await);
    let json = ("content-type", "application/json");

    let (status, view) = stack
        .json(
            "GET",
            "/api/v1/certificate",
            &[("authorization", &viewer)],
            "",
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(view["host"], "mox1");
    assert_eq!(view["can_rotate"], false);
    assert_eq!(view["pending"], Value::Null);
    let current = view["current"]["fingerprint"].as_str().unwrap().to_string();
    assert_eq!(
        current,
        tls::fingerprint(&stack.der),
        "the certificate being served"
    );
    assert_eq!(view["current"]["names"], json!(["localhost"]));

    let (status, error) = stack
        .json(
            "POST",
            "/api/v1/certificate/rotate",
            &[("authorization", &viewer), json],
            "",
        )
        .await;
    assert_eq!(
        (status, error["code"].as_str()),
        (StatusCode::FORBIDDEN, Some("forbidden"))
    );

    let (status, view) = stack
        .json(
            "POST",
            "/api/v1/certificate/rotate",
            &[("authorization", &admin), json],
            "",
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{view}");
    assert_eq!(view["can_rotate"], true);
    assert_eq!(view["current"]["fingerprint"], current.as_str());
    assert_eq!(view["pending"]["names"], json!(["localhost"]));
    let pending = view["pending"]["fingerprint"].as_str().unwrap().to_string();
    assert_ne!(pending, current);

    let switch = |fingerprint: &str| json!({"fingerprint": fingerprint}).to_string();
    let (status, error) = stack
        .json(
            "POST",
            "/api/v1/certificate/switch",
            &[("authorization", &admin), json],
            &switch(&current),
        )
        .await;
    assert_eq!(
        (status, error["code"].as_str()),
        (StatusCode::CONFLICT, Some("conflict")),
        "not the fingerprint shown"
    );
    assert_eq!(stack.switches.load(std::sync::atomic::Ordering::SeqCst), 0);

    let (status, view) = stack
        .json(
            "POST",
            "/api/v1/certificate/switch",
            &[("authorization", &admin), json],
            &switch(&pending),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{view}");
    assert_eq!(view["current"]["fingerprint"], pending.as_str());
    assert_eq!(view["pending"], Value::Null);
    assert_eq!(stack.switches.load(std::sync::atomic::Ordering::SeqCst), 1);
    assert!(stack.tls_dir.join("https.previous.pem").exists());

    let (status, _) = stack
        .json(
            "POST",
            "/api/v1/certificate/rotate",
            &[("authorization", &admin), json],
            "",
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let (status, view) = stack
        .json(
            "POST",
            "/api/v1/certificate/discard",
            &[("authorization", &admin), json],
            "{}",
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(view["pending"], Value::Null);
    assert_eq!(view["current"]["fingerprint"], pending.as_str());

    let audit = stack.auth.audit().recent(20).await;
    let messages: Vec<_> = audit
        .iter()
        .filter(|e| e.kind == "certificate_changed")
        .map(|e| e.message.clone())
        .collect();
    assert_eq!(messages.len(), 4, "{messages:?}");
    assert!(
        messages
            .iter()
            .any(|m| m == &format!("switched to the new certificate {pending}"))
    );
    assert!(messages.iter().any(|m| m == "new certificate discarded"));
}

#[tokio::test]
async fn anyone_signed_in_searches_logs_and_sees_replication() {
    let stack = Stack::new(Duration::from_secs(15)).await;
    let client = "127.0.0.1".parse().unwrap();
    let viewer = stack
        .auth
        .sessions()
        .create("vera@pve", Role::Viewer, client, now())
        .await
        .unwrap()
        .0;
    let viewer = format!("Bearer {viewer}");
    stack
        .auth
        .store()
        .append(vec![
            bmacd::store::Entry::journal(5_000, "corosync.service", 3, "link 0 down", false),
            bmacd::store::Entry::log(6_000, "info", "started"),
        ])
        .await
        .unwrap();
    let json = ("content-type", "application/json");
    let (status, page) = stack
        .json(
            "POST",
            "/api/v1/logs/search",
            &[("authorization", &viewer), json],
            &json!({"min_severity": "error"}).to_string(),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{page}");
    assert_eq!(
        page["records"][0]["summary"],
        "corosync.service: link 0 down"
    );
    assert_eq!(page["records"].as_array().unwrap().len(), 1);
    let (status, error) = stack
        .json(
            "POST",
            "/api/v1/logs/search",
            &[("authorization", &viewer), json],
            &json!({"before": "nonsense"}).to_string(),
        )
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{error}");
    let (status, view) = stack
        .json(
            "GET",
            "/api/v1/logs/replication",
            &[("authorization", &viewer)],
            "",
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{view}");
    assert_eq!(view["retention"]["keep_days"], 30);
    assert!(
        view["origins"]
            .as_array()
            .unwrap()
            .iter()
            .any(|o| o["records"].as_u64() >= Some(2))
    );
    assert_eq!(view["peers"], json!([]));
}

#[tokio::test]
async fn operators_handle_alerts_and_only_admins_set_up_email() {
    let stack = Stack::new(Duration::from_secs(15)).await;
    let client = "127.0.0.1".parse().unwrap();
    let create = |user: &'static str, role| {
        let auth = stack.auth.clone();
        async move {
            auth.sessions()
                .create(user, role, client, now())
                .await
                .unwrap()
                .0
        }
    };
    let viewer = format!("Bearer {}", create("vera@pve", Role::Viewer).await);
    let operator = format!("Bearer {}", create("bob@pve", Role::Operator).await);
    let admin = format!("Bearer {}", create("root@pam", Role::Admin).await);
    let json = ("content-type", "application/json");
    let forbidden = (StatusCode::FORBIDDEN, Some("forbidden"));

    let (status, view) = stack
        .json("GET", "/api/v1/alerts", &[("authorization", &viewer)], "")
        .await;
    assert_eq!(status, StatusCode::OK, "{view}");
    assert_eq!(view["alerts"], json!([]));
    assert_eq!(view["token_set"], false);
    assert_eq!(
        (view["can_acknowledge"].clone(), view["can_edit"].clone()),
        (json!(false), json!(false))
    );

    let silence = json!({
        "condition": "zfs.",
        "resource": null,
        "host": "mox2",
        "until_ms": now() + 3_600_000,
        "reason": "replacing a disk"
    })
    .to_string();
    let (status, error) = stack
        .json(
            "POST",
            "/api/v1/silences",
            &[("authorization", &viewer), json],
            &silence,
        )
        .await;
    assert_eq!((status, error["code"].as_str()), forbidden);
    let (status, saved) = stack
        .json(
            "POST",
            "/api/v1/silences",
            &[("authorization", &operator), json],
            &silence,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{saved}");
    assert_eq!(saved["created_by"], "bob@pve");
    let id = saved["id"].as_str().unwrap().to_string();

    let (status, error) = stack
        .json(
            "POST",
            "/api/v1/alerts/0a1b2c3d4e5f/acknowledge",
            &[("authorization", &operator), json],
            "",
        )
        .await;
    assert_eq!(
        (status, error["code"].as_str()),
        (StatusCode::NOT_FOUND, Some("not_found"))
    );

    let settings = json!({
        "settings": {
            "sender": "bmac@example.com",
            "recipients": ["ops@example.com"],
            "routes": [],
            "throttle_minutes": 60,
            "digest_hour": 7,
            "dashboard_url": "https://mox1.example.ts.net:8443/"
        },
        "expected_revision": null
    })
    .to_string();
    let (status, error) = stack
        .json(
            "PUT",
            "/api/v1/notify/settings",
            &[("authorization", &operator), json],
            &settings,
        )
        .await;
    assert_eq!((status, error["code"].as_str()), forbidden);
    let (status, view) = stack
        .json(
            "PUT",
            "/api/v1/notify/settings",
            &[("authorization", &admin), json],
            &settings,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{view}");
    assert_eq!(view["settings"]["recipients"], json!(["ops@example.com"]));
    assert_eq!(view["silences"][0]["id"], id.as_str());
    assert_eq!(view["can_edit"], true);

    let token = json!({"token": "8c1f7a52-0d4e-4b7b-9b60-2f3c1d9e6a10"}).to_string();
    let (status, error) = stack
        .json(
            "PUT",
            "/api/v1/notify/token",
            &[("authorization", &operator), json],
            &token,
        )
        .await;
    assert_eq!((status, error["code"].as_str()), forbidden);
    let (status, view) = stack
        .json(
            "PUT",
            "/api/v1/notify/token",
            &[("authorization", &admin), json],
            &token,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{view}");
    assert_eq!(view["token_set"], true);
    assert!(
        !view.to_string().contains("8c1f7a52"),
        "the token never comes back"
    );

    for (method, path, body) in [
        (
            "POST",
            "/api/v1/notify/test",
            json!({"to": null}).to_string(),
        ),
        ("GET", "/api/v1/notify/templates", String::new()),
        ("POST", "/api/v1/notify/templates", String::new()),
    ] {
        let (status, error) = stack
            .json(method, path, &[("authorization", &operator), json], &body)
            .await;
        assert_eq!(
            (status, error["code"].as_str()),
            forbidden,
            "{method} {path}"
        );
    }

    let (status, removed) = stack
        .json(
            "POST",
            &format!("/api/v1/silences/{id}/delete"),
            &[("authorization", &operator), json],
            "{}",
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{removed}");

    let audit = stack.auth.audit().recent(20).await;
    let kinds: Vec<_> = audit.iter().map(|e| e.kind.as_str()).collect();
    assert_eq!(
        kinds.iter().filter(|k| **k == "silence_changed").count(),
        2,
        "{kinds:?}"
    );
    assert_eq!(
        kinds
            .iter()
            .filter(|k| **k == "notifications_changed")
            .count(),
        2,
        "{kinds:?}"
    );
    assert!(audit.iter().all(|e| !e.message.contains("8c1f7a52")));
}
