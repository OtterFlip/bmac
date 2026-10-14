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

/// The body of `POST /api/v1/appliances/<uuid>/address`: the address or
/// DNS name where the hosts and restore jobs reach the appliance's port
/// 8445, or none.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(deny_unknown_fields)]
pub struct SetApplianceAddress {
    pub address: Option<String>,
}

/// `GET /api/v1/backups`: everything the Backups page shows, from the
/// backup records, the duty host's `backups` observation, and every host's
/// storage observation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct BackupsView {
    pub at_ms: i64,
    pub appliances: Vec<BackupApplianceView>,
    pub vms: Vec<BackupVmView>,
    /// Newest first.
    pub restores: Vec<RestoreView>,
    /// Finished sends to appliances, newest first.
    pub transfers: Vec<TransferView>,
    /// The caller may change policies and appliances (admins only).
    pub can_manage: bool,
    pub errors: Vec<String>,
    /// The cluster's time zone (IANA): schedules run in it, and the
    /// Backups page shows times in it.
    pub time_zone: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct BackupApplianceView {
    pub appliance: ApplianceView,
    pub status: Option<crate::status::BackupApplianceStatus>,
    /// PITs it is pulling now.
    pub live: Vec<LiveTransferView>,
}

/// A claimed PIT and the bytes the appliance last reported for it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct LiveTransferView {
    pub vm: String,
    pub vm_uuid: String,
    pub pit_uuid: String,
    pub pit_name: String,
    pub bytes: u64,
    /// When bytes last moved.
    pub progress_s: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct BackupVmView {
    pub vm: String,
    pub vm_uuid: String,
    pub vmid: Option<u32>,
    pub policy: Option<PolicyView>,
    pub status: Option<crate::status::BackupVmStatus>,
    /// The catalog, oldest first.
    pub pits: Vec<PitNodeView>,
    /// The branch the next PIT goes on.
    pub branch_uuid: Option<String>,
    /// Space the PIT snapshots hold on each host.
    pub local: Vec<LocalSpaceView>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct PolicyView {
    pub revision: u64,
    pub enabled: bool,
    pub appliances: Vec<String>,
    pub local_newest: u32,
    pub local_days: u32,
    pub local_weeks: u32,
    /// Such as "daily at 03:00".
    pub every: String,
    /// The schedule as the policy API takes it, to send back unchanged.
    pub every_json: String,
    /// `if_available`, `required`, or `never`.
    pub freeze: String,
    pub accept_multi_pool: bool,
    pub updated_by: String,
    pub updated_at_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct PitNodeView {
    pub pit_uuid: String,
    pub branch_uuid: String,
    pub parent: Option<String>,
    pub name: String,
    pub taken_s: i64,
    /// `snapshot` or `bookmark`.
    pub presence: String,
    /// `crash`, `filesystem`, or `application`.
    pub consistency: String,
    pub offers: Vec<PitOfferView>,
    pub release_blocked: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct PitOfferView {
    pub appliance_uuid: String,
    /// `open`, `claimed`, `published`, or `failed`.
    pub state: String,
    pub bytes: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct LocalSpaceView {
    pub host: String,
    pub snapshots: u32,
    /// Bytes only the PIT snapshots hold (`used` of each).
    pub bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct RestoreView {
    pub restore_uuid: String,
    pub vm_uuid: String,
    pub vm: Option<String>,
    pub pit_uuid: String,
    pub appliance_uuid: String,
    /// `new_vm`, `test`, `into_existing`, `rollback`, `promotion`, or
    /// `recovery`.
    pub mode: String,
    pub host: String,
    pub vmid: Option<u32>,
    pub name: Option<String>,
    /// Each volume's method (1, 2, or 3), by slot.
    pub methods: std::collections::BTreeMap<String, u8>,
    pub started_at_ms: i64,
    pub finished_at_ms: Option<i64>,
    pub ended_at_ms: Option<i64>,
    pub error: Option<String>,
    pub test: Option<RestoreTestView>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct RestoreTestView {
    pub booted: bool,
    pub agent: bool,
    pub agent_after_s: Option<u64>,
}

/// One finished send of a volume to an appliance.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(deny_unknown_fields)]
pub struct TransferView {
    pub at_ms: i64,
    /// The host that sent it.
    pub host: String,
    pub appliance: String,
    pub vm_uuid: String,
    pub pit_uuid: String,
    pub volume_uuid: String,
    pub bytes: u64,
    pub duration_ms: u64,
    pub error: Option<String>,
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
