// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

//! `bmac luks-enter`: the LUKS passphrase, typed at the host's console, into
//! a root-only file in tmpfs for the storage phase of the host setup job,
//! which deletes it and checks it is gone (v2 `add_proxmox_host.sh`
//! `stage_luks_password_file` and `remove_luks_password_file`).

use std::io::Write;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::Path;

/// In `bmacd`'s runtime directory, which is tmpfs.
pub const PASSPHRASE_FILE: &str = "/run/bmac/luks-passphrase";

/// Refuses a session that arrived over SSH: the passphrase is only ever
/// typed at the console (iDRAC's virtual console, another remote console, or
/// a keyboard).
pub fn at_console(env: impl Fn(&str) -> Option<String>) -> Result<(), String> {
    if ["SSH_CONNECTION", "SSH_CLIENT", "SSH_TTY"]
        .iter()
        .any(|name| env(name).is_some_and(|v| !v.is_empty()))
    {
        return Err("type the LUKS passphrase at the host's console, not over SSH".to_string());
    }
    Ok(())
}

/// Writes `passphrase` to `path` with mode 0600, owned by the caller,
/// without following a symlink at `path`. The bytes are written exactly, with
/// no newline, as `cryptsetup --key-file` reads the whole file.
pub fn store(path: &Path, passphrase: &[u8]) -> Result<(), String> {
    if passphrase.is_empty() {
        return Err("the passphrase is empty".to_string());
    }
    if passphrase.iter().any(|b| matches!(b, b'\n' | b'\r' | 0)) {
        return Err("the passphrase must be one line".to_string());
    }
    let dir = path
        .parent()
        .ok_or_else(|| format!("{} has no directory", path.display()))?;
    match std::fs::symlink_metadata(dir) {
        Ok(meta) if meta.is_dir() => {}
        Ok(_) => return Err(format!("{} is not a directory", dir.display())),
        Err(e) => {
            return Err(format!(
                "{} is missing ({e}); is bmacd running?",
                dir.display()
            ));
        }
    }
    match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_symlink() => {
            return Err(format!(
                "{} is a symlink; refusing to write it",
                path.display()
            ));
        }
        Ok(meta) if !meta.is_file() => {
            return Err(format!("{} is not a regular file", path.display()));
        }
        _ => {}
    }
    let temporary = dir.join(format!(".luks-passphrase.{}.tmp", std::process::id()));
    let _ = std::fs::remove_file(&temporary);
    let written = (|| -> std::io::Result<()> {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(rustix::fs::OFlags::NOFOLLOW.bits() as i32)
            .open(&temporary)?;
        file.write_all(passphrase)?;
        file.sync_all()?;
        std::fs::rename(&temporary, path)
    })();
    if let Err(e) = written {
        let _ = std::fs::remove_file(&temporary);
        return Err(format!("cannot write {}: {e}", path.display()));
    }
    verify(path, passphrase.len() as u64)
}

/// The file is a regular file of the expected size, mode 0600, owned by the
/// caller.
pub fn verify(path: &Path, len: u64) -> Result<(), String> {
    let meta = std::fs::symlink_metadata(path)
        .map_err(|e| format!("cannot check {}: {e}", path.display()))?;
    let uid = rustix::process::geteuid().as_raw();
    if !meta.is_file()
        || meta.len() != len
        || meta.permissions().mode() & 0o7777 != 0o600
        || meta.uid() != uid
    {
        return Err(format!(
            "{} is not the private file just written",
            path.display()
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_a_private_file_with_exactly_the_passphrase() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("luks-passphrase");
        store(&path, b"correct horse battery staple").unwrap();
        assert_eq!(
            std::fs::read(&path).unwrap(),
            b"correct horse battery staple"
        );
        let meta = std::fs::metadata(&path).unwrap();
        assert_eq!(meta.permissions().mode() & 0o7777, 0o600);
        store(&path, b"again").unwrap();
        assert_eq!(
            std::fs::read(&path).unwrap(),
            b"again",
            "a retry replaces it"
        );
        let names: Vec<_> = std::fs::read_dir(dir.path()).unwrap().collect();
        assert_eq!(names.len(), 1, "no temporary file is left behind");
    }

    #[test]
    fn refuses_symlinks_and_odd_paths() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("elsewhere");
        std::fs::write(&target, "keep").unwrap();
        let path = dir.path().join("luks-passphrase");
        std::os::unix::fs::symlink(&target, &path).unwrap();
        let error = store(&path, b"secret").unwrap_err();
        assert!(error.contains("is a symlink"), "{error}");
        assert_eq!(std::fs::read(&target).unwrap(), b"keep");

        std::fs::remove_file(&path).unwrap();
        std::fs::create_dir(&path).unwrap();
        assert!(
            store(&path, b"secret")
                .unwrap_err()
                .contains("not a regular file")
        );

        let linked_dir = dir.path().join("linked");
        std::os::unix::fs::symlink(dir.path(), &linked_dir).unwrap();
        let error = store(&linked_dir.join("luks-passphrase"), b"secret").unwrap_err();
        assert!(error.contains("is not a directory"), "{error}");

        let error = store(&dir.path().join("missing/luks-passphrase"), b"secret").unwrap_err();
        assert!(error.contains("is bmacd running"), "{error}");
    }

    #[test]
    fn refuses_empty_and_multi_line_passphrases() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("luks-passphrase");
        assert!(store(&path, b"").is_err());
        assert!(store(&path, b"one\ntwo").is_err());
        assert!(!path.exists());
    }

    #[test]
    fn only_at_the_console() {
        assert!(at_console(|_| None).is_ok());
        assert!(at_console(|name| (name == "SSH_TTY").then(|| "/dev/pts/0".into())).is_err());
        assert!(
            at_console(|name| (name == "SSH_CONNECTION").then(|| "1.2.3.4 5 6.7.8.9 22".into()))
                .is_err()
        );
        assert!(at_console(|name| (name == "SSH_CLIENT").then(String::new)).is_ok());
    }
}
