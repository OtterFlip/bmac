// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

//! The `bmac-ui` types against v2's published schema
//! (`legacy/protocol/bmac-ui-v2.schema.json`, copied from v2): every event type it
//! defines parses, and every line we write back has the keys it requires.

use std::collections::BTreeSet;
use std::path::Path;

use bmac_core::job::Answer;
use bmac_proto::bmac_ui::{WorkflowEvent, build_response, cancel_message, parse_line};
use serde_json::{Value, json};

fn schema() -> Value {
    let path =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../legacy/protocol/bmac-ui-v2.schema.json");
    serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap()
}

fn definitions_by_type(schema: &Value) -> Vec<(String, String, Value)> {
    schema["$defs"]
        .as_object()
        .unwrap()
        .iter()
        .filter_map(|(name, def)| {
            let kind = def["properties"]["type"]["const"].as_str()?;
            Some((name.clone(), kind.to_string(), def.clone()))
        })
        .collect()
}

fn samples() -> Vec<Value> {
    let field = json!({"id": "name", "type": "string", "label": "Name"});
    vec![
        json!({"type": "protocol", "protocol": "bmac-ui", "version": 2, "bmac_version": "2.4.0"}),
        json!({"type": "workflow_started", "workflow": "add_prod_vm", "run_id": "r-1", "argv": ["--json"], "pid": 7}),
        json!({"type": "workflow_metadata", "title": "Add a production VM", "destructive": false}),
        json!({"type": "input", "request_id": "q1", "field": field}),
        json!({"type": "input_group", "request_id": "q2", "title": "Details", "fields": [field]}),
        json!({"type": "confirm", "request_id": "q3", "title": "Wipe?", "severity": "destructive", "confirmation_text": "wipe"}),
        json!({"type": "manual_action", "request_id": "q4", "title": "Console", "instructions": ["Unlock"]}),
        json!({"type": "progress", "message": "Copying", "current": 1, "total": 4, "unit": "GiB"}),
        json!({"type": "phase", "id": "copy", "label": "Copy", "status": "running", "cancel_allowed": true}),
        json!({"type": "plan", "title": "Plan", "items": [{"kind": "create", "target": "prod1", "description": "VM"}]}),
        json!({"type": "info", "message": "ok"}),
        json!({"type": "warning", "message": "careful"}),
        json!({"type": "error", "code": "failed", "message": "boom", "recoverable": false}),
        json!({"type": "validation_error", "request_id": "q2", "field_errors": {"name": "required"}}),
        json!({"type": "log", "stream": "stderr", "level": "error", "text": "x"}),
        json!({"type": "result", "data": {"vmid": 101}}),
        json!({"type": "next_step", "text": "Add staging", "workflow": "add_staging_vm"}),
        json!({"type": "completed", "status": "success", "exit_code": 0}),
    ]
}

fn has_required(value: &Value, def: &Value) -> Result<(), String> {
    for key in def["required"].as_array().into_iter().flatten() {
        let key = key.as_str().unwrap();
        if value.get(key).is_none() {
            return Err(format!("{value} lacks {key}"));
        }
    }
    Ok(())
}

#[test]
fn every_schema_event_type_parses() {
    let schema = schema();
    let events: Vec<_> = definitions_by_type(&schema)
        .into_iter()
        .filter(|(name, ..)| name.ends_with("Event"))
        .collect();
    let samples = samples();
    let schema_types: BTreeSet<&str> = events.iter().map(|(_, kind, _)| kind.as_str()).collect();
    let sample_types: BTreeSet<&str> = samples
        .iter()
        .map(|s| s["type"].as_str().unwrap())
        .collect();
    assert_eq!(
        schema_types, sample_types,
        "a sample for every event type the schema defines"
    );
    for sample in &samples {
        let def = &events
            .iter()
            .find(|(_, kind, _)| sample["type"] == kind.as_str())
            .unwrap()
            .2;
        has_required(sample, def).unwrap();
        let parsed = parse_line(&sample.to_string()).unwrap_or_else(|e| panic!("{sample}: {e}"));
        let mut minimal = serde_json::Map::new();
        for key in def["required"].as_array().unwrap() {
            let key = key.as_str().unwrap();
            minimal.insert(key.into(), sample[key].clone());
        }
        parse_line(&Value::Object(minimal).to_string())
            .unwrap_or_else(|e| panic!("required keys of {sample}: {e}"));
        let back = serde_json::to_value(&parsed).unwrap();
        has_required(&back, def).unwrap();
    }
}

#[test]
fn schema_enums_match_the_types() {
    let schema = schema();
    let defs = &schema["$defs"];
    for kind in defs["FieldType"]["enum"].as_array().unwrap() {
        let field = json!({"id": "f", "type": kind, "label": "F", "options": [{"value": "a", "label": "A"}]});
        parse_line(&json!({"type": "input", "request_id": "r", "field": field}).to_string())
            .unwrap_or_else(|e| panic!("{kind}: {e}"));
    }
    for severity in defs["Severity"]["enum"].as_array().unwrap() {
        let confirm =
            json!({"type": "confirm", "request_id": "r", "title": "T", "severity": severity});
        parse_line(&confirm.to_string()).unwrap_or_else(|e| panic!("{severity}: {e}"));
    }
    let codes = &defs["ErrorEvent"]["properties"]["code"]["examples"];
    for code in codes.as_array().unwrap() {
        parse_line(&json!({"type": "error", "code": code, "message": "m"}).to_string()).unwrap();
    }
    let identifier = defs["Identifier"]["pattern"].as_str().unwrap();
    assert_eq!(
        identifier, "^[A-Za-z0-9_.:-]{1,128}$",
        "the ID rule bmac_core::job::is_event_id implements"
    );
}

#[test]
fn replies_have_the_keys_the_schema_requires() {
    let schema = schema();
    let messages = definitions_by_type(&schema);
    let def = |name: &str| messages.iter().find(|(n, ..)| n == name).unwrap().2.clone();
    let event = |value: Value| -> WorkflowEvent { parse_line(&value.to_string()).unwrap() };
    let input = event(
        json!({"type": "input", "request_id": "q1", "field": {"id": "n", "type": "string", "label": "N"}}),
    );
    let confirm = event(json!({"type": "confirm", "request_id": "q3", "title": "T"}));
    let manual = event(
        json!({"type": "manual_action", "request_id": "q4", "title": "T", "instructions": ["x"]}),
    );
    let cases = [
        (
            &input,
            Answer::Values {
                values: [("n".to_string(), json!("v"))].into(),
            },
            "ValuesResponse",
        ),
        (
            &confirm,
            Answer::Confirm { confirmed: false },
            "ConfirmResponse",
        ),
        (&manual, Answer::Acknowledge, "AcknowledgeResponse"),
        (&input, Answer::Cancel, "CancelledResponse"),
    ];
    for (request, answer, name) in cases {
        let (line, _) = build_response(request, &answer).unwrap();
        has_required(&serde_json::from_str(&line).unwrap(), &def(name)).unwrap();
    }
    has_required(
        &serde_json::from_str(&cancel_message()).unwrap(),
        &def("CancelMessage"),
    )
    .unwrap();
}
