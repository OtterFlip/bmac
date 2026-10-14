// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

//! Bringing a production VM's own disks back to a PIT (`BACKUP.md`,
//! "Restore"): `backup.restore_into` (restored volumes swapped in),
//! `backup.rollback` (the disks rolled back to the PIT's snapshots here), and
//! `backup.promote` (a VM restored as new made production). Each runs on the
//! VM's owner, after a typed confirmation, and wraps the change the same way:
//!
//! 1. Refuse without quorum, or anywhere but the VM's owner.
//! 2. Take the VM's orchestration lease, stop the VM (through HA when HA
//!    manages it, so HA doesn't start it again), and delete its replication
//!    jobs, which removes the replicas: a changed disk has no replication
//!    snapshot in common with them.
//! 3. Change the disks. A swap renames each current zvol to
//!    `<pool>/bmac-replaced/<vmid>/<volume_uuid>-<time>` with every snapshot
//!    and bookmark it has, then the restored zvol to the disk's name. If any
//!    step fails, what moved is put back, and the VM stays stopped.
//! 4. Recreate the replication jobs and wait for each one's first (full)
//!    sync, then start the VM and release the lease.
//! 5. Backups continue from the PIT on a new branch.
//!
//! Promotion swaps the restored VM's disks into the production VM, so the
//! production identity (registry record, VMID, MAC addresses, routes, HA
//! resource) stays as it is and needs no registry change; the restored VM's
//! own configuration, with its new MACs, is dropped.

use std::collections::{BTreeMap, BTreeSet};
use std::process::ExitCode;
use std::time::{Duration, Instant};

use bmac_backup::lifetime::Offer;
use bmac_backup::manifest::{self, Manifest};
use bmac_backup::records::{PitRecord, Presence, RECORD_SCHEMA, RestoreRecord};
use bmac_backup::restore::{self, Mode, human_bytes, staging};
use bmac_core::job::{FieldType, PhaseStatus, Severity};
use bmac_core::{GuestName, HostSlot};
use bmac_exec::{BoxFuture, Command, CommandRunner};
use bmac_proto::api::Role;
use bmac_proto::operations::{
    CancelRule, Category, Operation, OperationKind, ResumeRule, TargetRule,
};
use serde_json::{Value, json};
use uuid::Uuid;

use super::capture::{Guests, Production, RegistryGuests};
use super::restore::{self as job, Plan, Storage};
use super::store::BackupStore;
use crate::native::disks::{choice, field, text};
use crate::native::{Stop, Workflow};

pub const RESTORE_INTO: &str = "backup.restore_into";
pub const ROLLBACK: &str = "backup.rollback";
pub const PROMOTE: &str = "backup.promote";
pub const CLEANUP: &str = "backup.cleanup";
/// The lease outlasts a full re-replication of large disks.
const LEASE_S: u32 = 48 * 3600;
const FENCE_PHASE: (&str, &str) = ("fence", "Stop the VM and remove its replicas");
const CHANGE_PHASE: (&str, &str) = ("change", "Change the disks");
const REPLICATE_PHASE: (&str, &str) = ("replicate", "Replicate to every placement host again");
const CLEANUP_PHASE: (&str, &str) = ("remove", "Remove the set-aside disks");
const ZFS: &str = "/usr/sbin/zfs";
const QM: &str = "/usr/sbin/qm";
const PVESH: &str = "/usr/bin/pvesh";
const PVESR: &str = "/usr/bin/pvesr";
const BASH: &str = "/usr/bin/bash";
/// `$1` the clone's origin, `$2` the clone's newest snapshot, `$3` the copy.
const COPY_CLONE: &str = "set -euo pipefail
/usr/sbin/zfs send -- \"$1\" | /usr/sbin/zfs receive -o volmode=none -- \"$3\"
/usr/sbin/zfs send -I \"$1\" -- \"$2\" | /usr/sbin/zfs receive -- \"$3\"";
/// A local copy of a whole disk.
const COPY_TIMEOUT: Duration = Duration::from_secs(24 * 3600);
/// Set-aside disks nest one level per restore over a clone.
const MAX_PROMOTES: usize = 16;
/// A replication job that failed this many times since it was recreated
/// stops the wait with Proxmox's error.
const MAX_SYNC_FAILURES: u64 = 3;
const PVECM: &str = "/usr/bin/pvecm";
const HA_MANAGER: &str = "/usr/sbin/ha-manager";

fn spec(id: &str, title: &str, summary: &str, description: &str) -> Operation {
    Operation {
        id: id.parse().expect("a valid operation id"),
        title: title.into(),
        summary: summary.into(),
        description: description.into(),
        category: Category::Backups,
        kind: OperationKind::Native,
        script: None,
        min_role: Role::Admin,
        mutating: true,
        destructive: true,
        supports_dry_run: false,
        params: vec![
            serde_json::from_value(json!({
                "id": "vm",
                "label": "VM",
                "type": "production",
                "help": "The production VM whose disks change.",
                "required": true,
            }))
            .expect("a valid parameter"),
        ],
        locks: vec!["production:{vm}".into(), "backups:{vm}".into()],
        target: TargetRule::Owner { param: "vm".into() },
        cancel: CancelRule::Phases {
            not_during: vec![CHANGE_PHASE.0.into(), REPLICATE_PHASE.0.into()],
        },
        resume: ResumeRule::Never,
        notes: Some("New in v3.".into()),
    }
}

pub fn operations() -> Vec<Operation> {
    vec![
        spec(
            RESTORE_INTO,
            "Restore into the VM",
            "Replace a production VM's disks with a PIT from an appliance.",
            "Needs a restore grant from the appliance's administrator. Each volume comes by \
             the cheapest method the snapshots here allow and is checked before the VM \
             stops. The current disks are set aside with their snapshots, not written over, \
             until a cleanup. Replication then copies every disk to the placement hosts in \
             full, and backups continue from the PIT on a new branch.",
        ),
        spec(
            ROLLBACK,
            "Roll back to a PIT",
            "Roll a production VM's disks back to a PIT still on the cluster.",
            "Only for a PIT whose snapshots are still here (local retention), and only when \
             no clone depends on a newer snapshot. Every newer snapshot and bookmark of the \
             disks is destroyed. Replication then copies every disk to the placement hosts \
             in full, and backups continue from the PIT on a new branch.",
        ),
        spec(
            PROMOTE,
            "Promote a restored VM",
            "Make a VM restored beside production the production VM.",
            "The restored VM's disks replace the production VM's, which keeps its identity: \
             its VMID, MAC addresses, routes, and HA resource. The production VM's disks are \
             set aside until a cleanup, and the restored VM's own configuration is removed. \
             Refused without quorum.",
        ),
        cleanup_operation(),
    ]
}

fn cleanup_operation() -> Operation {
    Operation {
        id: CLEANUP.parse().expect("a valid operation id"),
        title: "Remove set-aside disks".into(),
        summary: "Destroy the disks a restore or promotion set aside on this host.".into(),
        description: "Each chosen disk is destroyed with its snapshots and bookmarks. A \
                      restored disk that is still a clone of one of its snapshots is promoted \
                      first, so it keeps working; a disk that a restore in progress still \
                      depends on is kept."
            .into(),
        category: Category::Backups,
        kind: OperationKind::Native,
        script: None,
        min_role: Role::Admin,
        mutating: true,
        destructive: true,
        supports_dry_run: false,
        params: vec![
            serde_json::from_value(json!({
                "id": "host",
                "label": "Host",
                "type": "host",
                "help": "The host whose set-aside disks are removed.",
                "required": true,
            }))
            .expect("a valid parameter"),
        ],
        locks: vec!["restore:{host}".into()],
        target: TargetRule::Host {
            param: "host".into(),
        },
        cancel: CancelRule::Phases {
            not_during: vec![CLEANUP_PHASE.0.into()],
        },
        resume: ResumeRule::Never,
        notes: Some("New in v3.".into()),
    }
}

/// One storage replication job, as recreated.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Job {
    pub id: String,
    pub target: HostSlot,
    pub schedule: String,
    pub rate: Option<String>,
    pub comment: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobState {
    pub busy: bool,
    pub failing: bool,
    pub last_sync_s: i64,
    pub fail_count: u64,
    pub error: Option<String>,
}

/// What changing a production VM's disks asks of its owner.
pub trait ReplaceHost: Send + Sync {
    fn quorate(&self) -> BoxFuture<'_, Result<bool, String>>;
    /// The node the guest is on, if it exists.
    fn owner(&self, vmid: u32) -> BoxFuture<'_, Result<Option<HostSlot>, String>>;
    fn config_text(&self, vmid: u32) -> BoxFuture<'_, Result<String, String>>;
    /// The dataset of a ZFS-backed Proxmox volume.
    fn dataset(&self, volume_id: &str) -> BoxFuture<'_, Result<String, String>>;
    fn ha_managed(&self, vmid: u32) -> BoxFuture<'_, Result<bool, String>>;
    fn set_ha(&self, vmid: u32, started: bool) -> BoxFuture<'_, Result<(), String>>;
    fn running(&self, vmid: u32) -> BoxFuture<'_, Result<bool, String>>;
    fn shutdown(&self, vmid: u32) -> BoxFuture<'_, Result<(), String>>;
    fn start(&self, vmid: u32) -> BoxFuture<'_, Result<(), String>>;
    fn jobs(&self, vmid: u32) -> BoxFuture<'_, Result<Vec<Job>, String>>;
    /// `pvesr delete`: Proxmox removes the replica, then the job.
    fn delete_job(&self, id: &str) -> BoxFuture<'_, Result<(), String>>;
    fn create_job(&self, job: &Job) -> BoxFuture<'_, Result<(), String>>;
    fn job_state(&self, id: &str) -> BoxFuture<'_, Result<JobState, String>>;
    fn schedule_now(&self, id: &str) -> BoxFuture<'_, Result<(), String>>;
    /// Makes the unmounted parents of `dataset`.
    fn ensure_parent(&self, dataset: &str) -> BoxFuture<'_, Result<(), String>>;
    fn rename(&self, from: &str, to: &str) -> BoxFuture<'_, Result<(), String>>;
    fn inherit_volmode(&self, dataset: &str) -> BoxFuture<'_, Result<(), String>>;
    fn promote(&self, dataset: &str) -> BoxFuture<'_, Result<(), String>>;
    fn snapshot(&self, name: &str) -> BoxFuture<'_, Result<(), String>>;
    /// Copies a clone into `target`, which then owns its history: a full
    /// stream of the clone's `origin`, then the clone's snapshots from it
    /// to `snapshot`. `target` is received with `volmode=none`.
    fn copy_clone(
        &self,
        origin: &str,
        snapshot: &str,
        target: &str,
    ) -> BoxFuture<'_, Result<(), String>>;
    fn guid(&self, snapshot: &str) -> BoxFuture<'_, Result<Option<String>, String>>;
    /// `zfs destroy -r` of a dataset or a snapshot; never `-R`.
    fn destroy(&self, name: &str) -> BoxFuture<'_, Result<(), String>>;
    /// A clone's origin snapshot.
    fn origin(&self, dataset: &str) -> BoxFuture<'_, Result<Option<String>, String>>;
    /// The dataset's snapshots, oldest first.
    fn snapshots_of(&self, dataset: &str) -> BoxFuture<'_, Result<Vec<String>, String>>;
    /// The clones of a snapshot.
    fn clones(&self, snapshot: &str) -> BoxFuture<'_, Result<Vec<String>, String>>;
    /// `zfs rollback -r`: newer snapshots and bookmarks go; never `-R`.
    fn rollback(&self, snapshot: &str) -> BoxFuture<'_, Result<(), String>>;
    fn rescan(&self, vmid: u32) -> BoxFuture<'_, Result<(), String>>;
    /// Removes a VM whose disks were moved away.
    fn forget_vm(&self, vmid: u32) -> BoxFuture<'_, Result<(), String>>;
}

pub struct Context<'a> {
    pub restore: job::Context<'a>,
    pub host: &'a dyn ReplaceHost,
    pub guests: &'a dyn Guests,
    pub stop_wait: Duration,
    pub replication_timeout: Duration,
    pub ask_every: Duration,
}

impl Context<'_> {
    fn flow(&self) -> &Workflow {
        self.restore.flow
    }

    fn me(&self) -> HostSlot {
        self.restore.host.this_host()
    }

    fn now_ms(&self) -> i64 {
        (self.restore.now_ms)()
    }

    fn store(&self) -> &BackupStore {
        self.restore.store
    }
}

/// A production VM stopped, with its replicas removed, under its lease.
#[derive(Debug)]
pub struct Fenced {
    pub production: Production,
    nonce: String,
    ha: bool,
    jobs: Vec<Job>,
    was_running: bool,
}

/// One disk to replace: the restored zvol takes `current`'s name.
#[derive(Debug, Clone)]
pub struct Swap {
    pub slot: String,
    pub current: String,
    pub restored: String,
    pub volume_uuid: String,
}

fn failed(message: impl Into<String>) -> Stop {
    Stop::failed(message)
}

async fn checks(cx: &Context<'_>, production: &Production) -> Result<(), Stop> {
    if !cx.host.quorate().await.map_err(failed)? {
        return Err(failed(
            "The cluster isn't quorate. A VM's disks change only with quorum, so its old \
             owner can't come back midway.",
        ));
    }
    let me = cx.me();
    match cx.host.owner(production.vmid).await.map_err(failed)? {
        Some(owner) if owner == me => Ok(()),
        Some(owner) => Err(failed(format!(
            "{} runs on {owner}; this job runs on {me}",
            production.name
        ))),
        None => Err(failed(format!(
            "{} (VM {}) doesn't exist",
            production.name, production.vmid
        ))),
    }
}

async fn wait_until<F, Fut>(
    cx: &Context<'_>,
    limit: Duration,
    what: &str,
    done: F,
) -> Result<(), Stop>
where
    F: Fn() -> Fut,
    Fut: std::future::Future<Output = Result<bool, String>>,
{
    let started = Instant::now();
    loop {
        if done().await.map_err(failed)? {
            return Ok(());
        }
        if started.elapsed() >= limit {
            return Err(failed(format!("{what} within {} seconds", limit.as_secs())));
        }
        tokio::time::sleep(cx.restore.poll).await;
    }
}

pub async fn fence(cx: &Context<'_>, production: &Production) -> Result<Fenced, Stop> {
    let flow = cx.flow();
    flow.phase(FENCE_PHASE.0, FENCE_PHASE.1, PhaseStatus::Running);
    let nonce = cx
        .guests
        .acquire(&production.name, LEASE_S)
        .await
        .map_err(|e| {
            failed(format!(
                "could not take {}'s orchestration lease (another operation may hold it): {e}",
                production.name
            ))
        })?;
    let mut fenced = Fenced {
        production: production.clone(),
        nonce,
        ha: false,
        jobs: Vec::new(),
        was_running: false,
    };
    match stop_and_drop(cx, &mut fenced).await {
        Ok(()) => {
            flow.phase(FENCE_PHASE.0, FENCE_PHASE.1, PhaseStatus::Complete);
            Ok(fenced)
        }
        Err(stop) => {
            let start = fenced.was_running;
            if let Err(e) = unfence(cx, &fenced, start).await {
                flow.warning(format!("Putting the VM back: {}", stop_text(&e)));
            }
            Err(stop)
        }
    }
}

async fn stop_and_drop(cx: &Context<'_>, f: &mut Fenced) -> Result<(), Stop> {
    let (host, flow) = (cx.host, cx.flow());
    let vmid = f.production.vmid;
    f.ha = host.ha_managed(vmid).await.map_err(failed)?;
    f.jobs = host.jobs(vmid).await.map_err(failed)?;
    f.was_running = host.running(vmid).await.map_err(failed)?;
    if f.ha {
        host.set_ha(vmid, false).await.map_err(failed)?;
        flow.info(format!("HA keeps vm:{vmid} stopped."));
    } else if f.was_running {
        host.shutdown(vmid).await.map_err(failed)?;
    }
    wait_until(
        cx,
        cx.stop_wait,
        &format!("VM {vmid} didn't stop"),
        move || async move { host.running(vmid).await.map(|r| !r) },
    )
    .await?;
    flow.info(format!("{} is stopped.", f.production.name));
    let ids: BTreeSet<String> = f.jobs.iter().map(|j| j.id.clone()).collect();
    for job in &f.jobs {
        host.delete_job(&job.id).await.map_err(failed)?;
        flow.info(format!(
            "Removing replication {} and its replica on {}.",
            job.id, job.target
        ));
    }
    if !ids.is_empty() {
        let ids = &ids;
        wait_until(
            cx,
            cx.replication_timeout,
            "Proxmox didn't finish removing the replicas",
            move || async move {
                let left = host.jobs(vmid).await?;
                Ok(left.iter().all(|j| !ids.contains(&j.id)))
            },
        )
        .await?;
    }
    Ok(())
}

fn stop_text(stop: &Stop) -> String {
    match stop {
        Stop::Failed { message, .. } => message.clone(),
        Stop::Cancelled => "cancelled".into(),
    }
}

/// Recreates the replication jobs and waits for each one's first sync,
/// starts the VM when `start`, and releases the lease.
pub async fn unfence(cx: &Context<'_>, f: &Fenced, start: bool) -> Result<(), Stop> {
    let outcome = replicate(cx, f).await;
    let outcome = match outcome {
        Ok(()) if start => restart(cx, f).await,
        Ok(()) => {
            cx.flow().warning(format!(
                "{} stays stopped.{}",
                f.production.name,
                if f.ha {
                    format!(
                        " Start it with: ha-manager set vm:{} --state started",
                        f.production.vmid
                    )
                } else {
                    String::new()
                }
            ));
            Ok(())
        }
        Err(e) => Err(e),
    };
    if let Err(e) = cx.guests.release(&f.production.name, &f.nonce).await {
        cx.flow().warning(format!(
            "Could not release {}'s orchestration lease: {e}",
            f.production.name
        ));
    }
    outcome
}

async fn restart(cx: &Context<'_>, f: &Fenced) -> Result<(), Stop> {
    let vmid = f.production.vmid;
    if f.ha {
        cx.host.set_ha(vmid, true).await.map_err(failed)?;
        cx.flow().info(format!("HA starts vm:{vmid} again."));
    } else {
        cx.host.start(vmid).await.map_err(failed)?;
        cx.flow().info(format!("{} is started.", f.production.name));
    }
    Ok(())
}

async fn replicate(cx: &Context<'_>, f: &Fenced) -> Result<(), Stop> {
    if f.jobs.is_empty() {
        return Ok(());
    }
    let (host, flow) = (cx.host, cx.flow());
    flow.phase_with(
        REPLICATE_PHASE.0,
        REPLICATE_PHASE.1,
        PhaseStatus::Running,
        false,
    );
    let present: BTreeSet<String> = host
        .jobs(f.production.vmid)
        .await
        .map_err(failed)?
        .into_iter()
        .map(|j| j.id)
        .collect();
    let since_s = cx.now_ms().div_euclid(1000);
    for job in f.jobs.iter().filter(|j| !present.contains(&j.id)) {
        host.create_job(job).await.map_err(failed)?;
        flow.info(format!(
            "Replication {} to {} is back; its first run copies every disk in full.",
            job.id, job.target
        ));
    }
    let started = Instant::now();
    let mut asked: BTreeMap<String, Instant> = BTreeMap::new();
    let mut pending: Vec<&Job> = f.jobs.iter().collect();
    while !pending.is_empty() {
        let mut still = Vec::new();
        for job in pending {
            let state = host.job_state(&job.id).await.map_err(failed)?;
            if !state.busy && !state.failing && state.last_sync_s >= since_s {
                flow.info(format!("{} has its replica again.", job.target));
                continue;
            }
            if state.fail_count >= MAX_SYNC_FAILURES {
                return Err(failed(format!(
                    "replication to {} failed {} times: {}; it carries on in Proxmox",
                    job.target,
                    state.fail_count,
                    state.error.as_deref().unwrap_or("no error given")
                )));
            }
            if !state.busy
                && asked
                    .get(&job.id)
                    .is_none_or(|t| t.elapsed() >= cx.ask_every)
            {
                // pvesr refuses while another run holds the guest's lock.
                let _ = host.schedule_now(&job.id).await;
                asked.insert(job.id.clone(), Instant::now());
            }
            still.push(job);
        }
        pending = still;
        if pending.is_empty() {
            break;
        }
        if started.elapsed() >= cx.replication_timeout {
            let late: Vec<String> = pending.iter().map(|j| j.target.to_string()).collect();
            return Err(failed(format!(
                "replication to {} didn't finish within {} seconds; it carries on in Proxmox",
                late.join(", "),
                cx.replication_timeout.as_secs()
            )));
        }
        tokio::time::sleep(cx.restore.poll).await;
    }
    flow.phase_with(
        REPLICATE_PHASE.0,
        REPLICATE_PHASE.1,
        PhaseStatus::Complete,
        false,
    );
    Ok(())
}

/// Swaps every restored zvol in, setting the current ones aside; the
/// aside datasets by slot. A failure puts back what moved.
pub async fn swap(
    cx: &Context<'_>,
    vmid: u32,
    swaps: &[Swap],
) -> Result<BTreeMap<String, String>, Stop> {
    let host = cx.host;
    let at_s = cx.now_ms().div_euclid(1000);
    let mut moved: Vec<(String, String)> = Vec::new();
    let mut aside = BTreeMap::new();
    let outcome = async {
        for s in swaps {
            let pool = s.current.split('/').next().unwrap_or_default();
            let to = restore::replaced(pool, vmid, &s.volume_uuid, at_s);
            host.ensure_parent(&to).await.map_err(failed)?;
            host.rename(&s.current, &to).await.map_err(failed)?;
            moved.push((s.current.clone(), to.clone()));
            host.rename(&s.restored, &s.current).await.map_err(failed)?;
            moved.push((s.restored.clone(), s.current.clone()));
            host.inherit_volmode(&s.current).await.map_err(failed)?;
            aside.insert(s.slot.clone(), to);
        }
        Ok::<(), Stop>(())
    }
    .await;
    if let Err(stop) = outcome {
        let mut states = Vec::new();
        for (from, to) in moved.iter().rev() {
            match host.rename(to, from).await {
                Ok(()) => states.push(format!("{from} is back")),
                Err(e) => states.push(format!("{to} is still where it moved ({e})")),
            }
        }
        return Err(Stop::failed(format!(
            "{}; {}",
            stop_text(&stop),
            if states.is_empty() {
                "no disk moved".to_string()
            } else {
                states.join("; ")
            }
        )));
    }
    for s in swaps {
        own_history(cx, &s.slot, &s.current).await?;
    }
    if let Err(e) = host.rescan(vmid).await {
        cx.flow()
            .warning(format!("Could not rescan VM {vmid}'s disk sizes: {e}"));
    }
    Ok(aside)
}

/// Makes a swapped-in disk own its snapshots: Proxmox replication sends a
/// clone's stream, which its target can't receive without the origin. It
/// is promoted over the set-aside and staging datasets it is a clone of,
/// one level each. A clone of anything else (another VM's disk) is copied
/// instead, since promoting it would take that disk's history.
async fn own_history(cx: &Context<'_>, slot: &str, disk: &str) -> Result<(), Stop> {
    let pool = disk.split('/').next().unwrap_or_default();
    let ours = |dataset: &str| {
        dataset.starts_with(&format!("{pool}/bmac-replaced/"))
            || dataset.starts_with(&format!("{pool}/bmac-restore/"))
    };
    for _ in 0..MAX_PROMOTES {
        let Some(origin) = cx.host.origin(disk).await.map_err(failed)? else {
            return Ok(());
        };
        let parent = origin.split('@').next().unwrap_or_default();
        if !ours(parent) {
            return copy_clone(cx, slot, disk, &origin).await;
        }
        if let Err(e) = cx.host.promote(disk).await {
            cx.flow().warning(format!(
                "{slot}: could not promote {disk} over {parent} ({e}); replication \
                 can't send it until it is"
            ));
            return Ok(());
        }
    }
    cx.flow().warning(format!(
        "{slot}: {disk} is still a clone after {MAX_PROMOTES} promotions"
    ));
    Ok(())
}

/// Replaces `disk`, a clone of another disk's `origin`, with a copy that
/// keeps the origin snapshot's GUID and the clone's own writes. The copy
/// is made under the staging tree, so a failure leaves the clone in place
/// and the copy for the cleanup.
async fn copy_clone(cx: &Context<'_>, slot: &str, disk: &str, origin: &str) -> Result<(), Stop> {
    let host = cx.host;
    let pool = disk.split('/').next().unwrap_or_default();
    let at_s = cx.now_ms().div_euclid(1000);
    let leaf = disk.rsplit('/').next().unwrap_or_default();
    let copy = format!("{pool}/bmac-restore/copy-{at_s}/{leaf}");
    let snapshot = format!("{disk}@bmac-copy-{at_s}");
    let origin_name = origin.split_once('@').map_or("", |(_, n)| n);
    cx.flow().info(format!(
        "{slot}: {disk} is a clone of {origin}, another disk's snapshot; copying it \
         so that replication can send it."
    ));
    host.snapshot(&snapshot).await.map_err(failed)?;
    host.ensure_parent(&copy).await.map_err(failed)?;
    host.copy_clone(origin, &snapshot, &copy)
        .await
        .map_err(failed)?;
    let copied = host
        .guid(&format!("{copy}@{origin_name}"))
        .await
        .map_err(failed)?;
    let wanted = host.guid(origin).await.map_err(failed)?;
    let last = host
        .guid(&format!("{copy}@bmac-copy-{at_s}"))
        .await
        .map_err(failed)?;
    if copied.is_none() || copied != wanted || last != host.guid(&snapshot).await.map_err(failed)? {
        return Err(failed(format!(
            "{slot}: the copy {copy} doesn't have the snapshots of {disk}; {disk} is \
             unchanged"
        )));
    }
    host.destroy(disk).await.map_err(failed)?;
    host.rename(&copy, disk).await.map_err(failed)?;
    host.inherit_volmode(disk).await.map_err(failed)?;
    for leftover in [
        format!("{disk}@bmac-copy-{at_s}"),
        format!("{pool}/bmac-restore/copy-{at_s}"),
    ] {
        if let Err(e) = host.destroy(&leftover).await {
            cx.flow()
                .warning(format!("{slot}: could not remove {leftover} ({e})"));
        }
    }
    cx.flow()
        .info(format!("{slot}: {disk} owns its history now."));
    Ok(())
}

/// The VM's current disks: the ZFS dataset of each by slot.
async fn current_disks(
    cx: &Context<'_>,
    vmid: u32,
) -> Result<BTreeMap<String, (String, String)>, Stop> {
    let text = cx.host.config_text(vmid).await.map_err(failed)?;
    let (_, config) = manifest::current_config(&text);
    let mut out = BTreeMap::new();
    for disk in manifest::disks(&config).map_err(failed)? {
        let dataset = cx.host.dataset(&disk.volume_id).await.map_err(failed)?;
        out.insert(disk.slot, (disk.volume_id, dataset));
    }
    Ok(out)
}

fn parent(dataset: &str) -> &str {
    dataset.rsplit_once('/').map_or(dataset, |(p, _)| p)
}

/// The PIT's catalog record from its sealed manifest.
fn pit_record(sealed: &str) -> Result<(PitRecord, Manifest), String> {
    let manifest = Manifest::open(sealed).map_err(|e| e.to_string())?;
    let uuid = |text: &str, what: &str| {
        text.parse::<Uuid>()
            .map_err(|_| format!("the manifest's {what} isn't a UUID"))
    };
    let record = PitRecord {
        schema_version: RECORD_SCHEMA,
        record_type: String::new(),
        revision: 0,
        pit_uuid: uuid(&manifest.pit_uuid, "PIT ID")?,
        vm_uuid: uuid(&manifest.vm_uuid, "VM ID")?,
        branch_uuid: uuid(&manifest.branch_uuid, "branch ID")?,
        parent: manifest
            .parent_pit_uuid
            .as_deref()
            .map(|p| uuid(p, "parent ID"))
            .transpose()?,
        name: manifest.snapshot.clone(),
        taken_s: manifest.taken_at_s,
        consistency: manifest.consistency,
        manifest: sealed.to_string(),
        presence: Presence::Snapshot,
        offers: BTreeMap::new(),
        release_blocked: None,
    };
    Ok((record, manifest))
}

/// Backups of `vm` continue from the PIT on a new branch: the PIT is in
/// the catalog again (if it had gone), and the lineage's head is it, with
/// each disk's volume under its current dataset.
pub fn continue_from(
    store: &BackupStore,
    vm: Uuid,
    sealed: &str,
    datasets: &BTreeMap<String, String>,
) -> Result<Uuid, String> {
    let (record, manifest) = pit_record(sealed)?;
    let mut lineage = restore::continued(vm, &manifest, datasets)?;
    store
        .locked(|l| {
            if let Some(old) = l.store().lineage(&vm)? {
                lineage.revision = old.revision;
                lineage.last_attempt = old.last_attempt;
            }
            if l.store().pit(&vm, &record.pit_uuid)?.is_none() {
                l.save_pit(PitRecord {
                    vm_uuid: vm,
                    ..record.clone()
                })?;
            }
            l.save_lineage(lineage.clone())
        })
        .map(|l| l.branch_uuid)
        .map_err(|e| e.message)
}

fn new_record(
    cx: &Context<'_>,
    mode: Mode,
    production: &Production,
    sealed: &str,
    appliance: Uuid,
) -> Result<RestoreRecord, Stop> {
    let (pit, _) = pit_record(sealed).map_err(failed)?;
    Ok(RestoreRecord {
        schema_version: RECORD_SCHEMA,
        record_type: String::new(),
        revision: 0,
        restore_uuid: Uuid::new_v4(),
        vm_uuid: production.vm_uuid,
        pit_uuid: pit.pit_uuid,
        branch_uuid: pit.branch_uuid,
        appliance_uuid: appliance,
        mode,
        host: cx.me().to_string(),
        vmid: Some(production.vmid),
        name: Some(production.name.to_string()),
        methods: BTreeMap::new(),
        started_at_ms: cx.now_ms(),
        finished_at_ms: None,
        error: None,
        ended_at_ms: None,
        replaced: BTreeMap::new(),
        test: None,
        manifest: Some(sealed.to_string()),
    })
}

/// Records how the job ended, and that backups continue when it stood.
fn finish(
    cx: &Context<'_>,
    mut record: RestoreRecord,
    outcome: &Result<BTreeMap<String, String>, Stop>,
) -> RestoreRecord {
    match outcome {
        Ok(datasets) => {
            record.finished_at_ms = Some(cx.now_ms());
            if let Some(sealed) = &record.manifest {
                match continue_from(cx.store(), record.vm_uuid, sealed, datasets) {
                    Ok(branch) => cx.flow().info(format!(
                        "Backups continue from the PIT on a new branch, {branch}."
                    )),
                    Err(e) => cx.flow().warning(format!(
                        "Backups couldn't be set to continue from the PIT: {e}; the next one \
                         starts a new history"
                    )),
                }
            }
        }
        Err(stop) => {
            record.error = Some(stop_text(stop));
            record.ended_at_ms = Some(cx.now_ms());
        }
    }
    job::record_restore(&cx.restore, &record)
}

fn vm_label(manifest: &Manifest, taken_s: i64) -> String {
    format!("{} at {}", job::vm_name(manifest), job::when(taken_s))
}

/// The replicas that will be copied again, for the confirmation.
fn replication_note(jobs: &[Job], bytes: u64) -> Option<String> {
    (!jobs.is_empty()).then(|| {
        let targets: Vec<String> = jobs.iter().map(|j| j.target.to_string()).collect();
        format!(
            "Replication then copies every disk ({}) in full to {}.",
            human_bytes(bytes),
            targets.join(", ")
        )
    })
}

// Restore into the existing VM.

pub async fn restore_into(cx: &Context<'_>, vm: &GuestName) -> Result<Value, Stop> {
    let flow = cx.flow();
    let production = cx.guests.production(vm).map_err(failed)?;
    checks(cx, &production).await?;
    flow.phase("plan", "Choose the PIT and plan", PhaseStatus::Running);
    let (appliance, grant, mut index) = job::source(&cx.restore).await?;
    index.pits.retain(|p| p.vm_uuid == production.vm_uuid);
    index
        .pits
        .sort_by_key(|p| std::cmp::Reverse((p.taken_s, p.pit_uuid)));
    if index.pits.is_empty() {
        return Err(failed(format!("That grant names no PIT of {vm}.")));
    }
    let mut pit = field("pit", FieldType::Select, "PIT");
    pit.options = index
        .pits
        .iter()
        .map(|p| {
            choice(
                &p.pit_uuid.to_string(),
                &job::when(p.taken_s),
                Some(p.name.clone()),
            )
        })
        .collect();
    pit.default = Some(json!(index.pits[0].pit_uuid.to_string()));
    let values = flow
        .input_group("What to restore", None, vec![pit], |_| Ok(()))
        .await?;
    let chosen = index
        .pits
        .iter()
        .find(|p| p.pit_uuid.to_string() == text(&values, "pit"))
        .cloned()
        .ok_or_else(|| failed("that PIT is no longer offered"))?;
    let manifest = Manifest::open(&chosen.manifest).map_err(|e| failed(e.to_string()))?;
    if manifest.cluster_uuid != cx.restore.cluster_uuid
        || manifest.pit_uuid != chosen.pit_uuid.to_string()
    {
        return Err(failed("the PIT's manifest is for another PIT or cluster"));
    }

    let current = current_disks(cx, production.vmid).await?;
    let mut problems = Vec::new();
    let mut placed = Vec::new();
    for volume in &manifest.volumes {
        let Some((volume_id, dataset)) = current.get(&volume.slot) else {
            problems.push(format!("{} isn't a disk of {vm} now", volume.slot));
            continue;
        };
        let storage = Storage {
            id: volume_id.split(':').next().unwrap_or_default().to_string(),
            dataset: parent(dataset).to_string(),
            block: volume.volblocksize,
        };
        placed.push((volume.clone(), storage));
    }
    if !problems.is_empty() {
        return Err(failed(format!(
            "The PIT doesn't fit {vm} as it is now: {}. Restore it as a new VM instead.",
            problems.join("; ")
        )));
    }
    let mut plan = Plan {
        appliance,
        grant,
        pit: chosen,
        manifest,
        name: vm.to_string(),
        isolate: false,
        start: true,
        test: false,
        vmid: None,
        volumes: Vec::new(),
    };
    plan.volumes = job::size_up(&cx.restore, &plan, &index, placed).await?;
    flow.phase("plan", "Choose the PIT and plan", PhaseStatus::Complete);

    let jobs = cx.host.jobs(production.vmid).await.map_err(failed)?;
    let mut details: Vec<String> = plan
        .volumes
        .iter()
        .map(|p| job::describe(p, |id| id.to_string()))
        .collect();
    let total: u64 = plan.volumes.iter().map(|p| p.bytes).sum();
    details.push(format!(
        "{} to transfer in all, before the VM stops",
        human_bytes(total)
    ));
    let restored_slots: BTreeSet<&str> = plan
        .volumes
        .iter()
        .map(|p| p.volume.slot.as_str())
        .collect();
    for slot in current
        .keys()
        .filter(|s| !restored_slots.contains(s.as_str()))
    {
        details.push(format!("{slot} isn't in the PIT and stays as it is."));
    }
    details.push(
        "The current disks are set aside under <pool>/bmac-replaced, with their snapshots, \
         until a cleanup."
            .into(),
    );
    let volsizes: u64 = plan.volumes.iter().map(|p| p.volume.volsize).sum();
    details.extend(replication_note(&jobs, volsizes));
    let confirmed = flow
        .confirm(
            "Restore into the VM",
            &format!(
                "{vm} stops, and its disks are replaced by those of {}. Type the VM's name to \
                 go ahead.",
                vm_label(&plan.manifest, plan.pit.taken_s)
            ),
            Severity::Critical,
            Some(&vm.to_string()),
            details,
        )
        .await?;
    if !confirmed {
        return Err(Stop::Cancelled);
    }

    let mut record = new_record(
        cx,
        Mode::IntoExisting,
        &production,
        &plan.pit.manifest,
        plan.appliance.appliance_uuid,
    )?;
    record.methods = plan
        .volumes
        .iter()
        .map(|p| (p.volume.slot.clone(), p.method.number()))
        .collect();
    let record = job::record_restore(&cx.restore, &record);
    let job_id = record.restore_uuid.simple().to_string();
    let pools: BTreeSet<String> = plan
        .volumes
        .iter()
        .map(|p| p.storage.pool().to_string())
        .collect();

    let outcome = async {
        flow.phase_with(
            "receive",
            "Bring each volume to the PIT",
            PhaseStatus::Running,
            true,
        );
        for pool in &pools {
            cx.restore
                .host
                .staging_parent(pool, &job_id)
                .await
                .map_err(failed)?;
        }
        let mut swaps = Vec::new();
        for planned in &plan.volumes {
            let target = staging(planned.storage.pool(), &job_id, &planned.volume.volume_uuid);
            job::bring(&cx.restore, &plan, planned, &target).await?;
            swaps.push(Swap {
                slot: planned.volume.slot.clone(),
                current: current[&planned.volume.slot].1.clone(),
                restored: target,
                volume_uuid: planned.volume.volume_uuid.clone(),
            });
        }
        flow.phase_with(
            "receive",
            "Bring each volume to the PIT",
            PhaseStatus::Complete,
            true,
        );
        let vmid = production.vmid;
        change(cx, &production, move |cx| {
            Box::pin(async move { swap(cx, vmid, &swaps).await.map(|aside| (aside, ())) })
        })
        .await
    }
    .await;
    for pool in &pools {
        let staged = format!("{pool}/bmac-restore/{job_id}");
        if let Err(e) = cx.restore.host.destroy_tree(&staged).await {
            flow.warning(format!("Could not remove {staged}: {e}"));
        }
    }
    let mut record = record;
    let datasets = outcome.map(|(aside, ())| {
        record.replaced = aside.clone();
        current_datasets(&current, aside.keys())
    });
    finish(cx, record, &datasets);
    datasets?;
    let result = json!({"vm": vm.to_string(), "vmid": production.vmid, "pit": plan.pit.pit_uuid});
    flow.result(result.clone());
    Ok(result)
}

fn current_datasets<'a>(
    current: &BTreeMap<String, (String, String)>,
    slots: impl Iterator<Item = &'a String>,
) -> BTreeMap<String, String> {
    slots
        .filter_map(|s| current.get(s).map(|(_, d)| (s.clone(), d.clone())))
        .collect()
}

type Change<'c, T> = BoxFuture<'c, Result<(BTreeMap<String, String>, T), Stop>>;

/// Fences the VM, runs `step`, and unfences it: started again when the
/// step worked, stopped when it didn't.
async fn change<'c, T>(
    cx: &'c Context<'c>,
    production: &Production,
    step: impl FnOnce(&'c Context<'c>) -> Change<'c, T>,
) -> Result<(BTreeMap<String, String>, T), Stop> {
    let fenced = fence(cx, production).await?;
    cx.flow()
        .phase_with(CHANGE_PHASE.0, CHANGE_PHASE.1, PhaseStatus::Running, false);
    let outcome = step(cx).await;
    match outcome {
        Ok(done) => {
            cx.flow()
                .phase_with(CHANGE_PHASE.0, CHANGE_PHASE.1, PhaseStatus::Complete, false);
            unfence(cx, &fenced, true).await?;
            Ok(done)
        }
        Err(stop) => {
            if let Err(e) = unfence(cx, &fenced, false).await {
                cx.flow()
                    .warning(format!("Putting replication back: {}", stop_text(&e)));
            }
            Err(stop)
        }
    }
}

// Rollback in place.

pub async fn rollback(cx: &Context<'_>, vm: &GuestName) -> Result<Value, Stop> {
    let flow = cx.flow();
    let production = cx.guests.production(vm).map_err(failed)?;
    checks(cx, &production).await?;
    flow.phase(
        "plan",
        "Choose the PIT and check the disks",
        PhaseStatus::Running,
    );
    let mut pits: Vec<PitRecord> = cx
        .store()
        .pits(&production.vm_uuid)
        .map_err(|e| failed(e.message))?;
    pits.sort_by_key(|p| std::cmp::Reverse((p.taken_s, p.pit_uuid)));
    let candidates: Vec<&PitRecord> = pits
        .iter()
        .filter(|p| p.presence == Presence::Snapshot)
        .collect();
    if candidates.is_empty() {
        return Err(failed(format!(
            "No PIT of {vm} has its snapshots on the cluster. Restore one from an appliance \
             instead."
        )));
    }
    let mut pit = field("pit", FieldType::Select, "PIT");
    pit.options = candidates
        .iter()
        .map(|p| {
            choice(
                &p.pit_uuid.to_string(),
                &job::when(p.taken_s),
                Some(p.name.to_string()),
            )
        })
        .collect();
    pit.default = Some(json!(candidates[0].pit_uuid.to_string()));
    let values = flow
        .input_group("Roll back to", None, vec![pit], |_| Ok(()))
        .await?;
    let chosen: PitRecord = candidates
        .iter()
        .find(|p| p.pit_uuid.to_string() == text(&values, "pit"))
        .map(|p| (*p).clone())
        .ok_or_else(|| failed("that PIT is no longer here"))?;
    let manifest = Manifest::open(&chosen.manifest).map_err(|e| failed(e.to_string()))?;

    let current = current_disks(cx, production.vmid).await?;
    let mut problems = Vec::new();
    let mut targets = Vec::new();
    let mut doomed = 0usize;
    for volume in &manifest.volumes {
        let Some((_, dataset)) = current.get(&volume.slot) else {
            problems.push(format!("{} isn't a disk of {vm} now", volume.slot));
            continue;
        };
        let snapshot = format!("{dataset}@{}", chosen.name);
        if cx.restore.host.guid(&snapshot).await.map_err(failed)? != Some(volume.snapshot_guid) {
            problems.push(format!(
                "{} has no snapshot of the PIT with its GUID",
                volume.slot
            ));
            continue;
        }
        let all = cx.host.snapshots_of(dataset).await.map_err(failed)?;
        let newer: Vec<&String> = all.iter().skip_while(|s| **s != snapshot).skip(1).collect();
        for s in &newer {
            let clones = cx.host.clones(s).await.map_err(failed)?;
            if !clones.is_empty() {
                problems.push(format!(
                    "{s} has clones ({}); restore the PIT as a new VM and promote it instead",
                    clones.join(", ")
                ));
            }
        }
        doomed += newer.len();
        targets.push((volume.slot.clone(), dataset.clone(), snapshot));
    }
    let later: Vec<&PitRecord> = pits.iter().filter(|p| p.taken_s > chosen.taken_s).collect();
    for p in &later {
        if p.offers
            .values()
            .any(|o| matches!(o, Offer::Claimed { .. }))
        {
            problems.push(format!(
                "an appliance is pulling the later PIT {}; wait for it",
                p.name
            ));
        }
    }
    if !problems.is_empty() {
        return Err(failed(format!(
            "{vm} can't be rolled back to that PIT: {}",
            problems.join("; ")
        )));
    }
    flow.phase(
        "plan",
        "Choose the PIT and check the disks",
        PhaseStatus::Complete,
    );

    let jobs = cx.host.jobs(production.vmid).await.map_err(failed)?;
    let mut details = vec![format!(
        "{doomed} newer snapshot{} of the disks are destroyed, with the bookmarks after the PIT.",
        if doomed == 1 { "" } else { "s" }
    )];
    if !later.is_empty() {
        details.push(format!(
            "{} later PIT{} leave the cluster's catalog; the appliances keep theirs.",
            later.len(),
            if later.len() == 1 { "" } else { "s" }
        ));
    }
    let slots: BTreeSet<&str> = targets.iter().map(|t| t.0.as_str()).collect();
    for slot in current.keys().filter(|s| !slots.contains(s.as_str())) {
        details.push(format!("{slot} isn't in the PIT and stays as it is."));
    }
    details.extend(replication_note(
        &jobs,
        manifest.volumes.iter().map(|v| v.volsize).sum(),
    ));
    let confirmed = flow
        .confirm(
            "Roll back to a PIT",
            &format!(
                "{vm} stops, and its disks go back to {}. Type the VM's name to go ahead.",
                vm_label(&manifest, chosen.taken_s)
            ),
            Severity::Critical,
            Some(&vm.to_string()),
            details,
        )
        .await?;
    if !confirmed {
        return Err(Stop::Cancelled);
    }

    let record = job::record_restore(
        &cx.restore,
        &new_record(
            cx,
            Mode::Rollback,
            &production,
            &chosen.manifest,
            Uuid::nil(),
        )?,
    );
    let outcome = change(cx, &production, |cx| {
        let targets = targets.clone();
        Box::pin(async move {
            let mut done = Vec::new();
            for (slot, dataset, snapshot) in &targets {
                if let Err(e) = cx.host.rollback(snapshot).await {
                    let mut states: Vec<String> =
                        done.iter().map(|s| format!("{s} is rolled back")).collect();
                    states.push(format!("{slot} is as it was ({e})"));
                    return Err(failed(format!(
                        "the rollback stopped: {}",
                        states.join("; ")
                    )));
                }
                cx.flow()
                    .info(format!("{slot}: {dataset} is back at the PIT."));
                done.push(slot.clone());
            }
            Ok((
                targets
                    .iter()
                    .map(|(slot, dataset, _)| (slot.clone(), dataset.clone()))
                    .collect(),
                (),
            ))
        })
    })
    .await;
    if outcome.is_ok() {
        for p in &later {
            if let Err(e) = cx
                .store()
                .locked(|l| l.delete_pit(&production.vm_uuid, &p.pit_uuid))
            {
                flow.warning(format!(
                    "Could not drop PIT {} from the catalog: {}",
                    p.name, e.message
                ));
            }
        }
    }
    let datasets = outcome.map(|(d, _)| d);
    finish(cx, record, &datasets);
    datasets?;
    let result = json!({"vm": vm.to_string(), "vmid": production.vmid, "pit": chosen.pit_uuid});
    flow.result(result.clone());
    Ok(result)
}

// Promotion.

pub async fn promote(cx: &Context<'_>, vm: &GuestName) -> Result<Value, Stop> {
    let flow = cx.flow();
    let production = cx.guests.production(vm).map_err(failed)?;
    checks(cx, &production).await?;
    let me = cx.me().to_string();
    let mut restored: Vec<RestoreRecord> = cx
        .store()
        .restores()
        .map_err(|e| failed(e.message))?
        .into_iter()
        .filter(|r| {
            r.vm_uuid == production.vm_uuid
                && r.mode == Mode::NewVm
                && r.stands()
                && r.test.is_none()
                && r.host == me
                && r.vmid.is_some()
                && r.manifest.is_some()
        })
        .collect();
    restored.sort_by_key(|r| std::cmp::Reverse(r.finished_at_ms));
    if restored.is_empty() {
        return Err(failed(format!(
            "No VM restored from {vm} is on {me}. Restore one as a new VM here first."
        )));
    }
    let mut which = field("restored", FieldType::Select, "Restored VM");
    which.options = restored
        .iter()
        .map(|r| {
            choice(
                &r.restore_uuid.to_string(),
                &format!(
                    "VM {} ({})",
                    r.vmid.unwrap_or_default(),
                    r.name.as_deref().unwrap_or("unnamed")
                ),
                r.finished_at_ms
                    .map(|t| format!("restored {}", job::when(t.div_euclid(1000)))),
            )
        })
        .collect();
    which.default = Some(json!(restored[0].restore_uuid.to_string()));
    let values = flow
        .input_group(
            "Which restored VM becomes production",
            None,
            vec![which],
            |_| Ok(()),
        )
        .await?;
    let chosen = restored
        .into_iter()
        .find(|r| r.restore_uuid.to_string() == text(&values, "restored"))
        .ok_or_else(|| failed("that restored VM is no longer offered"))?;
    let new_vmid = chosen.vmid.expect("filtered above");
    let sealed = chosen.manifest.clone().expect("filtered above");
    let manifest = Manifest::open(&sealed).map_err(|e| failed(e.to_string()))?;
    if cx.host.owner(new_vmid).await.map_err(failed)? != Some(cx.me()) {
        return Err(failed(format!("VM {new_vmid} isn't on {me} any more")));
    }

    let current = current_disks(cx, production.vmid).await?;
    let theirs = current_disks(cx, new_vmid).await?;
    let mut problems = Vec::new();
    let mut swaps = Vec::new();
    for volume in &manifest.volumes {
        let (Some((_, now)), Some((_, restored))) =
            (current.get(&volume.slot), theirs.get(&volume.slot))
        else {
            problems.push(format!(
                "{} isn't a disk of both {vm} and VM {new_vmid}",
                volume.slot
            ));
            continue;
        };
        if now.split('/').next() != restored.split('/').next() {
            problems.push(format!(
                "{}: {restored} and {now} are in different pools",
                volume.slot
            ));
            continue;
        }
        swaps.push(Swap {
            slot: volume.slot.clone(),
            current: now.clone(),
            restored: restored.clone(),
            volume_uuid: volume.volume_uuid.clone(),
        });
    }
    if !problems.is_empty() {
        return Err(failed(format!(
            "VM {new_vmid} can't be promoted: {}",
            problems.join("; ")
        )));
    }
    let jobs = cx.host.jobs(production.vmid).await.map_err(failed)?;
    let mut details = vec![
        format!(
            "{vm} keeps its VMID, MAC addresses, routes, and HA resource; VM {new_vmid}'s own \
             configuration is removed."
        ),
        "The production disks are set aside under <pool>/bmac-replaced, with their snapshots, \
         until a cleanup."
            .into(),
    ];
    details.extend(replication_note(
        &jobs,
        manifest.volumes.iter().map(|v| v.volsize).sum(),
    ));
    let confirmed = flow
        .confirm(
            "Promote a restored VM",
            &format!(
                "{vm} stops, and VM {new_vmid}'s disks ({}) become its disks. Type the VM's \
                 name to go ahead.",
                vm_label(&manifest, manifest.taken_at_s)
            ),
            Severity::Critical,
            Some(&vm.to_string()),
            details,
        )
        .await?;
    if !confirmed {
        return Err(Stop::Cancelled);
    }

    let record = job::record_restore(
        &cx.restore,
        &new_record(
            cx,
            Mode::Promotion,
            &production,
            &sealed,
            chosen.appliance_uuid,
        )?,
    );
    if cx.host.running(new_vmid).await.map_err(failed)? {
        cx.host.shutdown(new_vmid).await.map_err(failed)?;
        let host = cx.host;
        wait_until(
            cx,
            cx.stop_wait,
            &format!("VM {new_vmid} didn't stop"),
            move || async move { host.running(new_vmid).await.map(|r| !r) },
        )
        .await?;
    }
    let vmid = production.vmid;
    let outcome = change(cx, &production, move |cx| {
        Box::pin(async move {
            let aside = swap(cx, vmid, &swaps).await?;
            if let Err(e) = cx.host.forget_vm(new_vmid).await {
                cx.flow().warning(format!(
                    "Could not remove VM {new_vmid}'s configuration: {e}"
                ));
            }
            Ok((aside, ()))
        })
    })
    .await;
    let mut record = record;
    let datasets = outcome.map(|(aside, ())| {
        record.replaced = aside.clone();
        current_datasets(&current, aside.keys())
    });
    finish(cx, record, &datasets);
    datasets?;
    let mut ended = chosen;
    ended.ended_at_ms = Some(cx.now_ms());
    job::record_restore(&cx.restore, &ended);
    let result = json!({"vm": vm.to_string(), "vmid": production.vmid, "from_vmid": new_vmid});
    flow.result(result.clone());
    Ok(result)
}

/// A disk set aside under `<pool>/bmac-replaced/<vmid>/`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SetAside {
    pub dataset: String,
    pub vmid: u32,
    pub at_s: Option<i64>,
}

/// The disks set aside on this host.
pub async fn set_aside(cx: &Context<'_>) -> Result<Vec<SetAside>, Stop> {
    let storages = cx.restore.host.storages().await.map_err(failed)?;
    let pools: BTreeSet<String> = storages.iter().map(|s| s.pool().to_string()).collect();
    let mut out = Vec::new();
    for pool in pools {
        let root = format!("{pool}/bmac-replaced");
        for vm in cx.restore.host.children(&root).await.map_err(failed)? {
            let Ok(vmid) = vm.parse::<u32>() else {
                continue;
            };
            for disk in cx
                .restore
                .host
                .children(&format!("{root}/{vm}"))
                .await
                .map_err(failed)?
            {
                out.push(SetAside {
                    at_s: disk.rsplit_once('-').and_then(|(_, t)| t.parse().ok()),
                    dataset: format!("{root}/{vm}/{disk}"),
                    vmid,
                });
            }
        }
    }
    Ok(out)
}

/// What still depends on a set-aside disk's snapshots: the restored disks
/// that are clones of them (promoted first), and the staging clones of a
/// restore in progress (which keep it).
async fn dependents(cx: &Context<'_>, dataset: &str) -> Result<(Vec<String>, Vec<String>), Stop> {
    let mut promote = Vec::new();
    let mut staging = Vec::new();
    for snapshot in cx.host.snapshots_of(dataset).await.map_err(failed)? {
        for clone in cx.host.clones(&snapshot).await.map_err(failed)? {
            if clone.contains("/bmac-restore/") {
                staging.push(clone);
            } else {
                promote.push(clone);
            }
        }
    }
    Ok((promote, staging))
}

pub async fn cleanup(cx: &Context<'_>) -> Result<Value, Stop> {
    let me = cx.me();
    let found = set_aside(cx).await?;
    if found.is_empty() {
        cx.flow().info(format!("Nothing is set aside on {me}."));
        return Ok(json!({"removed": []}));
    }
    let mut offered = Vec::new();
    for disk in &found {
        let (_, staging) = dependents(cx, &disk.dataset).await?;
        if staging.is_empty() {
            offered.push(disk.clone());
        } else {
            cx.flow().warning(format!(
                "{} stays: a restore in progress depends on it ({}).",
                disk.dataset,
                staging.join(", ")
            ));
        }
    }
    if offered.is_empty() {
        return Err(failed(
            "Every set-aside disk has a restore in progress depending on it; let it finish \
             first.",
        ));
    }
    let mut disks = field("disks", FieldType::Multiselect, "Disks");
    disks.options = offered
        .iter()
        .map(|d| {
            choice(
                &d.dataset,
                &d.dataset,
                Some(match d.at_s {
                    Some(at) => format!("VM {}'s, set aside {}", d.vmid, job::when(at)),
                    None => format!("VM {}'s", d.vmid),
                }),
            )
        })
        .collect();
    disks.min_selected = Some(1);
    let values = cx
        .flow()
        .input_group("Which disks to remove", None, vec![disks], |_| Ok(()))
        .await?;
    let chosen: Vec<String> = values
        .get("disks")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    let chosen: Vec<&SetAside> = offered
        .iter()
        .filter(|d| chosen.contains(&d.dataset))
        .collect();
    if chosen.is_empty() {
        return Err(failed("no set-aside disk was chosen"));
    }
    let confirmed = cx
        .flow()
        .confirm(
            "Remove set-aside disks",
            &format!(
                "{} disk{} on {me} are destroyed with every snapshot and bookmark they have.",
                chosen.len(),
                if chosen.len() == 1 { "" } else { "s" }
            ),
            Severity::Critical,
            None,
            chosen.iter().map(|d| d.dataset.clone()).collect(),
        )
        .await?;
    if !confirmed {
        return Err(Stop::Cancelled);
    }
    cx.flow()
        .phase(CLEANUP_PHASE.0, CLEANUP_PHASE.1, PhaseStatus::Running);
    let mut removed = Vec::new();
    let mut kept = Vec::new();
    for disk in chosen {
        match remove(cx, &disk.dataset).await {
            Ok(()) => removed.push(disk.dataset.clone()),
            Err(e) => {
                cx.flow().warning(format!("{} stays: {e}", disk.dataset));
                kept.push(disk.dataset.clone());
            }
        }
        let parent = disk.dataset.rsplit_once('/').map(|(p, _)| p.to_string());
        if let Some(parent) = parent
            && cx
                .restore
                .host
                .children(&parent)
                .await
                .is_ok_and(|c| c.is_empty())
        {
            let _ = cx.restore.host.destroy_tree(&parent).await;
        }
    }
    let status = if kept.is_empty() {
        PhaseStatus::Complete
    } else {
        PhaseStatus::Failed
    };
    cx.flow().phase(CLEANUP_PHASE.0, CLEANUP_PHASE.1, status);
    if !kept.is_empty() {
        return Err(failed(format!("Kept {}", kept.join(", "))));
    }
    Ok(json!({"removed": removed}))
}

/// Promotes what still descends from the disk, then destroys it.
async fn remove(cx: &Context<'_>, dataset: &str) -> Result<(), String> {
    // Each promotion takes the snapshots up to the clone's origin; a clone
    // of a later snapshot needs another round.
    for _ in 0..8 {
        let (promote, staging) = dependents(cx, dataset).await.map_err(|s| match s {
            Stop::Failed { message, .. } => message,
            Stop::Cancelled => "cancelled".into(),
        })?;
        if !staging.is_empty() {
            return Err(format!(
                "a restore in progress depends on it ({})",
                staging.join(", ")
            ));
        }
        let Some(clone) = promote.first() else {
            return cx.restore.host.destroy_tree(dataset).await;
        };
        cx.host.promote(clone).await?;
        cx.flow()
            .info(format!("{clone} no longer depends on {dataset}."));
    }
    Err("clones of it remain after promoting them".into())
}

// The real host.

pub struct RealHost<'a> {
    pub runner: &'a dyn CommandRunner,
    pub node: HostSlot,
}

impl RealHost<'_> {
    async fn run(&self, program: &str, args: &[&str], timeout: Duration) -> Result<String, String> {
        let command = Command::new(program)
            .args(args.iter().map(|a| a.to_string()))
            .timeout(timeout);
        self.runner
            .run(command)
            .await
            .map(|o| o.stdout_text())
            .map_err(|e| {
                format!(
                    "{program} {}: {e}",
                    args.first().copied().unwrap_or_default()
                )
            })
    }

    async fn quick(&self, program: &str, args: &[&str]) -> Result<String, String> {
        self.run(program, args, Duration::from_secs(120)).await
    }

    async fn json(&self, path: &str) -> Result<Value, String> {
        let text = self
            .quick(PVESH, &["get", path, "--output-format", "json"])
            .await?;
        serde_json::from_str(&text).map_err(|e| format!("{path}: {e}"))
    }

    async fn value(&self, property: &str, name: &str) -> Result<Option<String>, String> {
        let command = Command::new(ZFS)
            .args(["get", "-Hp", "-o", "value", property, name].map(str::to_string))
            .expect_codes(&[0, 1])
            .timeout(Duration::from_secs(60));
        let output = self
            .runner
            .run(command)
            .await
            .map_err(|e| format!("zfs get {property}: {e}"))?;
        Ok((output.code() == Some(0))
            .then(|| output.stdout_text().trim().to_string())
            .filter(|v| v != "-" && !v.is_empty()))
    }
}

impl ReplaceHost for RealHost<'_> {
    fn quorate(&self) -> BoxFuture<'_, Result<bool, String>> {
        Box::pin(async move {
            let text = self.quick(PVECM, &["status"]).await?;
            Ok(text.lines().any(|l| {
                let mut words = l.split_whitespace();
                words.next() == Some("Quorate:") && words.next() == Some("Yes")
            }))
        })
    }

    fn owner(&self, vmid: u32) -> BoxFuture<'_, Result<Option<HostSlot>, String>> {
        Box::pin(async move {
            let rows = self.json("/cluster/resources").await?;
            let row = rows
                .as_array()
                .into_iter()
                .flatten()
                .find(|r| r["type"] == "qemu" && r["vmid"].as_u64() == Some(u64::from(vmid)));
            row.map(|r| {
                r["node"]
                    .as_str()
                    .and_then(|n| n.parse().ok())
                    .ok_or_else(|| format!("VM {vmid} is on a node with no BMAC host name"))
            })
            .transpose()
        })
    }

    fn config_text(&self, vmid: u32) -> BoxFuture<'_, Result<String, String>> {
        Box::pin(async move {
            let path = format!("/etc/pve/qemu-server/{vmid}.conf");
            std::fs::read_to_string(&path).map_err(|e| format!("{path}: {e}"))
        })
    }

    fn dataset(&self, volume_id: &str) -> BoxFuture<'_, Result<String, String>> {
        let volume_id = volume_id.to_string();
        Box::pin(async move {
            let (storage, volume) = volume_id
                .split_once(':')
                .ok_or_else(|| format!("{volume_id} isn't a storage volume"))?;
            let cfg = std::fs::read_to_string("/etc/pve/storage.cfg")
                .map_err(|e| format!("/etc/pve/storage.cfg: {e}"))?;
            let storages = job::parse_storages(&cfg, &self.node.to_string());
            let found = storages
                .iter()
                .find(|s| s.id == storage)
                .ok_or_else(|| format!("{storage} isn't a ZFS storage of {}", self.node))?;
            Ok(format!("{}/{volume}", found.dataset))
        })
    }

    fn ha_managed(&self, vmid: u32) -> BoxFuture<'_, Result<bool, String>> {
        Box::pin(async move {
            let rows = self.json("/cluster/ha/resources").await?;
            let sid = format!("vm:{vmid}");
            Ok(rows
                .as_array()
                .into_iter()
                .flatten()
                .any(|r| r["sid"].as_str() == Some(sid.as_str())))
        })
    }

    fn set_ha(&self, vmid: u32, started: bool) -> BoxFuture<'_, Result<(), String>> {
        Box::pin(async move {
            let sid = format!("vm:{vmid}");
            let state = if started { "started" } else { "stopped" };
            self.quick(HA_MANAGER, &["set", &sid, "--state", state])
                .await
                .map(drop)
        })
    }

    fn running(&self, vmid: u32) -> BoxFuture<'_, Result<bool, String>> {
        Box::pin(async move {
            let text = self.quick(QM, &["status", &vmid.to_string()]).await?;
            Ok(text.split_whitespace().nth(1) == Some("running"))
        })
    }

    fn shutdown(&self, vmid: u32) -> BoxFuture<'_, Result<(), String>> {
        Box::pin(async move {
            self.run(
                QM,
                &[
                    "shutdown",
                    &vmid.to_string(),
                    "--timeout",
                    "180",
                    "--forceStop",
                    "1",
                ],
                Duration::from_secs(300),
            )
            .await
            .map(drop)
        })
    }

    fn start(&self, vmid: u32) -> BoxFuture<'_, Result<(), String>> {
        Box::pin(async move {
            self.quick(QM, &["start", &vmid.to_string()])
                .await
                .map(drop)
        })
    }

    fn jobs(&self, vmid: u32) -> BoxFuture<'_, Result<Vec<Job>, String>> {
        Box::pin(async move {
            let rows = self.json("/cluster/replication").await?;
            let mut out = Vec::new();
            for r in rows.as_array().into_iter().flatten() {
                let guest = r["guest"]
                    .as_u64()
                    .or_else(|| r["guest"].as_str().and_then(|g| g.parse().ok()));
                if guest != Some(u64::from(vmid)) {
                    continue;
                }
                let text = |k: &str| r[k].as_str().map(str::to_string);
                out.push(Job {
                    id: text("id").ok_or("a replication job has no ID")?,
                    target: text("target")
                        .and_then(|t| t.parse().ok())
                        .ok_or("a replication job's target isn't a BMAC host")?,
                    schedule: text("schedule").unwrap_or_else(|| "*/15".into()),
                    rate: r["rate"]
                        .as_f64()
                        .map(|v| v.to_string())
                        .or_else(|| text("rate")),
                    comment: text("comment"),
                });
            }
            Ok(out)
        })
    }

    fn delete_job(&self, id: &str) -> BoxFuture<'_, Result<(), String>> {
        let id = id.to_string();
        Box::pin(async move { self.quick(PVESR, &["delete", &id]).await.map(drop) })
    }

    fn create_job(&self, job: &Job) -> BoxFuture<'_, Result<(), String>> {
        let job = job.clone();
        Box::pin(async move {
            let target = job.target.to_string();
            let mut args = vec![
                "create-local-job",
                &job.id,
                &target,
                "--schedule",
                &job.schedule,
            ];
            if let Some(rate) = &job.rate {
                args.extend(["--rate", rate]);
            }
            if let Some(comment) = &job.comment {
                args.extend(["--comment", comment]);
            }
            self.quick(PVESR, &args).await.map(drop)
        })
    }

    fn job_state(&self, id: &str) -> BoxFuture<'_, Result<JobState, String>> {
        let id = id.to_string();
        Box::pin(async move {
            let row = self
                .json(&format!("/nodes/{}/replication/{id}/status", self.node))
                .await?;
            Ok(JobState {
                busy: row.get("pid").is_some_and(|p| !p.is_null()),
                failing: row["fail_count"].as_u64().unwrap_or(0) != 0
                    || row.get("error").is_some_and(|e| !e.is_null()),
                last_sync_s: row["last_sync"].as_i64().unwrap_or(0),
                fail_count: row["fail_count"].as_u64().unwrap_or(0),
                error: row["error"].as_str().map(str::to_string),
            })
        })
    }

    fn schedule_now(&self, id: &str) -> BoxFuture<'_, Result<(), String>> {
        let id = id.to_string();
        Box::pin(async move { self.quick(PVESR, &["schedule-now", &id]).await.map(drop) })
    }

    fn ensure_parent(&self, dataset: &str) -> BoxFuture<'_, Result<(), String>> {
        let dataset = dataset.to_string();
        Box::pin(async move {
            let above = parent(&dataset).to_string();
            if self.value("name", &above).await?.is_some() {
                return Ok(());
            }
            let top = parent(&above).to_string();
            if self.value("name", &top).await?.is_none() {
                self.quick(
                    ZFS,
                    &[
                        "create",
                        "-p",
                        "-o",
                        "mountpoint=none",
                        "-o",
                        "canmount=off",
                        &top,
                    ],
                )
                .await?;
            }
            self.quick(ZFS, &["create", "-o", "canmount=off", &above])
                .await
                .map(drop)
        })
    }

    fn rename(&self, from: &str, to: &str) -> BoxFuture<'_, Result<(), String>> {
        let (from, to) = (from.to_string(), to.to_string());
        Box::pin(async move { self.quick(ZFS, &["rename", &from, &to]).await.map(drop) })
    }

    fn inherit_volmode(&self, dataset: &str) -> BoxFuture<'_, Result<(), String>> {
        let dataset = dataset.to_string();
        Box::pin(async move {
            self.quick(ZFS, &["inherit", "volmode", &dataset])
                .await
                .map(drop)
        })
    }

    fn promote(&self, dataset: &str) -> BoxFuture<'_, Result<(), String>> {
        let dataset = dataset.to_string();
        Box::pin(async move { self.quick(ZFS, &["promote", &dataset]).await.map(drop) })
    }

    fn origin(&self, dataset: &str) -> BoxFuture<'_, Result<Option<String>, String>> {
        let dataset = dataset.to_string();
        Box::pin(async move { self.value("origin", &dataset).await })
    }

    fn snapshots_of(&self, dataset: &str) -> BoxFuture<'_, Result<Vec<String>, String>> {
        let dataset = dataset.to_string();
        Box::pin(async move {
            let text = self
                .quick(
                    ZFS,
                    &[
                        "list",
                        "-H",
                        "-t",
                        "snapshot",
                        "-o",
                        "name",
                        "-s",
                        "createtxg",
                        "-d",
                        "1",
                        &dataset,
                    ],
                )
                .await?;
            Ok(text.lines().map(str::to_string).collect())
        })
    }

    fn clones(&self, snapshot: &str) -> BoxFuture<'_, Result<Vec<String>, String>> {
        let snapshot = snapshot.to_string();
        Box::pin(async move {
            Ok(self
                .value("clones", &snapshot)
                .await?
                .map(|v| v.split(',').map(str::to_string).collect())
                .unwrap_or_default())
        })
    }

    fn rollback(&self, snapshot: &str) -> BoxFuture<'_, Result<(), String>> {
        let snapshot = snapshot.to_string();
        Box::pin(async move {
            self.run(
                ZFS,
                &["rollback", "-r", &snapshot],
                Duration::from_secs(600),
            )
            .await
            .map(drop)
        })
    }

    fn snapshot(&self, name: &str) -> BoxFuture<'_, Result<(), String>> {
        let name = name.to_string();
        Box::pin(async move { self.quick(ZFS, &["snapshot", &name]).await.map(drop) })
    }

    fn copy_clone(
        &self,
        origin: &str,
        snapshot: &str,
        target: &str,
    ) -> BoxFuture<'_, Result<(), String>> {
        let [origin, snapshot, target] = [origin, snapshot, target].map(str::to_string);
        Box::pin(async move {
            let args = ["-c", COPY_CLONE, "bmac", &origin, &snapshot, &target];
            self.run(BASH, &args, COPY_TIMEOUT).await.map(drop)
        })
    }

    fn guid(&self, snapshot: &str) -> BoxFuture<'_, Result<Option<String>, String>> {
        let snapshot = snapshot.to_string();
        Box::pin(async move {
            Ok(self
                .value("guid", &snapshot)
                .await?
                .filter(|g| !g.is_empty() && g != "-"))
        })
    }

    fn destroy(&self, name: &str) -> BoxFuture<'_, Result<(), String>> {
        let name = name.to_string();
        Box::pin(async move { self.quick(ZFS, &["destroy", "-r", &name]).await.map(drop) })
    }

    fn rescan(&self, vmid: u32) -> BoxFuture<'_, Result<(), String>> {
        Box::pin(async move {
            self.quick(QM, &["disk", "rescan", "--vmid", &vmid.to_string()])
                .await
                .map(drop)
        })
    }

    fn forget_vm(&self, vmid: u32) -> BoxFuture<'_, Result<(), String>> {
        Box::pin(async move {
            // Its disks were renamed away; Proxmox warns about each and
            // goes on.
            self.quick(
                QM,
                &[
                    "destroy",
                    &vmid.to_string(),
                    "--purge",
                    "1",
                    "--destroy-unreferenced-disks",
                    "0",
                ],
            )
            .await
            .map(drop)
        })
    }
}

/// `bmacd native backup.restore_into|backup.rollback|backup.promote VM`.
pub async fn run(flow: &Workflow, operation: &str, args: &[String]) -> ExitCode {
    let spec = operations()
        .into_iter()
        .find(|o| o.id.as_str() == operation)
        .expect("one of this module's operations");
    flow.begin(operation, &spec.title, &spec.description);
    let outcome = async {
        let [arg] = args else {
            return Err(failed(format!(
                "usage: {operation} {}",
                if operation == CLEANUP { "HOST" } else { "VM" }
            )));
        };
        let node: HostSlot = crate::engine::listen::node_name()
            .parse()
            .map_err(|_| failed("this host's name is not a BMAC host name"))?;
        if operation == CLEANUP && arg != &node.to_string() {
            return Err(failed(format!("this job runs on {arg}, not {node}")));
        }
        let runner = bmac_exec::RealRunner;
        let guests = RegistryGuests {
            registry: bmac_registry::Registry::system(),
            runner: &runner,
            owner: format!("{operation}-{node}-{}", std::process::id()),
        };
        let cluster_uuid = crate::joins::JoinStore::system()
            .cluster()
            .map_err(|e| failed(e.message))?
            .map(|c| c.cluster_uuid)
            .ok_or_else(|| failed("the cluster has no identity record yet"))?;
        let now = || jiff::Timestamp::now().as_millisecond();
        let store = BackupStore::system();
        let restore_host = job::RealHost {
            runner: &runner,
            node,
        };
        let cx = Context {
            restore: job::Context {
                flow,
                host: &restore_host,
                transfers: &job::RealTransfers,
                store: &store,
                cluster_uuid,
                now_ms: &now,
                agent_wait: Duration::from_secs(300),
                poll: Duration::from_secs(10),
            },
            host: &RealHost {
                runner: &runner,
                node,
            },
            guests: &guests,
            stop_wait: Duration::from_secs(300),
            replication_timeout: Duration::from_secs(12 * 3600),
            ask_every: Duration::from_secs(60),
        };
        if operation == CLEANUP {
            return cleanup(&cx).await.map(drop);
        }
        let vm: GuestName = arg
            .parse()
            .map_err(|_| failed(format!("{arg} is not a guest name")))?;
        match operation {
            RESTORE_INTO => restore_into(&cx, &vm).await,
            ROLLBACK => rollback(&cx, &vm).await,
            _ => promote(&cx, &vm).await,
        }
        .map(drop)
    }
    .await;
    flow.finish(&outcome)
}

#[cfg(test)]
#[path = "replace_tests.rs"]
mod tests;
