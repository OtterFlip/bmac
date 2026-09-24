<!--
Copyright (c) 2026 BEENTHERE VENTURES, INC.
SPDX-License-Identifier: GPL-3.0-only
-->

# Proxmox shared orchestration libraries

## `config.sh`

`config.sh` is a sourceable Bash library and a safe validation command. It
parses literal `KEY=VALUE` assignments without `source` or `eval`, in this
order:

1. `env/cluster.conf`
2. the selected `env/moxN.conf`, when requested
3. optional Git-ignored `env/secrets.env`

It rejects unknown, misplaced, and duplicate keys; symlinks in any path
component; world-writable non-secret files; and any `secrets.env` mode other
than `0600`. Tracked `cluster.conf` / `moxN.conf` may be group-writable
(`664`) because Git cannot store that mode and Ubuntu umask `002` checkouts
are common. Secret values are never included in validation output or the
effective-configuration hash.

```bash
lib/config.sh --help
lib/config.sh --check --host mox1 --require-secrets

source lib/config.sh
load_proxmox_config --host mox1 --require-secrets
```

Host-specific derived values are authoritative:

- `moxN`: internal FQDN `moxN.pve.internal` and private host address `.10+N`
  (`.11` through `.20`)
- `haproxyN`: private address `.20+N` (`.21` through `.30`)
- local HAProxy gateway: the corresponding mox private address
- HAProxy VMID: `9110+N` (`9111` through `9120`)
- VRRP priority: `200-N`

The library also exports validation/prompt helpers, `mox_is_reachable`,
`reachable_mox_hosts`, `first_reachable_mox`, `mox_ssh`, `ssh_via_mox`,
`guest_ssh_via_mox`, and `scp_via_mox`. `prompt_tailscale_auth_key` reads a
fresh key silently and keeps it in memory only.

On an administrator workstation, mox SSH helpers use the operator's explicit
known-hosts file and Tailscale aliases. On a Proxmox node they resolve the
internal FQDN over the private VLAN and use Proxmox 9's native per-node
`/etc/pve/nodes/<moxN>/ssh_known_hosts` pin with `HostKeyAlias=moxN`.

## `cluster_registry.py`

The registry defaults to the root-only pmxcfs hierarchy
`/etc/pve/priv/app-ha`. `--state-dir` is available for tests. It stores only
typed policy, resource, route, placement, and cleanup metadata. VMIDs are
supplied by the caller after checking the Proxmox API; the registry reserves
them and rejects duplicates and the HAProxy range.

```bash
sudo lib/cluster_registry.py init
sudo lib/cluster_registry.py list
sudo lib/cluster_registry.py allocate-prod --help
sudo lib/cluster_registry.py allocate-staging --help
sudo lib/cluster_registry.py update --help
sudo lib/cluster_registry.py list-routes
```

All mutations use an atomic `mkdir` lock. A stale lock is never removed merely
because it is old: an operator must inspect it and opt in with
`--break-stale-lock`. Allocation IDs make retried creation requests
idempotent.

`reconcile --live` safely selects node, QEMU config, and attached-volume facts
from the local Proxmox API. For complete reconciliation (HA affinity,
replication health, ZFS snapshot GUIDs, and rendered routes), `--observed`
accepts a typed, secret-free observation assembled by the orchestrator:

```json
{
  "schema_version": 1,
  "nodes": [{"name": "mox1", "online": true}],
  "vms": [],
  "ha_resources": [],
  "replication_jobs": [],
  "volumes": [],
  "snapshots": [],
  "routes": [],
  "cleanup_completed": []
}
```

Sections may be omitted when they were not observed. Reconciliation is
read-only unless `--apply` is passed. Apply mode only marks expired,
VM-less reservations failed and marks explicitly reported cleanup IDs
completed; it does not destroy Proxmox resources.

## `haproxy_routes.py` and `sync_haproxy_routes.sh`

`haproxy_routes.py` converts the registry's exact enabled Host/SNI routes into
a deterministic, reject-by-default HAProxy generation. The synchronizer runs
only through an online, quorate coordinator; stages and validates every
currently configured proxy; persists the desired generation and per-node
application state in pmxcfs; disables stale ingress on failure; and
transactionally reloads or rolls back. Boot and periodic services reconcile a
returning node before its public DNAT path is enabled. Generation retention is
bounded.

## `process_deferred_cleanup.sh`

This root-only, timer-driven worker processes schema-validated cleanup records
for its local node. It revalidates QEMU, HA, ZFS, snapshot GUID, route, and
registry identity before every destructive action. Offline or ambiguous work
remains pending; it never invents completion evidence.

## `host_storage.py`

`collect` runs on a Proxmox host, fed over SSH as `python3 - collect`. It runs
only read-only `zpool`, `zfs`, and `lsblk` queries and prints one JSON layout
of `rpool`: capacity and ZFS available space, every top-level vdev with its
members resolved through LUKS to physical disk serials, the vdev that holds
the proxmox-boot-tool ESPs, device-removal progress, disks outside `rpool`,
and per-zvol allocation and snapshot usage. `render` prints that layout on
the workstation. `diagnostics/show_proxmox_host_state.sh` uses both.

## Tests

```bash
PYTHONDONTWRITEBYTECODE=1 python3 -m unittest \
  deploy.proxmox.lib.test_shared_libs \
  deploy.proxmox.lib.test_haproxy_routes \
  deploy.proxmox.lib.test_process_deferred_cleanup
```
