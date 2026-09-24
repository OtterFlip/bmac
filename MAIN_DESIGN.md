<!--
Copyright (c) 2026 BEENTHERE VENTURES, INC.
SPDX-License-Identifier: GPL-3.0-only
-->

# Proxmox application-HA deployment

This directory is the authoritative operator entry point for an
application-agnostic HA platform on Proxmox VE. It covers bare-metal host installation,
cluster formation, encrypted local ZFS, quorum, guest egress, public ingress,
generic production VM creation, disposable staging clones, failover, and
cleanup.

The platform runs one movable production VM per registered application
resource. Proxmox HA chooses its physical node; asynchronous ZFS replication
provides recoverable local storage on every eligible placement node. Each
physical host owns a fixed, non-HA HAProxy LXC. Cloudflare chooses a public
origin, HAProxy maps an exact hostname to a fixed guest IP, the shared
Layer-2 VLAN locates that guest's MAC, and Proxmox HA decides where the guest
runs. Those decisions are intentionally independent.

Run repository commands from the repository root. The scripts are
interactive, fail closed, and resume only after comparing durable state with
live hardware, Proxmox, ZFS, quorum, and registry state. A recorded phase is
never sufficient evidence by itself.

Executable scripts and the files under [`env/`](env/) win if prose ever
conflicts. The checked-in files are a FiberState example, not portable
defaults. In that example:

- guest egress uses floating `10.213.0.10`, not `.1` or `.3`;
- `.3` has no forwarding, DNAT, VIP, proxy, or gateway role;
- production and staging guests do not run Tailscale;
- supported host names are contiguous `mox1` through at most `mox10`;
- staging Host/SNI routing and generic production creation are implemented;
- the former legacy design documents have been consolidated into this README
  and removed.

## Contents

- [Purpose and scope](#purpose-and-scope)
- [Required assumptions](#required-assumptions)
- [Responsibility boundaries](#responsibility-boundaries)
- [Names, addresses, VMIDs, and MACs](#names-addresses-vmids-and-macs)
- [Traffic map and ports](#traffic-map-and-ports)
- [Cloudflare and HAProxy](#cloudflare-and-haproxy)
- [Guest egress and VRRP](#guest-egress-and-vrrp)
- [Corosync, quorum, and QDevice](#corosync-quorum-and-qdevice)
- [Host storage, LUKS, and mirrors](#host-storage-luks-and-mirrors)
- [Configuration, secrets, and artifacts](#configuration-secrets-and-artifacts)
- [The three main workflows](#the-three-main-workflows)
- [Helper inventory](#helper-inventory)
- [Required operational sequence](#required-operational-sequence)
- [Host workflow in detail](#host-workflow-in-detail)
- [Production workflow in detail](#production-workflow-in-detail)
- [Staging workflow in detail](#staging-workflow-in-detail)
- [Lifecycle admission, failover, and cleanup](#lifecycle-admission-failover-and-cleanup)
- [Guest and management access](#guest-and-management-access)
- [Exact pmxcfs inventory](#exact-pmxcfs-inventory)
- [Security invariants](#security-invariants)
- [Failure cases and recovery](#failure-cases-and-recovery)
- [Operator checks and drills](#operator-checks-and-drills)
- [Tests](#tests)
- [Deliberate boundaries and future scripts](#deliberate-boundaries-and-future-scripts)
- [Documentation authority and legacy-document removal](#documentation-authority-and-legacy-document-removal)

## Purpose and scope

The host layer builds an application-HA Proxmox cluster named `BMAC`.
The current repository has host definitions for `mox1` and `mox2`; the
implementation and allocation policy support adding contiguous hosts through
`mox10`.

The managed workload roles are:

- `prodN`: one generic production QEMU VM with a fixed private IP and MAC,
  replicated `local-zfs` storage, a strict HA placement rule, and exact
  production Host/SNI routes. It is active on one placement node at a time.
- `stageNprodN`: one production-derived, disposable staging QEMU VM. It is a
  sparse local ZFS clone on one production standby, is never HA-managed, has
  `onboot=0`, and can be evicted before production starts on that node.
- `haproxyN`: one fixed, unprivileged Debian 13 LXC on each `moxN`. It is
  deliberately local, non-HA, starts before ordinary guests, and fronts exact
  HTTP Host and TLS SNI routes.
- `qdevice`: an external Corosync QDevice reached over Tailscale. It is only a
  quorum voter.

This design provides host-level application availability. It does not create
an active/active application, synchronous storage, zero-RPO recovery, a
database cluster, a backup, or an application-consistency mechanism.

## Required assumptions

Do not rely on this architecture until all of these are true:

1. The data center presents the configured `PRIVATE_SUBNET_CIDR` as one shared
   Ethernet Layer-2 segment
   across every mox private NIC. Guests keep the same IP and MAC when
   production moves, and switching/ARP learning must converge to the new
   location. If the provider routes separate segments per host, this guest
   design is invalid.
2. Each private NIC is selected by permanent MAC, links at 10 Gbps, and can
   be bridged into `vmbr-private`. Interface names such as `enoN` are not
   stable identities.
3. Every mox has a unique public IPv4 and public default route on `vmbr0`.
   `vmbr-private` has only the connected `/24`; it has no default gateway.
4. At least two contiguous, online, quorate mox nodes exist before creating a
   production resource. All existing nodes are online for membership changes.
5. The external QDevice is operational and independent enough to arbitrate an
   even-node cluster. It is not hosted on the application cluster and is not a
   Tailscale exit node.
6. The same storage IDs have the same meaning on every placement node:
   `local-zfs` for QEMU/LXC data, `local` for disk-backed ISO and snippets.
   Local ZFS is replicated; it is not shared storage.
7. Hosts are amd64 and use Proxmox VE 9. Host media is Proxmox VE 9.2-1;
   production creation requires Proxmox VE 9.2 or newer for the complete
   `ms-cert=2023k` Secure Boot contract. Guests use Ubuntu Server 26.04.1
   amd64.
8. iDRAC is reachable through the provider IPMI VPN for installation,
   destructive storage work, LUKS entry, boot observation, and recovery.
9. Administrators have Tailscale access to mox hosts and the QDevice, strict
   SSH host keys, and key-based root access. Guests remain off Tailscale.
10. Cloudflare DNS, load balancing, origin allowlisting, certificates, and
    application-aware health checks are operated outside these scripts.
11. ZFS replication is asynchronous. The most recent successful replication
    is the recovery point after host loss; writes after it can be lost.
12. The private `/24` is connectivity, not a trust boundary. Same-subnet
    packets are switched directly and bypass the `.10` router.

### Administrator workstation: Linux or macOS

The operator scripts under `guests/`, `diagnostics/`, and `app/` run from
Linux or macOS. Everything GNU-specific in them executes on a Proxmox host or
inside a guest over SSH; the workstation side needs only:

- Bash 4.4 or newer (`mapfile -d`). macOS ships `/bin/bash` 3.2, so
  `brew install bash` and keep Homebrew's `bin` ahead of `/bin` on `PATH`.
- An `openssl` that supports `openssl passwd -6`. Apple's `/usr/bin/openssl`
  is LibreSSL and does not; `brew install openssl@3` and put it first on
  `PATH`. The creators check this before loading secrets.
- `python3` 3.9 or newer, plus the stock `ssh`, `scp`, `ssh-keygen`, `install`,
  `stat`, `wc`, `awk`, and `sha256sum`. `lib/config.sh` detects GNU versus BSD
  `stat` by probing the tool, so Homebrew coreutils is neither required nor a
  problem.

Every script connects with `BatchMode=yes` and `StrictHostKeyChecking=yes`, so
nothing can prompt mid-run. Before the first use from any workstation:

- load the SSH key into an agent (`ssh-add --apple-use-keychain` on macOS). A
  passphrase-protected key that is not in an agent fails as
  `Permission denied (publickey)` even though the host authorizes it;
- pin each mox and the QDevice once by hand (`ssh mox1 true`), comparing the
  fingerprint out of band. Unpinned hosts fail as `Host key verification
  failed`;
- point the `moxN` and `qdevice` SSH aliases at Tailscale MagicDNS names rather
  than `100.x` addresses, which change when a host is re-registered.

#### Which scripts run from macOS

Verified from macOS against the live cluster (2026-09-21, full
destroy/create/deploy/stage cycle on the test domain):

- `guests/prod/create_prod_vm.sh`, including `--dry-run`, the local root
  password hash, the `scp` uploads, and the jump-SSH alias it writes to
  `~/.ssh/config`;
- `guests/prod/destroy_prod_vm.sh`, including `--dry-run`;
- `app/deploy_hello_app_to_prod.sh`;
- `diagnostics/show_prod_vm_state.sh`;
- `guests/staging/create_staging_vm.sh`, without `--sanitizer`;
- `guests/staging/destroy_staging_vm.sh`.

Expected to work on macOS, not yet run there against a cluster:

- `guests/staging/create_staging_vm.sh --sanitizer PATH`. The sanitizer size
  check was the macOS-specific part; it is covered by unit tests only;
- `diagnostics/show_cluster_state.sh`. Its only local work is the config load
  and two `sha256sum` calls. It needs a terminal;
- `hosts/update_cluster_runtime.sh`. Local commands are portable and its unit
  tests pass on macOS. It rewrites the runtime on every node, so treat the
  first macOS run as a test, not as routine;
- `qdevice/purge_qdevice.sh`. Plain Bash plus SSH locally, read but not run.

Linux workstation only:

- `hosts/setup_proxmox_host.sh`. It needs `flock` and `ip` locally, and the
  install-ISO phase executes the amd64 Linux `proxmox-auto-install-assistant`,
  so it also cannot run on arm64 Linux.

Never run on a workstation, so the workstation OS does not matter. These
execute on a Proxmox host, inside a guest, or from a live Linux boot, and are
free to use GNU-only tools:

- `guests/prod/prepare_prod_iso.sh`, `guests/prod/build_ubuntu_autoinstall.py`;
- `guests/staging/patch_staging_clone.sh`,
  `guests/staging/patch_staging_guest_tree.py`;
- `hosts/app-ha-guest-role-hook.sh`, `hosts/inventory_disks.sh`;
- `lib/cluster_registry.py`, `lib/haproxy_routes.py`,
  `lib/sync_haproxy_routes.sh`, `lib/process_deferred_cleanup.sh`.

`lib/config.sh` is the one file that runs in both places: every workstation
script sources it, and the hosts keep their own installed copy.

When changing a workstation script, the portability traps that actually bit
were GNU `stat -c`, `realpath -e`, `date --iso-8601`, LibreSSL lacking
`openssl passwd -6`, and `chmod MODE -- FILE` (BSD `chmod` stops option
parsing at the mode, so it reads `--` as a file name). `rm -rf --`, `cd --`,
`install -m`, `mktemp -d TEMPLATE`, `paste -sd, -`, and `sha256sum` are fine on
both.

macOS limits:

- The checkout must not sit under a symlinked path (`/tmp`, `/var`, and `/etc`
  are symlinks on macOS). The loader rejects symlinked components of `env/`.
- The `0600` and "not group/world writable" checks read mode bits only. A
  macOS ACL (`chmod +a`) can grant another user read access to `secrets.env` or
  write access to the SSH `known_hosts` trust file without changing those
  bits, and is not detected. `ls -le` shows ACL entries.
- On macOS the unit suite skips the classes that exercise Proxmox-host-only
  scripts (`prepare_prod_iso.sh`, `process_deferred_cleanup.sh`), and the
  lifecycle-hook tests need `flock` and GNU `stat`. Those run only in a Linux
  test pass; [`DEVELOPMENT.md`](DEVELOPMENT.md) sets one up in a Lima VM.

## Responsibility boundaries

- **Cloudflare** owns public DNS, edge TLS, origin health, and selection among
  deliberately configured mox public origins. It never starts or moves a VM.
- **Host nftables** accepts the implemented application path by DNATing only
  public TCP 80/443 to that host's local HAProxy LXC. A separate nftables
  ruleset provides guest forwarding and masquerade.
- **HAProxy** maps an exact HTTP Host or TLS SNI to the registry-selected
  private guest IP. It does not query Proxmox placement or trigger HA.
- **The private Layer-2 VLAN** carries backend packets to whichever bridge
  currently owns the guest MAC.
- **Proxmox HA** is the only production placement/failover authority. Manual
  dual starts or HTTP-driven boot scripts would defeat fencing and quorum.
- **ZFS replication** supplies asynchronous copies to the allowed placement
  nodes.
- **Keepalived** chooses the quorate mox that owns the guest egress VIP. VIP
  ownership is independent of production placement and Cloudflare origin
  choice.
- **QDevice** votes on quorum only.
- **Tailscale** is host/QDevice administration and lower-priority Corosync
  transport only.
- **The app-ha registry** owns allocation and orchestration intent. Proxmox,
  QEMU, ZFS, and HA remain the live-state authorities that must agree with it.
- **The guest application** owns application services, origin certificates,
  app health, database consistency, and app-specific staging sanitization.

## Names, addresses, VMIDs, and MACs

Every IP address in this document is an example from one FiberState
deployment. Do not copy it into another installation. Enter the values
provided by your data center in `env/cluster.conf` and `env/moxN.conf` before
running any script. The scripts consume and validate those files; production
code must not use the example addresses as fallback defaults.

Provider inputs map to configuration as follows:

- the shared private VLAN CIDR and provider-reserved VLAN gateway go in
  `PRIVATE_SUBNET_CIDR` and `DATACENTER_PRIVATE_VLAN_GATEWAY`;
- the floating guest egress address and the mox, HAProxy, production, and
  staging allocation ranges go in `GUEST_EGRESS_VIP`, `MOX_IP_START/END`,
  `HAPROXY_IP_START/END`, `PRODUCTION_IP_START/END`, and
  `STAGING_IP_START/END`;
- each server's public IP, public gateway, netmask/prefix, private NIC
  address, and iDRAC address go in its `env/moxN.conf`;
- the provider's IPMI VPN URL and credentials are operator access
  information only and are not consumed by these scripts.

The design currently requires one shared IPv4 `/24`, but its first three
octets and all usable role allocations within it are configurable. Allocation
ranges and singleton addresses must not overlap. The mox and HAProxy ranges
must each contain at least `MAX_MOX_HOSTS` addresses.

The checked-in example uses `10.213.0.0/24` with these assignments:

```text
10.213.0.0        network address
10.213.0.1        FiberState-reserved; not used as a route or gateway here
10.213.0.2-.9     reserved; .3 has no special role
10.213.0.10       Keepalived guest-egress VIP
10.213.0.11-.20   mox1 through mox10
10.213.0.21-.30   haproxy1 through haproxy10
10.213.0.31-.50   prod1 through prod20
10.213.0.51-.200  dynamically allocated staging guests
10.213.0.201-.240 reserved for future use
10.213.0.241-.250 cluster dedicated migration/replication IPs for mox1 through mox10
10.213.0.251-.254 reserved for future use
10.213.0.255      broadcast
```

For host index `N`, addresses are derived from the configured range starts:

```text
host name                 moxN
internal FQDN             moxN.pve.internal
host private IP           MOX_IP_START+(N-1), with the private /24 prefix
fixed proxy name          haproxyN
fixed proxy IP            HAPROXY_IP_START+(N-1), with the private /24 prefix
fixed proxy gateway       MOX_IP_START+(N-1)
fixed proxy VMID          9110+N
VRRP priority             200-N
```

`PROXMOX_INTERNAL_DOMAIN` controls the private suffix and must end in
`.internal`; it is not an application or public-DNS domain. Host setup derives
all `moxN` FQDNs and prepopulates a managed `/etc/hosts` block for every
configured slot from `MOX_IP_START` and `MAX_MOX_HOSTS`. On mox hosts,
both `moxN` and its internal FQDN therefore resolve to the private VLAN.
Workstation SSH aliases may still map the short names to Tailscale addresses.

The checked-in non-secret example host definitions are:

```text
mox1
  FQDN/public:   mox1.myapp.com, 38.46.219.67/29 via 38.46.219.65
  private:       10.213.0.11/24
  HAProxy:       haproxy1, 10.213.0.21/24 via .11, VMID 9111
  VRRP/iDRAC:    priority 199, iDRAC 10.20.3.63
  storage input: one mandatory NVMe mirror pair

mox2
  FQDN/public:   mox2.myapp.com, 38.129.137.234/28 via 38.129.137.225
  private:       10.213.0.12/24
  HAProxy:       haproxy2, 10.213.0.22/24 via .12, VMID 9112
  VRRP/iDRAC:    priority 198, iDRAC 10.20.4.119
  storage input: one mandatory NVMe mirror pair
```

Only `mox1.conf` and `mox2.conf` currently exist. Adding `moxN` requires the
next contiguous file and node; an omitted index cannot be skipped. The VRRP
configuration on every installed candidate nevertheless lists all other
addresses through configured `MAX_MOX_HOSTS=10`.

Production identity is deterministic:

- the first free production name is `prodN`, where `1 <= N <= 20`;
- its address is `.30+N`, so `prod1` is `.31` and `prod20` is `.50`;
- an arbitrary application/workload purpose slug is globally unique among
  production resources, becomes a recognizable `purpose-<slug>` tag, and is
  the production creator's resume key rather than a role selector;
- the registry chooses a deterministic locally administered unicast MAC from
  the registry UUID and resource name;
- VMID comes from a live `/cluster/nextid` search and excludes all live and
  registered VMIDs plus `9111-9120`.

Staging identity is source-relative:

- the first unused positive suffix produces `prodNs1`, `prodNs2`, and so on;
- the registry allocates the first free address in `.51-.200`;
- the default route is
  `stageNprodN.<source-primary-FQDN>` (for example,
  `stage1prod1.mydomiain.com`); an operator may request one other
  exact FQDN;
- the staging MAC uses the same deterministic registry formula;
- name, VMID, IP, MAC, source, placement, route, volume, and snapshot
  dependency remain one validated identity. The QEMU `net0` MAC and guest
  netplan MAC must match it exactly.

## Traffic map and ports

### Public HTTP ingress

```text
client
  -> Cloudflare edge
  -> selected moxN public IPv4:80
  -> nftables DNAT, public IP TCP 80 -> haproxyN:80
  -> HAProxy exact, lowercased HTTP Host map
  -> registered prodN/stageNprodN private IP:80
  -> guest web server/application
```

HAProxy sets `X-Forwarded-Proto: http` and uses `option forwardfor` for HTTP.
An unknown Host receives a deliberate 404. Port 80 can support redirects and
ACME HTTP-01, but certificate issuance is outside the generic creators.

### Public HTTPS ingress

```text
client
  -> Cloudflare edge TLS
  -> new origin TLS connection to selected moxN public IPv4:443
  -> nftables DNAT, public IP TCP 443 -> haproxyN:443
  -> HAProxy ClientHello inspection and exact, lowercased TLS SNI map
  -> registered prodN/stageNprodN private IP:443
  -> guest terminates origin TLS
```

HAProxy is in TCP mode on 443. It does not decrypt origin TLS and does not use
PROXY protocol. Unknown or absent SNI is sent to a disabled reject backend.
Cloudflare headers inside the TLS stream reach the guest unchanged; the
application must trust client-IP headers only after proving the request came
through the controlled Cloudflare/HAProxy path.

### Ingress return path and proxy egress

```text
guest -> haproxyN -> moxN fixed private IP -> moxN public interface
      -> conntrack reverse-DNAT -> Cloudflare -> client
```

`haproxyN` always uses its local `moxN` `.11-.20` address as its default
gateway. Host nftables masquerades traffic sourced by that LXC through the
same public interface. This preserves the public host's conntrack return path.
The proxy must never use `.10`, `.1`, or `.3` as its gateway.

The same LXC masquerade path is used for Debian/HAProxy package egress. A
backend on another mox is reached directly across the private VLAN; HAProxy
does not route through the guest egress VIP.

### Production and staging Internet egress

```text
prodN/stageNprodN (.31-.200)
  -> default gateway 10.213.0.10
  -> current Keepalived owner
  -> nftables forward
  -> masquerade/SNAT through that owner's public IPv4
  -> Internet
```

The nftables source range is exactly `.31-.200`. Return packets are admitted
only as established/related traffic for this ruleset. The project does not
restrict guest destination ports in this NAT layer; guest/Proxmox firewall
policy must impose any egress controls. External egress allowlists need every
possible mox public IP. There is no DNAT to `.10`, no forwarding through `.3`,
and no provider-gateway dependency on `.1`.

### Administration and internal control

- Workstation to mox: TCP 22 over Tailscale for SSH and automation.
- Workstation to Proxmox UI/API: TCP 8006 over Tailscale.
- SPICE proxy, if used: TCP 3128 on the management path; public TCP 3128 is
  explicitly dropped.
- Mox to guest: TCP 22 to the guest's private `.31-.200` address. The
  workstation reaches it through a strict mox SSH `ProxyJump`.
- QEMU Guest Agent checks use the hypervisor channel, not an IP flow.
- Mox-to-mox orchestration uses strict Proxmox/root SSH trust and Proxmox API
  commands; it is not an application ingress path.

### Cluster and out-of-band traffic

- Corosync/Kronosnet link 0 uses `.11-.20` over the private 10-Gbps VLAN with
  priority 100. Proxmox owns its cluster port details.
- Corosync/Kronosnet link 1 uses mox Tailscale addresses with priority 10.
  Tailscale policy should allow the Proxmox Corosync UDP range
  `5405-5412` only among tagged mox hosts.
- QDevice voting uses TCP 5403 from mox Tailscale identities to the verified
  literal QDevice Tailscale IPv4. TCP 22 to the QDevice is administrative and
  used during setup.
- Keepalived peers exchange unicast VRRP, IP protocol 112, between mox
  `.11-.20` addresses on `vmbr-private`.
- Secure migration and ZFS replication use the dedicated
  `10.213.0.240/28` transport CIDR. Each mox host has exactly one `.241-.250`
  address in that CIDR, preventing its normal private address or floating VIP
  from also matching Proxmox endpoint selection. Proxmox manages their
  data-channel ports; this project pins the network and secure mode
  rather than exposing those channels publicly or over Tailscale.
- DNS, NTP, package retrieval, certificate issuance, and application API
  traffic leave guests through the unrestricted `.10` SNAT path unless guest
  firewall policy narrows them.
- iDRAC virtual media, HTTPS console, and boot observation travel only over
  the FiberState IPMI VPN using provider/vendor ports.

The generated first-boot host guard drops packets addressed to the host's
public IPv4 on TCP 22, 3128, and 8006. Its chain otherwise has policy
`accept`. The application ingress table adds only TCP 80/443 DNAT. These
specific rules are not a blanket public firewall: audit every listening
socket, the complete nftables/Proxmox ruleset, IPv6 exposure, and provider
firewall policy.

## Cloudflare and HAProxy

Cloudflare configuration is external and is never mutated by this repository.
For each production resource:

1. Use the public origin addresses printed by
   `create_prod_vm.sh`, with the initial/preferred node first. Additional mox
   origins are an explicit operator decision.
2. Configure TCP 443 and, while needed, TCP 80. Restrict origins to current
   Cloudflare source ranges at the provider/host edge; the generated nftables
   rule does not do that restriction.
3. Use a monitor Host header that is one of the exact registered domains.
   Health should traverse Cloudflare, local HAProxy, and the application.
   A static HAProxy-only 200 does not prove service health.
4. Keep origin TLS at Full (strict) once a valid guest certificate is present.
   HAProxy passes TLS to the guest.
5. Reserve `stageNprodN.<primary-FQDN>` names in DNS. These are first-level
   names, so `*.example.com` covers values such as
   `stage1prod1.example.com`.

The wildcard is only DNS/certificate namespace. The registry and renderer
still publish one exact Host/SNI entry per staging VM. Unknown production and
staging names never fall through to a default production backend.

Each fixed HAProxy LXC uses the configured starting size of 1 vCPU, 512 MiB
RAM, and 8 GiB `local-zfs`; increase it only from measured need. The LXC is
unprivileged, has `onboot=1` and startup order 1, and is not a Proxmox HA
resource.

Routes are generated from quorate pmxcfs by the lowest-numbered online mox:

1. Read up to 4096 schema-exact routes.
2. Validate domains, role/name/IP ranges, unique domains and IP ownership, and
   ports.
3. Render deterministic HTTP/SNI maps, HAProxy configuration, manifest, and a
   SHA-256 generation.
4. Stage and run `haproxy -c` for every online target before changing desired
   state. A staging failure leaves the previous generation desired and live.
5. Recheck coordinator identity, node membership, and quorum. Disable stale
   public DNAT on nodes that need a new generation.
6. Publish the desired generation and per-node state in pmxcfs, then commit,
   reload, and mark each target independently.
7. Re-enable public DNAT only where pmxcfs status, local generation marker,
   live HAProxy config, and quorum all agree.

A failure after desired-state publication leaves affected nodes
`reject-only` or disabled until periodic reconciliation; it does not serve a
known-stale route and does not globally roll already-current nodes back.
Host-local generation bundles retain at most four directories, protecting the
desired and active generations. pmxcfs retains eight generation metadata
records.

Cloudflare ingress failover and Proxmox VM recovery run at different speeds.
The surviving origin can be reachable while production is still restarting,
so temporary backend failures are expected. Do not couple Cloudflare or
HAProxy to VM placement merely to hide that interval.

## Guest egress and VRRP

Every configured mox installs the same forwarding and SNAT rules, but only a
healthy quorate candidate may own `10.213.0.10/24`.

For `moxN`:

- state starts as `BACKUP`;
- priority is `200-N`, from 199 through 190;
- VRID is 10;
- advertisement interval is one second;
- `nopreempt` is enabled, so a recovered higher-priority node does not
  displace a healthy current owner;
- unicast peers are all other `.11-.20` addresses through
  `MAX_MOX_HOSTS`, including configured future candidates not yet joined;
- the host and Proxmox firewall rules admit only expected VRRP protocol-112
  source/destination pairs;
- takeover sends repeated gratuitous ARP: initial delay 1 second, repeat 5,
  refresh every 60 seconds, refresh repeat 2.

The readiness script runs every 15 seconds, times out after 6 seconds, falls
after 3 failures, and rises after 2 successes. It requires:

- private and public interfaces up;
- the expected public IPv4 and default route;
- `net.ipv4.ip_forward=1`;
- the guest-egress nftables table, forwarding, established-return, and
  masquerade rules;
- no later nftables or iptables forward policy that is detectably `drop`;
- local Proxmox quorum;
- successful public egress bound to that host's public address.

VRRP ownership, production placement, and Cloudflare origin selection can all
be different hosts. Conntrack and public source addresses are not
synchronized, so established guest egress sessions usually reset on VIP
takeover. New sessions work after VRRP and ARP convergence.

## Corosync, quorum, and QDevice

Corosync is created with:

```text
link 0: mox private .11-.20, priority 100
link 1: mox Tailscale IPv4, priority 10
```

`/etc/pve/datacenter.cfg` is reconciled to:

```text
migration: secure,network=10.213.0.240/28
replication: secure,network=10.213.0.240/28
```

Tailscale link 1 can preserve cluster communication during a private-link
failure, but it does not replace the failed migration, replication, VRRP,
backend, or guest network.

QDevice parity is mandatory:

- odd Proxmox node count: QDevice absent; expected and total votes equal the
  node count;
- even Proxmox node count: QDevice present, alive, and voting; expected and
  total votes equal `node_count + 1`, with one QDevice vote and one alive
  QDevice flag for every mox view;
- before a join, remove an existing QDevice while every current node is
  online and the cluster remains quorate;
- join exactly the next `moxN` through `mox1` using the private address for
  link 0 and target Tailscale address for link 1;
- reconcile parity after the join; if a join fails without changing
  membership, the host workflow attempts to restore the prior QDevice;
- use the QDevice hostname for operator SSH, but use its independently
  verified literal Tailscale IPv4 for `pvecm qdevice setup` and TCP 5403;
- authorize the cluster RSA key on QDevice root only for certificate setup,
  then remove that exact key after the external vote is proven healthy.

QDevice is never an application node, router, gateway, proxy, storage target,
backup, migration endpoint, or guest-egress path.

The host workflow and production creator both validate this parity through ten
nodes. Before enabling HA, every selected node must report all configured mox
voters alive/voting and, for an even cluster, exactly one healthy QDevice vote.

## Host storage, LUKS, and mirrors

The reviewed source is `proxmox-ve_9.2-1.iso`, verified against
[`hosts/artifacts/proxmox-ve_9.2-1.iso.SHA256.txt`](hosts/artifacts/proxmox-ve_9.2-1.iso.SHA256.txt)
and the exact SHA-256 in `cluster.conf`. The host workflow creates a
host-specific unattended ISO with public networking, serial-selected mirror
1, administrator keys, a temporary setup key, a fresh one-time Tailscale key,
the public management-port guard, and first-boot Tailscale bootstrap.

One through five contiguous NVMe mirror pairs are supported:

- Mirror 1 is mandatory and bootable. The Proxmox installer destroys its two
  serial-selected disks, creates the raw ZFS `rpool` mirror and ESPs, and
  deliberately leaves at least 4 GiB after partition 3 for optional LUKS
  conversion.
- Each run selects iDRAC/Redfish or manual disk inventory. Redfish mode
  remotely verifies serial, capacity, and health. Manual mode uses the
  read-only `hosts/inventory_disks.sh` helper from a Linux Live environment
  and requires an exact `NVME_MIRROR_<P>_CAPACITY_BYTES_<M>` value for every
  configured serial; members of each pair must have identical byte capacity.
- Before boot-test selection, the operator chooses whether all configured
  rpool members remain unencrypted or use LUKS2. Both choices are resumable.
- LUKS conversion degrades mirror 1 one member at a time. The script records a GPT
  backup and partition start, detaches the ZFS member, expands partition 3
  into the reserved tail, clears only old ZFS signatures, and prepares LUKS2
  through a helper run at the target host console.
- Converted members are `/dev/mapper/crypt-rpool-a` and
  `/dev/mapper/crypt-rpool-b`. Each is reattached, resilvered, recorded once
  in `/etc/crypttab`, and both ESPs are refreshed.
- Selected boot testing uses three reboots in either mode. It boots from each
  mirror-1 member alone, restoring and resilvering after each simulation, then
  reboots the healthy final layout. LUKS testing begins only after both members
  are encrypted; it never duplicates the drills against the raw layout.
- Optional mirror pairs 2 through 5 must be complete and contiguous. Each pair
  is capacity-matched and added as one new top-level `rpool` mirror vdev.
  In LUKS mode its whole disks use names `crypt-rpool-mirror<P>-<M>`; in clear
  mode they remain unencrypted. Extra mirrors have no ESP.
- Every member must accept the same configured `PROXMOX_LUKS_PASSWORD`.
  `decrypt_keyctl` caches it only in the initramfs kernel keyring so one
  target-console prompt can unlock every mapper.
- Swap must remain absent. Final verification checks every configured mapper,
  backing serial, mirror relationship, crypttab keyscript, initramfs helper,
  pool health, and boot-tool state.

The LUKS password is loaded only from the mode-`0600`, Git-ignored
`env/secrets.env`. It is sent to the selected host only over SSH stdin, written
as root-owned mode `0600` `/root/.app-ha-luks-passphrase`, and consumed by
conversion/restoration helpers through cryptsetup `--key-file`; it is never a
process argument, setup-state value, log value, generated-media value,
registry value, pmxcfs value, or initramfs/crypttab key file. The temporary
file necessarily exists while the initially raw mirror is being converted;
the script tests it against any existing encrypted mapper immediately on
resume and against every configured mapper before cleanup. After every
selected LUKS boot test and restoration completes, the script deletes it and
verifies it is absent. Subsequent host boots still require one manual
passphrase entry through the target host console. A mode-`0600` LUKS header backup is
copied off-host for every member before completion, along with pre-resize GPT
backups for mirror 1 conversion.

Adding a top-level ZFS vdev expands and stripes the pool and may not be
reversible. Losing both devices in any one mirror vdev can lose the entire
pool. Removing a configured pair after it has entered `rpool`, changing disk
serial identity, ambiguous signatures, mounted devices, mismatched capacity,
or interrupted phase evidence all stop the workflow.

`--encrypt` and `--no-encrypt` select storage policy noninteractively.
`--skip-boot-tests` skips only the optional reboot drills; it does not change
the selected storage policy or skip final non-boot validation.

## Configuration, secrets, and artifacts

All three workflows use [`lib/config.sh`](lib/config.sh). It parses literal
`KEY=VALUE` data without `source` or `eval`, in this order:

1. [`env/cluster.conf`](env/cluster.conf): shared non-secret policy;
2. `env/moxN.conf`: selected host hardware and public/private identity;
3. local `env/secrets.env`: allowed secrets only, when required.

The loader rejects unknown, misplaced, inherited-as-substitute, or duplicate
keys; CRLF; shell syntax; symlinks in any path component; unsafe ownership;
world-writable non-secret files; and any secret file mode other than `0600`.
Git does not store `644` vs `664`, so a umask-`002` checkout of tracked
`cluster.conf` / `moxN.conf` is accepted. Group-writable tracked files are
not a secret-protection boundary; `secrets.env` is. Do not add a chmod init
step for those tracked files. Non-secret checks print only derived values and
a non-secret effective hash.

`cluster.conf` currently owns cluster name, private internal domain,
`MAX_MOX_HOSTS=10`, the complete address policy, fixed tags/storage IDs, the
host media path/hash, guest media HTTPS URL/hash/install mode, default
compute/storage policy, DNS, public
administrator keys, Tailscale host tag, and SSH behavior. Each `moxN.conf`
owns an optional iDRAC address, exact contiguous NVMe serial pairs, optional
manual-inventory byte capacities, public IP/gateway/prefix/MAC, private
MAC/address, and repeated derived HAProxy/VRRP values that the loader verifies
rather than trusts. `config.sh` derives each host's application-agnostic
internal FQDN.

Current creation defaults are:

```text
production  4 vCPU, 8 GiB RAM, 128 GiB local-zfs root,
            full/refreserved by default, x86-64-v2-AES, replication */15
staging     2 vCPU, 4 GiB RAM, source root size, sparse local clone
guests      vmbr-private, static /24, gateway GUEST_EGRESS_VIP,
            DNS 1.1.1.1 and 1.0.0.1
HAProxy     1 vCPU, 512 MiB RAM, 8 GiB local-zfs
```

Production and staging prompts can override their documented compute choices;
they cannot override the fixed network/storage identity model.

[`env/secrets_dot_env`](env/secrets_dot_env) is the checked-in key template;
copy it to local `env/secrets.env`, keep the template value-free, and populate
only the ignored local copy. `env/secrets.env` may contain only:

```text
IDRAC_USER
IDRAC_PASSWORD
PROXMOX_ROOT_PASSWORD
PROD_GUEST_VM_ROOT_PASSWORD
STAGING_GUEST_VM_ROOT_PASSWORD
PROXMOX_LUKS_PASSWORD
```

`IDRAC_USER` and `IDRAC_PASSWORD` are optional unless host setup is run in
iDRAC/Redfish inventory mode.

Secret values are shell-local, never printed, excluded from the public
configuration hash, and unset before SSH, Python, or builder children whenever
possible. Host setup separately computes a non-printed secret-inclusive
fingerprint so a changed destructive input requires explicit acceptance.

The Tailscale enrollment secret is deliberately not file-backed:

- a fresh, single-use, non-ephemeral `tag:proxmox-host` Tailscale auth key is
  entered silently for each host ISO and its digest is reserved against reuse.

`PROXMOX_LUKS_PASSWORD` is file-backed only in `env/secrets.env` and in the
short-lived on-host file described above. It is never embedded in an ISO.

Local artifacts are sensitive even when they contain hashes rather than
plaintext:

- `hosts/artifacts/<moxN>/*.iso`: generated host-specific installation media;
- `hosts/artifacts/<moxN>/generated/`: answer and first-boot input files;
- `hosts/artifacts/<moxN>/ssh/`: temporary setup key;
- `hosts/artifacts/<moxN>/state/`: resumable phase evidence and host key;
- `hosts/artifacts/<moxN>/logs/`: install, storage, quorum, and verification
  evidence;
- `hosts/artifacts/<moxN>/luks-headers/`: LUKS headers and GPT backups;
- `guests/prod/artifacts/` and `guests/staging/artifacts/`: mode-`0700`
  ephemeral creator workspaces.

These per-host trees are Git-ignored. The shared
`hosts/artifacts/used-tailscale-auth-key-sha256` digest denylist is
intentionally tracked so key reuse remains blocked across workstations and
fresh checkouts. Its ignored `.lock` file prevents concurrent setup processes
from both passing the check-before-append operation. Back up host recovery
material separately, encrypted, and with access controls. Git ignore is not a
backup or security boundary.

## The five main workflows

There are exactly five operator workflows in this project:

1. `hosts/setup_proxmox_host.sh` — destructively install or reconcile
   one `moxN`, create or join the cluster, and install storage, networking,
   QDevice, VRRP, HAProxy, registry, lifecycle, and cleanup services. Read
   [`hosts/README.md`](hosts/README.md). Usage:

   ```bash
   hosts/setup_proxmox_host.sh \
     --host moxN [--encrypt | --no-encrypt] \
     [--run-boot-tests | --skip-boot-tests]
   ```

2. `guests/prod/create_prod_vm.sh` — create or safely resume one
   replicated, HA-managed `prodN`. Read
   [`guests/prod/README.md`](guests/prod/README.md). Start with:

   ```bash
   guests/prod/create_prod_vm.sh --dry-run
   guests/prod/create_prod_vm.sh
   ```

3. `guests/prod/destroy_prod_vm.sh` — permanently remove one production VM
   after exact identity and dependency validation. It removes routes, HA and
   affinity, replication and target copies, QEMU config/disks, orchestration,
   and registry metadata while retaining the shared source ISO cache. Start
   with the read-only rehearsal:

   ```bash
   guests/prod/destroy_prod_vm.sh --dry-run prodN
   guests/prod/destroy_prod_vm.sh prodN
   ```

4. `guests/staging/create_staging_vm.sh` — create one
   disposable, non-HA `stageNprodN` linked clone from a registered active
   production VM. Read
   [`guests/staging/README.md`](guests/staging/README.md). Start with:

   ```bash
   guests/staging/create_staging_vm.sh --dry-run
   guests/staging/create_staging_vm.sh \
     --sanitizer /absolute/path/to/staging-sanitizer.sh
   ```

5. `guests/staging/destroy_staging_vm.sh` — interactively select and
   permanently remove one staging VM after exact identity, clone-origin, and
   snapshot-GUID validation. It removes only that guest's route, VM-owned
   disks, linked clone, one exact source-owned snapshot and its replicated
   copies, and registry allocation. Start with:

   ```bash
   guests/staging/destroy_staging_vm.sh --dry-run
   guests/staging/destroy_staging_vm.sh
   ```

The production/staging creators and destroyers have read-only
`--dry-run` modes that query live state without registry or Proxmox mutation.
Host setup has no dry run because bare-metal media, iDRAC gates, disk
conversion, and cluster membership cannot be faithfully rehearsed that way.
Use its explicit boot-test policy and read every destructive confirmation.
Creation gates use the case-sensitive token `GO`; branching installer recovery
may additionally offer `WIPE`, while production destruction requires
`DESTROY prodN`.

## Helper inventory

These are supporting diagnostics, maintenance utilities, implementation
helpers, and tests rather than additional deployment workflows:

All `test_*.py` files use Python's standard `unittest` framework plus mocked
Bash/Proxmox commands and temporary roots; they depend on the adjacent
scripts/libraries named in their descriptions.

- `diagnostics/show_cluster_state.sh` is an interactive, read-only
  workstation diagnostic for the QDevice and reachable `mox1` through
  `mox10`. It reports identity, clocks, Tailscale, firewall/listener state,
  Proxmox/Corosync quorum and links, native per-node SSH trust, QDevice
  state, services, storage/LUKS and SMART health, nftables, HAProxy
  generations, failed units, the shared registry, and a joined registered/live
  production/staging guest inventory that also flags unregistered QEMU VMs. Unreachable
  uninstalled mox slots are reported and skipped. Redirect its output when a
  durable report is useful:

  ```bash
  diagnostics/show_cluster_state.sh \
    >cluster_state.txt
  ```

- `diagnostics/show_prod_vm_state.sh prodN` is a read-only cross-layer
  production guest diagnostic. It checks registry/orchestration state, live
  QEMU identity and power, HA request and strict node-affinity, replication,
  routes, ZFS allocation, QGA, private networking, and QGA-attested strict
  root SSH, then prints a `HEALTHY` or `UNHEALTHY` verdict:

  ```bash
  diagnostics/show_prod_vm_state.sh prod1 \
    >prod1_state.txt
  ```

- `qdevice/purge_qdevice.sh` is an intentionally destructive maintenance tool
  for teardown or clean-room retesting. It first removes the QDevice through
  the supported Proxmox cluster command and refuses to purge the external host
  if cluster-side detachment cannot be proven. It then removes the temporary
  Proxmox SSH key, QNetd TLS/NSS identity, Corosync/QDevice packages, services,
  account, and package-specific state from the dedicated QDevice. It does not
  run `apt autoremove` or erase general journals. Read
  [`qdevice/QDEVICE_MANUAL_SETUP.md`](qdevice/QDEVICE_MANUAL_SETUP.md) and its
  exact `GO` destructive confirmation before running:

  ```bash
  qdevice/purge_qdevice.sh [qdevice-host] [proxmox-host]
  ```

- `hosts/setup_proxmox_host.sh` is the host setup workflow. It depends on
  `lib/`, layered `env/` configuration, either iDRAC/Redfish or a manual Live
  Linux disk inventory, Tailscale, the reviewed Proxmox ISO, and Proxmox
  cluster tools.
- `hosts/app-ha-guest-role-hook.sh` is the generic QEMU lifecycle hook installed
  on every mox node. It depends on installed `config.sh`,
  `cluster_registry.py`, Proxmox/QEMU/ZFS tools, and the deferred-cleanup
  service; it protects production starts and rejects unsafe staging starts.
- `hosts/test_setup_proxmox_host.py` tests host safety, QDevice parity, resume,
  storage, and VRRP invariants without installing a host.
- `hosts/test_app_ha_guest_role_hook.py` tests fail-closed production/staging
  admission and cleanup behavior against synthetic Proxmox and registry state.
- `guests/prod/prepare_prod_iso.sh` is installed on every mox and atomically
  downloads or reuses the hash-addressed Ubuntu source cache before invoking
  the host-installed `build_ubuntu_autoinstall.py`; it depends on `curl`,
  Python, and `xorriso`.
- `guests/prod/build_ubuntu_autoinstall.py` verifies a configured compatible
  Ubuntu source and builds the answer-file-driven per-VM installer when
  `PROD_GUEST_OS_INSTALL_MODE=ubuntu-autoinstall`.
- `guests/prod/test_create_prod_vm.py` tests installer rendering, allocation,
  resume, SSH, HA, and destructive guards for production creation.
- `guests/staging/patch_staging_clone.sh` is the root-only host wrapper for
  offline clone validation, mounting, and teardown. It is invoked by the
  staging creator and depends on ZFS block-device, filesystem, and Proxmox
  tools plus `patch_staging_guest_tree.py`.
- `guests/staging/patch_staging_guest_tree.py` performs descriptor-relative,
  no-symlink guest-tree rewrites for identity, MAC-bound networking, SSH keys,
  Tailscale removal, and the optional pre-network sanitizer.
- `guests/staging/test_staging_vm.py` tests staging allocation, source-snapshot
  dependency records, safe offline patching, rollback, and read-only dry-run.
- `app/deploy_hello_app_to_prod.sh` deploys a minimal NGINX HTTPS hello-world
  site onto one active registry `prodN`. It uses the guest's workstation SSH
  alias, installs NGINX if needed, writes a Cloudflare Origin CA certificate
  and key, and configures the registered primary/alias names plus every
  default `stageNprodN.<primary-domain>` hostname so a later staging clone
  inherits a usable site. It is a sample application for platform
  smoke-testing, not a fifth platform workflow. Real applications still use
  their own deployment tooling:

  ```bash
  app/deploy_hello_app_to_prod.sh
  ```

`lib/` is the shared orchestration layer:

- `lib/config.sh` strictly parses and validates `env/` without evaluating it,
  derives mox/HAProxy addressing, and supplies strict SSH/ProxyJump helpers; it
  depends on Bash and ordinary validation/SSH utilities.
- `lib/cluster_registry.py` owns schema-validated, secret-free pmxcfs metadata,
  deterministic IP/MAC allocation, mutation locking, cleanup records, ingress
  status, and reconciliation; it uses the Python standard library and, for
  live reconciliation, `pvesh`.
- `lib/haproxy_routes.py` validates exact registry routes and renders one
  deterministic reject-by-default HAProxy generation using the Python
  standard library.
- `lib/sync_haproxy_routes.sh` retrieves routes through a coordinator,
  validates every online target before publishing desired state, and keeps
  stale or partially committed targets fail-closed until reconciliation; it
  depends on `config.sh`, the registry/renderer, SSH, `pvesh`/`pct`, `jq`,
  `tar`, nftables, and HAProxy inside each LXC.
- `lib/process_deferred_cleanup.sh` is the bounded, idempotent cleanup worker
  run by the installed systemd timer; unsafe or unreachable work stays
  pending. It depends on `config.sh`, the registry and route synchronizer, plus
  Proxmox, QEMU, ZFS, and block-device inspection tools.
- `lib/test_shared_libs.py`, `lib/test_haproxy_routes.py`, and
  `lib/test_process_deferred_cleanup.py` test configuration, pmxcfs semantics,
  registry/IPAM, route rendering, transactions, and cleanup guards.
- [`lib/README.md`](lib/README.md) documents direct library interfaces.

`env/` is the only repository configuration layer for these workflows:
`cluster.conf` contains shared non-secret policy, each `moxN.conf` contains
non-secret host/hardware inputs, and the local Git-ignored `secrets.env` holds
only the allowed setup/console secrets at mode `0600`. All workflows depend on
`lib/config.sh`; never source these files directly, commit secret values, or
copy secrets into pmxcfs.

`hosts/artifacts/` contains the reviewed source-ISO checksum and ignored
per-host generated media, state, logs, setup keys, GPT backups, and LUKS header
backups. Keep those sensitive recovery artifacts off Git and back them up
securely.

## Required operational sequence

The order matters. Do not create guests before host and cluster convergence.

### 1. Confirm provider and external prerequisites

- Verify the FiberState private VLAN is shared Layer 2 at 10 Gbps.
- Verify every public IP, gateway, prefix, public NIC MAC, private NIC MAC,
  and iDRAC address against the provider inventory.
- Connect the iDRAC/IPMI VPN and Tailscale.
- Verify the QDevice is independent, patched, reachable by its Tailscale name,
  and reports the one literal Tailscale IPv4 resolved by the workstation.
- Put the reviewed Proxmox VE 9.2-1 source ISO at its absolute path in
  `cluster.conf`. Configure a public HTTPS production guest OS ISO URL, its
  independently reviewed hash, and either `ubuntu-autoinstall` or `manual`
  install mode.
- Arrange strict workstation SSH trust for `moxN` names and back up the local
  artifacts location securely.

### 2. Prepare and validate configuration

Create local mode-`0600` `env/secrets.env` from
[`env/secrets_dot_env`](env/secrets_dot_env), then validate one host without
mutation:

```bash
lib/config.sh \
  --check --host mox1 --require-secrets
```

Review `cluster.conf`, then each `moxN.conf`. The first host must be `mox1`;
the next must be `mox2`, and so on. Do not pre-create a non-contiguous cluster.

### 3. Install hosts in contiguous order

Run the host workflow first for `mox1`, then for `mox2`, and later one node at
a time:

```bash
hosts/setup_proxmox_host.sh --host mox1 --run-boot-tests
hosts/setup_proxmox_host.sh --host mox2 --run-boot-tests
```

Keep every existing node online while adding another. Enter the LUKS
passphrase through iDRAC when a boot requires it; conversion helpers use the
temporary root-only key file and require only `GO`. Preserve every exported
header. If a phase fails, correct the condition and rerun the identical
command; do not skip the phase marker or manually improvise the next step.

After each join, verify contiguous membership, exact QDevice parity, private
migration/replication, VRRP peers, local HAProxy, shared tools, timers, and
registry health before proceeding.

### 4. Create a generic production VM

```bash
guests/prod/create_prod_vm.sh --dry-run
guests/prod/create_prod_vm.sh
```

Choose at least two online placement nodes with enough CPU, RAM, and
`local-zfs` capacity. Complete console/QGA/SSH verification, every initial
replication, the strict HA rule, and route synchronization before application
handoff.

### 5. Deploy the application and external ingress

The generic creator intentionally stops at an empty Ubuntu application host.
Deploy the intended application, origin TLS certificate, and health endpoint
over the private address. Then configure Cloudflare origins/DNS/health checks
and verify both public paths.

For a first HTTPS smoke test after `create_prod_vm.sh` finishes,
`app/deploy_hello_app_to_prod.sh` installs a minimal NGINX hello-world site on
one active `prodN`, including `/healthz` and the registered
production/staging hostnames. It requires a working `ssh prodN` alias from
production creation, `curl` on the guest, and a Cloudflare Origin CA
certificate covering the primary domain and `*.<primary-domain>`. Real
applications still use their own deployment tooling after the generic VM is
ready.

### 6. Create staging only after production is healthy

First supply an idempotent app-specific sanitizer or choose link-down:

```bash
guests/staging/create_staging_vm.sh --dry-run
guests/staging/create_staging_vm.sh \
  --sanitizer /absolute/path/to/staging-sanitizer.sh
```

Validate the clone's application data and new SSH host key before use.
Remember that production can evict it without preserving staging runtime
state.

### 7. Operate and drill the complete system

Monitor quorum, QDevice, VRRP, routes, HAProxy, Cloudflare origins,
application health, replication age/failures, pool health/capacity, LUKS
header backups, and pending cleanup. Exercise planned and unplanned recovery
before depending on the platform.

## Host workflow in detail

`hosts/setup_proxmox_host.sh` is the host workflow. It is destructive and
resumable:

1. **Load and fingerprint inputs.** It strictly loads `cluster.conf`,
   `moxN.conf`, and `secrets.env`; derives fixed addressing; verifies
   contiguous mirror pairs; computes disk-layout and secret-inclusive setup
   fingerprints; and requires explicit acceptance if stored setup inputs
   changed.
2. **Discover hardware.** In iDRAC mode, Redfish inventories the target before
   mutation. In manual mode, the workflow validates configured exact byte
   capacities previously gathered by `hosts/inventory_disks.sh` from a Linux
   Live environment. Both paths validate serial-selected pairs and enough
   reserved tail space for mirror-1 LUKS conversion.
3. **Build host media.** It verifies the source hash; obtains the reviewed
   `proxmox-auto-install-assistant`; creates an unattended answer and
   before-network first-boot script; reserves a never-reused one-time
   Tailscale key digest; and validates/inspects the generated ISO.
4. **Install on the target host.** The operator attaches the generated ISO
   through iDRAC virtual media or other target-supported boot media, confirms
   the exact two mirror-1 serials to erase, watches the install and reboot,
   removes the media, and confirms a visible login prompt. First boot installs
   the public management-port guard, administrator/setup keys, and Tailscale.
5. **Bootstrap and configure storage.** The workstation reaches only the
   attested Tailscale address with its setup key, verifies host and disk
   identity, installs prerequisites, applies the selected clear or LUKS policy,
   and adds configured whole-disk mirror vdevs. LUKS mode converts mirror 1,
   verifies the shared passphrase, installs `decrypt_keyctl`, refreshes boot
   metadata, and exports headers. Selected boot drills test each member and
   the restored final state.
6. **Build the private plane.** It resolves public/private physical NICs by
   MAC, requires 10-Gbps carrier, creates `vmbr-private`, and stops for a
   provider-console/peer `PRIVATE VLAN VERIFIED` gate.
7. **Create or join the cluster.** `mox1` runs `pvecm create`; every later
   contiguous node joins through `mox1`. Link 0 is private, link 1 is
   Tailscale. The workflow reconciles secure migration/replication and
   QDevice parity only while all configured nodes are online.
8. **Install routing and orchestration.** It installs guest SNAT/VRRP,
   distributes the shared app-ha libraries and generic lifecycle hook to all
   online mox nodes, initializes the registry once, installs cleanup and route
   timers, creates this host's fixed HAProxy LXC, and converges registry
   routes.
9. **Verify the end state.** It checks serial-selected mirror membership and
   ZFS health in both modes; LUKS mode additionally checks encrypted mappers,
   crypttab/initramfs, and off-host headers. It also verifies no swap, boot
   state, private address, Tailscale, quorum, HAProxy, nftables, VRRP, hooks,
   and timers.

Important installed host-local paths and units include:

```text
/usr/local/lib/app-ha-proxmox/lib/config.sh
/usr/local/lib/app-ha-proxmox/lib/cluster_registry.py
/usr/local/lib/app-ha-proxmox/lib/haproxy_routes.py
/usr/local/lib/app-ha-proxmox/lib/sync_haproxy_routes.sh
/usr/local/lib/app-ha-proxmox/lib/process_deferred_cleanup.sh
/usr/local/lib/app-ha-proxmox/env/cluster.conf
/var/lib/vz/snippets/app-ha-guest-role-hook.sh

app-ha-host-guard.service
app-ha-guest-egress.service
keepalived.service
app-ha-haproxy-ingress.service
app-ha-haproxy-route-sync.service
app-ha-haproxy-route-sync.timer
app-ha-deferred-cleanup.service
app-ha-deferred-cleanup.timer
```

The hook is stored on local snippet storage on every host because guests may
move; it is not a custom pmxcfs registry file.

### Updating the deployed cluster runtime

After changing the registry, HAProxy renderer, deferred cleanup worker, or
lifecycle hook in this checkout, update their installed copies before running
a guest workflow:

```bash
hosts/update_cluster_runtime.sh --dry-run
hosts/update_cluster_runtime.sh
```

The updater requires every configured Proxmox node to be online and directly
reachable from the workstation. It stages and validates the full bundle on
all nodes before making changes, atomically replaces the installed files and
local snippet, synchronizes HAProxy routes, verifies hashes and registry
readability, and restores the prior files if the update fails. It does not
restart VMs or require existing guests to reattach their hook.

Host setup does not create production VMs, replication jobs, HA resources, or
Cloudflare configuration. Those belong to later workflows.

## Production workflow in detail

### Preconditions and input

`create_prod_vm.sh` runs from an administrator workstation and accepts:

```text
--dry-run
--install-timeout-seconds N       default 10800
--qga-timeout-seconds N           default 900
--replication-timeout-seconds N   default 14400
```

It requires a quorate expected cluster, at least two online contiguous nodes,
Proxmox VE 9.2+, amd64, `local-zfs`, disk-backed `local` ISO storage,
executable local lifecycle snippets, strict SSH trust, a reviewed
guest OS HTTPS source URL/hash, and the installed
registry/synchronizer/ISO preparation tools.

The script prompts for:

- an arbitrary application/workload purpose slug, which becomes the
  `purpose-<slug>` Proxmox tag and is the durable resume key; it does not
  select production versus staging, because the chosen creator determines
  that role;
- at least two HA placement nodes and an initial/preferred node;
- primary FQDN and zero or more exact aliases;
- vCPU, RAM in whole GiB, root-disk GiB, and sparse (default) or
  full/refreserved ZFS. Sparse lets a guest `fstrim` return space to `rpool`;
  with full allocation the host's disks cannot later be decommissioned with
  the BMAC scripts;
- replication interval in minutes;
- optional local regular file named exactly `startup.sh`;
- final VM network enabled/disabled policy.

It records the production domain used to derive first-level
`stageNprodN.<primary-FQDN>` staging names. It live-checks a candidate VMID,
then one cluster-wide registry
mutation reserves `prodN`, an address from `PRODUCTION_IP_START` through
`PRODUCTION_IP_END`, a deterministic MAC, VMID, purpose, domains, placement,
and specification.

### Root credentials and autoinstall

`PROD_GUEST_VM_ROOT_PASSWORD` is converted immediately to SHA-512 crypt with a
stable cluster-scoped salt and then unset along with the rest of the loaded
secret layer. The stable non-secret salt lets a resume compare the installed
shadow entry. Plaintext and the password hash are never returned from the
guest; QGA returns only a SHA-256 digest for comparison.

The per-VM ISO embeds the hash and exactly the two configured administrator
public keys. Root SSH is key-only (`prohibit-password`); the password is for
the Proxmox console. The ISO is built as mode `0600` on the selected initial
Proxmox node, replaying the source BIOS/UEFI metadata and updating its media
manifest.

The host-side preparation flow:

- requires disk-backed Proxmox `dir` storage with ISO content;
- downloads only from a public HTTPS `.iso` URL and verifies the configured
  SHA-256;
- caches the vanilla source as the root-only
  `/var/lib/app-ha-proxmox/iso-cache/<sha256>.iso` with an adjacent
  `.sha256` file;
- verifies cached bytes before every use and reuses a matching hash regardless
  of URL;
- creates the customized per-VM ISO directly in Proxmox ISO storage, so only
  the small mode-`0600` request crosses the workstation SSH path;
- retains the vanilla source cache while detaching and deleting per-VM media
  only after confirmed installation.

`PROD_GUEST_OS_INSTALL_MODE` defaults to `ubuntu-autoinstall`, which requires
a compatible Ubuntu live-server source and preserves strict root-password-hash
attestation. `manual` attaches a verified unchanged copy and prints the exact
systemd-Linux hostname/network/QGA/OpenSSH/root-key contract the operator must
configure before poweroff. Manual mode cannot embed `startup.sh`, does not
attest the exact console-password hash, and does not support Windows or an OS
that cannot satisfy that Linux contract.

### VM contract

The production VM is created stopped and outside HA with:

- q35, OVMF, a fresh 4-MiB EFI-vars disk, pre-enrolled Secure Boot keys, and
  the Microsoft 2023 certificate/KEK set (`ms-cert=2023k`);
- one socket, selected cores/RAM, ballooning off, and the configured
  `x86-64-v2-AES` CPU;
- one `scsi0` `local-zfs` data disk with discard, iothread, SSD, and
  replication enabled;
- one VirtIO `net0` with the registry MAC, `vmbr-private`, and Proxmox
  firewall flag;
- QGA with cloned-disk trim, generic production and purpose tags, the generic
  lifecycle hook, `onboot=0`, and installer CD-ROM first in boot order.

Ubuntu uses direct storage layout, exactly one direct ext4 root, a fixed
`.31-.50/24` address on interface `lan0`, `.10` gateway, configured DNS,
OpenSSH, QGA, and no Tailscale.

An optional `startup.sh` is limited to 256 KiB, must begin with a shebang, and
runs as root before `network-pre.target`. Its digest and presence are part of
the immutable orchestration contract. The script must be idempotent: a failed
run retries next boot; the service stops rerunning only after
`/var/lib/production-startup/completed` is written.

### Durable install and resume

The production creator takes a per-resource pmxcfs orchestration lease and
renews it before long operations. Durable phases are:

```text
unstarted
media-attached
installer-started
installed-confirmed
guest-verified
network-finalized
```

Legacy records can start as `unknown`. A missing VM is created only when the
resource is metadata-empty `reserved` and the durable phase is `unstarted`.
An `unknown` legacy reservation additionally requires exact `WIPE`.

The installer must power off. An interrupted `installer-started` phase
requires console inspection and either `GO` to preserve the completed install
or exact `WIPE` to rerun it; the script does not infer success from a stopped
VM. It detaches/deletes media only after durable installation confirmation.

The creator then starts the installed VM and uses QGA to attest hostname,
root-shadow digest, and its generated Ed25519 SSH host key. That exact key is
placed in a temporary strict known-hosts file before root SSH is attempted
through the mox jump host. It verifies QGA/SSH, static IP/default route/DNS,
key-only SSH, startup completion, and absence of Tailscale. The network link
stays enabled throughout install and attestation; only then is requested
`link_down` applied and recorded as `network-finalized`.

Unexpected name, VMID, node, disk identity/size/options/allocation, EFI/Secure
Boot, NIC/MAC/bridge, CPU/RAM, tags, hook, boot order, HA, replication,
placement, route, or SSH state causes a closed failure. A missing previously
provisioned VM is never silently recreated.

### Replication, placement, and HA ordering

After guest verification:

1. Attach the production lifecycle hook only after installer/QGA boots are
   complete, record owner/root volume, and transition through `provisioning`
   to `replicating`.
2. Create or validate exactly one `pvesr` job from the current owner to every
   other placement node.
3. Reconcile schedules, record each prior `last_sync`, trigger each job, and
   require a newer successful result with zero failures, no active PID, and
   no error before the timeout.
4. Require owner plus healthy targets to equal placement exactly.
5. Verify source and every replica have the exact zvol size and allocation
   policy. Sparse, the default, means no refreservation. Full allocation uses
   `refreservation=auto` and requires the numeric reservation to be at least
   `volsize` (OpenZFS may include metadata overhead).
6. Recheck all placement nodes online and quorum votes from every node.
7. Add the VM as an HA resource in `ignored` state with `max_restart=3`,
   `max_relocate=1`, `failback=0`, and `auto_rebalance=0`.
8. Add exactly one strict positive PVE 9 node-affinity rule. It contains only
   this VM and every placement node, all at equal priority `1`.
9. Request HA state `started`, wait until it is started on an allowed node,
   and revalidate effective replication targets.
10. Record HA/replication metadata, enable exact routes, converge all online
    HAProxy nodes, and transition `ready` to `active`.

HA is not enabled before a healthy initial replica exists everywhere.
`onboot=0` remains correct because Proxmox HA owns startup.

Every placement node must reserve enough CPU, RAM, pool capacity, and
full-allocation space to run all production resources assigned to it after
failure. Evicting staging is emergency reclamation, not a production capacity
plan.

### Failure and application handoff

Failure traps preserve the allocation, VM, volume, successful replicas, HA
resource, and rule. They release only the orchestration lease and
known-safe temporary media. Correct the fault and rerun with the same purpose.
There is intentionally no destructive production rollback.

After completion, the route exists even though no application is deployed.
An unavailable backend is expected until application code, services, origin
certificate, and health endpoint are installed. The generic creator never
deploys application code.

## Staging workflow in detail

### Unsupported storage model and source requirements

The staging workflow deliberately uses a direct ZFS clone of a production
snapshot replica. Proxmox does not support or track that as an ordinary linked
clone. The registry records the dependency, but Proxmox will not protect it
from manual snapshot/replication operations.

The source must be:

- a registry `active` production `prodN`;
- one running QEMU VM whose live owner equals registry owner;
- exactly one Proxmox HA resource in a started/enabled requested state;
- governed by exactly one enabled strict positive node-affinity rule matching
  registry placement;
- replicated by exactly one currently healthy job to each placement standby;
- on an all-online amd64 Proxmox VE 9 placement;
- q35/OVMF, exact production/purpose tags, QGA enabled with filesystem freeze,
  one NIC, one replicated `scsi0` root zvol, and one replicated EFI-vars
  volume;
- direct-partition Ubuntu with exactly one mounted ext4 root and no LVM,
  dm-crypt, MD, RAID, or other mapped root.

A QGA-frozen VM snapshot improves filesystem consistency but is not an
application-consistent database backup. Quiesce and validate the application
as required.

The creator automatically chooses the lowest-numbered online replication
target different from the production owner. That standby is the fixed staging
placement. It refuses the allocation when the host has reached
`MAX_STAGING_VM_COUNT_ON_THIS_HOST`; the registry repeats this check atomically
while holding its allocation lock.

### Allocation, snapshot, and clone

The workstation accepts:

```text
--dry-run
--sanitizer PATH
--cores N
--memory-gib N
--replication-timeout-seconds N   default 14400
--start-timeout-seconds N         default 300
```

It prompts for source, optional exact domain override, compute sizing, initial
NIC link-down, an optional sanitizer, and finally whether to start.

One mutation reserves a fresh `stageNprodN`, `.51-.200` IP, deterministic MAC,
live-free VMID, one standby placement, inherited purpose/storage/replication
policy, and exact `stageNprodN.<source-primary-FQDN>` domain. The registry
chooses the lowest unused `N` in
`1..MAX_STAGING_VM_COUNT_ON_THIS_HOST`; callers cannot provide a stage number.

Creation then:

1. Transitions to `snapshotting`.
2. Records a unique
   `stg-base-stageNprodN-<short-UTC>` intent in pmxcfs, including the source owner,
   root volume, all snapshotted source volumes, one dependent resource, and
   refcount 1, before touching Proxmox.
3. Runs a named `qm snapshot` with `vmstate=0`; raw target-only snapshots and
   the `__replicate_` namespace are forbidden.
4. Transitions to `replicating`, triggers every production replication job,
   and waits for a fresh successful sync.
5. Resolves every snapshotted ZFS volume on every placement node and requires
   each volume's numeric snapshot GUID to be identical everywhere.
6. Marks the dependency verified, transitions to `cloning`, and runs direct
   local `zfs clone` of the standby root snapshot with `refreservation=none`.
7. Creates a stopped q35/OVMF QEMU shell with fresh EFI-vars disk, source CPU
   type, selected compute, one registry-MAC NIC, exact staging/evictable/
   purpose tags, generic lifecycle hook, QGA, `onboot=0`, and no HA resource.
   The cloned data disk is still unattached.

The production owner is rechecked throughout. Owner movement during the
guarded window aborts and starts rollback.

### Offline identity and sanitizer gate

`patch_staging_clone.sh` acquires one host-wide offline-patch lock for up to
300 seconds. Before mounting, it proves the zvol is not referenced by any VM,
mounted, or open. It maps direct partitions, rejects unsupported block layers,
mounts each ext4 candidate read-only with `noload,nodev,nosuid,noexec` to find
exactly one Ubuntu root, then remounts that root read-write with
`nodev,nosuid,noexec`.

`patch_staging_guest_tree.py` performs descriptor-relative `O_NOFOLLOW`
operations and atomic sibling replacements. It bounds input files, rejects
symlink or non-regular substitutions, and:

- changes hostname and `/etc/hosts`;
- removes existing netplan and installs static `lan0` matched to the exact
  registry MAC, `.51-.200/24`, `.10` gateway, and configured DNS;
- replaces only the root shadow hash with the new staging console password;
- clears machine-id, cloud-init state/logs/network fragments, DHCP and network
  leases, and existing SSH host keys;
- disables cloud-init and installs a mandatory first-boot
  `ssh-keygen -A` dependency before SSH;
- removes Tailscale state, configuration, repositories, keys, startup links,
  units/drop-ins, and masks `tailscaled.service`;
- optionally installs the operator sanitizer as a root one-shot required
  before `network-pre.target`, `network.target`, systemd-networkd,
  NetworkManager, and classic networking.

The wrapper syncs, unmounts, flushes, removes partition mappings, and proves no
mount or writer remains. A failed unmount/mapping teardown is loud and
requires manual recovery; attach permission is never granted.

Only after the offline helper succeeds does the creator attach the clone as
`scsi0` with `replicate=0`, set boot to it, revalidate the exact stopped,
non-HA one-NIC/two-disk configuration, and transition to `stopped`.

The optional sanitizer must be an absolute, regular, non-symlink Bash file no
larger than 1 MiB and must be idempotent. The guest records `started` before
invocation and `complete` afterward. An interrupted run never retries
automatically; networking stays blocked until an operator reviews the partial
run. Without a sanitizer, enabling the NIC requires exact `GO` confirmation
after displaying the unsanitized-network warning.

Generic patching does not remove application credentials, production data,
scheduled jobs, webhooks, queues, payment integrations, email, or destructive
outbound behavior. Link-down is the safe default whenever those cannot be
proven sanitized, even though the interactive script's prompt defaults to an
enabled link if the operator answers no.

### Route, start, and rollback

After offline validation, the creator enables one exact staging route and
converges HAProxy. Declining the final start leaves a valid routed, stopped
VM. If production has moved onto the staging node, the creator leaves staging
stopped. A hook-refused or timed-out optional start also preserves the valid
stopped environment.

Before route/creation commit, an error trap tries in this order:

1. stop and destroy only a VM whose exact identity/tags/disks match;
2. prove the VM absent, then destroy only the unreferenced clone with the
   expected snapshot origin;
3. delete the source-owned Proxmox snapshot;
4. trigger replication and prove every tracked snapshot copy absent;
5. clear/release the registry allocation;
6. reconverge HAProxy if a route might have existed.

If any destructive fact is unavailable or ambiguous, the registry resource is
retained as `cleanup_pending` with routes disabled. Idempotent per-node
cleanup records preserve the VM, volume, snapshot, and route identities for
the timer worker. An offline-patch failure never attaches or starts the
clone.

The retained source snapshot can pin substantial production space. Never
delete or roll it back, rebuild a replica without it, remove/change its
replication jobs, promote a node lacking the exact GUID as replication source,
or destroy the source volume while a dependent clone exists. Destroy every
clone first, then remove the snapshot and replicate its deletion everywhere.

## Lifecycle admission, failover, and cleanup

### Generic lifecycle hook

Every production and staging VM carries the same QEMU hook. The hook uses one
host-local `flock` for up to 180 seconds and classifies a role only from
anchored name plus exact required tags.

Before a production start, it:

1. proves the VM is local, HA-managed, in an allowed registry state, on an
   allowed placement node, and has exact production/purpose tags;
2. writes a mode-`0600` start reservation under
   `/run/app-ha-production-starting/<VMID>.reservation`;
3. discovers local `stageNprodN` VMs with staging and evictable tags;
4. validates every candidate before the first stop: unique registry
   UUID/VMID/name, local owner/placement, non-HA state, exact tags, exact
   `scsi0`/fresh EFI identity, verified source snapshot dependency, and
   matching origin and GUID;
5. force-stops (`qm stop`, a hard power-off) every candidate that is running,
   then verifies that all of them are stopped;
6. for each candidate, revalidates it, disables its route, and commits a
   durable cleanup plan: local `destroy-vm` and `destroy-volume` records plus
   snapshot and route cleanup on every node;
7. returns so production starts, and leaves the production reservation
   through `post-start`.

The hook does not destroy anything itself. Stopping frees the CPU and RAM that
production needs; destroying the QEMU config and linked clone is slower, so
the local deferred-cleanup worker does it after production has started. The
worker defers local `destroy-vm` and `destroy-volume` work while any
production-start reservation on that node is younger than 900 seconds, and
`post-start` removes the reservation and triggers the worker.

An unreachable peer or remote snapshot does not delay the production start;
remote work remains queued. Ambiguous staging identity, HA membership, disks,
origin, GUID, registry, or cleanup-plan commit blocks the production start
rather than queuing destruction of an uncertain resource.

Before a staging start, the hook:

- refuses while any local production-start reservation is younger than 900
  seconds or belongs to a running production VM;
- removes only an expired reservation for a stopped production VM;
- refuses if any correctly tagged local production VM is running;
- revalidates the staging registry, QEMU, non-HA, disk, clone, snapshot, and
  state contract;
- permits only registry state `ready` or `stopped`.

`post-start` and `post-stop` remove the matching production reservation.
`post-start` also triggers deferred cleanup without blocking the VM.

### Full host-failure sequence

If a mox hosting production fails:

1. Corosync and QDevice determine quorum and fence/HA authority.
2. A healthy quorate Keepalived peer can own `.10`; existing NAT sessions
   normally reset.
3. Cloudflare independently marks the failed host/HAProxy origin unhealthy and
   directs new ingress to another origin.
4. Proxmox HA chooses an eligible node with a replica.
5. The production pre-start hook force-stops any verified disposable
   staging on that destination and queues its destruction and cross-node
   cleanup for after production starts.
6. Proxmox starts production from the most recent successful replica with the
   same VMID, MAC, private IP, and route.
7. The private switch relearns the production MAC. Both fixed proxies continue
   targeting the same private IP; no route-generation change is needed.
8. Application health eventually restores the Cloudflare origin path.

The production RPO is replication age; the recovery-time objective includes
failure detection, fencing/quorum, staging eviction, VM boot, Layer-2/ARP
convergence, application start, HAProxy checks, and Cloudflare health
intervals.

For planned migration, first stop and validate every staging VM on the
destination. The lifecycle hook is a pre-start safety net, not a migration
planner.

### Deferred-cleanup controller

`app-ha-deferred-cleanup.timer` runs about every two minutes with up to 30
seconds random delay. The oneshot worker defaults to a 120-second bounded
window; `--max-seconds` accepts 5 through 900. One local lock prevents
concurrent workers, and the lifecycle lock serializes only destructive local
VM/volume work. Local `destroy-vm` and `destroy-volume` records also wait
while a production-start reservation younger than 900 seconds exists on the
node, so staging evicted by a production start is destroyed only after that
production VM has started.

Pending local records are ordered:

```text
remove-route
destroy-vm
destroy-volume
delete-snapshot
remove-replication
```

The worker acts only while the resource UUID still matches and the resource
is `cleanup_pending` with routes disabled:

- `remove-route` batches a quorum-gated HAProxy convergence and marks
  completion only after success;
- `destroy-vm` requires exact staging config/tags/disks, non-HA status, safe
  clone identity, force-stops if required, and destroys only the config;
- `destroy-volume` requires VM absence, expected local ZFS volume/origin/GUID,
  no mounts/openers, and no reference from any QEMU config;
- `delete-snapshot` waits for local payload cleanup, no clone dependencies,
  exact source/resource/description/GUID, and an online current production
  owner; it removes Proxmox snapshot metadata on the owner, triggers
  replication, and waits for all assigned copies to disappear;
- `remove-replication` can remove only a staging job with exact VM/job
  identity. Production replication removal is deliberately left pending for
  operator review.

Unsafe, unreachable, timed-out, or ambiguous work remains pending. Completion
is written through typed `reconcile --observed ... --apply`. Once every
required VM, volume, snapshot, and route record is complete, the worker
archives and releases the staging resource. A worker restart can finish from
completed records without repeating destructive work.

Cleanup is queued only for nodes actually configured in the live Proxmox
cluster, including configured nodes that are temporarily offline. Future
uninstalled `moxN` slots never receive records, so they cannot prevent final
release.

## Guest and management access

Mox administration uses Tailscale:

```bash
ssh root@mox1
# Open https://mox1:8006 over Tailscale.
```

Public TCP 22/8006/3128 must remain unreachable. Verify host fingerprints out
of band, preferably through iDRAC during bootstrap. The shared SSH helpers
require `BatchMode`, `ClearAllForwardings`, strict host-key and host-IP checks,
an explicit non-writable known-hosts file, timeouts, and no global known-hosts
fallback.

Production and staging administration uses the registry private IP through a
mox jump:

```bash
ssh -J root@mox1 root@10.213.0.31
```

Use the actual registry IP and an explicit verified guest host key. The
production creator obtains and attests its first key through QGA; staging
regenerates keys on first boot, so an operator must record and verify the new
key before trusting it. Both roles retain exactly the configured administrator
public keys and key-only root SSH. Console passwords are for Proxmox console
recovery.

Do not install Tailscale in a guest. Guest management ACLs terminate at mox;
the mox `ProxyJump` and private guest address are the supported path.

iDRAC remains the only supported path for host LUKS entry and recovery when
the host OS or Tailscale is unavailable.

## Exact pmxcfs inventory

### Custom app-ha hierarchy

The complete custom persistent hierarchy is rooted at the `cluster.conf`
value `/etc/pve/priv/app-ha`. Every persistent JSON record uses
`schema_version: 1`.

```text
/etc/pve/priv/app-ha/
├── policy.json
├── resources/
│   ├── prodN.json
│   └── stageNprodN.json
├── orchestration/
│   └── prodN.json
├── deferred-cleanup/
│   └── cleanup-<24-lowercase-hex>.json
├── history/
│   └── <UTC-without-colons>-<resource>-<8-lowercase-hex>.json
├── ingress/
│   ├── desired.json
│   ├── generations/
│   │   └── <64-lowercase-hex>.json
│   └── nodes/
│       └── moxN.json
├── .allocator-lock-owner.json             transient while a mutation holds lock
└── .<destination-name>.<32-hex>.tmp       transient atomic-write sibling

/etc/pve/priv/lock/
└── app-ha-registry-4ce60843a91795e6/      empty mutation lock directory
```

`4ce60843a91795e6` is the first 16 hex digits of SHA-256 over the pmxcfs-
relative state path `priv/app-ha`. If `CLUSTER_STATE_DIR` changes, the suffix
changes deterministically; the current configured path and suffix above are
exact.

### `policy.json`

`record_type: "policy"` contains:

- immutable registry UUID and creation time;
- network CIDR and `.10/24` gateway;
- fixed mox, HAProxy, production, staging, and HAProxy-VMID ranges;
- limits: max hosts 10, production 20, staging 150, and reservation TTL;
- exact production/staging/evictable tags;
- production and staging compute/storage/replication defaults;
- the allowed state-transition graph.

Writer: `cluster_registry.py init`, invoked by host setup, writes it only if
absent. An existing policy is returned and never silently replaced.

Readers: every registry command validates it; production/staging creators,
route renderer/synchronizer, lifecycle hook, cleanup worker, reconciliation,
and operators depend on the resulting policy.

Recovery note: changing `cluster.conf` does not migrate an existing policy.
Treat any mismatch as an explicit schema/policy migration, not a reason to
delete this file.

### `resources/prodN.json` and `resources/stageNprodN.json`

Each `record_type: "resource"` has these exact top-level fields:

```text
schema_version, record_type, id, allocation_id, request_fingerprint,
kind, name, index, source, vmid, ip, mac, state, purpose, domains,
placement, initial_node, owner_node, routes_enabled, spec, proxmox,
created_at, updated_at, revision
```

Nested data records the purpose/source, primary/alias/staging domains,
cores/RAM/disk/allocation/replication interval, HA nodes, replication targets,
volume, and snapshot dependency. A staging snapshot additionally records
source resource, root and all snapshotted volumes, original owner, root and
per-volume GUID maps, verification flag, sole dependent, and refcount 1.

Writers: only registry mutations write them. Production/staging creators call
allocate/update/snapshot commands; the lifecycle hook disables routes and
commits cleanup state; reconciliation can mark an expired VM-less reservation
failed; release/finalization removes the live record after writing history.

Readers: all registry listings/invariant checks, both creators, route
generation, lifecycle hook, cleanup worker, reconciliation, and operators.

Uniqueness is cluster-wide for name, VMID, IP, MAC, route domain, allocation
ID, and production purpose. Allocation IDs and request fingerprints make safe
retries idempotent.

### `orchestration/prodN.json`

`record_type: "orchestration"` stores:

```text
schema_version, record_type, resource, resource_id, install_phase,
final_network_enabled, startup_sha256, lease,
created_at, updated_at, revision
```

The lease contains nonce, owner, acquired time, and expiry. TTL must be
60-172800 seconds; the production creator computes a renewable duration from
its operation timeouts with an 86400-second minimum.

Writer/reader: only the production creator, through
`orchestration-acquire`, `orchestration-set-phase`,
`orchestration-release`, and `orchestration-get`. Generic resource release
deletes this file after archiving the resource. A different unexpired nonce
blocks concurrent orchestration; the contract cannot change on resume.

### `deferred-cleanup/cleanup-<24-hex>.json`

`record_type: "deferred-cleanup"` contains:

```text
id, resource, resource_id, node, action, target, reason, state,
attempts, created_at, updated_at, revision
```

The ID is deterministic from resource name/UUID, node, action, and target, so
enqueue is idempotent. Allowed actions are:

```text
destroy-vm       target vm:VMID
destroy-volume   target STORAGE:vm-VMID-disk-N
delete-snapshot  target vm-VMID@SNAPSHOT
remove-replication target VMID-JOB
remove-route     target prodN or stageNprodN
```

Writers: staging rollback and the lifecycle hook enqueue through
`defer-cleanup`; the cleanup worker reports explicit successful IDs through
`reconcile --apply`, which increments attempts/revision and marks completed.

Readers: cleanup worker, reconcile, resource release, staging cleanup
finalizer, registry list/get, and operators.

### `history/*.json`

`record_type: "release"` contains release time, forced flag, and the complete
final resource record.

Writers: `release` and `finalize-staging-cleanup`, immediately before deleting
the live resource. Reader: `list --record-type history`.

History is an audit trail, not an executable rollback or a backup of Proxmox
VM/storage state.

### `ingress/`

- `ingress/generations/<64-hex>.json`,
  `record_type: "haproxy-generation"`, stores generation SHA-256, bundle
  SHA-256, route count, and creation time.
- `ingress/desired.json`, `record_type: "haproxy-desired"`, stores the desired
  generation/bundle/count, every configured `mox1..moxN` in order, timestamps,
  and revision.
- `ingress/nodes/moxN.json`,
  `record_type: "haproxy-node-status"`, stores desired/applied generations,
  `pending`, `staged`, `applied`, or `reject-only`, timestamps, and revision.

Writer: the quorum-gated route coordinator through `ingress-begin` and
`ingress-mark`. Readers: `ingress-status`, every local route-reconcile timer,
the ingress service's current-generation preflight, and operators.

Each node retains a timer so the lowest-numbered online node can take over as
coordinator after failure. A healthy non-coordinator whose local generation is
already current ensures its ingress unit is active, then returns without
delegating a duplicate periodic transaction. Coordinator commits likewise
queue ingress activation on every updated target. Periodic calls wait at most
20 seconds for the coordinator lock; host setup uses
`--lock-timeout 240` because a normal two-node reconciliation can take roughly
45 seconds and must not fail merely because a timer already holds the lock.
For a new host, its timer is enabled but not started until its fixed HAProxy
LXC has been created and the first generation has been applied.

The live maps/configuration and four-generation cache are inside each
HAProxy LXC under `/etc/haproxy`; they are not pmxcfs files.

### Transient files and cluster-wide lock

Every mutation acquires the one empty pmxcfs directory:

```text
/etc/pve/priv/lock/app-ha-registry-4ce60843a91795e6/
```

Atomic `mkdir` is the lock primitive. The lock directory must remain empty.
Owner/nonce/node/PID/time diagnostics live in sibling
`.allocator-lock-owner.json` because a non-empty pmxcfs directory cannot be
used for this lock protocol. Default lock wait is 15 seconds.

Every JSON write:

1. recursively rejects secret-shaped fields and unmistakable key material;
2. serializes canonical UTF-8 JSON ending in newline;
3. enforces a 1-MiB file limit;
4. opens a non-symlink sibling
   `.<name>.<UUID>.tmp`, writes/flushed content, and atomically replaces the
   destination;
5. removes a surviving temporary on handled failure.

pmxcfs is not a normal filesystem. The implementation deliberately skips
directory/file `fsync` and ordinary `chmod` under `/etc/pve`, does not rely on
`O_EXCL`, and never replaces a non-empty directory. Reads reject symlinks,
non-regular files, malformed/oversized JSON, schema drift, and secret-shaped
data.

### Permissions

All real registry commands under `/etc/pve` require effective root. The
`/etc/pve/priv` hierarchy supplies pmxcfs root-only visibility and its fixed
permission semantics. Requested local-test modes (`0700` directories and
`0600` files) describe intent but must not be confused with conventional
`chmod` control on pmxcfs.

Do not place credentials, password hashes, private keys, API/Tailscale tokens,
LUKS material, generated media, or secret-shaped names/values in this tree.
The registry rejects them recursively. `secrets.env`, setup keys, generated
media, and LUKS headers stay outside pmxcfs.

### Retention

- release history keeps the newest 256 records;
- ingress metadata keeps the newest eight generation records while always
  retaining the current desired generation;
- each HAProxy LXC keeps at most four generation directories, protecting
  desired and active;
- deferred cleanup has a hard 4096-record limit;
- at most 512 completed cleanup records are retained, and a completed record
  is pruned only after its resource UUID is no longer active;
- current resources remain until validated release/finalization;
- production orchestration remains until its resource is released;
- desired ingress and one node-status record per configured mox are replaced
  in place.

### Recovery and reconciliation

Lock age alone never authorizes removal. If a mutation timed out:

1. inspect the lock directory and `.allocator-lock-owner.json`;
2. verify the named process/node is not still mutating and cluster quorum is
   healthy;
3. use an explicit registry invocation with `--break-stale-lock`, for example
   the idempotent `init` command;
4. let pmxcfs handle the mtime-zero, checksum-guarded, cluster-wide stale-lock
   request.

```bash
sudo /usr/local/lib/app-ha-proxmox/lib/cluster_registry.py \
  --state-dir /etc/pve/priv/app-ha \
  --break-stale-lock init
```

Never manually remove an active, non-empty, or merely old lock. A crashed
atomic write can leave a hidden `.tmp` sibling; it is ignored by record
readers. Preserve and remove it only after proving no mutation/lock remains
and the destination record is valid.

Use read-only reconciliation first:

```bash
sudo /usr/local/lib/app-ha-proxmox/lib/cluster_registry.py \
  --state-dir /etc/pve/priv/app-ha reconcile --live
```

`--live` projects safe local node/QEMU/volume facts from `pvesh`.
`--observed` can supply typed, secret-free cluster-wide HA, replication,
snapshot, route, and cleanup facts. Reconciliation does nothing unless
`--apply` is supplied. Apply can only:

- mark an expired `reserved`/`provisioning` resource failed when no VM exists;
- mark explicitly reported, known cleanup IDs completed.

It never creates, starts, stops, moves, destroys, adopts, or repairs Proxmox
resources. After quorum/metadata recovery, validate `list`, `ingress-status`,
and `reconcile --live`, then rerun route sync and the cleanup worker.

Malformed or missing current records have no generic auto-restore path.
Freeze mutating workflows, preserve forensic copies, restore only from a
known-good Proxmox/pmxcfs backup or a verified quorate source, validate every
schema/invariant, and reconcile live infrastructure before re-enabling
ingress. Release history alone is insufficient.

### Standard pmxcfs files touched or read

The custom tree above is complete. The project also interacts with these
Proxmox-owned pmxcfs records:

- `/etc/pve/datacenter.cfg`: the only standard file this project rewrites
  directly. Host setup preserves unrelated lines and replaces
  `migration:`/`replication:` with secure `10.213.0.240/28` entries.
- `/etc/pve/nodes/<moxN>/ssh_known_hosts`: Proxmox 9's per-node SSH pin,
  refreshed through `pvecm updatecerts --unmerge-known-hosts` and used with
  `HostKeyAlias=moxN` for strict private mox-to-mox operations.
- `/root/.ssh/known_hosts`: host setup preserves unrelated operator entries
  and maintains only a delimited QDevice key obtained through the trusted
  workstation connection. It does not write Proxmox's legacy merged
  `/etc/pve/priv/known_hosts`.
- `/etc/pve/corosync.conf`: written by `pvecm create/add/qdevice/remove`; read
  indirectly through `pvecm`/cluster APIs. The project never hand-edits it.
- `/etc/pve/storage.cfg`: read and, when necessary, updated through `pvesm` to
  ensure local snippet content; never hand-edited.
- `/etc/pve/nodes/moxN/lxc/<9110+N>.conf`: written/read through `pct` for the
  fixed HAProxy LXC (`9111` through `9120`).
- `/etc/pve/nodes/<owner>/qemu-server/<VMID>.conf`: written/read through
  `qm`/`pvesh` for production and staging QEMU guests; ownership path can move
  with production.
- `/etc/pve/replication.cfg`: written/read through `pvesr`/`pvesh` for
  production replication jobs.
- `/etc/pve/ha/resources.cfg`: written/read through `ha-manager`/`pvesh` for
  production HA state.
- `/etc/pve/ha/rules.cfg`: written/read through `ha-manager rules` for the
  strict production node-affinity rule.
- `/etc/pve/nodes/moxN/host.fw`: written/read through the Proxmox API for
  managed VRRP and guest-forward rules.

Proxmox owns locking, schema, and permissions for those records. Do not edit
them as app-ha JSON. The scripts set `firewall=1` on guest NICs but do not
create a complete per-guest firewall policy. The lifecycle snippet under
`/var/lib/vz/snippets`, host-local `/run` locks/reservations, local systemd
units, nftables files, Keepalived config, and HAProxy generations are not
pmxcfs.

## Security invariants

### Secrets and recovery material

- Keep `env/secrets.env` local, Git-ignored, non-symlinked, owned by root or
  the invoking user, and exactly mode `0600`.
- Keep `PROXMOX_LUKS_PASSWORD` only in `secrets.env`; host setup may stage it
  temporarily at mode `0600` on the selected rpool for conversion and tests,
  then must delete and verify removal. Never embed it in installation media,
  crypttab, initramfs, arguments, logs, state, or pmxcfs. Boot-time unlock
  remains manual through iDRAC.
- Use a new one-time Tailscale auth key for every host image and never store
  that key in configuration.
- Keep shell tracing disabled around secret handling. The executable scripts
  call `set +x`; do not wrap them in tooling that records argv, stdin, or
  environment secrets.
- Treat generated host media, setup SSH keys, console password hashes,
  orchestration workspaces, GPT backups, and LUKS headers as sensitive.
  Encrypt and separately back up required recovery material.
- Never put secrets in pmxcfs app-ha metadata. Root-only visibility does not
  make pmxcfs an approved secret store.

### Network exposure

- Public application ingress is only TCP 80/443 through each host's fixed
  HAProxy LXC. Never DNAT Cloudflare directly to production or staging.
- Public TCP 22, 8006, and 3128 are explicitly dropped for the host public
  IPv4. Audit all other IPv4/IPv6 listeners and provider firewall rules; the
  generated chain otherwise accepts.
- Restrict public 80/443 to Cloudflare source ranges where operationally
  feasible. This is not implemented by the current host script.
- Keep Proxmox SSH/UI/API, HAProxy administration, QDevice administration,
  Corosync fallback, and metrics on management paths. Do not expose HAProxy
  statistics publicly.
- Do not expose PostgreSQL or application-internal services through HAProxy
  unless a separate reviewed route explicitly requires it.
- The shared `/24` is not isolation. Apply Proxmox per-VM and guest firewall
  policy to prevent staging-to-production database/API access, guest-to-host
  management access, and unnecessary east-west traffic. The current scripts
  set NIC firewall flags but do not install this complete policy.
- Guest egress currently permits every destination/port after source-range
  validation. Add least-privilege egress controls for mail, webhooks,
  payments, cloud APIs, metadata endpoints, and other high-risk integrations.

### Identity and access

- Verify mox and guest SSH fingerprints out of band. Never disable strict
  host-key checking to make a resume work.
- Root SSH in both guest roles remains key-only. Console passwords are not an
  SSH fallback.
- Guests do not run Tailscale. Remove/mask it in every production-derived
  staging clone and verify absence before trusting the clone.
- Keep QDevice ACLs narrow: mox-to-QDevice TCP 5403 and only necessary
  administrative TCP 22. It must remain independent and must not advertise
  routes or exit-node service.
- Production and staging role tags, names, VMIDs, MACs, volumes, and registry
  UUIDs are security identities. Do not casually rename or retag a VM; the
  lifecycle hook should fail rather than infer intent.

### Ingress and application trust

- Use Cloudflare Full (strict) with a valid guest origin certificate.
- HTTP mode adds forwarding metadata; TLS mode is passthrough. Sanitize and
  trust `CF-Connecting-IP`, `X-Forwarded-For`, `X-Forwarded-Proto`, and `Host`
  only at a controlled application boundary.
- Unknown Host and SNI must remain reject-by-default. Never add a catch-all
  production fallback for staging names.
- Keep HAProxy logs sufficient for route/backend/status/timing diagnosis but
  avoid authorization headers, cookies, request bodies, tokens, and other
  sensitive content.
- An application health endpoint should test critical service readiness
  without making an optional dependency trigger unnecessary full-origin
  failover.

### Staging data

- Treat every staging zvol, snapshot, local artifact, sanitizer, and log as
  production-derived sensitive data.
- Supply an application-specific sanitizer before enabling networking. A
  generic hostname/key/Tailscale reset does not remove production secrets.
- Use different credentials, destinations, webhook endpoints, queues,
  certificates, and databases for staging.
- A source snapshot is neither a backup nor a safe long-term data-sharing
  mechanism. Retain it only while the exact clone dependency requires it.

## Failure cases and recovery

### Complete mox failure

Effect:

- its public origin, fixed HAProxy LXC, host-local staging VMs, and possible
  `.10` ownership disappear;
- Corosync/QDevice decide whether survivors are quorate;
- guest egress sessions through that host reset;
- Cloudflare and Proxmox HA recover independently;
- production restarts from the most recent replica after destination staging
  eviction.

Recovery:

- do not manually start a second copy;
- verify fencing/quorum, QDevice votes, one VIP owner, HA placement, replica
  age, staging cleanup queue, MAC/ARP convergence, backend health, and
  Cloudflare origin state;
- after restoring the host, let `nopreempt` keep the current healthy gateway
  unless a controlled move is required.

### Guest-egress owner failure without production failure

Effect: production placement and ingress remain unchanged. Existing
conntrack-dependent outbound sessions normally reset; a surviving quorate
candidate acquires `.10` and new sessions use its public IP.

Recovery: verify exactly one `.10` owner, public egress bound to that owner's
IP, and external allowlists containing the new source.

### HAProxy LXC or one public origin failure

Effect: production and `.10` placement need not change. Cloudflare should use
another healthy origin. A failed local route-current check removes public
DNAT rather than serving stale state.

Recovery: inspect LXC status, active generation marker, `ingress-status`,
route-sync service, HAProxy validation/logs, and host nftables. Restore quorum
and rerun the synchronizer; do not bypass the current-generation preflight.

### Production application or VM failure

An application process failure may not be a Proxmox HA failure. HAProxy health
can fail while the VM remains running, and application service supervision
and alerts must recover it. A stopped/crashed QEMU resource is governed by
Proxmox HA according to `max_restart=3` and `max_relocate=1`.

Never infer HA success from ping alone. Verify the complete
Cloudflare-to-HAProxy-to-application response and critical dependencies.

### Private VLAN failure

Effect: preferred Corosync link 0, migration, ZFS replication, unicast VRRP,
cross-host HAProxy backends, guest networking, and direct guest
administration are impaired. Corosync may remain up on Tailscale link 1, but
that does not restore the data plane.

Recovery: stop membership, migration, staging creation, and placement changes.
Repair Layer 2 and verify carrier/speed, direct neighbors, MTU, ARP/MAC moves,
replication, VRRP, and cross-host backend flows before resuming.

### Tailscale failure

Effect: workstation administration, QDevice path, and Corosync link 1 are
affected. The private Corosync/data plane and application ingress may continue.
Guests are unaffected directly because they do not run Tailscale.

Recovery: avoid membership changes while QDevice or management trust is
uncertain. Restore Tailscale on infrastructure, then verify QDevice and both
Corosync links.

### QDevice failure or parity mismatch

Application packets do not traverse QDevice, but even-node quorum safety is
degraded. Do not add/remove nodes, perform risky maintenance, or claim HA
readiness until every mox reports the exact healthy vote layout.

For an odd cluster, remove an unnecessary QDevice only with all nodes online.
For an even cluster, restore `corosync-qnetd`, Tailscale TCP 5403, literal IP
identity, and voting state.

### Quorum loss

pmxcfs can become non-writable, HA authority is unsafe, route synchronization
fails closed, and Keepalived readiness should withdraw `.10`.

Recover quorum through Corosync/QDevice and provider connectivity. Do not
force expected votes or manually edit replicated state as an ordinary
runbook. After quorum returns, reconcile the registry, route generation, VIP,
HA resources, and cleanup.

### Replication lag, failure, or topology drift

Effect: the achievable RPO grows; HA can recover only a successful prior
copy. Staging creation stops if any target is unhealthy or lacks the exact
snapshot GUID. A full/refreserved zvol consumes capacity on every target.

Recovery: stop risky failover/staging work, restore every placement job,
trigger and verify a fresh sync, compare owner plus targets to placement, and
check zvol size/refreservation. Never delete an apparently stale snapshot
while a staging dependency exists.

### Pool capacity or local-storage failure

Sparse production can exhaust space at runtime, so pool free space must be
monitored; full allocation can fail initially or during replication due to
reservation needs. A staging snapshot
can pin unexpectedly large changed blocks. Losing both members of one
top-level mirror can lose all striped `rpool` data.

Recovery: preserve pool evidence, stop creation/replication pressure, restore
the failed mirror according to ZFS/LUKS procedures, and validate every
replica. Staging eviction is not a substitute for reserved production
capacity.

### LUKS or boot failure

Failure causes include wrong passphrase, lost/corrupt header, wrong disk
serial, stale ESP, interrupted member conversion, or initramfs/keyscript
drift.

Use iDRAC, the off-host header/GPT backups, recorded phase evidence, exact
serial identity, and ZFS state. Never format or reattach a member based only
on a device name. Losing the passphrase and all usable headers can make data
unrecoverable.

### Interrupted host setup

The artifacts state directory records resumable phase evidence, fingerprints,
boot IDs, generated ISO path, and destructive-storage milestones. A rerun
revalidates live state. Changed configuration requires explicit acceptance;
once any phase of the three-reboot drill starts, it cannot be changed to skip
until both member tests and the final healthy-state reboot complete.

Do not delete state to advance the script. Resolve the live discrepancy or
restore the prior configuration/artifacts.

### Interrupted production creation

The creator leaves all durable resources in place and reports its phase.
Rerun with the same purpose. Exact orchestration lease/phase and live VM state
decide whether it may continue.

- Ambiguous attached media is retained.
- An installer restart requires exact `WIPE`.
- A missing non-empty or post-install VM is never recreated.
- Unexpected identity or topology is never adopted.
- A stale unexpired orchestration lease blocks a second creator until expiry
  or legitimate owner release.

### Staging clone or snapshot failure

Direct-clone risks include unsupported Proxmox lifecycle, snapshot space
pinning, owner movement, partial replication, GUID mismatch, operator
snapshot deletion/rollback, a rebuilt replica, and promotion from a copy
without the source snapshot.

Before commit, use the guarded rollback. If rollback cannot prove safe
destruction, leave `cleanup_pending` records and let the worker retry. Do not
manually delete the source snapshot before every clone and reference is gone.

### Sanitizer failure

The sanitizer marks `started` before execution. If it crashes or the host
reboots, the runner refuses an automatic second attempt and blocks required
network targets.

Inspect the clone offline or from the Proxmox console, determine what partial
actions occurred, repair or replace the staging environment, and explicitly
verify that networking remains blocked. Do not delete the marker merely to
make an unsafe non-idempotent script rerun.

### Route-generation partial commit

Before desired publication, all targets retain the prior desired generation.
After publication, a target that cannot commit or prove current is disabled
or `reject-only`; periodic local reconciliation retries through the current
lowest online coordinator.

Treat a node that changes liveness during target selection or cannot disable
stale DNAT as an incident. Restore quorum/connectivity, inspect
`ingress-status`, run route sync, and verify each node's desired/applied
generation before re-enabling it.

### pmxcfs registry corruption or stale lock

Schema, secret, symlink, size, filename/record mismatch, and invariant failures
stop all dependent automation. Follow the locking and recovery procedure in
[Exact pmxcfs inventory](#exact-pmxcfs-inventory). Do not repair JSON while an
allocator lock or creator lease is active.

### Configuration drift

The strict loader rejects unknown/misplaced keys and formula changes.
Existing registry policy is immutable. If source config, installed
`cluster.conf`, registry policy, host runtime, or mox files diverge, stop and
reconcile deliberately. Copying a new file over only one mox can split
orchestration behavior.

### Public-firewall or Cloudflare misconfiguration

Cloudflare can select a reachable HAProxy whose backend is not yet ready; a
host can also expose an unrelated listener because the generated guard is
specific rather than default-deny.

Verify Cloudflare source restrictions, monitor Host, Full (strict), wildcard
coverage, both origin paths, reject behavior for unknown names, and every
public listener from a non-Tailscale network.

## Operator checks and drills

### Configuration and registry

```bash
lib/config.sh --check --host mox1 --require-secrets

ssh root@mox1 \
  /usr/local/lib/app-ha-proxmox/lib/cluster_registry.py list
ssh root@mox1 \
  /usr/local/lib/app-ha-proxmox/lib/cluster_registry.py ingress-status
ssh root@mox1 \
  /usr/local/lib/app-ha-proxmox/lib/cluster_registry.py reconcile --live
```

Establish that every resource has one unique name/VMID/IP/MAC/domain/purpose,
that owner/HA/replication matches live placement, and that cleanup does not
remain pending without an understood reason.

### Cluster, QDevice, and data plane

```bash
ssh root@mox1 pvecm status
ssh root@mox1 pvesh get /nodes --output-format json
ssh root@mox1 cat /etc/pve/datacenter.cfg
```

Check:

1. membership is exactly contiguous `mox1..moxN` and all are online;
2. every node reports quorate;
3. odd membership has no QDevice and exact node votes;
4. even membership has one alive/voting QDevice and `N+1` expected/total
   votes;
5. link 0 is private and preferred, link 1 is Tailscale fallback;
6. migration and replication are secure on `10.213.0.240/28`;
7. all production replication jobs have recent successful syncs and exact
   placement-minus-owner targets.

### VRRP, NAT, and routes

On every mox:

```bash
ip -4 route show default
ip -4 address show dev vmbr-private
systemctl is-active keepalived app-ha-guest-egress.service
keepalived --config-test --use-file=/etc/keepalived/keepalived.conf
nft list table ip app_ha_guest_egress
nft list table inet app_ha_haproxy_ingress
nft list table inet app_ha_host_guard
```

Prove:

- exactly one node owns `10.213.0.10`;
- all other `.11-.20` peers through max 10 are listed and self is absent;
- every host retains its public default route;
- guest sources `.31-.200` are forwarded/masqueraded through the current
  owner;
- each HAProxy LXC defaults to its local fixed mox address;
- only public 80/443 is DNATed to the local LXC;
- public 22/3128/8006 is blocked;
- no address, nftables rule, Keepalived peer, route, or proxy config references
  `10.213.0.3`.

From every HAProxy LXC, validate the exact generation and reach each active
backend on TCP 80/443. From outside Tailscale, test unknown Host/SNI rejection
and public management-port closure.

### Storage

```bash
zpool status -P rpool
zpool status -x rpool
proxmox-boot-tool status
cat /etc/crypttab
lsinitramfs "/boot/initrd.img-$(uname -r)"
```

Check one expected mapper per configured member, exact serial/backing device,
correct mirror pairing, `app-ha-rpool` plus
`luks,initramfs,nofail,keyscript=decrypt_keyctl`, both mirror-1 ESPs, no swap,
healthy pool, and one non-empty off-host header backup per member.

### Guest and application

For each production resource:

- QEMU identity, strict affinity, HA state, owner, route, and replica set match
  registry;
- private IP/MAC persist across planned migration;
- static `.10` gateway and DNS work;
- root SSH is key-only through ProxyJump;
- QGA and optional production-startup completion are healthy;
- Tailscale is absent;
- both Cloudflare origins reach the application-aware HTTPS health endpoint.

For each staging resource:

- source/owner/snapshot/GUID/refcount and local clone origin match;
- VM is non-HA, `onboot=0`, exact-tagged, and fixed to one standby;
- MAC agrees among registry, `net0`, and `lan0` netplan;
- SSH host key is unique;
- Tailscale is absent/masked;
- sanitizer completion or link-down policy is proven;
- app-specific production credentials and integrations are absent.

### Required failure drills

Before production reliance, perform controlled drills for:

- VRRP takeover, session reset, and `nopreempt` recovery;
- loss of each Cloudflare origin and one HAProxy process;
- unknown Host/SNI rejection and route-sync partial failure;
- planned production migration after clearing destination staging;
- staging start refusal while production is running or reserved;
- production-start eviction of running staging plus deferred cleanup;
- QDevice loss and restoration for even membership;
- private VLAN loss with Corosync Tailscale fallback;
- full power loss of each mox, including iDRAC LUKS entry;
- production restart from the latest replica and measured data-loss window;
- MAC/ARP relearning after movement;
- sanitizer interruption with networking blocked;
- one degraded storage member and verified header/ESP recovery.

Record measured Cloudflare, quorum, HA, VM boot, application, replication RPO,
VRRP, and Layer-2 convergence times. Ping alone is insufficient.

## Tests

Run shell syntax checks from the repository root:

```bash
bash -n \
  hosts/setup_proxmox_host.sh \
  hosts/app-ha-guest-role-hook.sh \
  guests/prod/create_prod_vm.sh \
  guests/staging/create_staging_vm.sh \
  guests/staging/destroy_staging_vm.sh \
  guests/staging/patch_staging_clone.sh \
  lib/config.sh \
  lib/sync_haproxy_routes.sh \
  lib/process_deferred_cleanup.sh
```

Run the complete Proxmox unit suite:

```bash
PYTHONDONTWRITEBYTECODE=1 python3 -m unittest \
  hosts/test_setup_proxmox_host.py \
  hosts/test_app_ha_guest_role_hook.py \
  guests/prod/test_create_prod_vm.py \
  guests/staging/test_staging_vm.py \
  lib/test_shared_libs.py \
  lib/test_haproxy_routes.py \
  lib/test_process_deferred_cleanup.py -v
```

On a Mac, `dev/run-tests-in-vm.sh` runs `bash -n` on every tracked script and
every tracked `test_*.py` inside a Debian 13 Lima VM; see
[`DEVELOPMENT.md`](DEVELOPMENT.md).

Coverage by test helper:

- `hosts/test_setup_proxmox_host.py`: QDevice vote/parity parsing, current ISO
  resume, changed boot ID, destructive confirmations, serial storage
  selection, strict SSH, VRRP peers/priorities, and host safety invariants.
- `hosts/test_app_ha_guest_role_hook.py`: exact role identity, no destructive
  authority without registry/HA, staging disk/volume/GUID guards, production
  reservations, stopping every staging guest before queuing any destruction,
  the real cleanup worker deferring destruction until post-start, and remote
  deferral.
- `guests/prod/test_create_prod_vm.py`: autoinstall network/root/QGA/SSH,
  startup retry semantics, ISO hash/release/boot metadata/atomicity, contiguous
  nodes, QDevice gate, resume sentinels, exact VM/Secure Boot contract,
  argv-safe SSH, non-mutating dry-run, and absence of destructive rollback.
- `guests/staging/test_staging_vm.py`: argv-safe node execution, identity and
  sanitizer rewrites, symlink escape rejection, mapping/unmount safety,
  stopped-VM disk-reference rejection, snapshot/refcount ordering, rollback,
  and non-mutating dry-run with no secret export.
- `lib/test_shared_libs.py`: layered config provenance/modes/redaction,
  deterministic allocation, uniqueness, leases/phases, state transitions,
  snapshot dependencies, deferred cleanup, pmxcfs-compatible atomic writes
  and stale-lock handling, route limits, ingress crash recovery/retention,
  history/cleanup bounds, and safe live reconciliation.
- `lib/test_haproxy_routes.py`: route schema/ranges/uniqueness, deterministic
  maps/manifests, reject-only empty config, real `haproxy -c` when available,
  quorum and partial-commit fail-closed behavior, and fixed local ingress.
- `lib/test_process_deferred_cleanup.py`: exact VM/volume/snapshot/route
  cleanup, offline-source deferral, lifecycle-lock scope, failed route sync,
  GUID protection, first-host no-op, restart finalization, and syntax.

These tests mock destructive Proxmox, ZFS, iDRAC, QDevice, and network
behavior. They do not replace real media, 10-Gbps Layer-2, LUKS boot,
Corosync/quorum, migration, replication, HA, Cloudflare, firewall, application
health, sanitizer, or full-power-loss drills.

## Deliberate boundaries and future scripts

Only the five workflows named above exist as supported operator entry points.
The following potential future commands/functions are explicitly out of scope
and must not be inferred from registry primitives or old design prose:

- **Host removal or cluster shrink.** Host setup can create/join contiguous
  nodes; no script removes a mox, rewrites placement/replication, shrinks VRRP
  peers, or reconciles QDevice around node removal.
- **Adding storage to an existing host.** A future operator script will select
  `moxN`, validate a newly installed equal-capacity NVMe pair, LUKS-encrypt it
  with the host's shared rpool passphrase, and add it as another mirror vdev.
  The current host installer supports up to five pairs present at initial
  installation, but it is not the later expansion workflow.
- **Online production-disk growth.** A future script will grow a selected
  production zvol, then extend the guest's final ext4 partition/filesystem
  online. It must enforce a pool-allocation ceiling of 90% and account for all
  HA replicas and full refreservations before changing anything.
- **Cluster status report.** A future read-only `GetClusterStatus` workflow
  will summarize hosts, tags, quorum/QDevice, VRRP owner, HA guests,
  staging guests, IPs, placement, replication health, and pending cleanup.
- **Cloudflare automation.** No script creates DNS, wildcard records,
  certificates, load balancers, origins, health monitors, source allowlists,
  or failover policy.
- **Generic application deployment.** Production creation installs Ubuntu,
  not an application. Real applications use their own deployment tooling.
  The sample `app/deploy_hello_app_to_prod.sh` only deploys a hello-world
  NGINX site for platform smoke-testing.
- **Backups and point-in-time recovery.** ZFS replication and staging
  snapshots are not backup jobs. No script configures off-cluster backups,
  PostgreSQL PITR, restore testing, or retention.
- **Monitoring and alerting.** No script deploys metrics/log aggregation or
  alerts for quorum, QDevice, VRRP, HAProxy, Cloudflare, application health,
  replication lag, pool capacity, or pending cleanup.
- **Complete network isolation.** The code does not create a staging VLAN,
  routed guest subnets, default-deny per-guest firewall rules, or restricted
  guest egress.
- **Ceph or another storage plane.** Current production is local ZFS plus
  replication. Ceph, shared SAN, and cross-cluster recovery require a separate
  network, capacity, quorum, and failure design.
- **Active/active production or database HA.** There is one production VM per
  resource and one active owner.
- **Custom failover controllers.** Cloudflare, Keepalived, and Proxmox HA keep
  separate authority. No script should start VMs from HTTP health or make
  QDevice/HAProxy a placement controller.
- **Guest Tailscale or `.3` forwarding.** Neither is a planned extension of
  this architecture. Guest access remains ProxyJump; egress remains `.10`.
- **Non-contiguous or more-than-ten hosts.** `mox1..mox10` and the fixed
  address/VMID/priority ranges are hard policy limits.

Any future implementation must add a reviewed workflow, failure model,
permissions/locking behavior, tests, and updates to this README before it is
treated as operational.

## Documentation authority and legacy-document removal

Authority order is:

1. current executable scripts and checked-in `env/cluster.conf`/`moxN.conf`;
2. this comprehensive top-level overview;
3. scoped detail in [`hosts/README.md`](hosts/README.md),
   [`guests/prod/README.md`](guests/prod/README.md),
   [`guests/staging/README.md`](guests/staging/README.md), and
   [`lib/README.md`](lib/README.md);

Still-valid explanatory material consolidated from the removed legacy
documents includes:

- the central separation among Cloudflare origin selection, HAProxy
  hostname-to-IP routing, Layer-2 MAC location, Proxmox HA placement, QDevice
  quorum, ZFS replication, and Tailscale management;
- the critical shared-Layer-2 assumption and need to test MAC/ARP relearning
  after VM movement;
- complete normal and failure traffic paths, asymmetric-return avoidance, and
  the distinction between ingress and guest egress;
- application-aware multi-layer health checks and the warning that ping or a
  static proxy response is not an HA proof;
- independent Cloudflare and Proxmox failover timing and the expected
  temporary backend-unavailable interval;
- HTTP forwarded-header and TLS-passthrough source-address implications;
- wildcard staging DNS versus deeper wildcard certificate coverage;
- least-privilege exposure, staging isolation, observability, alerting, and
  full-host/reverse-failover drill guidance;
- fixed non-HA HAProxy sizing/placement and QDevice's deliberately narrow
  non-application role.

The former `proxmox_traffic_map.md` and
`proxmox_haproxy_ha_design.md` have been removed. Their Git history remains
available, but no current code or operator procedure depends on those
filenames.
