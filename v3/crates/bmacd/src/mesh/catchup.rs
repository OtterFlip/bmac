// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

//! Catch-up (`MONITORING.md`, "Copying logs between hosts"): every minute,
//! and whenever a link connects, this host reads each peer's holdings for
//! every origin, not just the peer's own, and fetches the records it lacks,
//! so a rebuilt host gets a dead host's records from a third. Each copy
//! carries the hash before it, so holes fill in any order. Fetches share one
//! bandwidth cap.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::extract::State;
use axum::http::Uri;
use axum::response::Response;
use bmac_core::HostSlot;
use bmac_proto::api::ErrorCode;
use bmac_proto::logs::{PeerOrigin, PeerReplication};
use bmac_proto::peer::{HOLDINGS_PATH, Holdings, RECORDS_PATH, Record, StreamHolding};
use bmac_proto::status::PeerLag;
use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use tokio::sync::Notify;

use super::{Mesh, error, ok_json};
use crate::log;
use crate::records;
use crate::web::api::api_error;
use crate::web::https::ClientCertificate;

/// Records per `GET /v1/records`.
const FETCH: usize = 500;
/// About the most body bytes one answer carries.
const ANSWER_BYTES: usize = 4 << 20;
/// Holdings older than this mean the peer is not answering.
const FRESH: Duration = Duration::from_secs(180);

#[derive(Debug, Clone, Default)]
struct PeerHoldings {
    checked_ms: Option<i64>,
    checked: Option<Instant>,
    error: Option<String>,
    streams: Vec<StreamHolding>,
}

/// A token bucket over bytes, allowing one burst of a second's worth.
#[derive(Debug)]
struct Bucket {
    rate: f64,
    state: tokio::sync::Mutex<(f64, tokio::time::Instant)>,
}

impl Bucket {
    fn new(rate: u64) -> Bucket {
        let rate = rate.max(1) as f64;
        Bucket {
            rate,
            state: tokio::sync::Mutex::new((rate, tokio::time::Instant::now())),
        }
    }

    async fn take(&self, bytes: usize) {
        let wait = {
            let mut state = self.state.lock().await;
            let (tokens, last) = &mut *state;
            *tokens = (*tokens + last.elapsed().as_secs_f64() * self.rate).min(self.rate);
            *last = tokio::time::Instant::now();
            *tokens -= bytes as f64;
            (*tokens < 0.0).then(|| Duration::from_secs_f64(-*tokens / self.rate))
        };
        if let Some(wait) = wait {
            tokio::time::sleep(wait).await;
        }
    }
}

#[derive(Debug)]
pub(super) struct CatchUp {
    peers: Mutex<BTreeMap<HostSlot, PeerHoldings>>,
    wake: Notify,
    bucket: Bucket,
}

impl CatchUp {
    pub(super) fn new(bytes_per_sec: u64) -> CatchUp {
        CatchUp {
            peers: Mutex::new(BTreeMap::new()),
            wake: Notify::new(),
            bucket: Bucket::new(bytes_per_sec),
        }
    }

    pub(super) fn wake(&self) {
        self.wake.notify_one();
    }
}

type Sender = hyper::client::conn::http1::SendRequest<Full<Bytes>>;

/// A name for a query: letters, digits, `-`, and `.`.
fn name(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 64
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'.')
}

/// `origin`, `stream`, `after`, and `limit` from a records request.
fn records_query(uri: &Uri) -> Result<(String, String, u64, usize), bmac_proto::api::ApiError> {
    let (mut origin, mut stream, mut after, mut limit) = (None, None, 0, FETCH);
    for pair in uri
        .query()
        .unwrap_or("")
        .split('&')
        .filter(|p| !p.is_empty())
    {
        let invalid = || error(ErrorCode::Invalid, format!("bad query parameter {pair:?}"));
        match pair.split_once('=') {
            Some(("origin", value)) if name(value) => origin = Some(value.to_string()),
            Some(("stream", value)) if name(value) => stream = Some(value.to_string()),
            Some(("after", value)) => after = value.parse().map_err(|_| invalid())?,
            Some(("limit", value)) => {
                limit = value
                    .parse::<usize>()
                    .map_err(|_| invalid())?
                    .clamp(1, FETCH)
            }
            _ => return Err(invalid()),
        }
    }
    match (origin, stream) {
        (Some(origin), Some(stream)) => Ok((origin, stream, after, limit)),
        _ => Err(error(ErrorCode::Invalid, "origin and stream are required")),
    }
}

pub(super) async fn serve_holdings(
    State(mesh): State<Arc<Mesh>>,
    axum::Extension(certificate): axum::Extension<ClientCertificate>,
) -> Response {
    if let Err(e) = mesh.member(&certificate).await {
        return api_error(e);
    }
    match records::holdings(&mesh.store).await {
        Ok(streams) => ok_json(Holdings {
            host: mesh.config.host,
            streams,
        }),
        Err(e) => api_error(error(ErrorCode::Internal, e.to_string())),
    }
}

pub(super) async fn serve_records(
    State(mesh): State<Arc<Mesh>>,
    axum::Extension(certificate): axum::Extension<ClientCertificate>,
    uri: Uri,
) -> Response {
    if let Err(e) = mesh.member(&certificate).await {
        return api_error(e);
    }
    let (origin, stream, after, limit) = match records_query(&uri) {
        Ok(query) => query,
        Err(e) => return api_error(e),
    };
    match mesh
        .store
        .records_after(&origin, &stream, after, limit)
        .await
    {
        Ok(mut records) => {
            let mut bytes = 0;
            let keep = records
                .iter()
                .position(|r| {
                    bytes += r.body.len();
                    bytes > ANSWER_BYTES
                })
                .map_or(records.len(), |at| at.max(1));
            records.truncate(keep);
            ok_json(records)
        }
        Err(e) => api_error(error(ErrorCode::Internal, e.to_string())),
    }
}

impl Mesh {
    /// Runs catch-up with every other member until aborted.
    pub fn spawn_catch_up(self: &Arc<Self>) -> tokio::task::JoinHandle<()> {
        let mesh = self.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(mesh.config.catch_up_every);
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tokio::select! {
                    _ = tick.tick() => {}
                    _ = mesh.catch_up.wake.notified() => {}
                }
                mesh.catch_up_all().await;
            }
        })
    }

    /// One round of catch-up with every other member.
    pub async fn catch_up_all(&self) {
        let Ok(members) = self.members().await else {
            return;
        };
        self.catch_up
            .peers
            .lock()
            .unwrap()
            .retain(|host, _| members.contains(host));
        for peer in members.into_iter().filter(|h| *h != self.config.host) {
            let outcome = self.catch_up_with(peer).await;
            let mut peers = self.catch_up.peers.lock().unwrap();
            let held = peers.entry(peer).or_default();
            match outcome {
                Ok(()) => held.error = None,
                Err(e) => {
                    if held.error.as_ref() != Some(&e) {
                        log::warning(&format!("catch-up with {peer}: {e}"));
                    }
                    held.error = Some(e);
                }
            }
        }
    }

    async fn get_json<T: serde::de::DeserializeOwned>(
        sender: &mut Sender,
        peer: HostSlot,
        path: &str,
    ) -> Result<(T, usize), String> {
        sender.ready().await.map_err(|e| e.to_string())?;
        let response = sender
            .send_request(Mesh::request(peer, "GET", path, Bytes::new()))
            .await
            .map_err(|e| e.to_string())?;
        let status = response.status();
        let body = response
            .into_body()
            .collect()
            .await
            .map_err(|e| e.to_string())?
            .to_bytes();
        if !status.is_success() {
            let message = serde_json::from_slice::<bmac_proto::api::ApiError>(&body)
                .map(|e| e.message)
                .unwrap_or_default();
            return Err(format!("{path} answered {status}: {message}"));
        }
        let value = serde_json::from_slice(&body).map_err(|e| format!("{path}: {e}"))?;
        Ok((value, body.len()))
    }

    async fn catch_up_with(&self, peer: HostSlot) -> Result<(), String> {
        let mut sender = self.connect(peer).await?;
        let (holdings, _): (Holdings, _) = Self::get_json(&mut sender, peer, HOLDINGS_PATH).await?;
        if holdings.host != peer {
            return Err(format!("it answered as {}", holdings.host));
        }
        {
            let mut peers = self.catch_up.peers.lock().unwrap();
            let held = peers.entry(peer).or_default();
            held.checked_ms = Some(self.now());
            held.checked = Some(Instant::now());
            held.streams = holdings.streams.clone();
        }
        let kept_from = self.store.kept_from();
        for held in holdings.streams {
            if held.newest_ms < kept_from
                || (held.origin == self.store.origin() && held.stream == self.store.stream())
                || !name(&held.origin)
                || !name(&held.stream)
            {
                continue;
            }
            let missing = records::missing(
                &self.store,
                &held.origin,
                &held.stream,
                held.min_seq,
                held.max_seq,
            )
            .await
            .map_err(|e| e.to_string())?;
            for (from, to) in missing {
                let mut after = from - 1;
                while after < to {
                    let limit = ((to - after) as usize).min(FETCH);
                    let path = format!(
                        "{RECORDS_PATH}?origin={}&stream={}&after={after}&limit={limit}",
                        held.origin, held.stream
                    );
                    let (records, bytes): (Vec<Record>, _) =
                        Self::get_json(&mut sender, peer, &path).await?;
                    let Some(last) = records.last().map(|r| r.seq) else {
                        break;
                    };
                    if last <= after
                        || records
                            .iter()
                            .any(|r| r.origin != held.origin || r.stream != held.stream)
                    {
                        return Err(format!("{path} answered other records"));
                    }
                    self.catch_up.bucket.take(bytes).await;
                    let wanted: Vec<Record> = records.into_iter().filter(|r| r.seq <= to).collect();
                    if let Err(e) = self.store_copies(wanted).await {
                        log::warning(&format!(
                            "catch-up with {peer}: {} {}: {e}",
                            held.origin, held.stream
                        ));
                        break;
                    }
                    after = last;
                }
            }
        }
        Ok(())
    }

    /// How each peer's copies compare with this host's, from the holdings
    /// catch-up last read.
    pub async fn replication(&self) -> Vec<PeerReplication> {
        let peers = self.catch_up.peers.lock().unwrap().clone();
        let mine = records::origin_holdings(&self.store)
            .await
            .unwrap_or_default();
        let now = self.now();
        let mut out = Vec::new();
        for (peer, held) in peers {
            let mut origins = Vec::new();
            for origin in &mine {
                let Some(stream) = &origin.latest_stream else {
                    continue;
                };
                let theirs = held
                    .streams
                    .iter()
                    .find(|h| h.origin == origin.origin && &h.stream == stream);
                let last_seq = theirs.map_or(0, |h| h.max_seq);
                let behind_ms = if origin.last_seq > last_seq {
                    self.store
                        .records_after(&origin.origin, stream, last_seq, 1)
                        .await
                        .ok()
                        .and_then(|r| r.first().map(|r| (now - r.time_ms).max(0)))
                } else {
                    None
                };
                origins.push(PeerOrigin {
                    origin: origin.origin.clone(),
                    last_seq,
                    behind_records: origin.last_seq as i64 - last_seq as i64,
                    behind_ms,
                    gaps: theirs.map_or(0, |h| (h.max_seq - h.min_seq + 1).saturating_sub(h.count)),
                });
            }
            out.push(PeerReplication {
                peer,
                checked_ms: held.checked_ms,
                error: held.error.clone(),
                origins,
            });
        }
        out
    }

    /// Peers answering catch-up whose copy of this host's current stream
    /// lags, and how many peers answered.
    pub async fn lagging(&self) -> (Vec<PeerLag>, u32) {
        let own = self.store.origin().to_string();
        let mut lagging = Vec::new();
        let mut checked = 0;
        let fresh: Vec<HostSlot> = self
            .catch_up
            .peers
            .lock()
            .unwrap()
            .iter()
            .filter(|(_, h)| h.error.is_none() && h.checked.is_some_and(|t| t.elapsed() < FRESH))
            .map(|(peer, _)| *peer)
            .collect();
        for peer in self.replication().await {
            if !fresh.contains(&peer.peer) {
                continue;
            }
            checked += 1;
            if let Some(origin) = peer.origins.iter().find(|o| o.origin == own)
                && origin.behind_records > 0
            {
                lagging.push(PeerLag {
                    peer: peer.peer,
                    behind_records: origin.behind_records as u64,
                    behind_ms: origin.behind_ms.unwrap_or(0),
                });
            }
        }
        (lagging, checked)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_queries_need_an_origin_and_stream_and_cap_the_limit() {
        let uri: Uri = "/v1/records?origin=mox1&stream=19a-ff&after=3&limit=99999"
            .parse()
            .unwrap();
        assert_eq!(
            records_query(&uri).unwrap(),
            ("mox1".into(), "19a-ff".into(), 3, FETCH)
        );
        for bad in [
            "/v1/records?origin=mox1",
            "/v1/records?origin=../x&stream=a",
            "/v1/records?origin=a&stream=b&after=-1",
            "/v1/records?origin=a&stream=b&grep=1",
        ] {
            assert!(records_query(&bad.parse().unwrap()).is_err(), "{bad}");
        }
    }

    #[tokio::test]
    async fn the_bucket_paces_bytes_after_a_burst() {
        let bucket = Bucket::new(10_000);
        let started = tokio::time::Instant::now();
        bucket.take(10_000).await;
        assert!(started.elapsed() < Duration::from_millis(50));
        bucket.take(1_000).await;
        assert!(started.elapsed() >= Duration::from_millis(90));
    }
}
