#!/usr/bin/env bash

# Copyright (c) 2026 BEENTHERE VENTURES, INC.
# SPDX-License-Identifier: GPL-3.0-only

# Safely update the shared app-ha runtime (registry, deferred cleanup worker,
# QEMU lifecycle hook, guest networking, egress health, transfer counter,
# Cloudflare Tunnel tools, the monitor agent, and the production VM ISO
# builder) on every configured Proxmox node. With MONITORING_ENABLED=true it then brings monitoring to every node
# of the cluster: it synchronizes the cluster's Healthchecks.io checks and
# installs the monitor agent and its config on every host, the QDevice, and
# every active production VM (scripts/lib/monitoring.sh). With monitoring
# disabled it stops agents that an earlier run installed. Last, it upgrades
# cloudflared from Cloudflare's apt repository one host at a time, waiting for
# each upgraded tunnel replica to reconnect before the next host's restarts.

set -Eeuo pipefail
set +x
umask 077

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
REPO_ROOT="$(cd -- "${SCRIPT_DIR}/../../.." && pwd -P)"
# shellcheck source=../../lib/ui_protocol.sh
source "${REPO_ROOT}/scripts/lib/ui_protocol.sh"
CONFIG_LIB="${REPO_ROOT}/scripts/lib/config.sh"
MONITORING_LIB="${REPO_ROOT}/scripts/lib/monitoring.sh"
APT_LOCK_LIB="${REPO_ROOT}/scripts/lib/apt_lock_wait.sh"
RUNTIME_SOURCE_DIR="${REPO_ROOT}/scripts/host_runtime"
HOOK_NAME=app-ha-guest-role-hook.sh
# Every file this updater manages, relative to ${INSTALL_ROOT}. A file is
# staged under its basename, so basenames must be unique.
RUNTIME_FILES=(
  lib/config.sh lib/cluster_registry.py lib/process_deferred_cleanup.sh
  "lib/$HOOK_NAME" lib/guest_network.sh lib/egress_health.py
  lib/egress_probe_targets.txt lib/transfer_counter.py lib/tunnel_config.py
  lib/tunnel_sync.sh lib/cloudflared_setup.sh lib/app_ha_monitor.py
  lib/pbs_api.py lib/backup_link.sh lib/instant_switch.py
  guests/prod/build_ubuntu_autoinstall.py guests/prod/prepare_prod_iso.sh
)
INSTALL_ROOT="/usr/local/lib/app-ha-proxmox"
REMOTE_REGISTRY="${INSTALL_ROOT}/lib/cluster_registry.py"
REMOTE_TUNNEL_SYNC="${INSTALL_ROOT}/lib/tunnel_sync.sh"
REMOTE_SNIPPET=""

# config.sh comes from scripts/lib; every other runtime file from
# scripts/host_runtime.
runtime_source() {
  case "$1" in
    lib/config.sh) printf '%s\n' "$CONFIG_LIB" ;;
    *) printf '%s/%s\n' "$RUNTIME_SOURCE_DIR" "${1##*/}" ;;
  esac
}

DRY_RUN=false
ASSUME_YES=false
DELETE_OBSOLETE_CHECKS=false
SKIP_CLOUDFLARED=false
declare -a CLOUDFLARED_UPGRADES=()
COORDINATOR=""
REMOTE_STAGE=""
COMPLETED=false
ROLLBACK_RUNNING=false
declare -a CLUSTER_NODES=()
declare -a STAGED_NODES=()
declare -a COMMITTED_NODES=()

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
Usage: update_cluster_runtime.sh [--dry-run] [--yes] [--delete-obsolete-checks]
         [--skip-cloudflared]

Update these runtime files on every configured, online Proxmox node:
  /usr/local/lib/app-ha-proxmox/lib/config.sh
  /usr/local/lib/app-ha-proxmox/lib/cluster_registry.py
  /usr/local/lib/app-ha-proxmox/lib/process_deferred_cleanup.sh
  /usr/local/lib/app-ha-proxmox/lib/app-ha-guest-role-hook.sh
  /usr/local/lib/app-ha-proxmox/lib/guest_network.sh
  /usr/local/lib/app-ha-proxmox/lib/egress_health.py
  /usr/local/lib/app-ha-proxmox/lib/egress_probe_targets.txt
  /usr/local/lib/app-ha-proxmox/lib/transfer_counter.py
  /usr/local/lib/app-ha-proxmox/lib/tunnel_config.py
  /usr/local/lib/app-ha-proxmox/lib/tunnel_sync.sh
  /usr/local/lib/app-ha-proxmox/lib/cloudflared_setup.sh
  /usr/local/lib/app-ha-proxmox/lib/app_ha_monitor.py
  /usr/local/lib/app-ha-proxmox/lib/pbs_api.py
  /usr/local/lib/app-ha-proxmox/lib/backup_link.sh
  /usr/local/lib/app-ha-proxmox/lib/instant_switch.py
  /usr/local/lib/app-ha-proxmox/guests/prod/build_ubuntu_autoinstall.py
  /usr/local/lib/app-ha-proxmox/guests/prod/prepare_prod_iso.sh
  local:snippets/app-ha-guest-role-hook.sh

The script stages and validates the complete bundle on every node before
making changes. Installs are atomic, old files are retained until cluster-wide
verification and the tunnel config update succeed, and failures trigger
rollback. It does not change cluster.conf or any host's network settings; use
sync_cluster_network.sh for that.

Monitoring (docs/MONITORING_AND_ALERTING.md): with MONITORING_ENABLED=true
in cluster.conf, every host, the QDevice, and every active production VM must
be reachable over SSH, and HEALTHCHECKS_API_KEY in secrets.env must work,
before anything changes. After the hosts are updated, the cluster's
Healthchecks checks are created or updated, and the monitor agent, its
systemd unit, and its config are installed on every node, a phase at a time
(hosts, QDevice, production VMs), each phase rolling back on failure. Every
node's check must then receive a ping. With MONITORING_ENABLED=false, agents
installed earlier are stopped and disabled, and deleting the cluster's
checks is offered.

cloudflared: every host's cloudflared package is upgraded to the newest
version in Cloudflare's apt repository, one host at a time. Before a host's
tunnel replica restarts, another host's replica must be connected, so
ingress keeps working; the next host waits until the upgraded replica is
connected again. If an upgraded replica does not reconnect, the previous
version is reinstalled when the repository still has it, and the update stops
before any other host's cloudflared is touched.

Options:
  --dry-run  Validate local files and cluster reachability; show hash drift,
             the cloudflared upgrades, and the monitoring plan without
             changing any node or check (it does refresh the hosts' apt
             package lists).
  --yes      Skip the interactive GO confirmation.
  --delete-obsolete-checks
             Delete this cluster's Healthchecks checks that no node uses any
             more without asking (they are listed in the plan).
  --skip-cloudflared
             Leave every host's cloudflared version as it is.
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
      --delete-obsolete-checks)
        DELETE_OBSOLETE_CHECKS=true
        ;;
      --skip-cloudflared)
        SKIP_CLOUDFLARED=true
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

require_command() {
  command -v "$1" >/dev/null 2>&1 ||
    die "Required workstation command is unavailable: $1"
}

require_source() {
  [[ -f "$1" && ! -L "$1" ]] ||
    die "Required regular, non-symlink source is missing: $1"
}

load_config() {
  require_source "$CONFIG_LIB"
  # shellcheck disable=SC1090,SC1091 # Resolved from this script's path.
  source "$CONFIG_LIB"
  load_proxmox_config --no-secrets >/dev/null
  require_var MAX_MOX_HOSTS
  require_var CLUSTER_STATE_DIR
  require_var GUEST_ROLE_HOOK_PATH
  [[ "$GUEST_ROLE_HOOK_PATH" =~ ^[A-Za-z0-9._-]+:snippets/[A-Za-z0-9._-]+[.]sh$ ]] ||
    die "GUEST_ROLE_HOOK_PATH must be a safe Proxmox snippets volume"
  [[ "$(basename -- "$GUEST_ROLE_HOOK_PATH")" == "$HOOK_NAME" ]] ||
    die "This updater only manages $HOOK_NAME"
  REMOTE_SNIPPET="/var/lib/vz/snippets/$(basename -- "$GUEST_ROLE_HOOK_PATH")"
  require_source "$MONITORING_LIB"
  # shellcheck source=../../lib/monitoring.sh
  source "$MONITORING_LIB"
  require_source "$APT_LOCK_LIB"
  # shellcheck source=../../lib/apt_lock_wait.sh
  source "$APT_LOCK_LIB"
}

# Runs on a host as: bash -c SCRIPT bash ACTION [ATTEMPTS]
#   plan     refresh the package lists; print "INSTALLED CANDIDATE yes|no"
#   ready    exit 0 once the local replica has a connection to Cloudflare
#   upgrade  upgrade, restart, and wait for the replica; print
#            "upgraded OLD NEW", or exit 3 after reinstalling OLD, or 4
# The apt lock helpers are prepended by cloudflared_script.
# shellcheck disable=SC2016 # Evaluated by the remote Bash.
CLOUDFLARED_REMOTE='
set -Eeuo pipefail
action="$1"; attempts="${2:-45}"
installed() { dpkg-query -W -f="\${Version}" cloudflared 2>/dev/null || true; }
ready() {
  local index body
  for ((index = 0; index < attempts; index += 1)); do
    if body="$(curl -fsS --max-time 3 http://127.0.0.1:20241/ready 2>/dev/null)" &&
      python3 -c "import json, sys; sys.exit(0 if int(json.loads(sys.argv[1]).get(\"readyConnections\") or 0) > 0 else 1)" "$body"; then
      return 0
    fi
    sleep 2
  done
  return 1
}
# Lock-wait progress goes to stderr: stdout carries the result.
apt_install() {
  apt_wait_for_locks >&2
  DEBIAN_FRONTEND=noninteractive apt-get -o DPkg::Lock::Timeout=60 install -y -q "$@" >/dev/null
}
case "$action" in
  plan)
    apt_wait_for_locks >&2
    apt-get -o DPkg::Lock::Timeout=60 update -qq >/dev/null
    current="$(installed)"
    candidate="$(apt-cache policy cloudflared | awk "/Candidate:/ { print \$2 }")"
    needed=no
    if [[ -n "$current" && -n "$candidate" && "$candidate" != "(none)" ]] &&
      dpkg --compare-versions "$candidate" gt "$current"; then
      needed=yes
    fi
    printf "%s %s %s\n" "${current:-none}" "${candidate:-none}" "$needed"
    ;;
  ready) ready ;;
  upgrade)
    old="$(installed)"
    apt_install --only-upgrade cloudflared
    systemctl restart app-ha-cloudflared.service
    if ready; then
      printf "upgraded %s %s\n" "$old" "$(installed)"
      exit 0
    fi
    if apt_install --allow-downgrades "cloudflared=$old" 2>/dev/null; then
      systemctl restart app-ha-cloudflared.service
      ready && exit 3
    fi
    exit 4
    ;;
  *) exit 2 ;;
esac
'

cloudflared_script() {
  declare -f apt_lock_holder apt_wait_for_locks
  printf '%s\n' "$CLOUDFLARED_REMOTE"
}

cloudflared_exec() {
  local node="$1"
  shift
  mox_ssh "$node" bash -c "$(cloudflared_script)" bash "$@" </dev/null
}

plan_cloudflared() {
  CLOUDFLARED_UPGRADES=()
  if [[ "$SKIP_CLOUDFLARED" == true ]]; then
    info "cloudflared: left as it is (--skip-cloudflared)"
    return 0
  fi
  log "Checking every host's cloudflared version"
  local node output current candidate needed
  for node in "${CLUSTER_NODES[@]}"; do
    output="$(cloudflared_exec "$node" plan)" ||
      die "Could not read $node's cloudflared version from apt"
    read -r current candidate needed <<<"$output"
    if [[ "$current" == none ]]; then
      warn "$node has no cloudflared package; host setup installs it"
    elif [[ "$needed" == yes ]]; then
      info "$node: cloudflared $current -> $candidate"
      CLOUDFLARED_UPGRADES+=("$node")
    else
      info "$node: cloudflared $current is current"
    fi
  done
}

# Another host's replica must be connected before this host's restarts.
another_replica_ready() {
  local node="$1" other
  for other in "${CLUSTER_NODES[@]}"; do
    [[ "$other" != "$node" ]] || continue
    cloudflared_exec "$other" ready 3 >/dev/null 2>&1 && return 0
  done
  return 1
}

upgrade_cloudflared() {
  ((${#CLOUDFLARED_UPGRADES[@]})) || return 0
  log "Upgrading cloudflared one host at a time"
  local node output status
  for node in "${CLOUDFLARED_UPGRADES[@]}"; do
    if ((${#CLUSTER_NODES[@]} > 1)); then
      another_replica_ready "$node" ||
        die "No other host's tunnel replica is connected, so restarting $node's would interrupt ingress; cloudflared was not upgraded on: ${CLOUDFLARED_UPGRADES[*]}"
    else
      warn "$node is the only tunnel replica; ingress pauses for a few seconds while it restarts"
    fi
    status=0
    output="$(cloudflared_exec "$node" upgrade)" || status=$?
    case "$status" in
      0) info "$node: cloudflared ${output#upgraded }, replica connected again" ;;
      3) die "$node's upgraded tunnel replica did not reconnect, so its previous cloudflared was reinstalled and it is connected again. No other host was changed; check journalctl -u app-ha-cloudflared on $node." ;;
      *) die "$node's upgraded tunnel replica did not reconnect and its previous cloudflared could not be restored. Other hosts still serve ingress and were not changed; check journalctl -u app-ha-cloudflared on $node now." ;;
    esac
  done
}

validate_local_bundle() {
  local name source
  [[ -z "$(printf '%s\n' "${RUNTIME_FILES[@]##*/}" | sort | uniq -d)" ]] ||
    die "Managed runtime files must have unique basenames"
  for name in "${RUNTIME_FILES[@]}"; do
    source="$(runtime_source "$name")"
    require_source "$source"
    case "$name" in
      *.sh) bash -n "$source" ;;
      *.py) PYTHONDONTWRITEBYTECODE=1 python3 "$source" --help >/dev/null ;;
    esac
  done
}

discover_cluster() {
  COORDINATOR="$(first_reachable_mox)" ||
    die "No reachable Proxmox coordinator was found"
  local nodes_json nodes_text
  nodes_json="$(
    mox_ssh "$COORDINATOR" pvesh get /nodes --output-format json </dev/null
  )" || die "Could not query cluster membership through $COORDINATOR"
  nodes_text="$(
    python3 - "$nodes_json" "$MAX_MOX_HOSTS" <<'PY'
import json
import re
import sys

rows = json.loads(sys.argv[1])
maximum = int(sys.argv[2])
if not isinstance(rows, list) or not rows:
    raise SystemExit("cluster node inventory is empty or malformed")
nodes = []
for row in rows:
    if not isinstance(row, dict):
        raise SystemExit("cluster node inventory contains a malformed row")
    name = row.get("node")
    match = re.fullmatch(r"mox([1-9]|10)", str(name))
    if not match or int(match.group(1)) > maximum:
        raise SystemExit(f"unexpected cluster node: {name!r}")
    if row.get("status") != "online":
        raise SystemExit(f"cluster node is not online: {name}")
    nodes.append(name)
if len(nodes) != len(set(nodes)):
    raise SystemExit("cluster node inventory contains duplicate names")
for name in sorted(nodes, key=lambda value: int(value[3:])):
    print(name)
PY
  )" || die "Every configured cluster node must be valid and online"
  mapfile -t CLUSTER_NODES <<<"$nodes_text"
  ((${#CLUSTER_NODES[@]} > 0)) || die "No cluster nodes were discovered"

  local node
  for node in "${CLUSTER_NODES[@]}"; do
    mox_is_reachable "$node" ||
      die "$node is online in Proxmox but unreachable directly from this workstation"
  done
  info "Coordinator: $COORDINATOR"
  info "Online nodes: ${CLUSTER_NODES[*]}"
}

local_hash() {
  sha256sum "$1" | awk '{print $1}'
}

remote_hash() {
  local node="$1" path="$2"
  mox_ssh "$node" sha256sum "$path" |
    awk 'NF == 2 && $1 ~ /^[0-9a-f]{64}$/ {print $1}'
}

# NAME=HASH words for every managed file, as the staging check expects them.
expected_hashes() {
  local name
  for name in "${RUNTIME_FILES[@]}"; do
    printf '%s=%s\n' "${name##*/}" "$(local_hash "$(runtime_source "$name")")"
  done
}

show_hash_status() {
  local node name actual
  local -a outdated
  printf '\n%-8s %s\n' "Node" "Files to update"
  for node in "${CLUSTER_NODES[@]}"; do
    outdated=()
    for name in "${RUNTIME_FILES[@]}"; do
      actual="$(remote_hash "$node" "${INSTALL_ROOT}/${name}" 2>/dev/null || true)"
      [[ "$actual" == "$(local_hash "$(runtime_source "$name")")" ]] ||
        outdated+=("${name##*/}")
    done
    actual="$(remote_hash "$node" "$REMOTE_SNIPPET" 2>/dev/null || true)"
    [[ "$actual" == "$(local_hash "${RUNTIME_SOURCE_DIR}/${HOOK_NAME}")" ]] ||
      outdated+=("hook snippet")
    if ((${#outdated[@]})); then
      printf '%-8s %s\n' "$node" "${outdated[*]}"
    else
      printf '%-8s %s\n' "$node" "current"
    fi
  done
}

copy_to_node() {
  local node="$1" source="$2" destination="$3" ssh_destination
  _mox_ssh_options "$node"
  ssh_destination="$(_mox_ssh_destination "$node")"
  scp "${MOX_SSH_OPTIONS[@]}" -- "$source" \
    "${MOX_SSH_USER:-root}@${ssh_destination}:${destination}"
}

stage_node() {
  local node="$1" name
  local -a hashes
  mox_ssh "$node" install -d -m 0700 "$REMOTE_STAGE"
  STAGED_NODES+=("$node")
  for name in "${RUNTIME_FILES[@]}"; do
    copy_to_node "$node" "$(runtime_source "$name")" "${REMOTE_STAGE}/${name##*/}"
  done
  mapfile -t hashes < <(expected_hashes)
  # shellcheck disable=SC2016 # This script is evaluated by remote Bash.
  mox_ssh "$node" bash -c '
set -Eeuo pipefail
stage="$1"; shift
[[ -d "$stage" && ! -L "$stage" ]]
chmod 0700 "$stage"
for entry in "$@"; do
  name="${entry%%=*}"; expected="${entry#*=}"
  [[ "$name" =~ ^[A-Za-z0-9._-]+$ && -f "$stage/$name" && ! -L "$stage/$name" ]]
  [[ "$(sha256sum "$stage/$name" | awk "{print \$1}")" == "$expected" ]]
  case "$name" in
    *.txt) chmod 0644 "$stage/$name" ;;
    *.sh) chmod 0755 "$stage/$name"; bash -n "$stage/$name" ;;
    *.py) chmod 0755 "$stage/$name"
      PYTHONDONTWRITEBYTECODE=1 python3 "$stage/$name" --help >/dev/null ;;
  esac
done
' bash "$REMOTE_STAGE" "${hashes[@]}"
}

commit_node() {
  local node="$1"
  COMMITTED_NODES+=("$node")
  # shellcheck disable=SC2016 # This script is evaluated by remote Bash.
  if mox_ssh "$node" bash -c '
set -Eeuo pipefail
stage="$1"; install_root="$2"; snippet="$3"; hook_name="$4"; shift 4
[[ -f "$snippet" && ! -L "$snippet" ]]
# Keep every current file (or note that it was absent) for rollback.
for path in "$@"; do
  [[ "$path" =~ ^(lib|guests/prod)/[A-Za-z0-9._-]+$ ]]
  name="${path##*/}"; installed="${install_root}/${path}"
  if [[ -e "$installed" ]]; then
    [[ -f "$installed" && ! -L "$installed" ]]
    install -m "$(stat -c %a "$installed")" "$installed" "$stage/${name}.old"
  else
    : >"$stage/${name}.absent"
  fi
done
install -m 0755 "$snippet" "$stage/${hook_name}.snippet.old"
: >"$stage/committed"
for path in "$@"; do
  name="${path##*/}"; installed="${install_root}/${path}"
  install -d -m 0755 "${installed%/*}"
  install -m "$(stat -c %a "$stage/$name")" "$stage/$name" "${installed}.new"
  mv -f "${installed}.new" "$installed"
done
install -m 0755 "$stage/$hook_name" "${snippet}.new"
mv -f "${snippet}.new" "$snippet"
# The lifecycle hook and the tunnel sync load the installed cluster.conf
# through the installed config.sh; a parser that rejects it fails this
# commit, which is then rolled back.
"${install_root}/lib/config.sh" --check --no-secrets >/dev/null
# The egress health service and the monitor agent run from the installed
# files. The monitor unit exists only once monitoring was installed.
systemctl try-restart app-ha-egress-health.service
if [[ -f /etc/systemd/system/app-ha-monitor.service ]]; then
  systemctl try-restart app-ha-monitor.service
fi
' bash "$REMOTE_STAGE" "$INSTALL_ROOT" "$REMOTE_SNIPPET" "$HOOK_NAME" \
    "${RUNTIME_FILES[@]}"; then
    return 0
  fi
  if mox_ssh "$node" test ! -f "${REMOTE_STAGE}/committed" \
    >/dev/null 2>&1; then
    unset "COMMITTED_NODES[$((${#COMMITTED_NODES[@]} - 1))]"
  fi
  return 1
}

rollback_node() {
  local node="$1"
  # shellcheck disable=SC2016 # This script is evaluated by remote Bash.
  mox_ssh "$node" bash -c '
set -Eeuo pipefail
stage="$1"; install_root="$2"; snippet="$3"; hook_name="$4"; shift 4
[[ -f "$stage/committed" ]]
for path in "$@"; do
  [[ "$path" =~ ^(lib|guests/prod)/[A-Za-z0-9._-]+$ ]]
  name="${path##*/}"; installed="${install_root}/${path}"
  if [[ -f "$stage/${name}.absent" ]]; then
    rm -f "$installed"
  else
    install -m "$(stat -c %a "$stage/${name}.old")" "$stage/${name}.old" "${installed}.rollback"
    mv -f "${installed}.rollback" "$installed"
  fi
done
install -m 0755 "$stage/${hook_name}.snippet.old" "${snippet}.rollback"
mv -f "${snippet}.rollback" "$snippet"
systemctl try-restart app-ha-egress-health.service
if [[ -f /etc/systemd/system/app-ha-monitor.service ]]; then
  systemctl try-restart app-ha-monitor.service
fi
rm -f "$stage/committed"
' bash "$REMOTE_STAGE" "$INSTALL_ROOT" "$REMOTE_SNIPPET" "$HOOK_NAME" \
    "${RUNTIME_FILES[@]}"
}

verify_node() {
  local node="$1" name
  for name in "${RUNTIME_FILES[@]}"; do
    [[ "$(remote_hash "$node" "${INSTALL_ROOT}/${name}")" == \
      "$(local_hash "$(runtime_source "$name")")" ]] ||
      die "$node: ${name} hash differs after installation"
  done
  [[ "$(remote_hash "$node" "$REMOTE_SNIPPET")" == \
    "$(local_hash "${RUNTIME_SOURCE_DIR}/${HOOK_NAME}")" ]] ||
    die "$node hook snippet hash differs after installation"
  mox_ssh "$node" bash -n "$REMOTE_SNIPPET"
  mox_ssh "$node" "$REMOTE_REGISTRY" \
    --state-dir "$CLUSTER_STATE_DIR" list --record-type resources >/dev/null
  mox_ssh "$node" "$REMOTE_TUNNEL_SYNC" --check >/dev/null ||
    die "$node's tunnel config differs from the registry after the update"
}

cleanup_remote_stages() {
  local node
  for node in "${STAGED_NODES[@]}"; do
    mox_ssh "$node" rm -rf -- "$REMOTE_STAGE" >/dev/null 2>&1 || true
  done
}

rollback_committed_nodes() {
  [[ "$ROLLBACK_RUNNING" == false ]] || return 0
  ROLLBACK_RUNNING=true
  local index node failed=false
  warn "Runtime update failed; restoring prior files on committed nodes"
  for ((index = ${#COMMITTED_NODES[@]} - 1; index >= 0; index -= 1)); do
    node="${COMMITTED_NODES[index]}"
    if rollback_node "$node"; then
      info "Restored prior runtime on $node"
    else
      warn "Could not restore prior runtime on $node; manual recovery is required"
      failed=true
    fi
  done
  if [[ "$failed" == false ]] &&
    ! mox_ssh "$COORDINATOR" "$REMOTE_TUNNEL_SYNC" --all </dev/null; then
    warn "Prior files were restored, but the tunnel config could not be resynchronized"
    failed=true
  fi
  [[ "$failed" == false ]]
}

cleanup() {
  local code=$?
  trap - EXIT HUP INT TERM
  set +e
  set +x
  if ((code != 0)) && [[ "$COMPLETED" == false ]] &&
    ((${#COMMITTED_NODES[@]} > 0)); then
    rollback_committed_nodes || true
  fi
  cleanup_remote_stages
  exit "$code"
}

on_signal() {
  local signal="$1" code=1
  case "$signal" in
    HUP) code=129 ;;
    INT) code=130 ;;
    TERM) code=143 ;;
  esac
  warn "Received $signal during the cluster runtime update"
  exit "$code"
}

# Monitoring preflight and plan, before anything changes: every monitored
# node must be reachable and the API key must work. Leaves the plan in
# MONITORING_PLAN_OUT and decides whether obsolete checks are deleted.
plan_monitoring() {
  if ! monitoring_enabled; then
    [[ "$DRY_RUN" == false ]] || monitoring_disable_everywhere "$COORDINATOR" --dry-run ||
      die "Could not check for monitor agents installed earlier"
    return 0
  fi
  log "Planning monitoring (MONITORING_ENABLED=true)"
  monitoring_require_qdevice || die "The QDevice must be reachable while monitoring is enabled"
  MONITORING_PLAN_OUT="$(mktemp "${TMPDIR:-/tmp}/bmac-monitoring-plan.XXXXXX")"
  export MONITORING_PLAN_OUT
  monitoring_sync "$COORDINATOR" --dry-run ||
    die "The monitoring preflight failed; no node or check was changed"
  local obsolete
  obsolete="$(python3 -c 'import json, sys; print(", ".join(json.load(open(sys.argv[1]))["obsolete"]))' \
    "$MONITORING_PLAN_OUT")"
  rm -f -- "$MONITORING_PLAN_OUT"
  unset MONITORING_PLAN_OUT
  [[ -n "$obsolete" && "$DRY_RUN" == false ]] || return 0
  info "Checks no node uses any more: $obsolete"
  if [[ "$DELETE_OBSOLETE_CHECKS" == true ]]; then
    MONITORING_DELETE_OBSOLETE=true
  elif [[ "$ASSUME_YES" == true ]]; then
    info "Keeping them (rerun with --delete-obsolete-checks to delete them)"
  elif bmac_ui_is_json; then
    if bmac_ui_confirm --id obsolete_checks --question --severity warning \
      --title "Delete obsolete Healthchecks checks?" \
      --message "These checks belong to this cluster but to no current node: ${obsolete}. Deleting them stops their alerts." \
      --confirm-label "Delete them" --cancel-label "Keep them"; then
      MONITORING_DELETE_OBSOLETE=true
    fi
  else
    local answer
    read -r -p "Delete these obsolete checks? [y/N] " answer
    [[ ! "$answer" =~ ^[Yy]([Ee][Ss])?$ ]] || MONITORING_DELETE_OBSOLETE=true
  fi
}

update_monitoring() {
  if ! monitoring_enabled; then
    local -a options=()
    [[ "$ASSUME_YES" == false ]] || options+=(--yes)
    monitoring_disable_everywhere "$COORDINATOR" "${options[@]}" ||
      die "Could not stop the monitor agents installed earlier"
    return 0
  fi
  log "Updating monitoring on every host, the QDevice, and every production VM"
  local -a options=()
  [[ "${MONITORING_DELETE_OBSOLETE:-false}" == false ]] || options+=(--delete-obsolete)
  monitoring_sync "$COORDINATOR" "${options[@]}" ||
    die "The runtime is updated on every host, but monitoring is not complete (see above). Healthchecks checks created by this run are kept and reused. Fix the problem and rerun update_cluster_runtime.sh."
}

confirm_update() {
  [[ "$ASSUME_YES" == true ]] && return 0
  local entered
  printf '\nThis will atomically replace the app-ha runtime on: %s\n' \
    "${CLUSTER_NODES[*]}"
  ! monitoring_enabled ||
    printf 'Then it installs or updates the monitor agent on every host, the QDevice, and every active production VM.\n'
  ((${#CLOUDFLARED_UPGRADES[@]} == 0)) ||
    printf 'Last, it upgrades cloudflared one host at a time on: %s\n' "${CLOUDFLARED_UPGRADES[*]}"
  if bmac_ui_is_json; then
    bmac_ui_confirm --id go --severity warning --title "Update the cluster runtime?" \
      --message "This atomically replaces the app-ha runtime on: ${CLUSTER_NODES[*]}$(monitoring_enabled && printf '%s' ', then updates monitoring on every node')$( ((${#CLOUDFLARED_UPGRADES[@]} == 0)) || printf ', then upgrades cloudflared one host at a time on: %s' "${CLOUDFLARED_UPGRADES[*]}"). A failure rolls back the nodes already committed." \
      --confirm-label "Update the runtime" --text GO ||
      die "Confirmation did not match GO; no changes were made"
    return 0
  fi
  printf 'Type GO to continue.\n> '
  IFS= read -r entered
  [[ "$entered" == GO ]] || die "Confirmation did not match GO; no changes were made"
}

main() {
  parse_args "$@"
  local command_name
  for command_name in awk bash basename install python3 rm scp sha256sum ssh stat; do
    require_command "$command_name"
  done
  load_config
  validate_local_bundle
  discover_cluster
  show_hash_status
  plan_cloudflared
  plan_monitoring
  if [[ "$DRY_RUN" == true ]]; then
    log "Dry run complete; no cluster files were changed"
    bmac_ui_step_done
    bmac_ui_next_step "Review the hash drift above, then update the runtime for real." \
      --command "scripts/user_callable/hosts/update_cluster_runtime.sh" --workflow update_cluster_runtime --arg dry_run=false
    return 0
  fi

  confirm_update
  REMOTE_STAGE="/run/app-ha-runtime-update-$$"
  [[ "$REMOTE_STAGE" =~ ^/run/app-ha-runtime-update-[1-9][0-9]*$ ]] ||
    die "Derived remote staging path is unsafe"
  trap cleanup EXIT
  trap 'on_signal HUP' HUP
  trap 'on_signal INT' INT
  trap 'on_signal TERM' TERM

  log "Staging and validating the runtime bundle on every node"
  local node
  for node in "${CLUSTER_NODES[@]}"; do
    stage_node "$node"
    info "Validated staged bundle on $node"
  done

  log "Committing the runtime bundle"
  for node in "${CLUSTER_NODES[@]}"; do
    commit_node "$node"
    info "Installed runtime on $node"
  done

  log "Bringing every host's Cloudflare Tunnel config up to date"
  mox_ssh "$COORDINATOR" "$REMOTE_TUNNEL_SYNC" --all </dev/null

  log "Verifying the installed runtime on every node"
  for node in "${CLUSTER_NODES[@]}"; do
    verify_node "$node"
    info "Verified $node"
  done

  COMPLETED=true
  cleanup_remote_stages
  STAGED_NODES=()
  update_monitoring
  upgrade_cloudflared

  log "Cluster runtime update complete"
  info "Updated nodes: ${CLUSTER_NODES[*]}"
  info "No VM restart or hook reattachment is required"
  if monitoring_enabled; then
    info "Monitoring: every node's agent is installed and its check receives pings"
  fi
  bmac_ui_step_done
  ((${#CLOUDFLARED_UPGRADES[@]} == 0)) || info "Upgraded cloudflared on: ${CLOUDFLARED_UPGRADES[*]}"
  bmac_ui_result updated_nodes:raw "$(bmac_ui_json_array "${CLUSTER_NODES[@]}")" \
    cloudflared_upgraded:raw "$(bmac_ui_json_array "${CLOUDFLARED_UPGRADES[@]}")" \
    monitoring "$(monitoring_enabled && echo enabled || echo disabled)"
  if monitoring_enabled; then
    bmac_ui_next_step "See every node's check and open problems." \
      --command "scripts/user_callable/diagnostics/show_monitoring_state.sh" --workflow show_monitoring_state
    bmac_ui_next_step "Send a test alert to check the email, filter, and Pushover chain." \
      --command "scripts/user_callable/diagnostics/send_test_alert.sh" --workflow send_test_alert
  fi
}

if [[ "${APP_HA_RUNTIME_UPDATE_SOURCE_ONLY:-0}" != 1 ]]; then
  bmac_ui_bootstrap "$@"
  main "$@"
fi
