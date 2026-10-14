// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

//! The backup work against a model of the hosts: each host's snapshots and
//! bookmarks with their GUIDs, replication jobs that copy the owner's
//! snapshots (and drop released ones) when asked, and a guest agent.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bmac_backup::lifetime::Offer;
use bmac_backup::manifest::{Consistency, Manifest};
use bmac_backup::records::{
    ApplianceRecord, Attempt, Enrollment, Freeze, Policy, Presence, RECORD_SCHEMA,
};
use bmac_backup::retention::LocalRetention;
use bmac_core::{GuestName, HostSlot, SnapshotGuid};
use bmac_exec::BoxFuture;
use bmac_pmxcfs::{Pmxcfs, RealFs};
use serde_json::{Value, json};
use uuid::Uuid;

use super::capture::{self, Captured, Context, Guests, Production};
use super::cluster::{BackupCluster, LiveGuest, ReplicationJob};
use super::store::BackupStore;
use crate::native::Stop;

pub(crate) const NOW_MS: i64 = 1_791_860_400_000;

pub(crate) fn host(n: u8) -> HostSlot {
    format!("mox{n}").parse().unwrap()
}

pub(crate) fn vm_uuid() -> Uuid {
    Uuid::from_u128(0xa)
}

pub(crate) fn appliance_uuid(n: u128) -> Uuid {
    Uuid::from_u128(0xb00 + n)
}

#[derive(Default)]
pub(crate) struct World {
    pub owner: Option<HostSlot>,
    pub running: bool,
    pub config: String,
    /// After the snapshot, the configuration reads as this.
    pub config_after_snapshot: Option<String>,
    /// Snapshots (`ds@name`) and bookmarks (`ds#name`) per host.
    pub names: BTreeMap<HostSlot, BTreeMap<String, SnapshotGuid>>,
    pub jobs: Vec<ReplicationJob>,
    /// Refusals left per job before a run is accepted.
    pub busy: BTreeMap<String, u32>,
    /// Replication gives this host's copies a different GUID.
    pub mismatch_on: Option<HostSlot>,
    /// Replication to this host never happens.
    pub stuck: Option<HostSlot>,
    /// The VM moves to this host once replication has run.
    pub move_after_replication: Option<HostSlot>,
    /// Names whose destroy fails, as with a dependent clone.
    pub held: BTreeSet<String>,
    pub agent: bool,
    pub frozen: bool,
    pub armed: BTreeSet<String>,
    pub next_guid: u64,
    pub calls: Vec<String>,
    /// Receive resume tokens and the snapshot each continues.
    pub tokens: BTreeMap<String, String>,
}

impl World {
    fn guid(&mut self) -> SnapshotGuid {
        self.next_guid += 1;
        (1_000_000 + self.next_guid).to_string().parse().unwrap()
    }
}

#[derive(Clone)]
pub(crate) struct FakeCluster {
    pub me: HostSlot,
    pub world: Arc<Mutex<World>>,
}

fn ready<T: Send + 'static>(value: T) -> BoxFuture<'static, T> {
    Box::pin(async move { value })
}

impl BackupCluster for FakeCluster {
    fn this_host(&self) -> HostSlot {
        self.me
    }

    fn live_guest(&self, _vmid: u32) -> BoxFuture<'_, Result<Option<LiveGuest>, String>> {
        let w = self.world.lock().unwrap();
        ready(Ok(w.owner.map(|node| LiveGuest {
            node,
            running: w.running,
        })))
    }

    fn config_text(&self, _vmid: u32) -> BoxFuture<'_, Result<String, String>> {
        let w = self.world.lock().unwrap();
        let taken = w
            .names
            .get(&self.me)
            .is_some_and(|n| n.keys().any(|k| k.contains("@bmac-pit-")));
        let text = match (&w.config_after_snapshot, taken) {
            (Some(after), true) => after.clone(),
            _ => w.config.clone(),
        };
        ready(Ok(text))
    }

    fn dataset(&self, volume_id: &str) -> BoxFuture<'_, Result<String, String>> {
        let found = if let Some(v) = volume_id.strip_prefix("local-zfs:") {
            Ok(format!("rpool/data/{v}"))
        } else if let Some(v) = volume_id.strip_prefix("tank-zfs:") {
            Ok(format!("tank/data/{v}"))
        } else {
            Err(format!("{volume_id} is not a ZFS volume"))
        };
        ready(found)
    }

    fn volume_props(&self, _dataset: &str) -> BoxFuture<'_, Result<(u64, u64), String>> {
        ready(Ok((32 << 30, 16384)))
    }

    fn freeze(&self, _vmid: u32) -> BoxFuture<'_, Result<Value, String>> {
        let mut w = self.world.lock().unwrap();
        w.calls.push("freeze".into());
        if !w.agent {
            return ready(Err("QEMU guest agent is not running".into()));
        }
        w.frozen = true;
        ready(Ok(json!(2)))
    }

    fn thaw(&self, _vmid: u32) -> BoxFuture<'_, Result<(), String>> {
        let mut w = self.world.lock().unwrap();
        w.calls.push("thaw".into());
        w.frozen = false;
        ready(Ok(()))
    }

    fn arm_thaw(&self, vmid: u32, _after: Duration) -> BoxFuture<'_, Result<String, String>> {
        let mut w = self.world.lock().unwrap();
        let unit = format!("bmac-thaw-{vmid}-{}", w.armed.len());
        w.armed.insert(unit.clone());
        ready(Ok(unit))
    }

    fn disarm_thaw(&self, unit: &str) -> BoxFuture<'_, Result<(), String>> {
        self.world.lock().unwrap().armed.remove(unit);
        ready(Ok(()))
    }

    fn snapshot(&self, names: &[String]) -> BoxFuture<'_, Result<(), String>> {
        let mut w = self.world.lock().unwrap();
        w.calls.push(format!("snapshot {}", names.len()));
        for name in names {
            let guid = w.guid();
            w.names
                .entry(self.me)
                .or_default()
                .insert(name.clone(), guid);
        }
        ready(Ok(()))
    }

    fn guids(
        &self,
        node: HostSlot,
        names: &[String],
    ) -> BoxFuture<'_, Result<BTreeMap<String, SnapshotGuid>, String>> {
        let w = self.world.lock().unwrap();
        let on = w.names.get(&node).cloned().unwrap_or_default();
        ready(Ok(names
            .iter()
            .filter_map(|n| on.get(n).map(|g| (n.clone(), *g)))
            .collect()))
    }

    fn bookmark(
        &self,
        node: HostSlot,
        snapshot: &str,
        bookmark: &str,
    ) -> BoxFuture<'_, Result<(), String>> {
        let mut w = self.world.lock().unwrap();
        w.calls.push(format!("bookmark {node} {bookmark}"));
        let Some(guid) = w.names.get(&node).and_then(|n| n.get(snapshot)).copied() else {
            return ready(Err(format!("{snapshot} doesn't exist on {node}")));
        };
        w.names
            .entry(node)
            .or_default()
            .insert(bookmark.to_string(), guid);
        ready(Ok(()))
    }

    fn destroy(&self, node: HostSlot, name: &str) -> BoxFuture<'_, Result<(), String>> {
        let mut w = self.world.lock().unwrap();
        w.calls.push(format!("destroy {node} {name}"));
        if w.held.contains(name) {
            return ready(Err(format!(
                "cannot destroy '{name}': snapshot has dependent clones"
            )));
        }
        w.names.entry(node).or_default().remove(name);
        ready(Ok(()))
    }

    fn resume_target(&self, token: &str) -> BoxFuture<'_, Result<(String, SnapshotGuid), String>> {
        let w = self.world.lock().unwrap();
        let found = w.tokens.get(token).and_then(|name| {
            let guid = w.names.get(&self.me)?.get(name)?;
            Some((name.clone(), *guid))
        });
        ready(found.ok_or_else(|| "cannot resume send: invalid token".to_string()))
    }

    fn replication_jobs(&self, _vmid: u32) -> BoxFuture<'_, Result<Vec<ReplicationJob>, String>> {
        ready(Ok(self.world.lock().unwrap().jobs.clone()))
    }

    fn schedule_replication(&self, job: &str) -> BoxFuture<'_, Result<(), String>> {
        let mut w = self.world.lock().unwrap();
        w.calls.push(format!("replicate {job}"));
        if let Some(left) = w.busy.get_mut(job).filter(|n| **n > 0) {
            *left -= 1;
            return ready(Err("guest is locked by another replication run".into()));
        }
        let Some(target) = w.jobs.iter().find(|j| j.id == job).map(|j| j.target) else {
            return ready(Err(format!("no job {job}")));
        };
        if w.stuck == Some(target) {
            return ready(Ok(()));
        }
        replicate_now(&mut w, self.me, target);
        if let Some(to) = w.move_after_replication.take() {
            w.owner = Some(to);
        }
        ready(Ok(()))
    }

    fn versions(&self) -> BoxFuture<'_, Result<(String, String), String>> {
        ready(Ok(("pve-manager/9.0.0".into(), "zfs-2.3.4-pve1".into())))
    }
}

/// One replication run from `owner` to `target`: the owner's snapshots
/// arrive with their GUIDs; snapshots the owner no longer has go.
pub(crate) fn replicate_now(w: &mut World, owner: HostSlot, target: HostSlot) {
    let source: BTreeMap<String, SnapshotGuid> = w
        .names
        .get(&owner)
        .map(|n| {
            n.iter()
                .filter(|(k, _)| k.contains('@'))
                .map(|(k, v)| (k.clone(), *v))
                .collect()
        })
        .unwrap_or_default();
    let mismatch = w.mismatch_on == Some(target);
    let mut copies = Vec::new();
    for (name, guid) in &source {
        let guid = if mismatch { w.guid() } else { *guid };
        copies.push((name.clone(), guid));
    }
    let on = w.names.entry(target).or_default();
    on.retain(|k, _| !k.contains('@') || source.contains_key(k));
    for (name, guid) in copies {
        on.entry(name).or_insert(guid);
    }
}

pub(crate) struct FakeGuests {
    pub production: Production,
    pub leases: Mutex<Vec<String>>,
    pub leased: Mutex<bool>,
}

impl Guests for FakeGuests {
    fn production(&self, name: &GuestName) -> Result<Production, String> {
        if *name == self.production.name {
            Ok(self.production.clone())
        } else {
            Err(format!("the registry has no {name}"))
        }
    }

    fn by_uuid(&self, vm: &Uuid) -> Result<Option<Production>, String> {
        Ok((*vm == self.production.vm_uuid).then(|| self.production.clone()))
    }

    fn leased(&self, _name: &GuestName) -> Result<bool, String> {
        Ok(*self.leased.lock().unwrap())
    }

    fn acquire(&self, _name: &GuestName) -> BoxFuture<'_, Result<String, String>> {
        self.leases.lock().unwrap().push("acquire".into());
        ready(Ok("nonce".into()))
    }

    fn release(&self, _name: &GuestName, nonce: &str) -> BoxFuture<'_, Result<(), String>> {
        self.leases.lock().unwrap().push(format!("release {nonce}"));
        ready(Ok(()))
    }
}

pub(crate) const CONFIG: &str = "boot: order=scsi0\nbios: ovmf\nefidisk0: local-zfs:vm-100-disk-0,efitype=4m,pre-enrolled-keys=1\nscsi0: local-zfs:vm-100-disk-1,size=32G\nide2: none,media=cdrom\n";

pub(crate) struct Setup {
    pub _dir: tempfile::TempDir,
    pub clock: Arc<std::sync::atomic::AtomicI64>,
    pub store: BackupStore,
    pub cluster: FakeCluster,
    pub guests: FakeGuests,
}

pub(crate) fn setup() -> Setup {
    let dir = tempfile::tempdir().unwrap();
    let pmxcfs = Pmxcfs::new(dir.path(), Arc::new(RealFs));
    let store = BackupStore::new(pmxcfs, dir.path().join("priv/bmac/backup"));
    let world = World {
        owner: Some(host(1)),
        running: true,
        config: CONFIG.into(),
        jobs: vec![ReplicationJob {
            id: "100-0".into(),
            target: host(3),
        }],
        agent: true,
        ..World::default()
    };
    let cluster = FakeCluster {
        me: host(1),
        world: Arc::new(Mutex::new(world)),
    };
    let guests = FakeGuests {
        production: Production {
            vm_uuid: vm_uuid(),
            name: "prod1".parse().unwrap(),
            vmid: 100,
            placement: vec![host(1), host(3)],
            record: json!({"name": "prod1"}),
        },
        leases: Mutex::new(Vec::new()),
        leased: Mutex::new(false),
    };
    Setup {
        _dir: dir,
        clock: Arc::new(std::sync::atomic::AtomicI64::new(NOW_MS)),
        store,
        cluster,
        guests,
    }
}

pub(crate) fn policy(appliances: Vec<Uuid>) -> Policy {
    Policy {
        schema_version: RECORD_SCHEMA,
        record_type: String::new(),
        revision: 0,
        vm_uuid: vm_uuid(),
        vm: "prod1".into(),
        schedule_id: Policy::schedule_id_for(&vm_uuid()),
        appliances,
        local_retention: LocalRetention::default(),
        freeze: Freeze::IfAvailable,
        accept_multi_pool: false,
        enabled: true,
        updated_by: "test".into(),
        updated_at_ms: 0,
    }
}

pub(crate) fn appliance(n: u128, last_checkin_ms: Option<i64>) -> ApplianceRecord {
    ApplianceRecord {
        schema_version: RECORD_SCHEMA,
        record_type: String::new(),
        revision: 0,
        appliance_uuid: appliance_uuid(n),
        name: format!("appliance-{n}"),
        fingerprint: "00".repeat(32),
        address: None,
        enrollment: Enrollment::Approved,
        enrolled_at_ms: 0,
        paused: false,
        retired: false,
        last_checkin_ms,
        version: None,
        locked: false,
        progress: Default::default(),
        heads: BTreeMap::new(),
        index_revision: 0,
        scrub: None,
    }
}

impl Setup {
    pub(crate) fn save(&self, policy: Policy, appliances: Vec<ApplianceRecord>) {
        self.store
            .locked(|l| {
                l.save_policy(policy)?;
                for a in appliances {
                    l.save_appliance(a)?;
                }
                Ok(())
            })
            .unwrap();
    }

    pub(crate) async fn capture(&self) -> (Result<Captured, Stop>, String) {
        let work = tempfile::tempdir().unwrap();
        let (flow, out, _input) = crate::native::tests::workflow(work.path());
        let clock = self.clock.clone();
        let now = move || clock.load(std::sync::atomic::Ordering::SeqCst);
        let cx = Context {
            flow: &flow,
            cluster: &self.cluster,
            guests: &self.guests,
            store: &self.store,
            cluster_uuid: "c0ffee".into(),
            bmac_version: "3.0.0".into(),
            now_ms: &now,
            poll: Duration::from_millis(1),
            replication_timeout: Duration::from_millis(200),
            replication_ask_every: Duration::ZERO,
            thaw_after: Duration::from_secs(60),
        };
        let outcome = capture::capture(&cx, &"prod1".parse().unwrap()).await;
        let text = out
            .events()
            .iter()
            .map(|e| serde_json::to_string(e).unwrap())
            .collect::<Vec<_>>()
            .join("\n");
        (outcome, text)
    }

    pub(crate) fn names_on(&self, n: u8) -> BTreeMap<String, SnapshotGuid> {
        self.cluster
            .world
            .lock()
            .unwrap()
            .names
            .get(&host(n))
            .cloned()
            .unwrap_or_default()
    }

    fn attempt(&self) -> Option<Attempt> {
        self.store
            .lineage(&vm_uuid())
            .unwrap()
            .and_then(|l| l.last_attempt)
    }
}

fn failure(outcome: Result<Captured, Stop>) -> String {
    match outcome {
        Err(Stop::Failed { message, .. }) => message,
        other => panic!("expected a failure, got {other:?}"),
    }
}

#[tokio::test]
async fn a_capture_snapshots_every_disk_replicates_and_offers_the_pit() {
    let s = setup();
    s.save(
        policy(vec![appliance_uuid(1), appliance_uuid(2)]),
        vec![
            appliance(1, Some(NOW_MS - 30_000)),
            appliance(2, Some(NOW_MS - 600_000)),
        ],
    );
    let (outcome, _) = s.capture().await;
    let Ok(Captured::Pit { pit_uuid, name }) = outcome else {
        panic!("{outcome:?}");
    };
    let owner = s.names_on(1);
    let replica = s.names_on(3);
    assert_eq!(owner.len(), 2, "efidisk0 and scsi0, never the CD-ROM");
    assert_eq!(owner, replica, "the replica has the owner's GUIDs");
    {
        let w = s.cluster.world.lock().unwrap();
        assert!(
            !w.frozen && w.armed.is_empty(),
            "thawed, and the thaw timer stopped"
        );
        assert_eq!(&w.calls[..3], ["freeze", "snapshot 2", "thaw"]);
    }
    assert_eq!(
        *s.guests.leases.lock().unwrap(),
        ["acquire", "release nonce"]
    );

    let pit = s.store.pit(&vm_uuid(), &pit_uuid).unwrap().unwrap();
    assert_eq!(pit.name, name);
    assert_eq!(pit.presence, Presence::Snapshot);
    assert_eq!(pit.consistency, Consistency::Filesystem);
    assert_eq!(
        pit.offers,
        BTreeMap::from([(
            appliance_uuid(1),
            Offer::Open {
                offered_s: NOW_MS / 1000
            }
        )]),
        "only the appliance in its ready window is offered the PIT"
    );
    let manifest = Manifest::open(&pit.manifest).unwrap();
    assert_eq!(manifest.volumes.len(), 2);
    assert!(manifest.firmware.secure_boot);
    assert_eq!(manifest.parent_pit_uuid, None);
    let lineage = s.store.lineage(&vm_uuid()).unwrap().unwrap();
    assert_eq!(lineage.head, Some(pit_uuid));
    assert_eq!(lineage.volumes.len(), 2);
    assert!(matches!(s.attempt(), Some(Attempt::Captured { .. })));

    let (again, _) = s.capture().await;
    let Ok(Captured::Pit {
        pit_uuid: second, ..
    }) = again
    else {
        panic!("{again:?}");
    };
    let next =
        Manifest::open(&s.store.pit(&vm_uuid(), &second).unwrap().unwrap().manifest).unwrap();
    assert_eq!(next.parent_pit_uuid, Some(pit_uuid.to_string()));
    assert_eq!(
        next.volumes
            .iter()
            .map(|v| v.volume_uuid.clone())
            .collect::<Vec<_>>(),
        manifest
            .volumes
            .iter()
            .map(|v| v.volume_uuid.clone())
            .collect::<Vec<_>>(),
        "volume UUIDs stay with their disks"
    );
}

#[tokio::test]
async fn no_available_appliance_skips_unless_local_retention_claims_the_slot() {
    let s = setup();
    s.save(
        policy(vec![appliance_uuid(1)]),
        vec![appliance(1, Some(NOW_MS - 301_000))],
    );
    let (outcome, text) = s.capture().await;
    assert_eq!(outcome, Ok(Captured::Skipped));
    assert!(text.contains("none of its appliances checked in"));
    assert!(s.names_on(1).is_empty(), "no snapshot");
    assert!(s.guests.leases.lock().unwrap().is_empty());
    assert!(matches!(s.attempt(), Some(Attempt::Skipped { .. })));

    let mut kept = policy(vec![appliance_uuid(1)]);
    kept.local_retention = LocalRetention {
        days: 1,
        ..Default::default()
    };
    kept.revision = 1;
    s.store.locked(|l| l.save_policy(kept)).unwrap();
    let (outcome, _) = s.capture().await;
    let Ok(Captured::Pit { pit_uuid, .. }) = outcome else {
        panic!("{outcome:?}");
    };
    assert!(
        s.store
            .pit(&vm_uuid(), &pit_uuid)
            .unwrap()
            .unwrap()
            .offers
            .is_empty()
    );
    let (outcome, _) = s.capture().await;
    assert_eq!(
        outcome,
        Ok(Captured::Skipped),
        "the day already has its PIT"
    );
}

#[tokio::test]
async fn a_guid_mismatch_on_a_replica_refuses_the_offer() {
    let s = setup();
    s.save(
        policy(vec![appliance_uuid(1)]),
        vec![appliance(1, Some(NOW_MS))],
    );
    s.cluster.world.lock().unwrap().mismatch_on = Some(host(3));
    let message = failure(s.capture().await.0);
    assert!(message.contains("not the owner's"), "{message}");
    assert!(
        s.names_on(1).is_empty(),
        "the failed PIT's snapshots are destroyed"
    );
    assert!(s.store.pits(&vm_uuid()).unwrap().is_empty());
    assert!(matches!(s.attempt(), Some(Attempt::Failed { .. })));
    assert_eq!(
        s.guests.leases.lock().unwrap().last().unwrap(),
        "release nonce"
    );
}

#[tokio::test]
async fn an_owner_move_before_the_snapshot_fails_without_one() {
    let s = setup();
    s.save(
        policy(vec![appliance_uuid(1)]),
        vec![appliance(1, Some(NOW_MS))],
    );
    s.cluster.world.lock().unwrap().owner = Some(host(3));
    let message = failure(s.capture().await.0);
    assert!(message.contains("is on mox3 now"), "{message}");
    assert!(s.names_on(1).is_empty() && s.names_on(3).is_empty());
}

#[tokio::test]
async fn a_configuration_change_during_the_capture_discards_the_pit() {
    let s = setup();
    s.save(
        policy(vec![appliance_uuid(1)]),
        vec![appliance(1, Some(NOW_MS))],
    );
    s.cluster.world.lock().unwrap().config_after_snapshot =
        Some(format!("{CONFIG}scsi1: local-zfs:vm-100-disk-2,size=8G\n"));
    let message = failure(s.capture().await.0);
    assert!(message.contains("configuration changed"), "{message}");
    assert!(s.names_on(1).is_empty());
}

#[tokio::test]
async fn a_required_freeze_that_fails_fails_the_pit_and_otherwise_it_is_crash_consistent() {
    let s = setup();
    let mut required = policy(vec![appliance_uuid(1)]);
    required.freeze = Freeze::Required;
    s.save(required, vec![appliance(1, Some(NOW_MS))]);
    s.cluster.world.lock().unwrap().agent = false;
    let message = failure(s.capture().await.0);
    assert!(message.contains("didn't freeze"), "{message}");
    assert!(s.names_on(1).is_empty());
    assert!(s.cluster.world.lock().unwrap().armed.is_empty());

    let mut optional = s.store.policy(&vm_uuid()).unwrap().unwrap();
    optional.freeze = Freeze::IfAvailable;
    s.store.locked(|l| l.save_policy(optional)).unwrap();
    let (outcome, text) = s.capture().await;
    let Ok(Captured::Pit { pit_uuid, .. }) = outcome else {
        panic!("{outcome:?}");
    };
    assert!(text.contains("crash-consistent"));
    assert_eq!(
        s.store
            .pit(&vm_uuid(), &pit_uuid)
            .unwrap()
            .unwrap()
            .consistency,
        Consistency::Crash
    );
}

#[tokio::test]
async fn replication_is_asked_again_while_busy_and_times_out_when_it_never_arrives() {
    let s = setup();
    s.save(
        policy(vec![appliance_uuid(1)]),
        vec![appliance(1, Some(NOW_MS))],
    );
    s.cluster
        .world
        .lock()
        .unwrap()
        .busy
        .insert("100-0".into(), 2);
    let (outcome, _) = s.capture().await;
    assert!(matches!(outcome, Ok(Captured::Pit { .. })), "{outcome:?}");
    let asked = s
        .cluster
        .world
        .lock()
        .unwrap()
        .calls
        .iter()
        .filter(|c| *c == "replicate 100-0")
        .count();
    assert_eq!(asked, 3);

    let s = setup();
    s.save(
        policy(vec![appliance_uuid(1)]),
        vec![appliance(1, Some(NOW_MS))],
    );
    s.cluster.world.lock().unwrap().stuck = Some(host(3));
    let message = failure(s.capture().await.0);
    assert!(
        message.contains("didn't bring the snapshots to mox3"),
        "{message}"
    );
    assert!(s.names_on(1).is_empty());
}

#[tokio::test]
async fn disks_on_several_pools_need_the_policy_to_accept_it() {
    let s = setup();
    s.save(
        policy(vec![appliance_uuid(1)]),
        vec![appliance(1, Some(NOW_MS))],
    );
    s.cluster.world.lock().unwrap().config =
        format!("{CONFIG}scsi1: tank-zfs:vm-100-disk-3,size=8G\n");
    let message = failure(s.capture().await.0);
    assert!(message.contains("several pools"), "{message}");
}

#[tokio::test]
async fn an_owner_move_between_snapshot_and_offer_discards_the_pit() {
    let s = setup();
    s.save(
        policy(vec![appliance_uuid(1)]),
        vec![appliance(1, Some(NOW_MS))],
    );
    s.cluster.world.lock().unwrap().move_after_replication = Some(host(3));
    let message = failure(s.capture().await.0);
    assert!(message.contains("moved during the capture"), "{message}");
    assert!(s.store.pits(&vm_uuid()).unwrap().is_empty());
}

use super::upkeep::{Report, Upkeep, appliance_state};
use bmac_backup::records::ReportedHead;
use bmac_backup::settings::BackupSettings;
use bmac_backup::states::ApplianceState;

impl Setup {
    async fn upkeep_at(&self, now_ms: i64) -> Report {
        let now = move || now_ms;
        let upkeep = Upkeep {
            cluster: &self.cluster,
            store: &self.store,
            guests: &self.guests,
            now_ms: &now,
            bookmarks_proven: true,
        };
        let mut reports = upkeep.run().await;
        assert!(reports.len() <= 1, "{reports:?}");
        reports.pop().unwrap_or_default()
    }

    async fn captured(&self) -> Uuid {
        match self.capture().await.0 {
            Ok(Captured::Pit { pit_uuid, .. }) => pit_uuid,
            other => panic!("{other:?}"),
        }
    }

    fn set_offer(&self, pit: Uuid, appliance: Uuid, offer: Offer) {
        self.store
            .locked(|l| {
                let mut record = l.store().pit(&vm_uuid(), &pit)?.unwrap();
                record.offers.insert(appliance, offer);
                l.save_pit(record).map(drop)
            })
            .unwrap();
    }

    fn report_head(&self, appliance: Uuid, pit: Uuid, checkin_ms: i64) {
        self.store
            .locked(|l| {
                let mut record = l.store().appliance(&appliance)?.unwrap();
                let branch = l.store().lineage(&vm_uuid())?.unwrap().branch_uuid;
                record.heads.insert(
                    vm_uuid(),
                    ReportedHead {
                        branch_uuid: branch,
                        pit_uuid: pit,
                    },
                );
                record.last_checkin_ms = Some(checkin_ms);
                l.save_appliance(record).map(drop)
            })
            .unwrap();
    }

    fn pit_record(&self, pit: Uuid) -> Option<bmac_backup::records::PitRecord> {
        self.store.pit(&vm_uuid(), &pit).unwrap()
    }
}

#[tokio::test]
async fn an_open_offer_keeps_the_snapshot_until_the_claim_window_passes() {
    let s = setup();
    s.save(
        policy(vec![appliance_uuid(1)]),
        vec![appliance(1, Some(NOW_MS))],
    );
    let pit = s.captured().await;
    let report = s.upkeep_at(NOW_MS + 299_000).await;
    assert!(report.released.is_empty(), "{report:?}");
    assert_eq!(s.pit_record(pit).unwrap().presence, Presence::Snapshot);

    let report = s.upkeep_at(NOW_MS + 301_000).await;
    assert_eq!(report.released.len(), 1, "{report:?}");
    let record = s.pit_record(pit).unwrap();
    assert_eq!(
        record.presence,
        Presence::Bookmark,
        "the newest PIT keeps its bookmarks"
    );
    let mox1 = s.names_on(1);
    let mox3 = s.names_on(3);
    assert!(
        mox1.keys().all(|k| k.contains('#')) && mox1.len() == 2,
        "{mox1:?}"
    );
    assert_eq!(
        mox3.keys().filter(|k| k.contains('#')).count(),
        2,
        "bookmarked on the replica too"
    );
    let calls = s.cluster.world.lock().unwrap().calls.clone();
    let last_bookmark = calls
        .iter()
        .rposition(|c| c.starts_with("bookmark"))
        .unwrap();
    let first_destroy = calls
        .iter()
        .position(|c| c.starts_with("destroy mox1"))
        .unwrap();
    assert!(
        last_bookmark < first_destroy,
        "bookmarks on every host come first: {calls:?}"
    );
}

#[tokio::test]
async fn a_claimed_transfer_keeps_the_snapshot_until_it_stalls_or_publishes() {
    let s = setup();
    s.save(
        policy(vec![appliance_uuid(1)]),
        vec![appliance(1, Some(NOW_MS))],
    );
    let pit = s.captured().await;
    s.set_offer(
        pit,
        appliance_uuid(1),
        Offer::Claimed {
            progress_s: NOW_MS / 1000 + 600,
            bytes: 0,
        },
    );
    let report = s.upkeep_at(NOW_MS + 1_000_000).await;
    assert!(report.released.is_empty(), "{report:?}");
    let report = s.upkeep_at(NOW_MS + 1_201_000).await;
    assert_eq!(
        report.released.len(),
        1,
        "a stalled transfer lets go: {report:?}"
    );

    let s = setup();
    s.save(
        policy(vec![appliance_uuid(1)]),
        vec![appliance(1, Some(NOW_MS))],
    );
    let pit = s.captured().await;
    s.set_offer(pit, appliance_uuid(1), Offer::Published);
    let report = s.upkeep_at(NOW_MS + 10_000).await;
    assert_eq!(report.released.len(), 1, "{report:?}");
}

#[tokio::test]
async fn a_dependent_clone_blocks_the_release_with_its_reason_and_it_is_retried() {
    let s = setup();
    s.save(
        policy(vec![appliance_uuid(1)]),
        vec![appliance(1, Some(NOW_MS))],
    );
    let pit = s.captured().await;
    s.set_offer(pit, appliance_uuid(1), Offer::Published);
    let held = s
        .names_on(1)
        .keys()
        .find(|k| k.contains("disk-1@"))
        .unwrap()
        .clone();
    s.cluster.world.lock().unwrap().held.insert(held.clone());
    let report = s.upkeep_at(NOW_MS + 10_000).await;
    assert_eq!(report.blocked.len(), 1, "{report:?}");
    let record = s.pit_record(pit).unwrap();
    assert_eq!(record.presence, Presence::Snapshot);
    assert!(record.release_blocked.unwrap().contains("dependent clones"));

    s.cluster.world.lock().unwrap().held.clear();
    let report = s.upkeep_at(NOW_MS + 20_000).await;
    assert_eq!(report.released.len(), 1, "{report:?}");
    assert_eq!(s.pit_record(pit).unwrap().release_blocked, None);
}

#[tokio::test]
async fn bookmarks_follow_heads_and_the_newest_and_a_delinquent_head_loses_them() {
    let s = setup();
    s.save(
        policy(vec![appliance_uuid(1), appliance_uuid(2)]),
        vec![appliance(1, Some(NOW_MS)), appliance(2, Some(NOW_MS))],
    );
    let first = s.captured().await;
    for a in [1, 2] {
        s.set_offer(first, appliance_uuid(a), Offer::Published);
        s.report_head(appliance_uuid(a), first, NOW_MS);
    }
    s.clock
        .store(NOW_MS + 5_000, std::sync::atomic::Ordering::SeqCst);
    let second = s.captured().await;
    s.set_offer(second, appliance_uuid(1), Offer::Published);
    s.set_offer(second, appliance_uuid(2), Offer::Failed);
    s.report_head(appliance_uuid(1), second, NOW_MS);
    s.upkeep_at(NOW_MS + 10_000).await;
    assert_eq!(
        s.pit_record(first).unwrap().presence,
        Presence::Bookmark,
        "appliance 2's head"
    );
    assert_eq!(
        s.pit_record(second).unwrap().presence,
        Presence::Bookmark,
        "the newest"
    );

    // Appliance 2 stays silent past the delinquency period.
    let later = NOW_MS + 72 * 3_600_000 + 60_000;
    s.report_head(appliance_uuid(1), second, later);
    let report = s.upkeep_at(later).await;
    assert_eq!(report.dropped.len(), 1, "{report:?}");
    assert!(
        s.pit_record(first).is_none(),
        "neither snapshot nor bookmark is left"
    );
    let first_left: Vec<String> = s
        .names_on(1)
        .into_keys()
        .filter(|k| !k.contains(&s_name(&s, second)))
        .collect();
    assert!(first_left.is_empty(), "{first_left:?}");
    let appliances = s.store.appliances().unwrap();
    let pits = s.store.pits(&vm_uuid()).unwrap();
    let settings = BackupSettings::default();
    assert_eq!(
        appliance_state(&appliances[1], &vm_uuid(), &pits, later / 1000, &settings),
        ApplianceState::Delinquent
    );
    assert_eq!(
        appliance_state(&appliances[0], &vm_uuid(), &pits, later / 1000, &settings),
        ApplianceState::Active
    );
}

fn s_name(s: &Setup, pit: Uuid) -> String {
    s.pit_record(pit).unwrap().name.to_string()
}

#[tokio::test]
async fn upkeep_leaves_a_vm_alone_while_an_operation_holds_it_or_another_host_owns_it() {
    let s = setup();
    s.save(
        policy(vec![appliance_uuid(1)]),
        vec![appliance(1, Some(NOW_MS))],
    );
    let pit = s.captured().await;
    s.set_offer(pit, appliance_uuid(1), Offer::Published);
    *s.guests.leased.lock().unwrap() = true;
    assert_eq!(s.upkeep_at(NOW_MS + 10_000).await, Report::default());
    *s.guests.leased.lock().unwrap() = false;
    s.cluster.world.lock().unwrap().owner = Some(host(3));
    assert_eq!(s.upkeep_at(NOW_MS + 10_000).await, Report::default());
    assert_eq!(s.pit_record(pit).unwrap().presence, Presence::Snapshot);
}

#[test]
fn an_appliance_is_late_after_three_minutes_without_a_check_in() {
    let settings = BackupSettings::default();
    let mut a = appliance(1, Some(NOW_MS - 180_000));
    a.heads.insert(
        vm_uuid(),
        ReportedHead {
            branch_uuid: Uuid::nil(),
            pit_uuid: Uuid::nil(),
        },
    );
    assert_eq!(
        appliance_state(&a, &vm_uuid(), &[], NOW_MS / 1000, &settings),
        ApplianceState::Active
    );
    a.last_checkin_ms = Some(NOW_MS - 181_000);
    assert_eq!(
        appliance_state(&a, &vm_uuid(), &[], NOW_MS / 1000, &settings),
        ApplianceState::Late
    );
}

mod policy_records {
    use super::*;
    use crate::backup::policies::{self, PolicyChange};
    use crate::schedules::{Recurrence, Schedules};

    fn change(appliances: Vec<Uuid>, revision: Option<u64>) -> PolicyChange {
        PolicyChange {
            vm: "prod1".parse().unwrap(),
            every: policies::default_schedule(),
            appliances,
            local_retention: LocalRetention::default(),
            freeze: Freeze::IfAvailable,
            accept_multi_pool: false,
            enabled: true,
            revision,
        }
    }

    #[test]
    fn a_policy_owns_the_schedule_that_runs_its_capture() {
        let s = setup();
        let schedules = Schedules::new(
            Pmxcfs::new(s._dir.path(), Arc::new(RealFs)),
            s._dir.path().join("priv/bmac/schedules"),
        );
        s.save(policy(vec![]), vec![appliance(1, None)]);
        s.store.locked(|l| l.delete_policy(&vm_uuid())).unwrap();

        let saved = policies::save(
            &s.store,
            &schedules,
            &s.guests,
            change(vec![appliance_uuid(1)], None),
            "admin",
            5,
        )
        .unwrap();
        assert_eq!(saved.revision, 1);
        let schedule = schedules.get(&saved.schedule_id).unwrap().unwrap();
        assert_eq!(schedule.operation.as_str(), "backup.capture");
        assert_eq!(schedule.parameters["vm"], "prod1");
        assert_eq!(schedule.every, Recurrence::Daily { at: "03:00".into() });

        let stale = policies::save(
            &s.store,
            &schedules,
            &s.guests,
            change(vec![], None),
            "admin",
            6,
        );
        assert_eq!(
            stale.unwrap_err().code,
            bmac_proto::api::ErrorCode::Conflict
        );
        let mut off = change(vec![], Some(1));
        off.enabled = false;
        off.every = Recurrence::Interval { minutes: 60 };
        let saved = policies::save(&s.store, &schedules, &s.guests, off, "admin", 7).unwrap();
        let schedule = schedules.get(&saved.schedule_id).unwrap().unwrap();
        assert!(!schedule.enabled && schedule.revision == 2);

        let unknown = policies::save(
            &s.store,
            &schedules,
            &s.guests,
            change(vec![appliance_uuid(9)], Some(2)),
            "admin",
            8,
        );
        assert!(unknown.unwrap_err().field_errors.contains_key("appliances"));
        let mut too_much = change(vec![], Some(2));
        too_much.local_retention.days = 400;
        assert!(policies::save(&s.store, &schedules, &s.guests, too_much, "admin", 8).is_err());

        policies::delete(&s.store, &schedules, &vm_uuid(), 2).unwrap();
        assert!(schedules.get(&saved.schedule_id).unwrap().is_none());
        assert!(s.store.policy(&vm_uuid()).unwrap().is_none());
    }
}

#[tokio::test]
async fn the_backups_observation_names_who_is_behind() {
    use crate::schedules::Schedules;
    let s = setup();
    let schedules = Schedules::new(
        Pmxcfs::new(s._dir.path(), Arc::new(RealFs)),
        s._dir.path().join("priv/bmac/schedules"),
    );
    let mut p = policy(vec![appliance_uuid(1), appliance_uuid(2)]);
    p.updated_at_ms = NOW_MS - 3 * 24 * 3_600_000;
    s.save(
        p,
        vec![appliance(1, Some(NOW_MS)), appliance(2, Some(NOW_MS))],
    );
    let pit = s.captured().await;
    let later = NOW_MS + 301_000;
    s.set_offer(pit, appliance_uuid(1), Offer::Published);
    s.report_head(appliance_uuid(1), pit, later);

    let seen = super::status::observe(&s.store, &schedules, later);
    assert!(seen.errors.is_empty(), "{:?}", seen.errors);
    let states: Vec<_> = seen
        .appliances
        .iter()
        .map(|a| (a.name.as_str(), a.state.as_str()))
        .collect();
    assert_eq!(states, [("appliance-1", "ACTIVE"), ("appliance-2", "LATE")]);
    let vm = &seen.vms[0];
    assert_eq!(vm.last_attempt.as_ref().unwrap().outcome, "captured");
    assert_eq!((vm.pits, vm.snapshots), (1, 1));
    assert_eq!(
        vm.unclaimed,
        ["appliance-2"],
        "its offer's claim window passed"
    );
    assert_eq!(
        vm.overdue,
        ["appliance-2"],
        "nothing published in two days of a daily schedule"
    );
    assert_eq!((vm.healthy_appliances, vm.min_healthy), (1, 1));
}

mod source_service {
    use super::*;
    use crate::backup::source::{Source, enrolled_fingerprints};
    use bmac_proto::api::ErrorCode;
    use bmac_proto::appliance::{
        ApplianceProgress, BaseKind, Capacity, CheckinRequest, ClaimRequest, HeadReport,
        SendRequest, TransferReport,
    };

    const FP: &str = "0000000000000000000000000000000000000000000000000000000000000000";

    fn addresses(host: HostSlot) -> Vec<String> {
        vec![format!("bmaclab-{host}")]
    }

    fn source<'a>(s: &'a Setup, now: &'a (dyn Fn() -> i64 + Send + Sync)) -> Source<'a> {
        Source {
            store: &s.store,
            cluster: &s.cluster,
            guests: &s.guests,
            addresses: &addresses,
            cluster_uuid: "c0ffee".into(),
            now_ms: now,
        }
    }

    fn checkin(heads: Vec<HeadReport>, transfers: Vec<TransferReport>) -> CheckinRequest {
        CheckinRequest {
            appliance_uuid: appliance_uuid(1),
            version: "3.0.0".into(),
            locked: false,
            pool_health: "ONLINE".into(),
            capacity: Capacity::default(),
            progress: ApplianceProgress::Normal,
            heads,
            transfers,
            failed: vec![],
            index_revision: 0,
            scrub: None,
        }
    }

    fn head(s: &Setup, pit: Uuid) -> HeadReport {
        HeadReport {
            vm_uuid: vm_uuid(),
            branch_uuid: s.pit_record(pit).unwrap().branch_uuid,
            pit_uuid: pit,
        }
    }

    fn volumes(s: &Setup, pit: Uuid) -> Vec<(Uuid, String)> {
        let manifest = Manifest::open(&s.pit_record(pit).unwrap().manifest).unwrap();
        manifest
            .volumes
            .iter()
            .map(|v| (v.volume_uuid.parse().unwrap(), v.dataset.clone()))
            .collect()
    }

    fn send(pit: Uuid, volume: Uuid, base: Option<Uuid>) -> SendRequest {
        SendRequest {
            transfer_uuid: Uuid::from_u128(77),
            vm_uuid: vm_uuid(),
            pit_uuid: pit,
            volume_uuid: volume,
            base,
            resume_token: None,
            large_blocks: true,
            compressed: false,
        }
    }

    #[tokio::test]
    async fn an_appliance_is_offered_claims_pulls_and_publishes_then_goes_on_from_the_bookmark() {
        let s = setup();
        s.save(
            policy(vec![appliance_uuid(1)]),
            vec![appliance(1, Some(NOW_MS))],
        );
        let p1 = s.captured().await;
        let at = Arc::new(std::sync::atomic::AtomicI64::new(NOW_MS + 10_000));
        let clock = at.clone();
        let now = move || clock.load(std::sync::atomic::Ordering::SeqCst);
        let src = source(&s, &now);

        let reply = src.checkin(FP, &checkin(vec![], vec![])).unwrap();
        assert_eq!(reply.state, "SEEDING");
        let [offer] = &reply.offers[..] else {
            panic!("{reply:?}")
        };
        assert_eq!(
            (offer.pit_uuid, offer.base.clone()),
            (p1, None),
            "a first pull is a full send"
        );
        let hosts: Vec<_> = offer.hosts.iter().map(|h| h.addresses[0].clone()).collect();
        assert_eq!(hosts, ["bmaclab-mox1", "bmaclab-mox3"]);
        assert!(!reply.want_index);

        src.claim(
            FP,
            &ClaimRequest {
                vm_uuid: vm_uuid(),
                pit_uuid: p1,
            },
        )
        .unwrap();
        let (volume, dataset) = volumes(&s, p1)[1].clone();
        let plan = src
            .authorize_send(FP, &send(p1, volume, None))
            .await
            .unwrap();
        let name = s.pit_record(p1).unwrap().name.to_string();
        assert_eq!(
            plan.send_args(),
            ["send", "-L", &format!("{dataset}@{name}")]
        );
        assert_eq!(
            plan.hold_tag(),
            format!("bmac-send-{}", Uuid::from_u128(77))
        );

        let moving = TransferReport {
            transfer_uuid: Uuid::from_u128(77),
            vm_uuid: vm_uuid(),
            pit_uuid: p1,
            bytes: 5,
        };
        at.store(NOW_MS + 70_000, std::sync::atomic::Ordering::SeqCst);
        src.checkin(FP, &checkin(vec![], vec![moving.clone()]))
            .unwrap();
        assert_eq!(
            s.pit_record(p1).unwrap().offers[&appliance_uuid(1)],
            Offer::Claimed {
                progress_s: (NOW_MS + 70_000) / 1000,
                bytes: 5
            }
        );
        at.store(NOW_MS + 130_000, std::sync::atomic::Ordering::SeqCst);
        src.checkin(FP, &checkin(vec![], vec![moving])).unwrap();
        assert_eq!(
            s.pit_record(p1).unwrap().offers[&appliance_uuid(1)],
            Offer::Claimed {
                progress_s: (NOW_MS + 70_000) / 1000,
                bytes: 5
            },
            "no bytes moved, so no progress"
        );
        let reply = src
            .checkin(FP, &checkin(vec![head(&s, p1)], vec![]))
            .unwrap();
        assert_eq!(
            s.pit_record(p1).unwrap().offers[&appliance_uuid(1)],
            Offer::Published
        );
        assert!(reply.offers.is_empty());
        assert_eq!(reply.state, "ACTIVE");
        let stored = s.store.appliance(&appliance_uuid(1)).unwrap().unwrap();
        assert_eq!(stored.heads[&vm_uuid()].pit_uuid, p1);

        s.upkeep_at(NOW_MS + 131_000).await;
        assert_eq!(s.pit_record(p1).unwrap().presence, Presence::Bookmark);
        s.report_head(appliance_uuid(1), p1, NOW_MS + 131_000);
        s.clock
            .store(NOW_MS + 200_000, std::sync::atomic::Ordering::SeqCst);
        let p2 = s.captured().await;
        at.store(NOW_MS + 210_000, std::sync::atomic::Ordering::SeqCst);
        let reply = src
            .checkin(FP, &checkin(vec![head(&s, p1)], vec![]))
            .unwrap();
        let [offer] = &reply.offers[..] else {
            panic!("{reply:?}")
        };
        assert_eq!(offer.pit_uuid, p2);
        assert_eq!(
            offer.base.as_ref().map(|b| (b.pit_uuid, b.kind)),
            Some((p1, BaseKind::Bookmark))
        );
        let plan = src
            .authorize_send(FP, &send(p2, volume, Some(p1)))
            .await
            .unwrap();
        let name2 = s.pit_record(p2).unwrap().name.to_string();
        assert_eq!(
            plan.send_args(),
            [
                "send",
                "-L",
                "-i",
                &format!("{dataset}#{name}"),
                &format!("{dataset}@{name2}")
            ]
        );

        s.cluster
            .world
            .lock()
            .unwrap()
            .tokens
            .insert("1-abc-2".into(), format!("{dataset}@{name2}"));
        let mut resume = send(p2, volume, Some(p1));
        resume.resume_token = Some("1-abc-2".into());
        let plan = src.authorize_send(FP, &resume).await.unwrap();
        assert_eq!(plan.send_args(), ["send", "-L", "-t", "1-abc-2"]);
        let other = volumes(&s, p2)[0].1.clone();
        s.cluster
            .world
            .lock()
            .unwrap()
            .tokens
            .insert("1-abc-3".into(), format!("{other}@{name2}"));
        resume.resume_token = Some("1-abc-3".into());
        let refused = src.authorize_send(FP, &resume).await.unwrap_err();
        assert_eq!(
            refused.code,
            ErrorCode::Forbidden,
            "a token for another volume"
        );
    }

    #[tokio::test]
    async fn a_new_branch_is_offered_from_the_fork_point_the_appliance_holds() {
        let s = setup();
        s.save(
            policy(vec![appliance_uuid(1)]),
            vec![appliance(1, Some(NOW_MS))],
        );
        let p1 = s.captured().await;
        s.clock
            .store(NOW_MS + 60_000, std::sync::atomic::Ordering::SeqCst);
        let p2 = s.captured().await;
        let fork = s.pit_record(p1).unwrap();
        let mut forked = s.pit_record(p2).unwrap();
        assert_eq!(forked.parent, Some(p1));
        forked.branch_uuid = Uuid::from_u128(0xb);
        s.store.locked(|l| l.save_pit(forked).map(drop)).unwrap();
        let now = || NOW_MS + 70_000;
        let src = source(&s, &now);

        let reply = src.checkin(FP, &checkin(vec![], vec![])).unwrap();
        assert_eq!(reply.offers[0].base, None, "nothing held: a full send");

        let indexed = bmac_proto::appliance::IndexedPit {
            vm_uuid: vm_uuid().to_string(),
            branch_uuid: fork.branch_uuid.to_string(),
            pit_uuid: p1.to_string(),
            parent: None,
            name: fork.name.to_string(),
            taken_s: fork.taken_s,
            bytes: 1,
        };
        s.store
            .locked(|l| {
                l.save_index(crate::backup::store::IndexRecord {
                    schema_version: 1,
                    record_type: "backup_appliance_index".into(),
                    revision: 0,
                    appliance_uuid: appliance_uuid(1),
                    index_revision: 1,
                    reported_at_ms: NOW_MS,
                    pits: vec![indexed],
                })
                .map(drop)
            })
            .unwrap();
        let reply = src.checkin(FP, &checkin(vec![], vec![])).unwrap();
        let [offer] = &reply.offers[..] else {
            panic!("{reply:?}")
        };
        assert_eq!(
            (offer.pit_uuid, offer.branch_uuid),
            (p2, Uuid::from_u128(0xb))
        );
        assert_eq!(
            offer.base.as_ref().map(|b| (b.pit_uuid, b.kind)),
            Some((p1, BaseKind::Snapshot)),
            "branch B's first PIT comes from the fork point, received as a clone"
        );
        let (volume, _) = volumes(&s, p2)[1].clone();
        src.authorize_send(FP, &send(p2, volume, Some(p1)))
            .await
            .unwrap();
        let p0 = Uuid::from_u128(0xdead);
        let refused = src
            .authorize_send(FP, &send(p2, volume, Some(p0)))
            .await
            .unwrap_err();
        assert_eq!(refused.code, ErrorCode::Invalid);
    }

    #[tokio::test]
    async fn sends_outside_the_offer_and_strangers_are_refused() {
        let s = setup();
        let mut stranger = appliance(2, Some(NOW_MS));
        stranger.fingerprint = "11".repeat(32);
        let mut retired = appliance(3, Some(NOW_MS));
        retired.fingerprint = "22".repeat(32);
        retired.retired = true;
        s.save(
            policy(vec![appliance_uuid(1)]),
            vec![appliance(1, Some(NOW_MS)), stranger, retired],
        );
        let p1 = s.captured().await;
        let now = || NOW_MS + 10_000;
        let src = source(&s, &now);
        let (volume, _) = volumes(&s, p1)[0].clone();
        fn code<T>(r: Result<T, bmac_proto::api::ApiError>) -> ErrorCode {
            r.map(drop).unwrap_err().code
        }

        assert_eq!(
            code(src.checkin(&"33".repeat(32), &checkin(vec![], vec![]))),
            ErrorCode::Forbidden
        );
        assert_eq!(
            code(src.checkin(&"22".repeat(32), &checkin(vec![], vec![]))),
            ErrorCode::Forbidden
        );
        let mut posing = checkin(vec![], vec![]);
        posing.appliance_uuid = appliance_uuid(2);
        assert_eq!(
            code(src.checkin(FP, &posing)),
            ErrorCode::Forbidden,
            "naming another appliance"
        );
        assert_eq!(
            code(
                src.authorize_send(&"11".repeat(32), &send(p1, volume, None))
                    .await
            ),
            ErrorCode::Forbidden,
            "the VM isn't assigned to it"
        );
        assert_eq!(
            code(
                src.authorize_send(FP, &send(p1, Uuid::from_u128(5), None))
                    .await
            ),
            ErrorCode::Invalid
        );
        assert_eq!(
            code(src.authorize_send(FP, &send(p1, volume, Some(p1))).await),
            ErrorCode::Invalid,
            "a base must be earlier"
        );
        let mut fps = enrolled_fingerprints(&s.store).unwrap();
        fps.sort();
        assert_eq!(fps, ["00".repeat(32), "11".repeat(32)]);

        s.cluster
            .world
            .lock()
            .unwrap()
            .names
            .get_mut(&host(1))
            .unwrap()
            .values_mut()
            .for_each(|g| *g = SnapshotGuid::new(1));
        assert_eq!(
            code(src.authorize_send(FP, &send(p1, volume, None)).await),
            ErrorCode::NotFound,
            "this host's snapshot has another GUID"
        );
        s.set_offer(p1, appliance_uuid(1), Offer::Failed);
        assert_eq!(
            code(src.authorize_send(FP, &send(p1, volume, None)).await),
            ErrorCode::Forbidden
        );
        assert_eq!(
            code(src.claim(
                FP,
                &ClaimRequest {
                    vm_uuid: vm_uuid(),
                    pit_uuid: p1
                }
            )),
            ErrorCode::Conflict
        );
    }
}
