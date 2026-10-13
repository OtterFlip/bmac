// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

//! Every physical disk, from sysfs, udev's database, and the disk's own
//! first and last sectors. Serials, models, and WWNs are taken in the order
//! `lsblk` takes them, so the values match what the installed host shows.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use bmac_core::disk::DiskIdentity;
use bmac_proto::inventory::{DiskPartition, InventoryDisk, ZfsLabel};

use crate::luks::{read_luks, read_partition_table};
use crate::root::{Root, attr, attr_u64, names};
use crate::udev::UdevEntry;
use crate::zfs::read_label;

/// Block devices that are never a physical disk: loop and RAM devices,
/// device-mapper and md, optical drives (the virtual media the ISO boots
/// from), network block devices, and zvols.
const NOT_DISKS: &[&str] = &[
    "loop", "ram", "zram", "dm-", "md", "sr", "nbd", "zd", "fd", "rbd",
];

/// `nvmeXcYnZ` entries are hidden paths of a multipath namespace.
fn is_multipath_path(name: &str) -> bool {
    let Some(rest) = name.strip_prefix("nvme") else {
        return false;
    };
    let Some((controller, rest)) = rest.split_once('c') else {
        return false;
    };
    let Some((path, namespace)) = rest.split_once('n') else {
        return false;
    };
    [controller, path, namespace]
        .iter()
        .all(|part| !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit()))
}

pub fn is_disk_name(name: &str) -> bool {
    !NOT_DISKS.iter().any(|prefix| name.starts_with(prefix)) && !is_multipath_path(name)
}

fn transport(name: &str, udev: &UdevEntry) -> Option<String> {
    if name.starts_with("nvme") {
        return Some("nvme".into());
    }
    if name.starts_with("vd") {
        return Some("virtio".into());
    }
    let path = udev.get("ID_PATH").unwrap_or_default();
    let bus = udev.get("ID_BUS");
    if bus == Some("usb") || path.contains("-usb-") {
        return Some("usb".into());
    }
    if path.contains("-sas-") {
        return Some("sas".into());
    }
    if bus == Some("ata") {
        return Some(
            if udev.get("ID_ATA_SATA") == Some("1") {
                "sata"
            } else {
                "ata"
            }
            .into(),
        );
    }
    bus.map(str::to_string)
}

/// Reads every disk. Problems reading one disk's sectors are noted on that
/// disk rather than failing the inventory.
pub fn read_disks(root: &Root) -> Vec<InventoryDisk> {
    let mut disks: Vec<InventoryDisk> = names(&root.sys.join("block"))
        .into_iter()
        .filter(|name| is_disk_name(name))
        .filter_map(|name| read_disk(root, &name))
        .collect();
    mark_shared_identities(&mut disks);
    disks
}

fn read_disk(root: &Root, name: &str) -> Option<InventoryDisk> {
    let dir = root.sys.join("block").join(name);
    let size_bytes = attr_u64(&dir.join("size"))? * 512;
    if size_bytes == 0 {
        return None;
    }
    let udev = UdevEntry::for_block(root, attr(&dir.join("dev")).as_deref());
    // For SCSI disks ID_SERIAL_SHORT holds the WWN; lsblk prefers the SCSI serial.
    let serial = udev
        .get("ID_SCSI_SERIAL")
        .or_else(|| udev.get("ID_SERIAL_SHORT"))
        .or_else(|| udev.get("ID_SERIAL"))
        .map(str::to_string)
        .or_else(|| attr(&dir.join("device/serial")));
    let model = udev
        .decoded("ID_MODEL_ENC")
        .or_else(|| attr(&dir.join("device/model")))
        .or_else(|| udev.get("ID_MODEL").map(|m| m.replace('_', " ")));
    let wwn = udev
        .get("ID_WWN_WITH_EXTENSION")
        .or_else(|| udev.get("ID_WWN"))
        .map(str::to_string)
        .or_else(|| attr(&dir.join("wwid")))
        .or_else(|| attr(&dir.join("device/wwid")))
        .map(|w| w.to_ascii_lowercase());
    let logical = attr_u64(&dir.join("queue/logical_block_size")).unwrap_or(512) as u32;
    let physical =
        attr_u64(&dir.join("queue/physical_block_size")).unwrap_or(u64::from(logical)) as u32;
    let path = root.device(name);
    let mut problems = Vec::new();

    let mut partitions = Vec::new();
    for part in names(&dir) {
        let part_dir = dir.join(&part);
        let Some(number) = attr_u64(&part_dir.join("partition")) else {
            continue;
        };
        let part_path = root.device(&part);
        partitions.push(DiskPartition {
            path: part_path.display().to_string(),
            number: number as u32,
            start_bytes: attr_u64(&part_dir.join("start")).unwrap_or(0) * 512,
            size_bytes: attr_u64(&part_dir.join("size")).unwrap_or(0) * 512,
            luks: read_luks(&part_path).ok().flatten(),
            name: part,
        });
    }

    let mut zfs_labels: Vec<ZfsLabel> = Vec::new();
    for (device, start) in std::iter::once((path.clone(), 0)).chain(
        partitions
            .iter()
            .map(|p| (PathBuf::from(&p.path), p.start_bytes)),
    ) {
        match read_label(&device, start) {
            Ok(Some(label)) => zfs_labels.push(label),
            Ok(None) => {}
            Err(error) => problems.push(format!("Couldn't read {}: {error}.", device.display())),
        }
    }
    // A partition that runs to the end of the disk puts its last label
    // copies where the whole disk's would be. That isn't a label on the
    // whole disk, and clearing it would wipe the partition table.
    let whole = path.display().to_string();
    let on_partitions: Vec<Option<String>> = zfs_labels
        .iter()
        .filter(|l| l.device != whole)
        .map(|l| l.vdev_guid.clone())
        .collect();
    zfs_labels.retain(|l| l.device != whole || !on_partitions.contains(&l.vdev_guid));

    Some(InventoryDisk {
        identity: DiskIdentity::from_reported(wwn.as_deref(), serial.as_deref()),
        path: path.display().to_string(),
        model,
        serial,
        wwn,
        size_bytes,
        logical_sector_bytes: logical,
        physical_sector_bytes: physical,
        rotational: attr(&dir.join("queue/rotational")).as_deref() == Some("1"),
        removable: attr(&dir.join("removable")).as_deref() == Some("1"),
        transport: transport(name, &udev),
        by_id: udev.by_id(),
        udev_identity: udev.identity(),
        partition_table: read_partition_table(&path, logical).ok().flatten(),
        partitions,
        luks: read_luks(&path).ok().flatten(),
        zfs_labels,
        problems,
        name: name.to_string(),
    })
}

/// A disk BMAC can't tell apart from another can't be chosen for anything
/// (`STORAGE.md`, "Disks"); v2 required exactly one disk per serial for the
/// same reason (`rpool_mirror.sh`, `disk_by_serial`).
fn mark_shared_identities(disks: &mut [InventoryDisk]) {
    let mut by_identity: BTreeMap<DiskIdentity, Vec<String>> = BTreeMap::new();
    for disk in disks.iter() {
        if let Some(identity) = &disk.identity {
            by_identity
                .entry(identity.clone())
                .or_default()
                .push(disk.path.clone());
        }
    }
    for disk in disks.iter_mut() {
        match &disk.identity {
            None => disk.problems.push(
                "Reports no serial number and no WWN, so BMAC can't select it safely.".into(),
            ),
            Some(identity) => {
                let others: Vec<&str> = by_identity[identity]
                    .iter()
                    .filter(|p| **p != disk.path)
                    .map(String::as_str)
                    .collect();
                if !others.is_empty() {
                    let what = match identity {
                        DiskIdentity::Wwn(_) => "WWN",
                        DiskIdentity::Serial(_) => "serial number",
                    };
                    disk.problems.push(format!(
                        "Reports the same {what} ({}) as {}, so BMAC can't tell them apart.",
                        identity.value(),
                        others.join(", ")
                    ));
                }
            }
        }
    }
}

/// The disk that holds a device, by the device's kernel name.
pub fn disk_of<'a>(disks: &'a [InventoryDisk], device: &Path) -> Option<&'a InventoryDisk> {
    let device = device.display().to_string();
    disks
        .iter()
        .find(|d| d.path == device || d.partitions.iter().any(|p| p.path == device))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_physical_disks_count() {
        for name in ["sda", "nvme0n1", "vda", "nvme12n3"] {
            assert!(is_disk_name(name), "{name}");
        }
        for name in [
            "loop0",
            "sr0",
            "dm-1",
            "md127",
            "zd16",
            "nvme0c0n1",
            "zram0",
            "nbd3",
        ] {
            assert!(!is_disk_name(name), "{name}");
        }
    }
}
