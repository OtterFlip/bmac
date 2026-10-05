#!/usr/bin/env bash

# Copyright (c) 2026 BEENTHERE VENTURES, INC.
# SPDX-License-Identifier: GPL-3.0-only

# Set up, or repair, strict jump SSH from this workstation to one registered
# production or staging guest. Writes the same managed ~/.ssh/config block and
# QGA-attested known_hosts pin that create_prod_vm.sh and create_staging_vm.sh
# write, replacing whatever this workstation previously had for the alias.

set -Eeuo pipefail
set +x
umask 077

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
REPO_ROOT="$(cd -- "${SCRIPT_DIR}/.." && pwd -P)"
CONFIG_LIB="${REPO_ROOT}/lib/config.sh"
REMOTE_REGISTRY="/usr/local/lib/app-ha-proxmox/lib/cluster_registry.py"

RESOURCE_NAME=""
COORDINATOR=""
RUN_DIR=""
GUEST_KIND=""
GUEST_STATE=""
GUEST_VMID=""
GUEST_IP=""
GUEST_NODE=""
GUEST_STATUS=""
JUMP_HOST=""
SSH_ALIAS=""
GUEST_KEY_CORE=""
GUEST_FINGERPRINT=""
declare -a ONLINE_NODES=()
declare -a REACHABLE_NODES=()

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

usage() {
  cat <<'EOF'
Usage: guests/setup_jump_ssh_access.sh [prodN | stageNprodN]

Run from an administrator workstation. With no argument, lists every
registered production and staging guest and prompts for one. The script then:

  1. reads the guest's Ed25519 SSH host key through the QEMU guest agent on
     the Proxmox node currently running it;
  2. picks a reachable, online mox host as the SSH jump host (default: the
     node running the guest);
  3. replaces any existing ~/.ssh/config and ~/.ssh/known_hosts settings for
     the chosen alias with a strict managed block that jumps through that
     host and pins the attested host key;
  4. proves `ssh <alias>` works, restoring the previous files if it does not.

The workstation's SSH key must already be authorized for root in the guest,
and `ssh moxN` must already work from this workstation for the jump host.
Rerun this script whenever the recorded jump host leaves the cluster.

Options:
  -h, --help
EOF
}

parse_args() {
  while (($#)); do
    case "$1" in
      -h | --help)
        usage
        exit 0
        ;;
      -*)
        die "Unknown option: $1"
        ;;
      *)
        [[ -z "$RESOURCE_NAME" ]] || die "Only one guest may be selected"
        RESOURCE_NAME="$1"
        ;;
    esac
    shift
  done
}

prompt_with_default() {
  local destination="$1" prompt="$2" default="$3" entered
  IFS= read -r -p "${prompt} [${default}]: " entered || entered=""
  printf -v "$destination" '%s' "${entered:-$default}"
}

prompt_yes_default_yes() {
  local answer
  IFS= read -r -p "$1 [Y/n] " answer || answer=""
  [[ -z "$answer" || "${answer,,}" == y || "${answer,,}" == yes ]]
}

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
  mox_ssh "$COORDINATOR" "$REMOTE_REGISTRY" \
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

cleanup() {
  local code=$?
  trap - EXIT
  set +e
  [[ -z "$RUN_DIR" ]] || rm -rf -- "$RUN_DIR"
  exit "$code"
}

preflight_workstation() {
  local command_name
  for command_name in awk install mktemp python3 ssh ssh-keygen; do
    command -v "$command_name" >/dev/null 2>&1 ||
      die "Required workstation command is unavailable: $command_name"
  done
  [[ -n "${HOME:-}" && "$HOME" == /* ]] ||
    die "HOME is unavailable or not absolute"
  local path
  for path in "${HOME}/.ssh" "${HOME}/.ssh/config" "${HOME}/.ssh/known_hosts"; do
    [[ ! -L "$path" ]] ||
      die "Refusing to manage $path because it is a symlink"
  done
}

load_cluster_state() {
  log "Reading the cluster registry and live guest state"
  COORDINATOR="$(first_reachable_mox)" ||
    die "No reachable mox coordinator was found"
  info "Coordinator: $COORDINATOR"
  node_exec "$COORDINATOR" test -x "$REMOTE_REGISTRY" ||
    die "Installed cluster registry is unavailable on $COORDINATOR"

  write_json "${RUN_DIR}/resources.json" "$(registry_cmd list)" ||
    die "Could not list registry resources"
  write_json "${RUN_DIR}/cluster-vms.json" \
    "$(pvesh_get /cluster/resources --type vm)" ||
    die "Could not list live VMs"
  write_json "${RUN_DIR}/nodes.json" "$(pvesh_get /nodes)" ||
    die "Could not list cluster nodes"

  mapfile -t ONLINE_NODES < <(
    python3 - "${RUN_DIR}/nodes.json" <<'PY'
import json
import re
import sys
rows = json.load(open(sys.argv[1], encoding="utf-8"))
names = sorted(
    (
        row["node"]
        for row in rows
        if row.get("status") == "online"
        and re.fullmatch(r"mox([1-9]|10)", str(row.get("node", "")))
    ),
    key=lambda name: int(name[3:]),
)
print(*names, sep="\n")
PY
  )
  ((${#ONLINE_NODES[@]} > 0)) || die "Proxmox reports no online mox nodes"
}

# Print one tab-separated row per registered guest:
# name, kind, registry state, vmid, private IP, live node, live status.
guest_rows() {
  python3 - "${RUN_DIR}/resources.json" "${RUN_DIR}/cluster-vms.json" <<'PY'
import json
import sys
resources = json.load(open(sys.argv[1], encoding="utf-8"))
live = json.load(open(sys.argv[2], encoding="utf-8"))
by_vmid = {str(row.get("vmid")): row for row in live if row.get("type") == "qemu"}
rows = [
    row for row in resources
    if row.get("kind") in ("production", "staging")
    and row.get("vmid") is not None
    and row.get("ip")
]
rows.sort(key=lambda row: (row["kind"] != "production", row["name"]))
for row in rows:
    vm = by_vmid.get(str(row["vmid"]), {})
    if vm and vm.get("name") != row["name"]:
        vm = {}
    print("\t".join((
        row["name"],
        row["kind"],
        str(row.get("state", "?")),
        str(row["vmid"]),
        row["ip"],
        str(vm.get("node") or "-"),
        str(vm.get("status") or "absent"),
    )))
PY
}

select_guest() {
  local -a rows=()
  mapfile -t rows < <(guest_rows)
  ((${#rows[@]} > 0)) || die "No production or staging guests are registered"

  local row name kind state vmid ip node status default=""
  printf '\nRegistered guests:\n'
  printf '  %-20s %-10s %-16s %-6s %-15s %-6s %s\n' \
    NAME KIND REGISTRY-STATE VMID PRIVATE-IP NODE STATUS
  for row in "${rows[@]}"; do
    IFS=$'\t' read -r name kind state vmid ip node status <<<"$row"
    printf '  %-20s %-10s %-16s %-6s %-15s %-6s %s\n' \
      "$name" "$kind" "$state" "$vmid" "$ip" "$node" "$status"
    [[ -n "$default" || "$status" != running ]] || default="$name"
  done
  [[ -n "$default" ]] || die "None of the registered guests is running"

  if [[ -z "$RESOURCE_NAME" ]]; then
    printf '\n'
    prompt_with_default RESOURCE_NAME "Guest to set up jump SSH for" "$default"
  fi

  for row in "${rows[@]}"; do
    IFS=$'\t' read -r name kind state vmid ip node status <<<"$row"
    [[ "$name" == "$RESOURCE_NAME" ]] || continue
    GUEST_KIND="$kind"
    GUEST_STATE="$state"
    GUEST_VMID="$vmid"
    GUEST_IP="$ip"
    GUEST_NODE="$node"
    GUEST_STATUS="$status"
    break
  done
  [[ -n "$GUEST_KIND" ]] ||
    die "$RESOURCE_NAME is not a registered production or staging guest"
  [[ "$GUEST_VMID" =~ ^[1-9][0-9]*$ ]] || die "Unsafe VMID: $GUEST_VMID"
  [[ "$GUEST_IP" =~ ^[0-9]{1,3}(\.[0-9]{1,3}){3}$ ]] ||
    die "Unsafe private IP: $GUEST_IP"
  [[ "$GUEST_STATUS" == running ]] ||
    die "$RESOURCE_NAME is not running (live status: $GUEST_STATUS); start it and rerun"
  [[ "$GUEST_NODE" =~ ^mox([1-9]|10)$ ]] ||
    die "$RESOURCE_NAME is running on an unexpected node: $GUEST_NODE"
  info "Selected $GUEST_KIND guest $RESOURCE_NAME (VMID $GUEST_VMID, $GUEST_IP), registry state $GUEST_STATE, running on $GUEST_NODE"
}

select_jump_host() {
  log "Choosing the jump host"
  local node
  for node in "${ONLINE_NODES[@]}"; do
    if mox_is_reachable "$node"; then
      REACHABLE_NODES+=("$node")
    else
      info "$node is online in Proxmox but not reachable from this workstation"
    fi
  done
  ((${#REACHABLE_NODES[@]} > 0)) ||
    die "No online mox host is reachable from this workstation"
  info "Online and reachable: ${REACHABLE_NODES[*]}"

  local default="${REACHABLE_NODES[0]}"
  for node in "${REACHABLE_NODES[@]}"; do
    [[ "$node" != "$GUEST_NODE" ]] || default="$GUEST_NODE"
  done
  while true; do
    prompt_with_default JUMP_HOST "Jump host" "$default"
    for node in "${REACHABLE_NODES[@]}"; do
      [[ "$node" != "$JUMP_HOST" ]] || break 2
    done
    warn "Choose one of: ${REACHABLE_NODES[*]}"
  done

  # ProxyJump resolves the jump host through this workstation's own SSH
  # config, so prove that path independently of lib/config.sh's options.
  ssh -o BatchMode=yes -o ConnectTimeout=10 \
    -o ControlMaster=no -o ControlPath=none "$JUMP_HOST" true </dev/null ||
    die "'ssh $JUMP_HOST' does not work from this workstation. Add a Host $JUMP_HOST entry (HostName, User root) to ~/.ssh/config and accept its host key, then rerun."
  info "'ssh $JUMP_HOST' works from this workstation"
}

attest_guest_host_key() {
  log "Reading the guest SSH host key through QGA on $GUEST_NODE"
  local guest_key_json guest_key
  guest_key_json="$(
    node_exec "$GUEST_NODE" qm guest exec "$GUEST_VMID" --timeout 30 -- \
      /bin/cat /etc/ssh/ssh_host_ed25519_key.pub
  )" || die "Failed to obtain the guest Ed25519 host key through QGA"
  guest_key="$(
    python3 - "$guest_key_json" <<'PY'
import json
import re
import sys
value = json.loads(sys.argv[1])
key = value.get("out-data", "").strip()
if (
    value.get("exited") != 1
    or value.get("exitcode") != 0
    or not re.fullmatch(r"ssh-ed25519 [A-Za-z0-9+/=]+(?: [^\r\n]+)?", key)
):
    raise SystemExit("QGA returned an invalid Ed25519 host key")
print(key)
PY
  )" || die "QGA returned an invalid guest Ed25519 host key"
  GUEST_KEY_CORE="$(awk '{print $1 " " $2}' <<<"$guest_key")"
  GUEST_FINGERPRINT="$(
    printf '%s\n' "$GUEST_KEY_CORE" | ssh-keygen -lf - | awk '{print $2}'
  )" || die "Could not fingerprint the QGA-attested guest host key"
  info "QGA-attested guest Ed25519 fingerprint: $GUEST_FINGERPRINT"
}

effective_value() {
  awk -v key="$1" '$1 == key {print $2; exit}' <<<"$2"
}

show_current_settings() {
  local ssh_config="$1" known_hosts="$2" effective="$3"
  log "Current workstation settings for alias $SSH_ALIAS"
  local key
  for key in hostname user port proxyjump proxycommand hostkeyalias \
    stricthostkeychecking; do
    info "$(printf '%-22s %s' "$key" "$(effective_value "$key" "$effective")")"
  done

  if [[ -f "$ssh_config" ]]; then
    local stanzas
    stanzas="$(
      python3 - "$ssh_config" "$SSH_ALIAS" <<'PY'
import shlex
import sys
for number, raw in enumerate(open(sys.argv[1], encoding="utf-8"), 1):
    line = raw.strip()
    if not line or line.startswith("#"):
        continue
    try:
        parts = shlex.split(line, comments=True)
    except ValueError:
        continue
    if parts and parts[0].lower() == "host" and sys.argv[2] in parts[1:]:
        print(f"line {number}: {line}")
PY
    )"
    if [[ -n "$stanzas" ]]; then
      info "Host stanzas in ~/.ssh/config naming $SSH_ALIAS:"
      while IFS= read -r key; do
        info "  $key"
      done <<<"$stanzas"
    fi
  fi

  if [[ -f "$known_hosts" ]]; then
    local existing_key fingerprint
    while IFS= read -r existing_key; do
      [[ -n "$existing_key" ]] || continue
      fingerprint="$(
        printf '%s\n' "$existing_key" | ssh-keygen -lf - 2>/dev/null |
          awk '{print $2}'
      )" || fingerprint=""
      info "known_hosts pin for $SSH_ALIAS: ${fingerprint:-unreadable}"
    done < <(
      ssh-keygen -F "$SSH_ALIAS" -f "$known_hosts" 2>/dev/null |
        awk '$1 !~ /^#/ {print $2 " " $3}'
    )
  fi
}

effective_matches() {
  local effective="$1"
  grep -Fxq "hostname $GUEST_IP" <<<"$effective" &&
    grep -Fxq "user root" <<<"$effective" &&
    grep -Fxq "port 22" <<<"$effective" &&
    grep -Fxq "proxyjump $JUMP_HOST" <<<"$effective" &&
    grep -Fxq "hostkeyalias $SSH_ALIAS" <<<"$effective" &&
    grep -Fxq "stricthostkeychecking true" <<<"$effective" &&
    [[ -z "$(effective_value proxycommand "$effective")" ]]
}

strict_ssh_works() {
  ssh -o BatchMode=yes -o ConnectTimeout=15 \
    -o ControlMaster=no -o ControlPath=none "$SSH_ALIAS" true </dev/null
}

configure_alias() {
  local ssh_dir="${HOME}/.ssh"
  local known_hosts="${ssh_dir}/known_hosts"
  local ssh_config="${ssh_dir}/config"
  install -d -m 0700 "$ssh_dir"

  while true; do
    prompt_with_default SSH_ALIAS "Workstation SSH alias" "$RESOURCE_NAME"
    [[ "$SSH_ALIAS" =~ ^[A-Za-z0-9][A-Za-z0-9_.-]{0,63}$ ]] && break
    warn "SSH alias must use 1-64 letters, digits, dots, underscores, or hyphens"
  done
  [[ "$SSH_ALIAS" == "$RESOURCE_NAME" ]] ||
    warn "Other scripts (for example hosts/decommission_disks.sh) run 'ssh $RESOURCE_NAME'; they will not use alias $SSH_ALIAS"

  local effective
  effective="$(ssh -G "$SSH_ALIAS" 2>/dev/null || true)"
  show_current_settings "$ssh_config" "$known_hosts" "$effective"

  if effective_matches "$effective" && [[ -f "$known_hosts" ]] &&
    ssh-keygen -F "$SSH_ALIAS" -f "$known_hosts" 2>/dev/null |
    awk '$1 !~ /^#/ {print $2 " " $3}' | grep -Fxq -- "$GUEST_KEY_CORE" &&
    strict_ssh_works; then
    log "Alias $SSH_ALIAS is already correctly configured and working"
    info "Connect with: ssh $SSH_ALIAS"
    return 0
  fi

  log "Planned settings for alias $SSH_ALIAS"
  info "HostName $GUEST_IP, User root, Port 22, ProxyJump $JUMP_HOST"
  info "HostKeyAlias $SSH_ALIAS pinned to $GUEST_FINGERPRINT, StrictHostKeyChecking yes"
  info "Any other known_hosts entries for $SSH_ALIAS are removed. The managed"
  info "block is placed first in ~/.ssh/config, so it overrides any older"
  info "stanza for $SSH_ALIAS; those stanzas are left in place."
  prompt_yes_default_yes "Replace this workstation's SSH settings for $SSH_ALIAS?" ||
    die "No workstation files were changed"

  local backup_dir="${RUN_DIR}/ssh-jump-backup"
  install -d -m 0700 "$backup_dir"
  local config_existed=false known_existed=false
  if [[ -f "$ssh_config" ]]; then
    install -m 0600 "$ssh_config" "${backup_dir}/config"
    config_existed=true
  fi
  if [[ -f "$known_hosts" ]]; then
    install -m 0600 "$known_hosts" "${backup_dir}/known_hosts"
    known_existed=true
  fi

  local staged_known_hosts="${backup_dir}/known_hosts.new"
  if [[ "$known_existed" == true ]]; then
    install -m 0600 "$known_hosts" "$staged_known_hosts"
  else
    : >"$staged_known_hosts"
    chmod 0600 "$staged_known_hosts"
  fi
  ssh-keygen -R "$SSH_ALIAS" -f "$staged_known_hosts" >/dev/null 2>&1 || true
  rm -f -- "${staged_known_hosts}.old"
  printf '%s %s\n' "$SSH_ALIAS" "$GUEST_KEY_CORE" >>"$staged_known_hosts"

  local staged_config="${backup_dir}/config.new"
  python3 - "$ssh_config" "$staged_config" "$SSH_ALIAS" \
    "$GUEST_IP" "$JUMP_HOST" "$GUEST_KIND" <<'PY'
from pathlib import Path
import sys

source, destination, alias, address, jump, kind = sys.argv[1:]
markers = {
    "production": "production guest",
    "staging": "staging guest",
}
managed = {
    (f"# BEGIN app-ha managed {label} {alias}", f"# END app-ha managed {label} {alias}")
    for label in markers.values()
}
text = ""
path = Path(source)
if path.is_file():
    text = path.read_text(encoding="utf-8")
kept = []
end = None
for line in text.splitlines():
    if end is None:
        match = [pair for pair in managed if pair[0] == line]
        if match:
            end = match[0][1]
            continue
        kept.append(line)
    elif line == end:
        end = None
if end is not None:
    raise SystemExit("existing managed SSH block is unterminated")
label = markers[kind]
block = [
    f"# BEGIN app-ha managed {label} {alias}",
    f"Host {alias}",
    f"    HostName {address}",
    "    User root",
    "    Port 22",
    f"    ProxyJump {jump}",
    f"    HostKeyAlias {alias}",
    "    StrictHostKeyChecking yes",
    "    PasswordAuthentication no",
    "    KbdInteractiveAuthentication no",
    f"# END app-ha managed {label} {alias}",
    "",
]
Path(destination).write_text("\n".join(block + kept).rstrip() + "\n", encoding="utf-8")
PY
  chmod 0600 "$staged_config"

  install -m 0600 "$staged_known_hosts" "$known_hosts"
  # Prepend the exact managed block so first-value-wins OpenSSH semantics
  # override any older matching stanza without deleting unrelated user config.
  install -m 0600 "$staged_config" "$ssh_config"

  local failure=""
  effective="$(ssh -G "$SSH_ALIAS" 2>/dev/null || true)"
  if ! effective_matches "$effective"; then
    failure="ssh -G $SSH_ALIAS does not resolve to the managed settings (a Match block or Include earlier in the config may be overriding them)"
  elif ! strict_ssh_works; then
    failure="strict 'ssh $SSH_ALIAS true' failed; check that this workstation's SSH key is in /root/.ssh/authorized_keys on $RESOURCE_NAME"
  fi
  if [[ -n "$failure" ]]; then
    warn "Restoring the prior workstation files because $failure"
    if [[ "$known_existed" == true ]]; then
      install -m 0600 "${backup_dir}/known_hosts" "$known_hosts"
    else
      rm -f -- "$known_hosts"
    fi
    if [[ "$config_existed" == true ]]; then
      install -m 0600 "${backup_dir}/config" "$ssh_config"
    else
      rm -f -- "$ssh_config"
    fi
    die "Jump SSH to $RESOURCE_NAME was not configured"
  fi

  log "Configured and validated strict jump SSH alias: $SSH_ALIAS"
  info "Jumps through $JUMP_HOST; rerun this script if $JUMP_HOST leaves the cluster."
  info "Connect with: ssh $SSH_ALIAS"
}

main() {
  parse_args "$@"
  preflight_workstation
  [[ -f "$CONFIG_LIB" && ! -L "$CONFIG_LIB" ]] ||
    die "Configuration library is unavailable: $CONFIG_LIB"
  # shellcheck disable=SC1090
  source "$CONFIG_LIB"
  load_proxmox_config --no-secrets >/dev/null ||
    die "Could not load cluster configuration"

  RUN_DIR="$(mktemp -d "${TMPDIR:-/tmp}/setup-jump-ssh-access.XXXXXX")"
  chmod 0700 "$RUN_DIR"
  trap cleanup EXIT

  load_cluster_state
  select_guest
  select_jump_host
  attest_guest_host_key
  configure_alias
}

if [[ "${SETUP_JUMP_SSH_SOURCE_ONLY:-0}" != 1 ]]; then
  main "$@"
fi
