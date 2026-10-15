# BMAC: Simplified Internet Ingress and Egress

**Design proposal - October 14, 2026**  
**Scope:** Replace BMAC's HAProxy-based internet ingress and Keepalived/VRRP-based guest egress with Cloudflare hostname-to-port routing, host-level nftables DNAT/SNAT, and Proxmox SDN Simple Zone SNAT.

**Status:** Technically feasible design, **not yet tested in the BMAC lab**. Treat this as an implementation and acceptance-test plan, not an instruction to remove the working networking stack immediately.

## 1. Executive summary

BMAC does not intrinsically need HAProxy, Keepalived, VRRP, or a floating default-gateway IP merely because a production VM must retain its private IP when Proxmox HA restarts it on another host.

The simpler architecture is:

- **Ingress:** Cloudflare selects the destination port from the requested hostname. All Proxmox hosts have the same logical nftables destination-NAT rules mapping that port to a VM's stable private-VLAN IP and HTTPS port 443. The receiving host uses connection-specific source NAT so replies return through that host.
- **Egress:** Each VM has a **second virtual NIC** attached to a Proxmox SDN **Simple Zone** with its own host-local gateway and SNAT. Outbound internet connections leave using the public IP of the physical host currently running that VM. No floating gateway or VRRP election is needed.
- **Private traffic:** The VM's original virtual NIC retains its stable `10.213.0.x` address on the shared VLAN, which every Proxmox host can reach. This NIC carries cross-host access and replies to externally forwarded connections.
- **Staging:** **Each staging VM gets its own distinct Cloudflare origin port and nftables port mapping.** No shared staging reverse proxy is required. BMAC allocates, publishes, and later removes mappings as staging VMs are created and destroyed.
- **Failover:** Both public hosts can receive traffic for any VM reachable on the private VLAN. Host failure changes neither the VM's private IP nor the forwarding rules. The outbound connection source IP may change after failover.

The tradeoff is deliberate: local SDN egress does **not** automatically survive loss of a running VM host's public internet uplink while the host and VM are still running. The existing cross-host VRRP gateway can provide that extra failure-mode protection. Do not claim equivalent availability without accepting this difference.

## 2. Existing BMAC assumptions

The root-level BMAC v2 configuration describes:

| Item | Existing BMAC value / constraint |
|---|---|
| Proxmox hosts | `mox1`, `mox2`, optionally more |
| Host internet connectivity | Each host has its own public IP and upstream gateway |
| Shared private Layer-2 VLAN | `10.213.0.0/24` |
| Shared private bridge | `vmbr-private` |
| Production IP allocation | `10.213.0.31` through `.50` |
| Staging IP allocation | `10.213.0.51` through `.200` |
| Existing guest default gateway | Floating `10.213.0.10`, managed by Keepalived/VRRP |
| Existing ingress | Public TCP 80/443 forwarded to per-host HAProxy LXC |
| Public DNS and failover | Cloudflare, including existing Load Balancing |
| VM behavior | Production HA may restart on a different host; BMAC evicts staging on the destination before production starts |

These details are grounded in the [root-level BMAC cluster configuration](https://github.com/OtterFlip/bmac/blob/main/config/cluster_dot_conf) and [BMAC README](https://github.com/OtterFlip/bmac). Confirm that the active v3 implementation retains these assumptions before implementing changes.

For this document, example public IPs and staging assignments are illustrative, not actual deployment values.

## 3. Proposed topology

```text
                         INTERNET
                            |
                   Cloudflare HTTPS proxy
                   + Cloudflare Load Balancer
                   + per-hostname Origin Rules
                            |
             mydomain.com    -> origin TCP 443
             stage1prod1.*    -> origin TCP 20051
             stage2prod1.*    -> origin TCP 20052
                            |
               +------------+------------+
               |                         |
         mox1 public IP             mox2 public IP
               |                         |
        nftables DNAT/SNAT       nftables DNAT/SNAT
               |                         |
               +------- shared VLAN ----+
                       10.213.0.0/24
                            |
          +-----------------+--------------------+
          |                 |                    |
   prod1 10.213.0.31  stage1 10.213.0.51  stage2 10.213.0.52
          |                 |                    |
          +-- second NIC on host-local SDN VNet -+
                            |
                172.20.0.1 local gateway
                            |
                  Proxmox SDN SNAT
                            |
                Current host's public IP
                            |
                         INTERNET
```

The **ingress** path remains reachable via any healthy host. The **egress** path is deliberately tied to the host where the VM is actually running. These paths are independent.

## 4. Ingress: Cloudflare Origin Rules plus nftables

### 4.1 Per-VM mapping

Give each externally accessible staging VM a distinct allocated port. For example:

| Public hostname | Cloudflare origin port | Every Proxmox host DNATs to |
|---|---:|---|
| `mydomain.com` | 443 | `10.213.0.31:443` |
| `stage1prod1.mydomain.com` | 20051 | `10.213.0.51:443` |
| `stage2prod1.mydomain.com` | 20052 | `10.213.0.52:443` |
| `stage3prod1.mydomain.com` | 20053 | `10.213.0.53:443` |

The browser always connects to Cloudflare using normal HTTPS `:443`. The alternate ports are used only by Cloudflare when connecting to BMAC origins.

A Cloudflare Origin Rule for each staging hostname matches an expression such as:

```text
http.host eq "stage1prod1.mydomain.com"
```

and sets **Destination port** to `20051`. The request hostname and origin TLS SNI remain the actual staging hostname; the staging VM can continue serving HTTPS on private port 443.

**Cloudflare prerequisites and limits:**

- The DNS record must be **proxied** through Cloudflare.
- Cloudflare Origin Rules support destination port overrides from 1 through 65535.
- The standard Free plan currently allows **10 Origin Rules per zone**; Pro allows 25, Business 50, Enterprise 300. This is a real scalability constraint for a design using one Origin Rule per staging VM. Verify your actual plan and available rule capacity.
- Keep the production hostname distinct from wildcard staging DNS and ensure each staging hostname has an unambiguous origin-port rule. A wildcard DNS record by itself does **not** perform per-VM routing.
- Verify the interaction of your Origin Rules, load balancers, and health monitors in Cloudflare Trace and live tests. Do not assume a generic host-health check is sufficient for each staging VM.

**Source:** [Cloudflare Origin Rules](https://developers.cloudflare.com/rules/origin-rules/), [Origin Rules destination-port setting](https://developers.cloudflare.com/rules/origin-rules/features/).

### 4.2 nftables on every Proxmox host

Each physical host installs matching logical rules:

```text
Incoming TCP 443   -> DNAT to 10.213.0.31:443
Incoming TCP 20051 -> DNAT to 10.213.0.51:443
Incoming TCP 20052 -> DNAT to 10.213.0.52:443
Incoming TCP 20053 -> DNAT to 10.213.0.53:443
```

Also apply **connection-specific SNAT** for those forwarded connections, translating the source to the ingress host's stable private-VLAN address:

```text
Cloudflare -> mox2 public IP:20051
          -> DNAT destination 10.213.0.51:443
          -> SNAT source to mox2 private VLAN IP
          -> stage1 VM
          -> reply to mox2 private VLAN IP
          -> mox2 conntrack reverses NAT
          -> Cloudflare
```

The SNAT is important. Without it, a VM using host-local SDN egress could send the reply through a different host and break stateful NAT. SNAT makes the return path unambiguous whether the target VM is local or runs on another Proxmox host.

Implementation requirements:

1. Enable IPv4 forwarding on the Proxmox hosts.
2. Use nftables DNAT in `prerouting` and paired SNAT in `postrouting`; scope both to the appropriate interfaces and traffic. Do not globally SNAT private-VLAN traffic.
3. Allow the associated `forward` traffic in whichever nftables / Proxmox firewall rules apply.
4. Restrict public ingress ports to Cloudflare origin-facing IP ranges (including the appropriate IPv4/IPv6 considerations), and maintain that range list. Otherwise an attacker may bypass Cloudflare's protections.
5. Keep the host's SSH, Proxmox management, Corosync, replication, and other service ports separate from the web-ingress port allocation.
6. Test both same-host and cross-host NAT forwarding, with TCP and any required WebSocket traffic.
7. Keep rule management atomic and idempotent, with an inventory of allocated staging ports. On a new host, install the complete current mapping set before allowing it to be used as a public ingress endpoint.

These are **design requirements**, not tested, drop-in nftables rules. Generate exact nftables syntax against the real public NIC names, public addresses, Proxmox firewall configuration, and host private addresses in the lab.

### 4.3 Multiple staging VMs need no HAProxy

A staging VM gets:

- A stable unique private-VLAN address while it exists, allocated from `10.213.0.51-200`.
- A unique origin port, such as `20051`.
- A specific Cloudflare Origin Rule matching its hostname.
- An identical port-to-IP DNAT mapping on every active Proxmox host.

When the staging VM is deleted, withdraw or disable the Cloudflare rule and corresponding NAT mapping, then safely release the port and IP for reuse. Do not reuse a port while Cloudflare might still route requests to its previous owner. Handle partial failures with retryable cleanup and a reconciliation check.

This eliminates the previous staging HAProxy SNI-router role, **subject to Cloudflare rule quotas and any non-HTTP services that require separate handling**.

## 5. Egress: Proxmox SDN Simple Zone with SNAT

Use native Proxmox VE SDN rather than a shared Keepalived/VRRP gateway.

Create a **Simple Zone**, a VNet on all eligible Proxmox nodes, and a subnet such as:

```text
Zone type:  Simple
VNet:       bmac-egress
Subnet:     172.20.0.0/24
Gateway:    172.20.0.1
SNAT:       enabled
```

In a Simple Zone, a VNet bridge is **isolated to each physical host**. The gateway `172.20.0.1` exists separately on each host-local bridge. These identical addresses do not conflict because the bridges are not connected across hosts. Proxmox SNAT routes guest outbound traffic through the local host's normal internet route.

Each guest has two virtual NICs:

| NIC | Example IP | Network | Gateway |
|---|---|---|---|
| `lan0` | `10.213.0.31/24` | Shared `vmbr-private` VLAN | **None** |
| `wan0` | `172.20.0.31/24` | Host-local `bmac-egress` VNet | `172.20.0.1` (only default route) |

Example when prod1 runs on `mox1`:

```text
prod1 -> wan0 -> local 172.20.0.1 -> mox1 SNAT -> mox1 public IP
```

After Proxmox HA restarts prod1 on `mox2`, the guest configuration is unchanged:

```text
prod1 -> wan0 -> local 172.20.0.1 -> mox2 SNAT -> mox2 public IP
```

The shared `lan0` NIC still owns `10.213.0.31`, preserving ingress addressing and private cluster reachability.

Important qualifications:

- Guest interface names may differ; configure them by reliable identifiers and verify that **only the egress NIC installs a default route**.
- Allocate nonconflicting VNet IPs to all simultaneous guests on a host. Proxmox Simple Zones are not an inter-node guest network; cross-node traffic must use the existing private VLAN.
- Proxmox SDN's SNAT firewall rules and custom nftables ingress rules must be reviewed together for NAT priority, forwarding, and policy conflicts.
- Ensure the host still has the correct public default gateway and that the provider allows outbound traffic through its public interface.
- A VM failover changes its outbound public source IP. Existing outbound TCP connections normally break. Remote allowlists keyed to a single source IP may need updating.

**Source:** [Proxmox SDN documentation, Simple Zone and Source NAT Example](https://github.com/proxmox/pve-docs/blob/master/pvesdn.adoc).

## 6. What becomes removable

After the replacement passes acceptance testing, the following BMAC components are candidates for removal or major simplification:

| Existing mechanism | Replacement |
|---|---|
| Per-host HAProxy LXC | No LXC; host nftables DNAT/SNAT |
| HAProxy SNI routing tables | Cloudflare per-hostname Origin Rules |
| HAProxy configuration generation and route synchronization | Cloudflare rule + staging-port mapping reconciliation |
| Public 80/443 forwarding into HAProxy containers | Direct forwarding to private guest addresses |
| Keepalived processes and VRRP leader election | Independent host-local Proxmox SDN NAT gateways |
| Floating `GUEST_EGRESS_VIP` | Guest default gateway on second NIC, e.g. `172.20.0.1` |
| Guest egress VIP failover / gratuitous ARP handling | No shared VIP to manage |
| Custom VRRP-aware egress rule management | Proxmox-managed SDN SNAT |

Retain:

- Private VLAN, host-to-host transport, Corosync, Proxmox HA, and ZFS replication.
- Production and staging VM stable private IP allocation.
- Existing pre-start staging-eviction / production-priority policy.
- Cloudflare DNS, TLS, load balancing, and appropriately targeted health checks.
- Minimal host nftables ingress rules, plus the small staging port-allocation/rule-lifecycle controller.

**Do not remove** old mechanisms merely because their replacements have been configured. Remove them only after the failover and network tests below pass and a rollback exists.

## 7. Failover behavior and deliberately lost capability

| Event | Simplified design behavior |
|---|---|
| Production VM moves/restarts on mox2 | Same private IP; both hosts' ingress NAT still reaches it; outbound uses mox2's public IP |
| Production host dies | Cloudflare should route to another healthy public host; PVE HA restarts the VM elsewhere; no NAT or gateway VIP updates |
| Staging VM on mox2 is evicted before prod1 starts | Preserve existing BMAC eviction logic; its hostname may legitimately become unavailable |
| A public ingress host fails while the VM remains on another host | Cloudflare selects another healthy origin; its DNAT can cross the VLAN to that VM |
| VM's host WAN fails, but VM and VLAN remain healthy | Ingress may continue through the other host; **egress from that VM fails until its host's public route recovers or VM moves** |
| Source public IP matters to an external service | The failover source-IP change may require allowlist adjustments |
| Cloudflare unavailable | Public Cloudflare-proxied ingress unavailable; there is no automatic direct-origin substitute in this proposal |

The lost *independent egress-gateway failover* feature is the main functional regression compared with Keepalived/VRRP. Decide explicitly whether that protection is needed before removing it.

## 8. Cloudflare and security details

- Set Cloudflare TLS mode appropriately, ideally **Full (strict)**. Each VM needs an origin certificate valid for the requested hostname (a suitable wildcard can cover single-level staging subdomains).
- Avoid exposing host Proxmox management to the public internet. Origin-facing NAT ports are for application traffic only.
- Since private-side ingress SNAT hides the TCP client's source IP, the guest must recover the real visitor address from Cloudflare's `CF-Connecting-IP` header **only for trusted ingress paths**. Never trust client-supplied forwarding headers from arbitrary networks.
- If TLS terminates inside each VM, Cloudflare can connect directly to that VM's NGINX. There is no need for a host-side TLS proxy.
- Configure real application checks rather than relying only on a Proxmox-host-up check. Check Cloudflare LB monitor behavior on the rewritten origin ports and the actual `/healthz` (or equivalent) endpoint.
- Cloudflare Origin Rule quotas make one-rule-per-stage a good small-scale design, not an unlimited staging-routing solution. Monitor remaining quota before creating new staging VMs.
- Check the desired behavior of port 80 / HTTP-to-HTTPS redirects. If Cloudflare performs redirects at the edge, you may not need to expose inbound origin port 80 at all.

## 9. Migration and acceptance-test plan

### Phase A: Prove the egress replacement

1. In a disposable lab cluster, build the SDN Simple Zone, VNet, gateway and SNAT on all eligible nodes.
2. Give a disposable VM two NICs and verify `ip route`, `curl` to the internet, DNS resolution, and the observed public source IP.
3. Relocate/restart the VM on another Proxmox node. Confirm that addresses/routes inside the guest are unchanged and the observed outbound public IP now belongs to the new host.
4. Verify that inbound responses on the shared VLAN do not escape via the default-route NIC.
5. Repeat with the host's WAN intentionally disabled while the private VLAN remains healthy; document the lost independent-egress behavior.

### Phase B: Prove basic ingress without HAProxy

1. On one test host, implement DNAT plus return-path SNAT for a disposable HTTPS VM.
2. Restrict incoming public traffic to appropriate Cloudflare sources and verify HTTPS TLS/SNI end to end.
3. Use Cloudflare to send one test hostname to origin port 443 and another hostname to a nonstandard origin port.
4. Verify that both hostnames reach distinct VMs and that a request arriving through the *other* Proxmox host also works.
5. Test health checks, HTTP redirects, WebSockets, and the real visitor IP handling.
6. Repeat on all eligible public ingress hosts.

### Phase C: Prove per-staging-VM lifecycle

1. Allocate unique private IP/port pairs for two staging VMs.
2. Install the nftables maps on all hosts and configure one Cloudflare Origin Rule per staging hostname.
3. Verify simultaneous access to both stages and production over browser HTTPS port 443.
4. Destroy one stage; remove its Cloudflare and NAT mapping; verify the other stage remains accessible.
5. Attempt collision/reuse, interrupted provisioning, and host addition/removal; reconciliation must detect stale or mismatched mappings.
6. Explicitly test behavior at the Cloudflare Origin Rule plan quota.

### Phase D: Failover and safe cutover

1. Keep the existing HAProxy/VRRP path working until the new path passes its tests.
2. Test production HA failover while stages run on the destination; verify the staging eviction hook still runs before production.
3. Test cloud-side failover from public mox1 to public mox2, including cross-host forwarding.
4. Record external client IP, inbound TLS certificate, application connectivity, outbound public IP, and guest route tables before and after.
5. Change traffic to the new design through controlled Cloudflare and firewall updates; retain a rollback plan.
6. Only then disable and remove HAProxy LXCs, Keepalived/VRRP, floating VIP rules, and their associated BMAC automation.

## 10. Implementation scope for a coding agent

Build a *small networking adapter*, not a new proxy or gateway subsystem:

1. **Port allocator:** Deterministic, persistent, collision-free staging hostname/private IP/origin port mapping with a reserved port range.
2. **Cloudflare controller:** Create/update/delete exact-hostname Origin Rules. Reconcile rules and enforce plan quotas. Preserve existing Cloudflare Load Balancing rather than trying to reimplement it.
3. **nftables compiler:** Generate identical logical port-to-guest mappings for every public host, with host-specific public/private interface and SNAT addresses. Apply safely and idempotently.
4. **Guest NIC template:** Preserve the shared-VLAN NIC and add the SDN SNAT NIC as the only default-route interface.
5. **Health/verification:** Check per-VM external HTTPS reachability, outbound connectivity, and same-host/cross-host forwarding.
6. **Cleanup:** Coordinate Cloudflare rule retirement, firewall removal, port release, and staging VM deletion with retries and no accidental reassignment.

Keep the existing cluster, HA, replication, staging cloning, and staging eviction components unchanged except where guest NIC definitions and network integration actually require modifications.

## 11. Decision and caveats

**Recommended direction:** Prototype **Cloudflare exact-hostname-to-origin-port mapping + nftables DNAT/SNAT on every Proxmox host + two-NIC guest networking with PVE SDN Simple Zone SNAT**.

It directly meets the stated requirement: stable private VM identity across Proxmox HA failover, HTTP/HTTPS ingress to production and multiple staging VMs, and ordinary outbound internet access without HAProxy or a floating VRRP gateway.

**Before committing:** Verify Cloudflare Origin Rule quotas, cross-host NAT symmetry, Proxmox firewall coexistence, application health checks, and acceptance of the lost independent-WAN-failure egress protection. These are the important engineering unknowns; none requires assuming HAProxy/VRRP is inherently mandatory.

## References

- [BMAC root-level README](https://github.com/OtterFlip/bmac)
- [BMAC root-level cluster networking configuration](https://github.com/OtterFlip/bmac/blob/main/config/cluster_dot_conf)
- [Proxmox VE Software-Defined Networking - Simple Zones and Source NAT](https://github.com/proxmox/pve-docs/blob/master/pvesdn.adoc)
- [Cloudflare Origin Rules](https://developers.cloudflare.com/rules/origin-rules/)
- [Cloudflare Origin Rules destination port documentation](https://developers.cloudflare.com/rules/origin-rules/features/)
- [Cloudflare Origin Rules example: change destination port](https://developers.cloudflare.com/rules/origin-rules/examples/change-port/)
- [Cloudflare account-level Load Balancing configurations](https://developers.cloudflare.com/learning-paths/load-balancing/planning/multiple-zones/)

## TODO

Adjusts above design to accommodate multiple prod VMs each running a webapp for a different domain.  This likely means we need a CloudFlare Origin rule and nftables entries for all of the prod VMs too, and have one for port 80 for all of them so that any of them can potentially answer to a /healthz check. That way it's a single mechanism for setting up the nftables for all VMs, prod or staging.  Then at CloudFlare we just need to update which domain is going to be the one (which prod VM) that actually handles the /healthz request. This way if we retire a prod VM we only have one change to make - at CloudFlare - regarding these settings, and we don't need to update them in the future either - we can just pre-define the entire fixed set of domains and ports that get forwarded and the machine IP they're forwarded to, even if many of them point to machines that don't exist at the moment that's fine.