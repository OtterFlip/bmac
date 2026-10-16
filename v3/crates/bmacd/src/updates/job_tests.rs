// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

use std::collections::BTreeMap;
use std::sync::Mutex;

use bmac_proto::bmac_ui::WorkflowEvent;
use bmac_proto::updates::{Installed, InstalledBundle};

use super::*;
use crate::backup::restore::tests::{drive, ok, yes};
use crate::native::tests::{Shared, workflow};
use crate::updates::tests::{Rig, host_bundle, rig};

const NOW_MS: i64 = 1_767_300_000_000;

fn slot(name: &str) -> HostSlot {
    name.parse().unwrap()
}

fn job_id(n: u8) -> JobId {
    format!("{n:08x}-0000-4000-8000-000000000000")
        .parse()
        .unwrap()
}

#[derive(Debug, Clone)]
struct Sim {
    version: String,
    installed: Installed,
    installs: BTreeMap<(JobId, &'static str), InstallStatus>,
    online: bool,
    ha: Vec<String>,
}

#[derive(Default)]
struct World {
    hosts: BTreeMap<HostSlot, Sim>,
    /// Hosts whose `bmac-source` won't run on this version.
    broken: Vec<(HostSlot, String)>,
    /// Hosts whose package install fails.
    apt_fails: Vec<HostSlot>,
    calls: Vec<String>,
    schema: u64,
    url_bundle: Option<Vec<u8>>,
}

struct Fake {
    me: HostSlot,
    w: Mutex<World>,
    /// Bundle versions by SHA-256, as the hosts would read them.
    versions: BTreeMap<String, String>,
}

fn mode_key(mode: InstallMode) -> &'static str {
    match mode {
        InstallMode::Update => "update",
        InstallMode::Rollback => "rollback",
    }
}

impl Fake {
    fn new(me: &str, rig: &Rig, versions: &[&BundleSummary], running: &BundleSummary) -> Fake {
        let installed = Installed {
            current: Some(InstalledBundle {
                sha256: running.sha256.clone(),
                version: running.version.clone(),
            }),
            previous: None,
        };
        let hosts = ["mox1", "mox2", "mox3"]
            .into_iter()
            .map(|h| {
                (
                    slot(h),
                    Sim {
                        version: running.version.clone(),
                        installed: installed.clone(),
                        installs: BTreeMap::new(),
                        online: true,
                        ha: Vec::new(),
                    },
                )
            })
            .collect();
        let _ = rig;
        Fake {
            me: slot(me),
            w: Mutex::new(World {
                hosts,
                schema: 1,
                ..Default::default()
            }),
            versions: versions
                .iter()
                .map(|s| (s.sha256.clone(), s.version.clone()))
                .collect(),
        }
    }

    fn w(&self) -> std::sync::MutexGuard<'_, World> {
        self.w.lock().unwrap()
    }

    fn version_of(&self, host: &str) -> String {
        self.w().hosts[&slot(host)].version.clone()
    }

    fn installs(&self) -> Vec<String> {
        self.w()
            .calls
            .iter()
            .filter(|c| c.starts_with("install"))
            .cloned()
            .collect()
    }
}

impl UpdateCluster for Fake {
    fn me(&self) -> HostSlot {
        self.me
    }

    fn membership(&self) -> Result<Membership, String> {
        let w = self.w();
        Ok(Membership {
            quorate: true,
            online: w
                .hosts
                .iter()
                .filter(|(_, s)| s.online)
                .map(|(h, _)| *h)
                .collect(),
            members: w.hosts.keys().map(|h| (*h, None)).collect(),
        })
    }

    fn state(
        &self,
        host: HostSlot,
        job: JobId,
        mode: InstallMode,
    ) -> BoxFuture<'_, Result<HostUpdateState, String>> {
        let w = self.w();
        let sim = w.hosts[&host].clone();
        let source_up = !w
            .broken
            .iter()
            .any(|(h, v)| *h == host && *v == sim.version);
        let state = HostUpdateState {
            host,
            version: sim.version.clone(),
            installed: sim.installed.clone(),
            install: sim.installs.get(&(job, mode_key(mode))).cloned(),
            services: [
                ("bmacd-web".to_string(), true),
                ("bmac-source".to_string(), source_up),
            ]
            .into(),
            reachable_peers: w.hosts.keys().filter(|h| **h != host).copied().collect(),
        };
        ok(Ok(state))
    }

    fn install<'a>(
        &'a self,
        host: HostSlot,
        request: &'a InstallRequest,
    ) -> BoxFuture<'a, Result<InstallStatus, String>> {
        let mut w = self.w();
        w.calls
            .push(format!("install {host} {}", mode_key(request.mode)));
        let fails = w.apt_fails.contains(&host);
        let sim = w.hosts.get_mut(&host).unwrap();
        let key = (request.job, mode_key(request.mode));
        if let Some(status) = sim.installs.get(&key) {
            return ok(Ok(status.clone()));
        }
        let status = |state| InstallStatus {
            mode: request.mode,
            state,
            message: None,
            log_tail: Some("E: broken".into()),
        };
        if fails {
            sim.installs.insert(key, status(InstallState::Failed));
            return ok(Ok(status(InstallState::Failed)));
        }
        match request.mode {
            InstallMode::Update => {
                let to = InstalledBundle {
                    sha256: request.sha256.clone(),
                    version: self.versions[&request.sha256].clone(),
                };
                sim.installed.previous = sim.installed.current.take();
                sim.version = to.version.clone();
                sim.installed.current = Some(to);
            }
            InstallMode::Rollback => {
                let previous = sim.installed.previous.take().unwrap();
                sim.installed.previous = sim.installed.current.take();
                sim.version = previous.version.clone();
                sim.installed.current = Some(previous);
            }
        }
        sim.installs.insert(key, status(InstallState::Installed));
        ok(Ok(status(InstallState::Installing)))
    }

    fn fetch_url<'a>(&'a self, url: &'a str, to: &'a Path) -> BoxFuture<'a, Result<(), String>> {
        let mut w = self.w();
        w.calls.push(format!("fetch {url}"));
        let bytes = w.url_bundle.clone().unwrap();
        std::fs::write(to, bytes).unwrap();
        ok(Ok(()))
    }

    fn record_schema(&self) -> Result<u64, String> {
        Ok(self.w().schema)
    }

    fn set_record_schema(&self, schema: u64) -> Result<(), String> {
        self.w().schema = schema;
        self.w().calls.push(format!("schema {schema}"));
        Ok(())
    }

    fn maintenance(&self, host: HostSlot, on: bool) -> BoxFuture<'_, Result<(), String>> {
        let mut w = self.w();
        w.calls.push(format!("maintenance {host} {on}"));
        if on {
            // HA moves the resources on the next look.
        }
        ok(Ok(()))
    }

    fn ha_resources_on(&self, host: HostSlot) -> BoxFuture<'_, Result<Vec<String>, String>> {
        let mut w = self.w();
        let sim = w.hosts.get_mut(&host).unwrap();
        let left = std::mem::take(&mut sim.ha);
        w.calls.push(format!("ha {host} {}", left.len()));
        ok(Ok(left))
    }
}

struct Run {
    outcome: Result<Value, Stop>,
    events: Vec<WorkflowEvent>,
}

impl Run {
    fn said(&self, text: &str) -> bool {
        self.events.iter().any(|e| match e {
            WorkflowEvent::Info { message } | WorkflowEvent::Warning { message } => {
                message.contains(text)
            }
            _ => false,
        })
    }

    fn error(&self) -> String {
        match &self.outcome {
            Err(Stop::Failed { message, .. }) => message.clone(),
            other => panic!("expected a failure, got {other:?}"),
        }
    }
}

async fn run(
    rig: &Rig,
    fake: &Fake,
    work: &Path,
    job: JobId,
    rollback_op: bool,
    answers: Vec<Value>,
) -> Run {
    let (flow, out, input) = workflow(work);
    let driver = tokio::spawn(drive(out.clone(), input, answers));
    let now = || NOW_MS;
    let cx = Context {
        flow: &flow,
        cluster: fake,
        store: &rig.store,
        job,
        now_ms: &now,
        poll: Duration::from_millis(1),
        install_within: Duration::from_millis(200),
        health_within: Duration::from_millis(50),
        ha_within: Duration::from_millis(200),
    };
    let outcome = if rollback_op {
        rollback(&cx).await
    } else {
        update(&cx).await
    };
    driver.abort();
    Run {
        outcome,
        events: Shared::events(&out),
    }
}

fn pick(sha: &str) -> Value {
    json!({"values": {"bundle": sha, "url": ""}})
}

fn options(migrate: bool) -> Value {
    json!({"values": {"migrate": migrate}})
}

fn no() -> Value {
    json!({"confirmed": false})
}

#[tokio::test]
async fn an_update_installs_on_each_host_in_turn_ending_with_its_own() {
    let rig = rig();
    let a = rig.stage("3.0.0+b1");
    let b = rig.stage("3.0.0+b2");
    let fake = Fake::new("mox2", &rig, &[&a, &b], &a);
    let work = rig.dir.path().join("work");
    std::fs::create_dir_all(&work).unwrap();
    let run = run(
        &rig,
        &fake,
        &work,
        job_id(1),
        false,
        vec![pick(&b.sha256), options(false), yes()],
    )
    .await;
    assert!(run.outcome.is_ok(), "{:?}", run.outcome);
    assert_eq!(
        fake.installs(),
        [
            "install mox1 update",
            "install mox3 update",
            "install mox2 update"
        ]
    );
    for host in ["mox1", "mox2", "mox3"] {
        assert_eq!(fake.version_of(host), "3.0.0+b2");
    }
    assert!(fake.w().calls.contains(&"schema 1".to_string()));
    assert!(!work.join(crate::native::SERVICE_RESTART_MARKER).exists());
    assert!(run.said("mox2 is updated and healthy"));
}

#[tokio::test]
async fn a_failure_on_the_second_host_stops_the_update_and_the_rollback_returns_both() {
    let rig = rig();
    let a = rig.stage("3.0.0+b1");
    let b = rig.stage("3.0.0+b2");
    let fake = Fake::new("mox3", &rig, &[&a, &b], &a);
    for host in fake.w().hosts.values_mut() {
        host.installed.previous = None;
    }
    fake.w().broken.push((slot("mox2"), b.version.clone()));
    let work = rig.dir.path().join("work");
    std::fs::create_dir_all(&work).unwrap();
    let run = run(
        &rig,
        &fake,
        &work,
        job_id(2),
        false,
        vec![pick(&b.sha256), options(false), yes(), yes()],
    )
    .await;
    let error = run.error();
    assert!(error.contains("stopped at mox2"), "{error}");
    assert!(error.contains("bmac-source isn't running"), "{error}");
    assert!(error.contains("Rolled back mox1, mox2"), "{error}");
    assert_eq!(
        fake.installs(),
        [
            "install mox1 update",
            "install mox2 update",
            "install mox1 rollback",
            "install mox2 rollback",
        ]
    );
    for host in ["mox1", "mox2", "mox3"] {
        assert_eq!(fake.version_of(host), "3.0.0+b1", "{host}");
    }
    assert_eq!(fake.w().schema, 1);
}

#[tokio::test]
async fn a_declined_rollback_leaves_the_hosts_for_the_rollback_job() {
    let rig = rig();
    let a = rig.stage("3.0.0+b1");
    let b = rig.stage("3.0.0+b2");
    let fake = Fake::new("mox3", &rig, &[&a, &b], &a);
    fake.w().broken.push((slot("mox2"), b.version.clone()));
    let work = rig.dir.path().join("update");
    std::fs::create_dir_all(&work).unwrap();
    let run1 = run(
        &rig,
        &fake,
        &work,
        job_id(3),
        false,
        vec![pick(&b.sha256), options(false), yes(), no()],
    )
    .await;
    assert!(run1.error().contains("Nothing was rolled back"));
    assert_eq!(fake.version_of("mox1"), "3.0.0+b2");
    assert_eq!(fake.version_of("mox2"), "3.0.0+b2");
    assert_eq!(fake.version_of("mox3"), "3.0.0+b1");

    fake.w().broken.clear();
    let work = rig.dir.path().join("rollback");
    std::fs::create_dir_all(&work).unwrap();
    let run2 = run(
        &rig,
        &fake,
        &work,
        job_id(4),
        true,
        vec![pick(&b.sha256), yes()],
    )
    .await;
    assert!(run2.outcome.is_ok(), "{:?}", run2.outcome);
    for host in ["mox1", "mox2", "mox3"] {
        assert_eq!(fake.version_of(host), "3.0.0+b1", "{host}");
    }
}

#[tokio::test]
async fn a_resumed_update_skips_the_hosts_it_finished_and_asks_nothing() {
    let rig = rig();
    let a = rig.stage("3.0.0+b1");
    let b = rig.stage("3.0.0+b2");
    let fake = Fake::new("mox1", &rig, &[&a, &b], &a);
    let work = rig.dir.path().join("work");
    std::fs::create_dir_all(&work).unwrap();
    let mut done = Checkpoints::load(work.join("checkpoints.json"));
    done.record("bundle", json!({"sha256": b.sha256})).unwrap();
    done.record(
        "plan",
        json!({"hosts": ["mox2", "mox3", "mox1"], "members": ["mox1", "mox2", "mox3"], "migrate": false}),
    )
    .unwrap();
    done.record("updated:mox2", json!({})).unwrap();
    let run = run(&rig, &fake, &work, job_id(5), false, vec![]).await;
    assert!(run.outcome.is_ok(), "{:?}", run.outcome);
    assert_eq!(
        fake.installs(),
        ["install mox3 update", "install mox1 update"]
    );
}

#[tokio::test]
async fn moving_ha_resources_waits_for_them_to_leave_and_ends_maintenance() {
    let rig = rig();
    let a = rig.stage("3.0.0+b1");
    let b = rig.stage("3.0.0+b2");
    let fake = Fake::new("mox1", &rig, &[&a, &b], &a);
    fake.w().hosts.get_mut(&slot("mox2")).unwrap().ha = vec!["vm:100".into()];
    let work = rig.dir.path().join("work");
    std::fs::create_dir_all(&work).unwrap();
    let run = run(
        &rig,
        &fake,
        &work,
        job_id(6),
        false,
        vec![pick(&b.sha256), options(true), yes()],
    )
    .await;
    assert!(run.outcome.is_ok(), "{:?}", run.outcome);
    let calls = fake.w().calls.clone();
    let mox2: Vec<&String> = calls.iter().filter(|c| c.contains("mox2")).collect();
    assert_eq!(
        mox2,
        [
            "maintenance mox2 true",
            "ha mox2 1",
            "ha mox2 0",
            "install mox2 update",
            "maintenance mox2 false",
        ]
    );
}

#[tokio::test]
async fn an_older_bundle_is_refused() {
    let rig = rig();
    let a = rig.stage("3.0.0+b1");
    let b = rig.stage("3.0.0+b2");
    let fake = Fake::new("mox1", &rig, &[&a, &b], &b);
    let work = rig.dir.path().join("work");
    std::fs::create_dir_all(&work).unwrap();
    let run = run(&rig, &fake, &work, job_id(7), false, vec![pick(&a.sha256)]).await;
    assert!(
        run.error().contains("older than what runs now"),
        "{}",
        run.error()
    );
    assert!(fake.installs().is_empty());
}

#[tokio::test]
async fn every_host_must_be_online() {
    let rig = rig();
    let a = rig.stage("3.0.0+b1");
    let b = rig.stage("3.0.0+b2");
    let fake = Fake::new("mox1", &rig, &[&a, &b], &a);
    fake.w().hosts.get_mut(&slot("mox3")).unwrap().online = false;
    let work = rig.dir.path().join("work");
    std::fs::create_dir_all(&work).unwrap();
    let run = run(&rig, &fake, &work, job_id(8), false, vec![pick(&b.sha256)]).await;
    assert!(run.error().contains("mox3 isn't"), "{}", run.error());
}

#[tokio::test]
async fn a_failed_package_install_stops_before_the_next_host() {
    let rig = rig();
    let a = rig.stage("3.0.0+b1");
    let b = rig.stage("3.0.0+b2");
    let fake = Fake::new("mox3", &rig, &[&a, &b], &a);
    fake.w().apt_fails.push(slot("mox1"));
    let work = rig.dir.path().join("work");
    std::fs::create_dir_all(&work).unwrap();
    let run = run(
        &rig,
        &fake,
        &work,
        job_id(9),
        false,
        vec![pick(&b.sha256), options(false), yes()],
    )
    .await;
    let error = run.error();
    assert!(
        error.contains("stopped at mox1") && error.contains("E: broken"),
        "{error}"
    );
    assert!(error.contains("No host runs the new bundle"), "{error}");
    assert_eq!(fake.installs(), ["install mox1 update"]);
}

#[tokio::test]
async fn a_bundle_can_come_from_a_url() {
    let rig = rig();
    let a = rig.stage("3.0.0+b1");
    let bytes = host_bundle(&rig.key, "3.0.0+b2");
    let sha: String = {
        use sha2::{Digest, Sha256};
        Sha256::digest(&bytes)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect()
    };
    let b = BundleSummary {
        sha256: sha.clone(),
        version: "3.0.0+b2".into(),
        ..a.clone()
    };
    let fake = Fake::new("mox1", &rig, &[&a, &b], &a);
    fake.w().url_bundle = Some(bytes);
    let work = rig.dir.path().join("work");
    std::fs::create_dir_all(&work).unwrap();
    let run = run(
        &rig,
        &fake,
        &work,
        job_id(10),
        false,
        vec![
            json!({"values": {"bundle": "url", "url": "https://example.com/b.bundle"}}),
            options(false),
            yes(),
        ],
    )
    .await;
    assert!(run.outcome.is_ok(), "{:?}", run.outcome);
    assert!(
        fake.w()
            .calls
            .contains(&"fetch https://example.com/b.bundle".to_string())
    );
    assert!(rig.store.has(&sha));
}

#[test]
fn the_schema_check_refuses_old_bundles_and_bundles_that_cant_read_the_data() {
    let rig = rig();
    let mut summary = rig.stage("3.0.0+b1");
    summary.record_schema = 3;
    summary.reads_record_schema_from = 2;
    assert!(schema_refusal(&summary, 2).is_none());
    assert!(schema_refusal(&summary, 3).is_none());
    assert!(schema_refusal(&summary, 4).unwrap().contains("newer"));
    assert!(
        schema_refusal(&summary, 1)
            .unwrap()
            .contains("intermediate")
    );
}

#[test]
fn ha_resources_are_read_from_the_ha_status() {
    let status = json!([
        {"type": "quorum", "id": "quorum"},
        {"type": "service", "sid": "vm:100", "node": "mox2", "state": "started"},
        {"type": "service", "sid": "vm:101", "node": "mox2", "state": "stopped"},
        {"type": "service", "sid": "vm:102", "node": "mox1", "state": "started"},
    ]);
    assert_eq!(ha_resources_on(&status, slot("mox2")), ["vm:100"]);
}

#[test]
fn hosts_update_in_slot_order_with_this_host_last() {
    assert_eq!(
        order([slot("mox3"), slot("mox1"), slot("mox2")], slot("mox1")),
        [slot("mox2"), slot("mox3"), slot("mox1")]
    );
    assert!(older("3.0.0~dev.0+b1", "3.0.0~dev.0+b2"));
    assert!(older("3.0.0-dev.0+b2", "3.0.0"));
    assert!(!older("3.0.1", "3.0.0+b9"));
}
