// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

use std::sync::Mutex;

use bmac_notify::postmark::Pending;
use serde_json::{Value, json};

use super::*;

const MIN: i64 = 60_000;

/// Postmark answering every send with `answer`, recording what was sent.
#[derive(Default)]
struct Fake {
    answer: Mutex<Option<(u16, Value)>>,
    sent: Mutex<Vec<Value>>,
}

impl Fake {
    fn answer(&self, status: u16, body: Value) {
        *self.answer.lock().unwrap() = Some((status, body));
    }

    fn aliases(&self) -> Vec<String> {
        self.sent
            .lock()
            .unwrap()
            .iter()
            .map(|b| b["TemplateAlias"].as_str().unwrap().to_string())
            .collect()
    }
}

impl Transport for Fake {
    fn call<'a>(&'a self, _method: &'a str, _path: &'a str, body: Option<Value>) -> Pending<'a> {
        Box::pin(async move {
            self.sent.lock().unwrap().push(body.unwrap_or(Value::Null));
            Ok(self
                .answer
                .lock()
                .unwrap()
                .clone()
                .unwrap_or((200, json!({"ErrorCode": 0, "MessageID": "m1"}))))
        })
    }
}

struct Rig {
    _dir: tempfile::TempDir,
    metadata: Metadata,
    audit: Audit,
    fake: Fake,
}

fn rig() -> Rig {
    let dir = tempfile::tempdir().unwrap();
    Rig {
        metadata: Metadata::new(dir.path()),
        audit: Audit::new(dir.path()),
        _dir: dir,
        fake: Fake::default(),
    }
}

fn round(answered: &[&str]) -> Round {
    Round {
        cluster_uuid: "c1".into(),
        cluster_name: "lab".into(),
        dashboard_url: "https://mox1.example.ts.net/".into(),
        hosts: vec!["mox1".into(), "mox2".into()],
        answered: answered.iter().map(|h| h.to_string()).collect(),
    }
}

impl Rig {
    fn sender(&self) -> Sender<'_> {
        Sender {
            transport: &self.fake,
            from: "bmac@example.com".into(),
            to: vec!["ops@example.com".into()],
        }
    }

    async fn tick(
        &self,
        answered: &[&str],
        urls: &[(String, Result<(), String>)],
        at: i64,
        mail: bool,
    ) -> WatchState {
        let sender = self.sender();
        tick(
            &self.metadata,
            &self.audit,
            "vault",
            &[round(answered)],
            urls,
            mail.then_some(&sender),
            at,
        )
        .await
        .unwrap()
    }

    fn events(&self) -> Vec<String> {
        let mut events: Vec<String> = self
            .audit
            .recent(100)
            .unwrap()
            .into_iter()
            .map(|e| e.event)
            .collect();
        events.reverse();
        events
    }
}

#[tokio::test]
async fn a_silent_cluster_is_reported_once_after_the_wait_and_again_when_it_returns() {
    let r = rig();
    r.tick(&["mox1", "mox2"], &[], 0, true).await;
    r.tick(&["mox1"], &[], MIN, true).await;
    for minute in 2..=5 {
        r.tick(&[], &[], minute * MIN, true).await;
    }
    assert!(r.fake.aliases().is_empty(), "four minutes isn't five");
    let state = r.tick(&[], &[], 6 * MIN, true).await;
    assert_eq!(state.clusters[0].down_since_ms, Some(MIN));
    assert_eq!(r.fake.aliases(), vec!["bmac-watchdog-down-v1"]);
    let sent = r.fake.sent.lock().unwrap()[0].clone();
    assert_eq!(sent["To"], "ops@example.com");
    assert_eq!(sent["TemplateModel"]["cluster_name"], "lab");
    assert_eq!(sent["TemplateModel"]["appliance_name"], "vault");
    assert_eq!(
        sent["TemplateModel"]["hosts"],
        json!([
            {"host": "mox1", "last_seen": "1970-01-01 00:01 UTC"},
            {"host": "mox2", "last_seen": "1970-01-01 00:00 UTC"},
        ])
    );
    r.tick(&[], &[], 7 * MIN, true).await;
    assert_eq!(r.fake.aliases().len(), 1, "sent once");
    r.tick(&["mox2"], &[], 9 * MIN, true).await;
    assert_eq!(
        r.fake.aliases(),
        vec!["bmac-watchdog-down-v1", "bmac-watchdog-up-v1"]
    );
    let up = r.fake.sent.lock().unwrap()[1].clone();
    assert_eq!(up["TemplateModel"]["up_at"], "1970-01-01 00:09 UTC");
    assert_eq!(up["TemplateModel"]["hosts"], json!([{"host": "mox2"}]));
    assert_eq!(r.events(), vec!["watchdog_down", "watchdog_up"]);
}

#[tokio::test]
async fn without_email_settings_only_the_audit_log_hears_and_retries_wait_for_postmark() {
    let r = rig();
    r.tick(&["mox1"], &[], 0, false).await;
    r.tick(&[], &[], 5 * MIN, false).await;
    r.tick(&[], &[], 6 * MIN, false).await;
    r.tick(&["mox1"], &[], 7 * MIN, false).await;
    assert!(r.fake.aliases().is_empty());
    assert_eq!(r.events(), vec!["watchdog_down", "watchdog_up"]);

    r.fake.answer(503, json!({}));
    r.tick(&[], &[], 12 * MIN, true).await;
    r.tick(&[], &[], 13 * MIN, true).await;
    assert_eq!(r.fake.aliases().len(), 2, "a 503 is retried each minute");
    r.fake
        .answer(200, json!({"ErrorCode": 0, "MessageID": "m2"}));
    r.tick(&[], &[], 14 * MIN, true).await;
    r.tick(&[], &[], 15 * MIN, true).await;
    assert_eq!(r.fake.aliases().len(), 3, "then sent once");
}

#[tokio::test]
async fn a_refused_token_stops_the_mail_and_is_recorded_once() {
    let r = rig();
    r.fake
        .answer(401, json!({"ErrorCode": 10, "Message": "bad token"}));
    r.tick(&["mox1"], &[], 0, true).await;
    let state = r.tick(&[], &[], 5 * MIN, true).await;
    assert_eq!(
        state.notify_error.as_deref(),
        Some("Postmark refused the server token")
    );
    r.tick(&[], &[], 6 * MIN, true).await;
    assert_eq!(r.fake.aliases().len(), 1, "no retry after a bad token");
    let events = r.events();
    assert_eq!(events.iter().filter(|e| *e == "notify_failed").count(), 1);
}

#[tokio::test]
async fn a_failing_url_is_reported_after_the_wait_and_its_state_survives_a_restart() {
    let r = rig();
    let url = "https://shop.example.com/health".to_string();
    let bad = |e: &str| vec![(url.clone(), Err(e.to_string()))];
    let good = vec![(url.clone(), Ok(()))];
    r.tick(&["mox1"], &good, 0, true).await;
    for minute in 1..=5 {
        r.tick(&["mox1"], &bad("HTTP 503"), minute * MIN, true)
            .await;
    }
    assert!(r.fake.aliases().is_empty());
    r.tick(
        &["mox1"],
        &bad("no answer within 15 seconds"),
        6 * MIN,
        true,
    )
    .await;
    assert_eq!(r.fake.aliases(), vec!["bmac-url-down-v1"]);
    let down = r.fake.sent.lock().unwrap()[0].clone();
    assert_eq!(down["TemplateModel"]["url"], url.as_str());
    assert_eq!(
        down["TemplateModel"]["error"],
        "no answer within 15 seconds"
    );
    assert_eq!(
        down["TemplateModel"]["failing_since"],
        "1970-01-01 00:01 UTC"
    );

    let saved = WatchState::load(&r.metadata).unwrap();
    assert!(
        saved.urls[0].down_sent,
        "a restart reads this and sends nothing twice"
    );
    r.tick(&["mox1"], &bad("HTTP 503"), 7 * MIN, true).await;
    r.tick(&["mox1"], &good, 8 * MIN, true).await;
    assert_eq!(r.fake.aliases(), vec!["bmac-url-down-v1", "bmac-url-up-v1"]);
    assert_eq!(r.events(), vec!["url_down", "url_up"]);
    let state = r.tick(&["mox1"], &[], 9 * MIN, true).await;
    assert!(
        state.urls.is_empty(),
        "a URL taken off the list is forgotten"
    );
}
