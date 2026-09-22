# Future: what FiberState can change to simplify this cluster

We run two dedicated servers at FiberState as a small private cloud
(Proxmox). Each server already has a public internet address on its primary
network port. Virtual machines (the actual apps) sit on the **private 10
Gbps VLAN** FiberState set up on each server’s **secondary** NIC. Both
servers can talk to each other on that VLAN. VMs keep the same private
address when they move from one server to the other.

That VLAN cannot reach the internet today. It is a `/24`, and FiberState
reserved `.1` as the gateway, but that address does not actually route
traffic in or out. So we had to build our own workaround: extra software on
every server that accepts public web traffic and separately shares one
path out to the internet. That workaround is the complicated part of this
project. It is also work a datacenter can do once, correctly, for every
customer who wants VMs on a private VLAN.

**The ask:** sell us **six extra public IPv4 addresses** (we will pay for
them) — one for the production VM and five for staging VMs — and make those
addresses work on the private VLAN in **both** directions (inbound and
outbound using the same public IP per VM). We do not need a third network
card. We do not want those new addresses glued to a physical server; they
must follow the VM.

Once that VLAN actually routes to the internet, the **servers do not need
public IPs at all**. Each box already has a private address on the VLAN; it
can use `.1` as its default gateway, same as the VMs. Outbound traffic from
the hypervisors (package updates and the like) only needs ordinary NAT on
that gateway — dedicated public IPs are for the VMs, so the world can
reach the apps. We manage the servers over Tailscale and iDRAC, not over
public SSH. The primary network ports can stop being the internet path.

If FiberState does that, we can delete the extra proxy and failover-routing
software entirely. The internet talks to each VM’s own public IP. If a
server fails, the VM starts on the other box and the VLAN already knows
where it is.

Do inbound and outbound for the VMs in the same change. If VMs start
sending replies out FiberState’s gateway while public web traffic still
hits the old per-server addresses, connections break.

The options below are easiest complete fix first. **Any one of them**
replaces our custom internet path. They all keep the current VLAN so the
two servers still talk to each other on it. They differ only in **who holds
the public IP and how a packet finds the VM**.

## Option 1 — NAT on the gateway you already have (best)

The VM **never sees the public IP**. It keeps only its private address.
FiberState’s `.1` gateway translates internet traffic to and from that
private address (same public IP in and out). Turn `.1` into a real internet
gateway for the whole VLAN (servers and VMs). No new VLAN. This matches how
the VLAN is wired today (secondary NICs on both servers). The servers set
their default gateway to `.1` and can drop the public addresses on the
primary ports.

## Option 2 — Put the public IPs on a VLAN (a real public LAN)

The VM **owns the public IP** on a public subnet that lives on Ethernet,
like any ordinary LAN. Give us that subnet on the existing secondary-NIC
VLAN, or (cleaner) a second VLAN on those same 10 Gbps ports. The VM
configures the public address and uses FiberState’s gateway *on that
subnet*. No translation: switches find the VM by ordinary MAC/ARP. The
original private VLAN stays up so the servers still talk to each other.

## Option 3 — Route extra public IPs onto the VLAN (not a public LAN)

The VM **still owns the public IP**, but FiberState does **not** put a
whole public subnet on the VLAN. Their routers just say “these six
addresses are reachable via that private VLAN / via `.1`.” This is the
usual “additional IP” product when they will not bridge a public LAN onto
our ports. The VM holds the public address as a single host address on the
existing private NIC and uses `.1` as its gateway. No NAT, and no public
broadcast domain. The route must aim at the VLAN (or at the VM), **not** at
one physical server’s public port, or failover would break.

## Option 4 — A small firewall in front of the VLAN (option 1 on an appliance)

Same idea as option 1: VMs keep private addresses; a FiberState box NATs
each public IP to the matching private IP. Use this if `.1` on the current
gateway cannot do that NAT. Put a firewall FiberState already supports in
front of the VLAN and do the mapping there. We pay for the appliance. The
VLAN itself does not change, so the servers still talk to each other. We
do not need FiberState to run our websites or a load balancer.

After any of these, this project is mostly: install servers, run VMs, copy
disks between them, and fail over. FiberState owns the internet path.
Customers who want this kind of private cloud will look for a datacenter
that can do exactly that.
