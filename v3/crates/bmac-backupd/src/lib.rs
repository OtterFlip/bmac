// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

//! The backup appliance (`BACKUP.md`): it checks in with the clusters it is
//! enrolled with, pulls the PITs they offer it into its own pool, and
//! publishes them. The clusters have no credential for it and never write
//! to it.

pub mod admin;
pub mod audit;
pub mod checkin;
pub mod client;
pub mod daemon;
pub mod enroll;
pub mod export;
#[cfg(test)]
mod fake_pool;
pub mod fsx;
pub mod grants;
pub mod journal;
pub mod luks;
pub mod peers;
pub mod platform;
pub mod pool;
pub mod pull;
pub mod recovery;
pub mod repair;
pub mod retention;
pub mod roots;
pub mod scrub;
pub mod setup;
pub mod state;
pub mod terminal;
pub mod tunnels;
pub mod ui;
pub mod urlcheck;
pub mod watchdog;
