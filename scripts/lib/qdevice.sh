#!/usr/bin/env bash

# Copyright (c) 2026 BEENTHERE VENTURES, INC.
# SPDX-License-Identifier: GPL-3.0-only

# The one implementation of the cluster's external QDevice vote, shared by
# hosts/add_proxmox_host.sh, hosts/remove_proxmox_host.sh,
# qdevice/add_qdevice.sh, qdevice/remove_qdevice.sh, and the diagnostics:
# verifying access to the QDevice host, preparing corosync-qnetd on it, pinning
# its SSH host key on the members, adding, removing, and reconciling the vote,
# clearing stale registrations, forgetting a retired QDevice, and checking vote
# layouts.
#
# The caller defines log and info, sets PROXMOX_QDEVICE_HOST, and defines how
# to reach the cluster:
#   qd_coordinator               print the member that runs pvecm commands
#   qd_exec NODE ARGV...         run ARGV as root on member NODE; ARGV gets no
#                                stdin
#   qd_exec_coordinator_tty CMD  run command string CMD on the coordinator with
#                                a terminal
#   qd_member_states             print "NODE online|offline" for every member
# qd_remove calls qd_confirm_removal MESSAGE, when the caller defines it,
# before it gracefully removes a QDevice for the reason in MESSAGE.
#
# Removal is graceful when the QDevice is accessible: this workstation reaches
# it as root and it is the machine the cluster registered. Otherwise it is
# forced: the operator must agree, remove the machine from Tailscale, and
# confirm that, because the machine keeps this cluster's QDevice certificates.

# shellcheck source=./ui_protocol.sh
declare -F bmac_ui_is_json >/dev/null ||
  source "$(dirname -- "${BASH_SOURCE[0]}")/ui_protocol.sh"

# shellcheck source=apt_lock_wait.sh
source "$(dirname -- "${BASH_SOURCE[0]}")/apt_lock_wait.sh"

# The verified Tailscale IPv4 of PROXMOX_QDEVICE_HOST, set by qd_probe_access;
# empty while the QDevice is not accessible.
QD_IPV4=""
# Why the QDevice is not accessible, set by qd_probe_access.
QD_ACCESS_PROBLEM=""
# How the last qd_remove removed the QDevice: graceful, forced, or empty.
QD_REMOVAL=""

qd_fail() {
  printf '\nERROR: %s\n' "$*" >&2
  exit 1
}

qd_warn() {
  printf 'WARNING: %s\n' "$*" >&2
}

# Run a command string as root on the QDevice host with the operator's
# workstation SSH identity.
qd_ssh() {
  ssh -o BatchMode=yes -o StrictHostKeyChecking=yes -o CheckHostIP=yes \
    -o ConnectTimeout=10 "root@${PROXMOX_QDEVICE_HOST}" "$@"
}

pvecm_status_value() {
  awk -v key="$2" '$0 ~ "^" key ":" { print $NF; exit }' <<<"$1"
}

pvecm_status_is_quorate() {
  grep -Eq '^Quorate:[[:space:]]+Yes[[:space:]]*$' <<<"$1"
}

pvecm_status_has_qdevice() {
  grep -Eq '^Flags:.*(^|[[:space:]])Qdevice([[:space:]]|$)' <<<"$1"
}

# Every one of NODE_COUNT members votes, the QDevice votes once, and the
# expected and total votes count both.
qd_status_is_healthy() {
  local status=$1 node_count=$2 qdevice_voters
  [[ "$(pvecm_status_value "$status" 'Expected votes')" == "$((node_count + 1))" ]] ||
    return 1
  [[ "$(pvecm_status_value "$status" 'Total votes')" == "$((node_count + 1))" ]] ||
    return 1
  pvecm_status_is_quorate "$status" || return 1
  pvecm_status_has_qdevice "$status" || return 1
  qdevice_voters="$(
    awk '
      $1 ~ /^0x[0-9a-fA-F]+$/ && $1 != "0x00000000" &&
      $2 == 1 && $3 ~ /^A,V,/ { count++ }
      END { print count + 0 }
    ' <<<"$status"
  )"
  [[ "$qdevice_voters" == "$node_count" ]] || return 1
  awk '
    $1 == "0x00000000" && $2 == 1 && $3 == "Qdevice" { found++ }
    END { exit !(found == 1) }
  ' <<<"$status"
}

qd_status_absent_is_healthy() {
  local status=$1 node_count=$2
  [[ "$(pvecm_status_value "$status" 'Expected votes')" == "$node_count" &&
    "$(pvecm_status_value "$status" 'Total votes')" == "$node_count" ]] ||
    return 1
  pvecm_status_is_quorate "$status" || return 1
  ! pvecm_status_has_qdevice "$status"
}

# Check root SSH to the QDevice host and resolve its one Tailscale IPv4 into
# QD_IPV4. MagicDNS and the host itself must agree on the address. Returns 1,
# with the reason in QD_ACCESS_PROBLEM, when the QDevice is not accessible.
qd_probe_access() {
  local resolved remote_ip error
  QD_IPV4=""
  QD_ACCESS_PROBLEM=""
  if ! error="$(qd_ssh true 2>&1 </dev/null)"; then
    QD_ACCESS_PROBLEM="'ssh root@${PROXMOX_QDEVICE_HOST}' failed${error:+: ${error//$'\n'/ }}"
    return 1
  fi
  if ! resolved="$(
    tailscale status --json | jq -er --arg host "$PROXMOX_QDEVICE_HOST" '
      [
        .Peer | to_entries[] | .value |
        select(
          .HostName == $host or
          ((.DNSName // "") | rtrimstr(".") |
            (. == $host or startswith($host + ".")))
        ) |
        .TailscaleIPs[]? |
        select(type == "string" and test("^100\\.[0-9]+\\.[0-9]+\\.[0-9]+$"))
      ] | unique |
      if length == 1 then .[0]
      else error("QDevice hostname did not resolve to exactly one Tailscale IPv4")
      end
    ' 2>/dev/null
  )"; then
    QD_ACCESS_PROBLEM="${PROXMOX_QDEVICE_HOST} does not resolve to exactly one Tailscale IPv4"
    return 1
  fi
  if [[ ! "$resolved" =~ ^100\.([0-9]{1,3}\.){2}[0-9]{1,3}$ ]]; then
    QD_ACCESS_PROBLEM="${PROXMOX_QDEVICE_HOST} resolves to $resolved, which is not a Tailscale IPv4"
    return 1
  fi
  remote_ip="$(qd_ssh "tailscale ip -4" </dev/null | awk '/^100\./ { print; exit }')" || true
  if [[ "$remote_ip" != "$resolved" ]]; then
    QD_ACCESS_PROBLEM="MagicDNS resolves ${PROXMOX_QDEVICE_HOST} to $resolved, but that host reports ${remote_ip:-no Tailscale IPv4}"
    return 1
  fi
  QD_IPV4="$resolved"
}

qd_verify_access() {
  qd_probe_access || qd_fail "${QD_ACCESS_PROBLEM^}"
}

# Stop unless the QDevice is accessible when NODE_COUNT members need one.
qd_require_access_for() {
  local node_count="$1"
  ((node_count % 2 == 1)) || [[ "$QD_IPV4" =~ ^100\. ]] ||
    qd_fail "The ${node_count}-member cluster needs a QDevice, but ${PROXMOX_QDEVICE_HOST} is not accessible (${QD_ACCESS_PROBLEM:-not verified}). Fix access to it, or replace it with qdevice/remove_qdevice.sh, qdevice/QDEVICE_MANUAL_SETUP.md, and qdevice/add_qdevice.sh, then rerun."
}

# Install corosync-qnetd on the QDevice host when it is missing, then require
# it to be active and listening on TCP 5403.
qd_prepare_qnetd() {
  [[ "$QD_IPV4" =~ ^100\. ]] ||
    qd_fail "A verified Tailscale IPv4 is required to prepare the QDevice"
  info "Verifying the corosync-qnetd service on ${PROXMOX_QDEVICE_HOST}"
  # shellcheck disable=SC2029 # QD_IPV4 is a validated literal IPv4.
  qd_ssh "flock -w 1800 /run/lock/app-ha-qdevice-provision.lock bash -s -- '$QD_IPV4'" < <(
    printf '%s\n' "$APT_LOCK_WAIT_FUNCTIONS"
    cat <<'REMOTE'
set -Eeuo pipefail
expected_tailscale_ip="$1"
actual_tailscale_ip="$(tailscale ip -4 | awk '/^100\./ { print; exit }')"
[[ "$actual_tailscale_ip" == "$expected_tailscale_ip" ]]
if ! dpkg-query -W -f='${Status}\n' corosync-qnetd 2>/dev/null |
     grep -Fxq 'install ok installed'; then
  apt_wait_for_locks
  # The timeout covers another process taking the lock just after the wait above.
  if ! apt-get -o DPkg::Lock::Timeout=60 update; then
    rm -f /var/cache/apt/pkgcache.bin /var/cache/apt/pkgcache.bin.* \
      /var/cache/apt/srcpkgcache.bin /var/cache/apt/srcpkgcache.bin.*
    apt-get -o DPkg::Lock::Timeout=60 update
  fi
  DEBIAN_FRONTEND=noninteractive apt-get -o DPkg::Lock::Timeout=60 install -y corosync-qnetd
fi
systemctl enable --now corosync-qnetd
for _ in $(seq 1 10); do
  systemctl is-active --quiet corosync-qnetd &&
    ss -lnt | awk '$4 ~ /:5403$/ { found=1 } END { exit !found }' &&
    exit 0
  sleep 1
done
systemctl --no-pager --full status corosync-qnetd >&2 || true
printf 'corosync-qnetd did not become active and listen on TCP 5403\n' >&2
exit 1
REMOTE
  ) || qd_fail "Could not prepare corosync-qnetd on ${PROXMOX_QDEVICE_HOST}"
}

# Add or remove the coordinator's root RSA key in the QDevice host's
# authorized_keys; pvecm qdevice setup copies its certificates over that
# trust. Adding creates the key when the coordinator has none. Returns the
# QDevice SSH status so callers decide whether a failure is fatal.
qd_setup_key() {
  local mode="$1" coordinator key key_b64
  coordinator="$(qd_coordinator)"
  if [[ "$mode" == add ]]; then
    qd_exec "$coordinator" bash -c \
      'test -f /root/.ssh/id_rsa.pub || ssh-keygen -q -t rsa -b 4096 -N "" -f /root/.ssh/id_rsa' ||
      qd_fail "Could not prepare $coordinator's QDevice setup SSH key"
  elif ! qd_exec "$coordinator" test -f /root/.ssh/id_rsa.pub; then
    return 0
  fi
  key="$(qd_exec "$coordinator" cat /root/.ssh/id_rsa.pub)" ||
    qd_fail "Could not read $coordinator's QDevice setup SSH key"
  [[ "$key" == ssh-rsa\ * ]] ||
    qd_fail "$coordinator's QDevice setup SSH key is not an RSA public key"
  key_b64="$(printf '%s' "$key" | base64 | tr -d '\n')"
  # shellcheck disable=SC2029 # key_b64 is restricted to base64 output.
  qd_ssh "KEY_B64='$key_b64' MODE='$mode' bash -s" <<'REMOTE'
set -Eeuo pipefail
key="$(printf '%s' "$KEY_B64" | base64 -d)"
install -d -m 0700 /root/.ssh
touch /root/.ssh/authorized_keys
chmod 0600 /root/.ssh/authorized_keys
if [[ "$MODE" == add ]]; then
  grep -Fxq "$key" /root/.ssh/authorized_keys ||
    printf '%s\n' "$key" >>/root/.ssh/authorized_keys
else
  work="$(mktemp /root/.ssh/authorized_keys.app-ha.XXXXXX)"
  awk -v key="$key" '$0 != key { print }' /root/.ssh/authorized_keys >"$work"
  install -o root -g root -m 0600 "$work" /root/.ssh/authorized_keys
  rm -f "$work"
fi
REMOTE
}

# Replace the managed QDevice host-key block in a known_hosts file with one
# trusted line. Run on a member as:
#   bash -c "$QD_PIN_SCRIPT" bash TRUST_B64 KNOWN_HOSTS
QD_PIN_SCRIPT="$(
  cat <<'REMOTE'
set -Eeuo pipefail
trust="$(printf '%s' "$1" | base64 -d)"
known_hosts="$2"
begin='# BEGIN app-ha managed qdevice host key'
end='# END app-ha managed qdevice host key'
[[ "$trust" =~ ^[A-Za-z0-9._-]+,[0-9.]+\ ssh-ed25519\ [A-Za-z0-9+/]+={0,2}$ ]]
install -d -m 0700 "$(dirname -- "$known_hosts")"
touch "$known_hosts"
[[ -f "$known_hosts" && ! -L "$known_hosts" ]]
chmod 0600 "$known_hosts"
begin_count="$(grep -Fxc "$begin" "$known_hosts" || true)"
end_count="$(grep -Fxc "$end" "$known_hosts" || true)"
if ! { [[ "$begin_count" == 0 && "$end_count" == 0 ]] ||
  [[ "$begin_count" == 1 && "$end_count" == 1 ]]; }; then
  printf 'Managed QDevice host-key markers are unbalanced or duplicated in %s\n' \
    "$known_hosts" >&2
  exit 1
fi
if [[ "$begin_count" == 1 ]]; then
  begin_line="$(grep -Fxn "$begin" "$known_hosts" | cut -d: -f1)"
  end_line="$(grep -Fxn "$end" "$known_hosts" | cut -d: -f1)"
  ((begin_line < end_line)) || {
    printf 'Managed QDevice host-key markers are out of order in %s\n' "$known_hosts" >&2
    exit 1
  }
fi
work="$(mktemp "${known_hosts}.app-ha.XXXXXX")"
awk -v begin="$begin" -v end="$end" '
  $0 == begin { managed=1; next }
  $0 == end { managed=0; next }
  !managed { print }
' "$known_hosts" >"$work"
printf '%s\n%s\n%s\n' "$begin" "$trust" "$end" >>"$work"
chmod 0600 "$work"
mv -f -- "$work" "$known_hosts"
REMOTE
)"

# Pin the QDevice's ED25519 host key, read over the operator-verified
# workstation SSH connection, in every member's root known_hosts. pvecm
# qdevice setup connects to the QDevice by its Tailscale IPv4, and a
# replacement QDevice has a new host key.
qd_pin_host_key() {
  local host_key key_type key_data trust trust_b64 node state states
  [[ "$QD_IPV4" =~ ^100\. ]] ||
    qd_fail "A verified Tailscale IPv4 is required to pin the QDevice host key"
  host_key="$(qd_ssh cat /etc/ssh/ssh_host_ed25519_key.pub </dev/null)" ||
    qd_fail "Could not read the ED25519 host key of ${PROXMOX_QDEVICE_HOST}"
  read -r key_type key_data _ <<<"$host_key"
  [[ "$key_type" == ssh-ed25519 && "$key_data" =~ ^[A-Za-z0-9+/]+={0,2}$ ]] ||
    qd_fail "${PROXMOX_QDEVICE_HOST} returned an invalid ED25519 host key"
  info "QDevice host key: $(printf '%s %s\n' "$key_type" "$key_data" | ssh-keygen -lf -)"
  trust="${PROXMOX_QDEVICE_HOST},${QD_IPV4} ${key_type} ${key_data}"
  trust_b64="$(printf '%s' "$trust" | base64 | tr -d '\n')"
  states="$(qd_member_states)" || qd_fail "Could not list cluster members"
  while read -r node state; do
    [[ -n "$node" ]] || continue
    [[ "$state" == online ]] || qd_fail "$node is not online"
    qd_exec "$node" bash -c "$QD_PIN_SCRIPT" bash "$trust_b64" \
      /root/.ssh/known_hosts ||
      qd_fail "Could not pin the QDevice host key on $node"
  done <<<"$states"
  info "Pinned the QDevice host key on every member"
}

# Remove every trace of a retired QDevice from one member: its QDevice client
# service and certificates, the managed host-key block, and known_hosts
# entries for its name or address, including the cluster-wide
# /etc/pve/priv/known_hosts. Run on a member as:
#   bash -c "$QD_FORGET_SCRIPT" bash NAME [ADDRESS]
QD_FORGET_SCRIPT="$(
  cat <<'REMOTE'
set -Eeuo pipefail
qdevice_name="$1"
qdevice_addr="${2:-}"
begin='# BEGIN app-ha managed qdevice host key'
end='# END app-ha managed qdevice host key'

filter_known_hosts() {
  local file="$1" work
  [[ -f "$file" && ! -L "$file" ]] || return 0
  work="$(mktemp)"
  awk -v name="$qdevice_name" -v addr="$qdevice_addr" -v begin="$begin" -v end="$end" '
    $0 == begin { managed = 1; next }
    $0 == end { managed = 0; next }
    managed { next }
    /^[[:space:]]*(#|$)/ { print; next }
    {
      hosts = ($1 ~ /^@/) ? $2 : $1
      count = split(hosts, names, ",")
      for (i = 1; i <= count; i++) {
        host = names[i]
        sub(/^\[/, "", host)
        sub(/\](:[0-9]+)?$/, "", host)
        if (host == name || (addr != "" && host == addr)) next
      }
      print
    }
  ' "$file" >"$work"
  if ! cmp -s "$work" "$file"; then
    cat "$work" >"$file"
    echo "Removed QDevice entries from $file"
  fi
  rm -f "$work"
}

if systemctl cat corosync-qdevice.service >/dev/null 2>&1; then
  systemctl disable --now corosync-qdevice.service >/dev/null 2>&1 || true
fi
rm -rf /etc/corosync/qdevice

filter_known_hosts /root/.ssh/known_hosts
for entry in "$qdevice_name" "$qdevice_addr"; do
  [[ -n "$entry" && -f /root/.ssh/known_hosts ]] || continue
  if ssh-keygen -F "$entry" -f /root/.ssh/known_hosts >/dev/null 2>&1; then
    ssh-keygen -R "$entry" -f /root/.ssh/known_hosts >/dev/null 2>&1
    echo "Removed hashed known_hosts entries for $entry"
  fi
done
rm -f /root/.ssh/known_hosts.old
filter_known_hosts /etc/pve/priv/known_hosts

if systemctl is-active --quiet corosync-qdevice.service 2>/dev/null; then
  echo "ERROR: corosync-qdevice.service is still active." >&2
  exit 41
fi
if [[ -e /etc/corosync/qdevice ]]; then
  echo "ERROR: /etc/corosync/qdevice still exists." >&2
  exit 42
fi
REMOTE
)"

# Forget a retired QDevice, named NAME at ADDRESS, on every member.
qd_forget() {
  local name="$1" address="${2:-}" node state states
  states="$(qd_member_states)" || qd_fail "Could not list cluster members"
  while read -r node state; do
    [[ -n "$node" ]] || continue
    [[ "$state" == online ]] || qd_fail "$node is not online"
    info "Removing QDevice associations from $node"
    qd_exec "$node" bash -c "$QD_FORGET_SCRIPT" bash "$name" "$address" ||
      qd_fail "Could not remove QDevice associations from $node"
  done <<<"$states"
}

# Return 0 when corosync.conf registers a QDevice, 1 when it does not, and 2
# when it cannot be read. corosync.conf, not pvecm status, decides: corosync
# can keep a removed QDevice registered until it restarts.
qd_check_registered() {
  local status=0
  qd_exec "$(qd_coordinator)" grep -Eq '^[[:space:]]*device[[:space:]]*[{]' \
    /etc/pve/corosync.conf || status=$?
  ((status <= 1)) || status=2
  return "$status"
}

# Succeed when a QDevice is registered; stop when that cannot be determined.
qd_is_registered() {
  local status=0
  qd_check_registered || status=$?
  case "$status" in
    0 | 1) return "$status" ;;
    *) qd_fail "Could not read /etc/pve/corosync.conf on $(qd_coordinator)" ;;
  esac
}

# Print the QDevice address registered in corosync.conf, or nothing.
qd_registered_address() {
  # shellcheck disable=SC2016 # awk program.
  qd_exec "$(qd_coordinator)" awk '
    /^[[:space:]]*device[[:space:]]*[{]/ { device=1 }
    device && /^[[:space:]]*host:/ { print $2; exit }
  ' /etc/pve/corosync.conf
}

# Print the first reason the members do not show the vote layout NODE_COUNT
# needs (the QDevice alive and voting for an even count, absent for an odd
# one), or nothing when they all do. Returns 1 when there is a reason.
qd_layout_problem() {
  local node_count="$1" node state status states members=0
  states="$(qd_member_states)" || {
    printf 'could not list cluster members\n'
    return 1
  }
  while read -r node state; do
    [[ -n "$node" ]] || continue
    members=$((members + 1))
    if [[ "$state" != online ]]; then
      printf '%s is not online\n' "$node"
      return 1
    fi
    if ! status="$(qd_exec "$node" pvecm status)"; then
      printf 'could not read cluster status from %s\n' "$node"
      return 1
    fi
    if ((node_count % 2 == 0)); then
      if ! qd_status_is_healthy "$status" "$node_count"; then
        printf '%s does not report the QDevice alive and voting with expected/total votes %s\n' \
          "$node" "$((node_count + 1))"
        return 1
      fi
    elif ! qd_status_absent_is_healthy "$status" "$node_count"; then
      printf '%s does not report %s votes with the QDevice absent\n' "$node" "$node_count"
      return 1
    fi
  done <<<"$states"
  if ((members != node_count)); then
    printf 'expected %s cluster members, found %s\n' "$node_count" "$members"
    return 1
  fi
}

qd_assert_layout() {
  local problem
  problem="$(qd_layout_problem "$1")" || qd_fail "${problem^}"
}

# Add the QDevice vote for a NODE_COUNT-member cluster: prepare qnetd, pin its
# host key on every member, run pvecm qdevice setup on the coordinator over a
# temporary SSH key, and require every member to see it alive and voting.
qd_add() {
  local node_count="$1" coordinator
  coordinator="$(qd_coordinator)"
  qd_require_access_for "$node_count"
  log "Adding the external QDevice vote for the ${node_count}-node cluster"
  info "This needs the Tailscale ACL that lets tag:proxmox-host reach tag:proxmox-qdevice on tcp:22."
  qd_prepare_qnetd
  qd_pin_host_key
  qd_setup_key add ||
    qd_fail "Could not authorize $coordinator's setup key on ${PROXMOX_QDEVICE_HOST}"
  qd_exec_coordinator_tty "pvecm qdevice setup '${QD_IPV4}' --force" ||
    qd_fail "pvecm qdevice setup failed on $coordinator"
  qd_setup_key remove ||
    qd_warn "Could not remove $coordinator's setup key from ${PROXMOX_QDEVICE_HOST}; remove it manually"
  qd_assert_layout "$node_count"
  info "Every member reports the QDevice alive and voting"
}

# Explain why the registered QDevice at ADDRESS cannot be removed gracefully,
# then require the operator to agree to a forced removal and to confirm that
# the machine was removed from Tailscale.
qd_confirm_forced_removal() {
  local reason="$1" address="$2" problem answer confirmation
  if [[ "$QD_IPV4" =~ ^100\. ]]; then
    problem="${PROXMOX_QDEVICE_HOST} is reachable, but its Tailscale address ($QD_IPV4) is not the registered one, so it is a different machine (for example a replacement prepared under the same name). It is left untouched."
  else
    problem="The QDevice is not accessible: ${QD_ACCESS_PROBLEM:-not verified}."
  fi
  cat <<EOF

==============================================================================
THE QDEVICE CANNOT BE REMOVED GRACEFULLY
==============================================================================
${reason:+
$reason
}
The cluster has the QDevice registered at ${address:-an unknown address}.
$problem

If the QDevice is healthy and only this workstation cannot reach it, answer
no, fix 'ssh root@${PROXMOX_QDEVICE_HOST}', and rerun.

A forced removal does not contact the QDevice machine. It runs
'pvecm qdevice remove', which removes the QDevice from the cluster
configuration and its client service and certificates from every Proxmox
host, and it removes every known_hosts entry for the QDevice from every
Proxmox host. The machine keeps this cluster's QDevice certificates, so you
must remove it from Tailscale first.

EOF
  if bmac_ui_is_json; then
    bmac_ui_confirm --id forced_removal --severity destructive \
      --title "Forcefully remove the QDevice from the cluster?" \
      --message "Forced removal does not contact the QDevice machine. You must remove it from Tailscale before continuing." \
      --confirm-label "Force removal" --cancel-label "Keep it" && answer=yes || answer=""
  else
    read -r -p "Forcefully remove the QDevice from the cluster? [y/N] " answer || answer=""
  fi
  [[ "${answer,,}" == y || "${answer,,}" == yes ]] ||
    qd_fail "Forced QDevice removal was declined; the QDevice is still registered"
  cat <<EOF

==============================================================================
REQUIRED: REMOVE THE OLD QDEVICE FROM TAILSCALE NOW
==============================================================================

The old QDevice machine must never be able to communicate with the cluster
again. Before continuing:

  1. In the Tailscale admin console, open Machines and find the old QDevice
     by its address${address:+ $address}. Its name may be
     '${PROXMOX_QDEVICE_HOST}' or a variant such as '${PROXMOX_QDEVICE_HOST}-1'. If a replacement
     already uses the name '${PROXMOX_QDEVICE_HOST}', do not remove the replacement.
  2. Use its menu to Remove it from the tailnet, and revoke any auth key that
     could re-register it.
  3. If the machine is ever recovered, wipe or reinstall it before it joins
     any network. Never reconnect it as it is.

Type exactly, in all-caps, once it has been removed: REMOVED FROM TAILSCALE
EOF
  if bmac_ui_is_json; then
    bmac_ui_confirm --id removed_from_tailscale --severity critical \
      --title "Remove the old QDevice from Tailscale now" \
      --message "The old QDevice machine must never communicate with the cluster again. Remove it from the tailnet in the Tailscale admin console and revoke any auth key that could re-register it." \
      --detail "Its name may be '${PROXMOX_QDEVICE_HOST}' or a variant such as '${PROXMOX_QDEVICE_HOST}-1'${address:+ (address $address)}; do not remove a replacement." \
      --detail "If the machine is ever recovered, wipe or reinstall it before it joins any network." \
      --confirm-label "It has been removed" --text "REMOVED FROM TAILSCALE" &&
      confirmation="REMOVED FROM TAILSCALE" || confirmation=""
  else
    printf '> '
    read -r confirmation || confirmation=""
  fi
  [[ "$confirmation" == "REMOVED FROM TAILSCALE" ]] ||
    qd_fail "Confirmation did not match; the QDevice is still registered"
}

# Remove the registered QDevice vote for the reason in MESSAGE, gracefully
# when the QDevice is accessible and forcefully otherwise, then clear any
# stale registration. Graceful removal leaves the QDevice machine ready to be
# added again; forced removal also makes every member forget it. Proxmox
# stops the qdevice service over SSH on every configured node, so an offline
# node can make the command report failure after it already removed the
# device from corosync.conf; the configuration, not the exit status, decides.
qd_remove() {
  local reason="${1:-}" coordinator address
  coordinator="$(qd_coordinator)"
  QD_REMOVAL=""
  qd_is_registered || return 0
  address="$(qd_registered_address)" ||
    qd_fail "Could not read the registered QDevice address on $coordinator"
  if [[ "$QD_IPV4" =~ ^100\. && "$address" == "$QD_IPV4" ]]; then
    if [[ -n "$reason" ]] && declare -F qd_confirm_removal >/dev/null; then
      qd_confirm_removal "$reason"
    fi
    QD_REMOVAL=graceful
    log "Removing the external QDevice vote"
  else
    qd_confirm_forced_removal "$reason" "$address"
    QD_REMOVAL=forced
    log "Forcefully removing the external QDevice vote"
  fi
  qd_exec "$coordinator" pvecm qdevice remove ||
    qd_warn "pvecm qdevice remove reported an error; checking corosync.conf"
  ! qd_is_registered ||
    qd_fail "corosync.conf still configures the QDevice; inspect 'pvecm status' on $coordinator and remove it with 'pvecm qdevice remove' before rerunning"
  info "The QDevice is no longer configured"
  qd_clear_stale
  if [[ "$QD_REMOVAL" == graceful ]]; then
    qd_setup_key remove ||
      qd_warn "Could not remove $coordinator's setup key from ${PROXMOX_QDEVICE_HOST}; remove it manually"
  else
    qd_forget "$PROXMOX_QDEVICE_HOST" "$address"
    info "Every member has forgotten the old QDevice. Before preparing a replacement,"
    info "run 'ssh-keygen -R ${PROXMOX_QDEVICE_HOST}' on this workstation."
  fi
}

# Corosync can keep a removed QDevice registered (the Qdevice flag with 0
# votes) until it restarts. Vote checks and a later host join would treat that
# as a configured QDevice, so restart corosync, one member at a time, on each
# member that still reports it.
qd_clear_stale() {
  local node state states status deadline
  ! qd_is_registered || return 0
  states="$(qd_member_states)" || qd_fail "Could not list cluster members"
  while read -r node state; do
    [[ -n "$node" ]] || continue
    [[ "$state" == online ]] || qd_fail "$node is not online"
    status="$(qd_exec "$node" pvecm status)" ||
      qd_fail "Could not read cluster status from $node"
    pvecm_status_has_qdevice "$status" || continue
    log "Restarting corosync on $node to clear the removed QDevice registration"
    qd_exec "$node" systemctl restart corosync.service ||
      qd_fail "Could not restart corosync on $node"
    deadline=$((SECONDS + 120))
    while true; do
      if status="$(qd_exec "$node" pvecm status 2>/dev/null)" &&
        pvecm_status_is_quorate "$status" && ! pvecm_status_has_qdevice "$status"; then
        break
      fi
      ((SECONDS < deadline)) ||
        qd_fail "$node is not quorate without a QDevice registration 120 seconds after restarting corosync"
      sleep 3
    done
    info "$node no longer reports a QDevice registration"
  done <<<"$states"
}

# Bring the QDevice to the layout NODE_COUNT online members need: absent for
# an odd count, alive and voting for an even one.
qd_reconcile() {
  local node_count="$1" coordinator status
  coordinator="$(qd_coordinator)"
  if ((node_count % 2 == 1)); then
    qd_remove "The ${node_count}-node cluster has an unnecessary QDevice. Every member is online; removing the external vote restores the odd-member quorum layout."
    qd_clear_stale
    qd_assert_layout "$node_count"
    info "The ${node_count}-node cluster has an odd vote count; the QDevice is correctly absent"
    return 0
  fi
  status="$(qd_exec "$coordinator" pvecm status)" ||
    qd_fail "Could not read cluster status from $coordinator"
  if pvecm_status_has_qdevice "$status" && qd_is_registered; then
    qd_assert_layout "$node_count"
    qd_setup_key remove ||
      qd_warn "Could not remove $coordinator's setup key from ${PROXMOX_QDEVICE_HOST}; remove it manually"
    info "The QDevice is configured, alive, and voting"
    return 0
  fi
  qd_clear_stale
  qd_add "$node_count"
}
