// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

//! Pending branch roots (`BACKUP.md`, "Backups continue from the restored
//! PIT", step 4): restored PITs that the cluster's check-in replies name
//! because a new branch will continue from them. Each is kept from pruning
//! for up to 30 days from when it was first named, or until the cluster
//! stops naming it (the branch arrived here, or the restore was abandoned).
//! After that, a branch received here keeps its root through retention.

use std::collections::{BTreeMap, BTreeSet};
use std::io;

use bmac_backup::retention::KeepReason;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::fsx;
use crate::journal::Metadata;
use crate::retention::PitRef;

pub const KEEP_MS: i64 = 30 * 24 * 3600 * 1000;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Root {
    pub cluster_uuid: String,
    pub pit_uuid: Uuid,
    pub first_named_ms: i64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Roots {
    pub roots: Vec<Root>,
}

impl Roots {
    fn path(metadata: &Metadata) -> std::path::PathBuf {
        metadata.root.join("branch-roots.json")
    }

    pub fn load(metadata: &Metadata) -> io::Result<Roots> {
        Ok(fsx::read_json(&Roots::path(metadata))?.unwrap_or_default())
    }

    pub fn save(&self, metadata: &Metadata) -> io::Result<()> {
        fsx::write_json(&Roots::path(metadata), self, 0o600)
    }

    /// Takes `named` as `cluster`'s whole list; whether anything changed.
    pub fn named(&mut self, cluster: &str, named: &[Uuid], now_ms: i64) -> bool {
        let before = self.roots.clone();
        let wanted: BTreeSet<Uuid> = named.iter().copied().collect();
        self.roots
            .retain(|r| r.cluster_uuid != cluster || wanted.contains(&r.pit_uuid));
        for pit in wanted {
            if !self
                .roots
                .iter()
                .any(|r| r.cluster_uuid == cluster && r.pit_uuid == pit)
            {
                self.roots.push(Root {
                    cluster_uuid: cluster.to_string(),
                    pit_uuid: pit,
                    first_named_ms: now_ms,
                });
            }
        }
        self.roots != before
    }

    /// What the pending roots keep from retention.
    pub fn pins(&self, now_ms: i64) -> BTreeMap<PitRef, BTreeSet<KeepReason>> {
        let mut pins: BTreeMap<PitRef, BTreeSet<KeepReason>> = BTreeMap::new();
        for r in self
            .roots
            .iter()
            .filter(|r| now_ms - r.first_named_ms < KEEP_MS)
        {
            pins.entry((r.cluster_uuid.clone(), r.pit_uuid))
                .or_default()
                .insert(KeepReason::BranchRoot);
        }
        pins
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(n: u128) -> Uuid {
        Uuid::from_u128(n)
    }

    #[test]
    fn a_named_root_is_kept_for_thirty_days_from_when_it_was_first_named() {
        let mut roots = Roots::default();
        assert!(roots.named("c1", &[id(1)], 1_000));
        assert!(!roots.named("c1", &[id(1)], 2_000), "the first time stays");
        assert_eq!(roots.roots[0].first_named_ms, 1_000);
        assert_eq!(roots.pins(1_000 + KEEP_MS - 1).len(), 1);
        assert!(roots.pins(1_000 + KEEP_MS).is_empty());
    }

    #[test]
    fn each_cluster_names_only_its_own_roots() {
        let mut roots = Roots::default();
        roots.named("c1", &[id(1)], 0);
        roots.named("c2", &[id(2)], 0);
        assert!(roots.named("c1", &[], 0), "c1 dropped its root");
        let pins = roots.pins(0);
        assert_eq!(
            pins.keys().collect::<Vec<_>>(),
            vec![&("c2".to_string(), id(2))]
        );
    }

    #[test]
    fn roots_survive_a_restart() {
        let dir = tempfile::tempdir().unwrap();
        let metadata = Metadata::new(dir.path());
        let mut roots = Roots::default();
        roots.named("c1", &[id(1)], 5);
        roots.save(&metadata).unwrap();
        assert_eq!(Roots::load(&metadata).unwrap(), roots);
    }
}
