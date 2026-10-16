// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

//! `manifest.json`: what a release holds and which record schemas it reads
//! and writes.

use serde::{Deserialize, Serialize};

/// The bundle layout this build reads.
pub const BUNDLE_FORMAT: u32 = 1;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub format: u32,
    /// The BMAC version, as `bmacd --version` prints it: `3.0.1`,
    /// `3.0.0-dev.0+b2`.
    pub version: String,
    pub built_at_s: i64,
    /// The pmxcfs record schema this release writes.
    pub record_schema: u64,
    /// The oldest record schema it still reads; a cluster whose records are
    /// older can't take it.
    pub reads_record_schema_from: u64,
    pub packages: Vec<Package>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Package {
    /// `bmacd`, `bmac-backupd`, `bmac-deployer`.
    pub name: String,
    /// The file in the bundle.
    pub file: String,
    /// The Debian version.
    pub version: String,
    pub architecture: String,
    /// Lowercase hex.
    pub sha256: String,
    pub size: u64,
}

impl Manifest {
    pub fn package(&self, name: &str) -> Option<&Package> {
        self.packages.iter().find(|p| p.name == name)
    }
}

/// A package's file name: a plain `.deb` name, no path.
pub(crate) fn file_name_ok(name: &str) -> bool {
    name.ends_with(".deb")
        && !name.starts_with(['.', '-'])
        && name.len() <= 90
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "._+-~".contains(c))
}

pub(crate) fn package_name_ok(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 40
        && name.starts_with(|c: char| c.is_ascii_lowercase())
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

pub(crate) fn digest_ok(text: &str) -> bool {
    text.len() == 64
        && text
            .chars()
            .all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_plain() {
        assert!(file_name_ok("bmacd_3.0.0~dev.0+b2_amd64.deb"));
        for bad in ["../bmacd.deb", "a/b.deb", "bmacd.tar", ".deb", "-x.deb"] {
            assert!(!file_name_ok(bad), "{bad}");
        }
        assert!(package_name_ok("bmac-backupd"));
        assert!(!package_name_ok("Bmacd") && !package_name_ok("a b"));
        assert!(digest_ok(&"ab".repeat(32)));
        assert!(!digest_ok(&"AB".repeat(32)) && !digest_ok("ab"));
    }
}
