// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

//! The BMAC daemon: the root engine and the unprivileged HTTPS front end,
//! two processes from one binary (`SECURITY.md`, "Process separation").

pub mod audit;
pub mod auth;
pub mod backup;
pub mod certificate;
pub mod cluster;
pub mod db;
pub mod engine;
pub mod identity;
pub mod jobs;
pub mod joiner;
pub mod joins;
pub mod log;
pub mod mesh;
pub mod metrics;
pub mod native;
pub mod notify;
pub mod operations;
pub mod peers;
pub mod pinned;
pub mod records;
pub mod runner;
pub mod schedules;
pub mod settings;
pub mod status;
pub mod store;
pub mod supervisor;
pub mod systemd;
pub mod updates;
pub mod web;
pub mod wire;

/// This build's BMAC version: `BMAC_VERSION` when the build sets it, as
/// `dev/build_deb.py --version` does to tell successive lab builds apart.
pub const VERSION: &str = match option_env!("BMAC_VERSION") {
    Some(version) => version,
    None => env!("CARGO_PKG_VERSION"),
};
