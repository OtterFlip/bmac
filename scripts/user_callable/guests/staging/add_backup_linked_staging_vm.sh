#!/usr/bin/env bash

# Copyright (c) 2026 BEENTHERE VENTURES, INC.
# SPDX-License-Identifier: GPL-3.0-only

# Instant Restore to Backup-Linked Staging VM: a disposable staging VM in one
# of a production VM's staging slots, booted from one of its restore points
# on PBS within about a minute. Its disk is a qcow2 overlay over the restore
# point, which backup_link.sh maps read-only from PBS on the VM's host; the
# overlay takes every write and is patched offline with the slot's identity
# before the first boot, and the restore point's data is never copied.
#
# It is a staging VM in every other way: the same slots, per-host quota,
# lifecycle hook, eviction when a production VM starts on its host, and
# removal (remove_staging_vm.sh). It is also evicted when its mapping dies,
# for example when its host reboots. This script reuses add_staging_vm.sh's
# generic steps; the restore point replaces the production snapshot and its
# linked clone, so the production VM need not be running.

set -Eeuo pipefail
set +x
umask 077

BACKUP_LINKED_SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
# shellcheck source=add_staging_vm.sh
APP_HA_STAGING_VM_SOURCE_ONLY=1 source "${BACKUP_LINKED_SCRIPT_DIR}/add_staging_vm.sh"
# shellcheck source=../../../lib/backups.sh
source "${REPO_ROOT}/scripts/lib/backups.sh"

OVERLAY_STORAGE_ID=bmac-overlay
OVERLAY_STORAGE_PATH=/var/lib/bmac-overlay
REMOTE_BACKUP_LINK="${REMOTE_INSTALL_ROOT}/lib/backup_link.sh"
SNAPSHOT=""
SNAPSHOT_TIME=""
SNAPSHOT_BYTES=""
NODE_REQUEST=""
MAPPED=false
OVERLAY_PATH=""

usage() {
  cat <<'EOF'
Usage: add_backup_linked_staging_vm.sh [--snapshot vm/VMID/TIME] [--node moxN]
         [--cores N] [--memory-gib N] [--sanitizer PATH] [--link-down]
         [--no-start] [--no-jump-ssh] [--start-timeout-seconds N] [--json]

Instant Restore to Backup-Linked Staging VM: create a disposable staging VM
from one of a production VM's restore points on PBS, in the production VM's
lowest free staging slot (stageKprodN, its address and FQDN). The VM boots
from the restore point within about a minute: its disk is mapped read-only
from PBS on its host, with a local copy-on-write overlay that takes its
writes, and is patched with the slot's identity before the first boot. The
restore point's data is never copied to the cluster; reads of data the VM
has not changed come from PBS and are slower than local disk.

It counts toward the host's staging quota, runs only on a host of the
production VM's placement where no production VM runs, and is evicted like
any staging VM when a production VM starts on its host, and also when its
PBS mapping dies (for example when its host reboots). Remove it with
remove_staging_vm.sh. The production VM itself need not be running.

  --snapshot vm/VMID/TIME   The restore point (chosen from a list when omitted).
  --node moxN               The host (chosen when more than one is eligible).
  --cores N, --memory-gib N Default: STAGING_VM_CORES, STAGING_VM_MEMORY_GIB.
  --sanitizer PATH          An idempotent root startup.sh run before networking.
  --link-down               Create the VM's NIC link-down.
  --no-start                Leave the VM stopped.
  --no-jump-ssh             Do not set up `ssh stageKprodN` on this workstation.
EOF
}

parse_args() {
  while (($#)); do
    case "$1" in
      --link-down) NETWORK_LINK_DOWN=true ;;
      --no-start) START_AFTER_CREATION=false ;;
      --no-jump-ssh) SETUP_WORKSTATION_JUMP_SSH=false ;;
      --snapshot | --node | --cores | --memory-gib | --sanitizer | --start-timeout-seconds)
        (($# >= 2)) || die "$1 needs a value"
        case "$1" in
          --snapshot) SNAPSHOT="$2" ;;
          --node) NODE_REQUEST="$2" ;;
          --cores) CORES_OVERRIDE="$2" ;;
          --memory-gib) MEMORY_GIB_OVERRIDE="$2" ;;
          --sanitizer) SANITIZER_FILE="$2" ;;
          --start-timeout-seconds) START_TIMEOUT_SECONDS="$2" ;;
        esac
        shift
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
    shift
  done
  [[ -z "$SNAPSHOT" || "$SNAPSHOT" =~ ^vm/[1-9][0-9]{0,8}/[0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9]{2}:[0-9]{2}:[0-9]{2}Z$ ]] ||
    die "--snapshot must look like vm/100/2026-10-27T14:50:01Z"
  [[ -z "$NODE_REQUEST" || "$NODE_REQUEST" =~ ^mox([1-9]|10)$ ]] || die "--node must be moxN"
  [[ -z "$CORES_OVERRIDE" ]] || validate_positive_integer "--cores" "$CORES_OVERRIDE"
  [[ -z "$MEMORY_GIB_OVERRIDE" ]] || validate_positive_integer "--memory-gib" "$MEMORY_GIB_OVERRIDE"
  validate_positive_integer "--start-timeout-seconds" "$START_TIMEOUT_SECONDS"
}

# BMAC's restore points of registered production VMs, newest first: snapshot,
# production name, size in bytes, and a label, separated by \x1f.
list_restore_points() {
  local content="${RUN_DIR}/content.json"
  pvesh_get "/nodes/${COORDINATOR}/storage/${BACKUP_STORAGE_ID}/content" --content backup >"$content" ||
    die "Could not list the restore points on ${BACKUP_STORAGE_ID}"
  python3 - "$content" "${RUN_DIR}/resources.json" "$BACKUP_STORAGE_ID" <<'PY'
import calendar
import json
import sys
import time

rows = json.load(open(sys.argv[1], encoding="utf-8"))
productions = {
    row["name"]: row
    for row in json.load(open(sys.argv[2], encoding="utf-8"))
    if row.get("kind") == "production"
}
prefix = sys.argv[3] + ":backup/"
points = []
for row in rows:
    volid = str(row.get("volid", ""))
    words = str(row.get("notes") or "").split()
    if not volid.startswith(prefix) or words[:1] != ["bmac"] or len(words) < 2:
        continue
    record = productions.get(words[1])
    if record is None or str(record["vmid"]) != str(row.get("vmid")):
        continue
    snapshot = volid[len(prefix):]
    taken = calendar.timegm(time.strptime(snapshot.rsplit("/", 1)[-1], "%Y-%m-%dT%H:%M:%SZ"))
    label = f"{words[1]} · {time.strftime('%Y-%m-%d %H:%M %Z', time.localtime(taken))}"
    comment = str(row.get("notes") or "").splitlines()[0].partition(" | ")[2].strip()
    if comment:
        label += " · " + (comment if len(comment) <= 48 else comment[:47] + "…")
    if row.get("protected"):
        label += " · protected"
    points.append((snapshot, words[1], int(row.get("size") or 0), label))
for point in sorted(points, key=lambda point: point[0].rsplit("/", 1)[-1], reverse=True):
    print("\x1f".join(str(value) for value in point))
PY
}

choose_restore_point() {
  CURRENT_PHASE="choosing the restore point"
  local -a points=() options=()
  local row snapshot name bytes label
  mapfile -t points < <(list_restore_points)
  ((${#points[@]})) || die "No BMAC restore point of a registered production VM is on ${BACKUP_STORAGE_ID}"
  if [[ -z "$SNAPSHOT" ]]; then
    for row in "${points[@]}"; do
      IFS=$'\x1f' read -r snapshot name bytes label <<<"$row"
      options+=("$snapshot" "$label")
    done
    if bmac_ui_is_json; then
      bmac_ui_choose SNAPSHOT "Restore point to boot" "${options[0]}" "${options[@]}"
    else
      printf '\nRestore points, newest first:\n'
      for row in "${points[@]}"; do
        IFS=$'\x1f' read -r snapshot name bytes label <<<"$row"
        printf '  %s  %s\n' "$snapshot" "$label"
      done
      read -r -p "Restore point [${options[0]}]: " SNAPSHOT
      SNAPSHOT="${SNAPSHOT:-${options[0]}}"
    fi
  fi
  for row in "${points[@]}"; do
    IFS=$'\x1f' read -r snapshot name bytes label <<<"$row"
    if [[ "$snapshot" == "$SNAPSHOT" ]]; then
      SOURCE_NAME="$name"
      SNAPSHOT_BYTES="$bytes"
      SNAPSHOT_TIME="${label#* · }"
      SNAPSHOT_TIME="${SNAPSHOT_TIME%% · *}"
    fi
  done
  [[ -n "$SOURCE_NAME" ]] || die "$SNAPSHOT is not a BMAC restore point of a registered production VM"
  [[ "$SNAPSHOT_BYTES" =~ ^[1-9][0-9]*$ ]] || die "The size of $SNAPSHOT is unknown"
  info "Restore point: $SNAPSHOT of $SOURCE_NAME, taken $SNAPSHOT_TIME"
}

# The production VM whose staging slot the VM takes. It may be in any state:
# nothing it runs or stores is used.
parse_backup_source() {
  local -a fields=()
  mapfile -d '' -t fields < <(
    python3 - "${RUN_DIR}/resources.json" "$SOURCE_NAME" <<'PY'
import json
import sys

rows = [row for row in json.load(open(sys.argv[1], encoding="utf-8"))
        if row.get("kind") == "production" and row.get("name") == sys.argv[2]]
if len(rows) != 1:
    raise SystemExit("the restore point's production VM is not registered")
row = rows[0]
for value in (row["purpose"]["slug"], row["domains"]["primary"],
              row.get("owner_node") or "", ",".join(row["placement"])):
    sys.stdout.buffer.write(str(value).encode() + b"\0")
PY
  ) || die "Could not read $SOURCE_NAME from the registry"
  ((${#fields[@]} == 4)) || die "Could not read $SOURCE_NAME from the registry"
  SOURCE_PURPOSE="${fields[0]}"
  SOURCE_PRIMARY_DOMAIN="${fields[1]}"
  SOURCE_REGISTRY_OWNER="${fields[2]}"
  IFS=',' read -r -a SOURCE_PLACEMENT <<<"${fields[3]}"
}

# Hosts of the production VM's placement, other than its registered owner,
# online, and running no production VM (the lifecycle hook refuses to start
# a staging VM beside one).
choose_backup_linked_node() {
  CURRENT_PHASE="choosing the staging host"
  write_json "${RUN_DIR}/cluster-vms.json" "$(pvesh_get /cluster/resources --type vm)"
  local -a candidates=() options=()
  local node
  mapfile -t candidates < <(
    python3 - "${RUN_DIR}/cluster-vms.json" "${RUN_DIR}/resources.json" "$SOURCE_REGISTRY_OWNER" \
      "$(IFS=,; printf '%s' "${SOURCE_PLACEMENT[*]}")" "$(IFS=,; printf '%s' "${ONLINE_NODES[*]}")" <<'PY'
import json
import sys

live = json.load(open(sys.argv[1], encoding="utf-8"))
production_vmids = {str(row["vmid"]) for row in json.load(open(sys.argv[2], encoding="utf-8"))
                    if row.get("kind") == "production"}
owner, placement, online = sys.argv[3], sys.argv[4].split(","), set(sys.argv[5].split(","))
busy = {row.get("node") for row in live
        if str(row.get("vmid")) in production_vmids and row.get("status") == "running"}
for node in placement:
    if node != owner and node in online and node not in busy:
        print(node)
PY
  )
  ((${#candidates[@]})) ||
    die "No host can run a staging VM of $SOURCE_NAME: it needs an online host of ${SOURCE_PLACEMENT[*]}, other than ${SOURCE_REGISTRY_OWNER:-its owner}, with no production VM running"
  if [[ -n "$NODE_REQUEST" ]]; then
    for node in "${candidates[@]}"; do
      [[ "$node" != "$NODE_REQUEST" ]] || STAGING_NODE="$node"
    done
    [[ -n "$STAGING_NODE" ]] || die "$NODE_REQUEST cannot run it; eligible: ${candidates[*]}"
  elif ((${#candidates[@]} == 1)); then
    STAGING_NODE="${candidates[0]}"
  elif bmac_ui_is_json; then
    for node in "${candidates[@]}"; do
      options+=("$node" "$node")
    done
    bmac_ui_choose STAGING_NODE "Host for the staging VM" "${candidates[0]}" "${options[@]}"
  else
    prompt_with_default STAGING_NODE "Host (${candidates[*]})" "${candidates[0]}"
    [[ " ${candidates[*]} " == *" $STAGING_NODE "* ]] || die "$STAGING_NODE is not eligible"
  fi
  info "Host: $STAGING_NODE"
  validate_staging_host_capacity
}

collect_backup_linked_options() {
  local default_memory_gib="$STAGING_VM_MEMORY_GIB" requested_memory_gib
  local cores memory start jump link_down sanitizer sanitizer_supplied="$SANITIZER_FILE"
  if bmac_ui_is_json; then
    bmac_ui_group_begin staging_options "Backup-linked staging VM options" \
      "Boots $SNAPSHOT of $SOURCE_NAME ($SNAPSHOT_TIME) on $STAGING_NODE, in the lowest free staging slot of $SOURCE_NAME (currently stage${STAGING_CANDIDATE_INDEX}${SOURCE_NAME}.${SOURCE_PRIMARY_DOMAIN}). Reads of data it has not changed come from PBS."
    [[ -n "$CORES_OVERRIDE" ]] ||
      bmac_ui_group_add cores --id cores --type integer --label "vCPU cores" \
        --default "$STAGING_VM_CORES" --min 1 --required
    [[ -n "$MEMORY_GIB_OVERRIDE" ]] ||
      bmac_ui_group_add memory --id memory_gib --type integer --label "RAM" --suffix GiB \
        --default "$default_memory_gib" --min 1 --required
    bmac_ui_group_add start --id start_after_creation --type boolean \
      --label "Start the VM automatically after it has been created" --default "$START_AFTER_CREATION"
    bmac_ui_group_add jump --id setup_jump_ssh --type boolean \
      --label "Set up permanent jump SSH after the VM has started" --default "$SETUP_WORKSTATION_JUMP_SSH"
    bmac_ui_group_add link_down --id link_down --type boolean \
      --label "Create the staging NIC initially link-down" --default "$NETWORK_LINK_DOWN"
    [[ -n "$sanitizer_supplied" ]] ||
      bmac_ui_group_add sanitizer --id sanitizer --type file \
        --label "Application sanitizer (optional)" \
        --help "An idempotent root startup.sh with a Bash shebang, at most 1 MiB. Without one the VM keeps the restore point's credentials, jobs, and data."
    while true; do
      bmac_ui_group_request
      if [[ -z "$CORES_OVERRIDE" && ! "$cores" =~ ^[1-9][0-9]*$ ]]; then
        bmac_ui_field_error cores "Enter a positive whole number"
      fi
      if [[ -z "$MEMORY_GIB_OVERRIDE" && ! "$memory" =~ ^[1-9][0-9]*$ ]]; then
        bmac_ui_field_error memory_gib "Enter a positive whole number of GiB"
      fi
      if [[ -z "$sanitizer_supplied" && -n "$sanitizer" ]]; then
        SANITIZER_FILE="$sanitizer"
        if ! (
          die() { printf '%s\n' "$*" >&2; exit 1; }
          validate_sanitizer_file
        ) 2>"${RUN_DIR}/sanitizer.err" >/dev/null; then
          bmac_ui_field_error sanitizer "$(tail -n 1 "${RUN_DIR}/sanitizer.err")"
        fi
      fi
      bmac_ui_group_check && break
    done
    STAGING_VM_CORES="${CORES_OVERRIDE:-$cores}"
    requested_memory_gib="${MEMORY_GIB_OVERRIDE:-$memory}"
    START_AFTER_CREATION="$([[ "$start" == true ]] && echo true || echo false)"
    SETUP_WORKSTATION_JUMP_SSH="$([[ "$jump" == true ]] && echo true || echo false)"
    NETWORK_LINK_DOWN="$([[ "$link_down" == true ]] && echo true || echo false)"
    [[ -n "$sanitizer_supplied" ]] || SANITIZER_FILE="$sanitizer"
  else
    if [[ -n "$CORES_OVERRIDE" ]]; then
      STAGING_VM_CORES="$CORES_OVERRIDE"
    else
      prompt_with_default STAGING_VM_CORES "Staging vCPU cores" "$STAGING_VM_CORES"
      validate_positive_integer "Staging vCPU cores" "$STAGING_VM_CORES"
    fi
    if [[ -n "$MEMORY_GIB_OVERRIDE" ]]; then
      requested_memory_gib="$MEMORY_GIB_OVERRIDE"
    else
      prompt_with_default requested_memory_gib "Staging RAM in GiB" "$default_memory_gib"
      validate_positive_integer "Staging RAM in GiB" "$requested_memory_gib"
    fi
    if [[ -z "$SANITIZER_FILE" ]]; then
      prompt_optional SANITIZER_FILE \
        "Optional absolute path to an idempotent root startup.sh (blank for none)"
    fi
  fi
  [[ -z "$SANITIZER_FILE" ]] || validate_sanitizer_file
  STAGING_VM_MEMORY_MB=$((requested_memory_gib * 1024))
  if [[ -z "$SANITIZER_FILE" && "$NETWORK_LINK_DOWN" == false ]]; then
    warn "NO application sanitizer was supplied and guest networking will be enabled."
    warn "The VM can still contain the restore point's credentials, jobs, data, and outbound integrations."
    confirm_exact_local \
      "Identity and network cleanup is not application-secret sanitization." \
      "ALLOW UNSANITIZED STAGING NETWORK"
  fi
}

# The directory storage that holds every backup-linked staging VM's overlay,
# on each host's root filesystem. Created once per cluster.
ensure_overlay_storage() {
  CURRENT_PHASE="checking the overlay storage"
  local storage
  if storage="$(pvesh_get "/storage/${OVERLAY_STORAGE_ID}" 2>/dev/null)"; then
    python3 - "$storage" "$OVERLAY_STORAGE_PATH" <<'PY' || die "Storage bmac-overlay exists but is not the directory storage BMAC expects; remove it and run again"
import json
import sys

row = json.loads(sys.argv[1])
content = set(str(row.get("content", "")).split(","))
if row.get("type") != "dir" or row.get("path") != sys.argv[2] or "images" not in content or row.get("shared"):
    raise SystemExit(1)
PY
    return 0
  fi
  log "Adding the overlay storage ${OVERLAY_STORAGE_ID} (${OVERLAY_STORAGE_PATH} on every host)"
  node_exec "$COORDINATOR" pvesm add dir "$OVERLAY_STORAGE_ID" --path "$OVERLAY_STORAGE_PATH" \
    --content images --mkdir 1 >/dev/null ||
    die "Could not add the overlay storage ${OVERLAY_STORAGE_ID}"
}

reserve_backup_linked() {
  CURRENT_PHASE="atomically reserving staging identity"
  query_next_vmid
  local disk_gib=$(((SNAPSHOT_BYTES + 1073741823) / 1073741824)) result resource
  result="$(registry_cmd allocate-staging \
    --source "$SOURCE_NAME" \
    --vmid "$STAGING_VMID" \
    --placement "$STAGING_NODE" \
    --placement-limit "$STAGING_HOST_LIMIT" \
    --allocation-id "create-backup-linked-${SOURCE_NAME}-$(date -u +%Y%m%dT%H%M%SZ)-$$" \
    --backup-storage "$BACKUP_STORAGE_ID" \
    --backup-snapshot "$SNAPSHOT" \
    --cores "$STAGING_VM_CORES" \
    --memory-mb "$STAGING_VM_MEMORY_MB" \
    --disk-gib "$disk_gib" \
    --disk-allocation sparse)" ||
    die "Atomic staging allocation failed"
  resource="$(
    python3 - "$result" <<'PY'
import json
import sys
value = json.loads(sys.argv[1])
if value.get("created") is not True:
    raise SystemExit("staging creation requires a fresh allocation")
print(json.dumps(value["resource"], sort_keys=True))
PY
  )" || die "Registry did not return a fresh staging allocation"
  parse_staging_resource "$resource"
  REGISTRY_RESERVED=true
  STAGING_VOLUME="${OVERLAY_STORAGE_ID}:${STAGING_VMID}/vm-${STAGING_VMID}-disk-1.qcow2"
  validate_safe_id "staging volume" "$STAGING_VOLUME"
  info "Reserved: $STAGING_NAME / $STAGING_IP / VMID $STAGING_VMID"
  info "MAC/domain: $STAGING_MAC / $STAGING_DOMAIN"
  python3 - "$(pvesh_get /cluster/resources --type vm)" "$STAGING_VMID" <<'PY' ||
import json
import sys
sys.exit(1 if any(str(row.get("vmid")) == sys.argv[2] for row in json.loads(sys.argv[1])) else 0)
PY
    die "VMID $STAGING_VMID appeared in Proxmox after it was reserved"
}

create_backup_linked_vm() {
  CURRENT_PHASE="creating the stopped staging VM shell"
  local hook purpose_tag tags net0 hook_path
  hook="$(hook_reference)"
  purpose_tag="${PURPOSE_TAG_PREFIX}${SOURCE_PURPOSE}"
  validate_safe_id "purpose tag" "$purpose_tag"
  tags="${STAGING_VM_TAG};${EVICTABLE_VM_TAG};${purpose_tag}"
  net0="virtio=${STAGING_MAC},bridge=${STAGING_VM_BRIDGE},firewall=1"
  [[ "$NETWORK_LINK_DOWN" == false ]] || net0+=",link_down=1"
  hook_path="$(node_exec "$STAGING_NODE" pvesm path "$hook")" ||
    die "Lifecycle hook cannot be resolved on $STAGING_NODE"
  node_exec "$STAGING_NODE" test -x "$hook_path" ||
    die "Lifecycle hook is unavailable on $STAGING_NODE"
  node_exec "$STAGING_NODE" test -x "$REMOTE_BACKUP_LINK" ||
    die "$STAGING_NODE lacks backup_link.sh; run update_cluster_runtime.sh first"
  VM_MAY_EXIST=true
  node_exec "$STAGING_NODE" qm create "$STAGING_VMID" \
    --name "$STAGING_NAME" \
    --description "app-ha disposable backup-linked staging VM of ${SOURCE_NAME}; restore point ${SNAPSHOT}" \
    --machine q35 --bios ovmf --sockets 1 \
    --cores "$STAGING_VM_CORES" --memory "$STAGING_VM_MEMORY_MB" --balloon 0 \
    --cpu "$PROD_VM_CPU_TYPE" --ostype l26 --scsihw virtio-scsi-single \
    --efidisk0 "${STAGING_VM_STORAGE}:1,efitype=4m,pre-enrolled-keys=1" \
    --net0 "$net0" --agent "enabled=1,fstrim_cloned_disks=1" \
    --hookscript "$hook" --tags "$tags" --vga std --tablet 0 --onboot 0 >/dev/null ||
    die "Could not create the staging VM shell"
  verify_backup_linked_config unattached
}

verify_backup_linked_config() {
  local mode="$1" config="${RUN_DIR}/stage-qm.conf"
  node_exec "$STAGING_NODE" qm config "$STAGING_VMID" >"$config"
  python3 - "$config" "$mode" "$STAGING_NAME" "$STAGING_MAC" "$STAGING_VM_BRIDGE" "$(hook_reference)" \
    "${STAGING_VM_TAG};${EVICTABLE_VM_TAG};${PURPOSE_TAG_PREFIX}${SOURCE_PURPOSE}" \
    "$STAGING_VOLUME" "$STAGING_VM_STORAGE" "$STAGING_VMID" "$NETWORK_LINK_DOWN" <<'PY' ||
import re
import sys

path, mode, name, mac, bridge, hook, tags, volume, storage, vmid, link_down = sys.argv[1:]
config = {}
for line in open(path, encoding="utf-8"):
    if ": " in line:
        key, value = line.rstrip("\n").split(": ", 1)
        config[key] = value
problems = []
if config.get("name") != name:
    problems.append("name")
if config.get("onboot", "0") != "0" or config.get("template", "0") == "1":
    problems.append("onboot or template")
if config.get("hookscript") != hook:
    problems.append("lifecycle hook")
if {t for t in re.split(r"[;,]", config.get("tags", "")) if t} != set(tags.split(";")):
    problems.append("tags")
if [k for k in config if re.fullmatch(r"net[0-9]+", k)] != ["net0"]:
    problems.append("NICs")
net0 = config.get("net0", "")
if f"virtio={mac}".lower() not in net0.lower() or f"bridge={bridge}" not in net0:
    problems.append("net0")
if ("link_down=1" in net0) != (link_down == "true"):
    problems.append("link_down")
disks = {k for k in config if re.fullmatch(r"(?:scsi|sata|virtio|ide|unused|efidisk|tpmstate)[0-9]+", k)}
if disks != ({"efidisk0"} if mode == "unattached" else {"scsi0", "efidisk0"}):
    problems.append("disks")
if not re.fullmatch(rf"{re.escape(storage)}:vm-{vmid}-disk-[0-9]+", config.get("efidisk0", "").split(",")[0]):
    problems.append("EFI disk")
if mode == "attached" and config.get("scsi0", "").split(",")[0] != volume:
    problems.append("scsi0")
if problems:
    raise SystemExit("the staging VM's " + ", ".join(problems) + " differ from its registry identity")
PY
    die "The staging VM's configuration differs from its contract"
  stage_vm_is_ha && die "Staging VM must never be registered with Proxmox HA"
  [[ "$(node_exec "$STAGING_NODE" qm status "$STAGING_VMID")" == "status: stopped" ]] ||
    die "Staging VM must remain stopped until it is patched"
}

map_restore_point() {
  CURRENT_PHASE="mapping the restore point from PBS"
  registry_update_stage --state cloning --owner-node "$STAGING_NODE" --volume-id "$STAGING_VOLUME"
  log "Mapping $SNAPSHOT read-only from PBS on $STAGING_NODE"
  MAPPED=true
  node_exec "$STAGING_NODE" "$REMOTE_BACKUP_LINK" map "$STAGING_VMID" "$SNAPSHOT" \
    --storage "$BACKUP_STORAGE_ID" >/dev/null ||
    die "Could not map $SNAPSHOT from PBS"
  OVERLAY_PATH="$(node_exec "$STAGING_NODE" "$REMOTE_BACKUP_LINK" create-overlay "$STAGING_VMID" "$STAGING_VOLUME")" ||
    die "Could not create the overlay $STAGING_VOLUME"
  info "Overlay: $STAGING_VOLUME ($OVERLAY_PATH)"
}

patch_overlay_offline() {
  CURRENT_PHASE="offline-patching the overlay"
  registry_update_stage --state patching
  prepare_remote_patch_bundle
  local -a args=(
    "${REMOTE_RUN_DIR}/patch_staging_clone.sh"
    --qcow2 "$OVERLAY_PATH"
    --volume-id "$STAGING_VOLUME"
    --name "$STAGING_NAME"
    --address "${STAGING_IP}/24"
    --gateway "$GUEST_GATEWAY"
    --mac "$STAGING_MAC"
    --dns "$(IFS=,; printf '%s' "${DNS_SERVERS[*]}")"
    --root-hash-file "${REMOTE_RUN_DIR}/root-password.hash"
  )
  [[ -z "$SANITIZER_FILE" ]] || args+=(--sanitizer "${REMOTE_RUN_DIR}/sanitizer.sh")
  node_exec "$STAGING_NODE" bash "${args[@]}" ||
    die "Offline overlay patching failed; attachment and start are forbidden"
  PATCH_SUCCEEDED=true
  remove_remote_patch_bundle
}

attach_overlay() {
  CURRENT_PHASE="attaching the patched overlay"
  [[ "$PATCH_SUCCEEDED" == true ]] || die "Internal guard refused to attach an unpatched overlay"
  node_exec "$STAGING_NODE" qm set "$STAGING_VMID" \
    --scsi0 "${STAGING_VOLUME},discard=on,iothread=1,ssd=1" --boot "order=scsi0" >/dev/null ||
    die "Could not attach the overlay"
  verify_backup_linked_config attached
  registry_update_stage --state stopped
}

start_backup_linked_vm() {
  COMMITTED=true
  if [[ "$START_AFTER_CREATION" != true ]]; then
    info "$STAGING_NAME remains stopped"
    return 0
  fi
  CURRENT_PHASE="starting the staging VM"
  log "Starting $STAGING_NAME"
  local start_returned=true deadline status=""
  node_exec "$STAGING_NODE" qm start "$STAGING_VMID" || start_returned=false
  deadline=$((SECONDS + START_TIMEOUT_SECONDS))
  while ((SECONDS < deadline)); do
    status="$(node_exec "$STAGING_NODE" qm status "$STAGING_VMID" 2>/dev/null)" || status=""
    [[ "$status" != "status: running" && "$start_returned" == true ]] || break
    sleep 5
  done
  if [[ "$status" != "status: running" ]]; then
    node_exec "$STAGING_NODE" qm stop "$STAGING_VMID" --skiplock true --timeout 30 >/dev/null 2>&1 || true
    warn "The lifecycle hook or Proxmox refused the start; $STAGING_NAME remains registered and stopped."
    return 0
  fi
  local updated
  if ! updated="$(registry_cmd update "$STAGING_NAME" --expected-revision "$STAGING_REVISION" --state active)"; then
    node_exec "$STAGING_NODE" qm stop "$STAGING_VMID" --skiplock true --timeout 30 >/dev/null 2>&1 || true
    warn "Registry activation failed; $STAGING_NAME was returned to stopped."
    return 0
  fi
  parse_staging_resource "$updated"
}

print_backup_linked_completion() {
  local status
  status="$(node_exec "$STAGING_NODE" qm status "$STAGING_VMID")"
  log "Backup-linked staging VM created"
  info "Resource: $STAGING_NAME (VMID $STAGING_VMID) on $STAGING_NODE"
  info "Restore point: $SNAPSHOT of $SOURCE_NAME, taken $SNAPSHOT_TIME"
  info "Overlay: $STAGING_VOLUME; the restore point's data stays on PBS"
  info "Private IP: ${STAGING_IP}/24"
  info "URL: https://${STAGING_DOMAIN}"
  info "Compute: ${STAGING_VM_CORES} vCPU / $((STAGING_VM_MEMORY_MB / 1024)) GiB RAM"
  info "NIC link-down: $NETWORK_LINK_DOWN"
  info "Status: ${status#status: }"
  warn "It is evicted when a production VM starts on $STAGING_NODE, and when its PBS mapping dies (for example when $STAGING_NODE reboots)."
  [[ -n "$SANITIZER_FILE" ]] ||
    warn "No application sanitizer was injected; review the restore point's data, credentials, and jobs before use."
  setup_workstation_jump_ssh
  bmac_ui_step_done
  bmac_ui_result name "$STAGING_NAME" vmid "$STAGING_VMID" ip "$STAGING_IP" \
    url "https://${STAGING_DOMAIN}" source "$SOURCE_NAME" node "$STAGING_NODE" \
    snapshot "$SNAPSHOT" snapshot_taken "$SNAPSHOT_TIME" status "${status#status: }"
  [[ -n "$SANITIZER_FILE" ]] ||
    bmac_ui_next_step "Review the restore point's data, credentials, and jobs in $STAGING_NAME before using it; no sanitizer was injected."
  [[ "${status#status: }" == running ]] ||
    bmac_ui_next_step "$STAGING_NAME is stopped; start it when ready with qm start $STAGING_VMID on $STAGING_NODE."
  bmac_ui_next_step "Open https://${STAGING_DOMAIN} to check the restore point."
  bmac_ui_next_step "Destroy $STAGING_NAME when you are done with it." \
    --workflow remove_staging_vm --arg "resource=$STAGING_NAME"
}

# Undo a creation that failed before it was committed: the VM (which takes
# its EFI disk and, once attached, its overlay), any overlay left behind, the
# mapping, then the registry allocation.
rollback_backup_linked_creation() {
  [[ "$ROLLBACK_RUNNING" == false ]] || return 0
  ROLLBACK_RUNNING=true
  set +e
  warn "Creation failed during: $CURRENT_PHASE"
  remove_remote_patch_bundle
  local clean=true
  if [[ "$VM_MAY_EXIST" == true ]]; then
    node_exec "$STAGING_NODE" qm stop "$STAGING_VMID" --skiplock true --timeout 30 >/dev/null 2>&1
    if node_exec "$STAGING_NODE" qm status "$STAGING_VMID" >/dev/null 2>&1; then
      node_exec "$STAGING_NODE" qm destroy "$STAGING_VMID" --purge 1 --skiplock true >/dev/null 2>&1 || clean=false
    fi
  fi
  if [[ -n "$OVERLAY_PATH" ]] && node_exec "$STAGING_NODE" test -e "$OVERLAY_PATH" 2>/dev/null; then
    node_exec "$STAGING_NODE" pvesm free "$STAGING_VOLUME" >/dev/null 2>&1 || clean=false
  fi
  if [[ "$MAPPED" == true ]]; then
    node_exec "$STAGING_NODE" "$REMOTE_BACKUP_LINK" unmap "$STAGING_VMID" >/dev/null 2>&1 || clean=false
  fi
  if [[ "$REGISTRY_RESERVED" == true ]]; then
    if [[ "$clean" == true ]] && rollback_registry && registry_cmd release "$STAGING_NAME" >/dev/null 2>&1; then
      warn "Rollback removed the VM, overlay, PBS mapping, and registry allocation."
    else
      warn "Rollback was incomplete; $STAGING_NAME stays registered. Remove it with remove_staging_vm.sh $STAGING_NAME."
    fi
  elif [[ "$clean" == true ]]; then
    warn "Nothing was left behind."
  fi
}

cleanup() {
  local code=$?
  trap - EXIT HUP INT TERM
  set +e
  set +x
  if ((code != 0)) && [[ "$COMMITTED" == false ]]; then
    rollback_backup_linked_creation
  fi
  remove_remote_patch_bundle
  [[ -z "$RUN_DIR" ]] || rm -rf -- "$RUN_DIR"
  # shellcheck disable=SC2034 # Read by prepare_remote_patch_bundle; cleared here.
  ROOT_PASSWORD_HASH=""
  unset ROOT_PASSWORD_HASH
  exit "$code"
}

main() {
  parse_args "$@"
  require_command_local openssl
  require_openssl_sha512_crypt
  load_and_validate_config
  preflight_workstation
  install_traps
  select_coordinator
  backup_storage_check "$COORDINATOR" || die "PBS backups are not available"
  choose_restore_point
  parse_backup_source
  choose_backup_linked_node
  collect_backup_linked_options
  ensure_overlay_storage
  reserve_backup_linked
  create_backup_linked_vm
  map_restore_point
  patch_overlay_offline
  attach_overlay
  start_backup_linked_vm
  print_backup_linked_completion
}

if [[ "${APP_HA_BACKUP_LINKED_SOURCE_ONLY:-0}" != 1 ]]; then
  bmac_ui_bootstrap "$@"
  main "$@"
fi
