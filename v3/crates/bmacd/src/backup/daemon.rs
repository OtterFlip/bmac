// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

//! `bmacd source`, run by `bmac-source.service` as the user `bmac-source`
//! (`SECURITY.md`, "Process separation"; `BACKUP.md`, "Connections"). It
//! listens on nothing: it dials every approved appliance's address on port
//! 8445, accepts only the certificate the cluster recorded when it approved
//! the appliance, presents the node's certificate, and serves the
//! appliance's requests over that connection (HTTP/2, ALPN
//! `bmac-source/1`). Every request is decided by the engine, and a send runs
//! `zfs send` under ZFS's `send`, `hold`, and `release` delegation on the
//! guest datasets, nothing more.

use std::collections::BTreeMap;
use std::io;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::pin::Pin;
use std::process::Stdio;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use axum::Router;
use axum::extract::{DefaultBodyLimit, Extension, State};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use bmac_core::{AddressHost, ApplianceAddress};
use bmac_proto::api::{ApiError, ErrorCode};
use bmac_proto::appliance::{
    CheckinRequest, ClaimRequest, IndexRequest, LocateRequest, SOURCE_ALPN, SOURCE_PORT,
    SendRequest, on_source,
};
use bytes::Bytes;
use http_body::Frame;
use hyper_util::rt::{TokioExecutor, TokioIo, TokioTimer};
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::CryptoProvider;
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName, UnixTime};
use rustls::{ClientConfig, DigitallySignedStruct, SignatureScheme};
use sha2::{Digest, Sha256};
use tokio::io::AsyncReadExt;
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use super::source::{SendPlan, TransferDone};
use super::source_socket::{MAX_FRAME, SourceCall, SourceClient, SourceTarget};
use crate::log;
use crate::web::api::api_error;
use crate::web::https::ClientCertificate;
use crate::web::tls::TlsError;

/// The credentials the unit passes with `LoadCredential`: the node's
/// Proxmox certificate and key, and the cluster CA, which appliances pin.
pub const CERT_CREDENTIAL: &str = "node.pem";
pub const KEY_CREDENTIAL: &str = "node.key";
pub const CA_CREDENTIAL: &str = "ca.pem";
pub const ZFS: &str = "/usr/sbin/zfs";
/// How often the engine is asked which appliances to keep a connection to.
const APPLIANCES_EVERY: Duration = Duration::from_secs(10);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
/// The wait before dialing again, doubling from the first to the second.
const REDIAL: (Duration, Duration) = (Duration::from_secs(2), Duration::from_secs(60));
/// A connection that lasted this long was healthy: the next dial is quick.
const HEALTHY: Duration = Duration::from_secs(120);
const KEEPALIVE: Duration = Duration::from_secs(30);
const KEEPALIVE_TIMEOUT: Duration = Duration::from_secs(20);
const CHUNK: usize = 256 * 1024;
const MAX_STDERR: usize = 4096;

pub fn fingerprint(der: &[u8]) -> String {
    Sha256::digest(der)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// Accepts only the appliance's own certificate, by the fingerprint the
/// cluster recorded at enrollment; the certificate is self-signed.
#[derive(Debug)]
struct AppliancePin {
    fingerprint: String,
    provider: Arc<CryptoProvider>,
}

impl ServerCertVerifier for AppliancePin {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        if fingerprint(end_entity) == self.fingerprint {
            Ok(ServerCertVerified::assertion())
        } else {
            Err(rustls::Error::General(
                "the appliance presented another certificate than the one approved".into(),
            ))
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

/// TLS 1.3 to the appliance whose certificate has `appliance` as its
/// fingerprint, presenting the node certificate followed by the cluster CA
/// so the appliance can check the chain against the CA it pinned.
pub fn client_config(
    cert_pem: &[u8],
    key_pem: &[u8],
    ca_pem: &[u8],
    appliance: &str,
) -> Result<Arc<ClientConfig>, TlsError> {
    let provider = crate::mesh::tls::provider();
    let mut chain = crate::mesh::tls::certificates(cert_pem)?;
    chain.extend(crate::mesh::tls::certificates(ca_pem)?);
    let key = PrivateKeyDer::from_pem_slice(key_pem).map_err(|e| TlsError::Key(e.to_string()))?;
    let pin = Arc::new(AppliancePin {
        fingerprint: appliance.to_ascii_lowercase(),
        provider: provider.clone(),
    });
    let mut client = ClientConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS13])?
        .dangerous()
        .with_custom_certificate_verifier(pin)
        .with_client_auth_cert(chain, key)?;
    client.alpn_protocols = vec![SOURCE_ALPN.to_vec()];
    Ok(Arc::new(client))
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

/// Where `address` answers.
pub(crate) async fn resolve(address: &ApplianceAddress) -> Result<Vec<SocketAddr>, String> {
    let port = address.port_or(SOURCE_PORT);
    let found: Vec<SocketAddr> = tokio::net::lookup_host((address.host_text(), port))
        .await
        .map_err(|e| format!("{address}: {e}"))?
        .collect();
    if found.is_empty() {
        return Err(format!("{address} doesn't resolve"));
    }
    Ok(found)
}

/// Opens one connection to the appliance at `address` and serves its
/// requests over it until either side closes it.
pub async fn connect(
    app: Arc<App>,
    tls: Arc<ClientConfig>,
    name: &str,
    address: &ApplianceAddress,
) -> Result<(), String> {
    let mut last = format!("{address} has no address");
    let mut tcp = None;
    for socket in resolve(address).await? {
        match tokio::time::timeout(CONNECT_TIMEOUT, TcpStream::connect(socket)).await {
            Ok(Ok(stream)) => {
                tcp = Some(stream);
                break;
            }
            Ok(Err(e)) => last = format!("{socket}: {e}"),
            Err(_) => last = format!("{socket}: timed out"),
        }
    }
    let tcp = tcp.ok_or(last)?;
    let _ = tcp.set_nodelay(true);
    let server_name = match &address.host {
        AddressHost::Ip(ip) => ServerName::IpAddress((*ip).into()),
        AddressHost::Name(host) => {
            ServerName::try_from(host.clone()).map_err(|e| format!("{address}: {e}"))?
        }
    };
    let tls = tokio::time::timeout(
        CONNECT_TIMEOUT,
        tokio_rustls::TlsConnector::from(tls).connect(server_name, tcp),
    )
    .await
    .map_err(|_| format!("{address}: the TLS handshake timed out"))?
    .map_err(|e| format!("{address}: {e}"))?;
    let (_, session) = tls.get_ref();
    if session.alpn_protocol() != Some(SOURCE_ALPN) {
        return Err(format!("{address} doesn't speak bmac-source/1"));
    }
    let certificate = session
        .peer_certificates()
        .and_then(|chain| chain.first())
        .map(|c| c.clone().into_owned());
    log::info(&format!(
        "bmac-source: connected to appliance {name} at {address}"
    ));
    let service = hyper_util::service::TowerToHyperService::new(
        router(app).layer(Extension(ClientCertificate(certificate))),
    );
    hyper::server::conn::http2::Builder::new(TokioExecutor::new())
        .timer(TokioTimer::new())
        .keep_alive_interval(Some(KEEPALIVE))
        .keep_alive_timeout(KEEPALIVE_TIMEOUT)
        .serve_connection(TokioIo::new(tls), service)
        .await
        .map_err(|e| format!("{address}: {e}"))
}

/// Keeps a connection to one appliance, dialing again when it ends.
async fn keep_connected(app: Arc<App>, tls: Arc<ClientConfig>, target: SourceTarget) {
    let Some(address) = target.address.clone() else {
        return;
    };
    let mut wait = REDIAL.0;
    let mut failing = false;
    loop {
        let started = Instant::now();
        let outcome = connect(app.clone(), tls.clone(), &target.name, &address).await;
        if started.elapsed() >= HEALTHY {
            wait = REDIAL.0;
        }
        match outcome {
            Ok(()) => {
                log::info(&format!(
                    "bmac-source: appliance {} closed its connection",
                    target.name
                ));
                failing = false;
            }
            Err(e) if !failing => {
                log::warning(&format!(
                    "bmac-source: no connection to appliance {} at {address}: {e}",
                    target.name
                ));
                failing = true;
            }
            Err(_) => {}
        }
        tokio::time::sleep(wait).await;
        wait = (wait * 2).min(REDIAL.1);
    }
}

/// The node's credentials, PEM.
pub struct Credentials {
    pub cert: Vec<u8>,
    pub key: Vec<u8>,
    pub ca: Vec<u8>,
}

/// One task per appliance with an address, by fingerprint.
#[derive(Default)]
pub struct Dialers(BTreeMap<String, (SourceTarget, JoinHandle<()>)>);

impl Dialers {
    /// Starts a connection to each appliance that wants one, and drops the
    /// connections to those no longer approved or now somewhere else.
    pub fn reconcile(&mut self, targets: &[SourceTarget], app: &Arc<App>, node: &Credentials) {
        let wanted: BTreeMap<&str, &SourceTarget> = targets
            .iter()
            .filter(|t| t.address.is_some())
            .map(|t| (t.fingerprint.as_str(), t))
            .collect();
        self.0.retain(|fingerprint, (target, task)| {
            let keep = wanted.get(fingerprint.as_str()) == Some(&&*target);
            if !keep {
                task.abort();
            }
            keep
        });
        for (fingerprint, target) in wanted {
            if self.0.contains_key(fingerprint) {
                continue;
            }
            let tls = match client_config(&node.cert, &node.key, &node.ca, fingerprint) {
                Ok(tls) => tls,
                Err(e) => {
                    log::warning(&format!(
                        "bmac-source: no connection to appliance {}: {e}",
                        target.name
                    ));
                    continue;
                }
            };
            let task = tokio::spawn(keep_connected(app.clone(), tls, target.clone()));
            self.0
                .insert(fingerprint.to_string(), (target.clone(), task));
        }
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl Drop for Dialers {
    fn drop(&mut self) {
        for (_, task) in self.0.values() {
            task.abort();
        }
    }
}

async fn appliances(client: &SourceClient) -> Result<Vec<SourceTarget>, String> {
    client
        .typed(&SourceCall::Appliances)
        .await
        .map_err(|e| e.message)
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
    let node = Credentials {
        cert: read(CERT_CREDENTIAL)?,
        key: read(KEY_CREDENTIAL)?,
        ca: read(CA_CREDENTIAL)?,
    };
    let client = SourceClient::new(&options.socket);
    let mut tries = 0;
    let mut targets = loop {
        match appliances(&client).await {
            Ok(targets) => break targets,
            Err(e) => {
                tries += 1;
                if tries >= 40 {
                    return Err(format!("the engine doesn't answer: {e}"));
                }
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
        }
    };
    let app = Arc::new(App {
        client: client.clone(),
        zfs: options.zfs,
    });
    log::info(&format!(
        "bmac-source {} running",
        env!("CARGO_PKG_VERSION")
    ));
    crate::systemd::notify_ready().map_err(|e| format!("cannot notify systemd: {e}"))?;
    tokio::pin!(shutdown);
    let mut dialers = Dialers::default();
    let mut failing = false;
    loop {
        dialers.reconcile(&targets, &app, &node);
        tokio::select! {
            () = &mut shutdown => break,
            () = tokio::time::sleep(APPLIANCES_EVERY) => {}
        }
        match appliances(&client).await {
            Ok(next) => {
                if failing {
                    log::info("bmac-source: the engine answers again");
                    failing = false;
                }
                targets = next;
            }
            Err(e) if !failing => {
                log::warning(&format!(
                    "bmac-source: cannot list the approved appliances: {e}"
                ));
                failing = true;
            }
            Err(_) => {}
        }
    }
    drop(dialers);
    log::info("bmac-source stopped");
    Ok(())
}

#[cfg(test)]
#[path = "daemon_tests.rs"]
mod tests;
