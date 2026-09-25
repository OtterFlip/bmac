# shellcheck shell=bash

# Copyright (c) 2026 BEENTHERE VENTURES, INC.
# SPDX-License-Identifier: GPL-3.0-only

# Workstation helpers shared by hosts/add_new_disk_vdev.sh,
# hosts/add_replacement_disk.sh, hosts/decommission_disks.sh, and
# hosts/list_disks_ready_for_physically_removal.sh. Source after setting
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
DW_CONF_MIRRORS="${DW_LIB_DIR}/mox_conf_mirrors.py"
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
    "$DW_RPOOL_MIRROR" "$DW_CONF_MIRRORS"; do
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
  dw_host_python "$DW_HOST_STORAGE" collect >"$destination" ||
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

dw_sha256() {
  python3 -c 'import hashlib,sys; print(hashlib.sha256(open(sys.argv[1],"rb").read()).hexdigest())' "$1"
}

# Choose the target host: the --host value, or a menu of cluster nodes.
dw_select_host() {
  local requested="$1" coordinator nodes_json index choice
  local -a names=()
  if [[ -n "$requested" ]]; then
    [[ "$requested" =~ ^mox([1-9]|10)$ ]] || dw_die "host must be named mox1 through mox10"
    (("${requested#mox}" <= MAX_MOX_HOSTS)) ||
      dw_die "$requested exceeds MAX_MOX_HOSTS=$MAX_MOX_HOSTS"
    DW_HOST="$requested"
  else
    coordinator="$(first_reachable_mox)" || dw_die "no mox host is reachable over strict SSH"
    nodes_json="$(dw_on_node "$coordinator" pvesh get /nodes --output-format json)" ||
      dw_die "could not list cluster nodes through $coordinator"
    mapfile -t names < <(
      python3 - "$nodes_json" <<'PY'
import json
import sys
rows = sorted(json.loads(sys.argv[1]), key=lambda row: int(str(row.get("node", "mox0"))[3:] or 0))
for row in rows:
    print(f"{row.get('node')}\t{row.get('status', 'unknown')}")
PY
    )
    ((${#names[@]} > 0)) || dw_die "the cluster reported no nodes"
    printf '\nProxmox hosts in the cluster:\n'
    for index in "${!names[@]}"; do
      printf '  %d) %s (%s)\n' "$((index + 1))" "${names[index]%%$'\t'*}" "${names[index]#*$'\t'}"
    done
    IFS= read -r -p "Choose the target host [1-${#names[@]}]: " choice
    [[ "$choice" =~ ^[1-9][0-9]*$ ]] && ((choice <= ${#names[@]})) ||
      dw_die "no host was chosen"
    DW_HOST="${names[choice - 1]%%$'\t'*}"
  fi
  mox_is_reachable "$DW_HOST" || dw_die "$DW_HOST is not reachable over strict SSH"
  printf '\nTarget host: %s\n' "$DW_HOST"
}

# Ask before a command that can take a long time; nothing runs on "no".
dw_ready() {
  local what="$1" duration="$2"
  printf '\nNEXT: %s\n' "$what"
  printf 'This can take %s and blocks until it finishes.\n' "$duration"
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

dw_conf_path() {
  printf '%s/%s.conf\n' "$PROXMOX_ENV_DIR" "$DW_HOST"
}

# Apply a mox_conf_mirrors.py edit to env/moxN.conf, then prove lib/config.sh
# still loads it; restore the original bytes if it does not.
dw_edit_conf() {
  local conf backup status=0
  conf="$(dw_conf_path)"
  [[ -f "$conf" && ! -L "$conf" ]] || return 3
  backup="${DW_RUN_DIR}/conf-before-edit"
  cp -p -- "$conf" "$backup"
  python3 "$DW_CONF_MIRRORS" "$1" "$conf" "${@:2}" || status=$?
  ((status == 0)) || return "$status"
  if ! bash -c 'set -Eeuo pipefail; source "$1"; load_proxmox_config --host "$2" --no-secrets >/dev/null' \
    bash "$DW_CONFIG_LIB" "$DW_HOST"; then
    cp -p -- "$backup" "$conf"
    printf 'The edited %s did not load; the original was restored.\n' "$conf" >&2
    return 1
  fi
}
