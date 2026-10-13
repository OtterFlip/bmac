// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

//! Versioned records: a schema version that refuses newer formats, and a
//! revision that starts at 1 and must match on every update, as v2's
//! `--expected-revision` does (`cluster_registry.py` 2427–2454).

use std::path::Path;

use serde_json::Value;

use crate::lock::RegistryLock;
use crate::store::{Pmxcfs, WriteOutcome};
use crate::{PmxcfsError, Result};

/// The record schema this version of BMAC writes and understands.
pub const SCHEMA_VERSION: u64 = 1;

fn field_u64(value: &Value, key: &str) -> Option<u64> {
    value.get(key).and_then(Value::as_u64)
}

/// Reads a record, or `None` when it doesn't exist. A record written with a
/// newer schema than `supported` is refused, so an older engine never
/// misreads it during a rolling upgrade.
pub fn read_record(
    pmxcfs: &Pmxcfs,
    path: &Path,
    context: &str,
    supported: u64,
) -> Result<Option<Value>> {
    if !pmxcfs.exists(path) {
        return Ok(None);
    }
    let value = pmxcfs.read_json(path, context)?;
    let found = field_u64(&value, "schema_version").ok_or_else(|| {
        PmxcfsError::invalid(format!(
            "{context} has no valid schema_version: {}",
            path.display()
        ))
    })?;
    if found > supported {
        return Err(PmxcfsError::NewerSchema {
            context: context.into(),
            found,
            supported,
        });
    }
    Ok(Some(value))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordWrite {
    pub revision: u64,
    pub outcome: WriteOutcome,
}

/// Writes a record under the registry lock. `expected` is the revision the
/// caller read, or `None` to create a record that must not exist yet. The
/// stored revision becomes `expected + 1` (1 for a new record).
pub fn write_record(
    lock: &RegistryLock,
    path: &Path,
    mut value: Value,
    expected: Option<u64>,
) -> Result<RecordWrite> {
    if !path.starts_with(lock.state_root()) {
        return Err(PmxcfsError::invalid(format!(
            "{} is outside the locked state directory {}",
            path.display(),
            lock.state_root().display()
        )));
    }
    let Value::Object(map) = &mut value else {
        return Err(PmxcfsError::invalid(format!(
            "record must be a JSON object: {}",
            path.display()
        )));
    };
    let pmxcfs = lock.pmxcfs();
    let found = if pmxcfs.exists(path) {
        let current = pmxcfs.read_json(path, "record")?;
        field_u64(&current, "revision")
            .filter(|r| *r >= 1)
            .ok_or_else(|| {
                PmxcfsError::invalid(format!(
                    "record revision must be positive: {}",
                    path.display()
                ))
            })?
    } else {
        0
    };
    let wanted = expected.unwrap_or(0);
    if found != wanted {
        return Err(PmxcfsError::RevisionConflict {
            path: path.to_path_buf(),
            expected: wanted,
            found,
        });
    }
    let revision = wanted + 1;
    map.insert("revision".into(), revision.into());
    let outcome = pmxcfs.write_json(path, &value)?;
    Ok(RecordWrite { revision, outcome })
}
