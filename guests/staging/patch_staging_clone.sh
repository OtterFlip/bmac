#!/usr/bin/env bash

# Copyright (c) 2026 BEENTHERE VENTURES, INC.
# SPDX-License-Identifier: GPL-3.0-only

# Offline-patch one unattached staging zvol. This helper is copied to the
# selected Proxmox node by create_staging_vm.sh and runs there as root.

set -Eeuo pipefail
set +x
umask 077

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
GUEST_TREE_HELPER="$SCRIPT_DIR/patch_staging_guest_tree.py"

DEVICE=""
RESOLVED_DEVICE=""
VOLUME_ID=""
STAGING_NAME=""
ADDRESS=""
GATEWAY=""
MAC=""
DNS_CSV=""
ROOT_HASH_FILE=""
SANITIZER_FILE=""
WORK_DIR=""
MOUNT_DIR=""
ROOT_PARTITION=""
MOUNTED=false
FINISHED=false
declare -a MAPPED_DEVICE_PATHS=()

log() {
  printf '[app-ha staging offline patch] %s\n' "$*"
}

die() {
  printf 'ERROR: %s\n' "$*" >&2
  exit 1
}

usage() {
  cat <<'EOF'
Usage: patch_staging_clone.sh --device /dev/zvol/... \
  --volume-id STORAGE:vm-N-disk-N --name stageNprodN \
  --address A.B.C.D/24 --gateway A.B.C.G --mac 02:... --dns CSV \
  --root-hash-file /run/... [--sanitizer /run/...]

The zvol must not be attached to any VM, mounted, or opened by any process.
Exactly one direct ext4 Ubuntu root partition is required. Guest-tree reads,
writes, and deletes are delegated to the descriptor-relative Python helper.
The root password hash is read from a mode-0600 file and is never printed.
EOF
}

parse_args() {
  while (($#)); do
    case "$1" in
      -h | --help)
        usage
        exit 0
        ;;
      --device | --volume-id | --name | --address | --gateway | --mac | \
        --dns | --root-hash-file | --sanitizer)
        (($# >= 2)) || die "$1 requires a value"
        case "$1" in
          --device) DEVICE="$2" ;;
          --volume-id) VOLUME_ID="$2" ;;
          --name) STAGING_NAME="$2" ;;
          --address) ADDRESS="$2" ;;
          --gateway) GATEWAY="$2" ;;
          --mac) MAC="$2" ;;
          --dns) DNS_CSV="$2" ;;
          --root-hash-file) ROOT_HASH_FILE="$2" ;;
          --sanitizer) SANITIZER_FILE="$2" ;;
        esac
        shift 2
        ;;
      *) die "Unknown option: $1" ;;
    esac
  done
}

require_commands() {
  local command_name
  for command_name in "$@"; do
    command -v "$command_name" >/dev/null 2>&1 ||
      die "Required host command is unavailable: $command_name"
  done
}

validate_request() {
  ((EUID == 0)) || die "Offline patching must run as root"
  [[ "$DEVICE" =~ ^/dev/zvol/[A-Za-z0-9._/+:-]+$ ]] ||
    die "Device must be a safe /dev/zvol path"
  [[ "$VOLUME_ID" =~ ^[A-Za-z0-9._-]+:vm-[1-9][0-9]*-disk-[0-9]+$ ]] ||
    die "Volume id must be a safe Proxmox VM disk id"
  [[ "$STAGING_NAME" =~ ^stage[1-9][0-9]*prod[1-9][0-9]*$ ]] ||
    die "Staging name must use the stageNprodN formula"
  [[ "${MAC,,}" =~ ^([0-9a-f]{2}:){5}[0-9a-f]{2}$ ]] ||
    die "Staging MAC is invalid"
  python3 - "$ADDRESS" "$GATEWAY" "$DNS_CSV" <<'PY'
import ipaddress
import sys

address = ipaddress.ip_interface(sys.argv[1])
gateway = ipaddress.ip_address(sys.argv[2])
if not isinstance(address, ipaddress.IPv4Interface) or address.network.prefixlen != 24:
    raise SystemExit("staging address must be IPv4 /24")
if gateway not in address.network:
    raise SystemExit("gateway must belong to the staging /24")
servers = [value for value in sys.argv[3].split(",") if value]
if not servers or len(set(servers)) != len(servers):
    raise SystemExit("DNS servers must be present and unique")
for server in servers:
    if str(ipaddress.ip_address(server)) != server:
        raise SystemExit(f"DNS server is not canonical: {server}")
PY
  [[ -f "$ROOT_HASH_FILE" && ! -L "$ROOT_HASH_FILE" ]] ||
    die "Root password hash file must be a regular non-symlink file"
  [[ "$(stat -c %a "$ROOT_HASH_FILE")" == 600 ]] ||
    die "Root password hash file must have mode 0600"
  local root_hash
  IFS= read -r root_hash <"$ROOT_HASH_FILE"
  [[ "$root_hash" == '$6$'* && "$root_hash" != *$'\n'* ]] ||
    die "Root password hash file does not contain a SHA-512 crypt hash"
  root_hash=""
  unset root_hash
  if [[ -n "$SANITIZER_FILE" ]]; then
    local sanitizer_header
    [[ -f "$SANITIZER_FILE" && ! -L "$SANITIZER_FILE" ]] ||
      die "Sanitizer must be a regular non-symlink file"
    [[ "$(stat -c %s "$SANITIZER_FILE")" -le 1048576 ]] ||
      die "Sanitizer exceeds the 1 MiB limit"
    IFS= read -r sanitizer_header <"$SANITIZER_FILE"
    [[ "$sanitizer_header" == '#!/bin/bash' ||
      "$sanitizer_header" == '#!/usr/bin/env bash' ]] ||
      die "Sanitizer must have a Bash shebang"
    bash -n "$SANITIZER_FILE" ||
      die "Sanitizer is not valid Bash"
  fi
  [[ -f "$GUEST_TREE_HELPER" && ! -L "$GUEST_TREE_HELPER" ]] ||
    die "Safe guest-tree helper is missing"
}

assert_not_in_use() {
  local path
  for path in "$@"; do
    if findmnt -rn -S "$path" >/dev/null 2>&1; then
      die "Block device is mounted on the host: $path"
    fi
    if fuser "$path" >/dev/null 2>&1; then
      die "Block device has an open host process: $path"
    fi
  done
}

assert_not_referenced_by_vm() {
  python3 - "$VOLUME_ID" <<'PY'
import json
import re
import subprocess
import sys

volume_id = sys.argv[1]
resources = json.loads(
    subprocess.run(
        ["pvesh", "get", "/cluster/resources", "--type", "vm", "--output-format", "json"],
        check=True,
        text=True,
        stdout=subprocess.PIPE,
    ).stdout
)
if not isinstance(resources, list):
    raise SystemExit("malformed Proxmox VM inventory")
for resource in resources:
    if not isinstance(resource, dict) or resource.get("type") != "qemu":
        continue
    vmid = resource.get("vmid")
    node = resource.get("node")
    if (
        not isinstance(vmid, int)
        or not isinstance(node, str)
        or not re.fullmatch(r"mox([1-9]|10)", node)
    ):
        raise SystemExit("malformed Proxmox VM identity")
    config = json.loads(
        subprocess.run(
            [
                "pvesh",
                "get",
                f"/nodes/{node}/qemu/{vmid}/config",
                "--output-format",
                "json",
            ],
            check=True,
            text=True,
            stdout=subprocess.PIPE,
        ).stdout
    )
    if not isinstance(config, dict):
        raise SystemExit(f"malformed Proxmox VM configuration for {vmid}")
    for key, value in config.items():
        if (
            re.fullmatch(
                r"(?:(?:scsi|sata|virtio|ide|unused|efidisk|tpmstate)[0-9]+)",
                key,
            )
            and str(value).split(",", 1)[0] == volume_id
        ):
            raise SystemExit(
                f"refusing offline mapping: VM {vmid} still references {volume_id}"
            )
PY
}

cleanup_work_dir() {
  [[ -n "$WORK_DIR" ]] || return 0
  rm -f -- "$WORK_DIR/lsblk.json" "$WORK_DIR/paths.tsv"
  if [[ -n "$MOUNT_DIR" && -d "$MOUNT_DIR" ]]; then
    rmdir -- "$MOUNT_DIR"
  fi
  rmdir -- "$WORK_DIR"
  MOUNT_DIR=""
  WORK_DIR=""
}

emit_failure_diagnostics() {
  printf '%s\n' \
    '--- app-ha offline patch failure diagnostics (no guest secrets) ---' >&2
  printf 'volume_id=%s device=%s resolved_device=%s root_partition=%s mounted=%s\n' \
    "$VOLUME_ID" "$DEVICE" "${RESOLVED_DEVICE:-none}" \
    "${ROOT_PARTITION:-none}" "$MOUNTED" >&2
  if [[ -n "$RESOLVED_DEVICE" && -b "$RESOLVED_DEVICE" ]]; then
    printf '%s\n' 'lsblk inventory:' >&2
    lsblk --json --paths \
      --output NAME,PATH,TYPE,SIZE,FSTYPE,PTTYPE,MOUNTPOINTS \
      "$RESOLVED_DEVICE" >&2 || true
    printf '%s\n' 'partition table:' >&2
    fdisk -l "$RESOLVED_DEVICE" >&2 || true
  fi
  local path
  for path in "${MAPPED_DEVICE_PATHS[@]}"; do
    [[ -b "$path" ]] || continue
    printf 'blkid %s: ' "$path" >&2
    blkid -p -o export -- "$path" 2>&1 |
      grep -E '^(DEVNAME|TYPE|PTTYPE|PART_ENTRY_TYPE|PART_ENTRY_NUMBER)=' \
      >&2 || printf '%s\n' '(no safe filesystem metadata)' >&2
  done
  if [[ "$MOUNTED" == true && -n "$MOUNT_DIR" && -d "$MOUNT_DIR" ]]; then
    printf '%s\n' 'mounted candidate:' >&2
    findmnt -rn -R "$MOUNT_DIR" >&2 || true
    printf '%s\n' 'required guest path types:' >&2
    stat -c '%F mode=%a owner=%u:%g %n' \
      "$MOUNT_DIR/etc" \
      "$MOUNT_DIR/etc/os-release" \
      "$MOUNT_DIR/usr/lib/os-release" \
      "$MOUNT_DIR/etc/fstab" \
      "$MOUNT_DIR/etc/shadow" >&2 || true
    python3 "$GUEST_TREE_HELPER" \
      --diagnose-ubuntu-root "$MOUNT_DIR" >&2 || true
  fi
  printf '%s\n' '--- end offline patch failure diagnostics ---' >&2
}

emergency_cleanup() {
  local code=$?
  local cleanup_failed=false
  trap - EXIT
  set +e
  set +x
  if [[ "$FINISHED" != true ]]; then
    emit_failure_diagnostics
  fi
  if [[ "$MOUNTED" == true && -n "$MOUNT_DIR" ]]; then
    sync
    if umount -- "$MOUNT_DIR"; then
      MOUNTED=false
    else
      printf 'ERROR: failed to unmount %s; mount state is preserved for recovery.\n' \
        "$MOUNT_DIR" >&2
      cleanup_failed=true
    fi
  fi
  if [[ "$MOUNTED" == false ]]; then
    cleanup_work_dir ||
      printf 'WARNING: could not remove app-ha offline-patch workspace %s.\n' \
        "$WORK_DIR" >&2
  fi
  if [[ "$FINISHED" != true ]]; then
    printf 'ERROR: offline patch aborted; verified attach permission was not granted.\n' >&2
  fi
  if [[ "$cleanup_failed" == true ]]; then
    printf 'ERROR: manual recovery is required before this zvol may be attached.\n' >&2
    ((code == 0)) && code=1
  fi
  exit "$code"
}

map_and_locate_root() {
  local resolved inventory paths_file mount_count record record_type record_path
  local filesystem_type
  local root_count=0
  local -a partition_candidates=()
  [[ -b "$DEVICE" ]] || die "Staging zvol is not a block device: $DEVICE"
  resolved="$(readlink -f -- "$DEVICE")"
  [[ "$resolved" =~ ^/dev/[A-Za-z0-9._/+:-]+$ && -b "$resolved" ]] ||
    die "Staging zvol resolved to an unsafe block path"
  assert_not_in_use "$DEVICE" "$resolved"
  assert_not_referenced_by_vm
  RESOLVED_DEVICE="$resolved"

  udevadm settle
  inventory="$WORK_DIR/lsblk.json"
  paths_file="$WORK_DIR/paths.tsv"
  lsblk --json --paths \
    --output NAME,PATH,TYPE,FSTYPE,MOUNTPOINTS "$resolved" >"$inventory"
  python3 - "$inventory" >"$paths_file" <<'PY'
import json
import re
import sys

value = json.load(open(sys.argv[1], encoding="utf-8"))
roots = value.get("blockdevices")
if not isinstance(roots, list) or len(roots) != 1:
    raise SystemExit("zvol inventory must contain exactly one root device")
partitions = []
all_paths = []

def walk(node):
    if not isinstance(node, dict):
        raise SystemExit("malformed lsblk inventory")
    path = node.get("path") or node.get("name")
    kind = node.get("type")
    if not isinstance(path, str) or not re.fullmatch(r"/dev/[A-Za-z0-9._/+:-]+", path):
        raise SystemExit("lsblk returned an unsafe device path")
    if kind not in {"disk", "part"}:
        raise SystemExit(f"unsupported mapped layer {kind!r}; direct partitions required")
    mounts = node.get("mountpoints")
    if isinstance(mounts, str):
        mounts = [mounts]
    if mounts and any(mounts):
        raise SystemExit(f"device is already mounted: {path}")
    all_paths.append(path)
    if kind == "part":
        partitions.append(path)
    for child in node.get("children") or []:
        walk(child)

walk(roots[0])
if not partitions:
    raise SystemExit("partition-exposing zvol has no partitions")
for path in partitions:
    print(f"PART\t{path}")
for path in all_paths:
    print(f"PATH\t{path}")
PY
  while IFS=$'\t' read -r record_type record_path; do
    [[ "$record_path" =~ ^/dev/[A-Za-z0-9._/+:-]+$ ]] ||
      die "Block inventory returned an unsafe path"
    case "$record_type" in
      PART) partition_candidates+=("$record_path") ;;
      PATH) MAPPED_DEVICE_PATHS+=("$record_path") ;;
      *) die "Block inventory returned an unknown record type" ;;
    esac
  done <"$paths_file"
  ((${#partition_candidates[@]} > 0 && ${#MAPPED_DEVICE_PATHS[@]} > 1)) ||
    die "Could not inventory the staging zvol"
  assert_not_in_use "${MAPPED_DEVICE_PATHS[@]}"

  local -a ext4_candidates=()
  for record in "${partition_candidates[@]}"; do
    filesystem_type="$(blkid -p -o value -s TYPE -- "$record" 2>/dev/null || true)"
    [[ "$filesystem_type" != ext4 ]] || ext4_candidates+=("$record")
  done
  ((${#ext4_candidates[@]} > 0)) ||
    die "zvol has no ext4 partition"

  MOUNT_DIR="$WORK_DIR/root"
  install -d -m 0700 "$MOUNT_DIR"
  for record in "${ext4_candidates[@]}"; do
    mount -o ro,noload,nodev,nosuid,noexec -- "$record" "$MOUNT_DIR"
    MOUNTED=true
    mount_count="$(findmnt -rn -R "$MOUNT_DIR" | wc -l)"
    [[ "$mount_count" == 1 ]] ||
      die "Guest filesystem unexpectedly contains nested host mounts"
    if python3 "$GUEST_TREE_HELPER" --probe-ubuntu-root "$MOUNT_DIR"; then
      ROOT_PARTITION="$record"
      ((root_count += 1))
    else
      printf 'Candidate %s was rejected as the Ubuntu root: ' "$record" >&2
      python3 "$GUEST_TREE_HELPER" \
        --diagnose-ubuntu-root "$MOUNT_DIR" >&2 || true
    fi
    umount -- "$MOUNT_DIR"
    MOUNTED=false
  done
  ((root_count == 1)) ||
    die "Expected exactly one Ubuntu ext4 root; found $root_count"
  mount -o rw,nodev,nosuid,noexec -- "$ROOT_PARTITION" "$MOUNT_DIR"
  MOUNTED=true
}

patch_guest() {
  local -a helper_args=(
    --root "$MOUNT_DIR"
    --name "$STAGING_NAME"
    --address "$ADDRESS"
    --gateway "$GATEWAY"
    --mac "$MAC"
    --dns "$DNS_CSV"
    --root-hash-file "$ROOT_HASH_FILE"
  )
  [[ -z "$SANITIZER_FILE" ]] ||
    helper_args+=(--sanitizer "$SANITIZER_FILE")
  python3 "$GUEST_TREE_HELPER" "${helper_args[@]}"
}

finish_offline_access() {
  sync
  umount -- "$MOUNT_DIR"
  MOUNTED=false
  blockdev --flushbufs "$DEVICE"
  assert_not_in_use "$DEVICE" "${MAPPED_DEVICE_PATHS[@]}"
  if findmnt -rn -S "$DEVICE" >/dev/null 2>&1; then
    die "Staging zvol remained mounted after offline patch"
  fi
  if fuser "$DEVICE" >/dev/null 2>&1; then
    die "Staging zvol retained a host writer after offline patch"
  fi
  cleanup_work_dir
  ROOT_PARTITION=""
  RESOLVED_DEVICE=""
  MAPPED_DEVICE_PATHS=()
  FINISHED=true
}

main() {
  parse_args "$@"
  require_commands \
    bash blkid blockdev fdisk findmnt flock fuser grep install lsblk mktemp mount \
    pvesh python3 readlink rm rmdir stat sync udevadm umount wc
  validate_request
  install -d -m 0755 /run/lock
  exec 9>/run/lock/app-ha-staging-offline-patch.lock
  flock -w 300 9 ||
    die "Timed out waiting for exclusive staging offline-patch lock"
  WORK_DIR="$(mktemp -d /run/app-ha-staging-offline.XXXXXX)"
  trap emergency_cleanup EXIT

  map_and_locate_root
  patch_guest
  finish_offline_access
  log "Identity, network, credentials, and clone state were patched offline"
}

if [[ "${APP_HA_STAGING_PATCH_SOURCE_ONLY:-0}" != 1 ]]; then
  main "$@"
fi
