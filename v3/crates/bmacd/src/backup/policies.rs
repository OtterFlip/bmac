// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

//! Backup policies (`BACKUP.md`, "Backup policies"): one per production VM,
//! each owning the schedule record that runs its capture job, so every host
//! evaluates it and the first admission wins (`ARCHITECTURE.md`, "Schedules
//! without a leader").

use std::collections::BTreeMap;

use bmac_backup::records::{Enrollment, Freeze, Policy, RECORD_SCHEMA};
use bmac_backup::retention::LocalRetention;
use bmac_core::GuestName;
use bmac_proto::api::{ApiError, ErrorCode, Role};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

use super::capture::{self, Guests};
use super::store::BackupStore;
use crate::schedules::{Recurrence, Schedule, Schedules};

const MAX_NEWEST: u32 = 1000;
const MAX_DAYS: u32 = 366;
const MAX_WEEKS: u32 = 104;

/// The default schedule: daily at 03:00 in the cluster's time zone.
pub fn default_schedule() -> Recurrence {
    Recurrence::Daily { at: "03:00".into() }
}

/// A policy as the Dashboard edits it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PolicyChange {
    pub vm: GuestName,
    #[serde(default = "default_schedule")]
    pub every: Recurrence,
    #[serde(default)]
    pub appliances: Vec<Uuid>,
    #[serde(default)]
    pub local_retention: LocalRetention,
    #[serde(default)]
    pub freeze: Freeze,
    #[serde(default)]
    pub accept_multi_pool: bool,
    pub enabled: bool,
    /// The policy's revision as read, or `None` for a new policy.
    #[serde(default)]
    pub revision: Option<u64>,
}

fn invalid(field: &str, message: impl Into<String>) -> ApiError {
    let message = message.into();
    let mut error = ApiError::new(ErrorCode::Invalid, format!("{field}: {message}"));
    error.field_errors.insert(field.into(), message);
    error
}

pub fn validate_retention(retention: &LocalRetention) -> Result<(), ApiError> {
    if retention.newest > MAX_NEWEST {
        return Err(invalid(
            "local_retention",
            format!("keep at most the newest {MAX_NEWEST}"),
        ));
    }
    if retention.days > MAX_DAYS {
        return Err(invalid(
            "local_retention",
            format!("keep at most {MAX_DAYS} days"),
        ));
    }
    if retention.weeks > MAX_WEEKS {
        return Err(invalid(
            "local_retention",
            format!("keep at most {MAX_WEEKS} weeks"),
        ));
    }
    Ok(())
}

/// Saves the policy and its schedule; the schedule first, so a policy
/// never exists without one.
pub fn save(
    store: &BackupStore,
    schedules: &Schedules,
    guests: &dyn Guests,
    change: PolicyChange,
    by: &str,
    now_ms: i64,
) -> Result<Policy, ApiError> {
    let production = guests
        .production(&change.vm)
        .map_err(|m| invalid("vm", m))?;
    validate_retention(&change.local_retention)?;
    change.every.validate().map_err(|m| invalid("every", m))?;
    let appliances = store.appliances()?;
    let mut assigned = Vec::new();
    for id in &change.appliances {
        let Some(appliance) = appliances.iter().find(|a| a.appliance_uuid == *id) else {
            return Err(invalid("appliances", format!("no enrolled appliance {id}")));
        };
        if appliance.enrollment != Enrollment::Approved || appliance.retired {
            return Err(invalid(
                "appliances",
                format!("{} isn't an approved, active appliance", appliance.name),
            ));
        }
        if !assigned.contains(id) {
            assigned.push(*id);
        }
    }
    let current = store.policy(&production.vm_uuid)?;
    if current.as_ref().map(|p| p.revision) != change.revision {
        return Err(ApiError::new(
            ErrorCode::Conflict,
            "the backup policy changed since it was read; reload and try again",
        ));
    }
    let schedule_id = Policy::schedule_id_for(&production.vm_uuid);
    let existing = schedules.get(&schedule_id)?;
    schedules.save(
        Schedule {
            schema_version: 0,
            record_type: String::new(),
            revision: 0,
            id: schedule_id.clone(),
            title: format!("Back up {}", change.vm),
            operation: capture::OPERATION.parse().expect("a valid operation id"),
            parameters: BTreeMap::from([("vm".to_string(), Value::String(change.vm.to_string()))]),
            every: change.every,
            enabled: change.enabled,
            run_as: Role::Operator,
            updated_by: by.to_string(),
            updated_at_ms: now_ms,
        },
        existing.map(|s| s.revision),
    )?;
    store.locked(|l| {
        l.save_policy(Policy {
            schema_version: RECORD_SCHEMA,
            record_type: String::new(),
            revision: change.revision.unwrap_or(0),
            vm_uuid: production.vm_uuid,
            vm: change.vm.to_string(),
            schedule_id,
            appliances: assigned,
            local_retention: change.local_retention,
            started_s: current.as_ref().and_then(|p| p.started_s),
            freeze: change.freeze,
            accept_multi_pool: change.accept_multi_pool,
            enabled: change.enabled,
            updated_by: by.to_string(),
            updated_at_ms: now_ms,
        })
    })
}

/// Removes the policy and its schedule. The catalog stays, so upkeep keeps
/// releasing its PITs, and the appliances keep what they have.
pub fn delete(
    store: &BackupStore,
    schedules: &Schedules,
    vm: &Uuid,
    revision: u64,
) -> Result<(), ApiError> {
    let current = store
        .policy(vm)?
        .ok_or_else(|| ApiError::new(ErrorCode::NotFound, format!("no backup policy for {vm}")))?;
    if current.revision != revision {
        return Err(ApiError::new(
            ErrorCode::Conflict,
            "the backup policy changed since it was read; reload and try again",
        ));
    }
    if let Some(schedule) = schedules.get(&current.schedule_id)? {
        schedules.delete(&schedule.id, schedule.revision)?;
    }
    store.locked(|l| l.delete_policy(vm))
}

/// The Dashboard's policy calls, with the registry the VMs are looked up in.
#[derive(Clone)]
pub struct PolicyAdmin {
    pub store: BackupStore,
    pub schedules: Schedules,
    pub pmxcfs: bmac_pmxcfs::Pmxcfs,
    pub registry: std::path::PathBuf,
    pub clock: std::sync::Arc<dyn Fn() -> i64 + Send + Sync>,
}

impl std::fmt::Debug for PolicyAdmin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("PolicyAdmin")
    }
}

static RUNNER: bmac_exec::RealRunner = bmac_exec::RealRunner;

impl PolicyAdmin {
    fn guests(&self) -> capture::RegistryGuests<'static> {
        capture::RegistryGuests {
            registry: bmac_registry::Registry::new(self.pmxcfs.clone(), &self.registry),
            runner: &RUNNER,
            owner: "bmacd-policies".into(),
        }
    }

    pub fn list(&self) -> Result<Vec<Policy>, ApiError> {
        self.store.policies()
    }

    pub fn save(&self, change: Value, by: &str) -> Result<Policy, ApiError> {
        let change: PolicyChange = serde_json::from_value(change).map_err(|e| {
            ApiError::new(
                ErrorCode::Invalid,
                format!("the request body doesn't fit: {e}"),
            )
        })?;
        save(
            &self.store,
            &self.schedules,
            &self.guests(),
            change,
            by,
            (self.clock)(),
        )
    }

    pub fn delete(&self, vm: &Uuid, revision: u64) -> Result<(), ApiError> {
        delete(&self.store, &self.schedules, vm, revision)
    }
}
