<!--
Copyright (c) 2026 BEENTHERE VENTURES, INC.
SPDX-License-Identifier: GPL-3.0-only
-->

# BMAC v2: simplified internet ingress and egress

**Status:** implemented; not yet deployed to or tested on a real cluster.
**Scope:** BMAC v2 only (everything outside `v3/`). There is no deployed
cluster yet, so nothing here needs to stay compatible with the current
HAProxy/VRRP design or migrate from it.

This design replaces:

- the per-Proxmox-host HAProxy LXCs and their route-synchronization
  machinery (ingress), with **one Cloudflare Tunnel run on every Proxmox
  host**;
- Keepalived/VRRP and the floating guest-egress address (egress), with **a
  shared gateway that every Proxmox host provides to its own VMs**, plus a
  fallback through any other Proxmox host's internet connection;
- dynamically allocated staging addresses and routes, with **fixed address
  slots that are set up in advance**.

## Contents

- [Terms](#terms)
- [Why](#why)
- [Goals](#goals)
- [Address plan](#address-plan)
- [Egress: the shared gateway](#egress-the-shared-gateway)
- [Egress fallback](#egress-fallback)
- [Transfer counters](#transfer-counters)
- [Ingress: one Cloudflare Tunnel on every Proxmox host](#ingress-one-cloudflare-tunnel-on-every-proxmox-host)
- [FiberState UDP limit: the tunnel runs over TCP](#fiberstate-udp-limit-the-tunnel-runs-over-tcp)
- [API traffic and bot protection](#api-traffic-and-bot-protection)
- [Cloudflare setup](#cloudflare-setup)
- [Several BMAC clusters in one Cloudflare account](#several-bmac-clusters-in-one-cloudflare-account)
- [Staging lifecycle](#staging-lifecycle)
- [Dashboard](#dashboard)
- [Failure behaviour](#failure-behaviour)
- [Where it lives](#where-it-lives)
- [Testing](#testing)

## Terms

| Term | Means | Example |
|---|---|---|
| **Proxmox host** | A physical server in the cluster. Never used for anything else. | `mox1`, `mox2` |
| **VM** | A prod or staging virtual machine running on a Proxmox host. | `prod1`, `stage3prod1` |
| **slot** | A fixed private address reserved for one prod or staging VM, whether or not the VM exists. | prod1's slot `10.213.0.110` |
| **domain** | A registered domain name, managed as one Cloudflare zone. Each prod VM normally has its own domain. | `example.com` |
| **FQDN** | One fully-qualified name inside a domain that a browser requests. | `example.com`, `www.example.com`, `stage3prod1.example.com` |
| **tunnel** | One Cloudflare Tunnel identity (ID and credentials), created once in Cloudflare. | `bmac-MyAppCloud` |
| **replica** | One running `cloudflared` process connected to the tunnel. | the `cloudflared` service on mox1 |

This document never uses the word "host" on its own.

## Why

Staging VMs are a routine part of the CI/CD workflow. Each change is tested
in a staging VM cloned from prod's current state, and is pushed to prod only
if those tests pass. Prod VMs are created rarely; staging VMs are created and
removed constantly.

Before this redesign, every staging create and remove changed ingress
state and waited for it to converge on every Proxmox host:

- creation enabled the VM's route and ran the HAProxy route
  synchronization (`enable_route_and_sync` in
  `scripts/user_callable/guests/staging/add_staging_vm.sh`);
- removal disabled the route and queued `remove-route` cleanup, which ran
  the same synchronization
  (`scripts/user_callable/guests/staging/remove_staging_vm.sh`,
  `scripts/host_runtime/process_deferred_cleanup.sh`).

That synchronization rendered, validated and reloaded HAProxy on every
Proxmox host under a cluster-wide lock. It often added **several minutes** to each
staging create or remove. Removing that delay is the main goal of this
redesign. Dropping HAProxy and Keepalived/VRRP is how we get there, and it
also removes a large amount of code and runtime state.

## Goals

1. **Creating or removing a staging VM changes no network state at all.** No
   tunnel config change, no DNS or Cloudflare change, no nftables change, and
   no step that waits on other Proxmox hosts.
2. **Set and forget.** Every slot's address, FQDNs, tunnel entry and egress
   rule exist in advance. Network configuration changes only when a prod VM
   is created or removed, or when an operator deliberately changes
   cluster.conf's slot settings.
3. **No single Proxmox host's internet connection is a single point of
   failure**, for ingress or egress, including a Proxmox host that loses its
   connection by reaching its provider's transfer cap.
4. **API calls are never answered with a Cloudflare challenge**, while web
   pages keep Cloudflare's bot protection.
5. **Low cost.** Everything is free except Cloudflare Pro on the domains of
   web apps that have an API (see
   [API traffic and bot protection](#api-traffic-and-bot-protection)).

## Address plan

All VM addresses come from fixed slots on the existing private VLAN, derived
from cluster.conf.

| Slot | Address | Example (`VM_SLOT_START=10.213.0.110`) |
|---|---|---|
| prodN | `VM_SLOT_START + 10×(N−1)` | prod1 = `.110`, prod2 = `.120`, prod10 = `.200` |
| stageKprodN | prodN's address `+ K` | stage1prod1 = `.111` … stage9prod1 = `.119` |

The last digit of the address is the staging number (0 for the prod VM), and
the tens digit identifies the prod VM.

The full example layout for `10.213.0.0/24`:

```text
10.213.0.1          data-centre VLAN gateway (reserved, unused)
10.213.0.10         shared guest gateway, provided by every Proxmox host
10.213.0.11-.20     mox1-mox10
10.213.0.110-.209   VM slots: prod1-prod10, each followed by its 9 staging slots
10.213.0.240/28     migration/replication network (.241-.250 = mox1-mox10)
```

### cluster.conf changes

| Key | Change |
|---|---|
| `MAX_PROD_VMS` | **New.** Number of prod slots; 1–10, validated. |
| `MAX_STAGING_VMS_PER_PROD` | **New.** Staging slots per prod VM; 1–9, validated. |
| `VM_SLOT_START` | **New.** prod1's address. Its last octet must end in 0, and every slot (`VM_SLOT_START` + `10×MAX_PROD_VMS − 1`) must fall inside `PRIVATE_SUBNET_CIDR` without overlapping any other range or address. |
| `GUEST_GATEWAY_IP` | **Replaces `GUEST_EGRESS_VIP`.** A plain address (e.g. `10.213.0.10`), no longer floating. |
| `GUEST_GATEWAY_MAC` | **New.** The shared gateway's MAC address, identical on every Proxmox host. Must be a locally administered unicast MAC (e.g. `02:00:0a:d5:00:0a`). |
| `CLOUDFLARE_ACCOUNT_ID` | **New.** The Cloudflare account that owns the tunnel (`AccountTag` in the tunnel's credentials file). |
| `CLOUDFLARE_TUNNEL_ID` | **New.** The tunnel's ID (`TunnelID` in the credentials file). |
| `PRODUCTION_IP_START/END`, `STAGING_IP_START/END` | **Removed** (replaced by the slot keys). |
| `HAPROXY_IP_START/END`, `HAPROXY_LXC_*` | **Removed.** |

`secrets.env` gains **`CLOUDFLARE_TUNNEL_SECRET`** (`TunnelSecret` in the
credentials file). Host setup combines the three values to write the
tunnel's credentials file, readable only by root and `cloudflared`, on every
Proxmox host. The original credentials file never needs to be in the
checkout; `.gitignore` excludes `config/cloudflare_tunnel_credentials.json`
and `cert.pem` in case either is copied there.

### moxN.conf changes

- **Removed:** `HAPROXY_LXC_*` and `VRRP_PRIORITY`.
- **New, optional:** `PROXMOX_HOST_TRANSFER_MONTHLY_LIMIT_GIB` and
  `PROXMOX_HOST_TRANSFER_MONTHLY_RESET_DAY` (see
  [Transfer counters](#transfer-counters)).
- **Unchanged:** `MAX_STAGING_VM_COUNT_ON_THIS_HOST` stays, is still enforced
  at staging creation (it reflects each Proxmox host's RAM), and must be no
  more than `MAX_STAGING_VMS_PER_PROD`.

## Egress: the shared gateway

Every VM keeps its single NIC on `vmbr-private` and uses `GUEST_GATEWAY_IP`
(`.10`) as its only default gateway, exactly as in the current v2 design.
What changes is who answers for `.10`: **every Proxmox host does, but only
for its own VMs.** Nothing is elected and nothing moves between Proxmox
hosts.

### The gateway interface

Every Proxmox host has a veth pair: `bmacgw0` holds the gateway, and its
peer `bmacgw0br` is a port of `vmbr-private`. `bmacgw0` holds:

- the address `GUEST_GATEWAY_IP` (as a `/32`, so it doesn't add a second
  route for the private subnet), and
- **the same MAC address on every Proxmox host**, `GUEST_GATEWAY_MAC`.

### Keeping it visible only to that Proxmox host's own VMs

nftables bridge-family rules sit on the Proxmox host's physical private-VLAN
port, the bridge port that connects `vmbr-private` to the other Proxmox
hosts:

- **outgoing through that port:** drop ARP requests and replies about
  `GUEST_GATEWAY_IP`, and every frame whose source MAC is
  `GUEST_GATEWAY_MAC`;
- **incoming through that port:** drop ARP requests about `GUEST_GATEWAY_IP`,
  and every frame addressed to `GUEST_GATEWAY_MAC`.

Result:

- a VM's ARP request for `.10` is answered only by the Proxmox host it is
  running on;
- `GUEST_GATEWAY_MAC` never appears on the VLAN, so the identical address and
  MAC on every Proxmox host never conflict;
- the Proxmox hosts' own `.11`–`.20` addresses, VM-to-VM traffic, Corosync,
  migration and replication are unaffected.

### Why the MAC must be the same everywhere

After a live migration, the VM's ARP cache still maps `.10` to
`GUEST_GATEWAY_MAC`. The destination Proxmox host owns that same MAC, so the
VM's outbound traffic continues immediately through the new Proxmox host's
internet connection, without waiting for the ARP entry to expire.

Outbound connections that were open before the migration break, because the
public source address changes. The old VRRP design had the same
behaviour when `.10` moved.

### Outbound NAT

Each Proxmox host masquerades forwarded traffic whose source is any VM slot
out its own public interface. The rule covers every slot from cluster.conf,
so it is installed once at Proxmox host setup and never changes when VMs are
created or removed. External services that allowlist source addresses must
list every Proxmox host's public address, as today.

## Egress fallback

When a Proxmox host's own internet connection is down (including when it has
reached its transfer cap), its VMs' outbound traffic goes out through
another Proxmox host instead. The cluster can have up to 10 Proxmox hosts,
and any of them whose own connection works can serve as the fallback. Each
Proxmox host decides this on its own; there is no shared state and no
election.

### Static part (installed at Proxmox host setup)

- Traffic arriving on the gateway interface is looked up in its own route
  table, `guest-egress` (an `ip rule` on the incoming interface). Normally
  that table's default route is the Proxmox host's own internet gateway.
- One extra route table per other Proxmox host slot (up to 9), each with a
  default route via that Proxmox host's private address. The health service
  uses them to test, and then use, a particular Proxmox host as the
  fallback.
- Every Proxmox host also masquerades VM traffic handed to it by another
  Proxmox host over the VLAN. That traffic arrives on `vmbr-private`, not on
  the gateway interface, so it uses the main route table and always leaves
  through this Proxmox host's own connection. It is never passed on again,
  so loops are impossible. A Proxmox host that is itself falling back
  therefore can't carry another's traffic, and fails the test below, so it
  is never chosen.
- ICMP redirects are disabled on the private VLAN. Otherwise a Proxmox host
  forwarding a VM's packet to another Proxmox host would tell the VM to send
  directly, bypassing `.10`.
- Connection tracking on the forwarding Proxmox host must tolerate seeing
  only the outbound half of each connection, because replies return from the
  other Proxmox host straight to the VM. The forward path for VM traffic must
  not drop packets classified as invalid.

### Probe targets

The health service tests internet connections with **HTTPS requests** (TCP,
never UDP or ICMP, because of FiberState's UDP filter). To avoid sending
steady traffic to any one destination, which could get a Proxmox host's
address blocked, it rotates through a large list of independent targets.

- **A probe succeeds** when the TLS certificate verifies and the target
  returns any HTTP status within 4 seconds. The status code doesn't matter;
  the point is reaching the internet.
- **The list ships with BMAC** and is validated on each Proxmox host at
  setup, and again by the sync operation: every target is tried, and only
  those that succeed are used. Setup warns if fewer than 15 remain.
- **Retirement:** a target that fails on its turn while the targets tried
  just after it succeed gets a strike; a success clears its strikes. After 5
  strikes in a row it is retired: logged, removed from the rotation, and
  shown in the dashboard. It returns only when the sync operation
  revalidates the list.

The initial list (all confirmed reachable on 2026-10-21), from many
different operators:

| Operator | Targets |
|---|---|
| Google | `https://connectivitycheck.gstatic.com/generate_204`, `https://www.google.com/generate_204`, `https://clients3.google.com/generate_204`, `https://dns.google/resolve?name=example.com` |
| Cloudflare | `https://cp.cloudflare.com/generate_204`, `https://1.1.1.1/cdn-cgi/trace`, `https://1.0.0.1/cdn-cgi/trace` |
| Apple | `https://captive.apple.com/hotspot-detect.html`, `https://www.apple.com/library/test/success.html` |
| Mozilla | `https://detectportal.firefox.com/success.txt`, `https://www.mozilla.org/` |
| GNOME / Canonical | `https://nmcheck.gnome.org/check_network_status.txt`, `https://connectivity-check.ubuntu.com/`, `https://archive.ubuntu.com/ubuntu/` |
| DNS providers | `https://dns.quad9.net/`, `https://www.opendns.com/`, `https://dns.adguard-dns.com/` |
| Linux and open source | `https://www.kernel.org/`, `https://deb.debian.org/debian/`, `https://www.debian.org/`, `https://enterprise.proxmox.com/`, `https://www.gnu.org/`, `https://www.python.org/`, `https://pypi.org/`, `https://registry.npmjs.org/`, `https://go.dev/` |
| Others | `https://www.wikipedia.org/`, `https://github.com/`, `https://www.fastly.com/`, `https://www.microsoft.com/`, `https://www.bing.com/`, `https://aws.amazon.com/`, `https://duckduckgo.com/`, `https://www.example.com/`, `https://www.iana.org/`, `https://pkgs.tailscale.com/`, `https://www.digitalocean.com/`, `https://www.hetzner.com/`, `https://www.linode.com/`, `https://www.dropbox.com/` |

That's 39 targets. With one probe every 5 seconds, each target sees about
one request every 3 minutes from each Proxmox host.

### The health service

A small service on each Proxmox host, running these steps:

1. **Every 5 seconds** (never more often), probe the next target in the
   rotation through the Proxmox host's own public interface.
2. **If it fails,** keep advancing through the list, trying the following
   targets (a few at a time, so a full pass takes seconds rather than
   minutes), until one succeeds or every target has failed. Only when
   **every** target has failed is the Proxmox host's own connection
   considered down for that check.
3. **After 3 such checks in a row fail,** choose a fallback: try the other
   Proxmox hosts one at a time, starting with the next slot number after
   this Proxmox host's own and wrapping around (so different Proxmox hosts
   tend to pick different fallbacks), probing through each one's route table
   with the same rotation. Use the first that works by pointing
   `guest-egress`'s default route at it.
4. **While falling back,** keep checking both connections every 5 seconds:
   the fallback (if it fails, choose again as in step 3) and the Proxmox
   host's own connection.
5. **After 5 checks in a row succeed on its own connection,** point
   `guest-egress` back at its own internet gateway.
6. Log every switch and expose the current state (own connection, fallback
   in use, retired targets) for the dashboard.

Estimated size: 250–350 lines plus tests.

## Transfer counters

Some providers cap each server's monthly traffic and cut its internet
connection when the cap is reached. FiberState's cap counts inbound and
outbound traffic together. Every Proxmox host counts the bytes received and
sent on its public interface (both directions, combined), and the dashboard
shows the count and, when a cap is configured, how much of it is used.
Tunnel traffic counts too: a replica's connections to Cloudflare run over
the public interface, so a Proxmox host carrying more ingress uses more of
its cap.

Two optional `moxN.conf` keys:

```text
# Optional monthly inbound+outbound traffic cap (in GiB)
PROXMOX_HOST_TRANSFER_MONTHLY_LIMIT_GIB=301748
# Optional numeric day of the month on which the transfer quota is reset
PROXMOX_HOST_TRANSFER_MONTHLY_RESET_DAY=24
```

- The reset day is 1–28; a larger value is treated as 28 (with a warning at
  setup). It defaults to 1.
- The counter resets **once** on the reset day. The service persists the
  count, the last interface reading, and the date of its last reset in a
  state file on the Proxmox host, so a restart or reboot neither loses the
  count nor resets it a second time. It also handles the interface counter
  restarting at zero after a reboot.
- Like other `moxN.conf` values, these are copied onto the Proxmox host at
  setup. The sync operation re-reads them from `config/moxN.conf`, so a
  changed cap or reset day can be pushed later.
- The counter only reports. It doesn't change routing; when a provider cuts a
  capped Proxmox host's connection, the tunnel and egress fallback handle it
  like any other outage.

## Ingress: one Cloudflare Tunnel on every Proxmox host

### Shape

- **One tunnel for the whole cluster**, created once.
- **Every Proxmox host runs a `cloudflared` replica of that tunnel** as a
  sandboxed systemd service. Each replica keeps several outbound connections
  open to Cloudflare. No Proxmox host listens on a public port for web
  traffic. Cloudflare allows up to 25 replicas per tunnel, well above the
  10-Proxmox-host maximum.
- **Every replica has the same config file**, generated by BMAC, listing
  every FQDN and the slot address it goes to.

### How a request reaches a VM

1. A browser requests `https://stage3prod1.example.com`. DNS for that FQDN
   points at the tunnel, so the request goes to Cloudflare.
2. Cloudflare picks one connected replica and sends the request down that
   replica's open connection. In this example, the replica on mox2.
3. The replica looks up the FQDN in its config file:
   `stage3prod1.example.com → https://10.213.0.113:443`.
4. It opens an HTTPS connection over the private VLAN to `10.213.0.113`,
   which may be a VM on mox2 or on any other Proxmox host.
5. The VM replies to mox2's private address over the VLAN. The replica sends
   the response back to Cloudflare, which returns it to the browser.

The VM's client is the replica's private address, so the return path never
involves NAT, conntrack on another Proxmox host, or the VM's default gateway.
The VM still terminates TLS on port 443 with its own certificate; the replica
verifies it using `originServerName`.

### Failover

When a Proxmox host loses its internet connection, its replica's connections
to Cloudflare drop, and Cloudflare sends every request to the remaining
replicas within seconds. Those replicas reach VMs on the disconnected Proxmox
host over the VLAN, so its VMs stay reachable. Cloudflare Load Balancing is
not needed.

It does not matter which Proxmox host carries a VM's traffic. Cloudflare
sends each request to the nearest connected replica and offers no way to
choose or prefer one.

**Caveat, transfer caps:** because replicas are not load-balanced, one
Proxmox host may carry most of the ingress and reach its transfer cap first.
After it drops out, the remaining Proxmox hosts carry all ingress, plus any
VM egress that has failed over to them. The
[transfer counters](#transfer-counters) make this visible. If even
spreading ever becomes necessary, Cloudflare Load Balancing can be added
later.

### Bandwidth

Cloudflare documents no bandwidth or transfer limit for tunnels. Traffic
through a tunnel is ordinary Cloudflare-proxied traffic, so Cloudflare's
general terms apply: on Free, Pro and Business plans, Cloudflare may limit a
domain that uses its CDN to serve video or a disproportionate share of
pictures, audio or other large files, unless those are served through its
paid products (such as Stream, Images or R2). Ordinary web app and API
traffic is unaffected.

### The config file

- Rendered on each Proxmox host from the registry (in pmxcfs) and the
  installed cluster.conf, so every host produces the same file.
- One entry per FQDN of every prod VM: its domain, its aliases, and
  `stage1prodN.<domain>` through `stage<MAX_STAGING_VMS_PER_PROD>prodN.<domain>`,
  each mapped to its slot as `https://<slot address>:443`.
- A final catch-all entry returns 404 for any other FQDN.
- `protocol: http2` (see the next section).
- Changes only when a prod VM is created or removed, or when cluster.conf's
  slot settings change. On a change, BMAC restarts the replicas one Proxmox
  host at a time, so at least one replica stays connected.

While a staging slot has no VM, requests to its FQDN get a Cloudflare error
page because the replica can't reach the slot's address. That is expected.

## FiberState UDP limit: the tunnel runs over TCP

FiberState's DDoS filter drops most inbound UDP above roughly 1,400 packets
per second. Cloudflare Tunnel is not WireGuard, but `cloudflared`'s default
transport is QUIC over **UDP** port 7844, so Cloudflare's replies would
arrive at the Proxmox host as inbound UDP and be throttled. The other
transport is HTTP/2 over **TCP** port 7844.

The default setting, `protocol: auto`, falls back to HTTP/2 only when QUIC
cannot connect at all, not when QUIC connects and then loses packets. So
BMAC's config file **must** set `protocol: http2`.

- All tunnel traffic is then TCP connections started by the Proxmox host. No
  inbound UDP is involved.
- HTTP/2 transport cannot carry UDP through the tunnel. BMAC doesn't need
  that; HTTPS and WebSockets work.
- Proxmox host health checks and the dashboard flag any replica connection
  that isn't HTTP/2.

## API traffic and bot protection

### Which web apps this applies to

Only web apps that have an API need the `/api/` exception and Cloudflare
Pro. For a domain whose web app has no API, use the free plan and turn on
the free **Bot Fight Mode**; nothing in this section is needed.

### Requirements (web apps with an API)

- API calls are never answered with a Cloudflare challenge page. The
  application's client JavaScript can't handle one.
- The application implements its own API protection. Cloudflare's paid API
  Shield is not used.
- Web pages (everything that isn't API) get Cloudflare's bot protection.

### API traffic is identified by path only

On every prod and staging VM, all API requests use paths starting with
`/api/` (for example `example.com/api/v1/user/get/{id}`). API traffic never
uses a separate `api.` FQDN. This keeps one rule per domain sufficient and
avoids two-level staging FQDNs such as `api.stage1prod1.example.com`, which
Cloudflare's free certificate doesn't cover.

### API traffic still passes through Cloudflare

An FQDN routed through a tunnel must be proxied by Cloudflare; it can't be
DNS-only. So API requests do pass through Cloudflare's edge. That is
harmless in itself: Cloudflare only challenges a request when one of its
security features decides to. The design makes sure none of them act on
`/api/`.

### Why Super Bot Fight Mode (Cloudflare Pro) is necessary

Cloudflare's free bot protection, **Bot Fight Mode, cannot guarantee that
`/api/` is left alone:**

- It applies to the whole domain, and Cloudflare documents that it **cannot
  be skipped or bypassed** by WAF custom rules or Page Rules: Skip, Bypass
  and Allow actions have no effect on it.
- Configuration Rules have no Bot Fight Mode setting either.
- Cloudflare's own documentation warns that it "may challenge API or mobile
  app traffic".

So with Bot Fight Mode on, an `/api/` exception rule would exist but would
not stop Bot Fight Mode from challenging API calls. The only free way to
guarantee unchallenged API calls would be to turn bot protection off
entirely.

**Super Bot Fight Mode**, included in Cloudflare Pro (about $20–25 per month
per domain), runs on Cloudflare's rules engine, and **can be skipped** by a
WAF custom rule. That is what makes the `/api/` exception reliable while web
pages keep bot protection. Therefore every domain whose web app has an API:

- subscribes to **Cloudflare Pro**;
- turns **Super Bot Fight Mode on**;
- leaves the free **Bot Fight Mode off** (it can't be skipped, so it must not
  be on).

Everything else in this design (the tunnel, DNS, certificates) is free.

### The `/api/` skip rule

One WAF custom rule per domain (Pro allows 20; this uses 1). Every staging
FQDN is inside its prod VM's domain, so this one rule covers the prod VM and
all of its staging VMs. A single rule covering every domain at once would
need an account-level ruleset, which is Enterprise-only.

- **Expression:** `starts_with(http.request.uri.path, "/api/")`
- **Action:** Skip, with all of these selected:
  - all remaining custom rules;
  - all rate-limiting rules;
  - all managed rules;
  - **all Super Bot Fight Mode rules;**
  - the products Browser Integrity Check, Security Level (which now only
    controls Under Attack mode), User Agent Blocking, Zone Lockdown and
    Hotlink Protection.
- **Order:** first in the domain's custom rules, so any later rule with a
  challenge action never sees API requests.

| What could challenge an API call | How it's handled |
|---|---|
| Bot Fight Mode (free) | Kept off; it can't be skipped. |
| Super Bot Fight Mode (Pro) | Skipped by the `/api/` rule. |
| Under Attack mode | Skipped by the `/api/` rule. |
| Browser Integrity Check (on by default) | Skipped by the `/api/` rule. |
| Managed rules, rate limiting, User Agent Blocking, Zone Lockdown, Hotlink Protection | Skipped by the `/api/` rule. |
| The domain's other custom rules | Skipped (the `/api/` rule is first). |
| Cloudflare's always-on DDoS protection | Can't be turned off. It may act only during a detected attack. Accepted risk. |

### Other Cloudflare limits that affect APIs

- Request bodies up to 100 MB on Free and Pro.
- Cloudflare waits at most 100 seconds for the VM's response, so
  long-polling or slow API calls must finish sooner.
- By default Cloudflare caches only static file types, so API responses
  aren't cached.

## Cloudflare setup

These steps replace README.md's current Cloudflare instructions (load
balancer, origin pools and monitors are no longer needed). BMAC holds no
Cloudflare API token; the steps are manual and short, and BMAC's dashboard
shows exactly what each domain needs.

### Once per cluster: create the tunnel

The tunnel must be created with the `cloudflared` command-line tool, not in
the Cloudflare dashboard. A tunnel created in the dashboard is *remotely
managed*: its FQDN-to-address map lives in Cloudflare and replicas ignore a
local config file, so BMAC couldn't own the map. A tunnel created with the
CLI is *locally managed*, which is what BMAC needs.

On the admin workstation:

1. Install `cloudflared` (Cloudflare's apt repository on Linux, or
   `brew install cloudflared` on macOS).
2. Run `cloudflared tunnel login`. It opens a browser; sign in to Cloudflare.
   The page asks you to pick **one** zone and says the tunnel will be added to
   it. That choice only scopes this certificate for `cloudflared tunnel route
   dns` (creating DNS records from the CLI), which BMAC doesn't use. The
   tunnel itself belongs to the account, and serves any domain in the same
   account whose DNS points at it. So any domain will do.

   This writes `~/.cloudflared/cert.pem`, a certificate used only to create
   and delete tunnels (and to create DNS records in the chosen zone). BMAC
   never uses it, and it is not needed to run a tunnel: keep it safe on the
   workstation for creating future tunnels, or delete it after step 3. Never
   copy it into the checkout or onto a Proxmox host.
3. Run `cloudflared tunnel create bmac-<cluster name>`. Name it after the
   cluster rather than a domain, since it serves every domain whose prod VM
   runs on that cluster. It prints the tunnel
   ID and writes the credentials file `~/.cloudflared/<tunnel ID>.json`,
   which contains three values: `AccountTag`, `TunnelID` and
   `TunnelSecret`.
4. Copy them into BMAC's configuration:
   - `CLOUDFLARE_ACCOUNT_ID` = `AccountTag`, and `CLOUDFLARE_TUNNEL_ID` =
     `TunnelID`, in `config/cluster.conf`;
   - `CLOUDFLARE_TUNNEL_SECRET` = `TunnelSecret`, in `config/secrets.env`.

   Keep the credentials file somewhere safe outside the checkout (it can
   always be rebuilt from these three values).

Proxmox host setup then installs `cloudflared` from Cloudflare's apt
repository, writes the credentials file and the generated config, and starts
the replica on every Proxmox host.

**Seeing it in the Cloudflare dashboard:** open the Zero Trust dashboard
(`one.dash.cloudflare.com`) and go to Networks → Tunnels. The tunnel is
listed by name with its status (healthy once a replica is connected),
marked as locally managed. Opening it shows each connected replica (one per
Proxmox host) with its origin address and `cloudflared` version. The
dashboard never shows the tunnel secret, and the FQDN-to-address map, being
local, is shown in BMAC's dashboard rather than Cloudflare's.

### Once per prod VM's domain

Done when the prod VM is created; about 5 minutes.

1. The domain is a Cloudflare zone (its nameservers point at Cloudflare) in
   the **same Cloudflare account** as the tunnel. A tunnel only serves DNS
   records in its own account.
2. Add two **proxied** DNS records, both `CNAME → <tunnel ID>.cfargotunnel.com`:
   - `@`, the domain itself (`example.com`);
   - `*`, every FQDN one level below it (`www.example.com`,
     `stage1prod1.example.com`, and so on).
   Cloudflare's free certificate covers the domain and one level below it,
   which includes every staging FQDN.
3. Turn on **Always Use HTTPS**, so no Proxmox host ever needs to accept
   plain HTTP.
4. Bot protection:
   - **web app without an API:** stay on the free plan and turn on **Bot
     Fight Mode**;
   - **web app with an API:** subscribe the domain to **Cloudflare Pro**,
     leave **Bot Fight Mode off**, turn **Super Bot Fight Mode on**, and add
     the [`/api/` skip rule](#the-api-skip-rule) as the first custom rule.
5. For an alias domain in a different zone, repeat these steps in that zone.

## Several BMAC clusters in one Cloudflare account

Each BMAC cluster has its own tunnel, and one Cloudflare account can hold
many: Cloudflare allows 1,000 tunnels per account, and tunnels cost nothing
on any plan.

- Create one tunnel per cluster (`bmac-<cluster name>`), each from its own
  `cloudflared tunnel create`. The same `cert.pem` can create all of them.
- Each cluster's `cluster.conf` and `secrets.env` hold its own tunnel's
  values. `CLOUDFLARE_ACCOUNT_ID` is the same for all of them.
- A domain belongs to the cluster whose prod VM serves it: its two DNS
  records point at that cluster's tunnel ID. Domains in the same account can
  point at different clusters' tunnels.
- Clusters never share a tunnel. A replica of one cluster's tunnel can only
  reach its own cluster's private VLAN, so sharing would send some requests
  to a cluster that can't serve them.

## Staging lifecycle

**Create:** pick the lowest free staging slot for the source prod VM, clone,
sanitize, and start the VM at that slot's fixed address. The FQDN
`stageKprodN.<domain>` already reaches the slot, so it works as soon as the
VM's web server is up. There is no route to enable and nothing to
synchronize.

**Remove:** stop and destroy the VM. The slot becomes free. There is no route
to disable and nothing to synchronize.

**Placement:** unchanged. `MAX_STAGING_VM_COUNT_ON_THIS_HOST` still limits
how many staging VMs a Proxmox host runs, and the production-start eviction
in the guest lifecycle hook still frees capacity for prod.

Removed with this: staging FQDN overrides (the `--domain` option), the
registry's `routes_enabled` field, and all `remove-route` deferred-cleanup
actions.

## Dashboard

- **List VM slots** (Production page tile, workflow `list_slots`): every
  production and staging slot with its address, the registered guest using
  it (if any), the FQDNs the tunnel routes to it, and the proxied CNAME
  records each production domain needs.
- **Sync cluster with cluster.conf** (Hosts page tile, workflow
  `sync_cluster_network`): its dry run, the default, lists what differs on
  each Proxmox host (installed `cluster.conf`, guest network settings, tunnel
  config) and whether the registry's slot settings differ from cluster.conf.
  Without the dry run it brings everything in line: the registry's slots
  (refused if a registered VM would move), the installed cluster.conf, each
  host's gateway, egress rules, transfer cap and probe targets, the tunnel
  credentials, and every tunnel config, restarting replicas one host at a
  time. Used rarely.
- **Inspect a host** (`show_proxmox_host_state`) and **Full cluster state**
  (`show_cluster_state`): internet connection health and whether egress is
  falling back (and through which Proxmox host), retired probe targets, the
  guest-egress route, transfer this month (inbound plus outbound) against
  the cap, whether the tunnel config matches the registry, and the
  replica's recent connections and protocol. Falling-back egress, a stale
  tunnel config, or a replica not using HTTP/2 is an attention item.

## Failure behaviour

| Event | Ingress | Egress |
|---|---|---|
| A Proxmox host loses its internet connection (or reaches its cap) | Its replica disconnects; other replicas carry its VMs' traffic over the VLAN. | Its VMs fall back through another Proxmox host after 3 failed checks (about 15 seconds or more); open outbound connections break. |
| A Proxmox host fails completely | Its replica disappears; other replicas carry everything. Proxmox HA restarts prod VMs elsewhere at the same slot address. | Restarted VMs use their new Proxmox host's gateway. |
| A VM is live-migrated | Unaffected; slot address unchanged. | Continues through the new Proxmox host immediately (same gateway MAC); open outbound connections break. |
| Every Proxmox host loses its internet connection | Unavailable. | Unavailable. |
| Cloudflare is unavailable | Unavailable; there is no direct path. | Unaffected. |

## Where it lives

Removed entirely: the HAProxy route renderer and synchronizer
(`haproxy_routes.py`, `sync_haproxy_routes.sh`) and their tests, the HAProxy
LXC, public 80/443 DNAT, ingress timers, Keepalived/VRRP and its readiness
script, the registry's `ingress/` records and commands, `routes_enabled`,
`remove-route` cleanup actions, and the staging FQDN override.

On each Proxmox host, in `/usr/local/lib/app-ha-proxmox/lib/`:

| File | Role |
|---|---|
| `guest_network.sh` | Shared gateway (`bmacgw0` holds `GUEST_GATEWAY_IP`/32 and `GUEST_GATEWAY_MAC`; its veth peer `bmacgw0br` is a port of the private bridge), bridge and egress nftables rules (`/etc/nftables.d/app-ha-guest-network.nft`), sysctls, route tables 200 (`guest-egress`) and 201–210 (one per other Proxmox host, selected by fwmark `0x101`–`0x10a`), Proxmox firewall rules, and the units below. Settings: `/etc/app-ha/guest-network.env`. |
| `egress_health.py`, `egress_probe_targets.txt` | `app-ha-egress-health.service`: the probe rotation and fallback. While falling back it also adds a raw-table `notrack` rule for traffic from the gateway interface. State: `/run/app-ha/egress-health.json`, `/var/lib/app-ha/egress-targets.json`. It also recreates the gateway interface if a bridge reload detached it. |
| `transfer_counter.py` | `app-ha-transfer-counter.timer`: the monthly transfer counter. State: `/var/lib/app-ha/transfer.json`. |
| `tunnel_config.py` | Renders `/etc/cloudflared/config.yml` from the registry's `tunnel-productions` and the slot settings; writes the credentials file. |
| `tunnel_sync.sh` | `--local`, `--all` (one host at a time) and `--check`; `app-ha-tunnel-sync.timer` runs `--local` every 5 minutes as a backstop. |
| `cloudflared_setup.sh` | Installs `cloudflared`, writes `/etc/cloudflared/bmac-tunnel.json` (root:cloudflared, 0640) and the sandboxed `app-ha-cloudflared.service`. |

On the workstation:

- `scripts/lib/config.sh` validates the [new keys](#clusterconf-changes);
  `scripts/lib/guest_network_settings.sh` renders each host's settings.
- `add_proxmox_host.sh` runs `configure_guest_network` and
  `configure_tunnel` after installing the runtime; the tunnel secret is
  copied as a mode-0600 file and shredded on both ends, and is never in the
  installation ISO.
- `add_prod_vm.sh` and `remove_prod_vm.sh` run `tunnel_sync.sh --all`;
  `add_prod_vm.sh` prints the Cloudflare steps for the VM's domains.
  `add_staging_vm.sh` and `remove_staging_vm.sh` change no network state.
- `update_cluster_runtime.sh` deploys the runtime files above and runs
  `tunnel_sync.sh --all`; `sync_cluster_network.sh` and `list_slots.sh` are
  the operations described under [Dashboard](#dashboard).
- `remove_proxmox_host.sh` stops the leaving host's replica before powering
  it off.

## Testing

These changes are tested by deploying them to the real FiberState cluster
and checking the following by hand. A nested lab of Proxmox host VMs for v2,
modelled on `v3/lab`, is possible later but not part of this work.

- Prod and staging FQDNs reach the right VMs through the tunnel, including
  VMs on a different Proxmox host from the replica that carried the request.
- The replicas accept each VM's Cloudflare Origin CA certificate. The config
  verifies it against the FQDN (`originServerName`), relying on `cloudflared`
  trusting Cloudflare's own roots by default; if a replica logs certificate
  errors instead, give it the Origin CA root through `caPool`.
- Every replica connection is HTTP/2 (TCP), never QUIC, and large uploads
  and downloads through the tunnel keep their speed on FiberState.
- Taking one Proxmox host's public interface down (from its iDRAC console):
  ingress continues through the other replicas, and that Proxmox host's VMs'
  egress falls back through another Proxmox host. With three Proxmox hosts,
  taking two down leaves both ingress and egress working through the third.
- How requests divide between replicas.
- For a web app with an API: `/api/` calls on prod and staging FQDNs, from a
  script with no browser headers at a high rate, never get a challenge, with
  Super Bot Fight Mode on and Under Attack mode switched on. The same script
  on a web page path does get challenged.
- `.10` and `GUEST_GATEWAY_MAC` are never seen on the VLAN from another
  Proxmox host or from a VM on another Proxmox host. A VM's outbound traffic
  leaves through its own Proxmox host's public interface; after a live
  migration, outbound traffic continues through the new Proxmox host's.
- The probe rotation spreads requests across targets, and a target made to
  fail (blocked with a temporary nftables rule) is retired after 5 strikes.
- Editing cluster.conf's slot settings: the dashboard shows drift, and the
  sync operation brings every Proxmox host back in line.

The transfer counter's reset rules (resetting once on its day, treating a
day above 28 as 28, surviving restarts and interface counter resets) are
covered by unit tests with a controlled clock and counter readings.
