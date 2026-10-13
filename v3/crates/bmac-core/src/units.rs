// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

//! Byte counts.

use std::fmt;
use std::ops::{Add, Sub};

use serde::{Deserialize, Serialize};

/// A number of bytes. Arithmetic saturates rather than wrapping, so an
/// impossible size shows up as a refusal instead of a tiny number.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default, Serialize, Deserialize,
)]
#[serde(transparent)]
pub struct Bytes(pub u64);

impl Bytes {
    pub const KIB: u64 = 1024;
    pub const MIB: u64 = 1024 * 1024;
    pub const GIB: u64 = 1024 * 1024 * 1024;
    pub const TIB: u64 = 1024 * 1024 * 1024 * 1024;

    pub const fn from_gib(gib: u64) -> Bytes {
        Bytes(gib.saturating_mul(Self::GIB))
    }

    pub const fn from_mib(mib: u64) -> Bytes {
        Bytes(mib.saturating_mul(Self::MIB))
    }

    pub const fn get(self) -> u64 {
        self.0
    }

    /// Whole GiB, rounded down.
    pub const fn whole_gib(self) -> u64 {
        self.0 / Self::GIB
    }

    pub fn as_gib(self) -> f64 {
        self.0 as f64 / Self::GIB as f64
    }

    pub const fn saturating_sub(self, other: Bytes) -> Bytes {
        Bytes(self.0.saturating_sub(other.0))
    }
}

impl Add for Bytes {
    type Output = Bytes;
    fn add(self, other: Bytes) -> Bytes {
        Bytes(self.0.saturating_add(other.0))
    }
}

impl Sub for Bytes {
    type Output = Bytes;
    fn sub(self, other: Bytes) -> Bytes {
        self.saturating_sub(other)
    }
}

impl std::iter::Sum for Bytes {
    fn sum<I: Iterator<Item = Bytes>>(iter: I) -> Bytes {
        iter.fold(Bytes(0), Add::add)
    }
}

/// Binary units with up to two decimals, the way the Dashboard shows sizes:
/// `512 B`, `1.5 KiB`, `931.51 GiB`.
impl fmt::Display for Bytes {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        const UNITS: [&str; 6] = ["KiB", "MiB", "GiB", "TiB", "PiB", "EiB"];
        if self.0 < Self::KIB {
            return write!(f, "{} B", self.0);
        }
        let mut value = self.0 as f64 / Self::KIB as f64;
        let mut unit = 0;
        while value >= 1024.0 && unit < UNITS.len() - 1 {
            value /= 1024.0;
            unit += 1;
        }
        let text = format!("{value:.2}");
        let text = text.trim_end_matches('0').trim_end_matches('.');
        write!(f, "{text} {}", UNITS[unit])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn displays_binary_units() {
        assert_eq!(Bytes(0).to_string(), "0 B");
        assert_eq!(Bytes(1023).to_string(), "1023 B");
        assert_eq!(Bytes(1536).to_string(), "1.5 KiB");
        assert_eq!(Bytes::from_gib(2).to_string(), "2 GiB");
        assert_eq!(Bytes(1_000_204_886_016).to_string(), "931.51 GiB");
        assert_eq!(Bytes(u64::MAX).to_string(), "16 EiB");
    }

    #[test]
    fn arithmetic_saturates() {
        assert_eq!(Bytes(5) - Bytes(7), Bytes(0));
        assert_eq!(Bytes(u64::MAX) + Bytes(1), Bytes(u64::MAX));
        assert_eq!(Bytes::from_gib(u64::MAX), Bytes(u64::MAX));
        assert_eq!([Bytes(1), Bytes(2)].into_iter().sum::<Bytes>(), Bytes(3));
        assert_eq!(Bytes(Bytes::GIB * 3 - 1).whole_gib(), 2);
    }

    #[test]
    fn serializes_as_a_number() {
        assert_eq!(serde_json::to_string(&Bytes(42)).unwrap(), "42");
        assert_eq!(serde_json::from_str::<Bytes>("42").unwrap(), Bytes(42));
    }
}
