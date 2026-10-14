<!--
Copyright (c) 2026 BEENTHERE VENTURES, INC.
SPDX-License-Identifier: GPL-3.0-only
-->

# BMAC v3 deployment

How hosts and backup appliances get installed, how they join a cluster, and
how BMAC itself is upgraded. Read [`ARCHITECTURE.md`](ARCHITECTURE.md) first.

## BMAC Deployer

The Deployer starts as a copy of the v2 Dashboard (`dashboard/`) in
`v3/deployer/`, renamed and reduced. It keeps Tauri 2 and React, and uses the
shared component package `v3/ui/` (`ARCHITECTURE.md`, "Code organization").
v2's `dashboard/` stays as it is.

It does six things:

1. Discover a target machine's hardware by booting it into a temporary
   discovery OS.
2. Build a Proxmox host installer ISO.
3. Build a backup appliance installer ISO.
4. Tell the operator how to boot the target from those ISOs, and once a new
   host is on the tailnet, hand off: check its certificate, set up this
   workstation's SSH access to it, and give the operator the link to its
   Web Dashboard (see "Handoff"). Host setup then continues in the Web
   Dashboard, not in the Deployer.
5. Manage this workstation's SSH access to hosts, and jump SSH through a
   host to guests (see "Workstation SSH").
6. Deploy the hello app to a production VM over its jump SSH entry, as v2's
   `deploy_hello_app_to_prod.sh` does, including the pasted Cloudflare
   Origin CA certificate and key, which the Deployer doesn't keep after
   the deployment.

The operator always boots the target, using one of three methods chosen
per target (see "Booting the target"): the iDRAC's Virtual Console, any
other remote console, or physical media. The Deployer gives the steps and
waits.

Everything else from the v2 Dashboard is removed, not hidden: the overview
pages, operations, workflow supervisor, config editor, run history, and the
bundled scripts. v2's add-host progress pages move to the Web Dashboard's
job view (see "Host setup job"). The Deployer never calls `bmacd`'s API: it
is an ISO builder with instructions, plus the work that belongs on the
workstation: discovery, the certificate check, SSH setup, and the hello
app deployment.

It runs on amd64 Linux, because `proxmox-auto-install-assistant` is an amd64
Linux binary. It needs `xorriso` and a Tailscale client on the workstation.

**State.** The Deployer keeps only:

- a cache of verified source ISOs (Proxmox VE, Alpine Extended for
  discovery, and Ubuntu Server for appliances), checked
  against pinned SHA-256 hashes;
- its own settings, including the list of ISOs it built ("ISO lifecycle");
- temporary build directories, deleted when a build finishes or is
  abandoned.

It holds no cluster identity, no join tokens, and no secrets between runs.
For every host after the first, and for every appliance, it imports a join
bundle file that the running cluster issued.

## Hardware discovery

Disk selection needs exact disk identities (`STORAGE.md`). iDRAC's inventory
is not reliable enough for that: on Dell PowerEdge R640s it is often
inaccurate, and frequently lists no disks at all while the server is powered
off. So the Deployer never uses iDRAC's (or any BMC's) hardware inventory.
Discovery always boots the target into a temporary **discovery OS**, whose
only job is to report the hardware as the running machine sees it.

### The discovery ISO

The Deployer builds it from Alpine Extended, for one target and one
session. The source is a pinned release by default; the operator can choose
another Alpine ISO in Settings by URL and SHA-256, and the Deployer checks
the download against that hash. Extended carries broad hardware support and
ZFS. The Deployer adds:

- Tailscale, with a single-use, **ephemeral** auth key tagged
  `tag:bmac-discovery`, entered by the operator. Ephemeral nodes leave the
  tailnet on their own once they go offline;
- OpenSSH, accepting only key login as root with a client key the Deployer
  generated for this session. The Deployer also generates the SSH **host** key
  and embeds it, so it knows the fingerprint in advance and never has to trust
  on first use;
- network settings for the target's public port, all optional. By default
  the discovery OS asks DHCP for an address. An operator whose network has
  no DHCP gives a static address, prefix, gateway, and DNS servers. Given a
  MAC, it tries that NIC first; with static settings it uses that NIC only,
  and with DHCP it goes on to each other physical NIC in turn. It uses the
  link-wait rules below and stops when one gets an address, its gateway
  answers, and Tailscale's control server resolves, giving up after three
  rounds over the NICs;
- `bmac-inventory`, the v3 replacement for v2's `cluster_setup_prereq.sh`.

Alpine boots "diskless": it installs its packages into RAM and runs from
there. The Deployer adds one file to the stock ISO, an overlay
(`bmac-discovery.apkovl.tar.gz`) that Alpine's init finds on the boot media
and unpacks over the root, and cuts both boot menus (GRUB for UEFI,
syslinux for BIOS) to one entry that starts at once with every boot message
shown. OpenRC's `local` service then starts the discovery script, which:

1. copies the kernel modules and firmware image into RAM and mounts it from
   there. A remote console's virtual media can stall for minutes, which a
   live system reading its root from the media turns into read errors (the
   reason v3 doesn't use Ubuntu's 4 GB live ISO), so after this nothing reads
   the media;
2. installs its tools from the media (OpenSSH, ZFS, `lsblk`, `iproute2`, and
   the rest), each on its own so a release without one still boots;
3. brings up the public port, by DHCP or with the static settings. A NIC
   that fails any check gives its address back, so only the public port
   ends up with one;
4. switches device handling from busybox `mdev` to udev, installing `eudev`
   from the Alpine mirror of the media's own release (`apk` checks it
   against the signing keys on the media). Disk serials and WWNs must come
   from udev, because the Proxmox installer's disk filters match udev's
   values;
5. starts SSH and joins the tailnet with the pinned Tailscale build carried
   in the overlay, then prints a "ready" banner on the console once
   Tailscale reports itself online with an address. Alpine's login prompt
   appears about 30 seconds before the banner; the boot steps tell the
   operator not to log in, but to wait for the banner.

The discovery OS writes to a disk only in the one case below (clearing old
`rpool` labels, which the operator confirms disk by disk). It enables no
swap, imports no pool, mounts no disks, and `bmac-inventory`'s other
commands only read.

### The discovery session

1. The operator enters the ephemeral Tailscale key, and the network settings
   if the target's network has no DHCP.
2. The Deployer builds the discovery ISO, and the target is booted from it by
   any of the methods in "Booting the target".
3. The Deployer waits for the machine to appear on the tailnet under its
   generated name (`bmac-discover-<random>`), then connects with SSH, checking
   the pinned host key.
4. It runs `bmac-inventory --json`, which reports:
   - every disk: `/dev/disk/by-id` paths, WWN, serial, model, exact capacity,
     logical and physical sector size, transport, rotational or not, SMART
     health summary, the controller it sits behind and whether that is a
     RAID logical disk, and any existing partitions or filesystem signatures,
     including old ZFS labels (pool name and GUID), read without writing;
   - every NIC: name, MAC, driver, carrier, link speed, and how long carrier
     took to appear, measured with the link-wait rules below. This confirms
     the 10 Gbps private NIC before installation;
   - CPU, memory, firmware boot mode (UEFI or BIOS), Secure Boot state, system
     model and service tag.
5. The Deployer shows the inventory. A NIC that never got carrier is shown
   as "no carrier after N seconds", with a button to measure again, because a
   cable or switch port fixed during the session should not need a reboot.
6. **Old `rpool` labels.** The Proxmox installer erases only the disks it
   installs to. A disk that was a raw member of an earlier `rpool` keeps its
   label, and the first boot then finds two pools named `rpool` and stops at
   the `(initramfs)` prompt, before anything of BMAC runs. So when any disk
   carries an `rpool` label, the Deployer lists those disks, warns that the
   installed host may not boot unless they are cleared, and offers to clear
   them now: every listed disk is selected, each can be deselected, and a
   destructive confirmation names them by serial. `bmac-inventory` then
   re-identifies each disk by serial, WWN, and the old pool's GUID, refuses
   on any mismatch, runs `zpool labelclear -f` on the labelled devices only,
   checks the labels are gone, and the inventory is read again. If the
   operator declines, the warning stays on the host's card and in the
   installation steps, with v2's instructions for recovering at the
   initramfs prompt. Labels of other pools don't stop the boot and are only
   shown.
7. The Deployer shuts the discovery OS down, and tells the operator to
   remove the media. The ephemeral node leaves the tailnet. The SSH keys are
   discarded.

### Link-wait rules

Some links take a long time to come up. On FiberState's private 10 Gbps
VLAN, carrier often isn't there on the first look and appears only after
several seconds of negotiation. v2's `add_proxmox_host.sh` learned this. v3
keeps its lessons in one function in `bmac-inventory`, used by the discovery
OS, by the host setup job's private-network phase, and by the NIC collector:

1. Bring the NIC administratively up (`ip link set dev <nic> up`) before
   reading anything. A NIC that is down reports no carrier.
2. Poll `/sys/class/net/<nic>/carrier` repeatedly: by default 10 attempts,
   3 seconds apart, about 30 seconds in all, as v2 does. Both numbers are
   settings, so a slower provider can be given longer.
3. Read the link speed only after carrier is up. Speed reads as unknown or
   wrong while the link is still negotiating.
4. Report "no carrier after about N seconds" for a NIC that never came up,
   distinct from a NIC with carrier at the wrong speed.
5. Automatic detection of the private NIC (when its MAC isn't configured)
   applies the same wait to every candidate NIC first. v2's detection read
   carrier only once, so a slow link could be missed or picked wrongly. As in
   v2, detection succeeds only when exactly one candidate has carrier.
6. The discovery OS leaves NICs up but configures no address on any NIC
   other than the public one.

v2's test `test_private_nic_carrier_allows_slow_ten_gigabit_negotiation` is
carried over as a test of this function.

The inventory is then used to choose vdev members and NICs, both for the
Proxmox ISO and for the appliance ISO. It stays in the Deployer's memory for
the current build and can be exported to a file.

**Fallbacks.**

- **No Tailscale on the target network:** run `bmac-inventory --json` from any
  Linux live environment, as with v2's prerequisite script, and import the
  file. It is still the machine's own view, so it is treated the same.

There is no iDRAC-inventory fallback.

## Booting the target

Every ISO the Deployer builds (discovery, Proxmox installer, appliance
installer) is mounted and booted by the operator, in one of three ways. The
operator picks one per target, and can switch method at any step. The
Deployer shows the steps for the chosen method and the ISO's path.

| Method | The operator's steps |
| --- | --- |
| iDRAC Virtual Console | Open the iDRAC web interface (linked when the operator gave its address) and launch the Virtual Console; Virtual Media, Connect Virtual Media; choose the ISO under Map CD/DVD and click Map Device; Boot, Virtual CD/DVD/ISO (a one-time boot); Power On System; if it is on, Power Off System first (on the R640 a warm reboot into virtual media hung after the boot menu twice). Keep the console open, because the iDRAC reads the ISO through the browser and closing the console unmaps it. Afterwards: Virtual Media, Disconnect Virtual Media, Yes |
| Other remote console (another vendor's BMC, a KVM over IP, a provider's console) | Mount the ISO as a virtual CD or DVD and boot from it once, powering the machine off fully first if it is on; unmount it afterwards |
| Physical media | Write the ISO to a USB stick (the command is shown), boot from it once, powering the machine off fully first if it is on; remove it afterwards and wipe it, because it holds the ISO's keys |

The Deployer then waits, as v2 does today, until the machine appears on the
tailnet. A discovery ISO built without network settings relies on DHCP,
which many data centers don't offer (the lab's R640 got no lease on any
port), so its steps say that if the machine never joins, the operator
cancels and runs discovery again with the public port's static settings. The operator can also tell it that the machine has booted, which
makes it check right away. If the machine doesn't appear, the Deployer
shows what to check (console output, the public NIC's cable, the gateway,
the Tailscale key) rather than giving up.

**The Deployer doesn't drive the iDRAC.** Redfish can mount virtual media
only from a URL that the iDRAC fetches itself, so the iDRAC would have to
open a connection back to the workstation. On the lab's R640 (iDRAC9
7.00.00.181), reached over the provider's VPN, the iDRAC never connected
back. Its insert request hung and then timed out, and the iDRAC's own
`ping` diagnostic couldn't run. That is how such VPNs usually work: they
carry connections from clients, not to them. The Virtual Console's Virtual
Media works the other way round, with the browser pushing the ISO to the
iDRAC over the connection the browser opened, so that is what the
instructions use. Serving the ISO from a machine on the iDRAC's own network
would make Redfish mounting possible, and may be added later. The Deployer
also never reads hardware inventory from iDRAC ("Hardware discovery").

## Proxmox host ISO

### First host (new cluster)

The Deployer collects:

- cluster settings: the content of v2's `cluster.conf` (name, internal
  domain, address policy, guest defaults, DNS, administrator SSH keys,
  Tailscale tag and tailnet name, notification recipients; the Postmark
  token is entered later in Dashboard Settings, never in an ISO). The
  address policy is the cluster's **address plan**: the private subnet
  (`PRIVATE_SUBNET_CIDR`, any IPv4 network from /16 to /24), the most hosts
  it will ever have (`MAX_MOX_HOSTS`, 1 to 32), and the address ranges for
  hosts, replication, HAProxy, production guests and staging guests, which
  may be as large as the subnet allows. The plan is **fixed once the
  cluster exists**: hosts, guests, and proxies already hold addresses from
  it, so bmacd refuses any change to those keys and the Settings page
  shows them read-only. Choose it with room to grow;
- the cluster's **time zone** (`PROXMOX_TIMEZONE`, an IANA name such as
  `America/Denver`), prefilled with the settings file's value or, without
  one, the workstation's zone. Every host is installed in it; joining hosts
  take it from the join bundle and aren't asked. Schedules, the
  Dashboard's schedule times and backup retention's days follow it
  (`BACKUP.md`, "Calendar");
- host settings: the content of v2's `moxN.conf`: first the slot, the VM
  counts, and optionally the iDRAC address (used only to link to its web
  interface in the instructions);
- the hardware inventory, normally from a discovery session (above);
- the host's network, chosen from that inventory: the **public port** from
  the list of discovered ports (each shown with its name, MAC, driver, and
  link), with its static **address, prefix, and gateway**, and the
  **private VLAN port**, which must be a different port, with its
  **address and prefix**. The address is prefilled with the slot's default
  from the cluster's plan (`MOX_IP_START` plus the slot) and may be any
  other free address in `MOX_IP_START`–`MOX_IP_END`; the prefix defaults
  to the private subnet's and may be shorter, so the network still holds
  the whole private subnet. An
  optional **private VLAN gateway** (prefilled with
  `DATACENTER_PRIVATE_VLAN_GATEWAY`) is recorded for the host, and a box,
  **off by default**, routes to the internet through it: tick it only if
  that gateway gives internet access. Host setup then adds a second default
  route through it with a higher metric than the public gateway's, so the
  public port stays the preferred way out. The public port
  and its address come prefilled with what the discovery session used; an
  address that came from DHCP is flagged for checking, because a host keeps
  a static address and it must be the one the provider assigned. The
  private port is prefilled when exactly one other port has a link. A chosen
  port that had no link during discovery gets a warning;
- the pool layout and the encryption choice (`STORAGE.md`), picked from that
  inventory;
- the Proxmox root password, stored in the ISO only as a hash;
- a fresh single-use Tailscale auth key tagged `tag:bmac-proxmox-host` (the
  Deployer names the tag next to the field, and refuses the host if it
  appears on the tailnet without that tag; the main README's "Tailscale
  setup" lists every tag and rule).

It generates:

- the cluster UUID;
- the host's HTTPS key and self-signed certificate (`SECURITY.md`);
- the host's Ed25519 SSH host key, as v2 does, so the workstation's SSH
  access can be pinned without trusting on first use;
- the installer answer file and first-boot script.

It builds the ISO with the signed `bmacd` packages as their own file on it,
`/bmac/packages.tar`, whose SHA-256 the first-boot script carries: the
installer takes a first-boot file of at most 1 MiB, too small for the
packages. It then shows a **host card**: the Dashboard URL, the certificate's SHA-256
fingerprint, the host name, and the cluster name. The card can be saved as a
file or printed, and the Deployer tells the operator to keep it somewhere
other than the machine being installed.

### Additional hosts

1. In the Dashboard, the operator chooses Hosts, then Add host, picks the slot,
   and enters the VM counts and the optional iDRAC address. The network
   ports and addresses aren't known yet; they are chosen in the Deployer
   from the discovered hardware.
2. `bmacd` reserves the slot in the registry and issues a **join bundle**
   file. It contains the cluster UUID and name, a copy of the cluster settings
   (no secrets), the slot reservation, a single-use join token, the addresses
   private VLAN addresses, and HTTPS certificate fingerprints of the current
   hosts, the Proxmox
   cluster CA fingerprint, and a setup SSH public key generated for this
   join. The bundle was downloaded from a fingerprint-verified, logged-in
   Dashboard, so the Deployer can show those fingerprints in its approval
   instructions (see "Handoff"). pmxcfs stores only the token's hash and
   its expiry (default 7 days; the operator can shorten it).
3. The operator downloads the bundle and loads it into the Deployer, which
   runs a discovery session, collects the network ports and public address
   (as for the first host), the pool layout, encryption choice, root
   password, and Tailscale key, generates the host's HTTPS certificate,
   and builds the ISO. It shows the host card, as for the first host.
4. The Dashboard lists outstanding bundles and can revoke them.

### Installation and first boot

After the destructive confirmation, the target is booted from the ISO by the
operator's chosen method ("Booting the target"). The answer file selects the boot vdev's disks by serial or WWN through the
installer's disk filters, never by device name. The Proxmox automated
installer erases those disks and installs onto them (`STORAGE.md` covers which
layouts the installer can create). Because the boot override is one-time, the
installer's final reboot starts the installed system. The instructions
tell the operator to remove the media once the installed system's login
prompt appears, as v2's do; first boot, which comes before that prompt,
reads the packages from it. The Deployer waits for the host to appear on
the tailnet, then does the handoff.

The first-boot script, running before the network is up:

1. installs the public-port guard (v2's `app-ha-host-guard`);
2. installs the administrator SSH keys, plus the join's setup key on joining
   hosts;
3. writes the HTTPS key to `/etc/bmac/tls/` (root only) and removes it from
   the installed copy of the first-boot data;
4. writes either the new-cluster seed (first host) or the join bundle (other
   hosts) to `/var/lib/bmac/bootstrap/`;
5. sets up joining Tailscale with the single-use key once the network is up;
6. finds the install media whose `/bmac/packages.tar` has the digest it
   carries, and installs the `bmacd` packages from it. Without the media it
   says so on the console and stops here, with SSH and Tailscale already
   set up, so the packages can be installed by hand;
7. starts `bmacd` in setup mode.

### Handoff

The Deployer's last step for a host. It touches the new host in exactly two
ways: a TLS handshake to check the certificate, and SSH with the operator's
own key. It never calls `bmacd`'s API and never logs in to it.

1. **Detect the host.** The Deployer watches the workstation's own
   `tailscale status` until the host's name appears, and reads its Tailscale
   address from there. The operator can also say the machine has booted,
   which makes it look again at once.
2. **Check the certificate.** The Deployer opens a TLS connection to the
   Dashboard URL (`https://<host>.<tailnet>.ts.net:8443`), compares the
   certificate's SHA-256 fingerprint with the one it generated, and closes
   the connection without sending a request (`SECURITY.md`). A refused or
   timed-out connection means `bmacd` isn't up yet. The Deployer keeps
   trying and shows that it is waiting. A **mismatch** is a hard stop: the
   Deployer shows a warning, explains the possible causes (the wrong machine
   answering at that name, or interception), and does not show the Dashboard
   link.
3. **Set up SSH from this workstation** to the host, if the operator agrees
   (on by default), pinned to the SSH host key the Deployer generated for
   this ISO (see "Workstation SSH"). SSH problems don't block the Dashboard
   link; certificate problems do.
4. **Hand over to the Dashboard.** Only after a matching certificate, the
   Deployer shows:
   - an **Open Dashboard** button, which opens the URL in the operator's
     default browser;
   - the fingerprint, with the instruction to compare it in the browser's
     certificate warning **before logging in**, even though the Deployer's
     check passed. The Deployer's connection and the browser's are separate,
     and one can be intercepted while the other isn't (`SECURITY.md`);
   - what happens next. **First host:** log in as `root@pam`; the setup job
     is waiting for input. **Other hosts:** open the Dashboard of an existing
     host (URL and fingerprint from the join bundle, checked by the Deployer
     the same way), where the new host shows as pending within a minute or
     so; approve it, and the setup job starts and asks for input there. The
     new host's own link is shown as well.
   - for LUKS hosts, a reminder that setup will ask for the passphrase at the
     host's console, and the iDRAC address if one was given.
5. **Finish.** The Deployer shreds the ISO (see "ISO lifecycle"). It does
   nothing more for this host: the rest of the setup happens in the Web
   Dashboard.

## Workstation SSH

SSH access from the administrator's workstation to hosts and guests is
configured on the workstation, so it belongs to the Deployer. The Web
Dashboard can't change anything on the workstation. This keeps v2's behavior
from `add_proxmox_host.sh`, `setup_jump_ssh_access.sh`, `add_prod_vm.sh`, and
`add_staging_vm.sh`. The Deployer's **Workstation SSH** page lists every
entry it manages and can set up, repair, or remove each one. The list is
read from the workstation's own files without connecting anywhere, so the
problems it shows are the ones those files reveal: a key that isn't pinned,
or a guest jumping through a host that has no entry. Setting a host or
guest up again repairs its entry, and checks the cluster for the rest: a
changed key is a hard stop, and hosts that have left the cluster are
reported with the guest entries that jump through them. Removing an entry
takes its block and its pin out under the same lock, after the same backup.

The Deployer reaches the cluster only over SSH, as root with the operator's
own key, through a host it has already pinned. There it runs read-only
`bmac` CLI commands that read pmxcfs and Proxmox directly and don't need
`bmacd` running. It never calls `bmacd`'s API.

### Managed entries

Every entry is written the same way, as v2 does:

- a managed block between `BEGIN`/`END` markers, placed at the top of
  `~/.ssh/config` so it takes precedence over older stanzas for the same
  names without deleting them, and ending with `Host *` so the user's
  following top-level options stay global;
- the pinned host key in `~/.ssh/known_hosts` under the entry's
  `HostKeyAlias`, after removing older pins for those names, so there is never
  a trust-on-first-use prompt;
- `User root`, `Port 22`, `StrictHostKeyChecking yes`, and no password or
  keyboard-interactive login;
- written under a lock, after backing up both files. Symlinked files are
  refused;
- then proved: `ssh -G` must resolve each name to the managed settings,
  catching a `Match` or `Include` that overrides them, and `ssh <name> true`
  must connect. On failure both files are restored and the reason is shown.
  A host key mismatch is a hard stop.

If the operator declines, the Deployer shows the block and the `known_hosts`
line to add by hand.

Before building an ISO, the Deployer checks that one of the workstation's
SSH public keys (in `~/.ssh/` or the SSH agent) is among the cluster's
administrator keys, and warns if not.

### Hosts

- `HostName` is the host's Tailscale address, with `ProxyJump none`,
  `HostKeyAlgorithms ssh-ed25519`, and `UpdateHostKeys no`.
- **At handoff**, the pin is the SSH host key the Deployer generated for the
  ISO.
- **Hosts this workstation didn't install** (a second workstation, or a
  reinstalled Deployer) need their pin from somewhere trusted. For the first
  one, the Deployer reads the host's key fingerprint with `ssh-keyscan` and
  asks the operator to confirm it against the SSH fingerprints listed in the
  Dashboard's Settings, which was reached over a fingerprint-verified
  connection. After that, the other hosts' keys are read over the pinned SSH
  connection from Proxmox's own cluster-wide record of node host keys.
- Reinstalling a slot replaces its entry, because every ISO gets a new SSH
  host key. Setting up a host reports entries for hosts no longer in the
  cluster's node list, which can then be removed.

### Guests (jump SSH)

As v2's `setup_jump_ssh_access.sh` does:

1. **Choose a guest.** The Deployer lists registered production and staging
   guests, with VMID, private address, the node running each, and status,
   using `bmac guests --json` over SSH. The guest must be running.
2. **Choose a jump host** among the online hosts that have a working managed
   entry on this workstation. The default is the host running the guest. The
   Deployer proves `ssh <jump host> true` first.
3. **Attest the guest's SSH host key.** `bmac guest-host-key <guest> --json`
   reads `/etc/ssh/ssh_host_ed25519_key.pub` from inside the guest through
   the QEMU guest agent on the node running it (Proxmox's
   `agent/exec` and `agent/exec-status` API through `pvesh`, which reaches
   that node itself), and checks that it is a valid Ed25519 key. The Deployer shows its fingerprint. The key comes from
   the hypervisor, not over the network, so it doesn't rely on first use.
4. **Write the entry**: `HostName` is the guest's private address,
   `ProxyJump <jump host>`, and `HostKeyAlias <alias>` pinned to the attested
   key. The alias defaults to the guest's name (`prodN`, `stageNprodN`). A
   different alias gets a warning that tools expecting `ssh <guest name>`
   won't use it.
5. **Prove** `ssh <alias> true`. A failure usually means this workstation's
   key isn't in the guest's `/root/.ssh/authorized_keys`, and the message
   says so.

If the entry is already correct and working, nothing is changed. When the
guest's jump host leaves the cluster, the page shows the entry as needing
repair once the host's own entry is removed; setting the guest up again
through another host repairs it. When the guest's host key changes (for
example after rebuilding the guest), setting it up again pins the newly
attested key. A removed staging guest's entry stays until it is removed
from the page.

**After creating a guest.** VM creation now runs in `bmacd`, so it can no
longer write the workstation's SSH settings as v2's `add_prod_vm.sh` and
`add_staging_vm.sh` did. When a guest-creation job finishes, its next step in
the Dashboard tells the operator to set up SSH to the new guest in the
Deployer's Workstation SSH page.

**Workstations without the Deployer.** The Deployer runs only on amd64
Linux. On other workstations, such as a Mac, the Dashboard's guest page
shows the `ssh_config` block and `known_hosts` line, using the attested host
key, for the operator to add by hand. It can't write them itself.

### Host setup job

The post-install work of v2's `add_proxmox_host.sh` (its steps 5 to 9) becomes
the host setup job. The work runs in `bmacd`, on the new host and, for
joins, on an existing host. The operator follows and answers it in the Web
Dashboard. Its job view carries over v2's add-host experience: the phases
with their state, the live output, every operator gate and question, and
step-by-step console guidance for the LUKS steps, including the host's iDRAC
address when one is configured. If the operator's browser or workstation goes
away, the job keeps running or waits at its current gate. The operator
resumes from any browser by logging in again.

- On the **first host**, `bmacd` starts in setup mode, which offers only the
  setup job and its log.
- On **other hosts**, the new host's `bmacd` connects to an existing host
  named in the bundle on port 8444, checks the Proxmox cluster CA
  fingerprint, and presents the join token (`SECURITY.md`, "Joins and
  enrollment"). It retries every 30 seconds until a host takes the request,
  and its own setup page then says where to approve it. The request is
  pending on the Hosts page under Adding hosts, with the address it came
  from and the new host's SSH and HTTPS fingerprints. Approving starts the
  `hosts.join` job on that host, after one more confirmation that shows the
  fingerprints again, to compare with the Deployer's host card. A QDevice
  must be removed first, and every member must be online. The job:
  - reaches the new host over SSH with the join's setup key, pinned to the
    SSH host key in the request;
  - reads the new host's bootstrap file, checks that it carries this
    cluster's UUID and the token issued for this slot, and stores the host's
    facts in the cluster settings;
  - runs `bmacd native hosts.setup --driven-by <host>` on the new host and
    relays its phases, output, and questions into the job. The job is
    followed and answered from that host's Dashboard. The new host's setup
    keeps its own checkpoints, so when the connection drops (the boot
    drills restart the host), the job reconnects once the host answers and
    runs the setup again, and it carries on where it stopped;
  - once the setup has finished, drops the bundle, its token, the request,
    and the setup key.

  The new host's own Dashboard shows only "being set up from `<host>`"
  meanwhile; a joining host's setup can't be started locally.

The job's phases, in order:

1. **Verify identity.** The host's hardware and disks match the layout the ISO
   was built for.
2. **Storage.** Encrypt the boot vdev in place if LUKS was chosen, then create
   the other vdevs (`STORAGE.md`). The LUKS passphrase is only ever typed at
   the host's console (iDRAC's virtual console, another remote console, or a
   keyboard): the job shows a manual-action request telling the operator to
   run `bmac luks-enter` at the console. That command refuses to run over
   SSH, asks for the passphrase twice, and stores it in a root-only tmpfs
   file, `/run/bmac/luks-passphrase`, that the job deletes, and verifies deleted,
   before the storage phase completes, as v2 does with its temporary key
   file.
3. **Boot drills** (optional). These reboot the host. The job checkpoints
   before each reboot. When the job runs on the host being rebooted, its
   Dashboard goes away during the reboot. The page says so in advance, and
   on a LUKS host tells the operator to type the passphrase at the console.
   The page reconnects when the host is back. Sessions are stored on the
   host, so the token still in the page's memory keeps working unless it
   expired meanwhile.
4. **Private network.** Resolve NICs by MAC, wait for carrier with the
   link-wait rules ("Hardware discovery"), require 10 Gbps, create
   `vmbr-private`, and stop at the operator gate `PRIVATE VLAN VERIFIED`.
5. **Cluster.** `pvecm create` on the first host. A joining host joins
   through the approving host's private address. It first checks that the
   certificate served on port 8006 there has the fingerprint the approving
   host passed down. Only then does it ask for that host's `root@pam`
   password, which is handed to `pvecm add` once on stdin and kept nowhere;
   the job's record shows it redacted. Then it waits for quorum and
   restarts `bmacd` with the cluster's certificates. Every member's `bmacd`
   pins the hosts in the cluster settings to their private addresses in
   `/etc/hosts`, so the existing hosts reach the new one by name.
6. **Retire the setup key.** Joining merged root's `authorized_keys` into
   the cluster's, so the setup key is removed from both at once. After
   this the approving host reaches the new one with normal cluster SSH.
7. **Routing and orchestration.** Guest egress (SNAT and VRRP), `bmac-hook`,
   registry initialization on the first host, deferred-cleanup and
   route-sync timers, this host's HAProxy LXC, and convergence of the
   registry's routes.
8. **Back up LUKS headers.** Each LUKS member's header is stored on the host
   under `/var/lib/bmac/luks-headers/`, offered as a download in the
   Dashboard, and pulled by every enrolled backup appliance (`BACKUP.md`;
   the cluster never pushes to an appliance).
9. **Verify the end state**, as v2's step 9 does, and leave setup mode.
   Once the job's end is recorded, `bmacd` restarts itself, since what it
   runs is chosen at startup: as a cluster member it links with the other
   hosts, issues join bundles, and keeps the hosts pinned in `/etc/hosts`.

### Things that used to live on the workstation

| v2 location | v3 location |
| --- | --- |
| `config/cluster.conf`, `config/moxN.conf` | `/etc/pve/priv/bmac/config/`, edited in the Dashboard |
| `config/secrets.env` | Root password: hash in ISO only. Guest root password hashes: `/etc/pve/priv/bmac/secrets/`. LUKS passphrase: typed at the console, never stored. iDRAC credentials: never given to the Deployer |
| `artifacts/hosts/source-iso/` | Deployer's source ISO cache |
| `artifacts/hosts/<moxN>/*.iso` and `generated/` | Deployer temporary build directory, deleted after the build |
| `artifacts/hosts/<moxN>/ssh/` (setup key) | Generated by the cluster per join, removed after joining |
| `artifacts/hosts/<moxN>/state/`, `logs/` | The host's job records and logs |
| `artifacts/hosts/<moxN>/luks-headers/` | The host, the Dashboard download, and the backup appliances |
| `artifacts/hosts/used-tailscale-auth-key-sha256` | Digest list in `/etc/pve/priv/bmac/config/`; the Deployer checks it through the join bundle, which carries the current digests |
| `artifacts/prod/`, `artifacts/staging/` | `/var/lib/bmac/work/` on the executing host |
| Host and guest entries in `~/.ssh/config` and `known_hosts` | Still on the workstation, written by the Deployer ("Workstation SSH"), as v2's scripts do |

The first host's Tailscale key digest is recorded when the cluster is created.

## Backup appliance ISO

1. In the Dashboard, the operator chooses Backups, then Add appliance. `bmacd`
   issues an **appliance enrollment bundle**: the cluster UUID and name, the
   Proxmox cluster CA fingerprint, the addresses of the hosts, and a
   single-use enrollment token (hash and expiry in pmxcfs).
2. The Deployer loads the bundle and collects:
   - the appliance name and network settings;
   - the OS disk or disks, and the backup pool layout (`STORAGE.md`);
   - an appliance administrator account and password, stored only as a hash;
   - a fresh single-use Tailscale auth key tagged
     `tag:bmac-backup-appliance`, which the Deployer names next to the
     field and checks once the appliance appears on the tailnet;
   - optionally the appliance's own Postmark token, used for its watchdog
     alerts (`MONITORING.md`).
3. It generates the appliance's HTTPS key and self-signed certificate, builds
   an Ubuntu Server autoinstall ISO from the pinned source ISO, embeds the
   signed `bmac-backupd` package, and shows an appliance card with the URL and
   fingerprint.

Installation:

- Autoinstall installs Ubuntu onto the OS disk or disks.
- It stops at an interactive step where the operator types the new LUKS
  passphrase at the console. The ISO never contains it.
- It creates the LUKS devices and the ZFS pool from the layout, after checking
  every disk's identity and refusing to continue on any mismatch.

On first boot, `bmac-backupd` joins Tailscale, connects to the cluster, checks
the CA fingerprint, and presents the enrollment token together with its
client certificate and the address it suggests for itself. The Dashboard
shows the appliance as pending. An administrator approves it and sets its
address (a private or public IP or a DNS name, never a Tailscale one),
which records the appliance's certificate fingerprint in the cluster
(`BACKUP.md`). Until then no backup data moves; after it, the hosts
connect to the appliance at that address, and backup data never crosses
Tailscale.

The Deployer does the same handoff as for hosts: it checks the appliance's
certificate and withholds the link on a mismatch. It then shows the link to
the appliance's UI and the instruction to approve the appliance in the
cluster Dashboard, and always tells the operator to verify the fingerprint
in the browser.

**Installing from a package.** On an Ubuntu Server machine installed by hand,
`apt install ./bmac-backupd_<version>.deb`, then `bmac-backupd init`. The
interactive command does the same work as the ISO path: disk selection with
destructive confirmation, LUKS, pool, Tailscale, certificate, and
enrollment, using the same bundle file.

## ISO lifecycle

Every ISO the Deployer builds holds secrets made for one machine:

| ISO | Secrets in it |
| --- | --- |
| Discovery | the Tailscale auth key, the session's SSH host private key |
| Proxmox host | the Tailscale auth key, the HTTPS and SSH host private keys, the root password hash, the join token (joining hosts) |
| Backup appliance | the Tailscale auth key, the appliance's private keys, the enrollment token |

The Deployer:

- keeps a list of the ISOs it built (file, kind, target, build time) in its
  settings, and whether it still has the file;
- **shreds** each ISO by itself once its machine is online on the tailnet.
  By then the machine reads nothing more from the media: the discovery OS
  runs from RAM, and an installed host or appliance has rebooted from its
  own disks. This happens however the session ends after that point,
  including a failed certificate check or a cancel. A session that stops
  earlier keeps the ISO, because the machine may still be booting from it,
  and says so; Settings lists it with a Delete button, which shreds it too;
- shreds its scratch build files (the answer file, first-boot script, and
  discovery overlay) when a build finishes or fails, and a half-built ISO;
- after each session, tells the operator to disconnect the virtual media,
  and to wipe any USB stick the ISO was written to.

Shredding overwrites the file with zeros, flushes it to the disk, and then
deletes it. On a copy-on-write filesystem (btrfs, ZFS, APFS) or an SSD the
old blocks can outlive the overwrite, so this narrows the exposure rather
than proving erasure; full-disk encryption on the workstation covers the
rest.

The discovery ISO's Tailscale key should be ephemeral and single-use, or
short-lived; a reusable key in a leftover copy would let its holder join
the tailnet with the discovery tag until the key expires.

The guest installer ISO isn't built by the Deployer. `prod.add` builds it on
a Proxmox host (as v2's `add_prod_vm.sh` does) with the guest's root
password hash and the administrators' public keys, deletes it when the
install finishes, and asks whether to keep the verified source ISO cached
on the host for the next guest.

The Deployer can't know whether a token was used, because it never calls
`bmacd`. The Dashboard lists every join and enrollment bundle it issued as
`unused`, `used`, `expired`, or `revoked`. Tokens expire, and they can be
revoked there. A token is consumed
on first use, so a copied ISO cannot join a second machine. The Deployer
cannot delete copies it doesn't know about; expiry and single use are the
real protection, and the documentation says so.

## Upgrades

**Release artifacts.** Each release produces signed `.deb` packages for
`bmacd`, `bmac-backupd`, and the Deployer, plus a signed manifest of hashes
and the minimum compatible record schema versions. Signing uses an Ed25519
key. Its public key is compiled into `bmacd` and `bmac-backupd`.

**Development builds** are signed with a separate development key, kept on
the developers' machines. Only builds made with the `dev-signing` feature
trust it (the lab's builds have it); release builds trust the release key
alone, so a production cluster refuses a development bundle. Upgrades are
tested in the lab by uploading development bundles of successive builds, so
nothing untested has to be published.

**Cluster update job.** Settings, then Updates:

1. Take the **release bundle**: a single file holding the signed manifest
   and the `.deb` packages it lists. The administrator either uploads it from
   their browser or gives a URL for `bmacd` to fetch. A bare `.deb` is not
   accepted, because only the manifest carries the signature, the hashes,
   and the schema versions. The host that receives the bundle keeps it in
   `/var/lib/bmac/updates/`, and the other hosts copy it from that host over
   port 8444. pmxcfs is not used, since its files are limited to about 1 MiB.
2. Verify the manifest's signature and every package's hash before anything
   is installed.
3. Check that every host is online and the cluster is quorate.
4. Upgrade one host at a time. Each host installs the packages and restarts
   its own services. The job resumes from its checkpoint and checks the
   host's health before moving to the next host.
5. Once every host runs the new version, enable writes of any new record
   schema versions (see "Upgrades" in `ARCHITECTURE.md`).

The previous packages stay cached on each host for rollback. A failed health
check stops the rollout and offers to roll the upgraded hosts back.

**Appliances** upgrade from their own UI or with `apt`, independently of the
cluster. The cluster-to-appliance protocol is versioned and both sides
announce what they support.

## Removing a host

v2's graceful and forced removal semantics are kept, including the typed
confirmations `REMOVE moxN` and `moxN IS PERMANENTLY DISCONNECTED`. The
removed host's logs stay readable on the remaining hosts until normal
retention expires (`MONITORING.md`). Its slot can be reused only through a new
join bundle.

The Deployer's Workstation SSH page then shows the host's entry, and any
guest entries that jump through it, as needing removal or repair.
