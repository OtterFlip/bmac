// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

use std::sync::Mutex;

use bmac_backup::records::Policy;
use bmac_core::GuestName;
use bmac_proto::appliance::{RECOVERY_SCHEMA, RecoveryManifest};
use bmac_proto::bmac_ui::WorkflowEvent;

use super::*;
use crate::backup::capture::Production;
use crate::backup::restore::tests::{CLUSTER, Rig, drive, id, index, ok, rig, yes};
use crate::native::tests::workflow;

const NOW_MS: i64 = 1_767_300_000_000;
const GRANT: &str = "bmac-grant-good";

fn pin() -> String {
    "ab".repeat(32)
}

fn me() -> HostSlot {
    "mox1".parse().unwrap()
}

/// The lost cluster's manifest: what a recovery writes back, and what it
/// leaves behind.
fn lost_manifest() -> RecoveryManifest {
    let files: BTreeMap<String, String> = [
        ("registry/policy.json", "{\"a\":1}"),
        ("registry/resources/web.json", "{\"name\":\"web\"}"),
        ("config/cluster.json", "{\"PROXMOX_CLUSTER_NAME\":\"lost\"}"),
        ("config/hosts/mox1.json", "{\"old\":true}"),
        ("backup/policies/x.json", "{\"vm\":\"web\"}"),
        ("backup/pits/x/y.json", "{}"),
        ("schedules/s.json", "{}"),
    ]
    .into_iter()
    .map(|(p, t)| (p.to_string(), t.to_string()))
    .collect();
    RecoveryManifest {
        schema_version: RECOVERY_SCHEMA,
        cluster_uuid: CLUSTER.into(),
        generated_at_ms: NOW_MS - 3_600_000,
        host: "mox2".parse().unwrap(),
        revision: RecoveryManifest::revision_of(&files),
        files,
    }
}

/// The appliance's answer: PIT 3 of VM 7 and its two ancestors.
fn recovery_index() -> RecoveryIndex {
    let granted = index();
    let newest = granted.pits[0].clone();
    let mut pits: Vec<PeerPit> = granted
        .bases
        .iter()
        .map(|b| PeerPit {
            cluster_uuid: CLUSTER.into(),
            vm_uuid: b.vm_uuid,
            branch_uuid: b.branch_uuid,
            pit_uuid: b.pit_uuid,
            parent: b.parent,
            name: format!("older-{}", b.taken_s),
            taken_s: b.taken_s,
            volumes: b.volumes.clone(),
            manifest: String::new(),
        })
        .collect();
    pits.push(newest);
    RecoveryIndex {
        appliance_uuid: id(0xa),
        cluster_uuid: CLUSTER.into(),
        cluster_name: "lost".into(),
        grant_id: "r1".into(),
        expires_at_ms: NOW_MS + 3_600_000,
        manifest: Some(lost_manifest()),
        pits,
    }
}

#[derive(Default)]
struct Lost {
    cluster_uuid: Mutex<Option<String>>,
    written: Mutex<BTreeMap<String, String>>,
    bound: Mutex<Option<RecoveryBind>>,
    ha: Mutex<Vec<u32>>,
    taken: BTreeSet<u32>,
    no_manifest: bool,
}

impl RecoverHost for Lost {
    fn host_fingerprint(&self) -> Result<String, String> {
        Ok("cd".repeat(32))
    }

    fn ca_fingerprint(&self) -> Result<String, String> {
        Ok("ef".repeat(32))
    }

    fn cluster_uuid(&self) -> Result<Option<String>, String> {
        Ok(self.cluster_uuid.lock().unwrap().clone())
    }

    fn source_hosts(&self) -> Result<Vec<SourceHost>, String> {
        Ok(vec![SourceHost {
            host: me(),
            addresses: vec!["10.0.0.11".into()],
        }])
    }

    fn recovery_index<'a>(
        &'a self,
        appliance: &'a ApplianceRecord,
        grant: &'a str,
    ) -> BoxFuture<'a, Result<RecoveryIndex, String>> {
        if appliance.fingerprint != pin() {
            return ok(Err(
                "the appliance's certificate isn't the one pinned".into()
            ));
        }
        if grant != GRANT {
            return ok(Err("vault refused: that isn't a recovery grant".into()));
        }
        let mut index = recovery_index();
        if self.no_manifest {
            index.manifest = None;
        }
        ok(Ok(index))
    }

    fn bind<'a>(
        &'a self,
        appliance: &'a ApplianceRecord,
        bind: &'a RecoveryBind,
    ) -> BoxFuture<'a, Result<(), String>> {
        assert_eq!(appliance.appliance_uuid, id(0xa));
        *self.bound.lock().unwrap() = Some(bind.clone());
        ok(Ok(()))
    }

    fn exists(&self, path: &str) -> bool {
        path == "config/cluster.json"
    }

    fn write(&self, path: &str, text: &str) -> Result<(), String> {
        assert!(written_back(path), "{path}");
        self.written
            .lock()
            .unwrap()
            .insert(path.to_string(), text.to_string());
        Ok(())
    }

    fn take_cluster_uuid(&self, uuid: &str) -> Result<(), String> {
        *self.cluster_uuid.lock().unwrap() = Some(uuid.to_string());
        Ok(())
    }

    fn vmid_taken(&self, vmid: u32) -> BoxFuture<'_, Result<bool, String>> {
        ok(Ok(self.taken.contains(&vmid)))
    }

    fn ha_add(&self, vmid: u32) -> BoxFuture<'_, Result<(), String>> {
        self.ha.lock().unwrap().push(vmid);
        ok(Ok(()))
    }
}

/// The registry written back: VM 7 on mox1, mox2, and mox3.
struct Registry;

impl Guests for Registry {
    fn production(&self, _name: &GuestName) -> Result<Production, String> {
        unreachable!()
    }

    fn by_uuid(&self, vm: &Uuid) -> Result<Option<Production>, String> {
        Ok((*vm == id(7)).then(|| Production {
            vm_uuid: id(7),
            name: "prod1".parse().unwrap(),
            vmid: 100,
            placement: vec![me(), "mox2".parse().unwrap(), "mox3".parse().unwrap()],
            record: Value::Null,
        }))
    }

    fn leased(&self, _name: &GuestName) -> Result<bool, String> {
        unreachable!()
    }

    fn acquire(&self, _name: &GuestName, _ttl_s: u32) -> BoxFuture<'_, Result<String, String>> {
        unreachable!()
    }

    fn release(&self, _name: &GuestName, _nonce: &str) -> BoxFuture<'_, Result<(), String>> {
        unreachable!()
    }
}

struct Ran {
    outcome: Result<Value, Stop>,
    events: Vec<WorkflowEvent>,
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

    fn confirm(&self) -> Option<(Option<String>, Vec<String>)> {
        self.events
            .iter()
            .find_map(|e| match crate::native::request_form(e) {
                Some((
                    _,
                    bmac_core::job::RequestForm::Confirm {
                        confirmation_text,
                        details,
                        ..
                    },
                )) => Some((confirmation_text, details)),
                _ => None,
            })
    }
}

async fn run(r: &Rig, lost: &Lost, answers: Vec<Value>) -> Ran {
    let work = tempfile::tempdir().unwrap();
    let (flow, out, input) = workflow(work.path());
    let now = || NOW_MS;
    let cx = Context {
        restore: job::Context {
            flow: &flow,
            host: &r.fake,
            transfers: &r.fake,
            store: &r.store,
            cluster_uuid: String::new(),
            now_ms: &now,
            agent_wait: Duration::from_millis(50),
            poll: Duration::from_millis(1),
        },
        replace: &r.fake,
        host: lost,
        guests: &Registry,
    };
    let outcome = tokio::select! {
        outcome = recover(&cx) => outcome,
        () = drive(out.clone(), input, answers) => unreachable!(),
    };
    Ran {
        outcome,
        events: out.events(),
    }
}

fn appliance(grant: &str) -> Value {
    json!({"values": {"address": "vault.example.net", "fingerprint": pin(), "grant": grant}})
}

fn everything() -> Value {
    json!({"values": {"vms": [id(7).to_string()], "configure": true}})
}

fn fresh() -> Lost {
    Lost {
        cluster_uuid: Mutex::new(Some("new-cluster".into())),
        ..Lost::default()
    }
}

#[test]
fn only_cluster_wide_state_is_written_back() {
    for path in [
        "registry/policy.json",
        "registry/resources/prod1.json",
        "registry/history/prod1.json",
        "registry/ingress/desired.json",
        "registry/ingress/generations/7.json",
        "config/cluster.json",
        "schedules/nightly.json",
        "backup/settings.json",
        "backup/policies/prod1.json",
        "backup/appliances/a.json",
    ] {
        assert!(written_back(path), "{path}");
    }
    for path in [
        "config/hosts/mox1.json",
        "registry/leases/prod1.json",
        "registry/ingress/transition.json",
        "backup/pits/x/y.json",
        "backup/lineages/x.json",
        "backup/restores/x.json",
        "registry/resources/../../secrets/x",
        "secrets/postmark.token",
        "registry/resources/a/b.json",
    ] {
        assert!(!written_back(path), "{path}");
    }
}

#[test]
fn each_vm_offers_its_newest_pit() {
    let found = candidates(&recovery_index()).unwrap();
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].pit.pit_uuid, id(3));
    assert_eq!(found[0].name(), "web");
    let granted = granted_for(&recovery_index(), &found[0].pit);
    let bases: Vec<Uuid> = granted.bases.iter().map(|b| b.pit_uuid).collect();
    assert_eq!(bases, vec![id(1), id(2)]);
}

#[tokio::test]
async fn a_recovery_takes_over_the_lost_cluster_and_brings_its_vms_back() {
    let r = rig();
    let lost = fresh();
    let ran = run(&r, &lost, vec![appliance(GRANT), everything(), yes()]).await;
    let result = ran.outcome.as_ref().unwrap();
    assert_eq!(result["configured"], json!(true));
    assert_eq!(result["restored"][0]["vmid"], json!(100));
    assert_eq!(result["restored"][0]["replicas"], json!(["mox2", "mox3"]));

    assert!(ran.said(&display_fingerprint(&"cd".repeat(32))));
    assert!(ran.said("If your browser showed another one"));
    let (typed, details) = ran.confirm().unwrap();
    assert_eq!(typed.as_deref(), Some("lost"));
    assert!(
        details
            .iter()
            .any(|d| d.contains("1 replace this cluster's own"))
    );
    assert!(details.iter().any(|d| d.contains("Postmark")));

    assert_eq!(lost.cluster_uuid.lock().unwrap().as_deref(), Some(CLUSTER));
    let written: Vec<String> = lost.written.lock().unwrap().keys().cloned().collect();
    assert_eq!(
        written,
        [
            "backup/policies/x.json",
            "config/cluster.json",
            "registry/policy.json",
            "registry/resources/web.json",
            "schedules/s.json",
        ]
    );
    let bound = lost.bound.lock().unwrap().clone().unwrap();
    assert_eq!(bound.cluster_uuid, CLUSTER);
    assert_eq!(bound.grant, GRANT);
    assert_eq!(bound.proxmox_ca_fingerprint, "ef".repeat(32));

    let w = r.fake.w();
    let vm = w.vms.get(&100).expect("VM 100 is back under its own VMID");
    assert!(vm.iter().any(|(k, v)| k == "name" && v == "web"));
    assert!(w.started.is_empty(), "HA starts it");
    let jobs: Vec<(String, String)> = w
        .jobs
        .iter()
        .map(|(_, j, _)| (j.id.clone(), j.target.to_string()))
        .collect();
    assert_eq!(
        jobs,
        [
            ("100-0".to_string(), "mox2".to_string()),
            ("100-1".to_string(), "mox3".to_string())
        ]
    );
    drop(w);
    assert_eq!(*lost.ha.lock().unwrap(), vec![100]);

    let records = r.store.restores().unwrap();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].mode, Mode::Recovery);
    assert!(records[0].finished_at_ms.is_some() && records[0].error.is_none());
    let lineage = r.store.lineage(&id(7)).unwrap().unwrap();
    assert_ne!(lineage.branch_uuid, id(0xb), "a new branch");
}

#[tokio::test]
async fn a_wrong_pin_or_grant_is_asked_again() {
    let r = rig();
    let lost = fresh();
    let wrong_pin = json!({"values": {"address": "vault.example.net", "fingerprint": "12".repeat(32), "grant": GRANT}});
    let ran = run(
        &r,
        &lost,
        vec![
            wrong_pin,
            appliance("bmac-grant-bad"),
            appliance(GRANT),
            everything(),
            yes(),
        ],
    )
    .await;
    assert!(ran.outcome.is_ok(), "{:?}", ran.outcome);
    assert!(ran.said("isn't the one pinned"));
    assert!(ran.said("isn't a recovery grant"));
}

#[tokio::test]
async fn a_cluster_with_backup_policies_is_not_recovered_over() {
    let r = rig();
    r.store
        .locked(|l| {
            l.save_policy(Policy {
                schema_version: RECORD_SCHEMA,
                record_type: String::new(),
                revision: 0,
                vm_uuid: id(9),
                vm: "prod2".into(),
                schedule_id: Policy::schedule_id_for(&id(9)),
                appliances: Vec::new(),
                local_retention: Default::default(),
                started_s: None,
                freeze: Default::default(),
                accept_multi_pool: false,
                enabled: true,
                updated_by: "t".into(),
                updated_at_ms: 0,
            })
        })
        .unwrap();
    let lost = fresh();
    let ran = run(&r, &lost, vec![]).await;
    let Err(Stop::Failed { message, .. }) = ran.outcome else {
        panic!("{:?}", ran.outcome)
    };
    assert!(message.contains("already has backup policies"));
    assert!(lost.bound.lock().unwrap().is_none());
}

#[tokio::test]
async fn a_vmid_in_use_stops_the_recovery_before_anything_changes() {
    let r = rig();
    let lost = Lost {
        taken: [100].into(),
        ..fresh()
    };
    let ran = run(&r, &lost, vec![appliance(GRANT), everything()]).await;
    let Err(Stop::Failed { message, .. }) = ran.outcome else {
        panic!("{:?}", ran.outcome)
    };
    assert!(message.contains("VM 100 already exists here"));
    assert_eq!(
        lost.cluster_uuid.lock().unwrap().as_deref(),
        Some("new-cluster")
    );
    assert!(lost.written.lock().unwrap().is_empty());
}

#[tokio::test]
async fn without_a_manifest_only_the_vms_come_back() {
    let r = rig();
    let lost = Lost {
        no_manifest: true,
        ..fresh()
    };
    let only_vms = json!({"values": {"vms": [id(7).to_string()], "configure": false}});
    let ran = run(&r, &lost, vec![appliance(GRANT), only_vms, yes()]).await;
    let result = ran.outcome.as_ref().unwrap();
    assert_eq!(result["configured"], json!(false));
    assert!(lost.bound.lock().unwrap().is_none());
    assert_eq!(
        lost.cluster_uuid.lock().unwrap().as_deref(),
        Some("new-cluster")
    );
    assert!(r.fake.w().vms.contains_key(&100));
    assert!(r.store.lineage(&id(7)).unwrap().is_none(), "a new history");
}
