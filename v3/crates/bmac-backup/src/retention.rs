// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

//! Retention: local retention on the cluster (`BACKUP.md`, "Local retention
//! and fast recovery") and appliance retention ("Appliance retention").
//!
//! Every bucket is UTC: a day, an ISO week starting Monday 00:00, or a
//! calendar month; the newest PIT in a bucket wins it. Local retention counts
//! days and weeks back from now, so it bounds what the cluster holds.
//! Appliance retention keeps the newest K buckets that hold a published PIT,
//! so an appliance that was off for a month still keeps K of each.

use std::collections::{BTreeMap, BTreeSet};

use jiff::{Timestamp, tz::TimeZone};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

const DAY: i64 = 86_400;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Tier {
    Daily,
    Weekly,
    Monthly,
}

impl Tier {
    /// The UTC bucket a time falls in, as an ordinal that grows with time.
    pub fn bucket(self, t_s: i64) -> i64 {
        match self {
            Tier::Daily => t_s.div_euclid(DAY),
            // 1970-01-01 was a Thursday; Monday 1969-12-29 starts week 0.
            Tier::Weekly => (t_s.div_euclid(DAY) + 3).div_euclid(7),
            Tier::Monthly => {
                let civil = Timestamp::from_second(t_s)
                    .map(|t| t.to_zoned(TimeZone::UTC))
                    .expect("PIT times are within jiff's range");
                i64::from(civil.year()) * 12 + i64::from(civil.month()) - 1
            }
        }
    }
}

/// Winners of the newest `keep` buckets that hold any of `pits`
/// (`(id, taken_s)`), each the newest PIT in its bucket.
fn newest_buckets(pits: &[(Uuid, i64)], tier: Tier, keep: u32) -> Vec<Uuid> {
    let mut by_bucket: BTreeMap<i64, (i64, Uuid)> = BTreeMap::new();
    for &(id, taken) in pits {
        let entry = by_bucket.entry(tier.bucket(taken)).or_insert((taken, id));
        if (taken, id) > *entry {
            *entry = (taken, id);
        }
    }
    by_bucket
        .values()
        .rev()
        .take(keep as usize)
        .map(|&(_, id)| id)
        .collect()
}

/// Winners of the buckets from the current one back `count` buckets.
fn recent_buckets(pits: &[(Uuid, i64)], tier: Tier, count: u32, now_s: i64) -> Vec<Uuid> {
    if count == 0 {
        return Vec::new();
    }
    let oldest = tier.bucket(now_s) - i64::from(count) + 1;
    let recent: Vec<(Uuid, i64)> = pits
        .iter()
        .copied()
        .filter(|&(_, t)| tier.bucket(t) >= oldest && t <= now_s)
        .collect();
    newest_buckets(&recent, tier, count)
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct LocalRetention {
    /// Keep the newest N PITs that an appliance has published.
    pub newest: u32,
    /// Keep one PIT for each of the last D days.
    pub days: u32,
    /// Keep one PIT for each of the last W ISO weeks.
    pub weeks: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LocalPit {
    pub pit_uuid: Uuid,
    pub taken_s: i64,
    pub backed_up: bool,
}

impl LocalRetention {
    pub fn is_none(&self) -> bool {
        *self == LocalRetention::default()
    }

    /// The PITs whose snapshots local retention keeps on the cluster.
    pub fn keep(&self, pits: &[LocalPit], now_s: i64) -> BTreeSet<Uuid> {
        let all: Vec<(Uuid, i64)> = pits.iter().map(|p| (p.pit_uuid, p.taken_s)).collect();
        let mut backed_up: Vec<&LocalPit> = pits.iter().filter(|p| p.backed_up).collect();
        backed_up.sort_by_key(|p| std::cmp::Reverse((p.taken_s, p.pit_uuid)));
        let mut keep: BTreeSet<Uuid> = backed_up
            .iter()
            .take(self.newest as usize)
            .map(|p| p.pit_uuid)
            .collect();
        keep.extend(recent_buckets(&all, Tier::Daily, self.days, now_s));
        keep.extend(recent_buckets(&all, Tier::Weekly, self.weeks, now_s));
        keep
    }

    /// Whether a slot at `slot_s` is captured even with no appliance
    /// available: it would be the first PIT of a day or week that local
    /// retention keeps.
    pub fn claims_slot(&self, pits: &[LocalPit], slot_s: i64) -> bool {
        let unclaimed = |tier: Tier, count: u32| {
            count > 0
                && !pits
                    .iter()
                    .any(|p| tier.bucket(p.taken_s) == tier.bucket(slot_s))
        };
        unclaimed(Tier::Daily, self.days) || unclaimed(Tier::Weekly, self.weeks)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ApplianceRetention {
    pub daily: u32,
    pub weekly: u32,
    pub monthly: u32,
}

impl Default for ApplianceRetention {
    fn default() -> ApplianceRetention {
        ApplianceRetention {
            daily: 7,
            weekly: 4,
            monthly: 12,
        }
    }
}

impl ApplianceRetention {
    /// Whether `self` keeps less than `before` in any tier, which takes
    /// effect only after the weakening delay.
    pub fn weakens(&self, before: &ApplianceRetention) -> bool {
        self.daily < before.daily || self.weekly < before.weekly || self.monthly < before.monthly
    }
}

/// Why the appliance keeps a PIT.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum KeepReason {
    Latest,
    Daily,
    Weekly,
    Monthly,
    BranchHead,
    BranchRoot,
    CommonAncestor,
    RestoreGrant,
    CloneOrigin,
    Hold,
    PeerRepair,
    Transfer,
}

/// One published PIT on the appliance. `parent` is the PIT it was received
/// on top of (`None` for a full receive); a PIT whose parent is on another
/// branch is that branch's root, and so is a branch's newest full receive.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppliancePit {
    pub pit_uuid: Uuid,
    pub taken_s: i64,
    pub branch: String,
    pub parent: Option<Uuid>,
}

/// The appliance's keep set, with every reason for each kept PIT. `pinned`
/// carries the reasons only the caller knows (grants, clone origins,
/// holds, peer repairs, transfers). A PIT absent from the result is pruned.
pub fn appliance_keep(
    pits: &[AppliancePit],
    policy: &ApplianceRetention,
    pinned: &BTreeMap<Uuid, BTreeSet<KeepReason>>,
) -> BTreeMap<Uuid, BTreeSet<KeepReason>> {
    let mut keep: BTreeMap<Uuid, BTreeSet<KeepReason>> = BTreeMap::new();
    let mut add = |id: Uuid, why: KeepReason| {
        keep.entry(id).or_default().insert(why);
    };
    let times: Vec<(Uuid, i64)> = pits.iter().map(|p| (p.pit_uuid, p.taken_s)).collect();
    if let Some(&(id, _)) = times.iter().max_by_key(|&&(id, t)| (t, id)) {
        add(id, KeepReason::Latest);
    }
    for (tier, count, why) in [
        (Tier::Daily, policy.daily, KeepReason::Daily),
        (Tier::Weekly, policy.weekly, KeepReason::Weekly),
        (Tier::Monthly, policy.monthly, KeepReason::Monthly),
    ] {
        for id in newest_buckets(&times, tier, count) {
            add(id, why);
        }
    }
    let by_id: BTreeMap<Uuid, &AppliancePit> = pits.iter().map(|p| (p.pit_uuid, p)).collect();
    let mut heads: BTreeMap<&str, (i64, Uuid)> = BTreeMap::new();
    // A branch reseeded by a full receive starts a new lineage; only the
    // newest full receive roots it, so older lineages expire.
    let mut seeds: BTreeMap<&str, (i64, Uuid)> = BTreeMap::new();
    for p in pits {
        let head = heads.entry(&p.branch).or_insert((p.taken_s, p.pit_uuid));
        if (p.taken_s, p.pit_uuid) > *head {
            *head = (p.taken_s, p.pit_uuid);
        }
        let Some(parent) = p.parent else {
            let seed = seeds.entry(&p.branch).or_insert((p.taken_s, p.pit_uuid));
            if (p.taken_s, p.pit_uuid) > *seed {
                *seed = (p.taken_s, p.pit_uuid);
            }
            continue;
        };
        if let Some(parent) = by_id.get(&parent)
            && parent.branch != p.branch
        {
            add(p.pit_uuid, KeepReason::BranchRoot);
            add(parent.pit_uuid, KeepReason::CommonAncestor);
        }
    }
    for (_, id) in heads.into_values() {
        add(id, KeepReason::BranchHead);
    }
    for (_, id) in seeds.into_values() {
        add(id, KeepReason::BranchRoot);
    }
    for (id, reasons) in pinned {
        for &why in reasons {
            add(*id, why);
        }
    }
    keep
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 2026-10-12T00:00:00Z, a Monday.
    const MONDAY: i64 = 1_791_763_200;
    const HOUR: i64 = 3600;

    fn id(n: u128) -> Uuid {
        Uuid::from_u128(n)
    }

    #[test]
    fn buckets_are_utc_days_iso_weeks_and_months() {
        assert_eq!(
            Timestamp::from_second(MONDAY).unwrap().to_string(),
            "2026-10-12T00:00:00Z"
        );
        assert_eq!(
            Tier::Weekly.bucket(MONDAY),
            Tier::Weekly.bucket(MONDAY + 7 * DAY - 1)
        );
        assert_eq!(
            Tier::Weekly.bucket(MONDAY - 1) + 1,
            Tier::Weekly.bucket(MONDAY)
        );
        assert_eq!(
            Tier::Daily.bucket(MONDAY - 1) + 1,
            Tier::Daily.bucket(MONDAY)
        );
        // 2026-10-01T00:00:00Z starts October.
        let october = MONDAY - 11 * DAY;
        assert_eq!(
            Tier::Monthly.bucket(october - 1) + 1,
            Tier::Monthly.bucket(october)
        );
        assert_eq!(Tier::Monthly.bucket(october), 2026 * 12 + 9);
    }

    #[test]
    fn local_retention_keeps_newest_backed_up_and_recent_days_and_weeks() {
        // Hourly PITs over the last 20 days; the newest two not backed up.
        let now = MONDAY + 10 * HOUR;
        let pits: Vec<LocalPit> = (0..20 * 24)
            .map(|n| LocalPit {
                pit_uuid: id(n),
                taken_s: now - n as i64 * HOUR,
                backed_up: n >= 2,
            })
            .collect();
        assert!(LocalRetention::default().keep(&pits, now).is_empty());
        let newest = LocalRetention {
            newest: 3,
            ..Default::default()
        };
        assert_eq!(
            newest.keep(&pits, now),
            BTreeSet::from([id(2), id(3), id(4)])
        );
        let days = LocalRetention {
            days: 2,
            ..Default::default()
        };
        // Today's newest (now) and yesterday's newest (23:00, 11 hours ago).
        assert_eq!(days.keep(&pits, now), BTreeSet::from([id(0), id(11)]));
        let weeks = LocalRetention {
            weeks: 2,
            ..Default::default()
        };
        // This week's newest and last Sunday's 23:00.
        assert_eq!(weeks.keep(&pits, now), BTreeSet::from([id(0), id(11)]));
        let weeks = LocalRetention {
            weeks: 3,
            ..Default::default()
        };
        assert_eq!(
            weeks.keep(&pits, now),
            BTreeSet::from([id(0), id(11), id(11 + 7 * 24)])
        );
    }

    #[test]
    fn local_retention_claims_the_first_slot_of_a_kept_day() {
        let days = LocalRetention {
            days: 1,
            ..Default::default()
        };
        let pits = [LocalPit {
            pit_uuid: id(1),
            taken_s: MONDAY + HOUR,
            backed_up: false,
        }];
        assert!(!days.claims_slot(&pits, MONDAY + 2 * HOUR));
        assert!(days.claims_slot(&pits, MONDAY + DAY));
        assert!(
            !LocalRetention {
                newest: 5,
                ..Default::default()
            }
            .claims_slot(&[], MONDAY)
        );
    }

    fn pit(n: u128, taken_s: i64, branch: &str, parent: Option<u128>) -> AppliancePit {
        AppliancePit {
            pit_uuid: id(n),
            taken_s,
            branch: branch.into(),
            parent: parent.map(id),
        }
    }

    #[test]
    fn appliance_keeps_bucket_winners_and_the_branch_graph() {
        // Daily PITs for 60 days on branch A; branch B forks from day 10.
        let mut pits: Vec<AppliancePit> = (0..60)
            .map(|n| {
                pit(
                    n,
                    MONDAY - (60 - n as i64) * DAY + HOUR,
                    "A",
                    n.checked_sub(1),
                )
            })
            .collect();
        pits.push(pit(100, MONDAY - 40 * DAY, "B", Some(10)));
        pits.push(pit(101, MONDAY - 39 * DAY, "B", Some(100)));
        let policy = ApplianceRetention {
            daily: 7,
            weekly: 0,
            monthly: 0,
        };
        let keep = appliance_keep(&pits, &policy, &BTreeMap::new());
        let daily: BTreeSet<Uuid> = (53..60).map(id).collect();
        let kept: BTreeSet<Uuid> = keep.keys().copied().collect();
        let expected: BTreeSet<Uuid> = daily
            .iter()
            .copied()
            .chain([id(0), id(10), id(100), id(101)])
            .collect();
        assert_eq!(kept, expected);
        assert!(keep[&id(59)].contains(&KeepReason::Latest));
        assert!(keep[&id(59)].contains(&KeepReason::BranchHead));
        assert_eq!(keep[&id(10)], BTreeSet::from([KeepReason::CommonAncestor]));
        assert_eq!(keep[&id(100)], BTreeSet::from([KeepReason::BranchRoot]));
        assert_eq!(keep[&id(0)], BTreeSet::from([KeepReason::BranchRoot]));

        let pinned = BTreeMap::from([(id(30), BTreeSet::from([KeepReason::RestoreGrant]))]);
        let keep = appliance_keep(&pits, &ApplianceRetention::default(), &pinned);
        assert!(keep.contains_key(&id(30)));
        // Four ISO weeks and the months (August through October) add winners.
        assert!(keep[&id(59)].contains(&KeepReason::Weekly));
        assert!(keep[&id(59)].contains(&KeepReason::Monthly));
        assert!(
            keep.values()
                .filter(|r| r.contains(&KeepReason::Monthly))
                .count()
                == 3
        );
    }

    #[test]
    fn weakening_is_any_tier_lowered() {
        let d = ApplianceRetention::default();
        assert!(!d.weakens(&d));
        assert!(ApplianceRetention { weekly: 3, ..d }.weakens(&d));
        assert!(!ApplianceRetention { monthly: 24, ..d }.weakens(&d));
    }
}
