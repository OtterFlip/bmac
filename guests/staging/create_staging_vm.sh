#!/usr/bin/env bash

# Copyright (c) 2026 BEENTHERE VENTURES, INC.
# SPDX-License-Identifier: GPL-3.0-only

# Create one guarded, registry-backed staging VM from a production snapshot.
# This intentionally accepts a fragile, unofficial ZFS dependency beneath
# Proxmox replication. See README.md before using it.

set -Eeuo pipefail
set +x
umask 077

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
REPO_ROOT="$(cd -- "${SCRIPT_DIR}/../.." && pwd -P)"
CONFIG_LIB="${REPO_ROOT}/lib/config.sh"
PATCH_HELPER="${SCRIPT_DIR}/patch_staging_clone.sh"
GUEST_TREE_HELPER="${SCRIPT_DIR}/patch_staging_guest_tree.py"
REMOTE_INSTALL_ROOT="/usr/local/lib/app-ha-proxmox"
REMOTE_REGISTRY="${REMOTE_INSTALL_ROOT}/lib/cluster_registry.py"
REMOTE_HAPROXY_SYNC="${REMOTE_INSTALL_ROOT}/lib/sync_haproxy_routes.sh"

DRY_RUN=false
REPLICATION_TIMEOUT_SECONDS=14400
START_TIMEOUT_SECONDS=300
ROLLBACK_REPLICATION_TIMEOUT_SECONDS=600
SANITIZER_FILE=""
CORES_OVERRIDE=""
MEMORY_GIB_OVERRIDE=""

CURRENT_PHASE="startup"
COORDINATOR=""
RUN_DIR=""
REMOTE_RUN_DIR=""
ROOT_PASSWORD_HASH=""
DOMAIN_OVERRIDE=""
NETWORK_LINK_DOWN=false
START_AFTER_CREATION=true
SETUP_WORKSTATION_JUMP_SSH=true

SOURCE_NAME=""
SOURCE_VMID=""
SOURCE_PURPOSE=""
SOURCE_PRIMARY_DOMAIN=""
SOURCE_REGISTRY_OWNER=""
SOURCE_OWNER=""
SOURCE_VOLUME=""
SOURCE_STATE=""
SOURCE_DISK_GIB=""
SOURCE_REPLICATION_MINUTES=""
SOURCE_DATASET=""
declare -a SOURCE_VOLUMES=()

STAGING_NODE=""
STAGING_NAME=""
STAGING_VMID=""
STAGING_IP=""
STAGING_MAC=""
STAGING_DOMAIN=""
STAGING_REVISION=""
STAGING_STATE=""
STAGING_VOLUME=""
STAGING_HOST_LIMIT=""
STAGING_HOST_COUNT=""
STAGING_CANDIDATE_INDEX=""
CLONE_DATASET=""
CLONE_PATH=""
SNAPSHOT_NAME=""
SNAPSHOT_GUID=""

REGISTRY_RESERVED=false
SNAPSHOT_MAY_EXIST=false
CLONE_MAY_EXIST=false
VM_MAY_EXIST=false
DISK_ATTACHED=false
ROUTES_ENABLED=false
PATCH_SUCCEEDED=false
COMMITTED=false
ROLLBACK_RUNNING=false

declare -a CLUSTER_NODES=()
declare -a ONLINE_NODES=()
declare -a SOURCE_PLACEMENT=()
declare -a REPLICATION_JOBS=()
declare -a REPLICATION_TARGETS=()
declare -a DNS_SERVERS=()

log() {
  printf '\n==> %s\n' "$*"
}

info() {
  printf '    %s\n' "$*"
}

warn() {
  printf 'WARNING: %s\n' "$*" >&2
}

die() {
  printf '\nERROR: %s\n' "$*" >&2
  exit 1
}

usage() {
  cat <<'EOF'
Usage: create_staging_vm.sh [options]

Run from an administrator workstation. The script lists production registry
resources, prompts for a source prodN, automatically selects the lowest
currently-online HA/replication-eligible standby, and creates one stopped
stageNprodN staging VM.

Options:
  --dry-run                       Perform live read-only validation and show
                                  the candidate allocation without reserving
                                  or changing Proxmox.
  --sanitizer PATH                Optional local Bash script injected as a
                                  root one-shot before guest networking.
  --cores N                       Override the prompted cluster vCPU default.
  --memory-gib N                  Override the prompted cluster RAM default.
  --replication-timeout-seconds N Default: 14400
  --start-timeout-seconds N       Default: 300
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
      --sanitizer)
        (($# >= 2)) || die "--sanitizer requires a path"
        SANITIZER_FILE="$2"
        shift 2
        ;;
      --cores | --memory-gib)
        (($# >= 2)) || die "$1 requires a value"
        validate_positive_integer "$1" "$2"
        if [[ "$1" == --cores ]]; then
          CORES_OVERRIDE="$2"
        else
          MEMORY_GIB_OVERRIDE="$2"
        fi
        shift 2
        ;;
      --replication-timeout-seconds | --start-timeout-seconds)
        (($# >= 2)) || die "$1 requires a value"
        validate_positive_integer "$1" "$2"
        if [[ "$1" == --replication-timeout-seconds ]]; then
          REPLICATION_TIMEOUT_SECONDS="$2"
        else
          START_TIMEOUT_SECONDS="$2"
        fi
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

require_command_local() {
  command -v "$1" >/dev/null 2>&1 ||
    die "Required workstation command is unavailable: $1"
}

# The guest root password becomes a SHA-512 crypt hash locally so the plaintext
# never crosses SSH. Apple's /usr/bin/openssl is LibreSSL, which has no
# "passwd -6"; say so before secrets are loaded instead of failing mid-load.
require_openssl_sha512_crypt() {
  local probe
  probe="$(
    printf 'probe' | openssl passwd -6 -salt appha0probe -stdin 2>/dev/null
  )" || probe=""
  [[ "$probe" == '$6$'* ]] ||
    die "This openssl cannot produce SHA-512 crypt hashes (openssl passwd -6). On macOS: brew install openssl@3 and put its bin directory first on PATH."
}

require_regular_file() {
  [[ -f "$1" && ! -L "$1" ]] ||
    die "Required regular, non-symlink file is missing: $1"
}

validate_sanitizer_file() {
  [[ "$SANITIZER_FILE" == /* ]] ||
    die "Sanitizer path must be absolute"
  require_regular_file "$SANITIZER_FILE"
  # wc -c is portable where stat's format flags are not. A failed or
  # unparseable wc must reject: an empty string compares as 0 in arithmetic.
  local sanitizer_size
  sanitizer_size="$(wc -c <"$SANITIZER_FILE")" ||
    die "Could not measure the sanitizer size"
  sanitizer_size="${sanitizer_size//[[:space:]]/}"
  [[ "$sanitizer_size" =~ ^[0-9]+$ ]] ||
    die "Could not measure the sanitizer size"
  ((sanitizer_size <= 1048576)) ||
    die "Sanitizer exceeds the 1 MiB limit"
  local sanitizer_header
  IFS= read -r sanitizer_header <"$SANITIZER_FILE"
  [[ "$sanitizer_header" == '#!/bin/bash' ||
    "$sanitizer_header" == '#!/usr/bin/env bash' ]] ||
    die "Sanitizer must have a Bash shebang"
  bash -n "$SANITIZER_FILE" ||
    die "Sanitizer is not valid Bash"
}

validate_safe_id() {
  [[ "$2" =~ ^[A-Za-z0-9][A-Za-z0-9_.:+/@-]*$ ]] ||
    die "$1 contains unsafe characters: $2"
}

prompt_with_default() {
  local destination="$1" prompt="$2" default="$3" entered
  IFS= read -r -p "${prompt} [${default}]: " entered
  printf -v "$destination" '%s' "${entered:-$default}"
}

prompt_optional() {
  local destination="$1" prompt="$2" entered
  IFS= read -r -p "${prompt}: " entered
  printf -v "$destination" '%s' "$entered"
}

prompt_yes() {
  local answer
  IFS= read -r -p "$1 [y/N] " answer
  [[ "${answer,,}" == y || "${answer,,}" == yes ]]
}

prompt_boolean_default() {
  local destination="$1" prompt="$2" default="$3" answer suffix
  if [[ "$default" == true ]]; then
    suffix="[Y/n]"
  else
    suffix="[y/N]"
  fi
  IFS= read -r -p "$prompt $suffix " answer
  case "${answer,,}" in
    "")
      printf -v "$destination" '%s' "$default"
      ;;
    y | yes)
      printf -v "$destination" '%s' true
      ;;
    n | no)
      printf -v "$destination" '%s' false
      ;;
    *)
      die "Please answer yes or no"
      ;;
  esac
}

confirm_exact_local() {
  local prompt="$1" _legacy_phrase="${2:-}" entered
  printf '%s\nType GO to continue.\n> ' "$prompt"
  IFS= read -r entered
  [[ "$entered" == GO ]] ||
    die "Confirmation did not match GO; no mutation was made"
}

normalize_domain() {
  python3 - "$1" <<'PY'
import ipaddress
import re
import sys

value = sys.argv[1].strip().rstrip(".").lower()
if not value or value.startswith("*.") or "://" in value or "/" in value or ":" in value:
    raise SystemExit("domain must be an exact bare hostname")
try:
    value = value.encode("idna").decode("ascii")
except UnicodeError as exc:
    raise SystemExit(f"domain cannot be normalized: {exc}")
if len(value) > 253 or "." not in value:
    raise SystemExit("domain must be fully qualified")
label = re.compile(r"^[a-z0-9](?:[a-z0-9-]{0,61}[a-z0-9])?$")
if any(not label.fullmatch(part) for part in value.split(".")):
    raise SystemExit("domain contains an invalid label")
try:
    ipaddress.ip_address(value)
except ValueError:
    print(value)
else:
    raise SystemExit("domain may not be an IP address")
PY
}

split_dns_servers() {
  local raw="$1" parsed
  parsed="$(mktemp)"
  if ! python3 - "$raw" >"$parsed" <<'PY'
import ipaddress
import re
import sys

values = [part for part in re.split(r"[\s,;]+", sys.argv[1].strip()) if part]
if not values:
    raise SystemExit("at least one DNS server is required")
seen = set()
for value in values:
    normalized = str(ipaddress.ip_address(value))
    if normalized in seen:
        raise SystemExit(f"duplicate DNS server: {normalized}")
    seen.add(normalized)
    print(normalized)
PY
  then
    rm -f -- "$parsed"
    die "Could not parse GUEST_DNS_SERVERS"
  fi
  mapfile -t DNS_SERVERS <"$parsed"
  rm -f -- "$parsed"
}

load_and_validate_config() {
  CURRENT_PHASE="loading layered configuration"
  require_regular_file "$CONFIG_LIB"
  # shellcheck source=../../lib/config.sh
  source "$CONFIG_LIB"
  load_proxmox_config --require-secrets ||
    die "Could not load cluster.conf and secrets.env"

  local name
  local required=(
    PROXMOX_CLUSTER_NAME MAX_MOX_HOSTS PRIVATE_SUBNET_CIDR GUEST_EGRESS_VIP
    STAGING_IP_START STAGING_IP_END STAGING_GUEST_VM_ROOT_PASSWORD
    PRODUCTION_VM_TAG STAGING_VM_TAG EVICTABLE_VM_TAG PURPOSE_TAG_PREFIX
    GUEST_ROLE_HOOK_PATH CLUSTER_STATE_DIR PROD_VM_STORAGE PROD_VM_CPU_TYPE
    STAGING_VM_CORES STAGING_VM_MEMORY_GIB STAGING_VM_STORAGE
    STAGING_VM_BRIDGE SNIPPETS_STORAGE_ID
  )
  for name in "${required[@]}"; do
    require_var "$name" || die "Configuration is incomplete"
  done
  [[ "$PROD_VM_STORAGE" == local-zfs &&
    "$STAGING_VM_STORAGE" == "$PROD_VM_STORAGE" ]] ||
    die "Production and staging must both use local-zfs for this clone model"
  for name in \
    PRODUCTION_VM_TAG STAGING_VM_TAG EVICTABLE_VM_TAG PURPOSE_TAG_PREFIX \
    PROD_VM_STORAGE PROD_VM_CPU_TYPE STAGING_VM_BRIDGE SNIPPETS_STORAGE_ID; do
    validate_safe_id "$name" "${!name}"
  done
  [[ "$CLUSTER_STATE_DIR" =~ ^/[A-Za-z0-9._/-]+$ ]] ||
    die "CLUSTER_STATE_DIR must be a safe absolute path"
  validate_positive_integer "STAGING_VM_CORES" "$STAGING_VM_CORES"
  validate_positive_integer "STAGING_VM_MEMORY_GIB" "$STAGING_VM_MEMORY_GIB"
  split_dns_servers "${GUEST_DNS_SERVERS:-${PROXMOX_DNS_SERVER:-}}"
  GUEST_GATEWAY="${GUEST_EGRESS_VIP%/*}"

  # Derive a one-way crypt hash, then remove every secret-layer variable
  # before any SSH/SCP child can inherit it. Neither value is logged.
  local plaintext_password="$STAGING_GUEST_VM_ROOT_PASSWORD"
  ROOT_PASSWORD_HASH="$(
    printf '%s' "$plaintext_password" | openssl passwd -6 -stdin
  )"
  plaintext_password=""
  unset plaintext_password
  for name in "${_PROXMOX_CONFIG_SECRET_KEYS[@]}"; do
    unset "$name"
  done
  [[ "$ROOT_PASSWORD_HASH" == '$6$'* ]] ||
    die "Could not derive the staging console password hash"
}

preflight_workstation() {
  CURRENT_PHASE="validating workstation"
  local command_name
  for command_name in \
    bash basename chmod date install mktemp openssl python3 rm scp ssh stat \
    wc; do
    require_command_local "$command_name"
  done
  require_regular_file "$PATCH_HELPER"
  require_regular_file "$GUEST_TREE_HELPER"
  bash -n "$PATCH_HELPER" ||
    die "Offline patch helper has invalid Bash syntax"
  if [[ -n "$SANITIZER_FILE" ]]; then
    validate_sanitizer_file
  fi
  [[ ! -L "${SCRIPT_DIR}/artifacts" ]] ||
    die "The staging artifacts path may not be a symlink"
  install -d -m 0700 "${SCRIPT_DIR}/artifacts"
  chmod 0700 "${SCRIPT_DIR}/artifacts"
  RUN_DIR="${SCRIPT_DIR}/artifacts/run-$(date -u +%Y%m%dT%H%M%SZ)-$$"
  install -d -m 0700 "$RUN_DIR"
}

# Execute one argv array on a mox node. Non-coordinator nodes are reached
# through Proxmox root cluster SSH trust without constructing an eval string.
node_exec() {
  local node="$1"
  shift
  [[ "$node" =~ ^mox([1-9]|10)$ ]] || {
    printf 'Unsafe mox node: %s\n' "$node" >&2
    return 1
  }
  (($# > 0)) || {
    printf 'node_exec requires a command\n' >&2
    return 1
  }
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
        -o ConnectTimeout="${MOX_SSH_CONNECT_TIMEOUT:-8}" \
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
if mode not in {"all", "online"}:
    raise SystemExit("invalid cluster node selection mode")
seen = set()
selected = []
for row in rows:
    if not isinstance(row, dict):
        raise SystemExit("malformed node row")
    node = row.get("node", row.get("name"))
    match = re.fullmatch(r"mox([1-9]|10)", str(node))
    if not match or int(match.group(1)) > maximum:
        raise SystemExit(f"unexpected cluster node: {node!r}")
    if node in seen:
        raise SystemExit(f"duplicate cluster node: {node}")
    seen.add(node)
    is_online = (
        str(row.get("status", "")).lower() == "online"
        or row.get("online") in (1, True)
    )
    if mode == "all" or is_online:
        selected.append(node)
selected.sort(key=lambda value: int(value[3:]))
if not selected:
    raise SystemExit(
        "pvesh /nodes returned no configured mox nodes"
        if mode == "all"
        else "no online mox nodes"
    )
print(*selected, sep="\n")
PY
}

display_production_resources() {
  local path="$1"
  python3 - "$path" <<'PY'
import json
import sys

rows = json.load(open(sys.argv[1], encoding="utf-8"))
production = [row for row in rows if row.get("kind") == "production"]
production.sort(key=lambda row: row.get("index", 0))
if not production:
    raise SystemExit("registry contains no production resources")
for row in production:
    purpose = row.get("purpose", {}).get("slug", "?")
    domain = row.get("domains", {}).get("primary", "?")
    placement = ",".join(row.get("placement", []))
    owner = row.get("owner_node") or "unknown"
    print(
        f"{row.get('name')}\t{purpose}\t{domain}\t{placement}\t"
        f"{row.get('state')}\t{owner}"
    )
PY
}

select_coordinator() {
  CURRENT_PHASE="discovering the cluster"
  local configured_nodes online_nodes
  COORDINATOR="$(first_reachable_mox)" ||
    die "No reachable mox coordinator was found"
  info "Coordinator: $COORDINATOR"
  node_exec "$COORDINATOR" test -x "$REMOTE_REGISTRY" ||
    die "Installed cluster registry is unavailable on $COORDINATOR"
  node_exec "$COORDINATOR" test -x "$REMOTE_HAPROXY_SYNC" ||
    die "Installed HAProxy route sync is unavailable on $COORDINATOR"
  registry_cmd record-staging-snapshot --help >/dev/null ||
    die "Installed cluster registry lacks staging snapshot metadata support"
  registry_cmd record-staging-snapshot-intent --help >/dev/null ||
    die "Installed cluster registry lacks pre-snapshot intent support"
  node_exec "$COORDINATOR" bash -c \
    'for command in ha-manager pvesh pvesm pvesr qm zfs; do command -v "$command" >/dev/null; done' ||
    die "Coordinator lacks required Proxmox/ZFS commands"

  write_json "${RUN_DIR}/cluster-status.json" "$(pvesh_get /cluster/status)"
  python3 - "${RUN_DIR}/cluster-status.json" "$PROXMOX_CLUSTER_NAME" <<'PY'
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
  write_json "${RUN_DIR}/nodes.json" "$(pvesh_get /nodes)"
  configured_nodes="$(
    parse_cluster_nodes "${RUN_DIR}/nodes.json" "$MAX_MOX_HOSTS" all
  )" || die "Could not validate configured cluster nodes"
  online_nodes="$(
    parse_cluster_nodes "${RUN_DIR}/nodes.json" "$MAX_MOX_HOSTS" online
  )" || die "Could not validate online cluster nodes"
  mapfile -t CLUSTER_NODES <<<"$configured_nodes"
  mapfile -t ONLINE_NODES <<<"$online_nodes"
  write_json "${RUN_DIR}/resources.json" "$(registry_cmd list)"

  printf '\nRegistered production resources:\n'
  printf '  %-8s %-18s %-30s %-18s %-11s %s\n' \
    NAME PURPOSE DOMAIN PLACEMENT STATE OWNER
  while IFS=$'\t' read -r name purpose domain placement state owner; do
    printf '  %-8s %-18s %-30s %-18s %-11s %s\n' \
      "$name" "$purpose" "$domain" "$placement" "$state" "$owner"
  done < <(display_production_resources "${RUN_DIR}/resources.json") ||
    die "Could not list registered production resources"
}

parse_selected_source() {
  local source="$1" fields_path="${RUN_DIR}/source.fields"
  python3 - "${RUN_DIR}/resources.json" "$source" >"$fields_path" <<'PY'
import json
import re
import sys

rows = json.load(open(sys.argv[1], encoding="utf-8"))
name = sys.argv[2]
if not re.fullmatch(r"prod[1-9][0-9]*", name):
    raise SystemExit("source must use the prodN formula")
matches = [row for row in rows if row.get("name") == name]
if len(matches) != 1 or matches[0].get("kind") != "production":
    raise SystemExit("source is not one registered production resource")
row = matches[0]
if row.get("state") != "active":
    raise SystemExit("source production resource must be active")
placement = row.get("placement")
if not isinstance(placement, list) or len(placement) < 2:
    raise SystemExit("source needs at least two HA placement nodes")
if set(row.get("proxmox", {}).get("ha_nodes", [])) != set(placement):
    raise SystemExit("registered HA nodes differ from production placement")
volume = row.get("proxmox", {}).get("volume_id")
if not isinstance(volume, str) or not volume:
    raise SystemExit("source has no registered production root volume")
values = (
    row["name"],
    row["vmid"],
    row["purpose"]["slug"],
    row["domains"]["primary"],
    row.get("owner_node") or "",
    volume,
    row["state"],
    row["spec"]["disk_gib"],
    row["spec"]["replication_interval"],
    ",".join(placement),
)
for value in values:
    sys.stdout.buffer.write(str(value).encode() + b"\0")
PY
  local -a fields=()
  mapfile -d '' -t fields <"$fields_path"
  ((${#fields[@]} == 10)) ||
    die "Could not parse selected production resource"
  SOURCE_NAME="${fields[0]}"
  SOURCE_VMID="${fields[1]}"
  SOURCE_PURPOSE="${fields[2]}"
  SOURCE_PRIMARY_DOMAIN="${fields[3]}"
  SOURCE_REGISTRY_OWNER="${fields[4]}"
  SOURCE_VOLUME="${fields[5]}"
  SOURCE_STATE="${fields[6]}"
  SOURCE_DISK_GIB="${fields[7]}"
  SOURCE_REPLICATION_MINUTES="${fields[8]}"
  IFS=',' read -r -a SOURCE_PLACEMENT <<<"${fields[9]}"
}

validate_live_source_and_ha() {
  write_json "${RUN_DIR}/cluster-vms.json" "$(
    pvesh_get /cluster/resources --type vm
  )"
  write_json "${RUN_DIR}/ha-resources.json" "$(
    pvesh_get /cluster/ha/resources
  )"
  write_json "${RUN_DIR}/ha-rules.json" "$(
    node_exec "$COORDINATOR" ha-manager rules config --output-format json
  )"
  SOURCE_OWNER="$(
    python3 - \
      "${RUN_DIR}/cluster-vms.json" \
      "${RUN_DIR}/ha-resources.json" \
      "${RUN_DIR}/ha-rules.json" \
      "${RUN_DIR}/nodes.json" \
      "$SOURCE_NAME" "$SOURCE_VMID" \
      "$(IFS=,; printf '%s' "${SOURCE_PLACEMENT[*]}")" <<'PY'
import json
import re
import sys

vm_path, ha_path, rules_path, nodes_path, name, vmid_text, placement_csv = sys.argv[1:]
vmid = int(vmid_text)
placement = placement_csv.split(",")
vms = json.load(open(vm_path, encoding="utf-8"))
matches = [
    row for row in vms
    if row.get("vmid") in (vmid, str(vmid))
]
if len(matches) != 1:
    raise SystemExit("source VMID is not unique in live cluster resources")
vm = matches[0]
if (
    vm.get("name") != name
    or vm.get("type") != "qemu"
    or vm.get("status") != "running"
    or vm.get("node") not in placement
):
    raise SystemExit("source must be one running QEMU VM on registered placement")

nodes = json.load(open(nodes_path, encoding="utf-8"))
online = {
    row.get("node", row.get("name"))
    for row in nodes
    if str(row.get("status", "")).lower() == "online"
    or row.get("online") in (1, True)
}
missing = sorted(set(placement) - online)
if missing:
    raise SystemExit(
        "every production placement node must be online: " + ", ".join(missing)
    )

ha = json.load(open(ha_path, encoding="utf-8"))
ha_matches = [row for row in ha if row.get("sid") == f"vm:{vmid}"]
if len(ha_matches) != 1:
    raise SystemExit("source is not exactly one Proxmox HA resource")
requested = str(ha_matches[0].get("state", "started")).lower()
if requested not in {"started", "enabled"}:
    raise SystemExit(f"source HA requested state is unsafe: {requested}")
if (
    int(ha_matches[0].get("failback", -1)) != 0
    or int(ha_matches[0].get("auto-rebalance", -1)) != 0
):
    raise SystemExit("source HA failback/auto-rebalance policy is unsafe")

rules = json.load(open(rules_path, encoding="utf-8"))
matching_rules = []
for rule in rules:
    resources = rule.get("resources", [])
    if not isinstance(resources, list):
        resources = [part for part in str(resources).split(",") if part]
    if f"vm:{vmid}" not in resources:
        continue
    nodes_value = rule.get("nodes", [])
    if not isinstance(nodes_value, list):
        nodes_value = [part for part in str(nodes_value).split(",") if part]
    rule_nodes = {}
    for part in nodes_value:
        node, separator, priority = str(part).partition(":")
        rule_nodes[node] = int(priority) if separator else 0
    strict = rule.get("strict", 0) in (1, True, "1")
    enabled = rule.get("enabled", 1) in (1, True, "1")
    disabled = rule.get("disable", 0) in (1, True, "1")
    affinity = rule.get("affinity", "positive")
    if (
        rule.get("type") in (None, "node-affinity")
        and strict
        and enabled
        and not disabled
        and affinity == "positive"
        and rule_nodes == {node: 1 for node in placement}
    ):
        matching_rules.append(rule)
if len(matching_rules) != 1:
    raise SystemExit(
        "source needs exactly one strict positive HA node-affinity rule "
        "matching placement"
    )
print(vm["node"])
PY
  )" || die "Live source/HA validation failed"
  [[ "$SOURCE_OWNER" =~ ^mox([1-9]|10)$ ]] ||
    die "Live source owner is invalid"
  [[ "$SOURCE_REGISTRY_OWNER" == "$SOURCE_OWNER" ]] ||
    die "Registry owner $SOURCE_REGISTRY_OWNER differs from live owner $SOURCE_OWNER"
}

load_replication_jobs_and_choose_standby() {
  write_json "${RUN_DIR}/replication-config.json" "$(
    pvesh_get /cluster/replication
  )"
  mapfile -t REPLICATION_JOBS < <(
    python3 - "${RUN_DIR}/replication-config.json" "$SOURCE_VMID" <<'PY'
import json
import sys
rows = json.load(open(sys.argv[1], encoding="utf-8"))
wanted = int(sys.argv[2])
jobs = []
for row in rows:
    if row.get("guest") not in (wanted, str(wanted)):
        continue
    if row.get("disable") in (1, True, "1"):
        raise SystemExit(f"production replication job {row.get('id')} is disabled")
    jobs.append(str(row.get("id", "")))
if any(not job for job in jobs):
    raise SystemExit("production replication job has no id")
print(*sorted(jobs), sep="\n")
PY
  )
  ((${#REPLICATION_JOBS[@]} == ${#SOURCE_PLACEMENT[@]} - 1)) ||
    die "Production must have one replication job per standby placement node"

  REPLICATION_TARGETS=()
  local job status target replication_state parsed
  for job in "${REPLICATION_JOBS[@]}"; do
    local deadline=$((SECONDS + 300))
    while true; do
      status="$(pvesh_get \
        "/nodes/${SOURCE_OWNER}/replication/${job}/status")" ||
        die "Could not read replication status for $job"
      parsed="$(
        python3 - "$status" "$job" <<'PY'
import json
import re
import sys
row = json.loads(sys.argv[1])
if row.get("id") != sys.argv[2]:
    raise SystemExit("replication status id mismatch")
target = row.get("target")
if not re.fullmatch(r"mox([1-9]|10)", str(target)):
    raise SystemExit("replication status has invalid target")
if (
    not isinstance(row.get("last_sync"), int)
    or row["last_sync"] <= 0
    or int(row.get("fail_count", 0)) != 0
    or row.get("error")
):
    raise SystemExit("production replication job is not currently healthy")
state = "busy" if row.get("pid") else "ready"
print(f"{state}\t{target}")
PY
      )" || die "Replication job $job is not healthy"
      IFS=$'\t' read -r replication_state target <<<"$parsed"
      [[ "$replication_state" == ready || "$replication_state" == busy ]] ||
        die "Replication job $job returned an invalid status"
      if [[ "$replication_state" == ready ]]; then
        break
      fi
      ((SECONDS < deadline)) ||
        die "Timed out waiting for active replication job $job to finish"
      info "Replication job $job is currently running; waiting for it to finish"
      sleep 5
    done
    REPLICATION_TARGETS+=("$target")
  done

  STAGING_NODE="$(
    python3 - \
      "$SOURCE_OWNER" \
      "$(IFS=,; printf '%s' "${SOURCE_PLACEMENT[*]}")" \
      "$(IFS=,; printf '%s' "${REPLICATION_TARGETS[*]}")" \
      "$(IFS=,; printf '%s' "${ONLINE_NODES[*]}")" <<'PY'
import sys
owner = sys.argv[1]
placement = sys.argv[2].split(",")
targets = sys.argv[3].split(",")
online = set(sys.argv[4].split(","))
expected = set(placement) - {owner}
if set(targets) != expected or len(targets) != len(set(targets)):
    raise SystemExit("effective replication targets differ from placement minus owner")
candidates = sorted(
    (node for node in expected if node in online),
    key=lambda value: int(value[3:]),
)
if not candidates:
    raise SystemExit("no currently-online HA/replication-eligible standby exists")
print(candidates[0])
PY
  )" || die "Could not select an eligible standby"
  info "Active production owner: $SOURCE_OWNER"
  info "Selected staging standby: $STAGING_NODE"
  validate_staging_host_capacity
}

validate_staging_host_capacity() {
  CURRENT_PHASE="checking staging host capacity"
  STAGING_HOST_LIMIT="$(
    bash -c '
set -Eeuo pipefail
source "$1"
load_proxmox_config --host "$2" --no-secrets
printf "%s\n" "$MAX_STAGING_VM_COUNT_ON_THIS_HOST"
' bash "$CONFIG_LIB" "$STAGING_NODE"
  )" || die "Could not load the staging limit for $STAGING_NODE"
  validate_positive_integer \
    "MAX_STAGING_VM_COUNT_ON_THIS_HOST for $STAGING_NODE" \
    "$STAGING_HOST_LIMIT"
  local capacity
  capacity="$(
    python3 - "${RUN_DIR}/resources.json" "$STAGING_NODE" \
      "$SOURCE_NAME" "$STAGING_HOST_LIMIT" <<'PY'
import json
import sys

rows = json.load(open(sys.argv[1], encoding="utf-8"))
node = sys.argv[2]
source = sys.argv[3]
limit = int(sys.argv[4])
count = sum(
    1
    for row in rows
    if row.get("kind") == "staging" and row.get("placement") == [node]
)
used = {
    row["index"]
    for row in rows
    if row.get("kind") == "staging" and row.get("source") == source
}
candidate = next(
    (value for value in range(1, limit + 1) if value not in used),
    None,
)
if candidate is None:
    raise SystemExit(
        f"stage number range 1-{limit} is exhausted for source {source}"
    )
print(f"{count}\t{candidate}")
PY
  )" || die "Could not determine staging capacity on $STAGING_NODE"
  IFS=$'\t' read -r STAGING_HOST_COUNT STAGING_CANDIDATE_INDEX <<<"$capacity"
  [[ "$STAGING_HOST_COUNT" =~ ^[0-9]+$ ]] ||
    die "Staging host count is invalid"
  validate_positive_integer \
    "Candidate staging number" "$STAGING_CANDIDATE_INDEX"
  ((STAGING_HOST_COUNT < STAGING_HOST_LIMIT)) ||
    die "$STAGING_NODE already has $STAGING_HOST_COUNT registered staging guests (limit $STAGING_HOST_LIMIT)"
  info "Staging capacity on $STAGING_NODE: $STAGING_HOST_COUNT/$STAGING_HOST_LIMIT used"
}

verify_source_vm_contract() {
  CURRENT_PHASE="validating production clone contract"
  local architecture config="${RUN_DIR}/source-qm.conf" layout_json node pve_version
  for node in "${SOURCE_PLACEMENT[@]}"; do
    node_exec "$node" bash -c \
      'for command in pvesm pvesr pveversion qm uname zfs; do command -v "$command" >/dev/null; done' ||
      die "$node lacks required Proxmox/ZFS commands"
    architecture="$(node_exec "$node" uname -m)" ||
      die "Could not query architecture on $node"
    [[ "$architecture" == x86_64 ]] ||
      die "$node is not amd64; the cloned OVMF guest is incompatible"
    pve_version="$(node_exec "$node" pveversion)" ||
      die "Could not query the Proxmox version on $node"
    [[ "$pve_version" == pve-manager/9.* ]] ||
      die "$node is not running Proxmox VE 9: $pve_version"
    node_exec "$node" pvesm status --storage "$PROD_VM_STORAGE" >/dev/null ||
      die "$PROD_VM_STORAGE is unavailable on $node"
  done
  node_exec "$STAGING_NODE" bash -c '
set -Eeuo pipefail
for command in \
  bash blkid blockdev cat chmod fdisk find findmnt flock fuser grep install ln lsblk \
  mktemp mount mv pvesm python3 qm readlink rm stat sync udevadm \
  umount wc zfs; do
  command -v "$command" >/dev/null
done
' || die "$STAGING_NODE lacks offline-patch or ZFS prerequisites"
  node_exec "$STAGING_NODE" pvesm status --storage "$STAGING_VM_STORAGE" \
    >/dev/null ||
    die "$STAGING_VM_STORAGE is unavailable on $STAGING_NODE"
  node_exec "$SOURCE_OWNER" qm config "$SOURCE_VMID" >"$config"
  chmod 0600 "$config"
  local source_volumes
  source_volumes="$(
    python3 - "$config" "$SOURCE_NAME" "$SOURCE_VOLUME" \
      "$PROD_VM_STORAGE" "$PRODUCTION_VM_TAG" \
      "${PURPOSE_TAG_PREFIX}${SOURCE_PURPOSE}" <<'PY'
import re
import sys

path, name, registered_volume, storage, production_tag, purpose_tag = sys.argv[1:]
config = {}
for line in open(path, encoding="utf-8"):
    if ": " in line:
        key, value = line.rstrip("\n").split(": ", 1)
        config[key] = value
if config.get("name") != name:
    raise SystemExit("production VM name differs from registry")
if config.get("template", "0") == "1":
    raise SystemExit("production source may not be a template")
if config.get("bios") != "ovmf" or not config.get("machine", "").startswith("q35"):
    raise SystemExit("production source must use OVMF with q35")
if config.get("agent", "").split(",", 1)[0] not in {"1", "enabled=1"}:
    raise SystemExit("production QEMU guest agent is disabled")
if "freeze-fs-on-backup=0" in config.get("agent", ""):
    raise SystemExit("production QEMU guest-agent filesystem freeze is disabled")
tags = {value for value in re.split(r"[;,]", config.get("tags", "")) if value}
if tags != {production_tag, purpose_tag}:
    raise SystemExit("production source tags differ from its exact registry role")
networks = [key for key in config if re.fullmatch(r"net[0-9]+", key)]
if networks != ["net0"]:
    raise SystemExit("production source must have exactly one NIC")
disk_keys = {
    key
    for key in config
    if re.fullmatch(
        r"(?:(?:scsi|sata|virtio|ide|unused|efidisk|tpmstate)[0-9]+)",
        key,
    )
}
if disk_keys != {"scsi0", "efidisk0"}:
    raise SystemExit("production source must have exactly scsi0 and efidisk0")
volume = config["scsi0"].split(",", 1)[0]
value = config["scsi0"]
if volume != registered_volume or not volume.startswith(storage + ":"):
    raise SystemExit("production root volume differs from registry/local-zfs")
if "replicate=0" in value:
    raise SystemExit("production root volume has replication disabled")
efi_volume = config["efidisk0"].split(",", 1)[0]
if (
    not efi_volume.startswith(storage + ":")
    or efi_volume == volume
    or "replicate=0" in config["efidisk0"]
):
    raise SystemExit("production EFI vars volume is not replicated local-zfs")
print(volume)
print(efi_volume)
PY
  )" || die "Production VM does not meet the one-disk clone contract"
  mapfile -t SOURCE_VOLUMES <<<"$source_volumes"
  ((${#SOURCE_VOLUMES[@]} == 2)) ||
    die "Production snapshot volume inventory is incomplete"
  SOURCE_VOLUME="${SOURCE_VOLUMES[0]}"
  validate_safe_id "source root volume" "$SOURCE_VOLUME"
  validate_safe_id "source EFI volume" "${SOURCE_VOLUMES[1]}"
  node_exec "$SOURCE_OWNER" qm agent "$SOURCE_VMID" ping >/dev/null ||
    die "Production QEMU guest agent is not responsive"
  layout_json="$(
    node_exec "$SOURCE_OWNER" qm guest exec "$SOURCE_VMID" --timeout 30 -- \
      /bin/lsblk --json --paths \
      --output NAME,TYPE,FSTYPE,MOUNTPOINTS,PKNAME
  )" || die "Could not inspect production block layout through QGA"
  python3 - "$layout_json" <<'PY'
import json
import sys

envelope = json.loads(sys.argv[1])
if envelope.get("exited") != 1 or envelope.get("exitcode") != 0:
    raise SystemExit("production lsblk guest command failed")
layout = json.loads(envelope.get("out-data", ""))
roots = layout.get("blockdevices")
if not isinstance(roots, list):
    raise SystemExit("production lsblk returned malformed JSON")
entries = []
def walk(node):
    entries.append(node)
    for child in node.get("children") or []:
        walk(child)
for root in roots:
    walk(root)
for entry in entries:
    if entry.get("type") in {"lvm", "crypt", "raid0", "raid1", "raid10", "md"}:
        raise SystemExit("production root must use direct partitions, not a mapped layer")
roots_ext4 = []
for entry in entries:
    mounts = entry.get("mountpoints")
    if isinstance(mounts, str):
        mounts = [mounts]
    if entry.get("type") == "part" and entry.get("fstype") == "ext4" and "/" in (mounts or []):
        roots_ext4.append(entry)
if len(roots_ext4) != 1:
    raise SystemExit("production must have exactly one mounted direct ext4 root partition")
PY
}

query_next_vmid() {
  local nextid live="${RUN_DIR}/cluster-vms.json"
  nextid="$(pvesh_get /cluster/nextid)" ||
    die "Could not query the next free Proxmox VMID"
  STAGING_VMID="$(
    python3 - "$nextid" "$live" "${RUN_DIR}/resources.json" <<'PY'
import json
import sys
value = json.loads(sys.argv[1])
if isinstance(value, str) and value.isdigit():
    value = int(value)
if not isinstance(value, int) or not 100 <= value <= 999_999_999:
    raise SystemExit("invalid /cluster/nextid response")
live = json.load(open(sys.argv[2], encoding="utf-8"))
registered = json.load(open(sys.argv[3], encoding="utf-8"))
used = {
    int(row["vmid"])
    for row in [*live, *registered]
    if str(row.get("vmid", "")).isdigit()
}
while value in used or 9111 <= value <= 9120:
    value += 1
print(value)
PY
  )" || die "Could not select a free candidate VMID"
}

collect_request() {
  local source_default
  source_default="$(
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
  [[ -n "$source_default" ]] || die "No production source is available"
  prompt_with_default SOURCE_NAME "Production source" "$source_default"
  parse_selected_source "$SOURCE_NAME" ||
    die "Invalid production source selection"
  validate_live_source_and_ha
  load_replication_jobs_and_choose_standby
  verify_source_vm_contract
  query_next_vmid

  local default_memory_gib requested_memory_gib
  default_memory_gib="$STAGING_VM_MEMORY_GIB"
  if [[ -n "$CORES_OVERRIDE" ]]; then
    STAGING_VM_CORES="$CORES_OVERRIDE"
  else
    prompt_with_default STAGING_VM_CORES \
      "Staging vCPU cores" "$STAGING_VM_CORES"
    validate_positive_integer "Staging vCPU cores" "$STAGING_VM_CORES"
  fi
  if [[ -n "$MEMORY_GIB_OVERRIDE" ]]; then
    requested_memory_gib="$MEMORY_GIB_OVERRIDE"
  else
    prompt_with_default requested_memory_gib \
      "Staging RAM in GiB" "$default_memory_gib"
    validate_positive_integer "Staging RAM in GiB" "$requested_memory_gib"
  fi
  STAGING_VM_MEMORY_MB=$((requested_memory_gib * 1024))

  prompt_boolean_default START_AFTER_CREATION \
    "Start VM automatically after it has been created?" true
  prompt_boolean_default SETUP_WORKSTATION_JUMP_SSH \
    "Set up permanent jump SSH after the VM has started?" true

  prompt_optional DOMAIN_OVERRIDE \
    "Exact staging FQDN override (blank atomically selects the lowest free number; currently expected stage${STAGING_CANDIDATE_INDEX}${SOURCE_NAME}.${SOURCE_PRIMARY_DOMAIN})"
  if [[ -n "$DOMAIN_OVERRIDE" ]]; then
    DOMAIN_OVERRIDE="$(normalize_domain "$DOMAIN_OVERRIDE")" ||
      die "Invalid staging domain override"
  fi
  if prompt_yes "Create the staging NIC initially link-down?"; then
    NETWORK_LINK_DOWN=true
  else
    NETWORK_LINK_DOWN=false
  fi

  if [[ -z "$SANITIZER_FILE" ]]; then
    prompt_optional SANITIZER_FILE \
      "Optional absolute path to an idempotent root startup.sh (blank for none)"
    [[ -z "$SANITIZER_FILE" ]] || validate_sanitizer_file
  fi

  warn "This uses a direct ZFS clone pinned to a replicated production snapshot."
  warn "Proxmox does not officially track or support this cross-replica dependency."
  if [[ -z "$SANITIZER_FILE" && "$NETWORK_LINK_DOWN" == false ]]; then
    warn "NO application sanitizer was supplied and guest networking will be enabled."
    warn "The clone can still contain production credentials, jobs, data, and outbound integrations."
    confirm_exact_local \
      "Identity and network cleanup is not application-secret sanitization." \
      "ALLOW UNSANITIZED STAGING NETWORK"
  fi
}

dry_run_summary() {
  local candidate
  candidate="$(
    python3 - "${RUN_DIR}/resources.json" "$SOURCE_NAME" \
      "$SOURCE_PRIMARY_DOMAIN" "$DOMAIN_OVERRIDE" "$PRIVATE_SUBNET_CIDR" \
      "$STAGING_HOST_LIMIT" <<'PY'
import ipaddress
import json
import sys
rows = json.load(open(sys.argv[1], encoding="utf-8"))
source, primary, override, subnet, raw_limit = sys.argv[2:]
limit = int(raw_limit)
suffixes = {
    row["index"] for row in rows
    if row.get("kind") == "staging" and row.get("source") == source
}
suffix = next(
    (value for value in range(1, limit + 1) if value not in suffixes),
    None,
)
if suffix is None:
    raise SystemExit(
        f"stage number range 1-{limit} is exhausted for source {source}"
    )
name = f"stage{suffix}{source}"
used = {ipaddress.ip_address(row["ip"]) for row in rows}
network = ipaddress.ip_network(subnet)
address = next(network.network_address + value for value in range(51, 201) if network.network_address + value not in used)
domain = override or f"stage{suffix}{source}.{primary}"
print(f"{name}\t{address}\t{domain}")
PY
  )"
  IFS=$'\t' read -r STAGING_NAME STAGING_IP STAGING_DOMAIN <<<"$candidate"
  log "Dry run complete"
  info "Candidate identity (not reserved): $STAGING_NAME / $STAGING_IP / VMID $STAGING_VMID"
  info "Domain: $STAGING_DOMAIN"
  info "Source/owner: $SOURCE_NAME / $SOURCE_OWNER"
  info "Standby: $STAGING_NODE"
  info "Compute: ${STAGING_VM_CORES} vCPU / $((STAGING_VM_MEMORY_MB / 1024)) GiB RAM"
  info "Start after creation: $START_AFTER_CREATION"
  info "Set up workstation jump SSH: $SETUP_WORKSTATION_JUMP_SSH"
  info "NIC link-down: $NETWORK_LINK_DOWN"
  if [[ -n "$SANITIZER_FILE" ]]; then
    info "Optional pre-network sanitizer: supplied and syntax-valid"
  else
    info "Optional pre-network sanitizer: NOT supplied"
  fi
}

parse_staging_resource() {
  local value="$1" path="${RUN_DIR}/staging-resource.json"
  printf '%s\n' "$value" >"$path"
  chmod 0600 "$path"
  local -a fields=()
  mapfile -d '' -t fields < <(
    python3 - "$path" <<'PY'
import json
import sys
row = json.load(open(sys.argv[1], encoding="utf-8"))
values = (
    row["name"],
    row["vmid"],
    row["ip"],
    row["mac"],
    row["domains"]["primary"],
    row["revision"],
    row["state"],
)
for value in values:
    sys.stdout.buffer.write(str(value).encode() + b"\0")
PY
  )
  ((${#fields[@]} == 7)) || die "Could not parse staging registry resource"
  STAGING_NAME="${fields[0]}"
  STAGING_VMID="${fields[1]}"
  STAGING_IP="${fields[2]}"
  STAGING_MAC="${fields[3]}"
  STAGING_DOMAIN="${fields[4]}"
  STAGING_REVISION="${fields[5]}"
  STAGING_STATE="${fields[6]}"
}

registry_update_stage() {
  local value
  value="$(
    registry_cmd update "$STAGING_NAME" \
      --expected-revision "$STAGING_REVISION" "$@"
  )" || die "Registry update failed for $STAGING_NAME"
  parse_staging_resource "$value"
}

reserve_staging() {
  CURRENT_PHASE="atomically reserving staging identity"
  local -a args=(
    allocate-staging
    --source "$SOURCE_NAME"
    --vmid "$STAGING_VMID"
    --placement "$STAGING_NODE"
    --placement-limit "$STAGING_HOST_LIMIT"
    --allocation-id "create-staging-${SOURCE_NAME}-$(date -u +%Y%m%dT%H%M%SZ)-$$"
    --cores "$STAGING_VM_CORES"
    --memory-mb "$STAGING_VM_MEMORY_MB"
    --disk-gib "$SOURCE_DISK_GIB"
    --disk-allocation sparse
    --replication-interval "$SOURCE_REPLICATION_MINUTES"
  )
  [[ -z "$DOMAIN_OVERRIDE" ]] || args+=(--domain "$DOMAIN_OVERRIDE")
  local result resource
  result="$(registry_cmd "${args[@]}")" ||
    die "Atomic staging allocation failed"
  resource="$(
    python3 - "$result" <<'PY'
import json
import sys
value = json.loads(sys.argv[1])
if value.get("created") is not True:
    raise SystemExit("staging creation requires a fresh allocation")
print(json.dumps(value["resource"], sort_keys=True))
PY
  )" || die "Registry did not return a fresh staging allocation"
  parse_staging_resource "$resource"
  REGISTRY_RESERVED=true
  SNAPSHOT_NAME="stg-base-${STAGING_NAME}-$(date -u +%y%m%dT%H%M%SZ)"
  [[ "$SNAPSHOT_NAME" =~ ^[A-Za-z][A-Za-z0-9._-]{0,39}$ &&
    "$SNAPSHOT_NAME" != __replicate_* ]] ||
    die "Derived staging snapshot name is unsafe or exceeds Proxmox's 40-character limit"
  STAGING_VOLUME="${STAGING_VM_STORAGE}:vm-${STAGING_VMID}-disk-0"
  validate_safe_id "staging volume" "$STAGING_VOLUME"
  info "Reserved: $STAGING_NAME / $STAGING_IP / VMID $STAGING_VMID"
  info "MAC/domain: $STAGING_MAC / $STAGING_DOMAIN"
}

preflight_reserved_identity() {
  CURRENT_PHASE="rechecking reserved VM and storage identity"
  local live
  live="$(pvesh_get /cluster/resources --type vm)" ||
    die "Could not recheck live VMIDs after registry allocation"
  python3 - "$live" "$STAGING_VMID" <<'PY'
import json
import sys
wanted = int(sys.argv[2])
matches = [
    row for row in json.loads(sys.argv[1])
    if row.get("vmid") in (wanted, str(wanted))
]
if matches:
    raise SystemExit("reserved staging VMID became live before qm create")
PY
  SOURCE_DATASET="$(
    volume_dataset_on_node "$STAGING_NODE" "$SOURCE_VOLUME"
  )" || die "Could not resolve the standby production root zvol"
  CLONE_DATASET="${SOURCE_DATASET%/*}/vm-${STAGING_VMID}-disk-0"
  [[ "$CLONE_DATASET" != "$SOURCE_DATASET" &&
    "$CLONE_DATASET" =~ ^[A-Za-z0-9._/+:-]+$ ]] ||
    die "Derived staging clone dataset is unsafe"
  if node_exec "$STAGING_NODE" zfs list -H -o name "$CLONE_DATASET" \
    >/dev/null 2>&1; then
    die "Staging clone dataset already exists: $CLONE_DATASET"
  fi
}

source_owner_now() {
  local rows
  rows="$(pvesh_get /cluster/resources --type vm)" || return
  python3 - "$rows" "$SOURCE_NAME" "$SOURCE_VMID" <<'PY'
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

require_source_unchanged() {
  local observed
  observed="$(source_owner_now)" ||
    die "Production source is no longer one running VM"
  [[ "$observed" == "$SOURCE_OWNER" ]] ||
    die "Production owner changed from $SOURCE_OWNER to $observed during staging creation"
}

snapshot_presence() {
  local node="$1"
  local snapshots
  snapshots="$(pvesh_get \
    "/nodes/${node}/qemu/${SOURCE_VMID}/snapshot")" || return
  python3 - "$snapshots" "$SNAPSHOT_NAME" \
    "app-ha staging base for ${STAGING_NAME}" <<'PY'
import json
import sys
rows = json.loads(sys.argv[1])
matches = [row for row in rows if row.get("name") == sys.argv[2]]
if len(matches) > 1:
    raise SystemExit("duplicate Proxmox VM snapshot name")
if not matches:
    print("absent")
elif matches[0].get("description", "") == sys.argv[3]:
    print("present")
else:
    print("foreign")
PY
}

create_source_snapshot() {
  CURRENT_PHASE="creating Proxmox staging-base snapshot"
  require_source_unchanged
  registry_update_stage --state snapshotting
  local presence volume recorded
  presence="$(snapshot_presence "$SOURCE_OWNER")" ||
    die "Could not inspect production VM snapshots"
  if [[ "$presence" == present ]]; then
    die "Derived snapshot already exists in the production VM configuration"
  fi
  if [[ "$presence" == foreign ]]; then
    die "Derived snapshot name is already owned by another operation"
  fi
  [[ "$presence" == absent ]] ||
    die "Production VM snapshot query returned an unexpected result"
  local -a intent_args=(
    record-staging-snapshot-intent "$STAGING_NAME"
    --expected-revision "$STAGING_REVISION"
    --snapshot-name "$SNAPSHOT_NAME"
    --snapshot-owner-node "$SOURCE_OWNER"
    --source-volume-id "$SOURCE_VOLUME"
  )
  for volume in "${SOURCE_VOLUMES[@]}"; do
    intent_args+=(--snapshotted-volume-id "$volume")
  done
  recorded="$(registry_cmd "${intent_args[@]}")" ||
    die "Could not durably record snapshot intent before qm snapshot"
  parse_staging_resource "$recorded"
  SNAPSHOT_MAY_EXIST=true
  # This is a named Proxmox VM snapshot, never a raw target-only snapshot and
  # never a __replicate_* snapshot. Proxmox takes its normal VM/config locks.
  node_exec "$SOURCE_OWNER" qm snapshot "$SOURCE_VMID" "$SNAPSHOT_NAME" \
    --description "app-ha staging base for ${STAGING_NAME}" \
    --vmstate 0
  presence="$(snapshot_presence "$SOURCE_OWNER")" ||
    die "Could not verify the new Proxmox VM snapshot"
  [[ "$presence" == present ]] ||
    die "Proxmox VM snapshot is absent after qm snapshot"
  registry_update_stage --state replicating
}

replication_status_target() {
  local status="$1" job="$2" minimum_sync="$3"
  python3 - "$status" "$job" "$minimum_sync" <<'PY'
import json
import re
import sys
row = json.loads(sys.argv[1])
target = row.get("target")
ok = (
    row.get("id") == sys.argv[2]
    and re.fullmatch(r"mox([1-9]|10)", str(target))
    and isinstance(row.get("last_sync"), int)
    and row["last_sync"] >= int(sys.argv[3])
    and int(row.get("fail_count", 0)) == 0
    and not row.get("error")
    and not row.get("pid")
)
if not ok:
    raise SystemExit(1)
print(target)
PY
}

trigger_and_wait_for_replication() {
  CURRENT_PHASE="replicating staging-base snapshot"
  require_source_unchanged
  local minimum_sync=$(( $(date +%s) - 5 )) job
  for job in "${REPLICATION_JOBS[@]}"; do
    node_exec "$SOURCE_OWNER" pvesr schedule-now "$job"
  done

  local deadline=$((SECONDS + REPLICATION_TIMEOUT_SECONDS))
  local status target
  for job in "${REPLICATION_JOBS[@]}"; do
    target=""
    while ((SECONDS < deadline)); do
      status="$(pvesh_get \
        "/nodes/${SOURCE_OWNER}/replication/${job}/status" 2>/dev/null)" || {
        sleep 10
        continue
      }
      target="$(replication_status_target "$status" "$job" "$minimum_sync" 2>/dev/null)" ||
        target=""
      [[ -z "$target" ]] || break
      sleep 10
    done
    [[ -n "$target" ]] ||
      die "Timed out waiting for replication job $job after snapshot creation"
    info "Snapshot replication job $job completed to $target"
  done
  require_source_unchanged
}

volume_dataset_on_node() {
  local node="$1" volume="$2"
  node_exec "$node" bash -c '
set -Eeuo pipefail
path="$(pvesm path "$1")"
case "$path" in
  /dev/zvol/*) dataset="${path#/dev/zvol/}" ;;
  *) printf "volume is not a ZFS zvol: %s\n" "$path" >&2; exit 1 ;;
esac
[[ "$dataset" =~ ^[A-Za-z0-9._/+:-]+$ ]]
[[ "$(zfs list -Hp -o type "$dataset")" == volume ]]
printf "%s\n" "$dataset"
' -- "$volume"
}

verify_snapshot_guids() {
  CURRENT_PHASE="verifying every cluster-wide snapshot volume GUID"
  local volume node dataset guid first_guid selected_dataset=""
  local -a guid_args=() volume_guid_args=()
  for volume in "${SOURCE_VOLUMES[@]}"; do
    first_guid=""
    for node in "${SOURCE_PLACEMENT[@]}"; do
      dataset="$(volume_dataset_on_node "$node" "$volume")" ||
        die "Could not resolve $volume on $node"
      guid="$(
        node_exec "$node" zfs get -Hp -o value guid \
          "${dataset}@${SNAPSHOT_NAME}"
      )" || die "Snapshot $SNAPSHOT_NAME for $volume is absent on $node"
      [[ "$guid" =~ ^[0-9]{1,20}$ ]] ||
        die "$node returned an invalid ZFS snapshot GUID for $volume"
      if [[ -z "$first_guid" ]]; then
        first_guid="$guid"
      elif [[ "$guid" != "$first_guid" ]]; then
        die "Snapshot ZFS GUID for $volume differs between placement nodes"
      fi
      volume_guid_args+=(--snapshot-volume-guid "${volume},${node}=${guid}")
      if [[ "$volume" == "$SOURCE_VOLUME" ]]; then
        guid_args+=(--snapshot-guid "${node}=${guid}")
        [[ "$node" != "$STAGING_NODE" ]] || selected_dataset="$dataset"
      fi
    done
    [[ -n "$first_guid" ]] ||
      die "Could not verify every placement copy of $volume"
    [[ "$volume" != "$SOURCE_VOLUME" ]] || SNAPSHOT_GUID="$first_guid"
  done
  [[ -n "$SNAPSHOT_GUID" && -n "$selected_dataset" ]] ||
    die "Could not verify every root snapshot copy"
  [[ "$selected_dataset" == "$SOURCE_DATASET" ]] ||
    die "Standby source dataset changed after allocation preflight"

  local recorded
  recorded="$(
    registry_cmd record-staging-snapshot "$STAGING_NAME" \
      --expected-revision "$STAGING_REVISION" \
      --snapshot-name "$SNAPSHOT_NAME" \
      --snapshot-owner-node "$SOURCE_OWNER" \
      --source-volume-id "$SOURCE_VOLUME" \
      "${guid_args[@]}" \
      "${volume_guid_args[@]}"
  )" || die "Could not record the verified staging snapshot dependency"
  parse_staging_resource "$recorded"
  registry_update_stage --state cloning
}

clone_snapshot_locally() {
  CURRENT_PHASE="creating unofficial local ZFS linked clone"
  require_source_unchanged
  local output="${RUN_DIR}/clone.fields"
  [[ "$CLONE_DATASET" != "$SOURCE_DATASET" &&
    "$CLONE_DATASET" =~ ^[A-Za-z0-9._/+:-]+$ ]] ||
    die "Derived staging clone dataset is unsafe"
  CLONE_MAY_EXIST=true
  node_exec "$STAGING_NODE" bash -c '
set -Eeuo pipefail
source_dataset="$1"
snapshot="$2"
expected_guid="$3"
target_volume="$4"
storage="$5"
vmid="$6"

[[ "$source_dataset" =~ ^[A-Za-z0-9._/+:-]+$ ]]
[[ "$snapshot" =~ ^[A-Za-z][A-Za-z0-9._-]{0,39}$ ]]
[[ "$snapshot" != __replicate_* ]]
[[ "$target_volume" == "${storage}:vm-${vmid}-disk-0" ]]
source_guid="$(zfs get -Hp -o value guid "${source_dataset}@${snapshot}")"
[[ "$source_guid" == "$expected_guid" ]]
target_dataset="${source_dataset%/*}/vm-${vmid}-disk-0"
[[ "$target_dataset" != "$source_dataset" ]]
if zfs list -Hp -o name "$target_dataset" >/dev/null 2>&1; then
  printf "target clone dataset already exists: %s\n" "$target_dataset" >&2
  exit 1
fi
zfs clone -p -o volmode=full -o refreservation=none \
  "${source_dataset}@${snapshot}" "$target_dataset"
zfs set refreservation=none "$target_dataset"
[[ "$(zfs get -Hp -o value origin "$target_dataset")" == "${source_dataset}@${snapshot}" ]]
[[ "$(zfs list -Hp -o type "$target_dataset")" == volume ]]
path="$(pvesm path "$target_volume")"
[[ "$path" == "/dev/zvol/${target_dataset}" && -b "$path" ]]
printf "%s\n%s\n" "$target_dataset" "$path"
' -- "$SOURCE_DATASET" "$SNAPSHOT_NAME" "$SNAPSHOT_GUID" \
    "$STAGING_VOLUME" "$STAGING_VM_STORAGE" "$STAGING_VMID" >"$output"
  mapfile -t _clone_fields <"$output"
  ((${#_clone_fields[@]} == 2)) ||
    die "Could not parse the local clone identity"
  [[ "${_clone_fields[0]}" == "$CLONE_DATASET" ]] ||
    die "Created clone dataset differs from the precomputed rollback target"
  CLONE_PATH="${_clone_fields[1]}"
  [[ "$CLONE_DATASET" =~ ^[A-Za-z0-9._/+:-]+$ &&
    "$CLONE_PATH" == /dev/zvol/* ]] ||
    die "Local clone returned unsafe paths"
}

hook_reference() {
  local hook_name
  hook_name="$(basename -- "$GUEST_ROLE_HOOK_PATH")"
  [[ "$hook_name" =~ ^[A-Za-z0-9._-]+[.]sh$ ]] ||
    die "Lifecycle hook filename is unsafe"
  printf '%s:snippets/%s\n' "$SNIPPETS_STORAGE_ID" "$hook_name"
}

stage_vm_is_ha() {
  local resources
  resources="$(pvesh_get /cluster/ha/resources)" || return
  python3 - "$resources" "$STAGING_VMID" <<'PY'
import json
import sys
sid = f"vm:{sys.argv[2]}"
raise SystemExit(0 if any(row.get("sid") == sid for row in json.loads(sys.argv[1])) else 1)
PY
}

create_unattached_vm() {
  CURRENT_PHASE="creating stopped staging VM shell"
  require_source_unchanged
  local hook hook_path purpose_tag tags net0
  hook="$(hook_reference)"
  purpose_tag="${PURPOSE_TAG_PREFIX}${SOURCE_PURPOSE}"
  validate_safe_id "purpose tag" "$purpose_tag"
  [[ "$purpose_tag" != "$STAGING_VM_TAG" &&
    "$purpose_tag" != "$EVICTABLE_VM_TAG" &&
    "$STAGING_VM_TAG" != "$EVICTABLE_VM_TAG" ]] ||
    die "Staging, evictable, and purpose tags must be distinct"
  tags="${STAGING_VM_TAG};${EVICTABLE_VM_TAG};${purpose_tag}"
  net0="virtio=${STAGING_MAC},bridge=${STAGING_VM_BRIDGE},firewall=1"
  [[ "$NETWORK_LINK_DOWN" == false ]] || net0+=",link_down=1"
  hook_path="$(node_exec "$STAGING_NODE" pvesm path "$hook")" ||
    die "Lifecycle hook cannot be resolved on $STAGING_NODE"
  [[ "$hook_path" =~ ^/[A-Za-z0-9._/-]+$ ]] ||
    die "Lifecycle hook resolved to an unsafe path"
  node_exec "$STAGING_NODE" test -x "$hook_path" ||
    die "Lifecycle hook is unavailable on $STAGING_NODE"
  stage_vm_is_ha &&
    die "Reserved staging VMID is unexpectedly an HA resource"

  VM_MAY_EXIST=true
  node_exec "$STAGING_NODE" qm create "$STAGING_VMID" \
    --name "$STAGING_NAME" \
    --description "app-ha disposable staging clone of ${SOURCE_NAME}; snapshot ${SNAPSHOT_NAME}" \
    --machine q35 \
    --bios ovmf \
    --sockets 1 \
    --cores "$STAGING_VM_CORES" \
    --memory "$STAGING_VM_MEMORY_MB" \
    --balloon 0 \
    --cpu "$PROD_VM_CPU_TYPE" \
    --ostype l26 \
    --scsihw virtio-scsi-single \
    --efidisk0 "${STAGING_VM_STORAGE}:1,efitype=4m,pre-enrolled-keys=1" \
    --net0 "$net0" \
    --agent "enabled=1,fstrim_cloned_disks=1" \
    --hookscript "$hook" \
    --tags "$tags" \
    --vga std \
    --tablet 0 \
    --onboot 0
  verify_staging_vm_config unattached
}

verify_staging_vm_config() {
  local expected_mode="$1" hook purpose_tag config="${RUN_DIR}/stage-qm.conf"
  hook="$(hook_reference)"
  purpose_tag="${PURPOSE_TAG_PREFIX}${SOURCE_PURPOSE}"
  node_exec "$STAGING_NODE" qm config "$STAGING_VMID" >"$config"
  chmod 0600 "$config"
  python3 - "$config" "$STAGING_NAME" "$STAGING_MAC" "$STAGING_VM_BRIDGE" \
    "$STAGING_VM_TAG" "$EVICTABLE_VM_TAG" "$purpose_tag" "$hook" \
    "$expected_mode" "$STAGING_VOLUME" "$NETWORK_LINK_DOWN" \
    "$STAGING_VM_STORAGE" "$STAGING_VMID" <<'PY'
import re
import sys

(
    path, name, mac, bridge, staging_tag, evictable_tag, purpose_tag,
    hook, expected_mode, volume, link_down, storage, raw_vmid,
) = sys.argv[1:]
vmid = int(raw_vmid)
config = {}
for line in open(path, encoding="utf-8"):
    if ": " in line:
        key, value = line.rstrip("\n").split(": ", 1)
        config[key] = value
if config.get("name") != name:
    raise SystemExit("staging VM name differs from registry")
if config.get("onboot", "0") != "0":
    raise SystemExit("staging VM onboot must be disabled")
if config.get("template", "0") == "1":
    raise SystemExit("staging VM may not be a template")
if config.get("hookscript") != hook:
    raise SystemExit("staging lifecycle hook differs from contract")
tags = {value for value in re.split(r"[;,]", config.get("tags", "")) if value}
if tags != {staging_tag, evictable_tag, purpose_tag}:
    raise SystemExit("staging VM tags must be exactly staging, evictable, and purpose")
network_keys = [key for key in config if re.fullmatch(r"net[0-9]+", key)]
if network_keys != ["net0"]:
    raise SystemExit("staging VM must have exactly one NIC")
net0 = config["net0"]
if f"virtio={mac}".lower() not in net0.lower() or f"bridge={bridge}" not in net0:
    raise SystemExit("staging net0 MAC or bridge differs from registry")
has_link_down = "link_down=1" in net0
if has_link_down != (link_down == "true"):
    raise SystemExit("staging net0 link_down differs from the operator choice")
disk_keys = {
    key
    for key in config
    if re.fullmatch(
        r"(?:(?:scsi|sata|virtio|ide|unused|efidisk|tpmstate)[0-9]+)",
        key,
    )
}
expected_disk_keys = (
    {"efidisk0"} if expected_mode == "unattached" else {"scsi0", "efidisk0"}
)
if disk_keys != expected_disk_keys:
    raise SystemExit("staging VM disk keys differ from the exact lifecycle phase")
efi_volume = config["efidisk0"].split(",", 1)[0]
if not re.fullmatch(
    rf"{re.escape(storage)}:vm-{vmid}-disk-[1-9][0-9]*",
    efi_volume,
) or efi_volume == volume:
    raise SystemExit("staging EFI vars disk is not fresh VM-owned storage")
if expected_mode == "unattached":
    pass
else:
    if config["scsi0"].split(",", 1)[0] != volume:
        raise SystemExit("staging VM must have exactly the patched scsi0 clone")
PY
  if stage_vm_is_ha; then
    die "Staging VM must never be registered with Proxmox HA"
  fi
  [[ "$(node_exec "$STAGING_NODE" qm status "$STAGING_VMID")" == "status: stopped" ]] ||
    die "Staging VM must remain stopped until explicitly offered to the operator"
}

upload_file_to_staging_node() {
  local source="$1" destination="$2" mode="$3"
  local coordinator_stage coordinator_destination
  coordinator_stage="${REMOTE_RUN_DIR}/coordinator-$(basename -- "$destination")"
  _mox_ssh_options "$COORDINATOR"
  coordinator_destination="$(_mox_ssh_destination "$COORDINATOR")"
  scp "${MOX_SSH_OPTIONS[@]}" -- "$source" \
    "${MOX_SSH_USER:-root}@${coordinator_destination}:${coordinator_stage}"
  node_exec "$COORDINATOR" chmod "$mode" "$coordinator_stage"
  if [[ "$STAGING_NODE" == "$COORDINATOR" ]]; then
    node_exec "$COORDINATOR" mv -- "$coordinator_stage" "$destination"
  else
    node_exec "$COORDINATOR" scp \
      -o BatchMode=yes \
      -o ClearAllForwardings=yes \
      -o StrictHostKeyChecking=yes \
      -o CheckHostIP=no \
      -o "HostKeyAlias=${STAGING_NODE}" \
      -o "UserKnownHostsFile=/etc/pve/nodes/${STAGING_NODE}/ssh_known_hosts" \
      -o GlobalKnownHostsFile=none \
      -p -- "$coordinator_stage" \
      "root@${STAGING_NODE}.${PROXMOX_INTERNAL_DOMAIN}:${destination}"
    node_exec "$COORDINATOR" rm -f -- "$coordinator_stage"
  fi
  node_exec "$STAGING_NODE" chmod "$mode" "$destination"
}

prepare_remote_patch_bundle() {
  REMOTE_RUN_DIR="/run/app-ha-staging-${STAGING_VMID}-$$"
  [[ "$REMOTE_RUN_DIR" =~ ^/run/app-ha-staging-[0-9]+-[0-9]+$ ]] ||
    die "Derived remote staging directory is unsafe"
  node_exec "$COORDINATOR" install -d -m 0700 "$REMOTE_RUN_DIR"
  [[ "$STAGING_NODE" == "$COORDINATOR" ]] ||
    node_exec "$STAGING_NODE" install -d -m 0700 "$REMOTE_RUN_DIR"

  local hash_file="${RUN_DIR}/staging-root-password.hash"
  printf '%s\n' "$ROOT_PASSWORD_HASH" >"$hash_file"
  chmod 0600 "$hash_file"
  upload_file_to_staging_node \
    "$PATCH_HELPER" "${REMOTE_RUN_DIR}/patch_staging_clone.sh" 0700
  upload_file_to_staging_node \
    "$GUEST_TREE_HELPER" "${REMOTE_RUN_DIR}/patch_staging_guest_tree.py" 0700
  upload_file_to_staging_node \
    "$hash_file" "${REMOTE_RUN_DIR}/root-password.hash" 0600
  if [[ -n "$SANITIZER_FILE" ]]; then
    upload_file_to_staging_node \
      "$SANITIZER_FILE" "${REMOTE_RUN_DIR}/sanitizer.sh" 0700
  fi
  rm -f -- "$hash_file"
}

remove_remote_patch_bundle() {
  [[ -n "$REMOTE_RUN_DIR" ]] || return 0
  if [[ -n "$COORDINATOR" ]]; then
    node_exec "$COORDINATOR" rm -rf -- "$REMOTE_RUN_DIR" >/dev/null 2>&1 || true
  fi
  if [[ -n "$STAGING_NODE" && "$STAGING_NODE" != "$COORDINATOR" ]]; then
    node_exec "$STAGING_NODE" rm -rf -- "$REMOTE_RUN_DIR" >/dev/null 2>&1 || true
  fi
  REMOTE_RUN_DIR=""
}

patch_clone_offline() {
  CURRENT_PHASE="offline-patching the staging clone"
  require_source_unchanged
  registry_update_stage \
    --state patching \
    --owner-node "$STAGING_NODE" \
    --volume-id "$STAGING_VOLUME"
  prepare_remote_patch_bundle
  local -a args=(
    "${REMOTE_RUN_DIR}/patch_staging_clone.sh"
    --device "$CLONE_PATH"
    --volume-id "$STAGING_VOLUME"
    --name "$STAGING_NAME"
    --address "${STAGING_IP}/24"
    --gateway "$GUEST_GATEWAY"
    --mac "$STAGING_MAC"
    --dns "$(IFS=,; printf '%s' "${DNS_SERVERS[*]}")"
    --root-hash-file "${REMOTE_RUN_DIR}/root-password.hash"
  )
  [[ -z "$SANITIZER_FILE" ]] ||
    args+=(--sanitizer "${REMOTE_RUN_DIR}/sanitizer.sh")
  # /run is intentionally noexec on hardened Proxmox hosts. Pass the staged
  # helper to Bash explicitly while preserving every argument as a separate
  # SSH argv element.
  node_exec "$STAGING_NODE" bash "${args[@]}" ||
    die "Offline clone patching failed; attachment and start are forbidden"
  node_exec "$STAGING_NODE" bash -c '
set -Eeuo pipefail
dataset="$1"
volume="$2"
zfs set volmode=dev "$dataset"
udevadm settle
[[ "$(zfs get -Hp -o value volmode "$dataset")" == dev ]]
path="$(pvesm path "$volume")"
[[ "$path" == "/dev/zvol/${dataset}" && -b "$path" ]]
resolved="$(readlink -f -- "$path")"
mapfile -t paths < <(lsblk -nrpo PATH "$resolved")
((${#paths[@]} == 1))
[[ "${paths[0]}" == "$resolved" ]]
' bash "$CLONE_DATASET" "$STAGING_VOLUME" ||
    die "Could not return the patched clone to partition-hidden VM mode"
  PATCH_SUCCEEDED=true
  remove_remote_patch_bundle
}

attach_patched_disk() {
  CURRENT_PHASE="attaching the verified offline-patched disk"
  require_source_unchanged
  [[ "$PATCH_SUCCEEDED" == true ]] ||
    die "Internal guard refused to attach an unpatched staging clone"
  verify_staging_vm_config unattached
  node_exec "$STAGING_NODE" qm set "$STAGING_VMID" \
    --scsi0 "${STAGING_VOLUME},discard=on,iothread=1,replicate=0,ssd=1" \
    --boot "order=scsi0"
  DISK_ATTACHED=true
  verify_staging_vm_config attached
  registry_update_stage --state stopped
}

enable_route_and_sync() {
  CURRENT_PHASE="enabling staging route and syncing HAProxy"
  require_source_unchanged
  registry_update_stage --routes-enabled
  ROUTES_ENABLED=true
  mox_ssh "$COORDINATOR" "$REMOTE_HAPROXY_SYNC" </dev/null ||
    die "HAProxy route synchronization failed"
}

offer_start() {
  COMMITTED=true
  if [[ "$START_AFTER_CREATION" != true ]]; then
    info "$STAGING_NAME remains stopped"
    return 0
  fi
  CURRENT_PHASE="starting staging VM"
  local owner
  owner="$(source_owner_now)" ||
    die "Production source disappeared before optional staging start"
  if [[ "$owner" == "$STAGING_NODE" ]]; then
    warn "Production moved to $STAGING_NODE; staging remains safely stopped."
    return 0
  fi
  local start_returned=true
  node_exec "$STAGING_NODE" qm start "$STAGING_VMID" ||
    start_returned=false
  local deadline=$((SECONDS + START_TIMEOUT_SECONDS)) status=""
  while ((SECONDS < deadline)); do
    status="$(node_exec "$STAGING_NODE" qm status "$STAGING_VMID" 2>/dev/null)" ||
      status=""
    [[ "$status" != "status: running" ]] || break
    [[ "$start_returned" == true ]] || break
    sleep 5
  done
  if [[ "$status" != "status: running" ]]; then
    node_exec "$STAGING_NODE" qm stop "$STAGING_VMID" \
      --skiplock true --timeout 30 >/dev/null 2>&1 || true
    warn "The lifecycle hook or Proxmox refused start; staging remains registered and stopped."
    return 0
  fi
  local updated
  if ! updated="$(
    registry_cmd update "$STAGING_NAME" \
      --expected-revision "$STAGING_REVISION" --state active
  )"; then
    node_exec "$STAGING_NODE" qm stop "$STAGING_VMID" \
      --skiplock true --timeout 30 >/dev/null 2>&1 || true
    warn "Registry activation failed; staging was returned to stopped."
    return 0
  fi
  parse_staging_resource "$updated"
}

setup_workstation_jump_ssh() {
  [[ "$SETUP_WORKSTATION_JUMP_SSH" == true ]] || return 0
  [[ "$STAGING_STATE" == active ]] || {
    info "Jump SSH setup skipped because $STAGING_NAME is not running"
    return 0
  }

  local command_name
  for command_name in awk install mktemp python3 ssh ssh-keygen; do
    command -v "$command_name" >/dev/null 2>&1 || {
      warn "Cannot configure jump SSH because $command_name is unavailable"
      return 0
    }
  done
  [[ -n "${HOME:-}" && "$HOME" == /* ]] || {
    warn "Cannot configure jump SSH because HOME is unavailable or not absolute"
    return 0
  }

  local ssh_dir="${HOME}/.ssh"
  local known_hosts="${ssh_dir}/known_hosts"
  local ssh_config="${ssh_dir}/config"
  [[ ! -L "$ssh_dir" && ! -L "$known_hosts" && ! -L "$ssh_config" ]] || {
    warn "Refusing jump SSH setup because ~/.ssh or a managed file is a symlink"
    return 0
  }
  install -d -m 0700 "$ssh_dir"

  local guest_key_json="" guest_key="" guest_key_core guest_fingerprint
  local deadline=$((SECONDS + 180)) waiting_announced=false
  while ((SECONDS < deadline)); do
    guest_key_json="$(
      node_exec "$STAGING_NODE" qm guest exec "$STAGING_VMID" --timeout 30 -- \
        /bin/cat /etc/ssh/ssh_host_ed25519_key.pub 2>/dev/null
    )" || guest_key_json=""
    if [[ -n "$guest_key_json" ]]; then
      guest_key="$(
        python3 - "$guest_key_json" <<'PY' 2>/dev/null || true
import json
import re
import sys
value = json.loads(sys.argv[1])
key = value.get("out-data", "").strip()
if (
    value.get("exited") != 1
    or value.get("exitcode") != 0
    or not re.fullmatch(r"ssh-ed25519 [A-Za-z0-9+/=]+(?: [^\r\n]+)?", key)
):
    raise SystemExit("QGA returned an invalid Ed25519 host key")
print(key)
PY
      )"
    fi
    [[ -z "$guest_key" ]] || break
    if [[ "$waiting_announced" == false ]]; then
      info "Waiting up to 180 seconds for QGA and the regenerated SSH host key"
      waiting_announced=true
    fi
    sleep 5
  done
  if [[ -z "$guest_key" ]]; then
    warn "Timed out waiting for QGA and a valid guest Ed25519 host key"
    return 0
  fi
  guest_key_core="$(awk '{print $1 " " $2}' <<<"$guest_key")"
  guest_fingerprint="$(
    printf '%s\n' "$guest_key_core" | ssh-keygen -lf - | awk '{print $2}'
  )" || {
    warn "Could not fingerprint the QGA-attested guest host key"
    return 0
  }
  info "QGA-attested guest Ed25519 fingerprint: $guest_fingerprint"

  local ssh_alias="$STAGING_NAME" alias_used config_used known_used
  local effective existing_keys existing_key fingerprint
  local effective_proxyjump effective_hostkeyalias
  while true; do
    config_used=false
    if [[ -f "$ssh_config" ]]; then
      if python3 - "$ssh_config" "$ssh_alias" <<'PY'
import shlex
import sys
for raw in open(sys.argv[1], encoding="utf-8"):
    line = raw.strip()
    if not line or line.startswith("#"):
        continue
    try:
        parts = shlex.split(line, comments=True)
    except ValueError:
        continue
    if parts and parts[0].lower() == "host" and sys.argv[2] in parts[1:]:
        raise SystemExit(0)
raise SystemExit(1)
PY
      then
        config_used=true
      fi
    fi
    effective="$(ssh -G "$ssh_alias" 2>/dev/null || true)"
    effective_proxyjump="$(
      awk '$1 == "proxyjump" {print $2; exit}' <<<"$effective"
    )"
    effective_hostkeyalias="$(
      awk '$1 == "hostkeyalias" {print $2; exit}' <<<"$effective"
    )"
    if ! grep -Fxq "hostname $ssh_alias" <<<"$effective" ||
      [[ -n "$effective_proxyjump" && "$effective_proxyjump" != none ]] ||
      [[ -n "$effective_hostkeyalias" ]]; then
      config_used=true
    fi
    known_used=false
    if [[ -f "$known_hosts" ]] &&
      ssh-keygen -F "$ssh_alias" -f "$known_hosts" >/dev/null 2>&1; then
      known_used=true
    fi
    alias_used=false
    [[ "$config_used" == false && "$known_used" == false ]] || alias_used=true

    existing_keys=""
    if [[ "$known_used" == true ]]; then
      existing_keys="$(
        ssh-keygen -F "$ssh_alias" -f "$known_hosts" 2>/dev/null |
          awk '$1 !~ /^#/ && $2 == "ssh-ed25519" {print $2 " " $3}'
      )"
    fi
    if grep -Fxq -- "$guest_key_core" <<<"$existing_keys" &&
      grep -Fxq "hostname $STAGING_IP" <<<"$effective" &&
      grep -Fxq "user root" <<<"$effective" &&
      grep -Fxq "proxyjump $STAGING_NODE" <<<"$effective" &&
      grep -Fxq "hostkeyalias $ssh_alias" <<<"$effective"; then
      if ssh -o BatchMode=yes -o ConnectTimeout=10 \
        -o ControlMaster=no -o ControlPath=none "$ssh_alias" true; then
        info "SSH alias $ssh_alias is already safely configured and working"
      else
        warn "Alias $ssh_alias is pinned correctly but strict SSH authentication failed"
      fi
      return 0
    fi

    if [[ "$alias_used" == true ]]; then
      warn "SSH alias $ssh_alias is already used in ~/.ssh/config or known_hosts"
      if [[ -n "$existing_keys" ]]; then
        printf 'Existing Ed25519 fingerprint(s):\n' >&2
        while IFS= read -r existing_key; do
          [[ -n "$existing_key" ]] || continue
          fingerprint="$(
            printf '%s\n' "$existing_key" | ssh-keygen -lf - | awk '{print $2}'
          )"
          printf '  %s\n' "$fingerprint" >&2
        done <<<"$existing_keys"
        printf 'QGA-attested fingerprint:\n  %s\n' "$guest_fingerprint" >&2
      fi
      if prompt_yes "Use $ssh_alias anyway and replace its effective app-ha values and host-key pin?"; then
        :
      else
        info "Jump SSH setup skipped because alias $ssh_alias is already in use"
        return 0
      fi
    fi
    break
  done

  local backup_dir="${RUN_DIR}/ssh-jump-backup"
  install -d -m 0700 "$backup_dir"
  local config_existed=false known_existed=false
  if [[ -f "$ssh_config" ]]; then
    install -m 0600 "$ssh_config" "${backup_dir}/config"
    config_existed=true
  fi
  if [[ -f "$known_hosts" ]]; then
    install -m 0600 "$known_hosts" "${backup_dir}/known_hosts"
    known_existed=true
  fi

  local staged_known_hosts="${backup_dir}/known_hosts.new"
  if [[ "$known_existed" == true ]]; then
    install -m 0600 "$known_hosts" "$staged_known_hosts"
  else
    : >"$staged_known_hosts"
    chmod 0600 "$staged_known_hosts"
  fi
  ssh-keygen -R "$ssh_alias" -f "$staged_known_hosts" >/dev/null 2>&1 || true
  printf '%s %s\n' "$ssh_alias" "$guest_key_core" >>"$staged_known_hosts"

  local staged_config="${backup_dir}/config.new"
  python3 - "$ssh_config" "$staged_config" "$ssh_alias" \
    "$STAGING_IP" "$STAGING_NODE" <<'PY'
from pathlib import Path
import sys

source, destination, alias, address, jump = sys.argv[1:]
begin = f"# BEGIN app-ha managed staging guest {alias}"
end = f"# END app-ha managed staging guest {alias}"
text = ""
path = Path(source)
if path.is_file():
    text = path.read_text(encoding="utf-8")
lines = text.splitlines()
kept = []
inside = False
for line in lines:
    if line == begin:
        inside = True
        continue
    if line == end and inside:
        inside = False
        continue
    if not inside:
        kept.append(line)
if inside:
    raise SystemExit("existing managed SSH block is unterminated")
block = [
    begin,
    f"Host {alias}",
    f"    HostName {address}",
    "    User root",
    "    Port 22",
    f"    ProxyJump {jump}",
    f"    HostKeyAlias {alias}",
    "    StrictHostKeyChecking yes",
    "    PasswordAuthentication no",
    "    KbdInteractiveAuthentication no",
    end,
    "",
]
Path(destination).write_text("\n".join(block + kept).rstrip() + "\n", encoding="utf-8")
PY
  chmod 0600 "$staged_config"

  install -m 0600 "$staged_known_hosts" "$known_hosts"
  install -m 0600 "$staged_config" "$ssh_config"

  if ! ssh -o BatchMode=yes -o ConnectTimeout=10 \
    -o ControlMaster=no -o ControlPath=none "$ssh_alias" true; then
    warn "Strict SSH validation failed; restoring the prior workstation files"
    if [[ "$known_existed" == true ]]; then
      install -m 0600 "${backup_dir}/known_hosts" "$known_hosts"
    else
      rm -f -- "$known_hosts"
    fi
    if [[ "$config_existed" == true ]]; then
      install -m 0600 "${backup_dir}/config" "$ssh_config"
    else
      rm -f -- "$ssh_config"
    fi
    return 0
  fi

  info "Configured and validated strict jump SSH alias: $ssh_alias"
  info "Connect with: ssh $ssh_alias"
}

refresh_staging_for_rollback() {
  local value
  value="$(registry_cmd get "$STAGING_NAME" 2>/dev/null)" || return 1
  parse_staging_resource "$value"
}

rollback_registry() {
  refresh_staging_for_rollback || return 0
  local -a args=(
    --routes-disabled
    --clear-owner-node
    --clear-volume
    --clear-snapshot
  )
  case "$STAGING_STATE" in
    failed | cleanup_pending) ;;
    active) args=(--state failed "${args[@]}") ;;
    reserved | snapshotting | replicating | cloning | patching | stopped | ready)
      args=(--state failed "${args[@]}")
      ;;
  esac
  local updated
  updated="$(
    registry_cmd update "$STAGING_NAME" \
      --expected-revision "$STAGING_REVISION" "${args[@]}" 2>/dev/null
  )" || return 1
  parse_staging_resource "$updated"
}

rollback_vm_identity_matches() {
  local config purpose_tag
  config="$(node_exec "$STAGING_NODE" qm config "$STAGING_VMID" 2>/dev/null)" ||
    return 1
  purpose_tag="${PURPOSE_TAG_PREFIX}${SOURCE_PURPOSE}"
  python3 - "$config" "$STAGING_NAME" \
    "$STAGING_VM_TAG" "$EVICTABLE_VM_TAG" "$purpose_tag" \
    "$STAGING_VOLUME" "$STAGING_VM_STORAGE" "$STAGING_VMID" <<'PY'
import re
import sys
config = {}
for line in sys.argv[1].splitlines():
    if ": " in line:
        key, value = line.split(": ", 1)
        config[key] = value
tags = {value for value in re.split(r"[;,]", config.get("tags", "")) if value}
expected = set(sys.argv[3:6])
volume, storage, raw_vmid = sys.argv[6:9]
vmid = int(raw_vmid)
disk_keys = {
    key
    for key in config
    if re.fullmatch(
        r"(?:(?:scsi|sata|virtio|ide|unused|efidisk|tpmstate)[0-9]+)",
        key,
    )
}
safe_disks = disk_keys in (set(), {"efidisk0"}, {"scsi0", "efidisk0"})
root_safe = (
    "scsi0" not in disk_keys
    or config["scsi0"].split(",", 1)[0] == volume
)
efi_safe = "efidisk0" not in disk_keys or (
    re.fullmatch(
        rf"{re.escape(storage)}:vm-{vmid}-disk-[1-9][0-9]*",
        config["efidisk0"].split(",", 1)[0],
    )
    is not None
)
raise SystemExit(
    0
    if config.get("name") == sys.argv[2]
    and tags == expected
    and config.get("onboot", "0") == "0"
    and safe_disks
    and root_safe
    and efi_safe
    else 1
)
PY
}

cluster_volume_is_unreferenced() {
  local volume="$1" rows inventory node vmid config
  rows="$(pvesh_get /cluster/resources --type vm)" || return 1
  inventory="$(
    python3 - "$rows" <<'PY'
import json
import sys
rows = json.loads(sys.argv[1])
if not isinstance(rows, list):
    raise SystemExit("malformed cluster VM inventory")
for row in rows:
    if (
        row.get("type") == "qemu"
        and row.get("node")
        and str(row.get("vmid", "")).isdigit()
    ):
        print(f"{row['node']}\t{row['vmid']}")
PY
  )" || return 1
  while IFS=$'\t' read -r node vmid; do
    [[ -n "$node" && -n "$vmid" ]] || continue
    config="$(pvesh_get "/nodes/${node}/qemu/${vmid}/config")" || return 1
    python3 - "$volume" "$config" <<'PY' || return 1
import json
import re
import sys

volume = sys.argv[1]
config = json.loads(sys.argv[2])
for key, value in config.items():
    if re.fullmatch(
        r"(?:(?:scsi|sata|virtio|ide|unused|efidisk|tpmstate)[0-9]+)",
        key,
    ) and str(value).split(",", 1)[0] == volume:
        raise SystemExit(1)
PY
  done <<<"$inventory"
}

destroy_staging_vm_and_clone() {
  local ok=true live_presence=""
  if [[ "$VM_MAY_EXIST" == true && -n "$STAGING_VMID" && -n "$STAGING_NODE" ]]; then
    if node_exec "$STAGING_NODE" qm status "$STAGING_VMID" >/dev/null 2>&1; then
      if ! rollback_vm_identity_matches; then
        warn "Rollback refused to destroy VMID $STAGING_VMID because its identity changed"
        ok=false
      else
        node_exec "$STAGING_NODE" qm stop "$STAGING_VMID" \
          --skiplock true --timeout 30 >/dev/null 2>&1 || true
        if ! node_exec "$STAGING_NODE" qm destroy "$STAGING_VMID" \
          --purge 1 --destroy-unreferenced-disks 0 >/dev/null 2>&1; then
          warn "Rollback could not destroy staging VM $STAGING_VMID"
          ok=false
        fi
      fi
    fi
  fi
  if [[ "$VM_MAY_EXIST" == true && -n "$STAGING_VMID" ]]; then
    live_presence="$(
      pvesh_get /cluster/resources --type vm 2>/dev/null |
        python3 -c '
import json
import sys
wanted = int(sys.argv[1])
rows = json.load(sys.stdin)
print(
    "present"
    if any(row.get("vmid") in (wanted, str(wanted)) for row in rows)
    else "absent"
)
' "$STAGING_VMID"
    )" || live_presence=""
    if [[ "$live_presence" != absent ]]; then
      warn "Rollback could not prove staging VMID $STAGING_VMID is absent"
      ok=false
    fi
  fi
  if [[ "$CLONE_MAY_EXIST" == true && -n "$CLONE_DATASET" &&
    -n "$STAGING_NODE" && "$live_presence" == absent ]]; then
    local clone_presence=""
    clone_presence="$(
      node_exec "$STAGING_NODE" bash -c '
if zfs list -H -o name "$1" >/dev/null 2>&1; then
  printf "present\n"
else
  printf "absent\n"
fi
' -- "$CLONE_DATASET" 2>/dev/null
    )" || clone_presence=""
    if [[ "$clone_presence" == present ]]; then
      local origin
      origin="$(
        node_exec "$STAGING_NODE" zfs get -Hp -o value origin "$CLONE_DATASET" \
          2>/dev/null
      )" || origin=""
      if [[ "$origin" != "${SOURCE_DATASET}@${SNAPSHOT_NAME}" ]]; then
        warn "Rollback refused to destroy clone dataset with an unexpected origin"
        ok=false
      elif ! cluster_volume_is_unreferenced "$STAGING_VOLUME"; then
        warn "Rollback retained clone because a VM reference could not be excluded"
        ok=false
      elif ! node_exec "$STAGING_NODE" zfs destroy "$CLONE_DATASET" \
        >/dev/null 2>&1; then
        warn "Rollback could not destroy clone dataset $CLONE_DATASET"
        ok=false
      else
        clone_presence="$(
          node_exec "$STAGING_NODE" bash -c '
if zfs list -H -o name "$1" >/dev/null 2>&1; then
  printf "present\n"
else
  printf "absent\n"
fi
' -- "$CLONE_DATASET" 2>/dev/null
        )" || clone_presence=""
        if [[ "$clone_presence" != absent ]]; then
          warn "Rollback could not verify clone dataset destruction"
          ok=false
        fi
      fi
    elif [[ "$clone_presence" != absent ]]; then
      warn "Rollback could not prove whether clone dataset exists"
      ok=false
    fi
  fi
  [[ "$ok" == true ]]
}

delete_snapshot_and_trigger_cleanup() {
  [[ "$SNAPSHOT_MAY_EXIST" == true && -n "$SNAPSHOT_NAME" ]] || return 0
  local owner ok=true
  owner="$(source_owner_now 2>/dev/null)" || {
    warn "Rollback could not locate the current production owner"
    return 1
  }
  local presence
  presence="$(snapshot_presence "$owner")" || {
    warn "Rollback could not inspect Proxmox VM snapshots"
    return 1
  }
  if [[ "$presence" == present ]]; then
    if ! node_exec "$owner" qm delsnapshot "$SOURCE_VMID" "$SNAPSHOT_NAME" \
      >/dev/null 2>&1; then
      warn "Rollback could not delete Proxmox snapshot $SNAPSHOT_NAME"
      return 1
    fi
    presence="$(snapshot_presence "$owner")" || {
      warn "Rollback could not verify Proxmox snapshot deletion"
      return 1
    }
    if [[ "$presence" != absent ]]; then
      warn "Rollback snapshot remains in the production VM configuration"
      return 1
    fi
  elif [[ "$presence" != absent ]]; then
    warn "Rollback received an invalid production snapshot query"
    return 1
  fi
  local minimum_sync=$(( $(date +%s) - 5 )) job
  for job in "${REPLICATION_JOBS[@]}"; do
    if ! node_exec "$owner" pvesr schedule-now "$job" >/dev/null 2>&1; then
      warn "Rollback could not trigger replication cleanup job $job"
      ok=false
    fi
  done
  local deadline=$((SECONDS + ROLLBACK_REPLICATION_TIMEOUT_SECONDS))
  local status target
  for job in "${REPLICATION_JOBS[@]}"; do
    target=""
    while ((SECONDS < deadline)); do
      status="$(pvesh_get \
        "/nodes/${owner}/replication/${job}/status" 2>/dev/null)" || {
        sleep 5
        continue
      }
      target="$(replication_status_target "$status" "$job" "$minimum_sync" \
        2>/dev/null)" || target=""
      [[ -z "$target" ]] || break
      sleep 5
    done
    if [[ -z "$target" ]]; then
      warn "Rollback could not verify replication cleanup job $job"
      ok=false
    fi
  done

  local dataset node volume snapshot_copy_presence
  for volume in "${SOURCE_VOLUMES[@]}"; do
    for node in "${SOURCE_PLACEMENT[@]}"; do
      dataset="$(volume_dataset_on_node "$node" "$volume" 2>/dev/null)" ||
        dataset=""
      if [[ -z "$dataset" ]]; then
        warn "Rollback could not resolve $volume on $node"
        ok=false
        continue
      fi
      snapshot_copy_presence="$(
        node_exec "$node" bash -c '
if zfs list -H -o name "$1" >/dev/null 2>&1; then
  printf "present\n"
else
  printf "absent\n"
fi
' -- "${dataset}@${SNAPSHOT_NAME}" 2>/dev/null
      )" || snapshot_copy_presence=""
      if [[ "$snapshot_copy_presence" != absent ]]; then
        warn "Rollback could not prove $volume snapshot cleanup on $node"
        ok=false
      fi
    done
  done
  [[ "$ok" == true ]]
}

is_configured_cluster_node() {
  local wanted="$1" node
  for node in "${CLUSTER_NODES[@]}"; do
    [[ "$node" != "$wanted" ]] || return 0
  done
  return 1
}

record_deferred_rollback_cleanup() {
  local node
  [[ "$REGISTRY_RESERVED" == true ]] || return 0
  if [[ "$VM_MAY_EXIST" == true && -n "$STAGING_NODE" ]]; then
    registry_cmd defer-cleanup \
      --resource "$STAGING_NAME" \
      --node "$STAGING_NODE" \
      --action destroy-vm \
      --target "vm:${STAGING_VMID}" \
      --reason "staging creation rollback could not prove VM destruction" \
      >/dev/null 2>&1 || true
  fi
  if [[ "$CLONE_MAY_EXIST" == true && -n "$STAGING_NODE" ]]; then
    registry_cmd defer-cleanup \
      --resource "$STAGING_NAME" \
      --node "$STAGING_NODE" \
      --action destroy-volume \
      --target "$STAGING_VOLUME" \
      --reason "staging creation rollback could not prove clone destruction" \
      >/dev/null 2>&1 || true
  fi
  if [[ "$SNAPSHOT_MAY_EXIST" == true && -n "$SNAPSHOT_NAME" ]]; then
    for node in "${SOURCE_PLACEMENT[@]}"; do
      if ! is_configured_cluster_node "$node"; then
        warn "Skipping stale snapshot cleanup node absent from pvesh /nodes: $node"
        continue
      fi
      registry_cmd defer-cleanup \
        --resource "$STAGING_NAME" \
        --node "$node" \
        --action delete-snapshot \
        --target "vm-${SOURCE_VMID}@${SNAPSHOT_NAME}" \
        --reason "staging rollback retained an unverified production snapshot dependency" \
        >/dev/null 2>&1 || true
    done
  fi
  record_deferred_route_cleanup
}

record_deferred_route_cleanup() {
  local node
  [[ "$REGISTRY_RESERVED" == true ]] || return 0
  ((${#CLUSTER_NODES[@]} > 0)) || {
    warn "No validated configured nodes are available for route cleanup"
    return 1
  }
  for node in "${CLUSTER_NODES[@]}"; do
    registry_cmd defer-cleanup \
      --resource "$STAGING_NAME" \
      --node "$node" \
      --action remove-route \
      --target "$STAGING_NAME" \
      --reason "staging rollback requires converged HAProxy route removal" \
      >/dev/null 2>&1 || true
  done
}

rollback_failed_creation() {
  [[ "$ROLLBACK_RUNNING" == false ]] || return 0
  ROLLBACK_RUNNING=true
  set +e
  set +x
  warn "Creation failed during: $CURRENT_PHASE"
  remove_remote_patch_bundle
  if [[ "$REGISTRY_RESERVED" == false &&
    "$SNAPSHOT_MAY_EXIST" == false &&
    "$CLONE_MAY_EXIST" == false &&
    "$VM_MAY_EXIST" == false &&
    "$ROUTES_ENABLED" == false ]]; then
    warn "No staging resources had been allocated; no rollback actions were needed."
    return 0
  fi
  warn "Rolling back newly-created staging resources; production is never started or modified beyond its staging snapshot."
  local storage_ok=true snapshot_ok=true registry_ok=true route_ok=true
  destroy_staging_vm_and_clone || storage_ok=false
  if [[ "$storage_ok" == true ]]; then
    delete_snapshot_and_trigger_cleanup || snapshot_ok=false
  else
    snapshot_ok=false
    warn "Production snapshot was retained because clone/VM destruction was not proven"
  fi
  if [[ "$REGISTRY_RESERVED" == true ]]; then
    if [[ "$storage_ok" == true && "$snapshot_ok" == true ]]; then
      rollback_registry || registry_ok=false
    else
      registry_ok=false
      record_deferred_rollback_cleanup
      refresh_staging_for_rollback || true
      registry_cmd update "$STAGING_NAME" \
        --expected-revision "$STAGING_REVISION" \
        --state cleanup_pending --routes-disabled >/dev/null 2>&1 || true
    fi
  fi
  if [[ "$storage_ok" == true && "$snapshot_ok" == true &&
    "$registry_ok" != true && "$REGISTRY_RESERVED" == true ]]; then
    record_deferred_rollback_cleanup
    refresh_staging_for_rollback || true
    registry_cmd update "$STAGING_NAME" \
      --expected-revision "$STAGING_REVISION" \
      --state cleanup_pending --routes-disabled >/dev/null 2>&1 || true
  fi
  if [[ "$ROUTES_ENABLED" == true && "$storage_ok" == true &&
    "$snapshot_ok" == true && "$registry_ok" == true ]]; then
    if ! mox_ssh "$COORDINATOR" "$REMOTE_HAPROXY_SYNC" \
      </dev/null >/dev/null 2>&1; then
      warn "Rollback could not converge HAProxy route removal"
      route_ok=false
      record_deferred_route_cleanup
      refresh_staging_for_rollback || true
      registry_cmd update "$STAGING_NAME" \
        --expected-revision "$STAGING_REVISION" \
        --state cleanup_pending --routes-disabled >/dev/null 2>&1 || true
    fi
  fi
  if [[ "$storage_ok" == true && "$snapshot_ok" == true &&
    "$registry_ok" == true && "$route_ok" == true &&
    "$REGISTRY_RESERVED" == true ]]; then
    if ! registry_cmd release "$STAGING_NAME" >/dev/null 2>&1; then
      registry_ok=false
      record_deferred_route_cleanup
      refresh_staging_for_rollback || true
      registry_cmd update "$STAGING_NAME" \
        --expected-revision "$STAGING_REVISION" \
        --state cleanup_pending --routes-disabled >/dev/null 2>&1 || true
    fi
  fi
  if [[ "$storage_ok" != true || "$snapshot_ok" != true ||
    "$registry_ok" != true || "$route_ok" != true ]]; then
    warn "Rollback was incomplete. The registry record was retained when needed for cleanup."
  else
    warn "Rollback removed the VM, clone, source snapshot, and registry allocation."
  fi
}

cleanup() {
  local code=$?
  trap - EXIT HUP INT TERM
  set +e
  set +x
  if ((code != 0)) && [[ "$COMMITTED" == false && "$DRY_RUN" == false ]]; then
    rollback_failed_creation
  fi
  remove_remote_patch_bundle
  [[ -z "$RUN_DIR" ]] || rm -rf -- "$RUN_DIR"
  ROOT_PASSWORD_HASH=""
  unset ROOT_PASSWORD_HASH
  exit "$code"
}

on_signal() {
  local signal="$1" code=1
  case "$signal" in
    HUP) code=129 ;;
    INT) code=130 ;;
    TERM) code=143 ;;
  esac
  printf '\nERROR: received %s during %s\n' "$signal" "$CURRENT_PHASE" >&2
  exit "$code"
}

install_traps() {
  trap cleanup EXIT
  trap 'on_signal HUP' HUP
  trap 'on_signal INT' INT
  trap 'on_signal TERM' TERM
}

print_completion() {
  local status
  status="$(node_exec "$STAGING_NODE" qm status "$STAGING_VMID")"
  log "Staging VM creation complete"
  info "Resource: $STAGING_NAME (VMID $STAGING_VMID)"
  info "Source snapshot: ${SOURCE_NAME}@${SNAPSHOT_NAME}"
  info "Tracked snapshot volumes: ${SOURCE_VOLUMES[*]}"
  info "Root snapshot GUID: $SNAPSHOT_GUID on ${SOURCE_PLACEMENT[*]}"
  info "Linked clone: $STAGING_NODE / $STAGING_VOLUME"
  info "LAN name: lan0"
  info "Private IP: ${STAGING_IP}/24"
  info "FQDN: $STAGING_DOMAIN"
  info "URL: https://${STAGING_DOMAIN}"
  info "Compute: ${STAGING_VM_CORES} vCPU / $((STAGING_VM_MEMORY_MB / 1024)) GiB RAM"
  info "NIC link-down: $NETWORK_LINK_DOWN"
  info "Status: ${status#status: }"
  if [[ -z "$SANITIZER_FILE" ]]; then
    warn "No application sanitizer was injected; review production-derived data, credentials, and jobs before use."
  fi
  setup_workstation_jump_ssh
}

main() {
  parse_args "$@"
  require_command_local openssl
  require_openssl_sha512_crypt
  load_and_validate_config
  preflight_workstation
  install_traps
  select_coordinator
  collect_request
  if [[ "$DRY_RUN" == true ]]; then
    dry_run_summary
    return 0
  fi
  reserve_staging
  preflight_reserved_identity
  create_source_snapshot
  trigger_and_wait_for_replication
  verify_snapshot_guids
  clone_snapshot_locally
  create_unattached_vm
  patch_clone_offline
  attach_patched_disk
  enable_route_and_sync
  offer_start
  print_completion
}

if [[ "${APP_HA_STAGING_VM_SOURCE_ONLY:-0}" != 1 ]]; then
  main "$@"
fi
