#!/usr/bin/env bash

# Copyright (c) 2026 BEENTHERE VENTURES, INC.
# SPDX-License-Identifier: GPL-3.0-only

# View and change the ingress health-check probes the QDevice's monitor agent
# runs for each production VM: an HTTPS request through Cloudflare to the
# app's health endpoint, which proves the whole ingress path works. The
# settings live in each production VM's registry record (monitoring.probe);
# saving one re-renders and installs the QDevice's config.

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

SHOW_ONLY=false
ASSUME_YES=false
SAVE_IF_PROBE_FAILS=false
VM=""
ENABLED=""
PROBE_PATH=""
PROBE_STATUS=""
PROBE_TEXT=""
CLEAR_TEXT=false
declare -a FQDNS=()
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
Usage: scripts/user_callable/qdevice/manage_qdevice_probes.sh [prodN] [--show]
         [--vm prodN] [--enable | --disable] [--path PATH] [--status N]
         [--text TEXT | --clear-text] [--fqdn FQDN]... [--yes]
         [--save-if-probe-fails] [--json]

Show, and change, the ingress health-check probe the QDevice runs for each
production VM: every minute it requests https://FQDN/PATH through Cloudflare
and expects HTTP status N (and, optionally, TEXT in the body). A failing
probe is a high-priority alert. Put the endpoint under /api/, which BMAC's
Cloudflare setup never challenges; the example app answers /api/v1/healthz.

  --show           Show every production VM's probe and the QDevice's latest
                   result, and change nothing.
  --vm prodN       The VM to change (or give prodN as the first argument).
  --enable, --disable
  --path PATH      Default MONITOR_PROBE_DEFAULT_PATH (/api/v1/healthz).
  --status N       Default MONITOR_PROBE_DEFAULT_STATUS (200).
  --text TEXT      Text the response body must contain; --clear-text removes it.
  --fqdn FQDN      An FQDN to probe: the VM's primary domain or an alias
                   (repeatable; default the primary domain).
  --yes            Save without asking.
  --save-if-probe-fails
                   With --yes, save even if the trial probe fails (for an app
                   that is not deployed yet).

Before saving, the probe runs once from the QDevice with the new settings.
EOF
}

parse_args() {
  while (($#)); do
    case "$1" in
      --show) SHOW_ONLY=true ;;
      --yes) ASSUME_YES=true ;;
      --save-if-probe-fails) SAVE_IF_PROBE_FAILS=true ;;
      --enable) ENABLED=true ;;
      --disable) ENABLED=false ;;
      --clear-text) CLEAR_TEXT=true ;;
      --vm | --path | --status | --text | --fqdn)
        (($# >= 2)) || die "$1 needs a value"
        case "$1" in
          --vm) VM="$2" ;;
          --path) PROBE_PATH="$2" ;;
          --status) PROBE_STATUS="$2" ;;
          --text) PROBE_TEXT="$2" ;;
          --fqdn) FQDNS+=("$2") ;;
        esac
        shift
        ;;
      -h | --help)
        usage
        exit 0
        ;;
      prod[1-9]*)
        VM="$1"
        ;;
      *)
        usage >&2
        exit 2
        ;;
    esac
    shift
  done
}

registry() {
  mox_ssh "$COORDINATOR" "$MONITORING_REGISTRY" --state-dir "$CLUSTER_STATE_DIR" "$@" </dev/null
}

qdevice() {
  monitoring_exec qdevice "$PROXMOX_QDEVICE_HOST" "$@"
}

show_probes() {
  python3 - "${RUN_DIR}/probes.json" "${RUN_DIR}/qdevice-status.json" <<'PY'
import json
import sys

rows = json.load(open(sys.argv[1], encoding="utf-8"))
try:
    results = json.load(open(sys.argv[2], encoding="utf-8")).get("last_results", {})
except (OSError, ValueError):
    results = {}
if not rows:
    print("    No production VMs are registered.")
for row in rows:
    probe = row["probe"]
    print(f"\n    {row['name']} ({row['state']}): {row['primary']}"
          + (f", aliases {', '.join(row['aliases'])}" if row["aliases"] else ""))
    if not probe.get("enabled"):
        print("      probe: disabled")
        continue
    print(f"      probe: GET {probe['path']} expecting HTTP {probe['expected_status']}"
          + (f" and the text {probe['expected_text']!r}" if probe.get("expected_text") else ""))
    for fqdn in probe["fqdns"]:
        result = results.get(f"ingress:{fqdn}")
        challenged = results.get(f"ingress-challenged:{fqdn}")
        if challenged and challenged.get("ok") is False:
            latest = "CHALLENGED: " + challenged["message"]
        elif result is None:
            latest = "no result yet"
        else:
            latest = ("ok" if result.get("ok") else "FAILING: " + result.get("message", "")) + f" (at {result.get('at')})"
        print(f"      {fqdn}: {latest}")
PY
}

# Fill the new probe settings into RUN_DIR/new-probe.json.
choose_settings() {
  local record="$1"
  local -a current
  mapfile -t current < <(python3 - "$record" "$MONITOR_PROBE_DEFAULT_PATH" "$MONITOR_PROBE_DEFAULT_STATUS" <<'PY'
import json
import sys

record, default_path, default_status = sys.argv[1:]
row = json.load(open(record))
probe = (row.get("monitoring") or {}).get("probe") or {}
print("true" if probe.get("enabled", True) else "false")
print(probe.get("path") or default_path)
print(probe.get("expected_status") or default_status)
print(probe.get("expected_text") or "")
print(" ".join(probe.get("fqdns") or [row["domains"]["primary"]]))
print(" ".join([row["domains"]["primary"], *row["domains"]["aliases"]]))
PY
  )
  local enabled="${ENABLED:-${current[0]}}" path="${PROBE_PATH:-${current[1]}}"
  local status="${PROBE_STATUS:-${current[2]}}" text="${PROBE_TEXT:-${current[3]}}"
  local fqdns="${FQDNS[*]:-${current[4]}}" choices="${current[5]}"
  [[ "$CLEAR_TEXT" == false ]] || text=""
  local interactive=false
  [[ -n "$ENABLED$PROBE_PATH$PROBE_STATUS$PROBE_TEXT${FQDNS[*]}" || "$CLEAR_TEXT" == true ]] || interactive=true
  if [[ "$interactive" == true ]]; then
    if bmac_ui_is_json; then
      local -a fqdn_options=()
      local choice
      for choice in $choices; do fqdn_options+=(--option "$choice" "$choice"); done
      bmac_ui_group_begin probe "Health-check probe for $VM" \
        "The QDevice requests https://FQDN/PATH through Cloudflare every minute."
      bmac_ui_group_add enabled --id enabled --type boolean --label "Probe enabled" --default "$enabled"
      bmac_ui_group_add fqdns --id fqdns --type multiselect --label "FQDNs to probe" \
        --min-selected 1 --default "${fqdns// /,}" \
        "${fqdn_options[@]}"
      bmac_ui_group_add path --id path --label "Path" --default "$path" --pattern '^/\S{0,199}$' \
        --help "Use a cheap endpoint under /api/, such as /api/v1/healthz."
      bmac_ui_group_add status --id status --type integer --label "Expected HTTP status" \
        --default "$status" --min 100 --max 599
      bmac_ui_group_add text --id text --label "Text the body must contain (optional)" --default "$text"
      bmac_ui_group_request
      fqdns="${fqdns//,/ }"
    else
      local answer
      read -r -p "Probe enabled? [$enabled] (true/false): " answer
      enabled="${answer:-$enabled}"
      printf 'FQDNs this VM serves: %s\n' "$choices"
      read -r -p "FQDNs to probe, space-separated [$fqdns]: " answer
      fqdns="${answer:-$fqdns}"
      read -r -p "Path [$path]: " answer
      path="${answer:-$path}"
      read -r -p "Expected HTTP status [$status]: " answer
      status="${answer:-$status}"
      read -r -p "Text the body must contain, or - for none [${text:-none}]: " answer
      [[ -z "$answer" ]] || text="$answer"
      [[ "$text" != "-" ]] || text=""
    fi
  fi
  python3 - "$enabled" "$path" "$status" "$text" "$fqdns" "$choices" "${RUN_DIR}/new-probe.json" <<'PY' ||
import json
import re
import sys

enabled, path, status, text, fqdns, choices, output = sys.argv[1:]
if enabled not in ("true", "false"):
    raise SystemExit("enabled must be true or false")
if not re.fullmatch(r"/[^\s\x00-\x1f]{0,199}", path):
    raise SystemExit("the path must start with / and have no spaces (at most 200 characters)")
if not status.isdigit() or not 100 <= int(status) <= 599:
    raise SystemExit("the expected status must be 100-599")
if len(text) > 200 or re.search(r"[\x00-\x1f]", text):
    raise SystemExit("the expected text must be one line of at most 200 characters")
selected = [f.strip().lower().rstrip(".") for f in fqdns.split() if f.strip()]
unknown = [f for f in selected if f not in choices.split()]
if unknown:
    raise SystemExit(f"{', '.join(unknown)} is not one of this VM's domains ({choices.replace(' ', ', ')})")
json.dump({"probe": {"enabled": enabled == "true", "path": path, "expected_status": int(status),
                     "expected_text": text or None, "fqdns": selected or [choices.split()[0]]}},
          open(output, "w"))
PY
    die "Invalid probe settings; nothing was changed"
}

trial_probe() {
  local probe="$1" ok=true fqdn
  mapfile -t lines < <(python3 -c '
import json, sys
p = json.load(open(sys.argv[1]))["probe"]
for f in p["fqdns"]:
    print("\t".join([f, p["path"], str(p["expected_status"]), p["expected_text"] or ""]))' "$probe")
  log "Running the probe once from the QDevice"
  local path status text output
  for line in "${lines[@]}"; do
    IFS=$'\t' read -r fqdn path status text <<<"$line"
    local -a arguments=(probe --fqdn "$fqdn" --path "$path" --status "$status" --json)
    [[ -z "$text" ]] || arguments+=(--text "$text")
    if output="$(qdevice python3 -I "$MONITORING_NODE_PROGRAM" "${arguments[@]}" 2>&1)"; then
      info "ok: $(python3 -c 'import json, sys; print(json.loads(sys.argv[1])["message"])' "$output" 2>/dev/null || echo "$output")"
    else
      ok=false
      info "FAILED: $(python3 -c 'import json, sys; print(json.loads(sys.argv[1])["message"])' "$output" 2>/dev/null || echo "$output")"
    fi
  done
  [[ "$ok" == true ]]
}

main() {
  parse_args "$@"
  load_proxmox_config --no-secrets >/dev/null || die "cluster configuration is invalid"
  monitoring_enabled ||
    printf 'NOTE: MONITORING_ENABLED is false, so the QDevice runs no probes until it is enabled.\n'
  RUN_DIR="$(mktemp -d "${TMPDIR:-/tmp}/manage-qdevice-probes.XXXXXX")"
  chmod 0700 "$RUN_DIR"
  trap 'rm -rf -- "$RUN_DIR"' EXIT
  COORDINATOR="$(first_reachable_mox)" || die "No reachable Proxmox host was found"

  log "Health-check probes"
  registry monitoring-probes >"${RUN_DIR}/probes.json" ||
    die "Could not read the probes from the registry (run update_cluster_runtime.sh first if the registry predates monitoring)"
  qdevice python3 -I "$MONITORING_NODE_PROGRAM" status --json >"${RUN_DIR}/qdevice-status.json" 2>/dev/null ||
    printf '{}\n' >"${RUN_DIR}/qdevice-status.json"
  show_probes
  if [[ "$SHOW_ONLY" == true ]]; then
    bmac_ui_step_done
    python3 - "${RUN_DIR}/probes.json" "${RUN_DIR}/qdevice-status.json" >"${RUN_DIR}/result.json" <<'PY'
import json
import sys

rows = json.load(open(sys.argv[1]))
try:
    results = json.load(open(sys.argv[2])).get("last_results", {})
except ValueError:
    results = {}
for row in rows:
    row["latest"] = {fqdn: results.get(f"ingress:{fqdn}") for fqdn in row["probe"].get("fqdns") or []}
print(json.dumps({"probes": rows}))
PY
    bmac_ui_result_file "${RUN_DIR}/result.json"
    return 0
  fi

  if [[ -z "$VM" ]]; then
    local -a options=()
    mapfile -t options < <(python3 -c '
import json, sys
for row in json.load(open(sys.argv[1])):
    print(row["name"])
    print(row["name"] + " · " + row["primary"])' "${RUN_DIR}/probes.json")
    ((${#options[@]})) || die "No production VMs are registered"
    if bmac_ui_is_json; then
      bmac_ui_choose VM "Production VM" "${options[0]}" "${options[@]}"
    else
      read -r -p "Production VM to change (for example prod1): " VM
    fi
  fi
  [[ "$VM" =~ ^prod[1-9][0-9]*$ ]] || die "A production VM name such as prod1 is required"
  registry get "$VM" >"${RUN_DIR}/record.json" || die "$VM is not a registered production VM"
  local revision
  revision="$(python3 -c 'import json, sys; r = json.load(open(sys.argv[1])); assert r["kind"] == "production"; print(r["revision"])' \
    "${RUN_DIR}/record.json")" || die "$VM is not a production VM"

  choose_settings "${RUN_DIR}/record.json"
  if python3 -c 'import json, sys; sys.exit(0 if json.load(open(sys.argv[1]))["probe"]["enabled"] else 1)' \
    "${RUN_DIR}/new-probe.json"; then
    if ! trial_probe "${RUN_DIR}/new-probe.json"; then
      if [[ "$ASSUME_YES" == true ]]; then
        [[ "$SAVE_IF_PROBE_FAILS" == true ]] ||
          die "The trial probe failed; nothing was saved (add --save-if-probe-fails to save anyway)"
      elif bmac_ui_is_json; then
        bmac_ui_confirm --id save_anyway --question --severity warning --title "The trial probe failed. Save anyway?" \
          --message "Save it anyway if the app is not deployed yet; the QDevice will alert until the probe passes." \
          --confirm-label "Save anyway" --cancel-label "Don't save" ||
          { log "Not saved"; bmac_ui_result saved:bool false; return 0; }
      else
        local answer
        read -r -p "The trial probe failed. Save anyway? [y/N] " answer
        [[ "$answer" =~ ^[Yy]([Ee][Ss])?$ ]] || { log "Not saved"; return 0; }
      fi
    fi
  fi

  log "Plan for $VM"
  local summary
  summary="$(python3 - "${RUN_DIR}/new-probe.json" <<'PY'
import json
import sys

probe = json.load(open(sys.argv[1]))["probe"]
if not probe["enabled"]:
    print("disabled")
else:
    text = f" containing {probe['expected_text']!r}" if probe["expected_text"] else ""
    hosts = ",".join(probe["fqdns"])
    hosts = hosts if len(probe["fqdns"]) == 1 else "{" + hosts + "}"
    print(f"GET https://{hosts}{probe['path']} expecting HTTP {probe['expected_status']}{text}")
PY
)"
  info "Probe: $summary"
  info "Saved in $VM's registry record (revision $revision), then installed in the QDevice's config"
  bmac_ui_plan_begin "Health-check probe for $VM"
  bmac_ui_plan_item update "$VM" "Probe: $summary"
  monitoring_enabled && bmac_ui_plan_item update QDevice "Re-render and install the QDevice's monitor config"
  bmac_ui_plan_end
  if [[ "$ASSUME_YES" != true ]]; then
    if bmac_ui_is_json; then
      bmac_ui_confirm --id save --title "Save the probe for $VM?" --message "Probe: $summary" \
        --confirm-label Save || die "Cancelled; nothing was changed"
    else
      local answer
      read -r -p "Save? [y/N] " answer
      [[ "$answer" =~ ^[Yy]([Ee][Ss])?$ ]] || die "Cancelled; nothing was changed"
    fi
  fi

  log "Saving the probe in the registry"
  registry update "$VM" --expected-revision "$revision" \
    --monitoring-json "$(cat "${RUN_DIR}/new-probe.json")" >/dev/null ||
    die "The registry did not accept the change (it may have changed meanwhile; rerun to see the current settings)"
  info "Saved"
  monitoring_membership_sync "$COORDINATOR"
  bmac_ui_result vm "$VM" saved:bool true probe "$summary"
}

if [[ "${MANAGE_QDEVICE_PROBES_SOURCE_ONLY:-0}" != 1 ]]; then
  main "$@"
fi
