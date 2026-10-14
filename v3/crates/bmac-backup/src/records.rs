// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

//! The backup records BMAC keeps in pmxcfs under `/etc/pve/priv/bmac/backup`
//! (`BACKUP.md`, "Backup policies" and "PIT catalog"):
//!
//! ```text
//! settings.json                       BackupSettings
//! policies/<vm_uuid>.json             Policy
//! appliances/<appliance_uuid>.json    ApplianceRecord
//! pits/<vm_uuid>/lineage.json         Lineage
//! pits/<vm_uuid>/<pit_uuid>.json      PitRecord
//! ```

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::PitName;
use crate::lifetime::Offer;
use crate::manifest::Consistency;
use crate::retention::LocalRetention;
use crate::states::Progress;

pub const ROOT: &str = "/etc/pve/priv/bmac/backup";
pub const RECORD_SCHEMA: u64 = 1;

/// How a VM's PIT is made consistent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Freeze {
    /// Freeze when the guest agent answers; a crash-consistent PIT otherwise.
    #[default]
    IfAvailable,
    /// A PIT fails when the freeze does.
    Required,
    /// Never freeze: crash-consistent, chosen explicitly.
    Never,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Policy {
    pub schema_version: u64,
    pub record_type: String,
    #[serde(default)]
    pub revision: u64,
    pub vm_uuid: Uuid,
    /// The guest's name when the policy was saved, for display.
    pub vm: String,
    /// The schedule record (`schedules/<id>.json`) the policy owns.
    pub schedule_id: String,
    #[serde(default)]
    pub appliances: Vec<Uuid>,
    #[serde(default)]
    pub local_retention: LocalRetention,
    #[serde(default)]
    pub freeze: Freeze,
    /// Snapshot disks on several pools, which isn't atomic across them.
    #[serde(default)]
    pub accept_multi_pool: bool,
    pub enabled: bool,
    pub updated_by: String,
    pub updated_at_ms: i64,
}

impl Policy {
    pub fn schedule_id_for(vm_uuid: &Uuid) -> String {
        format!("backup-{vm_uuid}")
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Enrollment {
    /// Waiting for an administrator's approval in the Dashboard.
    Pending,
    Approved,
    Revoked,
}

/// What an appliance last reported for one VM.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReportedHead {
    pub branch_uuid: Uuid,
    pub pit_uuid: Uuid,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ApplianceRecord {
    pub schema_version: u64,
    pub record_type: String,
    #[serde(default)]
    pub revision: u64,
    pub appliance_uuid: Uuid,
    pub name: String,
    /// SHA-256 of its client certificate, as `normalized_fingerprint` gives.
    pub fingerprint: String,
    /// Where restore jobs reach its port 8445: an IP address or a DNS name,
    /// such as a dynamic DNS name. Appliances check in on their own, so
    /// backups work without it.
    #[serde(default)]
    pub address: Option<bmac_core::ApplianceAddress>,
    pub enrollment: Enrollment,
    pub enrolled_at_ms: i64,
    #[serde(default)]
    pub paused: bool,
    #[serde(default)]
    pub retired: bool,
    /// Written when its state changes, or when the stored time falls half a
    /// ready window behind, so every host agrees on availability without a
    /// pmxcfs write per check-in.
    #[serde(default)]
    pub last_checkin_ms: Option<i64>,
    #[serde(default)]
    pub version: Option<String>,
    #[serde(default)]
    pub locked: bool,
    #[serde(default)]
    pub progress: Progress,
    #[serde(default)]
    pub heads: BTreeMap<Uuid, ReportedHead>,
    #[serde(default)]
    pub index_revision: u64,
    #[serde(default)]
    pub scrub: Option<ScrubSummary>,
}

/// The appliance's last scrub as its check-in reports it, without the
/// progress, so a running scrub doesn't rewrite the record.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScrubSummary {
    pub state: String,
    pub finished_at_ms: Option<i64>,
    pub errors: u64,
    pub suspect_pits: Vec<Uuid>,
}

/// A VM's identity in the backups: its current branch and volumes.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Lineage {
    pub schema_version: u64,
    pub record_type: String,
    #[serde(default)]
    pub revision: u64,
    pub vm_uuid: Uuid,
    pub branch_uuid: Uuid,
    /// The newest PIT captured on the branch, even after the catalog lets
    /// it go: the next PIT's parent.
    pub head: Option<Uuid>,
    /// Each disk's `volume_uuid`, by the dataset that holds it.
    #[serde(default)]
    pub volumes: BTreeMap<String, Uuid>,
    /// The last scheduled or manual attempt, for the alerts.
    #[serde(default)]
    pub last_attempt: Option<Attempt>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case", deny_unknown_fields)]
pub enum Attempt {
    Captured {
        at_ms: i64,
        pit_uuid: Uuid,
    },
    /// No assigned appliance was available, and local retention didn't
    /// claim the slot.
    Skipped {
        at_ms: i64,
    },
    Failed {
        at_ms: i64,
        message: String,
    },
}

impl Attempt {
    pub fn at_ms(&self) -> i64 {
        match self {
            Attempt::Captured { at_ms, .. }
            | Attempt::Skipped { at_ms }
            | Attempt::Failed { at_ms, .. } => *at_ms,
        }
    }
}

/// Where a PIT's data still is on the cluster.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Presence {
    /// Its snapshots exist on every placement host.
    Snapshot,
    /// Its snapshots are released; bookmarks remain on every placement host.
    Bookmark,
}

/// One PIT in the catalog: its sealed manifest and the cluster's view of it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PitRecord {
    pub schema_version: u64,
    pub record_type: String,
    #[serde(default)]
    pub revision: u64,
    pub pit_uuid: Uuid,
    pub vm_uuid: Uuid,
    pub branch_uuid: Uuid,
    pub parent: Option<Uuid>,
    pub name: PitName,
    pub taken_s: i64,
    pub consistency: Consistency,
    /// The manifest as sealed (`Manifest::seal`), served to appliances as is.
    pub manifest: String,
    pub presence: Presence,
    /// Offers by appliance UUID.
    #[serde(default)]
    pub offers: BTreeMap<Uuid, Offer>,
    /// Why releasing the snapshots last failed, such as a dependent clone.
    #[serde(default)]
    pub release_blocked: Option<String>,
}

/// How long a restore's PIT is named to the appliances as a pending
/// branch root.
pub const BRANCH_ROOT_KEEP_MS: i64 = 30 * 24 * 3600 * 1000;

/// One restore job and what it left on the cluster.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RestoreRecord {
    pub schema_version: u64,
    pub record_type: String,
    #[serde(default)]
    pub revision: u64,
    pub restore_uuid: Uuid,
    /// The VM and PIT restored.
    pub vm_uuid: Uuid,
    pub pit_uuid: Uuid,
    pub branch_uuid: Uuid,
    pub appliance_uuid: Uuid,
    pub mode: crate::restore::Mode,
    pub host: String,
    /// The VM it made or changed, once known.
    pub vmid: Option<u32>,
    pub name: Option<String>,
    /// Each volume's method (1, 2, or 3), by slot.
    #[serde(default)]
    pub methods: BTreeMap<String, u8>,
    pub started_at_ms: i64,
    pub finished_at_ms: Option<i64>,
    /// Why it failed; none when it finished.
    pub error: Option<String>,
    /// Set when the restored VM is cleaned up or the restore abandoned; it
    /// stops naming the PIT as a pending branch root then.
    #[serde(default)]
    pub ended_at_ms: Option<i64>,
    /// The disks set aside, by slot, with their snapshots and bookmarks,
    /// until a cleanup destroys them.
    #[serde(default)]
    pub replaced: BTreeMap<String, String>,
    /// A restore test's outcome; its scratch VM is gone.
    #[serde(default)]
    pub test: Option<TestOutcome>,
    /// The PIT's sealed manifest, so backups can continue from it after
    /// the catalog lets the PIT go.
    #[serde(default)]
    pub manifest: Option<String>,
}

/// What a restore test found (`BACKUP.md`, "Restore tests").
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TestOutcome {
    pub booted: bool,
    pub agent: bool,
    /// Seconds from the VM's start until its agent answered.
    pub agent_after_s: Option<u64>,
}

impl RestoreRecord {
    /// Whether the restore used its PIT and still stands.
    pub fn stands(&self) -> bool {
        self.finished_at_ms.is_some() && self.error.is_none() && self.ended_at_ms.is_none()
    }

    /// Whether the VM's backups continue from the restored PIT on a new
    /// branch, so the appliances should keep it until they have the branch.
    pub fn continues(&self, now_ms: i64) -> bool {
        self.stands()
            && matches!(
                self.mode,
                crate::restore::Mode::IntoExisting
                    | crate::restore::Mode::Rollback
                    | crate::restore::Mode::Promotion
                    | crate::restore::Mode::Recovery
            )
            && self
                .finished_at_ms
                .is_some_and(|t| now_ms - t < BRANCH_ROOT_KEEP_MS)
    }

    /// Whether a volume is still a clone of the PIT's snapshot on the
    /// cluster (method 1), so the snapshot must stay.
    pub fn holds_cluster_snapshot(&self) -> bool {
        self.ended_at_ms.is_none() && self.methods.values().any(|m| *m == 1)
    }
}

impl PitRecord {
    /// Whether any appliance has published it.
    pub fn backed_up(&self) -> bool {
        self.offers.values().any(|o| *o == Offer::Published)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_round_trip_and_refuse_unknown_keys() {
        let attempt = Attempt::Failed {
            at_ms: 5,
            message: "x".into(),
        };
        let text = serde_json::to_string(&attempt).unwrap();
        assert_eq!(text, r#"{"outcome":"failed","at_ms":5,"message":"x"}"#);
        assert_eq!(serde_json::from_str::<Attempt>(&text).unwrap(), attempt);
        let offer: Offer = serde_json::from_str(r#"{"state":"open","offered_s":7}"#).unwrap();
        assert_eq!(offer, Offer::Open { offered_s: 7 });
        let lineage = r#"{"schema_version":1,"record_type":"backup_lineage","vm_uuid":"00000000-0000-0000-0000-000000000001","branch_uuid":"00000000-0000-0000-0000-000000000002","head":null,"extra":1}"#;
        assert!(serde_json::from_str::<Lineage>(lineage).is_err());
        assert_eq!(
            Policy::schedule_id_for(&Uuid::from_u128(1)),
            "backup-00000000-0000-0000-0000-000000000001"
        );
    }
}
