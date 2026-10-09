#!/usr/bin/env bash

# Copyright (c) 2026 BEENTHERE VENTURES, INC.
# SPDX-License-Identifier: GPL-3.0-only

# Workstation driver shared by the fast
# scripts/user_callable/diagnostics/list_*.sh scripts. It
# finds one reachable cluster member, runs scripts/utilities/quick_state.py's read-only
# collector there over a single SSH connection, and renders the result
# locally. Source after scripts/lib/config.sh, scripts/lib/cluster_control.sh, and
# scripts/lib/ui_protocol.sh.

QS_LIB_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
QS_COLLECTOR="$(cd -- "${QS_LIB_DIR}/../utilities" && pwd -P)/quick_state.py"
QS_RUN_DIR=""
QS_PROBE=""

qs_die() {
  printf 'ERROR: %s\n' "$*" >&2
  bmac_ui_error "${QS_ERROR_CODE:-quick_state_failed}" "$*"
  exit "${QS_EXIT:-1}"
}

qs_init() {
  command -v python3 >/dev/null 2>&1 || QS_EXIT=2 qs_die "python3 is required on this workstation"
  load_proxmox_config --no-secrets >/dev/null ||
    QS_EXIT=2 QS_ERROR_CODE=config_invalid qs_die "cluster configuration is invalid"
  QS_RUN_DIR="$(mktemp -d "${TMPDIR:-/tmp}/bmac-quick-state.XXXXXX")"
  chmod 0700 "$QS_RUN_DIR"
  trap 'rm -rf -- "$QS_RUN_DIR"' EXIT
}

# Find the lowest-numbered reachable member of the cluster.
qs_find_probe() {
  local status=0
  bmac_ui_step "Find a reachable cluster member"
  control_find_cluster || status=$?
  case "$status" in
    0) ;;
    1)
      bmac_ui_next_step "Check this workstation's Tailscale connection and SSH access to the hosts." \
        --command "scripts/user_callable/diagnostics/show_cluster_health.sh" --workflow show_cluster_health
      QS_ERROR_CODE=no_reachable_member qs_die \
        "no cluster member (mox1 through mox${MAX_MOX_HOSTS}) is reachable over strict SSH from this workstation"
      ;;
    *)
      QS_ERROR_CODE=membership_malformed qs_die \
        "cluster membership reported through ${CONTROL_PROBE_NODE} is malformed"
      ;;
  esac
  QS_PROBE="$CONTROL_PROBE_NODE"
  printf 'Read through %s (members: %s)\n' "$QS_PROBE" "${CONTROL_MEMBER_NODES[*]}"
}

# qs_collect KIND [COLLECTOR ARGS...]: leaves the report in $QS_RUN_DIR/report.json
qs_collect() {
  local kind="$1"
  shift
  bmac_ui_step "Collect ${kind} state on ${QS_PROBE}"
  mox_ssh "$QS_PROBE" python3 - collect "$kind" \
    --registry "$CONTROL_REMOTE_REGISTRY" --state-dir "$CLUSTER_STATE_DIR" "$@" \
    <"$QS_COLLECTOR" >"${QS_RUN_DIR}/report.json" ||
    QS_ERROR_CODE=collect_failed qs_die "could not collect ${kind} state on ${QS_PROBE}"
}

# Record, in parallel, whether this workstation reaches each member directly.
qs_check_ssh() {
  local node
  local -a pids=()
  bmac_ui_step "Check SSH from this workstation to every member"
  for node in "${CONTROL_MEMBER_NODES[@]}"; do
    (
      if mox_is_reachable "$node"; then
        printf '%s\tok\n' "$node"
      else
        printf '%s\tfailed\n' "$node"
      fi
    ) >"${QS_RUN_DIR}/ssh-${node}" &
    pids+=("$!")
  done
  ((${#pids[@]} == 0)) || wait "${pids[@]}"
  cat "${QS_RUN_DIR}"/ssh-* 2>/dev/null >"${QS_RUN_DIR}/ssh.tsv" || : >"${QS_RUN_DIR}/ssh.tsv"
}

# qs_render KIND [RENDERER ARGS...]
qs_render() {
  local kind="$1"
  shift
  bmac_ui_step "Summarize ${kind} state"
  python3 "$QS_COLLECTOR" render "$kind" \
    --report "${QS_RUN_DIR}/report.json" "$@" ||
    qs_die "could not summarize the ${kind} state"
  bmac_ui_step_done
}
