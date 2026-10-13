// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

//! HAProxy ingress generations, the desired pointer, rollout allowances, and
//! per-node apply status (`cluster_registry.py` 476–644).

use bmac_core::HostSlot;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::check::{
    exact_keys, fail, int, node, nodes, positive_revision, sha256, timestamp, typed,
};
use crate::policy::Policy;
use crate::state::IngressNodeState;
use crate::{MAX_ROUTES, Result, SCHEMA_VERSION};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IngressGeneration {
    pub schema_version: u64,
    pub record_type: String,
    pub generation: String,
    pub bundle_sha256: String,
    pub route_count: u32,
    pub created_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IngressDesired {
    pub schema_version: u64,
    pub record_type: String,
    pub generation: String,
    pub bundle_sha256: String,
    pub route_count: u32,
    pub nodes: Vec<HostSlot>,
    pub created_at: String,
    pub updated_at: String,
    pub revision: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IngressNodeStatus {
    pub schema_version: u64,
    pub record_type: String,
    pub node: HostSlot,
    pub desired_generation: String,
    pub applied_generation: Option<String>,
    pub state: IngressNodeState,
    pub created_at: String,
    pub updated_at: String,
    pub revision: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Allowance {
    pub node: HostSlot,
    pub generation: String,
    pub expires_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IngressTransition {
    pub schema_version: u64,
    pub record_type: String,
    pub generation: String,
    pub allowances: Vec<Allowance>,
    pub created_at: String,
    pub updated_at: String,
    pub revision: u64,
}

fn header(
    value: &Value,
    keys: &[&str],
    context: &str,
    record_type: &str,
    label: &str,
) -> Result<()> {
    if !value.is_object() {
        return fail(format!("{context} must be an object"));
    }
    exact_keys(value, keys, context)?;
    if value["schema_version"] != SCHEMA_VERSION {
        return fail(format!("{label} schema version is unsupported"));
    }
    if value["record_type"] != record_type {
        return fail(match label {
            "ingress generation" | "ingress transition" | "ingress node status" => {
                format!("{label} has the wrong record_type")
            }
            _ => format!("{label} record has the wrong record_type"),
        });
    }
    Ok(())
}

fn route_count(value: &Value, label: &str) -> Result<()> {
    if !int(value).is_some_and(|n| (0..=MAX_ROUTES as i128).contains(&n)) {
        return fail(format!("{label} must be between 0 and {MAX_ROUTES}"));
    }
    Ok(())
}

fn ordered_times(value: &Value, what: &str) -> Result<()> {
    let created = timestamp(&value["created_at"])?;
    let updated = timestamp(&value["updated_at"])?;
    if updated < created {
        return fail(format!("{what} updated_at predates created_at"));
    }
    Ok(())
}

pub fn validate_generation(value: &Value) -> Result<IngressGeneration> {
    let keys = [
        "schema_version",
        "record_type",
        "generation",
        "bundle_sha256",
        "route_count",
        "created_at",
    ];
    header(
        value,
        &keys,
        "ingress generation record",
        "haproxy-generation",
        "ingress generation",
    )?;
    sha256(&value["generation"], "ingress generation")?;
    sha256(&value["bundle_sha256"], "ingress bundle digest")?;
    route_count(&value["route_count"], "ingress route_count")?;
    timestamp(&value["created_at"])?;
    typed(value, "ingress generation record")
}

pub fn validate_desired(value: &Value, policy: &Policy) -> Result<IngressDesired> {
    let keys = [
        "schema_version",
        "record_type",
        "generation",
        "bundle_sha256",
        "route_count",
        "nodes",
        "created_at",
        "updated_at",
        "revision",
    ];
    header(
        value,
        &keys,
        "ingress desired record",
        "haproxy-desired",
        "ingress desired",
    )?;
    sha256(&value["generation"], "desired ingress generation")?;
    sha256(&value["bundle_sha256"], "desired ingress bundle digest")?;
    route_count(&value["route_count"], "desired ingress route_count")?;
    let expected: Vec<Value> = (1..=policy.limits.max_hosts)
        .map(|n| format!("mox{n}").into())
        .collect();
    if value["nodes"] != Value::Array(expected) {
        return fail("desired ingress nodes must contain every configured mox in order");
    }
    positive_revision(
        &value["revision"],
        "ingress desired revision must be a positive integer",
    )?;
    ordered_times(value, "ingress desired")?;
    typed(value, "ingress desired record")
}

pub fn validate_node_status(value: &Value, policy: &Policy) -> Result<IngressNodeStatus> {
    let keys = [
        "schema_version",
        "record_type",
        "node",
        "desired_generation",
        "applied_generation",
        "state",
        "created_at",
        "updated_at",
        "revision",
    ];
    header(
        value,
        &keys,
        "ingress node status",
        "haproxy-node-status",
        "ingress node status",
    )?;
    nodes(std::slice::from_ref(&value["node"]), policy, false, true)?;
    sha256(
        &value["desired_generation"],
        "node desired ingress generation",
    )?;
    if !value["applied_generation"].is_null() {
        sha256(
            &value["applied_generation"],
            "node applied ingress generation",
        )?;
    }
    let Some(state) = value["state"]
        .as_str()
        .and_then(IngressNodeState::from_text)
    else {
        return fail("ingress node status state is invalid");
    };
    if state == IngressNodeState::Applied
        && value["applied_generation"] != value["desired_generation"]
    {
        return fail("applied ingress node status must match its desired generation");
    }
    positive_revision(
        &value["revision"],
        "ingress node status revision must be positive",
    )?;
    ordered_times(value, "ingress node status")?;
    typed(value, "ingress node status")
}

pub fn validate_transition(value: &Value, policy: &Policy) -> Result<IngressTransition> {
    let keys = [
        "schema_version",
        "record_type",
        "generation",
        "allowances",
        "created_at",
        "updated_at",
        "revision",
    ];
    header(
        value,
        &keys,
        "ingress transition record",
        "haproxy-transition",
        "ingress transition",
    )?;
    sha256(&value["generation"], "ingress transition generation")?;
    let Some(allowances) = value["allowances"].as_array().filter(|a| !a.is_empty()) else {
        return fail("ingress transition allowances must be a non-empty list");
    };
    let mut seen = Vec::new();
    for allowance in allowances {
        if !allowance.is_object() {
            return fail("ingress transition allowance must be an object");
        }
        exact_keys(
            allowance,
            &["node", "generation", "expires_at"],
            "ingress transition allowance",
        )?;
        seen.push(node(&allowance["node"], policy)?);
        sha256(&allowance["generation"], "allowed previous generation")?;
        if allowance["generation"] == value["generation"] {
            return fail("an allowance must name a previous generation");
        }
        timestamp(&allowance["expires_at"])?;
    }
    if seen.windows(2).any(|pair| pair[0] >= pair[1]) {
        return fail("ingress transition allowances must be unique and ordered");
    }
    positive_revision(
        &value["revision"],
        "ingress transition revision must be positive",
    )?;
    ordered_times(value, "ingress transition")?;
    typed(value, "ingress transition record")
}
