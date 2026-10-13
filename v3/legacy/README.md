<!--
Copyright (c) 2026 BEENTHERE VENTURES, INC.
SPDX-License-Identifier: GPL-3.0-only
-->

# Adapted v2 scripts

Copies of the v2 scripts that run as `bmacd` script operations, adapted to
run on a host (`design/WORKFLOW_MIGRATION.md`, "Adapting the scripts"), in
`scripts/`, with their tests in `tests/`. Each one is deleted when its native
port passes the port gate. Tests of v2 behavior that no longer exists are
listed in `tests/NOT_APPLICABLE.md` with the reason.

v3 is not compatible with v2, so v2's `app-ha` names are renamed to `bmac`
everywhere: paths, units, locks, nftables tables, HAProxy maps, and
environment variables (`APP_HA_*` is `BMAC_*`). v2's install directory
`/usr/local/lib/app-ha-proxmox` is `/usr/lib/bmac/legacy`, its state
directory `/var/lib/app-ha-proxmox` is `/var/lib/bmac`, and the lifecycle
hook is `bmac-guest-role-hook.sh`. The registry lock prefix,
`bmac-registry-`, is shared with `bmac-pmxcfs` (`REGISTRY_LOCK_PREFIX`).

v2 scripts that are not copied here, and what replaces them:

| v2 | Becomes |
| --- | --- |
| `list_*.sh`, `quick_state.*`, `check_qdevice_access.sh` | Collectors (M5) |
| Storage workflows and their libraries (`disk_workflows.sh`, `rpool_mirror.sh`, `storage_state.py`, `disk_inventory.py`, `simulated_disk_failure.sh`) | Native storage (M8) |
| `add_proxmox_host.sh` | ISO building in the Deployer (M6), post-install phases in the host setup job (M7): most are native in `bmacd`; guest egress, the runtime, and HAProxy ingress are `scripts/host_setup/orchestrate_host.sh`, and final verification is `scripts/host_setup/verify_host.sh` |
| `cluster_setup_prereq.sh` | The Deployer (M6) |
| `update_cluster_runtime.sh` | Package installation and the cluster update job (M10) |
| `setup_jump_ssh_access.sh`, `report_stale_jump_ssh.sh` | The Deployer's Workstation SSH (M6.9) |
| `deploy_hello_app_to_prod.sh` | A Deployer feature (M6.9) |

## On a host

The `bmacd` package installs this directory's `scripts/` and `protocol/` as
`/usr/lib/bmac/legacy/`, where `bmacd` runs each script operation as
`bash SCRIPT --json`. It depends on the packages the scripts' commands come
from (`../packaging/bmacd.depends`), and also installs:

- the lifecycle hook as `/var/lib/vz/snippets/bmac-guest-role-hook.sh`
  (`local:snippets/bmac-guest-role-hook.sh`);
- `bmac-deferred-cleanup.timer`, enabled, and
  `bmac-haproxy-route-sync.timer`, which host setup enables once the host's
  HAProxy ingress exists, with the drop-in that makes
  `bmac-haproxy-ingress.service` start only on current routes
  (`../packaging/systemd/`).

The scripts, the hook, and the timers read the cluster settings from
`/run/bmac/config/` and SSH to other hosts through `/etc/bmac/ssh_config`.
The engine renders both before it reports ready, and so before Proxmox starts
any guest, then again within seconds of a settings change on any host and
before every script run. The timer services render them first too, and skip
a host that has no cluster settings yet. Until the packages exist (M10), the
lab's `lab/lab bootstrap` installs this same layout.
