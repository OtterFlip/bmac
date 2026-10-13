// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

//! The review step's summary (`STORAGE.md`, "Choosing a layout", step 3):
//! capacity per vdev and in total, what each vdev survives, whether LUKS is
//! possible, what the layout allows later, and every finding.

use std::collections::BTreeMap;

use bmac_core::Bytes;
use serde::Serialize;

use crate::checks::{self, Disk, Findings, Target};
use crate::geometry::{self, BlockSize, Geometry, PoolGeometry, Ratio};
use crate::layout::{ById, Encryption, PoolLayout, Vdev, VdevClass, VdevType};
use crate::partition;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct BlockFigure {
    pub block: BlockSize,
    pub inflation: Ratio,
    /// VM data this much space holds at this block size.
    pub bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct VdevSummary {
    pub index: usize,
    pub class: VdevClass,
    #[serde(rename = "type")]
    pub kind: VdevType,
    pub boot: bool,
    pub members: usize,
    pub description: String,
    /// Usable bytes of the smallest member's ZFS partition (after the LUKS
    /// header); `None` when a member is missing from the inventory.
    pub member_bytes: Option<u64>,
    pub nominal: Option<u64>,
    /// What ZFS will report for this vdev.
    pub reported: Option<u64>,
    /// Data vdevs only.
    pub vm: Vec<BlockFigure>,
    pub survives: usize,
    pub loss: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Operation {
    pub name: &'static str,
    pub allowed: bool,
    pub note: &'static str,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Summary {
    pub vdevs: Vec<VdevSummary>,
    pub nominal: u64,
    pub reported: u64,
    /// VM data the pool holds at each offered block size, using the worst
    /// data vdev's inflation.
    pub vm: Vec<BlockFigure>,
    pub suggested_block_size: Option<BlockSize>,
    pub luks_possible: bool,
    pub operations: Vec<Operation>,
    /// Plain statements the review shows above the details.
    pub statements: Vec<String>,
    pub findings: Findings,
}

fn loss(vdev: &Vdev) -> String {
    let survives = vdev.failures_tolerated();
    let failures = if survives == 1 {
        "1 disk failure".to_string()
    } else {
        format!("{survives} disk failures")
    };
    match vdev.class {
        VdevClass::Data | VdevClass::Special if survives == 0 => {
            "No redundancy: losing this disk loses the whole pool.".to_string()
        }
        VdevClass::Data | VdevClass::Special => format!("Survives {failures}; losing more loses the whole pool."),
        VdevClass::Log => {
            "Losing it loses at most the last few seconds of synchronous writes, and only if the host also crashes."
                .to_string()
        }
        VdevClass::Cache => "Losing it loses nothing; reads come from the pool again.".to_string(),
        VdevClass::Spare => "Holds nothing until it replaces a failed member.".to_string(),
    }
}

fn operations(layout: &PoolLayout) -> Vec<Operation> {
    let any = |kinds: &[VdevType]| layout.data_vdevs().any(|v| kinds.contains(&v.kind));
    let mirrors = layout.vdevs.iter().any(|v| v.kind == VdevType::Mirror);
    vec![
        Operation {
            name: "Add a data vdev",
            allowed: true,
            note: "Warned when its redundancy differs",
        },
        Operation {
            name: "Widen a mirror",
            allowed: any(&[VdevType::Mirror, VdevType::Single]),
            note: "Includes turning a single disk into a mirror",
        },
        Operation {
            name: "Add a disk to a raidz vdev",
            allowed: layout.has_raidz(),
            note: "Data written before keeps its old data-to-parity ratio until rewritten",
        },
        Operation {
            name: "Replace a disk",
            allowed: true,
            note: "Boot members also get an ESP",
        },
        Operation {
            name: "Grow by replacing with larger disks",
            allowed: true,
            note: "Once every member of a vdev is larger",
        },
        Operation {
            name: "Remove a data or special vdev",
            allowed: layout.allows_vdev_removal(),
            note: "Impossible once the pool has any raidz vdev; never the boot vdev",
        },
        Operation {
            name: "Add or remove log, cache, and spare vdevs",
            allowed: true,
            note: "",
        },
        Operation {
            name: "Detach a mirror member",
            allowed: mirrors,
            note: "",
        },
    ]
}

/// Summarizes `layout` built from `inventory`, with VM figures for each of
/// `offered`.
pub fn summarize(
    layout: &PoolLayout,
    inventory: &[Disk],
    target: Target,
    offered: &[BlockSize],
) -> Summary {
    let findings = checks::check(layout, inventory, target);
    let mut offered = offered.to_vec();
    offered.sort();
    let structural = layout.validate().is_ok();
    let by_id: BTreeMap<&ById, &Disk> = inventory.iter().map(|d| (&d.by_id, d)).collect();
    let header = if layout.encryption == Encryption::Luks2 {
        partition::LUKS2_HEADER_BYTES
    } else {
        0
    };

    let mut vdevs = Vec::new();
    for (index, vdev) in layout.vdevs.iter().enumerate().filter(|_| structural) {
        let sizes: Option<Vec<u64>> = vdev
            .members
            .iter()
            .map(|m| by_id.get(m).map(|d| d.size))
            .collect();
        let member_bytes = sizes.and_then(|sizes| {
            let partition = if vdev.boot {
                partition::boot_partition_bytes(&sizes)
            } else {
                partition::member_partition_bytes(&sizes)
            };
            partition.ok().map(|p| p.saturating_sub(header))
        });
        let g = Geometry::of(vdev, layout.ashift);
        let counts = matches!(vdev.class, VdevClass::Data | VdevClass::Special);
        let vm = match (vdev.class, member_bytes) {
            (VdevClass::Data, Some(bytes)) => offered
                .iter()
                .map(|&block| BlockFigure {
                    block,
                    inflation: g.inflation(block),
                    bytes: g.vm_bytes(bytes, block),
                })
                .collect(),
            _ => Vec::new(),
        };
        vdevs.push(VdevSummary {
            index,
            class: vdev.class,
            kind: vdev.kind,
            boot: vdev.boot,
            members: vdev.members.len(),
            description: vdev.describe(),
            member_bytes,
            nominal: member_bytes.filter(|_| counts).map(|b| g.nominal_bytes(b)),
            reported: member_bytes.filter(|_| counts).map(|b| g.reported_bytes(b)),
            vm,
            survives: vdev.failures_tolerated(),
            loss: loss(vdev),
        });
    }

    let data = || vdevs.iter().filter(|v| v.class == VdevClass::Data);
    let nominal = data().filter_map(|v| v.nominal).sum();
    let reported: u64 = data().filter_map(|v| v.reported).sum();
    let pool = PoolGeometry::of(layout);
    let vm: Vec<BlockFigure> = if structural {
        offered
            .iter()
            .map(|&block| {
                let inflation = pool.inflation(block);
                BlockFigure {
                    block,
                    inflation,
                    bytes: inflation.div_floor(reported),
                }
            })
            .collect()
    } else {
        Vec::new()
    };
    let suggested_block_size = if structural {
        geometry::suggested_block_size(std::slice::from_ref(&pool), &offered)
    } else {
        None
    };
    let luks_possible =
        target == Target::Appliance || layout.boot_vdevs().all(|(_, v)| v.failures_tolerated() > 0);

    let mut statements = Vec::new();
    if structural {
        statements.extend(raidz_headlines(layout, &vdevs, &offered));
        if let (Some(first), Some(best)) = (
            vm.first(),
            suggested_block_size.and_then(|b| vm.iter().find(|f| f.block == b)),
        ) && best.block != first.block
            && best.bytes > first.bytes
        {
            statements.push(format!(
                "raidz wastes space on small blocks: VM disks at {} hold {}; {} would give {}.",
                first.block,
                Bytes(first.bytes),
                best.block,
                Bytes(best.bytes)
            ));
        }
        if layout.has_raidz() {
            statements.push(
                "This pool can never be shrunk. It has a raidz vdev, so ZFS can never remove any of its vdevs, \
                 and disk decommissioning is unavailable for it."
                    .to_string(),
            );
            statements.push(
                "After a disk is added to a raidz vdev, data written before keeps its old data-to-parity ratio, \
                 and ZFS keeps reporting space at that ratio, which understates free space. BMAC uses ZFS's \
                 figure."
                    .to_string(),
            );
        } else {
            let which = if target == Target::Host {
                "Vdevs other than the boot vdev"
            } else {
                "Vdevs"
            };
            statements.push(format!(
                "{which} can be removed later, as long as the pool never gets a raidz vdev."
            ));
        }
        if !luks_possible {
            statements.push(
                "Encryption is not possible with this layout: an encrypted host needs a redundant boot vdev."
                    .to_string(),
            );
        }
    }

    Summary {
        vdevs,
        nominal,
        reported,
        vm,
        suggested_block_size,
        luks_possible,
        operations: operations(layout),
        statements,
        findings,
    }
}

/// "6 × 3.64 TiB raidz1: nominal 18.19 TiB (5 of 6 disks), ZFS will report
/// about 17.46 TiB, VM disks at 16 KiB: 14.55 TiB."
fn raidz_headlines(
    layout: &PoolLayout,
    vdevs: &[VdevSummary],
    offered: &[BlockSize],
) -> Vec<String> {
    let Some(&smallest_block) = offered.first() else {
        return Vec::new();
    };
    vdevs
        .iter()
        .filter(|v| v.class == VdevClass::Data && v.kind.is_raidz())
        .filter_map(|v| {
            let member = v.member_bytes?;
            let g = Geometry::of(&layout.vdevs[v.index], layout.ashift);
            let data_disks = v.members - v.kind.parity().unwrap_or(0) as usize;
            Some(format!(
                "{} × {} {}: nominal {} ({data_disks} of {} disks), ZFS will report about {}, VM disks at {}: {}.",
                v.members,
                Bytes(member),
                v.kind,
                Bytes(g.nominal_bytes(member)),
                v.members,
                Bytes(g.reported_bytes(member)),
                smallest_block,
                Bytes(g.vm_bytes(member, smallest_block))
            ))
        })
        .collect()
}
