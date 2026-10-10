# <img src="dashboard/src-tauri/icons/32x32.png" alt="BMAC"> BMAC - Build My App Cloud

## Intro

Sick of paying out-the-nose for hyperscaler cloud hosting? Trying to find your way out of their pricing labyrinth? Disillusioned with the complexity and the proprietary cloud APIs needed to glue-together a serverless architecture and their lock-in effect?

You're not the only one who noticed that the public clouds aren't really necessary for many webapps, even at scale.  Now that a single x64 server can host 1024 concurrent threads with a dual-socket AMD EPYC 9996 machine, all sharing the same RAM, the vertical-scaling ceiling is very high, which means that horizontal-scaling often isn't necessary.  That's a cloud-in-a-box.  For most apps you'll never need more, but you don't want to start with such an expensive box.  What you need is the ability to start with affordable hardware, suitable for your initial workload, with an easy way to migrate to better hardware as you grow, with minimal downtime, and with data-redundancy and high-availablity-failover baked-in so your data's safe.  You also need a system for testing upcoming changes to production without actually affecting production.  That's what BMAC delivers.

BMAC, at its heart, is a library of [user-callable scripts](./scripts/user_callable/).  However these scripts can also be called by a machine with the --json parameter, which changes how the scripts' inputs and outputs operate.  The BMAC Dashboard GUI performs the bulk of its work using this script library, called in machine mode.

## Screenshots

Screenshots of the BMAC Dashboard are [here](docs/DASHBOARD_SCREENSHOTS.md).

## Releases

BMAC releases represent tested, known-good points in the project's development and are the recommended versions for new installations.

The current source code on the `main` branch may contain changes that have not yet gone through the complete release testing process. For production use, download the latest release from the [GitHub Releases](https://github.com/OtterFlip/bmac/releases) page.

Each release is provided by GitHub as both `.zip` and `.tar.gz` source archives. BMAC's scripts don't not require a build step - download or extract the release on your x64 Ubuntu administrator workstation and follow the setup instructions below.  However if you want to use the BMAC Dashboard then you'll need to build it following [its instructions](./dashboard/README.md#running-it).

## BMAC's Features

- Grokable.  With moderate reading, BMAC can be understood without spending weeks to ramp up.  The complete design is [here](docs/MAIN_DESIGN.md).
- Simple App Architecture.  Your webapp's deploy target environment is the same environment as your dev workstation - a computer.  Build and test your webapp all on your own computer and then deploy your built webapp to your BMAC production guest VM (ex: prod1).  BMAC supports many webapps so you can use BMAC to extend the cluster to support them all.  This enables you to use monolithic architecture which is one of the simplest app architectures.
- Test Using Actual Production State Without Affecting Production.  This is [one of the killer features of BMAC](docs/STAGING.md), which handles all the setup steps for you.  Your standby host is not useless as it sits there waiting to act as a failover host.  Instead, it's actively leveraged to host your staging VMs, which are the VMs you push new changes to in order to test those changes prior to pushing them to the production VM.  Since all of the data for the production VM's disk is replicated by ZFS to the standby host(s), that data, on a standby host, can be used as the source for a temporary staging VM without any additional data copy.  This is done by making a temporary ZFS snapshot (removed when the staging VM itself is removed), and making a linked-clone of that snapshot on the standby host. The linked-clone of the snapshot is a fast and lightweight operation, using Copy-on-Write (CoW), and this linked clone acts as the hard disk for the new staging VM. Prior to attaching the linked clone to the staging VM, the linked clone is mounted on the standby host and its contained EXT4 filesystem is patched in order to alter the machine identity (IP, MAC address, hostname, machine ID, etc) of the machine so that the new staging VM doesn't collide with the production VM it's derived from.  An optional custom script can also be added to perform application-specficic patching, if desired.  Your staging VMs are visible with functional HTTPS access via a browser. Typically you'll want to have a custom app-specific patch which flips settings on your webapp in the staging VM to cause it to be in 'staging mode' such as to limit who can login, etc., as your staging VM can be fully accessible on the Internet for HTTPS browser access.  Your webapp can also look at the hostname - if it begins with 'stage' then your app knows it's running on a staging host, and can change its behavior.  During a failover event for a production VM, any staging VMs on the failover host are automatically stopped and removed, in order to ensure that the production VM has sufficient resources (RAM, CPU).
- Failover With Little Downtime.  Your production VM is a High-Availability (HA) guest/VM, so all of your production VM's disk data is replicated using ZFS async replication as often as once-per-minute from your production guest VM's primary host to the other placement hosts in the cluster.  If the primary host dies then the production VM is automaticlaly started on a standby host.  So failover downtime is the time to boot the VM and start its services.  Since the ZFS replication is async and at most once-per-minute, at most 1 minute of DB transactions may be lost in a critical host failure scenario, so BMAC is not suitable if your webapp requires zero data loss during failure events.  Email alerts are sent on both the failure and recovery.
- Scale Up with Little Downtime.  When your resource needs grow beyond the capacity of your intial hardware, add new hardware to your cluster and increase the 'placement' of your production VM to those new machines, which will cause all of the VM's data to be replicated, using ZFS, to those new cluster hosts.  When the data is fully replicated you can then migrate the VM to run on the higher-capacacity hosts and decomission the lower-capacity servers. Flipping that switch is a one-time reboot time cost of the production VM, so the downtime for such a migration, an uncommon event itself, is on the order of a few minutes at most.
- Full At-Rest Disk Encryption.  Many webapps need to satisfy security requirements and so BMAC has the option to configure all cluster hosts with disk-level encryption using LUKS.  When this option is enabled, you must enter a password on the hosts during host boot.  All mirror members for all mirror sets (each mirror is called a vdev) share the same password, so only one password must be entered at boot time.
- Software RAID1, Including OS Boot Volume.  Even if your servers don't have a hardware RAID controller, BMAC will configure them with ZFS RAID1, mirroring each set of identical-capacity disks. During host setup, BMAC also offers the option to fully-test failure scenarios for each mirror member of the boot disk RAID1 set in order to prove host bootability.  BMAC also ensures that each boot disk has a mirrored EFI System Partition (ESP), in addition to mirroring the ZFS partition.  For servers that support host-swap of NVMe disks, this enables each server to endure a single-member disk failure for each mirror set without any downtime or data loss.  Simply pull the failed disk, insert an identical-capacity replacment, and resilver the mirror set.
- Dell iDRAC9 IPMI support.  BMAC has been tested on Dell Poweredge R640 'Dual Intel Xeon Gold 6138 40 core/80 thread' hosts at FiberState, and one of the host setup script options is to use iDRAC to simplify hardware discovery. This is not required, and a hardware discovery script is provided in cases where you're using some other type of host.
- BMAC supports decommissioning disk capacity you no longer need. The disk workflows trim affected guests, force replication, scrub the pool, evacuate an eligible non-boot vdev, finalize its retirement, and identify the exact disks that are safe to remove physically. This can be useful after data has moved elsewhere and a host no longer needs all of its installed capacity.

## What You Need to Use BMAC

- 2 servers with UEFI BIOS, each with their own public IP and Internet gateway connection on a primary NIC, and a private network connection between these two servers on a secondary NIC on each (can be a VLAN).  We prefer [FiberState's 'Dedicated Server' option](https://www.fiberstate.com/dedicated-servers) (we have no affiliation, we're just a satisfied customer as FiberState is competitively-priced, particularly if you pay for a year in advance).  These will be your main cluster hosts, hosting the guest VMs which in turn host your webapp(s).  You can add more than 2 servers if you like, but start with 2.
- Each server must have at least two physical NVMe hard disks of the same nominal capacity (their exact byte capacities may differ by up to 1%) for the ESP and OS partitions
- 1 ultra-cheap VPS host with a public IP address, such as a $5/month Shared CPU Nanode 1GB VPS at Linode.  This node's sole purpose will be to play the role of a QDevice, to vote in your cluster quorum to break tie votes.
- A developer workstation running a debian-based Linux distro (we made and tested this with x64 Ubuntu) to run BMAC's scripts.  This is required for the host setup scripts, but the prod/staging scritps work on macOS too.
- A Tailscale account (the free account will work fine)
- Your app's domain (ex: 'myapp.com'), managed at Cloudflare (not essential, but our guides assume this)

## BMAC's Stack

- Proxmox
- LUKS
- ZFS 
- Corosync
- HAProxy
- VRRP+keepalived
- Cloudflare
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
- Follow the Cloudflare instructions to setup your website's domain for active/passive load-balancing.  This will cost you ~$5/month.

If you've completed the above steps then you're now ready to setup your Proxmox cluster hosts.  From a Debian-based (such as Ubuntu) workstation, run the `scripts/user_callable/hosts/add_proxmox_host.sh` script and choose to setup `mox1`.  A Debian-based workstation is required for the Proxmox host setup because that script must create a custom Proxmox installer ISO, and the Proxmox project only provides a Debian-based helper tool for this task.  Follow the `scripts/user_callable/hosts/add_proxmox_host.sh` script's instructions.  You  can actually run the same script in another terminal session to setup `mox2` at the same time, however I recommend at the point where the scipt asks you if you have verified that the VLAN (or private network connedtion) is working, that you only let one script at a time proceed past that point rather than letting many run beyond that point concurrently.

After our cluster is configured, you should be able to access the Proxmox admin web UI at:  [https://mox1:8006/](https://mox1:8006/) or [https://mox2:8006/](https://mox2:8006/)

You can now use the `scripts/user_callable/guests/prod/add_prod_vm.sh` script to create your first production VM to host your application, which will deploy a blank/vanilla Ubuntu x64 OS to that VM and setup SSH access, and nothing else.  You can always tear it back down if you're not satisfied with it using `scripts/user_callable/guests/prod/remove_prod_vm.sh`.  If you like, you can also deploy a test "hello world" static webapp to your new production VM using `scripts/user_callable/guests/prod/deploy_hello_app_to_prod.sh`.

After creating a production VM, you can test creation and teardown of a staging VM, based on that production VM, using the `scripts/user_callable/guests/staging/add_staging_vm.sh` and `scripts/user_callable/guests/staging/remove_staging_vm.sh` scripts.

### Instructions for Cloudflare Setup


1. Under Domains | Overview click on your app's domain (such as mydomain.com), then under Domains | DNS, add a new DNS record with these settings:
    Name: www
    Type: A
    IPv4 Address: 192.0.2.1
    Proxied: Turned On


2. Create two Load Balancing monitors, one for the prod pool and one for the staging pool.  Each of these two pools will typically just have the single endpoint - the IP of the proxmox host you assign as the owner of your prod VM, and the IP of the proxmox host that's acting as the standby for your prod VM, respectively. Traffic will be sent to the pool associated with this monitor in your load-balancer.  Within the Proxmox cluster, that traffic will then be directed to the prod VM which must be setup to respond to HTTP /healthz GET requests with 200 (and "ok" response body) for requests to your mydomain.com domain. The sample app deploy script, `deploy_hello_app_to_prod.sh`, will set up NGINX on a prod VM to respond to /healthz GET requests in this fashion. If you're still on the domains page then click "Back to Domains" to see the whole left-side menu and then under "Delivery & performance" | "Load Balancing" use the "Monitors" tab and create one monitor each for mox1 and mox2 (2 monitors total) with these settings (note that if you haven't yet enabled Load Balancing on your account, which costs ~$5/month, you'll need to enable it):

    ```
    Name: prod-via-prod-host-http-healthz
    Type: HTTP
    Path: /healthz
    Port: 80

    Name: prod-via-stage-host-http-healthz
    Type: HTTP
    Path: /healthz
    Port: 80
    ```

3. Now under "Load Balancing" on the "Pools" tab create one pool each for mox1 and mox2 (2 pools total) with these settings:

    ```
    Pool Name: prod
    Pool Description: Proxmox prod pool
    Endpoint Steering: Random
    Endpoint Name: mox1
    Endpoint Address: enter the public IP address of your mox1 host here
    Port: empty
    Weight: 1
    Enabled: checked
    Health Threshold: 1
    Monitor (select the prod-via-prod-host-http-healthz monitor you already made)
    Health Check Regions: All Data Centers
    Health Check Notification: checked, either
    Notification Email: enter your email address here

    Pool Name: stage
    Pool Description: Proxmox stage pool
    Endpoint Steering: Random
    Endpoint Name: mox2
    Endpoint Address: enter the public IP address of your mox2 host here
    Port: empty
    Weight: 1
    Enabled: checked
    Health Threshold: 1
    Monitor (select the prod-via-stage-host-http-healthz monitor you already made)
    Health Check Regions: All Data Centers
    Health Check Notification: checked, either
    Notification Email: enter your email address here
    ```

4. Now back on the "Load Balancing" page on the "Load Balancers" tab you need to create two load balancers, one for the traffic to the prod VM on the primary/prod host, and the other for traffic to possible staging guests on the standby/stage host, for a total of 2 load balancers, with these settings.  PAY VERY CLOSE ATTENTION to the Hostename field and note that for the staging hostname you must use the *.mydomain.com format:

    ```
    Hostname: mydomain.com
    proxy (orange checkbox): CHECKED
    load balancer description: mydomain.com for prod
    Session Affinity: UNCHECKED
    Adaptive Routing: UNCHECKED
    Pools: Add prod pool and then stage pool in that order (this order is CRITICAL)
    Fallback Pool: mox2
    Attached Monitors should be set automatically since you already set them for the pools
    Traffic Steering: Off - this is what you want as this will do active/passive failover for you
    Custom Rules: none

    Hostname: *.mydomain.com
    proxy (orange checkbox): CHECKED
    load balancer description: *.mydomain.com for staging
    Session Affinity: UNCHECKED
    Adaptive Routing: UNCHECKED
    Pools: Add stage pool and then prod pool in that order (this order is CRITICAL)
    Fallback Pool: mox1
    Attached Monitors should be set automatically since you already set them for the pools
    Traffic Steering: Off - this is what you want as this will do active/passive failover for you
    Custom Rules: none
    ```

5. Now Under Domains (click on your domain) | Rules | Overview, use "Create Rule" and choose "Redirect Rule" and in its settings choose these:

    ```
    Rule Name: Redirect from http/https www.mydomain.com to https mydomain.com
    CHECK "Wildcard pattern"
    Request URL: http*://www.mydomain.com/*
    Target URL: https://mydomain.com/${2}
    Status code: 301 - Permanent Redirect
    UNCHECKED "Preserve query string"
    Place at: First
    ```

6. Under Domains (click on your domain) | SSL/TLS | Origin Server | Origin Certificates click 'Create a certificate' with these settings:

    ```
    Private Key type: RSA (2048)
    Hostnames: mydomain.com and *.mydomain.com
    Validity: 15 years
    Save the certificate to a .cer file on your workstation.  This is the public key.
    Save the private key to a .key file in the same dir as the .cer file.  This is the private key.  Never share it.
    Both of these files will be fed to the `scripts/user_callable/guests/prod/deploy_hello_app_to_prod.sh` script.
    ```

7. Under Domains (click on your domain) | SSL/TLS | Overview click "Configure" and select "Full (Strict)"

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
```

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
./scripts/user_callable/diagnostics/list_guests.sh --kind production # registered VMs with HA, replication, and routes
./scripts/user_callable/diagnostics/list_replication.sh --guest prod1
./scripts/user_callable/diagnostics/list_storage.sh --host mox1      # ZFS pools and Proxmox storage
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
configures HA, and publishes its routes. It can safely resume its own
incomplete allocation after a failure.

To permanently remove a production VM, inspect the exact destruction plan
first:

```bash
./scripts/user_callable/guests/prod/remove_prod_vm.sh --dry-run prod1
./scripts/user_callable/guests/prod/remove_prod_vm.sh prod1
```

This removes the VM, its HA and replication configuration, routes, owned
volumes, and registry allocation. It is intentionally destructive.

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

### Creating or Removing a Production-Derived Staging VM

Preview and then create a staging VM from a selected production VM:

```bash
./scripts/user_callable/guests/staging/add_staging_vm.sh --dry-run
./scripts/user_callable/guests/staging/add_staging_vm.sh
```

The script selects an eligible standby placement host and creates a stopped
`stageNprodN` linked clone. Use `--sanitizer /absolute/path/to/script.sh` when
application-specific data or configuration must be changed before guest
networking starts. Review and start the guest when prompted.

Destroy staging guests as soon as testing is complete so their snapshots do
not continue pinning production pool space:

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
direct SSH, installs or updates NGINX, and configures the production and
possible staging hostnames. It is an example deployment, not a general BMAC
application deployer.

### Updating BMAC Runtime Code on Existing Hosts

When checked-in runtime helpers or hooks have changed, preview drift and then
deploy the validated bundle atomically to every configured online node:

```bash
./scripts/user_callable/hosts/update_cluster_runtime.sh --dry-run
./scripts/user_callable/hosts/update_cluster_runtime.sh
```

The second command requires `GO`; use `--yes` only in controlled automation.
This updates BMAC runtime code, not Proxmox packages or host configuration.

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

## Less Common Tasks

### Adding Another Host to the Cluster

Host names are slots `mox1` through `moxMAX_MOX_HOSTS`. The cluster registry
records which slots are in use, and setup recommends the lowest free slot, so
a slot freed by removing a host is reused first. Slots need not be
contiguous: a cluster of `mox1` and `mox3` is valid. Each host must fit the
host, replication, and HAProxy ranges already configured in
`config/cluster.conf`.

New hosts join through the control node (`PROXMOX_CONTROL_NODE` in
`config/cluster.conf`, normally `mox1`). Before a cluster exists, the control
node is the host that creates it; once it exists, the registry records the
authoritative control node and setup stops if `config/cluster.conf` disagrees.

1. Copy `config/mox1_dot_conf` or `config/mox2_dot_conf` to the new
   `config/moxN.conf` name (use the slot setup will recommend) and set the new
   host's disk, NIC, public/private network, iDRAC, capacity, and HAProxy
   values. This file is used only for initial setup and retries before setup
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
production placement, no staging VM, no pending cleanup), it holds no guest
other than its own HAProxy container, and no HA rule or replication job names
it. At least two hosts must remain.

The script asks you to type `REMOVE moxN`. It then removes the QDevice, powers
the host off, deletes it from the cluster, removes its SSH trust, and adds the
QDevice back only when the remaining host count is even, so the vote count
stays odd. Finally it frees the slot in the registry, refreshes HAProxy
routes, and archives `config/artifacts/hosts/moxN`. Take the host out of every
Cloudflare load balancer first; the script asks you to confirm this. If you
remove the control node, it asks which remaining host becomes the new control
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

We have many more improvements planned for BMAC, including support for integrated CEPH.  For FiberState deployments, BMAC could be improved significantly if FiberState adds Internet ingress/egress capability from the VLAN.  This would drammatically-simplify the BMAC implementation, doing away with the need for HAProxy and VRRP+keepalived for ingress/egress routing, and the complex coordination and syncing these currently require.  More details on low-hanging-fruit changes that FiberState could make are in [`docs/FUTURE.md`](docs/FUTURE.md).
