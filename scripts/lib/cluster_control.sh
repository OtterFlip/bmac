#!/usr/bin/env bash

# Copyright (c) 2026 BEENTHERE VENTURES, INC.
# SPDX-License-Identifier: GPL-3.0-only

# Workstation helpers that resolve the cluster control node: the member that
# new hosts join through and that hosts the cluster control-plane lock. Once a
# cluster exists, its registry records the authoritative control node and
# config/cluster.conf PROXMOX_CONTROL_NODE must agree with it. Before a cluster
# exists, PROXMOX_CONTROL_NODE (or the operator's answer) names the host that
# creates it. Source after scripts/lib/config.sh and load_proxmox_config.

# Callers read the CONTROL_* globals.
# shellcheck disable=SC2034

# shellcheck source=ui_protocol.sh
declare -F bmac_ui_is_json >/dev/null ||
  source "$(dirname -- "${BASH_SOURCE[0]}")/ui_protocol.sh"

CONTROL_REMOTE_REGISTRY="/usr/local/lib/app-ha-proxmox/lib/cluster_registry.py"

# The resolved control node.
CONTROL_NODE=""
# The control node recorded in the cluster registry, or empty when none is.
CONTROL_RECORDED_NODE=""
# A reachable cluster member used to read cluster state, or empty when no
# cluster member is reachable.
CONTROL_PROBE_NODE=""
# true when the installed registry on CONTROL_PROBE_NODE supports host slots.
CONTROL_REGISTRY_SUPPORTS_HOSTS=false
CONTROL_NODES_JSON=""
declare -ag CONTROL_MEMBER_NODES=()
declare -ag CONTROL_ONLINE_NODES=()

control_valid_node() {
  [[ "${1:-}" =~ ^mox([1-9]|10)$ ]] && ((${1#mox} <= MAX_MOX_HOSTS))
}

control_list_contains() {
  local wanted="$1" item
  shift
  for item in "$@"; do
    [[ "$item" != "$wanted" ]] || return 0
  done
  return 1
}

control_registry() {
  local node="$1"
  shift
  mox_ssh "$node" "$CONTROL_REMOTE_REGISTRY" \
    --state-dir "$CLUSTER_STATE_DIR" "$@" </dev/null
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
    match = re.fullmatch(r"mox([1-9]|10)", str(node))
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

# Wait until the background reachability probe PID finishes or SECONDS reaches
# DEADLINE, and succeed only when it finished and found its host reachable.
control_probe_reachable() {
  local pid="$1" deadline="$2"
  while kill -0 "$pid" 2>/dev/null && ((SECONDS < deadline)); do
    sleep 0.1
  done
  ! kill -0 "$pid" 2>/dev/null && wait "$pid" 2>/dev/null
}

control_stop_probes() {
  local pid
  for pid in "$@"; do
    if kill -0 "$pid" 2>/dev/null; then
      pkill -P "$pid" 2>/dev/null || true
      kill "$pid" 2>/dev/null || true
    fi
  done
  wait "$@" 2>/dev/null || true
}

# Find the lowest-numbered reachable host that belongs to a cluster. Returns 1
# when no cluster member is reachable and 2 when membership is malformed.
# Every slot is probed at once, and a slot that has not answered within
# CONTROL_PROBE_TIMEOUT_SECONDS counts as unreachable, so a workstation with no
# cluster yet does not wait out each slot's SSH timeout in turn.
control_find_cluster() {
  local index node nodes_json deadline status=1
  local -a probes=()
  CONTROL_PROBE_NODE=""
  CONTROL_NODES_JSON=""
  CONTROL_MEMBER_NODES=()
  CONTROL_ONLINE_NODES=()
  for ((index = 1; index <= MAX_MOX_HOSTS; index += 1)); do
    mox_is_reachable "mox${index}" </dev/null >/dev/null 2>&1 &
    probes+=("$!")
  done
  deadline=$((SECONDS + ${CONTROL_PROBE_TIMEOUT_SECONDS:-15}))
  for ((index = 1; index <= MAX_MOX_HOSTS; index += 1)); do
    node="mox${index}"
    control_probe_reachable "${probes[index - 1]}" "$deadline" || continue
    nodes_json="$(
      mox_ssh "$node" bash -c \
        'test -s /etc/pve/corosync.conf && pvesh get /nodes --output-format json' \
        </dev/null 2>/dev/null
    )" || continue
    CONTROL_PROBE_NODE="$node"
    CONTROL_NODES_JSON="$nodes_json"
    status=0
    control_parse_members || status=2
    break
  done
  control_stop_probes "${probes[@]}"
  return "$status"
}

# Read the registry control record through CONTROL_PROBE_NODE. A registry that
# is not installed or predates host slots records no control node.
control_read_recorded() {
  local help_text value
  CONTROL_RECORDED_NODE=""
  CONTROL_REGISTRY_SUPPORTS_HOSTS=false
  mox_ssh "$CONTROL_PROBE_NODE" test -x "$CONTROL_REMOTE_REGISTRY" \
    </dev/null 2>/dev/null || return 0
  help_text="$(control_registry "$CONTROL_PROBE_NODE" --help)" || return 1
  grep -q -- 'control-get' <<<"$help_text" || return 0
  CONTROL_REGISTRY_SUPPORTS_HOSTS=true
  value="$(control_registry "$CONTROL_PROBE_NODE" control-get)" || return 1
  CONTROL_RECORDED_NODE="$(
    python3 -c '
import json, sys
value = json.loads(sys.argv[1])
print(value["control_node"] if value else "")
' "$value"
  )" || return 1
  [[ -z "$CONTROL_RECORDED_NODE" ]] || control_valid_node "$CONTROL_RECORDED_NODE"
}

control_mismatch_message() {
  printf 'The cluster registry records %s as the control node, but %s. Set PROXMOX_CONTROL_NODE=%s in %s and rerun.' \
    "$CONTROL_RECORDED_NODE" "$1" "$CONTROL_RECORDED_NODE" "$PROXMOX_CLUSTER_CONFIG"
}

# Resolve CONTROL_NODE. With --allow-offline the control node may be an
# offline member (purge uses this to replace a dead control node).
resolve_control_node() {
  local allow_offline=false configured="${PROXMOX_CONTROL_NODE:-}" status=0
  local default answer
  [[ "${1:-}" != --allow-offline ]] || allow_offline=true
  CONTROL_NODE=""
  CONTROL_RECORDED_NODE=""
  control_find_cluster || status=$?
  if ((status == 2)); then
    config_die "Cluster membership reported through $CONTROL_PROBE_NODE is malformed"
    return 1
  fi

  if ((status == 0)); then
    control_read_recorded ||
      config_die "Could not read the control node from the cluster registry through $CONTROL_PROBE_NODE" ||
      return 1
    if [[ -n "$CONTROL_RECORDED_NODE" ]]; then
      default="$CONTROL_RECORDED_NODE"
    elif [[ -n "$configured" ]]; then
      default="$configured"
    else
      default="${CONTROL_ONLINE_NODES[0]:-${CONTROL_MEMBER_NODES[0]}}"
    fi
  else
    default="${configured:-mox1}"
  fi

  if [[ -n "$configured" ]]; then
    answer="$configured"
  else
    printf '\nPROXMOX_CONTROL_NODE is not set in %s.\n' "$PROXMOX_CLUSTER_CONFIG"
    if ((status == 0)); then
      printf 'Cluster members: %s (online: %s)\n' \
        "${CONTROL_MEMBER_NODES[*]}" "${CONTROL_ONLINE_NODES[*]:-none}"
    else
      printf 'No reachable cluster member was found; the control node will create the cluster.\n'
    fi
    if bmac_ui_is_json; then
      bmac_ui_input answer --id control_node --label "Cluster control node" \
        --default "$default" --required --pattern '^mox([1-9]|10)$' \
        --help "PROXMOX_CONTROL_NODE is not set in config/cluster.conf. The control node coordinates cluster changes."
    else
      IFS= read -r -p "Cluster control node [${default}]: " answer ||
        config_die "Input ended before the control node was chosen" || return 1
    fi
    answer="${answer:-$default}"
  fi
  control_valid_node "$answer" ||
    config_die "Control node must be mox1 through mox${MAX_MOX_HOSTS}: ${answer}" ||
    return 1

  if ((status == 0)); then
    if [[ -n "$CONTROL_RECORDED_NODE" && "$answer" != "$CONTROL_RECORDED_NODE" ]]; then
      if [[ -n "$configured" ]]; then
        config_die "$(control_mismatch_message "PROXMOX_CONTROL_NODE=${configured} in config/cluster.conf")"
      else
        config_die "$(control_mismatch_message "${answer} was entered")"
      fi
      return 1
    fi
    control_list_contains "$answer" "${CONTROL_MEMBER_NODES[@]}" ||
      config_die "Control node $answer is not a member of the cluster (members: ${CONTROL_MEMBER_NODES[*]})" ||
      return 1
    if [[ "$allow_offline" != true ]]; then
      control_list_contains "$answer" "${CONTROL_ONLINE_NODES[@]}" ||
        config_die "Control node $answer is not online (online members: ${CONTROL_ONLINE_NODES[*]:-none})" ||
        return 1
    fi
  fi
  CONTROL_NODE="$answer"
  if [[ -z "$configured" ]]; then
    printf 'Add PROXMOX_CONTROL_NODE=%s to %s to skip this prompt.\n' \
      "$CONTROL_NODE" "$PROXMOX_CLUSTER_CONFIG"
  fi
}

# Replace (or add, after PROXMOX_QDEVICE_HOST) the KEY=VALUE line in a
# cluster.conf file, preserving every other line and the file mode.
cluster_conf_set_value() {
  local path="$1" name="$2" value="$3"
  [[ "$name" =~ ^[A-Z][A-Z0-9_]*$ && "$value" =~ ^[A-Za-z0-9._-]+$ ]] || return 1
  [[ -f "$path" && ! -L "$path" ]] || return 1
  python3 - "$path" "$name" "$value" <<'PY'
from pathlib import Path
import contextlib
import os
import sys
import tempfile

path = Path(sys.argv[1])
key = sys.argv[2] + "="
value = sys.argv[3]
lines = path.read_text(encoding="utf-8").splitlines()
matches = [index for index, line in enumerate(lines) if line.startswith(key)]
if len(matches) > 1:
    raise SystemExit(f"{key[:-1]} appears more than once in {path}")
if matches:
    lines[matches[0]] = key + value
else:
    anchor = next(
        (
            index
            for index, line in enumerate(lines)
            if line.startswith("PROXMOX_QDEVICE_HOST=")
        ),
        len(lines) - 1,
    )
    lines.insert(anchor + 1, key + value)
mode = path.stat().st_mode & 0o7777
descriptor, temporary = tempfile.mkstemp(dir=path.parent, prefix=f".{path.name}.")
try:
    with os.fdopen(descriptor, "w", encoding="utf-8") as handle:
        handle.write("\n".join(lines) + "\n")
    os.chmod(temporary, mode)
    os.replace(temporary, path)
except BaseException:
    with contextlib.suppress(FileNotFoundError):
        os.unlink(temporary)
    raise
PY
}

control_rewrite_cluster_conf() {
  local path="$1" node="$2"
  control_valid_node "$node" || return 1
  cluster_conf_set_value "$path" PROXMOX_CONTROL_NODE "$node"
}

# Tell the operator about a control-node change and offer to update this
# workstation's config/cluster.conf.
control_offer_cluster_conf_update() {
  local node="$1" answer
  printf '\nThe cluster control node is now %s.\n' "$node"
  if [[ "${PROXMOX_CONTROL_NODE:-}" == "$node" ]]; then
    printf '%s already sets PROXMOX_CONTROL_NODE=%s.\n' \
      "$PROXMOX_CLUSTER_CONFIG" "$node"
    return 0
  fi
  printf 'Every workstation that runs the host scripts must set PROXMOX_CONTROL_NODE=%s in config/cluster.conf.\n' \
    "$node"
  if bmac_ui_is_json; then
    bmac_ui_ask "Update PROXMOX_CONTROL_NODE in ${PROXMOX_CLUSTER_CONFIG} to ${node} now?" \
      "Update it" "Leave it" && answer=y || answer=n
  else
    IFS= read -r -p "Update PROXMOX_CONTROL_NODE in ${PROXMOX_CLUSTER_CONFIG} now? [Y/n] " answer ||
      answer=n
  fi
  if [[ -z "$answer" || "${answer,,}" == y || "${answer,,}" == yes ]]; then
    if control_rewrite_cluster_conf "$PROXMOX_CLUSTER_CONFIG" "$node"; then
      PROXMOX_CONTROL_NODE="$node"
      printf 'Updated %s: PROXMOX_CONTROL_NODE=%s\n' "$PROXMOX_CLUSTER_CONFIG" "$node"
      return 0
    fi
    printf 'WARNING: Could not update %s.\n' "$PROXMOX_CLUSTER_CONFIG" >&2
  fi
  printf 'ACTION REQUIRED: set PROXMOX_CONTROL_NODE=%s in %s before running setup, remove, or purge again.\n' \
    "$node" "$PROXMOX_CLUSTER_CONFIG"
}
