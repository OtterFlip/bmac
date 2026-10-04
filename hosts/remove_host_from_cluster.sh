#!/usr/bin/env bash

# Copyright (c) 2026 BEENTHERE VENTURES, INC.
# SPDX-License-Identifier: GPL-3.0-only

# Gracefully remove one healthy host that no production VM, replica, staging
# VM, HA rule, route, or deferred cleanup depends on. The host is powered off,
# deleted from corosync and pmxcfs, the QDevice is reconciled so the vote count
# stays odd, and the host's registry slot is freed for reuse.

set -Eeuo pipefail
set +x
umask 077

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
REPO_ROOT="$(cd -- "${SCRIPT_DIR}/.." && pwd -P)"
CONFIG_LIB="${REPO_ROOT}/lib/config.sh"
CONTROL_LIB="${REPO_ROOT}/lib/cluster_control.sh"
MEMBERSHIP_LIB="${REPO_ROOT}/lib/host_membership.sh"

for library in "$CONFIG_LIB" "$CONTROL_LIB" "$MEMBERSHIP_LIB"; do
  [[ -f "$library" && ! -L "$library" ]] || {
    printf 'ERROR: Missing library: %s\n' "$library" >&2
    exit 1
  }
done
# shellcheck source=../lib/config.sh
source "$CONFIG_LIB"
# shellcheck source=../lib/cluster_control.sh
source "$CONTROL_LIB"
# shellcheck source=../lib/host_membership.sh
source "$MEMBERSHIP_LIB"

SHUTDOWN_TIMEOUT_SECONDS=600
LIVE_VMS_JSON=""
LIVE_RULES_JSON=""
LIVE_REPLICATION_JSON=""

CURRENT_PHASE="startup"
TARGET_HOST=""
TARGET_ONLINE=false
TARGET_KEY=""
NEW_CONTROL_NODE=""
CONTROL_CHANGED=false
CLUSTER_CHANGED=false
declare -a MEMBERS=()
declare -a ONLINE_MEMBERS=()

usage() {
  cat <<'EOF'
Usage: remove_host_from_cluster.sh [--host moxN]

Run from an administrator workstation. Removes one healthy cluster host that
nothing depends on: no production placement (move production away first with
guests/prod/change_prod_vm_owner.sh and guests/prod/change_prod_vm_placement.sh),
no replica, staging VM, HA rule, route, or pending deferred cleanup.

The script asks whether you can 'ssh qdevice', verifies every remaining member
is online and quorate, takes the cluster control-plane lock, removes the
QDevice vote, powers the host off, deletes it from the cluster, re-adds the
QDevice when the remaining member count is even, and frees the host's registry
slot so a future host can reuse its moxN name. If the host is the cluster
control node, you choose its replacement first.

At least two hosts must remain. To remove a dead host that cannot be shut down
gracefully, use hosts/purge_host_from_cluster.sh instead.

Options:
  --host moxN   Host to remove. Prompted when omitted.
  -h, --help
EOF
}

parse_args() {
  while (($#)); do
    case "$1" in
      --host)
        (($# >= 2)) || die "--host requires a value"
        TARGET_HOST="$2"
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
  hm_release_control_plane_lock
  if ((code != 0)); then
    printf '\nFailed while %s.\n' "$CURRENT_PHASE" >&2
    if [[ "$CONTROL_CHANGED" == true ]]; then
      printf 'The registry already records %s as the cluster control node.\n' \
        "$NEW_CONTROL_NODE" >&2
      printf 'Set PROXMOX_CONTROL_NODE=%s in %s before rerunning.\n' \
        "$NEW_CONTROL_NODE" "$PROXMOX_CLUSTER_CONFIG" >&2
    fi
    if [[ "$CLUSTER_CHANGED" == true ]]; then
      printf 'Cluster membership or QDevice state was already changing. Correct the\n' >&2
      printf 'fault and rerun this script for %s; completed steps are skipped.\n' \
        "$TARGET_HOST" >&2
    fi
  fi
  exit "$code"
}

load_member_states() {
  local states node state
  states="$(hm_member_states)" || die "Could not list cluster members"
  MEMBERS=()
  ONLINE_MEMBERS=()
  while read -r node state; do
    [[ -n "$node" ]] || continue
    MEMBERS+=("$node")
    [[ "$state" != online ]] || ONLINE_MEMBERS+=("$node")
  done <<<"$states"
}

load_live_state() {
  LIVE_VMS_JSON="$(hm_pvesh_get /cluster/resources --type vm)" ||
    die "Could not list cluster guests"
  LIVE_RULES_JSON="$(
    hm_exec "$HM_COORDINATOR" ha-manager rules config --output-format json
  )" || die "Could not list HA rules"
  LIVE_REPLICATION_JSON="$(hm_pvesh_get /cluster/replication)" ||
    die "Could not list replication jobs"
}

# Print the reasons NODE cannot be removed, one per line; print nothing when it
# is eligible. Uses the state from load_live_state.
removal_blockers() {
  local node="$1" references
  references="$(hm_registry host-references "$node")" ||
    die "Could not read registry references to $node"
  python3 - "$node" "$references" "$LIVE_VMS_JSON" "$LIVE_RULES_JSON" \
    "$LIVE_REPLICATION_JSON" <<'PY'
import json
import sys

node = sys.argv[1]
references = json.loads(sys.argv[2])
vms, rules, replication = (json.loads(value) for value in sys.argv[3:6])
index = int(node[3:])
haproxy_vmid = 9110 + index


def values(raw):
    return raw if isinstance(raw, list) else [
        part for part in str(raw or "").split(",") if part
    ]


for name in references["references"]:
    print(f"registry record {name} references {node}")
for row in vms:
    if row.get("node") != node:
        continue
    vmid = int(row.get("vmid", 0))
    if (
        row.get("type") == "lxc"
        and vmid == haproxy_vmid
        and row.get("name") in (None, f"haproxy{index}")
    ):
        continue
    print(f"guest {vmid} ({row.get('name', '?')}, {row.get('type', '?')}) is on {node}")
for rule in rules:
    nodes = {str(part).partition(":")[0] for part in values(rule.get("nodes"))}
    if node in nodes:
        print(f"HA rule {rule.get('rule', rule.get('id', '?'))} includes {node}")
for job in replication:
    if job.get("target") == node:
        print(f"replication job {job.get('id', '?')} targets {node}")
PY
}

show_members() {
  local node blockers status
  load_live_state
  printf '\nCluster members:\n'
  for node in "${MEMBERS[@]}"; do
    status=online
    control_list_contains "$node" "${ONLINE_MEMBERS[@]}" || status=OFFLINE
    blockers="$(removal_blockers "$node")"
    if [[ -n "$blockers" ]]; then
      printf '  %-6s %-8s in use (%s)\n' "$node" "$status" \
        "$(head -n 1 <<<"$blockers")"
    else
      printf '  %-6s %-8s removable%s\n' "$node" "$status" \
        "$([[ "$node" != "$CONTROL_NODE" ]] || printf ' (control node)')"
    fi
  done
}

choose_target() {
  CURRENT_PHASE="choosing the host to remove"
  show_members
  if [[ -z "$TARGET_HOST" ]]; then
    IFS= read -r -p "Host to remove: " TARGET_HOST ||
      die "Input ended before a host was chosen"
  fi
  hm_valid_node "$TARGET_HOST" ||
    die "Host must be mox1 through mox${MAX_MOX_HOSTS}: ${TARGET_HOST}"
  control_list_contains "$TARGET_HOST" "${MEMBERS[@]}" ||
    die "$TARGET_HOST is not a cluster member. A host that already left the cluster is cleaned up with hosts/purge_host_from_cluster.sh"
}

validate_cluster_shape() {
  CURRENT_PHASE="validating cluster membership and quorum"
  local status node
  status="$(hm_cluster_status)" || die "Could not read cluster status"
  grep -Eq "^Name:[[:space:]]+${PROXMOX_CLUSTER_NAME}[[:space:]]*$" <<<"$status" ||
    die "$HM_COORDINATOR is not a member of cluster $PROXMOX_CLUSTER_NAME"
  hm_status_is_quorate "$status" ||
    die "The cluster is not quorate; refusing a membership change"
  ((${#MEMBERS[@]} >= 3)) ||
    die "At least two hosts must remain; the cluster has ${#MEMBERS[@]} members"
  for node in "${MEMBERS[@]}"; do
    [[ "$node" != "$TARGET_HOST" ]] || continue
    control_list_contains "$node" "${ONLINE_MEMBERS[@]}" ||
      die "Every remaining member must be online; $node is offline"
  done
  TARGET_ONLINE=false
  if control_list_contains "$TARGET_HOST" "${ONLINE_MEMBERS[@]}"; then
    TARGET_ONLINE=true
  fi
}

validate_eligibility() {
  CURRENT_PHASE="checking that nothing depends on $TARGET_HOST"
  local blockers
  load_live_state
  blockers="$(removal_blockers "$TARGET_HOST")"
  if [[ -n "$blockers" ]]; then
    printf '\n%s cannot be removed yet:\n' "$TARGET_HOST" >&2
    sed 's/^/  - /' <<<"$blockers" >&2
    printf '\nMove production placement off %s with guests/prod/change_prod_vm_owner.sh\n' \
      "$TARGET_HOST" >&2
    printf 'and guests/prod/change_prod_vm_placement.sh, destroy staging VMs that use it, and\n' >&2
    printf 'wait for deferred cleanup to finish.\n' >&2
    exit 1
  fi
  info "No registry record, guest, HA rule, or replication job depends on $TARGET_HOST"
}

confirm_offline_resume() {
  [[ "$TARGET_ONLINE" == false ]] || return 0
  ! mox_is_reachable "$TARGET_HOST" ||
    die "$TARGET_HOST is reachable over SSH but Proxmox reports it offline; fix its cluster connectivity and rerun"
  printf '\n%s is already offline and unreachable.\n' "$TARGET_HOST"
  printf 'Continue only if an earlier run of this script powered it off. For a host that\n'
  printf 'failed or was never shut down by this script, use hosts/purge_host_from_cluster.sh.\n'
  hm_prompt_yes "Did an earlier run of this script power off ${TARGET_HOST}?" ||
    die "No change was made"
}

confirm_cloudflare() {
  printf '\nCloudflare Load Balancing sends traffic to each host by public IP. Before\n'
  printf 'removing %s, remove its pool from every load balancer (and its monitor),\n' \
    "$TARGET_HOST"
  printf 'after adding pools for any hosts that replace it.\n'
  hm_prompt_yes "Is ${TARGET_HOST} out of every Cloudflare load balancer?" ||
    die "Update Cloudflare first; no change was made"
}

choose_control_replacement() {
  [[ "$TARGET_HOST" == "$CONTROL_NODE" ]] || return 0
  local -a candidates=() node
  for node in "${ONLINE_MEMBERS[@]}"; do
    [[ "$node" == "$TARGET_HOST" ]] || candidates+=("$node")
  done
  hm_choose_new_control_node "$TARGET_HOST" NEW_CONTROL_NODE "${candidates[@]}"
}

show_plan() {
  local remaining=$((${#MEMBERS[@]} - 1))
  log "Removal plan"
  info "Host: $TARGET_HOST ($([[ "$TARGET_ONLINE" == true ]] && printf 'online' || printf 'already powered off'))"
  info "Cluster: ${MEMBERS[*]} -> $remaining members"
  if ((remaining % 2 == 0)); then
    info "QDevice: present afterward ($remaining nodes + 1 QDevice vote)"
  else
    info "QDevice: absent afterward ($remaining votes)"
  fi
  if [[ -n "$NEW_CONTROL_NODE" ]]; then
    info "Control node: $TARGET_HOST -> $NEW_CONTROL_NODE"
  else
    info "Control node: $CONTROL_NODE (unchanged)"
  fi
  info "The host is powered off and must be wiped or reinstalled before it joins any cluster again."
}

switch_control_node() {
  [[ -n "$NEW_CONTROL_NODE" ]] || return 0
  CURRENT_PHASE="recording $NEW_CONTROL_NODE as the cluster control node"
  hm_registry host-sync --live >/dev/null ||
    die "Could not record current cluster membership in the registry"
  hm_registry control-set "$NEW_CONTROL_NODE" \
    --expected-node "${CONTROL_RECORDED_NODE:-none}" >/dev/null ||
    die "Could not record $NEW_CONTROL_NODE as the cluster control node"
  CONTROL_CHANGED=true
  CONTROL_NODE="$NEW_CONTROL_NODE"
  HM_COORDINATOR="$NEW_CONTROL_NODE"
  info "The registry records $NEW_CONTROL_NODE as the cluster control node"
}

remove_qdevice_before_change() {
  CURRENT_PHASE="removing the QDevice vote"
  hm_qdevice_configured || {
    info "No QDevice is configured"
    return 0
  }
  CLUSTER_CHANGED=true
  hm_remove_qdevice
  local status
  status="$(hm_cluster_status)" || die "Could not read cluster status"
  hm_status_is_quorate "$status" ||
    die "The cluster lost quorum after QDevice removal"
}

power_off_target() {
  [[ "$TARGET_ONLINE" == true ]] || return 0
  CURRENT_PHASE="powering off $TARGET_HOST"
  log "Powering off $TARGET_HOST"
  TARGET_KEY="$(mox_ssh "$TARGET_HOST" cat /root/.ssh/id_rsa.pub </dev/null 2>/dev/null || true)"
  CLUSTER_CHANGED=true
  # Disabling corosync keeps the host from rejoining if it is powered on
  # before it is wiped.
  mox_ssh "$TARGET_HOST" bash -c '
set -Eeuo pipefail
systemctl disable corosync.service >/dev/null 2>&1 || true
if systemctl list-unit-files corosync-qdevice.service >/dev/null 2>&1; then
  systemctl disable corosync-qdevice.service >/dev/null 2>&1 || true
fi
systemd-run --quiet --on-active=3 --unit=app-ha-remove-poweroff systemctl poweroff
' </dev/null || die "Could not schedule the power-off of $TARGET_HOST"
  info "Waiting up to ${SHUTDOWN_TIMEOUT_SECONDS} seconds for $TARGET_HOST to go offline"
  hm_wait_until_offline "$TARGET_HOST" "$SHUTDOWN_TIMEOUT_SECONDS" ||
    die "$TARGET_HOST did not go offline within ${SHUTDOWN_TIMEOUT_SECONDS} seconds"
  info "$TARGET_HOST is offline"
}

delete_target() {
  CURRENT_PHASE="deleting $TARGET_HOST from the cluster"
  CLUSTER_CHANGED=true
  hm_delete_cluster_node "$TARGET_HOST"
  hm_strip_node_ssh_trust "$TARGET_HOST" "$TARGET_KEY"
}

reconcile_after_removal() {
  CURRENT_PHASE="reconciling the QDevice for the remaining members"
  load_member_states
  ((${#MEMBERS[@]} == ${#ONLINE_MEMBERS[@]})) ||
    die "Every remaining member must be online (members: ${MEMBERS[*]}; online: ${ONLINE_MEMBERS[*]})"
  hm_reconcile_qdevice "${#MEMBERS[@]}"
}

release_slot() {
  CURRENT_PHASE="freeing the $TARGET_HOST registry slot"
  hm_registry host-sync --live >/dev/null ||
    die "Could not record current cluster membership in the registry"
  hm_registry host-release "$TARGET_HOST" \
    --reason "removed with hosts/remove_host_from_cluster.sh" >/dev/null ||
    die "Could not free the $TARGET_HOST registry slot"
  info "The $TARGET_HOST slot is free for a future host"
}

sync_ingress() {
  CURRENT_PHASE="synchronizing HAProxy on the remaining members"
  mox_ssh "$HM_COORDINATOR" "$HM_REMOTE_HAPROXY_SYNC" --lock-timeout 240 \
    </dev/null >/dev/null ||
    warn "HAProxy route synchronization failed; run it later on $HM_COORDINATOR: $HM_REMOTE_HAPROXY_SYNC"
}

main() {
  parse_args "$@"
  load_proxmox_config --no-secrets || die "Could not load cluster configuration"
  require_vars PROXMOX_CLUSTER_NAME MAX_MOX_HOSTS CLUSTER_STATE_DIR \
    PROXMOX_QDEVICE_HOST PROXMOX_INTERNAL_DOMAIN PRIVATE_SUBNET_PREFIX \
    MOX_IP_START_OCTET || die "Configuration is incomplete"
  local command_name
  for command_name in awk flock jq openssl python3 ssh tailscale; do
    command -v "$command_name" >/dev/null 2>&1 ||
      die "Required workstation command is unavailable: $command_name"
  done
  trap cleanup EXIT

  hm_verify_qdevice_access

  CURRENT_PHASE="resolving the cluster control node"
  log "Cluster control node"
  resolve_control_node || exit 1
  [[ -n "$CONTROL_PROBE_NODE" ]] || die "No reachable cluster member was found"
  [[ "$CONTROL_REGISTRY_SUPPORTS_HOSTS" == true ]] ||
    die "The installed cluster registry predates host slots; run hosts/update_cluster_runtime.sh first"
  HM_COORDINATOR="$CONTROL_NODE"
  info "Control node: $CONTROL_NODE"
  hm_require_slot_registry

  load_member_states
  choose_target
  validate_cluster_shape
  validate_eligibility
  confirm_offline_resume
  confirm_cloudflare
  choose_control_replacement
  show_plan
  hm_confirm_phrase \
    "This powers off $TARGET_HOST and permanently removes it from cluster $PROXMOX_CLUSTER_NAME." \
    "REMOVE ${TARGET_HOST}"

  switch_control_node
  CURRENT_PHASE="acquiring the cluster control-plane lock"
  hm_acquire_control_plane_lock "$HM_COORDINATOR"
  load_member_states
  validate_cluster_shape
  validate_eligibility

  remove_qdevice_before_change
  power_off_target
  delete_target
  reconcile_after_removal
  release_slot
  sync_ingress
  hm_release_control_plane_lock
  hm_archive_host_artifacts "$TARGET_HOST"

  log "Host removal complete"
  info "$TARGET_HOST is powered off and no longer a member of $PROXMOX_CLUSTER_NAME"
  info "Members: ${MEMBERS[*]}"
  if [[ "$CONTROL_CHANGED" == true ]]; then
    control_offer_cluster_conf_update "$NEW_CONTROL_NODE"
  fi
  hm_print_reinstall_follow_ups "$TARGET_HOST"
  printf '  - Wipe or reinstall %s before it is connected to any network again.\n' \
    "$TARGET_HOST"
}

if [[ "${REMOVE_HOST_SOURCE_ONLY:-0}" != 1 ]]; then
  main "$@"
fi
