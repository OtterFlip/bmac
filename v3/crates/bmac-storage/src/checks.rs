// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

//! The layout checks (`STORAGE.md`, "Checks"): what is refused outright and
//! what is warned about and needs an acknowledgment. Messages are written
//! for the operator and shown as they are.

use std::collections::{BTreeMap, BTreeSet};

use bmac_core::{Bytes, DiskIdentity};
use serde::{Deserialize, Serialize};

use crate::layout::{ById, Encryption, PoolLayout, Vdev, VdevClass, VdevType};
use crate::partition::{self, MIN_DISK_BYTES};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Transport {
    Nvme,
    Sata,
    Sas,
    Usb,
    Other,
}

/// One disk from an inventory, as the machine itself reports it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Disk {
    pub by_id: ById,
    /// `None` when the disk reports neither a WWN nor a serial number.
    pub identity: Option<DiskIdentity>,
    pub size: u64,
    pub logical_sector: u32,
    pub physical_sector: u32,
    pub transport: Transport,
    pub rotational: bool,
    pub model: String,
    /// A logical disk presented by a hardware RAID controller.
    #[serde(default)]
    pub hardware_raid: bool,
    /// Why the disk can't be used, such as "member of pool tank".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub in_use: Option<String>,
}

impl Disk {
    /// "WWN 0x5000c500a1b2c3d4, 4.0 TB, ST4000NM0035", for messages and the
    /// destructive confirmation.
    pub fn describe(&self) -> String {
        let id = match &self.identity {
            Some(DiskIdentity::Wwn(w)) => format!("WWN {w}"),
            Some(DiskIdentity::Serial(s)) => format!("serial {s}"),
            None => self.by_id.to_string(),
        };
        format!("{id}, {}, {}", Bytes(self.size), self.model)
    }
}

/// What the pool is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Target {
    /// A Proxmox host's root pool: one boot vdev, created by the installer.
    Host,
    /// A backup appliance's data pool: no boot vdev.
    Appliance,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Code {
    // Refused.
    Structure,
    UnknownDisk,
    NoIdentity,
    HardwareRaid,
    DuplicateDisk,
    DiskInUse,
    DiskTooSmall,
    SpareTooSmall,
    LuksSingleBoot,
    BootVdevCount,
    BootVdevType,
    BootSectorMismatch,
    BootOnAppliance,
    AshiftTooSmall,
    // Warned.
    MixedCapacity,
    MixedSpeed,
    MismatchedRedundancy,
    MismatchedShape,
    HelperLessRedundant,
    SingleLog,
    NonRedundantData,
    UsbDisk,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Finding {
    pub code: Code,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vdev: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disk: Option<ById>,
    pub message: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Findings {
    pub refused: Vec<Finding>,
    pub warnings: Vec<Finding>,
    /// `zpool create` or `add` needs `-f`, passed only after the warnings
    /// are acknowledged.
    pub needs_force: bool,
}

impl Findings {
    pub fn is_allowed(&self) -> bool {
        self.refused.is_empty()
    }

    fn refuse(&mut self, code: Code, vdev: Option<usize>, disk: Option<&ById>, message: String) {
        self.refused.push(Finding {
            code,
            vdev,
            disk: disk.cloned(),
            message,
        });
    }

    fn warn(&mut self, code: Code, vdev: Option<usize>, disk: Option<&ById>, message: String) {
        self.warnings.push(Finding {
            code,
            vdev,
            disk: disk.cloned(),
            message,
        });
    }
}

/// The ashift a pool of these disks needs: 12, or the largest physical
/// sector size if that is larger.
pub fn required_ashift<'a>(disks: impl IntoIterator<Item = &'a Disk>) -> u8 {
    let largest = disks
        .into_iter()
        .map(|d| d.physical_sector.max(d.logical_sector))
        .max()
        .unwrap_or(0);
    let needed = if largest.is_power_of_two() {
        largest.trailing_zeros() as u8
    } else {
        0
    };
    needed.max(crate::layout::DEFAULT_ASHIFT)
}

/// The Proxmox installer's `zfs.raid` value for a boot vdev of this type.
pub fn installer_raid(kind: VdevType) -> Option<&'static str> {
    match kind {
        VdevType::Single => Some("raid0"),
        VdevType::Mirror => Some("raid1"),
        VdevType::Raidz1 => Some("raidz-1"),
        VdevType::Raidz2 => Some("raidz-2"),
        VdevType::Raidz3 => Some("raidz-3"),
    }
}

fn plural(n: usize, one: &str) -> String {
    if n == 1 {
        format!("1 {one}")
    } else {
        format!("{n} {one}s")
    }
}

/// Every check that needs the disks. `inventory` is every disk on the
/// machine; disks already in the pool must not be marked in use.
pub fn check(layout: &PoolLayout, inventory: &[Disk], target: Target) -> Findings {
    let mut f = Findings::default();
    if let Err(e) = layout.validate() {
        f.refuse(Code::Structure, None, None, e.to_string());
        return f;
    }
    let by_id: BTreeMap<&ById, &Disk> = inventory.iter().map(|d| (&d.by_id, d)).collect();

    check_disks(layout, &by_id, &mut f);
    check_boot(layout, &by_id, target, &mut f);
    check_spares(layout, &by_id, &mut f);
    check_ashift(layout, &by_id, &mut f);
    check_vdev_members(layout, &by_id, &mut f);
    check_redundancy(layout, &mut f);
    f
}

fn check_disks(layout: &PoolLayout, by_id: &BTreeMap<&ById, &Disk>, f: &mut Findings) {
    let mut seen = BTreeSet::new();
    let mut identities = BTreeMap::new();
    for (index, vdev) in layout.vdevs.iter().enumerate() {
        for member in &vdev.members {
            let at = Some(index);
            if !seen.insert(member) {
                f.refuse(
                    Code::DuplicateDisk,
                    at,
                    Some(member),
                    format!("disk {member} is listed twice"),
                );
                continue;
            }
            let Some(disk) = by_id.get(member) else {
                f.refuse(
                    Code::UnknownDisk,
                    at,
                    Some(member),
                    format!("disk {member} is not in the inventory"),
                );
                continue;
            };
            match &disk.identity {
                None => f.refuse(
                    Code::NoIdentity,
                    at,
                    Some(member),
                    format!("disk {member} reports no serial number and no WWN, so BMAC cannot tell it apart from others"),
                ),
                Some(identity) => {
                    if let Some(other) = identities.insert(identity.clone(), member) {
                        f.refuse(
                            Code::DuplicateDisk,
                            at,
                            Some(member),
                            format!("disks {other} and {member} report the same identity, {}; they are one disk", identity.value()),
                        );
                    }
                }
            }
            if disk.hardware_raid {
                f.refuse(
                    Code::HardwareRaid,
                    at,
                    Some(member),
                    format!(
                        "disk {member} is a logical disk presented by a hardware RAID controller; put the \
                         controller in HBA or passthrough mode so ZFS and SMART see the real drive"
                    ),
                );
            }
            if let Some(why) = &disk.in_use {
                f.refuse(
                    Code::DiskInUse,
                    at,
                    Some(member),
                    format!("disk {member} is already in use: {why}"),
                );
            }
            if disk.size < MIN_DISK_BYTES {
                f.refuse(
                    Code::DiskTooSmall,
                    at,
                    Some(member),
                    format!(
                        "disk {member} has {}, less than the 8 GiB minimum",
                        Bytes(disk.size)
                    ),
                );
            }
            if disk.transport == Transport::Usb {
                f.warn(
                    Code::UsbDisk,
                    at,
                    Some(member),
                    format!(
                        "disk {member} is attached by USB; USB bridges drop out under load and often hide \
                         SMART data"
                    ),
                );
            }
        }
    }
}

fn check_boot(
    layout: &PoolLayout,
    by_id: &BTreeMap<&ById, &Disk>,
    target: Target,
    f: &mut Findings,
) {
    let boots: Vec<(usize, &Vdev)> = layout.boot_vdevs().collect();
    match target {
        Target::Appliance => {
            for (index, _) in &boots {
                f.refuse(
                    Code::BootOnAppliance,
                    Some(*index),
                    None,
                    "a backup appliance's pool has no boot vdev".to_string(),
                );
            }
            return;
        }
        Target::Host if boots.len() != 1 => {
            f.refuse(
                Code::BootVdevCount,
                None,
                None,
                format!(
                    "a Proxmox host's pool needs exactly one boot vdev, not {}",
                    boots.len()
                ),
            );
            return;
        }
        Target::Host => {}
    }
    let (index, boot) = boots[0];
    if installer_raid(boot.kind).is_none() {
        f.refuse(
            Code::BootVdevType,
            Some(index),
            None,
            format!(
                "the Proxmox installer cannot create a {} boot vdev",
                boot.kind
            ),
        );
    }
    if layout.encryption == Encryption::Luks2 && boot.failures_tolerated() == 0 {
        f.refuse(
            Code::LuksSingleBoot,
            Some(index),
            None,
            "an encrypted host needs a redundant boot vdev (a mirror or raidz), because the boot vdev is \
             encrypted in place one disk at a time after installation"
                .to_string(),
        );
    }
    let sectors: BTreeSet<u32> = boot
        .members
        .iter()
        .filter_map(|m| by_id.get(m))
        .map(|d| d.logical_sector)
        .collect();
    if sectors.len() > 1 {
        let sizes = sectors
            .iter()
            .map(|s| format!("{s}-byte"))
            .collect::<Vec<_>>()
            .join(" and ");
        f.refuse(
            Code::BootSectorMismatch,
            Some(index),
            None,
            format!(
                "the boot vdev mixes {sizes} logical sectors; boot members need one sector size so a \
                 replacement can take a copy of the survivor's partition table"
            ),
        );
    }
}

fn check_spares(layout: &PoolLayout, by_id: &BTreeMap<&ById, &Disk>, f: &mut Findings) {
    let largest = layout
        .data_vdevs()
        .flat_map(|v| v.members.iter())
        .filter_map(|m| by_id.get(m))
        .max_by_key(|d| d.size);
    let Some(largest) = largest else { return };
    for (index, vdev) in layout
        .vdevs
        .iter()
        .enumerate()
        .filter(|(_, v)| v.class == VdevClass::Spare)
    {
        for member in &vdev.members {
            if let Some(spare) = by_id.get(member)
                && spare.size < largest.size
            {
                f.refuse(
                    Code::SpareTooSmall,
                    Some(index),
                    Some(member),
                    format!(
                        "spare {member} ({}) is smaller than data member {} ({}), which it could be asked to replace",
                        Bytes(spare.size),
                        largest.by_id,
                        Bytes(largest.size)
                    ),
                );
            }
        }
    }
}

fn check_ashift(layout: &PoolLayout, by_id: &BTreeMap<&ById, &Disk>, f: &mut Findings) {
    for member in layout.members() {
        let Some(disk) = by_id.get(member) else {
            continue;
        };
        let needed = required_ashift([*disk]);
        if layout.ashift < needed {
            f.refuse(
                Code::AshiftTooSmall,
                None,
                Some(member),
                format!(
                    "disk {member} has {}-byte physical sectors, which needs ashift {needed}; the layout uses {}",
                    disk.physical_sector.max(disk.logical_sector),
                    layout.ashift
                ),
            );
        }
    }
}

fn check_vdev_members(layout: &PoolLayout, by_id: &BTreeMap<&ById, &Disk>, f: &mut Findings) {
    for (index, vdev) in layout.vdevs.iter().enumerate() {
        let disks: Vec<&Disk> = vdev
            .members
            .iter()
            .filter_map(|m| by_id.get(m).copied())
            .collect();
        if disks.len() < 2 || matches!(vdev.class, VdevClass::Spare | VdevClass::Cache) {
            continue;
        }
        let smallest = disks.iter().map(|d| d.size).min().unwrap_or(0);
        let largest = disks.iter().map(|d| d.size).max().unwrap_or(0);
        if !partition::within_tolerance(smallest, largest) {
            f.warn(
                Code::MixedCapacity,
                Some(index),
                None,
                format!(
                    "the {} mixes disks of {} and {}; every member is used only up to the smallest, so {} of \
                     the largest goes unused",
                    vdev.describe(),
                    Bytes(smallest),
                    Bytes(largest),
                    Bytes(largest - smallest)
                ),
            );
        }
        let rotational: BTreeSet<bool> = disks.iter().map(|d| d.rotational).collect();
        let nvme: BTreeSet<bool> = disks
            .iter()
            .map(|d| d.transport == Transport::Nvme)
            .collect();
        let mix = if rotational.len() > 1 {
            Some("SSDs with spinning disks")
        } else if nvme.len() > 1 {
            Some("NVMe disks with SATA or SAS disks")
        } else {
            None
        };
        if let Some(mix) = mix {
            f.warn(
                Code::MixedSpeed,
                Some(index),
                None,
                format!(
                    "the {} mixes {mix}; it runs at the speed of its slowest member",
                    vdev.describe()
                ),
            );
        }
    }
}

fn check_redundancy(layout: &PoolLayout, f: &mut Findings) {
    let data: Vec<(usize, &Vdev)> = layout
        .vdevs
        .iter()
        .enumerate()
        .filter(|(_, v)| v.class == VdevClass::Data)
        .collect();
    let tolerated: BTreeSet<usize> = data.iter().map(|(_, v)| v.failures_tolerated()).collect();
    let shapes: BTreeSet<(VdevType, usize)> = data
        .iter()
        .map(|(_, v)| (v.kind, v.members.len()))
        .collect();
    let weakest = layout.data_redundancy();
    let force_note =
        "ZFS itself needs -f for this, and BMAC passes it only after you acknowledge this warning";
    if tolerated.len() > 1 {
        let (index, vdev) = data
            .iter()
            .find(|(_, v)| v.failures_tolerated() == weakest)
            .copied()
            .unwrap_or(data[0]);
        f.warn(
            Code::MismatchedRedundancy,
            Some(index),
            None,
            format!(
                "the data vdevs survive different numbers of disk failures; the pool is only as safe as \
                 vdev {index}, a {} that survives {}. {force_note}",
                vdev.describe(),
                plural(weakest, "failure")
            ),
        );
        f.needs_force = true;
    } else if shapes.len() > 1 {
        let list = data
            .iter()
            .map(|(_, v)| v.describe())
            .collect::<BTreeSet<_>>();
        f.warn(
            Code::MismatchedShape,
            None,
            None,
            format!(
                "the data vdevs differ in type or width ({}). {force_note}",
                list.into_iter().collect::<Vec<_>>().join(", ")
            ),
        );
        f.needs_force = true;
    }
    for (index, vdev) in &data {
        if vdev.failures_tolerated() == 0 {
            f.warn(
                Code::NonRedundantData,
                Some(*index),
                None,
                format!(
                    "data vdev {index} is a single disk: if it fails the whole pool is lost, and ZFS can \
                     detect but not repair corruption on it"
                ),
            );
        }
    }
    for (index, vdev) in layout.vdevs.iter().enumerate() {
        let survives = vdev.failures_tolerated();
        match vdev.class {
            VdevClass::Special if survives < weakest => f.warn(
                Code::HelperLessRedundant,
                Some(index),
                None,
                format!(
                    "special vdev {index} survives {} but the data vdevs survive {}; losing the special vdev \
                     loses the whole pool",
                    plural(survives, "failure"),
                    plural(weakest, "failure")
                ),
            ),
            VdevClass::Log if survives == 0 => f.warn(
                Code::SingleLog,
                Some(index),
                None,
                format!(
                    "log vdev {index} is a single disk; if it fails while the host crashes, the last few \
                     seconds of synchronous writes are lost"
                ),
            ),
            VdevClass::Log if survives < weakest => f.warn(
                Code::HelperLessRedundant,
                Some(index),
                None,
                format!(
                    "log vdev {index} survives {} but the data vdevs survive {}",
                    plural(survives, "failure"),
                    plural(weakest, "failure")
                ),
            ),
            _ => {}
        }
    }
}
