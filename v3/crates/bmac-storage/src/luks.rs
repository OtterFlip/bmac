// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

//! LUKS naming and `/etc/crypttab` (`STORAGE.md`, "LUKS"): mapper names
//! from each disk's identity, and the single-prompt `decrypt_keyctl` form
//! every member is listed in, so one console passphrase unlocks the pool.

use bmac_core::PoolName;
use bmac_core::disk::DiskIdentity;

/// The crypttab options of every member: unlocked in the initramfs, with
/// the passphrase typed once and reused through the kernel keyring.
pub const SHARED_OPTIONS: &str = "luks,initramfs,nofail,keyscript=decrypt_keyctl";
/// The longest `<id>` part of a mapper name.
const ID_CHARS: usize = 24;

/// The keyring key `decrypt_keyctl` shares the passphrase under.
pub fn shared_key_id(pool: &PoolName) -> String {
    format!("bmac-{pool}")
}

/// `bmac-<pool>-<id>`, where `<id>` is `w` and the WWN's hex digits, or `s`
/// and the serial's letters and digits; the last 24 of them, which differ
/// most between disks of one model.
pub fn mapper_name(pool: &PoolName, identity: &DiskIdentity) -> String {
    let (tag, value) = match identity {
        DiskIdentity::Wwn(wwn) => {
            let bare = wwn
                .strip_prefix("0x")
                .or_else(|| wwn.split_once('.').map(|(_, rest)| rest))
                .unwrap_or(wwn);
            ('w', bare)
        }
        DiskIdentity::Serial(serial) => ('s', serial.as_str()),
    };
    let kept: Vec<char> = value.chars().filter(char::is_ascii_alphanumeric).collect();
    let id: String = kept[kept.len().saturating_sub(ID_CHARS)..].iter().collect();
    format!("bmac-{pool}-{tag}{id}")
}

/// One member's line: `name UUID=<luks uuid> <key id> <options>`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub name: String,
    pub luks_uuid: String,
}

impl Entry {
    pub fn line(&self, pool: &PoolName) -> String {
        format!(
            "{}\tUUID={}\t{}\t{SHARED_OPTIONS}",
            self.name,
            self.luks_uuid,
            shared_key_id(pool)
        )
    }
}

fn fields(line: &str) -> Option<Vec<&str>> {
    let trimmed = line.trim();
    (!trimmed.is_empty() && !trimmed.starts_with('#')).then(|| trimmed.split_whitespace().collect())
}

/// Whether `text` lists `entry` exactly once, in the shared form.
pub fn lists(text: &str, pool: &PoolName, entry: &Entry) -> bool {
    let source = format!("UUID={}", entry.luks_uuid);
    let key = shared_key_id(pool);
    let mut matching = 0;
    for f in text.lines().filter_map(fields) {
        if f.first() != Some(&entry.name.as_str()) {
            continue;
        }
        let shared = f.get(1) == Some(&source.as_str())
            && f.get(2) == Some(&key.as_str())
            && f.get(3) == Some(&SHARED_OPTIONS);
        if !shared {
            return false;
        }
        matching += 1;
    }
    matching == 1
}

/// `text` with every line for these entries' names replaced by one shared
/// line each, appended; other lines, comments included, stay as they are.
pub fn with_entries(text: &str, pool: &PoolName, entries: &[Entry]) -> String {
    let mut out: Vec<String> = text
        .lines()
        .filter(|line| {
            fields(line)
                .and_then(|f| f.first().copied())
                .is_none_or(|name| entries.iter().all(|e| e.name != name))
        })
        .map(str::to_string)
        .collect();
    out.extend(entries.iter().map(|e| e.line(pool)));
    let mut joined = out.join("\n");
    joined.push('\n');
    joined
}

/// The mapper names the initramfs's `cryptroot/crypttab` lists.
pub fn initramfs_names(text: &str) -> Vec<String> {
    text.lines()
        .filter_map(fields)
        .filter_map(|f| f.first().map(|n| n.to_string()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rpool() -> PoolName {
        "rpool".parse().unwrap()
    }

    #[test]
    fn mapper_names_come_from_the_identity() {
        let wwn = DiskIdentity::Wwn("0x5000c500a1b2c3d4".into());
        assert_eq!(mapper_name(&rpool(), &wwn), "bmac-rpool-w5000c500a1b2c3d4");
        let eui = DiskIdentity::Wwn("eui.0025388b91c1e2f3".into());
        assert_eq!(mapper_name(&rpool(), &eui), "bmac-rpool-w0025388b91c1e2f3");
        let serial = DiskIdentity::Serial("WD-WX12 34/5".into());
        assert_eq!(mapper_name(&rpool(), &serial), "bmac-rpool-sWDWX12345");
        let long = DiskIdentity::Serial("ABCDEFGHIJKLMNOPQRSTUVWXYZ0123".into());
        assert_eq!(
            mapper_name(&rpool(), &long),
            "bmac-rpool-sGHIJKLMNOPQRSTUVWXYZ0123"
        );
    }

    #[test]
    fn entries_are_replaced_in_the_shared_form_and_the_rest_is_kept() {
        let a = Entry {
            name: "bmac-rpool-sA".into(),
            luks_uuid: "u1".into(),
        };
        let b = Entry {
            name: "bmac-rpool-sB".into(),
            luks_uuid: "u2".into(),
        };
        let before = "# <target name> <source device> <key file> <options>\n\
                      other UUID=x none luks\n\
                      bmac-rpool-sA UUID=old none luks,initramfs\n";
        let after = with_entries(before, &rpool(), &[a.clone(), b.clone()]);
        assert!(after.starts_with("# <target name>"));
        assert!(after.contains("other UUID=x none luks\n"));
        assert!(!after.contains("UUID=old"));
        assert!(lists(&after, &rpool(), &a) && lists(&after, &rpool(), &b));
        let again = with_entries(&after, &rpool(), std::slice::from_ref(&a));
        assert!(lists(&again, &rpool(), &a) && lists(&again, &rpool(), &b));
        assert_eq!(again.lines().count(), after.lines().count());
        assert!(
            !lists(before, &rpool(), &a),
            "the old plain form isn't enough"
        );
        let doubled = format!("{after}{}\n", a.line(&rpool()));
        assert!(!lists(&doubled, &rpool(), &a), "a name listed twice");
    }

    #[test]
    fn the_initramfs_crypttab_names_its_mappers() {
        let text = "bmac-rpool-sA UUID=u1 bmac-rpool luks,keyscript=/lib/cryptsetup/scripts/decrypt_keyctl\n\n# x\n";
        assert_eq!(initramfs_names(text), ["bmac-rpool-sA"]);
    }
}
