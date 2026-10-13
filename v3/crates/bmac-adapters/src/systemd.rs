// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

//! `systemctl is-active` and `systemctl show`.

use std::collections::BTreeMap;

use bmac_exec::{Command, CommandRunner};

use crate::{AdapterError, programs, run_text};

/// The units whose state the health view reports for every host.
pub const CLUSTER_UNITS: &[&str] = &[
    "corosync.service",
    "pve-cluster.service",
    "corosync-qdevice.service",
];

/// Parses `systemctl show --property=...`: `Key=Value` lines.
pub fn parse_show(text: &str) -> BTreeMap<String, String> {
    text.lines()
        .filter_map(|line| line.split_once('='))
        .map(|(key, value)| (key.to_string(), value.to_string()))
        .collect()
}

/// Parses `systemctl list-units --failed --plain --no-legend`: the unit
/// names, one per line, first column.
pub fn parse_failed(text: &str) -> Vec<String> {
    text.lines()
        .filter_map(|line| line.split_whitespace().find(|w| *w != "●"))
        .map(str::to_string)
        .collect()
}

/// `systemctl`.
pub struct Systemd<'a> {
    runner: &'a dyn CommandRunner,
}

impl<'a> Systemd<'a> {
    pub fn new(runner: &'a dyn CommandRunner) -> Systemd<'a> {
        Systemd { runner }
    }

    /// `active`, `inactive`, `failed`, `activating`, ..., or `unknown` when
    /// systemctl prints nothing. A non-zero exit only means "not active".
    pub async fn active_state(&self, unit: &str) -> Result<String, AdapterError> {
        let all: Vec<i32> = (0..=255).collect();
        let text = run_text(
            self.runner,
            Command::new(programs::SYSTEMCTL)
                .args(["is-active", unit])
                .expect_codes(&all),
        )
        .await?;
        let state = text.trim();
        Ok(if state.is_empty() {
            "unknown".to_string()
        } else {
            state.to_string()
        })
    }

    pub async fn failed_units(&self) -> Result<Vec<String>, AdapterError> {
        let text = run_text(
            self.runner,
            Command::new(programs::SYSTEMCTL).args([
                "list-units",
                "--failed",
                "--plain",
                "--no-legend",
                "--no-pager",
            ]),
        )
        .await?;
        Ok(parse_failed(&text))
    }

    pub async fn show(
        &self,
        unit: &str,
        properties: &[&str],
    ) -> Result<BTreeMap<String, String>, AdapterError> {
        let text = run_text(
            self.runner,
            Command::new(programs::SYSTEMCTL).args([
                "show".to_string(),
                format!("--property={}", properties.join(",")),
                unit.to_string(),
            ]),
        )
        .await?;
        Ok(parse_show(&text))
    }

    pub async fn restart(&self, unit: &str) -> Result<(), AdapterError> {
        run_text(
            self.runner,
            Command::new(programs::SYSTEMCTL).args(["restart", unit]),
        )
        .await
        .map(drop)
    }
}

#[cfg(test)]
mod tests {
    use bmac_exec::{FakeRunner, Reply};

    use super::*;

    #[tokio::test]
    async fn inactive_units_are_states_not_errors() {
        let fake = FakeRunner::new();
        fake.expect(
            programs::SYSTEMCTL,
            ["is-active", "corosync-qdevice.service"],
            Reply::ok("inactive\n").code(3),
        )
        .expect(
            programs::SYSTEMCTL,
            ["is-active", "gone.service"],
            Reply::ok("").code(4),
        )
        .expect(
            programs::SYSTEMCTL,
            [
                "show",
                "--property=ActiveState,SubState",
                "corosync.service",
            ],
            Reply::ok("ActiveState=active\nSubState=running\n"),
        );
        let systemd = Systemd::new(&fake);
        assert_eq!(
            systemd
                .active_state("corosync-qdevice.service")
                .await
                .unwrap(),
            "inactive"
        );
        assert_eq!(
            systemd.active_state("gone.service").await.unwrap(),
            "unknown"
        );
        let shown = systemd
            .show("corosync.service", &["ActiveState", "SubState"])
            .await
            .unwrap();
        assert_eq!(shown["SubState"], "running");
        fake.assert_done();
    }
}
