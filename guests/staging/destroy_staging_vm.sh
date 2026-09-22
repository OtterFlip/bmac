#!/usr/bin/env bash

# Copyright (c) 2026 BEENTHERE VENTURES, INC.
# SPDX-License-Identifier: GPL-3.0-only

# Permanently remove one registry-backed staging VM, its linked clone, its
# source-owned snapshot copies, ingress route, and registry allocation.

set -Eeuo pipefail
set +x
umask 077

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
REPO_ROOT="$(cd -- "${SCRIPT_DIR}/../.." && pwd -P)"
CONFIG_LIB="${REPO_ROOT}/lib/config.sh"
REMOTE_ROOT="/usr/local/lib/app-ha-proxmox"
REMOTE_REGISTRY="${REMOTE_ROOT}/lib/cluster_registry.py"
REMOTE_HAPROXY_SYNC="${REMOTE_ROOT}/lib/sync_haproxy_routes.sh"

DRY_RUN=false
RESOURCE_NAME=""
COORDINATOR=""
RUN_DIR=""
RESOURCE_ID=""
RESOURCE_REVISION=""
RESOURCE_STATE=""
VMID=""
STAGING_NODE=""
STAGING_VOLUME=""
STAGING_DATASET=""
SOURCE_NAME=""
SOURCE_VMID=""
SOURCE_OWNER=""
SOURCE_PURPOSE=""
SNAPSHOT_NAME=""
ROUTES_ENABLED=0
LIVE_PRESENT=0
SSH_ALIAS=""
declare -a SOURCE_PLACEMENT=()
declare -a SOURCE_VOLUMES=()
declare -a CLUSTER_NODES=()

die() {
  printf '\nERROR: %s\n' "$*" >&2
  exit 1
}

log() {
  printf '\n==> %s\n' "$*"
}

info() {
  printf '    %s\n' "$*"
}

usage() {
  cat <<'EOF'
Usage: destroy_staging_vm.sh [--dry-run] [stageNprodN]

With no resource argument, lists registered staging guests and prompts for one.
Permanently removes the selected guest's route, QEMU VM and VM-owned disks,
linked clone, and the one exact source-owned snapshot created for that staging
guest. It then verifies that replication removed copies of that same snapshot
from every production placement node. No other production snapshot is touched,
and production itself is never stopped or destroyed.

Options:
  --dry-run  Validate and print the complete destruction plan without changes.
  -h, --help
EOF
}

parse_args() {
  while (($#)); do
    case "$1" in
      --dry-run)
        DRY_RUN=true
        ;;
      -h | --help)
        usage
        exit 0
        ;;
      *)
        [[ -z "$RESOURCE_NAME" ]] ||
          die "Only one staging resource may be selected"
        RESOURCE_NAME="$1"
        ;;
    esac
    shift
  done
}

node_exec() {
  local node="$1"
  shift
  [[ "$node" =~ ^mox([1-9]|10)$ ]] || return 2
  (($# > 0)) || return 2
  local payload="set -Eeuo pipefail"$'\n'"exec" argument quoted
  for argument in "$@"; do
    printf -v quoted '%q' "$argument"
    payload+=" ${quoted}"
  done
  payload+=$'\n'
  if [[ "$node" == "$COORDINATOR" ]]; then
    printf '%s' "$payload" | mox_ssh "$COORDINATOR" bash -s
  else
    printf '%s' "$payload" |
      mox_ssh "$COORDINATOR" ssh \
        -o BatchMode=yes \
        -o ClearAllForwardings=yes \
        -o "ConnectTimeout=${MOX_SSH_CONNECT_TIMEOUT:-8}" \
        -o StrictHostKeyChecking=yes \
        -o CheckHostIP=no \
        -o "HostKeyAlias=${node}" \
        -o "UserKnownHostsFile=/etc/pve/nodes/${node}/ssh_known_hosts" \
        -o GlobalKnownHostsFile=none \
        "root@${node}.${PROXMOX_INTERNAL_DOMAIN}" bash -s
  fi
}

registry_cmd() {
  mox_ssh "$COORDINATOR" "$REMOTE_REGISTRY" \
    --state-dir "$CLUSTER_STATE_DIR" "$@" </dev/null
}

pvesh_get() {
  mox_ssh "$COORDINATOR" pvesh get "$@" --output-format json </dev/null
}

write_json() {
  local path="$1" value="$2"
  printf '%s\n' "$value" >"$path"
  chmod 0600 "$path"
  python3 - "$path" <<'PY'
import json
import sys
json.load(open(sys.argv[1], encoding="utf-8"))
PY
}

cleanup() {
  local code=$?
  trap - EXIT
  set +e
  [[ -z "$RUN_DIR" ]] || rm -rf -- "$RUN_DIR"
  exit "$code"
}

select_resource() {
  local resources_json="$1" table names default
  table="$(
    python3 - "$resources_json" <<'PY'
import json
import sys
rows = [
    row for row in json.loads(sys.argv[1])
    if row.get("kind") == "staging"
]
rows.sort(key=lambda row: (row.get("source", ""), row.get("index", 0)))
if not rows:
    raise SystemExit("no registered staging guests exist")
print("  NAME                 SOURCE     NODE     STATE       DOMAIN")
for row in rows:
    placement = row.get("placement") or ["?"]
    print(
        f"  {row['name']:<20} {str(row.get('source', '?')):<10} "
        f"{str(placement[0]):<8} {str(row.get('state', '?')):<11} "
        f"{row.get('domains', {}).get('primary', '?')}"
    )
PY
  )" || die "No registered staging guest is available for destruction"
  printf '\nRegistered staging guests:\n%s\n' "$table"
  [[ -z "$RESOURCE_NAME" ]] || return 0
  names="$(
    python3 - "$resources_json" <<'PY'
import json
import sys
rows = [
    row for row in json.loads(sys.argv[1])
    if row.get("kind") == "staging"
]
rows.sort(key=lambda row: (row.get("source", ""), row.get("index", 0)))
print(*[row["name"] for row in rows], sep="\n")
PY
  )"
  default="$(printf '%s\n' "$names" | awk 'NF {print; exit}')"
  IFS= read -r -p "Staging guest to destroy [${default}]: " RESOURCE_NAME
  RESOURCE_NAME="${RESOURCE_NAME:-$default}"
}

volume_dataset_on_node() {
  local node="$1" volume="$2"
  # shellcheck disable=SC2016 # This script is evaluated by remote Bash.
  node_exec "$node" bash -c '
set -Eeuo pipefail
volume="$1"
path="$(pvesm path "$volume")"
[[ "$path" == /dev/zvol/* && -b "$path" ]]
dataset="${path#/dev/zvol/}"
[[ "$dataset" =~ ^[A-Za-z0-9._/+:-]+$ ]]
[[ "$(zfs list -Hp -o type "$dataset")" == volume ]]
printf "%s\n" "$dataset"
' bash "$volume"
}

parse_and_validate_plan() {
  local resources_json="$1" live_json="$2" nodes_json="$3"
  local ha_json="$4" replication_json="$5" cleanup_json="$6"
  local -a fields=()
  mapfile -d '' -t fields < <(
    python3 - "$resources_json" "$live_json" "$nodes_json" "$ha_json" \
      "$replication_json" "$cleanup_json" "$RESOURCE_NAME" <<'PY'
import json
import re
import sys

resources, live, nodes, ha, replication, cleanup = [
    json.loads(value) for value in sys.argv[1:7]
]
name = sys.argv[7]
matches = [row for row in resources if row.get("name") == name]
if len(matches) != 1 or matches[0].get("kind") != "staging":
    raise SystemExit("selection is not exactly one registered staging resource")
stage = matches[0]
if not re.fullmatch(r"stage[1-9][0-9]*prod[1-9][0-9]*", name):
    raise SystemExit("staging name does not match the current naming contract")
source_matches = [
    row for row in resources
    if row.get("name") == stage.get("source") and row.get("kind") == "production"
]
if len(source_matches) != 1:
    raise SystemExit("staging source production resource is missing or ambiguous")
source = source_matches[0]
placement = stage.get("placement")
if not isinstance(placement, list) or len(placement) != 1:
    raise SystemExit("staging placement is not exactly one node")
stage_node = placement[0]
if stage.get("owner_node") not in (None, stage_node):
    raise SystemExit("staging owner differs from fixed placement")
source_placement = source.get("placement")
if not isinstance(source_placement, list) or len(source_placement) < 2:
    raise SystemExit("source production placement is invalid")
online = {
    row.get("node", row.get("name"))
    for row in nodes
    if str(row.get("status", "")).lower() == "online"
    or row.get("online") in (1, True)
}
if set(source_placement) - online:
    raise SystemExit("every source production placement node must be online")

vmid = int(stage["vmid"])
live_matches = [
    row for row in live if row.get("vmid") in (vmid, str(vmid))
]
if len(live_matches) == 1:
    vm = live_matches[0]
    if (
        vm.get("name") != name
        or vm.get("type") != "qemu"
        or vm.get("node") != stage_node
    ):
        raise SystemExit("live staging VM identity or node differs from registry")
    live_present = True
elif len(live_matches) == 0 and stage.get("state") in {
    "stopping", "stopped", "failed", "cleanup_pending"
}:
    live_present = False
else:
    raise SystemExit("live staging VM identity is absent or ambiguous")

if any(row.get("sid") == f"vm:{vmid}" for row in ha):
    raise SystemExit("staging VM must not be a Proxmox HA resource")
snapshot = stage.get("proxmox", {}).get("snapshot")
if (
    not isinstance(snapshot, dict)
    or snapshot.get("verified") is not True
    or snapshot.get("source_resource") != source["name"]
    or snapshot.get("dependent_resources") != [name]
    or snapshot.get("refcount") != 1
):
    raise SystemExit("staging snapshot dependency metadata is incomplete")
source_volumes = snapshot.get("source_volume_ids")
if (
    not isinstance(source_volumes, list)
    or not source_volumes
    or len(source_volumes) != len(set(source_volumes))
):
    raise SystemExit("source snapshot volume inventory is invalid")
volume = stage.get("proxmox", {}).get("volume_id")
if not isinstance(volume, str) or not volume:
    raise SystemExit("staging clone volume is missing")
owned_cleanup = [
    row for row in cleanup if row.get("resource_id") == stage["id"]
]
if any(row.get("resource") != name for row in owned_cleanup):
    raise SystemExit("deferred cleanup resource identity is inconsistent")

source_vmid = int(source["vmid"])
source_live = [
    row for row in live if row.get("vmid") in (source_vmid, str(source_vmid))
]
if (
    len(source_live) != 1
    or source_live[0].get("name") != source["name"]
    or source_live[0].get("node") not in source_placement
    or source_live[0].get("status") != "running"
):
    raise SystemExit("source production VM must be uniquely running on placement")
source_owner = source_live[0]["node"]
jobs = [
    row for row in replication
    if row.get("guest") in (source_vmid, str(source_vmid))
]
targets = {row.get("target") for row in jobs}
if targets != set(source_placement) - {source_owner}:
    raise SystemExit("source replication targets differ from placement minus owner")
job_ids = [str(row.get("id", "")) for row in jobs]
if any(not re.fullmatch(r"[1-9][0-9]{2,8}-[0-9]+", value) for value in job_ids):
    raise SystemExit("source replication job identity is unsafe")

values = (
    stage["id"],
    stage["revision"],
    stage["state"],
    vmid,
    stage_node,
    volume,
    "1" if stage.get("routes_enabled") else "0",
    "1" if live_present else "0",
    source["name"],
    source_vmid,
    source_owner,
    source["purpose"]["slug"],
    snapshot["name"],
    ",".join(source_placement),
    ",".join(source_volumes),
    stage["domains"]["primary"],
    ",".join(sorted(online, key=lambda value: int(value[3:]))),
)
for value in values:
    sys.stdout.buffer.write(str(value).encode() + b"\0")
PY
  ) || die "Staging destruction preflight failed"
  ((${#fields[@]} == 17)) || die "Could not parse destruction plan"
  RESOURCE_ID="${fields[0]}"
  RESOURCE_REVISION="${fields[1]}"
  RESOURCE_STATE="${fields[2]}"
  VMID="${fields[3]}"
  STAGING_NODE="${fields[4]}"
  STAGING_VOLUME="${fields[5]}"
  ROUTES_ENABLED="${fields[6]}"
  LIVE_PRESENT="${fields[7]}"
  SOURCE_NAME="${fields[8]}"
  SOURCE_VMID="${fields[9]}"
  SOURCE_OWNER="${fields[10]}"
  SOURCE_PURPOSE="${fields[11]}"
  SNAPSHOT_NAME="${fields[12]}"
  IFS=',' read -r -a SOURCE_PLACEMENT <<<"${fields[13]}"
  IFS=',' read -r -a SOURCE_VOLUMES <<<"${fields[14]}"
  STAGING_DOMAIN="${fields[15]}"
  IFS=',' read -r -a CLUSTER_NODES <<<"${fields[16]}"
}

validate_live_vm() {
  local config
  config="$(node_exec "$STAGING_NODE" qm config "$VMID")" || return 1
  python3 - "$config" "$RESOURCE_NAME" "$VMID" "$STAGING_VM_TAG" \
    "$EVICTABLE_VM_TAG" "${PURPOSE_TAG_PREFIX}${SOURCE_PURPOSE}" \
    "$STAGING_VOLUME" "$STAGING_VM_STORAGE" "$GUEST_ROLE_HOOK_PATH" <<'PY'
import re
import sys
config = {}
for line in sys.argv[1].splitlines():
    if ": " in line:
        key, value = line.split(": ", 1)
        config[key] = value
name, raw_vmid, staging_tag, evictable_tag, purpose_tag, volume, storage, hook = sys.argv[2:]
vmid = int(raw_vmid)
tags = {value for value in re.split(r"[;,]", config.get("tags", "")) if value}
if config.get("name") != name:
    raise SystemExit("staging VM name differs from registry")
if tags != {staging_tag, evictable_tag, purpose_tag}:
    raise SystemExit("staging VM tags differ from exact role")
if config.get("onboot", "0") != "0":
    raise SystemExit("staging VM onboot is enabled")
if config.get("hookscript") != hook:
    raise SystemExit("staging lifecycle hook differs from contract")
if config.get("scsi0", "").split(",", 1)[0] != volume:
    raise SystemExit("staging root volume differs from registry")
efi = config.get("efidisk0", "").split(",", 1)[0]
if not re.fullmatch(rf"{re.escape(storage)}:vm-{vmid}-disk-[1-9][0-9]*", efi):
    raise SystemExit("staging EFI volume identity is invalid")
disks = {
    key for key in config
    if re.fullmatch(r"(?:scsi|sata|virtio|ide|unused|efidisk|tpmstate)[0-9]+", key)
}
if disks != {"scsi0", "efidisk0"}:
    raise SystemExit("staging VM has unexpected disks")
PY
}

validate_owned_volumes() {
  local inventory
  inventory="$(
    node_exec "$STAGING_NODE" pvesm list "$STAGING_VM_STORAGE" --vmid "$VMID"
  )" || return 1
  python3 - "$inventory" "$STAGING_VOLUME" "$STAGING_VM_STORAGE" "$VMID" <<'PY'
import re
import sys
lines = [line.split() for line in sys.argv[1].splitlines()[1:] if line.split()]
observed = {parts[0] for parts in lines}
root, storage, raw_vmid = sys.argv[2:]
vmid = int(raw_vmid)
if root not in observed or len(observed) != 2:
    raise SystemExit("staging VM-owned volume inventory must contain exactly root and EFI")
others = observed - {root}
if len(others) != 1 or not re.fullmatch(
    rf"{re.escape(storage)}:vm-{vmid}-disk-[1-9][0-9]*",
    next(iter(others)),
):
    raise SystemExit("staging EFI volume inventory is invalid")
PY
}

verify_snapshot_and_clone() {
  local require_quiescent="${1:-false}"
  local volume node dataset guid expected_guid origin clone_path
  STAGING_DATASET="$(volume_dataset_on_node "$STAGING_NODE" "$STAGING_VOLUME")" ||
    die "Could not resolve the staging clone dataset"
  origin="$(node_exec "$STAGING_NODE" zfs get -Hp -o value origin "$STAGING_DATASET")" ||
    die "Could not read the staging clone origin"
  for volume in "${SOURCE_VOLUMES[@]}"; do
    for node in "${SOURCE_PLACEMENT[@]}"; do
      expected_guid="$(
        registry_cmd get "$RESOURCE_NAME" |
          python3 -c '
import json,sys
row=json.load(sys.stdin)
print(row["proxmox"]["snapshot"]["volume_guids"][sys.argv[1]][sys.argv[2]])
' "$volume" "$node"
      )" || die "Could not read registered snapshot GUID metadata"
      dataset="$(volume_dataset_on_node "$node" "$volume")" ||
        die "Could not resolve $volume on $node"
      guid="$(
        node_exec "$node" zfs get -Hp -o value guid \
          "${dataset}@${SNAPSHOT_NAME}"
      )" || die "Required snapshot copy is absent on $node"
      [[ "$guid" == "$expected_guid" ]] ||
        die "Snapshot GUID differs for $volume on $node"
      if [[ "$node" == "$STAGING_NODE" &&
        "$volume" == "${SOURCE_VOLUMES[0]}" ]]; then
        [[ "$origin" == "${dataset}@${SNAPSHOT_NAME}" ]] ||
          die "Staging clone origin differs from its registered source snapshot"
      fi
    done
  done
  clone_path="$(
    node_exec "$STAGING_NODE" pvesm path "$STAGING_VOLUME"
  )" || die "Could not resolve the staging clone block path"
  # shellcheck disable=SC2016 # This script is evaluated by remote Bash.
  node_exec "$STAGING_NODE" bash -c '
set -Eeuo pipefail
path="$1"; dataset="$2"; require_quiescent="$3"
[[ "$path" == "/dev/zvol/${dataset}" && -b "$path" ]]
resolved="$(readlink -f -- "$path")"
[[ "$resolved" =~ ^/dev/[A-Za-z0-9._/+:-]+$ && -b "$resolved" ]]
if [[ "$require_quiescent" == true ]]; then
  ! findmnt -rn -S "$path" >/dev/null 2>&1
  ! findmnt -rn -S "$resolved" >/dev/null 2>&1
  ! fuser "$path" >/dev/null 2>&1
  ! fuser "$resolved" >/dev/null 2>&1
fi
' bash "$clone_path" "$STAGING_DATASET" "$require_quiescent" ||
    die "Staging clone is mounted, open, or has an unsafe block identity"
}

registry_update() {
  local value
  value="$(
    registry_cmd update "$RESOURCE_NAME" \
      --expected-revision "$RESOURCE_REVISION" "$@"
  )" || die "Registry update failed during staging destruction"
  RESOURCE_REVISION="$(
    python3 -c 'import json,sys; print(json.loads(sys.argv[1])["revision"])' \
      "$value"
  )"
  RESOURCE_STATE="$(
    python3 -c 'import json,sys; print(json.loads(sys.argv[1])["state"])' \
      "$value"
  )"
}

queue_cleanup_plan() {
  local node
  registry_cmd defer-cleanup \
    --resource "$RESOURCE_NAME" \
    --node "$STAGING_NODE" \
    --action destroy-vm \
    --target "vm:${VMID}" \
    --reason "operator requested complete staging VM destruction" \
    >/dev/null ||
    die "Could not queue staging VM destruction"
  registry_cmd defer-cleanup \
    --resource "$RESOURCE_NAME" \
    --node "$STAGING_NODE" \
    --action destroy-volume \
    --target "$STAGING_VOLUME" \
    --reason "operator requested linked-clone destruction" \
    >/dev/null ||
    die "Could not queue staging clone destruction"
  for node in "${SOURCE_PLACEMENT[@]}"; do
    registry_cmd defer-cleanup \
      --resource "$RESOURCE_NAME" \
      --node "$node" \
      --action delete-snapshot \
      --target "vm-${SOURCE_VMID}@${SNAPSHOT_NAME}" \
      --reason "operator requested deletion of this staging guest snapshot" \
      >/dev/null ||
      die "Could not queue exact snapshot cleanup on $node"
  done
  for node in "${CLUSTER_NODES[@]}"; do
    registry_cmd defer-cleanup \
      --resource "$RESOURCE_NAME" \
      --node "$node" \
      --action remove-route \
      --target "$RESOURCE_NAME" \
      --reason "operator requested staging route removal" \
      >/dev/null ||
      die "Could not queue route cleanup on $node"
  done
  registry_update --state cleanup_pending --routes-disabled
}

wait_for_durable_cleanup() {
  local deadline=$((SECONDS + 1200)) node resources verdict
  local unconfirmed=0
  while ((SECONDS < deadline)); do
    for node in "${CLUSTER_NODES[@]}"; do
      node_exec "$node" systemctl start --no-block \
        app-ha-deferred-cleanup.service >/dev/null 2>&1 || true
    done
    # "Gone" must be a positive answer. "get" exits 1 with no output both when
    # the record was deleted and when the registry could not be read, and a
    # dropped SSH connection looks the same. "list" exits 0 with a JSON array
    # whenever the registry is readable, so only a successful listing that
    # lacks this name counts as deleted; anything else is unknown and retried.
    verdict=unknown
    if resources="$(registry_cmd list 2>/dev/null)"; then
      verdict="$(
        python3 - "$resources" "$RESOURCE_NAME" "$RESOURCE_ID" <<'PY' 2>/dev/null
import json
import sys

rows = json.loads(sys.argv[1])
if not isinstance(rows, list) or not all(isinstance(row, dict) for row in rows):
    raise SystemExit(1)
matches = [row for row in rows if row.get("name") == sys.argv[2]]
if not matches:
    print("gone")
elif (
    len(matches) == 1
    and matches[0].get("id") == sys.argv[3]
    and matches[0].get("state") == "cleanup_pending"
    and matches[0].get("routes_enabled") is False
):
    print("pending")
else:
    print("changed")
PY
      )" || verdict=unknown
    fi
    case "$verdict" in
      gone)
        return 0
        ;;
      pending)
        unconfirmed=0
        ;;
      changed)
        die "Staging cleanup resource identity or state changed"
        ;;
      *)
        ((unconfirmed += 1))
        if ((unconfirmed == 1)); then
          info "Could not read the registry; cleanup is unconfirmed, retrying" >&2
        fi
        ;;
    esac
    sleep 5
  done
  registry_cmd list --record-type cleanup >&2 || true
  if ((unconfirmed > 0)); then
    die "Timed out without confirming staging cleanup: the last ${unconfirmed} registry queries failed, so the resource may or may not be gone; queued actions will continue retrying"
  fi
  die "Timed out waiting for durable staging cleanup; queued actions will continue retrying"
}

remove_workstation_alias() {
  [[ -n "${HOME:-}" && "$HOME" == /* ]] || return 0
  local ssh_dir="${HOME}/.ssh" config="${HOME}/.ssh/config"
  local known_hosts="${HOME}/.ssh/known_hosts"
  [[ ! -L "$ssh_dir" && ! -L "$config" && ! -L "$known_hosts" ]] || {
    info "Skipped workstation SSH cleanup because a managed path is a symlink"
    return 0
  }
  [[ -f "$config" ]] || return 0
  local staged="${RUN_DIR}/ssh-config.new"
  local result=0
  python3 - "$config" "$staged" "$SSH_ALIAS" <<'PY' || result=$?
from pathlib import Path
import sys
source, destination, alias = sys.argv[1:]
begin = f"# BEGIN app-ha managed staging guest {alias}"
end = f"# END app-ha managed staging guest {alias}"
lines = Path(source).read_text(encoding="utf-8").splitlines()
kept = []
inside = False
found = False
for line in lines:
    if line == begin:
        if inside:
            raise SystemExit("nested managed SSH block")
        inside = True
        found = True
        continue
    if line == end and inside:
        inside = False
        continue
    if not inside:
        kept.append(line)
if inside:
    raise SystemExit("unterminated managed SSH block")
Path(destination).write_text("\n".join(kept).rstrip() + "\n", encoding="utf-8")
raise SystemExit(0 if found else 3)
PY
  case "$result" in
    0)
      install -m 0600 "$staged" "$config"
      if [[ -f "$known_hosts" ]]; then
        ssh-keygen -R "$SSH_ALIAS" -f "$known_hosts" >/dev/null 2>&1 || true
      fi
      info "Removed managed workstation SSH alias: $SSH_ALIAS"
      ;;
    3) info "No managed workstation SSH block found for $SSH_ALIAS" ;;
    *) die "Could not safely inspect the workstation SSH config" ;;
  esac
}

main() {
  parse_args "$@"
  [[ -f "$CONFIG_LIB" && ! -L "$CONFIG_LIB" ]] ||
    die "Configuration library is unavailable"
  # shellcheck disable=SC1090
  source "$CONFIG_LIB"
  load_proxmox_config --no-secrets ||
    die "Could not load cluster configuration"
  local command_name
  for command_name in awk bash date install mktemp python3 rm ssh ssh-keygen; do
    command -v "$command_name" >/dev/null 2>&1 ||
      die "Required workstation command is unavailable: $command_name"
  done
  RUN_DIR="$(mktemp -d "${TMPDIR:-/tmp}/destroy-staging-vm.XXXXXX")"
  chmod 0700 "$RUN_DIR"
  trap cleanup EXIT
  COORDINATOR="$(first_reachable_mox)" ||
    die "No reachable mox coordinator was found"
  node_exec "$COORDINATOR" test -x "$REMOTE_REGISTRY" ||
    die "Installed cluster registry is unavailable"

  local resources nodes live ha replication cleanup_records
  resources="$(registry_cmd list)" || die "Could not list registry resources"
  select_resource "$resources"
  [[ "$RESOURCE_NAME" =~ ^stage[1-9][0-9]*prod[1-9][0-9]*$ ]] ||
    die "A staging resource name such as stage1prod1 is required"
  nodes="$(pvesh_get /nodes)" || die "Could not list cluster nodes"
  live="$(pvesh_get /cluster/resources --type vm)" ||
    die "Could not list live VMs"
  ha="$(pvesh_get /cluster/ha/resources)" ||
    die "Could not list HA resources"
  replication="$(pvesh_get /cluster/replication)" ||
    die "Could not list replication jobs"
  cleanup_records="$(registry_cmd list --record-type cleanup)" ||
    die "Could not list deferred cleanup records"
  parse_and_validate_plan \
    "$resources" "$live" "$nodes" "$ha" "$replication" "$cleanup_records"
  if [[ "$LIVE_PRESENT" == 1 ]]; then
    validate_live_vm || die "Live staging VM failed exact identity validation"
    validate_owned_volumes ||
      die "Staging VM-owned volume inventory failed validation"
  fi
  verify_snapshot_and_clone false

  log "Validated staging destruction plan"
  info "Resource: $RESOURCE_NAME / VMID $VMID / state $RESOURCE_STATE"
  info "Source: $SOURCE_NAME (VMID $SOURCE_VMID; owner $SOURCE_OWNER)"
  info "Staging node/clone: $STAGING_NODE / $STAGING_VOLUME"
  info "Snapshot: $SNAPSHOT_NAME on ${SOURCE_PLACEMENT[*]}"
  info "Domain/route enabled: $STAGING_DOMAIN / $ROUTES_ENABLED"
  info "Live VM present: $LIVE_PRESENT"
  if [[ "$DRY_RUN" == true ]]; then
    log "Dry run complete; no state was changed"
    return 0
  fi

  printf '\nTHIS PERMANENTLY DESTROYS %s, ITS LINKED CLONE, AND ITS ONE EXACT SOURCE SNAPSHOT.\n' \
    "$RESOURCE_NAME"
  printf 'Type exactly: DESTROY %s\n> ' "$RESOURCE_NAME"
  local confirmation
  IFS= read -r confirmation
  [[ "$confirmation" == "DESTROY ${RESOURCE_NAME}" ]] ||
    die "Destruction confirmation did not match"

  IFS= read -r -p \
    "Workstation SSH alias to remove after success [${RESOURCE_NAME}]: " SSH_ALIAS
  SSH_ALIAS="${SSH_ALIAS:-$RESOURCE_NAME}"
  [[ "$SSH_ALIAS" =~ ^[A-Za-z0-9][A-Za-z0-9_.-]{0,63}$ ]] ||
    die "SSH alias is unsafe"

  log "Disabling staging ingress and committing durable cleanup"
  case "$RESOURCE_STATE" in
    active)
      registry_update --state stopping --routes-disabled
      ;;
    ready)
      registry_update --routes-disabled
      ;;
    stopping | stopped | failed)
      registry_update --routes-disabled
      ;;
    *)
      die "Unsupported staging destruction state: $RESOURCE_STATE"
      ;;
  esac
  queue_cleanup_plan
  mox_ssh "$COORDINATOR" "$REMOTE_HAPROXY_SYNC" \
    --lock-timeout 240 </dev/null ||
    die "Could not converge HAProxy after route removal"

  log "Waiting for the shared cleanup workers to remove the exact staging payload"
  wait_for_durable_cleanup
  remove_workstation_alias

  log "Staging VM destruction complete"
  info "Destroyed resource: $RESOURCE_NAME (former VMID $VMID)"
  info "Removed route, VM disks, linked clone, $SNAPSHOT_NAME and its replicated copies, and registry metadata."
}

if [[ "${APP_HA_DESTROY_STAGING_SOURCE_ONLY:-0}" != 1 ]]; then
  main "$@"
fi
