<!--
Copyright (c) 2026 BEENTHERE VENTURES, INC.
SPDX-License-Identifier: GPL-3.0-only
-->

# Production VM creation

`create_prod_vm.sh` creates or resumes a movable `prodN` VM from an
administrator workstation. It uses the shared strict configuration loader and
the pmxcfs registry installed by `setup_proxmox_host.sh`.

This is intentionally generic. Application installation and updates happen
afterward through each application's own deployment tooling.

## Prerequisites

- Run host setup successfully on every intended placement node.
- Keep `cluster.conf`, each `moxN.conf`, and mode-`0600` `secrets.env` in the
  existing `env` layout.
- Configure a public HTTPS ISO URL in `PROD_GUEST_OS_ISO_URL`, with its
  independently reviewed SHA-256 in `PROD_GUEST_OS_ISO_SHA256`.
- Set optional `PROD_GUEST_OS_INSTALL_MODE` to `ubuntu-autoinstall` or
  `manual`. An omitted value defaults to `ubuntu-autoinstall`.
- Install workstation commands `python3`, `openssl`, `ssh`, and `scp`.
- Ensure strict SSH host-key entries and key-based root access work for at
  least one `moxN`.
- Keep at least two contiguous nodes (`mox1`, `mox2`, …) online, quorate, and
  provisioned with `local-zfs`, disk-backed local ISO storage, and the
  lifecycle hook. A two-node cluster must report its external QDevice alive
  and voting from both placement nodes (three expected/total votes).

The root console password is read from `PROD_GUEST_VM_ROOT_PASSWORD`, converted
immediately to a SHA-512 crypt hash using a stable cluster-scoped salt, and
unset before remote or builder processes run. The stable non-secret salt makes
exact installed-hash attestation resume-safe; the plaintext is never printed
or written. The custom ISO contains only that password hash and the two
configured administrator public keys. Cloud-init applies the existing root
account's `hashed_passwd`; after installation QGA returns only a SHA-256 digest
of the shadow entry so the creator can compare it without returning or logging
the password hash.

## Usage

Start with a live read-only rehearsal:

```bash
guests/prod/create_prod_vm.sh --dry-run
```

Create or resume:

```bash
guests/prod/create_prod_vm.sh
```

The purpose slug is an arbitrary operator-chosen application/workload label,
such as `mydomain`. It becomes the `purpose-<slug>` Proxmox tag and is the
durable resume key. It does **not** select the guest role: this script always
creates production guests, while `create_staging_vm.sh` creates staging
guests and inherits the source production purpose. A rerun with the same
purpose shows the existing allocation and registered sizing, asks for
confirmation, and continues after validating live state.

The script prompts for:

- eligible HA placement nodes (at least two) and the initial node
- unique application/workload purpose slug, a primary FQDN, and optional
  alias FQDNs
- CPU cores, RAM in GiB, root disk in GiB, and sparse (the default) or
  full/refreserved ZFS allocation. The prompt warns that with full
  allocation the host's disks cannot later be decommissioned easily, and
  not with any BMAC script
- replication interval in minutes
- an optional local regular file named `startup.sh`
- whether the final VM network link is enabled (default: yes)

The `PROD_VM_MEMORY_GIB` cluster default and the prompt are whole GiB values.
The creator converts the selected value to MiB only for the registry and
Proxmox APIs.
Default staging FQDNs are derived later as
`stageNprodN.<primary-FQDN>`, such as `stage1prod1.mydomain.com`. This keeps the
staging name directly beneath the application's domain so a first-level
wildcard certificate can cover it.

It then reserves the next `prodN`, production IP, deterministic MAC, and
live-checked Proxmox VMID through one atomic `cluster_registry.py
allocate-prod` mutation.

## Installation and HA ordering

The generated VM has one VirtIO NIC and one `scsi0` data disk on `local-zfs`.
It uses q35, OVMF with a 4 MiB EFI-vars volume, pre-enrolled Secure Boot keys
including the complete Microsoft 2023 certificate/KEK set
(`ms-cert=2023k`), the configured cluster-safe CPU type, QGA, the generic
production tag, a purpose tag, and the installed
production-first lifecycle hook. It starts with autostart disabled and is not
HA-managed yet. The chosen allocation is applied and verified on every
replica. Sparse (no ZFS refreservation) is the default because it lets an
`fstrim` inside the guest return freed blocks to `rpool`, which top-level
vdev removal (`hosts/decommission_disks.sh`) depends on. The cost is that
`rpool` can be overcommitted: monitor pool free space on every placement
node, including room for replication snapshots. Full allocation applies a
ZFS refreservation instead, so placement nodes need free space for that
reservation, and space freed inside the guest stays reserved to it.
Existing VMs are not converted.

The selected initial Proxmox node downloads the configured source directly
over HTTPS. A root-only helper uses
`/var/lib/app-ha-proxmox/iso-cache/<sha256>.iso`, verifies the configured
SHA-256 before every use, and maintains an adjacent `.sha256` file. A matching
cache entry is reused even if the configured URL changes; a missing or corrupt
entry is downloaded to a `.partial` file, verified, and atomically installed.
The cache and per-VM output must be disk-backed.

The host-installed builder creates the customized per-VM ISO directly in the
selected node's Proxmox ISO storage. Only the small mode-`0600` build request
crosses the workstation SSH path. The per-VM ISO remains attached while the
unattended installer runs. After the operator confirms successful installation
and poweroff, the creator records that phase, detaches and deletes the per-VM
ISO, and continues guest verification. The verified vanilla source ISO and its
checksum sidecar remain cached; completion prints both paths so an operator can
remove them manually when they are no longer useful.

`ubuntu-autoinstall` requires a compatible Ubuntu live-server ISO and embeds
the generated NoCloud answer data. `manual` attaches a verified byte-for-byte
copy of the configured source instead. Before starting manual installation,
the creator prints the exact supported Linux contract: systemd, Python 3,
hostname, `lan0` static addressing, QGA, OpenSSH, both administrator keys,
key-only root access, the configured console password, and no Tailscale. The
operator must satisfy that contract and power off from the console before
confirming completion. Manual mode cannot embed `startup.sh` or attest the
exact installed console-password hash. Windows and operating systems that
cannot satisfy the printed Linux/QGA/SSH contract are not supported by this
creator.

The per-VM ISO embeds:

- hostname and fixed `.31-.50/24` address
- the floating `.10` default route and configured DNS servers
- `qemu-guest-agent` and OpenSSH
- key-only root SSH for both configured administrators
- a hashed console root password
- when selected, `startup.sh` plus a root systemd oneshot ordered before
  `network-pre.target`

The startup service writes `/var/lib/production-startup/completed` only after
the script exits successfully. It never runs again after that marker exists.
A failed attempt is retried on the next boot, so the supplied script **must
be idempotent** and must not depend on networking. Its SHA-256 fingerprint
becomes part of the durable orchestration contract; a pre-install resume must
provide the same file.

The unattended installer powers the VM off. Durable pmxcfs orchestration
metadata records `unstarted`, `media-attached`, `installer-started`,
`installed-confirmed`, `guest-verified`, and `network-finalized` phases. A resume never starts an
installer from an ambiguous or previously started phase. Re-running a failed
or legacy/unknown installation requires typing exactly `WIPE`; confirming a
completed poweroff records `installed-confirmed` before media is detached.
The installed VM is then started, QGA is checked, its root shadow digest and
SSH host key are attested through QGA, and strict root SSH is verified through
the mox jump host. Tailscale is not installed in production guests.
The installer and those checks always run with the VM link enabled. Only
afterward does the creator durably apply the requested final `net0`
`link_down` policy. Root SSH is key-only; the root password remains usable
only from the VM console.

Only after guest verification does the script:

1. create one `pvesr` job for every placement node other than the current
   owner;
2. capture every job's prior `last_sync`, schedule it, and require a newer
   successful sync on every target;
3. verify that owner plus healthy targets exactly equals placement;
4. record the VM as an ignored HA resource with `failback=0` and
   `auto-rebalance=0`, add a strict positive PVE 9 node-affinity rule with
   priority `1` on every eligible host, and change the HA request to started;
5. register the requested primary/alias Host and SNI routes, transactionally
   synchronize HAProxy, and transition the guest through `ready` to `active`.

Ingress mappings intentionally exist before application deployment. HAProxy
may therefore report the backend unavailable until the application, TLS, and
health endpoint are ready. The final warning makes that state explicit.

## Application handoff

After `create_prod_vm.sh` completes, provision and deploy the intended
application over the private guest address, then verify its HTTPS health from
the mox network and through every public origin. Until then, an unavailable
backend response is expected. The creator does not deploy application code.

Every guest SSH and SCP operation uses the selected mox as a strict
`ProxyJump`. The guest is addressed by its registry private IP and must not
have Tailscale installed. No sibling `.env` or application-specific
provisioner is used by this project.

After successful creation, the creator offers to configure a permanent
workstation SSH alias. It obtains the Ed25519 host key through QGA, displays
its fingerprint, detects existing `known_hosts` and SSH-config use, and asks
before replacing effective values or lets the operator choose another alias.
It writes an app-ha-delimited strict `ProxyJump` block and pinned host key
atomically, tests key-only SSH, and restores both workstation files if
validation fails. Repeating the setup with the same key and effective alias is
a no-op.

Inspect a completed production VM across registry, Proxmox, HA, replication,
storage, QGA, private networking, and strict SSH without changing it:

```bash
diagnostics/show_prod_vm_state.sh prodN >prodN_state.txt
```

## Destruction

Rehearse and then permanently remove one production resource:

```bash
guests/prod/destroy_prod_vm.sh --dry-run prodN
guests/prod/destroy_prod_vm.sh prodN
```

The destroyer refuses production resources with staging dependents, requires
every placement node online, validates exact registry/live identity, tags,
root and EFI volumes, HA request/rule, and replication topology, then requires
the exact confirmation `DESTROY prodN`. It disables and converges ingress,
stops/removes HA, removes the strict node-affinity rule, removes replication
jobs and target copies, destroys only the validated VM and VMID-owned volumes,
proves absence on every placement node, clears durable metadata, and archives
the released registry record. The hash-addressed source ISO cache below
`/var/lib/app-ha-proxmox/iso-cache` is deliberately retained.

## Failure and resume behavior

Temporary local request material lives under `guests/prod/artifacts` with
directory mode `0700` and files at `0600`; it is removed on exit. Remote
request files are mode `0600` below `/run` and are removed after the build or
by cleanup. A known-unattached per-VM ISO is removed after a failed run. If an
SSH reply makes attachment status ambiguous, that customized ISO is retained
for a safe rerun rather than risking deletion of mounted installation media.
Attached per-VM media is deleted immediately after detach. The hash-addressed
vanilla source cache is deliberately retained.

Each run acquires a per-resource lease in pmxcfs before mutating the VM. The
lease is renewed before long install/replication/HA phases and released after
cleanup; a second creator cannot orchestrate that resource concurrently.
Failure traps deliberately preserve the registry allocation and every
successfully created VM, replica, and HA rule. Correct the fault and rerun
with the same purpose. A missing `ready`, `active`, `stopped`, or otherwise
previously provisioned VM is never recreated. Only a metadata-empty
`reserved` resource with durable phase `unstarted` can create a missing VM;
a legacy reserved record with no sentinel additionally requires exact
`WIPE` confirmation.
Unexpected VM identity, exact root volume/size/options, ZFS allocation,
firmware/Secure Boot options, placement, tags, replication targets, HA
restart/relocate policy, HA rules, or SSH keys cause a closed failure rather
than adoption or replacement.

The final summary shows the VM name, private IP, primary FQDN, aliases,
default staging-name pattern, and every public origin IP with the initial node
first.

The `qm`, `pvesr`, and PVE 9 `ha-manager rules` calls cannot be exercised
off-cluster. Their syntax is kept adjacent to comments in the script; the
mock-friendly unit tests cover input validation, configuration rendering,
boot-metadata replay, fail-closed node selection, dry-run ordering, and the
absence of destructive rollback commands.

## Tests

```bash
bash -n guests/prod/create_prod_vm.sh

PYTHONDONTWRITEBYTECODE=1 python3 \
  guests/prod/test_create_prod_vm.py -v
```
