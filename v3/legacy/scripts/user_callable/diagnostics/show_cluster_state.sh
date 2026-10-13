#!/usr/bin/env bash

# Copyright (c) 2026 BEENTHERE VENTURES, INC.
# SPDX-License-Identifier: GPL-3.0-only

# Read-only diagnostic snapshot of the cluster, run on one host: every member
# and the QDevice in full detail, and the shared registry once.
#
# The script intentionally makes no configuration changes. It does create normal
# SSH/authentication and command-access log entries on the systems it inspects.

set -u
set -o pipefail

if ((BASH_VERSINFO[0] < 4 || (BASH_VERSINFO[0] == 4 && BASH_VERSINFO[1] < 4))); then
  printf 'ERROR: show_cluster_state.sh requires Bash 4.4 or newer (found %s).\n' \
    "${BASH_VERSION:-unknown}" >&2
  exit 2
fi

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
REPO_ROOT="$(cd -- "${SCRIPT_DIR}/../../.." && pwd -P)"
# shellcheck source=../../lib/ui_protocol.sh
source "${REPO_ROOT}/scripts/lib/ui_protocol.sh"
bmac_ui_bootstrap "$@"
# shellcheck source=../../lib/config.sh
source "${REPO_ROOT}/scripts/lib/config.sh"
# shellcheck source=../../lib/cluster_control.sh
source "${REPO_ROOT}/scripts/lib/cluster_control.sh"
# shellcheck source=../../lib/qdevice.sh
source "${REPO_ROOT}/scripts/lib/qdevice.sh"

load_proxmox_config >/dev/null || {
  printf 'ERROR: cluster configuration is invalid\n' >&2
  exit 2
}
QDEVICE_HOST="$PROXMOX_QDEVICE_HOST"
THIS_HOST="$(this_mox)" || exit 2

expected_prod_iso_sha256="${PROD_GUEST_OS_ISO_SHA256:-}"
expected_prod_iso_sha256="${expected_prod_iso_sha256,,}"
expected_prod_install_mode="${PROD_GUEST_OS_INSTALL_MODE:-ubuntu-autoinstall}"
expected_cluster_config_sha256="$CONFIG_EFFECTIVE_SHA256"
expected_migration_network="$PROXMOX_MIGRATION_NETWORK"
expected_replication_ip_start="$MOX_REPLICATION_IP_START"
expected_replication_ip_end="$MOX_REPLICATION_IP_END"
expected_iso_preparer_sha256="$(
  sha256sum "${REPO_ROOT}/scripts/host_runtime/prepare_prod_iso.sh" | awk '{print $1}'
)"
expected_iso_builder_sha256="$(
  sha256sum "${REPO_ROOT}/scripts/host_runtime/build_ubuntu_autoinstall.py" | awk '{print $1}'
)"
local_prod_config_ready=0
[[ -n "$expected_iso_preparer_sha256" && -n "$expected_iso_builder_sha256" ]] &&
  local_prod_config_ready=1

hr() {
  printf '%*s\n' 96 '' | tr ' ' '='
}

subhr() {
  printf '%*s\n' 96 '' | tr ' ' '-'
}

banner() {
  printf '\n'
  hr
  printf '%s\n' "$1"
  hr
  local label host
  case "$1" in
    "DISCOVERY - SSH REACHABILITY") label="Discovery - SSH reachability" ;;
    "QDEVICE - SKIPPED") label="QDevice - skipped" ;;
    "QDEVICE - "*) label="QDevice - ${1#QDEVICE - }" ;;
    "PROXMOX HOST - "*) host="${1#PROXMOX HOST - }"; label="Proxmox host - ${host,,}" ;;
    *) label="${1,,}"; label="${label^}" ;;
  esac
  bmac_ui_step "$label"
}

note() {
  printf '  %s\n' "$*"
}

printf 'READ-ONLY PROXMOX CLUSTER DIAGNOSTICS, run on %s\n\n' "$THIS_HOST"
cat <<'INTRO'
This only INSPECTS state; it does not intentionally modify cluster, network,
storage, guest, QDevice, firewall, HA, or service configuration. Members this
host cannot reach, and the QDevice when it cannot be reached with its pinned
host key, are reported and skipped. SSH connections and commands create normal
authentication, audit, and journal log entries on the inspected systems.
INTRO

STARTED_AT="$(date +%Y-%m-%dT%H:%M:%S%z)"
banner "DISCOVERY - SSH REACHABILITY"
note "Why: establish which expected machines can be inspected before running the detailed checks."
note "Started: $STARTED_AT"
if ((local_prod_config_ready)); then
  note "Production ISO readiness will be compared with $THIS_HOST."
  note "Expected install mode: $expected_prod_install_mode"
  note "Expected source SHA-256: $expected_prod_iso_sha256"
else
  note "WARNING: local production ISO configuration/tools could not be loaded; host-local checks will still run."
fi

probe_ssh() {
  local host="$1" out rc
  if [[ "$host" == "$QDEVICE_HOST" ]]; then
    out="$(qd_ssh true </dev/null 2>&1)"
  else
    out="$(mox_ssh "$host" true </dev/null 2>&1)"
  fi
  rc=$?
  if (( rc == 0 )); then
    printf '  [REACHABLE]   %-10s\n' "$host"
    return 0
  fi

  printf '  [UNREACHABLE] %-10s  ssh exit=%d\n' "$host" "$rc"
  if [[ -n "$out" ]]; then
    printf '%s\n' "$out" | sed 's/^/                /'
  fi
  return 1
}

qdevice_reachable=0
if probe_ssh "$QDEVICE_HOST"; then
  qdevice_reachable=1
fi

membership_status=0
control_find_cluster || membership_status=$?
if ((membership_status != 0)); then
  banner "NO CLUSTER"
  note "$THIS_HOST does not belong to a cluster, or its membership is malformed."
  exit 1
fi
reachable_mox=()
unreachable_mox=()
for host in "${CONTROL_MEMBER_NODES[@]}"; do
  if probe_ssh "$host"; then
    reachable_mox+=("$host")
  else
    unreachable_mox+=("$host")
  fi
done

printf '\n  Reachable Proxmox candidates: '
if ((${#reachable_mox[@]})); then
  printf '%s ' "${reachable_mox[@]}"
else
  printf '(none)'
fi
printf '\n'

printf '  Unreachable/skipped candidates: '
if ((${#unreachable_mox[@]})); then
  printf '%s ' "${unreachable_mox[@]}"
else
  printf '(none)'
fi
printf '\n'

if (( qdevice_reachable )); then
  banner "QDEVICE - ${QDEVICE_HOST}"
  note "Why: verify the external quorum server's identity, Tailscale connectivity, qnetd service,"
  note "     TCP/5403 listener, QNetd client state, firewall visibility, and local service health."

  qd_ssh 'bash -s' <<'QDEVICE'
set -u
set -o pipefail
export LC_ALL=C

section() {
  printf '\n'
  printf '%*s\n' 88 '' | tr ' ' '-'
  printf '%s\n' "$1"
  printf 'Why: %s\n' "$2"
  printf '%*s\n' 88 '' | tr ' ' '-'
}

run() {
  local description="$1"
  shift
  printf '\n[CHECK] %s\n' "$description"
  printf '  $'
  printf ' %q' "$@"
  printf '\n'
  "$@"
  local rc=$?
  printf '[exit %d]\n' "$rc"
  return 0
}

run_shell() {
  local description="$1" code="$2"
  shift 2
  printf '\n[CHECK] %s\n' "$description"
  printf '  $ %s\n' "$code"
  bash -c "$code" bash "$@"
  local rc=$?
  printf '[exit %d]\n' "$rc"
  return 0
}

run_if_command() {
  local cmd="$1" description="$2"
  shift 2
  if command -v "$cmd" >/dev/null 2>&1; then
    run "$description" "$@"
  else
    printf '\n[SKIP] %s - command %q is not installed.\n' "$description" "$cmd"
  fi
}

section "Identity, privilege, uptime, and clock" \
  "confirm this really is the expected QDevice and expose basic host/time problems that can complicate diagnostics."
run "Short hostname" hostname -s
run "Full hostname information" hostnamectl
run "Effective user" id
run "Kernel" uname -a
run "Uptime/load" uptime
run "Clock and time synchronization" timedatectl

section "Tailscale connectivity" \
  "the QDevice communicates with the Proxmox nodes over Tailscale, so its tailnet address and peer state matter."
run_if_command tailscale "QDevice Tailscale IPv4 address" tailscale ip -4
run_if_command tailscale "Tailscale peer/status view" tailscale status

section "QNetd packages and service" \
  "verify that the external qnetd server software exists, is enabled/running, and is listening on its normal TCP port 5403."
run_shell "Installed Corosync/QDevice-related Debian packages" \
  'dpkg-query -W -f="\${binary:Package}\t\${Version}\t\${db:Status-Abbrev}\n" 2>/dev/null | grep -E "^(corosync|corosync-qdevice|corosync-qnetd|libknet|libqb)" || true'
run "Whether corosync-qnetd is enabled" systemctl is-enabled corosync-qnetd.service
run "Whether corosync-qnetd is active" systemctl is-active corosync-qnetd.service
run "Detailed corosync-qnetd unit status" systemctl status corosync-qnetd.service --no-pager -l
run_shell "Processes listening on TCP port 5403" 'ss -lntp | awk "NR == 1 || /:5403([[:space:]]|$)/"'

section "QNetd cluster/client state" \
  "show whether any cluster is currently registered/connected to this QNetd server and expose stale certificate/state files."
if command -v corosync-qnetd-tool >/dev/null 2>&1; then
  run "QNetd server/client status" corosync-qnetd-tool -s -v
else
  printf '\n[SKIP] corosync-qnetd-tool is not installed.\n'
fi
run_shell "QNetd persistent state/certificate tree (names, owners, modes only)" \
  'if [ -d /etc/corosync/qnetd ]; then find /etc/corosync/qnetd -maxdepth 3 -printf "%M %u:%g %p\n" | sort; else echo "/etc/corosync/qnetd does not exist"; fi'

section "Firewall and network listeners" \
  "confirm that local firewall policy and listeners are consistent with QNetd being reachable through Tailscale."
if command -v ufw >/dev/null 2>&1; then
  run "UFW status" ufw status verbose
else
  printf '\n[INFO] UFW is not installed; checking nftables directly instead.\n'
fi
if command -v nft >/dev/null 2>&1; then
  run "nftables ruleset" nft list ruleset
else
  printf '\n[SKIP] nft is not installed.\n'
fi
run "IPv4 addresses" ip -br -4 address
run "IPv4 routes" ip -4 route show

section "Service failures and recent QNetd errors" \
  "surface failed systemd units and recent service/kernel errors that may explain a QDevice that looks configured but behaves incorrectly."
run "Failed systemd units" systemctl --failed --no-legend --no-pager
run "Recent corosync-qnetd journal" journalctl -b -u corosync-qnetd.service --no-pager -n 100
run "Recent boot errors" journalctl -b -p err..alert --no-pager -n 100
QDEVICE
else
  banner "QDEVICE - SKIPPED"
  note "$QDEVICE_HOST could not be reached with its pinned host key, so no QDevice checks were run."
fi

if ((${#reachable_mox[@]} == 0)); then
  banner "NO REACHABLE PROXMOX HOSTS"
  note "No member was reachable. There is no cluster state to inspect."
  exit 1
fi

for host in "${reachable_mox[@]}"; do
  banner "PROXMOX HOST - ${host^^}"
  note "The following checks are read-only. Optional/custom components are reported as absent rather than assumed present."

  mox_ssh "$host" bash -s -- \
    "$host" \
    "$local_prod_config_ready" \
    "${expected_prod_iso_sha256:-unavailable}" \
    "${expected_prod_install_mode:-unavailable}" \
    "${expected_cluster_config_sha256:-unavailable}" \
    "${expected_iso_preparer_sha256:-unavailable}" \
    "${expected_iso_builder_sha256:-unavailable}" \
    "${expected_migration_network:-unavailable}" \
    "${expected_replication_ip_start:-unavailable}" \
    "${expected_replication_ip_end:-unavailable}" \
    "${reachable_mox[@]}" <<'MOX'
set -u
set -o pipefail
export LC_ALL=C

expected="$1"
local_prod_config_ready="$2"
expected_prod_iso_sha256="$3"
expected_prod_install_mode="$4"
expected_cluster_config_sha256="$5"
expected_iso_preparer_sha256="$6"
expected_iso_builder_sha256="$7"
expected_migration_network="$8"
expected_replication_ip_start="$9"
expected_replication_ip_end="${10}"
shift 10
candidate_hosts=("$@")

section() {
  printf '\n'
  printf '%*s\n' 88 '' | tr ' ' '-'
  printf '%s\n' "$1"
  printf 'Why: %s\n' "$2"
  printf '%*s\n' 88 '' | tr ' ' '-'
}

run() {
  local description="$1"
  shift
  printf '\n[CHECK] %s\n' "$description"
  printf '  $'
  printf ' %q' "$@"
  printf '\n'
  "$@"
  local rc=$?
  printf '[exit %d]\n' "$rc"
  return 0
}

run_shell() {
  local description="$1" code="$2"
  shift 2
  printf '\n[CHECK] %s\n' "$description"
  printf '  $ %s\n' "$code"
  bash -c "$code" bash "$@"
  local rc=$?
  printf '[exit %d]\n' "$rc"
  return 0
}

run_if_command() {
  local cmd="$1" description="$2"
  shift 2
  if command -v "$cmd" >/dev/null 2>&1; then
    run "$description" "$@"
  else
    printf '\n[SKIP] %s - command %q is not installed.\n' "$description" "$cmd"
  fi
}

unit_report() {
  local unit="$1"
  local load active enabled
  load="$(systemctl show "$unit" -p LoadState --value 2>/dev/null || true)"
  if [[ -z "$load" || "$load" == "not-found" ]]; then
    printf '  %-46s %-11s\n' "$unit" 'not-found'
    return 0
  fi
  active="$(systemctl is-active "$unit" 2>/dev/null || true)"
  enabled="$(systemctl is-enabled "$unit" 2>/dev/null || true)"
  printf '  %-46s active=%-12s enabled=%s\n' "$unit" "${active:-unknown}" "${enabled:-unknown}"
}

node="$(hostname -s)"
index="${node#mox}"
vmid=""
remote_prod_config_loaded=0
remote_prod_iso_url=""
remote_prod_iso_sha256=""
remote_prod_install_mode=""
remote_cluster_config_sha256=""
remote_iso_storage=""
if [[ "$node" =~ ^mox([0-9]+)$ ]]; then
  index="${BASH_REMATCH[1]}"
  vmid="$((9110 + index))"
fi
if [[ -f /usr/lib/bmac/legacy/scripts/lib/config.sh &&
      ! -L /usr/lib/bmac/legacy/scripts/lib/config.sh ]]; then
  # shellcheck source=/dev/null
  source /usr/lib/bmac/legacy/scripts/lib/config.sh
  if load_proxmox_config >/dev/null 2>&1; then
    remote_prod_iso_url="${PROD_GUEST_OS_ISO_URL:-}"
    remote_prod_iso_sha256="${PROD_GUEST_OS_ISO_SHA256:-}"
    remote_prod_iso_sha256="${remote_prod_iso_sha256,,}"
    remote_prod_install_mode="${PROD_GUEST_OS_INSTALL_MODE:-ubuntu-autoinstall}"
    remote_cluster_config_sha256="${CONFIG_EFFECTIVE_SHA256:-}"
    remote_iso_storage="${ISO_STORAGE_ID:-}"
    remote_prod_config_loaded=1
  fi
fi

section "Identity, privilege, versions, uptime, and clock" \
  "verify that SSH reached the intended node and capture the software/time baseline before interpreting cluster behavior."
printf 'Expected node: %s\n' "$expected"
printf 'Actual node:   %s\n' "$node"
if [[ -n "$vmid" ]]; then
  printf 'Expected local HAProxy LXC VMID from naming convention: %s\n' "$vmid"
else
  printf 'Expected local HAProxy LXC VMID: unavailable (hostname does not match moxN)\n'
fi
run "Effective user" id
run "Host identity" hostnamectl
run_if_command pveversion "Proxmox package/version inventory" pveversion -v
run "Kernel" uname -a
run "Uptime/load" uptime
run "Clock and time synchronization" timedatectl
run "Memory summary" free -h

section "Network identity, private bridge, routes, and Tailscale" \
  "Corosync, replication, migration, ingress/egress routing, and administration depend on predictable node addressing and interfaces."
run "Compact IPv4 address view" ip -br -4 address
run "IPv4 routing table" ip -4 route show
run "Interface counters/errors" ip -s link
if ip link show vmbr-private >/dev/null 2>&1; then
  run "Private Proxmox bridge address" ip -4 address show dev vmbr-private
  if ((local_prod_config_ready)); then
    run_shell "Exactly one dedicated migration/replication address matches this node" \
      'python3 - "$1" "$2" "$3" "$4" "$(ip -j -4 address show dev vmbr-private)" <<'"'"'PY'"'"'
import ipaddress
import json
import re
import sys

node, network_text, start_text, end_text, raw = sys.argv[1:]
match = re.fullmatch(r"mox([1-9]|[12][0-9]|3[0-2])", node)
if not match:
    raise SystemExit(f"invalid mox node name: {node}")
network = ipaddress.ip_network(network_text, strict=True)
start = ipaddress.ip_address(start_text)
end = ipaddress.ip_address(end_text)
expected = start + int(match.group(1)) - 1
if expected > end or expected not in network:
    raise SystemExit("derived migration address is outside configured policy")
addresses = [
    ipaddress.ip_address(info["local"])
    for row in json.loads(raw)
    for info in row.get("addr_info", [])
    if info.get("family") == "inet"
]
matching = [address for address in addresses if address in network]
if matching != [expected]:
    raise SystemExit(
        f"{network} must match exactly {expected} on {node}; found {matching}"
    )
print(f"{node}: {expected} is the only address matching {network}")
PY' \
      "$node" "$expected_migration_network" \
      "$expected_replication_ip_start" "$expected_replication_ip_end"
  else
    printf '\n[SKIP] Dedicated migration-address validation needs the cluster settings.\n'
  fi
else
  printf '\n[INFO] vmbr-private is not present on this node.\n'
fi
run_if_command tailscale "This node's Tailscale IPv4 address" tailscale ip -4
run_if_command tailscale "Tailscale peer/status view" tailscale status

for target in "${candidate_hosts[@]}"; do
  printf '\n[LOOKUP] %s IPv4 resolution\n' "$target"
  getent ahostsv4 "$target" || true
done

section "Strict host-to-host SSH trust" \
  "the workflows reach other members through /etc/bmac/ssh_config (private address, cluster root key, pinned host key); -n prevents nested SSH from consuming this diagnostic script's stdin."
if [[ -f /etc/bmac/ssh_config && ! -L /etc/bmac/ssh_config ]]; then
  for target in "${candidate_hosts[@]}"; do
    [[ "$target" == "$node" ]] && continue
    run "Strict root SSH trust from $node to $target" \
      ssh -n -F /etc/bmac/ssh_config -o ConnectTimeout=4 -o ConnectionAttempts=1 "$target" true
  done
else
  printf '\n[FAIL] /etc/bmac/ssh_config is missing or unsafe; host-to-host SSH trust tests are skipped.\n'
fi

section "Proxmox cluster membership and quorum" \
  "this is the core cluster-health view: membership, expected votes, quorum, Corosync links, and QDevice participation should agree across nodes."
run_if_command pvecm "Proxmox cluster/quorum status" pvecm status
run_if_command pvecm "Proxmox node membership" pvecm nodes
if [[ -r /etc/pve/.members ]]; then
  run "pmxcfs member database" cat /etc/pve/.members
else
  printf '\n[INFO] /etc/pve/.members is not readable.\n'
fi
if [[ -r /etc/pve/corosync.conf ]]; then
  run "Current shared Corosync configuration" cat /etc/pve/corosync.conf
else
  printf '\n[INFO] /etc/pve/corosync.conf is not readable/present.\n'
fi
if [[ -r /etc/pve/datacenter.cfg ]]; then
  run "Shared Proxmox migration/replication configuration" \
    cat /etc/pve/datacenter.cfg
else
  printf '\n[FAIL] /etc/pve/datacenter.cfg is not readable/present.\n'
fi
run_if_command corosync-quorumtool "Corosync quorum view" corosync-quorumtool -s
run_if_command corosync-cfgtool "Corosync link/ring status" corosync-cfgtool -s
run_if_command corosync-qdevice-tool "Corosync QDevice client status" corosync-qdevice-tool -s

section "Core Proxmox, Corosync, HA, and custom service states" \
  "a healthy quorum is not enough if pmxcfs, HA daemons, routing, keepalived, or your bmac synchronization services are stopped."
for unit in \
  pve-cluster.service \
  pvedaemon.service \
  pveproxy.service \
  pvestatd.service \
  corosync.service \
  corosync-qdevice.service \
  pve-ha-lrm.service \
  pve-ha-crm.service \
  keepalived.service \
  bmac-guest-egress.service \
  bmac-haproxy-route-sync.timer \
  bmac-haproxy-ingress.service \
  bmac-deferred-cleanup.timer
do
  unit_report "$unit"
done

if command -v keepalived >/dev/null 2>&1 && [[ -r /etc/keepalived/keepalived.conf ]]; then
  run "Parse/validate keepalived configuration without starting it" \
    keepalived --config-test --use-file=/etc/keepalived/keepalived.conf
else
  printf '\n[INFO] keepalived or /etc/keepalived/keepalived.conf is absent; config validation skipped.\n'
fi

section "Production ISO preparation readiness" \
  "prove this node has the newly installed URL/cache/build tooling and matching configuration required before add_prod_vm.sh is resumed."
prod_iso_ready=1
readiness_pass() {
  printf '  [PASS] %s\n' "$1"
}
readiness_fail() {
  printf '  [FAIL] %s\n' "$1"
  prod_iso_ready=0
}

for command_name in cp curl df findmnt flock jq python3 sha256sum stat xorriso; do
  if command -v "$command_name" >/dev/null 2>&1; then
    readiness_pass "Required command is available: $command_name"
  else
    readiness_fail "Required command is missing: $command_name"
  fi
done

iso_preparer=/usr/lib/bmac/legacy/scripts/host_runtime/prepare_prod_iso.sh
iso_builder=/usr/lib/bmac/legacy/scripts/host_runtime/build_ubuntu_autoinstall.py
iso_cache=/var/lib/bmac/iso-cache

if [[ -x "$iso_preparer" && ! -L "$iso_preparer" ]]; then
  readiness_pass "ISO preparer is an executable non-symlink: $iso_preparer"
  if bash -n "$iso_preparer"; then
    readiness_pass "ISO preparer shell syntax is valid"
  else
    readiness_fail "ISO preparer shell syntax is invalid"
  fi
else
  readiness_fail "ISO preparer is missing, non-executable, or a symlink"
fi

if [[ -x "$iso_builder" && ! -L "$iso_builder" ]]; then
  readiness_pass "ISO builder is an executable non-symlink: $iso_builder"
  if PYTHONDONTWRITEBYTECODE=1 python3 "$iso_builder" --help >/dev/null; then
    readiness_pass "ISO builder starts successfully"
  else
    readiness_fail "ISO builder failed its --help smoke test"
  fi
else
  readiness_fail "ISO builder is missing, non-executable, or a symlink"
fi

if ((local_prod_config_ready)); then
  observed_iso_preparer_sha256=""
  if [[ -f "$iso_preparer" ]]; then
    observed_iso_preparer_sha256="$(
      sha256sum "$iso_preparer" | awk '{print $1}'
    )"
  fi
  if [[ -n "$observed_iso_preparer_sha256" &&
        "$observed_iso_preparer_sha256" == "$expected_iso_preparer_sha256" ]]; then
    readiness_pass "Installed ISO preparer matches the reporting host"
  else
    readiness_fail "Installed ISO preparer differs from the reporting host"
  fi
  observed_iso_builder_sha256=""
  if [[ -f "$iso_builder" ]]; then
    observed_iso_builder_sha256="$(
      sha256sum "$iso_builder" | awk '{print $1}'
    )"
  fi
  if [[ -n "$observed_iso_builder_sha256" &&
        "$observed_iso_builder_sha256" == "$expected_iso_builder_sha256" ]]; then
    readiness_pass "Installed ISO builder matches the reporting host"
  else
    readiness_fail "Installed ISO builder differs from the reporting host"
  fi
else
  printf '  [INFO] Checkout comparison unavailable; installed file identity was not checked.\n'
fi

if ((remote_prod_config_loaded)); then
  readiness_pass "Installed cluster configuration loads without secrets"
  printf '  Installed URL:          %s\n' "$remote_prod_iso_url"
  printf '  Installed source hash:  %s\n' "$remote_prod_iso_sha256"
  printf '  Installed install mode: %s\n' "$remote_prod_install_mode"
  printf '  Installed config hash:  %s\n' "$remote_cluster_config_sha256"
  if ((local_prod_config_ready)); then
    if [[ -z "$remote_prod_iso_sha256" && "$expected_prod_iso_sha256" == unavailable ]]; then
      readiness_pass "Neither this host nor the reporting host has a production guest ISO configured"
    elif [[ "$remote_prod_iso_sha256" == "$expected_prod_iso_sha256" ]]; then
      readiness_pass "Installed source SHA-256 matches the reporting host"
    else
      readiness_fail "Installed source SHA-256 differs from the reporting host"
    fi
    [[ "$remote_prod_install_mode" == "$expected_prod_install_mode" ]] &&
      readiness_pass "Installed install mode matches the reporting host" ||
      readiness_fail "Installed install mode differs from the reporting host"
    [[ "$remote_cluster_config_sha256" == "$expected_cluster_config_sha256" ]] &&
      readiness_pass "Installed non-secret config hash matches the reporting host" ||
      readiness_fail "Installed non-secret config hash differs from the reporting host"
  fi
else
  readiness_fail "Installed cluster configuration could not be loaded"
fi

if [[ -d "$iso_cache" && ! -L "$iso_cache" ]]; then
  cache_identity="$(stat -c '%U:%G:%a' "$iso_cache" 2>/dev/null || true)"
  [[ "$cache_identity" == root:root:700 ]] &&
    readiness_pass "ISO cache is root:root mode 0700" ||
    readiness_fail "ISO cache identity is ${cache_identity:-unavailable}, expected root:root:700"
  cache_fstype="$(findmnt -n -o FSTYPE --target "$iso_cache" 2>/dev/null || true)"
  case "$cache_fstype" in
    "" | tmpfs | ramfs)
      readiness_fail "ISO cache is not proven disk-backed (fstype=${cache_fstype:-unknown})"
      ;;
    *)
      readiness_pass "ISO cache is disk-backed (fstype=$cache_fstype)"
      ;;
  esac
  run "ISO cache filesystem capacity" df -hT "$iso_cache"
  run_shell "ISO cache inventory (names, sizes, owners, modes)" \
    'find /var/lib/bmac/iso-cache -maxdepth 1 -type f -printf "%M %u:%g %s %f\n" | sort'
else
  readiness_fail "Root-only ISO cache directory is missing or is a symlink"
fi

if ((remote_prod_config_loaded)) &&
  [[ "$remote_prod_iso_sha256" =~ ^[0-9a-f]{64}$ ]]; then
  cached_iso="${iso_cache}/${remote_prod_iso_sha256}.iso"
  cached_sidecar="${cached_iso}.sha256"
  if [[ -e "$cached_iso" || -e "$cached_sidecar" ]]; then
    if [[ -f "$cached_iso" && ! -L "$cached_iso" &&
          -f "$cached_sidecar" && ! -L "$cached_sidecar" ]]; then
      observed_cache_hash="$(sha256sum "$cached_iso" | awk '{print $1}')"
      expected_sidecar="${remote_prod_iso_sha256}  ${remote_prod_iso_sha256}.iso"
      observed_sidecar="$(cat "$cached_sidecar")"
      [[ "$observed_cache_hash" == "$remote_prod_iso_sha256" ]] &&
        readiness_pass "Existing cached ISO matches the configured SHA-256" ||
        readiness_fail "Existing cached ISO is corrupt or belongs to another hash"
      [[ "$observed_sidecar" == "$expected_sidecar" ]] &&
        readiness_pass "Existing cache checksum sidecar is exact" ||
        readiness_fail "Existing cache checksum sidecar is missing or incorrect"
    else
      readiness_fail "Source cache is partial: ISO and .sha256 sidecar must both be regular files"
    fi
  else
    printf '  [INFO] No source ISO is cached yet; the first production creation will download and verify it.\n'
  fi
fi

if [[ -n "$remote_iso_storage" ]] &&
  storage_json="$(pvesh get "/storage/${remote_iso_storage}" --output-format json 2>/dev/null)" &&
  jq -e '
    .type == "dir" and
    (.content | type == "string") and
    (.content | split(",") | index("iso") != null)
  ' <<<"$storage_json" >/dev/null; then
  readiness_pass "Configured ISO storage $remote_iso_storage is a dir backend allowing ISO content"
else
  readiness_fail "Configured ISO storage is unavailable or not a dir backend allowing ISO content"
fi

if ((remote_prod_config_loaded)) && command -v curl >/dev/null 2>&1; then
  if curl \
    --proto '=https' \
    --proto-redir '=https' \
    --fail \
    --location \
    --silent \
    --show-error \
    --head \
    --connect-timeout 10 \
    --max-time 30 \
    "$remote_prod_iso_url" >/dev/null; then
    readiness_pass "Configured source URL is reachable over HTTPS from this node"
  else
    readiness_fail "Configured source URL failed the bounded HTTPS HEAD check"
  fi
fi

if ((prod_iso_ready)); then
  printf '\n  [READY] %s is ready for production ISO download/cache/build and add_prod_vm.sh resume.\n' "$node"
else
  printf '\n  [NOT READY] %s has one or more production ISO preparation failures above.\n' "$node"
fi

section "Proxmox HA, replication, storage, and guest inventory" \
  "show where HA resources think they belong, whether replication jobs are healthy, storage availability, and what guests exist on this node."
run_if_command ha-manager "HA manager status" ha-manager status
run_if_command pvesr "Proxmox replication status" pvesr status
run_if_command pvesm "Proxmox storage status" pvesm status
run_if_command qm "QEMU VM inventory" qm list
run_if_command pct "LXC inventory" pct list

section "ZFS pool health and topology" \
  "your hosts use mirrored ZFS storage; these views expose DEGRADED/FAULTED devices, checksum/read/write errors, pool capacity, and exact leaf-device paths."
if command -v zpool >/dev/null 2>&1; then
  run "All ZFS pools and capacity" zpool list -v
  run "Concise health result for all ZFS pools" zpool status -x
  run "Full ZFS topology using persistent/full device paths" zpool status -P
  run "Verbose ZFS errors/topology" zpool status -v
  run_shell "Selected pool properties" \
    'pools=$(zpool list -H -o name 2>/dev/null || true); if [ -n "$pools" ]; then zpool get -o name,property,value,source health,autotrim,autoexpand $pools; else echo "No ZFS pools found."; fi'
else
  printf '\n[WARN] zpool command is not installed.\n'
fi

if command -v zfs >/dev/null 2>&1; then
  run "ZFS datasets/zvols and space usage" zfs list -o name,type,used,avail,refer,mountpoint
fi

section "Block devices, LUKS encryption, and mirror-member sizing evidence" \
  "ZFS mirror leaves should map to the expected disks and equal-capacity members. LUKS is optional, so this reports both raw crypto_LUKS devices and active dm-crypt mappings without attempting to unlock anything."
run "Block-device topology, exact byte sizes, filesystem/encryption types, model and serial" \
  lsblk -b -e7 -o NAME,PATH,TYPE,SIZE,FSTYPE,MOUNTPOINTS,MODEL,SERIAL
run_shell "Underlying devices carrying LUKS headers" \
  'if command -v blkid >/dev/null 2>&1; then blkid -t TYPE=crypto_LUKS -o device 2>/dev/null || true; fi'
if command -v dmsetup >/dev/null 2>&1; then
  run "Active dm-crypt device-mapper mappings" dmsetup ls --target crypt
fi
if command -v cryptsetup >/dev/null 2>&1; then
  run_shell "Detailed status for every active dm-crypt mapping" \
    'found=0; while read -r name rest; do [ -n "$name" ] || continue; found=1; echo; echo "### $name"; cryptsetup status "$name" || true; done < <(dmsetup ls --target crypt 2>/dev/null || true); if [ "$found" -eq 0 ]; then echo "No active dm-crypt mappings."; fi'
else
  printf '\n[INFO] cryptsetup is not installed; LUKS mapping details skipped.\n'
fi

section "Physical disk SMART health" \
  "ZFS can report pool-level faults, while SMART can expose underlying drive health warnings before ZFS necessarily marks a device failed."
if command -v smartctl >/dev/null 2>&1; then
  while IFS= read -r disk; do
    [[ -n "$disk" ]] || continue
    run "SMART health/attributes for $disk" smartctl -H -A "$disk"
  done < <(lsblk -dnpo PATH,TYPE | awk '$2 == "disk" {print $1}')
else
  printf '\n[INFO] smartctl is not installed; physical disk SMART checks skipped.\n'
fi

section "Boot configuration and filesystem capacity" \
  "a redundant pool is not sufficient if boot partitions are stale or filesystems/inodes are exhausted."
run_if_command proxmox-boot-tool "Proxmox boot synchronization/status" proxmox-boot-tool status
run "Mounted filesystem capacity" df -hT
run "Mounted filesystem inode usage" df -ih

section "Custom guest-egress and HAProxy ingress nftables" \
  "your custom HA design depends on these runtime rule tables; listing them verifies their current programmed state without changing rules."
if command -v nft >/dev/null 2>&1; then
  run "bmac_guest_egress IPv4 table" nft list table ip bmac_guest_egress
  run "bmac_haproxy_ingress inet table" nft list table inet bmac_haproxy_ingress
else
  printf '\n[INFO] nft is not installed.\n'
fi

section "Local HAProxy LXC" \
  "each moxN is expected to use LXC VMID 9110+N; this checks container state, HAProxy config validity, and host-vs-container generation synchronization."
if [[ -z "$vmid" ]]; then
  printf '\n[SKIP] Cannot derive HAProxy LXC VMID because hostname %q does not match moxN.\n' "$node"
elif ! command -v pct >/dev/null 2>&1; then
  printf '\n[SKIP] pct is not installed.\n'
else
  status="$(pct status "$vmid" 2>&1)"
  rc=$?
  printf '\n[CHECK] LXC %s status\n  $ pct status %q\n%s\n[exit %d]\n' "$vmid" "$vmid" "$status" "$rc"
  if (( rc == 0 )) && grep -q 'status: running' <<<"$status"; then
    run "HAProxy service inside LXC $vmid" pct exec "$vmid" -- systemctl is-active haproxy
    run "HAProxy configuration syntax inside LXC $vmid" pct exec "$vmid" -- haproxy -c -f /etc/haproxy/haproxy.cfg
    if [[ -r /run/bmac-haproxy-ingress-current ]]; then
      run "Host's current HAProxy ingress generation" cat /run/bmac-haproxy-ingress-current
    else
      printf '\n[INFO] /run/bmac-haproxy-ingress-current is absent on the host.\n'
    fi
    run "HAProxy LXC's active generation" pct exec "$vmid" -- cat /etc/haproxy/bmac-active-generation
  else
    printf '\n[INFO] LXC %s is absent or not running, so in-container HAProxy checks are skipped.\n' "$vmid"
  fi
fi

section "Failed units and recent errors" \
  "surface systemd failures and recent errors from this boot; these often explain otherwise-mysterious cluster, storage, or networking symptoms."
run "Failed systemd units" systemctl --failed --no-legend --no-pager
run "Recent Corosync journal" journalctl -b -u corosync.service --no-pager -n 120
run "Recent boot errors" journalctl -b -p err..alert --no-pager -n 120

section "Locale" \
  "capture locale because parsing/automation bugs can be caused by unexpected localized command output."
run "Locale environment" locale
MOX
done

# The ingress registry is cluster-shared, so querying it once is enough. Use the
# first reachable candidate as the inspection point.
registry_host="$THIS_HOST"
control_list_contains "$THIS_HOST" "${reachable_mox[@]}" || registry_host="${reachable_mox[0]}"
banner "SHARED INGRESS REGISTRY - queried via ${registry_host}"
note "Why: this registry is shared cluster state, so one healthy Proxmox member can report it without repeating identical output from every node."
mox_ssh "$registry_host" bash -s <<'REGISTRY'
set -u
path=/usr/lib/bmac/legacy/scripts/host_runtime/cluster_registry.py
if [[ -x "$path" ]]; then
  printf '[CHECK] Shared ingress registry status\n'
  printf '  $ %q ingress-status\n' "$path"
  "$path" ingress-status
  rc=$?
  printf '[exit %d]\n' "$rc"

  printf '\n[CHECK] Registered and live production/staging guest inventory\n'
  resources="$("$path" list)"
  live="$(pvesh get /cluster/resources --type vm --output-format json)"
  routes="$("$path" list-routes)"
  python3 - "$resources" "$live" "$routes" <<'PY'
import json
import sys

resources, live, routes = map(json.loads, sys.argv[1:])
live_by_vmid = {
    int(row["vmid"]): row
    for row in live
    if str(row.get("vmid", "")).isdigit()
}
route_counts = {}
for route in routes:
    route_counts[route["resource"]] = route_counts.get(route["resource"], 0) + 1

print(
    f"{'NAME':<12} {'KIND':<11} {'REGISTRY':<16} {'VMID':>6} "
    f"{'LIVE':<9} {'OWNER':<8} {'IP':<16} {'ROUTES':>6} PLACEMENT"
)
for resource in sorted(
    resources,
    key=lambda row: (row["kind"], int(row["vmid"])),
):
    vmid = int(resource["vmid"])
    observed = live_by_vmid.get(vmid, {})
    live_state = observed.get("status", "absent")
    live_owner = observed.get("node", "-")
    placement = ",".join(resource["placement"])
    print(
        f"{resource['name']:<12} {resource['kind']:<11} "
        f"{resource['state']:<16} {vmid:>6} {live_state:<9} "
        f"{live_owner:<8} {resource['ip']:<16} "
        f"{route_counts.get(resource['name'], 0):>6} {placement}"
    )

snapshot_users = {}
for resource in resources:
    snapshot = (resource.get("proxmox") or {}).get("snapshot")
    if resource["kind"] == "staging" and isinstance(snapshot, dict):
        key = (resource["source"], snapshot["name"])
        snapshot_users.setdefault(key, []).append(resource["name"])
if snapshot_users:
    print("\nSTAGING SOURCE SNAPSHOTS:")
    print(f"  {'SOURCE':<8} {'SNAPSHOT':<40} {'TAKEN':<20} STAGING VMS")
    for (source, name), users in sorted(snapshot_users.items()):
        stamp = name.rsplit("-", 1)[-1]
        taken = (
            f"20{stamp[0:2]}-{stamp[2:4]}-{stamp[4:6]} {stamp[7:9]}:{stamp[9:11]} UTC"
            if len(stamp) == 14 and stamp[6] == "T" and stamp.endswith("Z")
            else "unknown"
        )
        print(f"  {source:<8} {name:<40} {taken:<20} {', '.join(sorted(users))}")

registered_vmids = {int(row["vmid"]) for row in resources}
unexpected = [
    row for row in live
    if (
        row.get("type") == "qemu"
        and str(row.get("vmid", "")).isdigit()
        and int(row["vmid"]) not in registered_vmids
    )
]
if unexpected:
    print("\nUNREGISTERED QEMU VMS:")
    for row in sorted(unexpected, key=lambda value: int(value["vmid"])):
        print(
            f"  vmid={row['vmid']} name={row.get('name', '?')} "
            f"node={row.get('node', '?')} status={row.get('status', '?')}"
        )
else:
    print("\nNo unregistered QEMU VMs were observed.")
PY
  printf '[exit %d]\n' "$?"
else
  printf '[INFO] %s is absent or not executable; shared ingress registry check skipped.\n' "$path"
fi
REGISTRY

FINISHED_AT="$(date +%Y-%m-%dT%H:%M:%S%z)"
banner "DIAGNOSTIC RUN COMPLETE"
note "Started:  $STARTED_AT"
note "Finished: $FINISHED_AT"
note "Reachable Proxmox hosts inspected: ${reachable_mox[*]}"
if ((${#unreachable_mox[@]})); then
  note "Members that were unreachable/skipped: ${unreachable_mox[*]}"
fi
if (( qdevice_reachable )); then
  note "QDevice inspected: $QDEVICE_HOST"
else
  note "QDevice skipped as unreachable: $QDEVICE_HOST"
fi
note "No configuration-changing commands are intentionally used by this script."
bmac_ui_step_done
bmac_ui_result started_at "$STARTED_AT" finished_at "$FINISHED_AT" \
  inspected_hosts:raw "$(bmac_ui_json_array "${reachable_mox[@]}")" \
  unreachable_hosts:raw "$(bmac_ui_json_array "${unreachable_mox[@]}")" \
  qdevice_inspected:bool "$( ((qdevice_reachable)) && echo true || echo false)"
