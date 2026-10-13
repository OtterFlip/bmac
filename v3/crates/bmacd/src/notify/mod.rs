// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

//! Alerts and email notifications (`MONITORING.md`, "Alerts" and
//! "Notifications").
//!
//! The duty host turns the cluster's conditions into alerts, queues the
//! messages the routing rules ask for, and sends the outbox through
//! Postmark. Without quorum every host does the same with its own store,
//! and merges it into pmxcfs when quorum returns; duplicates from several
//! hosts during a partition are better than silence.

pub mod alerts;
pub mod outbox;
pub mod store;

use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bmac_core::HostSlot;
use bmac_notify::postmark::{self, Email, Outcome, Transport};
use bmac_notify::templates::{
    AlertOpened, AlertResolved, Digest, DigestAlert, Layout, Model, TestEmail, render,
};
use bmac_proto::alerts::{
    Alert, AlertState, NotifySettings, NotifyView, Route, SaveNotifySettings, SaveSilence, Silence,
    TemplatesView, TestEmailResult,
};
use bmac_proto::api::{ApiError, ErrorCode};
use bmac_proto::status::{Condition, ConditionSeverity, NOTIFY_COLLECTOR, NotifyObservation};
use jiff::Timestamp;
use jiff::tz::TimeZone;

use self::alerts::{Book, Change, archive, category, silenced, switched_off};
use self::outbox::{Entry, send_due};
use self::store::{NotifyStore, State};
use crate::auth::Clock;
use crate::cluster::ClusterView;
use crate::engine::events::EventHub;
use crate::log;
use crate::status::Publisher;

pub const EVALUATE_EVERY: Duration = Duration::from_secs(15);
/// After starting, alerts are opened but not resolved for this long, so
/// conditions from collectors that haven't reported yet aren't taken as
/// cleared.
pub const STARTUP_GRACE_MS: i64 = 5 * 60_000;
const HISTORY_SHOWN: usize = 200;
const MAX_SILENCE_MS: i64 = 90 * 24 * 60 * 60_000;
const MAX_RECIPIENTS: usize = 20;

/// The cluster's name and its current conditions.
pub type Conditions = Arc<dyn Fn() -> (Option<String>, Vec<Condition>) + Send + Sync>;
/// Postmark with a server token.
pub type Connect = Arc<dyn Fn(&str) -> Result<Arc<dyn Transport>, String> + Send + Sync>;

pub fn https() -> Connect {
    Arc::new(|token: &str| postmark::Https::new(token).map(|t| Arc::new(t) as Arc<dyn Transport>))
}

pub struct NotifierConfig {
    pub host: HostSlot,
    pub cluster: Arc<dyn ClusterView>,
    pub conditions: Conditions,
    pub hub: Arc<EventHub>,
    pub clock: Clock,
    pub zone: TimeZone,
    pub store: NotifyStore,
    pub local: NotifyStore,
    pub connect: Connect,
}

pub struct Notifier {
    config: NotifierConfig,
    started_ms: i64,
    publisher: Mutex<Publisher>,
}

impl std::fmt::Debug for Notifier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Notifier")
            .field("host", &self.config.host)
            .finish_non_exhaustive()
    }
}

fn internal(message: impl Into<String>) -> ApiError {
    ApiError::new(ErrorCode::Internal, message)
}

fn invalid(message: impl Into<String>) -> ApiError {
    ApiError::new(ErrorCode::Invalid, message)
}

fn utc(ms: i64) -> String {
    Timestamp::from_millisecond(ms)
        .map(|t| t.strftime("%Y-%m-%d %H:%M UTC").to_string())
        .unwrap_or_default()
}

fn local(ms: i64, zone: &TimeZone) -> String {
    Timestamp::from_millisecond(ms)
        .map(|t| {
            t.to_zoned(zone.clone())
                .strftime("%Y-%m-%d %H:%M %Z")
                .to_string()
        })
        .unwrap_or_default()
}

fn severity_name(severity: ConditionSeverity) -> &'static str {
    match severity {
        ConditionSeverity::Info => "Info",
        ConditionSeverity::Warning => "Warning",
        ConditionSeverity::Critical => "Critical",
    }
}

/// A plausible single email address: Postmark is the real judge.
pub fn is_address(text: &str) -> bool {
    let Some((user, domain)) = text.split_once('@') else {
        return false;
    };
    !user.is_empty()
        && domain.contains('.')
        && !domain.starts_with('.')
        && !domain.ends_with('.')
        && text.len() <= 254
        && text
            .bytes()
            .all(|b| b.is_ascii_graphic() && !b"<>,;\"'".contains(&b))
}

pub fn effective_routes(settings: &NotifySettings) -> Vec<Route> {
    if settings.routes.is_empty() {
        vec![Route {
            categories: Vec::new(),
            min_severity: ConditionSeverity::Warning,
            recipients: Vec::new(),
            recovery: true,
        }]
    } else {
        settings.routes.clone()
    }
}

/// Who is told about `alert`; for a recovery, only routes that send them.
pub fn recipients(alert: &Alert, settings: &NotifySettings, recovery: bool) -> BTreeSet<String> {
    let category = category(&alert.condition);
    let mut out = BTreeSet::new();
    for route in effective_routes(settings) {
        let matches = (route.categories.is_empty()
            || route.categories.iter().any(|c| c == category))
            && alert.severity >= route.min_severity
            && (!recovery || route.recovery);
        if matches {
            let to = if route.recipients.is_empty() {
                &settings.recipients
            } else {
                &route.recipients
            };
            out.extend(to.iter().cloned());
        }
    }
    out
}

pub fn validate_settings(settings: &NotifySettings) -> Result<(), ApiError> {
    if let Some(sender) = &settings.sender
        && !is_address(sender)
    {
        return Err(invalid(format!("{sender:?} is not an email address")));
    }
    let routes = &settings.routes;
    let all = settings
        .recipients
        .iter()
        .chain(routes.iter().flat_map(|r| r.recipients.iter()));
    for address in all {
        if !is_address(address) {
            return Err(invalid(format!("{address:?} is not an email address")));
        }
    }
    if settings.recipients.len() > MAX_RECIPIENTS
        || routes.iter().any(|r| r.recipients.len() > MAX_RECIPIENTS)
    {
        return Err(invalid(format!("at most {MAX_RECIPIENTS} recipients")));
    }
    if routes.len() > 20 {
        return Err(invalid("at most 20 routes"));
    }
    if !(1..=24 * 60).contains(&settings.throttle_minutes) {
        return Err(invalid("the throttle is 1 to 1440 minutes"));
    }
    if settings.digest_hour.is_some_and(|h| h > 23) {
        return Err(invalid("the digest hour is 0 to 23"));
    }
    if let Some(url) = &settings.dashboard_url
        && (!url.starts_with("https://") || url.len() > 200 || url.contains(char::is_whitespace))
    {
        return Err(invalid("the Dashboard address is an https:// URL"));
    }
    if settings.disabled_conditions.len() > 100 {
        return Err(invalid("at most 100 switched-off conditions"));
    }
    if let Some(bad) = settings
        .disabled_conditions
        .iter()
        .find(|c| !is_condition_pattern(c))
    {
        return Err(invalid(format!(
            "{bad:?} is not a condition (like backup.skipped) or a category (like zfs.)"
        )));
    }
    Ok(())
}

fn is_condition_pattern(text: &str) -> bool {
    let body = text.strip_suffix('.').unwrap_or(text);
    text.len() <= 64
        && !body.is_empty()
        && body.split('.').all(|part| {
            part.starts_with(|c: char| c.is_ascii_lowercase())
                && part
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
        })
}

/// What the messages say about the cluster.
struct Context<'a> {
    cluster_name: String,
    dashboard_url: String,
    zone: &'a TimeZone,
    now_ms: i64,
}

impl Context<'_> {
    fn layout(&self) -> Layout {
        Layout::new(&self.cluster_name, &self.dashboard_url, &utc(self.now_ms))
    }

    fn page(&self, page: &str) -> String {
        if self.dashboard_url.is_empty() {
            String::new()
        } else {
            format!("{}/{page}", self.dashboard_url.trim_end_matches('/'))
        }
    }

    fn entry<M: Model>(
        &self,
        key: String,
        model: &M,
        to: &str,
        tag: &str,
        alert: Option<&str>,
    ) -> Entry {
        Entry::new(
            key,
            M::ALIAS,
            render(&self.layout(), model),
            to,
            tag,
            alert,
            self.now_ms,
        )
    }
}

/// The messages `changes` call for, marking the alerts notified.
fn plan(
    book: &mut Book,
    changes: &[Change],
    state_settings: &NotifySettings,
    silences: &[Silence],
    context: &Context,
) -> Vec<Entry> {
    let throttle_ms = i64::from(state_settings.throttle_minutes) * 60_000;
    let mut out = Vec::new();
    for change in changes {
        let Some(alert) = book.get_mut(change.id()) else {
            continue;
        };
        // It doesn't try to email about email.
        if category(&alert.condition) == "notify" || silenced(alert, silences, context.now_ms) {
            continue;
        }
        let tag = category(&alert.condition).to_string();
        let host = alert
            .host
            .map(|h| h.to_string())
            .unwrap_or_else(|| "cluster".into());
        match change {
            Change::Opened(_) | Change::Reopened(_) | Change::Escalated(_) => {
                let escalated = matches!(change, Change::Escalated(_));
                if escalated && alert.state == AlertState::Acknowledged {
                    continue;
                }
                if !escalated
                    && alert
                        .notified_ms
                        .is_some_and(|t| context.now_ms - t < throttle_ms)
                {
                    continue;
                }
                let to = recipients(alert, state_settings, false);
                if to.is_empty() {
                    continue;
                }
                let model = AlertOpened {
                    alert_id: alert.id.clone(),
                    severity: severity_name(alert.severity).into(),
                    summary: alert.message.clone(),
                    host: host.clone(),
                    resource: alert.resource.clone(),
                    condition: alert.condition.clone(),
                    opened_at: utc(alert.opened_ms),
                    opened_at_local: local(alert.opened_ms, context.zone),
                    next_step: alert
                        .next_step
                        .as_ref()
                        .map(|s| s.text.clone())
                        .unwrap_or_else(|| "Open the alert in the Dashboard.".into()),
                    alert_url: context.page("alerts"),
                };
                for recipient in &to {
                    let key = format!("{}:opened:{}:{recipient}", alert.id, alert.generation);
                    out.push(context.entry(key, &model, recipient, &tag, Some(&alert.id)));
                }
                alert.notified_ms = Some(context.now_ms);
            }
            Change::Resolved(_) => {
                if alert.notified_ms.is_none() {
                    continue;
                }
                let resolved = alert.resolved_ms.unwrap_or(context.now_ms);
                let model = AlertResolved {
                    alert_id: alert.id.clone(),
                    severity: severity_name(alert.severity).into(),
                    summary: alert.message.clone(),
                    host: host.clone(),
                    resource: alert.resource.clone(),
                    condition: alert.condition.clone(),
                    opened_at: utc(alert.opened_ms),
                    resolved_at: utc(resolved),
                    resolved_at_local: local(resolved, context.zone),
                    alert_url: context.page("alerts"),
                };
                for recipient in recipients(alert, state_settings, true) {
                    let key = format!("{}:resolved:{}:{recipient}", alert.id, alert.generation);
                    out.push(context.entry(key, &model, &recipient, &tag, Some(&alert.id)));
                }
            }
        }
    }
    out
}

/// The daily digest, once a day at the configured hour.
fn digest(state: &State, context: &Context) -> Vec<Entry> {
    let Some(hour) = state.settings.digest_hour else {
        return Vec::new();
    };
    let Ok(now) = Timestamp::from_millisecond(context.now_ms) else {
        return Vec::new();
    };
    let zoned = now.to_zoned(context.zone.clone());
    if zoned.hour() != hour as i8 {
        return Vec::new();
    }
    let date = zoned.strftime("%Y-%m-%d").to_string();
    let open: Vec<&Alert> = state
        .book
        .alerts
        .iter()
        .filter(|a| a.state != AlertState::Resolved)
        .collect();
    let model = Digest {
        open_count: open.len() as u64,
        alerts: open
            .iter()
            .map(|a| DigestAlert {
                severity: severity_name(a.severity).into(),
                host: a
                    .host
                    .map(|h| h.to_string())
                    .unwrap_or_else(|| "cluster".into()),
                opened_at: utc(a.opened_ms),
                summary: a.message.clone(),
            })
            .collect(),
        backup_summary: "Backups are not set up yet.".into(),
        alerts_url: context.page("alerts"),
    };
    state
        .settings
        .recipients
        .iter()
        .filter_map(|to| {
            let key = format!("digest:{date}:{to}");
            (!state.outbox.contains(&key)).then(|| context.entry(key, &model, to, "digest", None))
        })
        .collect()
}

impl Notifier {
    pub fn new(config: NotifierConfig) -> Notifier {
        let started_ms = (config.clock)();
        let publisher = Publisher::new(config.hub.clone(), config.host, config.clock.clone());
        Notifier {
            config,
            started_ms,
            publisher: Mutex::new(publisher),
        }
    }

    fn quorate(&self) -> bool {
        self.config.cluster.membership().is_ok_and(|m| m.quorate)
    }

    /// The store the Dashboard's calls act on: pmxcfs with quorum, else
    /// this host's own.
    fn active(&self) -> (&NotifyStore, bool) {
        if self.quorate() {
            (&self.config.store, false)
        } else {
            (&self.config.local, true)
        }
    }

    /// Merges this host's local store into pmxcfs, once quorum is back.
    fn merge_local(&self) -> Result<(), String> {
        let local = self.config.local.read()?;
        if local.is_empty() {
            return Ok(());
        }
        self.config.store.update(|state| {
            state.book.merge(local.book.clone());
            for entry in local.outbox.entries.clone() {
                state.outbox.enqueue(entry);
            }
            Ok(())
        })?;
        self.config.local.clear()?;
        log::info("notifications: merged the alerts kept here without quorum");
        Ok(())
    }

    /// Evaluates the conditions and queues messages; returns the store it
    /// acted on, or `None` when another host has the duty.
    pub fn evaluate(&self) -> Result<Option<&NotifyStore>, String> {
        let membership = self.config.cluster.membership()?;
        let store = if membership.quorate {
            self.merge_local()?;
            if membership.duty() != Some(self.config.host) {
                return Ok(None);
            }
            &self.config.store
        } else {
            &self.config.local
        };
        let now = (self.config.clock)();
        let (cluster_name, conditions) = (self.config.conditions)();
        let may_resolve = now - self.started_ms >= STARTUP_GRACE_MS;
        let zone = self.config.zone.clone();
        // pmxcfs stays readable without quorum, so a host alone still uses
        // the cluster's settings and silences.
        let shared = if membership.quorate {
            None
        } else {
            self.config.store.read().ok()
        };
        store.update(|state| {
            if let Some(shared) = &shared {
                state.settings = shared.settings.clone();
                state.silences = shared.silences.clone();
            }
            let context = Context {
                cluster_name: cluster_name.clone().unwrap_or_default(),
                dashboard_url: state.settings.dashboard_url.clone().unwrap_or_default(),
                zone: &zone,
                now_ms: now,
            };
            let disabled = state.settings.disabled_conditions.clone();
            state.book.switch_off(&disabled, now);
            let enabled: Vec<_> = conditions
                .iter()
                .filter(|c| !switched_off(&disabled, &c.id))
                .cloned()
                .collect();
            let (changes, expired) = state.book.evaluate(&enabled, now, may_resolve);
            archive(&mut state.history, expired);
            let settings = state.settings.clone();
            let silences = state.silences.clone();
            for entry in plan(&mut state.book, &changes, &settings, &silences, &context) {
                state.outbox.enqueue(entry);
            }
            for entry in digest(state, &context) {
                state.outbox.enqueue(entry);
            }
            state.silences.retain(|s| s.until_ms > now);
            state.outbox.prune(now);
            Ok(())
        })?;
        Ok(Some(store))
    }

    /// Sends what is due from `store`'s outbox.
    pub async fn send(&self, store: &NotifyStore) -> Result<(), String> {
        let state = store.read()?;
        let now = (self.config.clock)();
        let (Some(token), Some(sender)) = (store.token(), state.settings.sender.clone()) else {
            return Ok(());
        };
        if state.outbox.health.unauthorized {
            return Ok(());
        }
        let due = state.outbox.due(now);
        if due.is_empty() {
            return Ok(());
        }
        let transport = (self.config.connect)(&token)?;
        let results = send_due(transport.as_ref(), &sender, &due).await;
        let now = (self.config.clock)();
        let store = store.clone();
        tokio::task::spawn_blocking(move || {
            store.update(|state| {
                for (key, outcome) in &results {
                    state.outbox.record(key, outcome, now);
                }
                Ok(())
            })
        })
        .await
        .map_err(|e| e.to_string())?
    }

    fn observe(&self, store: &NotifyStore) {
        let Ok(state) = store.read() else {
            return;
        };
        let health = &state.outbox.health;
        let observation = NotifyObservation {
            at_ms: (self.config.clock)(),
            token_set: store.token().is_some(),
            sender_set: state.settings.sender.is_some(),
            unauthorized: health.unauthorized,
            templates_missing: health.templates_missing,
            sender_refused: health.sender_refused,
            held: state.outbox.summary().held,
            inactive_recipients: health.inactive_recipients.clone(),
            failing_since_ms: health.failing_since_ms,
            last_error: health.last_error.clone(),
        };
        self.publisher
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .publish(NOTIFY_COLLECTOR, observation);
    }

    pub async fn tick(self: &Arc<Self>) {
        let notifier = self.clone();
        let evaluated =
            tokio::task::spawn_blocking(move || notifier.evaluate().map(|store| store.cloned()))
                .await
                .unwrap_or_else(|e| Err(e.to_string()));
        let store = match evaluated {
            Ok(Some(store)) => store,
            Ok(None) => return,
            Err(e) => {
                log::warning(&format!("notifications: {e}"));
                return;
            }
        };
        if let Err(e) = self.send(&store).await {
            log::warning(&format!("notifications: {e}"));
        }
        let notifier = self.clone();
        let _ = tokio::task::spawn_blocking(move || notifier.observe(&store)).await;
    }

    pub fn spawn(self: Arc<Self>) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(EVALUATE_EVERY);
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tick.tick().await;
                self.tick().await;
            }
        })
    }

    // The Dashboard's calls. The blocking ones run on the blocking pool.

    pub fn view(&self, can_acknowledge: bool, can_edit: bool) -> Result<NotifyView, ApiError> {
        let (store, local) = self.active();
        let mut state = store.read().map_err(internal)?;
        state
            .book
            .alerts
            .sort_by_key(|a| std::cmp::Reverse(a.opened_ms));
        state.history.truncate(HISTORY_SHOWN);
        Ok(NotifyView {
            alerts: state.book.alerts,
            history: state.history,
            silences: state.silences,
            settings: state.settings,
            settings_revision: state.settings_revision,
            token_set: store.token().is_some(),
            outbox: state.outbox.summary(),
            local,
            can_acknowledge,
            can_edit,
        })
    }

    pub fn acknowledge(&self, id: &str, by: &str) -> Result<Alert, ApiError> {
        let (store, _) = self.active();
        let now = (self.config.clock)();
        store
            .update(|state| state.book.acknowledge(id, by, now).cloned())
            .map_err(|e| ApiError::new(ErrorCode::NotFound, e))
    }

    pub fn save_silence(&self, request: SaveSilence, by: &str) -> Result<Silence, ApiError> {
        let now = (self.config.clock)();
        if request.until_ms <= now || request.until_ms - now > MAX_SILENCE_MS {
            return Err(invalid("a silence ends in the future, within 90 days"));
        }
        let reason = request.reason.trim();
        if reason.is_empty() || reason.len() > 200 {
            return Err(invalid("give a reason of up to 200 characters"));
        }
        for text in [&request.condition, &request.resource]
            .into_iter()
            .flatten()
        {
            if text.is_empty() || text.len() > 200 {
                return Err(invalid("a condition or resource is 1 to 200 characters"));
            }
        }
        let silence = Silence {
            id: uuid::Uuid::new_v4().simple().to_string()[..12].to_string(),
            condition: request.condition,
            resource: request.resource,
            host: request.host,
            until_ms: request.until_ms,
            reason: reason.to_string(),
            created_by: by.to_string(),
            created_ms: now,
        };
        self.config
            .store
            .update(|state| {
                state.silences.push(silence.clone());
                Ok(())
            })
            .map_err(internal)?;
        Ok(silence)
    }

    pub fn delete_silence(&self, id: &str) -> Result<Silence, ApiError> {
        self.config
            .store
            .update(|state| {
                let at = state
                    .silences
                    .iter()
                    .position(|s| s.id == id)
                    .ok_or_else(|| format!("no silence {id}"))?;
                Ok(state.silences.remove(at))
            })
            .map_err(|e| ApiError::new(ErrorCode::NotFound, e))
    }

    pub fn save_settings(&self, request: SaveNotifySettings) -> Result<u64, ApiError> {
        validate_settings(&request.settings)?;
        let store = &self.config.store;
        let now = (self.config.clock)();
        let result = store.update(|state| {
            if state.settings_revision != request.expected_revision {
                return Err("conflict".to_string());
            }
            state.settings = request.settings.clone();
            state.outbox.retry_sender(now);
            Ok(())
        });
        match result {
            Ok(()) => Ok(store
                .read()
                .map_err(internal)?
                .settings_revision
                .unwrap_or(0)),
            Err(e) if e == "conflict" => Err(ApiError::new(
                ErrorCode::Conflict,
                "the notification settings changed since they were read; reload and try again",
            )),
            Err(e) => Err(internal(e)),
        }
    }

    /// Stores the token (empty removes it) and lets sending resume.
    pub fn set_token(&self, token: &str) -> Result<(), ApiError> {
        self.config.store.set_token(token).map_err(invalid)?;
        let (store, _) = self.active();
        store
            .update(|state| {
                state.outbox.health.unauthorized = false;
                Ok(())
            })
            .map_err(internal)
    }

    fn transport(&self) -> Result<Arc<dyn Transport>, ApiError> {
        let token = self.config.store.token().ok_or_else(|| {
            ApiError::new(ErrorCode::Invalid, "set the Postmark server token first")
        })?;
        (self.config.connect)(&token).map_err(internal)
    }

    /// Sends the test email at once to `to`, or to the default recipients,
    /// and reports each result.
    pub async fn test_email(
        &self,
        to: Option<String>,
        by: &str,
    ) -> Result<Vec<TestEmailResult>, ApiError> {
        let (store, _) = self.active();
        let state = store.read().map_err(internal)?;
        let sender = state
            .settings
            .sender
            .clone()
            .ok_or_else(|| invalid("set the sender address first"))?;
        let recipients = match to {
            Some(address) if is_address(&address) => vec![address],
            Some(address) => return Err(invalid(format!("{address:?} is not an email address"))),
            None if state.settings.recipients.is_empty() => {
                return Err(invalid("add a recipient, or name one address"));
            }
            None => state.settings.recipients.clone(),
        };
        let transport = self.transport()?;
        let (cluster_name, _) = (self.config.conditions)();
        let context = Context {
            cluster_name: cluster_name.unwrap_or_default(),
            dashboard_url: state.settings.dashboard_url.clone().unwrap_or_default(),
            zone: &self.config.zone,
            now_ms: (self.config.clock)(),
        };
        let model = render(
            &context.layout(),
            &TestEmail {
                requested_by: by.to_string(),
                settings_url: context.page("settings"),
            },
        );
        let mut results = Vec::new();
        for recipient in recipients {
            let email = Email {
                from: sender.clone(),
                to: recipient.clone(),
                template: TestEmail::ALIAS.into(),
                model: model.clone(),
                tag: "test".into(),
                alert_id: None,
            };
            let (message_id, error) = match postmark::send(transport.as_ref(), &email).await {
                Outcome::Sent { message_id } => (Some(message_id), None),
                Outcome::TemplateMissing => (
                    None,
                    Some("the email templates are missing; create them below".into()),
                ),
                Outcome::BadToken => (None, Some("Postmark refused the server token".into())),
                Outcome::SenderRefused => (
                    None,
                    Some(format!(
                        "Postmark refused the sender {sender}: confirm it as a Sender Signature, or verify its domain, in Postmark"
                    )),
                ),
                Outcome::InactiveRecipient => {
                    (None, Some(format!("{recipient} is inactive in Postmark")))
                }
                Outcome::Retry(e) | Outcome::Failed(e) => (None, Some(e)),
            };
            results.push(TestEmailResult {
                recipient,
                message_id,
                error,
            });
        }
        Ok(results)
    }

    pub async fn templates(&self) -> Result<TemplatesView, ApiError> {
        let transport = self.transport()?;
        postmark::check(transport.as_ref()).await.map_err(internal)
    }

    /// Creates the missing templates, then releases messages held for them.
    pub async fn create_templates(&self) -> Result<TemplatesView, ApiError> {
        let transport = self.transport()?;
        let view = postmark::create_missing(transport.as_ref())
            .await
            .map_err(internal)?;
        let (store, _) = self.active();
        let store = store.clone();
        let now = (self.config.clock)();
        tokio::task::spawn_blocking(move || {
            store.update(|state| {
                state.outbox.release_held(now);
                Ok(())
            })
        })
        .await
        .map_err(|e| internal(e.to_string()))?
        .map_err(internal)?;
        Ok(view)
    }
}

#[cfg(test)]
mod tests;
