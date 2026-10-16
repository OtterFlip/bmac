// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

//! The capture job, `backup.capture` (`BACKUP.md`, "Capturing a PIT"): run
//! by the VM's backup schedule or by "Back up now", on the VM's owner.
//!
//! 0. No assigned appliance available: no snapshot, unless local retention
//!    claims the slot; the attempt is recorded for the alert.
//! 1. Take the production orchestration lease, which blocks disk growth,
//!    placement and owner changes, staging, and removal meanwhile.
//! 2. Check that this host is the VM's live owner; never snapshot a replica.
//! 3. Read the current configuration and its disks, with a digest.
//! 4. Freeze through the guest agent, with a systemd timer that thaws even
//!    if this process dies.
//! 5. Snapshot every disk in one `zfs snapshot`, atomic within a pool.
//! 6. Thaw.
//! 7. Read the configuration again; a change fails the PIT.
//! 8. Ask replication to run until every placement host has every snapshot
//!    with the owner's GUID.
//! 9. Write the sealed manifest to the catalog and offer the PIT to the
//!    available appliances.
//!
//! A PIT that fails after step 5 has its snapshots destroyed again;
//! replication removes the replicas' copies.

use std::collections::{BTreeMap, BTreeSet};
use std::process::ExitCode;
use std::time::{Duration, Instant};

use bmac_backup::lifetime::Offer;
use bmac_backup::manifest::{
    self, Consistency, DiskRef, Firmware, Manifest, SCHEMA_VERSION, Versions, Volume,
};
use bmac_backup::records::{
    ApplianceRecord, Attempt, Enrollment, Freeze, Lineage, PitRecord, Policy, Presence,
    RECORD_SCHEMA,
};
use bmac_backup::retention::LocalPit;
use bmac_backup::states::{self, ApplianceFacts};
use bmac_backup::{PitName, settings::BackupSettings};
use bmac_core::job::PhaseStatus;
use bmac_core::{GuestName, HostSlot, SnapshotGuid};
use bmac_exec::{BoxFuture, Command, CommandRunner};
use bmac_proto::api::Role;
use bmac_proto::operations::{
    CancelRule, Category, Operation, OperationKind, ResumeRule, TargetRule,
};
use serde_json::{Value, json};
use uuid::Uuid;

use super::cluster::BackupCluster;
use super::store::BackupStore;
use crate::native::{Stop, Workflow};

pub const OPERATION: &str = "backup.capture";
/// How long a capture's orchestration lease lasts.
const LEASE_S: u32 = 7200;
/// The phase during which the guest may be frozen; a cancel waits for it.
const SNAPSHOT_PHASE: (&str, &str) = ("snapshot", "Freeze, snapshot, and thaw");
const REGISTRY: &str = "/usr/lib/bmac/legacy/scripts/host_runtime/cluster_registry.py";
const PYTHON: &str = "/usr/bin/python3";
/// How often to ask a replication job again while its snapshot is missing.
pub const REPLICATION_ASK_EVERY: Duration = Duration::from_secs(30);

pub fn operation() -> Operation {
    Operation {
        id: OPERATION.parse().expect("a valid operation id"),
        title: "Back up now".into(),
        summary: "Capture a PIT of a production VM and offer it to its appliances.".into(),
        description: "Runs on the VM's owner. Skipped when none of the VM's appliances has \
                      checked in recently, unless local retention keeps this slot. Freezes the \
                      guest's filesystems through the guest agent, snapshots every disk at \
                      once, thaws, waits until replication carries the snapshots to every \
                      placement host, and records the PIT in the catalog."
            .into(),
        category: Category::Backups,
        kind: OperationKind::Native,
        script: None,
        min_role: Role::Operator,
        mutating: true,
        destructive: false,
        supports_dry_run: false,
        params: vec![
            serde_json::from_value(json!({
                "id": "vm",
                "label": "VM",
                "type": "production",
                "help": "The production VM to back up.",
                "required": true,
            }))
            .expect("a valid parameter"),
        ],
        locks: vec!["backups:{vm}".into()],
        target: TargetRule::Owner { param: "vm".into() },
        cancel: CancelRule::Phases {
            not_during: vec![SNAPSHOT_PHASE.0.into()],
        },
        resume: ResumeRule::Never,
        notes: Some("New in v3.".into()),
    }
}

/// What the registry says about a production VM.
#[derive(Debug, Clone)]
pub struct Production {
    pub vm_uuid: Uuid,
    pub name: GuestName,
    pub vmid: u32,
    pub placement: Vec<HostSlot>,
    pub record: Value,
}

pub trait Guests: Send + Sync {
    fn production(&self, name: &GuestName) -> Result<Production, String>;
    /// The production VM with this registry ID, if it still exists.
    fn by_uuid(&self, vm: &Uuid) -> Result<Option<Production>, String>;
    /// Whether an operation holds the VM's orchestration lease now.
    fn leased(&self, name: &GuestName) -> Result<bool, String>;
    /// Takes the VM's orchestration lease for `ttl_s`; its nonce.
    fn acquire(&self, name: &GuestName, ttl_s: u32) -> BoxFuture<'_, Result<String, String>>;
    fn release(&self, name: &GuestName, nonce: &str) -> BoxFuture<'_, Result<(), String>>;
}

pub struct Context<'a> {
    pub flow: &'a Workflow,
    pub cluster: &'a dyn BackupCluster,
    pub guests: &'a dyn Guests,
    pub store: &'a BackupStore,
    pub cluster_uuid: String,
    pub bmac_version: String,
    pub now_ms: &'a (dyn Fn() -> i64 + Send + Sync),
    pub poll: Duration,
    pub replication_timeout: Duration,
    pub replication_ask_every: Duration,
    /// How long the guest may stay frozen before the timer thaws it.
    pub thaw_after: Duration,
    /// The cluster's time zone, for local retention's days.
    pub zone: jiff::tz::TimeZone,
}

impl Context<'_> {
    fn now_s(&self) -> i64 {
        (self.now_ms)().div_euclid(1000)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Captured {
    Skipped,
    Pit { pit_uuid: Uuid, name: PitName },
}

/// What the cluster knows of an appliance, for its state.
pub fn facts_of(appliance: &ApplianceRecord) -> ApplianceFacts {
    ApplianceFacts {
        last_checkin_s: appliance.last_checkin_ms.map(|ms| ms.div_euclid(1000)),
        last_published_s: None,
        oldest_unpublished_offer_s: None,
        progress: appliance.progress,
        locked: appliance.locked,
        paused: appliance.paused,
        retired: appliance.retired,
    }
}

/// The policy's approved appliances that count as there for a capture now.
pub fn available_appliances(
    policy: &Policy,
    appliances: &[ApplianceRecord],
    now_s: i64,
    settings: &BackupSettings,
) -> Vec<Uuid> {
    policy
        .appliances
        .iter()
        .filter(|id| {
            appliances.iter().any(|a| {
                a.appliance_uuid == **id
                    && a.enrollment == Enrollment::Approved
                    && states::available(&facts_of(a), now_s, settings)
            })
        })
        .copied()
        .collect()
}

/// One of the policy's appliances still pulling a PIT of the VM, with bytes
/// moved within the stalled transfer period: `(appliance, PIT, bytes)`.
pub fn still_pulling<'a>(
    policy: &Policy,
    pits: &'a [PitRecord],
    now_s: i64,
    settings: &BackupSettings,
) -> Option<(Uuid, &'a PitRecord, u64)> {
    pits.iter().rev().find_map(|pit| {
        pit.offers
            .iter()
            .find_map(|(appliance, offer)| match offer {
                Offer::Claimed { progress_s, bytes }
                    if policy.appliances.contains(appliance)
                        && now_s - progress_s <= settings.stalled_after_s =>
                {
                    Some((*appliance, pit, *bytes))
                }
                _ => None,
            })
    })
}

pub async fn capture(cx: &Context<'_>, vm: &GuestName) -> Result<Captured, Stop> {
    cx.flow.phase(
        "check",
        "Check the policy and the appliances",
        PhaseStatus::Running,
    );
    let production = cx.guests.production(vm).map_err(Stop::failed)?;
    let policy = cx
        .store
        .policy(&production.vm_uuid)
        .map_err(|e| Stop::failed(e.message))?
        .ok_or_else(|| Stop::failed(format!("{vm} has no backup policy")))?;
    if !policy.enabled {
        return Err(Stop::failed(format!("backups of {vm} are switched off")));
    }
    let settings = cx.store.settings().map_err(|e| Stop::failed(e.message))?;
    let appliances = cx.store.appliances().map_err(|e| Stop::failed(e.message))?;
    let now_s = cx.now_s();
    let records = cx
        .store
        .pits(&production.vm_uuid)
        .map_err(|e| Stop::failed(e.message))?;
    if let Some((appliance, pit, bytes)) = still_pulling(&policy, &records, now_s, &settings) {
        let name = appliances
            .iter()
            .find(|a| a.appliance_uuid == appliance)
            .map_or_else(|| appliance.to_string(), |a| a.name.clone());
        cx.flow.info(format!(
            "No PIT of {vm} this time: appliance {name} is still pulling {} ({} bytes so far, \
             moving within the last {} seconds). The next slot tries again.",
            pit.name, bytes, settings.stalled_after_s
        ));
        cx.flow.phase(
            "check",
            "Check the policy and the appliances",
            PhaseStatus::Skipped,
        );
        return Ok(Captured::Skipped);
    }
    let available = available_appliances(&policy, &appliances, now_s, &settings);
    if available.is_empty() {
        let pits: Vec<LocalPit> = records
            .iter()
            .map(|p| LocalPit {
                pit_uuid: p.pit_uuid,
                taken_s: p.taken_s,
                backed_up: p.backed_up(),
            })
            .collect();
        let calendar = settings.calendar(cx.zone.clone());
        let start_day = policy.started_s.map(|s| calendar.day(s));
        if !policy
            .local_retention
            .claims_slot(&pits, &calendar, start_day, now_s)
        {
            cx.flow.warning(format!(
                "No PIT of {vm}: none of its appliances checked in within the last {} seconds.",
                settings.ready_window_s
            ));
            record_attempt(
                cx,
                &production.vm_uuid,
                Attempt::Skipped {
                    at_ms: (cx.now_ms)(),
                },
            );
            cx.flow.phase(
                "check",
                "Check the policy and the appliances",
                PhaseStatus::Skipped,
            );
            return Ok(Captured::Skipped);
        }
        cx.flow.info(
            "No appliance is available; local retention keeps this slot, so the PIT is taken.",
        );
    }
    cx.flow.phase(
        "check",
        "Check the policy and the appliances",
        PhaseStatus::Complete,
    );

    cx.flow.phase(
        "lease",
        "Take the VM's orchestration lease",
        PhaseStatus::Running,
    );
    let nonce = cx.guests.acquire(vm, LEASE_S).await.map_err(|e| {
        Stop::failed(format!(
            "could not take {vm}'s orchestration lease (another operation may hold it): {e}"
        ))
    })?;
    cx.flow.phase(
        "lease",
        "Take the VM's orchestration lease",
        PhaseStatus::Complete,
    );
    let outcome = leased(cx, &production, &policy, &available).await;
    if let Err(e) = cx.guests.release(vm, &nonce).await {
        cx.flow
            .warning(format!("Could not release {vm}'s orchestration lease: {e}"));
    }
    match &outcome {
        Ok(Captured::Pit { pit_uuid, .. }) => {
            record_attempt(
                cx,
                &production.vm_uuid,
                Attempt::Captured {
                    at_ms: (cx.now_ms)(),
                    pit_uuid: *pit_uuid,
                },
            );
        }
        Ok(Captured::Skipped) => {}
        Err(Stop::Failed { message, .. }) => {
            record_attempt(
                cx,
                &production.vm_uuid,
                Attempt::Failed {
                    at_ms: (cx.now_ms)(),
                    message: message.clone(),
                },
            );
        }
        Err(Stop::Cancelled) => {}
    }
    outcome
}

fn record_attempt(cx: &Context<'_>, vm: &Uuid, attempt: Attempt) {
    let result = cx.store.locked(|l| {
        let mut lineage = l.store().lineage(vm)?.unwrap_or_else(|| new_lineage(*vm));
        lineage.last_attempt = Some(attempt);
        l.save_lineage(lineage).map(drop)
    });
    if let Err(e) = result {
        cx.flow
            .warning(format!("Could not record the attempt: {}", e.message));
    }
}

fn new_lineage(vm: Uuid) -> Lineage {
    Lineage {
        schema_version: RECORD_SCHEMA,
        record_type: String::new(),
        revision: 0,
        vm_uuid: vm,
        branch_uuid: Uuid::new_v4(),
        head: None,
        volumes: BTreeMap::new(),
        last_attempt: None,
    }
}

/// A disk with its dataset.
struct Disk {
    reference: DiskRef,
    dataset: String,
}

async fn read_disks(
    cx: &Context<'_>,
    vmid: u32,
) -> Result<(String, BTreeMap<String, String>, Vec<Disk>), Stop> {
    let raw = cx.cluster.config_text(vmid).await.map_err(Stop::failed)?;
    let (text, parsed) = manifest::current_config(&raw);
    let mut disks = Vec::new();
    for reference in manifest::disks(&parsed).map_err(Stop::failed)? {
        let dataset = cx
            .cluster
            .dataset(&reference.volume_id)
            .await
            .map_err(Stop::failed)?;
        disks.push(Disk { reference, dataset });
    }
    Ok((text, parsed, disks))
}

async fn leased(
    cx: &Context<'_>,
    production: &Production,
    policy: &Policy,
    available: &[Uuid],
) -> Result<Captured, Stop> {
    let vm = &production.name;
    let vmid = production.vmid;
    let me = cx.cluster.this_host();
    cx.flow.phase(
        "owner",
        "Check the VM's owner and configuration",
        PhaseStatus::Running,
    );
    let live = cx
        .cluster
        .live_guest(vmid)
        .await
        .map_err(Stop::failed)?
        .ok_or_else(|| Stop::failed(format!("Proxmox has no VM {vmid}")))?;
    if live.node != me {
        return Err(Stop::failed(format!(
            "{vm} is on {} now, not {me}; the next slot backs it up there",
            live.node
        )));
    }
    let (config_text, config, disks) = read_disks(cx, vmid).await?;
    let pools: BTreeSet<&str> = disks
        .iter()
        .map(|d| d.dataset.split('/').next().unwrap_or_default())
        .collect();
    if pools.len() > 1 && !policy.accept_multi_pool {
        return Err(Stop::failed(format!(
            "{vm}'s disks are on several pools ({}), where one snapshot command isn't atomic; \
             its backup policy must accept that first",
            pools.into_iter().collect::<Vec<_>>().join(", ")
        )));
    }
    cx.flow.phase(
        "owner",
        "Check the VM's owner and configuration",
        PhaseStatus::Complete,
    );

    let name = PitName::new(cx.now_s(), Uuid::new_v4().as_u128() as u32);
    let snapshots: Vec<String> = disks
        .iter()
        .map(|d| format!("{}@{name}", d.dataset))
        .collect();

    cx.flow.phase_with(
        SNAPSHOT_PHASE.0,
        SNAPSHOT_PHASE.1,
        PhaseStatus::Running,
        false,
    );
    let (consistency, evidence) =
        snapshot_frozen(cx, policy, vmid, live.running, &snapshots).await?;
    cx.flow.phase_with(
        SNAPSHOT_PHASE.0,
        SNAPSHOT_PHASE.1,
        PhaseStatus::Complete,
        true,
    );
    cx.flow.info(format!(
        "Took {name} of {} disks ({consistency:?}).",
        snapshots.len()
    ));

    let finished = finish(
        cx,
        production,
        policy,
        available,
        FinishInput {
            name: name.clone(),
            snapshots: &snapshots,
            disks: &disks,
            config_text,
            config,
            consistency,
            evidence,
        },
    )
    .await;
    match finished {
        Ok(pit_uuid) => Ok(Captured::Pit { pit_uuid, name }),
        Err(stop) => {
            for snapshot in &snapshots {
                if let Err(e) = cx.cluster.destroy(me, snapshot).await {
                    cx.flow.warning(format!(
                        "Could not destroy {snapshot} of the failed PIT: {e}"
                    ));
                }
            }
            Err(stop)
        }
    }
}

/// Steps 4 to 6: freeze (or not), snapshot, thaw.
async fn snapshot_frozen(
    cx: &Context<'_>,
    policy: &Policy,
    vmid: u32,
    running: bool,
    snapshots: &[String],
) -> Result<(Consistency, Value), Stop> {
    let mut consistency = Consistency::Crash;
    let mut evidence = json!({"freeze": "not attempted"});
    let mut timer = None;
    if policy.freeze != Freeze::Never && running {
        let unit = cx
            .cluster
            .arm_thaw(vmid, cx.thaw_after)
            .await
            .map_err(Stop::failed)?;
        timer = Some(unit);
        match cx.cluster.freeze(vmid).await {
            Ok(answer) => {
                consistency = Consistency::Filesystem;
                evidence = json!({"freeze": answer});
            }
            Err(e) => {
                let _ = cx.cluster.thaw(vmid).await;
                if let Some(unit) = timer.take() {
                    let _ = cx.cluster.disarm_thaw(&unit).await;
                }
                if policy.freeze == Freeze::Required {
                    return Err(Stop::failed(format!(
                        "the guest agent didn't freeze the filesystems: {e}"
                    )));
                }
                cx.flow.warning(format!(
                    "The guest agent didn't freeze the filesystems ({e}); this PIT is crash-consistent."
                ));
                evidence = json!({"freeze": format!("failed: {e}")});
            }
        }
    }
    let taken = cx.cluster.snapshot(snapshots).await;
    if consistency == Consistency::Filesystem
        && let Err(e) = cx.cluster.thaw(vmid).await
    {
        cx.flow.warning(format!(
            "Thawing failed ({e}); the timer thaws the guest shortly."
        ));
    }
    if let Some(unit) = timer
        && let Err(e) = cx.cluster.disarm_thaw(&unit).await
    {
        cx.flow
            .warning(format!("Could not stop the thaw timer {unit}: {e}"));
    }
    taken.map_err(Stop::failed)?;
    Ok((consistency, evidence))
}

struct FinishInput<'a> {
    name: PitName,
    snapshots: &'a [String],
    disks: &'a [Disk],
    config_text: String,
    config: BTreeMap<String, String>,
    consistency: Consistency,
    evidence: Value,
}

/// Steps 7 to 9.
async fn finish(
    cx: &Context<'_>,
    production: &Production,
    policy: &Policy,
    available: &[Uuid],
    input: FinishInput<'_>,
) -> Result<Uuid, Stop> {
    let me = cx.cluster.this_host();
    cx.flow.phase(
        "recheck",
        "Check the configuration didn't change",
        PhaseStatus::Running,
    );
    let (again, _, _) = read_disks(cx, production.vmid).await?;
    if again != input.config_text {
        return Err(Stop::failed(
            "the VM's configuration changed during the capture; the PIT is discarded",
        ));
    }
    let owned = cx
        .cluster
        .guids(me, input.snapshots)
        .await
        .map_err(Stop::failed)?;
    let mut guids = BTreeMap::new();
    for snapshot in input.snapshots {
        let guid = owned.get(snapshot).ok_or_else(|| {
            Stop::failed(format!("{snapshot} is missing right after it was taken"))
        })?;
        guids.insert(snapshot.clone(), *guid);
    }
    cx.flow.phase(
        "recheck",
        "Check the configuration didn't change",
        PhaseStatus::Complete,
    );

    cx.flow.phase(
        "replicate",
        "Replicate to every placement host",
        PhaseStatus::Running,
    );
    replicate(cx, production, &guids).await?;
    let owner = cx
        .cluster
        .live_guest(production.vmid)
        .await
        .map_err(Stop::failed)?;
    if owner.as_ref().is_none_or(|o| o.node != me) {
        return Err(Stop::failed(format!(
            "{} moved during the capture; the PIT is discarded and the next slot backs it up",
            production.name
        )));
    }
    cx.flow.phase(
        "replicate",
        "Replicate to every placement host",
        PhaseStatus::Complete,
    );

    cx.flow.phase(
        "catalog",
        "Record the PIT and offer it",
        PhaseStatus::Running,
    );
    let (proxmox, openzfs) = cx.cluster.versions().await.map_err(Stop::failed)?;
    let mut volumes_props = Vec::new();
    for disk in input.disks {
        volumes_props.push(
            cx.cluster
                .volume_props(&disk.dataset)
                .await
                .map_err(Stop::failed)?,
        );
    }
    let pit_uuid = Uuid::new_v4();
    let now_s = cx.now_s();
    let vm_uuid = production.vm_uuid;
    let written = cx.store.locked(|l| {
        let mut lineage = l
            .store()
            .lineage(&vm_uuid)?
            .unwrap_or_else(|| new_lineage(vm_uuid));
        let volumes: Vec<Volume> = input
            .disks
            .iter()
            .zip(&volumes_props)
            .map(|(disk, (volsize, volblocksize))| {
                let volume_uuid = *lineage
                    .volumes
                    .entry(disk.dataset.clone())
                    .or_insert_with(Uuid::new_v4);
                Volume {
                    volume_uuid: volume_uuid.to_string(),
                    slot: disk.reference.slot.clone(),
                    role: disk.reference.role,
                    dataset: disk.dataset.clone(),
                    snapshot_guid: guids[&format!("{}@{}", disk.dataset, input.name)],
                    volsize: *volsize,
                    volblocksize: *volblocksize,
                    boot: disk.reference.boot,
                }
            })
            .collect();
        let mut manifest = Manifest {
            schema_version: SCHEMA_VERSION,
            cluster_uuid: cx.cluster_uuid.clone(),
            vm_uuid: vm_uuid.to_string(),
            vmid: production.vmid,
            pit_uuid: pit_uuid.to_string(),
            branch_uuid: lineage.branch_uuid.to_string(),
            parent_pit_uuid: lineage.head.map(|h| h.to_string()),
            snapshot: input.name.clone(),
            taken_at_s: input.name.taken_s(),
            consistency: input.consistency,
            evidence: input.evidence.clone(),
            multi_pool: policy.accept_multi_pool
                && input
                    .disks
                    .iter()
                    .map(|d| d.dataset.split('/').next())
                    .collect::<BTreeSet<_>>()
                    .len()
                    > 1,
            versions: Versions {
                proxmox: proxmox.clone(),
                openzfs: openzfs.clone(),
                bmac: cx.bmac_version.clone(),
            },
            config_text: input.config_text.clone(),
            config_sha256: manifest::sha256_hex(input.config_text.as_bytes()),
            config: input.config.clone(),
            volumes,
            firmware: firmware(&input.config),
            registry: production.record.clone(),
            prerequisites: prerequisites(&input.config),
            sha256: String::new(),
        };
        let sealed = manifest.seal();
        let pit = PitRecord {
            schema_version: RECORD_SCHEMA,
            record_type: String::new(),
            revision: 0,
            pit_uuid,
            vm_uuid,
            branch_uuid: lineage.branch_uuid,
            parent: lineage.head,
            name: input.name.clone(),
            taken_s: input.name.taken_s(),
            consistency: input.consistency,
            manifest: sealed,
            presence: Presence::Snapshot,
            offers: available
                .iter()
                .map(|a| (*a, Offer::Open { offered_s: now_s }))
                .collect(),
            release_blocked: None,
        };
        lineage.head = Some(pit_uuid);
        let taken_s = pit.taken_s;
        l.save_pit(pit)?;
        l.save_lineage(lineage)?;
        if let Some(mut policy) = l.store().policy(&vm_uuid)?
            && policy.started_s.is_none()
        {
            policy.started_s = Some(taken_s);
            l.save_policy(policy)?;
        }
        Ok(())
    });
    written.map_err(|e| Stop::failed(format!("could not record the PIT: {}", e.message)))?;
    cx.flow.phase(
        "catalog",
        "Record the PIT and offer it",
        PhaseStatus::Complete,
    );
    cx.flow.result(json!({
        "pit_uuid": pit_uuid,
        "snapshot": input.name,
        "consistency": input.consistency,
        "offered_to": available,
    }));
    Ok(pit_uuid)
}

/// Step 8: every placement host has every snapshot with the owner's GUID.
async fn replicate(
    cx: &Context<'_>,
    production: &Production,
    guids: &BTreeMap<String, SnapshotGuid>,
) -> Result<(), Stop> {
    let me = cx.cluster.this_host();
    let targets: Vec<HostSlot> = super::cluster::placed_members(cx.cluster, &production.placement)
        .map_err(Stop::failed)?
        .into_iter()
        .filter(|h| *h != me)
        .collect();
    if targets.is_empty() {
        return Ok(());
    }
    let jobs = cx
        .cluster
        .replication_jobs(production.vmid)
        .await
        .map_err(Stop::failed)?;
    let mut job_of = BTreeMap::new();
    for target in &targets {
        let job = jobs.iter().find(|j| j.target == *target).ok_or_else(|| {
            Stop::failed(format!(
                "{} has no replication job to {target}",
                production.name
            ))
        })?;
        job_of.insert(*target, job.id.clone());
    }
    let names: Vec<String> = guids.keys().cloned().collect();
    let started = Instant::now();
    let mut asked: BTreeMap<HostSlot, Instant> = BTreeMap::new();
    let mut pending: BTreeSet<HostSlot> = targets.iter().copied().collect();
    loop {
        cx.flow.check()?;
        for target in pending.clone() {
            let found = cx
                .cluster
                .guids(target, &names)
                .await
                .map_err(Stop::failed)?;
            for (name, guid) in &found {
                if guids.get(name) != Some(guid) {
                    return Err(Stop::failed(format!(
                        "{name} on {target} has GUID {guid}, not the owner's {}; the PIT isn't offered",
                        guids[name]
                    )));
                }
            }
            if found.len() == names.len() {
                cx.flow.info(format!(
                    "{target} has the snapshots, with the owner's GUIDs."
                ));
                pending.remove(&target);
                continue;
            }
            if asked
                .get(&target)
                .is_none_or(|t| t.elapsed() >= cx.replication_ask_every)
            {
                // pvesr refuses while another run holds the guest's lock.
                if let Err(e) = cx.cluster.schedule_replication(&job_of[&target]).await {
                    cx.flow.info(format!(
                        "Replication {} is busy ({e}); asking again.",
                        job_of[&target]
                    ));
                }
                asked.insert(target, Instant::now());
            }
        }
        if pending.is_empty() {
            return Ok(());
        }
        if started.elapsed() >= cx.replication_timeout {
            let late: Vec<String> = pending.iter().map(|h| h.to_string()).collect();
            return Err(Stop::failed(format!(
                "replication didn't bring the snapshots to {} within {} seconds",
                late.join(", "),
                cx.replication_timeout.as_secs()
            )));
        }
        tokio::time::sleep(cx.poll).await;
    }
}

fn firmware(config: &BTreeMap<String, String>) -> Firmware {
    Firmware {
        bios: config
            .get("bios")
            .cloned()
            .unwrap_or_else(|| "seabios".into()),
        machine: config.get("machine").cloned(),
        secure_boot: config
            .get("efidisk0")
            .is_some_and(|e| e.split(',').any(|p| p.trim() == "pre-enrolled-keys=1")),
    }
}

/// Configuration a restore can't recreate by itself.
fn prerequisites(config: &BTreeMap<String, String>) -> Vec<String> {
    config
        .iter()
        .filter(|(key, _)| {
            ["hostpci", "usb", "serial", "parallel"]
                .iter()
                .any(|p| key.starts_with(p))
        })
        .map(|(key, value)| format!("{key}: {value} (host hardware)"))
        .collect()
}

/// The registry and its orchestration lease, through the legacy CLI.
pub struct RegistryGuests<'a> {
    pub registry: bmac_registry::Registry,
    pub runner: &'a dyn CommandRunner,
    pub owner: String,
}

impl RegistryGuests<'_> {
    async fn cli(&self, args: &[&str]) -> Result<String, String> {
        let command = Command::new(PYTHON)
            .arg(REGISTRY)
            .args(args.iter().copied())
            .timeout(Duration::from_secs(60));
        self.runner
            .run(command)
            .await
            .map(|o| o.stdout_text())
            .map_err(|e| e.to_string())
    }
}

fn production_of(resource: &bmac_registry::Resource) -> Result<Production, String> {
    let name = &resource.name;
    if resource.kind != bmac_registry::Kind::Production {
        return Err(format!("{name} is not a production VM"));
    }
    Ok(Production {
        vm_uuid: resource
            .id
            .parse()
            .map_err(|_| format!("{name}'s registry ID is not a UUID"))?,
        name: *name,
        vmid: resource.vmid.get(),
        placement: resource.placement.clone(),
        record: serde_json::to_value(resource).map_err(|e| e.to_string())?,
    })
}

impl Guests for RegistryGuests<'_> {
    fn production(&self, name: &GuestName) -> Result<Production, String> {
        let resource = self
            .registry
            .find_resource(*name)
            .map_err(|e| e.to_string())?
            .ok_or_else(|| format!("the registry has no {name}"))?;
        production_of(&resource)
    }

    fn by_uuid(&self, vm: &Uuid) -> Result<Option<Production>, String> {
        let resources = self.registry.resources().map_err(|e| e.to_string())?;
        resources
            .iter()
            .find(|r| {
                r.kind == bmac_registry::Kind::Production && r.id.parse::<Uuid>().ok() == Some(*vm)
            })
            .map(production_of)
            .transpose()
    }

    fn leased(&self, name: &GuestName) -> Result<bool, String> {
        let record = self
            .registry
            .orchestration(*name)
            .map_err(|e| e.to_string())?;
        let now = jiff::Timestamp::now();
        Ok(record.and_then(|o| o.lease).is_some_and(|lease| {
            lease
                .expires_at
                .parse::<jiff::Timestamp>()
                .map_or(true, |expires| expires > now)
        }))
    }

    fn acquire(&self, name: &GuestName, ttl_s: u32) -> BoxFuture<'_, Result<String, String>> {
        let name = name.to_string();
        let ttl = ttl_s.to_string();
        Box::pin(async move {
            let text = self.cli(&["orchestration-get", &name]).await?;
            let record: Value = serde_json::from_str(&text).map_err(|e| e.to_string())?;
            if record["install_phase"] != "network-finalized" {
                return Err(format!("{name}'s installation isn't finished"));
            }
            let network = if record["final_network_enabled"] == true {
                "yes"
            } else {
                "no"
            };
            let startup = record["startup_sha256"]
                .as_str()
                .unwrap_or("none")
                .to_string();
            let iso = record["source_iso_sha256"]
                .as_str()
                .ok_or("the orchestration record has no source_iso_sha256")?
                .to_string();
            let mode = record["install_mode"]
                .as_str()
                .ok_or("the orchestration record has no install_mode")?
                .to_string();
            let nonce = Uuid::new_v4().simple().to_string();
            self.cli(&[
                "orchestration-acquire",
                &name,
                "--nonce",
                &nonce,
                "--owner",
                &self.owner,
                "--ttl-seconds",
                &ttl,
                "--final-network-enabled",
                network,
                "--startup-sha256",
                &startup,
                "--source-iso-sha256",
                &iso,
                "--install-mode",
                &mode,
            ])
            .await?;
            Ok(nonce)
        })
    }

    fn release(&self, name: &GuestName, nonce: &str) -> BoxFuture<'_, Result<(), String>> {
        let (name, nonce) = (name.to_string(), nonce.to_string());
        Box::pin(async move {
            self.cli(&["orchestration-release", &name, "--nonce", &nonce])
                .await
                .map(drop)
        })
    }
}

/// `bmacd native backup.capture VM`.
pub async fn run(flow: &Workflow, args: &[String]) -> ExitCode {
    let spec = operation();
    flow.begin(OPERATION, &spec.title, &spec.description);
    let outcome = async {
        let [vm] = args else {
            return Err(Stop::failed("usage: backup.capture VM"));
        };
        let vm: GuestName = vm
            .parse()
            .map_err(|_| Stop::failed(format!("{vm} is not a guest name")))?;
        let node: HostSlot = crate::engine::listen::node_name()
            .parse()
            .map_err(|_| Stop::failed("this host's name is not a BMAC host name"))?;
        let runner = bmac_exec::RealRunner;
        let cluster = super::cluster::RealCluster {
            runner: &runner,
            node,
        };
        let guests = RegistryGuests {
            registry: bmac_registry::Registry::system(),
            runner: &runner,
            owner: format!("backup-capture-{node}-{}", std::process::id()),
        };
        let cluster_uuid = crate::joins::JoinStore::system()
            .cluster()
            .map_err(|e| Stop::failed(e.message))?
            .map(|c| c.cluster_uuid)
            .ok_or_else(|| Stop::failed("the cluster has no identity record yet"))?;
        let now = || jiff::Timestamp::now().as_millisecond();
        let cx = Context {
            flow,
            cluster: &cluster,
            guests: &guests,
            store: &BackupStore::system(),
            cluster_uuid,
            bmac_version: crate::VERSION.into(),
            now_ms: &now,
            poll: Duration::from_secs(10),
            replication_timeout: Duration::from_secs(15 * 60),
            replication_ask_every: REPLICATION_ASK_EVERY,
            thaw_after: Duration::from_secs(60),
            zone: jiff::tz::TimeZone::system(),
        };
        capture(&cx, &vm).await.map(drop)
    }
    .await;
    flow.finish(&outcome)
}
