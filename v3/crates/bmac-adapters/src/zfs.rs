// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

//! `zfs list` and `zfs get`, always with `-H -p`.

use std::collections::BTreeMap;
use std::sync::LazyLock;

use bmac_core::DatasetName;
use bmac_exec::{Command, CommandRunner};
use regex::Regex;
use serde::Serialize;

use crate::zpool::parse_properties;
use crate::{AdapterError, programs, run_text};

static VOLUME_VMID_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"/(?:vm|base)-([1-9][0-9]*)-disk-[0-9]+$").unwrap());

/// Prefix of the snapshots Proxmox storage replication takes.
pub const REPLICATION_SNAPSHOT_PREFIX: &str = "__replicate_";
/// Prefix of the snapshots a staging VM is cloned from.
pub const STAGING_SNAPSHOT_PREFIX: &str = "stg-base-";

/// One zvol and a summary of its snapshots (v2 `host_storage.py`,
/// `parse_volumes`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Volume {
    pub name: String,
    /// The guest the zvol belongs to, from its `vm-N-disk-M` name.
    pub vmid: Option<u32>,
    pub volsize: Option<u64>,
    pub used: Option<u64>,
    /// Zero for a sparse zvol.
    pub refreservation: Option<u64>,
    pub used_by_snapshots: Option<u64>,
    pub snapshots: u32,
    pub replication_snapshots: u32,
    pub staging_snapshots: Vec<String>,
    /// Creation time of the newest replication snapshot, in Unix seconds.
    pub latest_replication_epoch: Option<u64>,
}

fn optional_int(text: &str) -> Option<u64> {
    let text = text.trim();
    if !text.is_empty() && text.bytes().all(|b| b.is_ascii_digit()) {
        text.parse().ok()
    } else {
        None
    }
}

/// Summarizes zvols from `zfs list -H -p -t volume -o
/// name,volsize,used,refreservation,usedbysnapshots` and their snapshots
/// from `zfs list -H -p -t snapshot -o name,used,creation`, sorted by name.
pub fn parse_volumes(volume_text: &str, snapshot_text: &str) -> Vec<Volume> {
    let mut volumes: BTreeMap<String, Volume> = BTreeMap::new();
    for line in volume_text.lines() {
        let fields: Vec<&str> = line.split('\t').collect();
        let [name, volsize, used, refreservation, by_snapshots] = fields[..] else {
            continue;
        };
        let vmid = VOLUME_VMID_RE
            .captures(name)
            .and_then(|captures| captures[1].parse().ok());
        volumes.insert(
            name.to_string(),
            Volume {
                name: name.to_string(),
                vmid,
                volsize: optional_int(volsize),
                used: optional_int(used),
                refreservation: optional_int(refreservation),
                used_by_snapshots: optional_int(by_snapshots),
                snapshots: 0,
                replication_snapshots: 0,
                staging_snapshots: Vec::new(),
                latest_replication_epoch: None,
            },
        );
    }
    for line in snapshot_text.lines() {
        let fields: Vec<&str> = line.split('\t').collect();
        let [full, _, creation] = fields[..] else {
            continue;
        };
        let Some((dataset, snapshot)) = full.split_once('@') else {
            continue;
        };
        let Some(volume) = volumes.get_mut(dataset) else {
            continue;
        };
        volume.snapshots += 1;
        let creation = optional_int(creation);
        if snapshot.starts_with(REPLICATION_SNAPSHOT_PREFIX) {
            volume.replication_snapshots += 1;
            if let Some(creation) = creation
                && volume
                    .latest_replication_epoch
                    .is_none_or(|latest| creation > latest)
            {
                volume.latest_replication_epoch = Some(creation);
            }
        } else if snapshot.starts_with(STAGING_SNAPSHOT_PREFIX) {
            volume.staging_snapshots.push(snapshot.to_string());
        }
    }
    volumes.into_values().collect()
}

/// A dataset's used and available bytes, as `zfs list -o used,available`
/// reports them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct Space {
    pub used: u64,
    pub available: u64,
}

/// Parses `zfs list -H -p -o used,available <dataset>`.
pub fn parse_space(text: &str) -> Option<Space> {
    let fields: Vec<&str> = text.split_whitespace().collect();
    match fields[..] {
        [used, available] => Some(Space {
            used: optional_int(used)?,
            available: optional_int(available)?,
        }),
        _ => None,
    }
}

/// The `zfs` command.
pub struct Zfs<'a> {
    runner: &'a dyn CommandRunner,
}

impl<'a> Zfs<'a> {
    pub fn new(runner: &'a dyn CommandRunner) -> Zfs<'a> {
        Zfs { runner }
    }

    fn command(args: &[&str]) -> Command {
        Command::new(programs::ZFS).args(args.iter().copied())
    }

    pub async fn properties(
        &self,
        dataset: &DatasetName,
        names: &[&str],
    ) -> Result<BTreeMap<String, String>, AdapterError> {
        let names = names.join(",");
        let text = run_text(
            self.runner,
            Zfs::command(&[
                "get",
                "-H",
                "-p",
                "-o",
                "property,value",
                &names,
                dataset.as_str(),
            ]),
        )
        .await?;
        Ok(parse_properties(&text))
    }

    pub async fn space(&self, dataset: &DatasetName) -> Result<Space, AdapterError> {
        let command = Zfs::command(&["list", "-H", "-p", "-o", "used,available", dataset.as_str()]);
        let text = run_text(self.runner, command.clone()).await?;
        parse_space(&text).ok_or_else(|| {
            AdapterError::parse(&command, format!("unexpected output {:?}", text.trim()))
        })
    }

    /// The zvols under `root` and their snapshots.
    pub async fn volumes(&self, root: &DatasetName) -> Result<Vec<Volume>, AdapterError> {
        let volumes = run_text(
            self.runner,
            Zfs::command(&[
                "list",
                "-H",
                "-p",
                "-t",
                "volume",
                "-r",
                "-o",
                "name,volsize,used,refreservation,usedbysnapshots",
                root.as_str(),
            ]),
        )
        .await?;
        let snapshots = run_text(
            self.runner,
            Zfs::command(&[
                "list",
                "-H",
                "-p",
                "-t",
                "snapshot",
                "-r",
                "-o",
                "name,used,creation",
                root.as_str(),
            ]),
        )
        .await?;
        Ok(parse_volumes(&volumes, &snapshots))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn space_needs_two_numbers() {
        assert_eq!(
            parse_space("429496729600\t1503238553600\n"),
            Some(Space {
                used: 429496729600,
                available: 1503238553600
            })
        );
        assert_eq!(parse_space("-\t5\n"), None);
        assert_eq!(parse_space("1\n"), None);
    }
}
