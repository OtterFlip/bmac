#!/usr/bin/env bash

# Copyright (c) 2026 BEENTHERE VENTURES, INC.
# SPDX-License-Identifier: GPL-3.0-only

# Root-only rpool mirror operations for one Proxmox host. This is the single
# copy of the disk, LUKS, and zpool logic used by hosts/setup_proxmox_host.sh,
# hosts/add_new_disk_vdev.sh, hosts/add_replacement_disk.sh, and
# hosts/list_disks_ready_for_physically_removal.sh. Those workstation scripts
# install it on the host as /usr/local/sbin/app-ha-rpool-mirror.
#
# Every subcommand selects disks by serial and re-proves their identity before
# changing anything. Boot-mirror members A and B use the LUKS mappings
# crypt-rpool-a and crypt-rpool-b on partition 3; extra mirror pair N always
# uses the whole-disk mappings crypt-rpool-mirrorN-1 and crypt-rpool-mirrorN-2.

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
  BOOT_UUIDS="${APP_HA_BOOT_UUIDS:?APP_HA_BOOT_UUIDS is required in test mode}"
else
  CRYPTTAB=/etc/crypttab
  HEADER_DIR=/root
  BY_ID_DIR=/dev/disk/by-id
  INITRD="/boot/initrd.img-$(uname -r)"
  WORK_ROOT=/var/tmp
  BOOT_UUIDS=/etc/kernel/proxmox-boot-uuids
fi
SHARED_KEY_ID=app-ha-rpool
SHARED_OPTIONS=luks,initramfs,nofail,keyscript=decrypt_keyctl
PLAIN_OPTIONS=luks,initramfs,nofail

KEY_FILE=""
PASSPHRASE=""
EXPECT_BYTES=""
MATCH=""
PAIR=""
MEMBER=""
SURVIVOR=""
SERIAL_COUNT=2
CONVERSION_OK=0
declare -a SERIALS=()

die() {
  printf 'ERROR: %s\n' "$*" >&2
  exit 1
}

usage() {
  cat <<'EOF'
Usage: app-ha-rpool-mirror COMMAND [OPTIONS] ARGS

Commands:
  check-new [SIZE] SERIAL_1 [SERIAL_2]
      Prove disks are safe to erase for a new mirror or a replacement member.
      Prints one "serial<TAB>disk<TAB>bytes<TAB>luks|fresh" line per disk.
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

One mirror member at a time (MEMBER is A or B for partition 3 of a boot-mirror
disk, or N-M for the whole disk of extra mirror N, member M):
  luks-prepare-member --member MEMBER (--key-file FILE | --prompt) [SIZE] SERIAL
      Interactive, at the host console. Proves the shared passphrase like
      luks-prepare, closes a leftover mapping of a pulled disk that holds the
      member's mapping name, formats the member as LUKS2 (a boot member's
      partition 3, or an extra member's whole disk), opens it, and backs up its
      header to /root/luks-header-{A,B,mirrorN-M}.bin.
  luks-check-member --member MEMBER SERIAL
      Verify the member's mapping, its backing device, and its header backup.
  luks-backup-headers --member MEMBER SERIAL
      Refresh the header backup of one member.
  luks-register --member MEMBER SERIAL
      Record the member's mapping in /etc/crypttab in the form the host already
      uses, then rebuild and verify the initramfs.
  boot-partition --survivor SERIAL NEW_SERIAL
      Erase a disk of identical capacity and copy the partition table of the
      surviving boot-mirror disk onto it. Safe to rerun.
  boot-esp --survivor SERIAL NEW_SERIAL
      Format the new disk's ESP, register it with proxmox-boot-tool, drop ESPs
      of pulled disks, and wait until it holds the same boot files as the
      survivor's ESP. Safe to rerun.
  replace-member --survivor SERIAL [--member MEMBER] NEW_SERIAL
      Put NEW_SERIAL into the mirror of the surviving disk SERIAL: zpool replace
      of its pulled member, or zpool attach when the mirror was detached down
      to SERIAL. With --member, first records the new LUKS member for boot
      unlock; for the boot mirror, first proves the new ESP is in sync. Starts
      the resilver without waiting for it. Safe to rerun.

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
  zpool status -P "$POOL" | awk '$1 ~ /^\// { print $1 }'
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
      --member)
        (($# >= 2)) || die "--member requires A, B, or N-M"
        [[ "$2" =~ ^(A|B|[2-5]-[12])$ ]] ||
          die "--member must be A, B, or N-M with N 2 through 5 and M 1 or 2"
        MEMBER="$2"
        shift 2
        ;;
      --survivor)
        (($# >= 2)) || die "--survivor requires a disk serial"
        validate_serial "$2"
        SURVIVOR="$2"
        shift 2
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
  if ((SERIAL_COUNT == 1)); then
    ((${#SERIALS[@]} == 1)) || die "exactly one disk serial is required"
    [[ "${SERIALS[0]}" != "$SURVIVOR" ]] ||
      die "the new disk and the surviving disk must differ"
  elif ((SERIAL_COUNT == 0 && ${#SERIALS[@]} == 1)); then
    # check-new of a single replacement disk.
    [[ "$MATCH" != equal ]] || die "--require-equal needs two disk serials"
  else
    ((${#SERIALS[@]} == 2)) || die "exactly two disk serials are required"
    [[ "${SERIALS[0]}" != "${SERIALS[1]}" ]] || die "the two disk serials must differ"
  fi
  if [[ "$MATCH" == exact || "$MATCH" == within-1pct ]]; then
    [[ -n "$EXPECT_BYTES" ]] || die "--match $MATCH requires --expect-bytes"
  fi
}

require_pair() {
  [[ -n "$PAIR" ]] || die "--pair is required"
}

require_member() {
  [[ -n "$MEMBER" ]] || die "--member is required"
}

require_survivor() {
  [[ -n "$SURVIVOR" ]] || die "--survivor is required"
}

member_mapper() {
  case "$1" in
    A) printf 'crypt-rpool-a\n' ;;
    B) printf 'crypt-rpool-b\n' ;;
    *) printf 'crypt-rpool-mirror%s\n' "$1" ;;
  esac
}

member_header() {
  case "$1" in
    A | B) printf '%s/luks-header-%s.bin\n' "$HEADER_DIR" "$1" ;;
    *) printf '%s/luks-header-mirror%s.bin\n' "$HEADER_DIR" "$1" ;;
  esac
}

is_boot_member() {
  [[ "$1" == A || "$1" == B ]]
}

partition_path() {
  # NVMe (and any disk name ending in a digit) puts "p" before the number.
  if [[ "$1" =~ [0-9]$ ]]; then
    printf '%sp%s\n' "$1" "$2"
  else
    printf '%s%s\n' "$1" "$2"
  fi
}

# The device that holds a member's LUKS: partition 3 of a boot-mirror disk,
# else the whole disk.
member_target() {
  if is_boot_member "$1"; then
    partition_path "$2" 3
  else
    printf '%s\n' "$2"
  fi
}

# Canonical paths of DISK, its partitions, and the mappings on them.
disk_device_set() {
  local device devices
  devices="$(lsblk -nrpo NAME "$1")" || die "could not list the devices of $1"
  while IFS= read -r device; do
    [[ -n "$device" ]] || continue
    canonical "$device" || exit 1
  done <<<"$devices"
}

on_device_set() {
  local wanted
  wanted="$(canonical "$1")" || exit 1
  grep -Fxq -- "$wanted" <<<"$2"
}

# True when PATH is a block device the kernel currently lists.
device_present() {
  local wanted devices device
  wanted="$(canonical "$1")" || exit 1
  devices="$(lsblk -nrpo NAME)" || die "could not list block devices"
  while IFS= read -r device; do
    [[ -n "$device" ]] || continue
    [[ "$(canonical "$device")" == "$wanted" ]] && return 0
  done <<<"$devices"
  return 1
}

leaf_state() {
  zpool status -P "$POOL" | awk -v leaf="$1" '$1 == leaf { print $2; exit }'
}

# Print "top parent leaf state" for every leaf of the pool's data vdevs.
# parent is the leaf's mirror-N or replacing-N, or "-" for a one-disk vdev.
pool_rows() {
  zpool status -P "$POOL" | awk -v pool="$POOL" '
    /^config:/ { in_config = 1; next }
    in_config && /^errors:/ { exit }
    in_config {
      line = $0
      sub(/^\t/, "", line)
      if (line !~ /[^ \t]/) next
      match(line, /^ */)
      indent = RLENGTH
      split(substr(line, indent + 1), field, /[ \t]+/)
      if (field[1] == "NAME") next
      if (indent == 0) { data = (field[1] == pool); next }
      if (!data) next
      if (indent == 2) top = field[1]
      if (field[1] ~ /^(mirror|replacing|spare|raidz[0-9]*|draid[0-9a-z:]*)-[0-9]+$/) {
        parent_at[indent] = field[1]
        next
      }
      print top, (indent == 2 ? "-" : parent_at[indent - 2]), field[1], field[2]
    }
  '
}

# Print "serial disk bytes luks|fresh" for each serial after proving that the
# disk is not part of rpool or any other imported pool, is not mounted, and has
# the expected capacity.
check_new_disks() {
  local serial disk size state device resolved pool
  local -a sizes=() disks=()
  [[ -n "$MATCH" ]] || die "a disk size requirement is required"
  pool="$(zpool status -LP)" || die "could not read the status of the imported pools"
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
        die "disk $serial ($disk) is already part of an imported ZFS pool as $device"
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

# Print "mapper<TAB>backing" for every ONLINE LUKS member of rpool (a pulled
# member cannot be tested). Refuses a pool that mixes encrypted and
# unencrypted members, except while setup converts the boot mirror in place.
existing_luks_members() {
  local leaves leaf mapper backing plain=0 luks=0
  local -a rows=()
  leaves="$(zpool status -P "$POOL" | awk '$1 ~ /^\// && $2 == "ONLINE" { print $1 }')" ||
    die "could not read rpool status"
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
  if ((CONVERSION_OK)); then
    # Setup converts boot member A while B is still raw; the setup key file
    # was verified separately and is proven here against any LUKS member.
    ((luks == 0)) || printf '%s\n' "${rows[@]}"
    return 0
  fi
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
  SERIAL_COUNT=0
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
    prepare_luks_target "$serial" "$disk" "$disk" "$mapper" "$header"
  done
  PASSPHRASE=""
  printf 'Extra mirror %s LUKS members prepared successfully.\n' "$PAIR"
}

# Format TARGET as LUKS2 with the shared passphrase, or reuse LUKS that the
# passphrase already opens; open it as MAPPER and back up its header. ERASE is
# the whole disk to wipe before formatting, or empty for a partition.
prepare_luks_target() {
  local serial="$1" target="$2" erase="$3" mapper="$4" header="$5" backing
  if cryptsetup isLuks "$target" >/dev/null 2>&1; then
    with_shared_key open --test-passphrase "$target" >/dev/null 2>&1 ||
      die "disk $serial already holds LUKS that the shared passphrase does not open; refusing to reuse or erase it"
    printf 'Reusing LUKS on %s (%s), which opens with the shared passphrase.\n' "$serial" "$target"
  else
    if [[ -n "$erase" ]]; then
      wipefs --all --force "$erase"
      sgdisk --zap-all "$erase"
      udevadm settle
    fi
    with_shared_key luksFormat --batch-mode --type luks2 "$target"
  fi
  if ! mapper_active "$mapper"; then
    with_shared_key open "$target" "$mapper"
  fi
  backing="$(mapper_backing "$mapper")" || die "could not read the backing device of $mapper"
  [[ "$(canonical "$backing")" == "$(canonical "$target")" ]] ||
    die "$mapper is backed by $backing, not $target"
  rm -f -- "$header"
  cryptsetup luksHeaderBackup "$target" --header-backup-file "$header"
  chmod 0600 "$header"
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
  if [[ " $* " == *" --member "* ]]; then
    SERIAL_COUNT=1
    parse_common "$@"
    require_member
    member_backup_header
    return
  fi
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

# One crypttab line for MAPPER and SOURCE (UUID=...) in FORM.
crypttab_line() {
  local form="$1" mapper="$2" source="$3"
  if [[ "$form" == shared ]]; then
    printf '%s\t%s\t%s\t%s\n' "$mapper" "$source" "$SHARED_KEY_ID" "$SHARED_OPTIONS"
  else
    printf '%s %s none %s\n' "$mapper" "$source" "$PLAIN_OPTIONS"
  fi
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
    lines[index]="$(crypttab_line "$form" "${lines[index]%%$'\t'*}" "${lines[index]#*$'\t'}")"
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
  local mapper form pattern
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
    fi
  done
  pattern="^($(IFS='|'; printf '%s' "${mappers[*]}"))\$"
  form="$(crypttab_form "$pattern")"
  replace_crypttab_entries "$pattern"
  # Always rebuild every kernel's initramfs and resync the ESPs: a run
  # interrupted after editing crypttab, or after rebuilding but before the
  # ESPs were refreshed, leaves a boot image that still unlocks the mappings.
  rebuild_initramfs
  verify_initramfs_mappers absent "$form" "${mappers[@]}"
  printf 'Retired %s: closed, removed from crypttab, and absent from the initramfs.\n' \
    "${mappers[*]}"
}

# ---------------------------------------------------------------------------
# One mirror member at a time: setup's in-place LUKS conversion of the boot
# mirror, and hosts/add_replacement_disk.sh.

# Print the disk, target, mapping, and header of MEMBER on the disk SERIALS[0]
# after proving the mapping is open on that target.
verified_member_mapping() {
  local disk target mapper backing
  disk="$(disk_by_serial "${SERIALS[0]}")" || exit 1
  target="$(member_target "$MEMBER" "$disk")"
  mapper="$(member_mapper "$MEMBER")"
  mapper_active "$mapper" || die "$mapper is not open"
  backing="$(mapper_backing "$mapper")" || die "could not read the backing device of $mapper"
  [[ "$(canonical "$backing")" == "$(canonical "$target")" ]] ||
    die "$mapper is backed by $backing, not $target of disk ${SERIALS[0]}"
  printf '%s\t%s\t%s\t%s\n' "$disk" "$target" "$mapper" "$(member_header "$MEMBER")"
}

member_backup_header() {
  local disk target mapper header
  IFS=$'\t' read -r disk target mapper header <<<"$(verified_member_mapping)"
  [[ -n "$header" ]] || exit 1
  rm -f -- "$header"
  cryptsetup luksHeaderBackup "$target" --header-backup-file "$header"
  chmod 0600 "$header"
}

# A pulled disk can leave its LUKS mapping open until the next reboot, which
# keeps the member's mapping name busy. Take that member offline (its disk is
# gone, so no redundancy is lost) and close the leftover mapping.
release_stale_mapper() {
  local mapper="$1" backing state
  mapper_active "$mapper" || return 0
  backing="$(mapper_backing "$mapper")" || backing=""
  if [[ -n "$backing" && "$backing" != "(null)" ]] && device_present "$backing"; then
    return 0
  fi
  state="$(leaf_state "/dev/mapper/$mapper")"
  [[ "$state" != ONLINE ]] ||
    die "$mapper is ONLINE in rpool although its disk is gone; refusing to touch it"
  if [[ -n "$state" && "$state" != OFFLINE ]]; then
    zpool offline "$POOL" "/dev/mapper/$mapper" ||
      die "could not take the pulled member /dev/mapper/$mapper offline"
  fi
  cryptsetup close "$mapper" ||
    die "could not close the leftover mapping $mapper of the pulled disk; gracefully migrate the production guests off this host, gracefully reboot it, and rerun"
  printf 'Closed the leftover mapping %s of the pulled disk.\n' "$mapper"
}

command_luks_prepare_member() {
  local confirmation serial disk target erase="" mapper header
  SERIAL_COUNT=1
  parse_common "$@"
  require_member
  [[ -n "$KEY_FILE" || "${PASSPHRASE_PROMPT:-0}" == 1 ]] ||
    die "luks-prepare-member requires --key-file FILE or --prompt"
  serial="${SERIALS[0]}"
  disk="$(disk_by_serial "$serial")" || exit 1
  target="$(member_target "$MEMBER" "$disk")"
  mapper="$(member_mapper "$MEMBER")"
  header="$(member_header "$MEMBER")"
  if mapper_active "$mapper" && [[ "$(leaf_state "/dev/mapper/$mapper")" == ONLINE ]]; then
    die "$mapper is already an ONLINE member of rpool"
  fi
  if is_boot_member "$MEMBER"; then
    # Setup converts a boot member in place and a replacement disk received
    # the survivor's partition table; either way partition 3 already exists.
    block_device_exists "$target" || die "$target is missing; partition disk $serial first"
    ! pool_contains "$target" || die "$target of disk $serial is still part of rpool"
    if lsblk -nrpo MOUNTPOINT "$target" | awk 'NF { found=1 } END { exit !found }'; then
      die "$target of disk $serial has a mounted filesystem or active swap"
    fi
    [[ -z "$KEY_FILE" ]] || CONVERSION_OK=1
    printf 'Boot-mirror member %s will be LUKS2 on %s of disk %s.\n' "$MEMBER" "$target" "$serial"
  else
    [[ -n "$MATCH" ]] || die "a disk size requirement is required"
    check_new_disks >/dev/null || exit 1
    erase="$disk"
    printf 'Extra mirror member %s will be whole-disk LUKS2 on disk %s (%s).\n' \
      "$MEMBER" "$serial" "$disk"
  fi
  printf 'A disk or partition that is not LUKS yet is erased. Type GO to prepare member %s.\n> ' "$MEMBER"
  IFS= read -r confirmation || die "input ended before confirmation"
  [[ "$confirmation" == GO ]] || die "confirmation did not match GO; nothing was changed"

  obtain_shared_key
  release_stale_mapper "$mapper"
  prepare_luks_target "$serial" "$target" "$erase" "$mapper" "$header"
  PASSPHRASE=""
  printf 'LUKS member %s prepared successfully.\n' "$MEMBER"
}

command_luks_check_member() {
  local disk target mapper header
  SERIAL_COUNT=1
  parse_common "$@"
  require_member
  IFS=$'\t' read -r disk target mapper header <<<"$(verified_member_mapping)"
  [[ -n "$header" ]] || exit 1
  [[ -s "$header" ]] || die "header backup $header is missing"
  printf 'Member %s: %s is open on %s and its header is backed up.\n' "$MEMBER" "$mapper" "$target"
}

# Record MEMBER's mapping in crypttab in the host's form, then rebuild and
# verify the initramfs so the member unlocks at the next boot.
register_member() {
  local disk target mapper header uuid form
  IFS=$'\t' read -r disk target mapper header <<<"$(verified_member_mapping)"
  [[ -n "$mapper" ]] || exit 1
  uuid="$(cryptsetup luksUUID "$target")" || die "could not read the LUKS UUID of $target"
  form="$(crypttab_form "^${mapper}\$")"
  [[ "$form" != mixed ]] ||
    die "$CRYPTTAB mixes shared-unlock and plain rpool entries; finish host setup first"
  replace_crypttab_entries "^${mapper}\$" "$(crypttab_line "$form" "$mapper" "UUID=${uuid}")"
  rebuild_initramfs
  verify_initramfs_mappers present "$form" "$mapper"
  printf '%s unlocks at boot (%s crypttab form).\n' "$mapper" "$form"
}

command_luks_register() {
  SERIAL_COUNT=1
  parse_common "$@"
  require_member
  register_member
}

# Print "number start end typecode" for each partition of DISK.
partition_table() {
  sgdisk --print "$1" 2>/dev/null |
    awk '$1 ~ /^[0-9]+$/ && $2 ~ /^[0-9]+$/ && $3 ~ /^[0-9]+$/ { print $1, $2, $3, $6 }'
}

disk_guid() {
  sgdisk --print "$1" 2>/dev/null |
    awk -F': *' '/^Disk identifier \(GUID\)/ { print $2; exit }'
}

# True when the new disk still carries the survivor's GUIDs: sgdisk
# --replicate copies them, and only --randomize-guids makes them unique.
shares_survivor_guids() {
  local guid
  guid="$(disk_guid "$1")"
  [[ -n "$guid" && "$guid" == "$(disk_guid "$2")" ]]
}

fs_uuid() {
  blkid -s UUID -o value "$1" 2>/dev/null || true
}

esp_registered() {
  [[ -n "$1" && -f "$BOOT_UUIDS" ]] &&
    awk -v uuid="$1" 'toupper($1) == toupper(uuid) { found=1 } END { exit !found }' "$BOOT_UUIDS"
}

# What proxmox-boot-tool status reports for one ESP, for example
# "uefi (versions: 6.14.8-2-pve)"; two ESPs in sync report the same text.
esp_configuration() {
  local status
  status="$(proxmox-boot-tool status 2>/dev/null)" || true
  awk -v uuid="$1" '
    toupper($1) == toupper(uuid) && $2 == "is" && $3 == "configured" {
      sub(/^[^:]*: */, "")
      print
      exit
    }
  ' <<<"$status"
}

# Prove SURVIVOR's disk is the healthy boot-mirror disk: its partition table
# has an ESP and partition 3, partition 3 (or the LUKS mapping on it) is an
# ONLINE rpool member, and its ESP is registered with proxmox-boot-tool.
# Prints the survivor's ESP UUID.
survivor_boot_esp() {
  local disk="$1" table p3 devices uuid leaf state online=0
  table="$(partition_table "$disk")"
  grep -q '^2 ' <<<"$table" && grep -q '^3 ' <<<"$table" ||
    die "surviving disk $SURVIVOR has no boot-mirror partition table"
  p3="$(partition_path "$disk" 3)"
  devices="$(disk_device_set "$p3")" || exit 1
  while read -r leaf state; do
    [[ -n "$leaf" && "$state" == ONLINE ]] || continue
    on_device_set "$leaf" "$devices" && online=1
  done < <(zpool status -P "$POOL" | awk '$1 ~ /^\// { print $1, $2 }')
  ((online)) || die "partition 3 of surviving disk $SURVIVOR is not an ONLINE rpool member"
  uuid="$(fs_uuid "$(partition_path "$disk" 2)")"
  esp_registered "$uuid" ||
    die "the ESP of surviving disk $SURVIVOR is not registered with proxmox-boot-tool"
  printf '%s\n' "$uuid"
}

verify_esps_in_sync() {
  local survivor_config new_config
  survivor_config="$(esp_configuration "$1")"
  new_config="$(esp_configuration "$2")"
  [[ -n "$survivor_config" && "$new_config" == "$survivor_config" ]] ||
    die "the new ESP ${2:-?} (${new_config:-not listed}) does not match the surviving ESP $1 (${survivor_config:-not listed}); run boot-esp"
  printf 'Both boot-mirror ESPs are registered and hold the same boot files: %s\n' "$survivor_config"
}

refuse_existing_luks() {
  local disk="$1" serial="$2" device devices
  devices="$(lsblk -nrpo NAME "$disk")" || die "could not list the devices of $disk"
  while IFS= read -r device; do
    [[ -n "$device" && "$device" != /dev/mapper/* ]] || continue
    ! cryptsetup isLuks "$device" >/dev/null 2>&1 ||
      die "disk $serial holds LUKS on $device; it is not erased automatically. If the disk is truly unused, erase it deliberately (cryptsetup erase, then wipefs --all) and rerun"
  done <<<"$devices"
}

command_boot_partition() {
  local serial survivor_disk new_disk table device devices number
  SERIAL_COUNT=1
  parse_common "$@"
  require_survivor
  serial="${SERIALS[0]}"
  survivor_disk="$(disk_by_serial "$SURVIVOR")" || exit 1
  new_disk="$(disk_by_serial "$serial")" || exit 1
  [[ "$survivor_disk" != "$new_disk" ]] || die "$SURVIVOR and $serial are the same disk"
  survivor_boot_esp "$survivor_disk" >/dev/null || exit 1
  table="$(partition_table "$survivor_disk")"
  if [[ "$(partition_table "$new_disk")" == "$table" ]]; then
    if shares_survivor_guids "$new_disk" "$survivor_disk"; then
      # An earlier run stopped between copying the table and giving the copy
      # its own GUIDs; nothing uses the new partitions yet.
      sgdisk --randomize-guids "$new_disk"
      partx --update "$new_disk" || true
      udevadm settle
      ! shares_survivor_guids "$new_disk" "$survivor_disk" ||
        die "disk $serial still has the GUIDs of $SURVIVOR"
    fi
    # The same layout can come from an earlier installation whose partition 3
    # still carries another pool's labels. Unless partition 3 already holds
    # this replacement's LUKS, clear it after the usual safety checks.
    if ! cryptsetup isLuks "$(partition_path "$new_disk" 3)" >/dev/null 2>&1; then
      EXPECT_BYTES="$(blockdev --getsize64 "$survivor_disk")" ||
        die "could not read the size of $survivor_disk"
      MATCH=exact
      check_new_disks >/dev/null || exit 1
      wipefs --all --force "$(partition_path "$new_disk" 3)"
    fi
    printf 'Disk %s already has the partition table of %s.\n' "$serial" "$SURVIVOR"
    return
  fi
  EXPECT_BYTES="$(blockdev --getsize64 "$survivor_disk")" ||
    die "could not read the size of $survivor_disk"
  MATCH=exact
  check_new_disks >/dev/null || exit 1
  refuse_existing_luks "$new_disk" "$serial"

  devices="$(lsblk -nrpo NAME "$new_disk")" || die "could not list the devices of $new_disk"
  while IFS= read -r device; do
    [[ -n "$device" && "$device" != "$new_disk" ]] || continue
    wipefs --all --force "$device"
  done <<<"$devices"
  wipefs --all --force "$new_disk"
  sgdisk --zap-all "$new_disk"
  # The healthy disk is the main device; --replicate writes to the new disk.
  sgdisk "$survivor_disk" --replicate="$new_disk"
  sgdisk --randomize-guids "$new_disk"
  partx --update "$new_disk" || true
  udevadm settle
  [[ "$(partition_table "$new_disk")" == "$table" ]] ||
    die "disk $serial did not receive the partition table of $SURVIVOR"
  ! shares_survivor_guids "$new_disk" "$survivor_disk" ||
    die "disk $serial still has the GUIDs of $SURVIVOR"
  for number in 2 3; do
    block_device_exists "$(partition_path "$new_disk" "$number")" ||
      die "partition $number of disk $serial did not appear"
    wipefs --all --force "$(partition_path "$new_disk" "$number")"
  done
  printf 'Disk %s now has the partition table of %s.\n' "$serial" "$SURVIVOR"
}

command_boot_esp() {
  local serial survivor_disk new_disk survivor_uuid new_esp new_uuid survivor_config
  local -a mode=()
  SERIAL_COUNT=1
  parse_common "$@"
  require_survivor
  serial="${SERIALS[0]}"
  survivor_disk="$(disk_by_serial "$SURVIVOR")" || exit 1
  new_disk="$(disk_by_serial "$serial")" || exit 1
  [[ "$survivor_disk" != "$new_disk" ]] || die "$SURVIVOR and $serial are the same disk"
  survivor_uuid="$(survivor_boot_esp "$survivor_disk")" || exit 1
  [[ "$(partition_table "$new_disk")" == "$(partition_table "$survivor_disk")" ]] ||
    die "disk $serial does not have the partition table of $SURVIVOR; run boot-partition first"
  ! pool_contains "$(partition_path "$new_disk" 3)" ||
    die "partition 3 of disk $serial is already part of rpool"
  survivor_config="$(esp_configuration "$survivor_uuid")"
  [[ -n "$survivor_config" ]] ||
    die "proxmox-boot-tool status does not describe the surviving ESP $survivor_uuid"
  # Install the same boot loader the survivor uses.
  [[ "$survivor_config" != grub* ]] || mode=(grub)
  new_esp="$(partition_path "$new_disk" 2)"
  new_uuid="$(fs_uuid "$new_esp")"
  if ! esp_registered "$new_uuid"; then
    proxmox-boot-tool format "$new_esp" --force
    proxmox-boot-tool init "$new_esp" "${mode[@]}"
    new_uuid="$(fs_uuid "$new_esp")"
    esp_registered "$new_uuid" || die "proxmox-boot-tool did not register $new_esp"
  fi
  # Forget the ESPs of pulled disks, then copy the current kernels and
  # initramfs images to every registered ESP.
  proxmox-boot-tool clean
  proxmox-boot-tool refresh
  verify_esps_in_sync "$survivor_uuid" "$new_uuid"
}

# Print the stable by-id path of DISK (or of its partition NUMBER) whose name
# contains SERIAL.
stable_by_id_partition() {
  local disk="$1" serial="$2" number="$3" candidate stable="" target
  target="$(partition_path "$disk" "$number")"
  for candidate in "$BY_ID_DIR"/*; do
    [[ -L "$candidate" && "${candidate##*/}" == *"$serial"*"-part${number}" &&
      "$(canonical "$candidate")" == "$(canonical "$target")" ]] || continue
    if [[ -z "$stable" || "${#candidate}" -lt "${#stable}" ||
      ("${#candidate}" -eq "${#stable}" && "$candidate" < "$stable") ]]; then
      stable="$candidate"
    fi
  done
  [[ -n "$stable" ]] ||
    die "no stable ${BY_ID_DIR} path contains serial $serial for $target"
  printf '%s\n' "$stable"
}

command_replace_member() {
  local serial survivor_disk new_disk survivor_devices new_devices rows row
  local top parent leaf state survivor_leaf="" survivor_top="" survivor_parent=""
  local other="" other_state="" siblings=0 new_device boot=false survivor_uuid="" new_uuid
  local backing mode
  SERIAL_COUNT=1
  parse_common "$@"
  require_survivor
  serial="${SERIALS[0]}"
  survivor_disk="$(disk_by_serial "$SURVIVOR")" || exit 1
  new_disk="$(disk_by_serial "$serial")" || exit 1
  [[ "$survivor_disk" != "$new_disk" ]] || die "$SURVIVOR and $serial are the same disk"
  survivor_devices="$(disk_device_set "$survivor_disk")" || exit 1
  new_devices="$(disk_device_set "$new_disk")" || exit 1
  rows="$(pool_rows)" || die "could not read rpool status"

  while read -r top parent leaf state; do
    [[ -n "$leaf" && "$state" == ONLINE ]] || continue
    on_device_set "$leaf" "$survivor_devices" || continue
    [[ -z "$survivor_leaf" ]] || die "surviving disk $SURVIVOR holds more than one rpool member"
    survivor_leaf="$leaf"
    survivor_top="$top"
    survivor_parent="$parent"
  done <<<"$rows"
  [[ -n "$survivor_leaf" ]] || die "surviving disk $SURVIVOR is not an ONLINE rpool member"

  # A rerun after the zpool change finds the new disk already resilvering.
  while read -r top parent leaf state; do
    [[ "$top" == "$survivor_top" && -n "$leaf" ]] || continue
    [[ "$state" != OFFLINE && "$state" != REMOVED && "$state" != UNAVAIL &&
      "$state" != FAULTED ]] || continue
    if on_device_set "$leaf" "$new_devices"; then
      printf 'Disk %s is already part of %s (%s); nothing to do.\n' "$serial" "$survivor_top" "$leaf"
      return
    fi
  done <<<"$rows"

  survivor_uuid="$(fs_uuid "$(partition_path "$survivor_disk" 2)")"
  esp_registered "$survivor_uuid" && boot=true
  if [[ -n "$MEMBER" ]]; then
    if is_boot_member "$MEMBER"; then
      [[ "$boot" == true ]] || die "member $MEMBER is a boot-mirror member but $SURVIVOR holds no registered ESP"
    else
      [[ "$boot" == false ]] || die "member $MEMBER is an extra-mirror member but $SURVIVOR is a boot-mirror disk"
    fi
  fi

  if [[ "$survivor_parent" == - ]]; then
    mode=attach
  elif [[ "$survivor_parent" == mirror-* ]]; then
    while read -r top parent leaf state; do
      [[ "$top" == "$survivor_top" && -n "$leaf" && "$leaf" != "$survivor_leaf" ]] || continue
      siblings=$((siblings + 1))
      other="$leaf"
      other_state="$state"
      [[ "$parent" == "$survivor_parent" ]] ||
        die "$survivor_top already has a replacement in progress"
    done <<<"$rows"
    ((siblings == 1)) || die "$survivor_top has $((siblings + 1)) members; only two-way mirrors are supported"
    [[ "$other_state" != ONLINE ]] ||
      die "$survivor_top is not missing a member: $other is ONLINE"
    # Only a pulled disk is replaced. Its member is shown by GUID after a
    # reboot, or by its old path, which now resolves to nothing, to a
    # leftover mapping whose disk is gone, or to the new disk itself.
    if [[ "$other" == /* ]] && ! on_device_set "$other" "$new_devices" &&
      device_present "$other"; then
      backing=""
      if [[ "$other" == /dev/mapper/* ]] && mapper_active "${other#/dev/mapper/}"; then
        backing="$(mapper_backing "${other#/dev/mapper/}")" || backing=""
      fi
      if [[ "$other" != /dev/mapper/* ]] ||
        { [[ -n "$backing" && "$backing" != "(null)" ]] && device_present "$backing"; }; then
        die "the failed member $other ($other_state) of $survivor_top is still installed; this script only replaces a member whose disk was pulled"
      fi
    fi
    mode=replace
  else
    die "$SURVIVOR is part of $survivor_parent; a replacement is already in progress"
  fi

  if [[ -n "$MEMBER" ]]; then
    new_device="/dev/mapper/$(member_mapper "$MEMBER")"
    register_member
  elif [[ "$boot" == true ]]; then
    [[ "$(partition_table "$new_disk")" == "$(partition_table "$survivor_disk")" ]] ||
      die "disk $serial does not have the partition table of $SURVIVOR; run boot-partition first"
    new_device="$(stable_by_id_partition "$new_disk" "$serial" 3)" || exit 1
  else
    grep -q '^/dev/mapper/' <<<"$survivor_leaf" &&
      die "rpool is encrypted; pass --member for the new LUKS member"
    EXPECT_BYTES="$(blockdev --getsize64 "$survivor_disk")" ||
      die "could not read the size of $survivor_disk"
    MATCH=exact
    check_new_disks >/dev/null || exit 1
    refuse_existing_luks "$new_disk" "$serial"
    new_device="$(stable_by_id_path "$new_disk" "$serial")" || exit 1
    wipefs --all --force "$new_disk"
    sgdisk --zap-all "$new_disk"
    udevadm settle
  fi
  if [[ "$boot" == true ]]; then
    new_uuid="$(fs_uuid "$(partition_path "$new_disk" 2)")"
    esp_registered "$new_uuid" || die "the ESP of disk $serial is not registered; run boot-esp first"
    verify_esps_in_sync "$survivor_uuid" "$new_uuid"
  fi

  if [[ "$mode" == replace ]]; then
    zpool replace "$POOL" "$other" "$new_device"
  else
    zpool attach "$POOL" "$survivor_leaf" "$new_device"
  fi
  # zpool attach turns a one-disk vdev into mirror-N, so find the survivor's
  # vdev again.
  rows="$(pool_rows)" || die "could not read rpool status"
  new_devices="$(disk_device_set "$new_disk")" || exit 1
  survivor_top="$(awk -v leaf="$survivor_leaf" '$3 == leaf { print $1; exit }' <<<"$rows")"
  while read -r top parent leaf state; do
    if [[ -n "$survivor_top" && "$top" == "$survivor_top" && -n "$leaf" ]] &&
      on_device_set "$leaf" "$new_devices"; then
      printf 'Disk %s joined %s as %s; ZFS is resilvering it in the background.\n' \
        "$serial" "$survivor_top" "$new_device"
      return
    fi
  done <<<"$rows"
  die "rpool does not show disk $serial in $survivor_top after zpool $mode"
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
    luks-prepare-member) command_luks_prepare_member "$@" ;;
    luks-check-member) command_luks_check_member "$@" ;;
    luks-register) command_luks_register "$@" ;;
    boot-partition) command_boot_partition "$@" ;;
    boot-esp) command_boot_esp "$@" ;;
    replace-member) command_replace_member "$@" ;;
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
