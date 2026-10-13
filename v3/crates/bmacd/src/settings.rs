// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

//! Cluster settings and host facts (`ARCHITECTURE.md`, "Where state
//! lives"): every key of v2's `cluster.conf` and `moxN.conf`, stored in
//! pmxcfs under `/etc/pve/priv/bmac/config/` and edited with revision
//! checks, and the renderer that writes them back out as those files in
//! `/run/bmac/config/` for script operations, in exactly the format
//! `scripts/lib/config.sh` parses.
//!
//! Each key has a kind, and the checks between keys are the ones
//! `config.sh` makes (`_config_validate_cluster`, `_config_validate_mox`).
//! Where `config.sh` is loose, this layer is stricter: addresses are always
//! full dotted quads, and empty values are refused (a key is either set or
//! absent). So anything stored here loads in `config.sh` unchanged, and
//! anything `config.sh` rejects is refused here first. No secret is a
//! setting: `secrets.env` has no counterpart.

use std::collections::BTreeSet;
use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::Duration;

use bmac_core::HostSlot;
use bmac_pmxcfs::{LockOptions, Pmxcfs, PmxcfsError, RegistryLock, read_record, write_record};
use bmac_proto::api::{ApiError, ErrorCode};
use bmac_proto::settings::{HostFactsRecord, SettingsRecord};
pub use bmac_settings::{
    BridgeAddresses, CLUSTER_KEYS, HEADER, HOST_KEYS, Key, Kind, Values, bridge_addresses,
    check_locked, check_private_ips, file_text, parse_conf, ssh_config_text, validate_cluster,
    validate_host,
};
use serde::{Deserialize, Serialize};

pub const CONFIG_DIR: &str = "/etc/pve/priv/bmac/config";
/// Where scripts find `cluster.conf` and the `moxN.conf` files.
pub const RUN_CONFIG_DIR: &str = "/run/bmac/config";
/// The SSH client config scripts use to reach other hosts (`ssh -F`).
pub const SSH_CONFIG: &str = "/etc/bmac/ssh_config";
/// `cluster.conf` values the package fixes; they aren't settings.
pub const INSTALLED: &[(&str, &str)] = &[
    ("CLUSTER_STATE_DIR", bmac_registry::DEFAULT_ROOT),
    (
        "GUEST_ROLE_HOOK_PATH",
        "local:snippets/bmac-guest-role-hook.sh",
    ),
    // The tags mark every VM BMAC manages, so they never change; the
    // registry's policy records the same ones (`cluster_registry.py init`).
    ("PRODUCTION_VM_TAG", "bmac-production"),
    ("STAGING_VM_TAG", "bmac-staging"),
    ("EVICTABLE_VM_TAG", "evictable"),
    ("PURPOSE_TAG_PREFIX", "purpose-"),
];
const LOCK_PREFIX: &str = "bmac-config-";
pub const RECORD_SCHEMA: u64 = 1;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClusterSettings {
    pub schema_version: u64,
    pub record_type: String,
    #[serde(default)]
    pub revision: u64,
    pub values: Values,
    pub updated_by: String,
    pub updated_at_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HostFacts {
    pub schema_version: u64,
    pub record_type: String,
    #[serde(default)]
    pub revision: u64,
    pub host: HostSlot,
    pub values: Values,
    pub updated_by: String,
    pub updated_at_ms: i64,
}

impl ClusterSettings {
    pub fn view(&self) -> SettingsRecord {
        SettingsRecord {
            revision: self.revision,
            values: self.values.clone(),
            updated_by: self.updated_by.clone(),
            updated_at_ms: self.updated_at_ms,
        }
    }
}

impl HostFacts {
    pub fn view(&self) -> HostFactsRecord {
        HostFactsRecord {
            host: self.host,
            record: SettingsRecord {
                revision: self.revision,
                values: self.values.clone(),
                updated_by: self.updated_by.clone(),
                updated_at_ms: self.updated_at_ms,
            },
        }
    }
}

fn invalid(message: impl Into<String>) -> ApiError {
    ApiError::new(ErrorCode::Invalid, message)
}

fn storage(e: PmxcfsError) -> ApiError {
    match e {
        PmxcfsError::RevisionConflict { .. } => ApiError::new(
            ErrorCode::Conflict,
            "the settings changed since they were read; reload and try again",
        ),
        PmxcfsError::LockTimeout { .. } => {
            ApiError::new(ErrorCode::Unavailable, "the cluster is busy; try again")
        }
        other => ApiError::new(ErrorCode::Internal, format!("cluster settings: {other}")),
    }
}

/// The settings records in pmxcfs.
#[derive(Clone)]
pub struct Settings {
    pmxcfs: Pmxcfs,
    dir: PathBuf,
    lock_timeout: Duration,
}

impl Settings {
    pub fn new(pmxcfs: Pmxcfs, dir: impl Into<PathBuf>) -> Settings {
        Settings {
            pmxcfs,
            dir: dir.into(),
            lock_timeout: Duration::from_secs(15),
        }
    }

    pub fn system() -> Settings {
        Settings::new(Pmxcfs::system(), CONFIG_DIR)
    }

    fn cluster_path(&self) -> PathBuf {
        self.dir.join("cluster.json")
    }

    fn host_path(&self, host: HostSlot) -> PathBuf {
        self.dir.join("hosts").join(format!("{host}.json"))
    }

    fn read<T: serde::de::DeserializeOwned>(
        &self,
        path: &Path,
        what: &str,
    ) -> Result<Option<T>, ApiError> {
        let Some(value) = read_record(&self.pmxcfs, path, what, RECORD_SCHEMA).map_err(storage)?
        else {
            return Ok(None);
        };
        serde_json::from_value(value)
            .map(Some)
            .map_err(|e| ApiError::new(ErrorCode::Internal, format!("{}: {e}", path.display())))
    }

    pub fn cluster(&self) -> Result<Option<ClusterSettings>, ApiError> {
        self.read(&self.cluster_path(), "cluster settings")
    }

    pub fn host(&self, host: HostSlot) -> Result<Option<HostFacts>, ApiError> {
        let facts: Option<HostFacts> = self.read(&self.host_path(host), "host facts")?;
        match facts {
            Some(facts) if facts.host != host => Err(ApiError::new(
                ErrorCode::Internal,
                format!(
                    "{} holds the facts of {}",
                    self.host_path(host).display(),
                    facts.host
                ),
            )),
            other => Ok(other),
        }
    }

    pub fn hosts(&self) -> Result<Vec<HostFacts>, ApiError> {
        let mut out = Vec::new();
        for host in HostSlot::all() {
            if let Some(facts) = self.host(host)? {
                out.push(facts);
            }
        }
        Ok(out)
    }

    fn locked<T>(
        &self,
        f: impl FnOnce(&RegistryLock) -> Result<T, ApiError>,
    ) -> Result<T, ApiError> {
        self.pmxcfs
            .ensure_directory(&self.dir.join("hosts"), 0o700)
            .map_err(storage)?;
        let options = LockOptions {
            timeout: self.lock_timeout,
            prefix: LOCK_PREFIX.to_string(),
            short_lived: true,
            ..LockOptions::default()
        };
        let lock = RegistryLock::acquire(&self.pmxcfs, &self.dir, &options).map_err(storage)?;
        let result = f(&lock);
        let released = lock.release().map_err(storage);
        let value = result?;
        released?;
        Ok(value)
    }

    /// Replaces the cluster settings at revision `expected` (`None` to
    /// create them). Every host's facts must stay valid with the new
    /// settings.
    pub fn save_cluster(
        &self,
        values: Values,
        expected: Option<u64>,
        by: &str,
        now_ms: i64,
    ) -> Result<ClusterSettings, ApiError> {
        validate_cluster(&values)?;
        self.locked(|lock| {
            if let Some(current) = self.cluster()? {
                check_locked(&current.values, &values)?;
            }
            let hosts = self.hosts()?;
            check_private_ips(hosts.iter().map(|f| (f.host, &f.values)), &values)?;
            for facts in hosts {
                validate_host(facts.host, &facts.values, &values).map_err(|e| {
                    invalid(format!(
                        "{}'s facts would no longer be valid: {}",
                        facts.host, e.message
                    ))
                })?;
            }
            let mut record = ClusterSettings {
                schema_version: RECORD_SCHEMA,
                record_type: "cluster_settings".into(),
                revision: 0,
                values,
                updated_by: by.into(),
                updated_at_ms: now_ms,
            };
            let value = serde_json::to_value(&record).expect("settings serialize");
            record.revision = write_record(lock, &self.cluster_path(), value, expected)
                .map_err(storage)?
                .revision;
            Ok(record)
        })
    }

    /// Replaces one host's facts at revision `expected` (`None` to create
    /// them), checked against the current cluster settings.
    pub fn save_host(
        &self,
        host: HostSlot,
        values: Values,
        expected: Option<u64>,
        by: &str,
        now_ms: i64,
    ) -> Result<HostFacts, ApiError> {
        self.locked(|lock| {
            let cluster = self.cluster()?.ok_or_else(|| {
                ApiError::new(
                    ErrorCode::Conflict,
                    "set the cluster settings before any host's facts",
                )
            })?;
            validate_host(host, &values, &cluster.values)?;
            let others = self.hosts()?;
            check_private_ips(
                others
                    .iter()
                    .filter(|f| f.host != host)
                    .map(|f| (f.host, &f.values))
                    .chain([(host, &values)]),
                &cluster.values,
            )?;
            let mut record = HostFacts {
                schema_version: RECORD_SCHEMA,
                record_type: "host_facts".into(),
                revision: 0,
                host,
                values,
                updated_by: by.into(),
                updated_at_ms: now_ms,
            };
            let value = serde_json::to_value(&record).expect("facts serialize");
            record.revision = write_record(lock, &self.host_path(host), value, expected)
                .map_err(storage)?
                .revision;
            Ok(record)
        })
    }

    /// Replaces the stored settings with `dir`'s `cluster.conf` and any
    /// `moxN.conf` files, each checked as an edit would be. Hosts without a
    /// file keep their facts. Returns the hosts imported.
    pub fn import(&self, dir: &Path, by: &str, now_ms: i64) -> Result<Vec<HostSlot>, ApiError> {
        let read = |name: &str| -> Result<Option<Values>, ApiError> {
            let path = dir.join(name);
            match std::fs::read_to_string(&path) {
                Ok(text) => parse_conf(&text)
                    .map(Some)
                    .map_err(|e| invalid(format!("{}: {e}", path.display()))),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
                Err(e) => Err(invalid(format!("cannot read {}: {e}", path.display()))),
            }
        };
        let cluster = read("cluster.conf")?
            .ok_or_else(|| invalid(format!("{} has no cluster.conf", dir.display())))?;
        let current = self.cluster()?.map(|c| c.revision);
        self.save_cluster(cluster, current, by, now_ms)?;
        let mut imported = Vec::new();
        for host in HostSlot::all() {
            if let Some(values) = read(&format!("{host}.conf"))? {
                let current = self.host(host)?.map(|f| f.revision);
                self.save_host(host, values, current, by, now_ms)?;
                imported.push(host);
            }
        }
        Ok(imported)
    }

    /// Writes `ssh_config_text` for the current settings to `path` (mode
    /// 0600); with no cluster settings yet, it names no hosts.
    pub fn render_ssh_config(&self, path: &Path) -> Result<(), String> {
        let text = match self.cluster().map_err(|e| e.message)? {
            Some(cluster) => {
                let hosts = self.hosts().map_err(|e| e.message)?;
                ssh_config_text(&cluster.values, hosts.iter().map(|f| (f.host, &f.values)))
                    .map_err(|e| e.message)?
            }
            None => HEADER.to_string(),
        };
        if let Some(parent) = path.parent() {
            std::fs::DirBuilder::new()
                .recursive(true)
                .mode(0o755)
                .create(parent)
                .map_err(|e| format!("cannot make {}: {e}", parent.display()))?;
        }
        write_private(path, &text).map_err(|e| format!("cannot write {}: {e}", path.display()))
    }

    /// Writes what scripts, the lifecycle hook, and the timers read:
    /// `RUN_CONFIG_DIR` and `SSH_CONFIG`.
    pub fn render_for_scripts(&self) -> Result<(), String> {
        self.render_to(Path::new(RUN_CONFIG_DIR))?;
        self.render_ssh_config(Path::new(SSH_CONFIG))
    }

    /// Writes the current settings to `dir` for scripts. With no cluster
    /// settings yet, `dir` is left empty.
    pub fn render_to(&self, dir: &Path) -> Result<(), String> {
        let cluster = self.cluster().map_err(|e| e.message)?;
        let hosts = match &cluster {
            Some(_) => self.hosts().map_err(|e| e.message)?,
            None => Vec::new(),
        };
        let hosts: Vec<(HostSlot, &Values)> = hosts.iter().map(|f| (f.host, &f.values)).collect();
        render(dir, cluster.as_ref().map(|c| &c.values), &hosts)
            .map_err(|e| format!("cannot write {}: {e}", dir.display()))
    }
}

/// The text of `cluster.conf`: the settings, then the `INSTALLED` values.
pub fn cluster_text(values: &Values) -> String {
    let mut text = file_text(CLUSTER_KEYS, values);
    for (name, value) in INSTALLED {
        text.push_str(&format!("{name}='{value}'\n"));
    }
    text
}

/// Replaces `path` with `text` (mode 0600), unless it already is exactly
/// that.
fn write_private(path: &Path, text: &str) -> std::io::Result<()> {
    if let Ok(meta) = std::fs::symlink_metadata(path)
        && meta.is_file()
        && meta.permissions().mode() & 0o7777 == 0o600
        && std::fs::read(path).ok().as_deref() == Some(text.as_bytes())
    {
        return Ok(());
    }
    let temp = path.with_extension("tmp");
    let _ = std::fs::remove_file(&temp);
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&temp)?;
    file.write_all(text.as_bytes())?;
    file.sync_all()?;
    std::fs::rename(&temp, path)
}

/// Writes `cluster.conf` and each host's `moxN.conf` into `dir` (mode
/// 0700, files 0600), and removes the files of hosts that have none. The
/// values aren't checked here.
pub fn render(
    dir: &Path,
    cluster: Option<&Values>,
    hosts: &[(HostSlot, &Values)],
) -> std::io::Result<()> {
    match std::fs::DirBuilder::new().mode(0o700).create(dir) {
        Err(e) if e.kind() != std::io::ErrorKind::AlreadyExists => return Err(e),
        _ => {}
    }
    let meta = std::fs::symlink_metadata(dir)?;
    if !meta.is_dir() {
        return Err(std::io::Error::other("it is not a directory"));
    }
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    let mut wanted = BTreeSet::new();
    if let Some(cluster) = cluster {
        write_private(&dir.join("cluster.conf"), &cluster_text(cluster))?;
        wanted.insert("cluster.conf".to_string());
        for (host, values) in hosts {
            let name = format!("{host}.conf");
            write_private(&dir.join(&name), &file_text(HOST_KEYS, values))?;
            wanted.insert(name);
        }
    }
    for entry in std::fs::read_dir(dir)? {
        let name = entry?.file_name().to_string_lossy().into_owned();
        if name.ends_with(".conf") && !wanted.contains(&name) {
            std::fs::remove_file(dir.join(&name))?;
        }
    }
    Ok(())
}
