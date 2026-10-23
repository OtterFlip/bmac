#!/usr/bin/env bash

# Copyright (c) 2026 BEENTHERE VENTURES, INC.
# SPDX-License-Identifier: GPL-3.0-only

# Install, roll back, or disable BMAC's monitor agent on one node. The
# workstation (scripts/lib/monitoring.sh) copies this file's bundle to a
# private staging directory on the node and runs it there as root:
#
#   monitor_node_install.sh install STAGE ROLE PROGRAM
#   monitor_node_install.sh rollback STAGE PROGRAM
#   monitor_node_install.sh cleanup STAGE
#   monitor_node_install.sh disable
#   monitor_node_install.sh installed
#
# STAGE holds config.json, app-ha-monitor.service, and, for the QDevice and
# production VMs, app_ha_monitor.py (Proxmox hosts get the program from the
# runtime update). install keeps every file it replaces in STAGE, so
# rollback restores exactly what was there, or removes what was absent. The
# agent re-reads its config every cycle, so it is restarted only when its
# program or unit changed or it was not running. A production VM's
# guest.json is created when absent and never overwritten.

set -Eeuo pipefail
set +x
umask 077

# Tests point every absolute path below a temporary root.
ROOT="${APP_HA_MONITOR_TEST_ROOT:-}"
UNIT="app-ha-monitor.service"
UNIT_PATH="${ROOT}/etc/systemd/system/${UNIT}"
CONFIG_DIR="${ROOT}/etc/app-ha-monitor"
CONFIG_PATH="${CONFIG_DIR}/config.json"
GUEST_SETTINGS="${CONFIG_DIR}/guest.json"
STATE_DIR="${ROOT}/var/lib/app-ha-monitor"

fail() {
  printf 'ERROR: %s\n' "$*" >&2
  exit 1
}

safe_stage() {
  [[ "$1" =~ ^(/run|/tmp)/app-ha-monitor-stage[.][A-Za-z0-9]+$ ||
    ( -n "$ROOT" && "$1" == "$ROOT"/* ) ]] ||
    fail "unsafe staging directory: $1"
  [[ -d "$1" && ! -L "$1" ]] || fail "staging directory is missing: $1"
}

safe_program() {
  [[ "$1" =~ ^/usr/local/lib/app-ha-(proxmox/lib|monitor)/app_ha_monitor[.]py$ ]] ||
    fail "unsafe program path: $1"
}

python_is_new_enough() {
  python3 -c 'import sys; sys.exit(sys.version_info < (3, 11))' 2>/dev/null
}

# The commands each role's checks use, as command:package.
role_tools() {
  case "$1" in
    host) printf '%s\n' smartctl:smartmontools zpool: zfs: corosync-cfgtool: \
      corosync-quorumtool: postqueue: ;;
    qdevice) printf '%s\n' ping:iputils-ping tailscale: df: ;;
    guest) printf '%s\n' openssl:openssl df: systemctl: ;;
  esac
}

ensure_tools() {
  local role="$1" stage="$2" entry command_name package
  local -a packages=() missing=()
  while IFS= read -r entry; do
    command_name="${entry%%:*}"
    package="${entry#*:}"
    command -v "$command_name" >/dev/null 2>&1 && continue
    if [[ -n "$package" ]]; then
      packages+=("$package")
    else
      missing+=("$command_name")
    fi
  done < <(role_tools "$role")
  ((${#missing[@]} == 0)) ||
    fail "required commands are missing and BMAC does not install them: ${missing[*]}"
  ((${#packages[@]})) || return 0
  [[ -z "$ROOT" ]] || fail "would install packages: ${packages[*]}"
  [[ -f "$stage/apt_lock_wait.sh" ]] || fail "missing packages: ${packages[*]}"
  # shellcheck source=apt_lock_wait.sh disable=SC1091 # Staged next to this script.
  source "$stage/apt_lock_wait.sh"
  printf 'Installing %s\n' "${packages[*]}"
  apt_wait_for_locks
  apt-get -o DPkg::Lock::Timeout=60 update
  DEBIAN_FRONTEND=noninteractive apt-get -o DPkg::Lock::Timeout=60 install -y "${packages[@]}"
}

# Keep one file for rollback: NAME.old, or NAME.absent when there was none.
keep() {
  local path="$1" name="$2" stage="$3"
  if [[ -e "$path" ]]; then
    [[ -f "$path" && ! -L "$path" ]] || fail "$path is not a regular file"
    cp -p -- "$path" "$stage/backup/${name}.old"
  else
    : >"$stage/backup/${name}.absent"
  fi
}

restore() {
  local path="$1" name="$2" stage="$3"
  if [[ -f "$stage/backup/${name}.absent" ]]; then
    rm -f -- "$path"
  elif [[ -f "$stage/backup/${name}.old" ]]; then
    cp -p -- "$stage/backup/${name}.old" "${path}.rollback"
    mv -f -- "${path}.rollback" "$path"
  fi
}

put() {
  local source="$1" destination="$2" mode="$3"
  install -d -m 0755 "$(dirname -- "$destination")"
  install -m "$mode" "$source" "${destination}.new"
  mv -f -- "${destination}.new" "$destination"
}

same() {
  [[ -f "$2" ]] && cmp -s -- "$1" "$2"
}

command_install() {
  local stage="$1" role="$2" program="$3" restart=false staged_program
  safe_stage "$stage"
  safe_program "$program"
  [[ "$role" =~ ^(host|qdevice|guest)$ ]] || fail "unknown role: $role"
  python_is_new_enough ||
    fail "python3 is missing or older than 3.11 ($(python3 --version 2>&1 || true)); upgrade it first"
  ensure_tools "$role" "$stage"
  for name in config.json "$UNIT"; do
    [[ -f "$stage/$name" && ! -L "$stage/$name" ]] || fail "staged $name is missing"
  done
  staged_program="$stage/app_ha_monitor.py"
  if [[ "$role" == host ]]; then
    [[ ! -e "$staged_program" ]] || fail "hosts get the agent from the runtime update"
    [[ -f "${ROOT}${program}" ]] ||
      fail "${program} is missing; run update_cluster_runtime.sh first"
    staged_program="${ROOT}${program}"
  else
    [[ -f "$staged_program" && ! -L "$staged_program" ]] || fail "staged agent is missing"
  fi
  PYTHONDONTWRITEBYTECODE=1 python3 -I "$staged_program" --help >/dev/null
  PYTHONDONTWRITEBYTECODE=1 python3 -I "$staged_program" \
    --config "$stage/config.json" validate-config >/dev/null

  install -d -m 0700 "$stage/backup"
  keep "$UNIT_PATH" unit "$stage"
  keep "$CONFIG_PATH" config "$stage"
  [[ "$role" == host ]] || keep "${ROOT}${program}" program "$stage"
  [[ "$role" != guest ]] || keep "$GUEST_SETTINGS" guest "$stage"
  if systemctl is-enabled --quiet "$UNIT" 2>/dev/null; then : >"$stage/backup/was-enabled"; fi
  if systemctl is-active --quiet "$UNIT" 2>/dev/null; then : >"$stage/backup/was-active"; fi
  : >"$stage/committed"

  install -d -m 0700 "$CONFIG_DIR" "$STATE_DIR"
  if [[ "$role" != host ]] && ! same "$staged_program" "${ROOT}${program}"; then
    put "$staged_program" "${ROOT}${program}" 0755
    restart=true
  fi
  if ! same "$stage/$UNIT" "$UNIT_PATH"; then
    put "$stage/$UNIT" "$UNIT_PATH" 0644
    restart=true
  fi
  put "$stage/config.json" "$CONFIG_PATH" 0600
  if [[ "$role" == guest && ! -e "$GUEST_SETTINGS" ]]; then
    printf '{\n  "cert_paths": [],\n  "schema_version": 1,\n  "services": []\n}\n' \
      >"$stage/guest.json"
    put "$stage/guest.json" "$GUEST_SETTINGS" 0600
  fi
  systemctl daemon-reload
  systemctl enable --quiet "$UNIT"
  if [[ "$restart" == true ]] || ! systemctl is-active --quiet "$UNIT"; then
    systemctl restart "$UNIT"
    printf '__MONITOR_INSTALL__ restarted\n'
  else
    printf '__MONITOR_INSTALL__ config-only\n'
  fi
}

command_rollback() {
  local stage="$1" program="$2"
  safe_stage "$stage"
  safe_program "$program"
  [[ -f "$stage/committed" ]] || return 0
  restore "$UNIT_PATH" unit "$stage"
  restore "$CONFIG_PATH" config "$stage"
  restore "${ROOT}${program}" program "$stage"
  restore "$GUEST_SETTINGS" guest "$stage"
  systemctl daemon-reload
  if [[ -f "$UNIT_PATH" && -f "$stage/backup/was-enabled" ]]; then
    systemctl enable --quiet "$UNIT"
  else
    systemctl disable --quiet "$UNIT" 2>/dev/null || true
  fi
  if [[ -f "$UNIT_PATH" && -f "$stage/backup/was-active" ]]; then
    systemctl restart "$UNIT"
  else
    systemctl stop "$UNIT" 2>/dev/null || true
  fi
  rm -f -- "$stage/committed"
  printf '__MONITOR_ROLLBACK__ ok\n'
}

command_disable() {
  if [[ -f "$UNIT_PATH" ]]; then
    systemctl disable --now --quiet "$UNIT" 2>/dev/null || systemctl stop "$UNIT" 2>/dev/null || true
    printf '__MONITOR_DISABLE__ stopped\n'
  else
    printf '__MONITOR_DISABLE__ absent\n'
  fi
}

main() {
  local action="${1:-}"
  shift || true
  case "$action" in
    install) (($# == 3)) || fail "usage: install STAGE ROLE PROGRAM"; command_install "$@" ;;
    rollback) (($# == 2)) || fail "usage: rollback STAGE PROGRAM"; command_rollback "$@" ;;
    cleanup)
      (($# == 1)) || fail "usage: cleanup STAGE"
      safe_stage "$1"
      rm -rf -- "$1"
      ;;
    disable) command_disable ;;
    installed)
      if [[ -f "$UNIT_PATH" ]]; then printf 'yes\n'; else printf 'no\n'; fi
      ;;
    *) fail "unknown action: ${action:-none}" ;;
  esac
}

main "$@"
