// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

//! The cluster recovery manifest (`BACKUP.md`, "Recovering BMAC after
//! losing the whole cluster"): the directories of `/etc/pve/priv/bmac/` in
//! [`RECOVERY_DIRS`], as text, which every appliance pulls and keeps.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use bmac_core::HostSlot;
use bmac_proto::appliance::{RECOVERY_DIRS, RECOVERY_SCHEMA, RecoveryManifest};

pub const ROOT: &str = "/etc/pve/priv/bmac";
/// pmxcfs keeps no file larger than 1 MiB; anything bigger isn't BMAC's.
const MAX_FILE: u64 = 1024 * 1024;
const MAX_TOTAL: u64 = 64 * 1024 * 1024;

/// The files of the manifest, by path under `root`. Hidden files (pmxcfs
/// write temporaries) and anything that isn't UTF-8 text are left out.
pub fn files(root: &Path) -> Result<BTreeMap<String, String>, String> {
    let mut out = BTreeMap::new();
    let mut total = 0u64;
    for dir in RECOVERY_DIRS {
        let mut pending: Vec<PathBuf> = vec![root.join(dir)];
        while let Some(path) = pending.pop() {
            let entries = match std::fs::read_dir(&path) {
                Ok(entries) => entries,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                Err(e) => return Err(format!("{}: {e}", path.display())),
            };
            for entry in entries {
                let entry = entry.map_err(|e| format!("{}: {e}", path.display()))?;
                let name = entry.file_name().to_string_lossy().into_owned();
                if name.starts_with('.') {
                    continue;
                }
                let kind = entry
                    .file_type()
                    .map_err(|e| format!("{}: {e}", entry.path().display()))?;
                if kind.is_dir() {
                    pending.push(entry.path());
                    continue;
                }
                if !kind.is_file() {
                    continue;
                }
                let size = entry.metadata().map(|m| m.len()).unwrap_or(u64::MAX);
                if size > MAX_FILE {
                    continue;
                }
                total += size;
                if total > MAX_TOTAL {
                    return Err(format!(
                        "BMAC's configuration under {} is over {} MiB",
                        root.display(),
                        MAX_TOTAL >> 20
                    ));
                }
                let Ok(bytes) = std::fs::read(entry.path()) else {
                    continue;
                };
                let Ok(text) = String::from_utf8(bytes) else {
                    continue;
                };
                let relative = entry
                    .path()
                    .strip_prefix(root)
                    .map(|p| p.to_string_lossy().into_owned())
                    .map_err(|e| e.to_string())?;
                out.insert(relative, text);
            }
        }
    }
    Ok(out)
}

pub fn manifest(
    root: &Path,
    cluster_uuid: &str,
    host: HostSlot,
    now_ms: i64,
) -> Result<RecoveryManifest, String> {
    let files = files(root)?;
    Ok(RecoveryManifest {
        schema_version: RECOVERY_SCHEMA,
        cluster_uuid: cluster_uuid.to_string(),
        generated_at_ms: now_ms,
        host,
        revision: RecoveryManifest::revision_of(&files),
        files,
    })
}

/// Whether a manifest path may be written back by a recovery: only
/// relative paths inside [`RECOVERY_DIRS`], never upward.
pub fn restorable(path: &str) -> bool {
    let p = Path::new(path);
    let mut parts = p.components();
    let first_ok = matches!(
        parts.next(),
        Some(std::path::Component::Normal(d)) if RECOVERY_DIRS.iter().any(|r| d == *r)
    );
    first_ok
        && parts.clone().next().is_some()
        && parts.all(|c| matches!(c, std::path::Component::Normal(_)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_manifest_carries_only_the_recovery_directories_as_text() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let write = |path: &str, bytes: &[u8]| {
            let full = root.join(path);
            std::fs::create_dir_all(full.parent().unwrap()).unwrap();
            std::fs::write(full, bytes).unwrap();
        };
        write("backup/policies/web.json", b"{\"vm\":\"web\"}");
        write("registry/resources/web.json", b"{}");
        write("config/cluster.json", b"{}");
        write("config/.tmp.cluster.json", b"half");
        write("schedules/s.json", b"{}");
        write("secrets/postmark.token", b"secret-value");
        write("sessions/revoked/x", b"x");
        write("enrollment/e.json", b"{\"token_sha256\":\"x\"}");
        write("qdevice_ed25519", b"-----BEGIN OPENSSH PRIVATE KEY-----");
        write("cluster.json", b"{}");
        write("backup/blob", &[0xff, 0xfe, 0x00]);
        let m = manifest(root, "c1", "mox1".parse().unwrap(), 5).unwrap();
        let paths: Vec<&str> = m.files.keys().map(String::as_str).collect();
        assert_eq!(
            paths,
            vec![
                "backup/policies/web.json",
                "config/cluster.json",
                "registry/resources/web.json",
                "schedules/s.json",
            ]
        );
        let text = serde_json::to_string(&m).unwrap();
        assert!(!text.contains("secret-value") && !text.contains("PRIVATE KEY"));
        assert_eq!(m.revision, RecoveryManifest::revision_of(&m.files));
        let again = manifest(root, "c1", "mox2".parse().unwrap(), 9).unwrap();
        assert_eq!(again.revision, m.revision, "the revision is the content's");
        write("schedules/s.json", b"{\"changed\":1}");
        assert_ne!(
            manifest(root, "c1", "mox1".parse().unwrap(), 9)
                .unwrap()
                .revision,
            m.revision
        );
    }

    #[test]
    fn only_paths_inside_the_recovery_directories_are_restorable() {
        assert!(restorable("backup/policies/web.json"));
        assert!(restorable("registry/resources/web.json"));
        assert!(!restorable("backup"));
        assert!(!restorable("secrets/postmark.token"));
        assert!(!restorable("backup/../secrets/postmark.token"));
        assert!(!restorable("/etc/pve/priv/bmac/backup/x"));
        assert!(!restorable("cluster.json"));
    }
}
