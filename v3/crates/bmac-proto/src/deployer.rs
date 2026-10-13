// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

//! What the Deployer's window and its Rust side exchange through Tauri
//! commands and events (`DEPLOYMENT.md`, "BMAC Deployer"). The Deployer
//! never calls `bmacd`'s API; these types stay on the workstation.
//!
//! Long work (a discovery session, an ISO build, a handoff) runs as a
//! **task**: the same phases, requests, output, and result as a `bmacd` job,
//! raised by the Deployer's own code, so the window shows it with the shared
//! job view.

use std::collections::BTreeMap;

use bmac_core::job::{Answer, EventBody, JobState};
use serde::{Deserialize, Serialize};
use ts_rs::TS;

pub use crate::inventory::LinkWait;

/// `get_app_info`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct DeployerInfo {
    pub name: String,
    pub version: String,
    /// No settings file existed when it started.
    pub first_launch: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
pub enum CheckStatus {
    Ok,
    Warning,
    Error,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct CheckLink {
    pub label: String,
    pub url: String,
}

/// One workstation check (`run_preflight`). None of them contacts a
/// cluster or a target machine.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct Check {
    pub id: String,
    pub label: String,
    pub status: CheckStatus,
    pub detail: String,
    /// What to do about a warning or error.
    pub fix: Option<String>,
    /// A shell command that installs what is missing, offered for copying.
    pub command: Option<String>,
    /// One sentence on what the Deployer uses it for.
    pub purpose: Option<String>,
    pub link: Option<CheckLink>,
}

/// `start_task`: the workflows the window can start. Each asks for what it
/// needs through its requests.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
pub enum TaskKind {
    /// Boot a machine into the discovery OS and read its hardware.
    Discovery,
    /// Build a host's ISO, give the boot steps, and hand the host over.
    Host,
    /// Pin SSH to a host this workstation didn't install, then to the
    /// cluster's other hosts.
    HostSsh,
    /// Pin SSH to a guest, jumping through a host.
    GuestSsh,
    /// Deploy the static hello app to a production guest.
    Hello,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
pub enum IsoKind {
    Discovery,
    Host,
    Appliance,
}

/// An ISO the Deployer built (`DEPLOYMENT.md`, "ISO lifecycle"). Each holds
/// a private key and a single-use token.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct BuiltIso {
    pub path: String,
    pub kind: IsoKind,
    /// The host slot, appliance name, or discovery session it is for.
    pub target: String,
    pub built_ms: i64,
}

/// A managed entry in the workstation's `~/.ssh/config` (`DEPLOYMENT.md`,
/// "Workstation SSH").
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(optional_fields)]
pub struct SshEntry {
    /// `host` or `guest`.
    pub kind: String,
    /// The `HostKeyAlias` the key is pinned under, `bmac-<cluster>-<name>`.
    pub alias: String,
    /// The names `ssh` accepts for it.
    pub names: Vec<String>,
    pub address: String,
    pub jump: Option<String>,
    /// What is wrong with it, as far as the workstation's own files show.
    pub problem: Option<String>,
}

/// The Deployer's own settings, kept in its config directory. It holds no
/// cluster identity, join token, or secret.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(default)]
pub struct DeployerSettings {
    pub link_wait: LinkWait,
    pub isos: Vec<BuiltIso>,
    /// Desktop notifications when a task needs the operator or finishes
    /// while the window isn't focused.
    pub notifications: bool,
    /// The ISO discovery ISOs are built from, when the operator chose one
    /// instead of the pinned Alpine Extended release.
    #[ts(optional)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub discovery_iso: Option<SourceIso>,
}

impl Default for DeployerSettings {
    fn default() -> DeployerSettings {
        DeployerSettings {
            link_wait: LinkWait::default(),
            isos: Vec::new(),
            notifications: true,
            discovery_iso: None,
        }
    }
}

/// A download the Deployer verifies by its SHA-256 before using it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct SourceIso {
    /// A plain https URL.
    pub url: String,
    /// 64 lowercase hex digits.
    pub sha256: String,
}

/// A built ISO as listed, with whether the file is still there.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct IsoView {
    pub iso: BuiltIso,
    pub present: bool,
}

/// `get_settings` and `update_settings`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct DeployerSettingsView {
    pub settings: DeployerSettings,
    pub isos: Vec<IsoView>,
    pub path: String,
    /// The pinned discovery source used when none is chosen.
    pub default_discovery_iso: SourceIso,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct TaskSummary {
    pub id: String,
    /// What kind of work, such as `discovery` or `host_iso`.
    pub kind: String,
    pub title: String,
    pub state: JobState,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
    /// The request waiting for an answer, if any.
    pub pending_request: Option<String>,
    /// Why it is in its current state.
    pub message: Option<String>,
    pub cancel_requested: bool,
    pub last_seq: u64,
}

/// One event of a task, numbered from 1 without gaps.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
pub struct TaskEvent {
    pub task: String,
    pub seq: u64,
    pub at_ms: i64,
    #[serde(flatten)]
    pub body: EventBody,
}

/// `get_task`: a task with every event so far.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
pub struct TaskView {
    pub task: TaskSummary,
    pub parameters: BTreeMap<String, String>,
    pub events: Vec<TaskEvent>,
}

/// `answer_task`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(deny_unknown_fields)]
pub struct AnswerTask {
    pub task: String,
    pub request_id: String,
    pub answer: Answer,
}

/// The Tauri event names the window listens to.
pub mod events {
    /// A [`super::TaskEvent`].
    pub const TASK_EVENT: &str = "bmac://task-event";
    /// A [`super::TaskSummary`] that changed.
    pub const TASK_UPDATED: &str = "bmac://task-updated";
    /// The window was asked to close while tasks run; carries their count.
    pub const CLOSE_REQUESTED: &str = "bmac://close-requested";
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn task_events_flatten_their_body_like_job_events() {
        let event = TaskEvent {
            task: "t1".into(),
            seq: 3,
            at_ms: 5,
            body: EventBody::Warning {
                message: "slow link".into(),
            },
        };
        let wire = serde_json::to_value(&event).unwrap();
        assert_eq!(
            wire,
            json!({"task": "t1", "seq": 3, "at_ms": 5, "type": "warning", "message": "slow link"})
        );
        assert_eq!(serde_json::from_value::<TaskEvent>(wire).unwrap(), event);
    }

    #[test]
    fn settings_fill_in_what_an_older_file_lacks() {
        let settings: DeployerSettings =
            serde_json::from_value(json!({"notifications": false})).unwrap();
        assert_eq!(settings.link_wait, LinkWait::default());
        assert!(!settings.notifications);
    }
}
