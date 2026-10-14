// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

//! The appliance's journal, in the pool's metadata dataset (`BACKUP.md`,
//! "Pulling a PIT", step 6): the published PITs, the receives in progress,
//! and the PITs quarantined after a failed check. A PIT is usable for
//! restore, and counted by retention, only once it is published here; its
//! manifest is written and fsynced first, then the journal is replaced in
//! one rename.

use std::collections::BTreeMap;
use std::io;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use bmac_core::SnapshotGuid;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::fsx;

pub const JOURNAL_SCHEMA: u32 = 1;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StoredVolume {
    pub volume_uuid: Uuid,
    pub dataset: String,
    pub guid: SnapshotGuid,
    pub volsize: u64,
    pub volblocksize: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StoredPit {
    pub cluster_uuid: String,
    pub vm_uuid: Uuid,
    pub branch_uuid: Uuid,
    pub pit_uuid: Uuid,
    pub parent: Option<Uuid>,
    /// `bmac-pit-…`, the snapshot name on every volume.
    pub name: String,
    pub taken_s: i64,
    pub published_at_ms: i64,
    pub volumes: Vec<StoredVolume>,
    /// The bytes received for it.
    pub bytes: u64,
    pub manifest_sha256: String,
    /// The PIT this one was received on top of, if any.
    pub received_from: Option<Uuid>,
}

impl StoredPit {
    pub fn volume(&self, id: &Uuid) -> Option<&StoredVolume> {
        self.volumes.iter().find(|v| &v.volume_uuid == id)
    }

    pub fn snapshot(&self, volume: &StoredVolume) -> String {
        format!("{}@{}", volume.dataset, self.name)
    }
}

/// One volume of a receive in progress.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReceivingVolume {
    pub volume_uuid: Uuid,
    pub transfer_uuid: Uuid,
    pub dataset: String,
    pub done: bool,
    pub bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Receiving {
    pub cluster_uuid: String,
    pub vm_uuid: Uuid,
    pub branch_uuid: Uuid,
    pub pit_uuid: Uuid,
    pub base: Option<Uuid>,
    pub started_at_ms: i64,
    pub volumes: Vec<ReceivingVolume>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Quarantined {
    pub cluster_uuid: String,
    pub vm_uuid: Uuid,
    pub pit_uuid: Uuid,
    pub reason: String,
    pub at_ms: i64,
    /// The datasets the receive left, kept for a look until pruned.
    pub datasets: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Journal {
    pub schema_version: u32,
    pub appliance_uuid: Uuid,
    pub pool_guid: String,
    /// Bumped on every change to the published PITs: the index revision
    /// the cluster compares.
    pub revision: u64,
    pub pits: Vec<StoredPit>,
    pub receiving: Vec<Receiving>,
    #[serde(default)]
    pub quarantined: Vec<Quarantined>,
}

impl Journal {
    pub fn new(appliance_uuid: Uuid, pool_guid: &str) -> Journal {
        Journal {
            schema_version: JOURNAL_SCHEMA,
            appliance_uuid,
            pool_guid: pool_guid.to_string(),
            revision: 0,
            pits: Vec::new(),
            receiving: Vec::new(),
            quarantined: Vec::new(),
        }
    }

    pub fn pit(&self, cluster: &str, pit: &Uuid) -> Option<&StoredPit> {
        self.pits
            .iter()
            .find(|p| p.cluster_uuid == cluster && &p.pit_uuid == pit)
    }

    pub fn pits_of<'a>(
        &'a self,
        cluster: &'a str,
        vm: &'a Uuid,
    ) -> impl Iterator<Item = &'a StoredPit> {
        self.pits
            .iter()
            .filter(move |p| p.cluster_uuid == cluster && &p.vm_uuid == vm)
    }

    /// Each VM's newest published PIT, which the check-in reports.
    pub fn heads(&self, cluster: &str) -> BTreeMap<Uuid, &StoredPit> {
        let mut heads: BTreeMap<Uuid, &StoredPit> = BTreeMap::new();
        for pit in self.pits.iter().filter(|p| p.cluster_uuid == cluster) {
            let newer = heads.get(&pit.vm_uuid).is_none_or(|h| {
                (pit.taken_s, pit.published_at_ms) > (h.taken_s, h.published_at_ms)
            });
            if newer {
                heads.insert(pit.vm_uuid, pit);
            }
        }
        heads
    }

    pub fn receiving(&self, cluster: &str, vm: &Uuid) -> Option<&Receiving> {
        self.receiving
            .iter()
            .find(|r| r.cluster_uuid == cluster && &r.vm_uuid == vm)
    }

    /// Every dataset a published PIT or a receive uses.
    pub fn datasets(&self) -> std::collections::BTreeSet<&str> {
        self.pits
            .iter()
            .flat_map(|p| p.volumes.iter().map(|v| v.dataset.as_str()))
            .chain(
                self.receiving
                    .iter()
                    .flat_map(|r| r.volumes.iter().map(|v| v.dataset.as_str())),
            )
            .collect()
    }
}

/// Held while the journal or the retention settings change.
#[derive(Debug)]
pub struct JournalLock<'a> {
    _file: std::fs::File,
    _guard: tokio::sync::MutexGuard<'a, ()>,
}

/// The journal and manifests under the metadata dataset's mount point.
/// Clones share one lock, which every change to the journal holds from
/// its load to its save.
#[derive(Debug, Clone)]
pub struct Metadata {
    pub root: PathBuf,
    lock: Arc<tokio::sync::Mutex<()>>,
}

impl Metadata {
    pub fn new(root: impl Into<PathBuf>) -> Metadata {
        Metadata {
            root: root.into(),
            lock: Arc::default(),
        }
    }

    /// The journal lock: the clones' mutex, then an exclusive `flock` on
    /// `journal.lock`, which keeps the console's commands and the service
    /// apart.
    pub async fn lock(&self) -> io::Result<JournalLock<'_>> {
        let guard = self.lock.lock().await;
        let path = self.root.join("journal.lock");
        let file = tokio::task::spawn_blocking(move || {
            let file = std::fs::OpenOptions::new()
                .create(true)
                .truncate(false)
                .write(true)
                .mode(0o600)
                .open(path)?;
            file.lock()?;
            Ok::<_, io::Error>(file)
        })
        .await
        .map_err(io::Error::other)??;
        Ok(JournalLock {
            _file: file,
            _guard: guard,
        })
    }

    fn journal_path(&self) -> PathBuf {
        self.root.join("journal.json")
    }

    pub fn manifest_path(&self, cluster: &str, pit: &Uuid) -> PathBuf {
        self.root
            .join("manifests")
            .join(cluster)
            .join(format!("{pit}.json"))
    }

    pub fn load(&self) -> io::Result<Option<Journal>> {
        let journal: Option<Journal> = fsx::read_json(&self.journal_path())?;
        if let Some(j) = &journal
            && j.schema_version != JOURNAL_SCHEMA
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("journal schema {} is not supported", j.schema_version),
            ));
        }
        Ok(journal)
    }

    pub fn save(&self, journal: &Journal) -> io::Result<()> {
        fsx::write_json(&self.journal_path(), journal, 0o600)
    }

    /// Phase one of publication: the manifest, durable before the journal
    /// names it.
    pub fn write_manifest(&self, cluster: &str, pit: &Uuid, text: &str) -> io::Result<()> {
        fsx::write_atomic(&self.manifest_path(cluster, pit), text.as_bytes(), 0o600)
    }

    pub fn manifest(&self, cluster: &str, pit: &Uuid) -> io::Result<Option<String>> {
        match std::fs::read_to_string(self.manifest_path(cluster, pit)) {
            Ok(text) => Ok(Some(text)),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e),
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }
}

/// The datasets of `<pool>/bmac` (`BACKUP.md`, "Appliance pool layout").
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Layout {
    pub pool: String,
}

impl Layout {
    pub fn base(&self) -> String {
        format!("{}/bmac", self.pool)
    }

    pub fn metadata(&self) -> String {
        format!("{}/bmac/metadata", self.pool)
    }

    pub fn namespace(&self, cluster: &str) -> String {
        format!("{}/bmac/clusters/{cluster}", self.pool)
    }

    /// A volume's zvol on a line of history. `lineage` is the branch UUID,
    /// or the branch UUID and a number after a reseed, so a reseed never
    /// receives into the zvols of the lineage it replaces.
    pub fn volume(&self, cluster: &str, vm: &Uuid, lineage: &str, volume: &Uuid) -> String {
        format!("{}/vms/{vm}/{lineage}/{volume}", self.namespace(cluster))
    }

    /// The lineage directory of `dataset`, if it is one of ours.
    pub fn lineage_of<'a>(&self, cluster: &str, dataset: &'a str) -> Option<&'a str> {
        let prefix = format!("{}/vms/", self.namespace(cluster));
        let rest = dataset.strip_prefix(&prefix)?;
        let mut parts = rest.split('/');
        let (_vm, lineage, _volume) = (parts.next()?, parts.next()?, parts.next()?);
        parts.next().is_none().then_some(lineage)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pit(vm: u128, id: u128, taken_s: i64) -> StoredPit {
        StoredPit {
            cluster_uuid: "c".into(),
            vm_uuid: Uuid::from_u128(vm),
            branch_uuid: Uuid::from_u128(0xa),
            pit_uuid: Uuid::from_u128(id),
            parent: None,
            name: format!("bmac-pit-{id}"),
            taken_s,
            published_at_ms: taken_s * 1000,
            volumes: vec![],
            bytes: 0,
            manifest_sha256: String::new(),
            received_from: None,
        }
    }

    #[test]
    fn the_journal_round_trips_and_names_each_vms_newest_pit() {
        let dir = tempfile::tempdir().unwrap();
        let meta = Metadata::new(dir.path());
        assert!(meta.load().unwrap().is_none());
        let mut journal = Journal::new(Uuid::from_u128(1), "123");
        journal.pits = vec![pit(1, 10, 100), pit(1, 11, 200), pit(2, 20, 50)];
        meta.save(&journal).unwrap();
        let back = meta.load().unwrap().unwrap();
        assert_eq!(back, journal);
        let heads = back.heads("c");
        assert_eq!(heads[&Uuid::from_u128(1)].pit_uuid, Uuid::from_u128(11));
        assert_eq!(heads[&Uuid::from_u128(2)].pit_uuid, Uuid::from_u128(20));
        assert!(back.heads("other").is_empty());
    }

    #[test]
    fn the_layout_keeps_clusters_vms_and_lineages_apart() {
        let layout = Layout { pool: "bk".into() };
        let ds = layout.volume("c1", &Uuid::from_u128(1), "b-2", &Uuid::from_u128(3));
        assert!(ds.starts_with("bk/bmac/clusters/c1/vms/"));
        assert_eq!(layout.lineage_of("c1", &ds), Some("b-2"));
        assert_eq!(layout.lineage_of("c2", &ds), None);
        assert_eq!(layout.metadata(), "bk/bmac/metadata");
    }
}
