// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

use std::sync::{Arc, Mutex};

use bmac_core::job::Field;
use bmac_proto::bmac_ui::parse_line;
use tokio::io::{AsyncWriteExt, DuplexStream};

use super::*;

#[derive(Clone, Default)]
pub(crate) struct Shared(Arc<Mutex<Vec<u8>>>);

impl Write for Shared {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Shared {
    /// Every line written so far, each checked as the supervisor checks it.
    pub(crate) fn events(&self) -> Vec<WorkflowEvent> {
        String::from_utf8(self.0.lock().unwrap().clone())
            .unwrap()
            .lines()
            .map(|line| parse_line(line).unwrap_or_else(|e| panic!("{line}: {e}")))
            .collect()
    }
}

pub(crate) fn workflow(work: &Path) -> (Workflow, Shared, DuplexStream) {
    let out = Shared::default();
    let (ours, theirs) = tokio::io::duplex(4096);
    let flow = Workflow::new(
        Box::new(out.clone()),
        tokio::io::BufReader::new(theirs),
        work.to_path_buf(),
    );
    (flow, out, ours)
}

pub(crate) async fn send(input: &mut DuplexStream, line: Value) {
    input
        .write_all(format!("{line}\n").as_bytes())
        .await
        .unwrap();
}

fn name_field() -> Field {
    serde_json::from_value(serde_json::json!({"id": "name", "type": "string", "label": "Name"}))
        .unwrap()
}

#[tokio::test]
async fn it_begins_with_the_protocol_header() {
    let dir = tempfile::tempdir().unwrap();
    let (flow, out, _input) = workflow(dir.path());
    flow.begin("hosts.setup", "Set up", "d");
    flow.phase("identity", "Verify identity", PhaseStatus::Running);
    let events = out.events();
    assert!(matches!(
        &events[0],
        WorkflowEvent::Protocol { protocol, version, .. }
            if protocol == PROTOCOL_NAME && *version == PROTOCOL_VERSION
    ));
    assert!(
        matches!(&events[1], WorkflowEvent::WorkflowStarted { workflow, .. } if workflow == "hosts.setup")
    );
    assert!(matches!(&events[3], WorkflowEvent::Phase { id, .. } if id == "identity"));
}

#[tokio::test]
async fn a_refused_answer_keeps_the_same_request_open() {
    let dir = tempfile::tempdir().unwrap();
    let (flow, out, mut input) = workflow(dir.path());
    let asking = tokio::spawn(async move {
        let values = flow
            .input_group("Name", None, vec![name_field()], |values| {
                if values["name"] == "ok" {
                    Ok(())
                } else {
                    Err(BTreeMap::from([("name".into(), "not ok".into())]))
                }
            })
            .await;
        (flow, values)
    });
    send(&mut input, json_response("r1", "bad")).await;
    send(&mut input, json_response("r1", "ok")).await;
    let (_flow, values) = asking.await.unwrap();
    assert_eq!(values.unwrap()["name"], "ok");
    let events = out.events();
    assert!(
        matches!(&events[0], WorkflowEvent::InputGroup { request_id, .. } if request_id == "r1")
    );
    assert!(matches!(
        &events[1],
        WorkflowEvent::ValidationError { request_id, field_errors, .. }
            if request_id == "r1" && field_errors["name"] == "not ok"
    ));
    assert_eq!(events.len(), 2);
}

fn json_response(id: &str, name: &str) -> Value {
    serde_json::json!({"type": "response", "request_id": id, "values": {"name": name}})
}

#[tokio::test]
async fn a_cancelled_answer_or_a_cancel_line_stops_the_wait() {
    let dir = tempfile::tempdir().unwrap();
    let (flow, _out, mut input) = workflow(dir.path());
    let asking = tokio::spawn(async move {
        flow.confirm("Go?", "m", Severity::Warning, None, Vec::new())
            .await
    });
    send(
        &mut input,
        serde_json::json!({"type": "response", "request_id": "r1", "cancelled": true}),
    )
    .await;
    assert_eq!(asking.await.unwrap(), Err(Stop::Cancelled));

    let (flow, _out, mut input) = workflow(dir.path());
    send(&mut input, serde_json::json!({"type": "cancel"})).await;
    flow.token().cancelled().await;
    assert_eq!(flow.check(), Err(Stop::Cancelled));
}

#[tokio::test]
async fn closed_input_cancels_rather_than_waiting_forever() {
    let dir = tempfile::tempdir().unwrap();
    let (flow, _out, input) = workflow(dir.path());
    drop(input);
    assert_eq!(
        flow.manual("Do it", Vec::new(), None).await,
        Err(Stop::Cancelled)
    );
}

#[tokio::test]
async fn answers_to_other_requests_are_ignored() {
    let dir = tempfile::tempdir().unwrap();
    let (flow, _out, mut input) = workflow(dir.path());
    let asking = tokio::spawn(async move { flow.manual("Do it", Vec::new(), None).await });
    send(
        &mut input,
        serde_json::json!({"type": "response", "request_id": "r9", "acknowledged": true}),
    )
    .await;
    send(
        &mut input,
        serde_json::json!({"type": "response", "request_id": "r1", "acknowledged": true}),
    )
    .await;
    assert_eq!(asking.await.unwrap(), Ok(()));
}

#[tokio::test]
async fn a_resumed_operation_never_reuses_a_request_id() {
    let dir = tempfile::tempdir().unwrap();
    let (flow, _out, mut input) = workflow(dir.path());
    let asking = tokio::spawn(async move { flow.manual("Before", Vec::new(), None).await });
    send(
        &mut input,
        serde_json::json!({"type": "response", "request_id": "r1", "acknowledged": true}),
    )
    .await;
    assert_eq!(asking.await.unwrap(), Ok(()));

    let (flow, out, mut input) = workflow(dir.path());
    let asking = tokio::spawn(async move { flow.manual("After", Vec::new(), None).await });
    send(
        &mut input,
        serde_json::json!({"type": "response", "request_id": "r2", "acknowledged": true}),
    )
    .await;
    assert_eq!(asking.await.unwrap(), Ok(()));
    assert!(
        matches!(&out.events()[0], WorkflowEvent::ManualAction { request_id, .. } if request_id == "r2")
    );
}

#[tokio::test]
async fn the_end_says_how_it_went() {
    let dir = tempfile::tempdir().unwrap();
    let (flow, out, _input) = workflow(dir.path());
    let code = flow.finish(&Err(Stop::Failed {
        message: "no disks".into(),
        details: Some("sda missing".into()),
    }));
    assert_eq!(code, ExitCode::from(1));
    let events = out.events();
    assert!(
        matches!(&events[0], WorkflowEvent::Error { message, details: Some(d), .. } if message == "no disks" && d == "sda missing")
    );
    assert!(matches!(
        &events[1],
        WorkflowEvent::Completed {
            status: CompletedStatus::Failed,
            exit_code: Some(1),
            ..
        }
    ));

    let (flow, out, _input) = workflow(dir.path());
    assert_eq!(flow.finish(&Ok(())), ExitCode::SUCCESS);
    assert!(matches!(
        &out.events()[0],
        WorkflowEvent::Completed {
            status: CompletedStatus::Success,
            ..
        }
    ));
}

#[test]
fn checkpoints_survive_a_restart() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(CHECKPOINTS);
    let mut done = Checkpoints::load(path.clone());
    assert!(!done.is_done("identity"));
    done.record("identity", serde_json::json!({"layout": 1}))
        .unwrap();
    let again = Checkpoints::load(path);
    assert!(again.is_done("identity"));
    assert_eq!(again.get("identity").unwrap()["layout"], 1);
    assert!(!dir.path().join("checkpoints.new").exists());
}

#[tokio::test]
async fn a_restart_the_job_asked_for_is_seen_once() {
    let dir = tempfile::tempdir().unwrap();
    let (flow, _, _input) = workflow(dir.path());
    assert_eq!(flow.take_restart("b1").unwrap(), None);
    flow.expect_restart("b1").unwrap();
    assert_eq!(
        flow.take_restart("b1").unwrap(),
        Some(false),
        "not booted yet"
    );
    assert_eq!(flow.take_restart("b2").unwrap(), Some(true));
    assert_eq!(
        flow.take_restart("b2").unwrap(),
        None,
        "forgotten once seen"
    );
}

#[test]
fn a_job_that_restarts_bmacd_resumes_only_under_a_new_bmacd() {
    let dir = tempfile::tempdir().unwrap();
    assert!(!service_restarted_since_asked(dir.path(), 42));
    std::fs::write(dir.path().join(SERVICE_RESTART_MARKER), "41\n").unwrap();
    assert!(service_restarted_since_asked(dir.path(), 42));
    assert!(!service_restarted_since_asked(dir.path(), 41));
    std::fs::write(dir.path().join(SERVICE_RESTART_MARKER), "garbage").unwrap();
    assert!(!service_restarted_since_asked(dir.path(), 42));
}
