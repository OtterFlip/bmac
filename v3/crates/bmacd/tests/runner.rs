// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

//! The job runner end to end: jobs admitted on one simulated host run on
//! their target, through the real supervisor and mock scripts, with every
//! host sharing one temporary pmxcfs directory.

use std::collections::{BTreeMap, HashMap};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bmac_core::job::{Answer, EventBody, JobState};
use bmac_core::{GuestName, HostSlot, JobId};
use bmac_pmxcfs::{Pmxcfs, RealFs};
use bmac_proto::api::{ApiError, ErrorCode, LaunchJob, Role};
use bmac_proto::stream::StreamBody;
use bmacd::auth::system_clock;
use bmacd::cluster::{ClusterView, Membership};
use bmacd::engine::events::EventHub;
use bmacd::jobs::{Caller, JobRecord, Jobs, JobsConfig};

/// Hands answers straight to the other simulated host's runner.
#[derive(Default)]
struct Relay(Mutex<HashMap<HostSlot, Arc<Runner>>>);

impl AnswerRelay for Relay {
    fn forward(
        &self,
        to: HostSlot,
        answer: RelayedAnswer,
    ) -> Pin<Box<dyn Future<Output = Result<(), ApiError>> + Send + '_>> {
        let runner = self.0.lock().unwrap().get(&to).cloned();
        Box::pin(async move {
            match runner {
                Some(runner) => runner.receive(answer).await,
                None => Err(ApiError::new(
                    ErrorCode::Unavailable,
                    format!("{to} is unreachable"),
                )),
            }
        })
    }
}

fn reply(caller: Caller, job: JobId, request_id: &str, answer: Answer) -> RelayedAnswer {
    RelayedAnswer {
        user: caller.user.into(),
        role: caller.role,
        job,
        request_id: request_id.into(),
        answer,
    }
}
use bmacd::db::Database;
use bmacd::operations::Registry;
use bmacd::runner::{AnswerRelay, EngineSink, RelayedAnswer, Runner, RunnerConfig};
use bmacd::store::Store;
use bmacd::supervisor::{Supervisor, SupervisorConfig};
use serde_json::{Value, json};

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

struct Cluster(Mutex<Membership>);

impl ClusterView for Cluster {
    fn membership(&self) -> Result<Membership, String> {
        Ok(self.0.lock().unwrap().clone())
    }

    fn guest_owner(&self, _guest: GuestName) -> Result<Option<HostSlot>, String> {
        Ok(None)
    }

    fn placement(&self, _guest: GuestName) -> Result<Vec<HostSlot>, String> {
        Ok(Vec::new())
    }
}

fn operation(id: &str, script: &str, lock: &str) -> Value {
    json!({
        "id": id, "title": id, "summary": "s", "description": "d",
        "category": "diagnostics", "kind": "script", "script": script,
        "min_role": "operator", "mutating": true, "destructive": false,
        "params": [], "locks": [lock],
        "target": {"rule": "duty"}, "cancel": {"rule": "script"}, "resume": "never"
    })
}

fn copy_tree(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for entry in std::fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let target = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_tree(&entry.path(), &target);
        } else {
            std::fs::copy(entry.path(), target).unwrap();
        }
    }
}

struct Lab {
    _dir: tempfile::TempDir,
    pmxcfs: Pmxcfs,
    jobs_dir: PathBuf,
    legacy: PathBuf,
    cluster: Arc<Cluster>,
    registry: Arc<Registry>,
    relay: Arc<Relay>,
    tasks: Vec<tokio::task::JoinHandle<()>>,
}

struct Node {
    jobs: Arc<Jobs>,
    hub: Arc<EventHub>,
    store: Store,
    db_path: PathBuf,
    runner: Arc<Runner>,
}

impl Lab {
    fn new() -> Lab {
        let dir = tempfile::tempdir().unwrap();
        let legacy = dir.path().join("legacy");
        let crate_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
        for sub in ["scripts/lib", "scripts/utilities"] {
            copy_tree(&crate_dir.join("../../legacy").join(sub), &legacy.join(sub));
        }
        copy_tree(&crate_dir.join("tests/mock-scripts"), &legacy.join("mock"));
        let operations = json!({
            "schema_version": 1,
            "operations": [
                operation("test.interactive", "mock/interactive.sh", "staging"),
                operation("test.long", "mock/long_running.sh", "capacity"),
                operation("test.missing", "mock/missing.sh", "qdevice"),
                operation("test.secret", "mock/secret.sh", "storage"),
            ],
        });
        let native = |id: &str, lock: &str| {
            serde_json::from_value(json!({
                "id": id, "title": id, "summary": "s", "description": "d",
                "category": "hosts", "kind": "native",
                "min_role": "operator", "mutating": true, "destructive": false,
                "params": [], "locks": [lock],
                "target": {"rule": "duty"}, "cancel": {"rule": "phases"}, "resume": "checkpoint"
            }))
            .unwrap()
        };
        let natives = vec![
            native("test.restarts", "cluster-membership"),
            native("test.crashed", "qdevice"),
        ];
        let registry = Arc::new(Registry::from_parts(&operations.to_string(), natives).unwrap());
        let pve = dir.path().join("pve");
        Lab {
            pmxcfs: Pmxcfs::new(&pve, Arc::new(RealFs)),
            jobs_dir: pve.join("priv/bmac/jobs"),
            legacy,
            _dir: dir,
            cluster: Arc::new(Cluster(Mutex::new(Membership {
                quorate: true,
                online: [host(1), host(2)].into_iter().collect(),
                ..Membership::default()
            }))),
            registry,
            relay: Arc::new(Relay::default()),
            tasks: Vec::new(),
        }
    }

    /// A simulated host; with `relay`, answers given on it reach jobs on the
    /// other relaying hosts.
    async fn node(&mut self, n: u8, relay: bool) -> Node {
        let config = JobsConfig {
            pmxcfs: self.pmxcfs.clone(),
            dir: self.jobs_dir.clone(),
            host: host(n),
            lease_ttl_ms: 60_000,
            claim_timeout_ms: 300_000,
            keep_finished: 500,
            lock_timeout: Duration::from_secs(30),
            clock: system_clock(),
        };
        let jobs = Arc::new(Jobs::new(
            config,
            self.registry.clone(),
            self.cluster.clone(),
        ));
        let hub = EventHub::with_epoch(format!("h{n}"));
        let db_path = self
            .legacy
            .parent()
            .unwrap()
            .join(format!("mox{n}/bmac.db"));
        std::fs::create_dir_all(db_path.parent().unwrap()).unwrap();
        let store = Store::open(Database::open(&db_path).unwrap(), format!("mox{n}"), 0)
            .await
            .unwrap();
        let (sink, changes) = EngineSink::new(hub.clone(), store.clone());
        let mut sup = SupervisorConfig::system(system_clock(), Arc::new(|_: &str| false));
        sup.root = self.legacy.clone();
        sup.work_root = self.legacy.parent().unwrap().join(format!("mox{n}/work"));
        sup.bash = PathBuf::from("bash");
        sup.native = self.legacy.join("mock/native.sh");
        sup.env = vec![(
            "PATH".into(),
            std::env::var("PATH").unwrap_or_else(|_| "/usr/bin:/bin".into()),
        )];
        let supervisor = Supervisor::new(sup, sink.clone());
        let runner = Runner::new(
            jobs.clone(),
            supervisor,
            sink,
            RunnerConfig {
                poll_every: Duration::from_millis(100),
                renew_every: Duration::from_secs(5),
                cancel_check_every: Duration::from_millis(100),
            },
        );
        let runner = if relay {
            let runner = runner.with_relay(self.relay.clone());
            self.relay.0.lock().unwrap().insert(host(n), runner.clone());
            runner
        } else {
            runner
        };
        self.tasks.push(runner.spawn(changes));
        Node {
            jobs,
            hub,
            store,
            db_path,
            runner,
        }
    }
}

impl Drop for Lab {
    fn drop(&mut self) {
        for task in &self.tasks {
            task.abort();
        }
    }
}

fn launch(jobs: &Jobs, operation: &str) -> JobRecord {
    jobs.admit(
        OPERATOR,
        &LaunchJob {
            operation: operation.parse().unwrap(),
            parameters: BTreeMap::new(),
            idempotency_key: format!("k-{operation}"),
            dry_run: false,
        },
    )
    .unwrap()
    .job
}

async fn wait_for(
    jobs: &Jobs,
    id: JobId,
    what: &str,
    cond: impl Fn(&JobRecord) -> bool,
) -> JobRecord {
    for _ in 0..400 {
        let record = jobs.get(id).unwrap();
        if cond(&record) {
            return record;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("timed out waiting for {what}: {:?}", jobs.get(id).unwrap());
}

async fn answer(node: &Node, id: JobId, after: &str, answer: Answer) -> String {
    let record = wait_for(&node.jobs, id, "the next request", |r| {
        r.pending_request.as_deref().is_some_and(|p| p != after)
    })
    .await;
    let request = record.pending_request.unwrap();
    node.runner
        .answer(self::reply(OPERATOR, id, &request, answer))
        .await
        .unwrap();
    request
}

fn values(v: Value) -> Answer {
    Answer::Values {
        values: serde_json::from_value(v).unwrap(),
    }
}

/// Every event `hub` published for `job`, in order.
fn job_events(hub: &Arc<EventHub>, job: JobId) -> Vec<(u64, EventBody)> {
    let subscription = hub.subscribe("test", Some(&format!("{}-0", hub_epoch(hub))));
    let StreamBody::Hello { stream } = &subscription.hello.body else {
        panic!("a stream starts with hello");
    };
    hub.watch("test", stream, vec![job]).unwrap();
    subscription
        .replay()
        .iter()
        .filter_map(|m| match &m.body {
            StreamBody::JobEvent { event } if event.job == job => {
                Some((event.seq, event.body.clone()))
            }
            _ => None,
        })
        .collect()
}

fn hub_epoch(hub: &Arc<EventHub>) -> String {
    let id = hub.publish(StreamBody::Quorum { quorate: true });
    id.rsplit_once('-').unwrap().0.to_string()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_job_admitted_on_one_host_runs_on_its_target_and_takes_answers_from_any_host() {
    let mut lab = Lab::new();
    let mox1 = lab.node(1, true).await;
    let mox2 = lab.node(2, true).await;
    let mox3 = lab.node(3, false).await;
    let job = launch(&mox2.jobs, "test.interactive");
    assert_eq!(job.target, host(1), "the duty host runs it");

    let record = wait_for(&mox1.jobs, job.id, "a request", |r| {
        r.pending_request.is_some()
    })
    .await;
    assert_eq!(record.state, JobState::Running);
    assert_eq!(record.lease.as_ref().unwrap().holder, host(1));
    let request = record.pending_request.unwrap();
    let unrelayed = mox3
        .runner
        .answer(reply(OPERATOR, job.id, &request, values(json!({}))))
        .await
        .unwrap_err();
    assert_eq!(unrelayed.code, ErrorCode::Unavailable);
    assert!(unrelayed.message.contains("runs on mox1"));
    let viewer = mox2
        .runner
        .answer(reply(VIEWER, job.id, &request, values(json!({}))))
        .await
        .unwrap_err();
    assert_eq!(viewer.code, ErrorCode::Forbidden);
    let forged = mox1
        .runner
        .receive(reply(VIEWER, job.id, &request, values(json!({}))))
        .await
        .unwrap_err();
    assert_eq!(
        forged.code,
        ErrorCode::Forbidden,
        "the running host checks again"
    );

    let group = values(json!({"cores": 4, "hosts": ["mox1", "mox2"]}));
    mox2.runner
        .answer(reply(OPERATOR, job.id, &request, group.clone()))
        .await
        .unwrap();
    let again = mox2
        .runner
        .answer(reply(OPERATOR, job.id, &request, group))
        .await
        .unwrap_err();
    assert_eq!(
        again.code,
        ErrorCode::Conflict,
        "a second answer has no effect"
    );
    let confirm = answer(&mox2, job.id, &request, Answer::Confirm { confirmed: true }).await;
    answer(&mox1, job.id, &confirm, Answer::Acknowledge).await;

    let done = wait_for(&mox2.jobs, job.id, "the end", |r| r.state.is_terminal()).await;
    assert_eq!(done.state, JobState::Completed, "{done:?}");
    assert!(!mox1.runner.is_active(job.id));

    let events = job_events(&mox1.hub, job.id);
    let stored = mox1.store.job_events(job.id, 1, 2000).await.unwrap();
    assert!(!stored.more);
    assert_eq!(
        stored
            .events
            .iter()
            .map(|e| (e.seq, e.body.clone()))
            .collect::<Vec<_>>(),
        events,
        "every published event was stored first"
    );
    assert_eq!(done.last_seq, events.last().unwrap().0);
    assert!(
        matches!(events.first(), Some((1, EventBody::Started { host: h, user, .. })) if *h == host(1) && user == "olive@pve")
    );
    assert!(matches!(
        events.last(),
        Some((_, EventBody::Completed { .. }))
    ));
    let seqs: Vec<u64> = events.iter().map(|(s, _)| *s).collect();
    assert_eq!(seqs, (1..=seqs.len() as u64).collect::<Vec<_>>());
    assert!(
        job_events(&mox2.hub, job.id).is_empty(),
        "events stay on the host that ran it"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_cancel_placed_on_another_host_reaches_the_script() {
    let mut lab = Lab::new();
    let mox1 = lab.node(1, false).await;
    let mox2 = lab.node(2, false).await;
    let job = launch(&mox2.jobs, "test.long");
    wait_for(
        &mox2.jobs,
        job.id,
        "the step that may be interrupted",
        |r| {
            r.cancel_allowed
                && r.phase
                    .as_deref()
                    .is_some_and(|p| !matches!(p, "Starting" | "Finalizing receive"))
        },
    )
    .await;
    mox2.jobs.cancel(OPERATOR, job.id, false).unwrap();
    let done = wait_for(&mox2.jobs, job.id, "the end", |r| r.state.is_terminal()).await;
    assert_eq!(done.state, JobState::Cancelled, "{done:?}");
    assert!(
        job_events(&mox1.hub, job.id).iter().any(
            |(_, e)| matches!(e, EventBody::Output { text, .. } if text.contains("cleaning up"))
        )
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_script_missing_on_the_target_fails_the_job() {
    let mut lab = Lab::new();
    let mox1 = lab.node(1, false).await;
    let job = launch(&mox1.jobs, "test.missing");
    mox1.runner.kick();
    let done = wait_for(&mox1.jobs, job.id, "the end", |r| r.state.is_terminal()).await;
    assert_eq!(done.state, JobState::Failed);
    assert!(done.message.unwrap().contains("not installed"));
    let again = launch(&mox1.jobs, "test.missing");
    assert_eq!(
        again.id, job.id,
        "the idempotency key still names the finished job"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn secrets_are_redacted_before_anything_is_stored() {
    let mut lab = Lab::new();
    let mox1 = lab.node(1, false).await;
    let job = launch(&mox1.jobs, "test.secret");
    let request = wait_for(&mox1.jobs, job.id, "the secret prompt", |r| {
        r.pending_request.is_some()
    })
    .await
    .pending_request
    .unwrap();
    mox1.runner
        .answer(reply(
            OPERATOR,
            job.id,
            &request,
            values(json!({"key": "tskey-very-secret"})),
        ))
        .await
        .unwrap();
    wait_for(&mox1.jobs, job.id, "the end", |r| r.state.is_terminal()).await;
    let page = mox1.store.job_events(job.id, 1, 2000).await.unwrap();
    let text = serde_json::to_string(&page.events).unwrap();
    assert!(text.contains("the key is [redacted]"), "{text}");
    for suffix in ["", "-wal"] {
        let path = PathBuf::from(format!("{}{suffix}", mox1.db_path.display()));
        if let Ok(bytes) = std::fs::read(&path) {
            assert!(
                !bytes.windows(17).any(|w| w == b"tskey-very-secret"),
                "{} holds the secret",
                path.display()
            );
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_job_that_restarted_its_host_resumes_by_itself() {
    let mut lab = Lab::new();
    let before = Jobs::new(
        JobsConfig {
            pmxcfs: lab.pmxcfs.clone(),
            dir: lab.jobs_dir.clone(),
            host: host(1),
            lease_ttl_ms: 60_000,
            claim_timeout_ms: 300_000,
            keep_finished: 500,
            lock_timeout: Duration::from_secs(30),
            clock: system_clock(),
        },
        lab.registry.clone(),
        lab.cluster.clone(),
    );
    let work = lab.legacy.parent().unwrap().join("mox1/work");
    let mut running = Vec::new();
    for (operation, marker) in [("test.restarts", Some("boot-1")), ("test.crashed", None)] {
        let job = launch(&before, operation);
        before.claim(job.id).unwrap();
        let dir = work.join(job.id.to_string());
        std::fs::create_dir_all(&dir).unwrap();
        if let Some(boot) = marker {
            std::fs::write(dir.join(bmacd::native::RESTART_MARKER), boot).unwrap();
        }
        running.push(job.id);
    }

    let node = lab.node(1, false).await;
    let (done, resumed) = node.runner.recover("boot-2").await;
    assert!(done);
    assert_eq!(
        resumed,
        vec![running[0]],
        "only the job that asked for the restart"
    );
    let finished = wait_for(&node.jobs, running[0], "the resumed job to finish", |r| {
        r.state.is_terminal()
    })
    .await;
    assert_eq!(finished.state, JobState::Completed, "{finished:?}");
    assert_eq!(finished.attempts, 2);
    let crashed = node.jobs.get(running[1]).unwrap();
    assert_eq!(
        crashed.state,
        JobState::Interrupted,
        "an unasked restart waits for an operator"
    );
    assert!(crashed.message.unwrap().contains("restarted while it ran"));
}

#[test]
fn a_restart_counts_only_once_the_boot_has_changed() {
    let dir = tempfile::tempdir().unwrap();
    assert!(!bmacd::native::restarted_since_asked(dir.path(), "b1"));
    std::fs::write(dir.path().join(bmacd::native::RESTART_MARKER), "b1\n").unwrap();
    assert!(!bmacd::native::restarted_since_asked(dir.path(), "b1"));
    assert!(bmacd::native::restarted_since_asked(dir.path(), "b2"));
}
