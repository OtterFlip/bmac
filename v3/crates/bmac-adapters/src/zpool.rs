// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

//! `zpool status`, `zpool list`, and `zpool get`.
//!
//! The status parser follows v2's `host_storage.py` (`parse_zpool_status`)
//! and also keeps the error counts and notes that `show_cluster_health.sh`
//! reads (`parse_pool`), so the layout and the health findings share one
//! parse.

use std::collections::BTreeMap;
use std::sync::LazyLock;

use bmac_core::PoolName;
use bmac_exec::{Command, CommandRunner};
use regex::Regex;
use serde::Serialize;

use crate::{AdapterError, programs, run_text};

const STATUS_KEYS: &[&str] = &[
    "pool",
    "state",
    "status",
    "action",
    "see",
    "scan",
    "remove",
    "checkpoint",
    "expand",
    "config",
    "errors",
];

static STATUS_KEY_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(&format!(r"^\s{{0,8}}({}):\s?(.*)$", STATUS_KEYS.join("|"))).unwrap()
});
static EVACUATING_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"Evacuation of (\S+) in progress").unwrap());
static POOL_LINE_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^\s*pool:\s+").unwrap());
static SCAN_ERRORS_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"with (\d+) errors").unwrap());
static VDEV_INDEX_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"-[0-9]+$").unwrap());

/// The allocation class a top-level vdev belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum VdevClass {
    Data,
    Logs,
    Cache,
    Spares,
    Special,
    Dedup,
}

impl VdevClass {
    fn from_section(name: &str) -> Option<VdevClass> {
        Some(match name {
            "logs" => VdevClass::Logs,
            "cache" => VdevClass::Cache,
            "spares" => VdevClass::Spares,
            "special" => VdevClass::Special,
            "dedup" => VdevClass::Dedup,
            _ => return None,
        })
    }

    pub fn as_str(self) -> &'static str {
        match self {
            VdevClass::Data => "data",
            VdevClass::Logs => "logs",
            VdevClass::Cache => "cache",
            VdevClass::Spares => "spares",
            VdevClass::Special => "special",
            VdevClass::Dedup => "dedup",
        }
    }
}

/// Read, write, and checksum error counts of one config row (`-p` prints
/// exact numbers).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct ErrorCounts {
    pub read: u64,
    pub write: u64,
    pub checksum: u64,
}

impl ErrorCounts {
    pub fn any(&self) -> bool {
        self.read != 0 || self.write != 0 || self.checksum != 0
    }
}

/// One row of the config tree: a vdev, a `replacing-N` or `spare-N`
/// grouping, or a leaf device.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ConfigNode {
    pub name: String,
    pub state: Option<String>,
    pub counts: Option<ErrorCounts>,
    /// Text after the counts, such as `(resilvering)` or `too many errors`.
    pub note: String,
    pub children: Vec<ConfigNode>,
}

impl ConfigNode {
    /// The leaf devices under this node, or the node itself when it is one.
    pub fn leaves(&self) -> Vec<&ConfigNode> {
        if self.children.is_empty() {
            return vec![self];
        }
        self.children.iter().flat_map(ConfigNode::leaves).collect()
    }

    /// `mirror`, `raidz2`, `draid1:...`, and so on, or `disk` for a single
    /// device vdev.
    pub fn vdev_type(&self) -> String {
        if self.children.is_empty() {
            "disk".to_string()
        } else {
            VDEV_INDEX_RE.replace(&self.name, "").into_owned()
        }
    }

    /// Whether a member of this vdev is being replaced.
    pub fn replacing(&self) -> bool {
        self.children
            .iter()
            .any(|child| child.name.starts_with("replacing-"))
    }
}

/// The top-level vdevs of one allocation class, in the order listed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Section {
    pub class: VdevClass,
    pub vdevs: Vec<ConfigNode>,
}

/// `zpool status -P -p` for one pool.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PoolStatus {
    pub pool: String,
    /// The pool row's own state and counts.
    pub root: ConfigNode,
    /// The header fields (`state`, `status`, `scan`, ...), one entry per
    /// line, with continuation lines kept separate.
    pub header: BTreeMap<String, Vec<String>>,
    /// Data vdevs first, then the auxiliary classes in the order listed.
    pub sections: Vec<Section>,
}

impl PoolStatus {
    fn joined(&self, key: &str, separator: &str) -> Option<String> {
        let lines = self.header.get(key)?;
        let text = lines.join(separator);
        (!text.is_empty()).then_some(text)
    }

    pub fn state(&self) -> Option<&str> {
        self.header
            .get("state")
            .and_then(|lines| lines.first())
            .map(String::as_str)
    }

    pub fn status(&self) -> Option<String> {
        self.joined("status", " ")
    }

    pub fn scan(&self) -> Option<String> {
        self.joined("scan", "\n")
    }

    pub fn remove(&self) -> Option<String> {
        self.joined("remove", "\n")
    }

    pub fn errors(&self) -> Option<String> {
        self.joined("errors", " ")
    }

    /// The top-level vdev being evacuated by `zpool remove`, if any.
    pub fn removing_vdev(&self) -> Option<String> {
        let remove = self.remove()?;
        EVACUATING_RE
            .captures(&remove)
            .map(|captures| captures[1].to_string())
    }

    pub fn vdevs(&self, class: VdevClass) -> &[ConfigNode] {
        self.sections
            .iter()
            .find(|section| section.class == class)
            .map(|section| section.vdevs.as_slice())
            .unwrap_or_default()
    }

    /// Problems an operator should see, as `show_cluster_health.sh` reports
    /// them (its pool section). `describe` names the physical disk behind a
    /// leaf path (`nvme0n1 SERIAL`); `healthy_summary` is the output of
    /// `zpool status -x <pool>`, consulted only when nothing else was found.
    pub fn findings(
        &self,
        describe: &dyn Fn(&str) -> Option<String>,
        healthy_summary: Option<&str>,
    ) -> Vec<String> {
        let pool = &self.pool;
        let mut found = Vec::new();
        let state = self.state().unwrap_or("?");
        if state != "ONLINE" {
            found.push(format!("pool {pool} is {state}"));
        }
        for section in &self.sections {
            let ok: &[&str] = if section.class == VdevClass::Spares {
                &["AVAIL", "INUSE"]
            } else {
                &["ONLINE"]
            };
            for vdev in &section.vdevs {
                node_findings(pool, vdev, ok, describe, &mut found);
            }
        }
        let errors = self.joined("errors", " ").unwrap_or_default();
        if !errors.is_empty() && !errors.starts_with("No known data errors") {
            found.push(format!("pool {pool} reports data errors: {errors}"));
        }
        let scan = self.joined("scan", " ").unwrap_or_default();
        if scan.contains("resilver in progress") {
            found.push(format!(
                "pool {pool} is resilvering; redundancy is reduced until it finishes"
            ));
        }
        if let Some(captures) = SCAN_ERRORS_RE.captures(&scan)
            && captures[1].parse::<u64>().is_ok_and(|n| n > 0)
        {
            found.push(format!(
                "the last scan of pool {pool} found {} errors",
                &captures[1]
            ));
        }
        if found.is_empty()
            && let Some(summary) = healthy_summary
            && !summary.contains(&format!("pool '{pool}' is healthy"))
        {
            let first = summary
                .lines()
                .map(str::trim)
                .find(|line| !line.is_empty())
                .unwrap_or_default();
            found.push(format!(
                "zpool status -x reports pool {pool} needs attention: {}",
                self.status().unwrap_or_else(|| first.to_string())
            ));
        }
        found
    }
}

fn node_findings(
    pool: &str,
    node: &ConfigNode,
    ok: &[&str],
    describe: &dyn Fn(&str) -> Option<String>,
    found: &mut Vec<String>,
) {
    if let Some(state) = &node.state {
        let leaf = node.children.is_empty();
        let (kind, label) = if leaf {
            let disk = describe(&node.name).unwrap_or_else(|| "?".to_string());
            ("member", format!("{} ({disk})", node.name))
        } else {
            ("vdev", node.name.clone())
        };
        if !ok.contains(&state.as_str()) && (leaf || state != "DEGRADED") {
            let note = if node.note.is_empty() {
                String::new()
            } else {
                format!(" ({})", node.note)
            };
            found.push(format!("{pool} {kind} {label} is {state}{note}"));
        }
        if let Some(counts) = node.counts.filter(ErrorCounts::any) {
            found.push(format!(
                "{pool} {kind} {label} has read/write/checksum errors {}/{}/{}",
                counts.read, counts.write, counts.checksum
            ));
        }
    }
    for child in &node.children {
        node_findings(pool, child, ok, describe, found);
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct ParseError(pub String);

/// Parses `zpool status -P [-p] <pool>` for one pool.
pub fn parse_status(text: &str) -> Result<PoolStatus, ParseError> {
    let mut header: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut config_lines: Vec<&str> = Vec::new();
    let mut current: Option<String> = None;
    for line in text.lines() {
        if let Some(captures) = STATUS_KEY_RE.captures(line)
            && !(current.as_deref() == Some("config") && line.starts_with('\t'))
        {
            let key = captures[1].to_string();
            if key != "config" {
                let lines = header.entry(key.clone()).or_default();
                let value = captures[2].trim();
                if !value.is_empty() {
                    lines.push(value.to_string());
                }
            }
            current = Some(key);
            continue;
        }
        match current.as_deref() {
            Some("config") => config_lines.push(line),
            Some(key) if !line.trim().is_empty() => header
                .entry(key.to_string())
                .or_default()
                .push(line.trim().to_string()),
            _ => {}
        }
    }
    let pool = header
        .get("pool")
        .and_then(|lines| lines.first())
        .cloned()
        .ok_or_else(|| ParseError("zpool status output has no pool line".to_string()))?;

    let mut root: Option<ConfigNode> = None;
    let mut sections: Vec<(VdevClass, Vec<(usize, ConfigNode)>)> = Vec::new();
    for line in config_lines {
        if line.trim().is_empty() {
            continue;
        }
        let body = line.strip_prefix('\t').unwrap_or(line);
        let tokens: Vec<&str> = body.split_whitespace().collect();
        if tokens.len() >= 2 && tokens[0] == "NAME" && tokens[1] == "STATE" {
            continue;
        }
        let indent = body.len() - body.trim_start_matches(' ').len();
        let node = config_node(&tokens)?;
        if indent == 0 {
            if node.name == pool {
                root = Some(node);
                sections.push((VdevClass::Data, Vec::new()));
            } else if let Some(class) = VdevClass::from_section(&node.name) {
                sections.push((class, Vec::new()));
            } else {
                return Err(ParseError(format!(
                    "unexpected zpool config entry: {}",
                    node.name
                )));
            }
            continue;
        }
        let Some((_, rows)) = sections.last_mut() else {
            return Err(ParseError(
                "zpool config lists devices before the pool".to_string(),
            ));
        };
        rows.push((indent, node));
    }
    let mut merged: Vec<Section> = Vec::new();
    for (class, rows) in sections {
        let vdevs = nest(rows);
        match merged.iter_mut().find(|section| section.class == class) {
            Some(section) => section.vdevs.extend(vdevs),
            None => merged.push(Section { class, vdevs }),
        }
    }
    if !merged
        .iter()
        .any(|section| section.class == VdevClass::Data)
    {
        merged.insert(
            0,
            Section {
                class: VdevClass::Data,
                vdevs: Vec::new(),
            },
        );
    }
    let root = root.unwrap_or(ConfigNode {
        name: pool.clone(),
        state: None,
        counts: None,
        note: String::new(),
        children: Vec::new(),
    });
    Ok(PoolStatus {
        pool,
        root,
        header,
        sections: merged,
    })
}

/// A row has counts when its third field is a number; spares print a note
/// there instead (`INUSE     currently in use`).
fn config_node(tokens: &[&str]) -> Result<ConfigNode, ParseError> {
    let has_counts = tokens.len() >= 5 && tokens[2].starts_with(|c: char| c.is_ascii_digit());
    let counts = if has_counts {
        let count = |text: &str| {
            text.parse::<u64>().map_err(|_| {
                ParseError(format!(
                    "error count {text:?} of {} is not a number; run zpool status with -p",
                    tokens[0]
                ))
            })
        };
        Some(ErrorCounts {
            read: count(tokens[2])?,
            write: count(tokens[3])?,
            checksum: count(tokens[4])?,
        })
    } else {
        None
    };
    Ok(ConfigNode {
        name: tokens[0].to_string(),
        state: tokens.get(1).map(|s| s.to_string()),
        counts,
        note: tokens
            .get(if has_counts { 5 } else { 2 }..)
            .unwrap_or_default()
            .join(" "),
        children: Vec::new(),
    })
}

/// Builds the tree from rows in listing order: a row belongs to the nearest
/// earlier row with a smaller indent.
fn nest(rows: Vec<(usize, ConfigNode)>) -> Vec<ConfigNode> {
    let mut top: Vec<ConfigNode> = Vec::new();
    let mut stack: Vec<(usize, ConfigNode)> = Vec::new();
    let attach =
        |node: ConfigNode, stack: &mut Vec<(usize, ConfigNode)>, top: &mut Vec<ConfigNode>| {
            match stack.last_mut() {
                Some((_, parent)) => parent.children.push(node),
                None => top.push(node),
            }
        };
    for (indent, node) in rows {
        while stack.last().is_some_and(|(depth, _)| *depth >= indent) {
            let (_, done) = stack.pop().unwrap();
            attach(done, &mut stack, &mut top);
        }
        stack.push((indent, node));
    }
    while let Some((_, done)) = stack.pop() {
        attach(done, &mut stack, &mut top);
    }
    top
}

/// Splits `zpool status` output that covers several pools into one text per
/// pool.
pub fn split_pools(text: &str) -> Vec<String> {
    let mut blocks: Vec<String> = Vec::new();
    for line in text.lines() {
        if POOL_LINE_RE.is_match(line) || blocks.is_empty() {
            blocks.push(String::new());
        }
        let block = blocks.last_mut().unwrap();
        block.push_str(line);
        block.push('\n');
    }
    blocks
        .into_iter()
        .filter(|block| block.lines().any(|line| POOL_LINE_RE.is_match(line)))
        .collect()
}

/// One row of `zpool list -v -H -p -P`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ListRow {
    pub size: Option<u64>,
    pub allocated: Option<u64>,
    pub free: Option<u64>,
    pub health: Option<String>,
}

fn optional_int(text: &str) -> Option<u64> {
    let text = text.trim();
    if !text.is_empty() && text.bytes().all(|b| b.is_ascii_digit()) {
        text.parse().ok()
    } else {
        None
    }
}

/// Parses `zpool list -v -H -p -P -o name,size,allocated,free,health`,
/// keyed by pool, vdev, or member name.
///
/// Only the pool row follows `-o`; OpenZFS prints every vdev and member row
/// with its fixed ten columns (name, size, allocated, free, checkpoint,
/// expandsize, fragmentation, capacity, dedup, health).
pub fn parse_list(text: &str) -> BTreeMap<String, ListRow> {
    let mut rows = BTreeMap::new();
    for line in text.lines() {
        let fields: Vec<&str> = line.trim().split('\t').map(str::trim).collect();
        if fields[0].is_empty() {
            continue;
        }
        let health = match fields.len() {
            5 => fields[4],
            10 => fields[9],
            _ => continue,
        };
        rows.insert(
            fields[0].to_string(),
            ListRow {
                size: optional_int(fields[1]),
                allocated: optional_int(fields[2]),
                free: optional_int(fields[3]),
                health: (health != "-").then(|| health.to_string()),
            },
        );
    }
    rows
}

/// Parses `zpool get` or `zfs get` with `-H -p -o property,value`.
pub fn parse_properties(text: &str) -> BTreeMap<String, String> {
    text.lines()
        .filter_map(|line| line.split_once('\t'))
        .map(|(key, value)| (key.trim().to_string(), value.trim().to_string()))
        .collect()
}

/// An integer property, or `None` when it is absent or `-`.
pub fn int_property(properties: &BTreeMap<String, String>, key: &str) -> Option<u64> {
    properties.get(key).and_then(|value| optional_int(value))
}

/// The `zpool` command.
pub struct Zpool<'a> {
    runner: &'a dyn CommandRunner,
}

impl<'a> Zpool<'a> {
    pub fn new(runner: &'a dyn CommandRunner) -> Zpool<'a> {
        Zpool { runner }
    }

    fn command(args: &[&str]) -> Command {
        Command::new(programs::ZPOOL).args(args.iter().copied())
    }

    /// The imported pools.
    pub async fn names(&self) -> Result<Vec<String>, AdapterError> {
        let text = run_text(self.runner, Zpool::command(&["list", "-H", "-o", "name"])).await?;
        Ok(text.split_whitespace().map(str::to_string).collect())
    }

    pub async fn status_text(&self, pool: &PoolName) -> Result<String, AdapterError> {
        run_text(
            self.runner,
            Zpool::command(&["status", "-P", "-p", pool.as_str()]),
        )
        .await
    }

    pub async fn status(&self, pool: &PoolName) -> Result<PoolStatus, AdapterError> {
        let command = Zpool::command(&["status", "-P", "-p", pool.as_str()]);
        let text = run_text(self.runner, command.clone()).await?;
        parse_status(&text).map_err(|e| AdapterError::parse(&command, e.0))
    }

    /// `zpool status` for every imported pool, with symlinks resolved.
    pub async fn status_all_text(&self) -> Result<String, AdapterError> {
        run_text(self.runner, Zpool::command(&["status", "-L", "-P", "-p"])).await
    }

    /// `zpool status -x <pool>`: one line when the pool is healthy.
    pub async fn health_summary(&self, pool: &PoolName) -> Result<String, AdapterError> {
        run_text(
            self.runner,
            Zpool::command(&["status", "-x", pool.as_str()]),
        )
        .await
    }

    pub async fn list_text(&self, pool: &PoolName) -> Result<String, AdapterError> {
        run_text(
            self.runner,
            Zpool::command(&[
                "list",
                "-v",
                "-H",
                "-p",
                "-P",
                "-o",
                "name,size,allocated,free,health",
                pool.as_str(),
            ]),
        )
        .await
    }

    pub async fn properties(
        &self,
        pool: &PoolName,
        names: &[&str],
    ) -> Result<BTreeMap<String, String>, AdapterError> {
        let names = names.join(",");
        let text = run_text(
            self.runner,
            Zpool::command(&[
                "get",
                "-H",
                "-p",
                "-o",
                "property,value",
                &names,
                pool.as_str(),
            ]),
        )
        .await?;
        Ok(parse_properties(&text))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rows_nest_by_indent_and_sections_merge() {
        let text = "  pool: tank\n state: ONLINE\nconfig:\n\n\
            \tNAME        STATE     READ WRITE CKSUM\n\
            \ttank        ONLINE       0     0     0\n\
            \t  mirror-0  ONLINE       0     0     0\n\
            \t    /dev/a  ONLINE       0     0     0\n\
            \t    /dev/b  ONLINE       0     0     0\n\
            \t  /dev/c    ONLINE       0     0     0\n\
            \tlogs\n\
            \t  /dev/d    ONLINE       0     0     0\n\n\
            errors: No known data errors\n";
        let status = parse_status(text).unwrap();
        let data = status.vdevs(VdevClass::Data);
        assert_eq!(data.len(), 2);
        assert_eq!(data[0].vdev_type(), "mirror");
        assert_eq!(
            data[0]
                .leaves()
                .iter()
                .map(|l| &l.name[..])
                .collect::<Vec<_>>(),
            ["/dev/a", "/dev/b"]
        );
        assert_eq!(data[1].vdev_type(), "disk");
        assert_eq!(status.vdevs(VdevClass::Logs)[0].name, "/dev/d");
        assert_eq!(status.root.state.as_deref(), Some("ONLINE"));
        assert_eq!(status.errors().as_deref(), Some("No known data errors"));
    }

    #[test]
    fn human_readable_counts_are_refused() {
        let text = "  pool: tank\nconfig:\n\ttank  ONLINE  1.2K  0  0\n";
        assert_eq!(
            parse_status(text).unwrap_err().0,
            "error count \"1.2K\" of tank is not a number; run zpool status with -p"
        );
    }

    #[test]
    fn several_pools_split_into_blocks() {
        let text = "  pool: a\n state: ONLINE\n\n  pool: b\n state: DEGRADED\n";
        let blocks = split_pools(text);
        assert_eq!(blocks.len(), 2);
        assert_eq!(parse_status(&blocks[1]).unwrap().state(), Some("DEGRADED"));
        assert!(split_pools("no pools available\n").is_empty());
    }
}
