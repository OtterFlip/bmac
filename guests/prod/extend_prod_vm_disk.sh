#!/usr/bin/env bash

# Copyright (c) 2026 BEENTHERE VENTURES, INC.
# SPDX-License-Identifier: GPL-3.0-only

# Grow one running production VM's root zvol, replicate the new size to every
# HA placement node, then grow the guest's final ext4 partition and filesystem
# online. Growth is capped so every placement pool keeps 10% of its total size
# available afterward.

set -Eeuo pipefail
set +x
umask 077

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
REPO_ROOT="$(cd -- "${SCRIPT_DIR}/../.." && pwd -P)"
# shellcheck source=../../lib/ui_protocol.sh
source "${REPO_ROOT}/lib/ui_protocol.sh"
CONFIG_LIB="${REPO_ROOT}/lib/config.sh"
REMOTE_ROOT="/usr/local/lib/app-ha-proxmox"
REMOTE_REGISTRY="${REMOTE_ROOT}/lib/cluster_registry.py"

MIB=1048576
POOL_RESERVE_PERCENT=10
SPARSE_OVERHEAD_PERMILLE=15
# Unallocated space (or ext4 slack) at or below this is normal partition
# alignment and GPT backup-header room, not an unfinished earlier growth.
GUEST_SLACK_TOLERANCE_BYTES=$((2 * MIB))

DRY_RUN=false
REPLICATION_TIMEOUT_SECONDS=3600
RESCAN_TIMEOUT_SECONDS=60

CURRENT_PHASE="startup"
COORDINATOR=""
RUN_DIR=""
LEASE_NONCE=""
LEASE_ACQUIRED=false
ZVOL_RESIZED=false
REGISTRY_RECORDED=false

RESOURCE_NAME=""
RESOURCE_REVISION=""
RESOURCE_IP=""
VMID=""
REGISTRY_OWNER=""
OWNER_NODE=""
ROOT_VOLUME=""
DISK_ALLOCATION=""
CURRENT_DISK_BYTES=""
NEW_DISK_BYTES=""
GUEST_ALIAS=""
ALLOWED_BYTES=""
MIN_HEADROOM_BYTES=""
INCREASE_BYTES=""

GUEST_SOURCE=""
GUEST_DISK=""
GUEST_PARTITION_NUMBER=""
GUEST_DISK_BYTES=""
GUEST_PARTITION_BYTES=""
GUEST_FS_BYTES=""
GUEST_FREE_BYTES=""
GUEST_TAIL_BYTES=""

declare -a CLUSTER_NODES=()
declare -a ONLINE_NODES=()
declare -a PLACEMENT_NODES=()
declare -a REPLICATION_JOBS=()
declare -A REPLICATION_TARGET=()
declare -A NODE_POOL_SIZE=()
declare -A NODE_POOL_AVAILABLE=()
declare -A NODE_VOLSIZE=()
declare -A NODE_REFRESERVATION=()

log() {
  printf '\n==> %s\n' "$*"
  bmac_ui_step "$*"
}

info() {
  printf '    %s\n' "$*"
}

warn() {
  printf 'WARNING: %s\n' "$*" >&2
}

die() {
  printf '\nERROR: %s\n' "$*" >&2
  bmac_ui_error failed "$*"
  exit 1
}

usage() {
  cat <<'EOF'
Usage: extend_prod_vm_disk.sh [options]

Run from an administrator workstation. Lists registered production VMs,
prompts for one (default: the lowest-numbered active prodN), verifies
workstation SSH to the guest, and calculates how much its root zvol may grow
while every HA placement pool keeps 10% of its total size available. After
confirmation it grows the zvol on the live HA owner, replicates the new size
to every placement node, records the exact size in the registry, and grows the
guest's final ext4 partition and filesystem online.

Options:
  --dry-run                       Validate and show the growth limit without
                                  changing the registry, Proxmox, or guest.
  --replication-timeout-seconds N Default: 3600
  -h, --help
EOF
}

validate_positive_integer() {
  [[ "$2" =~ ^[1-9][0-9]*$ ]] ||
    die "$1 must be a positive integer"
}

parse_args() {
  while (($#)); do
    case "$1" in
      --dry-run)
        DRY_RUN=true
        shift
        ;;
      --replication-timeout-seconds)
        (($# >= 2)) || die "$1 requires a value"
        validate_positive_integer "$1" "$2"
        REPLICATION_TIMEOUT_SECONDS="$2"
        shift 2
        ;;
      -h | --help)
        usage
        exit 0
        ;;
      *)
        die "Unknown option: $1"
        ;;
    esac
  done
}

prompt_with_default() {
  local destination="$1" prompt="$2" default="$3" entered
  if bmac_ui_is_json; then
    bmac_ui_text entered "$prompt" "$default"
    printf -v "$destination" '%s' "${entered:-$default}"
    return 0
  fi
  IFS= read -r -p "${prompt} [${default}]: " entered ||
    die "Input ended before a value was entered"
  printf -v "$destination" '%s' "${entered:-$default}"
}

prompt_yes() {
  local answer
  if bmac_ui_is_json; then
    bmac_ui_ask "$1"
    return
  fi
  IFS= read -r -p "$1 [y/N] " answer || return 1
  [[ "${answer,,}" == y || "${answer,,}" == yes ]]
}

confirm_go() {
  local entered
  if bmac_ui_is_json; then
    bmac_ui_confirm_go "$1" || die "Confirmation did not match GO; no change was made"
    return 0
  fi
  printf '%s\nType GO to continue.\n> ' "$1"
  IFS= read -r entered || entered=""
  [[ "$entered" == GO ]] ||
    die "Confirmation did not match GO; no change was made"
}

load_and_validate_config() {
  CURRENT_PHASE="loading cluster configuration"
  [[ -f "$CONFIG_LIB" && ! -L "$CONFIG_LIB" ]] ||
    die "Configuration library is unavailable"
  # shellcheck source=../../lib/config.sh
  source "$CONFIG_LIB"
  load_proxmox_config --no-secrets ||
    die "Could not load cluster configuration"
  local name
  for name in PROXMOX_CLUSTER_NAME MAX_MOX_HOSTS CLUSTER_STATE_DIR \
    PROD_VM_STORAGE; do
    require_var "$name" || die "Configuration is incomplete"
  done
  [[ "$PROD_VM_STORAGE" == local-zfs ]] ||
    die "Production root disks must use local-zfs"
  [[ "$CLUSTER_STATE_DIR" =~ ^/[A-Za-z0-9._/-]+$ ]] ||
    die "CLUSTER_STATE_DIR must be a safe absolute path"
}

preflight_workstation() {
  CURRENT_PHASE="validating workstation"
  local command_name
  for command_name in awk bash date mktemp python3 ssh; do
    command -v "$command_name" >/dev/null 2>&1 ||
      die "Required workstation command is unavailable: $command_name"
  done
  RUN_DIR="$(mktemp -d "${TMPDIR:-/tmp}/extend-prod-vm-disk.XXXXXX")"
  chmod 0700 "$RUN_DIR"
}

cleanup() {
  local code=$?
  trap - EXIT
  set +e
  if ((code != 0)); then
    printf '\nFailed while %s.\n' "$CURRENT_PHASE" >&2
    if [[ "$ZVOL_RESIZED" == true && "$REGISTRY_RECORDED" != true ]]; then
      printf 'The zvol of %s was resized to %s bytes but the registry was not updated.\n' \
        "$RESOURCE_NAME" "$NEW_DISK_BYTES" >&2
      printf 'After confirming the live size, record it on a mox host with:\n' >&2
      printf '  %s --state-dir %s update %s --disk-bytes %s\n' \
        "$REMOTE_REGISTRY" "$CLUSTER_STATE_DIR" "$RESOURCE_NAME" \
        "$NEW_DISK_BYTES" >&2
    elif [[ "$REGISTRY_RECORDED" == true ]]; then
      printf 'The zvol of %s is %s bytes and is recorded in the registry.\n' \
        "$RESOURCE_NAME" "$NEW_DISK_BYTES" >&2
      printf 'Correct the fault and rerun this script; it finishes any unfinished\n' >&2
      printf 'replica verification or guest partition/filesystem growth first.\n' >&2
    fi
  fi
  if [[ "$LEASE_ACQUIRED" == true && -n "$COORDINATOR" ]]; then
    registry_cmd orchestration-release "$RESOURCE_NAME" \
      --nonce "$LEASE_NONCE" >/dev/null 2>&1 ||
      warn "Could not release the orchestration lease; it expires on its own"
  fi
  [[ -z "$RUN_DIR" ]] || rm -rf -- "$RUN_DIR"
  exit "$code"
}

# Execute one argv array on a mox node. Non-coordinator nodes are reached
# through Proxmox root cluster SSH trust without constructing an eval string.
node_exec() {
  local node="$1"
  shift
  [[ "$node" =~ ^mox([1-9]|10)$ ]] || return 2
  (($# > 0)) || return 2
  local payload="set -Eeuo pipefail"$'\n'"exec" argument quoted
  for argument in "$@"; do
    printf -v quoted '%q' "$argument"
    payload+=" ${quoted}"
  done
  payload+=$'\n'
  if [[ "$node" == "$COORDINATOR" ]]; then
    printf '%s' "$payload" | mox_ssh "$COORDINATOR" bash -s
  else
    printf '%s' "$payload" |
      mox_ssh "$COORDINATOR" ssh \
        -o BatchMode=yes \
        -o ClearAllForwardings=yes \
        -o "ConnectTimeout=${MOX_SSH_CONNECT_TIMEOUT:-8}" \
        -o StrictHostKeyChecking=yes \
        -o CheckHostIP=no \
        -o "HostKeyAlias=${node}" \
        -o "UserKnownHostsFile=/etc/pve/nodes/${node}/ssh_known_hosts" \
        -o GlobalKnownHostsFile=none \
        "root@${node}.${PROXMOX_INTERNAL_DOMAIN}" bash -s
  fi
}

registry_cmd() {
  mox_ssh "$COORDINATOR" "$REMOTE_REGISTRY" \
    --state-dir "$CLUSTER_STATE_DIR" "$@" </dev/null
}

pvesh_get() {
  mox_ssh "$COORDINATOR" pvesh get "$@" --output-format json </dev/null
}

write_json() {
  local path="$1" value="$2"
  printf '%s\n' "$value" >"$path"
  chmod 0600 "$path"
  python3 - "$path" <<'PY'
import json
import sys
json.load(open(sys.argv[1], encoding="utf-8"))
PY
}

# Print "N bytes (M MiB, G GiB)". MiB and GiB are truncated, never rounded up,
# so a displayed limit never overstates the exact byte value.
format_size() {
  python3 - "$1" <<'PY'
import sys
from decimal import Decimal, ROUND_DOWN

value = int(sys.argv[1])
mib = Decimal(value) / Decimal(1048576)
gib = Decimal(value) / Decimal(1073741824)
mib_text = (
    str(value // 1048576)
    if value % 1048576 == 0
    else str(mib.quantize(Decimal("0.01"), rounding=ROUND_DOWN))
)
gib_text = str(gib.quantize(Decimal("0.001"), rounding=ROUND_DOWN))
print(f"{value} bytes ({mib_text} MiB, {gib_text} GiB)")
PY
}

# Integer-only byte arithmetic for the growth limit.
#   calculate_allowance ALLOCATION VOLSIZE REFRESERVATION GUEST_FREE \
#     RESERVE_PERCENT OVERHEAD_PERMILLE NODE=POOL_SIZE=AVAILABLE...
# Prints one "node" row per placement node, then "headroom" and "allowed".
calculate_allowance() {
  python3 - "$@" <<'PY'
import re
import sys

MIB = 1024 * 1024
allocation = sys.argv[1]
volsize, refreservation, guest_free, reserve_percent, overhead_permille = (
    int(value) for value in sys.argv[2:7]
)
if allocation not in {"sparse", "reserved"}:
    raise SystemExit("allocation must be sparse or reserved")
if volsize <= 0 or guest_free < 0:
    raise SystemExit("volume size and guest free space must be non-negative")
if not 0 <= reserve_percent < 100 or not 0 <= overhead_permille < 1000:
    raise SystemExit("reserve percent or overhead permille is out of range")
rows = []
for spec in sys.argv[7:]:
    match = re.fullmatch(r"(mox(?:[1-9]|10))=([0-9]+)=([0-9]+)", spec)
    if not match:
        raise SystemExit(f"invalid node capacity: {spec!r}")
    node, size, available = match.group(1), int(match.group(2)), int(match.group(3))
    if size <= 0:
        raise SystemExit(f"{node} reported an empty pool")
    # Ceiling, so the reserve is never smaller than the exact percentage.
    reserve = -(-size * reserve_percent // 100)
    rows.append((node, size, available, reserve, available - reserve))
if not rows:
    raise SystemExit("at least one placement node is required")

headroom = min(row[4] for row in rows)
headroom = max(headroom, 0) // MIB * MIB
if allocation == "reserved":
    # refreservation=auto reserves more than volsize (zvol metadata), so a
    # volsize increase of X consumes X * refreservation / volsize of pool.
    if refreservation < volsize:
        raise SystemExit("reserved zvol refreservation is smaller than volsize")
    allowed = headroom * volsize // refreservation
elif guest_free >= headroom:
    allowed = 0
else:
    # Free guest blocks can already be written into the sparse zvol, and
    # sparse zvol metadata adds roughly overhead_permille on top.
    allowed = (headroom - guest_free) * (1000 - overhead_permille) // 1000
allowed = allowed // MIB * MIB

for row in rows:
    print("node", *row, sep="\t")
print("headroom", headroom, sep="\t")
print("allowed", allowed, sep="\t")
PY
}

# parse_increase UNIT AMOUNT ALLOWED_BYTES prints "REQUESTED ROUNDED" bytes,
# where ROUNDED is REQUESTED rounded up to a whole MiB.
parse_increase() {
  python3 - "$@" <<'PY'
import re
import sys
from decimal import Decimal, ROUND_CEILING, getcontext

getcontext().prec = 80
MIB = 1024 * 1024
unit, raw, allowed = sys.argv[1], sys.argv[2].strip(), int(sys.argv[3])
factors = {"bytes": 1, "MiB": MIB, "GiB": 1024 * MIB}
if unit not in factors:
    raise SystemExit("unit must be bytes, MiB, or GiB")
pattern = r"[0-9]{1,24}" if unit == "bytes" else r"[0-9]{1,18}(?:\.[0-9]{1,9})?"
if not re.fullmatch(pattern, raw):
    raise SystemExit(
        "enter a whole number of bytes"
        if unit == "bytes"
        else f"enter a number of {unit}, such as 10 or 2.5"
    )
requested = int(
    (Decimal(raw) * factors[unit]).to_integral_value(rounding=ROUND_CEILING)
)
rounded = -(-requested // MIB) * MIB
if rounded <= 0:
    raise SystemExit("the increase must be greater than zero")
if rounded > allowed:
    raise SystemExit(
        f"{rounded} bytes (after rounding up to a whole MiB) exceeds the "
        f"maximum allowed increase of {allowed} bytes"
    )
print(requested, rounded)
PY
}

parse_cluster_nodes() {
  local path="$1" maximum="$2" mode="$3"
  python3 - "$path" "$maximum" "$mode" <<'PY'
import json
import re
import sys

rows = json.load(open(sys.argv[1], encoding="utf-8"))
if not isinstance(rows, list):
    raise SystemExit("pvesh /nodes did not return an array")
maximum = int(sys.argv[2])
mode = sys.argv[3]
seen = set()
selected = []
for row in rows:
    node = row.get("node", row.get("name"))
    match = re.fullmatch(r"mox([1-9]|10)", str(node))
    if not match or int(match.group(1)) > maximum:
        raise SystemExit(f"unexpected cluster node: {node!r}")
    if node in seen:
        raise SystemExit(f"duplicate cluster node: {node}")
    seen.add(node)
    online = (
        str(row.get("status", "")).lower() == "online"
        or row.get("online") in (1, True)
    )
    if mode == "all" or online:
        selected.append(node)
selected.sort(key=lambda value: int(value[3:]))
if not selected:
    raise SystemExit("no matching mox nodes")
print(*selected, sep="\n")
PY
}

display_production_resources() {
  python3 - "$1" <<'PY'
import json
import sys

rows = json.load(open(sys.argv[1], encoding="utf-8"))
production = [row for row in rows if row.get("kind") == "production"]
production.sort(key=lambda row: row.get("index", 0))
if not production:
    raise SystemExit("registry contains no production resources")
for row in production:
    spec = row.get("spec", {})
    disk_bytes = spec.get("disk_bytes", spec.get("disk_gib", 0) * 2**30)
    print(
        row.get("name"),
        row.get("purpose", {}).get("slug", "?"),
        row.get("domains", {}).get("primary", "?"),
        ",".join(row.get("placement", [])),
        row.get("state"),
        row.get("owner_node") or "unknown",
        f"{disk_bytes / 2**30:.3f}",
        spec.get("disk_allocation", "?"),
        sep="\t",
    )
PY
}

discover_cluster() {
  CURRENT_PHASE="discovering the cluster"
  local configured_nodes online_nodes node help_text
  COORDINATOR="$(first_reachable_mox)" ||
    die "No reachable mox coordinator was found"
  info "Coordinator: $COORDINATOR"
  # shellcheck disable=SC2016 # This loop is intentionally evaluated remotely.
  node_exec "$COORDINATOR" bash -c \
    'for command in ha-manager pvesh pvesm pvesr qm zfs zpool; do command -v "$command" >/dev/null; done' ||
    die "Coordinator lacks required Proxmox/ZFS commands"

  write_json "${RUN_DIR}/cluster-status.json" "$(pvesh_get /cluster/status)"
  python3 - "${RUN_DIR}/cluster-status.json" "$PROXMOX_CLUSTER_NAME" <<'PY' ||
import json
import sys
rows = json.load(open(sys.argv[1], encoding="utf-8"))
clusters = [row for row in rows if row.get("type") == "cluster"]
if (
    len(clusters) != 1
    or clusters[0].get("name") != sys.argv[2]
    or clusters[0].get("quorate") not in (1, True, "1")
):
    raise SystemExit("cluster identity or quorum differs from configuration")
PY
    die "Cluster identity or quorum validation failed"

  write_json "${RUN_DIR}/nodes.json" "$(pvesh_get /nodes)"
  configured_nodes="$(
    parse_cluster_nodes "${RUN_DIR}/nodes.json" "$MAX_MOX_HOSTS" all
  )" || die "Could not validate configured cluster nodes"
  online_nodes="$(
    parse_cluster_nodes "${RUN_DIR}/nodes.json" "$MAX_MOX_HOSTS" online
  )" || die "Could not validate online cluster nodes"
  mapfile -t CLUSTER_NODES <<<"$configured_nodes"
  mapfile -t ONLINE_NODES <<<"$online_nodes"

  # A registry record carrying spec.disk_bytes is rejected by an older
  # cluster_registry.py, including the one the guest-role hook runs on every
  # node, so every node must already run the version that understands it.
  ((${#ONLINE_NODES[@]} == ${#CLUSTER_NODES[@]})) ||
    die "Every cluster node must be online (configured: ${CLUSTER_NODES[*]}; online: ${ONLINE_NODES[*]})"
  for node in "${CLUSTER_NODES[@]}"; do
    help_text="$(
      node_exec "$node" "$REMOTE_REGISTRY" --state-dir "$CLUSTER_STATE_DIR" \
        update --help
    )" || die "Could not run the installed cluster registry on $node"
    grep -q -- '--disk-bytes' <<<"$help_text" ||
      die "The installed cluster registry on $node lacks disk-growth support; run hosts/update_cluster_runtime.sh first"
  done
  info "Every cluster node runs a registry with exact disk-size support"

  write_json "${RUN_DIR}/resources.json" "$(registry_cmd list)"
  printf '\nRegistered production resources:\n'
  printf '  %-8s %-16s %-28s %-16s %-9s %-8s %-11s %s\n' \
    NAME PURPOSE DOMAIN PLACEMENT STATE OWNER "DISK(GiB)" ALLOCATION
  local name purpose domain placement state owner disk allocation
  while IFS=$'\t' read -r name purpose domain placement state owner disk allocation; do
    printf '  %-8s %-16s %-28s %-16s %-9s %-8s %-11s %s\n' \
      "$name" "$purpose" "$domain" "$placement" "$state" "$owner" "$disk" \
      "$allocation"
  done < <(display_production_resources "${RUN_DIR}/resources.json") ||
    die "Could not list registered production resources"
}

parse_selected_production() {
  local fields_path="${RUN_DIR}/selected.fields"
  python3 - "${RUN_DIR}/resources.json" "$1" "$PROD_VM_STORAGE" \
    >"$fields_path" <<'PY' || return 1
import json
import re
import sys

rows = json.load(open(sys.argv[1], encoding="utf-8"))
name, storage = sys.argv[2], sys.argv[3]
if not re.fullmatch(r"prod[1-9][0-9]*", name):
    raise SystemExit("selection must use the prodN formula")
matches = [row for row in rows if row.get("name") == name]
if len(matches) != 1 or matches[0].get("kind") != "production":
    raise SystemExit(f"{name} is not one registered production resource")
row = matches[0]
if row.get("state") != "active":
    raise SystemExit(f"{name} must be active (registry state: {row.get('state')})")
placement = row.get("placement")
if not isinstance(placement, list) or len(placement) < 2:
    raise SystemExit("production needs at least two HA placement nodes")
if set(row["proxmox"].get("ha_nodes", [])) != set(placement):
    raise SystemExit("registered HA nodes differ from production placement")
volume = row["proxmox"].get("volume_id")
if not isinstance(volume, str) or not volume.startswith(storage + ":"):
    raise SystemExit("production has no registered root volume on production storage")
spec = row["spec"]
disk_bytes = spec.get("disk_bytes", spec["disk_gib"] * 2**30)
values = (
    row["name"],
    row["vmid"],
    row["ip"],
    row.get("owner_node") or "",
    row["revision"],
    volume,
    spec["disk_allocation"],
    disk_bytes,
    ",".join(placement),
)
for value in values:
    sys.stdout.buffer.write(str(value).encode() + b"\0")
PY
  local -a fields=()
  mapfile -d '' -t fields <"$fields_path"
  ((${#fields[@]} == 9)) || return 1
  RESOURCE_NAME="${fields[0]}"
  VMID="${fields[1]}"
  RESOURCE_IP="${fields[2]}"
  REGISTRY_OWNER="${fields[3]}"
  RESOURCE_REVISION="${fields[4]}"
  ROOT_VOLUME="${fields[5]}"
  DISK_ALLOCATION="${fields[6]}"
  CURRENT_DISK_BYTES="${fields[7]}"
  IFS=',' read -r -a PLACEMENT_NODES <<<"${fields[8]}"
}

select_production() {
  CURRENT_PHASE="selecting the production VM"
  local default selection
  default="$(
    python3 - "${RUN_DIR}/resources.json" <<'PY'
import json
import sys
rows = [
    row
    for row in json.load(open(sys.argv[1], encoding="utf-8"))
    if row.get("kind") == "production" and row.get("state") == "active"
]
rows.sort(key=lambda row: row.get("index", 0))
if rows:
    print(rows[0]["name"])
PY
  )"
  [[ -n "$default" ]] || die "No active production VM is registered"
  if bmac_ui_is_json; then
    local -a options=()
    mapfile -t options < <(python3 - "${RUN_DIR}/resources.json" <<'PY'
import json
import sys
rows = [
    row
    for row in json.load(open(sys.argv[1], encoding="utf-8"))
    if row.get("kind") == "production" and row.get("state") == "active"
]
rows.sort(key=lambda row: row.get("index", 0))
for row in rows:
    print(row["name"])
    print(f"{row['name']} · {row.get('domains', {}).get('primary', '-')} · owner {row.get('owner_node') or '-'}")
PY
    )
    bmac_ui_choose selection "Production VM to grow" "$default" "${options[@]}"
  else
    prompt_with_default selection "Production VM to grow" "$default"
  fi
  parse_selected_production "$selection" ||
    die "Invalid production VM selection"
}

# Determine the real HA owner from live cluster state; the registry owner can
# be stale after a manual or automatic HA failover.
validate_live_production() {
  CURRENT_PHASE="validating live production, HA, and replication state"
  write_json "${RUN_DIR}/cluster-vms.json" "$(
    pvesh_get /cluster/resources --type vm
  )"
  write_json "${RUN_DIR}/ha-resources.json" "$(
    pvesh_get /cluster/ha/resources
  )"
  write_json "${RUN_DIR}/ha-rules.json" "$(
    node_exec "$COORDINATOR" ha-manager rules config --output-format json
  )"
  write_json "${RUN_DIR}/replication.json" "$(
    pvesh_get /cluster/replication
  )"
  local parsed
  parsed="$(
    python3 - \
      "${RUN_DIR}/cluster-vms.json" "${RUN_DIR}/ha-resources.json" \
      "${RUN_DIR}/ha-rules.json" "${RUN_DIR}/nodes.json" \
      "${RUN_DIR}/replication.json" \
      "$RESOURCE_NAME" "$VMID" \
      "$(IFS=,; printf '%s' "${PLACEMENT_NODES[*]}")" <<'PY'
import json
import re
import sys

vm_path, ha_path, rules_path, nodes_path, replication_path = sys.argv[1:6]
name, vmid_text, placement_csv = sys.argv[6:]
vmid = int(vmid_text)
placement = placement_csv.split(",")

def load(path):
    return json.load(open(path, encoding="utf-8"))

def values(raw):
    return raw if isinstance(raw, list) else [
        part for part in str(raw or "").split(",") if part
    ]

matches = [row for row in load(vm_path) if row.get("vmid") in (vmid, str(vmid))]
if len(matches) != 1:
    raise SystemExit("production VMID is not unique in live cluster resources")
vm = matches[0]
if (
    vm.get("name") != name
    or vm.get("type") != "qemu"
    or vm.get("status") != "running"
    or vm.get("node") not in placement
):
    raise SystemExit("production must be one running QEMU VM on its registered placement")
owner = vm["node"]

online = {
    row.get("node", row.get("name"))
    for row in load(nodes_path)
    if str(row.get("status", "")).lower() == "online" or row.get("online") in (1, True)
}
missing = sorted(set(placement) - online)
if missing:
    raise SystemExit("every placement node must be online: " + ", ".join(missing))

ha = [row for row in load(ha_path) if row.get("sid") == f"vm:{vmid}"]
if len(ha) != 1:
    raise SystemExit("production is not exactly one Proxmox HA resource")
if str(ha[0].get("state", "started")).lower() not in {"started", "enabled"}:
    raise SystemExit("production HA requested state is not started")
if int(ha[0].get("failback", -1)) != 0 or int(ha[0].get("auto-rebalance", -1)) != 0:
    raise SystemExit("production HA failback/auto-rebalance policy differs from contract")

matching_rules = 0
for rule in load(rules_path):
    if f"vm:{vmid}" not in values(rule.get("resources")):
        continue
    priorities = {}
    for part in values(rule.get("nodes")):
        node, separator, priority = str(part).partition(":")
        priorities[node] = int(priority) if separator else 0
    if (
        rule.get("type") in (None, "node-affinity")
        and rule.get("strict", 0) in (1, True, "1")
        and rule.get("disable", 0) not in (1, True, "1")
        and rule.get("affinity", "positive") == "positive"
        and priorities == {node: 1 for node in placement}
    ):
        matching_rules += 1
if matching_rules != 1:
    raise SystemExit("production needs exactly one strict HA node-affinity rule matching placement")

jobs = []
for row in load(replication_path):
    if row.get("guest") not in (vmid, str(vmid)):
        continue
    if row.get("disable") in (1, True, "1"):
        raise SystemExit(f"replication job {row.get('id')} is disabled")
    job = str(row.get("id", ""))
    target = str(row.get("target", ""))
    if not re.fullmatch(r"[1-9][0-9]{2,8}-[0-9]+", job) or not re.fullmatch(r"mox([1-9]|10)", target):
        raise SystemExit("replication job id or target is invalid")
    jobs.append((job, target))
targets = [target for _, target in jobs]
if sorted(targets) != sorted(set(placement) - {owner}) or len(set(targets)) != len(targets):
    raise SystemExit("replication targets differ from placement minus the live owner")
print(owner)
for job, target in sorted(jobs):
    print(f"{job}\t{target}")
PY
  )" || die "Live production/HA/replication validation failed"
  local -a lines=()
  mapfile -t lines <<<"$parsed"
  OWNER_NODE="${lines[0]}"
  [[ "$OWNER_NODE" =~ ^mox([1-9]|10)$ ]] || die "Live owner is invalid"
  REPLICATION_JOBS=()
  REPLICATION_TARGET=()
  local line job target
  for line in "${lines[@]:1}"; do
    IFS=$'\t' read -r job target <<<"$line"
    REPLICATION_JOBS+=("$job")
    REPLICATION_TARGET["$job"]="$target"
  done
  info "Live HA owner: $OWNER_NODE; placement: ${PLACEMENT_NODES[*]}"
  info "Replication jobs: ${REPLICATION_JOBS[*]}"
  if [[ "$REGISTRY_OWNER" != "$OWNER_NODE" ]]; then
    info "Registry owner ${REGISTRY_OWNER:-unknown} is stale; the live owner is $OWNER_NODE"
  fi

  local status
  for job in "${REPLICATION_JOBS[@]}"; do
    status="$(pvesh_get "/nodes/${OWNER_NODE}/replication/${job}/status")" ||
      die "Could not read replication status for $job"
    python3 - "$status" "$job" <<'PY' ||
import json
import sys
row = json.loads(sys.argv[1])
if (
    row.get("id") != sys.argv[2]
    or not isinstance(row.get("last_sync"), int)
    or row["last_sync"] <= 0
    or int(row.get("fail_count", 0)) != 0
    or row.get("error")
):
    raise SystemExit(1)
PY
      die "Replication job $job is not currently healthy"
  done
  validate_vm_disk_config "$CURRENT_DISK_BYTES"
}

# Print the scsi0 size in bytes after checking volume identity and that no
# Proxmox lock (backup, migration, snapshot) is held.
validate_vm_disk_config() {
  local expected="$1" config
  config="$(node_exec "$OWNER_NODE" qm config "$VMID")" ||
    die "Could not read the VM configuration from $OWNER_NODE"
  python3 - "$config" "$ROOT_VOLUME" "$expected" "$RESOURCE_NAME" <<'PY' ||
import re
import sys

config = {}
for line in sys.argv[1].splitlines():
    if ": " in line:
        key, value = line.split(": ", 1)
        config[key] = value
volume, expected, name = sys.argv[2], int(sys.argv[3]), sys.argv[4]
if config.get("name") != name:
    raise SystemExit("VM name differs from registry")
if "lock" in config:
    raise SystemExit(f"VM is locked by Proxmox ({config['lock']})")
parts = config.get("scsi0", "").split(",")
if parts[0] != volume:
    raise SystemExit("scsi0 volume differs from the registered root volume")
options = dict(part.split("=", 1) for part in parts[1:] if "=" in part)
match = re.fullmatch(r"([0-9]+)([KMGTP]?)", options.get("size", ""), re.IGNORECASE)
units = {"": 1, "K": 2**10, "M": 2**20, "G": 2**30, "T": 2**40, "P": 2**50}
if not match:
    raise SystemExit("scsi0 has no parseable size")
size = int(match.group(1)) * units[match.group(2).upper()]
if size != expected:
    raise SystemExit(
        f"scsi0 size {size} bytes differs from the registered {expected} bytes"
    )
PY
    die "VM disk configuration differs from the registry (see message above)"
}

reconcile_registry_owner() {
  [[ "$REGISTRY_OWNER" != "$OWNER_NODE" ]] || return 0
  if [[ "$DRY_RUN" == true ]]; then
    info "Dry run: the stale registry owner was not updated"
    return 0
  fi
  CURRENT_PHASE="updating the stale production registry owner"
  local updated
  updated="$(registry_cmd update "$RESOURCE_NAME" \
    --expected-revision "$RESOURCE_REVISION" \
    --owner-node "$OWNER_NODE")" ||
    die "Could not update the registry owner of $RESOURCE_NAME to $OWNER_NODE"
  RESOURCE_REVISION="$(
    python3 -c 'import json,sys; print(json.loads(sys.argv[1])["revision"])' \
      "$updated"
  )"
  info "Registry owner of $RESOURCE_NAME updated from ${REGISTRY_OWNER:-unknown} to $OWNER_NODE"
  REGISTRY_OWNER="$OWNER_NODE"
}

# Runs on the guest as root through the workstation SSH alias. Every mode
# prints one JSON line describing the root filesystem, its partition, and disk.
GUEST_HELPER="$(
  cat <<'PY'
import json
import os
import shutil
import socket
import subprocess
import sys
import time


def fail(message):
    print(message, file=sys.stderr)
    raise SystemExit(1)


def run(*argv):
    return subprocess.run(argv, check=True, capture_output=True, text=True).stdout


def sysfs_int(path):
    with open(path, encoding="ascii") as stream:
        return int(stream.read().strip())


def inspect():
    mounts = json.loads(
        run("findmnt", "-J", "--mountpoint", "/", "-o", "SOURCE,FSTYPE,OPTIONS")
    )["filesystems"]
    if len(mounts) != 1:
        fail("root mount is ambiguous")
    source = os.path.realpath(mounts[0]["source"])
    partition = os.path.basename(source)
    sys_partition = f"/sys/class/block/{partition}"
    if not os.path.exists(f"{sys_partition}/partition"):
        fail(f"root filesystem source {source} is not a disk partition")
    disk = os.path.basename(os.path.dirname(os.path.realpath(sys_partition)))
    sys_disk = f"/sys/class/block/{disk}"
    sector = sysfs_int(f"{sys_disk}/queue/logical_block_size")
    disk_bytes = sysfs_int(f"{sys_disk}/size") * 512
    part_start = sysfs_int(f"{sys_partition}/start") * 512
    part_bytes = sysfs_int(f"{sys_partition}/size") * 512
    table = json.loads(run("sfdisk", "--json", f"/dev/{disk}"))["partitiontable"]
    entries = [
        (entry["node"], entry["start"] * sector, (entry["start"] + entry["size"]) * sector)
        for entry in table.get("partitions", [])
    ]
    root_entries = [entry for entry in entries if os.path.realpath(entry[0]) == source]
    root_is_last = (
        len(root_entries) == 1
        and all(end <= root_entries[0][2] for _, _, end in entries)
        and all(start <= root_entries[0][1] for _, start, _ in entries)
    )
    # After a disk grows, the GPT header still carries the old lastlba until
    # growpart relocates the backup header, so derive the usable end from the
    # current disk size: the backup GPT occupies the final 33 sectors.
    if table.get("label") == "gpt":
        usable_end = disk_bytes - 33 * sector
    else:
        usable_end = disk_bytes
    block_count = block_size = None
    for line in run("tune2fs", "-l", source).splitlines():
        key, _, value = line.partition(":")
        if key.strip() == "Block count":
            block_count = int(value)
        elif key.strip() == "Block size":
            block_size = int(value)
    if block_count is None or block_size is None:
        fail("tune2fs did not report the ext4 block count and size")
    vfs = os.statvfs("/")
    return {
        "hostname": socket.gethostname(),
        "uid": os.geteuid(),
        "fstype": mounts[0]["fstype"],
        "options": mounts[0]["options"].split(","),
        "source": source,
        "partition": partition,
        "partition_number": sysfs_int(f"{sys_partition}/partition"),
        "disk": disk,
        "disk_bytes": disk_bytes,
        "table_label": table.get("label", ""),
        "partition_bytes": part_bytes,
        "partition_end_bytes": part_start + part_bytes,
        "root_is_last": root_is_last,
        "unallocated_tail_bytes": max(usable_end - (part_start + part_bytes), 0),
        "fs_bytes": block_count * block_size,
        "free_bytes": vfs.f_bfree * vfs.f_frsize,
        "rescan_available": os.path.exists(f"{sys_disk}/device/rescan"),
        "tools": {
            name: shutil.which(name) is not None
            for name in ("growpart", "resize2fs", "sfdisk", "tune2fs", "findmnt")
        },
    }


mode = sys.argv[1]
if mode == "inspect":
    pass
elif mode == "rescan":
    disk, expected = sys.argv[2], int(sys.argv[3])
    with open(f"/sys/class/block/{disk}/device/rescan", "w", encoding="ascii") as stream:
        stream.write("1\n")
    deadline = time.monotonic() + int(sys.argv[4])
    while sysfs_int(f"/sys/class/block/{disk}/size") * 512 != expected:
        if time.monotonic() > deadline:
            fail(f"guest disk {disk} did not report {expected} bytes after rescan")
        time.sleep(1)
elif mode == "grow-partition":
    disk, number = sys.argv[2], sys.argv[3]
    completed = subprocess.run(
        ["growpart", f"/dev/{disk}", number], capture_output=True, text=True
    )
    output = (completed.stdout + completed.stderr).strip()
    if output:
        print(output, file=sys.stderr)
    if completed.returncode not in (0, 1) or (
        completed.returncode == 1 and "NOCHANGE" not in output
    ):
        fail(f"growpart failed with exit code {completed.returncode}")
elif mode == "grow-filesystem":
    completed = subprocess.run(
        ["resize2fs", f"/dev/{sys.argv[2]}"], capture_output=True, text=True
    )
    output = (completed.stdout + completed.stderr).strip()
    if output:
        print(output, file=sys.stderr)
    if completed.returncode != 0:
        fail(f"resize2fs failed with exit code {completed.returncode}")
else:
    fail(f"unknown guest helper mode: {mode}")
print(json.dumps(inspect(), sort_keys=True))
PY
)"

guest_helper() {
  local argument
  for argument in "$@"; do
    [[ "$argument" =~ ^[A-Za-z0-9_-]+$ ]] ||
      die "Unsafe guest helper argument: $argument"
  done
  ssh -o BatchMode=yes -o ConnectTimeout=15 \
    -o ControlMaster=no -o ControlPath=none \
    "$GUEST_ALIAS" python3 - "$@" <<<"$GUEST_HELPER"
}

# Validate one guest JSON report and load it into GUEST_* variables.
load_guest_report() {
  local report="$1" expected_disk_bytes="$2" fields_path="${RUN_DIR}/guest.fields"
  python3 - "$report" "$RESOURCE_NAME" "$expected_disk_bytes" \
    >"$fields_path" <<'PY' || return 1
import json
import re
import sys

value = json.loads(sys.argv[1])
name, expected = sys.argv[2], int(sys.argv[3])
problems = []
if value["hostname"].split(".", 1)[0] != name:
    problems.append(f"guest hostname is {value['hostname']!r}, not {name}")
if value["uid"] != 0:
    problems.append("guest SSH session is not root")
if value["fstype"] != "ext4":
    problems.append(f"root filesystem is {value['fstype']}, not ext4")
if "rw" not in value["options"]:
    problems.append("root filesystem is not mounted read-write")
if not re.fullmatch(r"sd[a-z]+", value["disk"]) or not re.fullmatch(r"sd[a-z]+[0-9]+", value["partition"]):
    problems.append("root filesystem is not on a SCSI disk partition")
if value["table_label"] not in {"gpt", "dos"}:
    problems.append(f"unsupported partition table: {value['table_label']!r}")
if not value["root_is_last"]:
    problems.append("the root ext4 partition is not the last partition on its disk")
if not value["rescan_available"]:
    problems.append("guest disk has no SCSI rescan interface")
missing = sorted(tool for tool, present in value["tools"].items() if not present)
if missing:
    problems.append(
        "guest lacks " + ", ".join(missing)
        + " (Ubuntu: apt install cloud-guest-utils e2fsprogs fdisk)"
    )
if value["disk_bytes"] != expected:
    problems.append(
        f"guest disk {value['disk']} is {value['disk_bytes']} bytes, "
        f"not the {expected}-byte root zvol"
    )
if problems:
    raise SystemExit("; ".join(problems))
for key in (
    "source", "disk", "partition_number", "disk_bytes", "partition_bytes",
    "fs_bytes", "free_bytes", "unallocated_tail_bytes",
):
    sys.stdout.buffer.write(str(value[key]).encode() + b"\0")
PY
  local -a fields=()
  mapfile -d '' -t fields <"$fields_path"
  ((${#fields[@]} == 8)) || return 1
  GUEST_SOURCE="${fields[0]}"
  GUEST_DISK="${fields[1]}"
  GUEST_PARTITION_NUMBER="${fields[2]}"
  GUEST_DISK_BYTES="${fields[3]}"
  GUEST_PARTITION_BYTES="${fields[4]}"
  GUEST_FS_BYTES="${fields[5]}"
  GUEST_FREE_BYTES="${fields[6]}"
  GUEST_TAIL_BYTES="${fields[7]}"
}

inspect_guest() {
  local expected_disk_bytes="$1" report
  report="$(guest_helper inspect)" ||
    die "Could not inspect the guest root filesystem over SSH"
  load_guest_report "$report" "$expected_disk_bytes" ||
    die "Guest root disk validation failed (see message above)"
}

show_guest_sizes() {
  info "Guest disk /dev/${GUEST_DISK}: $(format_size "$GUEST_DISK_BYTES")"
  info "Root partition ${GUEST_SOURCE}: $(format_size "$GUEST_PARTITION_BYTES")"
  info "Root ext4 filesystem: $(format_size "$GUEST_FS_BYTES")"
  info "Root ext4 free blocks (including root-reserved): $(format_size "$GUEST_FREE_BYTES")"
}

verify_guest_ssh() {
  CURRENT_PHASE="verifying workstation SSH access to the guest"
  log "Guest SSH access"
  info "This script runs growpart and resize2fs inside $RESOURCE_NAME as root over"
  info "your workstation SSH alias (add_prod_vm.sh offers to configure one)."
  info "Before continuing, confirm that 'ssh $RESOURCE_NAME' (or your alias) works."
  while true; do
    prompt_with_default GUEST_ALIAS "Workstation SSH alias for $RESOURCE_NAME" \
      "$RESOURCE_NAME"
    if [[ ! "$GUEST_ALIAS" =~ ^[A-Za-z0-9][A-Za-z0-9_.-]{0,63}$ ]]; then
      warn "SSH alias must use 1-64 letters, digits, dots, underscores, or hyphens"
      continue
    fi
    local effective
    effective="$(ssh -G "$GUEST_ALIAS" 2>/dev/null || true)"
    if ! grep -Fxq "hostname $RESOURCE_IP" <<<"$effective"; then
      warn "Alias $GUEST_ALIAS does not resolve to $RESOURCE_NAME's private IP $RESOURCE_IP"
    elif ! grep -Fxq "user root" <<<"$effective"; then
      warn "Alias $GUEST_ALIAS does not log in as root"
    elif ! ssh -o BatchMode=yes -o ConnectTimeout=15 \
      -o ControlMaster=no -o ControlPath=none "$GUEST_ALIAS" true; then
      warn "Non-interactive key-based 'ssh $GUEST_ALIAS' failed"
    else
      info "Verified non-interactive root SSH: ssh $GUEST_ALIAS"
      return 0
    fi
    prompt_yes "Try another alias?" ||
      die "Guest SSH access is required; fix 'ssh $RESOURCE_NAME' and rerun"
  done
}

measure_placement_storage() {
  CURRENT_PHASE="measuring placement pool capacity"
  local node measured pool dataset size available volsize refreservation
  for node in "${PLACEMENT_NODES[@]}"; do
    # shellcheck disable=SC2016 # This script is intentionally evaluated remotely.
    measured="$(
      node_exec "$node" bash -c '
set -Eeuo pipefail
path="$(pvesm path "$1")"
case "$path" in
  /dev/zvol/*) dataset="${path#/dev/zvol/}" ;;
  *) printf "volume is not a ZFS zvol: %s\n" "$path" >&2; exit 1 ;;
esac
[[ "$dataset" =~ ^[A-Za-z0-9._/+:-]+$ ]]
[[ "$(zfs list -Hp -o type "$dataset")" == volume ]]
pool="${dataset%%/*}"
printf "%s\t%s\t%s\t%s\t%s\t%s\n" "$pool" "$dataset" \
  "$(zpool list -Hp -o size "$pool")" \
  "$(zfs get -Hp -o value available "$pool")" \
  "$(zfs get -Hp -o value volsize "$dataset")" \
  "$(zfs get -Hp -o value refreservation "$dataset")"
' -- "$ROOT_VOLUME"
    )" || die "Could not measure pool capacity and the root zvol on $node"
    IFS=$'\t' read -r pool dataset size available volsize refreservation \
      <<<"$measured"
    for value in "$size" "$available" "$volsize" "$refreservation"; do
      [[ "$value" =~ ^[0-9]+$ ]] || die "$node returned a non-numeric ZFS value"
    done
    NODE_POOL_SIZE["$node"]="$size"
    NODE_POOL_AVAILABLE["$node"]="$available"
    NODE_VOLSIZE["$node"]="$volsize"
    NODE_REFRESERVATION["$node"]="$refreservation"
    printf '%s\t%s\n' "$pool" "$dataset" >"${RUN_DIR}/dataset-${node}"
  done
}

# Every placement copy must have the expected volsize and the registered
# allocation policy; reserved zvols must reserve at least their volsize.
verify_placement_zvols() {
  local expected="$1" node
  for node in "${PLACEMENT_NODES[@]}"; do
    [[ "${NODE_VOLSIZE[$node]}" == "$expected" ]] ||
      die "$node root zvol volsize is ${NODE_VOLSIZE[$node]} bytes, expected $expected"
    if [[ "$DISK_ALLOCATION" == reserved ]]; then
      ((NODE_REFRESERVATION[$node] >= expected)) ||
        die "$node reserved root zvol has refreservation ${NODE_REFRESERVATION[$node]} below its volsize"
    else
      [[ "${NODE_REFRESERVATION[$node]}" == 0 ]] ||
        die "$node sparse root zvol unexpectedly has a refreservation"
    fi
  done
}

compute_allowance() {
  local node output kind
  local -a node_args=()
  for node in "${PLACEMENT_NODES[@]}"; do
    node_args+=("${node}=${NODE_POOL_SIZE[$node]}=${NODE_POOL_AVAILABLE[$node]}")
  done
  output="$(
    calculate_allowance "$DISK_ALLOCATION" \
      "${NODE_VOLSIZE[$OWNER_NODE]}" "${NODE_REFRESERVATION[$OWNER_NODE]}" \
      "$GUEST_FREE_BYTES" "$POOL_RESERVE_PERCENT" "$SPARSE_OVERHEAD_PERMILLE" \
      "${node_args[@]}"
  )" || die "Could not calculate the growth limit"
  printf '%s\n' "$output" >"${RUN_DIR}/allowance.tsv"
  ALLOWED_BYTES=""
  MIN_HEADROOM_BYTES=""
  local first
  while IFS=$'\t' read -r kind first _; do
    case "$kind" in
      headroom) MIN_HEADROOM_BYTES="$first" ;;
      allowed) ALLOWED_BYTES="$first" ;;
    esac
  done <"${RUN_DIR}/allowance.tsv"
  [[ "$ALLOWED_BYTES" =~ ^[0-9]+$ && "$MIN_HEADROOM_BYTES" =~ ^[0-9]+$ ]] ||
    die "Growth limit calculation returned no result"
}

show_allowance() {
  log "Placement pool capacity (each pool keeps ${POOL_RESERVE_PERCENT}% of its total size available)"
  printf '    %-6s %-12s %-20s %-20s %-20s %s\n' \
    NODE POOL "POOL SIZE" "ZFS AVAILABLE" "${POOL_RESERVE_PERCENT}% RESERVE" HEADROOM
  local kind node size available reserve headroom pool dataset
  while IFS=$'\t' read -r kind node size available reserve headroom; do
    [[ "$kind" == node ]] || continue
    IFS=$'\t' read -r pool dataset <"${RUN_DIR}/dataset-${node}"
    printf '    %-6s %-12s %-20s %-20s %-20s %s\n' \
      "$node" "$pool" "$size" "$available" "$reserve" "$headroom"
  done <"${RUN_DIR}/allowance.tsv"
  info "(byte values; ZFS available already excludes other zvols' refreservations)"
  info "Smallest placement headroom, whole MiB: $(format_size "$MIN_HEADROOM_BYTES")"
  info "Root zvol: $ROOT_VOLUME, $(format_size "$CURRENT_DISK_BYTES"), $DISK_ALLOCATION"
  if [[ "$DISK_ALLOCATION" == reserved ]]; then
    info "Owner refreservation: $(format_size "${NODE_REFRESERVATION[$OWNER_NODE]}")"
    info "Limit = headroom x volsize / refreservation, rounded down to a whole MiB"
  else
    info "Guest root free blocks: $(format_size "$GUEST_FREE_BYTES")"
    info "Limit = (headroom - guest free) x (1 - 0.015), rounded down to a whole MiB"
  fi
  printf '\n    MAXIMUM ALLOWED INCREASE: %s\n' "$(format_size "$ALLOWED_BYTES")"
}

# A previous run may have grown the zvol and failed before the guest finished;
# the guest then shows unallocated tail space or ext4 smaller than its partition.
complete_pending_guest_growth() {
  local tail_pending=false fs_pending=false
  ((GUEST_TAIL_BYTES > GUEST_SLACK_TOLERANCE_BYTES)) && tail_pending=true
  ((GUEST_PARTITION_BYTES - GUEST_FS_BYTES > GUEST_SLACK_TOLERANCE_BYTES)) &&
    fs_pending=true
  [[ "$tail_pending" == true || "$fs_pending" == true ]] || return 0

  log "The guest has not grown into its existing disk"
  [[ "$tail_pending" == false ]] ||
    info "Unallocated space after the root partition: $(format_size "$GUEST_TAIL_BYTES")"
  [[ "$fs_pending" == false ]] ||
    info "Root partition exceeds its ext4 filesystem by $(format_size "$((GUEST_PARTITION_BYTES - GUEST_FS_BYTES))")"
  if [[ "$DRY_RUN" == true ]]; then
    info "Dry run: the guest partition and filesystem were not grown"
    return 0
  fi
  prompt_yes "Grow the root partition and filesystem into that space now?" ||
    die "Finish the guest growth before growing the zvol further"
  grow_guest
}

grow_guest() {
  local report
  if ((GUEST_TAIL_BYTES > GUEST_SLACK_TOLERANCE_BYTES)); then
    CURRENT_PHASE="growing the guest root partition"
    log "Growing guest partition ${GUEST_SOURCE} to the end of /dev/${GUEST_DISK}"
    report="$(guest_helper grow-partition "$GUEST_DISK" "$GUEST_PARTITION_NUMBER")" ||
      die "Could not grow the guest root partition"
    load_guest_report "$report" "$GUEST_DISK_BYTES" ||
      die "Guest validation failed after partition growth"
    ((GUEST_TAIL_BYTES <= GUEST_SLACK_TOLERANCE_BYTES)) ||
      die "Root partition still leaves $(format_size "$GUEST_TAIL_BYTES") unallocated"
    info "Root partition after growth: $(format_size "$GUEST_PARTITION_BYTES")"
  else
    info "Root partition already reaches the end of /dev/${GUEST_DISK}"
  fi

  CURRENT_PHASE="growing the guest root ext4 filesystem"
  log "Growing the ext4 filesystem on ${GUEST_SOURCE} online"
  report="$(guest_helper grow-filesystem "${GUEST_SOURCE#/dev/}")" ||
    die "Could not grow the guest root filesystem"
  load_guest_report "$report" "$GUEST_DISK_BYTES" ||
    die "Guest validation failed after filesystem growth"
  ((GUEST_PARTITION_BYTES - GUEST_FS_BYTES <= GUEST_SLACK_TOLERANCE_BYTES)) ||
    die "Root ext4 filesystem did not grow to fill its partition"
  info "Root ext4 filesystem after growth: $(format_size "$GUEST_FS_BYTES")"
}

unit_maximum() {
  case "$1" in
    bytes) printf '%s\n' "$ALLOWED_BYTES" ;;
    MiB) printf '%s\n' "$((ALLOWED_BYTES / MIB))" ;;
    GiB)
      python3 -c 'import sys; v = int(sys.argv[1]); print(f"{v // 2**30}.{v % 2**30 * 1000 // 2**30:03d}")' \
        "$ALLOWED_BYTES"
      ;;
  esac
}

prompt_increase_json() {
  local unit amount result
  local -a parsed=()
  bmac_ui_group_begin increase "Grow the root disk of $RESOURCE_NAME" \
    "Currently $(format_size "$CURRENT_DISK_BYTES"); up to $(format_size "$ALLOWED_BYTES") more is allowed without breaching the ${POOL_RESERVE_PERCENT}% pool reserve. Growth cannot be undone."
  bmac_ui_group_add unit --id unit --type select --label "Unit" --default GiB --required \
    --option bytes bytes --option MiB MiB --option GiB GiB
  bmac_ui_group_add amount --id amount --label "Increase" --required \
    --help "Maximum: $(unit_maximum GiB) GiB, $(unit_maximum MiB) MiB, or $ALLOWED_BYTES bytes. Rounded up to a whole MiB." \
    --pattern '^[0-9]+([.][0-9]+)?$'
  while true; do
    bmac_ui_group_request
    case "$unit" in
      bytes | MiB | GiB) ;;
      *) bmac_ui_field_error unit "Choose bytes, MiB, or GiB" ;;
    esac
    if [[ -z "${BMAC_UI__FIELD_ERRORS[unit]-}" ]]; then
      if result="$(parse_increase "$unit" "$amount" "$ALLOWED_BYTES" 2>"${RUN_DIR}/parse.err")"; then
        read -r -a parsed <<<"$result"
      else
        bmac_ui_field_error amount "$(<"${RUN_DIR}/parse.err") (maximum $(unit_maximum "$unit") $unit)"
      fi
    fi
    bmac_ui_group_check && break
  done
  INCREASE_BYTES="${parsed[1]}"
  if [[ "${parsed[0]}" != "${parsed[1]}" ]]; then
    info "Requested $(format_size "${parsed[0]}"), rounded up to a whole MiB"
  fi
}

prompt_increase() {
  CURRENT_PHASE="choosing the increase"
  local choice unit amount result maximum
  local -a parsed=()
  bmac_ui_is_json && prompt_increase_json
  while ! bmac_ui_is_json; do
    printf '\nEnter the increase in which unit?\n'
    printf '  1) bytes\n  2) MiB\n  3) GiB\n  q) quit without changes\n'
    IFS= read -r -p "Unit [3]: " choice || die "Input ended; no change was made"
    case "${choice,,}" in
      1 | b | byte | bytes) unit=bytes ;;
      2 | m | mib) unit=MiB ;;
      "" | 3 | g | gib) unit=GiB ;;
      q | quit)
        log "No change was made"
        exit 0
        ;;
      *)
        warn "Choose 1, 2, 3, or q"
        continue
        ;;
    esac
    case "$unit" in
      bytes) maximum="$ALLOWED_BYTES" ;;
      MiB) maximum="$((ALLOWED_BYTES / MIB))" ;;
      GiB)
        maximum="$(
          python3 -c 'import sys; v = int(sys.argv[1]); print(f"{v // 2**30}.{v % 2**30 * 1000 // 2**30:03d}")' \
            "$ALLOWED_BYTES"
        )"
        ;;
    esac
    IFS= read -r -p "Increase in ${unit} (maximum ${maximum}; q to quit): " amount ||
      die "Input ended; no change was made"
    if [[ "${amount,,}" == q || "${amount,,}" == quit ]]; then
      log "No change was made"
      exit 0
    fi
    if result="$(parse_increase "$unit" "$amount" "$ALLOWED_BYTES" 2>"${RUN_DIR}/parse.err")"; then
      read -r -a parsed <<<"$result"
      INCREASE_BYTES="${parsed[1]}"
      if [[ "${parsed[0]}" != "${parsed[1]}" ]]; then
        info "Requested $(format_size "${parsed[0]}"), rounded up to a whole MiB"
      fi
      break
    fi
    warn "$(<"${RUN_DIR}/parse.err")"
  done
  NEW_DISK_BYTES="$((CURRENT_DISK_BYTES + INCREASE_BYTES))"
  log "Planned growth"
  info "The zvol will grow by exactly $(format_size "$INCREASE_BYTES")"
  info "From $(format_size "$CURRENT_DISK_BYTES")"
  info "To   $(format_size "$NEW_DISK_BYTES")"
}

acquire_orchestration_lease() {
  CURRENT_PHASE="acquiring the production orchestration lease"
  local metadata contract
  metadata="$(registry_cmd orchestration-get "$RESOURCE_NAME")" ||
    die "Could not read production orchestration metadata"
  contract="$(
    python3 - "$metadata" <<'PY'
import json
import sys
value = json.loads(sys.argv[1])
if value.get("record_type") != "orchestration":
    raise SystemExit("production has no durable orchestration record")
if value.get("install_phase") != "network-finalized":
    raise SystemExit("production installation is not finalized")
for key in ("final_network_enabled", "source_iso_sha256", "install_mode"):
    if key not in value:
        raise SystemExit(f"orchestration record lacks {key}")
print(
    "yes" if value["final_network_enabled"] is True else "no",
    value.get("startup_sha256") or "none",
    value["source_iso_sha256"],
    value["install_mode"],
)
PY
  )" || die "Production orchestration metadata is not leasable"
  local network startup iso mode
  read -r network startup iso mode <<<"$contract"
  LEASE_NONCE="$(python3 -c 'import uuid; print(uuid.uuid4().hex)')"
  registry_cmd orchestration-acquire "$RESOURCE_NAME" \
    --nonce "$LEASE_NONCE" \
    --owner "extend-disk-${COORDINATOR}-$$" \
    --ttl-seconds 14400 \
    --final-network-enabled "$network" \
    --startup-sha256 "$startup" \
    --source-iso-sha256 "$iso" \
    --install-mode "$mode" >/dev/null ||
    die "Could not acquire the production orchestration lease (another operation may hold it)"
  LEASE_ACQUIRED=true
}

live_owner_now() {
  local rows
  rows="$(pvesh_get /cluster/resources --type vm)" || return 1
  python3 - "$rows" "$RESOURCE_NAME" "$VMID" <<'PY'
import json
import sys
rows = json.loads(sys.argv[1])
wanted = int(sys.argv[3])
matches = [row for row in rows if row.get("vmid") in (wanted, str(wanted))]
if (
    len(matches) != 1
    or matches[0].get("name") != sys.argv[2]
    or matches[0].get("status") != "running"
):
    raise SystemExit(1)
print(matches[0].get("node", ""))
PY
}

require_production_unchanged() {
  local observed registry_json revision
  observed="$(live_owner_now)" ||
    die "$RESOURCE_NAME is no longer one running VM"
  [[ "$observed" == "$OWNER_NODE" ]] ||
    die "$RESOURCE_NAME moved from $OWNER_NODE to $observed; rerun the script"
  registry_json="$(registry_cmd get "$RESOURCE_NAME")" ||
    die "Could not re-read the registry record"
  revision="$(
    python3 -c 'import json,sys; print(json.loads(sys.argv[1])["revision"])' \
      "$registry_json"
  )"
  [[ "$revision" == "$RESOURCE_REVISION" ]] ||
    die "The registry record of $RESOURCE_NAME changed concurrently; rerun the script"
}

revalidate_before_resize() {
  CURRENT_PHASE="revalidating immediately before the resize"
  require_production_unchanged
  validate_vm_disk_config "$CURRENT_DISK_BYTES"
  inspect_guest "$CURRENT_DISK_BYTES"
  measure_placement_storage
  verify_placement_zvols "$CURRENT_DISK_BYTES"
  compute_allowance
  ((INCREASE_BYTES <= ALLOWED_BYTES)) ||
    die "Capacity changed: the limit is now $(format_size "$ALLOWED_BYTES"); no change was made"
  info "Live owner, registry revision, guest disk, and capacity limit are unchanged"
}

resize_zvol() {
  CURRENT_PHASE="resizing the root zvol on $OWNER_NODE"
  local new_mib=$((NEW_DISK_BYTES / MIB))
  ((new_mib * MIB == NEW_DISK_BYTES)) || die "New size is not a whole MiB"
  log "Growing $ROOT_VOLUME on live owner $OWNER_NODE to ${new_mib} MiB"
  # qm resize updates the VM config, sets the zvol volsize, and notifies the
  # running QEMU so the guest can see the new capacity without a restart.
  ZVOL_RESIZED=true
  node_exec "$OWNER_NODE" qm resize "$VMID" scsi0 "${new_mib}M" ||
    die "qm resize failed"
  validate_vm_disk_config "$NEW_DISK_BYTES"
  if [[ "$DISK_ALLOCATION" == reserved ]]; then
    local dataset _pool
    IFS=$'\t' read -r _pool dataset <"${RUN_DIR}/dataset-${OWNER_NODE}"
    # OpenZFS raises an auto-sized refreservation with volsize; setting auto
    # again is idempotent and repairs a reservation that did not follow.
    node_exec "$OWNER_NODE" zfs set refreservation=auto "$dataset" ||
      die "Could not refresh the owner zvol refreservation"
  fi
}

record_registry_size() {
  CURRENT_PHASE="recording the exact disk size in the registry"
  local updated
  updated="$(registry_cmd update "$RESOURCE_NAME" \
    --expected-revision "$RESOURCE_REVISION" \
    --disk-bytes "$NEW_DISK_BYTES")" ||
    die "Could not record the new disk size in the registry"
  RESOURCE_REVISION="$(
    python3 -c 'import json,sys; print(json.loads(sys.argv[1])["revision"])' \
      "$updated"
  )"
  REGISTRY_RECORDED=true
  info "Registry records $RESOURCE_NAME root disk as $NEW_DISK_BYTES bytes"
}

target_volsize() {
  local node="$1" dataset _pool
  IFS=$'\t' read -r _pool dataset <"${RUN_DIR}/dataset-${node}"
  node_exec "$node" zfs get -Hp -o value volsize "$dataset"
}

# Replicate until every target zvol reports the new volsize. A sync already
# running when the resize happened can finish with the old size, so a job is
# rescheduled until its target shows the new size.
replicate_new_size() {
  CURRENT_PHASE="replicating the new size to every placement node"
  log "Replicating the new zvol size to ${REPLICATION_JOBS[*]}"
  local deadline=$((SECONDS + REPLICATION_TIMEOUT_SECONDS))
  local -A done_jobs=()
  local -A scheduled_at=()
  local job status state target observed pending
  while true; do
    pending=0
    for job in "${REPLICATION_JOBS[@]}"; do
      [[ -z "${done_jobs[$job]:-}" ]] || continue
      pending=1
      target="${REPLICATION_TARGET[$job]}"
      status="$(pvesh_get "/nodes/${OWNER_NODE}/replication/${job}/status" 2>/dev/null)" ||
        status=""
      state="$(
        python3 - "$status" "$job" 2>/dev/null <<'PY' || printf unknown
import json
import sys
row = json.loads(sys.argv[1])
if row.get("id") != sys.argv[2]:
    raise SystemExit(1)
if row.get("pid"):
    print("busy")
elif int(row.get("fail_count", 0)) != 0 or row.get("error"):
    print("failing")
else:
    print("idle")
PY
      )"
      if [[ "$state" == idle ]]; then
        observed="$(target_volsize "$target" 2>/dev/null || true)"
        if [[ "$observed" == "$NEW_DISK_BYTES" ]]; then
          done_jobs["$job"]=1
          info "Replication $job: $target now has the new $NEW_DISK_BYTES-byte zvol"
          continue
        fi
      fi
      if [[ "$state" != busy ]] &&
        ((SECONDS - ${scheduled_at[$job]:--1000} >= 30)); then
        # pvesr refuses while another sync holds the guest lock; retry later.
        if node_exec "$OWNER_NODE" pvesr schedule-now "$job" >/dev/null 2>&1; then
          info "Requested replication $job to $target"
        fi
        scheduled_at["$job"]=$SECONDS
      fi
    done
    ((pending)) || break
    ((SECONDS < deadline)) ||
      die "Timed out after ${REPLICATION_TIMEOUT_SECONDS}s waiting for replication of the new size"
    sleep 10
  done
  measure_placement_storage
  verify_placement_zvols "$NEW_DISK_BYTES"
  info "Every placement copy has volsize $NEW_DISK_BYTES and the $DISK_ALLOCATION policy"
}

rescan_guest_disk() {
  CURRENT_PHASE="rescanning the guest disk"
  log "Rescanning /dev/${GUEST_DISK} inside $RESOURCE_NAME"
  local report
  report="$(guest_helper rescan "$GUEST_DISK" "$NEW_DISK_BYTES" "$RESCAN_TIMEOUT_SECONDS")" ||
    die "The guest did not see the new disk size"
  load_guest_report "$report" "$NEW_DISK_BYTES" ||
    die "Guest validation failed after the disk rescan"
  info "Guest disk /dev/${GUEST_DISK}: $(format_size "$GUEST_DISK_BYTES")"
}

main() {
  parse_args "$@"
  load_and_validate_config
  preflight_workstation
  trap cleanup EXIT

  discover_cluster
  select_production
  validate_live_production
  reconcile_registry_owner
  verify_guest_ssh

  CURRENT_PHASE="inspecting the guest root filesystem"
  log "Guest root disk before growth"
  inspect_guest "$CURRENT_DISK_BYTES"
  show_guest_sizes
  measure_placement_storage
  verify_placement_zvols "$CURRENT_DISK_BYTES"
  complete_pending_guest_growth
  compute_allowance
  show_allowance

  if [[ "$DRY_RUN" == true ]]; then
    log "Dry run complete; no state was changed"
    bmac_ui_step_done
    bmac_ui_result resource "$RESOURCE_NAME" disk_bytes:int "$CURRENT_DISK_BYTES" \
      allowed_growth_bytes:int "$ALLOWED_BYTES"
    ((ALLOWED_BYTES == 0)) ||
      bmac_ui_next_step "Grow $RESOURCE_NAME by up to $(format_size "$ALLOWED_BYTES")." \
        --workflow extend_prod_vm_disk --arg dry_run=false
    return 0
  fi
  if ((ALLOWED_BYTES == 0)); then
    log "No growth is allowed without breaching the ${POOL_RESERVE_PERCENT}% pool reserve"
    bmac_ui_step_done
    bmac_ui_result resource "$RESOURCE_NAME" disk_bytes:int "$CURRENT_DISK_BYTES" allowed_growth_bytes:int 0
    bmac_ui_next_step "Free space in the placement pools or add a disk vdev before growing $RESOURCE_NAME." \
      --workflow add_new_disk_vdev
    return 0
  fi

  prompt_increase
  confirm_go "This grows $RESOURCE_NAME's root zvol, its replicas, root partition, and ext4 filesystem online. Growth cannot be undone."
  acquire_orchestration_lease
  revalidate_before_resize

  local before_partition="$GUEST_PARTITION_BYTES" before_fs="$GUEST_FS_BYTES"
  resize_zvol
  record_registry_size
  replicate_new_size
  rescan_guest_disk
  # A zvol growth leaves exactly the new space after the last partition.
  ((GUEST_TAIL_BYTES > GUEST_SLACK_TOLERANCE_BYTES)) ||
    die "The guest shows no unallocated space after the root partition"
  grow_guest

  log "Production disk growth complete"
  info "VM: $RESOURCE_NAME (VMID $VMID) on $OWNER_NODE"
  info "Zvol grew by $(format_size "$INCREASE_BYTES")"
  info "Zvol now $(format_size "$NEW_DISK_BYTES") on ${PLACEMENT_NODES[*]}"
  info "Root partition: $(format_size "$before_partition") -> $(format_size "$GUEST_PARTITION_BYTES")"
  info "Root ext4 filesystem: $(format_size "$before_fs") -> $(format_size "$GUEST_FS_BYTES")"
  info "Root ext4 free blocks now: $(format_size "$GUEST_FREE_BYTES")"
  CURRENT_PHASE="releasing the orchestration lease"
  registry_cmd orchestration-release "$RESOURCE_NAME" \
    --nonce "$LEASE_NONCE" >/dev/null ||
    die "Could not release the orchestration lease"
  LEASE_ACQUIRED=false
  bmac_ui_step_done
  bmac_ui_result resource "$RESOURCE_NAME" vmid "$VMID" increase_bytes:int "$INCREASE_BYTES" \
    disk_bytes:int "$NEW_DISK_BYTES" root_fs_bytes:int "$GUEST_FS_BYTES"
  bmac_ui_next_step "Check the application on $RESOURCE_NAME." --command "ssh $GUEST_ALIAS df -h /"
}

if [[ "${EXTEND_PROD_DISK_SOURCE_ONLY:-0}" != 1 ]]; then
  bmac_ui_bootstrap "$@"
  main "$@"
fi
