#!/usr/bin/env bash

# Copyright (c) 2026 BEENTHERE VENTURES, INC.
# SPDX-License-Identifier: GPL-3.0-only

# Generic app-ha QEMU lifecycle hook. Attach this one hook to registered
# production prodN and disposable staging stageNprodN guests.

set -Eeuo pipefail
set +x
umask 077

TEST_MODE="${APP_HA_HOOK_TEST_MODE:-0}"
if [[ "$TEST_MODE" == 1 ]]; then
  INSTALL_LIB="${APP_HA_INSTALL_LIB:?APP_HA_INSTALL_LIB is required in test mode}"
  LOCK_DIR="${APP_HA_LOCK_DIR:?APP_HA_LOCK_DIR is required in test mode}"
  RESERVATION_DIR="${APP_HA_RESERVATION_DIR:?APP_HA_RESERVATION_DIR is required in test mode}"
  WORK_ROOT="${APP_HA_WORK_DIR:-/tmp}"
  LOCAL_NODE="${APP_HA_LOCAL_NODE:?APP_HA_LOCAL_NODE is required in test mode}"
else
  INSTALL_LIB=/usr/local/lib/app-ha-proxmox/lib
  LOCK_DIR=/run/lock
  RESERVATION_DIR=/run/app-ha-production-starting
  WORK_ROOT=/run
  LOCAL_NODE="$(hostname -s)"
fi

CONFIG_LIB="${INSTALL_LIB}/config.sh"
REGISTRY="${INSTALL_LIB}/cluster_registry.py"
DEFERRED_CLEANUP_SERVICE=app-ha-deferred-cleanup.service
RESERVATION_TTL_SECONDS=900

VMID="${1:-}"
PHASE="${2:-}"
RESERVATION_FILE="${RESERVATION_DIR}/${VMID}.reservation"
PRESERVE_RESERVATION=false
WORK_DIR=""

declare -a STAGING_CANDIDATES=()

log() {
  printf '[app-ha guest hook] %s\n' "$*" >&2
  command -v logger >/dev/null 2>&1 &&
    logger -t app-ha-guest-hook -- "$*" >/dev/null 2>&1 || true
}

die() {
  log "ERROR: $*"
  exit 1
}

cleanup() {
  local code=$?
  trap - EXIT
  if [[ "$PRESERVE_RESERVATION" != true ]]; then
    rm -f -- "$RESERVATION_FILE"
  fi
  [[ -z "$WORK_DIR" ]] || rm -rf -- "$WORK_DIR"
  exit "$code"
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

config_value() {
  local config_file="$1" key="$2"
  awk -v prefix="${key}: " \
    'index($0, prefix) == 1 { print substr($0, length(prefix) + 1); exit }' \
    "$config_file"
}

has_tag() {
  local tags="$1" wanted="$2"
  [[ ";${tags//,/;};" == *";${wanted};"* ]]
}

is_production_name() {
  [[ "$1" =~ ^prod[1-9][0-9]*$ ]]
}

is_staging_name() {
  [[ "$1" =~ ^stage[1-9][0-9]*prod[1-9][0-9]*$ ]]
}

qm_config_to() {
  local vmid="$1" destination="$2"
  qm config "$vmid" >"$destination"
}

vm_status() {
  qm status "$1" 2>/dev/null
}

vm_is_stopped() {
  [[ "$(vm_status "$1")" == "status: stopped" ]]
}

list_local_vmids() {
  local rows
  rows="$(pvesh get "/nodes/${LOCAL_NODE}/qemu" --output-format json)" ||
    return 1
  python3 - "$rows" <<'PY'
import json
import sys

for row in json.loads(sys.argv[1]):
    vmid = row.get("vmid")
    if isinstance(vmid, int) and vmid > 0:
        print(vmid)
PY
}

list_configured_nodes() {
  local rows
  rows="$(pvesh get /nodes --output-format json)" || return 1
  python3 - "$MAX_MOX_HOSTS" "$rows" <<'PY'
import json
import re
import sys

maximum = int(sys.argv[1])
rows = json.loads(sys.argv[2])
if not isinstance(rows, list):
    raise SystemExit("pvesh /nodes did not return an array")
nodes = []
seen = set()
for row in rows:
    if not isinstance(row, dict):
        raise SystemExit("malformed configured node row")
    node = row.get("node", row.get("name"))
    match = re.fullmatch(r"mox([1-9]|10)", str(node))
    if match is None or int(match.group(1)) > maximum:
        raise SystemExit(f"unexpected configured node: {node!r}")
    if node in seen:
        raise SystemExit(f"duplicate configured node: {node}")
    seen.add(node)
    nodes.append(node)
if not nodes:
    raise SystemExit("pvesh /nodes returned no configured mox nodes")
print(*sorted(nodes, key=lambda value: int(value[3:])), sep="\n")
PY
}

array_contains() {
  local wanted="$1"
  shift
  local value
  for value in "$@"; do
    [[ "$value" != "$wanted" ]] || return 0
  done
  return 1
}

vm_is_local_qemu() {
  local wanted="$1" rows
  rows="$(pvesh get "/nodes/${LOCAL_NODE}/qemu" --output-format json)" ||
    return 2
  python3 - "$wanted" "$rows" <<'PY'
import json
import sys

wanted = int(sys.argv[1])
matches = [
    row for row in json.loads(sys.argv[2])
    if row.get("vmid") in (wanted, str(wanted))
]
raise SystemExit(0 if len(matches) == 1 else 1)
PY
}

vm_is_ha() {
  local wanted="$1" rows
  rows="$(pvesh get /cluster/ha/resources --output-format json)" || return 2
  python3 - "$wanted" "$rows" <<'PY'
import json
import sys

wanted = f"vm:{sys.argv[1]}"
matches = [row for row in json.loads(sys.argv[2]) if row.get("sid") == wanted]
raise SystemExit(0 if matches else 1)
PY
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

write_reservation() {
  local name="$1" temporary
  install -d -m 0700 "$RESERVATION_DIR"
  temporary="$(mktemp "${RESERVATION_DIR}/.${VMID}.XXXXXX")"
  printf 'vmid=%s\nname=%s\nnode=%s\ncreated_epoch=%s\n' \
    "$VMID" "$name" "$LOCAL_NODE" "$(date +%s)" >"$temporary"
  chmod 0600 "$temporary"
  mv -f -- "$temporary" "$RESERVATION_FILE"
}

validate_reservation_file() {
  local path="$1" filename vmid line
  filename="${path##*/}"
  [[ "$filename" =~ ^([1-9][0-9]*)[.]reservation$ ]] || return 1
  vmid="${BASH_REMATCH[1]}"
  IFS= read -r line <"$path" || return 1
  [[ "$line" == "vmid=${vmid}" ]]
}

check_production_reservations() {
  local now path pending_vmid created status
  now="$(date +%s)"
  [[ -d "$RESERVATION_DIR" ]] || return 0
  shopt -s nullglob
  for path in "$RESERVATION_DIR"/*.reservation; do
    validate_reservation_file "$path" ||
      die "invalid production-start reservation: $path"
    pending_vmid="${path##*/}"
    pending_vmid="${pending_vmid%.reservation}"
    created="$(stat -c %Y "$path")" ||
      die "could not inspect production-start reservation $path"
    status="$(vm_status "$pending_vmid" 2>/dev/null || true)"
    if [[ "$status" == "status: running" ]] ||
      ((now - created < RESERVATION_TTL_SECONDS)); then
      die "refusing staging start; production VM $pending_vmid is running or reserved"
    fi
    log "removing stale reservation for stopped production VM $pending_vmid"
    rm -f -- "$path"
  done
  shopt -u nullglob
}

discover_staging_candidates() {
  local vmid config name tags
  local vmids_text
  vmids_text="$(list_local_vmids)" ||
    die "could not list local QEMU guests on $LOCAL_NODE"
  STAGING_CANDIDATES=()
  while IFS= read -r vmid; do
    [[ -n "$vmid" && "$vmid" != "$VMID" ]] || continue
    config="${WORK_DIR}/candidate-${vmid}.conf"
    qm_config_to "$vmid" "$config" ||
      die "could not inspect local QEMU VM $vmid"
    name="$(config_value "$config" name)"
    tags="$(config_value "$config" tags)"
    is_staging_name "$name" || continue
    has_tag "$tags" "$STAGING_VM_TAG" || continue
    has_tag "$tags" "$EVICTABLE_VM_TAG" || continue
    STAGING_CANDIDATES+=("$vmid")
  done <<<"$vmids_text"
}

refresh_registry_resources() {
  [[ -f "${CLUSTER_STATE_DIR}/policy.json" ]] ||
    die "staging-shaped guests exist but the app-ha registry is not initialized"
  require_file "$REGISTRY"
  registry list --record-type resources >"${WORK_DIR}/resources.json" ||
    die "could not validate the app-ha registry"
}

validate_registry_candidate() {
  local vmid="$1" config_file="$2" metadata_file="$3"
  python3 - "$vmid" "$LOCAL_NODE" "$config_file" \
    "${WORK_DIR}/resources.json" "$PRODUCTION_VM_TAG" "$STAGING_VM_TAG" \
    "$EVICTABLE_VM_TAG" "$PURPOSE_TAG_PREFIX" >"$metadata_file" <<'PY'
import json
import re
import sys

vmid = int(sys.argv[1])
node = sys.argv[2]
config = {}
for line in open(sys.argv[3], encoding="utf-8"):
    if ": " in line:
        key, value = line.rstrip("\n").split(": ", 1)
        config[key] = value
resources = json.load(open(sys.argv[4], encoding="utf-8"))
production_tag, staging_tag, evictable_tag, purpose_prefix = sys.argv[5:9]

name = config.get("name", "")
if not re.fullmatch(r"stage[1-9][0-9]*prod[1-9][0-9]*", name):
    raise SystemExit("live staging name is not anchored stageNprodN")
if config.get("template", "0") == "1":
    raise SystemExit("live staging target is a template")
if config.get("onboot", "0") != "0":
    raise SystemExit("live staging target has onboot enabled")

matches = [
    row for row in resources
    if row["name"] == name or row["vmid"] == vmid
]
if len(matches) != 1:
    raise SystemExit("registry name/VMID identity is absent or ambiguous")
stage = matches[0]
if stage["name"] != name or stage["vmid"] != vmid or stage["kind"] != "staging":
    raise SystemExit("registry name/VMID does not match live staging")
tags = {tag for tag in re.split(r"[;,]", config.get("tags", "")) if tag}
expected_tags = {
    staging_tag,
    evictable_tag,
    f"{purpose_prefix}{stage['purpose']['slug']}",
}
if tags != expected_tags or production_tag in tags:
    raise SystemExit("live staging tags do not exactly match its registry role")
if stage["owner_node"] != node or stage["placement"] != [node]:
    raise SystemExit("registry staging owner/placement is not the local node")
if stage["proxmox"]["ha_nodes"]:
    raise SystemExit("registry incorrectly marks staging as HA-managed")
if stage["state"] not in {
    "cloning", "patching", "active", "ready", "stopped", "stopping",
    "cleanup_pending", "failed"
}:
    raise SystemExit("registry staging state is not safely evictable")

volume = stage["proxmox"]["volume_id"]
if not isinstance(volume, str):
    raise SystemExit("registry staging volume identity is absent")
if not re.fullmatch(
    rf"[A-Za-z0-9][A-Za-z0-9_.+-]{{0,63}}:vm-{vmid}-disk-0",
    volume,
):
    raise SystemExit("registry staging volume does not belong to VMID")
disk_keys = {
    key
    for key in config
    if re.fullmatch(
        r"(?:(?:scsi|sata|virtio|ide|unused|efidisk|tpmstate)[0-9]+)",
        key,
    )
}
if disk_keys != {"scsi0", "efidisk0"}:
    raise SystemExit("live staging disk keys are not exactly scsi0 and efidisk0")
if config["scsi0"].split(",", 1)[0] != volume:
    raise SystemExit("live staging root disk differs from registered volume")
efi_volume = config["efidisk0"].split(",", 1)[0]
storage = volume.split(":", 1)[0]
if not re.fullmatch(
    rf"{re.escape(storage)}:vm-{vmid}-disk-[1-9][0-9]*",
    efi_volume,
) or efi_volume == volume:
    raise SystemExit("live staging EFI vars disk is not fresh VM-owned storage")

snapshot = stage["proxmox"]["snapshot"]
if not isinstance(snapshot, dict):
    raise SystemExit("registry staging snapshot dependency is absent")
if snapshot.get("verified") is not True:
    raise SystemExit("registry staging snapshot dependency is not verified")
sources = [
    row
    for row in resources
    if row["name"] == stage["source"] and row["kind"] == "production"
]
if len(sources) != 1:
    raise SystemExit("registered production source is absent or ambiguous")
source = sources[0]
snapshot_name = snapshot["name"]
if (
    snapshot["source_resource"] != source["name"]
    or snapshot["source_volume_id"] != source["proxmox"]["volume_id"]
    or snapshot["dependent_resources"] != [stage["name"]]
    or snapshot["refcount"] != 1
    or not re.fullmatch(
        rf"stg-base-{re.escape(stage['name'])}-[A-Za-z0-9._-]+",
        snapshot_name,
    )
    or snapshot_name.startswith("__replicate_")
    or node not in snapshot["guids"]
):
    raise SystemExit("registered staging snapshot dependency is inconsistent")
if (
    set(snapshot["guids"]) != set(source["placement"])
    or set(snapshot["volume_guids"]) != set(snapshot["source_volume_ids"])
    or any(
        set(node_guids) != set(source["placement"])
        for node_guids in snapshot["volume_guids"].values()
    )
):
    raise SystemExit("snapshot cleanup nodes differ from production placement")

json.dump(
    {
        "resource_id": stage["id"],
        "revision": stage["revision"],
        "state": stage["state"],
        "routes_enabled": stage["routes_enabled"],
        "name": stage["name"],
        "vmid": stage["vmid"],
        "volume": volume,
        "snapshot": snapshot_name,
        "snapshot_guid": snapshot["guids"][node],
        "snapshot_nodes": sorted(snapshot["guids"]),
        "source_name": source["name"],
        "source_vmid": source["vmid"],
        "source_volume": source["proxmox"]["volume_id"],
    },
    sys.stdout,
    sort_keys=True,
)
PY
}

validate_storage_dependency() {
  local metadata_file="$1" storage_file="$2"
  local -a fields=() clone_fields=() source_fields=()
  mapfile -d '' -t fields < <(
    python3 - "$metadata_file" <<'PY'
import json
import sys
row = json.load(open(sys.argv[1], encoding="utf-8"))
for key in ("name", "volume", "snapshot", "snapshot_guid", "source_volume"):
    sys.stdout.buffer.write(str(row[key]).encode() + b"\0")
PY
  )
  ((${#fields[@]} == 5)) || return 1
  local name="${fields[0]}" volume="${fields[1]}" snapshot="${fields[2]}"
  local expected_guid="${fields[3]}" source_volume="${fields[4]}"
  local clone_dataset clone_path source_dataset source_path origin guid clones

  mapfile -t clone_fields < <(safe_zvol_dataset "$volume") || return 1
  mapfile -t source_fields < <(safe_zvol_dataset "$source_volume") || return 1
  ((${#clone_fields[@]} == 2 && ${#source_fields[@]} == 2)) || return 1
  clone_dataset="${clone_fields[0]}"
  clone_path="${clone_fields[1]}"
  source_dataset="${source_fields[0]}"
  source_path="${source_fields[1]}"
  [[ -n "$source_path" && "$snapshot" =~ ^stg-base-"$name"-[A-Za-z0-9._-]+$ &&
    "$snapshot" != __replicate_* ]] || return 1
  zfs_object_exists "$clone_dataset" || return 1
  [[ "$(zfs list -Hp -o type "$clone_dataset")" == volume ]] || return 1
  origin="$(zfs get -Hp -o value origin "$clone_dataset")" || return 1
  [[ "$origin" == "${source_dataset}@${snapshot}" ]] || return 1
  guid="$(zfs get -Hp -o value guid "${source_dataset}@${snapshot}")" ||
    return 1
  [[ "$guid" == "$expected_guid" ]] || return 1
  clones="$(zfs get -Hp -o value clones "${source_dataset}@${snapshot}")" ||
    return 1
  [[ ",${clones// /,}," == *",${clone_dataset},"* ]] || return 1
  printf '%s\n%s\n%s\n' "$clone_dataset" "$clone_path" "$source_dataset" \
    >"$storage_file"
}

validate_staging_candidate() {
  local vmid="$1" config metadata storage ha_status
  config="${WORK_DIR}/candidate-${vmid}.conf"
  metadata="${WORK_DIR}/candidate-${vmid}.json"
  storage="${WORK_DIR}/candidate-${vmid}.storage"
  vm_is_local_qemu "$vmid" || die "staging VM $vmid is no longer one local QEMU guest"
  ha_status=0
  vm_is_ha "$vmid" || ha_status=$?
  case "$ha_status" in
    0) die "staging VM $vmid is HA-managed; refusing automatic eviction" ;;
    1) ;;
    *) die "could not verify HA status for staging VM $vmid" ;;
  esac
  validate_registry_candidate "$vmid" "$config" "$metadata" ||
    die "staging VM $vmid does not exactly match its registry identity"
  validate_storage_dependency "$metadata" "$storage" ||
    die "staging VM $vmid does not match its registered clone/snapshot dependency"
}

refresh_candidate_validation() {
  local vmid="$1" config
  config="${WORK_DIR}/candidate-${vmid}.conf"
  qm_config_to "$vmid" "$config" ||
    die "staging VM $vmid disappeared while validating eviction"
  refresh_registry_resources
  validate_staging_candidate "$vmid"
}

disable_staging_route() {
  local vmid="$1" metadata
  metadata="${WORK_DIR}/candidate-${vmid}.json"
  local -a fields=()
  mapfile -d '' -t fields < <(
    python3 - "$metadata" <<'PY'
import json
import sys
row = json.load(open(sys.argv[1], encoding="utf-8"))
for value in (row["name"], row["revision"], row["state"]):
    sys.stdout.buffer.write(str(value).encode() + b"\0")
PY
  )
  ((${#fields[@]} == 3)) || die "could not parse registry route identity"
  local name="${fields[0]}" revision="${fields[1]}" state="${fields[2]}"
  local -a args=(
    update "$name" --expected-revision "$revision" --routes-disabled
  )
  [[ "$state" != active ]] || args+=(--state stopping)
  registry "${args[@]}" >/dev/null ||
    die "could not disable the registry route for $name"
}

verify_vm_config_absent() {
  local vmid="$1" status
  if qm status "$vmid" >/dev/null 2>&1; then
    return 1
  fi
  status=0
  vm_is_local_qemu "$vmid" || status=$?
  ((status == 1))
}

queue_cleanup_plan() {
  local vmid="$1" metadata configured_text
  metadata="${WORK_DIR}/candidate-${vmid}.json"
  local -a fields=() nodes=() configured_nodes=()
  mapfile -d '' -t fields < <(
    python3 - "$metadata" <<'PY'
import json
import sys
row = json.load(open(sys.argv[1], encoding="utf-8"))
for value in (
    row["name"],
    row["resource_id"],
    row["volume"],
    row["source_vmid"],
    row["snapshot"],
    ",".join(row["snapshot_nodes"]),
):
    sys.stdout.buffer.write(str(value).encode() + b"\0")
PY
  )
  ((${#fields[@]} == 6)) || die "could not parse deferred cleanup identity"
  local name="${fields[0]}" resource_id="${fields[1]}"
  local volume="${fields[2]}" source_vmid="${fields[3]}" snapshot="${fields[4]}"
  IFS=',' read -r -a nodes <<<"${fields[5]}"
  configured_text="$(list_configured_nodes)" ||
    die "could not validate configured nodes before queuing cleanup"
  mapfile -t configured_nodes <<<"$configured_text"
  ((${#configured_nodes[@]} > 0)) ||
    die "configured node inventory was unexpectedly empty"
  [[ "$snapshot" != __replicate_* ]] ||
    die "refusing to queue Proxmox reserved replication snapshot"
  local node result
  local ids_file="${WORK_DIR}/candidate-${vmid}.local-cleanup-ids"
  : >"$ids_file"

  result="$(
    registry defer-cleanup \
      --resource "$name" \
      --node "$LOCAL_NODE" \
      --action destroy-vm \
      --target "vm:${vmid}" \
      --reason "production start must remove the local staging VM config"
  )" || die "could not queue fallback VM cleanup for $name"
  python3 -c \
    'import json,sys; print(json.loads(sys.argv[1])["cleanup"]["id"])' \
    "$result" >>"$ids_file" ||
    die "could not parse fallback VM cleanup identity"

  result="$(
    registry defer-cleanup \
      --resource "$name" \
      --node "$LOCAL_NODE" \
      --action destroy-volume \
      --target "$volume" \
      --reason "production start must remove the local staging linked clone"
  )" || die "could not queue fallback volume cleanup for $name"
  python3 -c \
    'import json,sys; print(json.loads(sys.argv[1])["cleanup"]["id"])' \
    "$result" >>"$ids_file" ||
    die "could not parse fallback volume cleanup identity"

  for node in "${nodes[@]}"; do
    array_contains "$node" "${configured_nodes[@]}" ||
      die "snapshot cleanup node $node is absent from pvesh /nodes"
    registry defer-cleanup \
      --resource "$name" \
      --node "$node" \
      --action delete-snapshot \
      --target "vm-${source_vmid}@${snapshot}" \
      --reason "production start evicted the dependent staging clone" \
      >/dev/null ||
      die "could not queue snapshot cleanup for $name on $node"
  done
  for node in "${configured_nodes[@]}"; do
    registry defer-cleanup \
      --resource "$name" \
      --node "$node" \
      --action remove-route \
      --target "$name" \
      --reason "production start disabled the evicted staging route" \
      >/dev/null ||
      die "could not queue HAProxy route cleanup for $name on $node"
  done

  local current_file="${WORK_DIR}/candidate-${vmid}.current.json"
  registry get "$name" >"$current_file" ||
    die "could not refresh $name before committing cleanup state"
  local -a current=()
  mapfile -d '' -t current < <(
    python3 - "$current_file" "$resource_id" <<'PY'
import json
import sys
row = json.load(open(sys.argv[1], encoding="utf-8"))
if row["id"] != sys.argv[2]:
    raise SystemExit("resource identity changed")
for value in (row["revision"], row["state"]):
    sys.stdout.buffer.write(str(value).encode() + b"\0")
PY
  )
  ((${#current[@]} == 2)) ||
    die "registry identity changed before cleanup commit"
  local -a update_args=(
    update "$name"
    --expected-revision "${current[0]}"
    --routes-disabled
  )
  [[ "${current[1]}" == cleanup_pending ]] ||
    update_args+=(--state cleanup_pending)
  registry "${update_args[@]}" >/dev/null ||
    die "could not commit cleanup_pending for $name"
  log "committed durable cleanup plan for $name"
}

complete_local_cleanup_records() {
  local vmid="$1" ids_file
  ids_file="${WORK_DIR}/candidate-${vmid}.local-cleanup-ids"
  local -a ids=()
  mapfile -t ids <"$ids_file"
  ((${#ids[@]} == 2)) || return 1
  python3 - "${ids[@]}" <<'PY' |
import json
import sys
json.dump({"schema_version": 1, "cleanup_completed": sys.argv[1:]}, sys.stdout)
PY
    registry reconcile --observed - --apply >/dev/null
}

clear_destroyed_local_identity() {
  local vmid="$1" metadata current_file
  metadata="${WORK_DIR}/candidate-${vmid}.json"
  current_file="${WORK_DIR}/candidate-${vmid}.destroyed.json"
  local name resource_id
  read -r name resource_id < <(
    python3 - "$metadata" <<'PY'
import json
import sys
row = json.load(open(sys.argv[1], encoding="utf-8"))
print(row["name"], row["resource_id"])
PY
  ) || return 1
  registry get "$name" >"$current_file" || return 1
  local revision state
  read -r revision state < <(
    python3 - "$current_file" "$resource_id" <<'PY'
import json
import sys
row = json.load(open(sys.argv[1], encoding="utf-8"))
if row["id"] != sys.argv[2]:
    raise SystemExit("resource identity changed")
print(row["revision"], row["state"])
PY
  ) || return 1
  [[ "$state" == cleanup_pending ]] || return 1
  registry update "$name" \
    --expected-revision "$revision" \
    --routes-disabled \
    --clear-owner-node \
    --clear-volume >/dev/null
}

validate_production_authority() {
  local name="$1" config_file="$2" ha_status
  vm_is_local_qemu "$VMID" ||
    die "production VM $VMID is not exactly one local QEMU guest"
  ha_status=0
  vm_is_ha "$VMID" || ha_status=$?
  case "$ha_status" in
    0) ;;
    1) die "production VM $VMID is not HA-managed" ;;
    *) die "could not verify HA membership for production VM $VMID" ;;
  esac
  refresh_registry_resources
  python3 - "$VMID" "$name" "$LOCAL_NODE" "$config_file" \
    "${WORK_DIR}/resources.json" "$PRODUCTION_VM_TAG" "$STAGING_VM_TAG" \
    "$EVICTABLE_VM_TAG" "$PURPOSE_TAG_PREFIX" "$PROD_VM_BRIDGE" <<'PY'
import json
import re
import sys

vmid = int(sys.argv[1])
name = sys.argv[2]
node = sys.argv[3]
config = {}
for line in open(sys.argv[4], encoding="utf-8"):
    if ": " in line:
        key, value = line.rstrip("\n").split(": ", 1)
        config[key] = value
resources = json.load(open(sys.argv[5], encoding="utf-8"))
production_tag, staging_tag, evictable_tag, purpose_prefix, bridge = sys.argv[6:11]
matches = [
    row for row in resources if row["name"] == name or row["vmid"] == vmid
]
if len(matches) != 1:
    raise SystemExit("production registry identity is absent or ambiguous")
prod = matches[0]
if prod["kind"] != "production" or prod["name"] != name or prod["vmid"] != vmid:
    raise SystemExit("production registry name/VMID does not match the live VM")
if node not in prod["placement"] or node not in prod["proxmox"]["ha_nodes"]:
    raise SystemExit("production start node is outside registered placement/HA")
if set(prod["proxmox"]["ha_nodes"]) != set(prod["placement"]):
    raise SystemExit("production registry HA membership differs from placement")
if prod["state"] not in {"stopped", "ready", "active"}:
    raise SystemExit("production registry state does not grant eviction authority")
tags = {tag for tag in re.split(r"[;,]", config.get("tags", "")) if tag}
expected_tags = {production_tag, f"{purpose_prefix}{prod['purpose']['slug']}"}
if tags != expected_tags or staging_tag in tags or evictable_tag in tags:
    raise SystemExit("production live tags do not exactly match registry authority")

network_keys = sorted(key for key in config if re.fullmatch(r"net[0-9]+", key))
if network_keys != ["net0"]:
    raise SystemExit("production VM must have exactly one NIC")
network = {
    part.split("=", 1)[0]: part.split("=", 1)[1]
    for part in config["net0"].split(",")
    if "=" in part
}
if (
    network.get("virtio", "").lower() != prod["mac"].lower()
    or network.get("bridge") != bridge
):
    raise SystemExit("production VM MAC/bridge differs from registry policy")

data_disks = []
for key, value in config.items():
    if re.fullmatch(r"(?:scsi|sata|virtio|ide)[0-9]+", key):
        if "media=cdrom" not in value:
            data_disks.append((key, value.split(",", 1)[0]))
if data_disks != [("scsi0", prod["proxmox"]["volume_id"])]:
    raise SystemExit("production root-volume identity differs from registry")
PY
}

evict_staging_candidate() {
  local vmid="$1" metadata storage
  metadata="${WORK_DIR}/candidate-${vmid}.json"
  storage="${WORK_DIR}/candidate-${vmid}.storage"
  local ha_status
  local -a fields=() storage_fields=()
  refresh_candidate_validation "$vmid"
  local name
  name="$(python3 -c \
    'import json,sys; print(json.load(open(sys.argv[1], encoding="utf-8"))["name"])' \
    "$metadata")" || die "could not parse staging name"
  if ! vm_is_stopped "$vmid"; then
    log "force-stopping disposable staging VM $vmid ($name), ignoring its staging lock by policy"
    qm stop "$vmid" --skiplock true --timeout 30 ||
      die "force-stop failed for staging VM $vmid"
  fi
  vm_is_stopped "$vmid" ||
    die "staging VM $vmid remained running after force-stop"

  # Revalidate the complete identity after stop and immediately before the
  # irreversible config/storage operations.
  refresh_candidate_validation "$vmid"
  ha_status=0
  vm_is_ha "$vmid" || ha_status=$?
  [[ "$ha_status" == 1 ]] ||
    die "staging VM $vmid became HA-managed or HA state is unavailable"
  disable_staging_route "$vmid"

  # Route state changed in pmxcfs; validate live QEMU, HA, registry identity,
  # volume, origin, and snapshot GUID once more before destruction.
  refresh_candidate_validation "$vmid"
  fields=()
  storage_fields=()
  mapfile -d '' -t fields < <(
    python3 - "$metadata" <<'PY'
import json
import sys
row = json.load(open(sys.argv[1], encoding="utf-8"))
for value in (row["name"], row["volume"], row["snapshot"], row["snapshot_guid"]):
    sys.stdout.buffer.write(str(value).encode() + b"\0")
PY
  )
  mapfile -t storage_fields <"$storage"
  ((${#fields[@]} == 4 && ${#storage_fields[@]} == 3)) ||
    die "could not parse validated staging storage identity"
  name="${fields[0]}"
  local snapshot="${fields[2]}" expected_guid="${fields[3]}"
  local clone_dataset="${storage_fields[0]}" clone_path="${storage_fields[1]}"
  local source_dataset="${storage_fields[2]}" origin guid

  clone_is_quiescent "$clone_path" "$clone_dataset" ||
    die "staging clone $clone_dataset is mounted or open"
  queue_cleanup_plan "$vmid"
  origin="$(zfs get -Hp -o value origin "$clone_dataset")" ||
    die "could not re-read staging clone origin after cleanup-plan commit"
  [[ "$origin" == "${source_dataset}@${snapshot}" ]] ||
    die "staging clone origin changed after cleanup-plan commit"
  guid="$(zfs get -Hp -o value guid "${source_dataset}@${snapshot}")" ||
    die "could not re-read snapshot GUID after cleanup-plan commit"
  [[ "$guid" == "$expected_guid" ]] ||
    die "staging snapshot GUID changed after cleanup-plan commit"
  clone_is_quiescent "$clone_path" "$clone_dataset" ||
    die "staging clone became mounted or open after cleanup-plan commit"
  log "destroying validated staging VM config $vmid ($name)"
  qm destroy "$vmid" --purge 1 --destroy-unreferenced-disks 0 \
    --skiplock true ||
    die "could not destroy staging VM config $vmid"
  verify_vm_config_absent "$vmid" ||
    die "staging VM config $vmid remains after qm destroy"

  if zfs_object_exists "$clone_dataset"; then
    [[ "$(zfs list -Hp -o type "$clone_dataset")" == volume ]] ||
      die "staging clone changed type before explicit destruction"
    origin="$(zfs get -Hp -o value origin "$clone_dataset")" ||
      die "could not re-read staging clone origin"
    [[ "$origin" == "${source_dataset}@${snapshot}" ]] ||
      die "staging clone origin changed before explicit destruction"
    guid="$(zfs get -Hp -o value guid "${source_dataset}@${snapshot}")" ||
      die "could not re-read staging snapshot GUID"
    [[ "$guid" == "$expected_guid" ]] ||
      die "staging snapshot GUID changed before clone destruction"
    clone_is_quiescent "$clone_path" "$clone_dataset" ||
      die "staging clone became mounted or open"
    volume_unreferenced_by_any_vm "${fields[1]}" ||
      die "another VM still references the staging clone"
    log "destroying validated linked-clone zvol $clone_dataset"
    zfs destroy "$clone_dataset" ||
      die "could not destroy linked-clone zvol $clone_dataset"
  fi
  ! zfs_object_exists "$clone_dataset" ||
    die "linked-clone zvol $clone_dataset remains after destruction"
  if [[ "$TEST_MODE" != 1 ]]; then
    [[ ! -e "$clone_path" ]] ||
      die "linked-clone block path remains after destruction: $clone_path"
  fi

  if complete_local_cleanup_records "$vmid"; then
    clear_destroyed_local_identity "$vmid" ||
      log "local cleanup is complete but registry metadata clearing will retry asynchronously"
  else
    log "local cleanup is complete but completion recording will retry asynchronously"
  fi
  log "evicted $name; remote snapshot and HAProxy cleanup is queued"
}

production_pre_start() {
  local name="$1" self_config="$2"
  validate_production_authority "$name" "$self_config"
  write_reservation "$name"
  log "production VM $VMID ($name) reserved start on $LOCAL_NODE"
  discover_staging_candidates
  if ((${#STAGING_CANDIDATES[@]} > 0)); then
    refresh_registry_resources
    local candidate
    # Validate all candidates before the first destructive operation.
    for candidate in "${STAGING_CANDIDATES[@]}"; do
      validate_staging_candidate "$candidate"
    done
    for candidate in "${STAGING_CANDIDATES[@]}"; do
      evict_staging_candidate "$candidate"
    done
  else
    log "no registered disposable staging guests require eviction"
  fi
  PRESERVE_RESERVATION=true
  log "production pre-start cleanup completed; reservation remains through post-start"
}

staging_pre_start() {
  local name="$1" candidate config candidate_name candidate_tags status
  check_production_reservations

  local vmids_text
  vmids_text="$(list_local_vmids)" ||
    die "could not list local QEMU guests before staging start"
  while IFS= read -r candidate; do
    [[ -n "$candidate" && "$candidate" != "$VMID" ]] || continue
    config="${WORK_DIR}/production-${candidate}.conf"
    qm_config_to "$candidate" "$config" ||
      die "could not inspect local QEMU VM $candidate"
    candidate_name="$(config_value "$config" name)"
    candidate_tags="$(config_value "$config" tags)"
    is_production_name "$candidate_name" || continue
    has_tag "$candidate_tags" "$PRODUCTION_VM_TAG" || continue
    status="$(vm_status "$candidate" 2>/dev/null || true)"
    [[ "$status" != "status: running" ]] ||
      die "refusing to start $name beside production $candidate_name"
  done <<<"$vmids_text"

  refresh_registry_resources
  qm_config_to "$VMID" "${WORK_DIR}/candidate-${VMID}.conf" ||
    die "could not re-read staging QEMU configuration"
  validate_staging_candidate "$VMID"
  local stage_state
  stage_state="$(python3 -c \
    'import json,sys; print(json.load(open(sys.argv[1], encoding="utf-8"))["state"])' \
    "${WORK_DIR}/candidate-${VMID}.json")" ||
    die "could not parse staging registry state"
  [[ "$stage_state" == ready || "$stage_state" == stopped ]] ||
    die "refusing to start staging in registry state $stage_state"
  log "staging VM $VMID ($name) may start; no production is running or reserved on the host for this staging guest"
}

classify_role() {
  local config_file="$1" name tags
  name="$(config_value "$config_file" name)"
  tags="$(config_value "$config_file" tags)"
  if is_production_name "$name" &&
    has_tag "$tags" "$PRODUCTION_VM_TAG" &&
    ! has_tag "$tags" "$STAGING_VM_TAG" &&
    ! has_tag "$tags" "$EVICTABLE_VM_TAG"; then
    printf 'production\n%s\n' "$name"
    return
  fi
  if is_staging_name "$name" &&
    has_tag "$tags" "$STAGING_VM_TAG" &&
    has_tag "$tags" "$EVICTABLE_VM_TAG" &&
    ! has_tag "$tags" "$PRODUCTION_VM_TAG"; then
    printf 'staging\n%s\n' "$name"
    return
  fi
  return 1
}

trigger_deferred_cleanup() {
  systemctl start --no-block "$DEFERRED_CLEANUP_SERVICE" >/dev/null 2>&1 ||
    log "deferred-cleanup service could not be queued; the timer will retry"
}

main() {
  [[ "$VMID" =~ ^[1-9][0-9]*$ ]] || die "invalid VMID: $VMID"
  case "$PHASE" in
    pre-start | post-start | post-stop | pre-stop | pre-restart | post-restart) ;;
    *) return 0 ;;
  esac

  # Stop/restart notifications are deliberately lock-free no-ops. Cleanup
  # workers hold the lifecycle lock while force-stopping staging guests; a
  # pre-stop hook that tried to reacquire that lock would deadlock the stop.
  case "$PHASE" in
    post-start)
      rm -f -- "$RESERVATION_FILE"
      trigger_deferred_cleanup
      return 0
      ;;
    post-stop)
      rm -f -- "$RESERVATION_FILE"
      return 0
      ;;
    pre-stop | pre-restart | post-restart)
      return 0
      ;;
  esac

  require_file "$CONFIG_LIB"
  # shellcheck source=../lib/config.sh
  source "$CONFIG_LIB"
  load_proxmox_config --no-secrets ||
    die "could not load installed cluster.conf without secrets"
  require_vars MAX_MOX_HOSTS PRODUCTION_VM_TAG STAGING_VM_TAG \
    EVICTABLE_VM_TAG PURPOSE_TAG_PREFIX CLUSTER_STATE_DIR PROD_VM_BRIDGE
  [[ "$LOCAL_NODE" =~ ^mox([1-9]|10)$ ]] ||
    die "local hostname must be mox1 through mox10"
  local role_tag
  for role_tag in \
    "$PRODUCTION_VM_TAG" "$STAGING_VM_TAG" "$EVICTABLE_VM_TAG"; do
    [[ "$role_tag" =~ ^[A-Za-z0-9][A-Za-z0-9_.:+/-]{0,63}$ ]] ||
      die "configured guest role tag is unsafe"
  done
  [[ "$PRODUCTION_VM_TAG" != "$STAGING_VM_TAG" &&
    "$PRODUCTION_VM_TAG" != "$EVICTABLE_VM_TAG" &&
    "$STAGING_VM_TAG" != "$EVICTABLE_VM_TAG" ]] ||
    die "production, staging, and evictable tags must be distinct"

  install -d -m 0755 "$LOCK_DIR"
  exec 9>"${LOCK_DIR}/app-ha-guest-role-hook.lock"
  flock -w 180 9 || die "timed out waiting for the guest lifecycle lock"

  require_commands findmnt flock fuser lsblk pvesh pvesm python3 qm zfs
  WORK_DIR="$(mktemp -d "${WORK_ROOT}/app-ha-guest-hook.XXXXXX")"
  trap cleanup EXIT
  local self_config="${WORK_DIR}/self.conf" role_data role name
  qm_config_to "$VMID" "$self_config" ||
    die "could not read QEMU configuration for VM $VMID"
  role_data="$(classify_role "$self_config")" ||
    die "hook is attached to VM $VMID without one unambiguous configured role"
  role="${role_data%%$'\n'*}"
  name="${role_data#*$'\n'}"
  case "$role" in
    production) production_pre_start "$name" "$self_config" ;;
    staging) staging_pre_start "$name" ;;
    *) die "internal role-classification failure" ;;
  esac
}

if [[ "${BASH_SOURCE[0]}" == "$0" ]]; then
  main "$@"
fi
