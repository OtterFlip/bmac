#!/usr/bin/env bash
# Copyright (c) 2026 BEENTHERE VENTURES, INC.
# SPDX-License-Identifier: GPL-3.0-only

# BMAC first boot for __HOST__ (DEPLOYMENT.md, "Installation and first
# boot"), written by the BMAC Deployer. The Proxmox installer runs it once,
# before the network is up. A tar payload follows the script at byte
# PAYLOAD_OFFSET + 1; bash never reads that far, because the script exits
# first. The BMAC packages are on the install media, at MEDIA_PACKAGES.
set -Eeuo pipefail
set +x
umask 077

PUBLIC_IP='__PUBLIC_IP__'
TAILSCALE_NAME='__TAILSCALE_NAME__'
PAYLOAD_OFFSET=__PAYLOAD_OFFSET__
PACKAGES_SHA256='__PACKAGES_SHA256__'
MEDIA_PACKAGES='__MEDIA_PACKAGES__'
PENDING=/var/lib/proxmox-first-boot/pending-first-boot-setup

[[ "${1:-before-network}" == before-network ]]
command -v nft >/dev/null

self="$(readlink -f -- "$0")"
work=/run/bmac-first-boot
rm -rf -- "$work"
install -d -m 0700 "$work"
trap 'rm -rf -- "$work"' EXIT
tail -c +"$((10#$PAYLOAD_OFFSET + 1))" -- "$self" |
  tar -x --no-same-owner --no-same-permissions -C "$work" -f -
cd "$work"
sha256sum --check --strict --quiet SHA256SUMS

# The SSH host key the workstation pinned before the image was built.
# Installed before sshd first serves, so no other Ed25519 host key is ever
# offered.
rm -f /etc/ssh/ssh_host_ed25519_key /etc/ssh/ssh_host_ed25519_key.pub
install -o root -g root -m 0600 ssh/ssh_host_ed25519_key /etc/ssh/ssh_host_ed25519_key
install -o root -g root -m 0644 ssh/ssh_host_ed25519_key.pub /etc/ssh/ssh_host_ed25519_key.pub
[[ "$(ssh-keygen -y -f /etc/ssh/ssh_host_ed25519_key | awk '{print $1, $2}')" == \
  "$(awk '{print $1, $2}' /etc/ssh/ssh_host_ed25519_key.pub)" ]]
systemctl try-restart ssh.service

# The public-port guard: nothing that answers on the tailnet answers on the
# public address.
install -d -m 0755 /usr/local/sbin /etc/nftables.d
cat >/etc/nftables.d/bmac-host-guard.nft <<EOF
table inet bmac_host_guard {
  chain input {
    type filter hook input priority -200; policy accept;
    ip daddr ${PUBLIC_IP} tcp dport { 22, 3128, 8006, 8443, 8444, 8445 } counter drop
  }
}
EOF
cat >/usr/local/sbin/bmac-load-host-guard <<'EOF'
#!/usr/bin/env bash
set -Eeuo pipefail
nft list table inet bmac_host_guard >/dev/null 2>&1 && nft delete table inet bmac_host_guard
nft -f /etc/nftables.d/bmac-host-guard.nft
EOF
chmod 0755 /usr/local/sbin/bmac-load-host-guard
cat >/etc/systemd/system/bmac-host-guard.service <<'EOF'
[Unit]
Description=BMAC public management-port guard
DefaultDependencies=no
Before=network-pre.target
Wants=network-pre.target
[Service]
Type=oneshot
ExecStart=/usr/local/sbin/bmac-load-host-guard
RemainAfterExit=yes
[Install]
WantedBy=network-pre.target
EOF
systemctl daemon-reload
systemctl enable bmac-host-guard.service
/usr/local/sbin/bmac-load-host-guard

# The administrator keys, plus the join's setup key on joining hosts.
install -d -m 0700 /root/.ssh
: >/root/.ssh/authorized_keys
cat authorized_keys >>/root/.ssh/authorized_keys
chmod 0600 /root/.ssh/authorized_keys

# The HTTPS key, root only; bmacd-web gets it through LoadCredential.
install -d -m 0755 /etc/bmac
install -d -m 0700 /etc/bmac/tls
install -o root -g root -m 0600 tls/https.key /etc/bmac/tls/https.key
install -o root -g root -m 0644 tls/https.pem /etc/bmac/tls/https.pem

# The new-cluster seed or the join bundle, for bmacd's setup mode.
install -d -m 0700 /var/lib/bmac /var/lib/bmac/bootstrap
install -o root -g root -m 0600 bootstrap/bootstrap.json /var/lib/bmac/bootstrap/bootstrap.json

# Tailscale, once the network is up, with the single-use key.
install -d -m 0700 /root/.bmac-bootstrap
install -o root -g root -m 0600 tailscale/auth-key /root/.bmac-bootstrap/tailscale-auth-key
cat >/usr/local/sbin/bmac-bootstrap-tailscale <<EOF
#!/usr/bin/env bash
set -Eeuo pipefail
set +x

# network-online.target can be reached a few seconds before DNS and external
# HTTPS work. Wait here so a resolver race at first boot doesn't strand the
# host outside Tailscale.
repo_ready=false
for attempt in \$(seq 1 60); do
  if getent ahosts pkgs.tailscale.com >/dev/null 2>&1 &&
    curl -fsSI --connect-timeout 10 --max-time 30 \
      https://pkgs.tailscale.com/stable/debian/trixie.noarmor.gpg \
      >/dev/null; then
    repo_ready=true
    break
  fi
  echo "Waiting for DNS and HTTPS access to pkgs.tailscale.com (attempt \$attempt/60)"
  sleep 5
done
\$repo_ready || {
  echo "The Tailscale repository stayed unreachable for 5 minutes" >&2
  exit 1
}

install -d -m 0755 /usr/share/keyrings
curl -fsSL --retry 12 --retry-delay 5 --retry-all-errors \
  https://pkgs.tailscale.com/stable/debian/trixie.noarmor.gpg \
  -o /usr/share/keyrings/tailscale-archive-keyring.gpg
curl -fsSL --retry 12 --retry-delay 5 --retry-all-errors \
  https://pkgs.tailscale.com/stable/debian/trixie.tailscale-keyring.list \
  -o /etc/apt/sources.list.d/tailscale.list
chmod 0644 /usr/share/keyrings/tailscale-archive-keyring.gpg /etc/apt/sources.list.d/tailscale.list
apt-get update -o Acquire::Retries=5 -o Dir::Etc::sourcelist=/etc/apt/sources.list.d/tailscale.list -o Dir::Etc::sourceparts=- -o APT::Get::List-Cleanup=0
DEBIAN_FRONTEND=noninteractive apt-get install -y tailscale
systemctl enable --now tailscaled
tailscale up --auth-key=file:/root/.bmac-bootstrap/tailscale-auth-key --hostname=${TAILSCALE_NAME}
tailscale ip -4 | grep -E '^100\.'
rm -f /root/.bmac-bootstrap/tailscale-auth-key
touch /var/lib/bmac-bootstrap-complete
chmod 0600 /var/lib/bmac-bootstrap-complete
EOF
chmod 0700 /usr/local/sbin/bmac-bootstrap-tailscale
cat >/etc/systemd/system/bmac-bootstrap.service <<'EOF'
[Unit]
Description=BMAC first-boot Tailscale enrollment
After=network-online.target nss-lookup.target bmac-host-guard.service
Wants=network-online.target nss-lookup.target
Requires=bmac-host-guard.service
ConditionPathExists=!/var/lib/bmac-bootstrap-complete
StartLimitIntervalSec=1800
StartLimitBurst=6
[Service]
Type=oneshot
ExecStart=/usr/local/sbin/bmac-bootstrap-tailscale
RemainAfterExit=yes
Restart=on-failure
RestartSec=30s
[Install]
WantedBy=multi-user.target
EOF
systemctl daemon-reload
systemctl enable bmac-bootstrap.service
systemctl start --no-block bmac-bootstrap.service

# The BMAC packages, from the install media, which the operator removes
# only once the installed system's login prompt appears. Any medium will
# do whose copy has the digest this script was built with.
packages_found=false
install -d -m 0700 "$work/media"
for attempt in $(seq 1 20); do
  udevadm settle --timeout=10 || true
  for device in $(blkid -c /dev/null -t TYPE=iso9660 -o device || true); do
    mount -o ro -t iso9660 "$device" "$work/media" 2>/dev/null || continue
    if [[ -f "$work/media$MEDIA_PACKAGES" ]]; then
      cp -- "$work/media$MEDIA_PACKAGES" "$work/packages.tar"
    fi
    umount "$work/media" || true
    if [[ -f "$work/packages.tar" ]] &&
      [[ "$(sha256sum "$work/packages.tar" | awk '{print $1}')" == "$PACKAGES_SHA256" ]]; then
      packages_found=true
      break 2
    fi
    rm -f -- "$work/packages.tar"
  done
  echo "Waiting for the install media with the BMAC packages (attempt $attempt/20)"
  sleep 3
done

# Every secret above now has its one root-only copy, so the installer's copy
# of this script and its payload goes.
rm -f -- "$self" "$PENDING"

if ! $packages_found; then
  message="BMAC first boot: the install media with the BMAC packages wasn't found, so \
bmacd isn't installed. Attach the host ISO again and run: dpkg -i on the \
packages in ${MEDIA_PACKAGES}, then systemctl start bmacd. SSH and Tailscale are set up."
  echo "$message" >&2
  echo "$message" >/dev/console 2>/dev/null || true
  exit 1
fi
tar -x --no-same-owner --no-same-permissions -C "$work" -f "$work/packages.tar"
# The signed bundle the packages came from: bmacd keeps it as the installed
# bundle, so the host's first update has one to roll back to.
if compgen -G "$work/bundles/*.bundle" >/dev/null; then
  install -d -m 0700 /var/lib/bmac /var/lib/bmac/updates /var/lib/bmac/updates/seed
  install -m 0600 "$work"/bundles/*.bundle /var/lib/bmac/updates/seed/
fi
dpkg -i "$work"/packages/*.deb

# bmacd starts once the network is up; with the bootstrap file present and
# no cluster yet, it starts in setup mode.
systemctl enable bmacd.service
systemctl start --no-block bmacd.service
exit 0
