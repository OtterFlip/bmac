// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

//! A TLS client check that accepts only certificates whose exact bytes are
//! already known, for servers with self-signed certificates.

use std::sync::Arc;

use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::CryptoProvider;
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{ClientConfig, DigitallySignedStruct, SignatureScheme};

#[derive(Debug)]
pub struct Pinned {
    certificates: Vec<Vec<u8>>,
    provider: Arc<CryptoProvider>,
}

impl Pinned {
    /// Accepts a server whose certificate is one of `certificates` (DER).
    pub fn new(certificates: Vec<Vec<u8>>) -> Pinned {
        Pinned {
            certificates,
            provider: Arc::new(rustls::crypto::ring::default_provider()),
        }
    }

    /// The first certificate of each PEM file; files that are missing or
    /// hold no certificate are skipped.
    pub fn from_pem_files(paths: &[impl AsRef<std::path::Path>]) -> Pinned {
        let certificates = paths
            .iter()
            .filter_map(|path| std::fs::read(path).ok())
            .filter_map(|pem| CertificateDer::pem_slice_iter(&pem).next()?.ok())
            .map(|der| der.to_vec())
            .collect();
        Pinned::new(certificates)
    }

    pub fn is_empty(&self) -> bool {
        self.certificates.is_empty()
    }

    /// A client configuration trusting only these certificates.
    pub fn client_config(
        self,
        versions: &[&'static rustls::SupportedProtocolVersion],
    ) -> Result<ClientConfig, rustls::Error> {
        let provider = self.provider.clone();
        Ok(ClientConfig::builder_with_provider(provider)
            .with_protocol_versions(versions)?
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(self))
            .with_no_client_auth())
    }
}

impl ServerCertVerifier for Pinned {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _name: &ServerName<'_>,
        _ocsp: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        if self
            .certificates
            .iter()
            .any(|c| c.as_slice() == end_entity.as_ref())
        {
            Ok(ServerCertVerified::assertion())
        } else {
            Err(rustls::Error::General("not a pinned certificate".into()))
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
