#!/usr/bin/env bash

# Copyright (c) 2026 BEENTHERE VENTURES, INC.
# SPDX-License-Identifier: GPL-3.0-only

# Shared names and helpers of the PBS backup workflows under
# scripts/user_callable/backups/. Source after scripts/lib/config.sh.
# scripts/utilities/quick_state.py reads the same storage ID, job prefix, and
# notes format; keep them in step.

# shellcheck disable=SC2034 # Used by the scripts that source this file.
BACKUP_STORAGE_ID=bmac-pbs
BACKUP_JOB_PREFIX=bmac-
BACKUP_EVERY_HOURS=(1 2 3 4 6 8 12 24)
BACKUP_DEFAULT_EVERY_HOURS=4
BACKUP_REGISTRY=/usr/local/lib/app-ha-proxmox/lib/cluster_registry.py
BACKUP_PBS_API=/usr/local/lib/app-ha-proxmox/lib/pbs_api.py
BACKUP_LINK_HELPER=/usr/local/lib/app-ha-proxmox/lib/backup_link.sh
INSTANT_SWITCH_HELPER=/usr/local/lib/app-ha-proxmox/lib/instant_switch.py
# The directory storage, local to each host, holding the qcow2 overlays that
# backup-linked and instant-restored VMs boot from.
OVERLAY_STORAGE_ID=bmac-overlay
OVERLAY_STORAGE_PATH=/var/lib/bmac-overlay

backup_job_id() {
  printf '%s%s\n' "$BACKUP_JOB_PREFIX" "$1"
}

# backup_valid_every HOURS: true for one of BACKUP_EVERY_HOURS.
backup_valid_every() {
  local hours
  for hours in "${BACKUP_EVERY_HOURS[@]}"; do
    [[ "$1" != "$hours" ]] || return 0
  done
  return 1
}

# backup_schedule HOURS INDEX: the PVE calendar event for a backup every
# HOURS hours of production VM prodINDEX. Each slot starts at its own minute
# so the cluster's VMs do not all start at once; daily backups run at 01:MM.
backup_schedule() {
  local hours="$1" index="$2" minute
  minute=$(((5 + 7 * (index - 1)) % 60))
  if ((hours == 24)); then
    printf '01:%02d\n' "$minute"
  else
    printf '0/%d:%02d\n' "$hours" "$minute"
  fi
}

# backup_notes_template NAME PURPOSE FQDN [COMMENT]: the notes every BMAC
# backup of a VM carries. The Backups page lists only snapshots whose notes
# start with "bmac ", and restores read the VM's name, purpose, and FQDN from
# them. A comment follows " | " on the same line: Proxmox lists only a note's
# first line.
backup_notes_template() {
  printf 'bmac %s slug=%s fqdn=%s cluster={{cluster}} guest={{guestname}}%s\n' \
    "$1" "$2" "$3" "${4:+ | $4}"
}

# backup_valid_comment TEXT: one line of at most 200 printable characters
# without { } \ or |, which vzdump's notes template or BMAC's notes format
# would interpret.
backup_valid_comment() {
  [[ ${#1} -le 200 && "$1" != *[{}\\\|]* && ! "$1" =~ [[:cntrl:]] ]]
}

# backup_storage_check HOST: fail unless the cluster has the bmac-pbs storage
# and it is active on HOST.
backup_storage_check() {
  local host="$1" status
  mox_ssh "$host" pvesh get "/storage/${BACKUP_STORAGE_ID}" --output-format json </dev/null >/dev/null 2>&1 ||
    config_die "PBS is not set up: the cluster has no ${BACKUP_STORAGE_ID} storage. Run Setup PBS first." || return
  status="$(mox_ssh "$host" pvesh get "/nodes/${host}/storage/${BACKUP_STORAGE_ID}/status" \
    --output-format json </dev/null 2>/dev/null)" || status=""
  python3 -c 'import json, sys; sys.exit(0 if json.loads(sys.argv[1] or "{}").get("active") else 1)' "$status" ||
    config_die "Storage ${BACKUP_STORAGE_ID} is not active on ${host}; check that PBS is reachable (list_backups.sh shows details)."
}

# backup_production_record HOST NAME FILE: write NAME's registry record to
# FILE, failing unless it is a production VM with a VMID.
backup_production_record() {
  local host="$1" name="$2" file="$3"
  require_var CLUSTER_STATE_DIR >/dev/null || config_die "CLUSTER_STATE_DIR is not configured" || return
  [[ "$name" =~ ^prod[1-9][0-9]*$ ]] || config_die "A production VM name such as prod1 is required" || return
  mox_ssh "$host" "$BACKUP_REGISTRY" --state-dir "$CLUSTER_STATE_DIR" get "$name" </dev/null >"$file" 2>/dev/null ||
    config_die "$name is not a registered production VM" || return
  python3 -c '
import json, sys
row = json.load(open(sys.argv[1]))
sys.exit(0 if row.get("kind") == "production" and row.get("vmid") else 1)' "$file" ||
    config_die "$name is not a production VM with a VMID"
}

# backup_record_field FILE FIELD: one field of a registry record (dotted for
# nested fields, such as domains.primary); empty when absent.
backup_record_field() {
  python3 -c '
import json, sys
value = json.load(open(sys.argv[1]))
for part in sys.argv[2].split("."):
    value = value.get(part) if isinstance(value, dict) else None
print("" if value is None else value)' "$1" "$2"
}

# backup_production_names HOST: the registered production VMs, one per line,
# as prodN, purpose, and primary FQDN separated by the unit separator (\x1f),
# which keeps an empty purpose (tabs would collapse in `read`).
backup_production_names() {
  require_var CLUSTER_STATE_DIR >/dev/null || config_die "CLUSTER_STATE_DIR is not configured" || return
  mox_ssh "$1" "$BACKUP_REGISTRY" --state-dir "$CLUSTER_STATE_DIR" list </dev/null | python3 -c '
import json, sys
rows = [row for row in json.load(sys.stdin) if row.get("kind") == "production"]
for row in sorted(rows, key=lambda row: int(row.get("index") or 0)):
    print("\x1f".join([row["name"], (row.get("purpose") or {}).get("slug") or "",
                     (row.get("domains") or {}).get("primary") or ""]))'
}

# backup_start_task NODE VMID NAME PURPOSE FQDN WORKDIR [COMMENT]: start a
# backup of VMID on NODE, where it runs, with the settings of its backup job
# and an optional comment in its notes; print the task's UPID. PVE's errors
# are left in WORKDIR/start.err.
backup_start_task() {
  local node="$1" vmid="$2" name="$3" purpose="$4" fqdn="$5" workdir="$6" comment="${7:-}" upid
  upid="$(mox_ssh "$node" pvesh create "/nodes/${node}/vzdump" --vmid "$vmid" \
    --storage "$BACKUP_STORAGE_ID" --mode snapshot \
    --fleecing "enabled=1,storage=${PROD_VM_STORAGE:-local-zfs}" \
    --notes-template "$(backup_notes_template "$name" "$purpose" "$fqdn" "$comment")" \
    --output-format json </dev/null 2>"${workdir}/start.err" |
    grep -o 'UPID:[^"[:space:]]*' | head -n 1)" || true
  [[ -n "$upid" ]] || return 1
  printf '%s\n' "$upid"
}

# backup_newest_volid HOST VMID SINCE: the newest restore point of VMID made
# at or after epoch SINCE (less two minutes of clock skew), or nothing.
backup_newest_volid() {
  mox_ssh "$1" pvesh get "/nodes/${1}/storage/${BACKUP_STORAGE_ID}/content" \
    --vmid "$2" --output-format json </dev/null | python3 -c '
import json, sys
rows = [r for r in json.load(sys.stdin) if int(r.get("ctime") or 0) >= int(sys.argv[1]) - 120]
print(max(rows, key=lambda r: r["ctime"])["volid"] if rows else "")' "$3"
}

# pve_task_follow NODE UPID WORKDIR [POLL]: show a PVE task's log as it grows,
# with percentages as progress events, until the task stops. The task runs in
# PVE, so it continues if this workstation disconnects. Returns 0 when it
# ended OK; PVE_TASK_EXITSTATUS holds its exit status either way.
pve_task_follow() {
  local node="$1" upid="$2" workdir="$3" poll="${4:-5}" shown=0 status="" percent text
  PVE_TASK_EXITSTATUS=""
  while :; do
    shown="$(_pve_task_log "$node" "$upid" "$workdir" "$shown")"
    if [[ -s "${workdir}/progress" ]]; then
      IFS=$'\t' read -r percent text <"${workdir}/progress"
      bmac_ui_progress "$text" "$percent" 100 percent
      : >"${workdir}/progress"
    fi
    if mox_ssh "$node" pvesh get "/nodes/${node}/tasks/${upid}/status" --output-format json \
      </dev/null >"${workdir}/status.json" 2>/dev/null; then
      IFS=$'\x1f' read -r status PVE_TASK_EXITSTATUS < <(python3 -c '
import json, sys
row = json.load(open(sys.argv[1]))
print("\x1f".join([str(row.get("status") or ""), str(row.get("exitstatus") or "")]))' "${workdir}/status.json")
      [[ "$status" != stopped ]] || break
    fi
    sleep "$poll"
  done
  _pve_task_log "$node" "$upid" "$workdir" "$shown" >/dev/null
  [[ "$PVE_TASK_EXITSTATUS" == OK ]]
}

# Print the task's log lines after line START on stderr, leave the latest
# percentage in WORKDIR/progress, and print the number of the last line shown.
_pve_task_log() {
  local node="$1" upid="$2" workdir="$3" start="$4"
  mox_ssh "$node" pvesh get "/nodes/${node}/tasks/${upid}/log" --start "$start" --limit 1000 \
    --output-format json </dev/null 2>/dev/null >"${workdir}/log.json" || {
    printf '%s\n' "$start"
    return 0
  }
  python3 - "${workdir}/log.json" "$start" "${workdir}/progress" <<'PY'
import json
import re
import sys

rows = json.load(open(sys.argv[1]))
last = int(sys.argv[2])
progress = None
for row in rows:
    number, text = int(row.get("n", 0)), str(row.get("t", ""))
    if number <= last:
        continue
    last = number
    print(f"    {text}", file=sys.stderr)
    # vzdump writes "INFO:  45% (...)", pbs-restore "progress 45% (...)".
    match = re.match(r"(?:INFO:\s+|progress )(\d+)% \(", text)
    if match:
        progress = (int(match.group(1)), re.sub(r"^INFO:\s*", "", text).strip())
if progress:
    open(sys.argv[3], "w").write(f"{progress[0]}\t{progress[1]}\n")
print(last)
PY
}

# instant_copy_delay SECONDS NAME: the instant restores' developer option
# --copy-delay-seconds. NAME keeps running from its overlay over PBS for
# SECONDS before its data is copied, long enough to try it on PBS-backed
# storage; normally the copy starts at once and takes under a minute.
instant_copy_delay() {
  local remaining="$1" step
  ((remaining > 0)) || return 0
  log "Waiting ${remaining}s before the copy (--copy-delay-seconds): $2 runs from its restore point on PBS"
  while ((remaining > 0)); do
    step=$((remaining < 60 ? remaining : 60))
    sleep "$step"
    remaining=$((remaining - step))
    ((remaining == 0)) || info "${remaining}s until the copy starts"
  done
}

# Add the overlay storage, or check the one there. Needs pvesh_get,
# node_exec, COORDINATOR, log, warn, and die from the calling script.
ensure_overlay_storage() {
  CURRENT_PHASE="checking the overlay storage"
  local storage
  if storage="$(pvesh_get "/storage/${OVERLAY_STORAGE_ID}" 2>/dev/null)"; then
    if python3 -c 'import json, sys; sys.exit(0 if "mkdir" in json.loads(sys.argv[1]) else 1)' "$storage"; then
      node_exec "$COORDINATOR" pvesm set "$OVERLAY_STORAGE_ID" --delete mkdir >/dev/null ||
        warn "Could not remove the deprecated mkdir option from ${OVERLAY_STORAGE_ID}"
    fi
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
    --content images >/dev/null ||
    die "Could not add the overlay storage ${OVERLAY_STORAGE_ID}"
}
