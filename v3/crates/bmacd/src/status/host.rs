// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

//! The `host` collector: CPU, memory, load, uptime, and root file system
//! use, from `/proc` and `statvfs` (`MONITORING.md`, "Collectors").

use std::path::{Path, PathBuf};

use bmac_proto::status::HostObservation;

/// CPU time counters from the `cpu` line of `/proc/stat`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CpuTimes {
    pub busy: u64,
    pub total: u64,
}

pub fn parse_cpu(stat: &str) -> Option<CpuTimes> {
    let line = stat.lines().find(|l| l.starts_with("cpu "))?;
    let values: Vec<u64> = line
        .split_whitespace()
        .skip(1)
        .map(|v| v.parse().ok())
        .collect::<Option<_>>()?;
    if values.len() < 4 {
        return None;
    }
    let total: u64 = values.iter().take(8).sum();
    let idle = values[3] + values.get(4).copied().unwrap_or(0);
    Some(CpuTimes {
        busy: total.saturating_sub(idle),
        total,
    })
}

pub fn parse_cores(stat: &str) -> Option<u32> {
    let count = stat
        .lines()
        .filter(|l| l.starts_with("cpu") && l.as_bytes().get(3).is_some_and(u8::is_ascii_digit))
        .count();
    (count > 0).then_some(count as u32)
}

/// The busy share between two readings.
pub fn cpu_fraction(before: CpuTimes, now: CpuTimes) -> Option<f64> {
    let total = now.total.checked_sub(before.total)?;
    let busy = now.busy.checked_sub(before.busy)?;
    (total > 0).then(|| (busy as f64 / total as f64).clamp(0.0, 1.0))
}

/// Used and total memory in bytes: total less what is available.
pub fn parse_memory(meminfo: &str) -> Option<(u64, u64)> {
    let field = |name: &str| {
        meminfo
            .lines()
            .find_map(|l| l.strip_prefix(name)?.strip_prefix(':'))
            .and_then(|rest| rest.split_whitespace().next()?.parse::<u64>().ok())
            .map(|kib| kib * 1024)
    };
    let total = field("MemTotal")?;
    let available = field("MemAvailable")?;
    Some((total.saturating_sub(available), total))
}

pub fn parse_uptime(text: &str) -> Option<u64> {
    let seconds: f64 = text.split_whitespace().next()?.parse().ok()?;
    Some(seconds as u64)
}

pub fn parse_load1(text: &str) -> Option<f64> {
    text.split_whitespace().next()?.parse().ok()
}

/// Reads one host's `/proc`; tests point it at a directory of their own.
pub struct HostCollector {
    proc: PathBuf,
    root: PathBuf,
    previous: Option<CpuTimes>,
}

impl HostCollector {
    pub fn new(proc: impl Into<PathBuf>, root: impl Into<PathBuf>) -> HostCollector {
        HostCollector {
            proc: proc.into(),
            root: root.into(),
            previous: None,
        }
    }

    pub fn system() -> HostCollector {
        HostCollector::new("/proc", "/")
    }

    fn read(&self, name: &str) -> Option<String> {
        std::fs::read_to_string(self.proc.join(name)).ok()
    }

    pub fn observe(&mut self, at_ms: i64, version: &str) -> HostObservation {
        let stat = self.read("stat").unwrap_or_default();
        let times = parse_cpu(&stat);
        let cpu = match (self.previous, times) {
            (Some(before), Some(now)) => cpu_fraction(before, now),
            _ => None,
        };
        self.previous = times;
        let memory = self.read("meminfo").as_deref().and_then(parse_memory);
        let root = root_use(&self.root);
        HostObservation {
            at_ms,
            bmac_version: version.to_string(),
            uptime_seconds: self.read("uptime").as_deref().and_then(parse_uptime),
            cores: parse_cores(&stat),
            cpu_fraction: cpu,
            load1: self.read("loadavg").as_deref().and_then(parse_load1),
            memory_used: memory.map(|m| m.0),
            memory_total: memory.map(|m| m.1),
            root_used: root.map(|r| r.0),
            root_total: root.map(|r| r.1),
        }
    }
}

/// Used and total bytes of the file system holding `path`.
fn root_use(path: &Path) -> Option<(u64, u64)> {
    let stat = rustix::fs::statvfs(path).ok()?;
    let total = stat.f_blocks.checked_mul(stat.f_frsize)?;
    let free = stat.f_bfree.checked_mul(stat.f_frsize)?;
    Some((total.saturating_sub(free), total))
}

#[cfg(test)]
mod tests {
    use super::*;

    const STAT: &str = "cpu  100 0 100 700 100 0 0 0 0 0\ncpu0 50 0 50 350 50 0 0 0 0 0\n\
                        cpu1 50 0 50 350 50 0 0 0 0 0\nintr 1 2 3\n";

    #[test]
    fn cpu_busy_share_comes_from_two_readings() {
        let before = parse_cpu(STAT).unwrap();
        assert_eq!(
            before,
            CpuTimes {
                busy: 200,
                total: 1000
            }
        );
        let later = parse_cpu("cpu  340 0 100 760 100 0 0 0 0 0\n").unwrap();
        assert_eq!(cpu_fraction(before, later), Some(0.8));
        assert_eq!(
            cpu_fraction(later, before),
            None,
            "a counter reset reads as unknown"
        );
        assert_eq!(parse_cores(STAT), Some(2));
    }

    #[test]
    fn memory_counts_what_is_not_available() {
        let meminfo = "MemTotal:       16000 kB\nMemFree:  1000 kB\nMemAvailable:   4000 kB\n";
        assert_eq!(parse_memory(meminfo), Some((12000 * 1024, 16000 * 1024)));
        assert_eq!(parse_memory("MemTotal: 1 kB\n"), None);
    }

    #[test]
    fn a_collector_reads_its_proc_directory() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("stat"), STAT).unwrap();
        std::fs::write(dir.path().join("uptime"), "3600.52 7000.1\n").unwrap();
        std::fs::write(dir.path().join("loadavg"), "0.42 0.30 0.20 1/300 4242\n").unwrap();
        std::fs::write(
            dir.path().join("meminfo"),
            "MemTotal: 2048 kB\nMemAvailable: 1024 kB\n",
        )
        .unwrap();
        let mut collector = HostCollector::new(dir.path(), dir.path());
        let first = collector.observe(5, "3.0.0");
        assert_eq!(
            first.cpu_fraction, None,
            "the first reading has nothing to compare"
        );
        assert_eq!(first.uptime_seconds, Some(3600));
        assert_eq!(first.load1, Some(0.42));
        assert_eq!(first.memory_used, Some(1024 * 1024));
        assert!(first.root_total.is_some());
        std::fs::write(
            dir.path().join("stat"),
            "cpu  150 0 150 800 100 0 0 0 0 0\n",
        )
        .unwrap();
        assert_eq!(collector.observe(6, "3.0.0").cpu_fraction, Some(0.5));
    }
}
