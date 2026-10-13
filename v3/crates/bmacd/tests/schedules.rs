// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

//! Schedules evaluated by several hosts at once, on the shared job records
//! of [`harness::Lab`], with a clock the test moves.

mod harness;

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Barrier};

use bmac_proto::api::{ErrorCode, Role};
use bmacd::jobs::Caller;
use bmacd::schedules::{
    Outcome, RECORD_SCHEMA, Recurrence, Schedule, Scheduler, SchedulerConfig, Schedules,
};
use harness::Lab;
use jiff::tz::TimeZone;

const ADMIN: Caller = Caller {
    user: "ada@pve",
    role: Role::Admin,
};
const MINUTE: i64 = 60_000;
/// A quarter-hour boundary: 2026-10-11T12:00:00Z.
const SLOT: i64 = 1_791_720_000_000;

fn schedule(id: &str, minutes: u32) -> Schedule {
    Schedule {
        schema_version: RECORD_SCHEMA,
        record_type: "schedule".into(),
        revision: 0,
        id: id.into(),
        title: format!("{id} every {minutes} minutes"),
        operation: "test.long".parse().unwrap(),
        parameters: BTreeMap::new(),
        every: Recurrence::Interval { minutes },
        enabled: true,
        run_as: Role::Operator,
        updated_by: "ada@pve".into(),
        updated_at_ms: 0,
    }
}

struct Clock(Arc<AtomicI64>);

impl Clock {
    fn set(&self, ms: i64) {
        self.0.store(ms, Ordering::SeqCst);
    }
}

fn schedules(lab: &Lab) -> Schedules {
    Schedules::new(
        lab.pmxcfs.clone(),
        lab.dir.path().join("pve/priv/bmac/schedules"),
    )
}

fn scheduler(lab: &Lab, n: u8, clock: &Clock) -> Scheduler {
    let now = clock.0.clone();
    Scheduler::new(
        schedules(lab),
        lab.jobs(n),
        SchedulerConfig {
            every: std::time::Duration::from_secs(30),
            grace_ms: 15 * MINUTE,
            zone: TimeZone::UTC,
            clock: Arc::new(move || now.load(Ordering::SeqCst)),
        },
    )
}

#[test]
fn hosts_evaluating_the_same_schedule_produce_one_job() {
    let lab = Lab::new(&[1, 2, 3]);
    let clock = Clock(Arc::new(AtomicI64::new(SLOT - 30 * MINUTE)));
    let hosts: Vec<Arc<Scheduler>> = (1..=3)
        .map(|n| Arc::new(scheduler(&lab, n, &clock)))
        .collect();
    schedules(&lab).save(schedule("sweep", 60), None).unwrap();

    clock.set(SLOT + MINUTE);
    let barrier = Arc::new(Barrier::new(hosts.len()));
    let threads: Vec<_> = hosts
        .iter()
        .map(|host| {
            let (host, barrier) = (host.clone(), barrier.clone());
            std::thread::spawn(move || {
                barrier.wait();
                host.evaluate()
            })
        })
        .collect();
    let outcomes: Vec<Outcome> = threads
        .into_iter()
        .flat_map(|t| t.join().unwrap())
        .map(|(id, outcome)| {
            assert_eq!(id, "sweep");
            outcome
        })
        .collect();
    let admitted: Vec<_> = outcomes
        .iter()
        .filter_map(|o| match o {
            Outcome::Admitted(id) => Some(*id),
            _ => None,
        })
        .collect();
    assert_eq!(admitted.len(), 1, "{outcomes:?}");
    assert!(
        outcomes.iter().all(
            |o| matches!(o, Outcome::Admitted(id) | Outcome::Existing(id) if *id == admitted[0])
        ),
        "{outcomes:?}"
    );
    let jobs = lab.jobs(1).list().unwrap();
    assert_eq!(jobs.len(), 1);
    assert_eq!(jobs[0].user, "schedule:sweep");
    assert_eq!(jobs[0].idempotency_key, "schedule:sweep:20261011T1200Z");
    assert_eq!(jobs[0].role, Role::Operator);
    assert!(
        hosts[0].evaluate().is_empty(),
        "a handled slot isn't tried again"
    );

    // The next slot waits for the first job's lock, then starts once it is
    // free.
    clock.set(SLOT + 61 * MINUTE);
    assert!(matches!(
        &hosts[1].evaluate()[..],
        [(_, Outcome::Waiting(_))]
    ));
    lab.jobs(1).cancel(ADMIN, admitted[0], false).unwrap();
    clock.set(SLOT + 62 * MINUTE);
    let second = hosts[1].evaluate();
    assert!(
        matches!(&second[..], [(_, Outcome::Admitted(_))]),
        "{second:?}"
    );
    let Outcome::Admitted(second) = second[0].1 else {
        unreachable!()
    };
    assert!(matches!(&hosts[2].evaluate()[..], [(_, Outcome::Existing(id))] if *id == second));

    // Without quorum the slot waits, and past its grace it is missed.
    lab.jobs(1).cancel(ADMIN, second, false).unwrap();
    lab.cluster.0.lock().unwrap().quorate = false;
    clock.set(SLOT + 121 * MINUTE);
    let waiting = hosts[0].evaluate();
    assert!(
        matches!(&waiting[..], [(_, Outcome::Waiting(m))] if m.contains("quorum")),
        "{waiting:?}"
    );
    clock.set(SLOT + 136 * MINUTE);
    assert_eq!(hosts[0].evaluate(), vec![("sweep".into(), Outcome::Missed)]);
    assert!(hosts[0].evaluate().is_empty());
    assert_eq!(lab.jobs(1).list().unwrap().len(), 2);
}

#[test]
fn disabled_edited_and_refused_schedules() {
    let lab = Lab::new(&[1, 2]);
    let clock = Clock(Arc::new(AtomicI64::new(SLOT + MINUTE)));
    let store = schedules(&lab);

    let mut off = schedule("off", 15);
    off.enabled = false;
    store.save(off, None).unwrap();
    let mut edited = schedule("edited", 15);
    edited.updated_at_ms = SLOT + 30_000;
    store.save(edited, None).unwrap();
    let mut viewer = schedule("viewer", 15);
    viewer.run_as = Role::Viewer;
    store.save(viewer, None).unwrap();

    let host = scheduler(&lab, 1, &clock);
    let outcomes = host.evaluate();
    assert!(
        matches!(&outcomes[..], [(id, Outcome::Refused(m))] if id == "viewer" && m.contains("role")),
        "only the viewer schedule's slot is due, and it is refused: {outcomes:?}"
    );
    assert!(
        host.evaluate().is_empty(),
        "a refused slot isn't tried again"
    );
    assert!(lab.jobs(1).list().unwrap().is_empty());

    // A host that starts after a slot's grace has passed doesn't call it
    // missed: another host was up to run it.
    let mut late = schedule("late", 60);
    late.run_as = Role::Operator;
    store.save(late, None).unwrap();
    clock.set(SLOT + 20 * MINUTE);
    let fresh = scheduler(&lab, 2, &clock);
    let outcomes = fresh.evaluate();
    assert!(outcomes.iter().all(|(id, _)| id != "late"), "{outcomes:?}");
}

#[test]
fn schedule_records_are_checked_and_revisioned() {
    let lab = Lab::new(&[1]);
    let store = schedules(&lab);
    let saved = store.save(schedule("pit-prod1", 60), None).unwrap();
    assert_eq!(saved.revision, 1);
    let again = store.save(schedule("pit-prod1", 60), None).unwrap_err();
    assert_eq!(again.code, ErrorCode::Conflict);

    let mut changed = saved.clone();
    changed.title = "hourly".into();
    let changed = store.save(changed, Some(1)).unwrap();
    assert_eq!(changed.revision, 2);
    assert_eq!(store.get("pit-prod1").unwrap().unwrap(), changed);
    assert_eq!(
        store.save(saved.clone(), Some(1)).unwrap_err().code,
        ErrorCode::Conflict,
        "a stale revision is refused"
    );

    let mut bad = schedule("Bad_ID", 60);
    assert_eq!(
        store.save(bad.clone(), None).unwrap_err().code,
        ErrorCode::Invalid
    );
    bad.id = "bad".into();
    bad.every = Recurrence::Daily { at: "25:00".into() };
    assert_eq!(store.save(bad, None).unwrap_err().code, ErrorCode::Invalid);

    std::fs::write(
        lab.dir.path().join("pve/priv/bmac/schedules/other.json"),
        serde_json::to_vec(&changed).unwrap(),
    )
    .unwrap();
    let ids: Vec<String> = store.list().unwrap().into_iter().map(|s| s.id).collect();
    assert_eq!(
        ids,
        ["pit-prod1"],
        "a record under another schedule's name is ignored"
    );

    assert_eq!(
        store.delete("pit-prod1", 1).unwrap_err().code,
        ErrorCode::Conflict
    );
    store.delete("pit-prod1", 2).unwrap();
    assert!(store.get("pit-prod1").unwrap().is_none());
    assert_eq!(
        store.delete("pit-prod1", 2).unwrap_err().code,
        ErrorCode::NotFound
    );
}
