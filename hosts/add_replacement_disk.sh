#!/usr/bin/env bash

# Copyright (c) 2026 BEENTHERE VENTURES, INC.
# SPDX-License-Identifier: GPL-3.0-only

# Put a newly installed disk into a host's rpool mirror in place of a member
# whose disk was pulled. It finds the mirrors that are missing a member (or
# were detached down to one disk), offers the unused disks whose byte capacity
# is identical to the surviving member's, and after confirmation starts
# resilvering the chosen disk into that mirror. A boot-mirror replacement
# first receives the survivor's partition table and its own registered ESP, so
# either disk can boot the host. On an encrypted host the new member gets the
# shared rpool LUKS passphrase, which the operator types at the host console.
# The disk, LUKS, ESP, and zpool logic is lib/rpool_mirror.sh, the same code
# hosts/setup_proxmox_host.sh uses.

set -Eeuo pipefail
set +x
umask 077

DW_SCRIPT_NAME=add_replacement_disk.sh
SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
# shellcheck source=../lib/disk_workflows.sh
source "${SCRIPT_DIR}/../lib/disk_workflows.sh"

usage() {
  cat <<'EOF'
Usage: hosts/add_replacement_disk.sh [--host moxN]

Replaces the pulled member of one rpool mirror on the chosen host:

  1. lists the mirrors that are missing a member, and the unused disks whose
     byte capacity is identical to the surviving member's;
  2. for the boot mirror, copies the survivor's partition table to the new
     disk, then creates and registers its ESP and waits until it holds the
     same boot files as the survivor's;
  3. on an encrypted host, has you run a helper at the host console that asks
     for the shared rpool LUKS passphrase, proves it unlocks every rpool member,
     and encrypts the new disk with it, then records the disk for boot unlock;
  4. starts resilvering the new disk into the mirror (without waiting for it)
     and records its serial and capacity in env/moxN.conf.

A disk that failed but is still installed is not handled; pull it first.
A rerun resumes a replacement that stopped before the resilver started.
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
dw_collect_layout "$LAYOUT"

dw_section "Current rpool on $DW_HOST"
dw_render_layout "$LAYOUT"

[[ "$(dw_json "$LAYOUT" 'd["pool"]["removal_in_progress"]')" != true ]] ||
  dw_die "a vdev removal is in progress on $DW_HOST, and ZFS does not change mirror members until it finishes. Check it with hosts/list_disks_ready_for_physically_removal.sh --host $DW_HOST; to restore redundancy sooner, cancel it on the host with: zpool remove -s rpool"

CONF_PATH="$(dw_conf_path)"
CONF_PAIRS="{}"
if [[ -f "$CONF_PATH" ]]; then
  CONF_PAIRS="$(python3 "$DW_CONF_MIRRORS" show "$CONF_PATH")" ||
    dw_die "could not read NVMe mirror entries from $CONF_PATH"
fi

# One row per mirror that can take a replacement:
#   kind vdev role survivor survivor_size missing member pair conf_member resume
# kind is "replace" (a pulled member) or "attach" (detached to one disk);
# member is the LUKS member (A, B, or N-M) or "-" on an unencrypted host;
# resume names a disk an earlier run already prepared, or "-". Rows starting
# with NOTE explain mirrors this script leaves alone.
ANALYSIS="$(python3 - "$LAYOUT" "$CONF_PAIRS" <<'PY'
import json
import re
import sys

layout = json.load(open(sys.argv[1]))
conf = json.loads(sys.argv[2])
for vdev in layout["vdevs"]:
    members = vdev["members"]
    if vdev.get("replacing"):
        print(f"NOTE\t{vdev['name']} is already resilvering a replacement member; follow it with zpool status rpool on the host")
        continue
    online = [m for m in members if m["state"] == "ONLINE" and m["serial"]]
    installed = [m for m in members if m["state"] != "ONLINE" and m["serial"]]
    missing = [m for m in members if m["state"] != "ONLINE" and not m["serial"]]
    if installed:
        for m in installed:
            print(f"NOTE\t{vdev['name']} member {m['path']} (disk {m['serial']}) is {m['state']} but still installed; this script only replaces a member whose disk was pulled")
        continue
    if vdev["type"] == "mirror" and len(members) == 2 and len(online) == 1 and len(missing) == 1:
        kind, gone = "replace", missing[0]["path"]
    elif vdev["type"] == "disk" and len(online) == 1:
        kind, gone = "attach", "-"
    else:
        continue
    survivor = online[0]
    spec = pair = member = None
    if survivor["luks"]:
        mapper = (survivor["mapper"] or "").rsplit("/", 1)[-1]
        match = re.fullmatch(r"crypt-rpool-mirror([2-5])-([12])", mapper)
        if mapper in ("crypt-rpool-a", "crypt-rpool-b"):
            member = 2 if mapper == "crypt-rpool-a" else 1
            pair, spec = 1, "AB"[member - 1]
        elif match:
            pair, member = int(match.group(1)), 3 - int(match.group(2))
            spec = f"{pair}-{member}"
        else:
            print(f"NOTE\t{vdev['name']} survivor {survivor['path']} is not a BMAC rpool LUKS mapping")
            continue
    else:
        for number, values in conf.items():
            for index in (1, 2):
                if values.get(f"serial_{index}") == survivor["serial"]:
                    pair, member = int(number), 3 - index
        if pair is None and vdev["holds_esp"]:
            pair = 1
    resume = "-"
    if spec:
        wanted = "crypt-rpool-" + ({"A": "a", "B": "b"}.get(spec) or f"mirror{spec}")
        for row in layout["luks_mappings"]:
            if row["mapper"] == wanted and not row["in_pool"] and row["serial"] \
                    and row["size"] == survivor["disk_size"]:
                resume = row["serial"]
    if resume == "-" and vdev["holds_esp"]:
        for disk in layout.get("esp_only_disks", []):
            if disk["serial"] and disk["size"] == survivor["disk_size"]:
                resume = disk["serial"]
    print("\t".join(str(value) for value in (
        kind, vdev["name"], "boot" if vdev["holds_esp"] else "extra",
        survivor["serial"], survivor["disk_size"], gone, spec or "-",
        pair or "-", member or "-", resume,
    )))
PY
)" || dw_die "could not analyze the rpool layout of $DW_HOST"

dw_section "rpool mirrors on $DW_HOST that are missing a member"
ENTRIES=()
while IFS= read -r row; do
  [[ -n "$row" ]] || continue
  if [[ "$row" == NOTE$'\t'* ]]; then
    printf 'NOTE: %s\n' "${row#NOTE$'\t'}"
  else
    ENTRIES+=("$row")
  fi
done <<<"$ANALYSIS"
if ((${#ENTRIES[@]} == 0)); then
  printf 'No rpool mirror on %s is missing a member whose disk was pulled; nothing to replace.\n' "$DW_HOST"
  exit 0
fi
for index in "${!ENTRIES[@]}"; do
  IFS=$'\t' read -r KIND VDEV ROLE SURVIVOR SURVIVOR_SIZE MISSING MEMBER PAIR \
    CONF_MEMBER RESUME <<<"${ENTRIES[index]}"
  if [[ "$KIND" == replace ]]; then
    what="member $MISSING is gone"
  else
    what="it was detached down to one disk"
  fi
  label="extra mirror"
  [[ "$ROLE" != boot ]] || label="boot mirror"
  printf '  %d) %s (%s): %s; surviving disk %s has %s bytes\n' \
    "$((index + 1))" "$VDEV" "$label" "$what" "$SURVIVOR" "$SURVIVOR_SIZE"
done
CHOICE=1
if ((${#ENTRIES[@]} > 1)); then
  IFS= read -r -p "Which mirror gets a replacement disk this run [1-${#ENTRIES[@]}]? " CHOICE ||
    dw_die "input ended"
  [[ "$CHOICE" =~ ^[1-9][0-9]*$ ]] && ((CHOICE <= ${#ENTRIES[@]})) ||
    dw_die "no mirror was chosen"
fi
IFS=$'\t' read -r KIND VDEV ROLE SURVIVOR SURVIVOR_SIZE MISSING MEMBER PAIR \
  CONF_MEMBER RESUME <<<"${ENTRIES[CHOICE - 1]}"
[[ "$PAIR" != - ]] || PAIR=""
[[ "$CONF_MEMBER" != - ]] || CONF_MEMBER=""
ENCRYPTED=false
[[ "$MEMBER" == - ]] || ENCRYPTED=true

if [[ "$RESUME" != - ]]; then
  NEW_SERIAL="$RESUME"
  NEW_SIZE="$SURVIVOR_SIZE"
  printf '\nDisk %s was prepared for %s by an earlier run but has not joined it yet.\n' \
    "$NEW_SERIAL" "$VDEV"
  prompt_yes "Resume adding disk $NEW_SERIAL to $VDEV now?" ||
    dw_die "the prepared disk was left untouched; rerun to resume it"
else
  dw_section "Unused disks on $DW_HOST with exactly $SURVIVOR_SIZE bytes"
  mapfile -t CANDIDATES < <(python3 - "$LAYOUT" "$SURVIVOR_SIZE" <<'PY'
import json
import sys
layout = json.load(open(sys.argv[1]))
size = int(sys.argv[2])
open_luks = {row["serial"] for row in layout["luks_mappings"] if row["serial"]}
for disk in layout["unassigned_disks"]:
    if disk["size"] != size or not disk["serial"] or disk["mounted"]:
        continue
    if disk["serial"] in open_luks:
        continue
    contents = (
        "has partitions or signatures (they will be erased)"
        if disk["partitions_or_holders"] or disk["fstype"] else "blank"
    )
    print("\t".join(str(value) for value in (
        disk["disk"], disk["serial"], disk["size"],
        (disk["model"] or "-").replace("\t", " "), contents,
    )))
PY
  )
  if ((${#CANDIDATES[@]} == 0)); then
    dw_die "no unused, unmounted disk on $DW_HOST has exactly $SURVIVOR_SIZE bytes, the capacity of surviving disk $SURVIVOR; install an identical disk first"
  fi
  for index in "${!CANDIDATES[@]}"; do
    IFS=$'\t' read -r disk serial size model contents <<<"${CANDIDATES[index]}"
    printf '  %d) %-14s serial %-22s %16s bytes  %-20s %s\n' \
      "$((index + 1))" "$disk" "$serial" "$size" "$model" "$contents"
  done
  while true; do
    IFS= read -r -p "Number of the disk to add to $VDEV (q to quit): " CHOICE ||
      dw_die "input ended"
    [[ "$CHOICE" != q ]] || dw_die "no disks were changed"
    if [[ "$CHOICE" =~ ^[1-9][0-9]*$ ]] && ((CHOICE <= ${#CANDIDATES[@]})); then
      break
    fi
    printf 'Enter a number from the list.\n'
  done
  IFS=$'\t' read -r _ NEW_SERIAL NEW_SIZE _ _ <<<"${CANDIDATES[CHOICE - 1]}"

  dw_section "Checking disk $NEW_SERIAL on $DW_HOST"
  CHECKED="$(dw_tool check-new --expect-bytes "$SURVIVOR_SIZE" --match exact "$NEW_SERIAL")" ||
    dw_die "disk $NEW_SERIAL is not safe to use"
  printf '%s\n' "$CHECKED"
  if awk -F '\t' '$4 == "luks" { found=1 } END { exit !found }' <<<"$CHECKED"; then
    dw_die "disk $NEW_SERIAL already holds LUKS and is not erased automatically; if it is truly unused, erase it deliberately (cryptsetup erase, then wipefs --all) and rerun"
  fi

  action="add it to $VDEV on $DW_HOST in place of its pulled member"
  [[ "$KIND" == replace ]] || action="add it to $VDEV on $DW_HOST, which was detached down to disk $SURVIVOR"
  if [[ "$ROLE" == boot ]]; then
    action="${action}, giving it the boot partitions and ESP of disk $SURVIVOR"
  fi
  if [[ "$ENCRYPTED" == true ]]; then
    action="${action}, encrypted with the shared rpool LUKS passphrase (typed at the host console)"
  fi
  confirm_exact "ERASE disk $NEW_SERIAL and ${action}."
fi

if [[ "$ROLE" == boot ]]; then
  dw_section "Giving disk $NEW_SERIAL the boot-mirror partitions of $SURVIVOR"
  dw_tool boot-partition --survivor "$SURVIVOR" "$NEW_SERIAL" ||
    dw_die "partitioning disk $NEW_SERIAL failed; rerun this script to resume"
  dw_ready "create the ESP on disk $NEW_SERIAL, register it with proxmox-boot-tool, copy the boot loader, kernels, and initramfs images onto it, and wait until it matches the ESP of $SURVIVOR" \
    "a minute or two" ||
    dw_die "stopped before creating the ESP; rerun this script to resume"
  dw_tool boot-esp --survivor "$SURVIVOR" "$NEW_SERIAL" ||
    dw_die "the ESP of disk $NEW_SERIAL could not be created and synced; rerun this script to resume"
fi

LOCAL_HEADER=""
if [[ "$ENCRYPTED" == true ]]; then
  HELPER="${DW_REMOTE_ROOT}/app-ha-replace-member-${MEMBER}"
  if [[ "$MEMBER" == A || "$MEMBER" == B ]]; then
    REMOTE_HEADER="${DW_REMOTE_ROOT}/luks-header-${MEMBER}.bin"
    HEADER_NAME="rpool-${MEMBER}.bin"
    SIZE_ARGS=""
  else
    REMOTE_HEADER="${DW_REMOTE_ROOT}/luks-header-mirror${MEMBER}.bin"
    HEADER_NAME="rpool-mirror${MEMBER}.bin"
    SIZE_ARGS="--expect-bytes ${SURVIVOR_SIZE} --match exact"
  fi
  if dw_tool luks-check-member --member "$MEMBER" "$NEW_SERIAL" >/dev/null 2>&1; then
    printf '\nLUKS member %s is already open on disk %s with its header backed up.\n' \
      "$MEMBER" "$NEW_SERIAL"
  else
    # shellcheck disable=SC2016 # The program is expanded by the remote shell.
    dw_on_host bash -c '
set -Eeuo pipefail
umask 077
helper="$1"; tool="$2"; member="$3"; serial="$4"; size_args="$5"
{
  printf "#!/usr/bin/env bash\nset -Eeuo pipefail\nset +x\n"
  printf "exec %q luks-prepare-member --member %q --prompt %s %q\n" \
    "$tool" "$member" "$size_args" "$serial"
} >"$helper"
chmod 0700 "$helper"
' bash "$HELPER" "$DW_REMOTE_TOOL" "$MEMBER" "$NEW_SERIAL" "$SIZE_ARGS" ||
      dw_die "could not write the console helper on $DW_HOST"

    printf '\nMANUAL LUKS ACTION REQUIRED\n'
    printf 'At the %s console (iDRAC or physical), log in as root and run:\n  %s\n' \
      "$DW_HOST" "$HELPER"
    printf 'It asks you to type GO, then asks for the shared rpool LUKS passphrase with\n'
    printf 'the input hidden. It proves that passphrase unlocks every rpool LUKS member\n'
    printf 'before it encrypts the new disk. The passphrase is only ever typed at the\n'
    printf 'console; it never passes through this workstation or SSH.\n'
    while true; do
      printf '\nType GO once the console helper printed "LUKS member %s prepared successfully" (q to stop): ' \
        "$MEMBER"
      IFS= read -r answer || dw_die "input ended"
      [[ "$answer" != q ]] || dw_die "stopped; rerun this script to resume the replacement"
      [[ "$answer" == GO ]] || continue
      if dw_tool luks-check-member --member "$MEMBER" "$NEW_SERIAL"; then
        break
      fi
      printf 'The new disk is not fully prepared yet; rerun %s at the console.\n' "$HELPER"
    done
  fi

  # Keep the new member's LUKS header backup off-host beside the others.
  HEADER_DIR="${DW_ARTIFACTS_DIR}/${DW_HOST}/luks-headers"
  install -d -m 0700 "$HEADER_DIR"
  LOCAL_HEADER="${HEADER_DIR}/${HEADER_NAME}"
  if [[ "${APP_HA_DISK_TEST_MODE:-0}" != 1 ]]; then
    git -C "$DW_REPO_ROOT" check-ignore -q -- "$LOCAL_HEADER" ||
      dw_die "LUKS header backups under $HEADER_DIR are not ignored by Git"
  fi
  if [[ -e "$LOCAL_HEADER" ]]; then
    # The previous file belongs to the pulled disk; keep it for the record.
    mv -f -- "$LOCAL_HEADER" "${LOCAL_HEADER}.replaced-$(date +%Y%m%dT%H%M%S)"
  fi
  dw_on_host cat "$REMOTE_HEADER" >"$LOCAL_HEADER" ||
    dw_die "could not copy $REMOTE_HEADER from $DW_HOST"
  chmod 0600 "$LOCAL_HEADER"
  [[ "$(dw_on_host sha256sum "$REMOTE_HEADER" | awk '{print $1}')" == "$(dw_sha256 "$LOCAL_HEADER")" ]] ||
    dw_die "the copied LUKS header backup $LOCAL_HEADER does not match the host's copy"

  dw_ready "record the new member in /etc/crypttab, rebuild the initramfs for every installed kernel, prove it unlocks the member at boot, and start resilvering disk $NEW_SERIAL into $VDEV" \
    "a few minutes" ||
    dw_die "stopped before changing rpool; rerun this script to resume the replacement"
  dw_tool replace-member --survivor "$SURVIVOR" --member "$MEMBER" "$NEW_SERIAL" ||
    dw_die "adding disk $NEW_SERIAL to $VDEV failed; rerun this script to resume"
  dw_on_host rm -f -- "$HELPER" "$REMOTE_HEADER" ||
    printf 'WARNING: could not remove the console helper or header copy from %s\n' "$DW_HOST" >&2
else
  dw_tool replace-member --survivor "$SURVIVOR" "$NEW_SERIAL" ||
    dw_die "adding disk $NEW_SERIAL to $VDEV failed; rerun this script to resume"
fi

dw_section "Recording the replacement disk"
CONF_STATUS=4
if [[ ! -f "$CONF_PATH" ]]; then
  CONF_STATUS=3
elif [[ -n "$PAIR" && -n "$CONF_MEMBER" ]]; then
  CONF_STATUS=0
  dw_edit_conf replace --pair "$PAIR" --member "$CONF_MEMBER" --survivor "$SURVIVOR" \
    --serial "$NEW_SERIAL" --capacity "$NEW_SIZE" \
    --note "Replaced by hosts/add_replacement_disk.sh on $(date +%Y-%m-%d)" ||
    CONF_STATUS=$?
fi
case "$CONF_STATUS" in
  0) printf 'Recorded disk %s in %s.\n' "$NEW_SERIAL" "$CONF_PATH" ;;
  3) printf '%s is not on this workstation or lacks NVME_MIRROR_%s; record the lines below in it.\n' \
    "$CONF_PATH" "${PAIR:-N}" ;;
  4) printf 'The NVME_MIRROR entry of surviving disk %s was not found in %s; record the new disk in place of the pulled one by hand.\n' \
    "$SURVIVOR" "$CONF_PATH" ;;
  *) printf 'WARNING: %s was not updated; record the lines below in it by hand.\n' "$CONF_PATH" >&2 ;;
esac
printf '\nReplacement disk on %s:\n' "$DW_HOST"
printf '  NVME_MIRROR_%s_SERIAL_%s=%s\n' "${PAIR:-N}" "${CONF_MEMBER:-M}" "$NEW_SERIAL"
printf '  NVME_MIRROR_%s_CAPACITY_BYTES_%s=%s\n' "${PAIR:-N}" "${CONF_MEMBER:-M}" "$NEW_SIZE"
if [[ -n "$LOCAL_HEADER" ]]; then
  printf 'LUKS header backup: %s\n' "$LOCAL_HEADER"
fi

dw_collect_layout "$LAYOUT"
dw_section "rpool on $DW_HOST after the change"
dw_render_layout "$LAYOUT"

dw_section "Resilver underway"
printf 'ZFS is resilvering %s onto disk %s in the background; this script does not wait for it.\n' \
  "$VDEV" "$NEW_SERIAL"
printf 'Follow it with diagnostics/show_proxmox_host_state.sh %s (or zpool status rpool on the host).\n' \
  "$DW_HOST"
if [[ "$ROLE" == boot ]]; then
  printf 'Both boot-mirror disks hold a registered ESP with the same boot loader, kernels,\n'
  printf 'and initramfs images; once the resilver finishes, either disk can boot %s alone.\n' "$DW_HOST"
fi
printf '\nOnce the resilver finishes, test a reboot of %s by hand: first gracefully\n' "$DW_HOST"
printf 'migrate every production guest off it, then gracefully reboot it'
if [[ "$ENCRYPTED" == true ]]; then
  printf ' and enter the\nshared passphrase once at its console'
fi
printf '.\n'
