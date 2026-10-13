// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

//! The script supervisor end to end, ported from v2's
//! `dashboard/engine/tests/supervisor.rs`: real Bash mock scripts running
//! through the copied `legacy/scripts/utilities/ui_json_run.sh` and
//! `legacy/scripts/lib/ui_protocol.sh`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bmac_core::JobId;
use bmac_core::job::{Answer, EventBody, JobEvent, JobState, PhaseStatus, RequestForm};
use bmac_proto::api::ErrorCode;
use bmacd::auth::system_clock;
use bmacd::supervisor::{JobSink, Outcome, RunView, Supervisor, SupervisorConfig};
use serde_json::{Value, json};
use tokio::sync::watch;

#[derive(Default)]
struct Collect {
    events: Mutex<Vec<JobEvent>>,
    views: Mutex<Vec<(JobId, RunView)>>,
}

impl JobSink for Collect {
    fn events(&self, events: Vec<JobEvent>) {
        self.events.lock().unwrap().extend(events);
    }

    fn changed(&self, job: JobId, view: &RunView) {
        self.views.lock().unwrap().push((job, view.clone()));
    }
}

struct Fixture {
    _dir: tempfile::TempDir,
    root: PathBuf,
    sink: Arc<Collect>,
    sup: Arc<Supervisor>,
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

type Prepare = Arc<dyn Fn() -> Result<(), String> + Send + Sync>;

fn fixture_with(env: &[(&str, &str)]) -> Fixture {
    fixture_prepared(env, None)
}

fn fixture_prepared(env: &[(&str, &str)], prepare: Option<Prepare>) -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("legacy");
    let crate_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    for sub in ["scripts/lib", "scripts/utilities"] {
        copy_tree(&crate_dir.join("../../legacy").join(sub), &root.join(sub));
    }
    copy_tree(&crate_dir.join("tests/mock-scripts"), &root.join("mock"));
    let sink = Arc::new(Collect::default());
    let mut config = SupervisorConfig::system(
        system_clock(),
        Arc::new(|id: &str| id == "show_prod_vm_state"),
    );
    config.root = root.clone();
    config.work_root = dir.path().join("work");
    config.bash = PathBuf::from("bash");
    config.env = vec![(
        "PATH".into(),
        std::env::var("PATH").unwrap_or_else(|_| "/usr/bin:/bin".into()),
    )];
    config
        .env
        .extend(env.iter().map(|(k, v)| (k.to_string(), v.to_string())));
    config.force_kill_after = Duration::from_secs(2);
    config.prepare = prepare;
    config.native = root.join("mock/native.sh");
    let sup = Supervisor::new(config, sink.clone());
    Fixture {
        _dir: dir,
        root,
        sink,
        sup,
    }
}

fn fixture() -> Fixture {
    fixture_with(&[])
}

impl Fixture {
    fn start(&self, script: &str) -> (JobId, watch::Receiver<Option<Outcome>>) {
        let job = JobId::random();
        let outcome = self
            .sup
            .start(job, &format!("mock/{script}.sh"), &[], 2)
            .unwrap();
        (job, outcome)
    }

    async fn wait_for(&self, job: JobId, what: &str, cond: impl Fn(&RunView) -> bool) -> RunView {
        for _ in 0..300 {
            if let Some(view) = self.sup.view(job)
                && cond(&view)
            {
                return view;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!("timed out waiting for {what}: {:?}", self.sup.view(job));
    }

    async fn request(&self, job: JobId) -> (String, RequestForm) {
        self.wait_for(job, "a request", |v| v.pending_request.is_some())
            .await;
        let request = self.sup.pending_request(job).unwrap();
        (
            request.request_id().unwrap().to_string(),
            request.request_form().unwrap(),
        )
    }

    fn events(&self, job: JobId) -> Vec<EventBody> {
        self.sink
            .events
            .lock()
            .unwrap()
            .iter()
            .filter(|e| e.job == job)
            .map(|e| e.body.clone())
            .collect()
    }
}

async fn finished(mut outcome: watch::Receiver<Option<Outcome>>) -> Outcome {
    tokio::time::timeout(Duration::from_secs(20), outcome.wait_for(Option::is_some))
        .await
        .expect("the run finishes")
        .unwrap()
        .clone()
        .unwrap()
}

fn values(v: Value) -> Answer {
    Answer::Values {
        values: serde_json::from_value::<BTreeMap<String, Value>>(v).unwrap(),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_interactive_script_round_trip() {
    let f = fixture();
    let (job, outcome) = f.start("interactive");

    let (group, form) = f.request(job).await;
    assert!(matches!(form, RequestForm::InputGroup { .. }));
    let refused = f
        .sup
        .answer(
            job,
            &group,
            &values(json!({"cores": 3, "hosts": ["mox1"]})),
            "bob@pve",
        )
        .unwrap_err();
    assert_eq!(
        refused.code,
        ErrorCode::Invalid,
        "the engine checks what the request allows"
    );
    f.sup
        .answer(
            job,
            &group,
            &values(json!({"cores": 3, "hosts": ["mox1", "mox2"]})),
            "bob@pve",
        )
        .unwrap();
    let (again, _) = f.request(job).await;
    assert_eq!(again, group, "the script judged the answer and asks again");
    assert!(f.events(job).iter().any(
        |e| matches!(e, EventBody::ValidationError { field_errors, .. } if field_errors.contains_key("cores"))
    ));
    f.sup
        .answer(
            job,
            &group,
            &values(json!({"cores": 4, "hosts": ["mox1", "mox3"]})),
            "bob@pve",
        )
        .unwrap();

    let (confirm, form) = f.request(job).await;
    let RequestForm::Confirm {
        confirmation_text, ..
    } = form
    else {
        panic!("expected a confirmation, got {form:?}");
    };
    assert_eq!(confirmation_text.as_deref(), Some("GO"));
    f.sup
        .answer(
            job,
            &confirm,
            &Answer::Confirm { confirmed: true },
            "bob@pve",
        )
        .unwrap();
    let second = f
        .sup
        .answer(
            job,
            &confirm,
            &Answer::Confirm { confirmed: true },
            "eve@pve",
        )
        .unwrap_err();
    assert_eq!(
        second.code,
        ErrorCode::Conflict,
        "a second answer has no effect"
    );

    let (manual, form) = f.request(job).await;
    assert!(matches!(form, RequestForm::ManualAction { .. }));
    f.sup
        .answer(job, &manual, &Answer::Acknowledge, "bob@pve")
        .unwrap();

    let done = finished(outcome).await;
    assert_eq!(done.state, JobState::Completed, "{done:?}");
    assert_eq!(done.exit_code, Some(0));
    let events = f.events(job);
    let seqs: Vec<u64> = f
        .sink
        .events
        .lock()
        .unwrap()
        .iter()
        .filter(|e| e.job == job)
        .map(|e| e.seq)
        .collect();
    assert_eq!(
        seqs,
        (2..2 + seqs.len() as u64).collect::<Vec<_>>(),
        "no gaps, from first_seq"
    );
    assert!(events.iter().any(|e| matches!(e, EventBody::Result { data } if *data == json!({"vmid": 100, "name": "prod9"}))));
    let steps: Vec<_> = events
        .iter()
        .filter_map(|e| match e {
            EventBody::NextStep {
                operation, args, ..
            } => Some((operation.clone(), args.clone())),
            _ => None,
        })
        .collect();
    assert_eq!(steps.len(), 2);
    assert_eq!(steps[0].0.as_ref().unwrap().as_str(), "show_prod_vm_state");
    assert_eq!(steps[0].1["resource"], "prod9");
    assert!(
        steps[1].0.is_none() && steps[1].1.is_empty(),
        "unknown operations are dropped: {steps:?}"
    );
    let answered = events
        .iter()
        .filter(|e| matches!(e, EventBody::Answered { by, .. } if by == "bob@pve"))
        .count();
    assert_eq!(answered, 4);
    assert!(
        events
            .iter()
            .any(|e| matches!(e, EventBody::Plan { items, .. } if items.len() == 1))
    );
    let outputs: Vec<&str> = events
        .iter()
        .filter_map(|e| match e {
            EventBody::Output { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert!(outputs.contains(&"cores=4 hosts=mox1,mox3"), "{outputs:?}");
    assert!(matches!(events.last(), Some(EventBody::Completed { .. })));
    assert!(!f.sup.is_running(job));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn declining_a_confirmation_reports_cancelled() {
    let f = fixture();
    let (job, outcome) = f.start("interactive");
    let (group, _) = f.request(job).await;
    f.sup
        .answer(
            job,
            &group,
            &values(json!({"cores": 2, "hosts": ["mox1", "mox2"]})),
            "bob@pve",
        )
        .unwrap();
    let (confirm, _) = f.request(job).await;
    f.sup
        .answer(
            job,
            &confirm,
            &Answer::Confirm { confirmed: false },
            "bob@pve",
        )
        .unwrap();
    let done = finished(outcome).await;
    assert_eq!(done.state, JobState::Cancelled, "{done:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancelling_while_waiting_stops_cleanly() {
    let f = fixture();
    let (job, outcome) = f.start("interactive");
    f.request(job).await;
    f.sup.cancel(job, false).unwrap();
    let done = finished(outcome).await;
    assert_eq!(done.state, JobState::Cancelled, "{done:?}");
    assert_eq!(done.exit_code, Some(3));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_soft_cancel_respects_phases_that_must_not_be_interrupted() {
    let f = fixture_with(&[("MOCK_NO_CANCEL_SECONDS", "1.5")]);
    let (job, outcome) = f.start("long_running");
    let busy = f
        .wait_for(job, "the phase that must not be interrupted", |v| {
            v.phase.as_deref() == Some("Finalizing receive")
        })
        .await;
    assert!(!busy.cancel_allowed);
    let refused = f.sup.cancel(job, false).unwrap_err();
    assert_eq!(refused.code, ErrorCode::Conflict);
    assert!(refused.message.contains("Finalizing receive"));
    f.wait_for(job, "the step that may be interrupted", |v| {
        v.cancel_allowed && v.phase.as_deref().is_some_and(|p| p.starts_with("tick"))
    })
    .await;
    f.sup.cancel(job, false).unwrap();
    let done = finished(outcome).await;
    assert_eq!(done.state, JobState::Cancelled, "{done:?}");
    let events = f.events(job);
    assert!(
        events
            .iter()
            .any(|e| matches!(e, EventBody::Output { text, .. } if text.contains("cleaning up"))),
        "the script's own cleanup output survives the interrupt"
    );
    assert!(events.iter().any(|e| matches!(
        e,
        EventBody::Phase { id, status: PhaseStatus::Complete, .. } if id == "finalize"
    )));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_forced_cancel_stops_even_a_phase_that_forbids_interruption() {
    let f = fixture_with(&[("MOCK_NO_CANCEL_SECONDS", "30")]);
    let (job, outcome) = f.start("long_running");
    f.wait_for(job, "the phase", |v| !v.cancel_allowed).await;
    f.sup.cancel(job, true).unwrap();
    let done = finished(outcome).await;
    assert_eq!(done.state, JobState::Cancelled, "{done:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn secrets_are_redacted_everywhere() {
    let f = fixture();
    let (job, outcome) = f.start("secret");
    let (request, _) = f.request(job).await;
    f.sup
        .answer(
            job,
            &request,
            &values(json!({"key": "tskey-very-secret"})),
            "bob@pve",
        )
        .unwrap();
    let done = finished(outcome).await;
    assert_eq!(done.state, JobState::Failed);
    let sent: String = f
        .events(job)
        .iter()
        .map(|e| serde_json::to_string(e).unwrap())
        .collect();
    assert!(!sent.contains("tskey-very-secret"), "{sent}");
    assert!(sent.contains("the key is [redacted]"));
    assert!(sent.contains(r#""values":{"key":"[redacted]"}"#), "{sent}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn each_run_has_its_own_private_work_directory_that_is_kept() {
    let f = fixture_with(&[("BMAC_HOST", "mox2")]);
    let (job, outcome) = f.start("where");
    let done = finished(outcome).await;
    assert_eq!(done.state, JobState::Completed, "{done:?}");
    let work = f.root.parent().unwrap().join("work").join(job.to_string());
    let sent: String = f
        .events(job)
        .iter()
        .map(|e| serde_json::to_string(e).unwrap())
        .collect();
    assert!(
        sent.contains(&format!("host=mox2 work={} mode=700", work.display())),
        "{sent}"
    );
    assert_eq!(
        std::fs::read_to_string(work.join("artifact")).unwrap(),
        "artifact\n"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_native_operation_runs_the_native_program_under_the_same_rules() {
    let f = fixture();
    let job = JobId::random();
    let outcome = f
        .sup
        .start_native(job, "hosts.setup", &["--resume".to_string()], 2)
        .unwrap();
    let done = finished(outcome).await;
    assert_eq!(done.state, JobState::Completed, "{done:?}");
    let work = f.root.parent().unwrap().join("work").join(job.to_string());
    let sent: String = f
        .events(job)
        .iter()
        .map(|e| serde_json::to_string(e).unwrap())
        .collect();
    assert!(
        sent.contains(&format!(
            "operation=hosts.setup args=--resume work={}",
            work.display()
        )),
        "{sent}"
    );
    let unsafe_id = f.sup.start_native(JobId::random(), "../x", &[], 2);
    assert!(unsafe_id.is_err(), "operation ids can't name paths");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn output_without_the_protocol_fails_the_run() {
    let f = fixture();
    let (_, outcome) = f.start("no_protocol");
    let done = finished(outcome).await;
    assert_eq!(done.state, JobState::Failed);
    assert!(
        done.message
            .unwrap()
            .contains("did not begin with a bmac-ui protocol event")
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn exiting_without_completed_is_a_failure() {
    let f = fixture();
    let (_, outcome) = f.start("vanishes");
    let done = finished(outcome).await;
    assert_eq!(done.state, JobState::Failed, "{done:?}");
    assert!(done.message.unwrap().contains("without reporting"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn only_installed_regular_files_run() {
    let f = fixture();
    std::os::unix::fs::symlink(f.root.join("mock/vanishes.sh"), f.root.join("mock/link.sh"))
        .unwrap();
    for script in ["mock/link.sh", "mock/missing.sh", "../escape.sh", "/bin/sh"] {
        let error = f.sup.start(JobId::random(), script, &[], 1).unwrap_err();
        assert!(
            matches!(error.code, ErrorCode::Unavailable | ErrorCode::Internal),
            "{script}: {error:?}"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_script_starts_only_after_its_settings_are_written() {
    let calls = Arc::new(Mutex::new(0));
    let fail = Arc::new(Mutex::new(true));
    let (c, x) = (calls.clone(), fail.clone());
    let f = fixture_prepared(
        &[],
        Some(Arc::new(move || {
            *c.lock().unwrap() += 1;
            match *x.lock().unwrap() {
                true => Err("cannot write /run/bmac/config: disk full".into()),
                false => Ok(()),
            }
        })),
    );
    let job = JobId::random();
    let error = f.sup.start(job, "mock/vanishes.sh", &[], 1).unwrap_err();
    assert_eq!(error.code, ErrorCode::Unavailable);
    assert!(error.message.contains("disk full"));
    assert!(!f.sup.is_running(job), "nothing ran");
    assert!(f.events(job).is_empty());

    *fail.lock().unwrap() = false;
    let (_, outcome) = f.start("vanishes");
    assert_eq!(finished(outcome).await.state, JobState::Failed);
    assert_eq!(*calls.lock().unwrap(), 2, "once per run");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_endless_line_stops_the_run_without_filling_memory() {
    let f = fixture();
    let script = f.root.join("mock/endless.sh");
    std::fs::write(
        &script,
        "#!/usr/bin/env bash\nhead -c 20000000 /dev/zero | tr '\\0' x\n",
    )
    .unwrap();
    let (_, outcome) = f.start("endless");
    let done = finished(outcome).await;
    assert_eq!(done.state, JobState::Failed, "{done:?}");
    assert!(done.message.unwrap().contains("longer than"));
}
