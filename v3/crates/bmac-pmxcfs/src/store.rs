// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

//! Reading and atomically writing JSON records, on pmxcfs or, for tests and
//! host-local state, on an ordinary filesystem.

use std::io::{self, Write};
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde_json::Value;

use crate::fsops::{FsOps, RealFs};
use crate::json::{canonical_json, reject_secret_material};
use crate::{BUDGET_HARD, BUDGET_WARN, MAX_JSON_BYTES, PmxcfsError, Result};

/// A cap on the total size of everything under `root`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Budget {
    pub root: PathBuf,
    pub hard: u64,
    pub warn: u64,
}

impl Budget {
    /// BMAC's budget: 16 MiB under `/etc/pve/priv/bmac`, warning at 8 MiB.
    pub fn bmac(root: impl Into<PathBuf>) -> Budget {
        Budget {
            root: root.into(),
            hard: BUDGET_HARD,
            warn: BUDGET_WARN,
        }
    }
}

/// What a write did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WriteOutcome {
    pub bytes: u64,
    /// Set when the write took total use past the warning level.
    pub budget_warning: Option<String>,
}

/// Access to pmxcfs, rooted at `/etc/pve` in production and at a temporary
/// directory in tests. Paths under the root follow pmxcfs's rules: no
/// `fsync`, no `chmod`, no `O_EXCL`, and stale locks are broken by setting
/// their times to zero.
#[derive(Clone)]
pub struct Pmxcfs {
    root: PathBuf,
    ops: Arc<dyn FsOps>,
    budget: Option<Budget>,
}

impl Pmxcfs {
    pub fn system() -> Pmxcfs {
        Pmxcfs::new("/etc/pve", Arc::new(RealFs)).with_budget(Budget::bmac("/etc/pve/priv/bmac"))
    }

    pub fn new(root: impl Into<PathBuf>, ops: Arc<dyn FsOps>) -> Pmxcfs {
        Pmxcfs {
            root: root.into(),
            ops,
            budget: None,
        }
    }

    pub fn with_budget(mut self, budget: Budget) -> Pmxcfs {
        self.budget = Some(budget);
        self
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn lock_dir(&self) -> PathBuf {
        self.root.join("priv").join("lock")
    }

    /// Whether `path` is inside pmxcfs (`is_pmxcfs_path`).
    pub fn is_pmxcfs(&self, path: &Path) -> bool {
        path.starts_with(&self.root)
    }

    pub(crate) fn ops(&self) -> &dyn FsOps {
        self.ops.as_ref()
    }

    /// v2's `_fsync_directory`: skipped on pmxcfs, and ignoring filesystems
    /// that don't support it.
    pub(crate) fn fsync_dir(&self, path: &Path) -> Result<()> {
        if self.is_pmxcfs(path) {
            return Ok(());
        }
        match self.ops.fsync_dir(path) {
            Ok(()) => Ok(()),
            Err(e) if e.raw_os_error().is_some_and(is_ignorable) => Ok(()),
            Err(e) => Err(PmxcfsError::io(
                format!("cannot sync {}", path.display()),
                e,
            )),
        }
    }

    /// Creates `path` and its parents, refusing symlinks anywhere on the
    /// way, and sets the mode outside pmxcfs (`ensure_directory`).
    pub fn ensure_directory(&self, path: &Path, mode: u32) -> Result<()> {
        if !path.is_absolute() {
            return Err(PmxcfsError::invalid(format!(
                "state path must be absolute: {}",
                path.display()
            )));
        }
        ensure_no_symlink_components(path)?;
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(mode)
            .create(path)
            .map_err(|e| {
                PmxcfsError::io(
                    format!("cannot create state directory {}", path.display()),
                    e,
                )
            })?;
        let meta = path
            .symlink_metadata()
            .map_err(|e| PmxcfsError::io(path.display().to_string(), e))?;
        if !meta.is_dir() {
            return Err(PmxcfsError::invalid(format!(
                "state path is not a non-symlink directory: {}",
                path.display()
            )));
        }
        if !self.is_pmxcfs(path) {
            self.ops.chmod(path, mode).map_err(|e| {
                PmxcfsError::io(format!("cannot set mode {mode:o} on {}", path.display()), e)
            })?;
        }
        Ok(())
    }

    /// Reads a JSON record: a regular file, not a symlink, within the size
    /// limit, and free of secret material (`read_json_file`).
    pub fn read_json(&self, path: &Path, context: &str) -> Result<Value> {
        let meta = match path.symlink_metadata() {
            Ok(meta) => meta,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                return Err(PmxcfsError::invalid(format!(
                    "{context} does not exist: {}",
                    path.display()
                )));
            }
            Err(e) => {
                return Err(PmxcfsError::io(
                    format!("cannot read {context} {}", path.display()),
                    e,
                ));
            }
        };
        if !meta.is_file() {
            return Err(PmxcfsError::invalid(format!(
                "{context} is not a non-symlink regular file: {}",
                path.display()
            )));
        }
        if meta.len() > MAX_JSON_BYTES {
            return Err(PmxcfsError::invalid(format!(
                "{context} exceeds {MAX_JSON_BYTES} bytes: {}",
                path.display()
            )));
        }
        let data = std::fs::read(path)
            .map_err(|e| PmxcfsError::io(format!("cannot read {context} {}", path.display()), e))?;
        let value: Value = serde_json::from_slice(&data).map_err(|e| {
            PmxcfsError::invalid(format!("cannot read {context} {}: {e}", path.display()))
        })?;
        reject_secret_material(&value, context)?;
        Ok(value)
    }

    /// Whether `path` exists, without following a final symlink.
    pub fn exists(&self, path: &Path) -> bool {
        path.symlink_metadata().is_ok()
    }

    /// Total bytes of regular files under the budget root.
    pub fn usage(&self) -> Result<u64> {
        match &self.budget {
            Some(budget) => tree_size(&budget.root),
            None => Ok(0),
        }
    }

    fn check_budget(&self, path: &Path, new_len: u64) -> Result<Option<String>> {
        let Some(budget) = self.budget.as_ref().filter(|b| path.starts_with(&b.root)) else {
            return Ok(None);
        };
        let old_len = path.symlink_metadata().map(|m| m.len()).unwrap_or(0);
        let total = tree_size(&budget.root)?.saturating_sub(old_len) + new_len;
        let mib = |bytes: u64| bytes as f64 / (1024.0 * 1024.0);
        if total > budget.hard {
            return Err(PmxcfsError::invalid(format!(
                "refusing to write {}: BMAC's pmxcfs data would reach {:.1} MiB, over its {:.0} MiB budget",
                path.display(),
                mib(total),
                mib(budget.hard)
            )));
        }
        Ok((total > budget.warn).then(|| {
            format!(
                "BMAC's pmxcfs data is {:.1} MiB, over the {:.0} MiB warning level",
                mib(total),
                mib(budget.warn)
            )
        }))
    }

    /// Writes canonical JSON atomically through a sibling temporary file
    /// opened with `O_NOFOLLOW`, then `rename` (`atomic_write_json`).
    /// Writes a small secret, such as an API token, atomically with mode
    /// 0600. Only `/etc/pve/priv` holds secrets in pmxcfs.
    pub fn write_secret(&self, path: &Path, secret: &[u8]) -> Result<()> {
        if secret.len() > 4096 {
            return Err(PmxcfsError::invalid(format!(
                "refusing to write an oversized secret: {}",
                path.display()
            )));
        }
        let parent = path.parent().ok_or_else(|| {
            PmxcfsError::invalid(format!("no parent directory: {}", path.display()))
        })?;
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .ok_or_else(|| PmxcfsError::invalid(format!("not a file path: {}", path.display())))?;
        self.ensure_directory(parent, 0o700)?;
        let temporary = parent.join(format!(".{name}.{}.tmp", uuid::Uuid::new_v4().simple()));
        let pmxcfs = self.is_pmxcfs(path);
        let result = (|| -> io::Result<()> {
            let mut file = self.ops.create(&temporary, 0o600)?;
            file.write_all(secret)?;
            file.flush()?;
            if !pmxcfs {
                self.ops.fsync(&file, &temporary)?;
                self.ops.chmod(&temporary, 0o600)?;
            }
            drop(file);
            self.ops.rename(&temporary, path)
        })();
        if self.exists(&temporary) {
            let _ = self.ops.unlink(&temporary);
        }
        result.map_err(|e| PmxcfsError::io(format!("cannot write {}", path.display()), e))?;
        self.fsync_dir(parent)
    }

    pub fn write_json(&self, path: &Path, value: &Value) -> Result<WriteOutcome> {
        reject_secret_material(value, &path.display().to_string())?;
        let parent = path.parent().ok_or_else(|| {
            PmxcfsError::invalid(format!("no parent directory: {}", path.display()))
        })?;
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .ok_or_else(|| PmxcfsError::invalid(format!("not a file path: {}", path.display())))?;
        self.ensure_directory(parent, 0o700)?;
        let data = canonical_json(value);
        if data.len() as u64 > MAX_JSON_BYTES {
            return Err(PmxcfsError::invalid(format!(
                "refusing to write oversized JSON record: {}",
                path.display()
            )));
        }
        let budget_warning = self.check_budget(path, data.len() as u64)?;
        let temporary = parent.join(format!(".{name}.{}.tmp", uuid::Uuid::new_v4().simple()));
        let pmxcfs = self.is_pmxcfs(path);
        let result = (|| -> io::Result<()> {
            let mut file = self.ops.create(&temporary, 0o600)?;
            file.write_all(&data)?;
            file.flush()?;
            if !pmxcfs {
                self.ops.fsync(&file, &temporary)?;
                self.ops.chmod(&temporary, 0o600)?;
            }
            drop(file);
            self.ops.rename(&temporary, path)?;
            if !pmxcfs {
                self.ops.chmod(path, 0o600)?;
            }
            Ok(())
        })();
        if self.exists(&temporary) {
            let _ = self.ops.unlink(&temporary);
        }
        result.map_err(|e| {
            PmxcfsError::io(format!("cannot atomically write {}", path.display()), e)
        })?;
        self.fsync_dir(parent)?;
        Ok(WriteOutcome {
            bytes: data.len() as u64,
            budget_warning,
        })
    }

    /// Removes a record. Missing is not an error.
    pub fn remove(&self, path: &Path) -> Result<()> {
        match self.ops.unlink(path) {
            Err(e) if e.kind() != io::ErrorKind::NotFound => Err(PmxcfsError::io(
                format!("cannot remove {}", path.display()),
                e,
            )),
            _ => Ok(()),
        }
    }
}

/// `EINVAL`, `ENOTSUP`, and `EPERM`, which v2 treats as "this filesystem
/// doesn't support directory fsync".
fn is_ignorable(errno: i32) -> bool {
    [
        rustix::io::Errno::INVAL,
        rustix::io::Errno::NOTSUP,
        rustix::io::Errno::PERM,
    ]
    .iter()
    .any(|e| e.raw_os_error() == errno)
}

fn ensure_no_symlink_components(path: &Path) -> Result<()> {
    let mut current = PathBuf::new();
    for part in path.components() {
        current.push(part);
        if current
            .symlink_metadata()
            .is_ok_and(|m| m.file_type().is_symlink())
        {
            return Err(PmxcfsError::invalid(format!(
                "state path may not contain symlink: {}",
                current.display()
            )));
        }
    }
    Ok(())
}

fn tree_size(root: &Path) -> Result<u64> {
    let mut total = 0;
    let mut pending = vec![root.to_path_buf()];
    while let Some(dir) = pending.pop() {
        let entries = match std::fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
            Err(e) => {
                return Err(PmxcfsError::io(
                    format!("cannot measure {}", dir.display()),
                    e,
                ));
            }
        };
        for entry in entries {
            let entry = entry
                .map_err(|e| PmxcfsError::io(format!("cannot measure {}", dir.display()), e))?;
            let meta = entry
                .path()
                .symlink_metadata()
                .map_err(|e| PmxcfsError::io(entry.path().display().to_string(), e))?;
            if meta.is_dir() {
                pending.push(entry.path());
            } else if meta.is_file() {
                total += meta.len();
            }
        }
    }
    Ok(total)
}
