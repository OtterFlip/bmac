// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

//! Deleting files that held secrets (`DEPLOYMENT.md`, "ISO lifecycle"): the
//! contents are overwritten in place and flushed to the disk before the
//! file is unlinked. On a copy-on-write filesystem or an SSD the old blocks
//! can survive the overwrite, so this narrows exposure rather than proving
//! erasure; single use and expiry of the embedded secrets are the real
//! protection.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

const CHUNK: usize = 1 << 20;

/// Overwrites `path` with zeros, flushes it, and deletes it. A missing file
/// is already gone; a symbolic link is removed without touching its target.
pub fn file(path: &Path) -> io::Result<()> {
    let meta = match fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e),
    };
    if meta.file_type().is_symlink() {
        return fs::remove_file(path);
    }
    if !meta.is_file() {
        return Err(io::Error::other(format!(
            "{} isn't a regular file",
            path.display()
        )));
    }
    let mut out = OpenOptions::new().write(true).open(path)?;
    let opened = out.metadata()?;
    if (opened.dev(), opened.ino()) != (meta.dev(), meta.ino()) {
        return Err(io::Error::other(format!(
            "{} changed while it was being shredded",
            path.display()
        )));
    }
    overwrite(&mut out, opened.len())?;
    drop(out);
    fs::remove_file(path)
}

fn overwrite(out: &mut File, len: u64) -> io::Result<()> {
    let zeros = vec![0u8; CHUNK];
    let mut left = len;
    while left > 0 {
        let n = left.min(CHUNK as u64) as usize;
        out.write_all(&zeros[..n])?;
        left -= n as u64;
    }
    out.sync_all()
}

/// Shreds every file under `dir`, then removes the directory.
pub fn tree(dir: &Path) -> io::Result<()> {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e),
    };
    let mut first_error = None;
    for entry in entries {
        let result = entry.and_then(|entry| {
            let path = entry.path();
            if entry.file_type()?.is_dir() {
                tree(&path)
            } else {
                file(&path)
            }
        });
        if let Err(e) = result {
            first_error.get_or_insert(e);
        }
    }
    let removed = fs::remove_dir_all(dir);
    match first_error {
        Some(e) => Err(e),
        None => removed,
    }
}

/// A private scratch directory for a build, shredded when dropped, whether
/// the build finished or failed.
pub struct Scratch {
    dir: Option<tempfile::TempDir>,
}

impl Scratch {
    pub fn new_in(parent: &Path, prefix: &str) -> Result<Scratch, String> {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::Builder::new()
            .prefix(prefix)
            .tempdir_in(parent)
            .map_err(|e| format!("{}: {e}", parent.display()))?;
        fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o700))
            .map_err(|e| e.to_string())?;
        Ok(Scratch { dir: Some(dir) })
    }

    pub fn path(&self) -> &Path {
        self.dir.as_ref().map_or(Path::new(""), |d| d.path())
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        if let Some(dir) = self.dir.take() {
            let path: PathBuf = dir.keep();
            let _ = tree(&path);
        }
    }
}

/// Shreds a file when dropped unless [`Pending::keep`] was called: a build's
/// output that must not outlive a failed build.
pub struct Pending {
    path: Option<PathBuf>,
}

impl Pending {
    pub fn new(path: PathBuf) -> Pending {
        Pending { path: Some(path) }
    }

    pub fn keep(mut self) -> PathBuf {
        self.path.take().unwrap_or_default()
    }
}

impl Drop for Pending {
    fn drop(&mut self) {
        if let Some(path) = self.path.take() {
            let _ = file(&path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_shredded_file_is_zeroed_in_place_before_it_goes() {
        let dir = tempfile::tempdir().unwrap();
        let iso = dir.path().join("host.iso");
        let secret = b"tskey-auth-k123 PRIVATE KEY".repeat(100_000);
        fs::write(&iso, &secret).unwrap();
        // A second name for the same inode shows what the blocks now hold.
        let witness = dir.path().join("witness");
        fs::hard_link(&iso, &witness).unwrap();

        file(&iso).unwrap();

        assert!(!iso.exists());
        let left = fs::read(&witness).unwrap();
        assert_eq!(left.len(), secret.len());
        assert!(left.iter().all(|&b| b == 0));
        file(&iso).unwrap();
    }

    #[test]
    fn a_link_is_removed_and_its_target_left_alone() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("target");
        fs::write(&target, b"keep me").unwrap();
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        file(&link).unwrap();
        assert!(fs::symlink_metadata(&link).is_err());
        assert_eq!(fs::read(&target).unwrap(), b"keep me");
        assert!(file(dir.path()).is_err());
    }

    #[test]
    fn a_scratch_directory_is_shredded_when_dropped() {
        let dir = tempfile::tempdir().unwrap();
        let witness = dir.path().join("witness");
        let path;
        {
            let scratch = Scratch::new_in(dir.path(), ".build-").unwrap();
            path = scratch.path().to_path_buf();
            fs::create_dir(path.join("menus")).unwrap();
            fs::write(path.join("menus/overlay"), b"secret overlay").unwrap();
            fs::hard_link(path.join("menus/overlay"), &witness).unwrap();
        }
        assert!(!path.exists());
        assert_eq!(fs::read(&witness).unwrap(), vec![0u8; 14]);
    }

    #[test]
    fn a_pending_output_goes_unless_kept() {
        let dir = tempfile::tempdir().unwrap();
        let failed = dir.path().join("failed.iso");
        fs::write(&failed, b"half built").unwrap();
        drop(Pending::new(failed.clone()));
        assert!(!failed.exists());
        let built = dir.path().join("built.iso");
        fs::write(&built, b"built").unwrap();
        assert_eq!(Pending::new(built.clone()).keep(), built);
        assert!(built.exists());
    }
}
