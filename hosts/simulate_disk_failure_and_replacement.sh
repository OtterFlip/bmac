#!/usr/bin/env bash

# Copyright (c) 2026 BEENTHERE VENTURES, INC.
# SPDX-License-Identifier: GPL-3.0-only

# Test aid for hosts/add_replacement_disk.sh. The data center cannot swap a
# disk on request, so this script makes one member of a healthy two-way rpool
# mirror look like a disk that was pulled and replaced with a blank one: it
# takes the member offline (closing its LUKS mapping on an encrypted host),
# then erases the disk so it shows as a blank, unused disk. The host-side
# logic is lib/simulated_disk_failure.sh, piped to the host per run.

set -Eeuo pipefail
set +x
umask 077

DW_SCRIPT_NAME=simulate_disk_failure_and_replacement.sh
SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
# shellcheck source=../lib/disk_workflows.sh
source "${SCRIPT_DIR}/../lib/disk_workflows.sh"
SIMULATOR="${DW_LIB_DIR}/simulated_disk_failure.sh"

usage() {
  cat <<'EOF'
Usage: hosts/simulate_disk_failure_and_replacement.sh [--host moxN]

TEST AID; DESTROYS THE DATA ON ONE DISK. Simulates a failed rpool mirror
member whose disk the data center replaced with a blank one, so that
hosts/add_replacement_disk.sh can be tested without a physical disk swap:

  1. lists the healthy two-way mirrors and lets you pick one, then one of its
     two members;
  2. takes that member offline and, on an encrypted host, closes its LUKS
     mapping (reversible: the script can bring it back);
  3. after a second confirmation, erases the disk: LUKS key slots, ZFS
     labels, signatures, and partition table, then discards every block, so
     the disk shows as blank and unused (irreversible);
  4. shows the inventory hosts/add_replacement_disk.sh will work from.

Works with and without LUKS. The pool must be healthy, and the mirror has no
redundancy from step 2 until the replacement finishes resilvering. A rerun
finishes or backs out a simulation that stopped between steps 2 and 3.
EOF
}

HOST_ARG=""
while (($#)); do
  case "$1" in
    --host)
      (($# >= 2)) || dw_die "--host requires moxN"
      HOST_ARG="$2"
      shift 2
      ;;
    -h | --help)
      usage
      exit 0
      ;;
    *)
      usage >&2
      exit 2
      ;;
  esac
done

dw_init
[[ -f "$SIMULATOR" && ! -L "$SIMULATOR" ]] ||
  dw_die "required library is unavailable: $SIMULATOR"

dw_section "Simulate a disk failure and replacement"
cat <<'EOF'
This is a test aid for hosts/add_replacement_disk.sh. It makes one member of a
healthy rpool mirror look like a disk that failed, was pulled, and was replaced
by the data center with a blank disk:

  - it takes the member you choose OFFLINE and, on an encrypted host, closes
    its LUKS mapping;
  - it then ERASES THAT DISK (LUKS key slots, ZFS labels, partition table, and
    a discard of every block). Everything on it is destroyed for good.

The mirror has NO REDUNDANCY from the moment the member goes offline until
hosts/add_replacement_disk.sh has resilvered the disk back in. Make sure your
backups are current, and do not run this on a host whose data you cannot lose.
EOF
printf '\n'
prompt_yes "This script performs destructive operations. Continue?" || {
  printf 'Nothing was changed.\n'
  exit 0
}

dw_select_host "$HOST_ARG"
MARKER="${DW_REMOTE_ROOT}/app-ha-simulated-disk-failure"

on_host_simulator() {
  mox_ssh "$DW_HOST" bash -s -- "$@" <"$SIMULATOR"
}

# Bring the offlined member back; on an encrypted host whose mapping is closed
# that takes the passphrase, which is only ever typed at the host console.
restore_member() {
  local output status=0 target mapper leaf
  output="$(on_host_simulator restore "$MARKER")" || status=$?
  if ((status == 3)); then
    IFS=$'\t' read -r _ target mapper leaf <<<"$output"
    printf '\nMANUAL LUKS ACTION REQUIRED\n'
    printf 'At the %s console (iDRAC or physical), log in as root and run:\n' "$DW_HOST"
    printf '  cryptsetup open %s %s && zpool online rpool %s\n' "$target" "$mapper" "$leaf"
    printf 'It asks for the shared rpool LUKS passphrase. Then rerun this script, which\n'
    printf 'sees the member back ONLINE and clears its record of the simulation.\n'
    return 0
  fi
  printf '%s\n' "$output"
  ((status == 0)) || dw_die "could not bring the member back; rerun this script to retry"
}

wipe_disk() {
  local serial="$1"
  dw_section "Erasing disk $serial on $DW_HOST"
  on_host_simulator wipe "$MARKER" ||
    dw_die "erasing disk $serial did not finish; rerun this script to finish it"
}

# Ask before the irreversible step; anything but GO offers to back out.
confirm_wipe() {
  local serial="$1" vdev="$2" answer
  printf '\nNEXT (IRREVERSIBLE): erase disk %s so it looks like a blank replacement disk.\n' "$serial"
  printf 'Its member of %s cannot be brought back after this.\n' "$vdev"
  printf 'Type GO to erase disk %s, or anything else to stop before erasing it.\n> ' "$serial"
  IFS= read -r answer || answer=""
  [[ "$answer" == GO ]] && return 0
  printf '\nDisk %s was not erased.\n' "$serial"
  if prompt_yes "Bring its member back into $vdev now?"; then
    restore_member
  else
    printf 'Its member stays OFFLINE and %s has no redundancy. Rerun this script to\n' "$vdev"
    printf 'erase the disk or to bring the member back.\n'
  fi
  return 1
}

print_next_steps() {
  local serial="$1" vdev="$2" mapper="$3" role="$4"
  dw_section "Disk inventory on $DW_HOST after the simulation"
  LAYOUT="${DW_RUN_DIR}/layout.json"
  dw_collect_layout "$LAYOUT"
  dw_render_inventory "$LAYOUT"

  dw_section "Next steps"
  printf 'The inventory above should show %s with one ONLINE member and one OFFLINE\n' "$vdev"
  printf 'member with no disk, and disk %s as blank, unused, and eligible for safe\n' "$serial"
  printf 'physical removal.\n\n'
  printf 'Run the replacement script and choose %s and disk %s:\n' "$vdev" "$serial"
  printf '  ./hosts/add_replacement_disk.sh --host %s\n\n' "$DW_HOST"
  printf 'Do not reboot %s until add_replacement_disk.sh has finished:\n' "$DW_HOST"
  if [[ "$mapper" != - ]]; then
    printf '  - /etc/crypttab and the initramfs still list the erased LUKS UUID of %s,\n' "$mapper"
    printf '    so boot would wait for it; add_replacement_disk.sh records the new one.\n'
  fi
  if [[ "$role" == boot ]]; then
    printf '  - the erased disk held a boot-mirror ESP that is still registered with\n'
    printf '    proxmox-boot-tool; add_replacement_disk.sh gives the disk a new ESP and\n'
    printf '    forgets the old one. Until then only the surviving disk can boot.\n'
  fi
  printf '  - %s has no redundancy until the replacement finishes resilvering.\n' "$vdev"
}

# A simulation that stopped between taking the member offline and erasing
# its disk is finished or backed out before anything new starts.
PENDING="$(on_host_simulator status "$MARKER")" ||
  dw_die "could not read the simulation record on $DW_HOST"
if [[ -n "$PENDING" ]]; then
  IFS=$'\t' read -r P_KIND P_SERIAL P_VDEV P_LEAF P_MAPPER P_ROLE P_OTHER P_STATE \
    P_RESTORABLE <<<"$PENDING"
  case "$P_KIND" in
    RESTORED)
      printf '\nAn earlier simulation of disk %s in %s was backed out: %s is ONLINE again.\n' \
        "$P_SERIAL" "$P_VDEV" "$P_LEAF"
      ;;
    REPLACED)
      printf '\nAn earlier simulation of disk %s in %s is finished: %s was replaced.\n' \
        "$P_SERIAL" "$P_VDEV" "$P_LEAF"
      ;;
    PENDING)
      dw_section "Simulation in progress on $DW_HOST"
      printf 'Disk %s of %s (%s) is %s; surviving member %s.\n' \
        "$P_SERIAL" "$P_VDEV" "$P_LEAF" "$P_STATE" "$P_OTHER"
      [[ "$P_STATE" == OFFLINE ]] ||
        dw_die "$P_LEAF is $P_STATE rather than OFFLINE; check zpool status -P rpool on $DW_HOST"
      if [[ "$P_RESTORABLE" == yes ]]; then
        printf 'The disk has not been erased yet.\n'
        printf '  1) erase disk %s now (irreversible)\n' "$P_SERIAL"
        printf '  2) bring its member back into %s\n' "$P_VDEV"
        IFS= read -r -p "Choose 1 or 2 (q to quit): " CHOICE || dw_die "input ended"
        case "$CHOICE" in
          1)
            confirm_wipe "$P_SERIAL" "$P_VDEV" || exit 0
            ;;
          2)
            restore_member
            exit 0
            ;;
          *)
            printf 'Nothing was changed.\n'
            exit 0
            ;;
        esac
      else
        printf 'An earlier run started erasing the disk; it cannot be brought back.\n'
        prompt_yes "Finish erasing disk $P_SERIAL now?" || {
          printf 'Nothing was changed.\n'
          exit 0
        }
      fi
      wipe_disk "$P_SERIAL"
      print_next_steps "$P_SERIAL" "$P_VDEV" "$P_MAPPER" "$P_ROLE"
      exit 0
      ;;
    *) dw_die "unexpected simulation record on $DW_HOST: $PENDING" ;;
  esac
fi

LAYOUT="${DW_RUN_DIR}/layout.json"
dw_inventory "$LAYOUT"
dw_section "Current rpool on $DW_HOST"
dw_render_layout "$LAYOUT"

# One row per healthy two-way mirror:
#   vdev role luks, then per member: path serial disk size model
# Rows starting with NOTE explain mirrors this script leaves alone.
ANALYSIS="$(python3 - "$LAYOUT" <<'PY'
import json
import sys

layout = json.load(open(sys.argv[1]))
pool = layout["pool"]
if pool["removal_in_progress"]:
    print("STOP\ta vdev removal is in progress on rpool; wait for it to finish")
    raise SystemExit
if pool["health"] != "ONLINE":
    print(f"STOP\trpool is {pool['health']}; simulate a failure only on a healthy pool")
    raise SystemExit
if "resilver in progress" in (pool["scan"] or ""):
    print("STOP\trpool is resilvering; wait for it to finish")
    raise SystemExit
for vdev in layout["vdevs"]:
    members = vdev["members"]
    if vdev["type"] != "mirror":
        print(f"NOTE\t{vdev['name']} is a single-disk vdev with no redundancy to lose")
        continue
    if vdev.get("replacing") or vdev.get("removing") or len(members) != 2:
        print(f"NOTE\t{vdev['name']} is not a plain two-way mirror right now")
        continue
    if any(m["state"] != "ONLINE" or not m["serial"] for m in members) \
            or members[0]["serial"] == members[1]["serial"]:
        print(f"NOTE\t{vdev['name']} does not have two ONLINE members on separate disks")
        continue
    row = [vdev["name"], "boot" if vdev["holds_esp"] else "extra",
           "luks" if vdev["luks"] else "clear"]
    for m in members:
        row += [m["path"], m["serial"], m["disk"] or "-", str(m["disk_size"] or "?"),
                (m["model"] or "-").replace("\t", " ")]
    print("\t".join(row))
PY
)" || dw_die "could not analyze the rpool layout of $DW_HOST"

dw_section "Healthy rpool mirrors on $DW_HOST"
MIRRORS=()
while IFS= read -r row; do
  [[ -n "$row" ]] || continue
  case "$row" in
    STOP$'\t'*) dw_die "${row#STOP$'\t'}" ;;
    NOTE$'\t'*) printf 'NOTE: %s\n' "${row#NOTE$'\t'}" ;;
    *) MIRRORS+=("$row") ;;
  esac
done <<<"$ANALYSIS"
((${#MIRRORS[@]} > 0)) ||
  dw_die "no rpool mirror on $DW_HOST has two ONLINE members; nothing to simulate"
for index in "${!MIRRORS[@]}"; do
  IFS=$'\t' read -r vdev role luks _ serial_1 _ _ _ _ serial_2 _ _ _ <<<"${MIRRORS[index]}"
  label="extra mirror"
  [[ "$role" != boot ]] || label="boot mirror"
  encryption="unencrypted"
  [[ "$luks" != luks ]] || encryption="LUKS"
  printf '  %d) %-10s %-12s %-12s disks %s and %s\n' \
    "$((index + 1))" "$vdev" "$label" "$encryption" "$serial_1" "$serial_2"
done
while true; do
  IFS= read -r -p "Number of the mirror to simulate a failure in (q to quit): " CHOICE ||
    dw_die "input ended"
  [[ "$CHOICE" != q ]] || { printf 'Nothing was changed.\n'; exit 0; }
  if [[ "$CHOICE" =~ ^[1-9][0-9]*$ ]] && ((CHOICE <= ${#MIRRORS[@]})); then
    break
  fi
  printf 'Enter a number from the list.\n'
done
IFS=$'\t' read -r VDEV ROLE LUKS PATH_1 SERIAL_1 DISK_1 SIZE_1 MODEL_1 \
  PATH_2 SERIAL_2 DISK_2 SIZE_2 MODEL_2 <<<"${MIRRORS[CHOICE - 1]}"

dw_section "Members of $VDEV"
printf '  1) serial %-22s %-14s %16s bytes  %-20s %s\n' \
  "$SERIAL_1" "$DISK_1" "$SIZE_1" "$MODEL_1" "$PATH_1"
printf '  2) serial %-22s %-14s %16s bytes  %-20s %s\n' \
  "$SERIAL_2" "$DISK_2" "$SIZE_2" "$MODEL_2" "$PATH_2"
while true; do
  IFS= read -r -p "Number of the member whose disk fails (q to quit): " CHOICE ||
    dw_die "input ended"
  [[ "$CHOICE" != q ]] || { printf 'Nothing was changed.\n'; exit 0; }
  [[ "$CHOICE" == 1 || "$CHOICE" == 2 ]] && break
  printf 'Enter 1 or 2.\n'
done
if [[ "$CHOICE" == 1 ]]; then
  SERIAL="$SERIAL_1" DISK="$DISK_1" LEAF="$PATH_1" SURVIVOR="$SERIAL_2"
else
  SERIAL="$SERIAL_2" DISK="$DISK_2" LEAF="$PATH_2" SURVIVOR="$SERIAL_1"
fi

dw_section "Simulated failure"
printf '  host:            %s\n' "$DW_HOST"
printf '  mirror:          %s (%s mirror, %s)\n' "$VDEV" "$ROLE" \
  "$([[ "$LUKS" == luks ]] && echo LUKS || echo unencrypted)"
printf '  failing disk:    serial %s (currently %s)\n' "$SERIAL" "$DISK"
printf '  failing member:  %s\n' "$LEAF"
printf '  surviving disk:  serial %s\n' "$SURVIVOR"
if [[ "$ROLE" == boot ]]; then
  printf '\nThis is the BOOT mirror: after the wipe only disk %s can boot %s until\n' \
    "$SURVIVOR" "$DW_HOST"
  printf 'add_replacement_disk.sh gives the replacement disk its own ESP.\n'
fi
printf '\nNEXT (reversible): take %s offline' "$LEAF"
[[ "$LUKS" != luks ]] || printf ' and close its LUKS mapping'
printf '.\n%s has no redundancy from here until the replacement finishes resilvering.\n' "$VDEV"
confirm_exact "Take disk $SERIAL's member of $VDEV on $DW_HOST offline."

dw_section "Taking disk $SERIAL's member of $VDEV offline"
on_host_simulator offline "$MARKER" "$VDEV" "$SERIAL" ||
  dw_die "taking the member offline failed; rerun this script to see what is left to do"

confirm_wipe "$SERIAL" "$VDEV" || exit 0
wipe_disk "$SERIAL"
MAPPER=-
[[ "$LUKS" != luks ]] || MAPPER="${LEAF#/dev/mapper/}"
print_next_steps "$SERIAL" "$VDEV" "$MAPPER" "$ROLE"
