// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

use bmac_core::job::PhaseStatus;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use super::super::tests::{send, workflow};
use super::*;

fn line(value: Value) -> String {
    format!("{value}\n")
}

/// The new host's output, as it would print it before closing.
async fn remote(output: String) -> tokio::io::DuplexStream {
    let (mut theirs, ours) = tokio::io::duplex(65536);
    theirs.write_all(output.as_bytes()).await.unwrap();
    ours
}

#[test]
fn the_launched_job_gets_the_host_it_was_started_for() {
    let values = BTreeMap::from([("host".to_string(), Value::from("mox2"))]);
    let prepared = crate::operations::prepare(&operation(), &values, false).unwrap();
    assert_eq!(parse_args(&prepared.args).unwrap().to_string(), "mox2");
}

#[tokio::test]
async fn phases_and_messages_pass_through_and_the_run_ends_completed() {
    let dir = tempfile::tempdir().unwrap();
    let (flow, out, _input) = workflow(dir.path());
    let output = [
        json!({"type":"protocol","protocol":"bmac-ui","version":2}),
        json!({"type":"phase","id":"storage","label":"Storage","status":"running"}),
        json!({"type":"info","message":"Built the pool."}),
        json!({"type":"phase","id":"storage","label":"Storage","status":"complete"}),
        json!({"type":"completed","status":"success"}),
    ]
    .into_iter()
    .map(line)
    .collect::<String>();
    let lines = remote(output).await;
    let mut answers = Vec::new();
    let end = relay(&flow, BufReader::new(lines), &mut answers).await;
    assert_eq!(end, RunEnd::Completed(CompletedStatus::Success, None, None));
    let events = out.events();
    assert!(events.iter().any(|e| matches!(e,
        WorkflowEvent::Phase { id, status: PhaseStatus::Complete, .. } if id == "storage")));
    assert!(events.iter().any(|e| matches!(e,
        WorkflowEvent::Info { message } if message == "Built the pool.")));
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, WorkflowEvent::Protocol { .. }))
    );
}

#[tokio::test]
async fn a_run_that_closes_without_completing_has_dropped() {
    let dir = tempfile::tempdir().unwrap();
    let (flow, _out, _input) = workflow(dir.path());
    let output =
        line(json!({"type":"phase","id":"drills","label":"Boot drills","status":"running"}));
    let lines = remote(output).await;
    let mut answers = Vec::new();
    assert_eq!(
        relay(&flow, BufReader::new(lines), &mut answers).await,
        RunEnd::Dropped
    );
}

#[tokio::test]
async fn a_failed_run_reports_its_error() {
    let dir = tempfile::tempdir().unwrap();
    let (flow, _out, _input) = workflow(dir.path());
    let output = [
        json!({"type":"error","message":"pvecm add failed","details":"401","recoverable":false}),
        json!({"type":"completed","status":"failed","message":"pvecm add failed"}),
    ]
    .into_iter()
    .map(line)
    .collect::<String>();
    let lines = remote(output).await;
    let mut answers = Vec::new();
    assert_eq!(
        relay(&flow, BufReader::new(lines), &mut answers).await,
        RunEnd::Completed(
            CompletedStatus::Failed,
            Some("pvecm add failed".into()),
            Some("401".into())
        )
    );
}

#[tokio::test]
async fn a_question_is_asked_here_and_answered_under_the_remote_id() {
    let dir = tempfile::tempdir().unwrap();
    let (flow, out, mut input) = workflow(dir.path());
    let output = [
        json!({"type":"input_group","request_id":"r7","title":"Join the cluster c1",
               "fields":[{"id":"password","type":"password","label":"Password","required":true}]}),
        json!({"type":"completed","status":"success"}),
    ]
    .into_iter()
    .map(line)
    .collect::<String>();
    let lines = remote(output).await;
    let (mut answers, mut read_back) = tokio::io::duplex(65536);
    let relaying = async {
        let end = relay(&flow, BufReader::new(lines), &mut answers).await;
        drop(answers);
        end
    };
    let answering = async {
        loop {
            let asked = out.events().into_iter().find_map(|e| match e {
                WorkflowEvent::InputGroup { request_id, .. } => Some(request_id),
                _ => None,
            });
            if let Some(id) = asked {
                send(
                    &mut input,
                    json!({"type":"response","request_id":id,"values":{"password":"pw"}}),
                )
                .await;
                break;
            }
            tokio::task::yield_now().await;
        }
    };
    let (end, ()) = tokio::join!(relaying, answering);
    assert_eq!(end, RunEnd::Completed(CompletedStatus::Success, None, None));
    let mut sent = String::new();
    read_back.read_to_string(&mut sent).await.unwrap();
    let sent: Value = serde_json::from_str(sent.trim()).unwrap();
    assert_eq!(
        sent,
        json!({"type":"response","request_id":"r7","values":{"password":"pw"}})
    );
}

#[tokio::test]
async fn a_cancel_here_is_passed_on() {
    let dir = tempfile::tempdir().unwrap();
    let (flow, _out, mut input) = workflow(dir.path());
    let (mut theirs, ours) = tokio::io::duplex(65536);
    let (mut answers, mut read_back) = tokio::io::duplex(65536);
    send(&mut input, json!({"type":"cancel"})).await;
    let relaying = async {
        let end = relay(&flow, BufReader::new(ours), &mut answers).await;
        drop(answers);
        end
    };
    let remote = async {
        let mut got = [0u8; 64];
        let n = read_back.read(&mut got).await.unwrap();
        theirs
            .write_all(
                line(json!({"type":"completed","status":"cancelled","message":"Cancelled."}))
                    .as_bytes(),
            )
            .await
            .unwrap();
        String::from_utf8_lossy(&got[..n]).to_string()
    };
    let (end, sent) = tokio::join!(relaying, remote);
    assert_eq!(sent, "{\"type\":\"cancel\"}\n");
    assert!(matches!(
        end,
        RunEnd::Completed(CompletedStatus::Cancelled, ..)
    ));
}

#[test]
fn ssh_trusts_only_the_pinned_key_and_the_given_identity() {
    let route = Route {
        key: "/w/setup.key".into(),
        known_hosts: "/w/known_hosts".into(),
        address: "100.64.0.9".into(),
    };
    let args = route.ssh_args("mox2".parse().unwrap()).join(" ");
    for wanted in [
        "-F /dev/null",
        "-i /w/setup.key",
        "IdentitiesOnly=yes",
        "UserKnownHostsFile=/w/known_hosts",
        "GlobalKnownHostsFile=/dev/null",
        "StrictHostKeyChecking=yes",
        "HostKeyAlias=mox2",
        "BatchMode=yes",
    ] {
        assert!(args.contains(wanted), "{wanted} in {args}");
    }
    assert!(args.ends_with("100.64.0.9 --"));
}

#[test]
fn quorum_devices_and_offline_members_are_found() {
    let with = "quorum {\n  provider: corosync_votequorum\n  device {\n    model: net\n  }\n}\n";
    assert!(has_qdevice(with));
    assert!(!has_qdevice(
        "quorum {\n  provider: corosync_votequorum\n}\n"
    ));
    let members = r#"{"nodename":"mox1","version":3,"cluster":{"name":"c1","version":2,"nodes":2,"quorate":1},
        "nodelist":{"mox1":{"id":1,"online":1,"ip":"10.0.0.1"},"mox2":{"id":2,"online":0,"ip":"10.0.0.2"}}}"#;
    assert_eq!(offline_members(members).unwrap(), vec!["mox2".to_string()]);
    assert!(offline_members("{}").is_err());
}

#[test]
fn it_takes_one_host() {
    assert_eq!(
        parse_args(&["mox3".into()]).unwrap(),
        "mox3".parse().unwrap()
    );
    assert!(parse_args(&[]).is_err());
    assert!(parse_args(&["nope".into()]).is_err());
    assert!(parse_args(&["mox3".into(), "mox4".into()]).is_err());
}

#[test]
fn the_operation_is_valid() {
    let op = operation();
    assert_eq!(op.id.as_str(), OPERATION);
    assert_eq!(op.locks, vec!["cluster-membership".to_string()]);
}
