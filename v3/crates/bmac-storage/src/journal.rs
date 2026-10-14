// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

//! The host storage journal (`/var/lib/bmac/storage-state.json`): what the
//! storage workflows must remember that the pool itself forgets or can't
//! say, kept on each host (v2 `storage_state.py`):
//!
//! - when each production guest was last trimmed for this host, and the
//!   last scrubs with their results, so a removal reuses recent ones;
//! - each vdev removal with its members' identities, because ZFS forgets
//!   which disks a vdev used once its removal finishes;
//! - each disk chosen to replace a member, with the member it replaces,
//!   until the old member's LUKS mapping and crypttab line are retired:
//!   the old disk is often gone, and only this record says what it was;
//! - each new vdev from the moment its disks are chosen until it is in the
//!   pool, numbered, never reusing a number;
//! - a failure drill under way.
//!
//! Every change takes an exclusive lock and replaces the file atomically.
//! Times are the host's epoch seconds.

use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use bmac_core::{DiskIdentity, GuestName};
use serde::{Deserialize, Serialize};

use crate::layout::{VdevClass, VdevType};

pub const JOURNAL_FILE: &str = "/var/lib/bmac/storage-state.json";
pub const SCHEMA_VERSION: u32 = 1;
/// A trim or clean scrub younger than this is reused by a removal.
pub const RECENT_SECONDS: i64 = 24 * 60 * 60;
const MAX_SCRUBS: usize = 20;
const MAX_REMOVALS: usize = 200;
const MAX_LINE: usize = 500;

#[derive(Debug, thiserror::Error)]
pub enum JournalError {
    #[error("{0}")]
    Invalid(String),
    #[error("{what} {path}: {source}")]
    Io {
        what: &'static str,
        path: PathBuf,
        source: io::Error,
    },
}

fn invalid(message: impl Into<String>) -> JournalError {
    JournalError::Invalid(message.into())
}

pub type Result<T> = std::result::Result<T, JournalError>;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Trim {
    pub completed_at: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Scrub {
    pub completed_at: i64,
    /// `zpool status`'s scan line.
    pub result: String,
}

impl Scrub {
    pub fn clean(&self) -> bool {
        self.result.contains("with 0 errors")
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RemovalState {
    /// `zpool remove` was started, or may have been.
    Requested,
    /// The pool never started it, or cancelled it.
    Failed,
    /// Finished, and its disks released.
    Retired,
}

/// A member of a vdev being removed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RemovedMember {
    /// `None` for a member whose disk was already gone.
    pub identity: Option<DiskIdentity>,
    /// Its LUKS mapper, to close once the removal finishes.
    pub mapper: Option<String>,
    pub size: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Note {
    pub at: i64,
    pub note: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Removal {
    pub id: String,
    /// The vdev's name in `zpool status`, such as `mirror-2`.
    pub vdev: String,
    pub members: Vec<RemovedMember>,
    pub state: RemovalState,
    pub requested_at: i64,
    pub updated_at: i64,
    #[serde(default)]
    pub notes: Vec<Note>,
}

/// The member a disk was chosen to replace.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Replacement {
    /// The old member's leaf, as `zpool status -P` printed it.
    pub leaf: String,
    /// The old member's disk, when it was still installed. A failure drill
    /// replaces a disk with itself.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub old: Option<DiskIdentity>,
    /// The old member's LUKS mapper, to retire once the new one is in.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mapper: Option<String>,
    pub vdev: String,
    pub recorded_at: i64,
}

/// A new vdev whose disks were chosen.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Addition {
    pub number: u64,
    pub class: VdevClass,
    #[serde(rename = "type")]
    pub kind: VdevType,
    pub members: Vec<DiskIdentity>,
    pub recorded_at: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SimulationStage {
    /// The member was taken offline and its LUKS closed.
    Offline,
    /// Its disk was erased; it now looks like a new disk in the old one's
    /// place.
    Erased,
}

/// A failure drill under way: one member made to look pulled.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Simulation {
    pub vdev: String,
    /// The member's name in `zpool status`.
    pub leaf: String,
    pub disk: DiskIdentity,
    pub mapper: Option<String>,
    pub stage: SimulationStage,
    pub started_at: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct State {
    pub schema_version: u32,
    #[serde(default)]
    pub trims: BTreeMap<GuestName, Trim>,
    #[serde(default)]
    pub scrubs: Vec<Scrub>,
    #[serde(default)]
    pub removals: Vec<Removal>,
    /// Keyed by the identity of the new disk.
    #[serde(default)]
    pub replacements: BTreeMap<DiskIdentity, Replacement>,
    #[serde(default)]
    pub additions: Vec<Addition>,
    /// The last addition number handed out.
    #[serde(default)]
    pub last_addition: u64,
    #[serde(default)]
    pub simulation: Option<Simulation>,
}

impl Default for State {
    fn default() -> State {
        State {
            schema_version: SCHEMA_VERSION,
            trims: BTreeMap::new(),
            scrubs: Vec::new(),
            removals: Vec::new(),
            replacements: BTreeMap::new(),
            additions: Vec::new(),
            last_addition: 0,
            simulation: None,
        }
    }
}

fn one_line(what: &str, text: &str) -> Result<()> {
    if text.is_empty() || text.contains('\n') || text.len() > MAX_LINE {
        return Err(invalid(format!(
            "{what} must be one line of at most {MAX_LINE} characters"
        )));
    }
    Ok(())
}

impl State {
    pub fn validate(&self) -> Result<()> {
        if self.schema_version != SCHEMA_VERSION {
            return Err(invalid(format!(
                "unsupported storage journal schema {}",
                self.schema_version
            )));
        }
        if self
            .removals
            .iter()
            .filter(|r| r.state == RemovalState::Requested)
            .count()
            > 1
        {
            return Err(invalid("more than one vdev removal is in progress"));
        }
        for removal in &self.removals {
            if removal.members.is_empty() {
                return Err(invalid(format!("removal {} has no members", removal.id)));
            }
        }
        for scrub in &self.scrubs {
            one_line("a scrub result", &scrub.result)?;
        }
        if self.additions.iter().any(|a| a.number > self.last_addition) {
            return Err(invalid("an addition number was never handed out"));
        }
        Ok(())
    }

    pub fn record_trims(&mut self, guests: &[GuestName], now: i64) {
        for guest in guests {
            self.trims.insert(*guest, Trim { completed_at: now });
        }
    }

    pub fn recently_trimmed(&self, guest: GuestName, now: i64) -> bool {
        self.trims
            .get(&guest)
            .is_some_and(|t| now - t.completed_at < RECENT_SECONDS)
    }

    pub fn record_scrub(&mut self, result: &str, now: i64) -> Result<()> {
        one_line("a scrub result", result)?;
        self.scrubs.push(Scrub {
            completed_at: now,
            result: result.into(),
        });
        let excess = self.scrubs.len().saturating_sub(MAX_SCRUBS);
        self.scrubs.drain(..excess);
        Ok(())
    }

    /// The last scrub, if it was clean and is recent.
    pub fn recent_clean_scrub(&self, now: i64) -> Option<&Scrub> {
        self.scrubs
            .last()
            .filter(|s| s.clean() && now - s.completed_at < RECENT_SECONDS)
    }

    pub fn removal_in_progress(&self) -> Option<&Removal> {
        self.removals
            .iter()
            .find(|r| r.state == RemovalState::Requested)
    }

    /// Records a removal about to be started; its ID.
    pub fn record_removal(
        &mut self,
        vdev: &str,
        members: Vec<RemovedMember>,
        now: i64,
    ) -> Result<String> {
        one_line("a vdev name", vdev)?;
        if members.is_empty() {
            return Err(invalid("a removal needs its members"));
        }
        if !members.iter().any(|m| m.identity.is_some()) {
            return Err(invalid(
                "at least one member of a removed vdev must be an installed disk",
            ));
        }
        if let Some(other) = self.removal_in_progress() {
            return Err(invalid(format!(
                "the removal of {} is still in progress",
                other.vdev
            )));
        }
        let id = format!("removal-{now}-{vdev}");
        self.removals.push(Removal {
            id: id.clone(),
            vdev: vdev.into(),
            members,
            state: RemovalState::Requested,
            requested_at: now,
            updated_at: now,
            notes: Vec::new(),
        });
        let excess = self.removals.len().saturating_sub(MAX_REMOVALS);
        self.removals.drain(..excess);
        Ok(id)
    }

    pub fn set_removal(
        &mut self,
        id: &str,
        state: RemovalState,
        note: Option<&str>,
        now: i64,
    ) -> Result<()> {
        if let Some(note) = note {
            one_line("a note", note)?;
        }
        let removal = self
            .removals
            .iter_mut()
            .find(|r| r.id == id)
            .ok_or_else(|| invalid(format!("no removal {id}")))?;
        removal.state = state;
        removal.updated_at = now;
        if let Some(note) = note {
            removal.notes.push(Note {
                at: now,
                note: note.into(),
            });
        }
        Ok(())
    }

    /// Records `disk` as chosen to replace `replacement.leaf`.
    pub fn record_replacement(
        &mut self,
        disk: &DiskIdentity,
        replacement: Replacement,
    ) -> Result<()> {
        one_line("a vdev name", &replacement.vdev)?;
        one_line("a leaf", &replacement.leaf)?;
        if let Some((other, _)) = self
            .replacements
            .iter()
            .find(|(d, r)| *d != disk && r.vdev == replacement.vdev && r.leaf == replacement.leaf)
        {
            return Err(invalid(format!(
                "{} is already chosen to replace {}",
                other.value(),
                replacement.leaf
            )));
        }
        self.replacements.insert(disk.clone(), replacement);
        Ok(())
    }

    pub fn clear_replacement(&mut self, disk: &DiskIdentity) {
        self.replacements.remove(disk);
    }

    /// The member `disk` was chosen to replace.
    pub fn replacing(&self, disk: &DiskIdentity) -> Option<&Replacement> {
        self.replacements.get(disk)
    }

    /// Records a new vdev's chosen disks; its number.
    pub fn begin_addition(
        &mut self,
        class: VdevClass,
        kind: VdevType,
        members: Vec<DiskIdentity>,
        now: i64,
    ) -> Result<u64> {
        if members.is_empty() {
            return Err(invalid("a new vdev needs disks"));
        }
        for member in &members {
            if let Some(other) = self.additions.iter().find(|a| a.members.contains(member)) {
                return Err(invalid(format!(
                    "{} is already chosen for new vdev {}",
                    member.value(),
                    other.number
                )));
            }
        }
        self.last_addition += 1;
        self.additions.push(Addition {
            number: self.last_addition,
            class,
            kind,
            members,
            recorded_at: now,
        });
        Ok(self.last_addition)
    }

    pub fn finish_addition(&mut self, number: u64) {
        self.additions.retain(|a| a.number != number);
    }

    pub fn begin_simulation(&mut self, simulation: Simulation) -> Result<()> {
        if let Some(other) = &self.simulation {
            return Err(invalid(format!(
                "a failure drill on {} is already under way",
                other.leaf
            )));
        }
        self.simulation = Some(simulation);
        Ok(())
    }
}

/// The journal file and its lock.
#[derive(Debug, Clone)]
pub struct Journal {
    path: PathBuf,
}

impl Journal {
    pub fn new(path: impl Into<PathBuf>) -> Journal {
        Journal { path: path.into() }
    }

    pub fn system() -> Journal {
        Journal::new(JOURNAL_FILE)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    fn io(&self, what: &'static str, source: io::Error) -> JournalError {
        JournalError::Io {
            what,
            path: self.path.clone(),
            source,
        }
    }

    /// The journal, or an empty one if there is none yet.
    pub fn read(&self) -> Result<State> {
        match std::fs::symlink_metadata(&self.path) {
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(State::default()),
            Err(e) => return Err(self.io("cannot read", e)),
            Ok(meta) if !meta.is_file() => {
                return Err(invalid(format!(
                    "{} is not a regular file",
                    self.path.display()
                )));
            }
            Ok(_) => {}
        }
        let bytes = std::fs::read(&self.path).map_err(|e| self.io("cannot read", e))?;
        let state: State = serde_json::from_slice(&bytes)
            .map_err(|e| invalid(format!("{}: {e}", self.path.display())))?;
        state.validate()?;
        Ok(state)
    }

    fn dir(&self) -> Result<&Path> {
        let dir = self
            .path
            .parent()
            .ok_or_else(|| invalid("the journal has no directory"))?;
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)
            .map_err(|e| self.io("cannot create the directory of", e))?;
        Ok(dir)
    }

    fn write(&self, state: &State) -> Result<()> {
        state.validate()?;
        let dir = self.dir()?;
        let mut text = serde_json::to_vec_pretty(state).map_err(|e| invalid(e.to_string()))?;
        text.push(b'\n');
        let temporary = self.path.with_extension("json.new");
        let mut file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&temporary)
            .map_err(|e| self.io("cannot write", e))?;
        file.set_permissions(std::fs::Permissions::from_mode(0o600))
            .and_then(|()| file.write_all(&text))
            .and_then(|()| file.sync_all())
            .map_err(|e| self.io("cannot write", e))?;
        std::fs::rename(&temporary, &self.path).map_err(|e| self.io("cannot replace", e))?;
        File::open(dir)
            .and_then(|d| d.sync_all())
            .map_err(|e| self.io("cannot sync the directory of", e))
    }

    /// Changes the journal under its lock. Nothing is written when `change`
    /// fails.
    pub fn update<T>(&self, change: impl FnOnce(&mut State) -> Result<T>) -> Result<T> {
        self.dir()?;
        let lock_path = self.path.with_extension("json.lock");
        let lock = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(&lock_path)
            .map_err(|e| self.io("cannot open the lock of", e))?;
        lock.lock().map_err(|e| self.io("cannot lock", e))?;
        let mut state = self.read()?;
        let result = change(&mut state)?;
        self.write(&state)?;
        Ok(result)
    }
}

#[cfg(test)]
#[path = "journal_tests.rs"]
mod tests;
