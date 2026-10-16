<!--
Copyright (c) 2026 BEENTHERE VENTURES, INC.
SPDX-License-Identifier: GPL-3.0-only
-->

# BMAC v3

BMAC v3 is a re-architecture of BMAC v2. v2 keeps its operational code and
much of its state on the administrator's workstation. v3 moves both onto the
cluster and adds backup, restore, monitoring, and alerting.

- **`bmacd`** runs on every Proxmox host, installed by BMAC's custom Proxmox
  ISO. It runs the workflows, coordinates with the other hosts through
  Proxmox's cluster filesystem, and serves the **Web Dashboard** over HTTPS
  with a certificate generated for that host.
- **BMAC Deployer** is the desktop app (the v2 Dashboard, renamed and
  repurposed). It discovers a machine's hardware, builds the Proxmox host and
  backup appliance installer ISOs, and boots machines from them (through
  iDRAC or by hand). When a new host joins the tailnet, it checks the host's
  certificate, sets up SSH access from the workstation, and hands over to the
  host's Web Dashboard, where host setup continues. It also sets up SSH from
  the workstation to hosts, and jump SSH through a host to guests, as v2
  does, and deploys the hello app over that access. It never talks to
  `bmacd`'s API.
- **Backup appliances** are separate Ubuntu Server machines that pull
  encrypted ZFS backups from the cluster. The cluster holds no credential to
  them.

v3 is not compatible with v2. Everything for v3 lives in this directory; the
rest of the repository is v2, kept as reference.

**Status:** in development, following `design/IMPLEMENTATION_PLAN.md`.
Milestones M0 to M10 are built and proven in the lab, through backups,
restores, whole-cluster recovery, and signed rolling updates with
rollback; M11 (native ports of the legacy scripts) and M12 (hardware runs,
security review, and documentation for release) remain.

**Development platform:** amd64 Linux with KVM and nested virtualization.
The lab runs Proxmox hosts as nested KVM guests, the Deployer builds ISOs
with Linux-only tools, and `bmacd` uses Linux-only interfaces, so v3 is
developed and tested on Linux only; macOS is not supported for development.
Operators can use the Web Dashboard from any workstation's browser.

## Design documents

| Document | Covers |
| --- | --- |
| [`design/ARCHITECTURE.md`](design/ARCHITECTURE.md) | Start here: components, decisions, where state lives, coordination, the Web Dashboard, repository layout, testing |
| [`design/DEPLOYMENT.md`](design/DEPLOYMENT.md) | The Deployer, hardware discovery, booting the target, ISOs, adding hosts, host setup, workstation SSH, upgrades |
| [`design/SECURITY.md`](design/SECURITY.md) | Threat model, certificates, login, roles, secrets |
| [`design/STORAGE.md`](design/STORAGE.md) | Pool layouts, LUKS, raidz capacity math, VM disk block size, storage workflows |
| [`design/BACKUP.md`](design/BACKUP.md) | Backup, restore, retention, appliances |
| [`design/MONITORING.md`](design/MONITORING.md) | Health checks, alerts, notifications, logs |
| [`design/WORKFLOW_MIGRATION.md`](design/WORKFLOW_MIGRATION.md) | How v2's scripts run under `bmacd`, and the order they are ported to Rust |

`design/BMAC_MAJOR REWRITE_SPEC.md` is the earlier, longer specification. It
is kept as reference; where it disagrees with the documents above, they win.

## Tailscale setup

Every BMAC machine is reached over your Tailscale network (tailnet): the
Proxmox hosts, the QDevice, the backup appliances, and, while it runs, the
Deployer's temporary discovery system. Guests never join the tailnet; SSH to
a guest jumps through its host. Set up the tailnet once, before the first
host:

1. **Your workstation.** Install Tailscale on the workstation that runs the
   Deployer, log in, and check that it is listed in the Tailscale admin
   console under **Machines**.
2. **The admin group.** In the admin console, under **Access controls**, add
   a group named `bmac-admin` and put in it every user who administers
   BMAC (yourself included).
3. **The tags.** In the same place, add these tags, each owned by
   `group:bmac-admin`:

   | Tag | Given to |
   | --- | --- |
   | `tag:bmac-proxmox-host` | Each Proxmox host |
   | `tag:bmac-proxmox-qdevice` | The QDevice |
   | `tag:bmac-backup-appliance` | Each backup appliance |
   | `tag:bmac-discovery` | The Deployer's temporary discovery system |

4. **The access rules.** Under **Access controls**, add these rules to the
   policy's `grants`:

```jsonc
// Admin SSH: hosts (and guests through them), the QDevice, and the discovery system
{
	"src": ["group:bmac-admin"],
	"dst": ["tag:bmac-proxmox-host", "tag:bmac-proxmox-qdevice", "tag:bmac-discovery"],
	"ip":  ["tcp:22"],
}

// Proxmox web UI
{
	"src": ["group:bmac-admin"],
	"dst": ["tag:bmac-proxmox-host"],
	"ip":  ["tcp:8006"],
}

// BMAC Web Dashboard on the hosts, and the backup appliances' own UI
{
	"src": ["group:bmac-admin"],
	"dst": ["tag:bmac-proxmox-host", "tag:bmac-backup-appliance"],
	"ip":  ["tcp:8443"],
}

// QDevice: corosync-qnetd quorum traffic, and the hosts managing it over SSH
{
	"src": ["tag:bmac-proxmox-host"],
	"dst": ["tag:bmac-proxmox-qdevice"],
	"ip":  ["tcp:5403", "tcp:22"],
}

// Corosync/Kronosnet host-to-host links
{
	"src": ["tag:bmac-proxmox-host"],
	"dst": ["tag:bmac-proxmox-host"],
	"ip":  ["udp:5405-5412"],
}

// bmacd host to host: live streams, job relays, and joins (also on the private network)
{
	"src": ["tag:bmac-proxmox-host"],
	"dst": ["tag:bmac-proxmox-host"],
	"ip":  ["tcp:8444"],
}

// Hosts driving a new host's setup over SSH when its join is approved
{
	"src": ["tag:bmac-proxmox-host"],
	"dst": ["tag:bmac-proxmox-host"],
	"ip":  ["tcp:22"],
}

// Backup appliances: enrollment (8444), and pulling backups and heartbeats (8445)
{
	"src": ["tag:bmac-backup-appliance"],
	"dst": ["tag:bmac-proxmox-host"],
	"ip":  ["tcp:8444", "tcp:8445"],
}

// The cluster to backup appliances: status, early-pull requests, and restores
{
	"src": ["tag:bmac-proxmox-host"],
	"dst": ["tag:bmac-backup-appliance"],
	"ip":  ["tcp:8445"],
}

// Backup appliances repairing each other
{
	"src": ["tag:bmac-backup-appliance"],
	"dst": ["tag:bmac-backup-appliance"],
	"ip":  ["tcp:8445"],
}
```

5. **Auth keys.** The Deployer asks for a Tailscale auth key whenever it
   builds an ISO, and embeds it in that ISO. Make each one under
   **Settings**, **Keys**, **Generate auth key**: name it after the machine,
   turn **Reusable** off (the Deployer refuses a key it has used before),
   and turn **Tags** on with the tag below. Any expiry works for a host or
   an appliance, since a key stops expiring once it is used.

   | ISO | Ephemeral | Tag |
   | --- | --- | --- |
   | Proxmox host | Off | `tag:bmac-proxmox-host` |
   | Backup appliance | Off | `tag:bmac-backup-appliance` |
   | Discovery | **On**, so the node leaves the tailnet once it powers off | `tag:bmac-discovery` |

   The QDevice is an Ubuntu machine you set up yourself: join it to the
   tailnet with a key tagged `tag:bmac-proxmox-qdevice`, under the name in the
   cluster's QDevice setting. Add QDevice then asks you to authorize the
   cluster's QDevice access key for root SSH on it.

The lab uses these same tags, group, and rules on a Tailscale account of its
own (`lab/README.md`, "Tailscale for the lab").

## Choosing vdev types

A ZFS pool is made of top-level **vdevs**. Data is spread across all data
vdevs, so **losing any one data vdev loses the whole pool**. A pool is only
as safe as its weakest data vdev. v3 lets you choose the type of each vdev
and then its member disks (see [`design/STORAGE.md`](design/STORAGE.md)). The
Deployer and the Dashboard show the restrictions below next to each type
before you choose. Some choices can never be undone.

### Data vdev types

| Type | Survives | Usable space | Pros | Cons |
| --- | --- | --- | --- | --- |
| Single disk | No failures | 100% | All the capacity; can later become a mirror by attaching a disk; removable later (see below) | Any disk failure loses the pool. ZFS detects corruption but cannot repair it. Not allowed as the boot vdev of an encrypted host |
| Mirror (2-way) | 1 disk per vdev | 50% | Best random I/O; fastest, lowest-risk rebuilds (a straight copy); no space lost on small blocks; can be widened or narrowed; **vdevs can be removed later** (v2's disk decommissioning); grows two disks at a time | Half the raw capacity |
| Mirror (3-way) | 2 disks per vdev | 33% | As 2-way, and stays redundant during a rebuild | A third of the raw capacity |
| raidz1 | 1 disk per vdev | (N−1)/N nominal | More capacity than mirrors | No redundancy left while rebuilding, and rebuilds read every disk, which is risky with large spinning disks. Wastes space on small blocks (see below). Random I/O roughly that of one disk per vdev. **Rules out ever removing any vdev from the pool** |
| raidz2 | 2 disks per vdev | (N−2)/N nominal | Survives any two failures; the usual choice for large spinning disks | Same structural limits as raidz1; needs at least 4 disks, and at 4 disks gives the same space as two mirrors with slower I/O |
| raidz3 | 3 disks per vdev | (N−3)/N nominal | For wide vdevs of very large disks | Same limits; the most parity overhead |

dRAID is not supported in v3.0.

**What raidz rules out, permanently.** ZFS cannot remove a top-level vdev
from a pool that contains any raidz vdev. That includes vdevs that aren't
raidz themselves. Once a pool has a raidz vdev:

- v2's decommissioning workflow (evacuate a vdev, then pull its disks) is no
  longer possible for that pool;
- a raidz vdev can't be narrowed, given a different parity level, or turned
  into mirrors;
- the pool can grow, but never shrink. It grows by adding vdevs, by
  replacing every disk of a vdev with larger ones, or by adding disks to a
  raidz vdev one at a time (OpenZFS 2.3 or newer). Data written before such
  an expansion keeps its old ratio of data to parity.

On a pool of only single-disk and mirror vdevs, all of those stay possible.
Removal also needs every vdev to have the same sector alignment (ashift),
which BMAC guarantees. The boot vdev is never removed in any case.

**Space on raidz depends on block size.** VM disks use small blocks (16 KiB
by default). On raidz, each small block needs parity and padding, so a VM
disk can take up to about 20% more space than ZFS's free-space figures
suggest. At 16 KiB, every raidz1 layout stores data at two-thirds
efficiency: a 6-disk raidz1 holds no more VM data per raw byte than a 3-disk
one. Larger blocks (for example 64 KiB) avoid most of that. BMAC does this
math for you, shows the real figures, and suggests a block size when you
create each VM ([`design/STORAGE.md`](design/STORAGE.md), "Capacity
calculations").

### Helper vdevs

These classes don't hold ordinary data. With encryption enabled, every one of
them is encrypted too.

| Class | What it does | Pros | Cons and rules |
| --- | --- | --- | --- |
| Special | Holds metadata and, optionally, small blocks, on fast disks | Makes a pool of spinning disks feel much faster for metadata-heavy work | Losing it loses the pool, so give it at least the data vdevs' redundancy (a mirror; a 3-way mirror alongside raidz2). Removable only under the same rules as data vdevs |
| Log (SLOG) | Takes synchronous writes before they reach the pool | Speeds up guests that flush often (databases) | Helps only synchronous writes. Use disks with power-loss protection. Losing it loses at most the last few seconds of synchronous writes, and only if the host also crashes. Removable at any time |
| Cache (L2ARC) | A second-level read cache | Helps when the working set is larger than RAM | Uses some RAM to track its contents; rarely worth it on hosts with plenty of RAM. Removable at any time |
| Spare | Replaces a failed member automatically | Restores redundancy without waiting for someone to swap a disk | Must be at least as large as the largest member it could replace. A spare that replaces a boot-disk member has no boot partition until the failed disk is properly replaced (the Dashboard says so). Removable at any time |

### Suggestions

- **Proxmox hosts on NVMe or SSD:** mirrors, as in v2. Best performance, no
  small-block waste, and disk decommissioning keeps working.
- **Proxmox hosts with many large spinning disks:** raidz2 of 6 to 10 disks,
  with a mirrored special vdev on SSDs, and VM disks at the block size BMAC
  suggests for the layout (64 KiB, or 128 KiB on 8 disks) unless the
  workload is mostly small random writes. Accept that the pool can never
  be shrunk.
- **Encrypted hosts:** the boot vdev must be a two-disk mirror,
  because it is encrypted in place one disk at a time after installation.
- **Backup appliances:** raidz2 usually fits. Backups are mostly written in
  long streams, and capacity matters more than removing disks later. Backups
  keep each VM disk's block size, so the small-block math above applies on
  the appliance too, and the appliance's capacity figures include it.
- **Avoid single-disk data vdevs** for anything you can't afford to lose.
  BMAC allows them, with a warning you must acknowledge.

## Email alerts with Postmark (optional)

BMAC sends alert emails through [Postmark](https://postmarkapp.com). Without
it, everything else works and alerts appear only in the Web Dashboard. To
set it up:

1. **Create a Postmark account.** New accounts may be reviewed by Postmark
   before they can send to other people; until then you can send only to
   your own verified address. Ask for approval early so alerts reach
   everyone who needs them.
2. **Create a server for BMAC.** In Postmark, a "server" is not a machine:
   it is a named group of settings, templates, and API tokens inside your
   account. Create one just for BMAC (for example "BMAC production") with
   delivery type **Live**. A server's delivery type can't be changed later.
   One server can serve several BMAC clusters, but a separate server per
   cluster keeps their templates and statistics apart.
3. **Set up the sender.** Postmark sends only from a confirmed sender
   signature: either a single address you confirm by email, or a whole
   domain verified with the DKIM and Return-Path DNS records Postmark gives
   you. A verified domain delivers better. Use an address such as
   `bmac-alerts@example.com`.
4. **Copy the server's API token.** Open the server, then **API Tokens**, and
   copy a **Server API token**. Don't use the Account API token: it controls
   every server in your account, and BMAC never needs it.
5. **Enter it in BMAC.** In the Web Dashboard, go to Settings, then
   Notifications, and enter the server token, the sender address, and the
   recipients. This is the only place the token is entered; it is never put
   in an ISO. BMAC stores it as a cluster secret and never shows it again.
6. **Install the templates.** BMAC's emails are Postmark templates whose
   names start with `bmac-`. On the same page, **Check the Postmark server**
   lists them with whether each is on your server, and **Create N missing**
   creates the missing ones. BMAC never changes or deletes a template that
   already exists. If someone edited one in Postmark, it shows as differing:
   restore it there, or delete it and create it again here.
7. **Send a test.** **Send** under Test email proves the token, sender,
   templates, and recipients together.

The same page sets who gets what. With no routes, warnings and critical
alerts of every kind go to the recipients, with a message when each one
resolves. A route sends some categories (the first part of a condition,
such as `zfs` or `corosync`) at a minimum severity to other recipients.
**Repeat at most every** stops an alert that keeps clearing and returning
from being emailed each time, unless it gets worse. **Daily summary** sends
the open alerts once a day.

On the Alerts page, operators acknowledge alerts and add silences before
planned work: a silenced alert still shows, but isn't emailed until the
silence ends. If emails can't be delivered (a refused token, missing
templates, or a recipient Postmark has marked inactive), the Alerts page and
the Overview say so.

The backup appliance's watchdog emails are configured the same way on the
appliance itself, with their own token (its own Postmark server is
suggested), so they still arrive when the whole cluster is down.

For BMAC developers: `v3/dev/postmark_templates.py` manages the templates
from their definitions in `v3/notify/templates/`, using
`POSTMARK_SERVER_TOKEN` and `POSTMARK_SERVER_NAME` from the environment. Use
a separate server with delivery type **Sandbox** for development; Postmark
doesn't deliver its messages to real inboxes, though they count toward the
account's monthly volume. See `design/MONITORING.md`, "Email templates".
