<!--
Copyright (c) 2026 BEENTHERE VENTURES, INC.
SPDX-License-Identifier: GPL-3.0-only
-->

# Proxmox shared orchestration libraries

## `config.sh`

`config.sh` is a sourceable Bash library and a safe validation command. It
parses literal `KEY=VALUE` assignments without `source` or `eval`, in this
order:

1. `cluster.conf`
2. the selected `moxN.conf`, when requested

`bmacd` renders both into `/run/bmac/config` before each run from the
cluster settings (`PROXMOX_CONFIG_DIR`). Tests set `BMAC_CONFIG_TEST_MODE=1`
and `BMAC_CONFIG_DIR`. There is no secrets file: a script asks for a secret
with `prompt_secret`, which becomes a secret field in the run's form, and the
value stays in the shell.

It rejects unknown, misplaced, and duplicate keys; symlinks in any path
component; and any file whose mode is not exactly `0600` or that is not owned
by the running user or root.

```bash
scripts/lib/config.sh --help
scripts/lib/config.sh --check --host mox1

source scripts/lib/config.sh
load_proxmox_config --host mox1
```

Host-specific derived values are authoritative:

- `moxN`: internal FQDN `moxN.pve.internal` and, unless its `moxN.conf`
  sets `PROXMOX_SECONDARY_IP`, private host address `MOX_IP_START` plus
  `N-1` (`mox_private_ip moxN`)
- `haproxyN`: private address `HAPROXY_IP_START` plus `N-1`, with the
  private subnet's prefix (`haproxy_lxc_ip moxN`)
- local HAProxy gateway: the corresponding mox private address
- HAProxy VMID: `9110+N` (`9111` through `9142`, reserved whatever
  `MAX_MOX_HOSTS` is)
- VRRP priority: `200-N`

The library also exports validation and prompt helpers, `this_mox`,
`mox_is_local`, `mox_is_reachable`, and `mox_ssh`. `prompt_tailscale_auth_key`
reads a fresh key silently and keeps it in memory only.

Scripts run on a cluster host, which `bmacd` names in `BMAC_HOST`.
`mox_ssh moxN COMMAND...` runs the command directly when `moxN` is this host,
and otherwise runs it over `ssh -F /etc/bmac/ssh_config moxN` with every
argument quoted with `%q`. `bmacd` writes that file: each slot uses its private
VLAN address, `root`, `BatchMode`, and Proxmox's per-node
`/etc/pve/nodes/<moxN>/ssh_known_hosts` pin with `HostKeyAlias=moxN`.

## `cluster_registry.py`

The registry defaults to the root-only pmxcfs hierarchy
`/etc/pve/priv/bmac/registry`. `--state-dir` is available for tests. It stores only
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
usage. `render` prints that layout for an operator.
`scripts/user_callable/diagnostics/show_proxmox_host_state.sh` uses both.

## `cluster_control.sh`

Cluster membership as this host sees it, for the diagnostics.
`control_find_cluster` reads `pvesh get /nodes` on this host into the sorted
member and online-member arrays; it returns 1 when this host is not in a
cluster.

## `host_membership.sh`

Library shared by `scripts/user_callable/hosts/remove_proxmox_host.sh`,
`scripts/user_callable/qdevice/add_qdevice.sh`,
`scripts/user_callable/qdevice/remove_qdevice.sh`, and the diagnostics. The
coordinator, `HM_COORDINATOR`, is the host the job runs on:

- QDevice access check, add, remove, and vote-parity verification;
- `pvecm delnode` with a wait for the node to leave;
- removal of the node directory and of the host's cluster-wide SSH trust;
- the reinstall follow-up list.

## `qdevice.sh`

The QDevice vote, used by the scripts above. Hosts reach the QDevice as root
over Tailscale with the cluster's QDevice access key,
`/etc/pve/priv/bmac/qdevice_ed25519`, which pmxcfs gives every host, so any
host can run a QDevice job or a host removal. The key is made the first time
it is needed, and the operator adds its public half to the QDevice's root
`authorized_keys`: Add QDevice asks for it while the machine is prepared, and
a host removal that leaves an even count asks when the QDevice is not yet
accessible. Remove QDevice takes it off again when it purges the machine.
`pvecm qdevice setup` itself uses the coordinator's root RSA key, authorized
only while it runs, as in v2. The QDevice's host key is confirmed by the
fingerprint read on its console and pinned on every member.

## `guest_agent.sh`

Guest access through the QEMU guest agent: `qm guest exec` on the host that
runs the guest, with an optional stdin of at most 1 MiB. Scripts never log in
to a guest over SSH.

## `prod_ha.sh`

Library shared by `scripts/user_callable/guests/prod/change_prod_vm_placement.sh` and
`scripts/user_callable/guests/prod/change_prod_vm_owner.sh`:

- production VM selection;
- loading and validating live HA, rule, and replication state;
- refreshing a stale registry owner;
- the staging-dependents refusal;
- the VM's root disk storage, from its registry record;
- the per-resource orchestration lease;
- registry updates with revision checks;
- replication health checks and waits;
- HA node-affinity rule updates.

## `ui_protocol.sh`

Every operator script supports `--json`, which makes it speak the bmac-ui v2
NDJSON protocol ([`protocol/bmac-ui-v2.schema.json`](../../protocol/bmac-ui-v2.schema.json))
instead of drawing a terminal UI. `bmacd` always runs scripts this way. Without `--json` nothing changes: every helper below is silent and
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
  When the operator must run a script by hand, pass `--run-script HOST PATH`
  after its instruction: the dashboard highlights that step, offers to copy
  the path, and says the workflow waits until they ran it on HOST.
A declined confirmation followed by a non-zero exit reports the run as
cancelled rather than failed; answering a later request clears that.
Cancelling from the controller exits with status 3. `config.sh`, `prod_ha.sh`,
`host_membership.sh`, and `qdevice.sh` route their prompt helpers through this library, so
scripts that use them get JSON mode for those prompts automatically.

`ui_json_run.sh` is the runner `bmac_ui_bootstrap` starts. It writes the
`protocol`, `workflow_started`, and final `completed` events, wraps every plain
output line as a `log` event so printed JSON cannot pose as protocol, and maps
the exit status to success, cancelled, or failed. `ui_protocol.py` is its
Python half, and also lets Python helpers emit events (`emit_event`,
`emit_next_step`). `legacy/tests/ui_test_driver.py` drives a script's JSON mode in tests.

## Tests

From `v3/`, every library test, in parallel:

```bash
dev/run_tests.py legacy/tests/test_{haproxy_routes,host_storage,process_deferred_cleanup}.py \
  legacy/tests/test_{shared_libs,ui_json_forms,ui_protocol}.py
```
