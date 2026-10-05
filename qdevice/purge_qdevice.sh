#!/usr/bin/env bash

# Copyright (c) 2026 BEENTHERE VENTURES, INC.
# SPDX-License-Identifier: GPL-3.0-only

# purge_qdevice.sh
#
# Gracefully detaches an external Proxmox QDevice from a Proxmox VE cluster,
# then removes QDevice/Corosync software and persistent state from the external
# QDevice host.
#
# When the QDevice host cannot be reached (or is not the machine the cluster
# has registered), it offers to unregister the QDevice from the cluster and
# remove every association with it from the Proxmox hosts without contacting
# it. The operator must first remove that machine from Tailscale.
#
# Usage:
#   ./purge_qdevice.sh [qdevice-host] [proxmox-host]
#
# Examples:
#   ./purge_qdevice.sh
#   ./purge_qdevice.sh qdevice
#   ./purge_qdevice.sh qdevice mox2
#
# Defaults when prompted, from env/cluster.conf:
#   QDevice host:  PROXMOX_QDEVICE_HOST (qdevice when unset)
#   Proxmox host:  PROXMOX_CONTROL_NODE (mox1 when unset)
#
# Remote privilege requirement:
#   Each SSH target must either log in as root or allow passwordless sudo.
#
# This script is intentionally destructive. Read the interactive warning before
# confirming execution.

set -Eeuo pipefail

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
CONFIG_LIB="${SCRIPT_DIR}/../lib/config.sh"
[[ -f "$CONFIG_LIB" && ! -L "$CONFIG_LIB" ]] || {
  printf 'ERROR: Missing library: %s\n' "$CONFIG_LIB" >&2
  exit 1
}
# shellcheck source=../lib/config.sh
source "$CONFIG_LIB"
load_proxmox_config --no-secrets || {
  printf 'ERROR: Could not load cluster configuration from %s\n' "$PROXMOX_CLUSTER_CONFIG" >&2
  exit 1
}

IFS=$'\n\t'

DEFAULT_QDEVICE_HOST="${PROXMOX_QDEVICE_HOST:-qdevice}"
DEFAULT_PVE_HOST="${PROXMOX_CONTROL_NODE:-mox1}"
SSH_OPTS=(
  -o ConnectTimeout=10
  -o ServerAliveInterval=5
  -o ServerAliveCountMax=2
)

usage() {
  cat <<EOF
Usage: $(basename "$0") [qdevice-host] [proxmox-host]

Examples:
  $(basename "$0")
  $(basename "$0") qdevice
  $(basename "$0") qdevice mox2

If omitted, qdevice-host defaults to '${DEFAULT_QDEVICE_HOST}' (PROXMOX_QDEVICE_HOST)
and proxmox-host defaults to '${DEFAULT_PVE_HOST}' (PROXMOX_CONTROL_NODE) from
${PROXMOX_CLUSTER_CONFIG} after an interactive prompt.
EOF
}

if (( $# > 2 )); then
  usage >&2
  exit 2
fi

cat <<'EOF'
==============================================================================
WARNING: DESTRUCTIVE QDEVICE PURGE
==============================================================================

This script permanently removes the external Proxmox QDevice setup.

It assumes that you have SSH access to:
  1. The external QDevice host.
  2. One usable Proxmox VE node in the cluster.

Each SSH login must either be root or have passwordless sudo access.

If you continue, this script will:

  * Connect to the Proxmox VE node and determine whether a QDevice is still
    configured in the cluster.

  * If a QDevice is configured, run the supported Proxmox command:

        pvecm qdevice remove

    This removes the QDevice from the cluster configuration, removes QDevice
    client certificate state from the Proxmox nodes, stops/disables the
    corosync-qdevice clients, and reloads Corosync.

  * If the Proxmox cluster is already detached from a QDevice, report that
    condition and continue.

  * Abort BEFORE purging the external server if Proxmox reports that a
    configured QDevice cannot be cleanly removed. This avoids destroying a
    QDevice which the cluster still expects to exist.

  * Remove from the QDevice host the exact Proxmox root SSH public key used by
    pvecm qdevice setup, if that key can be identified from the selected
    Proxmox node.

  * Stop and disable QDevice/Corosync services on the external host.

  * Destroy the external QNetd NSS/TLS database and all old cluster
    certificates and QNetd identity material.

  * Purge these Debian/Ubuntu packages if present, including residual package
    configuration:

        corosync-qnetd
        corosync-qdevice
        corosync

    APT can also remove another installed package if that package has a hard
    dependency on one of the packages being purged. On a dedicated QDevice
    server this normally does not apply.

  * Remove remaining Corosync/QDevice-specific configuration and runtime state,
    including /etc/corosync and known Corosync/QDevice state directories.

  * Remove the dedicated coroqnetd system user and group if they still exist
    after package removal.

The script deliberately DOES NOT run "apt autoremove". Generic libraries or
other packages which may have been installed as dependencies are left alone so
that unrelated automatically-installed software is not removed accidentally.

The script also does NOT erase the systemd journal or other general system
logs. Historical log entries may therefore still mention the old QDevice or
cluster even though the QDevice software and active/persistent state are gone.

If the QDevice host CANNOT be reached over SSH, or it is not the machine whose
address the cluster has registered, nothing is done to it. The script instead
explains the situation and asks whether to unregister the QDevice from the
cluster anyway. That option requires you to remove the old QDevice machine
from Tailscale first, then removes the QDevice from the cluster configuration
and every association with it from the Proxmox hosts. Every cluster member
must be online.

Unlike the non-destructive reset script, this script does NOT create a fresh
QNetd certificate identity after deleting the old one, because QNetd itself is
being uninstalled.

To continue, type exactly, in all-caps: GO
Anything else aborts without making changes.

==============================================================================
WARNING: DESTRUCTIVE QDEVICE PURGE
==============================================================================
EOF

printf '\nConfirmation: '
read -r CONFIRMATION
if [[ "$CONFIRMATION" != "GO" ]]; then
  echo "Aborted. No changes were made."
  exit 0
fi

echo

echo "Confirmed."
echo

QDEVICE_HOST="${1:-}"
PVE_HOST="${2:-}"

if [[ -z "$QDEVICE_HOST" ]]; then
  read -r -p "QDevice hostname [${DEFAULT_QDEVICE_HOST}]: " QDEVICE_HOST
  QDEVICE_HOST="${QDEVICE_HOST:-$DEFAULT_QDEVICE_HOST}"
fi

if [[ -z "$PVE_HOST" ]]; then
  read -r -p "Proxmox cluster hostname [${DEFAULT_PVE_HOST}]: " PVE_HOST
  PVE_HOST="${PVE_HOST:-$DEFAULT_PVE_HOST}"
fi

if [[ "$QDEVICE_HOST" == -* || "$PVE_HOST" == -* ]]; then
  echo "ERROR: Hostnames beginning with '-' are not accepted." >&2
  exit 2
fi

if [[ "$QDEVICE_HOST" == "$PVE_HOST" ]]; then
  echo "ERROR: QDevice host and Proxmox host are the same string ('$QDEVICE_HOST')." >&2
  echo "Refusing to risk purging Corosync from a Proxmox VE node." >&2
  exit 2
fi

echo
echo "QDevice host : $QDEVICE_HOST"
echo "Proxmox host : $PVE_HOST"
echo

# Run a supplied Bash program as root on a remote host. If SSH did not log in
# as root, passwordless sudo is accepted. Interactive sudo is intentionally not
# used because the payload itself is supplied on stdin.
run_remote_root() {
  local host="$1"
  local payload="$2"

  printf '%s\n' "$payload" | ssh "${SSH_OPTS[@]}" "$host" '
    if [ "$(id -u)" -eq 0 ]; then
      exec bash -s
    fi

    if command -v sudo >/dev/null 2>&1 && sudo -n true >/dev/null 2>&1; then
      exec sudo -n bash -s
    fi

    echo "ERROR: Remote session is not root and passwordless sudo is unavailable." >&2
    exit 77
  '
}

# Fail before changing anything if the Proxmox host is unavailable. An
# unreachable QDevice host leads to the unregister-only path below.
echo "Checking SSH access..."
ssh "${SSH_OPTS[@]}" "$PVE_HOST" 'true'
echo "SSH access to $PVE_HOST OK."
QDEVICE_REACHABLE=1
if ! QDEVICE_SSH_ERROR="$(ssh "${SSH_OPTS[@]}" "$QDEVICE_HOST" 'true' 2>&1 </dev/null)"; then
  QDEVICE_REACHABLE=0
  echo "SSH access to $QDEVICE_HOST FAILED:"
  printf '%s\n' "$QDEVICE_SSH_ERROR" | sed 's/^/  /'
else
  echo "SSH access to $QDEVICE_HOST OK."
fi
echo

# The QDevice address the cluster has registered (corosync.conf quorum.device
# net host), or empty when none is registered.
# shellcheck disable=SC2016 # Evaluated on the Proxmox host.
REGISTERED_QDEVICE_ADDRESS="$(run_remote_root "$PVE_HOST" '
set -euo pipefail
if [[ -s /etc/pve/corosync.conf ]]; then
  awk "/^[[:space:]]*device[[:space:]]*[{]/ { device=1 } device && /^[[:space:]]*host:/ { print \$2; exit }" \
    /etc/pve/corosync.conf
fi
')"
if [[ -n "$REGISTERED_QDEVICE_ADDRESS" && ! "$REGISTERED_QDEVICE_ADDRESS" =~ ^[0-9A-Fa-f.:]+$ ]]; then
  echo "ERROR: corosync.conf registers an unexpected QDevice address: $REGISTERED_QDEVICE_ADDRESS" >&2
  exit 2
fi
echo "QDevice address registered in the cluster: ${REGISTERED_QDEVICE_ADDRESS:-none}"
echo

# Guard against accidentally pointing the destructive target at a Proxmox VE
# host. An external qnetd server should not contain Proxmox VE tooling/packages.
QDEVICE_MISMATCH=0
if (( QDEVICE_REACHABLE )); then
echo "Verifying that $QDEVICE_HOST does not appear to be a Proxmox VE host..."
run_remote_root "$QDEVICE_HOST" '
set -euo pipefail

if command -v pveversion >/dev/null 2>&1; then
  echo "ERROR: This host has the pveversion command and appears to be Proxmox VE." >&2
  echo "Refusing to purge Corosync from it." >&2
  exit 78
fi

if command -v dpkg-query >/dev/null 2>&1 && \
   dpkg-query -W -f="\${db:Status-Abbrev}" proxmox-ve 2>/dev/null | grep -q "^ii"; then
  echo "ERROR: The proxmox-ve package is installed on this host." >&2
  echo "Refusing to purge Corosync from it." >&2
  exit 78
fi
'
echo "External-host safety check passed."
echo

# A reachable host whose Tailscale address is not the registered one is a
# different machine, such as a replacement prepared under the same name.
QDEVICE_TAILSCALE_IP="$(run_remote_root "$QDEVICE_HOST" '
if command -v tailscale >/dev/null 2>&1; then
  tailscale ip -4 2>/dev/null | head -n 1
fi
' || true)"
if [[ -n "$REGISTERED_QDEVICE_ADDRESS" && -n "$QDEVICE_TAILSCALE_IP" &&
      "$REGISTERED_QDEVICE_ADDRESS" != "$QDEVICE_TAILSCALE_IP" ]]; then
  QDEVICE_MISMATCH=1
fi
fi

# Capture the exact Proxmox root RSA key that current pvecm qdevice setup uses
# with ssh-copy-id. It can then be removed from the QDevice authorized_keys.
PVE_ROOT_PUBKEY="$(run_remote_root "$PVE_HOST" '
set -euo pipefail
if [[ -r /root/.ssh/id_rsa.pub ]]; then
  cat /root/.ssh/id_rsa.pub
fi
')"

if [[ -n "$PVE_ROOT_PUBKEY" ]]; then
  echo "Found the Proxmox root RSA public key used by pvecm qdevice setup."
else
  echo "NOTE: /root/.ssh/id_rsa.pub was not found on $PVE_HOST."
  echo "      The old pvecm-added SSH key cannot be identified automatically."
fi

echo

# Inspect the cluster config before invoking pvecm qdevice remove. This matters
# because current pvecm checks that all cluster members are online before it
# gets as far as reporting "No QDevice configured". If there is no device
# stanza at all, we can correctly report that the QDevice is already detached
# even if another cluster member happens to be offline.
PVE_REMOVE_PAYLOAD=$(cat <<'REMOTE'
set -euo pipefail

if ! command -v pvecm >/dev/null 2>&1; then
  echo "ERROR: pvecm is not installed on this host." >&2
  exit 20
fi

if [[ ! -s /etc/pve/corosync.conf ]]; then
  echo "__QDEVICE_RESULT__:NO_CLUSTER_CONFIG"
  exit 0
fi

if ! grep -Eq '^[[:space:]]*device[[:space:]]*\{' /etc/pve/corosync.conf; then
  echo "__QDEVICE_RESULT__:ALREADY_DETACHED"
  exit 0
fi

set +e
REMOVE_OUTPUT="$(pvecm qdevice remove 2>&1)"
REMOVE_RC=$?
set -e

if [[ -n "$REMOVE_OUTPUT" ]]; then
  printf '%s\n' "$REMOVE_OUTPUT"
fi

if (( REMOVE_RC == 0 )); then
  echo "__QDEVICE_RESULT__:REMOVED"
elif grep -qi 'No QDevice configured' <<<"$REMOVE_OUTPUT"; then
  echo "__QDEVICE_RESULT__:ALREADY_DETACHED"
else
  echo "__QDEVICE_RESULT__:FAILED"
  exit "$REMOVE_RC"
fi

# Verify that the persistent configuration no longer contains a QDevice stanza.
if grep -Eq '^[[:space:]]*device[[:space:]]*\{' /etc/pve/corosync.conf; then
  echo "ERROR: /etc/pve/corosync.conf still contains a quorum device stanza." >&2
  exit 21
fi

# Verify the selected node's QDevice client is no longer running. The persistent
# corosync.conf check above is authoritative for whether the cluster remains
# configured to use a QDevice. A live votequorum registration can linger after
# removal, so the mere presence of "Qdevice" in `pvecm status` is NOT a failure.
if systemctl is-active --quiet corosync-qdevice.service 2>/dev/null; then
  echo "ERROR: corosync-qdevice.service is still active after pvecm qdevice remove." >&2
  exit 22
fi

# Runtime status is a supplementary safety check. If a stale QDevice entry is
# still displayed, tolerate it only when it contributes zero votes.
if STATUS_OUTPUT="$(pvecm status 2>&1)"; then
  if grep -Eiq '^[[:space:]]*Flags:[[:space:]].*Qdevice([[:space:]]|$)' <<<"$STATUS_OUTPUT"; then
    QDEVICE_LINE="$(grep -Ei '^[[:space:]]*0x0+[[:space:]]+[0-9]+.*Qdevice' <<<"$STATUS_OUTPUT" | tail -n 1 || true)"

    if [[ -n "$QDEVICE_LINE" ]]; then
      QDEVICE_VOTES="$(awk '{print $2}' <<<"$QDEVICE_LINE")"
      if [[ "$QDEVICE_VOTES" =~ ^[0-9]+$ ]] && (( QDEVICE_VOTES > 0 )); then
        echo "ERROR: pvecm still reports a QDevice contributing $QDEVICE_VOTES vote(s)." >&2
        printf '%s\n' "$STATUS_OUTPUT" >&2
        exit 23
      fi

      echo "NOTE: pvecm status still shows a live/stale QDevice registration," >&2
      echo "      but the persistent QDevice configuration is gone, the local" >&2
      echo "      corosync-qdevice service is inactive, and the displayed QDevice" >&2
      echo "      contributes zero votes. Continuing." >&2
    else
      echo "NOTE: pvecm status still contains the QDevice flag, but no QDevice" >&2
      echo "      membership row could be parsed. Persistent configuration is gone" >&2
      echo "      and corosync-qdevice.service is inactive, so continuing." >&2
    fi
  fi
fi
REMOTE
)

# Remove every association with the QDevice from one Proxmox node: its QDevice
# client service and certificates, the managed QDevice host-key block that
# host setup writes, and known_hosts entries for the QDevice name or address
# (including the cluster-wide /etc/pve/priv/known_hosts).
NODE_CLEANUP_BODY=$(cat <<'REMOTE'
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
echo "__QDEVICE_NODE_CLEANUP__:OK"
REMOTE
)

# The QDevice host cannot be purged: unregister the QDevice from the cluster
# and the Proxmox hosts without contacting it, after the operator has removed
# the machine from Tailscale. Never returns.
unregister_without_qdevice_host() {
  local reason="$1" answer confirmation cluster_output node state header
  local -a nodes=()

  cat <<EOF
==============================================================================
QDEVICE HOST CANNOT BE PURGED
==============================================================================

$reason

QDevice address registered in the cluster: ${REGISTERED_QDEVICE_ADDRESS:-none}

Nothing will be done to the QDevice machine. This script can instead
unregister the QDevice from the cluster without contacting it:

  * run 'pvecm qdevice remove' on $PVE_HOST, which removes the QDevice from
    the cluster configuration and stops and removes the QDevice client
    (service and certificates) on every Proxmox node;
  * on every Proxmox node, remove the managed QDevice host-key block and any
    known_hosts entry for '$QDEVICE_HOST' or ${REGISTERED_QDEVICE_ADDRESS:-its old address}, including the
    cluster-wide /etc/pve/priv/known_hosts.

Every cluster member must be online and the cluster must be quorate.

If the cluster has an even number of members, it then has no tie-breaking
vote: losing any one member loses quorum until a new QDevice is added with
qdevice/add_qdevice.sh. Add the replacement promptly.

EOF
  read -r -p "Unregister the QDevice from the cluster without contacting it? [y/N] " answer || answer=""
  if [[ "${answer,,}" != y && "${answer,,}" != yes ]]; then
    echo "Aborted. No changes were made."
    exit 0
  fi

  if [[ ! "$QDEVICE_HOST" =~ ^[A-Za-z0-9][A-Za-z0-9._-]*$ ]]; then
    echo "ERROR: Unregistering requires a plain QDevice hostname, not '$QDEVICE_HOST'." >&2
    exit 2
  fi

  cat <<EOF

==============================================================================
REQUIRED: REMOVE THE OLD QDEVICE FROM TAILSCALE NOW
==============================================================================

The old QDevice machine still holds this cluster's QDevice certificates. It
must never be able to communicate with the cluster again. Before continuing:

  1. In the Tailscale admin console, open Machines and find the old QDevice
     by its address${REGISTERED_QDEVICE_ADDRESS:+ $REGISTERED_QDEVICE_ADDRESS}. Its name may be
     '$QDEVICE_HOST' or a variant such as '$QDEVICE_HOST-1'. If a replacement
     already uses the name '$QDEVICE_HOST', do not remove the replacement.
  2. Use its menu to Remove it from the tailnet, and revoke any auth key that
     could re-register it.
  3. If the machine is ever recovered, wipe or reinstall it before it joins
     any network. Never reconnect it as it is.

Type exactly, in all-caps, once it has been removed: REMOVED FROM TAILSCALE
EOF
  printf '> '
  read -r confirmation || confirmation=""
  if [[ "$confirmation" != "REMOVED FROM TAILSCALE" ]]; then
    echo "Aborted. No changes were made."
    exit 0
  fi
  echo

  echo "Checking that every cluster member is online and the cluster is quorate..."
  cluster_output="$(run_remote_root "$PVE_HOST" '
set -euo pipefail
pvecm status | grep -Eq "^Quorate:[[:space:]]+Yes[[:space:]]*$" || {
  echo "ERROR: The cluster is not quorate." >&2
  exit 40
}
pvesh get /nodes --output-format json | python3 -c "
import json, sys
for row in json.load(sys.stdin):
    print(\"__NODE__\", row[\"node\"], row.get(\"status\", \"unknown\"))
"
')" || {
    echo "ERROR: Could not verify the cluster through $PVE_HOST. No changes were made." >&2
    exit 1
  }
  while IFS=' ' read -r _ node state; do
    [[ -n "$node" ]] || continue
    if [[ ! "$node" =~ ^[A-Za-z0-9][A-Za-z0-9._-]*$ ]]; then
      echo "ERROR: Unexpected cluster node name: $node" >&2
      exit 1
    fi
    if [[ "$state" != online ]]; then
      echo "ERROR: $node is $state. Every member must be online; no changes were made." >&2
      exit 1
    fi
    nodes+=("$node")
  done < <(grep '^__NODE__ ' <<<"$cluster_output")
  (( ${#nodes[@]} > 0 )) || {
    echo "ERROR: $PVE_HOST listed no cluster members." >&2
    exit 1
  }
  echo "Members: $(printf '%s ' "${nodes[@]}")"
  for node in "${nodes[@]}"; do
    ssh "${SSH_OPTS[@]}" "$node" true </dev/null || {
      echo "ERROR: Cannot SSH to cluster member '$node' from this workstation. No changes were made." >&2
      exit 1
    }
  done
  echo

  echo "Removing the QDevice from the Proxmox cluster configuration..."
  set +e
  PVE_REMOVE_OUTPUT="$(run_remote_root "$PVE_HOST" "$PVE_REMOVE_PAYLOAD" 2>&1)"
  PVE_REMOVE_RC=$?
  set -e
  printf '%s\n' "$PVE_REMOVE_OUTPUT"
  if (( PVE_REMOVE_RC != 0 )); then
    echo >&2
    echo "ERROR: Proxmox-side QDevice removal failed." >&2
    echo "Fix the cluster-side problem and run this script again." >&2
    exit "$PVE_REMOVE_RC"
  fi
  echo

  printf -v header 'set -euo pipefail\nqdevice_name=%q\nqdevice_addr=%q\n' \
    "$QDEVICE_HOST" "$REGISTERED_QDEVICE_ADDRESS"
  for node in "${nodes[@]}"; do
    echo "Removing QDevice associations from $node..."
    run_remote_root "$node" "${header}${NODE_CLEANUP_BODY}" || {
      echo "ERROR: QDevice cleanup failed on $node. Fix it and run this script again;" >&2
      echo "       every step is safe to repeat." >&2
      exit 1
    }
  done

  echo
  echo "Done."
  echo "- The cluster no longer has a QDevice registered."
  echo "- No Proxmox node runs a QDevice client or trusts the old QDevice's SSH host key."
  echo "- The QDevice machine was NOT contacted. It must stay removed from Tailscale"
  echo "  and must be wiped before it is ever reconnected to any network."
  if (( QDEVICE_REACHABLE == 0 )); then
    echo "- On this workstation, remove its old host key before preparing a replacement:"
    echo "      ssh-keygen -R $QDEVICE_HOST"
  fi
  echo "- Check the cluster with diagnostics/show_qdevice_state.sh. To add a"
  echo "  replacement, follow qdevice/QDEVICE_MANUAL_SETUP.md, then run qdevice/add_qdevice.sh."
  exit 0
}

if (( QDEVICE_REACHABLE == 0 )); then
  unregister_without_qdevice_host \
    "The QDevice host '$QDEVICE_HOST' cannot be reached over SSH, so it cannot be purged."
fi
if (( QDEVICE_MISMATCH )); then
  unregister_without_qdevice_host \
    "'$QDEVICE_HOST' is reachable, but its Tailscale address ($QDEVICE_TAILSCALE_IP) is not the QDevice address the cluster has registered ($REGISTERED_QDEVICE_ADDRESS). It is a different machine (for example a replacement prepared under the same name), so it is left untouched."
fi

echo "Checking/removing the QDevice from the Proxmox cluster..."
set +e
PVE_REMOVE_OUTPUT="$(run_remote_root "$PVE_HOST" "$PVE_REMOVE_PAYLOAD" 2>&1)"
PVE_REMOVE_RC=$?
set -e

printf '%s\n' "$PVE_REMOVE_OUTPUT"

if (( PVE_REMOVE_RC != 0 )); then
  echo >&2
  echo "ERROR: Proxmox-side QDevice removal failed." >&2
  echo "The external QDevice host has NOT been purged." >&2
  echo "Fix the cluster-side problem and run this script again." >&2
  exit "$PVE_REMOVE_RC"
fi

if grep -q '__QDEVICE_RESULT__:REMOVED' <<<"$PVE_REMOVE_OUTPUT"; then
  echo "Proxmox reports that the QDevice was removed successfully."
elif grep -q '__QDEVICE_RESULT__:ALREADY_DETACHED' <<<"$PVE_REMOVE_OUTPUT"; then
  echo "Proxmox configuration shows that the QDevice is already detached."
elif grep -q '__QDEVICE_RESULT__:NO_CLUSTER_CONFIG' <<<"$PVE_REMOVE_OUTPUT"; then
  echo "NOTE: $PVE_HOST has no /etc/pve/corosync.conf."
  echo "      There is no cluster configuration on that host from which to detach a QDevice."
else
  echo "ERROR: Could not determine the Proxmox-side QDevice state." >&2
  exit 23
fi

echo

# Base64 makes the public key safe to embed into the remote purge payload.
PVE_ROOT_PUBKEY_B64=""
if [[ -n "$PVE_ROOT_PUBKEY" ]]; then
  PVE_ROOT_PUBKEY_B64="$(printf '%s' "$PVE_ROOT_PUBKEY" | base64 | tr -d '\n')"
fi

QDEVICE_PURGE_PAYLOAD=$(cat <<REMOTE
set -euo pipefail

OLD_PVE_KEY_B64='$PVE_ROOT_PUBKEY_B64'

if ! command -v apt-get >/dev/null 2>&1 || ! command -v dpkg-query >/dev/null 2>&1; then
  echo "ERROR: This purge script currently supports Debian/Ubuntu package management only." >&2
  echo "apt-get and dpkg-query are required." >&2
  exit 30
fi

# One final server-side guard against accidentally running the purge on PVE.
if command -v pveversion >/dev/null 2>&1; then
  echo "ERROR: pveversion exists on the target. Refusing to purge Corosync." >&2
  exit 31
fi

if dpkg-query -W -f='\${db:Status-Abbrev}' proxmox-ve 2>/dev/null | grep -q '^ii'; then
  echo "ERROR: proxmox-ve is installed on the target. Refusing to purge Corosync." >&2
  exit 31
fi

# qnetd can serve multiple clusters. A clean pvecm removal should make the old
# cluster disappear from qnetd. Refuse to destroy qnetd while any cluster still
# appears connected, which protects against accidentally purging a shared qnetd.
if systemctl is-active --quiet corosync-qnetd.service 2>/dev/null; then
  if command -v corosync-qnetd-tool >/dev/null 2>&1; then
    for attempt in 1 2 3 4 5 6 7 8 9 10; do
      CLIENTS="\$(corosync-qnetd-tool -l 2>/dev/null || true)"
      if ! grep -q '^Cluster "' <<<"\$CLIENTS"; then
        break
      fi

      if [[ "\$attempt" == 10 ]]; then
        echo "ERROR: corosync-qnetd still reports one or more connected clusters:" >&2
        printf '%s\n' "\$CLIENTS" >&2
        echo "Refusing to purge a qnetd server while clients are still connected." >&2
        exit 32
      fi

      sleep 1
    done
  else
    echo "ERROR: corosync-qnetd is active but corosync-qnetd-tool is unavailable." >&2
    echo "Cannot safely verify that no cluster clients remain. Refusing to purge." >&2
    exit 33
  fi
fi

# Remove the exact SSH key copied by pvecm qdevice setup, if it can be
# identified. Compare the key blob, not the comment.
if [[ -n "\$OLD_PVE_KEY_B64" && -f /root/.ssh/authorized_keys ]]; then
  OLD_PVE_KEY="\$(printf '%s' "\$OLD_PVE_KEY_B64" | base64 -d)"
  OLD_PVE_KEY_BLOB="\$(awk '{print \$2}' <<<"\$OLD_PVE_KEY")"

  if [[ -n "\$OLD_PVE_KEY_BLOB" ]] && grep -Fq "\$OLD_PVE_KEY_BLOB" /root/.ssh/authorized_keys; then
    AUTH_TMP="\$(mktemp)"
    awk -v blob="\$OLD_PVE_KEY_BLOB" 'index(\$0, blob) == 0' /root/.ssh/authorized_keys >"\$AUTH_TMP"
    cat "\$AUTH_TMP" > /root/.ssh/authorized_keys
    rm -f "\$AUTH_TMP"
    echo "Removed the old Proxmox root SSH key from /root/.ssh/authorized_keys."
  else
    echo "The identified Proxmox root SSH key was not present in authorized_keys."
  fi
elif [[ -z "\$OLD_PVE_KEY_B64" ]]; then
  echo "No Proxmox root SSH key was available to identify/remove."
fi

# Stop/disable relevant services before removing files and packages. Missing
# units are expected on an already-purged host and are ignored.
for unit in corosync-qnetd.service corosync-qdevice.service corosync.service; do
  if systemctl cat "\$unit" >/dev/null 2>&1; then
    echo "Stopping/disabling \$unit..."
    systemctl stop "\$unit" 2>/dev/null || true
    systemctl disable "\$unit" 2>/dev/null || true
  fi
done

# Remove persistent identity/state before package purge. The package purge also
# removes qnetd's NSS DB, but deleting it explicitly makes the destructive
# intent and ordering unambiguous.
echo "Removing QNetd/Corosync persistent state..."
rm -rf \
  /etc/corosync/qnetd/nssdb \
  /etc/corosync/qdevice
rm -f \
  /tmp/qdevice-net-node.crq \
  /tmp/qdevice-net-node.p12

# Purge only the explicitly QDevice/Corosync packages. Do not run autoremove.
PURGE_PKGS=()
for pkg in corosync-qnetd corosync-qdevice corosync; do
  STATUS="\$(dpkg-query -W -f='\${db:Status-Abbrev}' "\$pkg" 2>/dev/null || true)"
  case "\$STATUS" in
    ii*|rc*) PURGE_PKGS+=("\$pkg") ;;
  esac
done

if (( \${#PURGE_PKGS[@]} > 0 )); then
  echo "Purging packages: \${PURGE_PKGS[*]}"
  export DEBIAN_FRONTEND=noninteractive
  apt-get purge -y "\${PURGE_PKGS[@]}"
else
  echo "The corosync-qnetd, corosync-qdevice, and corosync packages are already absent."
fi

# Remove package-specific configuration/state which may survive because it was
# locally modified or created outside dpkg.
echo "Removing remaining QDevice/Corosync files and runtime state..."
rm -rf \
  /etc/corosync \
  /etc/default/corosync-qnetd \
  /etc/default/corosync-qdevice \
  /etc/default/corosync \
  /etc/systemd/system/corosync-qnetd.service \
  /etc/systemd/system/corosync-qdevice.service \
  /etc/systemd/system/corosync.service \
  /etc/systemd/system/corosync-qnetd.service.d \
  /etc/systemd/system/corosync-qdevice.service.d \
  /etc/systemd/system/corosync.service.d \
  /etc/tmpfiles.d/corosync-qnetd.conf \
  /etc/tmpfiles.d/corosync-qdevice.conf \
  /etc/tmpfiles.d/corosync.conf \
  /var/lib/corosync \
  /var/lib/corosync-qnetd \
  /var/lib/corosync-qdevice \
  /var/log/corosync \
  /var/log/corosync-qnetd \
  /var/log/corosync-qdevice \
  /run/corosync \
  /run/corosync-qnetd \
  /run/corosync-qdevice

systemctl daemon-reload
systemctl reset-failed corosync-qnetd.service corosync-qdevice.service corosync.service 2>/dev/null || true

# Debian's corosync-qnetd package creates this dedicated service account but
# does not necessarily remove it on purge. Remove it explicitly.
if getent passwd coroqnetd >/dev/null 2>&1; then
  echo "Removing dedicated coroqnetd system user..."
  userdel coroqnetd
fi

if getent group coroqnetd >/dev/null 2>&1; then
  echo "Removing dedicated coroqnetd system group..."
  groupdel coroqnetd
fi

# Verify package state. 'rc' would mean residual config still exists and is not
# acceptable after a purge.
VERIFY_FAILED=0
for pkg in corosync-qnetd corosync-qdevice corosync; do
  STATUS="\$(dpkg-query -W -f='\${db:Status-Abbrev}' "\$pkg" 2>/dev/null || true)"
  if [[ "\$STATUS" == ii* || "\$STATUS" == rc* ]]; then
    echo "ERROR: Package \$pkg still has installed/residual state: \$STATUS" >&2
    VERIFY_FAILED=1
  fi
done

if [[ -e /etc/corosync ]]; then
  echo "ERROR: /etc/corosync still exists after purge." >&2
  VERIFY_FAILED=1
fi

if getent passwd coroqnetd >/dev/null 2>&1; then
  echo "ERROR: coroqnetd user still exists after purge." >&2
  VERIFY_FAILED=1
fi

# Detect manually-installed binaries which dpkg could not remove. Do not delete
# arbitrary /usr/local files automatically; report them so the caller knows the
# host is not completely clean.
for cmd in \
  corosync-qnetd \
  corosync-qnetd-tool \
  corosync-qnetd-certutil \
  corosync-qdevice \
  corosync-qdevice-tool \
  corosync-qdevice-net-certutil \
  corosync; do
  if CMD_PATH="\$(command -v "\$cmd" 2>/dev/null)"; then
    echo "ERROR: Corosync/QDevice executable still exists: \$CMD_PATH" >&2
    VERIFY_FAILED=1
  fi
done

if (( VERIFY_FAILED != 0 )); then
  echo "ERROR: QDevice purge completed with leftovers; see messages above." >&2
  exit 34
fi

echo "__QDEVICE_PURGE_RESULT__:OK"
REMOTE
)

echo "Purging QDevice software and state from $QDEVICE_HOST..."
run_remote_root "$QDEVICE_HOST" "$QDEVICE_PURGE_PAYLOAD"

echo
echo "Done."
echo "- The Proxmox cluster no longer uses the QDevice, or it was already detached."
echo "- The external QNetd TLS/NSS identity and old cluster certificate state are gone."
echo "- corosync-qnetd, corosync-qdevice, and corosync package state is purged if it existed."
echo "- Remaining /etc/corosync and known QDevice/Corosync runtime/state directories are gone."
echo "- The coroqnetd system account/group is gone."
if [[ -n "$PVE_ROOT_PUBKEY" ]]; then
  echo "- The selected Proxmox node's root SSH key was removed from the QDevice if present."
else
  echo "- The old Proxmox SSH key could not be identified, so authorized_keys was not changed."
fi
echo "- apt autoremove was NOT run; generic/shared dependency packages were intentionally retained."
echo "- General system journal/history was intentionally retained."
