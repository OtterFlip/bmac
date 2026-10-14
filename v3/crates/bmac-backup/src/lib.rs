// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

//! The backup model shared by `bmacd`, `bmac-source`, and `bmac-backupd`
//! (`BACKUP.md`): PIT snapshot names, manifests, the cluster backup
//! settings, appliance states as the cluster sees them, the lifetime of a
//! PIT's snapshots and bookmarks on the cluster, and retention. Everything
//! here is pure: callers bring the facts and the clock.

pub mod lifetime;
pub mod manifest;
pub mod names;
pub mod records;
pub mod restore;
pub mod retention;
pub mod settings;
pub mod states;

pub use names::PitName;
