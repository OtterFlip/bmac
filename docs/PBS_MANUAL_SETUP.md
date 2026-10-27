# Manual Setup: Proxmox Backup Server as a Repository for Multiple Clusters

This guide sets up one Proxmox Backup Server (PBS) as the off-site backup
repository for several independent Proxmox VE (PVE) clusters. The examples use
three clusters named **cluster1**, **cluster2** and **cluster3**.

What you end up with:

- One ZFS-backed PBS datastore (`pbs`), with one **namespace per cluster**.
- Per-cluster **users and API tokens** that can back up and restore only their
  own namespace, and read (but not change) their own retention policy.
- **Client-side encryption** with a separate key per cluster, plus one offline
  **master key** that can recover any cluster's key.
- Server-side **prune, garbage-collection and verify** jobs; clients cannot
  prune or delete.
- A firewall rule that exposes PBS (TCP 8007) only to the clusters' addresses.

The guide works whether PBS runs on bare metal or in a VM. VM-specific steps
(passing the data disks through as LUNs) are in [Appendix A](#appendix-a-running-pbs-as-a-libvirtkvm-vm-with-passed-through-disks).

---

## Contents

1. [Placeholders used in this guide](#1-placeholders-used-in-this-guide)
2. [Install PBS and set up updates](#2-install-pbs-and-set-up-updates)
3. [Prepare the data disks](#3-prepare-the-data-disks)
4. [Create the ZFS pool](#4-create-the-zfs-pool)
5. [Create the datastore](#5-create-the-datastore)
6. [Namespaces, users, tokens and permissions](#6-namespaces-users-tokens-and-permissions)
7. [Retention, garbage collection and verification](#7-retention-garbage-collection-and-verification)
8. [Network access](#8-network-access)
9. [Encryption keys](#9-encryption-keys)
10. [Connect each PVE cluster](#10-connect-each-pve-cluster)
11. [Reading the retention policy from PVE](#11-reading-the-retention-policy-from-pve)
12. [Operations](#12-operations)
13. [Moving the pool to another PBS host](#13-moving-the-pool-to-another-pbs-host)
- [Appendix A: Running PBS as a libvirt/KVM VM with passed-through disks](#appendix-a-running-pbs-as-a-libvirtkvm-vm-with-passed-through-disks)
- [Appendix B: Troubleshooting notes](#appendix-b-troubleshooting-notes)

---

## 1. Placeholders used in this guide

| Placeholder | Meaning | Example |
|---|---|---|
| `<PBS_PUBLIC_IP>` | Public address the clusters use to reach PBS | `203.0.113.10` |
| `<PBS_LAN_IP>` | PBS's own LAN address (behind the router) | `192.168.1.104` |
| `<PBS_FINGERPRINT>` | SHA-256 fingerprint of PBS's TLS certificate | `5d:17:0f:...:07:8d` |
| `<PBS_PUBKEY_SHA256_HEX>` | SHA-256 of PBS's certificate public key (hex) | from the PBS dashboard |
| `<CLUSTERn_IPS>` | Public source addresses of each cluster's nodes | `198.51.100.11` |
| `<DISK_A>` ... | Data disks, as `/dev/disk/by-id/...` paths | `/dev/disk/by-id/ata-Vendor_Model_SERIAL` |

Cluster naming used throughout:

| Cluster | PBS namespace | PBS user | API token ID | PVE storage ID |
|---|---|---|---|---|
| cluster1 | `cluster1` | `cluster1@pbs` | `cluster1@pbs!pve` | `pbs-cluster1` |
| cluster2 | `cluster2` | `cluster2@pbs` | `cluster2@pbs!pve` | `pbs-cluster2` |
| cluster3 | `cluster3` | `cluster3@pbs` | `cluster3@pbs!pve` | `pbs-cluster3` |

Unless a step says otherwise, run PBS commands **as root on the PBS server**
and PVE commands **as root on one node of the relevant cluster** (PVE storage
and backup-job settings live in `/etc/pve` and apply cluster-wide).

---

## 2. Install PBS and set up updates

1. Install PBS from the official ISO onto its **own OS disk**. Keep the data
   disks unattached (or unselected) so the installer cannot touch them. ext4 or
   xfs is fine for the OS disk; it keeps the OS separate from the ZFS data pool.
2. In the installer's network screen, set the hostname (FQDN), a static IP
   (`<PBS_LAN_IP>/24`), gateway and DNS. Leave **"Pin network interface
   names"** checked; it ties the interface name to the NIC's MAC address so it
   cannot change after upgrades or hardware changes.

### Switch to the no-subscription repository

GUI: **Administration → Repositories** → disable `pbs-enterprise` → **Add** →
**No-Subscription**. Or from the shell (PBS 4 / Debian 13 "trixie"):

```bash
sed -i '/^Enabled:/d' /etc/apt/sources.list.d/pbs-enterprise.sources
echo 'Enabled: false' >> /etc/apt/sources.list.d/pbs-enterprise.sources

cat > /etc/apt/sources.list.d/proxmox.sources <<'EOF'
Types: deb
URIs: http://download.proxmox.com/debian/pbs
Suites: trixie
Components: pbs-no-subscription
Signed-By: /usr/share/keyrings/proxmox-archive-keyring.gpg
EOF
```

### Update

```bash
apt update
apt full-upgrade      # Proxmox's supported method; review the list before confirming
```

Use `full-upgrade`, not plain `upgrade`: Proxmox point releases sometimes add
or replace dependencies, which `upgrade` would hold back. Neither command moves
you to a new major release.

### SSH key access (recommended)

PBS uses the standard `/root/.ssh/authorized_keys` (there is no `/etc/pve` on
PBS). From your workstation:

```bash
ssh-copy-id -i ~/.ssh/id_ed25519.pub root@<PBS_LAN_IP>
ssh -o PasswordAuthentication=no root@<PBS_LAN_IP> 'hostname'
```

Optionally require keys only:

```bash
cat > /etc/ssh/sshd_config.d/10-keys-only.conf <<'EOF'
PasswordAuthentication no
KbdInteractiveAuthentication no
PermitRootLogin prohibit-password
EOF
sshd -t && systemctl reload ssh
```

Never forward port 22 to PBS from the internet.

### Record the TLS fingerprint

Read it **on the PBS server itself** (console, a trusted SSH session, or the
local web UI). A value fetched over the network only shows what the network
served.

```bash
proxmox-backup-manager cert info | grep -i fingerprint
```

GUI: **Dashboard → Show Fingerprint**. This is `<PBS_FINGERPRINT>`. It changes
if the certificate is regenerated or replaced (for example with ACME), so
update every client if that happens.

---

## 3. Prepare the data disks

### Identify the disks

```bash
ls -l /dev/disk/by-id/ | grep -v -- '-part'
lsblk -o NAME,SIZE,MODEL,SERIAL,FSTYPE,MOUNTPOINTS
```

Always use `/dev/disk/by-id/...` paths. Kernel names (`sdb`, `sdc`, ...) can
change between boots.

Check firmware and health, and update firmware **before** putting data on the
disks:

```bash
smartctl -i -H /dev/disk/by-id/<DISK_A>     # repeat per disk; add -d sat if needed (Appendix A)
```

### Why partition instead of using whole disks

Drives sold as the same capacity are not always the same size. For example,
two "2 TB" drives can differ by about 380 MiB (an IDEMA-standard
2,000,398,934,016 bytes versus an exact-decimal 2,000,000,000,512 bytes); for
8 TB drives the same gap is about 1.46 GiB. ZFS refuses a replacement or
expansion disk that is even slightly smaller than the existing members.

So create one partition per disk on a whole-GiB boundary that leaves **at
least 2 GiB unused** at the end. Every future disk of the same nominal size
can then be partitioned identically and will always be accepted.

Compute the partition size (whole GiB, minus 2 GiB of buffer):

```bash
echo $(( $(blockdev --getsize64 /dev/disk/by-id/<DISK_A>) / 1024**3 - 2 ))G
```

Example: an IDEMA 8 TB drive is 8,001,563,222,016 bytes = 7,452 GiB + 37 MiB,
so use `+7450G`. Use the **same size on every disk**, now and for all future
replacement or expansion disks.

### Partition

```bash
apt-get install -y gdisk parted
PART_SIZE=7450G        # from the calculation above
DISKS=(
  /dev/disk/by-id/<DISK_A>
  /dev/disk/by-id/<DISK_B>
  /dev/disk/by-id/<DISK_C>
)

for d in "${DISKS[@]}"; do
  sgdisk --zap-all "$d"
  sgdisk -n1:1M:+${PART_SIZE} -t1:BF01 -c1:pbs-data "$d"
done
for d in "${DISKS[@]}"; do blockdev --rereadpt "$d" || true; done
udevadm settle; sleep 2

for d in "${DISKS[@]}"; do
  [ -e "${d}-part1" ] && echo "OK  ${d}-part1" || { echo "MISSING ${d}-part1"; exit 1; }
done
```

---

## 4. Create the ZFS pool

Example: three disks in RAIDZ1 (any one disk can fail). Adjust the vdev type
for your disk count.

```bash
[ -f /etc/hostid ] || zgenhostid        # required for multihost

zpool create -o ashift=12 -o autotrim=on -o multihost=on \
  -O compression=zstd -O recordsize=1M -O relatime=on -O xattr=sa \
  -m /mnt/datastore/pbs pbs raidz1 \
  "${DISKS[0]}-part1" "${DISKS[1]}-part1" "${DISKS[2]}-part1"

zpool status pbs
zfs list -o name,used,avail,mountpoint pbs
for d in "${DISKS[@]}"; do sgdisk -p "$d" | grep -E '^ +1 |free'; done
```

Property notes:

| Setting | Why |
|---|---|
| `ashift=12` | 4 KiB sectors (correct for SSDs and modern HDDs) |
| `autotrim=on` | passes freed space to SSDs as TRIM |
| `multihost=on` | refuses to import the pool on a second system while it is active elsewhere (protects against double import, for example host and VM, or two machines during a migration) |
| `compression=zstd` | cheap and effective |
| `recordsize=1M` | suits PBS's ~4 MiB chunk files and keeps RAIDZ padding overhead low |
| `relatime=on` | **leave atime enabled**: PBS garbage collection depends on chunk access times; never set `atime=off` |
| `xattr=sa` | stores extended attributes efficiently |

Do **not** enable ZFS `dedup`; PBS deduplicates chunks itself.

### Notes on RAIDZ and growth

- A replacement disk must be at least the partition size. With the 2 GiB
  buffer above, any disk of the same nominal size qualifies.
- RAIDZ expansion (`zpool attach pbs raidz1-0 <new-part1>`, OpenZFS 2.3+) adds
  a disk to an existing vdev. Existing data keeps the old parity ratio until it
  is rewritten; because PBS writes each chunk once, efficiency improves
  gradually as old backups are pruned. `zfs list` under-reports free space
  after an expansion.
- Moving from RAIDZ1 to RAIDZ2 requires recreating the pool (and re-running
  backups, or temporarily backing up to another PBS first).

---

## 5. Create the datastore

```bash
proxmox-backup-manager datastore create pbs /mnt/datastore/pbs
proxmox-backup-manager datastore list
```

The command initializes the chunk store (65,536 subdirectories) and ends with
"Access time update check successful". That check confirms the atime behaviour
that garbage collection needs.

Reboot once and confirm everything returns on its own:

```bash
reboot
# after it comes back:
zpool status -x pbs
zpool get -H -o property,value multihost,autotrim pbs
zfs get -H -o value mounted pbs
proxmox-backup-manager datastore list
```

---

## 6. Namespaces, users, tokens and permissions

Each cluster gets:

1. its own **namespace** in the datastore;
2. its own **user** in the PBS realm (`<name>@pbs`) with **no password**, so it
   cannot log in; it exists only to own the token;
3. an **API token** (`<name>@pbs!pve`) that the cluster uses;
4. **ACLs on its own namespace only**:
   - `DatastoreBackup`: create backups, and read/restore its own backups;
   - `DatastoreAudit`: read-only access to its namespace, including the
     namespace's **prune jobs (retention policy)** and snapshot metadata.

Rules to know:

- **A token's effective permissions are the intersection of the token's ACLs
  and its user's ACLs.** Grant every role to both the user and the token.
- `DatastoreBackup` does **not** include prune or delete. A compromised cluster
  can push new backups but cannot erase its history. Retention is enforced by
  server-side prune jobs (section 7).
- Nothing is granted at the datastore root or on other namespaces, so clusters
  cannot see each other's backups.
- The token ID format is `user@realm!tokenname`: `cluster1` is the user, `pbs`
  is PBS's built-in user database (as opposed to `pam`, Linux accounts), and
  `pve` is the token's name.

### 6.1 Parameters

```bash
DATASTORE=pbs
CLUSTERS="cluster1:cluster1 cluster2:cluster2 cluster3:cluster3"   # user:namespace
TOKEN_NAME=pve
```

### 6.2 Create the namespaces

This uses the API, so it asks for root's password. The first time, it also
asks you to confirm the fingerprint; compare it with `<PBS_FINGERPRINT>`
before answering `y`. (Alternatively: **Datastore → pbs → Content → Add
Namespace** in the GUI.)

```bash
for pair in $CLUSTERS; do
  ns=${pair#*:}
  proxmox-backup-client namespace create "$ns" --repository "root@pam@localhost:$DATASTORE"
done
proxmox-backup-client namespace list --repository "root@pam@localhost:$DATASTORE"
```

If you run this from another machine over SSH, use `ssh -t` so the password
prompt works.

### 6.3 Create users, tokens and ACLs

```bash
for pair in $CLUSTERS; do
  u=${pair%%:*}; ns=${pair#*:}
  proxmox-backup-manager user create "${u}@pbs" --comment "Backup client for ${u}"

  echo "===== Token ${u}@pbs!${TOKEN_NAME}: save the value NOW (shown only once) ====="
  proxmox-backup-manager user generate-token "${u}@pbs" "$TOKEN_NAME"

  for role in DatastoreBackup DatastoreAudit; do
    proxmox-backup-manager acl update "/datastore/${DATASTORE}/${ns}" "$role" --auth-id "${u}@pbs"
    proxmox-backup-manager acl update "/datastore/${DATASTORE}/${ns}" "$role" --auth-id "${u}@pbs!${TOKEN_NAME}"
  done
done
proxmox-backup-manager acl list
```

Store each token's `value` in a password manager, labelled with its token ID.
PBS never shows a token secret again.

### 6.4 Verify isolation

```bash
for pair in $CLUSTERS; do
  u=${pair%%:*}; a="${u}@pbs!${TOKEN_NAME}"
  echo "== $a"
  for p in "/datastore/${DATASTORE}" $(for q in $CLUSTERS; do echo "/datastore/${DATASTORE}/${q#*:}"; done); do
    printf "  %-28s %s\n" "$p" "$(proxmox-backup-manager user permissions "$a" --path "$p" | grep -o 'Datastore\.[A-Za-z]*' | tr '\n' ' ')"
  done
done
```

Each token must show `Datastore.Audit Datastore.Backup` on **its own namespace
only**; every other path must be empty.

### 6.5 Optional: a read-only restore credential for a cluster admin

To let someone restore a cluster's backups without being able to push new
ones, create a separate user and token with `DatastoreReader` (plus
`DatastoreAudit`) on that cluster's namespace only. Note that PBS records the
token that created each backup group as its owner, and a backup group can have
only **one** owner, so do not have two different tokens *write* backups of the
same guest.

---

## 7. Retention, garbage collection and verification

Because the clients cannot prune, all retention runs on PBS:

- **Prune**: decides which snapshots to keep. One job per namespace, so each
  cluster can have its own policy.
- **Garbage collection (GC)**: reclaims chunks no longer referenced after
  pruning. One schedule for the whole datastore; weekly is enough.
- **Verify**: reads chunks back and checks checksums.

### How PBS evaluates retention

Options are applied in the order `keep-last`, `keep-hourly`, `keep-daily`,
`keep-weekly`, `keep-monthly`, `keep-yearly`. Each option only considers
snapshots **older** than those already kept by earlier options. `keep-daily N`
keeps the newest snapshot of each of the N most recent days that have backups;
`keep-weekly` works the same per ISO week. There is no "keep everything from
today" option; use a rolling `keep-last` count instead.

### Example policy

For clusters that back up **four times a day** (every 6 hours) and want the
recent intra-day backups, then 3 daily and 4 weekly:

| Setting | Effect |
|---|---|
| `keep-last 6` | the 6 newest snapshots per guest: the last ~24 h of scheduled backups plus room for two manual ones |
| `keep-daily 3` | below those, the last snapshot from each of 3 earlier days |
| `keep-weekly 4` | below those, the last snapshot from each of 4 earlier weeks |
| no monthly/yearly | nothing older is kept |

That is about 13 snapshots per guest (about a month of history). Consecutive
snapshots share most chunks, so this costs far less than 13 full copies.

### Create the jobs

```bash
DATASTORE=pbs
for spec in cluster1:05:00 cluster2:05:10 cluster3:05:20; do
  IFS=: read -r ns hh mm <<<"$spec"
  proxmox-backup-manager prune-job create "prune-$ns" --store "$DATASTORE" --ns "$ns" \
    --schedule "$hh:$mm" --keep-last 6 --keep-daily 3 --keep-weekly 4
done

proxmox-backup-manager datastore update "$DATASTORE" --gc-schedule 'sat 06:00'

proxmox-backup-manager verify-job create "verify-$DATASTORE" --store "$DATASTORE" \
  --schedule 'sun 06:00' --ignore-verified true --outdated-after 30

proxmox-backup-manager prune-job list
proxmox-backup-manager verify-job list
proxmox-backup-manager datastore show "$DATASTORE" | grep -i gc
```

Schedule these for times when PBS is reliably powered on: **PBS does not catch
up on runs it missed** while it was off.

### Changing retention later

```bash
proxmox-backup-manager prune-job update prune-cluster1 --keep-daily 7
```

or **Datastore → pbs → Prune & GC** in the GUI. Changes apply at the next prune
run.

- **Lowering retention is irreversible**: the next prune removes snapshots
  outside the new policy and GC reclaims them.
- **Raising retention** only affects what is kept from then on.
- **Preview** a policy against real snapshots with a dry run (once backups
  exist; replace `vm/100` with a real backup group):

```bash
proxmox-backup-client prune vm/100 --ns cluster1 \
  --keep-last 6 --keep-daily 7 --keep-weekly 4 --dry-run \
  --repository root@pam@localhost:pbs
```

---

## 8. Network access

PBS listens on **TCP 8007** (API, web UI and backup traffic). If PBS is behind
a router, forward 8007 from the public address to `<PBS_LAN_IP>`, restricted
to the clusters' source addresses. Example for OpenWrt (fw4):

### LuCI

1. **Network → Firewall → IP Sets → Add**: name `pbs-clients`, family IPv4,
   *Packet Field Match* `src_ip`, and add each cluster node's public IP as a
   separate entry.
2. **Network → Firewall → Port Forwards → Add**:
   - *General Settings*: name `PBS-from-clusters`, IPv4 only, protocol TCP,
     source zone `wan`, external port `8007`, destination zone `lan`,
     internal IP `<PBS_LAN_IP>`, internal port `8007`.
   - *Advanced Settings*: **Use ipset** `pbs-clients`; leave *Source IP
     address* as `any` (the IP set does the filtering; the single-IP field
     would add a second condition).
3. **Save**, then **Save & Apply**.

### CLI equivalent

```bash
uci add firewall ipset
uci set firewall.@ipset[-1].name='pbs-clients'
uci set firewall.@ipset[-1].family='ipv4'
uci add_list firewall.@ipset[-1].match='src_ip'
uci add_list firewall.@ipset[-1].entry='<CLUSTER1_NODE1_IP>'
uci add_list firewall.@ipset[-1].entry='<CLUSTER1_NODE2_IP>'
# ... one entry per node of every cluster

uci add firewall redirect
uci set firewall.@redirect[-1].name='PBS-from-clusters'
uci set firewall.@redirect[-1].family='ipv4'
uci set firewall.@redirect[-1].proto='tcp'
uci set firewall.@redirect[-1].src='wan'
uci set firewall.@redirect[-1].src_dport='8007'
uci set firewall.@redirect[-1].dest='lan'
uci set firewall.@redirect[-1].dest_ip='<PBS_LAN_IP>'
uci set firewall.@redirect[-1].dest_port='8007'
uci set firewall.@redirect[-1].target='DNAT'
uci set firewall.@redirect[-1].ipset='pbs-clients'
uci commit firewall && /etc/init.d/firewall reload
```

### Verify

On the router:

```bash
nft list set inet fw4 pbs-clients
nft list ruleset | grep -B1 -A3 'dport 8007'     # expect: ip saddr @pbs-clients ... dnat ip to <PBS_LAN_IP>:8007
```

From **each cluster node**:

```bash
openssl s_client -connect <PBS_PUBLIC_IP>:8007 </dev/null 2>/dev/null \
  | openssl x509 -noout -fingerprint -sha256
```

It must print `<PBS_FINGERPRINT>` (in uppercase). From any other network, the
port must time out.

The rule matches the `wan` zone rather than a specific address, so it keeps
working if the public IP changes; update the clusters' storage entries
(`pvesm set <storage-id> --server <new IP>`) when it does.

### Bandwidth

`bwlimit` caps backup/restore throughput:

- per run or per job: `vzdump --bwlimit <KiB/s>` (for example `61440` ≈ 480 Mbit/s);
- PVE defaults: **Datacenter → Options → Bandwidth Limits**;
- centrally on PBS, by client network and time of day: **Configuration →
  Traffic Control**.

Incremental backups are small; consider a limit only for the first full
backup of each guest if it would saturate the PBS site's link.

---

## 9. Encryption keys

### Why encrypt

Backups are encrypted **on the cluster** before upload; PBS only stores
ciphertext. This protects the backups if the PBS host or its disks are stolen
or compromised, and if a failed drive is sent back under warranty.

### The two kinds of key

- **Per-cluster encryption key** (`/etc/pve/priv/storage/<storage-id>.enc`):
  generated on each cluster; encrypts only that cluster's backups. It has no
  passphrase so backups can run unattended. **If the cluster is lost along with
  this key, its backups cannot be decrypted**, so a copy must exist off the
  cluster.
- **Master key** (one RSA key pair for all clusters):
  - the **public** half is added to each cluster's PBS storage entry. On every
    backup, PVE encrypts the cluster's key with it and stores the result
    inside the snapshot. The public half can only encrypt.
  - the **private** half (passphrase-protected) stays **offline**: in a
    password manager plus one physical copy. It never goes on a cluster or on
    PBS.
  - with it, any cluster's key can be recovered from any of its snapshots
    (`proxmox-backup-client key import-with-master-key`), even if the exported
    copy is missing or out of date.

A cluster's own key is all a single-cluster admin needs to restore that
cluster; they never need the master key. Treat the master private key as the
root of trust: together with its passphrase and access to PBS data, it
decrypts every cluster's backups.

### Create the master key (once, on an offline-ish workstation)

```bash
mkdir -p ~/pbs-master-key && cd ~/pbs-master-key && chmod 700 .
openssl genpkey -algorithm RSA -pkeyopt rsa_keygen_bits:4096 -aes-256-cbc -out master-private.pem
#   prompts twice: choose the passphrase and confirm it
openssl pkey -in master-private.pem -pubout -out master-public.pem
#   prompts once: unlock the private key with that same passphrase
chmod 600 master-private.pem
```

Test the passphrase at any time, and confirm the pair matches:

```bash
openssl pkey -in master-private.pem -noout && echo "passphrase OK"
diff <(openssl pkey -in master-private.pem -pubout) master-public.pem && echo "key pair matches"
```

Save `master-private.pem` **and** its passphrase in a password manager, plus
one offline copy. Only `master-public.pem` is ever copied to clusters.

### Handing a cluster's key to that cluster's admin

Give out a passphrase-protected **copy**, not the working file:

```bash
cp /etc/pve/priv/storage/pbs-cluster1.enc /root/cluster1-handout.enc
proxmox-backup-client key change-passphrase /root/cluster1-handout.enc --kdf scrypt
# send the file and the passphrase through separate channels, then delete the copy
```

---

## 10. Connect each PVE cluster

Repeat for each cluster, on one of its nodes. Example: cluster1.

### 10.1 Copy the master public key to the node

```bash
scp ~/pbs-master-key/master-public.pem root@<cluster1-node>:/root/pbs-master-public.pem
```

### 10.2 Add the PBS storage

```bash
read -rsp 'cluster1@pbs!pve token secret: ' PBS_TOKEN; echo     # keeps the secret out of shell history

pvesm add pbs pbs-cluster1 \
  --server <PBS_PUBLIC_IP> \
  --datastore pbs \
  --namespace cluster1 \
  --username 'cluster1@pbs!pve' \
  --password "$PBS_TOKEN" \
  --fingerprint <PBS_FINGERPRINT> \
  --encryption-key autogen \
  --master-pubkey "$(base64 -w0 /root/pbs-master-public.pem)" \
  --content backup \
  --prune-backups keep-all=1

unset PBS_TOKEN
pvesm status | grep -E 'Name|pbs-cluster1'          # must show "active"
```

Notes:

- `--port` is optional and defaults to 8007; set it only if you forward a
  different external port.
- When `--username` contains `!`, `--password` is the token's secret.
- `--encryption-key autogen` creates `/etc/pve/priv/storage/pbs-cluster1.enc`.
- `--master-pubkey` stores the master public key as
  `/etc/pve/priv/storage/pbs-cluster1.master.pem`; that stored copy is used on
  every backup.
- `--prune-backups keep-all=1` stops PVE from trying to prune (the token is not
  allowed to; pruning is done by PBS).

### 10.3 Save the cluster's key and tidy up

```bash
ls -l /etc/pve/priv/storage/ | grep pbs-cluster1           # .enc and .master.pem
diff /etc/pve/priv/storage/pbs-cluster1.master.pem /root/pbs-master-public.pem && echo "stored copy matches"
cat /etc/pve/priv/storage/pbs-cluster1.enc                 # save into the password manager
proxmox-backup-client key paperkey /etc/pve/priv/storage/pbs-cluster1.enc --output-format text   # optional printable copy
rm /root/pbs-master-public.pem                             # temporary copy only
```

**Never delete** `pbs-cluster1.enc` or `pbs-cluster1.master.pem` under
`/etc/pve/priv/storage/`; those are the working copies.

### 10.4 Create the backup job

Example: every 6 hours (00:00, 06:00, 12:00, 18:00), snapshot mode, explicit
guest IDs.

```bash
pvesh create /cluster/backup \
  --id bmac-cluster1-6h \
  --schedule '00/6:00' \
  --storage pbs-cluster1 \
  --vmid 100 \
  --mode snapshot \
  --prune-backups keep-all=1 \
  --notes-template 'bmac {{cluster}} {{guestname}} ({{vmid}}) on {{node}}' \
  --enabled 1 \
  --comment 'managed-by=bmac; cluster1 prod -> PBS (pruning done on PBS)'

pvesh get /cluster/backup --output-format yaml | grep -A14 'id: bmac-cluster1-6h'
```

- List guests explicitly (`--vmid 100,105,...`) rather than `--all`, so
  disposable or staging guests are not backed up.
- Backup jobs have no tags field. To tell tooling-managed jobs apart from jobs
  made in the GUI, use the **job ID prefix** (`bmac-`; GUI jobs get
  `backup-<hex>`), the **comment**, and the **notes template** (which marks
  every resulting snapshot, visible in PBS and PVE). Variables:
  `{{cluster}}`, `{{guestname}}`, `{{node}}`, `{{vmid}}`.
- Find managed jobs: `pvesh get /cluster/backup --output-format json | jq -r '.[] | select(.id | startswith("bmac-")) | .id'`
- The job is cluster-wide: each run backs up a guest on whichever node it is
  running on, so HA failovers are handled.
- Incremental backups use QEMU's in-memory dirty bitmap. After a migration,
  failover or guest restart, the next backup **reads** the whole disk (it
  still only uploads changed chunks), so it takes longer.

### 10.5 First backup and verification

On the node where the guest is running:

```bash
vzdump 100 --storage pbs-cluster1 --mode snapshot      # optionally add --bwlimit for the first full backup
```

On PBS:

```bash
proxmox-backup-client snapshot list --ns cluster1 --repository root@pam@localhost:pbs
```

The snapshot appears as encrypted (lock icon under **Datastore → pbs →
Content → cluster1**).

### 10.6 On-demand backups

GUI: guest → **Backup → Backup now**, or:

```bash
vzdump <vmid> --storage pbs-cluster1 --mode snapshot
```

These land in the same namespace and are covered by `keep-last`.

### 10.7 Test a restore

Restore a snapshot to a **new VMID** on a node, with its network disconnected,
boot it, check it, then delete it. Do this for each cluster before relying on
the backups.

---

## 11. Reading the retention policy from PVE

PVE has no built-in command for PBS prune jobs, so call the PBS API with the
cluster's token. `DatastoreAudit` lets each token see only its own
namespace's prune job.

Pin PBS's certificate public key so the call only succeeds against the real
PBS. Take the public key SHA-256 (hex) from the PBS dashboard ("Show
Fingerprint" shows both the certificate and public key fingerprints):

```bash
read -rsp 'cluster1@pbs!pve token secret: ' T; echo
PIN="sha256//$(echo <PBS_PUBKEY_SHA256_HEX> | xxd -r -p | base64)"

curl -s --insecure --pinnedpubkey "$PIN" \
  -H "Authorization: PBSAPIToken=cluster1@pbs!pve:$T" \
  https://<PBS_PUBLIC_IP>:8007/api2/json/config/prune | python3 -m json.tool
unset T
```

`--insecure` skips CA validation (which cannot succeed with a self-signed
certificate); `--pinnedpubkey` then requires the server's key to match. The
output must list only `prune-cluster1`.

---

## 12. Operations

### Replacing a lost or exposed token

Deleting a token also deletes its ACLs; restore them after regenerating.

```bash
u=cluster1; ns=cluster1; TOKEN_NAME=pve
proxmox-backup-manager user delete-token "${u}@pbs" "$TOKEN_NAME"
proxmox-backup-manager user generate-token "${u}@pbs" "$TOKEN_NAME"      # save the new value
for role in DatastoreBackup DatastoreAudit; do
  proxmox-backup-manager acl update "/datastore/pbs/${ns}" "$role" --auth-id "${u}@pbs!${TOKEN_NAME}"
done
```

Then on the cluster:

```bash
read -rsp 'new secret: ' T; echo
pvesm set pbs-cluster1 --password "$T"; unset T
```

List tokens (IDs only, never secrets): `proxmox-backup-manager user list-tokens cluster1@pbs`

### Adding a cluster

Add its node IPs to the router's IP set, create its namespace, user, token and
ACLs (section 6 with a one-pair `CLUSTERS`), add a prune job (section 7), then
connect it (section 10).

### Health checks worth monitoring

Pool health (`zpool status -x`), usable free space (`zfs list -H -p -o
used,available pbs`; alert below 10%), SMART attributes of the pool member
disks, PBS services (`proxmox-backup`, `proxmox-backup-proxy`), prune/GC/verify
task results, and per-namespace backup freshness.

---

## 13. Moving the pool to another PBS host

Because the partition table, ZFS labels and by-id names live on the physical
disks, the pool can move to a new PBS host (for example from a VM to a
dedicated machine):

1. Install PBS on the new host's own OS disk (section 2).
2. On the old host: stop PBS services (or detach the datastore), then
   `zpool export pbs`, then shut down.
3. Move the disks.
4. On the new host: `zpool import pbs` (no `-f` needed after a clean export;
   with `multihost=on`, a forced import first checks that nothing else is
   using the pool).
5. Register the datastore again: `proxmox-backup-manager datastore create pbs
   /mnt/datastore/pbs`, or copy `/etc/proxmox-backup/datastore.cfg` and the
   user, token, ACL and job configuration from the old host
   (`/etc/proxmox-backup/`).
6. If the new host has a new certificate, update `<PBS_FINGERPRINT>` on every
   cluster (`pvesm set <storage-id> --fingerprint <new>`).

Exporting first is best practice: the import is clean, you do not get used to
`-f` (which can corrupt a pool that really is active elsewhere), and the old
host forgets the pool.

---

## Appendix A: Running PBS as a libvirt/KVM VM with passed-through disks

Use this when PBS runs as a VM on a Linux host (for example an Ubuntu
workstation) and the data disks are dedicated to it.

### A.1 Host preparation

```bash
sudo apt install libvirt-daemon-system libvirt-clients virtinst virt-manager \
                 qemu-system-x86 qemu-utils ovmf
sudo usermod -aG libvirt,kvm "$USER"          # log out and back in
virsh -c qemu:///system list --all
virt-host-validate qemu                        # cgroup 'devices' and secure-guest warnings are harmless
```

Shut guests down cleanly when the host shuts down
(`/etc/default/libvirt-guests`):

```bash
sudo sed -i -E 's/^#?ON_BOOT=.*/ON_BOOT=start/; s/^#?ON_SHUTDOWN=.*/ON_SHUTDOWN=shutdown/; s/^#?SHUTDOWN_TIMEOUT=.*/SHUTDOWN_TIMEOUT=300/' \
  /etc/default/libvirt-guests
sudo systemctl enable --now libvirt-guests
```

A normal `shutdown now` then gives the PBS VM up to 5 minutes to shut down
cleanly. If the host is a desktop, block suspend so the VM never freezes
(including GDM's login-screen auto-suspend):

```bash
sudo systemctl mask sleep.target suspend.target hibernate.target hybrid-sleep.target
```

`qemu:///system` VMs run as `libvirt-qemu`, independent of your login session,
so logging out does not stop them.

### A.2 Networking

Give the VM its own LAN address through a Linux bridge. With a spare NIC,
bridge it with no host IP so the host keeps a single address and default
route (netplan example):

```yaml
# /etc/netplan/60-br0.yaml   (mode 600)
network:
  version: 2
  renderer: NetworkManager
  ethernets:
    <spare-nic>:
      dhcp4: false
      dhcp6: false
      link-local: []
  bridges:
    br0:
      interfaces: [<spare-nic>]
      dhcp4: false
      dhcp6: false
      link-local: []
      parameters:
        stp: false
        forward-delay: 0
      networkmanager:
        passthrough:
          ipv4.method: "disabled"
          ipv6.method: "disabled"
```

Remove any existing NetworkManager-generated profile for that NIC in
`/etc/netplan/` first (otherwise it keeps requesting DHCP), then `sudo netplan
generate && sudo netplan apply`. Verify only the primary NIC has an IP and
there is exactly one default route.

If Docker runs on the host, its `FORWARD DROP` policy blocks bridged VM traffic
whenever `br_netfilter` is loaded. Allow `br0` through Docker's user chain,
re-applied whenever Docker starts:

```ini
# /etc/systemd/system/br0-docker-forward.service
[Unit]
Description=Allow bridged VM traffic on br0 through Docker's FORWARD policy
After=docker.service
PartOf=docker.service

[Service]
Type=oneshot
RemainAfterExit=yes
ExecStart=/bin/sh -c 'iptables  -C DOCKER-USER -i br0 -o br0 -j ACCEPT 2>/dev/null || iptables  -I DOCKER-USER -i br0 -o br0 -j ACCEPT'
ExecStart=/bin/sh -c 'ip6tables -C DOCKER-USER -i br0 -o br0 -j ACCEPT 2>/dev/null || ip6tables -I DOCKER-USER -i br0 -o br0 -j ACCEPT || true'

[Install]
WantedBy=docker.service
```

```bash
sudo systemctl daemon-reload && sudo systemctl enable --now br0-docker-forward.service
```

Choose a fixed MAC for the VM (`printf '52:54:00:%02x:%02x:%02x\n' $((RANDOM%256)) $((RANDOM%256)) $((RANDOM%256))`)
and create the router's static DHCP lease for it before creating the VM. The
bridge port NIC itself needs no lease; it never has an IP.

### A.3 Create the VM

```bash
sudo virt-install \
  --name pbs \
  --memory 16384 --vcpus 4 --cpu host-passthrough \
  --machine q35 --boot uefi \
  --osinfo debian13 \
  --controller type=scsi,model=virtio-scsi \
  --disk path=/var/lib/libvirt/images/pbs-os.qcow2,size=32,format=qcow2,bus=scsi,cache=none,discard=unmap \
  --cdrom /var/lib/libvirt/images/proxmox-backup-server_<version>.iso \
  --network bridge=br0,model=virtio,mac=<VM_MAC> \
  --channel unix,target.type=virtio,target.name=org.qemu.guest_agent.0 \
  --graphics spice --video virtio \
  --noautoconsole
```

- Put the ISO in `/var/lib/libvirt/images/` (QEMU runs as `libvirt-qemu`,
  which cannot traverse a `750` home directory). Verify it with
  `sha256sum -c <file>.SHA256` (not just `cat`).
- 16 GiB RAM leaves room for ZFS ARC; 8 GiB is a workable minimum.
- After the installer finishes the VM may power off instead of rebooting;
  start it again.
- After install: `virsh autostart pbs`, and inside PBS
  `apt install qemu-guest-agent && systemctl start qemu-guest-agent` (the
  unit is static and started by udev on later boots; `enable` warnings are
  expected). Check with `virsh qemu-agent-command pbs '{"execute":"guest-ping"}'`.
- The graphical console cannot paste into a text console; use SSH or the PBS
  web shell.

### A.4 Pass the data disks through as LUNs

Whole-disk LUN passthrough (`device='lun'`, QEMU `scsi-block`) lets the guest
see the real drives: model, serial, WWN, by-id names, TRIM and SMART. The
partitions and pool are then created **inside the VM** (sections 3 and 4) and
live entirely on the physical disks, which is what makes section 13 possible.

On the host, hide the disks from the desktop (udisks/GNOME), one line per
serial:

```bash
sudo tee /etc/udev/rules.d/99-hide-pbs-disks.rules >/dev/null <<'EOF'
ENV{ID_SERIAL_SHORT}=="<SERIAL_A>", ENV{UDISKS_IGNORE}="1"
ENV{ID_SERIAL_SHORT}=="<SERIAL_B>", ENV{UDISKS_IGNORE}="1"
ENV{ID_SERIAL_SHORT}=="<SERIAL_C>", ENV{UDISKS_IGNORE}="1"
EOF
sudo udevadm control --reload && sudo udevadm trigger --subsystem-match=block --action=change
```

To show them again, rename the file to `.disabled` and run the same two
`udevadm` commands. The rule only affects the desktop, not the kernel or the
VM.

With the VM shut off, remove the installer CD-ROM (it usually occupies target
`sdb`, which the LUNs need) and attach one LUN per disk:

```bash
virsh detach-disk pbs sdb --config          # only if `virsh domblklist pbs --details` shows the cdrom at sdb

cat > lun-sdb.xml <<'EOF'
<disk type='block' device='lun' rawio='yes'>
  <driver name='qemu' type='raw' cache='none' io='native' discard='unmap'/>
  <source dev='/dev/disk/by-id/<DISK_A>'/>
  <target dev='sdb' bus='scsi'/>
</disk>
EOF
# repeat as lun-sdc.xml / lun-sdd.xml for the other disks

for f in lun-sd*.xml; do virsh attach-device pbs "$f" --config; done
virsh domblklist pbs --details
```

- Do **not** use `sgio='unfiltered'`; libvirt 10 rejects it ("unfiltered sgio
  is no longer supported").
- `--config` copies the XML into libvirt's own definition
  (`/etc/libvirt/qemu/pbs.xml`); the `lun-*.xml` files are not needed
  afterwards. Back up the full definition with
  `virsh dumpxml pbs --inactive > pbs-vm-definition.xml`.

### A.5 Allow SMART through AppArmor (Ubuntu hosts)

`rawio='yes'` gives QEMU `CAP_SYS_RAWIO`, but the VM's AppArmor profile denies
using it, so SMART (ATA pass-through) fails with "scsi error aborted command"
and the host logs `apparmor="DENIED" ... capname="sys_rawio"`. Allow it in the
per-VM profile (written once at define time; not regenerated):

```bash
prof=/etc/apparmor.d/libvirt/libvirt-$(virsh domuuid pbs)
sudo cp -a "$prof" "$prof.bak"
sudo sed -i '/^}/i\  capability sys_rawio,' "$prof"
sudo cat "$prof"                 # the new line must be inside the braces
sudo apparmor_parser -r "$prof"  # applies to the running VM immediately
```

### A.6 SMART inside the VM

- Use `smartctl -d sat` for the passed-through disks; auto-detection reports
  "Probable ATA device behind a SAT layer".
- The overall-health bit is **not available** through the passthrough (the
  ATA return registers do not reach the guest); smartd logs "not capable of
  SMART Health Status check". Judge health from attributes (wear level,
  reallocated/bad-block/uncorrectable/CRC counters), not `-H`.
- Debian's `smartmontools.service` skips itself in VMs
  (`ConditionVirtualization=no`). Override it and list the disks explicitly:

```bash
mkdir -p /etc/systemd/system/smartmontools.service.d
cat > /etc/systemd/system/smartmontools.service.d/10-allow-in-vm.conf <<'EOF'
[Unit]
ConditionVirtualization=
EOF

cp -a /etc/smartd.conf /etc/smartd.conf.orig
{
  echo "# PBS data disks (LUN passthrough): force SAT"
  echo "/dev/disk/by-id/<DISK_A> -d sat -a -m root"
  echo "/dev/disk/by-id/<DISK_B> -d sat -a -m root"
  echo "/dev/disk/by-id/<DISK_C> -d sat -a -m root"
  cat /etc/smartd.conf.orig
} > /etc/smartd.conf

systemctl daemon-reload && systemctl restart smartmontools && systemctl is-active smartmontools
```

### A.7 Host safety

- Do not install ZFS on the host. With `multihost=on` on the pool, a
  simultaneous import elsewhere is refused anyway.
- SMART queries from the host are read-only and safe, but run them inside the
  VM to keep the separation clean.
- Do not mount or open the pool's partitions from the host.

---

## Appendix B: Troubleshooting notes

| Symptom | Cause / fix |
|---|---|
| `apt update` shows 401 from `enterprise.proxmox.com` | Enterprise repo still enabled; see section 2. |
| `ssh: Could not resolve hostname pbs.<lan>` from a workstation | The workstation uses public DNS servers that do not know local names; add an `/etc/hosts` entry or use the router as DNS. |
| `proxmox-backup-client` asks to confirm a fingerprint | That is PBS's **TLS** fingerprint (port 8007), not an SSH host key; compare with `proxmox-backup-manager cert info`. |
| `partprobe: command not found` | Install `parted`, or use `blockdev --rereadpt <disk>`. |
| ZFS: "device is too small" on replace/attach | The new disk/partition is smaller than existing members; partition all disks to the same size with a buffer (section 3). |
| `zpool import` says the pool may be in use by another system | It was not exported. Export it on the old host, or use `zpool import -f` only when certain nothing else is using it. |
| Token works for nothing | ACL was granted only to the token, not also to its user (intersection rule). |
| PVE logs prune permission errors after backups | Storage or job retention is not `keep-all=1`. |
| Second backup job for the same guest fails an ownership check | A backup group has one owner; use one token per cluster. |
| Prune/GC/verify did not run | PBS was off at the scheduled time; schedules do not catch up. |
| VM gets no network on the bridge | Docker's `FORWARD DROP` with `br_netfilter` loaded (Appendix A.2). |
| `virsh attach-device`: "target sdb already exists" | The installer CD-ROM uses that target; detach it. |
| SMART in VM: "scsi error aborted command" | AppArmor denies `sys_rawio` (Appendix A.5). |
| smartd "skipped, unmet condition check ConditionVirtualization=no" | Override in Appendix A.6. |
