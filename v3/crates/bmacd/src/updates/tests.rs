// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

use std::sync::Arc;

use bmac_core::JobId;
use bmac_exec::{FakeRunner, Reply};
use bmac_proto::updates::{InstallMode, InstallRequest, InstallState, InstalledBundle};
use bmac_release::build::TestKey;

use super::install::{Begun, Installer};
use super::*;

const NOW_MS: i64 = 1_767_300_000_000;
const DAY_MS: i64 = 24 * 3600 * 1000;

pub(crate) struct Rig {
    pub dir: tempfile::TempDir,
    pub key: TestKey,
    pub store: UpdateStore,
}

pub(crate) fn rig() -> Rig {
    let dir = tempfile::tempdir().unwrap();
    let key = TestKey::generate();
    let store = UpdateStore::new(dir.path().join("updates"), vec![key.trusted("development")]);
    Rig { dir, key, store }
}

pub(crate) fn host_bundle(key: &TestKey, version: &str) -> Vec<u8> {
    let deb = format!("bmacd {version}").into_bytes();
    let debian = bmac_release::debian_version(version);
    key.bundle(
        version,
        &[("bmacd", &format!("bmacd_{debian}_amd64.deb"), &debian, &deb)],
    )
}

impl Rig {
    /// Uploads `bytes` in chunks of `chunk`, as the Dashboard does.
    pub(crate) fn upload(&self, bytes: &[u8], chunk: usize) -> Result<BundleSummary, String> {
        let id = self.store.start_upload(bytes.len() as u64, NOW_MS)?;
        let mut offset = 0u64;
        for piece in bytes.chunks(chunk) {
            offset = self.store.append_upload(&id, offset, piece)?;
        }
        self.store.finish_upload(&id, NOW_MS)
    }

    pub(crate) fn stage(&self, version: &str) -> BundleSummary {
        self.upload(&host_bundle(&self.key, version), 1 << 20)
            .unwrap()
    }
}

#[test]
fn a_bundle_uploaded_in_chunks_is_checked_and_kept() {
    let rig = rig();
    let bytes = host_bundle(&rig.key, "3.0.0-dev.0+b1");
    let summary = rig.upload(&bytes, 700).unwrap();
    assert_eq!(summary.version, "3.0.0-dev.0+b1");
    assert_eq!(summary.signed_by, "development");
    assert_eq!(summary.size, bytes.len() as u64);
    assert_eq!(summary.packages[0].name, "bmacd");
    assert!(rig.store.has(&summary.sha256));
    assert_eq!(rig.store.staged(), vec![summary.clone()]);
    // The same bundle again keeps the first copy.
    let again = rig.upload(&bytes, 1 << 20).unwrap();
    assert_eq!(again, summary);
    assert_eq!(rig.store.staged().len(), 1);
    // No upload is left behind.
    assert_eq!(
        std::fs::read_dir(rig.store.uploads_dir().unwrap())
            .unwrap()
            .count(),
        0
    );
}

impl Rig {
    fn seed(&self, name: &str, bytes: &[u8]) {
        let seed = self.store.root.join("seed");
        std::fs::create_dir_all(&seed).unwrap();
        std::fs::write(seed.join(name), bytes).unwrap();
    }
}

#[test]
fn the_bundle_a_host_was_installed_from_is_kept_as_its_current_one() {
    let rig = rig();
    assert_eq!(rig.store.adopt_seed("3.0.0", NOW_MS).unwrap(), None);
    rig.seed("bmac-host-3.0.0.bundle", &host_bundle(&rig.key, "3.0.0"));
    let adopted = rig.store.adopt_seed("3.0.0", NOW_MS).unwrap().unwrap();
    assert_eq!(adopted.version, "3.0.0");
    assert!(rig.store.has(&adopted.sha256));
    assert_eq!(
        rig.store.installed(),
        Installed {
            current: Some(InstalledBundle {
                sha256: adopted.sha256.clone(),
                version: "3.0.0".into()
            }),
            previous: None,
        }
    );
    assert!(!rig.store.root.join("seed").exists());

    // Once something is recorded, a later seed is only removed.
    rig.seed("bmac-host-3.0.1.bundle", &host_bundle(&rig.key, "3.0.1"));
    assert_eq!(rig.store.adopt_seed("3.0.1", NOW_MS).unwrap(), None);
    assert_eq!(rig.store.installed().current.unwrap().version, "3.0.0");
    assert_eq!(rig.store.staged().len(), 1);
    assert!(!rig.store.root.join("seed").exists());
}

#[test]
fn a_seed_of_another_build_or_key_is_not_kept() {
    let rig = rig();
    rig.seed("old.bundle", &host_bundle(&rig.key, "2.9.0"));
    let error = rig.store.adopt_seed("3.0.0", NOW_MS).unwrap_err();
    assert!(error.contains("not the running 3.0.0"), "{error}");
    assert_eq!(rig.store.installed(), Installed::default());
    assert!(rig.store.staged().is_empty());

    rig.seed(
        "stranger.bundle",
        &host_bundle(&TestKey::generate(), "3.0.0"),
    );
    let error = rig.store.adopt_seed("3.0.0", NOW_MS).unwrap_err();
    assert!(error.contains("trust"), "{error}");
    assert_eq!(rig.store.installed(), Installed::default());
    assert!(!rig.store.root.join("seed").exists());
    assert_eq!(
        std::fs::read_dir(rig.store.uploads_dir().unwrap())
            .unwrap()
            .count(),
        0
    );
}

#[test]
fn chunks_must_arrive_in_order_and_fill_the_upload_exactly() {
    let rig = rig();
    let id = rig.store.start_upload(10, NOW_MS).unwrap();
    assert!(
        rig.store
            .append_upload(&id, 5, b"abc")
            .unwrap_err()
            .contains("0 bytes")
    );
    assert_eq!(rig.store.append_upload(&id, 0, b"abcd").unwrap(), 4);
    assert!(
        rig.store
            .append_upload(&id, 4, b"0123456789")
            .unwrap_err()
            .contains("past")
    );
    assert!(
        rig.store
            .finish_upload(&id, NOW_MS)
            .unwrap_err()
            .contains("4 of its 10")
    );
    assert!(rig.store.append_upload("../../etc", 0, b"x").is_err());
    assert!(rig.store.start_upload(0, NOW_MS).is_err());
    assert!(
        rig.store
            .start_upload(bmac_release::MAX_BUNDLE + 1, NOW_MS)
            .is_err()
    );
}

#[test]
fn a_bundle_signed_by_an_unknown_key_or_without_bmacd_is_refused() {
    let rig = rig();
    let stranger = TestKey::generate();
    let error = rig
        .upload(&host_bundle(&stranger, "3.0.1"), 1 << 20)
        .unwrap_err();
    assert!(error.contains("trust"), "{error}");
    let appliance_only = rig.key.bundle(
        "3.0.1",
        &[(
            "bmac-backupd",
            "bmac-backupd_3.0.1_amd64.deb",
            "3.0.1",
            b"x",
        )],
    );
    let error = rig.upload(&appliance_only, 1 << 20).unwrap_err();
    assert!(error.contains("no bmacd"), "{error}");
    assert!(rig.store.staged().is_empty());
    assert_eq!(
        std::fs::read_dir(rig.store.uploads_dir().unwrap())
            .unwrap()
            .count(),
        0
    );
}

#[test]
fn bundles_needed_for_a_rollback_are_kept() {
    let rig = rig();
    let a = rig.stage("3.0.0+b1");
    let b = rig.stage("3.0.0+b2");
    let c = rig.stage("3.0.0+b3");
    rig.store
        .record_installed(&Installed {
            current: Some(InstalledBundle {
                sha256: b.sha256.clone(),
                version: b.version.clone(),
            }),
            previous: Some(InstalledBundle {
                sha256: a.sha256.clone(),
                version: a.version.clone(),
            }),
        })
        .unwrap();
    assert!(
        rig.store
            .delete(&a.sha256)
            .unwrap_err()
            .contains("rollback")
    );
    assert!(rig.store.delete(&b.sha256).is_err());
    rig.store.prune(NOW_MS + 31 * DAY_MS);
    assert!(rig.store.has(&a.sha256) && rig.store.has(&b.sha256));
    assert!(!rig.store.has(&c.sha256));
    assert!(
        rig.store
            .delete(&c.sha256)
            .unwrap_err()
            .contains("isn't on")
    );
}

fn installer(rig: &Rig, runner: &Arc<FakeRunner>) -> Installer {
    Installer::new(rig.store.clone(), runner.clone())
}

fn job(n: u8) -> JobId {
    format!("{n:08x}-0000-4000-8000-000000000000")
        .parse()
        .unwrap()
}

fn expect_launch(runner: &FakeRunner, rig: &Rig, job: JobId, mode: &str) {
    let dir = rig
        .store
        .installs_dir()
        .unwrap()
        .join(format!("{job}-{mode}"));
    runner.expect(
        "/usr/bin/systemd-run",
        [
            format!("--unit=bmac-{mode}-{job}"),
            "--collect".into(),
            "--quiet".into(),
            "--setenv=DEBIAN_FRONTEND=noninteractive".into(),
            "--setenv=PATH=/usr/sbin:/usr/bin:/sbin:/bin".into(),
            "/bin/sh".into(),
            "-c".into(),
            include_str!("install_script.sh").into(),
            "sh".into(),
            dir.join("bmacd.deb").display().to_string(),
            dir.display().to_string(),
        ],
        Reply::ok(""),
    );
}

fn finish(rig: &Rig, job: JobId, mode: &str, code: i32, log: &str) {
    let dir = rig
        .store
        .installs_dir()
        .unwrap()
        .join(format!("{job}-{mode}"));
    std::fs::write(dir.join("log"), log).unwrap();
    std::fs::write(dir.join("exit"), format!("{code}\n")).unwrap();
}

#[tokio::test]
async fn an_install_unpacks_bmacd_runs_apt_in_its_own_unit_and_records_the_bundle() {
    let rig = rig();
    let a = rig.stage("3.0.0+b1");
    let b = rig.stage("3.0.0+b2");
    let a_installed = InstalledBundle {
        sha256: a.sha256.clone(),
        version: a.version.clone(),
    };
    rig.store
        .record_installed(&Installed {
            current: Some(a_installed.clone()),
            previous: None,
        })
        .unwrap();
    let runner = Arc::new(FakeRunner::new());
    let installer = installer(&rig, &runner);
    let request = InstallRequest {
        job: job(1),
        mode: InstallMode::Update,
        sha256: b.sha256.clone(),
    };
    assert_eq!(installer.begin(&request, NOW_MS).unwrap(), Begun::New);
    let status = installer
        .status(job(1), InstallMode::Update, NOW_MS)
        .unwrap()
        .unwrap();
    assert_eq!(status.state, InstallState::Fetching);
    expect_launch(&runner, &rig, job(1), "update");
    installer.launch(&request).await.unwrap();
    runner.assert_done();
    let dir = rig
        .store
        .installs_dir()
        .unwrap()
        .join(format!("{}-update", job(1)));
    assert_eq!(
        std::fs::read(dir.join("bmacd.deb")).unwrap(),
        b"bmacd 3.0.0+b2"
    );
    let Begun::Existing(status) = installer.begin(&request, NOW_MS).unwrap() else {
        panic!("a second ask starts nothing");
    };
    assert_eq!(status.state, InstallState::Installing);
    assert_eq!(rig.store.installed().current, Some(a_installed.clone()));

    finish(&rig, job(1), "update", 0, "done");
    let status = installer
        .status(job(1), InstallMode::Update, NOW_MS)
        .unwrap()
        .unwrap();
    assert_eq!(status.state, InstallState::Installed);
    let installed = rig.store.installed();
    assert_eq!(installed.current.unwrap().sha256, b.sha256);
    assert_eq!(installed.previous, Some(a_installed));
}

#[tokio::test]
async fn a_failed_install_reports_the_end_of_apts_output_and_records_nothing() {
    let rig = rig();
    let b = rig.stage("3.0.0+b2");
    let runner = Arc::new(FakeRunner::new());
    let installer = installer(&rig, &runner);
    let request = InstallRequest {
        job: job(2),
        mode: InstallMode::Update,
        sha256: b.sha256.clone(),
    };
    installer.begin(&request, NOW_MS).unwrap();
    expect_launch(&runner, &rig, job(2), "update");
    installer.launch(&request).await.unwrap();
    let log = format!("{}E: dpkg was interrupted", "x".repeat(10_000));
    finish(&rig, job(2), "update", 100, &log);
    let status = installer
        .status(job(2), InstallMode::Update, NOW_MS)
        .unwrap()
        .unwrap();
    assert_eq!(status.state, InstallState::Failed);
    assert!(status.message.unwrap().contains("exit status 100"));
    let tail = status.log_tail.unwrap();
    assert!(tail.len() <= 4000 && tail.ends_with("E: dpkg was interrupted"));
    assert_eq!(rig.store.installed(), Installed::default());
}

#[tokio::test]
async fn a_rollback_installs_the_previous_bundle_and_swaps_the_two() {
    let rig = rig();
    let a = rig.stage("3.0.0+b1");
    let b = rig.stage("3.0.0+b2");
    let installed = |sha: &str, version: &str| InstalledBundle {
        sha256: sha.into(),
        version: version.into(),
    };
    rig.store
        .record_installed(&Installed {
            current: Some(installed(&b.sha256, &b.version)),
            previous: Some(installed(&a.sha256, &a.version)),
        })
        .unwrap();
    let runner = Arc::new(FakeRunner::new());
    let installer = installer(&rig, &runner);
    let wrong = InstallRequest {
        job: job(3),
        mode: InstallMode::Rollback,
        sha256: a.sha256.clone(),
    };
    assert!(
        installer
            .begin(&wrong, NOW_MS)
            .unwrap_err()
            .contains("runs bundle")
    );
    let request = InstallRequest {
        sha256: b.sha256.clone(),
        ..wrong
    };
    assert_eq!(installer.needs(&request).unwrap(), a.sha256);
    installer.begin(&request, NOW_MS).unwrap();
    expect_launch(&runner, &rig, job(3), "rollback");
    installer.launch(&request).await.unwrap();
    let dir = rig
        .store
        .installs_dir()
        .unwrap()
        .join(format!("{}-rollback", job(3)));
    assert_eq!(
        std::fs::read(dir.join("bmacd.deb")).unwrap(),
        b"bmacd 3.0.0+b1"
    );
    finish(&rig, job(3), "rollback", 0, "");
    installer
        .status(job(3), InstallMode::Rollback, NOW_MS)
        .unwrap();
    assert_eq!(
        rig.store.installed(),
        Installed {
            current: Some(installed(&a.sha256, &a.version)),
            previous: Some(installed(&b.sha256, &b.version)),
        }
    );
}

#[test]
fn a_host_without_an_earlier_bundle_cant_roll_back() {
    let rig = rig();
    let b = rig.stage("3.0.0+b2");
    let runner = Arc::new(FakeRunner::new());
    let installer = installer(&rig, &runner);
    let request = InstallRequest {
        job: job(4),
        mode: InstallMode::Rollback,
        sha256: b.sha256.clone(),
    };
    assert!(
        installer
            .begin(&request, NOW_MS)
            .unwrap_err()
            .contains("no bundle")
    );
    rig.store
        .record_installed(&Installed {
            current: Some(InstalledBundle {
                sha256: b.sha256.clone(),
                version: b.version.clone(),
            }),
            previous: None,
        })
        .unwrap();
    assert!(
        installer
            .begin(&request, NOW_MS)
            .unwrap_err()
            .contains("no earlier")
    );
}

#[tokio::test]
async fn services_are_read_from_systemctl() {
    let rig = rig();
    let runner = Arc::new(FakeRunner::new());
    runner.expect(
        "/usr/bin/systemctl",
        ["is-active", "bmacd-web.service", "bmac-source.service"],
        Reply::ok("active\ninactive\n").code(3),
    );
    let services = installer(&rig, &runner).services().await;
    assert_eq!(services.get("bmacd-web"), Some(&true));
    assert_eq!(services.get("bmac-source"), Some(&false));
}
