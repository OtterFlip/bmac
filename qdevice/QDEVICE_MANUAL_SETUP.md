<!--
Copyright (c) 2026 BEENTHERE VENTURES, INC.
SPDX-License-Identifier: GPL-3.0-only
-->

# Proxmox QDevice VPS Setup

## Purpose

Dedicated Ubuntu VPS used only as a Proxmox QDevice.

All administrative and Proxmox cluster communication occurs over Tailscale.

No services should be intentionally exposed on the public Internet.

The base VPS, Tailscale, SSH, hostname, and firewall hardening remain manual.
`hosts/setup_proxmox_host.sh` installs, enables, and verifies
`corosync-qnetd` over Tailscale during the first Proxmox-host run. It prepares
the service immediately but follows quorum parity: the QDevice vote is absent
for an odd Proxmox node count and added for an even node count.

---

## 1. Install Ubuntu

Install a current Ubuntu Server LTS x64 image.

Update all packages:

```bash
sudo apt update
sudo apt upgrade -y
```

If prompted about GRUB on Linode, use `/dev/sda`.

If prompted about replacing `sshd_config`, retain the existing locally modified version unless there is a specific reason to replace it.

After the upgrade:

```bash
sudo update-grub
sudo dpkg --audit
```

`dpkg --audit` should ideally produce no output.

Reboot:

```bash
sudo reboot
```

After reconnecting, verify:

```bash
uname -r
systemctl --failed
sudo dpkg --audit
```

---

## 2. Install curl

```bash
sudo apt install -y curl
```

---

## 3. Install administrator SSH public keys

For root:

```bash
mkdir -p /root/.ssh
chmod 700 /root/.ssh
```

Edit:

```bash
nano /root/.ssh/authorized_keys
```

Put each SSH public key on its own line.

Then:

```bash
chmod 600 /root/.ssh/authorized_keys
chown -R root:root /root/.ssh
```

---

## 4. Install Tailscale

Install Tailscale using the official Tailscale installation method:

```bash
curl -fsSL https://tailscale.com/install.sh | sh
```

Join the tailnet using an auth key, which should be a new non-reusable, non-ephemeral key with the tag 'tag:proxmox-qdevice'

Replace TS_AUTHKEY below with the actual auth key value begins with 'tskey-auth-...'

```bash
read -s TS_AUTHKEY
sudo tailscale up --auth-key="$TS_AUTHKEY" --hostname=qdevice
unset TS_AUTHKEY
```

Verify:

```bash
tailscale status
tailscale ip -4
```

Record the assigned Tailscale IPv4 address.

---

## 5. Configure workstation SSH access

On the administrator workstation, add an entry to `~/.ssh/config` similar to:

```sshconfig
Host qdevice
    HostName 100.x.x.x
    User root
```

Replace `100.x.x.x` with the QDevice's Tailscale address.

Before accepting the host key for the first time, verify its fingerprint on the QDevice.

For a print of all the valid fingerprints on the QDevice:

```bash
for file in /etc/ssh/*; do ssh-keygen -lf $file; done;
```

Then connect from the workstation:

```bash
ssh qdevice
```

Verify that the fingerprint shown by SSH matches the fingerprint obtained directly from the QDevice before accepting it.

---

## 6. Set the hostname

Set the hostname:

```bash
sudo hostnamectl set-hostname qdevice
```

Edit:

```bash
sudo nano /etc/hosts
```

Ensure it contains:

```text
127.0.0.1   localhost
127.0.1.1   qdevice
```

Do not remove the normal IPv6 localhost entries.

Verify:

```bash
hostname
hostnamectl
getent hosts qdevice
```

Reboot:

```bash
sudo reboot
```

Reconnect and verify:

```bash
hostname
```

It should still report:

```text
qdevice
```

---

## 7. Inspect network services before enabling the firewall

Run:

```bash
ip -br addr
ss -lntup
sudo ufw status verbose
```

Expected interfaces should include:

```text
lo
eth0
tailscale0
```

At this point SSH may still be listening on `0.0.0.0:22` and `[::]:22`.

That is acceptable because the firewall will block public access.

---

## 8. Configure UFW

Set restrictive defaults:

```bash
sudo ufw default deny incoming
sudo ufw default allow outgoing
sudo ufw default deny routed
```

Allow inbound traffic only through the Tailscale interface:

```bash
sudo ufw allow in on tailscale0
```

Do **not** add a public SSH rule.

Do **not** add a public qnetd rule.

Do **not** add a public UDP `41641` rule unless direct Tailscale connectivity later proves to require it.

---

## 9. Verify IPv6 firewall support

Run:

```bash
grep '^IPV6=' /etc/default/ufw
```

It should return:

```text
IPV6=yes
```

If it says `IPV6=no`, fix that before enabling UFW.

---

## 10. Inspect the pending UFW rules

Run:

```bash
sudo ufw show added
```

Expected result:

```text
ufw allow in on tailscale0
```

There should be:

- No rule allowing TCP 22 on `eth0`
- No rule allowing TCP 5403 on `eth0`
- No rule allowing UDP 41641 on `eth0`

---

## 11. Enable UFW

Keep the current SSH session open while doing this.

Enable the firewall:

```bash
sudo ufw enable
```

Verify:

```bash
sudo ufw status verbose
```

Expected general configuration:

```text
Status: active
Default: deny (incoming), allow (outgoing)
```

Inbound rules should allow `tailscale0` and nothing else intentionally exposed to the Internet.

---

## 12. Verify SSH access over Tailscale

From a second workstation terminal:

```bash
ssh qdevice
```

This should succeed.

---

## 13. Verify public SSH is blocked

From the workstation, attempt to connect directly to the Linode public IPv4 address:

```bash
ssh -o ConnectTimeout=5 root@PUBLIC_IPV4
```

Expected result:

```text
Connection timed out
```

Do the same for the public IPv6 address if desired:

```bash
ssh -6 -o ConnectTimeout=5 root@PUBLIC_IPV6
```

That should also fail.

---

## 14. Verify Tailscale connectivity

From the workstation:

```bash
ping qdevice
```

Also run:

```bash
tailscale ping qdevice
```

`tailscale ping` is particularly useful because it reports whether communication is direct or going through DERP.

Direct connectivity is preferable, but DERP remains functional without opening inbound UDP `41641`.

If direct connectivity later proves unreliable, investigate before opening UDP `41641`.

If it must be opened, prefer restricting it to known Proxmox public IP addresses rather than allowing the entire Internet.

---

## 15. Linode Cloud Firewall

Configure the Linode Cloud Firewall with:

```text
Inbound default: DROP
Outbound default: ACCEPT
```

No public inbound SSH rule is required.

No public inbound qnetd TCP `5403` rule is required.

No public inbound Tailscale UDP `41641` rule is required for basic Tailscale operation or DERP.

If UDP `41641` is later intentionally opened to improve direct Tailscale connectivity, document why it was needed.

---

## 16. Install and configure the QDevice software

The Proxmox host setup workflow (`setup_proxmox_host.sh`) installs
`corosync-qnetd` here when it is missing, enables the service, and verifies
that it listens on TCP `5403`. Concurrent mox installers serialize this work
with `/run/lock/app-ha-qdevice-provision.lock` on the QDevice so their APT
cache updates cannot overlap.

The qnetd service normally uses TCP `5403`.

Do **not** expose TCP `5403` publicly.

The Proxmox hosts should connect to qnetd through the QDevice's Tailscale address.

Because inbound traffic on `tailscale0` is allowed by UFW, qnetd will be reachable over Tailscale while remaining inaccessible through `eth0`.

Do not manually force a QDevice vote into a one-node cluster merely because
the service is ready. The setup workflow adds or removes the vote according
to the current odd/even Proxmox membership.

---

## 17. Final security verification

Run:

```bash
sudo ufw status verbose
ss -lntup
tailscale status
systemctl --failed
```

From another machine, verify:

```bash
ssh qdevice
```

works, while:

```bash
ssh root@PUBLIC_IPV4
```

does not.

Also verify that the Proxmox nodes can reach the QDevice over Tailscale before configuring cluster quorum.

---

## Destructive QDevice teardown and clean-room retesting

Use [`purge_qdevice.sh`](purge_qdevice.sh) only when intentionally retiring
the QDevice or returning the dedicated host to a pre-QDevice state for a full
setup test:

```bash
qdevice/purge_qdevice.sh [qdevice-host] [proxmox-host]
```

The script is deliberately destructive. It requires the exact interactive
confirmation `GO`, verifies both SSH targets, refuses to purge a Proxmox VE node,
and first detaches the QDevice with `pvecm qdevice remove`. If cluster-side
detachment fails or cannot be proven, the external server is left intact.

After safe detachment it removes the identified Proxmox setup key, QNetd
TLS/NSS identity and certificates, Corosync/QDevice services and packages,
package-specific state, and the `coroqnetd` account/group. It intentionally
does not run `apt autoremove` and does not erase general system journals.
Review its complete warning before use.
