// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

//! Registry records: typed readers for the v2 schema, later the native registry.
//!
//! Every record is checked as v2's `cluster_registry.py` checks it, with the
//! same messages and unknown keys refused, then converted to its typed form.
//! Writing stays with the Python CLI until M11.

mod check;
mod cleanup;
mod history;
mod hosts;
mod ingress;
mod orchestration;
mod policy;
pub mod reconcile;
mod resource;
mod state;

use std::path::{Path, PathBuf};

use bmac_core::{GuestName, HostSlot};
use bmac_pmxcfs::{Pmxcfs, PmxcfsError};
use serde_json::Value;

pub use cleanup::{CleanupRecord, validate_cleanup, validate_cleanup_target};
pub use history::{HistoryRecord, validate_history};
pub use hosts::{ClusterControl, HostSlotRecord, validate_cluster_control, validate_host_slot};
pub use ingress::{
    Allowance, IngressDesired, IngressGeneration, IngressNodeStatus, IngressTransition,
    validate_desired, validate_generation, validate_node_status, validate_transition,
};
pub use orchestration::{Lease, Orchestration, validate_orchestration};
pub use policy::{
    DiskAllocation, GIB, MAX_DISK_BYTES, MIB, OffsetRange, Policy, PolicyDefaults, PolicyLimits,
    PolicyNetwork, PolicyTags, Spec, VmidRange, validate_policy, validate_spec,
};
pub use resource::{
    Domains, Guid, ProxmoxMeta, Purpose, Resource, Snapshot, validate_registry_invariants,
    validate_resource,
};
pub use state::{
    CleanupAction, CleanupState, HostSlotState, IngressNodeState, InstallMode, InstallPhase, Kind,
    ResourceState, state_transitions_json,
};

pub const SCHEMA_VERSION: u64 = 1;
pub const MAX_ROUTES: usize = 4096;
pub const MAX_HISTORY_RECORDS: usize = 256;
pub const MAX_COMPLETED_CLEANUP_RECORDS: usize = 512;
pub const MAX_CLEANUP_RECORDS: usize = 4096;
pub const MAX_INGRESS_GENERATIONS: usize = 8;
pub const MIN_INGRESS_COMPATIBLE_WINDOW_SECONDS: u64 = 60;
pub const MAX_INGRESS_COMPATIBLE_WINDOW_SECONDS: u64 = 3600;

/// Where v3 keeps the registry. v2 used `/etc/pve/priv/app-ha`.
pub const DEFAULT_ROOT: &str = "/etc/pve/priv/bmac/registry";

#[derive(Debug, thiserror::Error)]
pub enum RegistryError {
    #[error("{0}")]
    Invalid(String),
    #[error(transparent)]
    Storage(#[from] PmxcfsError),
}

pub type Result<T> = std::result::Result<T, RegistryError>;

/// Reads one registry directory.
pub struct Registry {
    pmxcfs: Pmxcfs,
    root: PathBuf,
}

impl Registry {
    pub fn new(pmxcfs: Pmxcfs, root: impl Into<PathBuf>) -> Registry {
        Registry {
            pmxcfs,
            root: root.into(),
        }
    }

    pub fn system() -> Registry {
        Registry::new(Pmxcfs::system(), DEFAULT_ROOT)
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    fn read(&self, path: &Path, context: &str) -> Result<Value> {
        Ok(self.pmxcfs.read_json(path, context)?)
    }

    fn read_optional(&self, path: &Path, context: &str) -> Result<Option<Value>> {
        if self.pmxcfs.exists(path) {
            self.read(path, context).map(Some)
        } else {
            Ok(None)
        }
    }

    /// The `*.json` files in a directory, sorted, without hidden files (as
    /// Python's `glob("*.json")`). A missing directory has none.
    fn listing(&self, dir: &Path) -> Result<Vec<(String, PathBuf)>> {
        let entries = match std::fs::read_dir(dir) {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(source) => {
                return Err(PmxcfsError::Io {
                    context: format!("cannot list {}", dir.display()),
                    source,
                }
                .into());
            }
        };
        let mut files = Vec::new();
        for entry in entries {
            let entry = entry.map_err(|source| PmxcfsError::Io {
                context: format!("cannot list {}", dir.display()),
                source,
            })?;
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.ends_with(".json") && !name.starts_with('.') {
                files.push((name, entry.path()));
            }
        }
        files.sort();
        Ok(files)
    }

    fn named<T>(
        &self,
        dir: &str,
        context: &str,
        validate: impl Fn(&Value) -> Result<T>,
        key: impl Fn(&T) -> String,
        mismatch: &str,
    ) -> Result<Vec<T>> {
        let mut records = Vec::new();
        for (name, path) in self.listing(&self.root.join(dir))? {
            let record = validate(&self.read(&path, context)?)?;
            if name != format!("{}.json", key(&record)) {
                return Err(RegistryError::Invalid(format!(
                    "{mismatch}: {}",
                    path.display()
                )));
            }
            records.push(record);
        }
        Ok(records)
    }

    pub fn policy(&self) -> Result<Policy> {
        validate_policy(&self.read(&self.root.join("policy.json"), "registry policy")?)
    }

    /// Every guest, checked together, production first.
    pub fn resources(&self) -> Result<Vec<Resource>> {
        let policy = self.policy()?;
        let mut records = self.named(
            "resources",
            "resource record",
            |v| validate_resource(v, &policy),
            |r| r.name.to_string(),
            "resource filename does not match record name",
        )?;
        validate_registry_invariants(&records)?;
        records.sort_by_key(Resource::sort_key);
        Ok(records)
    }

    pub fn resource(&self, name: GuestName) -> Result<Resource> {
        let policy = self.policy()?;
        let path = self.root.join("resources").join(format!("{name}.json"));
        validate_resource(&self.read(&path, "resource record")?, &policy)
    }

    /// The guest's resource, or None when it has no record.
    pub fn find_resource(&self, name: GuestName) -> Result<Option<Resource>> {
        let path = self.root.join("resources").join(format!("{name}.json"));
        match path.symlink_metadata() {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            _ => self.resource(name).map(Some),
        }
    }

    pub fn cleanup_records(&self) -> Result<Vec<CleanupRecord>> {
        let mut records = self.named(
            "deferred-cleanup",
            "deferred cleanup record",
            validate_cleanup,
            |r| r.id.clone(),
            "cleanup filename does not match record id",
        )?;
        records.sort_by(|a, b| (&a.created_at, &a.id).cmp(&(&b.created_at, &b.id)));
        Ok(records)
    }

    /// Release history, oldest first (names start with the release time).
    pub fn history(&self) -> Result<Vec<HistoryRecord>> {
        self.policy()?;
        let mut records = Vec::new();
        for (_, path) in self.listing(&self.root.join("history"))? {
            records.push(validate_history(&self.read(&path, "history record")?)?);
        }
        Ok(records)
    }

    /// A production guest's install orchestration record, refused when it
    /// belongs to an earlier guest of the same name
    /// (`command_orchestration_get`, 2860–2878).
    pub fn orchestration(&self, name: GuestName) -> Result<Option<Orchestration>> {
        let resources = self.resources()?;
        let Some(resource) = resources.iter().find(|r| r.name == name) else {
            return Err(RegistryError::Invalid(format!(
                "resource does not exist: {name}"
            )));
        };
        if resource.kind != Kind::Production {
            return Err(RegistryError::Invalid(
                "production orchestration is only valid for production".into(),
            ));
        }
        let path = self.root.join("orchestration").join(format!("{name}.json"));
        let Some(value) = self.read_optional(&path, "orchestration record")? else {
            return Ok(None);
        };
        let record = validate_orchestration(&value)?;
        if record.resource_id != resource.id {
            return Err(RegistryError::Invalid(
                "orchestration record belongs to an obsolete resource".into(),
            ));
        }
        Ok(Some(record))
    }

    pub fn host_slots(&self) -> Result<Vec<HostSlotRecord>> {
        let policy = self.policy()?;
        let mut records = self.named(
            "hosts",
            "host slot record",
            |v| validate_host_slot(v, &policy),
            |r| r.node.to_string(),
            "host slot filename does not match record",
        )?;
        records.sort_by_key(|r| r.node);
        Ok(records)
    }

    pub fn cluster_control(&self) -> Result<Option<ClusterControl>> {
        let path = self.root.join("cluster-control.json");
        if !self.pmxcfs.exists(&path) {
            return Ok(None);
        }
        let policy = self.policy()?;
        validate_cluster_control(&self.read(&path, "cluster control record")?, &policy).map(Some)
    }

    /// The desired ingress generation, checked against its generation record.
    pub fn ingress_desired(&self) -> Result<Option<IngressDesired>> {
        let path = self.root.join("ingress/desired.json");
        if !self.pmxcfs.exists(&path) {
            return Ok(None);
        }
        let policy = self.policy()?;
        let desired = validate_desired(&self.read(&path, "desired ingress generation")?, &policy)?;
        let generation_path = self
            .root
            .join("ingress/generations")
            .join(format!("{}.json", desired.generation));
        let generation = validate_generation(
            &self.read(&generation_path, "desired ingress generation metadata")?,
        )?;
        if generation.generation != desired.generation
            || generation.bundle_sha256 != desired.bundle_sha256
            || generation.route_count != desired.route_count
        {
            return Err(RegistryError::Invalid(
                "desired ingress pointer differs from generation metadata".into(),
            ));
        }
        Ok(Some(desired))
    }

    pub fn ingress_generations(&self) -> Result<Vec<IngressGeneration>> {
        self.named(
            "ingress/generations",
            "ingress generation metadata",
            validate_generation,
            |r| r.generation.clone(),
            "ingress generation filename does not match record",
        )
    }

    pub fn ingress_transition(&self) -> Result<Option<IngressTransition>> {
        let path = self.root.join("ingress/transition.json");
        if !self.pmxcfs.exists(&path) {
            return Ok(None);
        }
        let policy = self.policy()?;
        validate_transition(&self.read(&path, "ingress transition")?, &policy).map(Some)
    }

    pub fn ingress_node_statuses(&self) -> Result<Vec<IngressNodeStatus>> {
        let policy = self.policy()?;
        let mut records = self.named(
            "ingress/nodes",
            "ingress node status",
            |v| validate_node_status(v, &policy),
            |r| r.node.to_string(),
            "ingress node status filename does not match record",
        )?;
        records.sort_by_key(|r| r.node);
        Ok(records)
    }

    pub fn host_slot(&self, node: HostSlot) -> Result<Option<HostSlotRecord>> {
        Ok(self.host_slots()?.into_iter().find(|r| r.node == node))
    }
}
