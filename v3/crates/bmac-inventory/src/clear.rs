// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

//! `clear-zfs-labels`, the inventory's one writing command
//! (`IMPLEMENTATION_PLAN.md` M6.3): clears the old `rpool` labels the
//! operator confirmed, after re-identifying every disk, and proves they are
//! gone.

use std::path::{Path, PathBuf};
use std::process::Command;

use bmac_proto::inventory::{ClearReport, ClearRequest, InventoryDisk};

use crate::disks::read_disks;
use crate::root::Root;
use crate::zfs::read_label;

pub trait LabelClearer {
    /// Clears `device`, which starts `start` bytes into its disk.
    fn clear(&self, device: &Path, start: u64) -> Result<(), String>;
}

/// `zpool labelclear -f <device>` where ZFS is installed; where it isn't,
/// the four label slots are zeroed directly, which is what `labelclear -f`
/// writes. Either way, copies stranded near the end by a grown partition
/// are zeroed too, which `labelclear` doesn't look for; the re-read
/// afterwards proves it.
pub struct ZpoolLabelClear {
    zpool: Option<PathBuf>,
}

impl ZpoolLabelClear {
    /// With the system's `zpool`, if it has one.
    pub fn installed() -> ZpoolLabelClear {
        ZpoolLabelClear {
            zpool: ["/usr/sbin/zpool", "/sbin/zpool", "/usr/bin/zpool"]
                .iter()
                .map(PathBuf::from)
                .find(|p| p.is_file()),
        }
    }

    pub fn with(zpool: Option<PathBuf>) -> ZpoolLabelClear {
        ZpoolLabelClear { zpool }
    }
}

impl LabelClearer for ZpoolLabelClear {
    fn clear(&self, device: &Path, start: u64) -> Result<(), String> {
        let Some(zpool) = &self.zpool else {
            return crate::zfs::zero_labels(device, start)
                .map_err(|e| format!("couldn't clear {}: {e}", device.display()));
        };
        let output = Command::new(zpool)
            .arg("labelclear")
            .arg("-f")
            .arg(device)
            .output()
            .map_err(|e| format!("couldn't run zpool: {e}"))?;
        // It refuses a device whose standard slots hold no label, which is
        // what stranded copies look like, so its refusal isn't the verdict.
        crate::zfs::zero_labels(device, start).map_err(|e| {
            let refusal = String::from_utf8_lossy(&output.stderr).trim().to_string();
            format!("couldn't clear {}: {e} ({refusal})", device.display())
        })
    }
}

fn same_wwn(expected: Option<&str>, found: Option<&str>) -> bool {
    let norm = |w: Option<&str>| {
        w.map(|w| w.trim().to_ascii_lowercase())
            .filter(|w| !w.is_empty())
    };
    norm(expected) == norm(found)
}

/// Which devices to clear, each with where it starts on its disk, or why
/// nothing may be touched.
fn plan(
    disks: &[InventoryDisk],
    request: &ClearRequest,
) -> Result<Vec<(PathBuf, u64)>, Vec<String>> {
    let mut devices = Vec::new();
    let mut refused = Vec::new();
    if request.disks.is_empty() {
        refused.push("No disks were named.".to_string());
    }
    for target in &request.disks {
        let serial = target.serial.trim();
        let matches: Vec<&InventoryDisk> = disks
            .iter()
            .filter(|d| d.serial.as_deref().map(str::trim) == Some(serial))
            .collect();
        let disk = match matches.as_slice() {
            [disk] => *disk,
            [] => {
                refused.push(format!("No disk reports serial {serial}."));
                continue;
            }
            several => {
                refused.push(format!(
                    "{} disks report serial {serial}, so the right one can't be told apart.",
                    several.len()
                ));
                continue;
            }
        };
        if !same_wwn(target.wwn.as_deref(), disk.wwn.as_deref()) {
            refused.push(format!(
                "The disk with serial {serial} ({}) reports WWN {}, not {}.",
                disk.path,
                disk.wwn.as_deref().unwrap_or("none"),
                target.wwn.as_deref().unwrap_or("none"),
            ));
            continue;
        }
        let labelled: Vec<(PathBuf, u64)> = disk
            .zfs_labels
            .iter()
            .filter(|l| l.pool_guid.as_deref() == Some(target.pool_guid.as_str()))
            .map(|l| {
                let start = disk
                    .partitions
                    .iter()
                    .find(|p| p.path == l.device)
                    .map_or(0, |p| p.start_bytes);
                (PathBuf::from(&l.device), start)
            })
            .collect();
        if labelled.is_empty() {
            refused.push(format!(
                "The disk with serial {serial} ({}) carries no label of the pool with GUID {}.",
                disk.path, target.pool_guid
            ));
            continue;
        }
        devices.extend(labelled);
    }
    if refused.is_empty() {
        Ok(devices)
    } else {
        Err(refused)
    }
}

pub fn clear_zfs_labels(
    root: &Root,
    request: &ClearRequest,
    clearer: &dyn LabelClearer,
) -> ClearReport {
    let disks = read_disks(root);
    let devices = match plan(&disks, request) {
        Ok(devices) => devices,
        Err(refused) => {
            return ClearReport {
                refused,
                ..ClearReport::default()
            };
        }
    };
    let mut report = ClearReport::default();
    for (device, start) in &devices {
        if let Err(message) = clearer.clear(device, *start) {
            report
                .errors
                .push(format!("{}: {message}", device.display()));
        }
    }
    for (device, start) in devices {
        let name = device.display().to_string();
        match read_label(&device, start) {
            Ok(None) => report.cleared.push(name),
            Ok(Some(_)) => report.survived.push(name),
            Err(error) => report
                .errors
                .push(format!("{name}: couldn't read it back: {error}")),
        }
    }
    report
}
