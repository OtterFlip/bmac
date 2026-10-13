// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

//! The Settings page's API: cluster settings and host facts with their key
//! schema (`GET /api/v1/settings`), saving either with a revision check
//! (`PUT /api/v1/settings/cluster`, `PUT /api/v1/settings/hosts/{host}`),
//! and every host's certificate and SSH host key fingerprints as each host
//! published them to pmxcfs (`SECURITY.md`, "Certificates").

use std::collections::BTreeMap;

use bmac_core::HostSlot;
use serde::{Deserialize, Serialize};
use ts_rs::TS;

/// One key of `cluster.conf` or `moxN.conf`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct SettingKey {
    pub name: String,
    /// What it holds, such as `ipv4`, `private_address`, or `choice`.
    pub kind: String,
    /// The allowed values of a `choice`; empty otherwise.
    pub choices: Vec<String>,
    pub required: bool,
    /// Part of the cluster's address plan: fixed once the cluster settings
    /// exist.
    pub locked: bool,
}

/// Stored settings at a revision.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct SettingsRecord {
    pub revision: u64,
    pub values: BTreeMap<String, String>,
    pub updated_by: String,
    pub updated_at_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct HostFactsRecord {
    pub host: HostSlot,
    #[serde(flatten)]
    pub record: SettingsRecord,
}

/// What a host published about its own keys.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct HostIdentity {
    pub host: HostSlot,
    /// The Dashboard certificate's SHA-256, as colon-separated hex.
    pub https_fingerprint: Option<String>,
    /// `ssh-ed25519 <base64>`, for a `known_hosts` line.
    pub ssh_host_key: Option<String>,
    /// `SHA256:<base64>`, as `ssh-keygen -l` prints it.
    pub ssh_host_key_fingerprint: Option<String>,
    /// Its MagicDNS name, where its Dashboard is reached.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tailnet_name: Option<String>,
    pub updated_at_ms: i64,
}

/// `GET /api/v1/settings`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct SettingsView {
    /// Absent until someone saves them.
    pub cluster: Option<SettingsRecord>,
    pub hosts: Vec<HostFactsRecord>,
    pub cluster_keys: Vec<SettingKey>,
    pub host_keys: Vec<SettingKey>,
    pub identities: Vec<HostIdentity>,
    /// The caller may save (admins only).
    pub can_edit: bool,
}

/// The body of both `PUT`s: every value (a key left out is removed), and
/// the revision they were read at, or `null` to create them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(deny_unknown_fields)]
pub struct SaveSettings {
    pub values: BTreeMap<String, String>,
    pub expected_revision: Option<u64>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn host_facts_flatten_their_record() {
        let facts = HostFactsRecord {
            host: HostSlot::new(2).unwrap(),
            record: SettingsRecord {
                revision: 3,
                values: BTreeMap::from([("PROXMOX_IP".into(), "203.0.113.2".into())]),
                updated_by: "root@pam".into(),
                updated_at_ms: 7,
            },
        };
        assert_eq!(
            serde_json::to_value(&facts).unwrap(),
            json!({"host": "mox2", "revision": 3, "values": {"PROXMOX_IP": "203.0.113.2"}, "updated_by": "root@pam", "updated_at_ms": 7})
        );
    }

    #[test]
    fn a_save_names_only_values_and_a_revision() {
        let extra = json!({"values": {}, "expected_revision": 1, "updated_by": "mallory"});
        assert!(serde_json::from_value::<SaveSettings>(extra).is_err());
        let create: SaveSettings =
            serde_json::from_value(json!({"values": {}, "expected_revision": null})).unwrap();
        assert_eq!(create.expected_revision, None);
    }
}
