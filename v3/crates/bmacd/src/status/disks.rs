// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

//! Pool vdevs and the disks outside every pool, from a host's storage
//! layout, as v2's `disk_inventory.py` summarizes them (`summarize_vdevs`
//! 104–149, `classify_disk` 160–230). The statuses that come from the host
//! storage journal (awaiting finalization, evacuating, pending replacement
//! or addition) join with that journal in M8.2.

use std::collections::BTreeMap;

use bmac_adapters::layout::{DiskRow, Layout, Member};
use bmac_proto::status::{
    Disk, DiskContents, DiskStatus, Encryption, Vdev, VdevMember, VdevStatus,
};

fn encryption(members: &[Member]) -> Encryption {
    let resolved: Vec<&Member> = members
        .iter()
        .filter(|m| !m.device.is_empty() && m.disk.is_some())
        .collect();
    if resolved.is_empty() {
        Encryption::Unknown
    } else if resolved.iter().all(|m| m.luks) {
        Encryption::Luks
    } else if !resolved.iter().any(|m| m.luks) {
        Encryption::None
    } else {
        Encryption::Mixed
    }
}

/// Every imported pool's top-level data vdevs, with rpool's extra details.
pub fn vdevs(layout: &Layout) -> Vec<Vdev> {
    let rpool = &layout.pool.name;
    let details: BTreeMap<&str, _> = layout.vdevs.iter().map(|v| (v.name.as_str(), v)).collect();
    layout
        .imported_pool_vdevs
        .iter()
        .map(|vdev| {
            let extra = (&vdev.pool == rpool)
                .then(|| details.get(vdev.name.as_str()).copied())
                .flatten();
            let status = if extra.is_some_and(|e| e.removing) {
                VdevStatus::Evacuating
            } else if extra.is_some_and(|e| e.replacing) {
                VdevStatus::Resilvering
            } else {
                match vdev.state.as_deref() {
                    Some("ONLINE") => VdevStatus::Online,
                    Some("DEGRADED") => VdevStatus::Degraded,
                    _ => VdevStatus::Faulted,
                }
            };
            Vdev {
                pool: vdev.pool.clone(),
                name: vdev.name.clone(),
                kind: vdev.kind.clone(),
                state: vdev.state.clone(),
                status,
                encryption: encryption(&vdev.members),
                holds_esp: extra.is_some_and(|e| e.holds_esp),
                size: extra.and_then(|e| e.size),
                allocated: extra.and_then(|e| e.allocated),
                free: extra.and_then(|e| e.free),
                members: vdev
                    .members
                    .iter()
                    .map(|m| VdevMember {
                        path: m.path.clone(),
                        state: m.state.clone(),
                        disk: m.disk.clone(),
                        serial: m.serial.clone(),
                        model: m.model.clone(),
                        size: m.disk_size,
                        luks: m.luks,
                        mapper: m
                            .mapper
                            .as_deref()
                            .and_then(|p| p.rsplit('/').next())
                            .filter(|s| !s.is_empty())
                            .map(str::to_string),
                        missing: m.disk.is_none(),
                    })
                    .collect(),
            }
        })
        .collect()
}

fn contents(disk: &DiskRow) -> DiskContents {
    if disk.mounted {
        DiskContents::Mounted
    } else if disk.partitions_or_holders > 0 || disk.fstype.is_some() {
        DiskContents::PartitionsOrSignatures
    } else {
        DiskContents::Blank
    }
}

fn sentence(reasons: &[String]) -> String {
    let text = reasons.join("; ");
    let mut chars = text.chars();
    match chars.next() {
        Some(first) => format!("{}{}.", first.to_uppercase(), chars.as_str()),
        None => String::new(),
    }
}

/// The physical disks in no pool, each with a status and what to do.
pub fn disks(layout: &Layout) -> Vec<Disk> {
    let rpool = &layout.pool.name;
    let open_mappers: BTreeMap<&str, &str> = layout
        .luks_mappings
        .iter()
        .filter(|m| !m.in_pool)
        .filter_map(|m| Some((m.serial.as_deref()?, m.mapper.as_str())))
        .collect();
    let other_pools: BTreeMap<&str, String> = layout
        .imported_pool_vdevs
        .iter()
        .filter(|v| &v.pool != rpool)
        .flat_map(|v| {
            v.members
                .iter()
                .filter_map(move |m| Some((m.disk.as_deref()?, format!("{} ({})", v.pool, v.name))))
        })
        .collect();
    let mut rows: Vec<(&DiskRow, bool)> = layout
        .unassigned_disks
        .iter()
        .map(|d| (d, false))
        .chain(layout.esp_only_disks.iter().map(|d| (d, true)))
        .collect();
    rows.sort_by(|a, b| a.0.disk.cmp(&b.0.disk));
    rows.into_iter()
        .map(|(disk, esp_only)| {
            let serial = disk.serial.as_deref().filter(|s| !s.is_empty());
            let (status, detail) = if esp_only {
                (
                    DiskStatus::PendingReplacement,
                    "Holds a registered boot ESP but has not joined rpool. \
                     Rerun Replace a failed disk to finish."
                        .to_string(),
                )
            } else if let Some(mapper) = serial.and_then(|s| open_mappers.get(s)) {
                (
                    DiskStatus::PendingReplacement,
                    format!(
                        "LUKS mapping {mapper} is open but not in rpool; an add or replace was interrupted."
                    ),
                )
            } else if !disk.in_use_reasons.is_empty() {
                let reasons: Vec<String> = disk
                    .in_use_reasons
                    .iter()
                    .map(|reason| match other_pools.get(disk.disk.as_str()) {
                        Some(pool) if reason == "member of an imported ZFS pool" => {
                            format!("member of pool {pool}")
                        }
                        _ => reason.clone(),
                    })
                    .collect();
                (DiskStatus::InUse, sentence(&reasons))
            } else if serial.is_none() {
                (
                    DiskStatus::NoSerial,
                    "Reports no serial number, so the disk workflows cannot select it safely."
                        .to_string(),
                )
            } else {
                let detail = if contents(disk) == DiskContents::Blank {
                    "Blank."
                } else {
                    "Has old partitions or signatures; they are erased when the disk is used."
                };
                (
                    DiskStatus::Available,
                    format!("{detail} Safe to pull, or to use in a new vdev or replacement."),
                )
            };
            Disk {
                disk: disk.disk.clone(),
                serial: disk.serial.clone(),
                model: disk.model.clone(),
                size: disk.size,
                tran: disk.tran.clone(),
                contents: contents(disk),
                status,
                label: status.label().to_string(),
                detail,
                in_use_reasons: disk.in_use_reasons.clone(),
                removable: disk.in_use_reasons.is_empty()
                    && !matches!(status, DiskStatus::AwaitingFinalization | DiskStatus::Evacuating),
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use bmac_adapters::layout::{
        CrypttabEntry, ImportedVdev, LayoutVdev, LuksMapping, PoolSummary,
    };

    fn member(disk: Option<&str>, luks: bool) -> Member {
        Member {
            path: format!("/dev/{}", disk.unwrap_or("gone")),
            state: Some("ONLINE".into()),
            device: disk.map(|d| format!("/dev/{d}")).unwrap_or_default(),
            luks,
            mapper: luks.then(|| "/dev/mapper/rpool-a".into()),
            backing: None,
            disk: disk.map(str::to_string),
            serial: disk.map(|d| format!("SN-{d}")),
            model: None,
            disk_size: Some(100),
            member_size: Some(90),
        }
    }

    fn disk(name: &str, serial: Option<&str>, reasons: &[&str], fstype: Option<&str>) -> DiskRow {
        DiskRow {
            disk: name.into(),
            serial: serial.map(str::to_string),
            model: Some("QEMU".into()),
            size: Some(1 << 30),
            tran: None,
            fstype: fstype.map(str::to_string),
            partitions_or_holders: 0,
            mounted: false,
            in_use_reasons: reasons.iter().map(|r| r.to_string()).collect(),
        }
    }

    fn layout() -> Layout {
        Layout {
            schema_version: 1,
            host: "mox1".into(),
            collected_at: 0,
            pool: PoolSummary {
                name: "rpool".into(),
                state: Some("ONLINE".into()),
                health: Some("ONLINE".into()),
                size: None,
                allocated: None,
                free: None,
                capacity_percent: None,
                fragmentation_percent: None,
                dataset_available: None,
                dataset_used: None,
                status: None,
                scan: None,
                remove: None,
                removal_in_progress: false,
                errors: None,
            },
            vdevs: vec![LayoutVdev {
                name: "mirror-0".into(),
                kind: "mirror".into(),
                state: Some("ONLINE".into()),
                removing: false,
                replacing: true,
                size: Some(1000),
                allocated: Some(400),
                free: Some(600),
                holds_esp: true,
                luks: true,
                members: vec![],
            }],
            auxiliary_vdevs: BTreeMap::new(),
            imported_pool_vdevs: vec![
                ImportedVdev {
                    pool: "rpool".into(),
                    name: "mirror-0".into(),
                    kind: "mirror".into(),
                    state: Some("ONLINE".into()),
                    members: vec![member(Some("sda"), true), member(None, true)],
                },
                ImportedVdev {
                    pool: "tank".into(),
                    name: "sdz".into(),
                    kind: "disk".into(),
                    state: Some("FAULTED".into()),
                    members: vec![member(Some("sdz"), false)],
                },
            ],
            esp_partitions: vec![],
            all_disks: vec![],
            physically_removable_disks: vec![],
            unassigned_disks: vec![
                disk("sdc", Some("SN-c"), &[], None),
                disk("sdb", None, &[], None),
                disk(
                    "sdd",
                    Some("SN-d"),
                    &["member of an imported ZFS pool"],
                    None,
                ),
                disk("sde", Some("SN-e"), &[], Some("ext4")),
                disk("sdf", Some("SN-f"), &[], None),
            ],
            esp_only_disks: vec![disk("sdg", Some("SN-g"), &[], None)],
            luks_mappings: vec![LuksMapping {
                mapper: "rpool-f".into(),
                backing: None,
                disk: Some("sdf".into()),
                serial: Some("SN-f".into()),
                size: None,
                in_pool: false,
            }],
            crypttab: vec![],
            volumes: vec![],
        }
    }

    #[test]
    fn vdevs_carry_status_encryption_and_rpool_details() {
        let vdevs = vdevs(&layout());
        assert_eq!(vdevs[0].status, VdevStatus::Resilvering);
        assert_eq!(
            vdevs[0].encryption,
            Encryption::Luks,
            "an unresolved member doesn't count"
        );
        assert!(vdevs[0].holds_esp);
        assert_eq!(vdevs[0].allocated, Some(400));
        assert_eq!(vdevs[0].members[0].mapper.as_deref(), Some("rpool-a"));
        assert!(vdevs[0].members[1].missing);
        assert_eq!(vdevs[1].status, VdevStatus::Faulted);
        assert_eq!(vdevs[1].encryption, Encryption::None);
        assert_eq!(vdevs[1].size, None, "only rpool has the details");
    }

    #[test]
    fn disks_outside_pools_are_classified() {
        let mut layout = layout();
        layout.imported_pool_vdevs[1].members[0].disk = Some("sdd".into());
        let disks = disks(&layout);
        let by_name: BTreeMap<&str, &Disk> = disks.iter().map(|d| (d.disk.as_str(), d)).collect();
        assert_eq!(
            disks.iter().map(|d| d.disk.as_str()).collect::<Vec<_>>(),
            ["sdb", "sdc", "sdd", "sde", "sdf", "sdg"]
        );
        assert_eq!(by_name["sdb"].status, DiskStatus::NoSerial);
        assert_eq!(by_name["sdc"].status, DiskStatus::Available);
        assert_eq!(
            by_name["sdc"].detail,
            "Blank. Safe to pull, or to use in a new vdev or replacement."
        );
        assert!(by_name["sdc"].removable);
        assert_eq!(by_name["sdd"].status, DiskStatus::InUse);
        assert_eq!(by_name["sdd"].detail, "Member of pool tank (sdz).");
        assert!(!by_name["sdd"].removable);
        assert_eq!(
            by_name["sde"].contents,
            DiskContents::PartitionsOrSignatures
        );
        assert!(by_name["sde"].detail.starts_with("Has old partitions"));
        assert_eq!(by_name["sdf"].status, DiskStatus::PendingReplacement);
        assert!(by_name["sdf"].detail.contains("rpool-f"));
        assert_eq!(by_name["sdg"].status, DiskStatus::PendingReplacement);
        assert_eq!(by_name["sdg"].label, "Replacement in progress");
    }

    #[test]
    fn luks_mappings_must_match_crypttab_and_the_pool() {
        let mut layout = layout();
        let mapping = |mapper: &str| LuksMapping {
            mapper: mapper.into(),
            backing: None,
            disk: None,
            serial: None,
            size: None,
            in_pool: true,
        };
        let entry = |mapper: &str, shared_unlock| CrypttabEntry {
            mapper: mapper.into(),
            shared_unlock,
        };
        layout.vdevs[0].members = vec![member(Some("sda"), true)];
        layout.luks_mappings = vec![mapping("rpool-a")];
        layout.crypttab = vec![entry("rpool-a", true)];
        assert!(super::super::collect::luks_mismatches(&layout).is_empty());

        layout.crypttab = vec![entry("rpool-b", false)];
        layout.vdevs[0].members.push(member(Some("sdb"), false));
        assert_eq!(
            super::super::collect::luks_mismatches(&layout),
            [
                "rpool-b is in /etc/crypttab but not open",
                "rpool-b in /etc/crypttab is not unlocked with the shared passphrase",
                "rpool member /dev/sda uses rpool-a, which is not in /etc/crypttab, so it won't unlock at boot",
                "rpool mixes encrypted and unencrypted members (1 of 2 encrypted)",
            ]
        );
    }
}
