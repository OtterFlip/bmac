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
provides recoverable local storage on every eligible placement node. Every
production and staging VM has a fixed address slot derived from
`cluster.conf`. Cloudflare sends each request to any connected replica of the
cluster's one Cloudflare Tunnel (every mox runs one), the replica maps the
exact hostname to that fixed slot address, the shared Layer-2 VLAN locates the
guest's MAC, and Proxmox HA decides where the guest runs. Outbound, every mox
provides the same guest gateway address to the guests running on it. Those
decisions are intentionally independent. The networking design is described
in full in [`BMAC_SIMPLIFIED_NETWORKING.md`](BMAC_SIMPLIFIED_NETWORKING.md);
this document summarizes it and must agree with it.

Run repository commands from the repository root. The scripts are
interactive, fail closed, and resume only after comparing durable state with
live hardware, Proxmox, ZFS, quorum, and registry state. A recorded phase is
never sufficient evidence by itself.

Executable scripts and the files under [`config/`](../config/) win if prose ever
conflicts. The checked-in files are a FiberState example, not portable
defaults. In that example:

- guest egress uses the shared gateway `10.213.0.10`, which every mox
  provides to its own guests, not `.1` or `.3`;
- `.3` has no forwarding, NAT, gateway, or proxy role;
- no mox accepts inbound web traffic on its public IP; all public ingress
  arrives through the cluster's Cloudflare Tunnel;
- production and staging guests do not run Tailscale;
- supported host names are slots `mox1` through at most `mox10`, recorded in
  the cluster registry; slots may have gaps and freed slots are reused
  lowest first;
- hosts can be removed gracefully, or forcefully after a permanent failure
  (`scripts/user_callable/hosts/remove_proxmox_host.sh`), and production
  placement and ownership can be changed
  (`scripts/user_callable/guests/prod/change_prod_vm_placement.sh`,
  `scripts/user_callable/guests/prod/change_prod_vm_owner.sh`);
- fixed VM slots, staging FQDNs, the tunnel, and generic production creation
  are implemented;
- the former legacy design documents have been consolidated into this README
  and removed.

## Contents

- [Purpose and scope](#purpose-and-scope)
- [Required assumptions](#required-assumptions)
- [Responsibility boundaries](#responsibility-boundaries)
- [Names, addresses, VMIDs, and MACs](#names-addresses-vmids-and-macs)
- [Traffic map and ports](#traffic-map-and-ports)
- [Cloudflare Tunnel](#cloudflare-tunnel)
- [Shared guest gateway and egress fallback](#shared-guest-gateway-and-egress-fallback)
- [Corosync, quorum, and QDevice](#corosync-quorum-and-qdevice)
- [Monitoring and alerting](#monitoring-and-alerting)
- [Host storage, LUKS, and mirrors](#host-storage-luks-and-mirrors)
- [Configuration, secrets, and artifacts](#configuration-secrets-and-artifacts)
- [The main workflows](#the-main-workflows)
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
implementation and allocation policy support adding hosts in any free slot
through `mox10`, and removing them again.

The managed workload roles are:

- `prodN`: one generic production QEMU VM with a fixed slot IP and MAC,
  replicated `local-zfs` storage, a strict HA placement rule, and its primary
  and alias FQDNs in the tunnel config. It is active on one placement node at
  a time.
- `stageKprodN`: one production-derived, disposable staging QEMU VM in staging
  slot `K` of `prodN`. It is a sparse local ZFS clone on one production
  standby, is never HA-managed, has `onboot=0`, and can be evicted before
  production starts on that node. Its FQDN `stageKprodN.<prod primary domain>`
  is in the tunnel config whether or not the VM exists.
- `qdevice`: an external Corosync QDevice reached over Tailscale. It is only a
  quorum voter.

Each mox also runs host-level networking services rather than guests: the
shared guest gateway and its egress fallback, a transfer counter, and a
`cloudflared` replica of the cluster's tunnel.

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
4. At least two online, quorate mox nodes exist before creating a
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
10. One locally managed Cloudflare Tunnel exists for the cluster, created
    with `cloudflared tunnel create`, and every production domain is a
    Cloudflare zone in the same account. Cloudflare DNS records, edge
    certificates, bot protection, and application-aware health checks are
    operated outside these scripts.
11. Every mox can reach Cloudflare over outbound TCP 7844. The tunnel runs
    over HTTP/2 (TCP), never QUIC, because FiberState throttles inbound UDP.
12. ZFS replication is asynchronous. The most recent successful replication
    is the recovery point after host loss; writes after it can be lost.
13. The private `/24` is connectivity, not a trust boundary. Same-subnet
    packets are switched directly and bypass the `.10` gateway.

### Administrator workstation: Linux or macOS

The operator scripts under `scripts/user_callable/guests/` and
`scripts/user_callable/diagnostics/` run from
Linux or macOS. Everything GNU-specific in them executes on a Proxmox host or
inside a guest over SSH; the workstation side needs only:

- Bash 4.4 or newer (`mapfile -d`). macOS ships `/bin/bash` 3.2, so
  `brew install bash` and keep Homebrew's `bin` ahead of `/bin` on `PATH`.
- An `openssl` that supports `openssl passwd -6`. Apple's `/usr/bin/openssl`
  is LibreSSL and does not; `brew install openssl@3` and put it first on
  `PATH`. The creators check this before loading secrets.
- `python3` 3.9 or newer, plus the stock `ssh`, `scp`, `ssh-keygen`, `install`,
  `stat`, `wc`, `awk`, and `sha256sum`. `scripts/lib/config.sh` detects GNU versus BSD
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

Every workstation script listed in this section, plus
`scripts/user_callable/guests/setup_jump_ssh_access.sh` and
`scripts/user_callable/hosts/simulate_disk_failure_and_replacement.sh`, has been verified from an
x64 Ubuntu workstation against the live cluster (2026-10-06). The lists below
cover macOS only.

Verified from macOS against the live cluster (2026-09-21, full
destroy/create/deploy/stage cycle on the test domain):

- `scripts/user_callable/guests/prod/add_prod_vm.sh`, including `--dry-run`, the local root
  password hash, the `scp` uploads, and the jump-SSH alias it writes to
  `~/.ssh/config`;
- `scripts/user_callable/guests/prod/remove_prod_vm.sh`, including `--dry-run`;
- `scripts/user_callable/guests/prod/deploy_hello_app_to_prod.sh`;
- `scripts/user_callable/diagnostics/show_prod_vm_state.sh`;
- `scripts/user_callable/guests/staging/add_staging_vm.sh`, without `--sanitizer`;
- `scripts/user_callable/guests/staging/remove_staging_vm.sh`.

Expected to work on macOS, not yet run there against a cluster:

- `scripts/user_callable/guests/prod/extend_prod_vm_disk.sh`. Its local work is the config load,
  `ssh -G`, `python3`, and strict SSH, the same as the production creator;
- `scripts/user_callable/guests/staging/add_staging_vm.sh --sanitizer PATH`. The sanitizer size
  check was the macOS-specific part; it is covered by unit tests only;
- `scripts/user_callable/diagnostics/show_cluster_state.sh`. Its only local work is the config load
  and two `sha256sum` calls. It needs a terminal;
- `scripts/user_callable/diagnostics/show_proxmox_host_state.sh`. It uses the same strict SSH
  helpers as `show_prod_vm_state.sh`, plus local `python3`, and is covered by
  fake-SSH unit tests only;
- `scripts/user_callable/hosts/add_new_disk_vdev.sh`, `scripts/user_callable/hosts/add_replacement_disk.sh`,
  `scripts/user_callable/hosts/decommission_disks.sh`, and `scripts/user_callable/hosts/inventory_disks.sh`. Their local work is the
  config load, `python3`, and SSH; covered by fake-SSH unit tests only;
- `scripts/user_callable/hosts/update_cluster_runtime.sh`. Local commands are portable and its unit
  tests pass on macOS. It rewrites the runtime on every node, so treat the
  first macOS run as a test, not as routine;
- `scripts/user_callable/diagnostics/show_qdevice_state.sh`. Config load, `python3`, and strict SSH;
  covered by unit tests only;
- `scripts/user_callable/diagnostics/check_qdevice_access.sh`. Config load and one strict SSH
  connection; covered by fake-SSH unit tests only;
- `scripts/user_callable/diagnostics/show_cluster_health.sh`. Config load, `python3`, and strict SSH;
  covered by fake-SSH unit tests only;
- `scripts/user_callable/guests/prod/change_prod_vm_placement.sh` and
  `scripts/user_callable/guests/prod/change_prod_vm_owner.sh`. Their local work is the config load,
  `python3`, `mktemp`, and strict SSH, the same as the production creator.

Linux workstation only:

- `scripts/user_callable/hosts/add_proxmox_host.sh`. It needs `flock` and `ip` locally, and the
  install-ISO phase executes the amd64 Linux `proxmox-auto-install-assistant`,
  so it also cannot run on arm64 Linux.
- `scripts/user_callable/hosts/remove_proxmox_host.sh`, `scripts/user_callable/qdevice/add_qdevice.sh`, and
  `scripts/user_callable/qdevice/remove_qdevice.sh`. They take the
  same workstation `flock` as host setup.

Never run on a workstation, so the workstation OS does not matter. These
execute on a Proxmox host, inside a guest, or from a live Linux boot, and are
free to use GNU-only tools:

- `scripts/host_runtime/prepare_prod_iso.sh`, `scripts/host_runtime/build_ubuntu_autoinstall.py`;
- `scripts/utilities/patch_staging_clone.sh`,
  `scripts/utilities/patch_staging_guest_tree.py`;
- `scripts/host_runtime/app-ha-guest-role-hook.sh`, `scripts/user_callable/hosts/cluster_setup_prereq.sh`;
- `scripts/host_runtime/cluster_registry.py`, `scripts/host_runtime/process_deferred_cleanup.sh`;
- `scripts/host_runtime/guest_network.sh`, `scripts/host_runtime/egress_health.py`,
  `scripts/host_runtime/transfer_counter.py`, `scripts/host_runtime/tunnel_config.py`,
  `scripts/host_runtime/tunnel_sync.sh`, `scripts/host_runtime/cloudflared_setup.sh`.

`scripts/lib/config.sh` is the one file that runs in both places: every workstation
script sources it, and the hosts keep their own installed copy.

When changing a workstation script, the portability traps that actually bit
were GNU `stat -c`, `realpath -e`, `date --iso-8601`, LibreSSL lacking
`openssl passwd -6`, and `chmod MODE -- FILE` (BSD `chmod` stops option
parsing at the mode, so it reads `--` as a file name). `rm -rf --`, `cd --`,
`install -m`, `mktemp -d TEMPLATE`, `paste -sd, -`, and `sha256sum` are fine on
both.

macOS limits:

- The checkout must not sit under a symlinked path (`/tmp`, `/var`, and `/etc`
  are symlinks on macOS). The loader rejects symlinked components of `config/`.
- The `0600` and "not group/world writable" checks read mode bits only. A
  macOS ACL (`chmod +a`) can grant another user read access to `secrets.env` or
  write access to the SSH `known_hosts` trust file without changing those
  bits, and is not detected. `ls -le` shows ACL entries.
- On macOS the unit suite skips the classes that exercise Proxmox-host-only
  scripts (`prepare_prod_iso.sh`, `process_deferred_cleanup.sh`), and the
  lifecycle-hook tests need `flock` and GNU `stat`. Those run only in a Linux
  test pass; [`dev/README.md`](../dev/README.md) sets one up in a Lima VM.

## Responsibility boundaries

- **Cloudflare** owns public DNS, edge TLS, bot protection, and the choice of
  which connected tunnel replica carries each request. It never starts or
  moves a VM.
- **The `cloudflared` replica on each mox** maps an exact FQDN to its fixed
  slot address over the private VLAN, using a config that BMAC renders from
  the registry. It does not query Proxmox placement or trigger HA.
- **Host nftables** provides the shared guest gateway's isolation from the
  VLAN, guest forwarding, and masquerade. No mox DNATs any public port.
- **The egress health service on each mox** decides, for that host alone,
  whether its guests leave through its own internet connection or another
  mox's. Nothing is elected and nothing is shared.
- **The private Layer-2 VLAN** carries backend packets to whichever bridge
  currently owns the guest MAC.
- **Proxmox HA** is the only production placement/failover authority. Manual
  dual starts or HTTP-driven boot scripts would defeat fencing and quorum.
- **ZFS replication** supplies asynchronous copies to the allowed placement
  nodes.
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
provided by your data center in `config/cluster.conf` and `config/moxN.conf` before
running any script. The scripts consume and validate those files; production
code must not use the example addresses as fallback defaults.

Provider inputs map to configuration as follows:

- the shared private VLAN CIDR and provider-reserved VLAN gateway go in
  `PRIVATE_SUBNET_CIDR` and `DATACENTER_PRIVATE_VLAN_GATEWAY`;
- the shared guest gateway address and MAC, the mox range, and the VM slots
  go in `GUEST_GATEWAY_IP`, `GUEST_GATEWAY_MAC`, `MOX_IP_START/END`,
  `VM_SLOT_START`, `MAX_PROD_VMS`, and `MAX_STAGING_VMS_PER_PROD`;
- each server's public IP, public gateway, prefix length, private NIC
  address, optional transfer cap, and iDRAC address go in its
  `config/moxN.conf`;
- the Cloudflare account and tunnel IDs go in `CLOUDFLARE_ACCOUNT_ID` and
  `CLOUDFLARE_TUNNEL_ID`, and the tunnel secret in `secrets.env`;
- the provider's IPMI VPN URL and credentials are operator access
  information only and are not consumed by these scripts.

The design currently requires one shared IPv4 `/24`, but its first three
octets and all usable role allocations within it are configurable. Ranges and
singleton addresses must not overlap. The mox range must contain at least
`MAX_MOX_HOSTS` addresses.

VM slots are fixed and exist whether or not a VM uses them:

```text
prodN         VM_SLOT_START + 10*(N-1)     1 <= N <= MAX_PROD_VMS (at most 10)
stageKprodN   prodN's address + K          1 <= K <= MAX_STAGING_VMS_PER_PROD (at most 9)
```

`VM_SLOT_START`'s last octet must end in 0, so an address's last digit is the
staging number (0 for production) and its tens digit identifies the
production VM. Every slot through `VM_SLOT_START + 10*MAX_PROD_VMS - 1` must
fall inside `PRIVATE_SUBNET_CIDR` without overlapping any other range or
address. `GUEST_GATEWAY_MAC` must be a locally administered unicast MAC.

The checked-in example uses `10.213.0.0/24` with these assignments:

```text
10.213.0.0        network address
10.213.0.1        FiberState-reserved; not used as a route or gateway here
10.213.0.2-.9     reserved; .3 has no special role
10.213.0.10       shared guest gateway, provided by every mox to its own guests
10.213.0.11-.20   mox1 through mox10
10.213.0.21-.109  reserved for future use
10.213.0.110-.209 VM slots: prod1 through prod10, each followed by its 9 staging slots
10.213.0.210-.240 reserved for future use
10.213.0.241-.250 cluster dedicated migration/replication IPs for mox1 through mox10
10.213.0.251-.254 reserved for future use
10.213.0.255      broadcast
```

For host index `N`, addresses are derived from the configured range starts:

```text
host name                 moxN
internal FQDN             moxN.pve.internal
host private IP           MOX_IP_START+(N-1), with the private /24 prefix
egress fallback table     200+N, fwmark 0x100+N (used by every other mox)
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
  FQDN/public:   mox1.example.com, 38.46.219.67/29 via 38.46.219.65
  private:       10.213.0.11/24
  iDRAC:         10.20.3.63
  storage input: one mandatory NVMe mirror pair

mox2
  FQDN/public:   mox2.example.com, 38.129.137.234/28 via 38.129.137.225
  private:       10.213.0.12/24
  iDRAC:         10.20.4.119
  storage input: one mandatory NVMe mirror pair
```

Only `mox1.conf` and `mox2.conf` currently exist. Adding `moxN` requires its
`config/moxN.conf`; setup recommends the lowest slot the registry records as
free, and every address derives from the slot number, so gaps are safe. The
egress fallback tables on every installed mox nevertheless cover every other
slot through configured `MAX_MOX_HOSTS=10`.

Production identity is deterministic:

- the first free production name is `prodN`, where
  `1 <= N <= MAX_PROD_VMS`;
- its address is its slot, `VM_SLOT_START + 10*(N-1)`, so in the example
  `prod1` is `.110` and `prod10` is `.200`;
- an arbitrary application/workload purpose slug is globally unique among
  production resources, becomes a recognizable `purpose-<slug>` tag, and is
  the production creator's resume key rather than a role selector;
- the registry chooses a deterministic locally administered unicast MAC from
  the registry UUID and resource name;
- VMID comes from a live `/cluster/nextid` search and excludes all live and
  registered VMIDs.

Staging identity is source-relative:

- the lowest free staging number `K` of the source, in
  `1..MAX_STAGING_VMS_PER_PROD`, produces `stage1prodN`, `stage2prodN`, and
  so on;
- its address is the fixed slot `prodN + K`;
- its FQDN is always `stageKprodN.<source-primary-FQDN>` (for example,
  `stage1prod1.example.com`); there is no override. That FQDN is already in
  every replica's tunnel config, so creating or removing a staging VM changes
  no network state;
- the staging MAC uses the same deterministic registry formula;
- name, VMID, IP, MAC, source, placement, domain, volume, and snapshot
  dependency remain one validated identity. The QEMU `net0` MAC and guest
  netplan MAC must match it exactly.

## Traffic map and ports

### Public ingress through the Cloudflare Tunnel

```text
client
  -> Cloudflare edge (Always Use HTTPS, edge TLS, bot protection)
  -> one connected cloudflared replica, chosen by Cloudflare
     (an existing outbound HTTP/2 connection from some moxM to Cloudflare)
  -> exact FQDN lookup in /etc/cloudflared/config.yml
  -> https://<prodN/stageKprodN slot address>:443 over the private VLAN,
     verified with originServerName = that FQDN
  -> guest terminates origin TLS
```

No mox listens on, or DNATs, any public port for web traffic. Each replica
keeps outbound TCP connections to Cloudflare on port 7844, pinned to
`protocol: http2`. The guest may run on any mox, not only on the one whose
replica carried the request. An FQDN that is not in the config receives the
catch-all `http_status:404`. While a staging slot has no VM, its FQDN gets a
Cloudflare error page because the replica cannot reach the slot address.
Cloudflare headers reach the guest as request headers; the application must
trust client-IP headers only after proving the request came through the
controlled Cloudflare/tunnel path.

### Ingress return path

```text
guest -> moxM private IP (the replica's VLAN address) -> cloudflared on moxM
      -> the replica's existing connection -> Cloudflare -> client
```

The guest's client is the replica's private `.11-.20` address on the same
`/24`, so the reply is switched directly back over the VLAN. The return path
never involves NAT, conntrack on another mox, or the guest's default gateway.

### Production and staging Internet egress

```text
prodN/stageKprodN (VM slot range)
  -> default gateway 10.213.0.10 (GUEST_GATEWAY_MAC)
  -> bmacgw0 on the mox the guest is running on
  -> ip rule iif bmacgw0 -> route table 200 (guest-egress)
  -> nftables forward
  -> masquerade through that mox's own public IPv4
  -> Internet
```

Every mox holds `.10` and `GUEST_GATEWAY_MAC` on its own gateway interface,
and bridge rules keep both off the VLAN, so a guest's ARP for `.10` is
answered only by the mox it runs on. The masquerade source set is exactly the
VM slot range plus the mox private range. Return packets are admitted only as
established/related traffic for this ruleset. The project does not restrict
guest destination ports in this NAT layer; guest/Proxmox firewall policy must
impose any egress controls. External egress allowlists need every possible
mox public IP. There is no DNAT to `.10`, no forwarding through `.3`, and no
provider-gateway dependency on `.1`.

### Egress fallback path

```text
guest on moxA (whose own internet connection is down)
  -> 10.213.0.10 on moxA
  -> route table 200, default via moxB's private address
  -> moxB, arriving on vmbr-private (main route table)
  -> masquerade through moxB's public IPv4
  -> Internet; replies return from moxB straight to the guest over the VLAN
```

Traffic a mox receives from another mox over the VLAN always leaves through
its own connection and is never passed on again, so loops are impossible.
See [Shared guest gateway and egress fallback](#shared-guest-gateway-and-egress-fallback).

### Administration and internal control

- Workstation to mox: TCP 22 over Tailscale for SSH and automation.
- Workstation to Proxmox UI/API: TCP 8006 over Tailscale.
- SPICE proxy, if used: TCP 3128 on the management path; public TCP 3128 is
  explicitly dropped.
- Mox to guest: TCP 22 to the guest's private slot address. The
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
- Tunnel replicas connect outbound to Cloudflare on TCP 7844 (HTTP/2) from
  each mox's public interface. Each replica's metrics and readiness endpoint
  listens only on `127.0.0.1:20241`.
- Egress health probes are outbound HTTPS (TCP 443) requests, every 5 seconds
  per mox, through its own public interface or, marked with a per-host
  fwmark, through another mox. They never use UDP or ICMP.
- Secure migration and ZFS replication use the dedicated
  `10.213.0.240/28` transport CIDR. Each mox host has exactly one `.241-.250`
  address in that CIDR, preventing its normal private address or the shared
  gateway address from also matching Proxmox endpoint selection. Proxmox manages their
  data-channel ports; this project pins the network and secure mode
  rather than exposing those channels publicly or over Tailscale.
- DNS, NTP, package retrieval, certificate issuance, and application API
  traffic leave guests through the unrestricted `.10` masquerade path unless
  guest firewall policy narrows them.
- iDRAC virtual media, HTTPS console, and boot observation travel only over
  the FiberState IPMI VPN using provider/vendor ports.

The generated first-boot host guard drops packets addressed to the host's
public IPv4 on TCP 22, 3128, and 8006. Its chain otherwise has policy
`accept`. Nothing DNATs or forwards inbound public traffic to a guest. These
specific rules are not a blanket public firewall: audit every listening
socket, the complete nftables/Proxmox ruleset, IPv6 exposure, and provider
firewall policy.

## Cloudflare Tunnel

All public ingress arrives through one Cloudflare Tunnel per cluster. Its
design, the Cloudflare setup steps, and the reasons behind them are in
[`BMAC_SIMPLIFIED_NETWORKING.md`](BMAC_SIMPLIFIED_NETWORKING.md#ingress-one-cloudflare-tunnel-on-every-proxmox-host);
this section summarizes what the scripts install and rely on.

The tunnel is created once, by the operator, with `cloudflared tunnel login`
and `cloudflared tunnel create bmac-<cluster name>` on the workstation. A
tunnel created that way is *locally managed*: replicas read their FQDN map
from a local config file, which BMAC owns. (A tunnel created in the
Cloudflare dashboard is remotely managed and ignores that file, so it cannot
be used.) The credentials file it writes holds three values, which go in
BMAC's configuration: `CLOUDFLARE_ACCOUNT_ID` and `CLOUDFLARE_TUNNEL_ID` in
`cluster.conf`, and `CLOUDFLARE_TUNNEL_SECRET` in `secrets.env`. The
workstation's `cert.pem` is never copied into the checkout or onto a mox.

Every mox runs a replica of the tunnel:

- `scripts/host_runtime/cloudflared_setup.sh` installs `cloudflared` from
  Cloudflare's apt repository, writes the credentials file
  `/etc/cloudflared/bmac-tunnel.json` (mode `0640`, readable only by root and
  the `cloudflared` user) from the account ID, tunnel ID, and the secret read
  on stdin, and installs `app-ha-cloudflared.service`, which runs
  `cloudflared tunnel run` as user `cloudflared` under a strict systemd
  sandbox. The secret never appears in a command line or the journal.
- `scripts/host_runtime/tunnel_config.py` renders `/etc/cloudflared/config.yml`
  from the installed `cluster.conf` and the registry's
  `tunnel-productions` listing (every production guest's name, slot address,
  primary domain, and aliases, in any state). For each production guest it
  maps the primary and alias FQDNs, plus
  `stage1prodN.<primary>` through
  `stage<MAX_STAGING_VMS_PER_PROD>prodN.<primary>`, to
  `https://<slot address>:443` with `originServerName` set to that FQDN. A
  final catch-all returns `http_status:404`. The file sets
  `protocol: http2`, because `cloudflared`'s default QUIC transport is UDP and
  FiberState throttles inbound UDP, and serves metrics only on
  `127.0.0.1:20241`. The file carries a generation hash of its content.
- `scripts/host_runtime/tunnel_sync.sh` renders the config and installs it
  where it changed. `--local` updates only this mox; `--all` (the default)
  updates this mox first and then every other online mox, one at a time,
  restarting each replica and waiting for it to report ready before moving on,
  so at least one replica stays connected; `--check` compares this mox's
  installed and expected generations and exits 1 if they differ.
  `add_prod_vm.sh` and `remove_prod_vm.sh` run it with `--all`.
  `app-ha-tunnel-sync.timer` runs `--local` every five minutes on every mox as
  a backstop for a host that was offline when a production VM was added or
  removed.

The tunnel config changes only when a production VM is created or removed, or
when the slot settings change. Staging creation and removal never touch it.
The config is rendered on each mox from pmxcfs and `cluster.conf`; it is not
itself stored in pmxcfs.

Cloudflare sends each request to the nearest connected replica and offers no
way to prefer one. It does not matter which mox carries a guest's traffic,
because every replica reaches every slot over the VLAN. When a mox loses its
internet connection, its replica's connections drop and Cloudflare uses the
remaining replicas within seconds. Nothing spreads requests evenly between
replicas, so one mox may carry most of the ingress, and that traffic counts
toward its transfer cap.

Cloudflare configuration is external and is never mutated by this
repository; BMAC holds no Cloudflare API token. For each production domain,
`add_prod_vm.sh` prints the steps, which take a few minutes:

1. The domain is a Cloudflare zone in the same account as the tunnel.
2. Add two proxied DNS records, `@` and `*`, both
   `CNAME <tunnel ID>.cfargotunnel.com`. Cloudflare's free edge certificate
   covers the domain and one level below it, which includes every
   `stageKprodN.<domain>`.
3. Turn on Always Use HTTPS, so no mox ever needs to accept plain HTTP.
4. For a web app without an API, stay on the free plan and turn on Bot Fight
   Mode. For a web app with an API under `/api/`, subscribe the domain to
   Cloudflare Pro, leave Bot Fight Mode off, turn Super Bot Fight Mode on, and
   add the `/api/` WAF skip rule as the first custom rule. Bot Fight Mode
   cannot be skipped by any rule, so this is the only way to keep API calls
   from ever being challenged; see
   [API traffic and bot protection](BMAC_SIMPLIFIED_NETWORKING.md#api-traffic-and-bot-protection).
5. Repeat for an alias domain in a different zone.

Keep origin TLS valid for every FQDN the guest serves; the replica verifies
the guest's certificate against `originServerName`. Several BMAC clusters can
share one Cloudflare account, each with its own tunnel; clusters never share
a tunnel.

Cloudflare ingress failover and Proxmox VM recovery run at different speeds.
Replicas can be connected while production is still restarting, so temporary
backend failures are expected. Do not couple the tunnel to VM placement
merely to hide that interval.

## Shared guest gateway and egress fallback

Every production and staging VM has one NIC on `vmbr-private` and uses
`GUEST_GATEWAY_IP` (`.10`) as its only default gateway. Every mox provides
that gateway, but only to the guests running on it. Nothing is elected and
nothing moves between hosts. The full design is in
[`BMAC_SIMPLIFIED_NETWORKING.md`](BMAC_SIMPLIFIED_NETWORKING.md#egress-the-shared-gateway).

`scripts/host_runtime/guest_network.sh` builds it on each mox from the
settings file `/etc/app-ha/guest-network.env`, which host setup and the sync
operation generate from `cluster.conf` and the host's `moxN.conf`:

- **Gateway interface.** A veth pair: `bmacgw0` holds `GUEST_GATEWAY_IP/32`
  and `GUEST_GATEWAY_MAC`, and its peer `bmacgw0br` is a port of the private
  bridge. The address and MAC are the same on every mox. A post-up hook on the
  private bridge re-attaches the veth if a bridge reload detaches it, and the
  health service checks it as well.
- **Kept off the VLAN.** The nftables table `bridge app_ha_guest_gateway`, on
  the physical private-VLAN NIC in both directions, drops ARP about the
  gateway address and every frame from or to `GUEST_GATEWAY_MAC`. A guest's
  ARP for `.10` is therefore answered only by its own mox, and the identical
  address and MAC on every mox never conflict. Mox addresses, VM-to-VM
  traffic, Corosync, migration, and replication are unaffected.
- **Outbound NAT.** The table `ip app_ha_guest_egress` accepts forwarding from
  `bmacgw0` for the VM slot range, and from the private bridge to the public
  bridge for the slot range and the mox private range, plus
  established/related replies, and masquerades both sets out the public
  bridge. It covers every slot from `cluster.conf`, so VM creation and removal
  never change it.
- **Routing.** `ip rule iif bmacgw0` selects route table 200 (`guest-egress`),
  whose default route is normally this mox's own public gateway. Tables
  201-210, selected by fwmarks `0x101`-`0x10a`, each hold a default route via
  one other mox's private address, for probing and using that mox as the
  fallback.
- **Kernel settings** in `/etc/sysctl.d/91-app-ha-guest-network.conf`:
  `ip_forward=1`, `rp_filter=2`, `arp_ignore=1`, `arp_announce=2`,
  `send_redirects=0` (so a mox forwarding a guest's packet to another mox
  never tells the guest to bypass `.10`), and `nf_conntrack_tcp_loose=1` (a
  forwarding mox sees only the outbound half of a fallback connection).
- **Proxmox host firewall** forward-accept rules commented
  `app-ha managed guest network forward from VM slots` and
  `... forward from mox hosts`.
- **Units.** `app-ha-guest-network.service` applies the state at boot, before
  `pve-guests.service`; the nft rules live in
  `/etc/nftables.d/app-ha-guest-network.nft`. `guest_network.sh verify`
  checks every piece.

Because the gateway MAC is the same everywhere, a live-migrated guest keeps
sending to it and its traffic continues at once through the destination
mox's connection. Outbound connections open before a migration break,
because the public source address changes.

### Egress fallback

`scripts/host_runtime/egress_health.py` runs as
`app-ha-egress-health.service` on every mox and decides, for that mox only,
which internet connection its guests use:

1. Every 5 seconds it sends one HTTPS HEAD request to the next target in a
   rotation of about 39 independent endpoints
   (`scripts/host_runtime/egress_probe_targets.txt`) through its own public
   interface, to an address resolved and cached while the connection was
   healthy. Any HTTP status with a verified certificate within 4 seconds is a
   success. Probes are TCP only, never UDP or ICMP.
2. On a failure it tries the following targets, four at a time; the
   connection counts as down for that check only when every target fails.
3. After 3 such checks in a row, it points table 200 at the first other mox
   through which a probe (sent with that mox's `SO_MARK`) reaches the
   internet, trying them in order starting with the next slot number after
   its own and wrapping around. While falling back it also installs the
   raw-prerouting notrack table `app_ha_egress_fallback` and keeps checking
   both connections.
4. After 5 good checks of its own connection, it points table 200 back at
   its own gateway.

A target that fails on its turn while the following targets answer gets a
strike; a success clears its strikes, and 5 in a row retire it (never below 5
active targets). Setup and the sync operation validate the shipped list into
`/var/lib/app-ha/egress-targets.json` and warn when fewer than 15 work.
Current state, including the fallback in use and retired targets, is in
`/run/app-ha/egress-health.json` and shown by `egress_health.py status`.
Switches are logged. The cluster supports up to 10 mox, any of which can be a
fallback; a mox that is itself falling back cannot carry another's traffic,
fails the probe, and is never chosen.

### Transfer counter

`scripts/host_runtime/transfer_counter.py`, run every minute by
`app-ha-transfer-counter.timer`, adds the bytes received and sent on the
physical public NIC to a monthly count in `/var/lib/app-ha/transfer.json`.
Tunnel traffic counts too. The optional `moxN.conf` keys
`PROXMOX_HOST_TRANSFER_MONTHLY_LIMIT_GIB` and
`PROXMOX_HOST_TRANSFER_MONTHLY_RESET_DAY` (1-28, default 1; a larger value is
treated as 28) set the provider's cap and reset day. The count resets once per
period and survives reboots and interface-counter restarts. The counter only
reports; when a provider cuts a capped mox's connection, the tunnel and the
egress fallback handle it like any other outage.

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
failure, but it does not replace the failed migration, replication, egress
fallback, tunnel backend, or guest network.

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

## Monitoring and alerting

[`MONITORING_AND_ALERTING.md`](MONITORING_AND_ALERTING.md) is the full
design; this section records what is built. Monitoring is off unless
`cluster.conf` sets `MONITORING_ENABLED=true`; then `HEALTHCHECKS_API_KEY`
must be in `secrets.env` and `PROXMOX_QDEVICE_HOST` must be set and reachable.

Every monitored node runs one agent,
[`app_ha_monitor.py`](../scripts/host_runtime/app_ha_monitor.py)
(`app-ha-monitor.service`, standard library only, Python 3.11 or newer),
bound to its own Healthchecks.io check named `<PROXMOX_CLUSTER_NAME>
bmaccheck <node>`: every Proxmox host (role `host`), the QDevice (role
`qdevice`, node name always `qdevice` whatever `PROXMOX_QDEVICE_HOST` is),
and every production VM in registry state `active` (role `guest`). Staging
VMs are never monitored: the agent sends nothing on a host whose name starts
with `stage`, and the guest unit's `ExecCondition` keeps it from starting on
a staging clone.

The agent sends a plain success ping every `MONITOR_ALIVE_PERIOD_SECONDS`
(the dead-man switch, from its own thread so a slow check never delays it).
Each change in its problems is sent as a pulse: `/fail` with a body whose line
1 is `bmacevent bmacp1` or `bmacevent bmacp2` and whose sections are NEW,
REMINDER, RESOLVED, and ONGOING, then a success whose body starts with
`bmacclear`, then a plain heartbeat, so the most recent ping never carries a
token. Every message is sanitized so no other ping contains a token. Pulses
are limited to one a minute and twelve an hour; undelivered items stay
queued until a pulse carrying them is delivered, and a pending clear is
retried with every heartbeat.

Checks (each with a timeout; a check that cannot run becomes an
`agent:<check>` problem):

- host: corosync links to every expected peer (`corosync-cfgtool -n`, node
  IDs from `corosync.conf`), quorum, missing votes, and the QDevice's vote
  when one is registered (`corosync-quorumtool -s`,
  `corosync-qdevice-tool -s`), this host's tunnel replica
  (`http://127.0.0.1:20241/ready`), old Postfix mail (`postqueue -j`), ZFS
  health, pool capacity, and SMART/NVMe health, wear, spare, and counters;
- qdevice: ICMP over Tailscale to every host, qnetd connections while the
  QDevice is registered, ingress probes through Cloudflare for every enabled
  probe, and its own filesystems;
- guest: the systemd services and certificate files in
  `/etc/app-ha-monitor/guest.json`, and its own filesystems.

Connectivity checks open after `MONITOR_FAST_FAIL_THRESHOLD` failures and
resolve after as many passes; monitored services after two failures; slow
checks on first detection. Capacity opens below
`MONITOR_POOL_MIN_FREE_PERCENT` and resolves only above
`MONITOR_POOL_CLEAR_FREE_PERCENT`. The parsers for corosync links, `zpool
status`, and status lines are shared with `show_cluster_health.sh`.

Workstation side: [`monitoring.sh`](../scripts/lib/monitoring.sh) builds an
inventory through a coordinator (cluster nodes with node IDs, whether
`corosync.conf` has a QDevice, and the registry's `monitoring-probes`),
[`monitor_config.py`](../scripts/lib/monitor_config.py) renders the desired
checks and each node's config, [`healthchecks.py`](../scripts/lib/healthchecks.py)
creates or updates the checks (`unique: ["name"]`, slug always sent, budget
counted over the whole project against `MONITOR_MAX_CHECKS`), and
[`monitor_node_install.sh`](../scripts/lib/monitor_node_install.sh) installs
one node atomically, keeping every replaced file for rollback. `monitoring_sync`
does all of it, idempotently, a phase at a time (hosts, the QDevice,
production VMs); a failing node rolls its phase back, and checks created by
a failed run are kept for the next. It then waits until every check has
received a ping after the install and runs `check --once` on every node.

Membership: `add_proxmox_host.sh`, `remove_proxmox_host.sh`,
`add_qdevice.sh`, `remove_qdevice.sh`, `add_prod_vm.sh`, and
`remove_prod_vm.sh` end with `monitoring_membership_sync`, whose failure is
a warning with the next step "run update_cluster_runtime.sh", never a failure
of the main operation. Host and production VM removal delete that node's
check (`--delete-node`, listed in the removal plan). QDevice removal keeps the
QDevice's check and agent. `add_proxmox_host.sh`, `add_prod_vm.sh`, and
`update_cluster_runtime.sh` stop before changing anything when monitoring is
enabled and the QDevice is unreachable; removals only warn.

Operator tools: `show_monitoring_state.sh` (read-only, plus
`--reset-smart-baseline moxN:SERIAL`), `send_test_alert.sh`,
`manage_prod_vm_monitoring.sh` (edits only the VM's `guest.json`), and
`manage_qdevice_probes.sh` (edits the registry's probe, trial-runs it from
the QDevice first, then syncs). `deploy_hello_app_to_prod.sh` adds
`hello-todo.service` and `/etc/hello-todo/cert.pem` to `guest.json`, and
the example app serves `GET /api/v1/healthz`.

Node files:

```text
/usr/local/lib/app-ha-proxmox/lib/app_ha_monitor.py   hosts (runtime file set)
/usr/local/lib/app-ha-monitor/app_ha_monitor.py       QDevice and production VMs
/etc/systemd/system/app-ha-monitor.service
/etc/app-ha-monitor/config.json                       0600; this node's ping URL
/etc/app-ha-monitor/guest.json                        0600; production VMs only
/var/lib/app-ha-monitor/state.json                    0600; problems, queue, baselines
/var/lib/app-ha-monitor/requests/                     e.g. reset-smart-<serial>
```

The agent re-reads its config and `guest.json` every cycle, so a sync that
changes only config does not restart it. A state file that cannot be read
is moved aside as `state.json.corrupt-<time>`, and the agent starts fresh,
announcing every open problem once.

## Host storage, LUKS, and mirrors

The reviewed source is `proxmox-ve_9.2-1.iso`, verified against the exact
SHA-256 in `cluster.conf`. The host workflow creates a
host-specific unattended ISO with public networking, serial-selected mirror
1, administrator keys, a temporary setup key, a fresh one-time Tailscale key,
the public management-port guard, and first-boot Tailscale bootstrap.

One through five NVMe mirror pairs are supported, numbered 0 through 4 to match
ZFS's own `mirror-0`, `mirror-1`, ... vdev names:

- Mirror 0 is mandatory and bootable. The Proxmox installer destroys its two
  serial-selected disks, creates the raw ZFS `rpool` mirror and ESPs, and
  deliberately leaves at least 4 GiB after partition 3 for optional LUKS
  conversion.
- Each run selects iDRAC/Redfish or manual disk inventory. Redfish mode
  remotely verifies serial, capacity, and health. Manual mode uses the
  standalone, read-only `scripts/user_callable/hosts/cluster_setup_prereq.sh`, copied to and run in a
  Linux Live environment booted on the machine,
  and requires an exact `NVME_MIRROR_<P>_CAPACITY_BYTES_<M>` value for every
  configured serial; the members of each pair may differ by at most 1%, in
  either order, and the pair uses the smaller capacity.
- Before boot-test selection, the operator chooses whether all configured
  rpool members remain unencrypted or use LUKS2. Both choices are resumable.
- LUKS conversion degrades mirror 0 one member at a time. The script records a GPT
  backup and partition start, detaches the ZFS member, expands partition 3 to
  end 1 GiB short of the smaller boot disk's whole-GiB size (the same byte on
  both disks, whichever is smaller), clears only old ZFS signatures, and prepares LUKS2
  through a helper that setup runs over SSH after the operator confirms; it
  reads the temporary key file, so it needs no console. The helper, the crypttab
  entry, and the initramfs check are `scripts/host_runtime/rpool_mirror.sh` member commands,
  the same ones `scripts/user_callable/hosts/add_replacement_disk.sh` uses.
- Converted members are `/dev/mapper/crypt-rpool-a` and
  `/dev/mapper/crypt-rpool-b`. Each is recorded once in `/etc/crypttab`, the
  rebuilt initramfs is proven to unlock it, and it is reattached and
  resilvered with both ESPs refreshed.
- Selected boot testing uses three reboots in either mode. It boots from each
  mirror-0 member alone, restoring and resilvering after each simulation, then
  reboots the healthy final layout. LUKS testing begins only after both members
  are encrypted; it never duplicates the drills against the raw layout.
- Optional mirror pairs 1 through 4 must be complete. Gaps are allowed, so a
  decommissioned pair's entries can be commented out while later pairs keep
  their numbers and LUKS names. Each pair
  is capacity-matched within 1% and added as one new top-level `rpool` mirror
  vdev. Each disk gets one GPT partition (partition 1) that ends 1 GiB short of
  the smaller disk's whole-GiB size. In LUKS mode those partitions use names
  `crypt-rpool-mirror<P>-<M>`; in clear mode they remain unencrypted. Extra
  mirrors have no ESP. Host setup never adopts pre-existing LUKS on an extra
  pair: after explicit confirmation it erases both disks' metadata before
  creating fresh containers. Only LUKS created by an interrupted helper from
  that setup attempt is reused for resume.
- Because no member reaches the end of its disk, a replacement disk only has to
  hold the survivor's partition table (same logical sector size, at least the
  last partition's end plus 1 MiB), so a slightly smaller disk of the same
  nominal capacity works. Finalizing a decommission releases the disks:
  only LUKS key slots, ZFS labels, signatures, and the partition table are
  erased, never the data area, and their off-host LUKS header backups are
  deleted.
- Every member must accept the same configured `PROXMOX_LUKS_PASSWORD`.
  `decrypt_keyctl` caches it only in the initramfs kernel keyring so one
  target-console prompt can unlock every mapper.
- Swap must remain absent. Final verification checks every configured mapper,
  backing serial, mirror relationship, crypttab keyscript, initramfs helper,
  pool health, and boot-tool state.

The LUKS password is loaded only from the mode-`0600`, Git-ignored
`config/secrets.env`. It is sent to the selected host only over SSH stdin, written
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
backups for mirror 0 conversion.

Adding a top-level ZFS vdev expands and stripes the pool and may not be
reversible. Losing both devices in any one mirror vdev can lose the entire
pool. Removing a configured pair after it has entered `rpool`, changing disk
serial identity, ambiguous signatures, mounted devices, mismatched capacity,
or interrupted phase evidence all stop the workflow.

`--encrypt` and `--no-encrypt` select storage policy noninteractively.
`--skip-boot-tests` skips only the optional reboot drills; it does not change
the selected storage policy or skip final non-boot validation.

## Configuration, secrets, and artifacts

Workflows use [`scripts/lib/config.sh`](../scripts/lib/config.sh). It parses literal `KEY=VALUE`
data without `source` or `eval`, in this order when each layer is requested:

1. [`config/cluster.conf`](../config/cluster.conf): shared non-secret policy;
2. `config/moxN.conf`: selected host hardware and public/private identity;
3. local `config/secrets.env`: allowed secrets only, when required.

The config directory depends on how BMAC is run:

- From a checkout (dev): always `<checkout>/config`.
- From the installed Dashboard package: the scripts live under
  `/usr/lib/BMAC_Dashboard/scripts`, next to a `bmac-installed` marker, and
  the config directory defaults to
  `~/.config/com.btvcorp.bmac.dashboard/config` (respecting
  `XDG_CONFIG_HOME`). When the operator moves it from the Dashboard's Config
  page, the Dashboard writes the absolute path to the one-line pointer file
  `~/.config/com.btvcorp.bmac.dashboard/config-location`. Both the scripts and
  the Dashboard ignore a pointer that is not a regular file owned by the
  user, is group- or world-writable, or does not hold exactly one absolute
  path. No environment variable is involved.

The package ships only the `*_dot_*` examples (under
`/usr/lib/BMAC_Dashboard/config`). The Dashboard copies them into the config
directory as `cluster.conf`, `mox1.conf`, `mox2.conf`, and `secrets.env`
(mode `0600`) the first time each package version starts, and only when a
file does not already exist; it never overwrites the operator's files.
Uninstalling leaves the config directory alone. Settings and the Config page
warn about each of those four files while it is missing or still
byte-identical to its example. The Dashboard compares `secrets.env` in
process and never displays or returns its contents. It only opens the file
in the operator's own editor or shows its folder.

A checkout's config files must be Git-ignored where the scripts check it;
outside a checkout that check applies only when the config directory itself
sits inside a Git work tree.

`config/moxN.conf` is setup input only: initial host setup and retries before
setup completes. Post-setup disk and diagnostic workflows load cluster policy
only and use the target host's live state as authoritative.

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
administrator keys, Tailscale host tag, SSH behavior, and
`MONITORING_ENABLED`. The optional monitoring keys (`HEALTHCHECKS_API_URL` and
the `MONITOR_*` keys listed in the README's "Monitoring and Alerts" section)
are accepted, validated, and defaulted by `config.sh` but deliberately absent
from `cluster_dot_conf`. Monitoring keys are excluded from the effective
configuration hash, so changing them neither invalidates host-setup
checkpoints nor shows as configuration drift. Each `moxN.conf`
owns an optional iDRAC address, exact NVMe serial pairs, optional
manual-inventory byte capacities, public IP/gateway/prefix/MAC, private
MAC/address, per-host production and staging VM limits, and the optional
monthly transfer cap and reset day. `config.sh` derives each host's
application-agnostic internal FQDN.

The networking part of the address policy is `GUEST_GATEWAY_IP`,
`GUEST_GATEWAY_MAC`, `VM_SLOT_START`, `MAX_PROD_VMS` (1-10),
`MAX_STAGING_VMS_PER_PROD` (1-9), `CLOUDFLARE_ACCOUNT_ID`, and
`CLOUDFLARE_TUNNEL_ID`. The loader validates the slot range against the
subnet and every other range, and requires each host's
`MAX_STAGING_VM_COUNT_ON_THIS_HOST` to be no more than
`MAX_STAGING_VMS_PER_PROD`. After editing the slot, gateway, or Cloudflare
keys, or a host's transfer cap, push them with
`scripts/user_callable/hosts/sync_cluster_network.sh`.

Current creation defaults are:

```text
production  4 vCPU, 8 GiB RAM, 128 GiB local-zfs root,
            full/refreserved by default, x86-64-v2-AES, replication */15
staging     2 vCPU, 4 GiB RAM, source root size, sparse local clone
guests      vmbr-private, static slot address /24, gateway GUEST_GATEWAY_IP,
            DNS 1.1.1.1 and 1.0.0.1
```

Production and staging prompts can override their documented compute choices;
they cannot override the fixed network/storage identity model.

[`config/secrets_dot_env`](../config/secrets_dot_env) is the checked-in key template;
copy it to local `config/secrets.env`, keep the template value-free, and populate
only the ignored local copy. `config/secrets.env` may contain only:

```text
IDRAC_USER
IDRAC_PASSWORD
PROXMOX_ROOT_PASSWORD
PROD_GUEST_VM_ROOT_PASSWORD
STAGING_GUEST_VM_ROOT_PASSWORD
PROXMOX_LUKS_PASSWORD
PROXMOX_QDEVICE_ROOT_PASSWORD
CLOUDFLARE_TUNNEL_SECRET
HEALTHCHECKS_API_KEY
```

`HEALTHCHECKS_API_KEY` is the Healthchecks.io project's read-write API key,
required when `MONITORING_ENABLED=true`; the loader rejects an empty value or
the template's `redacted`. Only the workstation uses it, through the
environment of `healthchecks.py`.

`IDRAC_USER` and `IDRAC_PASSWORD` are optional unless host setup is run in
iDRAC/Redfish inventory mode. `CLOUDFLARE_TUNNEL_SECRET` is the tunnel's
`TunnelSecret`. Host setup and the sync operation copy it to the target mox
as a mode-`0600` file, feed it to `cloudflared_setup.sh` on stdin, and shred
the copy on both ends; on the mox it exists only inside the tunnel's
credentials file.

Secret values are shell-local, never printed, excluded from the public
configuration hash, and unset before SSH, Python, or builder children whenever
possible. Host setup separately computes a non-printed secret-inclusive
fingerprint so a changed destructive input requires explicit acceptance.

The Tailscale enrollment secret is deliberately not file-backed:

- a fresh, single-use, non-ephemeral `tag:proxmox-host` Tailscale auth key is
  entered silently for each host ISO and its digest is reserved against reuse.

`PROXMOX_LUKS_PASSWORD` is file-backed only in `config/secrets.env` and in the
short-lived on-host file described above. It is never embedded in an ISO.

Local artifacts live under `<config>/artifacts/` (in a checkout,
`config/artifacts/`, which is Git-ignored). They are sensitive even when they
contain hashes rather than plaintext:

- `artifacts/hosts/source-iso/`: the standard Proxmox VE ISO downloaded from
  `PROXMOX_ISO_FILE_URL`, shared by every host and kept or deleted at the
  operator's choice after media is built;
- `artifacts/hosts/<moxN>/*.iso`: generated host-specific installation media;
- `artifacts/hosts/<moxN>/generated/`: answer and first-boot input files;
- `artifacts/hosts/<moxN>/ssh/`: temporary setup key;
- `artifacts/hosts/<moxN>/state/`: resumable phase evidence and host key;
- `artifacts/hosts/<moxN>/logs/`: install, storage, quorum, and verification
  evidence;
- `artifacts/hosts/<moxN>/luks-headers/`: LUKS headers and GPT backups;
- `artifacts/prod/` and `artifacts/staging/`: mode-`0700` ephemeral creator
  workspaces.

The `artifacts/hosts/used-tailscale-auth-key-sha256` digest denylist blocks
reuse of an exposed Tailscale key on this workstation; its `.lock` file
prevents concurrent setup processes from both passing the check-before-append
operation. Back up host recovery material separately, encrypted, and with
access controls. Git ignore is not a backup or security boundary.

Earlier versions kept these trees under
`scripts/user_callable/{hosts,guests/prod,guests/staging}/artifacts`. The
scripts refuse to run while one of those legacy directories is non-empty and
print the exact `mkdir -p ... && mv ...` command that moves it into place.

## The main workflows

There are fifteen operator workflows in this project: six (1-6) that build
hosts and create, grow, or remove guests, four (7-10) that grow, shrink, or
repair a host's storage, three (11-13) that move production between hosts and
change cluster membership, one (14) that replaces a failed QDevice, and one
(15) that pushes changed networking settings to every host:

1. `scripts/user_callable/hosts/add_proxmox_host.sh` — destructively install or reconcile
   one `moxN`, create or join the cluster, and install storage, networking,
   QDevice, the shared guest gateway and egress fallback, the transfer
   counter, the tunnel replica, registry, lifecycle, and cleanup services. Read
   [`scripts/user_callable/hosts/README.md`](../scripts/user_callable/hosts/README.md). Usage:

   ```bash
   scripts/user_callable/hosts/add_proxmox_host.sh \
     --host moxN [--encrypt | --no-encrypt] \
     [--run-boot-tests | --skip-boot-tests]
   ```

2. `scripts/user_callable/guests/prod/add_prod_vm.sh` — create or safely resume one
   replicated, HA-managed `prodN`. Read
   [`scripts/user_callable/guests/prod/README.md`](../scripts/user_callable/guests/prod/README.md). Start with:

   ```bash
   scripts/user_callable/guests/prod/add_prod_vm.sh --dry-run
   scripts/user_callable/guests/prod/add_prod_vm.sh
   ```

3. `scripts/user_callable/guests/prod/remove_prod_vm.sh` — permanently remove one production VM
   after exact identity and dependency validation. It removes HA and
   affinity, replication and target copies, QEMU config/disks, orchestration,
   and registry metadata, then removes its FQDNs and its staging slots' FQDNs
   from every replica's tunnel config (`tunnel_sync.sh --all`), while
   retaining the shared source ISO cache. It changes nothing in Cloudflare.
   Start with the read-only rehearsal:

   ```bash
   scripts/user_callable/guests/prod/remove_prod_vm.sh --dry-run prodN
   scripts/user_callable/guests/prod/remove_prod_vm.sh prodN
   ```

4. `scripts/user_callable/guests/prod/extend_prod_vm_disk.sh` — after placement pools gain
   capacity, grow one running production VM's root zvol and then its final
   ext4 partition and filesystem online. Growth is capped so every placement
   pool keeps 10% of its total size available; it replicates the new size to
   every placement node and records the exact size in the registry. Start
   with the read-only rehearsal:

   ```bash
   scripts/user_callable/guests/prod/extend_prod_vm_disk.sh --dry-run
   scripts/user_callable/guests/prod/extend_prod_vm_disk.sh
   ```

5. `scripts/user_callable/guests/staging/add_staging_vm.sh` — create one
   disposable, non-HA `stageKprodN` linked clone from a registered active
   production VM in that VM's lowest free staging slot. It changes no network
   or Cloudflare state. Read
   [`scripts/user_callable/guests/staging/README.md`](../scripts/user_callable/guests/staging/README.md). Start with:

   ```bash
   scripts/user_callable/guests/staging/add_staging_vm.sh --dry-run
   scripts/user_callable/guests/staging/add_staging_vm.sh \
     --sanitizer /absolute/path/to/staging-sanitizer.sh
   ```

6. `scripts/user_callable/guests/staging/remove_staging_vm.sh` — interactively select and
   permanently remove one staging VM after exact identity, clone-origin, and
   snapshot-GUID validation. It removes only that guest's VM-owned
   disks, linked clone, one exact source-owned snapshot and its replicated
   copies, and registry allocation, freeing its slot. It changes no network
   or Cloudflare state. Start with:

   ```bash
   scripts/user_callable/guests/staging/remove_staging_vm.sh --dry-run
   scripts/user_callable/guests/staging/remove_staging_vm.sh
   ```

7. `scripts/user_callable/hosts/add_new_disk_vdev.sh` — add two new identical-capacity disks to a
   host's rpool as one new mirror vdev. On a LUKS host the operator types the
   shared passphrase at the host console, where it is proven against every
   existing member before the disks are formatted; the script then proves the
   rebuilt initramfs unlocks them before `zpool add`.
8. `scripts/user_callable/hosts/decommission_disks.sh` — prepare a host's rpool to give up one
   non-boot mirror vdev (related staging destroyed first; guest trims,
   forced replication, and a scrub, each skipped if done in the last 24
   hours) and start its `zpool remove` while keeping a minimum free space of
   at least 50 GiB.
9. `scripts/user_callable/hosts/inventory_disks.sh` — inventory all disks and rpool vdev members
   from live host state; once a removal completes, close the removed pair's
   LUKS mappings, update crypttab and the initramfs, then list only disks not
   in use as safe to pull.
10. `scripts/user_callable/hosts/add_replacement_disk.sh` — after a failed mirror member's disk was
   pulled and an identical-capacity disk installed, put the new disk into
   that mirror (`zpool replace`, or `zpool attach` for a mirror detached to
   one disk). A boot-mirror replacement
   first gets the survivor's partition table and its own registered,
   in-sync ESP; a LUKS replacement gets the shared passphrase at the host
   console. It starts the resilver without waiting for it.

Workflows 7-10 are described in
[`scripts/user_callable/hosts/README.md`](../scripts/user_callable/hosts/README.md#adding-and-decommissioning-rpool-disks).
They share `scripts/host_runtime/rpool_mirror.sh` with host setup, so there is one copy of the
logic that adds a mirror or encrypts a mirror member.

```bash
scripts/user_callable/hosts/add_new_disk_vdev.sh --host moxN
scripts/user_callable/hosts/add_replacement_disk.sh --host moxN
scripts/user_callable/hosts/decommission_disks.sh --host moxN
scripts/user_callable/hosts/inventory_disks.sh --host moxN
```

11. `scripts/user_callable/guests/prod/change_prod_vm_placement.sh` — add hosts to or remove hosts
   from one production VM's placement. Adding checks the host's
   `MAX_PROD_VM_COUNT_ON_THIS_HOST` and a 10% pool reserve after the replica.
   It records the registry placement, replicates, and widens the HA rule only
   after a successful initial replication. Removing narrows the HA rule
   first, then the registry, then deletes the replication job and replica. It
   never removes the live owner and keeps at least two placement hosts. It
   detects and finishes an interrupted change.
12. `scripts/user_callable/guests/prod/change_prod_vm_owner.sh` — move one production VM to another
   placement host with `ha-manager relocate` after checking that HA,
   replication, and the registry agree. It then verifies the reversed
   replication and records the new owner.
   `scripts/user_callable/guests/prod/reconcile_prod_vm_owner.sh` records,
   without moving anything, where every active production VM runs and the
   replication targets Proxmox uses for it. The guest hook normally does this
   after a failover; the script covers a failover that happened before the
   hook did so, or one whose update was skipped.
13. `scripts/user_callable/hosts/remove_proxmox_host.sh` — remove one host, gracefully when it is
   an online member that answers SSH, and forcefully when it cannot be
   contacted. Either way it chooses a new control node if needed and frees
   the host's registry slot.
   - Graceful removal retires a healthy host that holds nothing: remove the
     QDevice, stop and disable the host's tunnel replica and delete its
     tunnel credentials, power the host off, `pvecm delnode`, strip its SSH
     trust, and restore odd vote parity. Nothing changes in Cloudflare; the
     other replicas keep serving. Requires `REMOVE moxN`.
   - Forced removal deletes a dead host (an offline member, leftover node
     directory, or stale or unfinished slot) that does not answer SSH, after
     the operator agrees to it. It destroys dependent staging, narrows
     affected production placement, abandons the host's cleanup, and deletes
     the node (including the two-to-one-node case). The operator confirms
     that the machine is physically disconnected, removed from Tailscale, and
     will never reconnect in its old role by typing
     `moxN IS PERMANENTLY DISCONNECTED`. The removed machine must never
     communicate with the cluster again.
   - A host Proxmox sees online that does not answer SSH, or that answers SSH
     while Proxmox sees it offline, is refused.
14. `scripts/user_callable/qdevice/add_qdevice.sh` — add a QDevice to an even-member cluster that
   has none, typically after `scripts/user_callable/qdevice/remove_qdevice.sh` removed a failed
   one. `scripts/user_callable/diagnostics/show_qdevice_state.sh` reports whether a QDevice is
   needed, functional, or inaccessible. The full replacement procedure is in
   [`QDEVICE_MANUAL_SETUP.md`](QDEVICE_MANUAL_SETUP.md#replacing-a-failed-qdevice).
15. `scripts/user_callable/hosts/sync_cluster_network.sh` (the dashboard's
   "Sync cluster with cluster.conf" tile) — after editing `cluster.conf`'s
   slot, gateway, or Cloudflare keys, or a host's transfer cap in its
   `moxN.conf`, bring every online host in line. `--dry-run` shows each
   host's drift without changing anything. Otherwise, after `GO`, it runs the
   registry's `update-slots` (refused if any registered guest would move),
   pushes the installed `cluster.conf`, regenerates each host's
   guest-network settings from its `moxN.conf` and reruns
   `guest_network.sh configure` (which also revalidates the probe targets and
   applies the transfer cap), reinstalls the tunnel credentials, and finally
   runs `tunnel_sync.sh --all`. It is rarely needed.

   ```bash
   scripts/user_callable/hosts/sync_cluster_network.sh --dry-run
   scripts/user_callable/hosts/sync_cluster_network.sh
   ```

Workflows 11-13 are described in
[`scripts/user_callable/guests/prod/README.md`](../scripts/user_callable/guests/prod/README.md#changing-placement) and
[`scripts/user_callable/hosts/README.md`](../scripts/user_callable/hosts/README.md#removing-hosts). The
documented order for moving to bigger hosts is: set up the new hosts, add
them to placement, change the owner, remove the old hosts from placement,
then remove the old hosts.

The production/staging creators and destroyers have read-only
`--dry-run` modes that query live state without registry or Proxmox mutation.
Host setup has no dry run because bare-metal media, iDRAC gates, disk
conversion, and cluster membership cannot be faithfully rehearsed that way.
Use its explicit boot-test policy and read every destructive confirmation.
Creation gates use the case-sensitive token `GO`; branching installer recovery
may additionally offer `WIPE`, while production destruction requires
`DESTROY prodN`, graceful host removal `REMOVE moxN`, and forced host
removal `moxN IS PERMANENTLY DISCONNECTED`.

## Helper inventory

These are supporting diagnostics, maintenance utilities, implementation
helpers, and tests rather than additional deployment workflows.

Every script lives under `scripts/`:

- `scripts/user_callable/` holds the scripts an operator runs, by area:
  `diagnostics/`, `hosts/`, `guests/` (with `prod/` and `staging/`), and
  `qdevice/`. The dashboard's workflows are these scripts.
- `scripts/lib/` holds the libraries those scripts source or import.
- `scripts/utilities/` holds scripts that only other scripts run, on the
  workstation or piped or copied to a host for one run.
- `scripts/host_runtime/` holds the files installed on every Proxmox host:
  the registry, deferred-cleanup worker, QEMU lifecycle hook, guest network
  and egress health tools, transfer counter, tunnel config renderer,
  synchronizer, and installer, rpool mirror tool, and production ISO tools.
  Hosts install them under `/usr/local/lib/app-ha-proxmox` in their own
  layout.
- `scripts/tests/` holds every `test_*.py` and the JSON-mode test driver.

All `test_*.py` files use Python's standard `unittest` framework plus mocked
Bash/Proxmox commands and temporary roots; they depend on the scripts and
libraries named in their descriptions.

- `scripts/user_callable/diagnostics/show_cluster_health.sh` is a quick, read-only workstation
  check of hardware and networking that ignores guests. For each member it
  reports Proxmox online state, workstation SSH access, quorum, and the
  corosync, pve-cluster, and QDevice-client services. A member the
  workstation cannot reach is inspected through the first reachable member
  over cluster SSH. `corosync-cfgtool -n` on each member gives the state of
  link0 (private VLAN) and link1 (Tailscale) to every other member. For the
  QDevice it checks the need for one, registration, reachability, qnetd
  state, and per-member votes. It shows every ZFS pool's vdev tree with
  read/write/checksum counts, each member resolved through LUKS to its disk
  serial, and every drive's `smartctl -H` result. From `zfs list -p` it takes
  rpool's used and available bytes, treats their sum as usable space, and
  flags a host as needing more storage (`scripts/user_callable/hosts/add_new_disk_vdev.sh`) when
  less than 10% is available. It ends with a problem list and exits 1 when
  anything needs attention.
- `scripts/user_callable/diagnostics/show_cluster_state.sh` is an interactive, read-only
  workstation diagnostic for the QDevice and reachable `mox1` through
  `mox10`. It reports identity, clocks, Tailscale, firewall/listener state,
  Proxmox/Corosync quorum and links, native per-node SSH trust, QDevice
  state, services, storage/LUKS and SMART health, nftables, the shared guest
  gateway, egress fallback state, transfer count, and tunnel replica, failed
  units, the shared registry, and a joined registered/live
  production/staging guest inventory that also flags unregistered QEMU VMs. Unreachable
  uninstalled mox slots are reported and skipped. Redirect its output when a
  durable report is useful:

  ```bash
  scripts/user_callable/diagnostics/show_cluster_state.sh \
    >cluster_state.txt
  ```

- `scripts/user_callable/diagnostics/show_prod_vm_state.sh prodN` is a read-only cross-layer
  production guest diagnostic. It checks registry/orchestration state, live
  QEMU identity and power, HA request and strict node-affinity, replication,
  its tunnel input (`tunnel-productions`) and the coordinator's
  `tunnel_sync.sh --check`, ZFS allocation, QGA, private networking, and
  QGA-attested strict
  root SSH, then prints a `HEALTHY` or `UNHEALTHY` verdict:

  ```bash
  scripts/user_callable/diagnostics/show_prod_vm_state.sh prod1 \
    >prod1_state.txt
  ```

- `scripts/user_callable/diagnostics/show_proxmox_host_state.sh [--host moxN]` is a read-only report for one
  host: versions, CPU and RAM, quorum, HA and app-ha services, failed units,
  network, the shared guest gateway, egress mode (own connection or the
  fallback host) and retired probe targets, transfer this month, the tunnel
  replica, the guests it runs with their registry
  roles, the production replicas it stores, pending deferred cleanup, and its
  `rpool` layout. `scripts/utilities/host_storage.py` resolves every vdev through LUKS to
  physical disk serials, marks the boot/ESP vdev, and shows device-removal
  progress, disks outside `rpool`, ZFS available space in bytes, MiB, and
  GiB, and each zvol's allocation, snapshots, and last replication. It exits
  1 when anything needs attention:

  ```bash
  scripts/user_callable/diagnostics/show_proxmox_host_state.sh mox1 \
    >mox1_state.txt
  ```

- `scripts/user_callable/diagnostics/show_qdevice_state.sh` is a read-only workstation report on
  the QDevice. It says whether the member count needs one and whether one is
  registered, shows each member's vote view, and shows the QDevice host's
  qnetd state and connected clusters. It ends with a verdict: OK, needs a
  QDevice (`scripts/user_callable/qdevice/add_qdevice.sh`), registered but inaccessible
  (`scripts/user_callable/qdevice/remove_qdevice.sh`, then `scripts/user_callable/qdevice/add_qdevice.sh`), or reachable
  but unhealthy (the problems found). It exits 1 when anything needs
  attention.
- `scripts/user_callable/qdevice/add_qdevice.sh` adds a QDevice to an all-online, quorate cluster
  with an even member count and no registered QDevice. It verifies
  `ssh qdevice`, refuses a Proxmox VE host, and holds the cluster
  control-plane lock. It installs `corosync-qnetd`, pins the QDevice's SSH
  host key on every member, runs `pvecm qdevice setup` through the control
  node, and verifies every member's vote layout, as host setup does.
- `scripts/lib/qdevice.sh` is the one implementation of the QDevice vote: verifying
  access, preparing `corosync-qnetd`, pinning its host key, adding, removing,
  reconciling, clearing stale registrations, forgetting a retired QDevice,
  and checking vote layouts. Host setup, host removal,
  `scripts/user_callable/qdevice/add_qdevice.sh`, `scripts/user_callable/qdevice/remove_qdevice.sh`, and the diagnostics
  use it. It removes a QDevice gracefully when this workstation reaches it as
  root and it is the registered machine. Otherwise the removal is forced: the
  operator must agree and confirm the machine was removed from Tailscale, and
  every member forgets it. Membership changes continue with an inaccessible
  QDevice only when the resulting member count is odd. Each caller supplies
  how it reaches the cluster: `scripts/lib/host_membership.sh` through its
  coordinator, host setup through its cluster control node.
- `scripts/user_callable/qdevice/remove_qdevice.sh` is an intentionally destructive maintenance tool
  for teardown, clean-room retesting, or replacing a failed QDevice. Every
  member must be online and the cluster quorate. Under the cluster
  control-plane lock it first removes the QDevice through the supported
  Proxmox cluster command, clears any stale registration, and removes the
  QDevice's host-key trust from every member; it refuses to purge the
  external host if cluster-side detachment cannot be proven. It then removes
  the control node's setup SSH key, QNetd TLS/NSS identity, Corosync/QDevice
  packages, services, account, and package-specific state from the dedicated
  QDevice. It does not run `apt autoremove` or erase general journals. When
  the QDevice host cannot be reached, or is not the registered machine, it
  never touches it. It offers a forced removal instead, only after the
  operator confirms the old machine was removed from Tailscale. It then
  runs `pvecm qdevice remove` and removes the QDevice client, certificates,
  and host-key trust from every Proxmox node. Read
  [`QDEVICE_MANUAL_SETUP.md`](QDEVICE_MANUAL_SETUP.md) and its
  exact `GO` destructive confirmation before running:

  ```bash
  scripts/user_callable/qdevice/remove_qdevice.sh [qdevice-host]
  ```

- `scripts/user_callable/hosts/add_proxmox_host.sh` is the host setup workflow. It depends on
  `scripts/lib/`, layered `config/` configuration, either iDRAC/Redfish or a manual Live
  Linux disk inventory, Tailscale, the reviewed Proxmox ISO, and Proxmox
  cluster tools.
- `scripts/host_runtime/app-ha-guest-role-hook.sh` is the generic QEMU lifecycle hook installed
  on every mox node. It depends on installed `config.sh`,
  `cluster_registry.py`, Proxmox/QEMU/ZFS tools, and the deferred-cleanup
  service; it protects production starts and rejects unsafe staging starts.
- `scripts/tests/test_add_proxmox_host.py` tests host safety, QDevice parity, resume,
  storage, and networking invariants without installing a host.
- `scripts/user_callable/hosts/sync_cluster_network.sh` is workflow 15. It
  runs from the workstation over strict mox SSH and shares
  `scripts/lib/guest_network_settings.sh` with host setup to render each
  host's `/etc/app-ha/guest-network.env`.
- `scripts/tests/test_app_ha_guest_role_hook.py` tests fail-closed production/staging
  admission and cleanup behavior against synthetic Proxmox and registry state.
- `scripts/host_runtime/prepare_prod_iso.sh` is installed on every mox and atomically
  downloads or reuses the hash-addressed Ubuntu source cache before invoking
  the host-installed `build_ubuntu_autoinstall.py`; it depends on `curl`,
  Python, and `xorriso`.
- `scripts/host_runtime/build_ubuntu_autoinstall.py` verifies a configured compatible
  Ubuntu source and builds the answer-file-driven per-VM installer when
  `PROD_GUEST_OS_INSTALL_MODE=ubuntu-autoinstall`.
- `scripts/tests/test_add_prod_vm.py` tests installer rendering, allocation,
  resume, SSH, HA, and destructive guards for production creation.
- `scripts/user_callable/guests/prod/extend_prod_vm_disk.sh` runs from the workstation. It uses
  strict mox SSH for Proxmox/ZFS work and the operator's `ssh prodN` alias
  for the guest, where it needs Python 3, `findmnt`, `sfdisk`, `tune2fs`,
  `growpart` (`cloud-guest-utils`), and `resize2fs`.
- `scripts/tests/test_extend_prod_vm_disk.py` tests the integer growth-limit
  math, increase parsing and MiB rounding, guest-disk validation, and
  mutation ordering.
- `scripts/user_callable/guests/prod/change_prod_vm_placement.sh` and
  `scripts/user_callable/guests/prod/change_prod_vm_owner.sh` run from the workstation over strict
  mox SSH and share `scripts/lib/prod_ha.sh`. That library handles production
  selection, live HA/replication state, the per-resource lease, registry
  updates, and replication waits.
- `scripts/tests/test_change_prod_vm_placement.py` tests host eligibility,
  add/remove ordering, partial-change repair, and the owner script's
  agreement checks.
- `scripts/user_callable/hosts/remove_proxmox_host.sh` runs from the workstation and uses
  `scripts/lib/host_membership.sh` (QDevice access and parity, control-plane lock,
  node deletion, SSH-trust removal) and `scripts/lib/cluster_control.sh`
  (control-node resolution and the `cluster.conf` update).
  `scripts/tests/test_host_membership.py` tests the choice between graceful and
  forced removal, graceful-removal blockers, the forced-removal plan and
  candidates, and QDevice vote checks.
- `scripts/utilities/patch_staging_clone.sh` is the root-only host wrapper for
  offline clone validation, mounting, and teardown. It is invoked by the
  staging creator and depends on ZFS block-device, filesystem, and Proxmox
  tools plus `patch_staging_guest_tree.py`.
- `scripts/utilities/patch_staging_guest_tree.py` performs descriptor-relative,
  no-symlink guest-tree rewrites for identity, MAC-bound networking, SSH keys,
  Tailscale removal, and the optional pre-network sanitizer.
- `scripts/tests/test_staging_vm.py` tests staging allocation, source-snapshot
  dependency records, safe offline patching, rollback, and read-only dry-run.
- `scripts/user_callable/guests/prod/deploy_hello_app_to_prod.sh` deploys the
  example todo app (`scripts/user_callable/guests/prod/hello_todo/`) onto one
  active registry `prodN`. It builds a static musl binary on the Linux
  workstation, copies it with the Cloudflare Origin CA certificate and key
  over the guest's workstation SSH alias, and installs `hello-todo.service`
  (enabled at boot, `DynamicUser`, data in `/var/lib/hello-todo`). The app
  serves HTTPS on port 443 for any hostname, so a later staging clone answers
  as `stageKprodN.<primary-domain>` unchanged, and keeps at most 100 todos in
  SQLite behind a JSON API under `/api/v1/`. It is a sample application for
  platform smoke-testing, not a fifth platform workflow. Real applications
  still use their own deployment tooling:

  ```bash
  scripts/user_callable/guests/prod/deploy_hello_app_to_prod.sh
  ```

`scripts/lib/`, `scripts/utilities/`, and `scripts/host_runtime/` are the
shared orchestration layer:

- `scripts/lib/config.sh` strictly parses and validates `config/` without evaluating it,
  derives mox addressing and the VM slot range, and supplies strict
  SSH/ProxyJump helpers; it depends on Bash and ordinary validation/SSH
  utilities.
- `scripts/host_runtime/cluster_registry.py` owns schema-validated, secret-free pmxcfs metadata,
  fixed-slot IP and deterministic MAC allocation, mutation locking, cleanup
  records, the `tunnel-productions` listing, slot-setting changes
  (`update-slots`), and reconciliation; it uses the Python standard library
  and, for live reconciliation, `pvesh`.
- `scripts/host_runtime/guest_network.sh` installs and verifies the shared
  guest gateway, its bridge isolation rules, egress NAT, routing tables,
  kernel settings, firewall rules, and the gateway, egress-health, and
  transfer-counter units (`configure`, `apply`, `ensure-gateway`, `verify`).
- `scripts/host_runtime/egress_health.py` is the egress health and fallback
  service, with `validate-targets`, `run`, and `status` commands, and
  `scripts/host_runtime/egress_probe_targets.txt` is its shipped probe list.
- `scripts/host_runtime/transfer_counter.py` keeps the monthly public-NIC
  transfer count.
- `scripts/host_runtime/app_ha_monitor.py` is the monitor agent run on every
  host, the QDevice, and every active production VM (`run`, `check --once`,
  `status`, `test-pulse`, `probe`, `reset-smart-baseline`,
  `validate-config`, `print-unit`); its version is stamped by
  `dev/set_version.py`. `scripts/lib/monitoring.sh`,
  `scripts/lib/monitor_config.py`, `scripts/lib/healthchecks.py`, and
  `scripts/lib/monitor_node_install.sh` deploy it and keep the Healthchecks
  checks in step with membership; see
  [Monitoring and alerting](#monitoring-and-alerting).
- `scripts/host_runtime/tunnel_config.py` renders the deterministic
  Cloudflare Tunnel config from the registry and `cluster.conf`;
  `scripts/host_runtime/tunnel_sync.sh` installs it and restarts replicas one
  host at a time; `scripts/host_runtime/cloudflared_setup.sh` installs
  `cloudflared`, the credentials file, and the replica service.
- `scripts/host_runtime/process_deferred_cleanup.sh` is the bounded, idempotent cleanup worker
  run by the installed systemd timer; unsafe or unreachable work stays
  pending. It depends on `config.sh`, the registry, plus
  Proxmox, QEMU, ZFS, and block-device inspection tools.
- `scripts/utilities/host_storage.py` collects a read-only JSON layout of one host's
  `rpool` (vdevs, member disk serials through LUKS, the boot/ESP vdev,
  removal progress, disks outside the pool, registered ESPs on disks outside
  the pool, open rpool LUKS mappings, crypttab entries, zvols) and renders it
  for `scripts/user_callable/diagnostics/show_proxmox_host_state.sh` and the disk workflows.
- `scripts/host_runtime/rpool_mirror.sh` is the single copy of the host-side mirror logic
  (disk checks, console LUKS preparation, crypttab/initramfs, `zpool add`,
  boot-disk partitioning and ESPs, member replacement, and LUKS retirement)
  used by host setup and workflows 7, 9, and 10.
- `scripts/utilities/storage_state.py` keeps trim, scrub, vdev-removal, and pending
  mirror-addition and member-replacement records on the host;
  `scripts/lib/disk_workflows.sh` holds the workstation plumbing shared by workflows
  6-9.
- `scripts/tests/test_shared_libs.py`, `scripts/tests/test_process_deferred_cleanup.py`,
  and `scripts/tests/test_host_storage.py` test configuration, pmxcfs
  semantics, registry/slot allocation, transactions, cleanup guards, and
  `rpool` layout parsing; `scripts/tests/test_guest_network.py`,
  `scripts/tests/test_egress_health.py`, `scripts/tests/test_transfer_counter.py`,
  and `scripts/tests/test_tunnel_config.py` test the networking runtime.
- [`scripts/lib/README.md`](../scripts/lib/README.md) documents direct library interfaces.

`config/` is the only repository configuration layer for these workflows:
`cluster.conf` contains shared non-secret policy, each `moxN.conf` contains
non-secret host/hardware inputs, and the local Git-ignored `secrets.env` holds
only the allowed setup/console secrets at mode `0600`. All workflows depend on
`scripts/lib/config.sh`; never source these files directly, commit secret values, or
copy secrets into pmxcfs.

`<config>/artifacts/hosts/` contains per-host generated media, state, logs,
setup keys, GPT backups, and LUKS header backups. Keep those sensitive
recovery artifacts off Git and back them up securely.

## Required operational sequence

The order matters. Do not create guests before host and cluster convergence.

### 1. Confirm provider and external prerequisites

- Verify the FiberState private VLAN is shared Layer 2 at 10 Gbps.
- Verify every public IP, gateway, prefix, public NIC MAC, private NIC MAC,
  and iDRAC address against the provider inventory.
- Connect the iDRAC/IPMI VPN and Tailscale.
- Verify the QDevice is independent, patched, reachable by its Tailscale name,
  and reports the one literal Tailscale IPv4 resolved by the workstation.
- Configure the reviewed Proxmox VE 9.2-1 source ISO's HTTPS URL and hash in
  `cluster.conf`. Configure a public HTTPS production guest OS ISO URL, its
  independently reviewed hash, and either `ubuntu-autoinstall` or `manual`
  install mode.
- Create the cluster's Cloudflare Tunnel once with `cloudflared tunnel login`
  and `cloudflared tunnel create bmac-<cluster name>`, and copy its account
  ID, tunnel ID, and secret into `cluster.conf` and `secrets.env` (see
  [Cloudflare setup](BMAC_SIMPLIFIED_NETWORKING.md#cloudflare-setup)).
- Arrange strict workstation SSH trust for `moxN` names and back up the local
  artifacts location securely.

### 2. Prepare and validate configuration

Create local mode-`0600` `config/secrets.env` from
[`config/secrets_dot_env`](../config/secrets_dot_env), then validate one host without
mutation:

```bash
scripts/lib/config.sh \
  --check --host mox1 --require-secrets
```

Review `cluster.conf`, then each `moxN.conf`. The first host must be the
control node (`PROXMOX_CONTROL_NODE`, normally `mox1`), which creates the
cluster. Setup recommends the lowest free slot for each later host.

### 3. Install hosts one at a time

Run the host workflow first for the control node, then for each later host,
one at a time:

```bash
scripts/user_callable/hosts/add_proxmox_host.sh --host mox1 --run-boot-tests
scripts/user_callable/hosts/add_proxmox_host.sh --host mox2 --run-boot-tests
```

Keep every existing node online while adding another. Enter the LUKS
passphrase through iDRAC when a boot requires it; setup runs its conversion
helpers itself over SSH with the temporary root-only key file after you
confirm each one. Preserve every exported
header. If a phase fails, correct the condition and rerun the identical
command; do not skip the phase marker or manually improvise the next step.

After each join, verify membership, exact QDevice parity, private
migration/replication, the shared guest gateway and egress health, the
tunnel replica (connected over HTTP/2, config current), shared tools, timers,
and registry health before proceeding.

### 4. Create a generic production VM

```bash
scripts/user_callable/guests/prod/add_prod_vm.sh --dry-run
scripts/user_callable/guests/prod/add_prod_vm.sh
```

Choose at least two online placement nodes with enough CPU, RAM, and
`local-zfs` capacity. Complete console/QGA/SSH verification, every initial
replication, the strict HA rule, and the tunnel config update on every host
before application handoff.

### 5. Deploy the application and external ingress

The generic creator intentionally stops at an empty Ubuntu application host.
Deploy the intended application, origin TLS certificate, and health endpoint
over the private address. Then follow the Cloudflare steps the creator
printed for the domain (proxied `@` and `*` CNAMEs to the tunnel, Always Use
HTTPS, and bot protection) and verify the public path.

For a first HTTPS smoke test after `add_prod_vm.sh` finishes,
`scripts/user_callable/guests/prod/deploy_hello_app_to_prod.sh` installs the example todo app on
one active `prodN` as a systemd service serving HTTPS on port 443. It
requires a Linux workstation with the Rust `x86_64-unknown-linux-musl` target
and `musl-tools`, a working `ssh prodN` alias from
production creation, `curl` on the guest, and a Cloudflare Origin CA
certificate covering the primary domain and `*.<primary-domain>`. Real
applications still use their own deployment tooling after the generic VM is
ready.

### 6. Create staging only after production is healthy

First supply an idempotent app-specific sanitizer or choose link-down:

```bash
scripts/user_callable/guests/staging/add_staging_vm.sh --dry-run
scripts/user_callable/guests/staging/add_staging_vm.sh \
  --sanitizer /absolute/path/to/staging-sanitizer.sh
```

Validate the clone's application data and new SSH host key before use.
Remember that production can evict it without preserving staging runtime
state.

### 7. Operate and drill the complete system

Monitor quorum, QDevice, each host's egress mode and transfer count, tunnel
replicas and config drift, application health, replication age/failures,
pool health/capacity, LUKS header backups, and pending cleanup. Exercise planned and unplanned recovery
before depending on the platform.

## Host workflow in detail

`scripts/user_callable/hosts/add_proxmox_host.sh` is the host workflow. It is destructive and
resumable:

1. **Load and fingerprint inputs.** It strictly loads `cluster.conf`,
   `moxN.conf`, and `secrets.env`; derives fixed addressing; verifies
   mirror pairs (pair 0 mandatory, later gaps allowed); computes disk-layout
   and secret-inclusive setup
   fingerprints; and requires explicit acceptance if stored setup inputs
   changed.
2. **Discover hardware.** In iDRAC mode, Redfish inventories the target before
   mutation. In manual mode, the workflow validates configured exact byte
   capacities previously gathered by running `scripts/user_callable/hosts/cluster_setup_prereq.sh` in a
   Linux Live environment. Both paths validate serial-selected pairs and enough
   reserved tail space for mirror-0 LUKS conversion.
3. **Build host media.** It verifies the source hash; obtains the reviewed
   `proxmox-auto-install-assistant`; creates an unattended answer and
   before-network first-boot script; reserves a never-reused one-time
   Tailscale key digest; and validates/inspects the generated ISO.
4. **Install on the target host.** The operator attaches the generated ISO
   through iDRAC virtual media or other target-supported boot media, confirms
   the exact two mirror-0 serials to erase, watches the install and reboot,
   removes the media, and confirms a visible login prompt. First boot installs
   the public management-port guard, administrator/setup keys, and Tailscale.
5. **Bootstrap and configure storage.** The workstation reaches only the
   attested Tailscale address with its setup key, verifies host and disk
   identity, installs prerequisites, applies the selected clear or LUKS policy,
   and adds configured partitioned mirror vdevs. LUKS mode converts mirror 0,
   verifies the shared passphrase, installs `decrypt_keyctl`, refreshes boot
   metadata, and exports headers. Selected boot drills test each member and
   the restored final state.
6. **Build the private plane.** It resolves public/private physical NICs by
   MAC, requires 10-Gbps carrier, creates `vmbr-private`, and stops for a
   provider-console/peer `PRIVATE VLAN VERIFIED` gate.
7. **Create or join the cluster.** The control node runs `pvecm create` when
   no cluster exists; every later host reserves its registry slot and joins
   through the control node. Link 0 is private, link 1 is
   Tailscale. The workflow reconciles secure migration/replication and
   QDevice parity only while all configured nodes are online.
8. **Install orchestration tools** (`install_shared_orchestration_tools`).
   It distributes `config.sh`, `cluster_registry.py`,
   `process_deferred_cleanup.sh`, `guest_network.sh`, `egress_health.py`,
   `egress_probe_targets.txt`, `transfer_counter.py`, `tunnel_config.py`,
   `tunnel_sync.sh`, `cloudflared_setup.sh`, the generic lifecycle hook, the
   production ISO tools, and `cluster.conf` to all online mox nodes; installs
   the deferred-cleanup and tunnel-sync timers; and initializes the registry
   once.
9. **Configure the guest network** (`configure_guest_network`). It renders
   this host's `/etc/app-ha/guest-network.env` and runs
   `guest_network.sh configure`: the shared gateway veth, bridge isolation
   and egress nftables, route table 200 and the per-host fallback tables,
   kernel settings, Proxmox firewall rules, the bridge post-up hook, probe
   target validation, and the gateway, egress-health, and transfer-counter
   units.
10. **Verify the lifecycle guard** (`install_guest_role_hook`). It checks
    that the hook on local snippet storage, the cleanup worker, the registry,
    and the production ISO tools are installed and valid.
11. **Install the tunnel replica** (`configure_tunnel`). The tunnel secret is
    copied to the host as a mode-`0600` file and shredded on both ends after
    `cloudflared_setup.sh` writes the credentials file;
    `tunnel_sync.sh --local` then writes the config and starts
    `app-ha-cloudflared.service`, and `tunnel_sync.sh --check` must pass.
12. **Verify the end state** (`final_verify`). It checks serial-selected
    mirror membership and ZFS health in both modes; LUKS mode additionally
    checks encrypted mappers, crypttab/initramfs, and off-host headers. It
    also verifies no swap, boot state, private address, Tailscale, quorum,
    the host guard, `guest_network.sh verify`, egress health and transfer
    counter status, an active replica whose recent connections are all
    `protocol=http2` and never `quic`, `tunnel_sync.sh --check`, hooks, and
    timers.

Important installed host-local paths and units include:

```text
/usr/local/lib/app-ha-proxmox/lib/config.sh
/usr/local/lib/app-ha-proxmox/lib/cluster_registry.py
/usr/local/lib/app-ha-proxmox/lib/process_deferred_cleanup.sh
/usr/local/lib/app-ha-proxmox/lib/guest_network.sh
/usr/local/lib/app-ha-proxmox/lib/egress_health.py
/usr/local/lib/app-ha-proxmox/lib/egress_probe_targets.txt
/usr/local/lib/app-ha-proxmox/lib/transfer_counter.py
/usr/local/lib/app-ha-proxmox/lib/tunnel_config.py
/usr/local/lib/app-ha-proxmox/lib/tunnel_sync.sh
/usr/local/lib/app-ha-proxmox/lib/cloudflared_setup.sh
/usr/local/lib/app-ha-proxmox/lib/app_ha_monitor.py
/usr/local/lib/app-ha-proxmox/config/cluster.conf
/var/lib/vz/snippets/app-ha-guest-role-hook.sh
/etc/app-ha/guest-network.env
/etc/nftables.d/app-ha-guest-network.nft
/etc/sysctl.d/91-app-ha-guest-network.conf
/etc/cloudflared/bmac-tunnel.json
/etc/cloudflared/config.yml
/var/lib/app-ha/egress-targets.json
/var/lib/app-ha/transfer.json
/run/app-ha/egress-health.json
/etc/app-ha-monitor/config.json              (monitoring enabled)
/var/lib/app-ha-monitor/state.json           (monitoring enabled)

app-ha-host-guard.service
app-ha-guest-network.service
app-ha-egress-health.service
app-ha-transfer-counter.service
app-ha-transfer-counter.timer
app-ha-cloudflared.service
app-ha-tunnel-sync.service
app-ha-tunnel-sync.timer
app-ha-deferred-cleanup.service
app-ha-deferred-cleanup.timer
app-ha-monitor.service                       (monitoring enabled)
```

The hook is stored on local snippet storage on every host because guests may
move; it is not a custom pmxcfs registry file.

### Updating the deployed cluster runtime

After changing the registry, deferred cleanup worker, lifecycle hook, the
monitor agent (`app_ha_monitor.py`), or any of the networking runtime files
(`guest_network.sh`, `egress_health.py`, `egress_probe_targets.txt`,
`transfer_counter.py`, `tunnel_config.py`, `tunnel_sync.sh`,
`cloudflared_setup.sh`) in this checkout, update their installed copies
before running a guest workflow:

```bash
scripts/user_callable/hosts/update_cluster_runtime.sh --dry-run
scripts/user_callable/hosts/update_cluster_runtime.sh
```

The updater requires every configured Proxmox node to be online and directly
reachable from the workstation. It stages and validates the full bundle on
all nodes before making changes, atomically replaces the installed files and
local snippet (adding any managed file a node lacks; a rollback removes files
that were absent before), restarts `app-ha-egress-health.service`, runs
`tunnel_sync.sh --all`, verifies hashes, registry readability, and
`tunnel_sync.sh --check` on every node, and restores the prior files if the
update fails. It does not restart VMs or require existing guests to reattach
their hook. It does not change networking settings; use
`sync_cluster_network.sh` for those.

The host commit step only try-restarts `app-ha-monitor.service` where its
unit exists, so a running agent picks up its new program. With
`MONITORING_ENABLED=true`, a monitoring preflight runs before the
confirmation and before any node is staged: every host, the QDevice, and
every active production VM must answer BatchMode SSH, and the API key must
work; the dry run stops after showing the Healthchecks plan (checks to
create, update, keep, or delete as obsolete, and the budget). After the
hosts are verified, `monitoring_sync` installs the agent, unit, and config
on every node (the unit is written by this phase, not by the host commit
step) and waits for every check's first ping. Obsolete checks are deleted
only after the operator agrees, or with `--delete-obsolete-checks`. A
failure in this phase leaves the host runtime in place and says to rerun.
With `MONITORING_ENABLED=false`, agents installed earlier are stopped and
disabled on every node and deleting the cluster's checks is offered behind
the typed confirmation `DELETE CHECKS`; when no host has the unit, nothing is
contacted and no API call is made.

Host setup does not create production VMs, replication jobs, HA resources, or
Cloudflare DNS or zone configuration. Those belong to later workflows and the
operator.

## Production workflow in detail

### Preconditions and input

`add_prod_vm.sh` runs from an administrator workstation and accepts:

```text
--dry-run
--install-timeout-seconds N       default 10800
--qga-timeout-seconds N           default 900
--replication-timeout-seconds N   default 14400
```

It requires a quorate expected cluster, at least two online nodes,
Proxmox VE 9.2+, amd64, `local-zfs`, disk-backed `local` ISO storage,
executable local lifecycle snippets, strict SSH trust, a reviewed
guest OS HTTPS source URL/hash, and the installed
registry, tunnel synchronizer, and ISO preparation tools.

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
`stageKprodN.<primary-FQDN>` staging names. It live-checks a candidate VMID,
then one cluster-wide registry
mutation reserves the lowest free `prodN` in `1..MAX_PROD_VMS`, its fixed slot
address, a deterministic MAC, VMID, purpose, domains, placement, and
specification.

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
- detaches and deletes per-VM media only after confirmed installation, then
  offers (default yes) to delete the vanilla source cache under its cache
  lock; a kept cache is reused by later installs.

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

Ubuntu uses direct storage layout, exactly one direct ext4 root, its fixed
production slot address (`/24`) on interface `lan0`, `.10` gateway,
configured DNS, OpenSSH, QGA, and no Tailscale.

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
placement, domain, or SSH state causes a closed failure. A missing previously
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
10. Record HA/replication metadata, run `tunnel_sync.sh --all` so every
    online replica serves the primary, alias, and `stage1..N` FQDNs, and
    transition `ready` to `active`.

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

After completion, the tunnel routes the VM's FQDNs even though no
application is deployed. Cloudflare shows an error page until application
code, services, origin certificate, and health endpoint are installed and
the domain's DNS records point at the tunnel. The creator prints the
Cloudflare steps for the domain. It never deploys application code.

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

It prompts for source, compute sizing, initial NIC link-down, an optional
sanitizer, and finally whether to start. There is no domain prompt.

One mutation reserves a fresh `stageKprodN`, its fixed slot address
(`prodN + K`), deterministic MAC, live-free VMID, one standby placement,
inherited purpose/storage/replication policy, and the exact
`stageKprodN.<source-primary-FQDN>` domain. The registry chooses the lowest
unused `K` of that source in `1..MAX_STAGING_VMS_PER_PROD`; callers cannot
provide a stage number. Separately, the standby's
`MAX_STAGING_VM_COUNT_ON_THIS_HOST` limits how many staging VMs that host
runs.

Creation then:

1. Transitions to `snapshotting`.
2. Records a unique
   `stg-base-stageKprodN-<short-UTC>` intent in pmxcfs, including the source owner,
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

When the source already has staging guests, the operator may instead share
one of their verified snapshots. Steps 1 through 5 then become one
`share-staging-snapshot` registry call, which refuses once every existing user
of that snapshot is `cleanup_pending` or `failed`, followed by a fresh check
of every recorded ZFS GUID. The deferred-cleanup worker keeps a shared
snapshot while any other user is not being cleaned up, and otherwise waits for
the other users' clones to be destroyed before deleting it.

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
  registry MAC, its staging slot address (`/24`), `.10` gateway, and
  configured DNS;
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

### Start and rollback

After offline validation there is nothing to route: the staging FQDN has
been in every replica's tunnel config since the source production VM was
created, so it reaches the slot as soon as the VM's web server answers.
Declining the final start leaves a valid stopped VM. If production has moved
onto the staging node, the creator leaves staging stopped. A hook-refused or
timed-out optional start also preserves the valid stopped environment.

Before creation commit, an error trap tries in this order:

1. stop and destroy only a VM whose exact identity/tags/disks match;
2. prove the VM absent, then destroy only the unreferenced clone with the
   expected snapshot origin;
3. delete the source-owned Proxmox snapshot;
4. trigger replication and prove every tracked snapshot copy absent;
5. clear/release the registry allocation, freeing the slot.

If any destructive fact is unavailable or ambiguous, the registry resource is
retained as `cleanup_pending`. Idempotent per-node cleanup records preserve
the VM, volume, and snapshot identities for the timer worker. An
offline-patch failure never attaches or starts the clone.

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
3. discovers local `stageKprodN` VMs with staging and evictable tags;
4. validates every candidate before the first stop: unique registry
   UUID/VMID/name, local owner/placement, non-HA state, exact tags, exact
   `scsi0`/fresh EFI identity, verified source snapshot dependency, and
   matching origin and GUID;
5. force-stops (`qm stop`, a hard power-off) every candidate that is running,
   then verifies that all of them are stopped;
6. for each candidate, revalidates it, marks an `active` one `stopping`,
   revalidates again, and commits a durable cleanup plan (local `destroy-vm`
   and `destroy-volume` records plus `delete-snapshot` on every node) and the
   `cleanup_pending` state;
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
`post-start` also triggers deferred cleanup without blocking the VM. Then, for
a production start this hook reserved, it records the local node as the VM's
registry owner, with the other placement hosts as replication targets
(`cluster_registry.py record-owner`). Because HA uses `failback=0`, a VM that
failed over stays on its new host, and this keeps the registry in step with
it. The registry skips the update while the VM is not `active` or while a
workflow holds its orchestration lease (that workflow records the owner
itself); a failure is only logged and never affects the running VM.

### Full host-failure sequence

If a mox hosting production fails:

1. Corosync and QDevice determine quorum and fence/HA authority.
2. The failed host's tunnel replica disappears; Cloudflare sends new requests
   to the remaining replicas within seconds. Nothing else about the gateway
   or tunnel changes on the survivors.
3. Proxmox HA chooses an eligible node with a replica.
4. The production pre-start hook force-stops any verified disposable
   staging on that destination and queues its destruction and cross-node
   cleanup for after production starts.
5. Proxmox starts production from the most recent successful replica with the
   same VMID, MAC, and slot address.
6. The private switch relearns the production MAC. Every replica continues
   targeting the same slot address; no tunnel config change is needed.
7. The restarted VM's ARP for `.10` is answered by its new host, so its
   egress leaves through that host's connection. Outbound sessions from
   before the failure are gone.
8. Requests succeed again once the application answers.

The production RPO is replication age; the recovery-time objective includes
failure detection, fencing/quorum, staging eviction, VM boot, Layer-2/ARP
convergence, and application start.

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
destroy-vm
destroy-volume
delete-snapshot
remove-replication
```

The worker acts only while the resource UUID still matches and the resource
is `cleanup_pending`. No cleanup action touches networking or the tunnel:

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
required VM, volume, and snapshot record is complete, the worker
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
ssh -J root@mox1 root@10.213.0.110
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
│   └── stageKprodN.json
├── orchestration/
│   └── prodN.json
├── deferred-cleanup/
│   └── cleanup-<24-lowercase-hex>.json
├── history/
│   └── <UTC-without-colons>-<resource>-<8-lowercase-hex>.json
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
- `network`: `cidr`, `guest_gateway` (`.10/24`), `mox_host_range`, and
  `vm_slots` (`start`, `max_production`, `max_staging_per_production`);
- limits: max hosts 10, `max_production` equal to the slots'
  `max_production`, `max_staging` equal to
  `max_production * max_staging_per_production`, and reservation TTL;
- exact production/staging/evictable tags;
- production and staging compute/storage/replication defaults;
- the allowed state-transition graph.

Writers: `cluster_registry.py init` (`--network`, `--guest-gateway`,
`--mox-ip-start`, `--mox-ip-end`, `--vm-slot-start`, `--max-production`,
`--max-staging-per-production`, and defaults), invoked by host setup, writes
it only if absent. An existing policy is returned and never silently
replaced. `update-slots`, run by `sync_cluster_network.sh`, is the only
command that changes it: it replaces `vm_slots` and the derived limits, and
refuses if any registered guest's address or name would move.

Readers: every registry command validates it (`list --record-type policy`
prints it); production/staging creators, the tunnel config renderer,
lifecycle hook, cleanup worker, reconciliation, and operators depend on the
resulting policy.

Recovery note: changing `cluster.conf` does not migrate an existing policy by
itself. Slot settings are pushed deliberately with `sync_cluster_network.sh`;
treat any other mismatch as an explicit schema/policy migration, not a
reason to delete this file.

### `resources/prodN.json` and `resources/stageKprodN.json`

Each `record_type: "resource"` has these exact top-level fields:

```text
schema_version, record_type, id, allocation_id, request_fingerprint,
kind, name, index, source, vmid, ip, mac, state, purpose, domains,
placement, initial_node, owner_node, spec, proxmox,
created_at, updated_at, revision
```

Nested data records the purpose/source, primary/alias/staging domains,
cores/RAM/disk/allocation/replication interval, HA nodes, replication targets,
volume, and snapshot dependency. `spec.disk_gib` is the creation-time root
disk size. After `extend_prod_vm_disk.sh` grows a production disk, the
optional `spec.disk_bytes` holds the exact current size: a whole number of
MiB, always larger than `disk_gib`, and changed only through
`update --disk-bytes`, which allows growth only. Staging records keep the
source's `disk_gib`. A staging snapshot additionally records
source resource, root and all snapshotted volumes, original owner, root and
per-volume GUID maps, verification flag, sole dependent, and refcount 1. The
dependent and refcount describe that record's own reference: several staging
records may name one verified snapshot of the same source when their copies of
the metadata are identical, and the snapshot is deleted only after the last of
them has lost its clone.

Writers: only registry mutations write them. Production/staging creators call
allocate/update/snapshot commands; the lifecycle hook marks evicted staging
`stopping` and commits cleanup state; reconciliation can mark an expired
VM-less reservation failed; release/finalization removes the live record
after writing history.

Production records may carry an optional top-level `monitoring` field,
`{"probe": {"enabled", "path", "expected_status", "expected_text", "fqdns"}}`,
holding the ingress health-check probe that the QDevice's monitor agent runs
through Cloudflare. It holds no secrets. Records created before this field
existed, and every new record, lack it; readers treat that as a disabled
probe. Staging records never have it. `path` is an absolute URL path of at
most 200 characters, `expected_status` is 100-599, and `expected_text` is
null or one line of at most 200 characters. `fqdns` must be the record's own
primary domain or aliases and defaults to the primary. It is set only with
`cluster_registry.py update <name> --expected-revision N --monitoring-json
'<json>'` (cleared with `--clear-monitoring`), which the QDevice page's
Health-check probes tile runs. A later domain change drops FQDNs that no
longer belong. `cluster_registry.py monitoring-probes` lists every
production record's probe, in slot order, for rendering the QDevice agent's
config.

Readers: all registry listings/invariant checks, both creators,
`tunnel-productions` (and so the tunnel config renderer),
`monitoring-probes` (and so the monitoring config renderer), lifecycle hook,
cleanup worker, reconciliation, diagnostics, and operators.

Uniqueness is cluster-wide for name, VMID, IP, MAC, domain, allocation ID,
and production purpose. Every IP is the fixed slot its name implies. Allocation IDs and request fingerprints make safe
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
- deferred cleanup has a hard 4096-record limit;
- at most 512 completed cleanup records are retained, and a completed record
  is pruned only after its resource UUID is no longer active;
- current resources remain until validated release/finalization;
- production orchestration remains until its resource is released.

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
snapshot, and cleanup facts. Reconciliation does nothing unless
`--apply` is supplied. Apply can only:

- mark an expired `reserved`/`provisioning` resource failed when no VM exists;
- mark explicitly reported, known cleanup IDs completed.

It never creates, starts, stops, moves, destroys, adopts, or repairs Proxmox
resources. After quorum/metadata recovery, validate `list`,
`list --record-type policy`, `tunnel-productions`, and `reconcile --live`,
then rerun `tunnel_sync.sh --all` and the cleanup worker.

Malformed or missing current records have no generic auto-restore path.
Freeze mutating workflows, preserve forensic copies, restore only from a
known-good Proxmox/pmxcfs backup or a verified quorate source, validate every
schema/invariant, and reconcile live infrastructure before syncing the tunnel
config again. Release history alone is insufficient.

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
  the managed guest-forward rules (`app-ha managed guest network forward from
  VM slots` and `... from mox hosts`).

Proxmox owns locking, schema, and permissions for those records. Do not edit
them as app-ha JSON. The scripts set `firewall=1` on guest NICs but do not
create a complete per-guest firewall policy. The lifecycle snippet under
`/var/lib/vz/snippets`, host-local `/run` locks/reservations and status,
local systemd units, nftables and sysctl files, guest-network settings,
egress and transfer state under `/var/lib/app-ha`, and the tunnel
credentials and config under `/etc/cloudflared` are not pmxcfs.

## Security invariants

### Secrets and recovery material

- Keep `config/secrets.env` local, Git-ignored, non-symlinked, owned by root or
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

- Public application ingress is only the Cloudflare Tunnel. No mox listens
  on, DNATs, or forwards any public port to a guest; never add one.
- Public TCP 22, 8006, and 3128 are explicitly dropped for the host public
  IPv4. Audit all other IPv4/IPv6 listeners and provider firewall rules; the
  generated chain otherwise accepts.
- Keep Proxmox SSH/UI/API, QDevice administration, Corosync fallback, and the
  replica's metrics endpoint (`127.0.0.1:20241` only) on management paths.
- Only FQDNs of registered production guests and their staging slots appear
  in the tunnel config, each mapped to its own slot's port 443. Do not expose
  PostgreSQL or application-internal services through the tunnel.
- The tunnel credentials file is readable only by root and the `cloudflared`
  user; the replica runs unprivileged under a systemd sandbox. Anyone holding
  the tunnel secret can run a replica that receives a share of the cluster's
  traffic, so treat `CLOUDFLARE_TUNNEL_SECRET` like the other secrets, and
  never copy the workstation's `cert.pem` to a mox or into the checkout.
- `GUEST_GATEWAY_MAC` and `.10` must never appear on the VLAN; the bridge
  rules on each host's physical private-VLAN NIC are what keep every host's
  gateway private to its own guests.
- The shared `/24` is not isolation. Apply Proxmox per-VM and guest firewall
  policy to prevent staging-to-production database/API access, guest-to-host
  management access, and unnecessary east-west traffic. The current scripts
  set NIC firewall flags but do not install this complete policy.
- Guest egress currently permits every destination/port after source-range
  validation, through whichever mox's connection is in use. Add
  least-privilege egress controls for mail, webhooks, payments, cloud APIs,
  metadata endpoints, and other high-risk integrations.

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

- Give every guest a valid origin certificate for each FQDN it serves; the
  replica verifies it against `originServerName`. Keep Always Use HTTPS on
  for every domain.
- Cloudflare terminates edge TLS and the replica opens a new TLS connection
  to the guest, so client identity arrives only in headers. Sanitize and
  trust `CF-Connecting-IP`, `X-Forwarded-For`, `X-Forwarded-Proto`, and
  `Host` only at a controlled application boundary.
- Unknown FQDNs must remain `http_status:404` through the catch-all. Never
  add a catch-all production fallback for staging names.
- For a web app with an API, keep the `/api/` WAF skip rule first and Bot
  Fight Mode off, so API calls are never challenged while web pages keep
  Super Bot Fight Mode. The application must implement its own API
  protection.
- An application health endpoint should test critical service readiness
  without making an optional dependency look like a platform failure.

### Staging data

- Treat every staging zvol, snapshot, local artifact, sanitizer, and log as
  production-derived sensitive data.
- Supply an application-specific sanitizer before enabling networking. A
  generic hostname/key/Tailscale reset does not remove production secrets.
- Use different credentials, destinations, webhook endpoints, queues,
  certificates, and databases for staging.
- A source snapshot is neither a backup nor a safe long-term data-sharing
  mechanism. Retain it only while the exact clone dependency requires it.

### Monitoring

- `HEALTHCHECKS_API_KEY` stays on the workstation in `secrets.env`. It is
  passed to `healthchecks.py` only through that process's environment, never
  in argv, the environment of a remote command, logs, or pmxcfs.
- Each node receives only its own check's ping URL, in
  `/etc/app-ha-monitor/config.json` (root, `0600`); a compromised node can
  only affect its own check.
- Ping URLs and check UUIDs (also in `update_url`, `pause_url`, and
  `resume_url`) never appear in logs, the UI protocol stream, or dashboard
  output: the agent logs `ping=<redacted>`, and the workstation tools name
  checks by name only. Ping URLs exist on the workstation only in the sync's
  private `0600` run directory, removed when it ends.
- The registry's `monitoring` field holds probe settings only.
- No inbound port is added anywhere; the only new network permission is the
  Tailscale ICMP rule from `tag:proxmox-qdevice` to `tag:proxmox-host`.
- The agent runs as root under systemd sandboxing (`NoNewPrivileges`,
  `PrivateTmp`, `ProtectSystem=strict`, `ProtectHome`, writes only to
  `/var/lib/app-ha-monitor`).
- Staging clones inherit a production VM's agent config, including its ping
  URL; the hostname guard (in the agent and the unit's `ExecCondition`)
  keeps them from using it.
- Ingress probes send no credentials and follow no redirects. Pulse bodies
  hold host, pool, unit, and FQDN names and drive models and serials, never
  secrets or certificate contents.

## Failure cases and recovery

### Complete mox failure

Effect:

- its tunnel replica, its own gateway, and its host-local staging VMs
  disappear; Cloudflare sends every request to the remaining replicas;
- Corosync/QDevice decide whether survivors are quorate;
- outbound sessions of guests that ran there are lost;
- Cloudflare and Proxmox HA recover independently;
- production restarts from the most recent replica after destination staging
  eviction, at the same slot address, and egresses through its new host;
- any host that was using the failed host as its egress fallback probes
  again and chooses another.

Recovery:

- do not manually start a second copy;
- verify fencing/quorum, QDevice votes, HA placement, replica age, staging
  cleanup queue, MAC/ARP convergence, backend health, the remaining
  replicas' connections, and each survivor's egress mode;
- a restored host's replica reconnects and its tunnel-sync timer brings its
  config up to date; nothing in Cloudflare changes.

### A host's internet connection down or transfer cap reached

Effect: production placement does not change. The host's replica loses its
Cloudflare connections and the other replicas carry its guests' traffic over
the VLAN. After 3 failed checks (about 15 seconds or more), its egress health
service points table 200 at another mox whose connection works; open
outbound connections of its guests break and new ones leave through the
fallback host's public IP. After 5 good checks of its own connection it
returns. A host that reaches its provider's transfer cap behaves the same
once the provider cuts it off.

Recovery: check `egress_health.py status` and `transfer_counter.py status`
on the host, the provider's status, and external allowlists (they must
already list every mox public IP). With every host disconnected, ingress and
egress are both unavailable. If one host repeatedly carries most of the
ingress and reaches its cap, see the transfer-cap caveat under
[Failover](BMAC_SIMPLIFIED_NETWORKING.md#failover).

### Tunnel replica down

Effect: if `app-ha-cloudflared.service` stops, crashes, or cannot connect on
one host, Cloudflare uses the other replicas; ingress is unaffected while at
least one replica is connected. Egress is unaffected. A replica connected
over QUIC instead of HTTP/2 risks throttling by FiberState's UDP filter.

Recovery: inspect the unit, its journal (`Registered tunnel connection ...
protocol=http2`), `http://127.0.0.1:20241/ready`, the credentials file's
presence and mode, and outbound TCP 7844. Reinstall the credentials with
`sync_cluster_network.sh` if they were lost or the tunnel was recreated.

### Production application or VM failure

An application process failure may not be a Proxmox HA failure. The
application can fail while the VM remains running, and application service
supervision and alerts must recover it. A stopped/crashed QEMU resource is
governed by Proxmox HA according to `max_restart=3` and `max_relocate=1`.

Never infer HA success from ping alone. Verify the complete
Cloudflare-to-tunnel-to-application response and critical dependencies.

### Private VLAN failure

Effect: preferred Corosync link 0, migration, ZFS replication, replicas
reaching guests on other hosts, egress fallback between hosts, guest
networking, and direct guest administration are impaired. Corosync may remain
up on Tailscale link 1, but that does not restore the data plane. A replica
whose host cannot reach a guest's slot returns an error for that request,
because Cloudflare cannot tell which replicas can reach which guests.

Recovery: stop membership, migration, staging creation, and placement changes.
Repair Layer 2 and verify carrier/speed, direct neighbors, MTU, ARP/MAC moves,
replication, cross-host replica-to-guest flows, and that `.10` and
`GUEST_GATEWAY_MAC` are still absent from the VLAN before resuming.

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
identity, and voting state. `scripts/user_callable/diagnostics/show_qdevice_state.sh` reports which
case applies. When the QDevice machine is lost, replace it:

1. `scripts/user_callable/qdevice/remove_qdevice.sh` removes it forcefully, without contacting it,
   after you confirm it was removed from Tailscale;
2. prepare a new machine with `docs/QDEVICE_MANUAL_SETUP.md`;
3. `scripts/user_callable/qdevice/add_qdevice.sh` adds it.

Until the replacement is added, losing any one member of an even cluster
loses quorum.

### Quorum loss

pmxcfs can become non-writable and HA authority is unsafe. Registry
mutations, and so production and staging creation and removal, stop. The
tunnel replicas keep serving their installed config and every host keeps
providing its gateway, because neither depends on quorum at run time.

Recover quorum through Corosync/QDevice and provider connectivity. Do not
force expected votes or manually edit replicated state as an ordinary
runbook. After quorum returns, reconcile the registry, HA resources, and
cleanup, and confirm `tunnel_sync.sh --check` on every host.

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

### Tunnel config drift

A host that was offline, or failed during `tunnel_sync.sh --all`, while a
production VM was added or removed keeps its previous config. It may then
answer a new production VM's FQDNs with 404, or still route a removed VM's
FQDNs to a now-empty slot, for the requests Cloudflare happens to send it.
Staging changes never cause drift. Each host's `app-ha-tunnel-sync.timer`
renders the config from the registry every five minutes and installs it if
it changed.

Recovery: run `tunnel_sync.sh --check` on each host (or
`show_prod_vm_state.sh`, which reports the coordinator's), then
`tunnel_sync.sh --all`. If the slot settings or Cloudflare keys themselves
differ from `cluster.conf`, use `sync_cluster_network.sh --dry-run` and then
`sync_cluster_network.sh`.

### pmxcfs registry corruption or stale lock

Schema, secret, symlink, size, filename/record mismatch, and invariant failures
stop all dependent automation. Follow the locking and recovery procedure in
[Exact pmxcfs inventory](#exact-pmxcfs-inventory). Do not repair JSON while an
allocator lock or creator lease is active.

### Configuration drift

The strict loader rejects unknown/misplaced keys and formula changes.
Existing registry policy changes only through `update-slots`. If source
config, installed `cluster.conf`, registry policy, guest-network settings,
host runtime, or mox files diverge, stop and reconcile deliberately:
`sync_cluster_network.sh --dry-run` shows networking drift per host, and
`update_cluster_runtime.sh` brings runtime files in line. Copying a new file
over only one mox can split orchestration behavior.

### Cloudflare unavailable or misconfigured

If Cloudflare itself is unavailable, public ingress is unavailable; there is
no direct path to any guest. Guest egress is unaffected.

A domain whose DNS records do not point at this cluster's tunnel, are not
proxied, or sit in a different Cloudflare account is not served. A guest
without a valid origin certificate for the requested FQDN fails the
replica's TLS check. Bot Fight Mode left on for a web app with an API can
challenge API calls, because no rule can skip it. A host can also expose an
unrelated listener because the generated guard is specific rather than
default-deny.

Verify both CNAMEs and their proxy status, Always Use HTTPS, the
bot-protection settings and `/api/` rule order, the tunnel's status and
connected replicas in Cloudflare's Zero Trust dashboard, 404 for unknown
names, and every public listener from a non-Tailscale network.

### Monitoring failures

A node that dies or loses its internet connection goes silent and its check
reports DOWN after its period plus grace; that is the intended signal (the
README's "Interpreting alerts" table combines it with peers' link reports).
If Healthchecks is unreachable, agents keep checking, queue their pulses,
and deliver them when it answers again. If the QDevice is down, the cluster
loses its outside view (ingress probes and Tailscale reachability) as well
as its dead-man coverage of the QDevice itself, which Healthchecks reports.
A membership workflow that cannot update monitoring completes its main job
and says to run `update_cluster_runtime.sh`; `update_cluster_runtime.sh`
rolls back the failing phase's nodes and keeps the checks it created. A check
left in state `new` never alerts, which is why every install waits for each
check's first ping. `show_monitoring_state.sh` shows every check, agent, and
open problem, and checks no node uses.

## Operator checks and drills

### Configuration and registry

```bash
scripts/lib/config.sh --check --host mox1 --require-secrets

ssh root@mox1 \
  /usr/local/lib/app-ha-proxmox/lib/cluster_registry.py list
ssh root@mox1 \
  /usr/local/lib/app-ha-proxmox/lib/cluster_registry.py list --record-type policy
ssh root@mox1 \
  /usr/local/lib/app-ha-proxmox/lib/cluster_registry.py tunnel-productions
ssh root@mox1 \
  /usr/local/lib/app-ha-proxmox/lib/cluster_registry.py reconcile --live
```

Establish that every resource has one unique name/VMID/IP/MAC/domain/purpose,
that every IP is its name's slot, that the policy's `vm_slots` match
`cluster.conf`, that owner/HA/replication matches live placement, and that
cleanup does not remain pending without an understood reason.
`scripts/user_callable/hosts/sync_cluster_network.sh --dry-run` reports any
host whose networking settings differ from the configuration.

### Cluster, QDevice, and data plane

```bash
ssh root@mox1 pvecm status
ssh root@mox1 pvesh get /nodes --output-format json
ssh root@mox1 cat /etc/pve/datacenter.cfg
```

Check:

1. membership matches the registry's member slots and all are online;
2. every node reports quorate;
3. odd membership has no QDevice and exact node votes;
4. even membership has one alive/voting QDevice and `N+1` expected/total
   votes;
5. link 0 is private and preferred, link 1 is Tailscale fallback;
6. migration and replication are secure on `10.213.0.240/28`;
7. all production replication jobs have recent successful syncs and exact
   placement-minus-owner targets.

### Gateway, egress, and tunnel

On every mox:

```bash
ip -4 route show default
ip -4 address show dev vmbr-private
ip -4 address show dev bmacgw0
ip rule show
ip -4 route show table 200
systemctl is-active app-ha-guest-network.service app-ha-egress-health.service \
  app-ha-cloudflared.service app-ha-transfer-counter.timer app-ha-tunnel-sync.timer
/usr/local/lib/app-ha-proxmox/lib/guest_network.sh verify
python3 -I /usr/local/lib/app-ha-proxmox/lib/egress_health.py status
python3 -I /usr/local/lib/app-ha-proxmox/lib/transfer_counter.py status
/usr/local/lib/app-ha-proxmox/lib/tunnel_sync.sh --check
journalctl -u app-ha-cloudflared.service --since -15min | grep 'Registered tunnel connection'
nft list table bridge app_ha_guest_gateway
nft list table ip app_ha_guest_egress
nft list table inet app_ha_host_guard
```

Prove:

- every host holds `10.213.0.10/32` and `GUEST_GATEWAY_MAC` on `bmacgw0`,
  `bmacgw0br` is a port of `vmbr-private`, and neither the address nor the
  MAC is seen on the VLAN from another host or from a guest on another host;
- `ip rule iif bmacgw0` selects table 200, whose default route is the host's
  own public gateway unless egress is deliberately falling back;
- every host retains its public default route;
- guest sources in the VM slot range are forwarded and masqueraded through
  the host they run on;
- egress health reports its own connection in use, enough active probe
  targets, and no unexpected retired ones;
- the transfer count is plausible and below any configured cap;
- every replica is active, its recent connections are `protocol=http2` and
  never `quic`, and its installed config generation matches the registry;
- no public port is DNATed or forwarded to a guest, and public
  22/3128/8006 is blocked;
- no address, nftables rule, route, or tunnel config references
  `10.213.0.3`.

From each mox, reach every active guest's slot on TCP 443. From outside
Tailscale, test that each FQDN reaches the right guest, that an unknown name
under a domain gets 404, and that public management ports are closed.

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
`luks,initramfs,nofail,keyscript=decrypt_keyctl`, both mirror-0 ESPs, no swap,
healthy pool, and one non-empty off-host header backup per member.

### Guest and application

For each production resource:

- QEMU identity, strict affinity, HA state, owner, tunnel input, and replica
  set match registry;
- private slot IP/MAC persist across planned migration;
- static `.10` gateway and DNS work, and outbound traffic leaves through the
  public IP of the host the VM runs on;
- root SSH is key-only through ProxyJump;
- QGA and optional production-startup completion are healthy;
- Tailscale is absent;
- the application-aware HTTPS health endpoint answers through Cloudflare
  for the primary and every alias FQDN.

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

- taking each host's public interface down from its iDRAC console: ingress
  continues through the other replicas and that host's guests' egress falls
  back through another host and returns afterwards; with three hosts, two
  down at once;
- stopping one replica, and a live migration's effect on egress;
- unknown-FQDN 404 and tunnel config drift caught by the sync timer;
- for a web app with an API, high-rate `/api/` calls never challenged with
  Super Bot Fight Mode and Under Attack mode on;
- a probe target blocked by a temporary nftables rule and retired after 5
  strikes;
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

Record measured replica failover, egress fallback and return, quorum, HA, VM
boot, application, replication RPO, and Layer-2 convergence times. Ping
alone is insufficient. The networking drills are the manual test plan in
[Testing](BMAC_SIMPLIFIED_NETWORKING.md#testing), run on the real FiberState
cluster.

## Tests

Run shell syntax checks from the repository root:

```bash
bash -n \
  scripts/user_callable/hosts/add_proxmox_host.sh \
  scripts/host_runtime/app-ha-guest-role-hook.sh \
  scripts/user_callable/guests/prod/add_prod_vm.sh \
  scripts/user_callable/guests/staging/add_staging_vm.sh \
  scripts/user_callable/guests/staging/remove_staging_vm.sh \
  scripts/utilities/patch_staging_clone.sh \
  scripts/lib/config.sh \
  scripts/lib/guest_network_settings.sh \
  scripts/host_runtime/process_deferred_cleanup.sh \
  scripts/host_runtime/guest_network.sh \
  scripts/host_runtime/tunnel_sync.sh \
  scripts/host_runtime/cloudflared_setup.sh \
  scripts/user_callable/hosts/sync_cluster_network.sh \
  scripts/user_callable/hosts/update_cluster_runtime.sh
```

Run the complete unit suite, every tracked `test_*.py`, in parallel:

```bash
dev/run_tests.py
```

`dev/run_tests.py` runs each test in its own `python3` process, one worker
per CPU by default, and prints the tracebacks and output of any failures. On
a 32-vCPU workstation the suite takes about 12 seconds instead of more than
three minutes serially; its wall time is bounded by the slowest single test.
Pass files, modules, or test ids to run only those, `-k PATTERN` to filter
as `unittest` does, `-j N` to set the worker count, `-f` to stop starting
tests after the first failure, and `-v` to list every test with its duration
and the slowest ten. It uses only the Python standard library.

No test may depend on test order or on running alone: each builds its
fixtures under its own temporary directory, nothing uses `setUpClass` or
`setUpModule`, and no test writes to the checkout or to a fixed path. A fake
command that stands in for one fed by a pipe must read its stdin, as the real
command would; under `set -o pipefail`, a fake that exits without reading can
make the writer die of `SIGPIPE` when the machine is busy, which surfaces as
an intermittent failure.

Plain `python3 -m unittest FILE...` still works for any subset and runs it
serially in one process.

On a Mac, `dev/run-tests-in-vm.sh` runs `bash -n` on every tracked script and
then `dev/run_tests.py` inside a Debian 13 Lima VM; see
[`dev/README.md`](../dev/README.md).

Coverage by test helper:

- `scripts/tests/test_add_proxmox_host.py`: QDevice vote/parity parsing, current ISO
  resume, changed boot ID, destructive confirmations, serial storage
  selection, strict SSH, the guest-network and tunnel steps following the
  runtime install, the tunnel secret never appearing in a command line,
  final verification requiring HTTP/2 tunnel connections, and host safety
  invariants.
- `scripts/tests/test_app_ha_guest_role_hook.py`: exact role identity, no destructive
  authority without registry/HA, staging disk/volume/GUID guards, production
  reservations, stopping every staging guest before queuing any destruction,
  the real cleanup worker deferring destruction until post-start, and remote
  deferral.
- `scripts/tests/test_add_prod_vm.py`: autoinstall network/root/QGA/SSH,
  startup retry semantics, ISO hash/release/boot metadata/atomicity, live
  nodes with slot gaps, QDevice gate, resume sentinels, exact VM/Secure Boot contract,
  argv-safe SSH, non-mutating dry-run, and absence of destructive rollback.
- `scripts/tests/test_extend_prod_vm_disk.py`: per-host 10% pool reserve with
  smallest-headroom selection, sparse guest-free and overhead math, reserved
  refreservation ratio, whole-MiB rounding, unit parsing and limits, guest
  root-disk validation, and mutation ordering.
- `scripts/tests/test_staging_vm.py`: argv-safe node execution, identity and
  sanitizer rewrites, symlink escape rejection, mapping/unmount safety,
  stopped-VM disk-reference rejection, snapshot/refcount ordering, rollback,
  and non-mutating dry-run with no secret export.
- `scripts/tests/test_shared_libs.py`: layered config provenance/modes/redaction,
  deterministic allocation, uniqueness, leases/phases, state transitions,
  snapshot dependencies, deferred cleanup, pmxcfs-compatible atomic writes
  and stale-lock handling, fixed-slot allocation and `update-slots`,
  history/cleanup bounds, and safe live reconciliation.
- `scripts/tests/test_process_deferred_cleanup.py`: exact VM/volume/snapshot
  cleanup, offline-source deferral, lifecycle-lock scope, GUID protection,
  first-host no-op, restart finalization, and syntax.
- `scripts/tests/test_guest_network.py`: the gateway address and MAC never
  crossing the private-VLAN port, masquerade of every slot and of handed-over
  traffic, refusal of invalid settings, the shared settings renderer, and
  the sync operation's help.
- `scripts/tests/test_egress_health.py`: the probe rotation, only an
  all-failing pass counting as down, retirement and its minimum, a dead
  connection never retiring targets, fallback order, falling back after
  three failed checks and returning after five good ones, replacing a
  failing fallback, and staying put when no host works.
- `scripts/tests/test_app_ha_monitor.py`: the monitor agent against fixtures
  captured on a live cluster: corosync links, quorum with and without a
  QDevice and with its vote lost, qnetd, cloudflared readiness, Postfix
  queue, ZFS health and capacity hysteresis, NVMe and ATA SMART (warning
  bits, spare, wear, counter baselines, deduplication by serial, read errors),
  Tailscale reachability, ingress probes and challenges, guest services,
  filesystems, and certificates; debounce, escalation, reminders with an
  injected clock, the fail/clear/heartbeat sequence against a local HTTP
  server, retries, queued items and `pending_clear` across restarts, rate
  limits, body sanitizing and truncation, corrupt state, the staging guard,
  and no ping URL in output.
- `scripts/tests/test_monitoring_workstation.py`: `healthchecks.py` against a
  fake Management API (create, idempotent resync, update, obsolete checks,
  budget refusal, dry run, wait-ping, no key or UUID in output), config
  rendering, `monitor_node_install.sh` under a temporary root (install,
  config-only update, rollback, never overwriting `guest.json`), and
  `monitoring_sync` end to end (happy path, dry run, unreachable node before
  any change, phase rollback, removed node's check, missing first ping,
  membership warnings, disabled monitoring making no calls).
- `scripts/tests/test_registry_monitoring.py`: the registry's `monitoring`
  field, `update --monitoring-json` and `--clear-monitoring`, and
  `monitoring-probes`.
- `scripts/tests/test_transfer_counter.py`: combined inbound and outbound
  counting, reset day above 28, exactly one reset per period, and reboot or
  counter-reset handling, with a controlled clock and readings.
- `scripts/tests/test_tunnel_config.py`: the slot formula, every FQDN of a
  production guest and its staging slots, one guest per FQDN, the HTTP/2
  pin and final 404, deterministic generations, the credentials file format
  and mode, and refusal of invalid input.
- `scripts/tests/test_host_storage.py`: `zpool status` topology, resilver nesting and
  auxiliary sections, removal progress, LUKS and by-id member resolution to
  disk serials, boot/ESP vdev detection, disks outside `rpool`, registered
  ESPs outside `rpool`, all-disk use classification, safe physical-removal
  classification, zvol allocation and snapshots, and render output.
- `scripts/tests/test_rpool_mirror.py`: the shared mirror tool against fake disk, LUKS,
  zpool, and initramfs commands: disk safety checks, the passphrase proven
  before any disk is touched, reuse or refusal of existing LUKS, the host's
  crypttab form, no `zpool add` unless the initramfs unlocks the new disks,
  ashift uniformity, unencrypted adds, LUKS retirement, setup's in-place boot
  member conversion, and member replacement: the survivor's partition table
  copied (never written), a grub or uefi ESP matching the survivor's, no
  `zpool replace` until that ESP is in sync, closing a pulled disk's leftover
  mapping, refusing a failed disk that is still installed, and `zpool attach`
  for a mirror detached to one disk.
- `scripts/tests/test_storage_state.py`: locked host-side trim, scrub, vdev-removal,
  mirror-addition, and member-replacement resume records.
- `scripts/tests/test_disk_workflows.py`: workflows 7-10 end to end against a fake SSH
  host and guests: console hand-off, resume, capacity checks, the five-mirror
  limit, related-staging refusal, trim and scrub reuse within 24 hours,
  forced replication, minimum-free-space rules, removal records, retirement,
  and boot, extra, and unencrypted member replacement.
- `scripts/tests/test_show_proxmox_host_state.py`: the host report against a
  fake SSH that rejects any non-read-only command, live disk inventory,
  attention exit status, and usage errors.
- `scripts/tests/test_show_cluster_health.py`: the cluster health check with
  its real host collector run against fake `pvecm`, `corosync-cfgtool`,
  `zpool`, `zfs`, `lsblk`, `smartctl`, and `systemctl`: a healthy cluster, a
  faulted member and failed drive named by serial, low and unreadable rpool
  free space with the 10% boundary, a single down corosync link, a
  down host reported once, inspection through a member when the workstation
  cannot reach a host, QDevice failures, read-only commands, and usage
  errors.

These tests mock destructive Proxmox, ZFS, iDRAC, QDevice, and network
behavior. They do not replace real media, 10-Gbps Layer-2, LUKS boot,
Corosync/quorum, migration, replication, HA, Cloudflare, firewall, application
health, sanitizer, or full-power-loss drills. The tunnel, shared gateway, and
egress fallback are tested by hand on the real FiberState cluster, following
[Testing](BMAC_SIMPLIFIED_NETWORKING.md#testing).

## Deliberate boundaries and future scripts

Only the workflows named above exist as supported operator entry points.
The following potential future commands/functions are explicitly out of scope
and must not be inferred from registry primitives or old design prose:

- **Automatic rebalancing.** Host removal and production
  placement/owner changes are operator-driven scripts. Nothing moves
  production automatically to balance capacity, and a forced removal never re-adds
  placement hosts on its own.
- **Cluster status report.** A future read-only `GetClusterStatus` workflow
  will summarize hosts, tags, quorum/QDevice, egress modes, HA guests,
  staging guests, IPs, placement, replication health, and pending cleanup.
- **Cloudflare automation.** BMAC holds no Cloudflare API token. No script
  creates the tunnel, zones, DNS or wildcard records, certificates, bot
  protection settings, or WAF rules; the scripts print the steps.
- **Even spreading of ingress.** Cloudflare picks the nearest connected
  replica; no script spreads requests or transfer evenly between hosts.
- **Generic application deployment.** Production creation installs Ubuntu,
  not an application. Real applications use their own deployment tooling.
  The sample `scripts/user_callable/guests/prod/deploy_hello_app_to_prod.sh` only deploys an example todo
  app for platform smoke-testing.
- **Backups and point-in-time recovery.** ZFS replication and staging
  snapshots are not backup jobs. No script configures off-cluster backups,
  PostgreSQL PITR, restore testing, or retention.
- **Monitoring and alerting.** No script deploys metrics/log aggregation or
  alerts for quorum, QDevice, egress fallback, transfer caps, tunnel
  replicas, Cloudflare, application health, replication lag, pool capacity,
  or pending cleanup.
- **Complete network isolation.** The code does not create a staging VLAN,
  routed guest subnets, default-deny per-guest firewall rules, or restricted
  guest egress.
- **Ceph or another storage plane.** Current production is local ZFS plus
  replication. Ceph, shared SAN, and cross-cluster recovery require a separate
  network, capacity, quorum, and failure design.
- **Active/active production or database HA.** There is one production VM per
  resource and one active owner.
- **Custom failover controllers.** Cloudflare, each host's egress health
  service, and Proxmox HA keep separate authority. No script should start
  VMs from HTTP health or make QDevice or the tunnel a placement controller.
- **Guest Tailscale or `.3` forwarding.** Neither is a planned extension of
  this architecture. Guest access remains ProxyJump; egress remains `.10`.
- **More than ten hosts, ten production VMs, or nine staging slots per
  production VM.** `mox1..mox10`, the fallback route tables, and the slot
  formula are hard policy limits.

Any future implementation must add a reviewed workflow, failure model,
permissions/locking behavior, tests, and updates to this README before it is
treated as operational.

## Documentation authority and legacy-document removal

Authority order is:

1. current executable scripts and checked-in `config/cluster.conf`/`moxN.conf`;
2. this comprehensive top-level overview, together with
   [`BMAC_SIMPLIFIED_NETWORKING.md`](BMAC_SIMPLIFIED_NETWORKING.md) for the
   networking design (the two must agree);
3. scoped detail in [`scripts/user_callable/hosts/README.md`](../scripts/user_callable/hosts/README.md),
   [`scripts/user_callable/guests/prod/README.md`](../scripts/user_callable/guests/prod/README.md),
   [`scripts/user_callable/guests/staging/README.md`](../scripts/user_callable/guests/staging/README.md), and
   [`scripts/lib/README.md`](../scripts/lib/README.md);

Still-valid explanatory material consolidated from the removed legacy
documents includes:

- the central separation among Cloudflare replica selection, tunnel
  hostname-to-slot routing, Layer-2 MAC location, Proxmox HA placement,
  QDevice quorum, ZFS replication, and Tailscale management;
- the critical shared-Layer-2 assumption and need to test MAC/ARP relearning
  after VM movement;
- complete normal and failure traffic paths and the distinction between
  ingress and guest egress;
- application-aware multi-layer health checks and the warning that ping or a
  static proxy response is not an HA proof;
- independent Cloudflare and Proxmox failover timing and the expected
  temporary backend-unavailable interval;
- forwarded-header source-address implications;
- wildcard staging DNS versus deeper wildcard certificate coverage;
- least-privilege exposure, staging isolation, observability, alerting, and
  full-host/reverse-failover drill guidance;
- QDevice's deliberately narrow non-application role.

The former traffic-map and proxy-HA design documents have been removed.
Their Git history remains available, but no current code or operator
procedure depends on them.
