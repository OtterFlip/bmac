// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

//! Log copying between three simulated hosts (`MONITORING.md`, "Copying
//! logs between hosts"): live copies, catch-up for every origin, and gap
//! filling, over real TLS on loopback.

mod harness;

use std::time::Duration;

use bmac_proto::logs::LogQuery;
use bmacd::records::{self, search::search};
use bmacd::store::{Entry, Store};
use harness::{Lab, host};

async fn eventually<F: std::future::Future<Output = bool>>(
    what: &str,
    mut cond: impl FnMut() -> F,
) {
    for _ in 0..300 {
        if cond().await {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("timed out waiting for {what}");
}

fn lines(prefix: &str, n: i64) -> Vec<Entry> {
    (1..=n)
        .map(|i| Entry::log(10_000 + i, "info", &format!("{prefix} {i}")))
        .collect()
}

async fn held(store: &Store, origin: &str) -> u64 {
    records::origin_holdings(store)
        .await
        .unwrap()
        .into_iter()
        .find(|h| h.origin == origin)
        .map_or(0, |h| h.records)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_dead_hosts_records_stay_readable_and_reach_a_rebuilt_host() {
    let lab = Lab::new(&[1, 2, 3]);
    let mut mox1 = lab.node(1).await;
    let mut mox2 = lab.node(2).await;
    let mox3 = lab.node(3).await;
    mox3.store.append(lines("from mox3", 50)).await.unwrap();
    mox2.store.append(lines("from mox2", 20)).await.unwrap();
    mox1.link();
    mox2.link();
    eventually("mox3's records on mox1 and mox2", || async {
        held(&mox1.store, "mox3").await == 50 && held(&mox2.store, "mox3").await == 50
    })
    .await;

    drop(mox3);
    let found = search(
        &mox1.store,
        LogQuery {
            host: Some(host(3)),
            text: Some("FROM MOX3 7".into()),
            ..LogQuery::default()
        },
    )
    .await
    .unwrap();
    assert_eq!(found.records.len(), 1, "mox3 is gone, its records are not");
    assert_eq!(found.records[0].summary, "from mox3 7");

    // mox2 is rebuilt with an empty disk: mox3 is still dead, so its
    // records can only come from mox1, and mox2's own old ones come back.
    eventually("mox2's records on mox1", || async {
        held(&mox1.store, "mox2").await == 20
    })
    .await;
    drop(mox2);
    std::fs::remove_dir_all(lab.db_path(2).parent().unwrap()).unwrap();
    let mut mox2 = lab.node(2).await;
    assert_eq!(held(&mox2.store, "mox3").await, 0);
    mox2.catch_up();
    eventually(
        "mox3's and mox2's old records on the rebuilt mox2",
        || async { held(&mox2.store, "mox3").await == 50 && held(&mox2.store, "mox2").await == 20 },
    )
    .await;
    mox1.catch_up();
    let all = search(&mox2.store, LogQuery::default()).await.unwrap();
    assert_eq!(all.records.len(), 70);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn catch_up_fills_gaps_and_reports_lag() {
    let lab = Lab::new(&[1, 2]);
    let mox1 = lab.node(1).await;
    let mox2 = lab.node(2).await;
    mox1.store.append(lines("line", 1200)).await.unwrap();
    let stream = mox1.store.stream().to_string();
    let all = mox1
        .store
        .records_after("mox1", &stream, 0, 2000)
        .await
        .unwrap();
    let holey: Vec<_> = all
        .iter()
        .filter(|r| r.seq % 7 != 0 && !(300..=900).contains(&r.seq))
        .cloned()
        .collect();
    mox2.store.insert_copies(holey).await.unwrap();
    assert!(
        !records::missing(&mox2.store, "mox1", &stream, 1, 1200)
            .await
            .unwrap()
            .is_empty()
    );

    mox2.mesh.catch_up_all().await;
    assert!(
        records::missing(&mox2.store, "mox1", &stream, 1, 1200)
            .await
            .unwrap()
            .is_empty(),
        "every gap is filled"
    );
    let view = mox2.mesh.replication().await;
    let from_mox1 = view.iter().find(|p| p.peer == host(1)).unwrap();
    assert!(from_mox1.error.is_none());
    let origin = from_mox1
        .origins
        .iter()
        .find(|o| o.origin == "mox1")
        .unwrap();
    assert_eq!(
        (origin.last_seq, origin.behind_records, origin.gaps),
        (1200, 0, 0)
    );

    mox1.mesh.catch_up_all().await;
    let (lagging, checked) = mox1.mesh.lagging().await;
    assert_eq!((lagging.len(), checked), (0, 1));
    mox1.store.append(lines("more", 5)).await.unwrap();
    mox1.mesh.catch_up_all().await;
    let (lagging, _) = mox1.mesh.lagging().await;
    assert_eq!(lagging.len(), 1, "mox2 has no live link, so it lags");
    assert_eq!((lagging[0].peer, lagging[0].behind_records), (host(2), 5));
}
