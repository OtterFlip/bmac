// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

//! Connections to the cluster: TLS 1.3, the host's chain checked against
//! the Proxmox CA whose fingerprint the enrollment bundle gave, the host's
//! certificate checked for its name (`mox1`, whatever address reached it),
//! and the appliance's certificate presented to `bmac-source`.

use std::sync::Arc;
use std::time::Duration;

use bmac_core::ApplianceAddress;
use bmac_proto::api::{ApiError, ErrorCode};
use bmac_proto::appliance::SourceHost;
use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use hyper_util::rt::TokioIo;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::CryptoProvider;
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName, UnixTime};
use rustls::{ClientConfig, DigitallySignedStruct, RootCertStore, SignatureScheme};
use serde::Serialize;
use serde::de::DeserializeOwned;
use sha2::{Digest, Sha256};
use tokio::net::TcpStream;
use tokio::sync::mpsc;

use crate::pool::Stream;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
const CALL_TIMEOUT: Duration = Duration::from_secs(60);
/// A send's next chunk may take this long, while ZFS reads a big block.
const STREAM_IDLE: Duration = Duration::from_secs(300);

pub fn fingerprint(der: &[u8]) -> String {
    Sha256::digest(der)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

pub fn provider() -> Arc<CryptoProvider> {
    Arc::new(rustls::crypto::ring::default_provider())
}

/// What a server's certificate must be.
#[derive(Debug, Clone)]
enum Pin {
    /// A chain that carries the cluster CA with this fingerprint and leads
    /// to it, for the host's name.
    Ca(String),
    /// A peer appliance's own self-signed certificate, by fingerprint.
    Leaf(String),
}

/// Trusts a server only as its [`Pin`] says, and remembers the
/// fingerprint it presented.
#[derive(Debug)]
struct Pinned {
    pin: Pin,
    provider: Arc<CryptoProvider>,
    seen: std::sync::Mutex<Option<String>>,
}

impl ServerCertVerifier for Pinned {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        server_name: &ServerName<'_>,
        ocsp_response: &[u8],
        now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        let presented = fingerprint(end_entity);
        *self.seen.lock().unwrap_or_else(|p| p.into_inner()) = Some(presented.clone());
        let ca_fingerprint = match &self.pin {
            Pin::Leaf(wanted) if *wanted == presented => {
                return Ok(ServerCertVerified::assertion());
            }
            Pin::Leaf(_) => {
                return Err(rustls::Error::General(
                    "the peer presented another certificate than the trusted one".into(),
                ));
            }
            Pin::Ca(fingerprint) => fingerprint,
        };
        let ca = intermediates
            .iter()
            .find(|der| fingerprint(der).eq_ignore_ascii_case(ca_fingerprint))
            .ok_or_else(|| {
                rustls::Error::General(
                    "the host's chain has no certificate with the cluster CA's fingerprint".into(),
                )
            })?;
        let mut roots = RootCertStore::empty();
        roots
            .add(ca.clone().into_owned())
            .map_err(|e| rustls::Error::General(format!("cluster CA: {e}")))?;
        rustls::client::WebPkiServerVerifier::builder_with_provider(
            Arc::new(roots),
            self.provider.clone(),
        )
        .build()
        .map_err(|e| rustls::Error::General(e.to_string()))?
        .verify_server_cert(end_entity, intermediates, server_name, ocsp_response, now)
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

/// How a call failed: the host couldn't be reached (try another), or it
/// answered with a refusal (another host would answer the same).
#[derive(Debug, Clone, PartialEq)]
pub enum CallError {
    Unreachable(String),
    Refused(ApiError),
}

impl std::fmt::Display for CallError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CallError::Unreachable(e) => write!(f, "unreachable: {e}"),
            CallError::Refused(e) => write!(f, "refused: {}", e.message),
        }
    }
}

#[derive(Clone)]
pub struct ClusterClient {
    tls: Arc<ClientConfig>,
    verifier: Arc<Pinned>,
    /// The port when an address names none.
    pub port: u16,
}

impl std::fmt::Debug for ClusterClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ClusterClient")
            .field("port", &self.port)
            .finish_non_exhaustive()
    }
}

impl ClusterClient {
    /// `identity` is the appliance's certificate and key, PEM; without
    /// one (to enroll) no client certificate is presented.
    pub fn new(
        ca_fingerprint: &str,
        identity: Option<(&str, &str)>,
        port: u16,
    ) -> Result<ClusterClient, String> {
        ClusterClient::pinned(Pin::Ca(ca_fingerprint.to_ascii_lowercase()), identity, port)
    }

    /// A client for a peer appliance's 8445, which must present the
    /// certificate with `fingerprint`.
    pub fn peer(
        fingerprint: &str,
        identity: (&str, &str),
        port: u16,
    ) -> Result<ClusterClient, String> {
        let pin = bmac_proto::appliance::normalized_fingerprint(fingerprint)
            .ok_or("a fingerprint is 64 hexadecimal digits")?;
        ClusterClient::pinned(Pin::Leaf(pin), Some(identity), port)
    }

    fn pinned(
        pin: Pin,
        identity: Option<(&str, &str)>,
        port: u16,
    ) -> Result<ClusterClient, String> {
        let provider = provider();
        let verifier = Arc::new(Pinned {
            pin,
            provider: provider.clone(),
            seen: std::sync::Mutex::default(),
        });
        let builder = ClientConfig::builder_with_provider(provider)
            .with_protocol_versions(&[&rustls::version::TLS13])
            .map_err(|e| e.to_string())?
            .dangerous()
            .with_custom_certificate_verifier(verifier.clone());
        let mut tls = match identity {
            None => builder.with_no_client_auth(),
            Some((cert, key)) => {
                let chain = CertificateDer::pem_slice_iter(cert.as_bytes())
                    .collect::<Result<Vec<_>, _>>()
                    .map_err(|e| format!("appliance certificate: {e}"))?;
                let key = PrivateKeyDer::from_pem_slice(key.as_bytes())
                    .map_err(|e| format!("appliance key: {e}"))?;
                builder
                    .with_client_auth_cert(chain, key)
                    .map_err(|e| e.to_string())?
            }
        };
        tls.alpn_protocols = vec![b"http/1.1".to_vec()];
        Ok(ClusterClient {
            tls: Arc::new(tls),
            verifier,
            port,
        })
    }

    /// The fingerprint the last server presented, matched or not.
    pub fn presented(&self) -> Option<String> {
        self.verifier
            .seen
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
    }

    async fn connect(
        &self,
        name: &str,
        address: &str,
    ) -> Result<hyper::client::conn::http1::SendRequest<Full<Bytes>>, String> {
        let parsed: ApplianceAddress = address
            .parse()
            .map_err(|e| format!("{address} is not an address: {e}"))?;
        let port = parsed.port_or(self.port);
        let attempt = async {
            let tcp = TcpStream::connect((parsed.host_text().as_str(), port))
                .await
                .map_err(|e| e.to_string())?;
            let _ = tcp.set_nodelay(true);
            let name = ServerName::try_from(name.to_string()).map_err(|e| e.to_string())?;
            let tls = tokio_rustls::TlsConnector::from(self.tls.clone())
                .connect(name, tcp)
                .await
                .map_err(|e| e.to_string())?;
            let (sender, connection) = hyper::client::conn::http1::handshake(TokioIo::new(tls))
                .await
                .map_err(|e| e.to_string())?;
            tokio::spawn(async move {
                let _ = connection.await;
            });
            Ok::<_, String>(sender)
        };
        match tokio::time::timeout(CONNECT_TIMEOUT, attempt).await {
            Ok(Ok(sender)) => Ok(sender),
            Ok(Err(e)) => Err(format!("{address}: {e}")),
            Err(_) => Err(format!("{address}: timed out")),
        }
    }

    /// Sends one request to the first of the host's addresses that answers.
    async fn request(
        &self,
        host: &SourceHost,
        method: &str,
        path: &str,
        body: Option<Vec<u8>>,
    ) -> Result<hyper::Response<Incoming>, CallError> {
        self.request_to(&host.host.to_string(), &host.addresses, method, path, body)
            .await
    }

    /// `name` is the TLS server name and the `Host` header.
    async fn request_to(
        &self,
        name: &str,
        addresses: &[String],
        method: &str,
        path: &str,
        body: Option<Vec<u8>>,
    ) -> Result<hyper::Response<Incoming>, CallError> {
        let mut last = format!("{name} has no address");
        for address in addresses {
            let mut sender = match self.connect(name, address).await {
                Ok(sender) => sender,
                Err(e) => {
                    last = e;
                    continue;
                }
            };
            let mut request = hyper::Request::builder()
                .method(method)
                .uri(path)
                .header(hyper::header::HOST, name);
            if body.is_some() {
                request = request.header(hyper::header::CONTENT_TYPE, "application/json");
            }
            let request = request
                .body(Full::new(Bytes::from(body.clone().unwrap_or_default())))
                .map_err(|e| CallError::Unreachable(e.to_string()))?;
            match tokio::time::timeout(CALL_TIMEOUT, sender.send_request(request)).await {
                Ok(Ok(response)) => return Ok(response),
                Ok(Err(e)) => last = format!("{address}: {e}"),
                Err(_) => last = format!("{address}: no answer"),
            }
        }
        Err(CallError::Unreachable(last))
    }

    pub async fn call<Q: Serialize, A: DeserializeOwned>(
        &self,
        host: &SourceHost,
        method: &str,
        path: &str,
        body: Option<&Q>,
    ) -> Result<A, CallError> {
        let body = body.map(|b| serde_json::to_vec(b).expect("requests serialize"));
        let response = self.request(host, method, path, body).await?;
        answer(&host.host.to_string(), response).await
    }

    /// A call to a peer appliance at `address`.
    pub async fn call_peer<Q: Serialize, A: DeserializeOwned>(
        &self,
        address: &ApplianceAddress,
        method: &str,
        path: &str,
        body: Option<&Q>,
    ) -> Result<A, CallError> {
        let body = body.map(|b| serde_json::to_vec(b).expect("requests serialize"));
        let name = address.host_text();
        let response = self
            .request_to(&name, &[address.to_string()], method, path, body)
            .await?;
        answer(&name, response).await
    }

    /// An export from a peer appliance at `address`.
    pub async fn stream_peer<Q: Serialize>(
        &self,
        address: &ApplianceAddress,
        path: &str,
        body: &Q,
    ) -> Result<Stream, CallError> {
        let body = serde_json::to_vec(body).expect("requests serialize");
        let response = self
            .request_to(
                &address.host_text(),
                &[address.to_string()],
                "POST",
                path,
                Some(body),
            )
            .await?;
        chunks(response).await
    }

    /// A send: the response body as a stream of chunks.
    pub async fn stream<Q: Serialize>(
        &self,
        host: &SourceHost,
        path: &str,
        body: &Q,
    ) -> Result<Stream, CallError> {
        let body = serde_json::to_vec(body).expect("requests serialize");
        let response = self.request(host, "POST", path, Some(body)).await?;
        chunks(response).await
    }
}

async fn answer<A: DeserializeOwned>(
    name: &str,
    response: hyper::Response<Incoming>,
) -> Result<A, CallError> {
    let status = response.status();
    let bytes = tokio::time::timeout(CALL_TIMEOUT, response.into_body().collect())
        .await
        .map_err(|_| CallError::Unreachable(format!("{name}: the answer stalled")))?
        .map_err(|e| CallError::Unreachable(e.to_string()))?
        .to_bytes();
    if status.is_success() {
        serde_json::from_slice(&bytes)
            .map_err(|e| CallError::Unreachable(format!("{name} answered oddly: {e}")))
    } else {
        Err(refusal(status, &bytes))
    }
}

/// The response body as a stream of chunks.
async fn chunks(response: hyper::Response<Incoming>) -> Result<Stream, CallError> {
    let status = response.status();
    if !status.is_success() {
        let bytes = response
            .into_body()
            .collect()
            .await
            .map(|c| c.to_bytes())
            .unwrap_or_default();
        return Err(refusal(status, &bytes));
    }
    let (tx, rx) = mpsc::channel(8);
    let mut body = response.into_body();
    tokio::spawn(async move {
        loop {
            match tokio::time::timeout(STREAM_IDLE, body.frame()).await {
                Err(_) => {
                    let _ = tx.send(Err("the stream stalled".into())).await;
                    return;
                }
                Ok(None) => return,
                Ok(Some(Err(e))) => {
                    let _ = tx.send(Err(format!("the stream broke: {e}"))).await;
                    return;
                }
                Ok(Some(Ok(frame))) => {
                    if let Ok(data) = frame.into_data()
                        && !data.is_empty()
                        && tx.send(Ok(data)).await.is_err()
                    {
                        return;
                    }
                }
            }
        }
    });
    Ok(rx)
}

fn refusal(status: hyper::StatusCode, bytes: &[u8]) -> CallError {
    let error = serde_json::from_slice::<ApiError>(bytes)
        .unwrap_or_else(|_| ApiError::new(ErrorCode::Internal, format!("answered {status}")));
    match error.code {
        ErrorCode::Unavailable | ErrorCode::Internal | ErrorCode::RateLimited => {
            CallError::Unreachable(error.message)
        }
        _ => CallError::Refused(error),
    }
}
