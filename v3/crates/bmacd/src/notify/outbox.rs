// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

//! The outbox (`MONITORING.md`, "Notifications"): one entry per message and
//! recipient, keyed by an idempotency key, sent by the duty host with
//! exponential backoff capped at an hour. A change of duty host may resend
//! a message whose delivery wasn't recorded; exactly-once is not promised.

use bmac_notify::postmark::{self, Email, Outcome, Transport};
use bmac_proto::alerts::{OutboxProblem, OutboxSummary};
use serde::{Deserialize, Serialize};
use serde_json::Value;

const FIRST_RETRY_MS: i64 = 30_000;
const MAX_RETRY_MS: i64 = 60 * 60_000;
/// Held messages are tried again this often, in case the templates were
/// created some other way.
const HELD_RETRY_MS: i64 = 60 * 60_000;
const KEEP_DONE_MS: i64 = 7 * 24 * 60 * 60_000;
const MAX_ENTRIES: usize = 1000;
const PROBLEMS_SHOWN: usize = 20;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Delivery {
    Pending,
    /// Waiting for the templates (`ErrorCode` 1101).
    Held,
    Sent,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Entry {
    pub key: String,
    pub template: String,
    pub model: Value,
    pub to: String,
    pub tag: String,
    pub alert_id: Option<String>,
    pub created_ms: i64,
    pub attempts: u32,
    pub next_ms: i64,
    pub state: Delivery,
    pub message_id: Option<String>,
    pub error: Option<String>,
    pub done_ms: Option<i64>,
}

impl Entry {
    pub fn new(
        key: String,
        template: &str,
        model: Value,
        to: &str,
        tag: &str,
        alert_id: Option<&str>,
        now_ms: i64,
    ) -> Entry {
        Entry {
            key,
            template: template.into(),
            model,
            to: to.into(),
            tag: tag.into(),
            alert_id: alert_id.map(Into::into),
            created_ms: now_ms,
            attempts: 0,
            next_ms: now_ms,
            state: Delivery::Pending,
            message_id: None,
            error: None,
            done_ms: None,
        }
    }
}

/// What the sender knows about the transport, for the Dashboard's
/// conditions.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Health {
    pub unauthorized: bool,
    pub templates_missing: bool,
    #[serde(default)]
    pub sender_refused: bool,
    pub inactive_recipients: Vec<String>,
    /// Since when every attempt has failed to reach Postmark.
    pub failing_since_ms: Option<i64>,
    pub last_error: Option<String>,
    pub last_sent_ms: Option<i64>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Outbox {
    pub entries: Vec<Entry>,
    #[serde(default)]
    pub health: Health,
}

/// The delay before attempt `attempts + 1`.
pub fn backoff(attempts: u32) -> i64 {
    FIRST_RETRY_MS
        .saturating_mul(1i64 << attempts.min(20))
        .min(MAX_RETRY_MS)
}

impl Outbox {
    /// Adds `entry` unless one with its key is already here; returns
    /// whether it was added.
    pub fn enqueue(&mut self, entry: Entry) -> bool {
        if self.entries.iter().any(|e| e.key == entry.key) {
            return false;
        }
        self.entries.push(entry);
        true
    }

    pub fn contains(&self, key: &str) -> bool {
        self.entries.iter().any(|e| e.key == key)
    }

    /// The keys of the entries to send now.
    pub fn due(&self, now_ms: i64) -> Vec<Entry> {
        self.entries
            .iter()
            .filter(|e| match e.state {
                Delivery::Pending => e.next_ms <= now_ms,
                Delivery::Held => e.next_ms <= now_ms,
                Delivery::Sent | Delivery::Failed => false,
            })
            .cloned()
            .collect()
    }

    /// Makes held messages due, once the templates exist.
    pub fn release_held(&mut self, now_ms: i64) {
        for entry in self
            .entries
            .iter_mut()
            .filter(|e| e.state == Delivery::Held)
        {
            entry.state = Delivery::Pending;
            entry.next_ms = now_ms;
        }
        self.health.templates_missing = false;
    }

    /// Makes pending messages due again after the sender was refused, for
    /// a new or newly confirmed sender.
    pub fn retry_sender(&mut self, now_ms: i64) {
        if !self.health.sender_refused {
            return;
        }
        self.health.sender_refused = false;
        for entry in self
            .entries
            .iter_mut()
            .filter(|e| e.state == Delivery::Pending)
        {
            entry.next_ms = entry.next_ms.min(now_ms);
        }
    }

    /// Records what sending `key` came to.
    pub fn record(&mut self, key: &str, outcome: &Outcome, now_ms: i64) {
        let Some(entry) = self.entries.iter_mut().find(|e| e.key == key) else {
            return;
        };
        if matches!(entry.state, Delivery::Sent | Delivery::Failed) {
            return;
        }
        let health = &mut self.health;
        match outcome {
            Outcome::Sent { message_id } => {
                entry.state = Delivery::Sent;
                entry.message_id = Some(message_id.clone());
                entry.error = None;
                entry.done_ms = Some(now_ms);
                health.unauthorized = false;
                health.sender_refused = false;
                health.failing_since_ms = None;
                health.last_sent_ms = Some(now_ms);
                health.inactive_recipients.retain(|r| *r != entry.to);
            }
            Outcome::Retry(error) => {
                entry.attempts += 1;
                entry.next_ms = now_ms + backoff(entry.attempts - 1);
                entry.error = Some(error.clone());
                health.failing_since_ms.get_or_insert(now_ms);
                health.last_error = Some(error.clone());
            }
            Outcome::BadToken => {
                entry.error = Some("Postmark refused the server token".into());
                health.unauthorized = true;
            }
            Outcome::SenderRefused => {
                entry.error = Some("Postmark refused the sender address".into());
                health.sender_refused = true;
            }
            Outcome::TemplateMissing => {
                entry.state = Delivery::Held;
                entry.next_ms = now_ms + HELD_RETRY_MS;
                entry.error = Some(format!(
                    "template {} is missing on the Postmark server",
                    entry.template
                ));
                health.templates_missing = true;
            }
            Outcome::InactiveRecipient => {
                entry.state = Delivery::Failed;
                entry.done_ms = Some(now_ms);
                entry.error = Some(format!("{} is inactive in Postmark", entry.to));
                if !health.inactive_recipients.contains(&entry.to) {
                    health.inactive_recipients.push(entry.to.clone());
                }
            }
            Outcome::Failed(error) => {
                entry.state = Delivery::Failed;
                entry.done_ms = Some(now_ms);
                entry.error = Some(error.clone());
                health.last_error = Some(error.clone());
            }
        }
        if *outcome == Outcome::SenderRefused {
            let retry = now_ms + HELD_RETRY_MS;
            for pending in self
                .entries
                .iter_mut()
                .filter(|e| e.state == Delivery::Pending)
            {
                pending.next_ms = pending.next_ms.max(retry);
            }
        }
    }

    /// Drops delivered and failed entries after a week, and the oldest of
    /// them past [`MAX_ENTRIES`].
    pub fn prune(&mut self, now_ms: i64) {
        self.entries
            .retain(|e| e.done_ms.is_none_or(|done| now_ms - done <= KEEP_DONE_MS));
        while self.entries.len() > MAX_ENTRIES {
            let Some(oldest) = self
                .entries
                .iter()
                .enumerate()
                .filter(|(_, e)| e.done_ms.is_some())
                .min_by_key(|(_, e)| e.done_ms)
                .map(|(i, _)| i)
            else {
                break;
            };
            self.entries.remove(oldest);
        }
    }

    pub fn summary(&self) -> OutboxSummary {
        let count = |state| self.entries.iter().filter(|e| e.state == state).count() as u32;
        let mut problems: Vec<&Entry> = self
            .entries
            .iter()
            .filter(|e| e.error.is_some() && e.state != Delivery::Sent)
            .collect();
        problems.sort_by_key(|e| std::cmp::Reverse(e.done_ms.unwrap_or(e.next_ms)));
        OutboxSummary {
            pending: count(Delivery::Pending),
            held: count(Delivery::Held),
            last_sent_ms: self.health.last_sent_ms,
            problems: problems
                .into_iter()
                .take(PROBLEMS_SHOWN)
                .map(|e| OutboxProblem {
                    recipient: e.to.clone(),
                    template: e.template.clone(),
                    error: e.error.clone().unwrap_or_default(),
                    at_ms: e.done_ms.unwrap_or(e.next_ms),
                })
                .collect(),
        }
    }
}

/// Sends `due` with `transport` from `sender`, stopping at a refused token
/// or sender,
/// and returns each key with its outcome for [`Outbox::record`].
pub async fn send_due(
    transport: &dyn Transport,
    sender: &str,
    due: &[Entry],
) -> Vec<(String, Outcome)> {
    let mut results = Vec::new();
    for entry in due {
        let email = Email {
            from: sender.to_string(),
            to: entry.to.clone(),
            template: entry.template.clone(),
            model: entry.model.clone(),
            tag: entry.tag.clone(),
            alert_id: entry.alert_id.clone(),
        };
        let outcome = postmark::send(transport, &email).await;
        let stop = matches!(outcome, Outcome::BadToken | Outcome::SenderRefused);
        results.push((entry.key.clone(), outcome));
        if stop {
            break;
        }
    }
    results
}

#[cfg(test)]
mod tests {
    use super::*;
    use bmac_notify::postmark::Pending;
    use serde_json::json;
    use std::sync::Mutex;

    fn entry(key: &str, now: i64) -> Entry {
        Entry::new(
            key.into(),
            "bmac-test-v1",
            json!({}),
            "ops@example.com",
            "test",
            None,
            now,
        )
    }

    #[test]
    fn the_same_key_is_queued_once() {
        let mut outbox = Outbox::default();
        assert!(outbox.enqueue(entry("a:open:1:ops", 0)));
        assert!(!outbox.enqueue(entry("a:open:1:ops", 5)));
        assert_eq!(outbox.entries.len(), 1);
    }

    #[test]
    fn retries_back_off_to_an_hour() {
        assert_eq!(backoff(0), 30_000);
        assert_eq!(backoff(1), 60_000);
        assert_eq!(backoff(6), 32 * 60_000);
        assert_eq!(backoff(7), 60 * 60_000);
        assert_eq!(backoff(40), 60 * 60_000);
        let mut outbox = Outbox::default();
        outbox.enqueue(entry("k", 0));
        outbox.record("k", &Outcome::Retry("HTTP 503".into()), 1000);
        assert_eq!(outbox.entries[0].next_ms, 31_000);
        assert!(outbox.due(30_000).is_empty());
        assert_eq!(outbox.due(31_000).len(), 1);
        assert_eq!(outbox.health.failing_since_ms, Some(1000));
        outbox.record("k", &Outcome::Retry("timed out".into()), 31_000);
        assert_eq!(
            outbox.health.failing_since_ms,
            Some(1000),
            "since the first failure"
        );
        outbox.record(
            "k",
            &Outcome::Sent {
                message_id: "m".into(),
            },
            100_000,
        );
        assert_eq!(outbox.health.failing_since_ms, None);
        assert_eq!(outbox.entries[0].state, Delivery::Sent);
    }

    #[test]
    fn a_missing_template_holds_until_released() {
        let mut outbox = Outbox::default();
        outbox.enqueue(entry("k", 0));
        outbox.record("k", &Outcome::TemplateMissing, 10);
        assert_eq!(outbox.entries[0].state, Delivery::Held);
        assert!(outbox.health.templates_missing);
        assert!(outbox.due(20).is_empty());
        outbox.release_held(30);
        assert_eq!(outbox.due(30).len(), 1);
        assert!(!outbox.health.templates_missing);
    }

    #[test]
    fn inactive_recipients_and_other_refusals_are_not_retried() {
        let mut outbox = Outbox::default();
        outbox.enqueue(entry("a", 0));
        outbox.enqueue(entry("b", 0));
        outbox.record("a", &Outcome::InactiveRecipient, 1);
        outbox.record(
            "b",
            &Outcome::Failed("HTTP 422, ErrorCode 300: bad".into()),
            1,
        );
        assert!(outbox.due(10_000_000).is_empty());
        assert_eq!(outbox.health.inactive_recipients, vec!["ops@example.com"]);
        let summary = outbox.summary();
        assert_eq!(summary.problems.len(), 2);
        assert_eq!(summary.pending, 0);
    }

    #[test]
    fn a_refused_sender_pauses_every_message_until_the_settings_change() {
        let mut outbox = Outbox::default();
        for key in ["a", "b"] {
            outbox.enqueue(entry(key, 0));
        }
        outbox.record("a", &Outcome::SenderRefused, 10);
        assert!(outbox.health.sender_refused);
        assert!(
            outbox.due(10 + HELD_RETRY_MS - 1).is_empty(),
            "nothing is tried for an hour"
        );
        assert_eq!(outbox.due(10 + HELD_RETRY_MS).len(), 2);
        outbox.retry_sender(20);
        assert!(!outbox.health.sender_refused);
        assert_eq!(outbox.due(20).len(), 2, "a saved sender is tried at once");
    }

    #[test]
    fn a_unauthorized_stays_pending_and_flags_health() {
        let mut outbox = Outbox::default();
        outbox.enqueue(entry("a", 0));
        outbox.record("a", &Outcome::BadToken, 1);
        assert_eq!(outbox.entries[0].state, Delivery::Pending);
        assert!(outbox.health.unauthorized);
    }

    #[test]
    fn done_entries_age_out_after_a_week() {
        let mut outbox = Outbox::default();
        outbox.enqueue(entry("sent", 0));
        outbox.enqueue(entry("waiting", 0));
        outbox.record(
            "sent",
            &Outcome::Sent {
                message_id: "m".into(),
            },
            0,
        );
        outbox.prune(KEEP_DONE_MS + 1);
        assert_eq!(outbox.entries.len(), 1);
        assert_eq!(outbox.entries[0].key, "waiting");
    }

    struct Scripted(Mutex<Vec<(u16, Value)>>, Mutex<Vec<String>>);

    impl Transport for Scripted {
        fn call<'a>(&'a self, _: &'a str, _: &'a str, body: Option<Value>) -> Pending<'a> {
            self.1
                .lock()
                .unwrap()
                .push(body.unwrap()["To"].as_str().unwrap().into());
            let answer = self.0.lock().unwrap().remove(0);
            Box::pin(async move { Ok(answer) })
        }
    }

    #[tokio::test]
    async fn sending_stops_at_a_refused_token() {
        let transport = Scripted(
            Mutex::new(vec![
                (200, json!({"ErrorCode": 0, "MessageID": "m1"})),
                (401, json!({"ErrorCode": 10, "Message": "bad token"})),
            ]),
            Mutex::new(vec![]),
        );
        let mut due = vec![entry("a", 0), entry("b", 0), entry("c", 0)];
        due[1].to = "two@example.com".into();
        let results = send_due(&transport, "bmac@example.com", &due).await;
        assert_eq!(results.len(), 2);
        assert_eq!(results[1].1, Outcome::BadToken);
        assert_eq!(transport.1.lock().unwrap().len(), 2, "c was not attempted");
    }
}
