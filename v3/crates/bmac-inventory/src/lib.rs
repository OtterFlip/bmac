// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

//! Hardware facts and link-wait (`IMPLEMENTATION_PLAN.md` M6.2). Run on the
//! discovery ISO as a static binary; the link-wait rules are also used by
//! the host setup job and the NIC collector.
//!
//! Everything here reads, except [`clear::clear_zfs_labels`].

pub mod clear;
pub mod disks;
pub mod fixture;
pub mod luks;
pub mod nics;
pub mod root;
pub mod system;
pub mod udev;
pub mod zfs;

use std::time::Duration;

use bmac_proto::inventory::{INVENTORY_VERSION, Inventory, LinkWait};

pub use root::Root;

/// Takes the inventory. With `link_wait`, every NIC is brought up through
/// `control` and given that long to get carrier; without, links are read
/// once as they are.
pub fn take(
    root: &Root,
    link_wait: Option<LinkWait>,
    control: &dyn nics::LinkControl,
    sleep: &mut dyn FnMut(Duration),
    now_ms: i64,
) -> Inventory {
    let once = LinkWait {
        attempts: 1,
        interval_seconds: 0,
    };
    let nics = match link_wait {
        Some(wait) => nics::read_nics(root, &wait, control, sleep),
        None => nics::read_nics(root, &once, &nics::LeaveAsIs, sleep),
    };
    Inventory {
        version: INVENTORY_VERSION,
        taken_ms: now_ms,
        system: system::read_system(root),
        cpu: system::read_cpu(root),
        memory_bytes: system::read_memory(root),
        firmware: system::read_firmware(root),
        disks: disks::read_disks(root),
        nics,
        link_wait,
    }
}
