// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

//! A client for other members' port 8444, with this node's certificate:
//! what a job process uses to reach the other hosts, and the engine to
//! fetch a file from one.

use std::io::Write;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use bmac_core::HostSlot;
use bmac_proto::api::ApiError;
use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::http::header;
use hyper_util::rt::TokioIo;
use rustls::ClientConfig;
use tokio::net::TcpStream;
use tokio_rustls::TlsConnector;

use super::tls::{PeerTls, server_name};
use super::{Resolver, cluster_addresses};
use crate::cluster::ClusterView;

#[derive(Clone)]
pub struct PeerClient {
    tls: Arc<ClientConfig>,
    cluster: Arc<dyn ClusterView>,
    resolve: Resolver,
    connect_timeout: Duration,
    /// How long one JSON call may take, end to end.
    call_timeout: Duration,
}

impl std::fmt::Debug for PeerClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("PeerClient")
    }
}

impl PeerClient {
    pub fn new(tls: Arc<ClientConfig>, cluster: Arc<dyn ClusterView>, resolve: Resolver) -> Self {
        PeerClient {
            tls,
            cluster,
            resolve,
            connect_timeout: Duration::from_secs(5),
            call_timeout: Duration::from_secs(30),
        }
    }

    /// With this node's certificate from pmxcfs, reaching members at their
    /// cluster addresses.
    pub fn system(node: HostSlot) -> Result<PeerClient, String> {
        let tls = PeerTls::system(&node.to_string())?;
        Ok(PeerClient::new(
            tls.client,
            Arc::new(crate::cluster::PveCluster::system()),
            cluster_addresses(),
        ))
    }

    async fn connect(
        &self,
        host: HostSlot,
    ) -> Result<hyper::client::conn::http1::SendRequest<Full<Bytes>>, String> {
        let cluster = self.cluster.clone();
        let membership = tokio::task::spawn_blocking(move || cluster.membership())
            .await
            .map_err(|e| e.to_string())??;
        let addresses = (self.resolve)(host, &membership);
        if addresses.is_empty() {
            return Err(format!("no address is known for {host}"));
        }
        let connector = TlsConnector::from(self.tls.clone());
        let mut last = String::new();
        for address in addresses {
            let attempt = async {
                let tcp = TcpStream::connect(address)
                    .await
                    .map_err(|e| e.to_string())?;
                let _ = tcp.set_nodelay(true);
                let tls = connector
                    .connect(server_name(host), tcp)
                    .await
                    .map_err(|e| e.to_string())?;
                let (sender, connection) = hyper::client::conn::http1::handshake(TokioIo::new(tls))
                    .await
                    .map_err(|e| e.to_string())?;
                tokio::spawn(async move {
                    let _ = connection.await;
                });
                Ok::<_, String>(sender)
            };
            match tokio::time::timeout(self.connect_timeout, attempt).await {
                Ok(Ok(sender)) => return Ok(sender),
                Ok(Err(e)) => last = format!("{address}: {e}"),
                Err(_) => last = format!("{address}: timed out"),
            }
        }
        Err(last)
    }

    async fn send(
        &self,
        host: HostSlot,
        method: &str,
        path: &str,
        body: Bytes,
    ) -> Result<hyper::Response<hyper::body::Incoming>, String> {
        let mut sender = self.connect(host).await?;
        let request = hyper::Request::builder()
            .method(method)
            .uri(path)
            .header(header::HOST, host.to_string())
            .header(header::CONTENT_TYPE, "application/json")
            .body(Full::new(body))
            .map_err(|e| e.to_string())?;
        sender
            .send_request(request)
            .await
            .map_err(|e| e.to_string())
    }

    /// Sends `body` (JSON, or nothing for a `GET`) and reads the JSON answer;
    /// a refusal comes back as the peer's message.
    pub async fn json<T: serde::de::DeserializeOwned>(
        &self,
        host: HostSlot,
        method: &str,
        path: &str,
        body: Option<&(dyn erased::Body + Sync)>,
    ) -> Result<T, String> {
        let bytes = match body {
            Some(body) => Bytes::from(body.to_json()?),
            None => Bytes::new(),
        };
        let call = async {
            let response = self.send(host, method, path, bytes).await?;
            let status = response.status();
            let bytes = response
                .into_body()
                .collect()
                .await
                .map_err(|e| e.to_string())?
                .to_bytes();
            if !status.is_success() {
                return Err(serde_json::from_slice::<ApiError>(&bytes)
                    .map(|e| e.message)
                    .unwrap_or_else(|_| format!("answered {status}")));
            }
            serde_json::from_slice(&bytes).map_err(|e| format!("a bad answer: {e}"))
        };
        tokio::time::timeout(self.call_timeout, call)
            .await
            .map_err(|_| "it didn't answer in time".to_string())?
            .map_err(|e| format!("{host}: {e}"))
    }

    /// Streams `path`'s body into `to`, refusing more than `max` bytes.
    pub async fn download(
        &self,
        host: HostSlot,
        path: &str,
        to: &Path,
        max: u64,
    ) -> Result<u64, String> {
        let response = self.send(host, "GET", path, Bytes::new()).await?;
        let status = response.status();
        let mut body = response.into_body();
        if !status.is_success() {
            let bytes = body
                .collect()
                .await
                .map(|b| b.to_bytes())
                .unwrap_or_default();
            return Err(serde_json::from_slice::<ApiError>(&bytes)
                .map(|e| e.message)
                .unwrap_or_else(|_| format!("{host} answered {status}")));
        }
        let mut file = std::fs::File::create(to).map_err(|e| format!("{}: {e}", to.display()))?;
        let mut written = 0u64;
        loop {
            let frame = match tokio::time::timeout(Duration::from_secs(60), body.frame()).await {
                Err(_) => return Err(format!("{host} stopped sending")),
                Ok(None) => break,
                Ok(Some(frame)) => frame.map_err(|e| format!("{host}: {e}"))?,
            };
            let Ok(data) = frame.into_data() else {
                continue;
            };
            written += data.len() as u64;
            if written > max {
                return Err(format!("{host} sent more than {max} bytes"));
            }
            file.write_all(&data)
                .map_err(|e| format!("{}: {e}", to.display()))?;
        }
        file.sync_all()
            .map_err(|e| format!("{}: {e}", to.display()))?;
        Ok(written)
    }
}

/// A request body of any serializable type, so [`PeerClient::json`] stays
/// object safe for its callers' traits.
pub mod erased {
    pub trait Body {
        fn to_json(&self) -> Result<Vec<u8>, String>;
    }

    impl<T: serde::Serialize> Body for T {
        fn to_json(&self) -> Result<Vec<u8>, String> {
            serde_json::to_vec(self).map_err(|e| e.to_string())
        }
    }
}
