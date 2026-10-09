#!/usr/bin/env bash

# Copyright (c) 2026 BEENTHERE VENTURES, INC.
# SPDX-License-Identifier: GPL-3.0-only

# Fast, read-only list of every Proxmox replication job with its last sync,
# next sync, failure count, and error, read through one reachable member.

set -u
set -o pipefail
umask 077

if ((BASH_VERSINFO[0] < 4 || (BASH_VERSINFO[0] == 4 && BASH_VERSINFO[1] < 4))); then
  printf 'ERROR: list_replication.sh requires Bash 4.4 or newer (found %s).\n' \
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
Usage: scripts/user_callable/diagnostics/list_replication.sh [--guest prodN] [--json]

Quickly list every replication job on every online host: source and target
host, schedule, time of the last successful sync and of the next one, failure
count, and the last error.

  --guest prodN  Show only the jobs of one production VM.
  --json         Emit bmac-ui NDJSON events instead of a terminal report.

Nothing is changed. Exit status: 0 when the state was read (problems are
reported in the output), 1 when it could not be read, 2 for usage errors.
EOF
}

GUEST=""
while (($#)); do
  case "$1" in
    --guest)
      (($# >= 2)) && [[ "$2" =~ ^prod[1-9][0-9]*$ ]] || {
        usage >&2
        exit 2
      }
      GUEST="$2"
      shift 2
      ;;
    -h | --help) usage; exit 0 ;;
    *) usage >&2; exit 2 ;;
  esac
done

qs_init
qs_find_probe
qs_collect replication
qs_render replication --guest "$GUEST"
