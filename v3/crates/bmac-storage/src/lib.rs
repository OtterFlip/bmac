// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

//! The pool layout model, its checks, and the capacity function (`STORAGE.md`).
//!
//! Every program that designs, checks, or sizes storage uses this crate, so
//! the Deployer, `bmacd`, `bmac-cli`, and `bmac-backupd` apply the same rules.

pub mod capacity;
pub mod checks;
pub mod geometry;
pub mod layout;
pub mod luks;
pub mod partition;
pub mod summary;
pub mod types;

pub use capacity::{PoolSpace, ZvolAllocation};
pub use checks::{Disk, Finding, Findings, Target, Transport};
pub use geometry::{BlockSize, Geometry, PoolGeometry, Ratio};
pub use layout::{ById, Encryption, LayoutError, PoolLayout, Vdev, VdevClass, VdevType};
pub use summary::Summary;
