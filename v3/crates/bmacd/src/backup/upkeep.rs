// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

//! Upkeep of the PITs on the cluster (`BACKUP.md`, "Snapshots and bookmarks
//! on the cluster"): every host, every minute, for each VM it owns and no
//! operation holds, works out from the catalog and the appliances' reports
//! which snapshots and bookmarks something still needs
//! ([`bmac_backup::lifetime`]) and carries that out:
//!
//! - a released PIT is first bookmarked on every placement host from that
//!   host's copy, each bookmark's GUID checked, then its snapshots are
//!   destroyed on the owner (replication removes the replicas' copies);
//! - a snapshot that can't be destroyed (a clone depends on it) is retried
//!   later, never forced, with the reason shown;
//! - bookmarks nothing needs are removed from every placement host, and a
//!   PIT left with neither leaves the catalog.

use std::collections::BTreeSet;

use bmac_backup::lifetime::{self, ClusterPit, Head, Inputs, Offer, Plan};
use bmac_backup::manifest::Manifest;
use bmac_backup::records::{ApplianceRecord, Lineage, PitRecord, Policy, Presence, RestoreRecord};
use bmac_backup::retention::LocalPit;
use bmac_backup::settings::BackupSettings;
use bmac_backup::states::{self, ApplianceState};
use uuid::Uuid;

use super::capture::{Guests, Production, facts_of};
use super::cluster::BackupCluster;
use super::store::BackupStore;

/// Whether this release relies on bookmarks (lab items 2 and 3, M9.0) or
/// keeps the appliances' heads as snapshots instead.
pub const BOOKMARKS_PROVEN: bool = true;

pub struct Upkeep<'a> {
    pub cluster: &'a dyn BackupCluster,
    pub store: &'a BackupStore,
    pub guests: &'a dyn Guests,
    pub now_ms: &'a (dyn Fn() -> i64 + Send + Sync),
    pub bookmarks_proven: bool,
    /// The cluster's time zone, for local retention's days.
    pub zone: jiff::tz::TimeZone,
}

/// What one pass did for one VM.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Report {
    pub vm: Uuid,
    pub released: Vec<String>,
    pub blocked: Vec<(String, String)>,
    pub bookmarks_removed: Vec<String>,
    pub dropped: Vec<String>,
    pub errors: Vec<String>,
}

/// The state of each appliance of `policy` for this VM's catalog: its last
/// publication and oldest unpublished offer come from the PITs' offers.
pub fn appliance_state(
    appliance: &ApplianceRecord,
    vm: &Uuid,
    pits: &[PitRecord],
    now_s: i64,
    settings: &BackupSettings,
) -> ApplianceState {
    let mut facts = facts_of(appliance);
    let id = appliance.appliance_uuid;
    let published = pits
        .iter()
        .filter(|p| p.offers.get(&id) == Some(&Offer::Published))
        .map(|p| p.taken_s)
        .max();
    // A head reported for a PIT the catalog no longer holds still counts.
    facts.last_published_s = published.or(appliance.heads.get(vm).map(|_| 0));
    facts.oldest_unpublished_offer_s = pits
        .iter()
        .filter(|p| published.is_none_or(|t| p.taken_s > t))
        .filter(|p| p.offers.get(&id).is_some_and(|o| *o != Offer::Published))
        .map(|p| p.taken_s)
        .min();
    states::state(&facts, now_s, settings)
}

impl Upkeep<'_> {
    fn now_s(&self) -> i64 {
        (self.now_ms)().div_euclid(1000)
    }

    /// One pass over every cataloged VM this host owns.
    pub async fn run(&self) -> Vec<Report> {
        let vms = match self.store.cataloged_vms() {
            Ok(vms) => vms,
            Err(e) => {
                return vec![Report {
                    errors: vec![e.message],
                    ..Report::default()
                }];
            }
        };
        let mut reports = Vec::new();
        for vm in vms {
            match self.vm(vm).await {
                Ok(Some(report)) => reports.push(report),
                Ok(None) => {}
                Err(e) => reports.push(Report {
                    vm,
                    errors: vec![e],
                    ..Report::default()
                }),
            }
        }
        reports
    }

    pub fn plan(
        &self,
        pits: &[PitRecord],
        policy: Option<&Policy>,
        appliances: &[ApplianceRecord],
        continuity: &Continuity,
        settings: &BackupSettings,
    ) -> Plan {
        let now_s = self.now_s();
        let Some(vm) = pits.first().map(|p| p.vm_uuid) else {
            return Plan::default();
        };
        let assigned: BTreeSet<Uuid> = policy
            .map(|p| p.appliances.iter().copied().collect())
            .unwrap_or_default();
        let heads = appliances
            .iter()
            .filter(|a| assigned.contains(&a.appliance_uuid))
            .filter_map(|a| {
                let head = a.heads.get(&vm)?;
                Some(Head {
                    appliance: a.appliance_uuid.to_string(),
                    pit_uuid: head.pit_uuid,
                    delinquent: appliance_state(a, &vm, pits, now_s, settings)
                        == ApplianceState::Delinquent,
                })
            })
            .collect();
        let local: Vec<LocalPit> = pits
            .iter()
            .map(|p| LocalPit {
                pit_uuid: p.pit_uuid,
                taken_s: p.taken_s,
                backed_up: p.backed_up(),
            })
            .collect();
        let inputs = Inputs {
            pits: pits
                .iter()
                .map(|p| ClusterPit {
                    pit_uuid: p.pit_uuid,
                    taken_s: p.taken_s,
                    branch: p.branch_uuid.to_string(),
                    offers: p.offers.iter().map(|(a, o)| (a.to_string(), *o)).collect(),
                })
                .collect(),
            heads,
            local_retention: policy
                .map(|p| {
                    let calendar = settings.calendar(self.zone.clone());
                    let start_day = p.started_s.map(|s| calendar.day(s));
                    p.local_retention.keep(&local, &calendar, start_day, now_s)
                })
                .unwrap_or_default(),
            staging_origins: BTreeSet::new(),
            restores: continuity.restores.clone(),
            branch_starts: continuity.branch_start.into_iter().collect(),
            bookmarks_proven: self.bookmarks_proven,
        };
        lifetime::plan(&inputs, now_s, settings)
    }

    async fn vm(&self, vm: Uuid) -> Result<Option<Report>, String> {
        let Some(production) = self.guests.by_uuid(&vm)? else {
            return Ok(None);
        };
        let me = self.cluster.this_host();
        let live = self.cluster.live_guest(production.vmid).await?;
        if live.is_none_or(|l| l.node != me) || self.guests.leased(&production.name)? {
            return Ok(None);
        }
        let pits = self.store.pits(&vm).map_err(|e| e.message)?;
        if pits.is_empty() {
            return Ok(None);
        }
        let settings = self.store.settings().map_err(|e| e.message)?;
        let appliances = self.store.appliances().map_err(|e| e.message)?;
        let policy = self.store.policy(&vm).map_err(|e| e.message)?;
        let continuity = Continuity::of(
            vm,
            self.store.lineage(&vm).map_err(|e| e.message)?.as_ref(),
            &pits,
            &self.store.restores().map_err(|e| e.message)?,
        );
        let plan = self.plan(&pits, policy.as_ref(), &appliances, &continuity, &settings);
        let mut report = Report {
            vm,
            ..Report::default()
        };
        for pit in pits {
            if let Err(e) = self
                .pit(&production, &plan, pit, &settings, &mut report)
                .await
            {
                report.errors.push(e);
            }
        }
        Ok(Some(report))
    }

    async fn pit(
        &self,
        production: &Production,
        plan: &Plan,
        mut pit: PitRecord,
        settings: &BackupSettings,
        report: &mut Report,
    ) -> Result<(), String> {
        let me = self.cluster.this_host();
        let manifest = Manifest::open(&pit.manifest).map_err(|e| format!("{}: {e}", pit.name))?;
        let volumes: Vec<(String, bmac_core::SnapshotGuid)> = manifest
            .volumes
            .iter()
            .map(|v| (v.dataset.clone(), v.snapshot_guid))
            .collect();
        let snapshots: Vec<String> = volumes
            .iter()
            .map(|(d, _)| format!("{d}@{}", pit.name))
            .collect();
        let bookmarks: Vec<String> = volumes
            .iter()
            .map(|(d, _)| format!("{d}#{}", pit.name))
            .collect();
        let keep_bookmark = plan.keeps_bookmark(&pit.pit_uuid);
        let mut changed = false;

        if pit.presence == Presence::Snapshot && plan.releases_snapshot(&pit.pit_uuid) {
            // Read the offers again: a claim may have arrived since the plan.
            let fresh = self
                .store
                .pit(&pit.vm_uuid, &pit.pit_uuid)
                .map_err(|e| e.message)?
                .ok_or_else(|| format!("{} left the catalog", pit.name))?;
            if fresh
                .offers
                .values()
                .any(|o| o.holds(self.now_s(), settings))
            {
                return Ok(());
            }
            pit = fresh;
            let ready = if keep_bookmark {
                self.bookmark_everywhere(production, &pit, &volumes).await
            } else {
                Ok(())
            };
            match ready {
                Err(reason) => {
                    changed |= pit.release_blocked.as_deref() != Some(reason.as_str());
                    report.blocked.push((pit.name.to_string(), reason.clone()));
                    pit.release_blocked = Some(reason);
                }
                Ok(()) => {
                    let present = self.cluster.guids(me, &snapshots).await?;
                    let mut failed = None;
                    for snapshot in snapshots.iter().filter(|s| present.contains_key(*s)) {
                        if let Err(e) = self.cluster.destroy(me, snapshot).await {
                            failed = Some(e);
                        }
                    }
                    changed = true;
                    match failed {
                        Some(reason) => {
                            report.blocked.push((pit.name.to_string(), reason.clone()));
                            pit.release_blocked = Some(reason);
                        }
                        None => {
                            report.released.push(pit.name.to_string());
                            pit.release_blocked = None;
                            pit.presence = Presence::Bookmark;
                        }
                    }
                }
            }
        }

        if !keep_bookmark {
            let mut removed_all = true;
            for node in &production.placement {
                let present = match self.cluster.guids(*node, &bookmarks).await {
                    Ok(present) => present,
                    Err(e) => {
                        report.errors.push(e);
                        removed_all = false;
                        continue;
                    }
                };
                for bookmark in bookmarks.iter().filter(|b| present.contains_key(*b)) {
                    match self.cluster.destroy(*node, bookmark).await {
                        Ok(()) => report.bookmarks_removed.push(format!("{node}:{bookmark}")),
                        Err(e) => {
                            report.errors.push(e);
                            removed_all = false;
                        }
                    }
                }
            }
            if removed_all && pit.presence == Presence::Bookmark {
                self.store
                    .locked(|l| l.delete_pit(&pit.vm_uuid, &pit.pit_uuid))
                    .map_err(|e| e.message)?;
                report.dropped.push(pit.name.to_string());
                return Ok(());
            }
        }
        if changed {
            self.store
                .locked(|l| l.save_pit(pit).map(drop))
                .map_err(|e| e.message)?;
        }
        Ok(())
    }

    /// Bookmarks every volume on every placement host from its own copy of
    /// the snapshot, and checks each bookmark has the manifest's GUID.
    async fn bookmark_everywhere(
        &self,
        production: &Production,
        pit: &PitRecord,
        volumes: &[(String, bmac_core::SnapshotGuid)],
    ) -> Result<(), String> {
        for node in &production.placement {
            let names: Vec<String> = volumes
                .iter()
                .flat_map(|(d, _)| [format!("{d}@{}", pit.name), format!("{d}#{}", pit.name)])
                .collect();
            let present = self.cluster.guids(*node, &names).await?;
            for (dataset, guid) in volumes {
                let snapshot = format!("{dataset}@{}", pit.name);
                let bookmark = format!("{dataset}#{}", pit.name);
                if !present.contains_key(&bookmark) {
                    match present.get(&snapshot) {
                        Some(found) if found == guid => {
                            self.cluster.bookmark(*node, &snapshot, &bookmark).await?;
                        }
                        Some(found) => {
                            return Err(format!(
                                "{snapshot} on {node} has GUID {found}, not {guid}"
                            ));
                        }
                        None => {
                            return Err(format!(
                                "{node} has neither {snapshot} nor its bookmark; waiting for replication"
                            ));
                        }
                    }
                }
            }
            let check: Vec<String> = volumes
                .iter()
                .map(|(d, _)| format!("{d}#{}", pit.name))
                .collect();
            let made = self.cluster.guids(*node, &check).await?;
            for (dataset, guid) in volumes {
                let bookmark = format!("{dataset}#{}", pit.name);
                if made.get(&bookmark) != Some(guid) {
                    return Err(format!("{bookmark} on {node} doesn't have GUID {guid}"));
                }
            }
        }
        Ok(())
    }
}

pub const EVERY: std::time::Duration = std::time::Duration::from_secs(60);

/// Runs a pass every [`EVERY`] on this host until aborted.
pub fn spawn(node: bmac_core::HostSlot) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(EVERY);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tick.tick().await;
            let runner = bmac_exec::RealRunner;
            let cluster = super::cluster::RealCluster {
                runner: &runner,
                node,
            };
            let guests = super::capture::RegistryGuests {
                registry: bmac_registry::Registry::system(),
                runner: &runner,
                owner: format!("backup-upkeep-{node}"),
            };
            let store = BackupStore::system();
            let now = || jiff::Timestamp::now().as_millisecond();
            let upkeep = Upkeep {
                cluster: &cluster,
                store: &store,
                guests: &guests,
                now_ms: &now,
                bookmarks_proven: BOOKMARKS_PROVEN,
                zone: jiff::tz::TimeZone::system(),
            };
            for report in upkeep.run().await {
                for name in &report.released {
                    crate::log::info(&format!("backups: released the snapshots of {name}"));
                }
                for name in &report.dropped {
                    crate::log::info(&format!("backups: {name} left the catalog"));
                }
                for (name, why) in &report.blocked {
                    crate::log::warning(&format!("backups: can't release {name} yet: {why}"));
                }
                for e in &report.errors {
                    crate::log::warning(&format!("backups: {e}"));
                }
            }
        }
    })
}

/// What restores leave for one VM's snapshots.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Continuity {
    /// PITs a restore still clones from (method 1).
    pub restores: BTreeSet<Uuid>,
    /// The restored PIT the VM's current branch will start from, while that
    /// branch has no PIT of its own.
    pub branch_start: Option<Uuid>,
}

impl Continuity {
    pub fn of(
        vm: Uuid,
        lineage: Option<&Lineage>,
        pits: &[PitRecord],
        restores: &[RestoreRecord],
    ) -> Self {
        let branch_start = lineage.and_then(|l| {
            let head = l.head?;
            let on_other_branch = pits
                .iter()
                .find(|p| p.pit_uuid == head)
                .is_some_and(|p| p.branch_uuid != l.branch_uuid);
            on_other_branch.then_some(head)
        });
        Continuity {
            restores: restores
                .iter()
                .filter(|r| r.vm_uuid == vm && r.holds_cluster_snapshot())
                .map(|r| r.pit_uuid)
                .collect(),
            branch_start,
        }
    }
}
