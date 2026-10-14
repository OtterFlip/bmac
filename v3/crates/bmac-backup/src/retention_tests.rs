// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

use super::*;

/// 2026-10-12T00:00:00Z.
const MONDAY: i64 = 1_791_763_200;
const HOUR: i64 = 3600;
const DAY: i64 = 24 * HOUR;

fn id(n: u128) -> Uuid {
    Uuid::from_u128(n)
}

fn denver() -> Calendar {
    Calendar::new(TimeZone::get("America/Denver").unwrap(), None)
}

#[test]
fn days_are_the_zones_calendar_days() {
    let utc = Calendar::utc();
    assert_eq!(utc.day(MONDAY - 1) + 1, utc.day(MONDAY));
    assert_eq!(utc.day(MONDAY), utc.day(MONDAY + DAY - 1));
    // 2026-10-12 23:30 in Denver (MDT, UTC-6) is 05:30Z the next day.
    let denver = denver();
    assert_eq!(denver.day(MONDAY + 6 * HOUR), utc.day(MONDAY));
    assert_eq!(denver.day(MONDAY + 29 * HOUR + HOUR / 2), utc.day(MONDAY));
    assert_eq!(denver.day(MONDAY + 30 * HOUR), utc.day(MONDAY) + 1);
    // 2026-11-01 has 25 hours in Denver: 00:30 MDT and 23:30 MST.
    let november = 1_793_512_800; // 2026-11-01T06:00:00Z, 00:00 MDT
    assert_eq!(
        denver.day(november + HOUR / 2),
        denver.day(november + 24 * HOUR + HOUR / 2)
    );
    assert_eq!(
        denver.day(november + 25 * HOUR + HOUR / 2),
        denver.day(november) + 1
    );
}

#[test]
fn a_test_day_lasts_its_length_from_the_epoch() {
    let fast = Calendar::new(TimeZone::UTC, Some(60));
    assert_eq!(fast.day(MONDAY), MONDAY / 60);
    assert_eq!(fast.day(MONDAY + 59), fast.day(MONDAY));
    assert_eq!(fast.day(MONDAY + 60), fast.day(MONDAY) + 1);
    assert_eq!(Calendar::new(TimeZone::UTC, Some(0)), Calendar::utc());
}

/// Hourly PITs on one branch, each received on top of the one before,
/// hour `n` after `start` being `id(n)`.
fn hourly(start: i64, hours: u128) -> Vec<AppliancePit> {
    (0..hours)
        .map(|n| AppliancePit {
            pit_uuid: id(n),
            taken_s: start + n as i64 * HOUR,
            branch: "A".into(),
            parent: n.checked_sub(1).map(id),
        })
        .collect()
}

fn reasons(keep: &BTreeMap<Uuid, BTreeSet<KeepReason>>, why: KeepReason) -> BTreeSet<Uuid> {
    keep.iter()
        .filter(|(_, r)| r.contains(&why))
        .map(|(id, _)| *id)
        .collect()
}

/// The last PIT (23:00) of day `d` of `hourly`.
fn evening(d: i64) -> Uuid {
    id((d * 24 + 23) as u128)
}

#[test]
fn a_long_history_keeps_today_then_days_weeks_months_and_years() {
    // 800 days of hourly PITs from midnight; now is 10:00 on day 800.
    let calendar = Calendar::utc();
    let start = MONDAY - 800 * DAY;
    let pits = hourly(start, 800 * 24 + 11);
    let clock = Clock {
        calendar: &calendar,
        start_day: None,
        now_s: start + 800 * DAY + 10 * HOUR,
    };
    let keep = appliance_keep(
        &pits,
        &ApplianceRetention::default(),
        &clock,
        &BTreeMap::new(),
    );

    let today: BTreeSet<Uuid> = (800 * 24..=800 * 24 + 10).map(id).collect();
    assert_eq!(reasons(&keep, KeepReason::Today), today);
    let newest = id(800 * 24 + 10);
    let with_today = |mut s: BTreeSet<Uuid>| {
        s.insert(newest);
        s
    };
    // Days 793 to 799; weeks 110 to 113 (week 114 is this one) end on day
    // 7w + 6; months 14 to 25 of 30 days end on day 30m + 29; years 0 and 1
    // of 360 days end on day 360y + 359.
    assert_eq!(
        reasons(&keep, KeepReason::Daily),
        with_today((793..=799).map(evening).collect())
    );
    assert_eq!(
        reasons(&keep, KeepReason::Weekly),
        with_today((110..=113).map(|w| evening(7 * w + 6)).collect())
    );
    assert_eq!(
        reasons(&keep, KeepReason::Monthly),
        with_today((14..=25).map(|m| evening(30 * m + 29)).collect())
    );
    assert_eq!(
        reasons(&keep, KeepReason::Yearly),
        with_today((0..=1).map(|y| evening(360 * y + 359)).collect())
    );
    assert!(keep[&newest].contains(&KeepReason::Latest));
    assert!(keep[&newest].contains(&KeepReason::BranchHead));
    assert_eq!(keep[&id(0)], BTreeSet::from([KeepReason::BranchRoot]));
    // 11 today, 7 days, 4 weeks (day 797 is also a daily), 12 months,
    // 2 years (day 719 is also month 23's), and the first PIT.
    assert_eq!(keep.len(), 11 + 7 + 3 + 12 + 1 + 1);
}

#[test]
fn without_yearly_every_month_keeps_its_newest() {
    let calendar = Calendar::utc();
    let start = MONDAY - 800 * DAY;
    let pits = hourly(start, 800 * 24 + 11);
    let clock = Clock {
        calendar: &calendar,
        start_day: None,
        now_s: start + 800 * DAY + 10 * HOUR,
    };
    let policy = ApplianceRetention {
        yearly: false,
        ..ApplianceRetention::default()
    };
    let keep = appliance_keep(&pits, &policy, &clock, &BTreeMap::new());
    assert!(reasons(&keep, KeepReason::Yearly).is_empty());
    let mut months: BTreeSet<Uuid> = (0..=25).map(|m| evening(30 * m + 29)).collect();
    months.insert(id(800 * 24 + 10));
    assert_eq!(reasons(&keep, KeepReason::Monthly), months);
}

#[test]
fn periods_count_from_the_recorded_start_day() {
    // Daily PITs at noon for 40 days; weeks start on the recorded day 2.
    let calendar = Calendar::utc();
    let start = MONDAY - 40 * DAY;
    let pits: Vec<AppliancePit> = (0..40)
        .map(|n| AppliancePit {
            pit_uuid: id(n),
            taken_s: start + n as i64 * DAY + 12 * HOUR,
            branch: "A".into(),
            parent: n.checked_sub(1).map(id),
        })
        .collect();
    let day0 = calendar.day(start);
    let clock = Clock {
        calendar: &calendar,
        start_day: Some(day0 + 2),
        now_s: start + 39 * DAY + 13 * HOUR,
    };
    let policy = ApplianceRetention {
        daily: 0,
        weekly: 2,
        monthly: 0,
        ..ApplianceRetention::default()
    };
    let keep = appliance_keep(&pits, &policy, &clock, &BTreeMap::new());
    // Weeks from day 2: day 39 is in week 5 (days 37 to 43), so weeks 3
    // and 4 end on days 29 and 36.
    assert_eq!(
        reasons(&keep, KeepReason::Weekly),
        BTreeSet::from([id(29), id(36), id(39)])
    );
}

#[test]
fn a_pruned_pit_is_never_wanted_again() {
    // Four PITs a test day, for 400 test days, with retention run after
    // each one: what it dropped once, it never keeps later.
    let calendar = Calendar::new(TimeZone::UTC, Some(4));
    let policy = ApplianceRetention::default();
    let mut pruned: BTreeSet<Uuid> = BTreeSet::new();
    let mut pits: Vec<AppliancePit> = Vec::new();
    for n in 0..1600u128 {
        pits.push(AppliancePit {
            pit_uuid: id(n),
            taken_s: n as i64,
            branch: "A".into(),
            parent: n.checked_sub(1).map(id),
        });
        let clock = Clock {
            calendar: &calendar,
            start_day: Some(0),
            now_s: n as i64,
        };
        let keep = appliance_keep(&pits, &policy, &clock, &BTreeMap::new());
        assert!(
            pruned.iter().all(|p| !keep.contains_key(p)),
            "a pruned PIT is wanted again at {n}"
        );
        pits.retain(|p| {
            let kept = keep.contains_key(&p.pit_uuid);
            if !kept {
                pruned.insert(p.pit_uuid);
            }
            kept
        });
    }
    // 4 today, 7 days, 4 weeks, 12 months, a year, and the first PIT.
    assert!(pits.len() <= 4 + 7 + 4 + 12 + 1 + 1, "{} kept", pits.len());
}

#[test]
fn after_backups_stop_the_last_pit_of_each_period_stays() {
    // Hourly PITs for 40 days, then none for 30 more: the last PIT carries
    // on as the newest of its week and month.
    let calendar = Calendar::utc();
    let start = MONDAY - 70 * DAY;
    let pits = hourly(start, 40 * 24);
    let last = id(40 * 24 - 1);
    for days_later in [8, 20, 29, 30, 31] {
        let clock = Clock {
            calendar: &calendar,
            start_day: None,
            now_s: start + (40 + days_later) * DAY,
        };
        let keep = appliance_keep(
            &pits,
            &ApplianceRetention::default(),
            &clock,
            &BTreeMap::new(),
        );
        let why = &keep[&last];
        assert!(
            why.contains(&KeepReason::Weekly) || why.contains(&KeepReason::Monthly),
            "{days_later} days later: {why:?}"
        );
    }
}

#[test]
fn branches_keep_their_heads_roots_and_common_ancestors() {
    // Daily PITs for 60 days on branch A; branch B forks from day 10.
    let calendar = Calendar::utc();
    let pit = |n: u128, taken_s: i64, branch: &str, parent: Option<u128>| AppliancePit {
        pit_uuid: id(n),
        taken_s,
        branch: branch.into(),
        parent: parent.map(id),
    };
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
        ..ApplianceRetention::default()
    };
    let clock = Clock {
        calendar: &calendar,
        start_day: None,
        now_s: MONDAY + 2 * HOUR,
    };
    let keep = appliance_keep(&pits, &policy, &clock, &BTreeMap::new());
    let kept: BTreeSet<Uuid> = keep.keys().copied().collect();
    // No PIT yet today, so the 7 days before it: days 53 to 59.
    let daily: BTreeSet<Uuid> = (53..60).map(id).collect();
    assert!(daily.is_subset(&kept));
    for n in [0, 10, 100, 101] {
        assert!(kept.contains(&id(n)), "{n}");
    }
    assert!(keep[&id(59)].contains(&KeepReason::Latest));
    assert!(keep[&id(59)].contains(&KeepReason::BranchHead));
    assert!(keep[&id(10)].contains(&KeepReason::CommonAncestor));
    assert!(keep[&id(100)].contains(&KeepReason::BranchRoot));
    assert!(keep[&id(101)].contains(&KeepReason::BranchHead));

    let pinned = BTreeMap::from([(id(30), BTreeSet::from([KeepReason::RestoreGrant]))]);
    let keep = appliance_keep(&pits, &policy, &clock, &pinned);
    assert_eq!(keep[&id(30)], BTreeSet::from([KeepReason::RestoreGrant]));
}

#[test]
fn local_retention_keeps_newest_backed_up_and_recent_days_and_weeks() {
    // Hourly PITs over the last 20 days, from midnight; the newest two not
    // backed up. Weeks count from the first PIT's day.
    let calendar = Calendar::utc();
    let now = MONDAY + 10 * HOUR;
    let pits: Vec<LocalPit> = (0..20 * 24 + 11)
        .map(|n| LocalPit {
            pit_uuid: id(n),
            taken_s: now - n as i64 * HOUR,
            backed_up: n >= 2,
        })
        .collect();
    let keep = |r: LocalRetention| r.keep(&pits, &calendar, None, now);
    assert!(keep(LocalRetention::default()).is_empty());
    assert_eq!(
        keep(LocalRetention {
            newest: 3,
            ..Default::default()
        }),
        BTreeSet::from([id(2), id(3), id(4)])
    );
    // Today's newest (now) and yesterday's 23:00, 11 hours ago.
    assert_eq!(
        keep(LocalRetention {
            days: 2,
            ..Default::default()
        }),
        BTreeSet::from([id(0), id(11)])
    );
    // The first PIT is 20 days ago at midnight, so weeks start on that
    // day: this week began 6 days ago, and the last one ended 6 days ago
    // at 23:00 (6 * 24 + 11 hours back).
    assert_eq!(
        keep(LocalRetention {
            weeks: 2,
            ..Default::default()
        }),
        BTreeSet::from([id(0), id(6 * 24 + 11)])
    );
}

#[test]
fn local_retention_claims_the_first_slot_of_a_kept_day_or_week() {
    let calendar = Calendar::utc();
    let days = LocalRetention {
        days: 1,
        ..Default::default()
    };
    let pits = [LocalPit {
        pit_uuid: id(1),
        taken_s: MONDAY + HOUR,
        backed_up: false,
    }];
    assert!(!days.claims_slot(&pits, &calendar, None, MONDAY + 2 * HOUR));
    assert!(days.claims_slot(&pits, &calendar, None, MONDAY + DAY));
    let weeks = LocalRetention {
        weeks: 1,
        ..Default::default()
    };
    assert!(!weeks.claims_slot(&pits, &calendar, None, MONDAY + 6 * DAY));
    assert!(weeks.claims_slot(&pits, &calendar, None, MONDAY + 7 * DAY));
    assert!(
        !LocalRetention {
            newest: 5,
            ..Default::default()
        }
        .claims_slot(&[], &calendar, None, MONDAY)
    );
}

#[test]
fn weakening_is_a_lower_count_another_month_or_yearly_turned_on() {
    let d = ApplianceRetention::default();
    assert!(!d.weakens(&d));
    assert!(ApplianceRetention { weekly: 3, ..d }.weakens(&d));
    assert!(!ApplianceRetention { monthly: 24, ..d }.weakens(&d));
    assert!(
        ApplianceRetention {
            month_days: 31,
            ..d
        }
        .weakens(&d)
    );
    let forever = ApplianceRetention { yearly: false, ..d };
    assert!(!forever.weakens(&d));
    assert!(d.weakens(&forever));
    assert!(
        !ApplianceRetention {
            monthly: 1,
            ..forever
        }
        .weakens(&forever)
    );
    assert!(d.validate().is_ok());
    assert!(
        ApplianceRetention {
            month_days: 27,
            ..d
        }
        .validate()
        .is_err()
    );
}
