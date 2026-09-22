#!/usr/bin/env bash

# Copyright (c) 2026 BEENTHERE VENTURES, INC.
# SPDX-License-Identifier: GPL-3.0-only

# Quorum-gated, crash-recoverable synchronization of registry-backed HAProxy
# generations across fixed per-node ingress LXCs.

set -Eeuo pipefail
set +x
umask 077

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
CONFIG_LIB="${SCRIPT_DIR}/config.sh"
REGISTRY="${SCRIPT_DIR}/cluster_registry.py"
RENDERER="${SCRIPT_DIR}/haproxy_routes.py"
INSTALLED_SYNC="/usr/local/lib/app-ha-proxmox/lib/sync_haproxy_routes.sh"
MAX_RETAINED_GENERATIONS=4
MODE=delegate
LOCK_WAIT_SECONDS=20
COORDINATOR_NODES_JSON=""
WORK_DIR=""
TRANSACTION=""
LOCAL_FAIL_CLOSED_ARMED=false
declare -a TRANSACTION_NODES=()
declare -a TRANSACTION_VMIDS=()

log() {
  printf '[app-ha ingress] %s\n' "$*"
}

die() {
  printf 'ERROR: %s\n' "$*" >&2
  exit 1
}

usage() {
  cat <<EOF
Usage: $0 [--coordinator-local | --local-reconcile | --assert-local-current]
          [--lock-timeout SECONDS]

Loads cluster.conf through config.sh without reading secrets.env. Desired
routes are read only by the lowest-numbered online, quorate mox coordinator.
The desired generation and per-node application state are persisted in
pmxcfs before any live HAProxy configuration is committed.

  --coordinator-local    Run the coordinator transaction on this mox.
  --local-reconcile      Fail closed locally, then converge through coordinator.
  --assert-local-current Fail closed unless this mox serves desired generation.
  --lock-timeout SECONDS Wait this long for the coordinator transaction lock
                         (default: 20; setup-time callers use 240).
EOF
}

parse_args() {
  while (($#)); do
    case "$1" in
      --coordinator-local)
        [[ "$MODE" == delegate ]] || die "Choose only one synchronization mode"
        MODE=coordinator
        ;;
      --local-reconcile)
        [[ "$MODE" == delegate ]] || die "Choose only one synchronization mode"
        MODE=local-reconcile
        ;;
      --assert-local-current)
        [[ "$MODE" == delegate ]] || die "Choose only one synchronization mode"
        MODE=assert-local-current
        ;;
      --lock-timeout)
        (($# >= 2)) || die "--lock-timeout requires a value"
        [[ "$2" =~ ^[1-9][0-9]{0,2}$ ]] ||
          die "--lock-timeout must be an integer from 1 through 600"
        ((10#$2 <= 600)) ||
          die "--lock-timeout must be an integer from 1 through 600"
        LOCK_WAIT_SECONDS="$((10#$2))"
        shift 2
        continue
        ;;
      -h | --help)
        usage
        exit 0
        ;;
      *)
        die "Unknown option: $1"
        ;;
    esac
    shift
  done
}

require_file() {
  [[ -f "$1" && ! -L "$1" ]] ||
    die "Required regular file is missing or unsafe: $1"
}

require_tools() {
  local command_name
  for command_name in "$@"; do
    command -v "$command_name" >/dev/null 2>&1 ||
      die "Required command is unavailable: $command_name"
  done
}

validate_node() {
  local node="$1" index
  [[ "$node" =~ ^mox([1-9]|10)$ ]] ||
    die "Unsafe mox node argument: ${node}"
  index="${node#mox}"
  ((index <= MAX_MOX_HOSTS)) ||
    die "${node} exceeds MAX_MOX_HOSTS=${MAX_MOX_HOSTS}"
}

validate_vmid() {
  [[ "$1" =~ ^91(1[1-9]|20)$ ]] ||
    die "Unsafe fixed HAProxy VMID argument: $1"
}

validate_generation() {
  [[ "$1" =~ ^[0-9a-f]{64}$ ]] ||
    die "Unsafe HAProxy generation argument"
}

validate_digest() {
  [[ "$1" =~ ^[0-9a-f]{64}$ ]] ||
    die "Unsafe SHA-256 argument"
}

validate_transaction() {
  [[ "$1" =~ ^[A-Za-z0-9][A-Za-z0-9._-]{0,127}$ ]] ||
    die "Unsafe route transaction argument"
}

validate_interface() {
  [[ "$1" =~ ^[A-Za-z0-9][A-Za-z0-9_.-]{0,14}$ ]] ||
    die "Unsafe Linux interface argument: $1"
}

validate_ipv4() {
  python3 - "$1" <<'PY'
import ipaddress
import sys

value = sys.argv[1]
parsed = ipaddress.ip_address(value)
if not isinstance(parsed, ipaddress.IPv4Address) or str(parsed) != value:
    raise SystemExit(1)
PY
}

validate_ipv4_cidr() {
  python3 - "$1" <<'PY'
import ipaddress
import sys

value = sys.argv[1]
parsed = ipaddress.ip_interface(value)
if (
    not isinstance(parsed, ipaddress.IPv4Interface)
    or str(parsed) != value
    or parsed.network.prefixlen != 24
):
    raise SystemExit(1)
PY
}

online_nodes_json() {
  pvesh get /nodes --output-format json
}

cluster_status_json() {
  pvesh get /cluster/status --output-format json
}

node_is_online() {
  local node="$1" nodes_json="$2"
  validate_node "$node"
  jq -e --arg node "$node" '
    any(.[]; .node == $node and .status == "online")
  ' <<<"$nodes_json" >/dev/null
}

validate_nodes_json() {
  local nodes_json="$1" unexpected
  jq -e 'type == "array"' <<<"$nodes_json" >/dev/null ||
    die "Proxmox /nodes did not return an array"
  unexpected="$(
    jq -r --argjson maximum "$MAX_MOX_HOSTS" '
      [
        .[] |
        select(
          (.node | type) != "string" or
          (.node | test("^mox([1-9]|10)$") | not) or
          ((.node | sub("^mox"; "") | tonumber) > $maximum)
        ) |
        (.node // "<missing>")
      ] | sort | join(", ")
    ' <<<"$nodes_json"
  )" || die "Could not validate Proxmox node membership"
  [[ -z "$unexpected" ]] ||
    die "Proxmox nodes do not match mox1..mox${MAX_MOX_HOSTS}: ${unexpected}"
}

first_online_node_from_json() {
  local nodes_json="$1" index
  validate_nodes_json "$nodes_json"
  for ((index = 1; index <= MAX_MOX_HOSTS; index += 1)); do
    if node_is_online "mox${index}" "$nodes_json"; then
      printf 'mox%s\n' "$index"
      return 0
    fi
  done
  return 1
}

assert_quorate_cluster_json() {
  local status_json="$1"
  jq -e --arg name "$PROXMOX_CLUSTER_NAME" '
    type == "array" and
    ([.[] | select(.type == "cluster")] | length) == 1 and
    ([.[] | select(.type == "cluster")][0].name == $name) and
    (
      [.[] | select(.type == "cluster")][0].quorate == 1 or
      [.[] | select(.type == "cluster")][0].quorate == true or
      [.[] | select(.type == "cluster")][0].quorate == "1"
    )
  ' <<<"$status_json" >/dev/null
}

assert_local_node_online_and_quorate() {
  local local_node nodes_json status_json
  local_node="$(hostname -s)"
  validate_node "$local_node"
  nodes_json="$(online_nodes_json)" ||
    return 1
  validate_nodes_json "$nodes_json"
  node_is_online "$local_node" "$nodes_json" ||
    return 1
  status_json="$(cluster_status_json)" ||
    return 1
  assert_quorate_cluster_json "$status_json" ||
    return 1
  COORDINATOR_NODES_JSON="$nodes_json"
}

assert_coordinator_online_and_quorate() {
  local expected="$1" local_node coordinator status_json nodes_json
  validate_node "$expected"
  local_node="$(hostname -s)"
  [[ "$local_node" == "$expected" ]] ||
    die "Coordinator transaction is running on ${local_node}, expected ${expected}"
  nodes_json="$(online_nodes_json)" ||
    die "Could not query Proxmox node liveness"
  validate_nodes_json "$nodes_json"
  node_is_online "$expected" "$nodes_json" ||
    die "Coordinator ${expected} is not an online Proxmox node"
  coordinator="$(first_online_node_from_json "$nodes_json")" ||
    die "No online mox coordinator was found"
  [[ "$coordinator" == "$expected" ]] ||
    die "Coordinator changed to ${coordinator}; refusing stale coordination"
  status_json="$(cluster_status_json)" ||
    die "Could not query Proxmox cluster quorum"
  assert_quorate_cluster_json "$status_json" ||
    die "Coordinator ${expected} is not in the expected quorate cluster"
  COORDINATOR_NODES_JSON="$nodes_json"
}

registry_cmd() {
  "$REGISTRY" --state-dir "$CLUSTER_STATE_DIR" "$@"
}

render_routes() {
  "$RENDERER" "$@"
}

run_on_node() {
  local node="$1"
  shift
  validate_node "$node"
  (($# > 0)) || die "A remote node command is required"
  if [[ "$node" == "$(hostname -s)" ]]; then
    "$@"
  else
    mox_ssh "$node" "$@"
  fi
}

copy_bundle_to_node() {
  local node="$1" source="$2" destination="$3"
  validate_node "$node"
  [[ "$destination" =~ ^/run/app-ha-haproxy-routes-[A-Za-z0-9._-]+[.]tar$ ]] ||
    die "Unsafe remote bundle path: $destination"
  [[ -f "$source" && ! -L "$source" ]] ||
    die "Unsafe local generation bundle: $source"
  if [[ "$node" == "$(hostname -s)" ]]; then
    install -o root -g root -m 0600 "$source" "$destination"
  else
    # shellcheck disable=SC2016 # This command expands on the remote mox.
    mox_ssh "$node" bash -c \
      'set -Eeuo pipefail; umask 077; destination=$1; cat >"$destination"; chmod 0600 "$destination"' \
      app-ha-copy "$destination" <"$source"
  fi
}

stage_node() {
  local node="$1" vmid="$2" transaction="$3" generation="$4"
  local bundle="$5" bundle_sha256="$6" expected_ip="$7"
  local expected_gateway="$8" expected_bridge="$9"
  local host_bundle="/run/app-ha-haproxy-routes-${transaction}.tar"
  validate_node "$node"
  validate_vmid "$vmid"
  validate_transaction "$transaction"
  validate_generation "$generation"
  validate_digest "$bundle_sha256"
  validate_ipv4_cidr "$expected_ip" ||
    die "Unsafe fixed HAProxy CIDR argument: $expected_ip"
  validate_ipv4 "$expected_gateway" ||
    die "Unsafe fixed HAProxy gateway argument: $expected_gateway"
  validate_interface "$expected_bridge"

  copy_bundle_to_node "$node" "$bundle" "$host_bundle" || return
  run_on_node "$node" bash -s -- \
    "$vmid" "$transaction" "$generation" "$host_bundle" "$bundle_sha256" \
    "$expected_ip" "$expected_gateway" "$expected_bridge" <<'HOST'
set -Eeuo pipefail
vmid="$1"; transaction="$2"; generation="$3"; host_bundle="$4"; expected_sha256="$5"
expected_ip="$6"; expected_gateway="$7"; expected_bridge="$8"
[[ "$vmid" =~ ^91(1[1-9]|20)$ ]]
[[ "$transaction" =~ ^[A-Za-z0-9][A-Za-z0-9._-]{0,127}$ ]]
[[ "$generation" =~ ^[0-9a-f]{64}$ ]]
[[ "$expected_sha256" =~ ^[0-9a-f]{64}$ ]]
[[ "$host_bundle" == "/run/app-ha-haproxy-routes-${transaction}.tar" ]]
[[ "$expected_ip" =~ ^([0-9]{1,3}[.]){3}[0-9]{1,3}/24$ ]]
[[ "$expected_gateway" =~ ^([0-9]{1,3}[.]){3}[0-9]{1,3}$ ]]
[[ "$expected_bridge" =~ ^[A-Za-z0-9][A-Za-z0-9_.-]{0,14}$ ]]
[[ -f "$host_bundle" && ! -L "$host_bundle" ]]
[[ "$(sha256sum "$host_bundle" | awk '{print $1}')" == "$expected_sha256" ]]
if ! pct status "$vmid" |
  awk '$1 == "status:" && $2 == "running" { found=1 } END { exit !found }'; then
  pct start "$vmid"
  for _attempt in $(seq 1 30); do
    if pct exec "$vmid" -- true >/dev/null 2>&1; then
      break
    fi
    sleep 2
  done
fi
pct status "$vmid" |
  awk '$1 == "status:" && $2 == "running" { found=1 } END { exit !found }'
config="$(pct config "$vmid")"
net0="$(
  awk -F ': ' \
    '$1 == "net0" { print $2; count++ } END { if (count != 1) exit 1 }' \
    <<<"$config"
)"
for expected in "bridge=$expected_bridge" "gw=$expected_gateway" "ip=$expected_ip"; do
  case ",${net0}," in
    *",${expected},"*) ;;
    *)
      printf 'Fixed HAProxy LXC %s net0 is missing %s: %s\n' \
        "$vmid" "$expected" "$net0" >&2
      exit 1
      ;;
  esac
done

pct exec "$vmid" -- install -d -m 0700 \
  "/run/app-ha-haproxy-route-sync/${transaction}"
pct push "$vmid" "$host_bundle" \
  "/run/app-ha-haproxy-route-sync/${transaction}/generation.tar"
pct exec "$vmid" -- bash -s -- \
  "$transaction" "$generation" "$expected_sha256" <<'LXC'
set -Eeuo pipefail
transaction="$1"; generation="$2"; expected_sha256="$3"
transaction_dir="/run/app-ha-haproxy-route-sync/${transaction}"
archive="${transaction_dir}/generation.tar"
generation_root="/etc/haproxy/app-ha-generations"
final="${generation_root}/${generation}"
candidate="${generation_root}/.${generation}.${transaction}.candidate"
[[ "$transaction" =~ ^[A-Za-z0-9][A-Za-z0-9._-]{0,127}$ ]]
[[ "$generation" =~ ^[0-9a-f]{64}$ ]]
[[ "$expected_sha256" =~ ^[0-9a-f]{64}$ ]]
[[ -f "$archive" && ! -L "$archive" ]]
[[ "$(sha256sum "$archive" | awk '{print $1}')" == "$expected_sha256" ]]
rm -rf -- "$candidate"
install -d -m 0755 "$candidate"
tar --no-same-owner --no-same-permissions -xf "$archive" -C "$candidate"
mapfile -t files < <(
  find "$candidate" -xdev -type f -printf '%P\n' | LC_ALL=C sort
)
expected=(
  haproxy.cfg
  manifest.json
  maps/app-ha-http-host.map
  maps/app-ha-tls-sni.map
)
[[ "${files[*]}" == "${expected[*]}" ]]
! find "$candidate" -xdev -type l -print -quit | grep -q .
chmod 0755 "$candidate" "$candidate/maps"
chmod 0644 "$candidate/haproxy.cfg" "$candidate/manifest.json" \
  "$candidate"/maps/*.map
if [[ -e "$final" ]]; then
  [[ -d "$final" && ! -L "$final" ]]
  diff -qr "$candidate" "$final" >/dev/null
  rm -rf -- "$candidate"
else
  mv "$candidate" "$final"
fi
haproxy -c -f "${final}/haproxy.cfg"
rm -f -- "$archive"
LXC
rm -f -- "$host_bundle"
HOST
}

node_generation_is_current() {
  local node="$1" vmid="$2" generation="$3"
  validate_node "$node"
  validate_vmid "$vmid"
  validate_generation "$generation"
  run_on_node "$node" bash -s -- "$vmid" "$generation" <<'HOST'
set -Eeuo pipefail
vmid="$1"; generation="$2"
[[ "$vmid" =~ ^91(1[1-9]|20)$ ]]
[[ "$generation" =~ ^[0-9a-f]{64}$ ]]
pct status "$vmid" |
  awk '$1 == "status:" && $2 == "running" { found=1 } END { exit !found }'
pct exec "$vmid" -- bash -s -- "$generation" <<'LXC'
set -Eeuo pipefail
generation="$1"
root="/etc/haproxy/app-ha-generations/${generation}"
[[ "$generation" =~ ^[0-9a-f]{64}$ ]]
[[ -f /etc/haproxy/app-ha-active-generation &&
   ! -L /etc/haproxy/app-ha-active-generation ]]
[[ "$(< /etc/haproxy/app-ha-active-generation)" == "$generation" ]]
[[ -d "$root" && ! -L "$root" ]]
cmp -s "$root/haproxy.cfg" /etc/haproxy/haproxy.cfg
grep -Fq "/${generation}/maps/" /etc/haproxy/haproxy.cfg
haproxy -c -f /etc/haproxy/haproxy.cfg >/dev/null
systemctl is-active --quiet haproxy
LXC
HOST
}

disable_ingress_node() {
  local node="$1" vmid="$2"
  validate_node "$node"
  validate_vmid "$vmid"
  run_on_node "$node" bash -s -- "$vmid" <<'HOST'
set -Eeuo pipefail
vmid="$1"
[[ "$vmid" =~ ^91(1[1-9]|20)$ ]]
if ! command -v nft >/dev/null 2>&1; then
  pct stop "$vmid" >/dev/null 2>&1 || true
  exit 1
fi
for table in app_ha_haproxy_ingress; do
  if nft list table inet "$table" >/dev/null 2>&1; then
    if ! nft delete table inet "$table"; then
      pct stop "$vmid" >/dev/null 2>&1 || true
      exit 1
    fi
  fi
done
rm -f /run/app-ha-haproxy-ingress-current
HOST
}

enable_ingress_node() {
  local node="$1" vmid="$2" generation="$3"
  validate_node "$node"
  validate_vmid "$vmid"
  validate_generation "$generation"
  node_generation_is_current "$node" "$vmid" "$generation" || return
  run_on_node "$node" bash -s -- "$generation" <<'HOST'
set -Eeuo pipefail
generation="$1"
[[ "$generation" =~ ^[0-9a-f]{64}$ ]]
[[ -x /usr/local/sbin/app-ha-load-haproxy-ingress &&
   ! -L /usr/local/sbin/app-ha-load-haproxy-ingress ]]
current=""
if [[ -f /run/app-ha-haproxy-ingress-current &&
      ! -L /run/app-ha-haproxy-ingress-current ]]; then
  current="$(< /run/app-ha-haproxy-ingress-current)"
fi
if [[ "$current" != "$generation" ]] ||
   ! nft list table inet app_ha_haproxy_ingress >/dev/null 2>&1; then
  /usr/local/sbin/app-ha-load-haproxy-ingress
  printf '%s\n' "$generation" >/run/app-ha-haproxy-ingress-current
  chmod 0600 /run/app-ha-haproxy-ingress-current
fi
systemctl start --no-block app-ha-haproxy-ingress.service
HOST
}

install_node() {
  local node="$1" vmid="$2" transaction="$3" generation="$4"
  validate_node "$node"
  validate_vmid "$vmid"
  validate_transaction "$transaction"
  validate_generation "$generation"
  run_on_node "$node" bash -s -- \
    "$vmid" "$transaction" "$generation" <<'HOST'
set -Eeuo pipefail
vmid="$1"; transaction="$2"; generation="$3"
[[ "$vmid" =~ ^91(1[1-9]|20)$ ]]
[[ "$transaction" =~ ^[A-Za-z0-9][A-Za-z0-9._-]{0,127}$ ]]
[[ "$generation" =~ ^[0-9a-f]{64}$ ]]

# This is intentionally adjacent to the LXC commit. A node that no longer
# sees quorum cannot replace its live configuration.
pvecm status 2>/dev/null |
  awk '$1 == "Quorate:" && $2 == "Yes" { found=1 } END { exit !found }'
pct exec "$vmid" -- bash -s -- "$transaction" "$generation" <<'LXC'
set -Eeuo pipefail
transaction="$1"; generation="$2"
source_config="/etc/haproxy/app-ha-generations/${generation}/haproxy.cfg"
temporary="/etc/haproxy/.haproxy.cfg.app-ha-${transaction}"
marker_temporary="/etc/haproxy/.app-ha-active-generation-${transaction}"
[[ "$transaction" =~ ^[A-Za-z0-9][A-Za-z0-9._-]{0,127}$ ]]
[[ "$generation" =~ ^[0-9a-f]{64}$ ]]
[[ -f "$source_config" && ! -L "$source_config" ]]
install -m 0644 "$source_config" "$temporary"
haproxy -c -f "$temporary"
mv -f "$temporary" /etc/haproxy/haproxy.cfg
haproxy -c -f /etc/haproxy/haproxy.cfg
systemctl reload haproxy
systemctl is-active --quiet haproxy
printf '%s\n' "$generation" >"$marker_temporary"
chmod 0644 "$marker_temporary"
mv -f "$marker_temporary" /etc/haproxy/app-ha-active-generation
LXC
HOST
}

prune_node_generations() {
  local node="$1" vmid="$2" generation="$3"
  validate_node "$node"
  validate_vmid "$vmid"
  validate_generation "$generation"
  run_on_node "$node" bash -s -- \
    "$vmid" "$generation" "$MAX_RETAINED_GENERATIONS" <<'HOST'
set -Eeuo pipefail
vmid="$1"; current="$2"; maximum="$3"
[[ "$vmid" =~ ^91(1[1-9]|20)$ ]]
[[ "$current" =~ ^[0-9a-f]{64}$ ]]
[[ "$maximum" =~ ^[1-9][0-9]*$ ]]
pct exec "$vmid" -- bash -s -- "$current" "$maximum" <<'LXC'
set -Eeuo pipefail
protected="$1"; maximum="$2"
root=/etc/haproxy/app-ha-generations
[[ "$protected" =~ ^[0-9a-f]{64}$ ]]
[[ "$maximum" =~ ^[1-9][0-9]*$ ]]
[[ -d "$root" && ! -L "$root" ]]
declare -A retained=(["$protected"]=1)
retained_count=1
if [[ -f /etc/haproxy/app-ha-active-generation &&
      ! -L /etc/haproxy/app-ha-active-generation ]]; then
  active="$(< /etc/haproxy/app-ha-active-generation)"
  [[ "$active" =~ ^[0-9a-f]{64}$ ]]
  if [[ -z "${retained[$active]+x}" ]]; then
    retained["$active"]=1
    ((retained_count += 1))
  fi
fi
mapfile -t generations < <(
  find "$root" -mindepth 1 -maxdepth 1 -type d \
    -regextype posix-extended -regex '.*/[0-9a-f]{64}' \
    -printf '%T@ %f\n' |
    LC_ALL=C sort -rn |
    awk '{print $2}'
)
for candidate in "${generations[@]}"; do
  [[ -z "${retained[$candidate]+x}" ]] || continue
  if ((retained_count < maximum)); then
    retained["$candidate"]=1
    ((retained_count += 1))
  fi
done
for candidate in "${generations[@]}"; do
  [[ -n "${retained[$candidate]+x}" ]] ||
    rm -rf -- "$root/$candidate"
done
LXC
HOST
}

cleanup_node() {
  local node="$1" vmid="$2" transaction="$3"
  validate_node "$node"
  validate_vmid "$vmid"
  validate_transaction "$transaction"
  run_on_node "$node" bash -s -- "$vmid" "$transaction" <<'HOST'
set -Eeuo pipefail
vmid="$1"; transaction="$2"
[[ "$vmid" =~ ^91(1[1-9]|20)$ ]]
[[ "$transaction" =~ ^[A-Za-z0-9][A-Za-z0-9._-]{0,127}$ ]]
rm -f -- "/run/app-ha-haproxy-routes-${transaction}.tar"
if pct status "$vmid" 2>/dev/null |
  awk '$1 == "status:" && $2 == "running" { found=1 } END { exit !found }'; then
  pct exec "$vmid" -- bash -s -- "$transaction" <<'LXC'
set -Eeuo pipefail
transaction="$1"
[[ "$transaction" =~ ^[A-Za-z0-9][A-Za-z0-9._-]{0,127}$ ]]
rm -rf -- "/run/app-ha-haproxy-route-sync/${transaction}"
find /etc/haproxy/app-ha-generations -mindepth 1 -maxdepth 1 \
  -type d -name ".*.${transaction}.candidate" -exec rm -rf -- {} + \
  2>/dev/null || true
LXC
fi
HOST
}

best_effort_disable_local_ingress() {
  local node index
  node="$(hostname -s 2>/dev/null || true)"
  [[ "$node" =~ ^mox([1-9]|10)$ ]] || return 0
  index="${node#mox}"
  if command -v nft >/dev/null 2>&1; then
    if nft list table inet app_ha_haproxy_ingress >/dev/null 2>&1; then
      nft delete table inet app_ha_haproxy_ingress >/dev/null 2>&1 || true
    fi
  fi
  rm -f /run/app-ha-haproxy-ingress-current
}

process_exit_cleanup() {
  local exit_code=$?
  trap - EXIT
  [[ -z "${WORK_DIR:-}" ]] || rm -rf -- "$WORK_DIR"
  if [[ "$LOCAL_FAIL_CLOSED_ARMED" == true && "$exit_code" -ne 0 ]]; then
    best_effort_disable_local_ingress
  fi
  exit "$exit_code"
}

mark_ingress_state() {
  local node="$1" generation="$2" state="$3"
  validate_node "$node"
  validate_generation "$generation"
  [[ "$state" == pending || "$state" == staged ||
     "$state" == applied || "$state" == reject-only ]] ||
    die "Unsafe ingress state: $state"
  registry_cmd ingress-mark \
    --generation "$generation" --node "$node" --state "$state" >/dev/null
}

publish_desired_generation() {
  local coordinator="$1" generation="$2" bundle_sha256="$3"
  local route_count="$4" output="$5" index
  local -a arguments=(
    ingress-begin
    --generation "$generation"
    --bundle-sha256 "$bundle_sha256"
    --route-count "$route_count"
  )
  validate_node "$coordinator"
  validate_generation "$generation"
  validate_digest "$bundle_sha256"
  [[ "$route_count" =~ ^[0-9]+$ && "$route_count" -le 4096 ]] ||
    die "Unsafe route count: $route_count"
  [[ "$output" == "$WORK_DIR/ingress-plan.json" ]] ||
    die "Unsafe ingress plan output path"
  for ((index = 1; index <= MAX_MOX_HOSTS; index += 1)); do
    arguments+=(--node "mox${index}")
  done

  # Persisting the desired pointer is the transaction commit point in pmxcfs.
  # Recheck coordinator liveness and quorum immediately before that write.
  assert_coordinator_online_and_quorate "$coordinator"
  registry_cmd --lock-timeout 0 "${arguments[@]}" >"$output"
  jq -e --arg generation "$generation" '
    .desired.generation == $generation and
    (.nodes | type == "array")
  ' "$output" >/dev/null ||
    die "Registry did not confirm the desired ingress generation"
}

verify_target_membership() {
  local coordinator="$1" index node
  shift
  local -A targeted=()
  for node in "$@"; do
    validate_node "$node"
    targeted["$node"]=1
  done
  assert_coordinator_online_and_quorate "$coordinator"
  for ((index = 1; index <= MAX_MOX_HOSTS; index += 1)); do
    node="mox${index}"
    if node_is_online "$node" "$COORDINATOR_NODES_JSON"; then
      if [[ -z "${targeted[$node]+x}" ]]; then
        disable_ingress_node "$node" "$((9110 + index))" || true
        die "${node} came online after staging and was forced reject-only"
      fi
    elif [[ -n "${targeted[$node]+x}" ]]; then
      die "${node} went offline after staging"
    fi
  done
}

abort_transaction_on_signal() {
  local signal="$1" exit_code=1 index
  trap - HUP INT TERM
  case "$signal" in
    HUP) exit_code=129 ;;
    INT) exit_code=130 ;;
    TERM) exit_code=143 ;;
  esac
  printf 'ERROR: route synchronization received %s; desired state remains recoverable\n' \
    "$signal" >&2
  if [[ -n "$TRANSACTION" ]]; then
    for index in "${!TRANSACTION_NODES[@]}"; do
      cleanup_node "${TRANSACTION_NODES[$index]}" \
        "${TRANSACTION_VMIDS[$index]}" "$TRANSACTION" \
        >/dev/null 2>&1 || true
    done
  fi
  rm -rf -- "${WORK_DIR:-}"
  WORK_DIR=""
  exit "$exit_code"
}

coordinator_transaction() {
  ((EUID == 0)) ||
    die "Route synchronization must run as root on the coordinator"
  require_tools python3 jq pvesh pct tar sha256sum awk flock hostname nft
  require_file "$REGISTRY"
  require_file "$RENDERER"
  local coordinator
  coordinator="$(hostname -s)"
  validate_node "$coordinator"

  install -d -m 0755 /run/lock
  exec 9>/run/lock/app-ha-haproxy-routes.lock
  flock -w "$LOCK_WAIT_SECONDS" 9 ||
    die "Timed out after ${LOCK_WAIT_SECONDS}s waiting for another app-ha HAProxy route synchronization"

  local generation bundle bundle_sha256 transaction route_count private_prefix
  local index node failure_summary
  local -a nodes=() vmids=() lxc_ips=() lxc_gateways=()
  local -a needs_commit=() failures=()

  WORK_DIR="$(mktemp -d /run/app-ha-haproxy-render.XXXXXX)"

  # This check is deliberately before the only read of desired registry routes.
  assert_coordinator_online_and_quorate "$coordinator"
  log "Rendering routes from quorate pmxcfs on ${coordinator}"
  registry_cmd list-routes >"${WORK_DIR}/routes.json"
  route_count="$(
    jq -er '
      if type != "array" then error("routes must be an array")
      elif length > 4096 then error("too many routes")
      else length
      end
    ' "${WORK_DIR}/routes.json"
  )" || die "Registry routes exceeded the renderer contract"
  render_routes \
    --routes-json "${WORK_DIR}/routes.json" \
    --output-dir "${WORK_DIR}/generation" \
    --backend-network "$PRIVATE_SUBNET_CIDR" \
    --production-ip-start "$PRODUCTION_IP_START" \
    --production-ip-end "$PRODUCTION_IP_END" \
    --staging-ip-start "$STAGING_IP_START" \
    --staging-ip-end "$STAGING_IP_END" \
    >"${WORK_DIR}/render-result.json"
  generation="$(
    jq -er '.generation | select(test("^[0-9a-f]{64}$"))' \
      "${WORK_DIR}/render-result.json"
  )"
  validate_generation "$generation"
  bundle="${WORK_DIR}/generation.tar"
  tar --sort=name --mtime='UTC 1970-01-01' --owner=0 --group=0 \
    --numeric-owner -C "${WORK_DIR}/generation" -cf "$bundle" .
  bundle_sha256="$(sha256sum "$bundle" | awk '{print $1}')"
  validate_digest "$bundle_sha256"

  assert_coordinator_online_and_quorate "$coordinator"
  private_prefix="$PRIVATE_SUBNET_PREFIX"
  for ((index = 1; index <= MAX_MOX_HOSTS; index += 1)); do
    node="mox${index}"
    if node_is_online "$node" "$COORDINATOR_NODES_JSON"; then
      nodes+=("$node")
      vmids+=("$((9110 + index))")
      lxc_ips+=("${private_prefix}.$((HAPROXY_IP_START_OCTET + index - 1))/24")
      lxc_gateways+=("${private_prefix}.$((MOX_IP_START_OCTET + index - 1))")
      needs_commit+=(false)
    fi
  done
  ((${#nodes[@]} > 0)) || die "No online mox hosts were found"
  [[ "${nodes[0]}" == "$coordinator" ]] ||
    die "Coordinator is not the lowest-numbered online mox"

  transaction="${generation:0:16}-$(date -u +%Y%m%dT%H%M%SZ)-$$"
  validate_transaction "$transaction"
  TRANSACTION="$transaction"
  TRANSACTION_NODES=("${nodes[@]}")
  TRANSACTION_VMIDS=("${vmids[@]}")
  trap 'abort_transaction_on_signal HUP' HUP
  trap 'abort_transaction_on_signal INT' INT
  trap 'abort_transaction_on_signal TERM' TERM

  log "Reconciling generation ${generation:0:16} on ${#nodes[@]} online mox host(s)"

  # Build and validate every missing generation before changing desired state
  # or public ingress. A staging failure leaves the prior desired generation
  # untouched and still serviceable.
  for index in "${!nodes[@]}"; do
    node="${nodes[$index]}"
    if node_generation_is_current "$node" "${vmids[$index]}" "$generation"; then
      continue
    fi
    if stage_node "$node" "${vmids[$index]}" "$transaction" "$generation" \
      "$bundle" "$bundle_sha256" "${lxc_ips[$index]}" \
      "${lxc_gateways[$index]}" "$PROXMOX_PRIVATE_BRIDGE"; then
      if prune_node_generations \
        "$node" "${vmids[$index]}" "$generation"; then
        needs_commit[index]=true
      else
        failures+=("${node}: staged generation pruning failed")
      fi
    else
      failures+=("${node}: staging failed")
    fi
  done

  if ((${#failures[@]} == 0)); then
    verify_target_membership "$coordinator" "${nodes[@]}"
    # Once every online node has a validated candidate, close stale public
    # ingress before publishing the new desired pointer. A coordinator crash
    # from this point onward therefore leaves stale nodes disabled.
    for index in "${!nodes[@]}"; do
      [[ "${needs_commit[$index]}" == true ]] || continue
      node="${nodes[$index]}"
      if ! disable_ingress_node "$node" "${vmids[$index]}"; then
        failures+=("${node}: stale ingress could not be disabled")
      fi
    done
    # Membership can change while stale targets are being disabled. Refresh
    # once more immediately before the pmxcfs desired-generation commit so a
    # newly online node cannot continue serving an older generation.
    ((${#failures[@]} > 0)) ||
      verify_target_membership "$coordinator" "${nodes[@]}"
  fi

  if ((${#failures[@]} == 0)); then
    publish_desired_generation "$coordinator" "$generation" "$bundle_sha256" \
      "$route_count" "${WORK_DIR}/ingress-plan.json"
    for index in "${!nodes[@]}"; do
      node="${nodes[$index]}"
      if [[ "${needs_commit[$index]}" == true ]]; then
        if ! mark_ingress_state "$node" "$generation" staged; then
          failures+=("${node}: staged state could not be persisted")
        fi
        continue
      fi
      if ! mark_ingress_state "$node" "$generation" applied; then
        disable_ingress_node "$node" "${vmids[$index]}" || true
        failures+=("${node}: current applied state could not be persisted")
        continue
      fi
      if ! prune_node_generations "$node" "${vmids[$index]}" "$generation"; then
        disable_ingress_node "$node" "${vmids[$index]}" || true
        mark_ingress_state "$node" "$generation" reject-only || true
        failures+=("${node}: generation pruning failed")
        continue
      fi
      if ! enable_ingress_node "$node" "${vmids[$index]}" "$generation"; then
        disable_ingress_node "$node" "${vmids[$index]}" || true
        mark_ingress_state "$node" "$generation" reject-only || true
        failures+=("${node}: current ingress could not be enabled")
      fi
    done
  fi

  if ((${#failures[@]} == 0)); then
    for index in "${!nodes[@]}"; do
      [[ "${needs_commit[$index]}" == true ]] || continue
      node="${nodes[$index]}"

      # A fresh coordinator check and the target's own pvecm check in
      # install_node both occur immediately before this node's config commit.
      if ! assert_coordinator_online_and_quorate "$coordinator"; then
        failures+=("${node}: quorum was lost immediately before commit")
        break
      fi
      if ! node_is_online "$node" "$COORDINATOR_NODES_JSON"; then
        failures+=("${node}: node went offline immediately before commit")
        break
      fi
      if ! install_node "$node" "${vmids[$index]}" "$transaction" "$generation"; then
        mark_ingress_state "$node" "$generation" reject-only || true
        failures+=("${node}: generation commit or reload failed")
        continue
      fi
      if ! mark_ingress_state "$node" "$generation" applied; then
        disable_ingress_node "$node" "${vmids[$index]}" || true
        failures+=("${node}: applied status could not be committed")
        continue
      fi
      if ! prune_node_generations "$node" "${vmids[$index]}" "$generation"; then
        disable_ingress_node "$node" "${vmids[$index]}" || true
        mark_ingress_state "$node" "$generation" reject-only || true
        failures+=("${node}: generation pruning failed")
        continue
      fi
      if ! enable_ingress_node "$node" "${vmids[$index]}" "$generation"; then
        disable_ingress_node "$node" "${vmids[$index]}" || true
        mark_ingress_state "$node" "$generation" reject-only || true
        failures+=("${node}: ingress enablement failed")
      fi
    done
  fi

  for index in "${!nodes[@]}"; do
    cleanup_node "${nodes[$index]}" "${vmids[$index]}" "$transaction" \
      >/dev/null 2>&1 || true
  done
  TRANSACTION=""
  trap - HUP INT TERM

  if ((${#failures[@]} > 0)); then
    failure_summary="$(IFS=';'; printf '%s' "${failures[*]}")"
    die "${failure_summary}; pending nodes remain fail-closed for periodic reconciliation"
  fi
  rm -rf -- "$WORK_DIR"
  WORK_DIR=""
  log "Generation ${generation:0:16} is current on: ${nodes[*]}"
}

delegate_to_coordinator() {
  local coordinator nodes_json="" local_node
  local_node="$(hostname -s)"

  if command -v pvesh >/dev/null 2>&1 && [[ -d /etc/pve ]]; then
    nodes_json="$(online_nodes_json)" ||
      die "Could not query online Proxmox nodes"
    validate_nodes_json "$nodes_json"
    coordinator="$(first_online_node_from_json "$nodes_json")" ||
      die "No online mox coordinator was found"
    if [[ "$local_node" == "$coordinator" ]]; then
      coordinator_transaction
      return
    fi
    log "Delegating route synchronization to ${coordinator}"
    mox_ssh "$coordinator" "$INSTALLED_SYNC" --coordinator-local \
      --lock-timeout "$LOCK_WAIT_SECONDS"
    return
  fi

  coordinator="$(first_reachable_mox)" ||
    die "No strictly trusted, reachable mox coordinator was found"
  log "Requesting quorum-gated synchronization through ${coordinator}"
  mox_ssh "$coordinator" "$INSTALLED_SYNC" \
    --lock-timeout "$LOCK_WAIT_SECONDS"
}

local_vmid() {
  local node index
  node="$(hostname -s)"
  validate_node "$node"
  index="${node#mox}"
  printf '%s\n' "$((9110 + index))"
}

assert_local_current() {
  local node vmid status_json generation
  node="$(hostname -s)"
  vmid="$(local_vmid)"
  if ! assert_local_node_online_and_quorate; then
    disable_ingress_node "$node" "$vmid" || true
    printf 'ERROR: local ingress is disabled because %s is not online and quorate\n' \
      "$node" >&2
    return 1
  fi
  if ! status_json="$(registry_cmd ingress-status)"; then
    disable_ingress_node "$node" "$vmid" || true
    return 1
  fi
  generation="$(
    jq -er '.desired.generation | select(test("^[0-9a-f]{64}$"))' \
      <<<"$status_json"
  )" || {
    disable_ingress_node "$node" "$vmid" || true
    return 1
  }
  if ! jq -e --arg node "$node" --arg generation "$generation" '
    any(
      .nodes[];
      .node == $node and
      .desired_generation == $generation and
      .applied_generation == $generation and
      .state == "applied"
    )
  ' <<<"$status_json" >/dev/null; then
    disable_ingress_node "$node" "$vmid" || true
    return 1
  fi
  if [[ ! -f /run/app-ha-haproxy-ingress-current ||
        -L /run/app-ha-haproxy-ingress-current ||
        "$(< /run/app-ha-haproxy-ingress-current)" != "$generation" ]]; then
    disable_ingress_node "$node" "$vmid" || true
    return 1
  fi
  if ! node_generation_is_current "$node" "$vmid" "$generation"; then
    disable_ingress_node "$node" "$vmid" || true
    return 1
  fi
}

local_reconcile() {
  local node vmid nodes_json coordinator status_json generation
  local local_is_current=false
  node="$(hostname -s)"
  vmid="$(local_vmid)"

  # The preflight either proves this node already has desired state or removes
  # public DNAT before attempting any network coordination.
  if assert_local_current >/dev/null 2>&1; then
    local_is_current=true
  else
    disable_ingress_node "$node" "$vmid"
  fi
  # Every node keeps the timer so a new coordinator takes over after failure,
  # but a healthy non-coordinator does not create a duplicate transaction on
  # the current coordinator every minute.
  if [[ "$local_is_current" == true ]] &&
     nodes_json="$(online_nodes_json 2>/dev/null)" &&
     validate_nodes_json "$nodes_json" 2>/dev/null &&
     coordinator="$(first_online_node_from_json "$nodes_json" 2>/dev/null)" &&
     [[ "$node" != "$coordinator" ]]; then
    status_json="$(registry_cmd ingress-status)"
    generation="$(jq -er '.desired.generation' <<<"$status_json")"
    enable_ingress_node "$node" "$vmid" "$generation" ||
      die "Current non-coordinator ingress could not be enabled"
    log "Local generation is current; coordinator ${coordinator} owns periodic reconciliation"
    return 0
  fi
  if ! (
    # The parent performs a fresh local gate after any coordinator failure.
    # This prevents harmless timer lock contention from dropping current
    # ingress while still failing closed for quorum or generation changes.
    LOCAL_FAIL_CLOSED_ARMED=false
    delegate_to_coordinator
  ); then
    if assert_local_current; then
      log "Coordinator sync is busy; local desired generation remains current"
      return 0
    fi
    disable_ingress_node "$node" "$vmid" || true
    die "Coordinator reconciliation failed; local ingress remains disabled"
  fi
  if ! assert_local_current; then
    die "Local node did not reach the desired generation and remains disabled"
  fi
  status_json="$(registry_cmd ingress-status)"
  generation="$(jq -er '.desired.generation' <<<"$status_json")"
  enable_ingress_node "$node" "$vmid" "$generation" ||
    die "Current local ingress could not be enabled"
}

main() {
  parse_args "$@"
  if [[ "$MODE" == local-reconcile || "$MODE" == assert-local-current ]]; then
    LOCAL_FAIL_CLOSED_ARMED=true
  fi
  trap process_exit_cleanup EXIT
  require_file "$CONFIG_LIB"
  # shellcheck source=./config.sh
  source "$CONFIG_LIB"
  load_proxmox_config --no-secrets ||
    die "Could not load cluster.conf without secrets"
  require_vars MAX_MOX_HOSTS CLUSTER_STATE_DIR PROXMOX_CLUSTER_NAME \
    PRIVATE_SUBNET_PREFIX MOX_IP_START_OCTET HAPROXY_IP_START_OCTET \
    PRIVATE_SUBNET_CIDR PRODUCTION_IP_START PRODUCTION_IP_END \
    STAGING_IP_START STAGING_IP_END
  require_var PROXMOX_PRIVATE_BRIDGE
  [[ "$MAX_MOX_HOSTS" =~ ^([1-9]|10)$ ]] ||
    die "MAX_MOX_HOSTS must be between 1 and 10"
  [[ "$CLUSTER_STATE_DIR" == /* && "$CLUSTER_STATE_DIR" != *$'\n'* ]] ||
    die "CLUSTER_STATE_DIR must be a safe absolute path"
  validate_interface "$PROXMOX_PRIVATE_BRIDGE"

  case "$MODE" in
    coordinator) coordinator_transaction ;;
    local-reconcile) local_reconcile ;;
    assert-local-current) assert_local_current ;;
    delegate) delegate_to_coordinator ;;
    *) die "Internal synchronization mode error" ;;
  esac
  LOCAL_FAIL_CLOSED_ARMED=false
}

if [[ "${BASH_SOURCE[0]}" == "$0" ]]; then
  main "$@"
fi
