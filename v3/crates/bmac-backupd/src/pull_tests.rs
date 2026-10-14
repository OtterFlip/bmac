// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

use std::collections::{BTreeMap, BTreeSet};

use bmac_backup::manifest::{
    Consistency, Firmware, Manifest, Versions, Volume, VolumeRole, current_config, sha256_hex,
};
use bmac_backup::names::PitName;
use bmac_core::{HostSlot, SnapshotGuid};
use bmac_proto::api::ApiError;
use bmac_proto::appliance::{BaseKind, OfferBase};
use bytes::Bytes;
use serde_json::json;

use super::*;
use crate::fake_pool::{FakePool, Header};

const CLUSTER: &str = "11111111-2222-3333-4444-555555555555";

fn vm() -> Uuid {
    Uuid::from_u128(0x7)
}

fn disk(n: u128) -> Uuid {
    Uuid::from_u128(0xd0 + n)
}

/// One snapshot of one volume on the cluster.
#[derive(Debug, Clone, Copy)]
struct Snap {
    guid: u64,
    volsize: u64,
}

#[derive(Default)]
struct FakeLink {
    /// (PIT, volume) to what the cluster holds.
    snaps: Mutex<BTreeMap<(Uuid, Uuid), Snap>>,
    names: Mutex<BTreeMap<Uuid, String>>,
    refusing: Mutex<BTreeSet<HostSlot>>,
    /// Cut the next stream after its header.
    cut: Mutex<bool>,
    /// Send another GUID than the catalog's.
    lie: Mutex<bool>,
    sends: Mutex<Vec<(HostSlot, SendRequest)>>,
    claims: Mutex<Vec<ClaimRequest>>,
}

impl Link for FakeLink {
    fn claim<'a>(&'a self, request: &'a ClaimRequest) -> BoxFuture<'a, Result<(), CallError>> {
        Box::pin(async move {
            self.claims.lock().unwrap().push(request.clone());
            Ok(())
        })
    }

    fn send<'a>(
        &'a self,
        host: &'a SourceHost,
        request: &'a SendRequest,
    ) -> BoxFuture<'a, Result<Stream, CallError>> {
        Box::pin(async move {
            self.sends
                .lock()
                .unwrap()
                .push((host.host, request.clone()));
            if self.refusing.lock().unwrap().contains(&host.host) {
                return Err(CallError::Refused(ApiError::new(
                    ErrorCode::NotFound,
                    "not here",
                )));
            }
            let snaps = self.snaps.lock().unwrap();
            let snap = snaps[&(request.pit_uuid, request.volume_uuid)];
            let from = request.base.map(|b| snaps[&(b, request.volume_uuid)].guid);
            let lie = std::mem::take(&mut *self.lie.lock().unwrap());
            let header = Header {
                name: self.names.lock().unwrap()[&request.pit_uuid].clone(),
                guid: if lie { snap.guid + 1 } else { snap.guid },
                from,
                resume: request.resume_token.is_some(),
                volsize: snap.volsize,
                volblocksize: 16384,
            };
            let cut = std::mem::take(&mut *self.cut.lock().unwrap());
            let (tx, rx) = mpsc::channel(4);
            let mut first = serde_json::to_vec(&header).unwrap();
            first.push(b'\n');
            tokio::spawn(async move {
                let _ = tx.send(Ok(Bytes::from(first))).await;
                if cut {
                    let _ = tx.send(Err("the stream broke".into())).await;
                } else {
                    let _ = tx.send(Ok(Bytes::from_static(b"payload"))).await;
                }
            });
            Ok(rx)
        })
    }
}

struct Rig {
    _dir: tempfile::TempDir,
    pool: FakePool,
    link: FakeLink,
    metadata: Metadata,
    layout: Layout,
    progress: Progress,
}

fn rig() -> Rig {
    let dir = tempfile::tempdir().unwrap();
    let metadata = Metadata::new(dir.path());
    metadata
        .save(&Journal::new(Uuid::from_u128(1), "4242"))
        .unwrap();
    Rig {
        _dir: dir,
        pool: FakePool::new(),
        link: FakeLink::default(),
        metadata,
        layout: Layout { pool: "bk".into() },
        progress: Progress::default(),
    }
}

fn now() -> i64 {
    1_791_860_500_000
}

impl Rig {
    fn puller(&self) -> Puller<'_> {
        Puller {
            pool: &self.pool,
            link: &self.link,
            metadata: &self.metadata,
            layout: &self.layout,
            cluster_uuid: CLUSTER,
            progress: self.progress.clone(),
            now_ms: &now,
        }
    }

    fn journal(&self) -> Journal {
        self.metadata.load().unwrap().unwrap()
    }

    /// A PIT on the cluster with these volumes' GUIDs, and its offer.
    fn offer(
        &self,
        pit: u128,
        branch: u128,
        parent: Option<u128>,
        base: Option<u128>,
        guids: &[(Uuid, u64)],
    ) -> OfferedPit {
        let taken = 1_791_860_400 + pit as i64 * 3600;
        let name = PitName::new(taken, pit as u32);
        self.link
            .names
            .lock()
            .unwrap()
            .insert(Uuid::from_u128(pit), name.to_string());
        let (config_text, config) = current_config("scsi0: local-zfs:vm-100-disk-0,size=1G\n");
        let mut manifest = Manifest {
            schema_version: bmac_backup::manifest::SCHEMA_VERSION,
            cluster_uuid: CLUSTER.into(),
            vm_uuid: vm().to_string(),
            vmid: 100,
            pit_uuid: Uuid::from_u128(pit).to_string(),
            branch_uuid: Uuid::from_u128(branch).to_string(),
            parent_pit_uuid: parent.map(|p| Uuid::from_u128(p).to_string()),
            snapshot: name.clone(),
            taken_at_s: taken,
            consistency: Consistency::Filesystem,
            evidence: json!({}),
            multi_pool: false,
            versions: Versions {
                proxmox: "9".into(),
                openzfs: "2.3".into(),
                bmac: "3".into(),
            },
            config_sha256: sha256_hex(config_text.as_bytes()),
            config_text,
            config,
            volumes: guids
                .iter()
                .enumerate()
                .map(|(i, (id, guid))| Volume {
                    volume_uuid: id.to_string(),
                    slot: format!("scsi{i}"),
                    role: VolumeRole::Disk,
                    dataset: format!("rpool/data/vm-100-disk-{i}"),
                    snapshot_guid: SnapshotGuid::new(*guid),
                    volsize: 1 << 30,
                    volblocksize: 16384,
                    boot: i == 0,
                })
                .collect(),
            firmware: Firmware {
                bios: "seabios".into(),
                machine: None,
                secure_boot: false,
            },
            registry: json!({}),
            prerequisites: vec![],
            sha256: String::new(),
        };
        let text = manifest.seal();
        for (id, guid) in guids {
            self.link.snaps.lock().unwrap().insert(
                (Uuid::from_u128(pit), *id),
                Snap {
                    guid: *guid,
                    volsize: 1 << 30,
                },
            );
        }
        OfferedPit {
            vm_uuid: vm(),
            pit_uuid: Uuid::from_u128(pit),
            branch_uuid: Uuid::from_u128(branch),
            name: name.to_string(),
            taken_s: taken,
            parent: parent.map(Uuid::from_u128),
            base: base.map(|b| OfferBase {
                pit_uuid: Uuid::from_u128(b),
                kind: BaseKind::Bookmark,
            }),
            hosts: ["mox1", "mox3"]
                .map(|h| SourceHost {
                    host: h.parse().unwrap(),
                    addresses: vec![format!("bmaclab-{h}")],
                })
                .to_vec(),
            manifest: text,
        }
    }
}

#[tokio::test]
async fn a_first_pull_is_full_and_published_with_holds_and_its_manifest() {
    let r = rig();
    let offer = r.offer(1, 0xa, None, None, &[(disk(0), 100), (disk(1), 200)]);
    let published = r.puller().pull(&offer).await.unwrap();
    assert_eq!(published.pit.volumes.len(), 2);
    let journal = r.journal();
    assert_eq!(journal.revision, 1);
    assert!(journal.receiving.is_empty());
    let dataset = &journal.pits[0].volumes[0].dataset;
    assert_eq!(
        dataset,
        &format!(
            "bk/bmac/clusters/{CLUSTER}/vms/{}/{}/{}",
            vm(),
            Uuid::from_u128(0xa),
            disk(0)
        )
    );
    let snapshot = format!("{dataset}@{}", offer.name);
    assert!(
        r.pool
            .holds
            .lock()
            .unwrap()
            .contains(&(RETAIN_TAG.to_string(), snapshot))
    );
    assert_eq!(
        r.metadata
            .manifest(CLUSTER, &offer.pit_uuid)
            .unwrap()
            .as_deref(),
        Some(offer.manifest.as_str())
    );
    let sends = r.link.sends.lock().unwrap();
    assert!(
        sends
            .iter()
            .all(|(_, s)| s.base.is_none() && s.large_blocks && s.compressed)
    );
    assert_eq!(r.link.claims.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn the_next_pit_is_an_incremental_onto_the_heads_zvols() {
    let r = rig();
    let first = r.offer(1, 0xa, None, None, &[(disk(0), 100)]);
    r.puller().pull(&first).await.unwrap();
    let second = r.offer(2, 0xa, Some(1), Some(1), &[(disk(0), 101)]);
    let published = r.puller().pull(&second).await.unwrap();
    assert_eq!(published.pit.received_from, Some(Uuid::from_u128(1)));
    let journal = r.journal();
    assert_eq!(
        journal.pits[0].volumes[0].dataset,
        journal.pits[1].volumes[0].dataset
    );
    let zvol = r.pool.zvol(&journal.pits[1].volumes[0].dataset).unwrap();
    assert_eq!(zvol.snapshots.len(), 2);
    assert_eq!(journal.heads(CLUSTER)[&vm()].pit_uuid, Uuid::from_u128(2));
}

#[tokio::test]
async fn a_new_branch_is_received_as_a_clone_of_the_fork_point() {
    let r = rig();
    r.puller()
        .pull(&r.offer(1, 0xa, None, None, &[(disk(0), 100)]))
        .await
        .unwrap();
    r.puller()
        .pull(&r.offer(2, 0xa, Some(1), Some(1), &[(disk(0), 101)]))
        .await
        .unwrap();
    let forked = r.offer(3, 0xb, Some(1), Some(1), &[(disk(0), 300)]);
    r.puller().pull(&forked).await.unwrap();
    let journal = r.journal();
    let on_a = journal.pit(CLUSTER, &Uuid::from_u128(1)).unwrap();
    let on_b = journal.pit(CLUSTER, &Uuid::from_u128(3)).unwrap();
    assert!(
        on_b.volumes[0]
            .dataset
            .contains(&Uuid::from_u128(0xb).to_string())
    );
    let zvol = r.pool.zvol(&on_b.volumes[0].dataset).unwrap();
    assert_eq!(zvol.origin, Some(on_a.snapshot(&on_a.volumes[0])));
    assert_eq!(
        zvol.snapshots.len(),
        1,
        "the branch stores only what came after its fork"
    );
}

#[tokio::test]
async fn an_interrupted_pull_resumes_with_its_token_and_transfer() {
    let r = rig();
    let offer = r.offer(1, 0xa, None, None, &[(disk(0), 100)]);
    *r.link.cut.lock().unwrap() = true;
    let error = r.puller().pull(&offer).await.unwrap_err();
    assert!(matches!(error, PullError::Retry(_)), "{error:?}");
    let receiving = r.journal().receiving[0].clone();
    assert_eq!(receiving.pit_uuid, offer.pit_uuid);
    assert!(r.progress.lock().unwrap().is_empty());

    r.puller().pull(&offer).await.unwrap();
    let sends = r.link.sends.lock().unwrap();
    let (first, again) = (&sends[0].1, &sends[1].1);
    assert_eq!(
        first.transfer_uuid, again.transfer_uuid,
        "the same transfer"
    );
    assert_eq!(
        again.resume_token.as_deref(),
        Some(format!("token-{}", receiving.volumes[0].dataset).as_str())
    );
}

#[tokio::test]
async fn a_newer_offer_abandons_the_interrupted_receive_of_an_older_one() {
    let r = rig();
    r.puller()
        .pull(&r.offer(1, 0xa, None, None, &[(disk(0), 100)]))
        .await
        .unwrap();
    let stale = r.offer(2, 0xa, Some(1), Some(1), &[(disk(0), 101)]);
    *r.link.cut.lock().unwrap() = true;
    r.puller().pull(&stale).await.unwrap_err();
    let newest = r.offer(3, 0xa, Some(2), Some(1), &[(disk(0), 102)]);
    r.puller().pull(&newest).await.unwrap();
    let journal = r.journal();
    assert_eq!(
        r.pool.aborted.lock().unwrap().as_slice(),
        [journal.pits[0].volumes[0].dataset.clone()]
    );
    assert!(journal.pit(CLUSTER, &Uuid::from_u128(2)).is_none());
    assert!(journal.pit(CLUSTER, &Uuid::from_u128(3)).is_some());
}

#[tokio::test]
async fn a_host_without_the_snapshot_passes_the_pull_to_the_next() {
    let r = rig();
    r.link
        .refusing
        .lock()
        .unwrap()
        .insert("mox1".parse().unwrap());
    r.puller()
        .pull(&r.offer(1, 0xa, None, None, &[(disk(0), 100)]))
        .await
        .unwrap();
    let hosts: Vec<String> = r
        .link
        .sends
        .lock()
        .unwrap()
        .iter()
        .map(|(h, _)| h.to_string())
        .collect();
    assert_eq!(hosts, ["mox1", "mox3"]);
}

#[tokio::test]
async fn a_guid_that_isnt_the_manifests_is_quarantined_and_never_published() {
    let r = rig();
    r.puller()
        .pull(&r.offer(1, 0xa, None, None, &[(disk(0), 100)]))
        .await
        .unwrap();
    let bad = r.offer(2, 0xa, Some(1), Some(1), &[(disk(0), 101)]);
    *r.link.lie.lock().unwrap() = true;
    let error = r.puller().pull(&bad).await.unwrap_err();
    assert!(matches!(error, PullError::Failed(_)), "{error:?}");
    let journal = r.journal();
    assert!(journal.pit(CLUSTER, &bad.pit_uuid).is_none());
    assert_eq!(journal.quarantined.len(), 1);
    let dataset = &journal.pits[0].volumes[0].dataset;
    assert_eq!(
        r.pool.zvol(dataset).unwrap().snapshots.len(),
        1,
        "back at the head"
    );

    let good = r.offer(3, 0xa, Some(2), Some(1), &[(disk(0), 103)]);
    r.puller().pull(&good).await.unwrap();
}

#[tokio::test]
async fn a_manifest_for_another_pit_or_a_full_pool_stops_the_pull() {
    let r = rig();
    let mut offer = r.offer(1, 0xa, None, None, &[(disk(0), 100)]);
    offer.pit_uuid = Uuid::from_u128(99);
    assert!(matches!(
        r.puller().pull(&offer).await,
        Err(PullError::Failed(_))
    ));
    let offer = r.offer(2, 0xa, None, None, &[(disk(0), 100)]);
    *r.pool.free.lock().unwrap() = 1 << 20;
    assert!(matches!(
        r.puller().pull(&offer).await,
        Err(PullError::Retry(_))
    ));
    assert!(r.link.sends.lock().unwrap().is_empty());
}

#[test]
fn a_reseed_gets_a_new_lineage() {
    let r = rig();
    let journal = Journal::new(Uuid::from_u128(1), "1");
    let branch = Uuid::from_u128(0xa);
    let taken = BTreeSet::from([r
        .layout
        .volume(CLUSTER, &vm(), &branch.to_string(), &disk(0))]);
    assert_eq!(
        lineage(&journal, &r.layout, CLUSTER, &vm(), &branch, &taken),
        format!("{branch}-2")
    );
    assert_eq!(
        lineage(
            &journal,
            &r.layout,
            CLUSTER,
            &vm(),
            &branch,
            &BTreeSet::new()
        ),
        branch.to_string()
    );
}
