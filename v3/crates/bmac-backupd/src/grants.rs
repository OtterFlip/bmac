// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

//! Restore grants (`BACKUP.md`, "Restore"): the appliance administrator's
//! approval for one cluster to read chosen PITs until an expiry. The token
//! is shown once; only its SHA-256 is kept, in the metadata dataset. A
//! live grant keeps its PITs from being pruned.
//!
//! A recovery grant (`BACKUP.md`, "Recovering BMAC after losing the whole
//! cluster") is for a cluster rebuilt on new hardware: it names one host's
//! certificate, which the handshake then lets through, and lets that host
//! read the namespace's recovery manifest and every PIT of it, and bind the
//! namespace to the new cluster.

use std::collections::{BTreeMap, BTreeSet};
use std::io;

use base64::Engine;
use bmac_backup::retention::KeepReason;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::fsx;
use crate::journal::{Journal, Metadata};
use crate::retention::PitRef;

pub const DEFAULT_GRANT_HOURS: u32 = 24;
pub const MAX_GRANT_HOURS: u32 = 168;
const TOKEN_PREFIX: &str = "bmac-grant-";
const HOUR_MS: i64 = 3600 * 1000;
/// Expired and revoked grants stay listed this long.
const KEEP_ENDED_MS: i64 = 30 * 24 * HOUR_MS;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Grant {
    pub grant_id: String,
    pub token_sha256: String,
    pub cluster_uuid: String,
    pub pits: Vec<Uuid>,
    pub created_at_ms: i64,
    pub expires_at_ms: i64,
    pub by: String,
    #[serde(default)]
    pub revoked: bool,
    #[serde(default)]
    pub last_used_ms: Option<i64>,
    #[serde(default)]
    pub kind: GrantKind,
    /// A recovery grant's host certificate, SHA-256, lowercase hex.
    #[serde(default)]
    pub host_fingerprint: Option<String>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GrantKind {
    #[default]
    Restore,
    Recovery,
}

impl Grant {
    pub fn live(&self, now_ms: i64) -> bool {
        !self.revoked && now_ms < self.expires_at_ms
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Grants {
    pub grants: Vec<Grant>,
}

fn sha256_hex(text: &str) -> String {
    Sha256::digest(text.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

pub(crate) fn random(bytes: usize) -> Result<Vec<u8>, String> {
    let mut buf = vec![0u8; bytes];
    rustls::crypto::ring::default_provider()
        .secure_random
        .fill(&mut buf)
        .map_err(|_| "no randomness".to_string())?;
    Ok(buf)
}

impl Grants {
    fn path(metadata: &Metadata) -> std::path::PathBuf {
        metadata.root.join("grants.json")
    }

    pub fn load(metadata: &Metadata) -> io::Result<Grants> {
        Ok(fsx::read_json(&Grants::path(metadata))?.unwrap_or_default())
    }

    pub fn save(&self, metadata: &Metadata) -> io::Result<()> {
        fsx::write_json(&Grants::path(metadata), self, 0o600)
    }

    /// A grant for published PITs of one cluster; the token, to show once.
    pub fn issue(
        &mut self,
        journal: &Journal,
        cluster: &str,
        pits: &[Uuid],
        hours: Option<u32>,
        by: &str,
        now_ms: i64,
    ) -> Result<(Grant, String), String> {
        let hours = hours.unwrap_or(DEFAULT_GRANT_HOURS);
        if hours == 0 || hours > MAX_GRANT_HOURS {
            return Err(format!("a grant lasts 1 to {MAX_GRANT_HOURS} hours"));
        }
        if pits.is_empty() {
            return Err("a grant names at least one PIT".into());
        }
        for pit in pits {
            if journal.pit(cluster, pit).is_none() {
                return Err(format!(
                    "PIT {pit} of cluster {cluster} isn't published here"
                ));
            }
        }
        let token = format!(
            "{TOKEN_PREFIX}{}",
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(random(32)?)
        );
        let grant = Grant {
            grant_id: random(6)?.iter().map(|b| format!("{b:02x}")).collect(),
            token_sha256: sha256_hex(&token),
            cluster_uuid: cluster.to_string(),
            pits: pits
                .iter()
                .copied()
                .collect::<BTreeSet<_>>()
                .into_iter()
                .collect(),
            created_at_ms: now_ms,
            expires_at_ms: now_ms + i64::from(hours) * HOUR_MS,
            by: by.to_string(),
            revoked: false,
            last_used_ms: None,
            kind: GrantKind::Restore,
            host_fingerprint: None,
        };
        self.grants
            .retain(|g| g.live(now_ms) || now_ms - g.expires_at_ms < KEEP_ENDED_MS);
        self.grants.push(grant.clone());
        Ok((grant, token))
    }

    /// A recovery grant for `cluster`'s namespace, for the host whose
    /// certificate has `host_fingerprint`; the token, to show once.
    pub fn issue_recovery(
        &mut self,
        journal: &Journal,
        cluster: &str,
        host_fingerprint: &str,
        hours: Option<u32>,
        by: &str,
        now_ms: i64,
    ) -> Result<(Grant, String), String> {
        let hours = hours.unwrap_or(DEFAULT_GRANT_HOURS);
        if hours == 0 || hours > MAX_GRANT_HOURS {
            return Err(format!("a grant lasts 1 to {MAX_GRANT_HOURS} hours"));
        }
        let fingerprint = bmac_proto::appliance::normalized_fingerprint(host_fingerprint)
            .ok_or("the host's certificate fingerprint is not a SHA-256")?;
        if !journal.pits.iter().any(|p| p.cluster_uuid == cluster) {
            return Err(format!("no PIT of cluster {cluster} is published here"));
        }
        let token = format!(
            "{TOKEN_PREFIX}{}",
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(random(32)?)
        );
        let grant = Grant {
            grant_id: random(6)?.iter().map(|b| format!("{b:02x}")).collect(),
            token_sha256: sha256_hex(&token),
            cluster_uuid: cluster.to_string(),
            pits: Vec::new(),
            created_at_ms: now_ms,
            expires_at_ms: now_ms + i64::from(hours) * HOUR_MS,
            by: by.to_string(),
            revoked: false,
            last_used_ms: None,
            kind: GrantKind::Recovery,
            host_fingerprint: Some(fingerprint),
        };
        self.grants
            .retain(|g| g.live(now_ms) || now_ms - g.expires_at_ms < KEEP_ENDED_MS);
        self.grants.push(grant.clone());
        Ok((grant, token))
    }

    /// The live recovery grants: `(grant_id, cluster, host fingerprint)`.
    pub fn recovery_hosts(&self, now_ms: i64) -> Vec<(String, String, String)> {
        self.grants
            .iter()
            .filter(|g| g.kind == GrantKind::Recovery && g.live(now_ms))
            .filter_map(|g| {
                Some((
                    g.grant_id.clone(),
                    g.cluster_uuid.clone(),
                    g.host_fingerprint.clone()?,
                ))
            })
            .collect()
    }

    /// The live recovery grant `token` is, if it is `grant_id` for
    /// `cluster`.
    pub fn recovery(
        &self,
        token: &str,
        cluster: &str,
        grant_id: &str,
        now_ms: i64,
    ) -> Result<&Grant, String> {
        let grant = self.live_any(token, cluster, now_ms)?;
        if grant.kind != GrantKind::Recovery || grant.grant_id != grant_id {
            return Err(format!(
                "grant {} isn't this host's recovery grant",
                grant.grant_id
            ));
        }
        Ok(grant)
    }

    /// The live grant `token` is, if it lets `cluster` read `pit`.
    pub fn check(
        &self,
        token: &str,
        cluster: &str,
        pit: &Uuid,
        now_ms: i64,
    ) -> Result<&Grant, String> {
        let grant = self.live_for(token, cluster, now_ms)?;
        if !grant.pits.contains(pit) {
            return Err(format!(
                "restore grant {} doesn't name PIT {pit}",
                grant.grant_id
            ));
        }
        Ok(grant)
    }

    /// The live restore grant `token` is, if it is `cluster`'s.
    pub fn live_for(&self, token: &str, cluster: &str, now_ms: i64) -> Result<&Grant, String> {
        let grant = self.live_any(token, cluster, now_ms)?;
        if grant.kind != GrantKind::Restore {
            return Err(format!("grant {} is a recovery grant", grant.grant_id));
        }
        Ok(grant)
    }

    fn live_any(&self, token: &str, cluster: &str, now_ms: i64) -> Result<&Grant, String> {
        let hash = sha256_hex(token);
        let grant = self
            .grants
            .iter()
            .find(|g| g.token_sha256 == hash)
            .ok_or("that isn't a restore grant of this appliance")?;
        if !grant.live(now_ms) {
            return Err(format!("restore grant {} has ended", grant.grant_id));
        }
        if grant.cluster_uuid != cluster {
            return Err(format!(
                "restore grant {} is for another cluster",
                grant.grant_id
            ));
        }
        Ok(grant)
    }

    pub fn used(&mut self, grant_id: &str, now_ms: i64) {
        if let Some(g) = self.grants.iter_mut().find(|g| g.grant_id == grant_id) {
            g.last_used_ms = Some(now_ms);
        }
    }

    pub fn revoke(&mut self, grant_id: &str) -> bool {
        match self
            .grants
            .iter_mut()
            .find(|g| g.grant_id == grant_id && !g.revoked)
        {
            Some(g) => {
                g.revoked = true;
                true
            }
            None => false,
        }
    }

    /// What the live grants keep from retention.
    pub fn pins(&self, now_ms: i64) -> BTreeMap<PitRef, BTreeSet<KeepReason>> {
        let mut pins: BTreeMap<PitRef, BTreeSet<KeepReason>> = BTreeMap::new();
        for g in self.grants.iter().filter(|g| g.live(now_ms)) {
            for pit in &g.pits {
                pins.entry((g.cluster_uuid.clone(), *pit))
                    .or_default()
                    .insert(KeepReason::RestoreGrant);
            }
        }
        pins
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::journal::StoredPit;

    fn journal() -> Journal {
        let mut j = Journal::new(Uuid::from_u128(1), "g");
        j.pits.push(StoredPit {
            cluster_uuid: "c1".into(),
            vm_uuid: Uuid::from_u128(7),
            branch_uuid: Uuid::from_u128(0xb),
            pit_uuid: Uuid::from_u128(10),
            parent: None,
            name: "bmac-pit-x".into(),
            taken_s: 0,
            published_at_ms: 0,
            volumes: vec![],
            bytes: 0,
            manifest_sha256: String::new(),
            received_from: None,
        });
        j
    }

    #[test]
    fn a_grant_lets_one_cluster_read_its_pits_until_it_ends() {
        let mut grants = Grants::default();
        let pit = Uuid::from_u128(10);
        assert!(
            grants
                .issue(&journal(), "c1", &[Uuid::from_u128(11)], None, "admin", 0)
                .is_err()
        );
        assert!(
            grants
                .issue(&journal(), "c2", &[pit], None, "admin", 0)
                .is_err()
        );
        assert!(
            grants
                .issue(&journal(), "c1", &[pit], Some(169), "admin", 0)
                .is_err()
        );
        let (grant, token) = grants
            .issue(&journal(), "c1", &[pit], None, "admin", 0)
            .unwrap();
        assert!(token.starts_with(TOKEN_PREFIX));
        assert!(!serde_json::to_string(&grants).unwrap().contains(&token));
        assert_eq!(grant.expires_at_ms, 24 * HOUR_MS);
        assert!(grants.check(&token, "c1", &pit, 1).is_ok());
        assert!(
            grants
                .check(&token, "c2", &pit, 1)
                .unwrap_err()
                .contains("another cluster")
        );
        assert!(grants.check(&token, "c1", &Uuid::from_u128(11), 1).is_err());
        assert!(grants.check("bmac-grant-nope", "c1", &pit, 1).is_err());
        assert!(
            grants
                .check(&token, "c1", &pit, 24 * HOUR_MS)
                .unwrap_err()
                .contains("ended")
        );
        assert_eq!(grants.pins(1).len(), 1);
        assert!(grants.revoke(&grant.grant_id));
        assert!(grants.check(&token, "c1", &pit, 1).is_err());
        assert!(grants.pins(1).is_empty());
    }

    #[test]
    fn a_recovery_grant_names_one_host_and_reads_the_whole_namespace() {
        let mut grants = Grants::default();
        let host = "AB:".repeat(31) + "AB";
        assert!(
            grants
                .issue_recovery(&journal(), "c1", "ab", None, "admin", 0)
                .unwrap_err()
                .contains("SHA-256")
        );
        assert!(
            grants
                .issue_recovery(&journal(), "c2", &host, None, "admin", 0)
                .unwrap_err()
                .contains("no PIT of cluster c2")
        );
        let (grant, token) = grants
            .issue_recovery(&journal(), "c1", &host, Some(48), "admin", 0)
            .unwrap();
        assert_eq!(grant.kind, GrantKind::Recovery);
        assert_eq!(grant.host_fingerprint, Some("ab".repeat(32)));
        assert_eq!(
            grants.recovery_hosts(1),
            vec![(grant.grant_id.clone(), "c1".into(), "ab".repeat(32))]
        );
        assert!(grants.recovery(&token, "c1", &grant.grant_id, 1).is_ok());
        assert!(grants.recovery(&token, "c1", "other", 1).is_err());
        assert!(grants.recovery(&token, "c2", &grant.grant_id, 1).is_err());
        assert!(
            grants
                .check(&token, "c1", &Uuid::from_u128(10), 1)
                .unwrap_err()
                .contains("recovery grant"),
            "an enrolled cluster's host can't use it"
        );
        assert!(grants.pins(1).is_empty());
        grants.revoke(&grant.grant_id);
        assert!(grants.recovery_hosts(1).is_empty());
        assert!(grants.recovery(&token, "c1", &grant.grant_id, 1).is_err());
    }
}
