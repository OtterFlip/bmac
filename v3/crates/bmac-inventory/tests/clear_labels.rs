// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

//! `clear-zfs-labels` against fixture devices (`IMPLEMENTATION_PLAN.md`
//! M6.3): refuses on any mismatch without touching a device, clears only
//! the labelled ones, and reports a label that survives.

use std::cell::RefCell;
use std::io::Write;
use std::path::{Path, PathBuf};

use bmac_inventory::clear::{LabelClearer, ZpoolLabelClear, clear_zfs_labels};
use bmac_inventory::fixture::{DiskSpec, Fixture};
use bmac_inventory::zfs::{member_label, read_label, zero_labels};
use bmac_proto::inventory::{ClearReport, ClearRequest, ClearTarget};

const MIB: u64 = 1024 * 1024;
const OLD_GUID: u64 = 9_876_543_210_123_456_789;

/// Zeroes labels as `zpool labelclear` does, except on devices it is told
/// to leave alone, and records every call.
#[derive(Default)]
struct FakeClear {
    calls: RefCell<Vec<PathBuf>>,
    stubborn: Vec<PathBuf>,
    failing: Vec<PathBuf>,
}

impl LabelClearer for FakeClear {
    fn clear(&self, device: &Path, start: u64) -> Result<(), String> {
        self.calls.borrow_mut().push(device.to_path_buf());
        if self.failing.iter().any(|d| d == device) {
            return Err("failed to clear label".into());
        }
        if !self.stubborn.iter().any(|d| d == device) {
            zero_labels(device, start).unwrap();
        }
        Ok(())
    }
}

struct Lab {
    _dir: tempfile::TempDir,
    fx: Fixture,
    wwn: String,
}

/// Two disks: one an old raw `rpool` member, one a data disk from another
/// pool, plus a clean disk.
fn lab() -> Lab {
    let dir = tempfile::tempdir().unwrap();
    let mut fx = Fixture::new(dir.path());
    let old = DiskSpec::nvme("nvme1n1", "OLDRPOOL", 4 * 1024 * MIB).proxmox_layout();
    let wwn = old.wwn.clone().unwrap();
    fx.disk(&old);
    fx.label("nvme1n1p3", &member_label("rpool", OLD_GUID, 11, 0, 500));
    fx.disk(&DiskSpec::sata("sda", "TANKDISK", 512 * MIB));
    fx.label("sda", &member_label("tank", 77, 12, 1, 90));
    fx.disk(&DiskSpec::nvme("nvme0n1", "CLEAN", 4 * 1024 * MIB));
    Lab { _dir: dir, fx, wwn }
}

fn target(serial: &str, wwn: Option<&str>, guid: u64) -> ClearRequest {
    ClearRequest {
        disks: vec![ClearTarget {
            serial: serial.into(),
            wwn: wwn.map(str::to_string),
            pool_guid: guid.to_string(),
        }],
    }
}

fn labelled(lab: &Lab, device: &str) -> bool {
    read_label(&lab.fx.device(device), 0).unwrap().is_some()
}

#[test]
fn clears_only_the_confirmed_pools_labels_and_proves_it() {
    let lab = lab();
    let clear = FakeClear::default();
    let report = clear_zfs_labels(
        &lab.fx.root(),
        &target("OLDRPOOL", Some(&lab.wwn), OLD_GUID),
        &clear,
    );
    assert!(report.succeeded(), "{report:?}");
    let device = lab.fx.device("nvme1n1p3");
    assert_eq!(*clear.calls.borrow(), std::slice::from_ref(&device));
    assert_eq!(report.cleared, [device.display().to_string()]);
    assert!(!labelled(&lab, "nvme1n1p3"));
    assert!(labelled(&lab, "sda"), "the other pool's label is untouched");
}

#[test]
fn the_wwn_is_compared_without_regard_to_case() {
    let lab = lab();
    let report = clear_zfs_labels(
        &lab.fx.root(),
        &target("OLDRPOOL", Some(&lab.wwn.to_uppercase()), OLD_GUID),
        &FakeClear::default(),
    );
    assert!(report.succeeded(), "{report:?}");
}

fn refused(lab: &Lab, request: &ClearRequest) -> ClearReport {
    let clear = FakeClear::default();
    let report = clear_zfs_labels(&lab.fx.root(), request, &clear);
    assert!(
        clear.calls.borrow().is_empty(),
        "nothing may be touched: {report:?}"
    );
    assert!(labelled(lab, "nvme1n1p3"));
    assert!(!report.refused.is_empty());
    assert!(!report.succeeded());
    report
}

#[test]
fn refuses_an_unknown_serial() {
    let lab = lab();
    let report = refused(&lab, &target("NOSUCH", Some(&lab.wwn), OLD_GUID));
    assert_eq!(report.refused, ["No disk reports serial NOSUCH."]);
}

#[test]
fn refuses_a_wwn_mismatch() {
    let lab = lab();
    let report = refused(
        &lab,
        &target("OLDRPOOL", Some("eui.0000000000000001"), OLD_GUID),
    );
    assert!(
        report.refused[0].contains("reports WWN"),
        "{:?}",
        report.refused
    );
    refused(&lab, &target("OLDRPOOL", None, OLD_GUID));
}

#[test]
fn refuses_a_pool_guid_mismatch() {
    let lab = lab();
    let report = refused(&lab, &target("OLDRPOOL", Some(&lab.wwn), 1234));
    assert!(report.refused[0].contains("no label of the pool with GUID 1234"));
}

#[test]
fn refuses_everything_when_one_disk_of_several_mismatches() {
    let lab = lab();
    let mut request = target("OLDRPOOL", Some(&lab.wwn), OLD_GUID);
    request.disks.push(ClearTarget {
        serial: "CLEAN".into(),
        wwn: None,
        pool_guid: OLD_GUID.to_string(),
    });
    refused(&lab, &request);
}

#[test]
fn refuses_a_serial_two_disks_share() {
    let dir = tempfile::tempdir().unwrap();
    let mut fx = Fixture::new(dir.path());
    for name in ["sda", "sdb"] {
        let mut spec = DiskSpec::sata(name, "TWIN", 512 * MIB);
        spec.wwn = None;
        fx.disk(&spec);
        fx.label(name, &member_label("rpool", 5, 6, 0, 7));
    }
    let clear = FakeClear::default();
    let report = clear_zfs_labels(&fx.root(), &target("TWIN", None, 5), &clear);
    assert!(clear.calls.borrow().is_empty());
    assert!(report.refused[0].contains("2 disks report serial TWIN"));
}

#[test]
fn refuses_an_empty_request() {
    let lab = lab();
    refused(&lab, &ClearRequest { disks: vec![] });
}

#[test]
fn reports_a_label_that_survives() {
    let lab = lab();
    let device = lab.fx.device("nvme1n1p3");
    let clear = FakeClear {
        stubborn: vec![device.clone()],
        ..FakeClear::default()
    };
    let report = clear_zfs_labels(
        &lab.fx.root(),
        &target("OLDRPOOL", Some(&lab.wwn), OLD_GUID),
        &clear,
    );
    assert_eq!(report.survived, [device.display().to_string()]);
    assert!(report.cleared.is_empty());
    assert!(!report.succeeded());
}

#[test]
fn reports_a_failed_labelclear() {
    let lab = lab();
    let device = lab.fx.device("nvme1n1p3");
    let clear = FakeClear {
        failing: vec![device.clone()],
        stubborn: vec![device.clone()],
        ..FakeClear::default()
    };
    let report = clear_zfs_labels(
        &lab.fx.root(),
        &target("OLDRPOOL", Some(&lab.wwn), OLD_GUID),
        &clear,
    );
    assert!(report.errors[0].contains("failed to clear label"));
    assert_eq!(report.survived.len(), 1);
}

#[test]
fn zeroes_the_labels_even_when_labelclear_refuses() {
    let lab = lab();
    let bin = tempfile::tempdir().unwrap();
    let zpool = bin.path().join("zpool");
    std::fs::write(
        &zpool,
        "#!/bin/sh\necho 'failed to clear label' >&2\nexit 1\n",
    )
    .unwrap();
    std::fs::set_permissions(&zpool, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    let report = clear_zfs_labels(
        &lab.fx.root(),
        &target("OLDRPOOL", Some(&lab.wwn), OLD_GUID),
        &ZpoolLabelClear::with(Some(zpool)),
    );
    assert!(report.succeeded(), "{report:?}");
    assert!(!labelled(&lab, "nvme1n1p3"));
}

#[test]
fn the_binary_reads_the_request_on_stdin_and_refuses_a_mismatch() {
    let lab = lab();
    let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_bmac-inventory"))
        .args(["clear-zfs-labels", "--root", lab.fx.base.to_str().unwrap()])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let request =
        serde_json::to_vec(&target("OLDRPOOL", Some("eui.0000000000000001"), OLD_GUID)).unwrap();
    child.stdin.take().unwrap().write_all(&request).unwrap();
    let output = child.wait_with_output().unwrap();
    assert_eq!(output.status.code(), Some(1));
    let report: ClearReport = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report.refused.len(), 1);
    assert!(labelled(&lab, "nvme1n1p3"));
}
