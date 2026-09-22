#!/usr/bin/env bash

# Copyright (c) 2026 BEENTHERE VENTURES, INC.
# SPDX-License-Identifier: GPL-3.0-only

# Read-only disk inventory for a Live Linux environment. This script does not
# mount, partition, format, wipe, or otherwise modify any device.

set -Eeuo pipefail
set +x

fail() {
  printf 'ERROR: %s\n' "$*" >&2
  exit 1
}

trim() {
  local value=$1
  value="${value#"${value%%[![:space:]]*}"}"
  value="${value%"${value##*[![:space:]]}"}"
  printf '%s' "$value"
}

field() {
  local device=$1 column=$2 value
  if [[ "$column" == SIZE ]]; then
    value="$(lsblk -dnb -o "$column" "$device" 2>/dev/null || true)"
  else
    value="$(lsblk -dn -o "$column" "$device" 2>/dev/null || true)"
  fi
  trim "$value"
}

command -v lsblk >/dev/null 2>&1 || fail "lsblk is required"

printf 'READ-ONLY DISK INVENTORY\n'
printf 'No disks are modified by this script.\n\n'
printf 'For each ZFS mirror, choose two entries with:\n'
printf '  - Class: NVMe SSD\n'
printf '  - the same Capacity bytes value\n'
printf '  - a non-empty, unique Serial value\n'
printf 'Record both values exactly; do not use the human-readable size.\n\n'

disk_count=0
eligible_count=0
declare -A eligible_serials=()
while read -r device device_type; do
  [[ "$device_type" == disk ]] || continue
  disk_count=$((disk_count + 1))

  size_bytes="$(field "$device" SIZE)"
  serial="$(field "$device" SERIAL)"
  model="$(field "$device" MODEL)"
  vendor="$(field "$device" VENDOR)"
  transport="$(field "$device" TRAN)"
  rotational="$(field "$device" ROTA)"

  if [[ -z "$serial" ]] && command -v udevadm >/dev/null 2>&1; then
    serial="$(
      udevadm info --query=property --name "$device" 2>/dev/null |
        awk -F= '$1 == "ID_SERIAL_SHORT" { sub(/^[^=]*=/, ""); print; exit }' ||
        true
    )"
  fi

  disk_class="Other/unknown"
  if [[ "${transport,,}" == nvme || "$device" == /dev/nvme*n* ]]; then
    if [[ "$rotational" == 0 ]]; then
      disk_class="NVMe SSD"
    else
      disk_class="NVMe (media type unknown)"
    fi
  elif [[ "$rotational" == 0 ]]; then
    disk_class="Non-NVMe SSD"
  elif [[ "$rotational" == 1 ]]; then
    disk_class="Rotational disk"
  fi

  if [[ "$disk_class" == "NVMe SSD" && -n "$serial" &&
    "$size_bytes" =~ ^[1-9][0-9]*$ &&
    -z "${eligible_serials[$serial]+x}" ]]; then
    eligible_serials["$serial"]=1
    eligible_count=$((eligible_count + 1))
  fi

  printf 'Disk %d\n' "$disk_count"
  printf '  Device:         %s\n' "$device"
  printf '  Class:          %s\n' "$disk_class"
  printf '  Capacity bytes: %s\n' "${size_bytes:-<unavailable>}"
  printf '  Serial:         %s\n' "${serial:-<unavailable>}"
  printf '  Vendor:         %s\n' "${vendor:-<unavailable>}"
  printf '  Model:          %s\n' "${model:-<unavailable>}"
  printf '  Transport:      %s\n' "${transport:-<unavailable>}"
  if [[ "$disk_class" == "NVMe SSD" && -n "$serial" && "$size_bytes" =~ ^[1-9][0-9]*$ ]]; then
    printf '  Config values:\n'
    printf '    NVME_MIRROR_N_SERIAL_M=%s\n' "$serial"
    printf '    NVME_MIRROR_N_CAPACITY_BYTES_M=%s\n' "$size_bytes"
  else
    printf '  Config values:  not eligible as a configured NVMe mirror member\n'
  fi
  printf '\n'
done < <(lsblk -dn -o PATH,TYPE)

((disk_count > 0)) || fail "No whole-disk block devices were found"
((eligible_count >= 2)) ||
  fail "Fewer than two distinct, fully inventoried NVMe SSDs were found"

printf 'Replace N with the mirror number and M with member 1 or 2 in env/moxN.conf.\n'
printf 'Every configured pair must have identical Class and Capacity bytes values.\n'
