// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

//! The appliance's own HTTPS UI on port 8443 (`BACKUP.md`, "Appliance
//! platform"), with its own certificate, whose fingerprint the console
//! shows. The administrator logs in with the appliance password; the token
//! lives in page memory only, as in the Web Dashboard (`SECURITY.md`), and
//! the headers are the Dashboard's: a strict CSP with a per-load style
//! nonce, no cookies, no CORS, `no-store` on every API answer.

mod api;
pub mod assets;
pub mod sessions;
#[cfg(test)]
mod tests;

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::extract::{DefaultBodyLimit, State};
use axum::http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode, Uri, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::any;
use bmac_core::ApplianceAddress;
use bmac_exec::BoxFuture;
use bmac_notify::postmark::Transport;
use bmac_proto::appliance::TransferReport;
use bmac_proto::appliance_ui::APPLIANCE_API;
use hyper_util::rt::{TokioExecutor, TokioIo, TokioTimer};
use hyper_util::server::conn::auto;
use hyper_util::service::TowerToHyperService;
use rustls::ServerConfig;
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use tokio::net::{TcpListener, TcpStream};
use tokio_rustls::TlsAcceptor;

use crate::audit::Audit;
use crate::checkin::ClusterStatus;
use crate::journal::Metadata;
use crate::peers::Probe;
use crate::pool::Pool;
use crate::state::{Identity, Paths, PoolBinding};
use assets::Assets;
use sessions::Sessions;

pub const UI_PORT: u16 = 8443;
const MAX_BODY: usize = 64 * 1024;
const MAX_CONNECTIONS: usize = 128;
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

pub const CONTENT_SECURITY_POLICY: &str = "default-src 'self'; script-src 'self'; style-src 'self'; \
    img-src 'self' data:; font-src 'self'; connect-src 'self'; object-src 'none'; base-uri 'none'; \
    form-action 'none'; frame-ancestors 'none'";
pub const STYLE_NONCE_PLACEHOLDER: &str = "__BMAC_STYLE_NONCE__";

const SECURITY_HEADERS: [(&str, &str); 7] = [
    ("content-security-policy", CONTENT_SECURITY_POLICY),
    ("x-content-type-options", "nosniff"),
    ("referrer-policy", "no-referrer"),
    ("x-frame-options", "DENY"),
    ("cross-origin-opener-policy", "same-origin"),
    ("cross-origin-resource-policy", "same-origin"),
    (
        "permissions-policy",
        "camera=(), microphone=(), geolocation=(), usb=(), payment=()",
    ),
];

/// What the UI shows of one enrolled cluster.
#[derive(Debug, Clone, Default)]
pub struct ClusterSnapshot {
    pub status: ClusterStatus,
    pub hosts: Vec<String>,
    pub transfers: Vec<TransferReport>,
}

pub type Unlocked = Arc<dyn Fn() -> BoxFuture<'static, bool> + Send + Sync>;
pub type Clusters = Arc<dyn Fn() -> Vec<ClusterSnapshot> + Send + Sync>;
/// Connects to a peer and compares the certificate it presents.
pub type Prober = Arc<dyn Fn(String, ApplianceAddress) -> BoxFuture<'static, Probe> + Send + Sync>;
/// A Postmark transport for a server token.
pub type Mailer = Arc<dyn Fn(&str) -> Result<Arc<dyn Transport>, String> + Send + Sync>;

pub struct Ui {
    pub paths: Paths,
    pub identity: Identity,
    /// The certificate's SHA-256, lowercase hex.
    pub fingerprint: String,
    pub metadata: Metadata,
    pub audit: Arc<Audit>,
    pub pool: Arc<dyn Pool>,
    pub binding: PoolBinding,
    pub unlocked: Unlocked,
    pub clusters: Clusters,
    pub probe: Prober,
    pub mailer: Mailer,
    pub now_ms: Arc<dyn Fn() -> i64 + Send + Sync>,
    /// SHA-512 crypt rounds for a new password.
    pub rounds: u32,
    /// Wakes the scrub loop when the administrator asks for a scrub.
    pub scrub_now: Arc<tokio::sync::Notify>,
    pub sessions: Sessions,
    pub assets: Assets,
}

/// The client's address, for handlers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClientAddr(pub SocketAddr);

fn page_security_policy(nonce: &str) -> String {
    CONTENT_SECURITY_POLICY.replacen(
        "style-src 'self'",
        &format!("style-src 'self' 'nonce-{nonce}'"),
        1,
    )
}

async fn security_headers(request: axum::extract::Request, next: Next) -> Response {
    let mut response = next.run(request).await;
    let headers = response.headers_mut();
    for (name, value) in SECURITY_HEADERS {
        let name = HeaderName::from_static(name);
        if name == header::CONTENT_SECURITY_POLICY && headers.contains_key(&name) {
            continue;
        }
        headers.insert(name, HeaderValue::from_static(value));
    }
    headers.remove(header::SET_COOKIE);
    response
}

async fn static_file(State(ui): State<Arc<Ui>>, method: Method, uri: Uri) -> Response {
    if method != Method::GET && method != Method::HEAD {
        let mut headers = HeaderMap::new();
        headers.insert(header::ALLOW, HeaderValue::from_static("GET, HEAD"));
        return (StatusCode::METHOD_NOT_ALLOWED, headers).into_response();
    }
    let Some((path, bytes)) = ui.assets.resolve(uri.path()) else {
        return (
            StatusCode::NOT_FOUND,
            [(header::CACHE_CONTROL, "no-cache")],
            "not found\n",
        )
            .into_response();
    };
    let cache = if path.starts_with("/assets/") {
        "public, max-age=31536000, immutable"
    } else {
        "no-cache"
    };
    let headers = [
        (header::CONTENT_TYPE, assets::content_type(path)),
        (header::CACHE_CONTROL, cache),
    ];
    if path == "/index.html" {
        let Ok(nonce) = crate::grants::random(16) else {
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        };
        let nonce: String = nonce.iter().map(|b| format!("{b:02x}")).collect();
        let page = String::from_utf8_lossy(bytes).replace(STYLE_NONCE_PLACEHOLDER, &nonce);
        let mut response = if method == Method::HEAD {
            (headers, ()).into_response()
        } else {
            (headers, page).into_response()
        };
        response.headers_mut().insert(
            header::CONTENT_SECURITY_POLICY,
            HeaderValue::from_str(&page_security_policy(&nonce)).expect("ASCII policy"),
        );
        return response;
    }
    if method == Method::HEAD {
        let mut response = (headers, ()).into_response();
        response
            .headers_mut()
            .insert(header::CONTENT_LENGTH, HeaderValue::from(bytes.len()));
        return response;
    }
    (headers, bytes).into_response()
}

pub fn router(ui: Arc<Ui>) -> Router {
    Router::new()
        .route(&format!("{APPLIANCE_API}/{{*rest}}"), any(api::handle))
        .fallback(static_file)
        .with_state(ui)
        .layer(DefaultBodyLimit::max(MAX_BODY))
        .layer(middleware::from_fn(security_headers))
}

/// The server half of the appliance's TLS: its own certificate, which the
/// administrator checks by fingerprint.
pub fn tls_config(cert_pem: &str, key_pem: &str) -> Result<Arc<ServerConfig>, String> {
    let cert = CertificateDer::pem_slice_iter(cert_pem.as_bytes())
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("appliance certificate: {e}"))?;
    let key = PrivateKeyDer::from_pem_slice(key_pem.as_bytes())
        .map_err(|e| format!("appliance key: {e}"))?;
    let mut config = ServerConfig::builder_with_provider(crate::client::provider())
        .with_safe_default_protocol_versions()
        .map_err(|e| e.to_string())?
        .with_no_client_auth()
        .with_single_cert(cert, key)
        .map_err(|e| e.to_string())?;
    config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
    Ok(Arc::new(config))
}

async fn connection(acceptor: TlsAcceptor, app: Router, tcp: TcpStream, peer: SocketAddr) {
    let _ = tcp.set_nodelay(true);
    let tls = match tokio::time::timeout(HANDSHAKE_TIMEOUT, acceptor.accept(tcp)).await {
        Ok(Ok(tls)) => tls,
        Ok(Err(_)) | Err(_) => return,
    };
    let app = app.layer(axum::Extension(ClientAddr(peer)));
    let mut builder = auto::Builder::new(TokioExecutor::new());
    builder
        .http1()
        .timer(TokioTimer::new())
        .header_read_timeout(HANDSHAKE_TIMEOUT);
    builder
        .http2()
        .timer(TokioTimer::new())
        .max_concurrent_streams(32);
    let _ = builder
        .serve_connection(TokioIo::new(tls), TowerToHyperService::new(app))
        .await;
}

pub async fn serve(listener: TcpListener, tls: Arc<ServerConfig>, ui: Arc<Ui>) {
    let acceptor = TlsAcceptor::from(tls);
    let app = router(ui);
    let slots = Arc::new(tokio::sync::Semaphore::new(MAX_CONNECTIONS));
    let mut connections = tokio::task::JoinSet::new();
    loop {
        let accepted = tokio::select! {
            accepted = listener.accept() => accepted,
            Some(_) = connections.join_next() => continue,
        };
        let (tcp, peer) = match accepted {
            Ok(accepted) => accepted,
            Err(e) => {
                eprintln!("bmac-backupd: ui: accept failed: {e}");
                tokio::time::sleep(Duration::from_millis(100)).await;
                continue;
            }
        };
        let Ok(permit) = slots.clone().try_acquire_owned() else {
            continue;
        };
        let (acceptor, app) = (acceptor.clone(), app.clone());
        connections.spawn(async move {
            connection(acceptor, app, tcp, peer).await;
            drop(permit);
        });
    }
}
