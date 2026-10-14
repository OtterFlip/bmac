// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

//! `storage.remove_vdev` (v2 `decommission_disks.sh`).
//!
//! A data or special vdev can be removed only from a pool without raidz.
//! Before `zpool remove` copies its data to the other vdevs, v2's
//! preparation runs: related staging VMs must already be gone, every
//! production guest on this host is trimmed through its guest agent so
//! freed blocks don't travel, replication is forced so the standbys hold
//! the trimmed state, and a clean scrub proves the data being copied is
//! sound. Trims and a clean scrub from the last day are reused. The
//! minimum free space left afterwards is at least 50 GiB. Log, cache, and
//! spare vdevs need none of that and go at once. Storage inventory
//! finishes every removal.

use std::collections::BTreeMap;
use std::time::Duration;

use bmac_adapters::proxmox::Pvesh;
use bmac_adapters::qga::GuestAgent;
use bmac_core::job::{FieldType, Severity};
use bmac_core::{Bytes, GuestName, HostSlot, Vmid};
use bmac_exec::Command;
use bmac_storage::VdevClass;
use bmac_storage::journal::{RemovalState, RemovedMember};
use serde::Deserialize;
use serde_json::{Map, Value, json};

use super::{
    Context, POOL_PHASE, Snapshot, Top, choice, field, guests_on, mapper_of, online, refuse,
    staging_on, text,
};
use crate::native::Stop;

const CHOOSE: (&str, &str) = ("choose", "Choose the vdev");
const GUESTS: (&str, &str) = ("guests", "Trim and replicate the guests");
const SCRUB: (&str, &str) = ("scrub", "Scrub the pool");
const FINISH: (&str, &str) = ("finish", "Finish");
/// v2 `MIN_FREE_FLOOR_GIB`.
pub const MIN_FREE_FLOOR_GIB: u64 = 50;
const GIB: u64 = Bytes::GIB;
const TRIM_TIMEOUT: Duration = Duration::from_secs(30 * 60);
const REPLICATION_TIMEOUT: Duration = Duration::from_secs(60 * 60);
const CHOSEN: &str = "chosen";

/// A guest as the registry records it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GuestFacts {
    pub name: GuestName,
    pub vmid: Vmid,
    pub owner: Option<HostSlot>,
    pub placement: Vec<HostSlot>,
}

/// What a removal needs to know about the rest of the cluster.
pub trait Cluster: Send + Sync {
    fn guests(&self) -> Result<Vec<GuestFacts>, String>;
    /// Deferred cleanups still to run on `node`.
    fn pending_cleanups(&self, node: HostSlot) -> Result<Vec<String>, String>;
}

/// Whether ZFS can remove this vdev and BMAC lets it; why not otherwise.
pub fn removable(snapshot: &Snapshot, top: &Top<'_>) -> Result<(), String> {
    if top.removing {
        return Err("it is being removed already".into());
    }
    if matches!(
        top.class,
        VdevClass::Log | VdevClass::Cache | VdevClass::Spare
    ) {
        return Ok(());
    }
    if top.holds_esp {
        return Err("it is the boot vdev".into());
    }
    let tops = snapshot.tops();
    if tops
        .iter()
        .any(|t| t.kind.is_some_and(bmac_storage::VdevType::is_raidz))
    {
        return Err(
            "the pool has raidz, so ZFS can't remove a data or special vdev from it".into(),
        );
    }
    if top.class == VdevClass::Data
        && tops.iter().filter(|t| t.class == VdevClass::Data).count() < 2
    {
        return Err("it is the pool's only data vdev".into());
    }
    if !top
        .members
        .iter()
        .any(|m| online(m) && m.identity.is_some())
    {
        return Err("no member is online, so there is no copy of its data to move".into());
    }
    if let Some(m) = top
        .members
        .iter()
        .find(|m| m.disk.is_some() && m.identity.is_none())
    {
        return Err(format!(
            "{} has no WWN or serial, so its disk couldn't be tracked afterwards",
            m.path
        ));
    }
    Ok(())
}

/// Whether the rest of the pool can take the vdev's data and still keep
/// `min_free` free.
pub fn fits(snapshot: &Snapshot, top: &Top<'_>, min_free: u64) -> Result<(), String> {
    if !matches!(top.class, VdevClass::Data | VdevClass::Special) {
        return Ok(());
    }
    let available = snapshot.layout.pool.dataset_available.unwrap_or(0);
    let allocated = top.allocated.unwrap_or(0);
    let room = available.saturating_sub(min_free);
    if allocated > room {
        return Err(format!(
            "it holds {}, and the rest of the pool has room for {} while keeping {} free",
            Bytes(allocated),
            Bytes(room),
            Bytes(min_free)
        ));
    }
    Ok(())
}

fn check(
    snapshot: &Snapshot,
    values: &Map<String, Value>,
) -> Result<(String, u64), BTreeMap<String, String>> {
    let name = text(values, "vdev");
    let top = snapshot
        .top(&name)
        .ok_or_else(|| refuse("vdev", "Choose a vdev."))?;
    let gib = values
        .get("min_free_gib")
        .and_then(|v| {
            v.as_u64()
                .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
        })
        .ok_or_else(|| refuse("min_free_gib", "Enter a whole number of GiB."))?;
    if gib < MIN_FREE_FLOOR_GIB {
        return Err(refuse(
            "min_free_gib",
            format!("Keep at least {MIN_FREE_FLOOR_GIB} GiB free."),
        ));
    }
    removable(snapshot, &top).map_err(|why| refuse("vdev", format!("Can't remove it: {why}.")))?;
    fits(snapshot, &top, gib * GIB)
        .map_err(|why| refuse("vdev", format!("It doesn't fit: {why}.")))?;
    Ok((name, gib))
}

fn needs_preparation(class: VdevClass) -> bool {
    matches!(class, VdevClass::Data | VdevClass::Special)
}

pub async fn remove_vdev(cx: &Context<'_>) -> Result<(), Stop> {
    cx.phase(CHOOSE);
    let snapshot = cx.snapshot().await?;
    if snapshot.layout.pool.removal_in_progress {
        return Err(Stop::failed(
            "A vdev removal is already running; ZFS removes one at a time.",
        ));
    }
    if let Some(r) = snapshot.journal.removal_in_progress() {
        return Err(Stop::failed(format!(
            "The removal of {} hasn't been finished; run Storage inventory first.",
            r.vdev
        )));
    }
    let (name, min_free_gib) = match cx.flow.checkpoints().get(CHOSEN) {
        Some(saved) => (
            saved["vdev"].as_str().unwrap_or_default().to_string(),
            saved["min_free_gib"].as_u64().unwrap_or(MIN_FREE_FLOOR_GIB),
        ),
        None => choose(cx, &snapshot).await?,
    };
    cx.done(CHOOSE);
    let class = snapshot
        .top(&name)
        .map(|t| t.class)
        .ok_or_else(|| Stop::failed(format!("{name} is no longer in {}", cx.pool)))?;

    if needs_preparation(class) {
        staging_gate(cx)?;
        cx.phase(GUESTS);
        trim_and_replicate(cx).await?;
        cx.done(GUESTS);
        cx.phase(SCRUB);
        scrub(cx).await?;
        cx.done(SCRUB);
    }

    cx.phase(POOL_PHASE);
    let snapshot = cx.snapshot().await?;
    let top = snapshot
        .top(&name)
        .ok_or_else(|| Stop::failed(format!("{name} is no longer in {}", cx.pool)))?;
    removable(&snapshot, &top)
        .map_err(|why| Stop::failed(format!("{name} can't be removed now: {why}.")))?;
    fits(&snapshot, &top, min_free_gib * GIB)
        .map_err(|why| Stop::failed(format!("{name} no longer fits: {why}.")))?;
    let members: Vec<RemovedMember> = top
        .members
        .iter()
        .map(|m| RemovedMember {
            identity: m.identity.clone(),
            mapper: mapper_of(m),
            size: m.disk_size,
        })
        .collect();
    let id = cx.update(|s| s.record_removal(&name, members, cx.now()))?;
    cx.flow.info(format!("Starting the removal of {name}."));
    if let Err(error) = cx.zpool(&["remove", cx.pool.as_str(), &name]).await {
        let after = cx.snapshot().await?;
        let started = after.top(&name).is_none_or(|t| t.removing);
        if started {
            cx.flow.warning(format!(
                "zpool remove reported an error, but {name} is being removed; the record stays."
            ));
        } else {
            let note = "zpool remove failed and the vdev is still in the pool";
            cx.update(|s| s.set_removal(&id, RemovalState::Failed, Some(note), cx.now()))?;
            return Err(error);
        }
    }
    cx.done(POOL_PHASE);

    cx.phase(FINISH);
    if needs_preparation(class) {
        cx.flow.info(format!(
            "ZFS is copying {name}'s data to the other vdevs; the Storage page shows its progress."
        ));
        cx.flow.next_step(
            "When the removal has finished, run Storage inventory to close its LUKS mappings and \
             release its disks.",
        );
    } else {
        cx.flow.info(format!("{name} is out of the pool."));
        cx.flow
            .next_step("Run Storage inventory to close its LUKS mappings and release its disks.");
    }
    cx.flow.result(json!({ "vdev": name, "removal": id }));
    cx.done(FINISH);
    Ok(())
}

async fn choose(cx: &Context<'_>, snapshot: &Snapshot) -> Result<(String, u64), Stop> {
    let tops = snapshot.tops();
    let mut vdev = field("vdev", FieldType::Select, "Vdev");
    vdev.options = tops
        .iter()
        .filter(|t| removable(snapshot, t).is_ok())
        .map(|t| {
            let held = t
                .allocated
                .map(|a| format!("holds {}", Bytes(a)))
                .unwrap_or_else(|| "removed at once".into());
            choice(t.name, &t.describe(), Some(held))
        })
        .collect();
    let refused: Vec<String> = tops
        .iter()
        .filter_map(|t| {
            removable(snapshot, t)
                .err()
                .map(|why| format!("{}: {why}", t.name))
        })
        .collect();
    if vdev.options.is_empty() {
        return Err(Stop::failed(format!(
            "No vdev can be removed. {}",
            refused.join("; ")
        )));
    }
    let mut floor = field("min_free_gib", FieldType::Integer, "Free space to keep");
    floor.suffix = Some("GiB".into());
    floor.min = Some(MIN_FREE_FLOOR_GIB as f64);
    floor.default = Some(json!(MIN_FREE_FLOOR_GIB));
    floor.help = Some(format!(
        "The removal is refused unless at least this much stays free afterwards; at least \
         {MIN_FREE_FLOOR_GIB} GiB. Available now: {}.",
        Bytes(snapshot.layout.pool.dataset_available.unwrap_or(0))
    ));
    let values = cx
        .flow
        .input_group(
            "Remove a vdev",
            Some(&if refused.is_empty() {
                "Choose the vdev to remove.".to_string()
            } else {
                format!("Not offered: {}.", refused.join("; "))
            }),
            vec![vdev, floor],
            |values| check(snapshot, values).map(|_| ()),
        )
        .await?;
    let (name, gib) = check(snapshot, &values)
        .map_err(|e| Stop::failed(e.into_values().collect::<Vec<_>>().join(" ")))?;
    let top = snapshot.top(&name).expect("checked");
    let mut details = Vec::new();
    if needs_preparation(top.class) {
        details.push(
            "Delete what you don't need inside the guests first: the trims hand freed space back \
             to the pool, and the removal needs room."
                .into(),
        );
        details.push(
            "Plan: check no staging VM or deferred cleanup uses this host; trim every \
             production guest on it (trims from the last day are reused); force their \
             replication; scrub (a clean scrub from the last day is reused); then zpool remove."
                .into(),
        );
    }
    let confirmed = cx
        .flow
        .confirm(
            &format!("Remove {name}"),
            &format!(
                "{name} leaves the pool. Its disks are released by Storage inventory once the \
                 removal finishes."
            ),
            Severity::Destructive,
            Some(&name),
            details,
        )
        .await?;
    if !confirmed {
        return Err(Stop::Cancelled);
    }
    cx.flow
        .checkpoints()
        .record(CHOSEN, json!({ "vdev": name, "min_free_gib": gib }))?;
    Ok((name, gib))
}

/// Staging VMs and deferred cleanups would leave clones and snapshots
/// pinned to this host's pool; they go first (v2 `decommission_disks.sh`).
fn staging_gate(cx: &Context<'_>) -> Result<(), Stop> {
    let guests = cx.cluster.guests().map_err(|e| Stop::Failed {
        message: "cannot read the cluster registry".into(),
        details: Some(e),
    })?;
    let staging: Vec<String> = staging_on(&guests, cx.node)
        .iter()
        .map(|g| g.name.to_string())
        .collect();
    if !staging.is_empty() {
        return Err(Stop::failed(format!(
            "Remove the staging VMs that use {} first: {}.",
            cx.node,
            staging.join(", ")
        )));
    }
    let cleanups = cx
        .cluster
        .pending_cleanups(cx.node)
        .map_err(|e| Stop::Failed {
            message: "cannot read the deferred cleanups".into(),
            details: Some(e),
        })?;
    if !cleanups.is_empty() {
        return Err(Stop::failed(format!(
            "Deferred cleanups still have to run on {}: {}. Let them finish first.",
            cx.node,
            cleanups.join(", ")
        )));
    }
    Ok(())
}

#[derive(Default, Deserialize)]
struct NodeTime {
    time: i64,
}

async fn trim_and_replicate(cx: &Context<'_>) -> Result<(), Stop> {
    let guests = cx.cluster.guests().map_err(|e| Stop::Failed {
        message: "cannot read the cluster registry".into(),
        details: Some(e),
    })?;
    let guests: Vec<GuestFacts> = guests_on(&guests, cx.node).into_iter().cloned().collect();
    if guests.is_empty() {
        cx.flow
            .info(format!("No production guest is placed on {}.", cx.node));
        return Ok(());
    }
    let agent = GuestAgent::new(cx.runner)
        .with_timing(cx.poll.max(Duration::from_millis(250)), TRIM_TIMEOUT);
    for guest in &guests {
        let journal = cx.journal.read().map_err(|e| Stop::failed(e.to_string()))?;
        if journal.recently_trimmed(guest.name, cx.now()) {
            cx.flow
                .info(format!("{} was trimmed within the last day.", guest.name));
            continue;
        }
        let node = guest
            .owner
            .or_else(|| guest.placement.first().copied())
            .ok_or_else(|| Stop::failed(format!("{} has no owner", guest.name)))?;
        cx.flow.info(format!("Trimming {} on {node}.", guest.name));
        let status = agent
            .exec(node, guest.vmid, &["fstrim", "-av"])
            .await
            .map_err(|e| Stop::Failed {
                message: format!(
                    "{}'s guest agent didn't trim it; the removal needs every production guest \
                     trimmed",
                    guest.name
                ),
                details: Some(e.to_string()),
            })?;
        if !status.succeeded() {
            return Err(Stop::Failed {
                message: format!("fstrim failed in {}", guest.name),
                details: status.err_data.or(status.out_data),
            });
        }
        cx.update(|s| {
            s.record_trims(&[guest.name], cx.now());
            Ok(())
        })?;
    }
    let pvesh = Pvesh::new(cx.runner);
    for guest in &guests {
        let Some(source) = guest.owner.or_else(|| guest.placement.first().copied()) else {
            continue;
        };
        let source = source.to_string();
        let jobs = pvesh.replication(&source).await.map_err(|e| Stop::Failed {
            message: format!("cannot read {source}'s replication jobs"),
            details: Some(e.to_string()),
        })?;
        for job in jobs.iter().filter(|j| j.guest == guest.vmid) {
            replicate(cx, &pvesh, &source, &job.id).await?;
        }
    }
    Ok(())
}

/// Runs a replication job now and waits for a run that started after the
/// trims to succeed, retrying the start while the job is locked by a run
/// already under way (v2 318).
async fn replicate(
    cx: &Context<'_>,
    pvesh: &Pvesh<'_>,
    source: &str,
    job: &str,
) -> Result<(), Stop> {
    let NodeTime { time: since } = pvesh
        .get(&format!("/nodes/{source}/time"), &[])
        .await
        .map_err(|e| Stop::Failed {
            message: format!("cannot read {source}'s clock"),
            details: Some(e.to_string()),
        })?;
    cx.flow.info(format!("Replicating {job} from {source}."));
    let started = tokio::time::Instant::now();
    loop {
        let run = cx
            .run(Command::new(bmac_adapters::programs::PVESH).args([
                "create",
                &format!("/nodes/{source}/replication/{job}/schedule_now"),
            ]))
            .await;
        if run.is_ok() {
            break;
        }
        if started.elapsed() > REPLICATION_TIMEOUT {
            return Err(Stop::failed(format!(
                "replication job {job} couldn't be started"
            )));
        }
        cx.flow.check()?;
        tokio::time::sleep(cx.poll).await;
    }
    loop {
        let jobs = pvesh.replication(source).await.map_err(|e| Stop::Failed {
            message: format!("cannot read {source}'s replication jobs"),
            details: Some(e.to_string()),
        })?;
        if let Some(j) = jobs.iter().find(|j| j.id == job) {
            let synced = j.last_sync.is_some_and(|t| t as i64 >= since);
            if synced && j.fail_count.unwrap_or(0) == 0 && j.error.is_none() {
                return Ok(());
            }
        }
        if started.elapsed() > REPLICATION_TIMEOUT {
            return Err(Stop::failed(format!(
                "replication job {job} hasn't run successfully since the trims"
            )));
        }
        cx.flow.check()?;
        tokio::time::sleep(cx.poll).await;
    }
}

pub fn clean(scan: &str) -> bool {
    scan.contains("with 0 errors")
}

async fn scrub(cx: &Context<'_>) -> Result<(), Stop> {
    let snapshot = cx.snapshot().await?;
    if let Some(s) = snapshot.journal.recent_clean_scrub(cx.now()) {
        cx.flow.info(format!(
            "Reusing the clean scrub from the last day: {}",
            s.result
        ));
        return Ok(());
    }
    let pool = cx.pool.as_str();
    if snapshot
        .layout
        .pool
        .scan
        .as_deref()
        .is_some_and(|s| s.contains("scrub in progress"))
    {
        cx.flow.info("Waiting for the scrub already running.");
        cx.zpool(&["wait", "-t", "scrub", pool]).await?;
    } else {
        cx.flow
            .info(format!("Scrubbing {pool}; this reads every block."));
        cx.zpool(&["scrub", "-w", pool]).await?;
    }
    let after = cx.snapshot().await?;
    let result = after.layout.pool.scan.clone().unwrap_or_default();
    let first = result.lines().next().unwrap_or_default().trim().to_string();
    cx.update(|s| s.record_scrub(&first, cx.now()))?;
    if !clean(&first) {
        return Err(Stop::failed(format!(
            "The scrub didn't come back clean ({first}); fix the pool before removing a vdev."
        )));
    }
    cx.flow.info(format!("The scrub is clean: {first}"));
    Ok(())
}
