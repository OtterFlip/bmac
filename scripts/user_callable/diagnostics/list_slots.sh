#!/usr/bin/env bash

# Copyright (c) 2026 BEENTHERE VENTURES, INC.
# SPDX-License-Identifier: GPL-3.0-only

# Read-only list of every fixed VM slot from cluster.conf, the guest (if any)
# registered in each, the FQDNs the Cloudflare Tunnel routes to each slot, and
# the Cloudflare DNS records each production domain needs.

set -u
set -o pipefail
umask 077

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
REPO_ROOT="$(cd -- "${SCRIPT_DIR}/../../.." && pwd -P)"
for library in config.sh ui_protocol.sh; do
  [[ -f "${REPO_ROOT}/scripts/lib/${library}" && ! -L "${REPO_ROOT}/scripts/lib/${library}" ]] || {
    printf 'ERROR: required library is unavailable: scripts/lib/%s\n' "$library" >&2
    exit 2
  }
  # shellcheck source=/dev/null
  source "${REPO_ROOT}/scripts/lib/${library}"
done
bmac_ui_bootstrap "$@"
RENDERER="${REPO_ROOT}/scripts/host_runtime/tunnel_config.py"
REMOTE_REGISTRY=/usr/local/lib/app-ha-proxmox/lib/cluster_registry.py

usage() {
  cat <<'EOF'
Usage: scripts/user_callable/diagnostics/list_slots.sh [--offline]

List every production and staging slot that cluster.conf reserves, with its
fixed private address, the registered guest using it (read from the registry
through one reachable mox host), and the FQDNs the Cloudflare Tunnel routes
to it. For each production domain it also lists the Cloudflare DNS records
that domain needs.

  --offline  Use cluster.conf only; don't read the registry.

Nothing is changed. Exit status: 0 on success, 1 when the configuration or
registry could not be read, 2 for usage errors.
EOF
}

OFFLINE=false
while (($#)); do
  case "$1" in
    --offline) OFFLINE=true ;;
    -h | --help) usage; exit 0 ;;
    *) usage >&2; exit 2 ;;
  esac
  shift
done

load_proxmox_config --no-secrets >/dev/null || exit 1

resources="[]"
registry_note="registry not read (--offline)"
if [[ "$OFFLINE" == false ]]; then
  if coordinator="$(first_reachable_mox 2>/dev/null)" &&
    resources="$(mox_ssh "$coordinator" "$REMOTE_REGISTRY" --state-dir "$CLUSTER_STATE_DIR" list </dev/null)"; then
    registry_note="registry read through $coordinator"
  else
    resources="[]"
    registry_note="registry unavailable: no reachable mox host; showing cluster.conf only"
  fi
fi

slots="$(
  python3 -I "$RENDERER" slots --network "$PRIVATE_SUBNET_CIDR" \
    --slot-start "$VM_SLOT_START" --max-prod "$MAX_PROD_VMS" \
    --max-staging "$MAX_STAGING_VMS_PER_PROD"
)" || exit 1

python3 -I - "$slots" "$resources" "$CLOUDFLARE_TUNNEL_ID" "$registry_note" <<'PY'
import json
import sys

slots, resources = json.loads(sys.argv[1]), json.loads(sys.argv[2])
tunnel_id, note = sys.argv[3], sys.argv[4]
by_name = {row["name"]: row for row in resources}
print(f"VM slots ({note})\n")
print(f"{'SLOT':<13} {'ADDRESS':<15} {'IN USE':<7} {'STATE':<16} FQDNS")
for slot in slots:
    name = slot["resource"]
    production = name if name.startswith("prod") else "prod" + name.split("prod", 1)[1]
    source = by_name.get(production)
    row = by_name.get(name)
    if name.startswith("prod"):
        fqdns = [source["domains"]["primary"], *source["domains"]["aliases"]] if source else []
    else:
        fqdns = [f"{name}.{source['domains']['primary']}"] if source else []
    print(
        f"{name:<13} {slot['ip']:<15} {'yes' if row else 'no':<7} "
        f"{(row or {}).get('state', '-'):<16} {', '.join(fqdns) or '-'}"
    )

productions = [row for row in resources if row.get("kind") == "production"]
print("\nCloudflare DNS records each production domain needs (proxied):")
if not productions:
    print("  none: no production VM is registered")
for row in sorted(productions, key=lambda value: value["index"]):
    primary = row["domains"]["primary"]
    zones = [primary] + [
        alias for alias in row["domains"]["aliases"]
        if not (alias.endswith("." + primary) and alias.count(".") == primary.count(".") + 1)
    ]
    for zone in zones:
        print(f"  {row['name']} - zone {zone}:")
        print(f"    CNAME  @  {tunnel_id}.cfargotunnel.com")
        print(f"    CNAME  *  {tunnel_id}.cfargotunnel.com")
print("\nSee README.md, 'Instructions for Cloudflare Setup', for HTTPS and bot protection.")
PY
