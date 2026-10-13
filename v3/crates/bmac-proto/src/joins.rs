// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

//! Adding a host to a running cluster (`DEPLOYMENT.md`, "Additional
//! hosts"): `POST /api/v1/joins` reserves the slot and returns the join
//! bundle the Deployer builds the new host's ISO from, `GET /api/v1/joins`
//! lists the outstanding bundles with the request each one's host made, and
//! `DELETE /api/v1/joins/<host>` revokes one. Approving a host is launching
//! `hosts.join` for its slot.

use bmac_core::HostSlot;
use serde::{Deserialize, Serialize};
use ts_rs::TS;

/// How long a bundle is valid by default, and at most.
pub const DEFAULT_TTL_HOURS: u32 = 7 * 24;
pub const MAX_TTL_HOURS: u32 = 7 * 24;

/// The body of `POST /api/v1/joins`. The network ports and addresses are
/// chosen in the Deployer from the discovered hardware.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(optional_fields)]
#[serde(deny_unknown_fields)]
pub struct IssueJoin {
    pub host: HostSlot,
    pub max_prod_vms: u32,
    pub max_staging_vms: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idrac_ip: Option<String>,
    /// Defaults to [`DEFAULT_TTL_HOURS`]; never more than [`MAX_TTL_HOURS`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ttl_hours: Option<u32>,
}

/// The answer to `POST /api/v1/joins`: the bundle file, shown once. It holds
/// the single-use join token, which the cluster keeps only as a hash.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct IssuedJoin {
    pub file_name: String,
    /// The bundle's JSON.
    pub content: String,
}

impl std::fmt::Debug for IssuedJoin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("IssuedJoin")
            .field("file_name", &self.file_name)
            .finish_non_exhaustive()
    }
}

/// What a new host sent with its join token, for the administrator who
/// approves it to compare with the Deployer's host card.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct JoinRequestView {
    pub request_id: String,
    /// Where the request came from: the new host's Tailscale address.
    pub address: String,
    pub ssh_host_key_fingerprint: Option<String>,
    pub https_fingerprint: Option<String>,
    pub received_by: HostSlot,
    pub created_at_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct JoinView {
    pub host: HostSlot,
    pub issued_by: String,
    pub issued_at_ms: i64,
    pub expires_at_ms: i64,
    pub used_at_ms: Option<i64>,
    /// Present once the new host has presented the token.
    pub request: Option<JoinRequestView>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct JoinList {
    pub joins: Vec<JoinView>,
    /// The caller may issue, revoke, and approve (admins only).
    pub can_manage: bool,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn an_issued_bundle_never_prints_its_content() {
        let issued = IssuedJoin {
            file_name: "mox2-join.json".into(),
            content: "{\"join_token\":\"secret\"}".into(),
        };
        assert!(!format!("{issued:?}").contains("secret"));
    }

    #[test]
    fn issue_requests_refuse_extra_keys() {
        let body = json!({"host": "mox2", "max_prod_vms": 2, "max_staging_vms": 1});
        assert!(serde_json::from_value::<IssueJoin>(body.clone()).is_ok());
        let mut extra = body;
        extra["slot"] = json!("mox3");
        assert!(serde_json::from_value::<IssueJoin>(extra).is_err());
    }
}
