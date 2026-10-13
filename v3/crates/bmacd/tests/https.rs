// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

//! The HTTPS front end end to end: TLS settings, listeners, and the headers
//! on every response. The client pins the certificate, as the Deployer does.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use bmacd::web::app::{self, MAX_BODY, SECURITY_HEADERS, STYLE_NONCE_PLACEHOLDER};
use bmacd::web::assets::Assets;
use bmacd::web::https::{HttpsServer, Listeners};
use bmacd::web::tls::{self, CIPHER_SUITES};
use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::{Request, Response};
use hyper_util::rt::TokioIo;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::CryptoProvider;
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{
    ClientConfig, DigitallySignedStruct, ProtocolVersion, SignatureScheme, SupportedProtocolVersion,
};
use sha2::{Digest, Sha256};
use tokio::net::TcpStream;
use tokio_rustls::TlsConnector;

struct Certificate {
    cert_pem: String,
    key_pem: String,
    der: Vec<u8>,
}

fn certificate() -> Certificate {
    let made = rcgen::generate_simple_self_signed(vec!["localhost".to_string()]).unwrap();
    Certificate {
        cert_pem: made.cert.pem(),
        key_pem: made.key_pair.serialize_pem(),
        der: made.cert.der().to_vec(),
    }
}

#[derive(Debug)]
struct Pinned {
    der: Vec<u8>,
    provider: Arc<CryptoProvider>,
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
        if end_entity.as_ref() == self.der {
            Ok(ServerCertVerified::assertion())
        } else {
            Err(rustls::Error::General("not the pinned certificate".into()))
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

fn client(
    der: &[u8],
    versions: &[&'static SupportedProtocolVersion],
    alpn: &[&[u8]],
) -> ClientConfig {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let mut config = ClientConfig::builder_with_provider(provider.clone())
        .with_protocol_versions(versions)
        .unwrap()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(Pinned {
            der: der.to_vec(),
            provider,
        }))
        .with_no_client_auth();
    config.alpn_protocols = alpn.iter().map(|p| p.to_vec()).collect();
    config
}

struct Negotiated {
    version: ProtocolVersion,
    suite: rustls::CipherSuite,
    alpn: Option<Vec<u8>>,
}

async fn connect(
    address: SocketAddr,
    config: ClientConfig,
) -> (tokio_rustls::client::TlsStream<TcpStream>, Negotiated) {
    let tcp = TcpStream::connect(address).await.unwrap();
    let name = ServerName::try_from("localhost").unwrap();
    let tls = TlsConnector::from(Arc::new(config))
        .connect(name, tcp)
        .await
        .unwrap();
    let (_, conn) = tls.get_ref();
    let negotiated = Negotiated {
        version: conn.protocol_version().unwrap(),
        suite: conn.negotiated_cipher_suite().unwrap().suite(),
        alpn: conn.alpn_protocol().map(<[u8]>::to_vec),
    };
    (tls, negotiated)
}

async fn send(
    address: SocketAddr,
    der: &[u8],
    method: &str,
    path: &str,
    body: Vec<u8>,
) -> Response<Bytes> {
    let (tls, _) = connect(
        address,
        client(der, &[&rustls::version::TLS13], &[b"http/1.1"]),
    )
    .await;
    let (mut sender, connection) = hyper::client::conn::http1::handshake(TokioIo::new(tls))
        .await
        .unwrap();
    tokio::spawn(connection);
    let request = Request::builder()
        .method(method)
        .uri(path)
        .header("host", "localhost")
        .body(Full::new(Bytes::from(body)))
        .unwrap();
    let (parts, body) = sender.send_request(request).await.unwrap().into_parts();
    Response::from_parts(parts, body.collect().await.unwrap().to_bytes())
}

struct Server {
    listeners: Listeners,
    address: SocketAddr,
    der: Vec<u8>,
}

fn start() -> Server {
    let cert = certificate();
    let tls = tls::server_config(cert.cert_pem.as_bytes(), cert.key_pem.as_bytes()).unwrap();
    let assets = Assets::from_files([
        (
            "/index.html",
            &b"<!doctype html><meta name=\"bmac-style-nonce\" content=\"__BMAC_STYLE_NONCE__\"><title>BMAC</title>"[..],
        ),
        ("/assets/app-1a2b.js", &b"console.log(1)"[..]),
        ("/favicon.ico", &b"ico"[..]),
    ]);
    let server = HttpsServer::with_handshake_timeout(
        tls,
        app::router(
            Arc::new(assets),
            Arc::new(bmacd::web::client::EngineClient::new(
                "/nonexistent/engine.sock",
            )),
        ),
        Duration::from_secs(1),
    );
    let mut listeners = Listeners::new(server);
    let asked: SocketAddr = "127.0.0.1:0".parse().unwrap();
    assert!(listeners.reconcile(&[asked]).is_empty());
    let address = listeners.bound()[&asked];
    Server {
        listeners,
        address,
        der: cert.der,
    }
}

fn header<'a>(response: &'a Response<Bytes>, name: &str) -> Option<&'a str> {
    response.headers().get(name).map(|v| v.to_str().unwrap())
}

/// The style nonce in the page's policy, checked against the one in its body.
fn page_nonce(response: &Response<Bytes>) -> String {
    let policy = header(response, "content-security-policy").unwrap();
    let nonce = policy
        .split("'nonce-")
        .nth(1)
        .and_then(|rest| rest.split('\'').next())
        .expect("a style nonce")
        .to_string();
    assert_eq!(nonce.len(), 32);
    assert!(nonce.bytes().all(|b| b.is_ascii_hexdigit()));
    let body = std::str::from_utf8(response.body()).unwrap();
    if !body.is_empty() {
        assert!(body.contains(&format!("content=\"{nonce}\"")), "{body}");
        assert!(!body.contains(STYLE_NONCE_PLACEHOLDER));
    }
    nonce
}

#[tokio::test]
async fn the_page_gets_a_new_style_nonce_on_every_load() {
    let server = start();
    let mut seen = std::collections::HashSet::new();
    for _ in 0..3 {
        let page = send(server.address, &server.der, "GET", "/", Vec::new()).await;
        let nonce = page_nonce(&page);
        let policy = header(&page, "content-security-policy").unwrap();
        assert!(policy.contains("script-src 'self';"), "{policy}");
        assert!(!policy.contains("unsafe-inline"), "{policy}");
        assert!(seen.insert(nonce));
    }
}

#[tokio::test]
async fn every_response_has_the_security_headers_and_no_cookies() {
    let server = start();
    for (method, path, status) in [
        ("GET", "/", 200),
        ("GET", "/hosts/mox1", 200),
        ("GET", "/assets/app-1a2b.js", 200),
        ("GET", "/assets/gone.js", 404),
        ("GET", "/api/v1/nothing", 404),
        ("POST", "/", 405),
    ] {
        let response = send(server.address, &server.der, method, path, Vec::new()).await;
        assert_eq!(response.status().as_u16(), status, "{method} {path}");
        let page = status == 200 && !path.starts_with("/assets/");
        for (name, value) in SECURITY_HEADERS {
            if name == "content-security-policy" && page {
                let nonce = page_nonce(&response);
                assert_eq!(
                    header(&response, name),
                    Some(app::page_security_policy(&nonce).as_str()),
                    "{method} {path}"
                );
                continue;
            }
            assert_eq!(
                header(&response, name),
                Some(value),
                "{method} {path}: {name}"
            );
        }
        for absent in [
            "set-cookie",
            "strict-transport-security",
            "access-control-allow-origin",
            "server",
        ] {
            assert!(
                header(&response, absent).is_none(),
                "{method} {path}: {absent}"
            );
        }
    }
}

#[tokio::test]
async fn static_files_and_page_routes() {
    let server = start();
    let index = send(
        server.address,
        &server.der,
        "GET",
        "/hosts/mox1",
        Vec::new(),
    )
    .await;
    assert_eq!(
        header(&index, "content-type"),
        Some("text/html; charset=utf-8")
    );
    assert_eq!(header(&index, "cache-control"), Some("no-cache"));
    let nonce = page_nonce(&index);
    assert_eq!(
        std::str::from_utf8(index.body()).unwrap(),
        format!(
            "<!doctype html><meta name=\"bmac-style-nonce\" content=\"{nonce}\"><title>BMAC</title>"
        )
    );

    let script = send(
        server.address,
        &server.der,
        "GET",
        "/assets/app-1a2b.js",
        Vec::new(),
    )
    .await;
    assert_eq!(
        header(&script, "content-type"),
        Some("text/javascript; charset=utf-8")
    );
    assert_eq!(
        header(&script, "cache-control"),
        Some("public, max-age=31536000, immutable")
    );

    let head = send(server.address, &server.der, "HEAD", "/", Vec::new()).await;
    assert_eq!(head.status(), 200);
    assert!(head.body().is_empty());

    let post = send(server.address, &server.der, "POST", "/", Vec::new()).await;
    assert_eq!(header(&post, "allow"), Some("GET, HEAD"));
}

#[tokio::test]
async fn api_errors_are_json_and_never_cached() {
    let server = start();
    let response = send(
        server.address,
        &server.der,
        "GET",
        "/api/v1/no-such-endpoint",
        Vec::new(),
    )
    .await;
    assert_eq!(header(&response, "cache-control"), Some("no-store"));
    assert_eq!(header(&response, "content-type"), Some("application/json"));
    let body: serde_json::Value = serde_json::from_slice(response.body()).unwrap();
    assert_eq!(body["code"], "not_found");

    let big = send(
        server.address,
        &server.der,
        "POST",
        "/api/v1/jobs",
        vec![b'x'; MAX_BODY + 1],
    )
    .await;
    assert_eq!(big.status(), 413);
}

#[tokio::test]
async fn tls_versions_suites_and_alpn() {
    let server = start();
    for version in [&rustls::version::TLS13, &rustls::version::TLS12] {
        let (_, negotiated) = connect(
            server.address,
            client(&server.der, &[version], &[b"h2", b"http/1.1"]),
        )
        .await;
        assert_eq!(negotiated.version, version.version);
        assert!(
            CIPHER_SUITES.iter().any(|s| s.suite() == negotiated.suite),
            "{:?}",
            negotiated.suite
        );
        assert_eq!(negotiated.alpn.as_deref(), Some(&b"h2"[..]));
    }
}

#[tokio::test]
async fn a_client_that_never_finishes_the_handshake_is_dropped() {
    let server = start();
    let started = std::time::Instant::now();
    let mut tcp = TcpStream::connect(server.address).await.unwrap();
    let mut buffer = [0u8; 16];
    let read = tokio::time::timeout(
        Duration::from_secs(20),
        tokio::io::AsyncReadExt::read(&mut tcp, &mut buffer),
    )
    .await;
    assert!(matches!(read, Ok(Ok(0)) | Ok(Err(_))), "{read:?}");
    assert!(started.elapsed() >= Duration::from_millis(900));
}

#[tokio::test]
async fn listeners_follow_the_wanted_addresses_and_refuse_wildcards() {
    let mut server = start();
    let failed = server.listeners.reconcile(&[
        "0.0.0.0:8443".parse().unwrap(),
        "[::]:8443".parse().unwrap(),
    ]);
    assert_eq!(failed.len(), 2);
    assert!(
        failed
            .iter()
            .all(|(_, e)| e.to_string() == "refusing a wildcard address")
    );
    assert!(
        server.listeners.bound().is_empty(),
        "the old listener stops when no longer wanted"
    );
    let mut refused = false;
    for _ in 0..50 {
        if TcpStream::connect(server.address).await.is_err() {
            refused = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(refused, "nothing listens on {} any more", server.address);
}

#[test]
fn fingerprints_are_sha256_of_the_der() {
    let cert = certificate();
    let fingerprint = tls::pem_fingerprint(cert.cert_pem.as_bytes()).unwrap();
    let expected: Vec<String> = Sha256::digest(&cert.der)
        .iter()
        .map(|b| format!("{b:02X}"))
        .collect();
    assert_eq!(fingerprint, expected.join(":"));
    assert_eq!(fingerprint.len(), 95);
    assert!(tls::pem_fingerprint(b"no certificate here").is_err());
}

#[test]
fn bad_credentials_are_refused() {
    let cert = certificate();
    assert!(matches!(
        tls::server_config(b"", cert.key_pem.as_bytes()),
        Err(tls::TlsError::NoCertificate)
    ));
    assert!(tls::server_config(cert.cert_pem.as_bytes(), b"not a key").is_err());
    let other = certificate();
    assert!(tls::server_config(cert.cert_pem.as_bytes(), other.key_pem.as_bytes()).is_err());
}
