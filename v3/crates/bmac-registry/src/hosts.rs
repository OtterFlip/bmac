// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

//! Host slots and the cluster control record (`cluster_registry.py`
//! 504–531).

use bmac_core::HostSlot;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::check::{exact_keys, fail, node, positive_revision, timestamp, typed};
use crate::policy::Policy;
use crate::state::HostSlotState;
use crate::{Result, SCHEMA_VERSION};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HostSlotRecord {
    pub schema_version: u64,
    pub record_type: String,
    pub node: HostSlot,
    pub state: HostSlotState,
    pub created_at: String,
    pub updated_at: String,
    pub revision: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClusterControl {
    pub schema_version: u64,
    pub record_type: String,
    pub control_node: HostSlot,
    pub created_at: String,
    pub updated_at: String,
    pub revision: u64,
}

pub fn validate_host_slot(value: &Value, policy: &Policy) -> Result<HostSlotRecord> {
    if !value.is_object() {
        return fail("host slot record must be an object");
    }
    let keys = [
        "schema_version",
        "record_type",
        "node",
        "state",
        "created_at",
        "updated_at",
        "revision",
    ];
    exact_keys(value, &keys, "host slot record")?;
    if value["schema_version"] != SCHEMA_VERSION {
        return fail("host slot schema version is unsupported");
    }
    if value["record_type"] != "host-slot" {
        return fail("host slot record has the wrong record_type");
    }
    node(&value["node"], policy)?;
    if value["state"]
        .as_str()
        .and_then(HostSlotState::from_text)
        .is_none()
    {
        return fail(format!(
            "unknown host slot state: {}",
            crate::check::repr(&value["state"])
        ));
    }
    positive_revision(&value["revision"], "host slot revision must be positive")?;
    timestamp(&value["created_at"])?;
    timestamp(&value["updated_at"])?;
    typed(value, "host slot record")
}

pub fn validate_cluster_control(value: &Value, policy: &Policy) -> Result<ClusterControl> {
    if !value.is_object() {
        return fail("cluster control record must be an object");
    }
    let keys = [
        "schema_version",
        "record_type",
        "control_node",
        "created_at",
        "updated_at",
        "revision",
    ];
    exact_keys(value, &keys, "cluster control record")?;
    if value["schema_version"] != SCHEMA_VERSION {
        return fail("cluster control schema version is unsupported");
    }
    if value["record_type"] != "cluster-control" {
        return fail("cluster control record has the wrong record_type");
    }
    node(&value["control_node"], policy)?;
    positive_revision(
        &value["revision"],
        "cluster control revision must be positive",
    )?;
    timestamp(&value["created_at"])?;
    timestamp(&value["updated_at"])?;
    typed(value, "cluster control record")
}
