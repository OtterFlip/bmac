#!/usr/bin/env bash

# Copyright (c) 2026 BEENTHERE VENTURES, INC.
# SPDX-License-Identifier: GPL-3.0-only

# Full Restore to Existing VM: return a production VM to one of its own
# restore points on PBS. The VM keeps its identity (VMID, name, MAC, address,
# tags, lifecycle hook), only its disks and settings go back to the restore
# point. Proxmox cannot replicate over the replicas of a restored disk, so
# replication is removed before the restore and recreated after it, with a
# full initial sync.

set -Eeuo pipefail
set +x
umask 077

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
REPO_ROOT="$(cd -- "${SCRIPT_DIR}/../../.." && pwd -P)"
# shellcheck source=../../lib/ui_protocol.sh
source "${REPO_ROOT}/scripts/lib/ui_protocol.sh"
# shellcheck source=../../lib/config.sh
source "${REPO_ROOT}/scripts/lib/config.sh"
# shellcheck source=../../lib/prod_ha.sh
source "${REPO_ROOT}/scripts/lib/prod_ha.sh"
# shellcheck source=../../lib/backups.sh
source "${REPO_ROOT}/scripts/lib/backups.sh"
# shellcheck source=../../lib/monitoring.sh
source "${REPO_ROOT}/scripts/lib/monitoring.sh"

# shellcheck disable=SC2034 # Read by prod_acquire_lease in scripts/lib/prod_ha.sh.
LEASE_OWNER_LABEL="rollback-restore"
SNAPSHOT=""
CORES=""
MEMORY_GIB=""
REPLICATION_MINUTES=""
SAFETY_BACKUP=""
REPLICATION_TIMEOUT_SECONDS=3600
STOP_TIMEOUT_SECONDS=600
JOB_REMOVAL_TIMEOUT_SECONDS=600
POLL_SECONDS="${ROLLBACK_POLL_SECONDS:-5}"
REQUESTED_VM=""
# How far the rollback got, for the recovery advice after a failure:
# planned, stopped (HA stopped the VM), unreplicated, restored, started.
STAGE=planned
declare -A LIVE=()
declare -A SAVED_TARGET=()
declare -a SAVED_JOBS=()

usage() {
  cat <<'EOF'
Usage: scripts/user_callable/backups/rollback_restore.sh [prodN] [--vm prodN]
         [--snapshot vm/VMID/TIME] [--cores N] [--memory-gib N]
         [--replication-minutes N] [--safety-backup | --no-safety-backup]
         [--replication-timeout-seconds N] [--json]

Return a production VM to one of its own restore points on PBS. Its disks and
settings go back to that point in time; the VM keeps its identity (VMID,
name, MAC, address, tags, and lifecycle hook) and its vCPUs and RAM unless
you choose others. Everything written to the VM since the restore point is
lost, so a safety backup of the current state is offered first.

The rollback stops the VM through HA, deletes its replication jobs and their
replicas (Proxmox cannot replicate over a restored disk), restores the disks
on the VM's host, starts it again through HA, and recreates replication with
a full initial sync. Until that sync finishes the VM cannot fail over.

The VM must be running under HA with healthy placement, and must have no
staging VMs (they are clones of its replicas).

  --snapshot vm/VMID/TIME   The restore point (chosen from a list when omitted).
  --cores N, --memory-gib N Default: the VM's current values.
  --replication-minutes N   Minutes between replication runs of the
                            recreated jobs. Default: the current schedule.
  --safety-backup, --no-safety-backup
                            Back up the current state first (asked when omitted).
  --replication-timeout-seconds N
                            Wait for the initial sync of each replica.
                            Default: 3600.
EOF
}

parse_args() {
  while (($#)); do
    case "$1" in
      --safety-backup) SAFETY_BACKUP=true ;;
      --no-safety-backup) SAFETY_BACKUP=false ;;
      --vm | --snapshot | --cores | --memory-gib | --replication-minutes | --replication-timeout-seconds)
        (($# >= 2)) || die "$1 needs a value"
        case "$1" in
          --vm) REQUESTED_VM="$2" ;;
          --snapshot) SNAPSHOT="$2" ;;
          --cores) CORES="$2" ;;
          --memory-gib) MEMORY_GIB="$2" ;;
          --replication-minutes) REPLICATION_MINUTES="$2" ;;
          --replication-timeout-seconds) REPLICATION_TIMEOUT_SECONDS="$2" ;;
        esac
        shift
        ;;
      -h | --help)
        usage
        exit 0
        ;;
      prod[1-9]*) REQUESTED_VM="$1" ;;
      *)
        usage >&2
        exit 2
        ;;
    esac
    shift
  done
  [[ -z "$REQUESTED_VM" || "$REQUESTED_VM" =~ ^prod[1-9][0-9]*$ ]] || die "A production VM name such as prod1 is required"
  [[ -z "$SNAPSHOT" || "$SNAPSHOT" =~ ^vm/[1-9][0-9]{0,8}/[0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9]{2}:[0-9]{2}:[0-9]{2}Z$ ]] ||
    die "--snapshot must look like vm/100/2026-10-27T14:50:01Z"
  [[ -z "$CORES" || "$CORES" =~ ^[1-9][0-9]{0,3}$ ]] || die "--cores must be a positive whole number"
  [[ -z "$MEMORY_GIB" || "$MEMORY_GIB" =~ ^[1-9][0-9]{0,5}$ ]] || die "--memory-gib must be a positive whole number"
  [[ -z "$REPLICATION_MINUTES" || "$REPLICATION_MINUTES" =~ ^[1-9][0-9]{0,3}$ ]] ||
    die "--replication-minutes must be a positive whole number"
  [[ "$REPLICATION_TIMEOUT_SECONDS" =~ ^[1-9][0-9]*$ ]] || die "--replication-timeout-seconds must be a positive whole number"
}

# What to do after a failure, by how far the rollback got.
cleanup() {
  local code=$? job
  trap - EXIT
  set +e
  if ((code != 0)); then
    printf '\nFailed while %s.\n' "$CURRENT_PHASE" >&2
    case "$STAGE" in
      planned)
        printf 'Nothing was changed on %s.\n' "${RESOURCE_NAME:-the VM}" >&2
        ;;
      stopped | unreplicated)
        printf '%s is stopped and still has its current disks; nothing was restored.\n' "$RESOURCE_NAME" >&2
        printf 'Start it again with: ha-manager set vm:%s --state started\n' "$VMID" >&2
        ;;
      restored)
        printf '%s holds the restored disks but is stopped. Check its settings (qm config %s on %s),\n' \
          "$RESOURCE_NAME" "$VMID" "$OWNER_NODE" >&2
        printf 'then start it with: ha-manager set vm:%s --state started\n' "$VMID" >&2
        ;;
      started)
        printf '%s runs the restored state; only its replication is unfinished.\n' "$RESOURCE_NAME" >&2
        ;;
    esac
    if [[ "$STAGE" != planned ]]; then
      for job in "${SAVED_JOBS[@]}"; do
        printf 'Replication job %s to %s: recreate it with change_prod_vm_placement.sh if it is missing\n' \
          "$job" "${SAVED_TARGET[$job]}" >&2
      done
    fi
  fi
  prod_release_lease_quietly
  exit "$code"
}

# BMAC's restore points of this VM, newest first, as snapshot, protected,
# and time separated by \x1f. The time is the one in the snapshot's name (in
# UTC, when PBS took it), shown in this workstation's time zone.
list_restore_points() {
  pvesh_get "/nodes/${COORDINATOR}/storage/${BACKUP_STORAGE_ID}/content" --vmid "$VMID" | python3 -c '
import calendar, json, sys, time
prefix, name = sys.argv[1] + ":backup/", sys.argv[2]
rows = []
for row in json.load(sys.stdin):
    words = str(row.get("notes") or "").split()
    if str(row.get("volid", "")).startswith(prefix) and words[:2] == ["bmac", name]:
        rows.append(row)
for row in sorted(rows, key=lambda r: r["volid"], reverse=True):
    snapshot = row["volid"][len(prefix):]
    taken = calendar.timegm(time.strptime(snapshot.rsplit("/", 1)[-1], "%Y-%m-%dT%H:%M:%SZ"))
    when = time.strftime("%Y-%m-%d %H:%M %Z", time.localtime(taken))
    comment = str(row.get("notes") or "").splitlines()[0].partition(" | ")[2].strip()
    if comment:
        when += " · " + (comment if len(comment) <= 48 else comment[:47] + "…")
    print("\x1f".join([snapshot, "1" if row.get("protected") else "0", when]))' \
    "$BACKUP_STORAGE_ID" "$RESOURCE_NAME"
}

choose_snapshot() {
  local -a points=() options=()
  local row snapshot protected when found=false
  mapfile -t points < <(list_restore_points)
  ((${#points[@]})) || die "$RESOURCE_NAME has no BMAC restore points on ${BACKUP_STORAGE_ID}"
  if [[ -z "$SNAPSHOT" ]]; then
    for row in "${points[@]}"; do
      IFS=$'\x1f' read -r snapshot protected when <<<"$row"
      options+=("$snapshot" "${when}$([[ "$protected" == 1 ]] && printf ' · protected' || true)")
    done
    if bmac_ui_is_json; then
      bmac_ui_choose SNAPSHOT "Restore point of $RESOURCE_NAME" "${options[0]}" "${options[@]}"
    else
      printf '\nRestore points of %s, newest first:\n' "$RESOURCE_NAME"
      for row in "${points[@]}"; do
        IFS=$'\x1f' read -r snapshot protected when <<<"$row"
        printf '  %s  %s%s\n' "$snapshot" "$when" "$([[ "$protected" == 1 ]] && printf '  protected' || true)"
      done
      read -r -p "Restore point [${options[0]}]: " SNAPSHOT
      SNAPSHOT="${SNAPSHOT:-${options[0]}}"
    fi
  fi
  for row in "${points[@]}"; do
    IFS=$'\x1f' read -r snapshot protected when <<<"$row"
    if [[ "$snapshot" == "$SNAPSHOT" ]]; then
      found=true
      # The time alone; the list shows the comment after it.
      SNAPSHOT_TIME="${when%% · *}"
    fi
  done
  [[ "$found" == true ]] || die "$SNAPSHOT is not a BMAC restore point of $RESOURCE_NAME (VMID $VMID)"
}

# The live config keys the rollback keeps, into LIVE[].
read_live_config() {
  local key value
  node_exec "$OWNER_NODE" qm config "$VMID" >"${RUN_DIR}/live-config.txt" ||
    die "Could not read the configuration of $RESOURCE_NAME"
  while IFS= read -r line; do
    key="${line%%: *}"
    value="${line#*: }"
    case "$key" in
      name | cores | memory | net0 | tags | hookscript | onboot | description | scsi0 | lock) LIVE["$key"]="$value" ;;
    esac
  done <"${RUN_DIR}/live-config.txt"
  local required
  for required in name cores memory net0 tags hookscript scsi0; do
    [[ -n "${LIVE[$required]:-}" ]] || die "$RESOURCE_NAME's configuration has no $required"
  done
  [[ "${LIVE[name]}" == "$RESOURCE_NAME" ]] || die "VMID $VMID is named ${LIVE[name]}, not $RESOURCE_NAME"
  # A backup in progress holds the lock "backup"; the restore would then fail
  # after the VM was already stopped.
  [[ -z "${LIVE[lock]:-}" ]] ||
    die "$RESOURCE_NAME is locked (${LIVE[lock]}), for example by a running backup; wait for it to finish and try again"
  [[ "${LIVE[scsi0]%%,*}" == "$ROOT_VOLUME" ]] ||
    die "$RESOURCE_NAME's scsi0 (${LIVE[scsi0]%%,*}) differs from the registry ($ROOT_VOLUME)"
}

# Bytes of a Proxmox disk size such as 1T or 512G (from size=...).
disk_bytes() {
  python3 -c '
import re, sys
match = re.search(r"(?:^|,)size=([0-9.]+)([KMGT]?)", sys.argv[1])
if not match:
    sys.exit(1)
print(int(float(match.group(1)) * 1024 ** "BKMGT".index(match.group(2) or "B")))' "$1"
}

# The minutes of a "*/N" replication schedule, or nothing.
schedule_minutes() {
  [[ "$1" =~ ^\*/([1-9][0-9]*)$ ]] && printf '%s\n' "${BASH_REMATCH[1]}" || true
}

choose_compute() {
  local cores="${LIVE[cores]}" memory_gib=$((LIVE[memory] / 1024)) minutes
  minutes="$(schedule_minutes "${REPLICATION_SCHEDULE:-}")"
  minutes="${minutes:-${PROD_VM_REPLICATION_INTERVAL##*/}}"
  if [[ -z "$CORES$MEMORY_GIB$REPLICATION_MINUTES" ]] && bmac_ui_is_json; then
    bmac_ui_group_begin compute "vCPUs, RAM, and replication after the rollback" \
      "The restore point's own vCPUs and RAM are replaced by these; the defaults are $RESOURCE_NAME's current settings."
    bmac_ui_group_add CORES --id cores --type integer --label "vCPUs" --default "$cores" --min 1 --max 1024
    bmac_ui_group_add MEMORY_GIB --id memory_gib --type integer --label "RAM (GiB)" --default "$memory_gib" --min 1 --max 100000
    if ((${#REPLICATION_JOBS[@]})); then
      bmac_ui_group_add REPLICATION_MINUTES --id replication_minutes --type integer \
        --label "Replication interval" --suffix minutes --default "$minutes" --min 1 --max 1440
    fi
    bmac_ui_group_request
  elif [[ -z "$CORES$MEMORY_GIB$REPLICATION_MINUTES" ]]; then
    read -r -p "vCPUs after the rollback [$cores]: " CORES
    read -r -p "RAM in GiB after the rollback [$memory_gib]: " MEMORY_GIB
    if ((${#REPLICATION_JOBS[@]})); then
      read -r -p "Replication interval in minutes [$minutes]: " REPLICATION_MINUTES
    fi
  fi
  CORES="${CORES:-$cores}"
  MEMORY_GIB="${MEMORY_GIB:-$memory_gib}"
  REPLICATION_MINUTES="${REPLICATION_MINUTES:-$minutes}"
  [[ "$CORES" =~ ^[1-9][0-9]{0,3}$ && "$MEMORY_GIB" =~ ^[1-9][0-9]{0,5}$ &&
    "$REPLICATION_MINUTES" =~ ^[1-9][0-9]{0,3}$ ]] ||
    die "vCPUs, RAM, and the replication interval must be positive whole numbers"
}

choose_safety_backup() {
  [[ -z "$SAFETY_BACKUP" ]] || return 0
  if bmac_ui_is_json; then
    bmac_ui_yes_no "Back up $RESOURCE_NAME's current state first?" true \
      "Recommended: the rollback discards everything written since the restore point. The safety backup counts toward the PBS prune job's keep-last." &&
      SAFETY_BACKUP=true || SAFETY_BACKUP=false
  else
    local answer
    read -r -p "Back up $RESOURCE_NAME's current state first? [Y/n] " answer
    [[ "$answer" =~ ^[Nn] ]] && SAFETY_BACKUP=false || SAFETY_BACKUP=true
  fi
}

confirm_rollback() {
  local phrase="ROLLBACK ${RESOURCE_NAME}" answer job
  log "Plan"
  info "Restore $RESOURCE_NAME (VMID $VMID on $OWNER_NODE) to ${SNAPSHOT_TIME} ($SNAPSHOT)"
  info "Afterwards: ${CORES} vCPUs, ${MEMORY_GIB} GiB RAM; identity, network, tags, and hook unchanged"
  [[ "$SAFETY_BACKUP" != true ]] || info "First: a safety backup of the current state"
  info "Downtime: from the HA stop until the VM has started again"
  for job in "${REPLICATION_JOBS[@]}"; do
    info "Replication $job to ${REPLICATION_TARGET[$job]}: deleted with its replica, recreated every ${REPLICATION_MINUTES} minutes, full initial sync"
  done
  bmac_ui_plan_begin "Full restore of $RESOURCE_NAME" "Everything written to $RESOURCE_NAME after ${SNAPSHOT_TIME} is lost."
  [[ "$SAFETY_BACKUP" != true ]] || bmac_ui_plan_item create "safety backup" "A restore point of the current state, made first"
  bmac_ui_plan_item update "$RESOURCE_NAME" "Disks and settings restored from $SNAPSHOT; ${CORES} vCPUs, ${MEMORY_GIB} GiB RAM"
  for job in "${REPLICATION_JOBS[@]}"; do
    bmac_ui_plan_item update "replication $job" "Deleted with its replica on ${REPLICATION_TARGET[$job]}, recreated every ${REPLICATION_MINUTES} minutes, synced from scratch"
  done
  bmac_ui_plan_item keep "identity" "VMID $VMID, MAC, address, tags, lifecycle hook, HA rule, tunnel FQDNs"
  bmac_ui_plan_end
  printf '\nEVERYTHING WRITTEN TO %s AFTER %s IS LOST.\n' "$RESOURCE_NAME" "$SNAPSHOT_TIME"
  if bmac_ui_is_json; then
    bmac_ui_confirm --id rollback --severity critical --title "Roll back $RESOURCE_NAME to ${SNAPSHOT_TIME}?" \
      --message "Everything written to $RESOURCE_NAME after ${SNAPSHOT_TIME} is lost, and it is down until it starts again." \
      --confirm-label "Roll back $RESOURCE_NAME" --text "$phrase" || die "Cancelled; nothing was changed"
  else
    read -r -p "Type exactly '$phrase' to continue: " answer
    [[ "$answer" == "$phrase" ]] || die "Cancelled; nothing was changed"
  fi
}

safety_backup() {
  [[ "$SAFETY_BACKUP" == true ]] || return 0
  CURRENT_PHASE="making the safety backup"
  log "Backing up $RESOURCE_NAME's current state"
  local purpose fqdn upid started volid
  purpose="$(python3 -c 'import json,sys; print((json.load(open(sys.argv[1])).get("purpose") or {}).get("slug") or "")' "${RUN_DIR}/record.json")"
  fqdn="$(python3 -c 'import json,sys; print((json.load(open(sys.argv[1])).get("domains") or {}).get("primary") or "")' "${RUN_DIR}/record.json")"
  started="$(date +%s)"
  upid="$(backup_start_task "$OWNER_NODE" "$VMID" "$RESOURCE_NAME" "$purpose" "$fqdn" "$RUN_DIR" \
    "Before rollback to ${SNAPSHOT_TIME}")" ||
    die "PVE did not start the safety backup: $(tr '\n' ' ' <"${RUN_DIR}/start.err")"
  pve_task_follow "$OWNER_NODE" "$upid" "$RUN_DIR" "$POLL_SECONDS" ||
    die "The safety backup failed (${PVE_TASK_EXITSTATUS:-unknown status}); nothing was rolled back"
  volid="$(backup_newest_volid "$COORDINATOR" "$VMID" "$started")" || volid=""
  SAFETY_VOLID="$volid"
  info "Safety backup: ${volid:-made (not listed yet)}"
}

vm_status() {
  pvesh_get /cluster/resources --type vm | python3 -c '
import json, sys
for row in json.load(sys.stdin):
    if str(row.get("vmid")) == sys.argv[1]:
        print(row.get("status") or "")' "$VMID"
}

wait_for_status() {
  local wanted="$1" timeout="$2" deadline status
  deadline=$((SECONDS + timeout))
  while ((SECONDS < deadline)); do
    status="$(vm_status 2>/dev/null)" || status=""
    [[ "$status" != "$wanted" ]] || return 0
    sleep "$POLL_SECONDS"
  done
  return 1
}

stop_vm() {
  CURRENT_PHASE="stopping $RESOURCE_NAME through HA"
  log "Stopping $RESOURCE_NAME through HA"
  node_exec "$COORDINATOR" ha-manager set "vm:${VMID}" --state stopped ||
    die "HA did not accept the stop request"
  STAGE=stopped
  wait_for_status stopped "$STOP_TIMEOUT_SECONDS" ||
    die "$RESOURCE_NAME did not stop within ${STOP_TIMEOUT_SECONDS}s"
  info "Stopped"
}

remove_replication() {
  local job deadline remaining node volumes
  ((${#REPLICATION_JOBS[@]})) || return 0
  CURRENT_PHASE="removing replication and its replicas"
  for job in "${REPLICATION_JOBS[@]}"; do
    log "Deleting replication job $job and its replica on ${REPLICATION_TARGET[$job]}"
    node_exec "$COORDINATOR" pvesr delete "$job" || die "Could not delete replication job $job"
  done
  STAGE=unreplicated
  deadline=$((SECONDS + JOB_REMOVAL_TIMEOUT_SECONDS))
  while :; do
    remaining="$(pvesh_get /cluster/replication | python3 -c '
import json, sys
jobs = set(sys.argv[1:])
print(" ".join(sorted(row["id"] for row in json.load(sys.stdin) if row.get("id") in jobs)))' "${REPLICATION_JOBS[@]}")" ||
      remaining=unknown
    [[ -n "$remaining" ]] || break
    ((SECONDS < deadline)) || die "Replication jobs were not removed in time: $remaining"
    sleep "$POLL_SECONDS"
  done
  for job in "${REPLICATION_JOBS[@]}"; do
    node="${REPLICATION_TARGET[$job]}"
    volumes="$(node_exec "$node" pvesm list "$PROD_VM_STORAGE" --vmid "$VMID" 2>/dev/null |
      awk 'NR > 1 && NF' | wc -l)" || volumes=unknown
    [[ "$volumes" == 0 ]] ||
      die "$node still holds $volumes volume(s) of VMID $VMID after its replication job was removed; inspect 'pvesm list $PROD_VM_STORAGE --vmid $VMID' there"
    info "The replica on $node is gone"
  done
}

restore_disks() {
  CURRENT_PHASE="restoring $RESOURCE_NAME from $SNAPSHOT"
  log "Restoring $RESOURCE_NAME from $SNAPSHOT"
  local upid
  upid="$(mox_ssh "$OWNER_NODE" pvesh create "/nodes/${OWNER_NODE}/qemu" --vmid "$VMID" \
    --archive "${BACKUP_STORAGE_ID}:backup/${SNAPSHOT}" --force 1 --storage "$PROD_VM_STORAGE" \
    --output-format json </dev/null 2>"${RUN_DIR}/restore.err" |
    grep -o 'UPID:[^"[:space:]]*' | head -n 1)" || true
  [[ -n "$upid" ]] || die "PVE did not start the restore: $(tr '\n' ' ' <"${RUN_DIR}/restore.err")"
  info "Task $upid"
  pve_task_follow "$OWNER_NODE" "$upid" "$RUN_DIR" "$POLL_SECONDS" ||
    die "The restore failed: ${PVE_TASK_EXITSTATUS:-unknown status}"
  STAGE=restored
}

# Give the restored VM back its identity and the chosen compute, and keep its
# registry volume and disk size valid.
reapply_settings() {
  CURRENT_PHASE="re-applying $RESOURCE_NAME's identity and settings"
  log "Re-applying $RESOURCE_NAME's identity and settings"
  local -a settings=(
    --name "$RESOURCE_NAME" --cores "$CORES" --memory $((MEMORY_GIB * 1024))
    --net0 "${LIVE[net0]}" --tags "${LIVE[tags]}" --hookscript "${LIVE[hookscript]}"
    --onboot "${LIVE[onboot]:-0}"
  )
  [[ -z "${LIVE[description]:-}" ]] || settings+=(--description "${LIVE[description]}")
  node_exec "$OWNER_NODE" qm set "$VMID" "${settings[@]}" >/dev/null || die "Could not re-apply $RESOURCE_NAME's settings"
  # The start hook records these on every start, but not while this rollback
  # holds the lease, so record them here.
  registry_update_resource --cores "$CORES" --memory-mb $((MEMORY_GIB * 1024))

  local config scsi0 restored_bytes live_bytes
  config="$(node_exec "$OWNER_NODE" qm config "$VMID")" || die "Could not read the restored configuration"
  scsi0="$(awk -F': ' '$1 == "scsi0" {print $2}' <<<"$config")"
  [[ -n "$scsi0" ]] || die "The restored VM has no scsi0 disk"
  if [[ "${scsi0%%,*}" != "$ROOT_VOLUME" ]]; then
    registry_update_resource --volume-id "${scsi0%%,*}"
    info "Registry volume updated: $ROOT_VOLUME -> ${scsi0%%,*}"
    ROOT_VOLUME="${scsi0%%,*}"
  fi
  restored_bytes="$(disk_bytes "$scsi0")" || die "Cannot read the restored disk's size"
  live_bytes="$(disk_bytes "${LIVE[scsi0]}")" || die "Cannot read the previous disk's size"
  if ((restored_bytes < live_bytes)); then
    # The disk was extended after the restore point; keep the registered size.
    node_exec "$OWNER_NODE" qm resize "$VMID" scsi0 "$((live_bytes / 1024 / 1024 / 1024))G" >/dev/null ||
      die "Could not grow the restored disk back to $((live_bytes / 1024 / 1024 / 1024)) GiB"
    info "Disk grown back to $((live_bytes / 1024 / 1024 / 1024)) GiB (the guest's filesystem keeps the restore point's size)"
  fi
}

start_vm() {
  CURRENT_PHASE="starting $RESOURCE_NAME through HA"
  log "Starting $RESOURCE_NAME through HA"
  node_exec "$COORDINATOR" ha-manager set "vm:${VMID}" --state started || die "HA did not accept the start request"
  wait_for_status running "$STOP_TIMEOUT_SECONDS" ||
    die "$RESOURCE_NAME did not start within ${STOP_TIMEOUT_SECONDS}s; check 'ha-manager status' and the hook's log on $OWNER_NODE"
  STAGE=started
  info "Running on $(live_owner_now 2>/dev/null || printf '%s' "$OWNER_NODE")"
}

recreate_replication() {
  local job target schedule="*/${REPLICATION_MINUTES}"
  ((${#SAVED_JOBS[@]})) || return 0
  for job in "${SAVED_JOBS[@]}"; do
    target="${SAVED_TARGET[$job]}"
    CURRENT_PHASE="recreating replication $job to $target"
    log "Recreating replication $job to $target ($schedule)"
    node_exec "$OWNER_NODE" pvesr create-local-job "$job" "$target" --schedule "$schedule" \
      --comment "Production ${RESOURCE_NAME} replica" || die "Could not recreate replication job $job"
    wait_for_replication "$OWNER_NODE" "$job" "$target" 0 "$REPLICATION_TIMEOUT_SECONDS"
  done
  registry_update_resource --replication-interval "$REPLICATION_MINUTES"
}

main() {
  parse_args "$@"
  prod_load_config
  prod_preflight_workstation rollback-restore
  trap cleanup EXIT
  SNAPSHOT_TIME=""
  SAFETY_VOLID=""

  prod_discover_cluster
  backup_storage_check "$COORDINATOR" || die "PBS backups are not available"
  if [[ -n "$REQUESTED_VM" ]]; then
    parse_selected_production "$REQUESTED_VM" || die "$REQUESTED_VM is not an active production VM"
  else
    prod_select_production "Production VM to roll back"
  fi
  registry_cmd get "$RESOURCE_NAME" >"${RUN_DIR}/record.json" || die "Could not read $RESOURCE_NAME's registry record"
  prod_refuse_staging_dependents
  prod_load_live_state
  prod_reconcile_registry_owner
  prod_show_live_state
  prod_require_replication_healthy "$OWNER_NODE"
  local job
  for job in "${REPLICATION_JOBS[@]}"; do
    SAVED_JOBS+=("$job")
    SAVED_TARGET["$job"]="${REPLICATION_TARGET[$job]}"
  done

  choose_snapshot
  read_live_config
  choose_compute
  choose_safety_backup
  confirm_rollback

  prod_acquire_lease
  safety_backup
  prod_require_unchanged
  stop_vm
  remove_replication
  restore_disks
  reapply_settings
  start_vm
  recreate_replication
  prod_release_lease
  STAGE="done"

  log "$RESOURCE_NAME is rolled back to ${SNAPSHOT_TIME}"
  info "It runs on $OWNER_NODE with ${CORES} vCPUs and ${MEMORY_GIB} GiB RAM; replication every ${REPLICATION_MINUTES} minutes is healthy again"
  [[ -z "$SAFETY_VOLID" ]] || info "The state before the rollback is restore point ${SAFETY_VOLID#*:backup/}"
  # The restored disk brings back the monitor agent's config of the restore
  # point, whose ping URL may be an older check's; write the current one.
  monitoring_membership_sync "$COORDINATOR"
  bmac_ui_result vm "$RESOURCE_NAME" vmid:int "$VMID" snapshot "$SNAPSHOT" node "$OWNER_NODE" \
    cores:int "$CORES" memory_gib:int "$MEMORY_GIB" safety_backup "${SAFETY_VOLID#*:backup/}"
}

if [[ "${ROLLBACK_RESTORE_SOURCE_ONLY:-0}" != 1 ]]; then
  bmac_ui_bootstrap "$@"
  main "$@"
fi
