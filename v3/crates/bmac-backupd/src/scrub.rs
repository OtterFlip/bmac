// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

//! Scrubs of the backup pool (`BACKUP.md`, "Scrubs and integrity"): one
//! every `scrub_every_days`, sharing the disks with pulls under the I/O
//! policy, and reported in every check-in. ZFS names the datasets that hold
//! unrepairable blocks; every published PIT stored in such a zvol at or
//! after the named snapshot is marked suspect, since later snapshots share
//! the older ones' blocks.

use std::collections::BTreeSet;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

use bmac_exec::{Command, CommandRunner};
use bmac_proto::appliance::ScrubReport;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::audit::Audit;
use crate::fsx;
use crate::journal::{Journal, Metadata};
use crate::platform::{IoPolicy, Platform};
use crate::pool::ZPOOL;

const DAY_MS: i64 = 86_400_000;

/// Who has the disks: pulls count themselves in, the scrub loop says
/// whether a scrub runs.
#[derive(Debug, Default)]
pub struct IoGate {
    pulling: AtomicUsize,
    scrubbing: AtomicBool,
    scrub_first: AtomicBool,
}

pub struct Pulling<'a>(&'a IoGate);

impl Drop for Pulling<'_> {
    fn drop(&mut self) {
        self.0.pulling.fetch_sub(1, Ordering::SeqCst);
    }
}

impl IoGate {
    pub fn pulling(&self) -> bool {
        self.pulling.load(Ordering::SeqCst) > 0
    }

    pub fn start_pull(&self) -> Pulling<'_> {
        self.pulling.fetch_add(1, Ordering::SeqCst);
        Pulling(self)
    }

    /// Whether a new pull waits for the scrub.
    pub fn pulls_wait(&self) -> bool {
        self.scrubbing.load(Ordering::SeqCst) && self.scrub_first.load(Ordering::SeqCst)
    }

    pub fn set(&self, scrubbing: bool, policy: IoPolicy) {
        self.scrubbing.store(scrubbing, Ordering::SeqCst);
        self.scrub_first
            .store(policy == IoPolicy::ScrubFirst, Ordering::SeqCst);
    }
}

/// What `zpool status -pv` says of the scan.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Observed {
    /// `none`, `running`, `paused`, `finished`, or `canceled`.
    pub state: String,
    pub done_permille: Option<u32>,
    pub repaired_bytes: u64,
    pub scan_errors: u64,
    /// The `errors:` section's damaged objects.
    pub files: Vec<String>,
    pub data_errors: u64,
}

fn number_before(text: &str, word: &str) -> Option<u64> {
    let words: Vec<&str> = text.split_whitespace().collect();
    words
        .iter()
        .position(|w| *w == word)
        .and_then(|i| i.checked_sub(1))
        .and_then(|i| words[i].trim_end_matches('B').parse().ok())
}

fn number_after(text: &str, word: &str) -> Option<u64> {
    let words: Vec<&str> = text.split_whitespace().collect();
    words
        .iter()
        .position(|w| *w == word)
        .and_then(|i| words.get(i + 1))
        .and_then(|w| w.trim_end_matches('B').parse().ok())
}

pub fn parse(status: &str) -> Observed {
    let mut scan = String::new();
    let mut errors = Vec::new();
    let mut section = "";
    for line in status.lines() {
        let trimmed = line.trim();
        if let Some((key, rest)) = trimmed.split_once(':')
            && !line.starts_with('\t')
            && !line.starts_with("        ")
            && [
                "pool", "state", "status", "action", "scan", "config", "errors", "see", "remove",
            ]
            .contains(&key)
        {
            section = key;
            match key {
                "scan" => scan.push_str(rest),
                "errors" => errors.push(rest.trim().to_string()),
                _ => {}
            }
            continue;
        }
        match section {
            "scan" => {
                scan.push(' ');
                scan.push_str(trimmed);
            }
            "errors" if !trimmed.is_empty() => errors.push(trimmed.to_string()),
            _ => {}
        }
    }
    let scan = scan.trim();
    let mut out = Observed {
        state: "none".into(),
        ..Default::default()
    };
    if scan.starts_with("scrub in progress") {
        out.state = "running".into();
        out.done_permille = scan
            .split_whitespace()
            .find(|w| w.ends_with('%'))
            .and_then(|w| w.trim_end_matches('%').parse::<f64>().ok())
            .map(|p| (p * 10.0).round().clamp(0.0, 1000.0) as u32);
        out.repaired_bytes = number_before(scan, "repaired,").unwrap_or(0);
    } else if scan.starts_with("scrub paused") {
        out.state = "paused".into();
    } else if scan.starts_with("scrub repaired") {
        out.state = "finished".into();
        out.repaired_bytes = number_after(scan, "repaired").unwrap_or(0);
        out.scan_errors = number_after(scan, "with").unwrap_or(0);
    } else if scan.starts_with("scrub canceled") {
        out.state = "canceled".into();
    }
    out.files = errors.iter().skip(1).cloned().collect();
    if let Some(head) = errors.first() {
        out.data_errors = head
            .split_whitespace()
            .next()
            .and_then(|n| n.parse().ok())
            .unwrap_or(out.files.len() as u64);
    }
    out
}

/// The datasets of the damaged objects, with the snapshot when ZFS names
/// one: `pool/ds@snap:<0x1>` gives `("pool/ds", Some("snap"))`.
pub fn damaged(files: &[String]) -> Vec<(String, Option<String>)> {
    let mut out = Vec::new();
    for file in files {
        if file.starts_with('<') || file.starts_with('/') {
            continue;
        }
        let name = file.rsplit_once(':').map(|(n, _)| n).unwrap_or(file);
        let entry = match name.split_once('@') {
            Some((ds, snap)) => (ds.to_string(), Some(snap.to_string())),
            None => (name.to_string(), None),
        };
        if !out.contains(&entry) {
            out.push(entry);
        }
    }
    out
}

/// One PIT a scrub found damage under.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Suspect {
    pub cluster_uuid: String,
    pub pit_uuid: Uuid,
    pub datasets: Vec<String>,
    pub found_at_ms: i64,
}

/// The published PITs in a damaged zvol: all of them when ZFS names the
/// zvol, else the named snapshot's PIT and every later one.
pub fn suspects(
    journal: &Journal,
    damage: &[(String, Option<String>)],
) -> Vec<(String, Uuid, String)> {
    let mut out: Vec<(String, Uuid, String)> = Vec::new();
    for (dataset, snapshot) in damage {
        let holding: Vec<_> = journal
            .pits
            .iter()
            .filter(|p| p.volumes.iter().any(|v| &v.dataset == dataset))
            .collect();
        let from = snapshot
            .as_ref()
            .and_then(|s| holding.iter().find(|p| &p.name == s))
            .map(|p| p.taken_s);
        for pit in holding {
            if from.is_none_or(|t| pit.taken_s >= t) {
                out.push((pit.cluster_uuid.clone(), pit.pit_uuid, dataset.clone()));
            }
        }
    }
    out
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScrubState {
    pub schema_version: u32,
    pub last_started_ms: Option<i64>,
    pub report: Option<ScrubReport>,
    pub suspects: Vec<Suspect>,
    /// This appliance paused the scrub for a pull and resumes it.
    pub paused_for_pull: bool,
    /// The administrator asked for a scrub now, even with scheduled scrubs
    /// off; it starts at the next tick that may start one.
    #[serde(default)]
    pub requested: bool,
}

impl ScrubState {
    fn path(metadata: &Metadata) -> std::path::PathBuf {
        metadata.root().join("scrub.json")
    }

    pub fn load(metadata: &Metadata) -> std::io::Result<ScrubState> {
        Ok(fsx::read_json(&Self::path(metadata))?.unwrap_or_default())
    }

    pub fn save(&self, metadata: &Metadata) -> std::io::Result<()> {
        fsx::write_json(&Self::path(metadata), self, 0o644)
    }

    /// The check-in's report, with the suspects of one cluster.
    pub fn report_for(&self, cluster: &str) -> Option<ScrubReport> {
        let mut report = self.report.clone()?;
        report.suspect_pits = self
            .suspects
            .iter()
            .filter(|s| s.cluster_uuid == cluster)
            .map(|s| s.pit_uuid)
            .collect();
        Some(report)
    }
}

async fn zpool(runner: &dyn CommandRunner, args: &[&str]) -> Result<String, String> {
    runner
        .run(
            Command::new(ZPOOL)
                .args(args.iter().copied())
                .timeout(Duration::from_secs(120)),
        )
        .await
        .map(|o| o.stdout_text())
        .map_err(|e| e.to_string())
}

/// One look at the pool: start a scrub that is due, pause or resume one
/// for pulls, and record a finished one.
pub async fn tick(
    runner: &dyn CommandRunner,
    pool: &str,
    metadata: &Metadata,
    audit: &Audit,
    io: &IoGate,
    now_ms: i64,
) -> Result<ScrubState, String> {
    let platform = Platform::load(metadata).map_err(|e| e.to_string())?;
    let mut state = ScrubState::load(metadata).map_err(|e| e.to_string())?;
    let observed = parse(&zpool(runner, &["status", "-pv", pool]).await?);
    let policy = platform.io_policy;
    let mut report = state.report.clone().unwrap_or_default();
    let was = report.state.clone();
    match observed.state.as_str() {
        "running" => {
            if policy == IoPolicy::PullsFirst && io.pulling() {
                zpool(runner, &["scrub", "-p", pool]).await?;
                state.paused_for_pull = true;
                report.state = "paused".into();
            } else {
                report.state = "running".into();
            }
            report.done_permille = observed.done_permille;
            report.repaired_bytes = observed.repaired_bytes;
            if report.started_at_ms.is_none()
                || matches!(was.as_str(), "finished" | "canceled" | "")
            {
                report.started_at_ms = Some(now_ms);
                report.finished_at_ms = None;
            }
        }
        "paused" => {
            report.state = "paused".into();
            if state.paused_for_pull && !(policy == IoPolicy::PullsFirst && io.pulling()) {
                zpool(runner, &["scrub", pool]).await?;
                state.paused_for_pull = false;
                report.state = "running".into();
            }
        }
        "finished" | "canceled" if matches!(was.as_str(), "running" | "paused") => {
            report.state = observed.state.clone();
            report.finished_at_ms = Some(now_ms);
            report.done_permille = None;
            report.repaired_bytes = observed.repaired_bytes;
            state.paused_for_pull = false;
        }
        _ => {}
    }
    let journal = metadata.load().map_err(|e| e.to_string())?;
    let damage = damaged(&observed.files);
    report.errors = observed.data_errors.max(observed.scan_errors);
    report.affected = damage
        .iter()
        .map(|(d, s)| match s {
            Some(s) => format!("{d}@{s}"),
            None => d.clone(),
        })
        .collect();
    if let Some(journal) = &journal {
        let found = suspects(journal, &damage);
        let known: BTreeSet<Uuid> = state.suspects.iter().map(|s| s.pit_uuid).collect();
        for (cluster, pit, dataset) in found {
            if let Some(s) = state.suspects.iter_mut().find(|s| s.pit_uuid == pit) {
                if !s.datasets.contains(&dataset) {
                    s.datasets.push(dataset);
                }
            } else if !known.contains(&pit) {
                state.suspects.push(Suspect {
                    cluster_uuid: cluster,
                    pit_uuid: pit,
                    datasets: vec![dataset],
                    found_at_ms: now_ms,
                });
            }
        }
        state
            .suspects
            .retain(|s| journal.pits.iter().any(|p| p.pit_uuid == s.pit_uuid));
    }
    let finished_now =
        report.state != was && matches!(report.state.as_str(), "finished" | "canceled");
    if finished_now {
        let _ = audit.record(
            now_ms,
            "scrub",
            "bmac-backupd",
            serde_json::json!({
                "state": report.state,
                "repaired_bytes": report.repaired_bytes,
                "errors": report.errors,
                "affected": report.affected,
                "suspect_pits": state.suspects.iter().map(|s| s.pit_uuid).collect::<Vec<_>>(),
            }),
        );
    }
    let idle = !matches!(observed.state.as_str(), "running" | "paused");
    let due = state
        .last_started_ms
        .is_none_or(|t| now_ms - t >= i64::from(platform.scrub_every_days.max(1)) * DAY_MS);
    let blocked = policy == IoPolicy::PullsFirst && io.pulling();
    let wanted = state.requested || (due && platform.scrub_every_days > 0);
    if idle && wanted && !blocked {
        zpool(runner, &["scrub", pool]).await?;
        state.last_started_ms = Some(now_ms);
        state.requested = false;
        report = ScrubReport {
            state: "running".into(),
            started_at_ms: Some(now_ms),
            done_permille: Some(0),
            errors: report.errors,
            affected: report.affected,
            ..Default::default()
        };
        let _ = audit.record(now_ms, "scrub_start", "bmac-backupd", serde_json::json!({}));
    }
    if report.state.is_empty() {
        report.state = observed.state.clone();
    }
    io.set(matches!(report.state.as_str(), "running"), policy);
    state.report = (report.state != "none" || report.errors > 0).then_some(report);
    state.save(metadata).map_err(|e| e.to_string())?;
    Ok(state)
}

#[cfg(test)]
#[path = "scrub_tests.rs"]
mod tests;
