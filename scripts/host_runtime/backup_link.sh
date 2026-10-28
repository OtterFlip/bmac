#!/usr/bin/env bash

# Copyright (c) 2026 BEENTHERE VENTURES, INC.
# SPDX-License-Identifier: GPL-3.0-only

# The read-only PBS mapping behind a backup-linked staging VM, on its host.
#
#   backup_link.sh map VMID SNAPSHOT [--storage ID]
#                              map the restore point's scsi0 and print the
#                              stable path an overlay uses as its backing file
#   backup_link.sh status VMID print alive, dead, or absent
#   backup_link.sh create-overlay VMID VOLUME
#                              create the VM's qcow2 overlay over its mapping
#   backup_link.sh restore VMID ARCHIVE DEVICE [--skip-zero]
#                              write one image of the VM's restore point (such
#                              as drive-scsi0.img.fidx) to a /dev/zvol device
#   backup_link.sh unmap VMID  remove the mapping, idempotently
#   backup_link.sh forget VMID forget the VM's restore point (once its
#                              overlay is gone)
#
# proxmox-backup-client map presents the disk image through FUSE on a loop
# device, and serves it for as long as its process lives. That process runs
# in the systemd scope bmac-backup-link-VMID.scope, so the mapping is alive
# exactly while the scope is active. /run/bmac-backup-link/VMID.img points at
# the loop device and is the overlay's backing file, so the overlay never
# names a loop number. /run is emptied by a reboot, which ends every mapping;
# the deferred-cleanup worker then evicts the VMs that used them.
#
# The restore point an overlay was made over is also recorded on disk
# (/var/lib/app-ha/backup-link/VMID.snapshot), so a later map, after a dead
# mapping or a reboot, can only ever put the same restore point under it.
#
# The connection settings and the token come from the PVE PBS storage, as
# for pbs_api.py; the token is passed in the environment, never on a command
# line.

set -Eeuo pipefail
set +x
umask 077

TEST_MODE="${APP_HA_BACKUP_LINK_TEST_MODE:-0}"
if [[ "$TEST_MODE" == 1 ]]; then
  LINK_DIR="${APP_HA_BACKUP_LINK_DIR:?APP_HA_BACKUP_LINK_DIR is required in test mode}"
  RECORD_DIR="${APP_HA_BACKUP_LINK_RECORD_DIR:?APP_HA_BACKUP_LINK_RECORD_DIR is required in test mode}"
  ZVOL_DIR="${APP_HA_BACKUP_LINK_ZVOL_DIR:?APP_HA_BACKUP_LINK_ZVOL_DIR is required in test mode}"
  STORAGE_CFG="${APP_HA_STORAGE_CFG:?APP_HA_STORAGE_CFG is required in test mode}"
  STORAGE_PRIV="${APP_HA_STORAGE_PRIV:?APP_HA_STORAGE_PRIV is required in test mode}"
else
  LINK_DIR=/run/bmac-backup-link
  RECORD_DIR=/var/lib/app-ha/backup-link
  ZVOL_DIR=/dev/zvol
  STORAGE_CFG=/etc/pve/storage.cfg
  STORAGE_PRIV=/etc/pve/priv/storage
fi
SNAPSHOT_RE='^vm/[1-9][0-9]{0,8}/[0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9]{2}:[0-9]{2}:[0-9]{2}Z$'

die() {
  printf 'backup_link.sh: %s\n' "$*" >&2
  exit 1
}

usage() {
  sed -n '7,21p' "$0" | sed 's/^# \{0,1\}//'
}

require_vmid() {
  [[ "${1:-}" =~ ^[1-9][0-9]{0,8}$ ]] || die "VMID must be a positive number"
}

# A loop device; in test mode also a fake one under LINK_DIR.
is_block() {
  [[ -b "$1" ]] || [[ "$TEST_MODE" == 1 && -f "$1" && ("$1" == "${LINK_DIR}"/fake-loop* || "$1" == "${ZVOL_DIR}"/*) ]]
}

scope_unit() {
  printf 'bmac-backup-link-%s.scope\n' "$1"
}

link_path() {
  printf '%s/%s.img\n' "$LINK_DIR" "$1"
}

# The PBS storage's connection settings, one per line: repository,
# namespace, fingerprint.
storage_settings() {
  python3 - "$STORAGE_CFG" "$1" <<'PY'
import re
import sys

section, values = None, {}
for line in open(sys.argv[1], encoding="utf-8"):
    header = re.fullmatch(r"(\S+):\s*(\S+)\s*", line.rstrip("\n"))
    if header:
        if section is not None:
            break
        if header.groups() == ("pbs", sys.argv[2]):
            section = True
        continue
    if section and line.strip():
        key, _, value = line.strip().partition(" ")
        values[key] = value.strip()
if section is None:
    raise SystemExit(f"{sys.argv[2]} is not a PBS storage")
for key in ("server", "datastore", "username", "fingerprint"):
    if not values.get(key):
        raise SystemExit(f"storage {sys.argv[2]} has no {key}")
print(f"{values['username']}@{values['server']}:{values.get('port', '8007')}:{values['datastore']}")
print(values.get("namespace", ""))
print(values["fingerprint"])
PY
}

# The loop device that maps SNAPSHOT's scsi0 at LOOP, if it is that mapping.
# proxmox-backup-client unmap without arguments lists "/dev/loopN:\tREPO:SNAPSHOT/drive-scsi0.img".
loop_maps_snapshot() {
  local loop="$1" snapshot="$2"
  proxmox-backup-client unmap 2>&1 </dev/null | grep -qF "${loop}:"$'\t' || return 1
  proxmox-backup-client unmap 2>&1 </dev/null | grep -F "${loop}:"$'\t' | grep -qF ":${snapshot}/drive-scsi0.img"
}

state_of() {
  local vmid="$1" link loop
  link="$(link_path "$vmid")"
  if ! systemctl is-active --quiet "$(scope_unit "$vmid")"; then
    [[ -L "$link" ]] && printf 'dead\n' || printf 'absent\n'
    return 0
  fi
  loop="$(readlink "$link" 2>/dev/null)" || loop=""
  if [[ -n "$loop" ]] && is_block "$loop"; then
    printf 'alive\n'
  else
    printf 'dead\n'
  fi
}

command_map() {
  local vmid="$1" snapshot="$2" storage="bmac-pbs" link output loop
  shift 2
  while (($#)); do
    case "$1" in
      --storage)
        (($# >= 2)) || die "--storage needs a value"
        storage="$2"
        shift 2
        ;;
      *) die "unknown option: $1" ;;
    esac
  done
  require_vmid "$vmid"
  [[ "$snapshot" =~ $SNAPSHOT_RE ]] || die "SNAPSHOT must look like vm/100/2026-10-27T14:50:01Z"
  [[ "$storage" =~ ^[A-Za-z][A-Za-z0-9_.-]{0,63}$ ]] || die "invalid storage ID"
  link="$(link_path "$vmid")"
  install -d -m 0700 "$LINK_DIR" "$RECORD_DIR"
  local recorded
  recorded="$(cat "${RECORD_DIR}/${vmid}.snapshot" 2>/dev/null)" || recorded=""
  [[ -z "$recorded" || "$recorded" == "${storage} ${snapshot}" ]] ||
    die "VM $vmid's overlay was made over ${recorded#* }, not $snapshot"

  case "$(state_of "$vmid")" in
    alive)
      [[ "$(cat "${LINK_DIR}/${vmid}.snapshot" 2>/dev/null)" == "$snapshot" ]] ||
        die "VM $vmid already has a mapping of another restore point"
      printf '%s\n' "$link"
      return 0
      ;;
    dead) command_unmap "$vmid" ;;
  esac

  local -a settings=()
  mapfile -t settings < <(storage_settings "$storage") || die "cannot read storage $storage"
  ((${#settings[@]} == 3)) || die "cannot read storage $storage"
  local key="${STORAGE_PRIV}/${storage}.enc"
  local -a arguments=(map "$snapshot" drive-scsi0.img)
  [[ -z "${settings[1]}" ]] || arguments+=(--ns "${settings[1]}")
  [[ ! -f "$key" ]] || arguments+=(--keyfile "$key")
  PBS_PASSWORD="$(cat "${STORAGE_PRIV}/${storage}.pw")" || die "cannot read the token of storage $storage"
  export PBS_PASSWORD PBS_REPOSITORY="${settings[0]}" PBS_FINGERPRINT="${settings[2]}"
  # The client daemonizes once mapped; the scope keeps its daemon and ends
  # when it dies, which is how a dead mapping is recognized.
  output="$(systemd-run --scope --quiet --collect --unit "$(scope_unit "$vmid")" \
    --description "BMAC backup-linked staging VM ${vmid}: ${snapshot}" \
    proxmox-backup-client "${arguments[@]}" 2>&1 </dev/null)" ||
    die "mapping ${snapshot} failed: $(tr '\n' ' ' <<<"$output")"
  unset PBS_PASSWORD
  loop="$(grep -oE '(/dev/loop|'"${LINK_DIR}"'/fake-loop)[0-9]+' <<<"$output" | head -n 1)"
  [[ -n "$loop" ]] && is_block "$loop" || die "mapping ${snapshot} produced no loop device: $(tr '\n' ' ' <<<"$output")"
  printf '%s\n' "$snapshot" >"${LINK_DIR}/${vmid}.snapshot"
  printf '%s %s\n' "$storage" "$snapshot" >"${RECORD_DIR}/${vmid}.snapshot"
  ln -sfn "$loop" "$link"
  printf '%s\n' "$link"
}

# Write one image of the VM's recorded restore point to a zvol with
# pbs-restore, the tool Proxmox's own restores use. With --skip-zero it
# skips the zero chunks by their digest, without fetching or writing them;
# that is only correct on a device that already reads as zeros.
command_restore() {
  local vmid="$1" archive="$2" device="$3" skip_zero="${4:-}" recorded storage snapshot
  require_vmid "$vmid"
  [[ "$archive" =~ ^drive-[a-z]+[0-9]+\.img\.fidx$ ]] || die "ARCHIVE must look like drive-scsi0.img.fidx"
  [[ "${device#"${ZVOL_DIR}/"}" =~ ^[A-Za-z0-9._+:-]+(/[A-Za-z0-9._+:-]+)+$ && "$device" != *..* ]] &&
    is_block "$device" || die "DEVICE must be a /dev/zvol block device"
  [[ -z "$skip_zero" || "$skip_zero" == --skip-zero ]] || die "unknown option: $skip_zero"
  recorded="$(cat "${RECORD_DIR}/${vmid}.snapshot" 2>/dev/null)" || die "VM $vmid has no recorded restore point"
  storage="${recorded%% *}"
  snapshot="${recorded#* }"
  local -a settings=()
  mapfile -t settings < <(storage_settings "$storage") || die "cannot read storage $storage"
  ((${#settings[@]} == 3)) || die "cannot read storage $storage"
  local key="${STORAGE_PRIV}/${storage}.enc"
  local -a arguments=(--repository "${settings[0]}")
  [[ -z "${settings[1]}" ]] || arguments+=(--ns "${settings[1]}")
  arguments+=("$snapshot" "$archive" "$device" --format raw)
  [[ ! -f "$key" ]] || arguments+=(--keyfile "$key")
  [[ -z "$skip_zero" ]] || arguments+=(--skip-zero)
  PBS_PASSWORD="$(cat "${STORAGE_PRIV}/${storage}.pw")" || die "cannot read the token of storage $storage"
  export PBS_PASSWORD PBS_FINGERPRINT="${settings[2]}"
  pbs-restore "${arguments[@]}" </dev/null >&2 || die "pbs-restore of $archive from $snapshot failed"
}

command_create_overlay() {
  local vmid="$1" volume="$2" path link
  require_vmid "$vmid"
  [[ "$volume" =~ ^[A-Za-z0-9][A-Za-z0-9_.+-]{0,63}:${vmid}/vm-${vmid}-disk-[0-9]+\.qcow2$ ]] ||
    die "VOLUME must be STORAGE:${vmid}/vm-${vmid}-disk-N.qcow2"
  [[ "$(state_of "$vmid")" == alive ]] || die "VM $vmid has no live mapping"
  link="$(link_path "$vmid")"
  path="$(pvesm path "$volume")" || die "cannot resolve $volume"
  [[ "$path" == /* && ! -e "$path" ]] || die "$path already exists"
  install -d -m 0750 "$(dirname -- "$path")"
  qemu-img create -q -f qcow2 -F raw -b "$link" "$path" >/dev/null || die "could not create $path"
  printf '%s\n' "$path"
}

command_unmap() {
  local vmid="$1" link loop snapshot
  require_vmid "$vmid"
  link="$(link_path "$vmid")"
  snapshot="$(cat "${LINK_DIR}/${vmid}.snapshot" 2>/dev/null)" || snapshot=""
  loop="$(readlink "$link" 2>/dev/null)" || loop=""
  # Only the loop device that still maps this VM's restore point: after a
  # dead mapping its number may already belong to another one.
  if [[ -n "$loop" && -n "$snapshot" ]] && is_block "$loop" && loop_maps_snapshot "$loop" "$snapshot"; then
    proxmox-backup-client unmap "$loop" </dev/null >/dev/null 2>&1 || true
  fi
  systemctl stop "$(scope_unit "$vmid")" >/dev/null 2>&1 || true
  # Listing the mappings also removes those whose process died.
  proxmox-backup-client unmap </dev/null >/dev/null 2>&1 || true
  rm -f -- "$link" "${LINK_DIR}/${vmid}.snapshot"
  [[ "$(state_of "$vmid")" == absent ]] || die "the mapping of VM $vmid is still present"
}

command_forget() {
  require_vmid "$1"
  [[ "$(state_of "$1")" == absent ]] || die "VM $1 still has a mapping; unmap it first"
  rm -f -- "${RECORD_DIR}/${1}.snapshot"
}

main() {
  local command="${1:-}"
  shift || true
  case "$command" in
    map) (($# >= 2)) || die "map needs VMID SNAPSHOT"; command_map "$@" ;;
    status) (($# == 1)) || die "status needs VMID"; require_vmid "$1"; state_of "$1" ;;
    create-overlay) (($# == 2)) || die "create-overlay needs VMID VOLUME"; command_create_overlay "$@" ;;
    unmap) (($# == 1)) || die "unmap needs VMID"; command_unmap "$1" ;;
    restore) (($# == 3 || $# == 4)) || die "restore needs VMID ARCHIVE DEVICE [--skip-zero]"; command_restore "$@" ;;
    forget) (($# == 1)) || die "forget needs VMID"; command_forget "$1" ;;
    -h | --help) usage ;;
    *) usage >&2; exit 2 ;;
  esac
}

if [[ "${BASH_SOURCE[0]}" == "$0" ]]; then
  main "$@"
fi
