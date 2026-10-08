//! Workstation platform detection.

use std::path::Path;
use std::process::Command;

use serde::Serialize;

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct PlatformInfo {
    /// `linux`, `macos`, or another std::env::consts::OS value.
    pub os: String,
    /// `x86_64`, `aarch64`, ...
    pub arch: String,
    /// `debian` for Debian-based Linux distributions, when detectable.
    pub os_family: Option<String>,
    /// A human-readable name such as "Ubuntu 24.04.2 LTS".
    pub os_name: Option<String>,
    pub hostname: Option<String>,
}

impl PlatformInfo {
    pub fn detect() -> Self {
        let os = std::env::consts::OS.to_string();
        let arch = std::env::consts::ARCH.to_string();
        let (os_family, os_name) = if os == "linux" {
            let release = std::fs::read_to_string("/etc/os-release").unwrap_or_default();
            let family = if Path::new("/etc/debian_version").exists()
                || os_release_value(&release, "ID_LIKE").is_some_and(|v| v.split(' ').any(|p| p == "debian"))
                || os_release_value(&release, "ID").as_deref() == Some("debian")
            {
                Some("debian".to_string())
            } else {
                None
            };
            (family, os_release_value(&release, "PRETTY_NAME"))
        } else if os == "macos" {
            let version = command_output("sw_vers", &["-productVersion"]);
            (None, Some(format!("macOS {}", version.unwrap_or_default()).trim().to_string()))
        } else {
            (None, None)
        };
        let hostname = command_output("hostname", &[]);
        Self { os, arch, os_family, os_name, hostname }
    }

    pub fn os_label(&self) -> &str {
        match self.os.as_str() {
            "linux" => "Linux",
            "macos" => "macOS",
            "windows" => "Windows",
            other => other,
        }
    }
}

fn os_release_value(text: &str, key: &str) -> Option<String> {
    text.lines().find_map(|line| {
        let value = line.strip_prefix(key)?.strip_prefix('=')?;
        Some(value.trim().trim_matches('"').to_string())
    })
}

pub fn command_output(program: &str, args: &[&str]) -> Option<String> {
    let output = Command::new(program).args(args).output().ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout).trim().to_string();
    (!text.is_empty()).then_some(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_os_release_values() {
        let text = "NAME=\"Ubuntu\"\nID=ubuntu\nID_LIKE=debian\nPRETTY_NAME=\"Ubuntu 24.04 LTS\"\n";
        assert_eq!(os_release_value(text, "ID_LIKE").as_deref(), Some("debian"));
        assert_eq!(os_release_value(text, "PRETTY_NAME").as_deref(), Some("Ubuntu 24.04 LTS"));
        assert_eq!(os_release_value(text, "ID").as_deref(), Some("ubuntu"));
    }
}
