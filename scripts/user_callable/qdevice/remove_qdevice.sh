#!/usr/bin/env bash

# Copyright (c) 2026 BEENTHERE VENTURES, INC.
# SPDX-License-Identifier: GPL-3.0-only

# remove_qdevice.sh
#
# Removes the external Proxmox QDevice from the cluster, gracefully when the
# QDevice is accessible and forcefully when it is not.
#
# Graceful removal detaches the QDevice from the cluster, then removes
# QDevice/Corosync software and persistent state from the QDevice host. Its
# Tailscale enrollment is left as is, so scripts/user_callable/qdevice/add_qdevice.sh can add the
# same machine again.
#
# Forced removal is for a QDevice that this workstation cannot reach as root,
# or a reachable machine that is not the one the cluster registered. The
# machine is never contacted. The operator must agree, remove the old machine
# from Tailscale, and confirm that before the QDevice is removed from the
# cluster and every Proxmox host forgets it.
#
# The cluster side uses scripts/lib/qdevice.sh, the same code that host setup, host
# removal, and scripts/user_callable/qdevice/add_qdevice.sh use, through the cluster control node
# and under the cluster control-plane lock.
#
# Usage:
#   ./remove_qdevice.sh [qdevice-host]
#
# The QDevice host defaults, when prompted, to PROXMOX_QDEVICE_HOST in
# config/cluster.conf.

set -Eeuo pipefail
set +x
umask 077

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
REPO_ROOT="$(cd -- "${SCRIPT_DIR}/../../.." && pwd -P)"
CONFIG_LIB="${REPO_ROOT}/scripts/lib/config.sh"
CONTROL_LIB="${REPO_ROOT}/scripts/lib/cluster_control.sh"
MEMBERSHIP_LIB="${REPO_ROOT}/scripts/lib/host_membership.sh"

for library in "$CONFIG_LIB" "$CONTROL_LIB" "$MEMBERSHIP_LIB"; do
  [[ -f "$library" && ! -L "$library" ]] || {
    printf 'ERROR: Missing library: %s\n' "$library" >&2
    exit 1
  }
done
# shellcheck source=../../lib/config.sh
source "$CONFIG_LIB"
# shellcheck source=../../lib/cluster_control.sh
source "$CONTROL_LIB"
# shellcheck source=../../lib/host_membership.sh
source "$MEMBERSHIP_LIB"
bmac_ui_bootstrap "$@"

load_proxmox_config --no-secrets || {
  printf 'ERROR: Could not load cluster configuration from %s\n' "$PROXMOX_CLUSTER_CONFIG" >&2
  exit 1
}

DEFAULT_QDEVICE_HOST="${PROXMOX_QDEVICE_HOST:-qdevice}"
NODE_COUNT=0

usage() {
  cat <<EOF
Usage: $(basename "$0") [qdevice-host]

Examples:
  $(basename "$0")
  $(basename "$0") qdevice

If omitted, qdevice-host defaults to '${DEFAULT_QDEVICE_HOST}' (PROXMOX_QDEVICE_HOST
in ${PROXMOX_CLUSTER_CONFIG}) after an interactive prompt. The cluster is
reached through its control node.
EOF
}

case "${1:-}" in
  -h | --help)
    usage
    exit 0
    ;;
esac
if (($# > 1)); then
  usage >&2
  exit 2
fi

cleanup() {
  local code=$?
  trap - EXIT
  set +e
  hm_release_control_plane_lock
  exit "$code"
}
trap cleanup EXIT

cat <<'EOF'
==============================================================================
WARNING: DESTRUCTIVE QDEVICE REMOVAL
==============================================================================

This script removes the external Proxmox QDevice from the cluster. Every
cluster member must be online and the cluster must be quorate.

GRACEFUL REMOVAL, when this workstation can SSH as root to the QDevice host
and it is the machine the cluster registered:

  * Run the supported Proxmox command, through the cluster control node:

        pvecm qdevice remove

    This removes the QDevice from the cluster configuration, removes QDevice
    client certificate state from the Proxmox nodes, stops/disables the
    corosync-qdevice clients, and reloads Corosync. Members that still report
    the removed QDevice restart corosync one at a time to clear it.

  * Abort BEFORE touching the QDevice host if the QDevice cannot be cleanly
    removed from the cluster.

  * On every Proxmox node, remove the QDevice's pinned SSH host key and any
    known_hosts entries for it, and remove the control node's setup SSH key
    from the QDevice host.

  * On the QDevice host, stop and disable QDevice/Corosync services, destroy
    the QNetd NSS/TLS database and all old cluster certificates, purge the
    corosync-qnetd, corosync-qdevice, and corosync packages if present, and
    remove remaining Corosync/QDevice configuration, state, and the coroqnetd
    user and group. APT can also remove another installed package that has a
    hard dependency on these; on a dedicated QDevice server that normally
    does not apply.

  * Leave the QDevice host's Tailscale enrollment as is. scripts/user_callable/qdevice/add_qdevice.sh
    can add the same machine again; it reinstalls corosync-qnetd.

"apt autoremove" is NOT run, and the systemd journal and other general logs
are NOT erased, so they may still mention the old QDevice or cluster.

FORCED REMOVAL, when the QDevice host cannot be reached as root or is not
the machine the cluster registered (for example a replacement prepared under
the same name):

  * Nothing is done to the QDevice machine. The script explains why and asks
    whether to remove the QDevice from the cluster forcefully.
  * You must then remove the old QDevice machine from Tailscale and confirm
    that, because it keeps this cluster's QDevice certificates and must never
    communicate with the cluster again.
  * It then runs 'pvecm qdevice remove' and removes every association with the
    QDevice from every Proxmox node.

If the cluster has an even number of members, it has no tie-breaking vote
afterward: losing any one member loses quorum until a QDevice is added again
with scripts/user_callable/qdevice/add_qdevice.sh.

To continue, type exactly, in all-caps: GO
Anything else aborts without making changes.

==============================================================================
WARNING: DESTRUCTIVE QDEVICE REMOVAL
==============================================================================
EOF

if bmac_ui_is_json; then
  bmac_ui_confirm --id remove_qdevice --severity destructive \
    --title "Remove the QDevice from the cluster?" \
    --message "Graceful removal runs 'pvecm qdevice remove' through the control node and then purges QNetd/Corosync software and state from the QDevice host. Forced removal, when the QDevice cannot be reached, never contacts it and requires you to remove it from Tailscale." \
    --detail "Every cluster member must be online and the cluster must be quorate." \
    --detail "With an even number of members the cluster has no tie-breaking vote afterward until a QDevice is added again." \
    --detail "apt autoremove is not run and system logs are not erased." \
    --confirm-label "Remove the QDevice" --text GO && CONFIRMATION=GO || CONFIRMATION=""
else
  printf '\nConfirmation: '
  read -r CONFIRMATION || CONFIRMATION=""
fi
if [[ "$CONFIRMATION" != "GO" ]]; then
  echo "Aborted. No changes were made."
  ! bmac_ui_is_json || exit "$BMAC_UI_EXIT_CANCELLED"
  exit 0
fi
echo
echo "Confirmed."
echo

QDEVICE_HOST="${1:-}"
if [[ -z "$QDEVICE_HOST" ]] && bmac_ui_is_json; then
  bmac_ui_input QDEVICE_HOST --id qdevice_host --label "QDevice hostname" \
    --default "$DEFAULT_QDEVICE_HOST" --required --pattern '^[A-Za-z0-9][A-Za-z0-9._-]*$'
  QDEVICE_HOST="${QDEVICE_HOST:-$DEFAULT_QDEVICE_HOST}"
elif [[ -z "$QDEVICE_HOST" ]]; then
  read -r -p "QDevice hostname [${DEFAULT_QDEVICE_HOST}]: " QDEVICE_HOST || QDEVICE_HOST=""
  QDEVICE_HOST="${QDEVICE_HOST:-$DEFAULT_QDEVICE_HOST}"
fi
[[ "$QDEVICE_HOST" =~ ^[A-Za-z0-9][A-Za-z0-9._-]*$ ]] ||
  die "Enter a plain QDevice hostname such as qdevice, not '$QDEVICE_HOST'"
[[ ! "$QDEVICE_HOST" =~ ^mox[0-9]+$ ]] ||
  die "$QDEVICE_HOST is a Proxmox host name; refusing to remove Corosync from a Proxmox VE node"
PROXMOX_QDEVICE_HOST="$QDEVICE_HOST"

echo
echo "QDevice host : $QDEVICE_HOST"
echo

# Every member online and the cluster quorate; sets NODE_COUNT.
load_cluster_shape() {
  local states node state count=0 status
  states="$(hm_member_states)" || die "Could not list cluster members through $HM_COORDINATOR"
  while read -r node state; do
    [[ -n "$node" ]] || continue
    count=$((count + 1))
    [[ "$state" == online ]] ||
      die "$node is offline. Every member must be online to change the QDevice; no changes were made."
  done <<<"$states"
  status="$(hm_cluster_status)" || die "Could not read cluster status through $HM_COORDINATOR"
  pvecm_status_is_quorate "$status" || die "The cluster is not quorate; no changes were made"
  NODE_COUNT="$count"
}

print_even_count_warning() {
  ((NODE_COUNT % 2 == 0)) || return 0
  echo "- The ${NODE_COUNT}-member cluster now has no tie-breaking vote: losing any one"
  echo "  member loses quorum. Add a QDevice promptly with scripts/user_callable/qdevice/add_qdevice.sh."
  bmac_ui_warning "The ${NODE_COUNT}-member cluster has no tie-breaking vote: losing any one member loses quorum."
  bmac_ui_next_step "Add a QDevice promptly; the cluster has no tie-breaking vote." \
    --command "scripts/user_callable/qdevice/add_qdevice.sh" --workflow add_qdevice
}

echo "Resolving the cluster control node..."
resolve_control_node || die "Could not resolve the cluster control node"
[[ -n "$CONTROL_PROBE_NODE" ]] ||
  die "No reachable cluster member was found; no changes were made"
HM_COORDINATOR="$CONTROL_NODE"
load_cluster_shape
echo "Cluster: ${PROXMOX_CLUSTER_NAME}, ${NODE_COUNT} members, control node $HM_COORDINATOR"
REGISTERED_ADDRESS=""
if qd_is_registered; then
  REGISTERED_ADDRESS="$(qd_registered_address)" ||
    die "Could not read the registered QDevice address through $HM_COORDINATOR"
  echo "QDevice address registered in the cluster: ${REGISTERED_ADDRESS:-unknown}"
else
  echo "No QDevice is registered in the cluster."
fi

QDEVICE_ACCESSIBLE=0
if hm_verify_qdevice_access; then
  QDEVICE_ACCESSIBLE=1
  # Guard against accidentally pointing the destructive target at a Proxmox VE
  # host. An external qnetd server should not contain Proxmox VE tooling.
  echo "Verifying that $QDEVICE_HOST does not appear to be a Proxmox VE host..."
  qd_ssh bash -s <<'REMOTE'
set -euo pipefail
if command -v pveversion >/dev/null 2>&1; then
  echo "ERROR: This host has the pveversion command and appears to be Proxmox VE." >&2
  echo "Refusing to purge Corosync from it." >&2
  exit 78
fi
if command -v dpkg-query >/dev/null 2>&1 &&
   dpkg-query -W -f='${db:Status-Abbrev}' proxmox-ve 2>/dev/null | grep -q '^ii'; then
  echo "ERROR: The proxmox-ve package is installed on this host." >&2
  echo "Refusing to purge Corosync from it." >&2
  exit 78
fi
REMOTE
  echo "External-host safety check passed."
fi
echo

hm_acquire_control_plane_lock "$HM_COORDINATOR"
load_cluster_shape
qd_remove "This removes the QDevice from the ${NODE_COUNT}-member cluster ${PROXMOX_CLUSTER_NAME}."
qd_clear_stale

if [[ "$QD_REMOVAL" == forced ]]; then
  hm_release_control_plane_lock
  echo
  echo "Done."
  echo "- The QDevice was removed forcefully; the cluster no longer has it registered."
  echo "- No Proxmox node runs a QDevice client or trusts the old QDevice's SSH host key."
  echo "- The old QDevice machine was NOT contacted. It must stay removed from Tailscale"
  echo "  and must be wiped before it is ever reconnected to any network."
  echo "- On this workstation, remove its old host key before preparing a replacement:"
  echo "      ssh-keygen -R $QDEVICE_HOST"
  print_even_count_warning
  echo "- Check the cluster with scripts/user_callable/diagnostics/show_qdevice_state.sh. To add a"
  echo "  replacement, follow docs/QDEVICE_MANUAL_SETUP.md, then run scripts/user_callable/qdevice/add_qdevice.sh."
  bmac_ui_result removal forced qdevice_host "$QDEVICE_HOST"
  bmac_ui_next_step "Keep the old QDevice machine removed from Tailscale, and wipe it before it is ever reconnected to any network."
  bmac_ui_next_step "On this workstation, remove the old QDevice's host key before preparing a replacement." \
    --command "ssh-keygen -R $QDEVICE_HOST"
  bmac_ui_next_step "Check the cluster's QDevice state." --command "scripts/user_callable/diagnostics/show_qdevice_state.sh" \
    --workflow show_qdevice_state
  exit 0
fi

if ((QDEVICE_ACCESSIBLE == 0)); then
  hm_release_control_plane_lock
  echo
  echo "No QDevice is registered in the cluster, and $QDEVICE_HOST is not accessible,"
  echo "so there is nothing to remove. Fix 'ssh root@$QDEVICE_HOST' and rerun to remove"
  echo "QDevice software from it."
  bmac_ui_next_step "Nothing was removed. Fix 'ssh root@$QDEVICE_HOST' and run this workflow again to remove QDevice software from it." \
    --command "ssh root@$QDEVICE_HOST" --workflow remove_qdevice --arg "qdevice_host=$QDEVICE_HOST"
  exit 0
fi

[[ "$QD_REMOVAL" == graceful ]] ||
  info "The cluster configuration shows that the QDevice is already detached."
qd_forget "$QDEVICE_HOST" "$QD_IPV4"
hm_release_control_plane_lock
if [[ "$QD_REMOVAL" != graceful ]]; then
  qd_setup_key remove ||
    qd_warn "Could not remove $HM_COORDINATOR's setup key from $QDEVICE_HOST; the removal continues"
fi
echo

echo "Removing QDevice software and state from $QDEVICE_HOST..."
qd_ssh bash -s < <(
  printf '%s\n' "$APT_LOCK_WAIT_FUNCTIONS"
  cat <<'REMOTE'
set -Eeuo pipefail

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

if dpkg-query -W -f='${db:Status-Abbrev}' proxmox-ve 2>/dev/null | grep -q '^ii'; then
  echo "ERROR: proxmox-ve is installed on the target. Refusing to purge Corosync." >&2
  exit 31
fi

# qnetd can serve multiple clusters. A clean pvecm removal should make the old
# cluster disappear from qnetd. Refuse to destroy qnetd while any cluster still
# appears connected, which protects against accidentally purging a shared qnetd.
if systemctl is-active --quiet corosync-qnetd.service 2>/dev/null; then
  if command -v corosync-qnetd-tool >/dev/null 2>&1; then
    for attempt in 1 2 3 4 5 6 7 8 9 10; do
      CLIENTS="$(corosync-qnetd-tool -l 2>/dev/null || true)"
      if ! grep -q '^Cluster "' <<<"$CLIENTS"; then
        break
      fi

      if [[ "$attempt" == 10 ]]; then
        echo "ERROR: corosync-qnetd still reports one or more connected clusters:" >&2
        printf '%s\n' "$CLIENTS" >&2
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

# Stop/disable relevant services before removing files and packages. Missing
# units are expected on an already-purged host and are ignored.
for unit in corosync-qnetd.service corosync-qdevice.service corosync.service; do
  if systemctl cat "$unit" >/dev/null 2>&1; then
    echo "Stopping/disabling $unit..."
    systemctl stop "$unit" 2>/dev/null || true
    systemctl disable "$unit" 2>/dev/null || true
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
  STATUS="$(dpkg-query -W -f='${db:Status-Abbrev}' "$pkg" 2>/dev/null || true)"
  case "$STATUS" in
    ii*|rc*) PURGE_PKGS+=("$pkg") ;;
  esac
done

if (( ${#PURGE_PKGS[@]} > 0 )); then
  apt_wait_for_locks || exit 35
  echo "Purging packages: ${PURGE_PKGS[*]}"
  export DEBIAN_FRONTEND=noninteractive
  # The timeout covers another process taking the lock just after the wait above.
  apt-get -o DPkg::Lock::Timeout=60 purge -y "${PURGE_PKGS[@]}"
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
  STATUS="$(dpkg-query -W -f='${db:Status-Abbrev}' "$pkg" 2>/dev/null || true)"
  if [[ "$STATUS" == ii* || "$STATUS" == rc* ]]; then
    echo "ERROR: Package $pkg still has installed/residual state: $STATUS" >&2
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
  if CMD_PATH="$(command -v "$cmd" 2>/dev/null)"; then
    echo "ERROR: Corosync/QDevice executable still exists: $CMD_PATH" >&2
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

echo
echo "Done."
echo "- The QDevice was removed gracefully, or the cluster was already detached from it."
echo "- No Proxmox node runs a QDevice client or trusts the QDevice's SSH host key."
echo "- The QNetd TLS/NSS identity and old cluster certificate state on $QDEVICE_HOST are gone."
echo "- corosync-qnetd, corosync-qdevice, and corosync package state is purged if it existed."
echo "- Remaining /etc/corosync and known QDevice/Corosync runtime/state directories are gone."
echo "- The coroqnetd system account/group is gone."
echo "- apt autoremove was NOT run; generic/shared dependency packages were intentionally retained."
echo "- General system journal/history was intentionally retained."
echo "- $QDEVICE_HOST is still enrolled in Tailscale. scripts/user_callable/qdevice/add_qdevice.sh can add it again."
bmac_ui_result removal "${QD_REMOVAL:-detached}" qdevice_host "$QDEVICE_HOST"
print_even_count_warning
bmac_ui_next_step "$QDEVICE_HOST is still enrolled in Tailscale; the add QDevice workflow can add it again." \
  --workflow add_qdevice
