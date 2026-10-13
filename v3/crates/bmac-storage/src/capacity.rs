// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

//! Capacity decisions (`STORAGE.md`, "Capacity calculations", rules 3 to 6):
//! the 10% reserve, disk growth limits, placement additions, and
//! refreservations for full zvols. Each uses the host's own geometry, so
//! raidz inflation is charged wherever it applies.

use bmac_core::{Bytes, HostSlot};
use serde::{Deserialize, Serialize};

use crate::geometry::{BlockSize, PoolGeometry, Ratio};

/// Every placement pool keeps this share of its size free.
pub const POOL_RESERVE_PERCENT: u64 = 10;

/// A pool's space as ZFS reports it for the pool's root dataset.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PoolSpace {
    pub used: u64,
    pub available: u64,
}

impl PoolSpace {
    pub fn total(self) -> u64 {
        self.used.saturating_add(self.available)
    }

    /// 10% of `used + available`, rounded up so it is never smaller than
    /// the exact share.
    pub fn reserve(self) -> u64 {
        u64::try_from((u128::from(self.total()) * u128::from(POOL_RESERVE_PERCENT)).div_ceil(100))
            .unwrap_or(u64::MAX)
    }

    /// `available` less the reserve; zero when the reserve is already used.
    pub fn headroom(self) -> u64 {
        self.available.saturating_sub(self.reserve())
    }
}

/// How a zvol takes pool space.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "allocation", rename_all = "snake_case")]
pub enum ZvolAllocation {
    /// Thin: space is taken as blocks are written. `guest_free` is what the
    /// guest has free inside the disk, which it can write without growth.
    Sparse { guest_free: u64 },
    /// Full: the zvol's `refreservation` holds its space in advance.
    Reserved { refreservation: u64 },
}

/// One placement host's pool, with the geometry that sets its inflation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostPool {
    pub host: HostSlot,
    pub space: PoolSpace,
    pub geometry: PoolGeometry,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CapacityError {
    #[error("at least one placement host is needed")]
    NoHosts,
    #[error("the disk's size must be more than zero")]
    EmptyVolume,
    #[error("{0} reported an empty pool")]
    EmptyPool(HostSlot),
    #[error(
        "the full zvol's refreservation ({refreservation}) is smaller than its size ({volsize})"
    )]
    ReservationBelowSize {
        volsize: Bytes,
        refreservation: Bytes,
    },
}

/// One host's part of a growth decision.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GrowthRow {
    pub host: HostSlot,
    pub total: u64,
    pub available: u64,
    pub reserve: u64,
    pub headroom: u64,
    /// Pool bytes charged per byte of growth.
    pub charge: Ratio,
    pub allowed: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Growth {
    pub hosts: Vec<GrowthRow>,
    /// The most the disk may grow by on every host, in whole MiB.
    pub allowed: u64,
}

/// The largest growth `ΔV` that fits on every placement host: `ΔV × I(b) ×
/// (1 + m(b))` must fit within the host's headroom. A full zvol is charged
/// at least its current `refreservation / volsize`, since ZFS's automatic
/// reservation can exceed that estimate. A sparse zvol's free guest space
/// can already be written, so it is taken off first.
pub fn growth_allowance(
    allocation: ZvolAllocation,
    volsize: u64,
    block: BlockSize,
    hosts: &[HostPool],
) -> Result<Growth, CapacityError> {
    if hosts.is_empty() {
        return Err(CapacityError::NoHosts);
    }
    if volsize == 0 {
        return Err(CapacityError::EmptyVolume);
    }
    if let ZvolAllocation::Reserved { refreservation } = allocation
        && refreservation < volsize
    {
        return Err(CapacityError::ReservationBelowSize {
            volsize: Bytes(volsize),
            refreservation: Bytes(refreservation),
        });
    }
    let mut rows = Vec::with_capacity(hosts.len());
    for pool in hosts {
        if pool.space.total() == 0 {
            return Err(CapacityError::EmptyPool(pool.host));
        }
        let headroom = pool.space.headroom();
        let mut charge = pool.geometry.charge_per_byte(block);
        let allowed = match allocation {
            ZvolAllocation::Reserved { refreservation } => {
                charge = charge.max(Ratio::new(refreservation, volsize));
                charge.div_floor(headroom)
            }
            ZvolAllocation::Sparse { guest_free } => {
                charge.div_floor(headroom).saturating_sub(guest_free)
            }
        };
        rows.push(GrowthRow {
            host: pool.host,
            total: pool.space.total(),
            available: pool.space.available,
            reserve: pool.space.reserve(),
            headroom,
            charge,
            allowed,
        });
    }
    let allowed = rows.iter().map(|r| r.allowed).min().unwrap_or(0) / Bytes::MIB * Bytes::MIB;
    Ok(Growth {
        hosts: rows,
        allowed,
    })
}

/// What a new copy of a zvol needs on a host before it is placed there.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Placement {
    pub available: u64,
    pub needed: u64,
    pub reserve: u64,
}

impl Placement {
    pub fn check(space: PoolSpace, needed: u64) -> Placement {
        Placement {
            available: space.available,
            needed,
            reserve: space.reserve(),
        }
    }

    pub fn fits(self) -> bool {
        self.available
            .checked_sub(self.needed)
            .is_some_and(|left| left >= self.reserve)
    }

    /// Bytes left above the reserve once the copy is placed.
    pub fn spare(self) -> Option<u64> {
        self.available
            .checked_sub(self.needed)?
            .checked_sub(self.reserve)
    }

    pub fn explain(self, host: HostSlot) -> String {
        format!(
            "{host}: {} available, the copy needs {}, and the {POOL_RESERVE_PERCENT}% reserve is {}",
            Bytes(self.available),
            Bytes(self.needed),
            Bytes(self.reserve)
        )
    }
}

/// The `refreservation` BMAC sets on a full zvol: `V × I(b) × (1 + m(b))`,
/// or ZFS's own automatic value when that is already larger.
pub fn reserved_refreservation(
    volsize: u64,
    block: BlockSize,
    geometry: &PoolGeometry,
    zfs_auto: u64,
) -> u64 {
    geometry.space_needed(volsize, block).max(zfs_auto)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geometry::Geometry;
    use crate::layout::VdevType;

    const GIB: u64 = Bytes::GIB;
    const MIB: u64 = Bytes::MIB;

    fn mirror() -> PoolGeometry {
        PoolGeometry {
            data: vec![Geometry::new(VdevType::Mirror, 2, 12)],
        }
    }

    fn host(slot: &str, total: u64, available: u64) -> HostPool {
        HostPool {
            host: slot.parse().unwrap(),
            space: PoolSpace {
                used: total - available,
                available,
            },
            geometry: mirror(),
        }
    }

    /// `I × (1 + m)` on a mirror at 16 KiB: 1 + 256/16384.
    const MIRROR_16K: Ratio = Ratio { num: 65, den: 64 };

    // The tests below are test_extend_prod_vm_disk.py's, with v2's flat 1.5%
    // sparse overhead replaced by the capacity function's charge.

    #[test]
    fn sparse_limit_uses_smallest_headroom_guest_free_and_charge() {
        let sparse = ZvolAllocation::Sparse {
            guest_free: 20 * GIB,
        };
        let hosts = [
            host("mox1", 1000 * GIB, 300 * GIB),
            host("mox2", 1000 * GIB, 250 * GIB),
        ];
        let growth = growth_allowance(sparse, 64 * GIB, BlockSize::K16, &hosts).unwrap();
        let headroom = 250 * GIB - 100 * GIB;
        assert_eq!(growth.hosts[1].headroom, headroom);
        let expected = (headroom * 64 / 65 - 20 * GIB) / MIB * MIB;
        assert_eq!(growth.allowed, expected);
        assert_eq!(growth.allowed % MIB, 0);
        let rows: Vec<_> = growth
            .hosts
            .iter()
            .map(|r| (r.total, r.available, r.reserve, r.headroom))
            .collect();
        assert_eq!(
            rows,
            [
                (1000 * GIB, 300 * GIB, 100 * GIB, 200 * GIB),
                (1000 * GIB, 250 * GIB, 100 * GIB, 150 * GIB)
            ]
        );
        assert_eq!(growth.hosts[0].charge, MIRROR_16K);
    }

    #[test]
    fn sparse_limit_is_zero_when_guest_free_covers_headroom() {
        let sparse = ZvolAllocation::Sparse {
            guest_free: 150 * GIB,
        };
        let hosts = [
            host("mox1", 1000 * GIB, 250 * GIB),
            host("mox2", 1000 * GIB, 300 * GIB),
        ];
        assert_eq!(
            growth_allowance(sparse, 64 * GIB, BlockSize::K16, &hosts)
                .unwrap()
                .allowed,
            0
        );
    }

    #[test]
    fn reserved_limit_scales_by_refreservation_ratio() {
        let (volsize, refreservation) = (64 * GIB, 66 * GIB + 12345);
        let reserved = ZvolAllocation::Reserved { refreservation };
        let hosts = [
            host("mox1", 1000 * GIB, 300 * GIB),
            host("mox3", 2000 * GIB, 260 * GIB),
        ];
        let growth = growth_allowance(reserved, volsize, BlockSize::K16, &hosts).unwrap();
        // mox3 keeps 200 GiB of its 2000 GiB pool, leaving the smaller 60 GiB.
        let headroom = 60 * GIB;
        assert_eq!(growth.hosts[1].headroom, headroom);
        // ZFS's ratio (1.031...) exceeds the function's 1.0156 here.
        let scaled = u128::from(headroom) * u128::from(volsize) / u128::from(refreservation);
        assert_eq!(growth.allowed, u64::try_from(scaled).unwrap() / MIB * MIB);
    }

    #[test]
    fn reserved_limit_uses_the_function_when_it_charges_more() {
        let reserved = ZvolAllocation::Reserved {
            refreservation: GIB,
        };
        let growth = growth_allowance(
            reserved,
            GIB,
            BlockSize::K16,
            &[host("mox1", 1000 * GIB, 200 * GIB)],
        )
        .unwrap();
        assert_eq!(growth.hosts[0].charge, MIRROR_16K);
        assert_eq!(growth.allowed, 100 * GIB * 64 / 65 / MIB * MIB);
    }

    #[test]
    fn reserve_is_ceiling_and_allowance_floors_to_mib() {
        // 10% of 1000000007 bytes rounds up to 100000001.
        let space = PoolSpace {
            used: 1_000_000_007 - (100_000_001 + 5 * MIB + 99),
            available: 100_000_001 + 5 * MIB + 99,
        };
        assert_eq!(space.reserve(), 100_000_001);
        assert_eq!(space.headroom(), 5 * MIB + 99);
        let pool = HostPool {
            host: "mox1".parse().unwrap(),
            space,
            geometry: mirror(),
        };
        let reserved = ZvolAllocation::Reserved {
            refreservation: GIB,
        };
        let growth = growth_allowance(reserved, GIB, BlockSize::K128, &[pool]).unwrap();
        // (5 MiB + 99) × 512/513 is just under 5 MiB.
        assert_eq!(growth.allowed, 4 * MIB);
    }

    #[test]
    fn negative_headroom_allows_nothing() {
        let sparse = ZvolAllocation::Sparse { guest_free: 0 };
        let hosts = [
            host("mox1", 1000 * GIB, 50 * GIB),
            host("mox2", 1000 * GIB, 500 * GIB),
        ];
        let growth = growth_allowance(sparse, GIB, BlockSize::K16, &hosts).unwrap();
        assert_eq!(growth.hosts[0].headroom, 0);
        assert_eq!(growth.allowed, 0);
    }

    #[test]
    fn invalid_allowance_inputs_fail_closed() {
        let reserved = ZvolAllocation::Reserved {
            refreservation: GIB - 1,
        };
        let hosts = [host("mox1", GIB, GIB)];
        assert!(matches!(
            growth_allowance(reserved, GIB, BlockSize::K16, &hosts),
            Err(CapacityError::ReservationBelowSize { .. })
        ));
        let sparse = ZvolAllocation::Sparse { guest_free: 0 };
        assert_eq!(
            growth_allowance(sparse, GIB, BlockSize::K16, &[]),
            Err(CapacityError::NoHosts)
        );
        assert_eq!(
            growth_allowance(sparse, 0, BlockSize::K16, &hosts),
            Err(CapacityError::EmptyVolume)
        );
        let empty = [host("mox2", 0, 0)];
        assert_eq!(
            growth_allowance(sparse, GIB, BlockSize::K16, &empty),
            Err(CapacityError::EmptyPool("mox2".parse().unwrap()))
        );
    }

    #[test]
    fn raidz_hosts_are_charged_their_own_inflation() {
        let mut raidz = host("mox2", 1000 * GIB, 300 * GIB);
        raidz.geometry = PoolGeometry {
            data: vec![Geometry::new(VdevType::Raidz1, 5, 12)],
        };
        let hosts = [host("mox1", 1000 * GIB, 300 * GIB), raidz];
        let sparse = ZvolAllocation::Sparse { guest_free: 0 };
        let at16 = growth_allowance(sparse, 64 * GIB, BlockSize::K16, &hosts).unwrap();
        // 1.2 × 65/64 on the raidz host.
        assert_eq!(at16.hosts[1].charge, Ratio::new(78, 64));
        assert_eq!(at16.allowed, 200 * GIB * 64 / 78 / MIB * MIB);
        let at32 = growth_allowance(sparse, 64 * GIB, BlockSize::K32, &hosts).unwrap();
        assert!(at32.allowed > at16.allowed);
    }

    #[test]
    fn placement_keeps_the_reserve() {
        let space = PoolSpace {
            used: 600 * GIB,
            available: 400 * GIB,
        };
        let fits = Placement::check(space, 300 * GIB);
        assert!(fits.fits());
        assert_eq!(fits.spare(), Some(0));
        let short = Placement::check(space, 300 * GIB + 1);
        assert!(!short.fits());
        assert_eq!(short.spare(), None);
        assert!(!Placement::check(space, 500 * GIB).fits());
        assert_eq!(
            short.explain("mox2".parse().unwrap()),
            format!(
                "mox2: {} available, the copy needs {}, and the 10% reserve is {}",
                Bytes(400 * GIB),
                Bytes(300 * GIB + 1),
                Bytes(100 * GIB)
            )
        );
    }

    #[test]
    fn refreservation_is_the_larger_of_bmac_and_zfs() {
        let raidz = PoolGeometry {
            data: vec![Geometry::new(VdevType::Raidz1, 5, 12)],
        };
        let needed = raidz.space_needed(64 * GIB, BlockSize::K16);
        assert_eq!(needed, (64 * GIB * 6 * 65).div_ceil(5 * 64));
        assert_eq!(
            reserved_refreservation(64 * GIB, BlockSize::K16, &raidz, 65 * GIB),
            needed
        );
        assert_eq!(
            reserved_refreservation(64 * GIB, BlockSize::K16, &raidz, 80 * GIB),
            80 * GIB
        );
    }
}
