#!/usr/bin/env bash

# Copyright (c) 2026 BEENTHERE VENTURES, INC.
# SPDX-License-Identifier: GPL-3.0-only

# Create or resume one registry-backed production VM from a workstation.
# Live Proxmox mutations deliberately use the documented PVE 9 qm, pvesr,
# pvesh, and ha-manager CLIs.  The script does not destroy a VM or release its
# registry allocation on failure: rerunning with the same purpose resumes it.

set -Eeuo pipefail
set +x
umask 077

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
REPO_ROOT="$(cd -- "${SCRIPT_DIR}/../.." && pwd -P)"
CONFIG_LIB="${REPO_ROOT}/lib/config.sh"
REMOTE_INSTALL_ROOT="/usr/local/lib/app-ha-proxmox"
REMOTE_REGISTRY="${REMOTE_INSTALL_ROOT}/lib/cluster_registry.py"
REMOTE_HAPROXY_SYNC="${REMOTE_INSTALL_ROOT}/lib/sync_haproxy_routes.sh"
REMOTE_ISO_PREPARER="${REMOTE_INSTALL_ROOT}/guests/prod/prepare_prod_iso.sh"
REMOTE_ISO_CACHE_ROOT="/var/lib/app-ha-proxmox/iso-cache"

DRY_RUN=false
INSTALL_TIMEOUT_SECONDS=10800
QGA_TIMEOUT_SECONDS=900
REPLICATION_TIMEOUT_SECONDS=14400

CURRENT_PHASE="startup"
COORDINATOR=""
RESOURCE_NAME=""
RESOURCE_JSON=""
RESOURCE_REVISION=""
RESOURCE_STATE=""
NEW_RESOURCE=false
VMID=""
OWNER_NODE=""
REGISTERED_OWNER_NODE=""
ROOT_VOLUME=""
REGISTERED_ROOT_VOLUME=""
ROOT_PASSWORD_HASH=""
INSTALL_PHASE="unknown"
FINAL_NETWORK_ENABLED=true
STORED_FINAL_NETWORK_ENABLED=""
STARTUP_SCRIPT_PATH=""
STARTUP_SCRIPT_CONTENT=""
STARTUP_SHA256="none"
STORED_STARTUP_SHA256=""
ORCHESTRATION_NONCE=""
ORCHESTRATION_LEASE_ACQUIRED=false
RUN_DIR=""
REMOTE_ISO_NODE=""
REMOTE_ISO_PATH=""
REMOTE_ISO_REQUEST=""
COORDINATOR_REQUEST_STAGE=""
REMOTE_ISO_UPLOADED=false
REMOTE_ISO_ATTACHED=false
REMOTE_ISO_SAFE_TO_DELETE=false
SOURCE_ISO_CACHE_PATH=""
SOURCE_ISO_SHA256_PATH=""
SOURCE_ISO_CACHE_REUSED=false
PROD_GUEST_OS_INSTALL_MODE=""

declare -a ONLINE_NODES=()
declare -a PLACEMENT_NODES=()
declare -a ALIAS_DOMAINS=()
declare -a DNS_SERVERS=()
declare -a REPLICATION_TARGETS=()

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
  if [[ -n "$RESOURCE_NAME" ]]; then
    printf 'Registry allocation %s was preserved for a safe rerun.\n' \
      "$RESOURCE_NAME" >&2
  fi
  exit 1
}

usage() {
  cat <<'EOF'
Usage: create_prod_vm.sh [--dry-run] [timeout options]

Run from an administrator workstation. The script loads cluster.conf and the
mode-0600 secrets.env through lib/config.sh, chooses the first
reachable mox coordinator, prompts for placement and VM policy, and resumes an
existing allocation when the requested purpose is already registered.

Options:
  --dry-run                         Query and validate live state, prompt for
                                    a new request, and stop before allocation
                                    or any Proxmox mutation.
  --install-timeout-seconds N       Default: 10800
  --qga-timeout-seconds N           Default: 900
  --replication-timeout-seconds N   Default: 14400
  -h, --help
EOF
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

validate_positive_integer() {
  [[ "$2" =~ ^[1-9][0-9]*$ ]] ||
    die "$1 must be a positive integer"
}

validate_safe_id() {
  [[ "$2" =~ ^[A-Za-z0-9][A-Za-z0-9_.:+/@-]*$ ]] ||
    die "$1 contains unsafe characters: $2"
}

parse_args() {
  while (($#)); do
    case "$1" in
      --dry-run)
        DRY_RUN=true
        shift
        ;;
      --install-timeout-seconds | --qga-timeout-seconds | \
        --replication-timeout-seconds)
        (($# >= 2)) || die "$1 requires a value"
        validate_positive_integer "$1" "$2"
        case "$1" in
          --install-timeout-seconds) INSTALL_TIMEOUT_SECONDS="$2" ;;
          --qga-timeout-seconds) QGA_TIMEOUT_SECONDS="$2" ;;
          --replication-timeout-seconds) REPLICATION_TIMEOUT_SECONDS="$2" ;;
        esac
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
  local destination="$1" prompt="$2" default="$3" entered
  IFS= read -r -p "${prompt} [${default}]: " entered
  entered="${entered:-$default}"
  case "${entered,,}" in
    y | yes) printf -v "$destination" '%s' true ;;
    n | no) printf -v "$destination" '%s' false ;;
    *) die "Answer yes or no" ;;
  esac
}

confirm_exact_local() {
  local prompt="$1" _legacy_phrase="${2:-}" entered
  printf '%s\nType GO to continue.\n> ' "$prompt"
  IFS= read -r entered
  [[ "$entered" == GO ]] ||
    die "Confirmation did not match GO; no further action was taken"
}

cleanup() {
  local code=$?
  trap - EXIT
  set +e
  set +x

  if [[ -n "$REMOTE_ISO_REQUEST" && -n "$REMOTE_ISO_NODE" ]]; then
    node_exec "$REMOTE_ISO_NODE" rm -f -- "$REMOTE_ISO_REQUEST" \
      >/dev/null 2>&1
  fi
  if [[ -n "$COORDINATOR_REQUEST_STAGE" && -n "$COORDINATOR" ]]; then
    mox_ssh "$COORDINATOR" rm -f -- "$COORDINATOR_REQUEST_STAGE" \
      >/dev/null 2>&1
  fi

  if [[ "$REMOTE_ISO_UPLOADED" == true &&
    "$REMOTE_ISO_SAFE_TO_DELETE" == true &&
    "$REMOTE_ISO_ATTACHED" == false &&
    -n "$REMOTE_ISO_NODE" && -n "$REMOTE_ISO_PATH" ]]; then
    node_exec "$REMOTE_ISO_NODE" rm -f -- "$REMOTE_ISO_PATH" \
      "${REMOTE_ISO_PATH}.partial" >/dev/null 2>&1
  fi

  if [[ "$ORCHESTRATION_LEASE_ACQUIRED" == true &&
    -n "$ORCHESTRATION_NONCE" && -n "$RESOURCE_NAME" &&
    -n "$COORDINATOR" ]]; then
    registry_cmd orchestration-release "$RESOURCE_NAME" \
      --nonce "$ORCHESTRATION_NONCE" >/dev/null 2>&1
    ORCHESTRATION_LEASE_ACQUIRED=false
  fi

  [[ -z "$RUN_DIR" ]] || rm -rf -- "$RUN_DIR"
  ROOT_PASSWORD_HASH=""
  unset ROOT_PASSWORD_HASH
  STARTUP_SCRIPT_CONTENT=""
  unset STARTUP_SCRIPT_CONTENT
  ORCHESTRATION_NONCE=""
  unset ORCHESTRATION_NONCE
  exit "$code"
}

on_error() {
  local code=$?
  trap - ERR
  printf '\nERROR: production VM creation failed during phase %q near line %s (exit %s).\n' \
    "$CURRENT_PHASE" "${BASH_LINENO[0]}" "$code" >&2
  if [[ -n "$RESOURCE_NAME" ]]; then
    printf 'No VM, HA resource, replica, or registry allocation was automatically destroyed.\n' >&2
    printf 'Correct the transient fault and rerun with purpose %s to resume %s.\n' \
      "$PURPOSE_SLUG" "$RESOURCE_NAME" >&2
  fi
  exit "$code"
}

on_signal() {
  local signal="$1" code=1
  trap - HUP INT TERM
  case "$signal" in
    HUP) code=129 ;;
    INT) code=130 ;;
    TERM) code=143 ;;
  esac
  printf '\nERROR: received %s during %s; resumable state was preserved.\n' \
    "$signal" "$CURRENT_PHASE" >&2
  exit "$code"
}

install_traps() {
  trap cleanup EXIT
  trap on_error ERR
  trap 'on_signal HUP' HUP
  trap 'on_signal INT' INT
  trap 'on_signal TERM' TERM
}

# Execute an argv array on a selected node. The locally generated shell is
# shell-quoted one argument at a time and sent on stdin, avoiding nested-SSH
# command-string injection. Non-coordinator nodes are reached from the chosen
# coordinator over Proxmox's root cluster SSH trust.
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
with open(sys.argv[1], encoding="utf-8") as stream:
    json.load(stream)
PY
}

parse_online_nodes_json() {
  local path="$1" maximum="$2"
  python3 - "$path" "$maximum" <<'PY'
import json
import re
import sys

with open(sys.argv[1], encoding="utf-8") as stream:
    rows = json.load(stream)
maximum = int(sys.argv[2])
if not isinstance(rows, list):
    raise SystemExit("pvesh /nodes did not return an array")

seen = set()
online = []
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
    if str(row.get("status", "")).lower() == "online" or row.get("online") in (1, True):
        online.append(node)

online.sort(key=lambda value: int(value[3:]))
expected = [f"mox{index}" for index in range(1, len(online) + 1)]
if online != expected:
    raise SystemExit(
        "online mox nodes are not contiguous from mox1: "
        + (", ".join(online) if online else "none")
    )
if len(online) < 2:
    raise SystemExit("production HA requires at least two online contiguous mox nodes")
print(*online, sep="\n")
PY
}

display_node_purposes() {
  local nodes_file="$1" resources_file="$2"
  python3 - "$nodes_file" "$resources_file" "$PURPOSE_TAG_PREFIX" <<'PY'
import json
import sys

with open(sys.argv[1], encoding="utf-8") as stream:
    nodes = json.load(stream)
with open(sys.argv[2], encoding="utf-8") as stream:
    resources = json.load(stream)
prefix = sys.argv[3]

online = {
    row.get("node", row.get("name"))
    for row in nodes
    if str(row.get("status", "")).lower() == "online" or row.get("online") in (1, True)
}
for node in sorted(online, key=lambda value: int(value[3:])):
    labels = []
    for resource in resources:
        if node not in resource.get("placement", []):
            continue
        slug = resource.get("purpose", {}).get("slug", "?")
        labels.append(
            f"{prefix}{slug} "
            f"({resource.get('name')}:{resource.get('kind')}:{resource.get('state')})"
        )
    print(f"{node}\t{', '.join(sorted(labels)) if labels else '(no registered purposes)'}")
PY
}

validate_placement_csv() {
  local value="$1"
  python3 - "$value" "${ONLINE_NODES[@]}" <<'PY'
import re
import sys

requested = [part.strip() for part in sys.argv[1].split(",") if part.strip()]
online = sys.argv[2:]
if len(requested) < 2:
    raise SystemExit("select at least two placement nodes for HA")
if len(requested) != len(set(requested)):
    raise SystemExit("placement contains duplicate nodes")
if any(not re.fullmatch(r"mox([1-9]|10)", node) for node in requested):
    raise SystemExit("placement nodes must use moxN names")
missing = [node for node in requested if node not in online]
if missing:
    raise SystemExit("placement includes non-online nodes: " + ", ".join(missing))
print(",".join(requested))
PY
}

validate_prod_host_capacity() {
  CURRENT_PHASE="checking production host capacity"
  local node limit count
  for node in "${PLACEMENT_NODES[@]}"; do
    limit="$(
      bash -c '
set -Eeuo pipefail
source "$1"
load_proxmox_config --host "$2" --no-secrets >/dev/null
printf "%s\n" "$MAX_PROD_VM_COUNT_ON_THIS_HOST"
' bash "$CONFIG_LIB" "$node"
    )" || die "Could not load the production limit for $node"
    validate_positive_integer \
      "MAX_PROD_VM_COUNT_ON_THIS_HOST for $node" "$limit"
    count="$(
      python3 - "${RUN_DIR}/resources.json" "$node" <<'PY'
import json
import sys

with open(sys.argv[1], encoding="utf-8") as stream:
    resources = json.load(stream)
node = sys.argv[2]
print(sum(
    1
    for resource in resources
    if resource.get("kind") == "production"
    and node in resource.get("placement", [])
))
PY
    )" || die "Could not determine production capacity on $node"
    [[ "$count" =~ ^[0-9]+$ ]] ||
      die "Production host count is invalid for $node"
    ((count < limit)) ||
      die "$node already has $count registered production guests (limit $limit)"
    info "Production capacity on $node: $count/$limit used"
  done
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
    raise SystemExit("domain must be a fully-qualified hostname")
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

split_and_normalize_aliases() {
  local raw="$1" part normalized
  ALIAS_DOMAINS=()
  [[ -n "${raw//[[:space:],]/}" ]] || return 0
  IFS=',' read -r -a _alias_parts <<<"$raw"
  for part in "${_alias_parts[@]}"; do
    normalized="$(normalize_domain "$part")" ||
      die "Invalid alias domain: $part"
    [[ "$normalized" != "$PRIMARY_DOMAIN" ]] ||
      die "Primary domain may not also be an alias"
    local existing
    for existing in "${ALIAS_DOMAINS[@]}"; do
      [[ "$existing" != "$normalized" ]] ||
        die "Duplicate alias domain: $normalized"
    done
    ALIAS_DOMAINS+=("$normalized")
  done
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
    raise SystemExit("at least one GUEST_DNS_SERVERS address is required")
seen = set()
for value in values:
    try:
        normalized = str(ipaddress.ip_address(value))
    except ValueError:
        raise SystemExit(f"invalid DNS server: {value}")
    if normalized in seen:
        raise SystemExit(f"duplicate DNS server: {normalized}")
    seen.add(normalized)
    print(normalized)
PY
  then
    rm -f "$parsed"
    die "Could not parse GUEST_DNS_SERVERS"
  fi
  mapfile -t DNS_SERVERS <"$parsed"
  rm -f "$parsed"
  ((${#DNS_SERVERS[@]} > 0)) || die "At least one guest DNS server is required"
}

load_and_validate_config() {
  CURRENT_PHASE="loading layered configuration"
  require_regular_file "$CONFIG_LIB"
  # shellcheck source=../../lib/config.sh
  source "$CONFIG_LIB"
  load_proxmox_config --require-secrets ||
    die "Could not load cluster.conf and secrets.env"

  local required=(
    PROXMOX_CLUSTER_NAME MAX_MOX_HOSTS PRIVATE_SUBNET_CIDR GUEST_EGRESS_VIP
    PRODUCTION_IP_START PRODUCTION_IP_END
    PROD_GUEST_OS_ISO_URL PROD_GUEST_OS_ISO_SHA256
    ADMIN_1_PUBLIC_SSH_KEY ADMIN_2_PUBLIC_SSH_KEY
    PROD_GUEST_VM_ROOT_PASSWORD PRODUCTION_VM_TAG PURPOSE_TAG_PREFIX
    GUEST_ROLE_HOOK_PATH CLUSTER_STATE_DIR
    PROD_VM_CORES PROD_VM_MEMORY_GIB PROD_VM_DISK_GB PROD_VM_STORAGE
    PROD_VM_BRIDGE PROD_VM_CPU_TYPE PROD_VM_REPLICATION_INTERVAL
    ISO_STORAGE_ID SNIPPETS_STORAGE_ID
  )
  local name
  for name in "${required[@]}"; do
    require_var "$name" || die "Configuration is incomplete"
  done

  [[ "$PROD_VM_STORAGE" == local-zfs ]] ||
    die "PROD_VM_STORAGE must be local-zfs for replicated production guests"
  validate_safe_id "PROD_VM_STORAGE" "$PROD_VM_STORAGE"
  validate_safe_id "ISO_STORAGE_ID" "$ISO_STORAGE_ID"
  validate_safe_id "SNIPPETS_STORAGE_ID" "$SNIPPETS_STORAGE_ID"
  validate_safe_id "PROD_VM_BRIDGE" "$PROD_VM_BRIDGE"
  validate_safe_id "PROD_VM_CPU_TYPE" "$PROD_VM_CPU_TYPE"
  validate_safe_id "PRODUCTION_VM_TAG" "$PRODUCTION_VM_TAG"
  [[ "$PURPOSE_TAG_PREFIX" =~ ^[A-Za-z][A-Za-z0-9_.-]*$ ]] ||
    die "PURPOSE_TAG_PREFIX is not a safe Proxmox tag prefix"
  [[ "$CLUSTER_STATE_DIR" =~ ^/[A-Za-z0-9._/-]+$ ]] ||
    die "CLUSTER_STATE_DIR must be a safe absolute path"
  validate_safe_id "PROXMOX_CLUSTER_NAME" "$PROXMOX_CLUSTER_NAME"
  PROD_GUEST_OS_INSTALL_MODE="${PROD_GUEST_OS_INSTALL_MODE:-ubuntu-autoinstall}"
  SOURCE_ISO_CACHE_PATH="${REMOTE_ISO_CACHE_ROOT}/${PROD_GUEST_OS_ISO_SHA256,,}.iso"
  SOURCE_ISO_SHA256_PATH="${SOURCE_ISO_CACHE_PATH}.sha256"

  GUEST_GATEWAY="${GUEST_EGRESS_VIP%/*}"
  split_dns_servers "${GUEST_DNS_SERVERS:-${PROXMOX_DNS_SERVER:-}}"

  DEFAULT_REPLICATION_MINUTES="${PROD_VM_REPLICATION_INTERVAL##*/}"
  validate_positive_integer "PROD_VM_REPLICATION_INTERVAL" \
    "$DEFAULT_REPLICATION_MINUTES"

  # Keep the plaintext only in this shell long enough to hash it. Unset the
  # entire exported secret layer before any SSH, Python, or ISO-building child.
  # A stable cluster-scoped crypt salt makes QGA's installed-hash attestation
  # resume-safe; the configured root password is already shared by these VMs.
  local plaintext_password="$PROD_GUEST_VM_ROOT_PASSWORD" password_salt
  for name in "${_PROXMOX_CONFIG_SECRET_KEYS[@]}"; do
    unset "$name"
  done
  password_salt="$(
    printf 'app-ha-prod-root-v1:%s' "$PROXMOX_CLUSTER_NAME" |
      sha256sum | awk '{print substr($1, 1, 16)}'
  )"
  ROOT_PASSWORD_HASH="$(
    printf '%s' "$plaintext_password" |
      openssl passwd -6 -salt "$password_salt" -stdin
  )"
  export -n ROOT_PASSWORD_HASH 2>/dev/null || true
  plaintext_password=""
  password_salt=""
  unset plaintext_password password_salt
  [[ "$ROOT_PASSWORD_HASH" == '$6$'* ]] ||
    die "Could not derive the console root password hash"
}

preflight_workstation() {
  local command_name
  for command_name in \
    awk basename bash chmod date dirname install mktemp openssl paste python3 \
    scp sha256sum sort ssh stat; do
    require_command_local "$command_name"
  done

  [[ ! -L "${SCRIPT_DIR}/artifacts" ]] ||
    die "The production artifacts path may not be a symlink"
  install -d -m 0700 "${SCRIPT_DIR}/artifacts"
  [[ -d "${SCRIPT_DIR}/artifacts" ]] ||
    die "Could not create the production artifacts directory"
  RUN_DIR="${SCRIPT_DIR}/artifacts/run-$(date -u +%Y%m%dT%H%M%SZ)-$$"
  install -d -m 0700 "$RUN_DIR"
  chmod 0700 "${SCRIPT_DIR}/artifacts" "$RUN_DIR"
}

select_coordinator_and_nodes() {
  CURRENT_PHASE="discovering live cluster"
  COORDINATOR="$(first_reachable_mox)" ||
    die "No reachable mox coordinator was found"
  info "Coordinator: $COORDINATOR"

  node_exec "$COORDINATOR" test -x "$REMOTE_REGISTRY" ||
    die "Installed cluster registry is unavailable on $COORDINATOR"
  node_exec "$COORDINATOR" test -x "$REMOTE_HAPROXY_SYNC" ||
    die "Installed HAProxy sync is unavailable on $COORDINATOR"
  node_exec "$COORDINATOR" bash -c \
    'for command in pvesh qm pvesr ha-manager python3; do command -v "$command" >/dev/null; done' ||
    die "Coordinator lacks required Proxmox commands"

  local nodes_json resources_json cluster_status_json
  cluster_status_json="$(pvesh_get /cluster/status)" ||
    die "Could not query Proxmox quorum"
  write_json "${RUN_DIR}/cluster-status.json" "$cluster_status_json"
  python3 - "${RUN_DIR}/cluster-status.json" "$PROXMOX_CLUSTER_NAME" <<'PY'
import json
import sys
with open(sys.argv[1], encoding="utf-8") as stream:
    rows = json.load(stream)
cluster = [row for row in rows if row.get("type") == "cluster"]
if len(cluster) != 1:
    raise SystemExit("cluster status lacks one cluster record")
if cluster[0].get("name") != sys.argv[2]:
    raise SystemExit("live cluster name differs from cluster.conf")
if cluster[0].get("quorate") not in (1, True, "1"):
    raise SystemExit("Proxmox cluster is not quorate")
PY

  nodes_json="$(pvesh_get /nodes)" ||
    die "Could not query live Proxmox nodes"
  write_json "${RUN_DIR}/nodes.json" "$nodes_json"
  local online_file="${RUN_DIR}/online-nodes"
  if ! parse_online_nodes_json \
    "${RUN_DIR}/nodes.json" "$MAX_MOX_HOSTS" >"$online_file"; then
    die "Live node membership is not eligible for production HA"
  fi
  mapfile -t ONLINE_NODES <"$online_file"
  ((${#ONLINE_NODES[@]} >= 2)) ||
    die "At least two online mox nodes are required"
  local coordinator_online=false node
  for node in "${ONLINE_NODES[@]}"; do
    [[ "$node" != "$COORDINATOR" ]] || coordinator_online=true
  done
  [[ "$coordinator_online" == true ]] ||
    die "Reachable coordinator $COORDINATOR is not an online cluster member"

  resources_json="$(registry_cmd list)" ||
    die "Could not list the pmxcfs production registry"
  write_json "${RUN_DIR}/resources.json" "$resources_json"

  printf '\nOnline eligible nodes and registry purpose tags:\n'
  display_node_purposes "${RUN_DIR}/nodes.json" "${RUN_DIR}/resources.json" |
    while IFS=$'\t' read -r node purposes; do
      printf '  %-8s %s\n' "$node" "$purposes"
    done
}

find_existing_purpose() {
  local purpose="$1"
  python3 - "${RUN_DIR}/resources.json" "$purpose" <<'PY'
import json
import sys

with open(sys.argv[1], encoding="utf-8") as stream:
    resources = json.load(stream)
matches = [
    row for row in resources
    if row.get("kind") == "production"
    and row.get("purpose", {}).get("slug") == sys.argv[2]
]
if len(matches) > 1:
    raise SystemExit("registry contains duplicate production purposes")
if matches:
    print(json.dumps(matches[0], sort_keys=True))
PY
}

parse_resource() {
  local path="${RUN_DIR}/resource.json"
  printf '%s\n' "$RESOURCE_JSON" >"$path"
  chmod 0600 "$path"
  local -a fields=()
  mapfile -d '' -t fields < <(
    python3 - "$path" <<'PY'
import json
import sys

with open(sys.argv[1], encoding="utf-8") as stream:
    row = json.load(stream)
values = [
    row["name"],
    row["vmid"],
    row["ip"],
    row["mac"],
    row["state"],
    row["revision"],
    row["purpose"]["slug"],
    row["domains"]["primary"],
    ",".join(row["domains"]["aliases"]),
    row["domains"]["staging_base"],
    ",".join(row["placement"]),
    row["initial_node"],
    row["owner_node"] or "",
    row["spec"]["cores"],
    row["spec"]["memory_mb"],
    row["spec"]["disk_gib"],
    row["spec"]["disk_allocation"],
    row["spec"]["replication_interval"],
    row["proxmox"]["volume_id"] or "",
    ",".join(row["proxmox"]["replication_targets"]),
    ",".join(row["proxmox"]["ha_nodes"]),
    "1" if row["routes_enabled"] else "0",
]
for value in values:
    sys.stdout.buffer.write(str(value).encode("utf-8") + b"\0")
PY
  )
  ((${#fields[@]} == 22)) || die "Could not parse registry resource"

  RESOURCE_NAME="${fields[0]}"
  VMID="${fields[1]}"
  PROD_PRIVATE_IP="${fields[2]}"
  VM_MAC="${fields[3]}"
  RESOURCE_STATE="${fields[4]}"
  RESOURCE_REVISION="${fields[5]}"
  PURPOSE_SLUG="${fields[6]}"
  PRIMARY_DOMAIN="${fields[7]}"
  IFS=',' read -r -a ALIAS_DOMAINS <<<"${fields[8]}"
  [[ -n "${fields[8]}" ]] || ALIAS_DOMAINS=()
  STAGING_DNS_BASE="${fields[9]}"
  IFS=',' read -r -a PLACEMENT_NODES <<<"${fields[10]}"
  INITIAL_NODE="${fields[11]}"
  OWNER_NODE="${fields[12]}"
  REGISTERED_OWNER_NODE="${fields[12]}"
  VM_CORES="${fields[13]}"
  VM_MEMORY_MB="${fields[14]}"
  ((VM_MEMORY_MB % 1024 == 0)) ||
    die "Registered RAM is not a whole number of GiB"
  VM_MEMORY_GIB=$((VM_MEMORY_MB / 1024))
  VM_DISK_GIB="${fields[15]}"
  DISK_ALLOCATION="${fields[16]}"
  REPLICATION_MINUTES="${fields[17]}"
  ROOT_VOLUME="${fields[18]}"
  REGISTERED_ROOT_VOLUME="${fields[18]}"
  REGISTRY_REPLICATION_CSV="${fields[19]}"
  REGISTRY_HA_CSV="${fields[20]}"
  ROUTES_ENABLED="${fields[21]}"
}

refresh_resource() {
  RESOURCE_JSON="$(registry_cmd get "$RESOURCE_NAME")" ||
    die "Could not refresh registry resource $RESOURCE_NAME"
  parse_resource
}

registry_update() {
  RESOURCE_JSON="$(
    registry_cmd update "$RESOURCE_NAME" \
      --expected-revision "$RESOURCE_REVISION" "$@"
  )" || die "Registry update failed for $RESOURCE_NAME"
  parse_resource
}

load_orchestration_defaults() {
  local metadata
  metadata="$(registry_cmd orchestration-get "$RESOURCE_NAME")" ||
    die "Could not inspect durable orchestration metadata"
  local -a values=()
  mapfile -t values < <(
    python3 - "$metadata" <<'PY'
import json
import sys
value = json.loads(sys.argv[1])
print(value.get("install_phase", "unknown"))
network = value.get("final_network_enabled")
print("" if network is None else ("true" if network else "false"))
print(value.get("startup_sha256") or "")
PY
  )
  ((${#values[@]} == 3)) ||
    die "Could not parse durable orchestration defaults"
  INSTALL_PHASE="${values[0]}"
  STORED_FINAL_NETWORK_ENABLED="${values[1]}"
  STORED_STARTUP_SHA256="${values[2]}"
}

select_startup_script() {
  local required_hash="${STORED_STARTUP_SHA256:-}" startup_path_input canonical marker
  STARTUP_SCRIPT_PATH=""
  STARTUP_SCRIPT_CONTENT=""
  STARTUP_SHA256="none"

  if [[ -n "$required_hash" ]] && [[
    "$INSTALL_PHASE" == installed-confirmed ||
    "$INSTALL_PHASE" == guest-verified ||
    "$INSTALL_PHASE" == network-finalized
  ]]; then
    STARTUP_SHA256="$required_hash"
    info "Durable contract includes startup.sh ${required_hash:0:12}…"
    return
  fi
  if [[ -z "$required_hash" ]] && [[
    "$INSTALL_PHASE" == installed-confirmed ||
    "$INSTALL_PHASE" == guest-verified ||
    "$INSTALL_PHASE" == network-finalized
  ]]; then
    info "Installed legacy contract has no startup.sh; it cannot be added by resume"
    return
  fi

  prompt_optional startup_path_input \
    "Optional local startup.sh path (blank for none; must be idempotent)"
  if [[ -z "$startup_path_input" ]]; then
    [[ -z "$required_hash" ]] ||
      die "Resume requires the previously selected startup.sh"
    return
  fi
  canonical="$(
    python3 - "$startup_path_input" <<'PY'
import os
from pathlib import Path
import sys
raw = Path(os.path.abspath(os.path.expanduser(sys.argv[1])))
current = Path(raw.anchor)
for part in raw.parts[1:]:
    current /= part
    if current.is_symlink():
        raise SystemExit(f"startup.sh path may not contain symlinks: {current}")
resolved = raw.resolve(strict=True)
if not resolved.is_file() or resolved.name != "startup.sh":
    raise SystemExit("startup script must be a regular file named startup.sh")
if not 0 < resolved.stat().st_size <= 262144:
    raise SystemExit("startup.sh must contain 1 through 262144 bytes")
try:
    content = resolved.read_text(encoding="utf-8")
except UnicodeError as exc:
    raise SystemExit(f"startup.sh must be UTF-8 text: {exc}")
if "\0" in content:
    raise SystemExit("startup.sh may not contain NUL")
print(resolved)
PY
  )" || die "Invalid startup.sh selection"
  marker="$(
    cat -- "$canonical"
    printf '\001'
  )"
  STARTUP_SCRIPT_CONTENT="${marker%$'\001'}"
  [[ "$STARTUP_SCRIPT_CONTENT" == '#!'* ]] ||
    die "startup.sh must begin with a shebang"
  STARTUP_SCRIPT_PATH="$canonical"
  STARTUP_SHA256="$(
    printf '%s' "$STARTUP_SCRIPT_CONTENT" | sha256sum | awk '{print $1}'
  )"
  [[ -z "$required_hash" || "$STARTUP_SHA256" == "$required_hash" ]] ||
    die "Selected startup.sh differs from the durable resume contract"
  warn "startup.sh retries after failure and must be idempotent; its success marker prevents later reruns"
}

collect_install_options() {
  local network_default=yes
  [[ "$STORED_FINAL_NETWORK_ENABLED" != false ]] || network_default=no
  select_startup_script
  if [[ "$PROD_GUEST_OS_INSTALL_MODE" == manual &&
    "$STARTUP_SHA256" != none ]]; then
    die "startup.sh cannot be embedded when PROD_GUEST_OS_INSTALL_MODE=manual"
  fi
  prompt_boolean_default FINAL_NETWORK_ENABLED \
    "Enable VM networking after installation?" "$network_default"
  if [[ -n "$STORED_FINAL_NETWORK_ENABLED" &&
    "$FINAL_NETWORK_ENABLED" != "$STORED_FINAL_NETWORK_ENABLED" ]]; then
    die "Networking choice differs from the durable resume contract"
  fi
}

collect_new_request() {
  local default_placement placement_csv aliases_raw
  default_placement="$(IFS=,; printf '%s' "${ONLINE_NODES[*]}")"
  prompt_with_default placement_csv \
    "Eligible placement nodes (comma-separated; minimum two)" \
    "$default_placement"
  placement_csv="$(validate_placement_csv "$placement_csv")" ||
    die "Invalid placement selection"
  IFS=',' read -r -a PLACEMENT_NODES <<<"$placement_csv"

  prompt_optional PRIMARY_DOMAIN \
    "Primary production FQDN (such as example.com)"
  PRIMARY_DOMAIN="$(normalize_domain "$PRIMARY_DOMAIN")" ||
    die "Invalid primary domain"
  prompt_optional aliases_raw \
    "Alias domains (comma-separated; such as www.${PRIMARY_DOMAIN}; blank for none)"
  split_and_normalize_aliases "$aliases_raw"
  STAGING_DNS_BASE="staging.${PRIMARY_DOMAIN}"
  STAGING_DNS_BASE="$(normalize_domain "$STAGING_DNS_BASE")" ||
    die "Invalid staging DNS base"
  [[ "$STAGING_DNS_BASE" != "$PRIMARY_DOMAIN" ]] ||
    die "Staging DNS base must differ from the primary domain"
  local alias
  for alias in "${ALIAS_DOMAINS[@]}"; do
    [[ "$STAGING_DNS_BASE" != "$alias" ]] ||
      die "Staging DNS base must differ from every alias"
  done

  [[ "$PROD_VM_MEMORY_GIB" =~ ^[1-9][0-9]*$ ]] ||
    die "PROD_VM_MEMORY_GIB must be a positive integer"
  local default_memory_gib="$PROD_VM_MEMORY_GIB"
  prompt_with_default VM_CORES "CPU cores" "$PROD_VM_CORES"
  prompt_with_default VM_MEMORY_GIB "RAM in GiB" "$default_memory_gib"
  prompt_with_default VM_DISK_GIB "Root disk in GiB" "$PROD_VM_DISK_GB"
  validate_positive_integer "CPU cores" "$VM_CORES"
  validate_positive_integer "RAM GiB" "$VM_MEMORY_GIB"
  validate_positive_integer "disk size" "$VM_DISK_GIB"
  VM_MEMORY_MB=$((VM_MEMORY_GIB * 1024))

  # Sparse is the default because a guest fstrim can then return freed blocks
  # to rpool, which top-level vdev removal (hosts/decommission_disks.sh)
  # depends on. Resumed allocations keep whatever policy the registry recorded.
  info "Sparse allocation lets an fstrim inside the guest return freed space to rpool."
  info "With full allocation, the disks of this VM's hosts cannot later be decommissioned"
  info "easily, and not with any BMAC script."
  local allocation_choice
  prompt_with_default allocation_choice \
    "Disk allocation (sparse or full/refreservation)" "sparse"
  case "${allocation_choice,,}" in
    sparse) DISK_ALLOCATION="sparse" ;;
    full | reserved | refreservation)
      DISK_ALLOCATION="reserved"
      warn "Full allocation keeps freed space reserved to this VM; hosts/decommission_disks.sh cannot reclaim it"
      ;;
    *) die "Disk allocation must be sparse or full" ;;
  esac

  prompt_with_default INITIAL_NODE "Initial installation node" \
    "${PLACEMENT_NODES[0]}"
  local found=false node
  for node in "${PLACEMENT_NODES[@]}"; do
    [[ "$node" != "$INITIAL_NODE" ]] || found=true
  done
  [[ "$found" == true ]] ||
    die "Initial node must belong to the selected placement"

  prompt_with_default REPLICATION_MINUTES "Replication interval in minutes" \
    "$DEFAULT_REPLICATION_MINUTES"
  validate_positive_integer "replication interval" "$REPLICATION_MINUTES"
  collect_install_options
}

reserve_or_resume_resource() {
  CURRENT_PHASE="collecting production request"
  local purpose existing nextid_json allocation_result placement_csv

  info "The purpose slug is an arbitrary application/workload label and resume key;"
  info "it does not select the production or staging role."
  prompt_with_default purpose \
    "Application/workload purpose slug to create or resume" "production"
  [[ "$purpose" =~ ^[a-z][a-z0-9]*(-[a-z0-9]+)*$ && ${#purpose} -le 63 ]] ||
    die "Purpose must be a lowercase slug of at most 63 characters"
  existing="$(find_existing_purpose "$purpose")" ||
    die "Could not search the registry by purpose"
  if [[ -n "$existing" ]]; then
    RESOURCE_JSON="$existing"
    parse_resource
    printf '\nExisting production allocation:\n'
    info "$RESOURCE_NAME / VMID $VMID / $PROD_PRIVATE_IP"
    info "Purpose: $PURPOSE_SLUG; primary domain: $PRIMARY_DOMAIN"
    info "State: $RESOURCE_STATE; placement: ${PLACEMENT_NODES[*]}"
    info "Registered CPU/RAM/disk: ${VM_CORES} cores / ${VM_MEMORY_GIB} GiB / ${VM_DISK_GIB} GiB ($DISK_ALLOCATION)"
    info "This durable allocation and its registered sizing will be reused."
    prompt_yes "Resume this allocation?" ||
      die "Existing purpose was not selected for resume"
    load_orchestration_defaults
    collect_install_options
  else
    PURPOSE_SLUG="$purpose"
    # The purpose was prompted first so a rerun can find its reservation.
    collect_new_request
    validate_prod_host_capacity

    nextid_json="$(pvesh_get /cluster/nextid)" ||
      die "Could not query the next free Proxmox VMID"
    write_json "${RUN_DIR}/all-live-vms.json" "$(
      pvesh_get /cluster/resources --type vm
    )"
    VMID="$(
      python3 - "$nextid_json" "${RUN_DIR}/all-live-vms.json" \
        "${RUN_DIR}/resources.json" <<'PY'
import json
import sys
value = json.loads(sys.argv[1])
if isinstance(value, str) and value.isdigit():
    value = int(value)
if not isinstance(value, int) or not 100 <= value <= 999_999_999:
    raise SystemExit("invalid /cluster/nextid response")
with open(sys.argv[2], encoding="utf-8") as stream:
    live = json.load(stream)
with open(sys.argv[3], encoding="utf-8") as stream:
    registered = json.load(stream)
used = {
    int(row["vmid"])
    for row in [*live, *registered]
    if str(row.get("vmid", "")).isdigit()
}
while value in used or 9111 <= value <= 9120:
    value += 1
    if value > 999_999_999:
        raise SystemExit("VMID range is exhausted")
print(value)
PY
    )" || die "Proxmox returned an invalid next VMID"

    if [[ "$DRY_RUN" == true ]]; then
      printf '\nDry-run request (no registry or Proxmox mutation):\n'
      info "Candidate VMID at query time: $VMID (not reserved)"
      info "Placement: ${PLACEMENT_NODES[*]}; initial: $INITIAL_NODE"
      info "Purpose: $PURPOSE_SLUG"
      info "Domains: $PRIMARY_DOMAIN${ALIAS_DOMAINS[*]:+, ${ALIAS_DOMAINS[*]}}"
      info "Default staging names: stageN${RESOURCE_NAME:-prodN}.${PRIMARY_DOMAIN}"
      info "CPU/RAM/disk: ${VM_CORES} cores / ${VM_MEMORY_GIB} GiB / ${VM_DISK_GIB} GiB"
      info "Allocation: $DISK_ALLOCATION; replication: */$REPLICATION_MINUTES"
      info "Guest OS install mode: $PROD_GUEST_OS_INSTALL_MODE"
      info "Final networking: $FINAL_NETWORK_ENABLED"
      info "startup.sh: ${STARTUP_SCRIPT_PATH:-none}"
      return 10
    fi

    placement_csv="$(IFS=,; printf '%s' "${PLACEMENT_NODES[*]}")"
    local -a allocate_args=(
      allocate-prod
      --vmid "$VMID"
      --purpose "$PURPOSE_SLUG"
      --primary-domain "$PRIMARY_DOMAIN"
      --staging-dns-base "$STAGING_DNS_BASE"
      --placement "$placement_csv"
      --initial-node "$INITIAL_NODE"
      --allocation-id "create-production-${PURPOSE_SLUG}"
      --cores "$VM_CORES"
      --memory-mb "$VM_MEMORY_MB"
      --disk-gib "$VM_DISK_GIB"
      --disk-allocation "$DISK_ALLOCATION"
      --replication-interval "$REPLICATION_MINUTES"
    )
    local alias
    for alias in "${ALIAS_DOMAINS[@]}"; do
      allocate_args+=(--alias "$alias")
    done

    CURRENT_PHASE="atomically reserving registry identity"
    allocation_result="$(registry_cmd "${allocate_args[@]}")" ||
      die "Atomic production allocation failed"
    NEW_RESOURCE="$(
      python3 - "$allocation_result" <<'PY'
import json
import sys
print("true" if json.loads(sys.argv[1]).get("created") is True else "false")
PY
    )" || die "Registry returned an invalid allocation result"
    RESOURCE_JSON="$(
      python3 - "$allocation_result" <<'PY'
import json
import sys
value = json.loads(sys.argv[1])
print(json.dumps(value["resource"], sort_keys=True))
PY
    )" || die "Registry returned an invalid allocation"
    parse_resource
  fi

  local selected
  selected="$(IFS=,; printf '%s' "${PLACEMENT_NODES[*]}")"
  [[ "$(validate_placement_csv "$selected")" == "$selected" ]] ||
    die "Registered placement is not currently eligible and online"
  if [[ "$DRY_RUN" == true ]]; then
    info "Dry run: existing allocation was inspected; no mutation was made"
    return 10
  fi
}

parse_orchestration_metadata() {
  local -a values=()
  mapfile -t values < <(python3 - "$1" <<'PY'
import json
import sys
value = json.loads(sys.argv[1])
phase = value.get("install_phase")
if phase not in {
    "unknown", "unstarted", "media-attached", "installer-started",
    "installed-confirmed", "guest-verified", "network-finalized",
}:
    raise SystemExit("registry returned an invalid install phase")
print(phase)
network = value.get("final_network_enabled")
if not isinstance(network, bool):
    raise SystemExit("registry returned an invalid final network policy")
print("true" if network else "false")
startup = value.get("startup_sha256")
if startup is not None and (
    not isinstance(startup, str)
    or len(startup) != 64
    or any(character not in "0123456789abcdef" for character in startup)
):
    raise SystemExit("registry returned an invalid startup.sh fingerprint")
print(startup or "none")
PY
  )
  ((${#values[@]} == 3)) || return 1
  INSTALL_PHASE="${values[0]}"
  FINAL_NETWORK_ENABLED="${values[1]}"
  STARTUP_SHA256="${values[2]}"
}

renew_orchestration_lease() {
  local initial_phase="unknown" lease_ttl result
  [[ "$NEW_RESOURCE" == false ]] || initial_phase="unstarted"
  local network_choice=no
  [[ "$FINAL_NETWORK_ENABLED" == false ]] || network_choice=yes
  lease_ttl=$((INSTALL_TIMEOUT_SECONDS + QGA_TIMEOUT_SECONDS +
    REPLICATION_TIMEOUT_SECONDS + 3600))
  ((lease_ttl >= 86400)) || lease_ttl=86400
  ((lease_ttl <= 172800)) ||
    die "Configured operation timeouts exceed the maximum lease duration"
  result="$(
    registry_cmd orchestration-acquire "$RESOURCE_NAME" \
      --nonce "$ORCHESTRATION_NONCE" \
      --owner "${COORDINATOR}-$$" \
      --ttl-seconds "$lease_ttl" \
      --initial-install-phase "$initial_phase" \
      --final-network-enabled "$network_choice" \
      --startup-sha256 "$STARTUP_SHA256" \
      --source-iso-sha256 "${PROD_GUEST_OS_ISO_SHA256,,}" \
      --install-mode "$PROD_GUEST_OS_INSTALL_MODE"
  )" || die "Could not acquire the cluster-wide lease for $RESOURCE_NAME"
  parse_orchestration_metadata "$result" ||
    die "Could not parse the durable install phase"
  ORCHESTRATION_LEASE_ACQUIRED=true
}

acquire_orchestration_lease() {
  CURRENT_PHASE="acquiring resource orchestration lease"
  ORCHESTRATION_NONCE="$(python3 - <<'PY'
import uuid
print(uuid.uuid4().hex)
PY
  )" || die "Could not generate an orchestration lease nonce"
  renew_orchestration_lease
  info "Durable install phase: $INSTALL_PHASE"
}

set_install_phase() {
  local phase="$1" confirmation="${2:-}" result
  local -a args=(
    orchestration-set-phase "$RESOURCE_NAME"
    --nonce "$ORCHESTRATION_NONCE"
    --phase "$phase"
  )
  [[ -z "$confirmation" ]] || args+=(--confirm-wipe "$confirmation")
  result="$(registry_cmd "${args[@]}")" ||
    die "Could not durably advance install phase to $phase"
  parse_orchestration_metadata "$result" ||
    die "Could not parse updated install phase"
}

public_ip_for_node() {
  local node="$1"
  (
    # Isolate config.sh's global arrays and load no secret layer.
    source "$CONFIG_LIB"
    load_proxmox_config --host "$node" --no-secrets >/dev/null
    printf '%s\n' "$PROXMOX_IP"
  )
}

preflight_placement() {
  CURRENT_PHASE="validating placement storage"
  local hook_name hook_ref node hook_path architecture pve_version
  hook_name="$(basename -- "$GUEST_ROLE_HOOK_PATH")"
  [[ "$hook_name" =~ ^[A-Za-z0-9._-]+[.]sh$ ]] ||
    die "Lifecycle hook filename is unsafe"
  HOOK_REF="${SNIPPETS_STORAGE_ID}:snippets/${hook_name}"
  PURPOSE_TAG="${PURPOSE_TAG_PREFIX}${PURPOSE_SLUG}"
  validate_safe_id "purpose tag" "$PURPOSE_TAG"
  VM_TAGS="${PRODUCTION_VM_TAG};${PURPOSE_TAG}"
  ISO_FILENAME="production-${RESOURCE_NAME}-installer.iso"
  local -a affinity_nodes=()
  for node in "${PLACEMENT_NODES[@]}"; do
    affinity_nodes+=("${node}:1")
  done
  AFFINITY_NODES_CSV="$(IFS=,; printf '%s' "${affinity_nodes[*]}")"

  for node in "${PLACEMENT_NODES[@]}"; do
    node_exec "$node" bash -c \
      'for command in curl df findmnt flock qm pvecm pvesr pvesh zfs pvesm ha-manager pveversion python3 sha256sum xorriso; do command -v "$command" >/dev/null; done' ||
      die "$node lacks a required Proxmox/ZFS command"
    node_exec "$node" test -x "$REMOTE_ISO_PREPARER" ||
      die "$node lacks the installed production ISO preparation helper; rerun host setup"
    architecture="$(node_exec "$node" uname -m)"
    [[ "$architecture" == x86_64 ]] ||
      die "$node is not amd64; q35/OVMF Ubuntu installation is unsuitable"
    pve_version="$(node_exec "$node" pveversion)"
    [[ "$pve_version" == pve-manager/9.* ]] ||
      die "$node is not running Proxmox VE 9: $pve_version"
    [[ "$pve_version" =~ ^pve-manager/9\.([0-9]+)\. &&
      ${BASH_REMATCH[1]} -ge 2 ]] ||
      die "$node requires Proxmox VE 9.2+ for complete ms-cert=2023k enrollment"
    node_exec "$node" pvesm status --storage "$PROD_VM_STORAGE" >/dev/null ||
      die "$PROD_VM_STORAGE is unavailable on $node"
    hook_path="$(node_exec "$node" pvesm path "$HOOK_REF")" ||
      die "$HOOK_REF cannot be resolved on $node"
    [[ "$hook_path" =~ ^/[A-Za-z0-9._/-]+$ ]] ||
      die "$node returned an unsafe lifecycle-hook path"
    node_exec "$node" test -x "$hook_path" ||
      die "Lifecycle hook is not executable on $node: $hook_path"
  done
}

locate_vm() {
  local identity_mode="${1:-strict}"
  local cluster_json="${RUN_DIR}/cluster-vms.json"
  local parsed="${RUN_DIR}/cluster-vm-${VMID}.fields" status
  write_json "$cluster_json" "$(pvesh_get /cluster/resources --type vm)"
  if python3 - "$cluster_json" "$VMID" >"$parsed" <<'PY'
import json
import sys
with open(sys.argv[1], encoding="utf-8") as stream:
    rows = json.load(stream)
wanted = int(sys.argv[2])
matches = [row for row in rows if row.get("vmid") in (wanted, str(wanted))]
if len(matches) > 1:
    raise SystemExit("duplicate VMID in cluster resources")
if not matches:
    raise SystemExit(3)
row = matches[0]
for value in (
    row.get("node", ""),
    row.get("name", ""),
    row.get("type", ""),
    row.get("status", ""),
):
    sys.stdout.buffer.write(str(value).encode() + b"\0")
PY
  then
    status=0
  else
    status=$?
  fi
  if ((status == 3)); then
    OWNER_NODE=""
    VM_LIVE_STATUS="absent"
    return 1
  fi
  local -a fields=()
  mapfile -d '' -t fields <"$parsed"
  ((status == 0 && ${#fields[@]} == 4)) ||
    die "Could not parse live VM $VMID"
  OWNER_NODE="${fields[0]}"
  if [[ "${fields[1]}" != "$RESOURCE_NAME" || "${fields[2]}" != qemu ]]; then
    if [[ "$identity_mode" == allow-transient ]]; then
      return 4
    fi
    die "VMID $VMID exists with an unexpected name or type"
  fi
  VM_LIVE_STATUS="${fields[3]}"
  local eligible=false node
  for node in "${PLACEMENT_NODES[@]}"; do
    [[ "$node" != "$OWNER_NODE" ]] || eligible=true
  done
  [[ "$eligible" == true ]] ||
    die "VM $VMID is on $OWNER_NODE outside its registered placement"
  return 0
}

wait_for_created_vm_identity() {
  local deadline=$((SECONDS + 30)) status
  while ((SECONDS < deadline)); do
    if locate_vm allow-transient; then
      return 0
    else
      status=$?
    fi
    if ((status != 1 && status != 4)); then
      return "$status"
    fi
    sleep 2
  done
  # One strict inspection gives a precise fail-closed diagnostic.
  locate_vm
}

iso_storage_root() {
  local storage_json="${RUN_DIR}/iso-storage.json"
  write_json "$storage_json" "$(pvesh_get "/storage/${ISO_STORAGE_ID}")"
  python3 - "$storage_json" "$INITIAL_NODE" <<'PY'
import json
import re
import sys
with open(sys.argv[1], encoding="utf-8") as stream:
    storage = json.load(stream)
if storage.get("type") != "dir":
    raise SystemExit("ISO storage must use the Proxmox dir backend")
contents = {
    part.strip()
    for part in str(storage.get("content", "")).split(",")
    if part.strip()
}
if "iso" not in contents:
    raise SystemExit("ISO storage does not allow iso content")
nodes = {
    part.strip()
    for part in str(storage.get("nodes", "")).split(",")
    if part.strip()
}
if nodes and sys.argv[2] not in nodes:
    raise SystemExit("ISO storage is not enabled on the initial node")
path = storage.get("path")
if not isinstance(path, str) or not re.fullmatch(r"/[A-Za-z0-9._/-]+", path):
    raise SystemExit("ISO storage returned an unsafe path")
iso_directory = "template/iso"
for entry in str(storage.get("content-dirs", "")).split(","):
    if entry.startswith("iso="):
        iso_directory = entry.split("=", 1)[1]
if (
    not re.fullmatch(r"[A-Za-z0-9._/-]+", iso_directory)
    or iso_directory.startswith("/")
    or ".." in iso_directory.split("/")
):
    raise SystemExit("ISO storage has an unsafe content-dirs override")
print(path.rstrip("/") + "/" + iso_directory.strip("/"))
PY
}

write_iso_request() {
  local request="${RUN_DIR}/autoinstall-request.json"
  {
    printf '%s\0' \
      "$RESOURCE_NAME" \
      "${PROD_PRIVATE_IP}/24" \
      "$GUEST_GATEWAY" \
      "$PRODUCTION_IP_START" \
      "$PRODUCTION_IP_END" \
      "$VM_MAC" \
      "$ROOT_PASSWORD_HASH" \
      "$ADMIN_1_PUBLIC_SSH_KEY" \
      "$ADMIN_2_PUBLIC_SSH_KEY" \
      "$STARTUP_SCRIPT_CONTENT" \
      "${#DNS_SERVERS[@]}"
    printf '%s\0' "${DNS_SERVERS[@]}"
  } | python3 -c '
import json
import sys
parts = sys.stdin.buffer.read().split(b"\0")
if parts[-1] != b"":
    raise SystemExit("truncated autoinstall request")
parts = [part.decode("utf-8") for part in parts[:-1]]
count = int(parts[10])
if len(parts) != 11 + count:
    raise SystemExit("invalid DNS value count")
request = {
    "hostname": parts[0],
    "address": parts[1],
    "gateway": parts[2],
    "production_ip_start": parts[3],
    "production_ip_end": parts[4],
    "mac": parts[5],
    "root_password_hash": parts[6],
    "authorized_keys": parts[7:9],
    "startup_script": parts[9] or None,
    "nameservers": parts[11:],
}
json.dump(request, sys.stdout, sort_keys=True)
sys.stdout.write("\n")
' >"$request"
  chmod 0600 "$request"
  printf '%s\n' "$request"
}

copy_request_to_node() {
  local source="$1" destination="$2"
  local coordinator_destination coordinator_stage
  if [[ "$REMOTE_ISO_NODE" == "$COORDINATOR" ]]; then
    _mox_ssh_options "$COORDINATOR"
    coordinator_destination="$(_mox_ssh_destination "$COORDINATOR")"
    scp "${MOX_SSH_OPTIONS[@]}" -- "$source" \
      "${MOX_SSH_USER:-root}@${coordinator_destination}:${destination}"
  else
    coordinator_stage="/run/app-ha-prod-request-${ORCHESTRATION_NONCE}.json"
    COORDINATOR_REQUEST_STAGE="$coordinator_stage"
    _mox_ssh_options "$COORDINATOR"
    coordinator_destination="$(_mox_ssh_destination "$COORDINATOR")"
    scp "${MOX_SSH_OPTIONS[@]}" -- "$source" \
      "${MOX_SSH_USER:-root}@${coordinator_destination}:${coordinator_stage}"
    mox_ssh "$COORDINATOR" scp \
      -o BatchMode=yes \
      -o ClearAllForwardings=yes \
      -o StrictHostKeyChecking=yes \
      -o CheckHostIP=no \
      -o "HostKeyAlias=${REMOTE_ISO_NODE}" \
      -o "UserKnownHostsFile=/etc/pve/nodes/${REMOTE_ISO_NODE}/ssh_known_hosts" \
      -o GlobalKnownHostsFile=none \
      -- "$coordinator_stage" \
      "root@${REMOTE_ISO_NODE}.${PROXMOX_INTERNAL_DOMAIN}:${destination}"
    mox_ssh "$COORDINATOR" rm -f -- "$coordinator_stage"
    COORDINATOR_REQUEST_STAGE=""
  fi
  node_exec "$REMOTE_ISO_NODE" chmod 0600 "$destination"
}

build_and_upload_iso() {
  CURRENT_PHASE="downloading and building verified autoinstall ISO on the Proxmox host"
  local iso_root request preparation_json
  iso_root="$(iso_storage_root)" ||
    die "Could not validate $ISO_STORAGE_ID ISO storage"
  REMOTE_ISO_NODE="$INITIAL_NODE"
  REMOTE_ISO_PATH="${iso_root}/${ISO_FILENAME}"
  [[ "$REMOTE_ISO_PATH" =~ ^/[A-Za-z0-9._/-]+$ ]] ||
    die "Derived ISO destination is unsafe"

  request="$(write_iso_request)"
  REMOTE_ISO_REQUEST="/run/app-ha-prod-request-${ORCHESTRATION_NONCE}.json"
  copy_request_to_node "$request" "$REMOTE_ISO_REQUEST"

  # Preparation cannot attach media. If SSH fails ambiguously, cleanup may
  # therefore remove a possibly completed per-VM ISO without risking a VM.
  REMOTE_ISO_UPLOADED=true
  REMOTE_ISO_SAFE_TO_DELETE=true
  preparation_json="$(
    node_exec "$REMOTE_ISO_NODE" "$REMOTE_ISO_PREPARER" \
    --source-url "$PROD_GUEST_OS_ISO_URL" \
    --expected-sha256 "$PROD_GUEST_OS_ISO_SHA256" \
    --install-mode "$PROD_GUEST_OS_INSTALL_MODE" \
    --request "$REMOTE_ISO_REQUEST" \
    --output-iso "$REMOTE_ISO_PATH"
  )" || die "Could not prepare the production installer ISO on $REMOTE_ISO_NODE"
  node_exec "$REMOTE_ISO_NODE" rm -f -- "$REMOTE_ISO_REQUEST"
  REMOTE_ISO_REQUEST=""

  local -a prepared_fields=()
  mapfile -t prepared_fields < <(
    python3 - "$preparation_json" "$REMOTE_ISO_PATH" \
      "$SOURCE_ISO_CACHE_PATH" "$SOURCE_ISO_SHA256_PATH" \
      "$PROD_GUEST_OS_INSTALL_MODE" <<'PY'
import json
import re
import sys

value = json.loads(sys.argv[1])
if value.get("custom_iso") != sys.argv[2]:
    raise SystemExit("helper returned an unexpected custom ISO path")
if value.get("cache_iso") != sys.argv[3]:
    raise SystemExit("helper returned an unexpected source cache path")
if value.get("cache_sha256_file") != sys.argv[4]:
    raise SystemExit("helper returned an unexpected source checksum path")
if value.get("install_mode") != sys.argv[5]:
    raise SystemExit("helper returned an unexpected install mode")
digest = value.get("custom_sha256")
size = value.get("custom_bytes")
if not isinstance(digest, str) or not re.fullmatch(r"[0-9a-f]{64}", digest):
    raise SystemExit("helper returned an invalid custom ISO hash")
if not isinstance(size, int) or size <= 0:
    raise SystemExit("helper returned an invalid custom ISO size")
print(digest)
print(size)
print("true" if value.get("cache_reused") is True else "false")
PY
  ) || die "Could not validate the host ISO preparation result"
  ((${#prepared_fields[@]} == 3)) ||
    die "Host ISO preparation result is incomplete"
  SOURCE_ISO_CACHE_REUSED="${prepared_fields[2]}"
  info "Prepared custom installer ISO on $REMOTE_ISO_NODE (${prepared_fields[1]} bytes)"
  info "Source ISO cache: $SOURCE_ISO_CACHE_PATH ($(
    [[ "$SOURCE_ISO_CACHE_REUSED" == true ]] && printf reused || printf downloaded
  ))"
}

create_vm() {
  CURRENT_PHASE="creating stopped QEMU VM"
  [[ -n "$REMOTE_ISO_PATH" ]] || die "Installer ISO was not prepared"
  # Once qm create is submitted, a lost SSH reply is ambiguous: retain the ISO
  # until a rerun inspects the VM rather than deleting a possibly attached CD.
  REMOTE_ISO_SAFE_TO_DELETE=false

  # PVE 9 documented qm syntax. q35 + OVMF/4m is suitable for this amd64
  # Ubuntu guest. efidisk0 stores firmware variables; scsi0 is the VM's one
  # data disk. --ha-managed is intentionally omitted until replicas are healthy.
  node_exec "$INITIAL_NODE" qm create "$VMID" \
    --name "$RESOURCE_NAME" \
    --description "Production guest (${PURPOSE_SLUG})" \
    --machine q35 \
    --bios ovmf \
    --efidisk0 "${PROD_VM_STORAGE}:1,efitype=4m,ms-cert=2023k,pre-enrolled-keys=1" \
    --sockets 1 \
    --cores "$VM_CORES" \
    --memory "$VM_MEMORY_MB" \
    --balloon 0 \
    --cpu "$PROD_VM_CPU_TYPE" \
    --ostype l26 \
    --scsihw virtio-scsi-single \
    --scsi0 "${PROD_VM_STORAGE}:${VM_DISK_GIB},discard=on,iothread=1,replicate=1,ssd=1" \
    --net0 "virtio=${VM_MAC},bridge=${PROD_VM_BRIDGE},firewall=1" \
    --agent "enabled=1,fstrim_cloned_disks=1" \
    --tags "$VM_TAGS" \
    --vga std \
    --tablet 0 \
    --onboot 0 \
    --ide2 "${ISO_STORAGE_ID}:iso/${ISO_FILENAME},media=cdrom" \
    --boot "order=ide2;scsi0"
  REMOTE_ISO_ATTACHED=true
  REMOTE_ISO_SAFE_TO_DELETE=false
  OWNER_NODE="$INITIAL_NODE"
}

verify_vm_config() {
  local config_file="${RUN_DIR}/qm-${VMID}.conf"
  local parsed_config="${RUN_DIR}/qm-${VMID}.parsed"
  node_exec "$OWNER_NODE" qm config "$VMID" >"$config_file"
  chmod 0600 "$config_file"
  local -a values=()
  if ! python3 - "$config_file" \
      "$RESOURCE_NAME" "$VM_CORES" "$VM_MEMORY_MB" "$PROD_VM_CPU_TYPE" \
      "$PROD_VM_STORAGE" "$PROD_VM_BRIDGE" "$VM_MAC" "$HOOK_REF" \
      "$PRODUCTION_VM_TAG" "$PURPOSE_TAG" "$ISO_STORAGE_ID" "$ISO_FILENAME" \
      "$ROOT_VOLUME" "$VM_DISK_GIB" "$INSTALL_PHASE" \
      "$FINAL_NETWORK_ENABLED" >"$parsed_config" <<'PY'
import re
import sys

(
    path,
    name,
    cores,
    memory,
    cpu,
    storage,
    bridge,
    mac,
    hook,
    prod_tag,
    purpose_tag,
    iso_storage,
    iso_filename,
    registered_root_volume,
    disk_gib,
    install_phase,
    final_network_enabled,
) = sys.argv[1:]
config = {}
with open(path, encoding="utf-8") as stream:
    for line in stream:
        if ": " in line:
            key, value = line.rstrip("\n").split(": ", 1)
            config[key] = value

def fail(message):
    raise SystemExit(message)

if config.get("name") != name:
    fail("VM name differs from registry")
if config.get("cores") != cores:
    fail("VM core count differs from registry")
observed_memory = config.get("memory", "").split(",", 1)[0]
if observed_memory not in {memory, f"current={memory}"}:
    fail("VM memory differs from registry")
if config.get("cpu", "").split(",", 1)[0] != cpu:
    fail("VM CPU type differs from cluster contract")
if config.get("bios") != "ovmf":
    fail("VM must use OVMF")
if config.get("machine", "").split(",", 1)[0] not in {"q35"} and not config.get("machine", "").startswith("pc-q35-"):
    fail("VM must use q35")
efi_parts = config.get("efidisk0", "").split(",")
if not efi_parts[0].startswith(storage + ":"):
    fail("VM EFI vars are not on production storage")
efi_options = dict(part.split("=", 1) for part in efi_parts[1:] if "=" in part)
if (
    set(efi_options) != {"efitype", "ms-cert", "pre-enrolled-keys", "size"}
    or efi_options.get("efitype") != "4m"
    or efi_options.get("pre-enrolled-keys") != "1"
    or efi_options.get("ms-cert") != "2023k"
    # PVE's 4m OVMF varstore template is a sparse 528-KiB payload and its
    # ZFS-backed EFI volume is canonically reported by qm as size=1M.
    or efi_options.get("size", "").upper() != "1M"
):
    fail("VM EFI vars lack the current 4m Secure Boot certificate contract")
if any(
    re.fullmatch(r"(?:unused|efidisk|tpmstate)[0-9]+", key) and key != "efidisk0"
    for key in config
):
    fail("VM has an unexpected auxiliary or unused disk volume")
if config.get("scsihw") != "virtio-scsi-single":
    fail("VM SCSI controller differs from contract")
if config.get("sockets", "1") != "1":
    fail("VM socket count differs from contract")
if config.get("balloon", "0") != "0":
    fail("VM memory ballooning must be disabled")
if config.get("ostype") != "l26":
    fail("VM OS type differs from contract")
observed_hook = config.get("hookscript", "")
if install_phase == "network-finalized":
    if observed_hook != hook:
        fail("finalized VM lifecycle hook differs from contract")
elif observed_hook not in {"", hook}:
    fail("VM has an unexpected lifecycle hook")
if config.get("onboot", "0") != "0":
    fail("HA VM must not use onboot")
agent_parts = set(config.get("agent", "").split(","))
if (
    not ({"1", "enabled=1"} & agent_parts)
    or "fstrim_cloned_disks=1" not in agent_parts
):
    fail("QEMU guest agent channel is disabled")

network_keys = sorted(key for key in config if re.fullmatch(r"net[0-9]+", key))
if network_keys != ["net0"]:
    fail("production VM must have exactly one NIC (net0)")
net0 = config["net0"]
net_options = dict(
    part.split("=", 1) for part in net0.split(",") if "=" in part
)
if (
    net_options.get("virtio", "").upper() != mac.upper()
    or net_options.get("bridge") != bridge
    or net_options.get("firewall") != "1"
):
    fail("net0 MAC or bridge differs from registry")
link_down = net_options.get("link_down", "0")
if link_down not in {"0", "1"}:
    fail("net0 link_down is invalid")
if install_phase == "network-finalized":
    expected_link_down = "0" if final_network_enabled == "true" else "1"
    if link_down != expected_link_down:
        fail("net0 final link state differs from durable contract")
elif install_phase != "guest-verified" and link_down != "0":
    fail("installer and guest verification require an enabled VM network link")

data_disks = []
cdrom = ""
for key, value in config.items():
    if re.fullmatch(r"(?:scsi|sata|virtio|ide)[0-9]+", key):
        if "media=cdrom" in value:
            if cdrom:
                fail("VM has more than one CD-ROM")
            cdrom = value.split(",", 1)[0]
        else:
            data_disks.append((key, value))
if len(data_disks) != 1 or data_disks[0][0] != "scsi0":
    fail("production VM must have exactly one scsi0 data disk")
disk_parts = data_disks[0][1].split(",")
root_volume = disk_parts[0]
if not root_volume.startswith(storage + ":"):
    fail("root disk is not on production storage")
if registered_root_volume and root_volume != registered_root_volume:
    fail("root disk volume identity differs from registry")
disk_options = dict(part.split("=", 1) for part in disk_parts[1:] if "=" in part)
if set(disk_options) != {"discard", "iothread", "replicate", "size", "ssd"}:
    fail("root disk has unexpected or missing options")
for option in ("discard", "iothread", "replicate", "ssd"):
    if disk_options.get(option) != "1" and not (
        option == "discard" and disk_options.get(option) == "on"
    ):
        fail(f"root disk {option} option differs from contract")
size = disk_options.get("size", "")
match = re.fullmatch(r"([0-9]+)([KMGTP])", size, re.IGNORECASE)
units = {"K": 2**10, "M": 2**20, "G": 2**30, "T": 2**40, "P": 2**50}
if not match or int(match.group(1)) * units[match.group(2).upper()] != int(disk_gib) * 2**30:
    fail("root disk size differs from registry")
allowed_cdroms = {
    f"{iso_storage}:iso/{iso_filename}",
}
if cdrom and cdrom not in allowed_cdroms:
    fail("attached CD-ROM is not this VM's per-VM installer ISO")
boot_order = config.get("boot", "")
expected_boot = "order=ide2;scsi0" if cdrom else "order=scsi0"
if boot_order != expected_boot:
    fail("VM boot order differs from its durable install phase")

tags = {part for part in re.split(r"[;,]", config.get("tags", "")) if part}
if not {prod_tag, purpose_tag}.issubset(tags):
    fail("VM lacks its production and purpose tags")
if any("tailscale" in key.lower() or "tailscale" in value.lower() for key, value in config.items()):
    fail("VM config unexpectedly references Tailscale")

for value in (root_volume, cdrom):
    sys.stdout.buffer.write(value.encode() + b"\0")
PY
  then
    die "Live VM configuration does not match the registry contract"
  fi
  mapfile -d '' -t values <"$parsed_config"
  ((${#values[@]} == 2)) || die "Could not parse VM disk configuration"
  ROOT_VOLUME="${values[0]}"
  validate_safe_id "root volume ID" "$ROOT_VOLUME"
  ATTACHED_ISO_VOLUME="${values[1]}"
  if [[ -n "$ATTACHED_ISO_VOLUME" ]]; then
    ISO_FILENAME="${ATTACHED_ISO_VOLUME#*:iso/}"
    [[ "$ISO_FILENAME" =~ ^[A-Za-z0-9._-]+[.]iso$ ]] ||
      die "Attached installer ISO filename is unsafe"
    REMOTE_ISO_ATTACHED=true
    REMOTE_ISO_SAFE_TO_DELETE=false
  else
    REMOTE_ISO_ATTACHED=false
  fi
}

set_remote_iso_location() {
  local iso_root
  iso_root="$(iso_storage_root)" ||
    die "Could not resolve the per-VM installer path"
  REMOTE_ISO_NODE="$INITIAL_NODE"
  REMOTE_ISO_PATH="${iso_root}/${ISO_FILENAME}"
  [[ "$REMOTE_ISO_PATH" =~ ^/[A-Za-z0-9._/-]+$ ]] ||
    die "Derived ISO destination is unsafe"
}

apply_disk_allocation() {
  local storage_node="${1:-$OWNER_NODE}"
  CURRENT_PHASE="applying ZFS allocation policy"
  node_exec "$storage_node" bash -c '
set -Eeuo pipefail
volume="$1"
allocation="$2"
expected_gib="$3"
path="$(pvesm path "$volume")"
case "$path" in
  /dev/zvol/*) dataset="${path#/dev/zvol/}" ;;
  *) printf "Unexpected non-ZFS volume path: %s\n" "$path" >&2; exit 1 ;;
esac
volsize="$(zfs get -Hp -o value volsize "$dataset")"
[[ "$volsize" =~ ^[0-9]+$ && "$volsize" -eq $((expected_gib * 1024 * 1024 * 1024)) ]]
if [[ "$allocation" == reserved ]]; then
  zfs set refreservation=auto "$dataset"
  refreservation="$(zfs get -Hp -o value refreservation "$dataset")"
  # OpenZFS may reserve additional metadata overhead for a zvol, so "auto"
  # can legitimately exceed volsize. It must never reserve less.
  [[ "$refreservation" =~ ^[0-9]+$ && "$refreservation" -ge "$volsize" ]]
else
  zfs set refreservation=none "$dataset"
  [[ "$(zfs get -Hp -o value refreservation "$dataset")" == 0 ]]
fi
' -- "$ROOT_VOLUME" "$DISK_ALLOCATION" "$VM_DISK_GIB"
}

validate_disk_allocation() {
  local storage_node="${1:-$OWNER_NODE}"
  CURRENT_PHASE="validating ZFS allocation policy"
  node_exec "$storage_node" bash -c '
set -Eeuo pipefail
volume="$1"
allocation="$2"
expected_gib="$3"
path="$(pvesm path "$volume")"
case "$path" in
  /dev/zvol/*) dataset="${path#/dev/zvol/}" ;;
  *) printf "Unexpected non-ZFS volume path: %s\n" "$path" >&2; exit 1 ;;
esac
volsize="$(zfs get -Hp -o value volsize "$dataset")"
[[ "$volsize" =~ ^[0-9]+$ && "$volsize" -eq $((expected_gib * 1024 * 1024 * 1024)) ]]
refreservation="$(zfs get -Hp -o value refreservation "$dataset")"
if [[ "$allocation" == reserved ]]; then
  [[ "$refreservation" =~ ^[0-9]+$ && "$refreservation" -ge "$volsize" ]]
else
  [[ "$refreservation" == 0 ]]
fi
' -- "$ROOT_VOLUME" "$DISK_ALLOCATION" "$VM_DISK_GIB"
}

validate_or_complete_existing_disk_allocation() {
  if [[ "$RESOURCE_STATE" == reserved &&
    "$INSTALL_PHASE" == unstarted &&
    -z "$REGISTERED_OWNER_NODE" &&
    -z "$REGISTERED_ROOT_VOLUME" &&
    -z "$REGISTRY_REPLICATION_CSV" &&
    -z "$REGISTRY_HA_CSV" &&
    "$ROUTES_ENABLED" == 0 ]]; then
    info "Completing disk allocation policy for the preserved unstarted VM"
    apply_disk_allocation
  else
    validate_disk_allocation
  fi
}

ensure_registry_provisioning() {
  local -a changes=(--owner-node "$OWNER_NODE" --volume-id "$ROOT_VOLUME")
  case "$RESOURCE_STATE" in
    reserved | failed)
      changes=(--state provisioning "${changes[@]}")
      ;;
    provisioning) ;;
    *) die "Cannot resume registry state $RESOURCE_STATE" ;;
  esac
  registry_update "${changes[@]}"
}

vm_status() {
  node_exec "$OWNER_NODE" qm status "$VMID" |
    awk '$1 == "status:" {print $2; found=1} END {if (!found) exit 1}'
}

qga_ping() {
  node_exec "$OWNER_NODE" qm agent "$VMID" ping </dev/null >/dev/null 2>&1
}

wait_for_vm_stopped() {
  local deadline=$((SECONDS + INSTALL_TIMEOUT_SECONDS)) status
  while ((SECONDS < deadline)); do
    status="$(vm_status)"
    if [[ "$status" == stopped ]]; then
      return 0
    fi
    [[ "$status" == running ]] ||
      die "Installer VM entered unexpected status: $status"
    sleep 15
  done
  die "Timed out after ${INSTALL_TIMEOUT_SECONDS}s waiting for installer poweroff"
}

wait_for_qga() {
  local deadline=$((SECONDS + QGA_TIMEOUT_SECONDS))
  while ((SECONDS < deadline)); do
    locate_vm >/dev/null
    if [[ "$VM_LIVE_STATUS" == running ]] && qga_ping; then
      return 0
    fi
    sleep 10
  done
  return 1
}

detach_and_delete_iso() {
  CURRENT_PHASE="detaching installer ISO"
  locate_vm >/dev/null || die "VM disappeared before ISO detach"
  verify_vm_config
  set_remote_iso_location
  if [[ -n "$ATTACHED_ISO_VOLUME" ]]; then
    node_exec "$OWNER_NODE" qm set "$VMID" --delete ide2
    node_exec "$OWNER_NODE" qm set "$VMID" --boot "order=scsi0"
  fi
  REMOTE_ISO_ATTACHED=false
  REMOTE_ISO_SAFE_TO_DELETE=true
  if [[ -n "$REMOTE_ISO_PATH" ]]; then
    node_exec "$REMOTE_ISO_NODE" rm -f -- \
      "$REMOTE_ISO_PATH" "${REMOTE_ISO_PATH}.partial"
  fi
  REMOTE_ISO_UPLOADED=false
}

install_or_resume_os() {
  CURRENT_PHASE="inspecting installation state"
  renew_orchestration_lease
  local vm_exists=false
  if locate_vm; then
    vm_exists=true
    verify_vm_config
    validate_or_complete_existing_disk_allocation
  fi

  if [[ "$vm_exists" == false ]]; then
    [[ "$RESOURCE_STATE" == reserved ]] ||
      die "Registered $RESOURCE_STATE VM $VMID is missing; recovery must not recreate it"
    [[ "$INSTALL_PHASE" == unstarted || "$INSTALL_PHASE" == unknown ]] ||
      die "Missing VM has durable install phase $INSTALL_PHASE; refusing recreation"
    [[ -z "$REGISTERED_OWNER_NODE" && -z "$REGISTERED_ROOT_VOLUME" &&
      -z "$REGISTRY_REPLICATION_CSV" && -z "$REGISTRY_HA_CSV" &&
      "$ROUTES_ENABLED" == 0 ]] ||
      die "Reserved missing VM already has Proxmox metadata; refusing recreation"
    local legacy_missing_confirmation=""
    if [[ "$INSTALL_PHASE" == unknown ]]; then
      confirm_exact_local \
        "This legacy reservation lacks a durable sentinel. Confirm creating its missing VM." \
        "WIPE"
      legacy_missing_confirmation="WIPE"
    fi
    build_and_upload_iso
    create_vm
    wait_for_created_vm_identity >/dev/null ||
      die "New VM did not converge in cluster resources"
    [[ "$VM_LIVE_STATUS" == stopped ]] ||
      die "New VM must initially be stopped"
    verify_vm_config
    apply_disk_allocation
    ensure_registry_provisioning
    set_install_phase media-attached "$legacy_missing_confirmation"
  fi

  if [[ "$INSTALL_PHASE" == network-finalized ||
    "$INSTALL_PHASE" == guest-verified ||
    "$INSTALL_PHASE" == installed-confirmed ]]; then
    if [[ "$VM_LIVE_STATUS" == running ]] && ! qga_ping; then
      die "Durably installed VM is running without QGA; refusing installer fallback"
    fi
    detach_and_delete_iso
    return
  fi

  if [[ "$INSTALL_PHASE" == unknown || "$INSTALL_PHASE" == unstarted ]]; then
    if [[ "$VM_LIVE_STATUS" == running && -z "$ATTACHED_ISO_VOLUME" ]] &&
      qga_ping; then
      info "QGA attests an existing installed guest; recording the durable sentinel"
      set_install_phase installed-confirmed
      return
    fi
    if [[ "$INSTALL_PHASE" == unknown ]]; then
      [[ "$VM_LIVE_STATUS" == stopped ]] ||
        die "Unknown-phase VM is running; stop and inspect it before any WIPE"
      confirm_exact_local \
        "No durable install sentinel exists. Reinstalling can erase the root disk." \
        "WIPE"
    fi
    ensure_registry_provisioning
    if [[ -z "$ATTACHED_ISO_VOLUME" ]]; then
      build_and_upload_iso
      REMOTE_ISO_SAFE_TO_DELETE=false
      node_exec "$OWNER_NODE" qm set "$VMID" \
        --ide2 "${ISO_STORAGE_ID}:iso/${ISO_FILENAME},media=cdrom" \
        --boot "order=ide2;scsi0"
      REMOTE_ISO_ATTACHED=true
      REMOTE_ISO_SAFE_TO_DELETE=false
    else
      set_remote_iso_location
      REMOTE_ISO_UPLOADED=true
    fi
    if [[ "$INSTALL_PHASE" == unknown ]]; then
      set_install_phase media-attached WIPE
    else
      set_install_phase media-attached
    fi
  fi

  if [[ "$INSTALL_PHASE" == installer-started ]]; then
    [[ -n "$ATTACHED_ISO_VOLUME" ]] ||
      die "Installer-started sentinel has no attached installer; refusing to guess"
    if [[ "$VM_LIVE_STATUS" == running ]]; then
      info "Installer is already running on $OWNER_NODE"
      wait_for_vm_stopped
    elif [[ "$VM_LIVE_STATUS" != stopped ]]; then
      die "Installer VM entered unexpected status: $VM_LIVE_STATUS"
    fi
    local result
    printf '%s\nType GO to preserve the installed disk, or WIPE to rerun the installer.\n> ' \
      "Inspect the Proxmox console before choosing."
    IFS= read -r result
    if [[ "$result" == GO ]]; then
      set_install_phase installed-confirmed
      detach_and_delete_iso
      return
    fi
    [[ "$result" == WIPE ]] ||
      die "Confirmation was neither GO nor WIPE; installer was not restarted"
    set_install_phase media-attached WIPE
  fi

  [[ "$INSTALL_PHASE" == media-attached ]] ||
    die "Cannot start installer from durable phase $INSTALL_PHASE"
  [[ "$VM_LIVE_STATUS" == stopped && -n "$ATTACHED_ISO_VOLUME" ]] ||
    die "Media-attached install phase requires a stopped VM and attached ISO"
  CURRENT_PHASE="awaiting installation confirmation"
  printf '\nInstallation target:\n'
  info "$RESOURCE_NAME (VMID $VMID) on $OWNER_NODE"
  info "Console: Proxmox node $OWNER_NODE, VM $VMID"
  info "Installer ISO is attached and first in the VM boot order."
  if [[ "$PROD_GUEST_OS_INSTALL_MODE" == manual ]]; then
    print_manual_install_instructions
    info "The manual installer must finish by powering the VM off."
  else
    info "The unattended installer must finish by powering the VM off."
  fi
  confirm_exact_local "Start the guest OS installation now?" \
    "INSTALL ${RESOURCE_NAME}"
  log "Starting guest OS installation"
  info "Starting $RESOURCE_NAME (VMID $VMID) from its attached installer ISO."
  info "Monitor the console at https://${OWNER_NODE}:8006/ (select VM $VMID)."
  info "Waiting up to ${INSTALL_TIMEOUT_SECONDS} seconds for the installer to power the VM off."
  set_install_phase installer-started
  node_exec "$OWNER_NODE" qm start "$VMID"
  wait_for_vm_stopped
  info "VM $VMID powered off; inspect the console before confirming success."
  confirm_exact_local \
    "Confirm the Proxmox console reports a successful Ubuntu install and poweroff." \
    "INSTALL ${RESOURCE_NAME} COMPLETE"
  set_install_phase installed-confirmed
  detach_and_delete_iso
}

guest_exec_json() {
  node_exec "$OWNER_NODE" qm guest exec "$VMID" --timeout 30 -- "$@"
}

print_manual_install_instructions() {
  cat <<EOF

Manual guest installation contract
----------------------------------
The configured ISO is attached unchanged. Before powering the installed guest
off, use its Proxmox console to configure all of the following:

  OS:             64-bit systemd-based Linux with Python 3
  Hostname:       ${RESOURCE_NAME}
  NIC MAC:        ${VM_MAC}
  NIC name:       lan0
  IPv4 address:   ${PROD_PRIVATE_IP}/24
  Default route:  ${GUEST_GATEWAY}
  DNS servers:    $(IFS=,; printf '%s' "${DNS_SERVERS[*]}")

Install and enable qemu-guest-agent and OpenSSH. Their systemd units must be
named qemu-guest-agent.service and ssh.service. Ensure /usr/bin/python3,
/bin/hostname, /bin/cat, resolvectl, and an Ed25519 SSH host key exist.

Enable the root account with the console password configured locally as
PROD_GUEST_VM_ROOT_PASSWORD. Put both of these keys in
/root/.ssh/authorized_keys:

  ${ADMIN_1_PUBLIC_SSH_KEY}
  ${ADMIN_2_PUBLIC_SSH_KEY}

Configure sshd for key-only root access:
  PermitRootLogin prohibit-password
  PasswordAuthentication no
  KbdInteractiveAuthentication no

Do not install Tailscale. Confirm QGA is responsive, SSH is running, and then
power the guest off. The creator will attest hostname, networking, QGA, the
SSH host key, and strict root SSH after you confirm installation.

Manual mode cannot attest the exact console password hash and cannot embed
startup.sh. It is intended for compatible Linux installers, not Windows or an
OS that cannot satisfy this contract.
EOF
}

verify_qga_and_root_ssh() {
  CURRENT_PHASE="verifying installed guest"
  locate_vm >/dev/null || die "Installed VM disappeared"
  if [[ "$VM_LIVE_STATUS" == stopped ]]; then
    node_exec "$OWNER_NODE" qm start "$VMID"
  elif [[ "$VM_LIVE_STATUS" != running ]]; then
    die "Installed VM has unexpected status $VM_LIVE_STATUS"
  fi
  wait_for_qga ||
    die "QEMU guest agent did not become ready within ${QGA_TIMEOUT_SECONDS}s"
  locate_vm >/dev/null

  if [[ "$PROD_GUEST_OS_INSTALL_MODE" == ubuntu-autoinstall ]]; then
    local shadow_json expected_shadow_digest
    expected_shadow_digest="$(
      printf '%s' "$ROOT_PASSWORD_HASH" | sha256sum | awk '{print $1}'
    )"
    shadow_json="$(
      guest_exec_json /usr/bin/python3 -c \
        'import hashlib
rows = [line.split(":", 2)[1] for line in open("/etc/shadow", encoding="utf-8") if line.startswith("root:")]
if len(rows) != 1:
    raise SystemExit("invalid root shadow rows")
print(hashlib.sha256(rows[0].encode("utf-8")).hexdigest())'
    )" || die "QGA could not attest the installed root shadow entry"
    if ! python3 - "$shadow_json" "$expected_shadow_digest" <<'PY'
import json
import re
import sys
value = json.loads(sys.argv[1])
digest = value.get("out-data", "").strip()
if (
    value.get("exited") != 1
    or value.get("exitcode") != 0
    or not re.fullmatch(r"[0-9a-f]{64}", digest)
    or digest != sys.argv[2]
):
    raise SystemExit("installed root shadow hash does not match the requested hash")
PY
    then
      die "Installed root console password hash failed QGA attestation"
    fi
  else
    warn "Manual install mode cannot attest the exact root console password hash"
  fi

  local hostname_json="" host_key_json="" host_key=""
  local combined_known_hosts base_known_hosts
  local guest_deadline=$((SECONDS + QGA_TIMEOUT_SECONDS))
  while ((SECONDS < guest_deadline)); do
    hostname_json="$(guest_exec_json /bin/hostname -s 2>/dev/null)" || {
      sleep 5
      continue
    }
    if ! python3 - "$hostname_json" "$RESOURCE_NAME" 2>/dev/null <<'PY'
import json
import sys
value = json.loads(sys.argv[1])
if value.get("exited") != 1 or value.get("exitcode") != 0:
    raise SystemExit("hostname QGA command failed")
if value.get("out-data", "").strip() != sys.argv[2]:
    raise SystemExit("guest hostname differs from registry")
PY
    then
      sleep 5
      continue
    fi

    host_key_json="$(guest_exec_json \
      /bin/cat /etc/ssh/ssh_host_ed25519_key.pub 2>/dev/null)" || {
      sleep 5
      continue
    }
    host_key="$(
      python3 - "$host_key_json" 2>/dev/null <<'PY'
import json
import re
import sys
value = json.loads(sys.argv[1])
key = value.get("out-data", "").strip()
if value.get("exited") != 1 or value.get("exitcode") != 0:
    raise SystemExit("host-key QGA command failed")
if not re.fullmatch(r"ssh-ed25519 [A-Za-z0-9+/=]+(?: [^\r\n]+)?", key):
    raise SystemExit("guest returned an invalid ed25519 host key")
print(key)
PY
    )" || host_key=""
    [[ -z "$host_key" ]] || break
    sleep 5
  done
  [[ -n "$host_key" ]] ||
    die "QGA did not expose the configured hostname and SSH host key in time"

  # Attest the SSH host key over QGA, then use that exact key for the private
  # address. This avoids disabled host-key checking or unauthenticated keyscan.
  combined_known_hosts="${RUN_DIR}/guest-known-hosts"
  base_known_hosts="${PROXMOX_SSH_KNOWN_HOSTS_FILE:-${HOME}/.ssh/known_hosts}"
  if [[ -f "$base_known_hosts" && ! -L "$base_known_hosts" ]]; then
    install -m 0600 "$base_known_hosts" "$combined_known_hosts"
  else
    : >"$combined_known_hosts"
    chmod 0600 "$combined_known_hosts"
  fi
  printf '%s %s\n%s %s\n' \
    "$PROD_PRIVATE_IP" "$host_key" \
    "$RESOURCE_NAME" "$host_key" >>"$combined_known_hosts"

  local original_known_hosts="${PROXMOX_SSH_KNOWN_HOSTS_FILE:-}"
  PROXMOX_SSH_KNOWN_HOSTS_FILE="$combined_known_hosts"
  export PROXMOX_SSH_KNOWN_HOSTS_FILE
  local deadline=$((SECONDS + QGA_TIMEOUT_SECONDS))
  until ssh_via_mox "$COORDINATOR" "root@${PROD_PRIVATE_IP}" true \
    </dev/null >/dev/null 2>&1; do
    ((SECONDS < deadline)) ||
      die "Key-only root SSH did not become ready through $COORDINATOR"
    sleep 10
  done

  local dns_csv
  dns_csv="$(IFS=,; printf '%s' "${DNS_SERVERS[*]}")"
  ssh_via_mox "$COORDINATOR" "root@${PROD_PRIVATE_IP}" \
    bash -s -- "$RESOURCE_NAME" "$PROD_PRIVATE_IP" "$GUEST_GATEWAY" \
    "$dns_csv" "$STARTUP_SHA256" "$PROD_GUEST_OS_INSTALL_MODE" <<'GUEST'
set -Eeuo pipefail
expected_name="$1"
expected_ip="$2"
expected_gateway="$3"
expected_dns_csv="$4"
expected_startup_sha256="$5"
install_mode="$6"

[[ "$(id -u)" == 0 ]]
[[ "$(hostname -s)" == "$expected_name" ]]
systemctl is-active --quiet qemu-guest-agent.service
systemctl is-active --quiet ssh.service
if [[ "$install_mode" == ubuntu-autoinstall ]]; then
  timeout 600 cloud-init status --wait
fi
ip -4 -o address show dev lan0 |
  awk -v address="${expected_ip}/24" '$4 == address { found=1 } END { exit !found }'
ip -4 route show default |
  awk -v gateway="$expected_gateway" '$3 == gateway { found=1 } END { exit !found }'
dns_status="$(resolvectl dns lan0)"
IFS=',' read -r -a expected_dns <<<"$expected_dns_csv"
for server in "${expected_dns[@]}"; do
  grep -Fq -- "$server" <<<"$dns_status"
done
sshd_policy="$(sshd -T)"
grep -Eq '^permitrootlogin (without-password|prohibit-password)$' <<<"$sshd_policy"
grep -Fxq 'passwordauthentication no' <<<"$sshd_policy"
grep -Fxq 'kbdinteractiveauthentication no' <<<"$sshd_policy"
! command -v tailscale >/dev/null 2>&1
if [[ "$expected_startup_sha256" == none ]]; then
  ! systemctl list-unit-files production-startup-once.service --no-legend |
    grep -q .
else
  systemctl is-enabled --quiet production-startup-once.service
  systemctl is-active --quiet production-startup-once.service
  test -f /var/lib/production-startup/completed
  observed_startup_sha256="$(
    sha256sum /usr/local/libexec/production-startup.sh | awk '{print $1}'
  )"
  [[ "$observed_startup_sha256" == "$expected_startup_sha256" ]]
fi
GUEST

  if [[ -n "$original_known_hosts" ]]; then
    PROXMOX_SSH_KNOWN_HOSTS_FILE="$original_known_hosts"
    export PROXMOX_SSH_KNOWN_HOSTS_FILE
  else
    unset PROXMOX_SSH_KNOWN_HOSTS_FILE
  fi
  if [[ "$INSTALL_PHASE" == installed-confirmed ]]; then
    set_install_phase guest-verified
  elif [[ "$INSTALL_PHASE" != guest-verified ]]; then
    die "Guest verification completed from unexpected install phase $INSTALL_PHASE"
  fi
}

apply_final_network_policy() {
  [[ "$INSTALL_PHASE" == guest-verified ]] ||
    die "Final networking may only be applied after guest verification"
  CURRENT_PHASE="applying final VM network policy"
  locate_vm >/dev/null || die "Verified VM disappeared before network finalization"
  local net0="virtio=${VM_MAC},bridge=${PROD_VM_BRIDGE},firewall=1"
  if [[ "$FINAL_NETWORK_ENABLED" == false ]]; then
    net0+=",link_down=1"
  fi
  # Attach the production lifecycle hook only after installer and QGA starts
  # are complete. Its destructive authority intentionally requires HA/ready
  # production state and must never intercept the OS installation boot.
  node_exec "$OWNER_NODE" qm set "$VMID" \
    --net0 "$net0" \
    --hookscript "$HOOK_REF"
  locate_vm >/dev/null || die "VM disappeared after network finalization"
  verify_vm_config
  set_install_phase network-finalized
  verify_vm_config
}

ensure_replicating_state() {
  case "$RESOURCE_STATE" in
    reserved | failed)
      registry_update --state provisioning \
        --owner-node "$OWNER_NODE" --volume-id "$ROOT_VOLUME"
      registry_update --state replicating
      ;;
    provisioning | stopped)
      registry_update --state replicating \
        --owner-node "$OWNER_NODE" --volume-id "$ROOT_VOLUME"
      ;;
    replicating | ready | active)
      registry_update --owner-node "$OWNER_NODE" --volume-id "$ROOT_VOLUME"
      ;;
    *) die "Cannot enter replication from registry state $RESOURCE_STATE" ;;
  esac
}

replication_config_target() {
  local config_file="$1" job_id="$2"
  python3 - "$config_file" "$job_id" <<'PY'
import json
import sys
with open(sys.argv[1], encoding="utf-8") as stream:
    rows = json.load(stream)
matches = [row for row in rows if row.get("id") == sys.argv[2]]
if len(matches) != 1:
    raise SystemExit(1)
if matches[0].get("disable") in (1, True, "1"):
    raise SystemExit("replication job is disabled")
print(matches[0].get("target", ""))
PY
}

ensure_replication_jobs() {
  CURRENT_PHASE="creating and verifying replicas"
  renew_orchestration_lease
  ensure_replicating_state
  locate_vm >/dev/null

  REPLICATION_TARGETS=()
  local node
  for node in "${PLACEMENT_NODES[@]}"; do
    [[ "$node" == "$OWNER_NODE" ]] ||
      REPLICATION_TARGETS+=("$node")
  done
  ((${#REPLICATION_TARGETS[@]} == ${#PLACEMENT_NODES[@]} - 1)) ||
    die "Replication targets plus owner do not equal placement"

  local config_file="${RUN_DIR}/replication-config.json"
  write_json "$config_file" "$(pvesh_get /cluster/replication)"
  local -a existing_jobs=()
  mapfile -t existing_jobs < <(
    python3 - "$config_file" "$VMID" <<'PY'
import json
import sys
with open(sys.argv[1], encoding="utf-8") as stream:
    rows = json.load(stream)
wanted = int(sys.argv[2])
for row in rows:
    if row.get("guest") in (wanted, str(wanted)):
        print(row["id"])
PY
  )

  declare -A job_for_target=()
  local job target configured_target status_json
  for job in "${existing_jobs[@]}"; do
    configured_target="$(replication_config_target "$config_file" "$job")" ||
      die "Could not validate replication job $job"
    status_json="$(pvesh_get \
      "/nodes/${OWNER_NODE}/replication/${job}/status" 2>/dev/null)" || true
    target=""
    if [[ -n "$status_json" ]]; then
      target="$(
        python3 - "$status_json" <<'PY'
import json
import re
import sys
target = json.loads(sys.argv[1]).get("target", "")
if not re.fullmatch(r"mox([1-9]|10)", str(target)):
    raise SystemExit(1)
print(target)
PY
      )" || target=""
    fi
    if [[ -z "$target" ]]; then
      target="$configured_target"
    fi
    [[ -n "$target" ]] || die "Replication job $job has no target"
    [[ -z "${job_for_target[$target]+x}" ]] ||
      die "More than one replication job targets $target"
    job_for_target["$target"]="$job"
  done
  for target in "${!job_for_target[@]}"; do
    local expected=false
    for node in "${REPLICATION_TARGETS[@]}"; do
      [[ "$target" != "$node" ]] || expected=true
    done
    [[ "$expected" == true ]] ||
      die "Replication job targets $target outside placement minus owner"
  done

  local index=0 schedule="*/${REPLICATION_MINUTES}"
  declare -A baseline_for_job=()
  declare -A new_job=()
  for target in "${REPLICATION_TARGETS[@]}"; do
    if [[ -z "${job_for_target[$target]+x}" ]]; then
      while :; do
        job="${VMID}-${index}"
        local collision=false existing
        for existing in "${existing_jobs[@]}"; do
          [[ "$existing" != "$job" ]] || collision=true
        done
        [[ "$collision" == true ]] || break
        ((index += 1))
      done
      # pvesr create-local-job must run on the VM's current source node.
      node_exec "$OWNER_NODE" pvesr create-local-job "$job" "$target" \
        --schedule "$schedule" \
        --comment "Production ${RESOURCE_NAME} replica"
      existing_jobs+=("$job")
      job_for_target["$target"]="$job"
      new_job["$job"]=true
      ((index += 1))
    fi
  done

  # Re-read cluster configuration and require exactly placement-minus-owner.
  write_json "$config_file" "$(pvesh_get /cluster/replication)"
  for target in "${REPLICATION_TARGETS[@]}"; do
    job="${job_for_target[$target]}"
    local observed_schedule
    observed_schedule="$(
      python3 - "$config_file" "$job" <<'PY'
import json
import sys
with open(sys.argv[1], encoding="utf-8") as stream:
    rows = json.load(stream)
row = next((row for row in rows if row.get("id") == sys.argv[2]), None)
if row is None:
    raise SystemExit("replication job disappeared")
print(row.get("schedule", ""))
PY
    )"
    if [[ "$observed_schedule" != "$schedule" ]]; then
      node_exec "$OWNER_NODE" pvesr update "$job" --schedule "$schedule"
    fi
    if [[ "${new_job[$job]-false}" == true ]]; then
      # PVE may immediately launch the first replication after job creation.
      # Its baseline remains zero even if that automatic run completes before
      # the status endpoint is first observed.
      baseline_for_job["$job"]=0
    else
      status_json="$(pvesh_get \
        "/nodes/${OWNER_NODE}/replication/${job}/status" 2>/dev/null)" || true
      baseline_for_job["$job"]="$(
        python3 - "$status_json" <<'PY'
import json
import sys
if not sys.argv[1]:
    print(0)
else:
    value = json.loads(sys.argv[1]).get("last_sync", 0)
    print(value if isinstance(value, int) and value >= 0 else 0)
PY
      )" || die "Could not capture replication baseline for $job"
    fi
  done

  for target in "${REPLICATION_TARGETS[@]}"; do
    job="${job_for_target[$target]}"
    local deadline=$((SECONDS + REPLICATION_TIMEOUT_SECONDS))
    local schedule_requested=false wait_notice_printed=false
    while ((SECONDS < deadline)); do
      local status_json=""
      status_json="$(pvesh_get \
        "/nodes/${OWNER_NODE}/replication/${job}/status" 2>/dev/null)" || true
      if [[ -n "$status_json" ]] &&
        python3 - "$status_json" "$job" "$target" \
          "${baseline_for_job[$job]}" <<'PY'
import json
import sys
row = json.loads(sys.argv[1])
ok = (
    row.get("id") == sys.argv[2]
    and row.get("target") == sys.argv[3]
    and isinstance(row.get("last_sync"), int)
    and row["last_sync"] > int(sys.argv[4])
    and int(row.get("fail_count", 0)) == 0
    and not row.get("error")
    and not row.get("pid")
)
raise SystemExit(0 if ok else 1)
PY
      then
        info "Replica $job is healthy on $target"
        break
      fi

      local replication_running=false
      if [[ -n "$status_json" ]] &&
        python3 - "$status_json" <<'PY'
import json
import sys
row = json.loads(sys.argv[1])
raise SystemExit(0 if row.get("pid") else 1)
PY
      then
        replication_running=true
      fi
      if [[ "$replication_running" == true ]]; then
        if [[ "$wait_notice_printed" == false ]]; then
          info "Replication $job is already running; waiting up to ${REPLICATION_TIMEOUT_SECONDS} seconds for it to finish"
          wait_notice_printed=true
        fi
      elif [[ "$schedule_requested" == false ]]; then
        # An automatic or timer-started replication may briefly own the
        # per-VM migration lock. Treat that lock as transient and retry while
        # continuing to observe any sync another process already started.
        if node_exec "$OWNER_NODE" pvesr schedule-now "$job" \
          >/dev/null 2>&1; then
          schedule_requested=true
          info "Requested replication $job; waiting for a newer successful sync"
        else
          info "Replication $job is temporarily busy; waiting before retrying"
        fi
      fi
      sleep 15
    done
    ((SECONDS < deadline)) ||
      die "Timed out waiting for healthy initial replication to $target"
  done

  local union_csv placement_csv
  union_csv="$(printf '%s\n' "$OWNER_NODE" "${REPLICATION_TARGETS[@]}" |
    LC_ALL=C sort | paste -sd, -)"
  placement_csv="$(printf '%s\n' "${PLACEMENT_NODES[@]}" |
    LC_ALL=C sort | paste -sd, -)"
  [[ "$union_csv" == "$placement_csv" ]] ||
    die "Healthy replica targets plus owner differ from registered placement"

  # ZFS receive must preserve the exact source zvol size and allocation policy.
  for node in "${PLACEMENT_NODES[@]}"; do
    validate_disk_allocation "$node"
  done
}

validate_pvecm_status_file() {
  python3 - "$1" "$2" "$3" <<'PY'
import re
import sys

text = open(sys.argv[1], encoding="utf-8").read()
cluster_name = re.search(r"^Name:\s+(\S+)\s*$", text, re.M)
nodes = re.search(r"^Nodes:\s+([0-9]+)\s*$", text, re.M)
expected = re.search(r"^Expected votes:\s+([0-9]+)\s*$", text, re.M)
total = re.search(r"^Total votes:\s+([0-9]+)\s*$", text, re.M)
configured_count = int(sys.argv[3])
if (
    not 2 <= configured_count <= 10
    or not cluster_name
    or cluster_name.group(1) != sys.argv[2]
    or not nodes
    or int(nodes.group(1)) != configured_count
    or not expected
    or not total
    or not re.search(r"^Quorate:\s+Yes\s*$", text, re.M)
):
    raise SystemExit("cluster votes or identity are inconsistent")
node_count = int(nodes.group(1))
voters = re.findall(
    r"^0x([0-9a-fA-F]+)\s+([0-9]+)\s+(\S+)\s+"
    r"(\S+)",
    text,
    re.M,
)
node_ids = [int(node_id, 16) for node_id, _, _, _ in voters]
if (
    len(voters) != node_count
    or len(node_ids) != len(set(node_ids))
    or any(node_id == 0 for node_id in node_ids)
    or any(votes != "1" for _, votes, _, _ in voters)
    or any(
        not {"A", "V"}.issubset(set(flags.split(",")))
        for _, _, flags, _ in voters
    )
):
    raise SystemExit("every configured mox voter must be uniquely alive and voting")

flags_rows = re.findall(r"^Flags:\s*(.*?)\s*$", text, re.M)
if len(flags_rows) != 1:
    raise SystemExit("cluster status must contain exactly one Flags row")
has_qdevice_flag = "Qdevice" in flags_rows[0].split()
qdevice_rows = re.findall(
    r"^0x([0-9a-fA-F]+)\s+([0-9]+)\s+Qdevice\s*$",
    text,
    re.M,
)
if node_count % 2 == 0:
    required_votes = node_count + 1
    if (
        int(expected.group(1)) != required_votes
        or int(total.group(1)) != required_votes
        or not has_qdevice_flag
        or qdevice_rows != [("00000000", "1")]
    ):
        raise SystemExit(
            "an even-node HA cluster requires exactly one alive/voting QDevice"
        )
else:
    if (
        int(expected.group(1)) != node_count
        or int(total.group(1)) != node_count
        or has_qdevice_flag
        or qdevice_rows
    ):
        raise SystemExit(
            "an odd-node HA cluster requires native votes and no QDevice"
        )
PY
}

revalidate_placement_online() {
  local nodes_file="${RUN_DIR}/nodes-before-ha.json"
  local cluster_file="${RUN_DIR}/cluster-before-ha.json"
  local online_file="${RUN_DIR}/online-before-ha" node
  write_json "$cluster_file" "$(pvesh_get /cluster/status)"
  python3 - "$cluster_file" "$PROXMOX_CLUSTER_NAME" <<'PY'
import json
import sys
with open(sys.argv[1], encoding="utf-8") as stream:
    rows = json.load(stream)
cluster = [row for row in rows if row.get("type") == "cluster"]
if (
    len(cluster) != 1
    or cluster[0].get("name") != sys.argv[2]
    or cluster[0].get("quorate") not in (1, True, "1")
):
    raise SystemExit("cluster lost its expected quorate identity")
PY
  write_json "$nodes_file" "$(pvesh_get /nodes)"
  parse_online_nodes_json "$nodes_file" "$MAX_MOX_HOSTS" >"$online_file" ||
    die "Cluster membership is no longer eligible for production HA"
  local -a still_online=()
  mapfile -t still_online <"$online_file"
  for node in "${PLACEMENT_NODES[@]}"; do
    local found=false candidate
    for candidate in "${still_online[@]}"; do
      [[ "$candidate" != "$node" ]] || found=true
    done
    [[ "$found" == true ]] ||
      die "Placement node $node went offline before HA enablement"
  done

  local status_file
  for node in "${PLACEMENT_NODES[@]}"; do
    status_file="${RUN_DIR}/pvecm-${node}-before-ha.txt"
    node_exec "$node" pvecm status >"$status_file" ||
      die "Could not validate Corosync votes from $node before HA"
    if ! validate_pvecm_status_file "$status_file" \
      "$PROXMOX_CLUSTER_NAME" "${#still_online[@]}"; then
      die "$node does not report healthy votes required for production HA"
    fi
  done
}

verify_effective_replication_targets() {
  local config_file="${RUN_DIR}/replication-after-ha.json"
  local deadline=$((SECONDS + 600)) expected_csv actual_csv job status_json target node
  local -a jobs=() expected_targets=() actual_targets=()

  for node in "${PLACEMENT_NODES[@]}"; do
    [[ "$node" == "$OWNER_NODE" ]] || expected_targets+=("$node")
  done
  expected_csv="$(printf '%s\n' "${expected_targets[@]}" |
    LC_ALL=C sort | paste -sd, -)"

  write_json "$config_file" "$(pvesh_get /cluster/replication)"
  mapfile -t jobs < <(
    python3 - "$config_file" "$VMID" <<'PY'
import json
import sys
with open(sys.argv[1], encoding="utf-8") as stream:
    rows = json.load(stream)
wanted = int(sys.argv[2])
for row in rows:
    if row.get("guest") in (wanted, str(wanted)):
        print(row["id"])
PY
  )
  ((${#jobs[@]} == ${#expected_targets[@]})) ||
    die "Replication job count no longer matches placement minus HA owner"

  while ((SECONDS < deadline)); do
    actual_targets=()
    for job in "${jobs[@]}"; do
      status_json="$(pvesh_get \
        "/nodes/${OWNER_NODE}/replication/${job}/status" 2>/dev/null)" || {
        actual_targets=()
        break
      }
      target="$(
        python3 - "$status_json" <<'PY'
import json
import sys
row = json.loads(sys.argv[1])
if (
    not isinstance(row.get("last_sync"), int)
    or row["last_sync"] <= 0
    or int(row.get("fail_count", 0)) != 0
    or row.get("error")
    or row.get("pid")
):
    raise SystemExit(1)
print(row.get("target", ""))
PY
      )" || {
        actual_targets=()
        break
      }
      [[ "$target" =~ ^mox([1-9]|10)$ ]] || {
        actual_targets=()
        break
      }
      actual_targets+=("$target")
    done
    if ((${#actual_targets[@]} == ${#expected_targets[@]})); then
      actual_csv="$(printf '%s\n' "${actual_targets[@]}" |
        LC_ALL=C sort -u | paste -sd, -)"
      if [[ "$actual_csv" == "$expected_csv" ]]; then
        REPLICATION_TARGETS=("${expected_targets[@]}")
        return 0
      fi
    fi
    sleep 10
  done
  die "Healthy effective replication targets do not equal placement minus HA owner"
}

verify_or_create_affinity_rule() {
  local rules_file="${RUN_DIR}/ha-rules.json" disposition
  HA_RULE_ID="production-${RESOURCE_NAME}-${VMID}"
  write_json "$rules_file" "$(
    node_exec "$COORDINATOR" ha-manager rules config --output-format json
  )"
  disposition="$(
    python3 - "$rules_file" "$HA_RULE_ID" "vm:${VMID}" \
      "$AFFINITY_NODES_CSV" <<'PY'
import json
import sys

with open(sys.argv[1], encoding="utf-8") as stream:
    rules = json.load(stream)
wanted_id, sid = sys.argv[2:4]
wanted_nodes = {}
for entry in sys.argv[4].split(","):
    node, separator, priority = entry.partition(":")
    wanted_nodes[node] = int(priority) if separator else 0

def values(raw):
    if isinstance(raw, list):
        return raw
    return [part for part in str(raw or "").split(",") if part]

match = None
for rule in rules:
    rule_id = rule.get("rule", rule.get("id"))
    resources = set(values(rule.get("resources")))
    if rule_id == wanted_id and sid not in resources:
        raise SystemExit(f"HA rule ID {rule_id} belongs to another resource")
    if sid in resources and rule_id != wanted_id:
        raise SystemExit(f"{sid} is already referenced by HA rule {rule_id}")
    if sid in resources and rule_id == wanted_id:
        if match is not None:
            raise SystemExit(f"{sid} is referenced by more than one HA rule")
        match = rule
if match is None:
    print("missing")
    raise SystemExit
nodes = {}
for entry in values(match.get("nodes")):
    node, separator, priority = entry.partition(":")
    nodes[node] = int(priority) if separator else 0
strict = match.get("strict", 0) in (1, True, "1")
affinity = match.get("affinity", "positive")
disabled = match.get("disable", 0) in (1, True, "1")
enabled = match.get("enabled", 1) in (1, True, "1")
if (
    match.get("type") not in (None, "node-affinity")
    or set(values(match.get("resources"))) != {sid}
    or nodes != wanted_nodes
    or not strict
    or affinity != "positive"
    or disabled
    or not enabled
):
    raise SystemExit("existing HA affinity rule differs from the required strict rule")
print("ok\t" + str(match.get("rule", match.get("id"))))
PY
  )" || die "HA rule preflight failed"
  if [[ "$disposition" == missing ]]; then
    # PVE 9 node-affinity rules replace legacy HA groups. Strict positive
    # affinity prevents HA from ever placing the VM outside this exact set.
    node_exec "$COORDINATOR" ha-manager rules add node-affinity "$HA_RULE_ID" \
      --resources "vm:${VMID}" \
      --nodes "$AFFINITY_NODES_CSV" \
      --strict 1
    verify_or_create_affinity_rule
  elif [[ "$disposition" == ok$'\t'* ]]; then
    HA_RULE_ID="${disposition#*$'\t'}"
  else
    die "Could not parse HA rule validation result"
  fi
}

ensure_ha_resource() {
  CURRENT_PHASE="enabling strict Proxmox HA"
  renew_orchestration_lease
  revalidate_placement_online
  local resources_file="${RUN_DIR}/ha-resources.json" existing
  write_json "$resources_file" "$(pvesh_get /cluster/ha/resources)"
  existing="$(
    python3 - "$resources_file" "vm:${VMID}" <<'PY'
import json
import sys
with open(sys.argv[1], encoding="utf-8") as stream:
    rows = json.load(stream)
matches = [row for row in rows if row.get("sid") == sys.argv[2]]
if len(matches) > 1:
    raise SystemExit("duplicate HA resource")
if matches:
    if matches[0].get("group"):
        raise SystemExit("HA resource unexpectedly uses a legacy HA group")
    if (
        int(matches[0].get("max_restart", -1)) != 3
        or int(matches[0].get("max_relocate", -1)) != 1
        or int(matches[0].get("failback", -1)) != 0
        or int(matches[0].get("auto-rebalance", -1)) != 0
    ):
        raise SystemExit("HA restart/relocate/mobility policy differs from contract")
    state = matches[0].get("state")
    if state not in {"started", "ignored"}:
        raise SystemExit("HA requested state differs from contract")
    print(state)
PY
  )" || die "Could not inspect HA resources"
  if [[ -z "$existing" ]]; then
    # Rules should reference an existing HA resource. "ignored" records it
    # without letting CRM/LRM stop, move, or restart the already-running VM.
    node_exec "$COORDINATOR" ha-manager add "vm:${VMID}" \
      --state ignored --max_restart 3 --max_relocate 1 \
      --failback 0 --auto-rebalance 0
    existing=ignored
  elif [[ "$existing" != started && "$existing" != ignored ]]; then
    die "Existing HA resource has unsafe requested state: $existing"
  fi

  verify_or_create_affinity_rule
  if [[ "$existing" == ignored ]]; then
    node_exec "$COORDINATOR" ha-manager set "vm:${VMID}" \
      --state started --failback 0 --auto-rebalance 0
  fi

  local deadline=$((SECONDS + 300)) status_file="${RUN_DIR}/ha-status.txt"
  while ((SECONDS < deadline)); do
    node_exec "$COORDINATOR" ha-manager status >"$status_file"
    if python3 - "$status_file" "vm:${VMID}" \
      "$(IFS=,; printf '%s' "${PLACEMENT_NODES[*]}")" <<'PY'
import re
import sys
text = open(sys.argv[1], encoding="utf-8").read()
sid = re.escape(sys.argv[2])
nodes = set(sys.argv[3].split(","))
match = re.search(rf"^service {sid} \((mox(?:[1-9]|10)), started\)$", text, re.M)
raise SystemExit(0 if match and match.group(1) in nodes else 1)
PY
    then
      break
    fi
    sleep 10
  done
  ((SECONDS < deadline)) ||
    die "HA did not report vm:${VMID} started on an eligible node"
  locate_vm >/dev/null
  verify_effective_replication_targets
}

register_routes_and_finalize() {
  CURRENT_PHASE="registering ingress and finalizing guest"
  local placement_csv targets_csv
  placement_csv="$(IFS=,; printf '%s' "${PLACEMENT_NODES[*]}")"
  targets_csv="$(IFS=,; printf '%s' "${REPLICATION_TARGETS[*]}")"

  case "$RESOURCE_STATE" in
    replicating)
      registry_update \
        --state ready \
        --owner-node "$OWNER_NODE" \
        --volume-id "$ROOT_VOLUME" \
        --replication-targets "$targets_csv" \
        --ha-nodes "$placement_csv" \
        --routes-enabled
      ;;
    ready | active)
      registry_update \
        --owner-node "$OWNER_NODE" \
        --volume-id "$ROOT_VOLUME" \
        --replication-targets "$targets_csv" \
        --ha-nodes "$placement_csv" \
        --routes-enabled
      ;;
    *)
      die "Cannot finalize production guest from state $RESOURCE_STATE"
      ;;
  esac

  # Publish the requested Host/SNI mapping even before an application is
  # deployed. HAProxy will correctly report the backend unavailable meanwhile.
  info "Waiting up to 240 seconds for any current HAProxy route sync to finish, then converging production ingress."
  mox_ssh "$COORDINATOR" "$REMOTE_HAPROXY_SYNC" \
    --lock-timeout 240 </dev/null
  refresh_resource
  [[ "$ROUTES_ENABLED" == 1 ]] ||
    die "Production guest ingress mapping was not enabled"
  if [[ "$RESOURCE_STATE" == ready ]]; then
    registry_update --state active --owner-node "$OWNER_NODE"
  elif [[ "$RESOURCE_STATE" != active ]]; then
    die "Unexpected final registry state: $RESOURCE_STATE"
  fi
}

setup_workstation_jump_ssh() {
  prompt_yes "Set up permanent jump SSH from this workstation to $RESOURCE_NAME?" ||
    return 0

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

  local guest_key_json guest_key guest_key_core guest_fingerprint
  guest_key_json="$(
    node_exec "$OWNER_NODE" qm guest exec "$VMID" --timeout 30 -- \
      /bin/cat /etc/ssh/ssh_host_ed25519_key.pub
  )" || {
    warn "Failed to obtain the guest Ed25519 host key through QGA"
    return 0
  }
  guest_key="$(
    python3 - "$guest_key_json" <<'PY'
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
  )" || {
    warn "QGA returned an invalid guest Ed25519 host key"
    return 0
  }
  guest_key_core="$(awk '{print $1 " " $2}' <<<"$guest_key")"
  guest_fingerprint="$(
    printf '%s\n' "$guest_key_core" | ssh-keygen -lf - | awk '{print $2}'
  )" || {
    warn "Could not fingerprint the QGA-attested guest host key"
    return 0
  }
  info "QGA-attested guest Ed25519 fingerprint: $guest_fingerprint"

  local ssh_alias alias_used config_used known_used
  local effective existing_keys existing_key fingerprint
  local effective_proxyjump effective_hostkeyalias
  while true; do
    prompt_with_default ssh_alias \
      "Workstation SSH alias for ${RESOURCE_NAME}" "$RESOURCE_NAME"
    [[ "$ssh_alias" =~ ^[A-Za-z0-9][A-Za-z0-9_.-]{0,63}$ ]] || {
      warn "SSH alias must use 1-64 letters, digits, dots, underscores, or hyphens"
      continue
    }

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
      grep -Fxq "hostname $PROD_PRIVATE_IP" <<<"$effective" &&
      grep -Fxq "user root" <<<"$effective" &&
      grep -Fxq "proxyjump $COORDINATOR" <<<"$effective" &&
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
        info "Choose a different SSH alias"
        continue
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
    "$PROD_PRIVATE_IP" "$COORDINATOR" <<'PY'
from pathlib import Path
import sys

source, destination, alias, address, jump = sys.argv[1:]
begin = f"# BEGIN app-ha managed production guest {alias}"
end = f"# END app-ha managed production guest {alias}"
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
  # Prepend the exact managed block so first-value-wins OpenSSH semantics
  # override any older matching stanza without deleting unrelated user config.
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

print_completion() {
  log "Production VM creation complete"
  info "VM name: $RESOURCE_NAME (VMID $VMID)"
  info "Private IP: ${PROD_PRIVATE_IP}/24 via $GUEST_GATEWAY"
  info "Primary FQDN: $PRIMARY_DOMAIN"
  info "Aliases: ${ALIAS_DOMAINS[*]:-none}"
  info "Default staging names: stageN${RESOURCE_NAME:-prodN}.${PRIMARY_DOMAIN}"
  info "HA placement: ${PLACEMENT_NODES[*]}"
  info "Preferred startup node: $INITIAL_NODE"
  info "Root volume: $ROOT_VOLUME ($DISK_ALLOCATION)"
  info "CPU/RAM/disk: ${VM_CORES} cores / ${VM_MEMORY_GIB} GiB / ${VM_DISK_GIB} GiB"
  info "Replication interval: */$REPLICATION_MINUTES"
  info "Guest OS install mode: $PROD_GUEST_OS_INSTALL_MODE"
  info "Final VM networking: $FINAL_NETWORK_ENABLED"
  info "startup.sh: ${STARTUP_SHA256/none/not configured}"
  locate_vm >/dev/null || die "Completed VM disappeared before final summary"
  info "Current VM power state: $VM_LIVE_STATUS"
  info "Proxmox HA requested state: started"
  if [[ "$VM_LIVE_STATUS" == running ]]; then
    info "No manual power-on is required; HA is managing the running VM."
  else
    warn "HA requested started but the VM is $VM_LIVE_STATUS; inspect HA status rather than using qm start directly"
  fi
  if node_exec "$INITIAL_NODE" test -f "$SOURCE_ISO_CACHE_PATH" &&
    node_exec "$INITIAL_NODE" test -f "$SOURCE_ISO_SHA256_PATH"; then
    info "Retained source ISO cache: ${INITIAL_NODE}:${SOURCE_ISO_CACHE_PATH}"
    info "Retained source checksum: ${INITIAL_NODE}:${SOURCE_ISO_SHA256_PATH}"
    info "The verified source cache may be removed manually when it is no longer needed."
  else
    warn "The source ISO cache was removed before completion and is no longer reusable"
  fi
  warn "HAProxy ingress is enabled, but the backend may be unavailable until application deployment and health checks complete"

  printf '\nPublic origin IPs (preferred node first):\n'
  local node public_ip
  local -a ordered_nodes=("$INITIAL_NODE")
  for node in "${PLACEMENT_NODES[@]}"; do
    [[ "$node" == "$INITIAL_NODE" ]] || ordered_nodes+=("$node")
  done
  for node in "${ordered_nodes[@]}"; do
    public_ip="$(public_ip_for_node "$node")" ||
      die "Could not load the public IP contract for $node"
    if [[ "$node" == "$INITIAL_NODE" ]]; then
      printf '  %-8s %s  (initial/preferred)\n' "$node" "$public_ip"
    else
      printf '  %-8s %s\n' "$node" "$public_ip"
    fi
  done

  cat <<EOF

Ingress and DNS guidance:
  HAProxy Host/SNI mappings now include ${PRIMARY_DOMAIN}${ALIAS_DOMAINS[*]:+ and ${ALIAS_DOMAINS[*]}}.
  The backend can return unavailable until an application is deployed.
  Reserve stageN${RESOURCE_NAME}.${PRIMARY_DOMAIN} names for staging guests
  derived from ${RESOURCE_NAME}; for example stage1${RESOURCE_NAME}.${PRIMARY_DOMAIN}.

External load balancer/DNS work:
  1. Create/confirm one origin for each public IP above on TCP 443 (and TCP 80
     while ACME HTTP-01 is needed).
  2. Use ${PRIMARY_DOMAIN} as the monitor Host header and keep Full (strict)
     origin TLS once the guest certificate is installed.
  3. Point ${PRIMARY_DOMAIN}${ALIAS_DOMAINS[*]:+ and aliases ${ALIAS_DOMAINS[*]}} at that pool.
  4. Confirm both origins traverse their local HAProxy LXC and reach
     ${PROD_PRIVATE_IP}; Cloudflare configuration is intentionally not changed
     by this script.
EOF
  setup_workstation_jump_ssh
}

main() {
  parse_args "$@"
  require_command_local awk
  require_command_local openssl
  require_openssl_sha512_crypt
  require_command_local sha256sum
  load_and_validate_config
  preflight_workstation
  install_traps
  select_coordinator_and_nodes

  if reserve_or_resume_resource; then
    :
  else
    local status=$?
    if ((status == 10)); then
      log "Dry run complete"
      return 0
    fi
    return "$status"
  fi

  acquire_orchestration_lease
  preflight_placement
  install_or_resume_os
  if [[ "$INSTALL_PHASE" == installed-confirmed ]]; then
    verify_qga_and_root_ssh
  elif [[ "$INSTALL_PHASE" != guest-verified &&
    "$INSTALL_PHASE" != network-finalized ]]; then
    die "Unexpected post-install phase: $INSTALL_PHASE"
  fi
  if [[ "$INSTALL_PHASE" == guest-verified ]]; then
    apply_final_network_policy
  fi
  ensure_replication_jobs
  ensure_ha_resource
  register_routes_and_finalize
  print_completion
}

if [[ "${PRODUCTION_VM_SOURCE_ONLY:-0}" != 1 ]]; then
  main "$@"
fi
