// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

//! udev's database entry for a block device, the source `lsblk` reads
//! serials, models, and WWNs from. Reading it directly gives the same
//! values `lsblk` shows on the installed host.

use std::collections::BTreeMap;
use std::fs;

use crate::root::Root;

/// The udev properties the installer's disk filters may match on.
pub const IDENTITY_KEYS: [&str; 5] = [
    "ID_SERIAL",
    "ID_SERIAL_SHORT",
    "ID_SCSI_SERIAL",
    "ID_WWN",
    "ID_WWN_WITH_EXTENSION",
];

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UdevEntry {
    properties: BTreeMap<String, String>,
    links: Vec<String>,
}

impl UdevEntry {
    /// The entry for a block device by its `major:minor`; empty when udev
    /// has none (it isn't running, or the device is new).
    pub fn for_block(root: &Root, dev: Option<&str>) -> UdevEntry {
        let Some(dev) = dev else {
            return UdevEntry::default();
        };
        fs::read_to_string(root.udev.join(format!("b{dev}")))
            .map(|text| UdevEntry::parse(&text))
            .unwrap_or_default()
    }

    /// `E:KEY=VALUE` property lines and `S:` link lines; the others (tags,
    /// priority) are ignored.
    pub fn parse(text: &str) -> UdevEntry {
        UdevEntry {
            properties: text
                .lines()
                .filter_map(|line| line.strip_prefix("E:"))
                .filter_map(|line| line.split_once('='))
                .map(|(key, value)| (key.to_string(), value.to_string()))
                .collect(),
            links: text
                .lines()
                .filter_map(|line| line.strip_prefix("S:"))
                .map(str::to_string)
                .collect(),
        }
    }

    pub fn get(&self, key: &str) -> Option<&str> {
        self.properties
            .get(key)
            .map(|v| v.trim())
            .filter(|v| !v.is_empty())
    }

    /// The names under `/dev/disk/by-id/`.
    pub fn by_id(&self) -> Vec<String> {
        self.links
            .iter()
            .filter_map(|link| link.strip_prefix("disk/by-id/"))
            .filter(|name| !name.is_empty() && !name.contains('/'))
            .map(str::to_string)
            .collect()
    }

    /// The identity properties that are set, verbatim.
    pub fn identity(&self) -> BTreeMap<String, String> {
        IDENTITY_KEYS
            .iter()
            .filter_map(|key| {
                let value = self.properties.get(*key)?;
                (!value.is_empty()).then(|| (key.to_string(), value.clone()))
            })
            .collect()
    }

    /// An `_ENC` value with udev's `\xNN` escapes decoded, trimmed.
    pub fn decoded(&self, key: &str) -> Option<String> {
        let text = decode(self.get(key)?);
        let text = text.trim();
        (!text.is_empty()).then(|| text.to_string())
    }
}

fn decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\\'
            && bytes.get(i + 1) == Some(&b'x')
            && let Some(hex) = text.get(i + 2..i + 4)
            && let Ok(byte) = u8::from_str_radix(hex, 16)
        {
            out.push(byte);
            i += 4;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_properties_and_decodes_escapes() {
        let entry = UdevEntry::parse(
            "S:disk/by-id/wwn-0x5000c500a1b2c3d4\nE:ID_SERIAL_SHORT=ZA1B2C3D\nE:ID_MODEL_ENC=ST4000NM0035\\x20\\x20\\x20\nE:ID_WWN=0x5000c500a1b2c3d4\nG:systemd\n",
        );
        assert_eq!(entry.get("ID_SERIAL_SHORT"), Some("ZA1B2C3D"));
        assert_eq!(
            entry.decoded("ID_MODEL_ENC").as_deref(),
            Some("ST4000NM0035")
        );
        assert_eq!(entry.get("ID_WWN"), Some("0x5000c500a1b2c3d4"));
        assert_eq!(entry.get("S"), None);
        assert_eq!(entry.by_id(), ["wwn-0x5000c500a1b2c3d4"]);
        assert_eq!(
            entry.identity().into_iter().collect::<Vec<_>>(),
            [
                ("ID_SERIAL_SHORT".to_string(), "ZA1B2C3D".to_string()),
                ("ID_WWN".to_string(), "0x5000c500a1b2c3d4".to_string()),
            ]
        );
    }

    #[test]
    fn decode_leaves_a_broken_escape_alone() {
        assert_eq!(decode("a\\x2"), "a\\x2");
        assert_eq!(decode("Dell\\x20Ent"), "Dell Ent");
    }
}
