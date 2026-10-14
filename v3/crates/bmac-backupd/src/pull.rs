// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

//! Pulling one offered PIT (`BACKUP.md`, "Pulling a PIT"): choose the
//! zvols, receive each volume from a host that has it (resuming an
//! interrupted receive of the same PIT, abandoning one of another), check
//! every received snapshot against the manifest, and publish in two phases.
//! The appliance chooses every target itself; the cluster only names IDs.

use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use bmac_backup::manifest::{Manifest, Volume};
use bmac_exec::BoxFuture;
use bmac_proto::api::ErrorCode;
use bmac_proto::appliance::{ClaimRequest, OfferedPit, SendRequest, SourceHost, TransferReport};
use tokio::sync::mpsc;
use uuid::Uuid;

use crate::client::CallError;
use crate::journal::{
    Journal, JournalLock, Layout, Metadata, Quarantined, Receiving, ReceivingVolume, StoredPit,
    StoredVolume,
};
use crate::pool::{Pool, Receive, Stream};

/// The hold the retention service owns on every published snapshot.
pub const RETAIN_TAG: &str = "bmac-retain";
/// Pulls stop while the pool has less free than this share of its size.
const HEADROOM_PERCENT: u64 = 2;

/// The cluster, as a pull needs it.
pub trait Link: Send + Sync {
    fn claim<'a>(&'a self, request: &'a ClaimRequest) -> BoxFuture<'a, Result<(), CallError>>;
    fn send<'a>(
        &'a self,
        host: &'a SourceHost,
        request: &'a SendRequest,
    ) -> BoxFuture<'a, Result<Stream, CallError>>;
}

/// Transfers in progress, for the check-in.
pub type Progress = Arc<Mutex<Vec<TransferReport>>>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Method {
    Full,
    /// Onto the base's zvol, whose newest snapshot is the base.
    Incremental,
    /// A new branch's zvol, a clone of the base's snapshot.
    Clone {
        origin: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VolumePull {
    pub volume_uuid: Uuid,
    pub dataset: String,
    pub method: Method,
    /// The base PIT for this volume's send, when it isn't a full one.
    pub base: Option<Uuid>,
    pub transfer_uuid: Uuid,
    pub done: bool,
    pub bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PullPlan {
    pub volumes: Vec<VolumePull>,
    pub base: Option<Uuid>,
    /// Picking up an interrupted receive of this PIT.
    pub resumed: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PullError {
    /// Try again at a later check-in: the network, a host, the pool's room.
    Retry(String),
    /// The cluster refused; report nothing, the offer settles itself.
    Refused(String),
    /// What arrived didn't match the manifest: quarantined, reported failed.
    Failed(String),
}

impl std::fmt::Display for PullError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PullError::Retry(e) | PullError::Refused(e) | PullError::Failed(e) => f.write_str(e),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Published {
    pub pit: StoredPit,
    pub bytes: u64,
    pub duration_ms: u64,
}

/// The lineage directory for a branch: its UUID, or with the next free
/// number when the plain one holds zvols of another line of history.
fn lineage(
    journal: &Journal,
    layout: &Layout,
    cluster: &str,
    vm: &Uuid,
    branch: &Uuid,
    taken: &BTreeSet<String>,
) -> String {
    let prefix = format!("{}/vms/{vm}/", layout.namespace(cluster));
    let used: BTreeSet<String> = journal
        .datasets()
        .into_iter()
        .chain(taken.iter().map(String::as_str))
        .filter_map(|d| {
            d.strip_prefix(&prefix)?
                .split('/')
                .next()
                .map(str::to_string)
        })
        .collect();
    let plain = branch.to_string();
    if !used.contains(&plain) {
        return plain;
    }
    (2..)
        .map(|n| format!("{branch}-{n}"))
        .find(|l| !used.contains(l))
        .expect("a free lineage")
}

/// What to receive for `offer`. `taken` are datasets the pool holds that the
/// journal doesn't name, which a new lineage avoids.
pub fn plan(
    journal: &Journal,
    layout: &Layout,
    cluster: &str,
    offer: &OfferedPit,
    manifest: &Manifest,
    taken: &BTreeSet<String>,
) -> Result<PullPlan, String> {
    let volumes: Vec<Uuid> = manifest
        .volumes
        .iter()
        .map(|v| {
            v.volume_uuid
                .parse()
                .map_err(|_| format!("volume {} has no UUID", v.slot))
        })
        .collect::<Result<_, _>>()?;
    if let Some(receiving) = journal.receiving(cluster, &offer.vm_uuid)
        && receiving.pit_uuid == offer.pit_uuid
        && receiving.base == offer.base.as_ref().map(|b| b.pit_uuid)
    {
        let base = receiving.base.and_then(|b| journal.pit(cluster, &b));
        let mut out = Vec::new();
        for id in &volumes {
            let recorded = receiving
                .volumes
                .iter()
                .find(|v| &v.volume_uuid == id)
                .ok_or_else(|| format!("the interrupted receive lacks volume {id}"))?;
            let (method, volume_base) = method_for(base, offer, id, &recorded.dataset);
            out.push(VolumePull {
                volume_uuid: *id,
                dataset: recorded.dataset.clone(),
                method,
                base: volume_base,
                transfer_uuid: recorded.transfer_uuid,
                done: recorded.done,
                bytes: recorded.bytes,
            });
        }
        return Ok(PullPlan {
            volumes: out,
            base: receiving.base,
            resumed: true,
        });
    }

    let base = match &offer.base {
        None => None,
        Some(b) => Some(
            journal
                .pit(cluster, &b.pit_uuid)
                .ok_or_else(|| format!("the offered base {} isn't published here", b.pit_uuid))?,
        ),
    };
    let same_branch = base.is_some_and(|b| b.branch_uuid == offer.branch_uuid);
    let fresh = if same_branch {
        let first = base
            .and_then(|b| b.volumes.first())
            .map(|v| v.dataset.clone());
        first
            .and_then(|d| layout.lineage_of(cluster, &d).map(str::to_string))
            .unwrap_or_else(|| offer.branch_uuid.to_string())
    } else {
        lineage(
            journal,
            layout,
            cluster,
            &offer.vm_uuid,
            &offer.branch_uuid,
            taken,
        )
    };
    let mut out = Vec::new();
    for id in &volumes {
        let in_base = base.and_then(|b| b.volume(id));
        let dataset = match (same_branch, in_base) {
            (true, Some(v)) => v.dataset.clone(),
            _ => layout.volume(cluster, &offer.vm_uuid, &fresh, id),
        };
        let (method, volume_base) = method_for(base, offer, id, &dataset);
        out.push(VolumePull {
            volume_uuid: *id,
            dataset,
            method,
            base: volume_base,
            transfer_uuid: Uuid::new_v4(),
            done: false,
            bytes: 0,
        });
    }
    Ok(PullPlan {
        volumes: out,
        base: base.map(|b| b.pit_uuid),
        resumed: false,
    })
}

fn method_for(
    base: Option<&StoredPit>,
    offer: &OfferedPit,
    volume: &Uuid,
    dataset: &str,
) -> (Method, Option<Uuid>) {
    match base.and_then(|b| b.volume(volume).map(|v| (b, v))) {
        None => (Method::Full, None),
        Some((b, v)) if v.dataset == dataset && b.branch_uuid == offer.branch_uuid => {
            (Method::Incremental, Some(b.pit_uuid))
        }
        Some((b, v)) => (
            Method::Clone {
                origin: b.snapshot(v),
            },
            Some(b.pit_uuid),
        ),
    }
}

pub struct Puller<'a> {
    pub pool: &'a dyn Pool,
    pub link: &'a dyn Link,
    pub metadata: &'a Metadata,
    pub layout: &'a Layout,
    pub cluster_uuid: &'a str,
    pub progress: Progress,
    pub now_ms: &'a (dyn Fn() -> i64 + Send + Sync),
}

impl Puller<'_> {
    fn journal(&self) -> Result<Journal, PullError> {
        self.metadata
            .load()
            .map_err(|e| PullError::Retry(format!("journal: {e}")))?
            .ok_or_else(|| PullError::Retry("the metadata dataset has no journal".into()))
    }

    async fn locked(&self) -> Result<JournalLock<'_>, PullError> {
        self.metadata
            .lock()
            .await
            .map_err(|e| PullError::Retry(format!("the journal lock: {e}")))
    }

    fn save(&self, journal: &Journal) -> Result<(), PullError> {
        self.metadata
            .save(journal)
            .map_err(|e| PullError::Retry(format!("journal: {e}")))
    }

    /// Abandons an interrupted receive of another PIT of the VM: its
    /// partial state goes, and so do zvols it had begun and nothing else
    /// uses.
    async fn abandon(
        &self,
        journal: &mut Journal,
        vm: &Uuid,
        keep: &Uuid,
    ) -> Result<(), PullError> {
        let Some(index) = journal.receiving.iter().position(|r| {
            r.cluster_uuid == self.cluster_uuid && &r.vm_uuid == vm && &r.pit_uuid != keep
        }) else {
            return Ok(());
        };
        let stale = journal.receiving.remove(index);
        for volume in &stale.volumes {
            if let Ok(Some(facts)) = self.pool.volume(&volume.dataset).await
                && facts.resume_token.is_some()
            {
                let _ = self.pool.abort_receive(&volume.dataset).await;
            }
        }
        self.save(journal)
    }

    async fn check_room(&self) -> Result<(), PullError> {
        let status = self.pool.status().await.map_err(PullError::Retry)?;
        if status.free_bytes.saturating_mul(100)
            < status.size_bytes.saturating_mul(HEADROOM_PERCENT)
        {
            return Err(PullError::Retry(format!(
                "the pool has {} bytes free of {}; pulls wait for room",
                status.free_bytes, status.size_bytes
            )));
        }
        let (quota, used) = self
            .pool
            .quota(&self.layout.namespace(self.cluster_uuid))
            .await
            .unwrap_or((0, 0));
        if quota > 0 && used >= quota {
            return Err(PullError::Retry(format!(
                "this cluster's namespace is at its quota ({used} of {quota} bytes)"
            )));
        }
        Ok(())
    }

    pub async fn pull(&self, offer: &OfferedPit) -> Result<Published, PullError> {
        let started = Instant::now();
        let manifest = Manifest::open(&offer.manifest)
            .map_err(|e| PullError::Failed(format!("the manifest: {e}")))?;
        if manifest.pit_uuid != offer.pit_uuid.to_string()
            || manifest.vm_uuid != offer.vm_uuid.to_string()
            || manifest.branch_uuid != offer.branch_uuid.to_string()
            || manifest.cluster_uuid != self.cluster_uuid
            || manifest.snapshot.to_string() != offer.name
        {
            return Err(PullError::Failed(
                "the manifest describes another PIT".into(),
            ));
        }
        if !bmac_backup::names::is_pit(&offer.name) {
            return Err(PullError::Failed(format!(
                "{} isn't a PIT name",
                offer.name
            )));
        }
        let guard = self.locked().await?;
        let mut journal = self.journal()?;
        if journal.pit(self.cluster_uuid, &offer.pit_uuid).is_some() {
            return Err(PullError::Refused("already published".into()));
        }
        self.abandon(&mut journal, &offer.vm_uuid, &offer.pit_uuid)
            .await?;
        self.check_room().await?;
        let taken: BTreeSet<String> = journal
            .quarantined
            .iter()
            .flat_map(|q| q.datasets.clone())
            .collect();
        let mut plan = plan(
            &journal,
            self.layout,
            self.cluster_uuid,
            offer,
            &manifest,
            &taken,
        )
        .map_err(PullError::Retry)?;
        if !plan.resumed {
            journal
                .receiving
                .retain(|r| !(r.cluster_uuid == self.cluster_uuid && r.vm_uuid == offer.vm_uuid));
            journal.receiving.push(Receiving {
                cluster_uuid: self.cluster_uuid.to_string(),
                vm_uuid: offer.vm_uuid,
                branch_uuid: offer.branch_uuid,
                pit_uuid: offer.pit_uuid,
                base: plan.base,
                started_at_ms: (self.now_ms)(),
                volumes: plan
                    .volumes
                    .iter()
                    .map(|v| ReceivingVolume {
                        volume_uuid: v.volume_uuid,
                        transfer_uuid: v.transfer_uuid,
                        dataset: v.dataset.clone(),
                        done: false,
                        bytes: 0,
                    })
                    .collect(),
            });
            self.save(&journal)?;
        }
        drop(guard);
        match self
            .link
            .claim(&ClaimRequest {
                vm_uuid: offer.vm_uuid,
                pit_uuid: offer.pit_uuid,
            })
            .await
        {
            Ok(()) => {}
            Err(CallError::Refused(e)) => {
                return Err(PullError::Refused(format!("the claim: {}", e.message)));
            }
            Err(CallError::Unreachable(e)) => {
                return Err(PullError::Retry(format!("the claim: {e}")));
            }
        }

        for index in 0..plan.volumes.len() {
            if plan.volumes[index].done {
                continue;
            }
            let expected = manifest
                .volumes
                .iter()
                .find(|v| v.volume_uuid == plan.volumes[index].volume_uuid.to_string())
                .expect("planned from the manifest")
                .clone();
            let before: u64 = plan.volumes[..index].iter().map(|v| v.bytes).sum();
            let received = self
                .receive_volume(offer, &plan.volumes[index], &expected, before)
                .await?;
            plan.volumes[index].done = true;
            plan.volumes[index].bytes += received;
            let _guard = self.locked().await?;
            let mut journal = self.journal()?;
            if let Some(r) = journal
                .receiving
                .iter_mut()
                .find(|r| r.cluster_uuid == self.cluster_uuid && r.pit_uuid == offer.pit_uuid)
                && let Some(v) = r
                    .volumes
                    .iter_mut()
                    .find(|v| v.volume_uuid == plan.volumes[index].volume_uuid)
            {
                v.done = true;
                v.bytes = plan.volumes[index].bytes;
            }
            self.save(&journal)?;
        }
        self.publish(offer, &manifest, &plan, started).await
    }

    /// `before` is what the PIT's earlier volumes took, so the check-in
    /// reports the PIT's bytes as one growing count.
    async fn receive_volume(
        &self,
        offer: &OfferedPit,
        volume: &VolumePull,
        expected: &Volume,
        before: u64,
    ) -> Result<u64, PullError> {
        let snapshot = format!("{}@{}", volume.dataset, offer.name);
        let facts = self
            .pool
            .volume(&volume.dataset)
            .await
            .map_err(PullError::Retry)?;
        if facts.is_some()
            && self
                .pool
                .guids(std::slice::from_ref(&snapshot))
                .await
                .map_err(PullError::Retry)?
                .get(&snapshot)
                == Some(&expected.snapshot_guid)
        {
            return Ok(0);
        }
        let resume_token = facts.as_ref().and_then(|f| f.resume_token.clone());
        if resume_token.is_none() && matches!(volume.method, Method::Full | Method::Clone { .. }) {
            if facts.is_some() {
                return Err(PullError::Failed(format!(
                    "{} already exists, so a new zvol can't be received there",
                    volume.dataset
                )));
            }
            self.pool
                .ensure_parent(&volume.dataset)
                .await
                .map_err(PullError::Retry)?;
        }
        let request = SendRequest {
            transfer_uuid: volume.transfer_uuid,
            vm_uuid: offer.vm_uuid,
            pit_uuid: offer.pit_uuid,
            volume_uuid: volume.volume_uuid,
            base: volume.base,
            resume_token: resume_token.clone(),
            large_blocks: true,
            compressed: true,
        };
        let target = Receive {
            dataset: volume.dataset.clone(),
            origin: match &volume.method {
                Method::Clone { origin } if resume_token.is_none() => Some(origin.clone()),
                _ => None,
            },
        };
        let mut last = String::from("no host holds it");
        for host in &offer.hosts {
            let stream = match self.link.send(host, &request).await {
                Ok(stream) => stream,
                Err(CallError::Unreachable(e)) => {
                    last = format!("{}: {e}", host.host);
                    continue;
                }
                Err(CallError::Refused(e))
                    if matches!(e.code, ErrorCode::NotFound | ErrorCode::Forbidden) =>
                {
                    // This host lacks the snapshot or the base; another may not.
                    last = format!("{}: {}", host.host, e.message);
                    continue;
                }
                Err(CallError::Refused(e)) => return Err(PullError::Refused(e.message)),
            };
            eprintln!(
                "bmac-backupd: receiving {} ({}) from {}",
                volume.dataset,
                match (&volume.method, &resume_token) {
                    (_, Some(_)) => "resumed".to_string(),
                    (Method::Full, None) => "full".to_string(),
                    (Method::Incremental, None) => "incremental".to_string(),
                    (Method::Clone { origin }, None) => format!("a clone of {origin}"),
                },
                host.host
            );
            return self.feed(offer, volume, &target, stream, before).await;
        }
        Err(PullError::Retry(format!(
            "volume {}: {last}",
            volume.volume_uuid
        )))
    }

    /// Relays the stream into `zfs receive`, counting it for the check-in.
    async fn feed(
        &self,
        offer: &OfferedPit,
        volume: &VolumePull,
        target: &Receive,
        mut stream: Stream,
        before: u64,
    ) -> Result<u64, PullError> {
        let (tx, rx) = mpsc::channel(8);
        let progress = self.progress.clone();
        let report = TransferReport {
            transfer_uuid: volume.transfer_uuid,
            vm_uuid: offer.vm_uuid,
            pit_uuid: offer.pit_uuid,
            bytes: before + volume.bytes,
        };
        progress
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .push(report);
        let transfer = volume.transfer_uuid;
        let relay = async move {
            while let Some(chunk) = stream.recv().await {
                if let Ok(bytes) = &chunk {
                    let mut all = progress.lock().unwrap_or_else(|p| p.into_inner());
                    if let Some(t) = all.iter_mut().find(|t| t.transfer_uuid == transfer) {
                        t.bytes += bytes.len() as u64;
                    }
                }
                if tx.send(chunk).await.is_err() {
                    break;
                }
            }
        };
        let (_, received) = tokio::join!(relay, self.pool.receive(target, rx));
        self.progress
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .retain(|t| t.transfer_uuid != transfer);
        received.map_err(|e| PullError::Retry(format!("volume {}: {e}", volume.volume_uuid)))
    }

    async fn quarantine(&self, offer: &OfferedPit, plan: &PullPlan, reason: String) -> PullError {
        let _guard = self.metadata.lock().await;
        let Ok(mut journal) = self.journal() else {
            return PullError::Failed(reason);
        };
        journal
            .receiving
            .retain(|r| !(r.cluster_uuid == self.cluster_uuid && r.pit_uuid == offer.pit_uuid));
        let mut datasets = Vec::new();
        for v in &plan.volumes {
            if v.method == Method::Incremental {
                // The base's zvol must end at the base again for the next pull.
                let _ = self
                    .pool
                    .destroy(&format!("{}@{}", v.dataset, offer.name))
                    .await;
            } else {
                datasets.push(v.dataset.clone());
            }
        }
        journal.quarantined.push(Quarantined {
            cluster_uuid: self.cluster_uuid.to_string(),
            vm_uuid: offer.vm_uuid,
            pit_uuid: offer.pit_uuid,
            reason: reason.clone(),
            at_ms: (self.now_ms)(),
            datasets,
        });
        let _ = self.save(&journal);
        PullError::Failed(reason)
    }

    async fn publish(
        &self,
        offer: &OfferedPit,
        manifest: &Manifest,
        plan: &PullPlan,
        started: Instant,
    ) -> Result<Published, PullError> {
        let names: Vec<String> = plan
            .volumes
            .iter()
            .map(|v| format!("{}@{}", v.dataset, offer.name))
            .collect();
        let found = self.pool.guids(&names).await.map_err(PullError::Retry)?;
        let mut stored = Vec::new();
        for (v, name) in plan.volumes.iter().zip(&names) {
            let expected = manifest
                .volumes
                .iter()
                .find(|m| m.volume_uuid == v.volume_uuid.to_string())
                .expect("planned from the manifest");
            if found.get(name) != Some(&expected.snapshot_guid) {
                return Err(self
                    .quarantine(
                        offer,
                        plan,
                        format!("{name} doesn't have the manifest's GUID"),
                    )
                    .await);
            }
            let facts = self
                .pool
                .volume(&v.dataset)
                .await
                .map_err(PullError::Retry)?
                .ok_or_else(|| PullError::Retry(format!("{} vanished", v.dataset)))?;
            if facts.volsize != expected.volsize || facts.volblocksize != expected.volblocksize {
                return Err(self
                    .quarantine(
                        offer,
                        plan,
                        format!(
                            "{} is {} bytes in {}-byte blocks; the manifest says {} in {}",
                            v.dataset,
                            facts.volsize,
                            facts.volblocksize,
                            expected.volsize,
                            expected.volblocksize
                        ),
                    )
                    .await);
            }
            stored.push(StoredVolume {
                volume_uuid: v.volume_uuid,
                dataset: v.dataset.clone(),
                guid: expected.snapshot_guid,
                volsize: facts.volsize,
                volblocksize: facts.volblocksize,
            });
        }
        for name in &names {
            self.pool
                .hold(RETAIN_TAG, name)
                .await
                .map_err(PullError::Retry)?;
        }
        self.metadata
            .write_manifest(self.cluster_uuid, &offer.pit_uuid, &offer.manifest)
            .map_err(|e| PullError::Retry(format!("the manifest: {e}")))?;
        let bytes = plan.volumes.iter().map(|v| v.bytes).sum();
        let pit = StoredPit {
            cluster_uuid: self.cluster_uuid.to_string(),
            vm_uuid: offer.vm_uuid,
            branch_uuid: offer.branch_uuid,
            pit_uuid: offer.pit_uuid,
            parent: offer.parent,
            name: offer.name.clone(),
            taken_s: offer.taken_s,
            published_at_ms: (self.now_ms)(),
            volumes: stored,
            bytes,
            manifest_sha256: manifest.sha256.clone(),
            received_from: plan.base,
        };
        let _guard = self.locked().await?;
        let mut journal = self.journal()?;
        journal
            .receiving
            .retain(|r| !(r.cluster_uuid == self.cluster_uuid && r.pit_uuid == offer.pit_uuid));
        journal.pits.push(pit.clone());
        journal.revision += 1;
        self.save(&journal)?;
        Ok(Published {
            pit,
            bytes,
            duration_ms: started.elapsed().as_millis() as u64,
        })
    }
}

#[cfg(test)]
#[path = "pull_tests.rs"]
mod tests;
