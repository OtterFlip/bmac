<!--
Copyright (c) 2026 BEENTHERE VENTURES, INC.
SPDX-License-Identifier: GPL-3.0-only
-->

# Proxmox hosts, traffic, and application HA

This is the authoritative design and operations guide for the Proxmox host
layer. It replaces the removed `proxmox_traffic_map.md` and
`proxmox_haproxy_ha_design.md`, whose two-host, `.2`/`.3`, and
guest-Tailscale assumptions became obsolete.

The implementation supports a contiguous cluster named `mox1` through
`mox10`, bounded by `MAX_MOX_HOSTS`. `mox1` creates the cluster; each later
`moxN` joins through `mox1` only after every existing node is online. A
production VM is one movable `prodN`, active on one HA placement node at a
time. A `stageNprodN` staging VM is disposable, non-HA, and fixed to one eligible
standby node. Membership/parity logic covers one through ten nodes; the
completed application-HA/VRRP deployment requires at least two candidates.

## Address and identity contract

The checked-in FiberState example uses private Layer-2 VLAN
`10.213.0.0/24`. All addresses below are examples, not defaults: configure
the VLAN and role ranges in `env/cluster.conf`, and configure each server's
public, private-NIC, and iDRAC addresses in `env/moxN.conf`.

The implementation currently requires a shared IPv4 `/24`, but no role is
tied to the example's host octets. The example allocation is:

- `.0` is the network address; `.255` is broadcast.
- `.1` is FiberState-reserved and is not an application-HA route or Internet gateway.
- `.2-.9` are reserved. In particular, `.3` has no forwarding, DNAT, VIP, or
  proxy role. No packet flow in this design uses `.3`.
- `.10` is the Keepalived floating guest-egress VIP.
- `.11-.20` are `mox1` through `mox10`.
- `.21-.30` are the fixed `haproxy1` through `haproxy10` LXCs.
- `.31-.50` are `prod1` through `prod20` production HA VMs.
- `.51-.200` are dynamically allocated staging VMs.
- `.201-.240` are reserved for future use.
- `.241-.250` are dedicated migration/replication addresses for
  `mox1` through `mox10`.
- `.251-.254` are reserved for future use.

For host index `N`, the derived contract is:

- host: `moxN`, private IP `MOX_IP_START+(N-1)`;
- local proxy: `haproxyN`, private IP `HAPROXY_IP_START+(N-1)`, VMID `9110+N`;
- proxy default gateway: that host's derived mox address;
- VRRP priority: `200-N`, from 199 on `mox1` to 190 on `mox10`.

Host names, cluster membership, private addresses, HAProxy addresses/VMIDs,
and VRRP peers must stay contiguous. Never repurpose an unused address inside
one of these ranges.

Host setup derives `moxN.pve.internal` from the configurable
`PROXMOX_INTERNAL_DOMAIN` and writes every configured `mox1` through
`mox${MAX_MOX_HOSTS}` short name and internal FQDN into a managed
`/etc/hosts` block on every mox, starting at `MOX_IP_START`. These names always resolve to the private
VLAN from a mox host; public application domains are never host identities.
The FQDN is immutable after installation; changing the configured internal
domain requires a deliberate host reinstall rather than an in-place rename.

## Traffic planes

Each mox has four distinct access paths:

- Public `vmbr0`: the host's unique public IPv4 and only default route. It
  carries host updates, Cloudflare origin traffic, and SNATed egress.
- Private `vmbr-private`: the configured shared 10-Gbps `/24`. It carries Corosync link
  0, secure migration, ZFS replication, VRRP, HAProxy-to-guest traffic, and
  ordinary guest egress.
- Tailscale: host administration, Proxmox UI/API access, QDevice traffic, and
  lower-priority Corosync link 1. It is not application ingress or guest
  egress.
- iDRAC/IPMI VPN: installation media, console-only LUKS entry, boot
  observation, and recovery.

Production and staging guests deliberately do not run Tailscale. Operators
reach their private IPs with strict SSH host-key checking through a mox
Tailscale `ProxyJump`. Same-subnet guest traffic is switched directly and
does not pass through `.10`, so the shared `/24` is not an isolation boundary.

## Public ingress and per-host HAProxy

Every mox owns one unprivileged, non-HA Debian HAProxy LXC that stays on that
host. Host nftables performs only these application forwards:

```text
public moxN TCP 80/443
  -> per-host DNAT
  -> haproxyN .(20+N):80/443
  -> exact registry-selected prodN/stageNprodN private IP
```

Port 80 uses HTTP `Host` routing. Port 443 uses TLS SNI inspection and TCP
passthrough; HAProxy does not terminate origin TLS. Unknown hosts and SNI are
rejected. The registry renderer produces deterministic, reject-by-default
configuration, and `sync_haproxy_routes.sh` stages and validates every online
proxy before graceful reload. If a target fails, already-backed-up targets
are rolled back.

Each `haproxyN` uses its local `moxN` private IP as its default gateway, never
the `.10` VIP. This keeps replies for public DNAT connections on the host whose
conntrack table owns them. That host also masquerades the LXC's outbound
traffic. There is no shared forwarding address, especially not `.3`.

The host guard explicitly drops public TCP 22, 8006, and 3128 to the mox
address. Only 80/443 are installed as application DNAT. This is a specific
guard, not proof that every other listener is safe; audit the complete public
ruleset and listening sockets.

## Floating guest egress and VRRP

All production and staging guests use `.10/24` as their default gateway.
Every configured mox has the same forwarding and masquerade rules for
`.31-.200`, but exactly one quorate candidate owns `.10`.

Keepalived is configured identically on every candidate except for source IP
and priority:

- every mox starts in `BACKUP`;
- every mox lists all other configured `mox1` through
  `mox${MAX_MOX_HOSTS}` private addresses as unicast peers, including
  candidates not yet joined;
- all use VRID 10, one-second advertisements, and `nopreempt`;
- a candidate must have both interfaces up, its public IP/default route,
  forwarding and nftables loaded, working public egress, and local Proxmox
  quorum;
- takeover sends repeated gratuitous ARP.

The owner of `.10`, the host running a production VM, and the Cloudflare
origin selected for a request are independent decisions. Existing NAT
sessions normally reset when VIP ownership changes because conntrack and the
public source IP are not synchronized. New sessions work after VRRP/ARP
convergence. External egress allowlists therefore need every candidate mox
public IP.

## Cluster quorum and data traffic

Corosync uses:

```text
link 0: each mox .11-.20 over the private 10-Gbps VLAN, priority 100
link 1: each mox Tailscale address, priority 10
```

Each host also receives exactly one address from the dedicated
`10.213.0.240/28` migration/replication CIDR on that bridge. That narrower CIDR
is written to `/etc/pve/datacenter.cfg`, ensuring Proxmox matches only one
local transport address per host. Migration and normal ZFS replication must
not use public interfaces or Tailscale. Replication is asynchronous, so the
last successful replication point defines a non-zero recovery point
objective.

The external QDevice is a quorum voter only:

- odd Proxmox node count: QDevice must be absent;
- even Proxmox node count: QDevice must be present, alive, and voting, giving
  exactly `node_count + 1` expected and total votes;
- before any node joins, an existing QDevice is removed while all nodes are
  online; after the join, parity is reconciled;
- QDevice setup uses its verified literal Tailscale IPv4 for TCP 5403, while
  its name is used for administrative SSH;
- the cluster RSA key is authorized on the QDevice only while
  `pvecm qdevice setup` exchanges certificates, then removed after the
  configured vote is proven healthy.

QDevice never carries application, HAProxy, guest-egress, storage,
replication, or migration traffic and must never be configured as a router,
gateway, proxy, or Tailscale exit node.

## Host storage and source media

The reviewed bare-metal source is `proxmox-ve_9.2-1.iso`; its checksum is
tracked in `artifacts/proxmox-ve_9.2-1.iso.SHA256.txt` and the configured
absolute ISO path/hash must match before media is built. The setup script
creates a host-specific unattended ISO directly under `artifacts/<moxN>/`;
its answer and first-boot inputs are under that host's `generated/` directory.
Setup keys, logs, GPT backups, and LUKS headers also remain under the
mode-`0700` host artifact tree and must never be committed. The only shared
durable host artifact is the tracked
`artifacts/used-tailscale-auth-key-sha256` denylist. Its ignored `.lock` file
only serializes concurrent check-and-append operations.

One through five contiguous NVMe mirror pairs may be configured:

- Mirror 1 is mandatory. The Proxmox installer destroys its two
  serial-selected disks, creates the bootable ZFS mirror, and reserves space
  for optional later conversion of each data partition to LUKS2.
- At the start of each run, choose either iDRAC/Redfish inventory or manual
  Live Linux inventory. iDRAC mode verifies configured serials, capacity, and
  health remotely. Manual mode requires exact
  `NVME_MIRROR_<P>_CAPACITY_BYTES_<M>` values for every configured serial and
  requires both members of each pair to have identical byte capacities.
- The operator chooses LUKS2 or unencrypted storage before choosing whether
  to run boot tests. The choice is durable and cannot be changed on resume.
- In LUKS mode, the script degrades and rebuilds mirror 1 one member at a time
  behind `crypt-rpool-a` and `crypt-rpool-b`, preserving both ESPs. Optional
  mirror pairs 2 through 5 become whole-disk LUKS2 mirror vdevs.
- In unencrypted mode, mirror 1 remains as installed and optional mirror pairs
  are added as unencrypted whole-disk mirror vdevs.
- In LUKS mode all members use `PROXMOX_LUKS_PASSWORD` from the protected local
  `secrets.env`.
  `decrypt_keyctl` caches it only in the initramfs kernel keyring so one
  target-console prompt unlocks every member. Every host boot requires that
  console entry.
  During conversion and selected tests, helpers read a temporary root-owned
  mode-`0600` file on the host rpool instead of repeatedly prompting. The file
  is never embedded in media, crypttab, initramfs, setup state, logs, or
  pmxcfs, and is deleted and verified absent before storage setup completes.
- A header backup for every LUKS member is copied off-host into the local
  artifacts tree before completion. Protect and separately back up those
  files.

Adding a top-level vdev expands the pool and may not be reversible. Losing
both members of any mirror vdev loses the striped pool. Disk serials, mapper
identity, pool membership, and interrupted phase records are revalidated
before destructive post-install work. iDRAC mode obtains live pre-install
capacity and health; manual mode relies on the operator-attested Live Linux
inventory for mirror 1 because the automated installer cannot independently
check the configured capacity before erasing its serial-selected disks.
When selected, the boot drill always uses exactly three reboots: each mirror-1
member boots alone in the chosen clear or fully encrypted state, each missing
member is restored and resilvered, then the healthy final mirror is rebooted.
The drills can be skipped without changing the storage choice.

Whenever setup pauses for an on-host helper, it prints a prominent
`TARGET-HOST HELPER SCRIPT STATUS` block with the target host, exact helper
path, and expected result. The following `SCRIPT WORKED?` prompt accepts `GO`
only after the operator has actually run that helper successfully.

## Host setup flow

For manual inventory, boot the target host from a Linux Live CD/USB in its
try/live mode, not its installer. Copy or otherwise make
`hosts/inventory_disks.sh` available there and run:

```bash
chmod +x inventory_disks.sh
./inventory_disks.sh
```

The helper is read-only. For each desired mirror, choose two entries reported
as `NVMe SSD` with the same exact `Capacity bytes` value and distinct,
non-empty serials. Copy the printed serial and byte-count values into the
corresponding `NVME_MIRROR_<P>_SERIAL_<M>` and
`NVME_MIRROR_<P>_CAPACITY_BYTES_<M>` keys in `env/moxN.conf`. Leave disks that
must not enter `rpool` unconfigured. Capacity keys are optional in iDRAC mode.

Run host setup from an administrator workstation:

```bash
hosts/setup_proxmox_host.sh \
  --host moxN [--encrypt | --no-encrypt] \
  [--run-boot-tests | --skip-boot-tests]
```

If per-host artifacts already exist, the first host-specific prompt offers to
resume or delete the complete local artifact tree and start over. Starting
over removes generated media, state, setup SSH material, logs, and LUKS
recovery headers, but retains the global digest denylist for exposed Tailscale
auth keys.

The resumable flow:

1. Selects iDRAC/Redfish or manual disk inventory, then strictly loads
   `env/cluster.conf`, `env/moxN.conf`, and mode-`0600`
   `env/secrets.env`; validates host formulas, serial-selected NVMe pairs,
   public/private MACs, hashes, and a secret-inclusive setup fingerprint.
2. In iDRAC mode, queries Redfish without mutation. In manual mode, validates
   serials and exact capacities previously gathered with
   `hosts/inventory_disks.sh` from a Live Linux environment. It then verifies
   the source ISO and builds host-specific media containing a fresh one-time,
   non-ephemeral `tag:proxmox-host` Tailscale key.
3. Stops for explicit install-media mapping and destructive install
   confirmation, then verifies the newly booted host, public NIC, host guard,
   and Tailscale bootstrap.
4. Applies the selected clear or LUKS storage policy, adds configured mirror
   vdevs, and, in LUKS mode, installs shared one-passphrase unlock and captures
   every header backup. Selected boot testing then proves both members
   independently and the restored final state in three reboots.
5. Selects the private NIC by configured MAC, allows approximately 30 seconds
   for 10-Gbps carrier negotiation, creates `vmbr-private`, and stops for a
   provider-console/private-Layer-2 verification gate. It then installs the
   complete private mox hostname map before any cluster action.
6. Provisions and verifies `corosync-qnetd` over Tailscale even on the first
   host run. It creates the cluster on `mox1` or joins the next contiguous node
   through `mox1` using its private internal FQDN and a certificate fingerprint
   obtained over trusted workstation SSH. It refreshes Proxmox 9's native
   per-node SSH pins and verifies every private mox-to-mox path, configures
   Corosync links plus secure migration/replication, then reconciles QDevice
   vote parity. The prepared QDevice remains vote-absent for odd node counts
   and votes for even counts. After a join, subsequent setup operations use
   the verified administrator identity and remove the temporary per-host setup
   key from the cluster-wide root authorization file.
7. Installs quorum-aware guest SNAT/VRRP, shared orchestration tools, the
   generic lifecycle hook and cleanup timer, and the fixed local HAProxy LXC.
   The LXC uses `C.UTF-8`, and its first route generation is synchronized
   before strict ingress activation.
8. Synchronizes registry-backed routes and performs final storage, boot,
   quorum, network, service, hook, proxy, health-script, and exact single-VIP
   ownership verification.

All existing configured nodes must be online for membership changes. Never
skip a manual gate merely because local state says an earlier phase completed;
the script also reconciles live hardware, disk, cluster, and remote phase
state. Membership, native SSH-pin, and QDevice reconciliation is protected by
both a checkout-local lock and an SSH-held `flock` on mox1, so installers from
separate workstations cannot mutate the control plane concurrently. A
companion `/run` lease remains fail-closed if the SSH holder disappears; the
next run identifies its owner and requires an operator to verify that no
installer remains before removing a stale lease. Run the same command again
after correcting a failure.

## Production and staging lifecycle

`guests/prod/create_prod_vm.sh` allocates a deterministic `prodN`, an address
from `PRODUCTION_IP_START` through `PRODUCTION_IP_END`, a MAC, and a
live-checked VMID in pmxcfs. Host setup installs the production ISO
cache/builder tools and their `curl`/`xorriso` dependencies on every mox. The
creator downloads or reuses the configured hash-verified OS source on the
initial node and prepares the per-VM installer there. Ubuntu autoinstall mode
embeds answer data; manual mode attaches an unchanged verified copy and prints
the supported Linux guest contract. It installs one direct root filesystem,
uses the configured `GUEST_EGRESS_VIP` as its default gateway, enables QGA and
key-only root SSH, supports an optional idempotent pre-network `startup.sh`,
and does not install guest Tailscale. Only after guest identity/SSH attestation
does it create replication jobs, verify an initial sync to every placement
node, add a strict HA node-affinity rule, request HA start, and publish exact
Host/SNI routes.

Every production placement node needs enough CPU, RAM, and `local-zfs` space
to run all production resources assigned to it. Production root zvols are
sparse by default, so `rpool` can be overcommitted; monitor pool free space on every
placement node. Staging eviction is emergency reclamation, not production
capacity.

`guests/staging/create_staging_vm.sh` selects an active production source and
an online placement standby, takes a unique source-owned Proxmox snapshot,
requires the replicated snapshot GUID to match on every production placement
node, creates a direct local ZFS clone, patches it offline, and registers one
exact staging route. The result is non-HA, `onboot=0`, and may remain stopped
or start after operator confirmation.

The generic lifecycle hook is attached to both roles:

- production pre-start serializes lifecycle work, force-stops the guest, and
  destroys the QEMU config and local clone only for staging VMs whose name,
  exact tags, registry UUID/VMID, owner, volume, snapshot origin/GUID, and
  non-HA status all agree;
- staging pre-start refuses admission while production is running or reserved
  to start on that node;
- positively identified eviction queues unreachable route/snapshot cleanup
  rather than blocking production indefinitely;
- planned migration still requires operators to stop staging on the
  destination first.

The staging source snapshot is outside Proxmox's supported linked-clone
lifecycle and can pin substantial production ZFS space. Never delete,
rollback, or lose that source snapshot/replica while a clone depends on it.
See the production and staging READMEs for full creation, resume, sanitizer,
and rollback contracts.

Application deployment happens after generic production VM creation and is
outside this Proxmox project.

## Cloudflare, DNS, and certificates

Cloudflare configuration is external and is never changed by these scripts.
For each production application:

1. Use the public origin IPs printed by the production workflow (and any
   additional deliberately enabled mox origins) for TCP 80/443.
2. Route the primary FQDN and aliases to that origin pool, with a monitor Host
   header accepted by the exact registry route.
3. Keep origin TLS at Full (strict) after the backend certificate exists.
4. Reserve the `stageNprodN.<primary-FQDN>` names needed for staging derived
   from that production resource.

The registry creates one exact Host/SNI route per staging VM, normally
`stageNprodN.<primary-FQDN>`. Unknown names never fall through to production.
Operators must arrange DNS, Cloudflare health checks, origin allowlisting, and
valid guest certificates. A first-level wildcard such as
`*.example.com` can cover these staging names. A new route can correctly
show an unavailable backend until application/TLS provisioning finishes.

## Exact pmxcfs inventory

Production uses the root-only custom state root
`/etc/pve/priv/app-ha`. The following is the complete custom pmxcfs layout;
all persistent JSON records use `schema_version: 1`:

- `/etc/pve/priv/app-ha/` — registry root. Created by
  `cluster_registry.py init`; every registry command reads its layout.
- `policy.json` — `record_type: "policy"`; immutable registry UUID, fixed
  network/IP/VMID ranges, host/resource limits, tags, defaults, and allowed
  state transitions. Host setup writes it once; all allocators, validators,
  hooks, route tools, and cleanup workers read it.
- `resources/` — current resource records.
  `resources/prodN.json` and `resources/stageNprodN.json` use
  `record_type: "resource"` and store allocation/request identity, kind/name,
  VMID, IP, deterministic MAC, state, purpose/domains, placement/owner,
  compute/storage policy, routes flag, HA/replication volume metadata, and the
  staging snapshot dependency when applicable. Production/staging creators,
  lifecycle cleanup, and explicit registry updates write them; all
  orchestration components read them.
- `orchestration/` — durable production-installer state.
  `orchestration/prodN.json` uses `record_type: "orchestration"` and stores the
  resource UUID, install phase, immutable final-network/`startup.sh` digest
  contract, revision, and renewable lease. Only the production creator writes
  it through `orchestration-*`; production resume and release read it.
- `deferred-cleanup/` — cross-node cleanup queue.
  `deferred-cleanup/cleanup-<24-hex>.json` uses
  `record_type: "deferred-cleanup"` and identifies one resource UUID, target
  node, typed action (`destroy-vm`, `destroy-volume`, `delete-snapshot`,
  `remove-replication`, or `remove-route`), safe target, reason, attempts,
  state, and revision. Staging rollback and the lifecycle hook enqueue;
  `process_deferred_cleanup.sh` validates live identity and records completion
  through reconciliation.
- `history/` — release audit.
  `history/<UTC>-<resource>-<8-hex>.json` uses
  `record_type: "release"` and contains release time, force flag, and the final
  resource record. `cluster_registry.py release` and safe staging cleanup
  finalization write it; registry listing is the reader.
- `ingress/` — schema for durable desired/applied HAProxy generations.
  `ingress/generations/<64-hex>.json` has
  `record_type: "haproxy-generation"` with generation, bundle digest, route
  count, and creation time; `ingress/desired.json` has
  `record_type: "haproxy-desired"` with the desired generation and all
  configured mox nodes; `ingress/nodes/moxN.json` has
  `record_type: "haproxy-node-status"` with desired/applied generation and
  `pending`, `staged`, `applied`, or `reject-only` state.
  `cluster_registry.py ingress-begin/ingress-mark` write these records and
  `ingress-status` reads them. The current live synchronizer renders routes
  from `resources/` and performs its rollback transaction directly.
- `.allocator-lock-owner.json` — ephemeral
  `record_type: "allocator-lock"` owner/nonce/node/PID/timestamp metadata for
  contention diagnostics. Every mutating registry command creates/replaces it
  after obtaining the lock and removes it before releasing the lock.
- `.<filename>.<UUID>.tmp` — transient sibling used for one atomic pmxcfs file
  replacement. A successful write renames it over the destination; failure
  cleanup removes it.
- `/etc/pve/priv/lock/app-ha-registry-<16-hex>/` — the one cluster-wide
  allocator lock directory, with the suffix derived from the state-root path.
  It is created atomically and must stay empty. Owner metadata is deliberately
  the sibling file above, because a non-empty directory cannot be the pmxcfs
  `mkdir` lock. Never put a file in this directory.

`/etc/pve/priv/lock/` itself is Proxmox-owned. The only existing standard
pmxcfs file edited directly by this project is `/etc/pve/datacenter.cfg`,
where host setup reconciles the secure migration and replication network.
`pvecm`, `pvesh`, `pvesr`, and `ha-manager` own their normal Corosync,
firewall, replication, HA resource, and HA rule records; they are not custom
app-ha files. The lifecycle hook is copied to local snippet storage on every
host, not stored in this custom pmxcfs tree.

### pmxcfs permissions, locking, and retention

All real `/etc/pve` registry operations require root. `/etc/pve/priv` supplies
pmxcfs's root-only visibility and fixed permission semantics; pmxcfs is not a
normal filesystem and does not support ordinary `chmod`, `chown`, directory
`fsync`, `O_EXCL`, or non-empty-directory replacement assumptions. The
registry therefore validates non-symlink files, bounds each JSON file to
1 MiB, writes a sibling then atomically replaces the destination, and skips
unsupported local-filesystem operations under `/etc/pve`.

The empty `mkdir` lock serializes every mutation cluster-wide. Age alone never
authorizes removal. After inspection, `--break-stale-lock` sets the pmxcfs
lock mtime to zero to request pmxcfs's checksum-guarded, cluster-wide
stale-lock handling. Never manually remove a non-empty or apparently active
lock.

Retention is bounded:

- release history keeps at most 256 newest records;
- ingress generation metadata keeps the newest eight while retaining the
  current desired generation;
- deferred cleanup allows at most 4096 records and retains up to 512 completed
  records, pruning completed records only after their resource identity is no
  longer active;
- current resources, orchestration contracts, desired ingress, and one node
  status per configured mox are retained until an explicit validated
  lifecycle transition replaces or releases them.

`reconcile --live` compares safe local Proxmox node/VM/volume facts.
`reconcile --observed` accepts a typed, secret-free cross-cluster observation.
Reconciliation is read-only unless `--apply` is supplied; apply may only mark
expired VM-less reservations failed and explicitly reported cleanup IDs
completed. It does not create, move, stop, or destroy Proxmox resources.

No pmxcfs record may contain credentials, tokens, password hashes, private
keys, Tailscale auth keys, or secret-shaped field names/values. The registry
rejects them recursively. `secrets.env`, generated installation media, setup
keys, and LUKS headers stay outside pmxcfs.

## Security invariants

- Keep repository secret input local, Git-ignored, non-symlinked, and exactly
  mode `0600`; never enable shell tracing around secrets.
- Verify SSH host keys out of band. Workstation-to-mox and mox-to-guest SSH
  uses strict checking and no inherited forwarding.
- Keep root SSH in production and staging key-only. Console passwords are
  hashes inside installation/patch material and are never registry data.
- Do not install Tailscale in guests. Tailscale admin ACLs terminate on mox;
  guest access uses the private IP through a mox jump.
- Restrict Cloudflare origins and audit all public host listeners. The script's
  explicit management-port drops do not constitute a blanket firewall.
- Enforce production/staging isolation with Proxmox per-VM and guest firewall
  policy because same-subnet packets bypass `.10`.
- Keep QDevice independent, patched, and restricted to quorum/admin traffic.
- Treat local artifacts and staging clones as sensitive production-derived
  data even when they are Git-ignored.

## Failure behavior and material risks

- Mox failure: surviving quorum is decided by Corosync/QDevice; an eligible
  peer claims `.10`; the production lifecycle hook evicts verified local
  staging; Proxmox HA starts production from the latest replica; Cloudflare
  must choose a healthy remaining origin.
- Gateway-only failure: production placement is unchanged. New egress sessions
  use the new VIP owner; existing sessions normally reset.
- Proxy-only failure: the other Cloudflare origin can continue without moving
  production or `.10`.
- Private-VLAN failure: preferred Corosync, migration, replication, VRRP,
  cross-host backends, and guest networking are impaired. Tailscale Corosync
  fallback does not replace those data paths.
- QDevice failure: packet flow is unchanged, but even-node quorum safety is
  degraded. Do not perform membership changes until parity is healthy.
- Replication lag/failure: HA can recover only the last successful replicated
  state. A fully reserved production zvol also consumes reservation capacity
  on every target.
- Staging clone: the source-owned snapshot is unsupported as a Proxmox linked
  clone, pins storage, is not an application-consistent backup, and can be
  invalidated by operator snapshot/replication changes.
- LUKS: boot requires iDRAC passphrase entry. Lost passphrase/header recovery,
  mistaken serial selection, or both members of one vdev failing can make the
  host unavailable.
- Membership: nodes must remain contiguous and online; QDevice must be removed
  before a join and restored only when resulting parity is even.
- Route transaction: a node changing liveness during target selection aborts
  synchronization. A failed rollback is an operator incident.

Do not introduce Ceph, another storage/recovery plane, non-contiguous hosts,
another guest subnet, or automatic snapshot deletion without revisiting this
design and its failure drills.

## Operator checks and runbooks

Validate configuration before mutation:

```bash
lib/config.sh --check --host moxN --require-secrets
```

Inspect cluster and registry state from a mox:

```bash
pvecm status
pvesh get /nodes --output-format json
/usr/local/lib/app-ha-proxmox/lib/cluster_registry.py list
/usr/local/lib/app-ha-proxmox/lib/cluster_registry.py ingress-status
/usr/local/lib/app-ha-proxmox/lib/cluster_registry.py reconcile --live
```

For a broader read-only snapshot from the administrator workstation, run:

```bash
diagnostics/show_cluster_state.sh \
  >cluster_state.txt
```

It checks the QDevice and every reachable mox, including Proxmox 9 native
host-key pins, quorum/Corosync links, QDevice votes, the private VLAN and
single `GUEST_EGRESS_VIP` owner, encrypted storage and SMART health, services, nftables,
HAProxy generation agreement, failed units, shared ingress state, and joined
registered/live production and staging guest inventory.
Uninstalled or unreachable mox slots are reported and skipped.

Routine verification must establish:

1. every configured cluster node is online and contiguous;
2. QDevice is absent for odd membership or alive/voting with exact totals for
   even membership;
3. exactly one mox owns `.10`, and every candidate lists all other VRRP peers;
4. each host retains its own public default route and each HAProxy LXC uses
   its local mox private gateway;
5. public 80/443 reaches the local proxy, unknown Host/SNI is rejected, and
   public 22/8006/3128 remains blocked;
6. production and staging egress through `.10`, while no rule references `.3`;
7. migration/replication use the private network and every production
   replication job has a recent successful sync;
8. HA affinity, lifecycle hook, deferred-cleanup timer, route synchronizer,
   Keepalived, and both nftables services are healthy;
9. every serial-selected member is in its expected mirror vdev and `rpool` is
   healthy; in LUKS mode, every mapper, crypttab entry, initramfs helper, and
   off-host header is also present; both ESPs are current.

Before production, run controlled drills for VRRP takeover/nopreempt recovery,
one Cloudflare origin loss, HAProxy rollback, staging start refusal,
production-start staging eviction, planned migration into a cleared
destination, QDevice loss, and complete mox power loss. Confirm expected RPO
with real replication timing.

Repository checks:

```bash
bash -n \
  hosts/setup_proxmox_host.sh \
  hosts/app-ha-guest-role-hook.sh \
  guests/prod/create_prod_vm.sh \
  guests/staging/create_staging_vm.sh \
  guests/staging/patch_staging_clone.sh \
  lib/config.sh \
  lib/sync_haproxy_routes.sh \
  lib/process_deferred_cleanup.sh

PYTHONDONTWRITEBYTECODE=1 python3 -m unittest \
  hosts/test_setup_proxmox_host.py \
  hosts/test_app_ha_guest_role_hook.py \
  guests/prod/test_create_prod_vm.py \
  guests/staging/test_staging_vm.py \
  lib/test_shared_libs.py \
  lib/test_haproxy_routes.py \
  lib/test_process_deferred_cleanup.py -v
```

These tests mock destructive Proxmox behavior. They do not replace iDRAC,
physical-link, quorum, migration, replication, HA, Cloudflare, or full
power-loss drills.
