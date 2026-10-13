// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

//! What each vdev type and class allows and costs, shown next to every type
//! before one is picked. The wording is the README's ("Choosing vdev
//! types"), and a test keeps the two identical.

use serde::Serialize;

use crate::layout::{VdevClass, VdevType};

/// A data vdev choice. Mirrors appear twice, as 2-way and 3-way.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct DataTypeInfo {
    pub kind: VdevType,
    /// For mirrors, the width this row describes.
    pub width: Option<usize>,
    pub label: &'static str,
    pub survives: &'static str,
    pub usable: &'static str,
    pub pros: &'static str,
    pub cons: &'static str,
}

pub const DATA_TYPES: [DataTypeInfo; 6] = [
    DataTypeInfo {
        kind: VdevType::Single,
        width: None,
        label: "Single disk",
        survives: "No failures",
        usable: "100%",
        pros: "All the capacity; can later become a mirror by attaching a disk; removable later (see below)",
        cons: "Any disk failure loses the pool. ZFS detects corruption but cannot repair it. Not allowed as \
               the boot vdev of an encrypted host",
    },
    DataTypeInfo {
        kind: VdevType::Mirror,
        width: Some(2),
        label: "Mirror (2-way)",
        survives: "1 disk per vdev",
        usable: "50%",
        pros: "Best random I/O; fastest, lowest-risk rebuilds (a straight copy); no space lost on small \
               blocks; can be widened or narrowed; vdevs can be removed later (v2's disk decommissioning); \
               grows two disks at a time",
        cons: "Half the raw capacity",
    },
    DataTypeInfo {
        kind: VdevType::Mirror,
        width: Some(3),
        label: "Mirror (3-way)",
        survives: "2 disks per vdev",
        usable: "33%",
        pros: "As 2-way, and stays redundant during a rebuild",
        cons: "A third of the raw capacity",
    },
    DataTypeInfo {
        kind: VdevType::Raidz1,
        width: None,
        label: "raidz1",
        survives: "1 disk per vdev",
        usable: "(N−1)/N nominal",
        pros: "More capacity than mirrors",
        cons: "No redundancy left while rebuilding, and rebuilds read every disk, which is risky with large \
               spinning disks. Wastes space on small blocks (see below). Random I/O roughly that of one disk \
               per vdev. Rules out ever removing any vdev from the pool",
    },
    DataTypeInfo {
        kind: VdevType::Raidz2,
        width: None,
        label: "raidz2",
        survives: "2 disks per vdev",
        usable: "(N−2)/N nominal",
        pros: "Survives any two failures; the usual choice for large spinning disks",
        cons: "Same structural limits as raidz1; needs at least 4 disks, and at 4 disks gives the same \
               space as two mirrors with slower I/O",
    },
    DataTypeInfo {
        kind: VdevType::Raidz3,
        width: None,
        label: "raidz3",
        survives: "3 disks per vdev",
        usable: "(N−3)/N nominal",
        pros: "For wide vdevs of very large disks",
        cons: "Same limits; the most parity overhead",
    },
];

/// A helper vdev class.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct HelperInfo {
    pub class: VdevClass,
    pub label: &'static str,
    pub does: &'static str,
    pub pros: &'static str,
    pub cons: &'static str,
}

pub const HELPER_CLASSES: [HelperInfo; 4] = [
    HelperInfo {
        class: VdevClass::Special,
        label: "Special",
        does: "Holds metadata and, optionally, small blocks, on fast disks",
        pros: "Makes a pool of spinning disks feel much faster for metadata-heavy work",
        cons: "Losing it loses the pool, so give it at least the data vdevs' redundancy (a mirror; a 3-way \
               mirror alongside raidz2). Removable only under the same rules as data vdevs",
    },
    HelperInfo {
        class: VdevClass::Log,
        label: "Log (SLOG)",
        does: "Takes synchronous writes before they reach the pool",
        pros: "Speeds up guests that flush often (databases)",
        cons: "Helps only synchronous writes. Use disks with power-loss protection. Losing it loses at most \
               the last few seconds of synchronous writes, and only if the host also crashes. Removable at \
               any time",
    },
    HelperInfo {
        class: VdevClass::Cache,
        label: "Cache (L2ARC)",
        does: "A second-level read cache",
        pros: "Helps when the working set is larger than RAM",
        cons: "Uses some RAM to track its contents; rarely worth it on hosts with plenty of RAM. Removable \
               at any time",
    },
    HelperInfo {
        class: VdevClass::Spare,
        label: "Spare",
        does: "Replaces a failed member automatically",
        pros: "Restores redundancy without waiting for someone to swap a disk",
        cons: "Must be at least as large as the largest member it could replace. A spare that replaces a \
               boot-disk member has no boot partition until the failed disk is properly replaced (the \
               Dashboard says so). Removable at any time",
    },
];

/// The row that describes a data vdev of this type and width.
pub fn data_type(kind: VdevType, width: usize) -> &'static DataTypeInfo {
    let mirror_width = if width >= 3 { 3 } else { 2 };
    DATA_TYPES
        .iter()
        .find(|t| t.kind == kind && (kind != VdevType::Mirror || t.width == Some(mirror_width)))
        .unwrap_or(&DATA_TYPES[0])
}

pub fn helper(class: VdevClass) -> Option<&'static HelperInfo> {
    HELPER_CLASSES.iter().find(|h| h.class == class)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn readme_rows() -> Vec<String> {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../README.md");
        let text = std::fs::read_to_string(path).unwrap();
        text.lines()
            .filter(|l| l.starts_with("| "))
            .map(|l| l.replace("**", ""))
            .collect()
    }

    #[test]
    fn data_type_wording_matches_the_readme() {
        let rows = readme_rows();
        for t in DATA_TYPES {
            let row = format!(
                "| {} | {} | {} | {} | {} |",
                t.label, t.survives, t.usable, t.pros, t.cons
            );
            assert!(rows.contains(&row), "README has no row:\n{row}");
        }
    }

    #[test]
    fn helper_wording_matches_the_readme() {
        let rows = readme_rows();
        for h in HELPER_CLASSES {
            let row = format!("| {} | {} | {} | {} |", h.label, h.does, h.pros, h.cons);
            assert!(rows.contains(&row), "README has no row:\n{row}");
        }
    }

    #[test]
    fn every_offered_type_has_wording() {
        for kind in VdevClass::Data.allowed_types() {
            assert_eq!(data_type(*kind, kind.min_members()).kind, *kind);
        }
        assert_eq!(data_type(VdevType::Mirror, 4).label, "Mirror (3-way)");
        assert_eq!(data_type(VdevType::Mirror, 2).label, "Mirror (2-way)");
        assert!(data_type(VdevType::Raidz2, 6).cons.contains("raidz1"));
        assert!(
            data_type(VdevType::Raidz1, 3)
                .cons
                .ends_with("Rules out ever removing any vdev from the pool")
        );
        assert!(helper(VdevClass::Data).is_none());
        assert!(VdevClass::ALL.iter().skip(1).all(|c| helper(*c).is_some()));
    }
}
