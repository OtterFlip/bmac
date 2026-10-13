// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

//! Typed adapters over Proxmox, ZFS, LUKS, networking, SMART, and systemd
//! commands (`WORKFLOW_MIGRATION.md`, "Typed adapters").
//!
//! Each tool is wrapped once. The parsers are plain functions over the
//! command's output text, so tests feed them recorded output; the adapters
//! run the command through a [`CommandRunner`] and parse the result. This
//! milestone has only the read-only side.

pub mod corosync;
pub mod disks;
pub mod layout;
pub mod proxmox;
pub mod qga;
pub mod systemd;
pub mod tailscale;
pub mod zfs;
pub mod zpool;

use std::io::ErrorKind;

use bmac_exec::{Command, CommandRunner, ExecError, Output};

/// Absolute paths of the programs the adapters run, as Proxmox VE installs
/// them. Native code never looks a program up in `PATH`.
pub mod programs {
    pub const CHRONYC: &str = "/usr/bin/chronyc";
    pub const COROSYNC_CFGTOOL: &str = "/usr/sbin/corosync-cfgtool";
    pub const COROSYNC_QUORUMTOOL: &str = "/usr/sbin/corosync-quorumtool";
    pub const DMSETUP: &str = "/usr/sbin/dmsetup";
    pub const IP: &str = "/usr/bin/ip";
    pub const JOURNALCTL: &str = "/usr/bin/journalctl";
    pub const LSBLK: &str = "/usr/bin/lsblk";
    pub const PVECM: &str = "/usr/bin/pvecm";
    pub const PVESH: &str = "/usr/bin/pvesh";
    pub const QM: &str = "/usr/sbin/qm";
    pub const SMARTCTL: &str = "/usr/sbin/smartctl";
    pub const SYSTEMCTL: &str = "/usr/bin/systemctl";
    pub const TAILSCALE: &str = "/usr/bin/tailscale";
    pub const ZFS: &str = "/usr/sbin/zfs";
    pub const ZPOOL: &str = "/usr/sbin/zpool";
}

#[derive(Debug, thiserror::Error)]
pub enum AdapterError {
    #[error("{program} is not installed")]
    NotInstalled { program: String },
    #[error(transparent)]
    Exec(ExecError),
    /// Output past the command's capture limit; parsing a prefix could
    /// report a healthy state that the rest of the output contradicts.
    #[error("{command} printed more output than BMAC reads")]
    Truncated { command: String },
    #[error("{command} printed output BMAC could not read: {message}")]
    Parse { command: String, message: String },
    #[error("{command} did not finish within {seconds} s")]
    Unfinished { command: String, seconds: u64 },
}

impl AdapterError {
    pub fn parse(command: &Command, message: impl Into<String>) -> AdapterError {
        AdapterError::Parse {
            command: command.to_string(),
            message: message.into(),
        }
    }
}

impl From<ExecError> for AdapterError {
    fn from(error: ExecError) -> AdapterError {
        match error {
            ExecError::Spawn { command, source } if source.kind() == ErrorKind::NotFound => {
                AdapterError::NotInstalled {
                    program: command.split(' ').next().unwrap_or_default().to_string(),
                }
            }
            other => AdapterError::Exec(other),
        }
    }
}

/// Runs `command` and returns its whole output, refusing truncated stdout.
pub async fn run(runner: &dyn CommandRunner, command: Command) -> Result<Output, AdapterError> {
    let display = command.to_string();
    let output = runner.run(command).await?;
    if output.stdout_truncated {
        return Err(AdapterError::Truncated { command: display });
    }
    Ok(output)
}

/// Runs `command` and returns its stdout as text.
pub async fn run_text(
    runner: &dyn CommandRunner,
    command: Command,
) -> Result<String, AdapterError> {
    Ok(run(runner, command).await?.stdout_text())
}

/// Runs `command` and parses its stdout as JSON.
pub async fn run_json<T: serde::de::DeserializeOwned>(
    runner: &dyn CommandRunner,
    command: Command,
) -> Result<T, AdapterError> {
    let display = command.to_string();
    let text = run_text(runner, command).await?;
    serde_json::from_str(&text).map_err(|e| AdapterError::Parse {
        command: display,
        message: format!("invalid JSON: {e}"),
    })
}

/// Gathers the parts of a report that could be read. A failed query adds
/// its error and the collection continues, so one unreachable API does not
/// hide what the others show (v2 `quick_state.py` 98–207).
#[derive(Debug, Default, Clone, PartialEq, Eq, serde::Serialize)]
pub struct Collection {
    pub errors: Vec<String>,
}

impl Collection {
    pub fn take<T>(&mut self, result: Result<T, AdapterError>) -> Option<T> {
        self.take_as(result, |message| message)
    }

    /// Like [`Collection::take`], with the error message rewritten, for
    /// example to name the host it came from.
    pub fn take_as<T>(
        &mut self,
        result: Result<T, AdapterError>,
        label: impl FnOnce(String) -> String,
    ) -> Option<T> {
        match result {
            Ok(value) => Some(value),
            Err(error) => {
                self.errors.push(label(error.to_string()));
                None
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use bmac_exec::{FakeRunner, Reply};

    use super::*;

    #[tokio::test]
    async fn truncated_output_is_refused() {
        let fake = FakeRunner::new();
        fake.expect(programs::ZPOOL, ["list"], Reply::ok("x".repeat(20)));
        let error = run_text(
            &fake,
            Command::new(programs::ZPOOL).arg("list").stdout_limit(10),
        )
        .await
        .unwrap_err();
        assert_eq!(
            error.to_string(),
            "/usr/sbin/zpool list printed more output than BMAC reads"
        );
    }

    #[tokio::test]
    async fn collection_keeps_going_after_a_failure() {
        let fake = FakeRunner::new();
        fake.expect(programs::PVESH, ["a"], Reply::fail(2, "no quorum\n"))
            .expect(programs::PVESH, ["b"], Reply::ok("[1]"));
        let mut collection = Collection::default();
        let a: Option<Vec<u8>> =
            collection.take(run_json(&fake, Command::new(programs::PVESH).arg("a")).await);
        let b: Option<Vec<u8>> = collection.take_as(
            run_json(&fake, Command::new(programs::PVESH).arg("b")).await,
            |m| format!("mox1: {m}"),
        );
        assert_eq!((a, b), (None, Some(vec![1])));
        assert_eq!(
            collection.errors,
            ["/usr/bin/pvesh a failed with exit code 2: no quorum"]
        );
    }

    #[tokio::test]
    async fn a_missing_program_is_reported_as_not_installed() {
        let error = run_text(
            &bmac_exec::RealRunner,
            Command::new("/nonexistent/bmac-test-tool"),
        )
        .await
        .unwrap_err();
        assert_eq!(
            error.to_string(),
            "/nonexistent/bmac-test-tool is not installed"
        );
    }
}
