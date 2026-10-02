# BMAC - Build My App Cloud

## Intro

<img src="media/BMAC.png" align="left" width="260" hspace="20" vspace="10" alt="BMAC">

Sick of paying out-the-nose for hyperscaler cloud hosting? Trying to find your way out of their pricing labyrinth? Disillusioned with the complexity and the proprietary cloud APIs needed to glue-together a serverless architecture and their lock-in effect?

You're not the only one who noticed that the public clouds aren't really necessary for many webapps, even at scale.  Now that a single x64 server can host 1024 concurrent threads with a dual-socket Epyc 9996 motherboard, all sharing the same RAM, the vertical-scaling ceiling is very high, which means that horizontal-scaling often isn't necessary.  That's a cloud-in-a-box.  For most apps you'll never need more, but you don't want to start with such an expensive box.  What you need is the ability to start with affordable hardware, suitable for your initial workload, with an easy way to migrate to better hardware as you grow, with minimal downtime, and with data-redundancy and high-availablity-failover baked-in so your data's safe.  You also need a system for testing upcoming changes to production without actually affecting production.  That's what BMAC delivers.

<br clear="left">

## BMAC's Features

- Grokable.  With moderate reading, BMAC can be understood without spending weeks to ramp up.  The complete design is [here](MAIN_DESIGN.md).
- Simple App Architecture.  Your webapp's deploy target environment is the same environment as your dev workstation - a computer.  Build and test your webapp all on your own computer and then deploy your built webapp to your BMAC production guest VM (ex: prod1).  BMAC supports many webapps so you can use BMAC to extend the cluster to support them all.  This enables you to use monolithic architecture which is one of the simplest app architectures.
- Test Using Actual Production State Without Affecting Production.  This is [one of the killer features of BMAC](STAGING.md), which handles all the setup steps for you.  Your standby host is not useless as it sits there waiting to act as a failover host.  Instead, it's actively leveraged to host your staging VMs, which are the VMs you push new changes to in order to test those changes prior to pushing them to the production VM.  Since all of the data for the production VM's disk is replicated by ZFS to the standby host(s), that data, on a standby host, can be used as the source for a temporary staging VM without any additional data copy.  This is done by making a temporary ZFS snapshot (removed when the staging VM itself is removed), and making a linked-clone of that snapshot on the standby host. The linked-clone of the snapshot is a fast and lightweight operation, using Copy-on-Write (CoW), and this linked clone acts as the hard disk for the new staging VM. Prior to attaching the linked clone to the staging VM, the linked clone is mounted on the standby host and its contained EXT4 filesystem is patched in order to alter the machine identity (IP, MAC address, hostname, machine ID, etc) of the machine so that the new staging VM doesn't collide with the production VM it's derived from.  An optional custom script can also be added to perform application-specficic patching, if desired.  Your staging VMs are visible with functional HTTPS access via a browser. Typically you'll want to have a custom app-specific patch which flips settings on your webapp in the staging VM to cause it to be in 'staging mode' such as to limit who can login, etc., as your staging VM can be fully accessible on the Internet for HTTPS browser access.  Your webapp can also look at the hostname - if it begins with 'stage' then your app knows it's running on a staging host, and can change its behavior.  During a failover event for a production VM, any staging VMs on the failover host are automatically stopped and removed, in order to ensure that the production VM has sufficient resources (RAM, CPU).
- Failover With Little Downtime.  Your production VM is a High-Availability (HA) guest/VM, so all of your production VM's disk data is replicated using ZFS async replication as often as once-per-minute from your production guest VM's primary host to the other placement hosts in the cluster.  If the primary host dies then the production VM is automaticlaly started on a standby host.  So failover downtime is the time to boot the VM and start its services.  Since the ZFS replication is async and at most once-per-minute, at most 1 minute of DB transactions may be lost in a critical host failure scenario, so BMAC is not suitable if your webapp requires zero data loss during failure events.  Email alerts are sent on both the failure and recovery.
- Scale Up with Little Downtime.  When your resource needs grow beyond the capacity of your intial hardware, add new hardware to your cluster and increase the 'placement' of your production VM to those new machines, which will cause all of the VM's data to be replicated, using ZFS, to those new cluster hosts.  When the data is fully replicated you can then migrate the VM to run on the higher-capacacity hosts and decomission the lower-capacity servers. Flipping that switch is a one-time reboot time cost of the production VM, so the downtime for such a migration, an uncommon event itself, is on the order of a few minutes at most.
- Full At-Rest Disk Encryption.  Many webapps need to satisfy security requirements and so BMAC has the option to configure all cluster hosts with disk-level encryption using LUKS.  When this option is enabled, you must enter a password on the hosts during host boot.  All mirror members for all mirror sets (each mirror is called a vdev) share the same password, so only one password must be entered at boot time.
- Software RAID1, Including OS Boot Volume.  Even if your servers don't have a hardware RAID controller, BMAC will configure them with ZFS RAID1, mirroring each set of identical-capacity disks. During host setup, BMAC also offers the option to fully-test failure scenarios for each mirror member of the boot disk RAID1 set in order to prove host bootability.  BMAC also ensures that each boot disk has a mirrored EFI System Partition (ESP), in addition to mirroring the ZFS partition.  For servers that support host-swap of NVMe disks, this enables each server to endure a single-member disk failure for each mirror set without any downtime or data loss.  Simply pull the failed disk, insert an identical-capacity replacment, and resilver the mirror set.
- Dell iDRAC9 IPMI support.  BMAC has been tested on Dell Poweredge R640 'Dual Intel Xeon Gold 6138 40 core/80 thread' hosts at FiberState, and one of the host setup script options is to use iDRAC to simplify hardware discovery. This is not required, and a hardware discovery script is provided in cases where you're using some other type of host.
- BMAC supports decommissioning disk capacity you no longer need. The disk workflows trim affected guests, force replication, scrub the pool, evacuate an eligible non-boot vdev, finalize its retirement, and identify the exact disks that are safe to remove physically. This can be useful after data has moved elsewhere and a host no longer needs all of its installed capacity.

## What You Need to Use BMAC

- 2 servers with UEFI BIOS, each with their own public IP and Internet gateway connection on a primary NIC, and a private network connection between these two servers on a secondary NIC on each (can be a VLAN).  We prefer [FiberState's 'Dedicated Server' option](https://www.fiberstate.com/dedicated-servers) (we have no affiliation, we're just a satisfied customer as FiberState is competitively-priced, particularly if you pay for a year in advance).  These will be your main cluster hosts, hosting the guest VMs which in turn host your webapp(s).  You can add more than 2 servers if you like, but start with 2.
- Each server must have at least two identical-capacity physical NVMe hard disks for the ESP and OS partitions
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
- Read the top of `env/secrets_dot_env` and make a copy of it as the instructions direct, to `env/secrets.env` and choose your password values.  If you're using iDRAC then make sure all of your hosts use the same iDRAC password and set it in your `env/secrets.env` file.
- Read the top of `env/mox1_dot_conf` and `env/mox2_dot_conf` and make a copy of each, to `env/mox1.conf` and `env/mox2.conf` which you should fill in with your actual host values.
- Set appropriate values in `env/cluster.conf`, `env/mox1.conf`, and `env/mox2.conf` for your hardware and network. The example values are for hosts with 128GB RAM, two identical 2TB NVMe disks, and 40 cores/80 threads, with a shared private network on `10.213.0.0/24` and a separate public IP and Internet gateway per host. Adjust all network, storage, CPU, and RAM values for your environment and expected production/staging workload. Determine the serial numbers of the NVMe disks for the initial boot mirror. iDRAC can provide the byte capacity values (if you have already set the serial number values - which you can obtain visually in the iDRAC console or by running `hosts/inventory_disks` in a Live Linux CD env) and lets `hosts/setup_proxmox_host.sh` determine capacities; without iDRAC, boot the target into a reachable Live/Trial Linux environment and run `hosts/inventory_disks.sh --host moxN` from the administrator workstation to obtain exact serial and byte-capacity values for the host's `env/moxN.conf`. Also set `PROXMOX_PUBLIC_MAC` and `PROXMOX_SECONDARY_MAC` to the permanent MAC addresses of the host's primary and secondary NICs.
- Download the Proxmox VE x64 installer ISO to your dev workstation (the machine from which you'll be running `hosts/setup_proxmox_host.sh`) and set its path and SHA256 hash value in `env/cluster.conf` for PROXMOX_ISO_FILE_FULL_PATH and PROXMOX_ISO_FILE_SHA256.
- Add your (and another colleague's) SSH public key to the ADMIN_1_PUBLIC_SSH_KEY and ADMIN_2_PUBLIC_SSH_KEY values in `env/cluster.conf`
- Follow the instructions in `qdevice/QDEVICE_MANUAL_SETUP.MD` to set up your qdevice and add it to your tailscale network. Before doing that you'll need to follow the Tailscale setup steps below to add the appropriate tailscale tags and access policies.
- Follow the Cloudflare instructions to setup your website's domain for active/passive load-balancing.  This will cost you ~$5/month.

If you've completed the above steps then you're now ready to setup your Proxmox cluster hosts.  From a Debian-based (such as Ubuntu) workstation, run the `hosts/setup_proxmox_host.sh` script and choose to setup `mox1`.  A Debian-based workstation is required for the Proxmox host setup because that script must create a custom Proxmox installer ISO, and the Proxmox project only provides a Debian-based helper tool for this task.  Follow the `hosts/setup_proxmox_host.sh` script's instructions.  You  can actually run the same script in another terminal session to setup `mox2` at the same time, however I recommend at the point where the scipt asks you if you have verified that the VLAN (or private network connedtion) is working, that you only let one script at a time proceed past that point rather than letting many run beyond that point concurrently.

After our cluster is configured, you should be able to access the Proxmox admin web UI at:  [https://mox1:8006/](https://mox1:8006/) or [https://mox2:8006/](https://mox2:8006/)

You can now use the `guests/prod/create_prod_vm.sh` script to create your first production VM to host your application, which will deploy a blank/vanilla Ubuntu x64 OS to that VM and setup SSH access, and nothing else.  You can always tear it back down if you're not satisfied with it using `guests/prod/destroy_prod_vm.sh`.  If you like, you can also deploy a test "hello world" static webapp to your new production VM using `app/deploy_hello_app_to_prod.sh`.

After creating a production VM, you can test creation and teardown of a staging VM, based on that production VM, using the `guests/staging/create_staging_vm.sh` and `guests/staging/destroy_staging_vm.sh` scripts.

### Instructions for Cloudflare Setup


1. Under Domains | Overview click on your app's domain (such as mydomain.com), then under Domains | DNS, add a new DNS record with these settings:
    Name: www
    Type: A
    IPv4 Address: 192.0.2.1
    Proxied: Turned On


2. Create a Load Balancing monitor for mox1 and mox2. If you're still on the domains page then click "Back to Domains" to see the whole left-side menu and then under "Delivery & performance" | "Load Balancing" use the "Monitors" tab and create one monitor each for mox1 and mox2 (2 monitors total) with these settings (note that if you haven't yet enabled Load Balancing on your account, which costs ~$5/month, you'll need to enable it):

    ```
    Name: mox1-mydomain-http-healthz
    Type: HTTP
    Path: /healthz
    Port: 80

    Name: mox2-mydomain-http-healthz
    Type: HTTP
    Path: /healthz
    Port: 80
    ```

3. Now under "Load Balancing" on the "Pools" tab create one pool each for mox1 and mox2 (2 pools total) with these settings:

    ```
    Pool Name: mox1
    Pool Description: Proxmox host mox1
    Endpoint Steering: Random
    Endpoint Name: mox1
    Endpoint Address: enter the public IP address of your mox1 host here
    Port: empty
    Weight: 1
    Enabled: checked
    Health Threshold: 1
    Monitor (select the mox1-mydomain-http-healthz monitor you already made)
    Health Check Regions: All Data Centers
    Health Check Notification: checked, either
    Notification Email: enter your email address here

    Pool Name: mox2
    Pool Description: Proxmox host mox2
    Endpoint Steering: Random
    Endpoint Name: mox2
    Endpoint Address: enter the public IP address of your mox2 host here
    Port: empty
    Weight: 1
    Enabled: checked
    Health Threshold: 1
    Monitor (select the mox2-mydomain-http-healthz monitor you already made)
    Health Check Regions: All Data Centers
    Health Check Notification: checked, either
    Notification Email: enter your email address here
    ```

4. Now back on the "Load Balancing" page on the "Load Balancers" tab you need to create two load balancers, one for the traffic to the prod guest on the primary host, and the other for traffic to possible staging guests on the standby host, for a total of 2 load balancers, with these settings:

    ```
    Hostname: mydomain.com
    proxy (orange checkbox): CHECKED
    load balancer description: mydomain.com load balancer
    Session Affinity: UNCHECKED
    Adaptive Routing: UNCHECKED
    Pools: Add mox1 and then mox2 in that order (this order is critical)
    Fallback Pool: mox2
    Attached Monitors should be set automatically since you already set them for the pools
    Traffic Steering: Off - this is what you want as this will do active/passive failover for you
    Custom Rules: none

    Hostname: *.mydomain.com
    proxy (orange checkbox): CHECKED
    load balancer description: *.mydomain.com load balancer
    Session Affinity: UNCHECKED
    Adaptive Routing: UNCHECKED
    Pools: Add mox2 and then mox1 in that order (this order is critical)
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
    Both of these files will be fed to the `app/deploy_hello_app_to_prod.sh` script.
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

// Initial QDevice Setup by Proxmox Setup (temporary)
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

Each time you set up a proxmox host, the script will prompt you for a unique Tailscale Auth Key.  You can generate in the Tailscale web UI unser "Settings | "Keys" and on that page under "Auth Keys" use the "Generate auth key..." button and give the key any name (typically something like `mox1`) of the key, make sure Reusable is Off, any expiration day count value is fine (as these will switch to never expiring once they're actually used), and keep Ephemeral Off, and most importantly turn Tags ON and select the tag:proxmox-host tag.  Generate the key and copy it somewhere so that you have it ready for when the `hosts/setup_proxmox_host.sh` script prompts for it.  You'll need one key for each proxmox host you setup.

Your SSH access from your dev workstation to your proxmox hosts will be via your Tailscale network.

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

To add two new disks to a host's ZFS pool later, run `hosts/add_new_disk_vdev.sh`. After a failed disk is pulled and an identical one installed, run `hosts/add_replacement_disk.sh` to put it back into its mirror. To give up a pair of disks, run `hosts/decommission_disks.sh`, then `hosts/inventory_disks.sh` until it finalizes the retirement and lists the disks as safe to pull. See [`hosts/README.md`](hosts/README.md#adding-and-decommissioning-rpool-disks) for the details, including the manual reboot test after adding encrypted disks.

## Common Tasks

Run these commands from the repository root on an administrator workstation.
Scripts that target a Proxmox host prompt for its `moxN` name and default to
`mox1`; pass `--host moxN` to select it non-interactively. Read every plan and
confirmation before allowing a destructive operation.

### Checking Cluster, Host, Guest, and Disk State

Use the read-only diagnostics before and after maintenance:

```bash
./diagnostics/show_cluster_state.sh
./diagnostics/show_proxmox_host_state.sh --host mox1
./diagnostics/show_prod_vm_state.sh prod1
./hosts/inventory_disks.sh --host mox1
```

`show_cluster_state.sh` reports cluster membership, quorum, networking,
services, storage, and registered guests. `show_proxmox_host_state.sh` gives a
detailed report for one host, and `show_prod_vm_state.sh` checks one production
VM across HA, replication, routing, storage, QGA, and SSH.

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
./guests/prod/create_prod_vm.sh --dry-run
./guests/prod/create_prod_vm.sh
```

The workflow allocates the next `prodN`, installs its OS, verifies guest
identity and SSH, creates replication to every selected placement host,
configures HA, and publishes its routes. It can safely resume its own
incomplete allocation after a failure.

To permanently remove a production VM, inspect the exact destruction plan
first:

```bash
./guests/prod/destroy_prod_vm.sh --dry-run prod1
./guests/prod/destroy_prod_vm.sh prod1
```

This removes the VM, its HA and replication configuration, routes, owned
volumes, and registry allocation. It is intentionally destructive.

### Growing a Production VM's Disk

After adding capacity to the ZFS pools of a production VM's placement hosts
(for example with `hosts/add_new_disk_vdev.sh`), pass some of it to the VM
without stopping it:

```bash
./guests/prod/extend_prod_vm_disk.sh --dry-run
./guests/prod/extend_prod_vm_disk.sh
```

Pick the VM (default: the first active `prodN`), make sure `ssh prodN` works,
and the script shows the largest increase that still keeps 10% of every
placement pool's total size available. You enter the increase in bytes, MiB,
or GiB. It then grows the zvol on the live HA owner, replicates the new size
to every placement host, and grows the guest's root partition and ext4
filesystem online. Deploy the updated registry with
`hosts/update_cluster_runtime.sh` before first use. See
[`guests/prod/README.md`](guests/prod/README.md#online-root-disk-growth).

### Creating or Removing a Production-Derived Staging VM

Preview and then create a staging VM from a selected production VM:

```bash
./guests/staging/create_staging_vm.sh --dry-run
./guests/staging/create_staging_vm.sh
```

The script selects an eligible standby placement host and creates a stopped
`stageNprodN` linked clone. Use `--sanitizer /absolute/path/to/script.sh` when
application-specific data or configuration must be changed before guest
networking starts. Review and start the guest when prompted.

Destroy staging guests as soon as testing is complete so their snapshots do
not continue pinning production pool space:

```bash
./guests/staging/destroy_staging_vm.sh --dry-run stage1prod1
./guests/staging/destroy_staging_vm.sh stage1prod1
```

Omit `stage1prod1` to select a registered staging guest interactively.

### Deploying the Example Application

After creating a production VM and configuring the Cloudflare origin
certificate described above, run:

```bash
./app/deploy_hello_app_to_prod.sh
```

The script prompts for a registered `prodN` and certificate/key paths, checks
direct SSH, installs or updates NGINX, and configures the production and
possible staging hostnames. It is an example deployment, not a general BMAC
application deployer.

### Updating BMAC Runtime Code on Existing Hosts

When checked-in runtime helpers or hooks have changed, preview drift and then
deploy the validated bundle atomically to every configured online node:

```bash
./hosts/update_cluster_runtime.sh --dry-run
./hosts/update_cluster_runtime.sh
```

The second command requires `GO`; use `--yes` only in controlled automation.
This updates BMAC runtime code, not Proxmox packages or host configuration.

### Adding More Storage: Adding a Mirrored Vdev

1. Physically install two disks with identical exact byte capacity.
2. Confirm that both appear unused in:

   ```bash
   ./hosts/inventory_disks.sh --host mox1
   ```

3. Add them as one top-level mirror:

   ```bash
   ./hosts/add_new_disk_vdev.sh --host mox1
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

First inspect the failed vdev with `inventory_disks.sh` and `zpool status
rpool` on the host. This workflow handles a member whose failed disk has
already been physically removed; it deliberately refuses a failed disk that
is still installed.

1. Pull the failed disk and install a replacement with exactly the same byte
   capacity as the surviving member.
2. Run:

   ```bash
   ./hosts/add_replacement_disk.sh --host mox1
   ```

3. Select the degraded mirror and replacement disk, then follow any console
   instructions for the shared LUKS passphrase.
4. Monitor the background resilver with:

   ```bash
   ./diagnostics/show_proxmox_host_state.sh --host mox1
   ```

For a boot-mirror member, the script also copies the partition layout and
creates and verifies a registered ESP. After resilvering finishes, gracefully
migrate production guests away and perform the instructed manual reboot test.
An interrupted replacement can be resumed by rerunning the same command.

### Decommissioning Unnecessary Disk Capacity

1. Delete unnecessary files inside every production VM that runs on or
   replicates to the target host.
2. Destroy related staging VMs with `destroy_staging_vm.sh`; their clones and
   snapshots can pin blocks that must be freed.
3. Start one non-boot vdev removal:

   ```bash
   ./hosts/decommission_disks.sh --host mox1
   ```

The script validates staging state, trims affected production filesystems,
forces replication, scrubs rpool, asks how much free space must remain (at
least 50 GiB), and offers only eligible non-boot vdevs. ZFS evacuation then
runs in the background.

Run the inventory repeatedly to check progress:

```bash
./hosts/inventory_disks.sh --host mox1
```

When evacuation is complete, the inventory identifies the exact disks and
asks permission to close their LUKS mappings, update crypttab/initramfs, and
mark them retired. Physically pull only disks shown under **Disks eligible for
safe physical removal**. Run `decommission_disks.sh` once per vdev.

## Less Common Tasks

### Adding Another Host to the Cluster

The new host must use the next contiguous name (`mox3` after `mox1` and
`mox2`), must not exceed `MAX_MOX_HOSTS`, and must fit the host, replication,
and HAProxy ranges already configured in `env/cluster.conf`.

1. Copy `env/mox1_dot_conf` or `env/mox2_dot_conf` to the new
   `env/moxN.conf` name and set the new host's disk, NIC, public/private
   network, iDRAC, capacity, and HAProxy values. This file is used only for
   initial setup and retries before setup completes.
2. Ensure every existing cluster node is online and healthy:

   ```bash
   ./diagnostics/show_cluster_state.sh
   ```

3. From a Debian-based workstation, run setup using the same storage policy
   as the existing hosts:

   ```bash
   ./hosts/setup_proxmox_host.sh --host mox3 --encrypt
   # or, for a clear-storage cluster:
   ./hosts/setup_proxmox_host.sh --host mox3 --no-encrypt
   ```

   Choose `--run-boot-tests` or `--skip-boot-tests` explicitly when desired.
   The setup workflow temporarily reconciles QDevice quorum while joining the
   node and requires all existing nodes to remain online.

4. Verify the completed cluster and synchronize the current BMAC runtime:

   ```bash
   ./diagnostics/show_cluster_state.sh
   ./hosts/update_cluster_runtime.sh --dry-run
   ./hosts/update_cluster_runtime.sh
   ```

Adding the node does not automatically change any existing production VM's
placement.

### Migrating Workloads to Hosts with More Resources

First add and validate the new host as described above. Before moving a
production VM, its HA placement, replication targets, registry placement, and
route policy must all agree and an initial replication to the new host must
finish successfully.

The repository currently has no high-level operator workflow that safely
changes placement for an existing production VM. Do not change only the
registry or only the Proxmox HA rule: that would leave the other control
planes inconsistent. Until a placement-reconfiguration workflow is added,
treat this as an advanced manual operation using the Proxmox UI/CLI and the
registry tooling, and validate each stage with:

```bash
./diagnostics/show_cluster_state.sh
./diagnostics/show_prod_vm_state.sh prod1
```

The safe operational order is:

1. Add the new host to placement and replication without removing either
   existing placement host.
2. Wait for and verify a complete initial replication to the new host.
3. Destroy or stop staging guests on the intended destination.
4. Gracefully stop or HA-migrate the production VM to the new host, then
   verify it is healthy before serving traffic.
5. Remove the old host from placement only after HA, replication, registry,
   and routing state agree, while retaining at least two valid production
   placement hosts.
6. Remove or decommission the old cluster host only after no production VM,
   replica, staging VM, HA rule, route, or deferred cleanup depends on it and
   cluster/QDevice quorum remains healthy.

## A Basic CI/CD Workflow

A simple CI/CD workflow can be made for most webapps by doing the following

You need to make a deploy script that can target a host using SSH to access that host.  This deploy script, if it finds that the target host has never been configured, should do initial configuration such as by updating the OS and installing things like NGINX etc.  This deploy script should be able to build all of your webapp's assets and copy them to the target host.  Prior to building, your deploy script should create a new tag in version control (usually git) of the code it's building.  This tag, in some form, should accompany the assets that get copied to the target host, so that it's always clear on that target host the exact code that was deployed, and when assets are deployed to a staging host the prior deployment tag value should be preserved on the staging host before the current deployment tag value is updated, so that for any staging host you can always determine the prior and current tag values - the prior tag value is the tag value of the assets that were deployed to production, and the current tag value is the tag value of the updated code that overwrote production's assets on your staging VM.  Your deploy script should also have a mode where it can be told of a staging VM that has passed tests, and that you wish that staging VM's code changes to be pushed to production.

1. Create your production VM (a one-time step for your webapp) by running `guests/prod/create_prod_vm.sh`
2. Test your webapp locally.  When it passes your tests, run your deploy script targetting the new production VM (ex: `prod1`)

Then for any future updates to production, do the following:

1. Build your webapp and test it locally on your own dev workstation.  When your local tests pass the next step is to test in staging.
2. Create a temorary staging VM using `guests/staging/create_staging_vm.sh`
3. Run your deploy script targetting the new staging VM (ex: `stage1prod1`)
4. Test your app in staging.  If it passes your tests then call your Deloy script with parameters sufficient to tell it that you wish to push staging to production, specifying which staging VM passed tests.  The depoy code must then look at the staging VM's prior tag and ensure that it still matches the production VM's current tag, and if they match then the deploy code must checkout the code at the staging VM's current tag and build that code and deploy it to production, updating production's old current tag with this new current tag.  This way you only ever push to production changes that have been made directly against both production's prior code state AND its data state.
5. Cleanup: Remove the staging VM using `guests/staging/destroy_staging_vm.sh`

## Why not use Kubernetes (K8s)?

Kubernetes is primarily a container-orchestration system. It assumes you already have machines, networking, storage, and a working cluster underneath it. BMAC operates at that lower infrastructure layer: it provisions and manages the bare-metal hosts, ZFS storage and replication, quorum, networking, ingress, and VM-level high availability.

Kubernetes also doesn't, by itself, solve persistent-storage or database failover. On bare metal you still need to choose and operate the storage system, and stateful applications generally need additional database-aware HA tooling. BMAC takes a simpler approach for applications that fit comfortably on one machine: the entire production VM, including its application and database state, is replicated and failed over as a single unit.

BMAC also provides a production-derived staging model that Kubernetes does not provide out of the box. Replicated production storage on a standby host can be turned into a lightweight Copy-on-Write staging VM, patched with a unique machine identity, tested independently, and discarded when no longer needed. The standby hardware therefore remains useful during normal operation while still being available for production failover.

BMAC doesn't prevent you from using Kubernetes. If your application benefits from containers, horizontal scaling, service orchestration, or Kubernetes deployment tooling, you can run Kubernetes inside VMs hosted by BMAC. The two systems solve different layers of the problem.

## Developing BMAC

To work on BMAC itself from a Mac, including a Lima VM that runs the complete test suite and the known limits of that setup, see [`DEVELOPMENT.md`](DEVELOPMENT.md).

## History

This project was worked on, tested and used privately, with tons of commits, before being migrated to this public repo.  Our hope is to help others while garnering support from the community for self-built cloud infrastructure.

## Future

We have many more improvements planned for BMAC, including support for integrated CEPH.  For FiberState deployments, BMAC could be improved significantly if FiberState adds Internet ingress/egress capability from the VLAN.  This would drammatically-simplify the BMAC implementation, doing away with the need for HAProxy and VRRP+keepalived for ingress/egress routing, and the complex coordination and syncing these currently require.  More details on low-hanging-fruit changes that FiberState could make are in [`FUTURE.md`](FUTURE.md).

## Thoughts on Greenfield Webapp Tech-Stack Selection

If you're just starting a new webapp and you haven't yet chosen your tech stack, I think it'd be worthwhile for you to watch [these](https://www.youtube.com/watch?v=GFtFIxrAjIs&t=2s) [two](https://www.youtube.com/watch?v=sQXFhh_PiG4) videos, so that you can get a sense for how capable a monolith stack can be on a even a single vCPU, and which stack choices may benefit you.  In this age of agentic programming I suggest you lean into biasing your stack choices towards those that will most-empower your AI-agent coding collaborators rather than the human developer, as the AI is likely going to be writing the majority of the code, and the human is more likely to be focused on the requirements and design.

I'm currently quite interested in this stack:  Rust + Tokio + Axum + Hyper + rustls + SQLx + Askama + HTMX, with PostgreSQL or SQLite for the DB layer.   The reasons for this stack choice are:

- **Server-side rendering keeps application state simple** - It can be hard to understand, initially, how valuable this item is, until you have some experience trying to synchronize state across both the client and the server. By having all state remain at the server, where it's the sole source-of-truth, there's no need for state synchronization, which is a constant code-burden and can become more complex the more your app-logic grows.  The effect of a single state is a drammatic decrease in complexity and that's a win that will continue to pay dividends as your project grows. Durable application state can remain in PostgreSQL or SQLite, with the Rust application acting primarily as a stateless transformation layer from database state to HTML.  Any state manintained by the clint is not application-model state but rather interaction-related state, such as the current position within a list, etc.  Strategic use of client polling updates, isolated to areas of the UI that benefit most from state refresh, can eliminate the need for persistent SSE connections between the clients and server, which can be a significant memory overhead at scale.

- **Minimal client-side state** - HTMX allows rich browser interaction using server-generated HTML fragments, while keeping most JavaScript limited to ephemeral interaction state such as dialogs, animations, selections, and other UI behaviors.

- **No separate frontend application required** - For applications that do not need a native mobile client or public API, there is no need to maintain a separate React application, JSON API contract, frontend state store, API client layer, or duplicated frontend/backend data models.

- **High performance with low overhead** - Rust provides native-code performance, excellent memory efficiency, and no garbage collector, making it well suited to vertically-scaled servers with large CPU and memory resources.

- **Strong compile-time correctness** - Rust's type system, ownership model, exhaustive matching, and `Send`/`Sync` checks catch many classes of bugs before the application ever runs.

- **Excellent fit for AI-assisted development** - Rust's strict compiler provides detailed, machine-readable feedback that coding agents can repeatedly use to correct generated code, reducing the amount of structurally-invalid code that reaches runtime.

- **Compile-time checked SQL** - SQLx can validate static SQL queries against the actual database schema and verify parameter and result types during development/build time, catching many SQL mistakes before deployment.

- **Embedded database migrations** - SQLx migrations can be compiled into the application executable, avoiding the need to separately deploy migration SQL files.

- **Efficient asynchronous I/O** - Tokio provides a mature async runtime for efficiently handling large numbers of concurrent network and database operations without requiring one OS thread per connection.

- **Mature HTTP stack** - Axum provides routing and request handling on top of Hyper, giving the application a high-performance HTTP/1.1 and HTTP/2 implementation while retaining a relatively simple programming model.

- **Native HTTPS support** - rustls provides a mature TLS implementation written in Rust, allowing the application to terminate HTTPS directly without requiring NGINX solely for TLS termination.

- **Compiled server-side templates** - Askama compiles HTML templates into the Rust application, providing fast rendering and compile-time integration between templates and Rust types.

- **Very small deployment surface** - Templates, migrations, HTTP handling, TLS, database access, and application logic can all reside within a single Rust executable, with only static assets and external configuration/secrets needing to accompany it.

- **Excellent vertical scalability** - The architecture can scale from a developer workstation to a very large multi-core server without requiring the application itself to become a distributed system.

- **Portable and infrastructure-independent** - The application primarily depends on standard Linux, HTTP, TLS, and SQL rather than proprietary cloud services or APIs, making it straightforward to develop locally and deploy to bare metal, VMs, or conventional hosting.

- **Simple operational model** - A production system can remain conceptually close to `browser -> Rust application -> database`, reducing the number of independently deployed services, runtimes, proxies, and application layers that must be monitored and maintained.
