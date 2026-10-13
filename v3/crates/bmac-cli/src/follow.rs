// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

//! Following a job in the terminal: its events as text, and its requests
//! asked as prompts and answered through the engine, which checks them as
//! it checks the Dashboard's answers.

use std::collections::BTreeMap;
use std::time::Duration;

use bmac_core::JobId;
use bmac_core::job::{
    Answer, EventBody, Field, FieldType, Instruction, JobEvent, JobState, LogLevel, PhaseStatus,
    PlanKind, RequestForm, Severity,
};
use bmac_proto::api::{ErrorCode, JobSummary, JobView};
use bmac_proto::engine::EngineCall;
use serde_json::Value;

use crate::session::{CliError, Login, Result};
use crate::terminal::Prompter;

/// Typed at any prompt to cancel the request instead of answering it.
pub const CANCEL: &str = ":cancel";

/// One event as a line or a few, or `None` for events shown another way.
pub fn render(event: &JobEvent) -> Option<String> {
    Some(match &event.body {
        EventBody::Started {
            operation,
            host,
            user,
            dry_run,
        } => {
            let dry = if *dry_run { " (dry run)" } else { "" };
            format!("Started {operation}{dry} on {host} for {user}")
        }
        EventBody::Phase { label, status, .. } => match status {
            PhaseStatus::Running => format!("==> {label}"),
            PhaseStatus::Complete => format!("    done: {label}"),
            PhaseStatus::Failed => format!("    FAILED: {label}"),
            PhaseStatus::Skipped => format!("    skipped: {label}"),
            PhaseStatus::Pending => return None,
        },
        EventBody::Plan {
            title,
            description,
            items,
            dry_run,
        } => {
            let mut text = format!("Plan{}: {title}", if *dry_run { " (dry run)" } else { "" });
            if let Some(description) = description {
                text.push_str(&format!("\n  {description}"));
            }
            for item in items {
                let kind = match item.kind {
                    PlanKind::Create => "create",
                    PlanKind::Update => "update",
                    PlanKind::Remove => "remove",
                    PlanKind::Keep => "keep",
                    PlanKind::Check => "check",
                };
                text.push_str(&format!(
                    "\n  {kind:<7} {}: {}",
                    item.target, item.description
                ));
            }
            text
        }
        EventBody::Request { .. } => return None,
        EventBody::Answered { request_id, by, .. } => {
            format!("    (answered {request_id} as {by})")
        }
        EventBody::ValidationError {
            field_errors,
            message,
            ..
        } => {
            let mut text = match message.as_deref().filter(|m| !m.is_empty()) {
                Some(message) => format!("The answer was refused: {message}"),
                None => "The answer was refused:".to_string(),
            };
            for (field, why) in field_errors {
                text.push_str(&format!("\n  {field}: {why}"));
            }
            text
        }
        EventBody::Progress {
            message,
            current,
            total,
            unit,
            ..
        } => match (current, total) {
            (Some(current), Some(total)) => format!(
                "... {message} ({current}/{total}{})",
                unit.as_deref().map(|u| format!(" {u}")).unwrap_or_default()
            ),
            _ => format!("... {message}"),
        },
        EventBody::Output { level, text, .. } => match level {
            LogLevel::Warning => format!("warning: {text}"),
            LogLevel::Error => format!("error: {text}"),
            LogLevel::Debug | LogLevel::Info => text.clone(),
        },
        EventBody::Warning { message } => format!("warning: {message}"),
        EventBody::Result { data } => format!(
            "Result:\n{}",
            serde_json::to_string_pretty(data).unwrap_or_default()
        ),
        EventBody::NextStep {
            text,
            operation,
            args,
        } => match operation {
            Some(operation) => {
                let args: String = args.iter().map(|(k, v)| format!(" {k}={v}")).collect();
                format!("Next: {text}\n  bmac run {operation}{args}")
            }
            None => format!("Next: {text}"),
        },
        EventBody::Completed { message } => {
            format!(
                "Completed{}",
                message
                    .as_deref()
                    .map(|m| format!(": {m}"))
                    .unwrap_or_default()
            )
        }
        EventBody::Failed {
            message, details, ..
        } => match details {
            Some(details) => format!("Failed: {message}\n{details}"),
            None => format!("Failed: {message}"),
        },
        EventBody::Cancelled { by, message } => format!(
            "Cancelled{}{}",
            by.as_deref()
                .map(|b| format!(" by {b}"))
                .unwrap_or_default(),
            message
                .as_deref()
                .map(|m| format!(": {m}"))
                .unwrap_or_default()
        ),
        EventBody::Interrupted { message } => format!("Interrupted: {message}"),
    })
}

fn yes_no(text: &str) -> Option<bool> {
    match text.trim().to_ascii_lowercase().as_str() {
        "y" | "yes" | "true" | "1" => Some(true),
        "n" | "no" | "false" | "0" => Some(false),
        _ => None,
    }
}

fn option_value(field: &Field, text: &str) -> Option<String> {
    let text = text.trim();
    if let Ok(index) = text.parse::<usize>()
        && let Some(option) = index.checked_sub(1).and_then(|i| field.options.get(i))
    {
        return Some(option.value.clone());
    }
    field
        .options
        .iter()
        .find(|o| o.value == text)
        .map(|o| o.value.clone())
}

/// A typed answer as the field's JSON value. Empty text is `Ok(None)`.
pub fn parse_field(field: &Field, text: &str) -> std::result::Result<Option<Value>, String> {
    if text.trim().is_empty() && field.kind != FieldType::Multiline {
        return Ok(None);
    }
    let in_range = |n: f64| {
        if field.min.is_some_and(|min| n < min) || field.max.is_some_and(|max| n > max) {
            Err(format!(
                "must be between {} and {}",
                field.min.map_or("any".into(), |m| m.to_string()),
                field.max.map_or("any".into(), |m| m.to_string())
            ))
        } else {
            Ok(())
        }
    };
    Ok(Some(match field.kind {
        FieldType::Boolean => Value::Bool(yes_no(text).ok_or("answer yes or no")?),
        FieldType::Integer => {
            let n: i64 = text.trim().parse().map_err(|_| "expected a whole number")?;
            in_range(n as f64)?;
            Value::from(n)
        }
        FieldType::Number => {
            let n: f64 = text.trim().parse().map_err(|_| "expected a number")?;
            in_range(n)?;
            serde_json::Number::from_f64(n)
                .map(Value::Number)
                .ok_or("expected a number")?
        }
        FieldType::Select => {
            Value::String(option_value(field, text).ok_or("choose one of the listed options")?)
        }
        FieldType::Multiselect => {
            let mut values = Vec::new();
            for part in text.split(',').map(str::trim).filter(|p| !p.is_empty()) {
                let value = option_value(field, part)
                    .ok_or_else(|| format!("{part} is not one of the listed options"))?;
                if !values.contains(&value) {
                    values.push(value);
                }
            }
            let count = values.len() as u32;
            if field.min_selected.is_some_and(|min| count < min)
                || field.max_selected.is_some_and(|max| count > max)
            {
                return Err(format!(
                    "choose from {} to {} options",
                    field.min_selected.unwrap_or(0),
                    field.max_selected.map_or("any".into(), |m| m.to_string())
                ));
            }
            Value::Array(values.into_iter().map(Value::String).collect())
        }
        _ => Value::String(text.to_string()),
    }))
}

fn default_text(field: &Field) -> Option<String> {
    match field.default.as_ref()? {
        Value::String(s) => Some(s.clone()),
        Value::Bool(b) => Some(if *b { "yes" } else { "no" }.to_string()),
        Value::Array(items) => Some(
            items
                .iter()
                .map(|v| {
                    v.as_str()
                        .map(str::to_string)
                        .unwrap_or_else(|| v.to_string())
                })
                .collect::<Vec<_>>()
                .join(","),
        ),
        other => Some(other.to_string()),
    }
}

enum Asked<T> {
    Value(T),
    Cancel,
}

fn ended() -> CliError {
    CliError::Message("the input ended before the request was answered".to_string())
}

fn ask_field(prompter: &mut dyn Prompter, field: &Field) -> Result<Asked<Option<Value>>> {
    let mut intro = field.label.clone();
    if let Some(help) = &field.help {
        intro.push_str(&format!("\n  {help}"));
    }
    if matches!(field.kind, FieldType::Select | FieldType::Multiselect) {
        for (i, option) in field.options.iter().enumerate() {
            intro.push_str(&format!("\n  {}) {}", i + 1, option.label));
            if option.label != option.value {
                intro.push_str(&format!(" [{}]", option.value));
            }
        }
    }
    prompter.say(&intro);
    let default = default_text(field);
    let hint = match (field.kind, &default) {
        (FieldType::Boolean, Some(d)) => format!(" (yes/no, default {d})"),
        (FieldType::Boolean, None) => " (yes/no)".to_string(),
        (FieldType::Multiselect, Some(d)) => format!(" (numbers separated by commas, default {d})"),
        (FieldType::Multiselect, None) => " (numbers separated by commas)".to_string(),
        (FieldType::Multiline, _) => " (end with a line holding only .)".to_string(),
        (_, Some(d)) => format!(" (default {d})"),
        (_, None) => String::new(),
    };
    loop {
        let prompt = format!("{}{hint}: ", field.label);
        let text = if field.kind == FieldType::Multiline {
            prompter.say(&prompt);
            let mut lines = Vec::new();
            loop {
                let line = prompter.line("")?.ok_or_else(ended)?;
                if line == CANCEL && lines.is_empty() {
                    return Ok(Asked::Cancel);
                }
                if line == "." {
                    break;
                }
                lines.push(line);
            }
            lines.join("\n")
        } else if field.is_sensitive() {
            prompter.secret(&prompt)?.ok_or_else(ended)?
        } else {
            prompter.line(&prompt)?.ok_or_else(ended)?
        };
        if text.trim() == CANCEL {
            return Ok(Asked::Cancel);
        }
        let text = match (&default, text.trim().is_empty()) {
            (Some(default), true) => default.clone(),
            _ => text,
        };
        match parse_field(field, &text) {
            Ok(None) if field.required => prompter.say("  An answer is required."),
            Ok(value) => return Ok(Asked::Value(value)),
            Err(why) => prompter.say(&format!("  {why}")),
        }
    }
}

fn ask_fields(prompter: &mut dyn Prompter, fields: &[Field]) -> Result<Answer> {
    let mut values = BTreeMap::new();
    for field in fields.iter().filter(|f| !f.disabled) {
        match ask_field(prompter, field)? {
            Asked::Cancel => return Ok(Answer::Cancel),
            Asked::Value(Some(value)) => {
                values.insert(field.id.clone(), value);
            }
            Asked::Value(None) => {}
        }
    }
    Ok(Answer::Values { values })
}

/// Asks a request at the terminal. `:cancel` at any prompt cancels it.
pub fn ask(prompter: &mut dyn Prompter, form: &RequestForm) -> Result<Answer> {
    match form {
        RequestForm::Input {
            title,
            description,
            context,
            field,
        } => {
            for text in [title, description, context].into_iter().flatten() {
                prompter.say(text);
            }
            ask_fields(prompter, std::slice::from_ref(field))
        }
        RequestForm::InputGroup {
            title,
            description,
            context,
            fields,
            ..
        } => {
            prompter.say(title);
            for text in [description, context].into_iter().flatten() {
                prompter.say(text);
            }
            ask_fields(prompter, fields)
        }
        RequestForm::Confirm {
            title,
            message,
            severity,
            confirm_label,
            confirmation_text,
            context,
            details,
            ..
        } => {
            let tag = match severity {
                Severity::Normal => "",
                Severity::Warning => "[WARNING] ",
                Severity::Destructive => "[DESTRUCTIVE] ",
                Severity::Critical => "[CRITICAL] ",
            };
            prompter.say(&format!("{tag}{title}"));
            for text in [Some(message), context.as_ref()].into_iter().flatten() {
                if !text.is_empty() {
                    prompter.say(text);
                }
            }
            for detail in details {
                prompter.say(&format!("  - {detail}"));
            }
            let answer = match confirmation_text {
                Some(phrase) => {
                    let typed = prompter
                        .line(&format!(
                            "Type {phrase} to confirm, or press Enter to decline: "
                        ))?
                        .ok_or_else(ended)?;
                    if typed.trim() == CANCEL {
                        return Ok(Answer::Cancel);
                    }
                    typed == *phrase
                }
                None => loop {
                    let label = confirm_label.as_deref().unwrap_or("Confirm");
                    let typed = prompter
                        .line(&format!("{label}? [y/N]: "))?
                        .ok_or_else(ended)?;
                    if typed.trim() == CANCEL {
                        return Ok(Answer::Cancel);
                    }
                    if typed.trim().is_empty() {
                        break false;
                    }
                    if let Some(answer) = yes_no(&typed) {
                        break answer;
                    }
                },
            };
            Ok(Answer::Confirm { confirmed: answer })
        }
        RequestForm::ManualAction {
            title,
            instructions,
            acknowledge_label,
            context,
        } => {
            prompter.say(title);
            if let Some(context) = context {
                prompter.say(context);
            }
            for (i, step) in instructions.iter().enumerate() {
                match step {
                    Instruction::Text(text) => prompter.say(&format!("  {}. {text}", i + 1)),
                    Instruction::WithCopy { text, copy, run_on } => {
                        let on = run_on
                            .as_deref()
                            .map(|h| format!(" (on {h})"))
                            .unwrap_or_default();
                        prompter.say(&format!("  {}. {text}{on}\n       {copy}", i + 1));
                    }
                }
            }
            let label = acknowledge_label.as_deref().unwrap_or("Done");
            let typed = prompter
                .line(&format!("Press Enter when finished ({label}): "))?
                .ok_or_else(ended)?;
            Ok(if typed.trim() == CANCEL {
                Answer::Cancel
            } else {
                Answer::Acknowledge
            })
        }
    }
}

/// The latest request, with the sequence number of the event that asked it.
/// A script that refuses an answer reopens the same request with a
/// validation error rather than a new request event.
struct Pending {
    seq: u64,
    request_id: String,
    form: RequestForm,
}

pub struct Followed {
    pub job: JobSummary,
}

/// Shows `job`'s events from `from_seq` until it finishes or is
/// interrupted, answering its requests when `answer` is set and someone is
/// at the terminal.
pub async fn follow(
    login: &Login,
    prompter: &mut dyn Prompter,
    job: JobId,
    from_seq: u64,
    answer: bool,
    poll: Duration,
) -> Result<Followed> {
    let mut next = from_seq.max(1);
    let mut pending: Option<Pending> = None;
    let mut asked: Option<u64> = None;
    let mut told_waiting = false;
    loop {
        let view: JobView = login
            .call(EngineCall::Job {
                job,
                from_seq: next,
            })
            .await?;
        for event in &view.events {
            next = event.seq + 1;
            match &event.body {
                EventBody::Request { request_id, form } => {
                    pending = Some(Pending {
                        seq: event.seq,
                        request_id: request_id.clone(),
                        form: (**form).clone(),
                    });
                }
                EventBody::ValidationError { request_id, .. } => {
                    if let Some(p) = pending.as_mut().filter(|p| &p.request_id == request_id) {
                        p.seq = event.seq;
                    }
                }
                _ => {}
            }
            if let Some(text) = render(event) {
                prompter.say(&text);
            }
        }
        if view.more {
            continue;
        }
        let summary = view.job;
        if summary.state.is_terminal() || summary.state == JobState::Interrupted {
            return Ok(Followed { job: summary });
        }
        let waiting = pending.as_ref().filter(|p| {
            summary.pending_request.as_deref() == Some(p.request_id.as_str())
                && asked != Some(p.seq)
        });
        if let Some(request) = waiting {
            if answer && prompter.interactive() {
                let reply = ask(prompter, &request.form)?;
                let call = EngineCall::Answer {
                    job,
                    request_id: request.request_id.clone(),
                    answer: reply,
                };
                match login.call::<JobSummary>(call).await {
                    Ok(_) => asked = Some(request.seq),
                    Err(CliError::Api(error)) if error.code == ErrorCode::Invalid => {
                        prompter.say(&format!("The answer was refused: {}", error.message));
                        for (field, why) in &error.field_errors {
                            prompter.say(&format!("  {field}: {why}"));
                        }
                    }
                    Err(CliError::Api(error)) if error.code == ErrorCode::Conflict => {
                        prompter.say(&format!("  {}", error.message));
                        asked = Some(request.seq);
                    }
                    Err(error) => return Err(error),
                }
                continue;
            }
            if !told_waiting {
                prompter.say(&format!(
                    "The job is waiting for an answer to {}; answer it in the Dashboard or with \
                     bmac jobs {job}",
                    request.request_id
                ));
                told_waiting = true;
            }
        }
        tokio::time::sleep(poll).await;
    }
}

#[cfg(test)]
mod tests {
    use bmac_core::job::FieldOption;
    use serde_json::json;

    use super::*;
    use crate::terminal::Scripted;

    fn field(value: Value) -> Field {
        serde_json::from_value(value).unwrap()
    }

    fn form(value: Value) -> RequestForm {
        serde_json::from_value(value).unwrap()
    }

    #[test]
    fn typed_answers_become_json_values() {
        let cores =
            field(json!({"id": "cores", "type": "integer", "label": "Cores", "min": 1, "max": 8}));
        assert_eq!(parse_field(&cores, "4"), Ok(Some(json!(4))));
        assert!(
            parse_field(&cores, "9")
                .unwrap_err()
                .contains("between 1 and 8")
        );
        assert!(parse_field(&cores, "four").is_err());
        assert_eq!(parse_field(&cores, "  "), Ok(None));
        let flag = field(json!({"id": "f", "type": "boolean", "label": "F"}));
        assert_eq!(parse_field(&flag, "Yes"), Ok(Some(json!(true))));
        assert_eq!(parse_field(&flag, "n"), Ok(Some(json!(false))));
        let mut hosts = field(
            json!({"id": "hosts", "type": "multiselect", "label": "Hosts", "max_selected": 2}),
        );
        hosts.options = ["mox1", "mox2", "mox3"]
            .iter()
            .map(|v| FieldOption {
                value: v.to_string(),
                label: v.to_string(),
                help: None,
            })
            .collect();
        assert_eq!(
            parse_field(&hosts, "1, mox3, 1"),
            Ok(Some(json!(["mox1", "mox3"])))
        );
        assert!(
            parse_field(&hosts, "1,2,3")
                .unwrap_err()
                .contains("from 0 to 2")
        );
        assert!(parse_field(&hosts, "mox9").is_err());
    }

    #[test]
    fn an_input_group_is_asked_field_by_field() {
        let group = form(json!({
            "kind": "input_group", "title": "Sizing",
            "fields": [
                {"id": "cores", "type": "integer", "label": "Cores", "required": true, "default": 2},
                {"id": "note", "type": "string", "label": "Note"},
                {"id": "pw", "type": "password", "label": "Password", "required": true},
                {"id": "off", "type": "string", "label": "Off", "disabled": true}
            ]
        }));
        let mut prompter = Scripted::new(["lots", "", "", "", "hunter2"]);
        let answer = ask(&mut prompter, &group).unwrap();
        assert_eq!(
            answer,
            Answer::Values {
                values: [
                    ("cores".to_string(), json!(2)),
                    ("pw".to_string(), json!("hunter2"))
                ]
                .into_iter()
                .collect()
            }
        );
        let shown = prompter.text();
        assert!(shown.contains("expected a whole number"), "{shown}");
        assert!(shown.contains("An answer is required."), "{shown}");
        assert!(shown.contains("Password: <secret>"), "{shown}");
        assert!(!shown.contains("hunter2"), "secrets are never echoed");
        assert!(!shown.contains("Off"), "disabled fields are skipped");
    }

    #[test]
    fn confirmations_need_the_exact_phrase() {
        let typed = form(json!({
            "kind": "confirm", "title": "Destroy prod1", "severity": "destructive",
            "confirmation_text": "DESTROY prod1", "details": ["removes disks"]
        }));
        let mut prompter = Scripted::new(["destroy prod1"]);
        assert_eq!(
            ask(&mut prompter, &typed).unwrap(),
            Answer::Confirm { confirmed: false }
        );
        assert!(prompter.text().contains("[DESTRUCTIVE] Destroy prod1"));
        let mut prompter = Scripted::new(["DESTROY prod1"]);
        assert_eq!(
            ask(&mut prompter, &typed).unwrap(),
            Answer::Confirm { confirmed: true }
        );

        let plain = form(json!({"kind": "confirm", "title": "Go on?"}));
        let mut prompter = Scripted::new(["maybe", "y"]);
        assert_eq!(
            ask(&mut prompter, &plain).unwrap(),
            Answer::Confirm { confirmed: true }
        );
        let mut prompter = Scripted::new([""]);
        assert_eq!(
            ask(&mut prompter, &plain).unwrap(),
            Answer::Confirm { confirmed: false }
        );
        let mut prompter = Scripted::new([CANCEL]);
        assert_eq!(ask(&mut prompter, &plain).unwrap(), Answer::Cancel);
    }

    #[test]
    fn manual_actions_list_their_steps() {
        let manual = form(json!({
            "kind": "manual_action", "title": "Unlock the disks",
            "instructions": ["Open the console", {"text": "Run", "copy": "bmac luks-enter", "run_on": "mox2"}]
        }));
        let mut prompter = Scripted::new([""]);
        assert_eq!(ask(&mut prompter, &manual).unwrap(), Answer::Acknowledge);
        let shown = prompter.text();
        assert!(
            shown.contains("2. Run (on mox2)\n       bmac luks-enter"),
            "{shown}"
        );
        let mut prompter = Scripted::new(Vec::<String>::new());
        assert!(
            ask(&mut prompter, &manual).is_err(),
            "the end of input is not an answer"
        );
    }

    #[test]
    fn events_read_as_text() {
        let event = |body: Value| -> JobEvent {
            let mut value =
                json!({"job": "6f9619ff-8b86-4011-b42d-00c04fc964ff", "seq": 1, "at_ms": 0});
            value
                .as_object_mut()
                .unwrap()
                .extend(body.as_object().unwrap().clone());
            serde_json::from_value(value).unwrap()
        };
        let lines: Vec<Option<String>> = [
            json!({"type": "phase", "id": "p", "label": "Copy disks", "status": "running"}),
            json!({"type": "output", "level": "warning", "text": "slow"}),
            json!({"type": "progress", "message": "Copying", "current": 1.0, "total": 4.0, "unit": "GiB"}),
            json!({"type": "next_step", "text": "Set up SSH", "operation": "prod.add", "args": {"resource": "prod2"}}),
            json!({"type": "failed", "message": "no quorum"}),
            json!({"type": "phase", "id": "p", "label": "x", "status": "pending"}),
        ]
        .into_iter()
        .map(|b| render(&event(b)))
        .collect();
        assert_eq!(
            lines,
            [
                Some("==> Copy disks".to_string()),
                Some("warning: slow".to_string()),
                Some("... Copying (1/4 GiB)".to_string()),
                Some("Next: Set up SSH\n  bmac run prod.add resource=prod2".to_string()),
                Some("Failed: no quorum".to_string()),
                None,
            ]
        );
    }
}
