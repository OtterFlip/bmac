// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

//! How much raw space a block takes on each vdev type (`STORAGE.md`,
//! "Capacity calculations"). Every figure is an exact ratio of integers, so
//! comparisons such as "inflation at most 1.05" never depend on rounding.

use std::cmp::Ordering;
use std::fmt;

use serde::{Deserialize, Serialize};

use crate::layout::{PoolLayout, Vdev, VdevClass, VdevType};

/// A non-negative fraction `num / den` with `den > 0`.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct Ratio {
    pub num: u64,
    pub den: u64,
}

impl Ratio {
    pub const ONE: Ratio = Ratio { num: 1, den: 1 };

    pub fn new(num: u64, den: u64) -> Ratio {
        assert!(den > 0, "ratio with a zero denominator");
        let g = gcd(num, den).max(1);
        Ratio {
            num: num / g,
            den: den / g,
        }
    }

    pub fn as_f64(self) -> f64 {
        self.num as f64 / self.den as f64
    }

    /// `value × self`, rounded up.
    pub fn mul_ceil(self, value: u64) -> u64 {
        let n = u128::from(value) * u128::from(self.num);
        u64::try_from(n.div_ceil(u128::from(self.den))).unwrap_or(u64::MAX)
    }

    /// `value × self`, rounded down.
    pub fn mul_floor(self, value: u64) -> u64 {
        let n = u128::from(value) * u128::from(self.num) / u128::from(self.den);
        u64::try_from(n).unwrap_or(u64::MAX)
    }

    /// `value ÷ self`, rounded down; zero when the ratio is zero.
    pub fn div_floor(self, value: u64) -> u64 {
        if self.num == 0 {
            return 0;
        }
        let n = u128::from(value) * u128::from(self.den) / u128::from(self.num);
        u64::try_from(n).unwrap_or(u64::MAX)
    }

    pub fn times(self, other: Ratio) -> Ratio {
        Ratio::wide(
            u128::from(self.num) * u128::from(other.num),
            u128::from(self.den) * u128::from(other.den),
        )
    }

    pub fn over(self, other: Ratio) -> Ratio {
        Ratio::wide(
            u128::from(self.num) * u128::from(other.den),
            u128::from(self.den) * u128::from(other.num),
        )
    }

    fn wide(num: u128, den: u128) -> Ratio {
        assert!(den > 0, "ratio with a zero denominator");
        let g = gcd(num, den).max(1);
        let narrow = |v: u128| u64::try_from(v / g).expect("ratio too large after reduction");
        Ratio {
            num: narrow(num),
            den: narrow(den),
        }
    }
}

fn gcd<T: Copy + PartialEq + Default + std::ops::Rem<Output = T>>(mut a: T, mut b: T) -> T {
    while b != T::default() {
        (a, b) = (b, a % b);
    }
    a
}

impl PartialEq for Ratio {
    fn eq(&self, other: &Ratio) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}

impl Eq for Ratio {}

impl PartialOrd for Ratio {
    fn partial_cmp(&self, other: &Ratio) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Ratio {
    fn cmp(&self, other: &Ratio) -> Ordering {
        (u128::from(self.num) * u128::from(other.den))
            .cmp(&(u128::from(other.num) * u128::from(self.den)))
    }
}

impl fmt::Display for Ratio {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:.2}", self.as_f64())
    }
}

/// A zvol block size (`volblocksize`): a power of two from 512 bytes to
/// 128 KiB.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "u32", into = "u32")]
pub struct BlockSize(u32);

impl BlockSize {
    pub const K16: BlockSize = BlockSize(16 * 1024);
    pub const K32: BlockSize = BlockSize(32 * 1024);
    pub const K64: BlockSize = BlockSize(64 * 1024);
    pub const K128: BlockSize = BlockSize(128 * 1024);
    /// The sizes BMAC offers by default, smallest first.
    pub const OFFERED: [BlockSize; 4] = [Self::K16, Self::K32, Self::K64, Self::K128];
    /// ZFS reports raidz space in units of this block size.
    pub const REPORTING: BlockSize = Self::K128;

    pub fn new(bytes: u32) -> Option<BlockSize> {
        (bytes.is_power_of_two() && (512..=128 * 1024).contains(&bytes)).then_some(BlockSize(bytes))
    }

    pub fn bytes(self) -> u64 {
        u64::from(self.0)
    }

    /// The Proxmox storage BMAC defines for this block size.
    pub fn storage(self) -> String {
        match self {
            Self::K16 => "local-zfs".to_string(),
            other => format!("bmac-zfs-{}k", other.0 / 1024),
        }
    }

    /// The dataset under the pool that backs [`Self::storage`].
    pub fn dataset_suffix(self) -> String {
        match self {
            Self::K16 => "data".to_string(),
            other => format!("data-{}k", other.0 / 1024),
        }
    }

    /// `16K`, as the `VM_BLOCK_SIZES` setting writes it.
    pub fn short(self) -> String {
        format!("{}K", self.0 / 1024)
    }

    /// The `VM_BLOCK_SIZES` setting: some of [`Self::OFFERED`], such as
    /// `16K,64K`, each once.
    pub fn parse_offered(text: &str) -> Result<Vec<BlockSize>, String> {
        let mut sizes = Vec::new();
        for word in text.split(',') {
            let size = Self::OFFERED
                .into_iter()
                .find(|b| b.short() == word.trim())
                .ok_or_else(|| format!("{word} is not one of 16K, 32K, 64K, and 128K"))?;
            if sizes.contains(&size) {
                return Err(format!("{word} is listed twice"));
            }
            sizes.push(size);
        }
        sizes.sort();
        Ok(sizes)
    }

    /// `m(b) = 128 / b`: one 128-byte block pointer per data block. Its
    /// second copy is offset by the compression of indirect blocks
    /// (measured in the lab, `tests/capacity_lab.rs`).
    pub fn metadata_allowance(self) -> Ratio {
        Ratio::new(128, self.bytes())
    }
}

impl TryFrom<u32> for BlockSize {
    type Error = String;
    fn try_from(bytes: u32) -> Result<BlockSize, String> {
        BlockSize::new(bytes)
            .ok_or_else(|| format!("{bytes} is not a block size from 512 bytes to 128 KiB"))
    }
}

impl From<BlockSize> for u32 {
    fn from(size: BlockSize) -> u32 {
        size.0
    }
}

impl fmt::Display for BlockSize {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.0 >= 1024 {
            write!(f, "{} KiB", self.0 / 1024)
        } else {
            write!(f, "{} bytes", self.0)
        }
    }
}

/// The allocation shape of one top-level vdev.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Geometry {
    pub kind: VdevType,
    pub width: u32,
    pub ashift: u8,
}

impl Geometry {
    pub fn new(kind: VdevType, width: u32, ashift: u8) -> Geometry {
        assert!(
            width > kind.parity().unwrap_or(0),
            "a vdev needs more disks than parity"
        );
        Geometry {
            kind,
            width,
            ashift,
        }
    }

    pub fn of(vdev: &Vdev, ashift: u8) -> Geometry {
        Geometry::new(vdev.kind, vdev.members.len() as u32, ashift)
    }

    pub fn sector(self) -> u64 {
        1 << self.ashift
    }

    /// Sectors a `bytes`-byte block takes, as in `vdev_raidz_asize()`.
    pub fn allocated_sectors(self, bytes: u64) -> u64 {
        let d = bytes.div_ceil(self.sector()).max(1);
        match self.kind {
            VdevType::Single => d,
            VdevType::Mirror => d * u64::from(self.width),
            kind => {
                let p = u64::from(kind.parity().unwrap_or(0));
                let parity = p * d.div_ceil(u64::from(self.width) - p);
                (d + parity).div_ceil(p + 1) * (p + 1)
            }
        }
    }

    /// `E(B)`: data sectors over allocated sectors.
    pub fn efficiency(self, bytes: u64) -> Ratio {
        let d = bytes.div_ceil(self.sector()).max(1);
        Ratio::new(d, self.allocated_sectors(bytes))
    }

    /// The best efficiency this geometry can reach: `(N − p) / N` for
    /// raidz.
    pub fn ideal_efficiency(self) -> Ratio {
        match self.kind {
            VdevType::Single => Ratio::ONE,
            VdevType::Mirror => Ratio::new(1, u64::from(self.width)),
            kind => {
                let n = u64::from(self.width);
                Ratio::new(n - u64::from(kind.parity().unwrap_or(0)), n)
            }
        }
    }

    /// `I(b) = E(128 KiB) / E(b)`.
    pub fn inflation(self, block: BlockSize) -> Ratio {
        self.efficiency(BlockSize::REPORTING.bytes())
            .over(self.efficiency(block.bytes()))
    }

    /// What ZFS reports as this vdev's size: raw space scaled by
    /// `E(128 KiB)`, from the smallest member's usable bytes.
    pub fn reported_bytes(self, smallest_member: u64) -> u64 {
        match self.kind {
            VdevType::Single | VdevType::Mirror => smallest_member,
            _ => self
                .efficiency(BlockSize::REPORTING.bytes())
                .mul_floor(smallest_member * u64::from(self.width)),
        }
    }

    /// The nominal size: data disks times the smallest member.
    pub fn nominal_bytes(self, smallest_member: u64) -> u64 {
        match self.kind {
            VdevType::Single | VdevType::Mirror => smallest_member,
            kind => smallest_member * u64::from(self.width - kind.parity().unwrap_or(0)),
        }
    }

    /// How much VM data at block size `block` this vdev holds.
    pub fn vm_bytes(self, smallest_member: u64, block: BlockSize) -> u64 {
        self.inflation(block)
            .div_floor(self.reported_bytes(smallest_member))
    }
}

/// The data vdevs of one pool, which is all a capacity question needs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PoolGeometry {
    pub data: Vec<Geometry>,
}

/// The suggested block size accepts at most this much inflation.
pub const SUGGESTED_MAX_INFLATION: Ratio = Ratio { num: 21, den: 20 };

impl PoolGeometry {
    pub fn of(layout: &PoolLayout) -> PoolGeometry {
        PoolGeometry {
            data: layout
                .vdevs
                .iter()
                .filter(|v| v.class == VdevClass::Data)
                .map(|v| Geometry::of(v, layout.ashift))
                .collect(),
        }
    }

    /// The largest inflation among the data vdevs: ZFS spreads writes over
    /// all of them, so this is a safe upper bound.
    pub fn inflation(&self, block: BlockSize) -> Ratio {
        self.data
            .iter()
            .map(|g| g.inflation(block))
            .max()
            .unwrap_or(Ratio::ONE)
    }

    /// `I(b) × (1 + m(b))`: pool bytes charged per logical zvol byte.
    pub fn charge_per_byte(&self, block: BlockSize) -> Ratio {
        let m = block.metadata_allowance();
        self.inflation(block)
            .times(Ratio::new(m.den + m.num, m.den))
    }

    /// Space needed for a fully written, incompressible zvol of `volsize`
    /// bytes: `V × I(b) × (1 + m(b))`, rounded up.
    pub fn space_needed(&self, volsize: u64, block: BlockSize) -> u64 {
        self.charge_per_byte(block).mul_ceil(volsize)
    }
}

/// The smallest offered size whose inflation is at most 1.05 on every
/// placement host. When none qualifies, the size with the least worst-case
/// inflation.
pub fn suggested_block_size(hosts: &[PoolGeometry], offered: &[BlockSize]) -> Option<BlockSize> {
    let mut sizes = offered.to_vec();
    sizes.sort();
    let worst = |b: BlockSize| {
        hosts
            .iter()
            .map(|h| h.inflation(b))
            .max()
            .unwrap_or(Ratio::ONE)
    };
    sizes
        .iter()
        .copied()
        .find(|&b| worst(b) <= SUGGESTED_MAX_INFLATION)
        .or_else(|| sizes.iter().copied().min_by_key(|&b| worst(b)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn raidz(parity: u32, width: u32) -> Geometry {
        let kind = match parity {
            1 => VdevType::Raidz1,
            2 => VdevType::Raidz2,
            _ => VdevType::Raidz3,
        };
        Geometry::new(kind, width, 12)
    }

    #[test]
    fn raidz_allocation_matches_vdev_raidz_asize() {
        // raidz1 of 4, 16 KiB: 4 data, 2 parity, padded to 6 sectors.
        assert_eq!(raidz(1, 4).allocated_sectors(16 * 1024), 6);
        // raidz2 of 8, 128 KiB: 32 data, 2 × 6 parity, padded to 45.
        assert_eq!(raidz(2, 8).allocated_sectors(128 * 1024), 45);
        assert_eq!(
            Geometry::new(VdevType::Mirror, 3, 12).efficiency(4096),
            Ratio::new(1, 3)
        );
    }

    #[test]
    fn ratio_arithmetic_is_exact() {
        let r = Ratio::new(6, 4);
        assert_eq!((r.num, r.den), (3, 2));
        assert_eq!(r.mul_ceil(3), 5);
        assert_eq!(r.mul_floor(3), 4);
        assert_eq!(r.div_floor(10), 6);
        assert!(Ratio::new(21, 20) > Ratio::new(104, 100));
        assert_eq!(Ratio::new(0, 5).div_floor(10), 0);
    }

    #[test]
    fn block_sizes_and_storages() {
        assert_eq!(BlockSize::new(3000), None);
        assert_eq!(BlockSize::new(256 * 1024), None);
        let names: Vec<String> = BlockSize::OFFERED.iter().map(|b| b.storage()).collect();
        assert_eq!(
            names,
            ["local-zfs", "bmac-zfs-32k", "bmac-zfs-64k", "bmac-zfs-128k"]
        );
        let datasets: Vec<String> = BlockSize::OFFERED
            .iter()
            .map(|b| b.dataset_suffix())
            .collect();
        assert_eq!(datasets, ["data", "data-32k", "data-64k", "data-128k"]);
        assert_eq!(BlockSize::K16.to_string(), "16 KiB");
    }

    #[test]
    fn the_offered_sizes_setting_lists_some_of_the_four() {
        assert_eq!(
            BlockSize::parse_offered("64K, 16K"),
            Ok(vec![BlockSize::K16, BlockSize::K64])
        );
        assert_eq!(
            BlockSize::parse_offered("16K,32K,64K,128K"),
            Ok(BlockSize::OFFERED.to_vec())
        );
        for (text, why) in [
            ("", "not one of"),
            ("8K", "not one of"),
            ("16k", "not one of"),
            ("16K,16K", "twice"),
        ] {
            let error = BlockSize::parse_offered(text).unwrap_err();
            assert!(error.contains(why), "{text}: {error}");
        }
    }

    #[test]
    fn storage_md_summary_example() {
        // "6 × 4 TB raidz1: nominal 20 TB (5 of 6 disks), ZFS will report
        // about 19.2 TB, VM disks at 16 KiB: 16 TB ... 32 KiB would give
        // 19.2 TB."
        let tb = 1_000_000_000_000;
        let g = raidz(1, 6);
        assert_eq!(g.nominal_bytes(4 * tb), 20 * tb);
        assert_eq!(g.reported_bytes(4 * tb), 19_200_000_000_000);
        assert_eq!(g.vm_bytes(4 * tb, BlockSize::K16), 16 * tb);
        assert_eq!(g.vm_bytes(4 * tb, BlockSize::K32), 19_200_000_000_000);
    }

    #[test]
    fn mixed_pools_use_the_worst_data_vdev() {
        let pool = PoolGeometry {
            data: vec![Geometry::new(VdevType::Mirror, 2, 12), raidz(1, 5)],
        };
        assert_eq!(pool.inflation(BlockSize::K16), Ratio::new(6, 5));
        // 1 GiB at 16 KiB: × 1.2 × (1 + 128/16384).
        let gib = 1 << 30;
        assert_eq!(
            pool.space_needed(gib, BlockSize::K16),
            (gib * 6 * 16512).div_ceil(5 * 16384)
        );
    }

    #[test]
    fn suggested_block_size_follows_storage_md() {
        let mirror = PoolGeometry {
            data: vec![Geometry::new(VdevType::Mirror, 2, 12)],
        };
        let rz1_5 = PoolGeometry {
            data: vec![raidz(1, 5)],
        };
        assert_eq!(
            suggested_block_size(std::slice::from_ref(&mirror), &BlockSize::OFFERED),
            Some(BlockSize::K16)
        );
        assert_eq!(
            suggested_block_size(std::slice::from_ref(&rz1_5), &BlockSize::OFFERED),
            Some(BlockSize::K32)
        );
        let rz2_10 = PoolGeometry {
            data: vec![raidz(2, 10)],
        };
        assert_eq!(
            suggested_block_size(&[mirror, rz1_5, rz2_10], &BlockSize::OFFERED),
            Some(BlockSize::K64)
        );
        // raidz2 of 8 is at 1.07 below 128 KiB.
        let rz2_8 = PoolGeometry {
            data: vec![raidz(2, 8)],
        };
        assert_eq!(
            suggested_block_size(std::slice::from_ref(&rz2_8), &BlockSize::OFFERED),
            Some(BlockSize::K128)
        );
        // Without 128 KiB nothing qualifies; ties go to the smaller size.
        let offered = [BlockSize::K32, BlockSize::K16, BlockSize::K64];
        assert_eq!(
            suggested_block_size(&[rz2_8], &offered),
            Some(BlockSize::K16)
        );
        assert_eq!(suggested_block_size(&[], &[]), None);
    }
}
