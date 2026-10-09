#!/usr/bin/env bash

# Copyright (c) 2026 BEENTHERE VENTURES, INC.
# SPDX-License-Identifier: GPL-3.0-only

# Add an external QDevice to a cluster that needs one: an even number of
# members and no QDevice registered. Use it after scripts/user_callable/qdevice/remove_qdevice.sh
# unregistered a failed QDevice, once a replacement has been prepared with
# docs/QDEVICE_MANUAL_SETUP.md. It configures the QDevice the same way
# scripts/user_callable/hosts/add_proxmox_host.sh does.

set -Eeuo pipefail
set +x
umask 077

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
REPO_ROOT="$(cd -- "${SCRIPT_DIR}/../../.." && pwd -P)"
CONFIG_LIB="${REPO_ROOT}/scripts/lib/config.sh"
CONTROL_LIB="${REPO_ROOT}/scripts/lib/cluster_control.sh"
MEMBERSHIP_LIB="${REPO_ROOT}/scripts/lib/host_membership.sh"

for library in "$CONFIG_LIB" "$CONTROL_LIB" "$MEMBERSHIP_LIB"; do
  [[ -f "$library" && ! -L "$library" ]] || {
    printf 'ERROR: Missing library: %s\n' "$library" >&2
    exit 1
  }
done
# shellcheck source=../../lib/config.sh
source "$CONFIG_LIB"
# shellcheck source=../../lib/cluster_control.sh
source "$CONTROL_LIB"
# shellcheck source=../../lib/host_membership.sh
source "$MEMBERSHIP_LIB"

CURRENT_PHASE="startup"
NODE_COUNT=0

usage() {
  cat <<'EOF'
Usage: add_qdevice.sh

Run from an administrator workstation. Adds an external QDevice vote to the
Proxmox cluster when the cluster needs one: it has an even number of members
and no QDevice is registered. Every member must be online and the cluster
must be quorate.

Prepare the QDevice machine first by following docs/QDEVICE_MANUAL_SETUP.md.
The script then asks for its SSH hostname (default: PROXMOX_QDEVICE_HOST in
config/cluster.conf), checks 'ssh <hostname>', installs and verifies
corosync-qnetd there, pins its SSH host key on every member, and runs
'pvecm qdevice setup' through the cluster control node, as
scripts/user_callable/hosts/add_proxmox_host.sh does.

To replace a registered QDevice that has failed, run scripts/user_callable/qdevice/remove_qdevice.sh
first. scripts/user_callable/diagnostics/show_qdevice_state.sh reports which case applies.

Options:
  -h, --help
EOF
}

parse_args() {
  while (($#)); do
    case "$1" in
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
    printf 'Run scripts/user_callable/diagnostics/show_qdevice_state.sh to see the current QDevice state.\n' >&2
    printf 'Correct the fault and rerun this script; it starts again from the live state.\n' >&2
  fi
  exit "$code"
}

# Prints "COUNT" for an all-online quorate cluster, or dies.
load_cluster_shape() {
  local states node state count=0 status
  states="$(hm_member_states)" || die "Could not list cluster members through $HM_COORDINATOR"
  while read -r node state; do
    [[ -n "$node" ]] || continue
    count=$((count + 1))
    [[ "$state" == online ]] ||
      die "$node is offline. Every member must be online to change the QDevice; bring it back or remove it from the cluster first."
  done <<<"$states"
  status="$(hm_cluster_status)" || die "Could not read cluster status"
  pvecm_status_is_quorate "$status" || die "The cluster is not quorate"
  NODE_COUNT="$count"
}

# Stop unless the cluster needs a QDevice and has none registered.
require_qdevice_needed() {
  local problem
  if ((NODE_COUNT % 2 == 1)); then
    log "No QDevice is needed"
    info "The ${NODE_COUNT}-member cluster has an odd number of votes, so it does not use a QDevice."
    if qd_is_registered; then
      warn "A QDevice is registered anyway. scripts/user_callable/hosts/add_proxmox_host.sh removes it on its next run, or run 'pvecm qdevice remove' on a member while every member is online."
    fi
    exit 0
  fi
  if qd_is_registered; then
    if problem="$(qd_layout_problem "$NODE_COUNT")"; then
      log "The cluster already has a functional QDevice"
      info "Every member reports it alive and voting. No change was made."
      exit 0
    fi
    die "A QDevice is registered but not healthy (${problem}). Run scripts/user_callable/diagnostics/show_qdevice_state.sh; to replace it, run scripts/user_callable/qdevice/remove_qdevice.sh first, then this script."
  fi
  log "The ${NODE_COUNT}-member cluster needs a QDevice"
  info "With an even number of members and no QDevice, losing one member loses quorum."
}

print_setup_instructions() {
  cat <<EOF

Prepare the QDevice machine before continuing:

  1. Follow docs/QDEVICE_MANUAL_SETUP.md, sections 1 through 15. Section 16
     (corosync-qnetd) is done by this script.
  2. If it replaces a failed QDevice, the old machine must already be removed
     from the Tailscale admin console (scripts/user_callable/qdevice/remove_qdevice.sh asks for this).
     Otherwise the Tailscale hostname is ambiguous and this script stops.
  3. Tag the machine tag:proxmox-qdevice. The Tailscale policy must let
     tag:proxmox-host reach it on tcp:5403 and, while this script runs, on
     tcp:22 (the "Initial QDevice Setup" rule in README.md).
  4. On this workstation, remove any host key recorded for the old machine
     (ssh-keygen -R <hostname>), then run 'ssh <hostname>' once and verify and
     accept the new host key.
EOF
  if bmac_ui_is_json; then
    bmac_ui_manual_action --id qdevice_ready --title "Prepare the QDevice machine" \
      --instruction "Follow docs/QDEVICE_MANUAL_SETUP.md, sections 1 through 15. Section 16 (corosync-qnetd) is done by this workflow." \
      --instruction "If it replaces a failed QDevice, the old machine must already be removed from the Tailscale admin console." \
      --instruction "Tag the machine tag:proxmox-qdevice. The Tailscale policy must let tag:proxmox-host reach it on tcp:5403 and, while this runs, on tcp:22." \
      --instruction "On this workstation, remove any host key recorded for the old machine (ssh-keygen -R <hostname>), then run 'ssh <hostname>' once and verify and accept the new host key." \
      --ack-label "The QDevice machine is ready"
    return
  fi
  IFS= read -r -p $'\nPress ENTER when the QDevice machine is ready, or Ctrl-C to stop: ' _ ||
    die "Input ended; no change was made"
}

choose_qdevice_host() {
  local host
  while true; do
    prompt_with_default host "QDevice SSH hostname" "$PROXMOX_QDEVICE_HOST"
    if [[ ! "$host" =~ ^[A-Za-z0-9][A-Za-z0-9._-]*$ ]]; then
      warn "Enter a plain hostname such as qdevice"
    elif [[ "$host" =~ ^mox([0-9]+)$ ]]; then
      warn "$host is a Proxmox host name; the QDevice must be a separate machine"
    else
      break
    fi
  done
  [[ "$host" != "$PROXMOX_QDEVICE_HOST" ]] || return 0
  printf '\nThe cluster scripts reach the QDevice as PROXMOX_QDEVICE_HOST, which %s\n' \
    "$PROXMOX_CLUSTER_CONFIG"
  printf 'sets to %s. It must name %s instead.\n' "$PROXMOX_QDEVICE_HOST" "$host"
  hm_prompt_yes "Update PROXMOX_QDEVICE_HOST=${host} in ${PROXMOX_CLUSTER_CONFIG} now?" ||
    die "Set PROXMOX_QDEVICE_HOST=${host} in ${PROXMOX_CLUSTER_CONFIG} and rerun; no change was made"
  cluster_conf_set_value "$PROXMOX_CLUSTER_CONFIG" PROXMOX_QDEVICE_HOST "$host" ||
    die "Could not update $PROXMOX_CLUSTER_CONFIG"
  PROXMOX_QDEVICE_HOST="$host"
  info "Updated $PROXMOX_CLUSTER_CONFIG: PROXMOX_QDEVICE_HOST=$host"
  info "Every other workstation must make the same change."
}

# Refuse a Proxmox host or a non-APT machine, and report other clusters that
# this qnetd already serves.
inspect_qdevice_host() {
  local report
  CURRENT_PHASE="inspecting ${PROXMOX_QDEVICE_HOST}"
  report="$(
    ssh -o BatchMode=yes -o StrictHostKeyChecking=yes -o CheckHostIP=yes \
      -o ConnectTimeout=10 "root@${PROXMOX_QDEVICE_HOST}" bash -s <<'REMOTE'
set -Eeuo pipefail
if command -v pveversion >/dev/null 2>&1 ||
  dpkg-query -W -f='${Status}' proxmox-ve 2>/dev/null | grep -q 'install ok installed'; then
  printf 'PROXMOX\n'
  exit 0
fi
command -v apt-get >/dev/null 2>&1 || { printf 'NOAPT\n'; exit 0; }
printf 'OS %s\n' "$(. /etc/os-release && printf '%s' "${PRETTY_NAME:-unknown}")"
if command -v corosync-qnetd-tool >/dev/null 2>&1 &&
  systemctl is-active --quiet corosync-qnetd; then
  corosync-qnetd-tool -l 2>/dev/null | sed -n 's/^Cluster "\(.*\)":$/CLUSTER \1/p'
fi
REMOTE
  )" || die "Could not inspect ${PROXMOX_QDEVICE_HOST}"
  case "$report" in
    PROXMOX*) die "${PROXMOX_QDEVICE_HOST} is a Proxmox VE host; the QDevice must be a separate Ubuntu machine" ;;
    NOAPT*) die "${PROXMOX_QDEVICE_HOST} has no apt-get; follow docs/QDEVICE_MANUAL_SETUP.md" ;;
  esac
  info "${PROXMOX_QDEVICE_HOST}: $(awk '$1 == "OS" { sub(/^OS /, ""); print }' <<<"$report")"
  local clusters
  clusters="$(awk '$1 == "CLUSTER" { sub(/^CLUSTER /, ""); print }' <<<"$report" | paste -sd, -)"
  if [[ -n "$clusters" ]]; then
    warn "corosync-qnetd on ${PROXMOX_QDEVICE_HOST} already serves: ${clusters}"
    hm_prompt_yes "Continue adding ${PROXMOX_CLUSTER_NAME} to it?" ||
      die "No change was made"
  fi
}

main() {
  parse_args "$@"
  load_proxmox_config --no-secrets || die "Could not load cluster configuration"
  local name
  for name in PROXMOX_CLUSTER_NAME PROXMOX_QDEVICE_HOST MAX_MOX_HOSTS; do
    require_var "$name" || die "Configuration is incomplete"
  done
  local command_name
  for command_name in jq python3 ssh ssh-keygen tailscale flock openssl base64; do
    command -v "$command_name" >/dev/null 2>&1 ||
      die "Required workstation command is unavailable: $command_name"
  done
  trap cleanup EXIT

  CURRENT_PHASE="resolving the cluster control node"
  resolve_control_node || die "Could not resolve the cluster control node"
  HM_COORDINATOR="$CONTROL_NODE"
  CURRENT_PHASE="checking whether the cluster needs a QDevice"
  load_cluster_shape
  require_qdevice_needed

  print_setup_instructions
  choose_qdevice_host
  CURRENT_PHASE="verifying QDevice access"
  hm_verify_qdevice_access ||
    die "Set up root SSH from this workstation to ${PROXMOX_QDEVICE_HOST} (see docs/QDEVICE_MANUAL_SETUP.md) and rerun"
  inspect_qdevice_host

  log "QDevice plan"
  info "Cluster: ${PROXMOX_CLUSTER_NAME}, ${NODE_COUNT} members, control node $HM_COORDINATOR"
  info "QDevice: ${PROXMOX_QDEVICE_HOST} (Tailscale ${QD_IPV4})"
  info "Installs and verifies corosync-qnetd there, pins its SSH host key on every"
  info "member, runs 'pvecm qdevice setup ${QD_IPV4} --force' on"
  info "$HM_COORDINATOR, and checks that every member reports $((NODE_COUNT + 1)) votes."
  hm_confirm_phrase "This adds ${PROXMOX_QDEVICE_HOST} as the cluster's QDevice." GO

  CURRENT_PHASE="acquiring the cluster control-plane lock"
  hm_acquire_control_plane_lock "$HM_COORDINATOR"
  CURRENT_PHASE="revalidating the cluster"
  load_cluster_shape
  ((NODE_COUNT % 2 == 0)) || die "Cluster membership changed to ${NODE_COUNT} members; no change was made"
  ! qd_is_registered || die "A QDevice was registered meanwhile; no change was made"

  CURRENT_PHASE="adding the QDevice"
  qd_add "$NODE_COUNT"
  hm_release_control_plane_lock

  log "QDevice added"
  info "${PROXMOX_QDEVICE_HOST} (${QD_IPV4}) is alive and voting on all ${NODE_COUNT} members."
  info "Check it any time with scripts/user_callable/diagnostics/show_qdevice_state.sh."
  bmac_ui_result qdevice_host "$PROXMOX_QDEVICE_HOST" qdevice_ip "$QD_IPV4" members:int "$NODE_COUNT"
  bmac_ui_next_step "Check the QDevice any time." --command "scripts/user_callable/diagnostics/show_qdevice_state.sh" \
    --workflow show_qdevice_state
}

if [[ "${ADD_QDEVICE_SOURCE_ONLY:-0}" != 1 ]]; then
  bmac_ui_bootstrap "$@"
  main "$@"
fi
