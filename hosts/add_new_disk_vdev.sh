#!/usr/bin/env bash

# Copyright (c) 2026 BEENTHERE VENTURES, INC.
# SPDX-License-Identifier: GPL-3.0-only

# Add two newly installed, identical-capacity disks to a host's rpool as one
# new ZFS mirror vdev. On an encrypted host both disks become whole-disk LUKS2
# with the host's single shared rpool passphrase, which the operator types at
# the host console; on an unencrypted host they stay unencrypted. The disk,
# LUKS, and zpool logic is lib/rpool_mirror.sh, the same code
# hosts/setup_proxmox_host.sh uses for extra mirrors at install time.

set -Eeuo pipefail
set +x
umask 077

DW_SCRIPT_NAME=add_new_disk_vdev.sh
SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
# shellcheck source=../lib/disk_workflows.sh
source "${SCRIPT_DIR}/../lib/disk_workflows.sh"

MAX_MIRRORS=5

usage() {
  cat <<'EOF'
Usage: hosts/add_new_disk_vdev.sh [--host moxN]

Adds two new disks to the chosen host's rpool as one new mirror vdev:

  1. lists the disks that are not part of rpool and asks for two with
     identical byte capacity;
  2. on an encrypted host, has you run a helper at the host console that asks
     for the shared rpool LUKS passphrase, proves it unlocks every existing
     rpool member, and formats both disks with it;
  3. records the disks in /etc/crypttab, rebuilds and verifies the initramfs,
     and adds them to rpool;
  4. records their serials and capacities in env/moxN.conf as the next free
     NVME_MIRROR_N pair, and prints them.

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
dw_collect_layout "$LAYOUT"

dw_section "Current rpool on $DW_HOST"
dw_render_layout "$LAYOUT"

ENCRYPTION="$(dw_json "$LAYOUT" '("luks" if d["vdevs"] and all(v["luks"] for v in d["vdevs"]) else "clear" if not any(v["luks"] for v in d["vdevs"]) else "mixed")')" ||
  dw_die "could not read the encryption of rpool"
[[ "$ENCRYPTION" != mixed ]] ||
  dw_die "rpool on $DW_HOST mixes LUKS and unencrypted vdevs; resolve that first"
VDEV_COUNT="$(dw_json "$LAYOUT" 'len(d["vdevs"])')"

# Pair slots already used by env/moxN.conf, or by LUKS mappings and crypttab
# entries on the host. A decommissioned pair's slot is free again only after
# hosts/list_disks_ready_for_physically_removal.sh retired it.
CONF_PAIRS="[]"
CONF_PATH="$(dw_conf_path)"
if [[ -f "$CONF_PATH" ]]; then
  CONF_PAIRS="$(python3 "$DW_CONF_MIRRORS" show "$CONF_PATH" |
    python3 -c 'import json,sys; print(json.dumps(sorted(int(k) for k in json.load(sys.stdin))))')" ||
    dw_die "could not read NVMe mirror entries from $CONF_PATH"
else
  printf '\nNOTE: %s is not on this workstation; the new serials will be printed for you to record.\n' \
    "$CONF_PATH"
fi

# A pair whose two LUKS mappings are open but not in rpool was prepared by an
# interrupted run; offer to finish adding it.
RESUME="$(python3 - "$LAYOUT" <<'PY'
import json
import re
import sys
layout = json.load(open(sys.argv[1]))
pairs = {}
for row in layout["luks_mappings"]:
    match = re.fullmatch(r"crypt-rpool-mirror([2-5])-([12])", row["mapper"])
    if match and not row["in_pool"]:
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
  PAIR="$(python3 - "$LAYOUT" "$CONF_PAIRS" <<'PY'
import json
import re
import sys
layout = json.load(open(sys.argv[1]))
used = set(json.loads(sys.argv[2]))
for row in layout["luks_mappings"] + layout["crypttab"]:
    match = re.fullmatch(r"crypt-rpool-mirror([2-5])-[12]", row["mapper"])
    if match:
        used.add(int(match.group(1)))
free = [pair for pair in (2, 3, 4, 5) if pair not in used]
print(free[0] if free else "")
PY
)"
  [[ -n "$PAIR" ]] ||
    dw_die "NVME_MIRROR slots 2-5 are all in use in $CONF_PATH or on $DW_HOST"

  dw_section "Disks on $DW_HOST that are not part of rpool"
  mapfile -t CANDIDATES < <(python3 - "$LAYOUT" <<'PY'
import json
import sys
for disk in json.load(open(sys.argv[1]))["unassigned_disks"]:
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
    dw_die "fewer than two disks outside rpool were found on $DW_HOST; install two identical disks first"
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
  while true; do
    choose_disk first
    SERIAL_1="$CHOSEN_SERIAL"
    CAPACITY_1="$CHOSEN_SIZE"
    choose_disk second
    SERIAL_2="$CHOSEN_SERIAL"
    CAPACITY_2="$CHOSEN_SIZE"
    if [[ "$SERIAL_1" == "$SERIAL_2" ]]; then
      printf 'Choose two different disks.\n'
    elif [[ "$CAPACITY_1" != "$CAPACITY_2" ]]; then
      printf 'Those disks differ in capacity (%s vs %s bytes); a mirror needs identical disks.\n' \
        "$CAPACITY_1" "$CAPACITY_2"
    else
      break
    fi
  done

  dw_section "Checking the chosen disks on $DW_HOST"
  CHECKED="$(dw_tool check-new --require-equal "$SERIAL_1" "$SERIAL_2")" ||
    dw_die "the chosen disks are not safe to use"
  printf '%s\n' "$CHECKED"
  if awk -F '\t' '$4 == "luks" { found=1 } END { exit !found }' <<<"$CHECKED"; then
    [[ "$ENCRYPTION" == luks ]] ||
      dw_die "a chosen disk already holds LUKS; this unencrypted host will not erase it"
    printf 'NOTE: a chosen disk already holds LUKS. It is reused only if the shared rpool passphrase opens it; otherwise nothing is changed.\n'
  fi

  if [[ "$ENCRYPTION" == luks ]]; then
    confirm_exact "ERASE disks $SERIAL_1 and $SERIAL_2 on $DW_HOST, encrypt both with the shared rpool LUKS passphrase (typed at the host console), and add them to rpool as a new mirror recorded as NVME_MIRROR_${PAIR}."
  else
    confirm_exact "ERASE disks $SERIAL_1 and $SERIAL_2 on $DW_HOST and add them to its unencrypted rpool as a new mirror recorded as NVME_MIRROR_${PAIR}."
  fi
fi

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
  printf "exec %q luks-prepare --pair %q --prompt --require-equal %q %q\n" \
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
    printf '\nType GO once the console helper printed "LUKS members prepared successfully" (q to stop): '
    IFS= read -r answer || dw_die "input ended"
    [[ "$answer" != q ]] || dw_die "stopped; rerun this script to resume extra mirror $PAIR"
    [[ "$answer" == GO ]] || continue
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
  dw_tool clear-add --require-equal "$SERIAL_1" "$SERIAL_2" ||
    dw_die "adding the unencrypted mirror failed"
fi

dw_section "Recording the new mirror"
CONF_STATUS=0
dw_edit_conf assign --pair "$PAIR" \
  --serial "$SERIAL_1" --serial "$SERIAL_2" \
  --capacity "$CAPACITY_1" --capacity "$CAPACITY_2" \
  --note "Added to rpool by hosts/add_new_disk_vdev.sh on $(date +%Y-%m-%d)" ||
  CONF_STATUS=$?
case "$CONF_STATUS" in
  0) printf 'Recorded NVME_MIRROR_%s in %s.\n' "$PAIR" "$CONF_PATH" ;;
  3) printf '%s is not on this workstation; add the lines below to it.\n' "$CONF_PATH" ;;
  *) printf 'WARNING: %s was not updated; add the lines below to it by hand.\n' "$CONF_PATH" >&2 ;;
esac
printf '\nNew mirror disks on %s:\n' "$DW_HOST"
printf '  NVME_MIRROR_%s_SERIAL_1=%s\n' "$PAIR" "$SERIAL_1"
printf '  NVME_MIRROR_%s_SERIAL_2=%s\n' "$PAIR" "$SERIAL_2"
printf '  NVME_MIRROR_%s_CAPACITY_BYTES_1=%s\n' "$PAIR" "$CAPACITY_1"
printf '  NVME_MIRROR_%s_CAPACITY_BYTES_2=%s\n' "$PAIR" "$CAPACITY_2"
if [[ -n "$HEADER_1" ]]; then
  printf 'LUKS header backups: %s and %s\n' "$HEADER_1" "$HEADER_2"
fi

dw_collect_layout "$LAYOUT"
dw_section "rpool on $DW_HOST after the change"
dw_render_layout "$LAYOUT"

if [[ "$ENCRYPTION" == luks ]]; then
  printf '\nNEXT: test a reboot of %s by hand.\n' "$DW_HOST"
  printf 'The initramfs check above proved the next boot will try to unlock the new disks\n'
  printf 'with the one shared passphrase, but only a real reboot proves it end to end.\n'
  printf 'First gracefully migrate every production guest off %s, then gracefully\n' "$DW_HOST"
  printf 'reboot it and enter the shared passphrase once at its console.\n'
fi
