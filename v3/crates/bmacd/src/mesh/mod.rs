// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

//! Host-to-host links on port 8444 (`ARCHITECTURE.md`, "Cluster-wide view";
//! `SECURITY.md`, "Host to host").
//!
//! Every host serves its own record stream and pulls every other member's:
//! `GET /v1/stream` replays the server's records after the point the client
//! names, from its store, then sends new ones live, with the job and
//! observation changes made there. The client checks each record against its
//! stream's hash chain, stores it, and shows job events to its own browsers;
//! after a drop it asks again from the last record it stored. An answer for
//! a job on another host goes there as `POST /v1/answer`, checked again by
//! the host running the job. Catch-up (`catchup`) fills what the streams
//! missed, from any peer, for every origin. Only cluster members, by their
//! Proxmox node certificate, get any of this; a client without a
//! certificate may only present a join or enrollment token.

pub mod catchup;
pub mod client;
pub mod tls;
pub mod tokens;
mod updates;

use std::collections::{BTreeSet, HashMap};
use std::future::Future;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use axum::Router;
use axum::body::Body;
use axum::extract::State;
use axum::http::{StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use bmac_core::HostSlot;
use bmac_proto::api::{ApiError, ErrorCode};
use bmac_proto::peer::{
    ANSWER_PATH, AnswerRequest, ENROLL_PATH, EnrollRequest, HOLDINGS_PATH, JOIN_PATH, JoinRequest,
    PEER_PORT, PeerFrame, RECORDS_PATH, Record, STREAM_PATH,
};
use bmac_proto::stream::StreamBody;
use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper_util::rt::TokioIo;
use tokio::net::TcpStream;
use tokio::sync::{broadcast, mpsc, watch};
use tokio_rustls::TlsConnector;

use self::tls::{PeerTls, identify, server_name};
use self::tokens::Tokens;
use crate::auth::Clock;
use crate::cluster::{ClusterView, Membership};
use crate::engine::events::EventHub;
use crate::log;
use crate::runner::{AnswerRelay, RelayedAnswer, Runner};
use crate::store::{Store, StoreError};
use crate::web::api::{ChannelBody, api_error};
use crate::web::https::{ClientAddr, ClientCertificate, HttpsServer, Listeners};

/// Records sent per read from the store while catching a peer up.
const PAGE: usize = 500;
/// A received line longer than this ends the link.
const MAX_LINE: usize = 16 * 1024 * 1024;
const MEMBERS_TTL: Duration = Duration::from_secs(2);
/// How often addresses that failed to bind are tried again.
const REBIND_EVERY: Duration = Duration::from_secs(10);

pub type Resolver = Arc<dyn Fn(HostSlot, &Membership) -> Vec<SocketAddr> + Send + Sync>;

/// Reaches each member at its cluster address from `.members`.
pub fn cluster_addresses() -> Resolver {
    Arc::new(|host, membership| {
        membership
            .members
            .get(&host)
            .copied()
            .flatten()
            .map(|ip| SocketAddr::new(ip, PEER_PORT))
            .into_iter()
            .collect()
    })
}

#[derive(Clone)]
pub struct MeshConfig {
    pub host: HostSlot,
    pub tls: PeerTls,
    pub resolve: Resolver,
    pub connect_timeout: Duration,
    /// How often an idle stream sends a keepalive.
    pub keepalive: Duration,
    /// A stream silent this long is dropped and reopened.
    pub idle_timeout: Duration,
    /// How often membership is checked for links to open or close.
    pub links_every: Duration,
    pub retry_max: Duration,
    /// How often every peer's holdings are compared with this host's.
    pub catch_up_every: Duration,
    /// The most bytes of records catch-up fetches per second.
    pub copy_bytes_per_sec: u64,
}

impl MeshConfig {
    pub fn new(host: HostSlot, tls: PeerTls) -> MeshConfig {
        MeshConfig {
            host,
            tls,
            resolve: cluster_addresses(),
            connect_timeout: Duration::from_secs(5),
            keepalive: Duration::from_secs(15),
            idle_timeout: Duration::from_secs(45),
            links_every: Duration::from_secs(5),
            retry_max: Duration::from_secs(30),
            catch_up_every: Duration::from_secs(60),
            copy_bytes_per_sec: 2 << 20,
        }
    }
}

pub struct Mesh {
    config: MeshConfig,
    cluster: Arc<dyn ClusterView>,
    store: Store,
    hub: Arc<EventHub>,
    tokens: Arc<Tokens>,
    clock: Clock,
    runner: OnceLock<Arc<Runner>>,
    members: Mutex<Option<(Instant, BTreeSet<HostSlot>)>>,
    catch_up: catchup::CatchUp,
    installer: OnceLock<crate::updates::install::Installer>,
    /// Held while an install request is checked and recorded.
    installing: tokio::sync::Mutex<()>,
    /// The members whose streams this host receives now.
    reachable: Mutex<BTreeSet<HostSlot>>,
}

impl std::fmt::Debug for Mesh {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Mesh")
            .field("host", &self.config.host)
            .finish_non_exhaustive()
    }
}

fn error(code: ErrorCode, message: impl Into<String>) -> ApiError {
    ApiError::new(code, message)
}

fn ok_json(value: impl serde::Serialize) -> Response {
    (StatusCode::OK, axum::Json(value)).into_response()
}

fn line(frame: &PeerFrame) -> Bytes {
    let mut bytes = serde_json::to_vec(frame).expect("peer frames serialize");
    bytes.push(b'\n');
    Bytes::from(bytes)
}

fn parse<T: serde::de::DeserializeOwned>(body: &[u8]) -> Result<T, ApiError> {
    serde_json::from_slice(body)
        .map_err(|e| error(ErrorCode::Invalid, format!("bad request body: {e}")))
}

/// `stream` and `after` from a stream request's query.
fn stream_query(uri: &Uri) -> Result<(Option<String>, u64), ApiError> {
    let mut stream = None;
    let mut after = 0;
    for pair in uri
        .query()
        .unwrap_or("")
        .split('&')
        .filter(|p| !p.is_empty())
    {
        match pair.split_once('=') {
            Some(("stream", value))
                if !value.is_empty()
                    && value.len() <= 64
                    && value
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b == b'-') =>
            {
                stream = Some(value.to_string());
            }
            Some(("after", value)) => {
                after = value
                    .parse()
                    .map_err(|_| error(ErrorCode::Invalid, "after must be a whole number"))?;
            }
            _ => {
                return Err(error(
                    ErrorCode::Invalid,
                    format!("bad query parameter {pair:?}"),
                ));
            }
        }
    }
    Ok((stream, after))
}

impl Mesh {
    pub fn new(
        config: MeshConfig,
        cluster: Arc<dyn ClusterView>,
        store: Store,
        hub: Arc<EventHub>,
        tokens: Arc<Tokens>,
        clock: Clock,
    ) -> Arc<Mesh> {
        let config_rate = config.copy_bytes_per_sec;
        Arc::new(Mesh {
            config,
            cluster,
            store,
            hub,
            tokens,
            clock,
            runner: OnceLock::new(),
            members: Mutex::new(None),
            catch_up: catchup::CatchUp::new(config_rate),
            installer: OnceLock::new(),
            installing: tokio::sync::Mutex::new(()),
            reachable: Mutex::new(BTreeSet::new()),
        })
    }

    pub fn host(&self) -> HostSlot {
        self.config.host
    }

    pub fn tokens(&self) -> &Arc<Tokens> {
        &self.tokens
    }

    /// Lets peers deliver answers to the jobs running here.
    pub fn set_runner(&self, runner: Arc<Runner>) {
        let _ = self.runner.set(runner);
    }

    /// Lets this host take updates.
    pub fn set_installer(&self, installer: crate::updates::install::Installer) {
        let _ = self.installer.set(installer);
    }

    /// A client for the other members' port 8444, as this host.
    pub fn peer_client(&self) -> client::PeerClient {
        client::PeerClient::new(
            self.config.tls.client.clone(),
            self.cluster.clone(),
            self.config.resolve.clone(),
        )
    }

    /// Every cluster member's slot.
    pub async fn member_hosts(&self) -> Result<BTreeSet<HostSlot>, ApiError> {
        self.members().await
    }

    pub fn installer(&self) -> Option<&crate::updates::install::Installer> {
        self.installer.get()
    }

    pub fn reachable_peers(&self) -> Vec<HostSlot> {
        self.reachable.lock().unwrap().iter().copied().collect()
    }

    async fn membership(&self) -> Result<Membership, ApiError> {
        let cluster = self.cluster.clone();
        tokio::task::spawn_blocking(move || cluster.membership())
            .await
            .map_err(|e| error(ErrorCode::Internal, e.to_string()))?
            .map_err(|e| error(ErrorCode::Unavailable, e))
    }

    async fn members(&self) -> Result<BTreeSet<HostSlot>, ApiError> {
        if let Some((at, members)) = self.members.lock().unwrap().as_ref()
            && at.elapsed() < MEMBERS_TTL
        {
            return Ok(members.clone());
        }
        let members: BTreeSet<HostSlot> = self.membership().await?.members.into_keys().collect();
        *self.members.lock().unwrap() = Some((Instant::now(), members.clone()));
        Ok(members)
    }

    /// The member a client's certificate names.
    async fn member(&self, certificate: &ClientCertificate) -> Result<HostSlot, ApiError> {
        let Some(cert) = &certificate.0 else {
            return Err(error(
                ErrorCode::Forbidden,
                "a cluster member's certificate is required",
            ));
        };
        identify(cert, &self.members().await?).ok_or_else(|| {
            error(
                ErrorCode::Forbidden,
                "the certificate names no current cluster member",
            )
        })
    }

    /// The router for port 8444.
    pub fn router(self: &Arc<Self>) -> Router {
        Router::new()
            .route(STREAM_PATH, get(serve_stream))
            .route(ANSWER_PATH, post(serve_answer))
            .route(JOIN_PATH, post(serve_join))
            .route(ENROLL_PATH, post(serve_enroll))
            .route(HOLDINGS_PATH, get(catchup::serve_holdings))
            .route(RECORDS_PATH, get(catchup::serve_records))
            .route(
                &format!("{}{{sha}}", bmac_proto::updates::BUNDLE_PATH),
                get(updates::serve_bundle),
            )
            .route(
                bmac_proto::updates::INSTALL_PATH,
                post(updates::serve_install),
            )
            .route(bmac_proto::updates::STATE_PATH, get(updates::serve_state))
            .fallback(serve_other)
            .with_state(self.clone())
    }

    /// Serves port 8444 on the addresses `listen` names, until aborted.
    pub fn serve(
        self: &Arc<Self>,
        mut listen: watch::Receiver<Vec<SocketAddr>>,
    ) -> tokio::task::JoinHandle<()> {
        let server = HttpsServer::new(self.config.tls.server.clone(), self.router());
        tokio::spawn(async move {
            let mut listeners = Listeners::new(server);
            loop {
                let wanted: Vec<SocketAddr> = listen
                    .borrow_and_update()
                    .iter()
                    .map(|a| SocketAddr::new(a.ip(), PEER_PORT))
                    .collect();
                for (address, e) in listeners.reconcile(&wanted) {
                    log::warning(&format!("peers: cannot listen on {address}: {e}"));
                }
                tokio::select! {
                    changed = listen.changed() => if changed.is_err() { return },
                    _ = tokio::time::sleep(REBIND_EVERY) => {}
                }
            }
        })
    }

    /// Sends this host's records from `after`, a page at a time, and
    /// returns the last sequence sent; `None` once the peer is gone.
    async fn send_from(
        &self,
        tx: &mpsc::Sender<Bytes>,
        stream: &str,
        mut after: u64,
    ) -> Option<u64> {
        loop {
            let page = match self
                .store
                .records_after(self.store.origin(), stream, after, PAGE)
                .await
            {
                Ok(page) => page,
                Err(e) => {
                    log::warning(&format!("peer stream: reading records: {e}"));
                    return Some(after);
                }
            };
            let full = page.len() == PAGE;
            for record in page {
                after = record.seq;
                tx.send(line(&PeerFrame::Record { record })).await.ok()?;
            }
            if !full {
                return Some(after);
            }
        }
    }

    async fn feed(
        self: Arc<Self>,
        asked: Option<String>,
        after: u64,
        tx: mpsc::Sender<Bytes>,
    ) -> Option<()> {
        let mut live = self.store.subscribe();
        let mut outbox = self.hub.outbox();
        let current = self.store.stream().to_string();
        tx.send(line(&PeerFrame::Hello {
            host: self.config.host,
            stream: current.clone(),
        }))
        .await
        .ok()?;
        for (collector, data) in self.hub.observations_of(self.config.host) {
            tx.send(line(&PeerFrame::Observation { collector, data }))
                .await
                .ok()?;
        }
        let later = match &asked {
            Some(asked) => self.store.later_streams(asked).await.unwrap_or_else(|e| {
                log::warning(&format!("peer stream: reading streams: {e}"));
                None
            }),
            None => None,
        };
        let mut last = match (asked, later) {
            (Some(asked), Some(later)) => {
                let mut last = self.send_from(&tx, &asked, after).await?;
                for stream in later {
                    last = self.send_from(&tx, &stream, 0).await?;
                }
                last
            }
            _ => self.send_from(&tx, &current, 0).await?,
        };
        let mut keepalive = tokio::time::interval(self.config.keepalive);
        keepalive.tick().await;
        loop {
            tokio::select! {
                record = live.recv() => match record {
                    Ok(record) if record.seq <= last => {}
                    Ok(record) if record.seq == last + 1 => {
                        last = record.seq;
                        tx.send(line(&PeerFrame::Record { record: (*record).clone() })).await.ok()?;
                    }
                    Ok(_) | Err(broadcast::error::RecvError::Lagged(_)) => {
                        last = self.send_from(&tx, &current, last).await?;
                    }
                    Err(broadcast::error::RecvError::Closed) => return Some(()),
                },
                change = outbox.recv() => match change {
                    Ok(StreamBody::Job { job }) => {
                        tx.send(line(&PeerFrame::Job { job })).await.ok()?;
                    }
                    Ok(StreamBody::Observation { host, collector, data }) if host == self.config.host => {
                        tx.send(line(&PeerFrame::Observation { collector, data })).await.ok()?;
                    }
                    Ok(_) | Err(broadcast::error::RecvError::Lagged(_)) => {}
                    Err(broadcast::error::RecvError::Closed) => return Some(()),
                },
                _ = keepalive.tick() => {
                    tx.send(line(&PeerFrame::Keepalive)).await.ok()?;
                }
            }
        }
    }

    async fn connect(
        &self,
        host: HostSlot,
    ) -> Result<hyper::client::conn::http1::SendRequest<Full<Bytes>>, String> {
        let membership = self.membership().await.map_err(|e| e.message)?;
        let addresses = (self.config.resolve)(host, &membership);
        if addresses.is_empty() {
            return Err(format!("no address is known for {host}"));
        }
        let connector = TlsConnector::from(self.config.tls.client.clone());
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
            match tokio::time::timeout(self.config.connect_timeout, attempt).await {
                Ok(Ok(sender)) => return Ok(sender),
                Ok(Err(e)) => last = format!("{address}: {e}"),
                Err(_) => last = format!("{address}: timed out"),
            }
        }
        Err(last)
    }

    fn request(
        host: HostSlot,
        method: &str,
        path: &str,
        body: Bytes,
    ) -> hyper::Request<Full<Bytes>> {
        hyper::Request::builder()
            .method(method)
            .uri(path)
            .header(header::HOST, host.to_string())
            .header(header::CONTENT_TYPE, "application/json")
            .body(Full::new(body))
            .expect("requests are well formed")
    }

    /// Sends a JSON request to a peer and reads its JSON answer.
    async fn call(&self, host: HostSlot, path: &str, body: Bytes) -> Result<(), ApiError> {
        let unreachable = |e: String| {
            error(
                ErrorCode::Unavailable,
                format!("{host} is unreachable: {e}"),
            )
        };
        let mut sender = self.connect(host).await.map_err(unreachable)?;
        let response = sender
            .send_request(Mesh::request(host, "POST", path, body))
            .await
            .map_err(|e| unreachable(e.to_string()))?;
        let status = response.status();
        let bytes = response
            .into_body()
            .collect()
            .await
            .map_err(|e| unreachable(e.to_string()))?
            .to_bytes();
        if status.is_success() {
            return Ok(());
        }
        Err(serde_json::from_slice::<ApiError>(&bytes)
            .unwrap_or_else(|_| error(ErrorCode::Internal, format!("{host} answered {status}"))))
    }

    /// Keeps a link open to every other member until aborted.
    pub fn spawn_links(self: &Arc<Self>) -> tokio::task::JoinHandle<()> {
        let mesh = self.clone();
        tokio::spawn(async move {
            let mut links = Links(HashMap::new());
            let mut tick = tokio::time::interval(mesh.config.links_every);
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tick.tick().await;
                let Ok(membership) = mesh.membership().await else {
                    continue;
                };
                let wanted: BTreeSet<HostSlot> = membership
                    .members
                    .into_keys()
                    .filter(|h| *h != mesh.config.host)
                    .collect();
                links.0.retain(|host, task| {
                    let keep = wanted.contains(host) && !task.is_finished();
                    if !keep {
                        task.abort();
                    }
                    keep
                });
                for host in wanted {
                    links
                        .0
                        .entry(host)
                        .or_insert_with(|| tokio::spawn(mesh.clone().link(host)));
                }
            }
        })
    }

    fn now(&self) -> i64 {
        (self.clock)()
    }

    fn reachability(&self, host: HostSlot, reachable: bool, last_seen_ms: i64) {
        let mut peers = self.reachable.lock().unwrap();
        if reachable {
            peers.insert(host);
        } else {
            peers.remove(&host);
        }
        drop(peers);
        self.hub.publish(StreamBody::Host {
            host,
            reachable,
            last_seen_ms,
        });
    }

    async fn link(self: Arc<Self>, peer: HostSlot) {
        let mut backoff = Duration::from_secs(1);
        let mut last_error = String::new();
        let mut last_seen = 0;
        loop {
            let started = Instant::now();
            let outcome = self.pull(peer, &mut last_seen).await;
            if last_seen > 0 {
                self.reachability(peer, false, last_seen);
            }
            match outcome {
                Ok(()) => last_error.clear(),
                Err(e) if e != last_error => {
                    log::warning(&format!("link to {peer}: {e}"));
                    last_error = e;
                }
                Err(_) => {}
            }
            if started.elapsed() > Duration::from_secs(30) {
                backoff = Duration::from_secs(1);
            }
            tokio::time::sleep(backoff).await;
            backoff = (backoff * 2).min(self.config.retry_max);
        }
    }

    /// Pulls one peer's stream until it drops.
    async fn pull(&self, peer: HostSlot, last_seen: &mut i64) -> Result<(), String> {
        let origin = peer.to_string();
        let stream = self
            .store
            .latest_stream(&origin)
            .await
            .map_err(|e| e.to_string())?;
        let after = match &stream {
            Some(stream) => self
                .store
                .last_seq(&origin, stream)
                .await
                .map_err(|e| e.to_string())?,
            None => 0,
        };
        let path = match &stream {
            Some(stream) => format!("{STREAM_PATH}?stream={stream}&after={after}"),
            None => STREAM_PATH.to_string(),
        };
        let mut sender = self.connect(peer).await?;
        let response = sender
            .send_request(Mesh::request(peer, "GET", &path, Bytes::new()))
            .await
            .map_err(|e| e.to_string())?;
        if !response.status().is_success() {
            let status = response.status();
            let body = response
                .into_body()
                .collect()
                .await
                .map(|b| b.to_bytes())
                .unwrap_or_default();
            let message = serde_json::from_slice::<ApiError>(&body)
                .map(|e| e.message)
                .unwrap_or_default();
            return Err(format!("refused the stream ({status}): {message}"));
        }
        let mut body = response.into_body();
        let mut buffer: Vec<u8> = Vec::new();
        let mut greeted = false;
        loop {
            let frame = match tokio::time::timeout(self.config.idle_timeout, body.frame()).await {
                Err(_) => return Err("the stream went silent".into()),
                Ok(None) => return Ok(()),
                Ok(Some(Err(e))) => return Err(e.to_string()),
                Ok(Some(Ok(frame))) => frame,
            };
            let Ok(data) = frame.into_data() else {
                continue;
            };
            buffer.extend_from_slice(&data);
            let Some(end) = buffer.iter().rposition(|b| *b == b'\n') else {
                if buffer.len() > MAX_LINE {
                    return Err("a line is too long".into());
                }
                continue;
            };
            let complete: Vec<u8> = buffer.drain(..=end).collect();
            let mut records: Vec<Record> = Vec::new();
            for text in complete.split(|b| *b == b'\n').filter(|l| !l.is_empty()) {
                let frame: PeerFrame =
                    serde_json::from_slice(text).map_err(|e| format!("bad frame: {e}"))?;
                if !greeted {
                    match frame {
                        PeerFrame::Hello { host, .. } if host == peer => {
                            greeted = true;
                            self.catch_up.wake();
                            *last_seen = self.now();
                            self.reachability(peer, true, *last_seen);
                            continue;
                        }
                        _ => return Err("the stream didn't start with its host's hello".into()),
                    }
                }
                *last_seen = self.now();
                match frame {
                    PeerFrame::Record { record } => {
                        if record.origin != origin {
                            return Err(format!("it sent a record of {}", record.origin));
                        }
                        records.push(record);
                    }
                    other => {
                        self.store_copies(std::mem::take(&mut records)).await?;
                        self.apply(peer, other);
                    }
                }
            }
            self.store_copies(records).await?;
        }
    }

    async fn store_copies(&self, records: Vec<Record>) -> Result<(), String> {
        if records.is_empty() {
            return Ok(());
        }
        let stored = self
            .store
            .insert_copies(records)
            .await
            .map_err(|e: StoreError| e.to_string())?;
        for record in stored {
            if let Some(event) = record.job_event() {
                self.hub.publish_copy(StreamBody::JobEvent { event });
            }
        }
        Ok(())
    }

    fn apply(&self, peer: HostSlot, frame: PeerFrame) {
        match frame {
            PeerFrame::Job { job } => {
                self.hub.publish_copy(StreamBody::Job { job });
            }
            PeerFrame::Observation { collector, data } => {
                self.hub.publish_copy(StreamBody::Observation {
                    host: peer,
                    collector,
                    data,
                });
            }
            PeerFrame::Hello { .. } | PeerFrame::Record { .. } | PeerFrame::Keepalive => {}
        }
    }
}

/// The link tasks, stopped together.
struct Links(HashMap<HostSlot, tokio::task::JoinHandle<()>>);

impl Drop for Links {
    fn drop(&mut self) {
        for task in self.0.values() {
            task.abort();
        }
    }
}

impl AnswerRelay for Mesh {
    fn forward(
        &self,
        to: HostSlot,
        answer: RelayedAnswer,
    ) -> Pin<Box<dyn Future<Output = Result<(), ApiError>> + Send + '_>> {
        Box::pin(async move {
            let request = AnswerRequest {
                job: answer.job,
                request_id: answer.request_id,
                user: answer.user,
                role: answer.role,
                answer: answer.answer,
            };
            let body = Bytes::from(serde_json::to_vec(&request).expect("answers serialize"));
            self.call(to, ANSWER_PATH, body).await
        })
    }
}

async fn serve_stream(
    State(mesh): State<Arc<Mesh>>,
    axum::Extension(certificate): axum::Extension<ClientCertificate>,
    uri: Uri,
) -> Response {
    if let Err(e) = mesh.member(&certificate).await {
        return api_error(e);
    }
    let (stream, after) = match stream_query(&uri) {
        Ok(query) => query,
        Err(e) => return api_error(e),
    };
    let (tx, rx) = mpsc::channel(64);
    tokio::spawn(mesh.clone().feed(stream, after, tx));
    let mut response = Response::new(Body::new(ChannelBody(rx)));
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        header::HeaderValue::from_static("application/x-ndjson"),
    );
    response
}

async fn serve_answer(
    State(mesh): State<Arc<Mesh>>,
    axum::Extension(certificate): axum::Extension<ClientCertificate>,
    body: Bytes,
) -> Response {
    if let Err(e) = mesh.member(&certificate).await {
        return api_error(e);
    }
    let request: AnswerRequest = match parse(&body) {
        Ok(request) => request,
        Err(e) => return api_error(e),
    };
    let Some(runner) = mesh.runner.get() else {
        return api_error(error(ErrorCode::Unavailable, "this host runs no jobs"));
    };
    let relayed = RelayedAnswer {
        user: request.user,
        role: request.role,
        job: request.job,
        request_id: request.request_id,
        answer: request.answer,
    };
    match runner.receive(relayed).await {
        Ok(()) => ok_json(serde_json::json!({})),
        Err(e) => api_error(e),
    }
}

async fn serve_join(
    State(mesh): State<Arc<Mesh>>,
    axum::Extension(ClientAddr(client)): axum::Extension<ClientAddr>,
    body: Bytes,
) -> Response {
    let request: JoinRequest = match parse(&body) {
        Ok(request) => request,
        Err(e) => return api_error(e),
    };
    let tokens = mesh.tokens.clone();
    match tokio::task::spawn_blocking(move || tokens.redeem_join(&request, client.ip())).await {
        Ok(Ok(pending)) => ok_json(pending),
        Ok(Err(e)) => api_error(e),
        Err(e) => api_error(error(ErrorCode::Internal, e.to_string())),
    }
}

async fn serve_enroll(
    State(mesh): State<Arc<Mesh>>,
    axum::Extension(ClientAddr(client)): axum::Extension<ClientAddr>,
    body: Bytes,
) -> Response {
    let request: EnrollRequest = match parse(&body) {
        Ok(request) => request,
        Err(e) => return api_error(e),
    };
    let tokens = mesh.tokens.clone();
    match tokio::task::spawn_blocking(move || tokens.redeem_enroll(&request, client.ip())).await {
        Ok(Ok(pending)) => ok_json(pending),
        Ok(Err(e)) => api_error(e),
        Err(e) => api_error(error(ErrorCode::Internal, e.to_string())),
    }
}

async fn serve_other(
    State(mesh): State<Arc<Mesh>>,
    axum::Extension(certificate): axum::Extension<ClientCertificate>,
) -> Response {
    match mesh.member(&certificate).await {
        Ok(_) => api_error(error(ErrorCode::NotFound, "no such path")),
        Err(e) => api_error(e),
    }
}
