// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

//! Deferred cleanup records (`validate_cleanup_record`,
//! `cluster_registry.py` 1926–1980; targets 430–461).

use bmac_core::{GuestName, HostSlot};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::check::{
    MOX_RE, REPLICATION_TARGET_RE, RESOURCE_RE, SNAPSHOT_TARGET_RE, VM_TARGET_RE, VOLUME_TARGET_RE,
    exact_keys, fail, int, is_uuid, safe_id, snapshot_name, str_of, timestamp, typed,
};
use crate::state::{CleanupAction, CleanupState};
use crate::{Result, SCHEMA_VERSION};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CleanupRecord {
    pub schema_version: u64,
    pub record_type: String,
    pub id: String,
    pub resource: GuestName,
    pub resource_id: String,
    pub node: HostSlot,
    pub action: CleanupAction,
    pub target: String,
    pub reason: String,
    pub state: CleanupState,
    pub attempts: u32,
    pub created_at: String,
    pub updated_at: String,
    pub revision: u64,
}

pub fn validate_cleanup_target(action: CleanupAction, target: &Value) -> Result<()> {
    safe_id(target, "cleanup target")?;
    let target = str_of(target);
    match action {
        CleanupAction::DestroyVm if !VM_TARGET_RE.is_match(target) => {
            fail("destroy-vm cleanup target must be vm:VMID")
        }
        CleanupAction::DestroyVolume if !VOLUME_TARGET_RE.is_match(target) => {
            fail("destroy-volume cleanup target must be STORAGE:vm-VMID-disk-N")
        }
        CleanupAction::DeleteSnapshot => match SNAPSHOT_TARGET_RE.captures(target) {
            Some(captures) => snapshot_name(&captures[2]),
            None => {
                if let Some((_, candidate)) = target.split_once('@')
                    && candidate.starts_with("__replicate_")
                {
                    snapshot_name(candidate)?;
                }
                fail("delete-snapshot cleanup target must be vm-VMID@SNAPSHOT")
            }
        },
        CleanupAction::RemoveReplication if !REPLICATION_TARGET_RE.is_match(target) => {
            fail("remove-replication cleanup target must be VMID-JOB")
        }
        CleanupAction::RemoveRoute if !RESOURCE_RE.is_match(target) => {
            fail("remove-route cleanup target must be a resource name")
        }
        _ => Ok(()),
    }
}

pub fn validate_cleanup(record: &Value) -> Result<CleanupRecord> {
    if !record.is_object() {
        return fail("cleanup record must be an object");
    }
    exact_keys(
        record,
        &[
            "schema_version",
            "record_type",
            "id",
            "resource",
            "resource_id",
            "node",
            "action",
            "target",
            "reason",
            "state",
            "attempts",
            "created_at",
            "updated_at",
            "revision",
        ],
        "cleanup record",
    )?;
    if record["schema_version"] != SCHEMA_VERSION || record["record_type"] != "deferred-cleanup" {
        return fail("unsupported cleanup record schema");
    }
    safe_id(&record["id"], "cleanup id")?;
    if !record["resource"]
        .as_str()
        .is_some_and(|r| RESOURCE_RE.is_match(r))
    {
        return fail("cleanup resource name is invalid");
    }
    if !is_uuid(&record["resource_id"]) {
        return fail("cleanup resource_id is not a UUID");
    }
    if !record["node"].as_str().is_some_and(|n| MOX_RE.is_match(n)) {
        return fail("cleanup node name is invalid");
    }
    let Some(action) = record["action"].as_str().and_then(CleanupAction::from_text) else {
        return fail("cleanup action is invalid");
    };
    validate_cleanup_target(action, &record["target"])?;
    if !record["reason"]
        .as_str()
        .is_some_and(|r| r.chars().count() <= 240 && !r.contains('\n'))
    {
        return fail("cleanup reason is invalid");
    }
    if record["state"]
        .as_str()
        .and_then(CleanupState::from_text)
        .is_none()
    {
        return fail("cleanup state is invalid");
    }
    if !int(&record["attempts"]).is_some_and(|n| n >= 0) {
        return fail("cleanup attempts is invalid");
    }
    if !int(&record["revision"]).is_some_and(|n| n >= 1) {
        return fail("cleanup revision is invalid");
    }
    timestamp(&record["created_at"])?;
    timestamp(&record["updated_at"])?;
    typed(record, "cleanup record")
}
