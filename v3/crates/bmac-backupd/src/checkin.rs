// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

//! One cluster, as the appliance sees it (`BACKUP.md`, "Check-ins"): a
//! check-in every interval the cluster asks for, the PIT index when it
//! wants it, and the offers handed to the puller, which runs apart so a
//! long pull never delays a check-in.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bmac_exec::BoxFuture;
use bmac_proto::appliance::{
    ApplianceProgress, Capacity, CheckinReply, CheckinRequest, ClaimRequest, FailedReport,
    HeadReport, Heartbeat, IndexRequest, IndexedPit, LuksHeader, LuksHeaderRequest, LuksHeaders,
    OfferedPit, PeerIndex, RecoveryManifest, SendRequest, SourceHost, on_appliance, on_source,
};
use serde::Serialize;
use tokio::sync::Notify;

use crate::audit::Audit;
use crate::client::{CallError, ClusterClient};
use crate::daemon::EXPORT_PORT;
use crate::journal::{Journal, Layout, Metadata};
use crate::peers::Peers;
use crate::pool::{Pool, Stream};
use crate::pull::{Link, Progress, Published, PullError, Puller};
use crate::recovery;
use crate::repair::{self, PeerLink};
use crate::roots::Roots;
use crate::scrub::{IoGate, ScrubState};
use crate::state::{self, Enrollment, Identity, Paths};
use crate::watchdog::Round;

const DEFAULT_INTERVAL: Duration = Duration::from_secs(60);
/// How long the puller waits before trying a failed offer again.
const RETRY_AFTER: Duration = Duration::from_secs(60);
const HEARTBEAT_TIMEOUT: Duration = Duration::from_secs(20);

/// What the UI shows of a cluster.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct ClusterStatus {
    pub cluster_uuid: String,
    pub cluster_name: String,
    /// The appliance's state as the cluster last said, such as `ACTIVE`.
    pub state: Option<String>,
    pub last_checkin_ms: Option<i64>,
    pub last_error: Option<String>,
    pub offers: usize,
    pub pulling: Option<String>,
}

pub struct Agent {
    pub paths: Paths,
    pub identity: Identity,
    pub client: ClusterClient,
    pub enrollment: Mutex<Enrollment>,
    pub layout: Layout,
    pub metadata: Metadata,
    pub audit: Arc<Audit>,
    pub pool: Arc<dyn Pool>,
    /// Whether the bound pool is imported, checked before every check-in.
    pub unlocked: Arc<dyn Fn() -> BoxFuture<'static, bool> + Send + Sync>,
    pub progress: Progress,
    pub failed: Mutex<Vec<FailedReport>>,
    pub status: Mutex<ClusterStatus>,
    pub offers: Mutex<Vec<OfferedPit>>,
    pub offered: Notify,
    pub wake: Notify,
    pub now_ms: Arc<dyn Fn() -> i64 + Send + Sync>,
    /// The appliance's certificate and key, PEM, for connections to peers.
    pub tls_identity: (String, String),
    /// Set while a pull repairs from a peer, for the check-in.
    pub repairing: AtomicBool,
    pub io: Arc<IoGate>,
    /// When the recovery manifest and LUKS headers were last pulled.
    pub recovery_pulled: Mutex<Option<i64>>,
}

impl Agent {
    fn cluster_uuid(&self) -> String {
        self.enrollment
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .cluster_uuid
            .clone()
    }

    fn hosts(&self) -> Vec<SourceHost> {
        self.enrollment
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .hosts
            .clone()
    }

    fn journal(&self) -> Option<Journal> {
        self.metadata.load().ok().flatten()
    }

    /// Calls the hosts in turn until one answers; a refusal is final.
    async fn call<Q: Serialize + Sync, A: serde::de::DeserializeOwned>(
        &self,
        method: &str,
        path: &str,
        body: Option<&Q>,
    ) -> Result<A, CallError> {
        let mut hosts = self.hosts();
        let mut last = CallError::Unreachable("no host is known".into());
        // Rotate so a dead first host doesn't slow every call.
        let start = (self.now_ms)().unsigned_abs() as usize % hosts.len().max(1);
        hosts.rotate_left(start);
        for host in &hosts {
            match self.client.call(host, method, path, body).await {
                Ok(answer) => return Ok(answer),
                Err(CallError::Refused(e)) => return Err(CallError::Refused(e)),
                Err(e) => last = e,
            }
        }
        Err(last)
    }

    pub async fn request(&self, locked: bool) -> CheckinRequest {
        let cluster = self.cluster_uuid();
        let journal = if locked { None } else { self.journal() };
        let heads = journal
            .as_ref()
            .map(|j| {
                j.heads(&cluster)
                    .values()
                    .map(|p| HeadReport {
                        vm_uuid: p.vm_uuid,
                        branch_uuid: p.branch_uuid,
                        pit_uuid: p.pit_uuid,
                    })
                    .collect()
            })
            .unwrap_or_default();
        let (health, capacity) = if locked {
            ("LOCKED".to_string(), Capacity::default())
        } else {
            let status = self.pool.status().await.unwrap_or_default();
            let (quota, used) = self
                .pool
                .quota(&self.layout.namespace(&cluster))
                .await
                .unwrap_or((0, 0));
            (
                status.health,
                Capacity {
                    pool_size_bytes: status.size_bytes,
                    pool_free_bytes: status.free_bytes,
                    quota_bytes: (quota > 0).then_some(quota),
                    used_bytes: used,
                },
            )
        };
        CheckinRequest {
            appliance_uuid: self.identity.appliance_uuid,
            version: env!("CARGO_PKG_VERSION").to_string(),
            locked,
            pool_health: health,
            capacity,
            progress: if self.repairing.load(Ordering::Relaxed) {
                ApplianceProgress::Repairing
            } else {
                ApplianceProgress::Normal
            },
            heads,
            transfers: self
                .progress
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .clone(),
            failed: self
                .failed
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .clone(),
            index_revision: journal.map(|j| j.revision).unwrap_or(0),
            scrub: if locked {
                None
            } else {
                ScrubState::load(&self.metadata)
                    .ok()
                    .and_then(|s| s.report_for(&cluster))
            },
        }
    }

    /// The watchdog's round: every host's heartbeat at once. A host that
    /// refuses this appliance still answered.
    pub async fn round(&self) -> Round {
        let hosts = self.hosts();
        let mut calls = tokio::task::JoinSet::new();
        for host in hosts.clone() {
            let client = self.client.clone();
            calls.spawn(async move {
                let answer: Result<Heartbeat, CallError> =
                    match tokio::time::timeout(HEARTBEAT_TIMEOUT, async {
                        client
                            .call::<(), Heartbeat>(&host, "GET", on_source::HEARTBEAT, None)
                            .await
                    })
                    .await
                    {
                        Ok(answer) => answer,
                        Err(_) => Err(CallError::Unreachable("timed out".into())),
                    };
                (
                    host.host.to_string(),
                    matches!(answer, Ok(_) | Err(CallError::Refused(_))),
                )
            });
        }
        let mut answered = Vec::new();
        while let Some(Ok((host, ok))) = calls.join_next().await {
            if ok {
                answered.push(host);
            }
        }
        answered.sort();
        let status = self
            .status
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone();
        Round {
            cluster_uuid: status.cluster_uuid,
            cluster_name: status.cluster_name,
            dashboard_url: hosts
                .first()
                .and_then(|h| h.addresses.first())
                .map(|a| format!("https://{a}/"))
                .unwrap_or_default(),
            hosts: hosts.iter().map(|h| h.host.to_string()).collect(),
            answered,
        }
    }

    pub fn index(&self) -> Option<IndexRequest> {
        let cluster = self.cluster_uuid();
        let journal = self.journal()?;
        Some(IndexRequest {
            revision: journal.revision,
            pits: journal
                .pits
                .iter()
                .filter(|p| p.cluster_uuid == cluster)
                .map(|p| IndexedPit {
                    vm_uuid: p.vm_uuid.to_string(),
                    branch_uuid: p.branch_uuid.to_string(),
                    pit_uuid: p.pit_uuid.to_string(),
                    parent: p.parent.map(|u| u.to_string()),
                    name: p.name.clone(),
                    taken_s: p.taken_s,
                    bytes: p.bytes,
                })
                .collect(),
        })
    }

    /// One check-in; the interval the cluster asks for next.
    pub async fn checkin(&self) -> Duration {
        let locked = !(self.unlocked)().await;
        let request = self.request(locked).await;
        let sent_failed = request.failed.len();
        let reply: Result<CheckinReply, CallError> =
            self.call("POST", on_source::CHECKIN, Some(&request)).await;
        let now = (self.now_ms)();
        let mut status = self
            .status
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone();
        let interval = match reply {
            Err(e) => {
                status.last_error = Some(e.to_string());
                DEFAULT_INTERVAL
            }
            Ok(reply) => {
                if reply.cluster_uuid != self.cluster_uuid() {
                    status.last_error = Some(format!(
                        "the cluster answered as {}, not the one this appliance enrolled with",
                        reply.cluster_uuid
                    ));
                    *self.status.lock().unwrap_or_else(|p| p.into_inner()) = status;
                    return DEFAULT_INTERVAL;
                }
                {
                    let mut failed = self.failed.lock().unwrap_or_else(|p| p.into_inner());
                    let sent = sent_failed.min(failed.len());
                    failed.drain(..sent);
                }
                status.state = Some(reply.state.clone());
                status.last_checkin_ms = Some(now);
                status.last_error = None;
                status.offers = reply.offers.len();
                self.learn(&reply.offers);
                if reply.want_index
                    && !locked
                    && let Some(index) = self.index()
                {
                    let sent: Result<serde_json::Value, _> =
                        self.call("POST", on_source::INDEX, Some(&index)).await;
                    if let Err(e) = sent {
                        status.last_error = Some(format!("the PIT index: {e}"));
                    }
                }
                if !locked {
                    if let Err(e) = self.roots(&reply.branch_roots, now).await {
                        status.last_error = Some(format!("the branch roots: {e}"));
                    }
                    if let Err(e) = self.recovery(reply.recovery_revision.as_deref(), now).await {
                        status.last_error = Some(e);
                    }
                    if let Err(e) = self.tests(&reply.restore_tests).await {
                        status.last_error = Some(format!("the restore tests: {e}"));
                    }
                    *self.offers.lock().unwrap_or_else(|p| p.into_inner()) = reply.offers;
                    self.offered.notify_one();
                }
                Duration::from_secs(reply.checkin_interval_s.clamp(10, 3600) as u64)
            }
        };
        let previous = self
            .status
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone();
        let first = previous.last_checkin_ms.is_none() && status.last_checkin_ms.is_some();
        if status.last_error != previous.last_error || first {
            match &status.last_error {
                Some(e) => eprintln!("bmac-backupd: cluster {}: {e}", status.cluster_name),
                None => eprintln!(
                    "bmac-backupd: cluster {}: checked in ({})",
                    status.cluster_name,
                    status.state.as_deref().unwrap_or("?")
                ),
            }
        }
        status.pulling = previous.pulling;
        *self.status.lock().unwrap_or_else(|p| p.into_inner()) = status;
        interval
    }

    /// Remembers hosts the offers name, for later check-ins.
    async fn roots(&self, named: &[uuid::Uuid], now_ms: i64) -> std::io::Result<()> {
        let _guard = self.metadata.lock().await?;
        let mut roots = Roots::load(&self.metadata)?;
        if roots.named(&self.cluster_uuid(), named, now_ms) {
            roots.save(&self.metadata)?;
        }
        Ok(())
    }

    async fn tests(
        &self,
        reported: &[bmac_proto::appliance::RestoreTestReport],
    ) -> std::io::Result<()> {
        if reported.is_empty() {
            return Ok(());
        }
        let _guard = self.metadata.lock().await?;
        recovery::keep_tests(&self.metadata, &self.cluster_uuid(), reported).map(|_| ())
    }

    /// Pulls the recovery manifest when it changed or an hour went by,
    /// and each host's LUKS header backups hourly.
    async fn recovery(&self, revision: Option<&str>, now_ms: i64) -> Result<(), String> {
        let cluster = self.cluster_uuid();
        let last = *self
            .recovery_pulled
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        let kept = recovery::newest(&self.metadata, &cluster)
            .ok()
            .flatten()
            .map(|m| m.revision);
        if !recovery::due(last, revision, kept.as_deref(), now_ms) {
            return Ok(());
        }
        let hourly = last.is_none_or(|t| now_ms - t >= recovery::PULL_EVERY_MS);
        let manifest: RecoveryManifest = self
            .call::<(), _>("GET", on_source::RECOVERY_MANIFEST, None)
            .await
            .map_err(|e| format!("the recovery manifest: {e}"))?;
        {
            let _guard = self.metadata.lock().await.map_err(|e| e.to_string())?;
            recovery::keep_manifest(&self.metadata, &cluster, &manifest, now_ms)
                .map_err(|e| format!("the recovery manifest: {e}"))?;
        }
        if !hourly {
            return Ok(());
        }
        let mut problems = Vec::new();
        for host in self.hosts() {
            match self
                .client
                .call::<(), LuksHeaders>(&host, "GET", on_source::LUKS_HEADERS, None)
                .await
            {
                Ok(headers) if headers.host == host.host => {
                    for name in recovery::headers_wanted(&self.metadata, &cluster, &headers) {
                        let request = LuksHeaderRequest { name: name.clone() };
                        let header = match self
                            .client
                            .call::<_, LuksHeader>(
                                &host,
                                "POST",
                                on_source::LUKS_HEADER,
                                Some(&request),
                            )
                            .await
                        {
                            Ok(header) if header.host == host.host && header.name == name => header,
                            Ok(_) => {
                                problems
                                    .push(format!("{}: {name}: another header came", host.host));
                                continue;
                            }
                            Err(e) => {
                                problems.push(format!("{}: {name}: {e}", host.host));
                                continue;
                            }
                        };
                        let _guard = self.metadata.lock().await.map_err(|e| e.to_string())?;
                        if let Err(e) =
                            recovery::keep_header(&self.metadata, &cluster, &header, now_ms)
                        {
                            problems.push(format!("{}: {e}", host.host));
                        }
                    }
                }
                Ok(headers) => problems.push(format!(
                    "{} answered with {}'s headers",
                    host.host, headers.host
                )),
                Err(e) => problems.push(format!("{}: {e}", host.host)),
            }
        }
        *self
            .recovery_pulled
            .lock()
            .unwrap_or_else(|p| p.into_inner()) = Some(now_ms);
        if problems.is_empty() {
            Ok(())
        } else {
            Err(format!("the LUKS header backups: {}", problems.join("; ")))
        }
    }

    fn learn(&self, offers: &[OfferedPit]) {
        let mut enrollment = self.enrollment.lock().unwrap_or_else(|p| p.into_inner());
        let mut changed = false;
        for offer in offers {
            changed |= enrollment.learn(&offer.hosts);
        }
        if changed {
            let _ = state::save(
                &self.paths.enrollment(&enrollment.cluster_uuid),
                &*enrollment,
            );
        }
    }

    pub async fn run_checkins(self: Arc<Self>) {
        loop {
            let interval = self.checkin().await;
            tokio::select! {
                () = tokio::time::sleep(interval) => {}
                () = self.wake.notified() => {}
            }
        }
    }

    /// Pulls offers one at a time, newest first per VM as the cluster
    /// sends them; a failed offer waits [`RETRY_AFTER`].
    pub async fn run_pulls(self: Arc<Self>) {
        let mut waiting: BTreeMap<uuid::Uuid, std::time::Instant> = BTreeMap::new();
        loop {
            let offers = self
                .offers
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .clone();
            let mut pulled = false;
            for offer in offers {
                if self.io.pulls_wait() {
                    break;
                }
                if waiting
                    .get(&offer.pit_uuid)
                    .is_some_and(|at| at.elapsed() < RETRY_AFTER)
                {
                    continue;
                }
                pulled |= self.pull_one(&offer, &mut waiting).await;
            }
            if pulled {
                self.wake.notify_one();
            }
            tokio::select! {
                () = self.offered.notified() => {}
                () = tokio::time::sleep(RETRY_AFTER) => {}
            }
        }
    }

    async fn pull_one(
        &self,
        offer: &OfferedPit,
        waiting: &mut BTreeMap<uuid::Uuid, std::time::Instant>,
    ) -> bool {
        let cluster = self.cluster_uuid();
        let _pulling = self.io.start_pull();
        let label = format!("PIT {} of VM {}", offer.name, offer.vm_uuid);
        eprintln!(
            "bmac-backupd: pulling {label}{}",
            match &offer.base {
                Some(base) => format!(" from PIT {}", base.pit_uuid),
                None => " in full".into(),
            }
        );
        self.status
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .pulling = Some(label.clone());
        let now = self.now_ms.clone();
        let now_ms = move || now();
        let puller = Puller {
            pool: self.pool.as_ref(),
            link: self,
            metadata: &self.metadata,
            layout: &self.layout,
            cluster_uuid: &cluster,
            progress: self.progress.clone(),
            now_ms: &now_ms,
        };
        let (outcome, from_peer) = match self.repair(offer, &cluster, &now_ms).await {
            Some((published, peer)) => (Ok(published), Some(peer)),
            None => (puller.pull(offer).await, None),
        };
        self.status
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .pulling = None;
        let at = (self.now_ms)();
        match outcome {
            Ok(published) => {
                waiting.remove(&offer.pit_uuid);
                let seconds = published.duration_ms as f64 / 1000.0;
                let rate = if published.duration_ms == 0 {
                    0.0
                } else {
                    published.bytes as f64 / seconds / (1024.0 * 1024.0)
                };
                eprintln!(
                    "bmac-backupd: published {label}: {} bytes in {seconds:.1} s ({rate:.1} MiB/s){}",
                    published.bytes,
                    from_peer
                        .as_ref()
                        .map(|p| format!(", repaired from peer {p}"))
                        .unwrap_or_default()
                );
                let _ = self.audit.record(
                    at,
                    "publish",
                    "bmac-backupd",
                    serde_json::json!({
                        "cluster_uuid": cluster,
                        "vm_uuid": offer.vm_uuid,
                        "pit_uuid": offer.pit_uuid,
                        "branch_uuid": offer.branch_uuid,
                        "base": published.pit.received_from,
                        "from_peer": from_peer,
                        "bytes": published.bytes,
                        "duration_ms": published.duration_ms,
                        "volumes": published.pit.volumes.iter().map(|v| serde_json::json!({
                            "volume_uuid": v.volume_uuid, "guid": v.guid,
                        })).collect::<Vec<_>>(),
                    }),
                );
                true
            }
            Err(PullError::Failed(message)) => {
                waiting.insert(offer.pit_uuid, std::time::Instant::now());
                eprintln!("bmac-backupd: {label} failed its checks and is quarantined: {message}");
                let _ = self.audit.record(
                    at,
                    "quarantine",
                    "bmac-backupd",
                    serde_json::json!({"vm_uuid": offer.vm_uuid, "pit_uuid": offer.pit_uuid, "reason": message}),
                );
                self.failed
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .push(FailedReport {
                        vm_uuid: offer.vm_uuid,
                        pit_uuid: offer.pit_uuid,
                        message,
                    });
                true
            }
            Err(e) => {
                waiting.insert(offer.pit_uuid, std::time::Instant::now());
                eprintln!("bmac-backupd: {label} waits: {e}");
                self.status
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .last_error = Some(e.to_string());
                false
            }
        }
    }
}

impl Agent {
    /// Step 2 of catching up: an incremental from a trusted peer, when the
    /// cluster can offer only a full send over this appliance's head.
    async fn repair(
        &self,
        offer: &OfferedPit,
        cluster: &str,
        now_ms: &(dyn Fn() -> i64 + Send + Sync),
    ) -> Option<(Published, String)> {
        let journal = self.journal()?;
        let head = repair::behind(&journal, cluster, offer)?.clone();
        let repaired = repair::from_head(offer, &head)?;
        let peers = Peers::load(&self.metadata).ok()?;
        if peers.peers.is_empty() {
            return None;
        }
        self.repairing.store(true, Ordering::Relaxed);
        let mut result = None;
        for entry in &peers.peers {
            let peer = &entry.peer;
            let Ok(client) = ClusterClient::peer(
                &peer.fingerprint,
                (&self.tls_identity.0, &self.tls_identity.1),
                EXPORT_PORT,
            ) else {
                continue;
            };
            let index = client
                .call_peer::<(), PeerIndex>(&peer.address, "GET", on_appliance::PEER_INDEX, None)
                .await;
            let trusts = match &index {
                Ok(_) => Some(true),
                // It showed the trusted certificate, then refused ours.
                Err(CallError::Unreachable(_))
                    if client.presented().as_deref() == Some(peer.fingerprint.as_str()) =>
                {
                    Some(false)
                }
                Err(_) => None,
            };
            self.note_trust(&peer.appliance_uuid, trusts).await;
            let Ok(index) = index else {
                continue;
            };
            if !repair::can_serve(&index, cluster, &head, offer) {
                continue;
            }
            eprintln!(
                "bmac-backupd: repairing PIT {} of VM {} from peer {} ({})",
                offer.name, offer.vm_uuid, peer.appliance_uuid, peer.address
            );
            let link = PeerLink::new(client, peer.address.clone(), cluster, &head);
            let puller = Puller {
                pool: self.pool.as_ref(),
                link: &link,
                metadata: &self.metadata,
                layout: &self.layout,
                cluster_uuid: cluster,
                progress: self.progress.clone(),
                now_ms,
            };
            match puller.pull(&repaired).await {
                Ok(published) => {
                    result = Some((published, peer.appliance_uuid.clone()));
                    break;
                }
                Err(e) => {
                    eprintln!(
                        "bmac-backupd: the repair from peer {} failed: {e}",
                        peer.appliance_uuid
                    );
                    for v in &head.volumes {
                        if let Ok(Some(facts)) = self.pool.volume(&v.dataset).await
                            && facts.resume_token.is_some()
                        {
                            let _ = self.pool.abort_receive(&v.dataset).await;
                        }
                    }
                }
            }
        }
        self.repairing.store(false, Ordering::Relaxed);
        result
    }

    async fn note_trust(&self, appliance_uuid: &str, trusts: Option<bool>) {
        let Some(trusts) = trusts else {
            return;
        };
        let Ok(_guard) = self.metadata.lock().await else {
            return;
        };
        let Ok(mut peers) = Peers::load(&self.metadata) else {
            return;
        };
        if let Some(entry) = peers
            .peers
            .iter_mut()
            .find(|e| e.peer.appliance_uuid == appliance_uuid)
            && entry.trusts_us != Some(trusts)
        {
            entry.trusts_us = Some(trusts);
            let _ = peers.save(&self.metadata);
        }
    }
}

impl Link for Agent {
    fn claim<'a>(&'a self, request: &'a ClaimRequest) -> BoxFuture<'a, Result<(), CallError>> {
        Box::pin(async move {
            self.call::<_, serde_json::Value>("POST", on_source::CLAIM, Some(request))
                .await
                .map(drop)
        })
    }

    fn send<'a>(
        &'a self,
        host: &'a SourceHost,
        request: &'a SendRequest,
    ) -> BoxFuture<'a, Result<Stream, CallError>> {
        Box::pin(async move {
            let known = self
                .enrollment
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .host(host.host)
                .cloned();
            // The offer's addresses first, then any the bundle gave.
            let mut target = host.clone();
            if let Some(known) = known {
                for address in known.addresses {
                    if !target.addresses.contains(&address) {
                        target.addresses.push(address);
                    }
                }
            }
            self.client.stream(&target, on_source::SEND, request).await
        })
    }
}
