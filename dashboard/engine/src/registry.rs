//! The curated workflow registry. Only workflows listed in workflows.json can
//! run, and their arguments are built here from typed, validated values; the
//! frontend never supplies a script path or a raw argument.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::error::{EngineError, Result};
use crate::platform::PlatformInfo;

const REGISTRY_JSON: &str = include_str!("../workflows.json");

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowMode {
    ReadOnly,
    Mutating,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ParamType {
    Flag,
    FlagChoice,
    Choice,
    Host,
    Production,
    Staging,
    Guest,
    Integer,
    Path,
    Hostname,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ParamChoice {
    pub value: String,
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub flag: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Param {
    pub id: String,
    pub label: String,
    #[serde(rename = "type")]
    pub kind: ParamType,
    /// The option name, such as `--host`. Without one the value is positional.
    #[serde(default, skip_serializing_if = "Option::is_none")]
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
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub choices: Vec<ParamChoice>,
    /// Which names the launch form offers for a name-typed param. Without
    /// this, a `host` param offers the current cluster members.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub suggest: Option<ParamSuggest>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ParamSuggest {
    /// Host slots that are not cluster members, preferring those with an
    /// `config/<host>.conf`.
    FreeHostSlots,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Requirements {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub arch: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub os_family: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Workflow {
    pub id: String,
    pub title: String,
    pub summary: String,
    pub description: String,
    pub category: String,
    pub script: String,
    pub mode: WorkflowMode,
    pub destructive: bool,
    /// A mutating workflow that may run alongside other runs of itself. It
    /// still never runs alongside a different mutating workflow.
    #[serde(default)]
    pub concurrent: bool,
    #[serde(default)]
    pub supports_dry_run: bool,
    pub platforms: Vec<String>,
    #[serde(default)]
    pub requires: Requirements,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub platform_note: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notes: Option<String>,
    pub params: Vec<Param>,
}

#[derive(Debug, Deserialize)]
struct RegistryFile {
    schema_version: u32,
    workflows: Vec<Workflow>,
}

/// A workflow as the frontend sees it: its definition plus whether it can run
/// on this workstation and in the selected repository.
#[derive(Debug, Clone, Serialize)]
pub struct WorkflowInfo {
    #[serde(flatten)]
    pub workflow: Workflow,
    pub available: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unavailable_reason: Option<String>,
}

#[derive(Debug, Clone)]
pub struct Registry {
    workflows: Vec<Workflow>,
}

impl Registry {
    pub fn builtin() -> Self {
        Self::from_json(REGISTRY_JSON).expect("the embedded workflow registry is valid")
    }

    pub fn from_json(text: &str) -> std::result::Result<Self, String> {
        let file: RegistryFile = serde_json::from_str(text).map_err(|e| e.to_string())?;
        if file.schema_version != 1 {
            return Err(format!("unsupported registry schema {}", file.schema_version));
        }
        for workflow in &file.workflows {
            if !is_identifier(&workflow.id) {
                return Err(format!("unsafe workflow id {:?}", workflow.id));
            }
            if !is_safe_relative_script(&workflow.script) {
                return Err(format!("unsafe script path {:?}", workflow.script));
            }
            for param in &workflow.params {
                if !is_identifier(&param.id) {
                    return Err(format!("{}: unsafe param id {:?}", workflow.id, param.id));
                }
                if let Some(flag) = &param.flag {
                    if !flag.starts_with("--") || flag == "--json" {
                        return Err(format!("{}: bad flag {:?}", workflow.id, flag));
                    }
                }
            }
        }
        Ok(Self { workflows: file.workflows })
    }

    pub fn all(&self) -> &[Workflow] {
        &self.workflows
    }

    pub fn get(&self, id: &str) -> Result<&Workflow> {
        self.workflows
            .iter()
            .find(|w| w.id == id)
            .ok_or_else(|| EngineError::UnknownWorkflow(id.to_string()))
    }

    pub fn describe(&self, platform: &PlatformInfo, repo_ok: Option<&std::path::Path>) -> Vec<WorkflowInfo> {
        self.workflows
            .iter()
            .map(|workflow| {
                let mut reason = availability(workflow, platform);
                if reason.is_none() {
                    match repo_ok {
                        None => reason = Some("Select a BMAC repository in Settings.".into()),
                        Some(root) if !root.join(&workflow.script).is_file() => {
                            reason = Some(format!("{} is missing from the repository.", workflow.script))
                        }
                        _ => {}
                    }
                }
                WorkflowInfo {
                    workflow: workflow.clone(),
                    available: reason.is_none(),
                    unavailable_reason: reason,
                }
            })
            .collect()
    }
}

/// Why a workflow cannot run on this workstation, or None when it can.
pub fn availability(workflow: &Workflow, platform: &PlatformInfo) -> Option<String> {
    let note = || workflow.platform_note.clone();
    if !workflow.platforms.iter().any(|p| p == &platform.os) {
        return Some(note().unwrap_or_else(|| {
            format!("Not supported on {}; supported on {}.", platform.os_label(), workflow.platforms.join(", "))
        }));
    }
    if let Some(arch) = &workflow.requires.arch {
        if arch != &platform.arch {
            return Some(note().unwrap_or_else(|| format!("Requires an {arch} workstation.")));
        }
    }
    if let Some(family) = &workflow.requires.os_family {
        if platform.os_family.as_deref() != Some(family.as_str()) {
            return Some(note().unwrap_or_else(|| format!("Requires a {family}-based workstation.")));
        }
    }
    None
}

/// A validated invocation: the argv after the script path, and a redaction-free
/// summary of the launch values for display and history.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Invocation {
    pub args: Vec<String>,
    pub summary: BTreeMap<String, String>,
    pub dry_run: bool,
    pub target: Option<String>,
}

pub fn build_invocation(workflow: &Workflow, values: &BTreeMap<String, Value>) -> Result<Invocation> {
    for key in values.keys() {
        if !workflow.params.iter().any(|p| &p.id == key) {
            return Err(EngineError::InvalidArgument(format!("{} has no parameter {key:?}", workflow.id)));
        }
    }
    let mut options = Vec::new();
    let mut positional = Vec::new();
    let mut summary = BTreeMap::new();
    let mut dry_run = false;
    let mut target = None;
    for param in &workflow.params {
        let value = values.get(&param.id).filter(|v| !is_blank(v));
        let Some(value) = value else {
            if param.required {
                return Err(EngineError::InvalidArgument(format!("{} is required", param.label)));
            }
            continue;
        };
        let invalid = |why: &str| EngineError::InvalidArgument(format!("{}: {why}", param.label));
        match param.kind {
            ParamType::Flag => {
                let on = value.as_bool().ok_or_else(|| invalid("expected true or false"))?;
                if on {
                    options.push(param.flag.clone().ok_or_else(|| invalid("flag has no option name"))?);
                    summary.insert(param.id.clone(), "yes".into());
                    if param.id == "dry_run" {
                        dry_run = true;
                    }
                }
            }
            ParamType::FlagChoice => {
                let text = value.as_str().ok_or_else(|| invalid("expected a choice"))?;
                let choice = param
                    .choices
                    .iter()
                    .find(|c| c.value == text)
                    .ok_or_else(|| invalid("not one of the allowed choices"))?;
                options.push(choice.flag.clone().ok_or_else(|| invalid("choice has no option name"))?);
                summary.insert(param.id.clone(), choice.value.clone());
            }
            _ => {
                let text = scalar_text(value).ok_or_else(|| invalid("expected a single value"))?;
                validate_value(param, &text).map_err(|why| invalid(&why))?;
                match &param.flag {
                    Some(flag) => {
                        options.push(flag.clone());
                        options.push(text.clone());
                    }
                    None => positional.push(text.clone()),
                }
                if matches!(
                    param.kind,
                    ParamType::Host | ParamType::Production | ParamType::Staging | ParamType::Guest | ParamType::Hostname
                ) && target.is_none()
                {
                    target = Some(text.clone());
                }
                summary.insert(param.id.clone(), text);
            }
        }
    }
    // The scripts do not all accept `--`, so a positional value that looks
    // like an option must never get this far.
    if positional.iter().any(|p| p.starts_with('-')) {
        return Err(EngineError::InvalidArgument("a value may not start with '-'".into()));
    }
    let mut args = options;
    args.extend(positional);
    Ok(Invocation { args, summary, dry_run, target })
}

fn is_blank(value: &Value) -> bool {
    match value {
        Value::Null => true,
        Value::String(s) => s.trim().is_empty(),
        Value::Bool(false) => false,
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

fn validate_value(param: &Param, text: &str) -> std::result::Result<(), String> {
    match param.kind {
        ParamType::Choice => {
            if param.choices.iter().any(|c| c.value == text) {
                Ok(())
            } else {
                Err("not one of the allowed choices".into())
            }
        }
        ParamType::Host => is_host_name(text).then_some(()).ok_or_else(|| "expected moxN".into()),
        ParamType::Production => is_production_name(text).then_some(()).ok_or_else(|| "expected prodN".into()),
        ParamType::Staging => is_staging_name(text).then_some(()).ok_or_else(|| "expected stageNprodN".into()),
        ParamType::Guest => (is_production_name(text) || is_staging_name(text))
            .then_some(())
            .ok_or_else(|| "expected prodN or stageNprodN".into()),
        ParamType::Integer => {
            let ok = !text.is_empty()
                && text.len() <= 9
                && text.bytes().all(|b| b.is_ascii_digit())
                && !text.starts_with('0');
            ok.then_some(()).ok_or_else(|| "expected a positive whole number".into())
        }
        ParamType::Path => {
            let ok = text.starts_with('/') && text.len() <= 4096 && !text.chars().any(char::is_control);
            ok.then_some(()).ok_or_else(|| "expected an absolute path".into())
        }
        ParamType::Hostname => is_hostname(text).then_some(()).ok_or_else(|| "expected a host name".into()),
        ParamType::Flag | ParamType::FlagChoice => Ok(()),
    }
}

pub fn is_identifier(text: &str) -> bool {
    !text.is_empty()
        && text.len() <= 64
        && text.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-' || b == b'.')
}

fn is_safe_relative_script(path: &str) -> bool {
    !path.starts_with('/')
        && path.ends_with(".sh")
        && path
            .split('/')
            .all(|part| !part.is_empty() && part != "." && part != ".." && is_identifier(part))
}

/// `PREFIX` followed by a positive decimal number without a leading zero.
fn numbered(text: &str, prefix: &str) -> Option<usize> {
    let rest = text.strip_prefix(prefix)?;
    let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
    if digits == 0 || digits > 4 || rest.starts_with('0') {
        return None;
    }
    Some(prefix.len() + digits)
}

pub fn is_host_name(text: &str) -> bool {
    numbered(text, "mox") == Some(text.len())
}

pub fn is_production_name(text: &str) -> bool {
    numbered(text, "prod") == Some(text.len())
}

pub fn is_staging_name(text: &str) -> bool {
    match numbered(text, "stage") {
        Some(end) => is_production_name(&text[end..]),
        None => false,
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
                && label.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn values(pairs: &[(&str, Value)]) -> BTreeMap<String, Value> {
        pairs.iter().map(|(k, v)| (k.to_string(), v.clone())).collect()
    }

    #[test]
    fn builtin_registry_loads_and_scripts_are_unique() {
        let registry = Registry::builtin();
        assert!(registry.all().len() >= 25);
        let mut ids: Vec<_> = registry.all().iter().map(|w| w.id.as_str()).collect();
        ids.sort();
        ids.dedup();
        assert_eq!(ids.len(), registry.all().len());
    }

    #[test]
    fn every_registered_script_exists_in_the_repository() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        for workflow in Registry::builtin().all() {
            assert!(root.join(&workflow.script).is_file(), "{} is missing", workflow.script);
        }
    }

    #[test]
    fn builds_options_then_positionals() {
        let registry = Registry::builtin();
        let workflow = registry.get("remove_prod_vm").unwrap();
        let inv = build_invocation(workflow, &values(&[("dry_run", json!(true)), ("resource", json!("prod2"))])).unwrap();
        assert_eq!(inv.args, vec!["--dry-run", "prod2"]);
        assert!(inv.dry_run);
        assert_eq!(inv.target.as_deref(), Some("prod2"));
    }

    #[test]
    fn rejects_bad_values_and_unknown_params() {
        let registry = Registry::builtin();
        let workflow = registry.get("remove_prod_vm").unwrap();
        for bad in ["prod0", "prod01", "prod", "-rf", "prod1; rm", "stage1prod1"] {
            assert!(build_invocation(workflow, &values(&[("resource", json!(bad))])).is_err(), "{bad}");
        }
        assert!(build_invocation(workflow, &values(&[("other", json!("x"))])).is_err());
        assert!(build_invocation(workflow, &values(&[])).is_err(), "resource is required");
    }

    #[test]
    fn flag_choices_map_to_their_option() {
        let registry = Registry::builtin();
        let workflow = registry.get("add_proxmox_host").unwrap();
        let inv = build_invocation(
            workflow,
            &values(&[("host", json!("mox4")), ("encryption", json!("no-encrypt")), ("boot_tests", json!("skip"))]),
        )
        .unwrap();
        assert_eq!(inv.args, vec!["--host", "mox4", "--no-encrypt", "--skip-boot-tests"]);
        assert!(build_invocation(workflow, &values(&[("encryption", json!("maybe"))])).is_err());
    }

    #[test]
    fn integers_and_paths_are_strict() {
        let registry = Registry::builtin();
        let workflow = registry.get("add_staging_vm").unwrap();
        assert!(build_invocation(workflow, &values(&[("cores", json!(4))])).is_ok());
        assert!(build_invocation(workflow, &values(&[("cores", json!("04"))])).is_err());
        assert!(build_invocation(workflow, &values(&[("cores", json!(-1))])).is_err());
        assert!(build_invocation(workflow, &values(&[("sanitizer", json!("relative.sh"))])).is_err());
        let inv = build_invocation(workflow, &values(&[("sanitizer", json!("/home/a b/s.sh"))])).unwrap();
        assert_eq!(inv.args, vec!["--sanitizer", "/home/a b/s.sh"]);
    }

    #[test]
    fn names() {
        assert!(is_host_name("mox1") && is_host_name("mox12") && !is_host_name("mox0") && !is_host_name("mox"));
        assert!(is_staging_name("stage2prod13") && !is_staging_name("stage0prod1") && !is_staging_name("stage1prod"));
        assert!(is_hostname("qdevice.tail1234.ts.net") && !is_hostname("-x") && !is_hostname("a..b"));
    }
}
