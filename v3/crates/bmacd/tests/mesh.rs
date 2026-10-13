// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

//! Host-to-host links over real TLS on loopback, between the simulated
//! hosts of [`harness::Lab`].

mod harness;

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use bmac_core::JobId;
use bmac_core::job::{Answer, EventBody, JobEvent, JobState, LogLevel};
use bmac_proto::api::{ApiError, ErrorCode, LaunchJob, Role};
use bmac_proto::stream::StreamBody;
use bmacd::db::Database;
use bmacd::engine::events::{Next, Subscription};
use bmacd::jobs::{Caller, JobRecord, Jobs};
use bmacd::mesh::tls::{join_client, join_client_pinned};
use bmacd::mesh::tokens::TokenKind;
use bmacd::runner::{AnswerRelay, RelayedAnswer};
use bmacd::store::{Entry, Store};
use bytes::Bytes;
use harness::{Ca, Lab, host};
use http_body_util::{BodyExt, Full};
use hyper_util::rt::TokioIo;
use rustls::ClientConfig;
use rustls::pki_types::ServerName;
use serde_json::{Value, json};
use tokio::net::TcpStream;

fn output(job: JobId, seq: u64, text: &str) -> Entry {
    Entry::job_event(&JobEvent {
        job,
        seq,
        at_ms: 1_000 + seq as i64,
        body: EventBody::Output {
            stream: None,
            level: LogLevel::Info,
            text: text.into(),
            source: None,
        },
    })
}

async fn eventually<F: std::future::Future<Output = bool>>(
    what: &str,
    mut cond: impl FnMut() -> F,
) {
    for _ in 0..200 {
        if cond().await {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("timed out waiting for {what}");
}

/// Reads `tab` until a message matches.
async fn until(tab: &mut Subscription, what: &str, matches: impl Fn(&StreamBody) -> bool) {
    let wait = async {
        loop {
            match tab.next().await {
                Next::Message(message) if matches(&message.body) => return,
                Next::Closed => panic!("the hub closed"),
                _ => {}
            }
        }
    };
    tokio::time::timeout(Duration::from_secs(10), wait)
        .await
        .unwrap_or_else(|_| panic!("timed out waiting for {what}"));
}

/// Sends one request with `tls` and returns the status and body, or the
/// connection error.
async fn call(
    tls: Arc<ClientConfig>,
    to: SocketAddr,
    name: &str,
    method: &str,
    path: &str,
    body: Value,
) -> Result<(u16, Value), String> {
    let tcp = TcpStream::connect(to).await.map_err(|e| e.to_string())?;
    let tls = tokio_rustls::TlsConnector::from(tls)
        .connect(ServerName::try_from(name.to_string()).unwrap(), tcp)
        .await
        .map_err(|e| e.to_string())?;
    let (mut sender, connection) = hyper::client::conn::http1::handshake(TokioIo::new(tls))
        .await
        .map_err(|e| e.to_string())?;
    tokio::spawn(connection);
    let request = hyper::Request::builder()
        .method(method)
        .uri(path)
        .header("host", name)
        .header("content-type", "application/json")
        .body(Full::new(Bytes::from(body.to_string())))
        .unwrap();
    let response = sender
        .send_request(request)
        .await
        .map_err(|e| e.to_string())?;
    let status = response.status().as_u16();
    let bytes = response
        .into_body()
        .collect()
        .await
        .map_err(|e| e.to_string())?
        .to_bytes();
    Ok((
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    ))
}

fn code(body: &Value) -> ErrorCode {
    serde_json::from_value::<ApiError>(body.clone())
        .unwrap()
        .code
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn records_reach_every_member_and_resume_after_a_drop_or_restart() {
    let lab = Lab::new(&[1, 2]);
    let mox1 = lab.node(1).await;
    let mut mox2 = lab.node(2).await;
    let job = JobId::random();
    let first: Vec<Entry> = (1..=1200)
        .map(|s| output(job, s, &format!("line {s}")))
        .collect();
    mox1.store.append(first).await.unwrap();

    let mut tab = mox2.hub.subscribe("t", None);
    mox2.hub.watch("t", &tab.stream, vec![job]).unwrap();
    mox2.link();
    let old_stream = mox1.store.stream().to_string();
    eventually("the first records to arrive", || async {
        mox2.store.last_seq("mox1", &old_stream).await.unwrap() == 1200
    })
    .await;
    let page = mox2.store.job_events(job, 1, 2000).await.unwrap();
    let seqs: Vec<u64> = page.events.iter().map(|e| e.seq).collect();
    assert_eq!(seqs, (1..=1200).collect::<Vec<_>>());

    mox1.store
        .append(vec![output(job, 1201, "live")])
        .await
        .unwrap();
    until(
        &mut tab,
        "a live job event",
        |b| matches!(b, StreamBody::JobEvent { event } if event.seq == 1201),
    )
    .await;
    mox1.hub.publish(StreamBody::Observation {
        host: host(1),
        collector: "zpool".into(),
        data: json!({"health": "ONLINE"}),
    });
    until(&mut tab, "the observation", |b| {
        matches!(b, StreamBody::Observation { host: h, collector, .. } if *h == host(1) && collector == "zpool")
    })
    .await;

    // mox1 goes away and restarts twice; what each incarnation wrote
    // arrives once it is back, in order.
    mox1.store
        .append(vec![output(job, 1202, "while away")])
        .await
        .unwrap();
    drop(mox1);
    until(&mut tab, "mox1 unreachable", |b| {
        matches!(
            b,
            StreamBody::Host {
                reachable: false,
                ..
            }
        )
    })
    .await;
    let other = JobId::random();
    {
        let store = Store::open(Database::open(&lab.db_path(1)).unwrap(), "mox1", 0)
            .await
            .unwrap();
        store
            .append(vec![output(job, 1203, "also while away")])
            .await
            .unwrap();
    }
    let mox1 = lab.node(1).await;
    until(
        &mut tab,
        "mox1 reachable again",
        |b| matches!(b, StreamBody::Host { host: h, reachable: true, .. } if *h == host(1)),
    )
    .await;
    mox1.store
        .append(vec![output(other, 1, "after the restart")])
        .await
        .unwrap();
    let new_stream = mox1.store.stream().to_string();
    eventually("the new incarnation's records", || async {
        mox2.store.last_seq("mox1", &new_stream).await.unwrap() == 1
    })
    .await;
    assert_eq!(
        mox2.store.last_seq("mox1", &old_stream).await.unwrap(),
        1202
    );
    let page = mox2.store.job_events(job, 1190, 100).await.unwrap();
    let seqs: Vec<u64> = page.events.iter().map(|e| e.seq).collect();
    assert_eq!(seqs, (1190..=1203).collect::<Vec<_>>());
    assert_eq!(
        mox2.store
            .job_events(other, 1, 10)
            .await
            .unwrap()
            .events
            .len(),
        1
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn only_members_get_anything_but_joins_and_enrollments() {
    let lab = Lab::new(&[1, 2]);
    let mox1 = lab.node(1).await;
    let to = lab.address(1);
    let get = |tls: Arc<ClientConfig>, path: &'static str| {
        call(tls, to, "mox1", "GET", path, Value::Null)
    };

    let member = lab.ca.node("mox2").client;
    let (status, _) = call(member.clone(), to, "mox1", "GET", "/nope", Value::Null)
        .await
        .unwrap();
    assert_eq!(status, 404);

    let outsider = lab.ca.node("mox9").client;
    let anonymous = join_client(lab.ca.pem.as_bytes()).unwrap();
    for tls in [outsider, anonymous.clone()] {
        for path in ["/v1/stream", "/nope"] {
            let (status, body) = get(tls.clone(), path).await.unwrap();
            assert_eq!((status, code(&body)), (403, ErrorCode::Forbidden), "{path}");
        }
        let (status, _) = call(tls.clone(), to, "mox1", "POST", "/v1/answer", json!({}))
            .await
            .unwrap();
        assert_eq!(status, 403);
    }

    let foreign = Ca::new("another CA").node("mox2").client;
    assert!(
        get(foreign, "/v1/stream").await.is_err(),
        "a certificate from another CA is refused in the handshake"
    );
    assert!(
        get(anonymous, "/v1/stream")
            .await
            .is_ok_and(|(s, _)| s == 403),
        "the server is still serving"
    );
    drop(mox1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_join_token_works_once_and_only_until_it_expires() {
    let lab = Lab::new(&[1, 2]);
    let mox1 = lab.node(1).await;
    let to = lab.address(1);
    let anonymous = join_client(lab.ca.pem.as_bytes()).unwrap();
    let tokens = mox1.mesh.tokens().clone();
    let join = |token: &str| {
        json!({
            "token": token,
            "host": "mox3",
            "ssh_host_key": "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIOmM test",
            "https_fingerprint": vec!["AB"; 32].join(":"),
        })
    };
    let token = tokens
        .issue(TokenKind::Join, Some(host(3)), 60_000, "root@pam")
        .unwrap();
    let expired = tokens.issue(TokenKind::Join, None, 0, "root@pam").unwrap();
    let enroll = tokens
        .issue(TokenKind::Enroll, None, 60_000, "root@pam")
        .unwrap();
    let wrong_slot = tokens
        .issue(TokenKind::Join, Some(host(4)), 60_000, "root@pam")
        .unwrap();

    let (status, body) = call(
        anonymous.clone(),
        to,
        "mox1",
        "POST",
        "/v1/join",
        join(&token),
    )
    .await
    .unwrap();
    assert_eq!(status, 200, "{body}");
    let id = body["request_id"].as_str().unwrap().to_string();

    for (what, attempt) in [
        ("reused", token.clone()),
        ("expired", expired),
        ("for an enrollment", enroll),
        ("for another slot", wrong_slot),
        ("unknown", "0".repeat(64)),
        ("malformed", "nope".into()),
    ] {
        let (status, body) = call(
            anonymous.clone(),
            to,
            "mox1",
            "POST",
            "/v1/join",
            join(&attempt),
        )
        .await
        .unwrap();
        assert_eq!(
            (status, code(&body)),
            (401, ErrorCode::Unauthenticated),
            "{what}"
        );
        assert_eq!(
            body["message"],
            "the token is not valid; ask an administrator for a new one"
        );
    }
    let pending = tokens.pending().unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].id, id);
    assert_eq!(pending[0].host, Some(host(3)));
    assert_eq!(pending[0].address, "127.0.0.1");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_joining_host_needs_only_the_cluster_ca_fingerprint() {
    let lab = Lab::new(&[1]);
    let mox1 = lab.node(1).await;
    let to = lab.address(1);
    let fingerprint = bmacd::web::tls::pem_fingerprint(lab.ca.pem.as_bytes()).unwrap();
    let token = mox1
        .mesh
        .tokens()
        .issue(TokenKind::Join, Some(host(2)), 60_000, "root@pam")
        .unwrap();
    let body = json!({
        "token": token, "host": "mox2",
        "ssh_host_key": "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIOmM test",
        "https_fingerprint": vec!["AB"; 32].join(":"),
    });
    let pinned = join_client_pinned(&fingerprint.to_ascii_lowercase()).unwrap();
    let (status, answer) = call(pinned.clone(), to, "mox1", "POST", "/v1/join", body.clone())
        .await
        .unwrap();
    assert_eq!(status, 200, "{answer}");
    assert!(
        call(pinned, to, "mox9", "POST", "/v1/join", body.clone())
            .await
            .is_err(),
        "the node certificate must name the host dialed"
    );
    let other = bmacd::web::tls::pem_fingerprint(Ca::new("another CA").pem.as_bytes()).unwrap();
    let error = call(
        join_client_pinned(&other).unwrap(),
        to,
        "mox1",
        "POST",
        "/v1/join",
        body,
    )
    .await
    .unwrap_err();
    assert!(error.contains("fingerprint"), "{error}");
    drop(mox1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_joining_host_presents_its_token_once() {
    use bmac_proto::api::JoinState;
    use bmac_proto::bootstrap::{BundleHost, JOIN_BUNDLE_VERSION, JoinBundle};
    let lab = Lab::new(&[1]);
    let mox1 = lab.node(1).await;
    let port = lab.address(1).port();
    let token = mox1
        .mesh
        .tokens()
        .issue(TokenKind::Join, Some(host(2)), 60_000, "root@pam")
        .unwrap();
    let mut bundle = JoinBundle {
        version: JOIN_BUNDLE_VERSION,
        cluster_uuid: "c0ffee".into(),
        cluster_name: "lab".into(),
        cluster_settings: Default::default(),
        slot: host(2),
        host_settings: Default::default(),
        join_token: token,
        expires_ms: i64::MAX,
        hosts: vec![BundleHost {
            host: host(1),
            dashboard_url: "https://127.0.0.1:8443".into(),
            https_fingerprint: vec!["CD"; 32].join(":"),
            private_ip: "10.213.0.11".into(),
        }],
        proxmox_ca_fingerprint: bmacd::web::tls::pem_fingerprint(lab.ca.pem.as_bytes()).unwrap(),
        setup_ssh_public_key: "ssh-ed25519 AAAA setup".into(),
        used_tailscale_key_sha256: Vec::new(),
    };
    let identity = bmacd::joiner::Identity {
        ssh_host_key: "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIOmM".into(),
        https_fingerprint: vec!["AB"; 32].join(":"),
    };
    let progress = bmacd::joiner::request_once(&bundle, &identity, port, 5).await;
    assert_eq!(progress.state, JoinState::Waiting, "{progress:?}");
    assert_eq!(progress.host, Some(host(1)));
    let pending = mox1.mesh.tokens().pending().unwrap();
    assert_eq!(Some(pending[0].id.clone()), progress.request_id);
    assert_eq!(
        pending[0].ssh_host_key.as_deref(),
        Some(identity.ssh_host_key.as_str())
    );

    let again = bmacd::joiner::request_once(&bundle, &identity, port, 6).await;
    assert_eq!(again.state, JoinState::Refused, "a used token is refused");

    bundle.proxmox_ca_fingerprint = vec!["00"; 32].join(":");
    let wrong = bmacd::joiner::request_once(&bundle, &identity, port, 7).await;
    assert_eq!(
        wrong.state,
        JoinState::Requesting,
        "another CA is kept trying, not refused"
    );
    assert!(wrong.message.unwrap().contains("fingerprint"));
    drop(mox1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn answers_are_relayed_to_the_host_running_the_job() {
    let lab = Lab::new(&[1, 2, 3]);
    let mox1 = lab.node(1).await;
    let mox2 = lab.node(2).await;
    let answer = || RelayedAnswer {
        user: "olive@pve".into(),
        role: Role::Operator,
        job: JobId::random(),
        request_id: "r1".into(),
        answer: bmac_core::job::Answer::Acknowledge,
    };
    let refused = mox2.mesh.forward(host(1), answer()).await.unwrap_err();
    assert_eq!(
        refused.code,
        ErrorCode::NotFound,
        "mox1's own refusal comes back: {}",
        refused.message
    );
    let unreachable = mox2.mesh.forward(host(3), answer()).await.unwrap_err();
    assert_eq!(unreachable.code, ErrorCode::Unavailable);
    assert!(
        unreachable.message.contains("mox3 is unreachable"),
        "{}",
        unreachable.message
    );
    drop(mox1);
}

const OPERATOR: Caller = Caller {
    user: "olive@pve",
    role: Role::Operator,
};

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

/// Answers the next request after `after` from `node`'s runner.
async fn answer(node: &harness::Node, id: JobId, after: &str, answer: Answer) -> String {
    let request = wait_for(&node.jobs, id, "the next request", |r| {
        r.pending_request.as_deref().is_some_and(|p| p != after)
    })
    .await
    .pending_request
    .unwrap();
    node.runner
        .answer(RelayedAnswer {
            user: OPERATOR.user.into(),
            role: OPERATOR.role,
            job: id,
            request_id: request.clone(),
            answer,
        })
        .await
        .unwrap();
    request
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_job_on_one_host_is_answered_and_watched_from_another() {
    let lab = Lab::new(&[1, 2, 3]);
    let mut mox1 = lab.node(1).await;
    let mut mox2 = lab.node(2).await;
    let mut mox3 = lab.node(3).await;
    for node in [&mut mox1, &mut mox2, &mut mox3] {
        node.link();
    }
    let job = mox3
        .jobs
        .admit(
            OPERATOR,
            &LaunchJob {
                operation: "test.interactive".parse().unwrap(),
                parameters: Default::default(),
                idempotency_key: "k1".into(),
                dry_run: false,
            },
        )
        .unwrap()
        .job;
    assert_eq!(job.target, host(1));
    let mut tab = mox2.hub.subscribe("t", None);
    mox2.hub.watch("t", &tab.stream, vec![job.id]).unwrap();

    let values = Answer::Values {
        values: serde_json::from_value(json!({"cores": 4, "hosts": ["mox1", "mox2"]})).unwrap(),
    };
    let first = answer(&mox2, job.id, "", values).await;
    let confirm = answer(&mox3, job.id, &first, Answer::Confirm { confirmed: true }).await;
    answer(&mox2, job.id, &confirm, Answer::Acknowledge).await;
    let done = wait_for(&mox2.jobs, job.id, "the end", |r| r.state.is_terminal()).await;
    assert_eq!(done.state, JobState::Completed, "{done:?}");
    until(&mut tab, "the copied completion", |b| {
        matches!(b, StreamBody::JobEvent { event } if event.job == job.id && matches!(event.body, EventBody::Completed { .. }))
    })
    .await;
    for node in [&mox2, &mox3] {
        let copies = node.store.job_events(job.id, 1, 2000).await.unwrap();
        let seqs: Vec<u64> = copies.events.iter().map(|e| e.seq).collect();
        assert_eq!(seqs, (1..=done.last_seq).collect::<Vec<_>>());
    }
}
