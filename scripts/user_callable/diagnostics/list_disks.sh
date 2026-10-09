#!/usr/bin/env bash

# Copyright (c) 2026 BEENTHERE VENTURES, INC.
# SPDX-License-Identifier: GPL-3.0-only

# Read-only list of every host's pool vdevs (state, LUKS, member disks) and
# every disk outside a pool with its status: available, awaiting retirement
# finalization, part of an interrupted add or replace, or in use. Each host is
# read directly, in parallel, with the collectors scripts/user_callable/hosts/inventory_disks.sh
# uses; unlike that workflow, this never finalizes anything.

set -u
set -o pipefail
umask 077

if ((BASH_VERSINFO[0] < 4 || (BASH_VERSINFO[0] == 4 && BASH_VERSINFO[1] < 4))); then
  printf 'ERROR: list_disks.sh requires Bash 4.4 or newer (found %s).\n' \
    "${BASH_VERSION:-unknown}" >&2
  exit 2
fi

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
REPO_ROOT="$(cd -- "${SCRIPT_DIR}/../../.." && pwd -P)"
for library in lib/config.sh lib/cluster_control.sh lib/ui_protocol.sh lib/quick_state.sh \
  utilities/host_storage.py utilities/storage_state.py utilities/disk_inventory.py; do
  [[ -f "${REPO_ROOT}/scripts/${library}" && ! -L "${REPO_ROOT}/scripts/${library}" ]] || {
    printf 'ERROR: required library is unavailable: scripts/%s\n' "$library" >&2
    exit 2
  }
  [[ "$library" != *.py ]] || continue
  # shellcheck source=/dev/null
  source "${REPO_ROOT}/scripts/${library}"
done
bmac_ui_bootstrap "$@"

usage() {
  cat <<'EOF'
Usage: scripts/user_callable/diagnostics/list_disks.sh [--host moxN] [--json]

List every online host's ZFS pool vdevs with their state, whether they use
LUKS, and each member disk's serial and size, then every physical disk that
is not in a pool with its serial, size, and status:

  Available               unused; safe to pull or to use in a new vdev or
                          replacement
  Awaiting finalization   decommissioned and evacuated; run
                          scripts/user_callable/hosts/inventory_disks.sh to finalize its retirement
  Evacuating              ZFS is still copying data off its vdev
  Replacement in progress / New mirror in progress
                          an interrupted add or replace; rerun that workflow
  In use                  mounted, open, or in another pool

  --host moxN  Show only one host.
  --json       Emit bmac-ui NDJSON events instead of a terminal report.

Every host is read directly over SSH from this workstation. Nothing is
changed. Exit status: 0 when the state was read (problems are reported in the
output), 1 when it could not be read, 2 for usage errors.
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

NODES=()
for node in "${CONTROL_MEMBER_NODES[@]}"; do
  [[ -z "$HOST" || "$node" == "$HOST" ]] && NODES+=("$node")
done
((${#NODES[@]} > 0)) ||
  QS_ERROR_CODE=unknown_host qs_die "$HOST is not a member of the cluster"

# collect_host NODE: leave NODE.layout.json and NODE.state.json, or their .err.
collect_host() {
  local node="$1" base="${QS_RUN_DIR}/$1"
  mox_ssh "$node" python3 - collect --allow-missing-pool \
    <"${REPO_ROOT}/scripts/utilities/host_storage.py" >"${base}.layout.json" 2>"${base}.layout.err" ||
    { rm -f -- "${base}.layout.json"; return 0; }
  mox_ssh "$node" python3 - show \
    <"${REPO_ROOT}/scripts/utilities/storage_state.py" >"${base}.state.json" 2>"${base}.state.err" ||
    rm -f -- "${base}.state.json"
}

bmac_ui_step "Read the disks of ${NODES[*]}"
: >"${QS_RUN_DIR}/hosts.tsv"
pids=()
for node in "${NODES[@]}"; do
  if control_list_contains "$node" "${CONTROL_ONLINE_NODES[@]}"; then
    printf '%s\tonline\n' "$node" >>"${QS_RUN_DIR}/hosts.tsv"
    collect_host "$node" &
    pids+=("$!")
  else
    printf '%s\toffline\n' "$node" >>"${QS_RUN_DIR}/hosts.tsv"
  fi
done
((${#pids[@]} == 0)) || wait "${pids[@]}"

bmac_ui_step "Summarize disk state"
python3 "${REPO_ROOT}/scripts/utilities/disk_inventory.py" render --run-dir "$QS_RUN_DIR" ||
  qs_die "could not summarize the disk state"
bmac_ui_step_done
