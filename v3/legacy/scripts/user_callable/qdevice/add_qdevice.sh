#!/usr/bin/env bash

# Copyright (c) 2026 BEENTHERE VENTURES, INC.
# SPDX-License-Identifier: GPL-3.0-only

# Add an external QDevice to a cluster that needs one: an even number of
# members and no QDevice registered. Runs under bmacd on the duty host, holding
# the cluster-membership lock.

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
RUN_DIR=""

usage() {
  cat <<'EOF'
Usage: add_qdevice.sh

Runs under bmacd on a cluster host. Adds an external QDevice vote to the
Proxmox cluster when the cluster needs one: it has an even number of members
and no QDevice is registered. Every member must be online and the cluster
must be quorate.

The QDevice is the host named by the QDevice setting (PROXMOX_QDEVICE_HOST).
The script asks you to confirm its SSH host key by the fingerprint shown on
its own console, checks root SSH to it from this host, installs and verifies
corosync-qnetd there, pins its SSH host key on every member, and runs
'pvecm qdevice setup' on this host.

To replace a registered QDevice that has failed, run Remove QDevice first.
The QDevice state diagnostic reports which case applies.

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
  if ((code != 0)); then
    printf '\nFailed while %s.\n' "$CURRENT_PHASE" >&2
    printf 'Run the QDevice state diagnostic to see the current QDevice state.\n' >&2
    printf 'Correct the fault and rerun; it starts again from the live state.\n' >&2
  fi
  [[ -z "$RUN_DIR" ]] || rm -rf -- "$RUN_DIR"
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
      warn "A QDevice is registered anyway. Remove it with Remove QDevice while every member is online."
    fi
    exit 0
  fi
  if qd_is_registered; then
    if problem="$(qd_layout_problem "$NODE_COUNT")"; then
      log "The cluster already has a functional QDevice"
      info "Every member reports it alive and voting. No change was made."
      exit 0
    fi
    die "A QDevice is registered but not healthy (${problem}). Run the QDevice state diagnostic; to replace it, run Remove QDevice first, then Add QDevice."
  fi
  log "The ${NODE_COUNT}-member cluster needs a QDevice"
  info "With an even number of members and no QDevice, losing one member loses quorum."
}

print_setup_instructions() {
  local key
  key="$(qd_access_key)" || die "Could not prepare the cluster's QDevice access key"
  cat <<EOF

Prepare the QDevice machine (${PROXMOX_QDEVICE_HOST}) before continuing:

  1. Install Ubuntu and join it to Tailscale as ${PROXMOX_QDEVICE_HOST}.
  2. If it replaces a failed QDevice, the old machine must already be removed
     from the Tailscale admin console (Remove QDevice asks for this).
     Otherwise the Tailscale hostname is ambiguous and this stops.
  3. Tag the machine tag:bmac-proxmox-qdevice. The Tailscale policy must let
     tag:bmac-proxmox-host reach it on tcp:5403 and tcp:22.
  4. Add the cluster's QDevice access key to /root/.ssh/authorized_keys on it:
     ${key}
  5. Keep its console open: you confirm its SSH host key by fingerprint next.
EOF
  if bmac_ui_is_json; then
    bmac_ui_manual_action --id qdevice_ready --title "Prepare the QDevice machine" \
      --instruction "Install Ubuntu on ${PROXMOX_QDEVICE_HOST} and join it to Tailscale under that name." \
      --instruction "If it replaces a failed QDevice, the old machine must already be removed from the Tailscale admin console." \
      --instruction "Tag the machine tag:bmac-proxmox-qdevice. The Tailscale policy must let tag:bmac-proxmox-host reach it on tcp:5403 and tcp:22." \
      --instruction "Add the cluster's QDevice access key to /root/.ssh/authorized_keys on it: ${key}" \
      --instruction "Keep its console open: you confirm its SSH host key by fingerprint next." \
      --ack-label "The QDevice machine is ready"
    return
  fi
  IFS= read -r -p $'\nPress ENTER when the QDevice machine is ready, or Ctrl-C to stop: ' _ ||
    die "Input ended; no change was made"
}

require_qdevice_host() {
  [[ "$PROXMOX_QDEVICE_HOST" =~ ^[A-Za-z0-9][A-Za-z0-9._-]*$ ]] ||
    die "The QDevice setting is not a plain hostname: $PROXMOX_QDEVICE_HOST"
  [[ ! "$PROXMOX_QDEVICE_HOST" =~ ^mox[0-9]+$ ]] ||
    die "The QDevice setting names a Proxmox host ($PROXMOX_QDEVICE_HOST); the QDevice must be a separate machine. Change it in Settings."
  info "QDevice: ${PROXMOX_QDEVICE_HOST} (change it in Settings)"
}

# Refuse a Proxmox host or a non-APT machine, and report other clusters that
# this qnetd already serves.
inspect_qdevice_host() {
  local report
  CURRENT_PHASE="inspecting ${PROXMOX_QDEVICE_HOST}"
  report="$(
    qd_ssh bash -s <<'REMOTE'
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
    NOAPT*) die "${PROXMOX_QDEVICE_HOST} has no apt-get; the QDevice must be an Ubuntu machine" ;;
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
  load_proxmox_config || die "Could not load cluster configuration"
  local name
  for name in PROXMOX_CLUSTER_NAME PROXMOX_QDEVICE_HOST MAX_MOX_HOSTS; do
    require_var "$name" || die "Configuration is incomplete"
  done
  local command_name
  for command_name in jq python3 ssh ssh-keygen ssh-keyscan tailscale flock openssl base64; do
    command -v "$command_name" >/dev/null 2>&1 ||
      die "Required command is unavailable: $command_name"
  done
  RUN_DIR="$(mktemp -d "${BMAC_WORK_DIR:-${TMPDIR:-/tmp}}/add-qdevice.XXXXXX")"
  chmod 0700 "$RUN_DIR"
  trap cleanup EXIT

  HM_COORDINATOR="$(this_mox)" || die "Could not tell which cluster host this is"
  CURRENT_PHASE="checking whether the cluster needs a QDevice"
  load_cluster_shape
  require_qdevice_needed

  require_qdevice_host
  print_setup_instructions
  CURRENT_PHASE="confirming the QDevice host key"
  qd_trust_host_key "$RUN_DIR"
  CURRENT_PHASE="verifying QDevice access"
  hm_verify_qdevice_access ||
    die "Authorize the cluster's QDevice access key on ${PROXMOX_QDEVICE_HOST} and rerun"
  inspect_qdevice_host

  log "QDevice plan"
  info "Cluster: ${PROXMOX_CLUSTER_NAME}, ${NODE_COUNT} members, run from $HM_COORDINATOR"
  info "QDevice: ${PROXMOX_QDEVICE_HOST} (Tailscale ${QD_IPV4})"
  info "Installs and verifies corosync-qnetd there, pins its SSH host key on every"
  info "member, runs 'pvecm qdevice setup ${QD_IPV4} --force' on"
  info "$HM_COORDINATOR, and checks that every member reports $((NODE_COUNT + 1)) votes."
  hm_confirm_phrase "This adds ${PROXMOX_QDEVICE_HOST} as the cluster's QDevice." GO

  CURRENT_PHASE="revalidating the cluster"
  load_cluster_shape
  ((NODE_COUNT % 2 == 0)) || die "Cluster membership changed to ${NODE_COUNT} members; no change was made"
  ! qd_is_registered || die "A QDevice was registered meanwhile; no change was made"

  CURRENT_PHASE="adding the QDevice"
  qd_add "$NODE_COUNT"

  log "QDevice added"
  info "${PROXMOX_QDEVICE_HOST} (${QD_IPV4}) is alive and voting on all ${NODE_COUNT} members."
  info "Check it any time with the QDevice state diagnostic."
  bmac_ui_result qdevice_host "$PROXMOX_QDEVICE_HOST" qdevice_ip "$QD_IPV4" members:int "$NODE_COUNT"
  bmac_ui_next_step "Check the QDevice any time." \
    --workflow diagnostics.qdevice_state
}

if [[ "${ADD_QDEVICE_SOURCE_ONLY:-0}" != 1 ]]; then
  bmac_ui_bootstrap "$@"
  main "$@"
fi
