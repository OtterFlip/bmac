# <img src="dashboard/src-tauri/icons/32x32.png" alt="BMAC"> BMAC - Build My App Cloud

## Intro

Sick of paying out-the-nose for hyperscaler cloud hosting? Trying to find your way out of their pricing labyrinth? Disillusioned with the complexity and the proprietary cloud APIs needed to glue-together a serverless architecture and their lock-in effect?

You're not the only one who noticed that the public clouds aren't really necessary for many webapps, even at scale.  Now that a single x64 server can host 1024 concurrent threads with a dual-socket AMD EPYC 9996 machine, all sharing the same RAM, the vertical-scaling ceiling is very high, which means that horizontal-scaling often isn't necessary.  That's a cloud-in-a-box.  For most apps you'll never need more, but you don't want to start with such an expensive box.  What you need is the ability to start with affordable hardware, suitable for your initial workload, with an easy way to migrate to better hardware as you grow, with minimal downtime, and with data-redundancy and high-availablity-failover baked-in so your data's safe.  You also need a system for testing upcoming changes to production without actually affecting production.  That's what BMAC delivers.

BMAC, at its heart, is a library of [user-callable scripts](./scripts/user_callable/).  However these scripts can also be called by a machine with the --json parameter, which changes how the scripts' inputs and outputs operate.  The BMAC Dashboard GUI performs the bulk of its work using this script library, called in machine mode.

I vibe-coded BMAC with Claude Opus 5.5.  However I'm also a CS grad and software engineer with over 30 years of professional experience developing mostly storage solutions, HA clustering software and disaster recovery software where my key focus was on the kernel drivers, but also extended up the stack.

## Screenshots

Screenshots of the BMAC Dashboard are [here](docs/DASHBOARD_SCREENSHOTS.md).

## Do I Have to Read All This Crap?

No, it's here as reference - point your favorite chatbot at this page when you have questions about BMAC.  All you really need to do to use BMAC is to satisfy the [prereqs](#prereqs---what-you-need-to-use-bmac), then install the latest v2.0.0+ release's .deb file, which installs BMAC's client script engine, and its Dashboard GUI, then follow the guidance on the BMAC Dashboard GUI's Settings page.  It'll walk you through everything you need to do to get your cluster setup from scratch.

## Releases

BMAC releases represent tested, known-good points in the project's development and are the recommended versions for new installations.

The current source code on the `main` branch may contain changes that have not yet gone through the complete release testing process. For production use, download the latest release from the [GitHub Releases](https://github.com/OtterFlip/bmac/releases) page.

BMAC release [v1.0.0](https://github.com/OtterFlip/bmac/releases/tag/v1.0.0) was solely the script engine library.

BMAC release [v2.0.0](https://github.com/OtterFlip/bmac/releases/tag/v2.0.0) added the Dashboard GUI interface that exposes all of BMAC's functionality, so you don't need to call any of the scripts directly - although you can if you want to.  Install the dashboard and script engine with the release's .deb package on an x64 Debian-based distro.  v2.0.0 has breaking changes from v1.0.0, so there's no clean upgrade path between the two.  However given the short timeframe between these it's unlikely anyone is affected. This project has fairly low visibilty at present, so going to great lengths to preserve compatibility between these major versions isn't really necessary or a priority, at this time. That will change as usage grows.

## BMAC's Features

- Grokable.  With moderate reading, BMAC can be understood without spending weeks to ramp up.  The complete design is [here](docs/MAIN_DESIGN.md).
- Simple App Architecture.  Your webapp's deploy target environment is the same environment as your dev workstation - a computer.  Build and test your webapp all on your own computer and then deploy your built webapp to your BMAC production guest VM (ex: prod1).  BMAC supports many webapps so you can use BMAC to extend the cluster to support them all.  This enables you to use monolithic architecture which is one of the simplest app architectures.
- Test Using Actual Production State Without Affecting Production.  This is [one of the killer features of BMAC](docs/STAGING.md), which handles all the setup steps for you.  Your standby host is not useless as it sits there waiting to act as a failover host.  Instead, it's actively leveraged to host your staging VMs, which are the VMs you push new changes to in order to test those changes prior to pushing them to the production VM.  Since all of the data for the production VM's disk is replicated by ZFS to the standby host(s), that data, on a standby host, can be used as the source for a temporary staging VM without any additional data copy.  This is done by making a temporary ZFS snapshot (removed when the staging VM itself is removed), and making a linked-clone of that snapshot on the standby host. The linked-clone of the snapshot is a fast and lightweight operation, using Copy-on-Write (CoW), and this linked clone acts as the hard disk for the new staging VM. Prior to attaching the linked clone to the staging VM, the linked clone is mounted on the standby host and its contained EXT4 filesystem is patched in order to alter the machine identity (IP, MAC address, hostname, machine ID, etc) of the machine so that the new staging VM doesn't collide with the production VM it's derived from.  An optional custom script can also be added to perform application-specficic patching, if desired.  Your staging VMs are visible with functional HTTPS access via a browser. Typically you'll want to have a custom app-specific patch which flips settings on your webapp in the staging VM to cause it to be in 'staging mode' such as to limit who can login, etc., as your staging VM can be fully accessible on the Internet for HTTPS browser access.  Your webapp can also look at the hostname - if it begins with 'stage' then your app knows it's running on a staging host, and can change its behavior.  During a failover event for a production VM, any staging VMs on the failover host are automatically stopped and removed, in order to ensure that the production VM has sufficient resources (RAM, CPU).
- Failover With Little Downtime.  Your production VM is a High-Availability (HA) guest/VM, so all of your production VM's disk data is replicated using ZFS async replication as often as once-per-minute from your production guest VM's primary host to the other placement hosts in the cluster.  If the primary host dies then the production VM is automaticlaly started on a standby host.  So failover downtime is the time to boot the VM and start its services.  Since the ZFS replication is async and at most once-per-minute, at most 1 minute of DB transactions may be lost in a critical host failure scenario, so BMAC is not suitable if your webapp requires zero data loss during failure events.  Email alerts are sent on both the failure and recovery.
- Scale Up with Little Downtime.  When your resource needs grow beyond the capacity of your intial hardware, add new hardware to your cluster and increase the 'placement' of your production VM to those new machines, which will cause all of the VM's data to be replicated, using ZFS, to those new cluster hosts.  When the data is fully replicated you can then migrate the VM to run on the higher-capacacity hosts and decomission the lower-capacity servers. Flipping that switch is a one-time reboot time cost of the production VM, so the downtime for such a migration, an uncommon event itself, is on the order of a few minutes at most.
- Full At-Rest Disk Encryption.  Many webapps need to satisfy security requirements and so BMAC has the option to configure all cluster hosts with disk-level encryption using LUKS.  When this option is enabled, you must enter a password on the hosts during host boot.  All mirror members for all mirror sets (each mirror is called a vdev) share the same password, so only one password must be entered at boot time.
- Software RAID1, Including OS Boot Volume.  Even if your servers don't have a hardware RAID controller, BMAC will configure them with ZFS RAID1, mirroring each set of identical-capacity disks. During host setup, BMAC also offers the option to fully-test failure scenarios for each mirror member of the boot disk RAID1 set in order to prove host bootability.  BMAC also ensures that each boot disk has a mirrored EFI System Partition (ESP), in addition to mirroring the ZFS partition.  For servers that support host-swap of NVMe disks, this enables each server to endure a single-member disk failure for each mirror set without any downtime or data loss.  Simply pull the failed disk, insert an identical-capacity replacment, and resilver the mirror set.
- Dell iDRAC9 IPMI support.  BMAC has been tested on Dell Poweredge R640 'Dual Intel Xeon Gold 6138 40 core/80 thread' hosts at FiberState, and one of the host setup script options is to use iDRAC to simplify hardware discovery. This is not required, and a hardware discovery script is provided in cases where you're using some other type of host.
- Free Monitoring and Alerting.  Every host, the QDevice, and every production VM runs a monitor agent that reports to its own free Healthchecks.io check, so you get an email (and, through Gmail filters, an optional Pushover notification) when a node goes silent, a link or quorum is lost, an app stops answering through Cloudflare, a disk or pool degrades or fills, or a service stops. See [Monitoring and Alerts](#monitoring-and-alerts).
- BMAC supports decommissioning disk capacity you no longer need. The disk workflows trim affected guests, force replication, scrub the pool, evacuate an eligible non-boot vdev, finalize its retirement, and identify the exact disks that are safe to remove physically. This can be useful after data has moved elsewhere and a host no longer needs all of its installed capacity.

## Prereqs - What You Need to Use BMAC

- 2 servers with UEFI BIOS, each with their own public IP and Internet gateway connection on a primary NIC, and a private network connection between these two servers on a secondary NIC on each (can be a VLAN).  We prefer [FiberState's 'Dedicated Server' option](https://www.fiberstate.com/dedicated-servers) (we have no affiliation, we're just a satisfied customer as FiberState is competitively-priced, particularly if you pay for a year in advance).  These will be your cluster's Proxmox hosts, hosting the guest VMs which in turn host your webapp(s).  You can add more than 2 servers as Proxmox hosts if you like, but start with 2.
- Each server must have at least two physical NVMe hard disks of the same nominal capacity (their exact byte capacities may differ by up to 1%) for the ESP and OS partitions
- 1 ultra-cheap VPS host with a public IP address, such as a $5/month Shared CPU Nanode 1GB VPS at Linode.  This node's sole purpose will be to play the role of a QDevice, to vote in your cluster quorum to break tie votes.  You'll set up this QDevice VPS using [these instructions](docs/QDEVICE_MANUAL_SETUP.md).
- A developer workstation running a debian-based Linux distro (we made and tested this with x64 Ubuntu) to run BMAC's scripts.  This is required for the host setup scripts, but the prod/staging scripts usually work on macOS too, although macOS support at this time is in-flux so don't count on anything working on macOS, even for tagged releases.
- A Tailscale account (the free account will work fine), setup like [this](#instructions-for-tailscale-setup).
- A Cloudflare account, and your app's domain (ex: 'example.com') managed at Cloudflare, setup like [this](#instructions-for-cloudflare-setup). Public web traffic reaches your VMs through a Cloudflare Tunnel (free). A domain whose web app has an `/api/` needs Cloudflare Pro for bot protection that never challenges API calls.

## BMAC's Stack

- Proxmox
- LUKS
- ZFS 
- Corosync
- Cloudflare Tunnel (`cloudflared`)
- nftables
- Tailscale
- Ubuntu Server x64 (default OS for prod and staging guests)

## Step-by-Step Host Setup Guide

Prepare for Proxmox Host Setup

- If you're using iDRAC, make sure you can access it.  Additional tips on setting up iDRAC with FiberState are provided in a section below.
- Read the top of `config/secrets_dot_env` and make a copy of it as the instructions direct, to `config/secrets.env` and choose your password values.  If you're using iDRAC then make sure all of your hosts use the same iDRAC password and set it in your `config/secrets.env` file.
- Read the top of the example `config/mox1_dot_conf` and `config/mox2_dot_conf` and make a copy of each, to `config/mox1.conf` and `config/mox2.conf` which you should fill in with your actual host values.
- Copy the example `config/cluster_dot_conf` to `config/cluster.conf` and modify cluster.conf to act as your own source for your cluster install.
- Set appropriate values in `config/cluster.conf`, `config/mox1.conf`, and `config/mox2.conf` for your hardware and network. The example values are for hosts with 128GB RAM, two 2TB NVMe disks, and 40 cores/80 threads, with a shared private network on `10.213.0.0/24` and a separate public IP and Internet gateway per host. Adjust all network, storage, CPU, and RAM values for your environment and expected production/staging workload. Each `config/moxN.conf` also needs the serial numbers of the NVMe drives for each rpool mirror (mirror 0 is mandatory and becomes the boot mirror), the exact byte capacity of each of those drives, and the MAC addresses of the host's public NIC (`PROXMOX_PUBLIC_MAC`) and private NIC (`PROXMOX_SECONDARY_MAC`). To collect them, boot a Linux Live environment (USB or CD, in its try/live mode) on each machine you intend to set up as a Proxmox host, copy the standalone `scripts/user_callable/hosts/cluster_setup_prereq.sh` into it, and run it there with `bash cluster_setup_prereq.sh`. It changes nothing and lists every NVMe drive's serial number and byte capacity and every NIC's MAC address. If you use iDRAC for setup, you must still set the drive serial numbers and MAC addresses but not the byte capacities, which setup reads through Redfish; the iDRAC admin web UI shows the serial numbers and MAC addresses, so you do not need to run the prereq script from a Live environment. See [`scripts/user_callable/hosts/README.md`](scripts/user_callable/hosts/README.md#host-setup-flow) for details. In the iDRAC admin web UI the MAC addresses to look for are under the Port tab(s) and are the field labeled "Virtual MAC Addresses".
- Set the HTTPS download URL and SHA256 hash value of the Proxmox VE x64 installer ISO in `config/cluster.conf` for PROXMOX_ISO_FILE_URL and PROXMOX_ISO_FILE_SHA256. `scripts/user_callable/hosts/add_proxmox_host.sh` downloads it to `config/artifacts/hosts/source-iso/` on your dev workstation, verifies its hash, and after building a host's custom installer ISO asks (default yes) whether to keep it for future host installs.
- Add your (and another colleague's) SSH public key to the ADMIN_1_PUBLIC_SSH_KEY and ADMIN_2_PUBLIC_SSH_KEY values in `config/cluster.conf`
- Follow the instructions in `docs/QDEVICE_MANUAL_SETUP.md` to set up your qdevice and add it to your tailscale network. Before doing that you'll need to follow the Tailscale setup steps below to add the appropriate tailscale tags and access policies.
- Follow the Cloudflare instructions below to create your cluster's tunnel and put its three values in `config/cluster.conf` and `config/secrets.env`. The per-domain steps come later, when you create a production VM.

If you've completed the above steps then you're now ready to setup your Proxmox cluster hosts.  From a Debian-based (such as Ubuntu) workstation, run the `scripts/user_callable/hosts/add_proxmox_host.sh` script and choose to setup `mox1`.  A Debian-based workstation is required for the Proxmox host setup because that script must create a custom Proxmox installer ISO, and the Proxmox project only provides a Debian-based helper tool for this task.  Follow the `scripts/user_callable/hosts/add_proxmox_host.sh` script's instructions.  You  can actually run the same script in another terminal session to setup `mox2` at the same time, however I recommend at the point where the scipt asks you if you have verified that the VLAN (or private network connedtion) is working, that you only let one script at a time proceed past that point rather than letting many run beyond that point concurrently.

After our cluster is configured, you should be able to access the Proxmox admin web UI at:  [https://mox1:8006/](https://mox1:8006/) or [https://mox2:8006/](https://mox2:8006/)

You can now use the `scripts/user_callable/guests/prod/add_prod_vm.sh` script to create your first production VM to host your application, which will deploy a blank/vanilla Ubuntu x64 OS to that VM and setup SSH access, and nothing else.  You can always tear it back down if you're not satisfied with it using `scripts/user_callable/guests/prod/remove_prod_vm.sh`.  If you like, you can also deploy an example todo webapp to your new production VM using `scripts/user_callable/guests/prod/deploy_hello_app_to_prod.sh`.

After creating a production VM, you can test creation and teardown of a staging VM, based on that production VM, using the `scripts/user_callable/guests/staging/add_staging_vm.sh` and `scripts/user_callable/guests/staging/remove_staging_vm.sh` scripts.

### Instructions for Cloudflare Setup

Public web traffic reaches your VMs through one Cloudflare Tunnel per cluster. Every Proxmox host runs a replica of the tunnel, which only makes outbound connections to Cloudflare, so no Proxmox host accepts inbound web traffic and no Cloudflare load balancer is needed: when a host loses its internet connection, Cloudflare uses the other hosts' replicas. BMAC generates the map from each FQDN to its VM's address; your Cloudflare account only needs the tunnel and a few DNS records. See [`docs/BMAC_SIMPLIFIED_NETWORKING.md`](docs/BMAC_SIMPLIFIED_NETWORKING.md) for the design.

**Once per cluster: create the tunnel.** It must be created with the `cloudflared` command-line tool, not in the Cloudflare dashboard (a dashboard-created tunnel ignores BMAC's generated config).

1. Install `cloudflared` on your workstation (Cloudflare's apt repository on Linux, or `brew install cloudflared` on macOS).
2. Run `cloudflared tunnel login`. A browser opens; sign in and pick any one of your domains (the choice doesn't limit which domains the tunnel serves). This saves `~/.cloudflared/cert.pem`, which is only needed to create tunnels; keep it on your workstation or delete it after the next step, and never copy it into this checkout or onto a host.
3. Run `cloudflared tunnel create bmac-<cluster name>`. It prints the tunnel ID and writes `~/.cloudflared/<tunnel ID>.json`.
4. Copy the three values from that JSON file into your configuration: `AccountTag` into `CLOUDFLARE_ACCOUNT_ID` and `TunnelID` into `CLOUDFLARE_TUNNEL_ID` in `config/cluster.conf`, and `TunnelSecret` into `CLOUDFLARE_TUNNEL_SECRET` in `config/secrets.env`. Keep the JSON file somewhere safe outside the checkout, or delete it.

Host setup installs the replica on every Proxmox host. The tunnel then appears in the Zero Trust dashboard (`one.dash.cloudflare.com`) under Networks → Tunnels, marked as locally managed, with one connector per host. If you run several BMAC clusters, create one tunnel per cluster; an account can hold 1,000 tunnels at no cost. A tunnel only serves domains in its own Cloudflare account.

**Once per cluster: the tunnel health alert.** Under Notifications in the Cloudflare dashboard, add a **Tunnel Health Alert** for the tunnel, delivered by email, for the Degraded and Down states (see [Monitoring and Alerts](#cloudflare-tunnel-health-alert)).

**Once per production VM's domain** (when the production VM is created; `add_prod_vm.sh` prints these steps with the right values):

1. Add the domain to Cloudflare (its nameservers must point at Cloudflare), in the same account as the tunnel.
2. Under Domains (click your domain) | DNS, add two DNS records, both **proxied**:

    ```
    Type: CNAME   Name: @   Target: <tunnel ID>.cfargotunnel.com
    Type: CNAME   Name: *   Target: <tunnel ID>.cfargotunnel.com
    ```

    The `*` record covers `www.example.com` and every staging FQDN, `stage1prod1.example.com` through `stage9prod1.example.com`. Cloudflare's free certificate covers the domain and one level below it, which includes all of them.
3. Under SSL/TLS | Edge Certificates, turn on **Always Use HTTPS**, so no host or VM ever needs to accept plain HTTP.
4. Bot protection:
    - **Web app without an API:** stay on the free plan and turn on **Bot Fight Mode** (Security | Bots).
    - **Web app with an API under `/api/`:** subscribe the domain to **Cloudflare Pro**, leave **Bot Fight Mode off**, and turn on **Super Bot Fight Mode**. Then, under Security | WAF | Custom rules, create this rule and place it **first**:

        ```
        Rule name: Never challenge /api/
        Expression: starts_with(http.request.uri.path, "/api/")
        Action: Skip
          - All remaining custom rules
          - All rate limiting rules
          - All managed rules
          - All Super Bot Fight Mode rules
          - Browser Integrity Check, Security Level, User Agent Blocking,
            Zone Lockdown, Hotlink Protection
        ```

        The free Bot Fight Mode cannot be skipped by any rule and may challenge API calls, which client JavaScript can't handle; Super Bot Fight Mode can be skipped, which is why an app with an API needs Pro. All API traffic, on production and staging VMs, must use paths starting with `/api/`.
5. If a production VM answers for an alias domain in a different Cloudflare zone, repeat these steps in that zone.
6. Optionally, under Rules | Overview use "Create Rule" | "Redirect Rule" to send `www` to the bare domain:

    ```
    Rule Name: Redirect from http/https www.example.com to https example.com
    CHECK "Wildcard pattern"
    Request URL: http*://www.example.com/*
    Target URL: https://example.com/${2}
    Status code: 301 - Permanent Redirect
    UNCHECKED "Preserve query string"
    Place at: First
    ```

7. Under SSL/TLS | Origin Server | Origin Certificates click 'Create a certificate' with these settings. Your application uses it to serve HTTPS inside the VM; the tunnel replicas verify it.

    ```
    Private Key type: RSA (2048)
    Hostnames: example.com and *.example.com
    Validity: 15 years
    Save the certificate to a .cer file on your workstation.  This is the public key.
    Save the private key to a .key file in the same dir as the .cer file.  This is the private key.  Never share it.
    Both of these files will be fed to the `scripts/user_callable/guests/prod/deploy_hello_app_to_prod.sh` script.
    ```

### Instructions for Tailscale Setup

Install Tailscale on your development workstation and login/connect with your Tailscale network.  Make sure you can see your dev workstation listed in the Tailscale web UI under "Network" | "Machines".

In your Tailscale account under "Access controls" | "Definitions" you'll find a "Groups" tab.  On the Groups tag you need to create a new group called proxmox-admins and add your user (and any other users you've invited) to this group.  Next, also in the "Definitions" page you'll see a "Tags" tab, and under that tab create the following tags, all owned by group:proxmox-admins:

tag:proxmox-host
tag:proxmox-qdevice

Next, navigate to "Access controls" | "Policies" and under "General access rules" add the following 5 rules:

```jsonc
// Admin SSH
{
	"src": ["group:proxmox-admins"],

	"dst": [
		"tag:proxmox-host",
		"tag:proxmox-qdevice",
		"tag:prod-guest",
		"tag:staging-guest",
	],

	"ip": ["tcp:22"],
}

// Proxmox Web UI
{
	"src": ["group:proxmox-admins"],
	"dst": ["tag:proxmox-host"],
	"ip":  ["tcp:8006"],
}

// QDevice/corosync-qnetd quorum traffic
{
	"src": ["tag:proxmox-host"],
	"dst": ["tag:proxmox-qdevice"],
	"ip":  ["tcp:5403"],
}

// Initial QDevice Setup by Proxmox Setup
// This rule is required by add_proxmox_host.sh when it adds an even number of hosts, and by add_qdevice.sh
{
	"src": ["tag:proxmox-host"],
	"dst": ["tag:proxmox-qdevice"],
	"ip":  ["tcp:22"],
}

// Corosync/Kronosnet host-to-host links
{
	"src": ["tag:proxmox-host"],
	"dst": ["tag:proxmox-host"],
	"ip":  ["udp:5405-5412"],
}

// Monitoring: QDevice may ping hosts over Tailscale (no ports, no shell)
{
	"src": ["tag:proxmox-qdevice"],
	"dst": ["tag:proxmox-host"],
	"ip":  ["icmp:*"],
}
```

The last rule is required when monitoring is enabled (see [Monitoring and Alerts](#monitoring-and-alerts)): the QDevice's monitor agent pings every host over Tailscale. It grants no ports and no shell access.

Each time you set up a proxmox host, the script will prompt you for a unique Tailscale Auth Key.  You can generate in the Tailscale web UI unser "Settings | "Keys" and on that page under "Auth Keys" use the "Generate auth key..." button and give the key any name (typically something like `mox1`) of the key, make sure Reusable is Off, any expiration day count value is fine (as these will switch to never expiring once they're actually used), and keep Ephemeral Off, and most importantly turn Tags ON and select the tag:proxmox-host tag.  Generate the key and copy it somewhere so that you have it ready for when the `scripts/user_callable/hosts/add_proxmox_host.sh` script prompts for it.  You'll need one key for each proxmox host you setup.

Your SSH access from your dev workstation to your proxmox hosts will be via your Tailscale network. After each host is installed, `scripts/user_callable/hosts/add_proxmox_host.sh` offers to add it to your `~/.ssh/config` (for example `ssh mox1`), pinned to an SSH host key the script generated and embedded in that host's installation ISO, so there is no first-use fingerprint prompt to verify.

### FiberState iDRAC VPN Setup

Since host setup must be done using a Debian workstation, there's a good chance you may be using the GNOME desktop environment, in which case you may run into issues setting up your FiberState IPMI VPN access.  Here's the workaround.

1. First in a web browser connect to the OpenVPN URL provided by FiberState.  When the browser complains about the certificate being non-CA-issued, show the certificate and double check to ensure it has the following values, and only if these values match should you proceed to the page:

    ```
    Certificate 3e0d142fdde35a78a0abdb346daf22406ed92b156d30442f8f473e7c7bdaa529
    Public Key 383b401d4257b7090945f6a893c47e3f8015bbd57ae1e74495711f96704dbe69
    ```

    If you see other values then submit a ticket to FiberState to ask them what the current valid fingerprints are for their HTTPS certificate for their IMPI web VPN server.

2. Once connected to the "Access Server" use the download link to download the "Connection profile."  This will download a .ovpn file that can be imported directly into Gnome's network settings for VPN connections. Note that you can test this .ovpn connection with your openvpn cli client directly, like  so:

    ```
    sudo openvpn --config ~/Downloads/FiberState_IPMI_VPN.ovpn --auth-user-pass
    ```

3. After importing the .ovpn file into Gnome's Network VPN settings, you need to make the following changes to the imported VPN connection:

    - In the Details tab check the "Make available to other users" tab
    - Add the username and password provided by FiberState in the Identity tab
    - In the IPv4 tab, under Routes, check the box labeled "Use this connection only for resources on its network"
    - In the IPv6 tab, under Routes, check the box labeled "Use this connection only for resources on its network"
    - At the bottom of the Identity tab, use the "Advanced" button to load the "Advanced Properties" dialog and in that dialog's "Security" tab change the Cipher from AES-256-CBC to AES-256-GCM.

### Growing or shrinking a host's storage

To add two new disks to a host's ZFS pool later, run `scripts/user_callable/hosts/add_new_disk_vdev.sh`. After a failed disk is pulled and one of the same nominal capacity installed, run `scripts/user_callable/hosts/add_replacement_disk.sh` to put it back into its mirror. To give up a pair of disks, run `scripts/user_callable/hosts/decommission_disks.sh`, then `scripts/user_callable/hosts/inventory_disks.sh` until it finalizes the retirement and lists the disks as safe to pull. See [`scripts/user_callable/hosts/README.md`](scripts/user_callable/hosts/README.md#adding-and-decommissioning-rpool-disks) for the details, including the manual reboot test after adding encrypted disks.

## Common Tasks

Run these commands from the repository root on an administrator workstation.
Scripts that target a Proxmox host prompt for its `moxN` name and default to
`mox1`; pass `--host moxN` to select it non-interactively. Read every plan and
confirmation before allowing a destructive operation.

### Using the Dashboard Instead of a Terminal

The [BMAC Dashboard](dashboard/README.md) is a desktop control panel for the
same tasks. It runs these scripts in their `--json` mode and shows their
prompts, plans, progress, results, and suggested next steps as forms and
dialogs, with the exact command it runs and its live output always one click
away. Every script also accepts `--json` directly, for use by other tools.

### Checking Cluster, Host, Guest, and Disk State

Use the read-only diagnostics before and after maintenance:

```bash
./scripts/user_callable/diagnostics/show_cluster_health.sh
./scripts/user_callable/diagnostics/show_cluster_state.sh
./scripts/user_callable/diagnostics/show_proxmox_host_state.sh --host mox1
./scripts/user_callable/diagnostics/show_prod_vm_state.sh prod1
./scripts/user_callable/diagnostics/show_qdevice_state.sh
./scripts/user_callable/hosts/inventory_disks.sh --host mox1
```

For a quick look at one area, the `list_*` scripts read everything through
one reachable host and finish in seconds:

```bash
./scripts/user_callable/diagnostics/list_hosts.sh                    # hosts, quorum, votes, QDevice registration
./scripts/user_callable/diagnostics/list_guests.sh --kind production # registered VMs with HA, replication, and addresses
./scripts/user_callable/diagnostics/list_replication.sh --guest prod1
./scripts/user_callable/diagnostics/list_storage.sh --host mox1      # ZFS pools and Proxmox storage
./scripts/user_callable/diagnostics/check_qdevice_access.sh          # ssh root@ the QDevice works, even before any host exists
```

`list_disks.sh` reads each host directly instead, so it takes a little longer.
It lists every pool vdev with its state, whether it uses LUKS, and its member
disks' serials and sizes. It also lists every disk outside a pool: whether it
is available, awaiting retirement finalization after a decommission, or part
of an interrupted add or replace. It never finalizes anything; that is
`scripts/user_callable/hosts/inventory_disks.sh`'s job.

```bash
./scripts/user_callable/diagnostics/list_disks.sh --host mox3        # vdevs, LUKS, and disks outside a pool
```

Start with `show_cluster_health.sh` for a quick check of the cluster's
hardware and networking. It ignores guests. It reports whether every host and
the QDevice is up and reachable, whether each host's corosync links (private
VLAN and Tailscale) reach every other host, and whether every ZFS pool, vdev
member, and drive is healthy. Each failed pool member is named with its
physical disk serial. It also flags any host whose `rpool` has less than 10%
of its usable space free as needing more storage. It ends with a list of problems and exits 1 if there
are any.

`show_cluster_state.sh` reports cluster membership, quorum, networking,
services, storage, and registered guests. `show_proxmox_host_state.sh` gives a
detailed report for one host, and `show_prod_vm_state.sh` checks one production
VM across HA, replication, routing, storage, QGA, and SSH.
`show_qdevice_state.sh` says whether the cluster needs a QDevice and whether
its QDevice is alive and voting on every host. If not, it says what to do
(see [Replacing a Failed QDevice](#replacing-a-failed-qdevice)).

`inventory_disks.sh` prints every physical disk with its serial number and
exact byte capacity, every imported ZFS pool vdev and member, and disks that
are not in use and are safe to remove physically. It normally makes no
changes. If a removal started by `decommission_disks.sh` has finished, it
identifies the affected disks and asks permission before finalizing their
retirement. Declining still produces the complete inventory without making
retirement changes.

### Creating or Removing a Production VM

Start with the read-only preflight, then run the creator and answer its
placement, sizing, purpose, domain, and installer questions:

```bash
./scripts/user_callable/guests/prod/add_prod_vm.sh --dry-run
./scripts/user_callable/guests/prod/add_prod_vm.sh
```

The workflow allocates the next `prodN`, installs its OS, verifies guest
identity and SSH, creates replication to every selected placement host,
configures HA, and adds its FQDNs (and its staging slots' FQDNs) to every
host's Cloudflare Tunnel config. It can safely resume its own incomplete
allocation after a failure. It then prints the Cloudflare DNS and bot
protection steps for the VM's domain (see
[Instructions for Cloudflare Setup](#instructions-for-cloudflare-setup)).

To permanently remove a production VM, inspect the exact destruction plan
first:

```bash
./scripts/user_callable/guests/prod/remove_prod_vm.sh --dry-run prod1
./scripts/user_callable/guests/prod/remove_prod_vm.sh prod1
```

This removes the VM, its HA and replication configuration, owned volumes,
and registry allocation, and removes its FQDNs from the tunnel config. It is
intentionally destructive.

### Growing a Production VM's Disk

After adding capacity to the ZFS pools of a production VM's placement hosts
(for example with `scripts/user_callable/hosts/add_new_disk_vdev.sh`), pass some of it to the VM
without stopping it:

```bash
./scripts/user_callable/guests/prod/extend_prod_vm_disk.sh --dry-run
./scripts/user_callable/guests/prod/extend_prod_vm_disk.sh
```

Pick the VM (default: the first active `prodN`), make sure `ssh prodN` works,
and the script shows the largest increase that still keeps 10% of every
placement pool's total size available. You enter the increase in bytes, MiB,
or GiB. It then grows the zvol on the live HA owner, replicates the new size
to every placement host, and grows the guest's root partition and ext4
filesystem online. Deploy the updated registry with
`scripts/user_callable/hosts/update_cluster_runtime.sh` before first use. See
[`scripts/user_callable/guests/prod/README.md`](scripts/user_callable/guests/prod/README.md#online-root-disk-growth).

### Creating or Removing a Staging VM

A staging VM is a disposable copy of a production VM in one of its staging
slots (`stageKprodN`, with its own address and FQDN). There are two kinds; both
count toward a host's staging quota, run only where no production VM runs, and
are evicted when a production VM starts on their host.

A **prod-linked staging VM** is a linked clone of a production VM's current
state. Preview and then create one (the Staging page's **Add prod-linked
Staging VM**):

```bash
./scripts/user_callable/guests/staging/add_staging_vm.sh --dry-run
./scripts/user_callable/guests/staging/add_staging_vm.sh
```

The script selects an eligible standby placement host and creates a stopped
`stageNprodN` linked clone. Use `--sanitizer /absolute/path/to/script.sh` when
application-specific data or configuration must be changed before guest
networking starts. Review and start the guest when prompted.

A **backup-linked staging VM** boots one of a production VM's restore points
on PBS within about a minute (**Instant Restore to Backup-Linked Staging VM**,
on the Staging and Backups pages):

```bash
./scripts/user_callable/guests/staging/add_backup_linked_staging_vm.sh --snapshot vm/100/2026-10-27T16:04:23Z
```

Its disk is the restore point, mapped read-only from PBS on its host, with a
local overlay that keeps its writes; it is patched with the slot's identity
before it boots, and takes `--sanitizer` too. The restore point's data is
never copied, so reads of data the VM has not changed come from PBS and are
slower than local disk (tens of MB/s): it suits looking at an earlier state
or testing against it, not load. The production VM need not be running. Its
host's overlays live in the directory storage `bmac-overlay`
(`/var/lib/bmac-overlay` on each host's root filesystem), which the first one
creates. Besides a production start, it is also evicted when its PBS mapping
dies, for example when its host reboots.

Destroy staging guests as soon as testing is complete: a prod-linked one's
snapshot keeps pinning production pool space, and a backup-linked one's
overlay grows with what it writes. The same command removes both kinds:

```bash
./scripts/user_callable/guests/staging/remove_staging_vm.sh --dry-run stage1prod1
./scripts/user_callable/guests/staging/remove_staging_vm.sh stage1prod1
```

Omit `stage1prod1` to select a registered staging guest interactively.

### Deploying the Example Application

After creating a production VM and configuring the Cloudflare origin
certificate described above, run:

```bash
./scripts/user_callable/guests/prod/deploy_hello_app_to_prod.sh
```

The script prompts for a registered `prodN` and certificate/key paths, checks
direct SSH, builds the example todo app on your workstation, and installs it on
the VM as `hello-todo.service`, which starts at boot. The app is one static
Rust binary that serves HTTPS itself with the origin certificate and keeps up
to 100 todos in a SQLite file, with a JSON API under `/api/v1/todos`. It is an
example deployment, not a general BMAC application deployer, and runs on a
Linux workstation only. Install its build tools once:

```bash
rustup target add x86_64-unknown-linux-musl
sudo apt install musl-tools
```

The app's source is in `scripts/user_callable/guests/prod/hello_todo/`; run its
tests there with `cargo test`.

The app answers `GET /api/v1/healthz` with `200 {"status":"ok"}` after a read
from its database (`503 {"status":"error"}` otherwise). With monitoring
enabled, the script adds `hello-todo.service` and its certificate to the VM's
monitored services; turn on the QDevice's probe of the health endpoint as
described in [Monitoring your apps](#monitoring-your-apps).

### Backing Up Production VMs

Backups go to a Proxmox Backup Server (PBS) through Proxmox VE's own PBS
integration. Set up the PBS server, its per-cluster namespace and token, and
its retention policy first, following
[`docs/PBS_MANUAL_SETUP.md`](docs/PBS_MANUAL_SETUP.md). Then put the
`PROXMOX_PBS_*` settings in `config/cluster.conf` and the token secret
(`PROXMOX_PBS_TOKEN`) in `config/secrets.env`, run
`update_cluster_runtime.sh`, and use the Dashboard's **Backups** page:

- **Setup PBS** adds the cluster's PBS storage `bmac-pbs` with a new
  encryption key and your master public key, and saves a copy of the key for
  you to store offline. Keep that copy: without it, or the master private
  key, the cluster's backups cannot be restored.
- **Backup schedule** backs a production VM up every 1, 2, 3, 4, 6, 8, 12 or
  24 hours; **Back up now** makes one more backup straight away.
- **Protect a restore point** keeps one however old it gets, for example
  before a risky change.
- **Full Restore to Existing VM** returns a production VM to one of its
  restore points, keeping its identity; it is down until the restore
  finishes. A safety backup of its current state is offered first.
- **Instant Restore to Existing VM** does the same as Full Restore to
  Existing VM, but the VM is down only until it boots again from the restore
  point, usually a minute or two. Its data is copied back to its own disk
  while it runs, its disk is switched over without a restart, and its
  replication is recreated. Until then, reads of data not yet copied are
  slower, it cannot fail over, and it cannot be the source of a prod-linked
  staging VM. If it fails partway, run it again for the same VM to resume.
- **Full Restore to New VM** creates a new production VM from a restore
  point, for example on a rebuilt cluster, with a new identity when the
  original still exists.
- **Instant Restore to New Permanent VM** does the same as Full Restore to
  New VM, but the VM serves its FQDNs within minutes, from the restore point
  on PBS, while its data is copied to its own disk in the background. Then
  its disk is switched over without a restart, and it gets replication and
  HA. Until then, reads of data not yet copied are slower, it cannot fail
  over, and it cannot be the source of a prod-linked staging VM.
- **Instant Restore to Backup-Linked Staging VM** boots a restore point as a
  disposable staging VM within about a minute, without copying its data
  (see [Creating or Removing a Staging VM](#creating-or-removing-a-staging-vm)).

The page shows the PBS server's retention policy (read-only), each VM's
schedule and newest backup, and every restore point BMAC made. With
monitoring enabled, each host also alerts when it can't reach PBS
(`pbs:storage`), and the host running a scheduled VM alerts when the VM's
newest backup is more than two scheduled runs old (`backup:<prodN>`). To be
alerted about the PBS server itself, install its monitor by hand; see
[Monitoring the PBS server](#monitoring-the-pbs-server). How long
backups are kept is set on the PBS server, not by BMAC. The terminal
equivalents are under `scripts/user_callable/backups/`, and the design is in
[Backup and restore (PBS)](docs/MAIN_DESIGN.md#backup-and-restore-pbs).

### Updating BMAC Runtime Code on Existing Hosts

When checked-in runtime helpers or hooks have changed, preview drift and then
deploy the validated bundle atomically to every configured online node:

```bash
./scripts/user_callable/hosts/update_cluster_runtime.sh --dry-run
./scripts/user_callable/hosts/update_cluster_runtime.sh
```

The second command requires `GO`; use `--yes` only in controlled automation.
This updates BMAC runtime code, not Proxmox packages or host configuration,
with one exception: it also upgrades every host's `cloudflared` to the newest
version in Cloudflare's apt repository (the tunnel replicas run with
`--no-autoupdate`, so nothing else upgrades them). Hosts are upgraded one at
a time: a host's replica is restarted only while another host's replica is
connected, and the next host waits until the upgraded one has reconnected, so
ingress keeps working. If an upgraded replica does not reconnect, its previous
version is reinstalled (when the repository still has it) and no other host
is touched. The dry run lists each host's installed and available version;
`--skip-cloudflared` leaves cloudflared alone. Run the update when Cloudflare
asks you to upgrade cloudflared.

The update also installs the configuration parser (`config.sh`) on every
host. Run it before syncing a `cluster.conf` that uses a newly added key
(such as the `PROXMOX_PBS_*` keys): a host's older parser rejects keys it
doesn't know, so the sync keeps that host's previous `cluster.conf` and stops.

With `MONITORING_ENABLED=true`, the same command also brings monitoring up to
date on every node (see [Monitoring and Alerts](#monitoring-and-alerts)):
before changing anything it checks that every host, the QDevice, and every
active production VM is reachable over SSH and that `HEALTHCHECKS_API_KEY`
works; after the hosts are updated it creates or updates the cluster's
Healthchecks checks and installs the monitor agent, its unit, and its config
on every node, then waits for every check's first ping. The dry run shows the
checks it would create, update, or find obsolete. Checks that no node uses any
more are deleted only if you agree (or pass `--delete-obsolete-checks`). With
`MONITORING_ENABLED=false`, agents installed earlier are stopped.

### Changing the VM Slots, Guest Gateway, Transfer Caps, or Tunnel

The VM slot settings (`VM_SLOT_START`, `MAX_PROD_VMS`,
`MAX_STAGING_VMS_PER_PROD`), the shared guest gateway (`GUEST_GATEWAY_IP`,
`GUEST_GATEWAY_MAC`), the tunnel (`CLOUDFLARE_*`), and each host's optional
monthly transfer cap (`PROXMOX_HOST_TRANSFER_MONTHLY_*` in `config/moxN.conf`)
are normally set once, before the first host is installed. If you change
them later, push them to every host:

```bash
./scripts/user_callable/hosts/sync_cluster_network.sh --dry-run
./scripts/user_callable/hosts/sync_cluster_network.sh
```

The dry run lists what differs on each host. The second command requires
`GO`; it refuses slot changes that would move a registered VM, then updates
every host's guest gateway, egress rules, transfer cap, probe targets, tunnel
credentials, and tunnel config, restarting tunnel replicas one host at a time.
In the dashboard this is the **Sync cluster with cluster.conf** host
operation.

### Adding More Storage: Adding a Mirrored Vdev

1. Physically install two disks of the same nominal capacity; their exact
   byte capacities may differ by up to 1%.
2. Confirm that both appear unused in:

   ```bash
   ./scripts/user_callable/hosts/inventory_disks.sh --host mox1
   ```

3. Add them as one top-level mirror:

   ```bash
   ./scripts/user_callable/hosts/add_new_disk_vdev.sh --host mox1
   ```

The script asks which disks to use and rechecks their identity immediately
before erasing them. On an encrypted host, follow its instructions at the
physical or iDRAC console to apply the existing shared rpool passphrase; the
passphrase never crosses SSH. The script updates crypttab and initramfs before
adding the mirror. BMAC supports at most five rpool mirrors including the boot
mirror.

After an encrypted addition, wait for the operation to complete, gracefully
migrate production guests away, and perform the instructed manual reboot test.
Then inspect the host again with `show_proxmox_host_state.sh`.

### Replacing a Failed Mirror Member

`show_cluster_health.sh` reports which host and drive serial failed. First
inspect the failed vdev with `inventory_disks.sh` and `zpool status rpool` on
the host. This workflow handles a member whose failed disk has
already been physically removed; it deliberately refuses a failed disk that
is still installed.

1. Pull the failed disk and install a replacement with exactly the same byte
   capacity as the surviving member.
2. Run:

   ```bash
   ./scripts/user_callable/hosts/add_replacement_disk.sh --host mox1
   ```

3. Select the degraded mirror and replacement disk, then follow any console
   instructions for the shared LUKS passphrase.
4. Monitor the background resilver with:

   ```bash
   ./scripts/user_callable/diagnostics/show_proxmox_host_state.sh --host mox1
   ```

For a boot-mirror member, the script also copies the partition layout and
creates and verifies a registered ESP. After resilvering finishes, gracefully
migrate production guests away and perform the instructed manual reboot test.
An interrupted replacement can be resumed by rerunning the same command.

### Decommissioning Unnecessary Disk Capacity

1. Delete unnecessary files inside every production VM that runs on or
   replicates to the target host.
2. Destroy related staging VMs with `remove_staging_vm.sh`; their clones and
   snapshots can pin blocks that must be freed.
3. Start one non-boot vdev removal:

   ```bash
   ./scripts/user_callable/hosts/decommission_disks.sh --host mox1
   ```

The script validates staging state, trims affected production filesystems,
forces replication, scrubs rpool, asks how much free space must remain (at
least 50 GiB), and offers only eligible non-boot vdevs. ZFS evacuation then
runs in the background.

Run the inventory repeatedly to check progress:

```bash
./scripts/user_callable/hosts/inventory_disks.sh --host mox1
```

When evacuation is complete, the inventory identifies the exact disks and
asks permission to close their LUKS mappings, update crypttab/initramfs,
release the disks (erase only their LUKS key slots, ZFS labels, signatures,
and partition table; no data wipe), and mark them retired. Physically pull
only disks shown under **Disks eligible for safe physical removal**.
Run `decommission_disks.sh` once per vdev.

## Monitoring and Alerts

BMAC can alert you, by email and optionally as push notifications on your phone, when anything in the cluster goes wrong: a host, the QDevice, or a production VM stops working or loses its internet connection; hosts can't reach each other over Tailscale or the private VLAN; an app can't be reached through Cloudflare; a disk reports SMART problems or wears out; a ZFS pool is degraded or nearly full; Proxmox's own alert email is stuck; a host can't reach the backup server or a production VM's backups are overdue; a monitored service on a production VM stops, its disk fills, or its certificate nears expiry; or the cluster loses quorum or the QDevice's vote. Staging VMs are never monitored. The full design is in [`docs/MONITORING_AND_ALERTING.md`](docs/MONITORING_AND_ALERTING.md).

### How it works

You get alerts from three independent sources, all by email:

1. **BMAC's monitor agents through Healthchecks.io.** Every Proxmox host, the QDevice, and every active production VM runs a small agent (`app-ha-monitor.service`) bound to that node's own Healthchecks.io check. The agent pings its check every minute. If a node dies or loses its internet connection, the pings stop and Healthchecks emails you that the check is **DOWN** (the "dead-man switch"). When an agent finds a problem it sends a *pulse*: a failure ping that describes the problem, immediately followed by a success ping that clears it again. You receive a DOWN email describing the problem and an UP email that your Gmail filters delete. Because each pulse is cleared at once, the check stays up and the dead-man switch keeps working while problems are open. Each pulse lists what is NEW, REMINDER (every 6 hours while a problem stays open), RESOLVED, and ONGOING, so every email is a complete picture.
2. **Cloudflare's Tunnel Health Alert**, which reports when the cluster's tunnel is degraded or down.
3. **Proxmox's own email** (smartd, ZFS, fencing, replication), which BMAC leaves unchanged as a separate path.

It costs nothing: Healthchecks.io's free Hobbyist plan allows 20 checks, and Pushover (optional) is a one-time purchase per platform. BMAC uses one check per monitored node, so your cluster must satisfy **hosts + 1 (the QDevice) + production VMs ≤ 20** (or `MONITOR_MAX_CHECKS`). No component accepts inbound connections; everything only makes outbound requests.

The QDevice is required while monitoring is enabled: it is the only node outside the data center and the only one that can test your apps end to end through Cloudflare. It runs the agent even when the cluster has an odd number of hosts and doesn't use its vote.

### Setting up Healthchecks.io

1. Create an account and a project at [healthchecks.io](https://healthchecks.io).
2. In the project's settings, create a **read-write API key** and put it in `config/secrets.env` as `HEALTHCHECKS_API_KEY`. Only your workstation uses it; nodes never receive it, only their own check's ping URL.
3. Keep the project's default email integration. Its recipient must be the (Google Workspace) address your Gmail filters run on.
4. In your account settings, **turn on email reminders** for down checks, and **turn off the monthly summary report** (it lists every check by name, so the forwarding filter below would forward it).
5. Make sure the Tailscale ICMP rule for the QDevice is in your policy (see [Instructions for Tailscale Setup](#instructions-for-tailscale-setup)).
6. Set `MONITORING_ENABLED=true` in `config/cluster.conf`, then preview and deploy:

    ```bash
    ./scripts/user_callable/hosts/update_cluster_runtime.sh --dry-run
    ./scripts/user_callable/hosts/update_cluster_runtime.sh
    ```

    The dry run shows the checks it will create (named `<cluster> bmaccheck <node>`, such as `MyAppCloud bmaccheck mox1`), the budget, and every node that gets the agent. Every host, the QDevice, and every active production VM must be reachable over SSH (`ssh prodN`, set up by `setup_jump_ssh_access.sh`) before anything changes. The update then installs the agent a phase at a time (hosts, the QDevice, production VMs), rolling a phase back if one of its nodes fails, and waits until every check has received a ping, because a check that never received one never alerts.
7. Create the Gmail filters and Pushover addresses below, then test the whole chain:

    ```bash
    ./scripts/user_callable/diagnostics/send_test_alert.sh --node mox1 --priority p1
    ./scripts/user_callable/diagnostics/send_test_alert.sh --node qdevice --priority p2
    ```

    Then test the dead-man switch: run `ssh prod1 systemctl stop app-ha-monitor`, wait about four minutes for the DOWN email, and start it again (`systemctl start app-ha-monitor`) to get the UP email.

Check monitoring at any time with `./scripts/user_callable/diagnostics/show_monitoring_state.sh`: every node's check status and last ping, every agent's open problems, checks that no node uses any more, and the check budget.

Monitoring follows the cluster's membership: adding or removing a host, the QDevice, or a production VM updates every agent and the checks. Removing a host or production VM deletes its check. Removing the QDevice from its quorum role keeps its check and agent, because it remains the cluster's external monitor. If a membership change can't update monitoring (for example because Healthchecks is unreachable), the change itself still completes and tells you to run `update_cluster_runtime.sh` to finish.

To turn monitoring off, set `MONITORING_ENABLED=false` and run `update_cluster_runtime.sh`: it stops and disables every agent and offers (after you type `DELETE CHECKS`) to delete the cluster's checks. Checks you keep will report every node as down.

To retire the QDevice as the cluster's monitor without turning monitoring off (for example when replacing the machine), run `systemctl disable --now app-ha-monitor` on it, then delete its check (`<cluster> bmaccheck qdevice`) in the Healthchecks UI. The next `update_cluster_runtime.sh` recreates both on the configured QDevice.

### Optional monitoring settings

These settings are not in `config/cluster_dot_conf`. Add any of them to your `config/cluster.conf` to change the default:

| Key | Default | Meaning |
|---|---|---|
| `HEALTHCHECKS_API_URL` | `https://healthchecks.io` | Healthchecks instance (https, no trailing slash) |
| `MONITOR_MAX_CHECKS` | `20` | Most checks BMAC lets the Healthchecks project hold (1-1000) |
| `MONITOR_ALIVE_PERIOD_SECONDS` | `60` | Each check's period: how often agents ping (at least 60) |
| `MONITOR_ALIVE_GRACE_SECONDS` | `180` | Grace time before a silent node is reported down (at least 60) |
| `MONITOR_FAST_INTERVAL_SECONDS` | `30` | How often connectivity and service checks run (10-300) |
| `MONITOR_FAST_FAIL_THRESHOLD` | `3` | Consecutive failures (and passes) before a connectivity problem opens (or resolves) (1-10) |
| `MONITOR_SLOW_INTERVAL_SECONDS` | `1800` | How often SMART, ZFS, capacity, filesystem, and certificate checks run (300-86400) |
| `MONITOR_POOL_MIN_FREE_PERCENT` | `10` | A pool or filesystem below this free space is a problem (1-50) |
| `MONITOR_POOL_CLEAR_FREE_PERCENT` | `12` | ...and is resolved only above this (greater than the minimum) |
| `MONITOR_REMINDER_HOURS` | `6` | Hours between reminders of an open problem (1-168) |
| `MONITOR_POSTFIX_QUEUE_MAX_AGE_MINUTES` | `60` | Mail waiting longer than this in a host's queue is a problem (10-1440) |
| `MONITOR_CERT_WARN_DAYS` | `14` | Warn this many days before a monitored certificate expires (1-90) |
| `MONITOR_NVME_WEAR_WARN_PERCENT` | `90` | Warn when an NVMe drive has used this much of its rated endurance (50-100) |
| `MONITOR_PROBE_DEFAULT_PATH` | `/api/v1/healthz` | The path the probe editor suggests |
| `MONITOR_PROBE_DEFAULT_STATUS` | `200` | The status the probe editor suggests (100-599) |

### Monitoring your apps

A new production VM is monitored as soon as it is created (its check proves the VM is alive and its egress works), but nothing inside it is watched until you deploy an app:

- **Services and certificates.** Add your app's systemd services and its certificate files with the Production page's **Monitored services** tile, or `./scripts/user_callable/guests/prod/manage_prod_vm_monitoring.sh prod1 --add-service myapp.service --add-cert /etc/myapp/cert.pem`. A watched service that isn't active is a high-priority alert. `deploy_hello_app_to_prod.sh` adds the example app's service and certificate for you.
- **A health endpoint.** Give your app a cheap endpoint under `/api/` (which BMAC's Cloudflare setup never challenges) that checks its own dependencies, like the example app's `GET /api/v1/healthz`, which answers `200 {"status":"ok"}` or `503 {"status":"error"}`. Then turn on its probe with the QDevice page's **Health-check probes** tile, or `./scripts/user_callable/qdevice/manage_qdevice_probes.sh --vm prod1 --enable --path /api/v1/healthz --status 200`. The QDevice then requests it through Cloudflare every minute; the tool runs the probe once before saving.

### Monitoring the PBS server

The PBS server isn't part of any cluster, so no BMAC workflow touches it. You can run the same monitor agent on it, in its `pbs` role, with its own Healthchecks check. It pings every minute, so you hear when the server goes down, and every 30 minutes it checks:
- the pool's health, free space, and last scrub;
- the SMART attributes of the pool's disks;
- the `proxmox-backup` services;
- that the datastore is mounted;
- the results of garbage collection, prune, and verify runs;
- that each cluster's namespace received a backup recently.

It finds the disks from the pool itself, and it works the same whether PBS is a VM with passed-through disks ([`PBS_MANUAL_SETUP.md`](docs/PBS_MANUAL_SETUP.md) appendix A) or a physical machine. The details are in [`docs/MONITORING_AND_ALERTING.md`](docs/MONITORING_AND_ALERTING.md#64-pbs-server-role-pbs).

1. **Create the check.** Use a Healthchecks project whose email goes to your alert address: a cluster's project or a separate one. The check counts toward that project's 20 checks. Run this on your workstation, from this repository's root, with the project's read-write API key. It keeps the check's ping URL in `PBS_PING_URL` for step 3 and prints it; run steps 1 to 3 in the same terminal:

    ```bash
    export HC_API_KEY='<the project read-write API key>'
    PBS_PING_URL="$(curl -sS -X POST https://healthchecks.io/api/v3/checks/ \
      -H "X-Api-Key: $HC_API_KEY" -H 'Content-Type: application/json' \
      -d '{"name": "PBS bmaccheck pbs", "slug": "pbs-bmaccheck-pbs", "tags": "bmac bmac-role-pbs",
           "desc": "BMAC monitor of the PBS server. Installed by hand; no cluster manages it.",
           "timeout": 60, "grace": 180, "channels": "*", "unique": ["name"]}' |
      python3 -c 'import json, sys; t = sys.stdin.read(); r = json.loads(t) if t.lstrip().startswith("{") else {"error": t.strip() or "no answer"}; print(r["ping_url"]) if "ping_url" in r else sys.exit("Healthchecks refused: " + str(r.get("error", r)))')"
    echo "$PBS_PING_URL"
    ```

    Running it again returns the same check (`"unique": ["name"]`), so it's safe to repeat if you lose the URL.

    Keep `bmaccheck` in the name so your Gmail filters route its emails. Don't give it a `bmac-cluster-...` tag: that cluster's `update_cluster_runtime.sh` would delete it as a check no node uses.

2. **Copy the agent** from this repository to the PBS server:

    ```bash
    scp scripts/host_runtime/app_ha_monitor.py pbs:/tmp/
    ssh pbs 'install -D -m 0755 /tmp/app_ha_monitor.py /usr/local/lib/app-ha-monitor/app_ha_monitor.py && rm /tmp/app_ha_monitor.py'
    ```

    It needs Python 3.11 or newer (PBS 4 has 3.13) and `smartctl` (`apt install smartmontools` if it's missing). Repeat this step, then `systemctl restart app-ha-monitor`, when BMAC's agent changes.

3. **Write its config.** First set `PBS_NAMESPACES` to the namespaces that should be receiving backups. Then paste the whole block, from the same terminal as step 1. It writes `/etc/app-ha-monitor/config.json` on the PBS server, readable only by root, and prints it:

    ```bash
    PBS_NAMESPACES='["ExampleNamespace1"]'
    [ -n "${PBS_PING_URL:-}" ] || echo 'PBS_PING_URL is empty: run step 1 in this terminal first'
    [ -n "${PBS_PING_URL:-}" ] && PBS_CONFIG="$(python3 -c 'import json, sys; print(json.dumps({
        "schema_version": 1, "cluster": "PBS", "node": "pbs", "role": "pbs", "ping_url": sys.argv[1],
        "pbs": {"pool": "pbs", "datastore": "pbs", "namespaces": json.loads(sys.argv[2]), "tbw_rating_tb": 2880},
      }, indent=2))' "$PBS_PING_URL" "$PBS_NAMESPACES")" && printf '%s\n' "$PBS_CONFIG" |
      ssh pbs 'install -d -m 0700 /etc/app-ha-monitor && umask 077 &&
        cat > /etc/app-ha-monitor/config.json && chmod 0600 /etc/app-ha-monitor/config.json &&
        cat /etc/app-ha-monitor/config.json'
    ```

    - `cluster` and `node` only label the emails.
    - **`namespaces`:** a namespace alerts when its newest complete backup is older than 8 hours, which suits clusters backing up every 4 hours or more often. Don't list a namespace that receives no backups yet: it would alert at once. For a cluster that backs up less often, give its namespace a longer limit: two of its backup intervals plus an hour. For example, if a second cluster with namespace `ExampleNamespace2` backs up daily, use `'["ExampleNamespace1", {"name": "ExampleNamespace2", "max_age_hours": 49}]'`. To change the default for every namespace, add `"namespace_max_age_hours"` to the `pbs` section.
    - **`tbw_rating_tb`:** the disks' rated terabytes written, from their datasheet (2,880 for the Samsung 870 QVO 8TB). Remove that line to skip the TBW check.
    - **Optional settings:**
      - `scrub_max_age_days` (default 40);
      - `thresholds` such as `pool_min_free_percent` (10), `pool_clear_free_percent` (12), `reminder_hours` (6), and `nvme_wear_warn_percent` (90, which also applies to these disks' wear);
      - `intervals`, such as `slow` (1800 seconds).

4. **Check it once**, which sends nothing - run these from the pbs host:

    ```bash
    python3 /usr/local/lib/app-ha-monitor/app_ha_monitor.py validate-config
    python3 /usr/local/lib/app-ha-monitor/app_ha_monitor.py check --once
    ```

    Every line should start with `ok`. A `smart-read:` failure in a VM usually means the LUN passthrough lacks `rawio='yes'` or the host's AppArmor `sys_rawio` rule (appendix A.4 and A.5).

5. **Install the service**, which starts at boot - run these from the pbs host:

    ```bash
    python3 /usr/local/lib/app-ha-monitor/app_ha_monitor.py print-unit --role pbs \
      --program /usr/local/lib/app-ha-monitor/app_ha_monitor.py > /etc/systemd/system/app-ha-monitor.service
    systemctl daemon-reload
    systemctl enable --now app-ha-monitor
    systemctl status app-ha-monitor
    ```

    Within a minute the check shows its first ping in Healthchecks.

6. **Test the alerts:**
   - From the pbs host, run `python3 /usr/local/lib/app-ha-monitor/app_ha_monitor.py test-pulse --priority p2` for a test email.
   - Then test the dead-man switch: run `systemctl stop app-ha-monitor`, wait about four minutes for the DOWN email, and start it again.

7. **Confirm the pool is scrubbed monthly.** Debian's ZFS packages scrub every pool on the second Sunday of each month, from a cron job or systemd timer. Check with `cat /etc/cron.d/zfsutils-linux` and `systemctl list-timers 'zfs-scrub*'`. If neither exists, run `systemctl enable --now zfs-scrub-monthly@pbs.timer`.

`python3 /usr/local/lib/app-ha-monitor/app_ha_monitor.py status` shows the agent's open problems. After you've dealt with a disk whose counters rose, accept its new counters with `app_ha_monitor.py reset-smart-baseline <serial>`.

### Cloudflare tunnel health alert

In the Cloudflare dashboard, under Notifications, add a notification of type **Tunnel Health Alert** for your cluster's tunnel, delivered by email to your alert address, for the **Degraded** and **Down** states. It reports only the connection between the tunnel replicas and Cloudflare, not whether your apps answer, which is why the QDevice also probes each app.

### Gmail filters

A few facts about Gmail filters first:

- Gmail applies **every** filter that matches a message, in no particular order, so these filters are written not to overlap.
- Filters search the whole message, including the subject and the ping body Healthchecks puts in its email.
- The filters don't use the sender's address. They match the word `bmaccheck`, which every BMAC email contains because it is part of every check's name. Healthchecks' subjects are `DOWN | <check name>` and `UP | <check name>`.
- Test each query in Gmail's search box first and make sure it finds the test emails you expect and nothing else.
- Filters only act on mail that arrives after they are created.
- Before creating a forwarding filter, add each Pushover address as a forwarding address (Settings → Forwarding and POP/IMAP → Add a forwarding address). Gmail sends a confirmation code to it, which Pushover shows on your phone. With Google Workspace, an administrator must first allow automatic forwarding for your account in the Google Admin console's Gmail settings.

Create these two filters (Settings → Filters and Blocked Addresses → Create a new filter → "Has the words"):

| # | Purpose | Has the words | Actions |
|---|---|---|---|
| F1 | A pulse being cleared (noise) | `bmaccheck bmacclear subject:up` | Delete it |
| F2 | Everything else about BMAC checks: a node went silent, an event, a node came back | `bmaccheck -bmacclear` | Never send it to Spam; Apply label `BMAC`; Forward to your Pushover address |

F1 also requires "up" in the subject so a DOWN email can never be deleted. The agent makes sure the last ping before a node could go silent never contains `bmacclear`, so a dead-man DOWN email always reaches F2.

**Optional priority split.** To get two Pushover priorities, replace F2 with these two (the agents mark every event `bmacp1`, high, or `bmacp2`, normal):

| # | Purpose | Has the words | Actions |
|---|---|---|---|
| F2a | High priority: p1 events, and nodes going silent | `bmaccheck -bmacclear -bmacp2` | as F2, forwarding to the high-priority Pushover address |
| F2b | Normal priority: p2 events | `bmaccheck -bmacclear bmacp2` | as F2, forwarding to the normal Pushover address |

Healthchecks' reminder emails (while a check is down) also contain `bmaccheck` and are forwarded, which is useful.

For the other two sources, add filters on words from their subjects, as you receive them, for example `subject:tunnel` for Cloudflare's alert and `subject:("SMART error")` for Proxmox's SMART email, each with: Never send it to Spam; label; forward to Pushover.

### Pushover

[Pushover](https://pushover.net) turns emails into push notifications; after a 30-day trial it is a one-time purchase per platform, and each user can receive up to 10,000 messages a month. The basic filter set needs one Pushover address: your account's default `@pomail.net` address. For the priority split, create two email aliases in the Pushover dashboard, one with high priority (and a distinct sound) and one with normal priority. Send a test email from Gmail to each address before setting up forwarding.

### Keeping Proxmox's own email out of spam

Proxmox delivers its alert email straight to your mail server from `root@<host>.pve.internal`, a domain that isn't public, so it can never pass SPF or DKIM, and Gmail shows an unverified-sender warning. To keep it out of spam with Google Workspace:

1. In the Google Admin console, open Apps → Google Workspace → Gmail → Spam, Phishing and Malware → **Email allowlist**, and add each Proxmox host's public IPv4 address.
2. Optionally, set the Proxmox notification "from" address to one on a domain you control, publish an SPF record listing the hosts' public IPs, and ask your provider for reverse DNS (PTR) records for them.

BMAC's host agents report stuck Proxmox email (the `postfix-queue` problem) and check the disks themselves, so a blocked Proxmox email path doesn't hide disk alerts.

To prove smartd's email path on a host (optional), temporarily change the `DEVICESCAN` line in `/etc/smartd.conf` to `DEVICESCAN -d removable -n standby -m root -M exec /usr/share/smartmontools/smartd-runner -M test`, run `systemctl restart smartmontools`, and expect one test email per disk (`journalctl -u smartmontools --since '-2 min'` lists the disks). Remove ` -M test` and restart again afterwards.

### Interpreting alerts

Each event lists problem keys such as `link0:mox2` or `ingress:prod1.example.com`.

| What you receive | Most likely meaning |
|---|---|
| Dead-man DOWN for one host, and other hosts report `link1:<host>` DOWN but not `link0:<host>` | The host is alive but lost its internet connection. Its guests fall back through another host; ingress continues through the other replicas. |
| Dead-man DOWN for one host, other hosts report both `link0` and `link1` to it DOWN, and the QDevice reports `tailscale:<host>` | The host is down or completely isolated. Check its iDRAC. HA restarts its production VMs elsewhere. |
| Every host reports `link0` to every peer DOWN; no dead-man alerts | Private VLAN failure. Corosync keeps running over Tailscale; replication fails (Proxmox emails too). |
| Hosts report `link1` DOWN to each other, the QDevice reports `tailscale:*`; no dead-man alerts | Tailscale problem. The cluster runs on the VLAN link; the QDevice's vote (if used) and your SSH access are affected. |
| The QDevice reports `tailscale:<host>` for every host at once, hosts are fine | Tailscale problem on the QDevice's side. |
| Dead-man DOWN for the QDevice; hosts report `qdevice-vote` (if a QDevice is registered) | The QDevice is down. With an even number of hosts, one more host failure loses quorum. |
| The QDevice reports `ingress:<fqdn>` only | That app isn't answering through Cloudflare (app down, origin certificate, tunnel route). Look for `service:*` from the VM and Cloudflare's tunnel email. |
| The QDevice reports `ingress:*` for every app, plus Cloudflare's tunnel Down email | No tunnel replica is connected, or Cloudflare has a problem. |
| Dead-man DOWN for a production VM only | The VM is down, or its egress is broken. |
| `postfix-queue` on a host | Proxmox's own alert email isn't leaving that host; BMAC's alerts are unaffected. |
| `smart:<serial>` saying a counter rose | The drive is degrading. Once you've dealt with it, accept its counters with `show_monitoring_state.sh --reset-smart-baseline <host>:<serial>`. |
| `pbs:storage` from every host | PBS is down or unreachable, its certificate changed (update `PROXMOX_PBS_FINGERPRINT` and the storage), or it rejects the cluster's token. Backups are failing. |
| `pbs:storage` from one host only | That host's own path to PBS is broken, for example the PBS firewall doesn't admit its address. |
| `backup:<prodN>` without `pbs:storage` | The VM's backup job isn't running or its backups fail. Check the job's task log in the Proxmox web UI (or **Back up now** on the Backups page) and Proxmox's own backup email. |
| PBS server: `namespace:<NS>` | A cluster sharing the PBS server hasn't sent a backup recently. That cluster's own monitoring (if any) reports why. |
| PBS server: `pbs-task:...` | A garbage collection, prune, or verify run failed; its log is under Administration > Tasks in the PBS web UI. A failed verify means some backup chunks are damaged. |
| PBS server: `smart:<serial>` saying a counter rose | The drive is degrading. Once dealt with, run `app_ha_monitor.py reset-smart-baseline <serial>` on the PBS server. |
| `agent:<check>` | One of the agent's own checks failed to run; the message says why. |

## Less Common Tasks

### Adding Another Host to the Cluster

Host names are slots `mox1` through `moxMAX_MOX_HOSTS`. The cluster registry
records which slots are in use, and setup recommends the lowest free slot, so
a slot freed by removing a host is reused first. Slots need not be
contiguous: a cluster of `mox1` and `mox3` is valid. Each host must fit the
host and replication ranges already configured in `config/cluster.conf`.

New hosts join through the control node (`PROXMOX_CONTROL_NODE` in
`config/cluster.conf`, normally `mox1`). Before a cluster exists, the control
node is the host that creates it; once it exists, the registry records the
authoritative control node and setup stops if `config/cluster.conf` disagrees.

1. Copy `config/mox1_dot_conf` or `config/mox2_dot_conf` to the new
   `config/moxN.conf` name (use the slot setup will recommend) and set the new
   host's disk, NIC, public/private network, iDRAC, capacity, and optional
   monthly transfer cap values. This file is used only for initial setup and retries before setup
   completes.
2. Ensure every existing cluster node is online and healthy:

   ```bash
   ./scripts/user_callable/diagnostics/show_cluster_state.sh
   ```

3. From a Debian-based workstation, run setup using the same storage policy
   as the existing hosts:

   ```bash
   ./scripts/user_callable/hosts/add_proxmox_host.sh --host mox3 --encrypt
   # or, for a clear-storage cluster:
   ./scripts/user_callable/hosts/add_proxmox_host.sh --host mox3 --no-encrypt
   ```

   Choose `--run-boot-tests` or `--skip-boot-tests` explicitly when desired.
   The setup workflow temporarily reconciles QDevice quorum while joining the
   node and requires all existing nodes to remain online.

4. Verify the completed cluster and synchronize the current BMAC runtime:

   ```bash
   ./scripts/user_callable/diagnostics/show_cluster_state.sh
   ./scripts/user_callable/hosts/update_cluster_runtime.sh --dry-run
   ./scripts/user_callable/hosts/update_cluster_runtime.sh
   ```

Adding the node does not automatically change any existing production VM's
placement.

### Migrating Workloads to Hosts with More Resources

To move production onto bigger hosts and retire the old ones, for example
from `mox1`/`mox2` to `mox3`/`mox4`:

1. Add the new hosts as described above.
2. Add them to each production VM's placement:

   ```bash
   ./scripts/user_callable/guests/prod/change_prod_vm_placement.sh
   ```

   Pick the VM (default: the lowest `prodN`), answer `a`, and list the new
   hosts. A host is offered only if it is online, holds fewer production VMs
   than its `MAX_PROD_VM_COUNT_ON_THIS_HOST` (from `config/moxN.conf`), and keeps
   10% of its pool size available after receiving the replica. The script
   records the wider placement in the registry, replicates the VM's disks to
   each new host, waits for a successful initial replication, and only then
   lets Proxmox HA use the host.
3. Move the VM onto a new host:

   ```bash
   ./scripts/user_callable/guests/prod/change_prod_vm_owner.sh
   ```

   It reads the live HA owner (refreshing a stale registry owner), checks that
   HA, replication, and the registry agree, offers the other placement hosts,
   replicates the latest changes, and runs the equivalent of
   `ha-manager relocate vm:100 mox3`. It then waits for the VM to start there,
   checks that replication now runs from the new owner, and records the new
   owner in the registry. Staging VMs on the new owner are stopped and
   destroyed when the production VM starts there, and the script warns first.

   After an HA failover the VM stays on the host it restarted on, and the
   guest hook records that host as its owner. If the dashboard still reports
   that a production VM runs on a different host than its registry owner,
   `./scripts/user_callable/guests/prod/reconcile_prod_vm_owner.sh` records
   where it runs without moving it.
4. Remove the old hosts from the placement by running
   `change_prod_vm_placement.sh` again and answering `r`. It never removes the
   live owner and keeps at least two placement hosts. HA stops using a host
   first, then the registry placement narrows, then the replication job and
   its replica are deleted.
5. Retire each old host with `scripts/user_callable/hosts/remove_proxmox_host.sh` (see below).

Both production scripts refuse a VM that has staging VMs derived from it;
destroy those staging VMs first. If either script stops partway, rerun it: the
placement script detects an unfinished change and offers to finish it, and the
owner script records the live owner before offering a new one. Check each
stage with:

```bash
./scripts/user_callable/diagnostics/show_cluster_state.sh
./scripts/user_callable/diagnostics/show_prod_vm_state.sh prod1
```

### Removing a Host from the Cluster

```bash
./scripts/user_callable/hosts/remove_proxmox_host.sh
```

The script removes a host gracefully when it can be contacted, and forcefully
when it cannot. It lists every host with how it can be removed. It first
checks whether the QDevice is accessible; if it is not, it continues only
when an odd number of hosts remains, which needs no QDevice. The cluster must
be quorate with every other host online.

#### Graceful removal

A host that is online and answers SSH from this workstation is removed
gracefully. It is offered only if no registry record references it (no
production placement, no staging VM, no pending cleanup), it holds no guest,
and no HA rule or replication job names it. At least two hosts must remain.

The script asks you to type `REMOVE moxN`. It then removes the QDevice, stops
the host's Cloudflare Tunnel replica (the other hosts' replicas keep serving
every FQDN, so nothing changes in Cloudflare), powers the host off, deletes it from the cluster, removes its SSH trust, and adds the
QDevice back only when the remaining host count is even, so the vote count
stays odd. Finally it frees the slot in the registry and archives
`config/artifacts/hosts/moxN`. If you remove the control node, it asks which remaining host becomes the new control
node, records it, and offers to update `PROXMOX_CONTROL_NODE` in
`config/cluster.conf`. Every other workstation must set the same value.

Never boot the removed host on the cluster network with its old disks. Wipe
or reinstall it first; the script lists the remaining cleanup (Tailscale
device, `known_hosts`, `config/moxN.conf`, Cloudflare).

#### Forced removal

A host that cannot be contacted is removed forcefully: an offline member that
does not answer SSH, a leftover `/etc/pve/nodes` directory, or a registry
slot that is stale or never finished joining. The script explains why it
cannot remove the host gracefully and asks whether to remove it forcefully.
Answer no if the host is only temporarily unreachable, bring it back online,
and rerun to remove it gracefully. The script refuses a host that Proxmox
sees online but that does not answer SSH, or that answers SSH while Proxmox
sees it offline.

Forced removal shows this warning:

> The purpose of a forced removal is to enable the removal of a cluster host
> that is no longer functioning and has been physically disconnected from
> the cluster, permanently. The removed machine must never be allowed to
> communicate with the cluster via the network in any way after it has been
> removed from the cluster.

Proxmox HA must already have restarted any production VM that ran on the dead
host elsewhere. The script refuses a production VM placed only on the dead
host, and any guest it cannot account for. After showing the plan, it asks you
to physically disconnect the machine from every network, remove it from the
Tailscale admin console (Machines), and make sure it never connects to a
network again in its old role. Confirm that by typing
`moxN IS PERMANENTLY DISCONNECTED`.

It then destroys staging VMs on the dead host or derived from production VMs
that used it, narrows each affected production VM's HA rule, replication, and
registry placement to the surviving hosts, abandons the dead host's deferred
cleanup, deletes the node from the cluster, keeps the vote count odd with the
QDevice, frees the slot, and archives the host's artifacts. A forced removal
may leave a single host; the production VMs then have one placement host
until you add hosts back with `change_prod_vm_placement.sh`. Like a graceful
removal, it selects a new control node when the removed host held that role.

A graceful removal interrupted after it powered the host off is finished by
rerunning the script, which then removes the host forcefully.

### Replacing a Failed QDevice

A cluster with an even number of hosts needs the external QDevice's vote to
survive losing a host. Check it with:

```bash
./scripts/user_callable/diagnostics/show_qdevice_state.sh
```

If the report says the registered QDevice is inaccessible and has failed:

1. Remove it:

   ```bash
   ./scripts/user_callable/qdevice/remove_qdevice.sh
   ```

   The script cannot remove an inaccessible QDevice gracefully, so it offers
   to remove it forcefully, without contacting it. You must first remove the
   old machine from the Tailscale admin console, so it can never communicate
   with the cluster again, and type `REMOVED FROM TAILSCALE`.
   The script then runs `pvecm qdevice remove` and removes the QDevice client,
   its certificates, and its pinned SSH host key from every Proxmox host.
   Every host must be online.
2. Run `ssh-keygen -R qdevice` on your workstation.
3. Prepare a new machine with sections 1-15 of
   [`docs/QDEVICE_MANUAL_SETUP.md`](docs/QDEVICE_MANUAL_SETUP.md), and
   check that `ssh qdevice` works.
4. Add it:

   ```bash
   ./scripts/user_callable/qdevice/add_qdevice.sh
   ```

   It confirms the cluster needs a QDevice, asks for the QDevice hostname,
   and checks SSH access. It then installs `corosync-qnetd`, trusts the new
   host key on every Proxmox host, and runs `pvecm qdevice setup`, as host
   setup does. Finally it checks that every host sees the QDevice voting.

Until step 4 finishes, losing any host of an even cluster loses quorum, so
replace the QDevice promptly. Host setup and `scripts/user_callable/hosts/remove_proxmox_host.sh`
remove an inaccessible QDevice the same forced way when a membership change needs it removed, and stop before any
change when the cluster would need a QDevice afterward. When the QDevice is
reachable, `remove_qdevice.sh` removes it gracefully, uninstalls its QDevice
software, and leaves a plain Ubuntu machine, still in Tailscale, that
`add_qdevice.sh` can add again.

### Wiping a Test Cluster and Starting Over

If you've already set up a test cluster with BMAC and want to wipe it and set
it all up again from scratch, this is the easiest sequence:

1. Run `./scripts/user_callable/qdevice/remove_qdevice.sh` to reset the existing QDevice back to a
   plain, non-Proxmox machine. Every Proxmox host must still be online and
   reachable over SSH when you do this.
2. In the Tailscale admin web UI, under "Machines", remove your current
   Proxmox hosts (typically `mox1` and `mox2`). Leave the QDevice there so
   you can reuse it for the new cluster. `scripts/user_callable/hosts/add_proxmox_host.sh`
   configures it when it sets up the second Proxmox host, so the cluster has
   3 quorum votes.
3. Also in the Tailscale admin web UI, generate a new auth key for each
   Proxmox host of the new cluster. Each key must be non-reusable,
   non-ephemeral, and tagged `tag:proxmox-host` (see
   [Instructions for Tailscale Setup](#instructions-for-tailscale-setup)).
4. Run `./scripts/user_callable/hosts/add_proxmox_host.sh` for each Proxmox host. You can run it
   for `mox1` and `mox2` concurrently. It will likely find the setup artifacts
   left from the earlier test cluster; since you're setting up a new
   cluster, tell it to delete them and start from the beginning.  These
   script executions will ask for the Tailscale auth keys you generated above.

## A Basic CI/CD Workflow

A simple CI/CD workflow can be made for most webapps by doing the following

You need to make a deploy script that can target a host using SSH to access that host.  This deploy script, if it finds that the target host has never been configured, should do initial configuration such as by updating the OS and installing things like NGINX etc.  This deploy script should be able to build all of your webapp's assets and copy them to the target host.  Prior to building, your deploy script should create a new tag in version control (usually git) of the code it's building.  This tag, in some form, should accompany the assets that get copied to the target host, so that it's always clear on that target host the exact code that was deployed, and when assets are deployed to a staging host the prior deployment tag value should be preserved on the staging host before the current deployment tag value is updated, so that for any staging host you can always determine the prior and current tag values - the prior tag value is the tag value of the assets that were deployed to production, and the current tag value is the tag value of the updated code that overwrote production's assets on your staging VM.  Your deploy script should also have a mode where it can be told of a staging VM that has passed tests, and that you wish that staging VM's code changes to be pushed to production.

1. Create your production VM (a one-time step for your webapp) by running `scripts/user_callable/guests/prod/add_prod_vm.sh`
2. Test your webapp locally.  When it passes your tests, run your deploy script targetting the new production VM (ex: `prod1`)

Then for any future updates to production, do the following:

1. Build your webapp and test it locally on your own dev workstation.  When your local tests pass the next step is to test in staging.
2. Create a temorary staging VM using `scripts/user_callable/guests/staging/add_staging_vm.sh`
3. Run your deploy script targetting the new staging VM (ex: `stage1prod1`)
4. Test your app in staging.  If it passes your tests then call your Deloy script with parameters sufficient to tell it that you wish to push staging to production, specifying which staging VM passed tests.  The depoy code must then look at the staging VM's prior tag and ensure that it still matches the production VM's current tag, and if they match then the deploy code must checkout the code at the staging VM's current tag and build that code and deploy it to production, updating production's old current tag with this new current tag.  This way you only ever push to production changes that have been made directly against both production's prior code state AND its data state.
5. Cleanup: Remove the staging VM using `scripts/user_callable/guests/staging/remove_staging_vm.sh`

## Why not use Kubernetes (K8s)?

Kubernetes is primarily a container-orchestration system. It assumes you already have machines, networking, storage, and a working cluster underneath it. BMAC operates at that lower infrastructure layer: it provisions and manages the bare-metal hosts, ZFS storage and replication, quorum, networking, ingress, and VM-level high availability.

Kubernetes also doesn't, by itself, solve persistent-storage or database failover. On bare metal you still need to choose and operate the storage system, and stateful applications generally need additional database-aware HA tooling. BMAC takes a simpler approach for applications that fit comfortably on one machine: the entire production VM, including its application and database state, is replicated and failed over as a single unit.

BMAC also provides a production-derived staging model that Kubernetes does not provide out of the box. Replicated production storage on a standby host can be turned into a lightweight Copy-on-Write staging VM, patched with a unique machine identity, tested independently, and discarded when no longer needed. The standby hardware therefore remains useful during normal operation while still being available for production failover.

BMAC doesn't prevent you from using Kubernetes. If your application benefits from containers, horizontal scaling, service orchestration, or Kubernetes deployment tooling, you can run Kubernetes inside VMs hosted by BMAC. The two systems solve different layers of the problem.

## Developing BMAC

To work on BMAC itself from a Mac, including a Lima VM that runs the complete test suite and the known limits of that setup, see [`dev/README.md`](dev/README.md).

## History

This project was worked on, tested and used privately, with tons of commits, before being migrated to this public repo.  Our hope is to help others while garnering support from the community for self-built cloud infrastructure.

## Future

We have many more improvements planned for BMAC, including support for integrated CEPH.  For FiberState deployments, BMAC could be improved further if FiberState adds Internet ingress/egress capability from the VLAN, which would let guests reach the internet without going through a Proxmox host.  More details on low-hanging-fruit changes that FiberState could make are in [`docs/FUTURE.md`](docs/FUTURE.md).
