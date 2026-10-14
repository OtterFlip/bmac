// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

//! The watchdog's checks of public production URLs (`MONITORING.md`, "The
//! appliance as an external watchdog"): one `GET`, no redirects followed,
//! good when it answers 2xx or 3xx within [`TIMEOUT`]. HTTPS is verified
//! against the system's CA bundle.

use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use http_body_util::Empty;
use hyper_util::rt::TokioIo;
use rustls::pki_types::ServerName;
use tokio::net::TcpStream;
use tokio_rustls::TlsConnector;

pub const TIMEOUT: Duration = Duration::from_secs(15);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    pub https: bool,
    pub host: String,
    pub port: u16,
    /// The path and query, `/` at least.
    pub path: String,
}

pub fn parse(url: &str) -> Result<Target, String> {
    crate::platform::check_url(url)?;
    let (https, rest) = match url.strip_prefix("https://") {
        Some(rest) => (true, rest),
        None => (false, url.strip_prefix("http://").unwrap_or(url)),
    };
    let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let (authority, tail) = rest.split_at(end);
    let path = match tail.split_once('#').map(|(p, _)| p).unwrap_or(tail) {
        "" => "/".to_string(),
        p if p.starts_with('?') => format!("/{p}"),
        p => p.to_string(),
    };
    let default_port = if https { 443 } else { 80 };
    let (host, port) = if let Some(v6) = authority.strip_prefix('[') {
        let (host, after) = v6
            .split_once(']')
            .ok_or_else(|| format!("{url}: unclosed IPv6 address"))?;
        let port = match after.strip_prefix(':') {
            Some(p) => p.parse().map_err(|_| format!("{url}: bad port"))?,
            None if after.is_empty() => default_port,
            None => return Err(format!("{url}: bad address")),
        };
        (host.to_string(), port)
    } else {
        match authority.rsplit_once(':') {
            Some((host, p)) => (
                host.to_string(),
                p.parse().map_err(|_| format!("{url}: bad port"))?,
            ),
            None => (authority.to_string(), default_port),
        }
    };
    if host.is_empty() {
        return Err(format!("{url}: no host"));
    }
    Ok(Target {
        https,
        host,
        port,
        path,
    })
}

async fn get(target: &Target, tls: Arc<rustls::ClientConfig>) -> Result<u16, String> {
    let tcp = TcpStream::connect((target.host.as_str(), target.port))
        .await
        .map_err(|e| format!("cannot connect: {e}"))?;
    let authority = if target.host.contains(':') {
        format!("[{}]", target.host)
    } else {
        target.host.clone()
    };
    let request = hyper::Request::builder()
        .method("GET")
        .uri(&target.path)
        .header("host", &authority)
        .header(
            "user-agent",
            concat!("bmac-backupd/", env!("CARGO_PKG_VERSION")),
        )
        .body(Empty::<Bytes>::new())
        .map_err(|e| e.to_string())?;
    macro_rules! send {
        ($io:expr) => {{
            let (mut sender, connection) = hyper::client::conn::http1::handshake(TokioIo::new($io))
                .await
                .map_err(|e| e.to_string())?;
            tokio::spawn(async move {
                let _ = connection.await;
            });
            sender
                .send_request(request)
                .await
                .map_err(|e| e.to_string())?
                .status()
                .as_u16()
        }};
    }
    if target.https {
        let name = ServerName::try_from(target.host.clone()).map_err(|e| e.to_string())?;
        let stream = TlsConnector::from(tls)
            .connect(name, tcp)
            .await
            .map_err(|e| format!("TLS: {e}"))?;
        Ok(send!(stream))
    } else {
        Ok(send!(tcp))
    }
}

/// `Ok` for a good answer, else what went wrong.
pub async fn check(url: &str, tls: Arc<rustls::ClientConfig>) -> Result<(), String> {
    let target = parse(url)?;
    match tokio::time::timeout(TIMEOUT, get(&target, tls)).await {
        Err(_) => Err(format!("no answer within {} seconds", TIMEOUT.as_secs())),
        Ok(Err(e)) => Err(e),
        Ok(Ok(status)) if (200..400).contains(&status) => Ok(()),
        Ok(Ok(status)) => Err(format!("HTTP {status}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urls_are_split_into_host_port_and_path() {
        let t = parse("https://shop.example.com/health?x=1#top").unwrap();
        assert_eq!(
            t,
            Target {
                https: true,
                host: "shop.example.com".into(),
                port: 443,
                path: "/health?x=1".into()
            }
        );
        let t = parse("http://[2001:db8::1]:8080").unwrap();
        assert_eq!(
            (t.host.as_str(), t.port, t.path.as_str()),
            ("2001:db8::1", 8080, "/")
        );
        assert_eq!(parse("http://h?q").unwrap().path, "/?q");
        assert!(parse("http://h:x/").is_err());
        assert!(parse("ftp://h/").is_err());
    }

    #[tokio::test]
    async fn a_good_status_passes_and_others_fail() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                tokio::spawn(async move {
                    let mut seen = Vec::new();
                    let mut buf = [0u8; 1024];
                    while !seen.windows(4).any(|w| w == b"\r\n\r\n") {
                        match socket.read(&mut buf).await {
                            Ok(0) | Err(_) => return,
                            Ok(n) => seen.extend_from_slice(&buf[..n]),
                        }
                    }
                    let request = String::from_utf8_lossy(&seen);
                    let status = match request.split_whitespace().nth(1) {
                        Some("/ok") => "200 OK",
                        Some("/moved") => "302 Found\r\nlocation: /ok",
                        _ => "503 Service Unavailable",
                    };
                    let reply = format!("HTTP/1.1 {status}\r\ncontent-length: 0\r\n\r\n");
                    let _ = socket.write_all(reply.as_bytes()).await;
                });
            }
        });
        let tls = bmac_notify::postmark::system_tls().unwrap_or_else(|_| {
            Arc::new(
                rustls::ClientConfig::builder_with_provider(Arc::new(
                    rustls::crypto::ring::default_provider(),
                ))
                .with_safe_default_protocol_versions()
                .unwrap()
                .with_root_certificates(rustls::RootCertStore::empty())
                .with_no_client_auth(),
            )
        });
        let url = |p: &str| format!("http://127.0.0.1:{port}{p}");
        assert_eq!(check(&url("/ok"), tls.clone()).await, Ok(()));
        assert_eq!(check(&url("/moved"), tls.clone()).await, Ok(()));
        assert_eq!(
            check(&url("/down"), tls.clone()).await,
            Err("HTTP 503".into())
        );
        let closed = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let dead = closed.local_addr().unwrap().port();
        drop(closed);
        let error = check(&format!("http://127.0.0.1:{dead}/"), tls)
            .await
            .unwrap_err();
        assert!(error.starts_with("cannot connect"), "{error}");
    }
}
