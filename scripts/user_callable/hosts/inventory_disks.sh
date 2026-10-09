#!/usr/bin/env bash

# Copyright (c) 2026 BEENTHERE VENTURES, INC.
# SPDX-License-Identifier: GPL-3.0-only

# Inventory every physical disk and rpool vdev on a Proxmox host. When a
# decommissioned vdev has finished evacuating, this script offers to finalize
# its retirement before reporting which disks are safe to remove physically.

set -Eeuo pipefail
set +x
umask 077

DW_SCRIPT_NAME=inventory_disks.sh
SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
# shellcheck source=../../lib/disk_workflows.sh
source "${SCRIPT_DIR}/../../lib/disk_workflows.sh"
bmac_ui_bootstrap "$@"

usage() {
  cat <<'EOF'
Usage: scripts/user_callable/hosts/inventory_disks.sh [--host moxN]

Inventories all physical disks on the target host, every imported ZFS pool
vdev and its member disks, and disks that are not in use and can safely be removed
physically.

When a vdev removal started by scripts/user_callable/hosts/decommission_disks.sh has completed,
this script asks before closing its LUKS mappings, removing their crypttab
entries, rebuilding the initramfs, releasing the disks, and marking them
retired. Releasing a disk erases only its metadata (LUKS key slots, ZFS labels,
filesystem signatures, and the partition table) so it shows as blank; it takes
seconds and does not overwrite the data area. The off-host LUKS header backups
of released disks are deleted. Run this script after decommissioning to finish
retirement and identify disks that are safe to pull.
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
dw_select_host "$HOST_ARG"
LAYOUT="${DW_RUN_DIR}/layout.json"
STATE="${DW_RUN_DIR}/state.json"
dw_inventory "$LAYOUT" --allow-missing-pool
dw_state show >"$STATE" || dw_die "could not read the storage state on $DW_HOST"

# Finalize every completed vdev removal. scripts/user_callable/diagnostics/list_disks.sh reports
# disks as awaiting finalization from the same classification.
PENDING_TEXT="$(python3 "$DW_DISK_INVENTORY" pending-removals "$LAYOUT" "$STATE")" ||
  dw_die "could not classify the recorded vdev removals on $DW_HOST"
PENDING=()
[[ -z "$PENDING_TEXT" ]] || mapfile -t PENDING <<<"$PENDING_TEXT"

print_release_note() {
  printf 'Releasing a disk erases only its metadata (LUKS key slots, ZFS labels,\n'
  printf 'filesystem signatures, and the partition table) so it shows as blank. It\n'
  printf 'takes seconds and does not overwrite the data area; a full data wipe is\n'
  printf 'outside the scope of these scripts.\n'
}

print_removal_members() {
  python3 - "$STATE" "$1" <<'PY'
import json
import sys

state = json.load(open(sys.argv[1]))
record = next(row for row in state["removals"] if row["id"] == sys.argv[2])
for member in record["members"]:
    if not member["serial"]:
        print("  missing member {path}; its disk was already gone, so nothing is released".format(
            path=member["path"],
        ))
        continue
    print(
        "  serial {serial}  device {disk}  {size} bytes  model {model}".format(
            serial=member["serial"],
            disk=member["disk"],
            size=member["disk_size"],
            model=member["model"] or "-",
        )
    )
PY
}

# ask_permission VDEV: 0 when the operator lets this run finalize VDEV.
ask_permission() {
  if bmac_ui_is_json; then
    bmac_ui_confirm --id "finalize_$1" --question --severity warning \
      --title "Finalize the retirement of $1 now?" \
      --message "This releases its disks, erasing their metadata so they show as blank. The data area is not overwritten." \
      --confirm-label "Finalize" --cancel-label "Not now"
    return
  fi
  prompt_yes "Do you give permission to finalize these disks now?"
}

CHANGED=false
for row in "${PENDING[@]}"; do
  IFS=$'\t' read -r removal_id vdev status serials mappers <<<"$row"
  dw_section "Retirement status of $vdev (disks ${serials//,/, })"
  case "$status" in
    evacuating)
      printf 'ZFS is still copying data off %s:\n%s\n' "$vdev" \
        "$(dw_json "$LAYOUT" 'd["pool"]["remove"]')"
      printf 'Its disks are not safe to pull yet. Run this inventory again later.\n'
      bmac_ui_next_step "ZFS is still evacuating $vdev; its disks are not safe to pull yet. Run this inventory again later." \
        --workflow inventory_disks --arg "host=$DW_HOST"
      continue
      ;;
    still-in-pool)
      printf '%s is still part of rpool and no removal is running; its disks are not safe to pull.\n' "$vdev"
      dw_state set-removal "$removal_id" --state failed \
        --note "disks still in rpool with no removal running" >/dev/null ||
        dw_die "could not update the removal record on $DW_HOST"
      printf 'The removal record is marked failed; rerun scripts/user_callable/hosts/decommission_disks.sh.\n'
      bmac_ui_next_step "The removal of $vdev is marked failed; decommission it again." \
        --command "scripts/user_callable/hosts/decommission_disks.sh --host $DW_HOST" --workflow decommission_disks --arg "host=$DW_HOST"
      continue
      ;;
    needs-retire)
      printf 'The following disks will be finalized:\n'
      print_removal_members "$removal_id"
      printf '\nFinalizing these disks will close LUKS mappings %s, remove their\n' \
        "${mappers//,/ }"
      printf 'entries from /etc/crypttab, rebuild and verify every initramfs, release the\n'
      printf 'disks, delete their off-host LUKS header backups, and mark their host-side\n'
      printf 'removal record retired.\n'
      print_release_note
      if ! ask_permission "$vdev"; then
        printf 'Permission was not given. No retirement changes were made for %s.\n' "$vdev"
        continue
      fi
      dw_install_tool
      IFS=',' read -r -a mapper_list <<<"$mappers"
      dw_tool retire-luks "${mapper_list[@]}" ||
        dw_die "finalizing the LUKS retirement failed; its disks are not safe to pull"
      CHANGED=true
      ;;
    complete)
      printf 'The following unencrypted disks will be marked retired:\n'
      print_removal_members "$removal_id"
      printf '\nNo LUKS or crypttab change is needed. The disks will be released and the\n'
      printf 'host-side removal record changed to retired so they can be reported as\n'
      printf 'removable.\n'
      print_release_note
      if ! ask_permission "$vdev"; then
        printf 'Permission was not given. No retirement changes were made for %s.\n' "$vdev"
        continue
      fi
      dw_install_tool
      mapper_list=()
      ;;
    *) dw_die "unexpected removal status $status" ;;
  esac

  IFS=',' read -r -a serial_list <<<"$serials"
  if ! dw_tool release-disks "${serial_list[@]}"; then
    printf 'WARNING: not every disk of %s could be released; its removal record stays open.\n' "$vdev" >&2
    printf 'Resolve the reasons above and run this inventory again.\n' >&2
    continue
  fi
  CHANGED=true
  for mapper in "${mapper_list[@]}"; do
    rm -f -- "${DW_ARTIFACTS_DIR}/${DW_HOST}/luks-headers/rpool-${mapper#crypt-rpool-}.bin"
  done
  dw_state set-removal "$removal_id" --state retired \
    --note "removal complete; LUKS closed, crypttab and initramfs updated, disks released" >/dev/null ||
    dw_die "could not mark the disks retired on $DW_HOST"
  printf 'Retirement of %s is finalized.\n' "$vdev"
  bmac_ui_next_step "The disks of $vdev (${serials//,/, }) on $DW_HOST are retired and safe to pull physically."
done

# Recollect only after a finalization changed host state. With no change, the
# complete inventory printed at startup is still current and need not repeat.
if [[ "$CHANGED" == true ]]; then
  dw_section "Disk inventory after retirement finalization"
  dw_collect_layout "$LAYOUT" --allow-missing-pool
  dw_render_inventory "$LAYOUT"
fi
