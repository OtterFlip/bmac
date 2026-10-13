// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

//! Alerts, silences, and email notifications (`MONITORING.md`, "Alerts" and
//! "Notifications"). `GET /api/v1/alerts` answers a [`NotifyView`]; the
//! other calls acknowledge an alert, manage silences, and change the
//! notification settings, the Postmark token, and the email templates.

use bmac_core::HostSlot;
use serde::{Deserialize, Serialize};
use ts_rs::TS;

use crate::status::{ConditionSeverity, NextStep};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
pub enum AlertState {
    Open,
    Acknowledged,
    Resolved,
}

/// One condition on one resource that stayed at warning or critical for its
/// hold time.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct Alert {
    pub id: String,
    /// The condition's kind, such as `zfs.pool.member`.
    pub condition: String,
    pub resource: String,
    /// The host the resource is on, when it is on one.
    pub host: Option<HostSlot>,
    pub severity: ConditionSeverity,
    pub message: String,
    pub next_step: Option<NextStep>,
    pub state: AlertState,
    pub opened_ms: i64,
    pub resolved_ms: Option<i64>,
    pub acknowledged_by: Option<String>,
    pub acknowledged_ms: Option<i64>,
    /// Counts reopenings and escalations; part of each notification's
    /// idempotency key.
    pub generation: u32,
    /// When an opening notification was last queued, if ever; a recovery
    /// is sent only for a notified alert.
    pub notified_ms: Option<i64>,
}

/// Suppresses notifications for matching alerts until `until_ms`. An absent
/// field matches everything; `condition` ending in `.` matches that prefix.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct Silence {
    pub id: String,
    pub condition: Option<String>,
    pub resource: Option<String>,
    pub host: Option<HostSlot>,
    pub until_ms: i64,
    pub reason: String,
    pub created_by: String,
    pub created_ms: i64,
}

/// Which alerts are emailed to whom.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(deny_unknown_fields)]
pub struct Route {
    /// Condition categories, the part of the kind before the first `.`
    /// (`zfs`, `disk`, `corosync`); empty matches every category.
    pub categories: Vec<String>,
    pub min_severity: ConditionSeverity,
    /// Empty means the default recipients.
    pub recipients: Vec<String>,
    /// Send a message when the alert resolves.
    pub recovery: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(deny_unknown_fields)]
pub struct NotifySettings {
    /// A confirmed Sender Signature, or an address on a verified domain, of
    /// the Postmark account; no email is attempted without one.
    pub sender: Option<String>,
    pub recipients: Vec<String>,
    /// Empty means one route: warnings and criticals of every category to
    /// the default recipients, with recovery messages.
    pub routes: Vec<Route>,
    /// At most one opening message per alert in this many minutes.
    pub throttle_minutes: u32,
    /// The hour of the daily digest in the cluster's time zone; absent for
    /// none.
    pub digest_hour: Option<u8>,
    /// The Dashboard address put in emails, for example
    /// `https://mox1.tailnet.ts.net:8443/`.
    pub dashboard_url: Option<String>,
    /// Conditions switched off: an exact condition (`backup.skipped`) or a
    /// category ending in `.` (`zfs.`). They still show on their resource's
    /// page but open no alert and send nothing.
    #[serde(default)]
    pub disabled_conditions: Vec<String>,
}

impl Default for NotifySettings {
    fn default() -> NotifySettings {
        NotifySettings {
            sender: None,
            recipients: Vec::new(),
            routes: Vec::new(),
            throttle_minutes: 60,
            digest_hour: None,
            dashboard_url: None,
            disabled_conditions: Vec::new(),
        }
    }
}

/// A message that could not be delivered.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct OutboxProblem {
    pub recipient: String,
    pub template: String,
    pub error: String,
    pub at_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct OutboxSummary {
    /// Waiting to be sent, including retries.
    pub pending: u32,
    /// Held until the email templates exist on the Postmark server.
    pub held: u32,
    pub last_sent_ms: Option<i64>,
    /// The newest undelivered messages.
    pub problems: Vec<OutboxProblem>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct NotifyView {
    /// Open, acknowledged, and recently resolved alerts, newest first.
    pub alerts: Vec<Alert>,
    /// The newest resolved alerts.
    pub history: Vec<Alert>,
    pub silences: Vec<Silence>,
    pub settings: NotifySettings,
    pub settings_revision: Option<u64>,
    /// Whether a Postmark server token is stored; the token itself is never
    /// shown.
    pub token_set: bool,
    pub outbox: OutboxSummary,
    /// Without quorum, this host's own alerts, merged back when quorum
    /// returns.
    pub local: bool,
    /// The caller may acknowledge alerts and manage silences (operators
    /// and admins).
    pub can_acknowledge: bool,
    /// The caller may change the notification settings (admins).
    pub can_edit: bool,
}

/// The body of `POST /api/v1/alerts/{id}/acknowledge` is empty; this names
/// the alert over the engine socket.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(deny_unknown_fields)]
pub struct AcknowledgeAlert {
    pub id: String,
}

/// The body of `POST /api/v1/silences`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(deny_unknown_fields)]
pub struct SaveSilence {
    pub condition: Option<String>,
    pub resource: Option<String>,
    pub host: Option<HostSlot>,
    pub until_ms: i64,
    pub reason: String,
}

/// The body of `PUT /api/v1/notify/settings`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(deny_unknown_fields)]
pub struct SaveNotifySettings {
    pub settings: NotifySettings,
    pub expected_revision: Option<u64>,
}

/// The body of `PUT /api/v1/notify/token`: the Postmark server token, or
/// empty to remove it. It is written to `/etc/pve/priv` and never read back.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(deny_unknown_fields)]
pub struct SetPostmarkToken {
    pub token: String,
}

impl std::fmt::Debug for SetPostmarkToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SetPostmarkToken { token: <hidden> }")
    }
}

/// The body of `POST /api/v1/notify/test`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(deny_unknown_fields)]
pub struct SendTestEmail {
    /// One address, or absent for the default recipients.
    pub to: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct TestEmailResult {
    pub recipient: String,
    pub message_id: Option<String>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
pub enum TemplatePresence {
    /// On the server with the content this version embeds.
    Present,
    Missing,
    /// On the server with other content; reported, never overwritten.
    Differs,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct TemplateStatus {
    pub alias: String,
    pub presence: TemplatePresence,
}

/// The answer of `GET /api/v1/notify/templates` and of creating the
/// missing ones.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct TemplatesView {
    pub server_name: String,
    /// `Live` or `Sandbox`; a sandbox server accepts mail and delivers none.
    pub delivery_type: String,
    pub templates: Vec<TemplateStatus>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_token_is_not_debug_printed() {
        let request = SetPostmarkToken {
            token: "secret-value".into(),
        };
        assert!(!format!("{request:?}").contains("secret-value"));
    }
}
