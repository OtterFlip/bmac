// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

//! The setup job's routing and orchestration phase (`DEPLOYMENT.md`, "Host
//! setup job" phase 6): guest egress with VRRP, the runtime's timers, the
//! registry on the first host, this host's HAProxy LXC and ingress, and the
//! routes. It runs the adapted v2 steps in
//! `legacy/scripts/host_setup/orchestrate_host.sh` until they are ported
//! (IMPLEMENTATION_PLAN.md, M7.3 and M11), streaming their output into the
//! job.

use std::path::PathBuf;
use std::time::Duration;

use bmac_core::HostSlot;
use bmac_exec::{Command, CommandRunner, OutputLine, Stream};
use serde_json::{Value, json};

use super::packages::Apt;
use super::{Stop, Workflow};

pub const SCRIPT: &str = "/usr/lib/bmac/legacy/scripts/host_setup/orchestrate_host.sh";
pub const VERIFY_SCRIPT: &str = "/usr/lib/bmac/legacy/scripts/host_setup/verify_host.sh";
pub const BASH: &str = "/usr/bin/bash";
pub const PACKAGES: &[&str] = &["keepalived", "jq", "curl", "psmisc", "xorriso", "nftables"];
const TIMEOUT: Duration = Duration::from_secs(45 * 60);

pub struct Orchestration<'a> {
    pub flow: &'a Workflow,
    pub runner: &'a dyn CommandRunner,
    pub root: PathBuf,
    pub me: HostSlot,
    /// Writes the settings into `/run/bmac/config` for the script.
    pub render: &'a (dyn Fn() -> Result<(), String> + Sync),
}

impl Orchestration<'_> {
    pub async fn run_phase(&self) -> Result<Value, Stop> {
        let apt = Apt {
            runner: self.runner,
            root: self.root.clone(),
        };
        let installed = apt.ensure(PACKAGES).await?;
        if !installed.is_empty() {
            self.flow
                .info(format!("Installed {}.", installed.join(", ")));
        }
        HostScript {
            flow: self.flow,
            runner: self.runner,
            me: self.me,
            render: self.render,
        }
        .run(SCRIPT, &[], "routing and orchestration steps")
        .await?;
        Ok(json!({ "script": SCRIPT }))
    }
}

/// Runs one of the adapted v2 scripts in `legacy/scripts/host_setup/` on
/// this host, after rendering the settings it reads, with its output in the
/// job.
pub struct HostScript<'a> {
    pub flow: &'a Workflow,
    pub runner: &'a dyn CommandRunner,
    pub me: HostSlot,
    pub render: &'a (dyn Fn() -> Result<(), String> + Sync),
}

impl HostScript<'_> {
    pub async fn run(&self, script: &str, args: &[&str], what: &str) -> Result<(), Stop> {
        (self.render)().map_err(|e| Stop::Failed {
            message: format!("The settings couldn't be written for the {what}."),
            details: Some(e),
        })?;
        let (sender, mut lines) = tokio::sync::mpsc::unbounded_channel::<OutputLine>();
        let command = Command::new(BASH)
            .arg(script)
            .args(args.iter().copied())
            .env("BMAC_HOST", self.me.to_string())
            .timeout(TIMEOUT)
            .cancel_on(self.flow.token().clone())
            .stream_lines(sender);
        let run = self.runner.run(command);
        let forward = async {
            while let Some(line) = lines.recv().await {
                let text = line.text.trim_end();
                if text.is_empty() {
                    continue;
                }
                match line.stream {
                    Stream::Stdout => self.flow.info(text),
                    Stream::Stderr => self.flow.warning(text),
                }
            }
        };
        let (outcome, ()) = tokio::join!(run, forward);
        outcome.map_err(|e| Stop::Failed {
            message: format!("The {what} failed; their output is above."),
            details: Some(e.to_string()),
        })?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write;
    use std::sync::{Arc, Mutex};

    use bmac_exec::{FakeRunner, Reply};

    use super::*;

    #[derive(Clone, Default)]
    struct Sink(Arc<Mutex<Vec<u8>>>);

    impl Write for Sink {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn the_script_runs_after_the_packages_and_settings_with_its_output_in_the_job() {
        let root = tempfile::tempdir().unwrap();
        let work = tempfile::tempdir().unwrap();
        let out = Sink::default();
        let (_input, theirs) = tokio::io::duplex(64);
        let flow = Workflow::new(
            Box::new(out.clone()),
            tokio::io::BufReader::new(theirs),
            work.path().to_path_buf(),
        );
        let runner = FakeRunner::new();
        let installed: String = PACKAGES
            .iter()
            .map(|p| format!("{p} installed\n"))
            .collect();
        runner.expect(
            "/usr/bin/dpkg-query",
            [
                &["-W", "-f=${Package} ${db:Status-Status}\\n"][..],
                PACKAGES,
            ]
            .concat(),
            Reply::ok(installed),
        );
        runner.expect(
            BASH,
            [SCRIPT],
            Reply::fail(1, "Guest egress needs MAX_MOX_HOSTS of at least 2\n")
                .stdout("\n==> Configuring the quorum-aware floating guest egress gateway\n"),
        );
        let rendered = Mutex::new(0);
        let render = || {
            *rendered.lock().unwrap() += 1;
            Ok(())
        };
        let phase = Orchestration {
            flow: &flow,
            runner: &runner,
            root: root.path().to_path_buf(),
            me: "mox1".parse().unwrap(),
            render: &render,
        };
        let error = phase.run_phase().await.unwrap_err();
        assert!(
            format!("{error:?}").contains("output is above"),
            "{error:?}"
        );
        runner.assert_done();
        assert_eq!(*rendered.lock().unwrap(), 1);
        assert_eq!(runner.calls()[1].env["BMAC_HOST"], "mox1");
        let text = String::from_utf8(out.0.lock().unwrap().clone()).unwrap();
        assert!(text.contains("==> Configuring the quorum-aware"), "{text}");
        assert!(text.contains("needs MAX_MOX_HOSTS of at least 2"), "{text}");
    }
}
