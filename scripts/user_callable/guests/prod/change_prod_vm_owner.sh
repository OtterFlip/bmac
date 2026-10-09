#!/usr/bin/env bash

# Copyright (c) 2026 BEENTHERE VENTURES, INC.
# SPDX-License-Identifier: GPL-3.0-only

# Move one running production VM to another host in its HA placement with
# Proxmox HA relocation (ha-manager relocate), then record the new owner and
# the reversed replication targets in the registry.

set -Eeuo pipefail
set +x
umask 077

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
REPO_ROOT="$(cd -- "${SCRIPT_DIR}/../.." && pwd -P)"
# shellcheck source=../../lib/ui_protocol.sh
source "${REPO_ROOT}/lib/ui_protocol.sh"
CONFIG_LIB="${REPO_ROOT}/lib/config.sh"
PROD_HA_LIB="${REPO_ROOT}/lib/prod_ha.sh"

for library in "$CONFIG_LIB" "$PROD_HA_LIB"; do
  [[ -f "$library" && ! -L "$library" ]] || {
    printf 'ERROR: Missing library: %s\n' "$library" >&2
    exit 1
  }
done
# shellcheck source=../../lib/config.sh
source "$CONFIG_LIB"
# shellcheck source=../../lib/prod_ha.sh
source "$PROD_HA_LIB"

# shellcheck disable=SC2034 # Read by prod_acquire_lease in lib/prod_ha.sh.
LEASE_OWNER_LABEL="change-owner"
REPLICATION_TIMEOUT_SECONDS=3600
RELOCATE_TIMEOUT_SECONDS=900
TARGET_NODE=""
RELOCATED=false
REGISTRY_RECORDED=false
declare -a TARGET_STAGING=()

usage() {
  cat <<'EOF'
Usage: change_prod_vm_owner.sh [options]

Run from an administrator workstation. Lists registered production VMs,
prompts for one (default: the lowest-numbered active prodN), and determines its
real HA owner from live cluster state. A stale registry owner is corrected
first. The script then offers the VM's other placement hosts, and after
confirmation runs the equivalent of:

  ha-manager relocate vm:<VMID> <host>

Relocation shuts the VM down on its current host and starts it on the chosen
host, so the application is briefly unavailable. Replication runs to the
chosen host first to keep that window short. Afterward the script waits for
Proxmox to reverse the replication jobs and records the new owner and
replication targets in the registry.

The VM must not have staging VMs derived from it; destroy them first with
guests/staging/remove_staging_vm.sh. Staging VMs of other production VMs on
the chosen host are evicted when production starts there.

Options:
  --replication-timeout-seconds N Default: 3600
  -h, --help
EOF
}

parse_args() {
  while (($#)); do
    case "$1" in
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

cleanup() {
  local code=$?
  trap - EXIT
  set +e
  if ((code != 0)); then
    printf '\nFailed while %s.\n' "$CURRENT_PHASE" >&2
    if [[ "$RELOCATED" == true && "$REGISTRY_RECORDED" != true ]]; then
      printf 'HA relocation of %s to %s was requested, but the registry was not updated.\n' \
        "$RESOURCE_NAME" "$TARGET_NODE" >&2
      printf 'Once it runs on %s, rerun this script: it records the live owner and\n' \
        "$TARGET_NODE" >&2
      printf 'then offers the remaining hosts (answer q to stop there).\n' >&2
    fi
  fi
  prod_release_lease_quietly
  exit "$code"
}

# The owner may only change when HA, replication, and registry placement agree.
validate_strict_layout() {
  CURRENT_PHASE="validating placement, HA, and replication agreement"
  local node expected=() actual=()
  mapfile -t expected < <(sort_nodes "${PLACEMENT_NODES[@]}")
  mapfile -t actual < <(sort_nodes "${RULE_NODES[@]}")
  [[ "${expected[*]}" == "${actual[*]}" ]] ||
    die "HA rule nodes (${RULE_NODES[*]}) differ from registry placement (${PLACEMENT_NODES[*]}); finish the placement change with guests/prod/change_prod_vm_placement.sh first"
  local -a want_targets=() have_targets=()
  for node in "${PLACEMENT_NODES[@]}"; do
    [[ "$node" == "$OWNER_NODE" ]] || want_targets+=("$node")
  done
  local job
  for job in "${REPLICATION_JOBS[@]}"; do
    have_targets+=("${REPLICATION_TARGET[$job]}")
  done
  mapfile -t expected < <(sort_nodes "${want_targets[@]}")
  mapfile -t actual < <(sort_nodes "${have_targets[@]}")
  [[ "${expected[*]}" == "${actual[*]}" ]] ||
    die "Replication targets (${have_targets[*]:-none}) differ from placement minus the owner (${want_targets[*]:-none}); repair them with guests/prod/change_prod_vm_placement.sh first"
  for node in "${PLACEMENT_NODES[@]}"; do
    list_contains "$node" "${ONLINE_NODES[@]}" ||
      die "Every placement host must be online; $node is offline"
  done
  prod_require_replication_healthy "$OWNER_NODE"
  info "HA rule, replication jobs, and registry placement agree; replication is healthy"
  mapfile -t actual < <(sort_nodes "${REGISTRY_TARGETS[@]}")
  if [[ "${expected[*]}" != "${actual[*]}" ]]; then
    registry_update_resource --replication-targets "$(join_csv "${want_targets[@]}")"
    REGISTRY_TARGETS=("${want_targets[@]}")
    info "Registry replication targets updated to ${want_targets[*]:-none}"
  fi
}

# Print "NAME SOURCE" for every staging VM on NODE.
staging_on_node() {
  python3 - "${RUN_DIR}/resources.json" "$1" <<'PY'
import json
import sys
rows = json.load(open(sys.argv[1], encoding="utf-8"))
for row in rows:
    if row.get("kind") == "staging" and (row.get("owner_node") or row["placement"][0]) == sys.argv[2]:
        print(row["name"], row["source"])
PY
}

node_memory_line() {
  local node="$1" status
  status="$(pvesh_get "/nodes/${node}/status" 2>/dev/null)" || {
    printf 'memory unknown'
    return 0
  }
  python3 - "$status" <<'PY' || printf 'memory unknown'
import json
import sys
row = json.loads(sys.argv[1])
memory = row["memory"]
print(f"{memory['free'] // 2**20} MiB free of {memory['total'] // 2**20} MiB")
PY
}

choose_target() {
  CURRENT_PHASE="choosing the new owner"
  local -a candidates=() node
  for node in "${PLACEMENT_NODES[@]}"; do
    [[ "$node" == "$OWNER_NODE" ]] || candidates+=("$node")
  done
  mapfile -t candidates < <(sort_nodes "${candidates[@]}")
  ((${#candidates[@]} > 0)) ||
    die "$RESOURCE_NAME has no other placement host; add one with guests/prod/change_prod_vm_placement.sh"
  printf '\n%s runs on %s. It can be relocated to:\n' "$RESOURCE_NAME" "$OWNER_NODE"
  local staging memory
  local -a options=()
  for node in "${candidates[@]}"; do
    staging="$(staging_on_node "$node" | awk '{print $1}' | paste -sd, -)"
    memory="$(node_memory_line "$node")"
    printf '  %-6s %s; staging VMs: %s\n' "$node" "$memory" "${staging:-none}"
    options+=("$node" "$node · $memory; staging VMs: ${staging:-none}")
  done
  if bmac_ui_is_json; then
    bmac_ui_choose TARGET_NODE "New owner for $RESOURCE_NAME (now on $OWNER_NODE)" \
      "${candidates[0]}" "${options[@]}"
    list_contains "$TARGET_NODE" "${candidates[@]}" ||
      die "$TARGET_NODE is not a placement host of $RESOURCE_NAME"
    return 0
  fi
  while true; do
    prompt_with_default TARGET_NODE "New owner for $RESOURCE_NAME (q to quit)" \
      "${candidates[0]}"
    if [[ "${TARGET_NODE,,}" == q || "${TARGET_NODE,,}" == quit ]]; then
      log "No change was made"
      exit 0
    fi
    list_contains "$TARGET_NODE" "${candidates[@]}" && break
    warn "Choose one of: ${candidates[*]}"
  done
}

warn_about_target() {
  local name source
  TARGET_STAGING=()
  while read -r name source; do
    [[ -n "$name" ]] || continue
    TARGET_STAGING+=("$name (from $source)")
  done < <(staging_on_node "$TARGET_NODE")
  if ((${#TARGET_STAGING[@]} > 0)); then
    warn "Starting $RESOURCE_NAME on $TARGET_NODE stops and destroys these staging VMs there:"
    printf '  - %s\n' "${TARGET_STAGING[@]}" >&2
  fi
  if [[ "$VM_MEMORY_MB" =~ ^[0-9]+$ ]]; then
    local status free_mb
    status="$(pvesh_get "/nodes/${TARGET_NODE}/status" 2>/dev/null)" || status=""
    free_mb="$(
      python3 -c 'import json,sys; print(json.loads(sys.argv[1])["memory"]["free"] // 2**20)' \
        "$status" 2>/dev/null
    )" || free_mb=""
    if [[ "$free_mb" =~ ^[0-9]+$ ]] && ((free_mb < VM_MEMORY_MB)); then
      warn "$TARGET_NODE reports ${free_mb} MiB free memory; $RESOURCE_NAME uses ${VM_MEMORY_MB} MiB"
    fi
  fi
}

presync_target() {
  CURRENT_PHASE="replicating to $TARGET_NODE before relocation"
  local job baseline
  local -a result=()
  job="$(prod_job_for_target "$TARGET_NODE")" ||
    die "No replication job targets $TARGET_NODE"
  read -r -a result <<<"$(replication_job_state "$OWNER_NODE" "$job")"
  baseline="${result[1]:-0}"
  log "Replicating the latest changes to $TARGET_NODE"
  wait_for_replication "$OWNER_NODE" "$job" "$TARGET_NODE" "$baseline" \
    "$REPLICATION_TIMEOUT_SECONDS"
}

relocate() {
  CURRENT_PHASE="relocating $RESOURCE_NAME to $TARGET_NODE"
  log "ha-manager relocate vm:${VMID} ${TARGET_NODE}"
  RELOCATED=true
  node_exec "$COORDINATOR" ha-manager relocate "vm:${VMID}" "$TARGET_NODE" ||
    die "ha-manager relocate failed"
  local deadline=$((SECONDS + RELOCATE_TIMEOUT_SECONDS)) status observed
  info "Waiting up to ${RELOCATE_TIMEOUT_SECONDS} seconds for $RESOURCE_NAME to start on $TARGET_NODE"
  while ((SECONDS < deadline)); do
    status="$(node_exec "$COORDINATOR" ha-manager status 2>/dev/null || true)"
    observed="$(live_owner_now 2>/dev/null || true)"
    if [[ "$observed" == "$TARGET_NODE" ]] &&
      grep -Eq "^service vm:${VMID} \\(${TARGET_NODE}, started\\)$" <<<"$status"; then
      OWNER_NODE="$TARGET_NODE"
      info "$RESOURCE_NAME is running on $TARGET_NODE"
      return 0
    fi
    sleep 10
  done
  die "$RESOURCE_NAME did not start on $TARGET_NODE within ${RELOCATE_TIMEOUT_SECONDS} seconds; inspect 'ha-manager status'"
}

# Proxmox reverses the job that targeted the new owner so it targets the old
# owner; every other job keeps its target and now runs from the new owner.
verify_reversed_replication() {
  CURRENT_PHASE="verifying reversed replication"
  local deadline=$((SECONDS + 300)) node job
  local -a want=() have=() expected=() actual=()
  for node in "${PLACEMENT_NODES[@]}"; do
    [[ "$node" == "$OWNER_NODE" ]] || want+=("$node")
  done
  mapfile -t expected < <(sort_nodes "${want[@]}")
  while true; do
    prod_load_live_state
    have=()
    for job in "${REPLICATION_JOBS[@]}"; do
      have+=("${REPLICATION_TARGET[$job]}")
    done
    mapfile -t actual < <(sort_nodes "${have[@]}")
    [[ "${expected[*]}" == "${actual[*]}" ]] && break
    ((SECONDS < deadline)) ||
      die "Replication targets (${have[*]:-none}) did not become placement minus the new owner (${want[*]})"
    sleep 10
  done
  local -a result=()
  for job in "${REPLICATION_JOBS[@]}"; do
    read -r -a result <<<"$(replication_job_state "$OWNER_NODE" "$job")"
    wait_for_replication "$OWNER_NODE" "$job" "${REPLICATION_TARGET[$job]}" \
      "${result[1]:-0}" "$REPLICATION_TIMEOUT_SECONDS"
  done
}

record_registry_owner() {
  CURRENT_PHASE="recording the new owner in the registry"
  local -a targets=() node
  for node in "${PLACEMENT_NODES[@]}"; do
    [[ "$node" == "$OWNER_NODE" ]] || targets+=("$node")
  done
  registry_update_resource --owner-node "$OWNER_NODE" \
    --replication-targets "$(join_csv "${targets[@]}")"
  REGISTRY_RECORDED=true
  info "Registry owner of $RESOURCE_NAME is $OWNER_NODE; replication targets: ${targets[*]}"
}

main() {
  parse_args "$@"
  prod_load_config
  prod_preflight_workstation change-prod-vm-owner
  trap cleanup EXIT

  prod_discover_cluster
  prod_select_production "Production VM to relocate"
  prod_load_live_state
  prod_reconcile_registry_owner
  prod_refuse_staging_dependents
  log "Current state of $RESOURCE_NAME"
  prod_show_live_state
  validate_strict_layout
  choose_target
  warn_about_target

  log "Relocation plan"
  info "$RESOURCE_NAME (VMID $VMID): $OWNER_NODE -> $TARGET_NODE"
  info "Equivalent command: ha-manager relocate vm:${VMID} ${TARGET_NODE}"
  info "The VM shuts down on $OWNER_NODE and starts on $TARGET_NODE; expect a short outage."
  confirm_go "This relocates $RESOURCE_NAME to $TARGET_NODE."

  prod_acquire_lease
  prod_require_unchanged
  presync_target
  prod_require_unchanged
  local previous="$OWNER_NODE"
  relocate
  verify_reversed_replication
  record_registry_owner
  prod_release_lease

  log "Production owner change complete"
  info "$RESOURCE_NAME (VMID $VMID) moved from $previous to $OWNER_NODE"
  info "Replication: $(for job in "${REPLICATION_JOBS[@]}"; do printf '%s->%s ' "$job" "${REPLICATION_TARGET[$job]}"; done)"
  info "Verify the application, for example with: ssh $RESOURCE_NAME"
  bmac_ui_result resource "$RESOURCE_NAME" vmid "$VMID" previous_owner "$previous" owner "$OWNER_NODE"
  bmac_ui_next_step "Verify the application on its new owner." --command "ssh $RESOURCE_NAME"
  bmac_ui_next_step "Check the production VM state." --workflow show_prod_vm_state --arg "resource=$RESOURCE_NAME"
}

if [[ "${CHANGE_PROD_OWNER_SOURCE_ONLY:-0}" != 1 ]]; then
  bmac_ui_bootstrap "$@"
  main "$@"
fi
