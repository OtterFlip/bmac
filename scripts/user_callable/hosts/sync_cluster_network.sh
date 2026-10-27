#!/usr/bin/env bash

# Copyright (c) 2026 BEENTHERE VENTURES, INC.
# SPDX-License-Identifier: GPL-3.0-only

# Push cluster.conf's networking settings, and each host's moxN.conf transfer
# cap, to every Proxmox host: the VM slots (registry policy, guest egress NAT
# and tunnel config), the shared guest gateway, the probe target list, and the
# Cloudflare Tunnel credentials. Run it after editing those settings; it is
# rarely needed, since they are normally set once when the cluster is built.

set -Eeuo pipefail
set +x
umask 077

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
REPO_ROOT="$(cd -- "${SCRIPT_DIR}/../../.." && pwd -P)"
# shellcheck source=../../lib/ui_protocol.sh
source "${REPO_ROOT}/scripts/lib/ui_protocol.sh"
# shellcheck source=../../lib/guest_network_settings.sh
source "${REPO_ROOT}/scripts/lib/guest_network_settings.sh"
CONFIG_LIB="${REPO_ROOT}/scripts/lib/config.sh"
INSTALL_ROOT="/usr/local/lib/app-ha-proxmox"
REMOTE_LIB="${INSTALL_ROOT}/lib"
REMOTE_CLUSTER_CONF="${INSTALL_ROOT}/config/cluster.conf"
REMOTE_SETTINGS=/etc/app-ha/guest-network.env

DRY_RUN=false
ASSUME_YES=false
COORDINATOR=""
RUN_DIR=""
declare -a CLUSTER_NODES=()
declare -A DRIFT=()

log() {
  printf '\n==> %s\n' "$*"
  bmac_ui_step "$*"
}

info() {
  printf '    %s\n' "$*"
}

warn() {
  printf 'WARNING: %s\n' "$*" >&2
}

die() {
  printf '\nERROR: %s\n' "$*" >&2
  bmac_ui_error failed "$*"
  exit 1
}

usage() {
  cat <<'EOF'
Usage: sync_cluster_network.sh [--dry-run] [--yes]

Brings every Proxmox host in line with config/cluster.conf and its
config/moxN.conf:

  - the registry's VM slot settings (refused if any registered VM would move);
  - the installed cluster.conf;
  - the shared guest gateway, egress NAT range, and fallback routing tables;
  - the monthly transfer cap and reset day;
  - the egress probe target list (revalidated);
  - the Cloudflare Tunnel credentials and every host's tunnel config.

--dry-run shows which hosts differ without changing anything.

Options:
  --dry-run  Show drift only.
  --yes      Skip the interactive GO confirmation.
  -h, --help
EOF
}

parse_args() {
  while (($#)); do
    case "$1" in
      --dry-run)
        DRY_RUN=true
        ;;
      --yes)
        ASSUME_YES=true
        ;;
      -h | --help)
        usage
        exit 0
        ;;
      *)
        die "Unknown option: $1"
        ;;
    esac
    shift
  done
}

load_config() {
  [[ -f "$CONFIG_LIB" && ! -L "$CONFIG_LIB" ]] || die "Missing $CONFIG_LIB"
  # shellcheck disable=SC1090,SC1091 # Resolved from this script's path.
  source "$CONFIG_LIB"
  load_proxmox_config --require-secrets >/dev/null ||
    die "Could not load cluster.conf and secrets.env"
  require_var CLUSTER_STATE_DIR
  require_var CLOUDFLARE_TUNNEL_SECRET
}

discover_cluster() {
  COORDINATOR="$(first_reachable_mox)" ||
    die "No reachable Proxmox coordinator was found"
  local nodes_json
  nodes_json="$(
    mox_ssh "$COORDINATOR" pvesh get /nodes --output-format json </dev/null
  )" || die "Could not query cluster membership through $COORDINATOR"
  mapfile -t CLUSTER_NODES < <(
    python3 - "$nodes_json" "$MAX_MOX_HOSTS" <<'PY'
import json
import re
import sys

rows = json.loads(sys.argv[1])
maximum = int(sys.argv[2])
nodes = []
for row in rows:
    name = str(row.get("node"))
    match = re.fullmatch(r"mox([1-9]|10)", name)
    if not match or int(match.group(1)) > maximum:
        raise SystemExit(f"unexpected cluster node: {name!r}")
    if row.get("status") != "online":
        raise SystemExit(f"cluster node is not online: {name}")
    nodes.append(name)
for name in sorted(nodes, key=lambda value: int(value[3:])):
    print(name)
PY
  )
  ((${#CLUSTER_NODES[@]} > 0)) || die "Every cluster node must be online"
  local node
  for node in "${CLUSTER_NODES[@]}"; do
    mox_is_reachable "$node" ||
      die "$node is online in Proxmox but unreachable from this workstation"
  done
  info "Coordinator: $COORDINATOR"
  info "Online nodes: ${CLUSTER_NODES[*]}"
}

copy_to_node() {
  local node="$1" source="$2" destination="$3" ssh_destination
  _mox_ssh_options "$node"
  ssh_destination="$(_mox_ssh_destination "$node")"
  scp "${MOX_SSH_OPTIONS[@]}" -- "$source" \
    "${MOX_SSH_USER:-root}@${ssh_destination}:${destination}"
}

# Write each host's desired settings file under RUN_DIR, from its moxN.conf.
render_settings() {
  local node
  for node in "${CLUSTER_NODES[@]}"; do
    (
      load_proxmox_config --host "$node" --no-secrets >/dev/null ||
        die "Could not load config/${node}.conf"
      guest_network_settings
    ) >"${RUN_DIR}/${node}.env" || die "Could not render ${node}'s settings"
  done
}

# Compare the workstation-provided keys only; the host adds its NIC names.
settings_differ() {
  local node="$1" installed
  installed="$(mox_ssh "$node" cat "$REMOTE_SETTINGS" </dev/null 2>/dev/null || true)"
  ! diff -q <(sort "${RUN_DIR}/${node}.env") \
    <(grep -vE '^BMAC_(PUBLIC_NIC|PRIVATE_NIC|PUBLIC_IF)=' <<<"$installed" | sort) \
    >/dev/null
}

check_drift() {
  local node local_conf_hash remote_conf_hash
  local_conf_hash="$(sha256sum "$PROXMOX_CLUSTER_CONFIG" | awk '{print $1}')"
  for node in "${CLUSTER_NODES[@]}"; do
    local -a items=()
    remote_conf_hash="$(
      mox_ssh "$node" sha256sum "$REMOTE_CLUSTER_CONF" </dev/null 2>/dev/null |
        awk '{print $1}'
    )" || true
    [[ "$remote_conf_hash" == "$local_conf_hash" ]] || items+=("cluster.conf")
    settings_differ "$node" && items+=("guest network settings")
    mox_ssh "$node" "${REMOTE_LIB}/tunnel_sync.sh" --check </dev/null >/dev/null 2>&1 ||
      items+=("tunnel config")
    DRIFT["$node"]="${items[*]}"
  done
  local policy
  policy="$(
    mox_ssh "$COORDINATOR" "${REMOTE_LIB}/cluster_registry.py" \
      --state-dir "$CLUSTER_STATE_DIR" list --record-type policy </dev/null 2>/dev/null ||
      true
  )"
  SLOTS_DIFFER=false
  python3 - "$policy" "$VM_SLOT_START_OCTET" "$MAX_PROD_VMS" "$MAX_STAGING_VMS_PER_PROD" <<'PY' ||
import json
import sys

policy = json.loads(sys.argv[1])
slots = policy["network"]["vm_slots"]
wanted = {
    "start": int(sys.argv[2]),
    "max_production": int(sys.argv[3]),
    "max_staging_per_production": int(sys.argv[4]),
}
raise SystemExit(0 if slots == wanted else 1)
PY
    SLOTS_DIFFER=true
}

show_drift() {
  local node any=false
  printf '\n%-8s %s\n' "Node" "Differs from the configuration"
  for node in "${CLUSTER_NODES[@]}"; do
    printf '%-8s %s\n' "$node" "${DRIFT[$node]:-nothing}"
    [[ -z "${DRIFT[$node]}" ]] || any=true
  done
  if [[ "$SLOTS_DIFFER" == true ]]; then
    printf '\nThe registry VM slot settings differ from cluster.conf.\n'
    any=true
  fi
  [[ "$any" == true ]] && DRIFT_FOUND=true || DRIFT_FOUND=false
}

confirm_sync() {
  [[ "$ASSUME_YES" == true ]] && return 0
  local entered
  printf '\nThis reapplies the network settings, restarts egress health checks, and\n'
  printf 'restarts tunnel replicas one host at a time on: %s\n' "${CLUSTER_NODES[*]}"
  if bmac_ui_is_json; then
    bmac_ui_confirm --id go --severity warning --title "Sync the cluster with cluster.conf?" \
      --message "This reapplies the network settings on ${CLUSTER_NODES[*]} and restarts each tunnel replica whose config changed, one host at a time." \
      --confirm-label "Sync the cluster" --text GO ||
      die "Confirmation did not match GO; no changes were made"
    return 0
  fi
  printf 'Type GO to continue.\n> '
  IFS= read -r entered
  [[ "$entered" == GO ]] || die "Confirmation did not match GO; no changes were made"
}

sync_slots() {
  [[ "$SLOTS_DIFFER" == true ]] || return 0
  log "Updating the registry's VM slot settings"
  mox_ssh "$COORDINATOR" "${REMOTE_LIB}/cluster_registry.py" \
    --state-dir "$CLUSTER_STATE_DIR" update-slots \
    --vm-slot-start "$VM_SLOT_START" \
    --max-production "$MAX_PROD_VMS" \
    --max-staging-per-production "$MAX_STAGING_VMS_PER_PROD" </dev/null >/dev/null ||
    die "The registry refused the new VM slot settings; no host was changed"
}

sync_node() {
  local node="$1" secret_file status=0
  log "Applying the network settings on $node"
  copy_to_node "$node" "$PROXMOX_CLUSTER_CONFIG" /root/.app-ha-cluster.conf
  # The lifecycle hook and the tunnel sync parse the installed cluster.conf
  # with the host's config.sh. If that parser rejects the new file (it
  # predates a key, so update_cluster_runtime.sh has not run), put the
  # previous file back so guests keep starting.
  # shellcheck disable=SC2016 # Evaluated on the host.
  mox_ssh "$node" bash -c '
set -Eeuo pipefail
install -o root -g root -m 0644 /root/.app-ha-cluster.conf "$1.new"
rm -f /root/.app-ha-cluster.conf
cp -p "$1" "$1.previous"
mv -f "$1.new" "$1"
if ! "$2/config.sh" --check --no-secrets >/dev/null; then
  mv -f "$1.previous" "$1"
  exit 3
fi
rm -f "$1.previous"
' bash "$REMOTE_CLUSTER_CONF" "$REMOTE_LIB" </dev/null || status=$?
  ((status != 3)) ||
    die "$node's installed config.sh rejects this cluster.conf, so $node keeps its previous copy. Run update_cluster_runtime.sh first, then sync again."
  ((status == 0)) || die "Could not install cluster.conf on $node"
  copy_to_node "$node" "${RUN_DIR}/${node}.env" /root/.app-ha-guest-network.env
  mox_ssh "$node" "${REMOTE_LIB}/guest_network.sh" configure \
    /root/.app-ha-guest-network.env </dev/null
  mox_ssh "$node" rm -f /root/.app-ha-guest-network.env </dev/null
  secret_file="${RUN_DIR}/tunnel-secret"
  printf '%s\n' "$CLOUDFLARE_TUNNEL_SECRET" >"$secret_file"
  copy_to_node "$node" "$secret_file" /root/.app-ha-tunnel-secret
  rm -f "$secret_file"
  # shellcheck disable=SC2016 # Evaluated on the host.
  mox_ssh "$node" bash -c '
set -Eeuo pipefail
trap "shred -u /root/.app-ha-tunnel-secret 2>/dev/null || rm -f /root/.app-ha-tunnel-secret" EXIT
"$1/cloudflared_setup.sh" install "$2" "$3" </root/.app-ha-tunnel-secret
' bash "$REMOTE_LIB" "$CLOUDFLARE_ACCOUNT_ID" "$CLOUDFLARE_TUNNEL_ID" </dev/null
}

cleanup() {
  local code=$?
  [[ -z "$RUN_DIR" ]] || rm -rf -- "$RUN_DIR"
  exit "$code"
}

main() {
  parse_args "$@"
  load_config
  RUN_DIR="$(mktemp -d)"
  trap cleanup EXIT
  discover_cluster
  render_settings
  check_drift
  show_drift
  if [[ "$DRY_RUN" == true ]]; then
    log "Dry run complete; no host was changed"
    bmac_ui_step_done
    bmac_ui_result drift:bool "$DRIFT_FOUND"
    [[ "$DRIFT_FOUND" == false ]] ||
      bmac_ui_next_step "Sync the cluster with cluster.conf for real." \
        --workflow sync_cluster_network --arg dry_run=false
    return 0
  fi
  confirm_sync
  sync_slots
  local node
  for node in "${CLUSTER_NODES[@]}"; do
    sync_node "$node"
  done
  log "Bringing every host's Cloudflare Tunnel config up to date"
  mox_ssh "$COORDINATOR" "${REMOTE_LIB}/tunnel_sync.sh" --all </dev/null ||
    die "Could not update every host's tunnel config"
  check_drift
  show_drift
  [[ "$DRIFT_FOUND" == false ]] || die "Some hosts still differ from the configuration"
  log "Every host matches cluster.conf"
  bmac_ui_step_done
  bmac_ui_result synced_nodes:raw "$(bmac_ui_json_array "${CLUSTER_NODES[@]}")"
}

if [[ "${APP_HA_SYNC_NETWORK_SOURCE_ONLY:-0}" != 1 ]]; then
  bmac_ui_bootstrap "$@"
  main "$@"
fi
