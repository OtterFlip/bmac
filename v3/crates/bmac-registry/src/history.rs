// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

//! Release history (`command_release` 3123–3133, `command_host_release`
//! 3538–3548). v2 writes these without a validator and lists them raw; the
//! archived record is kept as written because the policy it was checked
//! against may since have changed.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::check::{exact_keys, fail, timestamp, typed};
use crate::hosts::HostSlotRecord;
use crate::resource::Resource;
use crate::{Result, SCHEMA_VERSION};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "record_type", deny_unknown_fields)]
pub enum HistoryRecord {
    #[serde(rename = "release")]
    Release {
        schema_version: u64,
        released_at: String,
        forced: bool,
        resource: Box<Resource>,
    },
    #[serde(rename = "host-release")]
    HostRelease {
        schema_version: u64,
        released_at: String,
        reason: String,
        slot: HostSlotRecord,
    },
}

pub fn validate_history(value: &Value) -> Result<HistoryRecord> {
    if !value.is_object() {
        return fail("history record must be an object");
    }
    match value["record_type"].as_str() {
        Some("release") => exact_keys(
            value,
            &[
                "schema_version",
                "record_type",
                "released_at",
                "forced",
                "resource",
            ],
            "release history record",
        )?,
        Some("host-release") => exact_keys(
            value,
            &[
                "schema_version",
                "record_type",
                "released_at",
                "reason",
                "slot",
            ],
            "host release history record",
        )?,
        _ => return fail("history record has an unknown record_type"),
    }
    if value["schema_version"] != SCHEMA_VERSION {
        return fail("history record schema version is unsupported");
    }
    timestamp(&value["released_at"])?;
    typed(value, "history record")
}
