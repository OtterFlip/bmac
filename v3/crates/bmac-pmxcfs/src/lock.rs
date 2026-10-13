// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

//! The cross-node registry lock: one empty directory created with `mkdir`
//! (`AllocatorLock`, `cluster_registry.py` 839–978).
//!
//! On pmxcfs the directory lives in `/etc/pve/priv/lock/`, which pmxcfs
//! requires to stay empty, so the owner's details go in a sibling file in
//! the state directory. Age alone never breaks a lock. An explicit break on
//! pmxcfs sets the directory's times to zero, which pmxcfs treats as a
//! cluster-wide unlock request checked against its own observation age;
//! outside pmxcfs an old, empty directory is removed.

use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::store::Pmxcfs;
use crate::{PmxcfsError, Result, utc_now};

/// The lock directory's name prefix on pmxcfs. It must match the Python
/// registry's, so both take the same lock while scripts still use it.
pub const REGISTRY_LOCK_PREFIX: &str = "bmac-registry-";
const OWNER_FILE: &str = ".allocator-lock-owner.json";
const LOCAL_LOCK_DIR: &str = ".allocator.lock";
const POLL: Duration = Duration::from_millis(100);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LockOptions {
    /// How long to wait for the lock (`--lock-timeout`, default 15 s).
    pub timeout: Duration,
    /// Outside pmxcfs, a lock older than this may be broken
    /// (`--stale-lock-seconds`, default 900 s).
    pub stale_after: Duration,
    /// Only an explicit operator request breaks a lock
    /// (`--break-stale-lock`).
    pub break_stale: bool,
    pub prefix: String,
}

impl Default for LockOptions {
    fn default() -> LockOptions {
        LockOptions {
            timeout: Duration::from_secs(15),
            stale_after: Duration::from_secs(900),
            break_stale: false,
            prefix: REGISTRY_LOCK_PREFIX.to_string(),
        }
    }
}

/// A held registry lock. Release it with [`RegistryLock::release`]; dropping
/// it releases on a best-effort basis.
pub struct RegistryLock {
    pmxcfs: Pmxcfs,
    state_root: PathBuf,
    path: PathBuf,
    owner_path: PathBuf,
    nonce: String,
    held: bool,
}

impl RegistryLock {
    /// The lock directory for a state root.
    pub fn path_for(pmxcfs: &Pmxcfs, state_root: &Path, prefix: &str) -> Result<PathBuf> {
        if pmxcfs.is_pmxcfs(state_root) {
            let relative = state_root.strip_prefix(pmxcfs.root()).unwrap_or(state_root);
            let digest = Sha256::digest(relative.to_string_lossy().as_bytes());
            let suffix: String = digest.iter().take(8).map(|b| format!("{b:02x}")).collect();
            Ok(pmxcfs.lock_dir().join(format!("{prefix}{suffix}")))
        } else {
            Ok(state_root.join(LOCAL_LOCK_DIR))
        }
    }

    pub fn acquire(
        pmxcfs: &Pmxcfs,
        state_root: &Path,
        options: &LockOptions,
    ) -> Result<RegistryLock> {
        let path = Self::path_for(pmxcfs, state_root, &options.prefix)?;
        let mut lock = RegistryLock {
            pmxcfs: pmxcfs.clone(),
            state_root: state_root.to_path_buf(),
            owner_path: state_root.join(OWNER_FILE),
            nonce: uuid::Uuid::new_v4().simple().to_string(),
            path,
            held: false,
        };
        let on_pmxcfs = pmxcfs.is_pmxcfs(state_root);
        let parent = lock.path.parent().unwrap_or(Path::new("/")).to_path_buf();
        pmxcfs.ensure_directory(&parent, 0o700)?;
        let deadline = Instant::now() + options.timeout;
        loop {
            match pmxcfs.ops().mkdir(&lock.path, 0o700) {
                Ok(()) => break,
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
                    let breakable = on_pmxcfs || lock.is_stale(options.stale_after);
                    if options.break_stale && breakable && lock.break_stale(on_pmxcfs)? {
                        continue;
                    }
                    if Instant::now() >= deadline {
                        let hint = if !options.break_stale && breakable {
                            "; inspect it, then retry with --break-stale-lock to request a safe stale-lock check"
                        } else {
                            ""
                        };
                        return Err(PmxcfsError::LockTimeout {
                            owner: lock.owner_description(),
                            hint: hint.into(),
                        });
                    }
                    std::thread::sleep(POLL);
                }
                Err(e) => return Err(PmxcfsError::io("cannot create allocator lock", e)),
            }
        }
        if let Err(error) = pmxcfs.write_json(&lock.owner_path, &lock.owner_record()) {
            let _ = pmxcfs.ops().rmdir(&lock.path);
            return Err(error);
        }
        lock.held = true;
        Ok(lock)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn owner_path(&self) -> &Path {
        &self.owner_path
    }

    pub fn state_root(&self) -> &Path {
        &self.state_root
    }

    pub(crate) fn pmxcfs(&self) -> &Pmxcfs {
        &self.pmxcfs
    }

    fn owner_record(&self) -> Value {
        let node = rustix::system::uname()
            .nodename()
            .to_string_lossy()
            .into_owned();
        json!({
            "schema_version": 1,
            "record_type": "allocator-lock",
            "nonce": self.nonce,
            "node": node,
            "pid": std::process::id(),
            "created_at": utc_now(),
        })
    }

    fn is_stale(&self, stale_after: Duration) -> bool {
        let Ok(modified) = self.path.metadata().and_then(|m| m.modified()) else {
            return false;
        };
        SystemTime::now()
            .duration_since(modified)
            .unwrap_or_default()
            >= stale_after
    }

    /// Returns whether the old lock is gone.
    fn break_stale(&self, on_pmxcfs: bool) -> Result<bool> {
        if on_pmxcfs {
            return match self.pmxcfs.ops().utime_zero(&self.path) {
                Ok(()) => Ok(!self.pmxcfs.exists(&self.path)),
                Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(true),
                Err(e) => Err(PmxcfsError::io(
                    "cannot request stale allocator lock removal",
                    e,
                )),
            };
        }
        let entries = match std::fs::read_dir(&self.path) {
            Ok(entries) => entries.count(),
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(true),
            Err(e) => return Err(PmxcfsError::io("cannot inspect stale allocator lock", e)),
        };
        if entries > 0 {
            return Err(PmxcfsError::invalid(format!(
                "refusing to remove non-empty allocator lock directory: {}",
                self.path.display()
            )));
        }
        match self.pmxcfs.ops().rmdir(&self.path) {
            Ok(()) => Ok(true),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(true),
            Err(e) => Err(PmxcfsError::io("cannot remove stale allocator lock", e)),
        }
    }

    fn owner_description(&self) -> String {
        if !self.pmxcfs.exists(&self.owner_path) {
            return "unknown".into();
        }
        match self
            .pmxcfs
            .read_json(&self.owner_path, "allocator lock owner")
        {
            Ok(owner) => {
                let field = |key: &str| match &owner[key] {
                    Value::String(s) => s.clone(),
                    Value::Null => "?".into(),
                    other => other.to_string(),
                };
                format!(
                    "{} pid={} since={}",
                    field("node"),
                    field("pid"),
                    field("created_at")
                )
            }
            Err(_) => "malformed".into(),
        }
    }

    /// Releases the lock after checking it is still ours.
    pub fn release(mut self) -> Result<()> {
        self.release_inner()
    }

    fn release_inner(&mut self) -> Result<()> {
        if !self.held {
            return Ok(());
        }
        self.held = false;
        if !self.pmxcfs.exists(&self.owner_path) {
            return Err(PmxcfsError::invalid(
                "allocator lock disappeared while held",
            ));
        }
        let owner = self
            .pmxcfs
            .read_json(&self.owner_path, "allocator lock owner")?;
        if owner["nonce"].as_str() != Some(self.nonce.as_str()) {
            return Err(PmxcfsError::invalid(
                "allocator lock ownership changed while held",
            ));
        }
        let gone = |e: io::Error| {
            if e.kind() == io::ErrorKind::NotFound {
                PmxcfsError::invalid("allocator lock disappeared while held")
            } else {
                PmxcfsError::io("cannot release allocator lock", e)
            }
        };
        self.pmxcfs.ops().unlink(&self.owner_path).map_err(gone)?;
        self.pmxcfs.ops().rmdir(&self.path).map_err(gone)?;
        let parent = self.path.parent().unwrap_or(Path::new("/")).to_path_buf();
        self.pmxcfs.fsync_dir(&parent)
    }
}

impl Drop for RegistryLock {
    fn drop(&mut self) {
        let _ = self.release_inner();
    }
}
