// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

//! Production install orchestration records and their leases
//! (`validate_orchestration_record`, `cluster_registry.py` 647–714).

use std::collections::BTreeSet;

use bmac_core::GuestName;
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;

use crate::check::{
    NONCE_RE, RESOURCE_RE, SHA256_RE, exact_keys, fail, is_uuid, py_str, safe_id, timestamp, typed,
};
use crate::state::{InstallMode, InstallPhase};
use crate::{Result, SCHEMA_VERSION};

const KEYS: &[&str] = &[
    "schema_version",
    "record_type",
    "resource",
    "resource_id",
    "install_phase",
    "lease",
    "created_at",
    "updated_at",
    "revision",
];
const LEGACY_CONTRACT_KEYS: &[&str] = &["final_network_enabled", "startup_sha256"];
const CONTRACT_KEYS: &[&str] = &[
    "final_network_enabled",
    "startup_sha256",
    "source_iso_sha256",
    "install_mode",
];
const LEASE_KEYS: &[&str] = &["nonce", "owner", "acquired_at", "expires_at"];

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Lease {
    pub nonce: String,
    pub owner: String,
    pub acquired_at: String,
    pub expires_at: String,
}

/// The VM contract fields are all absent, the two legacy ones, or all four.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Orchestration {
    pub schema_version: u64,
    pub record_type: String,
    pub resource: GuestName,
    pub resource_id: String,
    pub install_phase: InstallPhase,
    pub lease: Option<Lease>,
    pub created_at: String,
    pub updated_at: String,
    pub revision: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub final_network_enabled: Option<bool>,
    /// Absent without a contract; `Some(None)` when the contract has no
    /// startup script.
    #[serde(
        default,
        deserialize_with = "present",
        skip_serializing_if = "Option::is_none"
    )]
    pub startup_sha256: Option<Option<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_iso_sha256: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub install_mode: Option<InstallMode>,
}

fn present<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> std::result::Result<Option<Option<String>>, D::Error> {
    Option::<String>::deserialize(deserializer).map(Some)
}

pub fn validate_orchestration(value: &Value) -> Result<Orchestration> {
    let Some(map) = value.as_object() else {
        return fail("orchestration record must be an object");
    };
    let unknown: Vec<&str> = map
        .keys()
        .map(String::as_str)
        .filter(|k| !KEYS.contains(k) && !CONTRACT_KEYS.contains(k))
        .collect();
    let missing: Vec<&str> = KEYS
        .iter()
        .copied()
        .filter(|k| !map.contains_key(*k))
        .collect();
    if !unknown.is_empty() {
        return fail(format!(
            "orchestration record has unknown fields: {}",
            unknown.join(", ")
        ));
    }
    if !missing.is_empty() {
        return fail(format!(
            "orchestration record is missing fields: {}",
            missing.join(", ")
        ));
    }
    let contract: BTreeSet<&str> = map
        .keys()
        .map(String::as_str)
        .filter(|k| CONTRACT_KEYS.contains(k))
        .collect();
    let legacy: BTreeSet<&str> = LEGACY_CONTRACT_KEYS.iter().copied().collect();
    let full: BTreeSet<&str> = CONTRACT_KEYS.iter().copied().collect();
    if !contract.is_empty() && contract != legacy && contract != full {
        return fail("orchestration record has a partial VM contract");
    }
    if value["schema_version"] != SCHEMA_VERSION {
        return fail("orchestration record schema version is unsupported");
    }
    if value["record_type"] != "orchestration" {
        return fail("orchestration record has the wrong record_type");
    }
    if !value["resource"]
        .as_str()
        .is_some_and(|r| RESOURCE_RE.is_match(r))
    {
        return fail("orchestration record has an invalid resource name");
    }
    if !is_uuid(&value["resource_id"]) {
        return fail("orchestration resource_id must be a UUID");
    }
    if value["install_phase"]
        .as_str()
        .and_then(InstallPhase::from_text)
        .is_none()
    {
        return fail("orchestration install phase is invalid");
    }
    if !contract.is_empty() {
        if !value["final_network_enabled"].is_boolean() {
            return fail("final_network_enabled must be boolean");
        }
        let startup = &value["startup_sha256"];
        if !startup.is_null() && !SHA256_RE.is_match(&py_str(startup)) {
            return fail("startup_sha256 must be lowercase SHA-256 or null");
        }
        if contract == full {
            if !SHA256_RE.is_match(&py_str(&value["source_iso_sha256"])) {
                return fail("source_iso_sha256 must be lowercase SHA-256");
            }
            if value["install_mode"]
                .as_str()
                .and_then(InstallMode::from_text)
                .is_none()
            {
                return fail("orchestration install_mode is invalid");
            }
        }
    }
    if !crate::check::int(&value["revision"]).is_some_and(|r| r >= 1) {
        return fail("orchestration revision must be a positive integer");
    }
    let created = timestamp(&value["created_at"])?;
    let updated = timestamp(&value["updated_at"])?;
    if updated < created {
        return fail("orchestration updated_at predates created_at");
    }
    let lease = &value["lease"];
    if !lease.is_null() {
        if !lease.is_object() {
            return fail("orchestration lease must be an object or null");
        }
        exact_keys(lease, LEASE_KEYS, "orchestration lease")?;
        if !NONCE_RE.is_match(&py_str(&lease["nonce"])) {
            return fail("orchestration lease nonce is invalid");
        }
        safe_id(
            &Value::from(py_str(&lease["owner"])),
            "orchestration lease owner",
        )?;
        let acquired = timestamp(&lease["acquired_at"])?;
        let expires = timestamp(&lease["expires_at"])?;
        if expires <= acquired {
            return fail("orchestration lease expiry must follow acquisition");
        }
    }
    typed(value, "orchestration record")
}
