// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

//! Debian packages the setup job's phases need (`packaging/README.md`): the
//! `bmacd` package can't depend on what Proxmox VE lacks, so each phase
//! installs its own once the host is online. A fresh install points apt at
//! the subscription-only repositories, which refuse a host without a
//! subscription, so they are switched off first, as v2 does.

use std::path::{Path, PathBuf};
use std::time::Duration;

use bmac_exec::{Command, CommandRunner};

use super::Stop;

const APT_GET: &str = "/usr/bin/apt-get";
const DPKG_QUERY: &str = "/usr/bin/dpkg-query";
const SOURCES: &str = "etc/apt/sources.list.d";
/// The repositories that need a subscription.
const SUBSCRIPTION_ONLY: &[&str] = &["pve-enterprise.sources", "ceph.sources"];
const NO_SUBSCRIPTION: &str = "pve-no-subscription.sources";
const APT_TIMEOUT: Duration = Duration::from_secs(15 * 60);

/// Commands and files on the host; `root` is `/` except in tests.
pub struct Apt<'a> {
    pub runner: &'a dyn CommandRunner,
    pub root: PathBuf,
}

fn codename(root: &Path) -> Result<String, Stop> {
    let path = root.join("etc/os-release");
    let text = std::fs::read_to_string(&path)
        .map_err(|e| Stop::failed(format!("cannot read {}: {e}", path.display())))?;
    text.lines()
        .find_map(|l| l.strip_prefix("VERSION_CODENAME="))
        .map(|v| v.trim_matches('"').to_string())
        .filter(|v| !v.is_empty() && v.chars().all(|c| c.is_ascii_lowercase()))
        .ok_or_else(|| Stop::failed(format!("{} names no Debian release", path.display())))
}

fn no_subscription(codename: &str) -> String {
    format!(
        "Types: deb\n\
         URIs: http://download.proxmox.com/debian/pve\n\
         Suites: {codename}\n\
         Components: pve-no-subscription\n\
         Signed-By: /usr/share/keyrings/proxmox-archive-keyring.gpg\n"
    )
}

async fn run(runner: &dyn CommandRunner, command: Command) -> Result<String, Stop> {
    let shown = command.to_string();
    runner
        .run(command)
        .await
        .map(|o| o.stdout_text())
        .map_err(|e| Stop::Failed {
            message: format!("{shown} failed"),
            details: Some(e.to_string()),
        })
}

impl Apt<'_> {
    /// Switches the subscription-only repositories off and the
    /// no-subscription one on. Returns whether anything changed.
    pub fn sources(&self) -> Result<bool, Stop> {
        let dir = self.root.join(SOURCES);
        let io = |path: &Path, e: std::io::Error| {
            Stop::failed(format!("cannot update {}: {e}", path.display()))
        };
        let mut changed = false;
        for name in SUBSCRIPTION_ONLY {
            let path = dir.join(name);
            if path.exists() {
                let disabled = dir.join(format!("{name}.disabled"));
                std::fs::rename(&path, &disabled).map_err(|e| io(&path, e))?;
                changed = true;
            }
        }
        let path = dir.join(NO_SUBSCRIPTION);
        let wanted = no_subscription(&codename(&self.root)?);
        if std::fs::read_to_string(&path).ok().as_deref() != Some(wanted.as_str()) {
            std::fs::create_dir_all(&dir).map_err(|e| io(&dir, e))?;
            super::write_atomic(&path, wanted.as_bytes()).map_err(|e| io(&path, e))?;
            changed = true;
        }
        Ok(changed)
    }

    async fn missing(&self, packages: &[&str]) -> Result<Vec<String>, Stop> {
        let command = Command::new(DPKG_QUERY)
            .args(["-W", "-f=${Package} ${db:Status-Status}\\n"])
            .args(packages.iter().copied())
            .expect_codes(&[0, 1]);
        let text = run(self.runner, command).await?;
        let installed: Vec<&str> = text
            .lines()
            .filter_map(|l| l.split_once(' '))
            .filter(|(_, status)| *status == "installed")
            .map(|(name, _)| name)
            .collect();
        Ok(packages
            .iter()
            .filter(|p| !installed.contains(p))
            .map(|p| p.to_string())
            .collect())
    }

    /// Installs whichever of `packages` aren't installed; returns them.
    pub async fn ensure(&self, packages: &[&str]) -> Result<Vec<String>, Stop> {
        let missing = self.missing(packages).await?;
        if missing.is_empty() {
            return Ok(missing);
        }
        self.sources()?;
        let apt = |args: &[&str]| {
            Command::new(APT_GET)
                .args(["-o", "DPkg::Lock::Timeout=120", "-o", "Acquire::Retries=5"])
                .args(args.iter().copied())
                .env("DEBIAN_FRONTEND", "noninteractive")
                .timeout(APT_TIMEOUT)
        };
        run(self.runner, apt(&["update"])).await?;
        let install = apt(&["install", "-y"]).args(missing.iter().cloned());
        run(self.runner, install).await?;
        let still = self.missing(packages).await?;
        if !still.is_empty() {
            return Err(Stop::failed(format!(
                "apt-get didn't install {}",
                still.join(", ")
            )));
        }
        Ok(missing)
    }
}

#[cfg(test)]
mod tests {
    use bmac_exec::{FakeRunner, Reply};

    use super::*;

    fn host() -> tempfile::TempDir {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join(SOURCES)).unwrap();
        std::fs::write(
            root.path().join("etc/os-release"),
            "PRETTY_NAME=\"Debian GNU/Linux 13 (trixie)\"\nVERSION_CODENAME=trixie\n",
        )
        .unwrap();
        root
    }

    const QUERY: &str = "-f=${Package} ${db:Status-Status}\\n";

    #[tokio::test]
    async fn installed_packages_need_nothing() {
        let root = host();
        let runner = FakeRunner::new();
        runner.expect(
            DPKG_QUERY,
            ["-W", QUERY, "cryptsetup", "keyutils"],
            Reply::ok("cryptsetup installed\nkeyutils installed\n"),
        );
        let apt = Apt {
            runner: &runner,
            root: root.path().into(),
        };
        assert!(
            apt.ensure(&["cryptsetup", "keyutils"])
                .await
                .unwrap()
                .is_empty()
        );
        runner.assert_done();
        assert!(!root.path().join(SOURCES).join(NO_SUBSCRIPTION).exists());
    }

    #[tokio::test]
    async fn missing_packages_are_installed_from_the_no_subscription_repository() {
        let root = host();
        let sources = root.path().join(SOURCES);
        std::fs::write(sources.join("pve-enterprise.sources"), "x").unwrap();
        let runner = FakeRunner::new();
        runner.expect(
            DPKG_QUERY,
            ["-W", QUERY, "cryptsetup", "keyutils"],
            Reply::ok("cryptsetup installed\nkeyutils not-installed\n").code(1),
        );
        let options = ["-o", "DPkg::Lock::Timeout=120", "-o", "Acquire::Retries=5"];
        runner.expect(APT_GET, [&options[..], &["update"]].concat(), Reply::ok(""));
        runner.expect(
            APT_GET,
            [&options[..], &["install", "-y", "keyutils"]].concat(),
            Reply::ok(""),
        );
        runner.expect(
            DPKG_QUERY,
            ["-W", QUERY, "cryptsetup", "keyutils"],
            Reply::ok("cryptsetup installed\nkeyutils installed\n"),
        );
        let apt = Apt {
            runner: &runner,
            root: root.path().into(),
        };
        let installed = apt.ensure(&["cryptsetup", "keyutils"]).await.unwrap();
        assert_eq!(installed, ["keyutils"]);
        runner.assert_done();
        assert!(!sources.join("pve-enterprise.sources").exists());
        assert!(sources.join("pve-enterprise.sources.disabled").exists());
        let written = std::fs::read_to_string(sources.join(NO_SUBSCRIPTION)).unwrap();
        assert!(written.contains("Suites: trixie\n"), "{written}");
        let update = &runner.calls()[1];
        assert_eq!(update.env.get("DEBIAN_FRONTEND").unwrap(), "noninteractive");
        assert!(!apt.sources().unwrap(), "a second switch changes nothing");
    }
}
