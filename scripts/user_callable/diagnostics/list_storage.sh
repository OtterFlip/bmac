#!/usr/bin/env bash

# Copyright (c) 2026 BEENTHERE VENTURES, INC.
# SPDX-License-Identifier: GPL-3.0-only

# Fast, read-only list of every host's ZFS pools (health and capacity) and
# Proxmox storage, read through one reachable member. For vdev members, disk
# serials, and SMART use show_cluster_health.sh or show_proxmox_host_state.sh.

set -u
set -o pipefail
umask 077

if ((BASH_VERSINFO[0] < 4 || (BASH_VERSINFO[0] == 4 && BASH_VERSINFO[1] < 4))); then
  printf 'ERROR: list_storage.sh requires Bash 4.4 or newer (found %s).\n' \
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
Usage: scripts/user_callable/diagnostics/list_storage.sh [--host moxN] [--json]

Quickly list every online host's ZFS pools with health, size, allocation, and
free space (flagging less than 10% free), and its Proxmox storage with usage.

  --host moxN  Show only one host.
  --json       Emit bmac-ui NDJSON events instead of a terminal report.

Nothing is changed. Exit status: 0 when the state was read (problems are
reported in the output), 1 when it could not be read, 2 for usage errors.
EOF
}

HOST=""
while (($#)); do
  case "$1" in
    --host)
      (($# >= 2)) && [[ "$2" =~ ^mox([1-9]|10)$ ]] || {
        usage >&2
        exit 2
      }
      HOST="$2"
      shift 2
      ;;
    -h | --help) usage; exit 0 ;;
    *) usage >&2; exit 2 ;;
  esac
done

qs_init
qs_find_probe
qs_collect storage --host "$HOST"
qs_render storage
