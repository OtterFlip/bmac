// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

//! The backup records in pmxcfs (`bmac_backup::records`), each written under
//! one lock for the backup directory with a revision check.

use std::path::{Path, PathBuf};
use std::time::Duration;

use bmac_backup::records::{
    ApplianceRecord, Lineage, PitRecord, Policy, RECORD_SCHEMA, ROOT, RestoreRecord,
};
use bmac_backup::settings::BackupSettings;
use bmac_pmxcfs::{LockOptions, Pmxcfs, PmxcfsError, RegistryLock, read_record, write_record};
use bmac_proto::api::{ApiError, ErrorCode};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::log;

const LOCK_PREFIX: &str = "bmac-backup-";

/// A stored record: its type name and revision.
pub trait Record: Serialize + DeserializeOwned {
    const TYPE: &'static str;
    fn header(&mut self) -> (&mut u64, &mut String, &mut u64);
}

macro_rules! record {
    ($t:ty, $name:literal) => {
        impl Record for $t {
            const TYPE: &'static str = $name;
            fn header(&mut self) -> (&mut u64, &mut String, &mut u64) {
                (
                    &mut self.schema_version,
                    &mut self.record_type,
                    &mut self.revision,
                )
            }
        }
    };
}

record!(Policy, "backup_policy");
record!(ApplianceRecord, "backup_appliance");
record!(Lineage, "backup_lineage");
record!(PitRecord, "backup_pit");
record!(SettingsRecord, "backup_settings");
record!(IndexRecord, "backup_appliance_index");
record!(RestoreRecord, "backup_restore");
record!(TransfersRecord, "backup_transfers");

/// How many finished transfers each host keeps.
pub const KEEP_TRANSFERS: usize = 200;

/// One host's newest finished transfers to appliances, newest first.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TransfersRecord {
    pub schema_version: u64,
    pub record_type: String,
    #[serde(default)]
    pub revision: u64,
    pub host: String,
    pub transfers: Vec<bmac_proto::backups::TransferView>,
}

/// An appliance's published PITs as it last reported them (`POST
/// /v1/index`): metadata only, for the restore wizard.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IndexRecord {
    pub schema_version: u64,
    pub record_type: String,
    #[serde(default)]
    pub revision: u64,
    pub appliance_uuid: Uuid,
    /// The appliance's own revision of its index.
    pub index_revision: u64,
    pub reported_at_ms: i64,
    pub pits: Vec<bmac_proto::appliance::IndexedPit>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SettingsRecord {
    pub schema_version: u64,
    pub record_type: String,
    #[serde(default)]
    pub revision: u64,
    pub settings: BackupSettings,
}

fn storage(e: PmxcfsError) -> ApiError {
    match e {
        PmxcfsError::RevisionConflict { .. } => ApiError::new(
            ErrorCode::Conflict,
            "the backup record changed since it was read; reload and try again",
        ),
        PmxcfsError::LockTimeout { .. } => {
            ApiError::new(ErrorCode::Unavailable, "the cluster is busy; try again")
        }
        other => ApiError::new(ErrorCode::Internal, format!("backup records: {other}")),
    }
}

fn internal(message: String) -> ApiError {
    ApiError::new(ErrorCode::Internal, message)
}

#[derive(Clone)]
pub struct BackupStore {
    pmxcfs: Pmxcfs,
    root: PathBuf,
    lock_timeout: Duration,
}

impl BackupStore {
    pub fn new(pmxcfs: Pmxcfs, root: impl Into<PathBuf>) -> BackupStore {
        BackupStore {
            pmxcfs,
            root: root.into(),
            lock_timeout: Duration::from_secs(15),
        }
    }

    pub fn system() -> BackupStore {
        BackupStore::new(Pmxcfs::system(), ROOT)
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    fn settings_path(&self) -> PathBuf {
        self.root.join("settings.json")
    }

    fn policy_path(&self, vm: &Uuid) -> PathBuf {
        self.root.join("policies").join(format!("{vm}.json"))
    }

    fn appliance_path(&self, appliance: &Uuid) -> PathBuf {
        self.root
            .join("appliances")
            .join(format!("{appliance}.json"))
    }

    fn index_path(&self, appliance: &Uuid) -> PathBuf {
        self.root.join("indexes").join(format!("{appliance}.json"))
    }

    fn restore_path(&self, restore: &Uuid) -> PathBuf {
        self.root.join("restores").join(format!("{restore}.json"))
    }

    fn transfers_path(&self, host: &str) -> PathBuf {
        self.root.join("transfers").join(format!("{host}.json"))
    }

    fn vm_dir(&self, vm: &Uuid) -> PathBuf {
        self.root.join("pits").join(vm.to_string())
    }

    fn lineage_path(&self, vm: &Uuid) -> PathBuf {
        self.vm_dir(vm).join("lineage.json")
    }

    fn pit_path(&self, vm: &Uuid, pit: &Uuid) -> PathBuf {
        self.vm_dir(vm).join(format!("{pit}.json"))
    }

    /// Runs `f` under the backup lock.
    pub fn locked<T>(
        &self,
        f: impl FnOnce(&Locked<'_>) -> Result<T, ApiError>,
    ) -> Result<T, ApiError> {
        self.pmxcfs
            .ensure_directory(&self.root, 0o700)
            .map_err(storage)?;
        let options = LockOptions {
            timeout: self.lock_timeout,
            prefix: LOCK_PREFIX.to_string(),
            short_lived: true,
            ..LockOptions::default()
        };
        let lock = RegistryLock::acquire(&self.pmxcfs, &self.root, &options).map_err(storage)?;
        let result = f(&Locked {
            store: self,
            lock: &lock,
        });
        let released = lock.release().map_err(storage);
        let value = result?;
        released?;
        Ok(value)
    }

    fn read<T: Record>(&self, path: &Path) -> Result<Option<T>, ApiError> {
        let Some(value) =
            read_record(&self.pmxcfs, path, T::TYPE, RECORD_SCHEMA).map_err(storage)?
        else {
            return Ok(None);
        };
        serde_json::from_value(value)
            .map(Some)
            .map_err(|e| internal(format!("{}: {e}", path.display())))
    }

    /// Every readable record in `dir` whose file is `<uuid>.json`; one that
    /// can't be read is reported and left out.
    fn read_dir<T: Record>(&self, dir: &Path) -> Result<Vec<(Uuid, T)>, ApiError> {
        let entries = match std::fs::read_dir(dir) {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(internal(format!("cannot list {}: {e}", dir.display()))),
        };
        let mut out = Vec::new();
        for entry in entries.flatten() {
            let path = entry.path();
            let Some(id) = path
                .file_name()
                .and_then(|n| n.to_str())
                .and_then(|n| n.strip_suffix(".json"))
                .and_then(|n| n.parse::<Uuid>().ok())
            else {
                continue;
            };
            match self.read::<T>(&path) {
                Ok(Some(record)) => out.push((id, record)),
                Ok(None) => {}
                Err(e) => log::warning(&format!("backups: {}", e.message)),
            }
        }
        out.sort_by_key(|(id, _)| *id);
        Ok(out)
    }

    pub fn settings(&self) -> Result<BackupSettings, ApiError> {
        Ok(self
            .read::<SettingsRecord>(&self.settings_path())?
            .map(|r| r.settings)
            .unwrap_or_default())
    }

    pub fn settings_record(&self) -> Result<Option<SettingsRecord>, ApiError> {
        self.read(&self.settings_path())
    }

    pub fn policy(&self, vm: &Uuid) -> Result<Option<Policy>, ApiError> {
        self.read(&self.policy_path(vm))
    }

    pub fn policies(&self) -> Result<Vec<Policy>, ApiError> {
        Ok(self
            .read_dir::<Policy>(&self.root.join("policies"))?
            .into_iter()
            .filter(|(id, p)| *id == p.vm_uuid)
            .map(|(_, p)| p)
            .collect())
    }

    pub fn appliance(&self, id: &Uuid) -> Result<Option<ApplianceRecord>, ApiError> {
        self.read(&self.appliance_path(id))
    }

    pub fn appliances(&self) -> Result<Vec<ApplianceRecord>, ApiError> {
        Ok(self
            .read_dir::<ApplianceRecord>(&self.root.join("appliances"))?
            .into_iter()
            .filter(|(id, a)| *id == a.appliance_uuid)
            .map(|(_, a)| a)
            .collect())
    }

    pub fn index(&self, appliance: &Uuid) -> Result<Option<IndexRecord>, ApiError> {
        self.read(&self.index_path(appliance))
    }

    pub fn restore(&self, id: &Uuid) -> Result<Option<RestoreRecord>, ApiError> {
        self.read(&self.restore_path(id))
    }

    pub fn restores(&self) -> Result<Vec<RestoreRecord>, ApiError> {
        Ok(self
            .read_dir::<RestoreRecord>(&self.root.join("restores"))?
            .into_iter()
            .filter(|(id, r)| *id == r.restore_uuid)
            .map(|(_, r)| r)
            .collect())
    }

    pub fn lineage(&self, vm: &Uuid) -> Result<Option<Lineage>, ApiError> {
        self.read(&self.lineage_path(vm))
    }

    /// The VM's catalog, oldest PIT first.
    pub fn pits(&self, vm: &Uuid) -> Result<Vec<PitRecord>, ApiError> {
        let mut pits: Vec<PitRecord> = self
            .read_dir::<PitRecord>(&self.vm_dir(vm))?
            .into_iter()
            .filter(|(id, p)| *id == p.pit_uuid && p.vm_uuid == *vm)
            .map(|(_, p)| p)
            .collect();
        pits.sort_by_key(|p| (p.taken_s, p.pit_uuid));
        Ok(pits)
    }

    pub fn pit(&self, vm: &Uuid, pit: &Uuid) -> Result<Option<PitRecord>, ApiError> {
        self.read(&self.pit_path(vm, pit))
    }

    /// Every host's finished transfers, newest first.
    pub fn transfers(&self) -> Result<Vec<bmac_proto::backups::TransferView>, ApiError> {
        let dir = self.root.join("transfers");
        let entries = match std::fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(internal(format!("cannot list {}: {e}", dir.display()))),
        };
        let mut out = Vec::new();
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            match self.read::<TransfersRecord>(&path) {
                Ok(Some(record)) => out.extend(record.transfers),
                Ok(None) => {}
                Err(e) => log::warning(&format!("backups: {}", e.message)),
            }
        }
        out.sort_by_key(|t| std::cmp::Reverse(t.at_ms));
        Ok(out)
    }

    /// The VMs with a catalog directory.
    pub fn cataloged_vms(&self) -> Result<Vec<Uuid>, ApiError> {
        let dir = self.root.join("pits");
        let entries = match std::fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(internal(format!("cannot list {}: {e}", dir.display()))),
        };
        let mut vms: Vec<Uuid> = entries
            .flatten()
            .filter_map(|e| e.file_name().to_str().and_then(|n| n.parse().ok()))
            .collect();
        vms.sort();
        Ok(vms)
    }
}

/// The store with its lock held.
pub struct Locked<'a> {
    store: &'a BackupStore,
    lock: &'a RegistryLock,
}

impl Locked<'_> {
    pub fn store(&self) -> &BackupStore {
        self.store
    }

    /// Writes `record` over the revision it carries (0 for a new one), and
    /// returns it as stored.
    fn write<T: Record>(&self, path: PathBuf, mut record: T) -> Result<T, ApiError> {
        let expected = {
            let (schema, kind, revision) = record.header();
            *schema = RECORD_SCHEMA;
            *kind = T::TYPE.to_string();
            (*revision > 0).then_some(*revision)
        };
        if let Some(parent) = path.parent() {
            self.store
                .pmxcfs
                .ensure_directory(parent, 0o700)
                .map_err(storage)?;
        }
        let value = serde_json::to_value(&record).expect("backup records serialize");
        let written = write_record(self.lock, &path, value, expected).map_err(storage)?;
        *record.header().2 = written.revision;
        Ok(record)
    }

    fn remove(&self, path: PathBuf) -> Result<(), ApiError> {
        if self.store.pmxcfs.exists(&path) {
            self.store.pmxcfs.remove(&path).map_err(storage)?;
        }
        Ok(())
    }

    pub fn save_settings(&self, settings: BackupSettings, revision: u64) -> Result<u64, ApiError> {
        settings
            .validate()
            .map_err(|m| ApiError::new(ErrorCode::Invalid, m))?;
        let record = SettingsRecord {
            schema_version: RECORD_SCHEMA,
            record_type: String::new(),
            revision,
            settings,
        };
        Ok(self.write(self.store.settings_path(), record)?.revision)
    }

    pub fn save_policy(&self, policy: Policy) -> Result<Policy, ApiError> {
        self.write(self.store.policy_path(&policy.vm_uuid), policy)
    }

    pub fn delete_policy(&self, vm: &Uuid) -> Result<(), ApiError> {
        self.remove(self.store.policy_path(vm))
    }

    pub fn save_appliance(&self, appliance: ApplianceRecord) -> Result<ApplianceRecord, ApiError> {
        self.write(
            self.store.appliance_path(&appliance.appliance_uuid),
            appliance,
        )
    }

    pub fn save_index(&self, index: IndexRecord) -> Result<IndexRecord, ApiError> {
        self.write(self.store.index_path(&index.appliance_uuid), index)
    }

    pub fn save_restore(&self, restore: RestoreRecord) -> Result<RestoreRecord, ApiError> {
        self.write(self.store.restore_path(&restore.restore_uuid), restore)
    }

    pub fn save_lineage(&self, lineage: Lineage) -> Result<Lineage, ApiError> {
        self.write(self.store.lineage_path(&lineage.vm_uuid), lineage)
    }

    pub fn save_pit(&self, pit: PitRecord) -> Result<PitRecord, ApiError> {
        self.write(self.store.pit_path(&pit.vm_uuid, &pit.pit_uuid), pit)
    }

    /// Adds a host's finished transfer, keeping the newest
    /// [`KEEP_TRANSFERS`].
    pub fn record_transfer(
        &self,
        transfer: bmac_proto::backups::TransferView,
    ) -> Result<(), ApiError> {
        let path = self.store.transfers_path(&transfer.host);
        let mut record = self
            .store
            .read::<TransfersRecord>(&path)?
            .unwrap_or(TransfersRecord {
                schema_version: RECORD_SCHEMA,
                record_type: String::new(),
                revision: 0,
                host: transfer.host.clone(),
                transfers: Vec::new(),
            });
        record.transfers.insert(0, transfer);
        record.transfers.truncate(KEEP_TRANSFERS);
        self.write(path, record).map(|_| ())
    }

    /// Removes the PIT's record; its children then descend from its parent,
    /// so a walk up the catalog still reaches every remaining ancestor.
    pub fn delete_pit(&self, vm: &Uuid, pit: &Uuid) -> Result<(), ApiError> {
        let Some(gone) = self.store.pit(vm, pit)? else {
            return Ok(());
        };
        for mut child in self.store.pits(vm)? {
            if child.parent == Some(*pit) {
                child.parent = gone.parent;
                self.save_pit(child)?;
            }
        }
        self.remove(self.store.pit_path(vm, pit))
    }
}
