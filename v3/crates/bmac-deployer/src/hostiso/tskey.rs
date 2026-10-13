// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

//! The single-use Tailscale auth key embedded in a host's ISO. A key that
//! was in an earlier image, even a half-built one, may be in someone's
//! hands, so a key whose digest the cluster has seen is refused, as v2's
//! `reserve_fresh_tailscale_auth_key` does. The digests live in the cluster
//! settings and reach the Deployer through the join bundle; a first host
//! starts the list with its own key, so the Deployer keeps no state.

use sha2::{Digest, Sha256};

/// Lab builds accept a reused key with a warning: the lab's keys are
/// reusable, and the lab removes a VM's old device before it joins again.
pub const ACCEPT_REUSED_KEYS: bool = cfg!(feature = "lab");

/// The digest kept in place of the key: lowercase hex SHA-256.
pub fn key_digest(key: &str) -> String {
    Sha256::digest(key.trim().as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// The key as entered, checked.
#[derive(Clone, PartialEq, Eq)]
pub struct CheckedKey {
    pub key: String,
    pub digest: String,
    /// Set when the digest was seen before and reuse was accepted.
    pub reused: bool,
}

impl std::fmt::Debug for CheckedKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CheckedKey")
            .field("digest", &self.digest)
            .field("reused", &self.reused)
            .finish_non_exhaustive()
    }
}

pub fn check_key(key: &str, used: &[String], accept_reused: bool) -> Result<CheckedKey, String> {
    let key = key.trim();
    let well_formed = key.starts_with("tskey-auth-")
        && key.len() <= 200
        && key
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
    if !well_formed {
        return Err(
            "That isn't a Tailscale auth key. Auth keys start with tskey-auth-; create one in \
             the Tailscale admin console under Settings, Keys."
                .into(),
        );
    }
    let digest = key_digest(key);
    let reused = used.iter().any(|seen| seen.eq_ignore_ascii_case(&digest));
    if reused && !accept_reused {
        return Err(
            "This Tailscale auth key was already used for a BMAC host image. Create a new \
             single-use key."
                .into(),
        );
    }
    Ok(CheckedKey {
        key: key.to_string(),
        digest,
        reused,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY: &str = "tskey-auth-kAbCdEf1CNTRL-0123456789abcdef";

    #[test]
    fn a_fresh_key_passes_and_is_never_shown() {
        let checked = check_key(&format!(" {KEY}\n"), &[], false).unwrap();
        assert_eq!(checked.key, KEY);
        assert_eq!(checked.digest, key_digest(KEY));
        assert_eq!(checked.digest.len(), 64);
        assert!(!checked.reused);
        assert!(!format!("{checked:?}").contains("tskey"));
    }

    #[test]
    fn a_key_seen_before_is_refused() {
        let used = vec!["0".repeat(64), key_digest(KEY).to_uppercase()];
        let error = check_key(KEY, &used, false).unwrap_err();
        assert!(error.contains("already used"), "{error}");
    }

    #[test]
    fn the_lab_accepts_a_reused_key_and_says_so() {
        let used = vec![key_digest(KEY)];
        let checked = check_key(KEY, &used, true).unwrap();
        assert!(checked.reused);
    }

    #[test]
    fn other_text_is_not_a_key() {
        for bad in ["", "tskey-api-abc", "tskey-auth-has space", "hunter2"] {
            assert!(check_key(bad, &[], false).is_err(), "{bad}");
        }
    }
}
