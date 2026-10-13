// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

//! ZFS vdev labels, read straight from the device without `zdb` or the ZFS
//! module, so the discovery OS needs neither to report them.
//!
//! A vdev carries four 256 KiB labels: two at the start and two at the end
//! of the device, the end ones aligned down to 256 KiB. Each label holds,
//! 16 KiB in, a 112 KiB region whose start is an XDR-encoded nvlist
//! (`name`, `pool_guid`, `guid`, `state`, `txg`, `hostname`, and more).

use std::collections::BTreeMap;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

use bmac_proto::inventory::{ZfsLabel, ZfsPoolState};

pub const LABEL_SIZE: u64 = 256 * 1024;
pub const NVLIST_OFFSET: u64 = 16 * 1024;
pub const NVLIST_SIZE: usize = 112 * 1024 - 40;

const NV_ENCODE_XDR: u8 = 1;
const DATA_TYPE_UINT64: i32 = 8;
const DATA_TYPE_STRING: i32 = 9;
const DATA_TYPE_NVLIST: i32 = 19;
const DATA_TYPE_NVLIST_ARRAY: i32 = 20;

/// The byte offsets of the four labels on a device of `size` bytes; fewer
/// when the device is too small to hold them all.
pub fn label_offsets(size: u64) -> Vec<u64> {
    let aligned = size - size % LABEL_SIZE;
    if aligned < 4 * LABEL_SIZE {
        return if aligned >= 2 * LABEL_SIZE {
            vec![0, LABEL_SIZE]
        } else {
            Vec::new()
        };
    }
    vec![
        0,
        LABEL_SIZE,
        aligned - 2 * LABEL_SIZE,
        aligned - LABEL_SIZE,
    ]
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NvValue {
    U64(u64),
    Str(String),
    /// An embedded list; only encoded, for fixtures.
    List(Vec<(String, NvValue)>),
    /// An array of embedded lists; only encoded, for fixtures.
    Lists(Vec<Vec<(String, NvValue)>>),
}

/// The top-level pairs of a packed XDR nvlist that are numbers or strings;
/// embedded lists and other arrays are skipped. `None` unless it is a well-formed
/// XDR nvlist.
pub fn decode_nvlist(buf: &[u8]) -> Option<BTreeMap<String, NvValue>> {
    if buf.len() < 12 || buf[0] != NV_ENCODE_XDR {
        return None;
    }
    let mut pos = 4 + 8; // header, then version and flags
    let mut pairs = BTreeMap::new();
    loop {
        let encoded = be_i32(buf, pos)?;
        let decoded = be_i32(buf, pos + 4)?;
        if encoded == 0 && decoded == 0 {
            return Some(pairs);
        }
        if encoded < 24 || decoded <= 0 {
            return None;
        }
        // A pair's encoded size includes an embedded list's pairs, so
        // stepping by it skips the list whole.
        let end = pos.checked_add(encoded as usize)?;
        if end > buf.len() {
            return None;
        }
        let pair = &buf[pos..end];
        let (name, mut at) = xdr_string(pair, 8)?;
        let kind = be_i32(pair, at)?;
        let count = be_i32(pair, at + 4)?;
        at += 8;
        match (kind, count) {
            (DATA_TYPE_UINT64, 1) => {
                let value = u64::from_be_bytes(pair.get(at..at + 8)?.try_into().ok()?);
                pairs.insert(name, NvValue::U64(value));
            }
            (DATA_TYPE_STRING, 1) => {
                let (value, _) = xdr_string(pair, at)?;
                pairs.insert(name, NvValue::Str(value));
            }
            _ => {}
        }
        pos = end;
    }
}

fn be_i32(buf: &[u8], at: usize) -> Option<i32> {
    Some(i32::from_be_bytes(buf.get(at..at + 4)?.try_into().ok()?))
}

fn xdr_string(buf: &[u8], at: usize) -> Option<(String, usize)> {
    let len = usize::try_from(be_i32(buf, at)?).ok()?;
    let start = at + 4;
    let bytes = buf.get(start..start.checked_add(len)?)?;
    let text = std::str::from_utf8(bytes).ok()?.to_string();
    Some((text, start + len.div_ceil(4) * 4))
}

/// Encodes pairs as ZFS writes a label's nvlist. Used to build fixtures.
pub fn encode_nvlist(pairs: &[(&str, NvValue)]) -> Vec<u8> {
    let mut out = vec![NV_ENCODE_XDR, 1, 0, 0];
    let pairs: Vec<(String, NvValue)> = pairs
        .iter()
        .map(|(n, v)| (n.to_string(), v.clone()))
        .collect();
    put_list(&mut out, &pairs);
    out
}

/// As libnvpair's XDR encoder does: an embedded list follows its pair's
/// header, inside the pair's encoded size.
fn put_list(out: &mut Vec<u8>, pairs: &[(String, NvValue)]) {
    out.extend(0i32.to_be_bytes()); // version
    out.extend(1u32.to_be_bytes()); // NV_UNIQUE_NAME
    for (name, value) in pairs {
        let mut body = Vec::new();
        put_string(&mut body, name);
        let mut embedded = Vec::new();
        let (kind, count, data) = match value {
            NvValue::U64(v) => (DATA_TYPE_UINT64, 1, v.to_be_bytes().to_vec()),
            NvValue::Str(s) => {
                let mut data = Vec::new();
                put_string(&mut data, s);
                (DATA_TYPE_STRING, 1, data)
            }
            NvValue::List(list) => {
                put_list(&mut embedded, list);
                (DATA_TYPE_NVLIST, 1, Vec::new())
            }
            NvValue::Lists(lists) => {
                lists.iter().for_each(|l| put_list(&mut embedded, l));
                (DATA_TYPE_NVLIST_ARRAY, lists.len() as i32, Vec::new())
            }
        };
        body.extend(kind.to_be_bytes());
        body.extend(count.to_be_bytes());
        body.extend(&data);
        body.extend(embedded);
        let encoded = (8 + body.len()) as i32;
        out.extend(encoded.to_be_bytes());
        out.extend(encoded.to_be_bytes());
        out.extend(body);
    }
    out.extend([0u8; 8]);
}

fn put_string(out: &mut Vec<u8>, text: &str) {
    out.extend((text.len() as u32).to_be_bytes());
    out.extend(text.as_bytes());
    out.resize(out.len() + (4 - text.len() % 4) % 4, 0);
}

fn state(value: Option<u64>) -> ZfsPoolState {
    match value {
        Some(0) => ZfsPoolState::Active,
        Some(1) => ZfsPoolState::Exported,
        Some(2) => ZfsPoolState::Destroyed,
        Some(3) => ZfsPoolState::Spare,
        Some(4) => ZfsPoolState::L2cache,
        Some(5) => ZfsPoolState::Uninitialized,
        Some(6) => ZfsPoolState::Unavail,
        Some(7) => ZfsPoolState::PotentiallyActive,
        _ => ZfsPoolState::Unknown,
    }
}

/// How far from the end of a device stranded label copies are looked for.
/// Growing a partition (as encrypting a boot member in place does, to make
/// room for a LUKS header) leaves the old end copies that far inside it,
/// where a reader looking only at the end misses them, until a reinstall
/// recreates the partition at its old size and finds a second rpool.
pub const TAIL_SCAN: u64 = 64 * 1024 * 1024;

const GIB: u64 = 1024 * 1024 * 1024;

/// Where label copies may be on a device that starts `start` bytes into
/// its disk: the four slots; every 256 KiB-aligned slot in the last
/// [`TAIL_SCAN`] bytes; and the end slots of a partition that ended on a
/// whole GiB of the disk, as the Proxmox installer's does with the `hdsize`
/// the Deployer gives it. Encrypting such a partition in place grows it,
/// which strands its end copies there.
fn label_slots(size: u64, start: u64) -> Vec<u64> {
    let mut slots: std::collections::BTreeSet<u64> = label_offsets(size).into_iter().collect();
    let aligned = size - size % LABEL_SIZE;
    if aligned >= 4 * LABEL_SIZE {
        let mut offset = aligned.saturating_sub(TAIL_SCAN).max(2 * LABEL_SIZE);
        while offset + LABEL_SIZE <= aligned {
            slots.insert(offset);
            offset += LABEL_SIZE;
        }
        let mut end = (start / GIB + 1) * GIB;
        while end <= start + aligned {
            let old_size = end - start;
            if old_size.is_multiple_of(LABEL_SIZE) && old_size >= 4 * LABEL_SIZE {
                slots.extend(label_offsets(old_size).into_iter().skip(2));
            }
            end += GIB;
        }
    }
    slots.into_iter().collect()
}

/// A label copy's nvlist at `offset`, if it holds one.
fn label_at(
    file: &mut File,
    offset: u64,
    buf: &mut [u8],
) -> std::io::Result<Option<BTreeMap<String, NvValue>>> {
    file.seek(SeekFrom::Start(offset + NVLIST_OFFSET))?;
    if file.read_exact(&mut buf[..4]).is_err() || buf[0] != NV_ENCODE_XDR {
        return Ok(None);
    }
    if file.read_exact(&mut buf[4..]).is_err() {
        return Ok(None);
    }
    Ok(decode_nvlist(buf)
        .filter(|pairs| pairs.contains_key("guid") || pairs.contains_key("pool_guid")))
}

/// Drops what the kernel has cached of `file`. A disk and each of its
/// partitions are cached apart, so a label zeroed through one could still
/// be read back, stale, through another.
fn uncached(file: &File) -> std::io::Result<()> {
    rustix::fs::fadvise(file, 0, None, rustix::fs::Advice::DontNeed).map_err(Into::into)
}

fn device_size(file: &mut File) -> std::io::Result<u64> {
    let size = file.seek(SeekFrom::End(0))?;
    file.seek(SeekFrom::Start(0))?;
    Ok(size)
}

/// The label on a device that starts `start` bytes into its disk (0 for a
/// whole disk), from the newest readable copy; `None` when no copy is
/// readable. An I/O error reading the device is returned.
pub fn read_label(path: &Path, start: u64) -> std::io::Result<Option<ZfsLabel>> {
    let mut file = File::open(path)?;
    uncached(&file)?;
    let size = device_size(&mut file)?;
    let mut copies = Vec::new();
    let mut buf = vec![0u8; NVLIST_SIZE];
    for offset in label_slots(size, start) {
        if let Some(pairs) = label_at(&mut file, offset, &mut buf)? {
            copies.push(pairs);
        }
    }
    let count = copies.len() as u32;
    let txg = |pairs: &BTreeMap<String, NvValue>| match pairs.get("txg") {
        Some(NvValue::U64(t)) => *t,
        _ => 0,
    };
    let Some(newest) = copies.into_iter().max_by_key(txg) else {
        return Ok(None);
    };
    let text = |key: &str| match newest.get(key) {
        Some(NvValue::Str(s)) => Some(s.clone()),
        _ => None,
    };
    let number = |key: &str| match newest.get(key) {
        Some(NvValue::U64(n)) => Some(*n),
        _ => None,
    };
    Ok(Some(ZfsLabel {
        device: path.display().to_string(),
        pool: text("name"),
        pool_guid: number("pool_guid").map(|g| g.to_string()),
        vdev_guid: number("guid").map(|g| g.to_string()),
        state: state(number("state")),
        txg: number("txg"),
        hostname: text("hostname"),
        copies: count,
    }))
}

/// What a label's nvlist holds for a mirror member; used to build
/// fixtures. Its `guid` follows the embedded `vdev_tree`, so a decoder that
/// steps wrongly over an embedded list misses it.
pub fn member_label(pool: &str, pool_guid: u64, guid: u64, state: u64, txg: u64) -> Vec<u8> {
    let disk = |guid: u64, path: &str| {
        vec![
            ("type".to_string(), NvValue::Str("disk".into())),
            ("guid".to_string(), NvValue::U64(guid)),
            ("path".to_string(), NvValue::Str(path.into())),
        ]
    };
    let tree = vec![
        ("type".to_string(), NvValue::Str("mirror".into())),
        ("guid".to_string(), NvValue::U64(guid ^ 1)),
        (
            "children".to_string(),
            NvValue::Lists(vec![
                disk(guid, "/dev/disk/by-id/nvme-a-part3"),
                disk(guid ^ 2, "/dev/disk/by-id/nvme-b-part3"),
            ]),
        ),
    ];
    encode_nvlist(&[
        ("version", NvValue::U64(5000)),
        ("name", NvValue::Str(pool.to_string())),
        ("state", NvValue::U64(state)),
        ("txg", NvValue::U64(txg)),
        ("pool_guid", NvValue::U64(pool_guid)),
        ("hostname", NvValue::Str("old-host".to_string())),
        ("top_guid", NvValue::U64(guid ^ 1)),
        ("vdev_tree", NvValue::List(tree)),
        ("guid", NvValue::U64(guid)),
        ("features_for_read", NvValue::List(Vec::new())),
    ])
}

/// Writes `nvlist` into every label slot of the file at `path`.
pub fn write_label(path: &Path, nvlist: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)?;
    let size = device_size(&mut file)?;
    for offset in label_offsets(size) {
        file.seek(SeekFrom::Start(offset + NVLIST_OFFSET))?;
        file.write_all(nvlist)?;
    }
    Ok(())
}

/// Zeroes the four label slots, as `zpool labelclear` does, and any
/// stranded copy [`read_label`] would find, and flushes to the device.
pub fn zero_labels(path: &Path, start: u64) -> std::io::Result<()> {
    use std::io::Write;
    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)?;
    uncached(&file)?;
    let size = device_size(&mut file)?;
    let standard = label_offsets(size);
    let mut buf = vec![0u8; NVLIST_SIZE];
    let mut slots = Vec::new();
    for offset in label_slots(size, start) {
        if standard.contains(&offset) || label_at(&mut file, offset, &mut buf)?.is_some() {
            slots.push(offset);
        }
    }
    for offset in slots {
        file.seek(SeekFrom::Start(offset))?;
        file.write_all(&vec![0u8; LABEL_SIZE as usize])?;
    }
    file.sync_all()?;
    uncached(&file)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn device(size: u64) -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sdb3");
        File::create(&path).unwrap().set_len(size).unwrap();
        (dir, path)
    }

    #[test]
    fn offsets_put_two_labels_at_each_end_aligned_down() {
        let size = 64 * 1024 * 1024 + 4096;
        let aligned = 64 * 1024 * 1024;
        assert_eq!(
            label_offsets(size),
            [
                0,
                LABEL_SIZE,
                aligned - 2 * LABEL_SIZE,
                aligned - LABEL_SIZE
            ]
        );
        assert!(label_offsets(LABEL_SIZE).is_empty());
    }

    #[test]
    fn round_trips_a_member_label() {
        let (_dir, path) = device(64 * 1024 * 1024);
        write_label(
            &path,
            &member_label("rpool", 12_345_678_901_234_567_890, 42, 0, 1234),
        )
        .unwrap();
        let label = read_label(&path, 0).unwrap().unwrap();
        assert_eq!(label.pool.as_deref(), Some("rpool"));
        assert_eq!(label.pool_guid.as_deref(), Some("12345678901234567890"));
        assert_eq!(label.vdev_guid.as_deref(), Some("42"));
        assert_eq!(label.state, ZfsPoolState::Active);
        assert_eq!(label.txg, Some(1234));
        assert_eq!(label.hostname.as_deref(), Some("old-host"));
        assert_eq!(label.copies, 4);
    }

    #[test]
    fn a_blank_or_cleared_device_has_no_label() {
        let (_dir, path) = device(64 * 1024 * 1024);
        assert_eq!(read_label(&path, 0).unwrap(), None);
        write_label(&path, &member_label("rpool", 1, 2, 1, 9)).unwrap();
        zero_labels(&path, 0).unwrap();
        assert_eq!(read_label(&path, 0).unwrap(), None);
    }

    #[test]
    fn reports_the_newest_copy_and_counts_damaged_ones_out() {
        let (_dir, path) = device(64 * 1024 * 1024);
        write_label(&path, &member_label("rpool", 1, 2, 1, 9)).unwrap();
        {
            use std::io::Write;
            let mut file = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
            file.seek(SeekFrom::Start(NVLIST_OFFSET)).unwrap();
            file.write_all(&member_label("rpool", 1, 2, 2, 20)).unwrap();
            file.seek(SeekFrom::Start(LABEL_SIZE + NVLIST_OFFSET))
                .unwrap();
            file.write_all(b"garbage!").unwrap();
        }
        let label = read_label(&path, 0).unwrap().unwrap();
        assert_eq!(label.txg, Some(20));
        assert_eq!(label.state, ZfsPoolState::Destroyed);
        assert_eq!(label.copies, 3);
    }

    /// A real `rpool` member's label nvlist, from a Proxmox VE 9 install in
    /// the lab: the copy stranded at 28 GiB when its partition was grown,
    /// through the list's terminator.
    const REAL_LABEL: &str = concat!(
        "01010000000000000000000100000024000000200000000776657273696f6e0000000008000000010000000000001388",
        "0000002400000020000000046e616d6500000009000000010000000572706f6f6c000000000000240000002000000005",
        "737461746500000000000008000000010000000000000000000000200000002000000003747867000000000800000001",
        "000000000000005f000000280000002800000009706f6f6c5f67756964000000000000080000000110a451c246af1378",
        "000000240000002000000006657272617461000000000008000000010000000000000000000000280000002800000009",
        "6d696e5f616c6c6f63000000000000080000000100000000000010000000002800000028000000096d61785f616c6c6f",
        "6300000000000008000000010000000000001000000000240000002000000006686f7374696400000000000800000001",
        "000000005251ab36000000280000002800000008686f73746e616d65000000090000000100000006286e6f6e65290000",
        "000000240000002800000008746f705f6775696400000008000000011bac1100bf82e88c000000200000002000000004",
        "677569640000000800000001a365ed65ffdcc7c70000002c000000280000000d766465765f6368696c6472656e000000",
        "000000080000000100000000000000010000043c0000003800000009766465765f747265650000000000001300000001",
        "0000000000000001000000240000002000000004747970650000000900000001000000066d6972726f72000000000020",
        "000000200000000269640000000000080000000100000000000000000000002000000020000000046775696400000008",
        "000000011bac1100bf82e88c0000002c000000280000000e6d657461736c61625f617272617900000000000800000001",
        "00000000000000850000002c000000280000000e6d657461736c61625f73686966740000000000080000000100000000",
        "0000001d00000024000000200000000661736869667400000000000800000001000000000000000c0000002400000020",
        "000000056173697a65000000000000080000000100000006dfa800000000002800000028000000096d696e5f616c6c6f",
        "630000000000000800000001000000000000100000000024000000200000000669735f6c6f6700000000000800000001",
        "000000000000000000000028000000280000000a6372656174655f747867000000000008000000010000000000000004",
        "0000029400000060000000086368696c6472656e00000014000000020000000000000001000000200000002000000004",
        "747970650000000900000001000000046469736b00000020000000200000000269640000000000080000000100000000",
        "00000000000000200000002000000004677569640000000800000001f69bb1a5dac078bf0000007c0000007800000004",
        "7061746800000009000000010000005f2f6465762f6469736b2f62792d69642f6e766d652d6e766d652e316233362d34",
        "643466353833333465333133383334333933362d35313435346435353230346535363464363532303433373437323663",
        "2d30303030303030312d70617274330000000028000000280000000a77686f6c655f6469736b00000000000800000001",
        "000000000000000000000028000000280000000a6372656174655f747867000000000008000000010000000000000004",
        "00000000000000000000000000000001000000200000002000000004747970650000000900000001000000046469736b",
        "000000200000002000000002696400000000000800000001000000000000000100000020000000200000000467756964",
        "0000000800000001a365ed65ffdcc7c70000007c00000078000000047061746800000009000000010000005f2f646576",
        "2f6469736b2f62792d69642f6e766d652d6e766d652e316233362d34643466353833333465333033323435343134352d",
        "353134353464353532303465353634643635323034333734373236632d30303030303030312d70617274330000000028",
        "000000280000000a77686f6c655f6469736b00000000000800000001000000000000000000000028000000280000000a",
        "6372656174655f74786700000000000800000001000000000000000400000000000000000000000000000000000000c8",
        "000000400000001166656174757265735f666f725f72656164000000000000130000000100000000000000010000002c",
        "0000002800000016636f6d2e64656c706869783a686f6c655f6269727468000000000001000000000000003000000030",
        "00000019636f6d2e64656c706869783a656d6265646465645f6461746100000000000001000000000000003400000030",
        "0000001d636f6d2e6b6c61726173797374656d733a766465765f7a6170735f7632000000000000010000000000000000",
        "000000000000000000000000",
    );

    #[test]
    fn decodes_a_real_label() {
        let bytes: Vec<u8> = (0..REAL_LABEL.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&REAL_LABEL[i..i + 2], 16).unwrap())
            .collect();
        let pairs = decode_nvlist(&bytes).unwrap();
        assert_eq!(pairs.get("name"), Some(&NvValue::Str("rpool".into())));
        assert_eq!(
            pairs.get("pool_guid"),
            Some(&NvValue::U64(1199173295638778744))
        );
        assert_eq!(pairs.get("guid"), Some(&NvValue::U64(11774077823170168775)));
        assert_eq!(pairs.get("txg"), Some(&NvValue::U64(95)));
        assert!(!pairs.contains_key("vdev_tree"));
    }

    #[test]
    fn reads_past_embedded_lists_and_unknown_types() {
        let inner = vec![("guid".to_string(), NvValue::U64(99))];
        let mut list = encode_nvlist(&[
            ("name", NvValue::Str("tank".into())),
            ("vdev_tree", NvValue::List(inner.clone())),
            ("children", NvValue::Lists(vec![inner.clone(), inner])),
        ]);
        // Splice in a uint64 array (type 16), whose data is inside its size.
        list.truncate(list.len() - 8);
        let mut body = Vec::new();
        put_string(&mut body, "hole_array");
        body.extend(16i32.to_be_bytes());
        body.extend(2i32.to_be_bytes());
        body.extend([0u8; 16]);
        let size = (8 + body.len()) as i32;
        list.extend(size.to_be_bytes());
        list.extend(size.to_be_bytes());
        list.extend(body);
        list.extend(encode_nvlist(&[("pool_guid", NvValue::U64(7))])[12..].iter());
        let pairs = decode_nvlist(&list).unwrap();
        assert_eq!(pairs.get("name"), Some(&NvValue::Str("tank".into())));
        assert_eq!(pairs.get("pool_guid"), Some(&NvValue::U64(7)));
        assert!(
            !pairs.contains_key("guid"),
            "an embedded list's pairs aren't the label's"
        );
        assert!(!pairs.contains_key("vdev_tree"));
    }

    #[test]
    fn finds_the_end_copies_behind_a_luks_header() {
        let (_dir, path) = device(64 * 1024 * 1024);
        write_label(&path, &member_label("rpool", 5, 6, 0, 30)).unwrap();
        {
            use std::io::Write;
            let mut file = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
            file.write_all(&vec![0xa5; 2 * LABEL_SIZE as usize])
                .unwrap();
        }
        let label = read_label(&path, 0).unwrap().unwrap();
        assert_eq!(label.pool.as_deref(), Some("rpool"));
        assert_eq!(label.vdev_guid.as_deref(), Some("6"));
        assert_eq!(label.copies, 2);
    }

    #[test]
    fn finds_and_clears_copies_stranded_by_a_grown_partition() {
        let (_dir, path) = device(64 * 1024 * 1024);
        write_label(&path, &member_label("rpool", 5, 6, 0, 30)).unwrap();
        {
            use std::io::Write;
            let mut file = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
            file.write_all(&vec![0xa5; 2 * LABEL_SIZE as usize])
                .unwrap();
        }
        // The partition grows by 16 MiB past its old end copies.
        std::fs::OpenOptions::new()
            .write(true)
            .open(&path)
            .unwrap()
            .set_len(80 * 1024 * 1024)
            .unwrap();
        let label = read_label(&path, 0).unwrap().unwrap();
        assert_eq!(label.pool_guid.as_deref(), Some("5"));
        assert_eq!(label.copies, 2);
        zero_labels(&path, 0).unwrap();
        assert_eq!(read_label(&path, 0).unwrap(), None);
    }

    #[test]
    fn finds_copies_stranded_at_the_installers_gib_boundary() {
        // As in the lab: the installer's rpool partition starts at 513 MiB
        // and ends at hdsize (2 GiB here); encrypting it in place grew it
        // by far more than the tail scan covers.
        let start = 513 * 1024 * 1024;
        let (_dir, path) = device(2 * GIB - start);
        write_label(&path, &member_label("rpool", 5, 6, 0, 30)).unwrap();
        {
            use std::io::Write;
            let mut file = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
            file.write_all(&vec![0xa5; 2 * LABEL_SIZE as usize])
                .unwrap();
            file.set_len(2 * GIB - start + 3 * GIB / 2).unwrap();
        }
        assert_eq!(
            read_label(&path, 0).unwrap(),
            None,
            "off the disk's GiB boundaries, nothing is found"
        );
        let label = read_label(&path, start).unwrap().unwrap();
        assert_eq!(label.pool_guid.as_deref(), Some("5"));
        assert_eq!(label.copies, 2);
        zero_labels(&path, start).unwrap();
        assert_eq!(read_label(&path, start).unwrap(), None);
    }

    #[test]
    fn copies_further_in_than_the_tail_scan_are_not_read() {
        let (_dir, path) = device(4 * LABEL_SIZE);
        write_label(&path, &member_label("rpool", 5, 6, 0, 30)).unwrap();
        std::fs::OpenOptions::new()
            .write(true)
            .open(&path)
            .unwrap()
            .set_len(4 * LABEL_SIZE + TAIL_SCAN + 8 * LABEL_SIZE)
            .unwrap();
        let label = read_label(&path, 0).unwrap().unwrap();
        assert_eq!(label.copies, 2, "only the two at the start");
    }

    #[test]
    fn rejects_truncated_or_non_xdr_data() {
        let list = encode_nvlist(&[("name", NvValue::Str("rpool".into()))]);
        assert!(decode_nvlist(&list[..list.len() - 12]).is_none());
        let mut native = list.clone();
        native[0] = 0;
        assert!(decode_nvlist(&native).is_none());
    }
}
