// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

//! LUKS headers and partition tables, read from the first sectors.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

use bmac_proto::inventory::{LuksHeader, PartitionTable};

const LUKS_MAGIC: &[u8; 6] = b"LUKS\xba\xbe";
/// LUKS1 and LUKS2 both keep the UUID here, 40 bytes, NUL-padded.
const UUID_OFFSET: usize = 168;

pub fn read_luks(path: &Path) -> std::io::Result<Option<LuksHeader>> {
    let mut head = [0u8; 512];
    let read = File::open(path)?.read(&mut head)?;
    Ok(parse_luks(&head[..read]))
}

pub fn parse_luks(head: &[u8]) -> Option<LuksHeader> {
    if head.len() < UUID_OFFSET + 40 || &head[..6] != LUKS_MAGIC {
        return None;
    }
    let version = u16::from_be_bytes([head[6], head[7]]);
    let uuid = &head[UUID_OFFSET..UUID_OFFSET + 40];
    let end = uuid.iter().position(|&b| b == 0).unwrap_or(uuid.len());
    let uuid = std::str::from_utf8(&uuid[..end])
        .ok()
        .map(str::trim)
        .filter(|u| !u.is_empty())
        .map(str::to_string);
    Some(LuksHeader { version, uuid })
}

/// GPT when a GPT header sits at LBA 1, else MBR when the boot signature is
/// there, else none.
pub fn read_partition_table(
    path: &Path,
    logical_sector: u32,
) -> std::io::Result<Option<PartitionTable>> {
    let mut file = File::open(path)?;
    let mut mbr = [0u8; 512];
    if file.read_exact(&mut mbr).is_err() {
        return Ok(None);
    }
    let mut gpt = [0u8; 8];
    file.seek(SeekFrom::Start(u64::from(logical_sector.max(512))))?;
    if file.read_exact(&mut gpt).is_ok() && &gpt == b"EFI PART" {
        return Ok(Some(PartitionTable::Gpt));
    }
    Ok((mbr[510] == 0x55 && mbr[511] == 0xaa).then_some(PartitionTable::Mbr))
}

/// A LUKS header as `cryptsetup luksFormat` would leave one; for fixtures.
pub fn fake_header(version: u16, uuid: &str) -> Vec<u8> {
    let mut head = vec![0u8; 4096];
    head[..6].copy_from_slice(LUKS_MAGIC);
    head[6..8].copy_from_slice(&version.to_be_bytes());
    head[UUID_OFFSET..UUID_OFFSET + uuid.len()].copy_from_slice(uuid.as_bytes());
    head
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_version_and_uuid() {
        let header = parse_luks(&fake_header(2, "0f1e2d3c-4b5a-6978-8796-a5b4c3d2e1f0")).unwrap();
        assert_eq!(header.version, 2);
        assert_eq!(
            header.uuid.as_deref(),
            Some("0f1e2d3c-4b5a-6978-8796-a5b4c3d2e1f0")
        );
        assert_eq!(parse_luks(&[0u8; 512]), None);
    }

    #[test]
    fn tells_gpt_from_mbr() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sda");
        let mut data = vec![0u8; 8192];
        data[510] = 0x55;
        data[511] = 0xaa;
        std::fs::write(&path, &data).unwrap();
        assert_eq!(
            read_partition_table(&path, 512).unwrap(),
            Some(PartitionTable::Mbr)
        );
        data[4096..4104].copy_from_slice(b"EFI PART");
        std::fs::write(&path, &data).unwrap();
        assert_eq!(
            read_partition_table(&path, 512).unwrap(),
            Some(PartitionTable::Mbr)
        );
        assert_eq!(
            read_partition_table(&path, 4096).unwrap(),
            Some(PartitionTable::Gpt)
        );
        std::fs::write(&path, vec![0u8; 8192]).unwrap();
        assert_eq!(read_partition_table(&path, 512).unwrap(), None);
    }
}
