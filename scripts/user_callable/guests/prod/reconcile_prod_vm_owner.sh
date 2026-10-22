#!/usr/bin/env bash

# Copyright (c) 2026 BEENTHERE VENTURES, INC.
# SPDX-License-Identifier: GPL-3.0-only

# Record where every active production VM actually runs. After an HA failover
# the VM stays on the host it restarted on (failback is off) and Proxmox turns
# its replication jobs around, but the registry still names the old owner and
# targets. This script copies the live owner and replication targets into the
# registry. It never moves, stops, or restarts a VM.

set -Eeuo pipefail
set +x
umask 077

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
REPO_ROOT="$(cd -- "${SCRIPT_DIR}/../../../.." && pwd -P)"
# shellcheck source=../../../lib/ui_protocol.sh
source "${REPO_ROOT}/scripts/lib/ui_protocol.sh"
bmac_ui_bootstrap "$@"
CONFIG_LIB="${REPO_ROOT}/scripts/lib/config.sh"
REMOTE_REGISTRY="/usr/local/lib/app-ha-proxmox/lib/cluster_registry.py"

DRY_RUN=false
ASSUME_YES=false
COORDINATOR=""
RUN_DIR=""
declare -a PLAN=()

log() {
  printf '\n==> %s\n' "$*"
  bmac_ui_step "$*"
}

info() {
  printf '    %s\n' "$*"
}

die() {
  printf '\nERROR: %s\n' "$*" >&2
  bmac_ui_error failed "$*"
  exit 1
}

usage() {
  cat <<'EOF'
Usage: reconcile_prod_vm_owner.sh [--dry-run] [--yes]

Records in the cluster registry the host each active production VM runs on,
and the replication targets Proxmox is using for it. Use it after an HA
failover, when the dashboard reports "prodN runs on moxB but the registry
owner is moxA". Nothing is moved, stopped, or restarted; only registry
records change. A VM whose owner or move is in progress (an orchestration
lease is held) is left alone.

  --dry-run  Show what would be recorded and change nothing.
  --yes      Skip the confirmation.
EOF
}

parse_args() {
  while (($#)); do
    case "$1" in
      --dry-run) DRY_RUN=true; shift ;;
      --yes) ASSUME_YES=true; shift ;;
      -h | --help) usage; exit 0 ;;
      *) usage >&2; exit 2 ;;
    esac
  done
}

cleanup() {
  [[ -z "$RUN_DIR" ]] || rm -rf -- "$RUN_DIR"
}

registry_cmd() {
  mox_ssh "$COORDINATOR" "$REMOTE_REGISTRY" \
    --state-dir "$CLUSTER_STATE_DIR" "$@" </dev/null
}

pvesh_get() {
  mox_ssh "$COORDINATOR" pvesh get "$@" --output-format json </dev/null
}

# Print one tab-separated line per active production VM:
# name, registry owner, live node, registry targets, live targets, action,
# where action is "record", "ok", or "skip: <reason>".
plan_rows() {
  python3 - "$1" "$2" "$3" <<'PY'
import json
import sys

resources, vms, jobs = (json.load(open(path, encoding="utf-8")) for path in sys.argv[1:4])


def vmid_of(row):
    try:
        return int(row.get("vmid", row.get("guest")))
    except (TypeError, ValueError):
        return None


for resource in sorted(resources, key=lambda row: row.get("index", 0)):
    if resource.get("kind") != "production" or resource.get("state") != "active":
        continue
    vmid = resource["vmid"]
    placement = resource["placement"]
    registry_targets = resource["proxmox"]["replication_targets"]
    running = [
        row for row in vms
        if vmid_of(row) == vmid and row.get("type") == "qemu" and row.get("status") == "running"
    ]
    live_targets = sorted(
        (str(row.get("target")) for row in jobs
         if vmid_of(row) == vmid and row.get("disable") not in (1, True, "1")),
        key=lambda node: int(node[3:]) if node[3:].isdigit() else 99,
    )
    node = running[0].get("node") if len(running) == 1 else None
    if node is None:
        action = "skip: not running"
    elif running[0].get("name") != resource["name"]:
        action = "skip: live VM name differs from the registry"
    elif node not in placement:
        action = f"skip: {node} is outside placement {','.join(placement)}"
    elif node in live_targets or any(target not in placement for target in live_targets):
        action = "skip: replication jobs do not fit placement; run change_prod_vm_placement.sh"
    elif node == resource.get("owner_node") and live_targets == registry_targets:
        action = "ok"
    else:
        action = "record"
    print("\t".join((
        resource["name"],
        resource.get("owner_node") or "-",
        node or "-",
        ",".join(registry_targets) or "-",
        ",".join(live_targets) or "-",
        action,
    )))
PY
}

read_plan() {
  log "Compare registry owners with where production VMs run"
  registry_cmd list >"${RUN_DIR}/resources.json" ||
    die "Could not read the cluster registry"
  pvesh_get /cluster/resources --type vm >"${RUN_DIR}/vms.json" ||
    die "Could not read the cluster's VMs"
  pvesh_get /cluster/replication >"${RUN_DIR}/replication.json" ||
    die "Could not read the cluster's replication jobs"
  mapfile -t PLAN < <(
    plan_rows "${RUN_DIR}/resources.json" "${RUN_DIR}/vms.json" "${RUN_DIR}/replication.json"
  )
  local row name owner node targets live action
  printf '\n  %-8s %-14s %-10s %-20s %-20s %s\n' \
    NAME "REGISTRY OWNER" "RUNS ON" "REGISTRY TARGETS" "LIVE TARGETS" ACTION
  for row in "${PLAN[@]}"; do
    IFS=$'\t' read -r name owner node targets live action <<<"$row"
    printf '  %-8s %-14s %-10s %-20s %-20s %s\n' \
      "$name" "$owner" "$node" "$targets" "$live" "$action"
  done
  ((${#PLAN[@]} > 0)) || info "No active production VMs are registered."
}

pending_count() {
  local row count=0
  for row in "${PLAN[@]}"; do
    [[ "${row##*$'\t'}" == record ]] && count=$((count + 1))
  done
  printf '%s\n' "$count"
}

confirm_record() {
  [[ "$ASSUME_YES" == true ]] && return 0
  if bmac_ui_is_json; then
    bmac_ui_confirm --id record_owner --question \
      --title "Record where these production VMs run?" \
      --message "Only registry records change; no VM is moved or restarted." \
      --confirm-label "Record" --cancel-label "Cancel" ||
      die "Cancelled; nothing was changed"
    return 0
  fi
  local answer
  read -r -p "Record these owners in the registry? [y/N] " answer || answer=""
  [[ "$answer" =~ ^[Yy]([Ee][Ss])?$ ]] || die "Cancelled; nothing was changed"
}

record_owners() {
  log "Record production owners"
  local row name owner node targets live action result
  local -a recorded=()
  for row in "${PLAN[@]}"; do
    IFS=$'\t' read -r name owner node targets live action <<<"$row"
    [[ "$action" == record ]] || continue
    [[ "$live" != - ]] || live=""
    result="$(registry_cmd record-owner "$name" --node "$node" --replication-targets "$live")" ||
      die "Could not record the owner of $name"
    if python3 -c 'import json,sys; sys.exit(0 if json.loads(sys.argv[1])["changed"] else 1)' "$result"; then
      info "$name: owner $owner -> $node; replication targets ${live:-none}"
      recorded+=("$name")
    else
      info "$name: left unchanged ($(python3 -c 'import json,sys; print(json.loads(sys.argv[1]).get("reason", ""))' "$result"))"
    fi
  done
  bmac_ui_result recorded:raw "$(bmac_ui_json_array "${recorded[@]}")"
}

main() {
  parse_args "$@"
  # shellcheck source=../../../lib/config.sh
  source "$CONFIG_LIB"
  load_proxmox_config --no-secrets >/dev/null || die "Could not load cluster configuration"
  require_var CLUSTER_STATE_DIR || die "CLUSTER_STATE_DIR is not configured"
  RUN_DIR="$(mktemp -d "${TMPDIR:-/tmp}/reconcile-prod-owner.XXXXXX")"
  trap cleanup EXIT
  COORDINATOR="$(first_reachable_mox)" || die "No reachable mox host was found"
  info "Reading through $COORDINATOR"

  read_plan
  local pending
  pending="$(pending_count)"
  if ((pending == 0)); then
    printf '\nOK: the registry already matches where every production VM runs.\n'
    bmac_ui_result recorded:raw "[]"
    return 0
  fi
  if [[ "$DRY_RUN" == true ]]; then
    printf '\nDry run: %s production owner record(s) would change.\n' "$pending"
    bmac_ui_next_step "Record where the production VMs run." \
      --workflow reconcile_prod_vm_owner --arg "dry_run=false"
    return 0
  fi
  confirm_record
  record_owners
}

[[ "${RECONCILE_PROD_OWNER_SOURCE_ONLY:-0}" == 1 ]] || main "$@"
