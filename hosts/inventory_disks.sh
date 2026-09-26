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
# shellcheck source=../lib/disk_workflows.sh
source "${SCRIPT_DIR}/../lib/disk_workflows.sh"

usage() {
  cat <<'EOF'
Usage: hosts/inventory_disks.sh [--host moxN]

Inventories all physical disks on the target host, every imported ZFS pool
vdev and its member disks, and disks that are not in use and can safely be removed
physically.

When a vdev removal started by hosts/decommission_disks.sh has completed,
this script asks before closing its LUKS mappings, removing their crypttab
entries, rebuilding the initramfs, and marking the disks retired. Run this
script after decommissioning to finish retirement and identify disks that
are safe to pull.
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

# Finalize every completed vdev removal. The host-side record is authoritative
# after ZFS forgets the member serials of an evacuated vdev.
mapfile -t PENDING < <(python3 - "$LAYOUT" "$STATE" <<'PY'
import json
import re
import sys

layout = json.load(open(sys.argv[1]))
state = json.load(open(sys.argv[2]))
pool = layout["pool"]
in_pool = {m["serial"] for v in layout["vdevs"] for m in v["members"] if m["serial"]}
evacuating = re.search(r"Evacuation of (\S+) in progress", pool["remove"] or "")
for row in state["removals"]:
    if row["state"] != "requested":
        continue
    serials = [m["serial"] for m in row["members"]]
    mappers = [
        m["mapper"].rsplit("/", 1)[-1]
        for m in row["members"] if m["luks"] and m["mapper"]
    ]
    if pool["removal_in_progress"] and evacuating and evacuating.group(1) == row["vdev"]:
        status = "evacuating"
    elif set(serials) & in_pool:
        status = "still-in-pool"
    elif mappers:
        status = "needs-retire"
    else:
        status = "complete"
    print("\t".join([row["id"], row["vdev"], status, ",".join(serials), ",".join(mappers)]))
PY
)

FINALIZED=false
for row in "${PENDING[@]}"; do
  IFS=$'\t' read -r removal_id vdev status serials mappers <<<"$row"
  dw_section "Retirement status of $vdev (disks ${serials//,/, })"
  case "$status" in
    evacuating)
      printf 'ZFS is still copying data off %s:\n%s\n' "$vdev" \
        "$(dw_json "$LAYOUT" 'd["pool"]["remove"]')"
      printf 'Its disks are not safe to pull yet. Run this inventory again later.\n'
      continue
      ;;
    still-in-pool)
      printf '%s is still part of rpool and no removal is running; its disks are not safe to pull.\n' "$vdev"
      dw_state set-removal "$removal_id" --state failed \
        --note "disks still in rpool with no removal running" >/dev/null ||
        dw_die "could not update the removal record on $DW_HOST"
      printf 'The removal record is marked failed; rerun hosts/decommission_disks.sh.\n'
      continue
      ;;
    needs-retire)
      printf 'The following disks will be finalized:\n'
      python3 - "$STATE" "$removal_id" <<'PY'
import json
import sys

state = json.load(open(sys.argv[1]))
record = next(row for row in state["removals"] if row["id"] == sys.argv[2])
for member in record["members"]:
    print(
        "  serial {serial}  device {disk}  {size} bytes  model {model}".format(
            serial=member["serial"],
            disk=member["disk"],
            size=member["disk_size"],
            model=member["model"] or "-",
        )
    )
PY
      printf '\nFinalizing these disks will close LUKS mappings %s, remove their\n' \
        "${mappers//,/ }"
      printf 'entries from /etc/crypttab, rebuild and verify every initramfs, and mark\n'
      printf 'their host-side removal record retired.\n'
      if ! prompt_yes "Do you give permission to finalize these disks now?"; then
        printf 'Permission was not given. No retirement changes were made for %s.\n' "$vdev"
        continue
      fi
      dw_install_tool
      IFS=',' read -r -a mapper_list <<<"$mappers"
      dw_tool retire-luks "${mapper_list[@]}" ||
        dw_die "finalizing the LUKS retirement failed; its disks are not safe to pull"
      ;;
    complete)
      printf 'The following unencrypted disks will be marked retired:\n'
      python3 - "$STATE" "$removal_id" <<'PY'
import json
import sys

state = json.load(open(sys.argv[1]))
record = next(row for row in state["removals"] if row["id"] == sys.argv[2])
for member in record["members"]:
    print(
        "  serial {serial}  device {disk}  {size} bytes  model {model}".format(
            serial=member["serial"],
            disk=member["disk"],
            size=member["disk_size"],
            model=member["model"] or "-",
        )
    )
PY
      printf '\nNo LUKS or crypttab change is needed. The host-side removal record will\n'
      printf 'be changed to retired so these disks can be reported as removable.\n'
      if ! prompt_yes "Do you give permission to finalize these disks now?"; then
        printf 'Permission was not given. No retirement changes were made for %s.\n' "$vdev"
        continue
      fi
      ;;
    *) dw_die "unexpected removal status $status" ;;
  esac

  dw_state set-removal "$removal_id" --state retired \
    --note "removal complete; LUKS closed, crypttab and initramfs updated" >/dev/null ||
    dw_die "could not mark the disks retired on $DW_HOST"
  FINALIZED=true
  printf 'Retirement of %s is finalized.\n' "$vdev"
done

# Recollect only after a finalization changed host state. With no change, the
# complete inventory printed at startup is still current and need not repeat.
if [[ "$FINALIZED" == true ]]; then
  dw_section "Disk inventory after retirement finalization"
  dw_collect_layout "$LAYOUT" --allow-missing-pool
  dw_render_inventory "$LAYOUT"
fi
