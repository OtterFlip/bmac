// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

//! A joining host's request (`DEPLOYMENT.md`, "Host setup job"): in setup
//! mode with a join bundle, `bmacd` presents the bundle's token to an
//! existing host on port 8444, checking that host's chain against the
//! bundle's cluster CA fingerprint first (`SECURITY.md`, "Joins and
//! enrollment"), and sends its SSH host key and Dashboard fingerprint for
//! the approving administrator to compare. It tries the bundle's hosts in
//! turn until one takes the token, and records the outcome beside the
//! bootstrap file for the setup page.

use std::path::{Path, PathBuf};
use std::time::Duration;

use bmac_core::HostSlot;
use bmac_proto::api::{ApiError, ErrorCode, JoinProgress, JoinState};
use bmac_proto::bootstrap::{BootstrapOrigin, JoinBundle};
use bmac_proto::peer::{JOIN_PATH, JoinRequest, PEER_PORT, Pending};
use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::header;
use hyper_util::rt::TokioIo;
use tokio::net::TcpStream;
use tokio_rustls::TlsConnector;

use crate::auth::Clock;
use crate::log;
use crate::mesh::tls::{join_client_pinned, server_name};

/// Where the join's progress is kept, beside the bootstrap file.
pub const JOIN_STATE: &str = "/var/lib/bmac/bootstrap/join.json";
const RETRY: Duration = Duration::from_secs(30);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);

pub fn read_state(path: &Path) -> Option<JoinProgress> {
    serde_json::from_slice(&std::fs::read(path).ok()?).ok()
}

pub fn write_state(path: &Path, progress: &JoinProgress) {
    let text = serde_json::to_vec_pretty(progress).expect("join progress serializes");
    if let Err(e) = crate::native::write_atomic(path, &text) {
        log::warning(&format!("cannot record the join's progress: {e}"));
    }
}

/// The host name in a Dashboard URL such as `https://mox1.tail.ts.net:8443`.
pub fn dashboard_host(url: &str) -> Option<&str> {
    let rest = url.strip_prefix("https://")?;
    let end = rest.find([':', '/']).unwrap_or(rest.len());
    let host = &rest[..end];
    (!host.is_empty()).then_some(host)
}

#[derive(Debug)]
enum Attempt {
    Taken(Pending),
    /// The host answered and refused the token.
    Refused(ApiError),
    Unreachable(String),
}

async fn post(bundle: &JoinBundle, to: HostSlot, address: &str, port: u16, body: Bytes) -> Attempt {
    let tls = match join_client_pinned(&bundle.proxmox_ca_fingerprint) {
        Ok(tls) => tls,
        Err(e) => return Attempt::Unreachable(e.to_string()),
    };
    let attempt = async {
        let tcp = TcpStream::connect((address, port))
            .await
            .map_err(|e| e.to_string())?;
        let tls = TlsConnector::from(tls)
            .connect(server_name(to), tcp)
            .await
            .map_err(|e| e.to_string())?;
        let (mut sender, connection) = hyper::client::conn::http1::handshake(TokioIo::new(tls))
            .await
            .map_err(|e| e.to_string())?;
        tokio::spawn(async move {
            let _ = connection.await;
        });
        let request = hyper::Request::builder()
            .method("POST")
            .uri(JOIN_PATH)
            .header(header::HOST, to.to_string())
            .header(header::CONTENT_TYPE, "application/json")
            .body(Full::new(body))
            .expect("requests are well formed");
        let response = sender
            .send_request(request)
            .await
            .map_err(|e| e.to_string())?;
        let status = response.status();
        let bytes = response
            .into_body()
            .collect()
            .await
            .map_err(|e| e.to_string())?
            .to_bytes();
        Ok::<_, String>((status, bytes))
    };
    match tokio::time::timeout(CONNECT_TIMEOUT, attempt).await {
        Err(_) => Attempt::Unreachable(format!("{address}: timed out")),
        Ok(Err(e)) => Attempt::Unreachable(format!("{address}: {e}")),
        Ok(Ok((status, bytes))) if status.is_success() => {
            match serde_json::from_slice::<Pending>(&bytes) {
                Ok(pending) => Attempt::Taken(pending),
                Err(e) => Attempt::Unreachable(format!("{address} answered oddly: {e}")),
            }
        }
        Ok(Ok((status, bytes))) => {
            let error = serde_json::from_slice::<ApiError>(&bytes).unwrap_or_else(|_| {
                ApiError::new(ErrorCode::Internal, format!("{address} answered {status}"))
            });
            match error.code {
                ErrorCode::RateLimited | ErrorCode::Unavailable | ErrorCode::Internal => {
                    Attempt::Unreachable(format!("{address}: {}", error.message))
                }
                _ => Attempt::Refused(error),
            }
        }
    }
}

/// What this host sends with its token.
pub struct Identity {
    pub ssh_host_key: String,
    pub https_fingerprint: String,
}

/// Tries every host in the bundle once, on `port` (8444 but in tests).
pub async fn request_once(
    bundle: &JoinBundle,
    identity: &Identity,
    port: u16,
    now_ms: i64,
) -> JoinProgress {
    let body = serde_json::to_vec(&JoinRequest {
        token: bundle.join_token.clone(),
        host: bundle.slot,
        ssh_host_key: identity.ssh_host_key.clone(),
        https_fingerprint: identity.https_fingerprint.clone(),
    })
    .expect("join requests serialize");
    let body = Bytes::from(body);
    let mut problems = Vec::new();
    let mut refused = 0;
    for host in &bundle.hosts {
        let Some(address) = dashboard_host(&host.dashboard_url) else {
            problems.push(format!("{}: no host in {}", host.host, host.dashboard_url));
            continue;
        };
        match post(bundle, host.host, address, port, body.clone()).await {
            Attempt::Taken(pending) => {
                return JoinProgress {
                    state: JoinState::Waiting,
                    host: Some(host.host),
                    dashboard_url: Some(host.dashboard_url.clone()),
                    request_id: Some(pending.request_id),
                    message: None,
                    updated_at_ms: now_ms,
                };
            }
            Attempt::Refused(error) => {
                refused += 1;
                problems.push(format!("{}: {}", host.host, error.message));
            }
            Attempt::Unreachable(why) => problems.push(format!("{}: {why}", host.host)),
        }
    }
    JoinProgress {
        state: if refused > 0 && refused == bundle.hosts.len() {
            JoinState::Refused
        } else {
            JoinState::Requesting
        },
        host: None,
        dashboard_url: None,
        request_id: None,
        message: Some(problems.join("; ")),
        updated_at_ms: now_ms,
    }
}

/// Requests the join until a host takes the token or every host refuses
/// it. A request already taken isn't sent again: the token is single use.
pub fn spawn(
    bootstrap: PathBuf,
    state: PathBuf,
    identity: impl Fn() -> Result<Identity, String> + Send + 'static,
    clock: Clock,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let bundle = match crate::native::setup::read_bootstrap(&bootstrap) {
            Ok(b) => match b.origin {
                BootstrapOrigin::Join(bundle) => bundle,
                BootstrapOrigin::NewCluster(_) => return,
            },
            Err(e) => {
                log::warning(&format!("cannot read the join bundle: {e}"));
                return;
            }
        };
        if read_state(&state).is_some_and(|p| p.state != JoinState::Requesting) {
            return;
        }
        loop {
            let progress = match identity() {
                Ok(identity) => request_once(&bundle, &identity, PEER_PORT, clock()).await,
                Err(why) => JoinProgress {
                    state: JoinState::Requesting,
                    host: None,
                    dashboard_url: None,
                    request_id: None,
                    message: Some(why),
                    updated_at_ms: clock(),
                },
            };
            write_state(&state, &progress);
            match progress.state {
                JoinState::Waiting => {
                    log::info(&format!(
                        "join: {} took the request; approve it there",
                        progress.host.map(|h| h.to_string()).unwrap_or_default()
                    ));
                    return;
                }
                JoinState::Refused => {
                    log::error(&format!(
                        "join: every host refused the token: {}",
                        progress.message.as_deref().unwrap_or("")
                    ));
                    return;
                }
                JoinState::Requesting => {
                    log::warning(&format!(
                        "join: no host took the request yet: {}",
                        progress.message.as_deref().unwrap_or("")
                    ));
                }
            }
            tokio::time::sleep(RETRY).await;
        }
    })
}

/// This host's SSH host key and Dashboard fingerprint, from their files.
pub fn system_identity() -> Result<Identity, String> {
    let line = std::fs::read_to_string(crate::identity::SSH_HOST_KEY)
        .map_err(|e| format!("cannot read {}: {e}", crate::identity::SSH_HOST_KEY))?;
    let (ssh_host_key, _) = crate::identity::ssh_host_key(&line)
        .ok_or_else(|| format!("{} is not an Ed25519 key", crate::identity::SSH_HOST_KEY))?;
    let pem = std::fs::read(crate::identity::HTTPS_CERT)
        .map_err(|e| format!("cannot read {}: {e}", crate::identity::HTTPS_CERT))?;
    let https_fingerprint = crate::web::tls::pem_fingerprint(&pem).map_err(|e| e.to_string())?;
    Ok(Identity {
        ssh_host_key,
        https_fingerprint,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_dashboard_host_is_the_urls_name() {
        assert_eq!(
            dashboard_host("https://bmaclab-mox1.tail.ts.net:8443"),
            Some("bmaclab-mox1.tail.ts.net")
        );
        assert_eq!(dashboard_host("https://mox1/"), Some("mox1"));
        assert_eq!(dashboard_host("http://mox1:8443"), None);
        assert_eq!(dashboard_host("https://:8443"), None);
    }
}
