#!/usr/bin/env bash

# Copyright (c) 2026 BEENTHERE VENTURES, INC.
# SPDX-License-Identifier: GPL-3.0-only

# Purge one dead host that was never gracefully removed and has been
# physically disconnected from the cluster for good. Staging VMs that depend
# on it are destroyed, production placement, HA rules, and replication drop
# it, the QDevice vote is reconciled, the host is deleted from corosync and
# pmxcfs, and its registry slot is freed.

set -Eeuo pipefail
set +x
umask 077

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
REPO_ROOT="$(cd -- "${SCRIPT_DIR}/.." && pwd -P)"
CONFIG_LIB="${REPO_ROOT}/lib/config.sh"
CONTROL_LIB="${REPO_ROOT}/lib/cluster_control.sh"
MEMBERSHIP_LIB="${REPO_ROOT}/lib/host_membership.sh"

for library in "$CONFIG_LIB" "$CONTROL_LIB" "$MEMBERSHIP_LIB"; do
  [[ -f "$library" && ! -L "$library" ]] || {
    printf 'ERROR: Missing library: %s\n' "$library" >&2
    exit 1
  }
done
# shellcheck source=../lib/config.sh
source "$CONFIG_LIB"
# shellcheck source=../lib/cluster_control.sh
source "$CONTROL_LIB"
# shellcheck source=../lib/host_membership.sh
source "$MEMBERSHIP_LIB"

STAGING_CLEANUP_TIMEOUT_SECONDS=1200

CURRENT_PHASE="startup"
TARGET_HOST=""
TARGET_IS_MEMBER=false
NEW_CONTROL_NODE=""
CONTROL_CHANGED=false
CLUSTER_CHANGED=false
RUN_DIR=""
declare -a MEMBERS=()
declare -a ONLINE_MEMBERS=()
declare -a STAGING_NAMES=()
declare -a PRODUCTION_NAMES=()

usage() {
  cat <<'EOF'
Usage: purge_host_from_cluster.sh [--host moxN]

Run from an administrator workstation. Purges one dead cluster host that can
no longer be removed gracefully with hosts/remove_host_from_cluster.sh. The
purpose of this script is to enable the removal of a cluster host that is no
longer functioning and has been physically disconnected from the cluster,
permanently. The purged machine must never be allowed to communicate with the
cluster via the network in any way after it has been purged from the cluster.

Every other member must be online and the cluster must be quorate. HA must
already have recovered every production VM that ran on the dead host. The
script then:
  - destroys staging VMs on the dead host or derived from production VMs that
    used it, abandoning cleanup that only the dead host could perform;
  - removes the dead host from each production VM's registry placement, HA
    node-affinity rule, and replication jobs (a production VM may be left with
    a single placement host; add hosts back with
    guests/prod/change_prod_vm_placement.sh);
  - reconciles the QDevice vote, deletes the host from corosync and pmxcfs,
    removes its cluster SSH trust, and frees its registry slot.
A host slot that was reserved but never joined can be purged the same way.
When the dead host is the cluster control node, you choose its replacement.

Options:
  --host moxN   Host to purge. Prompted when omitted.
  -h, --help
EOF
}

parse_args() {
  while (($#)); do
    case "$1" in
      --host)
        (($# >= 2)) || die "--host requires a value"
        TARGET_HOST="$2"
        shift 2
        ;;
      -h | --help)
        usage
        exit 0
        ;;
      *)
        die "Unknown option: $1"
        ;;
    esac
  done
}

cleanup() {
  local code=$?
  trap - EXIT
  set +e
  hm_release_control_plane_lock
  if ((code != 0)); then
    printf '\nFailed while %s.\n' "$CURRENT_PHASE" >&2
    if [[ "$CONTROL_CHANGED" == true ]]; then
      printf 'The registry already records %s as the cluster control node.\n' \
        "$NEW_CONTROL_NODE" >&2
      printf 'Set PROXMOX_CONTROL_NODE=%s in %s before rerunning.\n' \
        "$NEW_CONTROL_NODE" "$PROXMOX_CLUSTER_CONFIG" >&2
    fi
    if [[ "$CLUSTER_CHANGED" == true ]]; then
      printf 'The purge of %s is partly complete. Correct the fault and rerun this\n' \
        "$TARGET_HOST" >&2
      printf 'script for %s; completed steps are skipped.\n' "$TARGET_HOST" >&2
    fi
  fi
  [[ -z "$RUN_DIR" ]] || rm -rf -- "$RUN_DIR"
  exit "$code"
}

print_purge_warning() {
  cat <<EOF

=============================================================================
  PURGE A DEAD HOST FROM CLUSTER ${PROXMOX_CLUSTER_NAME}
=============================================================================
The purpose of this script is to enable the removal of a cluster host that is
no longer functioning and has been physically disconnected from the cluster,
permanently.

The purged machine must never be allowed to communicate with the cluster via
the network in any way after it has been purged from the cluster. Before it is
ever connected to any network again, wipe its disks or reinstall it.

To remove a healthy host, use hosts/remove_host_from_cluster.sh instead.
=============================================================================
EOF
}

load_member_states() {
  local states node state
  states="$(hm_member_states)" || die "Could not list cluster members"
  MEMBERS=()
  ONLINE_MEMBERS=()
  while read -r node state; do
    [[ -n "$node" ]] || continue
    MEMBERS+=("$node")
    [[ "$state" != online ]] || ONLINE_MEMBERS+=("$node")
  done <<<"$states"
}

choose_coordinator() {
  local node
  HM_COORDINATOR=""
  if control_list_contains "$CONTROL_NODE" "${CONTROL_ONLINE_NODES[@]}"; then
    HM_COORDINATOR="$CONTROL_NODE"
    return 0
  fi
  for node in "${CONTROL_ONLINE_NODES[@]}"; do
    if mox_is_reachable "$node"; then
      HM_COORDINATOR="$node"
      return 0
    fi
  done
  die "No online cluster member is reachable"
}

# Print "NODE DESCRIPTION" for every host that this script can purge.
purge_candidates() {
  local slots directories
  slots="$(hm_registry host-list)" || die "Could not list registry host slots"
  directories="$(
    hm_exec "$HM_COORDINATOR" find /etc/pve/nodes -mindepth 1 -maxdepth 1 \
      -type d -printf '%f\n'
  )" || die "Could not list /etc/pve/nodes"
  python3 - "$slots" "$directories" "${MEMBERS[*]}" "${ONLINE_MEMBERS[*]}" <<'PY'
import json
import re
import sys

slots = {row["node"]: row["state"] for row in json.loads(sys.argv[1])}
directories = {
    name for name in sys.argv[2].split() if re.fullmatch(r"mox([1-9]|10)", name)
}
members = set(sys.argv[3].split())
online = set(sys.argv[4].split())
candidates = (set(slots) | directories | members) - online
for node in sorted(candidates, key=lambda value: int(value[3:])):
    if node in members:
        description = "offline cluster member"
    elif node in directories:
        description = "not a member; /etc/pve/nodes/%s remains" % node
    elif slots.get(node) == "joining":
        description = "reserved slot that never finished joining"
    else:
        description = "registry slot recorded for a host that is no longer a member"
    print(node, description)
PY
}

choose_target() {
  CURRENT_PHASE="choosing the host to purge"
  local candidates node description
  candidates="$(purge_candidates)"
  [[ -n "$candidates" ]] ||
    die "No offline member, leftover node directory, or stale slot exists; nothing can be purged"
  printf '\nHosts that can be purged:\n'
  while read -r node description; do
    printf '  %-6s %s\n' "$node" "$description"
  done <<<"$candidates"
  if [[ -z "$TARGET_HOST" ]]; then
    IFS= read -r -p "Host to purge: " TARGET_HOST ||
      die "Input ended before a host was chosen"
  fi
  hm_valid_node "$TARGET_HOST" ||
    die "Host must be mox1 through mox${MAX_MOX_HOSTS}: ${TARGET_HOST}"
  awk -v node="$TARGET_HOST" '$1 == node { found=1 } END { exit !found }' \
    <<<"$candidates" ||
    die "$TARGET_HOST is not offline or has nothing to purge. A healthy host is removed with hosts/remove_host_from_cluster.sh"
}

validate_cluster_shape() {
  CURRENT_PHASE="validating cluster membership and quorum"
  local status node
  TARGET_IS_MEMBER=false
  ! control_list_contains "$TARGET_HOST" "${ONLINE_MEMBERS[@]}" ||
    die "$TARGET_HOST is online; use hosts/remove_host_from_cluster.sh"
  if mox_is_reachable "$TARGET_HOST"; then
    die "$TARGET_HOST answers SSH from this workstation. Purge is only for a host that is permanently disconnected; physically disconnect it or use hosts/remove_host_from_cluster.sh"
  fi
  control_list_contains "$TARGET_HOST" "${MEMBERS[@]}" && TARGET_IS_MEMBER=true
  status="$(hm_cluster_status)" || die "Could not read cluster status"
  grep -Eq "^Name:[[:space:]]+${PROXMOX_CLUSTER_NAME}[[:space:]]*$" <<<"$status" ||
    die "$HM_COORDINATOR is not a member of cluster $PROXMOX_CLUSTER_NAME"
  hm_status_is_quorate "$status" ||
    die "The cluster is not quorate; restore quorum before purging $TARGET_HOST (if an earlier purge of a two-node cluster already removed the QDevice, run 'pvecm expected 1' on the survivor)"
  for node in "${MEMBERS[@]}"; do
    [[ "$node" != "$TARGET_HOST" ]] || continue
    control_list_contains "$node" "${ONLINE_MEMBERS[@]}" ||
      die "Every member except $TARGET_HOST must be online; $node is offline"
  done
  if [[ "$TARGET_IS_MEMBER" == true ]]; then
    ((${#MEMBERS[@]} >= 2)) ||
      die "$TARGET_HOST is the only cluster member"
  fi
}

# Analyze registry and live state. Writes the plan to RUN_DIR/plan.json and
# prints refusals, one per line, when the purge cannot proceed.
analyze() {
  CURRENT_PHASE="analyzing what depends on $TARGET_HOST"
  local resources cleanup vms rules replication
  resources="$(hm_registry list)" || die "Could not list registry resources"
  cleanup="$(hm_registry list --record-type cleanup)" ||
    die "Could not list deferred cleanup records"
  vms="$(hm_pvesh_get /cluster/resources --type vm)" ||
    die "Could not list cluster guests"
  rules="$(hm_exec "$HM_COORDINATOR" ha-manager rules config --output-format json)" ||
    die "Could not list HA rules"
  replication="$(hm_pvesh_get /cluster/replication)" ||
    die "Could not list replication jobs"
  printf '%s\n' "$resources" >"${RUN_DIR}/resources.json"
  printf '%s\n' "$cleanup" >"${RUN_DIR}/cleanup.json"
  printf '%s\n' "$vms" >"${RUN_DIR}/vms.json"
  printf '%s\n' "$rules" >"${RUN_DIR}/rules.json"
  printf '%s\n' "$replication" >"${RUN_DIR}/replication.json"
  build_plan "$RUN_DIR" "$TARGET_HOST" "${ONLINE_MEMBERS[*]}"
}

# build_plan DIR DEAD "ONLINE..." reads DIR/*.json, writes DIR/plan.json, and
# prints one refusal per line (nothing when the purge can proceed).
build_plan() {
  python3 - "$@" <<'PY'
import json
import re
import sys
from pathlib import Path

directory, dead, online_text = sys.argv[1:4]
root = Path(directory)
online = set(online_text.split())


def load(name):
    return json.loads((root / f"{name}.json").read_text(encoding="utf-8"))


def values(raw):
    return raw if isinstance(raw, list) else [
        part for part in str(raw or "").split(",") if part
    ]


resources = load("resources")
cleanup = load("cleanup")
vms = load("vms")
rules = load("rules")
replication = load("replication")
refusals = []
by_name = {row["name"]: row for row in resources}
live = {int(row["vmid"]): row for row in vms if "vmid" in row}
index = int(dead[3:])


def references(row):
    proxmox = row["proxmox"]
    snapshot = proxmox.get("snapshot") or {}
    return (
        dead in row["placement"]
        or dead == row.get("initial_node")
        or dead == row.get("owner_node")
        or dead in proxmox.get("ha_nodes", [])
        or dead in proxmox.get("replication_targets", [])
        or dead == snapshot.get("owner_node")
        or dead in (snapshot.get("guids") or {})
    )


productions = []
for row in sorted(
    (row for row in resources if row["kind"] == "production"),
    key=lambda row: row.get("index", 0),
):
    if not references(row):
        continue
    name, vmid = row["name"], int(row["vmid"])
    placement = [node for node in row["placement"] if node != dead]
    vm = live.get(vmid)
    if row["state"] not in {"active", "ready", "stopped"}:
        refusals.append(
            f"{name} is {row['state']}; finish or destroy it with the production scripts first"
        )
        continue
    if not placement:
        refusals.append(f"{name} is placed only on {dead}; destroy it with guests/prod/destroy_prod_vm.sh")
        continue
    if vm is None or vm.get("name") != name or vm.get("type") != "qemu":
        refusals.append(f"{name} (VMID {vmid}) is missing from live cluster resources")
        continue
    if vm.get("node") == dead:
        refusals.append(
            f"{name} is still on {dead}; wait for Proxmox HA to fence {dead} and recover it "
            f"onto {', '.join(placement)} (check 'ha-manager status')"
        )
        continue
    if vm.get("node") not in placement or vm.get("node") not in online:
        refusals.append(f"{name} runs on {vm.get('node')}, which is not an online placement host")
        continue
    sid = f"vm:{vmid}"
    rule_ids = [
        rule.get("rule", rule.get("id"))
        for rule in rules
        if sid in values(rule.get("resources"))
    ]
    if len(rule_ids) != 1:
        refusals.append(f"{name} must have exactly one HA rule (found {len(rule_ids)})")
        continue
    jobs = [
        str(job["id"])
        for job in replication
        if job.get("guest") in (vmid, str(vmid)) and job.get("target") == dead
    ]
    productions.append(
        {
            "name": name,
            "vmid": vmid,
            "revision": row["revision"],
            "owner": vm["node"],
            "registry_owner": row.get("owner_node"),
            "placement": placement,
            "ha_nodes": [node for node in row["proxmox"]["ha_nodes"] if node != dead],
            "replication_targets": [
                node
                for node in row["proxmox"]["replication_targets"]
                if node not in {dead, vm["node"]}
            ],
            "rule": rule_ids[0],
            "dead_jobs": jobs,
        }
    )

affected_sources = {
    row["name"]
    for row in resources
    if row["kind"] == "production" and references(row)
}
staging = []
for row in sorted(
    (row for row in resources if row["kind"] == "staging"),
    key=lambda row: row["name"],
):
    if not (references(row) or row["source"] in affected_sources):
        continue
    if row["state"] not in {
        "active", "ready", "stopping", "stopped", "failed", "cleanup_pending"
    }:
        refusals.append(
            f"staging {row['name']} is {row['state']}; finish or fail its creation first"
        )
        continue
    source = by_name.get(row["source"])
    if source is None:
        refusals.append(f"staging {row['name']} has no registered source")
        continue
    proxmox = row["proxmox"]
    staging.append(
        {
            "name": row["name"],
            "id": row["id"],
            "vmid": int(row["vmid"]),
            "state": row["state"],
            "revision": row["revision"],
            "node": row.get("owner_node") or row["placement"][0],
            "volume": proxmox.get("volume_id"),
            "snapshot": (proxmox.get("snapshot") or {}).get("name"),
            "source_vmid": int(source["vmid"]),
            "source_placement": source["placement"],
        }
    )

allowed_vmid = 9110 + index
guests = []
for row in vms:
    if row.get("node") != dead:
        continue
    vmid = int(row.get("vmid", 0))
    registered = any(int(record["vmid"]) == vmid for record in resources)
    if row.get("type") == "lxc" and vmid == allowed_vmid:
        continue
    if registered:
        continue
    guests.append(f"{vmid} ({row.get('name', '?')}, {row.get('type', '?')})")
if guests:
    refusals.append(
        f"unregistered guests are configured on {dead}: {', '.join(guests)}. Move each "
        f"config out of /etc/pve/nodes/{dead}/ (for example onto a host holding its "
        "replica) or delete it, then rerun"
    )

plan = {
    "productions": productions,
    "staging": staging,
    "abandoned_cleanup": sum(
        1 for record in cleanup if record["node"] == dead and record["state"] == "pending"
    ),
}
(root / "plan.json").write_text(json.dumps(plan, indent=2), encoding="utf-8")
for refusal in refusals:
    print(refusal)
PY
}

plan_field() {
  python3 - "${RUN_DIR}/plan.json" "$@" <<'PY'
import json
import sys

plan = json.load(open(sys.argv[1], encoding="utf-8"))
kind = sys.argv[2]
if kind == "names":
    for row in plan[sys.argv[3]]:
        print(row["name"])
elif kind == "abandoned":
    print(plan["abandoned_cleanup"])
else:
    row = next(row for row in plan[kind] if row["name"] == sys.argv[3])
    value = row[sys.argv[4]]
    print(" ".join(map(str, value)) if isinstance(value, list) else ("" if value is None else value))
PY
}

validate_plan() {
  local refusals
  refusals="$(analyze)"
  if [[ -n "$refusals" ]]; then
    printf '\n%s cannot be purged yet:\n' "$TARGET_HOST" >&2
    sed 's/^/  - /' <<<"$refusals" >&2
    exit 1
  fi
  mapfile -t PRODUCTION_NAMES < <(plan_field names productions)
  mapfile -t STAGING_NAMES < <(plan_field names staging)
}

show_plan() {
  local name remaining
  log "Purge plan"
  info "Host: $TARGET_HOST ($([[ "$TARGET_IS_MEMBER" == true ]] && printf 'offline cluster member' || printf 'not a cluster member'))"
  if [[ "$TARGET_IS_MEMBER" == true ]]; then
    remaining=$((${#MEMBERS[@]} - 1))
    info "Cluster: ${MEMBERS[*]} -> $remaining members"
    if ((remaining % 2 == 0)); then
      info "QDevice: present afterward ($remaining nodes + 1 QDevice vote)"
    else
      info "QDevice: absent afterward ($remaining votes)"
    fi
    ((remaining > 1)) ||
      info "A single remaining host cannot provide production HA; add hosts with hosts/setup_proxmox_host.sh"
  fi
  if ((${#STAGING_NAMES[@]} > 0)); then
    info "Staging VMs destroyed: ${STAGING_NAMES[*]}"
  else
    info "Staging VMs destroyed: none"
  fi
  for name in "${PRODUCTION_NAMES[@]}"; do
    info "Production $name: placement -> $(plan_field productions "$name" placement) (running on $(plan_field productions "$name" owner))"
  done
  ((${#PRODUCTION_NAMES[@]} > 0)) || info "Production VMs changed: none"
  info "Pending cleanup abandoned on $TARGET_HOST: $(plan_field abandoned)"
  if [[ -n "$NEW_CONTROL_NODE" ]]; then
    info "Control node: $TARGET_HOST -> $NEW_CONTROL_NODE"
  else
    info "Control node: $CONTROL_NODE (unchanged)"
  fi
}

choose_control_replacement() {
  [[ "$TARGET_HOST" == "$CONTROL_NODE" ]] || return 0
  local -a candidates=() node
  for node in "${ONLINE_MEMBERS[@]}"; do
    [[ "$node" == "$TARGET_HOST" ]] || candidates+=("$node")
  done
  hm_choose_new_control_node "$TARGET_HOST" NEW_CONTROL_NODE "${candidates[@]}"
}

switch_control_node() {
  [[ -n "$NEW_CONTROL_NODE" ]] || return 0
  CURRENT_PHASE="recording $NEW_CONTROL_NODE as the cluster control node"
  hm_registry host-sync --live >/dev/null ||
    die "Could not record current cluster membership in the registry"
  hm_registry control-set "$NEW_CONTROL_NODE" \
    --expected-node "${CONTROL_RECORDED_NODE:-none}" >/dev/null ||
    die "Could not record $NEW_CONTROL_NODE as the cluster control node"
  CONTROL_CHANGED=true
  CONTROL_NODE="$NEW_CONTROL_NODE"
  HM_COORDINATOR="$NEW_CONTROL_NODE"
  info "The registry records $NEW_CONTROL_NODE as the cluster control node"
}

registry_update_staging() {
  local name="$1" revision
  shift
  revision="$(
    hm_json_field "$(hm_registry get "$name")" revision
  )" || die "Could not read staging $name"
  hm_registry update "$name" --expected-revision "$revision" "$@" >/dev/null ||
    die "Could not update staging $name"
}

queue_staging_cleanup() {
  local name="$1" state node volume snapshot source_vmid placement member vmid
  state="$(plan_field staging "$name" state)"
  node="$(plan_field staging "$name" node)"
  vmid="$(plan_field staging "$name" vmid)"
  volume="$(plan_field staging "$name" volume)"
  snapshot="$(plan_field staging "$name" snapshot)"
  source_vmid="$(plan_field staging "$name" source_vmid)"
  read -r -a placement <<<"$(plan_field staging "$name" source_placement)"
  info "Committing destruction of staging $name (state $state, on $node)"
  case "$state" in
    active) registry_update_staging "$name" --state stopping --routes-disabled ;;
    ready | stopping | stopped | failed) registry_update_staging "$name" --routes-disabled ;;
    cleanup_pending) ;;
    *) die "Unsupported staging state for $name: $state" ;;
  esac
  local reason="purge of dead host ${TARGET_HOST}"
  hm_registry defer-cleanup --resource "$name" --node "$node" \
    --action destroy-vm --target "vm:${vmid}" --reason "$reason" >/dev/null ||
    die "Could not queue destruction of staging VM $name"
  if [[ -n "$volume" ]]; then
    hm_registry defer-cleanup --resource "$name" --node "$node" \
      --action destroy-volume --target "$volume" --reason "$reason" >/dev/null ||
      die "Could not queue destruction of the $name clone"
  fi
  if [[ -n "$snapshot" ]]; then
    for member in "${placement[@]}"; do
      hm_registry defer-cleanup --resource "$name" --node "$member" \
        --action delete-snapshot --target "vm-${source_vmid}@${snapshot}" \
        --reason "$reason" >/dev/null ||
        die "Could not queue snapshot cleanup for $name on $member"
    done
  fi
  local -A route_nodes=()
  for member in "${MEMBERS[@]}" "${placement[@]}"; do
    route_nodes["$member"]=1
  done
  for member in "${!route_nodes[@]}"; do
    hm_registry defer-cleanup --resource "$name" --node "$member" \
      --action remove-route --target "$name" --reason "$reason" >/dev/null ||
      die "Could not queue route cleanup for $name on $member"
  done
  [[ "$state" == cleanup_pending ]] ||
    registry_update_staging "$name" --state cleanup_pending --routes-disabled
}

# Mark every pending cleanup record on the dead host completed. Those actions
# could only run on that host, and its disks will never rejoin the cluster.
abandon_dead_host_cleanup() {
  local cleanup ids
  cleanup="$(hm_registry list --record-type cleanup)" ||
    die "Could not list deferred cleanup records"
  ids="$(
    python3 - "$cleanup" "$TARGET_HOST" <<'PY'
import json
import sys

dead = sys.argv[2]
ids = [
    record["id"]
    for record in json.loads(sys.argv[1])
    if record["node"] == dead and record["state"] == "pending"
]
if ids:
    print(json.dumps({"schema_version": 1, "cleanup_completed": ids}))
PY
  )" || die "Could not select cleanup records on $TARGET_HOST"
  [[ -n "$ids" ]] || return 0
  printf '%s\n' "$ids" |
    mox_ssh "$HM_COORDINATOR" "$HM_REMOTE_REGISTRY" --state-dir "$CLUSTER_STATE_DIR" \
      reconcile --observed - --apply >/dev/null ||
    die "Could not abandon cleanup records on $TARGET_HOST"
  info "Abandoned pending cleanup that only $TARGET_HOST could perform"
}

wait_for_staging_release() {
  local deadline=$((SECONDS + STAGING_CLEANUP_TIMEOUT_SECONDS)) node resources
  local remaining
  while ((SECONDS < deadline)); do
    abandon_dead_host_cleanup
    for node in "${ONLINE_MEMBERS[@]}"; do
      [[ "$node" != "$TARGET_HOST" ]] || continue
      hm_exec "$node" systemctl start --no-block app-ha-deferred-cleanup.service \
        >/dev/null 2>&1 || true
    done
    remaining=unknown
    if resources="$(hm_registry list 2>/dev/null)"; then
      remaining="$(
        python3 - "$resources" "${STAGING_NAMES[@]}" <<'PY' 2>/dev/null || printf unknown
import json
import sys

names = set(sys.argv[2:])
print(" ".join(sorted(row["name"] for row in json.loads(sys.argv[1]) if row["name"] in names)))
PY
      )"
    fi
    [[ -n "$remaining" ]] || return 0
    sleep 10
  done
  hm_registry list --record-type cleanup >&2 || true
  die "Timed out waiting for staging cleanup (${remaining}); the workers keep retrying, so rerun this script later"
}

destroy_dependent_staging() {
  ((${#STAGING_NAMES[@]} > 0)) || return 0
  CURRENT_PHASE="destroying staging VMs that depend on $TARGET_HOST"
  log "Destroying dependent staging VMs: ${STAGING_NAMES[*]}"
  CLUSTER_CHANGED=true
  local name
  for name in "${STAGING_NAMES[@]}"; do
    queue_staging_cleanup "$name"
  done
  mox_ssh "$HM_COORDINATOR" "$HM_REMOTE_HAPROXY_SYNC" --lock-timeout 240 \
    </dev/null >/dev/null ||
    die "Could not converge HAProxy after disabling staging routes"
  wait_for_staging_release
  info "Dependent staging VMs are destroyed and released"
}

narrow_production() {
  local name="$1" vmid owner registry_owner rule placement_text ha_text targets_text
  local -a placement=() ha_nodes=() targets=() jobs=()
  vmid="$(plan_field productions "$name" vmid)"
  owner="$(plan_field productions "$name" owner)"
  registry_owner="$(plan_field productions "$name" registry_owner)"
  rule="$(plan_field productions "$name" rule)"
  placement_text="$(plan_field productions "$name" placement)"
  ha_text="$(plan_field productions "$name" ha_nodes)"
  targets_text="$(plan_field productions "$name" replication_targets)"
  read -r -a placement <<<"$placement_text"
  read -r -a ha_nodes <<<"$ha_text"
  read -r -a targets <<<"$targets_text"
  read -r -a jobs <<<"$(plan_field productions "$name" dead_jobs)"
  log "Removing $TARGET_HOST from production $name"
  local affinity="" node
  for node in "${placement[@]}"; do
    affinity+="${affinity:+,}${node}:1"
  done
  hm_exec "$HM_COORDINATOR" ha-manager rules set node-affinity "$rule" \
    --nodes "$affinity" ||
    die "Could not narrow HA rule $rule of $name to ${placement[*]}"
  info "HA rule $rule now allows ${placement[*]}"
  local job
  for job in "${jobs[@]}"; do
    hm_exec "$HM_COORDINATOR" pvesr delete "$job" --force ||
      die "Could not remove replication job $job of $name"
    info "Removed replication job $job (its target $TARGET_HOST is gone)"
  done
  local revision current
  current="$(hm_registry get "$name")" || die "Could not read production $name"
  revision="$(hm_json_field "$current" revision)"
  if [[ "$registry_owner" != "$owner" ]]; then
    current="$(hm_registry update "$name" --expected-revision "$revision" \
      --owner-node "$owner")" ||
      die "Could not record live owner $owner for $name"
    revision="$(hm_json_field "$current" revision)"
    info "Registry owner of $name updated from ${registry_owner:-unknown} to $owner"
  fi
  local -a update_args=(
    --placement "$(IFS=,; printf '%s' "${placement[*]}")"
    --ha-nodes "$(IFS=,; printf '%s' "${ha_nodes[*]}")"
    --replication-targets "$(IFS=,; printf '%s' "${targets[*]}")"
  )
  hm_registry update "$name" --expected-revision "$revision" "${update_args[@]}" \
    >/dev/null || die "Could not remove $TARGET_HOST from the registry placement of $name"
  info "Registry placement of $name is now ${placement[*]}"
}

narrow_productions() {
  ((${#PRODUCTION_NAMES[@]} > 0)) || return 0
  CURRENT_PHASE="removing $TARGET_HOST from production placement"
  CLUSTER_CHANGED=true
  local name
  for name in "${PRODUCTION_NAMES[@]}"; do
    narrow_production "$name"
  done
}

# Remove the QDevice and delete the dead member. A two-node cluster keeps
# quorum only through the QDevice vote, so the survivor removes it and lowers
# expected votes in one command, well inside the HA watchdog window.
delete_dead_member() {
  [[ "$TARGET_IS_MEMBER" == true ]] || return 0
  CURRENT_PHASE="deleting $TARGET_HOST from the cluster"
  CLUSTER_CHANGED=true
  if hm_qdevice_configured; then
    if ((${#MEMBERS[@]} == 2)); then
      log "Removing the QDevice and setting expected votes to 1 on $HM_COORDINATOR"
      hm_exec "$HM_COORDINATOR" bash -c \
        'pvecm qdevice remove || true; pvecm expected 1' ||
        die "Could not remove the QDevice and lower expected votes on $HM_COORDINATOR"
      hm_qdevice_configured &&
        die "corosync.conf still configures the QDevice; remove it manually with 'pvecm qdevice remove' on $HM_COORDINATOR"
    else
      hm_remove_qdevice
    fi
  fi
  local status
  status="$(hm_cluster_status)" || die "Could not read cluster status"
  hm_status_is_quorate "$status" ||
    die "The cluster is not quorate after QDevice removal"
  hm_delete_cluster_node "$TARGET_HOST"
}

finish_cluster_cleanup() {
  CURRENT_PHASE="removing $TARGET_HOST from pmxcfs and cluster SSH trust"
  CLUSTER_CHANGED=true
  hm_remove_node_directory "$TARGET_HOST"
  hm_strip_node_ssh_trust "$TARGET_HOST"
  CURRENT_PHASE="reconciling the QDevice for the remaining members"
  load_member_states
  ((${#MEMBERS[@]} == ${#ONLINE_MEMBERS[@]})) ||
    die "Every remaining member must be online (members: ${MEMBERS[*]}; online: ${ONLINE_MEMBERS[*]})"
  hm_reconcile_qdevice "${#MEMBERS[@]}"
}

release_slot() {
  CURRENT_PHASE="freeing the $TARGET_HOST registry slot"
  hm_registry host-sync --live >/dev/null ||
    die "Could not record current cluster membership in the registry"
  hm_registry host-release "$TARGET_HOST" \
    --reason "purged with hosts/purge_host_from_cluster.sh" >/dev/null ||
    die "Could not free the $TARGET_HOST registry slot"
  info "The $TARGET_HOST slot is free for a future host"
}

main() {
  parse_args "$@"
  load_proxmox_config --no-secrets || die "Could not load cluster configuration"
  require_vars PROXMOX_CLUSTER_NAME MAX_MOX_HOSTS CLUSTER_STATE_DIR \
    PROXMOX_QDEVICE_HOST PROXMOX_INTERNAL_DOMAIN PRIVATE_SUBNET_PREFIX \
    MOX_IP_START_OCTET || die "Configuration is incomplete"
  local command_name
  for command_name in awk flock jq mktemp openssl python3 ssh tailscale; do
    command -v "$command_name" >/dev/null 2>&1 ||
      die "Required workstation command is unavailable: $command_name"
  done
  RUN_DIR="$(mktemp -d "${TMPDIR:-/tmp}/purge-host.XXXXXX")"
  chmod 0700 "$RUN_DIR"
  trap cleanup EXIT

  print_purge_warning
  hm_verify_qdevice_access

  CURRENT_PHASE="resolving the cluster control node"
  log "Cluster control node"
  resolve_control_node --allow-offline || exit 1
  [[ -n "$CONTROL_PROBE_NODE" ]] || die "No reachable cluster member was found"
  [[ "$CONTROL_REGISTRY_SUPPORTS_HOSTS" == true ]] ||
    die "The installed cluster registry predates host slots; run hosts/update_cluster_runtime.sh first"
  choose_coordinator
  info "Control node: $CONTROL_NODE; commands run through $HM_COORDINATOR"
  hm_require_slot_registry

  load_member_states
  choose_target
  validate_cluster_shape
  validate_plan
  choose_control_replacement
  show_plan
  print_purge_warning
  hm_confirm_phrase \
    "Purging permanently removes $TARGET_HOST from cluster $PROXMOX_CLUSTER_NAME and destroys the staging VMs listed above." \
    "PURGE ${TARGET_HOST}"

  switch_control_node
  CURRENT_PHASE="acquiring the cluster control-plane lock"
  hm_acquire_control_plane_lock "$HM_COORDINATOR"
  load_member_states
  validate_cluster_shape
  validate_plan

  destroy_dependent_staging
  narrow_productions
  delete_dead_member
  finish_cluster_cleanup
  release_slot
  mox_ssh "$HM_COORDINATOR" "$HM_REMOTE_HAPROXY_SYNC" --lock-timeout 240 \
    </dev/null >/dev/null ||
    warn "HAProxy route synchronization failed; run it later on $HM_COORDINATOR: $HM_REMOTE_HAPROXY_SYNC"
  hm_release_control_plane_lock
  hm_archive_host_artifacts "$TARGET_HOST"

  log "Host purge complete"
  info "$TARGET_HOST is no longer part of $PROXMOX_CLUSTER_NAME"
  info "Members: ${MEMBERS[*]}"
  local name
  for name in "${PRODUCTION_NAMES[@]}"; do
    info "Production $name placement: $(plan_field productions "$name" placement)"
  done
  ((${#PRODUCTION_NAMES[@]} == 0)) ||
    info "Restore redundancy with guests/prod/change_prod_vm_placement.sh"
  if [[ "$CONTROL_CHANGED" == true ]]; then
    control_offer_cluster_conf_update "$NEW_CONTROL_NODE"
  fi
  hm_print_reinstall_follow_ups "$TARGET_HOST"
  printf '\nREMEMBER: %s must never be allowed to communicate with the cluster via the\n' \
    "$TARGET_HOST"
  printf 'network in any way. Wipe its disks or reinstall it before it is connected to\n'
  printf 'any network again.\n'
}

if [[ "${PURGE_HOST_SOURCE_ONLY:-0}" != 1 ]]; then
  main "$@"
fi
