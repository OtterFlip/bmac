// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

//! The operation registry: script operations from
//! `legacy/script-operations.json` and native ones declared in Rust, checked
//! when loaded. [`Registry::prepare`] turns a launch's parameters into the
//! script's arguments and the job's resource locks; the browser never supplies
//! a path or arguments, unknown parameters are refused, a positional value
//! can't start with `-`, and `--json` and `--dry-run` are reserved.

use std::collections::{BTreeMap, BTreeSet};

use bmac_core::{GuestName, HostSlot};
use bmac_proto::api::{ApiError, ErrorCode, Role};
use bmac_proto::operations::{
    CancelRule, Operation, OperationKind, OperationList, OperationView, Param, ParamType,
    ResumeRule, TargetRule,
};
use serde::Deserialize;
use serde_json::Value;

/// The script operations, embedded at build time.
pub const SCRIPT_OPERATIONS: &str = include_str!("../../../legacy/script-operations.json");

/// The mock operations of a development build for the lab.
pub const LAB_OPERATIONS: &str = include_str!("../lab-operations.json");

/// The first part of every lock name.
pub const LOCK_CLASSES: &[&str] = &[
    "capacity",
    "cluster-membership",
    "host",
    "production",
    "staging",
    "qdevice",
    "storage",
    "backups",
];

const DRY_RUN_FLAG: &str = "--dry-run";
const RESERVED_FLAGS: &[&str] = &["--json", DRY_RUN_FLAG];

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ScriptFile {
    schema_version: u32,
    operations: Vec<Operation>,
}

/// Operations implemented in Rust. Each milestone that ports one adds it here.
pub fn native_operations() -> Vec<Operation> {
    vec![
        crate::native::setup::operation(),
        crate::native::join::operation(),
    ]
}

#[derive(Debug)]
pub struct Registry {
    operations: Vec<Operation>,
}

impl Registry {
    /// The embedded script operations and the native ones. A build with the
    /// `lab` feature adds the mock operations, run from `mock/` in the legacy
    /// runtime; release builds can't load them.
    pub fn builtin() -> Result<Registry, String> {
        #[allow(unused_mut)]
        let mut native = native_operations();
        #[cfg(feature = "lab")]
        {
            let lab: ScriptFile = serde_json::from_str(LAB_OPERATIONS)
                .map_err(|e| format!("lab-operations.json: {e}"))?;
            native.extend(lab.operations);
        }
        Registry::from_parts(SCRIPT_OPERATIONS, native)
    }

    pub fn from_parts(script_json: &str, native: Vec<Operation>) -> Result<Registry, String> {
        let file: ScriptFile = serde_json::from_str(script_json)
            .map_err(|e| format!("script-operations.json: {e}"))?;
        if file.schema_version != 1 {
            return Err(format!(
                "script-operations.json: unsupported schema_version {}",
                file.schema_version
            ));
        }
        let mut operations = file.operations;
        operations.extend(native);
        Registry::new(operations)
    }

    pub fn new(operations: Vec<Operation>) -> Result<Registry, String> {
        let mut ids = BTreeSet::new();
        for operation in &operations {
            if !ids.insert(operation.id.as_str()) {
                return Err(format!("{}: duplicate operation", operation.id));
            }
            check_operation(operation).map_err(|why| format!("{}: {why}", operation.id))?;
        }
        Ok(Registry { operations })
    }

    pub fn all(&self) -> &[Operation] {
        &self.operations
    }

    pub fn get(&self, id: &str) -> Option<&Operation> {
        self.operations.iter().find(|o| o.id.as_str() == id)
    }

    /// `GET /api/v1/operations` for a user with `role`.
    pub fn list_for(&self, role: Role) -> OperationList {
        OperationList {
            operations: self
                .operations
                .iter()
                .map(|operation| OperationView {
                    operation: operation.clone(),
                    allowed: role >= operation.min_role,
                })
                .collect(),
        }
    }
}

/// A launch's parameters, checked against its operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Prepared {
    /// The script's arguments after its path; empty for native operations.
    pub args: Vec<String>,
    /// Each supplied parameter as text, for the job record and display.
    pub summary: BTreeMap<String, String>,
    /// The resource locks the job holds while it runs.
    pub locks: Vec<String>,
    pub dry_run: bool,
}

/// Why a launch's parameters were refused, with a message per parameter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InvalidLaunch {
    pub message: String,
    pub field_errors: BTreeMap<String, String>,
}

impl From<InvalidLaunch> for ApiError {
    fn from(invalid: InvalidLaunch) -> ApiError {
        let mut error = ApiError::new(ErrorCode::Invalid, invalid.message);
        error.field_errors = invalid.field_errors;
        error
    }
}

fn find_param<'a>(operation: &'a Operation, id: &str) -> Option<&'a Param> {
    operation.params.iter().find(|p| p.id == id)
}

/// Whether a parameter's value goes after the options rather than after a flag.
fn is_positional(param: &Param) -> bool {
    param.flag.is_none() && !matches!(param.kind, ParamType::Flag | ParamType::FlagChoice)
}

/// Checks `values` against `operation` and builds its arguments and locks.
pub fn prepare(
    operation: &Operation,
    values: &BTreeMap<String, Value>,
    dry_run: bool,
) -> Result<Prepared, InvalidLaunch> {
    let mut field_errors = BTreeMap::new();
    for key in values.keys() {
        if find_param(operation, key).is_none() {
            field_errors.insert(key.clone(), "not a parameter of this operation".to_string());
        }
    }
    if dry_run && !operation.supports_dry_run {
        return Err(InvalidLaunch {
            message: format!("{} has no dry run", operation.title),
            field_errors,
        });
    }
    let mut options = Vec::new();
    let mut positional = Vec::new();
    let mut summary = BTreeMap::new();
    for param in &operation.params {
        let Some(value) = values.get(&param.id).filter(|v| !is_blank(v)) else {
            if param.required {
                field_errors.insert(param.id.clone(), "required".to_string());
            }
            continue;
        };
        match param_arguments(param, value) {
            Ok((text, mut args)) => {
                if let Some(text) = text {
                    // The scripts don't all accept `--`, so a positional value
                    // that looks like an option must never get this far.
                    if is_positional(param) && text.starts_with('-') {
                        field_errors.insert(param.id.clone(), "may not start with '-'".into());
                        continue;
                    }
                    if is_positional(param) {
                        positional.append(&mut args);
                    } else {
                        options.append(&mut args);
                    }
                    summary.insert(param.id.clone(), text);
                }
            }
            Err(why) => {
                field_errors.insert(param.id.clone(), why);
            }
        }
    }
    if !field_errors.is_empty() {
        let message = if field_errors.len() == 1 {
            let (id, why) = field_errors.iter().next().expect("one error");
            let label = find_param(operation, id).map_or(id.as_str(), |p| p.label.as_str());
            format!("{label}: {why}")
        } else {
            "some parameters need attention".to_string()
        };
        return Err(InvalidLaunch {
            message,
            field_errors,
        });
    }
    let mut args = options;
    if dry_run && operation.kind == OperationKind::Script {
        args.push(DRY_RUN_FLAG.to_string());
    }
    args.extend(positional);
    let locks = operation
        .locks
        .iter()
        .map(|template| resolve_lock(template, &summary))
        .collect();
    Ok(Prepared {
        args,
        summary,
        locks,
        dry_run,
    })
}

/// A parameter's text for the summary (`None` when a flag is off) and its
/// arguments.
fn param_arguments(param: &Param, value: &Value) -> Result<(Option<String>, Vec<String>), String> {
    match param.kind {
        ParamType::Flag => {
            let on = value.as_bool().ok_or("expected true or false")?;
            let args = match (&param.flag, on) {
                (Some(flag), true) => vec![flag.clone()],
                _ => Vec::new(),
            };
            Ok((on.then(|| "yes".to_string()), args))
        }
        ParamType::FlagChoice => {
            let text = value.as_str().ok_or("expected a choice")?;
            let choice = param
                .choices
                .iter()
                .find(|c| c.value == text)
                .ok_or("not one of the allowed choices")?;
            Ok((
                Some(choice.value.clone()),
                choice.flag.iter().cloned().collect(),
            ))
        }
        _ => {
            let text = scalar_text(value).ok_or("expected a single value")?;
            let text = canonical_value(param, &text)?;
            let args = match &param.flag {
                Some(flag) => vec![flag.clone(), text.clone()],
                None => vec![text.clone()],
            };
            Ok((Some(text), args))
        }
    }
}

fn is_blank(value: &Value) -> bool {
    match value {
        Value::Null => true,
        Value::String(s) => s.trim().is_empty(),
        _ => false,
    }
}

fn scalar_text(value: &Value) -> Option<String> {
    match value {
        Value::String(s) => Some(s.trim().to_string()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

/// Checks a value and returns its canonical text.
fn canonical_value(param: &Param, text: &str) -> Result<String, String> {
    let guest = || text.parse::<GuestName>().ok();
    match param.kind {
        ParamType::Choice => param
            .choices
            .iter()
            .find(|c| c.value == text)
            .map(|c| c.value.clone())
            .ok_or_else(|| "not one of the allowed choices".to_string()),
        ParamType::Host => text
            .parse::<HostSlot>()
            .map(|h| h.to_string())
            .map_err(|_| "expected mox1 to mox32".to_string()),
        ParamType::Production => guest()
            .filter(|g| g.is_production())
            .map(|g| g.to_string())
            .ok_or_else(|| "expected prodN".to_string()),
        ParamType::Staging => guest()
            .filter(|g| !g.is_production())
            .map(|g| g.to_string())
            .ok_or_else(|| "expected stageMprodN".to_string()),
        ParamType::Guest => guest()
            .map(|g| g.to_string())
            .ok_or_else(|| "expected prodN or stageMprodN".to_string()),
        ParamType::Integer => {
            let ok = !text.is_empty()
                && text.len() <= 9
                && text.bytes().all(|b| b.is_ascii_digit())
                && !text.starts_with('0');
            ok.then(|| text.to_string())
                .ok_or_else(|| "expected a positive whole number".to_string())
        }
        ParamType::Hostname => is_hostname(text)
            .then(|| text.to_ascii_lowercase())
            .ok_or_else(|| "expected a host name".to_string()),
        ParamType::Flag | ParamType::FlagChoice => unreachable!("handled by the caller"),
    }
}

pub fn is_hostname(text: &str) -> bool {
    !text.is_empty()
        && text.len() <= 253
        && text.split('.').all(|label| {
            !label.is_empty()
                && label.len() <= 63
                && !label.starts_with('-')
                && !label.ends_with('-')
                && label
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-')
        })
}

/// One part of a lock template.
#[derive(Debug, PartialEq, Eq)]
enum LockPart<'a> {
    Literal(&'a str),
    Param { id: &'a str, optional: bool },
}

fn lock_parts(template: &str) -> Result<Vec<LockPart<'_>>, String> {
    template
        .split(':')
        .map(|part| {
            if let Some(inner) = part.strip_prefix('{').and_then(|p| p.strip_suffix('}')) {
                let (id, optional) = match inner.strip_suffix('?') {
                    Some(id) => (id, true),
                    None => (inner, false),
                };
                if id.is_empty() || !id.bytes().all(|b| b.is_ascii_lowercase() || b == b'_') {
                    return Err(format!("lock {template:?}: bad parameter {part:?}"));
                }
                Ok(LockPart::Param { id, optional })
            } else if !part.is_empty()
                && part
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
            {
                Ok(LockPart::Literal(part))
            } else {
                Err(format!("lock {template:?}: bad part {part:?}"))
            }
        })
        .collect()
}

/// Fills a checked template from the summary. An absent optional parameter
/// ends the name there, so the lock covers the whole class.
fn resolve_lock(template: &str, summary: &BTreeMap<String, String>) -> String {
    let mut name = Vec::new();
    for part in lock_parts(template).expect("checked when the registry loaded") {
        match part {
            LockPart::Literal(text) => name.push(text),
            LockPart::Param { id, .. } => match summary.get(id) {
                Some(value) => name.push(value),
                None => break,
            },
        }
    }
    name.join(":")
}

/// Whether two locks cover a common resource: they are equal, or one is a
/// parent of the other (`production` covers `production:prod1`).
pub fn locks_conflict(a: &str, b: &str) -> bool {
    let (short, long) = if a.len() <= b.len() { (a, b) } else { (b, a) };
    long == short
        || long
            .strip_prefix(short)
            .is_some_and(|rest| rest.starts_with(':'))
}

fn is_identifier(text: &str) -> bool {
    !text.is_empty()
        && text.len() <= 64
        && text.starts_with(|c: char| c.is_ascii_lowercase())
        && text
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
}

fn is_safe_relative_script(path: &str) -> bool {
    path.ends_with(".sh")
        && path.split('/').all(|part| {
            !part.is_empty()
                && part != "."
                && part != ".."
                && part
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-' || b == b'.')
        })
}

fn check_operation(operation: &Operation) -> Result<(), String> {
    for (name, text) in [
        ("title", &operation.title),
        ("summary", &operation.summary),
        ("description", &operation.description),
    ] {
        if text.trim().is_empty() {
            return Err(format!("empty {name}"));
        }
    }
    let script = operation.kind == OperationKind::Script;
    match (&operation.script, script) {
        (Some(path), true) if !is_safe_relative_script(path) => {
            return Err(format!("unsafe script path {path:?}"));
        }
        (None, true) => return Err("a script operation needs a script".into()),
        (Some(_), false) => return Err("a native operation has no script".into()),
        _ => {}
    }
    if operation.destructive && !operation.mutating {
        return Err("destructive but not mutating".into());
    }
    if operation.destructive && operation.min_role != Role::Admin {
        return Err("destructive operations are for administrators".into());
    }
    if operation.mutating && operation.min_role == Role::Viewer {
        return Err("viewers can't start a mutating operation".into());
    }
    if !operation.mutating && (!operation.locks.is_empty() || operation.supports_dry_run) {
        return Err("a read-only operation takes no locks and has no dry run".into());
    }
    if operation.mutating && operation.locks.is_empty() {
        return Err("a mutating operation needs at least one lock".into());
    }
    if script && operation.resume != ResumeRule::Never {
        return Err("script operations are never resumed".into());
    }
    if script != matches!(operation.cancel, CancelRule::Script {}) {
        return Err("script operations, and only they, take the script's cancel rule".into());
    }
    check_params(operation, script)?;
    check_locks(operation)?;
    check_target(operation)
}

fn check_params(operation: &Operation, script: bool) -> Result<(), String> {
    let mut ids = BTreeSet::new();
    let mut flags = BTreeSet::new();
    let mut add_flag = |flag: &Option<String>| -> Result<(), String> {
        let Some(flag) = flag else { return Ok(()) };
        if !script {
            return Err(format!("native operations have no flags ({flag})"));
        }
        let name = flag.strip_prefix("--").unwrap_or("");
        if name.is_empty()
            || !name
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        {
            return Err(format!("bad flag {flag:?}"));
        }
        if RESERVED_FLAGS.contains(&flag.as_str()) {
            return Err(format!("{flag} is reserved"));
        }
        if !flags.insert(flag.clone()) {
            return Err(format!("flag {flag} used twice"));
        }
        Ok(())
    };
    for param in &operation.params {
        let id = &param.id;
        if !is_identifier(id) {
            return Err(format!("bad parameter id {id:?}"));
        }
        if !ids.insert(id.as_str()) {
            return Err(format!("parameter {id} declared twice"));
        }
        if param.label.trim().is_empty() {
            return Err(format!("{id}: empty label"));
        }
        add_flag(&param.flag)?;
        let has_choices = matches!(param.kind, ParamType::Choice | ParamType::FlagChoice);
        if has_choices == param.choices.is_empty() {
            return Err(format!(
                "{id}: choices go with choice parameters, and only them"
            ));
        }
        let mut values = BTreeSet::new();
        for choice in &param.choices {
            if !values.insert(choice.value.as_str()) {
                return Err(format!("{id}: choice {} listed twice", choice.value));
            }
            if param.kind == ParamType::FlagChoice {
                if script && choice.flag.is_none() {
                    return Err(format!("{id}: choice {} has no flag", choice.value));
                }
                add_flag(&choice.flag)?;
            } else if choice.flag.is_some() {
                return Err(format!("{id}: only flag_choice choices have flags"));
            }
        }
        if param.kind == ParamType::FlagChoice && param.flag.is_some() {
            return Err(format!("{id}: a flag_choice's flags are on its choices"));
        }
        if param.kind == ParamType::Flag && script && param.flag.is_none() {
            return Err(format!("{id}: a script flag needs a flag"));
        }
        if param.suggest.is_some() && param.kind != ParamType::Host {
            return Err(format!("{id}: only host parameters take suggestions"));
        }
        if let Some(default) = &param.default {
            if param.required {
                return Err(format!("{id}: a required parameter has no default"));
            }
            param_arguments(param, default).map_err(|why| format!("{id}: default: {why}"))?;
        }
    }
    Ok(())
}

fn check_locks(operation: &Operation) -> Result<(), String> {
    let mut seen = BTreeSet::new();
    for template in &operation.locks {
        let parts = lock_parts(template)?;
        match parts.first() {
            Some(LockPart::Literal(class)) if LOCK_CLASSES.contains(class) => {}
            _ => return Err(format!("lock {template:?}: unknown class")),
        }
        for part in &parts {
            let LockPart::Param { id, optional } = *part else {
                continue;
            };
            let param = find_param(operation, id)
                .ok_or_else(|| format!("lock {template:?}: no parameter {id}"))?;
            if !param.kind.names_a_resource() {
                return Err(format!("lock {template:?}: {id} doesn't name a resource"));
            }
            if !optional && !param.required {
                return Err(format!(
                    "lock {template:?}: {id} is optional, so write {{{id}?}}"
                ));
            }
        }
        if !seen.insert(template.as_str()) {
            return Err(format!("lock {template:?} listed twice"));
        }
    }
    Ok(())
}

fn check_target(operation: &Operation) -> Result<(), String> {
    let (id, kinds): (&str, &[ParamType]) = match &operation.target {
        TargetRule::Any {} | TargetRule::Duty {} | TargetRule::Placement {} => return Ok(()),
        TargetRule::Host { param } | TargetRule::AnyExcept { param } => (param, &[ParamType::Host]),
        TargetRule::Owner { param } => (
            param,
            &[ParamType::Production, ParamType::Staging, ParamType::Guest],
        ),
        TargetRule::Standby { param } => (param, &[ParamType::Production]),
    };
    let param = find_param(operation, id).ok_or_else(|| format!("target: no parameter {id}"))?;
    if !kinds.contains(&param.kind) || !param.required {
        return Err(format!(
            "target: {id} must be a required {kinds:?} parameter"
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn operation(extra: Value) -> Operation {
        let mut base = json!({
            "id": "prod.example", "title": "Example", "summary": "s", "description": "d",
            "category": "production", "kind": "script", "script": "scripts/example.sh",
            "min_role": "operator", "mutating": true, "destructive": false,
            "supports_dry_run": true,
            "params": [
                {"id": "dry", "label": "Flag", "type": "flag", "flag": "--fast"},
                {"id": "resource", "label": "VM", "type": "production", "required": true},
                {"id": "host", "label": "Host", "type": "host", "flag": "--host"},
                {"id": "timeout", "label": "Timeout", "type": "integer", "flag": "--timeout-seconds"},
                {"id": "mode", "label": "Mode", "type": "flag_choice", "choices": [
                    {"value": "a", "label": "A", "flag": "--mode-a"},
                    {"value": "b", "label": "B", "flag": "--mode-b"}
                ]},
                {"id": "level", "label": "Level", "type": "choice", "choices": [
                    {"value": "low", "label": "Low"}, {"value": "high", "label": "High"}
                ]}
            ],
            "locks": ["production:{resource}", "host:{host?}"],
            "target": {"rule": "owner", "param": "resource"},
            "cancel": {"rule": "script"}, "resume": "never"
        });
        for (key, value) in extra.as_object().unwrap() {
            base[key] = value.clone();
        }
        serde_json::from_value(base).unwrap()
    }

    fn values(pairs: Value) -> BTreeMap<String, Value> {
        serde_json::from_value(pairs).unwrap()
    }

    #[test]
    fn builtin_registry_loads() {
        let registry = Registry::builtin().unwrap();
        assert!(registry.get("prod.add").is_some());
        assert!(registry.get("add_prod_vm").is_none());
        for operation in registry.all() {
            let native = native_operations().iter().any(|n| n.id == operation.id);
            assert_eq!(
                operation.kind,
                if native {
                    OperationKind::Native
                } else {
                    OperationKind::Script
                },
                "{}",
                operation.id
            );
        }
    }

    #[test]
    fn arguments_put_options_first_and_dry_run_before_positionals() {
        let prepared = prepare(
            &operation(json!({})),
            &values(json!({
                "dry": true, "resource": "prod3", "host": "mox2", "timeout": 60,
                "mode": "b", "level": "high"
            })),
            true,
        )
        .unwrap();
        assert_eq!(
            prepared.args,
            [
                "--fast",
                "--host",
                "mox2",
                "--timeout-seconds",
                "60",
                "--mode-b",
                "--dry-run",
                "prod3",
                "high"
            ]
        );
        assert_eq!(prepared.summary["timeout"], "60");
        assert_eq!(prepared.summary["dry"], "yes");
        assert_eq!(prepared.locks, ["production:prod3", "host:mox2"]);
        assert!(prepared.dry_run);
    }

    #[test]
    fn an_absent_optional_parameter_widens_the_lock_to_its_class() {
        let prepared = prepare(
            &operation(json!({})),
            &values(json!({"resource": "prod1", "dry": false, "host": ""})),
            false,
        )
        .unwrap();
        assert_eq!(prepared.locks, ["production:prod1", "host"]);
        assert!(!prepared.summary.contains_key("dry"));
        assert_eq!(prepared.args, ["prod1"]);
    }

    #[test]
    fn every_bad_parameter_is_reported() {
        let error = prepare(
            &operation(json!({})),
            &values(json!({
                "host": "mox33", "timeout": "010", "mode": "c", "level": ["low"],
                "argv": "--force"
            })),
            false,
        )
        .unwrap_err();
        let fields: Vec<&str> = error.field_errors.keys().map(String::as_str).collect();
        assert_eq!(
            fields,
            ["argv", "host", "level", "mode", "resource", "timeout"]
        );
        assert_eq!(error.field_errors["resource"], "required");
        let api: ApiError = error.into();
        assert_eq!(api.code, ErrorCode::Invalid);
    }

    #[test]
    fn values_are_canonical_and_positionals_cannot_look_like_options() {
        let op = operation(json!({}));
        for bad in ["stage1prod1", "prod0", "-prod1", "prod1 ", "mox1"] {
            let launch = values(json!({"resource": bad}));
            if bad == "prod1 " {
                assert!(prepare(&op, &launch, false).is_ok(), "values are trimmed");
            } else {
                assert!(prepare(&op, &launch, false).is_err(), "{bad}");
            }
        }
    }

    #[test]
    fn dry_run_needs_support() {
        let op = operation(json!({"supports_dry_run": false}));
        let error = prepare(&op, &values(json!({"resource": "prod1"})), true).unwrap_err();
        assert!(error.message.contains("no dry run"));
    }

    #[test]
    fn the_lab_operations_are_valid_and_their_scripts_exist() {
        let lab: ScriptFile = serde_json::from_str(LAB_OPERATIONS).unwrap();
        let mock = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/mock-scripts");
        for op in &lab.operations {
            assert!(op.id.as_str().starts_with("lab."), "{}", op.id);
            let script = op.script.as_deref().unwrap();
            let name = script.strip_prefix("mock/").unwrap();
            assert!(mock.join(name).is_file(), "{script}");
        }
        Registry::from_parts(SCRIPT_OPERATIONS, lab.operations).unwrap();
    }

    #[test]
    fn native_operations_get_their_parameters_but_no_dry_run_flag() {
        let op = operation(json!({
            "kind": "native", "script": null, "cancel": {"rule": "phases"},
            "params": [{"id": "resource", "label": "VM", "type": "production", "required": true}],
            "locks": ["production:{resource}"]
        }));
        Registry::new(vec![op.clone()]).unwrap();
        let prepared = prepare(&op, &values(json!({"resource": "prod2"})), true).unwrap();
        assert_eq!(prepared.args, ["prod2"]);
        assert_eq!(prepared.locks, ["production:prod2"]);
    }

    #[test]
    fn roles_decide_what_is_allowed() {
        let registry = Registry::builtin().unwrap();
        let allowed = |role, id: &str| {
            registry
                .list_for(role)
                .operations
                .into_iter()
                .find(|v| v.operation.id.as_str() == id)
                .unwrap()
                .allowed
        };
        assert!(allowed(Role::Viewer, "diagnostics.cluster_health"));
        assert!(!allowed(Role::Viewer, "prod.add"));
        assert!(allowed(Role::Operator, "prod.add"));
        assert!(!allowed(Role::Operator, "prod.remove"));
        assert!(allowed(Role::Admin, "prod.remove"));
    }

    #[test]
    fn locks_conflict_with_their_parents_and_children() {
        assert!(locks_conflict("production", "production:prod1"));
        assert!(locks_conflict("production:prod1", "production"));
        assert!(locks_conflict("capacity", "capacity"));
        assert!(!locks_conflict("production:prod1", "production:prod10"));
        assert!(!locks_conflict("production:prod1", "staging:stage1prod1"));
        assert!(!locks_conflict("host", "hosts"));
    }

    #[test]
    fn bad_declarations_are_refused() {
        let cases = [
            (json!({"destructive": true}), "administrators"),
            (json!({"min_role": "viewer"}), "viewers"),
            (json!({"mutating": false}), "read-only"),
            (json!({"locks": []}), "at least one lock"),
            (json!({"script": "../escape.sh"}), "unsafe script"),
            (json!({"script": "/abs/x.sh"}), "unsafe script"),
            (json!({"resume": "checkpoint"}), "never resumed"),
            (json!({"cancel": {"rule": "phases"}}), "cancel rule"),
            (json!({"locks": ["vm:{resource}"]}), "unknown class"),
            (json!({"locks": ["host:{host}"]}), "write {host?}"),
            (json!({"locks": ["production:{timeout}"]}), "doesn't name"),
            (json!({"locks": ["production:{nope}"]}), "no parameter"),
            (json!({"locks": ["capacity", "capacity"]}), "twice"),
            (
                json!({"target": {"rule": "owner", "param": "host"}}),
                "target",
            ),
            (
                json!({"target": {"rule": "host", "param": "host"}}),
                "target",
            ),
            (
                json!({"params": [{"id": "j", "label": "J", "type": "flag", "flag": "--json"}]}),
                "reserved",
            ),
            (
                json!({"params": [{"id": "j", "label": "J", "type": "flag", "flag": "--dry-run"}]}),
                "reserved",
            ),
            (
                json!({"params": [{"id": "j", "label": "J", "type": "integer", "flag": "-j"}]}),
                "bad flag",
            ),
            (
                json!({"params": [
                    {"id": "a", "label": "A", "type": "integer", "flag": "--x"},
                    {"id": "b", "label": "B", "type": "integer", "flag": "--x"}
                ]}),
                "used twice",
            ),
            (
                json!({"params": [{"id": "Bad", "label": "B", "type": "integer"}]}),
                "bad parameter id",
            ),
            (
                json!({"params": [{"id": "c", "label": "C", "type": "choice"}]}),
                "choices",
            ),
            (
                json!({"params": [{"id": "n", "label": "N", "type": "integer", "default": "0"}]}),
                "default",
            ),
            (
                json!({"params": [{"id": "n", "label": "N", "type": "integer", "suggest": "free_host_slots"}]}),
                "suggestions",
            ),
        ];
        for (extra, expected) in cases {
            let mut base = operation(json!({}));
            let patched = operation(extra.clone());
            if extra.get("params").is_some() {
                base.params = patched.params;
                base.locks = vec!["capacity".into()];
                base.target = TargetRule::Any {};
            } else {
                base = patched;
            }
            let error = Registry::new(vec![base]).unwrap_err();
            assert!(error.contains(expected), "{extra}: {error}");
        }
    }

    #[test]
    fn duplicate_ids_are_refused() {
        let error = Registry::new(vec![operation(json!({})), operation(json!({}))]).unwrap_err();
        assert!(error.contains("duplicate"));
    }
}
