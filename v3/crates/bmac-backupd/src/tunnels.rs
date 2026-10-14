// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

//! The connections cluster hosts open to this appliance's 8445 with ALPN
//! `bmac-source/1` (`BACKUP.md`, "Connections"). The host opened it, but
//! the appliance makes the requests over it, as the HTTP/2 client: check-ins,
//! claims, and sends. Hosts listen on nothing, and no appliance ever dials
//! one.

use std::collections::BTreeMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use bmac_core::HostSlot;
use bytes::Bytes;
use http_body_util::Full;
use hyper::client::conn::http2::SendRequest;
use hyper_util::rt::{TokioExecutor, TokioIo, TokioTimer};
use tokio::io::{AsyncRead, AsyncWrite};

const KEEPALIVE: Duration = Duration::from_secs(30);
const KEEPALIVE_TIMEOUT: Duration = Duration::from_secs(20);
/// The largest frame the host may send; sends move in big chunks.
const MAX_FRAME: u32 = 1024 * 1024;

type Key = (String, HostSlot);
type Sender = SendRequest<Full<Bytes>>;

/// The open connections, by cluster and host. A host that connects again
/// replaces its older connection.
#[derive(Debug, Default)]
pub struct Tunnels {
    open: Mutex<BTreeMap<Key, (u64, Sender)>>,
    next: AtomicU64,
}

/// Forgets a connection when it ends, unless a newer one took its place.
struct Registration<'a> {
    tunnels: &'a Tunnels,
    key: Key,
    id: u64,
}

impl Drop for Registration<'_> {
    fn drop(&mut self) {
        let mut open = self.tunnels.open.lock().unwrap_or_else(|p| p.into_inner());
        if open.get(&self.key).is_some_and(|(id, _)| *id == self.id) {
            open.remove(&self.key);
        }
    }
}

impl Tunnels {
    /// Takes over `io`, a verified connection from `host` of `cluster`, and
    /// keeps it open for requests until it ends.
    pub async fn serve<IO>(&self, cluster: &str, host: HostSlot, io: IO) -> Result<(), String>
    where
        IO: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
        let (sender, connection) = hyper::client::conn::http2::Builder::new(TokioExecutor::new())
            .timer(TokioTimer::new())
            .adaptive_window(true)
            .max_frame_size(MAX_FRAME)
            .keep_alive_interval(KEEPALIVE)
            .keep_alive_timeout(KEEPALIVE_TIMEOUT)
            .keep_alive_while_idle(true)
            .handshake(TokioIo::new(io))
            .await
            .map_err(|e| format!("{host}: {e}"))?;
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        let key = (cluster.to_string(), host);
        self.open
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(key.clone(), (id, sender));
        let _registration = Registration {
            tunnels: self,
            key,
            id,
        };
        connection.await.map_err(|e| format!("{host}: {e}"))
    }

    /// The connection `host` of `cluster` keeps open, if any.
    pub fn sender(&self, cluster: &str, host: HostSlot) -> Option<Sender> {
        self.open
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(&(cluster.to_string(), host))
            .map(|(_, sender)| sender.clone())
            .filter(|sender| !sender.is_closed())
    }

    /// The hosts of `cluster` connected now.
    pub fn hosts(&self, cluster: &str) -> Vec<HostSlot> {
        self.open
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .iter()
            .filter(|((c, _), (_, sender))| c == cluster && !sender.is_closed())
            .map(|((_, host), _)| *host)
            .collect()
    }
}

/// The host a cluster certificate names: the first DNS name, or the
/// common name, whose first label is a host slot such as `mox1`.
pub fn host_of(der: &[u8]) -> Option<HostSlot> {
    use x509_parser::prelude::{FromDer, GeneralName, X509Certificate};
    let (_, cert) = X509Certificate::from_der(der).ok()?;
    let mut names: Vec<String> = Vec::new();
    if let Ok(Some(san)) = cert.subject_alternative_name() {
        for name in &san.value.general_names {
            if let GeneralName::DNSName(dns) = name {
                names.push(dns.to_string());
            }
        }
    }
    names.extend(
        cert.subject()
            .iter_common_name()
            .filter_map(|cn| cn.as_str().ok().map(String::from)),
    );
    names
        .iter()
        .find_map(|name| name.split('.').next()?.parse().ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_host_certificate_names_its_slot() {
        let key = rcgen::KeyPair::generate().unwrap();
        let named = |names: &[&str]| {
            rcgen::CertificateParams::new(names.iter().map(|n| n.to_string()).collect::<Vec<_>>())
                .unwrap()
                .self_signed(&key)
                .unwrap()
        };
        let proxmox = named(&["localhost", "mox2", "mox2.example.internal"]);
        assert_eq!(host_of(proxmox.der()), "mox2".parse().ok());
        let fqdn = named(&["mox3.example.internal"]);
        assert_eq!(host_of(fqdn.der()), "mox3".parse().ok());
        assert_eq!(host_of(named(&["vault.example.org"]).der()), None);
        assert_eq!(host_of(b"not a certificate"), None);
    }
}
