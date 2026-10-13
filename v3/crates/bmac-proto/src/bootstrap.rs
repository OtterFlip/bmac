// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

//! What a new host starts from (`DEPLOYMENT.md`, "Proxmox host ISO"). The
//! Deployer writes a [`HostBootstrap`] into the host's ISO, and the
//! first-boot script copies it to `/var/lib/bmac/bootstrap/`, where
//! `bmacd`'s setup mode reads it. It carries either a [`NewClusterSeed`]
//! (the first host) or the [`JoinBundle`] an existing host issued.

use std::collections::BTreeMap;
use std::fmt;

use bmac_core::HostSlot;
use bmac_storage::layout::PoolLayout;
use serde::{Deserialize, Serialize};

use crate::inventory::Inventory;

pub const BOOTSTRAP_VERSION: u32 = 1;
pub const JOIN_BUNDLE_VERSION: u32 = 1;
/// Where the first-boot script puts the bootstrap file.
pub const BOOTSTRAP_DIR: &str = "/var/lib/bmac/bootstrap";
pub const BOOTSTRAP_FILE: &str = "bootstrap.json";

/// Settings as `KEY=value` pairs, as in v2's `.conf` files.
pub type SettingValues = BTreeMap<String, String>;

/// Everything a new host needs to set itself up.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HostBootstrap {
    pub version: u32,
    pub host: HostSlot,
    /// The content of the host's `moxN.conf`.
    pub host_settings: SettingValues,
    pub origin: BootstrapOrigin,
    /// The pool the setup job builds; the installer made only its boot vdev.
    pub layout: PoolLayout,
    /// The hardware as the discovery session saw it, so the setup job can
    /// check it is on the same machine with the same disks.
    pub inventory: Inventory,
    /// The SHA-256 fingerprint of the HTTPS certificate in the ISO.
    pub https_fingerprint: String,
    /// The public half of the SSH host key in the ISO.
    pub ssh_host_key: String,
    /// The host's name on the tailnet.
    pub tailscale_name: String,
    pub built_ms: i64,
    pub deployer_version: String,
}

/// How the host comes into a cluster.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum BootstrapOrigin {
    NewCluster(NewClusterSeed),
    Join(JoinBundle),
}

/// The first host creates the cluster from this.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NewClusterSeed {
    pub cluster_uuid: String,
    /// The content of `cluster.conf`; no secrets.
    pub cluster_settings: SettingValues,
    /// Digests of Tailscale auth keys already used, starting with this
    /// host's own, so the cluster refuses them later.
    pub used_tailscale_key_sha256: Vec<String>,
}

/// Issued by a running cluster for one more host (`DEPLOYMENT.md`,
/// "Additional hosts"). The token is the only secret in it.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JoinBundle {
    pub version: u32,
    pub cluster_uuid: String,
    pub cluster_name: String,
    pub cluster_settings: SettingValues,
    /// The slot reserved in the registry for the new host.
    pub slot: HostSlot,
    pub host_settings: SettingValues,
    /// Single use; the cluster keeps only its hash and expiry.
    pub join_token: String,
    pub expires_ms: i64,
    pub hosts: Vec<BundleHost>,
    pub proxmox_ca_fingerprint: String,
    /// The public half of the key the cluster uses to set the host up; the
    /// private half stays in pmxcfs.
    pub setup_ssh_public_key: String,
    pub used_tailscale_key_sha256: Vec<String>,
}

impl fmt::Debug for JoinBundle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("JoinBundle")
            .field("cluster_uuid", &self.cluster_uuid)
            .field("cluster_name", &self.cluster_name)
            .field("slot", &self.slot)
            .field("expires_ms", &self.expires_ms)
            .field("hosts", &self.hosts)
            .finish_non_exhaustive()
    }
}

/// A current cluster host, as the join bundle lists it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BundleHost {
    pub host: HostSlot,
    pub dashboard_url: String,
    pub https_fingerprint: String,
    /// Its private VLAN address, so a joining host can't take it.
    pub private_ip: String,
}

impl HostBootstrap {
    pub fn cluster_settings(&self) -> &SettingValues {
        match &self.origin {
            BootstrapOrigin::NewCluster(seed) => &seed.cluster_settings,
            BootstrapOrigin::Join(bundle) => &bundle.cluster_settings,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bundle() -> JoinBundle {
        JoinBundle {
            version: JOIN_BUNDLE_VERSION,
            cluster_uuid: "c0ffee".into(),
            cluster_name: "lab".into(),
            cluster_settings: SettingValues::new(),
            slot: HostSlot::new(2).unwrap(),
            host_settings: SettingValues::new(),
            join_token: "join-secret-token".into(),
            expires_ms: 1,
            hosts: Vec::new(),
            proxmox_ca_fingerprint: "AA".into(),
            setup_ssh_public_key: "ssh-ed25519 AAAA setup".into(),
            used_tailscale_key_sha256: Vec::new(),
        }
    }

    #[test]
    fn a_join_bundle_never_prints_its_token() {
        let shown = format!("{:?}", bundle());
        assert!(!shown.contains("join-secret-token"));
        assert!(shown.contains("lab"));
    }

    #[test]
    fn the_origin_is_tagged() {
        let origin = BootstrapOrigin::Join(bundle());
        let value = serde_json::to_value(&origin).unwrap();
        assert_eq!(value["kind"], "join");
        let back: BootstrapOrigin = serde_json::from_value(value).unwrap();
        assert_eq!(back, origin);
    }
}
