// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

//! "Log out everywhere": a per-user revocation time in pmxcfs. Every host
//! ends that user's sessions created at or before it. One file per user, so
//! no lock is needed; a write only ever moves the time forward.

use std::path::{Path, PathBuf};

use bmac_pmxcfs::{Pmxcfs, PmxcfsError};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

pub const REVOCATIONS: &str = "/etc/pve/priv/bmac/sessions/revoked";

#[derive(Clone)]
pub struct Revocations {
    pmxcfs: Pmxcfs,
    directory: PathBuf,
}

impl std::fmt::Debug for Revocations {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Revocations")
            .field("directory", &self.directory)
            .finish()
    }
}

impl Revocations {
    pub fn system() -> Revocations {
        Revocations::new(Pmxcfs::system(), REVOCATIONS)
    }

    pub fn new(pmxcfs: Pmxcfs, directory: impl Into<PathBuf>) -> Revocations {
        Revocations {
            pmxcfs,
            directory: directory.into(),
        }
    }

    pub fn directory(&self) -> &Path {
        &self.directory
    }

    fn path(&self, user: &str) -> PathBuf {
        let digest = Sha256::digest(user.as_bytes());
        let name: String = digest.iter().take(16).map(|b| format!("{b:02x}")).collect();
        self.directory.join(format!("{name}.json"))
    }

    /// When `user`'s sessions were last revoked, in milliseconds.
    pub fn revoked_at(&self, user: &str) -> Result<Option<i64>, PmxcfsError> {
        let path = self.path(user);
        if !self.pmxcfs.exists(&path) {
            return Ok(None);
        }
        let record = self.pmxcfs.read_json(&path, "session revocation")?;
        if record.get("user").and_then(Value::as_str) != Some(user) {
            return Ok(None);
        }
        Ok(record.get("revoked_at_ms").and_then(Value::as_i64))
    }

    pub fn revoke(&self, user: &str, now_ms: i64) -> Result<(), PmxcfsError> {
        let at = self.revoked_at(user)?.map_or(now_ms, |old| old.max(now_ms));
        let record = json!({"schema_version": 1, "user": user, "revoked_at_ms": at});
        self.pmxcfs.write_json(&self.path(user), &record)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bmac_pmxcfs::RealFs;
    use std::sync::Arc;

    #[test]
    fn revocations_only_move_forward() {
        let dir = tempfile::tempdir().unwrap();
        let pmxcfs = Pmxcfs::new(dir.path().join("pve"), Arc::new(RealFs));
        let revocations =
            Revocations::new(pmxcfs, dir.path().join("pve/priv/bmac/sessions/revoked"));
        assert_eq!(revocations.revoked_at("alice@pve").unwrap(), None);
        revocations.revoke("alice@pve", 2000).unwrap();
        revocations.revoke("alice@pve", 1000).unwrap();
        assert_eq!(revocations.revoked_at("alice@pve").unwrap(), Some(2000));
        assert_eq!(revocations.revoked_at("bob@pve").unwrap(), None);
    }
}
