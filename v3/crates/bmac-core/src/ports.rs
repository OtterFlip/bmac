// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

//! The TCP ports BMAC listens on, and the ports the host guard drops on the
//! public address.

/// The Dashboard: `bmacd`'s HTTPS front end.
pub const DASHBOARD: u16 = 8443;
/// Host-to-host streams, joins, and appliance enrollment.
pub const PEER: u16 = 8444;
/// `bmac-source`, which streams backups to appliances.
pub const BACKUP_SOURCE: u16 = 8445;

/// Dropped on the public address by the host guard: v2's SSH, Proxmox's
/// SPICE proxy, and its GUI (`add_proxmox_host.sh`), then BMAC's own three.
pub const PUBLIC_GUARD_TCP: [u16; 6] = [22, 3128, 8006, DASHBOARD, PEER, BACKUP_SOURCE];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_guard_covers_every_bmac_listener() {
        for port in [DASHBOARD, PEER, BACKUP_SOURCE, 22, 3128, 8006] {
            assert!(PUBLIC_GUARD_TCP.contains(&port), "{port}");
        }
    }
}
