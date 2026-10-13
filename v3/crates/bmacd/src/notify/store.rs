// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

//! Where alerts, silences, notification settings, and the outbox live: in
//! pmxcfs (`/etc/pve/priv/bmac/notify`), so every host shows the same ones,
//! or, without quorum, in this host's `/var/lib/bmac/notify-local` until
//! they can be merged. The Postmark token is
//! `/etc/pve/priv/bmac/secrets/postmark.token`, readable only by root.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use bmac_pmxcfs::{
    LockOptions, Pmxcfs, PmxcfsError, RealFs, RegistryLock, read_record, write_record,
};
use bmac_proto::alerts::{Alert, NotifySettings, Silence};
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

use super::alerts::Book;
use super::outbox::Outbox;

pub const NOTIFY_DIR: &str = "/etc/pve/priv/bmac/notify";
pub const LOCAL_DIR: &str = "/var/lib/bmac/notify-local";
pub const TOKEN_FILE: &str = "/etc/pve/priv/bmac/secrets/postmark.token";
const LOCK_PREFIX: &str = "bmac-notify-";
const SCHEMA: u64 = 1;
const MAX_TOKEN: usize = 200;

/// Every notification record, as read under one lock.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct State {
    pub book: Book,
    pub history: Vec<Alert>,
    pub silences: Vec<Silence>,
    pub settings: NotifySettings,
    pub settings_revision: Option<u64>,
    pub outbox: Outbox,
}

impl State {
    /// Whether there is anything to merge from a host-local store.
    pub fn is_empty(&self) -> bool {
        self.book.alerts.is_empty() && self.outbox.entries.is_empty()
    }
}

#[derive(Clone)]
pub struct NotifyStore {
    pmxcfs: Pmxcfs,
    dir: PathBuf,
    token: PathBuf,
    lock_timeout: Duration,
}

impl std::fmt::Debug for NotifyStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NotifyStore")
            .field("dir", &self.dir)
            .finish_non_exhaustive()
    }
}

fn storage(e: PmxcfsError) -> String {
    match e {
        PmxcfsError::LockTimeout { .. } => "the cluster is busy; try again".into(),
        other => format!("notifications: {other}"),
    }
}

impl NotifyStore {
    pub fn new(pmxcfs: Pmxcfs, dir: impl Into<PathBuf>, token: impl Into<PathBuf>) -> NotifyStore {
        NotifyStore {
            pmxcfs,
            dir: dir.into(),
            token: token.into(),
            lock_timeout: Duration::from_secs(15),
        }
    }

    pub fn system() -> NotifyStore {
        NotifyStore::new(Pmxcfs::system(), NOTIFY_DIR, TOKEN_FILE)
    }

    /// This host's store for when the cluster has no quorum.
    pub fn local() -> NotifyStore {
        NotifyStore::new(
            Pmxcfs::new("/var/lib/bmac", Arc::new(RealFs)),
            LOCAL_DIR,
            TOKEN_FILE,
        )
    }

    fn path(&self, name: &str) -> PathBuf {
        self.dir.join(format!("{name}.json"))
    }

    fn load<T: DeserializeOwned + Default>(
        &self,
        name: &str,
        field: &str,
    ) -> Result<(T, Option<u64>), String> {
        let path = self.path(name);
        let Some(value) = read_record(&self.pmxcfs, &path, name, SCHEMA).map_err(storage)? else {
            return Ok((T::default(), None));
        };
        let revision = value["revision"].as_u64();
        let parsed = serde_json::from_value(value[field].clone())
            .map_err(|e| format!("{}: {e}", path.display()))?;
        Ok((parsed, revision))
    }

    /// Reads every record without the lock, for showing them.
    pub fn read(&self) -> Result<State, String> {
        let (alerts, _) = self.load::<Vec<Alert>>("book", "alerts")?;
        let (history, _) = self.load("history", "alerts")?;
        let (silences, _) = self.load("silences", "silences")?;
        let (settings, settings_revision) = self.load("settings", "settings")?;
        let (outbox, _) = self.load("outbox", "outbox")?;
        Ok(State {
            book: Book { alerts },
            history,
            silences,
            settings,
            settings_revision,
            outbox,
        })
    }

    fn write<T: Serialize>(
        &self,
        lock: &RegistryLock,
        name: &str,
        field: &str,
        value: &T,
        revision: Option<u64>,
    ) -> Result<u64, String> {
        let record = json!({
            "schema_version": SCHEMA,
            "record_type": format!("notify_{name}"),
            field: value,
        });
        write_record(lock, &self.path(name), record, revision)
            .map(|w| w.revision)
            .map_err(storage)
    }

    /// Reads every record under the lock, applies `change`, and writes back
    /// the records it changed.
    pub fn update<T>(
        &self,
        change: impl FnOnce(&mut State) -> Result<T, String>,
    ) -> Result<T, String> {
        self.pmxcfs
            .ensure_directory(&self.dir, 0o700)
            .map_err(storage)?;
        let options = LockOptions {
            timeout: self.lock_timeout,
            prefix: LOCK_PREFIX.to_string(),
            ..LockOptions::default()
        };
        let lock = RegistryLock::acquire(&self.pmxcfs, &self.dir, &options).map_err(storage)?;
        let result = (|| -> Result<T, String> {
            let revisions = [
                self.load::<Value>("book", "alerts")?.1,
                self.load::<Value>("history", "alerts")?.1,
                self.load::<Value>("silences", "silences")?.1,
                self.load::<Value>("settings", "settings")?.1,
                self.load::<Value>("outbox", "outbox")?.1,
            ];
            let before = self.read()?;
            let mut after = before.clone();
            let value = change(&mut after)?;
            if after.book != before.book {
                self.write(&lock, "book", "alerts", &after.book.alerts, revisions[0])?;
            }
            if after.history != before.history {
                self.write(&lock, "history", "alerts", &after.history, revisions[1])?;
            }
            if after.silences != before.silences {
                self.write(&lock, "silences", "silences", &after.silences, revisions[2])?;
            }
            if after.settings != before.settings {
                self.write(&lock, "settings", "settings", &after.settings, revisions[3])?;
            }
            if after.outbox != before.outbox {
                self.write(&lock, "outbox", "outbox", &after.outbox, revisions[4])?;
            }
            Ok(value)
        })();
        let released = lock.release().map_err(storage);
        let value = result?;
        released?;
        Ok(value)
    }

    /// Empties this store, after its contents were merged elsewhere.
    pub fn clear(&self) -> Result<(), String> {
        for name in ["book", "outbox"] {
            let path = self.path(name);
            if self.pmxcfs.exists(&path) {
                self.pmxcfs.remove(&path).map_err(storage)?;
            }
        }
        Ok(())
    }

    pub fn token(&self) -> Option<String> {
        read_token(&self.token)
    }

    /// Stores the Postmark server token, or removes it when `token` is
    /// empty.
    pub fn set_token(&self, token: &str) -> Result<(), String> {
        let token = token.trim();
        if token.is_empty() {
            if self.pmxcfs.exists(&self.token) {
                self.pmxcfs.remove(&self.token).map_err(storage)?;
            }
            return Ok(());
        }
        if token.len() > MAX_TOKEN || !token.bytes().all(|b| b.is_ascii_graphic()) {
            return Err(
                "a Postmark server token is up to 200 printable characters with no spaces".into(),
            );
        }
        self.pmxcfs
            .write_secret(&self.token, format!("{token}\n").as_bytes())
            .map_err(storage)
    }
}

fn read_token(path: &Path) -> Option<String> {
    let text = std::fs::read_to_string(path).ok()?;
    let token = text.trim();
    (!token.is_empty()).then(|| token.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use bmac_pmxcfs::RealFs;

    fn store(dir: &Path) -> NotifyStore {
        NotifyStore::new(
            Pmxcfs::new(dir, Arc::new(RealFs)),
            dir.join("notify"),
            dir.join("secrets/postmark.token"),
        )
    }

    #[test]
    fn records_round_trip_and_unchanged_ones_are_not_rewritten() {
        let scratch = tempfile::tempdir().unwrap();
        let store = store(scratch.path());
        assert_eq!(store.read().unwrap(), State::default());
        store
            .update(|state| {
                state.settings.sender = Some("bmac@example.com".into());
                Ok(())
            })
            .unwrap();
        let state = store.read().unwrap();
        assert_eq!(state.settings.sender.as_deref(), Some("bmac@example.com"));
        assert_eq!(state.settings_revision, Some(1));
        assert!(!scratch.path().join("notify/book.json").exists());
        store.update(|_| Ok(())).unwrap();
        assert_eq!(store.read().unwrap().settings_revision, Some(1));
    }

    #[test]
    fn the_token_is_stored_apart_and_removable() {
        let scratch = tempfile::tempdir().unwrap();
        let store = store(scratch.path());
        assert_eq!(store.token(), None);
        assert!(store.set_token("has space").is_err());
        store
            .set_token(" 0a1b2c3d-1111-2222-3333-444455556666 ")
            .unwrap();
        assert_eq!(
            store.token().as_deref(),
            Some("0a1b2c3d-1111-2222-3333-444455556666")
        );
        let records = std::fs::read_dir(scratch.path().join("secrets"))
            .unwrap()
            .count();
        assert_eq!(records, 1);
        store.set_token("").unwrap();
        assert_eq!(store.token(), None);
    }
}
