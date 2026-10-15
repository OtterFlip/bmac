// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

use bmac_core::SnapshotGuid;

use super::*;
use crate::fake_pool::{FakePool, Zvol};
use crate::journal::{Quarantined, Receiving, StoredVolume};

const CLUSTER: &str = "c1";
/// 2026-10-12T00:00:00Z, a Monday.
const MONDAY: i64 = 1_791_763_200;
const DAY: i64 = 86_400;
const DATASET: &str = "bk/bmac/clusters/c1/vms/v/b/d";

fn id(n: u128) -> Uuid {
    Uuid::from_u128(n)
}

fn weak(daily: u32) -> ApplianceRetention {
    ApplianceRetention {
        daily,
        weekly: 0,
        monthly: 0,
        ..ApplianceRetention::default()
    }
}

/// PIT `n` of one VM on one branch, taken `n` days after Monday at noon
/// and received on top of PIT `n - 1`.
fn pit(n: u128) -> StoredPit {
    StoredPit {
        cluster_uuid: CLUSTER.into(),
        vm_uuid: id(0x7),
        branch_uuid: id(0xb),
        pit_uuid: id(n),
        parent: (n > 1).then(|| id(n - 1)),
        name: format!("bmac-pit-{n}"),
        taken_s: MONDAY + n as i64 * DAY + DAY / 2,
        published_at_ms: 0,
        volumes: vec![StoredVolume {
            volume_uuid: id(0xd),
            dataset: DATASET.into(),
            guid: SnapshotGuid::new(100 + n as u64),
            volsize: 1 << 20,
            volblocksize: 16384,
        }],
        bytes: 1,
        manifest_sha256: String::new(),
        received_from: (n > 1).then(|| id(n - 1)),
    }
}

struct Rig {
    _dir: tempfile::TempDir,
    metadata: Metadata,
    audit: Audit,
    pool: FakePool,
}

/// PITs 1..=count published, each snapshot held, each manifest written.
fn rig(count: u128, policy: ApplianceRetention) -> Rig {
    let dir = tempfile::tempdir().unwrap();
    let metadata = Metadata::new(dir.path());
    let pool = FakePool::new();
    let mut journal = Journal::new(id(1), "4242");
    let mut zvol = Zvol {
        volsize: 1 << 20,
        volblocksize: 16384,
        ..Zvol::default()
    };
    for n in 1..=count {
        let p = pit(n);
        zvol.snapshots.push((p.name.clone(), p.volumes[0].guid));
        pool.holds
            .lock()
            .unwrap()
            .insert((RETAIN_TAG.into(), p.snapshot(&p.volumes[0])));
        metadata.write_manifest(CLUSTER, &p.pit_uuid, "{}").unwrap();
        journal.pits.push(p);
    }
    pool.zvols.lock().unwrap().insert(DATASET.into(), zvol);
    metadata.save(&journal).unwrap();
    Settings {
        policy,
        floor: weak(0),
        ..Settings::default()
    }
    .save(&metadata)
    .unwrap();
    Rig {
        audit: Audit::new(dir.path()),
        _dir: dir,
        metadata,
        pool,
    }
}

/// The start of day 7, the day after PIT 6 and before PITs 7 and 8.
fn now() -> i64 {
    (MONDAY + 7 * DAY) * 1000
}

#[test]
fn weakening_waits_out_the_delay_unless_confirmed_and_never_passes_the_floor() {
    let mut s = Settings::default();
    assert_eq!(s.policy, ApplianceRetention::default());
    let stronger = ApplianceRetention {
        daily: 14,
        ..ApplianceRetention::default()
    };
    assert_eq!(s.change(stronger, "admin", 0, false), Ok(Change::Applied));
    assert_eq!(
        s.change(ApplianceRetention::default(), "admin", 1000, false),
        Ok(Change::Delayed {
            effective_at_ms: 1000 + 72 * 3_600_000
        })
    );
    assert_eq!(s.policy, stronger);
    assert!(s.settle(1000 + 72 * 3_600_000 - 1).is_none());
    assert_eq!(
        s.settle(1000 + 72 * 3_600_000).map(|p| p.by),
        Some("admin".to_string())
    );
    assert_eq!(s.policy, ApplianceRetention::default());

    assert_eq!(s.change(weak(2), "root", 0, true), Ok(Change::Applied));
    assert!(s.pending.is_none());
    assert!(
        s.change(weak(0), "root", 0, true)
            .unwrap_err()
            .contains("floor")
    );
    assert!(s.set_floor(weak(3)).is_err());
    s.set_floor(weak(2)).unwrap();
    assert!(s.change(weak(1), "admin", 0, false).is_err());
}

#[test]
fn a_plan_keeps_bucket_winners_branch_roots_holds_and_transfer_bases() {
    let mut journal = Journal::new(id(1), "g");
    journal.pits = (1..=8).map(pit).collect();
    let mut settings = Settings {
        policy: weak(3),
        ..Settings::default()
    };
    settings.hold(CLUSTER, id(4), "admin", 0);
    journal.receiving.push(Receiving {
        cluster_uuid: CLUSTER.into(),
        vm_uuid: id(0x7),
        branch_uuid: id(0xb),
        pit_uuid: id(9),
        base: Some(id(2)),
        started_at_ms: 0,
        volumes: vec![],
    });
    let pins = BTreeMap::from([(
        (CLUSTER.to_string(), id(5)),
        BTreeSet::from([KeepReason::RestoreGrant]),
    )]);
    let plan = plan(&journal, &settings, &Zones::default(), &pins, now());
    let pruned: Vec<Uuid> = plan.prune.iter().map(|d| d.pit_uuid).collect();
    assert_eq!(pruned, [id(3)]);
    let why = |n| {
        plan.keep
            .iter()
            .find(|k| k.pit_uuid == id(n))
            .map(|k| k.reasons.clone())
            .unwrap_or_default()
    };
    assert!(why(1).contains(&KeepReason::BranchRoot));
    assert!(why(2).contains(&KeepReason::Transfer));
    assert!(why(4).contains(&KeepReason::Hold));
    assert!(why(5).contains(&KeepReason::RestoreGrant));
    assert!(why(6).contains(&KeepReason::Daily));
    assert!(why(8).contains(&KeepReason::Latest));
    assert!(why(8).contains(&KeepReason::BranchHead));
    assert_eq!(plan.coverage[0].pits, 7);
    assert_eq!(plan.prune[0].snapshots, [format!("{DATASET}@bmac-pit-3")]);
}

#[tokio::test]
async fn a_prune_releases_and_destroys_only_what_the_plan_names() {
    let r = rig(6, weak(2));
    let report = prune(&r.pool, &r.metadata, &r.audit, &BTreeMap::new(), now())
        .await
        .unwrap();
    // PIT 1 is the branch root; 5 and 6 win the two newest days.
    assert_eq!(report.pruned, [id(2), id(3), id(4)]);
    assert_eq!(report.reclaim_bytes, 3000);
    let left: Vec<String> = r
        .pool
        .zvol(DATASET)
        .unwrap()
        .snapshots
        .into_iter()
        .map(|s| s.0)
        .collect();
    assert_eq!(left, ["bmac-pit-1", "bmac-pit-5", "bmac-pit-6"]);
    let holds = r.pool.holds.lock().unwrap().clone();
    assert_eq!(holds.len(), 3);
    assert!(!holds.iter().any(|(_, s)| s.ends_with("bmac-pit-2")));
    let journal = r.metadata.load().unwrap().unwrap();
    assert_eq!(journal.revision, 1);
    assert_eq!(journal.pits.len(), 3);
    assert!(r.metadata.manifest(CLUSTER, &id(2)).unwrap().is_none());
    assert!(r.metadata.manifest(CLUSTER, &id(5)).unwrap().is_some());
    assert_eq!(
        journal.pit(CLUSTER, &id(5)).unwrap().parent,
        Some(id(1)),
        "PIT 5 descends from the nearest PIT left"
    );
    let entry = &r.audit.recent(1).unwrap()[0];
    assert_eq!(entry.event, "prune");
    assert_eq!(entry.details["kept"], 3);

    let again = prune(&r.pool, &r.metadata, &r.audit, &BTreeMap::new(), now())
        .await
        .unwrap();
    assert!(again.pruned.is_empty());
    assert_eq!(r.metadata.load().unwrap().unwrap().revision, 1);
}

#[tokio::test]
async fn a_snapshot_that_changed_or_has_clones_stays_with_its_hold() {
    let r = rig(4, weak(1));
    // PIT 2's snapshot isn't the one the journal recorded any more.
    r.pool
        .zvols
        .lock()
        .unwrap()
        .get_mut(DATASET)
        .unwrap()
        .snapshots[1]
        .1 = SnapshotGuid::new(999);
    // PIT 3's snapshot is the origin of a zvol the journal doesn't know.
    r.pool.zvols.lock().unwrap().insert(
        "bk/bmac/clusters/c1/vms/v/x/d".into(),
        Zvol {
            origin: Some(format!("{DATASET}@bmac-pit-3")),
            ..Zvol::default()
        },
    );
    let report = prune(&r.pool, &r.metadata, &r.audit, &BTreeMap::new(), now())
        .await
        .unwrap();
    assert!(report.pruned.is_empty());
    let skipped: Vec<Uuid> = report.skipped.iter().map(|s| s.0).collect();
    assert_eq!(skipped, [id(2), id(3)]);
    assert!(report.skipped[0].1.contains("isn't the snapshot"));
    assert!(report.skipped[1].1.contains("dependent clones"));
    assert!(
        r.pool
            .holds
            .lock()
            .unwrap()
            .contains(&(RETAIN_TAG.into(), format!("{DATASET}@bmac-pit-3")))
    );
    assert_eq!(r.metadata.load().unwrap().unwrap().pits.len(), 4);
}

#[tokio::test]
async fn an_emptied_lineage_and_old_quarantines_go() {
    let r = rig(2, weak(1));
    let mut journal = r.metadata.load().unwrap().unwrap();
    // A reseed left PIT 1 alone in its own lineage.
    let old = "bk/bmac/clusters/c1/vms/v/b-0/d";
    journal.pits[0].volumes[0].dataset = old.into();
    journal.pits[0].received_from = None;
    journal.pits[1].received_from = None;
    journal.pits[1].branch_uuid = id(0xb);
    journal.quarantined = vec![
        Quarantined {
            cluster_uuid: CLUSTER.into(),
            vm_uuid: id(0x7),
            pit_uuid: id(0x51),
            reason: "bad".into(),
            at_ms: now() - QUARANTINE_KEEP_MS,
            datasets: vec!["bk/bmac/clusters/c1/vms/v/q/d".into()],
        },
        Quarantined {
            cluster_uuid: CLUSTER.into(),
            vm_uuid: id(0x7),
            pit_uuid: id(0x52),
            reason: "bad".into(),
            at_ms: now() - 1000,
            datasets: vec!["bk/bmac/clusters/c1/vms/v/r/d".into()],
        },
    ];
    r.metadata.save(&journal).unwrap();
    {
        let mut zvols = r.pool.zvols.lock().unwrap();
        let mut moved = zvols.get_mut(DATASET).unwrap().snapshots.remove(0);
        moved.0 = "bmac-pit-1".into();
        zvols.insert(
            old.into(),
            Zvol {
                snapshots: vec![moved],
                ..Zvol::default()
            },
        );
    }
    r.pool
        .holds
        .lock()
        .unwrap()
        .insert((RETAIN_TAG.into(), format!("{old}@bmac-pit-1")));
    let report = prune(&r.pool, &r.metadata, &r.audit, &BTreeMap::new(), now())
        .await
        .unwrap();
    // PIT 2 reseeded the branch, so PIT 1's lineage expires with it.
    assert_eq!(report.pruned, [id(1)]);
    assert_eq!(report.zvols, [old.to_string()]);
    assert!(r.pool.zvol(old).is_none());
    assert_eq!(report.quarantine, [id(0x51)]);
    let journal = r.metadata.load().unwrap().unwrap();
    assert_eq!(journal.quarantined.len(), 1);
    assert!(
        r.pool
            .destroyed
            .lock()
            .unwrap()
            .contains(&"-r bk/bmac/clusters/c1/vms/v/q/d".to_string())
    );
}

#[test]
fn a_clusters_new_time_zone_waits_out_the_delay() {
    let mut zones = Zones::default();
    let hour = 3_600_000;
    assert_eq!(zones.zone(CLUSTER, 0), TimeZone::UTC);
    assert!(zones.report(CLUSTER, "America/Denver", 72, 0).unwrap());
    let denver = TimeZone::get("America/Denver").unwrap();
    assert_eq!(zones.zone(CLUSTER, 0), denver);
    assert!(!zones.report(CLUSTER, "America/Denver", 72, hour).unwrap());

    assert!(zones.report(CLUSTER, "Europe/Berlin", 72, hour).unwrap());
    assert!(
        !zones
            .report(CLUSTER, "Europe/Berlin", 72, 2 * hour)
            .unwrap()
    );
    assert_eq!(zones.zone(CLUSTER, 73 * hour - 1), denver);
    let berlin = TimeZone::get("Europe/Berlin").unwrap();
    assert_eq!(zones.zone(CLUSTER, 73 * hour), berlin);
    assert!(
        zones
            .report(CLUSTER, "Europe/Berlin", 72, 73 * hour)
            .unwrap()
    );
    assert_eq!(zones.clusters[CLUSTER].zone, "Europe/Berlin");
    assert!(zones.clusters[CLUSTER].pending.is_none());

    // Reporting the old zone again before the delay ends cancels the move.
    assert!(zones.report(CLUSTER, "Asia/Tokyo", 72, 74 * hour).unwrap());
    assert!(
        zones
            .report(CLUSTER, "Europe/Berlin", 72, 75 * hour)
            .unwrap()
    );
    assert!(zones.clusters[CLUSTER].pending.is_none());
    assert!(zones.report(CLUSTER, "Mars/Olympus", 72, 0).is_err());
}

#[tokio::test]
async fn a_prune_records_each_vms_start_before_it_deletes_anything() {
    let r = rig(6, weak(2));
    prune(&r.pool, &r.metadata, &r.audit, &BTreeMap::new(), now())
        .await
        .unwrap();
    let settings = Settings::load(&r.metadata).unwrap();
    assert_eq!(settings.start(CLUSTER, &id(0x7)), Some(pit(1).taken_s));
}
