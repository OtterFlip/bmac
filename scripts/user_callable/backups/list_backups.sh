#!/usr/bin/env bash

# Copyright (c) 2026 BEENTHERE VENTURES, INC.
# SPDX-License-Identifier: GPL-3.0-only

# Fast, read-only state of the cluster's PBS backups: the bmac-pbs storage,
# the retention policy the PBS server applies, each production VM's backup
# schedule, and the restore points BMAC made, read through one reachable
# member.

set -u
set -o pipefail
umask 077

if ((BASH_VERSINFO[0] < 4 || (BASH_VERSINFO[0] == 4 && BASH_VERSINFO[1] < 4))); then
  printf 'ERROR: list_backups.sh requires Bash 4.4 or newer (found %s).\n' \
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
Usage: scripts/user_callable/backups/list_backups.sh [--json]

Quickly list the cluster's PBS backups: whether the bmac-pbs storage is set
up and active, the retention policy the PBS server's prune job applies (when
the token can read it), each production VM's backup schedule and newest
backup, and every restore point BMAC made, newest first, with its protection,
encryption key, and how it was backed up (incremental or a full read, data
reused and new, time taken) as its vzdump task log records it. Those details
are read once and cached on the cluster host that answers, in
/var/lib/app-ha/backup-metrics.json, so every workstation sees the same.

  --json         Emit bmac-ui NDJSON events instead of a terminal report.

Nothing else is changed. Exit status: 0 when the state was
read (problems are reported in the output), 1 when it could not be read, 2 for
usage errors.
EOF
}

while (($#)); do
  case "$1" in
    -h | --help) usage; exit 0 ;;
    *) usage >&2; exit 2 ;;
  esac
done

qs_init
qs_find_probe
# How each restore point was backed up comes from its vzdump task log. The
# host that answers caches what it read in /var/lib/app-ha/ (outside pmxcfs),
# and consults the other hosts' caches before reading any log.
qs_collect backups --pbs-api /usr/local/lib/app-ha-proxmox/lib/pbs_api.py \
  --internal-domain "${PROXMOX_INTERNAL_DOMAIN:-}"
qs_render backups
