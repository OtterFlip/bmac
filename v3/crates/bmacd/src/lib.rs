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
pub mod web;
pub mod wire;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
