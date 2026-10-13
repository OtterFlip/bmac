// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

//! Jobs and their events (`ARCHITECTURE.md`, "Jobs and the event model").
//!
//! The request, plan, and field types are v2's `bmac-ui` shapes
//! (`dashboard/engine/src/protocol.rs` 20–168), shared with `bmac-proto` so a
//! script operation's events map onto job events without conversion.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use ts_rs::TS;

use crate::{HostSlot, JobId, OperationId};

/// The longest text kept in one event field; longer text is truncated.
pub const MAX_TEXT: usize = 64 * 1024;
pub const MAX_FIELDS: usize = 64;
pub const MAX_OPTIONS: usize = 1000;
/// Shown in place of a secret answer wherever answers are recorded.
pub const REDACTED: &str = "[redacted]";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(optional_fields)]
#[serde(rename_all = "snake_case")]
pub enum FieldType {
    String,
    Multiline,
    Integer,
    Number,
    Boolean,
    Password,
    Select,
    Multiselect,
    Hostname,
    IpAddress,
    Cidr,
    MacAddress,
    Path,
    File,
    Directory,
    SshPublicKey,
    Duration,
    Bytes,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[ts(optional_fields)]
pub struct FieldOption {
    pub value: String,
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub help: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[ts(optional_fields)]
pub struct FileFilter {
    pub name: String,
    pub extensions: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[ts(optional_fields)]
pub struct Field {
    pub id: String,
    #[serde(rename = "type")]
    pub kind: FieldType,
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub help: Option<String>,
    #[serde(default)]
    pub required: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub placeholder: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub suffix: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_selected: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_selected: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pattern: Option<String>,
    #[serde(default)]
    pub sensitive: bool,
    #[serde(default)]
    pub disabled: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[ts(optional = nullable)]
    pub options: Vec<FieldOption>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[ts(optional = nullable)]
    pub filters: Vec<FileFilter>,
}

impl Field {
    pub fn is_sensitive(&self) -> bool {
        self.sensitive || self.kind == FieldType::Password
    }

    /// v2's `validate_field`: a safe ID, passwords always sensitive, no
    /// default ever kept on a sensitive field, and options where needed.
    pub fn normalize(&mut self) -> Result<(), String> {
        check_id("field id", &self.id)?;
        if self.kind == FieldType::Password {
            self.sensitive = true;
        }
        if self.is_sensitive() {
            self.default = None;
        }
        if self.options.len() > MAX_OPTIONS {
            return Err(format!("field {} has too many options", self.id));
        }
        if matches!(self.kind, FieldType::Select | FieldType::Multiselect)
            && self.options.is_empty()
        {
            return Err(format!("field {} has no options", self.id));
        }
        self.label = truncate(std::mem::take(&mut self.label));
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default, TS)]
#[ts(optional_fields)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    #[default]
    Normal,
    Warning,
    Destructive,
    Critical,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(optional_fields)]
#[serde(rename_all = "snake_case")]
pub enum PhaseStatus {
    Pending,
    Running,
    Complete,
    Failed,
    Skipped,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default, TS)]
#[ts(optional_fields)]
#[serde(rename_all = "snake_case")]
pub enum LogLevel {
    Debug,
    #[default]
    Info,
    Warning,
    Error,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(optional_fields)]
#[serde(rename_all = "snake_case")]
pub enum PlanKind {
    Create,
    Update,
    Remove,
    Keep,
    Check,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[ts(optional_fields)]
pub struct PlanItem {
    pub kind: PlanKind,
    pub target: String,
    pub description: String,
}

/// One manual action step: plain text, or text with a value the Dashboard
/// offers to copy. `run_on` names the host where the operator runs it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[ts(optional_fields)]
#[serde(untagged)]
pub enum Instruction {
    Text(String),
    WithCopy {
        text: String,
        copy: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        run_on: Option<String>,
    },
}

/// What a request asks of the operator.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[ts(optional_fields)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RequestForm {
    Input {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        title: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        description: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        context: Option<String>,
        field: Field,
    },
    InputGroup {
        title: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        description: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        context: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        layout: Option<String>,
        fields: Vec<Field>,
    },
    Confirm {
        title: String,
        #[serde(default)]
        message: String,
        #[serde(default)]
        severity: Severity,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        confirm_label: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cancel_label: Option<String>,
        /// When set, the operator must type this phrase to confirm.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        confirmation_text: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        context: Option<String>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        #[ts(optional = nullable)]
        details: Vec<String>,
    },
    ManualAction {
        title: String,
        instructions: Vec<Instruction>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        acknowledge_label: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        context: Option<String>,
    },
}

impl RequestForm {
    pub fn fields(&self) -> &[Field] {
        match self {
            RequestForm::Input { field, .. } => std::slice::from_ref(field),
            RequestForm::InputGroup { fields, .. } => fields,
            _ => &[],
        }
    }

    pub fn normalize(&mut self) -> Result<(), String> {
        match self {
            RequestForm::Input { field, .. } => field.normalize(),
            RequestForm::InputGroup { fields, .. } => {
                if fields.is_empty() || fields.len() > MAX_FIELDS {
                    return Err(format!("an input group needs 1 to {MAX_FIELDS} fields"));
                }
                let mut seen = BTreeSet::new();
                for field in fields.iter_mut() {
                    field.normalize()?;
                    if !seen.insert(field.id.clone()) {
                        return Err(format!("duplicate field id {}", field.id));
                    }
                }
                Ok(())
            }
            _ => Ok(()),
        }
    }
}

/// An operator's answer to a pending request.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[ts(optional_fields)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Answer {
    Values { values: BTreeMap<String, Value> },
    Confirm { confirmed: bool },
    Acknowledge,
    Cancel,
}

/// A job's state in its record.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(optional_fields)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum JobState {
    /// Admitted and waiting for its target host to claim it.
    Admitted,
    Running,
    Completed,
    Failed,
    Cancelled,
    /// Its lease expired; an operator resumes it or reconciliation fails it.
    Interrupted,
}

impl JobState {
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            JobState::Completed | JobState::Failed | JobState::Cancelled
        )
    }
}

/// One event in a job's stream. `seq` starts at 1 and has no gaps within a
/// job, so a reader can resume after the last sequence it saw.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[ts(optional_fields)]
pub struct JobEvent {
    pub job: JobId,
    pub seq: u64,
    /// Milliseconds since the Unix epoch, on the host that recorded it.
    pub at_ms: i64,
    #[serde(flatten)]
    pub body: EventBody,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[ts(optional_fields)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum EventBody {
    Started {
        operation: OperationId,
        host: HostSlot,
        user: String,
        #[serde(default)]
        dry_run: bool,
    },
    Phase {
        id: String,
        label: String,
        status: PhaseStatus,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cancel_allowed: Option<bool>,
    },
    Plan {
        title: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        description: Option<String>,
        items: Vec<PlanItem>,
        #[serde(default)]
        dry_run: bool,
    },
    Request {
        request_id: String,
        #[serde(flatten)]
        form: Box<RequestForm>,
    },
    /// The answer was accepted. Values are recorded with secrets redacted.
    Answered {
        request_id: String,
        by: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        values: Option<BTreeMap<String, Value>>,
    },
    /// The answer was refused; the same request stays pending.
    ValidationError {
        request_id: String,
        #[serde(default)]
        field_errors: BTreeMap<String, String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        message: Option<String>,
    },
    Progress {
        message: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        current: Option<f64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        total: Option<f64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        unit: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        phase: Option<String>,
    },
    Output {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        stream: Option<String>,
        #[serde(default)]
        level: LogLevel,
        text: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        source: Option<String>,
    },
    Warning {
        message: String,
    },
    Result {
        data: Value,
    },
    NextStep {
        text: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        operation: Option<OperationId>,
        #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
        #[ts(optional = nullable)]
        args: BTreeMap<String, String>,
    },
    Completed {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        message: Option<String>,
    },
    Failed {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        code: Option<String>,
        message: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        details: Option<String>,
    },
    Cancelled {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        by: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        message: Option<String>,
    },
    Interrupted {
        message: String,
    },
}

impl EventBody {
    /// The state a job enters with this event, for the four terminal events
    /// and `Started`.
    pub fn state(&self) -> Option<JobState> {
        Some(match self {
            EventBody::Started { .. } => JobState::Running,
            EventBody::Completed { .. } => JobState::Completed,
            EventBody::Failed { .. } => JobState::Failed,
            EventBody::Cancelled { .. } => JobState::Cancelled,
            EventBody::Interrupted { .. } => JobState::Interrupted,
            _ => return None,
        })
    }

    /// v2's event validation: safe IDs, normalized request fields, and long
    /// text truncated.
    pub fn normalize(&mut self) -> Result<(), String> {
        match self {
            EventBody::Request { request_id, form } => {
                check_id("request id", request_id)?;
                form.normalize()
            }
            EventBody::Answered { request_id, .. }
            | EventBody::ValidationError { request_id, .. } => check_id("request id", request_id),
            EventBody::Phase { id, .. } => check_id("phase id", id),
            EventBody::Output { text, .. } => {
                *text = truncate(std::mem::take(text));
                Ok(())
            }
            EventBody::Warning { message } | EventBody::Interrupted { message } => {
                *message = truncate(std::mem::take(message));
                Ok(())
            }
            _ => Ok(()),
        }
    }
}

/// v2's `is_identifier` (letters, digits, `_-.`, up to 64), or up to 128 of
/// letters, digits, and `_-.:` (`protocol.rs` 395–401).
pub fn is_event_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_-.:".contains(&b))
}

fn check_id(kind: &str, id: &str) -> Result<(), String> {
    if is_event_id(id) {
        Ok(())
    } else {
        Err(format!("unsafe {kind} {id:?}"))
    }
}

/// Cuts text to [`MAX_TEXT`] bytes on a character boundary and marks it.
pub fn truncate(mut text: String) -> String {
    if text.len() > MAX_TEXT {
        let mut end = MAX_TEXT;
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        text.truncate(end);
        text.push_str(" ...(truncated)");
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn event(body: Value) -> JobEvent {
        let mut value =
            json!({"job": "6f9619ff-8b86-4011-b42d-00c04fc964ff", "seq": 1, "at_ms": 0});
        value
            .as_object_mut()
            .unwrap()
            .extend(body.as_object().unwrap().clone());
        serde_json::from_value(value).unwrap()
    }

    #[test]
    fn events_round_trip_through_json() {
        let cases = [
            json!({"type": "started", "operation": "add_prod_vm", "host": "mox2", "user": "alice@pve", "dry_run": false}),
            json!({"type": "phase", "id": "luks", "label": "Encrypt", "status": "running", "cancel_allowed": false}),
            json!({"type": "request", "request_id": "r1", "kind": "confirm", "title": "Wipe?", "message": "",
                   "severity": "destructive", "confirmation_text": "wipe mox3"}),
            json!({"type": "answered", "request_id": "r1", "by": "alice@pve"}),
            json!({"type": "output", "stream": "stdout", "level": "info", "text": "hi"}),
            json!({"type": "next_step", "text": "Add a disk", "operation": "storage.add_vdev", "args": {"host": "mox1"}}),
            json!({"type": "failed", "message": "boom"}),
            json!({"type": "interrupted", "message": "mox2 stopped renewing its lease"}),
        ];
        for case in cases {
            let parsed = event(case.clone());
            let back = serde_json::to_value(&parsed).unwrap();
            for (key, value) in case.as_object().unwrap() {
                assert_eq!(&back[key], value, "{key} in {case}");
            }
        }
    }

    #[test]
    fn terminal_events_set_the_state() {
        let failed = event(json!({"type": "failed", "message": "x"}));
        assert_eq!(failed.body.state(), Some(JobState::Failed));
        assert!(JobState::Failed.is_terminal());
        assert!(
            !JobState::Interrupted.is_terminal(),
            "an interrupted job can still be resumed"
        );
        assert_eq!(
            event(json!({"type": "warning", "message": "x"}))
                .body
                .state(),
            None
        );
        assert_eq!(
            serde_json::to_string(&JobState::Interrupted).unwrap(),
            "\"INTERRUPTED\""
        );
    }

    #[test]
    fn secrets_lose_their_defaults() {
        let mut body = event(
            json!({"type": "request", "request_id": "r1", "kind": "input",
            "field": {"id": "k", "type": "password", "label": "Key", "default": "oops"}}),
        )
        .body;
        body.normalize().unwrap();
        let EventBody::Request { form, .. } = body else {
            panic!()
        };
        assert!(form.fields()[0].sensitive && form.fields()[0].default.is_none());
    }

    #[test]
    fn unsafe_or_malformed_requests_are_refused() {
        let mut bad_id =
            event(json!({"type": "phase", "id": "a b", "label": "x", "status": "running"})).body;
        assert!(bad_id.normalize().is_err());
        let mut no_options = event(
            json!({"type": "request", "request_id": "r1", "kind": "input",
            "field": {"id": "x", "type": "select", "label": "X"}}),
        )
        .body;
        assert!(no_options.normalize().is_err());
        let field = json!({"id": "x", "type": "string", "label": "X"});
        let mut duplicate = event(
            json!({"type": "request", "request_id": "r1", "kind": "input_group",
            "title": "T", "fields": [field, field]}),
        )
        .body;
        assert!(duplicate.normalize().unwrap_err().contains("duplicate"));
        let mut empty = event(json!({"type": "request", "request_id": "r1", "kind": "input_group", "title": "T", "fields": []})).body;
        assert!(empty.normalize().is_err());
    }

    #[test]
    fn long_output_is_truncated_on_a_character_boundary() {
        let mut body = EventBody::Output {
            stream: None,
            level: LogLevel::Info,
            text: "é".repeat(MAX_TEXT),
            source: None,
        };
        body.normalize().unwrap();
        let EventBody::Output { text, .. } = body else {
            panic!()
        };
        assert!(text.ends_with(" ...(truncated)"));
        assert!(text.len() <= MAX_TEXT + 16);
    }

    #[test]
    fn manual_action_instructions_may_carry_a_copy_value() {
        let body = event(json!({"type": "request", "request_id": "m1", "kind": "manual_action", "title": "T",
            "instructions": ["Open the console", {"text": "Run:", "copy": "bmac luks-enter", "run_on": "mox3"}]}))
        .body;
        let EventBody::Request { form, .. } = body else {
            panic!()
        };
        let RequestForm::ManualAction { instructions, .. } = *form else {
            panic!()
        };
        assert_eq!(
            instructions[0],
            Instruction::Text("Open the console".into())
        );
        assert!(
            matches!(&instructions[1], Instruction::WithCopy { run_on: Some(h), .. } if h == "mox3")
        );
    }

    #[test]
    fn answers_use_the_v2_shapes() {
        let answer: Answer =
            serde_json::from_value(json!({"kind": "values", "values": {"memory": 16}})).unwrap();
        assert!(matches!(answer, Answer::Values { .. }));
        assert_eq!(
            serde_json::to_value(Answer::Acknowledge).unwrap(),
            json!({"kind": "acknowledge"})
        );
    }
}
