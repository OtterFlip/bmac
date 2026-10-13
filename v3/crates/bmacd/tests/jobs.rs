// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

//! Job admission, leases, conflicts, interruption, and cancellation, with
//! several simulated hosts sharing one temporary pmxcfs directory.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Barrier, Mutex};
use std::time::Duration;

use bmac_core::job::JobState;
use bmac_core::{GuestName, HostSlot};
use bmac_pmxcfs::{Pmxcfs, RealFs};
use bmac_proto::api::{ErrorCode, LaunchJob, Role};
use bmac_proto::operations::Operation;
use bmacd::cluster::{ClusterView, Membership};
use bmacd::jobs::{Caller, Jobs, JobsConfig, Progress};
use bmacd::operations::{Registry, SCRIPT_OPERATIONS};
use serde_json::{Value, json};

const ADMIN: Caller = Caller {
    user: "root@pam",
    role: Role::Admin,
};
const OPERATOR: Caller = Caller {
    user: "olive@pve",
    role: Role::Operator,
};
const VIEWER: Caller = Caller {
    user: "vera@pve",
    role: Role::Viewer,
};

fn host(n: u8) -> HostSlot {
    HostSlot::new(n).unwrap()
}

fn guest(name: &str) -> GuestName {
    name.parse().unwrap()
}

#[derive(Default)]
struct FakeCluster {
    membership: Mutex<Membership>,
    owners: Mutex<BTreeMap<GuestName, HostSlot>>,
    placement: Mutex<BTreeMap<GuestName, Vec<HostSlot>>>,
}

impl FakeCluster {
    fn set_online(&self, hosts: &[u8]) {
        self.membership.lock().unwrap().online = hosts.iter().map(|n| host(*n)).collect();
    }

    fn set_quorate(&self, quorate: bool) {
        self.membership.lock().unwrap().quorate = quorate;
    }
}

impl ClusterView for FakeCluster {
    fn membership(&self) -> Result<Membership, String> {
        Ok(self.membership.lock().unwrap().clone())
    }

    fn guest_owner(&self, guest: GuestName) -> Result<Option<HostSlot>, String> {
        Ok(self.owners.lock().unwrap().get(&guest).copied())
    }

    fn placement(&self, guest: GuestName) -> Result<Vec<HostSlot>, String> {
        Ok(self
            .placement
            .lock()
            .unwrap()
            .get(&guest)
            .cloned()
            .unwrap_or_default())
    }
}

fn resumable() -> Operation {
    serde_json::from_value(json!({
        "id": "test.resumable", "title": "Resumable", "summary": "s", "description": "d",
        "category": "production", "kind": "native",
        "min_role": "operator", "mutating": true, "destructive": false,
        "params": [{"id": "resource", "label": "VM", "type": "production", "required": true}],
        "locks": ["production:{resource}"],
        "target": {"rule": "owner", "param": "resource"},
        "cancel": {"rule": "phases"}, "resume": "checkpoint"
    }))
    .unwrap()
}

/// One pmxcfs directory, one cluster, one clock, and a `Jobs` per host.
struct Lab {
    _dir: tempfile::TempDir,
    pmxcfs: Pmxcfs,
    jobs_dir: std::path::PathBuf,
    cluster: Arc<FakeCluster>,
    clock: Arc<AtomicI64>,
    registry: Arc<Registry>,
    keep_finished: usize,
}

impl Lab {
    fn new() -> Lab {
        Lab::with_keep(500)
    }

    fn with_keep(keep_finished: usize) -> Lab {
        let dir = tempfile::tempdir().unwrap();
        let pve = dir.path().join("pve");
        let cluster = Arc::new(FakeCluster::default());
        cluster.set_quorate(true);
        cluster.set_online(&[1, 2, 3]);
        cluster
            .owners
            .lock()
            .unwrap()
            .insert(guest("prod1"), host(2));
        cluster
            .owners
            .lock()
            .unwrap()
            .insert(guest("prod2"), host(3));
        cluster
            .placement
            .lock()
            .unwrap()
            .insert(guest("prod1"), vec![host(1), host(2), host(3)]);
        Lab {
            pmxcfs: Pmxcfs::new(&pve, Arc::new(RealFs)),
            jobs_dir: pve.join("priv/bmac/jobs"),
            _dir: dir,
            cluster,
            clock: Arc::new(AtomicI64::new(1_000_000)),
            registry: Arc::new(Registry::from_parts(SCRIPT_OPERATIONS, vec![resumable()]).unwrap()),
            keep_finished,
        }
    }

    fn host(&self, n: u8) -> Jobs {
        let clock = self.clock.clone();
        let config = JobsConfig {
            pmxcfs: self.pmxcfs.clone(),
            dir: self.jobs_dir.clone(),
            host: host(n),
            lease_ttl_ms: 60_000,
            claim_timeout_ms: 300_000,
            keep_finished: self.keep_finished,
            lock_timeout: Duration::from_secs(30),
            clock: Arc::new(move || clock.load(Ordering::SeqCst)),
        };
        Jobs::new(config, self.registry.clone(), self.cluster.clone())
    }

    fn advance(&self, ms: i64) {
        self.clock.fetch_add(ms, Ordering::SeqCst);
    }
}

fn launch(operation: &str, parameters: Value, key: &str) -> LaunchJob {
    LaunchJob {
        operation: operation.parse().unwrap(),
        parameters: serde_json::from_value(parameters).unwrap(),
        idempotency_key: key.into(),
        dry_run: false,
    }
}

fn running() -> Progress {
    Progress {
        phase: None,
        cancel_allowed: true,
        pending_request: None,
        last_seq: 0,
    }
}

#[test]
fn concurrent_admissions_from_several_hosts_let_one_conflicting_job_in() {
    let lab = Lab::new();
    let hosts: Vec<Arc<Jobs>> = (1..=3).map(|n| Arc::new(lab.host(n))).collect();
    let barrier = Arc::new(Barrier::new(9));
    let threads: Vec<_> = (0..9)
        .map(|i| {
            let jobs = hosts[i % 3].clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                let resource = if i == 8 { "prod2" } else { "prod1" };
                barrier.wait();
                jobs.admit(
                    ADMIN,
                    &launch(
                        "prod.remove",
                        json!({"resource": resource}),
                        &format!("k{i}"),
                    ),
                )
                .map(|a| a.job)
            })
        })
        .collect();
    let results: Vec<_> = threads.into_iter().map(|t| t.join().unwrap()).collect();
    let admitted: Vec<_> = results.iter().filter_map(|r| r.as_ref().ok()).collect();
    let resources: BTreeSet<&str> = admitted
        .iter()
        .map(|j| j.parameters["resource"].as_str())
        .collect();
    assert_eq!(admitted.len(), 2, "{results:?}");
    assert_eq!(resources, BTreeSet::from(["prod1", "prod2"]));
    for refused in results.iter().filter_map(|r| r.as_ref().err()) {
        assert_eq!(refused.code, ErrorCode::Conflict);
        assert!(
            refused.message.contains("production:prod1 is in use"),
            "{}",
            refused.message
        );
    }
    let prod1 = admitted
        .iter()
        .find(|j| j.parameters["resource"] == "prod1")
        .unwrap();
    assert_eq!(prod1.target, host(2), "prod.remove runs on the owner");
    assert_eq!(prod1.locks, ["production:prod1"]);
    assert_eq!(hosts[0].list().unwrap().len(), 2);
}

#[test]
fn a_repeated_launch_returns_the_job_already_admitted() {
    let lab = Lab::new();
    let jobs = lab.host(1);
    let first = jobs
        .admit(
            OPERATOR,
            &launch("prod.extend_disk", json!({"resource": "prod1"}), "dialog-1"),
        )
        .unwrap();
    assert!(!first.existing);
    let again = lab
        .host(3)
        .admit(
            OPERATOR,
            &launch("prod.extend_disk", json!({"resource": "prod1"}), "dialog-1"),
        )
        .unwrap();
    assert!(again.existing);
    assert_eq!(again.job.id, first.job.id);

    let changed = jobs
        .admit(
            OPERATOR,
            &launch("prod.extend_disk", json!({"resource": "prod2"}), "dialog-1"),
        )
        .unwrap_err();
    assert_eq!(changed.code, ErrorCode::Conflict);
    assert!(changed.message.contains("idempotency key"));

    let other_user = Caller {
        user: "otto@pve",
        role: Role::Operator,
    };
    let refused = jobs
        .admit(
            other_user,
            &launch("prod.extend_disk", json!({"resource": "prod1"}), "dialog-1"),
        )
        .unwrap_err();
    assert_eq!(
        refused.code,
        ErrorCode::Conflict,
        "the same resource is busy"
    );

    for bad in ["", "has space", &"k".repeat(129)] {
        let error = jobs
            .admit(
                OPERATOR,
                &launch("prod.extend_disk", json!({"resource": "prod2"}), bad),
            )
            .unwrap_err();
        assert_eq!(error.code, ErrorCode::Invalid, "{bad:?}");
    }
}

#[test]
fn admission_checks_quorum_role_and_parameters() {
    let lab = Lab::new();
    let jobs = lab.host(1);
    let error = jobs
        .admit(VIEWER, &launch("prod.add", json!({}), "a"))
        .unwrap_err();
    assert_eq!(error.code, ErrorCode::Forbidden);
    let error = jobs
        .admit(
            OPERATOR,
            &launch("prod.remove", json!({"resource": "prod1"}), "b"),
        )
        .unwrap_err();
    assert_eq!(error.code, ErrorCode::Forbidden);
    let error = jobs
        .admit(
            ADMIN,
            &launch("prod.remove", json!({"resource": "mox1"}), "c"),
        )
        .unwrap_err();
    assert_eq!(error.code, ErrorCode::Invalid);
    assert_eq!(error.field_errors["resource"], "expected prodN");
    let error = jobs
        .admit(ADMIN, &launch("no.such", json!({}), "d"))
        .unwrap_err();
    assert_eq!(error.code, ErrorCode::NotFound);

    lab.cluster.set_quorate(false);
    let error = jobs
        .admit(
            VIEWER,
            &launch("diagnostics.cluster_health", json!({}), "e"),
        )
        .unwrap_err();
    assert_eq!(error.code, ErrorCode::NoQuorum);
    lab.cluster.set_quorate(true);
    let ok = jobs
        .admit(
            VIEWER,
            &launch("diagnostics.cluster_health", json!({}), "e"),
        )
        .unwrap();
    assert_eq!(
        ok.job.target,
        host(1),
        "read-only diagnostics run where admitted"
    );
    assert!(ok.job.locks.is_empty());
}

#[test]
fn targets_follow_each_operation_rule() {
    let lab = Lab::new();
    let target = |jobs: &Jobs, operation: &str, parameters: Value, key: &str| {
        jobs.admit(ADMIN, &launch(operation, parameters, key))
            .map(|a| a.job.target.to_string())
            .map_err(|e| (e.code, e.message))
    };
    let mox1 = lab.host(1);
    let mox3 = lab.host(3);
    assert_eq!(
        target(&mox3, "qdevice.add", json!({}), "q").unwrap(),
        "mox1",
        "QDevice changes run on the duty host"
    );
    assert_eq!(
        target(&mox1, "hosts.remove", json!({"host": "mox1"}), "h1")
            .unwrap_err()
            .0,
        ErrorCode::Conflict,
        "cluster-membership is held by qdevice.add"
    );
    let lab = Lab::new();
    let mox1 = lab.host(1);
    assert_eq!(
        target(&mox1, "hosts.remove", json!({"host": "mox1"}), "h1").unwrap(),
        "mox2",
        "a host is never removed by itself"
    );
    let lab = Lab::new();
    let mox1 = lab.host(1);
    assert_eq!(
        target(&mox1, "staging.add", json!({"source": "prod1"}), "s").unwrap(),
        "mox1",
        "the lowest online standby"
    );
    lab.cluster.set_online(&[1, 3]);
    let (code, message) =
        target(&mox1, "prod.extend_disk", json!({"resource": "prod1"}), "x").unwrap_err();
    assert_eq!(code, ErrorCode::Unavailable);
    assert!(message.contains("mox2 is offline"), "{message}");
    let (code, _) =
        target(&mox1, "prod.extend_disk", json!({"resource": "prod9"}), "y").unwrap_err();
    assert_eq!(code, ErrorCode::NotFound);
    let (code, _) = target(&mox1, "staging.add", json!({"source": "prod9"}), "z").unwrap_err();
    assert_eq!(
        code,
        ErrorCode::NotFound,
        "a guest the registry doesn't have"
    );
}

#[test]
fn a_lease_that_expires_interrupts_the_job_and_keeps_its_resources() {
    let lab = Lab::new();
    let mox1 = lab.host(1);
    let mox2 = lab.host(2);
    let job = mox1
        .admit(
            OPERATOR,
            &launch("prod.change_owner", json!({"resource": "prod1"}), "k"),
        )
        .unwrap()
        .job;
    assert_eq!(job.state, JobState::Admitted);
    assert_eq!(
        mox1.claim(job.id).unwrap_err().code,
        ErrorCode::Conflict,
        "only the target claims"
    );
    let claim = mox2.claim(job.id).unwrap();
    assert_eq!(claim.job.state, JobState::Running);
    assert_eq!(claim.nonce.len(), 32);
    assert_eq!(mox2.claim(job.id).unwrap_err().code, ErrorCode::Conflict);

    lab.advance(30_000);
    let renewed = mox2.renew(job.id, &claim.nonce, &running()).unwrap();
    assert_eq!(
        renewed.lease.unwrap().expires_at_ms,
        lab.clock.load(Ordering::SeqCst) + 60_000
    );
    assert_eq!(
        mox2.renew(job.id, "0".repeat(32).as_str(), &running())
            .unwrap_err()
            .code,
        ErrorCode::Conflict,
        "only the same nonce renews"
    );
    assert!(mox1.sweep().unwrap().is_empty());

    lab.advance(60_000);
    let swept = mox1.sweep().unwrap();
    assert_eq!(swept.len(), 1);
    assert_eq!(swept[0].state, JobState::Interrupted);
    assert!(
        swept[0]
            .message
            .as_ref()
            .unwrap()
            .contains("mox2 stopped renewing")
    );
    assert_eq!(
        mox2.renew(job.id, &claim.nonce, &running())
            .unwrap_err()
            .code,
        ErrorCode::Conflict
    );

    let blocked = mox1
        .admit(
            OPERATOR,
            &launch("prod.extend_disk", json!({"resource": "prod1"}), "k2"),
        )
        .unwrap_err();
    assert_eq!(blocked.code, ErrorCode::Conflict);
    assert!(
        blocked.message.contains("until someone resumes it"),
        "{}",
        blocked.message
    );

    let resume = mox1.resume(OPERATOR, job.id).unwrap_err();
    assert_eq!(
        resume.code,
        ErrorCode::Invalid,
        "script operations never resume"
    );
    assert_eq!(
        mox1.cancel(OPERATOR, job.id, false).unwrap_err().code,
        ErrorCode::Conflict
    );
    assert_eq!(
        mox1.reconcile(VIEWER, job.id).unwrap_err().code,
        ErrorCode::Forbidden
    );
    let failed = mox1.reconcile(OPERATOR, job.id).unwrap();
    assert_eq!(failed.state, JobState::Failed);
    assert!(
        failed
            .message
            .unwrap()
            .contains("marked failed by olive@pve")
    );

    mox1.admit(
        OPERATOR,
        &launch("prod.extend_disk", json!({"resource": "prod1"}), "k2"),
    )
    .unwrap();
}

#[test]
fn the_holder_reports_the_outcome_even_after_an_interruption() {
    let lab = Lab::new();
    let mox2 = lab.host(2);
    let job = mox2
        .admit(
            OPERATOR,
            &launch("prod.change_owner", json!({"resource": "prod1"}), "k"),
        )
        .unwrap()
        .job;
    let claim = mox2.claim(job.id).unwrap();
    lab.advance(61_000);
    mox2.sweep().unwrap();
    let done = mox2
        .finish(
            job.id,
            &claim.nonce,
            JobState::Completed,
            "moved to mox3",
            0,
        )
        .unwrap();
    assert_eq!(done.state, JobState::Completed);
    assert!(done.finished_at_ms.is_some());
    assert_eq!(
        mox2.finish(job.id, &claim.nonce, JobState::Failed, "again", 0)
            .unwrap_err()
            .code,
        ErrorCode::Conflict
    );
}

#[test]
fn an_interrupted_checkpoint_job_resumes_where_its_rule_now_points() {
    let lab = Lab::new();
    let mox1 = lab.host(1);
    let mox2 = lab.host(2);
    let job = mox1
        .admit(
            OPERATOR,
            &launch("test.resumable", json!({"resource": "prod1"}), "k"),
        )
        .unwrap()
        .job;
    mox2.claim(job.id).unwrap();
    lab.advance(61_000);
    mox1.sweep().unwrap();

    lab.cluster
        .owners
        .lock()
        .unwrap()
        .insert(guest("prod1"), host(3));
    lab.cluster.set_quorate(false);
    assert_eq!(
        mox1.resume(OPERATOR, job.id).unwrap_err().code,
        ErrorCode::NoQuorum
    );
    lab.cluster.set_quorate(true);
    let resumed = mox1.resume(OPERATOR, job.id).unwrap();
    assert_eq!(resumed.state, JobState::Admitted);
    assert_eq!(resumed.target, host(3));
    let claim = lab.host(3).claim(job.id).unwrap();
    assert_eq!(claim.job.attempts, 2);
}

#[test]
fn a_restarted_host_interrupts_its_jobs_and_resumes_checkpoint_ones_in_place() {
    let lab = Lab::new();
    let (mox1, mox2, mox3) = (lab.host(1), lab.host(2), lab.host(3));
    lab.cluster
        .owners
        .lock()
        .unwrap()
        .insert(guest("prod3"), host(2));
    let admit = |operation: &str, resource: &str, key: &str| {
        mox1.admit(
            OPERATOR,
            &launch(operation, json!({"resource": resource}), key),
        )
        .unwrap()
        .job
    };
    let native = admit("test.resumable", "prod1", "a");
    let script = admit("prod.change_owner", "prod3", "b");
    let elsewhere = admit("test.resumable", "prod2", "c");
    mox2.claim(native.id).unwrap();
    mox2.claim(script.id).unwrap();
    mox3.claim(elsewhere.id).unwrap();

    let interrupted = mox2.interrupt_own().unwrap();
    let mut ids: Vec<_> = interrupted.iter().map(|r| r.id).collect();
    ids.sort();
    let mut expected = vec![native.id, script.id];
    expected.sort();
    assert_eq!(ids, expected, "without waiting for the leases");
    assert!(interrupted.iter().all(|r| r.state == JobState::Interrupted));
    assert_eq!(mox2.get(elsewhere.id).unwrap().state, JobState::Running);
    assert!(mox2.interrupt_own().unwrap().is_empty());

    let refused = |jobs: &bmacd::jobs::Jobs, id| jobs.resume_after_restart(id).unwrap_err().code;
    assert_eq!(
        refused(&mox1, native.id),
        ErrorCode::Conflict,
        "only the host that ran it"
    );
    assert_eq!(
        refused(&mox2, script.id),
        ErrorCode::Conflict,
        "scripts never resume"
    );
    lab.cluster.set_quorate(false);
    assert_eq!(refused(&mox2, native.id), ErrorCode::NoQuorum);
    lab.cluster.set_quorate(true);
    lab.cluster
        .owners
        .lock()
        .unwrap()
        .insert(guest("prod1"), host(3));
    let resumed = mox2.resume_after_restart(native.id).unwrap();
    assert_eq!(resumed.state, JobState::Admitted);
    assert_eq!(resumed.target, host(2), "it continues where it restarted");
    assert_eq!(refused(&mox2, native.id), ErrorCode::Conflict);
    assert_eq!(mox2.claim(native.id).unwrap().job.attempts, 2);
}

#[test]
fn cancellation_follows_the_phase_and_the_role() {
    let lab = Lab::new();
    let mox2 = lab.host(2);
    let waiting = mox2
        .admit(
            OPERATOR,
            &launch("prod.change_owner", json!({"resource": "prod1"}), "a"),
        )
        .unwrap()
        .job;
    let cancelled = mox2.cancel(OPERATOR, waiting.id, false).unwrap();
    assert_eq!(
        cancelled.state,
        JobState::Cancelled,
        "a job that never started stops at once"
    );
    assert_eq!(
        mox2.cancel(OPERATOR, waiting.id, false).unwrap_err().code,
        ErrorCode::Conflict
    );

    let job = mox2
        .admit(
            OPERATOR,
            &launch("prod.change_owner", json!({"resource": "prod1"}), "b"),
        )
        .unwrap()
        .job;
    let claim = mox2.claim(job.id).unwrap();
    let unsafe_phase = Progress {
        phase: Some("relocating".into()),
        cancel_allowed: false,
        pending_request: None,
        last_seq: 0,
    };
    mox2.renew(job.id, &claim.nonce, &unsafe_phase).unwrap();
    assert_eq!(
        mox2.cancel(VIEWER, job.id, false).unwrap_err().code,
        ErrorCode::Forbidden
    );
    let refused = mox2.cancel(OPERATOR, job.id, false).unwrap_err();
    assert_eq!(refused.code, ErrorCode::Conflict);
    assert!(
        refused.message.contains("\"relocating\""),
        "{}",
        refused.message
    );
    assert_eq!(
        mox2.cancel(OPERATOR, job.id, true).unwrap_err().code,
        ErrorCode::Forbidden,
        "a forced cancel is for administrators"
    );

    let asking = Progress {
        pending_request: Some("confirm".into()),
        ..unsafe_phase
    };
    mox2.renew(job.id, &claim.nonce, &asking).unwrap();
    let ordered = lab.host(1).cancel(OPERATOR, job.id, false).unwrap();
    assert_eq!(
        ordered.state,
        JobState::Running,
        "the running host acts on the order"
    );
    assert!(ordered.summary().cancel_requested);
    let order = mox2
        .renew(job.id, &claim.nonce, &asking)
        .unwrap()
        .cancel
        .unwrap();
    assert!(!order.forced);
    let first = order.requested_at_ms;
    lab.advance(1_000);
    let forced = mox2.cancel(ADMIN, job.id, true).unwrap().cancel.unwrap();
    assert!(forced.forced);
    assert_eq!(forced.requested_at_ms, first);
    let done = mox2
        .finish(job.id, &claim.nonce, JobState::Cancelled, "cancelled", 0)
        .unwrap();
    assert!(!done.summary().cancel_requested);
}

#[test]
fn a_job_its_target_never_claims_fails() {
    let lab = Lab::new();
    let mox1 = lab.host(1);
    let job = mox1
        .admit(
            OPERATOR,
            &launch("prod.change_owner", json!({"resource": "prod1"}), "k"),
        )
        .unwrap()
        .job;
    lab.advance(299_999);
    assert!(mox1.sweep().unwrap().is_empty());
    lab.advance(1);
    let swept = mox1.sweep().unwrap();
    assert_eq!(swept[0].id, job.id);
    assert_eq!(swept[0].state, JobState::Failed);
    assert_eq!(swept[0].message.as_deref(), Some("mox2 never started it"));
}

#[test]
fn only_the_newest_finished_jobs_are_kept() {
    let lab = Lab::with_keep(3);
    let mox1 = lab.host(1);
    let active = mox1
        .admit(
            OPERATOR,
            &launch("prod.change_owner", json!({"resource": "prod2"}), "active"),
        )
        .unwrap()
        .job;
    let mut finished = Vec::new();
    for i in 0..5 {
        lab.advance(1_000);
        let job = mox1
            .admit(
                VIEWER,
                &launch("diagnostics.cluster_health", json!({}), &format!("k{i}")),
            )
            .unwrap()
            .job;
        let claim = mox1.claim(job.id).unwrap();
        mox1.finish(job.id, &claim.nonce, JobState::Completed, "ok", 0)
            .unwrap();
        finished.push(job.id);
    }
    let kept: Vec<_> = mox1.list().unwrap().iter().map(|j| j.id).collect();
    assert_eq!(kept.len(), 4);
    for id in &finished[..2] {
        assert!(!kept.contains(id));
        assert_eq!(mox1.get(*id).unwrap_err().code, ErrorCode::NotFound);
    }
    assert_eq!(
        kept,
        [finished[4], finished[3], finished[2], active.id],
        "newest first"
    );
}
