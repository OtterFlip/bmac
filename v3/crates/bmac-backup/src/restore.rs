// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

//! Restore planning (`BACKUP.md`, "Restore"): the cheapest method for each
//! volume, the names the restored volumes take, and the configuration of
//! the VM they are attached to. Pure; `bmacd`'s restore jobs bring the
//! facts and carry the plan out.

use std::collections::{BTreeMap, BTreeSet};

use bmac_core::SnapshotGuid;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::manifest::{Manifest, Volume};
use crate::records::{Lineage, RECORD_SCHEMA};

/// Where a restore puts the PIT.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    /// A new VM beside production, isolated.
    NewVm,
    /// The VM's disks replaced by restored ones; the current ones are set
    /// aside with their snapshots and bookmarks.
    IntoExisting,
    /// The VM's disks rolled back to the PIT's snapshots on the cluster.
    Rollback,
    /// A VM restored as new made production: its disks replace the
    /// production VM's, which keeps its identity.
    Promotion,
    /// A restore test: an isolated scratch VM, booted, checked, and
    /// destroyed.
    Test,
    /// A VM of a lost cluster, restored on the cluster rebuilt in its place.
    Recovery,
}

/// A snapshot of a volume on the restoring host, by GUID.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalSnapshot {
    /// `dataset@name`.
    pub name: String,
    pub guid: SnapshotGuid,
}

/// An earlier PIT of the same VM that the appliance has published: the
/// volume's GUID in it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Base {
    pub pit_uuid: Uuid,
    pub taken_s: i64,
    pub guid: SnapshotGuid,
}

/// How one volume is brought to the PIT, cheapest first.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "method", rename_all = "snake_case")]
pub enum Method {
    /// 1. The PIT's own snapshot is on the cluster: clone it, or roll back
    ///    to it. Nothing crosses the network.
    Local { snapshot: String },
    /// 2. An older PIT's snapshot is on the cluster and on the appliance:
    ///    the appliance's incremental from it, received as a clone of it.
    Incremental {
        base: String,
        base_guid: SnapshotGuid,
        base_pit: Uuid,
    },
    /// 3. A full stream from the appliance.
    Full,
}

impl Method {
    pub fn number(&self) -> u8 {
        match self {
            Method::Local { .. } => 1,
            Method::Incremental { .. } => 2,
            Method::Full => 3,
        }
    }

    /// Whether the appliance sends anything.
    pub fn from_appliance(&self) -> bool {
        !matches!(self, Method::Local { .. })
    }
}

fn pool_of(name: &str) -> &str {
    name.split(['/', '@']).next().unwrap_or_default()
}

/// The cheapest method for `volume`, restored into `pool`: a clone or a
/// receive with an origin stays in the snapshot's pool. `bases` are the
/// appliance's earlier PITs of the VM, any order.
pub fn choose(volume: &Volume, pool: &str, local: &[LocalSnapshot], bases: &[Base]) -> Method {
    let here: Vec<&LocalSnapshot> = local.iter().filter(|s| pool_of(&s.name) == pool).collect();
    if let Some(s) = here.iter().find(|s| s.guid == volume.snapshot_guid) {
        return Method::Local {
            snapshot: s.name.clone(),
        };
    }
    let mut bases: Vec<&Base> = bases.iter().collect();
    bases.sort_by_key(|b| std::cmp::Reverse((b.taken_s, b.pit_uuid)));
    for base in bases {
        if let Some(s) = here.iter().find(|s| s.guid == base.guid) {
            return Method::Incremental {
                base: s.name.clone(),
                base_guid: base.guid,
                base_pit: base.pit_uuid,
            };
        }
    }
    Method::Full
}

/// The staging dataset a volume is received or cloned into, out of
/// Proxmox's sight until it is checked.
pub fn staging(pool: &str, job: &str, volume_uuid: &str) -> String {
    format!("{pool}/bmac-restore/{job}/{volume_uuid}")
}

/// Where a replaced disk is set aside, with every snapshot and bookmark.
pub fn replaced(pool: &str, vmid: u32, volume_uuid: &str, at_s: i64) -> String {
    format!("{pool}/bmac-replaced/{vmid}/{volume_uuid}-{at_s}")
}

/// The next `vm-<vmid>-disk-<n>` that none of `taken` uses.
pub fn disk_name(vmid: u32, taken: &BTreeSet<String>) -> String {
    (0..)
        .map(|n| format!("vm-{vmid}-disk-{n}"))
        .find(|name| !taken.contains(name))
        .expect("an unused disk name")
}

/// Configuration keys that name another VM's state, never carried over.
const DROPPED: [&str; 7] = [
    "parent", "lock", "digest", "snaptime", "vmstate", "meta", "unused",
];

/// The VM a restore creates, as `qm create` and `qm set` options.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VmConfig {
    /// Options for `qm create`, without disks.
    pub options: Vec<(String, String)>,
    /// Each restored volume's slot and value, for `qm set` after it is in
    /// place.
    pub disks: Vec<(String, String)>,
    /// What was left out, for the operator.
    pub left_out: Vec<String>,
}

/// One option list (`a,b=c,d`) with `key=` options replaced or removed.
fn with_options(value: &str, drop: &[&str], set: &[(&str, &str)]) -> String {
    let mut parts: Vec<String> = value
        .split(',')
        .filter(|p| !drop.iter().any(|d| p.split('=').next() == Some(d)))
        .map(str::to_string)
        .collect();
    for (key, val) in set {
        parts.retain(|p| p.split('=').next() != Some(key));
        parts.push(format!("{key}={val}"));
    }
    parts.join(",")
}

fn is_bus_slot(key: &str) -> bool {
    ["scsi", "virtio", "sata", "ide"].iter().any(|bus| {
        key.strip_prefix(bus)
            .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
    })
}

fn numbered(key: &str, prefix: &str) -> bool {
    key.strip_prefix(prefix)
        .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
}

/// Translates the PIT's configuration field by field for a new VM:
/// `volumes` gives each restored slot its new volume ID. A new VM gets new
/// MAC addresses, a new SMBIOS UUID and generation ID, and doesn't start
/// on boot; `isolate` also holds every network link down. Host devices and
/// hook scripts are left out, since they belong to the host the PIT came
/// from.
pub fn translate(
    manifest: &Manifest,
    name: &str,
    volumes: &BTreeMap<String, String>,
    isolate: bool,
    smbios_uuid: Uuid,
) -> Result<VmConfig, String> {
    let mut out = VmConfig {
        options: vec![("name".into(), name.into())],
        disks: Vec::new(),
        left_out: Vec::new(),
    };
    for volume in &manifest.volumes {
        if !volumes.contains_key(&volume.slot) {
            return Err(format!("{} has no restored volume", volume.slot));
        }
    }
    for (key, value) in &manifest.config {
        let key = key.as_str();
        if key == "name" || DROPPED.iter().any(|d| key == *d || numbered(key, d)) {
            continue;
        }
        if let Some(id) = volumes.get(key) {
            let rest = value.split_once(',').map(|(_, r)| r);
            let value = match rest {
                Some(rest) => format!("{id},{rest}"),
                None => id.clone(),
            };
            out.disks.push((key.to_string(), value));
            continue;
        }
        if is_bus_slot(key) || key == "efidisk0" || key == "tpmstate0" {
            if value.split(',').any(|p| p.trim() == "media=cdrom") {
                out.options.push((key.into(), "none,media=cdrom".into()));
            } else {
                out.left_out
                    .push(format!("{key} ({value}) isn't in the PIT"));
            }
            continue;
        }
        if numbered(key, "hostpci") || (numbered(key, "usb") && value.contains("host=")) {
            out.left_out
                .push(format!("{key} ({value}) is a device of the original host"));
            continue;
        }
        if key == "hookscript" {
            out.left_out.push(format!("hookscript {value}"));
            continue;
        }
        let value = if numbered(key, "net") {
            let mut parts = value.splitn(2, ',');
            let model = parts.next().unwrap_or_default();
            let model = model.split_once('=').map_or(model, |(m, _)| m);
            let rest = parts.next().unwrap_or_default();
            let joined = if rest.is_empty() {
                model.to_string()
            } else {
                format!("{model},{rest}")
            };
            let link: &[(&str, &str)] = if isolate { &[("link_down", "1")] } else { &[] };
            with_options(&joined, &["macaddr"], link)
        } else if key == "smbios1" {
            with_options(value, &[], &[("uuid", &smbios_uuid.to_string())])
        } else if key == "vmgenid" {
            "1".to_string()
        } else if key == "onboot" {
            "0".to_string()
        } else if key == "startup" && isolate {
            continue;
        } else {
            value.clone()
        };
        out.options.push((key.to_string(), value));
    }
    if !out.options.iter().any(|(k, _)| k == "onboot") {
        out.options.push(("onboot".into(), "0".into()));
    }
    Ok(out)
}

/// `412 GiB`, `3.5 MiB`: bytes in binary units for the wizard.
pub fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 6] = ["B", "KiB", "MiB", "GiB", "TiB", "PiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 || value >= 100.0 {
        format!("{value:.0} {}", UNITS[unit])
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

/// `about 1 h 50 min` for `bytes` at `bytes_per_s`; `None` without a rate.
pub fn estimate(bytes: u64, bytes_per_s: Option<f64>) -> Option<String> {
    let rate = bytes_per_s.filter(|r| *r > 0.0)?;
    let seconds = (bytes as f64 / rate).ceil() as u64;
    Some(if seconds < 60 {
        "under a minute".to_string()
    } else if seconds < 3600 {
        format!("about {} min", seconds.div_ceil(60))
    } else {
        let minutes = seconds.div_ceil(60);
        format!("about {} h {} min", minutes / 60, minutes % 60)
    })
}

/// The lineage of a VM whose disks were restored and are backed up from
/// now on (`BACKUP.md`, "Backups continue from the restored PIT"): a new
/// branch whose first PIT's parent is the restored one, with each restored
/// dataset keeping its volume's ID. `datasets` gives each slot's dataset.
pub fn continued(
    vm_uuid: Uuid,
    restored: &Manifest,
    datasets: &BTreeMap<String, String>,
) -> Result<Lineage, String> {
    let pit: Uuid = restored
        .pit_uuid
        .parse()
        .map_err(|_| "the manifest's PIT ID isn't a UUID".to_string())?;
    let mut volumes = BTreeMap::new();
    for volume in &restored.volumes {
        let dataset = datasets
            .get(&volume.slot)
            .ok_or_else(|| format!("{} has no restored dataset", volume.slot))?;
        let id: Uuid = volume
            .volume_uuid
            .parse()
            .map_err(|_| format!("{}'s volume ID isn't a UUID", volume.slot))?;
        volumes.insert(dataset.clone(), id);
    }
    Ok(Lineage {
        schema_version: RECORD_SCHEMA,
        record_type: String::new(),
        revision: 0,
        vm_uuid,
        branch_uuid: Uuid::new_v4(),
        head: Some(pit),
        volumes,
        last_attempt: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::VolumeRole;

    fn volume(slot: &str, guid: u64) -> Volume {
        Volume {
            volume_uuid: Uuid::from_u128(guid.into()).to_string(),
            slot: slot.into(),
            role: VolumeRole::Disk,
            dataset: format!("rpool/data/vm-100-{slot}"),
            snapshot_guid: SnapshotGuid::new(guid),
            volsize: 1 << 30,
            volblocksize: 16384,
            boot: true,
        }
    }

    fn snap(name: &str, guid: u64) -> LocalSnapshot {
        LocalSnapshot {
            name: name.into(),
            guid: SnapshotGuid::new(guid),
        }
    }

    fn base(n: u128, taken_s: i64, guid: u64) -> Base {
        Base {
            pit_uuid: Uuid::from_u128(n),
            taken_s,
            guid: SnapshotGuid::new(guid),
        }
    }

    #[test]
    fn the_cheapest_method_that_the_snapshots_on_hand_allow() {
        let v = volume("scsi0", 30);
        let local = [
            snap("rpool/data/vm-100-disk-0@p1", 10),
            snap("rpool/data/vm-100-disk-0@p2", 20),
        ];
        let bases = [base(1, 100, 10), base(2, 200, 20)];
        assert_eq!(
            choose(&v, "rpool", &local, &bases),
            Method::Incremental {
                base: "rpool/data/vm-100-disk-0@p2".into(),
                base_guid: SnapshotGuid::new(20),
                base_pit: Uuid::from_u128(2),
            }
        );
        let mut with_pit = local.to_vec();
        with_pit.push(snap("rpool/data/vm-100-disk-0@p3", 30));
        assert_eq!(
            choose(&v, "rpool", &with_pit, &bases),
            Method::Local {
                snapshot: "rpool/data/vm-100-disk-0@p3".into()
            }
        );
        assert_eq!(
            choose(&v, "tank", &with_pit, &bases),
            Method::Full,
            "a clone stays in its pool"
        );
        assert_eq!(
            choose(&v, "rpool", &local, &[base(9, 50, 99)]),
            Method::Full
        );
        assert_eq!(choose(&v, "rpool", &[], &bases).number(), 3);
        assert!(
            !Method::Local {
                snapshot: String::new()
            }
            .from_appliance()
        );
    }

    #[test]
    fn names_for_staging_set_aside_and_new_disks() {
        assert_eq!(staging("rpool", "j1", "v1"), "rpool/bmac-restore/j1/v1");
        assert_eq!(
            replaced("rpool", 100, "v1", 7),
            "rpool/bmac-replaced/100/v1-7"
        );
        let taken: BTreeSet<String> = ["vm-120-disk-0".to_string(), "vm-120-disk-2".into()].into();
        assert_eq!(disk_name(120, &taken), "vm-120-disk-1");
    }

    fn manifest(config: &[(&str, &str)], volumes: Vec<Volume>) -> Manifest {
        Manifest {
            schema_version: crate::manifest::SCHEMA_VERSION,
            cluster_uuid: "c".into(),
            vm_uuid: Uuid::from_u128(7).to_string(),
            vmid: 100,
            pit_uuid: Uuid::from_u128(8).to_string(),
            branch_uuid: Uuid::from_u128(9).to_string(),
            parent_pit_uuid: None,
            snapshot: crate::PitName::new(1_700_000_000, 1),
            taken_at_s: 1_700_000_000,
            consistency: crate::manifest::Consistency::Crash,
            evidence: serde_json::Value::Null,
            multi_pool: false,
            versions: crate::manifest::Versions {
                proxmox: "pve".into(),
                openzfs: "zfs".into(),
                bmac: "b".into(),
            },
            config_text: String::new(),
            config: config
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            config_sha256: String::new(),
            volumes,
            firmware: crate::manifest::Firmware {
                bios: "ovmf".into(),
                machine: None,
                secure_boot: false,
            },
            registry: serde_json::Value::Null,
            prerequisites: Vec::new(),
            sha256: String::new(),
        }
    }

    #[test]
    fn a_new_vm_gets_new_identities_and_no_network_when_isolated() {
        let m = manifest(
            &[
                ("name", "web"),
                ("scsi0", "local-zfs:vm-100-disk-0,iothread=1,size=32G"),
                ("efidisk0", "local-zfs:vm-100-disk-1,efitype=4m,size=1M"),
                ("ide2", "local:iso/x.iso,media=cdrom"),
                ("unused0", "local-zfs:vm-100-disk-9"),
                ("net0", "virtio=AA:BB:CC:DD:EE:FF,bridge=vmbr1,firewall=1"),
                (
                    "smbios1",
                    "uuid=11111111-1111-1111-1111-111111111111,manufacturer=x",
                ),
                ("vmgenid", "22222222-2222-2222-2222-222222222222"),
                ("onboot", "1"),
                ("startup", "order=1"),
                ("hostpci0", "0000:01:00.0"),
                ("parent", "snap1"),
                ("memory", "4096"),
            ],
            vec![
                volume("scsi0", 1),
                Volume {
                    role: VolumeRole::EfiVars,
                    ..volume("efidisk0", 2)
                },
            ],
        );
        let volumes: BTreeMap<String, String> = [
            ("scsi0".to_string(), "local-zfs:vm-120-disk-0".to_string()),
            (
                "efidisk0".to_string(),
                "local-zfs:vm-120-disk-1".to_string(),
            ),
        ]
        .into();
        let smbios = Uuid::from_u128(5);
        let vm = translate(&m, "web-restored", &volumes, true, smbios).unwrap();
        let options: BTreeMap<&str, &str> = vm
            .options
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();
        assert_eq!(options["name"], "web-restored");
        assert_eq!(
            options["net0"],
            "virtio,bridge=vmbr1,firewall=1,link_down=1"
        );
        assert_eq!(options["smbios1"], format!("manufacturer=x,uuid={smbios}"));
        assert_eq!(options["vmgenid"], "1");
        assert_eq!(options["onboot"], "0");
        assert_eq!(options["ide2"], "none,media=cdrom");
        assert_eq!(options["memory"], "4096");
        for gone in [
            "startup", "hostpci0", "parent", "unused0", "scsi0", "efidisk0",
        ] {
            assert!(!options.contains_key(gone), "{gone}");
        }
        assert_eq!(
            vm.disks,
            vec![
                (
                    "efidisk0".to_string(),
                    "local-zfs:vm-120-disk-1,efitype=4m,size=1M".to_string()
                ),
                (
                    "scsi0".to_string(),
                    "local-zfs:vm-120-disk-0,iothread=1,size=32G".to_string()
                ),
            ]
        );
        assert!(vm.left_out.iter().any(|l| l.contains("hostpci0")));

        let connected = translate(&m, "web-restored", &volumes, false, smbios).unwrap();
        assert!(
            connected
                .options
                .contains(&("net0".into(), "virtio,bridge=vmbr1,firewall=1".into()))
        );
        assert!(
            connected
                .options
                .contains(&("startup".into(), "order=1".into()))
        );
        let missing: BTreeMap<String, String> = [volumes
            .iter()
            .next()
            .map(|(k, v)| (k.clone(), v.clone()))
            .unwrap()]
        .into();
        assert!(translate(&m, "x", &missing, true, smbios).is_err());
    }

    #[test]
    fn sizes_and_times_in_human_units() {
        assert_eq!(human_bytes(512), "512 B");
        assert_eq!(human_bytes(3 * 1024 * 1024 + 512 * 1024), "3.5 MiB");
        assert_eq!(human_bytes(412 << 30), "412 GiB");
        assert_eq!(estimate(1, None), None);
        assert_eq!(estimate(10, Some(100.0)).as_deref(), Some("under a minute"));
        assert_eq!(estimate(600, Some(1.0)).as_deref(), Some("about 10 min"));
        assert_eq!(
            estimate(6600, Some(1.0)).as_deref(),
            Some("about 1 h 50 min")
        );
    }

    #[test]
    fn a_restored_vm_continues_on_a_new_branch_from_the_pit() {
        let m = manifest(&[], vec![volume("scsi0", 1)]);
        let datasets: BTreeMap<String, String> =
            [("scsi0".to_string(), "rpool/data/vm-120-disk-0".to_string())].into();
        let lineage = continued(Uuid::from_u128(7), &m, &datasets).unwrap();
        assert_eq!(lineage.head, Some(Uuid::from_u128(8)));
        assert_ne!(lineage.branch_uuid, Uuid::from_u128(9));
        assert_eq!(
            lineage.volumes["rpool/data/vm-120-disk-0"],
            Uuid::from_u128(1)
        );
        assert!(continued(Uuid::from_u128(7), &m, &BTreeMap::new()).is_err());
    }
}
