<!--
Copyright (c) 2026 BEENTHERE VENTURES, INC.
SPDX-License-Identifier: GPL-3.0-only
-->

# Proxmox shared orchestration libraries

## `config.sh`

`config.sh` is a sourceable Bash library and a safe validation command. It
parses literal `KEY=VALUE` assignments without `source` or `eval`, in this
order:

1. `config/cluster.conf`
2. the selected `config/moxN.conf`, when requested
3. optional Git-ignored `config/secrets.env`

In a checkout the config directory is `<checkout>/config`. When the scripts
run from the installed Dashboard package (a `bmac-installed` marker next to
`scripts/`), it is `~/.config/com.btvcorp.bmac.dashboard/config`, or the
absolute path in the pointer file `config-location` beside it when that file
is a regular file owned by the user and not group- or world-writable.
`PROXMOX_CONFIG_DIR` holds the result and `PROXMOX_ARTIFACTS_DIR` is
`$PROXMOX_CONFIG_DIR/artifacts`. `config_legacy_artifacts_problem` reports a
non-empty pre-move `scripts/user_callable/.../artifacts` directory with the
command that moves it, and `config_require_git_ignored` enforces Git-ignore
only where Git applies.

It rejects unknown, misplaced, and duplicate keys; symlinks in any path
component; world-writable non-secret files; and any `secrets.env` mode other
than `0600`. Tracked `cluster.conf` / `moxN.conf` may be group-writable
(`664`) because Git cannot store that mode and Ubuntu umask `002` checkouts
are common. Secret values are never included in validation output or the
effective-configuration hash.

```bash
scripts/lib/config.sh --help
scripts/lib/config.sh --check --host mox1 --require-secrets

source scripts/lib/config.sh
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
sudo scripts/host_runtime/cluster_registry.py init
sudo scripts/host_runtime/cluster_registry.py list
sudo scripts/host_runtime/cluster_registry.py allocate-prod --help
sudo scripts/host_runtime/cluster_registry.py allocate-staging --help
sudo scripts/host_runtime/cluster_registry.py update --help
sudo scripts/host_runtime/cluster_registry.py list-routes
```

`update PRODN --disk-bytes N` records the exact root disk size after
`scripts/user_callable/guests/prod/extend_prod_vm_disk.sh` grows a production zvol. It accepts only
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
transactionally reloads or rolls back. A node keeps public ingress through a
graceful reload only when `haproxy_routes.py compare` proves its live
generation cannot route any request differently from the candidate; pmxcfs
records that as a bounded per-node allowance. Every other stale node closes
before the desired pointer moves. Boot and periodic services reconcile a
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
the workstation. `scripts/user_callable/diagnostics/show_proxmox_host_state.sh` uses both.

## `rpool_mirror.sh`

The root-only host tool behind every rpool mirror change, installed per run
as `/usr/local/sbin/app-ha-rpool-mirror` by `scripts/user_callable/hosts/add_proxmox_host.sh` and
the disk workflows. Subcommands select disks by serial and re-prove identity
before acting: `check-new`, `luks-prepare` (key file over SSH during setup,
hidden prompt at the console afterwards; proven against every existing LUKS member),
`luks-check-prepared`, `luks-backup-headers`, `luks-add` (crypttab in the
host's existing form, initramfs rebuilt and unpacked to prove boot unlock,
then `zpool add` with the pool's one ashift), `clear-add`, and `retire-luks`.
Extra mirror members live on partition 1, ending 1 GiB short of the smaller
disk's whole-GiB size. `replacement-bytes` reports what a replacement needs to
hold a survivor's partitions, and `release-disks` erases only the metadata of
retired disks. One member at a time (A or B on a boot disk's partition 3, N-M
on partition 1 of an extra mirror's disk): `luks-prepare-member`, `luks-check-member`,
`luks-backup-headers --member`, and `luks-register`, which setup's boot-mirror
LUKS conversion and `scripts/user_callable/hosts/add_replacement_disk.sh` share; plus
`copy-partitions` (copy the survivor's partition table onto a disk that can
hold it), `boot-esp` (format,
register, and sync the new ESP), and `replace-member` (`zpool replace` or
`zpool attach` without waiting for the resilver).

## `simulated_disk_failure.sh`

Host side of the `scripts/user_callable/hosts/simulate_disk_failure_and_replacement.sh` test aid,
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
and do not read or update `config/moxN.conf`.

## `cluster_control.sh`

Workstation library for the control node. `resolve_control_node
[--allow-offline]` reads the registry's control node through a reachable
member. It stops when `PROXMOX_CONTROL_NODE` in `config/cluster.conf`
disagrees, and prompts when the setting is missing. Before a cluster exists,
the configured or entered node is the one that creates it.
`control_offer_cluster_conf_update moxN` offers to rewrite the
`PROXMOX_CONTROL_NODE` line after the control node changes. Every other
workstation must then make the same change.

## `host_membership.sh`

Workstation library shared by `scripts/user_callable/hosts/remove_proxmox_host.sh`,
`scripts/user_callable/qdevice/add_qdevice.sh`, `scripts/user_callable/qdevice/remove_qdevice.sh`, and the diagnostics:

- QDevice access check, add, remove, and vote-parity verification;
- the control-plane lock that host setup also takes;
- `pvecm delnode` with a wait for the node to leave;
- removal of the node directory and of the host's cluster-wide SSH trust;
- archiving of `config/artifacts/hosts/moxN`;
- choice of a new control node;
- the reinstall follow-up list.

## `prod_ha.sh`

Workstation library shared by `scripts/user_callable/guests/prod/change_prod_vm_placement.sh` and
`scripts/user_callable/guests/prod/change_prod_vm_owner.sh`:

- production VM selection;
- loading and validating live HA, rule, and replication state;
- refreshing a stale registry owner;
- the staging-dependents refusal;
- the per-resource orchestration lease;
- registry updates with revision checks;
- replication health checks and waits;
- HA node-affinity rule updates.

## `ui_protocol.sh`

Every operator script supports `--json`, which makes it speak the bmac-ui v2
NDJSON protocol ([`dashboard/protocol/bmac-ui-v2.schema.json`](../../dashboard/protocol/bmac-ui-v2.schema.json))
instead of drawing a terminal UI. The BMAC dashboard uses it; anything else
may too. Without `--json` nothing changes: every helper below is silent and
scripts keep their own terminal prompts.

```bash
source "${REPO_ROOT}/lib/ui_protocol.sh"
...
bmac_ui_bootstrap "$@"   # just before main; re-executes under ui_json_run.sh with --json
main "$@"
```

Converting a prompt:

```bash
if bmac_ui_is_json; then
  bmac_ui_choose HOST "Host to inspect" mox1 mox1 "mox1" mox2 "mox2"
else
  read -r -p "Host to inspect [mox1]: " HOST
fi
```

Events: `bmac_ui_step`/`bmac_ui_step_done` (phases), `bmac_ui_progress`,
`bmac_ui_info`, `bmac_ui_warning`, `bmac_ui_error`, `bmac_ui_plan_begin` /
`bmac_ui_plan_item` / `bmac_ui_plan_end`, `bmac_ui_result KEY VALUE...` (or
`bmac_ui_result_json`), and `bmac_ui_next_step TEXT [--command C] [--workflow ID]
[--arg K=V]...`.

Requests, which block until the controller answers:

- `bmac_ui_input VAR FIELD-OPTIONS...` for one field, and `bmac_ui_text`,
  `bmac_ui_choose`, `bmac_ui_number_choice`, and `bmac_ui_yes_no` shorthands.
  `bmac_ui_reask VAR MESSAGE` rejects the last answer and waits again.
- `bmac_ui_group_begin` / `bmac_ui_group_add` / `bmac_ui_group_request` for a
  form, validated with `bmac_ui_field_error` and `bmac_ui_group_check`.
- `bmac_ui_confirm` (with `--severity` and `--text PHRASE` for typed
  confirmations), `bmac_ui_ask` for a yes/no question, and
  `bmac_ui_confirm_go`, the JSON form of "Type GO to continue".
- `bmac_ui_manual_action` for work done outside the controller, such as a
  LUKS passphrase typed at a host console. Secrets never travel this way.
A declined confirmation followed by a non-zero exit reports the run as
cancelled rather than failed; answering a later request clears that.
Cancelling from the controller exits with status 3. `config.sh`, `prod_ha.sh`,
`host_membership.sh`, `disk_workflows.sh`, `qdevice.sh`, and
`cluster_control.sh` route their prompt helpers through this library, so
scripts that use them get JSON mode for those prompts automatically.

`ui_json_run.sh` is the runner `bmac_ui_bootstrap` starts. It writes the
`protocol`, `workflow_started`, and final `completed` events, wraps every plain
output line as a `log` event so printed JSON cannot pose as protocol, and maps
the exit status to success, cancelled, or failed. `ui_protocol.py` is its
Python half, and also lets Python helpers emit events (`emit_event`,
`emit_next_step`). `ui_test_driver.py` drives a script's JSON mode in tests.

## `quick_state.sh` and `quick_state.py`

The engine behind `scripts/user_callable/diagnostics/list_hosts.sh`, `list_guests.sh`,
`list_replication.sh`, and `list_storage.sh`: fast, read-only state for one
area, read through one reachable member over a single SSH connection. The
collector (`quick_state.py collect KIND`) runs on that host and uses only
`pvesh get`, `pvecm status`, and registry reads. The renderer runs on the
workstation, prints a terminal report, and in JSON mode emits the same state
as one `result` event plus next steps.

## `disk_inventory.py`

The workstation half of `scripts/user_callable/diagnostics/list_disks.sh`. That script runs
`host_storage.py collect` and `storage_state.py show` on every online host in
parallel. `disk_inventory.py render` then turns the results into every pool
vdev (state, LUKS, member serials and sizes) and every disk outside a pool,
with a status: available, awaiting finalization, evacuating, part of an
interrupted replacement or new mirror, or in use. `disk_inventory.py
pending-removals` is the classification of requested vdev removals that
`scripts/user_callable/hosts/inventory_disks.sh` acts on, so both scripts agree on which disks
await finalization.

## Tests

From the repository root, every library test, in parallel:

```bash
dev/run_tests.py scripts/tests/test_{disk_inventory,haproxy_routes,host_storage}.py \
  scripts/tests/test_{process_deferred_cleanup,quick_state,report_stale_jump_ssh}.py \
  scripts/tests/test_{rpool_mirror,shared_libs,storage_state,ui_json_forms,ui_protocol}.py
```

The JSON-mode library, forms, and quick-state tests alone:

```bash
dev/run_tests.py scripts/tests/test_ui_protocol.py scripts/tests/test_ui_json_forms.py scripts/tests/test_quick_state.py \
  scripts/tests/test_disk_inventory.py
```

With no arguments `dev/run_tests.py` runs the whole suite. See
[`MAIN_DESIGN.md`](../../docs/MAIN_DESIGN.md#tests).
