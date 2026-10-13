#!/usr/bin/env bash

# Copyright (c) 2026 BEENTHERE VENTURES, INC.
# SPDX-License-Identifier: GPL-3.0-only

# The host setup job's final verification on this host: v2's add_proxmox_host.sh
# final_verify, run by bmacd's setup job (IMPLEMENTATION_PLAN.md, M7.3).
# Usage: verify_host.sh luks|none. bmacd renders the settings into
# /run/bmac/config and names this host in BMAC_HOST. It changes nothing.

set -Eeuo pipefail
set +x
umask 077

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
LIB_DIR="${SCRIPT_DIR}/../lib"
[[ -f "$LIB_DIR/config.sh" && ! -L "$LIB_DIR/config.sh" ]] || {
  printf 'ERROR: Missing library: %s\n' "$LIB_DIR/config.sh" >&2
  exit 1
}
# shellcheck source=../lib/config.sh
source "$LIB_DIR/config.sh"

HOOK_NAME=bmac-guest-role-hook.sh
HEADER_DIR=/var/lib/bmac/luks-headers

check_pool() {
  local encryption="$1" pool mappers mapper
  zpool status -P rpool
  zpool status -x rpool | grep -F "pool 'rpool' is healthy"
  pool="$(zpool status -P rpool)"
  mappers="$(awk '$1 ~ /^bmac-rpool-/ { print $1 }' /etc/crypttab 2>/dev/null || true)"
  if [[ "$encryption" == luks ]]; then
    [[ -n "$mappers" ]] || {
      printf 'The pool is meant to be encrypted, but /etc/crypttab lists no rpool member.\n' >&2
      exit 1
    }
    while IFS= read -r mapper; do
      awk -v member="/dev/mapper/$mapper" \
        '$1 == member { found++ } END { exit !(found == 1) }' <<<"$pool"
      awk -v mapper="$mapper" '
        $1 == mapper {
          if ($3 != "bmac-rpool" ||
              $4 !~ /(^|,)keyscript=decrypt_keyctl(,|$)/) exit 1
          found++
        }
        END { if (found != 1) exit 1 }
      ' /etc/crypttab
      [[ -s "${HEADER_DIR}/${mapper}.luks-header" ]] || {
        printf 'No LUKS header backup for %s.\n' "$mapper" >&2
        exit 1
      }
    done <<<"$mappers"
    lsinitramfs "/boot/initrd.img-$(uname -r)" |
      awk '/(^|\/)decrypt_keyctl$/ { found=1 } END { exit !found }'
  else
    [[ -z "$mappers" ]] || {
      printf 'The pool is meant to be unencrypted, but /etc/crypttab lists rpool members.\n' >&2
      exit 1
    }
  fi
  [[ -z "$(swapon --noheadings --show=NAME)" ]]
  proxmox-boot-tool status
}

check_network() {
  local bridge="$PROXMOX_PRIVATE_BRIDGE"
  ip -4 address show dev "$bridge"
  python3 - "$MOX_REPLICATION_IP" "$(ip -j -4 address show dev "$bridge")" <<'PY'
import ipaddress
import json
import sys

expected = ipaddress.ip_interface(sys.argv[1])
rows = json.loads(sys.argv[2])
matching = [
    ipaddress.ip_address(info["local"])
    for row in rows
    for info in row.get("addr_info", [])
    if info.get("family") == "inet"
    and ipaddress.ip_address(info["local"]) in expected.network
]
if matching != [expected.ip]:
    raise SystemExit(
        f"migration CIDR {expected.network} must match exactly {expected.ip}; "
        f"found {matching}"
    )
PY
  tailscale status
}

check_ingress() {
  local vmid="$HAPROXY_LXC_VMID"
  pct status "$vmid"
  pct exec "$vmid" -- systemctl is-active haproxy
  pct exec "$vmid" -- haproxy -c -f /etc/haproxy/haproxy.cfg
  pct exec "$vmid" -- grep -Fq \
    'Managed by bmac HAProxy route synchronization' /etc/haproxy/haproxy.cfg
  nft list table inet bmac_host_guard >/dev/null
  nft list table inet bmac_haproxy_ingress
  systemctl is-active bmac-haproxy-ingress.service
  systemctl is-enabled --quiet bmac-haproxy-route-sync.timer
  systemctl is-active --quiet bmac-haproxy-route-sync.timer
}

vip_state() {
  local node="$1" bridge="$PROXMOX_PRIVATE_BRIDGE" vip="${GUEST_EGRESS_VIP%/*}" pin
  local probe="if ip -4 -o address show dev '$bridge' | awk -v vip='$vip' '\$4 ~ (\"^\" vip \"/\") { found=1 } END { exit !found }'; then printf 'owner\n'; else printf 'backup\n'; fi"
  if [[ "$node" == "$MOX_HOSTNAME" ]]; then
    bash -c "$probe"
    return
  fi
  pin="/etc/pve/nodes/${node}/ssh_known_hosts"
  if [[ ! -s "$pin" || -L "$pin" ]]; then
    printf '%s has no SSH host key pin in %s.\n' "$node" "$pin" >&2
    return 1
  fi
  # -n: the caller's loop reads the node list from stdin.
  ssh -n -o BatchMode=yes -o ClearAllForwardings=yes \
    -o StrictHostKeyChecking=yes -o CheckHostIP=no \
    -o "HostKeyAlias=${node}" -o "UserKnownHostsFile=${pin}" \
    -o GlobalKnownHostsFile=none "root@${node}.${PROXMOX_INTERNAL_DOMAIN}" "$probe"
}

check_egress() {
  local vip="${GUEST_EGRESS_VIP%/*}" private_ip="${PROXMOX_SECONDARY_IP%/*}"
  local owner owners queried online_count failed before after node state index candidate
  nft list table ip bmac_guest_egress >/dev/null
  /usr/local/sbin/bmac-check-egress-router
  systemctl is-active keepalived.service
  keepalived --config-test --use-file=/etc/keepalived/keepalived.conf
  for _ in $(seq 1 10); do
    owner="" owners=0 queried=0 failed=false
    before="$(pvesh get /nodes --output-format json |
      jq -r '[.[] | select(.status == "online") | .node] | sort | join("\n")')"
    online_count="$(grep -c . <<<"$before" || true)"
    while IFS= read -r node; do
      [[ -n "$node" ]] || continue
      if state="$(vip_state "$node")"; then
        queried=$((queried + 1))
        if [[ "$state" == owner ]]; then
          owner="$node"
          owners=$((owners + 1))
        elif [[ "$state" != backup ]]; then
          failed=true
        fi
      else
        failed=true
      fi
    done <<<"$before"
    after="$(pvesh get /nodes --output-format json |
      jq -r '[.[] | select(.status == "online") | .node] | sort | join("\n")')"
    if [[ "$failed" == false && "$before" == "$after" ]] &&
      ((queried == online_count && owners == 1)); then
      break
    fi
    sleep 5
  done
  if [[ "$failed" != false || "$before" != "$after" ]] ||
    ((queried != online_count || owners != 1)); then
    printf 'Could not prove exactly one stable, reachable online host owns the guest-egress VIP %s (queried %s/%s, owners %s).\n' \
      "$vip" "$queried" "$online_count" "$owners" >&2
    exit 1
  fi
  printf 'The guest-egress VIP %s is owned by exactly one host: %s\n' "$vip" "$owner"
  grep -Fq "$vip" /etc/keepalived/keepalived.conf
  grep -Eq "^[[:space:]]*priority[[:space:]]+${VRRP_PRIORITY}[[:space:]]*$" \
    /etc/keepalived/keepalived.conf
  for ((index = 1; index <= MAX_MOX_HOSTS; index += 1)); do
    candidate="$(mox_private_ip "mox${index}")"
    if grep -Eq "^[[:space:]]*${candidate}[[:space:]]*$" /etc/keepalived/keepalived.conf; then
      [[ "$candidate" != "$private_ip" ]] || {
        printf 'keepalived lists this host, %s, as its own VRRP peer.\n' "$candidate" >&2
        exit 1
      }
    elif [[ "$candidate" != "$private_ip" ]]; then
      printf 'keepalived does not list the VRRP peer %s.\n' "$candidate" >&2
      exit 1
    fi
  done
}

check_runtime() {
  bash -n "/var/lib/vz/snippets/$HOOK_NAME"
  bash -n "${SCRIPT_DIR}/../host_runtime/process_deferred_cleanup.sh"
  systemctl is-enabled --quiet bmac-deferred-cleanup.timer
  systemctl is-active --quiet bmac-deferred-cleanup.timer
  systemctl is-active bmacd.service bmacd-web.service
}

main() {
  [[ $# == 1 && ("$1" == luks || "$1" == none) ]] || {
    printf 'Usage: verify_host.sh luks|none\n' >&2
    exit 2
  }
  local host
  host="$(this_mox)"
  load_proxmox_config --host "$host"
  pveversion
  pvecm status
  check_pool "$1"
  check_network
  check_ingress
  check_egress
  check_runtime
  printf '\n%s passed the final verification.\n' "$host"
}

main "$@"
