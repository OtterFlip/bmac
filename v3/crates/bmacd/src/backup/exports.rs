// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

//! A restore job's connection to an appliance's port 8445 (`BACKUP.md`,
//! "Restore"): this host's Proxmox certificate as the client's, the
//! appliance's own certificate pinned by the fingerprint approved at
//! enrollment, and the restore grant in every request. This is the only
//! connection the cluster opens to an appliance.

use std::sync::Arc;
use std::time::Duration;

use bmac_backup::records::ApplianceRecord;
use bmac_core::ApplianceAddress;
use bmac_proto::api::ApiError;
use bmac_proto::appliance::{
    ExportEstimate, ExportRequest, GrantedIndex, GrantedRequest, RecoveryBind, RecoveryIndex,
    RecoveryRequest, normalized_fingerprint, on_appliance,
};
use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use hyper_util::rt::TokioIo;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::CryptoProvider;
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName, UnixTime};
use rustls::{ClientConfig, DigitallySignedStruct, SignatureScheme};
use serde::Serialize;
use serde::de::DeserializeOwned;
use sha2::{Digest, Sha256};
use tokio::io::{AsyncWrite, AsyncWriteExt};
use tokio::net::TcpStream;

const HOST_CERT: &str = "/etc/pve/local/pve-ssl.pem";
const HOST_KEY: &str = "/etc/pve/local/pve-ssl.key";
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
const CALL_TIMEOUT: Duration = Duration::from_secs(120);
/// A stream's next chunk may take this long, while ZFS reads a big block.
const STREAM_IDLE: Duration = Duration::from_secs(300);

fn fingerprint(der: &[u8]) -> String {
    Sha256::digest(der)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// The host's certificate, then the CA unless the file already holds it.
fn chain_with_ca(cert: &str, ca: &str) -> String {
    if cert.contains(ca.trim()) {
        return cert.to_string();
    }
    format!("{}\n{}\n", cert.trim_end(), ca.trim())
}

/// Trusts only the appliance certificate with this fingerprint.
#[derive(Debug)]
struct Pinned {
    fingerprint: String,
    provider: Arc<CryptoProvider>,
}

impl ServerCertVerifier for Pinned {
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
                "the appliance presented another certificate than the one approved at enrollment"
                    .into(),
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

/// One approved appliance, at its address.
#[derive(Clone)]
pub struct ApplianceClient {
    tls: Arc<ClientConfig>,
    address: ApplianceAddress,
    name: String,
}

impl std::fmt::Debug for ApplianceClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ApplianceClient")
            .field("name", &self.name)
            .field("address", &self.address)
            .finish_non_exhaustive()
    }
}

impl ApplianceClient {
    /// With this host's certificate and key, as root reads them. The
    /// appliance knows the cluster only by its CA's fingerprint, so the CA
    /// goes in the chain after the host's certificate.
    pub fn for_host(appliance: &ApplianceRecord) -> Result<ApplianceClient, String> {
        let cert = std::fs::read_to_string(HOST_CERT).map_err(|e| format!("{HOST_CERT}: {e}"))?;
        let ca = std::fs::read_to_string(crate::joins::CLUSTER_CA)
            .map_err(|e| format!("{}: {e}", crate::joins::CLUSTER_CA))?;
        let key = std::fs::read_to_string(HOST_KEY).map_err(|e| format!("{HOST_KEY}: {e}"))?;
        ApplianceClient::new(appliance, &chain_with_ca(&cert, &ca), &key)
    }

    pub fn new(
        appliance: &ApplianceRecord,
        cert: &str,
        key: &str,
    ) -> Result<ApplianceClient, String> {
        let address = appliance.address.clone().ok_or_else(|| {
            format!(
                "{} has no address; set one in its settings so a restore can reach it",
                appliance.name
            )
        })?;
        let pin = normalized_fingerprint(&appliance.fingerprint)
            .ok_or_else(|| format!("{}'s recorded fingerprint isn't valid", appliance.name))?;
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let verifier = Arc::new(Pinned {
            fingerprint: pin,
            provider: provider.clone(),
        });
        let chain = CertificateDer::pem_slice_iter(cert.as_bytes())
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| format!("host certificate: {e}"))?;
        let key =
            PrivateKeyDer::from_pem_slice(key.as_bytes()).map_err(|e| format!("host key: {e}"))?;
        let mut tls = ClientConfig::builder_with_provider(provider)
            .with_protocol_versions(&[&rustls::version::TLS13])
            .map_err(|e| e.to_string())?
            .dangerous()
            .with_custom_certificate_verifier(verifier)
            .with_client_auth_cert(chain, key)
            .map_err(|e| e.to_string())?;
        tls.alpn_protocols = vec![b"http/1.1".to_vec()];
        Ok(ApplianceClient {
            tls: Arc::new(tls),
            address,
            name: appliance.name.clone(),
        })
    }

    async fn request(
        &self,
        path: &str,
        body: Vec<u8>,
    ) -> Result<hyper::Response<Incoming>, String> {
        let connect = async {
            let sockets = super::daemon::resolve(&self.address).await?;
            let tcp = TcpStream::connect(sockets.as_slice())
                .await
                .map_err(|e| e.to_string())?;
            let _ = tcp.set_nodelay(true);
            // The certificate is pinned, so the name only has to be valid.
            let name = ServerName::try_from(self.name.clone())
                .or_else(|_| ServerName::try_from("appliance".to_string()))
                .map_err(|e| e.to_string())?;
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
        let mut sender = tokio::time::timeout(CONNECT_TIMEOUT, connect)
            .await
            .map_err(|_| format!("{}: {} timed out", self.name, self.address))?
            .map_err(|e| format!("{}: {}: {e}", self.name, self.address))?;
        let request = hyper::Request::builder()
            .method("POST")
            .uri(path)
            .header(hyper::header::HOST, self.address.host_text())
            .header(hyper::header::CONTENT_TYPE, "application/json")
            .body(Full::new(Bytes::from(body)))
            .map_err(|e| e.to_string())?;
        tokio::time::timeout(CALL_TIMEOUT, sender.send_request(request))
            .await
            .map_err(|_| format!("{} didn't answer", self.name))?
            .map_err(|e| format!("{}: {e}", self.name))
    }

    async fn call<Q: Serialize, A: DeserializeOwned>(
        &self,
        path: &str,
        body: &Q,
    ) -> Result<A, String> {
        let response = self
            .request(path, serde_json::to_vec(body).expect("requests serialize"))
            .await?;
        let status = response.status();
        let bytes = tokio::time::timeout(CALL_TIMEOUT, response.into_body().collect())
            .await
            .map_err(|_| format!("{}: the answer stalled", self.name))?
            .map_err(|e| e.to_string())?
            .to_bytes();
        if !status.is_success() {
            return Err(refusal(&self.name, status, &bytes));
        }
        serde_json::from_slice(&bytes).map_err(|e| format!("{} answered oddly: {e}", self.name))
    }

    /// What `grant` lets this cluster read.
    pub async fn granted(&self, cluster_uuid: &str, grant: &str) -> Result<GrantedIndex, String> {
        self.call(
            on_appliance::GRANTED,
            &GrantedRequest {
                cluster_uuid: cluster_uuid.to_string(),
                grant: grant.to_string(),
            },
        )
        .await
    }

    /// Under a recovery grant: the lost cluster's recovery manifest and
    /// every PIT of it.
    pub async fn recovery_index(&self, grant: &str) -> Result<RecoveryIndex, String> {
        self.call(
            on_appliance::RECOVERY_INDEX,
            &RecoveryRequest {
                grant: grant.to_string(),
            },
        )
        .await
    }

    /// Under a recovery grant: binds the lost cluster's namespace to this
    /// cluster.
    pub async fn recovery_bind(&self, bind: &RecoveryBind) -> Result<(), String> {
        let _: serde_json::Value = self.call(on_appliance::RECOVERY_BIND, bind).await?;
        Ok(())
    }

    /// The size of the stream `request` would send.
    pub async fn estimate(&self, request: &ExportRequest) -> Result<u64, String> {
        let request = ExportRequest {
            estimate: true,
            ..request.clone()
        };
        let answer: ExportEstimate = self.call(on_appliance::EXPORT, &request).await?;
        Ok(answer.bytes)
    }

    /// Streams an export into `sink`; the bytes written. `progress` hears
    /// the running total.
    pub async fn export_into(
        &self,
        request: &ExportRequest,
        sink: &mut (dyn AsyncWrite + Unpin + Send),
        progress: &(dyn Fn(u64) + Send + Sync),
    ) -> Result<u64, String> {
        let response = self
            .request(
                on_appliance::EXPORT,
                serde_json::to_vec(request).expect("requests serialize"),
            )
            .await?;
        let status = response.status();
        let mut body = response.into_body();
        if !status.is_success() {
            let bytes = body
                .collect()
                .await
                .map(|c| c.to_bytes())
                .unwrap_or_default();
            return Err(refusal(&self.name, status, &bytes));
        }
        let mut total = 0u64;
        loop {
            let frame = match tokio::time::timeout(STREAM_IDLE, body.frame()).await {
                Err(_) => return Err(format!("{}: the stream stalled", self.name)),
                Ok(None) => break,
                Ok(Some(Err(e))) => return Err(format!("{}: the stream broke: {e}", self.name)),
                Ok(Some(Ok(frame))) => frame,
            };
            if let Ok(data) = frame.into_data() {
                sink.write_all(&data)
                    .await
                    .map_err(|e| format!("zfs receive stopped reading: {e}"))?;
                total += data.len() as u64;
                progress(total);
            }
        }
        sink.flush().await.map_err(|e| e.to_string())?;
        Ok(total)
    }
}

fn refusal(name: &str, status: hyper::StatusCode, bytes: &[u8]) -> String {
    match serde_json::from_slice::<ApiError>(bytes) {
        Ok(error) => format!("{name} refused: {}", error.message),
        Err(_) => format!("{name} answered {status}"),
    }
}

#[cfg(test)]
mod tests {
    use super::chain_with_ca;

    #[test]
    fn the_cluster_ca_follows_the_host_certificate_once() {
        let host = "-----BEGIN CERTIFICATE-----\nHOST\n-----END CERTIFICATE-----\n";
        let ca = "-----BEGIN CERTIFICATE-----\nCA\n-----END CERTIFICATE-----\n";
        let chain = chain_with_ca(host, ca);
        assert!(chain.starts_with(host.trim_end()));
        assert_eq!(chain.matches("BEGIN CERTIFICATE").count(), 2);
        assert_eq!(chain_with_ca(&chain, ca), chain);
    }
}
