//! Locating and validating the BMAC checkout the dashboard drives.

use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::error::{EngineError, Result};
use crate::platform::command_output;

/// Files that identify a BMAC checkout. A directory is accepted only if all of
/// them exist, so a user-supplied path is never trusted on its own.
const MARKERS: &[&str] = &[
    "scripts/lib/config.sh",
    "scripts/lib/ui_protocol.sh",
    "scripts/utilities/ui_json_run.sh",
    "scripts/lib/ui_protocol.py",
    "scripts/host_runtime/cluster_registry.py",
    "scripts/user_callable/diagnostics/list_hosts.sh",
    "scripts/user_callable/guests/prod/add_prod_vm.sh",
    "scripts/user_callable/hosts/add_proxmox_host.sh",
];

/// Non-secret `config/cluster.conf` settings shown in the dashboard. Nothing
/// else is read from config/: secrets.env is only checked for existence, and
/// `moxN.conf` files are only listed by name.
const CLUSTER_KEYS: &[&str] = &[
    "PROXMOX_CLUSTER_NAME",
    "PROXMOX_QDEVICE_HOST",
    "PROXMOX_CONTROL_NODE",
    "PROXMOX_INTERNAL_DOMAIN",
    "MAX_MOX_HOSTS",
];

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct ClusterSetting {
    pub key: String,
    pub value: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct RepositoryInfo {
    pub root: String,
    pub valid: bool,
    pub problems: Vec<String>,
    pub git_commit: Option<String>,
    pub git_describe: Option<String>,
    pub git_branch: Option<String>,
    pub git_dirty: Option<bool>,
    pub protocol_version: Option<u32>,
    pub cluster_conf_present: bool,
    pub secrets_env_present: bool,
    pub cluster_settings: Vec<ClusterSetting>,
    /// Hosts with an `config/<host>.conf`, such as `mox3`, in numeric order.
    pub host_configs: Vec<String>,
}

pub fn validate(root: &Path) -> Result<PathBuf> {
    let root = root
        .canonicalize()
        .map_err(|e| EngineError::Repository(format!("{}: {e}", root.display())))?;
    let missing: Vec<_> = MARKERS.iter().filter(|m| !root.join(m).is_file()).collect();
    if !missing.is_empty() {
        return Err(EngineError::Repository(format!(
            "{} is not a BMAC checkout with dashboard support (missing {})",
            root.display(),
            missing.iter().map(|m| m.to_string()).collect::<Vec<_>>().join(", ")
        )));
    }
    Ok(root)
}

/// Search the usual places for a checkout: $BMAC_REPO, then the ancestors of
/// the current directory and of the executable (a development build runs from
/// dashboard/target inside the checkout).
pub fn discover() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("BMAC_REPO") {
        if let Ok(root) = validate(Path::new(&path)) {
            return Some(root);
        }
    }
    let mut starts = Vec::new();
    if let Ok(cwd) = std::env::current_dir() {
        starts.push(cwd);
    }
    if let Ok(exe) = std::env::current_exe() {
        starts.push(exe);
    }
    for start in starts {
        for dir in start.ancestors() {
            if let Ok(root) = validate(dir) {
                return Some(root);
            }
        }
    }
    None
}

pub fn info(root: &Path) -> RepositoryInfo {
    let mut problems = Vec::new();
    let valid = match validate(root) {
        Ok(_) => true,
        Err(error) => {
            problems.push(error.to_string());
            false
        }
    };
    let git = |args: &[&str]| {
        let mut full = vec!["-C", root.to_str().unwrap_or(".")];
        full.extend_from_slice(args);
        command_output("git", &full)
    };
    let git_commit = git(&["rev-parse", "--short=12", "HEAD"]);
    let git_dirty = git_commit.as_ref().map(|_| {
        std::process::Command::new("git")
            .args(["-C", root.to_str().unwrap_or("."), "status", "--porcelain", "--untracked-files=no"])
            .output()
            .map(|o| !o.stdout.is_empty())
            .unwrap_or(false)
    });
    let protocol_version = std::fs::read_to_string(root.join("scripts/lib/ui_protocol.sh"))
        .ok()
        .and_then(|text| {
            text.lines().find_map(|line| line.strip_prefix("BMAC_UI_PROTOCOL_VERSION=")?.trim().parse().ok())
        });
    if valid && protocol_version != Some(crate::protocol::PROTOCOL_VERSION) {
        problems.push(format!(
            "the checkout speaks bmac-ui version {:?}; this dashboard requires version {}",
            protocol_version,
            crate::protocol::PROTOCOL_VERSION
        ));
    }
    let cluster_conf = root.join("config/cluster.conf");
    let cluster_settings = std::fs::read_to_string(&cluster_conf)
        .map(|text| cluster_settings(&text))
        .unwrap_or_default();
    RepositoryInfo {
        root: root.display().to_string(),
        valid: valid && problems.is_empty(),
        problems,
        git_describe: git(&["describe", "--tags", "--always", "--dirty"]),
        git_branch: git(&["rev-parse", "--abbrev-ref", "HEAD"]),
        git_commit,
        git_dirty,
        protocol_version,
        cluster_conf_present: cluster_conf.is_file(),
        secrets_env_present: root.join("config/secrets.env").is_file(),
        cluster_settings,
        host_configs: host_configs(&root.join("config")),
    }
}

fn host_configs(env: &Path) -> Vec<String> {
    let mut found: Vec<(u32, String)> = std::fs::read_dir(env)
        .into_iter()
        .flatten()
        .filter_map(|entry| {
            let entry = entry.ok()?;
            let name = entry.file_name().into_string().ok()?;
            let host = name.strip_suffix(".conf")?;
            let digits = host.strip_prefix("mox")?;
            if digits.is_empty() || digits.len() > 4 || digits.starts_with('0') || !digits.bytes().all(|b| b.is_ascii_digit()) {
                return None;
            }
            if !entry.path().is_file() {
                return None;
            }
            Some((digits.parse().ok()?, host.to_string()))
        })
        .collect();
    found.sort();
    found.into_iter().map(|(_, host)| host).collect()
}

fn cluster_settings(text: &str) -> Vec<ClusterSetting> {
    let mut found = Vec::new();
    for key in CLUSTER_KEYS {
        let value = text.lines().rev().find_map(|line| {
            let rest = line.trim_start().strip_prefix(key)?.strip_prefix('=')?;
            let rest = rest.split(" #").next().unwrap_or("").trim();
            Some(rest.trim_matches(|c| c == '"' || c == '\'').to_string())
        });
        if let Some(value) = value.filter(|v| !v.is_empty() && !v.contains('$')) {
            found.push(ClusterSetting { key: key.to_string(), value });
        }
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn this_checkout_validates() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let root = validate(&root).expect("the enclosing checkout is a BMAC repository");
        let info = info(&root);
        assert_eq!(info.protocol_version, Some(2));
    }

    #[test]
    fn rejects_other_directories() {
        let dir = tempfile::tempdir().unwrap();
        assert!(validate(dir.path()).is_err());
    }

    #[test]
    fn lists_host_configs_in_numeric_order() {
        let dir = tempfile::tempdir().unwrap();
        for name in ["mox10.conf", "mox2.conf", "mox1.conf", "mox0.conf", "mox3_dot_conf", "moxa.conf", "cluster.conf"] {
            std::fs::write(dir.path().join(name), "").unwrap();
        }
        std::fs::create_dir(dir.path().join("mox4.conf")).unwrap();
        assert_eq!(host_configs(dir.path()), vec!["mox1", "mox2", "mox10"]);
    }

    #[test]
    fn reads_only_allowlisted_settings() {
        let text = "PROXMOX_CLUSTER_NAME=\"bmac\"\nSECRET_THING=x\nPROXMOX_QDEVICE_HOST=qd # note\nMAX_MOX_HOSTS=${X}\n";
        let settings = cluster_settings(text);
        assert_eq!(
            settings,
            vec![
                ClusterSetting { key: "PROXMOX_CLUSTER_NAME".into(), value: "bmac".into() },
                ClusterSetting { key: "PROXMOX_QDEVICE_HOST".into(), value: "qd".into() },
            ]
        );
    }
}
