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
`scripts/user_callable/hosts/add_proxmox_host.sh` installs, enables, and verifies
`corosync-qnetd` over Tailscale during the first Proxmox-host run. It prepares
the service immediately but follows quorum parity: the QDevice vote is absent
for an odd Proxmox node count and added for an even node count.

---

## 0. Prereq

Your QDevice must connect to your Tailscale network with a tag:proxmox-qdevice Auth key. This means you need to configure your Tailscale account for this tag and its associated policies.  Please follow [these instructions](../README.md#instructions-for-tailscale-setup) to configure your Tailscale account.

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

Replace tskey-auth-xxxxxxx below with the actual auth key value begins with 'tskey-auth-...'

```bash
sudo tailscale up --auth-key="tskey-auth-xxxxxxx" --hostname=qdevice
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

The Proxmox host setup workflow (`add_proxmox_host.sh`), or
`scripts/user_callable/qdevice/add_qdevice.sh` for a replacement QDevice, installs
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

`scripts/user_callable/hosts/remove_proxmox_host.sh` follows the same rule. It removes the QDevice
before deleting a node and adds it back only when the remaining node count is
even. Re-adding it uses the workstation's `ssh qdevice` access to install a
temporary key, as setup does, so the script checks that access before making
any change.
The Tailscale policy must still allow the mox hosts to reach this machine on
TCP `22` for `pvecm qdevice setup`.

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

## Checking the QDevice

```bash
scripts/user_callable/diagnostics/show_qdevice_state.sh
```

This read-only report says whether the cluster needs a QDevice (it does with
an even number of members), whether one is registered in corosync.conf, and
whether every member sees it alive and voting. For a functional QDevice it
prints the registered address, each member's vote view, and the QDevice
host's OS, Tailscale address, `corosync-qnetd` version and state, and
connected clusters. When the QDevice is missing or has failed, it says what to
do next.

## Replacing a failed QDevice

When the cluster needs its QDevice and the registered one is inaccessible:

1. Run `scripts/user_callable/qdevice/remove_qdevice.sh`. It cannot remove an inaccessible QDevice
   gracefully, so it explains why and offers to remove it forcefully. If you
   accept, you must first remove the old machine from the Tailscale admin
   console and type `REMOVED FROM TAILSCALE`, so it can never communicate
   with the cluster again. The script then runs `pvecm qdevice remove`, and on
   every Proxmox node removes the QDevice client service and certificates and
   every `known_hosts` entry for the old QDevice, including the managed
   QDevice host-key block. Every member must be online. An even-member
   cluster has no tie-breaking vote until the replacement is added, so add it
   promptly.
2. On the workstation, remove the old machine's host key:
   `ssh-keygen -R qdevice`.
3. Prepare the replacement with sections 1 through 15 of this guide. Give it
   the same hostname and the `tag:proxmox-qdevice` tag. The Tailscale policy
   must still allow `tag:proxmox-host` to reach it on TCP `5403`, and on TCP
   `22` while the next step runs.
4. Run `scripts/user_callable/qdevice/add_qdevice.sh`. It checks that the cluster needs a QDevice,
   asks for the QDevice hostname (default `PROXMOX_QDEVICE_HOST`) and checks
   `ssh` access to it, refuses a Proxmox VE host, then does section 16 and the
   cluster side as host setup does. It installs and verifies `corosync-qnetd`,
   pins the new SSH host key on every member, runs `pvecm qdevice setup`
   through the control node, removes the temporary setup key, and checks that
   every member reports the QDevice alive and voting.

If you prepared the replacement under the same name before removing the old
one, the script sees that the reachable machine is not the registered one
(its Tailscale address differs). It then offers the same forced removal and
leaves the new machine untouched.

Host setup and `scripts/user_callable/hosts/remove_proxmox_host.sh` remove the QDevice the same way
when a membership change needs that: gracefully when it is accessible, and
otherwise forcefully, after the same explanation and Tailscale confirmation.
When the QDevice is inaccessible, they continue only if the cluster has an
odd number of members afterward, which needs no QDevice; otherwise they stop
before changing anything.

## Removing the QDevice

Use [`remove_qdevice.sh`](../scripts/user_callable/qdevice/remove_qdevice.sh) when intentionally retiring the
QDevice, replacing a failed one (above), or returning the dedicated host to a
pre-QDevice state for a full setup test:

```bash
scripts/user_callable/qdevice/remove_qdevice.sh [qdevice-host]
```

The script is deliberately destructive. It requires the exact interactive
confirmation `GO` and reaches the cluster through its control node with every
member online. When `ssh root@<qdevice-host>` works and that host is the
registered machine, the removal is graceful: the script refuses a Proxmox VE
node and, under the cluster control-plane lock, first detaches the QDevice
with `pvecm qdevice remove` and removes its host-key trust from every member.
If cluster-side detachment fails or cannot be proven, the external server is
left intact.

After safe detachment it removes the control node's setup key, QNetd
TLS/NSS identity and certificates, Corosync/QDevice services and packages,
package-specific state, and the `coroqnetd` account/group. It intentionally
does not run `apt autoremove` and does not erase general system journals.
That leaves a plain Ubuntu machine that can only act as a QDevice again if it
is deliberately re-added with `scripts/user_callable/qdevice/add_qdevice.sh`, so it stays enrolled
in Tailscale. If the QDevice is not accessible, the script offers the forced
removal described under "Replacing a failed QDevice". Review its complete
warning before use.
