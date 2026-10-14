// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

//! The trusted-peer list and the refusals (`BACKUP.md`, "Trusted peers"),
//! in the metadata dataset. Only this appliance's administrator changes
//! the list; an entry is the peer's UUID, the fingerprint typed from the
//! peer's own UI, and its address, and nothing is learned on first use.

use std::io;

use bmac_core::ApplianceAddress;
use bmac_proto::appliance::{TrustedPeer, normalized_fingerprint, trusted_peer};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::fsx;
use crate::journal::Metadata;

/// Refusals kept for the Trusted peers page.
const MAX_REFUSALS: usize = 100;
/// A peer's refusals reach the audit log at most this often.
pub const REFUSAL_AUDIT_EVERY_MS: i64 = 3600 * 1000;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PeerEntry {
    pub peer: TrustedPeer,
    pub added_at_ms: i64,
    pub by: String,
    /// When it last read from this appliance.
    #[serde(default)]
    pub last_served_ms: Option<i64>,
    /// Whether it accepted this appliance's last handshake: whether it
    /// trusts this appliance back, as far as this appliance can tell.
    #[serde(default)]
    pub trusts_us: Option<bool>,
}

/// A handshake refused because the certificate wasn't trusted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Refusal {
    pub fingerprint: String,
    /// The name its certificate claimed, which proves nothing.
    pub claimed: String,
    pub from: String,
    pub at_ms: i64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Peers {
    pub peers: Vec<PeerEntry>,
    pub refusals: Vec<Refusal>,
}

impl Peers {
    fn path(metadata: &Metadata) -> std::path::PathBuf {
        metadata.root.join("peers.json")
    }

    pub fn load(metadata: &Metadata) -> io::Result<Peers> {
        Ok(fsx::read_json(&Peers::path(metadata))?.unwrap_or_default())
    }

    pub fn save(&self, metadata: &Metadata) -> io::Result<()> {
        fsx::write_json(&Peers::path(metadata), self, 0o600)
    }

    pub fn list(&self) -> Vec<TrustedPeer> {
        self.peers.iter().map(|e| e.peer.clone()).collect()
    }

    pub fn trusted(&self, fingerprint: &str) -> Option<&TrustedPeer> {
        let list = self.list();
        let uuid = trusted_peer(&list, fingerprint)?.appliance_uuid.clone();
        self.peers
            .iter()
            .map(|e| &e.peer)
            .find(|p| p.appliance_uuid == uuid)
    }

    /// Adds a peer whose certificate was seen to match `fingerprint`.
    pub fn trust(
        &mut self,
        appliance_uuid: Uuid,
        fingerprint: &str,
        address: ApplianceAddress,
        own_fingerprint: &str,
        by: &str,
        now_ms: i64,
    ) -> Result<(), String> {
        let fingerprint =
            normalized_fingerprint(fingerprint).ok_or("a fingerprint is 64 hexadecimal digits")?;
        if Some(&fingerprint) == normalized_fingerprint(own_fingerprint).as_ref() {
            return Err("that is this appliance's own fingerprint".into());
        }
        if self.trusted(&fingerprint).is_some() {
            return Err("that peer is already trusted".into());
        }
        let uuid = appliance_uuid.to_string();
        if self.peers.iter().any(|e| e.peer.appliance_uuid == uuid) {
            return Err(format!(
                "appliance {uuid} is already trusted with another fingerprint; untrust it first"
            ));
        }
        self.peers.push(PeerEntry {
            peer: TrustedPeer {
                appliance_uuid: uuid,
                fingerprint,
                address,
            },
            added_at_ms: now_ms,
            by: by.to_string(),
            last_served_ms: None,
            trusts_us: None,
        });
        Ok(())
    }

    pub fn untrust(&mut self, appliance_uuid: &str) -> Option<PeerEntry> {
        let index = self
            .peers
            .iter()
            .position(|e| e.peer.appliance_uuid == appliance_uuid)?;
        Some(self.peers.remove(index))
    }

    /// Records a refusal; true if it should reach the audit log.
    pub fn refused(&mut self, refusal: Refusal) -> bool {
        let audit = !self.refusals.iter().any(|r| {
            r.fingerprint == refusal.fingerprint && refusal.at_ms - r.at_ms < REFUSAL_AUDIT_EVERY_MS
        });
        self.refusals.push(refusal);
        let excess = self.refusals.len().saturating_sub(MAX_REFUSALS);
        self.refusals.drain(..excess);
        audit
    }
}

/// What connecting to a would-be peer showed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Probe {
    /// It presented the certificate with the typed fingerprint.
    Matches,
    /// It presented another certificate: save nothing.
    Mismatch {
        presented: String,
    },
    Unreachable(String),
}

/// Connects to `address` and compares the certificate it presents with
/// `fingerprint`. Whether the peer then accepts this appliance doesn't
/// matter here: its certificate is seen first.
pub async fn probe(
    fingerprint: &str,
    address: &ApplianceAddress,
    identity: (&str, &str),
    port: u16,
) -> Probe {
    let client = match crate::client::ClusterClient::peer(fingerprint, identity, port) {
        Ok(client) => client,
        Err(e) => return Probe::Unreachable(e),
    };
    let answer = client
        .call_peer::<(), bmac_proto::appliance::PeerIndex>(
            address,
            "GET",
            bmac_proto::appliance::on_appliance::PEER_INDEX,
            None,
        )
        .await;
    let wanted = normalized_fingerprint(fingerprint).unwrap_or_default();
    match client.presented() {
        Some(seen) if seen == wanted => Probe::Matches,
        Some(seen) => Probe::Mismatch { presented: seen },
        None => Probe::Unreachable(match answer {
            Err(e) => e.to_string(),
            Ok(_) => "no certificate was presented".into(),
        }),
    }
}

/// Said after every successful trust, as `BACKUP.md` asks.
pub const TRUST_REMINDER: &str = "The match proves something only if the fingerprint came from the peer's own UI or its administrator, not from a message that could have been altered.";

#[cfg(test)]
mod tests {
    use super::*;

    fn fp(c: char) -> String {
        c.to_string().repeat(64)
    }

    #[test]
    fn trust_needs_a_real_fingerprint_once_per_appliance_and_never_our_own() {
        let mut peers = Peers::default();
        let addr: ApplianceAddress = "vault2.example.org".parse().unwrap();
        let a = Uuid::from_u128(1);
        assert!(
            peers
                .trust(a, "xyz", addr.clone(), &fp('0'), "admin", 1)
                .is_err()
        );
        assert!(
            peers
                .trust(a, &fp('0'), addr.clone(), &fp('0'), "admin", 1)
                .is_err()
        );
        let colons = fp('a')
            .as_bytes()
            .chunks(2)
            .map(|c| std::str::from_utf8(c).unwrap().to_uppercase())
            .collect::<Vec<_>>()
            .join(":");
        peers
            .trust(a, &colons, addr.clone(), &fp('0'), "admin", 1)
            .unwrap();
        assert_eq!(peers.peers[0].peer.fingerprint, fp('a'));
        assert!(peers.trusted(&fp('a')).is_some());
        assert!(
            peers
                .trust(a, &fp('b'), addr.clone(), &fp('0'), "admin", 1)
                .is_err()
        );
        assert!(
            peers
                .trust(Uuid::from_u128(2), &fp('a'), addr, &fp('0'), "admin", 1)
                .is_err()
        );
        assert!(peers.untrust(&a.to_string()).is_some());
        assert!(peers.trusted(&fp('a')).is_none());
    }

    #[test]
    fn refusals_reach_the_audit_log_once_an_hour_per_peer() {
        let mut peers = Peers::default();
        let refusal = |f: char, at_ms| Refusal {
            fingerprint: fp(f),
            claimed: "bmac-appliance x".into(),
            from: "192.0.2.1:5000".into(),
            at_ms,
        };
        assert!(peers.refused(refusal('a', 0)));
        assert!(!peers.refused(refusal('a', 1000)));
        assert!(peers.refused(refusal('b', 1000)));
        assert!(!peers.refused(refusal('a', REFUSAL_AUDIT_EVERY_MS)));
        assert!(peers.refused(refusal('a', 2 * REFUSAL_AUDIT_EVERY_MS)));
        for n in 0..200 {
            peers.refused(refusal('c', n));
        }
        assert_eq!(peers.refusals.len(), MAX_REFUSALS);
    }
}
