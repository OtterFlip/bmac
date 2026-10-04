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

`update PRODN --disk-bytes N` records the exact root disk size after
`guests/prod/extend_prod_vm_disk.sh` grows a production zvol. It accepts only
whole-MiB values larger than the current size, and stores them in the optional
`spec.disk_bytes`. `spec.disk_gib` keeps the creation-time size.

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

The registry also records host slots and the control node:

- `host-list`, `host-next [--exclude moxN]`: list slots, and print the lowest
  free slot that setup recommends.
- `host-sync [--member LIST | --live]`: record the live membership. It
  creates or promotes member slots and reports slots whose host is no longer
  a member as stale.
- `host-reserve moxN`: mark a slot `joining` before a host joins.
- `host-release moxN`: free a slot. It is refused while any resource
  references the host or the host is the control node.
- `host-references moxN`: list the resources that reference the host, and
  the control node.
- `control-get`, `control-set moxN --expected-node moxN|none`: read and
  compare-and-set the control node. New hosts join through it, and it holds
  the control-plane lock.

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
the proxmox-boot-tool ESPs, device-removal progress, disks outside `rpool`
(and those holding a registered ESP, a boot-disk replacement not yet in the
pool), vdevs resilvering a replacement, and per-zvol allocation and snapshot
usage. `render` prints that layout on
the workstation. `diagnostics/show_proxmox_host_state.sh` uses both.

## `rpool_mirror.sh`

The root-only host tool behind every rpool mirror change, installed per run
as `/usr/local/sbin/app-ha-rpool-mirror` by `hosts/setup_proxmox_host.sh` and
the disk workflows. Subcommands select disks by serial and re-prove identity
before acting: `check-new`, `luks-prepare` (console; key file during setup,
hidden prompt afterwards; proven against every existing LUKS member),
`luks-check-prepared`, `luks-backup-headers`, `luks-add` (crypttab in the
host's existing form, initramfs rebuilt and unpacked to prove boot unlock,
then `zpool add` with the pool's one ashift), `clear-add`, and `retire-luks`.
Extra mirror members live on partition 1, ending 1 GiB short of the smaller
disk's whole-GiB size. `replacement-bytes` reports what a replacement needs to
hold a survivor's partitions, and `release-disks` erases only the metadata of
retired disks. One member at a time (A or B on a boot disk's partition 3, N-M
on partition 1 of an extra mirror's disk): `luks-prepare-member`, `luks-check-member`,
`luks-backup-headers --member`, and `luks-register`, which setup's boot-mirror
LUKS conversion and `hosts/add_replacement_disk.sh` share; plus
`copy-partitions` (copy the survivor's partition table onto a disk that can
hold it), `boot-esp` (format,
register, and sync the new ESP), and `replace-member` (`zpool replace` or
`zpool attach` without waiting for the resilver).

## `simulated_disk_failure.sh`

Host side of the `hosts/simulate_disk_failure_and_replacement.sh` test aid,
piped to a host as `bash -s -- COMMAND MARKER`. `offline` proves that the pool
is healthy and the chosen disk holds exactly one member of a two-way mirror
whose other member is `ONLINE`. It records the simulation in MARKER, then
offlines the member and closes its LUKS mapping. `restore` brings the member
back, or exits 3 when its mapping has to be reopened at the console. `wipe`
re-proves that the survivor is `ONLINE` and that nothing else uses the disk.
It then erases the disk: `cryptsetup erase`, `wipefs`, `sgdisk --zap-all`,
zeroing of the ZFS label areas, and `blkdiscard`. `status` reports a pending
simulation.

## `storage_state.py`

Piped to a host as `python3 - COMMAND`. Keeps
`/var/lib/app-ha-storage/state.json` (root-only, locked, atomically replaced):
per-guest trim times, scrub results, one record per vdev removal with its
member serials, because ZFS forgets them once a removal completes, each new
mirror pair until its operation completes, and the disk chosen for a pending
mirror-member replacement, so a rerun can tell it from a failed disk.

## `disk_workflows.sh`

`disk_workflows.sh` is the workstation library sourced by the disk
workflows: target-host prompting, strict SSH, shared live disk inventory,
per-run tool installation with a hash check, host-side Python, and long-step
prompts. Post-setup workflows use the host as their storage source of truth
and do not read or update `env/moxN.conf`.

## `cluster_control.sh`

Workstation library for the control node. `resolve_control_node
[--allow-offline]` reads the registry's control node through a reachable
member. It stops when `PROXMOX_CONTROL_NODE` in `env/cluster.conf`
disagrees, and prompts when the setting is missing. Before a cluster exists,
the configured or entered node is the one that creates it.
`control_offer_cluster_conf_update moxN` offers to rewrite the
`PROXMOX_CONTROL_NODE` line after the control node changes. Every other
workstation must then make the same change.

## `host_membership.sh`

Workstation library shared by `hosts/remove_host_from_cluster.sh` and
`hosts/purge_host_from_cluster.sh`:

- QDevice access check, add, remove, and vote-parity verification;
- the control-plane lock that host setup also takes;
- `pvecm delnode` with a wait for the node to leave;
- removal of the node directory and of the host's cluster-wide SSH trust;
- archiving of `hosts/artifacts/moxN`;
- choice of a new control node;
- the reinstall follow-up list.

## `prod_ha.sh`

Workstation library shared by `guests/prod/change_prod_vm_placement.sh` and
`guests/prod/change_prod_vm_owner.sh`:

- production VM selection;
- loading and validating live HA, rule, and replication state;
- refreshing a stale registry owner;
- the staging-dependents refusal;
- the per-resource orchestration lease;
- registry updates with revision checks;
- replication health checks and waits;
- HA node-affinity rule updates.

## Tests

```bash
PYTHONDONTWRITEBYTECODE=1 python3 -m unittest \
  deploy.proxmox.lib.test_shared_libs \
  deploy.proxmox.lib.test_haproxy_routes \
  deploy.proxmox.lib.test_process_deferred_cleanup
```
