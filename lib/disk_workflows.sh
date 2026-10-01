# shellcheck shell=bash

# Copyright (c) 2026 BEENTHERE VENTURES, INC.
# SPDX-License-Identifier: GPL-3.0-only

# Workstation helpers shared by the host disk workflows. Source after setting
# DW_SCRIPT_NAME. Host work goes through lib/config.sh strict SSH; host-side
# logic lives in lib/rpool_mirror.sh, lib/host_storage.py, and
# lib/storage_state.py, which are piped or installed per run so the host
# always runs this checkout's version.

DW_LIB_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
DW_REPO_ROOT="$(cd -- "${DW_LIB_DIR}/.." && pwd -P)"
DW_CONFIG_LIB="${DW_LIB_DIR}/config.sh"
DW_HOST_STORAGE="${DW_LIB_DIR}/host_storage.py"
DW_STORAGE_STATE="${DW_LIB_DIR}/storage_state.py"
DW_RPOOL_MIRROR="${DW_LIB_DIR}/rpool_mirror.sh"
DW_REMOTE_REGISTRY=/usr/local/lib/app-ha-proxmox/lib/cluster_registry.py
if [[ "${APP_HA_DISK_TEST_MODE:-0}" == 1 ]]; then
  DW_REMOTE_TOOL="${APP_HA_REMOTE_RPOOL_MIRROR:?APP_HA_REMOTE_RPOOL_MIRROR is required in test mode}"
  DW_REMOTE_ROOT="${APP_HA_REMOTE_ROOT:?APP_HA_REMOTE_ROOT is required in test mode}"
  DW_ARTIFACTS_DIR="${APP_HA_ARTIFACTS_DIR:?APP_HA_ARTIFACTS_DIR is required in test mode}"
else
  DW_REMOTE_TOOL=/usr/local/sbin/app-ha-rpool-mirror
  DW_REMOTE_ROOT=/root
  DW_ARTIFACTS_DIR="${DW_REPO_ROOT}/hosts/artifacts"
fi
DW_HOST=""
DW_RUN_DIR=""

dw_die() {
  printf '\nERROR: %s\n' "$*" >&2
  exit 1
}

dw_info() {
  printf '    %s\n' "$*"
}

dw_section() {
  printf '\n==== %s ====\n' "$*"
}

dw_init() {
  if ((BASH_VERSINFO[0] < 4 || (BASH_VERSINFO[0] == 4 && BASH_VERSINFO[1] < 4))); then
    printf 'ERROR: %s requires Bash 4.4 or newer (found %s).\n' \
      "$DW_SCRIPT_NAME" "${BASH_VERSION:-unknown}" >&2
    exit 2
  fi
  command -v python3 >/dev/null 2>&1 || dw_die "python3 is required on this workstation"
  local library
  for library in "$DW_CONFIG_LIB" "$DW_HOST_STORAGE" "$DW_STORAGE_STATE" \
    "$DW_RPOOL_MIRROR"; do
    [[ -f "$library" && ! -L "$library" ]] || dw_die "required library is unavailable: $library"
  done
  # shellcheck source=./config.sh
  source "$DW_CONFIG_LIB"
  load_proxmox_config --no-secrets >/dev/null || dw_die "cluster configuration is invalid"
  DW_RUN_DIR="$(mktemp -d "${TMPDIR:-/tmp}/${DW_SCRIPT_NAME%.sh}.XXXXXX")"
  chmod 0700 "$DW_RUN_DIR"
  trap 'rm -rf -- "$DW_RUN_DIR"' EXIT
}

dw_on_host() {
  mox_ssh "$DW_HOST" "$@" </dev/null
}

dw_on_node() {
  local node="$1"
  shift
  mox_ssh "$node" "$@" </dev/null
}

# Run a checked-in Python helper on the target host: python3 - ARGS < FILE.
dw_host_python() {
  local file="$1"
  shift
  mox_ssh "$DW_HOST" python3 - "$@" <"$file"
}

dw_state() {
  dw_host_python "$DW_STORAGE_STATE" "$@"
}

dw_collect_layout() {
  local destination="$1"
  shift
  dw_host_python "$DW_HOST_STORAGE" collect "$@" >"$destination" ||
    dw_die "could not collect the rpool layout of $DW_HOST"
}

dw_json() {
  # dw_json FILE PYTHON_EXPRESSION: evaluate against the parsed document `d`.
  python3 -c 'import json,sys; d=json.load(open(sys.argv[1])); r=eval(sys.argv[2]); print(r if isinstance(r, str) else json.dumps(r))' \
    "$1" "$2"
}

dw_render_layout() {
  python3 "$DW_HOST_STORAGE" render "$1"
}

dw_render_inventory() {
  python3 "$DW_HOST_STORAGE" render-inventory "$1"
}

dw_inventory() {
  local destination="$1"
  shift
  dw_collect_layout "$destination" "$@"
  dw_section "Full disk inventory on $DW_HOST"
  dw_render_inventory "$destination"
}

dw_sha256() {
  python3 -c 'import hashlib,sys; print(hashlib.sha256(open(sys.argv[1],"rb").read()).hexdigest())' "$1"
}

# Choose the target host: the --host value, or an interactive prompt defaulting
# to mox1.
dw_select_host() {
  local requested="$1"
  if [[ -z "$requested" ]]; then
    IFS= read -r -p "Target Proxmox host [mox1]: " requested ||
      dw_die "input ended"
    requested="${requested:-mox1}"
  fi
  [[ "$requested" =~ ^mox([1-9]|10)$ ]] ||
    dw_die "host must be named mox1 through mox10"
  (("${requested#mox}" <= MAX_MOX_HOSTS)) ||
    dw_die "$requested exceeds MAX_MOX_HOSTS=$MAX_MOX_HOSTS"
  DW_HOST="$requested"
  mox_is_reachable "$DW_HOST" || dw_die "$DW_HOST is not reachable over strict SSH"
  printf '\nTarget host: %s\n' "$DW_HOST"
}

# Drop keystrokes typed while a long step ran (an Enter pressed to check for
# progress) so they cannot answer the next prompt.
dw_discard_typeahead() {
  [[ -t 0 ]] || return 0
  local _
  while IFS= read -r -s -t 0.1 -n 4096 _; do :; done
  return 0
}

# Ask before a command that can take a long time; nothing runs on "no".
dw_ready() {
  local what="$1" duration="$2"
  printf '\nNEXT: %s\n' "$what"
  printf 'This can take %s and blocks until it finishes.\n' "$duration"
  dw_discard_typeahead
  prompt_yes "Ready to start it now?"
}

# Install lib/rpool_mirror.sh on the host and prove its contents.
dw_install_tool() {
  local expected actual
  expected="$(dw_sha256 "$DW_RPOOL_MIRROR")"
  actual="$(dw_on_host sha256sum "$DW_REMOTE_TOOL" 2>/dev/null | awk '{print $1}')" ||
    actual=""
  if [[ "$actual" != "$expected" ]]; then
    # shellcheck disable=SC2016 # The program is expanded by the remote shell.
    mox_ssh "$DW_HOST" bash -c '
set -Eeuo pipefail
umask 077
target="$1"
temporary="$(mktemp "${target%/*}/.app-ha-rpool-mirror.XXXXXX")"
trap '\''rm -f -- "$temporary"'\'' EXIT
cat >"$temporary"
chmod 0700 "$temporary"
mv -f -- "$temporary" "$target"
trap - EXIT
' bash "$DW_REMOTE_TOOL" <"$DW_RPOOL_MIRROR" ||
      dw_die "could not install $DW_REMOTE_TOOL on $DW_HOST"
    actual="$(dw_on_host sha256sum "$DW_REMOTE_TOOL" | awk '{print $1}')" || actual=""
  fi
  [[ "$actual" == "$expected" ]] ||
    dw_die "$DW_REMOTE_TOOL on $DW_HOST does not match $DW_RPOOL_MIRROR"
}

dw_tool() {
  dw_on_host "$DW_REMOTE_TOOL" "$@"
}
