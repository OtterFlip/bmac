#!/usr/bin/env bash

# Copyright (c) 2026 BEENTHERE VENTURES, INC.
# SPDX-License-Identifier: GPL-3.0-only

# Report which disks removed from a host's rpool by hosts/decommission_disks.sh
# are safe to pull. When a recorded vdev removal has completed, the script
# first retires its disks: it closes their LUKS mappings, removes them from
# /etc/crypttab, rebuilds and verifies the initramfs, and comments out their
# NVME_MIRROR_N entries in env/moxN.conf. The disks are identified by the
# serials recorded on the host when the removal started, because ZFS no
# longer knows which disks a removed vdev used.

set -Eeuo pipefail
set +x
umask 077

DW_SCRIPT_NAME=list_disks_ready_for_physically_removal.sh
SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
# shellcheck source=../lib/disk_workflows.sh
source "${SCRIPT_DIR}/../lib/disk_workflows.sh"

usage() {
  cat <<'EOF'
Usage: hosts/list_disks_ready_for_physically_removal.sh [--host moxN]

Shows the progress of the vdev removal hosts/decommission_disks.sh started on
the chosen host. Once it has completed, retires the removed disks (closes
their LUKS mappings, removes them from /etc/crypttab, rebuilds the
initramfs, and comments them out of env/moxN.conf), then lists every retired
disk, by serial, that is still installed and safe to pull.
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
dw_collect_layout "$LAYOUT"
dw_state show >"$STATE" || dw_die "could not read the storage state on $DW_HOST"

if [[ "$(dw_json "$STATE" 'str(len(d["removals"]))')" == 0 ]]; then
  printf '\nNo vdev removals are recorded on %s.\n' "$DW_HOST"
  exit 0
fi

# Classify each recorded removal still waiting for its disks to be retired.
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
        m["mapper"].rsplit("/", 1)[-1] for m in row["members"] if m["luks"] and m["mapper"]
    ]
    if pool["removal_in_progress"] and evacuating and evacuating.group(1) == row["vdev"]:
        status = "evacuating"
    elif set(serials) & in_pool:
        status = "still-in-pool"
    elif mappers:
        # Always let the tool finish a LUKS retirement: a run interrupted after
        # it closed the mappings and edited crypttab may have left an
        # initramfs that still expects them. The tool only rebuilds when needed.
        status = "needs-retire"
    else:
        status = "complete"
    print("\t".join([row["id"], row["vdev"], status, ",".join(serials), ",".join(mappers)]))
PY
)

for row in "${PENDING[@]}"; do
  IFS=$'\t' read -r removal_id vdev status serials mappers <<<"$row"
  dw_section "Removal of $vdev (disks ${serials//,/, })"
  case "$status" in
    evacuating)
      printf 'ZFS is still copying data off %s:\n%s\n' "$vdev" \
        "$(dw_json "$LAYOUT" 'd["pool"]["remove"]')"
      printf 'Its disks are not safe to pull yet; run this script again later.\n'
      continue
      ;;
    still-in-pool)
      printf '%s and its disks are still part of rpool and no removal is running, so the\n' "$vdev"
      printf 'removal was canceled or failed. Nothing here is safe to pull.\n'
      dw_state set-removal "$removal_id" --state failed \
        --note "disks still in rpool with no removal running" >/dev/null ||
        dw_die "could not update the removal record on $DW_HOST"
      printf 'The record is marked failed; run hosts/decommission_disks.sh again to retry.\n'
      continue
      ;;
    needs-retire)
      dw_install_tool
      IFS=',' read -r -a mapper_list <<<"$mappers"
      dw_ready "close LUKS mappings ${mapper_list[*]}, remove them from /etc/crypttab, rebuild the initramfs for every installed kernel, and verify the next boot no longer expects them" \
        "a few minutes" ||
        dw_die "stopped before retiring the disks; run this script again when ready"
      dw_tool retire-luks "${mapper_list[@]}" ||
        dw_die "retiring the LUKS mappings failed; nothing is safe to pull yet"
      ;;
    complete) ;;
    *) dw_die "unexpected removal status $status" ;;
  esac

  IFS=',' read -r -a serial_list <<<"$serials"
  conf_args=()
  for serial in "${serial_list[@]}"; do
    conf_args+=(--serial "$serial")
  done
  CONF_STATUS=0
  dw_edit_conf retire "${conf_args[@]}" \
    --note "Removed from rpool ($vdev) on $(date +%Y-%m-%d) by hosts/list_disks_ready_for_physically_removal.sh" ||
    CONF_STATUS=$?
  case "$CONF_STATUS" in
    0) printf 'Commented out the removed mirror in %s.\n' "$(dw_conf_path)" ;;
    3) printf 'NOTE: %s does not record these serials (or is not on this workstation); nothing to comment out.\n' "$(dw_conf_path)" ;;
    *) printf 'WARNING: could not update %s; comment out the NVME_MIRROR entries for %s by hand.\n' "$(dw_conf_path)" "$serials" >&2 ;;
  esac
  dw_state set-removal "$removal_id" --state retired \
    --note "removal complete; LUKS closed, crypttab and initramfs updated" >/dev/null ||
    dw_die "could not update the removal record on $DW_HOST"
  printf 'Retired the disks of %s.\n' "$vdev"
done

dw_state show >"$STATE" || dw_die "could not read the storage state on $DW_HOST"
dw_collect_layout "$LAYOUT"

# The layout covers rpool only. Before calling a retired disk safe to pull,
# prove on the host that no imported pool holds it and nothing mounts it,
# with the same check that guards every disk erase.
mapfile -t RETIRED_PRESENT < <(python3 - "$LAYOUT" "$STATE" <<'PY'
import json
import sys
layout = json.load(open(sys.argv[1]))
state = json.load(open(sys.argv[2]))
in_pool = {m["serial"] for v in layout["vdevs"] for m in v["members"] if m["serial"]}
present = {d["serial"]: d for d in layout["unassigned_disks"] if d["serial"]}
for row in state["removals"]:
    if row["state"] == "retired":
        for member in row["members"]:
            disk = present.get(member["serial"])
            if disk and member["serial"] not in in_pool and not disk["mounted"]:
                print(f"{member['serial']}\t{disk['size']}")
PY
)
IN_USE=()
if ((${#RETIRED_PRESENT[@]} > 0)); then
  dw_install_tool
  for row in "${RETIRED_PRESENT[@]}"; do
    IFS=$'\t' read -r serial size <<<"$row"
    dw_tool check-new --expect-bytes "$size" --match exact "$serial" >/dev/null 2>&1 ||
      IN_USE+=("$serial")
  done
fi

dw_section "Disks on $DW_HOST that are safe to pull"
python3 - "$LAYOUT" "$STATE" "${IN_USE[@]}" <<'PY'
import datetime
import json
import sys
layout = json.load(open(sys.argv[1]))
state = json.load(open(sys.argv[2]))
in_use = set(sys.argv[3:])
in_pool = {m["serial"] for v in layout["vdevs"] for m in v["members"] if m["serial"]}
present = {d["serial"]: d for d in layout["unassigned_disks"] if d["serial"]}
ready, pulled, mounted, clear = [], [], [], False
for row in state["removals"]:
    if row["state"] != "retired":
        continue
    when = datetime.datetime.fromtimestamp(row["updated_at"]).strftime("%Y-%m-%d %H:%M")
    for member in row["members"]:
        serial = member["serial"]
        if serial in in_pool:
            continue
        if serial in present and (present[serial]["mounted"] or serial in in_use):
            mounted.append((serial, present[serial]["disk"], row["vdev"]))
        elif serial in present:
            ready.append((serial, present[serial]["disk"], member["disk_size"],
                          member["model"] or "-", row["vdev"], when))
            clear = clear or not member["luks"]
        else:
            pulled.append((serial, row["vdev"], when))
if ready:
    print(f"{'SERIAL':<22} {'DEVICE':<14} {'CAPACITY_BYTES':>16}  {'MODEL':<20} {'FROM':<9} RETIRED")
    for serial, device, size, model, vdev, when in ready:
        print(f"{serial:<22} {device:<14} {size:>16}  {model:<20} {vdev:<9} {when}")
    print("\nMatch each serial to the label on the drive before pulling it.")
    if clear:
        print("These disks were not encrypted and still hold old rpool data; wipe them")
        print("before reuse or disposal if that matters to you.")
else:
    print("  none")
if mounted:
    print("\nNOT safe to pull: these retired disks are in use again (a mounted")
    print("filesystem, another ZFS pool, or a failed check on the host):")
    for serial, device, vdev in mounted:
        print(f"  {serial} ({device}, from {vdev})")
if pulled:
    print("\nAlready pulled (retired serials no longer attached): "
          + ", ".join(f"{serial} ({vdev})" for serial, vdev, _ in pulled))
PY
