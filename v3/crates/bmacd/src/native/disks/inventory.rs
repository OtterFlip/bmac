// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

//! `storage.inventory` (v2 `inventory_disks.sh`, `command_retire_luks`,
//! `command_release_disks`): lists every disk outside the pool, finishes
//! what removals and replacements leave behind, and says which disks can be
//! pulled.
//!
//! Finishing closes the old members' LUKS mappings, takes them out of
//! crypttab, rebuilds the initramfs and proves it no longer waits for them,
//! deletes their header backups, and erases the metadata of their disks
//! (LUKS headers, ZFS labels, partition tables; never the data). A disk
//! that something uses again is left alone and its record kept.

use bmac_core::DiskIdentity;
use bmac_core::job::Severity;
use bmac_exec::Command;
use bmac_proto::status::DiskStatus;
use bmac_storage::journal::{Removal, RemovalState, Replacement};
use bmac_storage::luks;
use serde_json::json;

use super::{Context, Snapshot, describe_disk, mapper_of, named, online};
use crate::native::Stop;
use crate::native::storage::CRYPTSETUP;

const SURVEY: (&str, &str) = ("survey", "Read the disks");
const FINISH: (&str, &str) = ("finish", "Finish removals and replacements");
const REPORT: (&str, &str) = ("report", "Report");

/// What is left to do for the removal in progress.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RemovalNext {
    /// ZFS is still copying the vdev's data.
    Evacuating,
    /// The vdev is in the pool and nothing is removing it: it was
    /// cancelled or failed.
    Stopped,
    /// The vdev is gone; its disks are to be retired.
    Finish,
}

pub fn removal_next(snapshot: &Snapshot, removal: &Removal) -> RemovalNext {
    match snapshot.top(&removal.vdev) {
        Some(top) if top.removing || snapshot.layout.pool.removal_in_progress => {
            RemovalNext::Evacuating
        }
        Some(_) => RemovalNext::Stopped,
        None => RemovalNext::Finish,
    }
}

/// Whether a replacement's new member is in and resilvered, so the old
/// one can be retired.
pub fn replacement_done(snapshot: &Snapshot, disk: &DiskIdentity, r: &Replacement) -> bool {
    let Some(top) = snapshot.top(&r.vdev) else {
        return false;
    };
    !top.replacing
        && top
            .members
            .iter()
            .any(|m| m.identity.as_ref() == Some(disk) && online(m))
        && !snapshot
            .layout
            .pool
            .scan
            .as_deref()
            .is_some_and(|s| s.contains("resilver in progress"))
}

/// One piece of finishing work.
#[derive(Debug, Clone)]
enum Work {
    Removal(Removal),
    Replacement(DiskIdentity, Replacement),
}

impl Work {
    /// The mappers to retire and the disks to release.
    fn parts(&self, cx: &Context<'_>) -> (Vec<String>, Vec<DiskIdentity>) {
        match self {
            Work::Removal(r) => (
                r.members.iter().filter_map(|m| m.mapper.clone()).collect(),
                r.members
                    .iter()
                    .filter_map(|m| m.identity.clone())
                    .collect(),
            ),
            Work::Replacement(disk, r) => {
                let new_mapper = luks::mapper_name(&cx.pool, disk);
                let drill =
                    r.old.as_ref() == Some(disk) || r.mapper.as_deref() == Some(&new_mapper);
                if drill {
                    (Vec::new(), Vec::new())
                } else {
                    (
                        r.mapper.clone().into_iter().collect(),
                        r.old.clone().into_iter().collect(),
                    )
                }
            }
        }
    }

    fn describe(&self) -> String {
        match self {
            Work::Removal(r) => format!("the removal of {}", r.vdev),
            Work::Replacement(disk, r) => {
                format!("the replacement of {} by {}", r.leaf, named(disk))
            }
        }
    }
}

pub async fn inventory(cx: &Context<'_>) -> Result<(), Stop> {
    cx.phase(SURVEY);
    let snapshot = cx.snapshot().await?;
    let mut work = Vec::new();
    if let Some(removal) = snapshot.journal.removal_in_progress() {
        match removal_next(&snapshot, removal) {
            RemovalNext::Evacuating => cx.flow.info(format!(
                "{} is still being removed: {}. Its disks aren't safe to pull yet.",
                removal.vdev,
                snapshot
                    .layout
                    .pool
                    .remove
                    .as_deref()
                    .unwrap_or("in progress")
            )),
            RemovalNext::Stopped => {
                let note = "the vdev is still in the pool and no removal is running";
                cx.update(|s| {
                    s.set_removal(&removal.id, RemovalState::Failed, Some(note), cx.now())
                })?;
                cx.flow.warning(format!(
                    "The removal of {} stopped with the vdev still in the pool (cancelled, or it \
                     failed). Its record is closed; nothing was changed.",
                    removal.vdev
                ));
            }
            RemovalNext::Finish => work.push(Work::Removal(removal.clone())),
        }
    }
    for (disk, r) in &snapshot.journal.replacements {
        if replacement_done(&snapshot, disk, r) {
            work.push(Work::Replacement(disk.clone(), r.clone()));
        }
    }
    let in_pool: Vec<DiskIdentity> = snapshot
        .tops()
        .iter()
        .flat_map(|t| t.members.iter())
        .filter_map(|m| m.identity.clone())
        .collect();
    for addition in &snapshot.journal.additions {
        if addition.members.iter().all(|m| in_pool.contains(m)) {
            cx.update(|s| {
                s.finish_addition(addition.number);
                Ok(())
            })?;
        }
    }
    cx.done(SURVEY);

    if !work.is_empty() {
        cx.phase(FINISH);
        let mut details = Vec::new();
        for w in &work {
            let (mappers, disks) = w.parts(cx);
            let mut line = format!("Finish {}", w.describe());
            if !mappers.is_empty() {
                line += &format!("; retire {}", mappers.join(", "));
            }
            let present: Vec<String> = disks
                .iter()
                .filter_map(|d| snapshot.disk_with(d))
                .map(describe_disk)
                .collect();
            if !present.is_empty() {
                line += &format!("; erase the metadata of {}", present.join(", "));
            }
            details.push(line);
        }
        let confirmed = cx
            .flow
            .confirm(
                "Finish removals and replacements",
                "Closes the old LUKS mappings, takes them out of crypttab and the initramfs, and \
                 erases the old disks' LUKS headers, ZFS labels, and partition tables. Their data \
                 isn't overwritten.",
                Severity::Warning,
                None,
                details,
            )
            .await?;
        if confirmed {
            for w in &work {
                finish(cx, w).await?;
            }
        } else {
            cx.flow.info("Nothing was finished.");
        }
        cx.done(FINISH);
    }

    cx.phase(REPORT);
    let snapshot = cx.snapshot().await?;
    let disks = crate::status::disks::disks(&snapshot.layout, &snapshot.journal);
    if disks.is_empty() {
        cx.flow.info("Every disk is in the pool.");
    }
    for d in &disks {
        cx.flow.info(format!(
            "{} ({}): {}. {}",
            d.disk,
            d.model.as_deref().unwrap_or("disk"),
            d.label,
            d.detail
        ));
    }
    let pullable: Vec<&str> = disks
        .iter()
        .filter(|d| d.removable && d.status == DiskStatus::Available)
        .map(|d| d.disk.as_str())
        .collect();
    if !pullable.is_empty() {
        cx.flow
            .info(format!("Safe to pull: {}.", pullable.join(", ")));
    }
    if snapshot.layout.pool.health.as_deref() == Some("DEGRADED")
        && snapshot.tops().iter().all(|t| t.members.iter().all(online))
    {
        cx.flow.next_step(
            "The pool still says DEGRADED though every member is online; a scrub clears that.",
        );
    }
    cx.flow.result(json!({
        "disks": disks,
        "safe_to_pull": pullable,
    }));
    cx.done(REPORT);
    Ok(())
}

async fn finish(cx: &Context<'_>, work: &Work) -> Result<(), Stop> {
    let (mappers, disks) = work.parts(cx);
    let snapshot = cx.snapshot().await?;
    let in_pool: Vec<String> = snapshot
        .tops()
        .iter()
        .flat_map(|t| t.members.iter())
        .filter_map(mapper_of)
        .collect();
    for mapper in &mappers {
        if in_pool.contains(mapper) {
            return Err(Stop::failed(format!(
                "{mapper} is still in {}; {} isn't finished",
                cx.pool,
                work.describe()
            )));
        }
        if cx.root.join("dev/mapper").join(mapper).exists() {
            cx.run(Command::new(CRYPTSETUP).args(["close", mapper.as_str()]))
                .await?;
            cx.flow.info(format!("Closed {mapper}."));
        }
    }
    if !mappers.is_empty() {
        let layout = snapshot.pool_layout(&cx.pool, None);
        cx.storage(&layout).unregister(&mappers).await?;
        for mapper in &mappers {
            cx.delete_header(mapper);
        }
    }
    let snapshot = cx.snapshot().await?;
    let mut kept = Vec::new();
    for identity in &disks {
        let Some(disk) = snapshot.disk_with(identity) else {
            continue;
        };
        if snapshot.holds_member(disk) {
            kept.push(format!("{} is in a pool again", describe_disk(disk)));
            continue;
        }
        let reasons = snapshot
            .row(disk)
            .map(|r| r.in_use_reasons.clone())
            .unwrap_or_default();
        if !reasons.is_empty() {
            kept.push(format!("{}: {}", describe_disk(disk), reasons.join("; ")));
            continue;
        }
        let disk = cx.resolve(identity)?;
        cx.flow
            .info(format!("Erasing the metadata of {}.", describe_disk(&disk)));
        cx.wipe_metadata(&disk).await?;
    }
    if !kept.is_empty() {
        cx.flow.warning(format!(
            "Left alone, and {} stays open: {}.",
            work.describe(),
            kept.join("; ")
        ));
        return Ok(());
    }
    match work {
        Work::Removal(r) => {
            cx.update(|s| s.set_removal(&r.id, RemovalState::Retired, None, cx.now()))?;
        }
        Work::Replacement(disk, _) => {
            cx.update(|s| {
                s.clear_replacement(disk);
                Ok(())
            })?;
        }
    }
    cx.flow.info(format!("Finished {}.", work.describe()));
    Ok(())
}
