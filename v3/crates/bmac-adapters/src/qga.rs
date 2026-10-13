// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

//! The QEMU guest agent through the Proxmox API: start a program in a guest
//! with `agent/exec`, then poll `agent/exec-status` until it exits. The API
//! routes the call to the node running the guest, so this works from any
//! host, and the guest is never reached over the network.

use std::time::Duration;

use bmac_core::{HostSlot, Vmid};
use bmac_exec::{Command, CommandRunner};
use serde::{Deserialize, Deserializer};
use serde_json::Value;

use crate::{AdapterError, programs, run_json};

fn flag<'de, D: Deserializer<'de>>(deserializer: D) -> Result<bool, D::Error> {
    Ok(match Value::deserialize(deserializer)? {
        Value::Bool(value) => value,
        Value::Number(n) => n.as_u64() != Some(0),
        _ => false,
    })
}

#[derive(Debug, Deserialize)]
struct Started {
    pid: u64,
}

/// `agent/exec-status`: `exited` is 0 until the program has finished.
#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize)]
pub struct ExecStatus {
    #[serde(default, deserialize_with = "flag")]
    pub exited: bool,
    #[serde(default)]
    pub exitcode: Option<i64>,
    #[serde(default)]
    pub signal: Option<i64>,
    #[serde(default, rename = "out-data")]
    pub out_data: Option<String>,
    #[serde(default, rename = "err-data")]
    pub err_data: Option<String>,
    #[serde(default, rename = "out-truncated", deserialize_with = "flag")]
    pub out_truncated: bool,
}

impl ExecStatus {
    /// The program exited with status 0 and its output is whole.
    pub fn succeeded(&self) -> bool {
        self.exited && self.exitcode == Some(0) && self.signal.is_none() && !self.out_truncated
    }
}

pub struct GuestAgent<'a> {
    runner: &'a dyn CommandRunner,
    poll: Duration,
    timeout: Duration,
}

impl<'a> GuestAgent<'a> {
    /// Polls every 250 ms for up to 30 seconds, as v2's
    /// `qm guest exec --timeout 30`.
    pub fn new(runner: &'a dyn CommandRunner) -> GuestAgent<'a> {
        GuestAgent {
            runner,
            poll: Duration::from_millis(250),
            timeout: Duration::from_secs(30),
        }
    }

    pub fn with_timing(mut self, poll: Duration, timeout: Duration) -> GuestAgent<'a> {
        self.poll = poll;
        self.timeout = timeout;
        self
    }

    /// Runs `argv` in guest `vmid` on `node` and waits for it to finish.
    pub async fn exec(
        &self,
        node: HostSlot,
        vmid: Vmid,
        argv: &[&str],
    ) -> Result<ExecStatus, AdapterError> {
        let base = format!("/nodes/{node}/qemu/{vmid}/agent");
        let mut start = Command::new(programs::PVESH).args(["create", &format!("{base}/exec")]);
        for arg in argv {
            start = start.args(["--command", arg]);
        }
        let start = start.args(["--output-format", "json"]);
        let display = start.to_string();
        let Started { pid } = run_json(self.runner, start).await?;
        let deadline = tokio::time::Instant::now() + self.timeout;
        loop {
            let status = Command::new(programs::PVESH)
                .args(["get", &format!("{base}/exec-status")])
                .args(["--pid", &pid.to_string(), "--output-format", "json"]);
            let status: ExecStatus = run_json(self.runner, status).await?;
            if status.exited {
                return Ok(status);
            }
            if tokio::time::Instant::now() >= deadline {
                return Err(AdapterError::Unfinished {
                    command: display,
                    seconds: self.timeout.as_secs(),
                });
            }
            tokio::time::sleep(self.poll).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use bmac_exec::{FakeRunner, Reply};

    use super::*;

    const EXEC: [&str; 8] = [
        "create",
        "/nodes/mox3/qemu/100/agent/exec",
        "--command",
        "/bin/cat",
        "--command",
        "/etc/ssh/ssh_host_ed25519_key.pub",
        "--output-format",
        "json",
    ];
    const STATUS: [&str; 6] = [
        "get",
        "/nodes/mox3/qemu/100/agent/exec-status",
        "--pid",
        "7",
        "--output-format",
        "json",
    ];

    fn agent(fake: &FakeRunner) -> GuestAgent<'_> {
        GuestAgent::new(fake).with_timing(Duration::from_millis(1), Duration::from_millis(50))
    }

    async fn cat(fake: &FakeRunner) -> Result<ExecStatus, AdapterError> {
        agent(fake)
            .exec(
                "mox3".parse().unwrap(),
                Vmid::new(100).unwrap(),
                &["/bin/cat", "/etc/ssh/ssh_host_ed25519_key.pub"],
            )
            .await
    }

    #[tokio::test]
    async fn runs_a_program_and_waits_for_it_to_exit() {
        let fake = FakeRunner::new();
        fake.expect(programs::PVESH, EXEC, Reply::ok(r#"{"pid":7}"#))
            .expect(programs::PVESH, STATUS, Reply::ok(r#"{"exited":0}"#))
            .expect(
                programs::PVESH,
                STATUS,
                Reply::ok(r#"{"exited":1,"exitcode":0,"out-data":"ssh-ed25519 AAAA\n"}"#),
            );
        let status = cat(&fake).await.unwrap();
        assert!(status.succeeded());
        assert_eq!(status.out_data.as_deref(), Some("ssh-ed25519 AAAA\n"));
        fake.assert_done();
    }

    #[tokio::test]
    async fn failures_and_truncation_are_not_success() {
        for (body, ok) in [
            (
                r#"{"exited":1,"exitcode":1,"err-data":"No such file"}"#,
                false,
            ),
            (
                r#"{"exited":1,"exitcode":0,"out-data":"x","out-truncated":1}"#,
                false,
            ),
            (r#"{"exited":1,"exitcode":0,"signal":9}"#, false),
            (r#"{"exited":1,"exitcode":0,"out-data":""}"#, true),
        ] {
            let status: ExecStatus = serde_json::from_str(body).unwrap();
            assert_eq!(status.succeeded(), ok, "{body}");
        }
    }

    #[tokio::test]
    async fn a_program_that_never_exits_times_out() {
        let fake = FakeRunner::new();
        fake.expect(programs::PVESH, EXEC, Reply::ok(r#"{"pid":7}"#));
        for _ in 0..200 {
            fake.expect(programs::PVESH, STATUS, Reply::ok(r#"{"exited":0}"#));
        }
        let error = cat(&fake).await.unwrap_err();
        assert!(matches!(error, AdapterError::Unfinished { .. }), "{error}");
    }

    #[tokio::test]
    async fn an_agent_that_is_not_running_is_an_error() {
        let fake = FakeRunner::new();
        fake.expect(
            programs::PVESH,
            EXEC,
            Reply::fail(255, "QEMU guest agent is not running\n"),
        );
        let error = cat(&fake).await.unwrap_err();
        assert!(
            error
                .to_string()
                .contains("QEMU guest agent is not running"),
            "{error}"
        );
    }
}
