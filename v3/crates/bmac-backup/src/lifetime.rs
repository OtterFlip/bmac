// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

//! The lifetime of a PIT's snapshots and bookmarks on the cluster
//! (`BACKUP.md`, "Snapshots and bookmarks on the cluster"). Pure decisions;
//! `bmacd` carries them out, bookmarking every placement host and checking
//! the GUIDs before it destroys a released snapshot.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::settings::BackupSettings;

/// One appliance's part in one PIT.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum Offer {
    /// Offered at capture; not claimed yet.
    Open {
        offered_s: i64,
    },
    /// Claimed; `progress_s` is the last time bytes moved, and `bytes` how
    /// many the appliance had reported then.
    Claimed {
        progress_s: i64,
        #[serde(default)]
        bytes: u64,
    },
    Published,
    Failed,
}

impl Offer {
    /// Whether this offer still needs the PIT's snapshots.
    pub fn holds(&self, now_s: i64, settings: &BackupSettings) -> bool {
        match *self {
            Offer::Open { offered_s } => now_s - offered_s <= settings.claim_window_s,
            Offer::Claimed { progress_s, .. } => now_s - progress_s <= settings.stalled_after_s,
            Offer::Published | Offer::Failed => false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClusterPit {
    pub pit_uuid: Uuid,
    pub taken_s: i64,
    pub branch: String,
    /// Offers by appliance ID.
    pub offers: BTreeMap<String, Offer>,
}

/// An assigned appliance's head: the newest PIT of this VM it has published.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Head {
    pub appliance: String,
    pub pit_uuid: Uuid,
    pub delinquent: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SnapshotNeed {
    /// An open claim window or a transfer in progress.
    Offer,
    LocalRetention,
    StagingClone,
    Restore,
    /// Bookmarks aren't proven, and an appliance that isn't delinquent has
    /// this PIT as its head.
    ApplianceHead,
    /// Bookmarks aren't proven, and a new branch's first PIT will be sent
    /// from this one.
    BranchStart,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BookmarkNeed {
    ApplianceHead,
    Newest,
    /// A restored branch's first PIT, until every assigned appliance has
    /// published it or become delinquent.
    BranchStart,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Inputs {
    pub pits: Vec<ClusterPit>,
    pub heads: Vec<Head>,
    pub local_retention: BTreeSet<Uuid>,
    pub staging_origins: BTreeSet<Uuid>,
    pub restores: BTreeSet<Uuid>,
    pub branch_starts: BTreeSet<Uuid>,
    /// Lab items 2 and 3 passed for this release.
    pub bookmarks_proven: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Plan {
    /// PITs whose snapshots stay, and why.
    pub snapshots: BTreeMap<Uuid, BTreeSet<SnapshotNeed>>,
    /// PITs whose bookmarks stay (or are made), and why.
    pub bookmarks: BTreeMap<Uuid, BTreeSet<BookmarkNeed>>,
}

impl Plan {
    /// Whether the snapshot of `pit` may be released (after its bookmarks
    /// are made and checked, when it keeps them).
    pub fn releases_snapshot(&self, pit: &Uuid) -> bool {
        !self.snapshots.contains_key(pit)
    }

    pub fn keeps_bookmark(&self, pit: &Uuid) -> bool {
        self.bookmarks.contains_key(pit)
    }
}

pub fn plan(inputs: &Inputs, now_s: i64, settings: &BackupSettings) -> Plan {
    let mut out = Plan::default();
    let mut snapshot = |id: Uuid, need: SnapshotNeed| {
        out.snapshots.entry(id).or_default().insert(need);
    };
    for pit in &inputs.pits {
        if pit.offers.values().any(|o| o.holds(now_s, settings)) {
            snapshot(pit.pit_uuid, SnapshotNeed::Offer);
        }
    }
    let known: BTreeSet<Uuid> = inputs.pits.iter().map(|p| p.pit_uuid).collect();
    for (set, need) in [
        (&inputs.local_retention, SnapshotNeed::LocalRetention),
        (&inputs.staging_origins, SnapshotNeed::StagingClone),
        (&inputs.restores, SnapshotNeed::Restore),
    ] {
        for id in set.intersection(&known) {
            snapshot(*id, need);
        }
    }
    let live_heads: BTreeSet<Uuid> = inputs
        .heads
        .iter()
        .filter(|h| !h.delinquent && known.contains(&h.pit_uuid))
        .map(|h| h.pit_uuid)
        .collect();
    if !inputs.bookmarks_proven {
        for id in &live_heads {
            snapshot(*id, SnapshotNeed::ApplianceHead);
        }
    }

    let mut bookmark = |id: Uuid, need: BookmarkNeed| {
        out.bookmarks.entry(id).or_default().insert(need);
    };
    if inputs.bookmarks_proven {
        for id in &live_heads {
            bookmark(*id, BookmarkNeed::ApplianceHead);
        }
    }
    if let Some(newest) = inputs.pits.iter().max_by_key(|p| (p.taken_s, p.pit_uuid)) {
        bookmark(newest.pit_uuid, BookmarkNeed::Newest);
    }
    for id in inputs.branch_starts.intersection(&known) {
        if inputs.bookmarks_proven {
            bookmark(*id, BookmarkNeed::BranchStart);
        } else {
            out.snapshots
                .entry(*id)
                .or_default()
                .insert(SnapshotNeed::BranchStart);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: i64 = 1_000_000;

    fn id(n: u128) -> Uuid {
        Uuid::from_u128(n)
    }

    fn pit(n: u128, offers: &[(&str, Offer)]) -> ClusterPit {
        ClusterPit {
            pit_uuid: id(n),
            taken_s: NOW - (10 - n as i64) * 3600,
            branch: "A".into(),
            offers: offers.iter().map(|(a, o)| (a.to_string(), *o)).collect(),
        }
    }

    fn head(appliance: &str, n: u128, delinquent: bool) -> Head {
        Head {
            appliance: appliance.into(),
            pit_uuid: id(n),
            delinquent,
        }
    }

    #[test]
    fn offers_hold_snapshots_until_published_failed_lapsed_or_stalled() {
        let s = BackupSettings::default();
        assert!(
            Offer::Open {
                offered_s: NOW - 300
            }
            .holds(NOW, &s)
        );
        assert!(
            !Offer::Open {
                offered_s: NOW - 301
            }
            .holds(NOW, &s)
        );
        assert!(
            Offer::Claimed {
                progress_s: NOW - 600,
                bytes: 0,
            }
            .holds(NOW, &s)
        );
        assert!(
            !Offer::Claimed {
                progress_s: NOW - 601,
                bytes: 0,
            }
            .holds(NOW, &s)
        );
        assert!(!Offer::Published.holds(NOW, &s));
        assert!(!Offer::Failed.holds(NOW, &s));
    }

    #[test]
    fn released_pits_keep_bookmarks_only_for_live_heads_and_the_newest() {
        let s = BackupSettings::default();
        let inputs = Inputs {
            pits: vec![
                pit(1, &[("a", Offer::Published), ("b", Offer::Published)]),
                pit(2, &[("a", Offer::Published)]),
                pit(3, &[("a", Offer::Published)]),
                pit(
                    4,
                    &[(
                        "a",
                        Offer::Claimed {
                            progress_s: NOW - 5,
                            bytes: 0,
                        },
                    )],
                ),
            ],
            heads: vec![head("a", 3, false), head("b", 1, false), head("c", 2, true)],
            local_retention: BTreeSet::from([id(2)]),
            bookmarks_proven: true,
            ..Inputs::default()
        };
        let plan = plan(&inputs, NOW, &s);
        assert_eq!(
            plan.snapshots.keys().copied().collect::<Vec<_>>(),
            vec![id(2), id(4)]
        );
        assert!(plan.releases_snapshot(&id(1)) && plan.releases_snapshot(&id(3)));
        assert_eq!(
            plan.bookmarks.keys().copied().collect::<Vec<_>>(),
            vec![id(1), id(3), id(4)]
        );
        assert!(
            !plan.keeps_bookmark(&id(2)),
            "a delinquent appliance's head loses its bookmark"
        );
    }

    #[test]
    fn until_bookmarks_are_proven_live_heads_keep_snapshots() {
        let s = BackupSettings::default();
        let inputs = Inputs {
            pits: vec![
                pit(1, &[("a", Offer::Published)]),
                pit(2, &[("b", Offer::Published)]),
            ],
            heads: vec![head("a", 1, false), head("b", 2, true)],
            branch_starts: BTreeSet::from([id(1), id(9)]),
            ..Inputs::default()
        };
        let plan = plan(&inputs, NOW, &s);
        assert_eq!(
            plan.snapshots[&id(1)],
            BTreeSet::from([SnapshotNeed::ApplianceHead, SnapshotNeed::BranchStart]),
            "a branch start keeps its snapshot, not a bookmark"
        );
        assert!(plan.releases_snapshot(&id(2)));
        assert!(!plan.keeps_bookmark(&id(1)));
        assert_eq!(
            plan.bookmarks[&id(2)],
            BTreeSet::from([BookmarkNeed::Newest])
        );
        assert!(!plan.keeps_bookmark(&id(9)), "unknown PITs are ignored");
    }
}
