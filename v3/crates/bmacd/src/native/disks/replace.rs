// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

//! `storage.replace_disk` (v2 `add_replacement_disk.sh` and
//! `rpool_mirror.sh`'s `command_replace_member`).
//!
//! A boot member gets the surviving member's partition table, with new
//! GUIDs, and its own ESP before its pool partition is prepared. Other
//! members get the one partition every non-boot member has. The job ends at
//! `zpool replace`; the resilver shows on the Storage page, and Storage
//! inventory retires the old member's LUKS mapping and crypttab line once
//! it has finished. A hot spare the ZFS event daemon already put in for the
//! failed member stays in until then and returns to the spares by itself.

use std::collections::BTreeMap;

use bmac_core::DiskIdentity;
use bmac_core::job::{FieldType, Severity};
use bmac_exec::Command;
use bmac_proto::inventory::InventoryDisk;
use bmac_storage::journal::Replacement;
use bmac_storage::luks;
use bmac_storage::{Encryption, PoolLayout, Vdev, VdevClass, VdevType};
use serde_json::{Map, Value, json};

use super::add::big_enough;
use super::{
    Context, POOL_PHASE, Snapshot, Top, by_id, choice, describe_disk, field, mapper_of, named,
    online, refuse, text,
};
use crate::native::Stop;
use crate::native::storage::{
    BOOT_PARTITION, PARTX, PROXMOX_BOOT_TOOL, SGDISK, SLOW, UDEVADM, mapper_path,
};

const CHOOSE: (&str, &str) = ("choose", "Choose the member and the new disk");
const PREPARE: (&str, &str) = ("prepare", "Prepare the new disk");
const FINISH: (&str, &str) = ("finish", "Finish");
const ESP_PARTITION: u32 = 2;
const UEFI: &str = "/sys/firmware/efi";
/// The GPT's backup header at the end of the disk.
const GPT_TAIL_BYTES: u64 = 1 << 20;

/// A member that may be replaced, as the operator picks it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Choice {
    pub vdev: String,
    pub leaf: String,
    pub state: String,
}

/// The spare devices' leaf paths.
fn spares(snapshot: &Snapshot) -> Vec<String> {
    snapshot
        .tops()
        .iter()
        .filter(|t| t.class == VdevClass::Spare)
        .flat_map(|t| t.members.iter().map(|m| m.path.clone()))
        .collect()
}

/// Whether this member may be replaced now; why not otherwise.
pub fn replaceable(
    top: &Top<'_>,
    member: &bmac_adapters::layout::Member,
    spares: &[String],
) -> Result<(), String> {
    if matches!(top.class, VdevClass::Cache | VdevClass::Spare) {
        return Err(format!(
            "a {} device is removed and added, not replaced",
            top.class
        ));
    }
    if top.removing {
        return Err("its vdev is being removed".into());
    }
    if member
        .group
        .as_deref()
        .is_some_and(|g| g.starts_with("replacing-"))
    {
        return Err("it is already being replaced".into());
    }
    if member
        .group
        .as_deref()
        .is_some_and(|g| g.starts_with("spare-"))
        && spares.contains(&member.path)
    {
        return Err("it is a hot spare standing in for another member".into());
    }
    if !online(member) && !top.redundant() {
        return Err(format!(
            "it isn't online and {} has no redundancy, so there is nothing to rebuild it from",
            top.name
        ));
    }
    Ok(())
}

pub fn choices(snapshot: &Snapshot) -> (Vec<Choice>, Vec<String>) {
    let spares = spares(snapshot);
    let mut ok = Vec::new();
    let mut not = Vec::new();
    for top in snapshot.tops() {
        for member in top.members {
            match replaceable(&top, member, &spares) {
                Ok(()) => ok.push(Choice {
                    vdev: top.name.to_string(),
                    leaf: member.path.clone(),
                    state: member.state.clone().unwrap_or_else(|| "UNKNOWN".into()),
                }),
                Err(why) => not.push(format!("{} in {}: {why}", member.path, top.name)),
            }
        }
    }
    (ok, not)
}

fn choice_value(c: &Choice) -> String {
    format!("{}\t{}", c.vdev, c.leaf)
}

/// The online member of a boot vdev to copy the partition table and ESP
/// from.
fn survivor<'s>(snapshot: &'s Snapshot, top: &Top<'_>, leaf: &str) -> Option<&'s InventoryDisk> {
    top.members
        .iter()
        .filter(|m| m.path != leaf && online(m))
        .find_map(|m| snapshot.disk_named(m.disk.as_deref()?))
}

/// Room a disk needs to take the survivor's partitions.
pub fn boot_room(survivor: &InventoryDisk) -> u64 {
    survivor
        .partitions
        .iter()
        .map(|p| p.start_bytes + p.size_bytes)
        .max()
        .unwrap_or(0)
        + GPT_TAIL_BYTES
}

fn check_choice(
    snapshot: &Snapshot,
    values: &Map<String, Value>,
) -> Result<(), BTreeMap<String, String>> {
    let chosen = text(values, "member");
    let (vdev, leaf) = chosen
        .split_once('\t')
        .ok_or_else(|| refuse("member", "Choose a member."))?;
    let top = snapshot
        .top(vdev)
        .ok_or_else(|| refuse("member", "That vdev is gone."))?;
    let candidates = snapshot.candidates();
    let new = candidates
        .iter()
        .find(|c| c.by_id.as_str() == text(values, "disk"))
        .ok_or_else(|| refuse("disk", "Choose a disk."))?;
    let member = top.members.iter().find(|m| m.path == leaf);
    if member.and_then(|m| m.identity.as_ref()) == Some(&new.identity) {
        return Err(refuse("disk", "That is the member being replaced."));
    }
    if top.holds_esp {
        let survivor = survivor(snapshot, &top, leaf).ok_or_else(|| {
            refuse(
                "member",
                "The boot vdev has no other online member to copy from.",
            )
        })?;
        let needed = boot_room(survivor);
        if new.disk.size_bytes < needed {
            return Err(refuse(
                "disk",
                format!(
                    "Too small: the boot partitions need {}.",
                    bmac_core::Bytes(needed)
                ),
            ));
        }
        return Ok(());
    }
    big_enough(&top, new.disk.size_bytes, snapshot.encryption())
        .map_err(|why| refuse("disk", format!("Too small: {why}.")))
}

/// Offers the replacements earlier runs chose and didn't finish; the one
/// to carry on with, keyed by its new disk.
async fn pending(
    cx: &Context<'_>,
    snapshot: &Snapshot,
) -> Result<Option<(DiskIdentity, Replacement)>, Stop> {
    let in_pool = |identity: &DiskIdentity| {
        snapshot
            .tops()
            .iter()
            .flat_map(|t| t.members.iter())
            .any(|m| m.identity.as_ref() == Some(identity) && present(m))
    };
    let open: Vec<(DiskIdentity, Replacement)> = snapshot
        .journal
        .replacements
        .iter()
        .filter(|(disk, _)| !in_pool(disk))
        .map(|(d, r)| (d.clone(), r.clone()))
        .collect();
    let Some((disk, replacement)) = open.into_iter().next() else {
        return Ok(None);
    };
    if cx.flow.checkpoints().get(RESUME).and_then(Value::as_str) == Some(disk.value()) {
        return Ok(Some((disk, replacement)));
    }
    let mut how = field("choice", FieldType::Select, "What to do");
    how.default = Some(json!("resume"));
    how.options = vec![
        choice("resume", "Carry on replacing it", None),
        choice(
            "abandon",
            "Forget it",
            Some("The new disk stays as it is; Storage inventory lists it.".into()),
        ),
    ];
    let values = cx
        .flow
        .input_group(
            "A replacement isn't finished",
            Some(&format!(
                "An earlier run chose {} to replace {} in {} and stopped before zpool replace.",
                named(&disk),
                replacement.leaf,
                replacement.vdev
            )),
            vec![how],
            |_| Ok(()),
        )
        .await?;
    if text(&values, "choice") == "abandon" {
        cx.update(|s| {
            s.clear_replacement(&disk);
            Ok(())
        })?;
        return Ok(None);
    }
    Ok(Some((disk, replacement)))
}

/// A leaf ZFS is using or resilvering; a drilled disk's old leaf stays
/// OFFLINE under the same mapper name until it is replaced.
fn present(member: &bmac_adapters::layout::Member) -> bool {
    !matches!(
        member.state.as_deref(),
        Some("OFFLINE" | "UNAVAIL" | "FAULTED" | "REMOVED")
    )
}

const RESUME: &str = "resume";

async fn choose(
    cx: &Context<'_>,
    snapshot: &Snapshot,
) -> Result<(DiskIdentity, Replacement), Stop> {
    let (ok, not) = choices(snapshot);
    if ok.is_empty() {
        return Err(Stop::failed(format!(
            "No member can be replaced now. {}",
            not.join("; ")
        )));
    }
    let mut member = field("member", FieldType::Select, "Member to replace");
    member.options = ok
        .iter()
        .map(|c| {
            choice(
                &choice_value(c),
                &format!("{} in {} ({})", c.leaf, c.vdev, c.state),
                (c.state == "ONLINE").then(|| {
                    "Still working: it stays in until the new disk has resilvered.".to_string()
                }),
            )
        })
        .collect();
    if let Some(failed) = ok.iter().find(|c| c.state != "ONLINE") {
        member.default = Some(json!(choice_value(failed)));
    }
    let mut disk = field("disk", FieldType::Select, "New disk");
    disk.options = snapshot
        .candidates()
        .iter()
        .map(|c| choice(c.by_id.as_str(), &c.describe(), None))
        .collect();
    if disk.options.is_empty() {
        return Err(Stop::failed(
            "No disk can be used as the new member: every disk is in a pool, in use, chosen by \
             an unfinished workflow, or not recognizable by WWN or serial.",
        ));
    }
    let values = cx
        .flow
        .input_group(
            "Replace a disk",
            Some(&if not.is_empty() {
                "Choose the member and the blank disk to put in its place.".to_string()
            } else {
                format!("Not offered: {}.", not.join("; "))
            }),
            vec![member, disk],
            |values| check_choice(snapshot, values),
        )
        .await?;
    let chosen = text(&values, "member");
    let (vdev, leaf) = chosen.split_once('\t').unwrap_or_default();
    let top = snapshot
        .top(vdev)
        .ok_or_else(|| Stop::failed(format!("{vdev} is gone")))?;
    let old = top.members.iter().find(|m| m.path == leaf);
    let candidates = snapshot.candidates();
    let new = candidates
        .iter()
        .find(|c| c.by_id.as_str() == text(&values, "disk"))
        .ok_or_else(|| Stop::failed("the chosen disk is gone"))?;
    let mut details = vec![
        format!("New disk: {}", new.describe()),
        format!("Replaces: {leaf} in {vdev}"),
    ];
    if top.holds_esp {
        details.push(
            "It is a boot member: the new disk gets the surviving member's partition table and \
             its own boot partition first."
                .into(),
        );
    }
    let confirmed = cx
        .flow
        .confirm(
            "Erase the disk and replace the member",
            &format!(
                "Everything on {} is erased, and it replaces {leaf}.",
                new.describe()
            ),
            Severity::Destructive,
            Some("erase"),
            details,
        )
        .await?;
    if !confirmed {
        return Err(Stop::Cancelled);
    }
    let identity = new.identity.clone();
    let disk = cx.resolve(&identity)?;
    cx.flow.info(format!("Erasing {}.", describe_disk(&disk)));
    cx.wipe_metadata(&disk).await?;
    let replacement = Replacement {
        leaf: leaf.to_string(),
        old: old.and_then(|m| m.identity.clone()),
        mapper: old.and_then(mapper_of),
        vdev: vdev.to_string(),
        recorded_at: cx.now(),
    };
    cx.update(|s| s.record_replacement(&identity, replacement.clone()))?;
    Ok((identity, replacement))
}

pub async fn replace_disk(cx: &Context<'_>) -> Result<(), Stop> {
    cx.phase(CHOOSE);
    let snapshot = cx.snapshot().await?;
    if snapshot.layout.pool.removal_in_progress {
        return Err(Stop::failed(
            "A vdev removal is in progress; ZFS changes no vdev's members until it finishes. \
             Wait for it, or stop it with zpool remove -s.",
        ));
    }
    let (identity, replacement) = match pending(cx, &snapshot).await? {
        Some(found) => found,
        None => {
            let drill = snapshot.journal.simulation.clone();
            let snapshot = cx.snapshot().await?;
            let chosen = choose(cx, &snapshot).await?;
            if drill.is_some_and(|d| d.disk == chosen.0) {
                cx.flow
                    .info("This finishes the failure drill on that disk.");
            }
            chosen
        }
    };
    cx.flow
        .checkpoints()
        .record(RESUME, json!(identity.value()))?;
    cx.done(CHOOSE);

    let snapshot = cx.snapshot().await?;
    let top = snapshot
        .top(&replacement.vdev)
        .ok_or_else(|| Stop::failed(format!("{} is no longer in {}", replacement.vdev, cx.pool)))?;
    let disk = cx.resolve(&identity)?;
    let new_by_id = by_id(&disk)
        .ok_or_else(|| Stop::failed(format!("{} has no /dev/disk/by-id name", disk.name)))?;
    let encryption = snapshot.encryption();
    let boot = top.holds_esp;
    let class = top.class;
    let spare_in = top
        .members
        .iter()
        .find(|m| m.path == replacement.leaf)
        .and_then(|m| m.group.clone())
        .filter(|g| g.starts_with("spare-"));
    let mut survivor_disk = None;
    if boot {
        survivor_disk = Some(
            survivor(&snapshot, &top, &replacement.leaf)
                .cloned()
                .ok_or_else(|| {
                    Stop::failed("the boot vdev has no other online member to copy from")
                })?,
        );
    }
    let layout = PoolLayout {
        ashift: snapshot.ashift,
        ..PoolLayout::new(
            cx.pool.clone(),
            encryption,
            vec![Vdev::new(class, VdevType::Single, vec![new_by_id.clone()])],
        )
    };
    let storage = cx.storage(&layout);
    let members = cx
        .with_key(&snapshot, async {
            cx.phase(PREPARE);
            let (members, device) = match &survivor_disk {
                Some(survivor) => {
                    let device = prepare_boot(cx, &storage, survivor, &disk, encryption).await?;
                    (device.0, device.1)
                }
                None => {
                    let prepared = storage.prepare(0).await?;
                    if prepared.in_pool {
                        cx.done(PREPARE);
                        return Ok(prepared.members);
                    }
                    let device = prepared.devices().remove(0);
                    (prepared.members, device)
                }
            };
            cx.done(PREPARE);
            cx.phase(POOL_PHASE);
            if encryption == Encryption::Luks2 {
                storage.register(&members).await?;
            }
            let pool = cx.pool.as_str();
            cx.flow
                .info(format!("Replacing {} with {device}.", replacement.leaf));
            if replacement.leaf == device {
                cx.zpool(&["replace", pool, &device]).await?;
            } else {
                cx.zpool(&["replace", pool, &replacement.leaf, &device])
                    .await?;
            }
            cx.done(POOL_PHASE);
            Ok(members)
        })
        .await?;

    cx.phase(FINISH);
    if encryption == Encryption::Luks2 {
        cx.backup_headers(&members).await?;
    }
    cx.update(|s| {
        if s.simulation.as_ref().is_some_and(|d| d.disk == identity) {
            s.simulation = None;
        }
        Ok(())
    })?;
    cx.flow.info(format!(
        "ZFS is resilvering {} onto {}; the Storage page shows its progress.",
        replacement.vdev,
        describe_disk(&disk)
    ));
    if let Some(group) = spare_in {
        cx.flow.info(format!(
            "The hot spare in {group} goes back to the spares by itself when the resilver \
             finishes."
        ));
    }
    if replacement.mapper.is_some() || replacement.old.is_some() {
        cx.flow.next_step(
            "When the resilver has finished, run Storage inventory to retire the old member's \
             LUKS mapping and crypttab line and release its disk.",
        );
    }
    cx.flow.result(json!({
        "vdev": replacement.vdev,
        "replaced": replacement.leaf,
        "disk": identity.value(),
    }));
    cx.done(FINISH);
    Ok(())
}

/// Copies the survivor's partitions onto the new disk, gives it its own
/// ESP, and prepares its pool partition; the (mapper, partition) pair and
/// the device the pool takes. Every step looks at the disk first, so a
/// resumed run carries on.
async fn prepare_boot(
    cx: &Context<'_>,
    storage: &crate::native::storage::Storage<'_>,
    survivor: &InventoryDisk,
    disk: &InventoryDisk,
    encryption: Encryption,
) -> Result<(Vec<(String, String)>, String), Stop> {
    let survivor_path = by_id(survivor)
        .map(|b| b.path())
        .ok_or_else(|| Stop::failed(format!("{} has no /dev/disk/by-id name", survivor.name)))?;
    let identity = disk
        .identity
        .clone()
        .ok_or_else(|| Stop::failed(format!("{} has no identity", disk.name)))?;
    let new_path = by_id(disk).map(|b| b.path()).unwrap_or_default();
    let checkpoints = cx.flow.checkpoints();
    let current = cx.resolve(&identity)?;
    let same_table = current.partitions.len() == survivor.partitions.len()
        && current
            .partitions
            .iter()
            .zip(&survivor.partitions)
            .all(|(a, b)| {
                a.number == b.number
                    && a.start_bytes == b.start_bytes
                    && a.size_bytes == b.size_bytes
            });
    if !same_table || !checkpoints.is_done("partitions") {
        cx.flow.info(format!(
            "Copying {}'s partition table to {}.",
            survivor.name, disk.name
        ));
        cx.run(Command::new(SGDISK).args(["--zap-all", new_path.as_str()]))
            .await?;
        cx.run(
            Command::new(SGDISK).args([format!("--replicate={new_path}"), survivor_path.clone()]),
        )
        .await?;
        cx.run(Command::new(SGDISK).args(["--randomize-guids", new_path.as_str()]))
            .await?;
        cx.code(
            Command::new(PARTX).args(["--update", new_path.as_str()]),
            &[0, 1],
        )
        .await?;
        cx.run(
            Command::new(UDEVADM)
                .arg("settle")
                .timeout(std::time::Duration::from_secs(120)),
        )
        .await?;
        cx.flow.checkpoints().record("partitions", json!(true))?;
    }
    if !cx.flow.checkpoints().is_done("esp") {
        let esp = format!("{new_path}-part{ESP_PARTITION}");
        cx.flow
            .info(format!("Setting up the boot partition {esp}."));
        cx.run(
            Command::new(PROXMOX_BOOT_TOOL)
                .args(["format", esp.as_str(), "--force"])
                .timeout(SLOW),
        )
        .await?;
        let mut init = Command::new(PROXMOX_BOOT_TOOL).args(["init", esp.as_str()]);
        if !cx.root.join(UEFI.trim_start_matches('/')).exists() {
            init = init.arg("grub");
        }
        cx.run(init.timeout(SLOW)).await?;
        cx.run(Command::new(PROXMOX_BOOT_TOOL).arg("clean").timeout(SLOW))
            .await?;
        cx.flow.checkpoints().record("esp", json!(true))?;
    }
    let partition = format!("{new_path}-part{BOOT_PARTITION}");
    if encryption == Encryption::None {
        return Ok((Vec::new(), partition));
    }
    let mapper = luks::mapper_name(&cx.pool, &identity);
    if !storage.is_luks(&partition).await? {
        cx.flow.info(format!("Encrypting {partition}."));
        let start = cx
            .resolve(&identity)?
            .partitions
            .iter()
            .find(|p| p.number == BOOT_PARTITION)
            .map_or(0, |p| p.start_bytes);
        storage.format(&partition, start).await?;
    }
    if !storage.open_on(&mapper, &partition).await? {
        storage.open(&partition, &mapper).await?;
    }
    Ok((vec![(mapper.clone(), partition)], mapper_path(&mapper)))
}
