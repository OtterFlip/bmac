// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

//! `lsblk` and `smartctl`.

use std::collections::BTreeMap;
use std::sync::LazyLock;
use std::time::Duration;

use bmac_exec::{Command, CommandRunner, ExecError};
use regex::Regex;
use serde::Serialize;
use serde_json::Value;

use crate::{AdapterError, programs, run_text};

/// The columns BMAC reads from `lsblk`.
pub const LSBLK_COLUMNS: &str =
    "NAME,KNAME,TYPE,SIZE,SERIAL,MODEL,WWN,TRAN,FSTYPE,UUID,MOUNTPOINTS";

static NOT_PHYSICAL_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^/dev/(zd|rbd|nbd|zram|ram)").unwrap());

/// One block device from `lsblk -J -b -p`, with its place in the tree.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct BlockDevice {
    /// The display name, such as `/dev/mapper/crypt-rpool-a`.
    pub name: Option<String>,
    /// The kernel name, such as `/dev/dm-0`; the index key.
    pub kname: String,
    #[serde(rename = "type")]
    pub kind: Option<String>,
    pub size: Option<u64>,
    pub serial: Option<String>,
    pub model: Option<String>,
    pub wwn: Option<String>,
    pub tran: Option<String>,
    pub fstype: Option<String>,
    pub uuid: Option<String>,
    pub mountpoints: Vec<String>,
    pub parent: Option<String>,
    /// The kernel name of the whole disk this device is on.
    pub disk: Option<String>,
    pub children: Vec<String>,
}

impl BlockDevice {
    /// How the disk is recognized: its WWN, or its serial when it has none.
    pub fn identity(&self) -> Option<bmac_core::DiskIdentity> {
        bmac_core::DiskIdentity::from_reported(self.wwn.as_deref(), self.serial.as_deref())
    }
}

/// Every block device, keyed by kernel name (v2 `host_storage.py`,
/// `parse_lsblk`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct BlockDevices(pub BTreeMap<String, BlockDevice>);

fn text(node: &Value, key: &str) -> Option<String> {
    node.get(key).and_then(Value::as_str).map(str::to_string)
}

fn trimmed(node: &Value, key: &str) -> Option<String> {
    text(node, key)
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn kname_of(node: &Value) -> Option<String> {
    text(node, "kname")
        .filter(|value| !value.is_empty())
        .or_else(|| text(node, "name").filter(|value| !value.is_empty()))
}

impl BlockDevices {
    /// Parses `lsblk -J -b -p -o` [`LSBLK_COLUMNS`].
    pub fn parse(json: &str) -> Result<BlockDevices, String> {
        let data: Value = serde_json::from_str(json).map_err(|e| format!("invalid JSON: {e}"))?;
        let mut devices = BTreeMap::new();
        let roots = data
            .get("blockdevices")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        for root in &roots {
            visit(root, None, None, &mut devices)?;
        }
        Ok(BlockDevices(devices))
    }

    pub fn get(&self, kname: &str) -> Option<&BlockDevice> {
        self.0.get(kname)
    }

    /// The device and everything below it, depth first.
    pub fn subtree(&self, kname: &str) -> Vec<&BlockDevice> {
        let Some(device) = self.0.get(kname) else {
            return Vec::new();
        };
        let mut found = vec![device];
        for child in &device.children {
            found.extend(self.subtree(child));
        }
        found
    }

    /// Whether the device or anything below it is mounted or active swap.
    pub fn subtree_mounted(&self, kname: &str) -> bool {
        self.subtree(kname)
            .iter()
            .any(|device| !device.mountpoints.is_empty())
    }

    /// The whole disk a device name or kernel name belongs to, for labels.
    pub fn disk_of(&self, name: &str) -> Option<&BlockDevice> {
        let device = self
            .0
            .get(name)
            .or_else(|| self.0.values().find(|d| d.name.as_deref() == Some(name)))?;
        self.0.get(device.disk.as_deref()?)
    }

    /// Whole physical disks: what `smartctl` is asked about. zvols, RBD,
    /// NBD, zram, and RAM disks are skipped.
    pub fn physical_disks(&self) -> Vec<&BlockDevice> {
        self.0
            .values()
            .filter(|device| device.kind.as_deref() == Some("disk") && device.parent.is_none())
            .filter(|device| !NOT_PHYSICAL_RE.is_match(device.name.as_deref().unwrap_or_default()))
            .collect()
    }
}

/// `nvme0n1 SERIAL`: how a finding names a disk.
pub fn disk_label(disk: &BlockDevice) -> String {
    let name = disk.name.as_deref().unwrap_or(&disk.kname);
    format!(
        "{} {}",
        name.strip_prefix("/dev/").unwrap_or(name),
        disk.serial.as_deref().unwrap_or("no-serial")
    )
}

fn visit(
    node: &Value,
    parent: Option<&str>,
    disk: Option<&str>,
    devices: &mut BTreeMap<String, BlockDevice>,
) -> Result<(), String> {
    let Some(kname) = kname_of(node) else {
        return Ok(());
    };
    let kind = text(node, "type");
    let disk = match disk {
        None if kind.as_deref() == Some("disk") => Some(kname.clone()),
        other => other.map(str::to_string),
    };
    let size = match node.get("size") {
        None | Some(Value::Null) => None,
        Some(Value::Number(n)) => Some(
            n.as_u64()
                .ok_or_else(|| format!("{kname} has an invalid size {n}"))?,
        ),
        Some(Value::String(s)) => Some(
            s.trim()
                .parse()
                .map_err(|_| format!("{kname} has an invalid size {s:?}"))?,
        ),
        Some(other) => return Err(format!("{kname} has an invalid size {other}")),
    };
    let mountpoints = node
        .get("mountpoints")
        .and_then(Value::as_array)
        .map(|values| {
            values
                .iter()
                .filter_map(Value::as_str)
                .filter(|value| !value.is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    let children: Vec<&Value> = node
        .get("children")
        .and_then(Value::as_array)
        .map(|values| values.iter().collect())
        .unwrap_or_default();
    devices.insert(
        kname.clone(),
        BlockDevice {
            name: text(node, "name"),
            kname: kname.clone(),
            kind,
            size,
            serial: trimmed(node, "serial"),
            model: trimmed(node, "model"),
            wwn: text(node, "wwn"),
            tran: text(node, "tran"),
            fstype: text(node, "fstype"),
            uuid: text(node, "uuid"),
            mountpoints,
            parent: parent.map(str::to_string),
            disk: disk.clone(),
            children: children
                .iter()
                .filter_map(|child| kname_of(child))
                .collect(),
        },
    );
    for child in children {
        visit(child, Some(&kname), disk.as_deref(), devices)?;
    }
    Ok(())
}

/// A drive's own health assessment.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "health", rename_all = "snake_case")]
pub enum SmartHealth {
    /// The drive reports `PASSED` (ATA, NVMe) or `OK` (SCSI).
    Passed {
        verdict: String,
    },
    /// The drive reports failure, or smartctl's exit status has bit 3 set
    /// ("SMART status check returned DISK FAILING").
    Failing {
        verdict: String,
    },
    /// smartctl ran but could not read an assessment.
    Unavailable {
        exit_code: Option<i32>,
    },
    TimedOut,
    NotInstalled,
}

impl SmartHealth {
    /// Whether the drive needs an operator's attention. Unavailable and
    /// missing assessments are shown but are not findings.
    pub fn is_problem(&self) -> bool {
        matches!(self, SmartHealth::Failing { .. } | SmartHealth::TimedOut)
    }
}

/// Interprets `smartctl -j -H` with v2's rules (`show_cluster_health.sh`
/// 807–828): a verdict of passed with no failing bit is healthy; a failing
/// verdict or the failing bit is a failure; anything else is unavailable.
pub fn interpret_smart(exit_code: Option<i32>, json: &str) -> SmartHealth {
    let report: Value = serde_json::from_str(json).unwrap_or(Value::Null);
    let passed = report
        .get("smart_status")
        .and_then(|status| status.get("passed"))
        .and_then(Value::as_bool);
    let failing_bit = exit_code.is_some_and(|code| code & 8 != 0);
    match (passed, failing_bit) {
        (Some(true), false) => SmartHealth::Passed {
            verdict: if report.pointer("/device/protocol").and_then(Value::as_str) == Some("SCSI") {
                "OK".to_string()
            } else {
                "PASSED".to_string()
            },
        },
        (Some(false), _) => SmartHealth::Failing {
            verdict: "FAILED".to_string(),
        },
        (_, true) => SmartHealth::Failing {
            verdict: "FAILING".to_string(),
        },
        (None, false) => SmartHealth::Unavailable { exit_code },
    }
}

/// The wear counters whose growth predicts a failure: ATA reallocated
/// (attribute 5) and pending (197) sectors, and NVMe media errors.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct SmartCounters {
    pub reallocated_sectors: Option<u64>,
    pub pending_sectors: Option<u64>,
    pub media_errors: Option<u64>,
}

/// Reads [`SmartCounters`] from `smartctl -j -A`.
pub fn smart_counters(json: &str) -> SmartCounters {
    let report: Value = serde_json::from_str(json).unwrap_or(Value::Null);
    let attribute = |id: u64| {
        report
            .pointer("/ata_smart_attributes/table")?
            .as_array()?
            .iter()
            .find(|a| a.get("id").and_then(Value::as_u64) == Some(id))?
            .pointer("/raw/value")?
            .as_u64()
    };
    SmartCounters {
        reallocated_sectors: attribute(5),
        pending_sectors: attribute(197),
        media_errors: report
            .pointer("/nvme_smart_health_information_log/media_errors")
            .and_then(Value::as_u64),
    }
}

/// `lsblk` and `smartctl`.
pub struct Disks<'a> {
    runner: &'a dyn CommandRunner,
}

impl<'a> Disks<'a> {
    pub fn new(runner: &'a dyn CommandRunner) -> Disks<'a> {
        Disks { runner }
    }

    pub async fn block_devices_text(&self) -> Result<String, AdapterError> {
        run_text(
            self.runner,
            Command::new(programs::LSBLK).args(["-J", "-b", "-p", "-o", LSBLK_COLUMNS]),
        )
        .await
    }

    pub async fn block_devices(&self) -> Result<BlockDevices, AdapterError> {
        let command = Command::new(programs::LSBLK).args(["-J", "-b", "-p", "-o", LSBLK_COLUMNS]);
        let text = run_text(self.runner, command.clone()).await?;
        BlockDevices::parse(&text).map_err(|e| AdapterError::parse(&command, e))
    }

    /// The drive's SMART health. Every exit status is accepted, since
    /// smartctl reports findings as bits of it.
    pub async fn smart_health(&self, disk: &str) -> SmartHealth {
        self.smart(disk, "-H").await.0
    }

    /// The drive's SMART health and wear counters, from one `smartctl` run.
    pub async fn smart_report(&self, disk: &str) -> (SmartHealth, SmartCounters) {
        self.smart(disk, "-HA").await
    }

    async fn smart(&self, disk: &str, flags: &str) -> (SmartHealth, SmartCounters) {
        let all: Vec<i32> = (0..=255).collect();
        let command = Command::new(programs::SMARTCTL)
            .args(["-j", flags, disk])
            .timeout(Duration::from_secs(30))
            .expect_codes(&all);
        match crate::run(self.runner, command).await {
            Ok(output) => {
                let text = output.stdout_text();
                (interpret_smart(output.code(), &text), smart_counters(&text))
            }
            Err(AdapterError::NotInstalled { .. }) => {
                (SmartHealth::NotInstalled, SmartCounters::default())
            }
            Err(AdapterError::Exec(ExecError::TimedOut { .. })) => {
                (SmartHealth::TimedOut, SmartCounters::default())
            }
            Err(_) => (
                SmartHealth::Unavailable { exit_code: None },
                SmartCounters::default(),
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use bmac_exec::{FakeRunner, Reply};

    use super::*;

    const NVME_PASSED: &str = r#"{"json_format_version":[1,0],"smartctl":{"version":[7,3],"exit_status":0},"device":{"name":"/dev/nvme0","type":"nvme","protocol":"NVMe"},"smart_status":{"passed":true,"nvme":{"value":0}}}"#;
    const SCSI_PASSED: &str = r#"{"smartctl":{"exit_status":0},"device":{"name":"/dev/sda","type":"scsi","protocol":"SCSI"},"smart_status":{"passed":true}}"#;
    const ATA_FAILED: &str = r#"{"smartctl":{"exit_status":8},"device":{"name":"/dev/sdb","type":"sat","protocol":"ATA"},"smart_status":{"passed":false}}"#;
    const NO_ASSESSMENT: &str = r#"{"smartctl":{"exit_status":2,"messages":[{"string":"Smartctl open device: /dev/sdc failed: No such device","severity":"error"}]}}"#;

    #[test]
    fn wear_counters_come_from_ata_attributes_and_the_nvme_log() {
        let ata = r#"{"ata_smart_attributes":{"table":[{"id":5,"raw":{"value":8}},{"id":197,"raw":{"value":2}},{"id":9,"raw":{"value":100}}]}}"#;
        assert_eq!(
            smart_counters(ata),
            SmartCounters {
                reallocated_sectors: Some(8),
                pending_sectors: Some(2),
                media_errors: None
            }
        );
        let nvme = r#"{"nvme_smart_health_information_log":{"media_errors":3}}"#;
        assert_eq!(smart_counters(nvme).media_errors, Some(3));
        assert_eq!(smart_counters(SCSI_PASSED), SmartCounters::default());
    }

    #[test]
    fn smart_follows_v2s_interpretation() {
        assert_eq!(
            interpret_smart(Some(0), NVME_PASSED),
            SmartHealth::Passed {
                verdict: "PASSED".into()
            }
        );
        assert_eq!(
            interpret_smart(Some(0), SCSI_PASSED),
            SmartHealth::Passed {
                verdict: "OK".into()
            }
        );
        assert_eq!(
            interpret_smart(Some(8), ATA_FAILED),
            SmartHealth::Failing {
                verdict: "FAILED".into()
            }
        );
        assert_eq!(
            interpret_smart(Some(8), SCSI_PASSED),
            SmartHealth::Failing {
                verdict: "FAILING".into()
            },
            "the failing bit wins over a passed verdict"
        );
        assert_eq!(
            interpret_smart(Some(4), NVME_PASSED),
            SmartHealth::Passed {
                verdict: "PASSED".into()
            },
            "other bits are not failures"
        );
        assert_eq!(
            interpret_smart(Some(2), NO_ASSESSMENT),
            SmartHealth::Unavailable { exit_code: Some(2) }
        );
        assert_eq!(
            interpret_smart(Some(0), "not json"),
            SmartHealth::Unavailable { exit_code: Some(0) }
        );
    }

    #[tokio::test]
    async fn smart_accepts_every_exit_status_and_reports_timeouts() {
        let fake = FakeRunner::new();
        fake.expect(
            programs::SMARTCTL,
            ["-j", "-H", "/dev/sdb"],
            Reply::ok(ATA_FAILED).code(8),
        )
        .expect(
            programs::SMARTCTL,
            ["-j", "-H", "/dev/sdc"],
            Reply::timeout(),
        );
        let disks = Disks::new(&fake);
        assert!(disks.smart_health("/dev/sdb").await.is_problem());
        assert_eq!(disks.smart_health("/dev/sdc").await, SmartHealth::TimedOut);
        assert_eq!(fake.calls()[0].timeout, Duration::from_secs(30));
        fake.assert_done();
    }

    #[test]
    fn lsblk_tree_records_parents_disks_and_sizes_in_either_form() {
        let json = r#"{"blockdevices":[
            {"name":"/dev/nvme0n1","kname":"/dev/nvme0n1","type":"disk","size":2000398934016,
             "serial":" S0 ","model":"Fake NVMe","mountpoints":[null],"children":[
               {"name":"/dev/nvme0n1p3","kname":"/dev/nvme0n1p3","type":"part","size":"1000",
                "mountpoints":[null],"children":[
                  {"name":"/dev/mapper/crypt-rpool-a","kname":"/dev/dm-0","type":"crypt",
                   "mountpoints":["/mnt"]}]}]},
            {"name":"/dev/zd0","kname":"/dev/zd0","type":"disk"}]}"#;
        let devices = BlockDevices::parse(json).unwrap();
        let crypt = devices.get("/dev/dm-0").unwrap();
        assert_eq!(crypt.parent.as_deref(), Some("/dev/nvme0n1p3"));
        assert_eq!(crypt.disk.as_deref(), Some("/dev/nvme0n1"));
        assert_eq!(devices.get("/dev/nvme0n1p3").unwrap().size, Some(1000));
        let disk = devices.get("/dev/nvme0n1").unwrap();
        assert_eq!(disk.serial.as_deref(), Some("S0"));
        assert_eq!(disk.children, ["/dev/nvme0n1p3"]);
        assert!(devices.subtree_mounted("/dev/nvme0n1"));
        assert_eq!(
            disk_label(devices.disk_of("/dev/mapper/crypt-rpool-a").unwrap()),
            "nvme0n1 S0"
        );
        assert_eq!(
            devices
                .physical_disks()
                .iter()
                .map(|d| &d.kname[..])
                .collect::<Vec<_>>(),
            ["/dev/nvme0n1"]
        );
        assert!(BlockDevices::parse(r#"{"blockdevices":[{"kname":"x","size":"big"}]}"#).is_err());
    }
}
