// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

//! Cluster updates (`DEPLOYMENT.md`, "Upgrades"): the release bundles a
//! host keeps, what it has installed, and installing one. The
//! `cluster.update` and `cluster.rollback` jobs ([`job`]) drive the hosts
//! one at a time over port 8444; each host checks a bundle's signature and
//! hashes itself before installing from it.
//!
//! Under [`ROOT`]:
//!
//! - `bundles/<sha256>.bundle` and `<sha256>.json`: checked bundles and
//!   their summaries, kept while they are installed or were installed just
//!   before, so a rollback finds them.
//! - `uploads/`: bundles arriving from the Dashboard, a URL or a peer.
//! - `installs/<job>-<mode>/`: one install's package, log and exit status.
//! - `installed.json`: the bundle this host runs and the one before it.
//! - `seed/`: the bundle the host ISO installed from, until bmacd adopts it
//!   ([`UpdateStore::adopt_seed`]).

pub mod install;
pub mod job;

use std::path::{Path, PathBuf};

use bmac_proto::updates::{BundleSummary, Installed, InstalledBundle, PackageSummary};
use bmac_release::{Bundle, BundleError, MAX_BUNDLE, TrustedKey};
use serde::{Deserialize, Serialize};

pub const ROOT: &str = "/var/lib/bmac/updates";
/// The package every bundle must carry: the one each cluster host runs.
pub const HOST_PACKAGE: &str = "bmacd";
/// Bundles that are neither installed nor were just before go after this.
const KEEP_STAGED_MS: i64 = 30 * 24 * 3600 * 1000;
/// Uploads nobody finished go after this.
const KEEP_UPLOAD_MS: i64 = 24 * 3600 * 1000;
const MAX_UPLOADS: usize = 4;

pub fn sha256_ok(sha: &str) -> bool {
    sha.len() == 64 && sha.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

pub fn summary(bundle: &Bundle, staged_at_ms: i64) -> BundleSummary {
    BundleSummary {
        sha256: bundle.sha256.clone(),
        version: bundle.manifest.version.clone(),
        built_at_s: bundle.manifest.built_at_s,
        signed_by: bundle.signed_by.to_string(),
        size: bundle.size,
        record_schema: bundle.manifest.record_schema,
        reads_record_schema_from: bundle.manifest.reads_record_schema_from,
        packages: bundle
            .manifest
            .packages
            .iter()
            .map(|p| PackageSummary {
                name: p.name.clone(),
                version: p.version.clone(),
            })
            .collect(),
        staged_at_ms,
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct Upload {
    size: u64,
    started_at_ms: i64,
}

#[derive(Debug, Clone)]
pub struct UpdateStore {
    root: PathBuf,
    keys: Vec<TrustedKey>,
}

fn io(path: &Path) -> impl Fn(std::io::Error) -> String + '_ {
    move |e| format!("{}: {e}", path.display())
}

fn bundle_error(e: BundleError) -> String {
    e.to_string()
}

impl UpdateStore {
    pub fn new(root: impl Into<PathBuf>, keys: Vec<TrustedKey>) -> UpdateStore {
        UpdateStore {
            root: root.into(),
            keys,
        }
    }

    pub fn system() -> UpdateStore {
        UpdateStore::new(ROOT, bmac_release::trusted_keys())
    }

    pub fn keys(&self) -> &[TrustedKey] {
        &self.keys
    }

    fn dir(&self, name: &str) -> Result<PathBuf, String> {
        use std::os::unix::fs::DirBuilderExt;
        let dir = self.root.join(name);
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&dir)
            .map_err(io(&dir))?;
        Ok(dir)
    }

    pub fn bundles_dir(&self) -> Result<PathBuf, String> {
        self.dir("bundles")
    }

    pub fn uploads_dir(&self) -> Result<PathBuf, String> {
        self.dir("uploads")
    }

    pub fn installs_dir(&self) -> Result<PathBuf, String> {
        self.dir("installs")
    }

    /// Where a staged bundle is, whether or not it is there.
    pub fn bundle_path(&self, sha: &str) -> Result<PathBuf, String> {
        if !sha256_ok(sha) {
            return Err(format!("{sha:?} is not a SHA-256"));
        }
        Ok(self.root.join("bundles").join(format!("{sha}.bundle")))
    }

    /// Checks the bundle at `file`, which must be in this store's
    /// uploads, and keeps it. `expect` is the SHA-256 it must have.
    pub fn stage(
        &self,
        file: &Path,
        expect: Option<&str>,
        now_ms: i64,
    ) -> Result<BundleSummary, String> {
        let checked = Bundle::open(file, &self.keys);
        let bundle = match checked {
            Ok(bundle) => bundle,
            Err(e) => {
                let _ = std::fs::remove_file(file);
                return Err(bundle_error(e));
            }
        };
        if let Some(expect) = expect
            && bundle.sha256 != expect
        {
            let _ = std::fs::remove_file(file);
            return Err(format!(
                "the bundle's SHA-256 is {}, not {expect}",
                bundle.sha256
            ));
        }
        if bundle.manifest.package(HOST_PACKAGE).is_none() {
            let _ = std::fs::remove_file(file);
            return Err(format!("the bundle has no {HOST_PACKAGE} package"));
        }
        let dir = self.bundles_dir()?;
        let path = self.bundle_path(&bundle.sha256)?;
        let info = dir.join(format!("{}.json", bundle.sha256));
        let summary = match self.read_summary(&info) {
            Some(existing) if path.exists() => {
                let _ = std::fs::remove_file(file);
                existing
            }
            _ => {
                std::fs::rename(file, &path).map_err(io(&path))?;
                let summary = summary(&bundle, now_ms);
                let text = serde_json::to_vec_pretty(&summary).map_err(|e| e.to_string())?;
                crate::native::write_atomic(&info, &text).map_err(io(&info))?;
                summary
            }
        };
        Ok(summary)
    }

    fn read_summary(&self, path: &Path) -> Option<BundleSummary> {
        serde_json::from_slice(&std::fs::read(path).ok()?).ok()
    }

    /// Opens a staged bundle, checking it again.
    pub fn open(&self, sha: &str) -> Result<Bundle, String> {
        let path = self.bundle_path(sha)?;
        if !path.exists() {
            return Err(format!("bundle {sha} isn't on this host"));
        }
        Bundle::open(&path, &self.keys).map_err(bundle_error)
    }

    pub fn has(&self, sha: &str) -> bool {
        self.bundle_path(sha).is_ok_and(|p| p.exists())
    }

    /// The staged bundles, newest first.
    pub fn staged(&self) -> Vec<BundleSummary> {
        let Ok(entries) = std::fs::read_dir(self.root.join("bundles")) else {
            return Vec::new();
        };
        let mut all: Vec<BundleSummary> = entries
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|x| x == "json"))
            .filter_map(|p| self.read_summary(&p))
            .filter(|s| self.has(&s.sha256))
            .collect();
        all.sort_by(|a, b| {
            b.staged_at_ms
                .cmp(&a.staged_at_ms)
                .then(a.sha256.cmp(&b.sha256))
        });
        all
    }

    pub fn summary(&self, sha: &str) -> Option<BundleSummary> {
        if !sha256_ok(sha) || !self.has(sha) {
            return None;
        }
        self.read_summary(&self.root.join("bundles").join(format!("{sha}.json")))
    }

    /// Removes a staged bundle this host doesn't need for a rollback.
    pub fn delete(&self, sha: &str) -> Result<(), String> {
        let path = self.bundle_path(sha)?;
        let installed = self.installed();
        let kept = [installed.current, installed.previous];
        if kept.iter().flatten().any(|b| b.sha256 == sha) {
            return Err(format!(
                "bundle {sha} is installed here or kept for a rollback"
            ));
        }
        if !path.exists() {
            return Err(format!("bundle {sha} isn't on this host"));
        }
        std::fs::remove_file(&path).map_err(io(&path))?;
        let _ = std::fs::remove_file(path.with_extension("json"));
        Ok(())
    }

    fn installed_path(&self) -> PathBuf {
        self.root.join("installed.json")
    }

    pub fn installed(&self) -> Installed {
        std::fs::read(self.installed_path())
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default()
    }

    /// The bundle the host ISO installed this host from, left in `seed/`
    /// by first boot: kept as the installed bundle when nothing is recorded
    /// yet and it holds the running build, so the first update has a bundle
    /// to roll back to. The seed is removed either way.
    pub fn adopt_seed(&self, running: &str, now_ms: i64) -> Result<Option<BundleSummary>, String> {
        let seed = self.root.join("seed");
        let Ok(entries) = std::fs::read_dir(&seed) else {
            return Ok(None);
        };
        let mut files: Vec<PathBuf> = entries.flatten().map(|e| e.path()).collect();
        files.sort();
        let mut adopted = None;
        let mut refused = Vec::new();
        for file in files {
            let wanted = adopted.is_none()
                && self.installed().current.is_none()
                && file.extension().is_some_and(|x| x == "bundle");
            if !wanted {
                let _ = std::fs::remove_file(&file);
                continue;
            }
            let scratch = self.scratch(&uuid::Uuid::new_v4().simple().to_string())?;
            std::fs::rename(&file, &scratch).map_err(io(&file))?;
            match self.stage(&scratch, None, now_ms) {
                Ok(summary) if summary.version == running => {
                    self.record_installed(&Installed {
                        current: Some(InstalledBundle {
                            sha256: summary.sha256.clone(),
                            version: summary.version.clone(),
                        }),
                        previous: None,
                    })?;
                    adopted = Some(summary);
                }
                Ok(summary) => {
                    let _ = self.delete(&summary.sha256);
                    refused.push(format!(
                        "{} holds {}, not the running {running}",
                        file.display(),
                        summary.version
                    ));
                }
                Err(e) => refused.push(format!("{}: {e}", file.display())),
            }
        }
        let _ = std::fs::remove_dir(&seed);
        match adopted {
            None if !refused.is_empty() => Err(refused.join("; ")),
            _ => Ok(adopted),
        }
    }

    pub fn record_installed(&self, installed: &Installed) -> Result<(), String> {
        std::fs::create_dir_all(&self.root).map_err(io(&self.root))?;
        let path = self.installed_path();
        let text = serde_json::to_vec_pretty(installed).map_err(|e| e.to_string())?;
        crate::native::write_atomic(&path, &text).map_err(io(&path))
    }

    /// Drops what is no longer needed: bundles neither installed nor kept
    /// for a rollback once they are old, and abandoned uploads.
    pub fn prune(&self, now_ms: i64) {
        let installed = self.installed();
        let kept: Vec<String> = [installed.current, installed.previous]
            .into_iter()
            .flatten()
            .map(|b| b.sha256)
            .collect();
        for summary in self.staged() {
            if !kept.contains(&summary.sha256) && now_ms - summary.staged_at_ms > KEEP_STAGED_MS {
                let _ = self.delete(&summary.sha256);
            }
        }
        for (id, upload) in self.uploads() {
            if now_ms - upload.started_at_ms > KEEP_UPLOAD_MS {
                self.drop_upload(&id);
            }
        }
    }

    fn upload_paths(&self, id: &str) -> Result<(PathBuf, PathBuf), String> {
        if id.len() != 32 || !id.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err("no such upload".into());
        }
        let dir = self.uploads_dir()?;
        Ok((
            dir.join(format!("{id}.part")),
            dir.join(format!("{id}.upload")),
        ))
    }

    fn uploads(&self) -> Vec<(String, Upload)> {
        let Ok(entries) = std::fs::read_dir(self.root.join("uploads")) else {
            return Vec::new();
        };
        entries
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|x| x == "upload"))
            .filter_map(|p| {
                let id = p.file_stem()?.to_str()?.to_string();
                let upload = serde_json::from_slice(&std::fs::read(&p).ok()?).ok()?;
                Some((id, upload))
            })
            .collect()
    }

    fn drop_upload(&self, id: &str) {
        if let Ok((part, meta)) = self.upload_paths(id) {
            let _ = std::fs::remove_file(part);
            let _ = std::fs::remove_file(meta);
        }
    }

    /// A scratch file in the uploads directory, for a bundle fetched from
    /// a URL or a peer.
    pub fn scratch(&self, name: &str) -> Result<PathBuf, String> {
        Ok(self.uploads_dir()?.join(format!("{name}.part")))
    }

    pub fn start_upload(&self, size: u64, now_ms: i64) -> Result<String, String> {
        if size == 0 || size > MAX_BUNDLE {
            return Err(format!(
                "a bundle must be between 1 byte and {MAX_BUNDLE} bytes"
            ));
        }
        self.prune(now_ms);
        if self.uploads().len() >= MAX_UPLOADS {
            return Err("too many uploads are in progress; finish or wait for them".into());
        }
        let id = uuid::Uuid::new_v4().simple().to_string();
        let (part, meta) = self.upload_paths(&id)?;
        std::fs::File::create(&part).map_err(io(&part))?;
        let upload = Upload {
            size,
            started_at_ms: now_ms,
        };
        let text = serde_json::to_vec(&upload).map_err(|e| e.to_string())?;
        crate::native::write_atomic(&meta, &text).map_err(io(&meta))?;
        Ok(id)
    }

    /// Appends a chunk, which must start where the upload ends.
    pub fn append_upload(&self, id: &str, offset: u64, data: &[u8]) -> Result<u64, String> {
        use std::io::Write;
        let (part, meta) = self.upload_paths(id)?;
        let upload: Upload = std::fs::read(&meta)
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .ok_or("no such upload")?;
        let have = std::fs::metadata(&part).map_err(io(&part))?.len();
        if offset != have {
            return Err(format!("the upload has {have} bytes, not {offset}"));
        }
        if have + data.len() as u64 > upload.size {
            return Err("the chunk runs past the bundle's size".into());
        }
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&part)
            .map_err(io(&part))?;
        file.write_all(data).map_err(io(&part))?;
        Ok(have + data.len() as u64)
    }

    /// Checks and keeps a complete upload.
    pub fn finish_upload(&self, id: &str, now_ms: i64) -> Result<BundleSummary, String> {
        let (part, meta) = self.upload_paths(id)?;
        let upload: Upload = std::fs::read(&meta)
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .ok_or("no such upload")?;
        let have = std::fs::metadata(&part).map_err(io(&part))?.len();
        if have != upload.size {
            return Err(format!(
                "the upload has {have} of its {} bytes",
                upload.size
            ));
        }
        let staged = self.stage(&part, None, now_ms);
        self.drop_upload(id);
        staged
    }
}

#[cfg(test)]
mod tests;
