// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

use bmac_core::SnapshotGuid;
use bmac_exec::{FakeRunner, Reply};

use super::*;
use crate::journal::{StoredPit, StoredVolume};

const CLUSTER: &str = "c1";
const DATASET: &str = "bk/bmac/clusters/c1/vms/v/L1/d";

const IDLE: &str = "  pool: bk
 state: ONLINE
  scan: none requested
config:

\tNAME          STATE     READ WRITE CKSUM
\tbk            ONLINE       0     0     0
\t  bmac-bk-u1  ONLINE       0     0     0

errors: No known data errors
";

const RUNNING: &str = "  pool: bk
 state: ONLINE
  scan: scrub in progress since Tue Oct 13 02:00:01 2026
\t1073741824 / 10737418240 scanned at 104857600/s, 536870912 / 10737418240 issued at 52428800/s
\t4096 repaired, 5.00% done, 00:03:14 to go
config:

\tNAME          STATE     READ WRITE CKSUM
\tbk            ONLINE       0     0     0

errors: No known data errors
";

const PAUSED: &str = "  pool: bk
 state: ONLINE
  scan: scrub paused since Tue Oct 13 02:01:00 2026
\tscrub started on Tue Oct 13 02:00:01 2026
\t0 / 10737418240 scanned, 536870912 / 10737418240 issued, 5.00% done
config:

errors: No known data errors
";

fn damaged_status() -> String {
    format!(
        "  pool: bk
 state: ONLINE
status: One or more devices has experienced an error resulting in data
\tcorruption.  Applications may be affected.
action: Restore the file in question if possible.
  scan: scrub repaired 8192 in 00:00:05 with 2 errors on Tue Oct 13 02:10:00 2026
config:

\tNAME          STATE     READ WRITE CKSUM
\tbk            ONLINE       0     0     2

errors: Permanent errors have been detected in the following files:

        {DATASET}@bmac-pit-2:<0x1>
        <metadata>:<0x3f>
"
    )
}

fn id(n: u128) -> Uuid {
    Uuid::from_u128(n)
}

fn pit(n: u128, dataset: &str) -> StoredPit {
    StoredPit {
        cluster_uuid: CLUSTER.into(),
        vm_uuid: id(7),
        branch_uuid: id(0xb),
        pit_uuid: id(n),
        parent: (n > 1).then(|| id(n - 1)),
        name: format!("bmac-pit-{n}"),
        taken_s: n as i64 * 3600,
        published_at_ms: 0,
        volumes: vec![StoredVolume {
            volume_uuid: id(0xd),
            dataset: dataset.into(),
            guid: SnapshotGuid::new(100 + n as u64),
            volsize: 1 << 20,
            volblocksize: 16384,
        }],
        bytes: 1,
        manifest_sha256: String::new(),
        received_from: None,
    }
}

#[test]
fn zpool_status_is_read_for_progress_repairs_and_damage() {
    assert_eq!(parse(IDLE).state, "none");
    let running = parse(RUNNING);
    assert_eq!(
        (
            running.state.as_str(),
            running.done_permille,
            running.repaired_bytes
        ),
        ("running", Some(50), 4096)
    );
    assert_eq!(parse(PAUSED).state, "paused");
    let done = parse(&damaged_status());
    assert_eq!(
        (
            done.state.as_str(),
            done.repaired_bytes,
            done.scan_errors,
            done.data_errors
        ),
        ("finished", 8192, 2, 2)
    );
    assert_eq!(
        damaged(&done.files),
        vec![(DATASET.to_string(), Some("bmac-pit-2".to_string()))]
    );
    assert_eq!(
        parse(
            "  scan: scrub canceled on Tue Oct 13 02:10:00 2026\nerrors: 3 data errors, use '-v' for a list\n"
        ),
        Observed {
            state: "canceled".into(),
            data_errors: 3,
            ..Default::default()
        }
    );
}

#[test]
fn a_damaged_snapshot_makes_it_and_every_later_pit_of_its_zvol_suspect() {
    let mut journal = Journal::new(id(1), "777");
    journal.pits = vec![
        pit(1, DATASET),
        pit(2, DATASET),
        pit(3, DATASET),
        pit(4, "bk/bmac/clusters/c1/vms/v/L2/d"),
    ];
    let named = suspects(&journal, &[(DATASET.into(), Some("bmac-pit-2".into()))]);
    let ids: Vec<Uuid> = named.iter().map(|(_, p, _)| *p).collect();
    assert_eq!(ids, vec![id(2), id(3)]);
    let whole = suspects(&journal, &[(DATASET.into(), None)]);
    assert_eq!(whole.len(), 3);
}

struct Rig {
    _dir: tempfile::TempDir,
    metadata: Metadata,
    audit: Audit,
    io: IoGate,
    fake: FakeRunner,
}

fn rig() -> Rig {
    let dir = tempfile::tempdir().unwrap();
    let metadata = Metadata::new(dir.path());
    let mut journal = Journal::new(id(1), "777");
    journal.pits = vec![pit(1, DATASET), pit(2, DATASET), pit(3, DATASET)];
    metadata.save(&journal).unwrap();
    Rig {
        audit: Audit::new(dir.path()),
        metadata,
        _dir: dir,
        io: IoGate::default(),
        fake: FakeRunner::new(),
    }
}

impl Rig {
    fn status(&self, text: &str) {
        self.fake
            .expect(ZPOOL, ["status", "-pv", "bk"], Reply::ok(text));
    }

    async fn tick(&self, now: i64) -> ScrubState {
        tick(&self.fake, "bk", &self.metadata, &self.audit, &self.io, now)
            .await
            .unwrap()
    }

    fn events(&self) -> Vec<String> {
        self.audit
            .recent(50)
            .unwrap()
            .into_iter()
            .map(|e| e.event)
            .collect()
    }
}

#[tokio::test]
async fn a_due_scrub_starts_and_waits_for_pulls_under_pulls_first() {
    let r = rig();
    {
        let _pull = r.io.start_pull();
        r.status(IDLE);
        let state = r.tick(1_000).await;
        assert!(
            state.last_started_ms.is_none(),
            "no scrub starts during a pull"
        );
    }
    r.status(IDLE);
    r.fake.expect(ZPOOL, ["scrub", "bk"], Reply::ok(""));
    let state = r.tick(2_000).await;
    assert_eq!(state.last_started_ms, Some(2_000));
    assert_eq!(state.report.as_ref().unwrap().state, "running");
    assert!(!r.io.pulls_wait(), "pulls first: pulls never wait");

    let pull = r.io.start_pull();
    r.status(RUNNING);
    r.fake.expect(ZPOOL, ["scrub", "-p", "bk"], Reply::ok(""));
    let state = r.tick(3_000).await;
    assert!(state.paused_for_pull);
    assert_eq!(state.report.as_ref().unwrap().state, "paused");
    drop(pull);
    r.status(PAUSED);
    r.fake.expect(ZPOOL, ["scrub", "bk"], Reply::ok(""));
    let state = r.tick(4_000).await;
    assert!(!state.paused_for_pull);
    assert_eq!(state.report.as_ref().unwrap().state, "running");

    r.status(
        IDLE.replace(
            "none requested",
            "scrub repaired 0 in 00:01:00 with 0 errors on Tue Oct 13 02:10:00 2026",
        )
        .as_str(),
    );
    let state = r.tick(5_000).await;
    let report = state.report.unwrap();
    assert_eq!(
        (
            report.state.as_str(),
            report.started_at_ms,
            report.finished_at_ms,
            report.errors
        ),
        ("finished", Some(2_000), Some(5_000), 0)
    );
    r.status(IDLE);
    let state = r.tick(5_000 + 29 * DAY_MS).await;
    assert_eq!(state.last_started_ms, Some(2_000), "not due for 30 days");
    r.fake.assert_done();
    assert_eq!(r.events(), vec!["scrub", "scrub_start"], "newest first");
}

#[tokio::test]
async fn unrepairable_errors_mark_pits_suspect_and_reach_the_check_in() {
    let r = rig();
    let mut platform = Platform::load(&r.metadata).unwrap();
    platform.io_policy = IoPolicy::ScrubFirst;
    platform.save(&r.metadata).unwrap();
    r.status(IDLE);
    r.fake.expect(ZPOOL, ["scrub", "bk"], Reply::ok(""));
    r.tick(1_000).await;
    assert!(r.io.pulls_wait(), "scrub first: pulls wait");
    r.status(&damaged_status());
    let state = r.tick(2_000).await;
    assert!(!r.io.pulls_wait());
    let report = state.report_for(CLUSTER).unwrap();
    assert_eq!(report.errors, 2);
    assert_eq!(report.affected, vec![format!("{DATASET}@bmac-pit-2")]);
    assert_eq!(report.suspect_pits, vec![id(2), id(3)]);
    assert!(state.report_for("other").unwrap().suspect_pits.is_empty());
    r.fake.assert_done();
    let audit = r.audit.recent(5).unwrap();
    let scrub = audit.iter().find(|e| e.event == "scrub").unwrap();
    assert_eq!(scrub.details["errors"], 2);
}
