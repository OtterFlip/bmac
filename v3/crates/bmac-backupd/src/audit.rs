// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

//! The appliance's audit log (`BACKUP.md`, "Appliance platform"): every
//! pull, publish, prune, grant, trust and untrust, export, refused peer,
//! and policy change, as JSON lines in the metadata dataset, with IDs and
//! GUIDs, never data or secrets.

use std::fs::OpenOptions;
use std::io::{self, BufRead, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Kept whole up to this size; older entries are then rotated out.
const MAX_BYTES: u64 = 64 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Entry {
    pub at_ms: i64,
    /// `pull`, `publish`, `prune`, `grant`, `trust`, `untrust`, …
    pub event: String,
    /// The administrator, or `bmac-backupd` for the service itself.
    pub by: String,
    pub details: Value,
}

#[derive(Debug)]
pub struct Audit {
    path: PathBuf,
    lock: Mutex<()>,
}

impl Audit {
    pub fn new(metadata: &Path) -> Audit {
        Audit {
            path: metadata.join("audit.jsonl"),
            lock: Mutex::new(()),
        }
    }

    pub fn record(&self, at_ms: i64, event: &str, by: &str, details: Value) -> io::Result<()> {
        let _guard = self.lock.lock().unwrap_or_else(|p| p.into_inner());
        if std::fs::metadata(&self.path).is_ok_and(|m| m.len() > MAX_BYTES) {
            let old = self.path.with_extension("jsonl.1");
            std::fs::rename(&self.path, old)?;
        }
        let entry = Entry {
            at_ms,
            event: event.to_string(),
            by: by.to_string(),
            details,
        };
        let mut line = serde_json::to_vec(&entry).map_err(io::Error::other)?;
        line.push(b'\n');
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o600)
            .open(&self.path)?;
        file.write_all(&line)?;
        file.sync_data()
    }

    /// The newest `limit` entries, newest first.
    pub fn recent(&self, limit: usize) -> io::Result<Vec<Entry>> {
        let file = match std::fs::File::open(&self.path) {
            Ok(file) => file,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(e),
        };
        let mut entries: Vec<Entry> = io::BufReader::new(file)
            .lines()
            .map_while(Result::ok)
            .filter_map(|l| serde_json::from_str(&l).ok())
            .collect();
        entries.reverse();
        entries.truncate(limit);
        Ok(entries)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn entries_are_appended_and_read_newest_first() {
        let dir = tempfile::tempdir().unwrap();
        let audit = Audit::new(dir.path());
        audit
            .record(1, "pull", "bmac-backupd", serde_json::json!({"bytes": 5}))
            .unwrap();
        audit
            .record(2, "trust", "admin", serde_json::json!({"peer": "x"}))
            .unwrap();
        let recent = audit.recent(10).unwrap();
        assert_eq!(
            recent.iter().map(|e| e.event.as_str()).collect::<Vec<_>>(),
            ["trust", "pull"]
        );
        assert_eq!(audit.recent(1).unwrap().len(), 1);
    }
}
