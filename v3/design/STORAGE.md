<!--
Copyright (c) 2026 BEENTHERE VENTURES, INC.
SPDX-License-Identifier: GPL-3.0-only
-->

# BMAC v3 storage

Pool layouts, disks, LUKS, and the storage workflows. v2 supported one to five
NVMe mirror pairs (`NVME_MIRROR_<P>_SERIAL_<M>` in `moxN.conf`). v3 supports
any vdev type and any disk type, with LUKS wherever encryption is enabled. The
same model is used for Proxmox hosts and for backup appliances.

The v2 behavior this generalizes is described in
`scripts/user_callable/hosts/README.md` ("Adding and decommissioning rpool
disks") and implemented in `scripts/host_runtime/rpool_mirror.sh`; its tests
in `scripts/tests/test_rpool_mirror.py` and `test_disk_workflows.py` are the
behavioral reference.

## The layout model

The `bmac-storage` crate holds the model and all validation. The Deployer's
Rust backend, `bmacd`, `bmac-cli`, and `bmac-backupd` use it, so every place
applies the same rules.

A **pool layout** is a list of top-level vdevs. Each vdev has a class, a type,
and member disks.

| Class | Types | Notes |
| --- | --- | --- |
| Data | single disk, mirror (2 or more members), raidz1, raidz2, raidz3 | dRAID is deferred; the model leaves room for it |
| Special (metadata and small blocks) | single disk, mirror | Warn if less redundant than the data vdevs: losing it loses the pool |
| Log (SLOG) | single disk, mirror | Warn on a single disk |
| Cache (L2ARC) | single disks | No redundancy needed |
| Spare | single disks | Must be at least as large as the largest member it could replace |

Non-redundant data vdevs (a single disk, or a pool striped across single
disks) are allowed. The layout summary states plainly that losing that disk
loses the pool.

A **disk** is identified by:

- WWN when the disk reports one, otherwise serial number (NVMe disks usually
  report an EUI or NGUID; SATA and SAS disks a WWN). When an NVMe namespace
  has no usable identifier, or the kernel ignores it for a quirky
  controller, Linux still fills in a WWN of the form
  `nvme.<vendor>-<serial hex>-<model hex>-<nsid>`. That value only restates
  the serial, so it counts as no WWN, and the disk is identified by serial
  (QEMU's NVMe disks in the lab are like this);
- exact capacity in bytes;
- logical and physical sector size;
- transport (NVMe, SATA, SAS);
- whether it rotates;
- model.

Disks are always addressed through `/dev/disk/by-id/`, never by `/dev/sdX`
or `/dev/nvmeXnY` names.

Stored layout (in the host facts in pmxcfs, and in the ISO):

```json
{
  "schema_version": 1,
  "pool": "rpool",
  "ashift": 12,
  "encryption": "luks2",
  "vdevs": [
    { "class": "data", "type": "mirror", "boot": true,
      "members": ["wwn-0x5000c500a1b2c3d4", "wwn-0x5000c500a1b2c3d5"] },
    { "class": "data", "type": "raidz2",
      "members": ["wwn-0x5000c500a1b2c3d6", "wwn-0x5000c500a1b2c3d7",
                  "wwn-0x5000c500a1b2c3d9", "wwn-0x5000c500a1b2c3da"] },
    { "class": "special", "type": "mirror",
      "members": ["nvme-eui.0025385b71b0a1c2", "nvme-eui.0025385b71b0a1c3"] },
    { "class": "spare", "type": "single",
      "members": ["wwn-0x5000c500a1b2c3d8"] }
  ]
}
```

## Choosing a layout

The Deployer (host and appliance ISOs) and the Dashboard (adding vdevs later)
use the same flow:

1. **Inventory.** In the Deployer, from a discovery session, where the
   machine itself reports its disks (`DEPLOYMENT.md`, "Hardware discovery"),
   or from a `bmac-inventory` file run on the machine. Never from iDRAC. In
   the Dashboard, from the host's own disk collector.
2. **Pick a type, then members.** For each vdev, choose its class and type,
   then select member disks from the inventory. Disks already used are not
   offered. **Before any type is picked,** every type on offer shows its
   restrictions next to it, not behind a link, so they are read before the
   choice rather than discovered later. Most important: raidz rules out ever
   removing a vdev again (v2's disk decommissioning), and a single disk has no
   redundancy. Pros and cons per type are in [`../README.md`](../README.md)
   ("Choosing vdev types"), and the UI uses the same wording.
3. **Review.** The summary shows, before anything is written:
   - usable capacity per vdev and in total, using the same calculation as the
     capacity checks (see below);
   - how many disks each vdev can lose, and what losing more would mean;
   - whether LUKS is possible for this layout;
   - which later operations the layout allows and which it rules out,
     especially "this pool can never be shrunk";
   - every warning, each needing an explicit acknowledgment.
4. **Confirm.** Destructive confirmation lists every disk to be erased by
   identity, capacity, and model.

### Checks

Refused:

- disks with no serial number and no WWN;
- logical disks presented by a hardware RAID controller (for example a PERC
  virtual disk). Disks must be on an HBA or in passthrough mode, so ZFS and
  SMART see the real drive;
- a disk listed twice, or already part of a pool or in use;
- a spare smaller than the largest data member it could replace;
- a LUKS layout whose boot vdev is a single disk (see "LUKS");
- a boot vdev of more than two disks, or one the Proxmox installer can't be
  pointed at (see below).

Warned and acknowledged:

- members of one vdev with different capacities (usable size follows the
  smallest) or different speeds (SSD mixed with spinning disks, NVMe mixed
  with SATA);
- a vdev whose redundancy differs from the rest of the pool. ZFS itself needs
  `-f` for this, and BMAC passes it only after the acknowledgment;
- a special or log vdev less redundant than the data vdevs;
- non-redundant data vdevs;
- USB-attached disks.

**ashift** is chosen once per pool: 12, or higher if any member's physical
sector size needs it. Every vdev added later uses the pool's ashift. Removing
vdevs requires a uniform ashift, so this is never varied.

## Proxmox hosts: the boot vdev

The Proxmox automated installer creates the root pool. v3 uses the installer
only for the **boot vdev**: a single disk or a two-way mirror, whose members
all get an ESP managed by `proxmox-boot-tool`. Every other vdev, raidz
included, is created by the host setup job after installation, as v2 does
for mirrors 1 to 4.

Two disks is the most the installer can be pointed at. Its answer file
selects disks by udev property, naming each property once, and only two
properties read the same in the discovery OS and in the installer:
`ID_SERIAL_SHORT` and `ID_WWN` (systemd spells `ID_SERIAL` differently).
The two boot disks therefore need distinct values of different properties,
in practice the serial of one and the WWN of the other; the Deployer checks
this when the layout is chosen.

Each boot member's ZFS partition is limited with the installer's `hdsize` so
that at least 4 GiB stays free at the end of the disk for the LUKS header and
size tolerance, as v2 does today.

## LUKS

When encryption is enabled, every member of every vdev class (data, special,
log, cache, spare) sits on a LUKS2 device. Cache devices hold plaintext copies
of pool data, and spares become data members when they activate, so neither
is exempt.

**Partitioning.** Non-boot disks get one partition that ends 1 GiB short of
the disk's whole-GiB size, so a slightly smaller replacement still fits.
Boot disks keep the installer's layout plus the `hdsize` reserve.

**Mapper names** come from the disk's identity, not its position in a vdev:
`bmac-<pool>-<id>`, where `<id>` is a short form of the WWN or serial. vdev
numbering changes when vdevs are removed, and raidz members have no "mirror N
member M" position, so v2's position-based names (`crypt-rpool-a`,
`crypt-rpool-mirrorN-M`) are replaced.

**Unlocking at boot.** Every member is listed in `/etc/crypttab` in v2's
single-prompt `decrypt_keyctl` form and unlocked in the initramfs, so one
console passphrase unlocks all of them. Before any new member joins the pool
(`zpool add`, `attach`, or `replace`), the workflow rebuilds the initramfs and
proves that it unlocks the new device, as v2 does today.

**Encrypting the boot vdev in place.** The installer cannot create LUKS, so
the setup job converts the boot vdev one member at a time:

1. take the member offline;
2. LUKS-format its ZFS partition;
3. `zpool replace` the old partition with the new LUKS device;
4. wait for the resilver;
5. go on to the next member.

During each member's conversion the mirror has one disk less redundancy,
which for a two-way mirror is none.
The pool is nearly empty right after installation, so each resilver takes
minutes even on large spinning disks. A single-disk boot vdev has no
redundancy to convert with, so a LUKS host needs a mirrored boot vdev. Vdevs
created later are encrypted before the vdev exists, so they can be any type.

The passphrase handling is v2's: typed at the console, held in a root-only
temporary file during the job, deleted and verified absent afterwards
(`DEPLOYMENT.md`).

**Header backups.** After any LUKS change, the header of every affected member
is backed up (`DEPLOYMENT.md`, host setup, step 7).

**Boot drills** generalize v2's. For each boot-vdev member, boot with that one
member missing, then restore it and resilver. Then reboot the complete pool.
On LUKS hosts every boot waits for the console passphrase.

**Spares and the boot vdev.** A hot spare that replaces a failed boot-vdev
member has no ESP. The pool is whole again, but the host has one fewer
bootable disk. Monitoring raises "boot redundancy reduced" until the failed
disk is replaced properly, and the replacement workflow restores the ESP.

## What each layout allows later

| Operation | Allowed when | Notes |
| --- | --- | --- |
| Add a data vdev | Always | Warn on mismatched redundancy |
| Widen a mirror (`zpool attach`) | Mirror vdevs | Includes turning a single disk into a mirror |
| Add a disk to a raidz vdev (`zpool attach`) | raidz vdevs, OpenZFS 2.3 or newer | Proxmox VE 9 ships OpenZFS 2.3; verify on the target. Existing data keeps its old data-to-parity ratio until rewritten |
| Replace a disk (`zpool replace`) | Any redundant vdev, or any vdev while the old disk still works | Boot members also get the survivor's partition table and an ESP, as in v2 |
| Grow by replacing with larger disks | Any vdev | `autoexpand` once every member is larger |
| Remove a data or special vdev (`zpool remove`) | Only when the pool has no raidz vdevs and a uniform ashift | v2's decommission workflow is unavailable on raidz pools; the layout summary says so up front |
| Add or remove log, cache, spare | Always | |
| Detach a mirror member | Mirror vdevs | |

## Capacity calculations

v2's capacity rules (keep 10% of each placement pool free, minimum free space
for decommissioning, growth limits in `extend_prod_vm_disk.sh`, reservation
ratios for full zvols) assume mirrors, where `zfs list` reports space
accurately for VM disks. That is not true on raidz.

VM disks are zvols with a small block size (`volblocksize`, 16 KiB by
default on Proxmox). On raidz, each small block also needs parity and padding
sectors, so a zvol can take much more raw space than its logical size. ZFS's
space reporting doesn't show this, so v2's arithmetic would overestimate free
space on raidz, and a "fully reserved" zvol could still run out of space.

### How raidz allocates a block

For a raidz vdev with `N` disks, parity `p`, and sector size `s = 2^ashift`,
a block of `B` bytes (after compression) takes this many sectors. This is the
calculation in OpenZFS's `vdev_raidz_asize()`:

```text
d      = ceil(B / s)                       data sectors
parity = p × ceil(d / (N - p))             parity sectors, one set per row
A(B)   = roundup(d + parity, p + 1)        padded to a multiple of p + 1
```

Raw efficiency for blocks of size `B` is `E(B) = d / A(B)`. For a mirror of
`N` disks it is simply `1 / N`.

### Why `zfs list` misleads

ZFS reports `used` and `available` in "deflated" units: for each raidz vdev,
raw space is scaled by `E(128 KiB)`, the efficiency of a 128 KiB block. So
1 GiB of `available` means room for 1 GiB of 128 KiB blocks. A block smaller
than 128 KiB, with worse efficiency, is charged more than its size. The
**inflation factor** for a zvol with block size `b` is:

```text
I(b) = E(128 KiB) / E(b)
```

A fully written, incompressible zvol of size `V` uses about `V × I(b)` of
`available`, plus metadata. ZFS's automatic `refreservation` for a full zvol
is roughly `V` plus metadata, which assumes `I(b) = 1`.

### Examples (ashift 12, 4 KiB sectors)

| Layout | Efficiency at 16 KiB | at 64 KiB | at 128 KiB | Inflation at 16 KiB | at 64 KiB |
| --- | ---: | ---: | ---: | ---: | ---: |
| 2-way mirror | 50.0% | 50.0% | 50.0% | 1.00 | 1.00 |
| raidz1, 3 disks | 66.7% | 66.7% | 66.7% | 1.00 | 1.00 |
| raidz1, 4 disks | 66.7% | 72.7% | 72.7% | 1.09 | 1.00 |
| raidz1, 5 disks | 66.7% | 80.0% | 80.0% | 1.20 | 1.00 |
| raidz1, 6 disks | 66.7% | 80.0% | 80.0% | 1.20 | 1.00 |
| raidz2, 4 disks | 44.4% | 48.5% | 48.5% | 1.09 | 1.00 |
| raidz2, 6 disks | 66.7% | 66.7% | 66.7% | 1.00 | 1.00 |
| raidz2, 8 disks | 66.7% | 66.7% | 71.1% | 1.07 | 1.07 |
| raidz2, 10 disks | 66.7% | 76.2% | 76.2% | 1.14 | 1.00 |
| raidz3, 7 disks | 50.0% | 57.1% | 57.1% | 1.14 | 1.00 |

Two things stand out:

- At 16 KiB, every raidz1 layout stores data at two-thirds efficiency. A
  6-disk raidz1 holds no more VM data per raw byte than a 3-disk one.
- A 64 KiB block size removes most of the inflation.

### Rules

1. **One capacity function.** `bmac-storage` computes `E` and `I` from each
   vdev's real geometry (`N`, `p`, ashift) and a block size. Every capacity
   decision uses it: the layout summary, the 10% reserve, disk growth limits,
   staging space checks, placement additions, and restore space checks.
2. **Mixed pools.** ZFS spreads writes over all data vdevs. The function
   uses the largest inflation among the pool's data vdevs, which is a safe
   upper bound.
3. **Space needed for a zvol** of size `V` and block size `b`:
   `V × I(b) × (1 + m(b))`, where the metadata allowance
   `m(b) = 128 / b` covers one 128-byte block pointer per data block. That
   is about 0.8% at 16 KiB. ZFS keeps two copies of metadata, but compresses
   the indirect blocks that hold the pointers, so the lab measured 0.2 to 0.8
   times `128 / b` on raidz1, raidz2, and raidz3 of 3 to 10 disks at every
   offered block size (M8.5, `bmac-storage`'s `tests/fixtures/lab/capacity.json`).
   Twice the allowance, as first planned, overstated a 16 KiB zvol by more
   than 1%.
4. **Compression** is ignored for reservations and limits, which assume
   incompressible data, the worst case. The Dashboard can also show the
   measured compression ratio.
5. **Full (reserved) zvols on raidz** get `refreservation = V × I(b) ×
   (1 + m(b))`. If the installed OpenZFS's own automatic value is already at
   least that large (newer releases may account for raidz), BMAC keeps
   ZFS's value. The lab checks which applies to the supported Proxmox
   version.
6. **v2's checks** use the function. For example, the disk-growth limit
   becomes: growth `ΔV` is allowed on every placement host only when
   `ΔV × I(b) × (1 + m(b))` fits within `available` minus 10% of
   (`used` + `available`). Each host's own layout gives its own `I(b)`, and
   `b` is that VM's actual `volblocksize`, which replication preserves.
7. **Block size per VM disk**, chosen when the VM is created (see "VM disk
   block size" below).
8. **Special vdevs.** If `special_small_blocks` were at least the zvol block
   size, all VM data would go to the special vdev. BMAC keeps it below the
   smallest offered block size on guest datasets, and checks the special
   vdev's own free space separately.
9. **After raidz expansion,** data written before keeps its old
   data-to-parity ratio. ZFS keeps reporting space with the old ratio, which
   understates free space. BMAC uses the reported figure, which errs on the
   safe side, and says so in the summary.
10. **Layout summary.** For raidz, the summary shows usable space for VM
    disks at each offered block size, next to the nominal capacity. Example:
    "6 × 4 TB raidz1: nominal 20 TB (5 of 6 disks), ZFS will report about
    19.2 TB, VM disks at 16 KiB: 16 TB. raidz1 wastes space on small blocks;
    32 KiB would give 19.2 TB."
11. **Verified in the lab.** For each supported layout and each block size,
    the lab creates zvols, fills them with random data, and compares
    `zfs get used,referenced` with the prediction. The function's tests
    include those measurements, and must agree within 1%.

### VM disk block size

The operator chooses each VM disk's block size when creating the VM, from a
suggested default that suits the pools the VM will live on.

**How it maps onto Proxmox.** Proxmox sets block size per storage (the
`blocksize` option of a `zfspool` storage), not per disk. So BMAC defines one
Proxmox storage per offered block size, each on its own dataset, on every
host:

| Storage | Dataset | Block size |
| --- | --- | --- |
| `local-zfs` | `rpool/data` | 16 KiB (Proxmox's default; also used for disks created outside BMAC) |
| `bmac-zfs-32k` | `rpool/data-32k` | 32 KiB |
| `bmac-zfs-64k` | `rpool/data-64k` | 64 KiB |
| `bmac-zfs-128k` | `rpool/data-128k` | 128 KiB |

Each storage has its own dataset because two storages on one dataset would
each list the other's disks. Since a disk's storage carries its block size,
everything Proxmox does that writes a new copy of a disk keeps it:
replication, migration, Move disk, and restoring a Proxmox backup. BMAC's
own restores keep it too (`BACKUP.md` checks `volblocksize`). The lab
confirms each of these. The set of offered sizes is a cluster setting,
`VM_BLOCK_SIZES` (default: all four). Setup still creates all four storages
on every host, so changing the setting only changes what new VMs are
offered.

**Suggested default.** The smallest offered size whose inflation is at most
1.05 on every placement host of the VM. On mirror-only hosts that is 16 KiB.
On a 5-disk raidz1 it is 32 KiB, on a 10-disk raidz2 64 KiB, and on an
8-disk raidz2 128 KiB.

**What the form shows** for each size: the space the disk will really take
on each placement host, using the capacity function, and the trade-off.
Larger blocks waste less space on raidz and suit large sequential I/O.
Smaller blocks suit small random writes (databases, mail), where a large
block means rewriting the whole block for every small change.

**Afterwards.**

- The VM's registry record stores its storage, and every workflow (disk
  growth, staging clones, placement changes, backups) uses it.
- Adding a placement host whose layout has worse inflation for the VM's
  block size is checked and warned about with that host's numbers.
- Changing an existing disk's block size means copying the whole disk, so
  v3.0 doesn't offer it.

## Workflows

v2's storage workflows are reimplemented natively in Rust, because their
mirror-only logic needs redesign anyway (`WORKFLOW_MIGRATION.md`).

| v3 operation | Replaces (v2) | Does |
| --- | --- | --- |
| `storage.add_vdev` | `add_new_disk_vdev.sh` | Add a vdev of any class and type |
| `storage.attach_disk` | (new) | Widen a mirror, or add a disk to a raidz vdev |
| `storage.replace_disk` | `add_replacement_disk.sh` | Replace a failed or unwanted member; spares are returned after replacement |
| `storage.remove_vdev` | `decommission_disks.sh` | Prepare and start a vdev removal where the layout allows it; v2's preparation steps are kept (destroy related staging, trims, forced replication, scrub, minimum free space) |
| `storage.inventory` | `inventory_disks.sh` | List disks, finish removals (close LUKS, update crypttab and initramfs), list disks safe to pull |
| `storage.simulate_failure` | `simulate_disk_failure_and_replacement.sh` | Failure drill |
| Host setup, storage phase | `add_proxmox_host.sh`, `rpool_mirror.sh` | Encrypt boot vdev, create the other vdevs, boot drills |

Every workflow keeps v2's safety behavior: identity checks by serial or WWN
before any destructive step, the passphrase proven against existing members
before a new disk is touched, no pool change until the initramfs unlocks the
new devices, resumable phase records on the host, and dry-run plans.

## Backup appliances

Appliances use the same model with three differences:

- The OS is on its own disk or disks, so the backup pool has no boot vdev,
  no ESPs, and no installer limits. Any layout is allowed.
- LUKS devices are created before the pool, so a single-disk vdev can be
  encrypted too.
- The pool is not unlocked at boot. The operator unlocks it at the console
  with `bmac-backupd unlock` (one prompt for all members), and the pool is
  imported afterwards (`BACKUP.md`).

Received VM disks keep their source block size, so a raidz appliance pays
the same small-block inflation. The appliance's capacity figures and
retention space checks use the same capacity function, with each backed-up
disk's block size.
