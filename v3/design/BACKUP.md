<!--
Copyright (c) 2026 BEENTHERE VENTURES, INC.
SPDX-License-Identifier: GPL-3.0-only
-->

# BMAC v3 backup and recovery

Offsite backups of production VMs to independent backup appliances, local
fast recovery, restore, and recovery from complete cluster loss. This carries
forward Part B of [`BMAC_MAJOR REWRITE_SPEC.md`](BMAC_MAJOR%20REWRITE_SPEC.md),
changed so that **appliances pull** from the cluster, **the cluster owns the
schedule**, and checkpoints on the cluster are **bookmarks** rather than
held snapshots. Where the two disagree, this document wins. The full design
ships in v3.0.

## What this provides

- Point-in-time recovery sets (PITs) of each production VM: every disk
  snapshotted together, plus the exact VM configuration and BMAC metadata.
- One backup schedule per production VM, kept on the cluster, and any number
  of enrolled appliances assigned to each VM (many to many).
- Offsite PITs on each assigned appliance, each appliance with its own
  retention and history.
- Optional local PITs on the cluster for fast clones and rollback (none by
  default).
- Restore of any retained PIT to a new VM or into the existing one, moving
  and writing only what the available snapshots require, a promotion
  procedure to make it production, backups that continue incrementally from
  the restored PIT, and recovery of a whole cluster onto new hardware.
- Repair of an appliance that fell behind, from the cluster or from a peer
  appliance, when the two appliances trust each other.
- A PIT graph (the branch tree of each VM's history) and transfer throughput
  in the Dashboard and the appliance UI.

It does not provide application consistency beyond what guest filesystem
freeze or operator hooks give, and it does not back up anything outside the
VMs and BMAC's own metadata (Cloudflare, DNS, provider networking). Restores
are started by hand; there are no scheduled restores.

## Roles and credentials

| Party | Holds | Can |
| --- | --- | --- |
| `bmacd` on each host | Full authority on the cluster | Run the backup schedule; create PITs, bookmarks, and local snapshots and prune them; maintain the PIT catalog; record appliance check-ins; run restores using appliance-issued grants |
| `bmac-source` on each host | ZFS `send`, `hold`, and `release` delegated on guest datasets (a send from a bookmark needs nothing more); the enrolled appliances' certificate fingerprints and addresses | Dial each approved appliance's address, and over that connection answer appliance check-ins (passing them to the local `bmacd`), stream BMAC PIT snapshots, and serve the PIT catalog to enrolled appliances. Nothing else |
| Appliance (`bmac-backupd`) | Its client key; pinned Proxmox cluster CA; its trusted-peer list | Check in; pull the PITs it is offered; write its own pool; serve exports only to restore jobs under its grants and to the peers it trusts; repair only from peers it trusts |
| Appliance administrator | Appliance UI and console credentials | Retention policy, branch retirement, restore grants, the trusted-peer list, unlock, scrubs |

The cluster has **no credential for any appliance** and never opens a
connection to one, except a restore job presenting an appliance-issued grant.
Everything else flows from appliance check-ins. The cluster can only:

- record what each appliance reports when it checks in (published PITs,
  transfers, health, capacity, lock state);
- answer a check-in with the PITs that appliance should pull.

So a cluster compromised as root on every host can stop backups, or offer bad
data for new PITs. It cannot change, delete, or read any PIT the appliance
already published, change retention, or log in to the appliance. Owning the
schedule gives it nothing more: an appliance only ever adds PITs.

A compromised appliance exposes the backup data it already holds (it can read
production snapshots through `bmac-source`). It cannot change anything on the
cluster or on another appliance, and it can read nothing from another
appliance that doesn't trust it (see "Trusted peers").

## Identities

| ID | Meaning |
| --- | --- |
| `cluster_uuid` | The BMAC cluster; kept when a cluster is rebuilt from backup |
| `vm_uuid` | A production resource, independent of VMID and host |
| `volume_uuid` | One VM disk, independent of storage path and bus slot |
| `branch_uuid` | One line of history for a VM; a new branch starts after a rollback |
| `pit_uuid` | One recovery set |
| `appliance_uuid` | One appliance, independent of its address |
| `transfer_uuid` | One pull of one PIT volume, the idempotency key for retries |

Snapshot names are `bmac-pit-<UTCtimestamp>-<6 hex>`, the same name on every
volume of a PIT. A PIT's bookmarks carry the same name (`vol#bmac-pit-…`).
They never collide with staging or Proxmox replication snapshots. Names are
for people; every check uses snapshot GUIDs (a bookmark keeps its snapshot's
GUID).

## Backup policies (cluster)

Each production VM has one backup policy, edited in the Dashboard and stored
in pmxcfs with revision checks:

- **schedule** (default daily 03:00 in the cluster's time zone);
- **appliances**: the enrolled appliances that back this VM up, any number;
  an appliance can back up any number of VMs;
- **local retention** (see "Local retention"), none by default;
- consistency level and hooks (see "Capturing a PIT").

Cluster-wide backup settings:

| Setting | Default |
| --- | --- |
| Check-in interval (told to appliances) | 60 seconds |
| Appliance late after | 3 minutes without a check-in |
| Ready window: an appliance counts as available for a capture | Checked in within the last 5 minutes |
| Claim window: how long a new PIT's snapshot waits for an available appliance to start pulling | 5 minutes |
| Stalled transfer | No bytes for 10 minutes |
| Delinquency period: also how long checkpoint bookmarks are kept | 72 hours |
| Concurrent streams per appliance | 1 |
| Minimum healthy appliances per VM | 1 |
| Automatic appliance removal | Never |

An admin edits them on the Backups page (`POST /api/v1/backups/settings`,
with the revision read; the audit log records each change). A test day
(`test_day_s`, see "Calendar") is a further setting for compressed-time
tests only.

Every alert below has its own on/off switch in the alert settings
(`MONITORING.md`).

## Check-ins

Each appliance calls `POST /v1/checkin` on any connected host's
`bmac-source` once per check-in interval, over a connection the host
opened (see "Connections"). The request is small: appliance
UUID and version, lock state, pool health, capacity and namespace quota, its
published head for each assigned VM (branch, PIT, and snapshot GUIDs per
volume), transfers in progress with bytes so far, and the revision of its PIT
index. The reply lists the PITs offered to it, for each the hosts that hold
the snapshots and the base it should pull from, the pending branch roots of
restores of its VMs (see "Backups continue from the restored PIT"), and
whether the cluster wants its PIT index (sent next with `POST /v1/index`,
metadata only), and the cluster's time zone (see "Calendar").

`bmac-source` hands each check-in to the host's `bmacd` over its local
socket. `bmacd` keeps last-seen times in memory, shares them over the mesh,
and writes pmxcfs only when an appliance's state or index revision changes,
so a check-in a minute from each appliance doesn't churn pmxcfs. Any host
gives the same answer, because offers come from the PIT catalog and claims in
pmxcfs.

Appliance states, as the cluster sees them:

```text
PENDING -> SEEDING -> ACTIVE <-> LATE -> DELINQUENT -> REPAIRING -> ACTIVE
                                                    \-> RESEED_REQUIRED -> SEEDING
any -> PAUSED (operator)      any -> RETIRED (operator; data stays on the appliance)
```

- **ACTIVE**: checked in within the late threshold.
- **LATE**: no check-in for 3 minutes. Alert "appliance not checking in".
- **DELINQUENT**: no check-in, or no PIT published of those offered to it,
  for the delinquency period. Its checkpoint bookmarks are released (see
  below). Alert "appliance delinquent".
- **LOCKED** (the pool waits for the console passphrase) is reported from the
  check-in, separately from the states above, and never counts as removal.

## Capturing a PIT (cluster)

PITs are taken by the VM's scheduled job, admitted like any job
(`ARCHITECTURE.md`), with the idempotency key derived from the schedule slot.
"Back up now" in the Dashboard runs the same job outside the schedule.

0. **Is anyone there?** If none of the VM's appliances is available (checked
   in within the ready window), don't take the snapshot, unless local
   retention claims this slot. Raise the alert "backup skipped: no appliance
   available" for the VM. The next slot tries again. **Is the last one
   still moving?** If any of the VM's appliances is still pulling one of
   its PITs, and that transfer isn't stalled (it moved within the stalled
   transfer period), skip the slot: no snapshot, no alert, and a line in
   the job's log naming the appliance, the PIT and the bytes so far. A
   long first seed thus never piles up PITs no one can take. A stalled
   transfer doesn't stop the slot.
1. Take the VM's resource intent in the registry. This blocks disk growth,
   placement and owner changes, staging creation, and removal for the
   duration.
2. Resolve the authoritative HA owner live. The job runs on that host. Never
   snapshot a replica.
3. Read the VM configuration and its disk list, and record a digest.
4. Freeze guest filesystems through the QEMU guest agent, if the policy asks
   for it, with a watchdog that thaws on error, timeout, cancellation, or
   restart. If the policy requires the freeze and it fails, the PIT fails.
   Optional operator hooks (from configured paths only, with timeouts) give
   application consistency.
5. Snapshot every required zvol in **one** `zfs snapshot a@pit b@pit ...`
   command, which is atomic within a pool. Disks spread over several pools
   are refused unless the policy accepts the weaker guarantee, which is then
   recorded in the manifest.
6. Thaw.
7. Read the configuration again. If it changed, or a disk is missing, mark
   the PIT failed and remove its snapshots.
8. Run replication now (`pvesr schedule-now`, retried while locked) so the
   snapshots reach every placement host, and check their GUIDs there.
9. Write the manifest to the PIT catalog and offer the PIT to the VM's
   available appliances.

The consistency level (`crash`, `filesystem`, `application`) is recorded.
`filesystem` is the default when the guest agent answers; `crash` must be
chosen explicitly.

**PIT catalog.** Manifests live in pmxcfs at
`/etc/pve/priv/bmac/backup/pits/<vm_uuid>/<pit_uuid>.json`. The catalog
holds the PITs that still have a snapshot or a bookmark on the cluster, which
the delinquency period and local retention bound (for example 20 VMs × 6 PITs
× about 4 KiB, under 1 MiB). Any host's `bmac-source` can serve it, and local
restores can use it. Appliances keep their own copy of every manifest they
publish.

The manifest's parent PIT never changes. The catalog record's and an
appliance's journal entry's parent are the nearest ancestor still in that
store: when a PIT leaves (its snapshot and bookmarks gone from the cluster,
or pruned by an appliance), its children take its parent, and an appliance
that never received the cluster's parent records the offer's base instead.
The walks that find an incremental base, for an offer or a restore, then
reach every remaining ancestor.

**Manifest** (versioned JSON schema, canonical serialization, SHA-256 over the
payload without the hash field):

- schema version, `cluster_uuid`, `vm_uuid`, VMID at capture, `pit_uuid`,
  `branch_uuid`, parent PIT, capture time (UTC);
- consistency level and evidence (guest agent response, hook results);
- Proxmox and OpenZFS versions;
- the raw VM configuration (`/etc/pve/qemu-server/<vmid>.conf` text, current
  settings only, not pending ones), its parsed form, and its SHA-256;
- per volume: `volume_uuid`, bus slot, source dataset, snapshot name and GUID,
  `volsize`, `volblocksize`, whether it is needed to boot;
- EFI vars and TPM state volumes, machine type, Secure Boot settings;
- the BMAC registry record for the resource (IP, MAC, domains, placement,
  routes);
- external prerequisites the restore cannot recreate.

**Snapshots reach the replicas.** Proxmox storage replication copies BMAC's
snapshots to every placement host, as v2's staging workflow already relies
on. If the VM fails over, the new owner has the same snapshots, and the
bookmarks made from them.

## Pulling a PIT (appliance)

The appliance pulls only what its check-in reply offers. For each offer:

1. **Claim** it in the next check-in (or at once with `POST /v1/claim`), so
   the cluster keeps the snapshot while it pulls.
2. Find a host that has the new snapshot and the base. Any placement host
   works, owner or replica. Pulling from a replica keeps load off the
   production owner. `bmac-source` serves a send only for snapshots and
   bookmarks that exist locally with the requested GUIDs.
3. For each volume, request an incremental stream from the base to the new
   snapshot, or a full stream for the first backup of a branch. The base is
   the appliance's head, as a snapshot or a bookmark on the cluster (`zfs
   send -i vol#base vol@new` works the same from either). Large-block and
   compressed send flags are used only when both sides support them. The
   stream is a TLS 1.3 HTTP/2 response body; `bmac-source` places a hold
   named for the transfer on the new snapshot while it streams.
4. Receive into the volume's zvol with `zfs receive -s` (resumable), never
   `-F`, never a `-R` stream, with `volmode=none` so received zvols never
   appear as block devices, and with stream properties restricted to a fixed
   allowlist. A full receive goes only into a dataset that doesn't exist yet;
   an incremental only onto the expected base GUID. `zfs receive` itself
   rejects a stream whose base doesn't match.
5. Check every received snapshot's GUID, `volsize`, and `volblocksize` against
   the manifest.
6. **Publish** (two phases): write the manifest to the appliance's metadata
   dataset, fsync, and atomically mark the PIT published in the appliance's
   journal. Only now is the PIT usable for restore or counted by retention.
   A PIT missing any volume is never published. After a crash between
   receiving and publishing, the appliance finishes or quarantines it.
7. Report it published in the next check-in. The cluster's view of what each
   appliance holds always comes from the appliance's reports and is never
   assumed.

When the appliance is several PITs behind, it pulls one incremental from its
head straight to the newest PIT. Intermediate PITs are not replayed. If a pull
is interrupted, the next attempt resumes with the saved receive token
(`zfs send -t`), checked against the same transfer, as long as the new
snapshot still exists on the cluster; otherwise the appliance aborts the
partial receive (`zfs receive -A`) and pulls the next PIT from its head.

Because the appliance builds its own `zfs receive` command and chooses its own
target datasets, the cluster can't steer a write anywhere. It can only offer a
stream for the dataset the appliance chose. The hardened-receiver problem in
the earlier spec mostly disappears; what remains is that stream contents are
untrusted data, which the property allowlist and `volmode=none` handle.

### Transfer throughput

`bmac-source` counts the bytes of every stream it sends and samples the rate
every 10 seconds into the metric history (`MONITORING.md`), per appliance and
VM, so the Dashboard shows a live rate during a transfer and a history chart
to spot a network bottleneck. When a transfer ends, the cluster logs one
event with the total bytes, the duration, and the average rate (total bytes
divided by total time), and the appliance logs the same from its side. The
cluster also keeps the newest 200 finished transfers per host
(`backup/transfers/<host>.json`). The Backups page shows the live rate of
each transfer in progress, and the transfer history with each one's bytes,
duration, and average rate; restores use the rate for their time estimate.

### Appliance pool layout

```text
<pool>/bmac/                                  filesystem
<pool>/bmac/metadata/                         manifests, journal, audit, recovery manifests
<pool>/bmac/clusters/<cluster_uuid>/          namespace with quota
<pool>/bmac/clusters/<cluster_uuid>/vms/<vm_uuid>/<lineage>/<volume_uuid>   zvols
```

A lineage is `<branch_uuid>`, or `<branch_uuid>-<n>` once a full send
reseeds the branch (its old zvols can't take the new stream's snapshots,
and are pruned as their PITs expire). `bmac-backupd init` creates `bmac`
with compression on and deduplication off, and mounts `metadata` at
`/var/lib/bmac-backupd/metadata`; the service never mounts it itself, since
its own mount namespace would hide the mount from everyone else.

Each received zvol accumulates one snapshot per published PIT. A new branch's
zvols are ZFS clones of the branch point's snapshots, so a branch stores only
what changed after it (see "Branches"). Compression is on; deduplication is
off. Each cluster namespace has a quota, and every pull checks pool headroom
before it starts.

## Snapshots and bookmarks on the cluster

A PIT's snapshots exist on the cluster only as long as something needs the
data in them. Its bookmarks, which cost almost no space, carry the
incremental chain from then on.

**What a bookmark is.** A ZFS bookmark records a snapshot's GUID and the
transaction in which it was taken; it holds no data. An incremental send
from a bookmark to a newer snapshot sends every block written after that
transaction, exactly as it would from the snapshot, so the appliance (which
keeps the snapshot) can apply it. A bookmark can't be rolled back to, cloned,
or read, and `zfs receive` can't apply a stream onto one, so bookmarks don't
help restores; only snapshots do.

**What a snapshot costs.** ZFS writes copy-on-write whether or not a
snapshot exists. A snapshot keeps every block that has been overwritten or
freed since it was taken, so its cost is space that grows with the VM's write
churn, plus some fragmentation, not slower writes.

**Lifetime of a PIT's snapshots:**

1. After capture, the snapshots are kept while any appliance that was
   available at capture time hasn't yet published the PIT, failed it, or let
   the claim window pass without claiming it. A claimed transfer keeps them
   until it finishes, fails, or stalls.
2. Then the cluster bookmarks every volume on **every placement host**, from
   that host's own copy of the snapshot (same GUID), and checks each
   bookmark's GUID. Only then are the snapshots destroyed on the owner, unless
   local retention keeps them; replicas follow through replication.
3. A bookmark is kept while it is the head of an assigned appliance that
   isn't delinquent, or while it is the VM's newest PIT. Others are removed.

An appliance that misses PITs pulls, when it returns, one incremental from
its head's bookmark to the next PIT the cluster takes (or a "Back up now").
Once it is delinquent its bookmarks are gone; it repairs from a peer
appliance, or reseeds (see "Repairing an appliance").

`bmac-source` serves an incremental from a bookmark only when the catalog
shows the bookmark's PIT as an ancestor of the new PIT on the same branch.
ZFS itself refuses a rollback past a bookmark unless the rollback removes it,
so an in-place rollback also removes the bookmarks of the abandoned branch.

**Until the lab proves bookmarks** (items 2 and 3 of "To prove in the lab"),
the cluster keeps the appliance heads' snapshots instead of bookmarks, on the
same rules, and shows the extra space this costs on the Backups page.

**Source retention.** A BMAC snapshot on the cluster is deleted only when none
of these still needs it:

- local retention;
- an open claim window or a transfer in progress;
- a staging clone origin (v2's snapshot dependency records);
- a local restore in progress;
- (until bookmarks are proven) an appliance head within the delinquency
  period.

BMAC snapshots and bookmarks are only ever created and deleted by `bmacd`;
snapshots on the owner only, bookmarks on every placement host.

## Local retention and fast recovery

Local retention keeps some PITs' snapshots on the cluster after the
appliances have them. **The default is none**: the appliances keep the
history. Per policy, an administrator can keep:

- the newest N backed-up PITs;
- one PIT for each of the last D days;
- one PIT for each of the last W weeks.

Days and weeks are the cluster's (see "Calendar"), counted from the day of
the VM's first PIT on the cluster; the newest PIT of each wins it.

The setting explains the trade-off. Keeping a PIT's snapshot lets you clone
or roll back to it in seconds, and lets a restore of a nearby PIT move only
the difference. But each kept snapshot holds on to every block the VM has
overwritten since, so it costs space that grows with how much the VM writes.
Without kept snapshots, the cluster keeps only bookmarks, which cost almost
nothing, and every restore comes from an appliance.

A slot that local retention claims is captured even when no appliance is
available (step 0).

**Clone to a new VM** (preferred). Clone every disk of one local PIT into new
zvols, allocate a new VMID and identity through the registry, attach the
disks to an isolated VM (no production routes, disconnected or isolated
network, new MAC), and boot it. This reuses v2's staging identity-patching.
Production is untouched.

**Rollback in place** (destructive, admin only, separate typed confirmation):

1. stop and fence the VM;
2. check that every disk and dependency allows it;
3. roll back every disk to the PIT;
4. start a new branch (see "Branches").

Never `zfs rollback -R`. If clones depend on newer snapshots, refuse and offer
clone-and-promote instead. Multi-disk rollback is a recorded transaction; if
any disk fails, the VM stays stopped and the job reports exactly which state
each disk is in.

## Branches

If production history is rolled back to `P3` after `P5` was backed up, later
PITs `P6`, `P7` form a new branch whose common ancestor is `P3`:

```text
branch A:  P1 -- P2 -- P3 -- P4 -- P5
                        \
branch B:                P6 -- P7
```

1. Before the rollback, record the old branch's head on every appliance.
2. Pause this VM's backup schedule, create `branch_uuid` B with parent PIT
   `P3`, and mark branch A inactive (never deleted).
3. Each appliance receives the incremental `P3 → P6` **as a clone of its own
   `@P3`** (`zfs receive -o origin=<branch A zvol>@P3` into branch B's new
   zvol). Branch B shares every block of `P3` and stores only what changed
   after it. Only if the lab disproves this (item 3), the appliance receives a
   full `P6` instead. Branch A's zvols and PITs are never received into.
4. `P6` is published on branch B with parent `P3`. Later PITs are normal
   incrementals on B. Appliance retention keeps `P3` as long as branch B
   depends on it.
5. Retiring branch A is an appliance-admin action that checks for dependent
   clones and needed origins (`zfs promote` where required, run locally by the
   appliance).

## PIT graph

The Dashboard's Backups page and the appliance UI draw each VM's history as a
tree. Time runs top to bottom, the oldest PIT is always at the top left, and
each branch is a column to the right of the one it forked from, joined to
its parent PIT.
Each PIT shows its time and consistency level, and marks where it exists: on
which appliances, and whether the cluster still has its snapshot or only its
bookmark. Selecting a PIT shows its manifest summary and the restores it
allows. The Dashboard builds the graph from the PIT index each appliance
sends; the appliance UI from its own journal.

## Multiple appliances

Each appliance is independent: its own checkpoint, history, retention, and
state. One appliance failing never stops the others, or the schedule. Because
the cluster owns the schedule, every appliance assigned to a VM receives the
same PITs, and two appliances can never ask for snapshots of the same VM at
different moments. Appliances share nothing with each other unless their
administrators make them trusted peers.

## Repairing an appliance that fell behind

When an appliance returns, it tries, in order:

1. **From the cluster.** Its head's bookmark (or snapshot) is still on the
   placement hosts: pull one incremental to the next PIT offered.
2. **From a trusted peer appliance.** Ask the peers it trusts, that also
   trust it (see "Trusted peers"), for a published PIT that descends from its
   head on the same branch and is part of one complete recovery set. Pull
   `zfs send -i head new` from the peer. Received snapshots keep their GUIDs,
   so this works, and afterwards the appliance continues from the cluster's
   bookmark of that PIT if it still exists.
3. **Reseed.** Start a new lineage in new zvols from a full pull of the newest
   PIT. The old zvols and PITs stay until the appliance's own retention removes
   them.

PITs are never mixed across branches or across recovery sets. If the
appliance can't prove ancestry by GUID, it reseeds.

A healthy appliance may keep bookmarks for trusted peers known to be behind,
within a bounded age and count, so a later peer repair stays possible after
normal pruning. It keeps none for a peer it doesn't trust.

### Trusted peers

Each appliance decides for itself which other appliances are its trusted
peers. Trust is all or nothing: a trusted peer can read every published PIT
on the appliance, and an appliance that isn't trusted can't reach it at
all. Only peers that trust each other repair from each other.

- The trusted-peer list lives on the appliance, in its metadata dataset, and
  only that appliance's administrator changes it, on the **Trusted peers**
  page of the appliance UI. The cluster can't add to it (it has no
  credential for any appliance), and neither can another appliance.
- An entry is the peer's `appliance_uuid`, its certificate fingerprint
  (SHA-256), and its address (see "Appliance addresses"), nothing more. An
  appliance uses one certificate as client and server on 8445, so one
  fingerprint identifies it both ways. The fingerprint is never learned from the peer, or from the
  cluster, on first use.
- Trust goes one way. A serves B only if A trusts B, and B repairs from A
  only if B trusts A, so B never receives data from an appliance its
  administrator didn't choose. A repair needs both, set separately by each
  administrator.
- The appliance's export listener (8445) accepts a client certificate only
  if it belongs to a trusted peer, or to a cluster host (presenting a
  restore grant, or opening its `bmac-source/1` connection). Any other appliance is refused during the TLS handshake, before a
  request is read: it gets no exports, no index of published PITs, no size
  estimates, and no error detail beyond the refusal. The refusal is written
  to the audit log, at most once per peer per hour.
- A trusted peer can only read published PITs, by `GET /v1/peer-index` and
  `POST /v1/export`. It can't prune, hold, publish, or change anything, and
  it can't see unpublished receives or the appliance's settings.
- Untrusting a peer takes effect at once: the next connection is refused and
  open streams from it are closed. Bookmarks kept for it are released at the
  next prune.
- Trust between appliances is separate from enrollment with the cluster. An
  appliance revoked by the cluster stays a trusted peer until each
  administrator untrusts it; an enrolled appliance is never trusted by
  another one until its administrator adds it.

The Trusted peers page:

- shows this appliance's own `appliance_uuid` and fingerprint, to read out
  or copy to the other appliance's administrator;
- lists each trusted peer, when it last connected, and whether it trusts
  this appliance back. An appliance can't ask for the other's list:
  "doesn't trust this appliance" means that peer refused this appliance's
  last handshake;
- **Trust a peer**: the administrator enters the peer's address and the
  fingerprint shown on the peer's own UI. The appliance connects and saves
  the entry only if the certificate presented matches the fingerprint
  typed; on a mismatch it warns and saves nothing. Even on a match, the page
  reminds the administrator that the match only proves anything if the
  fingerprint came from the peer's own UI or its administrator, not from a
  message that could have been altered;
- **Untrust** a peer, after a confirmation naming it;
- lists recent refusals (the fingerprint and the `appliance_uuid` it
  claimed, the source address, and when). A refusal can't be turned into
  trust with one click; trusting that appliance still takes its fingerprint
  from its own UI.

Each trust or untrust needs the administrator's password again, and goes to
the audit log.

### Appliance addresses

Where an appliance's 8445 answers, for the cluster's hosts (the
appliance's record in the cluster's backup settings) and for its trusted
peers (each entry on the Trusted peers page), is an IP address or a DNS
name, with an optional port (8445 by default): a private address on the
hosts' network such as `10.10.0.50`, a public one such as `203.0.113.5`
or `[2001:db8::1]:9445`, `backup.example.org`, or a dynamic DNS name such
as `vault.example.dyndns.org:9445` for an appliance on a changing address.
An enrolling appliance may suggest its address, and the administrator can
change it when approving and at any time after. An approved appliance
without an address gets no backups, since no host can reach it.

Any address the hosts can reach will do, a Tailscale one included; BMAC
doesn't refuse it. A Tailscale address carries backup data through
Tailscale, though, and over its relay whenever a direct path fails, at a
fraction of the network's speed, so a private or public address is the
usual choice. The lab never gives an appliance a Tailscale address: its
appliances use their addresses on the hosts' VLAN, and the scenarios check
that every backup connection comes from a host's VLAN address.

### Connections

The appliance never connects to the cluster. Each host's `bmac-source`
dials every approved appliance that has an address and keeps one
connection open, reconnecting with backoff: TLS 1.3 with mutual
authentication, the host presenting its certificate from the cluster CA
and checking the appliance's fingerprint, with the ALPN protocol
`bmac-source/1`. Over that connection the roles turn around: the appliance
is the HTTP/2 client and makes the requests in "Protocol" (check-ins,
claims, sends), and `bmac-source` answers them as it would any client.
The appliance takes the host's name from its certificate, admitted only
under a cluster CA it enrolled with, and sends each request on a
connection from a host that can answer it. Hosts thus listen on no
address for backups, and an appliance on the hosts' network or across the
internet is reached the same way.

A name is resolved again on every connection, so a dynamic DNS update is
followed without any change in BMAC. The name is never the appliance's
identity: every connection checks the certificate fingerprint recorded at
enrollment or typed on the Trusted peers page, so whoever controls the DNS
record can make connections fail, but can't stand in for the appliance or
receive a grant's data.

## Appliance retention

Retention runs only on the appliance, under its own policy, which only the
appliance administrator can change (on the appliance UI).

A VM's history on the appliance reads like a calendar: every PIT of today,
one for each earlier day of the last week, one for each earlier week, one
for each earlier month, and one for each earlier year. The policy:

| Setting | Default | Keeps |
| --- | --- | --- |
| Daily | 7 | The newest PIT of each of the 7 days before today |
| Weekly | 4 | The newest PIT of each of the 4 weeks before this week |
| Month length | 30 days | How many days make a month |
| Monthly | 12 | The newest PIT of each of the 12 months before this month |
| Yearly | On | The newest PIT of every year (12 months) before this year; off keeps every month's newest PIT forever instead, and the monthly count is unused |

Every PIT taken today is kept, and so is the latest published PIT.

- Periods follow the calendar of the PIT's cluster (see "Calendar"). Days
  are its calendar days. Weeks, months and years count whole days from the
  VM's calendar start, the day of the first PIT of the VM the appliance
  published. The appliance records that day itself and never moves it, so
  the cluster can't shift the history's periods.
- The newest PIT of a period wins it. Each tier also keeps the current
  period's winner, which while backups run is today's newest PIT, so it
  costs nothing then; after backups stop, it keeps the last PIT of the
  current week, month and year. Since a week's, month's or year's newest
  PIT is also the newest of its last day, a PIT kept as a daily stays kept
  as it ages into a weekly, monthly or yearly, and a PIT once pruned would
  never have been kept again.
- The keep set is the union of: period winners, each branch's head, branch
  roots and common ancestors (including pending branch roots of restores),
  PITs named in live restore grants, clone origins, administrator holds,
  repair bookmarks kept for trusted peers, and transfers in progress.
- Every prune is planned first: PITs to delete, why each other PIT is kept,
  estimated space freed (often less than expected, since blocks are shared),
  and the restore coverage that remains. Live ZFS state is read again under
  the retention lock immediately before deleting.
- Published PITs carry ZFS holds owned by the retention service, which
  releases them only as part of an approved prune.
- Weakening retention, or retiring a branch, takes effect after a delay
  (default 72 hours), shown on the appliance UI and reported to the cluster,
  unless confirmed again at the appliance console. A configurable minimum
  retention floor can't be lowered from the UI. Weakening is a lower count,
  a different month length, or turning yearly on; a cluster reporting a new
  time zone counts as weakening too, so the new zone applies only after
  the delay.

### Calendar

The cluster's time zone is chosen when its first host is deployed (the
Deployer offers the workstation's zone) and every host installed after
it, by join or by recovery, takes the same one. Schedules run, and the
Dashboard shows them, in that zone. Each check-in reply carries the zone;
the appliance applies each cluster's retention in that cluster's zone and
shows the cluster's PITs and schedules in it, and its own events (audit
log, scrubs) in its system zone, set at its console.

For tests only, a day can be given a length in seconds: on the cluster a
backup setting, for local retention; on the appliance, only at its
console (`bmac-backupd test-day-seconds`). A compressed day starts every
multiple of its length since the Unix epoch, and weeks, months and years
are counted in those days as above.

## Restore

**Restore grants.** Reading historical data needs the appliance
administrator's approval. On the appliance UI the administrator selects PITs
and issues a grant: a random token bound to those PITs, the requesting
cluster, and an expiry (default 24 hours). It is shown once and pasted into
the cluster Dashboard's restore wizard. The cluster's restore job uses it to
pull the PIT's streams from the appliance's export endpoint, at the
appliance's address (see "Appliance addresses").

**Moving only what's needed.** Every restore uses the cheapest method that the
snapshots on hand allow, per volume, in this order:

1. **The PIT is still on the cluster** (local retention): clone it, or roll
   back to it. Nothing crosses the network.
2. **An older PIT on the same branch is on the cluster and on the
   appliance** (matching GUIDs): clone that local snapshot and apply the
   appliance's incremental from it to the PIT (`zfs receive -o origin=` for a
   new VM, or onto the rolled-back volume in place). Only the change between
   the two PITs is sent and written.
3. **Otherwise a full stream** of the PIT from the appliance. Only allocated
   blocks are sent (holes in a sparse zvol aren't), and compressed blocks stay
   compressed when both sides support it.

Every method ends with the volume at a snapshot whose GUID is the PIT's:
`zfs receive` checks each stream's checksums and its base GUID, and the job
then checks sizes against the manifest, so the faster methods give the same
result as the full one. Because the restored volume descends from the PIT,
the next backup is an incremental from it (see "Backups continue from the
restored PIT").

Before it starts, the wizard shows the method chosen for each volume and the
bytes involved: the stream size (`zfs send -nP` on the appliance, exact for
full and incremental streams), the bytes to be written, and an estimated
time from this appliance's measured throughput, in human units (for example
"412 GiB to transfer, about 1 h 50 min"). A full restore says plainly that it
may take a long time.

Not done: comparing the current disk block by block with the PIT and sending
only the differences. It would read both volumes in full, and the result
would carry no snapshot of the PIT, so the next backup of a promoted VM would
be a full send. For the same reason a restore never copies blocks into a
volume that already exists (`dd`, `qemu-img convert`, `qmrestore`): ZFS
would see no ancestry, and the next backup would be a full send.

### How a restored volume is made

A full `zfs receive` creates its dataset itself, with the PIT's snapshot and
that snapshot's GUID, so a restored volume never needs a blank zvol
created first. Method 2's clone and incremental also end at a snapshot of
its own with the PIT's GUID. Method 1's clone owns no snapshot; it descends
from the cluster snapshot it was cloned from, which is enough for an
isolated restore or a restore test. Proxmox replication can't send a clone
at all (its stream needs the origin on the target: "local origin for clone
does not exist"), so a method 1 volume that is to be backed up is first
made to own the snapshot. When the VM it came from is stopped and set aside
(promotion, restore into the existing VM), `zfs promote` moves the snapshot
to it, keeping the GUID, and the old disk becomes the clone. A PIT whose
snapshot survives only on a disk set aside by an earlier restore gives a
clone of a clone: it is promoted once per level, over BMAC's set-aside and
staging datasets only, until it has no origin. A clone of another live
disk is never promoted over it, since that would take the other disk's
history; it is copied locally with `zfs send | zfs receive`, which also
keeps the GUID and leaves that disk untouched. A replication job that fails
three times while the job waits for its first sync stops the wait with
Proxmox's error. On the cluster:

1. Each volume is received (or cloned) under a staging name,
   `<pool>/bmac-restore/<job_id>/<volume_uuid>`, with `volmode=none`, so
   Proxmox and udev never see it half-written. The receive uses `-s` for
   resume, and never `-F` or a `-R` stream. It never goes into an existing
   dataset, and it takes the same property allowlist as the appliance's
   receive. The appliance's export sends without `-p`, so its own
   properties (`volmode=none`, quotas) don't come along.
2. After the GUID and size checks, `zfs rename` moves it to the Proxmox
   storage whose dataset matches its `volblocksize` (`STORAGE.md`, `local-zfs`
   for 16 KiB, `bmac-zfs-64k` for 64 KiB, and so on), under the name
   Proxmox expects (`vm-<vmid>-disk-<n>`). `volmode` is then inherited. A
   rename in the same pool keeps every snapshot and GUID. A `volblocksize`
   with no matching storage on the target is refused in the preflight,
   because a zvol's block size can't change.
3. The VM is created with `qm create` and no disks, so Proxmox never
   allocates one. Each restored zvol is then attached with `qm set` at its
   bus slot.

### Backups continue from the restored PIT

When a restored volume becomes a disk that is backed up, its history
continues from the restored PIT instead of starting again. This applies after
a promotion, a restore into the existing VM, or when the restored VM gets a
backup policy of its own.

1. **A new branch.** The VM's backups start a branch (see "Branches") whose
   parent is the restored PIT, recorded per volume by GUID. When the restored
   VM isn't the VM the PIT came from (it keeps its own identity and gets a
   policy), the branch's parent names the original VM and PIT: a branch can
   descend from another VM's PIT in the same cluster namespace.
2. **The base on the cluster.** The restored volume's PIT snapshot is the
   base for the first PIT of the branch. It follows the same lifetime as any
   PIT's snapshots ("Snapshots and bookmarks on the cluster"). Replication
   carries it to every placement host with the volume's first sync (lab
   item 1). It is then bookmarked on every placement host, the bookmarks'
   GUIDs are checked, and it is destroyed unless local retention, a staging
   clone, or a restore still needs it. Only the bookmark is needed to send
   the next incremental, so the snapshot doesn't have to wait for the next
   PIT. The bookmark is kept until every assigned appliance has published the
   branch's first PIT or become delinquent. Until bookmarks are proven (lab
   item 2), the snapshot is kept instead, on the same terms.
3. **The first PIT of the branch** is sent as an incremental from that base
   (`zfs send -i vol#restored vol@new`). Each appliance that holds the
   restored PIT (matching GUIDs, whichever appliance the restore came from)
   receives it as a clone of its own copy, as for any new branch, so it
   stores only what changed since the restore. An appliance that doesn't
   hold it (pruned, or never assigned the original VM) gets a full send for
   that branch.
4. **Keeping the parent on the appliances.** No appliance can know in
   advance which PIT will become a branch point, so the cluster tells them.
   A PIT named in a live restore grant is held by the appliance that issued
   it. Once a restore job has used it, the cluster's check-in replies name it
   as a pending branch root to every appliance assigned to the VM, and each
   one that holds it keeps it (for up to 30 days) until the branch is
   reported or the restore is abandoned. After that it is kept as a branch
   root, which is already in the retention keep set. If an appliance prunes
   the PIT before it learns of the restore, that appliance gets a full send
   for the branch.

The restore wizard shows which appliances will continue incrementally and
which will need a full send, with the bytes involved.

**Restore to a new VM:**

1. Choose appliance, VM, branch, and PIT. Verify the published manifest lists
   every required disk.
2. Preflight the target host: architecture, memory, pool space (with the
   capacity function from `STORAGE.md`), OVMF/TPM, bridges, and a storage
   for each volume's `volblocksize`.
3. Allocate a new VMID and new dataset names through the registry.
4. Bring each volume to the PIT by the cheapest method above, with resume,
   as "How a restored volume is made" says. Verify GUIDs, sizes, and manifest
   completeness.
5. Recreate the VM from the backed-up configuration by translating it field
   by field through `qm create` and `qm set`, never by copying the file into
   pmxcfs. Attach the restored disks at their bus slots on local storage.
   Restore EFI and TPM state.
6. Keep it isolated: new MAC, no routes, isolated or disconnected network.
7. Boot it, then check the guest agent and any configured application checks.

**Restore into the existing VM** (admin, typed confirmation) replaces the
VM's disks with restored ones and never writes over the current ones. Its
zvols may carry snapshots and bookmarks that replication, staging clones, and
other appliances' heads depend on, so they are set aside, not received into
or rolled back.

1. Bring each volume to the PIT under a staging name, as for a new VM. For
   method 2 the local base can be any older snapshot of the VM's current
   disk on the same branch, so the restore sends only what changed between
   it and the PIT.
2. Stop and fence the VM, and pause its HA resource and replication through
   a registry transaction.
3. Rename each current zvol to `<pool>/bmac-replaced/<vmid>/<volume_uuid>-<time>`
   with every snapshot and bookmark it has. Staging clones of it keep
   working, and a method 2 clone that descends from it keeps its origin.
   Then rename the restored zvol to the disk's Proxmox name, and record
   both renames in the transaction. If any step fails, the job renames back
   what it moved, leaves the VM stopped, and reports each disk's state.
4. Re-enable replication and wait for the replicas to converge. A replaced
   disk has no replication snapshot in common with its replicas, so this is
   a full copy to each placement host over the private network. The
   wizard's estimate includes it.
5. Re-enable HA, start the VM, and start a new branch from the restored PIT.
   The old branch is marked inactive (never deleted).

The replaced zvols stay until an explicit cleanup (`backup.cleanup`, per
host, choosing among the set-aside disks), which refuses while a
staging clone still depends on them, and promotes (`zfs promote`) a restored
volume that is still a clone of one of their snapshots before destroying
them. Rollback in place ("Local retention and fast recovery")
remains the fastest way back when the PIT itself is still on the cluster and
nothing depends on the newer snapshots.

**Promotion** (admin, typed confirmation) makes a restored VM production:

1. fence and stop the former production VM;
2. pause its HA resource and replication through a registry transaction;
3. assign the production identity (registry record, IP, MAC, routes) to the
   restored VM;
4. make each restored volume own its PIT snapshot ("How a restored volume is
   made"), then recreate replication to every placement host and wait until
   the replicas converge;
5. re-enable HA;
6. start a new backup branch from the restored PIT ("Backups continue from
   the restored PIT").

The former VM's disks are kept until an explicit cleanup. Promotion refuses
to run without quorum, and guards against the old owner returning midway.

**Restore tests.** A manual workflow from the Backups page:

1. restore a chosen (or the newest) PIT into an isolated scratch VM;
2. boot it and run the checks;
3. record the result (PIT, time, boot result, agent result, application
   checks);
4. destroy only the scratch VM.

The results are stored in the cluster and reported to the appliance: every
check-in reply carries the newest 20 finished or failed tests that read from
that appliance, and the appliance keeps the newest 50 per cluster
(`recovery/<cluster_uuid>/restore-tests.json`) and shows them on its
Recovery page. Backup
publication, scrub health, and restore-test success are three separate
signals and are never merged into one "OK".

## Recovering BMAC after losing the whole cluster

Each appliance pulls a **cluster recovery manifest** every hour, and as
soon as it changes: every check-in reply carries the manifest's revision, a
SHA-256 over its files. The manifest is the text of BMAC's files under
`/etc/pve/priv/bmac/` in `backup/`, `config/`, `registry/`, and
`schedules/`: the configuration and host facts, the registry, the backup
policies and appliances, and the schedules. Nothing else in that directory
is read (secrets, sessions, enrollment tokens, keys), hidden files (pmxcfs
write temporaries) and anything that isn't UTF-8 are left out, and a file
over 1 MiB or a manifest over 64 MiB is refused. The appliance keeps the
newest 48 versions and one a day for 90 days, in its metadata dataset under
`recovery/<cluster_uuid>/`.

To recover onto new hardware:

1. Install new hosts with the Deployer, as a new cluster.
2. In the new cluster's Dashboard, start **Recover from an appliance**
   (`backup.recover`) on the host the VMs are to come back on. It refuses
   if this cluster already has backup policies, and shows this host's
   certificate fingerprint (SHA-256 of its `pve-ssl.pem`).
3. On an appliance's UI, Grants, then Grant a recovery: the administrator
   picks the lost cluster and types that fingerprint. The **recovery grant**
   is a random token, shown once, bound to the namespace, that one host
   certificate, and an expiry. The appliance's 8445 listener admits that
   certificate in the handshake while the grant is live, as it admits a
   trusted peer.
4. The operator gives the job the appliance's address, its certificate
   fingerprint (pinned for the connection), and the grant. As with any
   fingerprint BMAC asks for, the job also tells the operator to compare
   the appliance's fingerprint in the browser, since the job's own
   connection being genuine doesn't prove the browser's is.
5. `POST /v1/recovery/index` answers with the newest recovery manifest and
   every published PIT of the namespace. The operator picks the VMs (each
   one's newest PIT is offered) and whether to take over the configuration.
   A preflight checks each VM's VMID is free here and plans each volume as
   for a restore. A typed confirmation (the lost cluster's name) lists what
   changes and the manual prerequisites.
6. Taking over the configuration: the new cluster takes the old
   `cluster_uuid`, so backups continue on the same namespace and history,
   and the cluster-wide files are written back: the registry's policy,
   resources, history, and desired ingress with its generations;
   `config/cluster.json` (not the per-host files, which the Deployer wrote
   for the new hosts); every schedule; and the backup settings, policies,
   and appliances. Leases, live state, and the PIT catalog are not: the new
   hosts' snapshots are what they are. Then `POST /v1/recovery/bind` makes
   the appliance pin the new cluster's CA and reach its hosts in place of
   the old ones, which it audits; its agent for that cluster starts again
   with the new trust. Any other appliance of the lost cluster needs its own
   recovery grant and binding. The bound host stays named by its grant
   until the grant ends; its backup connection, which its certificate
   proves is the bound cluster's, is admitted as that cluster's.
7. Each VM is restored on this host under its own VMID and name (there is
   no production VM to promote over), keeping its MAC addresses and SMBIOS
   UUID, which the registry and its network know it by (only its
   generation ID is new), replication to its other placement
   hosts in the registry is recreated, it is added to HA, which starts it,
   and its backups continue from the PIT on a new branch. A placement host
   the new cluster doesn't have yet (it was rebuilt with fewer hosts) gets
   no replication job, and the job warns about each one. New host
   identities and certificates are issued normally.

This needs no surviving host, source snapshot, or old Dashboard; only the
appliance, its administrator's credentials, and new hardware. Anything
external (Cloudflare, DNS, provider networking, secrets not backed up) is
listed by the recovery job as a manual prerequisite. Without taking over
the configuration, the VMs still come back, with new backup histories.

LUKS header backups and cluster recovery manifests are stored on every
appliance, so a host can be recovered even if its LUKS headers are damaged.
The appliance pulls them, as it pulls everything: the manifest from any host,
and each host's headers from that host's `bmac-source`, on the same hourly
schedule. `GET /v1/luks-headers` lists each header file's SHA-256 and size;
the appliance then fetches only the files whose digest it doesn't hold, one
per `POST /v1/luks-header` (gzip, at most 20 MiB uncompressed), checks the
digest, and keeps the newest 10 versions of each. Its Recovery page lists,
per cluster, the manifests kept, each host's header files with their newest
time and version count, and the reported restore tests.

## Scrubs and integrity

- Each appliance scrubs its pool on a schedule (default every 30 days,
  configurable) and reports start, progress, finish, repaired and
  unrepairable errors, and affected datasets where ZFS can name them.
- An unrepairable error marks the affected PITs suspect, raises an alert, and
  points to other appliances that hold the same PITs.
- Scrubs and pulls share a configurable I/O policy.
- A clean scrub proves checksums, not that a VM boots. That is what restore
  tests are for.

## Appliance platform

- Ubuntu Server LTS, amd64, installed by the Deployer's appliance ISO or from
  the `.deb` (`DEPLOYMENT.md`).
- Ubuntu's unattended upgrades stay on, but needrestart is told not to
  restart `bmac-backupd` (`/etc/needrestart/conf.d/bmac-backupd.conf`):
  that would cut the pulls and exports in progress and end every UI
  session. Pulls and restores resume after any restart regardless.
- The OS is on its own disk or disks. The backup pool uses LUKS2 under ZFS,
  with any layout (`STORAGE.md`). `bmac-backupd create-pool POOL LAYOUT
  DISK...` (disks named by `/dev/disk/by-id/`, erased after the operator
  types the pool's name) formats every disk with a LUKS2 label naming the
  pool, so `unlock` finds the members with nothing from the OS disk. The
  pool's cache file is `none` and nothing is in crypttab.
- After a reboot the pool stays locked until the operator runs
  `bmac-backupd unlock` at the console (it refuses to run over SSH). One
  prompt unlocks every member, then the pool is imported by GUID and its
  metadata dataset mounted. Until then the appliance checks in as locked, its
  UI answers only `hello` and login, and it does nothing else. There is no
  unattended unlock.
- Identity is by appliance UUID and pool GUID, never pool name or path. A
  missing pool is never recreated automatically. `bmac-backupd init NAME
  POOL` binds an imported pool by GUID and makes the appliance's identity:
  a self-signed ECDSA P-256 certificate whose DNS name is the appliance's
  name and whose `urn:uuid:` name is its UUID.
- The administrator's password (`bmac-backupd admin-password`, at least 12
  characters, stored as SHA-512 crypt) and the appliance's name are kept in
  the metadata dataset, so they are encrypted with the pool and survive an
  OS reinstall.
- After an OS reinstall with the pool intact: unlock, import, and run
  `bmac-backupd adopt`, which rebuilds the catalog from the metadata dataset
  and requires the administrator's credentials and a new enrollment.
- The appliance's own HTTPS UI on port 8443 (`v3/applianceui/`, built from
  the shared React components and embedded in `bmac-backupd`) presents the
  appliance's own certificate, whose fingerprint the login page and the
  console show so the administrator can compare it with the browser's. It
  follows the same login rule as the Web Dashboard: the token lives only in
  page memory, is sent as a bearer token, and no cookie is ever set.
  Sessions are kept only in the daemon's memory (30 minutes idle, 12 hours
  at most, 64 at once), so a restart ends them all and a password change
  ends every other one. Logins are limited per address and in total; a
  request from another origin, or a `POST` that isn't JSON, is refused. It
  covers status, the PIT browser and graph, transfer throughput, retention
  policy (a weakening change waits out the delay), holds, prunes, restore
  grants (the token is shown once), trusted peers (trust and untrust, see
  "Trusted peers"; untrust needs the password again), scrubs (with "Scrub
  now"), the I/O policy, the watchdog and notification settings, unlock
  status, and the audit log.
- Appliance audit log: every pull (with bytes, duration, and average rate),
  publish, prune, grant, trust and untrust, export to a peer, refused peer,
  and policy change, with IDs and GUIDs, never data or secrets.

## Protocol

Everything is TLS 1.3 with mutual authentication, using HTTP/2 from the
`hyper`/`rustls` stack. Version negotiation comes first on every connection.
`bmac-source` listens on nothing: it answers the appliance over the
connections it opens (see "Connections"). The appliance listens on 8443
(its UI) and 8445, which takes the hosts' connections (`bmac-source/1`)
and exports to restore jobs and trusted peers.

| Endpoint | Served by | Client | Purpose |
| --- | --- | --- | --- |
| `POST /v1/checkin` | `bmac-source` | Enrolled appliance | Report status and heads; receive offers |
| `POST /v1/claim` | `bmac-source` | Enrolled appliance | Claim an offered PIT before the claim window ends |
| `POST /v1/index` | `bmac-source` | Enrolled appliance | The appliance's PIT index (metadata only), when asked |
| `GET /v1/catalog` | `bmac-source` | Enrolled appliance | PIT manifests for this cluster |
| `POST /v1/locate` | `bmac-source` | Enrolled appliance | Which hosts hold given snapshot or bookmark GUIDs |
| `POST /v1/send` | `bmac-source` | Enrolled appliance | Stream a full, incremental (from a snapshot or bookmark), or resumed send of a BMAC snapshot that exists on this host |
| `GET /v1/recovery-manifest` | `bmac-source` | Enrolled appliance | Cluster recovery manifest |
| `GET /v1/luks-headers` | `bmac-source` | Enrolled appliance | The SHA-256 and size of each of this host's LUKS header backups (`DEPLOYMENT.md`), which `bmacd` reads from `/var/lib/bmac/luks-headers/` and hands over the source socket; `bmac-source` cannot read `/var/lib/bmac` |
| `POST /v1/luks-header` | `bmac-source` | Enrolled appliance | One named header backup, gzip-compressed, with its SHA-256 |
| `GET /v1/heartbeat` | `bmac-source` | Enrolled appliance | Host and cluster liveness for the watchdog (`MONITORING.md`) |
| `POST /v1/export` | Appliance | Cluster restore job (with a restore or recovery grant) or trusted peer | Stream a published PIT volume, full or incremental from a given GUID, or its size estimate |
| `GET /v1/peer-index` | Appliance | Trusted peer | Published PITs (metadata only) |
| `POST /v1/recovery/index` | Appliance | Host named by a recovery grant | The lost cluster's newest recovery manifest and every published PIT of it |
| `POST /v1/recovery/bind` | Appliance | Host named by a recovery grant | Bind the namespace to the rebuilt cluster: its CA and hosts |
| `POST /v1/enroll` | `bmacd`, port 8444 (`SECURITY.md`, "Joins and enrollment") | New appliance (with token) | Present the appliance certificate for approval |

`bmac-source` checks the client certificate's fingerprint against the
approved appliances in pmxcfs. It refuses any snapshot or bookmark not named
`bmac-pit-*`, or not on a registered VM's volume, offered to that appliance,
and never sends `-R` streams.

## Alerts

All are listed in `MONITORING.md`, and each can be switched off in the alert
settings:

- appliance not checking in (LATE), delinquent, locked;
- backup skipped: no appliance available for a VM's scheduled PIT;
- an available appliance didn't claim, or failed, an offered PIT;
- PIT capture failed;
- a VM's newest published PIT is older than its schedule allows, per
  appliance;
- fewer healthy appliances than the minimum for a VM;
- appliance pool or namespace quota filling.

## Failure handling

| Condition | Response |
| --- | --- |
| One appliance stops checking in | LATE after 3 minutes, alert; the others continue |
| No assigned appliance available at a scheduled slot | No snapshot (unless local retention claims the slot); alert |
| Appliance doesn't claim within the claim window | Snapshot released for it; it catches up from its head's bookmark at the next PIT |
| Appliance delinquent | Its bookmarks are released; registration and data remain; it repairs from a trusted peer or reseeds |
| Appliance returns | Repair from cluster, then a peer that both trusts and is trusted by it, then reseed |
| Untrusted appliance connects to an appliance | Refused in the TLS handshake; audit log entry |
| Trusted peer untrusted during a stream | Stream closed; the receiving appliance aborts the partial receive and tries its other peers or reseeds |
| Pull interrupted | Resume with token while the snapshot exists; otherwise abort the partial receive and catch up at the next PIT; never publish partial |
| Publication report lost | The next check-in reports it; nothing is assumed |
| Pool locked after reboot | `LOCKED`; alert to unlock at console; no automatic unlock |
| Pool or quota full | Refuse new pulls; alert; never delete published PITs to make room |
| Unrepairable scrub error | Mark PITs suspect; alert; point to other copies |
| Required volume missing | PIT not published |
| GUID mismatch | Refuse the incremental; never force |
| Bookmark missing on the host serving a send | Serve from another placement host that has it; otherwise the appliance repairs from a trusted peer or reseeds |
| Owner moves before snapshot | Resolve again; retry under the resource intent |
| Owner moves during a pull | Any host with the snapshot and base can continue the stream; otherwise retry from another host |
| Placement host added | Replication brings the BMAC snapshots, and bookmarks are made there, before that host serves sends |
| Staging clone depends on a PIT | Local retention skips it and says why |
| Guest freeze hangs | Watchdog thaws; PIT fails unless the policy allows crash consistency |
| Rollback requested with dependent clones | Refuse; offer clone-and-promote |
| Appliance certificate revoked | Refuse its connections; its stored PITs stay |
| Whole cluster lost | Recover from an appliance (see above) |
| Appliance OS lost, pool intact | Reinstall, unlock, `adopt` |
| Appliance pool lost | Register a new appliance; it seeds from the newest PITs |

## To prove in the lab before relying on it

Each item has a safe fallback that is used until it's proven.

1. Proxmox replication carries BMAC snapshots to every placement host and
   removes them when the owner deletes them. Fallback: none needed; v2 staging
   already relies on it, but the backup tests check it explicitly.
2. Bookmarks made on each placement host from its copy of a snapshot have the
   snapshot's GUID, survive later replication runs and an HA move or forced
   failover, and an incremental from one, sent by the new owner, applies on
   the appliance. Fallback: keep appliance heads' snapshots instead (see
   "Snapshots and bookmarks on the cluster").
3. An incremental `P3 → P6` received with `-o origin=…@P3` as a clone on the
   appliance, for new branches, and the same on the cluster for restore
   method 2. Fallback: full send.
4. A restored volume keeps its PIT snapshot's GUID through `zfs rename` into
   a Proxmox storage, `zfs promote`, and a local `zfs send | zfs receive`.
   Replication's first sync carries that snapshot to every placement host,
   for a received volume and for a method 2 clone. A bookmark of it is the
   base of the next PIT, and an appliance receives that PIT as a clone of
   its own copy of the restored PIT, also when the branch descends from
   another VM's PIT. Fallback: a method 2 clone is promoted or copied
   locally like a method 1 one; if replication still drops the snapshot,
   the first PIT after a restore is a full send.
5. ZFS `send` and `hold` delegation to an unprivileged user on
   Proxmox VE 9. Fallback: `bmac-source` runs as root with the same fixed
   request set.
6. `volmode=none` and the property allowlist on receive, on the appliance's
   OpenZFS version. Fallback: receive into a dataset whose children have
   device nodes blocked by udev rules.
7. Incremental receive after an online disk growth (`volsize` change).
   Fallback: reseed that volume.
8. Guest agent freeze and thaw on the supported guest OS.
9. VM recreation from the manifest, including EFI, TPM, and Secure Boot, on a
   fresh cluster.
10. Throughput of large seeds over the hosts' connections to the
   appliance's private address, measured with the transfer metrics. The
   lab never sends backup data over Tailscale (see "Appliance addresses").

## Acceptance scenarios

Before release, the lab runs this continuous scenario, and then it runs on
hardware. During development it runs in compressed time, in about an
hour (short backup settings, schedules every few minutes, and a test day,
see "Calendar"); its soak run uses the defaults and real days for 8 days:

1. full seed to appliances A and B, with VM 1 on both and VM 2 on B only;
2. scheduled incrementals, with snapshots bookmarked and released after the
   pulls and throughput recorded for each transfer;
3. staging clones created and removed during backups;
4. a planned owner move, then a hard failover, with the next incremental
   sent from a bookmark by the new owner;
5. A stops checking in: LATE alert; B continues;
6. A returns within the delinquency period and catches up from its bookmark;
7. A returns after it and repairs from B, which it trusts and which trusts
   it, after both administrators add each other on the Trusted peers page;
   then B untrusts A, and A's next repair from B is refused;
8. a scheduled slot with no appliance available: no snapshot, and the alert;
9. a local rollback starts a new branch, received as a clone on both
   appliances;
10. restore from both branches, by each restore method, to a new VM and into
    the existing VM, with the estimates shown beforehand, and the first
    backup after each restore an incremental from the restored PIT on every
    appliance that holds it;
11. pruning of intermediate PITs;
12. an appliance reboot (`LOCKED`, then unlocked);
13. a resumed pull;
14. a scrub;
15. a restore test;
16. VM recreation;
17. complete cluster loss and recovery onto new lab hosts.

Security tests:

- with every cluster credential, try to delete, change, release, or read
  published PITs, change retention, and log in to an appliance; all must
  fail;
- a stolen appliance certificate can't get sends of non-BMAC snapshots, or
  of VMs not assigned to it;
- peers can't modify each other;
- an appliance that B doesn't trust gets nothing from B's 8445, not even
  the peer index or a size estimate, and its refusal is in B's audit log;
- with one-way trust (B trusts A, A doesn't trust B), A refuses B, and A
  never repairs from B;
- the cluster, with every cluster credential, can't add a trusted peer to
  any appliance;
- a typed fingerprint that doesn't match the peer's certificate saves no
  entry;
- quotas stop a cluster from filling an appliance;
- no LUKS passphrase or grant appears in any log, page, or email.
