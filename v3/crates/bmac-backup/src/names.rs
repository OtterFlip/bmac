// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

//! PIT snapshot names (`BACKUP.md`, "Identities"): `bmac-pit-<UTC
//! timestamp>-<6 hex>`, the same on every volume of a PIT, and on its
//! bookmarks. They never collide with staging snapshots or with Proxmox's
//! replication snapshots (`__replicate_…`), which BMAC never touches (v2
//! `cluster_registry.py` 423–445). Names are for people; every check uses
//! GUIDs.

use std::fmt;
use std::str::FromStr;

use jiff::Timestamp;
use serde::{Deserialize, Serialize};

pub const PREFIX: &str = "bmac-pit-";
/// Proxmox storage replication's snapshots; never created, read, or
/// destroyed by BMAC.
pub const REPLICATION_PREFIX: &str = "__replicate_";
const STAMP: &str = "%Y%m%dT%H%M%SZ";

/// A PIT's snapshot name, such as `bmac-pit-20261013T030000Z-a1b2c3`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct PitName {
    text: String,
    taken_s: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0:?} is not a PIT snapshot name (bmac-pit-YYYYMMDDTHHMMSSZ-xxxxxx)")]
pub struct NotAPitName(pub String);

impl PitName {
    /// The name for a PIT taken at `taken_s` (UTC epoch seconds), with six
    /// hex digits from `nonce`.
    pub fn new(taken_s: i64, nonce: u32) -> PitName {
        let stamp = Timestamp::from_second(taken_s)
            .expect("an epoch second in range")
            .strftime(STAMP)
            .to_string();
        PitName {
            text: format!("{PREFIX}{stamp}-{:06x}", nonce & 0xff_ffff),
            taken_s,
        }
    }

    /// A new name for now, with a random suffix.
    pub fn now() -> PitName {
        let nonce = uuid::Uuid::new_v4().as_u128() as u32;
        PitName::new(Timestamp::now().as_second(), nonce)
    }

    pub fn as_str(&self) -> &str {
        &self.text
    }

    /// When the name says the PIT was taken (UTC epoch seconds).
    pub fn taken_s(&self) -> i64 {
        self.taken_s
    }
}

impl FromStr for PitName {
    type Err = NotAPitName;

    fn from_str(text: &str) -> Result<PitName, NotAPitName> {
        let bad = || NotAPitName(text.to_string());
        let rest = text.strip_prefix(PREFIX).ok_or_else(bad)?;
        let (stamp, nonce) = rest.split_once('-').ok_or_else(bad)?;
        if nonce.len() != 6
            || !nonce
                .bytes()
                .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
        {
            return Err(bad());
        }
        if stamp.len() != 16 || !stamp.is_ascii() {
            return Err(bad());
        }
        let taken = jiff::fmt::strtime::parse(STAMP, stamp)
            .and_then(|t| t.to_datetime())
            .and_then(|t| t.to_zoned(jiff::tz::TimeZone::UTC))
            .map(|t| t.timestamp())
            .map_err(|_| bad())?;
        let name = PitName::new(
            taken.as_second(),
            u32::from_str_radix(nonce, 16).map_err(|_| bad())?,
        );
        if name.text != text {
            return Err(bad());
        }
        Ok(name)
    }
}

impl TryFrom<String> for PitName {
    type Error = NotAPitName;
    fn try_from(text: String) -> Result<PitName, NotAPitName> {
        text.parse()
    }
}

impl From<PitName> for String {
    fn from(name: PitName) -> String {
        name.text
    }
}

impl fmt::Display for PitName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.text)
    }
}

/// Whether a snapshot or bookmark name is BMAC's to manage as a PIT.
pub fn is_pit(name: &str) -> bool {
    name.parse::<PitName>().is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_round_trip_and_carry_their_time() {
        let name = PitName::new(1_791_860_400, 0xa1b2c3);
        assert_eq!(name.as_str(), "bmac-pit-20261013T030000Z-a1b2c3");
        let parsed: PitName = name.as_str().parse().unwrap();
        assert_eq!(parsed, name);
        assert_eq!(parsed.taken_s(), 1_791_860_400);
        assert_eq!(
            PitName::new(0, 0x1_000_005).as_str(),
            "bmac-pit-19700101T000000Z-000005"
        );
    }

    #[test]
    fn other_snapshots_are_never_pits() {
        for text in [
            "__replicate_100-0_1791860400__",
            "bmac-pit-20261013T030000Z-A1B2C3",
            "bmac-pit-20261013T030000Z-a1b2c",
            "bmac-pit-20261013T030000-a1b2c3",
            "bmac-pit-20261332T030000Z-a1b2c3",
            "bmac-pit-20261013T030000Z-a1b2c3x",
            "stage1prod3-20261013",
            "bmac-pit-",
        ] {
            assert!(!is_pit(text), "{text}");
        }
        assert!(PitName::now().as_str().starts_with(PREFIX));
    }
}
