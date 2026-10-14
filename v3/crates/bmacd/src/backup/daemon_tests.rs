// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use bmac_core::ApplianceAddress;
use bmac_core::SnapshotGuid;
use bmac_proto::api::{ApiError, ErrorCode};
use bmac_proto::appliance::{SOURCE_ALPN, SendRequest, on_source};
use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper_util::rt::{TokioExecutor, TokioIo};
use rcgen::{BasicConstraints, CertificateParams, IsCa, KeyPair};
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use serde_json::{Value, json};
use tokio::net::TcpListener;
use uuid::Uuid;

use super::*;
use crate::backup::source_socket::{SourceHandler, serve};
use crate::peers::AllowedPeers;

/// A cluster CA, a node certificate it signed, and two self-signed
/// appliance certificates, as PEM.
struct Certs {
    ca: String,
    node: String,
    node_key: String,
    appliance: (String, String),
    stranger: (String, String),
}

fn certs() -> Certs {
    let ca_key = KeyPair::generate().unwrap();
    let mut params = CertificateParams::new(Vec::new()).unwrap();
    params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    let ca = params.self_signed(&ca_key).unwrap();
    let node_key = KeyPair::generate().unwrap();
    let node = CertificateParams::new(vec!["localhost".to_string()])
        .unwrap()
        .signed_by(&node_key, &ca, &ca_key)
        .unwrap();
    let own = |name: &str| {
        let key = KeyPair::generate().unwrap();
        let cert = CertificateParams::new(vec![name.to_string()])
            .unwrap()
            .self_signed(&key)
            .unwrap();
        (cert.pem(), key.serialize_pem())
    };
    Certs {
        ca: ca.pem(),
        node: node.pem(),
        node_key: node_key.serialize_pem(),
        appliance: own("appliance"),
        stranger: own("stranger"),
    }
}

fn pem_fingerprint(pem: &str) -> String {
    fingerprint(&CertificateDer::from_pem_slice(pem.as_bytes()).unwrap())
}

/// The engine's side, answering what a send needs and recording reports.
struct FakeEngine {
    allowed: String,
    plan: Value,
    refuse_sends: std::sync::atomic::AtomicBool,
    calls: Mutex<Vec<SourceCall>>,
}

impl SourceHandler for FakeEngine {
    fn handle(&self, call: SourceCall) -> bmac_exec::BoxFuture<'_, Result<Value, ApiError>> {
        Box::pin(async move {
            self.calls.lock().unwrap().push(call.clone());
            match call {
                SourceCall::Appliances => Ok(json!([{
                    "name": "vault", "fingerprint": self.allowed, "address": "192.0.2.20",
                }])),
                SourceCall::AuthorizeSend { fingerprint, .. }
                    if fingerprint == self.allowed
                        && !self.refuse_sends.load(std::sync::atomic::Ordering::SeqCst) =>
                {
                    Ok(self.plan.clone())
                }
                SourceCall::TransferDone { .. } => Ok(json!({})),
                SourceCall::Catalog { fingerprint } => Ok(json!({ "who": fingerprint })),
                _ => Err(ApiError::new(ErrorCode::Forbidden, "not for you")),
            }
        })
    }
}

/// A fake `zfs` logging its arguments and streaming `STREAM` on a send.
/// Running it once first waits out a thread that forked while the file was
/// open for writing, which would make exec fail with "text file busy".
fn fake_zfs(dir: &Path) -> (PathBuf, PathBuf) {
    let log = dir.join("zfs.log");
    let program = dir.join("zfs");
    std::fs::write(
        &program,
        format!(
            "#!/bin/sh\nset -eu\n[ \"$1\" = probe ] && exit 0\necho \"$*\" >> '{}'\n\
             [ \"$1\" = send ] && printf '%s' '{STREAM}'\nexit 0\n",
            log.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(
        &program,
        std::os::unix::fs::PermissionsExt::from_mode(0o755),
    )
    .unwrap();
    for _ in 0..100 {
        match std::process::Command::new(&program).arg("probe").status() {
            Ok(_) => break,
            Err(e) if e.raw_os_error() == Some(26) => {
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            Err(e) => panic!("{e}"),
        }
    }
    (program, log)
}

const STREAM: &str = "a zfs send stream";

struct Rig {
    _dir: tempfile::TempDir,
    certs: Certs,
    engine: Arc<FakeEngine>,
    app: Arc<App>,
    log: PathBuf,
    tasks: Vec<tokio::task::JoinHandle<()>>,
}

impl Drop for Rig {
    fn drop(&mut self) {
        for task in &self.tasks {
            task.abort();
        }
    }
}

async fn rig() -> Rig {
    let dir = tempfile::tempdir().unwrap();
    let certs = certs();
    let (zfs, log) = fake_zfs(dir.path());
    let plan = SendPlan {
        transfer_uuid: Uuid::from_u128(7),
        snapshot: "rpool/data/vm-100-disk-1@bmac-pit-p2".to_string(),
        guid: SnapshotGuid::new(0x2222),
        from: Some("rpool/data/vm-100-disk-1#bmac-pit-p1".to_string()),
        resume_token: None,
        large_blocks: true,
        compressed: true,
    };
    let engine = Arc::new(FakeEngine {
        allowed: pem_fingerprint(&certs.appliance.0),
        plan: serde_json::to_value(&plan).unwrap(),
        refuse_sends: Default::default(),
        calls: Mutex::new(Vec::new()),
    });
    let socket = dir.path().join("engine.sock");
    let listener = crate::engine::server::bind(&socket, None).unwrap();
    let uid = rustix::process::geteuid().as_raw();
    let tasks = vec![tokio::spawn(serve(
        listener,
        AllowedPeers::new([uid]),
        engine.clone(),
        std::future::pending(),
    ))];
    let client = SourceClient::new(&socket);
    assert_eq!(appliances(&client).await.unwrap().len(), 1);
    let app = Arc::new(App { client, zfs });
    Rig {
        _dir: dir,
        certs,
        engine,
        app,
        log,
        tasks,
    }
}

type Sender = hyper::client::conn::http2::SendRequest<Full<Bytes>>;

impl Rig {
    fn node(&self) -> Credentials {
        Credentials {
            cert: self.certs.node.clone().into_bytes(),
            key: self.certs.node_key.clone().into_bytes(),
            ca: self.certs.ca.clone().into_bytes(),
        }
    }

    /// Plays an appliance presenting `who`'s certificate: the host, pinning
    /// `pinned`, dials it, and the appliance gets an HTTP/2 client over the
    /// connection, or the host's reason for giving up.
    async fn appliance(&mut self, who: &(String, String), pinned: &str) -> Result<Sender, String> {
        let mut roots = rustls::RootCertStore::empty();
        roots
            .add(CertificateDer::from_pem_slice(self.certs.ca.as_bytes()).unwrap())
            .unwrap();
        let provider = crate::mesh::tls::provider();
        let verifier = rustls::server::WebPkiClientVerifier::builder_with_provider(
            roots.into(),
            provider.clone(),
        )
        .build()
        .unwrap();
        let mut server = rustls::ServerConfig::builder_with_provider(provider)
            .with_protocol_versions(&[&rustls::version::TLS13])
            .unwrap()
            .with_client_cert_verifier(verifier)
            .with_single_cert(
                vec![CertificateDer::from_pem_slice(who.0.as_bytes()).unwrap()],
                PrivateKeyDer::from_pem_slice(who.1.as_bytes()).unwrap(),
            )
            .unwrap();
        server.alpn_protocols = vec![SOURCE_ALPN.to_vec()];
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address: ApplianceAddress = listener.local_addr().unwrap().to_string().parse().unwrap();
        let node = self.node();
        let tls = client_config(&node.cert, &node.key, &node.ca, pinned).unwrap();
        let (done, outcome) = tokio::sync::oneshot::channel();
        let app = self.app.clone();
        self.tasks.push(tokio::spawn(async move {
            let _ = done.send(connect(app, tls, "vault", &address).await);
        }));
        let (tcp, _) = listener.accept().await.unwrap();
        let accepted = tokio_rustls::TlsAcceptor::from(Arc::new(server))
            .accept(tcp)
            .await;
        let Ok(stream) = accepted else {
            return Err(outcome.await.unwrap().unwrap_err());
        };
        let (_, session) = stream.get_ref();
        assert_eq!(session.alpn_protocol(), Some(SOURCE_ALPN));
        let (sender, connection) =
            hyper::client::conn::http2::handshake(TokioExecutor::new(), TokioIo::new(stream))
                .await
                .map_err(|e| e.to_string())?;
        self.tasks.push(tokio::spawn(async move {
            let _ = connection.await;
        }));
        Ok(sender)
    }

    fn zfs_calls(&self) -> Vec<String> {
        std::fs::read_to_string(&self.log)
            .unwrap_or_default()
            .lines()
            .map(str::to_string)
            .collect()
    }
}

async fn request(
    sender: &mut Sender,
    method: &str,
    path: &str,
    body: Option<Value>,
) -> (u16, Bytes) {
    let body = body.map(|b| b.to_string()).unwrap_or_default();
    let request = hyper::Request::builder()
        .method(method)
        .uri(format!("https://mox1{path}"))
        .header("content-type", "application/json")
        .body(Full::new(Bytes::from(body)))
        .unwrap();
    sender.ready().await.unwrap();
    let response = sender.send_request(request).await.unwrap();
    let status = response.status().as_u16();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (status, bytes)
}

fn send_request() -> Value {
    serde_json::to_value(SendRequest {
        transfer_uuid: Uuid::from_u128(7),
        vm_uuid: Uuid::from_u128(1),
        pit_uuid: Uuid::from_u128(2),
        volume_uuid: Uuid::from_u128(3),
        base: Some(Uuid::from_u128(4)),
        resume_token: None,
        large_blocks: true,
        compressed: true,
    })
    .unwrap()
}

#[tokio::test]
async fn the_host_dials_the_appliance_which_streams_a_held_send_and_the_engine_hears_how_it_went() {
    let mut rig = rig().await;
    let (who, pinned) = (
        rig.certs.appliance.clone(),
        pem_fingerprint(&rig.certs.appliance.0),
    );
    let mut appliance = rig.appliance(&who, &pinned).await.unwrap();
    let (status, body) = request(
        &mut appliance,
        "POST",
        on_source::SEND,
        Some(send_request()),
    )
    .await;
    assert_eq!(status, 200);
    assert_eq!(body, STREAM.as_bytes());

    let reported = || {
        rig.engine
            .calls
            .lock()
            .unwrap()
            .iter()
            .find_map(|c| match c {
                SourceCall::TransferDone {
                    fingerprint,
                    report,
                } => Some((fingerprint.clone(), report.clone())),
                _ => None,
            })
    };
    for _ in 0..200 {
        if reported().is_some() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    let (fingerprint, report) = reported().expect("the transfer is reported");
    assert_eq!(fingerprint, pem_fingerprint(&rig.certs.appliance.0));
    assert_eq!(report.bytes, STREAM.len() as u64);
    assert_eq!(report.error, None);
    assert_eq!(report.volume_uuid, Uuid::from_u128(3));
    let snapshot = "rpool/data/vm-100-disk-1@bmac-pit-p2";
    let tag = format!("bmac-send-{}", Uuid::from_u128(7));
    assert_eq!(
        rig.zfs_calls(),
        [
            format!("hold {tag} {snapshot}"),
            format!("send -L -c -i rpool/data/vm-100-disk-1#bmac-pit-p1 {snapshot}"),
            format!("release {tag} {snapshot}"),
        ]
    );

    let (status, body) = request(&mut appliance, "GET", on_source::CATALOG, None).await;
    assert_eq!(status, 200);
    let body: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(body["who"], pem_fingerprint(&rig.certs.appliance.0));
}

#[tokio::test]
async fn a_send_the_engine_refuses_runs_no_zfs() {
    let mut rig = rig().await;
    rig.engine
        .refuse_sends
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let (who, pinned) = (
        rig.certs.appliance.clone(),
        pem_fingerprint(&rig.certs.appliance.0),
    );
    let mut appliance = rig.appliance(&who, &pinned).await.unwrap();
    let (status, _) = request(
        &mut appliance,
        "POST",
        on_source::SEND,
        Some(send_request()),
    )
    .await;
    assert_eq!(status, 403);
    let (status, _) = request(
        &mut appliance,
        "POST",
        on_source::SEND,
        Some(json!({ "transfer_uuid": 1 })),
    )
    .await;
    assert!((400..500).contains(&status), "{status}");
    assert!(rig.zfs_calls().is_empty());
}

#[tokio::test]
async fn the_host_hangs_up_on_an_appliance_with_another_certificate() {
    let mut rig = rig().await;
    let (stranger, pinned) = (
        rig.certs.stranger.clone(),
        pem_fingerprint(&rig.certs.appliance.0),
    );
    let refused = match rig.appliance(&stranger, &pinned).await {
        Err(e) => e,
        Ok(mut sender) => {
            let request = hyper::Request::builder()
                .uri(format!("https://mox1{}", on_source::CATALOG))
                .body(Full::new(Bytes::new()))
                .unwrap();
            match sender.send_request(request).await {
                Err(e) => e.to_string(),
                Ok(response) => panic!("a stranger got {}", response.status()),
            }
        }
    };
    assert!(!refused.is_empty());
    assert!(
        !rig.engine
            .calls
            .lock()
            .unwrap()
            .iter()
            .any(|c| matches!(c, SourceCall::Catalog { .. })),
        "a stranger's request never reaches the engine"
    );
}

#[tokio::test]
async fn a_connection_is_kept_to_each_appliance_with_an_address() {
    let rig = rig().await;
    let target = |n: u8, address: Option<&str>| SourceTarget {
        name: format!("vault{n}"),
        fingerprint: format!("{n:02x}").repeat(32),
        address: address.map(|a| a.parse().unwrap()),
    };
    let mut dialers = Dialers::default();
    dialers.reconcile(
        &[target(1, Some("192.0.2.1")), target(2, None)],
        &rig.app,
        &rig.node(),
    );
    assert_eq!(dialers.len(), 1, "no address, no connection");
    dialers.reconcile(
        &[target(1, Some("192.0.2.9")), target(2, Some("192.0.2.2"))],
        &rig.app,
        &rig.node(),
    );
    assert_eq!(dialers.len(), 2);
    dialers.reconcile(&[], &rig.app, &rig.node());
    assert!(
        dialers.is_empty(),
        "a revoked appliance's connection is dropped"
    );
}
