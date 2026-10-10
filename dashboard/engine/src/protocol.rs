//! bmac-ui v2 protocol types. Every line a script writes is parsed into
//! [`WorkflowEvent`] and validated before it reaches the frontend; unknown
//! keys are dropped and anything that does not fit is rejected.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::registry::is_identifier;

pub const PROTOCOL_NAME: &str = "bmac-ui";
pub const PROTOCOL_VERSION: u32 = 2;

const MAX_LINE_BYTES: usize = 4 * 1024 * 1024;
const MAX_TEXT: usize = 64 * 1024;
const MAX_FIELDS: usize = 64;
const MAX_OPTIONS: usize = 1000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
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

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FieldOption {
    pub value: String,
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub help: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FileFilter {
    pub name: String,
    pub extensions: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
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
    pub options: Vec<FieldOption>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub filters: Vec<FileFilter>,
}

impl Field {
    pub fn is_sensitive(&self) -> bool {
        self.sensitive || self.kind == FieldType::Password
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    #[default]
    Normal,
    Warning,
    Destructive,
    Critical,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PhaseStatus {
    Pending,
    Running,
    Complete,
    Failed,
    Skipped,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum LogLevel {
    Debug,
    #[default]
    Info,
    Warning,
    Error,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CompletedStatus {
    Success,
    Failed,
    Cancelled,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlanKind {
    Create,
    Update,
    Remove,
    Keep,
    Check,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PlanItem {
    pub kind: PlanKind,
    pub target: String,
    pub description: String,
}

/// One manual action step: plain text, or text with a value the dashboard
/// offers to copy to the clipboard. `run_on` names the host where the
/// operator must run the script whose path is `copy`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
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

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum WorkflowEvent {
    Protocol {
        protocol: String,
        version: u32,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        bmac_version: Option<String>,
    },
    WorkflowStarted {
        workflow: String,
        run_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        script: Option<String>,
        #[serde(default)]
        argv: Vec<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pid: Option<u32>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        started_at: Option<String>,
    },
    WorkflowMetadata {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        workflow: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        title: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        description: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        category: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        destructive: Option<bool>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        supports_dry_run: Option<bool>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        supports_cancel: Option<bool>,
    },
    Input {
        request_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        title: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        description: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        context: Option<String>,
        field: Field,
    },
    InputGroup {
        request_id: String,
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
        request_id: String,
        title: String,
        #[serde(default)]
        message: String,
        #[serde(default)]
        severity: Severity,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        confirm_label: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cancel_label: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        confirmation_text: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        context: Option<String>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        details: Vec<String>,
    },
    ManualAction {
        request_id: String,
        title: String,
        instructions: Vec<Instruction>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        acknowledge_label: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        context: Option<String>,
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
    Info {
        message: String,
    },
    Warning {
        message: String,
    },
    Error {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        code: Option<String>,
        message: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        details: Option<String>,
        #[serde(default)]
        recoverable: bool,
    },
    ValidationError {
        request_id: String,
        #[serde(default)]
        field_errors: BTreeMap<String, String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        message: Option<String>,
    },
    Log {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        stream: Option<String>,
        #[serde(default)]
        level: LogLevel,
        text: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        source: Option<String>,
    },
    Result {
        data: Value,
    },
    NextStep {
        text: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        command: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        workflow: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        args: Option<BTreeMap<String, String>>,
    },
    Completed {
        status: CompletedStatus,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        message: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        exit_code: Option<i32>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        finished_at: Option<String>,
    },
}

impl WorkflowEvent {
    pub fn request_id(&self) -> Option<&str> {
        match self {
            WorkflowEvent::Input { request_id, .. }
            | WorkflowEvent::InputGroup { request_id, .. }
            | WorkflowEvent::Confirm { request_id, .. }
            | WorkflowEvent::ManualAction { request_id, .. } => Some(request_id),
            _ => None,
        }
    }

    pub fn is_request(&self) -> bool {
        self.request_id().is_some()
    }

    pub fn log(stream: &str, level: LogLevel, text: impl Into<String>, source: Option<&str>) -> Self {
        WorkflowEvent::Log {
            stream: Some(stream.to_string()),
            level,
            text: truncate(text.into()),
            source: source.map(str::to_string),
        }
    }
}

#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum ProtocolError {
    #[error("output line is not JSON: {0}")]
    NotJson(String),
    #[error("protocol event is invalid: {0}")]
    Invalid(String),
    #[error("output line exceeds {MAX_LINE_BYTES} bytes")]
    TooLong,
}

pub fn parse_line(line: &str) -> Result<WorkflowEvent, ProtocolError> {
    if line.len() > MAX_LINE_BYTES {
        return Err(ProtocolError::TooLong);
    }
    let value: Value = serde_json::from_str(line).map_err(|e| ProtocolError::NotJson(e.to_string()))?;
    if !value.is_object() {
        return Err(ProtocolError::NotJson("not a JSON object".into()));
    }
    let mut event: WorkflowEvent =
        serde_json::from_value(value).map_err(|e| ProtocolError::Invalid(e.to_string()))?;
    validate(&mut event).map_err(ProtocolError::Invalid)?;
    Ok(event)
}

fn truncate(mut text: String) -> String {
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

fn check_id(kind: &str, id: &str) -> Result<(), String> {
    if is_identifier(id) || (id.len() <= 128 && id.bytes().all(|b| b.is_ascii_alphanumeric() || b"_-.:".contains(&b))) {
        Ok(())
    } else {
        Err(format!("unsafe {kind} {id:?}"))
    }
}

fn validate_field(field: &mut Field) -> Result<(), String> {
    check_id("field id", &field.id)?;
    if field.kind == FieldType::Password {
        field.sensitive = true;
    }
    if field.is_sensitive() {
        // A secret is never echoed back, not even as a default.
        field.default = None;
    }
    if field.options.len() > MAX_OPTIONS {
        return Err(format!("field {} has too many options", field.id));
    }
    if matches!(field.kind, FieldType::Select | FieldType::Multiselect) && field.options.is_empty() {
        return Err(format!("field {} has no options", field.id));
    }
    field.label = truncate(std::mem::take(&mut field.label));
    Ok(())
}

fn validate(event: &mut WorkflowEvent) -> Result<(), String> {
    if let Some(id) = event.request_id() {
        check_id("request id", id)?;
    }
    match event {
        WorkflowEvent::Input { field, .. } => validate_field(field)?,
        WorkflowEvent::InputGroup { fields, .. } => {
            if fields.is_empty() || fields.len() > MAX_FIELDS {
                return Err("an input group needs 1 to 64 fields".into());
            }
            let mut seen = std::collections::BTreeSet::new();
            for field in fields.iter_mut() {
                validate_field(field)?;
                if !seen.insert(field.id.clone()) {
                    return Err(format!("duplicate field id {}", field.id));
                }
            }
        }
        WorkflowEvent::ValidationError { request_id, .. } => check_id("request id", request_id)?,
        WorkflowEvent::Phase { id, .. } => check_id("phase id", id)?,
        WorkflowEvent::Log { text, .. } => *text = truncate(std::mem::take(text)),
        WorkflowEvent::NextStep { workflow: Some(workflow), .. } => check_id("workflow id", workflow)?,
        _ => {}
    }
    Ok(())
}

/// A response from the frontend to an outstanding request.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ResponsePayload {
    Values { values: BTreeMap<String, Value> },
    Confirm { confirmed: bool },
    Acknowledge,
    Cancel,
}

/// Validate a response against the request it answers. Returns the NDJSON
/// line for the script and a copy of the values with secrets redacted.
pub fn build_response(
    request: &WorkflowEvent,
    payload: &ResponsePayload,
) -> Result<(String, Option<BTreeMap<String, Value>>), String> {
    let request_id = request.request_id().ok_or("not a request")?;
    let mut message = serde_json::Map::new();
    message.insert("type".into(), "response".into());
    message.insert("request_id".into(), request_id.into());
    let mut redacted = None;
    match (request, payload) {
        (_, ResponsePayload::Cancel) => {
            message.insert("cancelled".into(), true.into());
        }
        (WorkflowEvent::Input { field, .. }, ResponsePayload::Values { values }) => {
            let (clean, safe) = check_values(std::slice::from_ref(field), values)?;
            message.insert("values".into(), Value::Object(clean));
            redacted = Some(safe);
        }
        (WorkflowEvent::InputGroup { fields, .. }, ResponsePayload::Values { values }) => {
            let (clean, safe) = check_values(fields, values)?;
            message.insert("values".into(), Value::Object(clean));
            redacted = Some(safe);
        }
        (WorkflowEvent::Confirm { .. }, ResponsePayload::Confirm { confirmed }) => {
            message.insert("confirmed".into(), (*confirmed).into());
        }
        (WorkflowEvent::ManualAction { .. }, ResponsePayload::Acknowledge) => {
            message.insert("acknowledged".into(), true.into());
        }
        _ => return Err("the response does not match the kind of request".into()),
    }
    let line = serde_json::to_string(&Value::Object(message)).map_err(|e| e.to_string())?;
    Ok((line, redacted))
}

pub const REDACTED: &str = "[redacted]";

type Checked = (serde_json::Map<String, Value>, BTreeMap<String, Value>);

fn check_values(fields: &[Field], values: &BTreeMap<String, Value>) -> Result<Checked, String> {
    for key in values.keys() {
        if !fields.iter().any(|f| &f.id == key) {
            return Err(format!("unknown field {key:?}"));
        }
    }
    let mut clean = serde_json::Map::new();
    let mut safe = BTreeMap::new();
    for field in fields {
        let value = values.get(&field.id).cloned().unwrap_or(Value::Null);
        let value = check_field_value(field, value).map_err(|why| format!("{}: {why}", field.label))?;
        safe.insert(
            field.id.clone(),
            if field.is_sensitive() && !value.is_null() { Value::String(REDACTED.into()) } else { value.clone() },
        );
        clean.insert(field.id.clone(), value);
    }
    Ok((clean, safe))
}

fn check_field_value(field: &Field, value: Value) -> Result<Value, String> {
    let empty = match &value {
        Value::Null => true,
        Value::String(s) => s.is_empty(),
        Value::Array(a) => a.is_empty(),
        _ => false,
    };
    if empty {
        if field.required && field.kind != FieldType::Multiselect {
            return Err("a value is required".into());
        }
        if field.kind == FieldType::Multiselect {
            if let Some(min) = field.min_selected.filter(|m| *m > 0) {
                return Err(format!("select at least {min}"));
            }
            return Ok(Value::Array(vec![]));
        }
        return Ok(Value::String(String::new()));
    }
    let has_option = |text: &str| field.options.iter().any(|o| o.value == text);
    match field.kind {
        FieldType::Boolean => match value {
            Value::Bool(_) => Ok(value),
            _ => Err("expected true or false".into()),
        },
        FieldType::Integer | FieldType::Number | FieldType::Bytes | FieldType::Duration => {
            let number = match &value {
                Value::Number(n) => n.as_f64(),
                Value::String(s) => s.trim().parse::<f64>().ok(),
                _ => None,
            };
            let Some(number) = number else {
                if field.kind == FieldType::Duration {
                    return value.as_str().map(|s| Value::String(s.to_string())).ok_or("expected a duration".into());
                }
                return Err("expected a number".into());
            };
            if field.kind == FieldType::Integer && number.fract() != 0.0 {
                return Err("expected a whole number".into());
            }
            if field.min.is_some_and(|min| number < min) || field.max.is_some_and(|max| number > max) {
                return Err("out of range".into());
            }
            if number.fract() == 0.0 && number.abs() < 9.0e15 {
                Ok(Value::from(number as i64))
            } else {
                Ok(Value::from(number))
            }
        }
        FieldType::Select => {
            let text = value.as_str().ok_or("expected one option")?;
            if has_option(text) {
                Ok(value)
            } else {
                Err("not one of the options".into())
            }
        }
        FieldType::Multiselect => {
            let items = value.as_array().ok_or("expected a list")?;
            let mut chosen = Vec::new();
            for item in items {
                let text = item.as_str().ok_or("expected option values")?;
                if !has_option(text) {
                    return Err(format!("{text:?} is not one of the options"));
                }
                if !chosen.iter().any(|c: &Value| c.as_str() == Some(text)) {
                    chosen.push(item.clone());
                }
            }
            let count = chosen.len() as u32;
            if field.min_selected.is_some_and(|min| count < min) {
                return Err(format!("select at least {}", field.min_selected.unwrap()));
            }
            if field.max_selected.is_some_and(|max| count > max) {
                return Err(format!("select at most {}", field.max_selected.unwrap()));
            }
            Ok(Value::Array(chosen))
        }
        _ => {
            let text = value.as_str().ok_or("expected text")?;
            if text.len() > MAX_TEXT || text.contains('\0') {
                return Err("the value is too long or contains a NUL".into());
            }
            if field.kind != FieldType::Multiline && field.kind != FieldType::SshPublicKey && text.contains('\n') {
                return Err("the value must be a single line".into());
            }
            Ok(value)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_runner_events() {
        let event = parse_line(r#"{"type":"protocol","protocol":"bmac-ui","version":2}"#).unwrap();
        assert_eq!(event, WorkflowEvent::Protocol { protocol: "bmac-ui".into(), version: 2, bmac_version: None });
        let event = parse_line(r#"{"type":"log","stream":"stdout","level":"warning","text":"hi","extra":1}"#).unwrap();
        assert!(matches!(event, WorkflowEvent::Log { level: LogLevel::Warning, .. }));
        let event = parse_line(
            r#"{"type":"completed","status":"failed","message":"boom","exit_code":1,"finished_at":"2026-01-01T00:00:00Z"}"#,
        )
        .unwrap();
        assert!(matches!(event, WorkflowEvent::Completed { status: CompletedStatus::Failed, exit_code: Some(1), .. }));
    }

    #[test]
    fn rejects_bad_events() {
        assert!(matches!(parse_line("hello"), Err(ProtocolError::NotJson(_))));
        assert!(matches!(parse_line("[1]"), Err(ProtocolError::NotJson(_))));
        assert!(parse_line(r#"{"type":"nope"}"#).is_err());
        assert!(parse_line(r#"{"type":"phase","id":"a b","label":"x","status":"running"}"#).is_err());
        assert!(parse_line(r#"{"type":"input","request_id":"r1","field":{"id":"x","type":"select","label":"X"}}"#).is_err());
    }

    #[test]
    fn secrets_lose_their_defaults() {
        let event = parse_line(
            r#"{"type":"input","request_id":"r1","field":{"id":"k","type":"password","label":"Key","default":"oops"}}"#,
        )
        .unwrap();
        let WorkflowEvent::Input { field, .. } = event else { panic!() };
        assert!(field.sensitive && field.default.is_none());
    }

    #[test]
    fn manual_action_instructions_may_carry_a_copy_value() {
        let event = parse_line(
            r#"{"type":"manual_action","request_id":"m1","title":"T","instructions":
              ["Open the console",{"text":"Mount the ISO","copy":"/x/a.iso"},
               {"text":"Run this script:","copy":"/root/helper","run_on":"mox3"}]}"#,
        )
        .unwrap();
        let WorkflowEvent::ManualAction { instructions, .. } = &event else { panic!() };
        assert_eq!(
            instructions,
            &[
                Instruction::Text("Open the console".into()),
                Instruction::WithCopy { text: "Mount the ISO".into(), copy: "/x/a.iso".into(), run_on: None },
                Instruction::WithCopy {
                    text: "Run this script:".into(),
                    copy: "/root/helper".into(),
                    run_on: Some("mox3".into()),
                },
            ]
        );
        let sent: Value = serde_json::to_value(&event).unwrap();
        assert_eq!(
            sent["instructions"],
            json!([
                "Open the console",
                {"text": "Mount the ISO", "copy": "/x/a.iso"},
                {"text": "Run this script:", "copy": "/root/helper", "run_on": "mox3"},
            ])
        );
        assert!(parse_line(r#"{"type":"manual_action","request_id":"m1","title":"T","instructions":[{"copy":"x"}]}"#).is_err());
    }

    fn group() -> WorkflowEvent {
        parse_line(
            r#"{"type":"input_group","request_id":"g1","title":"T","fields":[
              {"id":"cores","type":"integer","label":"Cores","required":true,"min":1,"max":64},
              {"id":"hosts","type":"multiselect","label":"Hosts","min_selected":2,
               "options":[{"value":"mox1","label":"mox1"},{"value":"mox2","label":"mox2"},{"value":"mox3","label":"mox3"}]},
              {"id":"key","type":"password","label":"Key"}]}"#,
        )
        .unwrap()
    }

    #[test]
    fn responses_are_validated_and_redacted() {
        let payload = ResponsePayload::Values {
            values: [
                ("cores".to_string(), json!("8")),
                ("hosts".to_string(), json!(["mox1", "mox3"])),
                ("key".to_string(), json!("s3cret")),
            ]
            .into(),
        };
        let (line, redacted) = build_response(&group(), &payload).unwrap();
        let sent: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(sent["values"]["cores"], json!(8));
        assert_eq!(sent["values"]["key"], json!("s3cret"));
        let redacted = redacted.unwrap();
        assert_eq!(redacted["key"], json!(REDACTED));
        assert!(!serde_json::to_string(&redacted).unwrap().contains("s3cret"));
    }

    #[test]
    fn responses_are_checked_against_the_request() {
        let bad = |values: Value| {
            let values: BTreeMap<String, Value> = serde_json::from_value(values).unwrap();
            build_response(&group(), &ResponsePayload::Values { values }).is_err()
        };
        assert!(bad(json!({"cores": 0, "hosts": ["mox1", "mox2"]})), "below min");
        assert!(bad(json!({"cores": 2.5, "hosts": ["mox1", "mox2"]})), "not whole");
        assert!(bad(json!({"cores": 2, "hosts": ["mox1"]})), "too few selected");
        assert!(bad(json!({"cores": 2, "hosts": ["mox1", "mox9"]})), "unknown option");
        assert!(bad(json!({"cores": 2, "hosts": ["mox1", "mox2"], "other": 1})), "unknown field");
        assert!(bad(json!({"hosts": ["mox1", "mox2"]})), "required");
        assert!(build_response(&group(), &ResponsePayload::Confirm { confirmed: true }).is_err());
        let (line, _) = build_response(&group(), &ResponsePayload::Cancel).unwrap();
        assert_eq!(line, r#"{"cancelled":true,"request_id":"g1","type":"response"}"#);
    }
}
