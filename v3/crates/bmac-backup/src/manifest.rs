// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

//! The PIT manifest (`BACKUP.md`, "Capturing a PIT"): a versioned record of
//! everything a restore needs besides the data, in canonical JSON (object
//! keys sorted, no whitespace) with a SHA-256 over the payload without its
//! `sha256` field. The cluster writes it to the PIT catalog; each appliance
//! keeps its own copy of every manifest it publishes.

use std::collections::BTreeMap;

use bmac_core::SnapshotGuid;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::names::PitName;

pub const SCHEMA_VERSION: u32 = 1;

/// How consistent the PIT's data is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Consistency {
    /// As if the power had been cut.
    Crash,
    /// Guest filesystems frozen through the guest agent.
    Filesystem,
    /// Frozen, and the operator's hooks reported success.
    Application,
}

/// What a volume is to the VM.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VolumeRole {
    Disk,
    EfiVars,
    TpmState,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Volume {
    pub volume_uuid: String,
    /// The VM configuration key it is attached at: `scsi0`, `efidisk0`, …
    pub slot: String,
    pub role: VolumeRole,
    /// The source dataset on the owner at capture, such as
    /// `rpool/data/vm-100-disk-0`.
    pub dataset: String,
    pub snapshot_guid: SnapshotGuid,
    pub volsize: u64,
    pub volblocksize: u64,
    pub boot: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Firmware {
    /// `seabios` or `ovmf`.
    pub bios: String,
    pub machine: Option<String>,
    pub secure_boot: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Versions {
    pub proxmox: String,
    pub openzfs: String,
    pub bmac: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub schema_version: u32,
    pub cluster_uuid: String,
    pub vm_uuid: String,
    pub vmid: u32,
    pub pit_uuid: String,
    pub branch_uuid: String,
    pub parent_pit_uuid: Option<String>,
    pub snapshot: PitName,
    /// UTC epoch seconds.
    pub taken_at_s: i64,
    pub consistency: Consistency,
    /// The guest agent's and hooks' answers, as recorded.
    pub evidence: Value,
    /// Snapshots spread over several pools aren't taken atomically; set
    /// only when the policy accepted that.
    pub multi_pool: bool,
    pub versions: Versions,
    /// `/etc/pve/qemu-server/<vmid>.conf`, current settings only.
    pub config_text: String,
    pub config: BTreeMap<String, String>,
    pub config_sha256: String,
    pub volumes: Vec<Volume>,
    pub firmware: Firmware,
    /// The BMAC registry record of the resource.
    pub registry: Value,
    /// What a restore can't recreate, for the operator.
    pub prerequisites: Vec<String>,
    /// Over the canonical payload without this field.
    #[serde(default)]
    pub sha256: String,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ManifestError {
    #[error("manifest schema {0} is not supported (expected {SCHEMA_VERSION})")]
    Schema(u32),
    #[error("the manifest's SHA-256 doesn't match its contents")]
    Digest,
    #[error("the manifest's configuration SHA-256 doesn't match its text")]
    ConfigDigest,
    #[error("the manifest names no volumes")]
    NoVolumes,
    #[error("the manifest lists slot {0} twice")]
    DuplicateSlot(String),
    #[error("the manifest is not valid JSON: {0}")]
    Json(String),
}

impl Manifest {
    /// The payload the digest covers.
    fn payload(&self) -> Value {
        let mut value = serde_json::to_value(self).expect("manifests serialize");
        if let Value::Object(map) = &mut value {
            map.remove("sha256");
        }
        value
    }

    pub fn digest(&self) -> String {
        hex(&Sha256::digest(canonical(&self.payload()).as_bytes()))
    }

    /// Sets the digest, and returns the canonical JSON to store.
    pub fn seal(&mut self) -> String {
        self.sha256 = self.digest();
        canonical(&serde_json::to_value(&*self).expect("manifests serialize"))
    }

    /// Reads a stored manifest and checks it.
    pub fn open(text: &str) -> Result<Manifest, ManifestError> {
        let manifest: Manifest =
            serde_json::from_str(text).map_err(|e| ManifestError::Json(e.to_string()))?;
        manifest.check()?;
        Ok(manifest)
    }

    pub fn check(&self) -> Result<(), ManifestError> {
        if self.schema_version != SCHEMA_VERSION {
            return Err(ManifestError::Schema(self.schema_version));
        }
        if self.sha256 != self.digest() {
            return Err(ManifestError::Digest);
        }
        if self.config_sha256 != sha256_hex(self.config_text.as_bytes()) {
            return Err(ManifestError::ConfigDigest);
        }
        if self.volumes.is_empty() {
            return Err(ManifestError::NoVolumes);
        }
        let mut slots = std::collections::BTreeSet::new();
        for v in &self.volumes {
            if !slots.insert(v.slot.as_str()) {
                return Err(ManifestError::DuplicateSlot(v.slot.clone()));
            }
        }
        Ok(())
    }
}

/// JSON with object keys sorted and no whitespace, whatever order the
/// value's maps keep.
pub fn canonical(value: &Value) -> String {
    let mut out = String::new();
    write_canonical(value, &mut out);
    out
}

fn write_canonical(value: &Value, out: &mut String) {
    match value {
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            out.push('{');
            for (i, key) in keys.into_iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                out.push_str(&Value::String(key.clone()).to_string());
                out.push(':');
                write_canonical(&map[key], out);
            }
            out.push('}');
        }
        Value::Array(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_canonical(item, out);
            }
            out.push(']');
        }
        scalar => out.push_str(&scalar.to_string()),
    }
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    hex(&Sha256::digest(bytes))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Proxmox's `key: value` configuration lines for the current settings:
/// up to the first `[section]` (snapshots, pending changes), without
/// comments.
pub fn current_config(text: &str) -> (String, BTreeMap<String, String>) {
    let mut kept = Vec::new();
    let mut parsed = BTreeMap::new();
    for line in text.lines() {
        if line.starts_with('[') {
            break;
        }
        if line.trim().is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some((key, value)) = line.split_once(':') {
            parsed.insert(key.trim().to_string(), value.trim().to_string());
            kept.push(line);
        }
    }
    let mut text: String = kept.join("\n");
    if !text.is_empty() {
        text.push('\n');
    }
    (text, parsed)
}

/// One disk of a VM's configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiskRef {
    pub slot: String,
    pub role: VolumeRole,
    /// The Proxmox volume ID, `storage:volume`.
    pub volume_id: String,
    pub boot: bool,
}

/// The disks a PIT must capture, from the parsed current configuration:
/// every bus disk, the EFI vars and TPM state volumes, never a CD-ROM or an
/// unused volume. A disk that isn't a storage volume (a passed-through
/// device) can't be captured, and is an error.
pub fn disks(config: &BTreeMap<String, String>) -> Result<Vec<DiskRef>, String> {
    let boot_order: Vec<&str> = config
        .get("boot")
        .and_then(|b| b.split(',').find_map(|p| p.strip_prefix("order=")))
        .map(|o| o.split(';').collect())
        .unwrap_or_default();
    let mut out = Vec::new();
    for (slot, value) in config {
        let role = if slot == "efidisk0" {
            VolumeRole::EfiVars
        } else if slot == "tpmstate0" {
            VolumeRole::TpmState
        } else if ["scsi", "virtio", "sata", "ide"].iter().any(|bus| {
            slot.strip_prefix(bus)
                .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
        }) {
            VolumeRole::Disk
        } else {
            continue;
        };
        let mut parts = value.split(',');
        let volume = parts.next().unwrap_or_default().trim();
        if parts.any(|p| p.trim() == "media=cdrom") || volume == "none" || volume.is_empty() {
            continue;
        }
        if volume.starts_with('/') || !volume.contains(':') {
            return Err(format!(
                "{slot} is {volume}, not a storage volume; a PIT can't capture it"
            ));
        }
        out.push(DiskRef {
            slot: slot.clone(),
            role,
            volume_id: volume.to_string(),
            boot: role != VolumeRole::Disk || boot_order.contains(&slot.as_str()),
        });
    }
    if out.is_empty() {
        return Err("the VM has no disks to capture".into());
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn disks_are_bus_disks_efi_and_tpm_never_cdroms() {
        let (_, config) = current_config(
            "boot: order=scsi0;net0\nscsi0: local-zfs:vm-100-disk-1,size=32G\nscsi1: bmac-zfs-64k:vm-100-disk-2,size=8G\nide2: local:iso/x.iso,media=cdrom\nefidisk0: local-zfs:vm-100-disk-0,efitype=4m\nunused0: local-zfs:vm-100-disk-9\nnet0: virtio=AA:BB:CC:DD:EE:FF,bridge=vmbr1\n",
        );
        let found = disks(&config).unwrap();
        let slots: Vec<(&str, VolumeRole, bool)> = found
            .iter()
            .map(|d| (d.slot.as_str(), d.role, d.boot))
            .collect();
        assert_eq!(
            slots,
            vec![
                ("efidisk0", VolumeRole::EfiVars, true),
                ("scsi0", VolumeRole::Disk, true),
                ("scsi1", VolumeRole::Disk, false),
            ]
        );
        assert_eq!(found[2].volume_id, "bmac-zfs-64k:vm-100-disk-2");
        let (_, passthrough) = current_config("scsi0: /dev/disk/by-id/ata-X,size=1T\n");
        assert!(
            disks(&passthrough)
                .unwrap_err()
                .contains("not a storage volume")
        );
        assert!(disks(&BTreeMap::new()).is_err());
    }

    pub(crate) fn sample() -> Manifest {
        let (config_text, config) = current_config(
            "#comment\nboot: order=scsi0\nscsi0: local-zfs:vm-100-disk-0,size=32G\nbios: ovmf\n\n[snap1]\nscsi0: old\n",
        );
        Manifest {
            schema_version: SCHEMA_VERSION,
            cluster_uuid: "11111111-2222-3333-4444-555555555555".into(),
            vm_uuid: "aaaaaaaa-2222-3333-4444-555555555555".into(),
            vmid: 100,
            pit_uuid: "bbbbbbbb-2222-3333-4444-555555555555".into(),
            branch_uuid: "cccccccc-2222-3333-4444-555555555555".into(),
            parent_pit_uuid: None,
            snapshot: PitName::new(1_791_860_400, 1),
            taken_at_s: 1_791_860_400,
            consistency: Consistency::Filesystem,
            evidence: json!({"fsfreeze": 2}),
            multi_pool: false,
            versions: Versions {
                proxmox: "9.0.3".into(),
                openzfs: "2.3.3".into(),
                bmac: "3.0.0".into(),
            },
            config_sha256: sha256_hex(config_text.as_bytes()),
            config_text,
            config,
            volumes: vec![Volume {
                volume_uuid: "dddddddd-2222-3333-4444-555555555555".into(),
                slot: "scsi0".into(),
                role: VolumeRole::Disk,
                dataset: "rpool/data/vm-100-disk-0".into(),
                snapshot_guid: SnapshotGuid::new(18_446_744_073_709_551_000),
                volsize: 32 << 30,
                volblocksize: 16384,
                boot: true,
            }],
            firmware: Firmware {
                bios: "ovmf".into(),
                machine: None,
                secure_boot: false,
            },
            registry: json!({"name": "prod1", "b": 1, "a": [3, {"z": 1, "y": 2}]}),
            prerequisites: vec![],
            sha256: String::new(),
        }
    }

    #[test]
    fn a_sealed_manifest_opens_and_any_change_is_caught() {
        let mut m = sample();
        let text = m.seal();
        assert!(!text.contains(": ") || text.contains("scsi0: local-zfs"));
        let opened = Manifest::open(&text).unwrap();
        assert_eq!(opened, m);
        assert!(text.contains("\"snapshot_guid\":\"18446744073709551000\""));

        let tampered = text.replace("\"vmid\":100", "\"vmid\":101");
        assert_eq!(Manifest::open(&tampered), Err(ManifestError::Digest));
        let mut wrong = m.clone();
        wrong.config_text.push_str("net0: x\n");
        wrong.seal();
        assert_eq!(wrong.check(), Err(ManifestError::ConfigDigest));
        let mut empty = m.clone();
        empty.volumes.clear();
        empty.seal();
        assert_eq!(empty.check(), Err(ManifestError::NoVolumes));
    }

    #[test]
    fn the_digest_ignores_key_order() {
        let a = json!({"b": 1, "a": {"d": [1, 2], "c": null}});
        let b: Value = serde_json::from_str(r#"{"a":{"c":null,"d":[1,2]},"b":1}"#).unwrap();
        assert_eq!(canonical(&a), r#"{"a":{"c":null,"d":[1,2]},"b":1}"#);
        assert_eq!(canonical(&a), canonical(&b));
        let mut m = sample();
        let first = m.digest();
        m.registry = json!({"a": [3, {"y": 2, "z": 1}], "b": 1, "name": "prod1"});
        assert_eq!(m.digest(), first);
    }

    #[test]
    fn only_current_settings_are_kept() {
        let (text, parsed) = current_config(
            "boot: order=scsi0\n# note\nscsi0: local-zfs:vm-100-disk-0\n[PENDING]\nmemory: 9\n[snap]\nmemory: 1\n",
        );
        assert_eq!(text, "boot: order=scsi0\nscsi0: local-zfs:vm-100-disk-0\n");
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed["scsi0"], "local-zfs:vm-100-disk-0");
    }
}
