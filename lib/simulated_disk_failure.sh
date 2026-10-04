#!/usr/bin/env bash

# Copyright (c) 2026 BEENTHERE VENTURES, INC.
# SPDX-License-Identifier: GPL-3.0-only

# Root-only host side of hosts/simulate_disk_failure_and_replacement.sh, piped
# to the host per run as: bash -s -- COMMAND MARKER [ARGS].
#
# It makes one member of a healthy two-way rpool mirror look like a disk that
# was pulled and replaced with a blank one, so hosts/add_replacement_disk.sh
# can be tested without physically swapping a disk. MARKER is a host file that
# records the simulation between steps so a rerun can finish or back it out.
#
#   status  MARKER               print the pending simulation, if any
#   offline MARKER VDEV SERIAL   take SERIAL's member of VDEV offline and close
#                                its LUKS mapping (reversible)
#   restore MARKER               bring the offlined member back (exit 3 when
#                                its LUKS mapping must be reopened at the console)
#   wipe    MARKER               erase the offlined disk so it is blank
#                                (irreversible)

set -Eeuo pipefail
set +x
umask 077

POOL=rpool
BOOT_UUIDS=/etc/kernel/proxmox-boot-uuids
# Room for the two ZFS labels at each end of a vdev (2 x 256 KiB), with slack
# for the alignment ZFS applies to the device size.
LABEL_ZERO_MIB=4

die() {
  printf 'ERROR: %s\n' "$*" >&2
  exit 1
}

canonical() {
  readlink -m -- "$1"
}

validate_serial() {
  [[ "$1" =~ ^[A-Za-z0-9._:+-]+$ ]] || die "unsafe disk serial: $1"
}

disk_by_serial() {
  local -a matches=()
  mapfile -t matches < <(
    lsblk -dnpo PATH,SERIAL | awk -v serial="$1" '$2 == serial { print $1 }'
  )
  ((${#matches[@]} == 1)) ||
    die "expected exactly one disk with serial $1; found ${#matches[@]}"
  printf '%s\n' "${matches[0]}"
}

# Canonical paths of DISK, its partitions, and the mappings on them.
disk_device_set() {
  local device
  while IFS= read -r device; do
    [[ -z "$device" ]] || canonical "$device"
  done < <(lsblk -nrpo NAME "$1")
}

on_device_set() {
  grep -Fxq -- "$(canonical "$1")" <<<"$2"
}

partition_path() {
  if [[ "$1" =~ [0-9]$ ]]; then
    printf '%sp%s\n' "$1" "$2"
  else
    printf '%s%s\n' "$1" "$2"
  fi
}

# Print "top parent leaf state" for every leaf of the pool's data vdevs.
# parent is the leaf's mirror-N or replacing-N, or "-" for a one-disk vdev.
pool_rows() {
  zpool status -P "$POOL" | awk -v pool="$POOL" '
    /^config:/ { in_config = 1; next }
    in_config && /^errors:/ { exit }
    in_config {
      line = $0
      sub(/^\t/, "", line)
      if (line !~ /[^ \t]/) next
      match(line, /^ */)
      indent = RLENGTH
      split(substr(line, indent + 1), field, /[ \t]+/)
      if (field[1] == "NAME") next
      if (indent == 0) { data = (field[1] == pool); next }
      if (!data) next
      if (indent == 2) top = field[1]
      if (field[1] ~ /^(mirror|replacing|spare|raidz[0-9]*|draid[0-9a-z:]*)-[0-9]+$/) {
        parent_at[indent] = field[1]
        next
      }
      print top, (indent == 2 ? "-" : parent_at[indent - 2]), field[1], field[2]
    }
  '
}

leaf_state() {
  pool_rows | awk -v leaf="$1" '$3 == leaf { print $4; exit }'
}

# Leaves of every imported pool, as "leaf state".
all_pool_leaves() {
  zpool status -P | awk '$1 ~ /^\// { print $1, $2 }'
}

mapper_active() {
  cryptsetup status "$1" >/dev/null 2>&1
}

# Print the partition number of a partition device, or nothing for a disk.
partition_number() {
  local name
  name="$(basename -- "$(canonical "$1")")"
  if [[ -r "/sys/class/block/${name}/partition" ]]; then
    cat "/sys/class/block/${name}/partition"
  fi
}

# The device that holds the member's ZFS label or LUKS header: the disk itself
# ("disk") or one of its partitions ("partN").
target_path() {
  local disk="$1" relative="$2"
  if [[ "$relative" == disk ]]; then
    printf '%s\n' "$disk"
  else
    partition_path "$disk" "${relative#part}"
  fi
}

disk_is_boot() {
  local uuid
  [[ -f "$BOOT_UUIDS" ]] || return 1
  while IFS= read -r uuid; do
    [[ -n "$uuid" ]] || continue
    awk -v uuid="$uuid" 'toupper($1) == toupper(uuid) { found=1 } END { exit !found }' \
      "$BOOT_UUIDS" && return 0
  done < <(lsblk -nro UUID "$1")
  return 1
}

# Refuse when anything other than this mirror member uses DISK: a mount, swap,
# a device-mapper or md holder, or a leaf of an imported pool that is not
# ALLOWED_LEAF (pass ALLOWED_STATE to require that leaf's state).
require_disk_unused() {
  local disk="$1" allowed_leaf="$2" allowed_mapper="$3" devices name type swap leaf state
  devices="$(disk_device_set "$disk")"
  while read -r name type; do
    [[ -n "$name" ]] || continue
    case "$type" in
      disk | part) ;;
      crypt)
        [[ -n "$allowed_mapper" && "$name" == "/dev/mapper/$allowed_mapper" ]] ||
          die "disk $disk holds $name ($type), which is not this mirror member"
        ;;
      *) die "disk $disk holds $name ($type), which is not this mirror member" ;;
    esac
  done < <(lsblk -nrpo NAME,TYPE "$disk")
  if lsblk -nrpo MOUNTPOINTS "$disk" | awk 'NF { found=1 } END { exit !found }'; then
    die "disk $disk has a mounted filesystem or active swap"
  fi
  while read -r swap _; do
    [[ "$swap" == /* ]] || continue
    ! on_device_set "$swap" "$devices" || die "disk $disk holds active swap $swap"
  done </proc/swaps
  while read -r leaf state; do
    [[ -n "$leaf" ]] || continue
    on_device_set "$leaf" "$devices" || continue
    [[ "$leaf" == "$allowed_leaf" ]] ||
      die "disk $disk holds $leaf ($state), which is not the chosen mirror member"
  done < <(all_pool_leaves)
}

# The survivor must stay ONLINE in VDEV for the whole simulation; without it
# the mirror has no copy of its data.
require_survivor_online() {
  local vdev="$1" other="$2" state
  state="$(pool_rows | awk -v vdev="$vdev" -v leaf="$other" '$1 == vdev && $3 == leaf { print $4; exit }')"
  [[ "$state" == ONLINE ]] ||
    die "the surviving member $other of $vdev is ${state:-not in rpool}, not ONLINE; refusing to touch the other member"
}

# Marker fields, tab-separated on one line.
MARKER_FIELDS=(SERIAL VDEV LEAF MAPPER TARGET ROLE OTHER)

read_marker() {
  local marker="$1" line
  [[ -f "$marker" ]] || return 1
  line="$(head -n 1 -- "$marker")"
  IFS=$'\t' read -r "${MARKER_FIELDS[@]}" <<<"$line"
  [[ -n "$SERIAL" && -n "$VDEV" && -n "$LEAF" && -n "$MAPPER" && -n "$TARGET" &&
    -n "$ROLE" && -n "$OTHER" ]] || die "$marker is malformed; inspect it and remove it by hand"
  validate_serial "$SERIAL"
}

write_marker() {
  local marker="$1" temporary
  temporary="$(mktemp "${marker}.XXXXXX")"
  printf '%s\t%s\t%s\t%s\t%s\t%s\t%s\n' \
    "$SERIAL" "$VDEV" "$LEAF" "$MAPPER" "$TARGET" "$ROLE" "$OTHER" >"$temporary"
  chmod 0600 "$temporary"
  mv -f -- "$temporary" "$marker"
}

# Whether the offlined member could still come back: its ZFS label or LUKS
# header is still on its device.
restorable() {
  local disk target
  disk="$(disk_by_serial "$SERIAL" 2>/dev/null)" || return 1
  target="$(target_path "$disk" "$TARGET")"
  [[ -b "$target" ]] || return 1
  if [[ "$MAPPER" == - ]]; then
    [[ "$(blkid -p -o value -s TYPE "$target" 2>/dev/null)" == zfs_member ]]
  else
    cryptsetup isLuks "$target" >/dev/null 2>&1
  fi
}

command_status() {
  local marker="$1" state
  read_marker "$marker" || return 0
  state="$(leaf_state "$LEAF")"
  if [[ "$state" == ONLINE ]]; then
    rm -f -- "$marker"
    printf 'RESTORED\t%s\t%s\t%s\n' "$SERIAL" "$VDEV" "$LEAF"
    return
  fi
  if [[ -z "$state" ]]; then
    # zpool replace already swapped the offlined member out of the mirror.
    rm -f -- "$marker"
    printf 'REPLACED\t%s\t%s\t%s\n' "$SERIAL" "$VDEV" "$LEAF"
    return
  fi
  local can_restore=no
  ! restorable || can_restore=yes
  printf 'PENDING\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\n' \
    "$SERIAL" "$VDEV" "$LEAF" "$MAPPER" "$ROLE" "$OTHER" "${state:-missing}" "$can_restore"
}

command_offline() {
  local marker="$1" disk devices rows top parent leaf state status
  local -a ours=() others=()
  VDEV="$2"
  SERIAL="$3"
  validate_serial "$SERIAL"
  [[ "$VDEV" =~ ^mirror-[0-9]+$ ]] || die "$VDEV is not a mirror vdev name"
  if read_marker "$marker"; then
    die "a simulated failure of disk $SERIAL in $VDEV is still pending ($marker); rerun the script to finish or back it out"
  fi

  status="$(zpool status -x "$POOL")"
  [[ "$status" == "pool '$POOL' is healthy" ]] ||
    die "$POOL is not healthy; simulate a failure only on a healthy pool:"$'\n'"$status"
  status="$(zpool status "$POOL")"
  ! grep -q 'resilver in progress' <<<"$status" || die "$POOL is resilvering"
  ! grep -q 'Evacuation of .* in progress' <<<"$status" ||
    die "a vdev removal is in progress on $POOL"

  disk="$(disk_by_serial "$SERIAL")"
  devices="$(disk_device_set "$disk")"
  rows="$(pool_rows)"
  while read -r top parent leaf state; do
    [[ "$top" == "$VDEV" && -n "$leaf" ]] || continue
    [[ "$parent" == "$VDEV" ]] || die "$VDEV has a $parent in progress"
    [[ "$state" == ONLINE ]] || die "member $leaf of $VDEV is $state"
    if on_device_set "$leaf" "$devices"; then
      ours+=("$leaf")
    else
      others+=("$leaf")
    fi
  done <<<"$rows"
  ((${#ours[@]} + ${#others[@]} == 2)) ||
    die "$VDEV has $((${#ours[@]} + ${#others[@]})) members; only two-way mirrors are supported"
  ((${#ours[@]} == 1)) || die "disk $SERIAL ($disk) is not a member of $VDEV"
  LEAF="${ours[0]}"
  OTHER="${others[0]}"

  MAPPER=-
  local holder="$LEAF"
  if [[ "$(lsblk -ndo TYPE "$LEAF")" == crypt ]]; then
    [[ "$LEAF" == /dev/mapper/crypt-rpool-* ]] ||
      die "$LEAF is not a BMAC rpool LUKS mapping"
    MAPPER="${LEAF#/dev/mapper/}"
    holder="$(cryptsetup status "$MAPPER" | awk '$1 == "device:" { print $2; exit }')"
    [[ -n "$holder" ]] || die "could not read the backing device of $MAPPER"
    on_device_set "$holder" "$devices" || die "$MAPPER is not backed by disk $disk"
  fi
  if [[ "$(canonical "$holder")" == "$(canonical "$disk")" ]]; then
    TARGET=disk
  else
    TARGET="part$(partition_number "$holder")"
    [[ "$TARGET" =~ ^part[0-9]+$ ]] || die "could not tell which partition of $disk holds $LEAF"
  fi
  [[ "$(canonical "$(target_path "$disk" "$TARGET")")" == "$(canonical "$holder")" ]] ||
    die "could not locate $holder on disk $disk"
  ROLE=extra
  ! disk_is_boot "$disk" || ROLE=boot
  require_disk_unused "$disk" "$LEAF" "${MAPPER#-}"

  write_marker "$marker"
  printf 'Taking %s (disk %s, %s) offline in %s.\n' "$LEAF" "$SERIAL" "$disk" "$VDEV"
  zpool offline "$POOL" "$LEAF"
  udevadm settle
  if [[ "$MAPPER" != - ]]; then
    local attempt
    for attempt in 1 2 3 4 5; do
      cryptsetup close "$MAPPER" && break
      ((attempt < 5)) || die "could not close $MAPPER; $LEAF is OFFLINE, so rerun the script to back out or finish"
      sleep 2
      udevadm settle
    done
    printf 'Closed LUKS mapping %s.\n' "$MAPPER"
  fi
  state="$(leaf_state "$LEAF")"
  [[ "$state" == OFFLINE ]] || die "$LEAF is ${state:-missing} after zpool offline, not OFFLINE"
  printf '\n'
  zpool status -P "$POOL"
}

command_restore() {
  local marker="$1" disk target state
  read_marker "$marker" || die "no simulated failure is pending"
  state="$(leaf_state "$LEAF")"
  if [[ "$state" == ONLINE ]]; then
    rm -f -- "$marker"
    printf '%s is already ONLINE in %s.\n' "$LEAF" "$VDEV"
    return
  fi
  [[ "$state" == OFFLINE ]] || die "$LEAF is ${state:-not in rpool}, not OFFLINE; it cannot be brought back"
  restorable || die "disk $SERIAL was already erased (or is missing); it can only be replaced now"
  disk="$(disk_by_serial "$SERIAL")"
  target="$(target_path "$disk" "$TARGET")"
  if [[ "$MAPPER" != - ]] && ! mapper_active "$MAPPER"; then
    printf 'CONSOLE\t%s\t%s\t%s\n' "$target" "$MAPPER" "$LEAF"
    exit 3
  fi
  zpool online "$POOL" "$LEAF"
  state="$(leaf_state "$LEAF")"
  [[ "$state" == ONLINE ]] || die "$LEAF is ${state:-missing} after zpool online"
  rm -f -- "$marker"
  printf '%s is ONLINE again; ZFS resilvers what changed while it was offline.\n\n' "$LEAF"
  zpool status -P "$POOL"
}

# Zero the first and last LABEL_ZERO_MIB MiB of DEVICE, where ZFS keeps its
# labels, so no stale rpool label survives on the blank disk.
zero_label_areas() {
  local device="$1" bytes mib
  bytes="$(blockdev --getsize64 "$device")"
  mib=$((bytes / 1048576))
  ((mib > 2 * LABEL_ZERO_MIB)) || return 0
  dd if=/dev/zero of="$device" bs=1M count="$LABEL_ZERO_MIB" oflag=direct conv=fsync status=none
  dd if=/dev/zero of="$device" bs=1M count="$LABEL_ZERO_MIB" seek=$((mib - LABEL_ZERO_MIB)) \
    oflag=direct conv=fsync status=none
}

command_wipe() {
  local marker="$1" disk state device
  local -a parts=()
  read_marker "$marker" || die "no simulated failure is pending; take a member offline first"
  disk="$(disk_by_serial "$SERIAL")"
  require_survivor_online "$VDEV" "$OTHER"
  state="$(leaf_state "$LEAF")"
  [[ "$state" == OFFLINE ]] ||
    die "$LEAF is ${state:-not in rpool}, not OFFLINE; refusing to erase disk $SERIAL"
  if [[ "$MAPPER" != - ]] && mapper_active "$MAPPER"; then
    cryptsetup close "$MAPPER" || die "could not close $MAPPER"
  fi
  require_disk_unused "$disk" "$LEAF" ""
  ! on_device_set "$OTHER" "$(disk_device_set "$disk")" ||
    die "the surviving member $OTHER is on disk $SERIAL"

  printf 'Erasing disk %s (%s).\n' "$SERIAL" "$disk"
  mapfile -t parts < <(lsblk -nrpo NAME,TYPE "$disk" | awk '$2 == "part" { print $1 }')
  for device in "${parts[@]}" "$disk"; do
    if cryptsetup isLuks "$device" >/dev/null 2>&1; then
      cryptsetup erase -q "$device"
      printf '  destroyed the LUKS key slots on %s\n' "$device"
    fi
    wipefs --all --force --quiet "$device"
    if [[ "$device" == "$disk" ]]; then
      # The checks after the wipe decide success; sgdisk complains about a
      # disk that has no GPT left.
      sgdisk --zap-all "$disk" >/dev/null 2>&1 || true
    fi
    zero_label_areas "$device"
  done
  if blkdiscard -f "$disk" 2>/dev/null; then
    printf '  discarded every block of %s\n' "$disk"
  else
    printf '  %s does not support discard; its data area was left in place\n' "$disk"
  fi
  partx --update "$disk" 2>/dev/null || blockdev --rereadpt "$disk" 2>/dev/null || true
  udevadm settle

  [[ "$(lsblk -nrpo NAME "$disk" | wc -l)" == 1 ]] ||
    die "disk $disk still shows partitions or holders after the wipe"
  if blkid -p "$disk" >/dev/null 2>&1; then
    die "disk $disk still carries a signature after the wipe"
  fi
  ! cryptsetup isLuks "$disk" >/dev/null 2>&1 || die "disk $disk still holds LUKS after the wipe"
  rm -f -- "$marker"
  printf '\nDisk %s is blank:\n' "$SERIAL"
  lsblk -o NAME,TYPE,FSTYPE,PTTYPE,SERIAL,SIZE "$disk"
  printf '\n'
  zpool status -P "$POOL"
}

main() {
  (($# >= 2)) || die "usage: COMMAND MARKER [ARGS]"
  [[ "$(id -u)" == 0 ]] || die "must run as root"
  local command="$1"
  shift
  case "$command" in
    status) (($# == 1)) || die "status takes MARKER"; command_status "$@" ;;
    offline) (($# == 3)) || die "offline takes MARKER VDEV SERIAL"; command_offline "$@" ;;
    restore) (($# == 1)) || die "restore takes MARKER"; command_restore "$@" ;;
    wipe) (($# == 1)) || die "wipe takes MARKER"; command_wipe "$@" ;;
    *) die "unknown command $command" ;;
  esac
}

main "$@"
