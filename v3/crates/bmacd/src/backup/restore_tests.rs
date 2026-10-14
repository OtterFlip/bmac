// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

use std::sync::{Arc, Mutex};

use bmac_backup::manifest::{Consistency, Firmware, SCHEMA_VERSION, Versions, Volume, VolumeRole};
use bmac_backup::records::{ApplianceRecord, Enrollment};
use bmac_core::ApplianceAddress;
use bmac_pmxcfs::{Pmxcfs, RealFs};
use bmac_proto::appliance::{GrantedBase, PeerVolume};
use bmac_proto::bmac_ui::WorkflowEvent;
use tokio::io::DuplexStream;

use super::*;
use crate::native::tests::{Shared, send, workflow};

const CLUSTER: &str = "c1";
const PIT_NAME: &str = "bmac-pit-20260101T000000Z-000001";

fn id(n: u128) -> Uuid {
    Uuid::from_u128(n)
}

#[derive(Debug, Default, Clone)]
struct Dataset {
    volsize: u64,
    origin: Option<String>,
    resume_token: Option<String>,
}

#[derive(Default)]
struct World {
    datasets: BTreeMap<String, Dataset>,
    snapshots: BTreeMap<String, SnapshotGuid>,
    vms: BTreeMap<u32, Vec<(String, String)>>,
    started: Vec<u32>,
    log: Vec<String>,
    /// Receives that fail once, part way, leaving a resume token.
    break_once: BTreeSet<String>,
    received: Vec<ExportRequest>,
    fail_create: bool,
}

#[derive(Clone, Default)]
struct Fake(Arc<Mutex<World>>);

impl Fake {
    fn w(&self) -> std::sync::MutexGuard<'_, World> {
        self.0.lock().unwrap()
    }
}

fn ok<T: Send + 'static>(value: T) -> BoxFuture<'static, T> {
    Box::pin(async move { value })
}

impl RestoreHost for Fake {
    fn this_host(&self) -> HostSlot {
        "mox1".parse().unwrap()
    }

    fn storages(&self) -> BoxFuture<'_, Result<Vec<Storage>, String>> {
        ok(Ok(parse_storages(STORAGE_CFG_TEXT, "mox1")))
    }

    fn snapshots(&self, pool: &str) -> BoxFuture<'_, Result<Vec<LocalSnapshot>, String>> {
        let prefix = format!("{pool}/");
        let found = self
            .w()
            .snapshots
            .iter()
            .filter(|(n, _)| n.starts_with(&prefix))
            .map(|(n, g)| LocalSnapshot {
                name: n.clone(),
                guid: *g,
            })
            .collect();
        ok(Ok(found))
    }

    fn available(&self, _dataset: &str) -> BoxFuture<'_, Result<u64, String>> {
        ok(Ok(100 << 30))
    }

    fn children(&self, parent: &str) -> BoxFuture<'_, Result<BTreeSet<String>, String>> {
        let prefix = format!("{parent}/");
        let names = self
            .w()
            .datasets
            .keys()
            .filter_map(|n| n.strip_prefix(&prefix))
            .filter(|n| !n.contains('/'))
            .map(str::to_string)
            .collect();
        ok(Ok(names))
    }

    fn bridges(&self) -> BoxFuture<'_, Result<BTreeSet<String>, String>> {
        ok(Ok(["vmbr0".to_string(), "vmbr1".to_string()].into()))
    }

    fn staging_parent(&self, pool: &str, job: &str) -> BoxFuture<'_, Result<(), String>> {
        self.w().log.push(format!("staging {pool}/bmac-restore/{job}"));
        ok(Ok(()))
    }

    fn clone_snapshot(&self, snapshot: &str, target: &str) -> BoxFuture<'_, Result<(), String>> {
        let mut w = self.w();
        w.log.push(format!("clone {snapshot} {target}"));
        let source = snapshot.split('@').next().unwrap().to_string();
        let volsize = w.datasets.get(&source).map(|d| d.volsize).unwrap_or(0);
        w.datasets.insert(
            target.to_string(),
            Dataset {
                volsize,
                origin: Some(snapshot.to_string()),
                resume_token: None,
            },
        );
        ok(Ok(()))
    }

    fn resume_token(&self, dataset: &str) -> BoxFuture<'_, Result<Option<String>, String>> {
        let token = self.w().datasets.get(dataset).and_then(|d| d.resume_token.clone());
        ok(Ok(token))
    }

    fn guid(&self, snapshot: &str) -> BoxFuture<'_, Result<Option<SnapshotGuid>, String>> {
        ok(Ok(self.w().snapshots.get(snapshot).copied()))
    }

    fn origin_guid(&self, dataset: &str) -> BoxFuture<'_, Result<Option<SnapshotGuid>, String>> {
        let w = self.w();
        let guid = w
            .datasets
            .get(dataset)
            .and_then(|d| d.origin.as_ref())
            .and_then(|o| w.snapshots.get(o))
            .copied();
        ok(Ok(guid))
    }

    fn volsize(&self, dataset: &str) -> BoxFuture<'_, Result<u64, String>> {
        let size = self.w().datasets.get(dataset).map(|d| d.volsize);
        ok(size.ok_or_else(|| format!("{dataset} doesn't exist")))
    }

    fn place(&self, from: &str, to: &str) -> BoxFuture<'_, Result<(), String>> {
        let mut w = self.w();
        w.log.push(format!("place {from} {to}"));
        let d = w.datasets.remove(from).expect("placed from an existing dataset");
        w.datasets.insert(to.to_string(), d);
        let moved: Vec<(String, SnapshotGuid)> = w
            .snapshots
            .iter()
            .filter(|(n, _)| n.starts_with(&format!("{from}@")))
            .map(|(n, g)| (n.clone(), *g))
            .collect();
        for (name, guid) in moved {
            w.snapshots.remove(&name);
            w.snapshots.insert(name.replacen(from, to, 1), guid);
        }
        ok(Ok(()))
    }

    fn destroy_tree(&self, dataset: &str) -> BoxFuture<'_, Result<(), String>> {
        let mut w = self.w();
        w.log.push(format!("destroy {dataset}"));
        let inside = |n: &String| n == dataset || n.starts_with(&format!("{dataset}/"));
        w.datasets.retain(|n, _| !inside(n));
        w.snapshots
            .retain(|n, _| !inside(&n.split('@').next().unwrap().to_string()));
        ok(Ok(()))
    }

    fn next_vmid(&self) -> BoxFuture<'_, Result<u32, String>> {
        ok(Ok(120))
    }

    fn create_vm(&self, vmid: u32, options: &[(String, String)]) -> BoxFuture<'_, Result<(), String>> {
        let mut w = self.w();
        if w.fail_create {
            return ok(Err("qm create: storage is locked".into()));
        }
        w.vms.insert(vmid, options.to_vec());
        ok(Ok(()))
    }

    fn set_vm(&self, vmid: u32, key: &str, value: &str) -> BoxFuture<'_, Result<(), String>> {
        let mut w = self.w();
        w.vms
            .get_mut(&vmid)
            .expect("set on a created VM")
            .push((key.to_string(), value.to_string()));
        ok(Ok(()))
    }

    fn start_vm(&self, vmid: u32) -> BoxFuture<'_, Result<(), String>> {
        self.w().started.push(vmid);
        ok(Ok(()))
    }

    fn agent_answers(&self, vmid: u32) -> BoxFuture<'_, bool> {
        let up = self.w().started.contains(&vmid);
        ok(up)
    }

    fn destroy_vm(&self, vmid: u32) -> BoxFuture<'_, Result<(), String>> {
        let mut w = self.w();
        w.log.push(format!("destroy vm {vmid}"));
        w.vms.remove(&vmid);
        ok(Ok(()))
    }
}

impl Transfers for Fake {
    fn granted(
        &self,
        _appliance: &ApplianceRecord,
        cluster_uuid: &str,
        grant: &str,
    ) -> BoxFuture<'_, Result<GrantedIndex, String>> {
        if grant != "bmac-grant-good" || cluster_uuid != CLUSTER {
            return ok(Err("vault refused: that isn't a restore grant of this appliance".into()));
        }
        ok(Ok(index()))
    }

    fn estimate(
        &self,
        _appliance: &ApplianceRecord,
        request: &ExportRequest,
    ) -> BoxFuture<'_, Result<u64, String>> {
        assert_eq!(request.grant.as_deref(), Some("bmac-grant-good"));
        ok(Ok(if request.from_guid.is_some() { 1 << 20 } else { 3 << 30 }))
    }

    fn receive(
        &self,
        _appliance: &ApplianceRecord,
        request: &ExportRequest,
        target: &str,
        origin: Option<&str>,
    ) -> BoxFuture<'_, Result<u64, String>> {
        let mut w = self.w();
        w.received.push(request.clone());
        let volume = manifest()
            .volumes
            .into_iter()
            .find(|v| v.volume_uuid == request.volume_uuid.to_string())
            .unwrap();
        if w.break_once.remove(target) {
            w.datasets.insert(
                target.to_string(),
                Dataset {
                    volsize: 0,
                    origin: None,
                    resume_token: Some("1-token".into()),
                },
            );
            return ok(Err("the stream broke".into()));
        }
        w.datasets.insert(
            target.to_string(),
            Dataset {
                volsize: volume.volsize,
                origin: origin.map(str::to_string),
                resume_token: None,
            },
        );
        w.snapshots
            .insert(format!("{target}@{PIT_NAME}"), volume.snapshot_guid);
        ok(Ok(volume.volsize / 2))
    }
}

const STORAGE_CFG_TEXT: &str = "\
dir: local
\tpath /var/lib/vz
\tcontent iso

zfspool: local-zfs
\tpool rpool/data
\tsparse 1
\tcontent images,rootdir

zfspool: bmac-zfs-64k
\tpool rpool/data-64k
\tblocksize 64k
\tcontent images

zfspool: elsewhere
\tpool tank/data
\tnodes mox2
\tcontent images
";

fn volume(slot: &str, n: u128, block: u64, guid: u64) -> Volume {
    Volume {
        volume_uuid: id(n).to_string(),
        slot: slot.into(),
        role: if slot == "efidisk0" {
            VolumeRole::EfiVars
        } else {
            VolumeRole::Disk
        },
        dataset: format!("rpool/data/vm-100-disk-{n}"),
        snapshot_guid: SnapshotGuid::new(guid),
        volsize: 8 << 30,
        volblocksize: block,
        boot: true,
    }
}

fn manifest() -> Manifest {
    let config_text = "name: web\nscsi0: local-zfs:vm-100-disk-0,size=8G\nscsi1: bmac-zfs-64k:vm-100-disk-1,size=8G\nnet0: virtio=AA:BB:CC:DD:EE:FF,bridge=vmbr1\nmemory: 2048\n";
    let (_, config) = bmac_backup::manifest::current_config(config_text);
    let mut m = Manifest {
        schema_version: SCHEMA_VERSION,
        cluster_uuid: CLUSTER.into(),
        vm_uuid: id(7).to_string(),
        vmid: 100,
        pit_uuid: id(3).to_string(),
        branch_uuid: id(0xb).to_string(),
        parent_pit_uuid: Some(id(2).to_string()),
        snapshot: PIT_NAME.parse().unwrap(),
        taken_at_s: 1_767_225_600,
        consistency: Consistency::Filesystem,
        evidence: Value::Null,
        multi_pool: false,
        versions: Versions {
            proxmox: "pve".into(),
            openzfs: "zfs".into(),
            bmac: "3".into(),
        },
        config_text: config_text.into(),
        config,
        config_sha256: bmac_backup::manifest::sha256_hex(config_text.as_bytes()),
        volumes: vec![volume("scsi0", 0xd0, 16384, 300), volume("scsi1", 0xd1, 65536, 310)],
        firmware: Firmware {
            bios: "seabios".into(),
            machine: None,
            secure_boot: false,
        },
        registry: Value::Null,
        prerequisites: Vec::new(),
        sha256: String::new(),
    };
    m.seal();
    m
}

fn peer_volumes(guids: [u64; 2]) -> Vec<PeerVolume> {
    [(0xd0, guids[0], 16384), (0xd1, guids[1], 65536)]
        .into_iter()
        .map(|(n, g, b)| PeerVolume {
            volume_uuid: id(n),
            guid: SnapshotGuid::new(g),
            volsize: 8 << 30,
            volblocksize: b,
        })
        .collect()
}

fn index() -> GrantedIndex {
    let mut m = manifest();
    let sealed = m.seal();
    GrantedIndex {
        appliance_uuid: id(0xa),
        grant_id: "g1".into(),
        expires_at_ms: 1_767_312_000_000,
        pits: vec![PeerPit {
            cluster_uuid: CLUSTER.into(),
            vm_uuid: id(7),
            branch_uuid: id(0xb),
            pit_uuid: id(3),
            parent: Some(id(2)),
            name: PIT_NAME.into(),
            taken_s: 1_767_225_600,
            volumes: peer_volumes([300, 310]),
            manifest: sealed,
        }],
        bases: vec![
            GrantedBase {
                vm_uuid: id(7),
                branch_uuid: id(0xb),
                pit_uuid: id(1),
                parent: None,
                taken_s: 1_767_139_200,
                volumes: peer_volumes([100, 110]),
            },
            GrantedBase {
                vm_uuid: id(7),
                branch_uuid: id(0xb),
                pit_uuid: id(2),
                parent: Some(id(1)),
                taken_s: 1_767_182_400,
                volumes: peer_volumes([200, 210]),
            },
        ],
    }
}

struct Rig {
    _dir: tempfile::TempDir,
    store: BackupStore,
    fake: Fake,
}

fn rig() -> Rig {
    let dir = tempfile::tempdir().unwrap();
    let store = BackupStore::new(
        Pmxcfs::new(dir.path(), Arc::new(RealFs)),
        dir.path().join("priv/bmac/backup"),
    );
    store
        .locked(|l| {
            l.save_appliance(ApplianceRecord {
                schema_version: RECORD_SCHEMA,
                record_type: String::new(),
                revision: 0,
                appliance_uuid: id(0xa),
                name: "vault".into(),
                fingerprint: "ab".repeat(32),
                address: Some("vault.example.net".parse::<ApplianceAddress>().unwrap()),
                enrollment: Enrollment::Approved,
                enrolled_at_ms: 0,
                paused: false,
                retired: false,
                last_checkin_ms: None,
                version: None,
                locked: false,
                progress: Default::default(),
                heads: BTreeMap::new(),
                index_revision: 0,
                scrub: None,
            })
        })
        .unwrap();
    Rig {
        _dir: dir,
        store,
        fake: Fake::default(),
    }
}

async fn drive(out: Shared, mut input: DuplexStream, answers: Vec<Value>) {
    let mut answered = 0;
    loop {
        let events = out.events();
        let requests: Vec<&WorkflowEvent> = events.iter().filter(|e| e.is_request()).collect();
        while answered < requests.len() {
            let Some(answer) = answers.get(answered) else {
                panic!("no answer for {:?}", requests[answered]);
            };
            let mut answer = answer.clone();
            answer["type"] = json!("response");
            answer["request_id"] = json!(requests[answered].request_id().unwrap());
            send(&mut input, answer).await;
            answered += 1;
        }
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
}

struct Job {
    outcome: Result<Restored, Stop>,
    events: Vec<WorkflowEvent>,
}

impl Job {
    fn said(&self, text: &str) -> bool {
        self.events.iter().any(|e| match e {
            WorkflowEvent::Info { message } | WorkflowEvent::Warning { message } => {
                message.contains(text)
            }
            _ => false,
        })
    }

    fn confirm_details(&self) -> Vec<String> {
        self.events
            .iter()
            .find_map(|e| match request_form(e) {
                Some((_, bmac_core::job::RequestForm::Confirm { details, .. })) => Some(details),
                _ => None,
            })
            .unwrap_or_default()
    }
}

fn request_form(e: &WorkflowEvent) -> Option<(String, bmac_core::job::RequestForm)> {
    crate::native::request_form(e)
}

impl Rig {
    async fn run(&self, answers: Vec<Value>) -> Job {
        let work = tempfile::tempdir().unwrap();
        let (flow, out, input) = workflow(work.path());
        let now = || 1_767_300_000_000i64;
        let cx = Context {
            flow: &flow,
            host: &self.fake,
            transfers: &self.fake,
            store: &self.store,
            cluster_uuid: CLUSTER.into(),
            now_ms: &now,
            agent_wait: Duration::from_millis(50),
            poll: Duration::from_millis(5),
        };
        let outcome = tokio::select! {
            outcome = restore(&cx) => outcome,
            () = drive(out.clone(), input, answers) => unreachable!(),
        };
        Job {
            outcome,
            events: out.events(),
        }
    }
}

fn source(grant: &str) -> Value {
    json!({"values": {"appliance": id(0xa).to_string(), "grant": grant}})
}

fn what() -> Value {
    json!({"values": {"pit": id(3).to_string(), "name": "web-restored", "isolate": true, "start": true}})
}

fn yes() -> Value {
    json!({"confirmed": true})
}

#[tokio::test]
async fn a_full_restore_receives_checks_places_and_boots_an_isolated_vm() {
    let r = rig();
    let job = r.run(vec![source("bmac-grant-good"), what(), yes()]).await;
    let restored = job.outcome.as_ref().unwrap();
    assert_eq!(restored.vmid, 120);
    assert!(restored.agent);
    let details = job.confirm_details();
    assert!(details.iter().any(|d| d.starts_with("scsi0: method 3")), "{details:?}");
    assert!(details.iter().any(|d| d.contains("6.0 GiB to transfer in all")), "{details:?}");
    assert!(details.iter().any(|d| d.contains("may take a long time")));

    let w = r.fake.w();
    let vm = &w.vms[&120];
    assert!(vm.contains(&("net0".into(), "virtio,bridge=vmbr1,link_down=1".into())));
    assert!(vm.contains(&("scsi0".into(), "local-zfs:vm-120-disk-0,size=8G".into())));
    assert!(vm.contains(&("scsi1".into(), "bmac-zfs-64k:vm-120-disk-0,size=8G".into())));
    assert!(w.datasets.contains_key("rpool/data/vm-120-disk-0"));
    assert!(w.datasets.contains_key("rpool/data-64k/vm-120-disk-0"));
    assert_eq!(
        w.snapshots.get(&format!("rpool/data/vm-120-disk-0@{PIT_NAME}")),
        Some(&SnapshotGuid::new(300))
    );
    assert!(w.datasets.keys().all(|d| !d.contains("bmac-restore")), "staging is gone");
    assert!(w.received.iter().all(|r| r.from_guid.is_none()));
    drop(w);

    let records = r.store.restores().unwrap();
    assert_eq!(records.len(), 1);
    assert!(records[0].stands());
    assert_eq!(records[0].vmid, Some(120));
    assert_eq!(records[0].methods["scsi0"], 3);
}

#[tokio::test]
async fn an_older_pit_here_makes_an_incremental_and_the_pit_itself_a_clone() {
    let r = rig();
    {
        let mut w = r.fake.w();
        w.datasets.insert(
            "rpool/data/vm-100-disk-0".into(),
            Dataset {
                volsize: 8 << 30,
                ..Dataset::default()
            },
        );
        w.snapshots
            .insert("rpool/data/vm-100-disk-0@bmac-pit-p1".into(), SnapshotGuid::new(100));
        w.snapshots
            .insert("rpool/data/vm-100-disk-0@bmac-pit-p2".into(), SnapshotGuid::new(200));
        w.datasets.insert(
            "rpool/data-64k/vm-100-disk-1".into(),
            Dataset {
                volsize: 8 << 30,
                ..Dataset::default()
            },
        );
        w.snapshots.insert(
            format!("rpool/data-64k/vm-100-disk-1@{PIT_NAME}"),
            SnapshotGuid::new(310),
        );
    }
    let job = r.run(vec![source("bmac-grant-good"), what(), yes()]).await;
    job.outcome.as_ref().unwrap();
    let details = job.confirm_details();
    assert!(
        details.iter().any(|d| d.starts_with("scsi0: method 2") && d.contains("1.0 MiB")),
        "{details:?}"
    );
    assert!(details.iter().any(|d| d.starts_with("scsi1: method 1")), "{details:?}");
    let w = r.fake.w();
    assert_eq!(w.received.len(), 1);
    assert_eq!(w.received[0].from_guid, Some(SnapshotGuid::new(200)), "the nearest ancestor");
    assert!(w.log.iter().any(|l| l.starts_with(&format!(
        "clone rpool/data-64k/vm-100-disk-1@{PIT_NAME} rpool/bmac-restore/"
    ))));
    assert_eq!(
        w.datasets["rpool/data/vm-120-disk-0"].origin.as_deref(),
        Some("rpool/data/vm-100-disk-0@bmac-pit-p2")
    );
    drop(w);
    assert!(r.store.restores().unwrap()[0].holds_cluster_snapshot());
}

#[tokio::test]
async fn an_interrupted_stream_resumes_from_its_token() {
    let r = rig();
    let job_prefix = "rpool/bmac-restore/";
    // The target's job directory isn't known up front: break any receive
    // into scsi0's staging name by watching for it.
    let fake = r.fake.clone();
    let watcher = tokio::spawn(async move {
        loop {
            let staged = fake
                .w()
                .log
                .iter()
                .find_map(|l| l.strip_prefix("staging rpool/bmac-restore/").map(str::to_string));
            if let Some(job) = staged {
                let target = format!("{job_prefix}{job}/{}", id(0xd0));
                fake.w().break_once.insert(target);
                return;
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    });
    let job = r.run(vec![source("bmac-grant-good"), what(), yes()]).await;
    watcher.await.unwrap();
    job.outcome.as_ref().unwrap();
    assert!(job.said("resuming it"));
    let w = r.fake.w();
    let scsi0: Vec<&ExportRequest> = w
        .received
        .iter()
        .filter(|q| q.volume_uuid == id(0xd0))
        .collect();
    assert_eq!(scsi0.len(), 2);
    assert_eq!(scsi0[1].resume_token.as_deref(), Some("1-token"));
}

#[tokio::test]
async fn a_bad_grant_is_asked_again_and_a_failed_create_leaves_nothing_behind() {
    let r = rig();
    r.fake.w().fail_create = true;
    let job = r
        .run(vec![source("bmac-grant-bad"), source("bmac-grant-good"), what(), yes()])
        .await;
    assert!(job.said("isn't a restore grant"));
    let Err(Stop::Failed { message, .. }) = &job.outcome else {
        panic!("{:?}", job.outcome)
    };
    assert!(message.contains("storage is locked"), "{message}");
    let w = r.fake.w();
    assert!(w.vms.is_empty());
    assert!(w.datasets.is_empty(), "{:?}", w.datasets.keys());
    drop(w);
    let records = r.store.restores().unwrap();
    assert!(records[0].error.is_some() && !records[0].stands());
}

#[test]
fn storages_come_from_storage_cfg_for_this_node() {
    let found = parse_storages(STORAGE_CFG_TEXT, "mox1");
    assert_eq!(
        found,
        vec![
            Storage {
                id: "local-zfs".into(),
                dataset: "rpool/data".into(),
                block: 16384
            },
            Storage {
                id: "bmac-zfs-64k".into(),
                dataset: "rpool/data-64k".into(),
                block: 65536
            },
        ]
    );
    assert_eq!(parse_storages(STORAGE_CFG_TEXT, "mox2").len(), 3);
}

#[test]
fn the_bases_are_the_pits_ancestors_only() {
    let mut index = index();
    index.bases.push(GrantedBase {
        vm_uuid: id(7),
        branch_uuid: id(0xc),
        pit_uuid: id(9),
        parent: Some(id(1)),
        taken_s: 1_767_200_000,
        volumes: peer_volumes([900, 910]),
    });
    let pit = index.pits[0].clone();
    let bases: Vec<Uuid> = bases_for(&index, &pit, &id(0xd0)).iter().map(|b| b.pit_uuid).collect();
    assert_eq!(bases, vec![id(2), id(1)], "another branch's PIT isn't an ancestor");
}
