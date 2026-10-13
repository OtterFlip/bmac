// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

use std::fs;
use std::path::{Path, PathBuf};

/// Where the inventory reads the system from. Tests point it at a fixture
/// tree; on the discovery OS it is the real one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Root {
    pub sys: PathBuf,
    pub dev: PathBuf,
    pub proc: PathBuf,
    /// udev's database, `/run/udev/data`.
    pub udev: PathBuf,
}

impl Root {
    pub fn system() -> Root {
        Root::under(Path::new("/"))
    }

    /// `sys/`, `dev/`, `proc/`, and `run/udev/data/` under `base`.
    pub fn under(base: &Path) -> Root {
        Root {
            sys: base.join("sys"),
            dev: base.join("dev"),
            proc: base.join("proc"),
            udev: base.join("run/udev/data"),
        }
    }

    /// The device node for a kernel name, such as `/dev/sda3`.
    pub fn device(&self, name: &str) -> PathBuf {
        self.dev.join(name)
    }
}

/// A sysfs attribute, trimmed; `None` when missing, unreadable, or blank.
pub fn attr(path: &Path) -> Option<String> {
    let text = fs::read_to_string(path).ok()?;
    let text = text.trim();
    (!text.is_empty()).then(|| text.to_string())
}

pub fn attr_u64(path: &Path) -> Option<u64> {
    attr(path)?.parse().ok()
}

/// The last component of a symlink's target, such as a driver's name.
pub fn link_name(path: &Path) -> Option<String> {
    let target = fs::read_link(path).ok()?;
    target.file_name()?.to_str().map(str::to_string)
}

/// Directory entries' names, sorted; empty when the directory is missing.
pub fn names(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(dir)
        .map(|entries| {
            entries
                .filter_map(Result::ok)
                .filter_map(|e| e.file_name().into_string().ok())
                .collect()
        })
        .unwrap_or_default();
    names.sort_by_key(|name| natural(name));
    names
}

/// Sorts `sda2` before `sda10` and `nvme2n1` before `nvme10n1`.
fn natural(name: &str) -> Vec<(String, u64)> {
    let mut parts = Vec::new();
    let mut text = String::new();
    let mut digits = String::new();
    for c in name.chars() {
        if c.is_ascii_digit() {
            digits.push(c);
        } else {
            if !digits.is_empty() {
                parts.push((
                    std::mem::take(&mut text),
                    digits.parse().unwrap_or(u64::MAX),
                ));
                digits.clear();
            }
            text.push(c);
        }
    }
    parts.push((text, digits.parse().unwrap_or(0)));
    parts
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_sort_numbers_by_value() {
        let dir = tempfile::tempdir().unwrap();
        for name in ["sda10", "sda2", "nvme10n1", "nvme2n1", "sda1"] {
            fs::write(dir.path().join(name), "").unwrap();
        }
        assert_eq!(
            names(dir.path()),
            ["nvme2n1", "nvme10n1", "sda1", "sda2", "sda10"]
        );
    }

    #[test]
    fn attributes_are_trimmed_and_blank_is_none() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("serial"), "S6BOOTA   \n").unwrap();
        fs::write(dir.path().join("blank"), "  \n").unwrap();
        assert_eq!(attr(&dir.path().join("serial")).as_deref(), Some("S6BOOTA"));
        assert_eq!(attr(&dir.path().join("blank")), None);
        assert_eq!(attr(&dir.path().join("missing")), None);
    }
}
