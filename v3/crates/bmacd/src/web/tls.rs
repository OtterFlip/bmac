// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

//! The Dashboard's TLS settings: TLS 1.3 and 1.2, ECDHE key exchange, and
//! AEAD ciphers only.

use std::sync::Arc;

use rustls::crypto::ring::{cipher_suite, default_provider, kx_group};
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use rustls::{ServerConfig, SupportedCipherSuite};
use sha2::{Digest, Sha256};

#[derive(Debug, thiserror::Error)]
pub enum TlsError {
    #[error("the certificate file has no certificate")]
    NoCertificate,
    #[error("cannot read the certificate: {0}")]
    Certificate(String),
    #[error("cannot read the private key: {0}")]
    Key(String),
    #[error(transparent)]
    Rustls(#[from] rustls::Error),
}

/// The suites offered, strongest first.
pub const CIPHER_SUITES: [SupportedCipherSuite; 9] = [
    cipher_suite::TLS13_AES_256_GCM_SHA384,
    cipher_suite::TLS13_CHACHA20_POLY1305_SHA256,
    cipher_suite::TLS13_AES_128_GCM_SHA256,
    cipher_suite::TLS_ECDHE_ECDSA_WITH_AES_256_GCM_SHA384,
    cipher_suite::TLS_ECDHE_ECDSA_WITH_CHACHA20_POLY1305_SHA256,
    cipher_suite::TLS_ECDHE_ECDSA_WITH_AES_128_GCM_SHA256,
    cipher_suite::TLS_ECDHE_RSA_WITH_AES_256_GCM_SHA384,
    cipher_suite::TLS_ECDHE_RSA_WITH_CHACHA20_POLY1305_SHA256,
    cipher_suite::TLS_ECDHE_RSA_WITH_AES_128_GCM_SHA256,
];

pub fn server_config(cert_pem: &[u8], key_pem: &[u8]) -> Result<Arc<ServerConfig>, TlsError> {
    let certs: Vec<CertificateDer<'static>> = CertificateDer::pem_slice_iter(cert_pem)
        .collect::<Result<_, _>>()
        .map_err(|e| TlsError::Certificate(e.to_string()))?;
    if certs.is_empty() {
        return Err(TlsError::NoCertificate);
    }
    let key = PrivateKeyDer::from_pem_slice(key_pem).map_err(|e| TlsError::Key(e.to_string()))?;
    let mut provider = default_provider();
    provider.cipher_suites = CIPHER_SUITES.to_vec();
    provider.kx_groups = vec![kx_group::X25519, kx_group::SECP256R1, kx_group::SECP384R1];
    let mut config = ServerConfig::builder_with_provider(Arc::new(provider))
        .with_protocol_versions(&[&rustls::version::TLS13, &rustls::version::TLS12])?
        .with_no_client_auth()
        .with_single_cert(certs, key)?;
    config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
    Ok(Arc::new(config))
}

/// The SHA-256 fingerprint of a certificate's DER encoding, as browsers
/// show it: uppercase hex pairs joined by colons.
pub fn fingerprint(der: &[u8]) -> String {
    Sha256::digest(der)
        .iter()
        .map(|b| format!("{b:02X}"))
        .collect::<Vec<_>>()
        .join(":")
}

/// The fingerprint of the first certificate in a PEM file.
pub fn pem_fingerprint(cert_pem: &[u8]) -> Result<String, TlsError> {
    let first = CertificateDer::pem_slice_iter(cert_pem)
        .next()
        .ok_or(TlsError::NoCertificate)?
        .map_err(|e| TlsError::Certificate(e.to_string()))?;
    Ok(fingerprint(&first))
}
