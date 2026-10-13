// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

//! The storage layer against a real directory and a simulated pmxcfs, ported
//! from v2's `test_shared_libs.py` lock and write tests.

use std::fs::File;
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bmac_pmxcfs::{
    Budget, FsOps, LockOptions, Pmxcfs, PmxcfsError, RealFs, RegistryLock, canonical_json,
    read_record, write_record,
};
use rustix::fs::{AtFlags, CWD, Timespec, Timestamps, utimensat};
use serde_json::{Value, json};

/// Refuses what pmxcfs refuses, and emulates its stale-lock break request
/// (v2's `SimulatedPmxcfs`).
struct SimulatedPmxcfs {
    root: PathBuf,
    forbidden: Mutex<Vec<String>>,
    regular_replacements: AtomicUsize,
    stale_break_requests: AtomicUsize,
}

impl SimulatedPmxcfs {
    fn new(root: &Path) -> Arc<SimulatedPmxcfs> {
        Arc::new(SimulatedPmxcfs {
            root: root.to_path_buf(),
            forbidden: Mutex::new(Vec::new()),
            regular_replacements: AtomicUsize::new(0),
            stale_break_requests: AtomicUsize::new(0),
        })
    }

    fn reject(&self, path: &Path, operation: &str) -> io::Result<()> {
        if path.starts_with(&self.root) {
            self.forbidden.lock().unwrap().push(operation.to_string());
            return Err(io::Error::from_raw_os_error(
                rustix::io::Errno::OPNOTSUPP.raw_os_error(),
            ));
        }
        Ok(())
    }

    fn forbidden(&self) -> Vec<String> {
        self.forbidden.lock().unwrap().clone()
    }
}

impl FsOps for SimulatedPmxcfs {
    fn fsync(&self, file: &File, path: &Path) -> io::Result<()> {
        self.reject(path, "fsync")?;
        RealFs.fsync(file, path)
    }

    fn fsync_dir(&self, path: &Path) -> io::Result<()> {
        self.reject(path, "fsync")?;
        RealFs.fsync_dir(path)
    }

    fn chmod(&self, path: &Path, mode: u32) -> io::Result<()> {
        self.reject(path, "chmod")?;
        RealFs.chmod(path, mode)
    }

    fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
        if from.starts_with(&self.root) || to.starts_with(&self.root) {
            if from.is_dir() {
                return self.reject(from, "directory rename");
            }
            self.regular_replacements.fetch_add(1, Ordering::SeqCst);
        }
        RealFs.rename(from, to)
    }

    fn utime_zero(&self, path: &Path) -> io::Result<()> {
        if path.starts_with(&self.root) && path.parent() == Some(&self.root.join("priv/lock")) {
            self.stale_break_requests.fetch_add(1, Ordering::SeqCst);
            if path.is_dir() && std::fs::read_dir(path)?.next().is_none() {
                std::fs::remove_dir(path)?;
            }
            return Ok(());
        }
        RealFs.utime_zero(path)
    }
}

fn set_mtime(path: &Path, seconds: i64) {
    let time = Timespec {
        tv_sec: seconds,
        tv_nsec: 0,
    };
    utimensat(
        CWD,
        path,
        &Timestamps {
            last_access: time,
            last_modification: time,
        },
        AtFlags::empty(),
    )
    .unwrap();
}

fn stale_owner() -> Value {
    json!({
        "schema_version": 1,
        "record_type": "allocator-lock",
        "nonce": "stale-owner",
        "node": "mox1",
        "pid": 1,
        "created_at": "2000-01-01T00:00:00+00:00",
    })
}

fn quick(break_stale: bool) -> LockOptions {
    LockOptions {
        timeout: Duration::ZERO,
        stale_after: Duration::from_secs(1),
        break_stale,
        ..LockOptions::default()
    }
}

fn local(dir: &Path) -> Pmxcfs {
    // A root that the test paths are not under, so they are "not pmxcfs".
    Pmxcfs::new(dir.join("not-pmxcfs"), Arc::new(RealFs))
}

#[test]
fn simulated_pmxcfs_uses_supported_atomic_file_and_lock_operations() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path().join("etc/pve");
    let state = root.join("priv/bmac/registry");
    std::fs::create_dir_all(&root).unwrap();
    let simulated = SimulatedPmxcfs::new(&root);
    let pmxcfs = Pmxcfs::new(&root, simulated.clone());
    pmxcfs.ensure_directory(&state, 0o700).unwrap();

    let lock = RegistryLock::acquire(&pmxcfs, &state, &quick(false)).unwrap();
    assert_eq!(lock.path().parent().unwrap(), root.join("priv/lock"));
    let name = lock
        .path()
        .file_name()
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();
    assert!(
        name.starts_with("bmac-registry-") && name.len() == "bmac-registry-".len() + 16,
        "{name}"
    );
    assert_eq!(std::fs::read_dir(lock.path()).unwrap().count(), 0);
    assert_eq!(lock.owner_path().parent().unwrap(), state);
    pmxcfs
        .write_json(&state.join("probe.json"), &json!({"generation": 1}))
        .unwrap();
    pmxcfs
        .write_json(&state.join("probe.json"), &json!({"generation": 2}))
        .unwrap();
    assert_eq!(std::fs::read_dir(lock.path()).unwrap().count(), 0);
    let (lock_path, owner_path) = (lock.path().to_path_buf(), lock.owner_path().to_path_buf());
    lock.release().unwrap();
    assert!(!lock_path.exists() && !owner_path.exists());

    assert_eq!(
        pmxcfs
            .read_json(&state.join("probe.json"), "probe")
            .unwrap(),
        json!({"generation": 2})
    );
    assert_eq!(simulated.forbidden(), Vec::<String>::new());
    assert!(simulated.regular_replacements.load(Ordering::SeqCst) >= 3);
}

#[test]
fn simulated_pmxcfs_stale_break_uses_safe_unlock_request() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path().join("etc/pve");
    let state = root.join("priv/bmac/registry");
    std::fs::create_dir_all(&root).unwrap();
    let simulated = SimulatedPmxcfs::new(&root);
    let pmxcfs = Pmxcfs::new(&root, simulated.clone());
    pmxcfs.ensure_directory(&state, 0o700).unwrap();
    let stale = RegistryLock::path_for(&pmxcfs, &state, "bmac-registry-").unwrap();
    std::fs::create_dir_all(&stale).unwrap();
    pmxcfs
        .write_json(&state.join(".allocator-lock-owner.json"), &stale_owner())
        .unwrap();
    set_mtime(&stale, 946_684_800);

    let without_break = RegistryLock::acquire(&pmxcfs, &state, &quick(false))
        .err()
        .unwrap();
    assert!(matches!(without_break, PmxcfsError::LockTimeout { .. }));
    assert!(
        without_break
            .to_string()
            .contains("owner mox1 pid=1 since=2000-01-01T00:00:00+00:00"),
        "{without_break}"
    );
    assert!(without_break.to_string().contains("--break-stale-lock"));
    assert_eq!(
        simulated.stale_break_requests.load(Ordering::SeqCst),
        0,
        "age alone never breaks a lock"
    );

    let acquired = RegistryLock::acquire(&pmxcfs, &state, &quick(true)).unwrap();
    assert_eq!(std::fs::read_dir(acquired.path()).unwrap().count(), 0);
    acquired.release().unwrap();
    assert!(!stale.exists());
    assert_eq!(simulated.stale_break_requests.load(Ordering::SeqCst), 1);
    assert_eq!(simulated.forbidden(), Vec::<String>::new());
}

#[test]
fn atomic_lock_times_out_and_can_explicitly_reap_stale_lock() {
    let temporary = tempfile::tempdir().unwrap();
    let state = temporary.path().join("state");
    let pmxcfs = local(temporary.path());
    pmxcfs.ensure_directory(&state, 0o700).unwrap();
    let lock = state.join(".allocator.lock");
    std::fs::create_dir(&lock).unwrap();
    std::fs::write(
        state.join(".allocator-lock-owner.json"),
        stale_owner().to_string(),
    )
    .unwrap();
    set_mtime(&lock, 946_684_800);

    let error = RegistryLock::acquire(&pmxcfs, &state, &quick(false))
        .err()
        .unwrap();
    assert!(
        error
            .to_string()
            .starts_with("timed out waiting for allocator lock (owner mox1 pid=1"),
        "{error}"
    );
    let fresh = LockOptions {
        stale_after: Duration::from_secs(1_000_000_000),
        ..quick(true)
    };
    assert!(
        RegistryLock::acquire(&pmxcfs, &state, &fresh).is_err(),
        "a lock younger than stale_after is kept"
    );

    let acquired = RegistryLock::acquire(&pmxcfs, &state, &quick(true)).unwrap();
    assert_eq!(acquired.path(), lock);
    let owner = pmxcfs.read_json(acquired.owner_path(), "owner").unwrap();
    assert_eq!(owner["record_type"], "allocator-lock");
    assert_eq!(owner["nonce"].as_str().unwrap().len(), 32);
    assert_eq!(owner["pid"], std::process::id());
}

#[test]
fn a_non_empty_stale_lock_directory_is_never_removed() {
    let temporary = tempfile::tempdir().unwrap();
    let state = temporary.path().join("state");
    let pmxcfs = local(temporary.path());
    pmxcfs.ensure_directory(&state, 0o700).unwrap();
    std::fs::create_dir_all(state.join(".allocator.lock/inside")).unwrap();
    set_mtime(&state.join(".allocator.lock"), 946_684_800);
    let error = RegistryLock::acquire(&pmxcfs, &state, &quick(true))
        .err()
        .unwrap();
    assert!(
        error
            .to_string()
            .contains("refusing to remove non-empty allocator lock directory"),
        "{error}"
    );
}

#[test]
fn release_refuses_when_ownership_changed_or_the_lock_vanished() {
    let temporary = tempfile::tempdir().unwrap();
    let state = temporary.path().join("state");
    let pmxcfs = local(temporary.path());
    let lock = RegistryLock::acquire(&pmxcfs, &state, &quick(false)).unwrap();
    std::fs::write(lock.owner_path(), stale_owner().to_string()).unwrap();
    let error = lock.release().unwrap_err();
    assert_eq!(
        error.to_string(),
        "allocator lock ownership changed while held"
    );

    std::fs::remove_dir(state.join(".allocator.lock")).unwrap();
    std::fs::remove_file(state.join(".allocator-lock-owner.json")).unwrap();
    let lock = RegistryLock::acquire(&pmxcfs, &state, &quick(false)).unwrap();
    std::fs::remove_file(lock.owner_path()).unwrap();
    assert_eq!(
        lock.release().unwrap_err().to_string(),
        "allocator lock disappeared while held"
    );
}

#[test]
fn dropping_a_lock_releases_it() {
    let temporary = tempfile::tempdir().unwrap();
    let state = temporary.path().join("state");
    let pmxcfs = local(temporary.path());
    drop(RegistryLock::acquire(&pmxcfs, &state, &quick(false)).unwrap());
    RegistryLock::acquire(&pmxcfs, &state, &quick(false))
        .unwrap()
        .release()
        .unwrap();
}

#[test]
fn atomic_writes_are_canonical_private_and_leave_no_temporary_files() {
    let temporary = tempfile::tempdir().unwrap();
    let pmxcfs = local(temporary.path());
    let path = temporary.path().join("state/sub/record.json");
    let value = json!({"b": [1, 2], "a": "é"});
    let outcome = pmxcfs.write_json(&path, &value).unwrap();
    let written = std::fs::read(&path).unwrap();
    assert_eq!(written, canonical_json(&value));
    assert_eq!(written, b"{\"a\":\"\\u00e9\",\"b\":[1,2]}\n");
    assert_eq!(outcome.bytes, written.len() as u64);
    assert_eq!(path.metadata().unwrap().permissions().mode() & 0o777, 0o600);
    assert_eq!(
        path.parent()
            .unwrap()
            .metadata()
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o700
    );
    let names: Vec<_> = std::fs::read_dir(path.parent().unwrap())
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    assert_eq!(names, ["record.json"]);
}

#[test]
fn symlinks_oversize_records_and_secrets_are_refused() {
    let temporary = tempfile::tempdir().unwrap();
    let pmxcfs = local(temporary.path());
    let real = temporary.path().join("real");
    std::fs::create_dir(&real).unwrap();
    std::os::unix::fs::symlink(&real, temporary.path().join("link")).unwrap();
    let error = pmxcfs
        .write_json(&temporary.path().join("link/record.json"), &json!({}))
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("state path may not contain symlink"),
        "{error}"
    );

    std::fs::write(real.join("target.json"), "{}").unwrap();
    std::os::unix::fs::symlink(real.join("target.json"), real.join("alias.json")).unwrap();
    let error = pmxcfs
        .read_json(&real.join("alias.json"), "record")
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("is not a non-symlink regular file"),
        "{error}"
    );

    let big = json!({"data": "x".repeat(1024 * 1024)});
    let error = pmxcfs.write_json(&real.join("big.json"), &big).unwrap_err();
    assert!(
        error
            .to_string()
            .contains("refusing to write oversized JSON record"),
        "{error}"
    );
    std::fs::write(real.join("big.json"), big.to_string()).unwrap();
    assert!(
        pmxcfs
            .read_json(&real.join("big.json"), "record")
            .unwrap_err()
            .to_string()
            .contains("exceeds")
    );

    let error = pmxcfs
        .write_json(&real.join("s.json"), &json!({"setup_token": "x"}))
        .unwrap_err();
    assert!(
        error.to_string().contains("may not contain secret field"),
        "{error}"
    );
    std::fs::write(real.join("s.json"), r#"{"note": "tskey-auth-k123"}"#).unwrap();
    assert!(
        pmxcfs
            .read_json(&real.join("s.json"), "record")
            .unwrap_err()
            .to_string()
            .contains("secret material")
    );
    assert!(
        pmxcfs
            .read_json(&real.join("missing.json"), "record")
            .unwrap_err()
            .to_string()
            .contains("does not exist")
    );
}

#[test]
fn the_budget_warns_then_refuses() {
    let temporary = tempfile::tempdir().unwrap();
    let budget_root = temporary.path().join("bmac");
    let pmxcfs = local(temporary.path()).with_budget(Budget {
        root: budget_root.clone(),
        hard: 1000,
        warn: 500,
    });
    let record = |n: usize| json!({"data": "x".repeat(n)});
    assert_eq!(
        pmxcfs
            .write_json(&budget_root.join("a.json"), &record(300))
            .unwrap()
            .budget_warning,
        None
    );
    let warned = pmxcfs
        .write_json(&budget_root.join("b.json"), &record(300))
        .unwrap();
    assert!(
        warned
            .budget_warning
            .unwrap()
            .contains("over the 0 MiB warning level")
    );
    // Rewriting a record counts its new size, not old plus new.
    assert!(
        pmxcfs
            .write_json(&budget_root.join("b.json"), &record(400))
            .is_ok()
    );
    let error = pmxcfs
        .write_json(&budget_root.join("c.json"), &record(300))
        .unwrap_err();
    assert!(
        error.to_string().contains("over its 0 MiB budget"),
        "{error}"
    );
    assert!(!budget_root.join("c.json").exists());
    assert!(
        pmxcfs
            .write_json(&temporary.path().join("elsewhere.json"), &record(3000))
            .is_ok(),
        "outside the budget root"
    );
    assert!(pmxcfs.usage().unwrap() > 700);
}

#[test]
fn records_carry_revisions_and_refuse_conflicts() {
    let temporary = tempfile::tempdir().unwrap();
    let state = temporary.path().join("state");
    let pmxcfs = local(temporary.path());
    let lock = RegistryLock::acquire(&pmxcfs, &state, &quick(false)).unwrap();
    let path = state.join("hosts/mox1.json");
    let created = write_record(
        &lock,
        &path,
        json!({"schema_version": 1, "node": "mox1"}),
        None,
    )
    .unwrap();
    assert_eq!(created.revision, 1);
    let conflict = write_record(&lock, &path, json!({"schema_version": 1}), None).unwrap_err();
    assert_eq!(
        conflict.to_string(),
        format!(
            "revision conflict on {}: expected 0, found 1",
            path.display()
        )
    );
    assert_eq!(
        write_record(
            &lock,
            &path,
            json!({"schema_version": 1, "node": "mox1"}),
            Some(1)
        )
        .unwrap()
        .revision,
        2
    );
    assert!(matches!(
        write_record(&lock, &path, json!({"schema_version": 1}), Some(1)),
        Err(PmxcfsError::RevisionConflict {
            expected: 1,
            found: 2,
            ..
        })
    ));
    assert_eq!(
        read_record(&pmxcfs, &path, "host", 1).unwrap().unwrap()["revision"],
        2
    );
    assert!(
        write_record(
            &lock,
            &temporary.path().join("outside.json"),
            json!({}),
            None
        )
        .is_err()
    );
    assert!(write_record(&lock, &state.join("list.json"), json!([1]), None).is_err());
}

#[test]
fn newer_schema_versions_are_refused() {
    let temporary = tempfile::tempdir().unwrap();
    let pmxcfs = local(temporary.path());
    let path = temporary.path().join("r.json");
    assert_eq!(read_record(&pmxcfs, &path, "host", 1).unwrap(), None);
    pmxcfs
        .write_json(&path, &json!({"schema_version": 2}))
        .unwrap();
    let error = read_record(&pmxcfs, &path, "host record", 1).unwrap_err();
    assert_eq!(
        error.to_string(),
        "host record uses schema version 2; this version of BMAC understands up to 1"
    );
    pmxcfs.write_json(&path, &json!({"revision": 1})).unwrap();
    assert!(
        read_record(&pmxcfs, &path, "host record", 1)
            .unwrap_err()
            .to_string()
            .contains("no valid schema_version")
    );
}

#[test]
fn concurrent_writers_never_lose_an_update() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path().join("etc/pve");
    let state = root.join("priv/bmac/registry");
    std::fs::create_dir_all(&root).unwrap();
    let simulated = SimulatedPmxcfs::new(&root);
    let pmxcfs = Pmxcfs::new(&root, simulated.clone());
    let counter = state.join("counter.json");
    let options = LockOptions {
        timeout: Duration::from_secs(60),
        ..LockOptions::default()
    };
    let (threads, rounds) = (8, 25);
    std::thread::scope(|scope| {
        for _ in 0..threads {
            scope.spawn(|| {
                for _ in 0..rounds {
                    let lock = RegistryLock::acquire(&pmxcfs, &state, &options).unwrap();
                    let current = read_record(&pmxcfs, &counter, "counter", 1).unwrap();
                    let (count, revision) = match &current {
                        Some(v) => (v["count"].as_u64().unwrap(), v["revision"].as_u64()),
                        None => (0, None),
                    };
                    let next = json!({"schema_version": 1, "count": count + 1});
                    write_record(&lock, &counter, next, revision).unwrap();
                    lock.release().unwrap();
                }
            });
        }
    });
    let last = read_record(&pmxcfs, &counter, "counter", 1)
        .unwrap()
        .unwrap();
    assert_eq!(last["count"], threads * rounds);
    assert_eq!(last["revision"], threads * rounds);
    assert_eq!(simulated.forbidden(), Vec::<String>::new());
}
