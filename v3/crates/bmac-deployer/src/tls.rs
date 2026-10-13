// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

//! TLS to a server with a self-signed certificate, trusted by its SHA-256
//! fingerprint: a new host's Dashboard at handoff. The handshake's
//! signature is still checked, so only the holder of the certificate's key
//! passes.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::CryptoProvider;
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{ClientConfig, DigitallySignedStruct, SignatureScheme};
use sha2::{Digest, Sha256};
use tokio::net::TcpStream;
use tokio_rustls::TlsConnector;
use tokio_rustls::client::TlsStream;

/// Uppercase hex pairs joined by colons, as browsers show it.
pub fn fingerprint(der: &[u8]) -> String {
    Sha256::digest(der)
        .iter()
        .map(|b| format!("{b:02X}"))
        .collect::<Vec<_>>()
        .join(":")
}

/// Fingerprints compare without regard to case or separators.
pub fn same_fingerprint(a: &str, b: &str) -> bool {
    let clean = |s: &str| {
        s.chars()
            .filter(char::is_ascii_hexdigit)
            .map(|c| c.to_ascii_uppercase())
            .collect::<String>()
    };
    let (a, b) = (clean(a), clean(b));
    a.len() == 64 && a == b
}

/// Accepts exactly the certificate with `expected`'s fingerprint, or, with
/// no expectation, any certificate, remembering what it saw.
#[derive(Debug)]
pub struct FingerprintVerifier {
    expected: Option<String>,
    seen: Mutex<Option<String>>,
    provider: Arc<CryptoProvider>,
}

impl FingerprintVerifier {
    pub fn new(expected: Option<String>) -> FingerprintVerifier {
        FingerprintVerifier {
            expected,
            seen: Mutex::new(None),
            provider: Arc::new(rustls::crypto::ring::default_provider()),
        }
    }

    /// The fingerprint of the last certificate presented.
    pub fn seen(&self) -> Option<String> {
        self.seen.lock().ok().and_then(|s| s.clone())
    }

    pub fn client_config(self: &Arc<Self>) -> ClientConfig {
        ClientConfig::builder_with_provider(self.provider.clone())
            .with_safe_default_protocol_versions()
            .expect("the ring provider supports the default versions")
            .dangerous()
            .with_custom_certificate_verifier(self.clone())
            .with_no_client_auth()
    }
}

impl ServerCertVerifier for FingerprintVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        let seen = fingerprint(end_entity);
        if let Ok(mut slot) = self.seen.lock() {
            *slot = Some(seen.clone());
        }
        match &self.expected {
            Some(expected) if !same_fingerprint(expected, &seen) => Err(rustls::Error::General(
                format!("the certificate's fingerprint is {seen}, not the pinned {expected}"),
            )),
            _ => Ok(ServerCertVerified::assertion()),
        }
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

/// Why a TLS check didn't give a fingerprint.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ProbeError {
    /// Nothing answered: refused, unreachable, or timed out. The server may
    /// not be up yet.
    #[error("no answer from {0}")]
    NoAnswer(String),
    /// Something answered but the TLS handshake failed.
    #[error("TLS with {target} failed: {message}")]
    Handshake { target: String, message: String },
}

fn server_name(host: &str) -> Result<ServerName<'static>, String> {
    ServerName::try_from(host.to_string()).map_err(|e| e.to_string())
}

/// Connects to `host:port` over TLS and returns the stream if the
/// certificate has the fingerprint `expected` (or any certificate when
/// `None`), with the fingerprint seen.
pub async fn connect(
    host: &str,
    port: u16,
    expected: Option<&str>,
    timeout: Duration,
) -> Result<(TlsStream<TcpStream>, String), ProbeError> {
    let target = format!("{host}:{port}");
    let tcp = match tokio::time::timeout(timeout, TcpStream::connect((host, port))).await {
        Ok(Ok(tcp)) => tcp,
        Ok(Err(_)) | Err(_) => return Err(ProbeError::NoAnswer(target)),
    };
    let verifier = Arc::new(FingerprintVerifier::new(expected.map(str::to_string)));
    let connector = TlsConnector::from(Arc::new(verifier.client_config()));
    let name = server_name(host).map_err(|message| ProbeError::Handshake {
        target: target.clone(),
        message,
    })?;
    match tokio::time::timeout(timeout, connector.connect(name, tcp)).await {
        Ok(Ok(stream)) => {
            let seen = verifier.seen().unwrap_or_default();
            Ok((stream, seen))
        }
        Ok(Err(e)) => match verifier.seen() {
            Some(seen) if expected.is_some_and(|x| !same_fingerprint(x, &seen)) => {
                Err(ProbeError::Handshake {
                    target,
                    message: format!("certificate fingerprint {seen}"),
                })
            }
            _ => Err(ProbeError::Handshake {
                target,
                message: e.to_string(),
            }),
        },
        Err(_) => Err(ProbeError::NoAnswer(target)),
    }
}

/// The fingerprint of the certificate `host:port` presents. The connection
/// is closed after the handshake without sending a request.
pub async fn probe(host: &str, port: u16, timeout: Duration) -> Result<String, ProbeError> {
    let (mut stream, seen) = connect(host, port, None, timeout).await?;
    let _ = tokio::io::AsyncWriteExt::shutdown(&mut stream).await;
    Ok(seen)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fingerprints_compare_loosely_but_completely() {
        let a = fingerprint(b"cert");
        assert!(same_fingerprint(&a, &a.to_lowercase()));
        assert!(same_fingerprint(&a, &a.replace(':', "")));
        assert!(!same_fingerprint(&a, &fingerprint(b"other")));
        assert!(!same_fingerprint("", ""));
        assert!(!same_fingerprint("AB", "AB"));
    }
}
