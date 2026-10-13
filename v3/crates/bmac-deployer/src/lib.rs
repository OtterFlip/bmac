// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

//! The BMAC Deployer's work, independent of its window (`DEPLOYMENT.md`,
//! "BMAC Deployer"): long work runs as [`tasks`], the workstation is checked
//! by [`preflight`], and the Deployer's few settings live in [`settings`].
//! [`hostiso`] builds a host's Proxmox ISO from files [`download`] fetches.

pub mod boot;
pub mod discovery;
pub mod download;
pub mod flows;
pub mod handoff;
pub mod hello;
pub mod hostiso;
pub mod jump_ssh;
pub mod preflight;
pub mod settings;
pub mod shred;
pub mod tailnet;
pub mod tasks;
pub mod tls;
pub mod workstation_ssh;

/// Whether `url` is a plain `https` link the Deployer may open in the
/// operator's browser: no credentials, whitespace, or control characters,
/// as v2's `open_external` requires.
pub fn plain_https(url: &str) -> bool {
    let Some(rest) = url.strip_prefix("https://") else {
        return false;
    };
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    url.len() <= 2048
        && !authority.is_empty()
        && !authority.contains('@')
        && !url.chars().any(|c| c.is_control() || c.is_whitespace())
}

/// Milliseconds since the Unix epoch.
pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    #[test]
    fn only_plain_https_links_open() {
        assert!(super::plain_https("https://mox1.tailnet.ts.net:8443/"));
        assert!(!super::plain_https("http://mox1:8443/"));
        assert!(!super::plain_https("https://user@mox1/"));
        assert!(!super::plain_https("https://mox1/ x"));
        assert!(!super::plain_https("https:///path"));
        assert!(!super::plain_https("javascript:alert(1)"));
    }
}
