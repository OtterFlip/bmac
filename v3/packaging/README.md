<!--
Copyright (c) 2026 BEENTHERE VENTURES, INC.
SPDX-License-Identifier: GPL-3.0-only
-->

# Packaging

The `.deb` builds for `bmacd` and `bmac-backupd`, their systemd units, and
release signing (`design/IMPLEMENTATION_PLAN.md`, M10). The Deployer's
package is built by Tauri from `deployer/`.

- `systemd/bmacd.service`: the engine, as root. It owns
  `/run/bmac/engine.sock` (mode 0660, group `bmac-web`) and pulls in the
  front end.
- `systemd/bmacd-web.service`: the HTTPS front end, as `bmac-web`, bound to
  the engine and sandboxed. It gets the HTTPS key and certificate through
  `LoadCredential` and cannot see `/etc/pve`, `/etc/bmac`, or
  `/var/lib/bmac`.
- `systemd/bmac-source.service`: the backup source, as `bmac-source`, bound
  to the engine like the front end. It listens on nothing: it dials each
  approved appliance's address on port 8445 and serves the appliance's
  requests over that connection. It gets the
  node's Proxmox certificate, key, and cluster CA through `LoadCredential`,
  talks to the engine on `/run/bmac-source/engine.sock`, and reaches only
  `/dev/zfs`, for the sends ZFS delegation allows it.
- `sysusers.d/bmacd.conf`: creates the `bmac-web` and `bmac-source` users.

`crates/bmacd/tests/units.rs` keeps these files in step with the code.

## The development package

`dev/build_deb.py` builds the web UI and the release `bmacd` and `bmac`,
then `bmacd_<version>_amd64.deb` into `target/deployer-bundle/packages/`,
where the Deployer's host ISO picks it up. `--no-build` packages what is
already built; `--out DIR` writes elsewhere. It honours `SOURCE_DATE_EPOCH`
and refuses binaries that need a newer glibc than Proxmox VE's Debian has.

The host's first boot installs the package with `dpkg -i` before the
network is up, so it `Depends` only on what Proxmox VE already has
(`systemd`, `pve-manager`). The programs the legacy scripts call, listed in
`bmacd.depends`, are `Recommends`; host setup installs them with apt once
the host is online. The postinst creates the `bmac-web` and `bmac-source` users, puts the
guest-role hook in `/var/lib/vz/snippets`, and enables `bmacd.service` and
the deferred-cleanup timer. Release signing comes with M10.
