#!/usr/bin/env bash

# Copyright (c) 2026 BEENTHERE VENTURES, INC.
# SPDX-License-Identifier: GPL-3.0-only

# View and change what a production VM's monitor agent watches inside the VM:
# its systemd services and its certificate files. These live on the VM in
# /etc/app-ha-monitor/guest.json, because they describe the VM's own
# software; the agent re-reads the file every cycle, so no restart is needed.
# This never touches the registry or the QDevice.

set -Eeuo pipefail
set +x
umask 077

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
REPO_ROOT="$(cd -- "${SCRIPT_DIR}/../../../.." && pwd -P)"
# shellcheck source=../../../lib/ui_protocol.sh
source "${REPO_ROOT}/scripts/lib/ui_protocol.sh"
# shellcheck source=../../../lib/config.sh
source "${REPO_ROOT}/scripts/lib/config.sh"
# shellcheck source=../../../lib/monitoring.sh
source "${REPO_ROOT}/scripts/lib/monitoring.sh"
bmac_ui_bootstrap "$@"

GUEST_SETTINGS=/etc/app-ha-monitor/guest.json
RESOURCE_NAME=""
SHOW_ONLY=false
ASSUME_YES=false
RUN_DIR=""
declare -a ADD_SERVICES=() REMOVE_SERVICES=() ADD_CERTS=() REMOVE_CERTS=()

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
Usage: scripts/user_callable/guests/prod/manage_prod_vm_monitoring.sh [prodN]
         [--show] [--add-service UNIT]... [--remove-service UNIT]...
         [--add-cert PATH]... [--remove-cert PATH]... [--yes] [--json]

Show, and change, the systemd services and certificate files that prodN's
monitor agent watches (/etc/app-ha-monitor/guest.json on the VM). A service
that is not active is reported as a p1 problem; a certificate that expires
within MONITOR_CERT_WARN_DAYS is reported. Reaches the VM with 'ssh prodN'
(set up by setup_jump_ssh_access.sh).

  --show                 Show the current settings and change nothing.
  --add-service UNIT     Watch a systemd unit, such as hello-todo.service.
  --remove-service UNIT  Stop watching a unit.
  --add-cert PATH        Watch a PEM certificate file on the VM.
  --remove-cert PATH     Stop watching a certificate file.
  --yes                  Apply the changes without asking.

Without a change option, shows the settings and asks for the new lists.
EOF
}

parse_args() {
  while (($#)); do
    case "$1" in
      --show) SHOW_ONLY=true ;;
      --yes) ASSUME_YES=true ;;
      --add-service | --remove-service | --add-cert | --remove-cert)
        (($# >= 2)) || die "$1 needs a value"
        case "$1" in
          --add-service) ADD_SERVICES+=("$2") ;;
          --remove-service) REMOVE_SERVICES+=("$2") ;;
          --add-cert) ADD_CERTS+=("$2") ;;
          --remove-cert) REMOVE_CERTS+=("$2") ;;
        esac
        shift
        ;;
      -h | --help)
        usage
        exit 0
        ;;
      prod[1-9]*)
        [[ -z "$RESOURCE_NAME" ]] || die "Give one production VM"
        RESOURCE_NAME="$1"
        ;;
      *)
        usage >&2
        exit 2
        ;;
    esac
    shift
  done
}

guest_ssh() {
  ssh "${mon_guest_ssh_options[@]}" "$RESOURCE_NAME" "$@"
}

# One helper program on the VM reads the settings and the state of
# everything in them, validates proposed settings, and writes them atomically.
write_helper() {
  cat >"${RUN_DIR}/helper.py" <<'PY'
import json
import os
from pathlib import Path
import re
import subprocess
import sys
import tempfile

action, path = sys.argv[1], Path(sys.argv[2])
UNIT_RE = re.compile(r"^[A-Za-z0-9@._:-]+$")


def run(argv):
    try:
        done = subprocess.run(argv, capture_output=True, text=True, timeout=20)
    except (OSError, subprocess.TimeoutExpired) as exc:
        return 1, "", str(exc)
    return done.returncode, done.stdout, done.stderr


def load():
    try:
        value = json.loads(path.read_text(encoding="utf-8"))
    except FileNotFoundError:
        return {"schema_version": 1, "services": [], "cert_paths": []}, False
    return {"schema_version": 1, "services": list(value.get("services", [])),
            "cert_paths": list(value.get("cert_paths", []))}, True


def describe(settings):
    services = []
    for unit in settings["services"]:
        code, out, _ = run(["systemctl", "is-active", unit])
        services.append({"unit": unit, "state": out.strip() or "unknown"})
    certs = []
    for cert in settings["cert_paths"]:
        code, out, err = run(["openssl", "x509", "-enddate", "-noout", "-in", cert])
        certs.append({"path": cert, "expires": out.strip().removeprefix("notAfter=") if code == 0 else None,
                      "error": None if code == 0 else (err.strip().splitlines() or ["unreadable"])[0]})
    code, out, _ = run(["systemctl", "list-unit-files", "--state=enabled", "--type=service", "--no-pager",
                        "--no-legend"])
    enabled = sorted({line.split()[0] for line in out.splitlines()
                      if line.split() and line.split()[0].endswith(".service")
                      and not line.split()[0].endswith("@.service")})
    return {"services": services, "certs": certs, "enabled_services": enabled}


def validate(settings):
    errors = {}
    for unit in settings["services"]:
        if not UNIT_RE.fullmatch(unit):
            errors.setdefault("services", []).append(f"{unit}: not a systemd unit name")
        elif run(["systemctl", "cat", unit])[0] != 0:
            errors.setdefault("services", []).append(f"{unit}: no such unit on this VM")
    for cert in settings["cert_paths"]:
        if not cert.startswith("/"):
            errors.setdefault("cert_paths", []).append(f"{cert}: not an absolute path")
        elif run(["openssl", "x509", "-noout", "-in", cert])[0] != 0:
            errors.setdefault("cert_paths", []).append(f"{cert}: not a readable PEM certificate")
    return errors


text = sys.stdin.read()
proposed = json.loads(text.partition("#INPUT\n")[2] or "null") if "#INPUT\n" in text else None
current, exists = load()
if action == "read":
    print(json.dumps({"settings": current, "exists": exists, **describe(current)}))
elif action == "validate":
    print(json.dumps({"errors": validate(proposed)}))
elif action == "write":
    errors = validate(proposed)
    if errors:
        print(json.dumps({"errors": errors}))
        raise SystemExit(1)
    path.parent.mkdir(mode=0o700, parents=True, exist_ok=True)
    descriptor, temporary = tempfile.mkstemp(dir=path.parent, prefix=".guest.json.")
    with os.fdopen(descriptor, "w", encoding="utf-8") as handle:
        json.dump({"schema_version": 1, "services": proposed["services"],
                   "cert_paths": proposed["cert_paths"]}, handle, indent=2, sort_keys=True)
        handle.write("\n")
    os.chmod(temporary, 0o600)
    os.replace(temporary, path)
    print(json.dumps({"written": True, **describe(proposed)}))
PY
}

# Run the helper: the program and its input travel together on stdin, so
# nothing is written to the VM before the final save.
helper() {
  local action="$1" input="${2:-}" payload="${RUN_DIR}/payload"
  {
    printf 'import sys, io\n_text = sys.stdin.read()\n_code, _, _input = _text.partition("\\n#PROGRAM_END\\n")\n'
    printf 'sys.stdin = io.StringIO("#INPUT\\n" + _input)\nexec(compile(_code, "helper", "exec"))\n'
  } >"${RUN_DIR}/loader.py"
  {
    cat "${RUN_DIR}/helper.py"
    printf '\n#PROGRAM_END\n'
    [[ -z "$input" ]] || cat "$input"
  } >"$payload"
  guest_ssh "python3 -c $(printf '%q' "$(cat "${RUN_DIR}/loader.py")") $action $GUEST_SETTINGS" <"$payload"
}

show_state() {
  python3 - "$1" <<'PY'
import json
import sys

state = json.load(open(sys.argv[1], encoding="utf-8"))
print("    Monitored services:")
for service in state["services"] or []:
    print(f"      {service['unit']:<40} {service['state']}")
if not state["services"]:
    print("      (none)")
print("    Monitored certificates:")
for cert in state["certs"] or []:
    print(f"      {cert['path']:<40} {'expires ' + cert['expires'] if cert['expires'] else 'UNREADABLE: ' + (cert['error'] or '')}")
if not state["certs"]:
    print("      (none)")
PY
}

# Print each item of the list at KEY [SUBKEY] in a JSON file, one per line.
field() {
  python3 -c '
import json, sys
value = json.load(open(sys.argv[1]))
for key in sys.argv[2:]:
    value = value[key]
for item in value:
    print(item)
' "$@"
}

ask_new_settings() {
  local current="$1" services certs suggestions
  mapfile -t current_services < <(field "$current" settings services)
  mapfile -t current_certs < <(field "$current" settings cert_paths)
  suggestions="$(field "$current" enabled_services | tr '\n' ' ')"
  if bmac_ui_is_json; then
    bmac_ui_group_begin monitoring "Monitored services and certificates on $RESOURCE_NAME" \
      "One entry per line. A watched service that is not active is a high-priority alert."
    bmac_ui_group_add services --id services --type multiline --label "systemd services" \
      --default "$(printf '%s\n' "${current_services[@]}")" \
      --help "Enabled services on $RESOURCE_NAME: ${suggestions:-none}"
    bmac_ui_group_add certs --id cert_paths --type multiline --label "Certificate files (PEM)" \
      --default "$(printf '%s\n' "${current_certs[@]}")"
    while :; do
      bmac_ui_group_request
      to_proposed "$services" "$certs"
      helper validate "${RUN_DIR}/proposed.json" >"${RUN_DIR}/validation.json" ||
        die "Could not validate the settings on $RESOURCE_NAME"
      while IFS=$'\t' read -r id message; do
        bmac_ui_field_error "$id" "$message"
      done < <(python3 -c 'import json, sys; [print(k + "\t" + "; ".join(v)) for k, v in json.load(open(sys.argv[1]))["errors"].items()]' "${RUN_DIR}/validation.json")
      bmac_ui_group_check && break
    done
    return 0
  fi
  printf '\nEnabled services on %s: %s\n' "$RESOURCE_NAME" "${suggestions:-none}"
  read -r -p "Services to watch, space-separated [${current_services[*]}]: " services
  [[ -n "$services" ]] || services="${current_services[*]}"
  [[ "$services" != "-" ]] || services=""
  read -r -p "Certificate files to watch, space-separated [${current_certs[*]}]: " certs
  [[ -n "$certs" ]] || certs="${current_certs[*]}"
  [[ "$certs" != "-" ]] || certs=""
  to_proposed "${services// /$'\n'}" "${certs// /$'\n'}"
}

to_proposed() {
  python3 - "$1" "$2" "${RUN_DIR}/proposed.json" <<'PY'
import json
import sys

def items(text):
    seen = []
    for item in text.split():
        if item not in seen:
            seen.append(item)
    return seen

services, certs, output = sys.argv[1:]
json.dump({"schema_version": 1, "services": items(services), "cert_paths": items(certs)}, open(output, "w"))
PY
}

apply_flags() {
  python3 - "$1" "${RUN_DIR}/proposed.json" "${#ADD_SERVICES[@]}" "${ADD_SERVICES[@]}" \
    "${#REMOVE_SERVICES[@]}" "${REMOVE_SERVICES[@]}" "${#ADD_CERTS[@]}" "${ADD_CERTS[@]}" \
    "${#REMOVE_CERTS[@]}" "${REMOVE_CERTS[@]}" <<'PY'
import json
import sys

state, output, *rest = sys.argv[1:]
lists = []
while rest:
    count = int(rest.pop(0))
    lists.append(rest[:count])
    rest = rest[count:]
add_services, remove_services, add_certs, remove_certs = lists
settings = json.load(open(state))["settings"]
services = [s for s in settings["services"] if s not in remove_services]
services += [s for s in add_services if s not in services]
certs = [c for c in settings["cert_paths"] if c not in remove_certs]
certs += [c for c in add_certs if c not in certs]
json.dump({"schema_version": 1, "services": services, "cert_paths": certs}, open(output, "w"))
PY
}

main() {
  parse_args "$@"
  load_proxmox_config --no-secrets >/dev/null || die "cluster configuration is invalid"
  if [[ -z "$RESOURCE_NAME" ]]; then
    if bmac_ui_is_json; then
      bmac_ui_text RESOURCE_NAME "Production VM" "" --required --pattern '^prod[1-9][0-9]*$'
    else
      read -r -p "Production VM (for example prod1): " RESOURCE_NAME
    fi
  fi
  [[ "$RESOURCE_NAME" =~ ^prod[1-9][0-9]*$ ]] || die "A production VM name such as prod1 is required"
  monitoring_enabled ||
    printf 'NOTE: MONITORING_ENABLED is false, so no agent reads these settings yet.\n'
  RUN_DIR="$(mktemp -d "${TMPDIR:-/tmp}/manage-prod-monitoring.XXXXXX")"
  chmod 0700 "$RUN_DIR"
  trap 'rm -rf -- "$RUN_DIR"' EXIT
  write_helper

  log "Current monitoring settings on $RESOURCE_NAME"
  helper read >"${RUN_DIR}/current.json" ||
    die "Could not read $GUEST_SETTINGS on $RESOURCE_NAME over 'ssh $RESOURCE_NAME' (set it up with setup_jump_ssh_access.sh)"
  show_state "${RUN_DIR}/current.json"
  if [[ "$SHOW_ONLY" == true ]]; then
    bmac_ui_step_done
    bmac_ui_result_file "${RUN_DIR}/current.json"
    return 0
  fi

  if ((${#ADD_SERVICES[@]} + ${#REMOVE_SERVICES[@]} + ${#ADD_CERTS[@]} + ${#REMOVE_CERTS[@]})); then
    apply_flags "${RUN_DIR}/current.json"
  else
    ask_new_settings "${RUN_DIR}/current.json"
  fi

  local changes
  changes="$(python3 - "${RUN_DIR}/current.json" "${RUN_DIR}/proposed.json" <<'PY'
import json
import sys

old = json.load(open(sys.argv[1]))["settings"]
new = json.load(open(sys.argv[2]))
for kind, key in (("service", "services"), ("certificate", "cert_paths")):
    for item in new[key]:
        if item not in old[key]:
            print(f"create\t{kind} {item}")
    for item in old[key]:
        if item not in new[key]:
            print(f"remove\t{kind} {item}")
PY
)"
  if [[ -z "$changes" ]]; then
    log "No changes"
    bmac_ui_result changed:bool false
    return 0
  fi
  log "Plan for $RESOURCE_NAME"
  bmac_ui_plan_begin "Monitoring settings on $RESOURCE_NAME" "Written to $GUEST_SETTINGS on the VM."
  local kind item
  while IFS=$'\t' read -r kind item; do
    info "$([[ $kind == create ]] && echo watch || echo 'stop watching') $item"
    bmac_ui_plan_item "$kind" "$item" "$([[ $kind == create ]] && echo "Watched by the monitor" || echo "No longer watched")"
  done <<<"$changes"
  bmac_ui_plan_end
  if [[ "$ASSUME_YES" != true ]]; then
    if bmac_ui_is_json; then
      bmac_ui_confirm --id save --title "Save the monitoring settings on $RESOURCE_NAME?" \
        --message "The agent applies them on its next cycle." --confirm-label Save ||
        die "Cancelled; nothing was changed"
    else
      local answer
      read -r -p "Save these settings? [y/N] " answer
      [[ "$answer" =~ ^[Yy]([Ee][Ss])?$ ]] || die "Cancelled; nothing was changed"
    fi
  fi

  log "Saving $GUEST_SETTINGS on $RESOURCE_NAME"
  helper write "${RUN_DIR}/proposed.json" >"${RUN_DIR}/written.json" || {
    python3 -c 'import json, sys; [print("    " + m) for v in json.load(open(sys.argv[1])).get("errors", {}).values() for m in v]' \
      "${RUN_DIR}/written.json" 2>/dev/null || true
    die "The settings were not saved"
  }
  show_state "${RUN_DIR}/written.json"
  if monitoring_enabled && guest_ssh test -x "$MONITORING_NODE_PROGRAM" </dev/null; then
    log "The monitor's view of the watched items"
    guest_ssh python3 -I "$MONITORING_NODE_PROGRAM" check --once --json </dev/null \
      >"${RUN_DIR}/check.json" 2>/dev/null &&
      python3 -c '
import json, sys
for row in json.load(open(sys.argv[1]))["results"]:
    if row["key"].startswith(("service:", "cert:")):
        print("    " + ("ok   " + row["key"] if row["ok"] else "FAIL " + row["key"] + " " + row["message"]))
' "${RUN_DIR}/check.json" || info "The agent check did not run; the settings are saved anyway"
  fi
  bmac_ui_result_file "${RUN_DIR}/written.json"
}

if [[ "${MANAGE_PROD_VM_MONITORING_SOURCE_ONLY:-0}" != 1 ]]; then
  main "$@"
fi
