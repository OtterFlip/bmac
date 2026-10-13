// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

//! Version negotiation, the first exchange on every host-to-host and
//! appliance connection (`BACKUP.md`, "Protocol"). Each side states the
//! range it speaks; both use the highest version in common. Hosts run mixed
//! versions during a rolling upgrade, so the range is what keeps them
//! talking.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Hello {
    /// Which protocol: `bmac-peer`, `bmac-source`, or `bmac-appliance`.
    pub protocol: String,
    pub min_version: u32,
    pub max_version: u32,
    /// The software version, for messages only.
    pub bmac_version: String,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum NegotiationError {
    #[error("expected protocol {expected}, the peer speaks {found}")]
    WrongProtocol { expected: String, found: String },
    #[error(
        "no version in common: we speak {ours_min}–{ours_max}, the peer ({bmac_version}) speaks {theirs_min}–{theirs_max}"
    )]
    NoCommonVersion {
        ours_min: u32,
        ours_max: u32,
        theirs_min: u32,
        theirs_max: u32,
        bmac_version: String,
    },
}

impl Hello {
    /// The version both sides use.
    pub fn agree(&self, theirs: &Hello) -> Result<u32, NegotiationError> {
        if self.protocol != theirs.protocol {
            return Err(NegotiationError::WrongProtocol {
                expected: self.protocol.clone(),
                found: theirs.protocol.clone(),
            });
        }
        let low = self.min_version.max(theirs.min_version);
        let high = self.max_version.min(theirs.max_version);
        if low > high {
            return Err(NegotiationError::NoCommonVersion {
                ours_min: self.min_version,
                ours_max: self.max_version,
                theirs_min: theirs.min_version,
                theirs_max: theirs.max_version,
                bmac_version: theirs.bmac_version.clone(),
            });
        }
        Ok(high)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hello(protocol: &str, min: u32, max: u32) -> Hello {
        Hello {
            protocol: protocol.into(),
            min_version: min,
            max_version: max,
            bmac_version: "3.0.0".into(),
        }
    }

    #[test]
    fn the_highest_common_version_wins() {
        assert_eq!(
            hello("bmac-peer", 1, 3).agree(&hello("bmac-peer", 2, 5)),
            Ok(3)
        );
        assert_eq!(
            hello("bmac-peer", 1, 1).agree(&hello("bmac-peer", 1, 1)),
            Ok(1)
        );
        let error = hello("bmac-peer", 1, 2)
            .agree(&hello("bmac-peer", 3, 4))
            .unwrap_err();
        assert_eq!(
            error.to_string(),
            "no version in common: we speak 1–2, the peer (3.0.0) speaks 3–4"
        );
        assert!(matches!(
            hello("bmac-peer", 1, 1).agree(&hello("bmac-source", 1, 1)),
            Err(NegotiationError::WrongProtocol { .. })
        ));
    }
}
