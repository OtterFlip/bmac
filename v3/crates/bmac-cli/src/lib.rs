// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

//! The `bmac` command for hosts. Read-only diagnostics read pmxcfs and
//! Proxmox directly, so they work with `bmacd` stopped; everything that
//! changes something goes through `bmacd`'s engine socket with the same
//! login and checks as the web UI. It is not a second implementation.

pub mod args;
pub mod capacity;
pub mod commands;
pub mod follow;
pub mod guests;
pub mod luks;
pub mod session;
pub mod terminal;
