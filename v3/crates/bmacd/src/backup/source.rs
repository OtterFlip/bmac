// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

//! What `bmac-source` asks the engine (`BACKUP.md`, "Check-ins" and
//! "Protocol"). `bmac-source` terminates the appliances' TLS and streams
//! sends; everything it decides on comes from here: who is enrolled, what
//! each appliance is offered, and whether a send names an offered PIT's
//! volume that exists on this host with the catalog's GUIDs. It names a
//! send only by IDs; the dataset, snapshot, and base come from the catalog.

use std::collections::{BTreeMap, BTreeSet};

use bmac_backup::lifetime::Offer;
use bmac_backup::manifest::Manifest;
use bmac_backup::records::{
    ApplianceRecord, Enrollment, PitRecord, Policy, Presence, ReportedHead, ScrubSummary,
};
use bmac_backup::states::Progress;
use bmac_core::{HostSlot, SnapshotGuid};
use bmac_proto::api::{ApiError, ErrorCode};
use bmac_proto::appliance::{
    ApplianceProgress, BaseKind, CheckinReply, CheckinRequest, ClaimRequest, IndexRequest,
    LocateRequest, LocateResponse, OfferBase, OfferedPit, SendRequest, SourceHost,
    normalized_fingerprint,
};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::capture::Guests;
use super::cluster::BackupCluster;
use super::source_socket::SourceTarget;
use super::status::{overall, state_text};
use super::store::{BackupStore, IndexRecord};

/// The largest index an appliance may report, in PITs.
pub const MAX_INDEX: usize = 20_000;

/// A send `bmac-source` may run: every name resolved from the catalog.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SendPlan {
    pub transfer_uuid: Uuid,
    /// `dataset@bmac-pit-…`, the snapshot sent and held while it streams.
    pub snapshot: String,
    pub guid: SnapshotGuid,
    /// `dataset@…` or `dataset#…`, for an incremental.
    pub from: Option<String>,
    pub resume_token: Option<String>,
    pub large_blocks: bool,
    pub compressed: bool,
}

impl SendPlan {
    /// The hold placed on the snapshot while it streams.
    pub fn hold_tag(&self) -> String {
        format!("bmac-send-{}", self.transfer_uuid)
    }

    /// `zfs`'s arguments. Never `-R`, `-p`, or `-w`.
    pub fn send_args(&self) -> Vec<String> {
        let mut args = vec!["send".to_string()];
        if self.large_blocks {
            args.push("-L".into());
        }
        if self.compressed {
            args.push("-c".into());
        }
        match (&self.resume_token, &self.from) {
            (Some(token), _) => {
                args.push("-t".into());
                args.push(token.clone());
            }
            (None, Some(from)) => {
                args.push("-i".into());
                args.push(from.clone());
                args.push(self.snapshot.clone());
            }
            (None, None) => args.push(self.snapshot.clone()),
        }
        args
    }
}

/// How a send ended, for the log and the metric history.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TransferDone {
    pub transfer_uuid: Uuid,
    pub vm_uuid: Uuid,
    pub pit_uuid: Uuid,
    pub volume_uuid: Uuid,
    pub bytes: u64,
    pub duration_ms: u64,
    pub error: Option<String>,
}

fn forbidden(message: impl Into<String>) -> ApiError {
    ApiError::new(ErrorCode::Forbidden, message)
}

fn invalid(message: impl Into<String>) -> ApiError {
    ApiError::new(ErrorCode::Invalid, message)
}

fn internal(message: impl Into<String>) -> ApiError {
    ApiError::new(ErrorCode::Internal, message)
}

/// The appliances `bmac-source` keeps a connection to: approved ones that
/// aren't retired.
pub fn enrolled_targets(store: &BackupStore) -> Result<Vec<SourceTarget>, ApiError> {
    Ok(store
        .appliances()?
        .into_iter()
        .filter(|a| a.enrollment == Enrollment::Approved && !a.retired)
        .filter_map(|a| {
            Some(SourceTarget {
                fingerprint: normalized_fingerprint(&a.fingerprint)?,
                name: a.name,
                address: a.address,
            })
        })
        .collect())
}

fn progress(p: ApplianceProgress) -> Progress {
    match p {
        ApplianceProgress::Normal => Progress::Normal,
        ApplianceProgress::Repairing => Progress::Repairing,
        ApplianceProgress::ReseedRequired => Progress::ReseedRequired,
    }
}

pub struct Source<'a> {
    pub store: &'a BackupStore,
    pub cluster: &'a dyn BackupCluster,
    pub guests: &'a dyn Guests,
    pub cluster_uuid: String,
    pub now_ms: &'a (dyn Fn() -> i64 + Send + Sync),
    /// The cluster's time zone, told to the appliances.
    pub time_zone: Option<String>,
}

impl Source<'_> {
    /// The approved appliance with this certificate.
    pub fn enrolled(&self, fingerprint: &str) -> Result<ApplianceRecord, ApiError> {
        let wanted = normalized_fingerprint(fingerprint)
            .ok_or_else(|| forbidden("no client certificate"))?;
        self.store
            .appliances()?
            .into_iter()
            .find(|a| normalized_fingerprint(&a.fingerprint).as_deref() == Some(wanted.as_str()))
            .filter(|a| a.enrollment == Enrollment::Approved && !a.retired)
            .ok_or_else(|| forbidden("this appliance isn't enrolled with the cluster"))
    }

    fn assigned(&self, appliance: &Uuid) -> Result<Vec<Policy>, ApiError> {
        Ok(self
            .store
            .policies()?
            .into_iter()
            .filter(|p| p.appliances.contains(appliance))
            .collect())
    }

    fn assigned_policy(&self, appliance: &Uuid, vm: &Uuid) -> Result<Policy, ApiError> {
        self.store
            .policy(vm)?
            .filter(|p| p.appliances.contains(appliance))
            .ok_or_else(|| forbidden("that VM isn't backed up to this appliance"))
    }

    pub fn checkin(
        &self,
        fingerprint: &str,
        request: &CheckinRequest,
    ) -> Result<CheckinReply, ApiError> {
        let now_ms = (self.now_ms)();
        let now_s = now_ms.div_euclid(1000);
        let enrolled = self.enrolled(fingerprint)?;
        let id = enrolled.appliance_uuid;
        if request.appliance_uuid != id {
            return Err(forbidden("the check-in names another appliance"));
        }
        let settings = self.store.settings()?;
        let policies = self.assigned(&id)?;
        let heads: BTreeMap<Uuid, ReportedHead> = request
            .heads
            .iter()
            .filter(|h| policies.iter().any(|p| p.vm_uuid == h.vm_uuid))
            .map(|h| {
                (
                    h.vm_uuid,
                    ReportedHead {
                        branch_uuid: h.branch_uuid,
                        pit_uuid: h.pit_uuid,
                    },
                )
            })
            .collect();

        let (record, catalogs) = self.store.locked(|l| {
            let mut record = l
                .store()
                .appliance(&id)?
                .ok_or_else(|| forbidden("this appliance isn't enrolled with the cluster"))?;
            let mut catalogs = Vec::new();
            for policy in &policies {
                let vm = policy.vm_uuid;
                let mut pits = l.store().pits(&vm)?;
                let mut changed = BTreeSet::new();
                for pit in pits.iter_mut() {
                    if settle(pit, &id, request, heads.get(&vm), now_s) {
                        changed.insert(pit.pit_uuid);
                    }
                }
                if let Some(head) = heads.get(&vm) {
                    changed.extend(superseded(&mut pits, &id, head));
                }
                for pit in pits.iter_mut().filter(|p| changed.contains(&p.pit_uuid)) {
                    *pit = l.save_pit(pit.clone())?;
                }
                catalogs.push((vm, pits));
            }
            let before = state_text(overall(&record, &policies, &catalogs, now_s, &settings));
            let scrub = request.scrub.as_ref().map(|s| ScrubSummary {
                state: s.state.clone(),
                finished_at_ms: s.finished_at_ms,
                errors: s.errors,
                suspect_pits: s.suspect_pits.clone(),
            });
            let changed = record.scrub != scrub
                || record.locked != request.locked
                || record.progress != progress(request.progress)
                || record.version.as_deref() != Some(request.version.as_str())
                || record.heads != heads;
            record.locked = request.locked;
            record.scrub = scrub;
            record.progress = progress(request.progress);
            record.version = Some(request.version.clone());
            record.heads = heads.clone();
            let stale = record
                .last_checkin_ms
                .is_none_or(|t| now_ms - t >= settings.ready_window_s * 1000 / 2);
            let mut seen = record.clone();
            seen.last_checkin_ms = Some(now_ms);
            let after = state_text(overall(&seen, &policies, &catalogs, now_s, &settings));
            if changed || stale || before != after {
                record = l.save_appliance(seen)?;
            }
            Ok((record, catalogs))
        })?;

        let restores = self.store.restores()?;
        let mut branch_roots = Vec::new();
        for (vm, _) in &catalogs {
            let Some(lineage) = self.store.lineage(vm)? else {
                continue;
            };
            if heads
                .get(vm)
                .is_some_and(|h| h.branch_uuid == lineage.branch_uuid)
            {
                continue;
            }
            branch_roots.extend(
                restores
                    .iter()
                    .filter(|r| r.vm_uuid == *vm && r.continues(now_ms))
                    .max_by_key(|r| r.finished_at_ms)
                    .map(|r| r.pit_uuid),
            );
        }

        let index = self.store.index(&id)?;
        let mut offers = Vec::new();
        for (vm, pits) in &catalogs {
            let held: BTreeSet<Uuid> = index
                .iter()
                .flat_map(|i| &i.pits)
                .filter(|p| p.vm_uuid == vm.to_string())
                .filter_map(|p| p.pit_uuid.parse().ok())
                .chain(heads.get(vm).map(|h| h.pit_uuid))
                .collect();
            if let Some(offer) = self.offer(&id, *vm, pits, &held, now_s, &settings)? {
                offers.push(offer);
            }
        }
        let mut tests: Vec<_> = restores
            .iter()
            .filter(|r| {
                r.appliance_uuid == id
                    && r.mode == bmac_backup::restore::Mode::Test
                    && (r.finished_at_ms.is_some() || r.error.is_some())
            })
            .collect();
        tests.sort_by_key(|r| std::cmp::Reverse(r.started_at_ms));
        let restore_tests = tests
            .into_iter()
            .take(bmac_proto::appliance::REPORTED_TESTS)
            .map(|r| bmac_proto::appliance::RestoreTestReport {
                restore_uuid: r.restore_uuid,
                vm_uuid: r.vm_uuid,
                pit_uuid: r.pit_uuid,
                host: r.host.clone(),
                started_at_ms: r.started_at_ms,
                finished_at_ms: r.finished_at_ms,
                booted: r.test.as_ref().is_some_and(|t| t.booted),
                agent: r.test.as_ref().is_some_and(|t| t.agent),
                agent_after_s: r.test.as_ref().and_then(|t| t.agent_after_s),
                error: r.error.clone(),
            })
            .collect();
        let mut shown = record.clone();
        shown.last_checkin_ms = Some(now_ms);
        Ok(CheckinReply {
            cluster_uuid: self.cluster_uuid.clone(),
            now_ms,
            checkin_interval_s: settings.checkin_interval_s,
            state: state_text(overall(&shown, &policies, &catalogs, now_s, &settings)),
            offers,
            want_index: request.index_revision != record.index_revision,
            branch_roots,
            recovery_revision: None,
            restore_tests,
            time_zone: self.time_zone.clone(),
        })
    }

    /// The newest PIT of `vm` still offered to the appliance, with the base
    /// it pulls from: the nearest ancestor in `held`, its head and indexed PITs.
    fn offer(
        &self,
        appliance: &Uuid,
        vm: Uuid,
        pits: &[PitRecord],
        held: &BTreeSet<Uuid>,
        now_s: i64,
        settings: &bmac_backup::settings::BackupSettings,
    ) -> Result<Option<OfferedPit>, ApiError> {
        let Some(pit) = pits.iter().rev().find(|p| {
            p.presence == Presence::Snapshot
                && p.offers.get(appliance).is_some_and(|o| {
                    matches!(o, Offer::Open { .. } | Offer::Claimed { .. })
                        && o.holds(now_s, settings)
                })
        }) else {
            return Ok(None);
        };
        let base = ancestors(pits, pit)
            .find(|a| held.contains(&a.pit_uuid))
            .map(|base| OfferBase {
                pit_uuid: base.pit_uuid,
                kind: match base.presence {
                    Presence::Snapshot => BaseKind::Snapshot,
                    Presence::Bookmark => BaseKind::Bookmark,
                },
            });
        let placement = self
            .guests
            .by_uuid(&vm)
            .map_err(internal)?
            .map(|p| p.placement)
            .unwrap_or_default();
        let placement =
            super::cluster::placed_members(self.cluster, &placement).map_err(internal)?;
        Ok(Some(OfferedPit {
            vm_uuid: vm,
            pit_uuid: pit.pit_uuid,
            branch_uuid: pit.branch_uuid,
            name: pit.name.to_string(),
            taken_s: pit.taken_s,
            parent: pit.parent,
            base,
            hosts: placement
                .into_iter()
                .map(|host| SourceHost {
                    host,
                    addresses: Vec::new(),
                })
                .collect(),
            manifest: pit.manifest.clone(),
        }))
    }

    pub fn claim(&self, fingerprint: &str, request: &ClaimRequest) -> Result<(), ApiError> {
        let id = self.enrolled(fingerprint)?.appliance_uuid;
        self.assigned_policy(&id, &request.vm_uuid)?;
        let now_s = (self.now_ms)().div_euclid(1000);
        let settings = self.store.settings()?;
        self.store.locked(|l| {
            let mut pit = l
                .store()
                .pit(&request.vm_uuid, &request.pit_uuid)?
                .ok_or_else(|| ApiError::new(ErrorCode::NotFound, "no such PIT in the catalog"))?;
            match pit.offers.get(&id) {
                Some(Offer::Claimed { .. }) => Ok(()),
                Some(o @ Offer::Open { .. })
                    if o.holds(now_s, &settings) && pit.presence == Presence::Snapshot =>
                {
                    pit.offers.insert(
                        id,
                        Offer::Claimed {
                            progress_s: now_s,
                            bytes: 0,
                        },
                    );
                    l.save_pit(pit).map(drop)
                }
                _ => Err(ApiError::new(
                    ErrorCode::Conflict,
                    "the PIT is no longer offered to this appliance",
                )),
            }
        })
    }

    pub fn index(&self, fingerprint: &str, request: IndexRequest) -> Result<(), ApiError> {
        let id = self.enrolled(fingerprint)?.appliance_uuid;
        if request.pits.len() > MAX_INDEX {
            return Err(invalid(format!("an index holds at most {MAX_INDEX} PITs")));
        }
        let now_ms = (self.now_ms)();
        self.store.locked(|l| {
            let revision = l.store().index(&id)?.map_or(0, |r| r.revision);
            l.save_index(IndexRecord {
                schema_version: 0,
                record_type: String::new(),
                revision,
                appliance_uuid: id,
                index_revision: request.revision,
                reported_at_ms: now_ms,
                pits: request.pits,
            })?;
            let mut appliance = l
                .store()
                .appliance(&id)?
                .ok_or_else(|| forbidden("this appliance isn't enrolled with the cluster"))?;
            appliance.index_revision = request.revision;
            l.save_appliance(appliance).map(drop)
        })
    }

    /// The sealed manifests of every PIT of the appliance's VMs.
    pub fn catalog(&self, fingerprint: &str) -> Result<Vec<String>, ApiError> {
        let id = self.enrolled(fingerprint)?.appliance_uuid;
        let mut out = Vec::new();
        for policy in self.assigned(&id)? {
            out.extend(
                self.store
                    .pits(&policy.vm_uuid)?
                    .into_iter()
                    .map(|p| p.manifest),
            );
        }
        Ok(out)
    }

    /// Which placement hosts of the appliance's VMs hold the snapshots or
    /// bookmarks with these GUIDs. Only the catalog's own volumes are
    /// looked at.
    pub async fn locate(
        &self,
        fingerprint: &str,
        request: &LocateRequest,
    ) -> Result<LocateResponse, ApiError> {
        let id = self.enrolled(fingerprint)?.appliance_uuid;
        let mut wanted: BTreeMap<HostSlot, Vec<(String, SnapshotGuid)>> = BTreeMap::new();
        for policy in self.assigned(&id)? {
            let placement = self
                .guests
                .by_uuid(&policy.vm_uuid)
                .map_err(internal)?
                .map(|p| p.placement)
                .unwrap_or_default();
            for pit in self.store.pits(&policy.vm_uuid)? {
                let Ok(manifest) = Manifest::open(&pit.manifest) else {
                    continue;
                };
                for volume in manifest.volumes {
                    if !request.guids.contains(&volume.snapshot_guid) {
                        continue;
                    }
                    for host in &placement {
                        let names = wanted.entry(*host).or_default();
                        for sep in ['@', '#'] {
                            names.push((
                                format!("{}{sep}{}", volume.dataset, pit.name),
                                volume.snapshot_guid,
                            ));
                        }
                    }
                }
            }
        }
        let mut hosts: BTreeMap<SnapshotGuid, Vec<HostSlot>> = BTreeMap::new();
        for (host, names) in wanted {
            let list: Vec<String> = names.iter().map(|(n, _)| n.clone()).collect();
            let Ok(found) = self.cluster.guids(host, &list).await else {
                continue;
            };
            for (name, guid) in names {
                if found.get(&name) == Some(&guid) {
                    let at = hosts.entry(guid).or_default();
                    if !at.contains(&host) {
                        at.push(host);
                    }
                }
            }
        }
        Ok(LocateResponse { hosts })
    }

    /// Checks a send and resolves it. Only a volume of a PIT offered to
    /// this appliance, present on this host with the catalog's GUID; an
    /// incremental only from an ancestor of it, on its branch or the one it
    /// forked from, present
    /// here as a snapshot or bookmark with its GUID; a resume only of that
    /// same snapshot.
    pub async fn authorize_send(
        &self,
        fingerprint: &str,
        request: &SendRequest,
    ) -> Result<SendPlan, ApiError> {
        let id = self.enrolled(fingerprint)?.appliance_uuid;
        self.assigned_policy(&id, &request.vm_uuid)?;
        let here = self.cluster.this_host();
        let placement = self
            .guests
            .by_uuid(&request.vm_uuid)
            .map_err(internal)?
            .map(|p| p.placement)
            .unwrap_or_default();
        if !placement.contains(&here) {
            return Err(forbidden(format!("{here} doesn't hold this VM")));
        }
        let pits = self.store.pits(&request.vm_uuid)?;
        let pit = pits
            .iter()
            .find(|p| p.pit_uuid == request.pit_uuid)
            .ok_or_else(|| ApiError::new(ErrorCode::NotFound, "no such PIT in the catalog"))?;
        if !matches!(
            pit.offers.get(&id),
            Some(Offer::Open { .. } | Offer::Claimed { .. })
        ) {
            return Err(forbidden("the PIT isn't offered to this appliance"));
        }
        if pit.presence != Presence::Snapshot {
            return Err(ApiError::new(
                ErrorCode::Conflict,
                "the PIT's snapshots were released",
            ));
        }
        let manifest = Manifest::open(&pit.manifest).map_err(|e| internal(e.to_string()))?;
        let volume = manifest
            .volumes
            .iter()
            .find(|v| v.volume_uuid == request.volume_uuid.to_string())
            .ok_or_else(|| invalid("the PIT has no such volume"))?;
        let snapshot = format!("{}@{}", volume.dataset, pit.name);

        let mut names = vec![snapshot.clone()];
        let base = match request.base {
            None => None,
            Some(base) => {
                let from = pits
                    .iter()
                    .find(|p| p.pit_uuid == base)
                    .ok_or_else(|| invalid("the base isn't in the catalog any more"))?;
                if !ancestors(&pits, pit).any(|a| a.pit_uuid == base) {
                    return Err(invalid("the base isn't an ancestor of the PIT"));
                }
                let from_manifest =
                    Manifest::open(&from.manifest).map_err(|e| internal(e.to_string()))?;
                let from_volume = from_manifest
                    .volumes
                    .iter()
                    .find(|v| v.volume_uuid == volume.volume_uuid)
                    .ok_or_else(|| invalid("the base has no such volume"))?;
                // A restored disk carries its base under the disk's own
                // name, which may not be the one the base was taken from.
                let mut candidates = Vec::new();
                for dataset in [&volume.dataset, &from_volume.dataset] {
                    for sep in ['@', '#'] {
                        let name = format!("{dataset}{sep}{}", from.name);
                        if !candidates.contains(&name) {
                            candidates.push(name);
                        }
                    }
                }
                names.extend(candidates.iter().cloned());
                Some((candidates, from_volume.snapshot_guid))
            }
        };
        let found = self
            .cluster
            .guids(here, &names)
            .await
            .map_err(|e| ApiError::new(ErrorCode::Unavailable, e))?;
        if found.get(&snapshot) != Some(&volume.snapshot_guid) {
            return Err(ApiError::new(
                ErrorCode::NotFound,
                format!("{here} doesn't have {snapshot} with the catalog's GUID"),
            ));
        }
        let from = match base {
            None => None,
            Some((candidates, guid)) => Some(
                candidates
                    .into_iter()
                    .find(|n| found.get(n) == Some(&guid))
                    .ok_or_else(|| {
                        ApiError::new(
                            ErrorCode::NotFound,
                            format!("{here} has neither a snapshot nor a bookmark of the base"),
                        )
                    })?,
            ),
        };
        if let Some(token) = &request.resume_token {
            if token.is_empty()
                || token.len() > 4096
                || !token.bytes().all(|b| b.is_ascii_graphic())
            {
                return Err(invalid("not a resume token"));
            }
            let (target, guid) = self
                .cluster
                .resume_target(token)
                .await
                .map_err(|e| invalid(format!("the resume token can't be used: {e}")))?;
            if target != snapshot || guid != volume.snapshot_guid {
                return Err(forbidden("the resume token continues another snapshot"));
            }
        }
        Ok(SendPlan {
            transfer_uuid: request.transfer_uuid,
            snapshot,
            guid: volume.snapshot_guid,
            from,
            resume_token: request.resume_token.clone(),
            large_blocks: request.large_blocks,
            compressed: request.compressed,
        })
    }
}

/// `pit`'s ancestors in the catalog, nearest first, across the branches
/// it descends from; a parent no longer in the catalog ends the walk.
pub fn ancestors<'a>(
    pits: &'a [PitRecord],
    pit: &PitRecord,
) -> impl Iterator<Item = &'a PitRecord> {
    let mut next = pit.parent;
    let mut left = pits.len();
    std::iter::from_fn(move || {
        let parent = pits.iter().find(|p| Some(p.pit_uuid) == next && left > 0)?;
        left -= 1;
        next = parent.parent;
        Some(parent)
    })
}

/// Applies what the check-in says about one PIT's offer to this
/// appliance; whether it changed.
fn settle(
    pit: &mut PitRecord,
    appliance: &Uuid,
    request: &CheckinRequest,
    head: Option<&ReportedHead>,
    now_s: i64,
) -> bool {
    let Some(offer) = pit.offers.get(appliance).copied() else {
        return false;
    };
    if !matches!(offer, Offer::Open { .. } | Offer::Claimed { .. }) {
        return false;
    }
    if head.is_some_and(|h| h.pit_uuid == pit.pit_uuid) {
        pit.offers.insert(*appliance, Offer::Published);
        return true;
    }
    if request
        .failed
        .iter()
        .any(|f| f.vm_uuid == pit.vm_uuid && f.pit_uuid == pit.pit_uuid)
    {
        pit.offers.insert(*appliance, Offer::Failed);
        return true;
    }
    let bytes = request
        .transfers
        .iter()
        .filter(|t| t.vm_uuid == pit.vm_uuid && t.pit_uuid == pit.pit_uuid)
        .map(|t| t.bytes)
        .max();
    let Some(bytes) = bytes else {
        return false;
    };
    let next = match offer {
        Offer::Claimed {
            progress_s,
            bytes: before,
        } if bytes <= before => Offer::Claimed {
            progress_s,
            bytes: before,
        },
        _ => Offer::Claimed {
            progress_s: now_s,
            bytes,
        },
    };
    let changed = next != offer;
    pit.offers.insert(*appliance, next);
    changed
}

/// Drops the older open offers to an appliance whose head is newer on the
/// same branch: it pulls from its head straight to the newest PIT.
pub fn superseded(pits: &mut [PitRecord], appliance: &Uuid, head: &ReportedHead) -> Vec<Uuid> {
    let Some(taken) = pits
        .iter()
        .find(|p| p.pit_uuid == head.pit_uuid)
        .map(|p| p.taken_s)
    else {
        return Vec::new();
    };
    let mut dropped = Vec::new();
    for pit in pits.iter_mut() {
        if pit.branch_uuid == head.branch_uuid
            && pit.taken_s < taken
            && matches!(
                pit.offers.get(appliance),
                Some(Offer::Open { .. } | Offer::Claimed { .. })
            )
        {
            pit.offers.remove(appliance);
            dropped.push(pit.pit_uuid);
        }
    }
    dropped
}
