#!/usr/bin/env bash

# Copyright (c) 2026 BEENTHERE VENTURES, INC.
# SPDX-License-Identifier: GPL-3.0-only

# Safely update the shared app-ha registry, HAProxy renderer, deferred cleanup
# worker, and QEMU lifecycle hook on every configured Proxmox node.

set -Eeuo pipefail
set +x
umask 077

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
REPO_ROOT="$(cd -- "${SCRIPT_DIR}/.." && pwd -P)"
CONFIG_LIB="${REPO_ROOT}/lib/config.sh"
REGISTRY_SOURCE="${REPO_ROOT}/lib/cluster_registry.py"
RENDERER_SOURCE="${REPO_ROOT}/lib/haproxy_routes.py"
CLEANUP_SOURCE="${REPO_ROOT}/lib/process_deferred_cleanup.sh"
HOOK_SOURCE="${SCRIPT_DIR}/app-ha-guest-role-hook.sh"
INSTALL_ROOT="/usr/local/lib/app-ha-proxmox"
REMOTE_REGISTRY="${INSTALL_ROOT}/lib/cluster_registry.py"
REMOTE_RENDERER="${INSTALL_ROOT}/lib/haproxy_routes.py"
REMOTE_CLEANUP="${INSTALL_ROOT}/lib/process_deferred_cleanup.sh"
REMOTE_HOOK="${INSTALL_ROOT}/lib/app-ha-guest-role-hook.sh"
REMOTE_ROUTE_SYNC="${INSTALL_ROOT}/lib/sync_haproxy_routes.sh"
REMOTE_SNIPPET=""

DRY_RUN=false
ASSUME_YES=false
COORDINATOR=""
REMOTE_STAGE=""
COMPLETED=false
ROLLBACK_RUNNING=false
declare -a CLUSTER_NODES=()
declare -a STAGED_NODES=()
declare -a COMMITTED_NODES=()

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
Usage: update_cluster_runtime.sh [--dry-run] [--yes]

Update these runtime files on every configured, online Proxmox node:
  /usr/local/lib/app-ha-proxmox/lib/cluster_registry.py
  /usr/local/lib/app-ha-proxmox/lib/haproxy_routes.py
  /usr/local/lib/app-ha-proxmox/lib/process_deferred_cleanup.sh
  /usr/local/lib/app-ha-proxmox/lib/app-ha-guest-role-hook.sh
  local:snippets/app-ha-guest-role-hook.sh

The script stages and validates the complete bundle on every node before
making changes. Installs are atomic, old files are retained until cluster-wide
verification and route synchronization succeed, and failures trigger rollback.

Options:
  --dry-run  Validate local files and cluster reachability; show hash drift
             without changing any node.
  --yes      Skip the interactive GO confirmation.
  -h, --help
EOF
}

parse_args() {
  while (($#)); do
    case "$1" in
      --dry-run)
        DRY_RUN=true
        ;;
      --yes)
        ASSUME_YES=true
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

require_command() {
  command -v "$1" >/dev/null 2>&1 ||
    die "Required workstation command is unavailable: $1"
}

require_source() {
  [[ -f "$1" && ! -L "$1" ]] ||
    die "Required regular, non-symlink source is missing: $1"
}

load_config() {
  require_source "$CONFIG_LIB"
  # shellcheck disable=SC1090,SC1091 # Resolved from this script's path.
  source "$CONFIG_LIB"
  load_proxmox_config --no-secrets >/dev/null
  require_var MAX_MOX_HOSTS
  require_var CLUSTER_STATE_DIR
  require_var GUEST_ROLE_HOOK_PATH
  [[ "$GUEST_ROLE_HOOK_PATH" =~ ^[A-Za-z0-9._-]+:snippets/[A-Za-z0-9._-]+[.]sh$ ]] ||
    die "GUEST_ROLE_HOOK_PATH must be a safe Proxmox snippets volume"
  [[ "$(basename -- "$GUEST_ROLE_HOOK_PATH")" == "app-ha-guest-role-hook.sh" ]] ||
    die "This updater only manages app-ha-guest-role-hook.sh"
  REMOTE_SNIPPET="/var/lib/vz/snippets/$(basename -- "$GUEST_ROLE_HOOK_PATH")"
}

validate_local_bundle() {
  local source
  for source in \
    "$REGISTRY_SOURCE" "$RENDERER_SOURCE" "$CLEANUP_SOURCE" "$HOOK_SOURCE"; do
    require_source "$source"
  done
  bash -n "$CLEANUP_SOURCE" "$HOOK_SOURCE"
  PYTHONDONTWRITEBYTECODE=1 python3 "$REGISTRY_SOURCE" --help >/dev/null
  PYTHONDONTWRITEBYTECODE=1 python3 "$RENDERER_SOURCE" --help >/dev/null
}

discover_cluster() {
  COORDINATOR="$(first_reachable_mox)" ||
    die "No reachable Proxmox coordinator was found"
  local nodes_json nodes_text
  nodes_json="$(
    mox_ssh "$COORDINATOR" pvesh get /nodes --output-format json </dev/null
  )" || die "Could not query cluster membership through $COORDINATOR"
  nodes_text="$(
    python3 - "$nodes_json" "$MAX_MOX_HOSTS" <<'PY'
import json
import re
import sys

rows = json.loads(sys.argv[1])
maximum = int(sys.argv[2])
if not isinstance(rows, list) or not rows:
    raise SystemExit("cluster node inventory is empty or malformed")
nodes = []
for row in rows:
    if not isinstance(row, dict):
        raise SystemExit("cluster node inventory contains a malformed row")
    name = row.get("node")
    match = re.fullmatch(r"mox([1-9]|10)", str(name))
    if not match or int(match.group(1)) > maximum:
        raise SystemExit(f"unexpected cluster node: {name!r}")
    if row.get("status") != "online":
        raise SystemExit(f"cluster node is not online: {name}")
    nodes.append(name)
if len(nodes) != len(set(nodes)):
    raise SystemExit("cluster node inventory contains duplicate names")
for name in sorted(nodes, key=lambda value: int(value[3:])):
    print(name)
PY
  )" || die "Every configured cluster node must be valid and online"
  mapfile -t CLUSTER_NODES <<<"$nodes_text"
  ((${#CLUSTER_NODES[@]} > 0)) || die "No cluster nodes were discovered"

  local node
  for node in "${CLUSTER_NODES[@]}"; do
    mox_is_reachable "$node" ||
      die "$node is online in Proxmox but unreachable directly from this workstation"
  done
  info "Coordinator: $COORDINATOR"
  info "Online nodes: ${CLUSTER_NODES[*]}"
}

local_hash() {
  sha256sum "$1" | awk '{print $1}'
}

remote_hash() {
  local node="$1" path="$2"
  mox_ssh "$node" sha256sum "$path" |
    awk 'NF == 2 && $1 ~ /^[0-9a-f]{64}$/ {print $1}'
}

show_hash_status() {
  local registry_hash renderer_hash cleanup_hash hook_hash node actual state
  registry_hash="$(local_hash "$REGISTRY_SOURCE")"
  renderer_hash="$(local_hash "$RENDERER_SOURCE")"
  cleanup_hash="$(local_hash "$CLEANUP_SOURCE")"
  hook_hash="$(local_hash "$HOOK_SOURCE")"
  printf '\n%-8s %-12s %-12s %-12s %-12s %-12s\n' \
    "Node" "Registry" "Renderer" "Cleanup" "Hook library" "Hook snippet"
  for node in "${CLUSTER_NODES[@]}"; do
    printf '%-8s' "$node"
    actual="$(remote_hash "$node" "$REMOTE_REGISTRY" 2>/dev/null || true)"
    [[ "$actual" == "$registry_hash" ]] && state=current || state=update
    printf ' %-12s' "$state"
    actual="$(remote_hash "$node" "$REMOTE_RENDERER" 2>/dev/null || true)"
    [[ "$actual" == "$renderer_hash" ]] && state=current || state=update
    printf ' %-12s' "$state"
    actual="$(remote_hash "$node" "$REMOTE_CLEANUP" 2>/dev/null || true)"
    [[ "$actual" == "$cleanup_hash" ]] && state=current || state=update
    printf ' %-12s' "$state"
    actual="$(remote_hash "$node" "$REMOTE_HOOK" 2>/dev/null || true)"
    [[ "$actual" == "$hook_hash" ]] && state=current || state=update
    printf ' %-12s' "$state"
    actual="$(remote_hash "$node" "$REMOTE_SNIPPET" 2>/dev/null || true)"
    [[ "$actual" == "$hook_hash" ]] && state=current || state=update
    printf ' %-12s\n' "$state"
  done
}

copy_to_node() {
  local node="$1" source="$2" destination="$3" ssh_destination
  _mox_ssh_options "$node"
  ssh_destination="$(_mox_ssh_destination "$node")"
  scp "${MOX_SSH_OPTIONS[@]}" -- "$source" \
    "${MOX_SSH_USER:-root}@${ssh_destination}:${destination}"
}

stage_node() {
  local node="$1"
  mox_ssh "$node" install -d -m 0700 "$REMOTE_STAGE"
  STAGED_NODES+=("$node")
  copy_to_node "$node" "$REGISTRY_SOURCE" "${REMOTE_STAGE}/cluster_registry.py"
  copy_to_node "$node" "$RENDERER_SOURCE" "${REMOTE_STAGE}/haproxy_routes.py"
  copy_to_node "$node" "$CLEANUP_SOURCE" \
    "${REMOTE_STAGE}/process_deferred_cleanup.sh"
  copy_to_node "$node" "$HOOK_SOURCE" \
    "${REMOTE_STAGE}/app-ha-guest-role-hook.sh"
  # shellcheck disable=SC2016 # This script is evaluated by remote Bash.
  mox_ssh "$node" bash -c '
set -Eeuo pipefail
stage="$1"; registry_hash="$2"; renderer_hash="$3"
cleanup_hash="$4"; hook_hash="$5"
[[ -d "$stage" && ! -L "$stage" ]]
chmod 0700 "$stage"
chmod 0755 \
  "$stage/cluster_registry.py" \
  "$stage/haproxy_routes.py" \
  "$stage/process_deferred_cleanup.sh" \
  "$stage/app-ha-guest-role-hook.sh"
[[ "$(sha256sum "$stage/cluster_registry.py" | awk "{print \$1}")" == "$registry_hash" ]]
[[ "$(sha256sum "$stage/haproxy_routes.py" | awk "{print \$1}")" == "$renderer_hash" ]]
[[ "$(sha256sum "$stage/process_deferred_cleanup.sh" | awk "{print \$1}")" == "$cleanup_hash" ]]
[[ "$(sha256sum "$stage/app-ha-guest-role-hook.sh" | awk "{print \$1}")" == "$hook_hash" ]]
bash -n "$stage/process_deferred_cleanup.sh" "$stage/app-ha-guest-role-hook.sh"
PYTHONDONTWRITEBYTECODE=1 python3 "$stage/cluster_registry.py" --help >/dev/null
PYTHONDONTWRITEBYTECODE=1 python3 "$stage/haproxy_routes.py" --help >/dev/null
' bash "$REMOTE_STAGE" \
    "$(local_hash "$REGISTRY_SOURCE")" \
    "$(local_hash "$RENDERER_SOURCE")" \
    "$(local_hash "$CLEANUP_SOURCE")" \
    "$(local_hash "$HOOK_SOURCE")"
}

commit_node() {
  local node="$1"
  COMMITTED_NODES+=("$node")
  # shellcheck disable=SC2016 # This script is evaluated by remote Bash.
  if mox_ssh "$node" bash -c '
set -Eeuo pipefail
stage="$1"; install_root="$2"; snippet="$3"
registry="${install_root}/lib/cluster_registry.py"
renderer="${install_root}/lib/haproxy_routes.py"
cleanup="${install_root}/lib/process_deferred_cleanup.sh"
hook="${install_root}/lib/app-ha-guest-role-hook.sh"
for path in "$registry" "$renderer" "$cleanup" "$hook" "$snippet"; do
  [[ -f "$path" && ! -L "$path" ]]
done
install -m 0755 "$registry" "$stage/cluster_registry.py.old"
install -m 0755 "$renderer" "$stage/haproxy_routes.py.old"
install -m 0755 "$cleanup" "$stage/process_deferred_cleanup.sh.old"
install -m 0755 "$hook" "$stage/app-ha-guest-role-hook.sh.old"
install -m 0755 "$snippet" "$stage/app-ha-guest-role-hook.snippet.old"
: >"$stage/committed"
install -m 0755 "$stage/cluster_registry.py" "${registry}.new"
install -m 0755 "$stage/haproxy_routes.py" "${renderer}.new"
install -m 0755 "$stage/process_deferred_cleanup.sh" "${cleanup}.new"
install -m 0755 "$stage/app-ha-guest-role-hook.sh" "${hook}.new"
install -m 0755 "$stage/app-ha-guest-role-hook.sh" "${snippet}.new"
mv -f "${registry}.new" "$registry"
mv -f "${renderer}.new" "$renderer"
mv -f "${cleanup}.new" "$cleanup"
mv -f "${hook}.new" "$hook"
mv -f "${snippet}.new" "$snippet"
' bash "$REMOTE_STAGE" "$INSTALL_ROOT" "$REMOTE_SNIPPET"; then
    return 0
  fi
  if mox_ssh "$node" test ! -f "${REMOTE_STAGE}/committed" \
    >/dev/null 2>&1; then
    unset "COMMITTED_NODES[$((${#COMMITTED_NODES[@]} - 1))]"
  fi
  return 1
}

rollback_node() {
  local node="$1"
  # shellcheck disable=SC2016 # This script is evaluated by remote Bash.
  mox_ssh "$node" bash -c '
set -Eeuo pipefail
stage="$1"; install_root="$2"; snippet="$3"
registry="${install_root}/lib/cluster_registry.py"
renderer="${install_root}/lib/haproxy_routes.py"
cleanup="${install_root}/lib/process_deferred_cleanup.sh"
hook="${install_root}/lib/app-ha-guest-role-hook.sh"
[[ -f "$stage/committed" ]]
install -m 0755 "$stage/cluster_registry.py.old" "${registry}.rollback"
install -m 0755 "$stage/haproxy_routes.py.old" "${renderer}.rollback"
install -m 0755 "$stage/process_deferred_cleanup.sh.old" "${cleanup}.rollback"
install -m 0755 "$stage/app-ha-guest-role-hook.sh.old" "${hook}.rollback"
install -m 0755 \
  "$stage/app-ha-guest-role-hook.snippet.old" "${snippet}.rollback"
mv -f "${registry}.rollback" "$registry"
mv -f "${renderer}.rollback" "$renderer"
mv -f "${cleanup}.rollback" "$cleanup"
mv -f "${hook}.rollback" "$hook"
mv -f "${snippet}.rollback" "$snippet"
rm -f "$stage/committed"
' bash "$REMOTE_STAGE" "$INSTALL_ROOT" "$REMOTE_SNIPPET"
}

verify_node() {
  local node="$1" expected_registry expected_renderer expected_cleanup expected_hook
  expected_registry="$(local_hash "$REGISTRY_SOURCE")"
  expected_renderer="$(local_hash "$RENDERER_SOURCE")"
  expected_cleanup="$(local_hash "$CLEANUP_SOURCE")"
  expected_hook="$(local_hash "$HOOK_SOURCE")"
  [[ "$(remote_hash "$node" "$REMOTE_REGISTRY")" == "$expected_registry" ]] ||
    die "$node registry hash differs after installation"
  [[ "$(remote_hash "$node" "$REMOTE_RENDERER")" == "$expected_renderer" ]] ||
    die "$node renderer hash differs after installation"
  [[ "$(remote_hash "$node" "$REMOTE_CLEANUP")" == "$expected_cleanup" ]] ||
    die "$node cleanup worker hash differs after installation"
  [[ "$(remote_hash "$node" "$REMOTE_HOOK")" == "$expected_hook" ]] ||
    die "$node hook library hash differs after installation"
  [[ "$(remote_hash "$node" "$REMOTE_SNIPPET")" == "$expected_hook" ]] ||
    die "$node hook snippet hash differs after installation"
  mox_ssh "$node" bash -n "$REMOTE_CLEANUP" "$REMOTE_HOOK" "$REMOTE_SNIPPET"
  mox_ssh "$node" "$REMOTE_REGISTRY" \
    --state-dir "$CLUSTER_STATE_DIR" list --record-type resources >/dev/null
  mox_ssh "$node" "$REMOTE_RENDERER" --help >/dev/null
}

cleanup_remote_stages() {
  local node
  for node in "${STAGED_NODES[@]}"; do
    mox_ssh "$node" rm -rf -- "$REMOTE_STAGE" >/dev/null 2>&1 || true
  done
}

rollback_committed_nodes() {
  [[ "$ROLLBACK_RUNNING" == false ]] || return 0
  ROLLBACK_RUNNING=true
  local index node failed=false
  warn "Runtime update failed; restoring prior files on committed nodes"
  for ((index = ${#COMMITTED_NODES[@]} - 1; index >= 0; index -= 1)); do
    node="${COMMITTED_NODES[index]}"
    if rollback_node "$node"; then
      info "Restored prior runtime on $node"
    else
      warn "Could not restore prior runtime on $node; manual recovery is required"
      failed=true
    fi
  done
  if [[ "$failed" == false ]] &&
    ! mox_ssh "$COORDINATOR" "$REMOTE_ROUTE_SYNC" --lock-timeout 240 \
      </dev/null; then
    warn "Prior files were restored, but HAProxy route resynchronization failed"
    failed=true
  fi
  [[ "$failed" == false ]]
}

cleanup() {
  local code=$?
  trap - EXIT HUP INT TERM
  set +e
  set +x
  if ((code != 0)) && [[ "$COMPLETED" == false ]] &&
    ((${#COMMITTED_NODES[@]} > 0)); then
    rollback_committed_nodes || true
  fi
  cleanup_remote_stages
  exit "$code"
}

on_signal() {
  local signal="$1" code=1
  case "$signal" in
    HUP) code=129 ;;
    INT) code=130 ;;
    TERM) code=143 ;;
  esac
  warn "Received $signal during the cluster runtime update"
  exit "$code"
}

confirm_update() {
  [[ "$ASSUME_YES" == true ]] && return 0
  local entered
  printf '\nThis will atomically replace the app-ha runtime on: %s\n' \
    "${CLUSTER_NODES[*]}"
  printf 'Type GO to continue.\n> '
  IFS= read -r entered
  [[ "$entered" == GO ]] || die "Confirmation did not match GO; no changes were made"
}

main() {
  parse_args "$@"
  local command_name
  for command_name in awk bash basename install python3 rm scp sha256sum ssh stat; do
    require_command "$command_name"
  done
  load_config
  validate_local_bundle
  discover_cluster
  show_hash_status
  if [[ "$DRY_RUN" == true ]]; then
    log "Dry run complete; no cluster files were changed"
    return 0
  fi

  confirm_update
  REMOTE_STAGE="/run/app-ha-runtime-update-$$"
  [[ "$REMOTE_STAGE" =~ ^/run/app-ha-runtime-update-[1-9][0-9]*$ ]] ||
    die "Derived remote staging path is unsafe"
  trap cleanup EXIT
  trap 'on_signal HUP' HUP
  trap 'on_signal INT' INT
  trap 'on_signal TERM' TERM

  log "Staging and validating the runtime bundle on every node"
  local node
  for node in "${CLUSTER_NODES[@]}"; do
    stage_node "$node"
    info "Validated staged bundle on $node"
  done

  log "Committing the runtime bundle"
  for node in "${CLUSTER_NODES[@]}"; do
    commit_node "$node"
    info "Installed runtime on $node"
  done

  log "Synchronizing registry-backed HAProxy routes"
  mox_ssh "$COORDINATOR" "$REMOTE_ROUTE_SYNC" --lock-timeout 240 </dev/null

  log "Verifying the installed runtime on every node"
  for node in "${CLUSTER_NODES[@]}"; do
    verify_node "$node"
    info "Verified $node"
  done

  COMPLETED=true
  cleanup_remote_stages
  STAGED_NODES=()
  log "Cluster runtime update complete"
  info "Updated nodes: ${CLUSTER_NODES[*]}"
  info "No VM restart or hook reattachment is required"
}

if [[ "${APP_HA_RUNTIME_UPDATE_SOURCE_ONLY:-0}" != 1 ]]; then
  main "$@"
fi
