#!/usr/bin/env bash

# Copyright (c) 2026 BEENTHERE VENTURES, INC.
# SPDX-License-Identifier: GPL-3.0-only

# Set how often a production VM is backed up to PBS: create, change, disable,
# or remove its PVE backup job bmac-prodN. The job is cluster-wide, so it backs
# the VM up on whichever host runs it, also after an HA failover.

set -Eeuo pipefail
set +x
umask 077

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
REPO_ROOT="$(cd -- "${SCRIPT_DIR}/../../.." && pwd -P)"
# shellcheck source=../../lib/ui_protocol.sh
source "${REPO_ROOT}/scripts/lib/ui_protocol.sh"
# shellcheck source=../../lib/config.sh
source "${REPO_ROOT}/scripts/lib/config.sh"
# shellcheck source=../../lib/backups.sh
source "${REPO_ROOT}/scripts/lib/backups.sh"
bmac_ui_bootstrap "$@"

VM=""
EVERY=""
ACTION=""
BWLIMIT_MIB=""
NO_BWLIMIT=false
ASSUME_YES=false
COORDINATOR=""
RUN_DIR=""

log() {
  printf '\n==> %s\n' "$*"
  bmac_ui_step "$*"
}

info() {
  printf '    %s\n' "$*"
}

die() {
  printf '\nERROR: %s\n' "$*" >&2
  bmac_ui_error failed "$*"
  exit 1
}

usage() {
  cat <<'EOF'
Usage: scripts/user_callable/backups/manage_backup_schedule.sh [prodN]
         [--vm prodN] [--every HOURS | --disable | --remove]
         [--bwlimit-mib N | --no-bwlimit] [--yes] [--json]

Set how often a production VM is backed up to the cluster's PBS storage
(bmac-pbs). The VM's backup job, bmac-prodN, runs on whichever host the VM is
on, in snapshot mode with the guest agent's filesystem freeze, fleecing on
local storage, and BMAC's notes on every backup.

  --vm prodN        The VM (or give prodN as the first argument).
  --every HOURS     Back up every 1, 2, 3, 4, 6, 8, 12 or 24 hours.
  --disable         Keep the job but stop running it.
  --remove          Delete the job. Its backups stay on PBS.
  --bwlimit-mib N   Limit the upload to N MiB/s (default: no limit).
  --no-bwlimit      Remove a limit set earlier.
  --yes             Apply without asking.

How long backups are kept is decided by the PBS server's prune job, not here:
more frequent backups keep more recent points, not a longer history.
EOF
}

parse_args() {
  while (($#)); do
    case "$1" in
      --yes) ASSUME_YES=true ;;
      --disable) ACTION=disable ;;
      --remove) ACTION=remove ;;
      --no-bwlimit) NO_BWLIMIT=true ;;
      --vm | --every | --bwlimit-mib)
        (($# >= 2)) || die "$1 needs a value"
        case "$1" in
          --vm) VM="$2" ;;
          --every) EVERY="$2"; ACTION="set" ;;
          --bwlimit-mib) BWLIMIT_MIB="$2" ;;
        esac
        shift
        ;;
      -h | --help)
        usage
        exit 0
        ;;
      prod[1-9]*) VM="$1" ;;
      *)
        usage >&2
        exit 2
        ;;
    esac
    shift
  done
  [[ -z "$EVERY" ]] || backup_valid_every "$EVERY" ||
    die "--every must be one of ${BACKUP_EVERY_HOURS[*]} hours"
  [[ -z "$BWLIMIT_MIB" || "$BWLIMIT_MIB" =~ ^[1-9][0-9]{0,5}$ ]] ||
    die "--bwlimit-mib must be a whole number of MiB/s"
  [[ -z "$BWLIMIT_MIB" || "$NO_BWLIMIT" == false ]] ||
    die "Choose --bwlimit-mib or --no-bwlimit, not both"
}

# The VM's current job as enabled, schedule, bwlimit (KiB/s), next run, VMID,
# and storage, separated by the unit separator (\x1f) so that an empty field
# is kept (tabs would collapse), or nothing when it has none.
read_job() {
  mox_ssh "$COORDINATOR" pvesh get /cluster/backup --output-format json </dev/null |
    python3 -c '
import json, sys
for job in json.load(sys.stdin):
    if job.get("id") == sys.argv[1]:
        enabled = "0" if job.get("enabled", 1) in (0, "0", False) else "1"
        print("\x1f".join([enabled, str(job.get("schedule") or ""), str(job.get("bwlimit") or ""),
                         str(job.get("next-run") or ""), str(job.get("vmid") or ""),
                         str(job.get("storage") or "")]))' "$JOB_ID"
}

choose_vm() {
  local -a rows=() options=()
  local row name purpose fqdn
  mapfile -t rows < <(backup_production_names "$COORDINATOR")
  ((${#rows[@]})) || die "No production VMs are registered"
  for row in "${rows[@]}"; do
    IFS=$'\x1f' read -r name purpose fqdn <<<"$row"
    options+=("$name" "${name}${purpose:+ · $purpose}${fqdn:+ · $fqdn}")
  done
  if bmac_ui_is_json; then
    bmac_ui_choose VM "Production VM" "${options[0]}" "${options[@]}"
  elif [[ "$ASSUME_YES" == true ]]; then
    die "Name the production VM (for example prod1)"
  else
    printf '    Production VMs: %s\n' "$(printf '%s\n' "${rows[@]}" | cut -d $'\x1f' -f1 | tr '\n' ' ')"
    read -r -p "Production VM: " VM
  fi
}

choose_action() {
  local current_every="$1" exists="$2" enabled="$3" answer default
  [[ -z "$ACTION" ]] || return 0
  default="${current_every:-$BACKUP_DEFAULT_EVERY_HOURS}"
  if bmac_ui_is_json; then
    local -a options=()
    local hours
    for hours in "${BACKUP_EVERY_HOURS[@]}"; do
      options+=("$hours" "Every ${hours} hour$( ((hours == 1)) || printf s)$( ((hours == BACKUP_DEFAULT_EVERY_HOURS)) && printf ' (recommended)' || true)")
    done
    if [[ "$exists" == true && "$enabled" == 1 ]]; then
      options+=(disable "Disable the job (keep its settings)")
    fi
    [[ "$exists" != true ]] || options+=(remove "Remove the job (its backups stay on PBS)")
    [[ "$enabled" == 1 || -z "$current_every" ]] || default="$current_every"
    bmac_ui_choose answer "Back up $VM" "$default" "${options[@]}"
  elif [[ "$ASSUME_YES" == true ]]; then
    die "Give --every HOURS, --disable, or --remove"
  else
    read -r -p "Back up $VM every how many hours (${BACKUP_EVERY_HOURS[*]}), or disable/remove [$default]: " answer
    answer="${answer:-$default}"
  fi
  case "$answer" in
    disable | remove) ACTION="$answer" ;;
    *)
      backup_valid_every "$answer" || die "Choose one of ${BACKUP_EVERY_HOURS[*]} hours"
      ACTION="set"
      EVERY="$answer"
      ;;
  esac
}

# What the namespace's prune job keeps at this frequency, when the token can
# read it.
describe_retention() {
  local hours="$1"
  mox_ssh "$COORDINATOR" python3 -I "$BACKUP_PBS_API" inspect --storage "$BACKUP_STORAGE_ID" \
    </dev/null 2>/dev/null | python3 -c '
import json, sys
hours = int(sys.argv[1])
try:
    jobs = json.load(sys.stdin)["prune"]["jobs"]
except (ValueError, KeyError, TypeError):
    sys.exit(0)
for job in jobs:
    last = job.get("keep-last")
    keeps = ", ".join(f"{k} {v}" for k, v in sorted(job.items()) if k.startswith("keep-"))
    name = job.get("id")
    line = f"PBS keeps: {keeps} ({name})"
    if last:
        line += f"; at every {hours}h, keep-last {last} covers the last {last * hours} hours"
    print(line)' "$hours" || true
}

main() {
  parse_args "$@"
  load_proxmox_config --no-secrets >/dev/null || die "cluster configuration is invalid"
  RUN_DIR="$(mktemp -d "${TMPDIR:-/tmp}/manage-backup-schedule.XXXXXX")"
  chmod 0700 "$RUN_DIR"
  trap 'rm -rf -- "$RUN_DIR"' EXIT
  COORDINATOR="$(first_reachable_mox)" || die "No reachable Proxmox host was found"
  backup_storage_check "$COORDINATOR" || die "PBS backups are not available"

  [[ -n "$VM" ]] || choose_vm
  backup_production_record "$COORDINATOR" "$VM" "${RUN_DIR}/record.json" || die "Cannot back up $VM"
  local vmid index purpose fqdn
  vmid="$(backup_record_field "${RUN_DIR}/record.json" vmid)"
  index="$(backup_record_field "${RUN_DIR}/record.json" index)"
  purpose="$(backup_record_field "${RUN_DIR}/record.json" purpose.slug)"
  fqdn="$(backup_record_field "${RUN_DIR}/record.json" domains.primary)"
  [[ "$index" =~ ^[1-9][0-9]*$ ]] || die "$VM's registry record has no slot index"
  JOB_ID="$(backup_job_id "$VM")"

  local job="" exists=false enabled="" schedule="" bwlimit="" next_run="" job_vmid="" job_storage=""
  job="$(read_job)" || die "Could not read the cluster's backup jobs"
  if [[ -n "$job" ]]; then
    exists=true
    IFS=$'\x1f' read -r enabled schedule bwlimit next_run job_vmid job_storage <<<"$job"
  fi
  local current_every=""
  if [[ "$schedule" =~ ^0/([0-9]+):[0-9]{2}$ ]]; then
    current_every="${BASH_REMATCH[1]}"
  elif [[ "$schedule" =~ ^[0-9]{1,2}:[0-9]{2}$ ]]; then
    current_every=24
  fi

  log "Backups of $VM (VMID $vmid)"
  if [[ "$exists" == true ]]; then
    info "Job $JOB_ID: $([[ "$enabled" == 1 ]] && printf 'every %sh (%s)' "${current_every:-?}" "$schedule" || printf 'disabled')${bwlimit:+, limited to $((bwlimit / 1024)) MiB/s}"
    [[ "$job_vmid" == "$vmid" && "$job_storage" == "$BACKUP_STORAGE_ID" ]] ||
      info "It backs up VMID ${job_vmid:-?} to ${job_storage:-?}; saving it repairs that"
  else
    info "No backup job yet"
  fi

  choose_action "$current_every" "$exists" "$enabled"
  [[ "$ACTION" != disable && "$ACTION" != remove || "$exists" == true ]] ||
    die "$VM has no backup job to ${ACTION}"

  local new_schedule="" new_bwlimit="$bwlimit" summary
  case "$ACTION" in
    set)
      new_schedule="$(backup_schedule "$EVERY" "$index")"
      [[ -z "$BWLIMIT_MIB" ]] || new_bwlimit=$((BWLIMIT_MIB * 1024))
      [[ "$NO_BWLIMIT" == false ]] || new_bwlimit=""
      summary="Back up $VM every ${EVERY}h (schedule ${new_schedule})${new_bwlimit:+, upload limited to $((new_bwlimit / 1024)) MiB/s}"
      ;;
    disable) summary="Disable job $JOB_ID; $VM is no longer backed up until it is enabled again" ;;
    remove) summary="Remove job $JOB_ID; $VM's existing backups stay on PBS" ;;
  esac

  log "Plan"
  info "$summary"
  [[ "$ACTION" != set ]] || describe_retention "$EVERY" | while IFS= read -r line; do info "$line"; done
  bmac_ui_plan_begin "Backup schedule for $VM"
  local verb=create
  [[ "$exists" != true ]] || verb=update
  [[ "$ACTION" != remove ]] || verb=remove
  bmac_ui_plan_item "$verb" "$JOB_ID" "$summary"
  bmac_ui_plan_end
  if [[ "$ASSUME_YES" != true ]]; then
    if bmac_ui_is_json; then
      bmac_ui_confirm --id apply --title "Save $VM's backup schedule?" --message "$summary" \
        --confirm-label Save || die "Cancelled; nothing was changed"
    else
      local answer
      read -r -p "Apply? [y/N] " answer
      [[ "$answer" =~ ^[Yy]([Ee][Ss])?$ ]] || die "Cancelled; nothing was changed"
    fi
  fi

  case "$ACTION" in
    remove)
      log "Removing job $JOB_ID"
      mox_ssh "$COORDINATOR" pvesh delete "/cluster/backup/${JOB_ID}" </dev/null >/dev/null ||
        die "Could not remove job $JOB_ID"
      bmac_ui_result vm "$VM" job "$JOB_ID" removed:bool true
      return 0
      ;;
    disable)
      log "Disabling job $JOB_ID"
      mox_ssh "$COORDINATOR" pvesh set "/cluster/backup/${JOB_ID}" --enabled 0 </dev/null >/dev/null ||
        die "Could not disable job $JOB_ID"
      bmac_ui_result vm "$VM" job "$JOB_ID" enabled:bool false
      return 0
      ;;
  esac

  local fleecing_storage="${PROD_VM_STORAGE:-local-zfs}"
  local -a settings=(
    --schedule "$new_schedule" --storage "$BACKUP_STORAGE_ID" --vmid "$vmid"
    --mode snapshot --enabled 1 --repeat-missed 1 --prune-backups keep-all=1
    --fleecing "enabled=1,storage=${fleecing_storage}"
    --notes-template "$(backup_notes_template "$VM" "$purpose" "$fqdn")"
    --comment "managed-by=bmac; ${VM}"
  )
  [[ -z "$new_bwlimit" ]] || settings+=(--bwlimit "$new_bwlimit")
  if [[ "$exists" == true ]]; then
    log "Updating job $JOB_ID"
    [[ -n "$new_bwlimit" || -z "$bwlimit" ]] || settings+=(--delete bwlimit)
    mox_ssh "$COORDINATOR" pvesh set "/cluster/backup/${JOB_ID}" "${settings[@]}" </dev/null >/dev/null ||
      die "Could not update job $JOB_ID"
  else
    log "Creating job $JOB_ID"
    mox_ssh "$COORDINATOR" pvesh create /cluster/backup --id "$JOB_ID" "${settings[@]}" </dev/null >/dev/null ||
      die "Could not create job $JOB_ID"
  fi

  job="$(read_job)" || die "Could not read job $JOB_ID back"
  IFS=$'\x1f' read -r enabled schedule bwlimit next_run job_vmid job_storage <<<"$job"
  [[ "$enabled" == 1 && "$schedule" == "$new_schedule" && "$job_vmid" == "$vmid" &&
    "$job_storage" == "$BACKUP_STORAGE_ID" ]] ||
    die "Job $JOB_ID does not hold the new settings (enabled=${enabled:-?}, schedule=${schedule:-?}, vmid=${job_vmid:-?})"
  info "Saved; next backup $(date -d "@${next_run}" '+%Y-%m-%d %H:%M %Z' 2>/dev/null ||
    date -r "${next_run:-0}" '+%Y-%m-%d %H:%M %Z' 2>/dev/null || printf 'at %s' "${next_run:-?}")"
  bmac_ui_result vm "$VM" job "$JOB_ID" enabled:bool true every_hours:int "$EVERY" \
    schedule "$new_schedule" next_run:int "${next_run:-0}"
}

if [[ "${MANAGE_BACKUP_SCHEDULE_SOURCE_ONLY:-0}" != 1 ]]; then
  main "$@"
fi
