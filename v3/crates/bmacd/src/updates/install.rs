// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

//! Installing a bundle's `bmacd` on this host, as the update job asks.
//! The package manager runs in a transient unit of its own, since the
//! package restarts `bmacd`; each step's state is kept in the install's
//! directory, so a restarted `bmacd` reports it and records the new
//! current bundle once the package manager has succeeded.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use bmac_core::JobId;
use bmac_exec::{Command, CommandRunner};
use bmac_proto::updates::{
    InstallMode, InstallRequest, InstallState, InstallStatus, Installed, InstalledBundle,
};
use serde::{Deserialize, Serialize};

use super::{HOST_PACKAGE, UpdateStore};

const PENDING: &str = "pending.json";
const STATE: &str = "state";
const MESSAGE: &str = "message";
const PACKAGE: &str = "bmacd.deb";
const LOG: &str = "log";
const EXIT: &str = "exit";
const SETTLED: &str = "settled";
const LOG_TAIL: usize = 4000;
const SYSTEMD_RUN: &str = "/usr/bin/systemd-run";
const SYSTEMCTL: &str = "/usr/bin/systemctl";

const INSTALL_SCRIPT: &str = include_str!("install_script.sh");

/// What one install will leave recorded, decided before it starts.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Pending {
    request: InstallRequest,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    after: Option<Installed>,
}

#[derive(Clone)]
pub struct Installer {
    store: UpdateStore,
    runner: Arc<dyn CommandRunner>,
}

impl std::fmt::Debug for Installer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Installer")
    }
}

/// What [`Installer::begin`] found.
#[derive(Debug, PartialEq)]
pub enum Begun {
    /// This job already asked; here is how it is going.
    Existing(InstallStatus),
    /// Started: fetch the bundle if it isn't here, then [`Installer::launch`].
    New,
}

fn mode_name(mode: InstallMode) -> &'static str {
    match mode {
        InstallMode::Update => "update",
        InstallMode::Rollback => "rollback",
    }
}

fn write(path: &Path, text: &str) -> Result<(), String> {
    crate::native::write_atomic(path, text.as_bytes())
        .map_err(|e| format!("{}: {e}", path.display()))
}

fn read(path: &Path) -> Option<String> {
    std::fs::read_to_string(path)
        .ok()
        .map(|s| s.trim().to_string())
}

fn tail(text: &str) -> String {
    let start = text.len().saturating_sub(LOG_TAIL);
    let start = (start..text.len())
        .find(|i| text.is_char_boundary(*i))
        .unwrap_or(text.len());
    text[start..].to_string()
}

impl Installer {
    pub fn new(store: UpdateStore, runner: Arc<dyn CommandRunner>) -> Installer {
        Installer { store, runner }
    }

    pub fn store(&self) -> &UpdateStore {
        &self.store
    }

    fn dir(&self, job: JobId, mode: InstallMode) -> Result<PathBuf, String> {
        Ok(self
            .store
            .installs_dir()?
            .join(format!("{job}-{}", mode_name(mode))))
    }

    fn pending(&self, dir: &Path) -> Option<Pending> {
        serde_json::from_slice(&std::fs::read(dir.join(PENDING)).ok()?).ok()
    }

    /// How the install `job` asked for is going, recording the new current
    /// bundle once the package manager has succeeded.
    pub fn status(
        &self,
        job: JobId,
        mode: InstallMode,
        now_ms: i64,
    ) -> Result<Option<InstallStatus>, String> {
        let dir = self.dir(job, mode)?;
        let Some(pending) = self.pending(&dir) else {
            return Ok(None);
        };
        let status = |state, message: Option<String>, log_tail| InstallStatus {
            mode,
            state,
            message,
            log_tail,
        };
        if let Some(code) = read(&dir.join(EXIT)) {
            if code == "0" {
                if !dir.join(SETTLED).exists() {
                    if let Some(after) = &pending.after {
                        self.store.record_installed(after)?;
                    }
                    write(&dir.join(SETTLED), "")?;
                    self.store.prune(now_ms);
                }
                return Ok(Some(status(InstallState::Installed, None, None)));
            }
            let log = std::fs::read_to_string(dir.join(LOG)).unwrap_or_default();
            return Ok(Some(status(
                InstallState::Failed,
                Some(format!("the package manager failed (exit status {code})")),
                Some(tail(&log)),
            )));
        }
        let state = match read(&dir.join(STATE)).as_deref() {
            Some("installing") => InstallState::Installing,
            Some("failed") => InstallState::Failed,
            _ => InstallState::Fetching,
        };
        Ok(Some(status(state, read(&dir.join(MESSAGE)), None)))
    }

    /// Starts the install `request` asks for, or reports the one it
    /// already started.
    pub fn begin(&self, request: &InstallRequest, now_ms: i64) -> Result<Begun, String> {
        if let Some(status) = self.status(request.job, request.mode, now_ms)? {
            return Ok(Begun::Existing(status));
        }
        if !super::sha256_ok(&request.sha256) {
            return Err(format!("{:?} is not a SHA-256", request.sha256));
        }
        if request.mode == InstallMode::Rollback {
            self.rollback_target(&request.sha256)?;
        }
        let dir = self.dir(request.job, request.mode)?;
        std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        write(&dir.join(STATE), "fetching")?;
        let pending = Pending {
            request: request.clone(),
            after: None,
        };
        write(
            &dir.join(PENDING),
            &serde_json::to_string(&pending).map_err(|e| e.to_string())?,
        )?;
        Ok(Begun::New)
    }

    /// The bundle a rollback from `from` installs: the one before it.
    fn rollback_target(&self, from: &str) -> Result<InstalledBundle, String> {
        let installed = self.store.installed();
        match (installed.current, installed.previous) {
            (Some(current), Some(previous)) if current.sha256 == from => Ok(previous),
            (Some(current), _) if current.sha256 != from => Err(format!(
                "this host runs bundle {}, not {from}",
                current.sha256
            )),
            (None, _) => Err("this host has installed no bundle to roll back from".into()),
            _ => Err("this host keeps no earlier bundle to roll back to".into()),
        }
    }

    /// The bundle this install needs on the host before it can launch.
    pub fn needs(&self, request: &InstallRequest) -> Result<String, String> {
        match request.mode {
            InstallMode::Update => Ok(request.sha256.clone()),
            InstallMode::Rollback => Ok(self.rollback_target(&request.sha256)?.sha256),
        }
    }

    /// Records that the install stopped before the package manager ran.
    pub fn fail(&self, job: JobId, mode: InstallMode, message: &str) {
        if let Ok(dir) = self.dir(job, mode) {
            let _ = write(&dir.join(MESSAGE), message);
            let _ = write(&dir.join(STATE), "failed");
        }
    }

    /// Checks the bundle again, unpacks its `bmacd`, and starts the package
    /// manager in a unit of its own.
    pub async fn launch(&self, request: &InstallRequest) -> Result<(), String> {
        let dir = self.dir(request.job, request.mode)?;
        let sha = self.needs(request)?;
        let bundle = self.store.open(&sha)?;
        let installed = self.store.installed();
        let to = InstalledBundle {
            sha256: bundle.sha256.clone(),
            version: bundle.manifest.version.clone(),
        };
        let after = match request.mode {
            InstallMode::Update => Installed {
                previous: match installed.current {
                    Some(current) if current.sha256 != to.sha256 => Some(current),
                    _ => installed.previous,
                },
                current: Some(to),
            },
            InstallMode::Rollback => Installed {
                previous: installed.current,
                current: Some(to),
            },
        };
        let package = dir.join(PACKAGE);
        bundle
            .extract(HOST_PACKAGE, &package)
            .map_err(|e| e.to_string())?;
        let pending = Pending {
            request: request.clone(),
            after: Some(after),
        };
        write(
            &dir.join(PENDING),
            &serde_json::to_string(&pending).map_err(|e| e.to_string())?,
        )?;
        write(&dir.join(STATE), "installing")?;
        let unit = format!("bmac-{}-{}", mode_name(request.mode), request.job);
        let command = Command::new(SYSTEMD_RUN)
            .args([
                format!("--unit={unit}"),
                "--collect".into(),
                "--quiet".into(),
                "--setenv=DEBIAN_FRONTEND=noninteractive".into(),
                "--setenv=PATH=/usr/sbin:/usr/bin:/sbin:/bin".into(),
                "/bin/sh".into(),
                "-c".into(),
                INSTALL_SCRIPT.into(),
                "sh".into(),
                package.display().to_string(),
                dir.display().to_string(),
            ])
            .timeout(Duration::from_secs(30));
        self.runner
            .run(command)
            .await
            .map_err(|e| format!("cannot start the package manager: {e}"))?;
        Ok(())
    }

    /// Which of `bmacd`'s companion units are active.
    pub async fn services(&self) -> std::collections::BTreeMap<String, bool> {
        let units = ["bmacd-web.service", "bmac-source.service"];
        let output = self
            .runner
            .run(
                Command::new(SYSTEMCTL)
                    .arg("is-active")
                    .args(units)
                    .expect_codes(&[0, 3, 4])
                    .timeout(Duration::from_secs(10)),
            )
            .await;
        let lines: Vec<String> = output
            .map(|o| o.stdout_text().lines().map(str::to_string).collect())
            .unwrap_or_default();
        units
            .iter()
            .enumerate()
            .map(|(i, unit)| {
                let name = unit.trim_end_matches(".service").to_string();
                (name, lines.get(i).is_some_and(|l| l.trim() == "active"))
            })
            .collect()
    }
}
