#!/usr/bin/env bash

# Copyright (c) 2026 BEENTHERE VENTURES, INC.
# SPDX-License-Identifier: GPL-3.0-only

# Fast, read-only list of registered production and staging VMs with their
# live state, HA, replication, and routes, read through one reachable member.
# For a deep check of one production VM use show_prod_vm_state.sh.

set -u
set -o pipefail
umask 077

if ((BASH_VERSINFO[0] < 4 || (BASH_VERSINFO[0] == 4 && BASH_VERSINFO[1] < 4))); then
  printf 'ERROR: list_guests.sh requires Bash 4.4 or newer (found %s).\n' \
    "${BASH_VERSION:-unknown}" >&2
  exit 2
fi

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
REPO_ROOT="$(cd -- "${SCRIPT_DIR}/.." && pwd -P)"
for library in config.sh cluster_control.sh ui_protocol.sh quick_state.sh; do
  [[ -f "${REPO_ROOT}/lib/${library}" && ! -L "${REPO_ROOT}/lib/${library}" ]] || {
    printf 'ERROR: required library is unavailable: lib/%s\n' "$library" >&2
    exit 2
  }
  # shellcheck source=/dev/null
  source "${REPO_ROOT}/lib/${library}"
done
bmac_ui_bootstrap "$@"

usage() {
  cat <<'EOF'
Usage: diagnostics/list_guests.sh [--kind production|staging] [--json]

Quickly list every registered guest: production VMs with their registry
state, live state and node, placement, HA state, replication health, routes,
and domain; staging VMs with their source, node, address, and URL; and any
QEMU VM the registry does not know about.

  --kind KIND  Show only production or only staging VMs.
  --json       Emit bmac-ui NDJSON events instead of a terminal report.

Nothing is changed. Exit status: 0 when the state was read (problems are
reported in the output), 1 when it could not be read, 2 for usage errors.
EOF
}

KIND=""
while (($#)); do
  case "$1" in
    --kind)
      (($# >= 2)) && [[ "$2" == production || "$2" == staging ]] || {
        usage >&2
        exit 2
      }
      KIND="$2"
      shift 2
      ;;
    -h | --help) usage; exit 0 ;;
    *) usage >&2; exit 2 ;;
  esac
done

qs_init
qs_find_probe
qs_collect guests
qs_render guests --kind-filter "$KIND"
