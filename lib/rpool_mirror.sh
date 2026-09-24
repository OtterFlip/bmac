#!/usr/bin/env bash

# Copyright (c) 2026 BEENTHERE VENTURES, INC.
# SPDX-License-Identifier: GPL-3.0-only

# Root-only rpool mirror operations for one Proxmox host. This is the single
# copy of the disk, LUKS, and zpool logic used by hosts/setup_proxmox_host.sh,
# hosts/add_new_disk_vdev.sh, and
# hosts/list_disks_ready_for_physically_removal.sh. Those workstation scripts
# install it on the host as /usr/local/sbin/app-ha-rpool-mirror.
#
# Every subcommand selects disks by serial and re-proves their identity before
# changing anything. Extra mirror pair N always uses the LUKS mappings
# crypt-rpool-mirrorN-1 and crypt-rpool-mirrorN-2.

set -Eeuo pipefail
set +x
umask 077

POOL=rpool
TEST_MODE="${APP_HA_RPOOL_MIRROR_TEST_MODE:-0}"
if [[ "$TEST_MODE" == 1 ]]; then
  CRYPTTAB="${APP_HA_CRYPTTAB:?APP_HA_CRYPTTAB is required in test mode}"
  HEADER_DIR="${APP_HA_HEADER_DIR:?APP_HA_HEADER_DIR is required in test mode}"
  BY_ID_DIR="${APP_HA_BY_ID_DIR:?APP_HA_BY_ID_DIR is required in test mode}"
  INITRD="${APP_HA_INITRD:?APP_HA_INITRD is required in test mode}"
  WORK_ROOT="${APP_HA_WORK_DIR:?APP_HA_WORK_DIR is required in test mode}"
else
  CRYPTTAB=/etc/crypttab
  HEADER_DIR=/root
  BY_ID_DIR=/dev/disk/by-id
  INITRD="/boot/initrd.img-$(uname -r)"
  WORK_ROOT=/var/tmp
fi
SHARED_KEY_ID=app-ha-rpool
SHARED_OPTIONS=luks,initramfs,nofail,keyscript=decrypt_keyctl
PLAIN_OPTIONS=luks,initramfs,nofail

KEY_FILE=""
PASSPHRASE=""
EXPECT_BYTES=""
MATCH=""
PAIR=""
declare -a SERIALS=()

die() {
  printf 'ERROR: %s\n' "$*" >&2
  exit 1
}

usage() {
  cat <<'EOF'
Usage: app-ha-rpool-mirror COMMAND [OPTIONS] ARGS

Commands:
  check-new [SIZE] SERIAL_1 SERIAL_2
      Prove two disks are safe to erase for a new mirror. Prints one
      "serial<TAB>disk<TAB>bytes<TAB>luks|fresh" line per disk.
  luks-prepare --pair N (--key-file FILE | --prompt) [SIZE] SERIAL_1 SERIAL_2
      Interactive, at the host console. Proves the shared rpool LUKS
      passphrase against every existing member, formats each fresh disk as
      whole-disk LUKS2 with it, opens both mappings, and backs up both headers
      to /root/luks-header-mirrorN-{1,2}.bin.
  luks-check-prepared --pair N [--expect-bytes BYTES] SERIAL_1 SERIAL_2
      Verify both prepared mappings, their backing disks, and header backups.
  luks-backup-headers --pair N SERIAL_1 SERIAL_2
      Refresh the header backups of an existing pair.
  luks-add --pair N SERIAL_1 SERIAL_2
      Record both mappings in /etc/crypttab in the form the host already uses,
      rebuild and verify the initramfs, then add them to rpool as one mirror.
      Safe to rerun after the mirror was added.
  clear-add [SIZE] SERIAL_1 SERIAL_2
      Erase two disks and add them to an unencrypted rpool as one mirror.
      Safe to rerun after the mirror was added.
  retire-luks MAPPER...
      After a completed vdev removal, close each crypt-rpool-mirrorN-M mapping,
      remove it from /etc/crypttab, and rebuild and verify the initramfs.

SIZE is one of:
  --require-equal                      both disks have identical byte counts
  --expect-bytes BYTES --match exact   each disk has exactly BYTES
  --expect-bytes BYTES --match within-1pct
                                       each disk has at least 99% of BYTES
EOF
}

block_device_exists() {
  [[ "$TEST_MODE" == 1 ]] || [[ -b "$1" ]]
}

canonical() {
  local resolved
  resolved="$(readlink -m -- "$1")" && [[ -n "$resolved" ]] ||
    die "could not resolve the path $1"
  printf '%s\n' "$resolved"
}

validate_serial() {
  [[ "$1" =~ ^[A-Za-z0-9._:+-]+$ ]] || die "unsafe disk serial: $1"
}

disk_by_serial() {
  local serial="$1"
  local -a matches=()
  mapfile -t matches < <(
    lsblk -dnpo PATH,SERIAL | awk -v serial="$serial" '$2 == serial { print $1 }'
  )
  ((${#matches[@]} == 1)) ||
    die "expected exactly one disk with serial $serial; found ${#matches[@]}"
  printf '%s\n' "${matches[0]}"
}

mapper_for() {
  printf 'crypt-rpool-mirror%s-%s\n' "$PAIR" "$1"
}

mapper_active() {
  cryptsetup status "$1" >/dev/null 2>&1
}

mapper_backing() {
  cryptsetup status "$1" | awk '$1 == "device:" { print $2; exit }'
}

pool_leaves() {
  zpool status -P "$POOL" | awk '$1 ~ /^\/dev\// { print $1 }'
}

pool_contains() {
  local wanted leaves leaf
  wanted="$(canonical "$1")" || exit 1
  leaves="$(pool_leaves)" || die "could not read rpool status"
  while IFS= read -r leaf; do
    [[ -n "$leaf" ]] || continue
    [[ "$(canonical "$leaf")" == "$wanted" ]] && return 0
  done <<<"$leaves"
  return 1
}

# Parse the options shared by several subcommands, leaving serials in SERIALS.
parse_common() {
  SERIALS=()
  while (($#)); do
    case "$1" in
      --pair)
        (($# >= 2)) || die "--pair requires a value"
        [[ "$2" =~ ^[2-5]$ ]] || die "--pair must be 2 through 5"
        PAIR="$2"
        shift 2
        ;;
      --key-file)
        (($# >= 2)) || die "--key-file requires a path"
        KEY_FILE="$2"
        shift 2
        ;;
      --prompt)
        KEY_FILE=""
        PASSPHRASE_PROMPT=1
        shift
        ;;
      --require-equal)
        MATCH=equal
        shift
        ;;
      --expect-bytes)
        (($# >= 2)) || die "--expect-bytes requires a byte count"
        [[ "$2" =~ ^[1-9][0-9]*$ ]] || die "--expect-bytes must be a positive byte count"
        EXPECT_BYTES="$2"
        shift 2
        ;;
      --match)
        (($# >= 2)) || die "--match requires exact or within-1pct"
        [[ "$2" == exact || "$2" == within-1pct ]] ||
          die "--match must be exact or within-1pct"
        MATCH="$2"
        shift 2
        ;;
      -*)
        die "unknown option: $1"
        ;;
      *)
        validate_serial "$1"
        SERIALS+=("$1")
        shift
        ;;
    esac
  done
  ((${#SERIALS[@]} == 2)) || die "exactly two disk serials are required"
  [[ "${SERIALS[0]}" != "${SERIALS[1]}" ]] || die "the two disk serials must differ"
  if [[ "$MATCH" == exact || "$MATCH" == within-1pct ]]; then
    [[ -n "$EXPECT_BYTES" ]] || die "--match $MATCH requires --expect-bytes"
  fi
}

require_pair() {
  [[ -n "$PAIR" ]] || die "--pair is required"
}

# Print "serial disk bytes luks|fresh" for each serial after proving that the
# disk is not part of rpool, is not mounted, and has the expected capacity.
check_new_disks() {
  local serial disk size state device resolved pool
  local -a sizes=() disks=()
  [[ -n "$MATCH" ]] || die "a disk size requirement is required"
  pool="$(zpool status -LP "$POOL")" || die "could not read rpool status"
  for serial in "${SERIALS[@]}"; do
    disk="$(disk_by_serial "$serial")" || exit 1
    ((${#disks[@]} == 0)) || [[ "${disks[0]}" != "$disk" ]] ||
      die "both serials resolve to $disk"
    size="$(blockdev --getsize64 "$disk")" || die "could not read the size of $disk"
    case "$MATCH" in
      exact)
        [[ "$size" == "$EXPECT_BYTES" ]] ||
          die "disk $serial ($disk) has $size bytes, not the expected $EXPECT_BYTES"
        ;;
      within-1pct)
        ((size * 100 >= EXPECT_BYTES * 99)) ||
          die "disk $serial ($disk) has $size bytes, less than 99% of $EXPECT_BYTES"
        ;;
    esac
    while IFS= read -r device; do
      [[ -n "$device" ]] || continue
      resolved="$(canonical "$device")" || exit 1
      if awk -v target="$resolved" '$1 == target { found=1 } END { exit !found }' <<<"$pool"; then
        die "disk $serial ($disk) is already part of rpool as $device"
      fi
    done < <(lsblk -nrpo NAME "$disk")
    if lsblk -nrpo MOUNTPOINT "$disk" | awk 'NF { found=1 } END { exit !found }'; then
      die "disk $serial ($disk) has a mounted filesystem or active swap"
    fi
    state=fresh
    ! cryptsetup isLuks "$disk" >/dev/null 2>&1 || state=luks
    sizes+=("$size")
    disks+=("$disk")
    printf '%s\t%s\t%s\t%s\n' "$serial" "$disk" "$size" "$state"
  done
  if [[ "$MATCH" == equal ]]; then
    [[ "${sizes[0]}" == "${sizes[1]}" ]] ||
      die "the disks differ in capacity: ${SERIALS[0]} has ${sizes[0]} bytes and ${SERIALS[1]} has ${sizes[1]} bytes"
  fi
}

# Run cryptsetup ACTION with the shared passphrase from the key file or from
# the passphrase entered at the console, without ever writing the latter out.
with_shared_key() {
  local action="$1"
  shift
  if [[ -n "$KEY_FILE" ]]; then
    cryptsetup "$action" --key-file="$KEY_FILE" "$@"
  else
    printf '%s' "$PASSPHRASE" | cryptsetup "$action" --key-file=- "$@"
  fi
}

# Print "mapper<TAB>backing" for every LUKS member of rpool. Refuses a pool
# that mixes encrypted and unencrypted members.
existing_luks_members() {
  local leaves leaf mapper backing plain=0 luks=0
  local -a rows=()
  leaves="$(pool_leaves)" || die "could not read rpool status"
  while IFS= read -r leaf; do
    [[ -n "$leaf" ]] || continue
    if [[ "$leaf" != /dev/mapper/* ]]; then
      plain=$((plain + 1))
      continue
    fi
    mapper="${leaf#/dev/mapper/}"
    backing="$(mapper_backing "$mapper")" ||
      die "could not read the backing device of $mapper"
    block_device_exists "$backing" ||
      die "backing device for $mapper is unavailable"
    rows+=("${mapper}"$'\t'"${backing}")
    luks=$((luks + 1))
  done <<<"$leaves"
  ((luks > 0)) || die "rpool has no LUKS members; this host is not encrypted"
  ((plain == 0)) || die "rpool mixes LUKS and unencrypted members"
  printf '%s\n' "${rows[@]}"
}

shared_key_opens_all_members() {
  local row mapper backing
  local -a rows=()
  mapfile -t rows <<<"$1"
  for row in "${rows[@]}"; do
    [[ -n "$row" ]] || continue
    IFS=$'\t' read -r mapper backing <<<"$row"
    with_shared_key open --test-passphrase "$backing" </dev/null >/dev/null 2>&1 || {
      printf 'The passphrase does not unlock existing rpool member %s.\n' "$mapper" >&2
      return 1
    }
  done
}

obtain_shared_key() {
  local members attempt
  members="$(existing_luks_members)" || exit 1
  if [[ -n "$KEY_FILE" ]]; then
    [[ -f "$KEY_FILE" && ! -L "$KEY_FILE" && -s "$KEY_FILE" ]] ||
      die "LUKS key file is missing, empty, or a symlink: $KEY_FILE"
    local expected_owner=root:root:600 owner
    owner="$(stat -c '%U:%G:%a' "$KEY_FILE")" || die "could not inspect $KEY_FILE"
    [[ "$TEST_MODE" != 1 ]] || expected_owner="${owner%:*}:600"
    [[ "$owner" == "$expected_owner" ]] ||
      die "LUKS key file must be owned by root with mode 0600: $KEY_FILE"
    shared_key_opens_all_members "$members" ||
      die "the key file does not hold the shared rpool passphrase"
    return
  fi
  for attempt in 1 2 3; do
    printf 'Enter the shared rpool LUKS passphrase (input is hidden): ' >&2
    IFS= read -rs PASSPHRASE || die "input ended before a passphrase was entered"
    printf '\n' >&2
    if [[ -z "$PASSPHRASE" ]]; then
      printf 'The passphrase must not be empty.\n' >&2
      continue
    fi
    if shared_key_opens_all_members "$members"; then
      printf 'The passphrase unlocks every existing rpool LUKS member.\n' >&2
      return
    fi
    printf 'Attempt %s of 3 failed.\n' "$attempt" >&2
  done
  PASSPHRASE=""
  die "the shared rpool passphrase was not entered; nothing was changed"
}

command_check_new() {
  parse_common "$@"
  check_new_disks
}

command_luks_prepare() {
  local confirmation checked serial disk size state index mapper header backing
  parse_common "$@"
  require_pair
  [[ -n "$KEY_FILE" || "${PASSPHRASE_PROMPT:-0}" == 1 ]] ||
    die "luks-prepare requires --key-file FILE or --prompt"

  checked="$(check_new_disks)" || exit 1
  printf 'Extra mirror %s will use these disks:\n%s\n' "$PAIR" "$checked"
  printf 'Fresh disks are erased and formatted as LUKS2. Type GO to format extra mirror %s.\n> ' "$PAIR"
  IFS= read -r confirmation || die "input ended before confirmation"
  [[ "$confirmation" == GO ]] || die "confirmation did not match GO; nothing was changed"

  # Proving the passphrase against every existing member is what keeps a
  # single console prompt (decrypt_keyctl) able to unlock the whole pool.
  obtain_shared_key

  local row
  local -a rows=()
  mapfile -t rows <<<"$checked"
  index=0
  for row in "${rows[@]}"; do
    IFS=$'\t' read -r serial disk size state <<<"$row"
    index=$((index + 1))
    mapper="$(mapper_for "$index")"
    header="${HEADER_DIR}/luks-header-mirror${PAIR}-${index}.bin"
    if [[ "$state" == luks ]]; then
      with_shared_key open --test-passphrase "$disk" >/dev/null 2>&1 ||
        die "disk $serial already holds LUKS that the shared passphrase does not open; refusing to reuse or erase it"
      printf 'Reusing LUKS on %s (%s), which opens with the shared passphrase.\n' "$serial" "$disk"
    else
      wipefs --all --force "$disk"
      sgdisk --zap-all "$disk"
      udevadm settle
      with_shared_key luksFormat --batch-mode --type luks2 "$disk"
    fi
    if ! mapper_active "$mapper"; then
      with_shared_key open "$disk" "$mapper"
    fi
    backing="$(mapper_backing "$mapper")" || die "could not read the backing device of $mapper"
    [[ "$(canonical "$backing")" == "$(canonical "$disk")" ]] ||
      die "$mapper is backed by $backing, not $disk"
    rm -f -- "$header"
    cryptsetup luksHeaderBackup "$disk" --header-backup-file "$header"
    chmod 0600 "$header"
  done
  PASSPHRASE=""
  printf 'Extra mirror %s LUKS members prepared successfully.\n' "$PAIR"
}

command_luks_check_prepared() {
  local index serial disk mapper header backing size difference larger
  local -a sizes=()
  parse_common --require-equal "$@"
  require_pair
  for index in 1 2; do
    serial="${SERIALS[index - 1]}"
    disk="$(disk_by_serial "$serial")" || exit 1
    mapper="$(mapper_for "$index")"
    header="${HEADER_DIR}/luks-header-mirror${PAIR}-${index}.bin"
    mapper_active "$mapper" || die "$mapper is not open"
    [[ -s "$header" ]] || die "header backup $header is missing"
    backing="$(mapper_backing "$mapper")" || die "could not read the backing device of $mapper"
    [[ "$(canonical "$backing")" == "$(canonical "$disk")" ]] ||
      die "$mapper is backed by $backing, not $serial ($disk)"
    size="$(blockdev --getsize64 "/dev/mapper/$mapper")"
    if [[ -n "$EXPECT_BYTES" ]]; then
      ((size * 100 >= EXPECT_BYTES * 98)) ||
        die "$mapper has $size bytes, less than 98% of $EXPECT_BYTES"
    fi
    sizes+=("$size")
  done
  if ((sizes[0] >= sizes[1])); then
    difference=$((sizes[0] - sizes[1]))
    larger=${sizes[0]}
  else
    difference=$((sizes[1] - sizes[0]))
    larger=${sizes[1]}
  fi
  ((difference * 100 <= larger)) || die "the two mappings differ in size by more than 1%"
  printf 'Extra mirror %s mappings, backing disks, and header backups are verified.\n' "$PAIR"
}

command_luks_backup_headers() {
  local index serial disk mapper header backing
  parse_common --require-equal "$@"
  require_pair
  for index in 1 2; do
    serial="${SERIALS[index - 1]}"
    disk="$(disk_by_serial "$serial")" || exit 1
    mapper="$(mapper_for "$index")"
    header="${HEADER_DIR}/luks-header-mirror${PAIR}-${index}.bin"
    mapper_active "$mapper" || die "$mapper is not open"
    backing="$(mapper_backing "$mapper")" || die "could not read the backing device of $mapper"
    [[ "$(canonical "$backing")" == "$(canonical "$disk")" ]] ||
      die "$mapper is backed by $backing, not $serial ($disk)"
    rm -f -- "$header"
    cryptsetup luksHeaderBackup "$disk" --header-backup-file "$header"
    chmod 0600 "$header"
  done
}

# Decide how new crypttab entries must look from the host's existing rpool
# entries: the shared single-prompt form once setup enabled it, else plain.
crypttab_form() {
  local exclude_pattern="$1"
  [[ -f "$CRYPTTAB" ]] || {
    printf 'plain\n'
    return
  }
  awk -v exclude="$exclude_pattern" -v key_id="$SHARED_KEY_ID" '
    $1 ~ /^crypt-rpool-/ && $1 !~ exclude {
      if ($3 == key_id && $4 ~ /(^|,)keyscript=decrypt_keyctl(,|$)/) shared++
      else plain++
    }
    END {
      if (shared && plain) { print "mixed"; exit }
      print (shared ? "shared" : "plain")
    }
  ' "$CRYPTTAB"
}

replace_crypttab_entries() {
  # Arguments: remove-pattern, then zero or more complete lines to append.
  local pattern="$1" work
  shift
  work="$(mktemp "${CRYPTTAB}.app-ha.XXXXXX")"
  if [[ -f "$CRYPTTAB" ]]; then
    awk -v pattern="$pattern" '$1 !~ pattern' "$CRYPTTAB" >"$work"
  fi
  if (($# > 0)); then
    printf '%s\n' "$@" >>"$work"
  fi
  chmod 0600 "$work"
  mv -f -- "$work" "$CRYPTTAB"
}

rebuild_initramfs() {
  update-initramfs -u -k all
  proxmox-boot-tool refresh
}

# Extract the running kernel's initramfs and require its cryptroot crypttab to
# list (or not list) each mapper, so boot unlock is proven before the pool
# depends on a new vdev.
verify_initramfs_mappers() {
  local expectation="$1" form="$2" work crypttab mapper
  shift 2
  work="$(mktemp -d "${WORK_ROOT}/app-ha-initramfs.XXXXXX")"
  if ! unmkinitramfs "$INITRD" "$work" >/dev/null 2>&1; then
    rm -rf -- "$work"
    die "could not unpack $INITRD to verify boot-time unlock"
  fi
  crypttab="$(find "$work" -path '*/cryptroot/crypttab' -type f -print -quit)"
  for mapper in "$@"; do
    if [[ "$expectation" == present ]]; then
      [[ -n "$crypttab" ]] &&
        awk -v mapper="$mapper" '$1 == mapper { found=1 } END { exit !found }' "$crypttab" || {
        rm -rf -- "$work"
        die "the rebuilt initramfs would not unlock $mapper at boot"
      }
    elif [[ -n "$crypttab" ]] &&
      awk -v mapper="$mapper" '$1 == mapper { found=1 } END { exit !found }' "$crypttab"; then
      rm -rf -- "$work"
      die "the rebuilt initramfs still unlocks retired $mapper at boot"
    fi
  done
  if [[ "$form" == shared ]] &&
    [[ -z "$(find "$work" -name decrypt_keyctl -type f -print -quit)" ]]; then
    rm -rf -- "$work"
    die "the rebuilt initramfs lacks decrypt_keyctl for single-prompt unlock"
  fi
  rm -rf -- "$work"
}

uniform_ashift() {
  local -a values=()
  mapfile -t values < <(zdb -C "$POOL" | awk '$1 == "ashift:" { print $2 }' | sort -u)
  ((${#values[@]} == 1)) && [[ "${values[0]}" =~ ^[0-9]+$ ]] ||
    die "rpool vdevs do not share one ashift (${values[*]:-none}); a mixed pool cannot remove vdevs later"
  printf '%s\n' "${values[0]}"
}

same_mirror() {
  # Both members appear under one top-level mirror of rpool.
  zpool status -P "$POOL" | awk -v one="$1" -v two="$2" '
    $1 ~ /^mirror-/ { mirror=$1 }
    $1 == one { a=mirror }
    $1 == two { b=mirror }
    END { exit !(a != "" && a == b) }
  '
}

pool_healthy() {
  zpool status -x "$POOL" | grep -Fq "pool '$POOL' is healthy"
}

command_luks_add() {
  local index serial disk mapper backing uuid form ashift in_pool=0
  local -a mappers=() lines=()
  parse_common --require-equal "$@"
  require_pair
  for index in 1 2; do
    serial="${SERIALS[index - 1]}"
    disk="$(disk_by_serial "$serial")" || exit 1
    mapper="$(mapper_for "$index")"
    mapper_active "$mapper" || die "$mapper is not open"
    backing="$(mapper_backing "$mapper")" || die "could not read the backing device of $mapper"
    [[ "$(canonical "$backing")" == "$(canonical "$disk")" ]] ||
      die "$mapper is backed by $backing, not $serial ($disk)"
    uuid="$(cryptsetup luksUUID "$disk")" || die "could not read the LUKS UUID of $disk"
    mappers+=("$mapper")
    if pool_contains "/dev/mapper/$mapper"; then
      in_pool=$((in_pool + 1))
    fi
    lines+=("${mapper}"$'\t'"UUID=${uuid}")
  done
  ((in_pool != 1)) || die "rpool contains only one member of extra mirror $PAIR"
  if ((in_pool == 0)); then
    existing_luks_members >/dev/null
  fi

  form="$(crypttab_form "^crypt-rpool-mirror${PAIR}-[12]\$")"
  [[ "$form" != mixed ]] ||
    die "$CRYPTTAB mixes shared-unlock and plain rpool entries; finish host setup first"
  for index in 0 1; do
    if [[ "$form" == shared ]]; then
      lines[index]="${lines[index]}"$'\t'"${SHARED_KEY_ID}"$'\t'"${SHARED_OPTIONS}"
    else
      lines[index]="${lines[index]%%$'\t'*} ${lines[index]#*$'\t'} none ${PLAIN_OPTIONS}"
    fi
  done
  replace_crypttab_entries "^crypt-rpool-mirror${PAIR}-[12]\$" "${lines[@]}"
  rebuild_initramfs
  verify_initramfs_mappers present "$form" "${mappers[@]}"

  if ((in_pool == 0)); then
    ashift="$(uniform_ashift)"
    zpool add -o "ashift=${ashift}" "$POOL" mirror \
      "/dev/mapper/${mappers[0]}" "/dev/mapper/${mappers[1]}"
  fi
  same_mirror "/dev/mapper/${mappers[0]}" "/dev/mapper/${mappers[1]}" ||
    die "rpool does not show ${mappers[0]} and ${mappers[1]} as one mirror"
  pool_healthy || die "rpool is not healthy after adding extra mirror $PAIR"
  printf 'Extra mirror %s is part of rpool and unlocks at boot (%s crypttab form).\n' \
    "$PAIR" "$form"
}

stable_by_id_path() {
  local disk="$1" serial="$2" candidate stable=""
  for candidate in "$BY_ID_DIR"/*; do
    [[ -L "$candidate" && "${candidate##*/}" == *"$serial"* &&
      "${candidate##*/}" != *-part* &&
      "$(canonical "$candidate")" == "$(canonical "$disk")" ]] || continue
    if [[ -z "$stable" || "${#candidate}" -lt "${#stable}" ||
      ("${#candidate}" -eq "${#stable}" && "$candidate" < "$stable") ]]; then
      stable="$candidate"
    fi
  done
  [[ -n "$stable" ]] ||
    die "no stable ${BY_ID_DIR} path contains serial $serial for $disk"
  printf '%s\n' "$stable"
}

serials_share_mirror() {
  zpool status -P "$POOL" | awk -v serial_1="$1" -v serial_2="$2" '
    $1 ~ /^mirror-/ { mirror=$1 }
    index($1, serial_1) { a=mirror }
    index($1, serial_2) { b=mirror }
    END { exit !(a != "" && a == b) }
  '
}

command_clear_add() {
  local checked serial disk size state ashift
  local -a stable=() wipe=()
  parse_common "$@"
  if serials_share_mirror "${SERIALS[0]}" "${SERIALS[1]}"; then
    printf 'Disks %s and %s are already one rpool mirror.\n' "${SERIALS[@]}"
    return
  fi
  local leaves
  leaves="$(pool_leaves)" || die "could not read rpool status"
  if grep -q '^/dev/mapper/' <<<"$leaves"; then
    die "rpool is encrypted; use luks-prepare and luks-add"
  fi
  local row path
  local -a rows=()
  checked="$(check_new_disks)" || exit 1
  mapfile -t rows <<<"$checked"
  for row in "${rows[@]}"; do
    IFS=$'\t' read -r serial disk size state <<<"$row"
    [[ "$state" != luks ]] ||
      die "refusing to erase existing LUKS disk $serial without a separate recovery decision"
    path="$(stable_by_id_path "$disk" "$serial")" || exit 1
    stable+=("$path")
    wipe+=("$disk")
  done
  ashift="$(uniform_ashift)"
  for disk in "${wipe[@]}"; do
    wipefs --all --force "$disk"
    sgdisk --zap-all "$disk"
  done
  udevadm settle
  zpool add -o "ashift=${ashift}" "$POOL" mirror "${stable[0]}" "${stable[1]}"
  serials_share_mirror "${SERIALS[0]}" "${SERIALS[1]}" ||
    die "rpool does not show ${SERIALS[0]} and ${SERIALS[1]} as one mirror"
  pool_healthy || die "rpool is not healthy after adding the mirror"
  printf 'Disks %s and %s were added to rpool as one unencrypted mirror.\n' "${SERIALS[@]}"
}

command_retire_luks() {
  local mapper form pattern changed=0
  local -a mappers=()
  (($# > 0)) || die "retire-luks requires at least one mapper"
  for mapper in "$@"; do
    # The boot mirror (crypt-rpool-a/b) holds the ESPs and is never retired.
    [[ "$mapper" =~ ^crypt-rpool-mirror[2-5]-[12]$ ]] ||
      die "refusing to retire $mapper; only crypt-rpool-mirrorN-M mappings of extra mirrors can be retired"
    ! pool_contains "/dev/mapper/$mapper" ||
      die "$mapper is still part of rpool; wait until its vdev removal completes"
    mappers+=("$mapper")
  done
  for mapper in "${mappers[@]}"; do
    if mapper_active "$mapper"; then
      cryptsetup close "$mapper" ||
        die "could not close $mapper; something still holds it open"
      printf 'Closed %s.\n' "$mapper"
      changed=1
    fi
    if [[ -f "$CRYPTTAB" ]] &&
      awk -v mapper="$mapper" '$1 == mapper { found=1 } END { exit !found }' "$CRYPTTAB"; then
      changed=1
    fi
  done
  pattern="^($(IFS='|'; printf '%s' "${mappers[*]}"))\$"
  form="$(crypttab_form "$pattern")"
  replace_crypttab_entries "$pattern"
  if ((changed)); then
    rebuild_initramfs
  fi
  verify_initramfs_mappers absent "$form" "${mappers[@]}"
  printf 'Retired %s: closed, removed from crypttab, and absent from the initramfs.\n' \
    "${mappers[*]}"
}

main() {
  (($# > 0)) || {
    usage >&2
    exit 2
  }
  local command="$1"
  shift
  [[ "$TEST_MODE" == 1 || "$(id -u)" == 0 ]] || die "app-ha-rpool-mirror must run as root"
  case "$command" in
    check-new) command_check_new "$@" ;;
    luks-prepare) command_luks_prepare "$@" ;;
    luks-check-prepared) command_luks_check_prepared "$@" ;;
    luks-backup-headers) command_luks_backup_headers "$@" ;;
    luks-add) command_luks_add "$@" ;;
    clear-add) command_clear_add "$@" ;;
    retire-luks) command_retire_luks "$@" ;;
    -h | --help) usage ;;
    *)
      usage >&2
      exit 2
      ;;
  esac
}

if [[ "${BASH_SOURCE[0]}" == "$0" ]]; then
  main "$@"
fi
