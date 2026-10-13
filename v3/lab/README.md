<!--
Copyright (c) 2026 BEENTHERE VENTURES, INC.
SPDX-License-Identifier: GPL-3.0-only
-->

# The nested lab

A scripted test environment on one Linux machine: three Proxmox VE hosts,
one appliance, and one QDevice, all QEMU/KVM VMs with nested virtualization.
It can be rebuilt from nothing with one command, snapshotted, reset, and
driven by scenarios that print PASS or FAIL for each check.

## Requirements

- An x86-64 Linux machine with KVM and nested virtualization, about 30 GiB of
  free memory, and 150 GiB of free disk (disks are thin qcow2 files).
- `sudo apt install qemu-system-x86 qemu-utils ovmf xorriso`, plus the usual
  `openssl`, `openssh-client`, and `dpkg`.
- Python 3.11 or later (standard library only).

`lab/lab check` lists anything missing. Nothing in the lab needs root once
those packages are installed: there are no bridges or tap devices.

## Quick start

```bash
cd v3
lab/lab check
lab/lab create            # downloads, builds, installs; snapshots `blank` and `installed`
lab/lab status
lab/lab ssh mox1 pveversion
lab/lab scenario smoke
lab/lab reset installed --start
lab/lab destroy           # add --images to also delete the downloads
```

`lab/lab rebuild` destroys and recreates the lab. `lab/lab --help` lists
every command.

## What it builds

| VM | OS | Disks | Networks |
|---|---|---|---|
| mox1, mox2, mox3 | Proxmox VE 9.2 (unattended install, ZFS mirror on the NVMe pair) | 2 NVMe, 2 SATA, 2 SCSI | uplink, management, VLAN |
| appliance | Ubuntu 24.04 cloud image | virtio system disk, SCSI backup disk | uplink, management |
| qdevice | Ubuntu 24.04 cloud image | virtio system disk | uplink, management |

`lab.toml` defines all of it. Every disk gets a serial number and, on SATA
and SCSI, a WWN (an EUI-64 on NVMe), derived from the lab, VM, and disk names
so they stay the same across rebuilds and differ between disks. Linux ignores
the EUI-64 of QEMU's NVMe controller (`NVME_QUIRK_BOGUS_NID`) and reports a
WWN built from the vendor, serial, and model, so the lab's NVMe disks
exercise identification by serial.

**Tailscale names.** A lab VM that joins a real tailnet is named
`tailnet_prefix` plus its VM name (`bmaclab-mox1`), never the bare name.
`reserved_tailnet_names` lists names already in use on the tailnet by the v2
test cluster (`qdevice`, `mox1`, `mox2`, `mox3`), and the topology is
refused if a lab name would collide with one. Code that joins a lab VM to
Tailscale takes the name from `Topology.tailnet_name()`.

Each VM has three kinds of network:

- **uplink**: QEMU user networking. It gives the VM outbound internet access,
  and forwards workstation ports on 127.0.0.1 to the VM, ten per VM starting
  at `base_port`: SSH (22002 for mox1, 22012 for mox2, ...), the Proxmox web
  UI, and BMAC's ports 8443, 8444, and 8445. `lab/lab status` lists them.
- **management** (`100.64.0.0/24`): a shared segment standing in for the
  tailnet, between VMs only.
- **VLAN** (`10.213.0.0/24`, hosts only): a shared segment standing in for the
  10 Gbps private VLAN, using v2's example addresses.

The management and VLAN segments are QEMU multicast sockets on loopback. The
Proxmox hosts get their management and VLAN addresses from a first-boot
script baked into their install ISO, which also turns on the serial console.
The Ubuntu VMs get theirs from cloud-init.

## Driving it

- `lab/lab power VM on|off|shutdown|reset|cycle`: `off` pulls the plug;
  `shutdown` asks the guest.
- `lab/lab link VM mgmt|vlan down|up`, `lab/lab cut VM`, and
  `lab/lab restore VM`: hold links down (they stay down across reboots)
  and bring them back.
- `lab/lab carrier VM SECONDS`: after every boot or reset of VM, its VLAN
  carrier stays down for SECONDS, the way FiberState's VLAN comes up well
  after the host. A background supervisor, started by `lab/lab start`,
  watches for guest resets and raises the link on time.
- `lab/lab media VM ISO` and `lab/lab media VM`: insert and eject a CD;
  `lab/lab start VM --iso ISO` boots from one.
- `lab/lab console VM`: the serial console (Ctrl-] detaches). Scenarios use
  `bmaclab.console.Console` to wait for text and type, for installer screens
  and LUKS passphrases.
- `lab/lab vnc VM`: how to view the VM's screen.
- `lab/lab snapshot NAME` shuts the lab down and snapshots every disk and
  firmware store; `lab/lab reset NAME` returns to it.

## Scenarios

A scenario is a Python file in `scenarios/` with a `DESCRIPTION` and a
`run(s)` function. `s.lab` is the lab, `s.check(description, test)` records
PASS or FAIL and continues, and `s.require(...)` stops the scenario when it
fails. `lab/lab scenario NAME` exits 0 only when every check passed.

`smoke` builds the lab if there is none, then checks:
- every VM answers SSH;
- every host runs Proxmox VE 9 with its rpool mirrored and sees each disk
  with its serial and WWN;
- the management and VLAN segments connect the VMs;
- serial consoles show a login prompt;
- a host's VLAN carrier comes up 90 seconds after a reboot;
- a reset discards changes.

### Stages

A stage is a VM saved by a scenario once it reached a state that took long
to reach, so later runs of a scenario that builds on it start there
instead (`m6.staged`). The disks keep qcow2 snapshots named `stage-NAME`,
and the VM's folder keeps its firmware store with a copy of the
Deployer's working files and the run's made-up root password. The host
keeps its tailnet device. A restored stage gets the current bmacd package.
`BMAC_LAB_FRESH=1` builds the stage again, installing the VM anew drops its
stages, and `lab/lab rebuild` drops them with the disks.

How a stage is saved depends on the lab. QEMU can't save a running NVMe
controller ("non-migratable device"), so in the NVMe lab (`lab.toml`) the
VM is shut down for the save and boots when restored. `lab-scsi.toml` and
`lab-sata.toml` are the same topology, each with its own name, ports,
multicast group, MACs, and tailnet prefix (`bmacscsi-`, `bmacsata-`), and
`nvme_as = "scsi"` or `"sata"`, which attaches the NVMe disks on that bus
with the same serials. There a stage is saved live (QMP `snapshot-save`,
the memory in the first disk) and restored with `snapshot-load` before the
VM runs, so it resumes where it was in seconds, past its boot's VLAN
carrier delay; the scenario then sets the guest's clock. The Deployer's lab
build names machines with the lab's prefix (`BMAC_LAB_TAILSCALE_PREFIX`,
set by the scenarios), so no lab reaches another's machines. A stage that
hasn't passed yet is developed on the SATA lab, from the live stage before
it; the final runs go on all three labs at once, up to 29 GiB of RAM each:

```bash
lab/lab --config lab/lab-sata.toml create            # once per live lab
lab/lab --config lab/lab-sata.toml scenario m7_join  # iterate here
lab/lab scenario m7_join &                           # final: all three at once
lab/lab --config lab/lab-scsi.toml scenario m7_join &
lab/lab --config lab/lab-sata.toml scenario m7_join
```

A VM's stages share its tailnet name, so they form one chain: each builds
on the one before, and a scenario that needs a VM differently set up adds a
stage after the chain's last rather than a second one beside it.

| Stage | VM | Saved by | Used by | State |
|---|---|---|---|---|
| `m7-first-host` | mox3 | `m7_storage`, when every check passes | `m7_join` | boot mirror and SATA data mirror, all LUKS2 (the run's made-up passphrase is kept with the stage and typed at the console when an NVMe-lab restore boots), setup finished, an encrypted one-host cluster whose bmacd issues join bundles |

## A cluster running bmacd

`lab/lab bootstrap` turns the running hosts into the Proxmox cluster
`bmaclab` and installs a development build of `bmacd` on each
(`design/IMPLEMENTATION_PLAN.md`, M2.13). It builds `bmacd` and `bmac` on
mox1, which runs the hosts' Debian, so the first run takes a while; later
runs rebuild incrementally, and `--no-build` keeps what is installed. It
builds the web app here first (`pnpm --dir webui build`), since `bmacd`
embeds `webui/dist`. The
crates are vendored from this machine's cargo cache (build v3 here once
first), so the hosts never reach crates.io. As v2's host setup does, it
swaps the enterprise repositories, which refuse an unsubscribed host, for
`pve-no-subscription`, and installs `packaging/bmacd.depends`.

The lab never joins the real tailnet before M6, so every lab VM gets a
`tailscale` stand-in in `/usr/bin` (unless the real package is installed)
that answers `status --json` and `ip -4` from the management network, and
each VM's tailnet name
(`bmaclab-qdevice`) in a marked block of `/etc/hosts`, where MagicDNS would
resolve it. The cluster's QDevice setting names `bmaclab-qdevice`, so the
QDevice operations run against the lab's QDevice VM.

`--snapshot NAME` snapshots the lab afterwards (`bootstrapped` is the usual
name, so `lab/lab reset bootstrapped` returns to it). Every step can be
rerun.

The build has the `lab` feature, which adds two mock operations that change
nothing: `lab.interactive` (requests, a confirmation, a manual action) and
`lab.long` (a minute of progress), each run on the host it is given. The
Proxmox users it adds, their lab-only passwords, alice's TOTP secret, and
her recovery keys are in `users.json` in the lab folder.

`bmacd` listens only on a host's private and management addresses, so reach
it through SSH: `lab/lab ssh mox1 -- bmac status`, or forward the Dashboard
with `ssh -L 8443:100.64.0.11:8443` using the lab's key. `bmaclab.api` does
this for scenarios, pinning each host's certificate.

`lab/lab scenario m2` checks M2's done-when: alice logs in on every host
with TOTP, and a `lab.interactive` job started on mox1 runs on mox2 and is
followed and answered from mox3 until it completes.

`lab/lab scenario m4` runs the diagnostics script operations on every host
through its API and keeps each job's events in `m4-jobs` in the lab folder.

`lab/lab scenario m5` checks M5's done-when, playing the faults: a SCSI disk
of a test mirror on mox2 hot-unplugged over QMP and put back, a 4 GiB test
pool filled and emptied, corosync stopped on mox3, and mox3 powered off.
Each must open its alert and send its email, then resolve and send the
recovery. It stores `POSTMARKAPP_LAB_API_KEY` as the cluster's Postmark
token for the run and removes it afterwards, emails only critical `zfs` and
`host` alerts to `test@blackhole.postmarkapp.com`, and silences alerts
already open, so one run sends about ten emails of the sandbox server's
100 a month. It also checks the journal collector and log catch-up after
mox3 returns.

`lab/lab scenario qdevice` runs the membership operations, playing the
operator: it reads the QDevice's host-key fingerprint on the QDevice VM and
adds the cluster's QDevice access key there when a job asks. Add QDevice on
three hosts changes nothing; removing mox3 adds the QDevice vote; the QDevice
diagnostic passes from mox1 and mox2; Remove QDevice takes the vote away and
purges the machine; Add QDevice puts it back. It leaves two hosts, so take a
snapshot of the bootstrapped lab first (`lab/lab bootstrap --snapshot m4`)
and return with `lab/lab reset m4` and `lab/lab bootstrap`.

`lab/lab scenario m7_storage` installs mox3 as `m6` does, with LUKS2 and
its two SATA disks as a data mirror, then runs the host setup job from the
Dashboard API as root@pam. It types a passphrase made up for the run at the
serial console, with `bmac luks-enter` and at each drill boot, and checks
that the boot mirror was encrypted in place, the SATA mirror was added
encrypted, the block-size storages exist, the passphrase file is gone, and
every boot drill passed, and that the private network phase put the VLAN NIC
(which reports 10 Gb/s in the lab) in `vmbr-private` with its addresses.
mox3 then makes a one-host cluster and gets guest egress, its HAProxy LXC,
and LUKS header backups, passes the final verification, and leaves setup
mode. mox3 is put back afterwards.

`lab/lab scenario m7_join` installs mox3 and sets it up as a one-host
cluster without the boot drills, then issues a join bundle for mox2 from
mox3's Dashboard API and installs mox2 from it. Once mox2 asks to join, it
approves the request on mox3; the `hosts.join` job sets mox2 up over SSH
(the boot drills included), answering the root@pam password once. It checks
for a quorate two-host cluster with mox2's first corosync link on its
private address, both hosts pinned in each `/etc/hosts`, the setup key
gone, the reservation cleared, and the password in none of the job's
events. Both hosts are put back afterwards. The join's SSH needs the
host-to-host `tcp:22` rule in the policy below.

### The Web Dashboard in Chrome

`lab/browser/check-webui.mjs` drives the Web Dashboard in headless Chrome
through a forwarded port. It logs in as alice with a fresh TOTP code, marks
any interrupted lab job failed (an interrupted job keeps its locks), runs
`lab.interactive` on mox2 through every request kind, including a rejected
answer, and checks that a reload returns to the login page. It fails on any
Content Security Policy violation, console error, cookie, or browser storage
use. Screenshots go to `/tmp/bmac-webui-check`.

```bash
ssh -i ~/.local/share/bmac-lab/bmaclab/id_ed25519 -p 22002 -N \
  -L 127.0.0.1:18443:100.64.0.11:8443 root@127.0.0.1 &
node lab/browser/check-webui.mjs [--recovery N] [--rotate] [--headed]
```

`--rotate` then rotates the certificate of the host behind the forwarded
port from the Settings page. The fingerprint shown before the switch must be
the one the host serves afterwards, and after logging in again Settings must
show it as the current certificate and as the host's published fingerprint.

`--recovery N` logs in with alice's recovery key N (0 to 9) instead.
Proxmox deletes a recovery key once it is used, so each N works once until
bootstrap makes the users again (after `lab/lab reset installed`, say).

## Tailscale for the lab

Until M6 the lab uses the `tailscale` stand-in above. From M6, lab VMs that
need the real tailnet join it as `bmaclab-<vm>`, never under the bare VM
name. The lab uses the production tags, group, and rules, on a Tailscale
account of its own, so nothing the lab does can reach a production tailnet:
log the workstation in to the lab account before working on the lab, and
back in to the production account afterwards. Set the lab account up once:

1. **The admin group.** Under **Access controls**, add a group named
   `bmac-admin` holding everyone who works on the lab, as in production.
2. **The tags**, each owned by `group:bmac-admin`:

   | Tag | Given to |
   | --- | --- |
   | `tag:bmac-proxmox-host` | Lab Proxmox hosts |
   | `tag:bmac-proxmox-qdevice` | The lab QDevice |
   | `tag:bmac-backup-appliance` | Lab backup appliances |
   | `tag:bmac-discovery` | Lab discovery systems |

3. **The access rules**, in the policy's `grants`: the main README's rules
   ("Tailscale setup"), unchanged:

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

4. **Auth keys.** Under **Settings**, **Keys**, **Generate auth key**, make
   one key per tag, each **Reusable**, with **Tags** on and only that tag.
   Only the discovery key is **Ephemeral**. Put them in `lab/secrets.env`
   (copied from `lab/secrets_dot_env`, mode 0600, Git-ignored):

   | Variable | Tag | Ephemeral |
   | --- | --- | --- |
   | `TAILSCALE_LAB_AUTH_KEY_PROXMOX_HOST` | `tag:bmac-proxmox-host` | Off |
   | `TAILSCALE_LAB_AUTH_KEY_QDEVICE` | `tag:bmac-proxmox-qdevice` | Off |
   | `TAILSCALE_LAB_AUTH_KEY_BACKUP_APPLIANCE` | `tag:bmac-backup-appliance` | Off |
   | `TAILSCALE_LAB_AUTH_KEY_DISCOVERY` | `tag:bmac-discovery` | **On** |

5. **API key.** Under **Settings**, **Keys**, **Generate access token**, make
   an API access token and put it in `TAILSCALE_LAB_API_KEY`. The lab uses it
   only to list and remove its own devices.
6. **Admin email.** Put the email address that `group:bmac-admin` holds on
   the lab account in `TAILSCALE_LAB_ADMIN_EMAIL`. Before the lab uses the
   tailnet, it checks that the API key's policy lists that address in
   `group:bmac-admin`, that this workstation is logged in to Tailscale as
   that address, and that the workstation's own node is one of that
   tailnet's devices. If either check fails, the API key or the
   workstation's login may be on the production account, so the lab stops
   without touching the tailnet. Give the production account's group a
   different address, or this check can't tell the two apart.

Why only discovery is ephemeral: Tailscale deletes an ephemeral device some
time after it goes offline. A lab host left off overnight, or reset to a
snapshot, would then have to join again as a new device with a new Tailscale
address. Corosync's second link and the registered QDevice use that address,
so the cluster would break. A discovery system lives for one session, so for
it, removal is exactly right.

**Removing lab devices.** The lab removes its devices from the tailnet
itself, through the API:

- `lab/lab destroy` and `lab/lab rebuild` remove every lab device. Without an
  API key they leave the tailnet alone and say so.
- Before a VM joins the tailnet again, the lab removes that VM's old device.
  Otherwise Tailscale names the new device `bmaclab-<vm>-1`, and MagicDNS
  keeps the bare name on the old one.
- `lab/lab tailnet list [VM...]` shows the lab's devices, and
  `lab/lab tailnet remove [VM...]` removes them.

A lab device is one named with the `tailnet_prefix` whose tags are all
BMAC tags. The lab never touches a device with any other name, with no
tags, or with any other tag, so a machine the lab didn't make is safe even
if the workstation is logged in to the wrong account.

What else reusable keys change:

- **Reused keys.** Real hosts and appliances must use single-use keys, and
  the Deployer and `bmacd` refuse a key whose digest they have seen before.
  Builds with the `lab` feature accept a reused key, saying so in the job
  log. Unit tests cover the refusal.
- **Expiry.** Auth keys expire after at most 90 days, and API access tokens
  too. When Tailscale refuses one, the lab names the variable to replace.
  Devices that already joined are not affected, because tagged devices
  don't expire.

The lab reads `secrets.env` in-process only and never prints its values.

## Email for the lab

Alert emails in the lab go through a Postmark server whose delivery type is
**Sandbox**: Postmark accepts each message, shows it in the server's
activity, and delivers none. Put that server's API token in
`POSTMARKAPP_LAB_API_KEY` in `lab/secrets.env`. The account allows few
messages a month, and sandbox messages count, so send only when a test
needs it.

`lab/lab postmark ...` runs `dev/postmark_templates.py` with that token
(passed as `POSTMARK_SERVER_TOKEN`, never printed):

```bash
lab/lab postmark check                          # offline: every template is well formed
lab/lab postmark validate --server bmac_lab     # Postmark renders each one with its test model
lab/lab postmark list --server bmac_lab         # what the server has
lab/lab postmark diff --server bmac_lab         # what differs from the server
lab/lab postmark sync --apply --server bmac_lab # create the missing templates
lab/lab postmark send bmac-test-v1 --to test@blackhole.postmarkapp.com --apply --server bmac_lab
```

`--server` names the server you expect, so a token for the wrong server is
refused before anything changes. Only `send` uses up the monthly volume.
Even a sandbox server refuses a From address that isn't a confirmed Sender
Signature of the account (Postmark's Sender Signatures page). Put a
confirmed one in `POSTMARKAPP_LAB_API_SENDER_SIGNATURE`; `send` uses it
unless you give `--from`.

On a lab cluster, enter the same token in the Dashboard (Settings, then
Notifications) to exercise delivery end to end. Because nothing reaches an
inbox, check the server's activity in Postmark instead; a real-inbox test
needs a Live server.

## Lab credentials

The VMs use a lab-only SSH key (generated in the lab folder) and the root
password in `lab.toml`. They are reachable only from this machine, through
127.0.0.1. SSH to them does not pin host keys, because every rebuild
changes them. None of this applies to a real cluster.

## Files

The lab keeps its state in `~/.local/share/bmac-lab/<name>/`, or in
`BMAC_LAB_DIR`: downloaded images and tools, the SSH key, one folder per VM
(disks, firmware store, console log, and the QMP, console, and VNC sockets),
and snapshots. The unit tests in `tests/` run without QEMU.
