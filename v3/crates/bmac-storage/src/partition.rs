// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

//! Partition sizes for pool members (`STORAGE.md`, "LUKS" and "Proxmox
//! hosts: the boot vdev"). The constants are v2's (`rpool_mirror.sh`,
//! `add_proxmox_host.sh`).

use bmac_core::Bytes;

const GIB: u64 = Bytes::GIB;

/// Room left past a non-boot member's partition, so a slightly smaller
/// replacement still fits.
pub const RESERVE_BYTES: u64 = GIB;
pub const FIRST_PARTITION_START_BYTES: u64 = Bytes::MIB;
/// Room after the last partition for the backup GPT, on 512-byte and 4Kn
/// disks.
pub const GPT_TAIL_BYTES: u64 = Bytes::MIB;
pub const MIN_DISK_BYTES: u64 = 8 * GIB;
/// The installer's `hdsize` leaves at least this much at the end of each boot
/// member for the LUKS header and size tolerance.
pub const HDSIZE_RESERVE_BYTES: u64 = 4 * GIB;
/// The installer's ESP on each boot member, ahead of the ZFS partition.
pub const ESP_BYTES: u64 = GIB;
/// The default LUKS2 header (metadata and keyslots) at the start of each
/// encrypted member.
pub const LUKS2_HEADER_BYTES: u64 = 16 * Bytes::MIB;
/// Members of one vdev, and a disk and its expected size, may differ by this
/// many percent.
pub const CAPACITY_TOLERANCE_PERCENT: u64 = 1;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PartitionError {
    #[error("no member disks were given")]
    NoDisks,
    #[error("a disk of {0} is smaller than the 8 GiB minimum")]
    TooSmall(Bytes),
    #[error("{0}-byte logical sectors are not supported")]
    Sector(u64),
    #[error("a boot disk of {0} leaves no room for an installer hdsize with a 4 GiB reserve")]
    NoHdsize(Bytes),
}

fn check_sector(sector: u64) -> Result<(), PartitionError> {
    if sector.is_power_of_two() && (512..=4096).contains(&sector) {
        Ok(())
    } else {
        Err(PartitionError::Sector(sector))
    }
}

/// Byte offset where the member partitions of a new non-boot vdev end: 1 GiB
/// short of the smallest member's whole-GiB size, in any order of disks.
pub fn member_end_bytes(disk_sizes: &[u64]) -> Result<u64, PartitionError> {
    let smallest = *disk_sizes.iter().min().ok_or(PartitionError::NoDisks)?;
    if smallest < MIN_DISK_BYTES {
        return Err(PartitionError::TooSmall(Bytes(smallest)));
    }
    Ok(smallest / GIB * GIB - RESERVE_BYTES)
}

/// The member partition as `(first sector, last sector)`, inclusive, as
/// `sgdisk` takes it.
pub fn member_partition(disk_sizes: &[u64], sector: u64) -> Result<(u64, u64), PartitionError> {
    check_sector(sector)?;
    let end = member_end_bytes(disk_sizes)?;
    Ok((FIRST_PARTITION_START_BYTES / sector, end / sector - 1))
}

/// The usable bytes of a member partition built by [`member_partition`].
pub fn member_partition_bytes(disk_sizes: &[u64]) -> Result<u64, PartitionError> {
    Ok(member_end_bytes(disk_sizes)? - FIRST_PARTITION_START_BYTES)
}

/// The fewest bytes a disk needs to take a copy of a partition table whose
/// last partition ends at `last_sector` (inclusive).
pub fn replacement_bytes(last_sector: u64, sector: u64) -> Result<u64, PartitionError> {
    check_sector(sector)?;
    Ok((last_sector + 1) * sector + GPT_TAIL_BYTES)
}

/// The installer's `hdsize` in GiB for the boot vdev: the smallest boot
/// member's whole GiB, less 4 GiB.
pub fn installer_hdsize_gib(boot_disk_sizes: &[u64]) -> Result<u64, PartitionError> {
    let smallest = *boot_disk_sizes
        .iter()
        .min()
        .ok_or(PartitionError::NoDisks)?;
    let hdsize = (smallest / GIB).saturating_sub(HDSIZE_RESERVE_BYTES / GIB);
    if hdsize == 0 || smallest - hdsize * GIB < HDSIZE_RESERVE_BYTES {
        return Err(PartitionError::NoHdsize(Bytes(smallest)));
    }
    Ok(hdsize)
}

/// About how many bytes a boot member's ZFS partition holds: the `hdsize`
/// less the ESP and the space before the first partition.
pub fn boot_partition_bytes(boot_disk_sizes: &[u64]) -> Result<u64, PartitionError> {
    let hdsize = installer_hdsize_gib(boot_disk_sizes)? * GIB;
    Ok(hdsize.saturating_sub(ESP_BYTES + FIRST_PARTITION_START_BYTES))
}

/// Whether two capacities differ by at most 1% of the larger.
pub fn within_tolerance(a: u64, b: u64) -> bool {
    let (smaller, larger) = if a <= b { (a, b) } else { (b, a) };
    u128::from(larger - smaller) * 100
        <= u128::from(larger) * u128::from(CAPACITY_TOLERANCE_PERCENT)
}

/// Whether a disk of `size` bytes is at least 99% of the `expected` size.
pub fn meets_expected(size: u64, expected: u64) -> bool {
    u128::from(size) * 100 >= u128::from(expected) * u128::from(100 - CAPACITY_TOLERANCE_PERCENT)
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIB: u64 = Bytes::MIB;
    /// A "2 TB" NVMe disk, as in v2's `test_rpool_mirror.py`.
    const DISK_BYTES: u64 = 2_000_398_934_016;
    const MEMBER_END: u64 = (DISK_BYTES / GIB * GIB - GIB) / 512 - 1;
    const REPLACEMENT_BYTES: u64 = (MEMBER_END + 1) * 512 + MIB;

    // Ported from test_rpool_mirror.py
    // test_pair_partitions_end_at_the_smaller_disk_in_either_order.
    #[test]
    fn pair_partitions_end_at_the_smaller_disk_in_either_order() {
        let smaller = DISK_BYTES - 400 * MIB;
        let expected = (smaller / GIB * GIB - GIB) / 512 - 1;
        assert!(expected < MEMBER_END);
        for sizes in [
            [DISK_BYTES, smaller],
            [smaller, DISK_BYTES],
            [smaller, smaller],
        ] {
            assert_eq!(
                member_partition(&sizes, 512),
                Ok((2048, expected)),
                "{sizes:?}"
            );
        }
        assert_eq!(
            member_partition(&[DISK_BYTES, DISK_BYTES], 512),
            Ok((2048, MEMBER_END))
        );
        // The same byte boundary on a 4Kn disk.
        let (first, last) = member_partition(&[smaller, DISK_BYTES], 4096).unwrap();
        assert_eq!((first, (last + 1) * 4096), (256, (expected + 1) * 512));
    }

    // Ported from test_rpool_mirror.py
    // test_replacements_only_need_room_for_the_survivors_partitions.
    #[test]
    fn replacements_only_need_room_for_the_survivors_partitions() {
        // The boot survivor's last partition ends at MEMBER_END.
        assert_eq!(replacement_bytes(MEMBER_END, 512), Ok(REPLACEMENT_BYTES));
        const { assert!(REPLACEMENT_BYTES < DISK_BYTES) };
        // A slightly smaller disk takes the layout as long as it holds the
        // partitions and the backup GPT.
        let (_, last) = member_partition(&[DISK_BYTES], 512).unwrap();
        assert!(replacement_bytes(last, 512).unwrap() <= REPLACEMENT_BYTES);
        const { assert!(REPLACEMENT_BYTES + GIB - MIB <= DISK_BYTES) };
    }

    #[test]
    fn member_partitions_need_a_real_disk() {
        assert_eq!(member_end_bytes(&[]), Err(PartitionError::NoDisks));
        assert_eq!(
            member_end_bytes(&[8 * GIB - 1]),
            Err(PartitionError::TooSmall(Bytes(8 * GIB - 1)))
        );
        assert_eq!(member_end_bytes(&[8 * GIB + 5]), Ok(7 * GIB));
        assert_eq!(
            member_partition(&[DISK_BYTES], 520),
            Err(PartitionError::Sector(520))
        );
        assert_eq!(member_partition_bytes(&[8 * GIB]), Ok(7 * GIB - MIB));
    }

    // add_proxmox_host.sh: hdsize = smaller / GiB - 4, keeping 4 GiB free.
    #[test]
    fn installer_hdsize_keeps_four_gib() {
        assert_eq!(
            installer_hdsize_gib(&[DISK_BYTES, DISK_BYTES + GIB]),
            Ok(DISK_BYTES / GIB - 4)
        );
        let hdsize = installer_hdsize_gib(&[DISK_BYTES]).unwrap();
        assert!(DISK_BYTES - hdsize * GIB >= 4 * GIB);
        assert_eq!(
            installer_hdsize_gib(&[4 * GIB]),
            Err(PartitionError::NoHdsize(Bytes(4 * GIB)))
        );
        assert_eq!(installer_hdsize_gib(&[5 * GIB]), Ok(1));
        assert_eq!(installer_hdsize_gib(&[]), Err(PartitionError::NoDisks));
    }

    // rpool_mirror.sh: members within 1%; a disk at least 99% of expected.
    #[test]
    fn capacity_tolerances() {
        assert!(within_tolerance(1000, 990));
        assert!(within_tolerance(990, 1000));
        assert!(!within_tolerance(1000, 989));
        assert!(meets_expected(990, 1000));
        assert!(!meets_expected(989, 1000));
        assert!(within_tolerance(u64::MAX, u64::MAX - 1));
    }
}
