<!--
Copyright (c) 2026 BEENTHERE VENTURES, INC.
SPDX-License-Identifier: GPL-3.0-only
-->

# BMAC v2: simplified internet ingress and egress

**Status:** design agreed, not yet implemented or lab-tested.
**Scope:** BMAC v2 only (everything outside `v3/`). There is no deployed
cluster yet, so nothing here needs to stay compatible with the current
HAProxy/VRRP design or migrate from it.

This design replaces:

- the per-Proxmox-host HAProxy LXCs and their route-synchronization
  machinery (ingress), with **one Cloudflare Tunnel run on every Proxmox
  host**;
- Keepalived/VRRP and the floating guest-egress address (egress), with **a
  shared gateway that every Proxmox host provides to its own VMs**, plus a
  fallback through another Proxmox host's internet connection;
- dynamically allocated staging addresses and routes, with **fixed address
  slots that are set up in advance**.

## Contents

- [Terms](#terms)
- [Why](#why)
- [Goals](#goals)
- [Address plan](#address-plan)
- [Egress: the shared gateway](#egress-the-shared-gateway)
- [Egress fallback](#egress-fallback)
- [Ingress: one Cloudflare Tunnel on every Proxmox host](#ingress-one-cloudflare-tunnel-on-every-proxmox-host)
- [FiberState UDP limit: the tunnel runs over TCP](#fiberstate-udp-limit-the-tunnel-runs-over-tcp)
- [API traffic and bot protection](#api-traffic-and-bot-protection)
- [Cloudflare setup](#cloudflare-setup)
- [Staging lifecycle](#staging-lifecycle)
- [Dashboard](#dashboard)
- [Failure behaviour](#failure-behaviour)
- [What changes in the code](#what-changes-in-the-code)
- [To verify in the lab](#to-verify-in-the-lab)

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

In the current v2 code, every staging create and remove changes ingress
state and waits for it to converge on every Proxmox host:

- creation enables the VM's route and runs the HAProxy route
  synchronization (`enable_route_and_sync` in
  `scripts/user_callable/guests/staging/add_staging_vm.sh`);
- removal disables the route and queues `remove-route` cleanup, which runs
  the same synchronization
  (`scripts/user_callable/guests/staging/remove_staging_vm.sh`,
  `scripts/host_runtime/process_deferred_cleanup.sh`).

That synchronization renders, validates and reloads HAProxy on every Proxmox
host under a cluster-wide lock. It often adds **several minutes** to each
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
   cluster.conf's ranges.
3. **No single Proxmox host's internet connection is a single point of
   failure**, for ingress or egress, including a Proxmox host that loses its
   connection by reaching its provider's transfer cap.
4. **API calls are never answered with a Cloudflare challenge**, while web
   pages keep Cloudflare's bot protection.
5. **Low cost.** Everything is free except Cloudflare Pro on each prod VM's
   domain (see [API traffic and bot protection](#api-traffic-and-bot-protection)).

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
| `MAX_STAGING_VMS_PER_PROD` | **New.** Staging slots per prod VM; 0–9, validated. |
| `VM_SLOT_START` | **New.** prod1's address. Its last octet must end in 0, and every slot (`VM_SLOT_START` + `10×MAX_PROD_VMS − 1`) must fall inside `PRIVATE_SUBNET_CIDR` without overlapping any other range or address. |
| `GUEST_GATEWAY_IP` | **Replaces `GUEST_EGRESS_VIP`.** A plain address (e.g. `10.213.0.10`), no longer floating. |
| `GUEST_GATEWAY_MAC` | **New.** The shared gateway's MAC address, identical on every Proxmox host. Must be a locally administered unicast MAC (e.g. `02:00:0a:d5:00:0a`). |
| `CLOUDFLARE_TUNNEL_ID` | **New.** The tunnel's ID (not secret). Its credentials file is kept beside `secrets.env` in `config/` and is git-ignored. |
| `PRODUCTION_IP_START/END`, `STAGING_IP_START/END` | **Removed** (replaced by the slot keys). |
| `HAPROXY_IP_START/END`, `HAPROXY_LXC_*` | **Removed.** |

Each `config/moxN.conf` loses `HAPROXY_LXC_*` and `VRRP_PRIORITY`.
`MAX_STAGING_VM_COUNT_ON_THIS_HOST` stays, is still enforced at staging
creation (it reflects each Proxmox host's RAM), and must be no more than
`MAX_STAGING_VMS_PER_PROD`.

## Egress: the shared gateway

Every VM keeps its single NIC on `vmbr-private` and uses `GUEST_GATEWAY_IP`
(`.10`) as its only default gateway, exactly as in the current v2 design.
What changes is who answers for `.10`: **every Proxmox host does, but only
for its own VMs.** Nothing is elected and nothing moves between Proxmox
hosts.

### The gateway interface

Every Proxmox host has a small virtual interface attached to `vmbr-private`
(for example, a veth pair with one end enslaved to the bridge). It holds:

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
public source address changes. The current VRRP design has the same
behaviour when `.10` moves.

### Outbound NAT

Each Proxmox host masquerades forwarded traffic whose source is any VM slot
out its own public interface. The rule covers every slot from cluster.conf,
so it is installed once at Proxmox host setup and never changes when VMs are
created or removed. External services that allowlist source addresses must
list every Proxmox host's public address, as today.

## Egress fallback

When a Proxmox host's own internet connection is down (including when it has
reached its transfer cap), its VMs' outbound traffic goes out through another
Proxmox host instead. Each Proxmox host decides this on its own; there is no
shared state and no election.

### Static part (installed at Proxmox host setup)

- Traffic arriving on the gateway interface is looked up in its own route
  table, `guest-egress` (an `ip rule` on the incoming interface). Normally
  that table's default route is the Proxmox host's own internet gateway.
- Every Proxmox host also masquerades VM traffic handed to it by another
  Proxmox host over the VLAN. That traffic arrives on `vmbr-private`, not on
  the gateway interface, so it uses the main route table and always leaves
  through this Proxmox host's own connection. It is never passed on again,
  so loops are impossible.
- ICMP redirects are disabled on the private VLAN. Otherwise a Proxmox host
  forwarding a VM's packet to another Proxmox host would tell the VM to send
  directly, bypassing `.10`.
- Connection tracking on the forwarding Proxmox host must tolerate seeing
  only the outbound half of each connection, because replies return from the
  other Proxmox host straight to the VM. The forward path for VM traffic must
  not drop packets classified as invalid.

### The health service

A small service on each Proxmox host:

1. Checks its own internet connection every few seconds with **TCP** probes
   (HTTPS requests to two or three independent targets), sent through its
   own public interface. No UDP or ICMP, because of FiberState's UDP filter.
2. After about 3 consecutive failures, points `guest-egress`'s default route
   at the private address of the first other Proxmox host through which a
   test connection reaches the internet.
3. After about 5 consecutive successes on its own connection, points
   `guest-egress` back at its own internet gateway.
4. Logs every switch and exposes its current state for the dashboard.

Estimated size: 150–250 lines plus tests.

## Ingress: one Cloudflare Tunnel on every Proxmox host

### Shape

- **One tunnel for the whole cluster**, created once.
- **Every Proxmox host runs a `cloudflared` replica of that tunnel** as a
  sandboxed systemd service. Each replica keeps several outbound connections
  open to Cloudflare. No Proxmox host listens on a public port for web
  traffic.
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
VM egress that has failed over to them. The dashboard shows each Proxmox
host's transfer counters so this is visible. If even spreading ever becomes
necessary, Cloudflare Load Balancing can be added later.

### The config file

- Generated by BMAC from cluster.conf and the registry, stored in pmxcfs,
  installed on every Proxmox host.
- One entry per FQDN of every prod VM: its domain, its aliases, and
  `stage1prodN.<domain>` through `stage<MAX_STAGING_VMS_PER_PROD>prodN.<domain>`,
  each mapped to its slot as `https://<slot address>:443`.
- A final catch-all entry returns 404 for any other FQDN.
- `protocol: http2` (see the next section).
- Changes only when a prod VM is created or removed, or when cluster.conf's
  ranges change. On a change, BMAC restarts the replicas one Proxmox host at
  a time, so at least one replica stays connected.

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

### Requirements

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
pages keep bot protection. Therefore every prod VM's domain:

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

- Request bodies up to 100 MB on Pro.
- Cloudflare waits at most 100 seconds for the VM's response, so
  long-polling or slow API calls must finish sooner.
- By default Cloudflare caches only static file types, so API responses
  aren't cached.

## Cloudflare setup

These steps replace README.md's current Cloudflare instructions (load
balancer, origin pools and monitors are no longer needed). BMAC holds no
Cloudflare API token; the steps are manual and short, and BMAC's dashboard
shows exactly what each domain needs.

### Once per cluster

1. On the admin workstation, run `cloudflared tunnel login`, then
   `cloudflared tunnel create bmac-<cluster name>`. This prints the tunnel ID
   and writes its credentials file.
2. Put the tunnel ID in cluster.conf (`CLOUDFLARE_TUNNEL_ID`) and copy the
   credentials file into `config/`. Proxmox host setup installs
   `cloudflared` from Cloudflare's apt repository and starts the replica on
   every Proxmox host.

### Once per prod VM's domain

Done when the prod VM is created; about 5 minutes.

1. The domain is a Cloudflare zone (its nameservers point at Cloudflare).
2. Subscribe the domain to **Cloudflare Pro**.
3. Add two **proxied** DNS records, both `CNAME → <tunnel ID>.cfargotunnel.com`:
   - `@`, the domain itself (`example.com`);
   - `*`, every FQDN one level below it (`www.example.com`,
     `stage1prod1.example.com`, and so on).
   Cloudflare's free certificate covers the domain and one level below it,
   which includes every staging FQDN.
4. Turn on **Always Use HTTPS**, so no Proxmox host ever needs to accept
   plain HTTP.
5. Leave **Bot Fight Mode off**, turn **Super Bot Fight Mode on**, and add the
   [`/api/` skip rule](#the-api-skip-rule) as the first custom rule.
6. For an alias domain in a different zone, repeat these steps in that zone.

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

- **Slot table:** every prod and staging slot with its address, the VM using
  it (if any), its FQDNs, and the Cloudflare steps its domain needs.
- **Drift warning:** shown when anything installed on a Proxmox host (the
  egress slot range and gateway settings, or the tunnel config version)
  doesn't match cluster.conf and the registry.
- **"Sync cluster with cluster.conf" Proxmox-host operation:** regenerates
  the egress rules and the tunnel config and pushes them to every Proxmox
  host. Used rarely, when cluster.conf's ranges have been edited.
- **Per-Proxmox-host status:** internet connection health, whether egress is
  currently falling back through another Proxmox host, replica connection
  state and protocol (flagging anything not HTTP/2), and transfer counters.

## Failure behaviour

| Event | Ingress | Egress |
|---|---|---|
| A Proxmox host loses its internet connection (or reaches its cap) | Its replica disconnects; other replicas carry its VMs' traffic over the VLAN. | Its VMs fall back through another Proxmox host after about 3 failed probes; open outbound connections break. |
| A Proxmox host fails completely | Its replica disappears; other replicas carry everything. Proxmox HA restarts prod VMs elsewhere at the same slot address. | Restarted VMs use their new Proxmox host's gateway. |
| A VM is live-migrated | Unaffected; slot address unchanged. | Continues through the new Proxmox host immediately (same gateway MAC); open outbound connections break. |
| Every Proxmox host loses its internet connection | Unavailable. | Unavailable. |
| Cloudflare is unavailable | Unavailable; there is no direct path. | Unaffected. |

## What changes in the code

Removed entirely:

- `scripts/host_runtime/haproxy_routes.py`,
  `scripts/host_runtime/sync_haproxy_routes.sh` and their tests;
- HAProxy LXC creation, public 80/443 DNAT and the ingress timers in
  `scripts/user_callable/hosts/add_proxmox_host.sh`; their teardown in
  `remove_proxmox_host.sh` and `scripts/lib/host_membership.sh`;
- Keepalived/VRRP setup and its readiness script in `add_proxmox_host.sh`;
- the `ingress/` records in pmxcfs and the registry code that reads and
  writes them (`scripts/host_runtime/cluster_registry.py`);
- route enable/sync and `remove-route` handling in `add_staging_vm.sh`,
  `remove_staging_vm.sh`, `remove_prod_vm.sh` and
  `process_deferred_cleanup.sh`; `routes_enabled` in the registry, the guest
  lifecycle hook, the diagnostics and the dashboard.

Changed:

- `scripts/lib/config.sh` and `config/cluster_dot_conf`, `config/moxN_dot_conf`:
  the [new and removed keys](#clusterconf-changes) and their validation.
- `cluster_registry.py`, `add_prod_vm.sh`, `add_staging_vm.sh`: slot-based
  addresses instead of first-free allocation.
- `add_proxmox_host.sh`: shared gateway interface and bridge rules, slot-range
  masquerade, `guest-egress` route table and rule, kernel settings, egress
  health service, `cloudflared` install and replica service.
- `scripts/user_callable/hosts/update_cluster_runtime.sh`: deploys the tunnel
  config instead of synchronizing HAProxy routes.
- Diagnostics (`show_proxmox_host_state.sh`, `show_cluster_state.sh`,
  `show_cluster_health`): report the gateway, fallback state and replica
  instead of HAProxy and VRRP.
- Dashboard: the slot table, drift warning, sync operation and status above.
- `README.md`: the [Cloudflare setup](#cloudflare-setup) above, replacing the
  load-balancer instructions. `docs/MAIN_DESIGN.md`: networking sections
  rewritten to match this document.

New:

- The tunnel config generator (Python, alongside `cluster_registry.py`).
- The egress health service and its systemd unit.
- The "sync cluster with cluster.conf" script and dashboard workflow.

## To verify in the lab

- Prod and staging FQDNs reach the right VMs through the tunnel, including
  VMs on a different Proxmox host from the replica that carried the request.
- Every replica connection is HTTP/2 (TCP), never QUIC. A large upload and
  download through the tunnel keep their speed on FiberState.
- Cutting one Proxmox host's internet connection: ingress continues through
  the other replicas, and that Proxmox host's VMs' egress falls back. Measure
  both gaps.
- How requests divide between replicas.
- `/api/` calls on prod and staging FQDNs, from a script with no browser
  headers at a high rate, never get a challenge, with Super Bot Fight Mode on
  and Under Attack mode switched on. The same script on a web page path does
  get challenged.
- Staging create and remove make no network change and wait on no other
  Proxmox host. Time both against the current v2 code and confirm the
  multi-minute delay is gone.
- `.10` and `GUEST_GATEWAY_MAC` are never seen on the VLAN from another
  Proxmox host or from a VM on another Proxmox host. A VM's outbound public
  address is its own Proxmox host's; after a live migration, outbound traffic
  continues and the public address is the new Proxmox host's.
- Editing cluster.conf's ranges: the dashboard shows drift, and the sync
  operation brings every Proxmox host back in line.
