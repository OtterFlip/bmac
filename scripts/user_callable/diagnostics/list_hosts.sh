#!/usr/bin/env bash

# Copyright (c) 2026 BEENTHERE VENTURES, INC.
# SPDX-License-Identifier: GPL-3.0-only

# Fast, read-only list of the cluster's hosts with quorum and QDevice
# registration, read through one reachable member. For a deep check use
# show_cluster_health.sh.

set -u
set -o pipefail
umask 077

if ((BASH_VERSINFO[0] < 4 || (BASH_VERSINFO[0] == 4 && BASH_VERSINFO[1] < 4))); then
  printf 'ERROR: list_hosts.sh requires Bash 4.4 or newer (found %s).\n' \
    "${BASH_VERSION:-unknown}" >&2
  exit 2
fi

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
REPO_ROOT="$(cd -- "${SCRIPT_DIR}/../../.." && pwd -P)"
for library in config.sh cluster_control.sh ui_protocol.sh quick_state.sh; do
  [[ -f "${REPO_ROOT}/scripts/lib/${library}" && ! -L "${REPO_ROOT}/scripts/lib/${library}" ]] || {
    printf 'ERROR: required library is unavailable: scripts/lib/%s\n' "$library" >&2
    exit 2
  }
  # shellcheck source=/dev/null
  source "${REPO_ROOT}/scripts/lib/${library}"
done
bmac_ui_bootstrap "$@"

usage() {
  cat <<'EOF'
Usage: scripts/user_callable/diagnostics/list_hosts.sh [--no-ssh-check] [--json]

Quickly list every Proxmox host in the cluster: online state, uptime, CPU and
memory use, registry slot, and which host is the control node, plus cluster
quorum, votes, and QDevice registration and voting. Everything is read through
one reachable member, so this takes seconds rather than minutes.

  --no-ssh-check  Skip checking SSH from this workstation to every host.
  --json          Emit bmac-ui NDJSON events instead of a terminal report.

Nothing is changed. Exit status: 0 when the state was read (problems are
reported in the output), 1 when it could not be read, 2 for usage errors.
EOF
}

SSH_CHECK=true
while (($#)); do
  case "$1" in
    --no-ssh-check) SSH_CHECK=false; shift ;;
    -h | --help) usage; exit 0 ;;
    *) usage >&2; exit 2 ;;
  esac
done

qs_init
qs_find_probe
qs_collect hosts
render_args=(--qdevice-host "${PROXMOX_QDEVICE_HOST:-}")
if [[ "$SSH_CHECK" == true ]]; then
  qs_check_ssh
  render_args+=(--ssh-file "${QS_RUN_DIR}/ssh.tsv")
fi
qs_render hosts "${render_args[@]}"
