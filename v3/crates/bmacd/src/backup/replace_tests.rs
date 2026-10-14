// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

use std::sync::Mutex;

use bmac_backup::lifetime::Offer;
use bmac_backup::manifest::Consistency;
use bmac_core::SnapshotGuid;
use bmac_proto::bmac_ui::WorkflowEvent;

use super::*;
use crate::backup::restore::tests::{
    CLUSTER, Dataset, Fake, PIT_NAME, Rig, STORAGE_CFG_TEXT, World, drive, id, manifest, ok, rig,
    source, yes,
};
use crate::native::tests::workflow;

const NOW_MS: i64 = 1_767_300_000_000;
const CONFIG: &str = "name: prod1\nscsi0: local-zfs:vm-100-disk-0,size=8G\nscsi1: bmac-zfs-64k:vm-100-disk-1,size=8G\nnet0: virtio=AA:BB:CC:DD:EE:FF,bridge=vmbr1\nmemory: 2048\n";
const DISK0: &str = "rpool/data/vm-100-disk-0";
const DISK1: &str = "rpool/data-64k/vm-100-disk-1";

fn me() -> HostSlot {
    "mox1".parse().unwrap()
}

fn job(n: u32, target: &str) -> Job {
    Job {
        id: format!("100-{n}"),
        target: target.parse().unwrap(),
        schedule: "*/15".into(),
        rate: None,
        comment: Some("Production web replica".into()),
    }
}

/// Production VM 100, `web`, on mox1 under HA, replicated to mox2 and mox3,
/// with an unrelated older snapshot on each disk.
fn production(w: &mut World) {
    w.quorate = true;
    w.configs.insert(100, CONFIG.into());
    w.owners.insert(100, me());
    w.ha.insert(100, true);
    w.running.insert(100);
    w.jobs = vec![(100, job(0, "mox2"), 5), (100, job(1, "mox3"), 5)];
    for disk in [DISK0, DISK1] {
        w.datasets.insert(
            disk.into(),
            Dataset {
                volsize: 8 << 30,
                ..Dataset::default()
            },
        );
        w.snapshots.insert(
            format!("{disk}@bmac-pit-20251201T000000Z-000000"),
            SnapshotGuid::new(999),
        );
    }
}

#[derive(Default)]
struct Leases(Mutex<Vec<String>>);

impl Guests for Leases {
    fn production(&self, name: &GuestName) -> Result<Production, String> {
        if name.to_string() != "prod1" {
            return Err(format!("the registry has no {name}"));
        }
        Ok(Production {
            vm_uuid: id(7),
            name: *name,
            vmid: 100,
            placement: vec![me(), "mox2".parse().unwrap(), "mox3".parse().unwrap()],
            record: Value::Null,
        })
    }

    fn by_uuid(&self, _vm: &Uuid) -> Result<Option<Production>, String> {
        unreachable!()
    }

    fn leased(&self, _name: &GuestName) -> Result<bool, String> {
        unreachable!()
    }

    fn acquire(&self, name: &GuestName, ttl_s: u32) -> BoxFuture<'_, Result<String, String>> {
        self.0
            .lock()
            .unwrap()
            .push(format!("acquire {name} {ttl_s}"));
        ok(Ok("nonce".into()))
    }

    fn release(&self, name: &GuestName, nonce: &str) -> BoxFuture<'_, Result<(), String>> {
        self.0
            .lock()
            .unwrap()
            .push(format!("release {name} {nonce}"));
        ok(Ok(()))
    }
}

impl ReplaceHost for Fake {
    fn quorate(&self) -> BoxFuture<'_, Result<bool, String>> {
        ok(Ok(self.w().quorate))
    }

    fn owner(&self, vmid: u32) -> BoxFuture<'_, Result<Option<HostSlot>, String>> {
        ok(Ok(self.w().owners.get(&vmid).copied()))
    }

    fn config_text(&self, vmid: u32) -> BoxFuture<'_, Result<String, String>> {
        let text = self.w().configs.get(&vmid).cloned();
        ok(text.ok_or_else(|| format!("VM {vmid} has no configuration")))
    }

    fn dataset(&self, volume_id: &str) -> BoxFuture<'_, Result<String, String>> {
        let (storage, volume) = volume_id.split_once(':').unwrap();
        let found = job::parse_storages(STORAGE_CFG_TEXT, "mox1")
            .into_iter()
            .find(|s| s.id == storage)
            .map(|s| format!("{}/{volume}", s.dataset));
        ok(found.ok_or_else(|| format!("{storage} isn't a ZFS storage")))
    }

    fn ha_managed(&self, vmid: u32) -> BoxFuture<'_, Result<bool, String>> {
        ok(Ok(self.w().ha.contains_key(&vmid)))
    }

    fn set_ha(&self, vmid: u32, started: bool) -> BoxFuture<'_, Result<(), String>> {
        let mut w = self.w();
        w.log.push(format!("ha vm:{vmid} {started}"));
        w.ha.insert(vmid, started);
        if started {
            w.running.insert(vmid);
        } else {
            w.running.remove(&vmid);
        }
        ok(Ok(()))
    }

    fn running(&self, vmid: u32) -> BoxFuture<'_, Result<bool, String>> {
        ok(Ok(self.w().running.contains(&vmid)))
    }

    fn shutdown(&self, vmid: u32) -> BoxFuture<'_, Result<(), String>> {
        let mut w = self.w();
        w.log.push(format!("shutdown {vmid}"));
        w.running.remove(&vmid);
        ok(Ok(()))
    }

    fn start(&self, vmid: u32) -> BoxFuture<'_, Result<(), String>> {
        let mut w = self.w();
        w.log.push(format!("start {vmid}"));
        w.running.insert(vmid);
        ok(Ok(()))
    }

    fn jobs(&self, vmid: u32) -> BoxFuture<'_, Result<Vec<Job>, String>> {
        let jobs = self
            .w()
            .jobs
            .iter()
            .filter(|(v, _, _)| *v == vmid)
            .map(|(_, j, _)| j.clone())
            .collect();
        ok(Ok(jobs))
    }

    fn delete_job(&self, id: &str) -> BoxFuture<'_, Result<(), String>> {
        let mut w = self.w();
        w.log.push(format!("delete job {id}"));
        w.jobs.retain(|(_, j, _)| j.id != id);
        ok(Ok(()))
    }

    fn create_job(&self, job: &Job) -> BoxFuture<'_, Result<(), String>> {
        let mut w = self.w();
        w.log.push(format!("create job {} {}", job.id, job.target));
        let vmid = job.id.split('-').next().unwrap().parse().unwrap();
        w.jobs.push((vmid, job.clone(), 0));
        ok(Ok(()))
    }

    fn job_state(&self, id: &str) -> BoxFuture<'_, Result<JobState, String>> {
        let w = self.w();
        let last = w
            .jobs
            .iter()
            .find(|(_, j, _)| j.id == id)
            .map(|(_, _, l)| *l);
        let fail_count = w.sync_fails.get(id).copied().unwrap_or(0);
        ok(last
            .map(|last_sync_s| JobState {
                busy: false,
                failing: fail_count > 0,
                last_sync_s,
                fail_count,
                error: w.sync_error.clone().filter(|_| fail_count > 0),
            })
            .ok_or_else(|| format!("no job {id}")))
    }

    fn schedule_now(&self, id: &str) -> BoxFuture<'_, Result<(), String>> {
        let mut w = self.w();
        w.log.push(format!("schedule {id}"));
        if w.sync_error.is_some() {
            *w.sync_fails.entry(id.to_string()).or_default() += 1;
            return ok(Ok(()));
        }
        for (_, j, last) in w.jobs.iter_mut() {
            if j.id == id {
                *last = NOW_MS / 1000;
            }
        }
        ok(Ok(()))
    }

    fn ensure_parent(&self, dataset: &str) -> BoxFuture<'_, Result<(), String>> {
        self.w().log.push(format!("parent of {dataset}"));
        ok(Ok(()))
    }

    fn rename(&self, from: &str, to: &str) -> BoxFuture<'_, Result<(), String>> {
        let mut w = self.w();
        if w.fail_rename.as_deref() == Some(from) {
            w.fail_rename = None;
            return ok(Err(format!("zfs rename {from}: dataset is busy")));
        }
        w.log.push(format!("rename {from} {to}"));
        w.move_dataset(from, to);
        ok(Ok(()))
    }

    fn inherit_volmode(&self, _dataset: &str) -> BoxFuture<'_, Result<(), String>> {
        ok(Ok(()))
    }

    fn promote(&self, dataset: &str) -> BoxFuture<'_, Result<(), String>> {
        let mut w = self.w();
        w.log.push(format!("promote {dataset}"));
        let origin = w.datasets.get(dataset).and_then(|d| d.origin.clone());
        if let Some((old, snap)) = origin.as_deref().and_then(|o| o.split_once('@')) {
            // As ZFS does: the origin's snapshots up to the branch point
            // move over, and the clone takes the origin's own origin.
            let prefix = format!("{old}@");
            let moving: Vec<String> = w
                .snapshots
                .keys()
                .filter(|n| n.strip_prefix(&prefix).is_some_and(|s| s <= snap))
                .cloned()
                .collect();
            for name in moving {
                let guid = w.snapshots.remove(&name).unwrap();
                w.snapshots
                    .insert(format!("{dataset}@{}", &name[prefix.len()..]), guid);
            }
            let above = w.datasets.get(old).and_then(|d| d.origin.clone());
            if let Some(d) = w.datasets.get_mut(dataset) {
                d.origin = above;
            }
            if let Some(d) = w.datasets.get_mut(old) {
                d.origin = Some(format!("{dataset}@{snap}"));
            }
        }
        ok(Ok(()))
    }

    fn snapshot(&self, name: &str) -> BoxFuture<'_, Result<(), String>> {
        let mut w = self.w();
        w.log.push(format!("snapshot {name}"));
        let guid = SnapshotGuid::new(5000 + w.snapshots.len() as u64);
        w.snapshots.insert(name.to_string(), guid);
        ok(Ok(()))
    }

    fn copy_clone(
        &self,
        origin: &str,
        snapshot: &str,
        target: &str,
    ) -> BoxFuture<'_, Result<(), String>> {
        let mut w = self.w();
        w.log.push(format!("copy {origin} {snapshot} {target}"));
        let source = snapshot.split('@').next().unwrap();
        let volsize = w.datasets[source].volsize;
        w.datasets.insert(
            target.to_string(),
            Dataset {
                volsize,
                ..Dataset::default()
            },
        );
        for from in [origin, snapshot] {
            let guid = w.snapshots[from];
            let name = from.split('@').nth(1).unwrap();
            w.snapshots.insert(format!("{target}@{name}"), guid);
        }
        ok(Ok(()))
    }

    fn guid(&self, snapshot: &str) -> BoxFuture<'_, Result<Option<String>, String>> {
        ok(Ok(self.w().snapshots.get(snapshot).map(|g| g.to_string())))
    }

    fn destroy(&self, name: &str) -> BoxFuture<'_, Result<(), String>> {
        let mut w = self.w();
        w.log.push(format!("destroy {name}"));
        if w.snapshots.remove(name).is_none() {
            let prefix = format!("{name}@");
            w.datasets.remove(name);
            w.snapshots.retain(|n, _| !n.starts_with(&prefix));
        }
        ok(Ok(()))
    }

    fn origin(&self, dataset: &str) -> BoxFuture<'_, Result<Option<String>, String>> {
        ok(Ok(self
            .w()
            .datasets
            .get(dataset)
            .and_then(|d| d.origin.clone())))
    }

    fn snapshots_of(&self, dataset: &str) -> BoxFuture<'_, Result<Vec<String>, String>> {
        let prefix = format!("{dataset}@");
        let names = self
            .w()
            .snapshots
            .keys()
            .filter(|n| n.starts_with(&prefix))
            .cloned()
            .collect();
        ok(Ok(names))
    }

    fn clones(&self, snapshot: &str) -> BoxFuture<'_, Result<Vec<String>, String>> {
        let clones = self
            .w()
            .datasets
            .iter()
            .filter(|(_, d)| d.origin.as_deref() == Some(snapshot))
            .map(|(n, _)| n.clone())
            .collect();
        ok(Ok(clones))
    }

    fn rollback(&self, snapshot: &str) -> BoxFuture<'_, Result<(), String>> {
        let mut w = self.w();
        w.log.push(format!("rollback {snapshot}"));
        let dataset = snapshot.split('@').next().unwrap().to_string();
        let prefix = format!("{dataset}@");
        w.snapshots
            .retain(|n, _| !n.starts_with(&prefix) || n.as_str() <= snapshot);
        ok(Ok(()))
    }

    fn rescan(&self, vmid: u32) -> BoxFuture<'_, Result<(), String>> {
        self.w().log.push(format!("rescan {vmid}"));
        ok(Ok(()))
    }

    fn forget_vm(&self, vmid: u32) -> BoxFuture<'_, Result<(), String>> {
        let mut w = self.w();
        w.log.push(format!("forget {vmid}"));
        w.configs.remove(&vmid);
        w.owners.remove(&vmid);
        ok(Ok(()))
    }
}

struct Ran {
    outcome: Result<Value, Stop>,
    events: Vec<WorkflowEvent>,
    leases: Vec<String>,
}

impl Ran {
    fn said(&self, text: &str) -> bool {
        self.events.iter().any(|e| match e {
            WorkflowEvent::Info { message } | WorkflowEvent::Warning { message } => {
                message.contains(text)
            }
            _ => false,
        })
    }

    fn message(&self) -> String {
        match &self.outcome {
            Err(Stop::Failed { message, .. }) => message.clone(),
            other => panic!("{other:?}"),
        }
    }
}

async fn run(r: &Rig, operation: &str, answers: Vec<Value>) -> Ran {
    let work = tempfile::tempdir().unwrap();
    let (flow, out, input) = workflow(work.path());
    let now = || NOW_MS;
    let leases = Leases::default();
    let cx = Context {
        restore: job::Context {
            flow: &flow,
            host: &r.fake,
            transfers: &r.fake,
            store: &r.store,
            cluster_uuid: CLUSTER.into(),
            now_ms: &now,
            agent_wait: Duration::from_millis(50),
            poll: Duration::from_millis(1),
        },
        host: &r.fake,
        guests: &leases,
        stop_wait: Duration::from_millis(200),
        replication_timeout: Duration::from_millis(500),
        ask_every: Duration::from_millis(1),
    };
    let vm: GuestName = "prod1".parse().unwrap();
    let work = async {
        match operation {
            RESTORE_INTO => restore_into(&cx, &vm).await,
            ROLLBACK => rollback(&cx, &vm).await,
            _ => promote(&cx, &vm).await,
        }
    };
    let outcome = tokio::select! {
        outcome = work => outcome,
        () = drive(out.clone(), input, answers) => unreachable!(),
    };
    Ran {
        outcome,
        events: out.events(),
        leases: leases.0.lock().unwrap().clone(),
    }
}

fn pit(n: u128) -> Value {
    json!({"values": {"pit": id(n).to_string()}})
}

#[tokio::test]
async fn a_restore_into_the_vm_sets_the_disks_aside_and_swaps_in_the_pit() {
    let r = rig();
    production(&mut r.fake.w());
    let ran = run(
        &r,
        RESTORE_INTO,
        vec![source("bmac-grant-good"), pit(3), yes()],
    )
    .await;
    ran.outcome.as_ref().unwrap();

    let w = r.fake.w();
    let aside0 = format!("rpool/bmac-replaced/100/{}-{}", id(0xd0), NOW_MS / 1000);
    assert!(w.datasets.contains_key(&aside0), "{:?}", w.datasets.keys());
    assert!(
        w.snapshots
            .contains_key(&format!("{aside0}@bmac-pit-20251201T000000Z-000000")),
        "the old disk keeps its snapshots"
    );
    assert_eq!(
        w.snapshots.get(&format!("{DISK0}@{PIT_NAME}")),
        Some(&SnapshotGuid::new(300)),
        "the disk is the PIT now"
    );
    assert_eq!(
        w.snapshots.get(&format!("{DISK1}@{PIT_NAME}")),
        Some(&SnapshotGuid::new(310))
    );
    assert!(w.datasets.keys().all(|d| !d.contains("bmac-restore")));
    let order: Vec<&String> = w.log.iter().filter(|l| !l.starts_with("parent")).collect();
    let at = |prefix: &str| {
        order
            .iter()
            .position(|l| l.starts_with(prefix))
            .unwrap_or_else(|| panic!("no {prefix}: {order:#?}"))
    };
    assert!(at("ha vm:100 false") < at("delete job 100-0"));
    assert!(at("delete job 100-1") < at(&format!("rename {DISK0}")));
    assert!(at(&format!("rename {DISK1}")) < at("create job 100-0"));
    assert!(at("create job 100-1") < at("ha vm:100 true"));
    assert_eq!(w.jobs.len(), 2, "replication is back");
    assert!(w.running.contains(&100));
    drop(w);
    assert_eq!(
        ran.leases,
        vec!["acquire prod1 172800", "release prod1 nonce"]
    );

    let records = r.store.restores().unwrap();
    let record = &records[0];
    assert_eq!(record.mode, Mode::IntoExisting);
    assert!(record.stands() && record.continues(NOW_MS));
    assert_eq!(record.replaced["scsi0"], aside0);
    let lineage = r.store.lineage(&id(7)).unwrap().unwrap();
    assert_eq!(lineage.head, Some(id(3)));
    assert_ne!(lineage.branch_uuid, id(0xb), "a new branch");
    assert_eq!(lineage.volumes[DISK0], id(0xd0));
    let catalog = r.store.pit(&id(7), &id(3)).unwrap().unwrap();
    assert_eq!(
        catalog.branch_uuid,
        id(0xb),
        "the PIT stays on its own branch"
    );
    assert!(ran.said("Backups continue from the PIT on a new branch"));
}

#[tokio::test]
async fn a_disk_cloned_from_one_set_aside_earlier_is_promoted_until_it_owns_its_history() {
    let r = rig();
    let earlier = format!("rpool/bmac-replaced/100/{}-5", id(0xd0));
    {
        // An earlier promotion set aside the disk that holds the PIT, a
        // clone of the current disk's older snapshot.
        let mut w = r.fake.w();
        production(&mut w);
        w.datasets.insert(
            earlier.clone(),
            Dataset {
                volsize: 8 << 30,
                origin: Some(format!("{DISK0}@bmac-pit-20251201T000000Z-000000")),
                resume_token: None,
            },
        );
        w.snapshots
            .insert(format!("{earlier}@{PIT_NAME}"), SnapshotGuid::new(300));
    }
    let ran = run(
        &r,
        RESTORE_INTO,
        vec![source("bmac-grant-good"), pit(3), yes()],
    )
    .await;
    ran.outcome.as_ref().unwrap();

    let w = r.fake.w();
    assert!(
        w.log
            .iter()
            .any(|l| l.starts_with(&format!("clone {earlier}@{PIT_NAME} "))),
        "method 1, from the disk set aside: {:#?}",
        w.log
    );
    let promotes = w
        .log
        .iter()
        .filter(|l| **l == format!("promote {DISK0}"))
        .count();
    assert_eq!(
        promotes, 2,
        "over the earlier disk, then the one set aside now"
    );
    assert_eq!(w.datasets[DISK0].origin, None, "replication can send it");
    assert_eq!(
        w.snapshots.get(&format!("{DISK0}@{PIT_NAME}")),
        Some(&SnapshotGuid::new(300))
    );
    assert_eq!(
        w.snapshots
            .get(&format!("{DISK0}@bmac-pit-20251201T000000Z-000000")),
        Some(&SnapshotGuid::new(999))
    );
    assert!(w.running.contains(&100));
}

#[tokio::test]
async fn a_disk_cloned_from_another_vms_disk_is_copied_and_that_disk_is_left_alone() {
    let r = rig();
    let other = "rpool/data/vm-200-disk-0";
    {
        let mut w = r.fake.w();
        production(&mut w);
        w.datasets.insert(
            other.into(),
            Dataset {
                volsize: 8 << 30,
                ..Dataset::default()
            },
        );
        w.snapshots
            .insert(format!("{other}@{PIT_NAME}"), SnapshotGuid::new(300));
    }
    let ran = run(
        &r,
        RESTORE_INTO,
        vec![source("bmac-grant-good"), pit(3), yes()],
    )
    .await;
    ran.outcome.as_ref().unwrap();

    let w = r.fake.w();
    assert!(
        !w.log.iter().any(|l| l.starts_with("promote")),
        "never promoted over another VM's disk: {:#?}",
        w.log
    );
    assert!(
        w.log
            .iter()
            .any(|l| l.starts_with(&format!("copy {other}@{PIT_NAME} {DISK0}@bmac-copy-")))
    );
    assert_eq!(w.datasets[DISK0].origin, None, "replication can send it");
    assert_eq!(
        w.snapshots.get(&format!("{DISK0}@{PIT_NAME}")),
        Some(&SnapshotGuid::new(300)),
        "the copy keeps the PIT's GUID"
    );
    assert_eq!(
        w.snapshots.get(&format!("{other}@{PIT_NAME}")),
        Some(&SnapshotGuid::new(300)),
        "the other disk keeps its snapshot"
    );
    assert!(w.datasets.contains_key(other));
    assert!(w.snapshots.keys().all(|n| !n.contains("@bmac-copy-")));
    assert!(w.datasets.keys().all(|d| !d.contains("bmac-restore")));
    assert!(w.running.contains(&100));
}

#[tokio::test]
async fn a_replication_that_keeps_failing_stops_the_wait_with_its_error() {
    let r = rig();
    {
        let mut w = r.fake.w();
        production(&mut w);
        w.sync_error = Some("cannot receive: local origin for clone does not exist".into());
    }
    let ran = run(
        &r,
        RESTORE_INTO,
        vec![source("bmac-grant-good"), pit(3), yes()],
    )
    .await;
    let message = ran.message();
    assert!(
        message.contains("failed 3 times: cannot receive: local origin"),
        "{message}"
    );
    assert!(message.contains("it carries on in Proxmox"));
    assert_eq!(
        ran.leases,
        vec!["acquire prod1 172800", "release prod1 nonce"]
    );
}

#[tokio::test]
async fn a_failed_swap_puts_the_disks_back_and_leaves_the_vm_stopped() {
    let r = rig();
    production(&mut r.fake.w());
    r.fake.w().fail_rename = Some(DISK1.into());
    let ran = run(
        &r,
        RESTORE_INTO,
        vec![source("bmac-grant-good"), pit(3), yes()],
    )
    .await;
    let message = ran.message();
    assert!(message.contains("dataset is busy"), "{message}");
    assert!(message.contains(&format!("{DISK0} is back")), "{message}");
    let w = r.fake.w();
    assert_eq!(
        w.snapshots
            .get(&format!("{DISK0}@bmac-pit-20251201T000000Z-000000")),
        Some(&SnapshotGuid::new(999)),
        "the original disk is back under its name"
    );
    assert!(!w.snapshots.contains_key(&format!("{DISK0}@{PIT_NAME}")));
    assert!(!w.running.contains(&100), "the VM stays stopped");
    assert!(!w.ha[&100]);
    assert_eq!(w.jobs.len(), 2, "replication is back anyway");
    drop(w);
    assert!(ran.said("stays stopped"));
    let record = &r.store.restores().unwrap()[0];
    assert!(record.error.is_some() && !record.continues(NOW_MS));
    assert!(r.store.lineage(&id(7)).unwrap().is_none());
}

#[tokio::test]
async fn nothing_changes_without_quorum_or_off_the_owner() {
    let r = rig();
    production(&mut r.fake.w());
    r.fake.w().quorate = false;
    let ran = run(&r, RESTORE_INTO, vec![]).await;
    assert!(ran.message().contains("isn't quorate"));
    r.fake.w().quorate = true;
    r.fake.w().owners.insert(100, "mox2".parse().unwrap());
    let ran = run(&r, ROLLBACK, vec![]).await;
    assert!(
        ran.message().contains("prod1 runs on mox2"),
        "{}",
        ran.message()
    );
    assert!(r.fake.w().log.is_empty());
}

/// The PIT's snapshots on the current disks, and a later PIT's.
fn local_pits(r: &Rig) {
    let sealed = manifest().seal();
    let record = |n: u128, name: &str, taken_s: i64, offers| PitRecord {
        schema_version: RECORD_SCHEMA,
        record_type: String::new(),
        revision: 0,
        pit_uuid: id(n),
        vm_uuid: id(7),
        branch_uuid: id(0xb),
        parent: None,
        name: name.parse().unwrap(),
        taken_s,
        consistency: Consistency::Filesystem,
        manifest: sealed.clone(),
        presence: Presence::Snapshot,
        offers,
        release_blocked: None,
    };
    let later = "bmac-pit-20260102T000000Z-000002";
    r.store
        .locked(|l| {
            l.save_pit(record(3, PIT_NAME, 1_767_225_600, BTreeMap::new()))?;
            l.save_pit(record(
                4,
                later,
                1_767_312_000,
                BTreeMap::from([(id(0xa), Offer::Published)]),
            ))
        })
        .unwrap();
    let mut w = r.fake.w();
    for (disk, guid) in [(DISK0, 300), (DISK1, 310)] {
        w.snapshots
            .insert(format!("{disk}@{PIT_NAME}"), SnapshotGuid::new(guid));
        w.snapshots
            .insert(format!("{disk}@{later}"), SnapshotGuid::new(guid + 1));
    }
}

#[tokio::test]
async fn a_rollback_destroys_the_newer_snapshots_and_starts_a_branch() {
    let r = rig();
    production(&mut r.fake.w());
    local_pits(&r);
    let ran = run(&r, ROLLBACK, vec![pit(3), yes()]).await;
    ran.outcome.as_ref().unwrap();
    let w = r.fake.w();
    assert!(w.log.contains(&format!("rollback {DISK0}@{PIT_NAME}")));
    assert!(
        !w.snapshots
            .contains_key(&format!("{DISK0}@bmac-pit-20260102T000000Z-000002"))
    );
    assert!(w.running.contains(&100));
    drop(w);
    assert!(
        r.store.pit(&id(7), &id(4)).unwrap().is_none(),
        "the later PIT left the catalog"
    );
    let lineage = r.store.lineage(&id(7)).unwrap().unwrap();
    assert_eq!(lineage.head, Some(id(3)));
    let record = &r.store.restores().unwrap()[0];
    assert_eq!(record.mode, Mode::Rollback);
    assert!(record.methods.is_empty() && !record.holds_cluster_snapshot());
}

#[tokio::test]
async fn a_rollback_is_refused_while_a_clone_needs_a_newer_snapshot() {
    let r = rig();
    production(&mut r.fake.w());
    local_pits(&r);
    r.fake.w().datasets.insert(
        "rpool/data/staging-web".into(),
        Dataset {
            origin: Some(format!("{DISK0}@bmac-pit-20260102T000000Z-000002")),
            ..Dataset::default()
        },
    );
    let ran = run(&r, ROLLBACK, vec![pit(3)]).await;
    let message = ran.message();
    assert!(
        message.contains("has clones (rpool/data/staging-web)"),
        "{message}"
    );
    assert!(message.contains("promote it instead"));
    assert!(r.fake.w().log.is_empty(), "nothing was touched");
}

#[tokio::test]
async fn promotion_swaps_the_restored_vms_disks_in_and_keeps_the_identity() {
    let r = rig();
    production(&mut r.fake.w());
    {
        // VM 120 was restored here by method 1 for scsi0 (a clone of the
        // PIT's snapshot on production's disk) and method 3 for scsi1.
        let mut w = r.fake.w();
        w.snapshots
            .insert(format!("{DISK0}@{PIT_NAME}"), SnapshotGuid::new(300));
        w.datasets.insert(
            "rpool/data/vm-120-disk-0".into(),
            Dataset {
                volsize: 8 << 30,
                origin: Some(format!("{DISK0}@{PIT_NAME}")),
                resume_token: None,
            },
        );
        w.datasets.insert(
            "rpool/data-64k/vm-120-disk-1".into(),
            Dataset {
                volsize: 8 << 30,
                ..Dataset::default()
            },
        );
        w.snapshots.insert(
            format!("rpool/data-64k/vm-120-disk-1@{PIT_NAME}"),
            SnapshotGuid::new(310),
        );
        w.configs.insert(
            120,
            "name: restored-prod1\nscsi0: local-zfs:vm-120-disk-0,size=8G\nscsi1: bmac-zfs-64k:vm-120-disk-1,size=8G\n".into(),
        );
        w.owners.insert(120, me());
        w.running.insert(120);
    }
    let sealed = manifest().seal();
    let earlier = r
        .store
        .locked(|l| {
            l.save_restore(RestoreRecord {
                schema_version: RECORD_SCHEMA,
                record_type: String::new(),
                revision: 0,
                restore_uuid: id(0x77),
                vm_uuid: id(7),
                pit_uuid: id(3),
                branch_uuid: id(0xb),
                appliance_uuid: id(0xa),
                mode: Mode::NewVm,
                host: "mox1".into(),
                vmid: Some(120),
                name: Some("restored-prod1".into()),
                methods: BTreeMap::from([("scsi0".into(), 1), ("scsi1".into(), 3)]),
                started_at_ms: NOW_MS - 1000,
                finished_at_ms: Some(NOW_MS - 500),
                error: None,
                ended_at_ms: None,
                replaced: BTreeMap::new(),
                test: None,
                manifest: Some(sealed),
            })
        })
        .unwrap();
    let ran = run(
        &r,
        PROMOTE,
        vec![
            json!({"values": {"restored": earlier.restore_uuid.to_string()}}),
            yes(),
        ],
    )
    .await;
    ran.outcome.as_ref().unwrap();
    let w = r.fake.w();
    assert!(w.log.contains(&"shutdown 120".to_string()));
    assert!(w.log.contains(&format!("promote {DISK0}")), "{:#?}", w.log);
    assert_eq!(
        w.snapshots.get(&format!("{DISK0}@{PIT_NAME}")),
        Some(&SnapshotGuid::new(300)),
        "the promoted disk owns the PIT's snapshot"
    );
    let aside0 = format!("rpool/bmac-replaced/100/{}-{}", id(0xd0), NOW_MS / 1000);
    assert_eq!(
        w.datasets[&aside0].origin.as_deref(),
        Some(format!("{DISK0}@{PIT_NAME}").as_str()),
        "the old disk is the clone now"
    );
    assert!(w.log.contains(&"forget 120".to_string()));
    assert_eq!(
        w.configs[&100], CONFIG,
        "production keeps its configuration"
    );
    assert!(w.running.contains(&100));
    drop(w);
    let records = r.store.restores().unwrap();
    let old = records.iter().find(|r| r.restore_uuid == id(0x77)).unwrap();
    assert!(old.ended_at_ms.is_some(), "the restored VM is gone");
    let new = records.iter().find(|r| r.mode == Mode::Promotion).unwrap();
    assert!(new.continues(NOW_MS));
    assert_eq!(r.store.lineage(&id(7)).unwrap().unwrap().head, Some(id(3)));
}

#[test]
fn continuity_keeps_a_restored_pit_until_its_branch_has_one_of_its_own() {
    let pit = |n: u128, branch: u128| PitRecord {
        schema_version: RECORD_SCHEMA,
        record_type: String::new(),
        revision: 0,
        pit_uuid: id(n),
        vm_uuid: id(7),
        branch_uuid: id(branch),
        parent: None,
        name: PIT_NAME.parse().unwrap(),
        taken_s: 0,
        consistency: Consistency::Filesystem,
        manifest: String::new(),
        presence: Presence::Snapshot,
        offers: BTreeMap::new(),
        release_blocked: None,
    };
    let lineage = |head: u128| bmac_backup::records::Lineage {
        schema_version: RECORD_SCHEMA,
        record_type: String::new(),
        revision: 0,
        vm_uuid: id(7),
        branch_uuid: id(0xc),
        head: Some(id(head)),
        volumes: BTreeMap::new(),
        last_attempt: None,
    };
    let pits = vec![pit(3, 0xb), pit(5, 0xc)];
    use crate::backup::upkeep::Continuity;
    assert_eq!(
        Continuity::of(id(7), Some(&lineage(3)), &pits, &[]).branch_start,
        Some(id(3))
    );
    assert_eq!(
        Continuity::of(id(7), Some(&lineage(5)), &pits, &[]).branch_start,
        None,
        "the branch has a PIT of its own"
    );
}

async fn clean(r: &Rig, answers: Vec<Value>) -> Result<Value, Stop> {
    let work = tempfile::tempdir().unwrap();
    let (flow, out, input) = workflow(work.path());
    let now = || NOW_MS;
    let leases = Leases::default();
    let cx = Context {
        restore: job::Context {
            flow: &flow,
            host: &r.fake,
            transfers: &r.fake,
            store: &r.store,
            cluster_uuid: CLUSTER.into(),
            now_ms: &now,
            agent_wait: Duration::from_millis(50),
            poll: Duration::from_millis(1),
        },
        host: &r.fake,
        guests: &leases,
        stop_wait: Duration::from_millis(200),
        replication_timeout: Duration::from_millis(500),
        ask_every: Duration::from_millis(1),
    };
    tokio::select! {
        outcome = cleanup(&cx) => outcome,
        () = drive(out.clone(), input, answers) => unreachable!(),
    }
}

#[tokio::test]
async fn cleanup_promotes_a_restored_clone_then_destroys_the_set_aside_disk() {
    let r = rig();
    let aside = "rpool/bmac-replaced/100/d0-1767300000";
    let staged_from = "rpool/bmac-replaced/100/d1-1767300000";
    {
        let mut w = r.fake.w();
        for d in [aside, staged_from] {
            w.datasets.insert(d.into(), Dataset::default());
            w.snapshots
                .insert(format!("{d}@{PIT_NAME}"), SnapshotGuid::new(300));
        }
        w.datasets.insert(
            DISK0.into(),
            Dataset {
                origin: Some(format!("{aside}@{PIT_NAME}")),
                ..Dataset::default()
            },
        );
        w.datasets.insert(
            "rpool/bmac-restore/job/d1".into(),
            Dataset {
                origin: Some(format!("{staged_from}@{PIT_NAME}")),
                ..Dataset::default()
            },
        );
    }
    let outcome = clean(&r, vec![json!({"values": {"disks": [aside]}}), yes()])
        .await
        .unwrap();
    assert_eq!(outcome["removed"], json!([aside]));
    let w = r.fake.w();
    assert!(w.log.contains(&format!("promote {DISK0}")));
    assert!(!w.datasets.contains_key(aside));
    assert!(
        w.datasets[DISK0].origin.is_none(),
        "the restored disk owns the PIT"
    );
    assert!(w.snapshots.contains_key(&format!("{DISK0}@{PIT_NAME}")));
    assert!(
        w.datasets.contains_key(staged_from),
        "a restore in progress keeps its origin"
    );
}

#[tokio::test]
async fn cleanup_offers_nothing_a_restore_in_progress_depends_on() {
    let r = rig();
    let aside = "rpool/bmac-replaced/100/d1-1767300000";
    {
        let mut w = r.fake.w();
        w.datasets.insert(aside.into(), Dataset::default());
        w.snapshots
            .insert(format!("{aside}@{PIT_NAME}"), SnapshotGuid::new(300));
        w.datasets.insert(
            "rpool/bmac-restore/job/d1".into(),
            Dataset {
                origin: Some(format!("{aside}@{PIT_NAME}")),
                ..Dataset::default()
            },
        );
    }
    let Err(Stop::Failed { message, .. }) = clean(&r, vec![]).await else {
        panic!("refused");
    };
    assert!(message.contains("restore in progress"), "{message}");
    assert!(r.fake.w().datasets.contains_key(aside));
}

#[test]
fn the_copy_script_parses() {
    let checked = std::process::Command::new("bash")
        .args(["-n", "-c", COPY_CLONE])
        .output()
        .unwrap();
    assert!(
        checked.status.success(),
        "{}",
        String::from_utf8_lossy(&checked.stderr)
    );
}
