//! End-to-end supervisor tests: real Bash mock scripts running through the
//! repository's real scripts/utilities/ui_json_run.sh and scripts/lib/ui_protocol.sh.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bmac_engine::history::{History, RunEvent, RunRecord, RunStatus};
use bmac_engine::platform::PlatformInfo;
use bmac_engine::protocol::{PhaseStatus, ResponsePayload, WorkflowEvent};
use bmac_engine::registry::Registry;
use bmac_engine::supervisor::{EventSink, Supervisor};
use serde_json::{json, Value};

#[derive(Default)]
struct Collect {
    events: Mutex<Vec<(String, RunEvent)>>,
    records: Mutex<Vec<RunRecord>>,
}

impl EventSink for Collect {
    fn events(&self, run_id: &str, events: Vec<RunEvent>) {
        let mut all = self.events.lock().unwrap();
        for event in events {
            all.push((run_id.to_string(), event));
        }
    }
    fn updated(&self, record: &RunRecord) {
        self.records.lock().unwrap().push(record.clone());
    }
}

const REGISTRY: &str = r#"{
  "schema_version": 1,
  "workflows": [
    {"id":"interactive","title":"Interactive","summary":"s","description":"d","category":"diagnostics",
     "script":"mock/interactive.sh","mode":"mutating","destructive":false,"platforms":["linux","macos"],"params":[]},
    {"id":"parallel","title":"Parallel","summary":"s","description":"d","category":"hosts",
     "script":"mock/interactive.sh","mode":"mutating","destructive":false,"concurrent":true,"platforms":["linux","macos"],"params":[]},
    {"id":"long_running","title":"Long","summary":"s","description":"d","category":"diagnostics",
     "script":"mock/long_running.sh","mode":"read_only","destructive":false,"platforms":["linux","macos"],"params":[]},
    {"id":"secret","title":"Secret","summary":"s","description":"d","category":"diagnostics",
     "script":"mock/secret.sh","mode":"read_only","destructive":false,"platforms":["linux","macos"],"params":[]},
    {"id":"no_protocol","title":"No protocol","summary":"s","description":"d","category":"diagnostics",
     "script":"mock/no_protocol.sh","mode":"read_only","destructive":false,"platforms":["linux","macos"],"params":[]},
    {"id":"vanishes","title":"Vanishes","summary":"s","description":"d","category":"diagnostics",
     "script":"mock/vanishes.sh","mode":"read_only","destructive":false,"platforms":["linux","macos"],"params":[]},
    {"id":"show_prod_vm_state","title":"Inspect","summary":"s","description":"d","category":"diagnostics",
     "script":"mock/vanishes.sh","mode":"read_only","destructive":false,"platforms":["linux","macos"],
     "params":[{"id":"resource","label":"VM","type":"production"}]}
  ]
}"#;

struct Fixture {
    _dir: tempfile::TempDir,
    root: PathBuf,
    sink: Arc<Collect>,
    sup: Arc<Supervisor>,
    platform: PlatformInfo,
}

fn fixture() -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("repo");
    let real = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    for sub in [
        "scripts/lib",
        "scripts/utilities",
        "scripts/host_runtime",
        "scripts/user_callable/diagnostics",
        "scripts/user_callable/guests/prod",
        "scripts/user_callable/hosts",
        "mock",
    ] {
        std::fs::create_dir_all(root.join(sub)).unwrap();
    }
    for file in ["scripts/lib/ui_protocol.sh", "scripts/utilities/ui_json_run.sh", "scripts/lib/ui_protocol.py"] {
        std::fs::copy(real.join(file), root.join(file)).unwrap();
    }
    for marker in ["scripts/lib/config.sh", "scripts/host_runtime/cluster_registry.py", "scripts/user_callable/diagnostics/list_hosts.sh", "scripts/user_callable/guests/prod/add_prod_vm.sh", "scripts/user_callable/hosts/add_proxmox_host.sh"] {
        std::fs::write(root.join(marker), "").unwrap();
    }
    let mocks = Path::new(env!("CARGO_MANIFEST_DIR")).join("../mock-scripts");
    for entry in std::fs::read_dir(mocks).unwrap() {
        let entry = entry.unwrap();
        std::fs::copy(entry.path(), root.join("mock").join(entry.file_name())).unwrap();
    }
    let sink = Arc::new(Collect::default());
    let history = History::open(dir.path().join("history")).unwrap();
    let sup = Supervisor::new(
        Registry::from_json(REGISTRY).unwrap(),
        history,
        sink.clone(),
        tokio::runtime::Handle::current(),
    );
    Fixture { _dir: dir, root, sink, sup, platform: PlatformInfo::detect() }
}

impl Fixture {
    fn start(&self, id: &str) -> String {
        self.sup.start(&self.root, &self.platform, id, &BTreeMap::new()).unwrap().run_id
    }

    async fn wait_for<F: Fn(&RunRecord) -> bool>(&self, run_id: &str, what: &str, cond: F) -> RunRecord {
        for _ in 0..300 {
            if let Ok(details) = self.sup.details(run_id) {
                if cond(&details.record) {
                    return details.record;
                }
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        let details = self.sup.details(run_id).unwrap();
        panic!("timed out waiting for {what}: {:#?}", details.record);
    }

    async fn finished(&self, run_id: &str) -> RunRecord {
        self.wait_for(run_id, "the run to finish", |r| r.status.is_terminal()).await
    }

    fn events(&self, run_id: &str) -> Vec<WorkflowEvent> {
        self.sink.events.lock().unwrap().iter().filter(|(id, _)| id == run_id).map(|(_, e)| e.event.clone()).collect()
    }
}

fn values(v: Value) -> ResponsePayload {
    ResponsePayload::Values { values: serde_json::from_value(v).unwrap() }
}

fn request_id(record: &RunRecord) -> String {
    record.pending_request.as_ref().unwrap().request_id().unwrap().to_string()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn interactive_workflow_round_trip() {
    let f = fixture();
    let run = f.start("interactive");

    let waiting = f.wait_for(&run, "the input group", |r| r.status == RunStatus::WaitingInput).await;
    assert!(matches!(waiting.pending_request, Some(WorkflowEvent::InputGroup { .. })));
    let group = request_id(&waiting);

    // The engine rejects a response that the request does not allow ...
    assert!(f.sup.respond(&run, &group, values(json!({"cores": 3, "hosts": ["mox1"]}))).is_err());
    // ... and the script rejects one that only it can judge.
    f.sup.respond(&run, &group, values(json!({"cores": 3, "hosts": ["mox1", "mox2"]}))).unwrap();
    let again = f.wait_for(&run, "re-request after validation_error", |r| r.pending_request.is_some()).await;
    assert_eq!(request_id(&again), group);
    assert!(f.events(&run).iter().any(|e| matches!(e, WorkflowEvent::ValidationError { field_errors, .. } if field_errors.contains_key("cores"))));
    f.sup.respond(&run, &group, values(json!({"cores": 4, "hosts": ["mox1", "mox3"]}))).unwrap();

    let confirm = f.wait_for(&run, "the confirmation", |r| r.status == RunStatus::WaitingConfirmation).await;
    let Some(WorkflowEvent::Confirm { confirmation_text, .. }) = &confirm.pending_request else { panic!() };
    assert_eq!(confirmation_text.as_deref(), Some("GO"));
    f.sup.respond(&run, &request_id(&confirm), ResponsePayload::Confirm { confirmed: true }).unwrap();

    let manual = f.wait_for(&run, "the manual action", |r| r.status == RunStatus::WaitingManualAction).await;
    f.sup.respond(&run, &request_id(&manual), ResponsePayload::Acknowledge).unwrap();

    let done = f.finished(&run).await;
    assert_eq!(done.status, RunStatus::Succeeded, "{done:#?}");
    assert_eq!(done.result, Some(json!({"vmid": 100, "name": "prod9"})));
    assert_eq!(done.next_steps.len(), 2);
    assert_eq!(done.next_steps[0].workflow.as_deref(), Some("show_prod_vm_state"));
    assert_eq!(done.next_steps[0].args.as_ref().unwrap()["resource"], "prod9");
    assert_eq!(done.next_steps[1].workflow, None, "unknown workflows are dropped");
    assert!(done.phases.iter().all(|p| p.status == PhaseStatus::Complete), "{:?}", done.phases);
    assert_eq!(done.phases.len(), 4);
    assert_eq!(done.responses.len(), 4);
    assert!(f.events(&run).iter().any(|e| matches!(e, WorkflowEvent::Plan { items, .. } if items.len() == 1)));
    let logs: Vec<String> = f
        .events(&run)
        .into_iter()
        .filter_map(|e| match e {
            WorkflowEvent::Log { text, .. } => Some(text),
            _ => None,
        })
        .collect();
    assert!(logs.contains(&"cores=4 hosts=mox1,mox3".to_string()), "{logs:?}");

    // History has the same run, with its events and logs.
    let stored = f.sup.history().details(&run).unwrap();
    assert_eq!(stored.record.status, RunStatus::Succeeded);
    assert!(stored.logs.iter().any(|l| matches!(&l.event, WorkflowEvent::Log { text, .. } if text == "done")));
    assert!(stored.events.iter().any(|e| matches!(e.event, WorkflowEvent::Completed { .. })));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn declining_a_confirmation_reports_cancelled() {
    let f = fixture();
    let run = f.start("interactive");
    let waiting = f.wait_for(&run, "the input group", |r| r.status == RunStatus::WaitingInput).await;
    f.sup.respond(&run, &request_id(&waiting), values(json!({"cores": 2, "hosts": ["mox1", "mox2"]}))).unwrap();
    let confirm = f.wait_for(&run, "the confirmation", |r| r.status == RunStatus::WaitingConfirmation).await;
    f.sup.respond(&run, &request_id(&confirm), ResponsePayload::Confirm { confirmed: false }).unwrap();
    let done = f.finished(&run).await;
    assert_eq!(done.status, RunStatus::Cancelled, "{done:#?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancelling_while_waiting_stops_cleanly() {
    let f = fixture();
    let run = f.start("interactive");
    f.wait_for(&run, "the input group", |r| r.status == RunStatus::WaitingInput).await;
    f.sup.cancel(&run, false).unwrap();
    let done = f.finished(&run).await;
    assert_eq!(done.status, RunStatus::Cancelled, "{done:#?}");
    assert_eq!(done.exit_code, Some(3));
}

// The GUI thread that runs synchronous Tauri commands has no Tokio context.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn starting_and_cancelling_work_outside_a_tokio_context() {
    let f = fixture();
    let (sup, root, platform) = (Arc::clone(&f.sup), f.root.clone(), f.platform.clone());
    let run = std::thread::spawn(move || sup.start(&root, &platform, "interactive", &BTreeMap::new()))
        .join()
        .expect("start panicked outside a Tokio context")
        .unwrap()
        .run_id;
    f.wait_for(&run, "the input group", |r| r.status == RunStatus::WaitingInput).await;
    let (sup, id) = (Arc::clone(&f.sup), run.clone());
    std::thread::spawn(move || sup.cancel(&id, false))
        .join()
        .expect("cancel panicked outside a Tokio context")
        .unwrap();
    assert_eq!(f.finished(&run).await.status, RunStatus::Cancelled);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mutating_runs_are_serialized() {
    let f = fixture();
    let run = f.start("interactive");
    let second = f.sup.start(&f.root, &f.platform, "interactive", &BTreeMap::new());
    assert!(matches!(second, Err(bmac_engine::EngineError::Busy(_))));
    // Read-only diagnostics may still run alongside.
    let other = f.start("vanishes");
    f.finished(&other).await;
    f.sup.cancel(&run, false).unwrap();
    f.finished(&run).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn concurrent_workflows_run_alongside_only_themselves() {
    let f = fixture();
    let first = f.start("parallel");
    let second = f.start("parallel");
    assert!(matches!(
        f.sup.start(&f.root, &f.platform, "interactive", &BTreeMap::new()),
        Err(bmac_engine::EngineError::Busy(_))
    ));
    for run in [&first, &second] {
        f.sup.cancel(run, false).unwrap();
        f.finished(run).await;
    }

    let run = f.start("interactive");
    assert!(matches!(
        f.sup.start(&f.root, &f.platform, "parallel", &BTreeMap::new()),
        Err(bmac_engine::EngineError::Busy(_))
    ));
    f.sup.cancel(&run, false).unwrap();
    f.finished(&run).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sigint_cancellation_respects_non_cancellable_phases() {
    let f = fixture();
    std::env::set_var("MOCK_NO_CANCEL_SECONDS", "1.5");
    let run = f.start("long_running");
    let busy = f.wait_for(&run, "the non-cancellable phase", |r| !r.phases.is_empty()).await;
    assert!(!busy.cancel_allowed);
    assert!(matches!(f.sup.cancel(&run, false), Err(bmac_engine::EngineError::NotCancellable(_))));
    f.wait_for(&run, "the cancellable step", |r| r.cancel_allowed && r.phases.len() == 2).await;
    f.sup.cancel(&run, false).unwrap();
    let done = f.finished(&run).await;
    assert_eq!(done.status, RunStatus::Cancelled, "{done:#?}");
    let logs: Vec<_> = f.sup.history().details(&run).unwrap().logs;
    assert!(
        logs.iter().any(|l| matches!(&l.event, WorkflowEvent::Log { text, .. } if text.contains("cleaning up"))),
        "the script's own cleanup output survives the interrupt"
    );
    assert!(f.events(&run).iter().any(|e| matches!(e, WorkflowEvent::Completed { .. })));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn secrets_are_redacted_everywhere() {
    let f = fixture();
    let run = f.start("secret");
    let waiting = f.wait_for(&run, "the secret input", |r| r.pending_request.is_some()).await;
    f.sup.respond(&run, &request_id(&waiting), values(json!({"key": "tskey-very-secret"}))).unwrap();
    let done = f.finished(&run).await;
    assert_eq!(done.status, RunStatus::Failed);
    let sent: String = f.events(&run).iter().map(|e| serde_json::to_string(e).unwrap()).collect();
    assert!(!sent.contains("tskey-very-secret"), "{sent}");
    assert!(sent.contains("the key is [redacted]"));
    let dir = f.sup.history().dir().to_path_buf();
    for entry in std::fs::read_dir(dir).unwrap() {
        let text = std::fs::read_to_string(entry.unwrap().path()).unwrap();
        assert!(!text.contains("tskey-very-secret"));
    }
    assert_eq!(done.responses[0].values.as_ref().unwrap()["key"], json!("[redacted]"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn output_without_the_protocol_fails_the_run() {
    let f = fixture();
    let run = f.start("no_protocol");
    let done = f.finished(&run).await;
    assert_eq!(done.status, RunStatus::Failed);
    assert!(done.message.unwrap().contains("did not begin with a bmac-ui protocol event"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn exiting_without_completed_is_a_failure() {
    let f = fixture();
    let run = f.start("vanishes");
    let done = f.finished(&run).await;
    assert_eq!(done.status, RunStatus::Failed, "{done:#?}");
    assert!(done.message.unwrap().contains("without reporting"));
}
