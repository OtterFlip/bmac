#!/usr/bin/env bash

# Copyright (c) 2026 BEENTHERE VENTURES, INC.
# SPDX-License-Identifier: GPL-3.0-only

# Standalone and read-only. Copy this single file to a Linux Live environment
# booted on a machine that will become a Proxmox host, and run it there. It
# lists every NVMe drive's serial number and exact byte capacity and every
# physical Ethernet NIC's MAC address: the values env/moxN.conf needs before
# hosts/add_proxmox_host.sh runs. It depends on nothing else in this repo.

set -Eeuo pipefail

SYS="${CLUSTER_SETUP_PREREQ_SYS:-/sys}"

describe() {
  cat <<'EOF'
CLUSTER SETUP PREREQUISITES: DRIVE AND NIC VALUES FOR env/moxN.conf

Run this in a Linux Live environment booted on a machine you intend to set up
as a Proxmox host (repeat on each such machine). It only reads hardware
information and changes nothing on this machine.

It lists:
  - every NVMe drive's serial number and exact byte capacity
  - every physical Ethernet NIC's MAC address and link state

Copy these values into that host's env/moxN.conf on your administrator
workstation before running hosts/add_proxmox_host.sh:
  - For each rpool mirror (mirror 0 is mandatory and becomes the boot mirror),
    pick two NVMe drives whose capacities differ by at most 1% and set
    NVME_MIRROR_<P>_SERIAL_<M> and NVME_MIRROR_<P>_CAPACITY_BYTES_<M>.
    Setup erases both mirror-0 drives.
  - Set PROXMOX_PUBLIC_MAC to the NIC cabled to the public network and
    PROXMOX_SECONDARY_MAC to the NIC cabled to the private cluster network.
    The LINK column shows which ports have a cable and link.

iDRAC ALTERNATIVE: if you will run setup with iDRAC/Redfish, you do not need
this script or a Live environment. You must still set the drive serial numbers
and NIC MAC addresses in env/moxN.conf, and you can read both in the iDRAC
admin web UI. The CAPACITY_BYTES values are not needed in that case; setup
reads the capacities through Redfish.  In the iDRAC admin web UI the NIC
MAC address values you should look for are under the port tab with the
field label "Virtual MAC Addresses".
EOF
}

case "${1:-}" in
  "") ;;
  -h | --help)
    describe
    exit 0
    ;;
  *)
    printf 'Usage: %s\n' "$0" >&2
    exit 2
    ;;
esac

command -v lsblk >/dev/null 2>&1 || {
  printf 'ERROR: lsblk (util-linux) is required.\n' >&2
  exit 1
}

describe
printf '\n'
if [[ -t 0 ]]; then
  read -r -p "Press ENTER to list this machine's drives and NICs (Ctrl-C to quit): " _ || exit 1
  printf '\n'
fi

# print_table HEADER... -- ROW_FIELD...: left-aligned columns; the last column
# is printed as-is so it may contain spaces.
print_table() {
  local -a headers=() cells=()
  while (($#)) && [[ "$1" != -- ]]; do
    headers+=("$1")
    shift
  done
  shift
  cells=("$@")
  local columns=${#headers[@]} index width
  local -a widths=()
  for ((index = 0; index < columns; index++)); do
    widths[index]=${#headers[index]}
  done
  for ((index = 0; index < ${#cells[@]}; index++)); do
    width=${#cells[index]}
    ((width <= widths[index % columns])) || widths[index % columns]=$width
  done
  local -a row
  for ((index = 0; index <= ${#cells[@]}; index += columns)); do
    if ((index == 0)); then
      row=("${headers[@]}")
    else
      row=("${cells[@]:index-columns:columns}")
    fi
    local line="" column
    for ((column = 0; column < columns - 1; column++)); do
      printf -v line '%s%-*s  ' "$line" "${widths[column]}" "${row[column]}"
    done
    printf '%s%s\n' "$line" "${row[columns - 1]}"
  done
}

trim() {
  local value="$*"
  value="${value#"${value%%[![:space:]]*}"}"
  printf '%s' "${value%"${value##*[![:space:]]}"}"
}

printf 'NVMe drives:\n'
nvme=()
for path in "$SYS"/block/nvme*; do
  name="${path##*/}"
  # Namespaces only; nvmeXcYnZ entries are hidden multipath paths.
  [[ "$name" =~ ^nvme[0-9]+n[0-9]+$ ]] || continue
  device="/dev/$name"
  serial="$(trim "$(lsblk -dno SERIAL "$device" 2>/dev/null || true)")"
  bytes="$(trim "$(lsblk -dnbo SIZE "$device" 2>/dev/null || true)")"
  model="$(trim "$(lsblk -dno MODEL "$device" 2>/dev/null || true)")"
  if [[ -z "$serial" && -r "$path/device/serial" ]]; then
    serial="$(trim "$(<"$path/device/serial")")"
  fi
  if [[ -z "$model" && -r "$path/device/model" ]]; then
    model="$(trim "$(<"$path/device/model")")"
  fi
  if [[ ! "$bytes" =~ ^[0-9]+$ && -r "$path/size" ]]; then
    bytes=$(($(<"$path/size") * 512))
  fi
  nvme+=("$device" "${serial:-?}" "${bytes:-?}" "${model:--}")
done
if ((${#nvme[@]})); then
  print_table DEVICE SERIAL CAPACITY_BYTES MODEL -- "${nvme[@]}"
else
  printf '  none found\n'
fi

printf '\nNetwork interfaces (physical Ethernet):\n'
nics=()
for path in "$SYS"/class/net/*; do
  name="${path##*/}"
  # Physical NICs have a backing device; bridges, VLANs, and loopback do not.
  [[ -e "$path/device" && "$(<"$path/type")" == 1 ]] || continue
  [[ ! -e "$path/wireless" && ! -e "$path/phy80211" ]] || continue
  mac=""
  if command -v ethtool >/dev/null 2>&1; then
    mac="$(ethtool -P "$name" 2>/dev/null | sed -n 's/^Permanent address: *//p' || true)"
  fi
  [[ "$mac" =~ ^([0-9a-fA-F]{2}:){5}[0-9a-fA-F]{2}$ && "$mac" != 00:00:00:00:00:00 ]] ||
    mac="$(<"$path/address")"
  carrier="$(cat "$path/carrier" 2>/dev/null || true)"
  case "$carrier" in
    1) link=up ;;
    0) link=no-cable-or-link ;;
    *) link="$(<"$path/operstate")" ;;
  esac
  nics+=("$name" "${mac^^}" "$link")
done
if ((${#nics[@]})); then
  print_table INTERFACE MAC_ADDRESS LINK -- "${nics[@]}"
else
  printf '  none found\n'
fi
