<!--
Copyright (c) 2026 BEENTHERE VENTURES, INC.
SPDX-License-Identifier: GPL-3.0-only
-->

# BMAC v3 workflow engine and migration

How v2's Bash/Python workflows run under `bmacd`, and how each is ported to
native Rust.

## Approach

v2's scripts are tested (518 Python test functions) and proven on real
hardware. v3 keeps them running while the platform is built, and replaces
them one workflow at a time:

- `bmacd` has **two kinds of operation** behind one operation registry:
  - **Script operations** run an adapted v2 script under a supervisor that
    reads the v2 `bmac-ui` protocol (NDJSON on stdout, answers on stdin). The
    supervisor starts from copies of v2's `dashboard/engine/src/supervisor.rs`
    and `protocol.rs`.
  - **Native operations** are Rust code calling typed adapters.
- Both produce the same job events, use the same locks and roles, and look
  the same in the Dashboard.
- A workflow switches from script to native in a release, when its port
  passes the gate below. The switch is a change to the operation registry,
  not a runtime toggle. The two are never mixed within one job.
- The goal is that every workflow is native. A script operation is a
  temporary state.

## Native from the start

These are new, or read-only, or must be redesigned for v3 anyway:

| Area | Reason |
| --- | --- |
| Job engine, scheduler, web, login, sessions | New |
| Health collectors (replacing the diagnostics and `list_*` scripts) | Read-only, the safest to port, and polled constantly |
| Monitoring, alerts, notifications, log store and copying | New |
| Backup (cluster side and appliance) | New |
| Storage workflows and the storage phase of host setup | Mirror-only logic must be redesigned for any vdev type (`STORAGE.md`) |
| ISO building (in the Deployer) | Moves from `add_proxmox_host.sh` into the Deployer |
| Workstation SSH setup for a new host (in the Deployer) | Moves from `add_proxmox_host.sh` (`write_workstation_ssh_config` and its tests) into the Deployer's Rust side, keeping its lock, backup, `ssh -G` check, connection proof, and restore on failure |
| Hardware discovery (in the Deployer) | New; `bmac-inventory` replaces `cluster_setup_prereq.sh` and the inventory parts of `disk_inventory.py` |
| Bootstrap, join, enrollment | New |
| Cluster update | Replaces `update_cluster_runtime.sh` |
| Jump SSH to guests (in the Deployer) | Moves from `setup_jump_ssh_access.sh` into the Deployer's Rust side, with the same managed block, QGA-attested pin, and proof. The host side becomes the read-only `bmac guests` and `bmac guest-host-key` commands. The Dashboard shows the block for workstations without the Deployer |
| Hello app deployment (in the Deployer) | Moves from `deploy_hello_app_to_prod.sh` into the Deployer. It runs from the workstation over the guest's jump SSH entry, uploads files, and takes a pasted Cloudflare Origin CA certificate and key; it changes nothing in Proxmox, so it is a workstation operation |
| Registry reads | `bmacd` reads the v2 registry records directly; writes go through `cluster_registry.py` until the registry is ported |

## Script operations at first

| v2 workflow | Notes |
| --- | --- |
| `add_prod_vm.sh`, `remove_prod_vm.sh` | `add_prod_vm.sh` takes the chosen block size's storage instead of the fixed `PROD_VM_STORAGE`, and records it in the registry (`STORAGE.md`, "VM disk block size"). The adapted `add_prod_vm.sh` and `add_staging_vm.sh` drop their workstation SSH step and end with a next step pointing to the Deployer |
| `add_staging_vm.sh`, `remove_staging_vm.sh` | Plus `patch_staging_clone.sh`, `patch_staging_guest_tree.py`. Staging clones use their production disk's storage |
| `extend_prod_vm_disk.sh` | Its capacity math must use the `STORAGE.md` function before raidz hosts are allowed to run it |
| `change_prod_vm_placement.sh`, `change_prod_vm_owner.sh` | |
| `remove_proxmox_host.sh` | |
| `add_qdevice.sh`, `remove_qdevice.sh` | |
| Host setup phases 4 to 6 (private network, cluster, routing) | From `add_proxmox_host.sh`, split out (see `DEPLOYMENT.md`). The private-network script keeps v2's carrier wait (bring up, 10 attempts 3 seconds apart, then the speed check) and gains the same wait in automatic NIC detection, until the native link-wait function replaces it |
| Installed runtime: `cluster_registry.py`, `haproxy_routes.py`, `sync_haproxy_routes.sh`, `process_deferred_cleanup.sh`, the lifecycle hook | Installed by the `bmacd` package; the hook is packaged as `bmac-hook` |

## Adapting the scripts

Adapted copies live in `v3/legacy/scripts/`, with their tests in
`v3/legacy/tests/`. The originals in `scripts/` are never edited. Changes:

- **Where they run.** On a host, as root, started by `bmacd`, which chooses
  the host from the operation's target rule.
- **Configuration.** `bmacd` writes `cluster.conf` and the `moxN.conf` files
  from pmxcfs into `/run/bmac/config/` before each run, in exactly the format
  `config.sh` already parses (each value single-quoted, so any one-line value
  comes back exactly; directory `0700`, files `0600`). A run doesn't start if
  the files can't be written. The config-location pointer, macOS handling,
  and Git checks are removed.
- **Secrets.** Secret values come on stdin through the existing secret-field
  protocol, or from a root-only temporary file that `bmacd` deletes after the
  run. There is no `secrets.env`.
- **SSH.** `ssh moxN` keeps working: `bmacd` maintains `/etc/bmac/ssh_config`,
  mapping `moxN` to the private addresses with cluster root keys and strict
  host keys from the Proxmox cluster's known hosts. Scripts reach the QDevice
  as before. Workstation-only SSH (writing to `~/.ssh/config`, guest
  aliases) is removed.
- **Guest access** goes through the QEMU guest agent (`qm guest exec`) on
  the host running the guest, never SSH: host root keys are not authorized
  in guests. This replaces v2's three uses of the workstation's jump SSH:
  `fstrim` before decommissioning, `growpart` and `resize2fs` after growing
  a disk, and the SSH login check after creating a VM. That login check
  becomes guest-agent checks (sshd active, attested host key), and the
  Deployer's jump SSH setup proves a real login.
- **Artifacts** go to `/var/lib/bmac/work/<job>/` on the executing host.
- **JSON mode always.** The terminal mode stays available for debugging
  through `bmac run --terminal`.
- **Registry paths** point to `/etc/pve/priv/bmac/registry/`.

Every adaptation keeps the script's tests passing under
`v3/dev/run_tests.py` (a copy of the v2 runner pointed at
`v3/legacy/tests/`), with the fakes updated for host-side execution.

## Typed adapters

Native code never builds a shell command line.

- **`CommandRunner`** (`bmac-exec`): a literal program path, an argv vector, an
  environment allowlist, a working directory, stdin, a timeout, cancellation
  by process group, byte limits on captured output with streaming for long
  output, expected exit codes, and redaction. It has a real implementation
  and a fake that matches expected calls and returns recorded results.
- **Adapters** (`bmac-adapters`) wrap each external tool once, with typed
  requests, results, and errors: `ProxmoxCluster` (`pvecm`, corosync tools),
  `ProxmoxGuests` (`qm`, `pct`, `pvesh`), `ProxmoxHa` (`ha-manager`),
  `Replication` (`pvesr`), `Zfs`, `Zpool`, `Luks` (`cryptsetup`, crypttab,
  initramfs), `Disks` (`lsblk`, `blkid`, `sgdisk`, `smartctl`),
  `Network` (`ip`, nftables, keepalived), `GuestAgent`, `Tailscale`,
  `Systemd`. Output is parsed once, in the adapter. Machine-readable output
  (`-j`, `--output-format json`, `-Hp`) is used wherever the tool offers it.
- Typed identifiers and units throughout: VMID, host slot, pool, dataset,
  snapshot GUID, bytes, job ID.
- After any change, adapters verify the resulting state where the v2 scripts
  do, or where correctness requires it. A zero exit status alone is never
  taken as proof.

## Porting order

After the native-from-the-start work, roughly from least to most risk:

1. Registry writes (`cluster_registry.py` → `bmac-registry`, keeping its
   command-line interface so the remaining scripts keep working; v2's registry
   tests drive that interface and run against the Rust binary unchanged)
2. `change_prod_vm_owner`, `change_prod_vm_placement`
3. `remove_staging_vm`, `remove_prod_vm`
4. `add_staging_vm` with the clone patching
5. `extend_prod_vm_disk`
6. `add_prod_vm`
7. Host setup phases 4 to 6, `remove_proxmox_host`, QDevice workflows
8. Route rendering and sync, deferred cleanup
9. The lifecycle hook, last, because it is on the failover path. It becomes a
   standalone Rust binary that reads pmxcfs directly and never calls `bmacd`.

## Port gate

A workflow switches to native when all of these hold:

1. **Behavior checklist.** A checklist in `v3/design/workflows/<id>.md`,
   written from the script and its tests, lists every precondition, phase,
   external call, confirmation, failure path, cleanup, and resume behavior. It
   names the v2 tests each item comes from. v2 tests of things that no longer
   exist in v3 (workstation config, macOS, `~/.ssh/config`) are listed as not
   applicable, with the reason.
2. **Rust tests** cover every checklist item, using the fake `CommandRunner`.
3. **Comparison.** Where practical, the script and the native operation run
   against the same fakes, and their sequences of external calls are
   compared. Any difference is either fixed or recorded in the checklist as
   intended, with a reason. Never compared against a live cluster.
4. **Lab.** The workflow runs end to end in the nested lab, including its
   failure and resume paths and, for destructive workflows, the relevant
   drills.
5. **Hardware.** Workflows that touch HA, replication, ingress, storage, or
   membership run once on real hardware before release.
6. **Review** of the checklist and any intended differences.

## The lab

`v3/lab/` builds the nested test environment described in `ARCHITECTURE.md`
from scripts:

- three Proxmox VMs with nested virtualization, each with virtual NVMe, SATA,
  and SCSI disks carrying distinct serials and WWNs, so every vdev type and
  disk type can be tested;
- a shared private bridge standing in for the 10 Gbps VLAN;
- a Tailscale-like management network (a real tailnet or a local
  substitute);
- one appliance VM and one QDevice VM;
- serial consoles driven by scripts, for installer screens and LUKS entry.

It can install hosts from Deployer ISOs, run any workflow, power-cycle or cut
off any VM, and reset to a clean state. Lab runs are scripted scenarios with
pass/fail output, so they can be repeated after every change to a workflow.
