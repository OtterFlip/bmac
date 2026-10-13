// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

//! The Proxmox automated installer's answer file, as v2's `build_iso`
//! writes it: the root password only as a SHA-512 crypt hash, the
//! administrator keys, the public NIC chosen by MAC, the boot vdev's disks
//! chosen by identity, never by device name, `zfs.hdsize` leaving room for
//! a LUKS header, and the first-boot script run before the network.

use std::collections::BTreeSet;

use bmac_proto::inventory::InventoryDisk;
use bmac_storage::{Vdev, VdevType};
use sha2::{Digest, Sha256};

const GIB: u64 = 1 << 30;
/// Boot members smaller than this are refused, as in v2.
pub const MIN_BOOT_DISK_BYTES: u64 = 8 * GIB;
/// Room left at the end of each boot member for the LUKS header and a
/// slightly smaller replacement (`STORAGE.md`).
pub const BOOT_RESERVE_GIB: u64 = 4;
/// The shortest root password the Deployer accepts.
pub const MIN_ROOT_PASSWORD: usize = 8;

/// The salt is derived, not random, so rebuilding a host's ISO gives the
/// same hash for the same password; it differs per cluster and host.
pub fn root_password_salt(cluster: &str, host: &str) -> String {
    let digest = Sha256::digest(format!("bmac-proxmox-root-v1:{cluster}:{host}"));
    digest[..8].iter().map(|b| format!("{b:02x}")).collect()
}

/// `$6$<salt>$<hash>`, what `openssl passwd -6 -salt <salt>` prints.
pub fn sha512_crypt(password: &str, salt: &str) -> String {
    const ORDER: [usize; 64] = [
        42, 21, 0, 1, 43, 22, 23, 2, 44, 45, 24, 3, 4, 46, 25, 26, 5, 47, 48, 27, 6, 7, 49, 28, 29,
        8, 50, 51, 30, 9, 10, 52, 31, 32, 11, 53, 54, 33, 12, 13, 55, 34, 35, 14, 56, 57, 36, 15,
        16, 58, 37, 38, 17, 59, 60, 39, 18, 19, 61, 40, 41, 20, 62, 63,
    ];
    const ALPHABET: &[u8; 64] = b"./0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";
    let digest = sha_crypt::sha512_crypt(
        password.as_bytes(),
        salt.as_bytes(),
        sha_crypt::Params::RECOMMENDED,
    );
    let ordered: Vec<u8> = ORDER.iter().map(|&i| digest[i]).collect();
    let mut text = format!("$6${salt}$");
    for chunk in ordered.chunks(3) {
        let mut value = 0u32;
        for (shift, byte) in chunk.iter().enumerate() {
            value |= u32::from(*byte) << (8 * shift);
        }
        let chars = match chunk.len() {
            3 => 4,
            2 => 3,
            _ => 2,
        };
        for _ in 0..chars {
            text.push(ALPHABET[(value & 0x3f) as usize] as char);
            value >>= 6;
        }
    }
    text
}

/// The hash for the answer file, after checking the password.
pub fn root_password_hash(password: &str, cluster: &str, host: &str) -> Result<String, String> {
    if password.chars().count() < MIN_ROOT_PASSWORD {
        return Err(format!(
            "The root password must be at least {MIN_ROOT_PASSWORD} characters."
        ));
    }
    if password.chars().any(char::is_control) {
        return Err("The root password must not contain control characters.".into());
    }
    Ok(sha512_crypt(password, &root_password_salt(cluster, host)))
}

/// The ZFS partition size for every boot member, in GiB: the smallest
/// member's whole GiB less the reserve. Members must be at least 8 GiB and,
/// when there is more than one, within 1% of each other, as v2 requires.
pub fn boot_hdsize_gib(members: &[&InventoryDisk]) -> Result<u64, String> {
    let Some(smallest) = members.iter().map(|d| d.size_bytes).min() else {
        return Err("The boot vdev has no disks.".into());
    };
    let largest = members
        .iter()
        .map(|d| d.size_bytes)
        .max()
        .unwrap_or(smallest);
    if let Some(small) = members.iter().find(|d| d.size_bytes < MIN_BOOT_DISK_BYTES) {
        return Err(format!(
            "{} is smaller than 8 GiB ({} bytes), too small for a boot disk.",
            small.name, small.size_bytes
        ));
    }
    if u128::from(largest - smallest) * 100 > u128::from(largest) {
        return Err(format!(
            "The boot disks differ in capacity by more than 1% ({smallest} and {largest} bytes); \
             the installer gives every member the same size, so pick disks of one size."
        ));
    }
    let hdsize = smallest / GIB - BOOT_RESERVE_GIB;
    debug_assert!(smallest - hdsize * GIB >= BOOT_RESERVE_GIB * GIB);
    Ok(hdsize)
}

/// The installer's name for a boot vdev type.
pub fn zfs_raid(kind: VdevType) -> &'static str {
    match kind {
        VdevType::Single => "raid0",
        VdevType::Mirror => "raid1",
        VdevType::Raidz1 => "raidz-1",
        VdevType::Raidz2 => "raidz-2",
        VdevType::Raidz3 => "raidz-3",
    }
}

/// One `filter.<PROPERTY> = "<value>"` line of `[disk-setup]`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiskFilter {
    pub property: String,
    pub value: String,
    /// The disk it selects.
    pub disk: String,
}

fn has_glob(value: &str) -> bool {
    value.contains(['*', '?', '[', ']', '{', '}', '\\'])
}

/// The udev properties a boot disk may be selected by. The discovery OS's
/// udev and the installer's (systemd's) agree on these two; systemd spells
/// `ID_SERIAL` differently (underscores for spaces, and an NVMe namespace's
/// `_1` appended), so it is never used.
const FILTER_PROPERTIES: [&str; 2] = ["ID_SERIAL_SHORT", "ID_WWN"];

/// Filters that select exactly the boot disks. The installer matches udev
/// properties with `filter-match = "any"`, and an answer file names each
/// property once, so each boot disk needs a property of its own whose value
/// no other disk shares. Built from the udev values the discovery session
/// read.
pub fn disk_filters(
    boot: &[&InventoryDisk],
    all: &[InventoryDisk],
) -> Result<Vec<DiskFilter>, String> {
    let shared = |property: &str, value: &str, disk: &str| {
        all.iter().any(|other| {
            other.name != disk
                && other.udev_identity.get(property).map(String::as_str) == Some(value)
        })
    };
    let choices: Vec<Vec<(String, String)>> = boot
        .iter()
        .map(|disk| {
            FILTER_PROPERTIES
                .iter()
                .filter_map(|&property| {
                    let value = disk.udev_identity.get(property)?;
                    (!value.is_empty() && !has_glob(value) && !shared(property, value, &disk.name))
                        .then(|| (property.to_string(), value.clone()))
                })
                .collect()
        })
        .collect();
    if let Some((disk, _)) = boot.iter().zip(&choices).find(|(_, c)| c.is_empty()) {
        return Err(format!(
            "{} has no serial number or WWN that udev reports and no other disk shares, so the \
             installer can't be told to use it.",
            disk.name
        ));
    }
    let mut picked: Vec<usize> = Vec::new();
    let mut used = BTreeSet::new();
    if !assign(&choices, 0, &mut picked, &mut used) {
        return Err(format!(
            "The installer selects boot disks through udev properties, one property per disk, \
             and these {} disks don't have enough distinct ones. Use fewer boot disks; the setup \
             job adds the rest of the pool after installation.",
            boot.len()
        ));
    }
    Ok(boot
        .iter()
        .zip(&choices)
        .zip(&picked)
        .map(|((disk, choice), &i)| DiskFilter {
            property: choice[i].0.clone(),
            value: choice[i].1.clone(),
            disk: disk.name.clone(),
        })
        .collect())
}

fn assign(
    choices: &[Vec<(String, String)>],
    at: usize,
    picked: &mut Vec<usize>,
    used: &mut BTreeSet<String>,
) -> bool {
    let Some(options) = choices.get(at) else {
        return true;
    };
    for (i, (property, _)) in options.iter().enumerate() {
        if used.insert(property.clone()) {
            picked.push(i);
            if assign(choices, at + 1, picked, used) {
                return true;
            }
            picked.pop();
            used.remove(property);
        }
    }
    false
}

/// What the answer file says.
#[derive(Debug, Clone)]
pub struct Answer<'a> {
    pub keyboard: &'a str,
    pub country: &'a str,
    pub fqdn: &'a str,
    pub mailto: &'a str,
    pub timezone: &'a str,
    pub root_password_hash: &'a str,
    pub root_ssh_keys: &'a [String],
    pub cidr: &'a str,
    pub dns: &'a str,
    pub gateway: &'a str,
    pub public_mac: &'a str,
    pub boot: &'a Vdev,
    pub filters: &'a [DiskFilter],
    pub ashift: u8,
    pub hdsize_gib: u64,
}

fn q(text: &str) -> String {
    serde_json::to_string(text).expect("strings serialize")
}

impl Answer<'_> {
    pub fn toml(&self) -> String {
        let keys = self
            .root_ssh_keys
            .iter()
            .map(|k| q(k))
            .collect::<Vec<_>>()
            .join(", ");
        let mac = self.public_mac.replace(':', "").to_ascii_lowercase();
        let mut text = format!(
            "[global]\n\
             keyboard = {}\n\
             country = {}\n\
             fqdn = {}\n\
             mailto = {}\n\
             timezone = {}\n\
             root-password-hashed = {}\n\
             root-ssh-keys = [{keys}]\n\
             reboot-on-error = false\n\
             reboot-mode = \"reboot\"\n\
             \n\
             [network]\n\
             source = \"from-answer\"\n\
             cidr = {}\n\
             dns = {}\n\
             gateway = {}\n\
             filter.ID_NET_NAME_MAC = {}\n\
             \n\
             [disk-setup]\n\
             filesystem = \"zfs\"\n\
             filter-match = \"any\"\n",
            q(self.keyboard),
            q(self.country),
            q(self.fqdn),
            q(self.mailto),
            q(self.timezone),
            q(self.root_password_hash),
            q(self.cidr),
            q(self.dns),
            q(self.gateway),
            q(&format!("*{mac}")),
        );
        for filter in self.filters {
            text.push_str(&format!(
                "filter.{} = {}\n",
                filter.property,
                q(&filter.value)
            ));
        }
        text.push_str(&format!(
            "zfs.raid = {}\n\
             zfs.ashift = {}\n\
             zfs.hdsize = {}\n\
             \n\
             [first-boot]\n\
             source = \"from-iso\"\n\
             ordering = \"before-network\"\n",
            q(zfs_raid(self.boot.kind)),
            self.ashift,
            self.hdsize_gib,
        ));
        text
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use std::collections::BTreeMap;

    use super::*;

    pub(crate) fn disk(name: &str, size: u64, props: &[(&str, &str)]) -> InventoryDisk {
        InventoryDisk {
            name: name.into(),
            path: format!("/dev/{name}"),
            identity: None,
            model: None,
            serial: None,
            wwn: None,
            size_bytes: size,
            logical_sector_bytes: 512,
            physical_sector_bytes: 4096,
            rotational: false,
            removable: false,
            transport: None,
            by_id: Vec::new(),
            udev_identity: props
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect::<BTreeMap<_, _>>(),
            partition_table: None,
            partitions: Vec::new(),
            luks: None,
            zfs_labels: Vec::new(),
            problems: Vec::new(),
        }
    }

    #[test]
    fn the_hash_matches_openssl_passwd() {
        assert_eq!(
            sha512_crypt("correct horse battery", "0123456789abcdef"),
            "$6$0123456789abcdef$HssQvjT3.V52fY7d7LgJbgJzE7FOmKdyHRQCSglzi1lEICe2eOxJwFUv9PNh/Oa6lbTGyEGZyegu3nPMDihqv1"
        );
        assert_eq!(
            sha512_crypt("x", "ab"),
            "$6$ab$jnZj29NS/hK2GaouP6AGMIBwD6owZFRejcn9QRdlwSjZaVr.fR9dH2TXrKL/PaacUXwtGfI.4O9zxvy2k6fbE1"
        );
    }

    #[test]
    fn the_salt_comes_from_cluster_and_host() {
        let salt = root_password_salt("MyAppCloud", "mox1");
        assert_eq!(salt.len(), 16);
        assert!(salt.chars().all(|c| c.is_ascii_hexdigit()));
        assert_eq!(salt, root_password_salt("MyAppCloud", "mox1"));
        assert_ne!(salt, root_password_salt("MyAppCloud", "mox2"));
        assert_ne!(salt, root_password_salt("Other", "mox1"));
        let hash = root_password_hash("long enough", "MyAppCloud", "mox1").unwrap();
        assert!(hash.starts_with(&format!("$6${salt}$")));
        assert!(!hash.contains("long enough"));
    }

    #[test]
    fn short_or_multiline_passwords_are_refused() {
        assert!(root_password_hash("short", "c", "mox1").is_err());
        assert!(root_password_hash("long enough\nsecond", "c", "mox1").is_err());
    }

    #[test]
    fn hdsize_leaves_four_gib_and_needs_matching_disks() {
        let a = disk("nvme0n1", 500_107_862_016, &[]);
        let b = disk("nvme1n1", 500_000_000_000, &[]);
        // 500000000000 bytes is 465 whole GiB.
        assert_eq!(boot_hdsize_gib(&[&a, &b]).unwrap(), 461);
        // 500107862016 bytes is also 465 whole GiB.
        assert_eq!(boot_hdsize_gib(&[&a]).unwrap(), 461);

        let big = disk("nvme2n1", 1_000_204_886_016, &[]);
        assert!(boot_hdsize_gib(&[&a, &big]).unwrap_err().contains("1%"));
        let tiny = disk("sda", 7 * GIB, &[]);
        assert!(boot_hdsize_gib(&[&tiny]).unwrap_err().contains("8 GiB"));
        assert_eq!(boot_hdsize_gib(&[&disk("sdb", 8 * GIB, &[])]).unwrap(), 4);
        assert!(boot_hdsize_gib(&[]).is_err());
    }

    #[test]
    fn each_boot_disk_gets_a_property_no_other_disk_shares() {
        let a = disk(
            "nvme0n1",
            GIB * 100,
            &[
                ("ID_SERIAL_SHORT", "S1"),
                ("ID_SERIAL", "Samsung_S1"),
                ("ID_WWN", "eui.1"),
            ],
        );
        let b = disk(
            "nvme1n1",
            GIB * 100,
            &[
                ("ID_SERIAL_SHORT", "S2"),
                ("ID_SERIAL", "Samsung_S2"),
                ("ID_WWN", "eui.2"),
            ],
        );
        let data = disk("sda", GIB * 100, &[("ID_SERIAL_SHORT", "S3")]);
        let all = vec![a.clone(), b.clone(), data];
        let filters = disk_filters(&[&a, &b], &all).unwrap();
        assert_eq!(filters.len(), 2);
        assert_ne!(filters[0].property, filters[1].property);
        assert_eq!(filters[0].disk, "nvme0n1");
        assert_eq!(filters[1].disk, "nvme1n1");
        for filter in &filters {
            let selected: Vec<_> = all
                .iter()
                .filter(|d| d.udev_identity.get(&filter.property) == Some(&filter.value))
                .map(|d| d.name.as_str())
                .collect();
            assert_eq!(selected, [filter.disk.as_str()]);
        }
    }

    #[test]
    fn nvme_boot_disks_match_the_installer_s_udev_too() {
        // As the discovery OS read the lab's NVMe mirror; systemd 257 in the
        // installer reports ID_SERIAL as QEMU_NVMe_Ctrl_MOX3N02EAE_1.
        let a = disk(
            "nvme0n1",
            GIB * 32,
            &[
                ("ID_SERIAL_SHORT", "MOX3N02EAE"),
                ("ID_SERIAL", "QEMU NVMe Ctrl_MOX3N02EAE"),
                ("ID_WWN", "nvme.1b36-4d4f58334e3032454145-00000001"),
            ],
        );
        let b = disk(
            "nvme1n1",
            GIB * 32,
            &[
                ("ID_SERIAL_SHORT", "MOX3N18496"),
                ("ID_SERIAL", "QEMU NVMe Ctrl_MOX3N18496"),
                ("ID_WWN", "nvme.1b36-4d4f58334e3138343936-00000001"),
            ],
        );
        let filters = disk_filters(&[&a, &b], &[a.clone(), b.clone()]).unwrap();
        let properties: Vec<_> = filters.iter().map(|f| f.property.as_str()).collect();
        assert_eq!(properties, ["ID_SERIAL_SHORT", "ID_WWN"]);
    }

    #[test]
    fn a_value_another_disk_shares_is_never_used() {
        // Some SCSI disks report the WWN as ID_SERIAL_SHORT, and a cloned
        // serial can appear twice.
        let a = disk(
            "sda",
            GIB * 100,
            &[("ID_SERIAL_SHORT", "DUP"), ("ID_WWN", "0xa")],
        );
        let b = disk(
            "sdb",
            GIB * 100,
            &[("ID_SERIAL_SHORT", "S2"), ("ID_WWN", "0xb")],
        );
        let clone = disk("sdd", GIB * 100, &[("ID_SERIAL_SHORT", "DUP")]);
        let filters = disk_filters(&[&a, &b], &[a.clone(), b.clone(), clone.clone()]).unwrap();
        assert!(filters.iter().all(|f| f.value != "DUP"));
        assert_eq!(filters[0].value, "0xa");
        // Two boot disks whose serials both clash need two WWNs, and the
        // installer takes each property once.
        let mut c = b.clone();
        c.udev_identity
            .insert("ID_SERIAL_SHORT".into(), "DUP".into());
        assert!(disk_filters(&[&a, &c], &[a.clone(), c.clone(), clone]).is_err());

        let lonely = disk("sdc", GIB * 100, &[("ID_SERIAL_SHORT", "DUP")]);
        let all = vec![a.clone(), lonely.clone()];
        assert!(disk_filters(&[&lonely], &all).unwrap_err().contains("sdc"));
    }

    #[test]
    fn too_few_distinct_properties_is_refused() {
        let disks: Vec<_> = (0..3)
            .map(|i| {
                disk(
                    &format!("sd{i}"),
                    GIB * 100,
                    &[("ID_SERIAL_SHORT", &format!("S{i}"))],
                )
            })
            .collect();
        let boot: Vec<_> = disks.iter().collect();
        assert!(
            disk_filters(&boot, &disks)
                .unwrap_err()
                .contains("fewer boot disks")
        );
    }

    #[test]
    fn glob_characters_are_never_used_as_values() {
        let a = disk(
            "sda",
            GIB * 100,
            &[("ID_SERIAL_SHORT", "AB*"), ("ID_WWN", "0x1")],
        );
        let filters = disk_filters(&[&a], std::slice::from_ref(&a)).unwrap();
        assert_eq!(filters[0].property, "ID_WWN");
    }
}
