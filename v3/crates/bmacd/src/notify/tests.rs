// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::atomic::{AtomicI64, Ordering};

use bmac_core::GuestName;
use bmac_notify::postmark::Pending;
use bmac_pmxcfs::{Pmxcfs, RealFs};
use serde_json::{Value, json};

use super::*;
use crate::cluster::Membership;

const TOKEN: &str = "0a1b2c3d-1111-2222-3333-444455556666";
const MIN: i64 = 60_000;
const START: i64 = 1_760_000_000_000;

fn slot(n: u8) -> HostSlot {
    HostSlot::new(n).unwrap()
}

struct Cluster(Mutex<Membership>);

impl Cluster {
    fn set(&self, online: &[u8], quorate: bool) {
        let mut m = self.0.lock().unwrap();
        m.online = online.iter().map(|n| slot(*n)).collect();
        m.quorate = quorate;
    }
}

impl ClusterView for Cluster {
    fn membership(&self) -> Result<Membership, String> {
        Ok(self.0.lock().unwrap().clone())
    }
    fn guest_owner(&self, _: GuestName) -> Result<Option<HostSlot>, String> {
        Ok(None)
    }
    fn placement(&self, _: GuestName) -> Result<Vec<HostSlot>, String> {
        Ok(vec![])
    }
}

/// Postmark in memory: templates by alias, and every message sent.
#[derive(Default)]
struct Postmark {
    sent: Mutex<Vec<Value>>,
    /// Answers for the next sends, before the default success.
    script: Mutex<Vec<(u16, Value)>>,
    templates: Mutex<BTreeMap<String, Value>>,
}

impl Transport for Postmark {
    fn call<'a>(&'a self, method: &'a str, path: &'a str, body: Option<Value>) -> Pending<'a> {
        Box::pin(async move {
            Ok(match (method, path) {
                ("POST", "/email/withTemplate") => {
                    let body = body.unwrap();
                    let mut script = self.script.lock().unwrap();
                    if !script.is_empty() {
                        return Ok(script.remove(0));
                    }
                    if !self.templates.lock().unwrap().is_empty()
                        && !self
                            .templates
                            .lock()
                            .unwrap()
                            .contains_key(body["TemplateAlias"].as_str().unwrap())
                    {
                        return Ok((422, json!({"ErrorCode": 1101, "Message": "no template"})));
                    }
                    let mut sent = self.sent.lock().unwrap();
                    sent.push(body);
                    (
                        200,
                        json!({"ErrorCode": 0, "MessageID": format!("m{}", sent.len())}),
                    )
                }
                ("GET", "/server") => (200, json!({"Name": "bmac_lab", "DeliveryType": "Sandbox"})),
                ("POST", "/templates") => {
                    let body = body.unwrap();
                    let alias = body["Alias"].as_str().unwrap().to_string();
                    self.templates.lock().unwrap().insert(alias.clone(), body);
                    (200, json!({"Alias": alias}))
                }
                ("GET", path) => match self
                    .templates
                    .lock()
                    .unwrap()
                    .get(path.trim_start_matches("/templates/"))
                {
                    Some(t) => (200, t.clone()),
                    None => (422, json!({"ErrorCode": 1101})),
                },
                other => panic!("unexpected {other:?}"),
            })
        })
    }
}

struct World {
    _scratch: tempfile::TempDir,
    root: std::path::PathBuf,
    cluster: Arc<Cluster>,
    conditions: Arc<Mutex<Vec<Condition>>>,
    now: Arc<AtomicI64>,
    postmark: Arc<Postmark>,
}

impl World {
    fn new() -> World {
        let scratch = tempfile::tempdir().unwrap();
        let root = scratch.path().to_path_buf();
        World {
            _scratch: scratch,
            root,
            cluster: Arc::new(Cluster(Mutex::new(Membership {
                quorate: true,
                online: [slot(1), slot(2), slot(3)].into(),
                members: (1..=3).map(|n| (slot(n), None)).collect(),
            }))),
            conditions: Arc::new(Mutex::new(Vec::new())),
            now: Arc::new(AtomicI64::new(START)),
            postmark: Arc::new(Postmark::default()),
        }
    }

    fn store(&self) -> NotifyStore {
        NotifyStore::new(
            Pmxcfs::new(self.root.join("pve"), Arc::new(RealFs)),
            self.root.join("pve/priv/bmac/notify"),
            self.root.join("pve/priv/bmac/secrets/postmark.token"),
        )
    }

    fn local(&self, host: u8) -> NotifyStore {
        let dir = self.root.join(format!("local{host}"));
        NotifyStore::new(
            Pmxcfs::new(&dir, Arc::new(RealFs)),
            dir.join("notify-local"),
            self.root.join("pve/priv/bmac/secrets/postmark.token"),
        )
    }

    fn notifier(&self, host: u8) -> Arc<Notifier> {
        let conditions = self.conditions.clone();
        let now = self.now.clone();
        let postmark = self.postmark.clone();
        Arc::new(Notifier::new(NotifierConfig {
            host: slot(host),
            cluster: self.cluster.clone(),
            conditions: Arc::new(move || (Some("lab".into()), conditions.lock().unwrap().clone())),
            hub: EventHub::new(),
            clock: Arc::new(move || now.load(Ordering::SeqCst)),
            zone: TimeZone::UTC,
            store: self.store(),
            local: self.local(host),
            connect: Arc::new(move |token: &str| {
                assert_eq!(token, TOKEN);
                Ok(postmark.clone() as Arc<dyn Transport>)
            }),
        }))
    }

    fn configure(&self) {
        self.store().set_token(TOKEN).unwrap();
        self.store()
            .update(|state| {
                state.settings = NotifySettings {
                    sender: Some("bmac@example.com".into()),
                    recipients: vec!["ops@example.com".into(), "oncall@example.com".into()],
                    dashboard_url: Some("https://mox1.example.ts.net:8443/".into()),
                    ..NotifySettings::default()
                };
                Ok(())
            })
            .unwrap();
    }

    fn raise(&self, id: &str, resource: &str, severity: ConditionSeverity) {
        self.conditions.lock().unwrap().push(Condition {
            id: id.into(),
            resource: resource.into(),
            severity,
            message: format!("{resource}: {id}"),
            since_ms: START,
            next_steps: vec![],
        });
    }

    fn clear(&self) {
        self.conditions.lock().unwrap().clear();
    }

    fn advance(&self, ms: i64) {
        self.now.fetch_add(ms, Ordering::SeqCst);
    }

    fn sent(&self) -> Vec<(String, String)> {
        self.postmark
            .sent
            .lock()
            .unwrap()
            .iter()
            .map(|m| {
                (
                    m["TemplateAlias"].as_str().unwrap().to_string(),
                    m["To"].as_str().unwrap().to_string(),
                )
            })
            .collect()
    }
}

fn opened(to: &str) -> (String, String) {
    (AlertOpened::ALIAS.to_string(), to.to_string())
}

fn resolved(to: &str) -> (String, String) {
    (AlertResolved::ALIAS.to_string(), to.to_string())
}

#[tokio::test]
async fn an_alert_is_emailed_once_to_each_recipient_and_its_recovery_follows() {
    let world = World::new();
    world.configure();
    let duty = world.notifier(1);
    world.raise("zfs.pool.member", "mox2/rpool", ConditionSeverity::Critical);
    duty.tick().await;
    assert_eq!(
        world.sent(),
        vec![opened("oncall@example.com"), opened("ops@example.com")]
    );
    let message = world.postmark.sent.lock().unwrap()[0].clone();
    assert_eq!(message["From"], "bmac@example.com");
    assert_eq!(message["Tag"], "zfs");
    assert_eq!(message["TemplateModel"]["cluster_name"], "lab");
    assert_eq!(message["TemplateModel"]["host"], "mox2");
    assert_eq!(
        message["TemplateModel"]["alert_url"],
        "https://mox1.example.ts.net:8443/alerts"
    );
    assert_eq!(
        message["TemplateModel"]["opened_at_local"]
            .as_str()
            .unwrap()
            .len(),
        20
    );

    world.advance(MIN);
    duty.tick().await;
    assert_eq!(world.sent().len(), 2, "no repeat while it stays open");

    world.clear();
    world.advance(2 * MIN);
    duty.tick().await;
    assert_eq!(
        world.sent().len(),
        2,
        "nothing resolves during the startup grace"
    );
    world.advance(STARTUP_GRACE_MS);
    duty.tick().await;
    assert_eq!(
        &world.sent()[2..],
        &[resolved("oncall@example.com"), resolved("ops@example.com")]
    );
    let view = duty.view(true, true).unwrap();
    assert_eq!(view.alerts[0].state, AlertState::Resolved);
    assert_eq!(view.outbox.pending, 0);
    assert!(view.outbox.last_sent_ms.is_some());
}

#[tokio::test]
async fn only_the_duty_host_sends_and_a_new_duty_host_does_not_resend() {
    let world = World::new();
    world.configure();
    let (one, two) = (world.notifier(1), world.notifier(2));
    world.raise("host.offline", "mox3", ConditionSeverity::Critical);
    two.tick().await;
    assert!(world.sent().is_empty());
    one.tick().await;
    assert_eq!(world.sent().len(), 2);
    world.cluster.set(&[2, 3], true);
    world.advance(MIN);
    two.tick().await;
    assert_eq!(
        world.sent().len(),
        2,
        "the new duty host finds the alert and its delivery"
    );
}

#[tokio::test]
async fn warnings_wait_for_their_hold_time() {
    let world = World::new();
    world.configure();
    let duty = world.notifier(1);
    world.raise("unit.failed", "mox1/x.service", ConditionSeverity::Warning);
    duty.tick().await;
    assert!(world.sent().is_empty());
    world.advance(alerts::WARNING_HOLD_MS);
    duty.tick().await;
    assert_eq!(world.sent().len(), 2);
}

#[tokio::test]
async fn silences_and_notification_conditions_send_nothing_but_still_alert() {
    let world = World::new();
    world.configure();
    let duty = world.notifier(1);
    duty.save_silence(
        SaveSilence {
            condition: Some("zfs.".into()),
            resource: None,
            host: Some(slot(2)),
            until_ms: START + 60 * MIN,
            reason: "replacing a disk".into(),
        },
        "alice",
    )
    .unwrap();
    world.raise("zfs.pool.member", "mox2/rpool", ConditionSeverity::Critical);
    world.raise(
        "notify.unauthorized",
        "postmark",
        ConditionSeverity::Critical,
    );
    duty.tick().await;
    assert!(world.sent().is_empty());
    assert_eq!(duty.view(true, true).unwrap().alerts.len(), 2);
}

#[tokio::test]
async fn a_reopened_alert_is_throttled_but_an_escalation_is_not() {
    let world = World::new();
    world.configure();
    let duty = world.notifier(1);
    world.advance(STARTUP_GRACE_MS);
    world.raise("disk.wear", "mox1/sda", ConditionSeverity::Critical);
    duty.tick().await;
    world.clear();
    world.advance(MIN);
    duty.tick().await;
    assert_eq!(world.sent().len(), 4, "opened and resolved");
    world.raise("disk.wear", "mox1/sda", ConditionSeverity::Critical);
    world.advance(MIN);
    duty.tick().await;
    assert_eq!(world.sent().len(), 4, "reopened within the hour: throttled");
    assert_eq!(duty.view(true, true).unwrap().alerts[0].generation, 2);
}

#[tokio::test]
async fn without_quorum_each_host_alerts_alone_and_merges_when_quorum_returns() {
    let world = World::new();
    world.configure();
    let two = world.notifier(2);
    world.cluster.set(&[2], false);
    world.raise(
        "corosync.link",
        "mox1-mox2/link0",
        ConditionSeverity::Critical,
    );
    two.tick().await;
    assert_eq!(
        world.sent().len(),
        2,
        "a host without quorum notifies directly"
    );
    assert!(two.view(true, true).unwrap().local);
    assert!(world.store().read().unwrap().book.alerts.is_empty());

    world.cluster.set(&[1, 2, 3], true);
    world.advance(MIN);
    two.tick().await;
    let merged = world.store().read().unwrap();
    assert_eq!(merged.book.alerts.len(), 1);
    assert_eq!(merged.outbox.entries.len(), 2);
    assert!(world.local(2).read().unwrap().is_empty());
    let one = world.notifier(1);
    one.tick().await;
    assert_eq!(
        world.sent().len(),
        2,
        "the merged deliveries are not sent again"
    );
}

#[tokio::test]
async fn messages_wait_for_missing_templates_and_go_once_they_are_created() {
    let world = World::new();
    world.configure();
    world
        .postmark
        .templates
        .lock()
        .unwrap()
        .insert("unrelated".into(), json!({}));
    let duty = world.notifier(1);
    world.raise("host.offline", "mox3", ConditionSeverity::Critical);
    duty.tick().await;
    assert!(world.sent().is_empty());
    let view = duty.view(true, true).unwrap();
    assert_eq!(view.outbox.held, 2);
    duty.observe(&world.store());

    let templates = duty.create_templates().await.unwrap();
    assert!(
        templates
            .templates
            .iter()
            .all(|t| t.presence == bmac_proto::alerts::TemplatePresence::Present)
    );
    duty.tick().await;
    assert_eq!(world.sent().len(), 2);
}

#[tokio::test]
async fn a_refused_token_stops_sending_until_a_new_one_is_set() {
    let world = World::new();
    world.configure();
    world
        .postmark
        .script
        .lock()
        .unwrap()
        .push((401, json!({"ErrorCode": 10, "Message": "bad token"})));
    let duty = world.notifier(1);
    world.raise("host.offline", "mox3", ConditionSeverity::Critical);
    duty.tick().await;
    assert!(world.sent().is_empty());
    assert!(world.store().read().unwrap().outbox.health.unauthorized);
    world.advance(MIN);
    duty.tick().await;
    assert!(
        world.sent().is_empty(),
        "no sends while the token is refused"
    );
    duty.set_token(TOKEN).unwrap();
    duty.tick().await;
    assert_eq!(world.sent().len(), 2);
}

#[tokio::test]
async fn the_test_email_reports_each_recipient() {
    let world = World::new();
    let duty = world.notifier(1);
    let refused = duty.test_email(None, "alice").await.unwrap_err();
    assert!(refused.message.contains("sender"), "{}", refused.message);
    world.configure();
    let results = duty
        .test_email(Some("me@example.com".into()), "alice")
        .await
        .unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].message_id.as_deref(), Some("m1"));
    assert_eq!(
        world.sent(),
        vec![(TestEmail::ALIAS.to_string(), "me@example.com".into())]
    );
    let model = world.postmark.sent.lock().unwrap()[0]["TemplateModel"].clone();
    assert_eq!(model["requested_by"], "alice");
    assert_eq!(
        model["settings_url"],
        "https://mox1.example.ts.net:8443/settings"
    );
    assert!(
        duty.test_email(Some("not an address".into()), "alice")
            .await
            .is_err()
    );
}

#[tokio::test]
async fn the_digest_goes_once_a_day_at_its_hour() {
    let world = World::new();
    world.configure();
    world
        .store()
        .update(|state| {
            state.settings.digest_hour = Some(7);
            Ok(())
        })
        .unwrap();
    let seven = Timestamp::from_second(1_760_000_000)
        .unwrap()
        .to_zoned(TimeZone::UTC)
        .date()
        .at(7, 0, 0, 0)
        .to_zoned(TimeZone::UTC)
        .unwrap()
        .timestamp()
        .as_millisecond();
    world.now.store(seven - MIN, Ordering::SeqCst);
    let duty = world.notifier(1);
    duty.tick().await;
    assert!(world.sent().is_empty());
    world.advance(MIN);
    duty.tick().await;
    world.advance(30 * MIN);
    duty.tick().await;
    let digests: Vec<_> = world
        .sent()
        .into_iter()
        .filter(|(a, _)| a == Digest::ALIAS)
        .collect();
    assert_eq!(digests.len(), 2, "one for each recipient, once");
}

#[tokio::test]
async fn no_record_holds_the_token() {
    let world = World::new();
    world.configure();
    let duty = world.notifier(1);
    world.raise("host.offline", "mox3", ConditionSeverity::Critical);
    duty.tick().await;
    duty.test_email(None, "alice").await.unwrap();
    fn sweep(dir: &Path, found: &mut Vec<String>) {
        for entry in std::fs::read_dir(dir).unwrap().flatten() {
            let path = entry.path();
            if path.is_dir() {
                sweep(&path, found);
            } else if std::fs::read_to_string(&path).is_ok_and(|t| t.contains(TOKEN)) {
                found.push(path.display().to_string());
            }
        }
    }
    let mut found = Vec::new();
    sweep(&world.root, &mut found);
    assert_eq!(found.len(), 1, "{found:?}");
    assert!(found[0].ends_with("secrets/postmark.token"));
    for message in world.postmark.sent.lock().unwrap().iter() {
        assert!(!message.to_string().contains(TOKEN));
    }
}

#[test]
fn settings_are_validated() {
    let good = NotifySettings {
        sender: Some("bmac@example.com".into()),
        recipients: vec!["ops@example.com".into()],
        ..NotifySettings::default()
    };
    assert!(validate_settings(&good).is_ok());
    for bad in [
        NotifySettings {
            sender: Some("nobody".into()),
            ..good.clone()
        },
        NotifySettings {
            recipients: vec!["a@b".into()],
            ..good.clone()
        },
        NotifySettings {
            throttle_minutes: 0,
            ..good.clone()
        },
        NotifySettings {
            digest_hour: Some(24),
            ..good.clone()
        },
        NotifySettings {
            dashboard_url: Some("http://x".into()),
            ..good.clone()
        },
        NotifySettings {
            disabled_conditions: vec!["Zfs.".into()],
            ..good.clone()
        },
        NotifySettings {
            disabled_conditions: vec![".".into()],
            ..good.clone()
        },
        NotifySettings {
            disabled_conditions: vec!["zfs..pool".into()],
            ..good.clone()
        },
    ] {
        assert!(validate_settings(&bad).is_err(), "{bad:?}");
    }
    let switches = NotifySettings {
        disabled_conditions: vec![
            "zfs.".into(),
            "backup.skipped".into(),
            "logs.copy_lag".into(),
        ],
        ..good
    };
    assert!(validate_settings(&switches).is_ok());
}

#[test]
fn routes_pick_recipients_by_category_severity_and_recovery() {
    let settings = NotifySettings {
        recipients: vec!["ops@example.com".into()],
        routes: vec![
            Route {
                categories: vec!["zfs".into(), "disk".into()],
                min_severity: ConditionSeverity::Warning,
                recipients: vec!["storage@example.com".into()],
                recovery: false,
            },
            Route {
                categories: vec![],
                min_severity: ConditionSeverity::Critical,
                recipients: vec![],
                recovery: true,
            },
        ],
        ..NotifySettings::default()
    };
    let mut book = Book::default();
    let condition = |id: &str, severity| Condition {
        id: id.into(),
        resource: "mox1/rpool".into(),
        severity,
        message: "m".into(),
        since_ms: 0,
        next_steps: vec![],
    };
    book.evaluate(
        &[condition("zfs.pool.member", ConditionSeverity::Critical)],
        0,
        true,
    );
    book.evaluate(
        &[
            condition("unit.failed", ConditionSeverity::Warning),
            condition("zfs.pool.member", ConditionSeverity::Critical),
        ],
        10 * MIN,
        true,
    );
    let zfs = &book.alerts[0];
    let unit = &book.alerts[1];
    let names = |set: BTreeSet<String>| set.into_iter().collect::<Vec<_>>();
    assert_eq!(
        names(recipients(zfs, &settings, false)),
        ["ops@example.com", "storage@example.com"]
    );
    assert_eq!(names(recipients(zfs, &settings, true)), ["ops@example.com"]);
    assert!(recipients(unit, &settings, false).is_empty());
}
