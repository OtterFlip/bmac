// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

//! pmxcfs access: the registry lock, atomic record writes, size budgets,
//! schema versions, and revision checks.
//!
//! This is a port of v2's `cluster_registry.py` storage layer (`AllocatorLock`
//! 839–978, `atomic_write_json` 804–836, `reject_secret_material` 303–315).
//! The lock and the file format interoperate with the Python implementation,
//! which script operations keep using until the registry is ported.

mod fsops;
mod json;
mod lock;
mod record;
mod store;
mod time;

use std::path::PathBuf;

pub use fsops::{FsOps, RealFs};
pub use json::{canonical_json, reject_secret_material};
pub use lock::{LockOptions, REGISTRY_LOCK_PREFIX, RegistryLock};
pub use record::{RecordWrite, SCHEMA_VERSION, read_record, write_record};
pub use store::{Budget, Pmxcfs, WriteOutcome};
pub use time::utc_now;

/// The largest JSON record accepted, read or written (`MAX_JSON_BYTES`).
pub const MAX_JSON_BYTES: u64 = 1024 * 1024;
/// BMAC's total pmxcfs use (`ARCHITECTURE.md`, "Where state lives").
pub const BUDGET_HARD: u64 = 16 * 1024 * 1024;
pub const BUDGET_WARN: u64 = 8 * 1024 * 1024;

/// A safe, operator-facing error, worded as v2's `RegistryError`s are.
#[derive(Debug, thiserror::Error)]
pub enum PmxcfsError {
    #[error("{0}")]
    Invalid(String),
    #[error("{context}: {source}")]
    Io {
        context: String,
        source: std::io::Error,
    },
    #[error("timed out waiting for allocator lock (owner {owner}){hint}")]
    LockTimeout { owner: String, hint: String },
    /// Revision 0 means the record does not exist.
    #[error("revision conflict on {}: expected {expected}, found {found}", path.display())]
    RevisionConflict {
        path: PathBuf,
        expected: u64,
        found: u64,
    },
    #[error(
        "{context} uses schema version {found}; this version of BMAC understands up to {supported}"
    )]
    NewerSchema {
        context: String,
        found: u64,
        supported: u64,
    },
}

impl PmxcfsError {
    pub(crate) fn invalid(message: impl Into<String>) -> PmxcfsError {
        PmxcfsError::Invalid(message.into())
    }

    pub(crate) fn io(context: impl Into<String>, source: std::io::Error) -> PmxcfsError {
        PmxcfsError::Io {
            context: context.into(),
            source,
        }
    }
}

pub type Result<T> = std::result::Result<T, PmxcfsError>;
