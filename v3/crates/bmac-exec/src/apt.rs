// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

//! Waiting for the apt and dpkg locks instead of failing when automatic
//! updates hold them (v2's `scripts/lib/apt_lock_wait.sh`).
//!
//! apt and dpkg take POSIX record locks, so `/proc/locks` names the holder
//! without `fuser` or `lslocks`. Each entry identifies the file as
//! `MAJOR:MINOR:INODE` with hex device numbers; matching the device as well
//! as the inode keeps a container's apt lock, also listed on a Proxmox host,
//! from matching a host lock file with the same inode number.

use std::path::{Path, PathBuf};
use std::time::Duration;

use rustix::fs::{major, minor, stat};

pub const APT_LOCK_FILES: [&str; 4] = [
    "/var/lib/dpkg/lock-frontend",
    "/var/lib/dpkg/lock",
    "/var/lib/apt/lists/lock",
    "/var/cache/apt/archives/lock",
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AptLockHolder {
    pub pid: u32,
    pub file: PathBuf,
    /// The holder's command line, when it can still be read.
    pub command: Option<String>,
}

/// The `/proc/locks` key for a file.
fn lock_key(path: &Path) -> Option<String> {
    let st = stat(path).ok()?;
    Some(format!(
        "{:02x}:{:02x}:{}",
        major(st.st_dev),
        minor(st.st_dev),
        st.st_ino
    ))
}

/// The PID holding the lock with this key, ignoring blocked waiters (`->`).
fn holder_in(proc_locks: &str, key: &str) -> Option<u32> {
    proc_locks.lines().find_map(|line| {
        let fields: Vec<&str> = line.split_whitespace().collect();
        if fields.get(1) == Some(&"->") || fields.get(5) != Some(&key) {
            return None;
        }
        fields.get(4)?.parse().ok()
    })
}

fn command_of(pid: u32) -> Option<String> {
    let raw = std::fs::read(format!("/proc/{pid}/cmdline")).ok()?;
    let text: Vec<String> = raw
        .split(|b| *b == 0)
        .filter(|a| !a.is_empty())
        .map(|a| String::from_utf8_lossy(a).into_owned())
        .collect();
    (!text.is_empty()).then(|| text.join(" "))
}

/// The first holder of any of `files` in `proc_locks` (normally
/// `/proc/locks`), or `None` when all are free.
pub fn apt_lock_holder(files: &[&Path], proc_locks: &Path) -> Option<AptLockHolder> {
    let locks = std::fs::read_to_string(proc_locks).ok()?;
    files.iter().find_map(|file| {
        let pid = holder_in(&locks, &lock_key(file)?)?;
        Some(AptLockHolder {
            pid,
            file: file.to_path_buf(),
            command: command_of(pid),
        })
    })
}

/// Waits until no file in `files` is locked, polling every `poll` for at most
/// `limit`. `progress` is told who holds a lock and how long it has waited.
/// Returns how long it waited, or the holder when the limit passed.
pub async fn wait_for_apt_locks(
    files: &[&Path],
    proc_locks: &Path,
    limit: Duration,
    poll: Duration,
    mut progress: impl FnMut(&AptLockHolder, Duration),
) -> Result<Duration, AptLockHolder> {
    let mut waited = Duration::ZERO;
    while let Some(holder) = apt_lock_holder(files, proc_locks) {
        if waited >= limit {
            return Err(holder);
        }
        progress(&holder, waited);
        tokio::time::sleep(poll).await;
        waited += poll;
    }
    Ok(waited)
}

#[cfg(test)]
mod tests {
    use std::io::{BufRead, BufReader};
    use std::process::{Command, Stdio};

    use super::*;

    #[test]
    fn finds_the_holder_and_skips_waiters() {
        let locks = "1: POSIX  ADVISORY  WRITE 4242 fd:01:1234 0 EOF\n\
                     1: -> POSIX  ADVISORY  WRITE 777 08:02:99 0 EOF\n\
                     2: POSIX  ADVISORY  WRITE 31 08:02:99 0 EOF\n";
        assert_eq!(holder_in(locks, "fd:01:1234"), Some(4242));
        assert_eq!(holder_in(locks, "08:02:99"), Some(31));
        assert_eq!(
            holder_in(locks, "08:02:1234"),
            None,
            "same inode on another device"
        );
    }

    /// Holds a POSIX lock on `path` in a child process until dropped.
    struct Holder(std::process::Child);

    impl Holder {
        fn lock(path: &Path) -> Holder {
            let mut child = Command::new("/usr/bin/python3")
                .args(["-c", "import fcntl,sys; f=open(sys.argv[1],'w'); fcntl.lockf(f, fcntl.LOCK_EX); print('locked', flush=True); sys.stdin.read()"])
                .arg(path)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .spawn()
                .unwrap();
            let mut line = String::new();
            BufReader::new(child.stdout.as_mut().unwrap())
                .read_line(&mut line)
                .unwrap();
            assert_eq!(line.trim(), "locked");
            Holder(child)
        }
    }

    impl Drop for Holder {
        fn drop(&mut self) {
            drop(self.0.stdin.take());
            let _ = self.0.wait();
        }
    }

    #[tokio::test]
    async fn waits_for_a_real_posix_lock() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("lock-frontend");
        let missing = dir.path().join("absent");
        let files = [missing.as_path(), file.as_path()];
        let proc_locks = Path::new("/proc/locks");
        std::fs::write(&file, "").unwrap();
        assert_eq!(apt_lock_holder(&files, proc_locks), None);

        let holder = Holder::lock(&file);
        let found = apt_lock_holder(&files, proc_locks).unwrap();
        assert_eq!(found.pid, holder.0.id());
        assert!(found.command.unwrap().contains("python3"));

        let mut reports = 0;
        let timed_out = wait_for_apt_locks(
            &files,
            proc_locks,
            Duration::from_millis(30),
            Duration::from_millis(10),
            |_, _| reports += 1,
        )
        .await;
        assert_eq!(timed_out.unwrap_err().pid, holder.0.id());
        assert!(reports >= 1);

        let release = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(100)).await;
            drop(holder);
        });
        let waited = wait_for_apt_locks(
            &files,
            proc_locks,
            Duration::from_secs(20),
            Duration::from_millis(20),
            |_, _| {},
        )
        .await;
        assert!(waited.is_ok());
        release.await.unwrap();
    }
}
