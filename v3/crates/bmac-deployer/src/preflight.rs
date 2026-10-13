// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

//! Workstation checks shown when the Deployer opens, from v2's
//! `dashboard/engine/src/preflight.rs`, cut down to what the Deployer uses.
//! None of them contacts a cluster or a target machine.

use std::path::{Path, PathBuf};
use std::time::Duration;

use bmac_exec::{Command, CommandRunner};
use bmac_proto::deployer::{Check, CheckLink, CheckStatus};

/// Searched after the workstation's own `PATH`, so a desktop launcher with
/// a minimal environment still finds system tools.
pub const SYSTEM_PATH: &str = "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";

/// What the checks need to know about the workstation.
#[derive(Debug, Clone)]
pub struct Workstation {
    pub os: String,
    pub arch: String,
    /// The `PATH` the Deployer was started with.
    pub path: String,
    pub home: Option<PathBuf>,
    /// `SSH_AUTH_SOCK` is set.
    pub agent: bool,
}

impl Workstation {
    pub fn detect() -> Workstation {
        Workstation {
            os: std::env::consts::OS.into(),
            arch: std::env::consts::ARCH.into(),
            path: std::env::var("PATH").unwrap_or_default(),
            home: std::env::var_os("HOME").map(PathBuf::from),
            agent: std::env::var_os("SSH_AUTH_SOCK").is_some(),
        }
    }

    /// The absolute path of `program`, from `PATH` and then the system
    /// directories. Commands are always run by absolute path.
    pub fn which(&self, program: &str) -> Option<PathBuf> {
        self.path
            .split(':')
            .chain(SYSTEM_PATH.split(':'))
            .filter(|dir| dir.starts_with('/'))
            .map(|dir| Path::new(dir).join(program))
            .find(|candidate| candidate.is_file())
    }
}

fn purpose(id: &str) -> Option<String> {
    Some(
        match id {
            "platform" => "The Deployer runs on amd64 Linux, because Proxmox's auto-install assistant is an amd64 Linux program.",
            "ssh" => "Reaches discovery sessions, new hosts, and guests, always with pinned host keys.",
            "ssh-keygen" => "Reads key fingerprints and removes old pins from known_hosts.",
            "ssh-keyscan" => "Reads a host's SSH key when this workstation didn't install it, for you to confirm.",
            "ssh_agent" => "Lets SSH use your passphrase-protected key without asking for it.",
            "ssh_key" => "One of your SSH public keys must be among the cluster's administrator keys.",
            "xorriso" => "Builds the discovery, host, and appliance ISOs.",
            "dpkg-deb" => "Unpacks Proxmox's auto-install assistant, which prepares the host ISO.",
            "tailscale" => "New hosts and discovery sessions are found on your tailnet.",
            _ => return None,
        }
        .into(),
    )
}

fn check(id: &str, label: &str, status: CheckStatus, detail: impl Into<String>) -> Check {
    Check {
        id: id.into(),
        label: label.into(),
        status,
        detail: detail.into(),
        fix: None,
        command: None,
        purpose: purpose(id),
        link: None,
    }
}

fn tool(
    ws: &Workstation,
    program: &str,
    missing: CheckStatus,
    needed_by: &str,
    install: &str,
) -> Check {
    match ws.which(program) {
        Some(path) => check(
            program,
            program,
            CheckStatus::Ok,
            path.display().to_string(),
        ),
        None => Check {
            command: Some(install.into()),
            ..check(
                program,
                program,
                missing,
                format!("Not found; {needed_by}."),
            )
        },
    }
}

/// Whether `~/.ssh` holds a public key, or the agent offers one.
async fn has_ssh_key(ws: &Workstation, runner: &dyn CommandRunner) -> bool {
    let on_disk = ws.home.as_ref().is_some_and(|home| {
        std::fs::read_dir(home.join(".ssh")).is_ok_and(|entries| {
            entries
                .flatten()
                .any(|e| e.file_name().to_string_lossy().ends_with(".pub"))
        })
    });
    if on_disk || !ws.agent {
        return on_disk;
    }
    let Some(ssh_add) = ws.which("ssh-add") else {
        return false;
    };
    let command = Command::new(ssh_add)
        .arg("-L")
        .inherit_env("SSH_AUTH_SOCK")
        .timeout(Duration::from_secs(5));
    runner
        .run(command)
        .await
        .is_ok_and(|out| out.stdout_text().lines().any(|l| l.starts_with("ssh-")))
}

pub async fn run(ws: &Workstation, runner: &dyn CommandRunner) -> Vec<Check> {
    let mut checks = Vec::new();
    checks.push(if ws.os == "linux" && ws.arch == "x86_64" {
        check(
            "platform",
            "Workstation",
            CheckStatus::Ok,
            format!("{} on {}", ws.os, ws.arch),
        )
    } else {
        Check {
            fix: Some("Run the Deployer on an amd64 Linux workstation.".into()),
            ..check(
                "platform",
                "Workstation",
                CheckStatus::Error,
                format!("{} on {} is not supported.", ws.os, ws.arch),
            )
        }
    });
    checks.push(tool(
        ws,
        "ssh",
        CheckStatus::Error,
        "every SSH step needs it",
        "sudo apt install openssh-client",
    ));
    checks.push(tool(
        ws,
        "ssh-keygen",
        CheckStatus::Error,
        "SSH setup needs it",
        "sudo apt install openssh-client",
    ));
    checks.push(tool(
        ws,
        "ssh-keyscan",
        CheckStatus::Warning,
        "adding hosts this workstation didn't install needs it",
        "sudo apt install openssh-client",
    ));
    checks.push(if ws.agent {
        check("ssh_agent", "SSH agent", CheckStatus::Ok, "SSH_AUTH_SOCK is set.")
    } else {
        Check {
            fix: Some("Start the Deployer from a session with ssh-agent running.".into()),
            ..check(
                "ssh_agent",
                "SSH agent",
                CheckStatus::Warning,
                "No SSH agent is visible. A passphrase-protected key fails as \"Permission denied (publickey)\".",
            )
        }
    });
    checks.push(if has_ssh_key(ws, runner).await {
        check("ssh_key", "SSH key", CheckStatus::Ok, "A public key is available.")
    } else {
        Check {
            fix: Some("Create one with ssh-keygen -t ed25519, and add it to the cluster's administrator keys.".into()),
            ..check("ssh_key", "SSH key", CheckStatus::Warning, "No SSH public key was found in ~/.ssh or the agent.")
        }
    });
    checks.push(tool(
        ws,
        "xorriso",
        CheckStatus::Error,
        "building any ISO needs it",
        "sudo apt install xorriso",
    ));
    checks.push(tool(
        ws,
        "dpkg-deb",
        CheckStatus::Warning,
        "building a host ISO needs it",
        "sudo apt install dpkg",
    ));
    checks.push(tailscale(ws, runner).await);
    checks
}

async fn tailscale(ws: &Workstation, runner: &dyn CommandRunner) -> Check {
    let Some(path) = ws.which("tailscale") else {
        return Check {
            link: Some(CheckLink {
                label: "Install Tailscale".into(),
                url: "https://tailscale.com/download/linux".into(),
            }),
            ..check(
                "tailscale",
                "Tailscale",
                CheckStatus::Warning,
                "The tailscale command was not found.",
            )
        };
    };
    let command = Command::new(path)
        .args(["status", "--json"])
        .timeout(Duration::from_secs(5));
    let state = match runner.run(command).await {
        Ok(out) => serde_json::from_slice::<serde_json::Value>(&out.stdout)
            .ok()
            .and_then(|v| {
                v.get("BackendState")
                    .and_then(|s| s.as_str())
                    .map(str::to_string)
            })
            .unwrap_or_else(|| "unknown".into()),
        Err(_) => {
            return check(
                "tailscale",
                "Tailscale",
                CheckStatus::Warning,
                "tailscale status did not answer.",
            );
        }
    };
    if state == "Running" {
        check(
            "tailscale",
            "Tailscale",
            CheckStatus::Ok,
            "Connected to the tailnet.",
        )
    } else {
        Check {
            fix: Some("Connect this workstation to the tailnet the cluster uses.".into()),
            command: Some("sudo tailscale up".into()),
            ..check(
                "tailscale",
                "Tailscale",
                CheckStatus::Warning,
                format!("Backend state: {state}"),
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bmac_exec::{FakeRunner, Reply};
    use std::os::unix::fs::PermissionsExt;

    fn workstation(bin: &Path, home: &Path) -> Workstation {
        Workstation {
            os: "linux".into(),
            arch: "x86_64".into(),
            path: bin.display().to_string(),
            home: Some(home.to_path_buf()),
            agent: false,
        }
    }

    fn install(bin: &Path, name: &str) -> PathBuf {
        let path = bin.join(name);
        std::fs::write(&path, "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    fn status<'a>(checks: &'a [Check], id: &str) -> &'a Check {
        checks
            .iter()
            .find(|c| c.id == id)
            .unwrap_or_else(|| panic!("no check {id}"))
    }

    #[tokio::test]
    async fn the_workstation_tools_are_found_and_tailscale_is_asked() {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("bin");
        std::fs::create_dir(&bin).unwrap();
        let tailscale = install(&bin, "tailscale");
        install(&bin, "xorriso");
        std::fs::create_dir(dir.path().join(".ssh")).unwrap();
        std::fs::write(
            dir.path().join(".ssh/id_ed25519.pub"),
            "ssh-ed25519 AAAA me",
        )
        .unwrap();

        let runner = FakeRunner::new();
        runner.expect(
            &tailscale,
            ["status", "--json"],
            Reply::ok(r#"{"BackendState":"NeedsLogin"}"#),
        );
        let checks = run(&workstation(&bin, dir.path()), &runner).await;
        runner.assert_done();

        assert_eq!(status(&checks, "platform").status, CheckStatus::Ok);
        assert_eq!(status(&checks, "xorriso").status, CheckStatus::Ok);
        assert_eq!(status(&checks, "ssh_key").status, CheckStatus::Ok);
        let tailscale = status(&checks, "tailscale");
        assert_eq!(tailscale.status, CheckStatus::Warning);
        assert_eq!(tailscale.detail, "Backend state: NeedsLogin");
        assert!(
            checks.iter().all(|c| c.purpose.is_some()),
            "every check says why"
        );
    }

    #[test]
    fn other_platforms_are_refused() {
        let ws = Workstation {
            os: "macos".into(),
            arch: "aarch64".into(),
            path: String::new(),
            home: None,
            agent: false,
        };
        let runner = FakeRunner::new();
        let checks = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap()
            .block_on(run(&ws, &runner));
        assert_eq!(status(&checks, "platform").status, CheckStatus::Error);
        assert_eq!(status(&checks, "ssh_key").status, CheckStatus::Warning);
    }
}
