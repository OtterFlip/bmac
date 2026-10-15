// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

//! Appliance retention (`BACKUP.md`, "Appliance retention"): the
//! appliance's own policy, a plan for every prune, and the prune itself,
//! which reads the journal and the live snapshots again under the journal
//! lock and releases a PIT's holds only as part of it.

use std::collections::{BTreeMap, BTreeSet};
use std::io;

use bmac_backup::retention::{
    AppliancePit, ApplianceRetention, Calendar, Clock, KeepReason, appliance_keep,
};
use jiff::tz::TimeZone;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::audit::Audit;
use crate::fsx;
use crate::journal::{Journal, Metadata, StoredPit};
use crate::pool::Pool;
use crate::pull::RETAIN_TAG;

pub const DEFAULT_DELAY_HOURS: u32 = 72;
/// How long a quarantined receive's zvols stay for a look.
pub const QUARANTINE_KEEP_MS: i64 = 7 * 24 * 3600 * 1000;
const HOUR_MS: i64 = 3600 * 1000;

/// A weaker policy waiting out the delay.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Pending {
    pub policy: ApplianceRetention,
    pub requested_at_ms: i64,
    pub effective_at_ms: i64,
    pub by: String,
}

/// An administrator's hold: the PIT stays whatever the policy says.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AdminHold {
    pub cluster_uuid: String,
    pub pit_uuid: Uuid,
    pub by: String,
    pub at_ms: i64,
}

/// When a VM's first PIT here was taken: its day starts the VM's weeks,
/// months and years, and never moves.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VmStart {
    pub cluster_uuid: String,
    pub vm_uuid: Uuid,
    pub taken_s: i64,
}

/// `retention.json` in the metadata dataset, so it moves with the pool.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Settings {
    pub policy: ApplianceRetention,
    /// Set only at the console; the UI can't keep less than this.
    pub floor: ApplianceRetention,
    pub delay_hours: u32,
    pub pending: Option<Pending>,
    pub holds: Vec<AdminHold>,
    pub starts: Vec<VmStart>,
    /// For tests only, set at the console: days last this many seconds.
    pub test_day_s: Option<i64>,
}

impl Default for Settings {
    fn default() -> Settings {
        Settings {
            policy: ApplianceRetention::default(),
            floor: ApplianceRetention {
                daily: 1,
                weekly: 0,
                monthly: 0,
                ..ApplianceRetention::default()
            },
            delay_hours: DEFAULT_DELAY_HOURS,
            pending: None,
            holds: Vec::new(),
            starts: Vec::new(),
            test_day_s: None,
        }
    }
}

/// What a policy change did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Change {
    Applied,
    /// It weakens retention, so it waits until then.
    Delayed {
        effective_at_ms: i64,
    },
}

/// Whether `policy` keeps fewer of any tier than `floor`; without yearly it
/// keeps every month.
fn below(policy: &ApplianceRetention, floor: &ApplianceRetention) -> bool {
    policy.daily < floor.daily
        || policy.weekly < floor.weekly
        || (policy.yearly && policy.monthly < floor.monthly)
}

impl Settings {
    pub fn load(metadata: &Metadata) -> io::Result<Settings> {
        Ok(fsx::read_json(&metadata.root.join("retention.json"))?.unwrap_or_default())
    }

    pub fn save(&self, metadata: &Metadata) -> io::Result<()> {
        fsx::write_json(&metadata.root.join("retention.json"), self, 0o600)
    }

    /// Applies a pending policy whose delay is over.
    pub fn settle(&mut self, now_ms: i64) -> Option<Pending> {
        if self.pending.as_ref()?.effective_at_ms > now_ms {
            return None;
        }
        let pending = self.pending.take()?;
        self.policy = pending.policy;
        Some(pending)
    }

    /// A new policy. One that keeps less in any tier waits out the delay
    /// unless `confirmed` (at the console); none may go below the floor.
    pub fn change(
        &mut self,
        policy: ApplianceRetention,
        by: &str,
        now_ms: i64,
        confirmed: bool,
    ) -> Result<Change, String> {
        policy.validate()?;
        if below(&policy, &self.floor) {
            return Err(format!(
                "the retention floor is {} daily, {} weekly, {} monthly",
                self.floor.daily, self.floor.weekly, self.floor.monthly
            ));
        }
        if !policy.weakens(&self.policy) || confirmed {
            self.policy = policy;
            self.pending = None;
            return Ok(Change::Applied);
        }
        let effective_at_ms = now_ms + i64::from(self.delay_hours) * HOUR_MS;
        self.pending = Some(Pending {
            policy,
            requested_at_ms: now_ms,
            effective_at_ms,
            by: by.to_string(),
        });
        Ok(Change::Delayed { effective_at_ms })
    }

    /// The floor, from the console only; the policy must already keep it.
    pub fn set_floor(&mut self, floor: ApplianceRetention) -> Result<(), String> {
        if below(&self.policy, &floor) {
            return Err("the policy keeps less than that floor; raise the policy first".into());
        }
        self.floor = floor;
        if self
            .pending
            .as_ref()
            .is_some_and(|p| below(&p.policy, &floor))
        {
            self.pending = None;
        }
        Ok(())
    }

    pub fn hold(&mut self, cluster: &str, pit: Uuid, by: &str, now_ms: i64) {
        if !self.held(cluster, &pit) {
            self.holds.push(AdminHold {
                cluster_uuid: cluster.to_string(),
                pit_uuid: pit,
                by: by.to_string(),
                at_ms: now_ms,
            });
        }
    }

    pub fn release(&mut self, cluster: &str, pit: &Uuid) -> bool {
        let before = self.holds.len();
        self.holds
            .retain(|h| !(h.cluster_uuid == cluster && &h.pit_uuid == pit));
        self.holds.len() != before
    }

    pub fn start(&self, cluster: &str, vm: &Uuid) -> Option<i64> {
        self.starts
            .iter()
            .find(|s| s.cluster_uuid == cluster && &s.vm_uuid == vm)
            .map(|s| s.taken_s)
    }

    /// Records the start of every VM in `journal` that has none yet, from
    /// its oldest PIT; whether any was new.
    pub fn note_starts(&mut self, journal: &Journal) -> bool {
        let mut oldest: BTreeMap<(&str, Uuid), i64> = BTreeMap::new();
        for pit in &journal.pits {
            let t = oldest
                .entry((pit.cluster_uuid.as_str(), pit.vm_uuid))
                .or_insert(pit.taken_s);
            *t = (*t).min(pit.taken_s);
        }
        let mut noted = false;
        for ((cluster, vm), taken_s) in oldest {
            if self.start(cluster, &vm).is_none() {
                self.starts.push(VmStart {
                    cluster_uuid: cluster.to_string(),
                    vm_uuid: vm,
                    taken_s,
                });
                noted = true;
            }
        }
        noted
    }

    fn held(&self, cluster: &str, pit: &Uuid) -> bool {
        self.holds
            .iter()
            .any(|h| h.cluster_uuid == cluster && &h.pit_uuid == pit)
    }
}

/// A cluster's time zone as its check-ins report it. A different zone
/// shifts every day, so it waits out the weakening delay.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClusterZone {
    pub zone: String,
    pub pending: Option<PendingZone>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PendingZone {
    pub zone: String,
    pub effective_at_ms: i64,
}

/// `zones.json` in the metadata dataset, written only by the check-ins.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Zones {
    pub clusters: BTreeMap<String, ClusterZone>,
}

impl Zones {
    pub fn load(metadata: &Metadata) -> io::Result<Zones> {
        Ok(fsx::read_json(&metadata.root.join("zones.json"))?.unwrap_or_default())
    }

    pub fn save(&self, metadata: &Metadata) -> io::Result<()> {
        fsx::write_json(&metadata.root.join("zones.json"), self, 0o600)
    }

    /// Records the zone `cluster` reported: its first at once, a different
    /// one after `delay_hours`, and a pending one that came due. Whether
    /// anything changed.
    pub fn report(
        &mut self,
        cluster: &str,
        zone: &str,
        delay_hours: u32,
        now_ms: i64,
    ) -> Result<bool, String> {
        TimeZone::get(zone).map_err(|_| format!("{zone:?} isn't a time zone"))?;
        let Some(known) = self.clusters.get_mut(cluster) else {
            self.clusters.insert(
                cluster.to_string(),
                ClusterZone {
                    zone: zone.to_string(),
                    pending: None,
                },
            );
            return Ok(true);
        };
        if let Some(p) = known.pending.take_if(|p| p.effective_at_ms <= now_ms) {
            known.zone = p.zone;
            return Ok(true);
        }
        if known.zone == zone {
            return Ok(known.pending.take().is_some());
        }
        if known.pending.as_ref().is_some_and(|p| p.zone == zone) {
            return Ok(false);
        }
        known.pending = Some(PendingZone {
            zone: zone.to_string(),
            effective_at_ms: now_ms + i64::from(delay_hours) * HOUR_MS,
        });
        Ok(true)
    }

    /// The zone `cluster`'s days follow now; UTC until it reports one.
    pub fn zone(&self, cluster: &str, now_ms: i64) -> TimeZone {
        self.clusters
            .get(cluster)
            .map(|c| match &c.pending {
                Some(p) if p.effective_at_ms <= now_ms => p.zone.as_str(),
                _ => c.zone.as_str(),
            })
            .and_then(|z| TimeZone::get(z).ok())
            .unwrap_or(TimeZone::UTC)
    }
}

/// A PIT by cluster and UUID.
pub type PitRef = (String, Uuid);

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Doomed {
    pub cluster_uuid: String,
    pub vm_uuid: Uuid,
    pub pit_uuid: Uuid,
    pub taken_s: i64,
    /// `dataset@name` of each volume.
    pub snapshots: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Kept {
    pub cluster_uuid: String,
    pub vm_uuid: Uuid,
    pub pit_uuid: Uuid,
    pub taken_s: i64,
    pub reasons: BTreeSet<KeepReason>,
}

/// What a VM can still be restored to after the prune.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Coverage {
    pub cluster_uuid: String,
    pub vm_uuid: Uuid,
    pub pits: usize,
    pub oldest_s: i64,
    pub newest_s: i64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Plan {
    pub prune: Vec<Doomed>,
    pub keep: Vec<Kept>,
    pub coverage: Vec<Coverage>,
    /// Quarantined receives old enough to go.
    pub quarantine: Vec<Uuid>,
    /// ZFS's estimate, filled in by [`estimate`]; often less than the PITs'
    /// sizes, since blocks are shared.
    pub reclaim_bytes: u64,
}

impl Plan {
    pub fn is_empty(&self) -> bool {
        self.prune.is_empty() && self.quarantine.is_empty()
    }
}

/// The plan for `journal` under `settings`. `pins` carries the reasons
/// only the caller knows: live restore grants, peer repairs.
pub fn plan(
    journal: &Journal,
    settings: &Settings,
    zones: &Zones,
    pins: &BTreeMap<PitRef, BTreeSet<KeepReason>>,
    now_ms: i64,
) -> Plan {
    let mut groups: BTreeMap<(&str, Uuid), Vec<&StoredPit>> = BTreeMap::new();
    for pit in &journal.pits {
        groups
            .entry((pit.cluster_uuid.as_str(), pit.vm_uuid))
            .or_default()
            .push(pit);
    }
    let mut out = Plan::default();
    for ((cluster, vm), pits) in groups {
        let mut pinned: BTreeMap<Uuid, BTreeSet<KeepReason>> = BTreeMap::new();
        for pit in &pits {
            let key = (cluster.to_string(), pit.pit_uuid);
            if let Some(reasons) = pins.get(&key) {
                pinned.entry(pit.pit_uuid).or_default().extend(reasons);
            }
            if settings.held(cluster, &pit.pit_uuid) {
                pinned
                    .entry(pit.pit_uuid)
                    .or_default()
                    .insert(KeepReason::Hold);
            }
        }
        if let Some(base) = journal.receiving(cluster, &vm).and_then(|r| r.base) {
            pinned.entry(base).or_default().insert(KeepReason::Transfer);
        }
        let candidates: Vec<AppliancePit> = pits
            .iter()
            .map(|p| AppliancePit {
                pit_uuid: p.pit_uuid,
                taken_s: p.taken_s,
                branch: p.branch_uuid.to_string(),
                parent: p.received_from,
            })
            .collect();
        let calendar = Calendar::new(zones.zone(cluster, now_ms), settings.test_day_s);
        let clock = Clock {
            calendar: &calendar,
            start_day: settings.start(cluster, &vm).map(|t| calendar.day(t)),
            now_s: now_ms.div_euclid(1000),
        };
        let keep = appliance_keep(&candidates, &settings.policy, &clock, &pinned);
        let mut kept_times = Vec::new();
        for pit in pits {
            match keep.get(&pit.pit_uuid) {
                Some(reasons) => {
                    kept_times.push(pit.taken_s);
                    out.keep.push(Kept {
                        cluster_uuid: cluster.to_string(),
                        vm_uuid: vm,
                        pit_uuid: pit.pit_uuid,
                        taken_s: pit.taken_s,
                        reasons: reasons.clone(),
                    });
                }
                None => out.prune.push(Doomed {
                    cluster_uuid: cluster.to_string(),
                    vm_uuid: vm,
                    pit_uuid: pit.pit_uuid,
                    taken_s: pit.taken_s,
                    snapshots: pit.volumes.iter().map(|v| pit.snapshot(v)).collect(),
                }),
            }
        }
        out.coverage.push(Coverage {
            cluster_uuid: cluster.to_string(),
            vm_uuid: vm,
            pits: kept_times.len(),
            oldest_s: kept_times.iter().copied().min().unwrap_or(0),
            newest_s: kept_times.iter().copied().max().unwrap_or(0),
        });
    }
    out.quarantine = journal
        .quarantined
        .iter()
        .filter(|q| now_ms - q.at_ms >= QUARANTINE_KEEP_MS)
        .map(|q| q.pit_uuid)
        .collect();
    out
}

/// Fills in ZFS's estimate of the space the plan frees.
pub async fn estimate(pool: &dyn Pool, plan: &mut Plan) {
    let mut by_dataset: BTreeMap<&str, Vec<String>> = BTreeMap::new();
    for doomed in &plan.prune {
        for snapshot in &doomed.snapshots {
            if let Some((dataset, name)) = snapshot.split_once('@') {
                by_dataset
                    .entry(dataset)
                    .or_default()
                    .push(name.to_string());
            }
        }
    }
    let mut total = 0;
    for (dataset, names) in &by_dataset {
        total += pool.reclaim(dataset, names).await.unwrap_or(0);
    }
    plan.reclaim_bytes = total;
}

/// What a prune did.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Report {
    pub pruned: Vec<Uuid>,
    pub reclaim_bytes: u64,
    /// PITs the plan named that stay, and why.
    pub skipped: Vec<(Uuid, String)>,
    pub zvols: Vec<String>,
    pub quarantine: Vec<Uuid>,
    /// The pending policy that took effect first, if any.
    pub settled: Option<ApplianceRetention>,
}

/// Plans and prunes under the journal lock, which pulls also take, so no
/// receive can start from a PIT while it goes.
pub async fn prune(
    pool: &dyn Pool,
    metadata: &Metadata,
    audit: &Audit,
    pins: &BTreeMap<PitRef, BTreeSet<KeepReason>>,
    now_ms: i64,
) -> Result<Report, String> {
    let _guard = metadata
        .lock()
        .await
        .map_err(|e| format!("the journal lock: {e}"))?;
    let mut journal = metadata
        .load()
        .map_err(|e| format!("journal: {e}"))?
        .ok_or("the metadata dataset has no journal")?;
    let mut settings = Settings::load(metadata).map_err(|e| format!("retention: {e}"))?;
    let zones = Zones::load(metadata).map_err(|e| format!("zones: {e}"))?;
    let mut report = Report::default();
    if settings.note_starts(&journal) {
        settings
            .save(metadata)
            .map_err(|e| format!("retention: {e}"))?;
    }
    if let Some(pending) = settings.settle(now_ms) {
        settings
            .save(metadata)
            .map_err(|e| format!("retention: {e}"))?;
        report.settled = Some(pending.policy);
        let _ = audit.record(
            now_ms,
            "policy",
            &pending.by,
            serde_json::json!({"retention": pending.policy, "delayed_from_ms": pending.requested_at_ms}),
        );
    }
    let mut plan = plan(&journal, &settings, &zones, pins, now_ms);
    if plan.is_empty() {
        return Ok(report);
    }
    estimate(pool, &mut plan).await;
    report.reclaim_bytes = plan.reclaim_bytes;

    let mut freed: BTreeSet<String> = BTreeSet::new();
    for doomed in &plan.prune {
        let Some(stored) = journal.pit(&doomed.cluster_uuid, &doomed.pit_uuid).cloned() else {
            continue;
        };
        match destroy_pit(pool, &stored).await {
            Ok(()) => {}
            Err(Failed::Untouched(e)) => {
                report.skipped.push((doomed.pit_uuid, e));
                continue;
            }
            Err(Failed::Partly(e)) => report.skipped.push((doomed.pit_uuid, e)),
        }
        journal
            .pits
            .retain(|p| !(p.cluster_uuid == doomed.cluster_uuid && p.pit_uuid == doomed.pit_uuid));
        // Its children descend from its parent, so restores and the graph
        // still find every remaining ancestor.
        for child in journal
            .pits
            .iter_mut()
            .filter(|p| p.cluster_uuid == doomed.cluster_uuid && p.parent == Some(doomed.pit_uuid))
        {
            child.parent = stored.parent;
        }
        let _ =
            std::fs::remove_file(metadata.manifest_path(&doomed.cluster_uuid, &doomed.pit_uuid));
        settings.release(&doomed.cluster_uuid, &doomed.pit_uuid);
        freed.extend(stored.volumes.iter().map(|v| v.dataset.clone()));
        report.pruned.push(doomed.pit_uuid);
    }
    let used: BTreeSet<String> = journal.datasets().into_iter().map(String::from).collect();
    for dataset in freed.difference(&used) {
        if matches!(pool.snapshots(dataset).await, Ok(s) if s.is_empty())
            && pool.destroy(dataset).await.is_ok()
        {
            report.zvols.push(dataset.clone());
        }
    }
    for id in &plan.quarantine {
        let Some(index) = journal.quarantined.iter().position(|q| &q.pit_uuid == id) else {
            continue;
        };
        let datasets = journal.quarantined[index].datasets.clone();
        let mut gone = true;
        for dataset in datasets.iter().filter(|d| !used.contains(*d)) {
            gone &= pool.destroy_tree(dataset).await.is_ok();
        }
        if gone {
            journal.quarantined.remove(index);
            report.quarantine.push(*id);
        }
    }
    if !report.pruned.is_empty() {
        journal.revision += 1;
    }
    metadata
        .save(&journal)
        .map_err(|e| format!("journal: {e}"))?;
    settings
        .save(metadata)
        .map_err(|e| format!("retention: {e}"))?;
    let _ = audit.record(
        now_ms,
        "prune",
        "bmac-backupd",
        serde_json::json!({
            "pits": report.pruned,
            "skipped": report.skipped,
            "zvols": report.zvols,
            "quarantine": report.quarantine,
            "kept": plan.keep.len(),
            "estimated_reclaim_bytes": report.reclaim_bytes,
            "policy": settings.policy,
        }),
    );
    Ok(report)
}

enum Failed {
    /// Nothing was destroyed; the PIT stays published.
    Untouched(String),
    /// Some of its snapshots are gone, so it can't be restored any more and
    /// is unpublished; the rest keep their holds.
    Partly(String),
}

/// Destroys a PIT's snapshots once each still has the GUID the journal
/// recorded; a snapshot already gone is fine. A failure puts back the
/// hold it released.
async fn destroy_pit(pool: &dyn Pool, pit: &StoredPit) -> Result<(), Failed> {
    let names: Vec<String> = pit.volumes.iter().map(|v| pit.snapshot(v)).collect();
    let live = pool.guids(&names).await.map_err(Failed::Untouched)?;
    let mut present = Vec::new();
    for (volume, name) in pit.volumes.iter().zip(&names) {
        match live.get(name) {
            None => {}
            Some(guid) if *guid == volume.guid => present.push(name.clone()),
            Some(_) => {
                return Err(Failed::Untouched(format!(
                    "{name} isn't the snapshot the journal recorded"
                )));
            }
        }
    }
    for (destroyed, name) in present.iter().enumerate() {
        let result = match pool.release(RETAIN_TAG, name).await {
            Ok(()) => pool.destroy(name).await,
            Err(e) => Err(e),
        };
        if let Err(e) = result {
            let _ = pool.hold(RETAIN_TAG, name).await;
            return Err(if destroyed == 0 && present.len() == names.len() {
                Failed::Untouched(e)
            } else {
                Failed::Partly(e)
            });
        }
    }
    Ok(())
}

#[cfg(test)]
#[path = "retention_tests.rs"]
mod tests;
