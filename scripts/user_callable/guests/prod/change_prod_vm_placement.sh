#!/usr/bin/env bash

# Copyright (c) 2026 BEENTHERE VENTURES, INC.
# SPDX-License-Identifier: GPL-3.0-only

# Add hosts to or remove hosts from one production VM's placement. Adding
# records the hosts in the registry, replicates the VM's disks to them, and
# only then lets Proxmox HA place the VM there. Removing first stops HA from
# using the hosts, then records the narrower placement, then deletes their
# replication jobs and replicas.

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
LEASE_OWNER_LABEL="change-placement"
POOL_RESERVE_PERCENT=10
MIN_PLACEMENT_HOSTS=2
REPLICATION_TIMEOUT_SECONDS=3600
JOB_REMOVAL_TIMEOUT_SECONDS=600
REPLICA_BYTES=""
POOL_NAME=""
HOOK_REF=""
declare -a REQUESTED_NODES=()

usage() {
  cat <<'EOF'
Usage: change_prod_vm_placement.sh [options]

Run from an administrator workstation. Lists registered production VMs,
prompts for one (default: the lowest-numbered active prodN), then asks whether
to add or remove placement hosts and which ones.

Adding a host requires it to be online, to have room under its
MAX_PROD_VM_COUNT_ON_THIS_HOST limit (env/moxN.conf), and to keep 10% of its
pool size available after receiving a replica. The script records the host in
the registry placement, creates a replication job from the live owner, waits
for a successful initial replication, and only then adds the host to the
VM's strict HA node-affinity rule.

Removing a host never removes the live owner (move the VM first with
change_prod_vm_owner.sh) and keeps at least two placement hosts. HA stops
using the host first, then the registry placement narrows, then the
replication job and the host's replica are deleted.

The VM must not have staging VMs derived from it. If an earlier run stopped
partway, the script detects the partial change and offers to finish it.

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
    printf 'Correct the fault and rerun this script for %s; it detects a partial\n' \
      "${RESOURCE_NAME:-the VM}" >&2
    printf 'placement change and offers to finish it.\n' >&2
  fi
  prod_release_lease_quietly
  exit "$code"
}

# parse_node_list INPUT ALLOWED... prints the requested moxN names, one per
# line, refusing duplicates and names outside ALLOWED.
parse_node_list() {
  python3 - "$@" <<'PY'
import re
import sys

raw, allowed = sys.argv[1], sys.argv[2:]
nodes = [part for part in re.split(r"[\s,]+", raw.strip()) if part]
if not nodes:
    raise SystemExit("enter at least one host")
if len(set(nodes)) != len(nodes):
    raise SystemExit("a host is listed more than once")
invalid = [node for node in nodes if node not in allowed]
if invalid:
    raise SystemExit("not offered: " + ", ".join(invalid))
print(*sorted(nodes, key=lambda node: int(node[3:])), sep="\n")
PY
}

production_count_on() {
  python3 - "${RUN_DIR}/resources.json" "$1" <<'PY'
import json
import sys
rows = json.load(open(sys.argv[1], encoding="utf-8"))
print(sum(
    1 for row in rows
    if row.get("kind") == "production" and sys.argv[2] in row.get("placement", [])
))
PY
}

production_limit_on() {
  bash -c '
set -Eeuo pipefail
source "$1"
load_proxmox_config --host "$2" --no-secrets >/dev/null
printf "%s\n" "$MAX_PROD_VM_COUNT_ON_THIS_HOST"
' bash "$CONFIG_LIB" "$1" 2>/dev/null
}

# Measure what a new replica needs: the summed ZFS "used" of every VM volume
# on the live owner, the owner's pool, and the hookscript the VM starts with.
measure_replica() {
  CURRENT_PHASE="measuring the production volumes on $OWNER_NODE"
  local measured config
  # shellcheck disable=SC2016 # This script is intentionally evaluated remotely.
  measured="$(
    node_exec "$OWNER_NODE" bash -c '
set -Eeuo pipefail
vmid="$1"; storage="$2"
total=0; pool=""; count=0
while read -r volume; do
  [[ -n "$volume" ]] || continue
  path="$(pvesm path "$volume")"
  case "$path" in
    /dev/zvol/*) dataset="${path#/dev/zvol/}" ;;
    *) printf "volume is not a ZFS zvol: %s\n" "$path" >&2; exit 1 ;;
  esac
  [[ "$dataset" =~ ^[A-Za-z0-9._/+:-]+$ ]]
  used="$(zfs get -Hp -o value used "$dataset")"
  [[ "$used" =~ ^[0-9]+$ ]]
  total=$((total + used))
  pool="${dataset%%/*}"
  count=$((count + 1))
done < <(pvesm list "$storage" --vmid "$vmid" | awk "NR > 1 && NF { print \$1 }")
((count > 0))
printf "%s\t%s\n" "$total" "$pool"
' bash "$VMID" "$PROD_VM_STORAGE"
  )" || die "Could not measure the volumes of $RESOURCE_NAME on $OWNER_NODE"
  IFS=$'\t' read -r REPLICA_BYTES POOL_NAME <<<"$measured"
  [[ "$REPLICA_BYTES" =~ ^[0-9]+$ && "$POOL_NAME" =~ ^[A-Za-z0-9._-]+$ ]] ||
    die "$OWNER_NODE returned an invalid volume measurement"
  config="$(node_exec "$OWNER_NODE" qm config "$VMID")" ||
    die "Could not read the VM configuration from $OWNER_NODE"
  HOOK_REF="$(awk -F': ' '$1 == "hookscript" { print $2; exit }' <<<"$config")"
  [[ "$HOOK_REF" =~ ^[A-Za-z0-9._-]+:snippets/[A-Za-z0-9._-]+$ ]] ||
    die "$RESOURCE_NAME has no valid guest-role hookscript"
}

# Print "OK", or one reason NODE cannot receive this VM.
add_blocker() {
  local node="$1" limit count measured size available volumes reserve
  list_contains "$node" "${ONLINE_NODES[@]}" || {
    printf 'offline\n'
    return 0
  }
  limit="$(production_limit_on "$node")" || {
    printf 'env/%s.conf is missing or invalid\n' "$node"
    return 0
  }
  [[ "$limit" =~ ^[1-9][0-9]*$ ]] || {
    printf 'MAX_PROD_VM_COUNT_ON_THIS_HOST is invalid in env/%s.conf\n' "$node"
    return 0
  }
  count="$(production_count_on "$node")"
  ((count < limit)) || {
    printf 'already holds %s of %s production VMs (MAX_PROD_VM_COUNT_ON_THIS_HOST)\n' \
      "$count" "$limit"
    return 0
  }
  # shellcheck disable=SC2016 # This script is intentionally evaluated remotely.
  measured="$(
    node_exec "$node" bash -c '
set -Eeuo pipefail
storage="$1"; pool="$2"; vmid="$3"; hook="$4"
pvesm status --storage "$storage" | awk -v id="$storage" "\$1 == id && \$3 == \"active\" { found=1 } END { exit !found }"
hook_path="$(pvesm path "$hook")"
[[ -x "$hook_path" ]]
volumes="$(pvesm list "$storage" --vmid "$vmid" | awk "NR > 1 && NF" | wc -l)"
printf "%s\t%s\t%s\n" "$(zpool list -Hp -o size "$pool")" \
  "$(zfs get -Hp -o value available "$pool")" "$volumes"
' bash "$PROD_VM_STORAGE" "$POOL_NAME" "$VMID" "$HOOK_REF" 2>/dev/null
  )" || {
    printf '%s is not active, the hookscript is missing, or pool %s is absent\n' \
      "$PROD_VM_STORAGE" "$POOL_NAME"
    return 0
  }
  IFS=$'\t' read -r size available volumes <<<"$measured"
  [[ "$size" =~ ^[0-9]+$ && "$available" =~ ^[0-9]+$ && "$volumes" =~ ^[0-9]+$ ]] || {
    printf 'returned invalid pool values\n'
    return 0
  }
  ((volumes == 0)) || {
    printf 'already has %s volume(s) for VMID %s; remove the leftovers first\n' \
      "$volumes" "$VMID"
    return 0
  }
  reserve=$(((size * POOL_RESERVE_PERCENT + 99) / 100))
  ((available - REPLICA_BYTES >= reserve)) || {
    printf 'pool %s: %s bytes available, replica needs %s bytes, %s%% reserve is %s bytes\n' \
      "$POOL_NAME" "$available" "$REPLICA_BYTES" "$POOL_RESERVE_PERCENT" "$reserve"
    return 0
  }
  printf 'OK %s\n' "$((available - REPLICA_BYTES - reserve))"
}

registry_set_placement() {
  local -a placement=("$@") targets=() node
  for node in "${placement[@]}"; do
    [[ "$node" == "$OWNER_NODE" ]] || targets+=("$node")
  done
  registry_update_resource \
    --placement "$(join_csv "${placement[@]}")" \
    --ha-nodes "$(join_csv "${placement[@]}")" \
    --replication-targets "$(join_csv "${targets[@]}")"
  PLACEMENT_NODES=("${placement[@]}")
  REGISTRY_HA_NODES=("${placement[@]}")
  REGISTRY_TARGETS=("${targets[@]}")
  info "Registry placement of $RESOURCE_NAME: ${placement[*]}"
}

registry_set_targets() {
  local -a targets=("$@")
  registry_update_resource --replication-targets "$(join_csv "${targets[@]}")"
  REGISTRY_TARGETS=("${targets[@]}")
  info "Registry replication targets of $RESOURCE_NAME: ${targets[*]:-none}"
}

next_job_id() {
  local index=0 job
  while true; do
    job="${VMID}-${index}"
    list_contains "$job" "${REPLICATION_JOBS[@]}" || break
    ((index += 1))
  done
  printf '%s\n' "$job"
}

ensure_replica() {
  local node="$1" job schedule
  CURRENT_PHASE="replicating $RESOURCE_NAME to $node"
  if ! job="$(prod_job_for_target "$node")"; then
    job="$(next_job_id)"
    schedule="${REPLICATION_SCHEDULE:-*/${PROD_VM_REPLICATION_INTERVAL##*/}}"
    [[ "$schedule" =~ ^\*/[1-9][0-9]*$ ]] ||
      die "Replication schedule is invalid: $schedule"
    # pvesr create-local-job must run on the VM's current source node.
    node_exec "$OWNER_NODE" pvesr create-local-job "$job" "$node" \
      --schedule "$schedule" \
      --comment "Production ${RESOURCE_NAME} replica" ||
      die "Could not create replication job $job to $node"
    REPLICATION_JOBS+=("$job")
    REPLICATION_TARGET["$job"]="$node"
    info "Created replication job $job to $node ($schedule)"
  fi
  wait_for_replication "$OWNER_NODE" "$job" "$node" 0 "$REPLICATION_TIMEOUT_SECONDS"
}

# Delete the replication jobs that target NODES, which also removes their
# replicas, and wait until Proxmox has removed them.
remove_replicas() {
  local node job deadline remaining volumes
  local -a jobs=()
  for node in "$@"; do
    if job="$(prod_job_for_target "$node")"; then
      jobs+=("$job")
    fi
  done
  ((${#jobs[@]} > 0)) || return 0
  CURRENT_PHASE="removing replication to $*"
  for job in "${jobs[@]}"; do
    node_exec "$COORDINATOR" pvesr delete "$job" ||
      die "Could not delete replication job $job"
    info "Deleting replication job $job and its replica on ${REPLICATION_TARGET[$job]}"
  done
  deadline=$((SECONDS + JOB_REMOVAL_TIMEOUT_SECONDS))
  while true; do
    remaining="$(
      pvesh_get /cluster/replication 2>/dev/null |
        python3 -c '
import json, sys
jobs = set(sys.argv[1:])
print(" ".join(sorted(row["id"] for row in json.load(sys.stdin) if row.get("id") in jobs)))
' "${jobs[@]}" 2>/dev/null
    )" || remaining="unknown"
    [[ -n "$remaining" ]] || break
    ((SECONDS < deadline)) ||
      die "Replication jobs did not finish removal: $remaining"
    sleep 5
  done
  for job in "${jobs[@]}"; do
    node="${REPLICATION_TARGET[$job]}"
    unset 'REPLICATION_TARGET[$job]'
    volumes="$(node_exec "$node" pvesm list "$PROD_VM_STORAGE" --vmid "$VMID" 2>/dev/null |
      awk 'NR > 1 && NF' | wc -l)" || volumes=unknown
    if [[ "$volumes" != 0 ]]; then
      warn "$node still lists $volumes volume(s) for VMID $VMID; inspect 'pvesm list $PROD_VM_STORAGE --vmid $VMID' there"
    else
      info "The replica on $node is gone"
    fi
  done
  local -a kept=()
  for job in "${REPLICATION_JOBS[@]}"; do
    list_contains "$job" "${jobs[@]}" || kept+=("$job")
  done
  REPLICATION_JOBS=("${kept[@]}")
}

placement_minus() {
  local node
  for node in "${PLACEMENT_NODES[@]}"; do
    list_contains "$node" "$@" || printf '%s\n' "$node"
  done
}

add_hosts() {
  local -a added=("$@") placement=() node
  placement=("${PLACEMENT_NODES[@]}")
  for node in "${added[@]}"; do
    list_contains "$node" "${placement[@]}" || placement+=("$node")
  done
  if [[ "${placement[*]}" != "${PLACEMENT_NODES[*]}" ]]; then
    CURRENT_PHASE="recording the wider placement in the registry"
    registry_set_placement "${placement[@]}"
  fi
  for node in "${added[@]}"; do
    ensure_replica "$node"
  done
  CURRENT_PHASE="widening the HA rule"
  set_ha_rule_nodes "${placement[@]}"
  local -a targets=()
  for node in "${placement[@]}"; do
    [[ "$node" == "$OWNER_NODE" ]] || targets+=("$node")
  done
  registry_set_targets "${targets[@]}"
}

remove_hosts() {
  local -a removed=("$@") remaining=()
  mapfile -t remaining < <(placement_minus "${removed[@]}")
  list_contains "$OWNER_NODE" "${remaining[@]}" ||
    die "The live owner $OWNER_NODE cannot be removed; move the VM first with change_prod_vm_owner.sh"
  CURRENT_PHASE="narrowing the HA rule"
  set_ha_rule_nodes "${remaining[@]}"
  CURRENT_PHASE="recording the narrower placement in the registry"
  registry_set_placement "${remaining[@]}"
  remove_replicas "${removed[@]}"
}

# Finish an earlier run that stopped partway. The registry placement is the
# record of intent; the HA rule may lag it (an unfinished add, or a removal
# whose registry step had not run) and replication may hold jobs outside it.
repair_partial_change() {
  local node job choice
  local -a missing=() orphans=() rule_extra=()
  for node in "${RULE_NODES[@]}"; do
    list_contains "$node" "${PLACEMENT_NODES[@]}" || rule_extra+=("$node")
  done
  ((${#rule_extra[@]} == 0)) ||
    die "HA rule $HA_RULE_ID allows ${rule_extra[*]}, which is outside the registry placement ${PLACEMENT_NODES[*]}; correct it manually"
  for node in "${PLACEMENT_NODES[@]}"; do
    list_contains "$node" "${RULE_NODES[@]}" || missing+=("$node")
  done
  for job in "${REPLICATION_JOBS[@]}"; do
    list_contains "${REPLICATION_TARGET[$job]}" "${PLACEMENT_NODES[@]}" ||
      orphans+=("${REPLICATION_TARGET[$job]}")
  done
  local -a want_targets=() have_targets=()
  for node in "${PLACEMENT_NODES[@]}"; do
    [[ "$node" == "$OWNER_NODE" ]] || want_targets+=("$node")
  done
  for job in "${REPLICATION_JOBS[@]}"; do
    have_targets+=("${REPLICATION_TARGET[$job]}")
  done
  local registry_mismatch=false
  [[ "$(sort_nodes "${REGISTRY_HA_NODES[@]}")" == "$(sort_nodes "${PLACEMENT_NODES[@]}")" ]] ||
    registry_mismatch=true
  [[ "$(sort_nodes "${REGISTRY_TARGETS[@]}")" == "$(sort_nodes "${have_targets[@]}")" ]] ||
    registry_mismatch=true
  if ((${#missing[@]} == 0 && ${#orphans[@]} == 0)) &&
    [[ "$registry_mismatch" == false ]] &&
    [[ "$(sort_nodes "${have_targets[@]}")" == "$(sort_nodes "${want_targets[@]}")" ]]; then
    return 0
  fi

  log "Unfinished placement change detected"
  ((${#missing[@]} == 0)) ||
    info "Registry placement includes ${missing[*]}, but the HA rule does not"
  ((${#orphans[@]} == 0)) ||
    info "Replication still targets ${orphans[*]}, which is outside the registry placement"
  [[ "$registry_mismatch" == false ]] ||
    info "Registry HA nodes or replication targets differ from live state"
  confirm_go "The script will finish the earlier change before offering a new one."
  prod_acquire_lease
  prod_require_unchanged
  if ((${#missing[@]} > 0)); then
    while true; do
      if bmac_ui_is_json; then
        bmac_ui_choose choice "Finish the earlier change for ${missing[*]}" a \
          a "Finish adding ${missing[*]}" r "Finish removing ${missing[*]}"
      else
        prompt_with_default choice \
          "Finish adding (a) or finish removing (r) ${missing[*]}" a
      fi
      case "${choice,,}" in
        a | add) add_hosts "${missing[@]}"; break ;;
        r | remove)
          list_contains "$OWNER_NODE" "${missing[@]}" &&
            die "The live owner $OWNER_NODE cannot be removed"
          remove_hosts "${missing[@]}"
          break
          ;;
        *) warn "Enter a or r" ;;
      esac
    done
  fi
  ((${#orphans[@]} == 0)) || remove_replicas "${orphans[@]}"
  local -a targets=()
  for node in "${PLACEMENT_NODES[@]}"; do
    [[ "$node" == "$OWNER_NODE" ]] || targets+=("$node")
  done
  for node in "${targets[@]}"; do
    prod_job_for_target "$node" >/dev/null || ensure_replica "$node"
  done
  [[ "$(sort_nodes "${REGISTRY_HA_NODES[@]}")" == "$(sort_nodes "${PLACEMENT_NODES[@]}")" ]] ||
    registry_set_placement "${PLACEMENT_NODES[@]}"
  [[ "$(sort_nodes "${REGISTRY_TARGETS[@]}")" == "$(sort_nodes "${targets[@]}")" ]] ||
    registry_set_targets "${targets[@]}"
  prod_release_lease
  info "The earlier placement change is finished"
}

choose_add_hosts() {
  local -a candidates=() eligible=() node
  local blocker input parsed
  for node in "${CLUSTER_NODES[@]}"; do
    list_contains "$node" "${PLACEMENT_NODES[@]}" || candidates+=("$node")
  done
  ((${#candidates[@]} > 0)) ||
    die "Every cluster member already holds $RESOURCE_NAME; add a host with hosts/add_proxmox_host.sh first"
  measure_replica
  printf '\nHosts that can be added (replica needs %s bytes in pool %s):\n' \
    "$REPLICA_BYTES" "$POOL_NAME"
  for node in "${candidates[@]}"; do
    blocker="$(add_blocker "$node")"
    if [[ "$blocker" == OK\ * ]]; then
      eligible+=("$node")
      printf '  %-6s eligible; %s bytes would remain above the %s%% reserve\n' \
        "$node" "${blocker#OK }" "$POOL_RESERVE_PERCENT"
    else
      printf '  %-6s not eligible: %s\n' "$node" "$blocker"
    fi
  done
  ((${#eligible[@]} > 0)) || die "No cluster member can receive $RESOURCE_NAME"
  local -a options=()
  for node in "${eligible[@]}"; do
    options+=(--option "$node" "$node")
  done
  if bmac_ui_is_json; then
    bmac_ui_input input --id add_hosts --type multiselect --label "Hosts to add" \
      --help "Each new host receives a replica of $REPLICA_BYTES bytes in pool $POOL_NAME." \
      --default "${eligible[0]}" --required --min-selected 1 "${options[@]}"
  fi
  while true; do
    bmac_ui_is_json ||
      prompt_with_default input "Hosts to add (comma-separated)" "${eligible[0]}"
    if parsed="$(parse_node_list "$input" "${eligible[@]}" 2>"${RUN_DIR}/parse.err")"; then
      mapfile -t REQUESTED_NODES <<<"$parsed"
      break
    fi
    if bmac_ui_is_json; then
      bmac_ui_reask input "$(<"${RUN_DIR}/parse.err")"
    else
      warn "$(<"${RUN_DIR}/parse.err")"
    fi
  done
}

choose_remove_hosts() {
  local -a candidates=() node remaining=()
  local input parsed
  for node in "${PLACEMENT_NODES[@]}"; do
    [[ "$node" == "$OWNER_NODE" ]] || candidates+=("$node")
  done
  mapfile -t candidates < <(sort_nodes "${candidates[@]}")
  ((${#PLACEMENT_NODES[@]} > MIN_PLACEMENT_HOSTS)) ||
    die "$RESOURCE_NAME has ${#PLACEMENT_NODES[@]} placement hosts; at least $MIN_PLACEMENT_HOSTS must remain. Add a host first."
  printf '\n%s runs on %s, which cannot be removed (move it first with change_prod_vm_owner.sh).\n' \
    "$RESOURCE_NAME" "$OWNER_NODE"
  printf 'Hosts that can be removed: %s\n' "${candidates[*]}"
  local -a options=()
  for node in "${candidates[@]}"; do
    options+=(--option "$node" "$node")
  done
  if bmac_ui_is_json; then
    bmac_ui_input input --id remove_hosts --type multiselect --label "Hosts to remove" \
      --help "$OWNER_NODE runs $RESOURCE_NAME and cannot be removed; at least $MIN_PLACEMENT_HOSTS placement hosts must remain." \
      --required --min-selected 1 \
      --max-selected "$((${#PLACEMENT_NODES[@]} - MIN_PLACEMENT_HOSTS))" "${options[@]}"
  fi
  local message
  while true; do
    bmac_ui_is_json ||
      IFS= read -r -p "Hosts to remove (comma-separated): " input ||
      die "Input ended; no change was made"
    if parsed="$(parse_node_list "$input" "${candidates[@]}" 2>"${RUN_DIR}/parse.err")"; then
      mapfile -t REQUESTED_NODES <<<"$parsed"
      mapfile -t remaining < <(placement_minus "${REQUESTED_NODES[@]}")
      if ((${#remaining[@]} >= MIN_PLACEMENT_HOSTS)); then
        break
      fi
      message="At least $MIN_PLACEMENT_HOSTS placement hosts must remain"
    else
      message="$(<"${RUN_DIR}/parse.err")"
    fi
    if bmac_ui_is_json; then
      bmac_ui_reask input "$message"
    else
      warn "$message"
    fi
  done
}

main() {
  parse_args "$@"
  prod_load_config
  require_var PROD_VM_REPLICATION_INTERVAL || die "Configuration is incomplete"
  prod_preflight_workstation change-prod-vm-placement
  trap cleanup EXIT

  prod_discover_cluster
  prod_select_production "Production VM to change"
  prod_load_live_state
  prod_reconcile_registry_owner
  prod_refuse_staging_dependents
  log "Current state of $RESOURCE_NAME"
  prod_show_live_state
  repair_partial_change

  local action
  while true; do
    printf '\n'
    if bmac_ui_is_json; then
      bmac_ui_choose action "Change the placement of $RESOURCE_NAME" a \
        a "Add placement hosts" r "Remove placement hosts"
    else
      prompt_with_default action "Add or remove placement hosts? (a/r, q to quit)" a
    fi
    case "${action,,}" in
      a | add) action=add; break ;;
      r | remove) action=remove; break ;;
      q | quit)
        log "No change was made"
        exit 0
        ;;
      *) warn "Enter a, r, or q" ;;
    esac
  done

  local -a final=()
  if [[ "$action" == add ]]; then
    choose_add_hosts
    final=("${PLACEMENT_NODES[@]}" "${REQUESTED_NODES[@]}")
  else
    prod_require_replication_healthy "$OWNER_NODE"
    choose_remove_hosts
    mapfile -t final < <(placement_minus "${REQUESTED_NODES[@]}")
  fi

  log "Placement plan"
  info "$RESOURCE_NAME (VMID $VMID) running on $OWNER_NODE"
  info "Placement: ${PLACEMENT_NODES[*]} -> ${final[*]}"
  if [[ "$action" == add ]]; then
    info "Replicates to ${REQUESTED_NODES[*]} first; HA may use them only after a successful replication."
  else
    info "HA stops using ${REQUESTED_NODES[*]} first; their replication jobs and replicas are then deleted."
  fi
  bmac_ui_plan_begin "Change the placement of $RESOURCE_NAME" "Placement: ${PLACEMENT_NODES[*]} -> ${final[*]}"
  local requested
  for requested in "${REQUESTED_NODES[@]}"; do
    if [[ "$action" == add ]]; then
      bmac_ui_plan_item create "$requested" "Replicate $RESOURCE_NAME here, then let HA use it after a successful replication"
    else
      bmac_ui_plan_item remove "$requested" "HA stops using it; its replication job and replica are deleted"
    fi
  done
  bmac_ui_plan_item keep "$OWNER_NODE" "$RESOURCE_NAME keeps running here"
  bmac_ui_plan_end
  confirm_go "This changes the placement of $RESOURCE_NAME."

  prod_acquire_lease
  prod_require_unchanged
  if [[ "$action" == add ]]; then
    measure_replica
    local node blocker
    for node in "${REQUESTED_NODES[@]}"; do
      blocker="$(add_blocker "$node")"
      [[ "$blocker" == OK\ * ]] || die "$node can no longer receive $RESOURCE_NAME: $blocker"
    done
    add_hosts "${REQUESTED_NODES[@]}"
  else
    remove_hosts "${REQUESTED_NODES[@]}"
  fi
  prod_release_lease

  log "Production placement change complete"
  info "$RESOURCE_NAME placement: ${PLACEMENT_NODES[*]} (running on $OWNER_NODE)"
  info "HA rule $HA_RULE_ID: ${RULE_NODES[*]}"
  local job
  for job in "${REPLICATION_JOBS[@]}"; do
    info "Replication $job -> ${REPLICATION_TARGET[$job]}"
  done
  bmac_ui_result resource "$RESOURCE_NAME" owner "$OWNER_NODE" \
    placement "${PLACEMENT_NODES[*]}" ha_rule "$HA_RULE_ID"
  bmac_ui_next_step "Check the production VM state and replication health." --workflow show_prod_vm_state --arg "resource=$RESOURCE_NAME"
}

if [[ "${CHANGE_PROD_PLACEMENT_SOURCE_ONLY:-0}" != 1 ]]; then
  bmac_ui_bootstrap "$@"
  main "$@"
fi
