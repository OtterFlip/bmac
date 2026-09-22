<!--
Copyright (c) 2026 BEENTHERE VENTURES, INC.
SPDX-License-Identifier: GPL-3.0-only
-->

# Staging VM creation

`create_staging_vm.sh` creates a disposable, non-HA `stageNprodN` staging VM from
a currently-running registered production VM. Run it from an administrator
workstation:

```bash
hosts/update_cluster_runtime.sh --dry-run
hosts/update_cluster_runtime.sh
guests/staging/create_staging_vm.sh --dry-run
guests/staging/create_staging_vm.sh \
  --sanitizer /absolute/path/to/local-staging-sanitizer.sh
```

Run the runtime updater first whenever this checkout's registry, HAProxy
renderer, or lifecycle hook differs from the copies installed on the mox
nodes. Existing VMs are not restarted and do not need hook reattachment.

The optional sanitizer must be a regular, non-symlink Bash file no larger
than 1 MiB and must be idempotent. It is copied into the guest as a root-only
service that records `started` before its single automatic invocation and
`complete` afterward. An interrupted invocation is not retried automatically;
networking stays blocked until an operator reviews the partial run.

## Important unsupported-storage warning

This workflow deliberately implements the accepted source-snapshot model, but
it is not an officially supported Proxmox linked-clone workflow.

The script creates a named Proxmox VM snapshot on the current active
production owner and relies on current Proxmox ZFS replication behavior to
carry that snapshot to every production placement node. It then issues a
direct local `zfs clone` from the selected standby copy. Proxmox does not
track or protect that clone-to-production-snapshot dependency.

The retained snapshot is source-owned, not staging-owned, and can pin
substantial production ZFS space. Replication can fail if an operator deletes
or rolls it back, rebuilds a replica without it, changes storage or replication
jobs, or makes a node lacking the exact snapshot GUID a replication source
while the clone exists. Teardown must destroy every dependent clone before
deleting the production snapshot and must replicate the deletion everywhere.
The later production-start cleanup item must honor the dependency/refcount
metadata recorded by this script.

The VM snapshot requires a responsive QEMU guest agent and refuses a source
that disables guest-agent filesystem freezing, but this is not an
application-consistent database backup. Validate cloned application data
before relying on it.

Never describe this direct clone as Proxmox-supported. A dedicated independent
staging template or full copy remains the safer design.

## Preconditions

- The updated `cluster_registry.py` must already be installed by the existing
  host orchestration deployment.
- The selected production resource must be registry state `active`, running,
  registered with Proxmox HA, and governed by exactly one strict positive
  node-affinity rule matching its registry placement.
- Every placement node must be online. There must be exactly one healthy
  production replication job for each placement node other than the current
  owner.
- Every placement node must be amd64 and run Proxmox VE 9.
- Production must have exactly one replicated `scsi0` ZFS data disk. Its
  Ubuntu guest layout must be direct partitions with exactly one mounted ext4
  root; LVM, dm-crypt, MD, and other mapped root layers are rejected.
- The shared lifecycle hook and `local-zfs` storage must be available on the
  chosen standby.
- `secrets.env` must contain `STAGING_GUEST_VM_ROOT_PASSWORD` with mode `0600`.
  The script hashes it immediately, unsets every loaded secret, never logs the
  plaintext or hash, and transfers only a temporary mode-`0600` hash file.

## Selection and allocation

The script prints every registered production resource with its purpose,
primary domain, placement, state, and registry owner. After the operator
chooses `prodN`, it validates live owner/HA/replication state and automatically
chooses the lowest-numbered online eligible standby different from the active
owner.

The registry atomically reserves:

- the lowest unused `stageNprodN` stage number in
  `1..MAX_STAGING_VM_COUNT_ON_THIS_HOST`;
- the next free address from `.51-.200`;
- a live-checked free Proxmox VMID;
- a deterministic locally-administered MAC;
- `stageNprodN.<source-primary-domain>` by default (for example,
  `stage1prod1.mydomain.com`), or the exact optional
  override entered by the operator.

Before allocation, the creator loads
`MAX_STAGING_VM_COUNT_ON_THIS_HOST` from the selected standby's `moxN.conf`.
It refuses a full host, and passes the same limit into the registry's locked
allocation so concurrent creators cannot exceed the per-host cap. The registry
also restricts `N` to that range and chooses it itself; no caller can request a
specific stage number.

The operator is asked whether `net0` should start with Proxmox
`link_down=1`. The default is **network enabled**. If no sanitizer was
supplied and networking is enabled, creation requires an explicit high-risk
confirmation because generic identity cleanup does not remove
application-specific production secrets, data, jobs, or integrations.
The operator is also prompted for vCPU cores and RAM in whole GiB, using the
cluster values as defaults. `--cores` and `--memory-gib` provide noninteractive
overrides. At the same point, the operator chooses whether to start the
completed VM and configure workstation jump SSH; both default to **yes**.
Completion prints the `lan0` interface name, private IP, and FQDN.

The creator prompts for an optional staging-specific `startup.sh` (also
accepted through `--sanitizer`). It is injected as a root one-shot before
networking and is the intended place for application-specific changes such as
rewriting NGINX domains. It must be idempotent because a crash can cause a
retry. The operator also chooses whether the NIC begins link-down. Final output
always reports that network choice, status, `lan0`, the exact FQDN, and the
`.51-.200/24` private IP.

## Snapshot, replication, and clone ordering

The mutating workflow is fail-closed:

1. Transition the staging reservation to `snapshotting` and durably record
   the unique snapshot intent plus every source volume.
2. Run `qm snapshot` on the current active production owner with a unique
   `stg-base-stageNprodN-<short-UTC>` name. Raw target-only snapshots and names under
   Proxmox's `__replicate_` namespace are forbidden.
3. Trigger every production replication job and wait for a fresh successful
   sync.
4. Resolve the root zvol on every placement node and require the snapshot to
   exist with one identical numeric ZFS GUID everywhere.
5. Record the unique snapshot dependency on the staging resource: source
   resource/volume, original owner, per-node GUIDs, dependent resource list,
   and refcount `1`.
6. Create one local sparse ZFS clone on the selected standby.
7. Create a stopped QEMU shell with no data disk, no HA registration,
   `onboot=0`, exactly one NIC, the lifecycle hook, and exactly the generic
   staging, evictable, and inherited purpose tags.

Production owner movement during the guarded creation window aborts the
operation and invokes rollback.

## Offline patch gate

`patch_staging_clone.sh` runs as root on the selected host while the clone is
not attached to any VM. A global host lock serializes offline patching. The
helper:

- rejects a mounted/open zvol and maps its partitions with `partx`;
- rejects LVM, encryption, RAID, or any layout other than one ext4 root
  partition;
- mounts the root read-only first to verify Ubuntu root markers, then remounts
  it read-write with `nodev,nosuid,noexec`;
- replaces hostname, `/etc/hosts`, static netplan (`/24`, `.10` gateway,
  configured DNS, new MAC), and the root password hash;
- clears machine-id, cloud-init state, SSH host keys, and DHCP/network leases;
- installs a mandatory `ssh-keygen -A` first-boot unit required before SSH;
- optionally installs the sanitizer as a root one-shot required before
  `network-pre.target`, `network.target`, and common network managers;
- syncs and unmounts, removes partition mappings, flushes the block device,
  and verifies that no host mount or open process remains.

The actual tree rewrite is delegated to `patch_staging_guest_tree.py`. It
opens the guest root and every parent directory with descriptor-relative
`O_NOFOLLOW` operations, rejects symlink/non-regular substitutions, bounds
files it reads, and atomically replaces files in their verified parent
directories. Netplan matches the registry's deterministic MAC, renames that
interface to `lan0`, and disables DHCP; attaching the disk is forbidden until
the live QEMU `net0` MAC and bridge match the same registry values.

Only after that helper succeeds does the workstation script attach `scsi0`.
The final VM config is revalidated as stopped, non-HA, one-NIC, one-disk,
`onboot=0`, exact-tagged, and hook-attached.

## Administrative access

The clone retains the production guest's two administrator public keys and
key-only root SSH policy; offline patching replaces the console root password
hash and regenerates unique SSH host keys before SSH can start. It also removes
the clone's stale machine and network identity.

Administrators are expected to SSH to the reported private IP through a
strictly verified mox Tailscale `ProxyJump`, just as for production. Record and
verify the newly generated staging SSH host key before first use. When selected
during request collection, the creator waits up to 180 seconds for QGA and the
regenerated Ed25519 key, then installs and validates the strict workstation
alias automatically.

## Route and start behavior

The exact staging domain is enabled in the registry and the installed
transactional HAProxy synchronizer runs across online mox hosts. If the
operator declined automatic start during request collection, the result is a
fully registered, routed, stopped VM. If production moved onto the staging
node, or if the lifecycle hook rejects admission, staging remains stopped.

The first boot regenerates machine identity and SSH host keys. An injected
sanitizer is invoked automatically once as root before networking; its failure
prevents the required network targets/services from starting.

## Destroying a staging guest

Run the read-only plan first, or omit the resource name to list registered
staging guests and select one interactively:

```bash
guests/staging/destroy_staging_vm.sh --dry-run
guests/staging/destroy_staging_vm.sh
```

Destruction requires every source production placement node online. It
validates the exact staging name, VMID, tags, fixed node, VM disks, linked
clone origin, one source-owned snapshot identity, and that snapshot's
registered GUID on every placement node. It then disables the exact route,
stops and destroys the staging VM and clone, deletes only that one named
snapshot, triggers production replication, verifies that copies of the same
snapshot disappear everywhere, and releases the registry allocation. It
does not delete any other production snapshot or stop the production VM.
Staging shutdown is always a force-stop (`qm stop --skiplock`), including
operator destruction, creation rollback, and production-start eviction; it
does not wait for a graceful guest shutdown.

## Failure behavior

Before the offline patch gate and route transaction commit, traps attempt to:

1. stop and destroy the newly-created QEMU VM;
2. destroy the local linked clone;
3. delete the named Proxmox production snapshot;
4. trigger every production replication job to propagate snapshot deletion;
5. clear/release the staging registry allocation; and
6. resynchronize HAProxy if a route may have been installed.

If destructive cleanup cannot be proven, the failed registry record is
retained in `cleanup_pending` rather than discarding the dependency metadata;
idempotent deferred VM, volume, and per-node snapshot cleanup records are
queued for the later cleanup controller.
An offline-patch failure never attaches or starts the clone. A failure of the
optional post-commit start attempt does not destroy an otherwise valid stopped
staging environment.

## Tests

```bash
bash -n \
  guests/staging/create_staging_vm.sh \
  guests/staging/destroy_staging_vm.sh \
  guests/staging/patch_staging_clone.sh

PYTHONDONTWRITEBYTECODE=1 python3 -m unittest \
  guests/staging/test_staging_vm.py -v

PYTHONDONTWRITEBYTECODE=1 python3 -m unittest \
  guests/staging/test_destroy_staging_vm.py -v

PYTHONDONTWRITEBYTECODE=1 python3 -m unittest \
  lib/test_shared_libs.py -v
```

The tests exercise shell syntax, argv-safe remote execution, offline identity
and sanitizer rendering, snapshot metadata/refcount invariants, rollback
guards, and a fully mocked read-only `--dry-run` that checks secrets are not
exported to SSH and no Proxmox/registry mutation is attempted.
