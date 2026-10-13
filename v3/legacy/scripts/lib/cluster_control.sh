#!/usr/bin/env bash

# Copyright (c) 2026 BEENTHERE VENTURES, INC.
# SPDX-License-Identifier: GPL-3.0-only

# Cluster membership as this host sees it, for the diagnostics. Source after
# scripts/lib/config.sh and load_proxmox_config.

# Callers read the CONTROL_* globals.
# shellcheck disable=SC2034

# shellcheck source=ui_protocol.sh
declare -F bmac_ui_is_json >/dev/null ||
  source "$(dirname -- "${BASH_SOURCE[0]}")/ui_protocol.sh"

# This host, once control_find_cluster finds it in a cluster.
CONTROL_PROBE_NODE=""
CONTROL_NODES_JSON=""
declare -ag CONTROL_MEMBER_NODES=()
declare -ag CONTROL_ONLINE_NODES=()

control_valid_node() {
  [[ "${1:-}" =~ ^mox([1-9]|[12][0-9]|3[0-2])$ ]] && ((${1#mox} <= MAX_MOX_HOSTS))
}

control_list_contains() {
  local wanted="$1" item
  shift
  for item in "$@"; do
    [[ "$item" != "$wanted" ]] || return 0
  done
  return 1
}

# Parse CONTROL_NODES_JSON (pvesh get /nodes) into the sorted member and
# online-member arrays.
control_parse_members() {
  local parsed node state
  parsed="$(
    python3 - "$CONTROL_NODES_JSON" "$MAX_MOX_HOSTS" <<'PY'
import json
import re
import sys

rows = json.loads(sys.argv[1])
maximum = int(sys.argv[2])
if not isinstance(rows, list) or not rows:
    raise SystemExit("pvesh /nodes returned no cluster members")
seen = set()
members = []
for row in rows:
    if not isinstance(row, dict):
        raise SystemExit("pvesh /nodes returned a malformed row")
    node = row.get("node", row.get("name"))
    match = re.fullmatch(r"mox([1-9]|[12][0-9]|3[0-2])", str(node))
    if not match or int(match.group(1)) > maximum:
        raise SystemExit(f"cluster contains unexpected node {node!r}")
    if node in seen:
        raise SystemExit(f"cluster returned duplicate node {node}")
    seen.add(node)
    online = (
        str(row.get("status", "")).lower() == "online"
        or row.get("online") in (1, True)
    )
    members.append((int(match.group(1)), node, "online" if online else "offline"))
for _, node, state in sorted(members):
    print(node, state)
PY
  )" || return 1
  CONTROL_MEMBER_NODES=()
  CONTROL_ONLINE_NODES=()
  while read -r node state; do
    [[ -n "$node" ]] || continue
    CONTROL_MEMBER_NODES+=("$node")
    [[ "$state" != online ]] || CONTROL_ONLINE_NODES+=("$node")
  done <<<"$parsed"
}

# Read cluster membership on this host. Returns 1 when this host does not
# belong to a cluster and 2 when it cannot tell which host it is or membership
# is malformed.
control_find_cluster() {
  local node nodes_json
  CONTROL_PROBE_NODE=""
  CONTROL_NODES_JSON=""
  CONTROL_MEMBER_NODES=()
  CONTROL_ONLINE_NODES=()
  node="$(this_mox)" || return 2
  nodes_json="$(
    mox_ssh "$node" bash -c \
      'test -s /etc/pve/corosync.conf && pvesh get /nodes --output-format json' \
      </dev/null 2>/dev/null
  )" || return 1
  CONTROL_PROBE_NODE="$node"
  CONTROL_NODES_JSON="$nodes_json"
  control_parse_members || return 2
}
