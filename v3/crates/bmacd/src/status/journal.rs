// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

//! The journal collector (`MONITORING.md`, "What is stored"): warning-and-
//! above entries from the cluster's units, kernel storage and hardware
//! errors, and BMAC's units, rate-limited per unit and stored as records,
//! which copying carries to every peer. When this host turns critical, the
//! recent entries of those units at every level are stored at once.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use bmac_adapters::programs::JOURNALCTL;
use bmac_adapters::run_text;
use bmac_exec::{Command, CommandRunner};
use serde_json::Value;

use crate::auth::Clock;
use crate::log;
use crate::store::{Entry, Store};

pub const UNITS: &[&str] = &[
    "pve-ha-crm.service",
    "pve-ha-lrm.service",
    "corosync.service",
    "pve-cluster.service",
    "pvescheduler.service",
    "zfs-zed.service",
    "keepalived.service",
    "bmacd.service",
    "bmacd-web.service",
    "bmac-backupd.service",
    "bmac-haproxy-ingress.service",
    "bmac-deferred-cleanup.service",
    "bmac-haproxy-route-sync.service",
];
/// Kernel messages kept: storage and hardware errors, matched in lower case.
const KERNEL_WORDS: &[&str] = &[
    "i/o error",
    "blk_update_request",
    "medium error",
    "sense key",
    "nvme",
    "ata",
    "scsi",
    "zfs",
    "zio",
    "hardware error",
    "mce:",
    "edac",
    "ext4-fs error",
    "xfs",
    "device offline",
    "link is down",
];
pub const EVERY: Duration = Duration::from_secs(30);
/// Entries kept per unit per minute; the rest are counted and noted.
const PER_UNIT_PER_MINUTE: u32 = 20;
const MAX_MESSAGE: usize = 4096;
/// The most entries one read takes.
const MAX_ENTRIES: u32 = 2000;
const SNAPSHOT_ENTRIES: u32 = 300;
const SNAPSHOT_SINCE: &str = "-15min";
const MINUTE_MS: i64 = 60_000;

/// One journal entry worth keeping.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JournalEntry {
    pub time_ms: i64,
    pub unit: String,
    pub priority: u8,
    pub message: String,
    pub cursor: Option<String>,
}

fn text(value: &Value) -> Option<String> {
    match value {
        Value::String(s) => Some(s.clone()),
        Value::Array(bytes) => {
            let bytes: Vec<u8> = bytes
                .iter()
                .filter_map(|b| b.as_u64().map(|b| b as u8))
                .collect();
            Some(String::from_utf8_lossy(&bytes).into_owned())
        }
        _ => None,
    }
}

/// Parses `journalctl -o json` output, keeping BMAC's units and the
/// kernel's storage and hardware messages.
pub fn parse(output: &str) -> Vec<JournalEntry> {
    let units: HashSet<&str> = UNITS.iter().copied().collect();
    let mut out = Vec::new();
    for line in output.lines().filter(|l| !l.trim().is_empty()) {
        let Ok(fields) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let field = |key: &str| fields.get(key).and_then(text);
        let Some(message) = field("MESSAGE") else {
            continue;
        };
        let Some(time_ms) = field("__REALTIME_TIMESTAMP").and_then(|t| t.parse::<i64>().ok())
        else {
            continue;
        };
        let kernel = field("_TRANSPORT").as_deref() == Some("kernel");
        let unit = if kernel {
            "kernel".to_string()
        } else {
            match field("_SYSTEMD_UNIT").or_else(|| field("UNIT")) {
                Some(unit) if units.contains(unit.as_str()) => unit,
                _ => continue,
            }
        };
        if kernel {
            let lower = message.to_lowercase();
            if !KERNEL_WORDS.iter().any(|w| lower.contains(w)) {
                continue;
            }
        }
        let message = match message.char_indices().nth(MAX_MESSAGE) {
            Some((at, _)) => format!("{}…", &message[..at]),
            None => message,
        };
        out.push(JournalEntry {
            time_ms: time_ms / 1000,
            unit,
            priority: field("PRIORITY").and_then(|p| p.parse().ok()).unwrap_or(6),
            message,
            cursor: field("__CURSOR"),
        });
    }
    out
}

/// Keeps at most [`PER_UNIT_PER_MINUTE`] entries per unit in each minute,
/// noting how many it dropped.
#[derive(Debug, Default)]
pub struct RateLimit {
    windows: HashMap<String, (i64, u32, u32)>,
}

impl RateLimit {
    pub fn admit(&mut self, entries: Vec<JournalEntry>) -> Vec<Entry> {
        let mut out = Vec::new();
        for entry in entries {
            let window = self
                .windows
                .entry(entry.unit.clone())
                .or_insert((entry.time_ms, 0, 0));
            if entry.time_ms - window.0 >= MINUTE_MS {
                if window.2 > 0 {
                    out.push(dropped(&entry.unit, window.0 + MINUTE_MS, window.2));
                }
                *window = (entry.time_ms, 0, 0);
            }
            if window.1 < PER_UNIT_PER_MINUTE {
                window.1 += 1;
                out.push(Entry::journal(
                    entry.time_ms,
                    &entry.unit,
                    entry.priority,
                    &entry.message,
                    false,
                ));
            } else {
                window.2 += 1;
            }
        }
        out
    }

    /// Notes for windows that ended before `now_ms` with entries dropped.
    pub fn flush(&mut self, now_ms: i64) -> Vec<Entry> {
        let mut out = Vec::new();
        for (unit, window) in &mut self.windows {
            if now_ms - window.0 >= MINUTE_MS && window.2 > 0 {
                out.push(dropped(unit, window.0 + MINUTE_MS, window.2));
                window.2 = 0;
            }
        }
        out
    }
}

fn dropped(unit: &str, time_ms: i64, count: u32) -> Entry {
    Entry::journal(
        time_ms,
        unit,
        4,
        &format!("{count} more entries from {unit} in that minute were not kept"),
        false,
    )
}

fn base() -> Command {
    Command::new(JOURNALCTL)
        .args(["-o", "json", "--no-pager", "--quiet"])
        .timeout(Duration::from_secs(20))
        .stdout_limit(16 << 20)
}

pub struct JournalConfig {
    pub runner: Arc<dyn CommandRunner>,
    pub store: Store,
    pub clock: Clock,
    /// Whether this host's health is critical.
    pub critical: Arc<dyn Fn() -> bool + Send + Sync>,
    pub every: Duration,
}

/// Collects into `config.store` until aborted.
pub fn spawn(config: JournalConfig) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let runner = config.runner.as_ref();
        let mut cursor: Option<String> = None;
        let mut limit = RateLimit::default();
        let mut was_critical = false;
        let mut failing = false;
        let mut tick = tokio::time::interval(config.every);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let since = latest_journal_ms(&config.store).await;
        loop {
            tick.tick().await;
            let mut command = base().args(["-p", "warning", "-n", &MAX_ENTRIES.to_string()]);
            command = match (&cursor, since) {
                (Some(cursor), _) => command.arg(format!("--after-cursor={cursor}")),
                (None, Some(ms)) => command.arg(format!("--since=@{}", ms / 1000 + 1)),
                (None, None) => command.arg(format!("--since={SNAPSHOT_SINCE}")),
            };
            let mut entries = match run_text(runner, command).await {
                Ok(output) => {
                    failing = false;
                    parse(&output)
                }
                Err(e) => {
                    if !failing {
                        log::warning(&format!("journal collector: {e}"));
                    }
                    failing = true;
                    Vec::new()
                }
            };
            if let Some(last) = entries.iter().rev().find_map(|e| e.cursor.clone()) {
                cursor = Some(last);
            }
            if cursor.is_none()
                && let Some(ms) = since
            {
                entries.retain(|e| e.time_ms > ms);
            }
            let mut records = limit.admit(entries);
            records.extend(limit.flush((config.clock)()));
            let critical = (config.critical)();
            if critical && !was_critical {
                records.extend(snapshot(runner).await);
            }
            was_critical = critical;
            if !records.is_empty()
                && let Err(e) = config.store.append(records).await
            {
                log::warning(&format!("journal collector: storing: {e}"));
            }
        }
    })
}

/// The recent entries of BMAC's units at every level, marked as a snapshot.
pub async fn snapshot(runner: &dyn CommandRunner) -> Vec<Entry> {
    let command = base().args([
        "-p",
        "info",
        "-n",
        &SNAPSHOT_ENTRIES.to_string(),
        &format!("--since={SNAPSHOT_SINCE}"),
    ]);
    match run_text(runner, command).await {
        Ok(output) => parse(&output)
            .into_iter()
            .map(|e| Entry::journal(e.time_ms, &e.unit, e.priority, &e.message, true))
            .collect(),
        Err(e) => {
            log::warning(&format!("journal snapshot: {e}"));
            Vec::new()
        }
    }
}

async fn latest_journal_ms(store: &Store) -> Option<i64> {
    let origin = store.origin().to_string();
    store
        .database()
        .run(move |c| {
            c.query_row(
                "SELECT MAX(time_ms) FROM records WHERE origin = ?1 AND kind = 'journal'",
                [origin],
                |r| r.get::<_, Option<i64>>(0),
            )
        })
        .await
        .ok()
        .flatten()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn line(unit: &str, transport: &str, priority: u8, message: Value, us: i64) -> String {
        json!({
            "__CURSOR": format!("c{us}"),
            "__REALTIME_TIMESTAMP": us.to_string(),
            "_SYSTEMD_UNIT": unit,
            "_TRANSPORT": transport,
            "PRIORITY": priority.to_string(),
            "MESSAGE": message,
        })
        .to_string()
    }

    #[test]
    fn only_bmacs_units_and_kernel_storage_errors_are_kept() {
        let output = [
            line(
                "corosync.service",
                "journal",
                3,
                json!("link 1 down"),
                5_000_000,
            ),
            line("cron.service", "journal", 3, json!("cron noise"), 5_000_001),
            line(
                "",
                "kernel",
                3,
                json!("blk_update_request: I/O error, dev sda"),
                5_000_002,
            ),
            line("", "kernel", 4, json!("usb 1-1: new device"), 5_000_003),
            line(
                "zfs-zed.service",
                "journal",
                4,
                json!([104, 105]),
                5_000_004,
            ),
            "not json".to_string(),
        ]
        .join("\n");
        let entries = parse(&output);
        let seen: Vec<(&str, &str)> = entries
            .iter()
            .map(|e| (e.unit.as_str(), e.message.as_str()))
            .collect();
        assert_eq!(
            seen,
            [
                ("corosync.service", "link 1 down"),
                ("kernel", "blk_update_request: I/O error, dev sda"),
                ("zfs-zed.service", "hi"),
            ]
        );
        assert_eq!(entries[0].time_ms, 5_000);
        assert_eq!(entries[0].priority, 3);
        assert_eq!(entries[2].cursor.as_deref(), Some("c5000004"));
    }

    #[test]
    fn each_unit_is_rate_limited_and_the_drops_are_noted() {
        let entry = |unit: &str, time_ms| JournalEntry {
            time_ms,
            unit: unit.into(),
            priority: 4,
            message: "m".into(),
            cursor: None,
        };
        let mut limit = RateLimit::default();
        let mut burst: Vec<JournalEntry> = (0..30)
            .map(|i| entry("corosync.service", 1_000 + i))
            .collect();
        burst.push(entry("zfs-zed.service", 1_000));
        let kept = limit.admit(burst);
        assert_eq!(kept.len(), 21, "20 from corosync and the one from zed");
        assert!(limit.flush(30_000).is_empty(), "the minute isn't over");
        let notes = limit.flush(61_000);
        assert_eq!(notes.len(), 1);
        assert!(
            notes[0]
                .body
                .contains("10 more entries from corosync.service")
        );
        assert!(limit.flush(200_000).is_empty(), "noted once");
        assert_eq!(
            limit.admit(vec![entry("corosync.service", 70_000)]).len(),
            1
        );
    }

    #[tokio::test]
    async fn the_snapshot_reads_recent_entries_at_every_level() {
        use bmac_exec::{FakeRunner, Reply};
        let fake = FakeRunner::new();
        fake.expect(
            JOURNALCTL,
            [
                "-o",
                "json",
                "--no-pager",
                "--quiet",
                "-p",
                "info",
                "-n",
                "300",
                "--since=-15min",
            ],
            Reply::ok(line(
                "pve-ha-lrm.service",
                "journal",
                6,
                json!("status change"),
                9_000_000,
            )),
        );
        let entries = snapshot(&fake).await;
        assert_eq!(entries.len(), 1);
        assert!(entries[0].body.contains("\"snapshot\":true"));
    }
}
