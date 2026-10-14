// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

//! `bmacd source`, run by `bmac-source.service` as the user `bmac-source`
//! (`SECURITY.md`, "Process separation"): port 8445 for enrolled
//! appliances, with mutual TLS. The handshake lets through only the
//! certificates of approved appliances, which the engine lists; every
//! request is then decided by the engine, and a send runs `zfs send` under
//! ZFS's `send`, `hold`, and `release` delegation on the guest datasets,
//! nothing more.

use std::collections::BTreeSet;
use std::io;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::pin::Pin;
use std::process::Stdio;
use std::sync::{Arc, RwLock};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use axum::Router;
use axum::extract::{DefaultBodyLimit, Extension, State};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use bmac_proto::api::{ApiError, ErrorCode};
use bmac_proto::appliance::{
    CheckinRequest, ClaimRequest, IndexRequest, LocateRequest, SendRequest, on_source,
};
use bytes::Bytes;
use http_body::Frame;
use rustls::client::danger::HandshakeSignatureValid;
use rustls::crypto::CryptoProvider;
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, UnixTime};
use rustls::server::danger::{ClientCertVerified, ClientCertVerifier};
use rustls::{DigitallySignedStruct, DistinguishedName, ServerConfig, SignatureScheme};
use sha2::{Digest, Sha256};
use tokio::io::AsyncReadExt;
use tokio::sync::mpsc;

use super::source::{SendPlan, TransferDone};
use super::source_socket::{MAX_FRAME, SourceCall, SourceClient};
use crate::log;
use crate::web::api::api_error;
use crate::web::https::{ClientCertificate, HttpsServer, Listeners};
use crate::web::tls::TlsError;

/// The credentials the unit passes with `LoadCredential`: the node's
/// Proxmox certificate and key, and the cluster CA, which appliances pin.
pub const CERT_CREDENTIAL: &str = "node.pem";
pub const KEY_CREDENTIAL: &str = "node.key";
pub const CA_CREDENTIAL: &str = "ca.pem";
pub const ZFS: &str = "/usr/sbin/zfs";
const FINGERPRINTS_EVERY: Duration = Duration::from_secs(10);
/// Without a fresh list from the engine for this long, everyone is refused.
const FINGERPRINTS_STALE: Duration = Duration::from_secs(60);
const LISTEN_EVERY: Duration = Duration::from_secs(30);
const CHUNK: usize = 256 * 1024;
const MAX_STDERR: usize = 4096;

/// The approved appliances' fingerprints, lowercase hex.
#[derive(Debug, Default)]
pub struct Allowlist(RwLock<(BTreeSet<String>, Option<Instant>)>);

impl Allowlist {
    pub fn set(&self, fingerprints: impl IntoIterator<Item = String>) {
        let mut inner = self.0.write().unwrap_or_else(|p| p.into_inner());
        *inner = (fingerprints.into_iter().collect(), Some(Instant::now()));
    }

    pub fn allows(&self, fingerprint: &str) -> bool {
        let inner = self.0.read().unwrap_or_else(|p| p.into_inner());
        inner.1.is_some_and(|at| at.elapsed() < FINGERPRINTS_STALE) && inner.0.contains(fingerprint)
    }
}

pub fn fingerprint(der: &[u8]) -> String {
    Sha256::digest(der)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// Lets through the handshake only a certificate on the allowlist. An
/// appliance's certificate is its own, self-signed; its fingerprint, from
/// enrollment, is the trust.
#[derive(Debug)]
struct EnrolledVerifier {
    allow: Arc<Allowlist>,
    provider: Arc<CryptoProvider>,
}

impl ClientCertVerifier for EnrolledVerifier {
    fn offer_client_auth(&self) -> bool {
        true
    }

    fn client_auth_mandatory(&self) -> bool {
        true
    }

    fn root_hint_subjects(&self) -> &[DistinguishedName] {
        &[]
    }

    fn verify_client_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _now: UnixTime,
    ) -> Result<ClientCertVerified, rustls::Error> {
        if self.allow.allows(&fingerprint(end_entity)) {
            Ok(ClientCertVerified::assertion())
        } else {
            Err(rustls::Error::General("not an enrolled appliance".into()))
        }
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Err(rustls::Error::General("TLS 1.2 is not offered".into()))
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

/// TLS 1.3 with the node certificate, followed by the cluster CA so an
/// appliance that pins the CA can verify the chain.
pub fn server_config(
    cert_pem: &[u8],
    key_pem: &[u8],
    ca_pem: &[u8],
    allow: Arc<Allowlist>,
) -> Result<Arc<ServerConfig>, TlsError> {
    let provider = crate::mesh::tls::provider();
    let mut chain = crate::mesh::tls::certificates(cert_pem)?;
    chain.extend(crate::mesh::tls::certificates(ca_pem)?);
    let key = PrivateKeyDer::from_pem_slice(key_pem).map_err(|e| TlsError::Key(e.to_string()))?;
    let verifier = Arc::new(EnrolledVerifier {
        allow,
        provider: provider.clone(),
    });
    let mut server = ServerConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS13])?
        .with_client_cert_verifier(verifier)
        .with_single_cert(chain, key)?;
    server.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
    Ok(Arc::new(server))
}

pub struct App {
    pub client: SourceClient,
    pub zfs: PathBuf,
}

pub fn router(app: Arc<App>) -> Router {
    Router::new()
        .route(on_source::CHECKIN, post(checkin))
        .route(on_source::CLAIM, post(claim))
        .route(on_source::INDEX, post(index))
        .route(on_source::CATALOG, get(catalog))
        .route(on_source::LOCATE, post(locate))
        .route(on_source::SEND, post(send))
        .route(on_source::HEARTBEAT, get(heartbeat))
        .route(on_source::LUKS_HEADERS, get(luks_headers))
        .route(on_source::LUKS_HEADER, post(luks_header))
        .route(on_source::RECOVERY_MANIFEST, get(recovery_manifest))
        .layer(DefaultBodyLimit::max(MAX_FRAME))
        .with_state(app)
}

type Cert = Extension<ClientCertificate>;
use axum::Json;

fn presented(certificate: &ClientCertificate) -> Result<String, ApiError> {
    certificate
        .0
        .as_ref()
        .map(|c| fingerprint(c))
        .ok_or_else(|| ApiError::new(ErrorCode::Unauthenticated, "no client certificate"))
}

/// Asks the engine, on behalf of the appliance whose certificate this is.
async fn forward(
    app: &App,
    certificate: &ClientCertificate,
    call: impl FnOnce(String) -> SourceCall,
) -> Response {
    let answer = match presented(certificate) {
        Ok(fingerprint) => app.client.call(&call(fingerprint)).await,
        Err(e) => Err(e),
    };
    match answer {
        Ok(body) => Json(body).into_response(),
        Err(error) => api_error(error),
    }
}

async fn checkin(
    State(app): State<Arc<App>>,
    Extension(cert): Cert,
    Json(request): Json<CheckinRequest>,
) -> Response {
    forward(&app, &cert, |fingerprint| SourceCall::Checkin {
        fingerprint,
        request: Box::new(request),
    })
    .await
}

async fn claim(
    State(app): State<Arc<App>>,
    Extension(cert): Cert,
    Json(request): Json<ClaimRequest>,
) -> Response {
    forward(&app, &cert, |fingerprint| SourceCall::Claim {
        fingerprint,
        request,
    })
    .await
}

async fn index(
    State(app): State<Arc<App>>,
    Extension(cert): Cert,
    Json(request): Json<IndexRequest>,
) -> Response {
    forward(&app, &cert, |fingerprint| SourceCall::Index {
        fingerprint,
        request,
    })
    .await
}

async fn catalog(State(app): State<Arc<App>>, Extension(cert): Cert) -> Response {
    forward(&app, &cert, |fingerprint| SourceCall::Catalog {
        fingerprint,
    })
    .await
}

async fn locate(
    State(app): State<Arc<App>>,
    Extension(cert): Cert,
    Json(request): Json<LocateRequest>,
) -> Response {
    forward(&app, &cert, |fingerprint| SourceCall::Locate {
        fingerprint,
        request,
    })
    .await
}

async fn heartbeat(State(app): State<Arc<App>>, Extension(cert): Cert) -> Response {
    forward(&app, &cert, |fingerprint| SourceCall::Heartbeat {
        fingerprint,
    })
    .await
}

async fn luks_headers(State(app): State<Arc<App>>, Extension(cert): Cert) -> Response {
    forward(&app, &cert, |fingerprint| SourceCall::LuksHeaders {
        fingerprint,
    })
    .await
}

async fn luks_header(
    State(app): State<Arc<App>>,
    Extension(cert): Cert,
    Json(request): Json<bmac_proto::appliance::LuksHeaderRequest>,
) -> Response {
    forward(&app, &cert, |fingerprint| SourceCall::LuksHeader {
        fingerprint,
        request,
    })
    .await
}

async fn recovery_manifest(State(app): State<Arc<App>>, Extension(cert): Cert) -> Response {
    forward(&app, &cert, |fingerprint| SourceCall::RecoveryManifest {
        fingerprint,
    })
    .await
}

async fn zfs(program: &PathBuf, args: &[String]) -> Result<(), String> {
    let output = tokio::process::Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .await
        .map_err(|e| format!("cannot run zfs: {e}"))?;
    if output.status.success() {
        Ok(())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        Err(format!("zfs {}: {}", args[0], stderr.trim()))
    }
}

/// A response body fed by the task that reads `zfs send`.
struct ChannelBody(mpsc::Receiver<Result<Frame<Bytes>, io::Error>>);

impl http_body::Body for ChannelBody {
    type Data = Bytes;
    type Error = io::Error;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, io::Error>>> {
        self.0.poll_recv(cx)
    }
}

async fn send(
    State(app): State<Arc<App>>,
    Extension(cert): Cert,
    Json(request): Json<SendRequest>,
) -> Response {
    let fingerprint = match presented(&cert) {
        Ok(f) => f,
        Err(e) => return api_error(e),
    };
    let plan: SendPlan = match app
        .client
        .typed(&SourceCall::AuthorizeSend {
            fingerprint: fingerprint.clone(),
            request: request.clone(),
        })
        .await
    {
        Ok(plan) => plan,
        Err(error) => return api_error(error),
    };
    let hold = vec!["hold".to_string(), plan.hold_tag(), plan.snapshot.clone()];
    if let Err(e) = zfs(&app.zfs, &hold).await {
        return api_error(ApiError::new(ErrorCode::Conflict, e));
    }
    let child = tokio::process::Command::new(&app.zfs)
        .args(plan.send_args())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn();
    let child = match child {
        Ok(child) => child,
        Err(e) => {
            release(&app, &plan).await;
            return api_error(ApiError::new(
                ErrorCode::Internal,
                format!("cannot run zfs send: {e}"),
            ));
        }
    };
    let (tx, rx) = mpsc::channel(4);
    tokio::spawn(stream(app, fingerprint, request, plan, child, tx));
    Response::builder()
        .header(axum::http::header::CONTENT_TYPE, "application/octet-stream")
        .body(axum::body::Body::new(ChannelBody(rx)))
        .unwrap_or_else(|_| api_error(ApiError::new(ErrorCode::Internal, "no response")))
}

async fn release(app: &App, plan: &SendPlan) {
    let args = vec![
        "release".to_string(),
        plan.hold_tag(),
        plan.snapshot.clone(),
    ];
    if let Err(e) = zfs(&app.zfs, &args).await {
        log::warning(&format!("backup: the hold on {} stays: {e}", plan.snapshot));
    }
}

/// Feeds the stream, then releases the hold and reports the transfer.
async fn stream(
    app: Arc<App>,
    fingerprint: String,
    request: SendRequest,
    plan: SendPlan,
    mut child: tokio::process::Child,
    tx: mpsc::Sender<Result<Frame<Bytes>, io::Error>>,
) {
    let started = Instant::now();
    let mut stdout = child.stdout.take().expect("piped");
    let mut stderr = child.stderr.take().expect("piped");
    let errors = tokio::spawn(async move {
        let mut text = Vec::new();
        let _ = (&mut stderr)
            .take(MAX_STDERR as u64)
            .read_to_end(&mut text)
            .await;
        String::from_utf8_lossy(&text).trim().to_string()
    });
    let mut bytes = 0u64;
    let mut error = None;
    let mut buffer = vec![0u8; CHUNK];
    loop {
        match stdout.read(&mut buffer).await {
            Ok(0) => break,
            Ok(n) => {
                bytes += n as u64;
                let chunk = Bytes::copy_from_slice(&buffer[..n]);
                if tx.send(Ok(Frame::data(chunk))).await.is_err() {
                    error = Some("the appliance closed the stream".to_string());
                    let _ = child.start_kill();
                    break;
                }
            }
            Err(e) => {
                error = Some(format!("reading zfs send: {e}"));
                let _ = child.start_kill();
                break;
            }
        }
    }
    let status = child.wait().await;
    let stderr = errors.await.unwrap_or_default();
    if error.is_none() {
        match status {
            Ok(s) if s.success() => {}
            Ok(s) => error = Some(format!("zfs send exited with {s}: {stderr}")),
            Err(e) => error = Some(format!("zfs send: {e}")),
        }
        if let Some(e) = &error {
            let _ = tx.send(Err(io::Error::other(e.clone()))).await;
        }
    }
    drop(tx);
    release(&app, &plan).await;
    let report = TransferDone {
        transfer_uuid: request.transfer_uuid,
        vm_uuid: request.vm_uuid,
        pit_uuid: request.pit_uuid,
        volume_uuid: request.volume_uuid,
        bytes,
        duration_ms: started.elapsed().as_millis() as u64,
        error,
    };
    if let Err(e) = app
        .client
        .call(&SourceCall::TransferDone {
            fingerprint,
            report,
        })
        .await
    {
        log::warning(&format!("backup: cannot report a transfer: {}", e.message));
    }
}

async fn refresh(client: &SourceClient, allow: &Allowlist) -> Result<(), String> {
    let fingerprints: Vec<String> = client
        .typed(&SourceCall::Fingerprints)
        .await
        .map_err(|e| e.message)?;
    allow.set(fingerprints);
    Ok(())
}

pub struct Options {
    pub socket: PathBuf,
    pub zfs: PathBuf,
}

/// Runs until `shutdown` resolves.
pub async fn run(
    options: Options,
    shutdown: impl std::future::Future<Output = ()>,
) -> Result<(), String> {
    let read = |name: &str| {
        let path = crate::systemd::credential(name).map_err(|e| e.to_string())?;
        std::fs::read(&path).map_err(|e| format!("cannot read credential {name}: {e}"))
    };
    let (cert, key, ca) = (
        read(CERT_CREDENTIAL)?,
        read(KEY_CREDENTIAL)?,
        read(CA_CREDENTIAL)?,
    );
    let allow = Arc::new(Allowlist::default());
    let client = SourceClient::new(&options.socket);
    let mut tries = 0;
    while let Err(e) = refresh(&client, &allow).await {
        tries += 1;
        if tries >= 40 {
            return Err(format!("the engine doesn't answer: {e}"));
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    let tls = server_config(&cert, &key, &ca, allow.clone()).map_err(|e| e.to_string())?;
    let app = Arc::new(App {
        client: client.clone(),
        zfs: options.zfs,
    });
    let mut listeners = Listeners::new(HttpsServer::new(tls, router(app)));
    log::info(&format!(
        "bmac-source {} running",
        env!("CARGO_PKG_VERSION")
    ));
    crate::systemd::notify_ready().map_err(|e| format!("cannot notify systemd: {e}"))?;
    tokio::pin!(shutdown);
    let mut last_listen: Option<Instant> = None;
    let mut failing = false;
    loop {
        if last_listen.is_none_or(|t| t.elapsed() >= LISTEN_EVERY) {
            match client
                .typed::<Vec<std::net::IpAddr>>(&SourceCall::Listen)
                .await
            {
                Ok(ips) => {
                    let wanted: Vec<SocketAddr> = ips
                        .into_iter()
                        .map(|ip| SocketAddr::new(ip, bmac_proto::appliance::SOURCE_PORT))
                        .collect();
                    for (address, e) in listeners.reconcile(&wanted) {
                        log::warning(&format!("bmac-source: cannot listen on {address}: {e}"));
                    }
                    last_listen = Some(Instant::now());
                }
                Err(e) => log::warning(&format!("bmac-source: no listen addresses: {}", e.message)),
            }
        }
        match refresh(&client, &allow).await {
            Ok(()) if failing => {
                log::info("bmac-source: the engine answers again");
                failing = false;
            }
            Err(e) if !failing => {
                log::warning(&format!(
                    "bmac-source: cannot refresh the enrolled appliances: {e}"
                ));
                failing = true;
            }
            _ => {}
        }
        tokio::select! {
            () = &mut shutdown => break,
            () = tokio::time::sleep(FINGERPRINTS_EVERY) => {}
        }
    }
    drop(listeners);
    log::info("bmac-source stopped");
    Ok(())
}

#[cfg(test)]
#[path = "daemon_tests.rs"]
mod tests;
