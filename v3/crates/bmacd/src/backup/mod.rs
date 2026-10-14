// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

//! The cluster side of backups (`BACKUP.md`): backup policies and their
//! schedules, the capture job, and the upkeep that keeps each PIT's
//! snapshots and bookmarks only as long as something needs them.

pub mod capture;
pub mod cluster;
pub mod daemon;
pub mod enrollment;
pub mod exports;
pub mod policies;
pub mod restore;
pub mod source;
pub mod source_engine;
pub mod source_socket;
pub mod status;
pub mod store;
pub mod upkeep;

#[cfg(test)]
mod tests;
