#!/usr/bin/env bash

# Copyright (c) 2026 BEENTHERE VENTURES, INC.
# SPDX-License-Identifier: GPL-3.0-only

# Fast, read-only check that this workstation reaches the prepared QDevice
# (PROXMOX_QDEVICE_HOST) as root over strict SSH. It needs no cluster member,
# so it answers before the first Proxmox host exists.

set -u
set -o pipefail
umask 077

if ((BASH_VERSINFO[0] < 4 || (BASH_VERSINFO[0] == 4 && BASH_VERSINFO[1] < 4))); then
  printf 'ERROR: check_qdevice_access.sh requires Bash 4.4 or newer (found %s).\n' \
    "${BASH_VERSION:-unknown}" >&2
  exit 2
fi

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
REPO_ROOT="$(cd -- "${SCRIPT_DIR}/../../.." && pwd -P)"
for library in config.sh ui_protocol.sh; do
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
Usage: scripts/user_callable/diagnostics/check_qdevice_access.sh [--json]

Check that this workstation reaches the QDevice named by PROXMOX_QDEVICE_HOST
in config/cluster.conf as root over SSH (BatchMode, strict host key checking).
Prepare the QDevice with docs/QDEVICE_MANUAL_SETUP.md first. No cluster member
is needed, so this works before the first Proxmox host is set up.

  --json  Emit bmac-ui NDJSON events instead of a terminal report.

Nothing is changed. Exit status: 0 when the check ran (whether or not the
QDevice is accessible, which the output reports), 2 for usage or
configuration errors.
EOF
}

while (($#)); do
  case "$1" in
    -h | --help) usage; exit 0 ;;
    *) usage >&2; exit 2 ;;
  esac
done

load_proxmox_config --no-secrets >/dev/null || {
  printf 'ERROR: cluster configuration is invalid\n' >&2
  bmac_ui_error config_invalid "cluster configuration is invalid"
  exit 2
}

host="${PROXMOX_QDEVICE_HOST:-}"
accessible=false
problem=""
bmac_ui_step "Check SSH to the QDevice"
if [[ -z "$host" ]]; then
  problem="PROXMOX_QDEVICE_HOST is not set in config/cluster.conf"
elif error="$(ssh -o BatchMode=yes -o StrictHostKeyChecking=yes -o CheckHostIP=yes \
  -o ConnectTimeout=10 "root@${host}" true 2>&1 </dev/null)"; then
  accessible=true
else
  problem="'ssh root@${host}' failed${error:+: ${error//$'\n'/ }}"
fi
bmac_ui_step_done

if [[ "$accessible" == true ]]; then
  printf 'OK: ssh root@%s works from this workstation.\n' "$host"
else
  printf 'ATTENTION: the QDevice is not accessible: %s.\n' "$problem"
  printf 'Prepare it with docs/QDEVICE_MANUAL_SETUP.md, then check again.\n'
  bmac_ui_next_step "Prepare the QDevice with docs/QDEVICE_MANUAL_SETUP.md so 'ssh root@${host:-<qdevice>}' works from this workstation."
fi
bmac_ui_result "configured_host?" "$host" accessible:bool "$accessible" "problem?" "$problem"
