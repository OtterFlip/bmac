// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

//! An in-memory [`Pool`] for the pull and retention tests. A fake stream's
//! first chunk is a JSON [`Header`] saying what it carries.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Mutex;

use bmac_core::SnapshotGuid;
use bmac_exec::BoxFuture;
use serde::{Deserialize, Serialize};

use crate::pool::{Pool, PoolStatus, Receive, Stream, VolumeFacts};

/// What the fake stream says it carries, as its first chunk.
#[derive(Debug, Serialize, Deserialize)]
pub struct Header {
    pub name: String,
    pub guid: u64,
    pub from: Option<u64>,
    pub resume: bool,
    pub volsize: u64,
    pub volblocksize: u64,
}

#[derive(Debug, Clone, Default)]
pub struct Zvol {
    pub snapshots: Vec<(String, SnapshotGuid)>,
    pub volsize: u64,
    pub volblocksize: u64,
    pub token: Option<String>,
    pub origin: Option<String>,
}

#[derive(Default)]
pub struct FakePool {
    pub zvols: Mutex<BTreeMap<String, Zvol>>,
    pub holds: Mutex<BTreeSet<(String, String)>>,
    pub aborted: Mutex<Vec<String>>,
    pub destroyed: Mutex<Vec<String>>,
    pub parents: Mutex<Vec<String>>,
    pub free: Mutex<u64>,
}

impl FakePool {
    pub fn new() -> FakePool {
        let pool = FakePool::default();
        *pool.free.lock().unwrap() = 1 << 40;
        pool
    }

    pub fn zvol(&self, dataset: &str) -> Option<Zvol> {
        self.zvols.lock().unwrap().get(dataset).cloned()
    }

    pub fn guid_of(&self, name: &str) -> Option<SnapshotGuid> {
        let (dataset, snap) = name.split_once('@')?;
        self.zvol(dataset)?
            .snapshots
            .iter()
            .find(|(n, _)| n == snap)
            .map(|(_, g)| *g)
    }

    fn apply(&self, target: &Receive, header: Header) -> Result<(), String> {
        let mut zvols = self.zvols.lock().unwrap();
        let guid = SnapshotGuid::new(header.guid);
        if header.resume {
            let zvol = zvols
                .get_mut(&target.dataset)
                .filter(|z| z.token.is_some())
                .ok_or("nothing to resume")?;
            zvol.token = None;
            zvol.volsize = header.volsize;
            zvol.volblocksize = header.volblocksize;
            zvol.snapshots.push((header.name, guid));
            return Ok(());
        }
        match (header.from, &target.origin) {
            (None, _) => {
                if zvols
                    .get(&target.dataset)
                    .is_some_and(|z| z.token.is_none())
                {
                    return Err("destination exists".into());
                }
            }
            (Some(from), None) => {
                let zvol = zvols.get(&target.dataset).ok_or("no base zvol")?;
                if zvol.snapshots.last().map(|(_, g)| g.get()) != Some(from) {
                    return Err("the newest snapshot isn't the stream's base".into());
                }
            }
            (Some(from), Some(origin)) => {
                drop(zvols);
                if self.guid_of(origin).map(|g| g.get()) != Some(from) {
                    return Err("the origin isn't the stream's base".into());
                }
                zvols = self.zvols.lock().unwrap();
                zvols.insert(
                    target.dataset.clone(),
                    Zvol {
                        origin: Some(origin.clone()),
                        ..Zvol::default()
                    },
                );
            }
        }
        let zvol = zvols.entry(target.dataset.clone()).or_default();
        zvol.token = None;
        zvol.volsize = header.volsize;
        zvol.volblocksize = header.volblocksize;
        zvol.snapshots.push((header.name, guid));
        Ok(())
    }
}

impl Pool for FakePool {
    fn guids<'a>(
        &'a self,
        names: &'a [String],
    ) -> BoxFuture<'a, Result<BTreeMap<String, SnapshotGuid>, String>> {
        Box::pin(async move {
            Ok(names
                .iter()
                .filter_map(|n| self.guid_of(n).map(|g| (n.clone(), g)))
                .collect())
        })
    }

    fn volume<'a>(
        &'a self,
        dataset: &'a str,
    ) -> BoxFuture<'a, Result<Option<VolumeFacts>, String>> {
        Box::pin(async move {
            Ok(self.zvol(dataset).map(|z| VolumeFacts {
                volsize: z.volsize,
                volblocksize: z.volblocksize,
                resume_token: z.token,
            }))
        })
    }

    fn snapshots<'a>(
        &'a self,
        dataset: &'a str,
    ) -> BoxFuture<'a, Result<Vec<(String, SnapshotGuid)>, String>> {
        Box::pin(async move { Ok(self.zvol(dataset).map(|z| z.snapshots).unwrap_or_default()) })
    }

    fn abort_receive<'a>(&'a self, dataset: &'a str) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(async move {
            self.aborted.lock().unwrap().push(dataset.to_string());
            let mut zvols = self.zvols.lock().unwrap();
            if zvols.get(dataset).is_some_and(|z| z.snapshots.is_empty()) {
                zvols.remove(dataset);
            } else if let Some(z) = zvols.get_mut(dataset) {
                z.token = None;
            }
            Ok(())
        })
    }

    fn ensure_parent<'a>(&'a self, dataset: &'a str) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(async move {
            self.parents.lock().unwrap().push(dataset.to_string());
            Ok(())
        })
    }

    fn receive<'a>(
        &'a self,
        target: &'a Receive,
        mut stream: Stream,
    ) -> BoxFuture<'a, Result<u64, String>> {
        Box::pin(async move {
            let mut data = Vec::new();
            while let Some(chunk) = stream.recv().await {
                match chunk {
                    Ok(bytes) => data.extend_from_slice(&bytes),
                    Err(e) => {
                        let mut zvols = self.zvols.lock().unwrap();
                        let zvol = zvols.entry(target.dataset.clone()).or_default();
                        zvol.token = Some(format!("token-{}", target.dataset));
                        return Err(e);
                    }
                }
            }
            let newline = data.iter().position(|b| *b == b'\n').ok_or("no header")?;
            let header: Header =
                serde_json::from_slice(&data[..newline]).map_err(|e| e.to_string())?;
            self.apply(target, header)?;
            Ok(data.len() as u64)
        })
    }

    fn hold<'a>(&'a self, tag: &'a str, snapshot: &'a str) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(async move {
            self.holds
                .lock()
                .unwrap()
                .insert((tag.to_string(), snapshot.to_string()));
            Ok(())
        })
    }

    fn release<'a>(&'a self, tag: &'a str, snapshot: &'a str) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(async move {
            self.holds
                .lock()
                .unwrap()
                .remove(&(tag.to_string(), snapshot.to_string()));
            Ok(())
        })
    }

    fn destroy<'a>(&'a self, name: &'a str) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(async move {
            if self.holds.lock().unwrap().iter().any(|(_, s)| s == name) {
                return Err(format!("cannot destroy '{name}': dataset is busy"));
            }
            let mut zvols = self.zvols.lock().unwrap();
            if zvols.values().any(|z| z.origin.as_deref() == Some(name)) {
                return Err(format!(
                    "cannot destroy '{name}': snapshot has dependent clones"
                ));
            }
            match name.split_once('@') {
                Some((dataset, snap)) => {
                    if let Some(z) = zvols.get_mut(dataset) {
                        z.snapshots.retain(|(n, _)| n != snap);
                    }
                }
                None => {
                    if zvols.get(name).is_some_and(|z| !z.snapshots.is_empty()) {
                        return Err(format!("cannot destroy '{name}': it has snapshots"));
                    }
                    zvols.remove(name);
                }
            }
            self.destroyed.lock().unwrap().push(name.to_string());
            Ok(())
        })
    }

    fn destroy_tree<'a>(&'a self, dataset: &'a str) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(async move {
            self.zvols.lock().unwrap().remove(dataset);
            self.destroyed.lock().unwrap().push(format!("-r {dataset}"));
            Ok(())
        })
    }

    fn reclaim<'a>(
        &'a self,
        _dataset: &'a str,
        snapshots: &'a [String],
    ) -> BoxFuture<'a, Result<u64, String>> {
        Box::pin(async move { Ok(snapshots.len() as u64 * 1000) })
    }

    fn status(&self) -> BoxFuture<'_, Result<PoolStatus, String>> {
        Box::pin(async move {
            Ok(PoolStatus {
                health: "ONLINE".into(),
                size_bytes: 1 << 40,
                free_bytes: *self.free.lock().unwrap(),
            })
        })
    }

    fn quota<'a>(&'a self, _dataset: &'a str) -> BoxFuture<'a, Result<(u64, u64), String>> {
        Box::pin(async move { Ok((0, 0)) })
    }
}
