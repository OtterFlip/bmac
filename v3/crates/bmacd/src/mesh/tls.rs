// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

//! TLS on port 8444 (`SECURITY.md`, "Host to host"): TLS 1.3 only, with the
//! Proxmox node certificate on both sides, verified against the cluster CA.
//! A client without a certificate is let through the handshake, for joins
//! and enrollments only; the router refuses it everything else.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use bmac_core::HostSlot;
use rustls::client::danger::HandshakeSignatureValid;
use rustls::crypto::CryptoProvider;
use rustls::crypto::ring::{cipher_suite, default_provider, kx_group};
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName};
use rustls::pki_types::{TrustAnchor, UnixTime};
use rustls::server::danger::{ClientCertVerified, ClientCertVerifier};
use rustls::{
    ClientConfig, DigitallySignedStruct, DistinguishedName, RootCertStore, ServerConfig,
    SignatureScheme,
};

use crate::web::tls::TlsError;

pub const CLUSTER_CA: &str = "/etc/pve/pve-root-ca.pem";

/// A node's Proxmox certificate and key.
pub fn node_cert_paths(node: &str) -> (PathBuf, PathBuf) {
    let dir = Path::new("/etc/pve/nodes").join(node);
    (dir.join("pve-ssl.pem"), dir.join("pve-ssl.key"))
}

pub(crate) fn provider() -> Arc<CryptoProvider> {
    let mut provider = default_provider();
    provider.cipher_suites = vec![
        cipher_suite::TLS13_AES_256_GCM_SHA384,
        cipher_suite::TLS13_CHACHA20_POLY1305_SHA256,
        cipher_suite::TLS13_AES_128_GCM_SHA256,
    ];
    provider.kx_groups = vec![kx_group::X25519, kx_group::SECP256R1, kx_group::SECP384R1];
    Arc::new(provider)
}

pub(crate) fn certificates(pem: &[u8]) -> Result<Vec<CertificateDer<'static>>, TlsError> {
    let certs: Vec<CertificateDer<'static>> = CertificateDer::pem_slice_iter(pem)
        .collect::<Result<_, _>>()
        .map_err(|e| TlsError::Certificate(e.to_string()))?;
    if certs.is_empty() {
        return Err(TlsError::NoCertificate);
    }
    Ok(certs)
}

fn roots(ca_pem: &[u8]) -> Result<Arc<RootCertStore>, TlsError> {
    let mut roots = RootCertStore::empty();
    for cert in certificates(ca_pem)? {
        roots
            .add(cert)
            .map_err(|e| TlsError::Certificate(format!("cluster CA: {e}")))?;
    }
    Ok(Arc::new(roots))
}

/// Verifies a client's Proxmox node certificate against the cluster CA.
/// Proxmox issues node certificates for server use, so this accepts the
/// server-auth key usage where the stock verifier would want client-auth.
/// Which member the certificate names is checked per request.
#[derive(Debug)]
struct NodeCertVerifier {
    anchors: Vec<TrustAnchor<'static>>,
    provider: Arc<CryptoProvider>,
}

impl ClientCertVerifier for NodeCertVerifier {
    fn offer_client_auth(&self) -> bool {
        true
    }

    fn client_auth_mandatory(&self) -> bool {
        false
    }

    fn root_hint_subjects(&self) -> &[DistinguishedName] {
        &[]
    }

    fn verify_client_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        now: UnixTime,
    ) -> Result<ClientCertVerified, rustls::Error> {
        let cert = webpki::EndEntityCert::try_from(end_entity)
            .map_err(|e| rustls::Error::General(format!("client certificate: {e}")))?;
        cert.verify_for_usage(
            self.provider.signature_verification_algorithms.all,
            &self.anchors,
            intermediates,
            now,
            webpki::KeyUsage::required_if_present(SERVER_AUTH),
            None,
            None,
        )
        .map_err(|e| rustls::Error::General(format!("client certificate: {e}")))?;
        Ok(ClientCertVerified::assertion())
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

/// id-kp-serverAuth, DER-encoded without its tag.
const SERVER_AUTH: &[u8] = &[0x2b, 0x06, 0x01, 0x05, 0x05, 0x07, 0x03, 0x01];

/// The server and client settings for talking to peers.
#[derive(Debug, Clone)]
pub struct PeerTls {
    pub server: Arc<ServerConfig>,
    pub client: Arc<ClientConfig>,
}

impl PeerTls {
    pub fn new(ca_pem: &[u8], cert_pem: &[u8], key_pem: &[u8]) -> Result<PeerTls, TlsError> {
        let roots = roots(ca_pem)?;
        let certs = certificates(cert_pem)?;
        let key =
            PrivateKeyDer::from_pem_slice(key_pem).map_err(|e| TlsError::Key(e.to_string()))?;
        let anchors = certificates(ca_pem)?
            .iter()
            .map(|der| {
                webpki::anchor_from_trusted_cert(der)
                    .map(|a| a.to_owned())
                    .map_err(|e| TlsError::Certificate(format!("cluster CA: {e}")))
            })
            .collect::<Result<Vec<_>, _>>()?;
        let verifier = Arc::new(NodeCertVerifier {
            anchors,
            provider: provider(),
        });
        // The CA follows the node certificate, so a joining host that knows
        // only the CA's fingerprint can still verify the chain.
        let mut chain = certs.clone();
        chain.extend(certificates(ca_pem)?);
        let mut server = ServerConfig::builder_with_provider(provider())
            .with_protocol_versions(&[&rustls::version::TLS13])?
            .with_client_cert_verifier(verifier)
            .with_single_cert(chain, key.clone_key())?;
        server.alpn_protocols = vec![b"http/1.1".to_vec()];
        let mut client = ClientConfig::builder_with_provider(provider())
            .with_protocol_versions(&[&rustls::version::TLS13])?
            .with_root_certificates(roots)
            .with_client_auth_cert(certs, key)?;
        client.alpn_protocols = vec![b"http/1.1".to_vec()];
        Ok(PeerTls {
            server: Arc::new(server),
            client: Arc::new(client),
        })
    }

    /// Reads the cluster CA and this node's certificate from pmxcfs.
    pub fn system(node: &str) -> Result<PeerTls, String> {
        let (cert, key) = node_cert_paths(node);
        let read = |path: &Path| {
            std::fs::read(path).map_err(|e| format!("cannot read {}: {e}", path.display()))
        };
        PeerTls::new(&read(Path::new(CLUSTER_CA))?, &read(&cert)?, &read(&key)?)
            .map_err(|e| e.to_string())
    }
}

/// Settings for a client with no node certificate yet, as a joining host
/// uses: it checks the server against the cluster CA and presents none.
pub fn join_client(ca_pem: &[u8]) -> Result<Arc<ClientConfig>, TlsError> {
    let mut client = ClientConfig::builder_with_provider(provider())
        .with_protocol_versions(&[&rustls::version::TLS13])?
        .with_root_certificates(roots(ca_pem)?)
        .with_no_client_auth();
    client.alpn_protocols = vec![b"http/1.1".to_vec()];
    Ok(Arc::new(client))
}

/// Checks a server's chain against the one CA certificate in it whose
/// SHA-256 is the pinned fingerprint from a join bundle.
#[derive(Debug)]
struct PinnedCa {
    fingerprint: String,
    provider: Arc<CryptoProvider>,
}

impl rustls::client::danger::ServerCertVerifier for PinnedCa {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        server_name: &ServerName<'_>,
        ocsp_response: &[u8],
        now: UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        let ca = intermediates
            .iter()
            .find(|der| crate::web::tls::fingerprint(der).eq_ignore_ascii_case(&self.fingerprint))
            .ok_or_else(|| {
                rustls::Error::General(
                    "the server's chain has no certificate with the cluster CA's fingerprint"
                        .into(),
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
        .verify_server_cert(end_entity, &[], server_name, ocsp_response, now)
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

/// Settings for a joining host that has only the cluster CA's fingerprint
/// from its bundle (`SECURITY.md`, "Joins and enrollment").
pub fn join_client_pinned(ca_fingerprint: &str) -> Result<Arc<ClientConfig>, TlsError> {
    let provider = provider();
    let mut client = ClientConfig::builder_with_provider(provider.clone())
        .with_protocol_versions(&[&rustls::version::TLS13])?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(PinnedCa {
            fingerprint: ca_fingerprint.to_string(),
            provider,
        }))
        .with_no_client_auth();
    client.alpn_protocols = vec![b"http/1.1".to_vec()];
    Ok(Arc::new(client))
}

/// The name a peer's certificate must be valid for.
pub fn server_name(host: HostSlot) -> ServerName<'static> {
    ServerName::try_from(host.to_string()).expect("host names are valid DNS names")
}

/// The one member a verified certificate is valid for, if exactly one.
pub fn identify(cert: &CertificateDer<'_>, members: &BTreeSet<HostSlot>) -> Option<HostSlot> {
    let cert = webpki::EndEntityCert::try_from(cert).ok()?;
    let mut named = members
        .iter()
        .filter(|host| {
            cert.verify_is_valid_for_subject_name(&server_name(**host))
                .is_ok()
        })
        .copied();
    let first = named.next()?;
    named.next().is_none().then_some(first)
}
