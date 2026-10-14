// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

//! What the appliance keeps to rebuild a lost cluster (`BACKUP.md`,
//! "Recovering BMAC after losing the whole cluster"): each revision of the
//! cluster recovery manifest, and each host's LUKS header backups, in the
//! metadata dataset under `recovery/<cluster>/`.
//!
//! Manifests: the newest [`KEEP_RECENT`], and the newest of each day for
//! [`KEEP_DAYS`] days. Headers are kept by content; the newest
//! [`KEEP_HEADER_VERSIONS`] of each file stay.

use std::collections::{BTreeMap, BTreeSet};
use std::io;
use std::path::{Path, PathBuf};

use base64::Engine;
use bmac_proto::appliance::{LuksHeader, LuksHeaders, RecoveryManifest, RestoreTestReport};
use sha2::{Digest, Sha256};

use crate::fsx;
use crate::journal::Metadata;

/// The cluster serves header backups of up to this size.
pub const MAX_HEADER: u64 = 20 * 1024 * 1024;
pub const KEEP_RECENT: usize = 48;
pub const KEEP_DAYS: i64 = 90;
pub const KEEP_HEADER_VERSIONS: usize = 10;
/// How often the manifest and headers are pulled when nothing changed.
pub const PULL_EVERY_MS: i64 = 3600 * 1000;
const DAY_MS: i64 = 24 * 3600 * 1000;

fn cluster_dir(metadata: &Metadata, cluster: &str) -> PathBuf {
    metadata.root.join("recovery").join(cluster)
}

fn manifests_dir(metadata: &Metadata, cluster: &str) -> PathBuf {
    cluster_dir(metadata, cluster).join("manifests")
}

fn headers_dir(metadata: &Metadata, cluster: &str) -> PathBuf {
    cluster_dir(metadata, cluster).join("luks-headers")
}

/// A name the appliance writes under: no separators, no leading dot.
fn safe(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with('.')
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
}

/// The manifests kept, newest first: `(generated_at_ms, path)`.
fn listed(dir: &Path) -> io::Result<Vec<(i64, PathBuf)>> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let Some(stem) = name.strip_suffix(".json") else {
            continue;
        };
        let Some(at) = stem.split('-').next().and_then(|t| t.parse::<i64>().ok()) else {
            continue;
        };
        out.push((at, entry.path()));
    }
    out.sort_by(|a, b| b.cmp(a));
    Ok(out)
}

/// The manifests kept, newest first: when each was made, and the start of
/// its revision.
pub fn manifests_kept(metadata: &Metadata, cluster: &str) -> Vec<(i64, String)> {
    listed(&manifests_dir(metadata, cluster))
        .unwrap_or_default()
        .into_iter()
        .map(|(at, path)| {
            let revision = path
                .file_stem()
                .and_then(|s| s.to_str())
                .and_then(|s| s.split_once('-'))
                .map(|(_, r)| r.to_string())
                .unwrap_or_default();
            (at, revision)
        })
        .collect()
}

pub fn newest(metadata: &Metadata, cluster: &str) -> io::Result<Option<RecoveryManifest>> {
    match listed(&manifests_dir(metadata, cluster))?.first() {
        Some((_, path)) => fsx::read_json(path),
        None => Ok(None),
    }
}

/// Checks and keeps a pulled manifest; whether it was a new revision.
pub fn keep_manifest(
    metadata: &Metadata,
    cluster: &str,
    manifest: &RecoveryManifest,
    now_ms: i64,
) -> Result<bool, String> {
    if manifest.cluster_uuid != cluster {
        return Err(format!(
            "the recovery manifest is for cluster {}",
            manifest.cluster_uuid
        ));
    }
    if RecoveryManifest::revision_of(&manifest.files) != manifest.revision
        || !safe(&manifest.revision)
    {
        return Err("the recovery manifest doesn't match its revision".into());
    }
    if newest(metadata, cluster)
        .map_err(|e| e.to_string())?
        .is_some_and(|m| m.revision == manifest.revision)
    {
        return Ok(false);
    }
    let dir = manifests_dir(metadata, cluster);
    let path = dir.join(format!(
        "{:013}-{}.json",
        manifest.generated_at_ms.max(0),
        &manifest.revision[..16.min(manifest.revision.len())]
    ));
    fsx::write_json(&path, manifest, 0o600).map_err(|e| e.to_string())?;
    prune_manifests(&dir, now_ms).map_err(|e| e.to_string())?;
    Ok(true)
}

fn prune_manifests(dir: &Path, now_ms: i64) -> io::Result<()> {
    let all = listed(dir)?;
    let mut keep: BTreeSet<PathBuf> = all
        .iter()
        .take(KEEP_RECENT)
        .map(|(_, p)| p.clone())
        .collect();
    let mut days = BTreeSet::new();
    for (at, path) in &all {
        if now_ms - at < KEEP_DAYS * DAY_MS && days.insert(at.div_euclid(DAY_MS)) {
            keep.insert(path.clone());
        }
    }
    for (_, path) in all {
        if !keep.contains(&path) {
            std::fs::remove_file(path)?;
        }
    }
    Ok(())
}

fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// Restore tests kept per cluster, newest first.
pub const KEEP_TESTS: usize = 50;

fn tests_path(metadata: &Metadata, cluster: &str) -> PathBuf {
    cluster_dir(metadata, cluster).join("restore-tests.json")
}

/// The restore tests the cluster reported, newest first.
pub fn tests_kept(metadata: &Metadata, cluster: &str) -> Vec<RestoreTestReport> {
    std::fs::read(tests_path(metadata, cluster))
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

/// Merges the reported tests into those kept; whether anything changed.
pub fn keep_tests(
    metadata: &Metadata,
    cluster: &str,
    reported: &[RestoreTestReport],
) -> io::Result<bool> {
    let mut kept = tests_kept(metadata, cluster);
    let before = kept.clone();
    for report in reported {
        kept.retain(|t| t.restore_uuid != report.restore_uuid);
        kept.push(report.clone());
    }
    kept.sort_by_key(|t| std::cmp::Reverse(t.started_at_ms));
    kept.truncate(KEEP_TESTS);
    if kept == before {
        return Ok(false);
    }
    let bytes = serde_json::to_vec_pretty(&kept).map_err(io::Error::other)?;
    fsx::write_atomic(&tests_path(metadata, cluster), &bytes, 0o600)?;
    Ok(true)
}

/// The headers in a host's listing that aren't kept yet, by content.
pub fn headers_wanted(metadata: &Metadata, cluster: &str, headers: &LuksHeaders) -> Vec<String> {
    let host_dir = headers_dir(metadata, cluster).join(headers.host.to_string());
    headers
        .headers
        .iter()
        .filter(|(name, digest)| {
            safe(name)
                && !std::fs::read_dir(host_dir.join(name))
                    .map(|entries| {
                        entries.flatten().any(|e| {
                            e.file_name()
                                .to_string_lossy()
                                .ends_with(&format!("-{}", digest.sha256))
                        })
                    })
                    .unwrap_or(false)
        })
        .map(|(name, _)| name.clone())
        .collect()
}

/// Keeps one header backup after checking its digest; whether it was new.
pub fn keep_header(
    metadata: &Metadata,
    cluster: &str,
    header: &LuksHeader,
    now_ms: i64,
) -> Result<bool, String> {
    use std::io::Read as _;
    let name = &header.name;
    if !safe(name) {
        return Err(format!("{name:?} isn't a header file name"));
    }
    let compressed = base64::engine::general_purpose::STANDARD
        .decode(&header.gzip)
        .map_err(|e| format!("{name}: {e}"))?;
    let mut bytes = Vec::new();
    flate2::read::GzDecoder::new(compressed.as_slice())
        .take(MAX_HEADER + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| format!("{name}: {e}"))?;
    if bytes.len() as u64 > MAX_HEADER {
        return Err(format!("{name}: larger than {MAX_HEADER} bytes"));
    }
    let digest = sha256_hex(&bytes);
    if digest != header.sha256 {
        return Err(format!("{name}: its content doesn't match its digest"));
    }
    let dir = headers_dir(metadata, cluster)
        .join(header.host.to_string())
        .join(name);
    let exists = std::fs::read_dir(&dir)
        .map(|entries| {
            entries.flatten().any(|e| {
                e.file_name()
                    .to_string_lossy()
                    .ends_with(&format!("-{digest}"))
            })
        })
        .unwrap_or(false);
    if exists {
        return Ok(false);
    }
    fsx::write_atomic(
        &dir.join(format!("{:013}-{digest}", now_ms.max(0))),
        &bytes,
        0o600,
    )
    .map_err(|e| format!("{name}: {e}"))?;
    let mut versions: Vec<PathBuf> = std::fs::read_dir(&dir)
        .map_err(|e| e.to_string())?
        .flatten()
        .filter(|e| !e.file_name().to_string_lossy().starts_with('.'))
        .map(|e| e.path())
        .collect();
    versions.sort();
    let excess = versions.len().saturating_sub(KEEP_HEADER_VERSIONS);
    for old in versions.into_iter().take(excess) {
        let _ = std::fs::remove_file(old);
    }
    Ok(true)
}

/// What the UI shows: each host's header files, with the newest copy's
/// time and how many versions are kept.
pub fn headers_kept(
    metadata: &Metadata,
    cluster: &str,
) -> BTreeMap<String, Vec<(String, i64, usize)>> {
    let mut out = BTreeMap::new();
    let Ok(hosts) = std::fs::read_dir(headers_dir(metadata, cluster)) else {
        return out;
    };
    for host in hosts.flatten() {
        let mut files = Vec::new();
        for file in std::fs::read_dir(host.path())
            .into_iter()
            .flatten()
            .flatten()
        {
            let versions: Vec<i64> = std::fs::read_dir(file.path())
                .into_iter()
                .flatten()
                .flatten()
                .filter_map(|v| {
                    v.file_name()
                        .to_string_lossy()
                        .split('-')
                        .next()
                        .and_then(|t| t.parse().ok())
                })
                .collect();
            if let Some(newest) = versions.iter().max() {
                files.push((
                    file.file_name().to_string_lossy().into_owned(),
                    *newest,
                    versions.len(),
                ));
            }
        }
        files.sort();
        out.insert(host.file_name().to_string_lossy().into_owned(), files);
    }
    out
}

/// Whether the manifest is due: it changed, or an hour went by.
pub fn due(
    last_pull_ms: Option<i64>,
    revision: Option<&str>,
    kept: Option<&str>,
    now_ms: i64,
) -> bool {
    last_pull_ms.is_none_or(|t| now_ms - t >= PULL_EVERY_MS)
        || revision.is_some_and(|r| Some(r) != kept)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manifest(at: i64, text: &str) -> RecoveryManifest {
        let files = BTreeMap::from([("config/cluster.json".to_string(), text.to_string())]);
        RecoveryManifest {
            schema_version: 1,
            cluster_uuid: "c1".into(),
            generated_at_ms: at,
            host: "mox1".parse().unwrap(),
            revision: RecoveryManifest::revision_of(&files),
            files,
        }
    }

    #[test]
    fn a_manifest_is_kept_once_per_revision_and_checked() {
        let dir = tempfile::tempdir().unwrap();
        let metadata = Metadata::new(dir.path());
        assert!(keep_manifest(&metadata, "c1", &manifest(1, "a"), 1).unwrap());
        assert!(!keep_manifest(&metadata, "c1", &manifest(2, "a"), 2).unwrap());
        assert!(keep_manifest(&metadata, "c1", &manifest(3, "b"), 3).unwrap());
        assert_eq!(newest(&metadata, "c1").unwrap().unwrap().generated_at_ms, 3);
        let mut forged = manifest(4, "c");
        forged.files.insert("config/x.json".into(), "{}".into());
        assert!(
            keep_manifest(&metadata, "c1", &forged, 4)
                .unwrap_err()
                .contains("revision")
        );
        assert!(
            keep_manifest(&metadata, "c2", &manifest(5, "d"), 5)
                .unwrap_err()
                .contains("cluster c1")
        );
        let mode = std::fs::metadata(&listed(&manifests_dir(&metadata, "c1")).unwrap()[0].1)
            .unwrap()
            .permissions();
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(mode.mode() & 0o777, 0o600);
    }

    #[test]
    fn old_manifests_thin_to_one_a_day_then_go() {
        let dir = tempfile::tempdir().unwrap();
        let metadata = Metadata::new(dir.path());
        let now = 200 * DAY_MS;
        // Every six hours for 100 days; names are all pruning reads.
        let dir = manifests_dir(&metadata, "c1");
        std::fs::create_dir_all(&dir).unwrap();
        for h in 0..(100 * 4) {
            let at = now - h * 6 * 3600 * 1000;
            std::fs::write(dir.join(format!("{at:013}-r{h}.json")), "{}").unwrap();
        }
        prune_manifests(&manifests_dir(&metadata, "c1"), now).unwrap();
        let kept = listed(&manifests_dir(&metadata, "c1")).unwrap();
        // 48 recent (12 days at four a day), and one per day for the other
        // 78 of the 90 days.
        let expected = KEEP_RECENT + KEEP_DAYS as usize - 12;
        assert!(kept.len().abs_diff(expected) <= 1, "{}", kept.len());
        assert!(
            kept.iter()
                .all(|(at, _)| now - at < KEEP_DAYS * DAY_MS + DAY_MS)
        );
    }

    #[test]
    fn headers_are_kept_by_content_and_bounded() {
        let dir = tempfile::tempdir().unwrap();
        let metadata = Metadata::new(dir.path());
        let header = |name: &str, text: &str| {
            use std::io::Write as _;
            let mut gzip = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
            gzip.write_all(text.as_bytes()).unwrap();
            LuksHeader {
                host: "mox1".parse().unwrap(),
                name: name.into(),
                sha256: sha256_hex(text.as_bytes()),
                gzip: base64::engine::general_purpose::STANDARD.encode(gzip.finish().unwrap()),
            }
        };
        let listing = |text: &str| LuksHeaders {
            host: "mox1".parse().unwrap(),
            headers: BTreeMap::from([
                (
                    "bmac-rpool-s1.img".to_string(),
                    bmac_proto::appliance::LuksHeaderDigest {
                        sha256: sha256_hex(text.as_bytes()),
                        size: text.len() as u64,
                    },
                ),
                (
                    "../escape".to_string(),
                    bmac_proto::appliance::LuksHeaderDigest {
                        sha256: String::new(),
                        size: 0,
                    },
                ),
            ]),
        };
        assert_eq!(
            headers_wanted(&metadata, "c1", &listing("v1")),
            vec!["bmac-rpool-s1.img"]
        );
        assert!(keep_header(&metadata, "c1", &header("bmac-rpool-s1.img", "v1"), 1).unwrap());
        assert!(headers_wanted(&metadata, "c1", &listing("v1")).is_empty());
        assert!(!keep_header(&metadata, "c1", &header("bmac-rpool-s1.img", "v1"), 2).unwrap());
        for i in 0..12 {
            let text = format!("v{}", i + 2);
            keep_header(&metadata, "c1", &header("bmac-rpool-s1.img", &text), 10 + i).unwrap();
        }
        let kept = headers_kept(&metadata, "c1");
        assert_eq!(
            kept["mox1"],
            vec![("bmac-rpool-s1.img".to_string(), 21, KEEP_HEADER_VERSIONS)]
        );
        assert!(keep_header(&metadata, "c1", &header("../escape", "x"), 3).is_err());
        assert!(!dir.path().join("recovery/c1/escape").exists());
        let mut forged = header("bmac-rpool-s1.img", "v99");
        forged.sha256 = sha256_hex(b"other");
        assert!(keep_header(&metadata, "c1", &forged, 30).is_err());
    }

    #[test]
    fn reported_restore_tests_are_merged_newest_first_and_bounded() {
        let dir = tempfile::tempdir().unwrap();
        let metadata = Metadata::new(dir.path());
        let report = |n: u128, booted: bool| RestoreTestReport {
            restore_uuid: uuid::Uuid::from_u128(n),
            vm_uuid: uuid::Uuid::from_u128(7),
            pit_uuid: uuid::Uuid::from_u128(100 + n),
            host: "mox2".into(),
            started_at_ms: n as i64,
            finished_at_ms: Some(n as i64 + 1),
            booted,
            agent: booted,
            agent_after_s: None,
            error: None,
        };
        assert!(tests_kept(&metadata, "c1").is_empty());
        assert!(keep_tests(&metadata, "c1", &[report(1, false), report(2, true)]).unwrap());
        assert!(!keep_tests(&metadata, "c1", &[report(2, true)]).unwrap());
        assert!(keep_tests(&metadata, "c1", &[report(1, true)]).unwrap());
        let kept = tests_kept(&metadata, "c1");
        assert_eq!(
            kept.iter().map(|t| t.started_at_ms).collect::<Vec<_>>(),
            [2, 1]
        );
        assert!(kept[1].booted, "a report replaces the one kept");
        let many: Vec<_> = (10..(10 + KEEP_TESTS as u128 + 5))
            .map(|n| report(n, true))
            .collect();
        keep_tests(&metadata, "c1", &many).unwrap();
        assert_eq!(tests_kept(&metadata, "c1").len(), KEEP_TESTS);
        assert!(tests_kept(&metadata, "c2").is_empty());
    }

    #[test]
    fn a_pull_is_due_hourly_or_when_the_revision_changed() {
        assert!(due(None, None, None, 0));
        assert!(!due(Some(0), Some("a"), Some("a"), 1000));
        assert!(due(Some(0), Some("b"), Some("a"), 1000));
        assert!(due(Some(0), None, Some("a"), PULL_EVERY_MS));
    }
}
