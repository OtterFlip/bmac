// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

//! Backup appliances in the Dashboard (`DEPLOYMENT.md`, "Backup appliance
//! ISO"): `POST /api/v1/appliances` issues an enrollment bundle,
//! `GET /api/v1/appliances` lists the appliances and the requests waiting
//! for approval, and approving a request records the appliance's
//! certificate fingerprint, which is all the cluster ever trusts it by.

use serde::{Deserialize, Serialize};
use ts_rs::TS;

/// How long a bundle is valid by default, and at most.
pub const DEFAULT_TTL_HOURS: u32 = 7 * 24;
pub const MAX_TTL_HOURS: u32 = 7 * 24;

/// The body of `POST /api/v1/appliances`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(optional_fields)]
#[serde(deny_unknown_fields)]
pub struct IssueEnroll {
    /// Defaults to [`DEFAULT_TTL_HOURS`]; never more than [`MAX_TTL_HOURS`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ttl_hours: Option<u32>,
}

/// The bundle file, shown once. It holds the single-use enrollment token,
/// which the cluster keeps only as a hash.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct IssuedEnroll {
    pub file_name: String,
    /// The bundle's JSON.
    pub content: String,
}

impl std::fmt::Debug for IssuedEnroll {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("IssuedEnroll")
            .field("file_name", &self.file_name)
            .finish_non_exhaustive()
    }
}

/// The body of `POST /api/v1/appliances/requests/<id>/approve`: the
/// fingerprint the administrator compared with the appliance's console, so
/// approval is for exactly that certificate.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(deny_unknown_fields)]
pub struct ApproveAppliance {
    pub fingerprint: String,
}

/// An enrollment request waiting for an administrator.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct EnrollRequestView {
    pub request_id: String,
    pub appliance_uuid: Option<String>,
    pub name: Option<String>,
    /// The SHA-256 of its certificate, as colon-separated hex.
    pub fingerprint: Option<String>,
    /// Where the request came from.
    pub from: String,
    /// The address it suggested for restore jobs.
    pub suggested_address: Option<String>,
    pub received_by: String,
    pub created_at_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct ApplianceView {
    pub appliance_uuid: String,
    pub name: String,
    pub fingerprint: String,
    pub address: Option<String>,
    /// `pending`, `approved`, or `revoked`.
    pub enrollment: String,
    pub enrolled_at_ms: i64,
    pub last_checkin_ms: Option<i64>,
    pub version: Option<String>,
    pub locked: bool,
    pub paused: bool,
    pub retired: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct ApplianceList {
    pub appliances: Vec<ApplianceView>,
    pub requests: Vec<EnrollRequestView>,
    /// The caller may issue, approve, reject, and revoke (admins only).
    pub can_manage: bool,
}

/// Colon-separated uppercase hex, as the Dashboard shows fingerprints.
pub fn display_fingerprint(hex: &str) -> String {
    hex.as_bytes()
        .chunks(2)
        .map(|pair| String::from_utf8_lossy(pair).to_ascii_uppercase())
        .collect::<Vec<_>>()
        .join(":")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_issued_bundle_never_prints_its_content() {
        let issued = IssuedEnroll {
            file_name: "vault-enroll.json".into(),
            content: "{\"enroll_token\":\"secret\"}".into(),
        };
        assert!(!format!("{issued:?}").contains("secret"));
    }

    #[test]
    fn fingerprints_show_as_colon_separated_pairs() {
        assert_eq!(display_fingerprint("ab01ff"), "AB:01:FF");
    }
}
