// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

//! The alert state machine (`MONITORING.md`, "Alerts"): a condition at
//! warning or critical for its hold time opens an alert, once per condition
//! and resource; it resolves when the condition clears, and reopens, with
//! the same ID, if it returns within [`REOPEN_MS`].

use bmac_core::HostSlot;
use bmac_proto::alerts::{Alert, AlertState, Silence};
use bmac_proto::status::{Condition, ConditionSeverity};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub const REOPEN_MS: i64 = 10 * 60_000;
pub const WARNING_HOLD_MS: i64 = 5 * 60_000;
pub const HISTORY: usize = 2000;
const MAX_MESSAGE: usize = 300;

/// The alerts that are open, acknowledged, or resolved within
/// [`REOPEN_MS`].
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Book {
    pub alerts: Vec<Alert>,
}

/// What evaluating the conditions did to one alert, by ID.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Change {
    Opened(String),
    Reopened(String),
    /// A warning became critical while open.
    Escalated(String),
    Resolved(String),
}

impl Change {
    pub fn id(&self) -> &str {
        match self {
            Change::Opened(id)
            | Change::Reopened(id)
            | Change::Escalated(id)
            | Change::Resolved(id) => id,
        }
    }
}

/// The host a resource is on: `mox2` in `mox2/rpool` or `mox2`.
pub fn host_of(resource: &str) -> Option<HostSlot> {
    resource.split('/').next()?.parse().ok()
}

/// The condition's category: `zfs` in `zfs.pool.member`.
pub fn category(condition: &str) -> &str {
    condition.split('.').next().unwrap_or(condition)
}

fn alert_id(condition: &str, resource: &str, opened_ms: i64) -> String {
    let digest = Sha256::digest(format!("{condition}\0{resource}\0{opened_ms}"));
    digest.iter().take(6).map(|b| format!("{b:02x}")).collect()
}

fn trimmed(message: &str) -> String {
    match message.char_indices().nth(MAX_MESSAGE) {
        Some((at, _)) => format!("{}...", &message[..at]),
        None => message.to_string(),
    }
}

fn due(condition: &Condition, now_ms: i64) -> bool {
    match condition.severity {
        ConditionSeverity::Critical => true,
        ConditionSeverity::Warning => now_ms - condition.since_ms >= WARNING_HOLD_MS,
        ConditionSeverity::Info => false,
    }
}

impl Book {
    pub fn get(&self, id: &str) -> Option<&Alert> {
        self.alerts.iter().find(|a| a.id == id)
    }

    pub fn get_mut(&mut self, id: &str) -> Option<&mut Alert> {
        self.alerts.iter_mut().find(|a| a.id == id)
    }

    fn find(&mut self, condition: &str, resource: &str) -> Option<&mut Alert> {
        self.alerts
            .iter_mut()
            .find(|a| a.condition == condition && a.resource == resource)
    }

    /// Applies the current conditions. Alerts are resolved only when
    /// `may_resolve`, so a host that has just started, and hasn't heard
    /// every collector yet, doesn't resolve what it can't see. Returns what
    /// changed and the resolved alerts old enough to leave the book for
    /// the history.
    pub fn evaluate(
        &mut self,
        conditions: &[Condition],
        now_ms: i64,
        may_resolve: bool,
    ) -> (Vec<Change>, Vec<Alert>) {
        let mut expired = Vec::new();
        self.alerts.retain(|alert| {
            let stale = alert.state == AlertState::Resolved
                && now_ms - alert.resolved_ms.unwrap_or(0) > REOPEN_MS;
            if stale {
                expired.push(alert.clone());
            }
            !stale
        });
        let mut changes = Vec::new();
        for condition in conditions.iter().filter(|c| due(c, now_ms)) {
            let next_step = condition.next_steps.first().cloned();
            match self.find(&condition.id, &condition.resource) {
                Some(alert) if alert.state != AlertState::Resolved => {
                    alert.message = trimmed(&condition.message);
                    alert.next_step = next_step;
                    if condition.severity > alert.severity {
                        alert.severity = condition.severity;
                        alert.generation += 1;
                        changes.push(Change::Escalated(alert.id.clone()));
                    } else {
                        alert.severity = condition.severity;
                    }
                }
                Some(alert) => {
                    alert.state = AlertState::Open;
                    alert.resolved_ms = None;
                    alert.acknowledged_by = None;
                    alert.acknowledged_ms = None;
                    alert.severity = condition.severity;
                    alert.message = trimmed(&condition.message);
                    alert.next_step = next_step;
                    alert.generation += 1;
                    changes.push(Change::Reopened(alert.id.clone()));
                }
                None => {
                    let id = alert_id(&condition.id, &condition.resource, now_ms);
                    self.alerts.push(Alert {
                        id: id.clone(),
                        condition: condition.id.clone(),
                        resource: condition.resource.clone(),
                        host: host_of(&condition.resource),
                        severity: condition.severity,
                        message: trimmed(&condition.message),
                        next_step,
                        state: AlertState::Open,
                        opened_ms: now_ms,
                        resolved_ms: None,
                        acknowledged_by: None,
                        acknowledged_ms: None,
                        generation: 1,
                        notified_ms: None,
                    });
                    changes.push(Change::Opened(id));
                }
            }
        }
        if may_resolve {
            for alert in self.alerts.iter_mut() {
                if alert.state == AlertState::Resolved {
                    continue;
                }
                let present = conditions.iter().any(|c| {
                    c.id == alert.condition
                        && c.resource == alert.resource
                        && c.severity >= ConditionSeverity::Warning
                });
                if !present {
                    alert.state = AlertState::Resolved;
                    alert.resolved_ms = Some(now_ms);
                    changes.push(Change::Resolved(alert.id.clone()));
                }
            }
        }
        (changes, expired)
    }

    /// Closes the open alerts of switched-off conditions without a
    /// change, so no recovery message is sent for them.
    pub fn switch_off(&mut self, disabled: &[String], now_ms: i64) {
        for alert in self.alerts.iter_mut() {
            if alert.state != AlertState::Resolved && switched_off(disabled, &alert.condition) {
                alert.state = AlertState::Resolved;
                alert.resolved_ms = Some(now_ms);
            }
        }
    }

    /// Marks an open alert acknowledged, which stops further messages about
    /// it until it resolves.
    pub fn acknowledge(&mut self, id: &str, by: &str, now_ms: i64) -> Result<&Alert, String> {
        let alert = self
            .get_mut(id)
            .ok_or_else(|| format!("no open alert {id}"))?;
        match alert.state {
            AlertState::Open => {
                alert.state = AlertState::Acknowledged;
                alert.acknowledged_by = Some(by.to_string());
                alert.acknowledged_ms = Some(now_ms);
                Ok(alert)
            }
            AlertState::Acknowledged => Ok(alert),
            AlertState::Resolved => Err(format!("alert {id} is already resolved")),
        }
    }

    /// Adds alerts kept on one host without quorum, for those not already
    /// in this book.
    pub fn merge(&mut self, local: Book) {
        for alert in local.alerts {
            let known = self.alerts.iter().any(|a| {
                a.id == alert.id || (a.condition == alert.condition && a.resource == alert.resource)
            });
            if !known {
                self.alerts.push(alert);
            }
        }
    }
}

/// Adds `expired` to the front of `history`, newest first, keeping
/// [`HISTORY`].
pub fn archive(history: &mut Vec<Alert>, expired: Vec<Alert>) {
    if expired.is_empty() {
        return;
    }
    let mut newest = expired;
    newest.sort_by_key(|a| std::cmp::Reverse(a.resolved_ms));
    newest.append(history);
    newest.truncate(HISTORY);
    *history = newest;
}

/// Whether `condition` matches `pattern`: the same condition, or a category
/// ending in `.` that it belongs to.
fn matches(pattern: &str, condition: &str) -> bool {
    if pattern.ends_with('.') {
        condition.starts_with(pattern)
    } else {
        condition == pattern
    }
}

pub fn switched_off(disabled: &[String], condition: &str) -> bool {
    disabled.iter().any(|p| matches(p, condition))
}

pub fn silenced(alert: &Alert, silences: &[Silence], now_ms: i64) -> bool {
    silences.iter().any(|s| {
        s.until_ms > now_ms
            && s.condition
                .as_deref()
                .is_none_or(|c| matches(c, &alert.condition))
            && s.resource.as_deref().is_none_or(|r| alert.resource == r)
            && s.host.is_none_or(|h| alert.host == Some(h))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIN: i64 = 60_000;

    fn condition(
        id: &str,
        resource: &str,
        severity: ConditionSeverity,
        since_ms: i64,
    ) -> Condition {
        Condition {
            id: id.into(),
            resource: resource.into(),
            severity,
            message: format!("{resource}: {id}"),
            since_ms,
            next_steps: vec![],
        }
    }

    fn critical(resource: &str) -> Condition {
        condition("zfs.pool.member", resource, ConditionSeverity::Critical, 0)
    }

    #[test]
    fn critical_opens_at_once_and_warning_after_its_hold() {
        let mut book = Book::default();
        let warning = condition(
            "unit.failed",
            "mox1/x.service",
            ConditionSeverity::Warning,
            0,
        );
        let (changes, _) = book.evaluate(&[critical("mox2/rpool"), warning.clone()], MIN, true);
        assert_eq!(changes.len(), 1);
        assert!(matches!(&changes[0], Change::Opened(_)));
        assert_eq!(book.alerts[0].host, HostSlot::new(2).ok());
        let (changes, _) = book.evaluate(&[critical("mox2/rpool"), warning], 5 * MIN, true);
        assert_eq!(
            changes.len(),
            1,
            "the critical one doesn't open again: {changes:?}"
        );
        assert_eq!(book.alerts.len(), 2);
    }

    #[test]
    fn info_never_opens_an_alert() {
        let mut book = Book::default();
        let info = condition("x.y", "mox1", ConditionSeverity::Info, 0);
        assert!(book.evaluate(&[info], 60 * MIN, true).0.is_empty());
    }

    #[test]
    fn a_return_within_ten_minutes_reopens_the_same_alert() {
        let mut book = Book::default();
        book.evaluate(&[critical("mox2/rpool")], 0, true);
        let id = book.alerts[0].id.clone();
        book.acknowledge(&id, "alice", MIN).unwrap();
        let (changes, _) = book.evaluate(&[], 2 * MIN, true);
        assert_eq!(changes, vec![Change::Resolved(id.clone())]);
        let (changes, _) = book.evaluate(&[critical("mox2/rpool")], 11 * MIN, true);
        assert_eq!(changes, vec![Change::Reopened(id.clone())]);
        let alert = book.get(&id).unwrap();
        assert_eq!((alert.state, alert.generation), (AlertState::Open, 2));
        assert_eq!(
            alert.acknowledged_by, None,
            "reopening clears the acknowledgment"
        );
    }

    #[test]
    fn a_return_after_ten_minutes_is_a_new_alert_and_the_old_one_is_history() {
        let mut book = Book::default();
        book.evaluate(&[critical("mox2/rpool")], 0, true);
        let first = book.alerts[0].id.clone();
        book.evaluate(&[], MIN, true);
        let (changes, expired) = book.evaluate(&[], 12 * MIN, true);
        assert!(changes.is_empty());
        assert_eq!(expired.len(), 1);
        assert_eq!(expired[0].id, first);
        assert!(book.alerts.is_empty());
        let (changes, _) = book.evaluate(&[critical("mox2/rpool")], 13 * MIN, true);
        assert!(matches!(&changes[0], Change::Opened(id) if *id != first));
    }

    #[test]
    fn escalation_bumps_the_generation() {
        let mut book = Book::default();
        let warning = condition("disk.wear", "mox1/sda", ConditionSeverity::Warning, 0);
        book.evaluate(&[warning], 6 * MIN, true);
        let critical = condition("disk.wear", "mox1/sda", ConditionSeverity::Critical, 0);
        let (changes, _) = book.evaluate(&[critical], 7 * MIN, true);
        assert!(matches!(&changes[0], Change::Escalated(_)));
        assert_eq!(book.alerts[0].generation, 2);
    }

    #[test]
    fn nothing_resolves_until_resolving_is_allowed() {
        let mut book = Book::default();
        book.evaluate(&[critical("mox2/rpool")], 0, true);
        assert!(book.evaluate(&[], MIN, false).0.is_empty());
        assert_eq!(book.alerts[0].state, AlertState::Open);
    }

    #[test]
    fn history_is_newest_first_and_bounded() {
        let alert = |n: i64| {
            let mut book = Book::default();
            book.evaluate(&[critical(&format!("mox1/p{n}"))], n, true);
            book.evaluate(&[], n, true);
            book.alerts.remove(0)
        };
        let mut history: Vec<Alert> = (0..HISTORY as i64).map(alert).rev().collect();
        archive(&mut history, vec![alert(5000), alert(5001)]);
        assert_eq!(history.len(), HISTORY);
        assert_eq!(history[0].resolved_ms, Some(5001));
        assert_eq!(history[1].resolved_ms, Some(5000));
    }

    #[test]
    fn silences_match_by_condition_prefix_resource_and_host_until_they_end() {
        let mut book = Book::default();
        book.evaluate(&[critical("mox2/rpool")], 0, true);
        let alert = &book.alerts[0];
        let silence = |condition: Option<&str>, resource: Option<&str>, host: Option<u8>| Silence {
            id: "s".into(),
            condition: condition.map(Into::into),
            resource: resource.map(Into::into),
            host: host.map(|h| HostSlot::new(h).unwrap()),
            until_ms: 100,
            reason: "maintenance".into(),
            created_by: "alice".into(),
            created_ms: 0,
        };
        assert!(silenced(alert, &[silence(Some("zfs."), None, None)], 50));
        assert!(silenced(
            alert,
            &[silence(
                Some("zfs.pool.member"),
                Some("mox2/rpool"),
                Some(2)
            )],
            50
        ));
        assert!(
            !silenced(alert, &[silence(Some("zfs.pool"), None, None)], 50),
            "no dot, exact only"
        );
        assert!(!silenced(alert, &[silence(None, None, Some(1))], 50));
        assert!(!silenced(alert, &[silence(None, None, None)], 100), "ended");
    }

    #[test]
    fn switched_off_conditions_close_quietly_and_match_by_category() {
        let off = vec!["zfs.".to_string(), "backup.skipped".to_string()];
        assert!(switched_off(&off, "zfs.pool_degraded"));
        assert!(switched_off(&off, "backup.skipped"));
        assert!(!switched_off(&off, "backup.skipped_twice"));
        assert!(!switched_off(&off, "disk.smart"));
        let mut book = Book::default();
        book.evaluate(&[critical("mox2/rpool")], MIN, true);
        book.switch_off(&off, 2 * MIN);
        assert_eq!(book.alerts[0].state, AlertState::Resolved);
        let (changes, _) = book.evaluate(&[], 3 * MIN, true);
        assert!(
            changes.is_empty(),
            "no recovery message for a switched-off alert"
        );
    }

    #[test]
    fn local_alerts_merge_without_duplicates() {
        let mut cluster = Book::default();
        cluster.evaluate(&[critical("mox2/rpool")], 0, true);
        let mut local = Book::default();
        local.evaluate(&[critical("mox2/rpool"), critical("mox3/rpool")], 1, true);
        cluster.merge(local);
        assert_eq!(cluster.alerts.len(), 2);
    }

    #[test]
    fn long_messages_are_trimmed() {
        let mut book = Book::default();
        let mut long = critical("mox1/rpool");
        long.message = "x".repeat(1000);
        book.evaluate(&[long], 0, true);
        assert_eq!(book.alerts[0].message.chars().count(), MAX_MESSAGE + 3);
    }
}
