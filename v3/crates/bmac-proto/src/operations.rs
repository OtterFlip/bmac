// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

//! The operation registry (`ARCHITECTURE.md`, "Jobs and the event model"):
//! every operation a job can run, with its typed parameters, minimum role,
//! resource locks, target host rule, and cancel and resume rules.
//! `GET /api/v1/operations` serves it. Script paths and command-line flags
//! are read from the registry file but never sent to the browser.

use bmac_core::OperationId;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use ts_rs::TS;

use crate::api::Role;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, TS)]
#[ts(optional_fields)]
#[serde(rename_all = "snake_case")]
pub enum Category {
    Production,
    Staging,
    Hosts,
    Qdevice,
    Storage,
    Backups,
    Cluster,
    Diagnostics,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(optional_fields)]
#[serde(rename_all = "snake_case")]
pub enum OperationKind {
    /// An adapted v2 script run under the supervisor.
    Script,
    /// Rust code calling typed adapters.
    Native,
}

/// Parameter types (v2's `ParamType`, less `path`: the browser never
/// supplies a path).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(optional_fields)]
#[serde(rename_all = "snake_case")]
pub enum ParamType {
    /// On or off.
    Flag,
    /// One of `choices`, each passed as its own option.
    FlagChoice,
    /// One of `choices`, passed as the value.
    Choice,
    /// A host slot, `moxN`.
    Host,
    /// `prodN`.
    Production,
    /// `stageMprodN`.
    Staging,
    /// `prodN` or `stageMprodN`.
    Guest,
    /// A positive whole number.
    Integer,
    /// A DNS host name.
    Hostname,
}

impl ParamType {
    /// Types that name a host or guest, usable in lock and target rules.
    pub fn names_a_resource(self) -> bool {
        matches!(
            self,
            ParamType::Host | ParamType::Production | ParamType::Staging | ParamType::Guest
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(optional_fields)]
#[serde(deny_unknown_fields)]
pub struct ParamChoice {
    pub value: String,
    pub label: String,
    #[serde(default, skip_serializing)]
    #[ts(skip)]
    pub flag: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(optional_fields)]
#[serde(rename_all = "snake_case")]
pub enum ParamSuggest {
    /// Host slots that are not cluster members.
    FreeHostSlots,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[ts(optional_fields)]
#[serde(deny_unknown_fields)]
pub struct Param {
    pub id: String,
    pub label: String,
    #[serde(rename = "type")]
    pub kind: ParamType,
    /// The script's option, such as `--host`; without one the value is
    /// positional. Never sent to the browser.
    #[serde(default, skip_serializing)]
    #[ts(skip)]
    pub flag: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub help: Option<String>,
    #[serde(default)]
    pub required: bool,
    #[serde(default)]
    pub advanced: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub placeholder: Option<String>,
    #[serde(default)]
    pub choices: Vec<ParamChoice>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub suggest: Option<ParamSuggest>,
}

/// Which host runs the job.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(optional_fields)]
#[serde(tag = "rule", rename_all = "snake_case", deny_unknown_fields)]
pub enum TargetRule {
    /// Any online host; the admitting host runs it.
    Any {},
    /// The cluster duty host.
    Duty {},
    /// The host named by a `host` parameter.
    Host { param: String },
    /// Any online host except the one named by a `host` parameter.
    AnyExcept { param: String },
    /// The host that owns the guest named by a parameter.
    Owner { param: String },
    /// The standby placement host of the production guest named by a
    /// parameter.
    Standby { param: String },
    /// An online placement host for a new production guest, preferring its
    /// initial owner.
    Placement {},
}

/// When a running job may be cancelled.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(optional_fields)]
#[serde(tag = "rule", rename_all = "snake_case", deny_unknown_fields)]
pub enum CancelRule {
    /// The script says, phase by phase (`cancel_allowed` in the `bmac-ui`
    /// protocol).
    Script {},
    /// Always, except during the named phases.
    Phases {
        #[serde(default)]
        not_during: Vec<String>,
    },
}

/// What happens when a job is interrupted because its host stopped.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(optional_fields)]
#[serde(rename_all = "snake_case")]
pub enum ResumeRule {
    /// An operator inspects the live state; nothing is retried.
    Never,
    /// It continues from its last recorded checkpoint when resumed.
    Checkpoint,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[ts(optional_fields)]
#[serde(deny_unknown_fields)]
pub struct Operation {
    pub id: OperationId,
    pub title: String,
    pub summary: String,
    pub description: String,
    pub category: Category,
    pub kind: OperationKind,
    /// The script, relative to the legacy runtime. Never sent to the browser.
    #[serde(default, skip_serializing)]
    #[ts(skip)]
    pub script: Option<String>,
    pub min_role: Role,
    /// Changes something. Read-only operations take no locks.
    pub mutating: bool,
    /// Destroys data or removes something; administrators only, with the
    /// confirmations the operation asks for.
    pub destructive: bool,
    /// The launch dialog offers a dry run, on by default.
    #[serde(default)]
    pub supports_dry_run: bool,
    #[serde(default)]
    pub params: Vec<Param>,
    /// Resources held while the job runs, as templates: `production:{resource}`
    /// names the guest in the `resource` parameter, and `{name?}` drops the
    /// part when the parameter is left out, leaving the whole class.
    #[serde(default)]
    pub locks: Vec<String>,
    pub target: TargetRule,
    pub cancel: CancelRule,
    pub resume: ResumeRule,
    /// For maintainers: how the operation differs from v2. Never sent to the
    /// browser.
    #[serde(default, skip_serializing)]
    #[ts(skip)]
    pub notes: Option<String>,
}

/// An operation as one user sees it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[ts(optional_fields)]
pub struct OperationView {
    #[serde(flatten)]
    pub operation: Operation,
    /// Whether this user's role may start it.
    pub allowed: bool,
}

/// `GET /api/v1/operations`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[ts(optional_fields)]
pub struct OperationList {
    pub operations: Vec<OperationView>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn scripts_and_flags_stay_on_the_server() {
        let operation: Operation = serde_json::from_value(json!({
            "id": "prod.remove", "title": "Remove", "summary": "s", "description": "d",
            "category": "production", "kind": "script", "script": "scripts/remove_prod_vm.sh",
            "min_role": "admin", "mutating": true, "destructive": true,
            "params": [{"id": "resource", "label": "VM", "type": "production", "flag": "--resource", "required": true}],
            "target": {"rule": "owner", "param": "resource"},
            "cancel": {"rule": "script"}, "resume": "never"
        }))
        .unwrap();
        assert_eq!(
            operation.script.as_deref(),
            Some("scripts/remove_prod_vm.sh")
        );
        assert_eq!(operation.params[0].flag.as_deref(), Some("--resource"));
        let sent = serde_json::to_string(&operation).unwrap();
        assert!(
            !sent.contains("scripts/") && !sent.contains("--resource"),
            "{sent}"
        );
        assert!(sent.contains(r#""target":{"rule":"owner","param":"resource"}"#));
    }

    #[test]
    fn unknown_fields_are_refused() {
        let extra = json!({"rule": "any", "host": "mox1"});
        assert!(serde_json::from_value::<TargetRule>(extra).is_err());
        let extra = json!({"rule": "script", "not_during": []});
        assert!(serde_json::from_value::<CancelRule>(extra).is_err());
        let param = json!({"id": "p", "label": "P", "type": "path"});
        assert!(
            serde_json::from_value::<Param>(param).is_err(),
            "no path parameters"
        );
    }
}
