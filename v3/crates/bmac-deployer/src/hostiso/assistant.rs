// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

//! Proxmox's `proxmox-auto-install-assistant`, pinned to one package and
//! verified by hash before it is unpacked (v2's `assistant_binary`).

use std::fs;
use std::path::{Path, PathBuf};

use bmac_exec::{Command, CommandRunner};

use crate::download::{self, Fetch, Pinned};

pub const VERSION: &str = "9.2.5";
pub const PACKAGE_URL: &str = "http://download.proxmox.com/debian/pve/dists/trixie/pve-no-subscription/binary-amd64/proxmox-auto-install-assistant_9.2.5_amd64.deb";
pub const PACKAGE_SHA256: &str = "9a61e37c3efce192a5ea86af06b3ccf7027fe581226068911d4833f4af7ac83e";

pub fn pin() -> Pinned {
    Pinned {
        url: PACKAGE_URL.into(),
        sha256: PACKAGE_SHA256.into(),
    }
}

/// The unpacked binary, downloading and unpacking the package first when
/// needed.
pub async fn prepare(
    cache_dir: &Path,
    fetch: &dyn Fetch,
    runner: &dyn CommandRunner,
    dpkg_deb: &Path,
    note: &(dyn Fn(String) + Send + Sync),
) -> Result<PathBuf, String> {
    let tools = cache_dir.join("tools");
    let unpacked = tools.join(format!("proxmox-auto-install-assistant-{VERSION}"));
    let binary = unpacked.join("usr/bin/proxmox-auto-install-assistant");
    if binary.is_file() {
        return Ok(binary);
    }
    let deb = download::ensure(&tools, &pin(), fetch, &|_, _| {}, note).await?;
    let partial = tools.join(format!(".unpack-{VERSION}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&partial);
    runner
        .run(
            Command::new(dpkg_deb)
                .arg("-x")
                .arg(deb.display().to_string())
                .arg(partial.display().to_string()),
        )
        .await
        .map_err(|e| {
            let _ = fs::remove_dir_all(&partial);
            format!("unpacking the auto-install assistant: {e}")
        })?;
    let _ = fs::remove_dir_all(&unpacked);
    fs::rename(&partial, &unpacked).map_err(|e| format!("{}: {e}", unpacked.display()))?;
    if !binary.is_file() {
        return Err(format!(
            "The assistant package has no {}.",
            binary.display()
        ));
    }
    Ok(binary)
}
