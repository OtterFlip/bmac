// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use bmac_core::SnapshotGuid;
use bmac_proto::api::{ApiError, ErrorCode};
use bmac_proto::appliance::{SendRequest, on_source};
use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper_util::rt::TokioIo;
use rcgen::{BasicConstraints, CertificateParams, IsCa, KeyPair};
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName};
use serde_json::{Value, json};
use tokio::net::{TcpListener, TcpStream};
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
                SourceCall::Fingerprints => Ok(json!([self.allowed])),
                SourceCall::Listen => Ok(json!([])),
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
    address: std::net::SocketAddr,
    engine: Arc<FakeEngine>,
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
    let mut tasks = vec![tokio::spawn(serve(
        listener,
        AllowedPeers::new([uid]),
        engine.clone(),
        std::future::pending(),
    ))];
    let client = SourceClient::new(&socket);
    let allow = Arc::new(Allowlist::default());
    refresh(&client, &allow).await.unwrap();
    let tls = server_config(
        certs.node.as_bytes(),
        certs.node_key.as_bytes(),
        certs.ca.as_bytes(),
        allow,
    )
    .unwrap();
    let app = Arc::new(App { client, zfs });
    let tcp = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = tcp.local_addr().unwrap();
    tasks.push(tokio::spawn(HttpsServer::new(tls, router(app)).serve(tcp)));
    Rig {
        _dir: dir,
        certs,
        address,
        engine,
        log,
        tasks,
    }
}

impl Rig {
    /// A client pinning the cluster CA and presenting `who`'s certificate.
    async fn connect(
        &self,
        who: &(String, String),
    ) -> Result<hyper::client::conn::http1::SendRequest<Full<Bytes>>, String> {
        let mut roots = rustls::RootCertStore::empty();
        roots
            .add(CertificateDer::from_pem_slice(self.certs.ca.as_bytes()).unwrap())
            .unwrap();
        let config = rustls::ClientConfig::builder_with_provider(crate::mesh::tls::provider())
            .with_protocol_versions(&[&rustls::version::TLS13])
            .unwrap()
            .with_root_certificates(roots)
            .with_client_auth_cert(
                vec![CertificateDer::from_pem_slice(who.0.as_bytes()).unwrap()],
                PrivateKeyDer::from_pem_slice(who.1.as_bytes()).unwrap(),
            )
            .unwrap();
        let tcp = TcpStream::connect(self.address).await.unwrap();
        let tls = tokio_rustls::TlsConnector::from(Arc::new(config))
            .connect(ServerName::try_from("localhost").unwrap(), tcp)
            .await
            .map_err(|e| e.to_string())?;
        let (sender, connection) = hyper::client::conn::http1::handshake(TokioIo::new(tls))
            .await
            .map_err(|e| e.to_string())?;
        tokio::spawn(connection);
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
    sender: &mut hyper::client::conn::http1::SendRequest<Full<Bytes>>,
    method: &str,
    path: &str,
    body: Option<Value>,
) -> (u16, Bytes) {
    let body = body.map(|b| b.to_string()).unwrap_or_default();
    let request = hyper::Request::builder()
        .method(method)
        .uri(path)
        .header("host", "localhost")
        .header("content-type", "application/json")
        .body(Full::new(Bytes::from(body)))
        .unwrap();
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
async fn an_enrolled_appliance_streams_a_held_send_and_the_engine_hears_how_it_went() {
    let rig = rig().await;
    let mut appliance = rig.connect(&rig.certs.appliance).await.unwrap();
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
    let rig = rig().await;
    rig.engine
        .refuse_sends
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let mut appliance = rig.connect(&rig.certs.appliance).await.unwrap();
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
async fn a_certificate_that_isnt_enrolled_fails_the_handshake() {
    let rig = rig().await;
    let refused = match rig.connect(&rig.certs.stranger).await {
        Err(e) => e,
        Ok(mut sender) => {
            // TLS 1.3 reports a refused client certificate on the first read.
            let request = hyper::Request::builder()
                .uri(on_source::CATALOG)
                .header("host", "localhost")
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

#[test]
fn the_allowlist_admits_no_one_until_the_engine_has_answered() {
    let allow = Allowlist::default();
    assert!(!allow.allows("abc"));
    allow.set(["abc".to_string()]);
    assert!(allow.allows("abc"));
    assert!(!allow.allows("abd"));
    allow.set([]);
    assert!(!allow.allows("abc"));
}
