#!/usr/bin/env bash

# Copyright (c) 2026 BEENTHERE VENTURES, INC.
# SPDX-License-Identifier: GPL-3.0-only

# Workstation helpers shared by guests/prod/change_prod_vm_placement.sh and
# guests/prod/change_prod_vm_owner.sh: cluster discovery through one
# coordinator, production selection, live HA/replication inspection, stale
# registry-owner repair, the production orchestration lease, and replication
# health waits. Callers define REPO_ROOT and source lib/config.sh first.

# Callers read and set many of these globals.
# shellcheck disable=SC2034

PROD_REMOTE_ROOT="/usr/local/lib/app-ha-proxmox"
PROD_REMOTE_REGISTRY="${PROD_REMOTE_ROOT}/lib/cluster_registry.py"

CURRENT_PHASE="startup"
COORDINATOR=""
RUN_DIR=""
LEASE_NONCE=""
LEASE_ACQUIRED=false
LEASE_OWNER_LABEL="prod-ha"

RESOURCE_NAME=""
RESOURCE_REVISION=""
RESOURCE_STATE=""
VMID=""
VM_MEMORY_MB=""
REGISTRY_OWNER=""
OWNER_NODE=""
ROOT_VOLUME=""
HA_RULE_ID=""
declare -a CLUSTER_NODES=()
declare -a ONLINE_NODES=()
declare -a PLACEMENT_NODES=()
declare -a REGISTRY_HA_NODES=()
declare -a REGISTRY_TARGETS=()
declare -a RULE_NODES=()
declare -a STAGING_DEPENDENTS=()
declare -a REPLICATION_JOBS=()
declare -A REPLICATION_TARGET=()
REPLICATION_SCHEDULE=""

log() {
  printf '\n==> %s\n' "$*"
}

info() {
  printf '    %s\n' "$*"
}

warn() {
  printf 'WARNING: %s\n' "$*" >&2
}

die() {
  printf '\nERROR: %s\n' "$*" >&2
  exit 1
}

prompt_with_default() {
  local destination="$1" prompt="$2" default="$3" entered
  IFS= read -r -p "${prompt} [${default}]: " entered ||
    die "Input ended before a value was entered"
  printf -v "$destination" '%s' "${entered:-$default}"
}

prompt_yes() {
  local answer
  IFS= read -r -p "$1 [y/N] " answer || return 1
  [[ "${answer,,}" == y || "${answer,,}" == yes ]]
}

confirm_go() {
  local entered
  printf '%s\nType GO to continue.\n> ' "$1"
  IFS= read -r entered || entered=""
  [[ "$entered" == GO ]] ||
    die "Confirmation did not match GO; no change was made"
}

validate_positive_integer() {
  [[ "$2" =~ ^[1-9][0-9]*$ ]] ||
    die "$1 must be a positive integer"
}

list_contains() {
  local wanted="$1" item
  shift
  for item in "$@"; do
    [[ "$item" != "$wanted" ]] || return 0
  done
  return 1
}

join_csv() {
  local IFS=,
  printf '%s' "$*"
}

# Print the moxN names in "$@" sorted by slot number, one per line.
sort_nodes() {
  (($# > 0)) || return 0
  printf '%s\n' "$@" | sort -t x -k 2 -n
}

prod_load_config() {
  CURRENT_PHASE="loading cluster configuration"
  local name
  load_proxmox_config --no-secrets ||
    die "Could not load cluster configuration"
  for name in PROXMOX_CLUSTER_NAME MAX_MOX_HOSTS CLUSTER_STATE_DIR \
    PROD_VM_STORAGE PROXMOX_INTERNAL_DOMAIN; do
    require_var "$name" || die "Configuration is incomplete"
  done
  [[ "$PROD_VM_STORAGE" == local-zfs ]] ||
    die "Production root disks must use local-zfs"
  [[ "$CLUSTER_STATE_DIR" =~ ^/[A-Za-z0-9._/-]+$ ]] ||
    die "CLUSTER_STATE_DIR must be a safe absolute path"
}

prod_preflight_workstation() {
  local label="$1" command_name
  CURRENT_PHASE="validating workstation"
  for command_name in awk bash date mktemp python3 sort ssh; do
    command -v "$command_name" >/dev/null 2>&1 ||
      die "Required workstation command is unavailable: $command_name"
  done
  RUN_DIR="$(mktemp -d "${TMPDIR:-/tmp}/${label}.XXXXXX")"
  chmod 0700 "$RUN_DIR"
}

prod_release_lease_quietly() {
  if [[ "$LEASE_ACQUIRED" == true && -n "$COORDINATOR" ]]; then
    registry_cmd orchestration-release "$RESOURCE_NAME" \
      --nonce "$LEASE_NONCE" >/dev/null 2>&1 ||
      warn "Could not release the orchestration lease; it expires on its own"
    LEASE_ACQUIRED=false
  fi
  [[ -z "$RUN_DIR" ]] || rm -rf -- "$RUN_DIR"
  RUN_DIR=""
}

# Execute one argv array on a mox node. Non-coordinator nodes are reached
# through Proxmox root cluster SSH trust without constructing an eval string.
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
  mox_ssh "$COORDINATOR" "$PROD_REMOTE_REGISTRY" \
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

json_revision() {
  python3 -c 'import json,sys; print(json.loads(sys.argv[1])["revision"])' "$1"
}

parse_cluster_nodes() {
  local path="$1" maximum="$2" mode="$3"
  python3 - "$path" "$maximum" "$mode" <<'PY'
import json
import re
import sys

rows = json.load(open(sys.argv[1], encoding="utf-8"))
if not isinstance(rows, list):
    raise SystemExit("pvesh /nodes did not return an array")
maximum = int(sys.argv[2])
mode = sys.argv[3]
seen = set()
selected = []
for row in rows:
    node = row.get("node", row.get("name"))
    match = re.fullmatch(r"mox([1-9]|10)", str(node))
    if not match or int(match.group(1)) > maximum:
        raise SystemExit(f"unexpected cluster node: {node!r}")
    if node in seen:
        raise SystemExit(f"duplicate cluster node: {node}")
    seen.add(node)
    online = (
        str(row.get("status", "")).lower() == "online"
        or row.get("online") in (1, True)
    )
    if mode == "all" or online:
        selected.append(node)
selected.sort(key=lambda value: int(value[3:]))
if not selected:
    raise SystemExit("no matching mox nodes")
print(*selected, sep="\n")
PY
}

display_production_resources() {
  python3 - "$1" <<'PY'
import json
import sys

rows = json.load(open(sys.argv[1], encoding="utf-8"))
production = [row for row in rows if row.get("kind") == "production"]
production.sort(key=lambda row: row.get("index", 0))
if not production:
    raise SystemExit("registry contains no production resources")
for row in production:
    staging = sum(
        1 for other in rows
        if other.get("kind") == "staging" and other.get("source") == row.get("name")
    )
    print(
        row.get("name"),
        row.get("purpose", {}).get("slug", "?"),
        row.get("domains", {}).get("primary", "?"),
        ",".join(row.get("placement", [])),
        row.get("state"),
        row.get("owner_node") or "unknown",
        staging,
        sep="\t",
    )
PY
}

prod_discover_cluster() {
  CURRENT_PHASE="discovering the cluster"
  local configured_nodes online_nodes
  COORDINATOR="$(first_reachable_mox)" ||
    die "No reachable mox coordinator was found"
  info "Coordinator: $COORDINATOR"
  # shellcheck disable=SC2016 # This loop is intentionally evaluated remotely.
  node_exec "$COORDINATOR" bash -c \
    'for command in ha-manager pvesh pvesm pvesr qm zfs zpool; do command -v "$command" >/dev/null; done' ||
    die "Coordinator lacks required Proxmox/ZFS commands"

  write_json "${RUN_DIR}/cluster-status.json" "$(pvesh_get /cluster/status)"
  python3 - "${RUN_DIR}/cluster-status.json" "$PROXMOX_CLUSTER_NAME" <<'PY' ||
import json
import sys
rows = json.load(open(sys.argv[1], encoding="utf-8"))
clusters = [row for row in rows if row.get("type") == "cluster"]
if (
    len(clusters) != 1
    or clusters[0].get("name") != sys.argv[2]
    or clusters[0].get("quorate") not in (1, True, "1")
):
    raise SystemExit("cluster identity or quorum differs from configuration")
PY
    die "Cluster identity or quorum validation failed"

  write_json "${RUN_DIR}/nodes.json" "$(pvesh_get /nodes)"
  configured_nodes="$(
    parse_cluster_nodes "${RUN_DIR}/nodes.json" "$MAX_MOX_HOSTS" all
  )" || die "Could not validate configured cluster nodes"
  online_nodes="$(
    parse_cluster_nodes "${RUN_DIR}/nodes.json" "$MAX_MOX_HOSTS" online
  )" || die "Could not validate online cluster nodes"
  mapfile -t CLUSTER_NODES <<<"$configured_nodes"
  mapfile -t ONLINE_NODES <<<"$online_nodes"
  info "Cluster members: ${CLUSTER_NODES[*]} (online: ${ONLINE_NODES[*]})"

  write_json "${RUN_DIR}/resources.json" "$(registry_cmd list)"
  printf '\nRegistered production resources:\n'
  printf '  %-8s %-16s %-28s %-24s %-9s %-8s %s\n' \
    NAME PURPOSE DOMAIN PLACEMENT STATE OWNER STAGING
  local name purpose domain placement state owner staging
  while IFS=$'\t' read -r name purpose domain placement state owner staging; do
    printf '  %-8s %-16s %-28s %-24s %-9s %-8s %s\n' \
      "$name" "$purpose" "$domain" "$placement" "$state" "$owner" "$staging"
  done < <(display_production_resources "${RUN_DIR}/resources.json") ||
    die "Could not list registered production resources"
}

parse_selected_production() {
  local fields_path="${RUN_DIR}/selected.fields"
  python3 - "${RUN_DIR}/resources.json" "$1" "$PROD_VM_STORAGE" \
    >"$fields_path" <<'PY' || return 1
import json
import re
import sys

rows = json.load(open(sys.argv[1], encoding="utf-8"))
name, storage = sys.argv[2], sys.argv[3]
if not re.fullmatch(r"prod[1-9][0-9]*", name):
    raise SystemExit("selection must use the prodN formula")
matches = [row for row in rows if row.get("name") == name]
if len(matches) != 1 or matches[0].get("kind") != "production":
    raise SystemExit(f"{name} is not one registered production resource")
row = matches[0]
if row.get("state") != "active":
    raise SystemExit(f"{name} must be active (registry state: {row.get('state')})")
placement = row.get("placement")
if not isinstance(placement, list) or not placement:
    raise SystemExit("production has no placement")
volume = row["proxmox"].get("volume_id")
if not isinstance(volume, str) or not volume.startswith(storage + ":"):
    raise SystemExit("production has no registered root volume on production storage")
staging = sorted(
    other["name"]
    for other in rows
    if other.get("kind") == "staging" and other.get("source") == name
)
values = (
    row["name"],
    row["vmid"],
    row.get("owner_node") or "",
    row["revision"],
    row["state"],
    volume,
    ",".join(placement),
    ",".join(row["proxmox"].get("ha_nodes", [])),
    ",".join(row["proxmox"].get("replication_targets", [])),
    ",".join(staging),
    row["spec"].get("memory_mb", ""),
)
for value in values:
    sys.stdout.buffer.write(str(value).encode() + b"\0")
PY
  local -a fields=()
  mapfile -d '' -t fields <"$fields_path"
  ((${#fields[@]} == 11)) || return 1
  RESOURCE_NAME="${fields[0]}"
  VMID="${fields[1]}"
  REGISTRY_OWNER="${fields[2]}"
  RESOURCE_REVISION="${fields[3]}"
  RESOURCE_STATE="${fields[4]}"
  ROOT_VOLUME="${fields[5]}"
  IFS=',' read -r -a PLACEMENT_NODES <<<"${fields[6]}"
  IFS=',' read -r -a REGISTRY_HA_NODES <<<"${fields[7]}"
  IFS=',' read -r -a REGISTRY_TARGETS <<<"${fields[8]}"
  IFS=',' read -r -a STAGING_DEPENDENTS <<<"${fields[9]}"
  VM_MEMORY_MB="${fields[10]}"
}

prod_select_production() {
  local prompt="$1" default selection
  CURRENT_PHASE="selecting the production VM"
  default="$(
    python3 - "${RUN_DIR}/resources.json" <<'PY'
import json
import sys
rows = [
    row
    for row in json.load(open(sys.argv[1], encoding="utf-8"))
    if row.get("kind") == "production" and row.get("state") == "active"
]
rows.sort(key=lambda row: row.get("index", 0))
if rows:
    print(rows[0]["name"])
PY
  )"
  [[ -n "$default" ]] || die "No active production VM is registered"
  prompt_with_default selection "$prompt" "$default"
  parse_selected_production "$selection" ||
    die "Invalid production VM selection"
}

prod_refuse_staging_dependents() {
  ((${#STAGING_DEPENDENTS[@]} == 0)) && return 0
  die "$RESOURCE_NAME has staging VMs derived from it (${STAGING_DEPENDENTS[*]}); destroy them with guests/staging/destroy_staging_vm.sh first"
}

# Read live HA and replication state for the selected VM into OWNER_NODE,
# HA_RULE_ID, RULE_NODES, REPLICATION_JOBS, REPLICATION_TARGET, and
# REPLICATION_SCHEDULE. Only identity and the HA contract are enforced here;
# callers decide how far the rule and jobs may differ from placement.
prod_load_live_state() {
  CURRENT_PHASE="reading live production, HA, and replication state"
  write_json "${RUN_DIR}/cluster-vms.json" "$(
    pvesh_get /cluster/resources --type vm
  )"
  write_json "${RUN_DIR}/ha-resources.json" "$(
    pvesh_get /cluster/ha/resources
  )"
  write_json "${RUN_DIR}/ha-rules.json" "$(
    node_exec "$COORDINATOR" ha-manager rules config --output-format json
  )"
  write_json "${RUN_DIR}/replication.json" "$(
    pvesh_get /cluster/replication
  )"
  local parsed
  parsed="$(
    python3 - \
      "${RUN_DIR}/cluster-vms.json" "${RUN_DIR}/ha-resources.json" \
      "${RUN_DIR}/ha-rules.json" "${RUN_DIR}/replication.json" \
      "$RESOURCE_NAME" "$VMID" <<'PY'
import json
import re
import sys

vm_path, ha_path, rules_path, replication_path = sys.argv[1:5]
name, vmid_text = sys.argv[5:]
vmid = int(vmid_text)


def load(path):
    return json.load(open(path, encoding="utf-8"))


def values(raw):
    return raw if isinstance(raw, list) else [
        part for part in str(raw or "").split(",") if part
    ]


matches = [row for row in load(vm_path) if row.get("vmid") in (vmid, str(vmid))]
if len(matches) != 1:
    raise SystemExit("production VMID is not unique in live cluster resources")
vm = matches[0]
if vm.get("name") != name or vm.get("type") != "qemu" or vm.get("status") != "running":
    raise SystemExit("production must be one running QEMU VM")
owner = vm["node"]

ha = [row for row in load(ha_path) if row.get("sid") == f"vm:{vmid}"]
if len(ha) != 1:
    raise SystemExit("production is not exactly one Proxmox HA resource")
if str(ha[0].get("state", "started")).lower() not in {"started", "enabled"}:
    raise SystemExit("production HA requested state is not started")
if int(ha[0].get("failback", -1)) != 0 or int(ha[0].get("auto-rebalance", -1)) != 0:
    raise SystemExit("production HA failback/auto-rebalance policy differs from contract")

rules = [
    rule for rule in load(rules_path)
    if f"vm:{vmid}" in values(rule.get("resources"))
]
if len(rules) != 1:
    raise SystemExit("production needs exactly one HA node-affinity rule")
rule = rules[0]
priorities = {}
for part in values(rule.get("nodes")):
    node, separator, priority = str(part).partition(":")
    priorities[node] = int(priority) if separator else 0
if (
    rule.get("type") not in (None, "node-affinity")
    or set(values(rule.get("resources"))) != {f"vm:{vmid}"}
    or rule.get("strict", 0) not in (1, True, "1")
    or rule.get("disable", 0) in (1, True, "1")
    or rule.get("affinity", "positive") != "positive"
    or set(priorities.values()) - {1}
):
    raise SystemExit("production HA rule is not one strict positive node-affinity rule with priority 1")
if owner not in priorities:
    raise SystemExit(f"live owner {owner} is outside the HA rule nodes")

jobs = []
for row in load(replication_path):
    if row.get("guest") not in (vmid, str(vmid)):
        continue
    if row.get("disable") in (1, True, "1"):
        raise SystemExit(f"replication job {row.get('id')} is disabled")
    job = str(row.get("id", ""))
    target = str(row.get("target", ""))
    if not re.fullmatch(r"[1-9][0-9]{2,8}-[0-9]+", job) or not re.fullmatch(r"mox([1-9]|10)", target):
        raise SystemExit("replication job id or target is invalid")
    jobs.append((job, target, str(row.get("schedule", ""))))
targets = [target for _, target, _ in jobs]
if len(set(targets)) != len(targets):
    raise SystemExit("more than one replication job targets the same node")
if owner in targets:
    raise SystemExit("a replication job targets the live owner")
print(owner)
print(rule.get("rule", rule.get("id")))
print(",".join(sorted(priorities, key=lambda node: int(node[3:]))))
for job, target, schedule in sorted(jobs):
    print(f"{job}\t{target}\t{schedule}")
PY
  )" || die "Live production/HA/replication validation failed"
  local -a lines=()
  mapfile -t lines <<<"$parsed"
  OWNER_NODE="${lines[0]}"
  HA_RULE_ID="${lines[1]}"
  IFS=',' read -r -a RULE_NODES <<<"${lines[2]}"
  [[ "$OWNER_NODE" =~ ^mox([1-9]|10)$ ]] || die "Live owner is invalid"
  [[ "$HA_RULE_ID" =~ ^[A-Za-z0-9][A-Za-z0-9_.-]*$ ]] || die "HA rule ID is invalid"
  list_contains "$OWNER_NODE" "${PLACEMENT_NODES[@]}" ||
    die "Live owner $OWNER_NODE is outside registered placement ${PLACEMENT_NODES[*]}"
  REPLICATION_JOBS=()
  REPLICATION_TARGET=()
  REPLICATION_SCHEDULE=""
  local line job target schedule
  for line in "${lines[@]:3}"; do
    [[ -n "$line" ]] || continue
    IFS=$'\t' read -r job target schedule <<<"$line"
    REPLICATION_JOBS+=("$job")
    REPLICATION_TARGET["$job"]="$target"
    [[ -n "$REPLICATION_SCHEDULE" ]] || REPLICATION_SCHEDULE="$schedule"
  done
}

prod_job_for_target() {
  local target="$1" job
  for job in "${REPLICATION_JOBS[@]}"; do
    if [[ "${REPLICATION_TARGET[$job]}" == "$target" ]]; then
      printf '%s\n' "$job"
      return 0
    fi
  done
  return 1
}

prod_show_live_state() {
  local job
  info "Registry placement: ${PLACEMENT_NODES[*]}"
  info "Live HA owner: $OWNER_NODE"
  info "HA rule $HA_RULE_ID nodes: ${RULE_NODES[*]}"
  if ((${#REPLICATION_JOBS[@]} > 0)); then
    for job in "${REPLICATION_JOBS[@]}"; do
      info "Replication $job -> ${REPLICATION_TARGET[$job]}"
    done
  else
    info "Replication: none"
  fi
}

# Update a stale registry owner from live HA state, as extend_prod_vm_disk.sh
# does.
prod_reconcile_registry_owner() {
  [[ "$REGISTRY_OWNER" != "$OWNER_NODE" ]] || return 0
  CURRENT_PHASE="updating the stale production registry owner"
  local updated
  updated="$(registry_cmd update "$RESOURCE_NAME" \
    --expected-revision "$RESOURCE_REVISION" \
    --owner-node "$OWNER_NODE")" ||
    die "Could not update the registry owner of $RESOURCE_NAME to $OWNER_NODE"
  RESOURCE_REVISION="$(json_revision "$updated")"
  info "Registry owner of $RESOURCE_NAME updated from ${REGISTRY_OWNER:-unknown} to $OWNER_NODE"
  REGISTRY_OWNER="$OWNER_NODE"
}

registry_update_resource() {
  local updated
  updated="$(registry_cmd update "$RESOURCE_NAME" \
    --expected-revision "$RESOURCE_REVISION" "$@")" ||
    die "Registry update of $RESOURCE_NAME failed"
  RESOURCE_REVISION="$(json_revision "$updated")"
}

prod_acquire_lease() {
  CURRENT_PHASE="acquiring the production orchestration lease"
  local metadata contract
  metadata="$(registry_cmd orchestration-get "$RESOURCE_NAME")" ||
    die "Could not read production orchestration metadata"
  contract="$(
    python3 - "$metadata" <<'PY'
import json
import sys
value = json.loads(sys.argv[1])
if value.get("record_type") != "orchestration":
    raise SystemExit("production has no durable orchestration record")
if value.get("install_phase") != "network-finalized":
    raise SystemExit("production installation is not finalized")
for key in ("final_network_enabled", "source_iso_sha256", "install_mode"):
    if key not in value:
        raise SystemExit(f"orchestration record lacks {key}")
print(
    "yes" if value["final_network_enabled"] is True else "no",
    value.get("startup_sha256") or "none",
    value["source_iso_sha256"],
    value["install_mode"],
)
PY
  )" || die "Production orchestration metadata is not leasable"
  local network startup iso mode
  read -r network startup iso mode <<<"$contract"
  LEASE_NONCE="$(python3 -c 'import uuid; print(uuid.uuid4().hex)')"
  registry_cmd orchestration-acquire "$RESOURCE_NAME" \
    --nonce "$LEASE_NONCE" \
    --owner "${LEASE_OWNER_LABEL}-${COORDINATOR}-$$" \
    --ttl-seconds 14400 \
    --final-network-enabled "$network" \
    --startup-sha256 "$startup" \
    --source-iso-sha256 "$iso" \
    --install-mode "$mode" >/dev/null ||
    die "Could not acquire the production orchestration lease (another operation may hold it)"
  LEASE_ACQUIRED=true
}

prod_release_lease() {
  CURRENT_PHASE="releasing the orchestration lease"
  registry_cmd orchestration-release "$RESOURCE_NAME" \
    --nonce "$LEASE_NONCE" >/dev/null ||
    die "Could not release the orchestration lease"
  LEASE_ACQUIRED=false
}

live_owner_now() {
  local rows
  rows="$(pvesh_get /cluster/resources --type vm)" || return 1
  python3 - "$rows" "$RESOURCE_NAME" "$VMID" <<'PY'
import json
import sys
rows = json.loads(sys.argv[1])
wanted = int(sys.argv[3])
matches = [row for row in rows if row.get("vmid") in (wanted, str(wanted))]
if (
    len(matches) != 1
    or matches[0].get("name") != sys.argv[2]
    or matches[0].get("status") != "running"
):
    raise SystemExit(1)
print(matches[0].get("node", ""))
PY
}

prod_require_unchanged() {
  local observed registry_json revision
  observed="$(live_owner_now)" ||
    die "$RESOURCE_NAME is no longer one running VM"
  [[ "$observed" == "$OWNER_NODE" ]] ||
    die "$RESOURCE_NAME moved from $OWNER_NODE to $observed; rerun the script"
  registry_json="$(registry_cmd get "$RESOURCE_NAME")" ||
    die "Could not re-read the registry record"
  revision="$(json_revision "$registry_json")"
  [[ "$revision" == "$RESOURCE_REVISION" ]] ||
    die "The registry record of $RESOURCE_NAME changed concurrently; rerun the script"
}

# Print idle, busy, failing, or unknown for JOB on SOURCE, followed by the
# job's last_sync value.
replication_job_state() {
  local source="$1" job="$2" status
  status="$(pvesh_get "/nodes/${source}/replication/${job}/status" 2>/dev/null)" ||
    status=""
  python3 - "$status" "$job" 2>/dev/null <<'PY' || printf 'unknown 0\n'
import json
import sys
row = json.loads(sys.argv[1])
if row.get("id") != sys.argv[2]:
    raise SystemExit(1)
last = row.get("last_sync", 0)
last = last if isinstance(last, int) and last >= 0 else 0
if row.get("pid"):
    state = "busy"
elif int(row.get("fail_count", 0)) != 0 or row.get("error"):
    state = "failing"
else:
    state = "idle"
print(state, last)
PY
}

# Wait for JOB on SOURCE to finish a successful sync newer than BASELINE,
# requesting runs while it is idle or failing.
wait_for_replication() {
  local source="$1" job="$2" target="$3" baseline="$4" timeout="$5"
  local deadline=$((SECONDS + timeout)) state last requested_at=-1000
  local -a result=()
  while ((SECONDS < deadline)); do
    read -r -a result <<<"$(replication_job_state "$source" "$job")"
    state="${result[0]:-unknown}"
    last="${result[1]:-0}"
    if [[ "$state" == idle ]] && ((last > baseline)); then
      info "Replication $job to $target is healthy"
      return 0
    fi
    if [[ "$state" != busy ]] && ((SECONDS - requested_at >= 30)); then
      # pvesr refuses while another sync holds the guest lock; retry later.
      if node_exec "$source" pvesr schedule-now "$job" >/dev/null 2>&1; then
        info "Requested replication $job to $target"
      fi
      requested_at=$SECONDS
    fi
    sleep 10
  done
  die "Timed out after ${timeout}s waiting for a successful replication $job to $target"
}

# Require every job to be healthy now (last_sync set, no failures, idle or
# busy with a prior success).
prod_require_replication_healthy() {
  local source="$1" job state last
  local -a result=()
  for job in "${REPLICATION_JOBS[@]}"; do
    read -r -a result <<<"$(replication_job_state "$source" "$job")"
    state="${result[0]:-unknown}"
    last="${result[1]:-0}"
    if [[ "$state" != idle && "$state" != busy ]] || ((last <= 0)); then
      die "Replication job $job to ${REPLICATION_TARGET[$job]} is not currently healthy ($state)"
    fi
  done
}

# Set the HA node-affinity rule to exactly the given nodes.
set_ha_rule_nodes() {
  local affinity="" node
  for node in "$@"; do
    affinity+="${affinity:+,}${node}:1"
  done
  node_exec "$COORDINATOR" ha-manager rules set node-affinity "$HA_RULE_ID" \
    --nodes "$affinity" ||
    die "Could not set HA rule $HA_RULE_ID to $*"
  RULE_NODES=("$@")
  info "HA rule $HA_RULE_ID now allows $*"
}
