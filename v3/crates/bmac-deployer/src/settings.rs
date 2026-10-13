// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

//! The Deployer's settings file (`DEPLOYMENT.md`, "State"): link-wait
//! numbers and the ISOs it built. It holds no
//! cluster identity, token, or secret, but it is still written `0600`.

use std::fs;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use crate::flows::IsoBook;
use bmac_proto::deployer::{
    BuiltIso, DeployerSettings, DeployerSettingsView, IsoView, LinkWait, SourceIso,
};

pub const MAX_LINK_ATTEMPTS: u32 = 100;
pub const MAX_LINK_INTERVAL: u32 = 60;

#[derive(Debug, thiserror::Error)]
pub enum SettingsError {
    #[error("{path}: {message}")]
    Io { path: String, message: String },
    #[error("{0}")]
    Invalid(String),
}

fn io(path: &Path, error: impl std::fmt::Display) -> SettingsError {
    SettingsError::Io {
        path: path.display().to_string(),
        message: error.to_string(),
    }
}

/// The settings at `path` and whether the file existed. An unreadable or
/// malformed file gives the defaults, so a broken file never stops the
/// Deployer from starting; the next save replaces it.
pub fn load(path: &Path) -> (DeployerSettings, bool) {
    match fs::read(path) {
        Ok(bytes) => (
            serde_json::from_slice::<DeployerSettings>(&bytes)
                .map(sanitized)
                .unwrap_or_default(),
            true,
        ),
        Err(_) => (DeployerSettings::default(), false),
    }
}

/// Keeps values in their allowed ranges.
pub fn sanitized(mut settings: DeployerSettings) -> DeployerSettings {
    settings.link_wait = LinkWait {
        attempts: settings.link_wait.attempts.clamp(1, MAX_LINK_ATTEMPTS),
        interval_seconds: settings
            .link_wait
            .interval_seconds
            .clamp(1, MAX_LINK_INTERVAL),
    };
    settings
}

/// Writes `settings` atomically: a `0600` temporary file in the same
/// directory, synced, then renamed over `path`.
pub fn save(path: &Path, settings: &DeployerSettings) -> Result<(), SettingsError> {
    let dir = path.parent().unwrap_or(Path::new("."));
    fs::create_dir_all(dir).map_err(|e| io(dir, e))?;
    let mut file = tempfile::NamedTempFile::new_in(dir).map_err(|e| io(dir, e))?;
    file.as_file()
        .set_permissions(fs::Permissions::from_mode(0o600))
        .map_err(|e| io(path, e))?;
    let text = serde_json::to_vec_pretty(settings).map_err(|e| io(path, e))?;
    file.write_all(&text).map_err(|e| io(path, e))?;
    file.as_file().sync_all().map_err(|e| io(path, e))?;
    file.persist(path).map_err(|e| io(path, e.error))?;
    Ok(())
}

/// The settings as the window shows them, with whether each built ISO is
/// still on disk.
pub fn view(path: &Path, settings: &DeployerSettings) -> DeployerSettingsView {
    DeployerSettingsView {
        settings: settings.clone(),
        isos: settings
            .isos
            .iter()
            .map(|iso| IsoView {
                iso: iso.clone(),
                present: Path::new(&iso.path).is_file(),
            })
            .collect(),
        path: path.display().to_string(),
        default_discovery_iso: {
            let pin = crate::discovery::alpine_iso();
            SourceIso {
                url: pin.url,
                sha256: pin.sha256,
            }
        },
    }
}

/// Applies the parts of `next` the window may change. The ISO list changes
/// only through the Deployer's own work.
pub fn merge(
    current: &DeployerSettings,
    next: DeployerSettings,
) -> Result<DeployerSettings, SettingsError> {
    if let Some(iso) = &next.discovery_iso {
        check_source(iso)?;
    }
    Ok(sanitized(DeployerSettings {
        link_wait: next.link_wait,
        notifications: next.notifications,
        discovery_iso: next.discovery_iso.map(|iso| SourceIso {
            url: iso.url.trim().to_string(),
            sha256: iso.sha256.trim().to_ascii_lowercase(),
        }),
        isos: current.isos.clone(),
    }))
}

/// A chosen source ISO: a plain https URL and a SHA-256 to check it by.
pub fn check_source(iso: &SourceIso) -> Result<(), SettingsError> {
    if !crate::plain_https(iso.url.trim()) {
        return Err(SettingsError::Invalid(
            "The discovery ISO's URL must be a plain https URL.".into(),
        ));
    }
    let sha = iso.sha256.trim();
    if sha.len() != 64 || !sha.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(SettingsError::Invalid(
            "The discovery ISO's SHA-256 must be 64 hex digits.".into(),
        ));
    }
    Ok(())
}

/// Records a built ISO, replacing an older entry for the same file.
pub fn record_iso(settings: &mut DeployerSettings, iso: BuiltIso) {
    settings.isos.retain(|known| known.path != iso.path);
    settings.isos.push(iso);
}

/// Forgets an ISO and shreds the file if it is one the Deployer built.
pub fn delete_iso(settings: &mut DeployerSettings, path: &str) -> Result<(), SettingsError> {
    if !settings.isos.iter().any(|iso| iso.path == path) {
        return Err(io(Path::new(path), "the Deployer did not build this file"));
    }
    crate::shred::file(Path::new(path)).map_err(|e| io(Path::new(path), e))?;
    settings.isos.retain(|iso| iso.path != path);
    Ok(())
}

/// The settings the window and the workflows share, saved on each change.
pub struct Shared {
    pub path: PathBuf,
    pub current: Mutex<DeployerSettings>,
}

impl Shared {
    pub fn get(&self) -> DeployerSettings {
        self.current
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
    }

    /// Changes the settings and saves them; nothing changes if saving fails.
    pub fn change(
        &self,
        f: impl FnOnce(&mut DeployerSettings) -> Result<(), SettingsError>,
    ) -> Result<DeployerSettingsView, SettingsError> {
        let mut current = self.current.lock().unwrap_or_else(|p| p.into_inner());
        let mut next = current.clone();
        f(&mut next)?;
        save(&self.path, &next)?;
        *current = next;
        Ok(view(&self.path, &current))
    }

    pub fn view(&self) -> DeployerSettingsView {
        view(&self.path, &self.get())
    }
}

impl IsoBook for Shared {
    fn record(&self, iso: BuiltIso) {
        let _ = self.change(|s| {
            record_iso(s, iso);
            Ok(())
        });
    }

    fn delete(&self, path: &str) -> Result<(), String> {
        self.change(|s| delete_iso(s, path))
            .map(|_| ())
            .map_err(|e| e.to_string())
    }
}

/// The settings file inside the Deployer's config directory.
pub fn path_in(config_dir: &Path) -> PathBuf {
    config_dir.join("settings.json")
}

#[cfg(test)]
mod tests {
    use super::*;
    use bmac_proto::deployer::IsoKind;

    #[test]
    fn saving_is_private_and_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let path = path_in(&dir.path().join("config"));
        assert_eq!(load(&path), (DeployerSettings::default(), false));

        let settings = DeployerSettings {
            link_wait: LinkWait {
                attempts: 20,
                interval_seconds: 3,
            },
            ..Default::default()
        };
        save(&path, &settings).unwrap();
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(load(&path), (settings, true));

        fs::write(&path, "{not json").unwrap();
        assert_eq!(load(&path), (DeployerSettings::default(), true));
    }

    #[test]
    fn the_window_changes_only_its_own_settings_and_ranges_hold() {
        let mut current = DeployerSettings::default();
        record_iso(
            &mut current,
            BuiltIso {
                path: "/isos/mox1.iso".into(),
                kind: IsoKind::Host,
                target: "mox1".into(),
                built_ms: 1,
            },
        );
        let next = DeployerSettings {
            link_wait: LinkWait {
                attempts: 0,
                interval_seconds: 900,
            },
            notifications: false,
            ..Default::default()
        };
        let merged = merge(&current, next).unwrap();
        assert_eq!(
            merged.link_wait,
            LinkWait {
                attempts: 1,
                interval_seconds: MAX_LINK_INTERVAL
            }
        );
        assert!(!merged.notifications);
        assert_eq!(merged.isos, current.isos);
    }

    #[test]
    fn a_chosen_discovery_iso_needs_https_and_a_sha256() {
        let current = DeployerSettings::default();
        let with = |url: &str, sha256: &str| DeployerSettings {
            discovery_iso: Some(SourceIso {
                url: url.into(),
                sha256: sha256.into(),
            }),
            ..Default::default()
        };
        let sha = "A".repeat(64);
        let merged = merge(&current, with(" https://mirror.example/alpine.iso ", &sha)).unwrap();
        let chosen = merged.discovery_iso.unwrap();
        assert_eq!(chosen.url, "https://mirror.example/alpine.iso");
        assert_eq!(chosen.sha256, "a".repeat(64));
        assert!(merge(&current, with("http://mirror.example/alpine.iso", &sha)).is_err());
        assert!(merge(&current, with("https://mirror.example/a.iso", "abc")).is_err());
        assert!(
            merge(
                &current,
                with("https://mirror.example/a.iso", &"g".repeat(64))
            )
            .is_err()
        );
        assert_eq!(
            merge(&current, DeployerSettings::default())
                .unwrap()
                .discovery_iso,
            None
        );
    }

    #[test]
    fn only_built_isos_are_deleted() {
        let dir = tempfile::tempdir().unwrap();
        let iso = dir.path().join("mox1.iso");
        fs::write(&iso, b"iso").unwrap();
        let other = dir.path().join("other.iso");
        fs::write(&other, b"iso").unwrap();
        let mut settings = DeployerSettings::default();
        record_iso(
            &mut settings,
            BuiltIso {
                path: iso.display().to_string(),
                kind: IsoKind::Host,
                target: "mox1".into(),
                built_ms: 1,
            },
        );
        let shown = view(Path::new("settings.json"), &settings);
        assert!(shown.isos[0].present);

        assert!(delete_iso(&mut settings, &other.display().to_string()).is_err());
        assert!(other.exists());
        delete_iso(&mut settings, &iso.display().to_string()).unwrap();
        assert!(!iso.exists());
        assert!(settings.isos.is_empty());
    }
}
