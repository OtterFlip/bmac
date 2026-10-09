#!/usr/bin/env bash

# Copyright (c) 2026 BEENTHERE VENTURES, INC.
# SPDX-License-Identifier: GPL-3.0-only

# Add two newly installed disks whose capacities differ by at most 1% to a
# host's rpool as one new ZFS mirror vdev. Each disk gets one member partition
# that ends 1 GiB short of the smaller disk's whole-GiB size. On an encrypted
# host both partitions become LUKS2 with the host's single shared rpool
# passphrase, which the operator types at
# the host console; on an unencrypted host they stay unencrypted. The disk,
# LUKS, and zpool logic is lib/rpool_mirror.sh, the same code
# hosts/add_proxmox_host.sh uses for extra mirrors at install time.

set -Eeuo pipefail
set +x
umask 077

DW_SCRIPT_NAME=add_new_disk_vdev.sh
SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
# shellcheck source=../lib/disk_workflows.sh
source "${SCRIPT_DIR}/../lib/disk_workflows.sh"
bmac_ui_bootstrap "$@"

MAX_MIRRORS=5

usage() {
  cat <<'EOF'
Usage: hosts/add_new_disk_vdev.sh [--host moxN]

Adds two new disks to the chosen host's rpool as one new mirror vdev:

  1. lists the disks that are not part of rpool and asks for two whose byte
     capacities differ by at most 1%; if either is not blank (LUKS,
     partitions, ZFS labels, or filesystem signatures), both must be wiped
     before anything else happens, and nothing on them is ever reused;
  2. on an encrypted host, has you run a helper at the host console that asks
     for the shared rpool LUKS passphrase, proves it unlocks every existing
     rpool member, and gives each disk one LUKS partition formatted with it;
  3. records the disks in /etc/crypttab, rebuilds and verifies the initramfs,
     and adds them to rpool;
  4. prints the new disks' serials and exact byte capacities.

A rerun resumes a mirror whose disks were prepared but not yet added. rpool
holds at most five mirrors, the boot mirror included.
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
dw_install_tool
LAYOUT="${DW_RUN_DIR}/layout.json"
dw_inventory "$LAYOUT"

dw_section "Current rpool on $DW_HOST"
dw_render_layout "$LAYOUT"

ENCRYPTION="$(dw_json "$LAYOUT" '("luks" if d["vdevs"] and all(v["luks"] for v in d["vdevs"]) else "clear" if not any(v["luks"] for v in d["vdevs"]) else "mixed")')" ||
  dw_die "could not read the encryption of rpool"
[[ "$ENCRYPTION" != mixed ]] ||
  dw_die "rpool on $DW_HOST mixes LUKS and unencrypted vdevs; resolve that first"
VDEV_COUNT="$(dw_json "$LAYOUT" 'len(d["vdevs"])')"

# Print a completed addition and clear its temporary host-side resume record.
record_new_pair() {
  local pair="$1" serial_1="$2" serial_2="$3" capacity_1="$4" capacity_2="$5"
  printf '\nNew mirror disks on %s:\n' "$DW_HOST"
  printf '  mirror slot:    %s\n' "$pair"
  printf '  serial 1:       %s\n' "$serial_1"
  printf '  capacity bytes: %s\n' "$capacity_1"
  printf '  serial 2:       %s\n' "$serial_2"
  printf '  capacity bytes: %s\n' "$capacity_2"
  dw_state clear-addition --pair "$pair" ||
    printf 'WARNING: could not clear the addition record on %s\n' "$DW_HOST" >&2
}

# A pair an earlier run added to rpool but did not finish reporting.
STATE="${DW_RUN_DIR}/state.json"
dw_state show >"$STATE" || dw_die "could not read the storage records on $DW_HOST"
mapfile -t ADDED < <(python3 - "$LAYOUT" "$STATE" <<'PY'
import json
import sys
layout = json.load(open(sys.argv[1]))
additions = json.load(open(sys.argv[2])).get("additions", {})
for pair, row in sorted(additions.items()):
    for vdev in layout["vdevs"]:
        if set(row["serials"]) <= {member["serial"] for member in vdev["members"]}:
            print("\t".join(str(value) for value in (pair, *row["serials"], *row["capacities"])))
PY
)
if ((${#ADDED[@]} > 0)); then
  for row in "${ADDED[@]}"; do
    IFS=$'\t' read -r added_pair added_1 added_2 added_capacity_1 added_capacity_2 <<<"$row"
    dw_section "Mirror slot $added_pair, which an earlier run added to rpool"
    record_new_pair "$added_pair" "$added_1" "$added_2" "$added_capacity_1" "$added_capacity_2"
  done
  printf '\nRun this script again to add another mirror.\n'
  bmac_ui_next_step "Run this workflow again to add another mirror." --workflow add_new_disk_vdev --arg "host=$DW_HOST"
  exit 0
fi

# A pair whose two LUKS mappings are open but not in rpool was prepared by an
# interrupted run; offer to finish adding it.
RESUME="$(python3 - "$LAYOUT" "$STATE" <<'PY'
import json
import re
import sys
layout = json.load(open(sys.argv[1]))
# A removed vdev keeps its mappings open until inventory_disks.sh retires them;
# those disks are leaving, not being added.
leaving = {
    member["serial"]
    for row in json.load(open(sys.argv[2]))["removals"] if row["state"] == "requested"
    for member in row["members"]
}
pairs = {}
for row in layout["luks_mappings"]:
    match = re.fullmatch(r"crypt-rpool-mirror([2-5])-([12])", row["mapper"])
    if match and not row["in_pool"] and row["serial"] not in leaving:
        pairs.setdefault(int(match.group(1)), {})[int(match.group(2))] = row
for pair, members in sorted(pairs.items()):
    if set(members) == {1, 2}:
        print(pair, members[1]["serial"], members[2]["serial"], members[1]["size"], members[2]["size"])
        break
    print(f"PARTIAL {pair}")
    break
PY
)"
PAIR=""
SERIAL_1=""
SERIAL_2=""
CAPACITY_1=""
CAPACITY_2=""
RESUMING=false
if [[ "$RESUME" == PARTIAL* ]]; then
  dw_die "only one LUKS mapping of extra mirror ${RESUME#PARTIAL } is open and it is not in rpool; rerun its console helper (/root/app-ha-add-mirror-${RESUME#PARTIAL }) or close the mapping before adding disks"
fi
if [[ -n "$RESUME" ]]; then
  read -r PAIR SERIAL_1 SERIAL_2 CAPACITY_1 CAPACITY_2 <<<"$RESUME"
  printf '\nExtra mirror %s (disks %s and %s) was prepared by an earlier run but is not in rpool yet.\n' \
    "$PAIR" "$SERIAL_1" "$SERIAL_2"
  prompt_yes "Resume adding that mirror now?" ||
    dw_die "the prepared mirror was left untouched; rerun to resume it"
  RESUMING=true
fi

if [[ "$RESUMING" == false ]]; then
  ((VDEV_COUNT < MAX_MIRRORS)) ||
    dw_die "rpool on $DW_HOST already has $VDEV_COUNT mirror vdevs, the most BMAC supports; decommission one first"
  PAIR="$(python3 - "$LAYOUT" "$STATE" <<'PY'
import json
import re
import sys
layout = json.load(open(sys.argv[1]))
used = {int(pair) for pair in json.load(open(sys.argv[2])).get("additions", {})}
for row in layout["luks_mappings"] + layout["crypttab"]:
    match = re.fullmatch(r"crypt-rpool-mirror([2-5])-[12]", row["mapper"])
    if match:
        used.add(int(match.group(1)))
free = [pair for pair in (2, 3, 4, 5) if pair not in used]
print(free[0] if free else "")
PY
)"
  [[ -n "$PAIR" ]] ||
    dw_die "extra-mirror slots 2-5 are all in use on $DW_HOST"

  dw_section "Disks on $DW_HOST that are not part of rpool"
  mapfile -t CANDIDATES < <(python3 - "$LAYOUT" "$STATE" <<'PY'
import json
import sys
leaving = {
    member["serial"]
    for row in json.load(open(sys.argv[2]))["removals"] if row["state"] == "requested"
    for member in row["members"]
}
for disk in json.load(open(sys.argv[1]))["unassigned_disks"]:
    if disk["serial"] in leaving or disk.get("in_use_reasons"):
        continue
    contents = (
        "mounted" if disk["mounted"]
        else "has partitions or signatures" if disk["partitions_or_holders"] or disk["fstype"]
        else "blank"
    )
    print("\t".join(str(value) for value in (
        disk["disk"], disk["serial"] or "-", disk["size"] or 0,
        (disk["model"] or "-").replace("\t", " "), contents
    )))
PY
  )
  ((${#CANDIDATES[@]} >= 2)) ||
    dw_die "fewer than two disks outside rpool were found on $DW_HOST; install two disks of the same nominal capacity first"
  for index in "${!CANDIDATES[@]}"; do
    IFS=$'\t' read -r disk serial size model contents <<<"${CANDIDATES[index]}"
    printf '  %d) %-14s serial %-22s %16s bytes  %-20s %s\n' \
      "$((index + 1))" "$disk" "$serial" "$size" "$model" "$contents"
  done

  # Sets CHOSEN_SERIAL and CHOSEN_SIZE from the numbered list above.
  choose_disk() {
    local label="$1" choice disk serial size model contents
    while true; do
      IFS= read -r -p "Number of the ${label} disk (q to quit): " choice ||
        dw_die "input ended"
      [[ "$choice" != q ]] || dw_die "no disks were changed"
      if ! [[ "$choice" =~ ^[1-9][0-9]*$ ]] || ((choice > ${#CANDIDATES[@]})); then
        printf 'Enter a number from the list.\n'
        continue
      fi
      IFS=$'\t' read -r disk serial size model contents <<<"${CANDIDATES[choice - 1]}"
      if [[ "$serial" == - ]]; then
        printf 'That disk reports no serial number and cannot be selected safely.\n'
      elif [[ "$contents" == mounted ]]; then
        printf 'That disk has a mounted filesystem and cannot be used.\n'
      else
        CHOSEN_SERIAL="$serial"
        CHOSEN_SIZE="$size"
        return
      fi
    done
  }
  # JSON mode: both disks are chosen together in one form.
  choose_disks_json() {
    local -a options=()
    local index disk serial size model contents first second
    for index in "${!CANDIDATES[@]}"; do
      IFS=$'\t' read -r disk serial size model contents <<<"${CANDIDATES[index]}"
      [[ "$serial" != - && "$contents" != mounted ]] || continue
      options+=(--option "$((index + 1))" "$disk  ·  $serial  ·  $size bytes  ·  $model" --option-help "$contents")
    done
    ((${#options[@]} >= 10)) ||
      dw_die "fewer than two disks outside rpool can be selected safely on $DW_HOST"
    bmac_ui_group_begin disks "Choose the disks for extra mirror $PAIR" \
      "Their byte capacities must differ by at most 1%. Disks without a serial number or with a mounted filesystem are not offered."
    bmac_ui_group_add first --id first --type select --label "First disk" --required "${options[@]}"
    bmac_ui_group_add second --id second --type select --label "Second disk" --required "${options[@]}"
    while true; do
      bmac_ui_group_request
      IFS=$'\t' read -r disk SERIAL_1 CAPACITY_1 model contents <<<"${CANDIDATES[first - 1]}"
      IFS=$'\t' read -r disk SERIAL_2 CAPACITY_2 model contents <<<"${CANDIDATES[second - 1]}"
      if [[ "$SERIAL_1" == "$SERIAL_2" ]]; then
        bmac_ui_field_error second "Choose two different disks."
      elif (((CAPACITY_1 > CAPACITY_2 ? CAPACITY_1 - CAPACITY_2 : CAPACITY_2 - CAPACITY_1) * 100 >
        (CAPACITY_1 > CAPACITY_2 ? CAPACITY_1 : CAPACITY_2))); then
        bmac_ui_field_error second "These disks differ in capacity by more than 1% ($CAPACITY_1 vs $CAPACITY_2 bytes)."
      fi
      bmac_ui_group_check && break
    done
  }
  while true; do
    if bmac_ui_is_json; then
      choose_disks_json
      break
    fi
    choose_disk first
    SERIAL_1="$CHOSEN_SERIAL"
    CAPACITY_1="$CHOSEN_SIZE"
    choose_disk second
    SERIAL_2="$CHOSEN_SERIAL"
    CAPACITY_2="$CHOSEN_SIZE"
    if [[ "$SERIAL_1" == "$SERIAL_2" ]]; then
      printf 'Choose two different disks.\n'
    elif (((CAPACITY_1 > CAPACITY_2 ? CAPACITY_1 - CAPACITY_2 : CAPACITY_2 - CAPACITY_1) * 100 >
      (CAPACITY_1 > CAPACITY_2 ? CAPACITY_1 : CAPACITY_2))); then
      printf 'Those disks differ in capacity by more than 1%% (%s vs %s bytes); choose a matching pair.\n' \
        "$CAPACITY_1" "$CAPACITY_2"
    else
      break
    fi
  done

  dw_section "Checking the chosen disks on $DW_HOST"
  CHECKED="$(dw_tool check-new "$SERIAL_1" "$SERIAL_2")" ||
    dw_die "the chosen disks are not safe to use"
  printf '%s\n' "$CHECKED"
  # A new vdev is built only on blank disks: anything left on a chosen disk,
  # LUKS included, is wiped first and never reused.
  declare -a LEFTOVERS=()
  for serial in "$SERIAL_1" "$SERIAL_2"; do
    if awk -F '\t' -v serial="$serial" '$1 == serial && $4 == "luks" { found=1 } END { exit !found }' <<<"$CHECKED"; then
      LEFTOVERS+=("$serial (LUKS)")
      continue
    fi
    for row in "${CANDIDATES[@]}"; do
      IFS=$'\t' read -r disk candidate_serial size model contents <<<"$row"
      if [[ "$candidate_serial" == "$serial" && "$contents" != blank ]]; then
        LEFTOVERS+=("$serial ($contents)")
      fi
    done
  done
  WIPE=false
  WIPE_NOTE=""
  if ((${#LEFTOVERS[@]} > 0)); then
    dw_section "Existing structures on the chosen disks"
    printf 'These chosen disks are not blank:\n'
    printf '  %s\n' "${LEFTOVERS[@]}"
    printf 'A new mirror is built only on blank disks; nothing on them is reused. Wiping\n'
    printf 'removes all metadata from both chosen disks (LUKS key slots, ZFS labels,\n'
    printf 'filesystem signatures, and the partition table) so they show as blank. It\n'
    printf 'takes seconds and does not overwrite the data area, but whatever the disks\n'
    printf 'held becomes unrecoverable. Answering no stops here with nothing changed.\n'
    prompt_yes "Wipe disks $SERIAL_1 and $SERIAL_2 on $DW_HOST before adding them?" ||
      dw_die "the chosen disks are not blank and were not wiped; nothing was changed"
    WIPE=true
    WIPE_NOTE=" Everything currently on both disks is wiped first."
  fi
  if [[ "$ENCRYPTION" == luks ]]; then
    confirm_exact "ERASE disks $SERIAL_1 and $SERIAL_2 on $DW_HOST, encrypt both with the shared rpool LUKS passphrase (typed at the host console), and add them to rpool as a new mirror.${WIPE_NOTE}" "" destructive
  else
    confirm_exact "ERASE disks $SERIAL_1 and $SERIAL_2 on $DW_HOST and add them to its unencrypted rpool as a new mirror.${WIPE_NOTE}" "" destructive
  fi

  if [[ "$WIPE" == true ]]; then
    dw_section "Wiping the chosen disks on $DW_HOST"
    dw_tool release-disks "$SERIAL_1" "$SERIAL_2" ||
      dw_die "the chosen disks could not be wiped; nothing was added to rpool"
    CHECKED="$(dw_tool check-new "$SERIAL_1" "$SERIAL_2")" ||
      dw_die "the chosen disks are not safe to use after wiping"
    ! awk -F '\t' '$4 == "luks" { found=1 } END { exit !found }' <<<"$CHECKED" ||
      dw_die "a chosen disk still holds LUKS after wiping; nothing was added to rpool"
  fi
fi

dw_state record-addition --pair "$PAIR" --serial "$SERIAL_1" --serial "$SERIAL_2" \
  --capacity "$CAPACITY_1" --capacity "$CAPACITY_2" ||
  dw_die "could not record the new mirror on $DW_HOST; nothing was changed"

HEADER_1=""
HEADER_2=""
if [[ "$ENCRYPTION" == luks ]]; then
  HELPER="${DW_REMOTE_ROOT}/app-ha-add-mirror-${PAIR}"
  if [[ "$RESUMING" == false ]]; then
    # shellcheck disable=SC2016 # The program is expanded by the remote shell.
    dw_on_host bash -c '
set -Eeuo pipefail
umask 077
helper="$1"; tool="$2"; pair="$3"; serial_1="$4"; serial_2="$5"
{
  printf "#!/usr/bin/env bash\nset -Eeuo pipefail\nset +x\n"
  printf "exec %q luks-prepare --pair %q --prompt %q %q\n" \
    "$tool" "$pair" "$serial_1" "$serial_2"
} >"$helper"
chmod 0700 "$helper"
' bash "$HELPER" "$DW_REMOTE_TOOL" "$PAIR" "$SERIAL_1" "$SERIAL_2" ||
      dw_die "could not write the console helper on $DW_HOST"

    printf '\nMANUAL LUKS ACTION REQUIRED\n'
    printf 'At the %s console (iDRAC or physical), log in as root and run:\n  %s\n' \
      "$DW_HOST" "$HELPER"
    printf 'It asks you to type GO, then asks for the shared rpool LUKS passphrase with\n'
    printf 'the input hidden. It proves that passphrase unlocks every existing rpool LUKS\n'
    printf 'member before it formats and opens the two new disks. The passphrase is only\n'
    printf 'ever typed at the console; it never passes through this workstation or SSH.\n'
  fi
  while true; do
    if bmac_ui_is_json; then
      bmac_ui_manual_action --id luks_console --title "Prepare the LUKS members at the $DW_HOST console" \
        --instruction "At the $DW_HOST console (iDRAC or physical), log in as root and run: $HELPER" \
        --instruction "It asks you to type GO, then for the shared rpool LUKS passphrase. The passphrase is only ever typed at the console; it never passes through this workstation." \
        --instruction "Continue once it printed \"LUKS members prepared successfully\". Cancel to stop; rerunning this workflow resumes extra mirror $PAIR." \
        --ack-label "The helper finished"
    else
      printf '\nType GO once the console helper printed "LUKS members prepared successfully" (q to stop): '
      IFS= read -r answer || dw_die "input ended"
      [[ "$answer" != q ]] || dw_die "stopped; rerun this script to resume extra mirror $PAIR"
      [[ "$answer" == GO ]] || continue
    fi
    if dw_tool luks-check-prepared --pair "$PAIR" "$SERIAL_1" "$SERIAL_2"; then
      break
    fi
    printf 'The new disks are not fully prepared yet; rerun %s at the console.\n' "$HELPER"
  done

  # Keep the LUKS header backups off-host beside the ones setup made.
  HEADER_DIR="${DW_ARTIFACTS_DIR}/${DW_HOST}/luks-headers"
  install -d -m 0700 "$HEADER_DIR"
  if [[ "${APP_HA_DISK_TEST_MODE:-0}" != 1 ]]; then
    git -C "$DW_REPO_ROOT" check-ignore -q -- "${HEADER_DIR}/rpool-mirror${PAIR}-1.bin" ||
      dw_die "LUKS header backups under $HEADER_DIR are not ignored by Git"
  fi
  for member in 1 2; do
    local_header="${HEADER_DIR}/rpool-mirror${PAIR}-${member}.bin"
    remote_header="${DW_REMOTE_ROOT}/luks-header-mirror${PAIR}-${member}.bin"
    if [[ -e "$local_header" ]]; then
      mv -f -- "$local_header" "${local_header}.replaced-$(date +%Y%m%dT%H%M%S)"
    fi
    dw_on_host cat "$remote_header" >"$local_header" ||
      dw_die "could not copy $remote_header from $DW_HOST"
    chmod 0600 "$local_header"
    [[ "$(dw_on_host sha256sum "$remote_header" | awk '{print $1}')" == "$(dw_sha256 "$local_header")" ]] ||
      dw_die "the copied LUKS header backup $local_header does not match the host's copy"
  done
  HEADER_1="${HEADER_DIR}/rpool-mirror${PAIR}-1.bin"
  HEADER_2="${HEADER_DIR}/rpool-mirror${PAIR}-2.bin"

  dw_ready "record the new disks in /etc/crypttab, rebuild the initramfs for every installed kernel, prove it unlocks them at boot, and add them to rpool" \
    "a few minutes" ||
    dw_die "stopped before changing rpool; rerun this script to resume extra mirror $PAIR"
  dw_tool luks-add --pair "$PAIR" "$SERIAL_1" "$SERIAL_2" ||
    dw_die "adding extra mirror $PAIR failed; rerun this script to resume it"
  dw_on_host rm -f -- "$HELPER" \
    "${DW_REMOTE_ROOT}/luks-header-mirror${PAIR}-1.bin" \
    "${DW_REMOTE_ROOT}/luks-header-mirror${PAIR}-2.bin" ||
    printf 'WARNING: could not remove the console helper or header copies from %s\n' "$DW_HOST" >&2
else
  dw_tool clear-add "$SERIAL_1" "$SERIAL_2" ||
    dw_die "adding the unencrypted mirror failed"
fi

dw_section "New mirror added"
record_new_pair "$PAIR" "$SERIAL_1" "$SERIAL_2" "$CAPACITY_1" "$CAPACITY_2"
if [[ -n "$HEADER_1" ]]; then
  printf 'LUKS header backups: %s and %s\n' "$HEADER_1" "$HEADER_2"
fi

dw_collect_layout "$LAYOUT"
dw_section "rpool on $DW_HOST after the change"
dw_render_layout "$LAYOUT"

bmac_ui_result host "$DW_HOST" mirror_slot "$PAIR" serial_1 "$SERIAL_1" \
  serial_2 "$SERIAL_2" capacity_1 "$CAPACITY_1" capacity_2 "$CAPACITY_2"
if [[ "$ENCRYPTION" == luks ]]; then
  bmac_ui_next_step "Test a reboot of $DW_HOST by hand: migrate every production guest off it, reboot it gracefully, and enter the shared passphrase once at its console."
  printf '\nNEXT: test a reboot of %s by hand.\n' "$DW_HOST"
  printf 'The initramfs check above proved the next boot will try to unlock the new disks\n'
  printf 'with the one shared passphrase, but only a real reboot proves it end to end.\n'
  printf 'First gracefully migrate every production guest off %s, then gracefully\n' "$DW_HOST"
  printf 'reboot it and enter the shared passphrase once at its console.\n'
fi
