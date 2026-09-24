#!/usr/bin/env bash

# Copyright (c) 2026 BEENTHERE VENTURES, INC.
# SPDX-License-Identifier: GPL-3.0-only

# Prepare one host's rpool for giving up a top-level mirror vdev, then start
# that vdev's removal. The run trims the production guests whose disks live on
# the host, forces their replication so the freed space reaches the host's
# copies, scrubs rpool, and offers only vdevs whose removal keeps at least the
# operator's minimum free space. ZFS removes one top-level vdev at a time, so
# each run starts at most one removal; hosts/list_disks_ready_for_physically_removal.sh
# reports when its disks can be pulled.

set -Eeuo pipefail
set +x
umask 077

DW_SCRIPT_NAME=decommission_disks.sh
SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
# shellcheck source=../lib/disk_workflows.sh
source "${SCRIPT_DIR}/../lib/disk_workflows.sh"

RECENT_SECONDS=86400
MIN_FREE_FLOOR_GIB=50
REPLICATION_TIMEOUT_SECONDS="${APP_HA_REPLICATION_TIMEOUT_SECONDS:-3600}"
REPLICATION_POLL_SECONDS="${APP_HA_REPLICATION_POLL_SECONDS:-10}"

usage() {
  cat <<'EOF'
Usage: hosts/decommission_disks.sh [--host moxN]

Prepares the chosen host's rpool to give up one top-level mirror vdev and
starts its removal:

  1. requires that no staging VM related to the host still exists;
  2. trims (fstrim) every production guest that runs on the host or replicates
     to it, over `ssh prodN`, then forces their replication;
  3. scrubs rpool (zpool scrub -w);
  4. asks for the minimum free space rpool must keep, at least 50 GiB, and
     offers only vdevs whose removal keeps it; the boot mirror is never offered;
  5. records the chosen vdev's disk serials on the host and runs
     `zpool remove`, which evacuates the vdev in the background.

Trims and scrubs completed in the last 24 hours are recorded on the host and
not repeated, so an interrupted run can simply be started again. Run the
script once per vdev; ZFS removes one top-level vdev at a time.
EOF
}

HOST_ARG=""
while (($#)); do
  case "$1" in
    --host)
      (($# >= 2)) || dw_die "--host requires moxN"
      HOST_ARG="$2"
      shift 2
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
done

dw_init
require_vars STAGING_VM_TAG CLUSTER_STATE_DIR ||
  dw_die "cluster.conf must define STAGING_VM_TAG and CLUSTER_STATE_DIR"
dw_select_host "$HOST_ARG"
LAYOUT="${DW_RUN_DIR}/layout.json"
STATE="${DW_RUN_DIR}/state.json"
dw_collect_layout "$LAYOUT"
dw_state show >"$STATE" || dw_die "could not read the storage state on $DW_HOST"

units() {
  python3 -c 'import sys; sys.path.insert(0, sys.argv[1]); import host_storage; print(host_storage.format_byte_units(int(sys.argv[2])))' \
    "$DW_LIB_DIR" "$1"
}

dw_section "rpool on $DW_HOST"
dw_render_layout "$LAYOUT"

# ZFS evacuates one top-level vdev at a time, and the previous removal's disks
# must be retired before the next one is recorded.
if [[ "$(dw_json "$LAYOUT" 'str(d["pool"]["removal_in_progress"])')" == True ]]; then
  printf '\nA vdev removal is already in progress on %s:\n%s\n' \
    "$DW_HOST" "$(dw_json "$LAYOUT" 'd["pool"]["remove"]')"
  printf 'ZFS removes one top-level vdev at a time. Follow it with\n  hosts/list_disks_ready_for_physically_removal.sh --host %s\nand run this script again after it completes.\n' "$DW_HOST"
  exit 1
fi
PENDING="$(dw_json "$STATE" '" ".join(r["vdev"] for r in d["removals"] if r["state"] == "requested")')"
if [[ -n "$PENDING" ]]; then
  printf '\nThe removal of %s is recorded on %s but has not been retired yet.\n' "$PENDING" "$DW_HOST"
  printf 'Run hosts/list_disks_ready_for_physically_removal.sh --host %s first.\n' "$DW_HOST"
  exit 1
fi

dw_section "Staging VMs related to $DW_HOST"
RESOURCES="${DW_RUN_DIR}/resources.json"
CLEANUP="${DW_RUN_DIR}/cleanup.json"
CLUSTER_VMS="${DW_RUN_DIR}/cluster-vms.json"
REPLICATION="${DW_RUN_DIR}/replication.json"
HOST_QEMU="${DW_RUN_DIR}/host-qemu.json"
dw_on_host "$DW_REMOTE_REGISTRY" --state-dir "$CLUSTER_STATE_DIR" \
  list --record-type resources >"$RESOURCES" ||
  dw_die "could not read the app-ha registry through $DW_HOST"
dw_on_host "$DW_REMOTE_REGISTRY" --state-dir "$CLUSTER_STATE_DIR" \
  list --record-type cleanup >"$CLEANUP" ||
  dw_die "could not read deferred-cleanup records through $DW_HOST"
dw_on_host pvesh get /cluster/resources --type vm --output-format json >"$CLUSTER_VMS" ||
  dw_die "could not list cluster guests"
dw_on_host pvesh get /cluster/replication --output-format json >"$REPLICATION" ||
  dw_die "could not list replication jobs"
dw_on_host pvesh get "/nodes/${DW_HOST}/qemu" --output-format json >"$HOST_QEMU" ||
  dw_die "could not list the guests on $DW_HOST"

# Two groups: staging on the target host, and staging cloned from a
# production guest whose disk lives on the target (its base snapshot, which
# pins old blocks, exists on every placement node of that guest).
if ! python3 - "$DW_HOST" "$RESOURCES" "$CLEANUP" "$HOST_QEMU" "$LAYOUT" \
  "$STAGING_VM_TAG" <<'PY'
import json
import re
import sys

host, resources, cleanup, host_qemu, layout, staging_tag = sys.argv[1:]
resources = json.load(open(resources))
cleanup = json.load(open(cleanup))
host_qemu = json.load(open(host_qemu))
layout = json.load(open(layout))
production = {row["name"]: row for row in resources if row["kind"] == "production"}
problems = []
for row in resources:
    if row["kind"] != "staging":
        continue
    source = production.get(row.get("source"), {})
    reasons = []
    if row.get("owner_node") == host or host in row.get("placement", []):
        reasons.append(f"runs on {host}")
    if host in source.get("placement", []):
        reasons.append(f"cloned from {row.get('source')}, whose disk is on {host}")
    if reasons:
        problems.append(f"staging VM {row['name']} (VMID {row['vmid']}, {row['state']}): " + "; ".join(reasons))
registered = {row["vmid"] for row in resources}
for vm in host_qemu:
    tags = set(re.split(r"[;,]", vm.get("tags", "") or ""))
    if staging_tag in tags and vm.get("vmid") not in registered:
        problems.append(f"unregistered staging-tagged VM {vm.get('name')} (VMID {vm.get('vmid')}) on {host}")
for volume in layout["volumes"]:
    for snapshot in volume["staging_snapshots"]:
        problems.append(f"staging base snapshot {volume['name']}@{snapshot} on {host}")
for row in cleanup:
    if row.get("node") == host and row.get("state") == "pending":
        problems.append(f"pending deferred cleanup {row['action']} {row['target']} for {row['resource']}")
for problem in problems:
    print(f"  - {problem}")
raise SystemExit(1 if problems else 0)
PY
then
  printf '\nEvery staging VM related to %s must be destroyed first, so no staging clone\n' "$DW_HOST"
  printf 'or base snapshot keeps old blocks allocated. Destroy each staging VM with\n'
  printf '  guests/staging/destroy_staging_vm.sh stageNprodN\n'
  printf 'which also removes its base snapshot from every node. Wait until its\n'
  printf 'snapshot and cleanup records are gone, then run this script again.\n'
  exit 1
fi
printf '  none\n'

dw_section "Production guests whose disks are on $DW_HOST"
GUESTS="${DW_RUN_DIR}/guests.tsv"
python3 - "$DW_HOST" "$RESOURCES" "$CLUSTER_VMS" "$REPLICATION" >"$GUESTS" <<'PY'
import json
import sys

host, resources, cluster_vms, replication = sys.argv[1:]
resources = json.load(open(resources))
nodes = {int(row["vmid"]): row.get("node") for row in json.load(open(cluster_vms))}
jobs = json.load(open(replication))
for row in sorted(
    (row for row in resources if row["kind"] == "production"),
    key=lambda row: int(row["name"][4:]),
):
    vmid = int(row["vmid"])
    node = nodes.get(vmid)
    if node == host:
        reason = f"runs on {host}"
    elif any(int(job["guest"]) == vmid and job.get("target") == host for job in jobs):
        reason = f"replicates to {host}"
    else:
        continue
    print(f"{row['name']}\t{vmid}\t{node or '?'}\t{reason}")
PY
mapfile -t GUEST_ROWS <"$GUESTS"
declare -a GUEST_NAMES=()
if ((${#GUEST_ROWS[@]} == 0)); then
  printf '  none; no guest trims or replication are needed.\n'
else
  for row in "${GUEST_ROWS[@]}"; do
    IFS=$'\t' read -r name vmid node reason <<<"$row"
    printf '  %-8s VMID %-6s on %-6s %s\n' "$name" "$vmid" "$node" "$reason"
    GUEST_NAMES+=("$name")
  done

  while true; do
    IFS= read -r -p $'\nHave you deleted all unwanted files inside each of these guests? [Y/n] ' answer ||
      dw_die "input ended"
    case "${answer,,}" in
      "" | y | yes) break ;;
      n | no)
        printf 'Delete the unwanted files inside each guest first, then run this script again.\n'
        exit 0
        ;;
    esac
  done

  printf '\nThe trims run over SSH as `ssh prodN`, using the alias create_prod_vm.sh wrote\n'
  printf 'to ~/.ssh/config. Checking that each guest accepts it:\n'
  ssh_failed=0
  for name in "${GUEST_NAMES[@]}"; do
    if ssh -o BatchMode=yes -o ConnectTimeout=15 "$name" true </dev/null >/dev/null 2>&1; then
      printf '  [PASS] ssh %s\n' "$name"
    else
      printf '  [FAIL] ssh %s\n' "$name"
      ssh_failed=1
    fi
  done
  ((ssh_failed == 0)) ||
    dw_die "fix SSH access to the guests marked FAIL (try: ssh prodN), then run this script again"

  dw_section "Trimming guest filesystems"
  mapfile -t TRIM_NEEDED < <(python3 - "$STATE" "$RECENT_SECONDS" "${GUEST_NAMES[@]}" <<'PY'
import json
import sys
state = json.load(open(sys.argv[1]))
window = int(sys.argv[2])
for guest in sys.argv[3:]:
    done = state["trims"].get(guest, {}).get("completed_at")
    if done is not None and state["now"] - done < window:
        print(f"SKIP\t{guest}\t{(state['now'] - done) // 60}")
    else:
        print(f"TRIM\t{guest}")
PY
  )
  declare -a TO_TRIM=()
  for row in "${TRIM_NEEDED[@]}"; do
    IFS=$'\t' read -r action name minutes <<<"$row"
    if [[ "$action" == SKIP ]]; then
      printf '  %s was trimmed %s minutes ago; treating that as sufficient.\n' "$name" "$minutes"
    else
      TO_TRIM+=("$name")
    fi
  done
  if ((${#TO_TRIM[@]} > 0)); then
    dw_ready "fstrim / inside ${TO_TRIM[*]}, one guest at a time" \
      "minutes to hours per guest, depending on how much was deleted" ||
      dw_die "stopped before trimming; run this script again when ready"
    for name in "${TO_TRIM[@]}"; do
      printf '\n$ ssh %s fstrim -v /\n' "$name"
      ssh -o BatchMode=yes "$name" fstrim -v / </dev/null ||
        dw_die "fstrim failed on $name; completed trims are recorded, so run this script again"
      dw_state record-trim "$name" >/dev/null ||
        dw_die "could not record the trim of $name on $DW_HOST"
    done
  fi

  dw_section "Forcing replication of the trimmed guests"
  mapfile -t JOBS < <(python3 - "$DW_HOST" "$REPLICATION" "$CLUSTER_VMS" "${GUEST_NAMES[@]}" <<'PY'
import json
import sys
host, replication, cluster_vms, *names = sys.argv[1:]
vms = {int(row["vmid"]): row for row in json.load(open(cluster_vms))}
wanted = {int(vmid) for vmid, row in vms.items() if row.get("name") in names}
for job in sorted(json.load(open(replication)), key=lambda job: job["id"]):
    vmid = int(job["guest"])
    source = vms.get(vmid, {}).get("node")
    if vmid in wanted and (source == host or job.get("target") == host):
        print(f"{job['id']}\t{source}\t{job.get('target')}")
PY
  )
  if ((${#JOBS[@]} == 0)); then
    printf '  no replication jobs involve these guests and %s.\n' "$DW_HOST"
  else
    dw_ready "run ${#JOBS[@]} replication job(s) now so the trimmed space is also freed in $DW_HOST's copies and old replication snapshots rotate" \
      "a few minutes" ||
      dw_die "stopped before replication; run this script again when ready"
    for row in "${JOBS[@]}"; do
      IFS=$'\t' read -r job source target <<<"$row"
      status_path="/nodes/${source}/replication/${job}/status"
      baseline="$(dw_on_node "$source" pvesh get "$status_path" --output-format json 2>/dev/null |
        python3 -c 'import json,sys; v=json.load(sys.stdin).get("last_sync", 0); print(v if isinstance(v, int) else 0)' 2>/dev/null)" ||
        baseline=0
      printf '  %s (%s -> %s): ' "$job" "$source" "$target"
      requested=false
      deadline=$((SECONDS + REPLICATION_TIMEOUT_SECONDS))
      while true; do
        if [[ "$requested" == false ]] &&
          dw_on_node "$source" pvesr schedule-now "$job" >/dev/null 2>&1; then
          requested=true
        fi
        status_json="$(dw_on_node "$source" pvesh get "$status_path" --output-format json 2>/dev/null)" ||
          status_json=""
        if [[ -n "$status_json" ]] && python3 - "$status_json" "$baseline" <<'PY'
import json
import sys
row = json.loads(sys.argv[1])
ok = (
    isinstance(row.get("last_sync"), int)
    and row["last_sync"] > int(sys.argv[2])
    and int(row.get("fail_count", 0)) == 0
    and not row.get("error")
    and not row.get("pid")
)
raise SystemExit(0 if ok else 1)
PY
        then
          printf 'replicated\n'
          break
        fi
        ((SECONDS < deadline)) ||
          dw_die "timed out waiting for replication job $job; check it with pvesr status on $source"
        sleep "$REPLICATION_POLL_SECONDS"
      done
    done
  fi
fi

dw_section "Scrubbing rpool on $DW_HOST"
RECENT_SCRUB="$(python3 - "$STATE" "$RECENT_SECONDS" <<'PY'
import json
import sys
state = json.load(open(sys.argv[1]))
clean = [row for row in state["scrubs"] if "with 0 errors" in row["result"]]
if clean and state["now"] - clean[-1]["completed_at"] < int(sys.argv[2]):
    print((state["now"] - clean[-1]["completed_at"]) // 60)
PY
)"
if [[ -n "$RECENT_SCRUB" ]]; then
  printf '  A clean scrub completed %s minutes ago; treating it as sufficient.\n' "$RECENT_SCRUB"
else
  SCAN="$(dw_json "$LAYOUT" 'd["pool"]["scan"] or ""')"
  if [[ "$SCAN" == *"scrub in progress"* ]]; then
    dw_ready "wait for the scrub already running on $DW_HOST to finish (zpool wait -t scrub rpool)" \
      "hours" || dw_die "stopped; run this script again when ready"
    dw_on_host zpool wait -t scrub rpool || dw_die "waiting for the scrub failed"
  else
    dw_ready "scrub rpool on $DW_HOST (zpool scrub -w rpool) so every block is verified before any data is evacuated" \
      "several hours on a large pool" || dw_die "stopped before scrubbing; run this script again when ready"
    dw_on_host zpool scrub -w rpool ||
      dw_die "the scrub did not complete; if it is still running, run this script again to wait for it"
  fi
  dw_collect_layout "$LAYOUT"
  SCAN="$(dw_json "$LAYOUT" '(d["pool"]["scan"] or "").splitlines()[0] if d["pool"]["scan"] else ""')"
  [[ "$SCAN" == *scrub* ]] || dw_die "could not read the scrub result from zpool status"
  dw_state record-scrub --result "$SCAN" >/dev/null ||
    dw_die "could not record the scrub on $DW_HOST"
  printf '  %s\n' "$SCAN"
  [[ "$SCAN" == *"with 0 errors"* ]] ||
    dw_die "the scrub reported errors; resolve them before removing a vdev"
fi

dw_collect_layout "$LAYOUT"
CURRENT_FREE="$(dw_json "$LAYOUT" 'd["pool"]["dataset_available"]')"
[[ "$CURRENT_FREE" =~ ^[0-9]+$ ]] || dw_die "could not read the free space of rpool"
dw_section "Free space on $DW_HOST"
printf 'current_zfs_free_space (zfs get available rpool):\n  %s\n' "$(units "$CURRENT_FREE")"

MIN_FREE=""
while [[ -z "$MIN_FREE" ]]; do
  IFS= read -r -p $'\nMinimum free space rpool must keep after the removal, in GiB (at least 50): ' answer ||
    dw_die "input ended"
  MIN_FREE="$(python3 - "$answer" "$MIN_FREE_FLOOR_GIB" <<'PY'
from decimal import Decimal, InvalidOperation, ROUND_CEILING
import sys
try:
    gib = Decimal(sys.argv[1].strip())
except InvalidOperation:
    raise SystemExit(0)
if not gib.is_finite() or gib < Decimal(sys.argv[2]):
    raise SystemExit(0)
print(int((gib * 1024 ** 3).to_integral_value(rounding=ROUND_CEILING)))
PY
  )"
  [[ -n "$MIN_FREE" ]] || printf 'Enter a number of GiB that is at least %s.\n' "$MIN_FREE_FLOOR_GIB"
done
printf 'min_free_space:\n  %s\n' "$(units "$MIN_FREE")"

if ((MIN_FREE >= CURRENT_FREE)); then
  printf '\nThe minimum free space you set exceeds what rpool can offer even before any\n'
  printf 'disk is removed, so no vdev can be decommissioned while keeping it.\n'
  exit 0
fi
REMOVABLE=$((CURRENT_FREE - MIN_FREE))
printf 'removable_space = current_zfs_free_space - min_free_space:\n  %s\n' "$(units "$REMOVABLE")"

dw_section "Removal candidates"
mapfile -t VDEV_ROWS < <(python3 - "$LAYOUT" "$REMOVABLE" <<'PY'
import json
import sys
layout = json.load(open(sys.argv[1]))
removable = int(sys.argv[2])
for vdev in layout["vdevs"]:
    if vdev["holds_esp"]:
        status = "never: boot mirror holding the ESPs"
    elif vdev["removing"]:
        status = "never: already being removed"
    elif vdev["size"] is None:
        status = "never: size unknown"
    elif vdev["size"] > removable:
        status = "no: its capacity exceeds removable_space"
    else:
        status = "eligible"
    serials = ",".join(member["serial"] or "?" for member in vdev["members"])
    print(f"{vdev['name']}\t{vdev['size'] or 0}\t{status}\t{serials}")
PY
)
declare -a ELIGIBLE=()
for row in "${VDEV_ROWS[@]}"; do
  IFS=$'\t' read -r vdev size status serials <<<"$row"
  if [[ "$status" == eligible ]]; then
    ELIGIBLE+=("$vdev")
    printf '  %d) %-9s %16s bytes  disks %s\n' "${#ELIGIBLE[@]}" "$vdev" "$size" "$serials"
  else
    printf '     %-9s %16s bytes  disks %s  (%s)\n' "$vdev" "$size" "$serials" "$status"
  fi
done
if ((${#ELIGIBLE[@]} == 0)); then
  printf '\nNo vdev fits within removable_space; nothing can be decommissioned.\n'
  exit 0
fi
while true; do
  IFS= read -r -p $'\nNumber of the vdev to remove (q to stop without removing anything): ' choice ||
    dw_die "input ended"
  if [[ "$choice" == q ]]; then
    printf 'No vdev was removed.\n'
    exit 0
  fi
  [[ "$choice" =~ ^[1-9][0-9]*$ ]] && ((choice <= ${#ELIGIBLE[@]})) && break
  printf 'Enter a number from the list.\n'
done
VDEV="${ELIGIBLE[choice - 1]}"

CONF_PATH="$(dw_conf_path)"
REQUEST="$(python3 - "$LAYOUT" "$VDEV" "$CONF_PATH" "$DW_CONF_MIRRORS" <<'PY'
import json
from pathlib import Path
import subprocess
import sys
layout, vdev_name, conf, conf_tool = sys.argv[1:]
vdev = next(v for v in json.load(open(layout))["vdevs"] if v["name"] == vdev_name)
members = [
    {
        "path": m["path"],
        "mapper": m["mapper"],
        "luks": m["luks"],
        "disk": m["disk"],
        "serial": m["serial"],
        "model": m["model"],
        "disk_size": m["disk_size"],
    }
    for m in vdev["members"]
]
if any(not m["serial"] for m in members):
    raise SystemExit("a member of this vdev has no disk serial; it cannot be tracked for removal")
pair = None
shown = None
if Path(conf).is_file():
    shown = subprocess.run(
        [sys.executable, conf_tool, "show", conf], capture_output=True, text=True
    )
if shown is not None and shown.returncode == 0:
    serials = {m["serial"] for m in members}
    for number, values in json.loads(shown.stdout).items():
        if {values.get("serial_1"), values.get("serial_2")} & serials and int(number) != 1:
            pair = int(number)
print(json.dumps({"vdev": vdev_name, "members": members, "conf_pair": pair}))
PY
)" || dw_die "could not describe $VDEV for the removal record"

printf '\n%s holds these disks:\n' "$VDEV"
python3 - "$REQUEST" <<'PY'
import json
import sys
for member in json.loads(sys.argv[1])["members"]:
    print("  serial {serial}  {disk}  {disk_size} bytes  {model}".format(
        serial=member["serial"], disk=member["disk"],
        disk_size=member["disk_size"], model=member["model"] or "-",
    ))
PY
confirm_exact "Start removing $VDEV from rpool on $DW_HOST? ZFS copies its data onto the other vdevs in the background; its disks can be pulled only after hosts/list_disks_ready_for_physically_removal.sh says so."

REMOVAL_ID="$(dw_state record-removal --request "$REQUEST")" ||
  dw_die "could not record the removal on $DW_HOST; nothing was removed"
if ! REMOVE_OUTPUT="$(dw_on_host zpool remove rpool "$VDEV" 2>&1)"; then
  dw_state set-removal "$REMOVAL_ID" --state failed \
    --note "zpool remove failed: ${REMOVE_OUTPUT//$'\n'/ }" >/dev/null || true
  dw_die "zpool remove rpool $VDEV failed: $REMOVE_OUTPUT"
fi

dw_collect_layout "$LAYOUT"
dw_section "Removal started"
printf '%s\n' "$(dw_json "$LAYOUT" 'd["pool"]["remove"] or "The removal already finished."')"
printf '\n%s has been marked for removal from rpool on %s. ZFS is copying its data\n' "$VDEV" "$DW_HOST"
printf 'onto the other vdevs. Run\n  hosts/list_disks_ready_for_physically_removal.sh --host %s\n' "$DW_HOST"
printf 'later to see when its disks are safe to pull. To remove another vdev, run this\n'
printf 'script again after this removal completes.\n'
