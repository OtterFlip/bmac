// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

//! The UI over real HTTPS on a loopback port: the certificate it presents,
//! login and sessions, the request checks, and each page's changes.

use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use bmac_notify::postmark::Pending;
use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::header::HeaderMap;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::pki_types::{ServerName, UnixTime};
use rustls::{DigitallySignedStruct, SignatureScheme};
use serde_json::{Value, json};
use sha2::Digest;
use uuid::Uuid;

use super::*;
use crate::fake_pool::FakePool;
use crate::journal::{Journal, StoredPit, StoredVolume};

const PASSWORD: &str = "correct horse battery";
const CLUSTER: &str = "c1";
const AT: i64 = 1_800_000_000_000;

fn id(n: u128) -> Uuid {
    Uuid::from_u128(n)
}

fn pit(n: u128) -> StoredPit {
    StoredPit {
        cluster_uuid: CLUSTER.into(),
        vm_uuid: id(7),
        branch_uuid: id(0xb),
        pit_uuid: id(n),
        parent: (n > 1).then(|| id(n - 1)),
        name: format!("bmac-pit-{n}"),
        taken_s: 1_700_000_000 + n as i64 * 86_400,
        published_at_ms: AT,
        volumes: vec![StoredVolume {
            volume_uuid: id(0xd),
            dataset: "bk/bmac/clusters/c1/vms/v/b/d".into(),
            guid: bmac_core::SnapshotGuid::new(100 + n as u64),
            volsize: 1 << 20,
            volblocksize: 16384,
        }],
        bytes: 1000,
        manifest_sha256: String::new(),
        received_from: None,
    }
}

/// The certificate the server presented, accepted whatever it is.
#[derive(Debug)]
struct Seen {
    der: Mutex<Option<Vec<u8>>>,
    provider: Arc<rustls::crypto::CryptoProvider>,
}

impl ServerCertVerifier for Seen {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _: &[CertificateDer<'_>],
        _: &ServerName<'_>,
        _: &[u8],
        _: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        *self.der.lock().unwrap() = Some(end_entity.to_vec());
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider
            .signature_verification_algorithms
            .supported_schemes()
    }
}

/// Postmark, answering every send with success.
#[derive(Default)]
struct Postmark {
    sent: Mutex<Vec<Value>>,
}

impl Transport for Postmark {
    fn call<'a>(&'a self, _: &'a str, _: &'a str, body: Option<Value>) -> Pending<'a> {
        Box::pin(async move {
            self.sent.lock().unwrap().push(body.unwrap_or(Value::Null));
            Ok((200, json!({"ErrorCode": 0, "MessageID": "m1"})))
        })
    }
}

struct Rig {
    _dir: tempfile::TempDir,
    ui: Arc<Ui>,
    address: SocketAddr,
    seen: Arc<Seen>,
    locked: Arc<AtomicBool>,
    probe_answer: Arc<Mutex<Option<Probe>>>,
    postmark: Arc<Postmark>,
    _server: tokio::task::JoinHandle<()>,
}

struct Reply {
    status: u16,
    headers: HeaderMap,
    body: Value,
    text: String,
}

async fn rig() -> Rig {
    let dir = tempfile::tempdir().unwrap();
    let paths = Paths::at(dir.path());
    std::fs::create_dir_all(paths.metadata()).unwrap();
    let identity = Identity {
        schema_version: 1,
        appliance_uuid: id(0xa),
        name: "vault1".into(),
        created_at_ms: AT,
    };
    let (cert, key) = crate::state::certificate("vault1", &identity.appliance_uuid).unwrap();
    let metadata = Metadata::new(paths.metadata());
    let mut journal = Journal::new(identity.appliance_uuid, "123");
    journal.pits = (1..=4).map(pit).collect();
    metadata.save(&journal).unwrap();
    crate::admin::set_password(metadata.root(), PASSWORD, AT, 1000).unwrap();
    let audit = Arc::new(Audit::new(&paths.metadata()));
    audit
        .record(
            AT,
            "publish",
            "bmac-backupd",
            json!({"cluster_uuid": CLUSTER, "vm_uuid": id(7), "pit_uuid": id(4), "bytes": 5000, "duration_ms": 2000}),
        )
        .unwrap();
    let locked = Arc::new(AtomicBool::new(false));
    let unlocked: Unlocked = {
        let locked = locked.clone();
        Arc::new(move || {
            let open = !locked.load(Ordering::SeqCst);
            Box::pin(async move { open })
        })
    };
    let probe_answer: Arc<Mutex<Option<Probe>>> = Arc::default();
    let probe: Prober = {
        let answer = probe_answer.clone();
        Arc::new(move |_, _| {
            let answer = answer.lock().unwrap().take().unwrap_or(Probe::Matches);
            Box::pin(async move { answer })
        })
    };
    let postmark = Arc::new(Postmark::default());
    let mailer: Mailer = {
        let postmark = postmark.clone();
        Arc::new(move |_| Ok(postmark.clone() as Arc<dyn Transport>))
    };
    let clusters: Clusters = Arc::new(|| {
        vec![ClusterSnapshot {
            status: ClusterStatus {
                cluster_uuid: CLUSTER.into(),
                cluster_name: "prod".into(),
                state: Some("ACTIVE".into()),
                last_checkin_ms: Some(AT),
                ..Default::default()
            },
            hosts: vec!["mox1".into()],
            transfers: vec![TransferReport {
                transfer_uuid: id(0x77),
                vm_uuid: id(7),
                pit_uuid: id(5),
                bytes: 4096,
            }],
        }]
    });
    let ui = Arc::new(Ui {
        paths,
        identity,
        fingerprint: crate::state::pem_fingerprint(&cert).unwrap(),
        metadata,
        audit,
        pool: Arc::new(FakePool::new()),
        binding: PoolBinding {
            schema_version: 1,
            pool: "bk".into(),
            pool_guid: "123".into(),
        },
        unlocked,
        clusters,
        probe,
        mailer,
        now_ms: Arc::new(|| AT + 1000),
        rounds: 1000,
        scrub_now: Arc::default(),
        sessions: Sessions::default(),
        assets: Assets::from_files([
            (
                "/index.html",
                &b"<meta name=\"bmac-style-nonce\" content=\"__BMAC_STYLE_NONCE__\">"[..],
            ),
            ("/assets/app-1.js", &b"js"[..]),
        ]),
    });
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(serve(
        listener,
        tls_config(&cert, &key).unwrap(),
        ui.clone(),
    ));
    Rig {
        _dir: dir,
        ui,
        address,
        seen: Arc::new(Seen {
            der: Mutex::default(),
            provider: crate::client::provider(),
        }),
        locked,
        probe_answer,
        postmark,
        _server: server,
    }
}

impl Rig {
    async fn send(
        &self,
        method: &str,
        path: &str,
        token: Option<&str>,
        body: Option<Value>,
        headers: &[(&str, &str)],
    ) -> Reply {
        let config = rustls::ClientConfig::builder_with_provider(crate::client::provider())
            .with_safe_default_protocol_versions()
            .unwrap()
            .dangerous()
            .with_custom_certificate_verifier(self.seen.clone())
            .with_no_client_auth();
        let tcp = TcpStream::connect(self.address).await.unwrap();
        let tls = tokio_rustls::TlsConnector::from(Arc::new(config))
            .connect(ServerName::try_from("vault1").unwrap(), tcp)
            .await
            .unwrap();
        let (mut sender, connection) = hyper::client::conn::http1::handshake(TokioIo::new(tls))
            .await
            .unwrap();
        tokio::spawn(connection);
        let mut request = hyper::Request::builder()
            .method(method)
            .uri(path)
            .header("host", "vault1:8443");
        if let Some(token) = token {
            request = request.header("authorization", format!("Bearer {token}"));
        }
        if body.is_some() && !headers.iter().any(|(k, _)| *k == "content-type") {
            request = request.header("content-type", "application/json");
        }
        for (k, v) in headers {
            request = request.header(*k, *v);
        }
        let bytes = body.map(|b| b.to_string()).unwrap_or_default();
        let response = sender
            .send_request(request.body(Full::new(Bytes::from(bytes))).unwrap())
            .await
            .unwrap();
        let status = response.status().as_u16();
        let headers = response.headers().clone();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let text = String::from_utf8_lossy(&bytes).into_owned();
        Reply {
            status,
            headers,
            body: serde_json::from_str(&text).unwrap_or(Value::Null),
            text,
        }
    }

    async fn get(&self, path: &str, token: &str) -> Reply {
        self.send("GET", &format!("/api/v1{path}"), Some(token), None, &[])
            .await
    }

    async fn post(&self, path: &str, token: &str, body: Value) -> Reply {
        self.send(
            "POST",
            &format!("/api/v1{path}"),
            Some(token),
            Some(body),
            &[],
        )
        .await
    }

    async fn login(&self) -> String {
        let reply = self
            .send(
                "POST",
                "/api/v1/login",
                None,
                Some(json!({"password": PASSWORD})),
                &[],
            )
            .await;
        assert_eq!(reply.status, 200, "{}", reply.text);
        reply.body["token"].as_str().unwrap().to_string()
    }

    fn events(&self) -> Vec<String> {
        self.ui
            .audit
            .recent(100)
            .unwrap()
            .into_iter()
            .map(|e| e.event)
            .collect()
    }
}

#[tokio::test]
async fn it_serves_the_appliance_certificate_and_the_dashboard_headers() {
    let rig = rig().await;
    let page = rig.send("GET", "/peers", None, None, &[]).await;
    assert_eq!(page.status, 200);
    let der = rig.seen.der.lock().unwrap().clone().unwrap();
    let fingerprint: String = sha2::Sha256::digest(&der)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    assert_eq!(fingerprint, rig.ui.fingerprint);

    let policy = page.headers["content-security-policy"].to_str().unwrap();
    let nonce = policy
        .split("'nonce-")
        .nth(1)
        .and_then(|r| r.split('\'').next())
        .unwrap();
    assert!(page.text.contains(nonce));
    assert!(!page.text.contains(STYLE_NONCE_PLACEHOLDER));
    assert!(policy.contains("script-src 'self';"));
    assert_eq!(page.headers["x-frame-options"], "DENY");
    assert!(page.headers.get("set-cookie").is_none());
    let again = rig.send("GET", "/", None, None, &[]).await;
    assert!(!again.text.contains(nonce));
    let asset = rig.send("GET", "/assets/app-1.js", None, None, &[]).await;
    assert_eq!(asset.text, "js");
    assert_eq!(
        rig.send("GET", "/assets/none.js", None, None, &[])
            .await
            .status,
        404
    );

    let hello = rig.send("GET", "/api/v1/hello", None, None, &[]).await;
    assert_eq!(hello.status, 200);
    assert_eq!(hello.headers["cache-control"], "no-store");
    assert_eq!(hello.body["name"], "vault1");
    assert_eq!(hello.body["locked"], false);
    assert_eq!(hello.body["no_password"], false);
    assert_eq!(
        hello.body["fingerprint"],
        bmac_proto::backups::display_fingerprint(&rig.ui.fingerprint)
    );
}

#[tokio::test]
async fn login_starts_a_memory_session_and_everything_else_needs_it() {
    let rig = rig().await;
    let wrong = rig
        .send(
            "POST",
            "/api/v1/login",
            None,
            Some(json!({"password": "not the password"})),
            &[],
        )
        .await;
    assert_eq!(wrong.status, 401);
    assert_eq!(rig.events()[0], "login_failed");
    assert!(!wrong.text.contains(PASSWORD));

    assert_eq!(
        rig.send("GET", "/api/v1/status", None, None, &[])
            .await
            .status,
        401
    );
    let token = rig.login().await;
    assert_eq!(rig.events()[0], "login");
    let status = rig.get("/status", &token).await;
    assert_eq!(status.status, 200, "{}", status.text);
    assert_eq!(status.body["pits"], 4);
    assert_eq!(status.body["pool"]["health"], "ONLINE");
    assert_eq!(status.body["clusters"][0]["cluster_name"], "prod");
    assert_eq!(status.body["transfers"][0]["bytes"], 4096);
    assert!(status.headers.get("set-cookie").is_none());

    let foreign = rig
        .send(
            "GET",
            "/api/v1/status",
            Some(&token),
            None,
            &[("origin", "https://evil.example")],
        )
        .await;
    assert_eq!(foreign.status, 403);
    let same = rig
        .send(
            "GET",
            "/api/v1/status",
            Some(&token),
            None,
            &[("origin", "https://vault1:8443")],
        )
        .await;
    assert_eq!(same.status, 200);
    let form = rig
        .send(
            "POST",
            "/api/v1/prune",
            Some(&token),
            Some(json!({})),
            &[("content-type", "text/plain")],
        )
        .await;
    assert_eq!(form.status, 422);

    rig.locked.store(true, Ordering::SeqCst);
    assert_eq!(rig.get("/status", &token).await.status, 503);
    let hello = rig.send("GET", "/api/v1/hello", None, None, &[]).await;
    assert_eq!(hello.body["locked"], true);
    rig.locked.store(false, Ordering::SeqCst);

    rig.post("/logout", &token, json!({})).await;
    assert_eq!(rig.get("/status", &token).await.status, 401);
}

#[tokio::test]
async fn logins_are_rate_limited_without_checking_the_password() {
    let rig = rig().await;
    for _ in 0..10 {
        rig.send(
            "POST",
            "/api/v1/login",
            None,
            Some(json!({"password": "nope nope nope"})),
            &[],
        )
        .await;
    }
    let limited = rig
        .send(
            "POST",
            "/api/v1/login",
            None,
            Some(json!({"password": PASSWORD})),
            &[],
        )
        .await;
    assert_eq!(limited.status, 429);
    assert!(limited.headers.contains_key("retry-after"));
    assert_eq!(rig.events()[0], "login_rate_limited");
}

#[tokio::test]
async fn pits_throughput_and_retention_show_the_journal() {
    let rig = rig().await;
    let token = rig.login().await;
    let pits = rig.get("/pits", &token).await;
    assert_eq!(pits.body["pits"].as_array().unwrap().len(), 4);
    assert_eq!(pits.body["pits"][1]["parent"], id(1).to_string());

    let throughput = rig.get("/throughput", &token).await;
    assert_eq!(throughput.body["recent"][0]["bytes"], 5000);
    assert_eq!(throughput.body["recent"][0]["duration_ms"], 2000);
    assert_eq!(throughput.body["live"][0]["pit_uuid"], id(5).to_string());

    let retention = rig.get("/retention", &token).await;
    assert_eq!(retention.status, 200, "{}", retention.text);
    let policy = retention.body["policy"].clone();

    let weaker = rig
        .post(
            "/retention",
            &token,
            json!({"policy": {"daily": 2, "weekly": 1, "monthly": 1, "month_days": 30, "yearly": true}}),
        )
        .await;
    assert_eq!(weaker.status, 200, "{}", weaker.text);
    assert_eq!(weaker.body["applied"], false);
    let retention = rig.get("/retention", &token).await;
    assert_eq!(retention.body["policy"], policy);
    assert_eq!(retention.body["pending"]["policy"]["daily"], 2);
    assert_eq!(rig.events()[0], "policy");
    assert_eq!(
        rig.post("/retention/cancel", &token, json!({}))
            .await
            .status,
        200
    );
    assert!(rig.get("/retention", &token).await.body["pending"].is_null());
    let below = rig
        .post(
            "/retention",
            &token,
            json!({"policy": {"daily": 0, "weekly": 0, "monthly": 0, "month_days": 30, "yearly": true}}),
        )
        .await;
    assert_eq!(below.status, 422);

    let hold = rig
        .post(
            "/holds",
            &token,
            json!({"pit_uuid": id(1).to_string(), "hold": true}),
        )
        .await;
    assert_eq!(hold.status, 200, "{}", hold.text);
    let pits = rig.get("/pits", &token).await;
    assert_eq!(pits.body["pits"][0]["held"], true);
    assert_eq!(
        rig.post(
            "/holds",
            &token,
            json!({"pit_uuid": id(99).to_string(), "hold": true})
        )
        .await
        .status,
        404
    );
}

#[tokio::test]
async fn a_grant_token_is_shown_once_and_can_be_revoked() {
    let rig = rig().await;
    let token = rig.login().await;
    let issued = rig
        .post(
            "/grants",
            &token,
            json!({"cluster_uuid": CLUSTER, "hours": 4, "pits": [id(2).to_string()]}),
        )
        .await;
    assert_eq!(issued.status, 200, "{}", issued.text);
    let secret = issued.body["token"].as_str().unwrap().to_string();
    let grant_id = issued.body["grant"]["grant_id"]
        .as_str()
        .unwrap()
        .to_string();
    let list = rig.get("/grants", &token).await;
    assert_eq!(list.body[0]["state"], "live");
    assert!(!list.text.contains(&secret));
    assert!(
        !rig.ui
            .audit
            .recent(10)
            .unwrap()
            .iter()
            .any(|e| e.details.to_string().contains(&secret))
    );
    assert_eq!(
        rig.get("/pits", &token).await.body["pits"][1]["granted"],
        true
    );
    assert_eq!(
        rig.post("/grants/revoke", &token, json!({"grant_id": grant_id}))
            .await
            .status,
        200
    );
    assert_eq!(rig.get("/grants", &token).await.body[0]["state"], "revoked");
    let bad = rig
        .post(
            "/grants",
            &token,
            json!({"cluster_uuid": CLUSTER, "hours": 400, "pits": [id(2).to_string()]}),
        )
        .await;
    assert_eq!(bad.status, 422);
}

#[tokio::test]
async fn trusting_a_peer_needs_its_certificate_and_untrusting_the_password() {
    let rig = rig().await;
    let token = rig.login().await;
    let typed = "ab".repeat(32);
    let peer = id(0xbeef).to_string();
    *rig.probe_answer.lock().unwrap() = Some(Probe::Mismatch {
        presented: "cd".repeat(32),
    });
    let refused = rig
        .post(
            "/peers/trust",
            &token,
            json!({"appliance_uuid": peer, "fingerprint": typed, "address": "10.40.0.21"}),
        )
        .await;
    assert_eq!(refused.status, 200, "{}", refused.text);
    assert_eq!(refused.body["trusted"], false);
    assert!(
        refused.body["message"]
            .as_str()
            .unwrap()
            .starts_with("WARNING")
    );
    assert_eq!(rig.events()[0], "trust_mismatch");
    let peers = rig.get("/peers", &token).await;
    assert!(peers.body["peers"].as_array().unwrap().is_empty());
    assert!(
        peers.body["reminder"]
            .as_str()
            .unwrap()
            .contains("peer's own UI")
    );

    let trusted = rig
        .post(
            "/peers/trust",
            &token,
            json!({"appliance_uuid": peer, "fingerprint": typed, "address": "vault2.example.net"}),
        )
        .await;
    assert_eq!(trusted.body["trusted"], true, "{}", trusted.text);
    let peers = rig.get("/peers", &token).await;
    assert_eq!(peers.body["peers"][0]["address"], "vault2.example.net");

    let wrong = rig
        .post(
            "/peers/untrust",
            &token,
            json!({"appliance_uuid": peer, "password": "not the password"}),
        )
        .await;
    assert_eq!(wrong.status, 403);
    assert_eq!(
        rig.get("/peers", &token).await.body["peers"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    let gone = rig
        .post(
            "/peers/untrust",
            &token,
            json!({"appliance_uuid": peer, "password": PASSWORD}),
        )
        .await;
    assert_eq!(gone.status, 200, "{}", gone.text);
    assert!(
        rig.get("/peers", &token).await.body["peers"]
            .as_array()
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn settings_are_checked_and_the_postmark_token_is_never_shown() {
    let rig = rig().await;
    let token = rig.login().await;
    let settings = rig.get("/settings", &token).await;
    assert_eq!(settings.body["io_policy"], "pulls_first");
    assert_eq!(settings.body["token_set"], false);

    let bad = rig
        .post(
            "/settings",
            &token,
            json!({"scrub_every_days": 30, "io_policy": "pulls_first", "down_after_minutes": 5,
                   "urls": ["ftp://x"], "from": "", "to": []}),
        )
        .await;
    assert_eq!(bad.status, 422);
    let saved = rig
        .post(
            "/settings",
            &token,
            json!({"scrub_every_days": 0, "io_policy": "together", "down_after_minutes": 10,
                   "urls": ["https://shop.example.com/"], "from": "bmac@example.com", "to": ["ops@example.com"]}),
        )
        .await;
    assert_eq!(saved.status, 200, "{}", saved.text);
    assert_eq!(saved.body["io_policy"], "together");

    assert_eq!(
        rig.post("/settings/test-email", &token, json!({}))
            .await
            .status,
        409
    );
    let secret = "pm-server-token-123";
    assert_eq!(
        rig.post("/settings/token", &token, json!({"token": secret}))
            .await
            .status,
        200
    );
    let settings = rig.get("/settings", &token).await;
    assert_eq!(settings.body["token_set"], true);
    assert!(!settings.text.contains(secret));
    let test = rig.post("/settings/test-email", &token, json!({})).await;
    assert_eq!(test.body["sent"], true, "{}", test.text);
    let sent = rig.postmark.sent.lock().unwrap().clone();
    assert_eq!(sent[0]["TemplateAlias"], "bmac-test-v1");
    assert_eq!(sent[0]["To"], "ops@example.com");
    let audit = rig.get("/audit?limit=50", &token).await;
    assert!(!audit.text.contains(secret));
    assert_eq!(audit.body["entries"][0]["event"], "test_email");
}

#[tokio::test]
async fn a_scrub_can_be_asked_for_even_with_scheduled_scrubs_off() {
    let rig = rig().await;
    let token = rig.login().await;
    let woken = rig.ui.scrub_now.clone();
    let wait = tokio::spawn(async move { woken.notified().await });
    tokio::task::yield_now().await;
    assert_eq!(
        rig.post("/scrub/start", &token, json!({})).await.status,
        200
    );
    tokio::time::timeout(std::time::Duration::from_secs(5), wait)
        .await
        .unwrap()
        .unwrap();
    assert!(
        crate::scrub::ScrubState::load(&rig.ui.metadata)
            .unwrap()
            .requested
    );
    let scrub = rig.get("/scrub", &token).await;
    assert_eq!(scrub.body["state"], "requested");
}

#[tokio::test]
async fn a_new_password_ends_the_other_sessions() {
    let rig = rig().await;
    let mine = rig.login().await;
    let other = rig.login().await;
    let short = rig
        .post(
            "/password",
            &mine,
            json!({"current": PASSWORD, "new": "short"}),
        )
        .await;
    assert_eq!(short.status, 422);
    let changed = rig
        .post(
            "/password",
            &mine,
            json!({"current": PASSWORD, "new": "a much longer passphrase"}),
        )
        .await;
    assert_eq!(changed.status, 200, "{}", changed.text);
    assert_eq!(rig.get("/status", &mine).await.status, 200);
    assert_eq!(rig.get("/status", &other).await.status, 401);
    assert!(crate::admin::verify(rig.ui.metadata.root(), "a much longer passphrase").unwrap());
}

#[tokio::test]
async fn the_recovery_page_shows_manifests_headers_and_reported_restore_tests() {
    let rig = rig().await;
    let metadata = &rig.ui.metadata;
    let files =
        std::collections::BTreeMap::from([("config/cluster.json".to_string(), "{}".to_string())]);
    let manifest = bmac_proto::appliance::RecoveryManifest {
        schema_version: 1,
        cluster_uuid: CLUSTER.into(),
        generated_at_ms: AT,
        host: "mox1".parse().unwrap(),
        revision: bmac_proto::appliance::RecoveryManifest::revision_of(&files),
        files,
    };
    crate::recovery::keep_manifest(metadata, CLUSTER, &manifest, AT).unwrap();
    crate::recovery::keep_tests(
        metadata,
        CLUSTER,
        &[bmac_proto::appliance::RestoreTestReport {
            restore_uuid: id(0x90),
            vm_uuid: id(7),
            pit_uuid: id(5),
            host: "mox2".into(),
            started_at_ms: AT,
            finished_at_ms: Some(AT + 60_000),
            booted: true,
            agent: false,
            agent_after_s: None,
            error: None,
        }],
    )
    .unwrap();
    let token = rig.login().await;
    let page = rig.get("/recovery", &token).await;
    assert_eq!(page.status, 200, "{}", page.text);
    let cluster = &page.body["clusters"][0];
    assert_eq!(cluster["cluster_name"], "prod");
    assert_eq!(cluster["manifests"][0]["generated_at_ms"], AT);
    assert_eq!(
        cluster["manifests"][0]["revision"],
        manifest.revision[..16].to_string()
    );
    assert_eq!(cluster["headers"], json!([]));
    assert_eq!(cluster["restore_tests"][0]["booted"], true);
    assert_eq!(cluster["restore_tests"][0]["agent"], false);
    assert!(
        !page.text.contains("config/cluster.json"),
        "only what was kept, not its files"
    );
}
