#!/usr/bin/env bash

# Copyright (c) 2026 BEENTHERE VENTURES, INC.
# SPDX-License-Identifier: GPL-3.0-only

# Instant Restore to Existing VM: return a production VM to one of its own
# restore points on PBS, as Full Restore to Existing VM does, but down only
# until it boots again. HA stops it, its replication goes, its zvol is freed
# and sized to the restore point's disk, and it boots through HA from a qcow2
# overlay over the restore point, mapped read-only from PBS. While it runs,
# the restore point's in-use data is copied into the zvol, the running disk
# is switched onto the zvol (instant_switch.py), and replication comes back.
#
# The registry's instant_restore marker carries the restore point and how far
# the restore got (planned, freed, switched). It lets the lifecycle hook start
# the VM from its overlay, keeps prod-linked staging VMs from using it as a
# source, and lets a rerun of this script resume.

set -Eeuo pipefail
set +x
umask 077

# shellcheck source=rollback_restore.sh
ROLLBACK_RESTORE_SOURCE_ONLY=1 source "$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)/rollback_restore.sh"

# shellcheck disable=SC2034 # Read by prod_acquire_lease in scripts/lib/prod_ha.sh.
LEASE_OWNER_LABEL="instant-restore"
INSTANT_OVERLAY=""
INSTANT_PHASE=""
BACKUP_DISK_BYTES=""
REGISTERED_BYTES=""
RESUMING=false
COPY_DELAY_SECONDS=0

usage() {
  cat <<'EOF'
Usage: scripts/user_callable/backups/instant_restore_existing.sh [prodN] [--vm prodN]
         [--snapshot vm/VMID/TIME] [--cores N] [--memory-gib N]
         [--replication-minutes N] [--safety-backup | --no-safety-backup]
         [--replication-timeout-seconds N] [--copy-delay-seconds N] [--json]

Return a production VM to one of its own restore points on PBS, down only
until it boots again. As with Full Restore to Existing VM, everything written
since the restore point is lost (a safety backup is offered first), and the VM
keeps its identity and current settings, with the vCPUs and RAM you choose.

HA stops the VM, its replication jobs and replicas are deleted, its zvol is
freed and sized to the restore point's disk, and it boots through HA from a
local overlay over the restore point, mapped read-only from PBS. While it
runs, reads of data not yet copied come from PBS and are slower. Its
restore point's in-use data is copied into the zvol, the running disk is
switched onto the zvol, the zvol is grown back to its registered size, and
replication is recreated with a full initial sync. Until then the VM cannot
fail over, and it is no source for prod-linked staging VMs.

The VM must be running under HA with healthy replication, and must have no
prod-linked staging VMs. After a failure, run this again for the same VM to
resume; the restore point is the one it started from.

  --snapshot vm/VMID/TIME   The restore point (chosen from a list when omitted).
  --cores N, --memory-gib N Default: the VM's current values.
  --replication-minutes N   Minutes between replication runs of the
                            recreated jobs. Default: the current schedule.
  --safety-backup, --no-safety-backup
                            Back up the current state first (asked when omitted).
  --replication-timeout-seconds N
                            Wait for the initial sync of each replica.
                            Default: 3600.
  --copy-delay-seconds N    Developer option: keep the VM running from PBS
                            for N seconds (at most 3600) before its data is
                            copied, to try it there.
EOF
}

# Full Restore to Existing VM's options, parsed there.
eval "rollback_parse_args() $(declare -f parse_args | tail -n +2)"
parse_args() {
  local -a forwarded=()
  while (($#)); do
    case "$1" in
      --vm | --snapshot | --cores | --memory-gib | --replication-minutes | --replication-timeout-seconds)
        (($# >= 2)) || die "$1 needs a value"
        forwarded+=("$1" "$2")
        shift 2
        ;;
      --copy-delay-seconds)
        (($# >= 2)) || die "$1 needs a value"
        [[ "$2" =~ ^[0-9]{1,4}$ ]] && (($2 <= 3600)) || die "--copy-delay-seconds must be 0 through 3600"
        COPY_DELAY_SECONDS="$2"
        shift 2
        ;;
      --safety-backup | --no-safety-backup | -h | --help)
        forwarded+=("$1")
        shift
        ;;
      *)
        forwarded+=("$1")
        shift
        ;;
    esac
  done
  rollback_parse_args "${forwarded[@]}"
}

# Prod-linked staging VMs are clones of the zvol's replicas; backup-linked
# ones depend on PBS only and may stay.
refuse_prod_linked_dependents() {
  local linked
  linked="$(python3 -c '
import json, sys
print(" ".join(sorted(row["name"] for row in json.load(open(sys.argv[1]))
                      if row.get("kind") == "staging" and row.get("source") == sys.argv[2] and "backup" not in row)))' \
    "${RUN_DIR}/resources.json" "$RESOURCE_NAME")"
  [[ -z "$linked" ]] ||
    die "$RESOURCE_NAME has prod-linked staging VMs ($linked); remove them with scripts/user_callable/guests/staging/remove_staging_vm.sh first"
}

# What to do after a failure, by the registry's phase.
cleanup() {
  local code=$?
  trap - EXIT
  set +e
  if ((code != 0)); then
    printf '\nFailed while %s.\n' "$CURRENT_PHASE" >&2
    case "$INSTANT_PHASE" in
      "")
        printf 'Nothing was changed on %s.\n' "${RESOURCE_NAME:-the VM}" >&2
        ;;
      planned)
        printf '%s still has its current disks. Run this again to continue the instant restore,\n' "$RESOURCE_NAME" >&2
        printf 'or, to keep its current state instead, end the restore on a mox host and start it:\n' >&2
        printf '  %s update %s --clear-instant-restore\n  ha-manager set vm:%s --state started\n' \
          "$PROD_REMOTE_REGISTRY" "$RESOURCE_NAME" "$VMID" >&2
        printf 'Its replication may then need recreating with change_prod_vm_placement.sh.\n' >&2
        ;;
      freed | switched)
        printf "%s's zvol no longer holds its previous data. Run this again for %s to finish the instant restore.\n" \
          "$RESOURCE_NAME" "$RESOURCE_NAME" >&2
        ;;
    esac
  fi
  prod_release_lease_quietly
  exit "$code"
}

# The marker of an instant restore under way, from the registry record:
# STORAGE SNAPSHOT PHASE, or nothing.
instant_marker() {
  python3 -c '
import json, sys
restore = json.load(open(sys.argv[1])).get("instant_restore")
if restore:
    print(restore["storage"], restore["snapshot"], restore["phase"])' "${RUN_DIR}/record.json"
}

# The root disk size the registry records, in bytes.
registered_bytes() {
  python3 -c '
import json, sys
spec = json.load(open(sys.argv[1]))["spec"]
print(spec.get("disk_bytes") or int(spec["disk_gib"]) * 1024 ** 3)' "${RUN_DIR}/record.json"
}

# The restore point's VM must have one EFI disk and one scsi0 root disk, as
# BMAC creates them; BACKUP_DISK_BYTES is its root disk's size.
read_restore_point_disks() {
  local config
  config="$(pvesh_get "/nodes/${COORDINATOR}/vzdump/extractconfig" --volume "${BACKUP_STORAGE_ID}:backup/${SNAPSHOT}")" ||
    die "Could not read the VM configuration stored in $SNAPSHOT"
  BACKUP_DISK_BYTES="$(python3 - "$config" <<'PY'
import json
import re
import sys

config = {}
for line in str(json.loads(sys.argv[1])).splitlines():
    if line.startswith("["):
        break
    if ": " in line and not line.startswith("#"):
        key, value = line.split(": ", 1)
        config[key] = value
disks = sorted(key for key in config if re.fullmatch(r"(?:scsi|sata|virtio|ide|efidisk|tpmstate|unused)[0-9]+", key)
               and "media=cdrom" not in config[key])
if disks != ["efidisk0", "scsi0"]:
    raise SystemExit(f"the restore point's VM has disks {disks}, not one EFI disk and one scsi0 root disk")
match = re.search(r"(?:^|,)size=([0-9]+)([KMGT])(?:,|$)", config["scsi0"])
if not match:
    raise SystemExit("the restore point's root disk has no size")
print(int(match.group(1)) * 1024 ** "KMGT".index(match.group(2)) * 1024)
PY
  )" || die "$SNAPSHOT cannot be restored instantly"
  ((BACKUP_DISK_BYTES % (1024 * 1024) == 0)) || die "The restore point's root disk is not a whole number of MiB"
}

# Where the VM is and whether it runs, without the checks of a healthy VM:
# a resumed restore may find it stopped and without replication.
locate_resumed_vm() {
  local fields
  fields="$(pvesh_get /cluster/resources --type vm | python3 -c '
import json, sys
rows = [row for row in json.load(sys.stdin) if str(row.get("vmid")) == sys.argv[1]]
if len(rows) != 1 or rows[0].get("name") != sys.argv[2]:
    raise SystemExit(1)
print(rows[0]["node"], rows[0].get("status", ""))' "$VMID" "$RESOURCE_NAME")" ||
    die "VMID $VMID is not one VM named $RESOURCE_NAME"
  read -r OWNER_NODE LIVE_STATUS <<<"$fields"
  list_contains "$OWNER_NODE" "${PLACEMENT_NODES[@]}" || die "$RESOURCE_NAME is on $OWNER_NODE, outside its placement"
}

scsi0_volume() {
  node_exec "$OWNER_NODE" qm config "$VMID" | awk -F': ' '$1 == "scsi0" {print $2}' | cut -d, -f1
}

# The VM's replication jobs, whatever their health, into REPLICATION_JOBS.
load_replication_jobs() {
  local job target
  REPLICATION_JOBS=()
  REPLICATION_TARGET=()
  while IFS=$'\t' read -r job target; do
    [[ -n "$job" ]] || continue
    REPLICATION_JOBS+=("$job")
    REPLICATION_TARGET["$job"]="$target"
  done < <(pvesh_get /cluster/replication | python3 -c '
import json, sys
for row in json.load(sys.stdin):
    if str(row.get("guest")) == sys.argv[1]:
        print(row["id"], row.get("target", ""), sep="\t")' "$VMID")
}

require_helpers() {
  { node_exec "$OWNER_NODE" test -x "$BACKUP_LINK_HELPER" &&
    node_exec "$OWNER_NODE" test -x "$INSTANT_SWITCH_HELPER"; } ||
    die "$OWNER_NODE lacks the instant-restore helpers; run update_cluster_runtime.sh first"
}

confirm_instant() {
  local phrase="RESTORE ${RESOURCE_NAME}" answer job grow=""
  ((REGISTERED_BYTES <= BACKUP_DISK_BYTES)) ||
    grow=", then grown back to $((REGISTERED_BYTES / 1024 / 1024 / 1024)) GiB"
  log "Plan"
  info "Instantly restore $RESOURCE_NAME (VMID $VMID on $OWNER_NODE) to ${SNAPSHOT_TIME} ($SNAPSHOT)"
  info "Afterwards: ${CORES} vCPUs, ${MEMORY_GIB} GiB RAM; identity, network, tags, hook, and other settings unchanged"
  [[ "$SAFETY_BACKUP" != true ]] || info "First: a safety backup of the current state"
  info "Downtime: from the HA stop until it boots from the restore point, usually a minute or two"
  info "Then: slower reads of data not yet copied, until its zvol is filled and the disk switched${grow}"
  for job in "${REPLICATION_JOBS[@]}"; do
    info "Replication $job to ${REPLICATION_TARGET[$job]}: deleted with its replica, recreated every ${REPLICATION_MINUTES} minutes after the switch, full initial sync"
  done
  bmac_ui_plan_begin "Instant restore of $RESOURCE_NAME" "Everything written to $RESOURCE_NAME after ${SNAPSHOT_TIME} is lost."
  [[ "$SAFETY_BACKUP" != true ]] || bmac_ui_plan_item create "safety backup" "A restore point of the current state, made first"
  bmac_ui_plan_item update "$RESOURCE_NAME" "Boots from $SNAPSHOT mapped from PBS; its zvol is filled while it runs and the disk switched${grow}; ${CORES} vCPUs, ${MEMORY_GIB} GiB RAM"
  for job in "${REPLICATION_JOBS[@]}"; do
    bmac_ui_plan_item update "replication $job" "Deleted with its replica on ${REPLICATION_TARGET[$job]}, recreated after the switch every ${REPLICATION_MINUTES} minutes, synced from scratch"
  done
  bmac_ui_plan_item keep "identity" "VMID $VMID, MAC, address, tags, lifecycle hook, HA rule, tunnel FQDNs, other settings"
  bmac_ui_plan_end
  printf '\nEVERYTHING WRITTEN TO %s AFTER %s IS LOST.\n' "$RESOURCE_NAME" "$SNAPSHOT_TIME"
  if bmac_ui_is_json; then
    bmac_ui_confirm --id instant-restore --severity critical --title "Instantly restore $RESOURCE_NAME to ${SNAPSHOT_TIME}?" \
      --message "Everything written to $RESOURCE_NAME after ${SNAPSHOT_TIME} is lost. It is down until it boots from the restore point, and cannot fail over until its replication is recreated." \
      --confirm-label "Restore $RESOURCE_NAME" --text "$phrase" || die "Cancelled; nothing was changed"
  else
    read -r -p "Type exactly '$phrase' to continue: " answer
    [[ "$answer" == "$phrase" ]] || die "Cancelled; nothing was changed"
  fi
}

set_phase() {
  registry_update_resource --instant-restore-phase "$1"
  INSTANT_PHASE="$1"
}

# With the VM stopped and phase freed: map the restore point, write its EFI
# variables, free the zvol and size it to the restore point's disk, create the
# overlay, and point scsi0 at it. Nothing here is undone by a rerun: the VM
# has never booted from an overlay that is replaced.
prepare_overlay() {
  CURRENT_PHASE="preparing ${RESOURCE_NAME}'s overlay over $SNAPSHOT"
  log "Mapping $SNAPSHOT read-only from PBS on $OWNER_NODE"
  node_exec "$OWNER_NODE" "$BACKUP_LINK_HELPER" map "$VMID" "$SNAPSHOT" --storage "$BACKUP_STORAGE_ID" >/dev/null ||
    die "Could not map $SNAPSHOT from PBS"
  local efi_volume efi_path path dataset overlay_path
  efi_volume="$(node_exec "$OWNER_NODE" qm config "$VMID" | awk -F': ' '$1 == "efidisk0" {print $2}' | cut -d, -f1)"
  efi_path="$(node_exec "$OWNER_NODE" pvesm path "$efi_volume")" || die "Could not resolve the EFI disk $efi_volume"
  node_exec "$OWNER_NODE" "$BACKUP_LINK_HELPER" restore "$VMID" drive-efidisk0.img.fidx "$efi_path" ||
    die "Could not restore the EFI variables of $SNAPSHOT"

  path="$(node_exec "$OWNER_NODE" pvesm path "$ROOT_VOLUME")" || die "Could not resolve $ROOT_VOLUME"
  [[ "$path" =~ ^/dev/zvol/[A-Za-z0-9._/+:-]+$ ]] || die "Unexpected path for $ROOT_VOLUME: $path"
  dataset="${path#/dev/zvol/}"
  log "Freeing $ROOT_VOLUME and sizing it to the restore point's $((BACKUP_DISK_BYTES / 1024 / 1024 / 1024)) GiB"
  node_exec "$OWNER_NODE" blkdiscard -f "$path" 2>"${RUN_DIR}/blkdiscard.err" ||
    die "Could not free $ROOT_VOLUME: $(tr '\n' ' ' <"${RUN_DIR}/blkdiscard.err")"
  node_exec "$OWNER_NODE" bash -c 'zfs set "volsize=$2" "$1" && udevadm settle' bash "$dataset" "$BACKUP_DISK_BYTES" ||
    die "Could not size $ROOT_VOLUME to $BACKUP_DISK_BYTES bytes"

  overlay_path="$(node_exec "$OWNER_NODE" pvesm path "$INSTANT_OVERLAY" 2>/dev/null)" || overlay_path=""
  if [[ -n "$overlay_path" ]] && node_exec "$OWNER_NODE" test -e "$overlay_path"; then
    node_exec "$OWNER_NODE" pvesm free "$INSTANT_OVERLAY" >/dev/null || die "Could not remove the earlier overlay"
  fi
  node_exec "$OWNER_NODE" "$BACKUP_LINK_HELPER" create-overlay "$VMID" "$INSTANT_OVERLAY" >/dev/null ||
    die "Could not create the overlay $INSTANT_OVERLAY"
  node_exec "$OWNER_NODE" python3 "$INSTANT_SWITCH_HELPER" attach "$VMID" "$INSTANT_OVERLAY" "$ROOT_VOLUME" >/dev/null ||
    die "Could not attach the overlay"
  node_exec "$OWNER_NODE" qm set "$VMID" --cores "$CORES" --memory $((MEMORY_GIB * 1024)) >/dev/null ||
    die "Could not set ${RESOURCE_NAME}'s vCPUs and RAM"
}

# A dead mapping (its process exited, or the host rebooted) fails the VM's
# reads: stop it through HA and map the same restore point again.
ensure_mapping() {
  local state
  state="$(node_exec "$OWNER_NODE" "$BACKUP_LINK_HELPER" status "$VMID")" ||
    die "Could not read the PBS mapping of $RESOURCE_NAME"
  [[ "$state" != alive ]] || return 0
  warn "The PBS mapping of $RESOURCE_NAME is $state; mapping its restore point again"
  if [[ "$(vm_status 2>/dev/null)" != stopped ]]; then
    stop_vm
  fi
  node_exec "$OWNER_NODE" "$BACKUP_LINK_HELPER" map "$VMID" "$SNAPSHOT" --storage "$BACKUP_STORAGE_ID" >/dev/null ||
    die "Could not map $SNAPSHOT again"
}

# With the VM running from its overlay: copy the restore point's in-use data
# into the zvol and switch the running disk onto it.
copy_and_switch() {
  local state path started
  CURRENT_PHASE="copying $SNAPSHOT into ${RESOURCE_NAME}'s zvol"
  state="$(node_exec "$OWNER_NODE" python3 "$INSTANT_SWITCH_HELPER" state "$VMID" "$INSTANT_OVERLAY" "$ROOT_VOLUME")" ||
    die "Could not read the disk state of $RESOURCE_NAME"
  if [[ "$state" == backup ]]; then
    path="$(node_exec "$OWNER_NODE" pvesm path "$ROOT_VOLUME")" || die "Could not resolve $ROOT_VOLUME"
    if ((COPY_DELAY_SECONDS > 0)); then
      instant_copy_delay "$COPY_DELAY_SECONDS" "$RESOURCE_NAME"
      [[ "$(node_exec "$OWNER_NODE" "$BACKUP_LINK_HELPER" status "$VMID")" == alive ]] ||
        die "The PBS mapping of $RESOURCE_NAME died during the wait; run this again to map it again"
    fi
    log "Copying the in-use data of $SNAPSHOT into $ROOT_VOLUME while $RESOURCE_NAME runs"
    # --skip-zero leaves zero chunks unwritten, so the zvol must read as
    # zeros first; it is not in use until the switch.
    node_exec "$OWNER_NODE" blkdiscard -f "$path" 2>"${RUN_DIR}/blkdiscard.err" ||
      die "Could not free $ROOT_VOLUME before the copy: $(tr '\n' ' ' <"${RUN_DIR}/blkdiscard.err")"
    started=$SECONDS
    node_exec "$OWNER_NODE" "$BACKUP_LINK_HELPER" restore "$VMID" drive-scsi0.img.fidx "$path" --skip-zero ||
      die "The copy of $SNAPSHOT failed; $RESOURCE_NAME keeps running on its overlay"
    info "Copied in $((SECONDS - started))s"
  fi
  CURRENT_PHASE="switching ${RESOURCE_NAME}'s running disk to $ROOT_VOLUME"
  log "Switching ${RESOURCE_NAME}'s running disk to $ROOT_VOLUME"
  node_exec "$OWNER_NODE" python3 "$INSTANT_SWITCH_HELPER" switch "$VMID" "$INSTANT_OVERLAY" "$ROOT_VOLUME" >/dev/null ||
    die "The switch did not complete; $RESOURCE_NAME keeps running on its overlay"
  set_phase switched
  info "$RESOURCE_NAME now runs on $ROOT_VOLUME alone"
}

# After the switch: drop the mapping, its record, and the overlay; grow the
# zvol back to the registered size; recreate replication; end the restore.
finish() {
  CURRENT_PHASE="removing the overlay and PBS mapping"
  local path job target index schedule="*/${REPLICATION_MINUTES}" existing
  { node_exec "$OWNER_NODE" "$BACKUP_LINK_HELPER" unmap "$VMID" >/dev/null &&
    node_exec "$OWNER_NODE" "$BACKUP_LINK_HELPER" forget "$VMID" >/dev/null; } ||
    die "Could not remove the PBS mapping of $RESOURCE_NAME"
  path="$(node_exec "$OWNER_NODE" pvesm path "$INSTANT_OVERLAY" 2>/dev/null)" || path=""
  if [[ -n "$path" ]] && node_exec "$OWNER_NODE" test -e "$path"; then
    node_exec "$OWNER_NODE" pvesm free "$INSTANT_OVERLAY" >/dev/null || die "Could not remove the overlay $INSTANT_OVERLAY"
  fi

  CURRENT_PHASE="sizing ${RESOURCE_NAME}'s root disk"
  local live
  live="$(disk_bytes "$(node_exec "$OWNER_NODE" qm config "$VMID" | awk -F': ' '$1 == "scsi0" {print $2}')")" ||
    die "Cannot read ${RESOURCE_NAME}'s disk size"
  if ((live < REGISTERED_BYTES)); then
    node_exec "$OWNER_NODE" qm resize "$VMID" scsi0 "$REGISTERED_BYTES" >/dev/null ||
      die "Could not grow ${RESOURCE_NAME}'s disk back to $REGISTERED_BYTES bytes"
    info "Disk grown back to $((REGISTERED_BYTES / 1024 / 1024 / 1024)) GiB (the guest's filesystem keeps the restore point's size)"
  elif ((live > REGISTERED_BYTES)); then
    registry_update_resource --disk-bytes "$live"
    REGISTERED_BYTES="$live"
  fi

  load_replication_jobs
  SAVED_JOBS=()
  SAVED_TARGET=()
  for target in "${PLACEMENT_NODES[@]}"; do
    [[ "$target" != "$OWNER_NODE" ]] || continue
    job=""
    for existing in "${REPLICATION_JOBS[@]}"; do
      [[ "${REPLICATION_TARGET[$existing]}" != "$target" ]] || job="$existing"
    done
    if [[ -z "$job" ]]; then
      index=0
      while list_contains "${VMID}-${index}" "${REPLICATION_JOBS[@]}" "${SAVED_JOBS[@]}"; do
        index=$((index + 1))
      done
      job="${VMID}-${index}"
      CURRENT_PHASE="recreating replication $job to $target"
      log "Recreating replication $job to $target ($schedule)"
      node_exec "$OWNER_NODE" pvesr create-local-job "$job" "$target" --schedule "$schedule" \
        --comment "Production ${RESOURCE_NAME} replica" || die "Could not recreate replication job $job"
    fi
    SAVED_JOBS+=("$job")
    SAVED_TARGET["$job"]="$target"
    wait_for_replication "$OWNER_NODE" "$job" "$target" 0 "$REPLICATION_TIMEOUT_SECONDS"
  done
  registry_update_resource --owner-node "$OWNER_NODE" \
    --replication-targets "$(join_csv "${SAVED_TARGET[@]}")" --clear-instant-restore
  INSTANT_PHASE=""
}

# The VM, its compute, and the restore point; then, for a new restore, the
# confirmation and the steps that keep its data until the zvol is freed.
start_new_restore() {
  prod_load_live_state
  prod_reconcile_registry_owner
  prod_show_live_state
  prod_require_replication_healthy "$OWNER_NODE"
  choose_snapshot
  read_live_config
  read_restore_point_disks
  choose_compute
  choose_safety_backup
  confirm_instant
  require_helpers

  prod_acquire_lease
  safety_backup
  prod_require_unchanged
  ensure_overlay_storage
  CURRENT_PHASE="recording the instant restore"
  registry_update_resource --cores "$CORES" --memory-mb $((MEMORY_GIB * 1024)) \
    --replication-interval "$REPLICATION_MINUTES" \
    --instant-restore-storage "$BACKUP_STORAGE_ID" --instant-restore-snapshot "$SNAPSHOT"
  INSTANT_PHASE=planned
}

resume_restore() {
  local storage snapshot phase
  read -r storage snapshot phase <<<"$1"
  [[ "$storage" == "$BACKUP_STORAGE_ID" ]] || die "$RESOURCE_NAME's instant restore uses storage $storage, not $BACKUP_STORAGE_ID"
  [[ -z "$SNAPSHOT" || "$SNAPSHOT" == "$snapshot" ]] ||
    die "$RESOURCE_NAME's instant restore under way is from $snapshot, not $SNAPSHOT"
  SNAPSHOT="$snapshot"
  SNAPSHOT_TIME="$snapshot"
  INSTANT_PHASE="$phase"
  RESUMING=true
  locate_resumed_vm
  info "Resuming the instant restore of $RESOURCE_NAME from $SNAPSHOT (phase $phase, VM $LIVE_STATUS on $OWNER_NODE)"
  CORES="$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["spec"]["cores"])' "${RUN_DIR}/record.json")"
  MEMORY_GIB=$(($(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["spec"]["memory_mb"])' "${RUN_DIR}/record.json") / 1024))
  REPLICATION_MINUTES="$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["spec"]["replication_interval"])' "${RUN_DIR}/record.json")"
  read_restore_point_disks
  require_helpers
  if bmac_ui_is_json; then
    bmac_ui_ask "Resume the instant restore of $RESOURCE_NAME from $SNAPSHOT?" "Resume" "Cancel" ||
      die "Cancelled; the instant restore stays where it was"
  else
    prompt_yes "Resume the instant restore of $RESOURCE_NAME from $SNAPSHOT?" ||
      die "Cancelled; the instant restore stays where it was"
  fi
  prod_acquire_lease
}

main() {
  parse_args "$@"
  prod_load_config
  prod_preflight_workstation instant-restore
  trap cleanup EXIT
  SNAPSHOT_TIME=""
  SAFETY_VOLID=""

  prod_discover_cluster
  backup_storage_check "$COORDINATOR" || die "PBS backups are not available"
  if [[ -n "$REQUESTED_VM" ]]; then
    parse_selected_production "$REQUESTED_VM" || die "$REQUESTED_VM is not an active production VM"
  else
    prod_select_production "Production VM to restore"
  fi
  registry_cmd get "$RESOURCE_NAME" >"${RUN_DIR}/record.json" || die "Could not read $RESOURCE_NAME's registry record"
  refuse_prod_linked_dependents
  INSTANT_OVERLAY="${OVERLAY_STORAGE_ID}:${VMID}/vm-${VMID}-disk-1.qcow2"
  REGISTERED_BYTES="$(registered_bytes)"

  local marker
  marker="$(instant_marker)"
  if [[ -n "$marker" ]]; then
    resume_restore "$marker"
  else
    start_new_restore
  fi

  if [[ "$INSTANT_PHASE" == planned ]]; then
    load_replication_jobs
    [[ "$(vm_status 2>/dev/null)" == stopped ]] || stop_vm
    remove_replication
    # From here the zvol's data is gone; the hook boots the VM only from its
    # overlay.
    set_phase freed
  fi
  if [[ "$INSTANT_PHASE" == freed ]]; then
    if [[ "$(scsi0_volume)" == "$ROOT_VOLUME" ]]; then
      [[ "$(vm_status 2>/dev/null)" == stopped ]] ||
        die "$RESOURCE_NAME runs from its freed zvol; stop it with 'ha-manager set vm:$VMID --state stopped' and run this again"
      prepare_overlay
    else
      ensure_mapping
    fi
    [[ "$(vm_status 2>/dev/null)" == running ]] || start_vm
    copy_and_switch
  fi
  finish
  prod_release_lease

  log "$RESOURCE_NAME is restored to ${SNAPSHOT_TIME}"
  info "It runs on $OWNER_NODE with ${CORES} vCPUs and ${MEMORY_GIB} GiB RAM; replication every ${REPLICATION_MINUTES} minutes is healthy again"
  [[ -z "$SAFETY_VOLID" ]] || info "The state before the restore is restore point ${SAFETY_VOLID#*:backup/}"
  bmac_ui_result vm "$RESOURCE_NAME" vmid:int "$VMID" snapshot "$SNAPSHOT" node "$OWNER_NODE" \
    cores:int "$CORES" memory_gib:int "$MEMORY_GIB" safety_backup "${SAFETY_VOLID#*:backup/}" resumed:bool "$RESUMING"
}

if [[ "${INSTANT_RESTORE_EXISTING_SOURCE_ONLY:-0}" != 1 ]]; then
  bmac_ui_bootstrap "$@"
  main "$@"
fi
