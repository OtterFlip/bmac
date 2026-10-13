// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

//! The HTTPS listeners: one per address the engine names, each accepting
//! TLS connections and serving the router over HTTP/1.1 or HTTP/2.

use std::collections::BTreeMap;
use std::io;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use hyper_util::rt::{TokioExecutor, TokioIo, TokioTimer};
use hyper_util::server::conn::auto;
use hyper_util::service::TowerToHyperService;
use rustls::ServerConfig;
use rustls::pki_types::CertificateDer;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Semaphore;
use tokio::task::{JoinHandle, JoinSet};
use tokio_rustls::TlsAcceptor;

use crate::log;

/// A client must finish the TLS handshake, and send each request's headers,
/// within this long.
pub const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
/// Connections open at once, across every listener.
pub const MAX_CONNECTIONS: usize = 512;

/// The client's address, available to handlers as an extension.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClientAddr(pub SocketAddr);

/// The certificate the client presented and the server verified, if any,
/// available to handlers as an extension.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClientCertificate(pub Option<CertificateDer<'static>>);

pub struct HttpsServer {
    acceptor: TlsAcceptor,
    app: Router,
    connections: Arc<Semaphore>,
    handshake_timeout: Duration,
}

impl HttpsServer {
    pub fn new(tls: Arc<ServerConfig>, app: Router) -> Arc<HttpsServer> {
        HttpsServer::with_handshake_timeout(tls, app, HANDSHAKE_TIMEOUT)
    }

    pub fn with_handshake_timeout(
        tls: Arc<ServerConfig>,
        app: Router,
        handshake_timeout: Duration,
    ) -> Arc<HttpsServer> {
        Arc::new(HttpsServer {
            acceptor: TlsAcceptor::from(tls),
            app,
            connections: Arc::new(Semaphore::new(MAX_CONNECTIONS)),
            handshake_timeout,
        })
    }

    /// Serves `listener` until the task is aborted, which also closes the
    /// connections it accepted.
    pub async fn serve(self: Arc<Self>, listener: TcpListener) {
        let mut connections = JoinSet::new();
        loop {
            let accepted = tokio::select! {
                accepted = listener.accept() => accepted,
                Some(_) = connections.join_next() => continue,
            };
            let (tcp, peer) = match accepted {
                Ok(accepted) => accepted,
                Err(e) => {
                    log::warning(&format!("https: accept failed: {e}"));
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    continue;
                }
            };
            let Ok(permit) = self.connections.clone().try_acquire_owned() else {
                log::warning(&format!(
                    "https: {MAX_CONNECTIONS} connections open; refused {peer}"
                ));
                continue;
            };
            let server = self.clone();
            connections.spawn(async move {
                server.connection(tcp, peer).await;
                drop(permit);
            });
        }
    }

    async fn connection(&self, tcp: TcpStream, peer: SocketAddr) {
        let _ = tcp.set_nodelay(true);
        let tls =
            match tokio::time::timeout(self.handshake_timeout, self.acceptor.accept(tcp)).await {
                Ok(Ok(tls)) => tls,
                // Failed and abandoned handshakes are normal on a network port.
                Ok(Err(_)) | Err(_) => return,
            };
        let certificate = tls
            .get_ref()
            .1
            .peer_certificates()
            .and_then(|chain| chain.first())
            .map(|c| c.clone().into_owned());
        let app = self
            .app
            .clone()
            .layer(axum::Extension(ClientAddr(peer)))
            .layer(axum::Extension(ClientCertificate(certificate)));
        let mut builder = auto::Builder::new(TokioExecutor::new());
        builder
            .http1()
            .timer(TokioTimer::new())
            .header_read_timeout(self.handshake_timeout);
        builder
            .http2()
            .timer(TokioTimer::new())
            .max_concurrent_streams(64);
        let _ = builder
            .serve_connection_with_upgrades(TokioIo::new(tls), TowerToHyperService::new(app))
            .await;
    }
}

/// The running listeners, keyed by the address asked for.
pub struct Listeners {
    server: Arc<HttpsServer>,
    running: BTreeMap<SocketAddr, (SocketAddr, JoinHandle<()>)>,
}

impl Listeners {
    pub fn new(server: Arc<HttpsServer>) -> Listeners {
        Listeners {
            server,
            running: BTreeMap::new(),
        }
    }

    /// Starts listeners for new addresses and stops those no longer wanted.
    /// Returns the addresses that could not be bound; they are tried again
    /// on the next call. A wildcard address is never bound.
    pub fn reconcile(&mut self, wanted: &[SocketAddr]) -> Vec<(SocketAddr, io::Error)> {
        self.running.retain(|address, (_, task)| {
            let keep = wanted.contains(address) && !task.is_finished();
            if !keep {
                task.abort();
            }
            keep
        });
        let mut failed = Vec::new();
        for &address in wanted {
            if self.running.contains_key(&address) {
                continue;
            }
            if address.ip().is_unspecified() {
                failed.push((
                    address,
                    io::Error::new(io::ErrorKind::InvalidInput, "refusing a wildcard address"),
                ));
                continue;
            }
            match bind(address) {
                Ok(listener) => {
                    let bound = listener.local_addr().unwrap_or(address);
                    let task = tokio::spawn(self.server.clone().serve(listener));
                    self.running.insert(address, (bound, task));
                }
                Err(e) => failed.push((address, e)),
            }
        }
        failed
    }

    /// The bound address for each address asked for.
    pub fn bound(&self) -> BTreeMap<SocketAddr, SocketAddr> {
        self.running
            .iter()
            .map(|(asked, (bound, _))| (*asked, *bound))
            .collect()
    }
}

impl Drop for Listeners {
    fn drop(&mut self) {
        for (_, task) in self.running.values() {
            task.abort();
        }
    }
}

fn bind(address: SocketAddr) -> io::Result<TcpListener> {
    let listener = std::net::TcpListener::bind(address)?;
    listener.set_nonblocking(true)?;
    TcpListener::from_std(listener)
}
