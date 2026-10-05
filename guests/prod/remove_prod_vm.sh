#!/usr/bin/env bash

# Copyright (c) 2026 BEENTHERE VENTURES, INC.
# SPDX-License-Identifier: GPL-3.0-only

# Destructively remove one registry-backed production VM and every owned
# Proxmox/HA/replication/route artifact. The shared source ISO cache is kept.

set -Eeuo pipefail
set +x
umask 077

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
REPO_ROOT="$(cd -- "${SCRIPT_DIR}/../.." && pwd -P)"
CONFIG_LIB="${REPO_ROOT}/lib/config.sh"
REMOTE_ROOT="/usr/local/lib/app-ha-proxmox"
REMOTE_REGISTRY="${REMOTE_ROOT}/lib/cluster_registry.py"
REMOTE_HAPROXY_SYNC="${REMOTE_ROOT}/lib/sync_haproxy_routes.sh"

DRY_RUN=false
RESOURCE_NAME=""
COORDINATOR=""
RUN_DIR=""
LEASE_NONCE=""
LEASE_ACQUIRED=false

die() {
  printf '\nERROR: %s\n' "$*" >&2
  exit 1
}

log() {
  printf '\n==> %s\n' "$*"
}

info() {
  printf '    %s\n' "$*"
}

usage() {
  cat <<'EOF'
Usage: destroy_prod_vm.sh [--dry-run] prodN

Permanently removes one production VM after exact identity validation:
routes, HA request and node-affinity rule, replication jobs and target copies,
QEMU config and owned volumes, orchestration metadata, and registry allocation.
The shared hash-addressed source ISO cache is deliberately retained.
EOF
}

while (($#)); do
  case "$1" in
    --dry-run)
      DRY_RUN=true
      shift
      ;;
    -h | --help)
      usage
      exit 0
      ;;
    *)
      [[ -z "$RESOURCE_NAME" ]] || die "Only one production resource may be destroyed"
      RESOURCE_NAME="$1"
      shift
      ;;
  esac
done

[[ "$RESOURCE_NAME" =~ ^prod[1-9][0-9]*$ ]] ||
  die "A production resource name such as prod1 is required"
[[ -f "$CONFIG_LIB" && ! -L "$CONFIG_LIB" ]] ||
  die "Configuration library is unavailable"
# shellcheck disable=SC1090
source "$CONFIG_LIB"
load_proxmox_config --no-secrets ||
  die "Could not load cluster configuration"

for command_name in awk bash date install mktemp paste python3 scp ssh; do
  command -v "$command_name" >/dev/null 2>&1 ||
    die "Required workstation command is unavailable: $command_name"
done

RUN_DIR="$(mktemp -d "${TMPDIR:-/tmp}/destroy-prod-vm.XXXXXX")"
chmod 0700 "$RUN_DIR"

cleanup() {
  local code=$?
  trap - EXIT
  set +e
  if [[ "$LEASE_ACQUIRED" == true && -n "$COORDINATOR" ]]; then
    registry_cmd orchestration-release "$RESOURCE_NAME" \
      --nonce "$LEASE_NONCE" >/dev/null 2>&1
  fi
  rm -rf -- "$RUN_DIR"
  exit "$code"
}
trap cleanup EXIT

COORDINATOR="$(first_reachable_mox)" ||
  die "No reachable mox coordinator was found"

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

resource_json="$(registry_cmd get "$RESOURCE_NAME")" ||
  die "Production resource $RESOURCE_NAME is not registered"
orchestration_json="$(registry_cmd orchestration-get "$RESOURCE_NAME")" ||
  die "Production orchestration metadata is unavailable"
resources_json="$(registry_cmd list)" || die "Could not list registry resources"
cluster_vms_json="$(pvesh_get /cluster/resources --type vm)" ||
  die "Could not list live VMs"
nodes_json="$(pvesh_get /nodes)" || die "Could not list cluster nodes"
ha_resources_json="$(pvesh_get /cluster/ha/resources)" ||
  die "Could not list HA resources"
replication_json="$(pvesh_get /cluster/replication)" ||
  die "Could not list replication jobs"
rules_json="$(
  node_exec "$COORDINATOR" ha-manager rules config --output-format json
)" || die "Could not list HA rules"

write_json "$RUN_DIR/resource.json" "$resource_json"
write_json "$RUN_DIR/orchestration.json" "$orchestration_json"
write_json "$RUN_DIR/resources.json" "$resources_json"
write_json "$RUN_DIR/cluster-vms.json" "$cluster_vms_json"
write_json "$RUN_DIR/nodes.json" "$nodes_json"
write_json "$RUN_DIR/ha-resources.json" "$ha_resources_json"
write_json "$RUN_DIR/replication.json" "$replication_json"
write_json "$RUN_DIR/rules.json" "$rules_json"

fields=()
mapfile -d '' -t fields < <(
  python3 - \
    "$RUN_DIR/resource.json" "$RUN_DIR/orchestration.json" \
    "$RUN_DIR/resources.json" "$RUN_DIR/cluster-vms.json" \
    "$RUN_DIR/nodes.json" "$RUN_DIR/ha-resources.json" \
    "$RUN_DIR/replication.json" "$RUN_DIR/rules.json" <<'PY'
import json
import re
import sys

resource, orchestration, resources, live, nodes, ha, replication, rules = [
    json.load(open(path, encoding="utf-8")) for path in sys.argv[1:]
]
if resource["kind"] != "production":
    raise SystemExit("resource is not production")
dependents = [
    row["name"] for row in resources if row.get("source") == resource["name"]
]
if dependents:
    raise SystemExit(
        "destroy staging dependents first: " + ", ".join(sorted(dependents))
    )
placement = resource["placement"]
online = {
    row.get("node", row.get("name"))
    for row in nodes
    if str(row.get("status", "")).lower() == "online"
    or row.get("online") in (1, True)
}
if set(placement) - online:
    raise SystemExit("every production placement node must be online")

vmid = int(resource["vmid"])
sid = f"vm:{vmid}"
matches = [row for row in live if row.get("vmid") in (vmid, str(vmid))]
teardown_states = {"stopping", "stopped", "failed", "reserved"}
if len(matches) == 1:
    vm = matches[0]
    if (
        vm.get("name") != resource["name"]
        or vm.get("type") != "qemu"
        or vm.get("node") not in placement
    ):
        raise SystemExit("live VM identity/owner differs from registry")
    owner = vm["node"]
    live_present = True
    if resource["owner_node"] not in (None, owner):
        raise SystemExit("registry owner differs from live owner")
elif len(matches) == 0 and resource["state"] in teardown_states:
    owner = resource["owner_node"] or resource["initial_node"]
    if owner not in placement:
        raise SystemExit("registry lacks a safe owner for partial teardown")
    live_present = False
else:
    raise SystemExit("live production VM identity is not unique")

ha_matches = [row for row in ha if row.get("sid") == sid]
if len(ha_matches) > 1:
    raise SystemExit("duplicate HA resource")
ha_state = ""
if ha_matches:
    if ha_matches[0].get("group"):
        raise SystemExit("production HA resource uses a legacy group")
    if (
        int(ha_matches[0].get("max_restart", -1)) != 3
        or int(ha_matches[0].get("max_relocate", -1)) != 1
        or int(ha_matches[0].get("failback", -1)) != 0
        or int(ha_matches[0].get("auto-rebalance", -1)) != 0
    ):
        raise SystemExit("production HA resource policy differs from contract")
    ha_state = str(ha_matches[0].get("state", ""))

def values(raw):
    return raw if isinstance(raw, list) else [
        value for value in str(raw or "").split(",") if value
    ]

rule_ids = []
for rule in rules:
    rule_resources = set(values(rule.get("resources")))
    if sid not in rule_resources:
        continue
    priorities = {}
    for part in values(rule.get("nodes")):
        node, separator, priority = part.partition(":")
        priorities[node] = int(priority) if separator else 0
    if (
        rule_resources != {sid}
        or
        rule.get("type") not in (None, "node-affinity")
        or not rule.get("strict") in (1, True, "1")
        or priorities != {node: 1 for node in placement}
    ):
        raise SystemExit("HA rule referencing VM differs from strict placement")
    rule_ids.append(str(rule.get("rule", rule.get("id", ""))))
if len(rule_ids) > 1 or any(not re.fullmatch(r"[A-Za-z0-9][A-Za-z0-9_.-]*", value) for value in rule_ids):
    raise SystemExit("production VM has ambiguous HA rules")

jobs = [row for row in replication if row.get("guest") in (vmid, str(vmid))]
job_ids = [str(row["id"]) for row in jobs]
expected_targets = set(placement) - {owner}
actual_targets = {row.get("target") for row in jobs}
if resource["state"] in teardown_states:
    if not actual_targets.issubset(expected_targets):
        raise SystemExit("remaining replication target is outside placement")
elif actual_targets != expected_targets:
    raise SystemExit("replication targets differ from placement minus owner")
if any(not re.fullmatch(r"[1-9][0-9]{2,8}-[0-9]+", value) for value in job_ids):
    raise SystemExit("replication job ID is unsafe")

startup = orchestration.get("startup_sha256") or "none"
values_out = (
    resource["name"],
    resource["id"],
    resource["state"],
    resource["revision"],
    vmid,
    resource["purpose"]["slug"],
    owner,
    "1" if live_present else "0",
    resource["initial_node"],
    ",".join(placement),
    resource["proxmox"]["volume_id"] or "",
    "1" if resource["routes_enabled"] else "0",
    orchestration.get("final_network_enabled") is True and "yes" or "no",
    startup,
    orchestration.get("source_iso_sha256", ""),
    orchestration.get("install_mode", "ubuntu-autoinstall"),
    ha_state,
    ",".join(rule_ids),
    ",".join(job_ids),
)
for value in values_out:
    sys.stdout.buffer.write(str(value).encode() + b"\0")
PY
) || die "Production destruction preflight failed"
((${#fields[@]} == 19)) || die "Could not parse destruction preflight"

RESOURCE_ID="${fields[1]}"
RESOURCE_STATE="${fields[2]}"
RESOURCE_REVISION="${fields[3]}"
VMID="${fields[4]}"
PURPOSE="${fields[5]}"
OWNER_NODE="${fields[6]}"
LIVE_PRESENT="${fields[7]}"
INITIAL_NODE="${fields[8]}"
IFS=',' read -r -a PLACEMENT_NODES <<<"${fields[9]}"
ROOT_VOLUME="${fields[10]}"
ROUTES_ENABLED="${fields[11]}"
FINAL_NETWORK="${fields[12]}"
STARTUP_SHA256="${fields[13]}"
SOURCE_ISO_SHA256="${fields[14]}"
INSTALL_MODE="${fields[15]}"
HA_STATE="${fields[16]}"
IFS=',' read -r -a HA_RULES <<<"${fields[17]}"
[[ -n "${fields[17]}" ]] || HA_RULES=()
IFS=',' read -r -a REPLICATION_JOBS <<<"${fields[18]}"
[[ -n "${fields[18]}" ]] || REPLICATION_JOBS=()

validate_live_vm_config() {
  config_text="$(node_exec "$OWNER_NODE" qm config "$VMID")" ||
    return 1
  python3 - "$config_text" \
    "$RESOURCE_NAME" "$VMID" "$PURPOSE_TAG_PREFIX" "$PURPOSE" \
    "$PRODUCTION_VM_TAG" "$ROOT_VOLUME" "$PROD_VM_STORAGE" \
    "$GUEST_ROLE_HOOK_PATH" <<'PY'
import re
import sys
config = {}
for line in sys.argv[1].splitlines():
    if ": " in line:
        key, value = line.split(": ", 1)
        config[key] = value
name, vmid, prefix, purpose, prod_tag, volume, storage, hook = sys.argv[2:]
tags = {value for value in re.split(r"[;,]", config.get("tags", "")) if value}
if config.get("name") != name:
    raise SystemExit("VM name differs from registry")
if tags != {prod_tag, prefix + purpose}:
    raise SystemExit("VM tags differ from exact production identity")
if config.get("scsi0", "").split(",", 1)[0] != volume:
    raise SystemExit("root volume differs from registry")
if config.get("efidisk0", "").split(",", 1)[0] != f"{storage}:vm-{vmid}-disk-0":
    raise SystemExit("EFI volume differs from expected identity")
allowed = {"scsi0", "efidisk0"}
disks = {
    key for key in config
    if re.fullmatch(r"(?:scsi|sata|virtio|ide|unused|efidisk|tpmstate)[0-9]+", key)
    and "media=cdrom" not in config[key]
}
if disks != allowed:
    raise SystemExit("VM has unexpected disks")
if config.get("hookscript", "") not in {"", hook}:
    raise SystemExit("VM lifecycle hook differs from contract")
PY
}

if [[ "$LIVE_PRESENT" == 1 ]]; then
  validate_live_vm_config ||
    die "Live VM configuration failed exact destruction identity validation"
fi

EFI_VOLUME="${PROD_VM_STORAGE}:vm-${VMID}-disk-0"
for node in "${PLACEMENT_NODES[@]}"; do
  volume_inventory="$(
    node_exec "$node" pvesm list "$PROD_VM_STORAGE" --vmid "$VMID"
  )" || die "Could not inspect VMID-owned volumes on $node"
  python3 - "$volume_inventory" "$ROOT_VOLUME" "$EFI_VOLUME" \
    "$RESOURCE_STATE" <<'PY' ||
import sys

lines = [line.split() for line in sys.argv[1].splitlines()[1:] if line.split()]
observed = {parts[0] for parts in lines}
expected = {value for value in sys.argv[2:4] if value}
state = sys.argv[4]
if not observed.issubset(expected):
    raise SystemExit("unvalidated VMID-owned volume exists: " + ", ".join(sorted(observed - expected)))
if state not in {"stopping", "stopped", "failed", "reserved"} and observed != expected:
    raise SystemExit("production volume set is incomplete before teardown")
PY
    die "VMID-owned volume validation failed on $node"
done

CACHE_ISO="/var/lib/app-ha-proxmox/iso-cache/${SOURCE_ISO_SHA256}.iso"
CACHE_SIDECAR="${CACHE_ISO}.sha256"
CACHE_WAS_VALID=false
# shellcheck disable=SC2016 # This script is intentionally evaluated remotely.
cache_state="$(
  node_exec "$INITIAL_NODE" bash -c '
set -Eeuo pipefail
iso="$1"; sidecar="$2"; expected="$3"
if [[ ! -e "$iso" && ! -e "$sidecar" ]]; then
  printf "absent\n"
  exit 0
fi
[[ -f "$iso" && ! -L "$iso" && -f "$sidecar" && ! -L "$sidecar" ]]
read -r observed _ < <(sha256sum "$iso")
[[ "$observed" == "$expected" ]]
[[ "$(<"$sidecar")" == "$expected  $(basename -- "$iso")" ]]
printf "valid\n"
' -- "$CACHE_ISO" "$CACHE_SIDECAR" "$SOURCE_ISO_SHA256"
)" || die "Shared source cache exists but failed integrity validation"
[[ "$cache_state" != valid ]] || CACHE_WAS_VALID=true

log "Validated production destruction plan"
info "Resource: $RESOURCE_NAME / VMID $VMID / state $RESOURCE_STATE"
info "Registry resource ID: $RESOURCE_ID"
info "Owner: $OWNER_NODE; placement: ${PLACEMENT_NODES[*]}"
info "Live VM present: $LIVE_PRESENT"
info "Root volume: $ROOT_VOLUME"
info "EFI volume: $EFI_VOLUME"
info "HA state/rules: ${HA_STATE:-none} / ${HA_RULES[*]:-none}"
info "Replication jobs: ${REPLICATION_JOBS[*]:-none}"
info "Routes enabled: $ROUTES_ENABLED"
info "Shared source cache: $CACHE_ISO ($cache_state; retained)"

if [[ "$DRY_RUN" == true ]]; then
  log "Dry run complete; no state was changed"
  exit 0
fi

printf '\nTHIS PERMANENTLY DESTROYS %s, ALL OWNED VM DISKS/REPLICAS, HA/RULES, ROUTES, AND REGISTRY METADATA.\n' \
  "$RESOURCE_NAME"
printf 'Type exactly: DESTROY %s\n> ' "$RESOURCE_NAME"
IFS= read -r confirmation
[[ "$confirmation" == "DESTROY ${RESOURCE_NAME}" ]] ||
  die "Destruction confirmation did not match"

LEASE_NONCE="$(python3 - <<'PY'
import uuid
print(uuid.uuid4().hex)
PY
)"
registry_cmd orchestration-acquire "$RESOURCE_NAME" \
  --nonce "$LEASE_NONCE" \
  --owner "destroy-${COORDINATOR}-$$" \
  --ttl-seconds 7200 \
  --final-network-enabled "$FINAL_NETWORK" \
  --startup-sha256 "$STARTUP_SHA256" \
  --source-iso-sha256 "$SOURCE_ISO_SHA256" \
  --install-mode "$INSTALL_MODE" >/dev/null ||
  die "Could not acquire the production orchestration lease"
LEASE_ACQUIRED=true

registry_update() {
  local value
  value="$(registry_cmd update "$RESOURCE_NAME" \
    --expected-revision "$RESOURCE_REVISION" "$@")" ||
    die "Registry update failed during destruction"
  RESOURCE_REVISION="$(
    python3 -c \
      'import json,sys; print(json.loads(sys.argv[1])["revision"])' "$value"
  )"
}

log "Disabling ingress routes and entering teardown state"
case "$RESOURCE_STATE" in
  active)
    registry_update --state stopping --routes-disabled
    RESOURCE_STATE=stopping
    ;;
  ready | replicating | provisioning)
    registry_update --state stopped --routes-disabled
    RESOURCE_STATE=stopped
    ;;
  stopping | stopped | reserved | failed)
    registry_update --routes-disabled
    ;;
  *)
    die "Unsupported production destruction state: $RESOURCE_STATE"
    ;;
esac
ROUTES_ENABLED=0
info "Waiting up to 240 seconds for any current HAProxy route sync to finish, then converging route removal."
mox_ssh "$COORDINATOR" "$REMOTE_HAPROXY_SYNC" \
  --lock-timeout 240 </dev/null ||
  die "Could not converge HAProxy after route removal"

if [[ -n "$HA_STATE" ]]; then
  log "Stopping and removing the HA resource"
  if [[ "$LIVE_PRESENT" == 1 ]]; then
    node_exec "$COORDINATOR" ha-manager set "vm:${VMID}" --state stopped
    deadline=$((SECONDS + 300))
    while ((SECONDS < deadline)); do
      status="$(node_exec "$OWNER_NODE" qm status "$VMID" 2>/dev/null || true)"
      [[ "$status" == "status: stopped" ]] && break
      sleep 5
    done
    [[ "$status" == "status: stopped" ]] ||
      die "HA did not stop VM $VMID within 300 seconds"
  fi
  node_exec "$COORDINATOR" ha-manager remove "vm:${VMID}"
elif [[ "$LIVE_PRESENT" == 1 ]]; then
  log "Stopping the non-HA production VM"
  status="$(node_exec "$OWNER_NODE" qm status "$VMID")"
  if [[ "$status" == "status: running" ]]; then
    node_exec "$OWNER_NODE" qm shutdown "$VMID" --timeout 60 ||
      node_exec "$OWNER_NODE" qm stop "$VMID" --timeout 30
  fi
  [[ "$(node_exec "$OWNER_NODE" qm status "$VMID")" == "status: stopped" ]] ||
    die "Non-HA VM $VMID could not be proven stopped"
fi

for rule in "${HA_RULES[@]}"; do
  log "Removing HA node-affinity rule $rule"
  node_exec "$COORDINATOR" ha-manager rules remove "$rule"
done

for job in "${REPLICATION_JOBS[@]}"; do
  log "Removing replication job $job and its target volumes"
  node_exec "$COORDINATOR" pvesr delete "$job"
done

deadline=$((SECONDS + 600))
while ((SECONDS < deadline)); do
  current_replication="$(pvesh_get /cluster/replication)" || current_replication=""
  remaining="$(
    python3 - "$current_replication" "$VMID" 2>/dev/null <<'PY' || printf unknown
import json
import sys
rows = json.loads(sys.argv[1])
print(sum(row.get("guest") in (int(sys.argv[2]), sys.argv[2]) for row in rows))
PY
  )"
  [[ "$remaining" == 0 ]] && break
  sleep 5
done
[[ "$remaining" == 0 ]] ||
  die "Replication jobs did not finish removal"

for node in "${PLACEMENT_NODES[@]}"; do
  [[ "$node" == "$OWNER_NODE" ]] && continue
  deadline=$((SECONDS + 600))
  lines=-1
  while ((SECONDS < deadline)); do
    volumes="$(
      node_exec "$node" pvesm list "$PROD_VM_STORAGE" --vmid "$VMID"
    )" || die "Could not inspect replica volumes on $node"
    lines="$(awk 'NR > 1 && NF {count++} END {print count+0}' <<<"$volumes")"
    ((lines == 0)) && break
    sleep 5
  done
  ((lines == 0)) ||
    die "Replica volumes remain on $node after replication cleanup"
done

if [[ "$LIVE_PRESENT" == 1 ]]; then
  latest_live_json="$(pvesh_get /cluster/resources --type vm)"
  python3 - "$latest_live_json" "$VMID" "$RESOURCE_NAME" "$OWNER_NODE" <<'PY' ||
import json
import sys
rows = json.loads(sys.argv[1])
wanted = int(sys.argv[2])
matches = [row for row in rows if row.get("vmid") in (wanted, str(wanted))]
raise SystemExit(
    0
    if len(matches) == 1
    and matches[0].get("name") == sys.argv[3]
    and matches[0].get("type") == "qemu"
    and matches[0].get("node") == sys.argv[4]
    and matches[0].get("status") == "stopped"
    else 1
)
PY
    die "VM identity, owner, or stopped state changed before destruction"
  validate_live_vm_config ||
    die "VM configuration changed before destruction"
  owner_inventory="$(
    node_exec "$OWNER_NODE" pvesm list "$PROD_VM_STORAGE" --vmid "$VMID"
  )" || die "Could not revalidate owner volumes before destruction"
  python3 - "$owner_inventory" "$ROOT_VOLUME" "$EFI_VOLUME" <<'PY' ||
import sys
observed = {
    line.split()[0]
    for line in sys.argv[1].splitlines()[1:]
    if line.split()
}
raise SystemExit(0 if observed == set(sys.argv[2:4]) else 1)
PY
    die "Owner VMID volume set changed before destruction"
  log "Destroying the validated QEMU config and owner volumes"
  node_exec "$OWNER_NODE" qm destroy "$VMID" \
    --purge 1 --destroy-unreferenced-disks 0
fi

live_json="$(pvesh_get /cluster/resources --type vm)"
python3 - "$live_json" "$VMID" <<'PY' ||
import json
import sys
rows = json.loads(sys.argv[1])
wanted = int(sys.argv[2])
raise SystemExit(
    1 if any(row.get("vmid") in (wanted, str(wanted)) for row in rows) else 0
)
PY
  die "VMID $VMID remains in live cluster resources"

for node in "${PLACEMENT_NODES[@]}"; do
  volumes="$(
    node_exec "$node" pvesm list "$PROD_VM_STORAGE" --vmid "$VMID"
  )" || die "Could not inspect final volume state on $node"
  lines="$(awk 'NR > 1 && NF {count++} END {print count+0}' <<<"$volumes")"
  ((lines == 0)) || die "VM-owned volumes remain on $node"
done

installer_name="production-${RESOURCE_NAME}-installer.iso"
# shellcheck disable=SC2016 # This script is intentionally evaluated remotely.
node_exec "$INITIAL_NODE" bash -c '
path="$(pvesm path "$1" 2>/dev/null || true)"
[[ -z "$path" || "$path" =~ ^/[A-Za-z0-9._/-]+$ ]] || exit 1
[[ -z "$path" ]] || rm -f -- "$path" "${path}.partial"
' -- "${ISO_STORAGE_ID}:iso/${installer_name}" ||
  die "Could not remove known per-VM installer media"

log "Clearing external metadata and releasing the registry allocation"
update_args=(--clear-owner-node --clear-volume --ha-nodes "" --replication-targets "")
if [[ "$RESOURCE_STATE" == stopping ]]; then
  update_args=(--state stopped "${update_args[@]}")
  RESOURCE_STATE=stopped
fi
registry_update "${update_args[@]}"

registry_cmd orchestration-release "$RESOURCE_NAME" \
  --nonce "$LEASE_NONCE" >/dev/null ||
  die "Could not release orchestration lease"
LEASE_ACQUIRED=false
registry_cmd release "$RESOURCE_NAME" >/dev/null ||
  die "Could not archive and release the registry resource"

if [[ "$CACHE_WAS_VALID" == true ]]; then
  # shellcheck disable=SC2016 # This script is intentionally evaluated remotely.
  node_exec "$INITIAL_NODE" bash -c '
set -Eeuo pipefail
iso="$1"; sidecar="$2"; expected="$3"
[[ -f "$iso" && ! -L "$iso" && -f "$sidecar" && ! -L "$sidecar" ]]
read -r observed _ < <(sha256sum "$iso")
[[ "$observed" == "$expected" ]]
[[ "$(<"$sidecar")" == "$expected  $(basename -- "$iso")" ]]
' -- "$CACHE_ISO" "$CACHE_SIDECAR" "$SOURCE_ISO_SHA256" ||
    die "Shared source cache changed during teardown"
fi

log "Production VM destruction complete"
info "Destroyed resource: $RESOURCE_NAME (former VMID $VMID)"
info "Removed routes, HA resource/rules, replication, owner and replica volumes, and registry metadata."
info "Retained shared source cache: $CACHE_ISO"
