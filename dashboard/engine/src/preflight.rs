//! Lightweight launch checks. None of them contacts the cluster.

use std::path::{Path, PathBuf};
use std::process::Command;

use serde::Serialize;

use crate::platform::{command_output, PlatformInfo};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckStatus {
    Ok,
    Warning,
    Error,
}

#[derive(Debug, Clone, Serialize)]
pub struct Check {
    pub id: &'static str,
    pub label: &'static str,
    pub status: CheckStatus,
    pub detail: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fix: Option<String>,
}

/// Directories searched ahead of PATH. A desktop app launched from a macOS
/// Finder or Dock gets a minimal PATH without Homebrew, so its Bash 4+,
/// OpenSSL 3, and Python would otherwise be invisible.
pub const EXTRA_PATH: &[&str] = &["/opt/homebrew/bin", "/opt/homebrew/sbin", "/usr/local/bin", "/usr/local/sbin"];

/// The PATH given to every script.
pub fn script_path() -> String {
    let mut parts: Vec<String> = EXTRA_PATH.iter().filter(|p| Path::new(p).is_dir()).map(|p| p.to_string()).collect();
    let current = std::env::var("PATH").unwrap_or_else(|_| "/usr/bin:/bin:/usr/sbin:/sbin".into());
    for part in current.split(':') {
        if !part.is_empty() && !parts.iter().any(|p| p == part) {
            parts.push(part.to_string());
        }
    }
    parts.join(":")
}

fn which(program: &str) -> Option<PathBuf> {
    script_path()
        .split(':')
        .map(|dir| Path::new(dir).join(program))
        .find(|candidate| candidate.is_file())
}

fn bash_version(path: &Path) -> Option<(u32, u32, String)> {
    let output = Command::new(path)
        .args(["-c", "printf '%s %s %s' \"${BASH_VERSINFO[0]}\" \"${BASH_VERSINFO[1]}\" \"$BASH_VERSION\""])
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&output.stdout).to_string();
    let mut parts = text.splitn(3, ' ');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    Some((major, minor, parts.next().unwrap_or("").to_string()))
}

/// The first Bash 4.4 or newer on the script PATH.
pub fn find_bash() -> Option<PathBuf> {
    script_path()
        .split(':')
        .map(|dir| Path::new(dir).join("bash"))
        .filter(|candidate| candidate.is_file())
        .find(|candidate| matches!(bash_version(candidate), Some((major, minor, _)) if major > 4 || (major == 4 && minor >= 4)))
}

fn check(id: &'static str, label: &'static str, status: CheckStatus, detail: impl Into<String>) -> Check {
    Check { id, label, status, detail: detail.into(), fix: None }
}

fn with_fix(mut check: Check, fix: impl Into<String>) -> Check {
    if check.status != CheckStatus::Ok {
        check.fix = Some(fix.into());
    }
    check
}

pub fn run(platform: &PlatformInfo, repo: Option<&Path>) -> Vec<Check> {
    let mut checks = Vec::new();
    let macos = platform.os == "macos";

    checks.push(match platform.os.as_str() {
        "linux" | "macos" => check(
            "platform",
            "Workstation",
            CheckStatus::Ok,
            format!("{} on {}", platform.os_name.clone().unwrap_or_else(|| platform.os_label().into()), platform.arch),
        ),
        _ => check("platform", "Workstation", CheckStatus::Error, "BMAC workstation scripts run on Linux and macOS only."),
    });

    checks.push(match repo {
        Some(root) => {
            let info = crate::repo::info(root);
            if info.valid {
                check("repository", "BMAC repository", CheckStatus::Ok, info.root)
            } else {
                with_fix(
                    check("repository", "BMAC repository", CheckStatus::Error, info.problems.join("; ")),
                    "Select a BMAC checkout that includes the dashboard protocol in Settings.",
                )
            }
        }
        None => with_fix(
            check("repository", "BMAC repository", CheckStatus::Error, "No BMAC checkout was found."),
            "Open Settings and choose the directory of your BMAC checkout.",
        ),
    });

    checks.push(match find_bash() {
        Some(path) => {
            let version = bash_version(&path).map(|v| v.2).unwrap_or_default();
            check("bash", "Bash 4.4+", CheckStatus::Ok, format!("{} ({version})", path.display()))
        }
        None => with_fix(
            check("bash", "Bash 4.4+", CheckStatus::Error, "No Bash 4.4 or newer was found."),
            if macos { "brew install bash" } else { "Install bash 4.4 or newer." },
        ),
    });

    checks.push(match which("python3").and_then(|p| command_output(p.to_str()?, &["-c", "import sys; print('%d.%d' % sys.version_info[:2])"])) {
        Some(version) => {
            let ok = version
                .split_once('.')
                .and_then(|(a, b)| Some((a.parse::<u32>().ok()?, b.parse::<u32>().ok()?)))
                .is_some_and(|(a, b)| a > 3 || (a == 3 && b >= 9));
            with_fix(
                check(
                    "python3",
                    "Python 3.9+",
                    if ok { CheckStatus::Ok } else { CheckStatus::Error },
                    format!("python3 {version}"),
                ),
                "Install Python 3.9 or newer.",
            )
        }
        None => with_fix(check("python3", "Python 3.9+", CheckStatus::Error, "python3 was not found."), "Install Python 3.9 or newer."),
    });

    checks.push(match which("ssh") {
        Some(path) => check("ssh", "OpenSSH client", CheckStatus::Ok, path.display().to_string()),
        None => check("ssh", "OpenSSH client", CheckStatus::Error, "ssh was not found."),
    });

    checks.push(if std::env::var_os("SSH_AUTH_SOCK").is_some() {
        check("ssh_agent", "SSH agent", CheckStatus::Ok, "SSH_AUTH_SOCK is set.")
    } else {
        with_fix(
            check(
                "ssh_agent",
                "SSH agent",
                CheckStatus::Warning,
                "No SSH agent is visible to the dashboard. A passphrase-protected key fails as \"Permission denied (publickey)\".",
            ),
            if macos { "ssh-add --apple-use-keychain ~/.ssh/id_ed25519" } else { "Start the dashboard from a session with ssh-agent running." },
        )
    });

    if macos {
        let ok = which("openssl")
            .map(|p| Command::new(p).args(["passwd", "-6", "probe"]).output().map(|o| o.status.success()).unwrap_or(false))
            .unwrap_or(false);
        checks.push(with_fix(
            check(
                "openssl",
                "OpenSSL with passwd -6",
                if ok { CheckStatus::Ok } else { CheckStatus::Warning },
                if ok { "openssl supports SHA-512 password hashes." } else { "The first openssl on PATH is LibreSSL, which the VM creators cannot use." },
            ),
            "brew install openssl@3",
        ));
    }

    if platform.os == "linux" {
        checks.push(match which("flock") {
            Some(_) => check("flock", "flock", CheckStatus::Ok, "Available for host and QDevice workflows."),
            None => check("flock", "flock", CheckStatus::Warning, "Host and QDevice workflows need flock (util-linux)."),
        });
    }

    checks.push(match which("tailscale") {
        Some(path) => match command_output(path.to_str().unwrap_or("tailscale"), &["status", "--json"]) {
            Some(text) => {
                let state = serde_json::from_str::<serde_json::Value>(&text)
                    .ok()
                    .and_then(|v| v.get("BackendState").and_then(|s| s.as_str()).map(str::to_string))
                    .unwrap_or_else(|| "unknown".into());
                with_fix(
                    check(
                        "tailscale",
                        "Tailscale",
                        if state == "Running" { CheckStatus::Ok } else { CheckStatus::Warning },
                        format!("Backend state: {state}"),
                    ),
                    "Connect Tailscale so the cluster hosts are reachable.",
                )
            }
            None => check("tailscale", "Tailscale", CheckStatus::Warning, "tailscale status did not answer."),
        },
        None => check("tailscale", "Tailscale", CheckStatus::Warning, "The tailscale CLI was not found on PATH."),
    });

    if let Some(root) = repo {
        let conf = root.join("env/cluster.conf").is_file();
        checks.push(with_fix(
            check(
                "cluster_conf",
                "env/cluster.conf",
                if conf { CheckStatus::Ok } else { CheckStatus::Error },
                if conf { "Present." } else { "Missing; every cluster workflow needs it." },
            ),
            "Copy env/cluster_dot_conf to env/cluster.conf and fill it in.",
        ));
        let secrets = root.join("env/secrets.env").is_file();
        checks.push(with_fix(
            check(
                "secrets_env",
                "env/secrets.env",
                if secrets { CheckStatus::Ok } else { CheckStatus::Warning },
                if secrets { "Present (its contents are never read by the dashboard)." } else { "Missing; creating hosts and VMs needs it." },
            ),
            "Copy env/secrets_dot_env to env/secrets.env, fill it in, and chmod 600 it.",
        ));
    }
    checks
}
