#!/usr/bin/env bash

# Copyright (c) 2026 BEENTHERE VENTURES, INC.
# SPDX-License-Identifier: GPL-3.0-only

# Make one monitored node send a clearly labelled test pulse through its
# Healthchecks check, to test the whole alert chain: Healthchecks emails,
# the Gmail filters, and Pushover. Nothing in the cluster changes.

set -Eeuo pipefail
set +x
umask 077

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
REPO_ROOT="$(cd -- "${SCRIPT_DIR}/../../.." && pwd -P)"
# shellcheck source=../../lib/ui_protocol.sh
source "${REPO_ROOT}/scripts/lib/ui_protocol.sh"
# shellcheck source=../../lib/config.sh
source "${REPO_ROOT}/scripts/lib/config.sh"
# shellcheck source=../../lib/monitoring.sh
source "${REPO_ROOT}/scripts/lib/monitoring.sh"
bmac_ui_bootstrap "$@"

NODE=""
PRIORITY=""
RUN_DIR=""

die() {
  printf '\nERROR: %s\n' "$*" >&2
  bmac_ui_error failed "$*"
  exit 1
}

usage() {
  cat <<'EOF'
Usage: scripts/user_callable/diagnostics/send_test_alert.sh [--node NODE]
         [--priority p1|p2] [--json]

Make one monitored node (moxN, qdevice, or prodN) send a test pulse through
its Healthchecks check: a failure ping whose body holds
"NEW [pN] test:alert", then the clearing success ping. Expect two emails:

  DOWN | <cluster> bmaccheck <node>   (bmacevent bmacpN) - your forwarding
                                       filter (F2, or F2a/F2b) should label it
                                       and forward it to Pushover
  UP | <cluster> bmaccheck <node>     (bmacclear) - filter F1 should delete it

Without --node or --priority, asks for them (default priority p2).
EOF
}

parse_args() {
  while (($#)); do
    case "$1" in
      --node)
        (($# >= 2)) || die "--node needs a node name"
        NODE="$2"
        shift
        ;;
      --priority)
        (($# >= 2)) || die "--priority needs p1 or p2"
        PRIORITY="$2"
        shift
        ;;
      -h | --help)
        usage
        exit 0
        ;;
      *)
        usage >&2
        exit 2
        ;;
    esac
    shift
  done
}

choose() {
  local -a options=() names=()
  local node role target
  while IFS=$'\t' read -r node role target; do
    names+=("$node")
    options+=("$node" "$node ($role)")
  done <"${RUN_DIR}/nodes.tsv"
  if [[ -z "$NODE" ]]; then
    if bmac_ui_is_json; then
      bmac_ui_choose NODE "Node that sends the test alert" "${names[0]}" "${options[@]}"
    else
      printf 'Monitored nodes: %s\n' "${names[*]}"
      read -r -p "Node to send the test alert from [${names[0]}]: " NODE
      NODE="${NODE:-${names[0]}}"
    fi
  fi
  if [[ -z "$PRIORITY" ]]; then
    if bmac_ui_is_json; then
      bmac_ui_choose PRIORITY "Priority" p2 p1 "p1 (high: bmacp1)" p2 "p2 (normal: bmacp2)"
    else
      read -r -p "Priority, p1 or p2 [p2]: " PRIORITY
      PRIORITY="${PRIORITY:-p2}"
    fi
  fi
  [[ "$PRIORITY" =~ ^p[12]$ ]] || die "Priority must be p1 or p2"
}

main() {
  parse_args "$@"
  load_proxmox_config --no-secrets >/dev/null || die "cluster configuration is invalid"
  monitoring_enabled || die "Monitoring is disabled (MONITORING_ENABLED=false in config/cluster.conf)"
  RUN_DIR="$(mktemp -d "${TMPDIR:-/tmp}/send-test-alert.XXXXXX")"
  chmod 0700 "$RUN_DIR"
  trap 'rm -rf -- "$RUN_DIR"' EXIT

  local coordinator node role target found=""
  bmac_ui_step "Read the cluster"
  coordinator="$(first_reachable_mox)" || die "No reachable Proxmox host was found"
  monitoring_inventory "$coordinator" "${RUN_DIR}/inventory.json" || die "Could not read the cluster"
  mon_nodes "${RUN_DIR}/inventory.json" >"${RUN_DIR}/nodes.tsv" || die "The cluster inventory is invalid"
  choose
  while IFS=$'\t' read -r node role target; do
    [[ "$node" != "$NODE" ]] || found="$role $target"
  done <"${RUN_DIR}/nodes.tsv"
  [[ -n "$found" ]] || die "$NODE is not a monitored node ($(cut -f1 "${RUN_DIR}/nodes.tsv" | tr '\n' ' '))"
  read -r role target <<<"$found"

  bmac_ui_step "Send the test pulse from $NODE"
  monitoring_exec "$role" "$target" python3 -I "$(mon_program_for "$role")" test-pulse \
    --priority "$PRIORITY" >"${RUN_DIR}/pulse.json" ||
    die "$NODE could not send the test pulse (is its agent installed? run update_cluster_runtime.sh)"
  bmac_ui_step_done

  local name="${PROXMOX_CLUSTER_NAME} bmaccheck ${NODE}"
  printf '\nSent a %s test pulse from %s. Within a minute or two, expect:\n\n' "$PRIORITY" "$NODE"
  printf '  1. "DOWN | %s" containing "bmacevent bmac%s" and\n' "$name" "$PRIORITY"
  printf '     "NEW [%s] test:alert": your forwarding filter (F2, or F2%s with the\n' "$PRIORITY" \
    "$([[ $PRIORITY == p1 ]] && echo a || echo b)"
  printf '     priority split) labels it BMAC and forwards it to Pushover.\n'
  printf '  2. "UP | %s" containing "bmacclear": filter F1 deletes it.\n\n' "$name"
  printf 'If the DOWN email is not forwarded, or the UP email stays in the inbox, check\n'
  printf 'the filters in the README section "Monitoring and Alerts".\n'
  bmac_ui_result node "$NODE" priority "$PRIORITY" check "$name"
  bmac_ui_next_step "Check the DOWN email was forwarded to Pushover and the UP email was deleted."
}

if [[ "${SEND_TEST_ALERT_SOURCE_ONLY:-0}" != 1 ]]; then
  main "$@"
fi
