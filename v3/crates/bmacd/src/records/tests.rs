// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

use super::retention::{self, DAY_MS, Policy};
use super::search::search;
use super::*;
use crate::db::Database;
use crate::store::Entry;
use bmac_core::JobId;
use bmac_core::job::{EventBody, JobEvent, LogLevel};
use bmac_proto::logs::{LogKind, LogQuery, LogSeverity};
use bmac_proto::peer::Record;

async fn open(dir: &tempfile::TempDir, origin: &str) -> Store {
    let db = Database::open(&dir.path().join(format!("{origin}.db"))).unwrap();
    Store::open(db, origin, 1_000).await.unwrap()
}

fn event(job: JobId, seq: u64, at_ms: i64, body: EventBody) -> Entry {
    Entry::job_event(&JobEvent {
        job,
        seq,
        at_ms,
        body,
    })
}

fn output(job: JobId, seq: u64, at_ms: i64, text: &str) -> Entry {
    event(
        job,
        seq,
        at_ms,
        EventBody::Output {
            stream: None,
            level: LogLevel::Info,
            text: text.to_string(),
            source: None,
        },
    )
}

async fn own(store: &Store) -> Vec<Record> {
    store
        .records_after(store.origin(), store.stream(), 0, 10_000)
        .await
        .unwrap()
}

#[tokio::test]
async fn holes_are_found_and_filled_from_any_order() {
    let dir = tempfile::tempdir().unwrap();
    let mox1 = open(&dir, "mox1").await;
    let mox2 = open(&dir, "mox2").await;
    mox1.append(
        (1..=10)
            .map(|n| Entry::log(n, "info", &format!("line {n}")))
            .collect(),
    )
    .await
    .unwrap();
    let records = own(&mox1).await;
    let stream = mox1.stream().to_string();
    let pick = |seqs: &[u64]| -> Vec<Record> {
        records
            .iter()
            .filter(|r| seqs.contains(&r.seq))
            .cloned()
            .collect()
    };
    mox2.insert_copies(pick(&[2, 3, 6, 9])).await.unwrap();
    assert_eq!(
        missing(&mox2, "mox1", &stream, 1, 10).await.unwrap(),
        [(1, 1), (4, 5), (7, 8), (10, 10)]
    );
    let held = origin_holdings(&mox2).await.unwrap();
    let mox1_held = held.iter().find(|h| h.origin == "mox1").unwrap();
    assert_eq!(
        (mox1_held.records, mox1_held.gaps, mox1_held.last_seq),
        (4, 4, 9)
    );
    assert_eq!(mox1_held.latest_stream.as_deref(), Some(stream.as_str()));

    mox2.insert_copies(pick(&[10, 1, 5, 4, 8, 7]))
        .await
        .unwrap();
    assert!(
        missing(&mox2, "mox1", &stream, 1, 10)
            .await
            .unwrap()
            .is_empty()
    );
    let stream_held = holdings(&mox2)
        .await
        .unwrap()
        .into_iter()
        .find(|h| h.origin == "mox1")
        .unwrap();
    assert_eq!(
        (stream_held.min_seq, stream_held.max_seq, stream_held.count),
        (1, 10, 10)
    );
}

#[tokio::test]
async fn search_filters_and_pages_over_several_origins() {
    let dir = tempfile::tempdir().unwrap();
    let mox1 = open(&dir, "mox1").await;
    let mox2 = open(&dir, "mox2").await;
    let job = JobId::random();
    let mut entries = vec![
        Entry::audit(100, "login", Some("admin@pam"), None, "signed in"),
        Entry::journal(110, "corosync.service", 3, "link 0 down", false),
        Entry::journal(120, "zfs-zed.service", 4, "pool degraded", false),
    ];
    let pad = "-".repeat(2000);
    entries.extend(
        (1..=40).map(|n| output(job, n, 200 + n as i64, &format!("copying block {n} {pad}"))),
    );
    entries.push(event(
        job,
        41,
        300,
        EventBody::Failed {
            code: None,
            message: "Disk Vanished".into(),
            details: None,
        },
    ));
    mox1.append(entries).await.unwrap();
    mox2.append(vec![Entry::log(150, "warning", "peer mox3 unreachable")])
        .await
        .unwrap();
    mox2.insert_copies(own(&mox1).await).await.unwrap();

    let query = |q: LogQuery| {
        let mox2 = mox2.clone();
        async move { search(&mox2, q).await.unwrap() }
    };
    let all = query(LogQuery::default()).await;
    assert_eq!(all.records.len(), 45);
    assert!(all.next.is_none());
    assert!(
        all.records.windows(2).all(|w| w[0].time_ms >= w[1].time_ms),
        "newest first"
    );

    let warnings = query(LogQuery {
        min_severity: Some(LogSeverity::Warning),
        ..LogQuery::default()
    })
    .await;
    let summaries: Vec<&str> = warnings
        .records
        .iter()
        .map(|r| r.summary.as_str())
        .collect();
    assert_eq!(
        summaries,
        [
            "failed: Disk Vanished",
            "peer mox3 unreachable",
            "zfs-zed.service: pool degraded",
            "corosync.service: link 0 down"
        ]
    );
    let from_mox2 = query(LogQuery {
        host: Some("mox2".parse().unwrap()),
        ..LogQuery::default()
    })
    .await;
    assert_eq!(from_mox2.records.len(), 1);
    let unit = query(LogQuery {
        unit: Some("corosync.service".into()),
        ..LogQuery::default()
    })
    .await;
    assert_eq!(unit.records.len(), 1);
    assert_eq!(unit.records[0].kind, LogKind::Journal);
    let audit = query(LogQuery {
        kinds: vec![LogKind::Audit],
        ..LogQuery::default()
    })
    .await;
    assert_eq!(audit.records[0].summary, "login by admin@pam: signed in");
    let window = query(LogQuery {
        from_ms: Some(110),
        to_ms: Some(150),
        ..LogQuery::default()
    })
    .await;
    assert_eq!(window.records.len(), 3);

    assert!(
        mox2.compact_job(job).await.unwrap() > 0,
        "the copied output is compacted"
    );
    let mut text = Vec::new();
    let mut before = None;
    loop {
        let page = query(LogQuery {
            job: Some(job),
            text: Some("BLOCK 1".into()),
            limit: Some(4),
            before: before.clone(),
            ..LogQuery::default()
        })
        .await;
        text.extend(page.records.into_iter().map(|r| r.seq));
        match page.next {
            Some(next) => before = Some(next),
            None => break,
        }
    }
    let expected: Vec<u64> = (1..=40)
        .filter(|n| format!("copying block {n}").contains("block 1"))
        .map(|n| n + 3)
        .rev()
        .collect();
    assert_eq!(
        text, expected,
        "pages cover the matches once each, newest first"
    );
    assert!(matches!(
        search(
            &mox2,
            LogQuery {
                before: Some("garbage".into()),
                ..LogQuery::default()
            }
        )
        .await,
        Err(super::search::SearchError::Cursor)
    ));
}

#[tokio::test]
async fn eviction_follows_the_class_order_and_flags_audit_loss() {
    let dir = tempfile::tempdir().unwrap();
    let mox1 = open(&dir, "mox1").await;
    let done = JobId::random();
    let failed = JobId::random();
    let big = "x".repeat(1000);
    let mut entries = Vec::new();
    for n in 1..=5 {
        entries.push(output(done, n, 10 + n as i64, &big));
    }
    entries.push(event(done, 6, 20, EventBody::Completed { message: None }));
    for n in 1..=5 {
        entries.push(output(failed, n, 30 + n as i64, &big));
    }
    entries.push(event(
        failed,
        6,
        40,
        EventBody::Failed {
            code: None,
            message: big.clone(),
            details: None,
        },
    ));
    entries.push(Entry::journal(50, "corosync.service", 3, &big, false));
    entries.push(Entry::audit(5, "login", Some("root@pam"), None, &big));
    mox1.append(entries).await.unwrap();

    let (used, _, _) = retention::usage(&mox1).await.unwrap();
    let policy = |budget_bytes| Policy {
        budget_bytes,
        ..Policy::default()
    };
    let now = 1_000;
    let report = retention::enforce(&mox1, policy(used - 4500), now, None)
        .await
        .unwrap();
    assert_eq!(report.evicted, [5, 0, 0, 0], "successful output goes first");
    let report = retention::enforce(&mox1, policy(used - 10_000), now, None)
        .await
        .unwrap();
    assert_eq!(
        report.evicted,
        [0, 6, 0, 0],
        "then other job events, oldest first"
    );
    assert!(report.audit_evicted_ms.is_none());
    let report = retention::enforce(&mox1, policy(2500), now, None)
        .await
        .unwrap();
    assert_eq!(report.evicted, [0, 0, 1, 0], "then the journal");
    let report = retention::enforce(&mox1, policy(1000), now, None)
        .await
        .unwrap();
    assert_eq!(report.evicted, [0, 0, 0, 2], "audit and failures last");
    assert_eq!(report.audit_evicted_ms, Some(now));
    assert!(report.used_bytes <= 1000);

    let tombstones: Vec<Record> = own(&mox1).await.into_iter().filter(|r| r.evicted).collect();
    assert_eq!(tombstones.len(), 14);
    assert!(tombstones.iter().all(|r| r.body.is_empty()));
    let mox2 = open(&dir, "mox2").await;
    let all = own(&mox1).await;
    assert_eq!(
        mox2.insert_copies(all.clone()).await.unwrap().len(),
        all.len()
    );
    let stream = mox1.stream().to_string();
    assert!(
        missing(&mox2, "mox1", &stream, 1, all.len() as u64)
            .await
            .unwrap()
            .is_empty(),
        "evicted records are held as such, so never fetched again"
    );
    let shown = search(&mox2, LogQuery::default()).await.unwrap();
    assert_eq!(shown.records.iter().filter(|r| r.evicted).count(), 14);
}

#[tokio::test]
async fn aged_records_are_deleted_and_never_copied_back() {
    let dir = tempfile::tempdir().unwrap();
    let mox1 = open(&dir, "mox1").await;
    let mox2 = open(&dir, "mox2").await;
    mox1.append(
        (1..=6)
            .map(|day| Entry::log(day * DAY_MS, "info", &format!("day {day}")))
            .collect(),
    )
    .await
    .unwrap();
    let all = own(&mox1).await;
    mox2.insert_copies(all.clone()).await.unwrap();
    let policy = Policy {
        keep_days: 2,
        ..Policy::default()
    };
    let report = retention::enforce(&mox2, policy, 6 * DAY_MS, None)
        .await
        .unwrap();
    assert_eq!(report.aged, 3, "days 1 to 3 are older than two days");
    assert_eq!(report.oldest_ms, Some(4 * DAY_MS));
    assert_eq!(
        mox2.insert_copies(all.clone()).await.unwrap().len(),
        0,
        "nothing at or below the floor is stored again"
    );
    let stream = mox1.stream().to_string();
    assert!(
        missing(&mox2, "mox1", &stream, 1, 6)
            .await
            .unwrap()
            .is_empty()
    );

    let mox3 = open(&dir, "mox3").await;
    retention::enforce(&mox3, policy, 6 * DAY_MS, None)
        .await
        .unwrap();
    let stored = mox3.insert_copies(all).await.unwrap();
    assert_eq!(
        stored.iter().map(|r| r.seq).collect::<Vec<_>>(),
        [4, 5, 6],
        "copies retention would delete are skipped"
    );
}
