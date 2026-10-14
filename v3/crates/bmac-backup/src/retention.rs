// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

//! Retention: local retention on the cluster (`BACKUP.md`, "Local retention
//! and fast recovery") and appliance retention ("Appliance retention"), both
//! on the cluster's calendar ("Calendar").
//!
//! Days are the cluster's calendar days. Weeks, months and years count whole
//! days from a VM's start day, the day of its first PIT, so they never
//! straddle a day. The newest PIT of a period wins it.

use std::collections::{BTreeMap, BTreeSet};
use std::ops::RangeInclusive;

use jiff::civil::Date;
use jiff::{Timestamp, Unit, tz::TimeZone};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

const WEEK_DAYS: i64 = 7;
const YEAR_MONTHS: i64 = 12;

/// How times fall into days: calendar days in the cluster's time zone or,
/// for tests, days of `test_day_s` seconds starting every multiple of it
/// since the Unix epoch.
#[derive(Debug, Clone, PartialEq)]
pub struct Calendar {
    zone: TimeZone,
    test_day_s: Option<i64>,
}

impl Calendar {
    pub fn new(zone: TimeZone, test_day_s: Option<i64>) -> Calendar {
        Calendar {
            zone,
            test_day_s: test_day_s.filter(|s| *s > 0),
        }
    }

    pub fn utc() -> Calendar {
        Calendar::new(TimeZone::UTC, None)
    }

    /// The day `t_s` falls on, numbered so that each day is one more than
    /// the day before.
    pub fn day(&self, t_s: i64) -> i64 {
        if let Some(length) = self.test_day_s {
            return t_s.div_euclid(length);
        }
        let date = Timestamp::from_second(t_s)
            .expect("PIT times are within jiff's range")
            .to_zoned(self.zone.clone())
            .date();
        date.since((Unit::Day, Date::constant(1970, 1, 1)))
            .expect("dates are within jiff's range")
            .get_days()
            .into()
    }
}

/// A run of days from a VM's start day: a week, a month or a year.
#[derive(Debug, Clone, Copy)]
struct Period {
    start_day: i64,
    days: i64,
}

impl Period {
    fn of(self, day: i64) -> i64 {
        (day - self.start_day).div_euclid(self.days)
    }
}

/// The newest PIT of each period in `periods` that holds any of `pits`
/// (`(id, day, taken_s)`), keyed by `period_of(day)`.
fn winners(
    pits: &[(Uuid, i64, i64)],
    period_of: impl Fn(i64) -> i64,
    periods: RangeInclusive<i64>,
) -> Vec<Uuid> {
    let mut by_period: BTreeMap<i64, (i64, Uuid)> = BTreeMap::new();
    for &(id, day, taken) in pits {
        let period = period_of(day);
        if !periods.contains(&period) {
            continue;
        }
        let entry = by_period.entry(period).or_insert((taken, id));
        if (taken, id) > *entry {
            *entry = (taken, id);
        }
    }
    by_period.into_values().map(|(_, id)| id).collect()
}

/// The current period and `before` periods before it, or every one up to
/// it.
fn back(current: i64, before: Option<u32>) -> RangeInclusive<i64> {
    match before {
        Some(n) => current - i64::from(n)..=current,
        None => i64::MIN..=current,
    }
}

/// The last `count` periods, the current one included; none when `count`
/// is zero.
fn last(current: i64, count: u32) -> RangeInclusive<i64> {
    match count.checked_sub(1) {
        Some(before) => back(current, Some(before)),
        None => RangeInclusive::new(current + 1, current),
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct LocalRetention {
    /// Keep the newest N PITs that an appliance has published.
    pub newest: u32,
    /// Keep one PIT for each of the last D days, today included.
    pub days: u32,
    /// Keep one PIT for each of the last W weeks, this week included.
    pub weeks: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LocalPit {
    pub pit_uuid: Uuid,
    pub taken_s: i64,
    pub backed_up: bool,
}

/// The day of the oldest of `times`, which starts a VM's weeks when nothing
/// recorded it.
fn first_day(calendar: &Calendar, times: impl Iterator<Item = i64>) -> Option<i64> {
    times.min().map(|t| calendar.day(t))
}

impl LocalRetention {
    pub fn is_none(&self) -> bool {
        *self == LocalRetention::default()
    }

    /// The PITs whose snapshots local retention keeps on the cluster;
    /// `start_day` is the day of the VM's first PIT on the cluster.
    pub fn keep(
        &self,
        pits: &[LocalPit],
        calendar: &Calendar,
        start_day: Option<i64>,
        now_s: i64,
    ) -> BTreeSet<Uuid> {
        let mut backed_up: Vec<&LocalPit> = pits.iter().filter(|p| p.backed_up).collect();
        backed_up.sort_by_key(|p| std::cmp::Reverse((p.taken_s, p.pit_uuid)));
        let mut keep: BTreeSet<Uuid> = backed_up
            .iter()
            .take(self.newest as usize)
            .map(|p| p.pit_uuid)
            .collect();
        let all: Vec<(Uuid, i64, i64)> = pits
            .iter()
            .filter(|p| p.taken_s <= now_s)
            .map(|p| (p.pit_uuid, calendar.day(p.taken_s), p.taken_s))
            .collect();
        let today = calendar.day(now_s);
        keep.extend(winners(&all, |d| d, last(today, self.days)));
        let start_day = start_day
            .or_else(|| first_day(calendar, pits.iter().map(|p| p.taken_s)))
            .unwrap_or(today);
        let week = Period {
            start_day,
            days: WEEK_DAYS,
        };
        keep.extend(winners(
            &all,
            |d| week.of(d),
            last(week.of(today), self.weeks),
        ));
        keep
    }

    /// Whether a slot at `slot_s` is captured even with no appliance
    /// available: it would be the first PIT of a day or week that local
    /// retention keeps.
    pub fn claims_slot(
        &self,
        pits: &[LocalPit],
        calendar: &Calendar,
        start_day: Option<i64>,
        slot_s: i64,
    ) -> bool {
        let slot_day = calendar.day(slot_s);
        let start_day = start_day
            .or_else(|| first_day(calendar, pits.iter().map(|p| p.taken_s)))
            .unwrap_or(slot_day);
        let week = Period {
            start_day,
            days: WEEK_DAYS,
        };
        let days: Vec<i64> = pits.iter().map(|p| calendar.day(p.taken_s)).collect();
        (self.days > 0 && !days.contains(&slot_day))
            || (self.weeks > 0 && !days.iter().any(|&d| week.of(d) == week.of(slot_day)))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ApplianceRetention {
    /// The days before today that keep their newest PIT.
    pub daily: u32,
    /// The weeks before this week that keep their newest PIT.
    pub weekly: u32,
    /// The months before this month that keep their newest PIT, when
    /// `yearly` is on.
    pub monthly: u32,
    /// The days in a month.
    pub month_days: u32,
    /// Whether every year (12 months) before this one keeps its newest PIT,
    /// and older months none; off keeps every month's newest PIT.
    pub yearly: bool,
}

impl Default for ApplianceRetention {
    fn default() -> ApplianceRetention {
        ApplianceRetention {
            daily: 7,
            weekly: 4,
            monthly: 12,
            month_days: 30,
            yearly: true,
        }
    }
}

impl ApplianceRetention {
    pub fn validate(&self) -> Result<(), String> {
        if self.month_days < 28 || self.month_days > 31 {
            return Err("a month must be 28 to 31 days".into());
        }
        Ok(())
    }

    /// Whether `self` keeps less than `before`, which takes effect only
    /// after the weakening delay: a lower count, a different month, or
    /// yearly turned on.
    pub fn weakens(&self, before: &ApplianceRetention) -> bool {
        self.daily < before.daily
            || self.weekly < before.weekly
            || (self.yearly && before.yearly && self.monthly < before.monthly)
            || self.month_days != before.month_days
            || (self.yearly && !before.yearly)
    }
}

/// Why the appliance keeps a PIT.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum KeepReason {
    Latest,
    Today,
    Daily,
    Weekly,
    Monthly,
    Yearly,
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

/// Where a VM's history stands on the calendar: its cluster's calendar,
/// the day of its first PIT on this appliance, and now.
#[derive(Debug, Clone)]
pub struct Clock<'a> {
    pub calendar: &'a Calendar,
    pub start_day: Option<i64>,
    pub now_s: i64,
}

/// The appliance's keep set for one VM, with every reason for each kept
/// PIT. `pinned` carries the reasons only the caller knows (grants, clone
/// origins, holds, peer repairs, transfers). A PIT absent from the result
/// is pruned.
pub fn appliance_keep(
    pits: &[AppliancePit],
    policy: &ApplianceRetention,
    clock: &Clock,
    pinned: &BTreeMap<Uuid, BTreeSet<KeepReason>>,
) -> BTreeMap<Uuid, BTreeSet<KeepReason>> {
    let mut keep: BTreeMap<Uuid, BTreeSet<KeepReason>> = BTreeMap::new();
    let mut add = |id: Uuid, why: KeepReason| {
        keep.entry(id).or_default().insert(why);
    };
    if let Some(p) = pits.iter().max_by_key(|p| (p.taken_s, p.pit_uuid)) {
        add(p.pit_uuid, KeepReason::Latest);
    }
    let calendar = clock.calendar;
    let today = calendar.day(clock.now_s);
    let dated: Vec<(Uuid, i64, i64)> = pits
        .iter()
        .map(|p| (p.pit_uuid, calendar.day(p.taken_s), p.taken_s))
        .collect();
    for &(id, day, _) in &dated {
        if day >= today {
            add(id, KeepReason::Today);
        }
    }
    let start_day = clock
        .start_day
        .or_else(|| first_day(calendar, pits.iter().map(|p| p.taken_s)))
        .unwrap_or(today);
    let week = Period {
        start_day,
        days: WEEK_DAYS,
    };
    let month = Period {
        start_day,
        days: i64::from(policy.month_days.max(1)),
    };
    let year = Period {
        start_day,
        days: month.days * YEAR_MONTHS,
    };
    let day = Period { start_day, days: 1 };
    // Each tier keeps the current period's newest PIT as well, so a PIT
    // ages from one tier into the next without a day uncovered, even after
    // backups stop. `None` keeps every earlier period.
    let mut tiers = vec![
        (day, Some(policy.daily), KeepReason::Daily),
        (week, Some(policy.weekly), KeepReason::Weekly),
        (
            month,
            policy.yearly.then_some(policy.monthly),
            KeepReason::Monthly,
        ),
    ];
    if policy.yearly {
        tiers.push((year, None, KeepReason::Yearly));
    }
    for (period, count, why) in tiers {
        for id in winners(&dated, |d| period.of(d), back(period.of(today), count)) {
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
#[path = "retention_tests.rs"]
mod tests;
