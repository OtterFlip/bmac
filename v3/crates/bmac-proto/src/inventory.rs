// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

//! What `bmac-inventory` reports about a machine booted from the discovery
//! ISO (`DEPLOYMENT.md`, "Discovery ISO"), and its one writing command,
//! `clear-zfs-labels`. The Deployer reads both over pinned SSH as JSON.

use bmac_core::disk::DiskIdentity;
use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use ts_rs::TS;

/// Bumped when a field changes meaning; the Deployer refuses a newer one.
pub const INVENTORY_VERSION: u32 = 1;

/// How long a NIC may take to get carrier (`DEPLOYMENT.md`, "NicLink-wait
/// rules"): `attempts` reads, `interval_seconds` apart.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct LinkWait {
    pub attempts: u32,
    pub interval_seconds: u32,
}

impl Default for LinkWait {
    fn default() -> LinkWait {
        LinkWait {
            attempts: 10,
            interval_seconds: 3,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct Inventory {
    pub version: u32,
    pub taken_ms: i64,
    pub system: SystemInfo,
    pub cpu: CpuInfo,
    pub memory_bytes: Option<u64>,
    pub firmware: Firmware,
    pub disks: Vec<InventoryDisk>,
    pub nics: Vec<InventoryNic>,
    /// The wait applied to every NIC before its link was read; `None` when
    /// links were read once without waiting.
    pub link_wait: Option<LinkWait>,
}

/// From DMI. The serial is the Dell service tag on Dell hardware.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct SystemInfo {
    pub vendor: Option<String>,
    pub product: Option<String>,
    pub serial: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct CpuInfo {
    pub model: Option<String>,
    pub sockets: u32,
    pub cores: u32,
    pub threads: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
pub enum FirmwareMode {
    Uefi,
    Bios,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
pub enum SecureBoot {
    Enabled,
    Disabled,
    /// UEFI, but the variable couldn't be read.
    Unknown,
    /// BIOS boot has no Secure Boot.
    NotApplicable,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct Firmware {
    pub mode: FirmwareMode,
    pub secure_boot: SecureBoot,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
pub enum PartitionTable {
    Gpt,
    Mbr,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct LuksHeader {
    pub version: u16,
    pub uuid: Option<String>,
}

/// A pool's state as its label records it (`zpool import` reads the same).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
pub enum ZfsPoolState {
    Active,
    Exported,
    Destroyed,
    Spare,
    L2cache,
    Uninitialized,
    Unavail,
    PotentiallyActive,
    Unknown,
}

/// A ZFS vdev label found on a whole disk or a partition.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct ZfsLabel {
    /// The labelled device, such as `/dev/sdb3`.
    pub device: String,
    /// Spares and cache devices carry no pool name.
    pub pool: Option<String>,
    /// Decimal; too large for a JSON number.
    pub pool_guid: Option<String>,
    pub vdev_guid: Option<String>,
    pub state: ZfsPoolState,
    pub txg: Option<u64>,
    /// The host that last used the pool.
    pub hostname: Option<String>,
    /// How many of the four label copies could be read.
    pub copies: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct DiskPartition {
    pub name: String,
    pub path: String,
    pub number: u32,
    pub start_bytes: u64,
    pub size_bytes: u64,
    pub luks: Option<LuksHeader>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct InventoryDisk {
    /// The kernel name, such as `nvme0n1` or `sda`.
    pub name: String,
    pub path: String,
    /// How BMAC recognizes the disk (`STORAGE.md`, "Disks"); `None` when it
    /// reports neither a WWN nor a serial number.
    #[ts(as = "Option<String>")]
    pub identity: Option<DiskIdentity>,
    pub model: Option<String>,
    pub serial: Option<String>,
    /// As `lsblk` shows it, lowercase.
    pub wwn: Option<String>,
    pub size_bytes: u64,
    pub logical_sector_bytes: u32,
    pub physical_sector_bytes: u32,
    pub rotational: bool,
    pub removable: bool,
    /// `nvme`, `sata`, `sas`, `usb`, `virtio`, ...
    pub transport: Option<String>,
    /// The disk's names under `/dev/disk/by-id/` from udev, in udev's
    /// order. Pool layouts name members this way.
    pub by_id: Vec<String>,
    /// udev's identity properties (`ID_SERIAL`, `ID_SERIAL_SHORT`,
    /// `ID_SCSI_SERIAL`, `ID_WWN`, `ID_WWN_WITH_EXTENSION`) as udev reports
    /// them. The installer's disk filters match these, so the answer file
    /// is built from the same values.
    pub udev_identity: BTreeMap<String, String>,
    pub partition_table: Option<PartitionTable>,
    pub partitions: Vec<DiskPartition>,
    /// A LUKS header on the whole disk; partitions carry their own.
    pub luks: Option<LuksHeader>,
    /// Labels on the whole disk and on each partition.
    pub zfs_labels: Vec<ZfsLabel>,
    /// Why BMAC can't use the disk safely, such as a serial shared with
    /// another disk.
    pub problems: Vec<String>,
}

/// The link as read after the wait.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum NicLink {
    Up {
        /// Megabits per second; `None` when the driver doesn't say.
        speed_mbps: Option<u32>,
        duplex: Option<String>,
        /// Seconds from the first look until carrier appeared.
        waited_seconds: u32,
    },
    NoCarrier {
        waited_seconds: u32,
    },
    /// The NIC couldn't be brought up to read it.
    Error {
        message: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct InventoryNic {
    pub name: String,
    /// Lowercase, colon-separated, as the OS reports it; what the Proxmox
    /// installer's `ID_NET_NAME_MAC` filter matches.
    pub mac: String,
    pub driver: Option<String>,
    /// `pci`, `usb`, ...
    pub bus: Option<String>,
    pub link: NicLink,
}

/// One disk whose old ZFS labels the operator agreed to clear, as the
/// Deployer saw it in the inventory.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct ClearTarget {
    pub serial: String,
    pub wwn: Option<String>,
    pub pool_guid: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct ClearRequest {
    pub disks: Vec<ClearTarget>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct ClearReport {
    /// Devices whose labels were cleared and are gone on a second read.
    pub cleared: Vec<String>,
    /// Devices that still carry a label after clearing.
    pub survived: Vec<String>,
    /// Why nothing was touched: a disk that didn't match what the operator
    /// confirmed. Empty when the clearing went ahead.
    pub refused: Vec<String>,
    /// `zpool labelclear` failures, by device.
    pub errors: Vec<String>,
}

impl ClearReport {
    pub fn succeeded(&self) -> bool {
        self.refused.is_empty() && self.survived.is_empty() && self.errors.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn link_is_tagged_by_state() {
        let up = NicLink::Up {
            speed_mbps: Some(10_000),
            duplex: Some("full".into()),
            waited_seconds: 9,
        };
        let json = serde_json::to_value(&up).unwrap();
        assert_eq!(json["state"], "up");
        assert_eq!(json["speed_mbps"], 10_000);
        let none: NicLink =
            serde_json::from_str(r#"{"state":"no_carrier","waited_seconds":27}"#).unwrap();
        assert_eq!(none, NicLink::NoCarrier { waited_seconds: 27 });
    }

    #[test]
    fn disk_identity_is_written_as_a_string() {
        let identity: DiskIdentity = "wwn:0x5000c500a1b2c3d4".parse().unwrap();
        assert_eq!(
            serde_json::to_value(Some(identity)).unwrap(),
            "wwn:0x5000c500a1b2c3d4"
        );
    }

    #[test]
    fn a_report_succeeds_only_when_everything_cleared() {
        let mut report = ClearReport {
            cleared: vec!["/dev/sdb3".into()],
            ..ClearReport::default()
        };
        assert!(report.succeeded());
        report.survived.push("/dev/sdc3".into());
        assert!(!report.succeeded());
    }
}
