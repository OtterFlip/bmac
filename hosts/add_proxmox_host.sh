#!/usr/bin/env bash

# Copyright (c) 2026 BEENTHERE VENTURES, INC.
# SPDX-License-Identifier: GPL-3.0-only

set -Eeuo pipefail
set +x
umask 077

# Destructive, resumable bare-metal setup for one app-ha Proxmox host.
# All local state, logs, generated media, SSH keys, GPT backups, and LUKS
# headers stay below ./artifacts. Re-run the script to resume after a reboot.

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd -- "${SCRIPT_DIR}/.." && pwd)"
# shellcheck source=../lib/ui_protocol.sh
source "${REPO_ROOT}/lib/ui_protocol.sh"
CONFIG_LIB="${SCRIPT_DIR}/../lib/config.sh"
CLUSTER_REGISTRY_SOURCE="${SCRIPT_DIR}/../lib/cluster_registry.py"
HAPROXY_RENDERER_SOURCE="${SCRIPT_DIR}/../lib/haproxy_routes.py"
HAPROXY_SYNC_SOURCE="${SCRIPT_DIR}/../lib/sync_haproxy_routes.sh"
DEFERRED_CLEANUP_SOURCE="${SCRIPT_DIR}/../lib/process_deferred_cleanup.sh"
RPOOL_MIRROR_SOURCE="${SCRIPT_DIR}/../lib/rpool_mirror.sh"
RPOOL_MIRROR_TOOL=/usr/local/sbin/app-ha-rpool-mirror
PROD_ISO_BUILDER_SOURCE="${SCRIPT_DIR}/../guests/prod/build_ubuntu_autoinstall.py"
PROD_ISO_PREPARER_SOURCE="${SCRIPT_DIR}/../guests/prod/prepare_prod_iso.sh"
CANONICAL_GUEST_ROLE_HOOK_SOURCE="${SCRIPT_DIR}/app-ha-guest-role-hook.sh"
CLUSTER_CONFIG_SOURCE="${SCRIPT_DIR}/../env/cluster.conf"
ARTIFACTS_DIR="${SCRIPT_DIR}/artifacts"

[[ -f "$CONFIG_LIB" ]] || {
  printf 'ERROR: Missing Proxmox configuration library: %s\n' "$CONFIG_LIB" >&2
  exit 1
}
# shellcheck source=../lib/config.sh
source "$CONFIG_LIB"
CONTROL_LIB="${SCRIPT_DIR}/../lib/cluster_control.sh"
[[ -f "$CONTROL_LIB" ]] || {
  printf 'ERROR: Missing cluster control-node library: %s\n' "$CONTROL_LIB" >&2
  exit 1
}
# shellcheck source=../lib/cluster_control.sh
source "$CONTROL_LIB"
QDEVICE_LIB="${SCRIPT_DIR}/../lib/qdevice.sh"
[[ -f "$QDEVICE_LIB" ]] || {
  printf 'ERROR: Missing QDevice library: %s\n' "$QDEVICE_LIB" >&2
  exit 1
}
# shellcheck source=../lib/qdevice.sh
source "$QDEVICE_LIB"

SELECTED_HOST=""
HOST_ID=""
HOST_ARTIFACTS=""
LOG_DIR=""
STATE_DIR=""
GENERATED_DIR=""
SSH_DIR=""
HEADER_DIR=""
SSH_KEY=""
HOST_KEY=""
HOST_KEY_PUB=""
KNOWN_HOSTS=""
LUKS_SECRET_FILE="/root/.app-ha-luks-passphrase"
SETUP_ROLE=""
EXISTING_NODE=""
GUEST_ROLE_HOOK_SOURCE=""
GUEST_ROLE_HOOK_NAME=""
GUEST_ROLE_HOOK_COMPAT_NAME=""
TAILSCALE_IP=""
WORKSTATION_SSH_FAILURE=""
QDEVICE_REMOVED_FOR_JOIN=0
USE_ADMIN_SSH=0
CLUSTER_CONTROL_LOCK_HELD=0
REMOTE_CLUSTER_LOCK_READ_FD=""
REMOTE_CLUSTER_LOCK_WRITE_FD=""
REMOTE_CLUSTER_LOCK_PID=""
REMOTE_CLUSTER_LOCK_TOKEN=""
ENCRYPTION_POLICY=""
REQUESTED_ENCRYPTION_POLICY=""
BOOT_TEST_POLICY=""
REQUESTED_BOOT_TEST_POLICY=""
HARDWARE_INVENTORY_MODE=""
HOST_SETUP_CONFIG_SHA256=""
RPOOL_LAYOUT_SHA256=""
declare -a CONFIGURED_MIRROR_PAIRS=()
declare -a CONFIGURED_NVME_SERIALS=()
declare -a EXPECTED_RPOOL_MAPPERS=()

timestamp() { date -u +'%Y-%m-%dT%H:%M:%SZ'; }
log() {
  printf '\n[%s] %s\n' "$(timestamp)" "$*"
  bmac_ui_step "$*"
}
info() { printf '    %s\n' "$*"; }
fail() {
  printf '\nERROR: %s\n' "$*" >&2
  bmac_ui_error failed "$*"
  exit 1
}

on_error() {
  local code=$?
  printf '\nERROR: host setup failed near line %s (exit %s).\n' "${BASH_LINENO[0]}" "$code" >&2
  printf 'Fix the reported condition and re-run this script; completed phases are recorded under %s.\n' \
    "${STATE_DIR:-${HOST_ARTIFACTS:-$ARTIFACTS_DIR}/state}" >&2
  bmac_ui_error failed "Host setup failed near line ${BASH_LINENO[0]} (exit $code). Fix the reported condition and rerun; completed phases are kept."
  exit "$code"
}
trap on_error ERR

release_remote_cluster_control_lock() {
  [[ -n "$REMOTE_CLUSTER_LOCK_PID" ]] || return 0
  if [[ -n "$REMOTE_CLUSTER_LOCK_WRITE_FD" ]]; then
    printf 'RELEASE:%s\n' "$REMOTE_CLUSTER_LOCK_TOKEN" \
      1>&"$REMOTE_CLUSTER_LOCK_WRITE_FD" || true
    exec {REMOTE_CLUSTER_LOCK_WRITE_FD}>&-
  fi
  if [[ -n "$REMOTE_CLUSTER_LOCK_READ_FD" ]]; then
    exec {REMOTE_CLUSTER_LOCK_READ_FD}<&-
  fi
  wait "$REMOTE_CLUSTER_LOCK_PID" 2>/dev/null || true
  REMOTE_CLUSTER_LOCK_READ_FD=""
  REMOTE_CLUSTER_LOCK_WRITE_FD=""
  REMOTE_CLUSTER_LOCK_PID=""
  REMOTE_CLUSTER_LOCK_TOKEN=""
}
trap release_remote_cluster_control_lock EXIT

require_command() {
  local command_name
  for command_name in "$@"; do
    command -v "$command_name" >/dev/null 2>&1 || fail "Required command is unavailable: $command_name"
  done
}

require_var() {
  local name=$1
  [[ -n "${!name:-}" ]] || fail "Required configuration variable $name is unset or empty"
}

prompt_yes() {
  local prompt=$1 answer
  if bmac_ui_is_json; then
    bmac_ui_ask "$1"
    return
  fi
  read -r -p "$prompt [y/N] " answer
  [[ "${answer,,}" == "y" || "${answer,,}" == "yes" ]]
}

prompt_yes_default_yes() {
  local answer
  if bmac_ui_is_json; then
    bmac_ui_ask "$1"
    return
  fi
  read -r -p "$1 [Y/n] " answer || answer=""
  [[ -z "$answer" || "${answer,,}" == y || "${answer,,}" == yes ]]
}

choose_hardware_inventory_mode() {
  printf '\nDISK INVENTORY METHOD\n'
  printf 'iDRAC mode queries Redfish to verify configured disk serials, capacities, and health.\n'
  printf 'Manual mode uses serials and exact byte capacities gathered beforehand by copying hosts/cluster_setup_prereq.sh to a Linux Live environment booted on the host and running it there.\n'
  if bmac_ui_is_json; then
    bmac_ui_choose HARDWARE_INVENTORY_MODE "Disk inventory method" idrac \
      idrac "iDRAC/Redfish: verify disk serials, capacities, and health" \
      manual "Manual: serials and byte capacities from hosts/cluster_setup_prereq.sh"
  elif prompt_yes "Use iDRAC/Redfish for disk inventory on this run?"; then
    HARDWARE_INVENTORY_MODE=idrac
  else
    HARDWARE_INVENTORY_MODE=manual
  fi
}

require_yes() {
  prompt_yes "$1" || fail "Prerequisite was not confirmed."
}

confirm_exact() {
  local description=$1 _legacy_expected=${2:-} actual
  if bmac_ui_is_json; then
    bmac_ui_confirm_go "$1" || fail "Confirmation did not match GO; no action was taken."
    return 0
  fi
  printf '\n!!!!!!!!!!!!!!!! DESTRUCTIVE / DISRUPTIVE ACTION !!!!!!!!!!!!!!!!\n' >&2
  printf '%s\n\nType GO to continue.\n> ' "$description" >&2
  read -r actual
  [[ "$actual" == GO ]] || fail "Confirmation did not match GO; no action was taken."
}

wait_for_exact() {
  local prompt=$1 _legacy_expected=${2:-} _legacy_case_insensitive=${3:-false} actual
  if bmac_ui_is_json; then
    bmac_ui_manual_action --id wait --title "$prompt" \
      --instruction "Complete the steps in the script output above, then continue." \
      --ack-label Done
    return
  fi
  while true; do
    printf '%s Type GO to continue: ' "$prompt" >&2
    read -r actual ||
      fail "Input ended while waiting for GO"
    [[ "$actual" == GO ]] && return
    printf 'Please type GO exactly.\n' >&2
  done
}

wait_for_helper_success() {
  local helper=$1 purpose=$2
  printf '\n========== TARGET-HOST HELPER SCRIPT STATUS ==========\n'
  printf 'Target host: %s\n' "$HOST_ID"
  printf 'Helper script: %s\n' "$helper"
  printf 'Expected result: %s\n' "$purpose"
  printf 'Do not continue until you have run this helper on the target host and it has completed successfully.\n'
  if bmac_ui_is_json; then
    bmac_ui_manual_action --id helper --title "Run $helper on $HOST_ID" \
      --instruction "At the console of $HOST_ID, run $helper as shown in the script output above." \
      --instruction "Expected result: $purpose" \
      --instruction "Continue only after the helper has completed successfully." \
      --ack-label "The helper succeeded"
    return
  fi
  wait_for_exact "SCRIPT WORKED?" "GO"
}

usage() {
  cat <<EOF
Usage: $0 [--host moxN] [--encrypt | --no-encrypt] [--skip-boot-tests | --run-boot-tests]

  --host moxN         Configure one host, mox1 through mox10. If omitted,
                      show every slot's status and prompt, recommending
                      the lowest free slot.
  --encrypt          Convert every configured rpool member to LUKS2.
  --no-encrypt       Keep every configured rpool member unencrypted.
  --skip-boot-tests  Skip the optional mirror-member and final boot tests.
  --run-boot-tests   Test each mirror member, then test the healthy final state.

The selected encryption and test policies are recorded and reused on resume.
EOF
}

host_artifacts_present() {
  local -a existing=()
  [[ -d "${ARTIFACTS_DIR}/$1" ]] || return 1
  shopt -s nullglob dotglob
  existing=("${ARTIFACTS_DIR}/$1"/*)
  shopt -u nullglob dotglob
  ((${#existing[@]} > 0))
}

# Show every slot's status and prompt for the host. The recommendation is the
# lowest slot that is not a cluster member, has no registry slot record, and
# has no local setup artifacts; before any cluster exists it is the control
# node, which creates the cluster.
choose_host_slot() {
  load_proxmox_config --no-secrets >/dev/null ||
    fail "Could not load env/cluster.conf to list host slots"
  local status=0 slots_json="" index node state config recommended=""
  local -A slot_state=()
  info "Looking for a reachable cluster member..."
  control_find_cluster || status=$?
  ((status != 2)) ||
    fail "Cluster membership reported through $CONTROL_PROBE_NODE is malformed"
  if ((status == 0)) && control_read_recorded &&
    [[ "$CONTROL_REGISTRY_SUPPORTS_HOSTS" == true ]]; then
    slots_json="$(control_registry "$CONTROL_PROBE_NODE" host-list 2>/dev/null)" ||
      slots_json=""
  fi
  if [[ -n "$slots_json" ]]; then
    while read -r node state; do
      [[ -n "$node" ]] && slot_state["$node"]="$state"
    done < <(
      python3 -c '
import json, sys
for row in json.loads(sys.argv[1]):
    print(row["node"], row["state"])
' "$slots_json"
    )
  fi

  printf '\nProxmox host slots:\n'
  for ((index = 1; index <= MAX_MOX_HOSTS; index += 1)); do
    node="mox${index}"
    state=""
    if control_list_contains "$node" "${CONTROL_MEMBER_NODES[@]}"; then
      if control_list_contains "$node" "${CONTROL_ONLINE_NODES[@]}"; then
        state="cluster member"
      else
        state="cluster member (OFFLINE)"
      fi
    elif [[ "${slot_state[$node]:-}" == joining ]]; then
      state="reserved for a joining host"
    elif [[ "${slot_state[$node]:-}" == member ]]; then
      state="recorded member missing from the cluster"
    fi
    if host_artifacts_present "$node"; then
      state="${state:+${state}; }setup artifacts present"
    fi
    if [[ -z "$state" ]]; then
      state="free"
      [[ -n "$recommended" ]] || recommended="$node"
    fi
    config="no env/${node}.conf"
    [[ ! -f "${SCRIPT_DIR}/../env/${node}.conf" ]] || config="env/${node}.conf"
    printf '  %-6s %-44s %s\n' "$node" "$state" "$config"
  done
  if ((status != 0)); then
    recommended="${PROXMOX_CONTROL_NODE:-mox1}"
    printf '\nNo reachable cluster member was found. The first host must be the control node, which creates the cluster.\n'
  fi
  [[ -n "$recommended" ]] ||
    fail "Every host slot through mox${MAX_MOX_HOSTS} is in use"
  printf '\nRecommended host slot: %s\n' "$recommended"
  [[ -f "${SCRIPT_DIR}/../env/${recommended}.conf" ]] ||
    printf 'Create env/%s.conf with that host'"'"'s values before continuing with it.\n' \
      "$recommended"
  printf 'To resume an interrupted setup, enter that host instead.\n'
  if bmac_ui_is_json; then
    local -a options=()
    for ((index = 1; index <= MAX_MOX_HOSTS; index += 1)); do
      node="mox${index}"
      [[ -f "${SCRIPT_DIR}/../env/${node}.conf" ]] || continue
      state="free"
      if control_list_contains "$node" "${CONTROL_MEMBER_NODES[@]}"; then
        state="cluster member"
      elif [[ -n "${slot_state[$node]:-}" ]]; then
        state="${slot_state[$node]}"
      fi
      ! host_artifacts_present "$node" || state+="; resume setup"
      options+=("$node" "$node · $state")
    done
    ((${#options[@]} > 0)) || fail "Create env/${recommended}.conf with that host's values first"
    bmac_ui_choose SELECTED_HOST "Host to configure (recommended: $recommended)" \
      "$([[ -f "${SCRIPT_DIR}/../env/${recommended}.conf" ]] && printf '%s' "$recommended")" \
      "${options[@]}"
  else
    read -r -p "Host to configure [${recommended}]: " SELECTED_HOST
  fi
  SELECTED_HOST="${SELECTED_HOST:-$recommended}"
}

parse_args() {
  while (($#)); do
    case "$1" in
      --host)
        (($# >= 2)) || fail "--host requires a value from mox1 through mox10"
        [[ -z "$SELECTED_HOST" ]] || fail "--host may only be specified once"
        SELECTED_HOST="$2"
        shift 2
        continue
        ;;
      --encrypt)
        [[ -z "$REQUESTED_ENCRYPTION_POLICY" || "$REQUESTED_ENCRYPTION_POLICY" == luks ]] ||
          fail "--encrypt and --no-encrypt are mutually exclusive"
        REQUESTED_ENCRYPTION_POLICY=luks
        ;;
      --no-encrypt)
        [[ -z "$REQUESTED_ENCRYPTION_POLICY" || "$REQUESTED_ENCRYPTION_POLICY" == clear ]] ||
          fail "--encrypt and --no-encrypt are mutually exclusive"
        REQUESTED_ENCRYPTION_POLICY=clear
        ;;
      --skip-boot-tests)
        [[ -z "$REQUESTED_BOOT_TEST_POLICY" || "$REQUESTED_BOOT_TEST_POLICY" == skip ]] ||
          fail "--skip-boot-tests and --run-boot-tests are mutually exclusive"
        REQUESTED_BOOT_TEST_POLICY=skip
        ;;
      --run-boot-tests)
        [[ -z "$REQUESTED_BOOT_TEST_POLICY" || "$REQUESTED_BOOT_TEST_POLICY" == run ]] ||
          fail "--skip-boot-tests and --run-boot-tests are mutually exclusive"
        REQUESTED_BOOT_TEST_POLICY=run
        ;;
      -h|--help) usage; exit 0 ;;
      *) fail "Unknown argument: $1" ;;
    esac
    shift
  done
  if [[ -z "$SELECTED_HOST" ]]; then
    choose_host_slot
  fi
  mox_index "$SELECTED_HOST" >/dev/null ||
    fail "--host must be mox1 through mox10"
  [[ -f "${SCRIPT_DIR}/../env/${SELECTED_HOST}.conf" ]] ||
    fail "Missing env/${SELECTED_HOST}.conf"
}

reset_or_resume_existing_state() {
  HOST_ID="$SELECTED_HOST"
  HOST_ARTIFACTS="${ARTIFACTS_DIR}/${HOST_ID}"
  [[ -d "$HOST_ARTIFACTS" ]] || return 0
  local -a existing=()
  shopt -s nullglob dotglob
  existing=("$HOST_ARTIFACTS"/*)
  shopt -u nullglob dotglob
  ((${#existing[@]} > 0)) || return 0

  printf '\nEXISTING SETUP ARTIFACTS FOUND\n'
  info "Host: $HOST_ID"
  info "Artifacts: $HOST_ARTIFACTS"
  printf 'Resuming preserves completed phases, generated media, setup SSH material, logs, and any LUKS recovery headers.\n'
  local restart=no
  if bmac_ui_is_json; then
    bmac_ui_choose restart "Existing setup artifacts for $HOST_ID" no \
      no "Resume the recorded setup" \
      yes "Delete every host artifact and start from the beginning"
  elif prompt_yes "Delete every host artifact (including the custom ISO and Tailscale setup state) and start this destructive installer from the beginning? Answer no to resume"; then
    restart=yes
  fi
  if [[ "$restart" == yes ]]; then
    confirm_exact \
      "This permanently deletes the complete local artifact tree for $HOST_ID. If this host was already installed, continuing afterward can reinstall it and erase its configured mirror disks. The global record of exposed Tailscale auth-key digests is intentionally retained so a used key cannot be reused." \
      "DELETE ${HOST_ID} SETUP ARTIFACTS"
    rm -rf -- "$HOST_ARTIFACTS"
    printf 'Deleted %s; this run will start with no per-host setup state.\n' "$HOST_ARTIFACTS"
  else
    printf 'Resuming the recorded setup for %s.\n' "$HOST_ID"
  fi
}

state_path() { printf '%s/%s\n' "$STATE_DIR" "$1"; }
has_state() { [[ -s "$(state_path "$1")" ]]; }
write_state() {
  local name=$1 value=${2:-"$(timestamp)"}
  [[ "$name" =~ ^[A-Za-z0-9._-]+$ ]] || fail "Unsafe state name: $name"
  printf '%s\n' "$value" >"$(state_path "$name")"
  chmod 0600 "$(state_path "$name")"
}
read_state() {
  local value
  [[ -s "$(state_path "$1")" ]] || fail "Missing setup state: $1"
  IFS= read -r value <"$(state_path "$1")"
  printf '%s\n' "$value"
}

shred_secret_file() {
  local path=$1
  [[ -e "$path" || -L "$path" ]] || return 0
  [[ -f "$path" && ! -L "$path" ]] ||
    fail "Refusing to shred $path because it is not a regular file"
  shred -n 1 -u -- "$path"
}

installation_media_is_current() {
  has_state installed && return 0
  has_state iso-built &&
    has_state iso-config-sha256 &&
    [[ "$(read_state iso-config-sha256)" == "$HOST_SETUP_CONFIG_SHA256" ]] &&
    has_state prepared-iso &&
    [[ -f "$(read_state prepared-iso)" ]] &&
    [[ -s "$HOST_KEY_PUB" ]]
}

source_iso_is_required() {
  installation_media_is_current && return 1
  return 0
}

load_configuration() {
  load_proxmox_config --host "$SELECTED_HOST" --require-secrets ||
    fail "Could not load cluster.conf, ${SELECTED_HOST}.conf, and secrets.env"
  local required=(
    NVME_MIRROR_0_SERIAL_1 NVME_MIRROR_0_SERIAL_2
    PROXMOX_IP PROXMOX_GATEWAY PROXMOX_PREFIX PROXMOX_FQDN
    PROXMOX_DNS_SERVER PROXMOX_ADMIN_EMAIL PROXMOX_ROOT_PASSWORD
    PROXMOX_TIMEZONE PROXMOX_COUNTRY PROXMOX_KEYBOARD
    PROXMOX_ISO_FILE_PATH PROXMOX_ISO_FILE_SHA256 PROXMOX_PUBLIC_MAC
    TAILSCALE_HOSTNAME TAILSCALE_TAG
    ADMIN_1_PUBLIC_SSH_KEY ADMIN_2_PUBLIC_SSH_KEY
    PROXMOX_SECONDARY_MAC PROXMOX_SECONDARY_IP MOX_REPLICATION_IP
    PROXMOX_PRIVATE_BRIDGE
    PROXMOX_MIGRATION_NETWORK PROXMOX_CLUSTER_NAME PROXMOX_QDEVICE_HOST
    PROXMOX_INTERNAL_DOMAIN
    MAX_MOX_HOSTS PRIVATE_SUBNET_CIDR PRIVATE_SUBNET_PREFIX
    MOX_IP_START MOX_IP_END MOX_IP_START_OCTET
    HAPROXY_IP_START HAPROXY_IP_END HAPROXY_IP_START_OCTET
    GUEST_EGRESS_VIP PRODUCTION_IP_START PRODUCTION_IP_END
    STAGING_IP_START STAGING_IP_END VRRP_PRIORITY
    HAPROXY_LXC_VMID HAPROXY_LXC_HOSTNAME HAPROXY_LXC_IP
    HAPROXY_LXC_GATEWAY HAPROXY_LXC_ROOTFS_GB HAPROXY_LXC_MEMORY_MB
    HAPROXY_LXC_CORES HAPROXY_LXC_STORAGE GUEST_ROLE_HOOK_PATH
    PRODUCTION_VM_TAG STAGING_VM_TAG EVICTABLE_VM_TAG
    CLUSTER_STATE_DIR PROD_VM_CORES PROD_VM_MEMORY_GIB PROD_VM_DISK_GB
    PROD_VM_REPLICATION_INTERVAL STAGING_VM_CORES STAGING_VM_MEMORY_GIB
    MAX_PROD_VM_COUNT_ON_THIS_HOST MAX_STAGING_VM_COUNT_ON_THIS_HOST
  )
  local name
  for name in "${required[@]}"; do require_var "$name"; done
  # Relative paths are anchored at the repo root, not the caller's cwd.
  # shellcheck disable=SC2088 # Matching a literal, unexpanded tilde.
  if [[ "$PROXMOX_ISO_FILE_PATH" == "~/"* ]]; then
    PROXMOX_ISO_FILE_PATH="${HOME:?HOME is required to expand PROXMOX_ISO_FILE_PATH}/${PROXMOX_ISO_FILE_PATH#"~/"}"
  elif [[ "$PROXMOX_ISO_FILE_PATH" != /* ]]; then
    PROXMOX_ISO_FILE_PATH="${REPO_ROOT}/${PROXMOX_ISO_FILE_PATH}"
  fi
  if [[ "$HARDWARE_INVENTORY_MODE" == idrac ]]; then
    for name in IDRAC_IP IDRAC_USER IDRAC_PASSWORD; do require_var "$name"; done
  fi

  [[ "$TAILSCALE_TAG" == "tag:proxmox-host" ]] || fail "TAILSCALE_TAG must be tag:proxmox-host"
  [[ "$TAILSCALE_HOSTNAME" == "$SELECTED_HOST" ]] ||
    fail "Loaded TAILSCALE_HOSTNAME does not match --host $SELECTED_HOST"
  [[ "${PROXMOX_FQDN%%.*}" == "$TAILSCALE_HOSTNAME" ]] ||
    fail "PROXMOX_FQDN short name must equal TAILSCALE_HOSTNAME ($TAILSCALE_HOSTNAME)"
  [[ "$PROXMOX_PRIVATE_BRIDGE" =~ ^[A-Za-z0-9_.:-]+$ ]] || fail "Unsafe PROXMOX_PRIVATE_BRIDGE"
  [[ "$PROXMOX_CLUSTER_NAME" =~ ^[A-Za-z0-9_-]+$ ]] || fail "Unsafe PROXMOX_CLUSTER_NAME"
  [[ "$PROXMOX_QDEVICE_HOST" =~ ^[A-Za-z0-9._-]+$ ]] || fail "Unsafe PROXMOX_QDEVICE_HOST"
  [[ "$HAPROXY_LXC_HOSTNAME" =~ ^[A-Za-z0-9._-]+$ ]] || fail "Unsafe HAPROXY_LXC_HOSTNAME"
  [[ "$HAPROXY_LXC_VMID" =~ ^[1-9][0-9]{2,8}$ ]] || fail "HAPROXY_LXC_VMID must be a numeric Proxmox VMID"
  [[ "$HAPROXY_LXC_ROOTFS_GB" =~ ^[1-9][0-9]*$ ]] || fail "Invalid HAPROXY_LXC_ROOTFS_GB"
  [[ "$HAPROXY_LXC_MEMORY_MB" =~ ^[1-9][0-9]*$ ]] || fail "Invalid HAPROXY_LXC_MEMORY_MB"
  [[ -z "${LUKS_PASSWORD:-}${LUKS_PASSPHRASE:-}${CRYPT_PASSWORD:-}" ]] ||
    fail "Remove the LUKS secret from configuration; it must only be entered at the target host console"

  CONFIGURED_NVME_SERIALS=()
  CONFIGURED_MIRROR_PAIRS=()
  EXPECTED_RPOOL_MAPPERS=()
  local pair member serial_name serial capacity_name capacity
  # Pairs 1-4 may have gaps: decommissioning a mirror comments out its
  # entries, and pair N keeps its crypt-rpool-mirrorN-* LUKS names.
  for pair in 0 1 2 3 4; do
    serial_name="NVME_MIRROR_${pair}_SERIAL_1"
    [[ -n "${!serial_name:-}" ]] || continue
    CONFIGURED_MIRROR_PAIRS+=("$pair")
    for member in 1 2; do
      serial_name="NVME_MIRROR_${pair}_SERIAL_${member}"
      serial="${!serial_name}"
      [[ "$serial" =~ ^[A-Za-z0-9._:+-]+$ ]] ||
        fail "$serial_name contains characters unsafe for exact disk selection"
      CONFIGURED_NVME_SERIALS+=("$serial")
      if [[ "$HARDWARE_INVENTORY_MODE" == manual ]]; then
        capacity_name="NVME_MIRROR_${pair}_CAPACITY_BYTES_${member}"
        capacity="${!capacity_name:-}"
        [[ "$capacity" =~ ^[1-9][0-9]*$ ]] ||
          fail "$capacity_name must be an exact positive byte count in manual inventory mode; gather it by running hosts/cluster_setup_prereq.sh in a Linux Live environment booted on the host"
      fi
    done
  done
  [[ "${CONFIGURED_MIRROR_PAIRS[0]:-}" == 0 ]] || fail "NVMe mirror 0 is mandatory"
  RPOOL_LAYOUT_SHA256="$(
    printf '%s\n' "${CONFIGURED_NVME_SERIALS[@]}" |
      sha256sum | awk '{print $1}'
  )"

  python3 <<'PY'
import ipaddress
import os

public = ipaddress.ip_interface(f'{os.environ["PROXMOX_IP"]}/{os.environ["PROXMOX_PREFIX"]}')
gateway = ipaddress.ip_address(os.environ["PROXMOX_GATEWAY"])
if gateway not in public.network:
    raise SystemExit("PROXMOX_GATEWAY is outside the public network")

private = ipaddress.ip_interface(os.environ["PROXMOX_SECONDARY_IP"])
migration_address = ipaddress.ip_interface(os.environ["MOX_REPLICATION_IP"])
migration = ipaddress.ip_network(os.environ["PROXMOX_MIGRATION_NETWORK"], strict=True)
haproxy = ipaddress.ip_interface(os.environ["HAPROXY_LXC_IP"])
haproxy_gateway = ipaddress.ip_address(os.environ["HAPROXY_LXC_GATEWAY"])
egress_vip = ipaddress.ip_interface(os.environ["GUEST_EGRESS_VIP"])
if migration_address.network != migration:
    raise SystemExit("MOX_REPLICATION_IP must use PROXMOX_MIGRATION_NETWORK exactly")
for label, address in (
    ("host private IP", private.ip),
    ("HAProxy IP", haproxy.ip),
    ("guest-egress VIP", egress_vip.ip),
):
    if address in migration:
        raise SystemExit(f"{label} must be outside PROXMOX_MIGRATION_NETWORK")
if haproxy_gateway != private.ip:
    raise SystemExit("HAPROXY_LXC_GATEWAY must be this host's PROXMOX_SECONDARY_IP address")
if len({private.ip, migration_address.ip, egress_vip.ip, haproxy.ip}) != 4:
    raise SystemExit("Host, replication, egress VIP, and HAProxy addresses must be distinct")
PY

  HOST_ID="$SELECTED_HOST"
  HOST_ARTIFACTS="${ARTIFACTS_DIR}/${HOST_ID}"
  LOG_DIR="${HOST_ARTIFACTS}/logs"
  STATE_DIR="${HOST_ARTIFACTS}/state"
  GENERATED_DIR="${HOST_ARTIFACTS}/generated"
  SSH_DIR="${HOST_ARTIFACTS}/ssh"
  HEADER_DIR="${HOST_ARTIFACTS}/luks-headers"
  SSH_KEY="${SSH_DIR}/${HOST_ID}_setup_ed25519"
  HOST_KEY="${SSH_DIR}/${HOST_ID}_ssh_host_ed25519_key"
  HOST_KEY_PUB="${HOST_KEY}.pub"
  KNOWN_HOSTS="${STATE_DIR}/known_hosts"
  mkdir -p "$LOG_DIR" "$STATE_DIR" "$GENERATED_DIR" "$SSH_DIR" "$HEADER_DIR"
  chmod 0700 "$HOST_ARTIFACTS" "$LOG_DIR" "$STATE_DIR" "$GENERATED_DIR" "$SSH_DIR" "$HEADER_DIR"
  has_state admin-ssh-enabled && USE_ADMIN_SSH=1

  GUEST_ROLE_HOOK_SOURCE="$CANONICAL_GUEST_ROLE_HOOK_SOURCE"
  GUEST_ROLE_HOOK_NAME="$(basename -- "$GUEST_ROLE_HOOK_SOURCE")"
  [[ "$GUEST_ROLE_HOOK_NAME" =~ ^[A-Za-z0-9._-]+\.sh$ ]] ||
    fail "Unsafe lifecycle hook filename: $GUEST_ROLE_HOOK_NAME"
  local configured_hook_name
  configured_hook_name="$(basename -- "$GUEST_ROLE_HOOK_PATH")"
  [[ "$configured_hook_name" =~ ^[A-Za-z0-9._-]+\.sh$ ]] ||
    fail "Unsafe configured lifecycle hook filename: $configured_hook_name"
  [[ "$configured_hook_name" == "$GUEST_ROLE_HOOK_NAME" ]] ||
    fail "GUEST_ROLE_HOOK_PATH must name $GUEST_ROLE_HOOK_NAME"
  GUEST_ROLE_HOOK_COMPAT_NAME=""

  # Include secret-backed setup inputs in the fingerprint without ever
  # recording or printing their values.
  HOST_SETUP_CONFIG_SHA256="$(
    {
      printf 'public=%s\n' "$CONFIG_EFFECTIVE_SHA256"
      printf 'hardware-inventory-mode=%s\n' "$HARDWARE_INVENTORY_MODE"
      printf 'PROXMOX_ROOT_PASSWORD=%s\n' "$PROXMOX_ROOT_PASSWORD"
      if [[ "$HARDWARE_INVENTORY_MODE" == idrac ]]; then
        for name in IDRAC_USER IDRAC_PASSWORD; do
          printf '%s=%s\n' "$name" "${!name}"
        done
      fi
      if [[ -n "${PROXMOX_LUKS_PASSWORD+x}" ]]; then
        printf 'PROXMOX_LUKS_PASSWORD=%s\n' "$PROXMOX_LUKS_PASSWORD"
      else
        printf 'PROXMOX_LUKS_PASSWORD=<unset>\n'
      fi
    } | sha256sum | awk '{print $1}'
  )"
  local config_hash_file="${STATE_DIR}/configuration-sha256"
  if [[ -s "$config_hash_file" && "$(<"$config_hash_file")" != "$HOST_SETUP_CONFIG_SHA256" ]]; then
    confirm_exact \
      "Loaded configuration changed after setup state was created for $HOST_ID. Review cluster.conf, ${HOST_ID}.conf, and secrets.env before accepting it. Pending installation media will be rebuilt against the new inputs." \
      "ACCEPT UPDATED CONFIG FOR ${HOST_ID}"
  fi
  printf '%s\n' "$HOST_SETUP_CONFIG_SHA256" >"$config_hash_file"
  chmod 0600 "$config_hash_file"
  write_state hardware-inventory-mode "$HARDWARE_INVENTORY_MODE"
}

preflight() {
  require_command curl jq openssl ssh scp ssh-keygen ssh-keyscan sha256sum python3 \
    ip tailscale git awk base64 flock shred
  if source_iso_is_required; then
    require_command xorriso dpkg-deb
  fi
  [[ -f "$GUEST_ROLE_HOOK_SOURCE" ]] ||
    fail "Missing production/staging guest hook: $GUEST_ROLE_HOOK_SOURCE"
  local shared_source
  for shared_source in "$CONFIG_LIB" "$CLUSTER_CONFIG_SOURCE" \
    "$CLUSTER_REGISTRY_SOURCE" "$HAPROXY_RENDERER_SOURCE" \
    "$HAPROXY_SYNC_SOURCE" "$DEFERRED_CLEANUP_SOURCE" \
    "$PROD_ISO_BUILDER_SOURCE" "$PROD_ISO_PREPARER_SOURCE" \
    "$GUEST_ROLE_HOOK_SOURCE"; do
    [[ -f "$shared_source" && ! -L "$shared_source" ]] ||
      fail "Missing shared orchestration source: $shared_source"
  done
  bash -n "$CONFIG_LIB" "$HAPROXY_SYNC_SOURCE" "$DEFERRED_CLEANUP_SOURCE" \
    "$PROD_ISO_PREPARER_SOURCE" "$GUEST_ROLE_HOOK_SOURCE"
  PYTHONDONTWRITEBYTECODE=1 python3 "$CLUSTER_REGISTRY_SOURCE" --help >/dev/null
  PYTHONDONTWRITEBYTECODE=1 python3 "$HAPROXY_RENDERER_SOURCE" --help >/dev/null
  PYTHONDONTWRITEBYTECODE=1 python3 "$PROD_ISO_BUILDER_SOURCE" --help >/dev/null

  if [[ "$HARDWARE_INVENTORY_MODE" == idrac ]]; then
    require_yes "Is the FiberState IPMI VPN connected and have you verified that iDRAC ${IDRAC_IP} is reachable?"
    ip route get "$IDRAC_IP" >/dev/null || fail "No route to iDRAC $IDRAC_IP"
  fi
  require_yes "Is this workstation connected to Tailscale?"
  tailscale status >/dev/null 2>&1 || fail "The local Tailscale client is not connected"
  if qd_probe_access; then
    write_state qdevice-ipv4 "$QD_IPV4"
  else
    printf '\nWARNING: The QDevice is not accessible: %s\n' "$QD_ACCESS_PROBLEM" >&2
  fi

  if has_state host-bootstrapped; then
    info "$HOST_ID is already enrolled in Tailscale; reusing the recorded installation state."
  fi

  git -C "$REPO_ROOT" check-ignore -q "${GENERATED_DIR}/answer.toml" ||
    fail "Generated secret artifacts are not ignored by Git"
  git -C "$REPO_ROOT" check-ignore -q "${HEADER_DIR}/probe" ||
    fail "LUKS header artifacts are not ignored by Git"

  if source_iso_is_required; then
    local iso="$PROXMOX_ISO_FILE_PATH"
    [[ -f "$iso" ]] || fail "Missing source ISO: $iso"
    local actual_hash
    actual_hash="$(sha256sum -- "$iso" | awk '{print $1}')"
    [[ "${actual_hash,,}" == "${PROXMOX_ISO_FILE_SHA256,,}" ]] ||
      fail "Source ISO SHA-256 mismatch: expected $PROXMOX_ISO_FILE_SHA256, got $actual_hash"
  fi

  if [[ ! -f "$SSH_KEY" ]]; then
    ssh-keygen -q -t ed25519 -N '' -C "${HOST_ID} setup automation" -f "$SSH_KEY"
    chmod 0600 "$SSH_KEY"
    chmod 0644 "$SSH_KEY.pub"
  fi
}

reserve_fresh_tailscale_auth_key() {
  [[ -z "${TAILSCALE_AUTH_KEY:-}" ]] ||
    fail "TAILSCALE_AUTH_KEY must not be loaded from a file; enter a fresh key only at the silent prompt"
  printf '\nA fresh Tailscale auth key is mandatory for this installation image.\n'
  info "Required properties: single-use, non-ephemeral, tag:proxmox-host"
  require_yes "Have you created a NEW key with exactly those properties?"
  prompt_tailscale_auth_key ||
    fail "A fresh Tailscale auth key is required to build the installation image"

  local key_hash used_file lock_file lock_fd
  key_hash="$(printf '%s' "$TAILSCALE_AUTH_KEY" | sha256sum | awk '{print $1}')"
  used_file="${ARTIFACTS_DIR}/used-tailscale-auth-key-sha256"
  lock_file="${used_file}.lock"
  exec {lock_fd}>"$lock_file"
  chmod 0600 "$lock_file"
  flock -x "$lock_fd"
  if [[ -f "$used_file" ]] && grep -Eq "^${key_hash}[[:space:]]" "$used_file"; then
    unset TAILSCALE_AUTH_KEY
    fail "This Tailscale auth key fingerprint was already reserved by this setup tooling. Create a new key."
  fi

  # Reserve before embedding: a key exposed in even a partially built image
  # must never be offered to another host or reinstall.
  printf '%s %s\n' "$key_hash" "$HOST_ID" >>"$used_file"
  sort -u -o "$used_file" "$used_file"
  chmod 0600 "$used_file"
  flock -u "$lock_fd"
  exec {lock_fd}>&-
  write_state tailscale-auth-key-sha256 "$key_hash"
}

# A host creates the cluster only when it is the control node and no cluster
# member is reachable; every other host joins through the control node.
choose_role() {
  log "Resolving the cluster control node"
  resolve_control_node || fail "Could not determine the cluster control node"
  local cluster_exists=false host_is_member=false
  if [[ -n "$CONTROL_PROBE_NODE" ]]; then
    cluster_exists=true
    if control_list_contains "$HOST_ID" "${CONTROL_MEMBER_NODES[@]}"; then
      host_is_member=true
    fi
    info "Cluster members: ${CONTROL_MEMBER_NODES[*]}"
  fi
  info "Control node: $CONTROL_NODE"

  if has_state setup-role; then
    SETUP_ROLE="$(read_state setup-role)"
    EXISTING_NODE="$(read_state existing-node)"
    case "$SETUP_ROLE" in
      first)
        [[ "$EXISTING_NODE" == none ]] ||
          fail "Recorded cluster role for $HOST_ID is invalid; expected first/none"
        if [[ "$cluster_exists" == true && "$host_is_member" != true ]]; then
          fail "$HOST_ID was recorded as the host that creates the cluster, but cluster members ${CONTROL_MEMBER_NODES[*]} already exist without it. Delete this host's setup artifacts and rerun so it joins through $CONTROL_NODE."
        fi
        [[ "$cluster_exists" == true || "$HOST_ID" == "$CONTROL_NODE" ]] ||
          fail "$HOST_ID was recorded as the host that creates the cluster, but the control node is now $CONTROL_NODE"
        ;;
      join)
        [[ "$EXISTING_NODE" =~ ^mox([1-9]|10)$ ]] ||
          fail "Recorded cluster role for $HOST_ID is invalid; expected join/moxN"
        ;;
      *)
        fail "Recorded cluster role for $HOST_ID is invalid: $SETUP_ROLE"
        ;;
    esac
  elif [[ "$cluster_exists" != true && "$HOST_ID" != "$CONTROL_NODE" ]]; then
    SETUP_ROLE='join'
  elif [[ "$cluster_exists" != true ]]; then
    confirm_exact \
      "No member of an existing Proxmox cluster is reachable from this workstation. $HOST_ID is the control node, so it will CREATE a new cluster named $PROXMOX_CLUSTER_NAME. If a cluster already exists but is unreachable, stop now and restore connectivity instead." \
      "CREATE CLUSTER ON ${HOST_ID}"
    SETUP_ROLE=first
    EXISTING_NODE=none
  else
    SETUP_ROLE='join'
  fi

  if [[ "$SETUP_ROLE" == join ]]; then
    [[ "$CONTROL_NODE" != "$HOST_ID" || "$host_is_member" == true ]] ||
      fail "$HOST_ID is the control node but is not a cluster member; PROXMOX_CONTROL_NODE must name an existing member"
    EXISTING_NODE="$CONTROL_NODE"
    if [[ "$cluster_exists" != true ]]; then
      info "No reachable cluster member was found yet. Assuming control node $CONTROL_NODE will create the cluster; $HOST_ID will wait for it before joining, after its private VLAN is verified."
    elif [[ "$host_is_member" == true ]]; then
      info "$HOST_ID is already a cluster member; cluster changes go through control node $CONTROL_NODE."
    else
      info "$HOST_ID will join the cluster through control node $CONTROL_NODE after its private VLAN is verified."
    fi
  fi
  write_state setup-role "$SETUP_ROLE"
  write_state existing-node "$EXISTING_NODE"
}

# The member that runs cluster-wide commands: this host when it created the
# cluster or is itself the control node, otherwise the control node.
cluster_control_node() {
  if [[ "$SETUP_ROLE" == first || "$EXISTING_NODE" == "$HOST_ID" ]]; then
    printf '%s\n' "$HOST_ID"
  else
    printf '%s\n' "$EXISTING_NODE"
  fi
}

# Print every configured cluster member, lowest-numbered first.
cluster_member_nodes() {
  local nodes_json
  nodes_json="$(cluster_control pvesh get /nodes --output-format json)" ||
    fail "Could not list cluster members through $(cluster_control_node)"
  jq -er --argjson max "$MAX_MOX_HOSTS" '
    [ .[] | .node |
      if type == "string" and test("^mox([1-9]|10)$") and
         ((ltrimstr("mox") | tonumber) <= $max)
      then . else error("unexpected cluster node: \(.)") end ] |
    sort_by(ltrimstr("mox") | tonumber) | .[]
  ' <<<"$nodes_json" || fail "Cluster membership is malformed"
}

choose_encryption_policy() {
  if has_state encryption-policy; then
    ENCRYPTION_POLICY="$(read_state encryption-policy)"
    [[ "$ENCRYPTION_POLICY" == luks || "$ENCRYPTION_POLICY" == clear ]] ||
      fail "Invalid recorded encryption policy: $ENCRYPTION_POLICY"
    if [[ -n "$REQUESTED_ENCRYPTION_POLICY" && "$REQUESTED_ENCRYPTION_POLICY" != "$ENCRYPTION_POLICY" ]]; then
      fail "Cannot change the recorded encryption policy from $ENCRYPTION_POLICY to $REQUESTED_ENCRYPTION_POLICY while resuming. Start fresh only if reinstalling this host is safe."
    fi
  elif has_state luks-A-attached || has_state luks-B-attached ||
       has_state encrypted-mirror-verified || has_state shared-luks-unlock-configured; then
    ENCRYPTION_POLICY=luks
    [[ -z "$REQUESTED_ENCRYPTION_POLICY" || "$REQUESTED_ENCRYPTION_POLICY" == luks ]] ||
      fail "Existing LUKS conversion state cannot be resumed with --no-encrypt"
    write_state encryption-policy "$ENCRYPTION_POLICY"
    info "Recorded LUKS conversion artifacts prove this is an encrypted legacy run; preserving that policy."
  elif [[ -n "$REQUESTED_ENCRYPTION_POLICY" ]]; then
    ENCRYPTION_POLICY="$REQUESTED_ENCRYPTION_POLICY"
    write_state encryption-policy "$ENCRYPTION_POLICY"
  else
    printf '\nRPOOL ENCRYPTION CHOICE\n'
    printf 'LUKS2 protects data at rest, but every Proxmox host boot requires the shared rpool passphrase to be entered through the target host console. The host cannot complete an unattended reboot.\n'
    if bmac_ui_is_json; then
      bmac_ui_choose ENCRYPTION_POLICY "rpool encryption" luks \
        luks "LUKS2: every boot needs the passphrase at the host console" \
        clear "Unencrypted: unattended reboots"
    elif prompt_yes "Encrypt every configured rpool mirror member with LUKS2"; then
      ENCRYPTION_POLICY=luks
    else
      ENCRYPTION_POLICY=clear
    fi
    write_state encryption-policy "$ENCRYPTION_POLICY"
  fi

  if [[ "$ENCRYPTION_POLICY" == luks ]]; then
    require_var PROXMOX_LUKS_PASSWORD
    EXPECTED_RPOOL_MAPPERS=(crypt-rpool-a crypt-rpool-b)
    local pair member
    for pair in "${CONFIGURED_MIRROR_PAIRS[@]:1}"; do
      for member in 1 2; do
        EXPECTED_RPOOL_MAPPERS+=("crypt-rpool-mirror${pair}-${member}")
      done
    done
    info "LUKS2 encryption is enabled; every host boot will require console passphrase entry."
  else
    unset PROXMOX_LUKS_PASSWORD 2>/dev/null || true
    info "LUKS2 encryption is disabled; configured rpool members will remain unencrypted."
  fi
}

choose_boot_test_policy() {
  if has_state boot-test-policy; then
    BOOT_TEST_POLICY="$(read_state boot-test-policy)"
    [[ "$BOOT_TEST_POLICY" == run || "$BOOT_TEST_POLICY" == skip ]] ||
      fail "Invalid recorded boot-test policy: $BOOT_TEST_POLICY"
    if [[ -n "$REQUESTED_BOOT_TEST_POLICY" && "$REQUESTED_BOOT_TEST_POLICY" != "$BOOT_TEST_POLICY" ]]; then
      if [[ "$REQUESTED_BOOT_TEST_POLICY" == skip ]] &&
         { has_state raw-A-only-passed ||
           has_state raw-B-only-passed ||
           has_state encrypted-final-A-only-passed ||
           has_state encrypted-final-B-only-passed ||
           has_state final-normal-boot-active; } &&
         ! has_state final-normal-boot-passed; then
        fail "Cannot skip boot tests after the three-reboot drill has started; resume the remaining member test and final healthy-state reboot"
      fi
      BOOT_TEST_POLICY="$REQUESTED_BOOT_TEST_POLICY"
      write_state boot-test-policy "$BOOT_TEST_POLICY"
    fi
  elif [[ -n "$REQUESTED_BOOT_TEST_POLICY" ]]; then
    BOOT_TEST_POLICY="$REQUESTED_BOOT_TEST_POLICY"
    write_state boot-test-policy "$BOOT_TEST_POLICY"
  elif bmac_ui_is_json; then
    bmac_ui_choose BOOT_TEST_POLICY "Boot tests" run \
      run "Test each mirror member alone, then the healthy mirror (three reboots)" \
      skip "Skip the boot tests"
    write_state boot-test-policy "$BOOT_TEST_POLICY"
  elif prompt_yes "Test each mirror member independently and then reboot once more with the restored healthy mirror? This requires three reboots"; then
    BOOT_TEST_POLICY=run
    write_state boot-test-policy "$BOOT_TEST_POLICY"
  else
    BOOT_TEST_POLICY=skip
    write_state boot-test-policy "$BOOT_TEST_POLICY"
  fi

  if [[ "$BOOT_TEST_POLICY" == skip ]]; then
    local active
    for active in raw-A-test-active raw-B-test-active encrypted-final-A-test-active encrypted-final-B-test-active final-normal-boot-active; do
      has_state "$active" &&
        fail "Cannot skip boot tests while interrupted test state '$active' is active; resume and complete that test"
    done
    info "Optional boot tests are disabled for $HOST_ID; storage setup and non-boot verification remain enabled."
  else
    info "Mirror-member tests and the final healthy-state reboot are enabled for $HOST_ID (three reboots total)."
  fi
}

redfish_get() {
  local path=$1 credentials
  credentials="${IDRAC_USER}:${IDRAC_PASSWORD}"
  credentials="${credentials//\\/\\\\}"
  credentials="${credentials//\"/\\\"}"
  # Feed credentials through curl's stdin config so they never appear in the
  # process argument list.
  printf 'user = "%s"\n' "$credentials" |
    curl --config - --fail --silent --show-error --insecure \
      --connect-timeout 10 --max-time 45 -H 'Accept: application/json' \
      "https://${IDRAC_IP}${path}"
  credentials=""
}

redfish_collection() {
  local path=$1 prefix=$2 member index=0
  redfish_get "$path" >"${prefix}.collection.json"
  while IFS= read -r member; do
    redfish_get "$member" >"${prefix}.${index}.json"
    index=$((index + 1))
  done < <(jq -r '.Members[]?."@odata.id"' "${prefix}.collection.json")
}

discover_hardware() {
  local recorded_hardware_complete=true pair
  has_state zfs-hdsize-gib || recorded_hardware_complete=false
  for pair in "${CONFIGURED_MIRROR_PAIRS[@]}"; do
    has_state "mirror-${pair}-minimum-capacity-bytes" ||
      recorded_hardware_complete=false
  done
  if has_state hardware-verified &&
     has_state hardware-config-sha256 &&
     [[ "$(read_state hardware-config-sha256)" == "$HOST_SETUP_CONFIG_SHA256" ]] &&
     [[ "$recorded_hardware_complete" == true ]]; then
    return
  fi

  if [[ "$HARDWARE_INVENTORY_MODE" == manual ]]; then
    log "Validating manually inventoried disk capacities"
    local capacity_1_name capacity_2_name capacity_1 capacity_2 smaller larger
    for pair in "${CONFIGURED_MIRROR_PAIRS[@]}"; do
      capacity_1_name="NVME_MIRROR_${pair}_CAPACITY_BYTES_1"
      capacity_2_name="NVME_MIRROR_${pair}_CAPACITY_BYTES_2"
      capacity_1="${!capacity_1_name}"
      capacity_2="${!capacity_2_name}"
      [[ "$capacity_1" =~ ^[1-9][0-9]*$ && "$capacity_2" =~ ^[1-9][0-9]*$ ]] ||
        fail "Mirror $pair capacities must be exact positive byte counts"
      ((capacity_1 >= 8 * 1073741824 && capacity_2 >= 8 * 1073741824)) ||
        fail "Mirror $pair contains a disk smaller than 8 GiB"
      smaller=$((capacity_1 < capacity_2 ? capacity_1 : capacity_2))
      larger=$((capacity_1 < capacity_2 ? capacity_2 : capacity_1))
      (((larger - smaller) * 100 <= larger)) ||
        fail "Mirror $pair disk capacities differ by more than 1% (${capacity_1} and ${capacity_2} bytes)"
      write_state "mirror-${pair}-minimum-capacity-bytes" "$smaller"
      info "Mirror $pair: ${capacity_1} and ${capacity_2} bytes; the mirror uses ${smaller}"
    done

    local manual_hdsize_gib
    capacity_1="$(read_state mirror-0-minimum-capacity-bytes)"
    manual_hdsize_gib="$((capacity_1 / 1073741824 - 4))"
    ((manual_hdsize_gib > 0)) || fail "Computed invalid ZFS hdsize"
    ((capacity_1 - manual_hdsize_gib * 1073741824 >= 4 * 1073741824)) ||
      fail "ZFS hdsize does not reserve at least 4 GiB for LUKS metadata"
    write_state zfs-hdsize-gib "$manual_hdsize_gib"
    write_state hardware-config-sha256 "$HOST_SETUP_CONFIG_SHA256"
    write_state hardware-verified manual
    return
  fi

  log "Discovering iDRAC hardware without changing it"
  local run_dir
  run_dir="${LOG_DIR}/idrac-$(date -u +%Y%m%dT%H%M%SZ)"
  mkdir -p "$run_dir"
  chmod 0700 "$run_dir"

  local system_uri manager_uri
  system_uri="$(redfish_get /redfish/v1/Systems | jq -er '.Members[0]."@odata.id"')"
  manager_uri="$(redfish_get /redfish/v1/Managers | jq -er '.Members[0]."@odata.id"')"
  redfish_get "$system_uri" >"${run_dir}/system.json"
  redfish_get "$manager_uri" >"${run_dir}/manager.json"
  redfish_collection "${system_uri}/Storage" "${run_dir}/storage"

  local drive_uri
  : >"${run_dir}/drives.jsonl"
  while IFS= read -r drive_uri; do
    redfish_get "$drive_uri" | jq -c '.' >>"${run_dir}/drives.jsonl"
  done < <(
    jq -r '.. | objects | .["@odata.id"]? // empty' "${run_dir}"/storage*.json |
      awk '/\/Drives\// && !seen[$0]++'
  )

  jq -s 'map({
    id: .Id, model: .Model, serial: .SerialNumber,
    media_type: .MediaType, protocol: .Protocol,
    capacity_bytes: .CapacityBytes, status: .Status
  })' "${run_dir}/drives.jsonl" >"${run_dir}/drives-summary.json"

  local serial count capacity health
  for serial in "${CONFIGURED_NVME_SERIALS[@]}"; do
    count="$(jq --arg serial "$serial" '[.[] | select(
      .serial == $serial and
      (.media_type // "" | ascii_upcase) == "SSD" and
      ((.protocol // "" | ascii_upcase) == "NVME" or
       (.protocol // "" | ascii_upcase) == "PCIE")
    )] | length' "${run_dir}/drives-summary.json")"
    [[ "$count" == 1 ]] ||
      fail "Expected exactly one eligible NVMe/PCIe SSD with serial $serial; iDRAC reported $count"
    capacity="$(jq -er --arg serial "$serial" \
      '.[] | select(.serial == $serial) | .capacity_bytes' \
      "${run_dir}/drives-summary.json")"
    if [[ ! "$capacity" =~ ^[0-9]+$ ]] ||
       ((capacity < 8 * 1073741824)); then
      fail "Drive $serial has an invalid or unexpectedly small capacity"
    fi
    health="$(jq -r --arg serial "$serial" \
      '.[] | select(.serial == $serial) |
       if (.status | type) == "object"
       then (.status.Health // .status.HealthRollup // "OK")
       else "OK"
       end' \
      "${run_dir}/drives-summary.json")"
    [[ "${health^^}" == OK ]] ||
      fail "Drive $serial is not healthy according to iDRAC (health: $health)"
  done
  count="$(jq '[.[] | select(
    (.media_type // "" | ascii_upcase) == "SSD" and
    ((.protocol // "" | ascii_upcase) == "NVME" or (.protocol // "" | ascii_upcase) == "PCIE")
  )] | length' "${run_dir}/drives-summary.json")"
  ((count >= ${#CONFIGURED_NVME_SERIALS[@]})) ||
    fail "iDRAC reported fewer eligible NVMe/PCIe SSDs than configured"
  if ((count > ${#CONFIGURED_NVME_SERIALS[@]})); then
    info "iDRAC reported $count eligible NVMe/PCIe SSDs; only the ${#CONFIGURED_NVME_SERIALS[@]} explicitly configured serials will be used."
  fi

  local serial_1_name serial_2_name serial_1 serial_2
  local capacity_1 capacity_2 larger_bytes smaller_bytes difference
  for pair in "${CONFIGURED_MIRROR_PAIRS[@]}"; do
    serial_1_name="NVME_MIRROR_${pair}_SERIAL_1"
    serial_2_name="NVME_MIRROR_${pair}_SERIAL_2"
    serial_1="${!serial_1_name}"
    serial_2="${!serial_2_name}"
    capacity_1="$(jq -er --arg serial "$serial_1" \
      '.[] | select(.serial == $serial) | .capacity_bytes' \
      "${run_dir}/drives-summary.json")"
    capacity_2="$(jq -er --arg serial "$serial_2" \
      '.[] | select(.serial == $serial) | .capacity_bytes' \
      "${run_dir}/drives-summary.json")"
    if ((capacity_1 >= capacity_2)); then
      larger_bytes="$capacity_1"
      smaller_bytes="$capacity_2"
    else
      larger_bytes="$capacity_2"
      smaller_bytes="$capacity_1"
    fi
    difference="$((larger_bytes - smaller_bytes))"
    ((difference * 100 <= larger_bytes)) ||
      fail "NVMe mirror $pair members differ in capacity by more than one percent"
    write_state "mirror-${pair}-minimum-capacity-bytes" "$smaller_bytes"
  done

  local hdsize_gib
  smaller_bytes="$(read_state mirror-0-minimum-capacity-bytes)"
  [[ "$smaller_bytes" =~ ^[0-9]+$ ]] || fail "Could not determine NVMe capacity"
  hdsize_gib="$((smaller_bytes / 1073741824 - 4))"
  ((hdsize_gib > 0)) || fail "Computed invalid ZFS hdsize"
  ((smaller_bytes - hdsize_gib * 1073741824 >= 4 * 1073741824)) ||
    fail "ZFS hdsize does not reserve at least 4 GiB for LUKS metadata"
  write_state zfs-hdsize-gib "$hdsize_gib"
  write_state hardware-config-sha256 "$HOST_SETUP_CONFIG_SHA256"
  write_state hardware-verified "$run_dir"
  jq . "${run_dir}/drives-summary.json"
}

assistant_binary() {
  local path="${ARTIFACTS_DIR}/tools/proxmox-auto-install-assistant-9.2.5/usr/bin/proxmox-auto-install-assistant"
  if command -v proxmox-auto-install-assistant >/dev/null 2>&1; then
    command -v proxmox-auto-install-assistant
    return
  fi
  if [[ ! -x "$path" ]]; then
    # assistant_binary is consumed through command substitution, so stdout is
    # reserved exclusively for the resolved executable path.
    log "Downloading and verifying the official Proxmox auto-install assistant package" >&2
    mkdir -p "${ARTIFACTS_DIR}/tools"
    local deb="${ARTIFACTS_DIR}/tools/proxmox-auto-install-assistant_9.2.5_amd64.deb"
    curl --fail --location --silent --show-error \
      "http://download.proxmox.com/debian/pve/dists/trixie/pve-no-subscription/binary-amd64/proxmox-auto-install-assistant_9.2.5_amd64.deb" \
      -o "$deb"
    printf '%s  %s\n' \
      '9a61e37c3efce192a5ea86af06b3ccf7027fe581226068911d4833f4af7ac83e' "$deb" |
      sha256sum --check --strict >&2
    rm -rf "${ARTIFACTS_DIR}/tools/proxmox-auto-install-assistant-9.2.5"
    mkdir -p "${ARTIFACTS_DIR}/tools/proxmox-auto-install-assistant-9.2.5"
    dpkg-deb -x "$deb" "${ARTIFACTS_DIR}/tools/proxmox-auto-install-assistant-9.2.5"
  fi
  [[ -x "$path" ]] || fail "Could not prepare proxmox-auto-install-assistant"
  printf '%s\n' "$path"
}

build_iso() {
  if installation_media_is_current; then
    return
  fi
  if has_state iso-built; then
    confirm_exact \
      "The pending installation ISO was built from older or unverifiable inputs. Rebuild it with a newly entered one-time Tailscale key before installing $HOST_ID." \
      "REBUILD INSTALL ISO FOR ${HOST_ID}"
    rm -f "$(state_path iso-built)" "$(state_path prepared-iso)" \
      "$(state_path iso-config-sha256)"
  fi

  reserve_fresh_tailscale_auth_key
  log "Building the host-specific automated Proxmox ISO"
  local assistant answer first_boot output hdsize setup_key_b64 auth_key_b64 iso_name
  local root_password_hash root_password_salt
  assistant="$(assistant_binary)"
  answer="${GENERATED_DIR}/answer.toml"
  first_boot="${GENERATED_DIR}/first-boot.sh"
  iso_name="$(basename -- "${PROXMOX_ISO_FILE_PATH%.iso}")"
  output="${HOST_ARTIFACTS}/${iso_name}-${HOST_ID}-auto.iso"
  hdsize="$(read_state zfs-hdsize-gib)"
  # Every image gets a new host key: an earlier image may still exist
  # somewhere, and its embedded private key must not identify this install.
  rm -f -- "$HOST_KEY" "$HOST_KEY_PUB"
  ssh-keygen -q -t ed25519 -N '' -C "root@${PROXMOX_FQDN}" -f "$HOST_KEY"
  chmod 0600 "$HOST_KEY"
  chmod 0644 "$HOST_KEY_PUB"
  setup_key_b64="$(base64 -w0 <"$SSH_KEY.pub")"
  auth_key_b64="$(printf '%s' "$TAILSCALE_AUTH_KEY" | base64 -w0)"
  root_password_salt="$(
    printf 'app-ha-proxmox-root-v1:%s:%s' "$PROXMOX_CLUSTER_NAME" "$HOST_ID" |
      sha256sum | awk '{print substr($1, 1, 16)}'
  )"
  root_password_hash="$(
    printf '%s' "$PROXMOX_ROOT_PASSWORD" |
      openssl passwd -6 -salt "$root_password_salt" -stdin
  )"
  [[ "$root_password_hash" == "\$6\$"* ]] ||
    fail "Could not derive the Proxmox root password hash"

  ANSWER_OUTPUT="$answer" FIRST_BOOT_OUTPUT="$first_boot" \
  SETUP_KEY="$(<"$SSH_KEY.pub")" SETUP_KEY_B64="$setup_key_b64" \
  HOST_KEY_B64="$(base64 -w0 <"$HOST_KEY")" \
  HOST_KEY_PUB_B64="$(base64 -w0 <"$HOST_KEY_PUB")" \
  ADMIN_1_KEY="$ADMIN_1_PUBLIC_SSH_KEY" ADMIN_2_KEY="$ADMIN_2_PUBLIC_SSH_KEY" \
  BOOT_SERIAL_1="$NVME_MIRROR_0_SERIAL_1" \
  BOOT_SERIAL_2="$NVME_MIRROR_0_SERIAL_2" \
  AUTH_KEY_B64="$auth_key_b64" ZFS_HDSIZE_GIB="$hdsize" \
  python3 3<<<"$root_password_hash" <<'PY'
import json
import os
from pathlib import Path

q = json.dumps
root_password_hash = os.fdopen(3, encoding="utf-8").read().rstrip("\n")
if not root_password_hash.startswith("$6$") or "\n" in root_password_hash:
    raise SystemExit("invalid Proxmox root password hash input")
answer = f"""[global]
keyboard = {q(os.environ["PROXMOX_KEYBOARD"])}
country = {q(os.environ["PROXMOX_COUNTRY"])}
fqdn = {q(os.environ["PROXMOX_FQDN"])}
mailto = {q(os.environ["PROXMOX_ADMIN_EMAIL"])}
timezone = {q(os.environ["PROXMOX_TIMEZONE"])}
root-password-hashed = {q(root_password_hash)}
root-ssh-keys = [{q(os.environ["SETUP_KEY"])}, {q(os.environ["ADMIN_1_KEY"])}, {q(os.environ["ADMIN_2_KEY"])}]
reboot-on-error = false
reboot-mode = "reboot"

[network]
source = "from-answer"
cidr = {q(os.environ["PROXMOX_IP"] + "/" + os.environ["PROXMOX_PREFIX"])}
dns = {q(os.environ["PROXMOX_DNS_SERVER"])}
gateway = {q(os.environ["PROXMOX_GATEWAY"])}
filter.ID_NET_NAME_MAC = {q("*" + os.environ["PROXMOX_PUBLIC_MAC"].replace(":", "").lower())}

[disk-setup]
filesystem = "zfs"
filter-match = "any"
filter.ID_SERIAL_SHORT = {q(os.environ["BOOT_SERIAL_1"])}
filter.ID_SERIAL = {q("*" + os.environ["BOOT_SERIAL_2"] + "*")}
zfs.raid = "raid1"
zfs.hdsize = {os.environ["ZFS_HDSIZE_GIB"]}

[first-boot]
source = "from-iso"
ordering = "before-network"
"""
Path(os.environ["ANSWER_OUTPUT"]).write_text(answer)

first_boot = r'''#!/usr/bin/env bash
set -Eeuo pipefail
set +x
umask 077
PUBLIC_IP=__PUBLIC_IP__
PUBLIC_GATEWAY=__PUBLIC_GATEWAY__
TAILSCALE_HOSTNAME=__TAILSCALE_HOSTNAME__
TAILSCALE_AUTH_KEY_B64=__AUTH_KEY_B64__
SETUP_KEY_B64=__SETUP_KEY_B64__
ADMIN_1_KEY_B64=__ADMIN_1_KEY_B64__
ADMIN_2_KEY_B64=__ADMIN_2_KEY_B64__
HOST_KEY_B64=__HOST_KEY_B64__
HOST_KEY_PUB_B64=__HOST_KEY_PUB_B64__

[[ "${1:-before-network}" == before-network ]]
command -v nft >/dev/null

# The workstation pinned this pre-generated key before the image was built.
# Installing it before sshd first starts means no other Ed25519 host key is
# ever served.
rm -f /etc/ssh/ssh_host_ed25519_key /etc/ssh/ssh_host_ed25519_key.pub
printf '%s' "$HOST_KEY_B64" | base64 -d >/etc/ssh/ssh_host_ed25519_key
printf '%s' "$HOST_KEY_PUB_B64" | base64 -d >/etc/ssh/ssh_host_ed25519_key.pub
chown root:root /etc/ssh/ssh_host_ed25519_key /etc/ssh/ssh_host_ed25519_key.pub
chmod 0600 /etc/ssh/ssh_host_ed25519_key
chmod 0644 /etc/ssh/ssh_host_ed25519_key.pub
[[ "$(ssh-keygen -y -f /etc/ssh/ssh_host_ed25519_key | awk '{print $1, $2}')" == \
   "$(awk '{print $1, $2}' /etc/ssh/ssh_host_ed25519_key.pub)" ]]
systemctl try-restart ssh.service

install -d -m 0755 /usr/local/sbin /etc/nftables.d
cat >/usr/local/sbin/app-ha-disk-by-serial <<'EOF'
#!/usr/bin/env bash
set -Eeuo pipefail
serial="${1:?serial is required}"
mapfile -t matches < <(lsblk -dnpo PATH,SERIAL | awk -v serial="$serial" '$2 == serial { print $1 }')
((${#matches[@]} == 1)) || { printf 'Expected one disk for serial %s; found %s\n' "$serial" "${#matches[@]}" >&2; exit 1; }
printf '%s\n' "${matches[0]}"
EOF
chmod 0755 /usr/local/sbin/app-ha-disk-by-serial

cat >/usr/local/sbin/app-ha-rpool-member-for-serial <<'EOF'
#!/usr/bin/env bash
set -Eeuo pipefail
serial="${1:?serial is required}"
disk="$(app-ha-disk-by-serial "$serial")"
expected="$(readlink -f "${disk}p3")"
mapfile -t matches < <(
  while IFS= read -r candidate; do
    [[ "$(readlink -f "$candidate")" == "$expected" ]] &&
      printf '%s\n' "$candidate"
  done < <(zpool status -P rpool | awk '$1 ~ /^\/dev\// { print $1 }')
)
((${#matches[@]} == 1)) || {
  printf 'Expected exactly one canonical rpool member for serial %s; found %s\n' \
    "$serial" "${#matches[@]}" >&2
  exit 1
}
printf '%s\n' "${matches[0]}"
EOF
chmod 0755 /usr/local/sbin/app-ha-rpool-member-for-serial

cat >/etc/nftables.d/app-ha-host-guard.nft <<EOF
table inet app_ha_host_guard {
  chain input {
    type filter hook input priority -200; policy accept;
    ip daddr ${PUBLIC_IP} tcp dport { 22, 3128, 8006 } counter drop
  }
}
EOF
cat >/usr/local/sbin/app-ha-load-host-guard <<'EOF'
#!/usr/bin/env bash
set -Eeuo pipefail
nft list table inet app_ha_host_guard >/dev/null 2>&1 && nft delete table inet app_ha_host_guard
nft -f /etc/nftables.d/app-ha-host-guard.nft
EOF
chmod 0755 /usr/local/sbin/app-ha-load-host-guard
cat >/etc/systemd/system/app-ha-host-guard.service <<'EOF'
[Unit]
Description=app-ha public Proxmox management-port guard
DefaultDependencies=no
Before=network-pre.target
Wants=network-pre.target
[Service]
Type=oneshot
ExecStart=/usr/local/sbin/app-ha-load-host-guard
RemainAfterExit=yes
[Install]
WantedBy=network-pre.target
EOF
systemctl daemon-reload
systemctl enable app-ha-host-guard.service
/usr/local/sbin/app-ha-load-host-guard

install -d -m 0700 /root/.ssh /root/.app-ha-bootstrap
: >/root/.ssh/authorized_keys
for encoded in "$SETUP_KEY_B64" "$ADMIN_1_KEY_B64" "$ADMIN_2_KEY_B64"; do
  printf '%s' "$encoded" | base64 -d >>/root/.ssh/authorized_keys
  printf '\n' >>/root/.ssh/authorized_keys
done
chmod 0600 /root/.ssh/authorized_keys
{ printf '%s' "$TAILSCALE_AUTH_KEY_B64" | base64 -d; printf '\n'; } >/root/.app-ha-bootstrap/tailscale-auth-key
chmod 0600 /root/.app-ha-bootstrap/tailscale-auth-key

cat >/usr/local/sbin/app-ha-bootstrap-tailscale <<EOF
#!/usr/bin/env bash
set -Eeuo pipefail
set +x

# network-online.target can be reached a few seconds before DNS and external
# HTTPS are actually usable. Wait here so a transient first-boot resolver race
# does not strand this host outside Tailscale.
repo_ready=false
for attempt in \$(seq 1 60); do
  if getent ahosts pkgs.tailscale.com >/dev/null 2>&1 &&
    curl -fsSI --connect-timeout 10 --max-time 30 \
      https://pkgs.tailscale.com/stable/debian/trixie.noarmor.gpg \
      >/dev/null; then
    repo_ready=true
    break
  fi
  echo "Waiting for DNS/HTTPS access to pkgs.tailscale.com (attempt \$attempt/60)"
  sleep 5
done
\$repo_ready || {
  echo "Tailscale repository remained unreachable after 5 minutes" >&2
  exit 1
}

install -d -m 0755 /usr/share/keyrings
curl -fsSL --retry 12 --retry-delay 5 --retry-all-errors \
  https://pkgs.tailscale.com/stable/debian/trixie.noarmor.gpg \
  -o /usr/share/keyrings/tailscale-archive-keyring.gpg
curl -fsSL --retry 12 --retry-delay 5 --retry-all-errors \
  https://pkgs.tailscale.com/stable/debian/trixie.tailscale-keyring.list \
  -o /etc/apt/sources.list.d/tailscale.list
chmod 0644 /usr/share/keyrings/tailscale-archive-keyring.gpg /etc/apt/sources.list.d/tailscale.list
apt-get update -o Acquire::Retries=5 -o Dir::Etc::sourcelist=/etc/apt/sources.list.d/tailscale.list -o Dir::Etc::sourceparts=- -o APT::Get::List-Cleanup=0
DEBIAN_FRONTEND=noninteractive apt-get install -y tailscale
systemctl enable --now tailscaled
tailscale up --auth-key=file:/root/.app-ha-bootstrap/tailscale-auth-key --hostname=${TAILSCALE_HOSTNAME}
tailscale ip -4 | grep -E '^100\.'
rm -f /root/.app-ha-bootstrap/tailscale-auth-key
touch /var/lib/app-ha-bootstrap-complete
chmod 0600 /var/lib/app-ha-bootstrap-complete
EOF
chmod 0700 /usr/local/sbin/app-ha-bootstrap-tailscale
cat >/etc/systemd/system/app-ha-bootstrap.service <<'EOF'
[Unit]
Description=app-ha first-boot Tailscale enrollment
After=network-online.target nss-lookup.target app-ha-host-guard.service
Wants=network-online.target nss-lookup.target
Requires=app-ha-host-guard.service
ConditionPathExists=!/var/lib/app-ha-bootstrap-complete
StartLimitIntervalSec=1800
StartLimitBurst=6
[Service]
Type=oneshot
ExecStart=/usr/local/sbin/app-ha-bootstrap-tailscale
RemainAfterExit=yes
Restart=on-failure
RestartSec=30s
[Install]
WantedBy=multi-user.target
EOF
systemctl daemon-reload
systemctl enable app-ha-bootstrap.service
systemctl start --no-block app-ha-bootstrap.service
'''
replacements = {
    "__PUBLIC_IP__": q(os.environ["PROXMOX_IP"]),
    "__PUBLIC_GATEWAY__": q(os.environ["PROXMOX_GATEWAY"]),
    "__TAILSCALE_HOSTNAME__": q(os.environ["TAILSCALE_HOSTNAME"]),
    "__AUTH_KEY_B64__": q(os.environ["AUTH_KEY_B64"]),
    "__SETUP_KEY_B64__": q(os.environ["SETUP_KEY_B64"]),
    "__HOST_KEY_B64__": q(os.environ["HOST_KEY_B64"]),
    "__HOST_KEY_PUB_B64__": q(os.environ["HOST_KEY_PUB_B64"]),
    "__ADMIN_1_KEY_B64__": q(__import__("base64").b64encode(os.environ["ADMIN_1_KEY"].encode()).decode()),
    "__ADMIN_2_KEY_B64__": q(__import__("base64").b64encode(os.environ["ADMIN_2_KEY"].encode()).decode()),
}
for old, new in replacements.items():
    first_boot = first_boot.replace(old, new)
Path(os.environ["FIRST_BOOT_OUTPUT"]).write_text(first_boot)
PY
  chmod 0600 "$answer" "$first_boot"

  "$assistant" validate-answer "$answer"
  rm -f "$output"
  "$assistant" prepare-iso "$PROXMOX_ISO_FILE_PATH" \
    --fetch-from iso --answer-file "$answer" --on-first-boot "$first_boot" --output "$output"
  chmod 0600 "$output"
  sha256sum -- "$output" | tee "${LOG_DIR}/prepared-iso.sha256"
  "$assistant" inspect-iso "$output" >/dev/null
  printf '%s inspect-iso succeeded for %s\n' "$(timestamp)" "$output" \
    >"${LOG_DIR}/prepared-iso-inspection.txt"
  # The ISO is now the only copy of the auth key, root password hash, and
  # private host key that it needs.
  shred_secret_file "$answer"
  shred_secret_file "$first_boot"
  shred_secret_file "$HOST_KEY"
  unset TAILSCALE_AUTH_KEY auth_key_b64 root_password_hash root_password_salt
  write_state prepared-iso "$output"
  write_state iso-config-sha256 "$HOST_SETUP_CONFIG_SHA256"
  write_state iso-built
}

host_key_fingerprint() {
  ssh-keygen -lf "$HOST_KEY_PUB" | awk '{print $2}'
}

# Once the host is installed its image is useless: the embedded Tailscale key
# is single-use. Remove the image and every workstation copy of its secrets.
scrub_installation_media() {
  local iso=""
  has_state prepared-iso && iso="$(read_state prepared-iso)"
  if [[ -n "$iso" && -e "$iso" ]]; then
    [[ "$iso" == "${HOST_ARTIFACTS}/"*.iso ]] ||
      fail "Recorded installation ISO is outside $HOST_ARTIFACTS: $iso"
    printf '\nREMOVING THE INSTALLATION ISO\n'
    printf 'The script is about to shred %s.\n' "$iso"
    if [[ "${HARDWARE_INVENTORY_MODE:-}" == idrac ]]; then
      printf 'If anything is still using it, such as an iDRAC/IPMI virtual media mapping for %s, detach it now.\n' "$HOST_ID"
    else
      printf 'If anything is still using it, such as a remote console virtual media mapping for %s, detach it now.\n' "$HOST_ID"
    fi
    printf 'Copies outside this workstation (for example, a USB stick) are not removed. Wipe them too: they still contain the host'"'"'s private SSH key and root password hash.\n'
    wait_for_exact "Confirm nothing is using the installation ISO."
    info "Shredding the spent installation ISO $iso"
    shred_secret_file "$iso"
  fi
  shred_secret_file "${GENERATED_DIR}/answer.toml"
  shred_secret_file "${GENERATED_DIR}/first-boot.sh"
  shred_secret_file "$HOST_KEY"
}

# The installer erases only the two mirror-0 disks. A disk that was a raw
# (unencrypted) member of an earlier rpool keeps its ZFS label, and the
# installer cannot rename that old pool once it has overwritten the pool's
# mirror-0 half, so the first boot finds two pools named rpool.
print_duplicate_rpool_help() {
  local boot_1="$NVME_MIRROR_0_SERIAL_1" boot_2="$NVME_MIRROR_0_SERIAL_2"
  local -a extra_serials=("${CONFIGURED_NVME_SERIALS[@]:2}")
  cat <<EOF

IF THE FIRST BOOT STOPS AT AN "(initramfs)" PROMPT
After the installer reboots, the target host console may show this instead of
a login prompt:

    Message: cannot import 'rpool': more than one matching pool
    Failed to import pool 'rpool'.
    Manually import the pool and exit.
    (initramfs)

Why: the installation itself succeeded. The installer erases only the two
installer disks ($boot_1 and $boot_2); every other disk
keeps whatever it held. If another disk was a member of an earlier unencrypted
rpool (for example an extra mirror from a previous install of this host), it
still carries a ZFS label for a pool named "rpool". The installer renames an
old rpool out of the way only if it can still import it, and it cannot: it has
just overwritten that old pool's installer-disk half. The boot then finds two
pools named rpool and refuses to guess which one to use. Disks that held LUKS
are not affected, because their ZFS labels are hidden inside LUKS.

Identify the stale disks. At the (initramfs) prompt run:

    zpool import

It lists two pools named rpool, each with its own id:
  - The NEW pool has state ONLINE, and its only members are partition 3 of the
    installer disks: names ending in ${boot_1}-part3 and
    ${boot_2}-part3. Leave it alone.
  - The STALE pool usually has state UNAVAIL. It also lists the installer
    disks, as FAULTED; never clear those, they now belong to the new pool.
    Every other device it lists (typically ONLINE, often ending in -part1) is
    a stale disk. Lines named indirect-N are not devices; skip them.
EOF
  if ((${#extra_serials[@]} > 0)); then
    printf '    The stale devices are most likely the extra configured disks: %s.\n' \
      "${extra_serials[*]}"
  fi
  cat <<'EOF'

Clear them. For each stale device, using its name exactly as zpool import
printed it (names without a leading /dev/ live under /dev/disk/by-id/):

    zpool labelclear -f /dev/disk/by-id/NAME

This erases only the old pool's ZFS labels on that device; nothing on the
installer disks is touched. Run zpool import again: exactly one rpool, state
ONLINE, on the two installer disks must remain. Then run:

    zpool import -N rpool
    exit

The boot continues to the normal login prompt and the first-boot setup runs.
Then continue with step 5. Later reboots are unaffected, because the stale
labels are gone. If more than one rpool still remains, do not import either;
stop and investigate before continuing.
EOF
}

installation_gate() {
  if has_state installed; then
    scrub_installation_media
    return
  fi
  local output operator_hostname
  output="$(read_state prepared-iso)"
  operator_hostname="${TAILSCALE_HOSTNAME:-$HOST_ID}"
  local -a steps=()
  if [[ "$HARDWARE_INVENTORY_MODE" == idrac ]]; then
    steps+=("Keep the Virtual Console open."
      "Mount the ISO above as Virtual CD/DVD."
      "Boot the server once from that virtual media.")
  else
    steps+=("Keep the target host console open."
      "Put the ISO above on bootable media supported by the target host."
      "Boot the target host once from that media.")
  fi
  steps+=("When prompted, select the default \"Install Proxmox VE (Automated)\" option and watch the unattended install through its reboot. If the first boot stops at an \"(initramfs)\" prompt instead of a login prompt, follow IF THE FIRST BOOT STOPS AT AN \"(initramfs)\" PROMPT above."
    "Remove or unmap the install media when the installed Proxmox login prompt appears."
    "Before reporting INSTALL COMPLETE, open the Tailscale admin machine list and verify $operator_hostname appeared with tag $TAILSCALE_TAG. If it is absent, the ISO Tailscale bootstrap failed; do not continue. A stale $operator_hostname left there by an earlier install may be deleted, but it is ignored either way: the script only accepts the one online, ${TAILSCALE_TAG}-tagged $operator_hostname that enrolled after this ISO was built.")
  # The same text is the dashboard request's context, so it shows in full there
  # instead of whatever fits in the recent output.
  local instructions_text
  instructions_text="$(print_installation_instructions "$output" "${steps[@]}")"
  printf '\n%s\n' "$instructions_text"
  local prompt="Confirm installation is finished, the login prompt is visible, and step ${#steps[@]} is complete."
  if bmac_ui_is_json; then
    local -a instructions=()
    local step
    for step in "${steps[@]}"; do
      instructions+=(--instruction "$step")
    done
    # shellcheck disable=SC2034 # Read by bmac_ui_manual_action.
    BMAC_UI_CONTEXT="$instructions_text"
    bmac_ui_manual_action --id install_complete --title "$prompt" \
      "${instructions[@]}" --ack-label "Install complete"
  else
    wait_for_exact "$prompt" "INSTALL COMPLETE" true
  fi
  write_state installed
  scrub_installation_media
}

# print_installation_instructions ISO STEP...
print_installation_instructions() {
  local output="$1"
  shift
  local -a steps=("$@")
  printf 'CUSTOM INSTALLER READY - INSTALL THE ISO ON THE HOST NOW\n'
  info "ISO: $output"
  info "Target: $HOST_ID / $PROXMOX_FQDN / $PROXMOX_IP"
  info "Installer mirror disks that will be destroyed: $NVME_MIRROR_0_SERIAL_1 and $NVME_MIRROR_0_SERIAL_2"
  if [[ "$HARDWARE_INVENTORY_MODE" == manual ]]; then
    info "Manually inventoried capacity of each installer disk: ${NVME_MIRROR_0_CAPACITY_BYTES_1} bytes"
    info "The automated installer selects these disks by serial; it cannot independently verify the recorded byte capacities before erasing them."
  fi
  if ((${#CONFIGURED_MIRROR_PAIRS[@]} > 1)); then
    info "Extra configured mirror disks are serial-selected and left untouched by the installer."
  fi
  info "Installer public NIC MAC selector: ${PROXMOX_PUBLIC_MAC^^}"
  info "Post-install private NIC MAC selector: ${PROXMOX_SECONDARY_MAC^^}"
  # Printed before the numbered steps so the steps stay on screen at the prompt.
  print_duplicate_rpool_help
  printf '\n!!!!!!!!!!!!!!!! DESTRUCTIVE ACTION !!!!!!!!!!!!!!!!\n'
  printf 'Following the installation steps below erases both listed NVMe disks and installs Proxmox on them as a raw ZFS mirror.\n'
  if [[ "$HARDWARE_INVENTORY_MODE" == idrac ]]; then
    info "iDRAC console: https://${IDRAC_IP}/restgui/start.html"
    printf '\nUsing the iDRAC/IPMI HTML5 console:\n'
  else
    printf '\nUsing the physical or remote console for the target host:\n'
  fi
  local index
  for index in "${!steps[@]}"; do
    printf '  %d. %s\n' "$((index + 1))" "${steps[index]}"
  done
  printf '\nNo SSH fingerprint check is needed. This ISO installs an SSH host key generated on this workstation, and every SSH connection this script makes to %s is pinned to it:\n' \
    "$HOST_ID"
  info "Ed25519 $(host_key_fingerprint)"
  printf 'After you confirm, the script asks you to detach the ISO, deletes it, and offers to add that pinned host to this workstation'"'"'s ~/.ssh/config.\n'
}

# Peer HostNames are self-reported, so a name match alone could select a stale
# node from an earlier install or any tailnet member using this name.
tailscale_peer_selection() {
  local since=$1
  jq -r --arg host "$TAILSCALE_HOSTNAME" --arg tag "$TAILSCALE_TAG" \
    --argjson since "$since" '
    def created_epoch:
      try (.Created | sub("\\.[0-9]+Z$"; "Z") | fromdateiso8601) catch -1;
    [(.Peer // {})[] |
     select(.HostName == $host or ((.DNSName // "") | startswith($host + "."))) |
     select(any((.Tags // [])[]; . == $tag)) |
     select(created_epoch >= $since)] as $candidates |
    if ($candidates | length) > 1 then
      "ambiguous \($candidates | length)"
    elif ($candidates | length) == 0 then
      "absent"
    elif ($candidates[0].Online != true) then
      "offline"
    else
      ([$candidates[0].TailscaleIPs[]? | select(test("^100\\."))][0] // null) as $ip |
      if $ip == null then "absent" else "ip \($ip)" end
    end
  '
}

resolve_tailscale_ip() {
  local since=0 built selection
  if has_state iso-built; then
    built="$(read_state iso-built)"
    [[ "$built" =~ ^[0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9]{2}:[0-9]{2}:[0-9]{2}Z$ ]] ||
      fail "Recorded ISO build time is malformed: $built"
    since="$(date -u -d "$built" +%s)"
  fi
  while true; do
    selection="$(tailscale status --json | tailscale_peer_selection "$since")" ||
      fail "Could not read this workstation's Tailscale peer list"
    case "$selection" in
      "ip "100.*)
        break
        ;;
      "ambiguous "*)
        fail "${selection#ambiguous } Tailscale peers named $TAILSCALE_HOSTNAME with tag $TAILSCALE_TAG enrolled after this ISO was built (${built:-unknown}); expected exactly one. Remove the unexpected machines in the Tailscale admin console before continuing."
        ;;
      offline)
        info "Waiting for $TAILSCALE_HOSTNAME to come online in Tailscale..."
        ;;
      *)
        info "Waiting for $TAILSCALE_HOSTNAME to enroll in Tailscale with tag $TAILSCALE_TAG (enrolled after ${built:-the ISO build})..."
        ;;
    esac
    sleep 10
  done
  TAILSCALE_IP="${selection#ip }"
  write_state tailscale-ip "$TAILSCALE_IP"
}

prepare_qdevice() {
  if [[ -z "$QD_IPV4" ]]; then
    cat <<EOF

The QDevice ${PROXMOX_QDEVICE_HOST} is not accessible: ${QD_ACCESS_PROBLEM}

This host can be set up without it only if the cluster has an odd number of
members afterward, which needs no QDevice. A QDevice that is still registered
is then removed forcefully, after you remove it from Tailscale. If the cluster
will need a QDevice, stop now and fix access to it, or replace it with
qdevice/remove_qdevice.sh, qdevice/QDEVICE_MANUAL_SETUP.md, and
qdevice/add_qdevice.sh. Setup stops before the join if it would need one.
EOF
    require_yes "Continue without an accessible QDevice?"
    return
  fi
  log "Provisioning and verifying the external QDevice service over Tailscale"
  qd_prepare_qnetd
  write_state qdevice-service-prepared "$QD_IPV4"
}

ssh_options() {
  if ((USE_ADMIN_SSH == 1)); then
    printf '%s\n' -o BatchMode=yes -o ClearAllForwardings=yes \
      -o StrictHostKeyChecking=yes -o CheckHostIP=yes \
      -o "ConnectTimeout=${MOX_SSH_CONNECT_TIMEOUT:-8}"
    return
  fi
  printf '%s\n' -i "$SSH_KEY" -o BatchMode=yes -o IdentitiesOnly=yes \
    -o StrictHostKeyChecking=yes -o "UserKnownHostsFile=$KNOWN_HOSTS" -o ConnectTimeout=15
  if [[ -s "$HOST_KEY_PUB" ]]; then
    printf '%s\n' -o "HostKeyAlias=$HOST_ID" -o HostKeyAlgorithms=ssh-ed25519 \
      -o GlobalKnownHostsFile=none -o UpdateHostKeys=no
  fi
}

pinned_host_key() {
  awk '{print $1 " " $2}' "$HOST_KEY_PUB"
}

remote_target() {
  if ((USE_ADMIN_SSH == 1)); then
    printf 'root@%s\n' "$HOST_ID"
  else
    printf 'root@%s\n' "$TAILSCALE_IP"
  fi
}

remote() {
  local -a options
  local command_string
  mapfile -t options < <(ssh_options)
  printf -v command_string '%q ' "$@"
  # shellcheck disable=SC2029 # command_string is assembled with Bash %q.
  ssh "${options[@]}" "$(remote_target)" "$command_string"
}

remote_script() {
  local -a options
  local args
  mapfile -t options < <(ssh_options)
  printf -v args '%q ' "$@"
  # shellcheck disable=SC2029 # args is assembled with Bash %q.
  ssh "${options[@]}" "$(remote_target)" "bash -s -- ${args}"
}

# remote_script with the apt/dpkg lock wait functions defined ahead of the
# payload read from stdin.
remote_apt_script() {
  { printf '%s\n' "$APT_LOCK_WAIT_FUNCTIONS"; cat; } | remote_script "$@"
}

copy_from_host() {
  local -a options
  mapfile -t options < <(ssh_options)
  scp "${options[@]}" "$(remote_target):$1" "$2"
}

copy_to_host() {
  local source=$1 destination=$2
  local -a options
  mapfile -t options < <(ssh_options)
  scp "${options[@]}" "$source" "$(remote_target):$destination"
}

enable_admin_ssh_for_target() {
  USE_ADMIN_SSH=1
  write_state admin-ssh-enabled
}

retire_setup_key_with_admin_identity() {
  local setup_key_b64 retirement_lock_fd=""
  if ((CLUSTER_CONTROL_LOCK_HELD == 0)); then
    exec {retirement_lock_fd}>"${ARTIFACTS_DIR}/cluster-control-plane.lock"
    chmod 0600 "${ARTIFACTS_DIR}/cluster-control-plane.lock"
    flock -w 1800 "$retirement_lock_fd" ||
      fail "Timed out waiting to retire the temporary setup key safely"
  fi
  setup_key_b64="$(base64 -w0 <"$SSH_KEY.pub")"
  ssh -o BatchMode=yes -o ClearAllForwardings=yes \
    -o StrictHostKeyChecking=yes -o CheckHostIP=yes \
    -o "ConnectTimeout=${MOX_SSH_CONNECT_TIMEOUT:-8}" \
    "root@${HOST_ID}" "SETUP_KEY_B64='$setup_key_b64' bash -s" <<'REMOTE'
set -Eeuo pipefail
key="$(printf '%s' "$SETUP_KEY_B64" | base64 -d)"
work="$(mktemp /root/.ssh/authorized_keys.app-ha.XXXXXX)"
awk -v key="$key" '$0 != key { print }' /root/.ssh/authorized_keys >"$work"
cat "$work" >/root/.ssh/authorized_keys
chmod 0600 /root/.ssh/authorized_keys
rm -f "$work"
REMOTE
  write_state setup-key-retired
  if [[ -n "$retirement_lock_fd" ]]; then
    flock -u "$retirement_lock_fd"
    exec {retirement_lock_fd}>&-
  fi
}

fail_host_key_mismatch() {
  fail "$1 did not present the SSH host key embedded in the $HOST_ID installation ISO (Ed25519 $(host_key_fingerprint)). It may be a different machine or a man-in-the-middle, or the first-boot payload did not install the key. Do not continue; inspect the host at its console."
}

wait_for_pinned_setup_ssh() {
  local -a options
  local output
  printf '%s %s\n' "$HOST_ID" "$(pinned_host_key)" >"$KNOWN_HOSTS"
  chmod 0600 "$KNOWN_HOSTS"
  mapfile -t options < <(ssh_options)
  until output="$(ssh "${options[@]}" "root@${TAILSCALE_IP}" true 2>&1 </dev/null)"; do
    if grep -Eq 'Host key verification failed|REMOTE HOST IDENTIFICATION HAS CHANGED' <<<"$output"; then
      fail_host_key_mismatch "Tailscale peer $TAILSCALE_IP"
    fi
    info "Waiting for pinned setup-key SSH access to $HOST_ID ($TAILSCALE_IP)..."
    sleep 10
  done
}

wait_for_host() {
  resolve_tailscale_ip
  local -a options
  if ((USE_ADMIN_SSH == 1)); then
    mapfile -t options < <(ssh_options)
    until ssh "${options[@]}" "$(remote_target)" true 2>/dev/null; do
      info "Waiting for operator-admin SSH access to $HOST_ID..."
      sleep 10
    done
    return
  fi
  if ssh -o BatchMode=yes -o ClearAllForwardings=yes \
       -o StrictHostKeyChecking=yes -o CheckHostIP=yes \
       -o "ConnectTimeout=${MOX_SSH_CONNECT_TIMEOUT:-8}" \
       "root@${HOST_ID}" pvecm status 2>/dev/null |
       grep -Eq "^Name:[[:space:]]+${PROXMOX_CLUSTER_NAME}[[:space:]]*$"
  then
    enable_admin_ssh_for_target
    retire_setup_key_with_admin_identity
    return
  fi
  if [[ -s "$HOST_KEY_PUB" ]]; then
    wait_for_pinned_setup_ssh
    return
  fi
  # Hosts installed from images without an embedded host key fall back to
  # trust on first use over the authenticated Tailscale path.
  rm -f "$KNOWN_HOSTS"
  until ssh-keyscan -T 10 -H "$TAILSCALE_IP" >"$KNOWN_HOSTS" 2>/dev/null; do
    info "Waiting for SSH on $HOST_ID ($TAILSCALE_IP)..."
    sleep 10
  done
  chmod 0600 "$KNOWN_HOSTS"
  mapfile -t options < <(ssh_options)
  for _ in 1 2 3; do
    if ssh "${options[@]}" "root@${TAILSCALE_IP}" true 2>/dev/null; then
      break
    fi
    sleep 3
  done
  until ssh "${options[@]}" "root@${TAILSCALE_IP}" true 2>/dev/null; do
    info "Waiting for setup-key SSH access..."
    sleep 10
  done
}

workstation_ssh_settings_match() {
  local name=$1 effective
  effective="$(ssh -G "$name" 2>/dev/null)" || return 1
  grep -Fxq "hostname $TAILSCALE_IP" <<<"$effective" &&
    grep -Fxq "user root" <<<"$effective" &&
    grep -Fxq "port 22" <<<"$effective" &&
    grep -Fxq "hostkeyalias $HOST_ID" <<<"$effective" &&
    grep -Fxq "hostkeyalgorithms ssh-ed25519" <<<"$effective" &&
    grep -Fxq "stricthostkeychecking true" <<<"$effective" &&
    grep -Fxq "userknownhostsfile ${HOME}/.ssh/known_hosts" <<<"$effective" &&
    ! grep -Eq '^(proxyjump|proxycommand) ' <<<"$effective"
}

print_manual_workstation_ssh_instructions() {
  local name=$1
  printf '\nTo set up "ssh %s" yourself without trusting a first-use prompt, put this entry at the top of ~/.ssh/config:\n\n' "$name"
  printf '    Host %s\n' "$name"
  printf '        HostName %s\n' "$TAILSCALE_IP"
  printf '        User root\n'
  printf '        HostKeyAlias %s\n' "$HOST_ID"
  printf '        HostKeyAlgorithms ssh-ed25519\n'
  printf '        StrictHostKeyChecking yes\n'
  printf '    Host *\n'
  printf '\nand add this line to ~/.ssh/known_hosts:\n\n'
  printf '    %s %s\n' "$HOST_ID" "$(pinned_host_key)"
  printf '\nLater phases of this script connect with ssh root@%s and need that name to work.\n' "$HOST_ID"
}

# Writes the managed block and known_hosts pin, then proves them. When the
# workstation cannot log in, restores the previous files and sets
# WORKSTATION_SSH_FAILURE instead of returning nonzero, so callers keep errexit.
write_workstation_ssh_config() {
  local -a names=("$@")
  WORKSTATION_SSH_FAILURE=""
  local ssh_dir="${HOME}/.ssh"
  local ssh_config="${ssh_dir}/config" known_hosts="${ssh_dir}/known_hosts"
  local path backup_dir name output="" result=retry attempt lock_fd
  local config_existed=false known_existed=false
  [[ "$HOME" == /* && "$HOME" != *[\"$'\n']* ]] ||
    fail "HOME must be an absolute path without quotes or newlines to manage ~/.ssh/config"
  for path in "$ssh_dir" "$ssh_config" "$known_hosts"; do
    [[ ! -L "$path" ]] || fail "Refusing to manage $path because it is a symlink"
  done
  # Concurrent host installers share this workstation's SSH files.
  exec {lock_fd}>"${ARTIFACTS_DIR}/workstation-ssh-config.lock"
  chmod 0600 "${ARTIFACTS_DIR}/workstation-ssh-config.lock"
  flock -w 900 "$lock_fd" ||
    fail "Timed out waiting for another host installer to finish editing ~/.ssh/config"
  install -d -m 0700 "$ssh_dir"
  backup_dir="$(mktemp -d "${STATE_DIR}/workstation-ssh.XXXXXX")"
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
  for name in "${names[@]}" "$HOST_ID"; do
    ssh-keygen -R "$name" -f "$staged_known_hosts" >/dev/null 2>&1 || true
  done
  rm -f -- "${staged_known_hosts}.old"
  printf '%s %s\n' "$HOST_ID" "$(pinned_host_key)" >>"$staged_known_hosts"

  local staged_config="${backup_dir}/config.new"
  python3 - "$ssh_config" "$staged_config" "$HOST_ID" "$TAILSCALE_IP" \
    "$known_hosts" "${names[@]}" <<'PY'
from pathlib import Path
import sys

source, destination, host_id, address, known_hosts, *names = sys.argv[1:]
begin = f"# BEGIN app-ha managed proxmox host {host_id}"
end = f"# END app-ha managed proxmox host {host_id}"
text = ""
path = Path(source)
if path.is_file():
    text = path.read_text(encoding="utf-8")
kept = []
inside = False
for line in text.splitlines():
    if not inside and line == begin:
        inside = True
    elif inside and line == end:
        inside = False
    elif not inside:
        kept.append(line)
if inside:
    raise SystemExit("existing managed SSH block for " + host_id + " is unterminated")
block = [
    begin,
    "Host " + " ".join(names),
    f"    HostName {address}",
    "    User root",
    "    Port 22",
    "    ProxyJump none",
    f"    HostKeyAlias {host_id}",
    "    HostKeyAlgorithms ssh-ed25519",
    "    StrictHostKeyChecking yes",
    "    UpdateHostKeys no",
    f'    UserKnownHostsFile "{known_hosts}"',
    "    PasswordAuthentication no",
    "    KbdInteractiveAuthentication no",
    # Keeps any top-level options that follow in the user's file global.
    "Host *",
    end,
    "",
]
Path(destination).write_text("\n".join(block + kept).rstrip() + "\n", encoding="utf-8")
PY
  chmod 0600 "$staged_config"

  install -m 0600 "$staged_known_hosts" "$known_hosts"
  # Prepending the managed block lets first-value-wins OpenSSH semantics
  # override any older stanza for these names without deleting user config.
  install -m 0600 "$staged_config" "$ssh_config"

  for name in "${names[@]}"; do
    workstation_ssh_settings_match "$name" || {
      result="ssh -G $name does not resolve to the managed settings (a Match block or Include may be overriding them)"
      break
    }
  done
  if [[ "$result" == retry ]]; then
    for attempt in $(seq 1 30); do
      if output="$(ssh -o BatchMode=yes -o ConnectTimeout=15 \
          -o ControlMaster=no -o ControlPath=none "${names[0]}" true 2>&1 </dev/null)"; then
        result=ok
        break
      fi
      if grep -Eq 'Host key verification failed|REMOTE HOST IDENTIFICATION HAS CHANGED' <<<"$output"; then
        result=mismatch
        break
      fi
      if grep -Fq 'Permission denied' <<<"$output"; then
        result="this workstation's SSH key is not authorized for root on $HOST_ID (it must be ADMIN_1_PUBLIC_SSH_KEY or ADMIN_2_PUBLIC_SSH_KEY)"
        break
      fi
      info "Waiting for SSH on $HOST_ID ($TAILSCALE_IP), attempt $attempt/30..."
      sleep 10
    done
    [[ "$result" != retry ]] ||
      result="ssh ${names[0]} did not connect within 5 minutes: ${output##*$'\n'}"
  fi

  if [[ "$result" != ok ]]; then
    printf 'Restoring the previous ~/.ssh/config and ~/.ssh/known_hosts.\n' >&2
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
    rm -rf -- "$backup_dir"
    [[ "$result" != mismatch ]] || fail_host_key_mismatch "ssh ${names[0]} ($TAILSCALE_IP)"
    WORKSTATION_SSH_FAILURE="$result"
  else
    rm -rf -- "$backup_dir"
  fi
  exec {lock_fd}>&-
}

configure_workstation_ssh() {
  has_state workstation-ssh-config && return
  [[ -s "$HOST_KEY_PUB" ]] || return 0
  local alias=""
  local -a names=()
  printf '\nWORKSTATION SSH ACCESS\n'
  printf 'The script can add a Host entry for %s to the top of this workstation'"'"'s ~/.ssh/config. It connects over Tailscale and accepts only the host key embedded in the installation ISO (Ed25519 %s), with the pin stored in ~/.ssh/known_hosts, so no fingerprint needs to be checked by hand.\n' \
    "$HOST_ID" "$(host_key_fingerprint)"
  if [[ "$HARDWARE_INVENTORY_MODE" == idrac ]]; then
    printf 'The explicit entry also keeps SSH from reaching the iDRAC/IPMI-side address, which uses the same hostname.\n'
  fi
  if ! prompt_yes_default_yes "Add $HOST_ID to this workstation's SSH config?"; then
    resolve_tailscale_ip
    print_manual_workstation_ssh_instructions "$HOST_ID"
    write_state workstation-ssh-config declined
    return
  fi
  while true; do
    if bmac_ui_is_json; then
      bmac_ui_input alias --id ssh_alias --label "SSH name for $HOST_ID (ssh NAME)" \
        --default "$HOST_ID" --required --pattern '^[A-Za-z0-9][A-Za-z0-9_.-]{0,63}$'
    else
      read -r -p "SSH name to use for $HOST_ID (ssh NAME) [${HOST_ID}]: " alias || alias=""
    fi
    alias="${alias:-$HOST_ID}"
    [[ "$alias" =~ ^[A-Za-z0-9][A-Za-z0-9_.-]{0,63}$ ]] && break
    printf 'The SSH name must use 1-64 letters, digits, dots, underscores, or hyphens.\n' >&2
  done
  names=("$alias")
  if [[ "$alias" != "$HOST_ID" ]]; then
    printf 'This script and the other cluster tools connect with ssh root@%s, so that name needs the same settings.\n' "$HOST_ID"
    if prompt_yes_default_yes "Also apply these settings to the name $HOST_ID?"; then
      names+=("$HOST_ID")
    else
      printf 'WARNING: Later phases of this script will wait until ssh root@%s works from this workstation.\n' "$HOST_ID" >&2
    fi
  fi
  resolve_tailscale_ip
  write_workstation_ssh_config "${names[@]}"
  if [[ -n "$WORKSTATION_SSH_FAILURE" ]]; then
    printf '\nWARNING: Workstation SSH for %s was not configured because %s.\n' \
      "$HOST_ID" "$WORKSTATION_SSH_FAILURE" >&2
    print_manual_workstation_ssh_instructions "$alias"
    printf 'This prompt is offered again when the script is rerun.\n'
    return
  fi
  write_state workstation-ssh-config "${names[*]}"
  info "Configured and verified pinned workstation SSH: ssh $alias"
}

bootstrap_host() {
  if has_state host-bootstrapped; then
    wait_for_host
    remote_script "$ADMIN_1_PUBLIC_SSH_KEY" "$ADMIN_2_PUBLIC_SSH_KEY" <<'REMOTE'
set -Eeuo pipefail
install -d -m 0700 /root/.ssh
touch /root/.ssh/authorized_keys
chmod 0600 /root/.ssh/authorized_keys
for key in "$1" "$2"; do
  grep -Fxq "$key" /root/.ssh/authorized_keys ||
    printf '%s\n' "$key" >>/root/.ssh/authorized_keys
done
REMOTE
  else
    wait_for_host
    remote_apt_script "$PROXMOX_FQDN" "$ADMIN_1_PUBLIC_SSH_KEY" \
      "$ADMIN_2_PUBLIC_SSH_KEY" "$NVME_MIRROR_0_SERIAL_1" \
      "$NVME_MIRROR_0_SERIAL_2" <<'REMOTE'
set -Eeuo pipefail
expected_fqdn="$1"
[[ "$(hostname -f)" == "$expected_fqdn" ]]
install -d -m 0700 /root/.ssh
touch /root/.ssh/authorized_keys
chmod 0600 /root/.ssh/authorized_keys
for key in "$2" "$3"; do
  grep -Fxq "$key" /root/.ssh/authorized_keys || printf '%s\n' "$key" >>/root/.ssh/authorized_keys
done
[[ -f /var/lib/app-ha-bootstrap-complete ]]
zpool status -x rpool | grep -F "pool 'rpool' is healthy"
[[ -z "$(swapon --noheadings --show=NAME)" ]]
pool="$(zpool status -P rpool)"
for serial in "$4" "$5"; do
  disk="$(app-ha-disk-by-serial "$serial")"
  expected_partition="$(readlink -f "${disk}p3")"
  found=false
  while IFS= read -r member; do
    [[ "$(readlink -f "$member")" != "$expected_partition" ]] || found=true
  done < <(awk '$1 ~ /^\// { print $1 }' <<<"$pool")
  [[ "$found" == true ]] ||
    {
      printf 'Installer rpool does not contain configured mirror-0 disk %s\n' "$serial" >&2
      exit 1
    }
done

# Disable subscription-only repositories before general apt operations.
for source in /etc/apt/sources.list.d/pve-enterprise.sources /etc/apt/sources.list.d/ceph.sources; do
  [[ ! -f "$source" ]] || mv "$source" "${source}.disabled"
done
cat >/etc/apt/sources.list.d/pve-no-subscription.sources <<'EOF'
Types: deb
URIs: http://download.proxmox.com/debian/pve
Suites: trixie
Components: pve-no-subscription
Signed-By: /usr/share/keyrings/proxmox-archive-keyring.gpg
EOF
apt_wait_for_locks
apt-get -o DPkg::Lock::Timeout=60 update
DEBIAN_FRONTEND=noninteractive apt-get -o DPkg::Lock::Timeout=60 install -y cryptsetup cryptsetup-initramfs \
  keyutils gdisk jq corosync-qdevice psmisc
systemctl enable --now chrony.service 2>/dev/null || true
REMOTE

    write_state host-bootstrapped
  fi

  # The Proxmox first-boot unit retries its entire ISO payload on every boot
  # until this pending marker is removed. Clear it only after Tailscale
  # enrollment and host bootstrap have both been verified. The retained
  # payload copy embeds the spent Tailscale key and the host private key.
  remote_script <<'REMOTE'
set -Eeuo pipefail
[[ -f /var/lib/app-ha-bootstrap-complete ]]
rm -f /var/lib/proxmox-first-boot/pending-first-boot-setup
systemctl reset-failed proxmox-first-boot-network-pre.service 2>/dev/null || true
if [[ -d /var/lib/proxmox-first-boot ]]; then
  while IFS= read -r -d '' payload; do
    [[ -f "$payload" && ! -L "$payload" ]] || continue
    shred -n 1 -u -- "$payload"
    printf 'Removed spent first-boot payload %s\n' "$payload"
  done < <(grep -rlZF -- TAILSCALE_AUTH_KEY_B64= /var/lib/proxmox-first-boot || true)
fi
REMOTE

  # Reconcile this helper on every resume so canonical device matching does not
  # depend on ZFS displaying a serial-bearing by-id path.
  remote_script <<'REMOTE'
set -Eeuo pipefail
cat >/usr/local/sbin/app-ha-rpool-member-for-serial <<'EOF'
#!/usr/bin/env bash
set -Eeuo pipefail
serial="${1:?serial is required}"
disk="$(app-ha-disk-by-serial "$serial")"
expected="$(readlink -f "${disk}p3")"
mapfile -t matches < <(
  while IFS= read -r candidate; do
    [[ "$(readlink -f "$candidate")" == "$expected" ]] &&
      printf '%s\n' "$candidate"
  done < <(zpool status -P rpool | awk '$1 ~ /^\/dev\// { print $1 }')
)
((${#matches[@]} == 1)) || {
  printf 'Expected exactly one canonical rpool member for serial %s; found %s\n' \
    "$serial" "${#matches[@]}" >&2
  exit 1
}
printf '%s\n' "${matches[0]}"
EOF
chmod 0755 /usr/local/sbin/app-ha-rpool-member-for-serial
REMOTE
}

stage_luks_password_file() {
  local -a options
  local command_string
  mapfile -t options < <(ssh_options)
  # shellcheck disable=SC2016 # This program expands only on the target host.
  printf -v command_string '%q ' bash -c '
    set -Eeuo pipefail
    umask 077
    destination="$1"
    [[ "$(findmnt -n -o FSTYPE /)" == zfs ]]
    [[ ! -e "$destination" || (-f "$destination" && ! -L "$destination") ]]
    temporary="$(mktemp /root/.app-ha-luks-passphrase.XXXXXX)"
    trap '\''rm -f "$temporary"'\'' EXIT
    cat >"$temporary"
    [[ -s "$temporary" ]]
    chmod 0600 "$temporary"
    mv -f "$temporary" "$destination"
    trap - EXIT
  ' app-ha-stage-luks-password "$LUKS_SECRET_FILE"
  # shellcheck disable=SC2029 # command_string is assembled with Bash %q.
  printf '%s' "$PROXMOX_LUKS_PASSWORD" |
    ssh "${options[@]}" "$(remote_target)" "$command_string"
  unset PROXMOX_LUKS_PASSWORD
  remote test -s "$LUKS_SECRET_FILE"
  remote test ! -L "$LUKS_SECRET_FILE"
  write_state luks-password-file-staged
}

verify_luks_password_file() {
  local mode=$1
  remote_script "$mode" "$LUKS_SECRET_FILE" \
    "${EXPECTED_RPOOL_MAPPERS[@]}" <<'REMOTE'
set -Eeuo pipefail
mode="$1"; key_file="$2"; shift 2
[[ "$mode" == existing || "$mode" == all ]]
[[ -s "$key_file" && ! -L "$key_file" ]]
[[ "$(stat -c '%U:%G:%a' "$key_file")" == root:root:600 ]]
verified=0
for mapper in "$@"; do
  if [[ ! -b "/dev/mapper/$mapper" ]]; then
    [[ "$mode" == existing ]] && continue
    printf 'Expected LUKS mapper is unavailable for final key verification: %s\n' \
      "$mapper" >&2
    exit 1
  fi
  backing="$(cryptsetup status "$mapper" | awk '$1 == "device:" { print $2; exit }')"
  [[ -b "$backing" ]]
  cryptsetup open --test-passphrase --key-file="$key_file" "$backing"
  verified=$((verified + 1))
done
if [[ "$mode" == all ]]; then
  ((verified == $#))
fi
printf 'Temporary LUKS key file verified against %s existing mapper(s).\n' "$verified"
REMOTE
}

remove_luks_password_file() {
  remote rm -f "$LUKS_SECRET_FILE"
  remote sync
  remote test ! -e "$LUKS_SECRET_FILE"
  write_state luks-password-file-removed
}

assert_pool_healthy() {
  remote zpool status -x rpool | grep -F "pool 'rpool' is healthy" >/dev/null ||
    fail "rpool is not healthy"
}

reboot_and_wait() {
  local message=$1 active=$2 before after
  before="$(read_state "${active}-boot-id")"
  printf '\n%s\n' "$message"
  wait_for_exact "Confirm the Proxmox login prompt is visible at the target host console." "BOOTED"
  TAILSCALE_IP=""
  wait_for_host
  after="$(remote cat /proc/sys/kernel/random/boot_id)"
  [[ "$before" != "$after" ]] ||
    fail "The host is reachable, but its kernel boot ID did not change; the requested reboot was not proven"
}

raw_boot_test() {
  local survivor=$1 disabled_serial disabled_member state active
  local schedule_reboot=false
  state="raw-${survivor}-only-passed"
  if [[ "$survivor" == A ]]; then
    disabled_serial="$NVME_MIRROR_0_SERIAL_2"
    disabled_member=B
  else
    disabled_serial="$NVME_MIRROR_0_SERIAL_1"
    disabled_member=A
  fi
  active="raw-${survivor}-test-active"
  if has_state "$state"; then
    rm -f "$(state_path "$active")" "$(state_path "${active}-boot-id")"
    return
  fi
  if ! has_state "$active"; then
    assert_pool_healthy
    confirm_exact \
      "With the target host console open, disable member ${disabled_member}'s ESP, offline it from rpool, and reboot to prove raw member ${survivor} boots alone." \
      "TEST RAW MEMBER ${survivor} BOOT"
    write_state "${active}-boot-id" "$(remote cat /proc/sys/kernel/random/boot_id)"
    write_state "$active" preparing
    schedule_reboot=true
  elif [[ "$(remote cat /proc/sys/kernel/random/boot_id)" == "$(read_state "${active}-boot-id")" ]]; then
    info "The raw member $survivor test is armed but has not rebooted; reconciling its reversible failure simulation and scheduling the reboot again."
    schedule_reboot=true
  fi
  if [[ "$schedule_reboot" == true ]]; then
    remote_script "$disabled_serial" "$survivor" <<'REMOTE'
set -Eeuo pipefail
serial="$1"
survivor="$2"
disk="$(app-ha-disk-by-serial "$serial")"
part="$(zpool status -P rpool | awk -v serial="$serial" '$1 ~ serial { print $1; exit }')"
[[ -b "$part" ]]
mountpoint="/mnt/app-ha-esp-test-$serial"
mkdir -p "$mountpoint"
mountpoint -q "$mountpoint" || mount "${disk}p2" "$mountpoint"
if [[ -d "$mountpoint/EFI" && ! -e "$mountpoint/EFI.APP-HA-DISABLED" ]]; then
  mv "$mountpoint/EFI" "$mountpoint/EFI.APP-HA-DISABLED"
fi
[[ ! -e "$mountpoint/EFI" && -d "$mountpoint/EFI.APP-HA-DISABLED" ]]
sync
umount "$mountpoint"
zpool status -P rpool |
  awk -v member="$part" '$1 == member && $2 == "OFFLINE" { found=1 } END { exit !found }' ||
  zpool offline rpool "$part"
systemd-run --unit="app-ha-raw-${survivor}-reboot-$(date +%s)" \
  --on-active=1s --timer-property=AccuracySec=100ms \
  --collect /usr/bin/systemctl reboot
REMOTE
    write_state "$active" scheduled
  fi
  reboot_and_wait "No LUKS password is required for this unencrypted mirror-member boot test." "$active"
  remote_script "$disabled_serial" <<'REMOTE'
set -Eeuo pipefail
serial="$1"
disk="$(app-ha-disk-by-serial "$serial")"
part="$(zpool status -P rpool | awk -v serial="$serial" '$1 ~ serial { print $1; exit }')"
[[ -b "$part" ]]
mountpoint="/mnt/app-ha-esp-test-$serial"
mkdir -p "$mountpoint"
mountpoint -q "$mountpoint" || mount "${disk}p2" "$mountpoint"
if [[ ! -e "$mountpoint/EFI" && -d "$mountpoint/EFI.APP-HA-DISABLED" ]]; then
  mv "$mountpoint/EFI.APP-HA-DISABLED" "$mountpoint/EFI"
fi
[[ -d "$mountpoint/EFI" && ! -e "$mountpoint/EFI.APP-HA-DISABLED" ]]
sync
umount "$mountpoint"
zpool status -P rpool |
  awk -v member="$part" '$1 == member && $2 == "ONLINE" { found=1 } END { exit !found }' ||
  zpool online rpool "$part"
while zpool status rpool | grep -q 'resilver in progress'; do sleep 15; done
zpool status -x rpool | grep -F "pool 'rpool' is healthy"
proxmox-boot-tool refresh
REMOTE
  write_state "$state"
  rm -f "$(state_path "$active")" "$(state_path "${active}-boot-id")"
}

rebuild_luks_member() {
  local member=$1 serial mapper survivor state prepared_state helper
  local phase_file backup remote_header local_header probe
  local raw_present=false mapper_in_pool=false luks_present=false phase_present=false
  local partner_serial
  if [[ "$member" == A ]]; then
    serial="$NVME_MIRROR_0_SERIAL_1"; mapper=crypt-rpool-a; survivor=B
    partner_serial="$NVME_MIRROR_0_SERIAL_2"
  else
    serial="$NVME_MIRROR_0_SERIAL_2"; mapper=crypt-rpool-b; survivor=A
    partner_serial="$NVME_MIRROR_0_SERIAL_1"
  fi
  state="luks-${member}-attached"
  prepared_state="luks-${member}-prepared"
  helper="/root/app-ha-luks-format-${member}"
  phase_file="/root/app-ha-luks-${member}.phase"
  backup="/root/app-ha-${member}-pre-luks.gpt"
  remote_header="/root/luks-header-${member}.bin"
  local_header="${HEADER_DIR}/rpool-${member}.bin"

  if has_state "$state" && [[ -s "$local_header" ]] &&
     remote_script "$serial" "$mapper" <<'REMOTE' >/dev/null 2>&1
set -Eeuo pipefail
serial="$1"; mapper="$2"
disk="$(app-ha-disk-by-serial "$serial")"
pool="$(zpool status -P rpool)"
awk -v member="/dev/mapper/$mapper" '$1 == member { found++ } END { exit !(found == 1) }' <<<"$pool"
backing="$(cryptsetup status "$mapper" | awk '$1 == "device:" { print $2; exit }')"
[[ "$(readlink -f "$backing")" == "$(readlink -f "${disk}p3")" ]]
uuid="$(cryptsetup luksUUID "${disk}p3")"
awk -v mapper="$mapper" -v source="UUID=$uuid" '
  $1 == mapper && $2 == source { found++ }
  END { exit !(found == 1) }
' /etc/crypttab
zpool status -x rpool | grep -F "pool 'rpool' is healthy"
REMOTE
  then
    return
  fi

  probe="$(remote_script "$serial" "$mapper" "$phase_file" "$backup" <<'REMOTE'
set -Eeuo pipefail
serial="$1"; mapper="$2"; phase_file="$3"; backup="$4"
disk="$(app-ha-disk-by-serial "$serial")"
pool="$(zpool status -P rpool)"
if app-ha-rpool-member-for-serial "$serial" >/dev/null 2>&1; then echo raw=yes; else echo raw=no; fi
if awk -v member="/dev/mapper/$mapper" '$1 == member { found=1 } END { exit !found }' <<<"$pool"; then
  echo mapper_in_pool=yes
else
  echo mapper_in_pool=no
fi
if cryptsetup isLuks "${disk}p3"; then echo luks=yes; else echo luks=no; fi
if [[ -s "$phase_file" ]]; then
  [[ "$(sed -n '1p' "$phase_file")" == "$serial" ]]
  echo phase=yes
else
  echo phase=no
fi
if [[ -s "$backup" ]]; then echo backup=yes; else echo backup=no; fi
REMOTE
  )"
  grep -Fxq raw=yes <<<"$probe" && raw_present=true
  grep -Fxq mapper_in_pool=yes <<<"$probe" && mapper_in_pool=true
  grep -Fxq luks=yes <<<"$probe" && luks_present=true
  grep -Fxq phase=yes <<<"$probe" && phase_present=true

  if [[ "$mapper_in_pool" == false && "$luks_present" == false ]]; then
    if [[ "$raw_present" == true ]]; then
      assert_pool_healthy
      confirm_exact \
        "Detach rpool member $member ($serial), expand only partition 3 to 1 GiB short of the smaller boot disk's whole-GiB size, and remove its obsolete ZFS signatures." \
        "PREPARE MEMBER ${member} FOR LUKS"
    elif [[ "$phase_present" == true || "$probe" == *$'backup=yes'* ]]; then
      confirm_exact \
        "Resume the interrupted, already-detached member $member conversion using its recorded GPT backup and partition geometry." \
        "RESUME MEMBER ${member} LUKS PREPARATION"
      info "Recovered an interrupted member $member preparation from durable host state."
    else
      fail "Member $member is neither a canonical raw rpool member nor LUKS, and has no trusted preparation record"
    fi

    remote_script "$serial" "$phase_file" "$backup" "$partner_serial" <<'REMOTE'
set -Eeuo pipefail
serial="$1"; phase_file="$2"; backup="$3"; partner_serial="$4"
disk="$(app-ha-disk-by-serial "$serial")"
partner="$(app-ha-disk-by-serial "$partner_serial")"
if [[ -s "$phase_file" ]]; then
  [[ "$(sed -n '1p' "$phase_file")" == "$serial" ]]
  start="$(sed -n '2p' "$phase_file")"
else
  [[ -b "${disk}p3" ]]
  start="$(sgdisk -i 3 "$disk" | awk '/First sector/ { print $3; exit }')"
  [[ "$start" =~ ^[0-9]+$ ]]
  printf '%s\n%s\n' "$serial" "$start" >"$phase_file"
  chmod 0600 "$phase_file"
fi
[[ "$start" =~ ^[0-9]+$ ]]

if part="$(app-ha-rpool-member-for-serial "$serial" 2>/dev/null)"; then
  [[ -s "$backup" ]] || sgdisk --backup="$backup" "$disk"
  chmod 0600 "$backup"
  zpool detach rpool "$part"
else
  [[ -s "$backup" ]] || {
    printf 'Detached member %s has no pre-resize GPT backup\n' "$serial" >&2
    exit 1
  }
fi

last_usable="$(
  sgdisk -p "$disk" |
    awk '/First usable sector is / { value=$10; gsub(/[^0-9]/, "", value); print value; exit }'
)"
[[ "$last_usable" =~ ^[0-9]+$ && "$start" -lt "$last_usable" ]]
logical_sector_size="$(blockdev --getss "$disk")"
[[ "$logical_sector_size" =~ ^[0-9]+$ ]]
[[ "$(blockdev --getss "$partner")" == "$logical_sector_size" ]]
# Both boot disks end partition 3 at the same byte: 1 GiB short of the smaller
# disk's whole-GiB size, whichever disk is smaller, so a slightly smaller
# replacement still holds the layout.
disk_bytes="$(blockdev --getsize64 "$disk")"
partner_bytes="$(blockdev --getsize64 "$partner")"
[[ "$disk_bytes" =~ ^[0-9]+$ && "$partner_bytes" =~ ^[0-9]+$ ]]
smaller_bytes=$((disk_bytes < partner_bytes ? disk_bytes : partner_bytes))
gib=1073741824
reference_end=$(((smaller_bytes / gib * gib - gib) / logical_sector_size - 1))
((start < reference_end && reference_end <= last_usable))
current_end=""
current_start="$(sgdisk -i 3 "$disk" 2>/dev/null | awk '/First sector/ { print $3; exit }' || true)"
if [[ -n "$current_start" ]]; then
  current_end="$(sgdisk -i 3 "$disk" | awk '/Last sector/ { print $3; exit }')"
  [[ "$current_start" == "$start" ]]
  ((current_end <= reference_end)) || {
    printf 'Partition 3 of %s already ends past sector %s\n' "$serial" "$reference_end" >&2
    exit 1
  }
fi
if [[ "$current_end" != "$reference_end" ]]; then
  if [[ -n "$current_start" ]]; then
    sgdisk --delete=3 --new=3:"$start":"$reference_end" --typecode=3:BF01 "$disk"
  else
    sgdisk --new=3:"$start":"$reference_end" --typecode=3:BF01 "$disk"
  fi
fi
partx --update --nr 3 "$disk" || true
udevadm settle
[[ -b "${disk}p3" ]]
end="$(sgdisk -i 3 "$disk" | awk '/Last sector/ {print $3}')"
[[ "$end" == "$reference_end" ]]
expected_bytes=$(((end - start + 1) * logical_sector_size))
[[ "$(blockdev --getsize64 "${disk}p3")" == "$expected_bytes" ]]
wipefs --types zfs_member --all "${disk}p3"
! wipefs -n "${disk}p3" | grep -q zfs_member
REMOTE
    copy_from_host "$backup" "${HEADER_DIR}/member-${member}-pre-resize.gpt"
    chmod 0600 "${HEADER_DIR}/member-${member}-pre-resize.gpt"
    write_state "$prepared_state"
  fi

  # lib/rpool_mirror.sh holds the one copy of the member LUKS logic, shared
  # with hosts/add_replacement_disk.sh.
  install_rpool_mirror_tool
  if [[ "$mapper_in_pool" == false ]]; then
    remote_script "$helper" "$RPOOL_MIRROR_TOOL" "$member" "$LUKS_SECRET_FILE" \
      "$serial" <<'REMOTE'
set -Eeuo pipefail
helper="$1"; tool="$2"; member="$3"; key_file="$4"; serial="$5"
{
  printf '#!/usr/bin/env bash\nset -Eeuo pipefail\nset +x\n'
  printf 'exec %q luks-prepare-member --member %q --key-file %q %q\n' \
    "$tool" "$member" "$key_file" "$serial"
} >"$helper"
chmod 0700 "$helper"
REMOTE

    remote test -x "$helper" ||
      fail "Prepared LUKS helper for member $member is missing from $HOST_ID"
    remote grep -Fq -- "$serial" "$helper" ||
      fail "Prepared member $member helper no longer matches configured serial $serial"
    printf '\nMANUAL LUKS ACTION REQUIRED\n'
    printf 'At the target host console, log in as root and run:\n  %s\n' "$helper"
    printf 'It asks you to type GO. Rerunning it after it reported success is harmless.\n'
    printf 'The helper reads the root-only temporary LUKS key file; it will not prompt for the password.\n'
    wait_for_helper_success "$helper" \
      "LUKS member ${member} was formatted/opened and its header was backed up"
    remote "$RPOOL_MIRROR_TOOL" luks-check-member --member "$member" "$serial" ||
      fail "LUKS member $member is not open on partition 3 of $serial with its header backed up"

    confirm_exact \
      "Attach /dev/mapper/${mapper} to rpool, resilver, configure initramfs manual unlock, refresh both ESPs, and export the LUKS header." \
      "ATTACH ENCRYPTED MEMBER ${member}"
  else
    info "Recovered member $member after its encrypted mapper was already attached; reconciling boot files and backups."
  fi

  # Record the mapping in crypttab and prove the rebuilt initramfs unlocks it
  # before the pool depends on it.
  remote "$RPOOL_MIRROR_TOOL" luks-register --member "$member" "$serial" ||
    fail "Could not record LUKS member $member for boot unlock"
  remote_script "$serial" "$mapper" <<'REMOTE'
set -Eeuo pipefail
serial="$1"; mapper="$2"
disk="$(app-ha-disk-by-serial "$serial")"
[[ -b "/dev/mapper/$mapper" ]]
backing="$(cryptsetup status "$mapper" | awk '$1 == "device:" { print $2; exit }')"
[[ "$(readlink -f "$backing")" == "$(readlink -f "${disk}p3")" ]]
pool="$(zpool status -P rpool)"
if ! awk -v member="/dev/mapper/$mapper" '$1 == member { found=1 } END { exit !found }' <<<"$pool"; then
  survivor="$(awk '$1 ~ /^\/dev\// { print $1; exit }' <<<"$pool")"
  [[ -b "$survivor" ]]
  (( $(blockdev --getsize64 "/dev/mapper/$mapper") >= $(blockdev --getsize64 "$survivor") ))
  zpool attach rpool "$survivor" "/dev/mapper/$mapper"
fi
while zpool status rpool | grep -q 'resilver in progress'; do sleep 30; done
zpool status -x rpool | grep -F "pool 'rpool' is healthy"
pool="$(zpool status -P rpool)"
awk -v member="/dev/mapper/$mapper" '$1 == member { found++ } END { exit !(found == 1) }' <<<"$pool"
REMOTE
  remote "$RPOOL_MIRROR_TOOL" luks-backup-headers --member "$member" "$serial" ||
    fail "Could not back up the LUKS header of member $member"
  copy_from_host "$remote_header" "$local_header"
  chmod 0600 "$local_header"
  remote rm -f "$remote_header" "$helper" "$backup" "$phase_file"
  write_state "$state"
  rm -f "$(state_path "$prepared_state")"
}

encrypted_boot_test() {
  local survivor=$1 unavailable serial mapper state active backup helper
  local schedule_reboot=false
  if [[ "$survivor" == A ]]; then
    unavailable=B
    serial="$NVME_MIRROR_0_SERIAL_2"
    mapper=crypt-rpool-b
  else
    unavailable=A
    serial="$NVME_MIRROR_0_SERIAL_1"
    mapper=crypt-rpool-a
  fi
  state="encrypted-final-${survivor}-only-passed"
  active="encrypted-final-${survivor}-test-active"
  backup="/etc/crypttab.app-ha-failtest-${mapper}"
  helper="/root/app-ha-open-${mapper}"
  if has_state "$state"; then
    remote rm -f "$backup" "$helper"
    rm -f "$(state_path "$active")" "$(state_path "${active}-boot-id")"
    return
  fi

  if ! has_state "$active"; then
    assert_pool_healthy
    confirm_exact \
      "Temporarily remove encrypted member ${unavailable} from the boot-time unlock set, disable its ESP, offline it from rpool, and reboot to prove encrypted member ${survivor} boots alone." \
      "TEST ENCRYPTED MEMBER ${survivor} BOOT"
    write_state "${active}-boot-id" "$(remote cat /proc/sys/kernel/random/boot_id)"
    write_state "$active" preparing
    schedule_reboot=true
  elif [[ "$(remote cat /proc/sys/kernel/random/boot_id)" == "$(read_state "${active}-boot-id")" ]]; then
    info "The encrypted member $survivor test is armed but has not rebooted; reconciling its reversible failure simulation and scheduling the reboot again."
    schedule_reboot=true
  fi
  if [[ "$schedule_reboot" == true ]]; then
    remote_script "$serial" "$mapper" "$backup" <<'REMOTE'
set -Eeuo pipefail
serial="$1"; mapper="$2"; backup="$3"
disk="$(app-ha-disk-by-serial "$serial")"
member="/dev/mapper/$mapper"
[[ -b "$member" ]]
zpool status -P rpool |
  awk -v member="$member" '$1 == member { found++ } END { exit !(found == 1) }'
if [[ ! -e "$backup" ]]; then
  install -m 0600 /etc/crypttab "$backup"
fi
[[ -s "$backup" ]]
awk -v name="$mapper" '$1 != name' /etc/crypttab > /etc/crypttab.app-ha-failtest-new
install -m 0600 /etc/crypttab.app-ha-failtest-new /etc/crypttab
rm -f /etc/crypttab.app-ha-failtest-new
update-initramfs -u -k all
proxmox-boot-tool refresh
mountpoint="/mnt/app-ha-esp-test-$serial"
mkdir -p "$mountpoint"
mountpoint -q "$mountpoint" || mount "${disk}p2" "$mountpoint"
if [[ -d "$mountpoint/EFI" && ! -e "$mountpoint/EFI.APP-HA-DISABLED" ]]; then
  mv "$mountpoint/EFI" "$mountpoint/EFI.APP-HA-DISABLED"
fi
[[ ! -e "$mountpoint/EFI" && -d "$mountpoint/EFI.APP-HA-DISABLED" ]]
sync
umount "$mountpoint"
zpool status -P rpool |
  awk -v member="$member" '$1 == member && $2 == "OFFLINE" { found=1 } END { exit !found }' ||
  zpool offline rpool "$member"
systemd-run --unit="app-ha-encrypted-${mapper}-reboot-$(date +%s)" \
  --on-active=1s --timer-property=AccuracySec=100ms \
  --collect /usr/bin/systemctl reboot
REMOTE
    write_state "$active" scheduled
  fi

  reboot_and_wait \
    "Enter the shared LUKS passphrase at the target host console; only encrypted member ${survivor} should be requested during this failure-simulation boot." \
    "$active"
  remote_script "$mapper" "$backup" "$helper" "$LUKS_SECRET_FILE" <<'REMOTE'
set -Eeuo pipefail
mapper="$1"; backup="$2"; helper="$3"; key_file="$4"
[[ -s "$backup" ]]
install -m 0600 "$backup" /etc/crypttab
cat >"$helper" <<EOF
#!/usr/bin/env bash
set -Eeuo pipefail
set +x
on_error() {
  code=\$?
  printf 'ERROR: Could not open encrypted rpool mapper $mapper (failed near line %s, exit %s).\n' \
    "\${BASH_LINENO[0]}" "\$code" >&2
  exit "\$code"
}
trap on_error ERR
if [[ -b "/dev/mapper/$mapper" ]]; then
  printf 'Encrypted rpool mapper $mapper is already open; no action was needed.\n'
else
  device="\$(awk '\$1 == "$mapper" { sub(/^UUID=/, "", \$2); print "/dev/disk/by-uuid/" \$2; exit }' /etc/crypttab)"
  [[ -n "\$device" && -b "\$device" ]]
  key_file="$key_file"
  [[ -s "\$key_file" && ! -L "\$key_file" ]]
  [[ "\$(stat -c '%U:%G:%a' "\$key_file")" == root:root:600 ]]
  cryptsetup open --key-file="\$key_file" "\$device" "$mapper"
  [[ -b "/dev/mapper/$mapper" ]]
  printf 'Encrypted rpool mapper $mapper opened successfully.\n'
fi
EOF
chmod 0700 "$helper"
REMOTE
  printf '\nMANUAL LUKS ACTION REQUIRED\n'
  printf 'Encrypted member %s booted alone. At the target host console, log in as root and restore member %s by running:\n  %s\n' \
    "$survivor" "$unavailable" "$helper"
  printf 'The helper reads the root-only temporary LUKS key file; it will not prompt for the password.\n'
  wait_for_helper_success "$helper" \
    "encrypted member ${unavailable} reopened as /dev/mapper/${mapper}"
  remote test -b "/dev/mapper/$mapper" ||
    fail "Expected restored mapper /dev/mapper/$mapper is not open"

  remote_script "$serial" "$mapper" "$backup" "$helper" <<'REMOTE'
set -Eeuo pipefail
serial="$1"; mapper="$2"; backup="$3"; helper="$4"
disk="$(app-ha-disk-by-serial "$serial")"
member="/dev/mapper/$mapper"
[[ -b "$member" ]]
mountpoint="/mnt/app-ha-esp-test-$serial"
mkdir -p "$mountpoint"
mountpoint -q "$mountpoint" || mount "${disk}p2" "$mountpoint"
if [[ ! -e "$mountpoint/EFI" && -d "$mountpoint/EFI.APP-HA-DISABLED" ]]; then
  mv "$mountpoint/EFI.APP-HA-DISABLED" "$mountpoint/EFI"
fi
[[ -d "$mountpoint/EFI" && ! -e "$mountpoint/EFI.APP-HA-DISABLED" ]]
sync
umount "$mountpoint"
zpool status -P rpool |
  awk -v member="$member" '$1 == member && $2 == "ONLINE" { found=1 } END { exit !found }' ||
  zpool online rpool "$member"
while zpool status rpool | grep -q 'resilver in progress'; do sleep 15; done
zpool status -x rpool | grep -F "pool 'rpool' is healthy"
update-initramfs -u -k all
proxmox-boot-tool refresh
REMOTE
  write_state "$state"
  remote rm -f "$backup" "$helper"
  rm -f "$(state_path "$active")" "$(state_path "${active}-boot-id")"
}

verify_encrypted_mirror() {
  {
    remote_script "$NVME_MIRROR_0_SERIAL_1" "$NVME_MIRROR_0_SERIAL_2" <<'REMOTE'
set -Eeuo pipefail
serials=("$1" "$2")
mappers=(crypt-rpool-a crypt-rpool-b)
pool="$(zpool status -P rpool)"
printf '%s\n' "$pool"
for index in 0 1; do
  mapper="${mappers[$index]}"
  disk="$(app-ha-disk-by-serial "${serials[$index]}")"
  awk -v member="/dev/mapper/$mapper" '$1 == member { found++ } END { exit !(found == 1) }' <<<"$pool"
  backing="$(cryptsetup status "$mapper" | awk '$1 == "device:" { print $2; exit }')"
  [[ "$(readlink -f "$backing")" == "$(readlink -f "${disk}p3")" ]]
done
awk '
  $1 ~ /^mirror-/ { mirror=$1 }
  $1 == "/dev/mapper/crypt-rpool-a" { a=mirror }
  $1 == "/dev/mapper/crypt-rpool-b" { b=mirror }
  END { exit !(a != "" && a == b) }
' <<<"$pool"
zpool status -x rpool | grep -F "pool 'rpool' is healthy"
[[ -z "$(swapon --noheadings --show=NAME)" ]]
proxmox-boot-tool status
REMOTE
  } | tee "${LOG_DIR}/encrypted-mirror.txt"
  [[ -s "${HEADER_DIR}/rpool-A.bin" && -s "${HEADER_DIR}/rpool-B.bin" ]] ||
    fail "Both off-host LUKS header backups are required"
  write_state encrypted-mirror-verified
}

# Install lib/rpool_mirror.sh on the target as the one copy of the member LUKS
# and extra-mirror disk, LUKS, and zpool logic. hosts/add_new_disk_vdev.sh and
# hosts/add_replacement_disk.sh install the same file after setup.
install_rpool_mirror_tool() {
  local expected actual staged=/root/.app-ha-rpool-mirror.upload
  expected="$(sha256sum "$RPOOL_MIRROR_SOURCE" | awk '{print $1}')"
  actual="$(remote sha256sum "$RPOOL_MIRROR_TOOL" 2>/dev/null | awk '{print $1}')" ||
    actual=""
  [[ "$actual" == "$expected" ]] && return
  copy_to_host "$RPOOL_MIRROR_SOURCE" "$staged"
  remote install -o root -g root -m 0700 "$staged" "$RPOOL_MIRROR_TOOL"
  remote rm -f "$staged"
  actual="$(remote sha256sum "$RPOOL_MIRROR_TOOL" | awk '{print $1}')"
  [[ "$actual" == "$expected" ]] ||
    fail "Installed $RPOOL_MIRROR_TOOL does not match $RPOOL_MIRROR_SOURCE"
}

# Disk-size requirement for the rpool mirror tool: each disk must hold at
# least 99% of the smaller recorded (manual or iDRAC) capacity of its pair.
rpool_mirror_size_args() {
  printf '%s\n' --expect-bytes "$1"
}

configure_extra_mirror() {
  local pair=$1
  local serial_1_name="NVME_MIRROR_${pair}_SERIAL_1"
  local serial_2_name="NVME_MIRROR_${pair}_SERIAL_2"
  local serial_1="${!serial_1_name}" serial_2="${!serial_2_name}"
  local mapper_1="crypt-rpool-mirror${pair}-1"
  local mapper_2="crypt-rpool-mirror${pair}-2"
  local helper="/root/app-ha-luks-extra-mirror-${pair}"
  local helper_config="${helper}.config-sha256"
  local prepared_state="extra-mirror-${pair}-prepared"
  local added_state="extra-mirror-${pair}-added"
  local header_1="${HEADER_DIR}/rpool-mirror${pair}-1.bin"
  local header_2="${HEADER_DIR}/rpool-mirror${pair}-2.bin"
  local remote_header_1="/root/luks-header-mirror${pair}-1.bin"
  local remote_header_2="/root/luks-header-mirror${pair}-2.bin"
  local expected_capacity pair_config_hash remote_helper_hash pool
  local has_1=false has_2=false
  local -a size_args=()
  expected_capacity="$(read_state "mirror-${pair}-minimum-capacity-bytes")"
  mapfile -t size_args < <(rpool_mirror_size_args "$expected_capacity")
  pair_config_hash="$(
    printf '%s\n%s\n%s\n' "$serial_1" "$serial_2" "$expected_capacity" |
      sha256sum | awk '{print $1}'
  )"
  install_rpool_mirror_tool
  pool="$(remote zpool status -P rpool)"
  awk -v member="/dev/mapper/$mapper_1" '$1 == member { found=1 } END { exit !found }' <<<"$pool" && has_1=true
  awk -v member="/dev/mapper/$mapper_2" '$1 == member { found=1 } END { exit !found }' <<<"$pool" && has_2=true

  if [[ "$has_1" == true || "$has_2" == true ]]; then
    [[ "$has_1" == true && "$has_2" == true ]] ||
      fail "rpool contains only one member of configured extra mirror $pair"
    remote "$RPOOL_MIRROR_TOOL" luks-backup-headers --pair "$pair" \
      "$serial_1" "$serial_2" ||
      fail "Could not back up the LUKS headers of extra mirror $pair"
    remote "$RPOOL_MIRROR_TOOL" luks-add --pair "$pair" "$serial_1" "$serial_2" ||
      fail "Could not reconcile boot unlock for extra mirror $pair"
    if [[ ! -s "$header_1" ]] && remote test -s "$remote_header_1"; then
      copy_from_host "$remote_header_1" "$header_1"
      chmod 0600 "$header_1"
    fi
    if [[ ! -s "$header_2" ]] && remote test -s "$remote_header_2"; then
      copy_from_host "$remote_header_2" "$header_2"
      chmod 0600 "$header_2"
    fi
    [[ -s "$header_1" && -s "$header_2" ]] ||
      fail "Extra mirror $pair is in rpool but both off-host LUKS header backups are not available"
    write_state "$added_state"
    rm -f "$(state_path "$prepared_state")"
    remote rm -f "$helper" "$helper_config" "$remote_header_1" "$remote_header_2"
    return
  fi
  has_state "$added_state" &&
    fail "Recorded extra mirror $pair is absent from rpool; refusing to format or re-add disks"

  if ! has_state "$prepared_state"; then
    if remote test -x "$helper"; then
      remote_helper_hash=""
      if remote test -s "$helper_config"; then
        remote_helper_hash="$(remote cat "$helper_config")"
      fi
      [[ "$remote_helper_hash" == "$pair_config_hash" ]] ||
        fail "Remote helper for extra mirror $pair was prepared for different serials or capacities"
      info "Recovered interrupted LUKS preparation for extra mirror $pair from $HOST_ID."
      write_state "$prepared_state"
    else
      local luks_probe
      luks_probe="$(remote "$RPOOL_MIRROR_TOOL" check-new "${size_args[@]}" \
        "$serial_1" "$serial_2")" ||
        fail "The configured disks for extra mirror $pair are not safe to format"
      if awk -F '\t' '$4 == "luks" { found=1 } END { exit !found }' <<<"$luks_probe"; then
        confirm_exact \
          "At least one serial-selected disk for extra mirror $pair already contains LUKS. Confirm that it belongs to this interrupted setup before adopting it; non-LUKS members will be erased." \
          "ADOPT OR FORMAT EXTRA MIRROR ${pair}"
      else
        confirm_exact \
          "Erase the two serial-selected disks for extra mirror $pair, give each one LUKS partition, and prepare them for an rpool mirror vdev. These disks receive no ESP." \
          "FORMAT EXTRA MIRROR ${pair}"
      fi

      # The console helper only wraps the shared tool, which proves the key
      # file against every existing rpool LUKS member before formatting.
      remote_script "$helper" "$helper_config" "$pair_config_hash" \
        "$RPOOL_MIRROR_TOOL" "$pair" "$LUKS_SECRET_FILE" \
        "$serial_1" "$serial_2" "${size_args[@]}" <<'REMOTE'
set -Eeuo pipefail
helper="$1"; helper_config="$2"; pair_config_hash="$3"; tool="$4"; pair="$5"
key_file="$6"; serial_1="$7"; serial_2="$8"
shift 8
{
  printf '#!/usr/bin/env bash\nset -Eeuo pipefail\nset +x\n'
  printf 'exec %q luks-prepare --pair %q --key-file %q' "$tool" "$pair" "$key_file"
  printf ' %q' "$@" "$serial_1" "$serial_2"
  printf '\n'
} >"$helper"
chmod 0700 "$helper"
printf '%s\n' "$pair_config_hash" >"$helper_config"
chmod 0600 "$helper_config"
REMOTE
      write_state "$prepared_state"
    fi
  fi

  remote test -x "$helper" ||
    fail "Prepared helper for extra mirror $pair is missing from $HOST_ID"
  remote_helper_hash=""
  if remote test -s "$helper_config"; then
    remote_helper_hash="$(remote cat "$helper_config")"
  fi
  [[ "$remote_helper_hash" == "$pair_config_hash" ]] ||
    fail "Prepared helper for extra mirror $pair no longer matches configured serials and capacity"

  printf '\nMANUAL LUKS ACTION REQUIRED\n'
  printf 'At the target host console, log in as root and run:\n  %s\n' "$helper"
  printf 'The helper reads the root-only temporary LUKS key file; it will not prompt for the password.\n'
  wait_for_helper_success "$helper" \
    "both encrypted members of extra mirror ${pair} were prepared"

  remote "$RPOOL_MIRROR_TOOL" luks-check-prepared --pair "$pair" \
    "$serial_1" "$serial_2" ||
    fail "Extra mirror $pair was not fully prepared; rerun the helper at the console"
  copy_from_host "$remote_header_1" "$header_1"
  copy_from_host "$remote_header_2" "$header_2"
  chmod 0600 "$header_1" "$header_2"

  confirm_exact \
    "Add encrypted mappers $mapper_1 and $mapper_2 to rpool as one new top-level mirror vdev. A later top-level vdev removal is possible (hosts/decommission_disks.sh) but evacuates its data first." \
    "ADD EXTRA MIRROR ${pair} TO RPOOL"
  remote "$RPOOL_MIRROR_TOOL" luks-add --pair "$pair" "$serial_1" "$serial_2" ||
    fail "Could not add extra mirror $pair to rpool"
  write_state "$added_state"
  rm -f "$(state_path "$prepared_state")"
  remote rm -f "$helper" "$helper_config" "$remote_header_1" "$remote_header_2"
}

configure_raw_extra_mirror() {
  local pair=$1
  local serial_1_name="NVME_MIRROR_${pair}_SERIAL_1"
  local serial_2_name="NVME_MIRROR_${pair}_SERIAL_2"
  local serial_1="${!serial_1_name}" serial_2="${!serial_2_name}"
  local expected_capacity state
  local -a size_args=()
  expected_capacity="$(read_state "mirror-${pair}-minimum-capacity-bytes")"
  mapfile -t size_args < <(rpool_mirror_size_args "$expected_capacity")
  state="raw-extra-mirror-${pair}-added"

  if remote_script "$serial_1" "$serial_2" <<'REMOTE' >/dev/null 2>&1
set -Eeuo pipefail
serial_1="$1"; serial_2="$2"
pool="$(zpool status -P rpool)"
awk -v serial_1="$serial_1" -v serial_2="$serial_2" '
  $1 ~ /^mirror-/ { mirror=$1 }
  $1 ~ serial_1 { a=mirror }
  $1 ~ serial_2 { b=mirror }
  END { exit !(a != "" && a == b) }
' <<<"$pool"
REMOTE
  then
    write_state "$state"
    return
  fi
  has_state "$state" &&
    fail "Recorded unencrypted extra mirror $pair is no longer present in rpool"

  confirm_exact \
    "Erase both serial-selected disks for extra mirror $pair, give each one partition, and add the partitions as one unencrypted top-level rpool mirror vdev. These disks receive no ESP. A later top-level vdev removal is possible (hosts/decommission_disks.sh) but evacuates its data first." \
    "ADD UNENCRYPTED EXTRA MIRROR ${pair}"
  install_rpool_mirror_tool
  remote "$RPOOL_MIRROR_TOOL" clear-add "${size_args[@]}" "$serial_1" "$serial_2" ||
    fail "Could not add unencrypted extra mirror $pair to rpool"
  write_state "$state"
}

configure_extra_mirrors() {
  local pair pool configured configured_pair
  pool="$(remote zpool status -P rpool)"
  for pair in 1 2 3 4; do
    configured=false
    for configured_pair in "${CONFIGURED_MIRROR_PAIRS[@]}"; do
      [[ "$configured_pair" != "$pair" ]] || configured=true
    done
    [[ "$configured" == false ]] || continue
    if awk -v one="/dev/mapper/crypt-rpool-mirror${pair}-1" \
      -v two="/dev/mapper/crypt-rpool-mirror${pair}-2" \
      '$1 == one || $1 == two { found=1 } END { exit !found }' <<<"$pool"; then
      fail "rpool still contains managed mirror $pair, but that pair was removed from configuration"
    fi
    if remote test -e "/root/app-ha-luks-extra-mirror-${pair}"; then
      fail "A prepared helper still exists for extra mirror $pair, but that pair was removed from configuration"
    fi
    # shellcheck disable=SC2016 # This is an awk program, not shell expansion.
    if remote awk \
      -v "one=crypt-rpool-mirror${pair}-1" \
      -v "two=crypt-rpool-mirror${pair}-2" \
      '$1 == one || $1 == two { found=1 } END { exit !found }' \
      /etc/crypttab; then
      fail "crypttab still contains managed mirror $pair, but that pair was removed from configuration"
    fi
  done
  for pair in "${CONFIGURED_MIRROR_PAIRS[@]:1}"; do
    if [[ "$ENCRYPTION_POLICY" == luks ]]; then
      configure_extra_mirror "$pair"
    else
      configure_raw_extra_mirror "$pair"
    fi
  done
}

verify_clear_mirrors() {
  remote_script "${CONFIGURED_NVME_SERIALS[@]}" <<'REMOTE' |
    tee "${LOG_DIR}/clear-mirrors.txt"
set -Eeuo pipefail
(($# >= 2 && $# % 2 == 0))
pool="$(zpool status -P rpool)"
printf '%s\n' "$pool"
while (($#)); do
  serial_1="$1"; serial_2="$2"; shift 2
  awk -v serial_1="$serial_1" -v serial_2="$serial_2" '
    $1 ~ /^mirror-/ { mirror=$1 }
    index($1, serial_1) { a=mirror; count_a++ }
    index($1, serial_2) { b=mirror; count_b++ }
    END { exit !(count_a == 1 && count_b == 1 && a != "" && a == b) }
  ' <<<"$pool"
done
! grep -Fq '/dev/mapper/crypt-rpool-' <<<"$pool"
! awk '$1 ~ /^crypt-rpool-/ { found=1 } END { exit !found }' /etc/crypttab
zpool status -x rpool | grep -F "pool 'rpool' is healthy"
proxmox-boot-tool status
REMOTE
  write_state clear-mirrors-verified "$RPOOL_LAYOUT_SHA256"
}

configure_shared_luks_unlock() {
  local expected_state="layout-${RPOOL_LAYOUT_SHA256}"
  local helper="/root/app-ha-verify-shared-luks-${RPOOL_LAYOUT_SHA256}"
  local marker="/root/.app-ha-shared-luks-verified-${RPOOL_LAYOUT_SHA256}"
  if has_state shared-luks-unlock-configured &&
     [[ "$(read_state shared-luks-unlock-configured)" == "$expected_state" ]] &&
     remote_script "${EXPECTED_RPOOL_MAPPERS[@]}" <<'REMOTE' >/dev/null 2>&1
set -Eeuo pipefail
for mapper in "$@"; do
  awk -v mapper="$mapper" '
    $1 == mapper {
      if ($3 != "app-ha-rpool" ||
          $4 !~ /(^|,)keyscript=decrypt_keyctl(,|$)/) exit 1
      found++
    }
    END { if (found != 1) exit 1 }
  ' /etc/crypttab
done
lsinitramfs "/boot/initrd.img-$(uname -r)" |
  awk '/(^|\/)decrypt_keyctl$/ { found=1 } END { exit !found }'
REMOTE
  then
    return
  fi

  log "Configuring one-passphrase unlock for all rpool LUKS members"
  remote_apt_script "$helper" "$marker" "$LUKS_SECRET_FILE" \
    "${EXPECTED_RPOOL_MAPPERS[@]}" <<'REMOTE'
set -Eeuo pipefail
helper="$1"; marker="$2"; key_file="$3"; shift 3
export DEBIAN_FRONTEND=noninteractive
apt_wait_for_locks
apt-get -o DPkg::Lock::Timeout=60 update
apt-get -o DPkg::Lock::Timeout=60 install -y cryptsetup cryptsetup-initramfs keyutils
[[ -x /usr/lib/cryptsetup/scripts/decrypt_keyctl ]]
[[ -x "$(command -v keyctl)" ]]
for mapper in "$@"; do
  [[ -b "/dev/mapper/$mapper" ]]
  backing="$(cryptsetup status "$mapper" | awk '$1 == "device:" { print $2; exit }')"
  [[ -b "$backing" ]]
  cryptsetup isLuks "$backing"
  uuid="$(cryptsetup luksUUID "$backing")"
  awk -v mapper="$mapper" -v source="UUID=$uuid" '
    $1 == mapper && $2 == source { found++ }
    END { exit !(found == 1) }
  ' /etc/crypttab
done

{
  cat <<'EOF'
#!/usr/bin/env bash
set -Eeuo pipefail
set +x
umask 077
mappers=(
EOF
  for mapper in "$@"; do printf '  %q\n' "$mapper"; done
  cat <<EOF
)
marker=$(printf '%q' "$marker")
key_file=$(printf '%q' "$key_file")
[[ -s "\$key_file" && ! -L "\$key_file" ]]
[[ "\$(stat -c '%U:%G:%a' "\$key_file")" == root:root:600 ]]
for mapper in "\${mappers[@]}"; do
  backing="\$(cryptsetup status "\$mapper" | awk '\$1 == "device:" { print \$2; exit }')"
  [[ -b "\$backing" ]] || {
    printf 'Backing device for %s is unavailable\n' "\$mapper" >&2
    exit 1
  }
  cryptsetup open --test-passphrase --key-file="\$key_file" "\$backing"
  printf 'Verified shared passphrase for %s\n' "\$mapper"
done
printf 'verified-mappers=%s\n' "\${#mappers[@]}" >"\$marker"
chmod 0600 "\$marker"
printf 'Verified one shared passphrase against all %s rpool LUKS members.\n' "\${#mappers[@]}"
EOF
} >"$helper"
chmod 0700 "$helper"
REMOTE

  if ! remote test -s "$marker"; then
    printf '\nMANUAL SHARED-PASSPHRASE VERIFICATION REQUIRED\n'
    printf 'At the target host console, log in as root and run:\n  %s\n' "$helper"
    printf 'The helper reads the root-only temporary LUKS key file and tests every member; it will not prompt for the password.\n'
    wait_for_helper_success "$helper" \
      "all ${#EXPECTED_RPOOL_MAPPERS[@]} LUKS members passed shared-key verification"
  fi
  remote test -s "$marker" ||
    fail "The host did not record successful shared-passphrase verification"

  printf 'decrypt_keyctl will now be enabled only after every configured LUKS member accepted the same passphrase.\n'
  wait_for_exact \
    "Confirm crypttab may be updated to enable shared LUKS unlock." \
    "ENABLE SHARED LUKS UNLOCK"
  remote_script "${EXPECTED_RPOOL_MAPPERS[@]}" <<'REMOTE'
set -Eeuo pipefail
export DEBIAN_FRONTEND=noninteractive
[[ -x /usr/lib/cryptsetup/scripts/decrypt_keyctl ]]
[[ -x "$(command -v keyctl)" ]]

backup=/etc/crypttab.app-ha-before-shared-unlock
[[ -e "$backup" ]] || install -m 0600 /etc/crypttab "$backup"
work="$(mktemp /etc/crypttab.app-ha-shared.XXXXXX)"
install -m 0600 /etc/crypttab "$work"
for mapper in "$@"; do
  count="$(awk -v mapper="$mapper" '$1 == mapper { count++ } END { print count + 0 }' "$work")"
  [[ "$count" == 1 ]]
  source_spec="$(awk -v mapper="$mapper" '$1 == mapper { print $2; exit }' "$work")"
  [[ -n "$source_spec" ]]
  awk -v mapper="$mapper" '$1 != mapper' "$work" >"${work}.next"
  printf '%s\t%s\tapp-ha-rpool\tluks,initramfs,nofail,keyscript=decrypt_keyctl\n' \
    "$mapper" "$source_spec" >>"${work}.next"
  mv "${work}.next" "$work"
done
install -m 0600 "$work" /etc/crypttab
rm -f "$work"
update-initramfs -u -k all

for mapper in "$@"; do
  awk -v mapper="$mapper" '
    $1 == mapper {
      if ($3 != "app-ha-rpool" ||
          $4 !~ /(^|,)keyscript=decrypt_keyctl(,|$)/) exit 1
      found++
    }
    END { if (found != 1) exit 1 }
  ' /etc/crypttab
done
lsinitramfs "/boot/initrd.img-$(uname -r)" |
  awk '/(^|\/)decrypt_keyctl$/ { found=1 } END { exit !found }'
keyctl show >/dev/null
REMOTE
  remote rm -f "$helper" "$marker"
  write_state shared-luks-unlock-configured "$expected_state"
}

final_normal_boot_test() {
  local expected_state="layout-${RPOOL_LAYOUT_SHA256}"
  local schedule_reboot=false
  if has_state final-normal-boot-passed &&
     [[ "$(read_state final-normal-boot-passed)" == "$expected_state" ]]; then
    return
  fi
  local active=final-normal-boot-active
  if ! has_state "$active"; then
    confirm_exact \
      "Reboot the restored, healthy rpool and verify the host boots successfully in its final ${ENCRYPTION_POLICY} state." \
      "TEST FINAL HEALTHY MIRROR BOOT"
    write_state "${active}-boot-id" "$(remote cat /proc/sys/kernel/random/boot_id)"
    write_state "$active" preparing
    schedule_reboot=true
  elif [[ "$(remote cat /proc/sys/kernel/random/boot_id)" == "$(read_state "${active}-boot-id")" ]]; then
    info "The final healthy-state test is armed but has not rebooted; scheduling the orderly reboot again."
    schedule_reboot=true
  fi
  if [[ "$schedule_reboot" == true ]]; then
    remote systemd-run \
      "--unit=app-ha-final-normal-reboot-$(date +%s)" \
      --on-active=1s --timer-property=AccuracySec=100ms \
      --collect /usr/bin/systemctl reboot
    write_state "$active" scheduled
  fi
  if [[ "$ENCRYPTION_POLICY" == luks ]]; then
    reboot_and_wait "Watch the target host console and enter the shared LUKS passphrase once. decrypt_keyctl should reuse it for every encrypted rpool member." "$active"
  else
    reboot_and_wait "No LUKS passphrase is required. Verify the normal Proxmox login prompt appears at the target host console." "$active"
  fi
  assert_pool_healthy
  local pool mapper
  pool="$(remote zpool status -P rpool)"
  for mapper in "${EXPECTED_RPOOL_MAPPERS[@]}"; do
    awk -v member="/dev/mapper/$mapper" \
      '$1 == member { found++ } END { exit !(found == 1) }' <<<"$pool" ||
      fail "Encrypted rpool member $mapper did not return after the reboot"
  done
  write_state final-normal-boot-passed "$expected_state"
  rm -f "$(state_path "$active")" "$(state_path "${active}-boot-id")"
}

configure_private_network() {
  has_state private-network-configured && return
  log "Configuring the 10 Gbps private VLAN bridge"
  remote_script "$PROXMOX_PUBLIC_MAC" "$PROXMOX_SECONDARY_MAC" \
    "$PROXMOX_SECONDARY_IP" "$MOX_REPLICATION_IP" \
    "$PROXMOX_PRIVATE_BRIDGE" "$PROXMOX_IP" <<'REMOTE'
set -Eeuo pipefail
public_mac="${1,,}"; requested_private_mac="${2,,}"; private_cidr="$3"
replication_cidr="$4"; bridge="$5"; public_ip="$6"

resolve_physical_nic() {
  local requested_mac=$1 path nic
  local -a matches=()
  for path in /sys/class/net/*; do
    [[ -e "$path/device" ]] || continue
    [[ "$(<"$path/address")" == "$requested_mac" ]] || continue
    nic="${path##*/}"
    matches+=("$nic")
  done
  ((${#matches[@]} == 1)) || {
    printf 'MAC %s resolved to %s physical interfaces: %s\n' \
      "$requested_mac" "${#matches[@]}" "${matches[*]:-none}" >&2
    return 1
  }
  printf '%s\n' "${matches[0]}"
}

public_nic="$(resolve_physical_nic "$public_mac")"
public_master_path="$(readlink -f "/sys/class/net/$public_nic/master" 2>/dev/null || true)"
[[ -n "$public_master_path" ]] || {
  printf 'Public NIC %s (%s) has no bridge master\n' "$public_nic" "$public_mac" >&2
  exit 1
}
public_master="${public_master_path##*/}"
[[ -d "/sys/class/net/$public_master/bridge" ]] || {
  printf 'Public NIC %s (%s) master %s is not a bridge\n' "$public_nic" "$public_mac" "$public_master" >&2
  exit 1
}
ip -4 -o address show dev "$public_master" |
  awk -v ip="$public_ip" '$4 ~ ("^" ip "/") { found=1 } END { exit !found }' || {
    printf 'Public bridge %s does not own expected address %s\n' "$public_master" "$public_ip" >&2
    exit 1
  }

if [[ "$requested_private_mac" == auto ]]; then
  mapfile -t candidates < <(
    for path in /sys/class/net/*; do
      nic="${path##*/}"
      [[ "$nic" != "$public_nic" && "$nic" != lo && "$nic" != tailscale0 && ! -d "$path/bridge" ]] || continue
      [[ -e "$path/device" && "$(cat "$path/carrier" 2>/dev/null || echo 0)" == 1 ]] && printf '%s\n' "$nic"
    done
  )
  ((${#candidates[@]} == 1)) || {
    printf 'auto private-NIC detection found %s carrier-up candidates: %s\n' "${#candidates[@]}" "${candidates[*]:-none}" >&2
    exit 1
  }
  private_nic="${candidates[0]}"
else
  private_nic="$(resolve_physical_nic "$requested_private_mac")"
  [[ "$private_nic" != "$public_nic" ]]
fi
ip link set dev "$private_nic" up
carrier=0
for attempt in $(seq 1 10); do
  carrier="$(cat "/sys/class/net/$private_nic/carrier" 2>/dev/null || printf 0)"
  [[ "$carrier" == 1 ]] && break
  if ((attempt < 10)); then
    printf 'Waiting for private NIC %s carrier (attempt %s/10; retrying in 3 seconds)\n' \
      "$private_nic" "$attempt"
    sleep 3
  fi
done
[[ "$carrier" == 1 ]] || {
  printf 'Private NIC %s (%s) has no physical carrier after approximately 30 seconds\n' \
    "$private_nic" "$(<"/sys/class/net/$private_nic/address")" >&2
  exit 1
}
speed="$(cat "/sys/class/net/$private_nic/speed" 2>/dev/null || printf unknown)"
[[ "$speed" == 10000 ]] || {
  printf 'Private NIC %s negotiated unexpected speed %s Mb/s; expected 10000\n' \
    "$private_nic" "$speed" >&2
  exit 1
}
cat >"/etc/network/interfaces.d/app-ha-private.cfg" <<EOF
auto $bridge
iface $bridge inet static
    address $private_cidr
    address $replication_cidr
    bridge-ports $private_nic
    bridge-stp off
    bridge-fd 0
EOF
ifreload -a
ip -4 address show dev "$bridge"
python3 - "$replication_cidr" "$(ip -j -4 address show dev "$bridge")" <<'PY'
import ipaddress
import json
import sys

expected = ipaddress.ip_interface(sys.argv[1])
rows = json.loads(sys.argv[2])
addresses = [
    ipaddress.ip_address(info["local"])
    for row in rows
    for info in row.get("addr_info", [])
    if info.get("family") == "inet"
]
matching = [address for address in addresses if address in expected.network]
if matching != [expected.ip]:
    raise SystemExit(
        f"migration CIDR {expected.network} must match exactly {expected.ip}; "
        f"found {matching}"
    )
PY
printf '%s\n' "$public_nic" >/etc/app-ha-public-nic
printf '%s\n' "$private_nic" >/etc/app-ha-private-nic
printf 'Resolved public MAC %s to %s via %s\n' "$public_mac" "$public_nic" "$public_master"
printf 'Resolved private MAC %s to %s at 10 Gbps\n' \
  "$(<"/sys/class/net/$private_nic/address")" "$private_nic"
REMOTE
  printf '\nVerify FiberState provides a shared Layer-2 VLAN, not separate routed segments.\n'
  wait_for_exact \
    "Confirm the bridge/IP was tested from the provider console or peer.

If you are running multiple instances of add_proxmox_host.sh concurrently, then it's recommended you only allow one instance at a time to proceed past this point in order to minimize possible issues such as lock-contention issues during host setup.  If one of the concurrent host setups you're executing is for the proxmox host specified by the PROXMOX_CONTROL_NODE value in your cluster.conf file then let that script run to completion first before letting the other scripts proceed one at a time past this point.
" \
    "PRIVATE VLAN VERIFIED"
  write_state private-network-configured
}

configure_cluster_host_resolution() {
  log "Pinning every mox hostname to its private VLAN address"
  local node expected_fqdn members
  local -a nodes=()
  members="$(cluster_member_nodes)" ||
    fail "Could not list cluster members through $(cluster_control_node)"
  mapfile -t nodes < <(
    printf '%s\n' "$HOST_ID" "$members" | sed '/^$/d' | sort -u -t x -k 2,2n
  )
  info "Hosts to pin: ${nodes[*]}"
  for node in "${nodes[@]}"; do
    expected_fqdn="${node}.${PROXMOX_INTERNAL_DOMAIN}"
    ssh -o BatchMode=yes -o ClearAllForwardings=yes \
      -o StrictHostKeyChecking=yes -o CheckHostIP=yes \
      -o "ConnectTimeout=${MOX_SSH_CONNECT_TIMEOUT:-8}" \
      "root@${node}" bash -s -- "$PRIVATE_SUBNET_PREFIX" "$MOX_IP_START_OCTET" \
      "$MAX_MOX_HOSTS" \
      "$PROXMOX_INTERNAL_DOMAIN" "$node" "$expected_fqdn" <<'REMOTE'
set -Eeuo pipefail
prefix="$1"; mox_start="$2"; max_hosts="$3"; internal_domain="$4"
host_id="$5"; expected_fqdn="$6"
begin='# BEGIN app-ha managed mox private identities'
end='# END app-ha managed mox private identities'
[[ -f /etc/hosts && ! -L /etc/hosts ]]
current_fqdn="$(hostname -f)"
[[ "$current_fqdn" == "$expected_fqdn" ]] || {
  printf 'Host %s was installed with FQDN %s, but configuration requires immutable identity %s. Perform a deliberate fresh reinstall; do not rename a clustered Proxmox node in place.\n' \
    "$host_id" "$current_fqdn" "$expected_fqdn" >&2
  exit 1
}
begin_count="$(grep -Fxc "$begin" /etc/hosts || true)"
end_count="$(grep -Fxc "$end" /etc/hosts || true)"
if ! { [[ "$begin_count" == 0 && "$end_count" == 0 ]] ||
       [[ "$begin_count" == 1 && "$end_count" == 1 ]]; }; then
  printf 'Managed mox identity markers in /etc/hosts are unbalanced or duplicated (begin=%s, end=%s).\n' \
    "$begin_count" "$end_count" >&2
  exit 1
fi
if [[ "$begin_count" == 1 ]]; then
  begin_line="$(grep -Fn "$begin" /etc/hosts | cut -d: -f1)"
  end_line="$(grep -Fn "$end" /etc/hosts | cut -d: -f1)"
  ((begin_line < end_line)) || {
    printf 'Managed mox identity markers in /etc/hosts are out of order.\n' >&2
    exit 1
  }
fi
work="$(mktemp /etc/hosts.app-ha.XXXXXX)"
awk -v begin="$begin" -v end="$end" '
  $0 == begin { managed=1; next }
  $0 == end { managed=0; next }
  managed { next }
  {
    has_mox=0
    has_other=0
    for (i=2; i<=NF; i++) {
      if ($i ~ /^#/) break
      if ($i ~ /^mox([1-9]|10)([.][A-Za-z0-9.-]+)?$/) has_mox=1
      else has_other=1
    }
    if (has_mox && has_other) {
      printf "Refusing to replace mixed-purpose /etc/hosts line: %s\n", $0 >"/dev/stderr"
      exit 42
    }
    if (!has_mox) print
  }
' /etc/hosts >"$work"
{
  printf '\n%s\n' "$begin"
  for ((index = 1; index <= max_hosts; index += 1)); do
    printf '%s.%s mox%s.%s mox%s\n' \
      "$prefix" "$((mox_start + index - 1))" "$index" "$internal_domain" "$index"
  done
  printf '%s\n' "$end"
} >>"$work"
install -o root -g root -m 0644 "$work" /etc/hosts
rm -f "$work"
[[ "$(hostname -s)" == "$host_id" ]]
[[ "$(hostname -f)" == "$expected_fqdn" ]]
for ((index = 1; index <= max_hosts; index += 1)); do
  expected_ip="${prefix}.$((mox_start + index - 1))"
  short="mox${index}"
  fqdn="${short}.${internal_domain}"
  [[ "$(getent ahostsv4 "$short" | awk 'NR == 1 { print $1 }')" == "$expected_ip" ]]
  [[ "$(getent ahostsv4 "$fqdn" | awk 'NR == 1 { print $1 }')" == "$expected_ip" ]]
done
REMOTE
  done
  write_state cluster-host-resolution-configured \
    "${MOX_IP_START}:${MAX_MOX_HOSTS}:${PROXMOX_INTERNAL_DOMAIN}"
}

cluster_control() {
  if [[ "$(cluster_control_node)" == "$HOST_ID" ]]; then
    remote "$@"
  else
    local command_string
    printf -v command_string '%q ' "$@"
    ssh -o BatchMode=yes -o ClearAllForwardings=yes \
      -o StrictHostKeyChecking=yes -o CheckHostIP=yes \
      -o "ConnectTimeout=${MOX_SSH_CONNECT_TIMEOUT:-8}" \
      "root@${EXISTING_NODE}" "$command_string"
  fi
}

cluster_control_tty() {
  local command_string=$1
  if [[ "$(cluster_control_node)" == "$HOST_ID" ]]; then
    local -a options
    mapfile -t options < <(ssh_options)
    ssh -tt "${options[@]}" "$(remote_target)" "$command_string"
  else
    ssh -tt -o ClearAllForwardings=yes \
      -o StrictHostKeyChecking=yes -o CheckHostIP=yes \
      -o "ConnectTimeout=${MOX_SSH_CONNECT_TIMEOUT:-8}" \
      "root@${EXISTING_NODE}" "$command_string"
  fi
}

# Run a command on cluster member NODE through the cluster control node.
cluster_node_control() {
  local node=$1
  shift
  if [[ "$node" == "$(cluster_control_node)" ]]; then
    cluster_control "$@"
    return
  fi
  cluster_control ssh -o BatchMode=yes -o ConnectTimeout=8 \
    -o StrictHostKeyChecking=yes \
    -o CheckHostIP=no \
    -o "HostKeyAlias=${node}" \
    -o "UserKnownHostsFile=/etc/pve/nodes/${node}/ssh_known_hosts" \
    -o GlobalKnownHostsFile=none \
    "root@${node}.${PROXMOX_INTERNAL_DOMAIN}" "$@"
}

# lib/qdevice.sh reaches the cluster through the cluster control node.
qd_coordinator() {
  cluster_control_node
}

qd_exec() {
  local node=$1 payload="set -Eeuo pipefail"$'\n'"exec" argument quoted
  shift
  for argument in "$@"; do
    printf -v quoted '%q' "$argument"
    payload+=" ${quoted}"
  done
  printf '%s\n' "$payload" | cluster_node_control "$node" bash -s
}

qd_exec_coordinator_tty() {
  cluster_control_tty "$1"
}

qd_member_states() {
  local nodes_json
  nodes_json="$(cluster_control pvesh get /nodes --output-format json)" || return 1
  jq -er --argjson max "$MAX_MOX_HOSTS" '
    [ .[] |
      if (.node | type == "string" and test("^mox([1-9]|10)$")) and
         ((.node | ltrimstr("mox") | tonumber) <= $max)
      then . else error("unexpected cluster node: \(.node)") end ] |
    sort_by(.node | ltrimstr("mox") | tonumber) | .[] |
    "\(.node) \(if .status == "online" then "online" else "offline" end)"
  ' <<<"$nodes_json"
}

qd_confirm_removal() {
  confirm_exact "$1"
}

assert_all_cluster_nodes_online() {
  local expected_count=${1:-} status nodes_json node_count api_count offline
  local control
  control="$(cluster_control_node)"
  status="$(cluster_control pvecm status)" ||
    fail "Could not read cluster status from $control"
  grep -Eq "^Name:[[:space:]]+${PROXMOX_CLUSTER_NAME}[[:space:]]*$" <<<"$status" ||
    fail "$control is not a member of expected cluster $PROXMOX_CLUSTER_NAME"
  grep -Eq '^Quorate:[[:space:]]+Yes' <<<"$status" ||
    fail "Cluster is not quorate; refusing a membership or QDevice change"
  node_count="$(awk '/^Nodes:/ {print $2; exit}' <<<"$status")"
  [[ "$node_count" =~ ^[1-9][0-9]*$ ]] ||
    fail "Could not determine configured Proxmox node count"
  ((node_count <= MAX_MOX_HOSTS)) ||
    fail "Cluster contains $node_count nodes, exceeding MAX_MOX_HOSTS=$MAX_MOX_HOSTS"
  if [[ -n "$expected_count" ]]; then
    ((node_count == expected_count)) ||
      fail "Expected $expected_count configured nodes before this phase; cluster reports $node_count"
  fi

  nodes_json="$(cluster_control pvesh get /nodes --output-format json)" ||
    fail "Could not query configured Proxmox node liveness"
  api_count="$(jq -er 'length' <<<"$nodes_json")"
  ((api_count == node_count)) ||
    fail "Cluster status reports $node_count nodes but /nodes reports $api_count"
  offline="$(jq -r '[.[] | select(.status != "online") | .node] | join(", ")' <<<"$nodes_json")"
  [[ -z "$offline" ]] ||
    fail "Every configured node must be online; unavailable nodes: $offline"
  # Removed and purged hosts leave gaps; every member must still use a slot name.
  jq -e --argjson max "$MAX_MOX_HOSTS" '
    all(.[]; (.node | type == "string" and test("^mox([1-9]|10)$")) and
      ((.node | ltrimstr("mox") | tonumber) <= $max))
  ' <<<"$nodes_json" >/dev/null ||
    fail "Every configured node must be named mox1 through mox${MAX_MOX_HOSTS}"
  printf '%s\n' "$node_count"
}

wait_for_all_cluster_nodes_online() {
  local expected_count=$1 status nodes_json status_count
  for _ in $(seq 1 60); do
    status="$(cluster_control pvecm status 2>/dev/null || true)"
    nodes_json="$(cluster_control pvesh get /nodes --output-format json 2>/dev/null || true)"
    status_count="$(awk '/^Nodes:/ {print $2; exit}' <<<"$status")"
    if [[ "$status_count" == "$expected_count" ]] &&
       jq -e --argjson count "$expected_count" \
         'length == $count and all(.[]; .status == "online")' \
         <<<"$nodes_json" >/dev/null 2>&1; then
      assert_all_cluster_nodes_online "$expected_count" >/dev/null
      return
    fi
    sleep 5
  done
  fail "Timed out waiting for all $expected_count configured cluster nodes to be online"
}

remove_qdevice_before_membership_change() {
  local status
  if ! qd_is_registered; then
    QDEVICE_REMOVED_FOR_JOIN=0
    qd_clear_stale
    return
  fi
  qd_remove "Proxmox requires removing the configured QDevice before adding $HOST_ID. Every configured node is online and must remain online until the join and quorum reconciliation complete."
  QDEVICE_REMOVED_FOR_JOIN=1
  status="$(cluster_control pvecm status)"
  pvecm_status_is_quorate "$status" ||
    fail "Cluster lost quorum after QDevice removal; do not continue the join"
}

# Record this slot as joining so slot recommendations skip it while the host
# joins. A registry that is not initialized yet records the slot later, when
# the shared tools are installed.
reserve_registry_host_slot() {
  local registry=/usr/local/lib/app-ha-proxmox/lib/cluster_registry.py help_text
  if ! cluster_control test -x "$registry" ||
    ! cluster_control test -s "${CLUSTER_STATE_DIR}/policy.json"; then
    info "The cluster registry is not initialized yet; $HOST_ID's slot is recorded when the shared tools are installed."
    return 0
  fi
  help_text="$(cluster_control "$registry" --help)" ||
    fail "Could not run the installed cluster registry on ${EXISTING_NODE}"
  if ! grep -q -- 'host-reserve' <<<"$help_text"; then
    info "The installed cluster registry predates host slots; $HOST_ID's slot is recorded when the shared tools are installed."
    return 0
  fi
  cluster_control "$registry" --state-dir "$CLUSTER_STATE_DIR" \
    host-reserve "$HOST_ID" >/dev/null ||
    fail "Could not reserve registry host slot $HOST_ID"
  info "Registry host slot $HOST_ID is reserved for this join."
}

cluster_setup() {
  local private_ip="${PROXMOX_SECONDARY_IP%/*}" ts_ip target_status
  local target_cluster_name="" already_member=false expected_count existing_private
  local existing_fqdn cluster_fingerprint fingerprint_output resolved_join_ip
  local served_fingerprint served_fingerprint_output
  ts_ip="$(remote tailscale ip -4 | awk '/^100\./ {print; exit}')"
  [[ "$ts_ip" == 100.* ]] || fail "Could not read target Tailscale IP"
  target_status="$(remote pvecm status 2>/dev/null || true)"
  target_cluster_name="$(awk '/^Name:/ {print $2; exit}' <<<"$target_status")"
  if [[ "$target_cluster_name" == "$PROXMOX_CLUSTER_NAME" ]]; then
    already_member=true
    info "$HOST_ID is already a member of cluster $PROXMOX_CLUSTER_NAME; reconciling shared settings."
  elif [[ -n "$target_cluster_name" ]]; then
    fail "$HOST_ID already belongs to unexpected cluster $target_cluster_name"
  fi

  if [[ "$already_member" == true ]]; then
    assert_all_cluster_nodes_online >/dev/null
  elif [[ "$SETUP_ROLE" == first ]]; then
    remote pvecm create "$PROXMOX_CLUSTER_NAME" \
      --link0 "address=${private_ip},priority=100" \
      --link1 "address=${ts_ip},priority=10"
    wait_for_all_cluster_nodes_online 1
  else
    ssh -o BatchMode=yes -o StrictHostKeyChecking=yes -o CheckHostIP=yes \
      -o ConnectTimeout=10 "root@${EXISTING_NODE}" true ||
      fail "Cannot SSH to root@${EXISTING_NODE}. Verify its SSH fingerprint at that host's trusted console, refresh any stale workstation known_hosts entry, and connect once manually."
    expected_count="$(assert_all_cluster_nodes_online)"
    ((expected_count < MAX_MOX_HOSTS)) ||
      fail "The cluster already has $expected_count nodes, the MAX_MOX_HOSTS limit"
    local members
    members="$(cluster_member_nodes)" ||
      fail "Could not list cluster members through ${EXISTING_NODE}"
    if grep -Fxq "$HOST_ID" <<<"$members"; then
      fail "$HOST_ID is already listed as a cluster member, but it does not report membership itself. If this host was reinstalled, first remove the old $HOST_ID with hosts/remove_proxmox_host.sh"
    fi
    if cluster_control test -e "/etc/pve/nodes/${HOST_ID}"; then
      fail "/etc/pve/nodes/${HOST_ID} still exists although $HOST_ID is not a member. Finish removing the previous $HOST_ID with hosts/remove_proxmox_host.sh before reusing the slot."
    fi
    reserve_registry_host_slot

    local expected_private
    expected_private="${PRIVATE_SUBNET_PREFIX}.$((MOX_IP_START_OCTET + ${EXISTING_NODE#mox} - 1))"
    existing_private="$(
      cluster_control ip -4 -o address show dev "$PROXMOX_PRIVATE_BRIDGE" |
        awk 'NR == 1 {sub(/\/.*/, "", $4); print $4}'
    )"
    [[ "$existing_private" == "$expected_private" ]] ||
      fail "${EXISTING_NODE} private VLAN address is ${existing_private:-missing}; expected ${expected_private}"
    existing_fqdn="${EXISTING_NODE}.${PROXMOX_INTERNAL_DOMAIN}"
    resolved_join_ip="$(
      remote getent ahostsv4 "$existing_fqdn" |
        awk 'NR == 1 { print $1 }'
    )"
    [[ "$resolved_join_ip" == "$existing_private" ]] ||
      fail "$existing_fqdn resolves to ${resolved_join_ip:-nothing} on $HOST_ID; expected private address $existing_private"
    fingerprint_output="$(
      # shellcheck disable=SC2016 # The script expands on the trusted control node.
      cluster_control bash -c '
        certificate=/etc/pve/local/pve-ssl.pem
        if [[ -s /etc/pve/local/pveproxy-ssl.pem ]]; then
          certificate=/etc/pve/local/pveproxy-ssl.pem
        fi
        openssl x509 -in "$certificate" -noout -sha256 -fingerprint
      '
    )" || fail "Could not obtain ${EXISTING_NODE}'s cluster certificate fingerprint through trusted SSH"
    cluster_fingerprint="${fingerprint_output#*=}"
    [[ "$cluster_fingerprint" =~ ^([0-9A-F]{2}:){31}[0-9A-F]{2}$ ]] ||
      fail "${EXISTING_NODE} returned an invalid SHA-256 cluster certificate fingerprint"
    served_fingerprint_output="$(
      remote_script "$existing_fqdn" <<'REMOTE'
set -Eeuo pipefail
fqdn="$1"
openssl s_client -connect "${fqdn}:8006" -servername "$fqdn" </dev/null 2>/dev/null |
  openssl x509 -noout -sha256 -fingerprint
REMOTE
    )" || fail "Could not verify the certificate served by $existing_fqdn:8006 over the private VLAN"
    served_fingerprint="${served_fingerprint_output#*=}"
    [[ "$served_fingerprint" == "$cluster_fingerprint" ]] ||
      fail "The certificate served by $existing_fqdn:8006 does not match the fingerprint obtained through trusted SSH"
    qd_require_access_for "$((expected_count + 1))"
    remove_qdevice_before_membership_change
    printf '\nThe Proxmox join command will use %s at verified private address %s.\n' \
      "$existing_fqdn" "$existing_private"
    info "Pinned ${EXISTING_NODE} X.509 fingerprint: $cluster_fingerprint"
    printf 'The command will prompt only for root@pam credentials.\n'
    local -a options
    mapfile -t options < <(ssh_options)
    local join_command="pvecm add '${existing_fqdn}' --fingerprint '${cluster_fingerprint}' --link0 'address=${private_ip},priority=100' --link1 'address=${ts_ip},priority=10'"
    local join_ok=true
    if bmac_ui_is_json; then
      # pvecm reads the password as one stdin line when stdin is not a TTY.
      local root_password=""
      bmac_ui_input root_password --title "Join ${HOST_ID} to the cluster" \
        --description "pvecm add on ${HOST_ID} logs in to ${existing_fqdn} with root@pam. The password is piped to pvecm once and never stored." \
        --id root_password --label "root@pam password for ${existing_fqdn}" \
        --type password --required
      printf '%s\n' "$root_password" |
        ssh -T "${options[@]}" "root@${TAILSCALE_IP}" "$join_command" || join_ok=false
      root_password=""
      unset root_password 'BMAC_UI_RESPONSE[root_password]'
    else
      ssh -tt "${options[@]}" "root@${TAILSCALE_IP}" "$join_command" || join_ok=false
    fi
    [[ "$join_ok" == true ]] ||
      {
        recover_qdevice_after_failed_join "$expected_count"
        fail "pvecm add failed for $HOST_ID"
      }
    enable_admin_ssh_for_target
    retire_setup_key_with_admin_identity
    wait_for_all_cluster_nodes_online "$((expected_count + 1))"
  fi

  remote_script "$PROXMOX_MIGRATION_NETWORK" <<'REMOTE'
set -Eeuo pipefail
network="$1"
config=/etc/pve/datacenter.cfg
touch "$config"
awk '!/^migration: / && !/^replication: /' "$config" >"${config}.new"
printf 'migration: secure,network=%s\n' "$network" >>"${config}.new"
printf 'replication: secure,network=%s\n' "$network" >>"${config}.new"
cat "${config}.new" >"$config"
rm -f "${config}.new"
pvecm status
REMOTE
  assert_all_cluster_nodes_online >/dev/null
  write_state cluster-configured
}

reconcile_private_cluster_ssh_trust() {
  local node_count node members
  local -a member_nodes=()
  node_count="$(assert_all_cluster_nodes_online)"
  members="$(cluster_member_nodes)" ||
    fail "Could not list cluster members through $(cluster_control_node)"
  mapfile -t member_nodes <<<"$members"
  ((${#member_nodes[@]} == node_count)) ||
    fail "Cluster status reports $node_count nodes but lists ${member_nodes[*]}"

  log "Refreshing Proxmox 9 per-node SSH pins and private mox trust"
  for node in "${member_nodes[@]}"; do
    ssh -o BatchMode=yes -o ClearAllForwardings=yes \
      -o StrictHostKeyChecking=yes -o CheckHostIP=yes \
      -o "ConnectTimeout=${MOX_SSH_CONNECT_TIMEOUT:-8}" \
      "root@${node}" pvecm updatecerts --unmerge-known-hosts
    ssh -o BatchMode=yes -o ClearAllForwardings=yes \
      -o StrictHostKeyChecking=yes -o CheckHostIP=yes \
      -o "ConnectTimeout=${MOX_SSH_CONNECT_TIMEOUT:-8}" \
      "root@${node}" test -s "/etc/pve/nodes/${node}/ssh_known_hosts" ||
      fail "Proxmox did not publish the per-node SSH pin for $node"
  done

  for node in "${member_nodes[@]}"; do
    ssh -o BatchMode=yes -o ClearAllForwardings=yes \
      -o StrictHostKeyChecking=yes -o CheckHostIP=yes \
      -o "ConnectTimeout=${MOX_SSH_CONNECT_TIMEOUT:-8}" \
      "root@${node}" bash -s -- "$PROXMOX_INTERNAL_DOMAIN" \
      "$PRIVATE_SUBNET_PREFIX" "$MOX_IP_START_OCTET" "${member_nodes[@]}" <<'REMOTE'
set -Eeuo pipefail
internal_domain="$1"; prefix="$2"; mox_start="$3"
shift 3
for peer_node in "$@"; do
  [[ "$peer_node" =~ ^mox([1-9]|10)$ ]]
  peer="${peer_node#mox}"
  peer_fqdn="${peer_node}.${internal_domain}"
  peer_ip="${prefix}.$((mox_start + peer - 1))"
  [[ "$(getent ahostsv4 "$peer_fqdn" | awk 'NR == 1 { print $1 }')" == "$peer_ip" ]]
  pin="/etc/pve/nodes/${peer_node}/ssh_known_hosts"
  [[ -s "$pin" && ! -L "$pin" ]]
  ssh -o BatchMode=yes -o ClearAllForwardings=yes \
    -o StrictHostKeyChecking=yes \
    -o CheckHostIP=no \
    -o "HostKeyAlias=${peer_node}" \
    -o "UserKnownHostsFile=${pin}" \
    -o GlobalKnownHostsFile=none "root@${peer_fqdn}" true </dev/null
done
REMOTE
  done
  write_state private-cluster-ssh-trust-configured \
    "${node_count}:${MOX_IP_START}:${PROXMOX_INTERNAL_DOMAIN}"
}

recover_qdevice_after_failed_join() {
  local expected_count=$1 status node_count
  ((QDEVICE_REMOVED_FOR_JOIN == 1)) || return
  status="$(cluster_control pvecm status 2>/dev/null || true)"
  node_count="$(awk '/^Nodes:/ { print $2; exit }' <<<"$status")"
  if [[ "$node_count" != "$expected_count" ]]; then
    printf 'QDevice was not restored because cluster membership changed during the failed join (expected %s nodes, found %s).\n' \
      "$expected_count" "${node_count:-unknown}" >&2
    return
  fi
  if [[ "$QD_REMOVAL" == forced ]]; then
    printf 'The QDevice was removed forcefully, so it cannot be restored. Add a replacement with qdevice/add_qdevice.sh.\n' >&2
    return
  fi
  assert_all_cluster_nodes_online "$expected_count" >/dev/null
  log "Restoring QDevice after failed join"
  qd_add "$expected_count"
  QDEVICE_REMOVED_FOR_JOIN=0
}

reconcile_qdevice() {
  local node_count
  node_count="$(assert_all_cluster_nodes_online)"
  qd_reconcile "$node_count"
  if ((node_count % 2 == 1)); then
    write_state qdevice-configured "absent-for-${node_count}-node-cluster"
    return
  fi
  cluster_control pvecm status | tee "${LOG_DIR}/cluster-with-qdevice.txt"
  write_state qdevice-configured "present-for-${node_count}-node-cluster"
}

# A joining host may have started before its control node created the cluster.
# Wait outside the control-plane locks, which the control node itself needs in
# order to create the cluster.
wait_for_control_node_cluster() {
  local deadline=$((SECONDS + 1800))
  [[ "$SETUP_ROLE" == join && "$EXISTING_NODE" != "$HOST_ID" ]] || return 0
  cluster_control test -s /etc/pve/corosync.conf </dev/null 2>/dev/null && return 0
  info "Waiting for control node $EXISTING_NODE to be reachable and to create the cluster..."
  until cluster_control test -s /etc/pve/corosync.conf </dev/null 2>/dev/null; do
    ((SECONDS < deadline)) ||
      fail "Control node $EXISTING_NODE did not create a reachable cluster within 30 minutes. Let its setup create the cluster, verify this workstation can SSH to root@${EXISTING_NODE}, then rerun to resume."
    sleep 15
  done
  info "Control node $EXISTING_NODE has created the cluster."
}

reconcile_cluster_control_plane() {
  local cluster_lock_file="${ARTIFACTS_DIR}/cluster-control-plane.lock"
  local cluster_lock_fd remote_lock_command lock_host="$CONTROL_NODE"
  [[ "$lock_host" =~ ^mox([1-9]|10)$ ]] ||
    fail "The cluster control node is unknown; cannot take the control-plane lock"
  wait_for_control_node_cluster
  exec {cluster_lock_fd}>"$cluster_lock_file"
  chmod 0600 "$cluster_lock_file"
  info "Waiting for exclusive cluster membership, SSH-trust, and QDevice reconciliation..."
  flock -w 1800 "$cluster_lock_fd" ||
    fail "Timed out waiting 30 minutes for another host installer to finish cluster control-plane changes"
  REMOTE_CLUSTER_LOCK_TOKEN="$(
    printf '%s:%s:%s' "$HOST_ID" "$(date -u +%Y%m%dT%H%M%SZ)" \
      "$(openssl rand -hex 16)"
  )"
  # shellcheck disable=SC2016 # The lock program expands only on the lock host.
  printf -v remote_lock_command '%q ' bash -c '
    set -Eeuo pipefail
    token="$1"
    host="$2"
    lease=/run/lock/app-ha-cluster-control-plane.lease
    lock=/run/lock/app-ha-cluster-control-plane.lock
    if ! mkdir "$lease" 2>/dev/null; then
      owner="$(cat "$lease/owner" 2>/dev/null || printf unknown)"
      printf "Cluster control-plane lease already exists on %s (owner: %s). Verify that no installer is active; if it is stale, remove %s manually on %s.\n" \
        "$host" "$owner" "$lease" "$host" >&2
      exit 1
    fi
    printf "%s\n" "$token" >"$lease/owner"
    chmod 0600 "$lease/owner"
    exec 9>"$lock"
    flock -w 1800 9 || exit 1
    printf "LOCKED\n"
    release=""
    IFS= read -r release || exit 2
    [[ "$release" == "RELEASE:${token}" ]] || exit 3
    rm -rf -- "$lease"
  ' _ "$REMOTE_CLUSTER_LOCK_TOKEN" "$lock_host"
  coproc REMOTE_CLUSTER_LOCK_PROCESS {
    ssh -o BatchMode=yes -o ClearAllForwardings=yes \
      -o StrictHostKeyChecking=yes -o CheckHostIP=yes \
      -o "ConnectTimeout=${MOX_SSH_CONNECT_TIMEOUT:-8}" "root@${lock_host}" \
      "$remote_lock_command"
  }
  REMOTE_CLUSTER_LOCK_READ_FD="${REMOTE_CLUSTER_LOCK_PROCESS[0]}"
  REMOTE_CLUSTER_LOCK_WRITE_FD="${REMOTE_CLUSTER_LOCK_PROCESS[1]}"
  REMOTE_CLUSTER_LOCK_PID="$REMOTE_CLUSTER_LOCK_PROCESS_PID"
  local remote_lock_status=""
  IFS= read -r -u "$REMOTE_CLUSTER_LOCK_READ_FD" remote_lock_status ||
    fail "Could not acquire the cluster control-plane lock hosted on ${lock_host}"
  [[ "$remote_lock_status" == LOCKED ]] ||
    fail "Unexpected response while acquiring the ${lock_host} cluster control-plane lock"
  CLUSTER_CONTROL_LOCK_HELD=1
  configure_cluster_host_resolution
  cluster_setup
  reconcile_private_cluster_ssh_trust
  reconcile_qdevice
  CLUSTER_CONTROL_LOCK_HELD=0
  release_remote_cluster_control_lock
  flock -u "$cluster_lock_fd"
  exec {cluster_lock_fd}>&-
}

configure_guest_egress() {
  log "Configuring the quorum-aware floating guest egress gateway"
  remote_apt_script "$HOST_ID" "$MOX_INDEX" "$PROXMOX_SECONDARY_IP" \
    "$GUEST_EGRESS_VIP" "$PROXMOX_PRIVATE_BRIDGE" "$PROXMOX_IP" \
    "$PRIVATE_SUBNET_PREFIX" "$MOX_IP_START_OCTET" "$MAX_MOX_HOSTS" "$VRRP_PRIORITY" \
    "$PRODUCTION_IP_START" "$STAGING_IP_END" <<'REMOTE'
set -Eeuo pipefail
host_id="$1"; host_index="$2"; private_cidr="$3"; vip_cidr="$4"
bridge="$5"; public_ip="$6"; subnet_prefix="$7"; mox_start="$8"; max_hosts="$9"
priority="${10}"; production_start="${11}"; staging_end="${12}"
private_ip="${private_cidr%/*}"
vip_ip="${vip_cidr%/*}"

[[ "$host_id" == "mox${host_index}" ]]
[[ "$host_index" =~ ^([1-9]|10)$ && "$max_hosts" =~ ^([1-9]|10)$ ]]
((host_index <= max_hosts))
((max_hosts >= 2))
[[ "$private_ip" == "${subnet_prefix}.$((mox_start + host_index - 1))" ]]
[[ "$priority" =~ ^[1-9][0-9]*$ && "$priority" -le 255 ]]

peer_lines=""
peer_set=""
peer_count=0
for ((index = 1; index <= max_hosts; index += 1)); do
  candidate="${subnet_prefix}.$((mox_start + index - 1))"
  [[ "$candidate" == "$private_ip" ]] && continue
  peer_lines+="        ${candidate}"$'\n'
  peer_set+="${candidate}, "
  peer_count=$((peer_count + 1))
done
((peer_count == max_hosts - 1))
peer_set="${peer_set%, }"

public_if="$(ip -4 route get 1.1.1.1 | awk '{for (i=1;i<=NF;i++) if ($i=="dev") {print $(i+1); exit}}')"
[[ -n "$public_if" && "$public_if" != "$bridge" ]]
ip -4 -o address show dev "$public_if" |
  awk -v ip="$public_ip" '$4 ~ ("^" ip "/") { found=1 } END { exit !found }'

apt_wait_for_locks
apt-get -o DPkg::Lock::Timeout=60 update
DEBIAN_FRONTEND=noninteractive apt-get -o DPkg::Lock::Timeout=60 install -y keepalived

cat >/etc/sysctl.d/91-app-ha-guest-egress.conf <<'EOF'
net.ipv4.ip_forward=1
EOF
sysctl --system >/dev/null

install -d -m 0755 /etc/nftables.d
cat >/etc/nftables.d/app-ha-guest-egress.nft <<EOF
table ip app_ha_guest_egress {
  chain vrrp_input {
    type filter hook input priority -190; policy accept;
    iifname "$bridge" ip protocol 112 ip saddr { $peer_set } ip daddr $private_ip counter accept
    iifname "$bridge" ip protocol 112 counter drop
  }

  chain vrrp_output {
    type filter hook output priority -190; policy accept;
    oifname "$bridge" ip protocol 112 ip saddr $private_ip ip daddr { $peer_set } counter accept
    oifname "$bridge" ip protocol 112 counter drop
  }

  chain forward {
    type filter hook forward priority -190; policy accept;
    iifname != "$bridge" ip saddr $production_start-$staging_end counter drop
    iifname "$bridge" oifname "$public_if" ip saddr $production_start-$staging_end counter accept
    iifname "$public_if" oifname "$bridge" ip daddr $production_start-$staging_end ct state established,related counter accept
  }

  chain postrouting {
    type nat hook postrouting priority srcnat; policy accept;
    iifname "$bridge" oifname "$public_if" ip saddr $production_start-$staging_end masquerade
  }
}
EOF

# Reconcile Proxmox host-firewall rules as well as the nft guard. An nft
# accept verdict is not final if a later Proxmox firewall chain rejects VRRP.
# Proxmox's pve-iface validator rejects hyphens even though Linux and its VM
# bridge parser accept them, so interface binding remains in the earlier nft
# chains while these rules repeat the exact protocol/address constraints.
rules_json="$(pvesh get "/nodes/$host_id/firewall/rules" --output-format json)"
while IFS= read -r position; do
  pvesh delete "/nodes/$host_id/firewall/rules/$position"
done < <(
  jq -r '
    [.[] | select(
      ((.comment // "") | startswith("app-ha managed VRRP peer ")) or
      (.comment // "") == "app-ha managed guest egress forward"
    )] |
    sort_by(.pos) | reverse | .[].pos
  ' <<<"$rules_json"
)
for ((index = 1; index <= max_hosts; index += 1)); do
  candidate="${subnet_prefix}.$((mox_start + index - 1))"
  [[ "$candidate" == "$private_ip" ]] && continue
  pvesh create "/nodes/$host_id/firewall/rules" \
    --pos 0 --type in --action ACCEPT --enable 1 \
    --proto 112 --source "$candidate" --dest "$private_ip" \
    --comment "app-ha managed VRRP peer $candidate inbound"
  pvesh create "/nodes/$host_id/firewall/rules" \
    --pos 0 --type out --action ACCEPT --enable 1 \
    --proto 112 --source "$private_ip" --dest "$candidate" \
    --comment "app-ha managed VRRP peer $candidate outbound"
done
pvesh create "/nodes/$host_id/firewall/rules" \
  --pos 0 --type forward --action ACCEPT --enable 1 \
  --source "$production_start-$staging_end" \
  --comment "app-ha managed guest egress forward"
rules_json="$(pvesh get "/nodes/$host_id/firewall/rules" --output-format json)"
managed_rule_count="$(
  jq -r '
    [.[] | select(
      ((.comment // "") | startswith("app-ha managed VRRP peer ")) and
      .action == "ACCEPT" and (.enable | tostring) == "1" and
      (.proto | tostring) == "112"
    )] | length
  ' <<<"$rules_json"
)"
((managed_rule_count == peer_count * 2))
jq -e --arg source "$production_start-$staging_end" '
  any(.[];
    (.comment // "") == "app-ha managed guest egress forward" and
    .type == "forward" and .action == "ACCEPT" and
    (.enable | tostring) == "1" and
    (.iface // "") == "" and .source == $source
  )
' <<<"$rules_json" >/dev/null

cat >/usr/local/sbin/app-ha-load-guest-egress <<'EOF'
#!/usr/bin/env bash
set -Eeuo pipefail
nft list table ip app_ha_guest_egress >/dev/null 2>&1 &&
  nft delete table ip app_ha_guest_egress
nft -f /etc/nftables.d/app-ha-guest-egress.nft
EOF
chmod 0755 /usr/local/sbin/app-ha-load-guest-egress

cat >/etc/systemd/system/app-ha-guest-egress.service <<'EOF'
[Unit]
Description=app-ha production and staging guest egress NAT
After=network-online.target
Wants=network-online.target

[Service]
Type=oneshot
ExecStart=/usr/local/sbin/app-ha-load-guest-egress
RemainAfterExit=yes

[Install]
WantedBy=multi-user.target
EOF

cat >/usr/local/sbin/app-ha-check-egress-router <<EOF
#!/usr/bin/env bash
set -Eeuo pipefail
[[ "\$(sysctl -n net.ipv4.ip_forward)" == 1 ]]
ip -br link show dev "$bridge" |
  awk '\$2 == "UP" { found=1 } END { exit !found }'
ip -br link show dev "$public_if" |
  awk '\$2 == "UP" { found=1 } END { exit !found }'
ip -4 route get 1.1.1.1 |
  awk -v dev="$public_if" '{ for (i=1; i<=NF; i++) if (\$i == "dev" && \$(i+1) == dev) found=1 } END { exit !found }'
ip -4 -o address show dev "$public_if" |
  awk -v ip="$public_ip" '\$4 ~ ("^" ip "/") { found=1 } END { exit !found }'
nft list table ip app_ha_guest_egress >/dev/null
nft -nn list chain ip app_ha_guest_egress vrrp_input |
  grep -Fq 'ip protocol 112'
nft -nn list chain ip app_ha_guest_egress vrrp_output |
  grep -Fq 'ip protocol 112'
nft list chain ip app_ha_guest_egress forward |
  grep -Fq 'ct state established,related'
nft list chain ip app_ha_guest_egress postrouting |
  grep -Fq 'masquerade'
! nft -nn list ruleset |
  awk '/hook forward/ && /policy drop/ { found=1 } END { exit !found }'
if command -v iptables >/dev/null 2>&1; then
  ! iptables -w -S FORWARD |
    awk '\$1 == "-P" && \$2 == "FORWARD" && \$3 == "DROP" { found=1 } END { exit !found }'
fi
pvecm status 2>/dev/null |
  awk '\$1 == "Quorate:" && \$2 == "Yes" { found=1 } END { exit !found }'
curl -4 --interface "$public_ip" --connect-timeout 3 --max-time 4 \
  --fail --silent --show-error https://1.1.1.1/cdn-cgi/trace >/dev/null
EOF
chmod 0755 /usr/local/sbin/app-ha-check-egress-router

cat >/usr/local/sbin/app-ha-egress-vrrp-notify <<'EOF'
#!/usr/bin/env bash
set -Eeuo pipefail
state="${1:?VRRP state is required}"
logger -t app-ha-egress "guest egress gateway entered ${state}"
EOF
chmod 0755 /usr/local/sbin/app-ha-egress-vrrp-notify

cat >/etc/keepalived/keepalived.conf <<EOF
global_defs {
    router_id $host_id
    enable_script_security
    script_user root
    vrrp_check_unicast_src
}

vrrp_script app_ha_egress_ready {
    script "/usr/local/sbin/app-ha-check-egress-router"
    interval 15
    timeout 6
    fall 3
    rise 2
}

vrrp_instance APP_HA_GUEST_EGRESS {
    state BACKUP
    interface $bridge
    virtual_router_id 10
    priority $priority
    advert_int 1
    nopreempt

    unicast_src_ip $private_ip
    unicast_peer {
$peer_lines    }

    virtual_ipaddress {
        $vip_cidr dev $bridge
    }

    track_interface {
        $public_if
    }

    track_script {
        app_ha_egress_ready
    }

    garp_master_delay 1
    garp_master_repeat 5
    garp_master_refresh 60
    garp_master_refresh_repeat 2

    notify_master "/usr/local/sbin/app-ha-egress-vrrp-notify MASTER"
    notify_backup "/usr/local/sbin/app-ha-egress-vrrp-notify BACKUP"
    notify_fault "/usr/local/sbin/app-ha-egress-vrrp-notify FAULT"
}
EOF

install -d -m 0755 /etc/systemd/system/keepalived.service.d
cat >/etc/systemd/system/keepalived.service.d/app-ha.conf <<'EOF'
[Unit]
After=app-ha-guest-egress.service pve-cluster.service
Requires=app-ha-guest-egress.service
EOF

systemctl daemon-reload
systemctl enable app-ha-guest-egress.service
systemctl restart app-ha-guest-egress.service
keepalived --config-test --use-file=/etc/keepalived/keepalived.conf
systemctl enable keepalived.service
systemctl restart keepalived.service

systemctl is-active app-ha-guest-egress.service
systemctl is-active keepalived.service
nft list table ip app_ha_guest_egress
ip -4 address show dev "$bridge"
printf 'Floating gateway %s is managed by Keepalived; this node is priority %s.\n' "$vip_ip" "$priority"
REMOTE
  write_state guest-egress-configured
}

install_guest_role_hook() {
  log "Verifying the generic production/staging VM lifecycle guard"
  remote_script "$GUEST_ROLE_HOOK_NAME" "$GUEST_ROLE_HOOK_COMPAT_NAME" <<'REMOTE'
set -Eeuo pipefail
hook_name="$1"; compat_name="$2"
[[ "$hook_name" =~ ^[A-Za-z0-9._-]+\.sh$ ]]
[[ -z "$compat_name" || "$compat_name" =~ ^[A-Za-z0-9._-]+\.sh$ ]]
test -x "/var/lib/vz/snippets/$hook_name"
test -x /usr/local/lib/app-ha-proxmox/lib/process_deferred_cleanup.sh
test -x /usr/local/lib/app-ha-proxmox/lib/cluster_registry.py
test -x /usr/local/lib/app-ha-proxmox/guests/prod/build_ubuntu_autoinstall.py
test -x /usr/local/lib/app-ha-proxmox/guests/prod/prepare_prod_iso.sh
test -d /var/lib/app-ha-proxmox/iso-cache
[[ "$(stat -c %a /var/lib/app-ha-proxmox/iso-cache)" == 700 ]]
test -f /usr/local/lib/app-ha-proxmox/env/cluster.conf
bash -n "/var/lib/vz/snippets/$hook_name" \
  /usr/local/lib/app-ha-proxmox/lib/process_deferred_cleanup.sh \
  /usr/local/lib/app-ha-proxmox/guests/prod/prepare_prod_iso.sh
pvesm path "local:snippets/$hook_name"
if [[ -n "$compat_name" ]]; then
  test -x "/var/lib/vz/snippets/$compat_name"
  cmp -s "/var/lib/vz/snippets/$hook_name" \
    "/var/lib/vz/snippets/$compat_name"
fi
systemctl is-enabled --quiet app-ha-deferred-cleanup.timer
systemctl is-active --quiet app-ha-deferred-cleanup.timer
REMOTE
  write_state guest-role-hook-installed
}

install_shared_orchestration_tools() {
  log "Installing shared app-ha registry, ingress, and cleanup tools on online mox nodes"
  local remote_stage="/tmp/app-ha-orchestration-${CONFIG_EFFECTIVE_SHA256:0:16}"
  remote rm -rf "$remote_stage"
  remote install -d -m 0700 "$remote_stage"
  copy_to_host "$CONFIG_LIB" "${remote_stage}/config.sh"
  copy_to_host "$CLUSTER_CONFIG_SOURCE" "${remote_stage}/cluster.conf"
  copy_to_host "$CLUSTER_REGISTRY_SOURCE" "${remote_stage}/cluster_registry.py"
  copy_to_host "$HAPROXY_RENDERER_SOURCE" "${remote_stage}/haproxy_routes.py"
  copy_to_host "$HAPROXY_SYNC_SOURCE" "${remote_stage}/sync_haproxy_routes.sh"
  copy_to_host "$DEFERRED_CLEANUP_SOURCE" \
    "${remote_stage}/process_deferred_cleanup.sh"
  copy_to_host "$PROD_ISO_BUILDER_SOURCE" \
    "${remote_stage}/build_ubuntu_autoinstall.py"
  copy_to_host "$PROD_ISO_PREPARER_SOURCE" \
    "${remote_stage}/prepare_prod_iso.sh"
  copy_to_host "$GUEST_ROLE_HOOK_SOURCE" \
    "${remote_stage}/${GUEST_ROLE_HOOK_NAME}"

  remote_apt_script "$remote_stage" "$CLUSTER_STATE_DIR" "$PRIVATE_SUBNET_CIDR" \
    "$GUEST_EGRESS_VIP" "$MAX_MOX_HOSTS" "$PRODUCTION_VM_TAG" \
    "$STAGING_VM_TAG" "$EVICTABLE_VM_TAG" "$PROD_VM_CORES" \
    "$((PROD_VM_MEMORY_GIB * 1024))" "$PROD_VM_DISK_GB" "$PROD_VM_REPLICATION_INTERVAL" \
    "$STAGING_VM_CORES" "$((STAGING_VM_MEMORY_GIB * 1024))" \
    "${STAGING_VM_DISK_GB:-$PROD_VM_DISK_GB}" \
    "${MOX_SSH_CONNECT_TIMEOUT:-8}" "$GUEST_ROLE_HOOK_NAME" \
    "$GUEST_ROLE_HOOK_COMPAT_NAME" "$PROXMOX_INTERNAL_DOMAIN" \
    "$MOX_IP_START" "$MOX_IP_END" "$HAPROXY_IP_START" "$HAPROXY_IP_END" \
    "$PRODUCTION_IP_START" "$PRODUCTION_IP_END" \
    "$STAGING_IP_START" "$STAGING_IP_END" "$CONTROL_NODE" <<'REMOTE'
set -Eeuo pipefail
source_dir="$1"; state_dir="$2"; network="$3"; guest_gateway="$4"
max_hosts="$5"; production_tag="$6"; staging_tag="$7"; evictable_tag="$8"
prod_cores="$9"; prod_memory="${10}"; prod_disk="${11}"
replication_schedule="${12}"; staging_cores="${13}"; staging_memory="${14}"
staging_disk="${15}"; connect_timeout="${16}"; hook_name="${17}"
compat_hook_name="${18}"
internal_domain="${19}"
mox_ip_start="${20}"; mox_ip_end="${21}"
haproxy_ip_start="${22}"; haproxy_ip_end="${23}"
production_ip_start="${24}"; production_ip_end="${25}"
staging_ip_start="${26}"; staging_ip_end="${27}"
control_node="${28}"
[[ "$control_node" =~ ^mox([1-9]|10)$ ]]
install_root=/usr/local/lib/app-ha-proxmox

if ! command -v fuser >/dev/null 2>&1 ||
   ! command -v findmnt >/dev/null 2>&1 ||
   ! command -v flock >/dev/null 2>&1 ||
   ! command -v lsblk >/dev/null 2>&1 ||
   ! command -v curl >/dev/null 2>&1 ||
   ! command -v xorriso >/dev/null 2>&1; then
  apt_wait_for_locks
  apt-get -o DPkg::Lock::Timeout=60 update
  DEBIAN_FRONTEND=noninteractive apt-get -o DPkg::Lock::Timeout=60 install -y \
    curl psmisc util-linux xorriso
fi
for command_name in bash curl findmnt flock fuser jq lsblk pvesh pvesm \
  python3 qm scp sha256sum ssh systemctl tar xorriso zfs; do
  command -v "$command_name" >/dev/null 2>&1 || {
    printf 'Required orchestration command is unavailable: %s\n' "$command_name" >&2
    exit 1
  }
done
for source in config.sh cluster.conf cluster_registry.py haproxy_routes.py \
  sync_haproxy_routes.sh process_deferred_cleanup.sh \
  build_ubuntu_autoinstall.py prepare_prod_iso.sh "$hook_name"; do
  [[ -f "$source_dir/$source" && ! -L "$source_dir/$source" ]]
done
bash -n "$source_dir/config.sh" "$source_dir/sync_haproxy_routes.sh" \
  "$source_dir/process_deferred_cleanup.sh" "$source_dir/prepare_prod_iso.sh" \
  "$source_dir/$hook_name"
PYTHONDONTWRITEBYTECODE=1 python3 "$source_dir/cluster_registry.py" --help >/dev/null
PYTHONDONTWRITEBYTECODE=1 python3 "$source_dir/haproxy_routes.py" --help >/dev/null
PYTHONDONTWRITEBYTECODE=1 \
  python3 "$source_dir/build_ubuntu_autoinstall.py" --help >/dev/null

install_tree() {
  local source_root="$1"
  install -d -m 0755 \
    "$install_root/lib" "$install_root/env" "$install_root/guests/prod"
  install -d -o root -g root -m 0700 /var/lib/app-ha-proxmox/iso-cache
  install -o root -g root -m 0755 "$source_root/config.sh" \
    "$install_root/lib/.config.sh.new"
  install -o root -g root -m 0755 "$source_root/cluster_registry.py" \
    "$install_root/lib/.cluster_registry.py.new"
  install -o root -g root -m 0755 "$source_root/haproxy_routes.py" \
    "$install_root/lib/.haproxy_routes.py.new"
  install -o root -g root -m 0755 "$source_root/sync_haproxy_routes.sh" \
    "$install_root/lib/.sync_haproxy_routes.sh.new"
  install -o root -g root -m 0755 \
    "$source_root/process_deferred_cleanup.sh" \
    "$install_root/lib/.process_deferred_cleanup.sh.new"
  install -o root -g root -m 0755 "$source_root/$hook_name" \
    "$install_root/lib/.${hook_name}.new"
  install -o root -g root -m 0755 \
    "$source_root/build_ubuntu_autoinstall.py" \
    "$install_root/guests/prod/.build_ubuntu_autoinstall.py.new"
  install -o root -g root -m 0755 "$source_root/prepare_prod_iso.sh" \
    "$install_root/guests/prod/.prepare_prod_iso.sh.new"
  install -o root -g root -m 0644 "$source_root/cluster.conf" \
    "$install_root/env/.cluster.conf.new"
  mv -f "$install_root/lib/.config.sh.new" "$install_root/lib/config.sh"
  mv -f "$install_root/lib/.cluster_registry.py.new" \
    "$install_root/lib/cluster_registry.py"
  mv -f "$install_root/lib/.haproxy_routes.py.new" \
    "$install_root/lib/haproxy_routes.py"
  mv -f "$install_root/lib/.sync_haproxy_routes.sh.new" \
    "$install_root/lib/sync_haproxy_routes.sh"
  mv -f "$install_root/lib/.process_deferred_cleanup.sh.new" \
    "$install_root/lib/process_deferred_cleanup.sh"
  mv -f "$install_root/lib/.${hook_name}.new" \
    "$install_root/lib/$hook_name"
  mv -f "$install_root/guests/prod/.build_ubuntu_autoinstall.py.new" \
    "$install_root/guests/prod/build_ubuntu_autoinstall.py"
  mv -f "$install_root/guests/prod/.prepare_prod_iso.sh.new" \
    "$install_root/guests/prod/prepare_prod_iso.sh"
  mv -f "$install_root/env/.cluster.conf.new" "$install_root/env/cluster.conf"
}

install_tree "$source_dir"
"$install_root/lib/config.sh" --check --no-secrets >/dev/null

install_runtime() {
  local content
  content="$(
    pvesh get /storage/local --output-format json |
      jq -er '.content | select(type == "string" and length > 0)'
  )"
  [[ -n "$content" ]]
  case ",${content}," in
    *,snippets,*) ;;
    *) pvesm set local --content "${content},snippets" ;;
  esac
  install -d -m 0755 /var/lib/vz/snippets
  install -o root -g root -m 0755 "$install_root/lib/$hook_name" \
    "/var/lib/vz/snippets/$hook_name"
  if [[ -n "$compat_hook_name" ]]; then
    [[ "$compat_hook_name" =~ ^[A-Za-z0-9._-]+[.]sh$ ]]
    install -o root -g root -m 0755 "$install_root/lib/$hook_name" \
      "/var/lib/vz/snippets/$compat_hook_name"
  fi
  cat >/etc/systemd/system/app-ha-deferred-cleanup.service <<EOF
[Unit]
Description=Process deferred app-ha staging cleanup
After=pve-cluster.service
Wants=pve-cluster.service

[Service]
Type=oneshot
ExecStart=$install_root/lib/process_deferred_cleanup.sh --max-seconds 120
TimeoutStartSec=150
Nice=10
IOSchedulingClass=idle
EOF
  cat >/etc/systemd/system/app-ha-deferred-cleanup.timer <<'EOF'
[Unit]
Description=Retry deferred app-ha staging cleanup

[Timer]
OnBootSec=2min
OnUnitActiveSec=2min
RandomizedDelaySec=30s
AccuracySec=15s
Persistent=false

[Install]
WantedBy=timers.target
EOF
  cat >/etc/systemd/system/app-ha-haproxy-route-sync.service <<EOF
[Unit]
Description=Converge and fail-close local app-ha HAProxy ingress
After=network-online.target pve-cluster.service
Wants=network-online.target pve-cluster.service
Before=app-ha-haproxy-ingress.service

[Service]
Type=oneshot
ExecStart=$install_root/lib/sync_haproxy_routes.sh --local-reconcile
TimeoutStartSec=180
UMask=0077
EOF
  cat >/etc/systemd/system/app-ha-haproxy-route-sync.timer <<'EOF'
[Unit]
Description=Continuously reconcile app-ha HAProxy ingress

[Timer]
OnBootSec=20s
OnUnitActiveSec=1min
RandomizedDelaySec=10s
AccuracySec=5s
Persistent=false
Unit=app-ha-haproxy-route-sync.service

[Install]
WantedBy=timers.target
EOF
  install -d -m 0755 \
    /etc/systemd/system/app-ha-haproxy-ingress.service.d
  cat >/etc/systemd/system/app-ha-haproxy-ingress.service.d/route-sync.conf <<EOF
[Unit]
After=app-ha-haproxy-route-sync.service
Wants=app-ha-haproxy-route-sync.service

[Service]
ExecStartPre=
ExecStartPre=$install_root/lib/sync_haproxy_routes.sh --assert-local-current
EOF
  chmod 0644 /etc/systemd/system/app-ha-deferred-cleanup.service \
    /etc/systemd/system/app-ha-deferred-cleanup.timer \
    /etc/systemd/system/app-ha-haproxy-route-sync.service \
    /etc/systemd/system/app-ha-haproxy-route-sync.timer \
    /etc/systemd/system/app-ha-haproxy-ingress.service.d/route-sync.conf
  systemctl daemon-reload
  systemctl enable --now app-ha-deferred-cleanup.timer
  systemctl enable app-ha-haproxy-route-sync.timer
  pvesm path "local:snippets/$hook_name" >/dev/null
}

install_runtime

bundle="$(mktemp /run/app-ha-orchestration.XXXXXX.tar)"
trap 'rm -f "$bundle"; rm -rf "$source_dir"' EXIT
tar -C "$install_root" -cf "$bundle" lib/config.sh lib/cluster_registry.py \
  lib/haproxy_routes.py lib/sync_haproxy_routes.sh \
  lib/process_deferred_cleanup.sh "lib/$hook_name" \
  guests/prod/build_ubuntu_autoinstall.py \
  guests/prod/prepare_prod_iso.sh env/cluster.conf

nodes_json="$(pvesh get /nodes --output-format json)"
unexpected="$(jq -r --argjson maximum "$max_hosts" '
  [.[] | select(
    .status == "online" and
    (if (.node | test("^mox([1-9]|10)$"))
     then ((.node | sub("^mox"; "") | tonumber) > $maximum)
     else true
     end)
  ) | .node] | sort | join(", ")
' <<<"$nodes_json")"
[[ -z "$unexpected" ]] || {
  printf 'Unexpected online Proxmox nodes: %s\n' "$unexpected" >&2
  exit 1
}

current_node="$(hostname -s)"
[[ "$current_node" =~ ^mox([1-9]|10)$ ]]
ssh_common_options=(
  -o BatchMode=yes
  -o ClearAllForwardings=yes
  -o "ConnectTimeout=$connect_timeout"
  -o ServerAliveInterval=15
  -o ServerAliveCountMax=2
  -o StrictHostKeyChecking=yes
  -o CheckHostIP=no
  -o GlobalKnownHostsFile=none
)
for ((index = 1; index <= max_hosts; index += 1)); do
  node="mox${index}"
  jq -e --arg node "$node" \
    'any(.[]; .node == $node and .status == "online")' \
    <<<"$nodes_json" >/dev/null || continue
  [[ "$node" != "$current_node" ]] || continue
  destination="${node}.${internal_domain}"
  pin="/etc/pve/nodes/${node}/ssh_known_hosts"
  [[ -s "$pin" && ! -L "$pin" ]]
  ssh_options=(
    "${ssh_common_options[@]}"
    -o "HostKeyAlias=${node}"
    -o "UserKnownHostsFile=${pin}"
  )
  remote_bundle="/run/app-ha-orchestration-${current_node}-$$.tar"
  [[ "$remote_bundle" =~ ^/run/app-ha-orchestration-mox([1-9]|10)-[0-9]+[.]tar$ ]]
  scp "${ssh_options[@]}" "$bundle" "root@${destination}:${remote_bundle}"
  printf -v remote_command '%q ' bash -s -- \
    "$remote_bundle" "$install_root" "$hook_name" "$compat_hook_name"
  ssh "${ssh_options[@]}" "root@${destination}" "$remote_command" < <(
    declare -f apt_lock_holder apt_wait_for_locks
    cat <<'NODE'
set -Eeuo pipefail
bundle="$1"; install_root="$2"; hook_name="$3"; compat_hook_name="$4"
if ! command -v fuser >/dev/null 2>&1 ||
   ! command -v findmnt >/dev/null 2>&1 ||
   ! command -v flock >/dev/null 2>&1 ||
   ! command -v lsblk >/dev/null 2>&1 ||
   ! command -v curl >/dev/null 2>&1 ||
   ! command -v xorriso >/dev/null 2>&1; then
  apt_wait_for_locks
  apt-get -o DPkg::Lock::Timeout=60 update
  DEBIAN_FRONTEND=noninteractive apt-get -o DPkg::Lock::Timeout=60 install -y \
    curl psmisc util-linux xorriso
fi
for command_name in bash curl findmnt flock fuser lsblk pvesh pvesm python3 \
  qm sha256sum systemctl tar xorriso zfs; do
  command -v "$command_name" >/dev/null 2>&1 || {
    printf 'Required cleanup command is unavailable: %s\n' "$command_name" >&2
    exit 1
  }
done
incoming="$(mktemp -d /run/app-ha-orchestration.XXXXXX)"
trap 'rm -rf "$incoming"; rm -f "$bundle"' EXIT
tar -xf "$bundle" -C "$incoming"
for source in lib/config.sh lib/cluster_registry.py lib/haproxy_routes.py \
  lib/sync_haproxy_routes.sh lib/process_deferred_cleanup.sh \
  "lib/$hook_name" guests/prod/build_ubuntu_autoinstall.py \
  guests/prod/prepare_prod_iso.sh env/cluster.conf; do
  [[ -f "$incoming/$source" && ! -L "$incoming/$source" ]]
done
bash -n "$incoming/lib/config.sh" "$incoming/lib/sync_haproxy_routes.sh" \
  "$incoming/lib/process_deferred_cleanup.sh" \
  "$incoming/guests/prod/prepare_prod_iso.sh" "$incoming/lib/$hook_name"
PYTHONDONTWRITEBYTECODE=1 python3 "$incoming/lib/cluster_registry.py" --help >/dev/null
PYTHONDONTWRITEBYTECODE=1 python3 "$incoming/lib/haproxy_routes.py" --help >/dev/null
PYTHONDONTWRITEBYTECODE=1 \
  python3 "$incoming/guests/prod/build_ubuntu_autoinstall.py" --help >/dev/null
install -d -m 0755 \
  "$install_root/lib" "$install_root/env" "$install_root/guests/prod"
install -d -o root -g root -m 0700 /var/lib/app-ha-proxmox/iso-cache
for name in config.sh cluster_registry.py haproxy_routes.py \
  sync_haproxy_routes.sh process_deferred_cleanup.sh "$hook_name"; do
  mode=0755
  install -o root -g root -m "$mode" "$incoming/lib/$name" \
    "$install_root/lib/.${name}.new"
  mv -f "$install_root/lib/.${name}.new" "$install_root/lib/$name"
done
for name in build_ubuntu_autoinstall.py prepare_prod_iso.sh; do
  install -o root -g root -m 0755 "$incoming/guests/prod/$name" \
    "$install_root/guests/prod/.${name}.new"
  mv -f "$install_root/guests/prod/.${name}.new" \
    "$install_root/guests/prod/$name"
done
install -o root -g root -m 0644 "$incoming/env/cluster.conf" \
  "$install_root/env/.cluster.conf.new"
mv -f "$install_root/env/.cluster.conf.new" "$install_root/env/cluster.conf"
"$install_root/lib/config.sh" --check --no-secrets >/dev/null

content="$(
  pvesh get /storage/local --output-format json |
    jq -er '.content | select(type == "string" and length > 0)'
)"
[[ -n "$content" ]]
case ",${content}," in
  *,snippets,*) ;;
  *) pvesm set local --content "${content},snippets" ;;
esac
install -d -m 0755 /var/lib/vz/snippets
install -o root -g root -m 0755 "$install_root/lib/$hook_name" \
  "/var/lib/vz/snippets/$hook_name"
if [[ -n "$compat_hook_name" ]]; then
  [[ "$compat_hook_name" =~ ^[A-Za-z0-9._-]+[.]sh$ ]]
  install -o root -g root -m 0755 "$install_root/lib/$hook_name" \
    "/var/lib/vz/snippets/$compat_hook_name"
fi
cat >/etc/systemd/system/app-ha-deferred-cleanup.service <<EOF
[Unit]
Description=Process deferred app-ha staging cleanup
After=pve-cluster.service
Wants=pve-cluster.service

[Service]
Type=oneshot
ExecStart=$install_root/lib/process_deferred_cleanup.sh --max-seconds 120
TimeoutStartSec=150
Nice=10
IOSchedulingClass=idle
EOF
cat >/etc/systemd/system/app-ha-deferred-cleanup.timer <<'EOF'
[Unit]
Description=Retry deferred app-ha staging cleanup

[Timer]
OnBootSec=2min
OnUnitActiveSec=2min
RandomizedDelaySec=30s
AccuracySec=15s
Persistent=false

[Install]
WantedBy=timers.target
EOF
cat >/etc/systemd/system/app-ha-haproxy-route-sync.service <<EOF
[Unit]
Description=Converge and fail-close local app-ha HAProxy ingress
After=network-online.target pve-cluster.service
Wants=network-online.target pve-cluster.service
Before=app-ha-haproxy-ingress.service

[Service]
Type=oneshot
ExecStart=$install_root/lib/sync_haproxy_routes.sh --local-reconcile
TimeoutStartSec=180
UMask=0077
EOF
cat >/etc/systemd/system/app-ha-haproxy-route-sync.timer <<'EOF'
[Unit]
Description=Continuously reconcile app-ha HAProxy ingress

[Timer]
OnBootSec=20s
OnUnitActiveSec=1min
RandomizedDelaySec=10s
AccuracySec=5s
Persistent=false
Unit=app-ha-haproxy-route-sync.service

[Install]
WantedBy=timers.target
EOF
install -d -m 0755 \
  /etc/systemd/system/app-ha-haproxy-ingress.service.d
cat >/etc/systemd/system/app-ha-haproxy-ingress.service.d/route-sync.conf <<EOF
[Unit]
After=app-ha-haproxy-route-sync.service
Wants=app-ha-haproxy-route-sync.service

[Service]
ExecStartPre=
ExecStartPre=$install_root/lib/sync_haproxy_routes.sh --assert-local-current
EOF
chmod 0644 /etc/systemd/system/app-ha-deferred-cleanup.service \
  /etc/systemd/system/app-ha-deferred-cleanup.timer \
  /etc/systemd/system/app-ha-haproxy-route-sync.service \
  /etc/systemd/system/app-ha-haproxy-route-sync.timer \
  /etc/systemd/system/app-ha-haproxy-ingress.service.d/route-sync.conf
systemctl daemon-reload
systemctl enable --now app-ha-deferred-cleanup.timer
systemctl enable app-ha-haproxy-route-sync.timer
pvesm path "local:snippets/$hook_name" >/dev/null
NODE
  )
done

replication_minutes="${replication_schedule##*/}"
[[ "$replication_minutes" =~ ^[1-9][0-9]*$ ]]
"$install_root/lib/cluster_registry.py" --state-dir "$state_dir" init \
  --network "$network" \
  --guest-gateway "$guest_gateway" \
  --mox-ip-start "$mox_ip_start" \
  --mox-ip-end "$mox_ip_end" \
  --haproxy-ip-start "$haproxy_ip_start" \
  --haproxy-ip-end "$haproxy_ip_end" \
  --production-ip-start "$production_ip_start" \
  --production-ip-end "$production_ip_end" \
  --staging-ip-start "$staging_ip_start" \
  --staging-ip-end "$staging_ip_end" \
  --max-hosts "$max_hosts" \
  --production-tag "$production_tag" \
  --staging-tag "$staging_tag" \
  --evictable-tag "$evictable_tag" \
  --prod-cores "$prod_cores" \
  --prod-memory-mb "$prod_memory" \
  --prod-disk-gib "$prod_disk" \
  --prod-disk-allocation sparse \
  --staging-cores "$staging_cores" \
  --staging-memory-mb "$staging_memory" \
  --staging-disk-gib "$staging_disk" \
  --replication-interval "$replication_minutes" >/dev/null

# Record every member's host slot, then the control node if none is recorded.
"$install_root/lib/cluster_registry.py" --state-dir "$state_dir" \
  host-sync --live >/dev/null
recorded_control="$(
  "$install_root/lib/cluster_registry.py" --state-dir "$state_dir" control-get
)"
if [[ "$recorded_control" == null ]]; then
  "$install_root/lib/cluster_registry.py" --state-dir "$state_dir" \
    control-set "$control_node" --expected-node none >/dev/null
fi
REMOTE
  write_state shared-orchestration-tools-installed
}

create_haproxy_lxc() {
  has_state haproxy-lxc-generic-ingress-v3-configured && return
  log "Creating fixed HAProxy LXC $HAPROXY_LXC_VMID on $HOST_ID"
  remote systemctl stop app-ha-haproxy-route-sync.timer
  remote_apt_script "$HAPROXY_LXC_VMID" "$HAPROXY_LXC_HOSTNAME" "$HAPROXY_LXC_IP" \
    "$HAPROXY_LXC_GATEWAY" "$PROXMOX_PRIVATE_BRIDGE" "$HAPROXY_LXC_STORAGE" \
    "$HAPROXY_LXC_ROOTFS_GB" "$HAPROXY_LXC_MEMORY_MB" \
    "${HAPROXY_LXC_CORES:-1}" <<'REMOTE'
set -Eeuo pipefail
vmid="$1"; name="$2"; ip_cidr="$3"; gateway="$4"; bridge="$5"; storage="$6"
rootfs_gb="$7"; memory_mb="$8"; cores="$9"
[[ "$(dpkg --print-architecture)" == amd64 && "$(uname -m)" == x86_64 ]]

# Recover only the exact stopped ARM64 container produced by the former
# unqualified template selector. Refuse to replace any other VMID collision.
if pct status "$vmid" >/dev/null 2>&1; then
  existing_config="$(pct config "$vmid")"
  existing_arch="$(
    awk -F ': ' '$1 == "arch" { print $2; count++ } END { if (count != 1) exit 1 }' \
      <<<"$existing_config"
  )"
  if [[ "$existing_arch" != amd64 ]]; then
    [[ "$existing_arch" == arm64 ]]
    [[ "$(pct status "$vmid")" == "status: stopped" ]]
    grep -Fxq "hostname: $name" <<<"$existing_config"
    existing_net0="$(
      awk -F ': ' '$1 == "net0" { print $2; count++ } END { if (count != 1) exit 1 }' \
        <<<"$existing_config"
    )"
    for expected in "bridge=$bridge" "gw=$gateway" "ip=$ip_cidr"; do
      case ",${existing_net0}," in
        *",${expected},"*) ;;
        *) exit 1 ;;
      esac
    done
    grep -Eq "^rootfs: ${storage}:[^,]+,size=${rootfs_gb}G(,|$)" \
      <<<"$existing_config"
    printf 'Removing exact stopped ARM64 partial HAProxy LXC %s before AMD64 recreation.\n' \
      "$vmid"
    pct destroy "$vmid" --purge 1
  fi
fi

created_lxc=0
if ! pct status "$vmid" >/dev/null 2>&1; then
  created_lxc=1
  pveam update
  template="$(
    pveam available --section system |
      awk '$2 ~ /^debian-13-standard_.*_amd64[.]tar[.]zst$/ { print $2 }' |
      sort -V | tail -n1
  )"
  [[ "$template" =~ ^debian-13-standard_.*_amd64[.]tar[.]zst$ ]] ||
    { echo "No Debian 13 AMD64 standard LXC template is available" >&2; exit 1; }
  [[ -f "/var/lib/vz/template/cache/$template" ]] || pveam download local "$template"
  pct create "$vmid" "local:vztmpl/$template" \
    --arch amd64 --tags app-ha-fixed-haproxy \
    --hostname "$name" --unprivileged 1 --cores "$cores" --memory "$memory_mb" --swap 0 \
    --rootfs "${storage}:${rootfs_gb}" --onboot 1 --startup order=1 \
    --net0 "name=eth0,bridge=${bridge},ip=${ip_cidr},gw=${gateway},type=veth,firewall=1"
fi
pct set "$vmid" --onboot 1 --startup order=1
config="$(pct config "$vmid")"
grep -Fxq "arch: amd64" <<<"$config"
grep -Fxq "hostname: $name" <<<"$config"
net0="$(awk -F ': ' '$1 == "net0" { print $2; count++ } END { if (count != 1) exit 1 }' <<<"$config")"
for expected in "bridge=$bridge" "gw=$gateway" "ip=$ip_cidr"; do
  case ",${net0}," in
    *",${expected},"*) ;;
    *)
      printf 'Fixed HAProxy LXC %s net0 is missing %s: %s\n' \
        "$vmid" "$expected" "$net0" >&2
      exit 1
      ;;
  esac
done
pct start "$vmid" || true
for _ in $(seq 1 30); do
  pct exec "$vmid" -- true 2>/dev/null && break
  sleep 2
done

# Give the private-only LXC outbound Internet access before apt runs. The
# durable ingress/NAT table and systemd unit are installed immediately below.
lxc_ip="${ip_cidr%/*}"
public_if="$(ip -4 route get 1.1.1.1 | awk '{for (i=1;i<=NF;i++) if ($i=="dev") {print $(i+1); exit}}')"
[[ -n "$public_if" ]]
sysctl -w net.ipv4.ip_forward=1 >/dev/null
nft list table inet app_ha_haproxy_bootstrap >/dev/null 2>&1 &&
  nft delete table inet app_ha_haproxy_bootstrap
nft add table inet app_ha_haproxy_bootstrap
nft 'add chain inet app_ha_haproxy_bootstrap forward { type filter hook forward priority -151; policy accept; }'
nft 'add chain inet app_ha_haproxy_bootstrap postrouting { type nat hook postrouting priority srcnat; policy accept; }'
nft add rule inet app_ha_haproxy_bootstrap forward ip saddr "$lxc_ip" accept
nft add rule inet app_ha_haproxy_bootstrap forward ct state established,related accept
nft add rule inet app_ha_haproxy_bootstrap postrouting ip saddr "$lxc_ip" oifname "$public_if" masquerade

pct exec "$vmid" -- bash -s -- "$created_lxc" < <(
  declare -f apt_lock_holder apt_wait_for_locks
  cat <<'LXC'
set -Eeuo pipefail
created_lxc="$1"
export LANG=C.UTF-8
export LC_ALL=C.UTF-8
cat >/etc/default/locale <<'EOF'
LANG=C.UTF-8
LC_ALL=C.UTF-8
EOF
apt_wait_for_locks
apt-get -o DPkg::Lock::Timeout=60 update
DEBIAN_FRONTEND=noninteractive apt-get -o DPkg::Lock::Timeout=60 install -y diffutils haproxy rsyslog
if [[ "$created_lxc" == 1 || ! -s /etc/haproxy/haproxy.cfg ]]; then
cat >/etc/haproxy/haproxy.cfg <<EOF
# Temporary reject-only bootstrap. sync_haproxy_routes.sh replaces this with
# the deterministic app-ha route generation.
global
    log /dev/log local0
    log /dev/log local1 notice
    user haproxy
    group haproxy
    daemon

defaults
    log global
    timeout connect 5s
    timeout client  60s
    timeout server  60s

frontend public_http
    bind :80
    mode http
    option httplog
    default_backend no_configured_http_route

frontend public_tls
    bind :443
    mode tcp
    option tcplog
    default_backend no_configured_tls_route

backend no_configured_http_route
    mode http
    http-request deny deny_status 404

backend no_configured_tls_route
    mode tcp
    server no_route 127.0.0.1:1 disabled
EOF
fi
haproxy -c -f /etc/haproxy/haproxy.cfg
systemctl enable haproxy rsyslog
systemctl start rsyslog
systemctl reload-or-restart haproxy
LXC
)
pct exec "$vmid" -- ip -4 route show default |
  awk -v gateway="$gateway" '$1 == "default" && $3 == gateway { found=1 } END { exit !found }'
REMOTE

  remote_script "$PROXMOX_IP" "$HAPROXY_LXC_IP" "$HAPROXY_LXC_GATEWAY" \
    "$HAPROXY_LXC_VMID" <<'REMOTE'
set -Eeuo pipefail
public_ip="$1"; lxc_ip="${2%/*}"; private_gateway="$3"; vmid="$4"
public_if="$(ip -4 route get 1.1.1.1 | awk '{for (i=1;i<=NF;i++) if ($i=="dev") {print $(i+1); exit}}')"
[[ -n "$public_if" ]]
pct exec "$vmid" -- ip -4 route show default |
  awk -v gateway="$private_gateway" '$1 == "default" && $3 == gateway { found=1 } END { exit !found }'
cat >/etc/sysctl.d/90-app-ha-haproxy-router.conf <<'EOF'
net.ipv4.ip_forward=1
EOF
sysctl --system >/dev/null
install -d -m 0755 /etc/nftables.d
cat >/etc/nftables.d/app-ha-haproxy-ingress.nft <<EOF
table inet app_ha_haproxy_ingress {
  chain prerouting {
    type nat hook prerouting priority dstnat; policy accept;
    ip daddr $public_ip tcp dport { 80, 443 } dnat ip to $lxc_ip
  }
  chain forward {
    type filter hook forward priority -150; policy accept;
    ct state established,related accept
    iifname "$public_if" ip daddr $lxc_ip tcp dport { 80, 443 } accept
    ip saddr $lxc_ip accept
  }
  chain postrouting {
    type nat hook postrouting priority srcnat; policy accept;
    ip saddr $lxc_ip oifname "$public_if" masquerade
  }
}
EOF
cat >/usr/local/sbin/app-ha-load-haproxy-ingress <<'EOF'
#!/usr/bin/env bash
set -Eeuo pipefail
nft list table inet app_ha_haproxy_ingress >/dev/null 2>&1 &&
  nft delete table inet app_ha_haproxy_ingress
nft -f /etc/nftables.d/app-ha-haproxy-ingress.nft
EOF
chmod 0755 /usr/local/sbin/app-ha-load-haproxy-ingress
cat >/etc/systemd/system/app-ha-haproxy-ingress.service <<EOF
[Unit]
Description=app-ha public ingress DNAT to fixed local HAProxy LXC
After=network-online.target pve-container@${vmid}.service app-ha-haproxy-route-sync.service
Wants=network-online.target app-ha-haproxy-route-sync.service
[Service]
Type=oneshot
ExecStartPre=/usr/local/lib/app-ha-proxmox/lib/sync_haproxy_routes.sh --assert-local-current
ExecStart=/usr/local/sbin/app-ha-load-haproxy-ingress
RemainAfterExit=yes
[Install]
WantedBy=multi-user.target
EOF
systemctl daemon-reload
systemctl enable app-ha-haproxy-ingress.service
/usr/local/lib/app-ha-proxmox/lib/sync_haproxy_routes.sh \
  --local-reconcile --lock-timeout 240
systemctl restart app-ha-haproxy-ingress.service

nft list table inet app_ha_haproxy_bootstrap >/dev/null 2>&1 &&
  nft delete table inet app_ha_haproxy_bootstrap
systemctl daemon-reload
systemctl is-active app-ha-haproxy-ingress.service
nft list table inet app_ha_haproxy_ingress
REMOTE
  write_state haproxy-lxc-configured
  write_state haproxy-lxc-base-v2-configured
  write_state haproxy-lxc-generic-ingress-v3-configured
}

sync_haproxy_routes() {
  log "Synchronizing registry-backed routes across online HAProxy LXCs"
  remote /usr/local/lib/app-ha-proxmox/lib/sync_haproxy_routes.sh \
    --lock-timeout 240
  remote systemctl start app-ha-haproxy-route-sync.timer
  write_state haproxy-routes-synchronized
}

final_verify() {
  local pair member header
  if [[ "$ENCRYPTION_POLICY" == luks ]]; then
    for pair in "${CONFIGURED_MIRROR_PAIRS[@]}"; do
      for member in 1 2; do
        if ((pair == 0)); then
          if ((member == 1)); then
            header="${HEADER_DIR}/rpool-A.bin"
          else
            header="${HEADER_DIR}/rpool-B.bin"
          fi
        else
          header="${HEADER_DIR}/rpool-mirror${pair}-${member}.bin"
        fi
        [[ -s "$header" ]] ||
          fail "Missing off-host LUKS header backup for mirror $pair member $member"
      done
    done
  fi

  log "Running final host verification"
  remote_script "$PROXMOX_PRIVATE_BRIDGE" "$HAPROXY_LXC_VMID" \
    "$GUEST_EGRESS_VIP" "$GUEST_ROLE_HOOK_NAME" "$PROXMOX_SECONDARY_IP" \
    "$MOX_REPLICATION_IP" "$PRIVATE_SUBNET_PREFIX" \
    "$MOX_IP_START_OCTET" "$MAX_MOX_HOSTS" \
    "$VRRP_PRIORITY" "$ENCRYPTION_POLICY" \
    "$PROXMOX_INTERNAL_DOMAIN" \
    "${EXPECTED_RPOOL_MAPPERS[@]}" <<'REMOTE' |
    tee "${LOG_DIR}/final-verification.txt"
set -Eeuo pipefail
bridge="$1"; vmid="$2"; egress_vip="${3%/*}"; hook_name="$4"
private_ip="${5%/*}"; replication_cidr="$6"; subnet_prefix="$7"
mox_start="$8"; max_hosts="$9"; priority="${10}"; encryption_policy="${11}"
internal_domain="${12}"
shift 12
pveversion
pvecm status
zpool status -P rpool
zpool status -x rpool | grep -F "pool 'rpool' is healthy"
pool="$(zpool status -P rpool)"
for mapper in "$@"; do
  awk -v member="/dev/mapper/$mapper" \
    '$1 == member { found++ } END { exit !(found == 1) }' <<<"$pool"
  awk -v mapper="$mapper" '
    $1 == mapper {
      if ($3 != "app-ha-rpool" ||
          $4 !~ /(^|,)keyscript=decrypt_keyctl(,|$)/) exit 1
      found++
    }
    END { if (found != 1) exit 1 }
  ' /etc/crypttab
done
[[ -z "$(swapon --noheadings --show=NAME)" ]]
proxmox-boot-tool status
ip -4 address show dev "$bridge"
python3 - "$replication_cidr" "$(ip -j -4 address show dev "$bridge")" <<'PY'
import ipaddress
import json
import sys

expected = ipaddress.ip_interface(sys.argv[1])
rows = json.loads(sys.argv[2])
matching = [
    ipaddress.ip_address(info["local"])
    for row in rows
    for info in row.get("addr_info", [])
    if info.get("family") == "inet"
    and ipaddress.ip_address(info["local"]) in expected.network
]
if matching != [expected.ip]:
    raise SystemExit(
        f"migration CIDR {expected.network} must match exactly {expected.ip}; "
        f"found {matching}"
    )
PY
tailscale status
if [[ "$encryption_policy" == luks ]]; then
  lsinitramfs "/boot/initrd.img-$(uname -r)" |
    awk '/(^|\/)decrypt_keyctl$/ { found=1 } END { exit !found }'
else
  ! awk '$1 ~ /^crypt-rpool-/ { found=1 } END { exit !found }' /etc/crypttab
fi
pct status "$vmid"
pct exec "$vmid" -- systemctl is-active haproxy
pct exec "$vmid" -- haproxy -c -f /etc/haproxy/haproxy.cfg
pct exec "$vmid" -- grep -Fq \
  'Managed by app-ha HAProxy route synchronization' /etc/haproxy/haproxy.cfg
nft list table inet app_ha_host_guard
nft list table inet app_ha_haproxy_ingress
systemctl is-active app-ha-haproxy-ingress.service
systemctl is-enabled --quiet app-ha-haproxy-route-sync.timer
systemctl is-active --quiet app-ha-haproxy-route-sync.timer
nft list table ip app_ha_guest_egress
/usr/local/sbin/app-ha-check-egress-router
systemctl is-active keepalived.service
keepalived --config-test --use-file=/etc/keepalived/keepalived.conf
vip_owner=""
for _ in $(seq 1 10); do
  vip_owner=""
  vip_owner_count=0
  queried_node_count=0
  query_failed=false
  nodes_json="$(pvesh get /nodes --output-format json)"
  online_nodes_before="$(
    jq -r '[.[] | select(.status == "online") | .node] | sort | join("\n")' \
      <<<"$nodes_json"
  )"
  online_node_count="$(
    jq -r '[.[] | select(.status == "online")] | length' <<<"$nodes_json"
  )"
  while IFS= read -r node; do
    [[ -n "$node" ]] || continue
    node_fqdn="${node}.${internal_domain}"
    node_pin="/etc/pve/nodes/${node}/ssh_known_hosts"
    [[ -s "$node_pin" && ! -L "$node_pin" ]] || {
      query_failed=true
      continue
    }
    if node_vip_state="$(
      ssh -o BatchMode=yes -o ClearAllForwardings=yes \
      -o StrictHostKeyChecking=yes \
      -o CheckHostIP=no \
      -o "HostKeyAlias=${node}" \
      -o "UserKnownHostsFile=${node_pin}" \
      -o GlobalKnownHostsFile=none "root@$node_fqdn" \
      "if ip -4 -o address show dev '$bridge' | awk -v vip='$egress_vip' '\$4 ~ (\"^\" vip \"/\") { found=1 } END { exit !found }'; then printf 'owner\n'; else printf 'backup\n'; fi"
    )"
    then
      queried_node_count=$((queried_node_count + 1))
      if [[ "$node_vip_state" == owner ]]; then
        vip_owner="$node"
        vip_owner_count=$((vip_owner_count + 1))
      elif [[ "$node_vip_state" != backup ]]; then
        query_failed=true
      fi
    else
      query_failed=true
    fi
  done <<<"$online_nodes_before"
  online_nodes_after="$(
    pvesh get /nodes --output-format json |
      jq -r '[.[] | select(.status == "online") | .node] | sort | join("\n")'
  )"
  if [[ "$query_failed" == false &&
        "$online_nodes_before" == "$online_nodes_after" ]] &&
     ((queried_node_count == online_node_count && vip_owner_count == 1)); then
    break
  fi
  sleep 5
done
if [[ "$query_failed" != false ||
      "$online_nodes_before" != "$online_nodes_after" ]] ||
   ((queried_node_count != online_node_count || vip_owner_count != 1)); then
  printf 'Could not prove exactly one stable, reachable online mox owns guest-egress VIP %s (queried %s/%s, owners %s).\n' \
    "$egress_vip" "$queried_node_count" "$online_node_count" \
    "$vip_owner_count" >&2
  exit 1
fi
printf 'Guest-egress VIP %s is owned by exactly one node: %s\n' \
  "$egress_vip" "$vip_owner"
bash -n "/var/lib/vz/snippets/$hook_name"
test -x /usr/local/lib/app-ha-proxmox/lib/process_deferred_cleanup.sh
bash -n /usr/local/lib/app-ha-proxmox/lib/process_deferred_cleanup.sh
systemctl is-enabled --quiet app-ha-deferred-cleanup.timer
systemctl is-active --quiet app-ha-deferred-cleanup.timer
grep -Fq "$egress_vip" /etc/keepalived/keepalived.conf
grep -Eq "^[[:space:]]*priority[[:space:]]+${priority}[[:space:]]*$" \
  /etc/keepalived/keepalived.conf
for ((index = 1; index <= max_hosts; index += 1)); do
  candidate="${subnet_prefix}.$((mox_start + index - 1))"
  if [[ "$candidate" == "$private_ip" ]]; then
    ! grep -Eq "^[[:space:]]*${candidate}[[:space:]]*$" /etc/keepalived/keepalived.conf
  else
    grep -Eq "^[[:space:]]*${candidate}[[:space:]]*$" /etc/keepalived/keepalived.conf
  fi
done
REMOTE
  write_state complete
}

main() {
  cat <<'EOF'

NOTE: During storage setup, cryptsetup may print messages like:

  cryptsetup: ERROR: Couldn't resolve device rpool/ROOT/pve-1
  cryptsetup: WARNING: Couldn't determine root device

These messages are expected in this workflow and may be ignored.


NOTE: You can typically run multiple instances of this script concurrently
for different Proxmox hosts. Some operations may still collide because not
every concurrency issue has been eliminated. If an instance fails, rerun it;
the workflow is resumable and will generally continue from where it stopped.

When building a brand-new cluster with concurrent instances, the other hosts
assume the control node (PROXMOX_CONTROL_NODE) will create the cluster. At the
"PRIVATE VLAN VERIFIED" prompt, let the control node's instance continue past
that prompt and finish first, and only then continue past that prompt in the
other hosts' instances, so the control node is reachable when they join.


This script will install packages on the QDevice if it is not yet already set
up as a QDevice. Before continuing, it is recommended that you run
"sudo apt update && sudo apt upgrade" on the QDevice host, and likely also
reboot it, so it is up to date and ready for those installations.

EOF
  if bmac_ui_is_json; then
    bmac_ui_manual_action --id before_start --title "Before adding a Proxmox host" \
      --instruction "cryptsetup messages such as \"Couldn't resolve device rpool/ROOT/pve-1\" during storage setup are expected and may be ignored." \
      --instruction "When building a new cluster with several hosts at once, let the control node's run pass the PRIVATE VLAN VERIFIED step and finish first, then continue the others." \
      --instruction "Run sudo apt update && sudo apt upgrade on the QDevice host (and likely reboot it) first; this script may install packages there." \
      --ack-label Continue
  else
    read -r -p "Press ENTER to continue: "
  fi

  parse_args "$@"
  reset_or_resume_existing_state
  choose_hardware_inventory_mode
  load_configuration
  preflight
  choose_role
  prepare_qdevice
  choose_encryption_policy
  choose_boot_test_policy
  discover_hardware
  build_iso
  installation_gate
  configure_workstation_ssh
  bootstrap_host

  if [[ "$ENCRYPTION_POLICY" == luks ]]; then
    stage_luks_password_file
    verify_luks_password_file existing
    rebuild_luks_member A
    rebuild_luks_member B
    verify_encrypted_mirror
    configure_extra_mirrors
    configure_shared_luks_unlock
    if [[ "$BOOT_TEST_POLICY" == run ]]; then
      encrypted_boot_test A
      encrypted_boot_test B
      final_normal_boot_test
    fi
    verify_luks_password_file all
    remove_luks_password_file
  else
    if [[ "$BOOT_TEST_POLICY" == run ]]; then
      raw_boot_test A
      raw_boot_test B
    fi
    configure_extra_mirrors
    verify_clear_mirrors
    if [[ "$BOOT_TEST_POLICY" == run ]]; then
      final_normal_boot_test
    fi
  fi

  configure_private_network
  reconcile_cluster_control_plane
  configure_guest_egress
  install_shared_orchestration_tools
  install_guest_role_hook
  create_haproxy_lxc
  sync_haproxy_routes
  final_verify
  retire_setup_key_with_admin_identity
  enable_admin_ssh_for_target

  log "Proxmox host setup complete"
  info "Host: $HOST_ID"
  info "Cluster role: $SETUP_ROLE"
  info "Artifacts: $HOST_ARTIFACTS"
  if [[ "$ENCRYPTION_POLICY" == luks ]]; then
    if [[ "$HARDWARE_INVENTORY_MODE" == idrac ]]; then
      info "LUKS remains operator-unlocked: use iDRAC for every host boot."
    else
      info "LUKS remains operator-unlocked: use the target host's physical or remote console for every boot."
    fi
  else
    info "rpool is unencrypted; normal reboots do not require a console passphrase."
  fi
  info "Attach local:snippets/${GUEST_ROLE_HOOK_NAME} to every production and staging VM."
  info "Production ISO downloads/builds use the host cache at /var/lib/app-ha-proxmox/iso-cache."
  info "Production VMs require tag ${PRODUCTION_VM_TAG}; disposable staging VMs require tags ${STAGING_VM_TAG} and ${EVICTABLE_VM_TAG}."
  info "HAProxy exact Host/TLS-SNI routes are synchronized from the shared app-ha registry."
  info "Production VMs, ZFS replication jobs, and HA resources remain intentionally out of scope."
  bmac_ui_step_done
  bmac_ui_result host "$HOST_ID" role "$SETUP_ROLE" encryption "$ENCRYPTION_POLICY" \
    artifacts "$HOST_ARTIFACTS"
  if [[ "$ENCRYPTION_POLICY" == luks ]]; then
    bmac_ui_next_step "Every boot of $HOST_ID needs the rpool passphrase typed at its $([[ "$HARDWARE_INVENTORY_MODE" == idrac ]] && echo "iDRAC console" || echo "physical or remote console")."
  fi
  bmac_ui_next_step "Attach local:snippets/${GUEST_ROLE_HOOK_NAME} to every production and staging VM."
  bmac_ui_next_step "Check the new host." --workflow show_proxmox_host_state --arg "host=$HOST_ID"
  bmac_ui_next_step "Check cluster health." --workflow show_cluster_health
}

if [[ "${BASH_SOURCE[0]}" == "$0" ]]; then
  bmac_ui_bootstrap "$@"
  main "$@"
fi
