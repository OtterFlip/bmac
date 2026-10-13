// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

//! The Deployer's last step for a host (`DEPLOYMENT.md`, "Handoff"). It
//! touches the new host only with a TLS handshake and SSH: the certificate
//! the Dashboard presents must have the fingerprint the Deployer generated,
//! or there is no link at all. Even after a match, the operator is told to
//! compare the fingerprint in the browser before logging in, because the
//! Deployer's connection and the browser's are separate and one can be
//! intercepted while the other isn't.

use std::time::Duration;

use crate::tls::{self, ProbeError};
use serde::Serialize;
use tokio_util::sync::CancellationToken;

/// What one look at the Dashboard's certificate found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CertCheck {
    Match,
    /// A hard stop.
    Mismatch {
        seen: String,
    },
    /// Refused, timed out, or the handshake failed: `bmacd` isn't up yet.
    Waiting(String),
}

/// Connects, compares, and closes without sending a request.
pub async fn check_certificate(
    host: &str,
    port: u16,
    expected: &str,
    timeout: Duration,
) -> CertCheck {
    match tls::probe(host, port, timeout).await {
        Ok(seen) if tls::same_fingerprint(&seen, expected) => CertCheck::Match,
        Ok(seen) => CertCheck::Mismatch { seen },
        Err(ProbeError::NoAnswer(_)) => CertCheck::Waiting("no answer yet".into()),
        Err(ProbeError::Handshake { message, .. }) => CertCheck::Waiting(message),
    }
}

/// Looks until the certificate matches or doesn't; waiting states repeat.
pub async fn wait_for_certificate(
    host: &str,
    port: u16,
    expected: &str,
    every: Duration,
    cancel: &CancellationToken,
    report: &(dyn Fn(&CertCheck) + Send + Sync),
) -> Result<CertCheck, String> {
    let mut last = None;
    loop {
        let check = check_certificate(host, port, expected, Duration::from_secs(10)).await;
        if last.as_ref() != Some(&check) {
            report(&check);
            last = Some(check.clone());
        }
        if !matches!(check, CertCheck::Waiting(_)) {
            return Ok(check);
        }
        tokio::select! {
            _ = cancel.cancelled() => return Err("Cancelled.".into()),
            _ = tokio::time::sleep(every) => {}
        }
    }
}

/// What the handoff shows. `dashboard_url` is set only after a match.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct HandoffView {
    pub dashboard_url: Option<String>,
    pub fingerprint: String,
    pub messages: Vec<String>,
    pub warning: Option<String>,
}

/// How the host comes into the cluster, for the "what next" text.
#[derive(Debug, Clone)]
pub enum NextStep {
    FirstHost,
    /// The Dashboard of an existing host, from the join bundle, already
    /// checked the same way.
    Join {
        existing_url: String,
        existing_fingerprint: String,
    },
}

/// Instructions that always go with the link.
pub fn browser_check(fingerprint: &str) -> String {
    format!(
        "Before you log in, open the browser's certificate details on the warning page and \
         check that the SHA-256 fingerprint is {fingerprint}. The Deployer's check passed, but \
         your browser makes its own connection, and only you can check that one. If it differs, \
         don't log in."
    )
}

/// The view after a certificate check. A mismatch never yields a link.
pub fn view(
    check: &CertCheck,
    url: &str,
    expected: &str,
    next: &NextStep,
    luks: bool,
    idrac: Option<&str>,
) -> HandoffView {
    match check {
        CertCheck::Match => {
            let mut messages = vec![browser_check(expected)];
            match next {
                NextStep::FirstHost => messages.push(
                    "Log in as root@pam with the root password you chose. The setup job is \
                     waiting for you there."
                        .into(),
                ),
                NextStep::Join {
                    existing_url,
                    existing_fingerprint,
                } => {
                    messages.push(format!(
                        "Open the Dashboard of an existing host, {existing_url} (fingerprint \
                         {existing_fingerprint}), where the new host shows as pending within a \
                         minute or so. Approve it there; the setup job then starts and asks for \
                         input there."
                    ));
                    messages.push(format!("The new host's own Dashboard is {url}."));
                }
            }
            if luks {
                let console = match idrac {
                    Some(address) => format!("the host's console (iDRAC {address})"),
                    None => "the host's console".to_string(),
                };
                messages.push(format!(
                    "Setup encrypts the disks and asks for the passphrase at {console}; it is \
                     never typed into the browser."
                ));
            }
            HandoffView {
                dashboard_url: Some(url.to_string()),
                fingerprint: expected.to_string(),
                messages,
                warning: None,
            }
        }
        CertCheck::Mismatch { seen } => HandoffView {
            dashboard_url: None,
            fingerprint: expected.to_string(),
            messages: Vec::new(),
            warning: Some(format!(
                "The certificate at {url} has the fingerprint {seen}, not {expected}, the one \
                 this Deployer put in the ISO. Either a different machine is answering at that \
                 name, or the connection is being intercepted. Don't open the Dashboard or log \
                 in. Check the machines in the Tailscale admin console, and rebuild and \
                 reinstall the host if you can't explain it."
            )),
        },
        CertCheck::Waiting(reason) => HandoffView {
            dashboard_url: None,
            fingerprint: expected.to_string(),
            messages: vec![format!("Waiting for the Dashboard to start ({reason}).")],
            warning: None,
        },
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use tokio::net::TcpListener;
    use tokio_rustls::TlsAcceptor;

    use super::*;
    use crate::hostiso::keys::HttpsCertificate;

    /// A TLS server presenting `cert` that never reads a request; returns
    /// its port and a counter of bytes received after the handshake.
    async fn server(cert: &HttpsCertificate) -> (u16, Arc<std::sync::atomic::AtomicUsize>) {
        use rustls::pki_types::pem::PemObject;
        let chain = vec![
            rustls::pki_types::CertificateDer::from_pem_slice(cert.cert_pem.as_bytes()).unwrap(),
        ];
        let key =
            rustls::pki_types::PrivateKeyDer::from_pem_slice(cert.key_pem.as_bytes()).unwrap();
        let config = rustls::ServerConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(chain, key)
        .unwrap();
        let acceptor = TlsAcceptor::from(Arc::new(config));
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let received = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter = received.clone();
        tokio::spawn(async move {
            while let Ok((tcp, _)) = listener.accept().await {
                let acceptor = acceptor.clone();
                let counter = counter.clone();
                tokio::spawn(async move {
                    if let Ok(mut tls) = acceptor.accept(tcp).await {
                        use tokio::io::AsyncReadExt;
                        let mut buffer = [0u8; 1024];
                        while let Ok(n) = tls.read(&mut buffer).await {
                            if n == 0 {
                                break;
                            }
                            counter.fetch_add(n, std::sync::atomic::Ordering::SeqCst);
                        }
                    }
                });
            }
        });
        (port, received)
    }

    const URL: &str = "https://mox1.tail1234.ts.net:8443/";

    #[tokio::test]
    async fn the_expected_certificate_gives_the_link_and_the_browser_check() {
        let cert = HttpsCertificate::generate(&["localhost".into()], 1_760_000_000_000).unwrap();
        let (port, received) = server(&cert).await;
        let check =
            check_certificate("localhost", port, &cert.fingerprint, Duration::from_secs(5)).await;
        assert_eq!(check, CertCheck::Match);
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(
            received.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "no request is sent"
        );
        let shown = view(
            &check,
            URL,
            &cert.fingerprint,
            &NextStep::FirstHost,
            false,
            None,
        );
        assert_eq!(shown.dashboard_url.as_deref(), Some(URL));
        assert!(shown.messages[0].contains(&cert.fingerprint));
        assert!(shown.messages[0].contains("Before you log in"));
        assert!(shown.warning.is_none());
    }

    #[tokio::test]
    async fn a_different_certificate_is_a_hard_stop_without_a_link() {
        let expected = HttpsCertificate::generate(&["localhost".into()], 0).unwrap();
        let presented = HttpsCertificate::generate(&["localhost".into()], 0).unwrap();
        let (port, _) = server(&presented).await;
        let check = wait_for_certificate(
            "localhost",
            port,
            &expected.fingerprint,
            Duration::from_millis(10),
            &CancellationToken::new(),
            &|_| {},
        )
        .await
        .unwrap();
        assert_eq!(
            check,
            CertCheck::Mismatch {
                seen: presented.fingerprint.clone()
            }
        );
        for next in [
            NextStep::FirstHost,
            NextStep::Join {
                existing_url: "https://mox2.tail1234.ts.net:8443/".into(),
                existing_fingerprint: "AA".into(),
            },
        ] {
            let shown = view(
                &check,
                URL,
                &expected.fingerprint,
                &next,
                true,
                Some("10.0.0.5"),
            );
            assert_eq!(shown.dashboard_url, None);
            assert!(shown.messages.is_empty());
            let warning = shown.warning.unwrap();
            assert!(warning.contains("intercepted") && warning.contains(&presented.fingerprint));
            assert!(warning.contains("Don't open the Dashboard"));
        }
    }

    #[tokio::test]
    async fn nothing_listening_keeps_waiting() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        let check = check_certificate("127.0.0.1", port, "AA", Duration::from_secs(2)).await;
        assert!(matches!(check, CertCheck::Waiting(_)));
        let shown = view(&check, URL, "AA", &NextStep::FirstHost, false, None);
        assert_eq!(shown.dashboard_url, None);
        assert!(shown.warning.is_none());

        let cancel = CancellationToken::new();
        cancel.cancel();
        assert!(
            wait_for_certificate(
                "127.0.0.1",
                port,
                "AA",
                Duration::from_secs(60),
                &cancel,
                &|_| {}
            )
            .await
            .is_err()
        );
    }

    #[test]
    fn joining_hosts_are_sent_to_an_existing_dashboard() {
        let shown = view(
            &CertCheck::Match,
            URL,
            "AB:CD",
            &NextStep::Join {
                existing_url: "https://mox2.tail1234.ts.net:8443/".into(),
                existing_fingerprint: "EF:01".into(),
            },
            true,
            Some("10.0.0.5"),
        );
        assert!(shown.messages[0].contains("AB:CD"));
        assert!(shown.messages[1].contains("mox2") && shown.messages[1].contains("EF:01"));
        assert!(shown.messages[2].contains(URL));
        assert!(shown.messages[3].contains("iDRAC 10.0.0.5"));
    }
}
