// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

//! The appliance's ZFS operations behind one trait, so the pull logic is
//! tested against an in-memory pool. The real one runs `zfs` and `zpool`
//! through [`bmac_exec::CommandRunner`], except `zfs receive`, which reads
//! a stream.

use std::collections::BTreeMap;
use std::process::Stdio;
use std::time::Duration;

use bmac_core::SnapshotGuid;
use bmac_exec::{BoxFuture, Command, CommandRunner};
use bytes::Bytes;
use tokio::io::AsyncWriteExt;
use tokio::sync::mpsc;

pub const ZFS: &str = "/usr/sbin/zfs";
pub const ZPOOL: &str = "/usr/sbin/zpool";

/// A stream's chunks, ending with an error if the sender failed.
pub type Stream = mpsc::Receiver<Result<Bytes, String>>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VolumeFacts {
    pub volsize: u64,
    pub volblocksize: u64,
    /// Set while an interrupted `zfs receive -s` can be resumed.
    pub resume_token: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Receive {
    pub dataset: String,
    /// `dataset@snapshot` to receive a new branch's zvol as a clone of.
    pub origin: Option<String>,
}

impl Receive {
    /// `zfs receive`'s arguments: resumable, never `-F`, no mounts, no
    /// device nodes, and none of the stream's space reservations.
    pub fn args(&self) -> Vec<String> {
        let mut args: Vec<String> = ["receive", "-s", "-u", "-o", "volmode=none"]
            .map(String::from)
            .to_vec();
        for property in ["refreservation", "reservation"] {
            args.extend(["-x".to_string(), property.to_string()]);
        }
        if let Some(origin) = &self.origin {
            args.extend(["-o".to_string(), format!("origin={origin}")]);
        }
        args.push(self.dataset.clone());
        args
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PoolStatus {
    pub health: String,
    pub size_bytes: u64,
    pub free_bytes: u64,
}

pub trait Pool: Send + Sync {
    /// The GUIDs of those of `names` (snapshots or bookmarks) that exist.
    fn guids<'a>(
        &'a self,
        names: &'a [String],
    ) -> BoxFuture<'a, Result<BTreeMap<String, SnapshotGuid>, String>>;
    /// `None` when the zvol doesn't exist.
    fn volume<'a>(&'a self, dataset: &'a str)
    -> BoxFuture<'a, Result<Option<VolumeFacts>, String>>;
    /// The zvol's snapshots, oldest first.
    fn snapshots<'a>(
        &'a self,
        dataset: &'a str,
    ) -> BoxFuture<'a, Result<Vec<(String, SnapshotGuid)>, String>>;
    fn abort_receive<'a>(&'a self, dataset: &'a str) -> BoxFuture<'a, Result<(), String>>;
    /// Creates the filesystems above `dataset`, unmounted.
    fn ensure_parent<'a>(&'a self, dataset: &'a str) -> BoxFuture<'a, Result<(), String>>;
    /// Feeds `stream` to `zfs receive`; the bytes written.
    fn receive<'a>(
        &'a self,
        target: &'a Receive,
        stream: Stream,
    ) -> BoxFuture<'a, Result<u64, String>>;
    fn hold<'a>(&'a self, tag: &'a str, snapshot: &'a str) -> BoxFuture<'a, Result<(), String>>;
    fn release<'a>(&'a self, tag: &'a str, snapshot: &'a str) -> BoxFuture<'a, Result<(), String>>;
    fn destroy<'a>(&'a self, name: &'a str) -> BoxFuture<'a, Result<(), String>>;
    /// Destroys a zvol and its snapshots.
    fn destroy_tree<'a>(&'a self, dataset: &'a str) -> BoxFuture<'a, Result<(), String>>;
    /// What destroying `snapshots` of `dataset` together would free, by
    /// ZFS's own estimate.
    fn reclaim<'a>(
        &'a self,
        dataset: &'a str,
        snapshots: &'a [String],
    ) -> BoxFuture<'a, Result<u64, String>>;
    fn status(&self) -> BoxFuture<'_, Result<PoolStatus, String>>;
    /// The namespace's quota (0 for none) and the space it uses.
    fn quota<'a>(&'a self, dataset: &'a str) -> BoxFuture<'a, Result<(u64, u64), String>>;
}

/// How long `zfs receive` may leave its input unread before it is killed.
const RECEIVE_STALL: Duration = Duration::from_secs(300);

/// The real pool, through `zfs` and `zpool`.
pub struct RealPool<'r> {
    pub runner: &'r dyn CommandRunner,
    pub pool: String,
}

impl RealPool<'_> {
    async fn zfs(&self, args: &[&str]) -> Result<String, String> {
        self.run(ZFS, args).await
    }

    async fn run(&self, program: &str, args: &[&str]) -> Result<String, String> {
        let command = Command::new(program)
            .args(args.iter().copied())
            .timeout(Duration::from_secs(600));
        self.runner
            .run(command)
            .await
            .map(|o| o.stdout_text())
            .map_err(|e| e.to_string())
    }
}

fn number(text: &str) -> Result<u64, String> {
    text.trim()
        .parse()
        .map_err(|_| format!("not a number: {text:?}"))
}

impl Pool for RealPool<'_> {
    fn guids<'a>(
        &'a self,
        names: &'a [String],
    ) -> BoxFuture<'a, Result<BTreeMap<String, SnapshotGuid>, String>> {
        Box::pin(async move {
            let mut out = BTreeMap::new();
            if names.is_empty() {
                return Ok(out);
            }
            let mut args = vec!["get", "-Hp", "-o", "name,value", "guid"];
            args.extend(names.iter().map(String::as_str));
            // A missing name fails the whole call, so on a failure each
            // is asked alone.
            let text = match self.zfs(&args).await {
                Ok(text) => text,
                Err(_) => {
                    let mut text = String::new();
                    for name in names {
                        if let Ok(one) = self
                            .zfs(&["get", "-Hp", "-o", "name,value", "guid", name])
                            .await
                        {
                            text.push_str(&one);
                        }
                    }
                    text
                }
            };
            for line in text.lines() {
                let mut fields = line.split('\t');
                if let (Some(name), Some(value)) = (fields.next(), fields.next())
                    && let Ok(guid) = value.trim().parse::<u64>()
                {
                    out.insert(name.to_string(), SnapshotGuid::new(guid));
                }
            }
            Ok(out)
        })
    }

    fn volume<'a>(
        &'a self,
        dataset: &'a str,
    ) -> BoxFuture<'a, Result<Option<VolumeFacts>, String>> {
        Box::pin(async move {
            let text = match self
                .zfs(&[
                    "get",
                    "-Hp",
                    "-o",
                    "property,value",
                    "volsize,volblocksize,receive_resume_token",
                    dataset,
                ])
                .await
            {
                Ok(text) => text,
                Err(e) if e.contains("does not exist") => return Ok(None),
                Err(e) => return Err(e),
            };
            let mut values = BTreeMap::new();
            for line in text.lines() {
                if let Some((key, value)) = line.split_once('\t') {
                    values.insert(key.to_string(), value.trim().to_string());
                }
            }
            let token = values
                .get("receive_resume_token")
                .filter(|t| !t.is_empty() && t.as_str() != "-")
                .cloned();
            Ok(Some(VolumeFacts {
                volsize: values
                    .get("volsize")
                    .map(|v| number(v))
                    .transpose()?
                    .unwrap_or(0),
                volblocksize: values
                    .get("volblocksize")
                    .map(|v| number(v))
                    .transpose()?
                    .unwrap_or(0),
                resume_token: token,
            }))
        })
    }

    fn snapshots<'a>(
        &'a self,
        dataset: &'a str,
    ) -> BoxFuture<'a, Result<Vec<(String, SnapshotGuid)>, String>> {
        Box::pin(async move {
            let text = self
                .zfs(&[
                    "list",
                    "-Hp",
                    "-t",
                    "snapshot",
                    "-d",
                    "1",
                    "-s",
                    "createtxg",
                    "-o",
                    "name,guid",
                    dataset,
                ])
                .await?;
            Ok(text
                .lines()
                .filter_map(|line| {
                    let (name, guid) = line.split_once('\t')?;
                    let snapshot = name.split_once('@')?.1.to_string();
                    Some((snapshot, SnapshotGuid::new(guid.trim().parse().ok()?)))
                })
                .collect())
        })
    }

    fn abort_receive<'a>(&'a self, dataset: &'a str) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(async move { self.zfs(&["receive", "-A", dataset]).await.map(drop) })
    }

    fn ensure_parent<'a>(&'a self, dataset: &'a str) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(async move {
            let Some((parent, _)) = dataset.rsplit_once('/') else {
                return Ok(());
            };
            self.zfs(&[
                "create",
                "-p",
                "-o",
                "mountpoint=none",
                "-o",
                "canmount=off",
                parent,
            ])
            .await
            .map(drop)
        })
    }

    fn receive<'a>(
        &'a self,
        target: &'a Receive,
        mut stream: Stream,
    ) -> BoxFuture<'a, Result<u64, String>> {
        Box::pin(async move {
            let mut child = tokio::process::Command::new(ZFS)
                .args(target.args())
                .env_clear()
                .env("PATH", "/usr/sbin:/usr/bin:/sbin:/bin")
                .stdin(Stdio::piped())
                .stdout(Stdio::null())
                .stderr(Stdio::piped())
                .kill_on_drop(true)
                .spawn()
                .map_err(|e| format!("cannot run zfs receive: {e}"))?;
            let mut stdin = child.stdin.take().expect("piped");
            let mut written = 0u64;
            let mut failed = None;
            while let Some(chunk) = stream.recv().await {
                match chunk {
                    Ok(bytes) => {
                        match tokio::time::timeout(RECEIVE_STALL, stdin.write_all(&bytes)).await {
                            Ok(Ok(())) => {}
                            Ok(Err(e)) => {
                                failed = Some(format!("zfs receive stopped reading: {e}"));
                                break;
                            }
                            Err(_) => {
                                let _ = child.start_kill();
                                failed = Some(format!(
                                    "zfs receive read nothing for {} s",
                                    RECEIVE_STALL.as_secs()
                                ));
                                break;
                            }
                        }
                        written += bytes.len() as u64;
                    }
                    Err(e) => {
                        failed = Some(e);
                        break;
                    }
                }
            }
            drop(stdin);
            let output = child
                .wait_with_output()
                .await
                .map_err(|e| format!("zfs receive: {e}"))?;
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
            match (failed, output.status.success()) {
                (None, true) => Ok(written),
                (Some(e), _) => Err(format!("{e} after {written} bytes; zfs receive: {stderr}")),
                (None, false) => Err(format!(
                    "zfs receive failed after {written} bytes: {stderr}"
                )),
            }
        })
    }

    fn hold<'a>(&'a self, tag: &'a str, snapshot: &'a str) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(async move {
            match self.zfs(&["hold", tag, snapshot]).await {
                Err(e) if e.contains("tag already exists") => Ok(()),
                other => other.map(drop),
            }
        })
    }

    fn release<'a>(&'a self, tag: &'a str, snapshot: &'a str) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(async move {
            match self.zfs(&["release", tag, snapshot]).await {
                Err(e) if e.contains("no such tag") => Ok(()),
                other => other.map(drop),
            }
        })
    }

    fn destroy<'a>(&'a self, name: &'a str) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(async move { self.zfs(&["destroy", name]).await.map(drop) })
    }

    fn destroy_tree<'a>(&'a self, dataset: &'a str) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(async move {
            if dataset.contains('@') || dataset.split('/').count() < 4 {
                return Err(format!("{dataset} isn't a zvol of ours"));
            }
            self.zfs(&["destroy", "-r", dataset]).await.map(drop)
        })
    }

    fn reclaim<'a>(
        &'a self,
        dataset: &'a str,
        snapshots: &'a [String],
    ) -> BoxFuture<'a, Result<u64, String>> {
        Box::pin(async move {
            if snapshots.is_empty() {
                return Ok(0);
            }
            let target = format!("{dataset}@{}", snapshots.join(","));
            let text = self.zfs(&["destroy", "-nvp", &target]).await?;
            text.lines()
                .find_map(|l| l.strip_prefix("reclaim\t"))
                .map(number)
                .unwrap_or(Ok(0))
        })
    }

    fn status(&self) -> BoxFuture<'_, Result<PoolStatus, String>> {
        Box::pin(async move {
            let text = self
                .run(
                    ZPOOL,
                    &["list", "-Hp", "-o", "health,size,free", &self.pool],
                )
                .await?;
            let fields: Vec<&str> = text.trim().split('\t').collect();
            let [health, size, free] = fields[..] else {
                return Err(format!("zpool list: {text:?}"));
            };
            Ok(PoolStatus {
                health: health.to_string(),
                size_bytes: number(size)?,
                free_bytes: number(free)?,
            })
        })
    }

    fn quota<'a>(&'a self, dataset: &'a str) -> BoxFuture<'a, Result<(u64, u64), String>> {
        Box::pin(async move {
            let text = self
                .zfs(&["get", "-Hp", "-o", "value", "quota,used", dataset])
                .await?;
            let mut values = text.lines().map(number);
            match (values.next(), values.next()) {
                (Some(quota), Some(used)) => Ok((quota?, used?)),
                _ => Err(format!("zfs get quota,used: {text:?}")),
            }
        })
    }
}

/// Imports nothing and creates nothing: the pool's GUID, if it is
/// imported under `name`. Every imported pool is listed, since some
/// OpenZFS releases answer `zpool get` for a missing pool with a usage
/// error rather than "no such pool".
pub async fn pool_guid(runner: &dyn CommandRunner, name: &str) -> Result<Option<String>, String> {
    let command = Command::new(ZPOOL)
        .args(["list", "-Hp", "-o", "name,guid"])
        .timeout(Duration::from_secs(60));
    let output = runner.run(command).await.map_err(|e| e.to_string())?;
    Ok(output.stdout_text().lines().find_map(|line| {
        let (pool, guid) = line.split_once('\t')?;
        (pool == name).then(|| guid.trim().to_string())
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_receive_is_resumable_unmounted_without_devices_and_never_forced() {
        let plain = Receive {
            dataset: "bk/bmac/clusters/c/vms/v/b/d".into(),
            origin: None,
        };
        let args = plain.args();
        assert_eq!(
            args.join(" "),
            "receive -s -u -o volmode=none -x refreservation -x reservation bk/bmac/clusters/c/vms/v/b/d"
        );
        assert!(!args.iter().any(|a| a == "-F"));
        let clone = Receive {
            origin: Some("bk/x@bmac-pit-1".into()),
            ..plain
        };
        assert!(
            clone
                .args()
                .join(" ")
                .contains("-o origin=bk/x@bmac-pit-1 bk/")
        );
    }

    #[tokio::test]
    async fn guids_and_volumes_are_read_from_zfs() {
        let runner = bmac_exec::FakeRunner::new();
        runner.expect(
            ZFS,
            ["get", "-Hp", "-o", "name,value", "guid", "a@x", "a#y"],
            bmac_exec::Reply::ok("a@x\t17\na#y\t18\n"),
        );
        runner.expect(
            ZFS,
            [
                "get",
                "-Hp",
                "-o",
                "property,value",
                "volsize,volblocksize,receive_resume_token",
                "a",
            ],
            bmac_exec::Reply::ok(
                "volsize\t1048576\nvolblocksize\t16384\nreceive_resume_token\t-\n",
            ),
        );
        let pool = RealPool {
            runner: &runner,
            pool: "bk".into(),
        };
        let found = pool.guids(&["a@x".into(), "a#y".into()]).await.unwrap();
        assert_eq!(found["a#y"], SnapshotGuid::new(18));
        let facts = pool.volume("a").await.unwrap().unwrap();
        assert_eq!(
            (facts.volsize, facts.volblocksize, facts.resume_token),
            (1 << 20, 16384, None)
        );
        runner.assert_done();
    }
}
