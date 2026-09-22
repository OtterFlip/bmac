#!/usr/bin/env bash

# Copyright (c) 2026 BEENTHERE VENTURES, INC.
# SPDX-License-Identifier: GPL-3.0-only

# Read-only registry, Proxmox, HA, replication, QGA, network, and strict-SSH
# diagnostics for one registered production VM.

set -u
set -o pipefail
umask 077

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
REPO_ROOT="$(cd -- "${SCRIPT_DIR}/.." && pwd -P)"
CONFIG_LIB="${REPO_ROOT}/lib/config.sh"
REMOTE_REGISTRY="/usr/local/lib/app-ha-proxmox/lib/cluster_registry.py"

usage() {
  cat <<'EOF'
Usage: diagnostics/show_prod_vm_state.sh prodN

Inspect one registered production VM without changing guest, Proxmox, HA,
replication, route, or registry configuration. QGA and guest SSH inspection
commands can create normal process/audit log entries.
EOF
}

if (($# != 1)) || [[ "$1" == -h || "$1" == --help ]]; then
  usage
  (($# == 1)) && exit 0
  exit 2
fi

RESOURCE_NAME="$1"
[[ "$RESOURCE_NAME" =~ ^prod[1-9][0-9]*$ ]] || {
  printf 'ERROR: production resource must be named prodN\n' >&2
  exit 2
}

[[ -f "$CONFIG_LIB" && ! -L "$CONFIG_LIB" ]] || {
  printf 'ERROR: configuration library is unavailable: %s\n' "$CONFIG_LIB" >&2
  exit 1
}
# shellcheck disable=SC1090
source "$CONFIG_LIB"
load_proxmox_config --no-secrets >/dev/null || {
  printf 'ERROR: cluster configuration is invalid\n' >&2
  exit 1
}

COORDINATOR="$(first_reachable_mox)" || exit 1
RUN_DIR="$(mktemp -d "${TMPDIR:-/tmp}/show-prod-vm-state.XXXXXX")"
chmod 0700 "$RUN_DIR"
trap 'rm -rf -- "$RUN_DIR"' EXIT

hr() {
  printf '%*s\n' 96 '' | tr ' ' '='
}

section() {
  printf '\n'
  hr
  printf '%s\n' "$1"
  printf 'Why: %s\n' "$2"
  hr
}

run() {
  local description="$1"
  shift
  printf '\n[CHECK] %s\n  $' "$description"
  printf ' %q' "$@"
  printf '\n'
  "$@"
  local rc=$?
  printf '[exit %d]\n' "$rc"
  return 0
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

section "Production VM diagnostic target" \
  "identify the registry resource and coordinator used for this read-only report."
printf 'Resource:    %s\n' "$RESOURCE_NAME"
printf 'Coordinator: %s\n' "$COORDINATOR"
printf 'Started:     %s\n' "$(date +%Y-%m-%dT%H:%M:%S%z)"

resource_json="$(registry_cmd get "$RESOURCE_NAME")" || {
  printf 'ERROR: registered resource %s was not found\n' "$RESOURCE_NAME" >&2
  exit 1
}
orchestration_json="$(registry_cmd orchestration-get "$RESOURCE_NAME")" || exit 1
routes_json="$(registry_cmd list-routes)" || exit 1
cluster_vms_json="$(pvesh_get /cluster/resources --type vm)" || exit 1
ha_resources_json="$(pvesh_get /cluster/ha/resources)" || exit 1
replication_json="$(pvesh_get /cluster/replication)" || exit 1
ha_rules_json="$(
  node_exec "$COORDINATOR" ha-manager rules config --output-format json
)" || exit 1

write_json "${RUN_DIR}/resource.json" "$resource_json"
write_json "${RUN_DIR}/orchestration.json" "$orchestration_json"
write_json "${RUN_DIR}/routes.json" "$routes_json"
write_json "${RUN_DIR}/cluster-vms.json" "$cluster_vms_json"
write_json "${RUN_DIR}/ha-resources.json" "$ha_resources_json"
write_json "${RUN_DIR}/replication.json" "$replication_json"
write_json "${RUN_DIR}/ha-rules.json" "$ha_rules_json"

fields=()
mapfile -d '' -t fields < <(
  python3 - "${RUN_DIR}/resource.json" <<'PY'
import json
import sys
row = json.load(open(sys.argv[1], encoding="utf-8"))
values = (
    row["kind"],
    row["state"],
    row["name"],
    row["vmid"],
    row["ip"],
    row["mac"],
    row["owner_node"] or "",
    ",".join(row["placement"]),
    row["initial_node"],
    row["proxmox"]["volume_id"] or "",
    row["domains"]["primary"],
    ",".join(row["domains"]["aliases"]),
)
for value in values:
    sys.stdout.buffer.write(str(value).encode() + b"\0")
PY
)
((${#fields[@]} == 12)) || {
  printf 'ERROR: could not parse registry resource\n' >&2
  exit 1
}
KIND="${fields[0]}"
RESOURCE_STATE="${fields[1]}"
VMID="${fields[3]}"
PRIVATE_IP="${fields[4]}"
VM_MAC="${fields[5]}"
OWNER_NODE="${fields[6]}"
PLACEMENT_CSV="${fields[7]}"
ROOT_VOLUME="${fields[9]}"
PRIMARY_DOMAIN="${fields[10]}"
ALIASES_CSV="${fields[11]}"

[[ "$KIND" == production ]] || {
  printf 'ERROR: %s is not a production resource\n' "$RESOURCE_NAME" >&2
  exit 1
}

section "Registry and orchestration records" \
  "show the durable source of truth for identity, placement, lifecycle, routes, and resume state."
run "Production resource record" python3 -m json.tool "${RUN_DIR}/resource.json"
run "Production orchestration record" \
  python3 -m json.tool "${RUN_DIR}/orchestration.json"
printf '\nRegistry summary:\n'
printf '  state=%s vmid=%s owner=%s placement=%s\n' \
  "$RESOURCE_STATE" "$VMID" "${OWNER_NODE:-none}" "$PLACEMENT_CSV"
printf '  ip=%s mac=%s volume=%s\n' \
  "$PRIVATE_IP" "$VM_MAC" "${ROOT_VOLUME:-none}"
printf '  primary=%s aliases=%s\n' "$PRIMARY_DOMAIN" "${ALIASES_CSV:-none}"

section "Cross-layer contract validation" \
  "require registry, live QEMU identity, HA request/rule, routes, and replication topology to agree."
contract_ready=1
if python3 - \
  "${RUN_DIR}/resource.json" \
  "${RUN_DIR}/orchestration.json" \
  "${RUN_DIR}/routes.json" \
  "${RUN_DIR}/cluster-vms.json" \
  "${RUN_DIR}/ha-resources.json" \
  "${RUN_DIR}/ha-rules.json" \
  "${RUN_DIR}/replication.json" <<'PY'
import json
import sys

resource, orchestration, routes, vms, ha, rules, replication = [
    json.load(open(path, encoding="utf-8")) for path in sys.argv[1:]
]
name = resource["name"]
vmid = int(resource["vmid"])
sid = f"vm:{vmid}"
placement = set(resource["placement"])
owner = resource["owner_node"]

def require(condition, message):
    if not condition:
        raise SystemExit(message)
    print(f"  [PASS] {message}")

require(resource["kind"] == "production", "registry kind is production")
require(resource["state"] == "active", "registry state is active")
require(resource["routes_enabled"] is True, "registry routes are enabled")
require(orchestration.get("install_phase") == "network-finalized",
        "install phase is network-finalized")

vm_matches = [row for row in vms if row.get("vmid") in (vmid, str(vmid))]
require(len(vm_matches) == 1, "VMID is unique in live cluster resources")
vm = vm_matches[0]
require(vm.get("name") == name and vm.get("type") == "qemu",
        "live VM name and type match registry")
require(vm.get("node") == owner and owner in placement,
        "live owner matches registered placement")
require(vm.get("status") == "running", "live VM power state is running")

ha_matches = [row for row in ha if row.get("sid") == sid]
require(len(ha_matches) == 1, "exactly one HA resource exists")
require(str(ha_matches[0].get("state", "")).lower() == "started",
        "HA requested state is started")
require(not ha_matches[0].get("group"), "HA resource has no legacy group")
require(
    int(ha_matches[0].get("max_restart", -1)) == 3
    and int(ha_matches[0].get("max_relocate", -1)) == 1,
    "HA restart/relocate limits match policy",
)
require(
    int(ha_matches[0].get("failback", -1)) == 0
    and int(ha_matches[0].get("auto-rebalance", -1)) == 0,
    "HA failback and automatic rebalancing are disabled",
)

def values(raw):
    return raw if isinstance(raw, list) else [
        item for item in str(raw or "").split(",") if item
    ]

matching_rules = []
for rule in rules:
    if sid not in values(rule.get("resources")):
        continue
    nodes = {}
    for item in values(rule.get("nodes")):
        node, separator, priority = item.partition(":")
        nodes[node] = int(priority) if separator else 0
    strict = rule.get("strict", 0) in (1, True, "1")
    disabled = rule.get("disable", 0) in (1, True, "1")
    enabled = rule.get("enabled", 1) in (1, True, "1")
    if (
        rule.get("type") in (None, "node-affinity")
        and nodes == {node: 1 for node in placement}
        and strict
        and enabled
        and not disabled
    ):
        matching_rules.append(rule)
require(len(matching_rules) == 1,
        "exactly one enabled strict equal-priority node-affinity rule matches placement")

jobs = [row for row in replication if row.get("guest") in (vmid, str(vmid))]
expected_targets = placement - {owner}
actual_targets = {row.get("target") for row in jobs}
require(len(jobs) == len(expected_targets) and actual_targets == expected_targets,
        "replication jobs exactly match placement minus owner")

resource_routes = [row for row in routes if row.get("resource") == name]
expected_domains = {
    resource["domains"]["primary"],
    *resource["domains"]["aliases"],
}
require({row.get("domain") for row in resource_routes} == expected_domains,
        "registered routes exactly match primary and alias domains")
PY
then
  printf '\n  [PASS] Cross-layer production contract is internally consistent.\n'
else
  contract_ready=0
  printf '\n  [FAIL] Cross-layer production contract validation failed.\n'
fi

section "Live Proxmox, HA, replication, and disk state" \
  "capture the current runtime request, owner, affinity, replica health, and exact VM/storage configuration."
run "Live QEMU cluster resource row" \
  python3 - "$RUN_DIR/cluster-vms.json" "$VMID" <<'PY'
import json
import sys
rows = json.load(open(sys.argv[1], encoding="utf-8"))
print(json.dumps(
    [row for row in rows if row.get("vmid") in (int(sys.argv[2]), sys.argv[2])],
    indent=2,
    sort_keys=True,
))
PY
run "HA resource configuration" \
  python3 -m json.tool "${RUN_DIR}/ha-resources.json"
run "HA node-affinity rules" python3 -m json.tool "${RUN_DIR}/ha-rules.json"
run "HA manager runtime status" \
  node_exec "$COORDINATOR" ha-manager status
run "Replication job configuration" \
  python3 -m json.tool "${RUN_DIR}/replication.json"
if [[ -n "$OWNER_NODE" ]]; then
  run "VM power state" node_exec "$OWNER_NODE" qm status "$VMID"
  run "VM configuration" node_exec "$OWNER_NODE" qm config "$VMID"
fi
if [[ -n "$OWNER_NODE" && -n "$ROOT_VOLUME" ]]; then
  # shellcheck disable=SC2016 # This script is intentionally evaluated remotely.
  run "Root volume size, allocation, and snapshot properties" \
    node_exec "$OWNER_NODE" bash -c '
set -Eeuo pipefail
path="$(pvesm path "$1")"
case "$path" in
  /dev/zvol/*) dataset="${path#/dev/zvol/}" ;;
  *) printf "Unexpected root volume path: %s\n" "$path" >&2; exit 1 ;;
esac
zfs get -Hp -o name,property,value,source \
  volsize,refreservation,used,referenced,logicalused "$dataset"
zfs list -t snapshot -o name,creation,used,refer -r "$dataset"
' -- "$ROOT_VOLUME"
fi

section "QGA, private network, and strict root SSH" \
  "prove the running guest is reachable, QGA-responsive, identity-attested, and accessible through a strict mox jump path."
runtime_ready=1
if [[ -n "$OWNER_NODE" ]] &&
  node_exec "$OWNER_NODE" qm agent "$VMID" ping >/dev/null 2>&1; then
  printf '  [PASS] QEMU guest agent responds on %s.\n' "$OWNER_NODE"
else
  printf '  [FAIL] QEMU guest agent does not respond.\n'
  runtime_ready=0
fi

if mox_ssh "$COORDINATOR" ping -c 2 -W 1 "$PRIVATE_IP"; then
  printf '  [PASS] Private guest IP responds from %s.\n' "$COORDINATOR"
else
  printf '  [FAIL] Private guest IP does not respond from %s.\n' "$COORDINATOR"
  runtime_ready=0
fi

host_key=""
if [[ -n "$OWNER_NODE" ]]; then
  host_key_json="$(
    node_exec "$OWNER_NODE" qm guest exec "$VMID" --timeout 30 -- \
      /bin/cat /etc/ssh/ssh_host_ed25519_key.pub 2>/dev/null
  )" || host_key_json=""
  host_key="$(
    python3 - "$host_key_json" 2>/dev/null <<'PY'
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
    raise SystemExit(1)
print(key)
PY
  )" || host_key=""
fi

guest_known_hosts="${RUN_DIR}/guest-known-hosts"
base_known_hosts="${PROXMOX_SSH_KNOWN_HOSTS_FILE:-${HOME}/.ssh/known_hosts}"
if [[ -f "$base_known_hosts" && ! -L "$base_known_hosts" ]]; then
  install -m 0600 "$base_known_hosts" "$guest_known_hosts"
else
  : >"$guest_known_hosts"
  chmod 0600 "$guest_known_hosts"
fi

if [[ -n "$host_key" ]]; then
  printf '%s %s\n%s %s\n' \
    "$PRIVATE_IP" "$host_key" "$RESOURCE_NAME" "$host_key" \
    >>"$guest_known_hosts"
  original_known_hosts="${PROXMOX_SSH_KNOWN_HOSTS_FILE:-}"
  PROXMOX_SSH_KNOWN_HOSTS_FILE="$guest_known_hosts"
  export PROXMOX_SSH_KNOWN_HOSTS_FILE
  if ssh_via_mox "$COORDINATOR" "root@${PRIVATE_IP}" \
    bash -s <<'GUEST'
set -u
set -o pipefail
printf '%s\n' '--- identity ---'
id
hostnamectl
uptime
printf '%s\n' '--- services ---'
systemctl is-active qemu-guest-agent.service ssh.service
printf '%s\n' '--- networking ---'
ip -br -4 address
ip -4 route show
resolvectl status lan0
printf '%s\n' '--- storage ---'
lsblk -o NAME,TYPE,SIZE,FSTYPE,MOUNTPOINTS
df -hT
printf '%s\n' '--- cloud-init ---'
if command -v cloud-init >/dev/null 2>&1; then
  cloud-init status --long
else
  echo 'cloud-init is not installed'
fi
printf '%s\n' '--- Tailscale absence ---'
if command -v tailscale >/dev/null 2>&1; then
  echo 'FAIL: tailscale is installed'
  exit 1
else
  echo 'PASS: tailscale is absent'
fi
GUEST
  then
    printf '  [PASS] Strict root SSH and guest runtime inspection succeeded.\n'
  else
    printf '  [FAIL] Strict root SSH or guest runtime inspection failed.\n'
    runtime_ready=0
  fi
  if [[ -n "$original_known_hosts" ]]; then
    PROXMOX_SSH_KNOWN_HOSTS_FILE="$original_known_hosts"
    export PROXMOX_SSH_KNOWN_HOSTS_FILE
  else
    unset PROXMOX_SSH_KNOWN_HOSTS_FILE
  fi
else
  printf '  [FAIL] QGA did not return a valid Ed25519 SSH host key.\n'
  runtime_ready=0
fi

section "Production VM diagnostic verdict" \
  "summarize whether durable policy and live guest health agree."
if ((contract_ready && runtime_ready)); then
  printf '  [HEALTHY] %s is active, HA-managed, replicated, routed, QGA-responsive, and reachable by strict SSH.\n' \
    "$RESOURCE_NAME"
  exit 0
fi
printf '  [UNHEALTHY] %s has one or more failed checks above.\n' "$RESOURCE_NAME"
exit 1
