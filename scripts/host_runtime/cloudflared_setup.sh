#!/usr/bin/env bash

# Copyright (c) 2026 BEENTHERE VENTURES, INC.
# SPDX-License-Identifier: GPL-3.0-only

# Install cloudflared and this mox host's replica of the cluster's tunnel.
#
#   cloudflared_setup.sh install ACCOUNT_ID TUNNEL_ID   (tunnel secret on stdin)
#
# Installs cloudflared from Cloudflare's apt repository, writes the tunnel's
# credentials file (readable only by root and the cloudflared user), installs
# the sandboxed app-ha-cloudflared.service, and leaves the config file to
# tunnel_sync.sh. The secret never appears in a command line or the journal.

set -Eeuo pipefail
set +x
umask 022

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
RENDERER="${SCRIPT_DIR}/tunnel_config.py"
CREDENTIALS_FILE=/etc/cloudflared/bmac-tunnel.json
KEYRING=/usr/share/keyrings/cloudflare-main.gpg
SOURCES=/etc/apt/sources.list.d/cloudflared.list
UNIT=/etc/systemd/system/app-ha-cloudflared.service

die() {
  printf 'cloudflared_setup: %s\n' "$*" >&2
  exit 1
}

install_package() {
  if ! command -v cloudflared >/dev/null 2>&1; then
    install -d -m 0755 /usr/share/keyrings
    curl -fsSL https://pkg.cloudflare.com/cloudflare-main.gpg -o "${KEYRING}.new"
    mv -f "${KEYRING}.new" "$KEYRING"
    printf 'deb [signed-by=%s] https://pkg.cloudflare.com/cloudflared any main\n' \
      "$KEYRING" >"$SOURCES"
    if declare -F apt_wait_for_locks >/dev/null; then
      apt_wait_for_locks
    fi
    apt-get -o DPkg::Lock::Timeout=60 update
    DEBIAN_FRONTEND=noninteractive apt-get -o DPkg::Lock::Timeout=60 install -y cloudflared
  fi
  cloudflared --version
}

install_credentials() {
  local account_id="$1" tunnel_id="$2" secret temporary
  IFS= read -r secret || true
  [[ -n "$secret" ]] || die "the tunnel secret must be given on stdin"
  getent group cloudflared >/dev/null || groupadd --system cloudflared
  getent passwd cloudflared >/dev/null ||
    useradd --system --gid cloudflared --home-dir /nonexistent --shell /usr/sbin/nologin cloudflared
  install -d -m 0750 -o root -g cloudflared /etc/cloudflared
  temporary="$(mktemp /etc/cloudflared/.bmac-tunnel.json.XXXXXX)"
  CLOUDFLARE_TUNNEL_SECRET="$secret" python3 -I "$RENDERER" credentials \
    --account-id "$account_id" --tunnel-id "$tunnel_id" --output "$temporary"
  chown root:cloudflared "$temporary"
  chmod 0640 "$temporary"
  mv -f "$temporary" "$CREDENTIALS_FILE"
}

install_unit() {
  cat >"$UNIT" <<'EOF'
[Unit]
Description=BMAC Cloudflare Tunnel replica
After=network-online.target
Wants=network-online.target
ConditionPathExists=/etc/cloudflared/config.yml

[Service]
Type=notify
User=cloudflared
Group=cloudflared
ExecStart=/usr/bin/cloudflared --no-autoupdate --config /etc/cloudflared/config.yml tunnel run
Restart=on-failure
RestartSec=5
TimeoutStartSec=90
NoNewPrivileges=yes
ProtectSystem=strict
ProtectHome=yes
PrivateTmp=yes
PrivateDevices=yes
ProtectKernelTunables=yes
ProtectKernelModules=yes
ProtectKernelLogs=yes
ProtectControlGroups=yes
ProtectClock=yes
ProtectHostname=yes
RestrictNamespaces=yes
RestrictRealtime=yes
RestrictSUIDSGID=yes
LockPersonality=yes
MemoryDenyWriteExecute=yes
RestrictAddressFamilies=AF_INET AF_INET6 AF_UNIX AF_NETLINK
CapabilityBoundingSet=
AmbientCapabilities=
SystemCallArchitectures=native
ReadOnlyPaths=/etc/cloudflared

[Install]
WantedBy=multi-user.target
EOF
  # The package's own unit (if any) is not used.
  if systemctl list-unit-files cloudflared.service >/dev/null 2>&1; then
    systemctl disable --now cloudflared.service >/dev/null 2>&1 || true
  fi
  systemctl daemon-reload
  systemctl enable app-ha-cloudflared.service
}

main() {
  case "${1:-}" in
    install)
      (($# == 3)) || die "usage: cloudflared_setup.sh install ACCOUNT_ID TUNNEL_ID"
      install_package
      install_credentials "$2" "$3"
      install_unit
      ;;
    *) die "usage: cloudflared_setup.sh install ACCOUNT_ID TUNNEL_ID" ;;
  esac
}

main "$@"
