#!/usr/bin/env bash

# Copyright (c) 2026 BEENTHERE VENTURES, INC.
# SPDX-License-Identifier: GPL-3.0-only

# Read-only view of BMAC's monitoring: every monitored node's Healthchecks
# check (status and last ping) and its agent's open problems, this cluster's
# checks that no node uses, and the project's check budget. The one change it
# can make is --reset-smart-baseline, after confirmation.

set -Eeuo pipefail
set +x
umask 077

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
REPO_ROOT="$(cd -- "${SCRIPT_DIR}/../../.." && pwd -P)"
# shellcheck source=../../lib/ui_protocol.sh
source "${REPO_ROOT}/scripts/lib/ui_protocol.sh"
# shellcheck source=../../lib/config.sh
source "${REPO_ROOT}/scripts/lib/config.sh"
# shellcheck source=../../lib/monitoring.sh
source "${REPO_ROOT}/scripts/lib/monitoring.sh"
bmac_ui_bootstrap "$@"

RESET=""
RUN_DIR=""

die() {
  printf '\nERROR: %s\n' "$*" >&2
  bmac_ui_error failed "$*"
  exit 2
}

usage() {
  cat <<'EOF'
Usage: scripts/user_callable/diagnostics/show_monitoring_state.sh
         [--reset-smart-baseline moxN:SERIAL] [--json]

Show BMAC's monitoring (docs/MONITORING_AND_ALERTING.md):

  - every monitored node (Proxmox hosts, the QDevice, active production VMs)
    with its Healthchecks check status (up, down, new, grace, paused), its
    last ping, and its agent's open problems;
  - checks tagged for this cluster that no node uses, and nodes without one;
  - the check budget: checks in the Healthchecks project / MONITOR_MAX_CHECKS.

  --reset-smart-baseline moxN:SERIAL
      Accept a drive's current SMART counters (media errors, reallocated,
      pending, or uncorrectable sectors) as its new baseline, which resolves
      a smart:SERIAL problem whose counter rose. Asks first.

Exit status: 0 when nothing needs attention, 1 when something does, 2 for
usage or configuration errors.
EOF
}

parse_args() {
  while (($#)); do
    case "$1" in
      --reset-smart-baseline)
        (($# >= 2)) || die "--reset-smart-baseline needs moxN:SERIAL"
        RESET="$2"
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
}

reset_baseline() {
  local host="${RESET%%:*}" serial="${RESET#*:}" answer
  [[ "$host" =~ ^mox([1-9]|10)$ && "$serial" =~ ^[A-Za-z0-9._:/-]{1,128}$ && "$RESET" == *:* ]] ||
    die "--reset-smart-baseline needs moxN:SERIAL, such as mox1:PHLJ9252002S2P0BGN"
  printf 'This accepts the current SMART counters of drive %s on %s as its new baseline.\n' "$serial" "$host"
  printf 'Only do this after you have looked at the drive (and replaced it if needed).\n'
  if bmac_ui_is_json; then
    bmac_ui_confirm --id reset --title "Reset the SMART baseline of $serial on $host?" \
      --message "The agent accepts this drive's current counters, so the smart:$serial problem resolves on its next SMART check." \
      --confirm-label "Reset the baseline" || die "Cancelled; nothing was changed"
  else
    read -r -p "Reset it? [y/N] " answer
    [[ "$answer" =~ ^[Yy]([Ee][Ss])?$ ]] || die "Cancelled; nothing was changed"
  fi
  monitoring_exec host "$host" python3 -I "$MONITORING_HOST_PROGRAM" reset-smart-baseline "$serial" ||
    die "Could not request the reset on $host"
  bmac_ui_result reset "$RESET"
  exit 0
}

main() {
  parse_args "$@"
  load_proxmox_config --no-secrets >/dev/null || die "cluster configuration is invalid"
  if ! monitoring_enabled; then
    printf 'Monitoring is disabled (MONITORING_ENABLED=false in config/cluster.conf).\n'
    printf 'See docs/MONITORING_AND_ALERTING.md and the README section "Monitoring and Alerts".\n'
    bmac_ui_result enabled:bool false
    exit 0
  fi
  [[ -z "$RESET" ]] || reset_baseline

  RUN_DIR="$(mktemp -d "${TMPDIR:-/tmp}/show-monitoring.XXXXXX")"
  chmod 0700 "$RUN_DIR"
  trap 'rm -rf -- "$RUN_DIR"' EXIT
  local coordinator node role target
  bmac_ui_step "Read the cluster"
  coordinator="$(first_reachable_mox)" || die "No reachable Proxmox host was found"
  monitoring_inventory "$coordinator" "${RUN_DIR}/inventory.json" || die "Could not read the cluster"
  mon_nodes "${RUN_DIR}/inventory.json" >"${RUN_DIR}/nodes.tsv" || die "The cluster inventory is invalid"

  bmac_ui_step "Read the Healthchecks checks"
  if ! monitoring_hc status --tag "$(monitoring_tag)" >"${RUN_DIR}/checks.json"; then
    printf '{"error": "the Healthchecks API could not be read"}\n' >"${RUN_DIR}/checks.json"
  fi

  install -d -m 0700 "${RUN_DIR}/agents"
  while IFS=$'\t' read -r node role target; do
    bmac_ui_step "Read the agent on $node"
    monitoring_exec "$role" "$target" python3 -I "$(mon_program_for "$role")" status --json \
      >"${RUN_DIR}/agents/${node}.json" 2>"${RUN_DIR}/agents/${node}.err" ||
      printf '{"unreachable": true}\n' >"${RUN_DIR}/agents/${node}.json"
  done <"${RUN_DIR}/nodes.tsv"
  bmac_ui_step_done

  python3 - "$RUN_DIR" "$PROXMOX_CLUSTER_NAME" "${MONITOR_MAX_CHECKS:-20}" "$BMAC_UI_LIB_DIR" <<'PY'
import json
from pathlib import Path
import sys

run_dir, cluster, max_checks, ui_lib = Path(sys.argv[1]), sys.argv[2], int(sys.argv[3]), sys.argv[4]
sys.path.insert(0, ui_lib)
from ui_protocol import emit_event, emit_next_step  # noqa: E402

nodes = [line.split("\t") for line in (run_dir / "nodes.tsv").read_text().splitlines() if line]
checks_report = json.loads((run_dir / "checks.json").read_text())
api_ok = "error" not in checks_report
checks = {check["name"]: check for check in checks_report.get("checks", [])}
problems, rows, data_nodes = [], [], []
for node, role, target in nodes:
    name = f"{cluster} bmaccheck {node}"
    check = checks.pop(name, None)
    try:
        agent = json.loads((run_dir / "agents" / f"{node}.json").read_text())
    except ValueError:
        agent = {"unreachable": True}
    status = check["status"] if check else ("unknown" if not api_ok else "MISSING")
    if api_ok and check is None:
        problems.append(f"{node} has no Healthchecks check; run update_cluster_runtime.sh")
    elif check and status in ("down", "new", "paused"):
        problems.append(f"{node}'s check is {status}" + (" (it never received a ping, so it never alerts)" if status == "new" else ""))
    open_problems = agent.get("problems", {}) if not agent.get("unreachable") else {}
    if agent.get("unreachable"):
        problems.append(f"{node}'s agent could not be read over SSH")
    elif not agent.get("state_present"):
        problems.append(f"{node}'s agent has no state yet (not installed or not started?)")
    for key, problem in sorted(open_problems.items()):
        problems.append(f"{node}: [{problem['priority']}] {key}: {problem['message']}")
    for check_name, error in sorted((agent.get("check_errors") or {}).items()):
        problems.append(f"{node}: the {check_name} check fails: {error}")
    rows.append([node, role, status, (check or {}).get("last_ping") or "-",
                 "unreachable" if agent.get("unreachable") else (agent.get("last_heartbeat_ok") or "-"),
                 str(len(open_problems)) if not agent.get("unreachable") else "?"])
    data_nodes.append({"node": node, "role": role, "check_status": status,
                       "last_ping": (check or {}).get("last_ping"), "reachable": not agent.get("unreachable"),
                       "problems": [{"key": k, **v} for k, v in sorted(open_problems.items())]})
obsolete = sorted(checks)
for name in obsolete:
    problems.append(f"check {name} belongs to this cluster but to no current node "
                    "(update_cluster_runtime.sh offers to delete it)")
used = checks_report.get("project_checks")
widths = [max(len(str(r[i])) for r in rows + [["NODE", "ROLE", "CHECK", "LAST PING", "AGENT HEARTBEAT", "OPEN"]]) for i in range(6)]
print()
for row in [["NODE", "ROLE", "CHECK", "LAST PING", "AGENT HEARTBEAT", "OPEN"], *rows]:
    print("  " + "  ".join(str(value).ljust(widths[i]) for i, value in enumerate(row)).rstrip())
print()
if api_ok:
    print(f"Check budget: {used} of {max_checks} checks in the Healthchecks project")
    if used is not None and used > max_checks:
        problems.append(f"the project has {used} checks, more than MONITOR_MAX_CHECKS={max_checks}")
else:
    problems.append("the Healthchecks API could not be read (check HEALTHCHECKS_API_KEY)")
emit_event({"type": "result", "data": {
    "enabled": True, "nodes": data_nodes, "obsolete_checks": obsolete,
    "budget": {"used": used, "max": max_checks}, "problems": problems,
    "verdict": "attention" if problems else "ok",
}})
if not problems:
    print("OK: every node's check is up and no agent reports a problem.")
    raise SystemExit(0)
print(f"ATTENTION: {len(problems)} item{'s' if len(problems) != 1 else ''}:")
for problem in problems:
    print(f"  - {problem}")
print("\nNext steps:")
print("  - Problems an agent reports are explained in the README section \"Monitoring and Alerts\".")
if any("no Healthchecks check" in p or "never received" in p or "no state" in p for p in problems) or obsolete:
    print("  - scripts/user_callable/hosts/update_cluster_runtime.sh installs missing agents and checks.")
    emit_next_step("Install missing agents and checks.", "scripts/user_callable/hosts/update_cluster_runtime.sh",
                   "update_cluster_runtime", {"dry_run": "false"})
emit_next_step("Send a test alert through the email, filter, and Pushover chain.",
               "scripts/user_callable/diagnostics/send_test_alert.sh", "send_test_alert")
raise SystemExit(1)
PY
}

if [[ "${SHOW_MONITORING_STATE_SOURCE_ONLY:-0}" != 1 ]]; then
  main "$@"
fi
