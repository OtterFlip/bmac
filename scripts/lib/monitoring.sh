#!/usr/bin/env bash

# Copyright (c) 2026 BEENTHERE VENTURES, INC.
# SPDX-License-Identifier: GPL-3.0-only

# BMAC monitoring, from the workstation: build the cluster's monitoring
# inventory, keep its Healthchecks.io checks in step with membership, render
# every node's agent config, and install the monitor agent on every Proxmox
# host, the QDevice, and every active production VM. See
# docs/MONITORING_AND_ALERTING.md.
#
# The caller sources config.sh and ui_protocol.sh and has loaded the
# configuration. Entry points:
#
#   monitoring_enabled                  MONITORING_ENABLED=true
#   monitoring_sync COORD [OPTIONS]     the whole idempotent sync (see below)
#   monitoring_membership_sync COORD [OPTIONS]
#                                       monitoring_sync for membership
#                                       workflows: a failure is reported as a
#                                       warning with a next step, never fatal
#   monitoring_require_qdevice          stop unless the QDevice is reachable
#   monitoring_disable_everywhere COORD [--dry-run] [--yes]
#                                       stop agents after monitoring is turned
#                                       off, and offer to delete the checks
#   monitoring_inventory COORD FILE     write the inventory JSON
#   monitoring_hc ARGS...               healthchecks.py with the API key
#   monitoring_exec ROLE TARGET ARGV... run ARGV as root on one node
#
# The API key is read in a subshell and handed to healthchecks.py through
# its environment only; it never appears in argv or output. Ping URLs exist
# only in the 0600 files under the sync's private run directory and in each
# node's /etc/app-ha-monitor/config.json.

MONITORING_LIB_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
MONITORING_AGENT_SOURCE="${MONITORING_LIB_DIR}/../host_runtime/app_ha_monitor.py"
MONITORING_HOST_PROGRAM=/usr/local/lib/app-ha-proxmox/lib/app_ha_monitor.py
MONITORING_NODE_PROGRAM=/usr/local/lib/app-ha-monitor/app_ha_monitor.py
MONITORING_REGISTRY=/usr/local/lib/app-ha-proxmox/lib/cluster_registry.py
MONITORING_UNIT=app-ha-monitor.service
# Seconds to wait for every node's check to receive a ping after an install.
MONITORING_PING_TIMEOUT="${MONITORING_PING_TIMEOUT:-240}"
MONITORING_PING_INTERVAL="${MONITORING_PING_INTERVAL:-10}"

mon_say() { printf '    %s\n' "$*"; }
mon_warn() { printf 'WARNING: %s\n' "$*" >&2; }
mon_fail() {
  printf 'ERROR: %s\n' "$*" >&2
  exit 1
}

monitoring_enabled() {
  [[ "${MONITORING_ENABLED:-false}" == true ]]
}

monitoring_tag() {
  python3 -c 'import re, sys; print("bmac-cluster-" + re.sub(r"[^a-z0-9-]+", "-", sys.argv[1].lower()).strip("-"))' \
    "$PROXMOX_CLUSTER_NAME"
}

monitoring_hc() {
  local api_url="${HEALTHCHECKS_API_URL:-https://healthchecks.io}"
  # Tests point the client at a local fake API; cluster.conf allows https only.
  [[ "${APP_HA_CONFIG_TEST_MODE:-0}" != 1 || -z "${BMAC_TEST_HEALTHCHECKS_API_URL:-}" ]] ||
    api_url="$BMAC_TEST_HEALTHCHECKS_API_URL"
  (
    load_proxmox_config --require-secrets >/dev/null || exit 2
    [[ -n "${HEALTHCHECKS_API_KEY:-}" ]] ||
      mon_fail "HEALTHCHECKS_API_KEY is not set in config/secrets.env"
    HEALTHCHECKS_API_KEY="$HEALTHCHECKS_API_KEY" HEALTHCHECKS_API_URL="$api_url" \
      python3 "${MONITORING_LIB_DIR}/healthchecks.py" "$@" </dev/null
  )
}

mon_qdevice_ssh_options=(-o BatchMode=yes -o StrictHostKeyChecking=yes -o CheckHostIP=yes -o ConnectTimeout=10)
mon_guest_ssh_options=(-o BatchMode=yes -o ConnectTimeout=10)

monitoring_exec() {
  local role="$1" target="$2" command_line
  shift 2
  printf -v command_line '%q ' "$@"
  # shellcheck disable=SC2029 # The command line is %q-quoted for the remote shell.
  case "$role" in
    host) mox_ssh "$target" "$@" </dev/null ;;
    qdevice) ssh "${mon_qdevice_ssh_options[@]}" "root@${target}" "$command_line" </dev/null ;;
    guest) ssh "${mon_guest_ssh_options[@]}" "$target" "$command_line" </dev/null ;;
    *) return 2 ;;
  esac
}

mon_describe_target() {
  case "$1" in
    host) printf 'ssh %s' "$2" ;;
    qdevice) printf 'ssh root@%s' "$2" ;;
    guest) printf 'ssh %s, set up by setup_jump_ssh_access.sh' "$2" ;;
  esac
}

mon_copy() {
  local role="$1" target="$2" source="$3" destination="$4"
  case "$role" in
    host)
      _mox_ssh_options "$target" || return
      scp -q "${MOX_SSH_OPTIONS[@]}" -- "$source" \
        "${MOX_SSH_USER:-root}@$(_mox_ssh_destination "$target"):${destination}"
      ;;
    qdevice) scp -q "${mon_qdevice_ssh_options[@]}" -- "$source" "root@${target}:${destination}" ;;
    guest) scp -q "${mon_guest_ssh_options[@]}" -- "$source" "${target}:${destination}" ;;
    *) return 2 ;;
  esac
}

# Returns 1, with the reason on stderr, when monitoring is enabled and the
# QDevice (the cluster's external monitor) is not reachable.
monitoring_require_qdevice() {
  monitoring_enabled || return 0
  [[ -n "${PROXMOX_QDEVICE_HOST:-}" ]] || {
    mon_warn "MONITORING_ENABLED=true requires PROXMOX_QDEVICE_HOST in cluster.conf"
    return 1
  }
  monitoring_exec qdevice "$PROXMOX_QDEVICE_HOST" true >/dev/null 2>&1 && return 0
  printf 'ERROR: %s\n' "Monitoring is enabled, but ssh root@${PROXMOX_QDEVICE_HOST} fails. The QDevice runs the cluster's external monitor; fix SSH to it (see docs/QDEVICE_MANUAL_SETUP.md) or set MONITORING_ENABLED=false, then rerun." >&2
  return 1
}

# Write the monitoring inventory: hosts with node IDs, the QDevice's
# registration, and every production record with its probe.
monitoring_inventory() {
  local coordinator="$1" output="$2" work status=0
  work="$(dirname -- "$output")"
  mox_ssh "$coordinator" pvesh get /cluster/config/nodes --output-format json \
    </dev/null >"${work}/config-nodes.json" ||
    { mon_warn "Could not read cluster nodes through $coordinator"; return 1; }
  mox_ssh "$coordinator" grep -Eq '^[[:space:]]*device[[:space:]]*[{]' /etc/pve/corosync.conf \
    </dev/null || status=$?
  ((status <= 1)) || { mon_warn "Could not read /etc/pve/corosync.conf through $coordinator"; return 1; }
  if ! mox_ssh "$coordinator" "$MONITORING_REGISTRY" --state-dir "$CLUSTER_STATE_DIR" \
    monitoring-probes </dev/null >"${work}/productions.json" 2>/dev/null; then
    # A registry from before monitoring: no probes yet.
    mox_ssh "$coordinator" "$MONITORING_REGISTRY" --state-dir "$CLUSTER_STATE_DIR" \
      list --kind production </dev/null >"${work}/productions.json" ||
      { mon_warn "Could not list production VMs from the registry"; return 1; }
  fi
  python3 - "${work}/config-nodes.json" "${work}/productions.json" "$((status == 0))" \
    "$PROXMOX_CLUSTER_NAME" "${PROXMOX_QDEVICE_HOST:-qdevice}" "${TAILSCALE_TAG:-tag:proxmox-host}" \
    "$output" <<'PY'
import json
import sys

nodes_path, productions_path, registered, cluster, qdevice_host, tag, output = sys.argv[1:]
hosts = []
for row in json.load(open(nodes_path, encoding="utf-8")):
    name = row.get("name", row.get("node"))
    nodeid = str(row.get("nodeid", ""))
    if name and nodeid.isdigit():
        hosts.append({"name": name, "nodeid": int(nodeid)})
productions = []
for row in json.load(open(productions_path, encoding="utf-8")):
    if row.get("kind", "production") != "production":
        continue
    probe = row.get("probe") or (row.get("monitoring") or {}).get("probe") or {"enabled": False}
    domains = row.get("domains") or {}
    productions.append({
        "name": row["name"], "state": row.get("state"),
        "primary": row.get("primary", domains.get("primary")),
        "aliases": row.get("aliases", domains.get("aliases", [])),
        "probe": probe,
    })
inventory = {
    "cluster": cluster,
    "hosts": sorted(hosts, key=lambda host: int(host["name"][3:])),
    "qdevice": {"host": qdevice_host, "registered": registered == "1"},
    "host_tag": tag,
    "productions": productions,
}
with open(output, "w", encoding="utf-8") as handle:
    json.dump(inventory, handle, indent=2)
PY
}

# NODE ROLE TARGET lines for the inventory.
mon_nodes() {
  python3 "${MONITORING_LIB_DIR}/monitor_config.py" nodes --inventory "$1"
}

mon_program_for() {
  if [[ "$1" == host ]]; then
    printf '%s\n' "$MONITORING_HOST_PROGRAM"
  else
    printf '%s\n' "$MONITORING_NODE_PROGRAM"
  fi
}

# Stage one node's bundle and install it. Prints the remote stage path on
# success (the caller keeps it for rollback).
mon_install_node() {
  local node="$1" role="$2" target="$3" run_dir="$4" stage program
  program="$(mon_program_for "$role")"
  stage="$(monitoring_exec "$role" "$target" mktemp -d /run/app-ha-monitor-stage.XXXXXX)" ||
    { mon_warn "$node: could not create a staging directory"; return 1; }
  [[ "$stage" =~ ^/run/app-ha-monitor-stage[.][A-Za-z0-9]+$ ]] ||
    { mon_warn "$node: unexpected staging path"; return 1; }
  printf '%s\t%s\t%s\t%s\n' "$node" "$role" "$target" "$stage" >>"${run_dir}/staged.tsv"
  mon_copy "$role" "$target" "${run_dir}/configs/${node}.json" "${stage}/config.json" &&
    mon_copy "$role" "$target" "${run_dir}/unit-${role}" "${stage}/${MONITORING_UNIT}" &&
    mon_copy "$role" "$target" "${MONITORING_LIB_DIR}/monitor_node_install.sh" "${stage}/monitor_node_install.sh" &&
    mon_copy "$role" "$target" "${MONITORING_LIB_DIR}/apt_lock_wait.sh" "${stage}/apt_lock_wait.sh" ||
    { mon_warn "$node: could not copy the monitoring bundle"; return 1; }
  if [[ "$role" != host ]]; then
    mon_copy "$role" "$target" "$MONITORING_AGENT_SOURCE" "${stage}/app_ha_monitor.py" ||
      { mon_warn "$node: could not copy the agent"; return 1; }
  fi
  monitoring_exec "$role" "$target" bash "${stage}/monitor_node_install.sh" install \
    "$stage" "$role" "$program" >"${run_dir}/install-${node}.out" 2>&1 || {
    sed 's/^/      /' "${run_dir}/install-${node}.out" >&2
    mon_warn "$node: installing the monitor agent failed"
    return 1
  }
  if grep -q '^__MONITOR_INSTALL__ restarted' "${run_dir}/install-${node}.out"; then
    mon_say "$node: agent installed and (re)started"
  else
    mon_say "$node: config updated (the agent re-reads it every cycle)"
  fi
}

mon_rollback_phase() {
  local run_dir="$1" phase="$2" node role target stage failed=0
  [[ -f "${run_dir}/staged.tsv" ]] || return 0
  while IFS=$'\t' read -r node role target stage; do
    [[ "$role" == "$phase" ]] || continue
    if monitoring_exec "$role" "$target" bash "${stage}/monitor_node_install.sh" rollback \
      "$stage" "$(mon_program_for "$role")" >/dev/null 2>&1; then
      mon_say "$node: restored its previous monitor agent, unit, and config"
    else
      mon_warn "$node: could not restore its previous monitor files; check it by hand"
      failed=1
    fi
  done < <(awk '{ lines[NR] = $0 } END { for (i = NR; i > 0; i--) print lines[i] }' "${run_dir}/staged.tsv")
  return "$failed"
}

mon_cleanup_stages() {
  local run_dir="$1" node role target stage
  [[ -f "${run_dir}/staged.tsv" ]] || return 0
  while IFS=$'\t' read -r node role target stage; do
    monitoring_exec "$role" "$target" rm -rf -- "$stage" >/dev/null 2>&1 || true
  done <"${run_dir}/staged.tsv"
  : >"${run_dir}/staged.tsv"
}

mon_print_plan() {
  local plan="$1"
  python3 - "$plan" <<'PY'
import json
import sys

plan = json.load(open(sys.argv[1], encoding="utf-8"))
labels = (("created", "create"), ("updated", "update"), ("unchanged", "keep"),
          ("obsolete", "obsolete"), ("deleted", "delete"), ("refused", "REFUSED"))
for key, label in labels:
    for name in plan.get(key, []):
        print(f"      {label:<9} {name}")
budget = plan.get("budget", {})
print(f"      check budget: {budget.get('used', '?')} of {budget.get('max', '?')} checks in the Healthchecks project")
PY
  if bmac_ui_is_json; then
    bmac_ui_plan_begin "Healthchecks checks" "Checks for every monitored node of cluster ${PROXMOX_CLUSTER_NAME}."
    local kind name
    while IFS=$'\t' read -r kind name; do
      bmac_ui_plan_item "$kind" "$name" "Healthchecks check"
    done < <(python3 -c '
import json, sys
plan = json.load(open(sys.argv[1]))
for key, kind in (("created", "create"), ("updated", "update"), ("unchanged", "keep"), ("obsolete", "keep"), ("deleted", "remove"), ("refused", "check")):
    for name in plan.get(key, []):
        print(f"{kind}\t{name}")
' "$plan")
    bmac_ui_plan_end
  fi
}

# monitoring_sync COORD [--dry-run] [--delete-node NODE]... [--delete-obsolete]
#   [--no-verify]
# With MONITORING_PLAN_OUT set, the Healthchecks plan (created, updated,
# unchanged, obsolete, refused, budget) is also written there as JSON.
# Bring every node's monitoring in line with the cluster. Phases (hosts, the
# QDevice, production VMs) each roll back the nodes they changed when one of
# their nodes fails. Checks created by a failed run are left in place; the
# next run reuses them.
monitoring_sync() (
  set -Eeuo pipefail
  local coordinator="$1" dry_run=false delete_obsolete=false verify=true
  local -a delete_nodes=()
  shift
  while (($#)); do
    case "$1" in
      --dry-run) dry_run=true ;;
      --delete-obsolete) delete_obsolete=true ;;
      --delete-node) delete_nodes+=("$2"); shift ;;
      --no-verify) verify=false ;;
      *) mon_fail "unknown monitoring_sync option: $1" ;;
    esac
    shift
  done
  local run_dir tag node role target started phase failed
  run_dir="$(mktemp -d "${TMPDIR:-/tmp}/bmac-monitoring.XXXXXX")" || mon_fail "no temporary directory"
  chmod 0700 "$run_dir"
  install -d -m 0700 "${run_dir}/configs"
  : >"${run_dir}/staged.tsv"
  trap 'mon_cleanup_stages "$run_dir"; rm -rf -- "$run_dir"' EXIT
  tag="$(monitoring_tag)"

  monitoring_inventory "$coordinator" "${run_dir}/inventory.json" ||
    mon_fail "Could not build the monitoring inventory"
  mon_nodes "${run_dir}/inventory.json" >"${run_dir}/nodes.tsv" ||
    mon_fail "The monitoring inventory is invalid"

  printf '\n==> Monitoring preflight\n'
  while IFS=$'\t' read -r node role target; do
    monitoring_exec "$role" "$target" true >/dev/null 2>&1 ||
      mon_fail "$node is not reachable over SSH ($(mon_describe_target "$role" "$target")); monitoring changes nothing until every monitored node is reachable"
  done <"${run_dir}/nodes.tsv"
  mon_say "Every monitored node is reachable: $(cut -f1 "${run_dir}/nodes.tsv" | tr '\n' ' ')"
  monitoring_hc status --tag "$tag" >"${run_dir}/status.json" ||
    mon_fail "The Healthchecks API rejected the request; check HEALTHCHECKS_API_KEY"

  python3 "${MONITORING_LIB_DIR}/monitor_config.py" desired --inventory "${run_dir}/inventory.json" \
    >"${run_dir}/desired.json" || mon_fail "Could not render the desired checks"
  local -a sync_args=(sync --desired "${run_dir}/desired.json" --tag "$tag"
    --max-checks "${MONITOR_MAX_CHECKS:-20}" --out "${run_dir}/pings.json")
  [[ "$dry_run" == false ]] || sync_args+=(--dry-run)
  [[ "$delete_obsolete" == false || "$dry_run" == true ]] || sync_args+=(--delete-obsolete)
  printf '\n==> Healthchecks checks%s\n' "$([[ $dry_run == true ]] && echo ' (dry run)')"
  local sync_status=0
  monitoring_hc "${sync_args[@]}" >"${run_dir}/plan.json" || sync_status=$?
  ((sync_status == 0 || sync_status == 2)) || mon_fail "Synchronizing the Healthchecks checks failed"
  mon_print_plan "${run_dir}/plan.json"
  [[ -z "${MONITORING_PLAN_OUT:-}" ]] || cp -- "${run_dir}/plan.json" "$MONITORING_PLAN_OUT"
  ((sync_status == 0)) ||
    mon_fail "The Healthchecks project has no room for every node's check (MONITOR_MAX_CHECKS=${MONITOR_MAX_CHECKS:-20}); the refused nodes are listed above"

  if ((${#delete_nodes[@]})); then
    local -a names=()
    for node in "${delete_nodes[@]}"; do
      names+=(--name "${PROXMOX_CLUSTER_NAME} bmaccheck ${node}")
    done
    if [[ "$dry_run" == true ]]; then
      mon_say "Would delete the checks of: ${delete_nodes[*]}"
    else
      monitoring_hc delete --tag "$tag" "${names[@]}" >/dev/null ||
        mon_fail "Could not delete the checks of: ${delete_nodes[*]}"
      mon_say "Deleted the checks of: ${delete_nodes[*]}"
    fi
  fi

  if [[ "$dry_run" == true ]]; then
    printf '\n==> Monitor agents (dry run)\n'
    while IFS=$'\t' read -r node role target; do
      mon_say "$node ($role): would install or update the agent, its unit, and its config"
    done <"${run_dir}/nodes.tsv"
    exit 0
  fi

  python3 "${MONITORING_LIB_DIR}/monitor_config.py" render --inventory "${run_dir}/inventory.json" \
    --pings "${run_dir}/pings.json" --out-dir "${run_dir}/configs" >/dev/null ||
    mon_fail "Could not render every node's agent config"
  for role in host qdevice guest; do
    python3 "$MONITORING_AGENT_SOURCE" print-unit --role "$role" --program "$(mon_program_for "$role")" \
      >"${run_dir}/unit-${role}" || mon_fail "Could not render the $role unit"
  done

  started="$(date -u +%Y-%m-%dT%H:%M:%SZ)"
  for phase in host qdevice guest; do
    printf '\n==> Monitor agents: %s\n' "$(case $phase in host) echo "Proxmox hosts" ;; qdevice) echo "the QDevice" ;; guest) echo "production VMs" ;; esac)"
    failed=""
    while IFS=$'\t' read -r node role target; do
      [[ "$role" == "$phase" ]] || continue
      mon_install_node "$node" "$role" "$target" "$run_dir" || { failed="$node"; break; }
    done <"${run_dir}/nodes.tsv"
    if [[ -n "$failed" ]]; then
      mon_warn "Monitoring update failed on $failed; restoring this phase's nodes"
      mon_rollback_phase "$run_dir" "$phase" || true
      mon_fail "Monitoring was not updated on $failed. Checks created by this run are kept and reused by the next run. Fix the problem above, then run update_cluster_runtime.sh."
    fi
    mon_cleanup_stages "$run_dir"
  done

  [[ "$verify" == true ]] || exit 0
  printf '\n==> Waiting for every node'"'"'s first ping (up to %ss)\n' "$MONITORING_PING_TIMEOUT"
  local -a names=()
  while IFS=$'\t' read -r node role target; do
    names+=(--name "${PROXMOX_CLUSTER_NAME} bmaccheck ${node}")
  done <"${run_dir}/nodes.tsv"
  local wait_status=0
  monitoring_hc wait-ping --tag "$tag" "${names[@]}" --since "$started" \
    --timeout "$MONITORING_PING_TIMEOUT" --interval "$MONITORING_PING_INTERVAL" \
    >"${run_dir}/pings-seen.json" || wait_status=$?
  python3 - "${run_dir}/pings-seen.json" <<'PY' || true
import json
import sys

for name, result in json.load(open(sys.argv[1], encoding="utf-8")).items():
    mark = "ok" if result["ok"] else "NO PING"
    print(f"      {mark:<8} {name}: {result['status']}, last ping {result['last_ping'] or 'never'}")
PY
  ((wait_status == 0)) ||
    mon_fail "Some nodes' checks received no ping after the install (listed above); a check in state 'new' never alerts. Inspect them with show_monitoring_state.sh."

  printf '\n==> Checks on every node\n'
  while IFS=$'\t' read -r node role target; do
    if monitoring_exec "$role" "$target" python3 -I "$(mon_program_for "$role")" check --once --json \
      >"${run_dir}/check-${node}.json" 2>/dev/null; then
      python3 - "$node" "${run_dir}/check-${node}.json" <<'PY' || true
import json
import sys

node, path = sys.argv[1:]
report = json.load(open(path, encoding="utf-8"))
failing = [row for row in report["results"] if row["ok"] is False]
print(f"      {node}: {len(report['results'])} checked, {len(failing)} failing, {len(report['errors'])} check errors")
for row in failing:
    print(f"        [{row['priority']}] {row['key']}: {row['message']}")
for error in report["errors"]:
    print(f"        agent:{error['check']}: {error['error']}")
PY
    else
      mon_warn "$node: app_ha_monitor.py check --once failed"
    fi
  done <"${run_dir}/nodes.tsv"
)

monitoring_membership_sync() {
  monitoring_enabled || return 0
  printf '\n==> Updating monitoring for the new cluster membership\n'
  bmac_ui_step "Update monitoring"
  if monitoring_sync "$@"; then
    return 0
  fi
  mon_warn "The main operation completed, but monitoring could not be updated."
  bmac_ui_warning "Monitoring could not be updated; run update_cluster_runtime.sh to finish updating monitoring."
  bmac_ui_next_step "Run update_cluster_runtime.sh to finish updating monitoring." \
    --command "scripts/user_callable/hosts/update_cluster_runtime.sh" --workflow update_cluster_runtime
  return 0
}

# After MONITORING_ENABLED is set to false: stop and disable every installed
# agent, then offer (typed confirmation) to delete this cluster's checks.
monitoring_disable_everywhere() (
  set -Eeuo pipefail
  local coordinator="$1" dry_run=false assume_yes=false run_dir node role target found=0 answer
  shift
  while (($#)); do
    case "$1" in
      --dry-run) dry_run=true ;;
      --yes) assume_yes=true ;;
    esac
    shift
  done
  run_dir="$(mktemp -d "${TMPDIR:-/tmp}/bmac-monitoring.XXXXXX")" || exit 1
  trap 'rm -rf -- "$run_dir"' EXIT
  monitoring_inventory "$coordinator" "${run_dir}/inventory.json" || mon_fail "Could not build the monitoring inventory"
  mon_nodes "${run_dir}/inventory.json" >"${run_dir}/nodes.tsv" || mon_fail "The monitoring inventory is invalid"
  # Hosts tell whether monitoring was ever installed; without it nothing runs.
  while IFS=$'\t' read -r node role target; do
    [[ "$role" == host ]] || continue
    monitoring_exec host "$target" test -f "/etc/systemd/system/${MONITORING_UNIT}" 2>/dev/null && found=1
  done <"${run_dir}/nodes.tsv"
  ((found)) || exit 0
  printf '\n==> Monitoring is disabled (MONITORING_ENABLED=false): stopping the monitor agents\n'
  while IFS=$'\t' read -r node role target; do
    if [[ "$dry_run" == true ]]; then
      mon_say "$node: would stop and disable ${MONITORING_UNIT}"
      continue
    fi
    if monitoring_exec "$role" "$target" bash -c \
      'if [[ -f /etc/systemd/system/app-ha-monitor.service ]]; then systemctl disable --now --quiet app-ha-monitor.service; echo stopped; else echo absent; fi' \
      >"${run_dir}/disable.out" 2>&1; then
      mon_say "$node: monitor agent $(tail -n 1 "${run_dir}/disable.out")"
    else
      mon_warn "$node: could not stop the monitor agent (unreachable?); its check will report it down"
    fi
  done <"${run_dir}/nodes.tsv"
  [[ "$dry_run" == false ]] || { mon_say "Would offer to delete this cluster's Healthchecks checks"; exit 0; }
  if ! monitoring_hc status --tag "$(monitoring_tag)" >"${run_dir}/status.json" 2>/dev/null; then
    mon_warn "The Healthchecks API is not usable (no HEALTHCHECKS_API_KEY?); this cluster's checks were left in place and will report the nodes as down. Delete them in the Healthchecks UI."
    exit 0
  fi
  mapfile -t names < <(python3 -c 'import json, sys; [print(c["name"]) for c in json.load(open(sys.argv[1]))["checks"]]' "${run_dir}/status.json")
  ((${#names[@]})) || exit 0
  printf '\nThis cluster has %s Healthchecks checks:\n' "${#names[@]}"
  printf '    %s\n' "${names[@]}"
  printf 'With the agents stopped, these checks will report every node as DOWN.\n'
  if [[ "$assume_yes" != true ]]; then
    if bmac_ui_is_json; then
      if ! bmac_ui_confirm --id delete_checks --severity destructive --question \
        --title "Delete this cluster's Healthchecks checks?" \
        --message "Monitoring is off and the agents are stopped. Deleting the ${#names[@]} checks stops their DOWN alerts; keeping them leaves them reporting every node as down." \
        --confirm-label "Delete the checks" --cancel-label "Keep them" --text "DELETE CHECKS"; then
        mon_say "Kept the checks; they will report the nodes as down"
        exit 0
      fi
    else
      printf 'Type DELETE CHECKS to delete them, or anything else to keep them.\n> '
      IFS= read -r answer || answer=""
      [[ "$answer" == "DELETE CHECKS" ]] || { mon_say "Kept the checks; they will report the nodes as down"; exit 0; }
    fi
  fi
  local -a arguments=()
  for node in "${names[@]}"; do arguments+=(--name "$node"); done
  monitoring_hc delete --tag "$(monitoring_tag)" "${arguments[@]}" >/dev/null ||
    mon_fail "Could not delete the checks"
  mon_say "Deleted ${#names[@]} checks"
)
