#!/usr/bin/env bash

# Copyright (c) 2026 BEENTHERE VENTURES, INC.
# SPDX-License-Identifier: GPL-3.0-only

# Bring every mox host's Cloudflare Tunnel replica up to date with the
# registry. Each host renders the same config file from the same registry and
# cluster.conf, so the file only changes when a production guest is allocated
# or released, or when the slot settings change. A host whose file is current
# does nothing; one whose file changed installs it and restarts its replica,
# one host at a time, so the other replicas keep serving.

set -Eeuo pipefail
set +x
umask 022

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
CONFIG_LIB="${SCRIPT_DIR}/config.sh"
REGISTRY="${SCRIPT_DIR}/cluster_registry.py"
RENDERER="${SCRIPT_DIR}/tunnel_config.py"
INSTALLED_SYNC="/usr/local/lib/app-ha-proxmox/lib/tunnel_sync.sh"
CONFIG_FILE=/etc/cloudflared/config.yml
CREDENTIALS_FILE=/etc/cloudflared/bmac-tunnel.json
SERVICE=app-ha-cloudflared.service
READY_URL=http://127.0.0.1:20241/ready
LOCK_FILE=/run/lock/app-ha-tunnel-sync.lock
READY_TIMEOUT_SECONDS=90
MODE=all

log() {
  printf '[app-ha tunnel] %s\n' "$*"
}

die() {
  printf 'ERROR: %s\n' "$*" >&2
  exit 1
}

usage() {
  cat <<EOF
Usage: $0 [--local | --all | --check]

Renders the Cloudflare Tunnel configuration from the registry and the
installed cluster.conf, and installs it where it changed.

  --all     This host first, then every other online mox host, one at a time
            (default).
  --local   Only this host.
  --check   Print this host's installed and expected generations; exit 1 if
            they differ.
EOF
}

load_config() {
  [[ -f "$CONFIG_LIB" ]] || die "missing $CONFIG_LIB"
  # shellcheck source=/dev/null
  source "$CONFIG_LIB"
  load_proxmox_config --no-secrets || die "could not load the installed cluster.conf"
}

expected_config() {
  "$REGISTRY" --state-dir "$CLUSTER_STATE_DIR" tunnel-productions |
    python3 -I "$RENDERER" render \
      --network "$PRIVATE_SUBNET_CIDR" \
      --slot-start "$VM_SLOT_START" \
      --max-prod "$MAX_PROD_VMS" \
      --max-staging "$MAX_STAGING_VMS_PER_PROD" \
      --tunnel-id "$CLOUDFLARE_TUNNEL_ID" \
      --credentials-file "$CREDENTIALS_FILE" \
      --productions -
}

installed_generation() {
  python3 -I "$RENDERER" generation "$CONFIG_FILE" 2>/dev/null || true
}

wait_until_ready() {
  local deadline=$((SECONDS + READY_TIMEOUT_SECONDS)) body
  while ((SECONDS < deadline)); do
    if body="$(curl -fsS --max-time 3 "$READY_URL" 2>/dev/null)" &&
      python3 -I -c 'import json,sys; sys.exit(0 if json.loads(sys.argv[1]).get("readyConnections", 0) > 0 else 1)' "$body"; then
      return 0
    fi
    sleep 2
  done
  return 1
}

sync_local() {
  local rendered expected installed temporary
  [[ -s "$CREDENTIALS_FILE" ]] || die "$CREDENTIALS_FILE is missing; rerun host setup"
  rendered="$(expected_config)" || die "could not render the tunnel configuration"
  expected="$(python3 -I -c 'import sys; print(sys.stdin.readline().split(": ", 1)[1].strip())' <<<"$rendered")"
  installed="$(installed_generation)"
  if [[ "$installed" == "$expected" ]] && systemctl is-active --quiet "$SERVICE"; then
    log "$(hostname -s) is current (generation ${expected:0:12})"
    return 0
  fi
  install -d -m 0755 /etc/cloudflared
  temporary="$(mktemp /etc/cloudflared/.config.yml.XXXXXX)"
  printf '%s\n' "$rendered" >"$temporary"
  chmod 0644 "$temporary"
  mv -f "$temporary" "$CONFIG_FILE"
  log "$(hostname -s): installed generation ${expected:0:12}; restarting its replica"
  systemctl restart "$SERVICE"
  wait_until_ready ||
    die "$(hostname -s)'s replica did not reconnect within ${READY_TIMEOUT_SECONDS} seconds"
  log "$(hostname -s)'s replica is connected"
}

sync_all() {
  local nodes_json current node destination pin index
  local -a ssh_options
  locked_sync_local
  current="$(hostname -s)"
  nodes_json="$(pvesh get /nodes --output-format json)"
  for ((index = 1; index <= MAX_MOX_HOSTS; index += 1)); do
    node="mox${index}"
    [[ "$node" != "$current" ]] || continue
    jq -e --arg node "$node" 'any(.[]; .node == $node and .status == "online")' \
      <<<"$nodes_json" >/dev/null || continue
    destination="${node}.${PROXMOX_INTERNAL_DOMAIN}"
    pin="/etc/pve/nodes/${node}/ssh_known_hosts"
    [[ -s "$pin" && ! -L "$pin" ]] || die "missing SSH host key pin for $node"
    ssh_options=(
      -o BatchMode=yes
      -o ClearAllForwardings=yes
      -o "ConnectTimeout=${MOX_SSH_CONNECT_TIMEOUT:-8}"
      -o StrictHostKeyChecking=yes
      -o CheckHostIP=no
      -o GlobalKnownHostsFile=none
      -o "HostKeyAlias=${node}"
      -o "UserKnownHostsFile=${pin}"
    )
    ssh "${ssh_options[@]}" "root@${destination}" "$INSTALLED_SYNC --local" </dev/null ||
      die "$node could not apply the tunnel configuration"
  done
}

main() {
  while (($#)); do
    case "$1" in
      --all) MODE=all ;;
      --local) MODE=local ;;
      --check) MODE=check ;;
      -h | --help)
        usage
        return 0
        ;;
      *) die "Unknown option: $1" ;;
    esac
    shift
  done
  load_config
  if [[ "$MODE" == check ]]; then
    local expected installed
    expected="$(expected_config | head -n1)"
    expected="${expected#*: }"
    installed="$(installed_generation)"
    printf 'installed=%s\nexpected=%s\n' "${installed:-none}" "$expected"
    [[ "$installed" == "$expected" ]]
    return
  fi
  if [[ "$MODE" == local ]]; then
    locked_sync_local
  else
    sync_all
  fi
}

# Only one sync changes a host's file at a time. The lock is never held while
# waiting on another host, so two hosts syncing everything can't deadlock.
locked_sync_local() {
  (
    flock -w 300 9 || die "another tunnel sync is still running on $(hostname -s)"
    sync_local
  ) 9>"$LOCK_FILE"
}

main "$@"
