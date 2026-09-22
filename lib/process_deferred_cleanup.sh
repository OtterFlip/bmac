#!/usr/bin/env bash

# Copyright (c) 2026 BEENTHERE VENTURES, INC.
# SPDX-License-Identifier: GPL-3.0-only

# Process schema-validated app-ha cleanup records assigned to this Proxmox
# node.  This helper is intentionally bounded and idempotent: an offline
# source or peer leaves its record pending for the next timer run.

set -Eeuo pipefail
set +x
umask 077

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
CONFIG_LIB="${SCRIPT_DIR}/config.sh"
REGISTRY="${SCRIPT_DIR}/cluster_registry.py"
HAPROXY_SYNC="${SCRIPT_DIR}/sync_haproxy_routes.sh"
TEST_MODE="${APP_HA_CLEANUP_TEST_MODE:-0}"
MAX_SECONDS=120

if [[ "$TEST_MODE" == 1 ]]; then
  LOCK_DIR="${APP_HA_LOCK_DIR:?APP_HA_LOCK_DIR is required in test mode}"
  LOCAL_NODE="${APP_HA_LOCAL_NODE:?APP_HA_LOCAL_NODE is required in test mode}"
else
  LOCK_DIR=/run/lock
  LOCAL_NODE="$(hostname -s)"
fi

log() {
  printf '[app-ha cleanup] %s\n' "$*" >&2
  command -v logger >/dev/null 2>&1 &&
    logger -t app-ha-cleanup -- "$*" >/dev/null 2>&1 || true
}

die() {
  log "ERROR: $*"
  exit 1
}

usage() {
  cat <<EOF
Usage: $0 [--max-seconds N]

Processes pending pmxcfs cleanup records for the local mox node. Work that
cannot be proved safe or whose source is offline remains pending.
EOF
}

parse_args() {
  while (($#)); do
    case "$1" in
      --max-seconds)
        (($# >= 2)) || die "--max-seconds requires a value"
        MAX_SECONDS="$2"
        shift 2
        continue
        ;;
      -h | --help)
        usage
        exit 0
        ;;
      *)
        die "unknown option: $1"
        ;;
    esac
  done
  [[ "$MAX_SECONDS" =~ ^[1-9][0-9]*$ ]] &&
    ((MAX_SECONDS >= 5 && MAX_SECONDS <= 900)) ||
    die "--max-seconds must be between 5 and 900"
}

require_file() {
  [[ -f "$1" && ! -L "$1" ]] || die "required regular file is missing: $1"
}

require_commands() {
  local command_name
  for command_name in "$@"; do
    command -v "$command_name" >/dev/null 2>&1 ||
      die "required command is unavailable: $command_name"
  done
}

registry() {
  "$REGISTRY" --state-dir "$CLUSTER_STATE_DIR" "$@"
}

deadline_reached() {
  ((SECONDS >= DEADLINE))
}

safe_zvol_dataset() {
  local volume="$1" path
  path="$(pvesm path "$volume")" || return 1
  [[ "$path" == /dev/zvol/* ]] || return 1
  printf '%s\n%s\n' "${path#/dev/zvol/}" "$path"
}

zfs_object_exists() {
  zfs list -Hp -o name "$1" >/dev/null 2>&1
}

clone_is_quiescent() {
  local path="$1" dataset="$2" fuser_status findmnt_status mounts device
  local -a devices=("$path")
  [[ "$dataset" =~ ^[A-Za-z0-9._/+:-]+$ && "$path" == /dev/zvol/* ]] ||
    return 1
  if [[ "$TEST_MODE" != 1 ]]; then
    [[ -b "$path" ]] || {
      log "refusing to remove $dataset; zvol device is not a block device"
      return 1
    }
  fi
  set +e
  mounts="$(findmnt -rn -S "$path" -o TARGET 2>/dev/null)"
  findmnt_status=$?
  set -e
  case "$findmnt_status" in
    0)
      [[ -z "$mounts" ]] || {
        log "refusing to remove $dataset; its zvol is mounted"
        return 1
      }
      ;;
    1) ;;
    *)
      log "refusing to remove $dataset; mount state could not be checked"
      return 1
      ;;
  esac
  if [[ -b "$path" ]]; then
    mounts="$(lsblk -nrpo MOUNTPOINT "$path" 2>/dev/null)" || {
      log "refusing to remove $dataset; child mount state could not be checked"
      return 1
    }
    if awk 'NF { found=1 } END { exit !found }' <<<"$mounts"; then
      log "refusing to remove $dataset; a child block device is mounted"
      return 1
    fi
    mapfile -t devices < <(lsblk -nrpo PATH "$path" 2>/dev/null) || {
      log "refusing to remove $dataset; child device state could not be checked"
      return 1
    }
    ((${#devices[@]} > 0)) || {
      log "refusing to remove $dataset; no block-device identity was returned"
      return 1
    }
  fi
  for device in "${devices[@]}"; do
    [[ "$device" == /dev/* ]] || return 1
    set +e
    fuser "$device" >/dev/null 2>&1
    fuser_status=$?
    set -e
    case "$fuser_status" in
      1) ;;
      0)
        log "refusing to remove $dataset; $device is open by a process"
        return 1
        ;;
      *)
        log "refusing to remove $dataset; open-writer state could not be checked"
        return 1
        ;;
    esac
  done
}

ha_resource_exists() {
  local vmid="$1" resources
  resources="$(pvesh get /cluster/ha/resources --output-format json)" || return 2
  python3 - "$vmid" "$resources" <<'PY'
import json
import sys

wanted = f"vm:{sys.argv[1]}"
rows = json.loads(sys.argv[2])
raise SystemExit(0 if any(row.get("sid") == wanted for row in rows) else 1)
PY
}

resource_identity_matches() {
  local resource_file="$1" resource_id="$2"
  python3 - "$resource_file" "$resource_id" <<'PY'
import json
import sys

row = json.load(open(sys.argv[1], encoding="utf-8"))
raise SystemExit(0 if row.get("id") == sys.argv[2] else 1)
PY
}

load_resource() {
  local name="$1" resource_id="$2" destination="$3"
  registry get "$name" >"$destination" 2>/dev/null || return 1
  resource_identity_matches "$destination" "$resource_id"
}

validate_live_staging_config() {
  local resource_file="$1" config_file="$2" expected_volume="$3"
  python3 - "$resource_file" "$config_file" \
    "$PRODUCTION_VM_TAG" "$STAGING_VM_TAG" "$EVICTABLE_VM_TAG" \
    "$PURPOSE_TAG_PREFIX" "$expected_volume" <<'PY'
import json
import re
import sys

resource = json.load(open(sys.argv[1], encoding="utf-8"))
config = {}
for line in open(sys.argv[2], encoding="utf-8"):
    if ": " in line:
        key, value = line.rstrip("\n").split(": ", 1)
        config[key] = value

if resource["kind"] != "staging":
    raise SystemExit("cleanup target is not staging")
if config.get("name") != resource["name"]:
    raise SystemExit("QEMU name differs from registry")
if config.get("template", "0") == "1":
    raise SystemExit("QEMU target is a template")
if config.get("onboot", "0") != "0":
    raise SystemExit("QEMU cleanup target has onboot enabled")
volume = resource["proxmox"]["volume_id"]
expected_volume = sys.argv[7]
if volume is not None and volume != expected_volume:
    raise SystemExit("registered staging volume differs from cleanup plan")
tags = {tag for tag in re.split(r"[;,]", config.get("tags", "")) if tag}
expected_tags = {
    sys.argv[4],
    sys.argv[5],
    f"{sys.argv[6]}{resource['purpose']['slug']}",
}
if tags != expected_tags or sys.argv[3] in tags:
    raise SystemExit("QEMU tags do not exactly match the staging registry role")
disk_keys = {
    key
    for key in config
    if re.fullmatch(
        r"(?:(?:scsi|sata|virtio|ide|unused|efidisk|tpmstate)[0-9]+)",
        key,
    )
}
if disk_keys not in (set(), {"efidisk0"}, {"scsi0", "efidisk0"}):
    raise SystemExit("QEMU disk keys are not an allowed staging shell/config")
if "scsi0" in disk_keys and config["scsi0"].split(",", 1)[0] != expected_volume:
    raise SystemExit("QEMU root disk differs from registry")
if "efidisk0" in disk_keys:
    storage = expected_volume.split(":", 1)[0]
    efi_volume = config["efidisk0"].split(",", 1)[0]
    if not re.fullmatch(
        rf"{re.escape(storage)}:vm-{resource['vmid']}-disk-[1-9][0-9]*",
        efi_volume,
    ) or efi_volume == expected_volume:
        raise SystemExit("QEMU EFI vars disk is not fresh VM-owned storage")
PY
}

registered_or_deferred_volume() {
  local resource_file="$1" cleanup_json
  cleanup_json="$(registry list --record-type cleanup)" || return 1
  python3 - "$resource_file" "$LOCAL_NODE" "$cleanup_json" <<'PY'
import json
import re
import sys

resource = json.load(open(sys.argv[1], encoding="utf-8"))
node = sys.argv[2]
volume = resource["proxmox"]["volume_id"]
if volume is not None:
    print(volume)
    raise SystemExit(0)
matches = [
    row["target"]
    for row in json.loads(sys.argv[3])
    if row["resource_id"] == resource["id"]
    and row["node"] == node
    and row["action"] == "destroy-volume"
    and re.fullmatch(
        rf"[A-Za-z0-9][A-Za-z0-9_.+-]{{0,63}}:vm-{resource['vmid']}-disk-[0-9]+",
        row["target"],
    )
]
if len(set(matches)) != 1:
    raise SystemExit("staging volume identity is absent or ambiguous")
print(matches[0])
PY
}

validate_staging_volume_for_config_removal() {
  local resource_file="$1" volume="$2"
  local -a fields=() target_fields=() source_fields=()
  mapfile -d '' -t fields < <(
    staging_storage_fields "$resource_file" "$volume"
  )
  ((${#fields[@]} == 5)) || return 1
  local name="${fields[1]}" source_volume="${fields[2]}"
  local snapshot="${fields[3]}" expected_guid="${fields[4]}"
  local target_dataset target_path source_dataset origin guid
  mapfile -t target_fields < <(safe_zvol_dataset "$volume") || return 1
  ((${#target_fields[@]} == 2)) || return 1
  target_dataset="${target_fields[0]}"
  target_path="${target_fields[1]}"
  zfs_object_exists "$target_dataset" || return 0
  [[ "$(zfs list -Hp -o type "$target_dataset")" == volume ]] || return 1
  mapfile -t source_fields < <(safe_zvol_dataset "$source_volume") || return 1
  ((${#source_fields[@]} == 2)) || return 1
  source_dataset="${source_fields[0]}"
  [[ "$snapshot" =~ ^stg-base-"$name"-[A-Za-z0-9._-]+$ &&
    "$snapshot" != __replicate_* ]] || return 1
  origin="$(zfs get -Hp -o value origin "$target_dataset")" || return 1
  [[ "$origin" == "${source_dataset}@${snapshot}" ]] || return 1
  guid="$(zfs get -Hp -o value guid "${source_dataset}@${snapshot}")" ||
    return 1
  [[ "$guid" == "$expected_guid" ]] || return 1
  clone_is_quiescent "$target_path" "$target_dataset"
}

volume_unreferenced_by_any_vm() {
  local volume="$1" rows vmids vmid config
  rows="$(pvesh get /cluster/resources --type vm --output-format json)" ||
    return 1
  vmids="$(
    python3 - "$rows" <<'PY'
import json
import sys

rows = json.loads(sys.argv[1])
if not isinstance(rows, list):
    raise SystemExit("malformed cluster VM inventory")
for row in rows:
    if row.get("type") == "qemu" and str(row.get("vmid", "")).isdigit():
        print(row["vmid"])
PY
  )" || return 1
  while IFS= read -r vmid; do
    [[ -n "$vmid" ]] || continue
    config="$(qm config "$vmid")" || return 1
    python3 - "$volume" "$vmid" "$config" <<'PY' || return 1
import re
import sys

volume, vmid, text = sys.argv[1:]
for line in text.splitlines():
    key, separator, value = line.partition(": ")
    if (
        separator
        and re.fullmatch(
            r"(?:(?:scsi|sata|virtio|ide|unused|efidisk|tpmstate)[0-9]+)",
            key,
        )
        and value.split(",", 1)[0] == volume
    ):
        raise SystemExit(f"VM {vmid} still references {volume}")
PY
  done <<<"$vmids"
}

destroy_vm_action() {
  local cleanup_id="$1" target="$2" resource_file="$3"
  local -a fields=()
  mapfile -d '' -t fields < <(
    python3 - "$resource_file" "$target" "$LOCAL_NODE" <<'PY'
import json
import sys

row = json.load(open(sys.argv[1], encoding="utf-8"))
owner = row["owner_node"] or row["placement"][0]
expected = f"vm:{row['vmid']}"
if row["kind"] != "staging" or sys.argv[2] != expected or owner != sys.argv[3]:
    raise SystemExit("destroy-vm cleanup identity differs from registry")
if row["state"] != "cleanup_pending":
    raise SystemExit("staging cleanup plan is not committed")
if row["routes_enabled"]:
    raise SystemExit("staging route must be disabled before VM destruction")
for value in (row["vmid"], row["name"]):
    sys.stdout.buffer.write(str(value).encode() + b"\0")
PY
  ) || return 75
  ((${#fields[@]} == 2)) || return 75
  local vmid="${fields[0]}" name="${fields[1]}" config status expected_volume

  if ! qm status "$vmid" >/dev/null 2>&1; then
    log "$cleanup_id: VM $vmid is already absent"
    return 0
  fi
  config="$(mktemp "${WORK_DIR}/qemu-${vmid}.XXXXXX")"
  qm config "$vmid" >"$config" || return 75
  expected_volume="$(registered_or_deferred_volume "$resource_file")" ||
    return 75
  validate_live_staging_config "$resource_file" "$config" "$expected_volume" || {
    log "$cleanup_id: refusing VM destruction; live identity is ambiguous"
    return 75
  }
  if ha_resource_exists "$vmid"; then
    log "$cleanup_id: refusing VM destruction; $vmid is HA-managed"
    return 75
  else
    status=$?
    ((status == 1)) || return 75
  fi
  status="$(qm status "$vmid" 2>/dev/null)" || return 75
  if [[ "$status" != "status: stopped" ]]; then
    log "$cleanup_id: force-stopping disposable staging VM $vmid ($name)"
    qm stop "$vmid" --skiplock true --timeout 30 || return 75
  fi
  [[ "$(qm status "$vmid" 2>/dev/null)" == "status: stopped" ]] || return 75
  validate_staging_volume_for_config_removal \
    "$resource_file" "$expected_volume" || {
    log "$cleanup_id: refusing VM destruction; linked-clone identity is unsafe"
    return 75
  }
  qm destroy "$vmid" --purge 1 --destroy-unreferenced-disks 0 \
    --skiplock true || return 75
  ! qm status "$vmid" >/dev/null 2>&1 || return 75
  log "$cleanup_id: destroyed staging VM config $vmid ($name)"
}

staging_storage_fields() {
  local resource_file="$1" target="$2"
  python3 - "$resource_file" "$target" "$LOCAL_NODE" <<'PY'
import json
import re
import sys

row = json.load(open(sys.argv[1], encoding="utf-8"))
snapshot = row["proxmox"]["snapshot"]
owner = row["owner_node"] or row["placement"][0]
volume = row["proxmox"]["volume_id"]
if (
    row["kind"] != "staging"
    or owner != sys.argv[3]
    or snapshot is None
    or snapshot.get("verified") is not True
):
    raise SystemExit("staging storage identity is incomplete")
if row["state"] != "cleanup_pending":
    raise SystemExit("staging cleanup plan is not committed")
if row["routes_enabled"]:
    raise SystemExit("staging route must be disabled before storage cleanup")
if volume is not None and volume != sys.argv[2]:
    raise SystemExit("cleanup volume differs from registry")
if not re.fullmatch(
    rf"[A-Za-z0-9][A-Za-z0-9_.+-]{{0,63}}:vm-{row['vmid']}-disk-[0-9]+",
    sys.argv[2],
):
    raise SystemExit("cleanup volume does not belong to the staging VMID")
source_name = snapshot["source_resource"]
if source_name != row["source"]:
    raise SystemExit("snapshot source differs from staging source")
values = (
    row["vmid"],
    row["name"],
    snapshot["source_volume_id"],
    snapshot["name"],
    snapshot["guids"].get(sys.argv[3], ""),
)
if not values[-1]:
    raise SystemExit("snapshot lacks a GUID for the cleanup node")
for value in values:
    sys.stdout.buffer.write(str(value).encode() + b"\0")
PY
}

destroy_volume_action() {
  local cleanup_id="$1" target="$2" resource_file="$3"
  local -a fields=() target_fields=() source_fields=()
  mapfile -d '' -t fields < <(
    staging_storage_fields "$resource_file" "$target"
  ) || return 75
  ((${#fields[@]} == 5)) || return 75
  local vmid="${fields[0]}" name="${fields[1]}" source_volume="${fields[2]}"
  local snapshot="${fields[3]}" expected_guid="${fields[4]}"
  local target_dataset target_path source_dataset origin guid

  ! qm status "$vmid" >/dev/null 2>&1 || {
    log "$cleanup_id: VM config $vmid still exists; retaining its volume"
    return 75
  }
  mapfile -t target_fields < <(safe_zvol_dataset "$target") || return 75
  ((${#target_fields[@]} == 2)) || return 75
  target_dataset="${target_fields[0]}"
  target_path="${target_fields[1]}"
  if ! zfs_object_exists "$target_dataset"; then
    log "$cleanup_id: staging volume $target is already absent"
    return 0
  fi
  [[ "$(zfs list -Hp -o type "$target_dataset")" == volume ]] || return 75
  mapfile -t source_fields < <(safe_zvol_dataset "$source_volume") || return 75
  ((${#source_fields[@]} == 2)) || return 75
  source_dataset="${source_fields[0]}"
  [[ "$snapshot" =~ ^stg-base-"$name"-[A-Za-z0-9._-]+$ &&
    "$snapshot" != __replicate_* ]] || return 75
  origin="$(zfs get -Hp -o value origin "$target_dataset")" || return 75
  [[ "$origin" == "${source_dataset}@${snapshot}" ]] || {
    log "$cleanup_id: refusing volume destruction; clone origin changed"
    return 75
  }
  guid="$(zfs get -Hp -o value guid "${source_dataset}@${snapshot}")" ||
    return 75
  [[ "$guid" == "$expected_guid" ]] || return 75
  clone_is_quiescent "$target_path" "$target_dataset" || return 75
  volume_unreferenced_by_any_vm "$target" || {
    log "$cleanup_id: another VM still references the staging clone"
    return 75
  }
  zfs destroy "$target_dataset" || return 75
  ! zfs_object_exists "$target_dataset" || return 75
  log "$cleanup_id: destroyed linked-clone volume for $name"
}

pending_local_payload_cleanup() {
  local resource_id="$1" cleanup_json
  cleanup_json="$(registry list --record-type cleanup)" || return 0
  python3 - "$resource_id" "$cleanup_json" <<'PY'
import json
import sys

resource_id = sys.argv[1]
rows = json.loads(sys.argv[2])
blocked = any(
    row["resource_id"] == resource_id
    and row["state"] == "pending"
    and row["action"] in {"destroy-vm", "destroy-volume"}
    for row in rows
)
raise SystemExit(0 if blocked else 1)
PY
}

snapshot_cleanup_fields() {
  local resource_id="$1" target="$2"
  local resources_json
  resources_json="$(registry list --record-type resources)" || return 1
  python3 - "$resource_id" "$target" "$LOCAL_NODE" "$resources_json" <<'PY'
import json
import re
import sys

resource_id, target, node = sys.argv[1:4]
rows = json.loads(sys.argv[4])
matches = [row for row in rows if row["id"] == resource_id]
if len(matches) != 1:
    raise SystemExit("cleanup resource is absent or ambiguous")
stage = matches[0]
if stage["kind"] != "staging":
    raise SystemExit("snapshot cleanup resource is not staging")
if stage["state"] != "cleanup_pending":
    raise SystemExit("staging cleanup plan is not committed")
if stage["routes_enabled"]:
    raise SystemExit("staging route must be disabled before snapshot cleanup")
snapshot = stage["proxmox"]["snapshot"]
if snapshot is None:
    raise SystemExit("staging snapshot dependency is absent")
sources = [
    row
    for row in rows
    if row["name"] == stage["source"] and row["kind"] == "production"
]
if len(sources) != 1:
    raise SystemExit("production source is absent or ambiguous")
source = sources[0]
name = snapshot["name"]
if (
    snapshot["source_resource"] != source["name"]
    or snapshot["source_volume_id"] != source["proxmox"]["volume_id"]
    or not re.fullmatch(rf"stg-base-{re.escape(stage['name'])}-[A-Za-z0-9._-]+", name)
    or name.startswith("__replicate_")
    or target != f"vm-{source['vmid']}@{name}"
):
    raise SystemExit("snapshot cleanup target differs from registry dependency")
values = (
    stage["name"],
    stage["vmid"],
    source["name"],
    source["vmid"],
    name,
    stage["proxmox"]["volume_id"] or "",
    "\x1f".join(snapshot["source_volume_ids"]),
    json.dumps(snapshot["volume_guids"], sort_keys=True, separators=(",", ":")),
    "yes" if snapshot["verified"] else "no",
)
for value in values:
    sys.stdout.buffer.write(str(value).encode() + b"\0")
PY
}

online_source_node() {
  local source_name="$1" source_vmid="$2" nodes_json resources_json
  nodes_json="$(pvesh get /nodes --output-format json)" || return 1
  resources_json="$(pvesh get /cluster/resources --type vm --output-format json)" ||
    return 1
  python3 - "$source_name" "$source_vmid" "$nodes_json" "$resources_json" <<'PY'
import json
import sys

name, raw_vmid = sys.argv[1:3]
vmid = int(raw_vmid)
online = {
    row.get("node")
    for row in json.loads(sys.argv[3])
    if row.get("status") == "online"
}
matches = [
    row
    for row in json.loads(sys.argv[4])
    if row.get("type") == "qemu"
    and row.get("name") == name
    and row.get("vmid") in (vmid, str(vmid))
]
if len(matches) != 1 or matches[0].get("node") not in online:
    raise SystemExit(1)
print(matches[0]["node"])
PY
}

snapshot_config_presence() {
  local source_node="$1" source_vmid="$2" snapshot="$3" staging_name="$4"
  local rows
  rows="$(pvesh get \
    "/nodes/${source_node}/qemu/${source_vmid}/snapshot" \
    --output-format json)" || return 1
  python3 - "$snapshot" "$staging_name" "$rows" <<'PY'
import json
import sys

snapshot = sys.argv[1]
description = f"app-ha staging base for {sys.argv[2]}"
matches = [row for row in json.loads(sys.argv[3]) if row.get("name") == snapshot]
if len(matches) > 1:
    raise SystemExit("duplicate Proxmox snapshot identity")
if not matches:
    print("absent")
else:
    if matches[0].get("description") != description:
        raise SystemExit("Proxmox snapshot description differs from intent")
    print("present")
PY
}

trigger_source_replication() {
  local source_node="$1" source_vmid="$2" rows jobs job
  [[ "$source_node" == "$LOCAL_NODE" ]] || return 1
  rows="$(pvesh get /cluster/replication --output-format json)" || return 1
  jobs="$(
    python3 - "$source_vmid" "$rows" <<'PY'
import json
import sys

vmid = int(sys.argv[1])
matches = [
    row for row in json.loads(sys.argv[2])
    if row.get("guest") in (vmid, str(vmid))
]
for row in matches:
    job = row.get("id")
    if not isinstance(job, str) or not job:
        raise SystemExit("malformed production replication job")
    print(job)
PY
  )" || return 1
  while IFS= read -r job; do
    [[ -n "$job" ]] || continue
    pvesr schedule-now "$job" || return 1
  done <<<"$jobs"
}

delete_snapshot_action() {
  local cleanup_id="$1" resource_id="$2" target="$3" resource_file="$4"
  local -a fields=() volume_fields=() staging_volume_fields=() source_volumes=()
  mapfile -d '' -t fields < <(
    snapshot_cleanup_fields "$resource_id" "$target"
  ) || return 75
  ((${#fields[@]} == 9)) || return 75
  local staging_name="${fields[0]}" staging_vmid="${fields[1]}"
  local source_name="${fields[2]}" source_vmid="${fields[3]}"
  local snapshot="${fields[4]}" staging_volume="${fields[5]}"
  IFS=$'\x1f' read -r -a source_volumes <<<"${fields[6]}"
  local volume_guids_json="${fields[7]}" verified="${fields[8]}"
  local source_node presence source_volume source_dataset snapshot_dataset
  local expected_guid guid clones
  local delete_result
  local snapshot_present=false

  if pending_local_payload_cleanup "$resource_id"; then
    log "$cleanup_id: local staging payload cleanup is still pending"
    return 75
  fi
  if [[ -n "$staging_volume" ]]; then
    mapfile -t staging_volume_fields < <(
      safe_zvol_dataset "$staging_volume" 2>/dev/null
    ) || true
    if ((${#staging_volume_fields[@]} == 2)) &&
      zfs_object_exists "${staging_volume_fields[0]}"; then
      log "$cleanup_id: linked clone still exists; retaining source snapshot"
      return 75
    fi
  fi

  ((${#source_volumes[@]} > 0)) || return 75
  for source_volume in "${source_volumes[@]}"; do
    mapfile -t volume_fields < <(safe_zvol_dataset "$source_volume") || return 75
    ((${#volume_fields[@]} == 2)) || return 75
    source_dataset="${volume_fields[0]}"
    snapshot_dataset="${source_dataset}@${snapshot}"
    if ! zfs_object_exists "$snapshot_dataset"; then
      continue
    fi
    snapshot_present=true
    [[ "$(zfs list -Hp -o type "$snapshot_dataset")" == snapshot ]] ||
      return 75
    if [[ "$verified" == yes ]]; then
      expected_guid="$(
        python3 - "$volume_guids_json" "$source_volume" "$LOCAL_NODE" <<'PY'
import json
import sys
print(json.loads(sys.argv[1]).get(sys.argv[2], {}).get(sys.argv[3], ""))
PY
      )" || return 75
      [[ -n "$expected_guid" ]] || return 75
      guid="$(zfs get -Hp -o value guid "$snapshot_dataset")" || return 75
      [[ "$guid" == "$expected_guid" ]] || {
        log "$cleanup_id: snapshot GUID differs from the registry"
        return 75
      }
    fi
    clones="$(zfs get -Hp -o value clones "$snapshot_dataset")" || return 75
    [[ "$clones" == "-" || -z "$clones" ]] || {
      log "$cleanup_id: snapshot still has clone dependencies: $clones"
      return 75
    }
  done

  source_node="$(online_source_node "$source_name" "$source_vmid")" || {
    log "$cleanup_id: production source $source_name is offline; leaving cleanup pending"
    return 75
  }
  presence="$(
    snapshot_config_presence \
      "$source_node" "$source_vmid" "$snapshot" "$staging_name"
  )" || return 75
  case "$presence" in
    present)
      if [[ "$source_node" != "$LOCAL_NODE" ]]; then
        log "$cleanup_id: waiting for $source_node to remove the Proxmox snapshot metadata"
        return 75
      fi
      log "$cleanup_id: deleting registered Proxmox snapshot $source_name@$snapshot"
      delete_result="$(
        pvesh delete \
          "/nodes/${source_node}/qemu/${source_vmid}/snapshot/${snapshot}"
      )" || return 75
      if [[ "$delete_result" == *UPID:* ]]; then
        log "$cleanup_id: Proxmox snapshot task was queued; a later run will verify it"
        return 75
      fi
      [[ "$(
        snapshot_config_presence \
          "$source_node" "$source_vmid" "$snapshot" "$staging_name"
      )" == absent ]] || return 75
      ;;
    absent) ;;
  esac

  snapshot_present=false
  for source_volume in "${source_volumes[@]}"; do
    mapfile -t volume_fields < <(safe_zvol_dataset "$source_volume") || return 75
    ((${#volume_fields[@]} == 2)) || return 75
    if zfs_object_exists "${volume_fields[0]}@${snapshot}"; then
      snapshot_present=true
      break
    fi
  done

  if [[ "$source_node" == "$LOCAL_NODE" ]]; then
    if [[ "$snapshot_present" == true ]]; then
      log "$cleanup_id: waiting for Proxmox snapshot deletion to finish locally"
      return 75
    fi
    trigger_source_replication "$source_node" "$source_vmid" || {
      log "$cleanup_id: could not trigger production replication cleanup"
      return 75
    }
    log "$cleanup_id: Proxmox snapshot is absent; replication cleanup was triggered"
    return 0
  fi
  if [[ "$snapshot_present" == true ]]; then
    log "$cleanup_id: waiting for Proxmox replication to remove snapshot copies"
    return 75
  fi
  log "$cleanup_id: all tracked snapshot copies are absent on $LOCAL_NODE"
}

remove_replication_action() {
  local cleanup_id="$1" target="$2" resource_file="$3" jobs
  local kind vmid match
  read -r kind vmid < <(
    python3 - "$resource_file" <<'PY'
import json
import sys
row = json.load(open(sys.argv[1], encoding="utf-8"))
print(row["kind"], row["vmid"])
PY
  ) || return 75
  [[ "$target" == "${vmid}-"* ]] || return 75
  if [[ "$kind" == production ]]; then
    log "$cleanup_id: production replication removal requires operator review"
    return 75
  fi
  jobs="$(pvesh get /cluster/replication --output-format json)" || return 75
  match="$(
    python3 - "$target" "$vmid" "$jobs" <<'PY'
import json
import sys
target, raw_vmid = sys.argv[1:3]
vmid = int(raw_vmid)
matches = [
    row for row in json.loads(sys.argv[3])
    if row.get("id") == target and row.get("guest") in (vmid, str(vmid))
]
if len(matches) > 1:
    raise SystemExit("ambiguous replication job")
print("present" if matches else "absent")
PY
  )" || return 75
  if [[ "$match" == present ]]; then
    pvesh delete "/cluster/replication/${target}" || return 75
  fi
  log "$cleanup_id: staging replication job $target is absent"
}

route_is_disabled() {
  local resource_file="$1" target="$2"
  python3 - "$resource_file" "$target" <<'PY'
import json
import sys
row = json.load(open(sys.argv[1], encoding="utf-8"))
raise SystemExit(
    0
    if row["kind"] == "staging"
    and row["name"] == sys.argv[2]
    and row["state"] == "cleanup_pending"
    and row["routes_enabled"] is False
    else 1
)
PY
}

run_with_lifecycle_lock() {
  local status=0
  exec {payload_lock_fd}>"${LOCK_DIR}/app-ha-guest-role-hook.lock"
  if ! flock -n "$payload_lock_fd"; then
    log "guest lifecycle action is active; deferring this payload record"
    exec {payload_lock_fd}>&-
    return 75
  fi
  "$@" || status=$?
  flock -u "$payload_lock_fd"
  exec {payload_lock_fd}>&-
  return "$status"
}

mark_completed() {
  (($# > 0)) || return 0
  python3 - "$@" <<'PY' |
import json
import sys
json.dump({"schema_version": 1, "cleanup_completed": sys.argv[1:]}, sys.stdout)
PY
    registry reconcile --observed - --apply >/dev/null
}

finalize_ready_resources() {
  local resources_file="${WORK_DIR}/finalize-resources.json"
  local name resource_id index
  local -a fields=()
  registry list --record-type resources >"$resources_file" || return 1
  mapfile -d '' -t fields < <(
    python3 - "$resources_file" <<'PY'
import json
import sys

for row in json.load(open(sys.argv[1], encoding="utf-8")):
    if row["kind"] == "staging" and row["state"] == "cleanup_pending":
        for value in (row["name"], row["id"]):
            sys.stdout.buffer.write(str(value).encode() + b"\0")
PY
  )
  ((${#fields[@]} % 2 == 0)) || return 1
  for ((index = 0; index < ${#fields[@]}; index += 2)); do
    name="${fields[index]}"
    resource_id="${fields[index + 1]}"
    if registry finalize-staging-cleanup "$name" \
      --resource-id "$resource_id" >/dev/null 2>&1; then
      log "released fully cleaned staging allocation $name"
    fi
  done
}

flush_route_cleanup() {
  (($# > 0)) || return 0
  if "$HAPROXY_SYNC"; then
    mark_completed "$@" ||
      log "HAProxy was synchronized but route completion could not be recorded"
  else
    log "HAProxy synchronization failed; route cleanup remains pending"
  fi
}

process_records() {
  local cleanup_file="${WORK_DIR}/cleanup.json" resource_file
  local cleanup_id name resource_id action target status
  local -a fields=() route_ids=()
  registry list --record-type cleanup >"$cleanup_file" || return 1
  mapfile -d '' -t fields < <(
    python3 - "$cleanup_file" "$LOCAL_NODE" <<'PY'
import json
import sys

priority = {
    "remove-route": 5,
    "destroy-vm": 10,
    "destroy-volume": 20,
    "delete-snapshot": 30,
    "remove-replication": 40,
}
rows = json.load(open(sys.argv[1], encoding="utf-8"))
rows = sorted(
    (
        row for row in rows
        if row["node"] == sys.argv[2] and row["state"] == "pending"
    ),
    key=lambda row: (priority[row["action"]], row["created_at"], row["id"]),
)
for row in rows:
    for value in (
        row["id"],
        row["resource"],
        row["resource_id"],
        row["action"],
        row["target"],
    ):
        sys.stdout.buffer.write(str(value).encode() + b"\0")
PY
  )
  ((${#fields[@]} % 5 == 0)) || die "could not parse deferred cleanup records"

  local index
  for ((index = 0; index < ${#fields[@]}; index += 5)); do
    deadline_reached && {
      log "bounded cleanup window elapsed; remaining records stay pending"
      break
    }
    cleanup_id="${fields[index]}"
    name="${fields[index + 1]}"
    resource_id="${fields[index + 2]}"
    action="${fields[index + 3]}"
    target="${fields[index + 4]}"
    if [[ "$action" != remove-route && ${#route_ids[@]} -gt 0 ]]; then
      flush_route_cleanup "${route_ids[@]}"
      route_ids=()
    fi
    resource_file="${WORK_DIR}/resource-${cleanup_id}.json"
    if ! load_resource "$name" "$resource_id" "$resource_file"; then
      log "$cleanup_id: resource identity is absent or changed; refusing action"
      continue
    fi
    status=0
    case "$action" in
      destroy-vm)
        run_with_lifecycle_lock \
          destroy_vm_action "$cleanup_id" "$target" "$resource_file" ||
          status=$?
        ;;
      destroy-volume)
        run_with_lifecycle_lock \
          destroy_volume_action "$cleanup_id" "$target" "$resource_file" ||
          status=$?
        ;;
      delete-snapshot)
        delete_snapshot_action \
          "$cleanup_id" "$resource_id" "$target" "$resource_file" || status=$?
        ;;
      remove-replication)
        remove_replication_action "$cleanup_id" "$target" "$resource_file" ||
          status=$?
        ;;
      remove-route)
        route_is_disabled "$resource_file" "$target" || status=75
        ((status != 0)) || route_ids+=("$cleanup_id")
        continue
        ;;
      *)
        status=75
        ;;
    esac
    if ((status == 0)); then
      mark_completed "$cleanup_id" || {
        log "$cleanup_id: action succeeded but completion could not be recorded"
        continue
      }
    else
      log "$cleanup_id: action remains pending"
    fi
  done

  if ((${#route_ids[@]} > 0)) && ! deadline_reached; then
    flush_route_cleanup "${route_ids[@]}"
  fi
}

main() {
  parse_args "$@"
  require_file "$CONFIG_LIB"
  # shellcheck source=./config.sh
  source "$CONFIG_LIB"
  load_proxmox_config --no-secrets ||
    die "could not load installed cluster.conf without secrets"
  require_vars CLUSTER_STATE_DIR PRODUCTION_VM_TAG STAGING_VM_TAG \
    EVICTABLE_VM_TAG PURPOSE_TAG_PREFIX
  [[ "$LOCAL_NODE" =~ ^mox([1-9]|10)$ ]] ||
    die "local hostname must be mox1 through mox10"
  local role_tag
  for role_tag in \
    "$PRODUCTION_VM_TAG" "$STAGING_VM_TAG" "$EVICTABLE_VM_TAG"; do
    [[ "$role_tag" =~ ^[A-Za-z0-9][A-Za-z0-9_.:+/-]{0,63}$ ]] ||
      die "configured guest role tag is unsafe"
  done

  # During first-host setup the installed timer may start before the registry
  # has been initialized. There is nothing safe or useful to process yet.
  if [[ ! -f "${CLUSTER_STATE_DIR}/policy.json" ]]; then
    log "registry is not initialized; nothing to process"
    return 0
  fi

  require_file "$REGISTRY"
  require_file "$HAPROXY_SYNC"
  require_commands flock findmnt fuser lsblk pvesh pvesm pvesr python3 qm zfs
  install -d -m 0755 "$LOCK_DIR"
  exec 9>"${LOCK_DIR}/app-ha-deferred-cleanup.lock"
  flock -n 9 || {
    log "another deferred-cleanup worker is active"
    return 0
  }
  if [[ "$TEST_MODE" == 1 ]]; then
    WORK_DIR="$(mktemp -d "${APP_HA_WORK_DIR:-/tmp}/app-ha-cleanup.XXXXXX")"
  else
    WORK_DIR="$(mktemp -d /run/app-ha-deferred-cleanup.XXXXXX)"
  fi
  trap 'rm -rf -- "${WORK_DIR:-}"' EXIT
  DEADLINE=$((SECONDS + MAX_SECONDS))
  process_records
  finalize_ready_resources
}

if [[ "${BASH_SOURCE[0]}" == "$0" ]]; then
  main "$@"
fi
