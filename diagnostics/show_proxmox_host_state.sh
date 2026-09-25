#!/usr/bin/env bash

# Copyright (c) 2026 BEENTHERE VENTURES, INC.
# SPDX-License-Identifier: GPL-3.0-only

# Read-only configuration, guest, and storage report for one Proxmox host.

set -u
set -o pipefail
umask 077

if ((BASH_VERSINFO[0] < 4 || (BASH_VERSINFO[0] == 4 && BASH_VERSINFO[1] < 4))); then
  printf 'ERROR: show_proxmox_host_state.sh requires Bash 4.4 or newer (found %s).\n' \
    "${BASH_VERSION:-unknown}" >&2
  exit 2
fi

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
REPO_ROOT="$(cd -- "${SCRIPT_DIR}/.." && pwd -P)"
CONFIG_LIB="${REPO_ROOT}/lib/config.sh"
STORAGE_LIB="${REPO_ROOT}/lib/host_storage.py"
REMOTE_REGISTRY="/usr/local/lib/app-ha-proxmox/lib/cluster_registry.py"
RESERVATION_DIR="/run/app-ha-production-starting"

usage() {
  cat <<'EOF'
Usage: diagnostics/show_proxmox_host_state.sh moxN

Show one Proxmox host's current configuration, the guests it runs and the
replicas it stores, and its rpool storage layout: every vdev resolved to
physical disk serials, the boot/ESP vdev, device-removal progress, disks
that are not part of rpool, and per-zvol allocation and snapshot usage.

Nothing on the host, its guests, or the registry is changed. Commands create
normal SSH and command-access log entries. When env/moxN.conf is present on
this workstation, its NVMe serials are compared with the live pool.

Exit status: 0 when no attention items were found, 1 when some were, and 2
for usage or configuration errors.
EOF
}

if (($# != 1)) || [[ "$1" == -h || "$1" == --help ]]; then
  usage
  (($# == 1)) && [[ "$1" == -h || "$1" == --help ]] && exit 0
  exit 2
fi

HOST="$1"
[[ "$HOST" =~ ^mox([1-9]|10)$ ]] || {
  printf 'ERROR: host must be named mox1 through mox10\n' >&2
  exit 2
}

for library in "$CONFIG_LIB" "$STORAGE_LIB"; do
  [[ -f "$library" && ! -L "$library" ]] || {
    printf 'ERROR: required library is unavailable: %s\n' "$library" >&2
    exit 2
  }
done
command -v python3 >/dev/null 2>&1 || {
  printf 'ERROR: python3 is required on this workstation\n' >&2
  exit 2
}
# shellcheck disable=SC1090
source "$CONFIG_LIB"
load_proxmox_config --no-secrets >/dev/null || {
  printf 'ERROR: cluster configuration is invalid\n' >&2
  exit 2
}
(("${HOST#mox}" <= MAX_MOX_HOSTS)) || {
  printf 'ERROR: %s exceeds MAX_MOX_HOSTS=%s\n' "$HOST" "$MAX_MOX_HOSTS" >&2
  exit 2
}
mox_is_reachable "$HOST" || {
  printf 'ERROR: %s is not reachable over strict SSH\n' "$HOST" >&2
  exit 1
}

RUN_DIR="$(mktemp -d "${TMPDIR:-/tmp}/show-proxmox-host-state.XXXXXX")"
chmod 0700 "$RUN_DIR"
trap 'rm -rf -- "$RUN_DIR"' EXIT
ATTENTION=0

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

on_host() {
  mox_ssh "$HOST" "$@" </dev/null
}

save_json() {
  local path="$1"
  shift
  "$@" >"$path" 2>"${path}.err" &&
    python3 -c 'import json,sys; json.load(open(sys.argv[1]))' "$path" 2>/dev/null
}

# Serials recorded in this workstation's env/moxN.conf, as PAIR:MEMBER:SERIAL.
configured_serials=()
serial_note="env/${HOST}.conf is not present on this workstation; serials were not compared."
if [[ -f "${PROXMOX_ENV_DIR}/${HOST}.conf" ]]; then
  if serial_text="$(
    bash -c '
set -Eeuo pipefail
source "$1"
load_proxmox_config --host "$2" --no-secrets >/dev/null
for pair in 1 2 3 4 5; do
  for member in 1 2; do
    name="NVME_MIRROR_${pair}_SERIAL_${member}"
    [[ -z "${!name:-}" ]] || printf "%s:%s:%s\n" "$pair" "$member" "${!name}"
  done
done
' bash "$CONFIG_LIB" "$HOST"
  )"; then
    mapfile -t configured_serials <<<"$serial_text"
    [[ -n "${configured_serials[0]:-}" ]] || configured_serials=()
    serial_note="Compared rpool with ${#configured_serials[@]} serial(s) from env/${HOST}.conf."
  else
    serial_note="env/${HOST}.conf could not be loaded; serials were not compared."
  fi
fi

section "Proxmox host diagnostic target" \
  "identify the host and the workstation configuration used for this read-only report."
printf 'Host:        %s (%s.%s)\n' "$HOST" "$HOST" "$PROXMOX_INTERNAL_DOMAIN"
printf 'Started:     %s\n' "$(date +%Y-%m-%dT%H:%M:%S%z)"
printf 'Serials:     %s\n' "$serial_note"

section "Identity, versions, and compute resources" \
  "show what is installed and how much CPU and RAM the host has for its guests."
run "Proxmox VE version" on_host pveversion
run "Kernel" on_host uname -r
run "Uptime and load" on_host uptime
run "CPU threads" on_host nproc
run "Memory" on_host free -h

section "Cluster membership, quorum, HA, and app-ha services" \
  "a host serves production only while it is quorate and its HA and app-ha services run."
run "Cluster quorum and membership" on_host pvecm status
run "Corosync link status" on_host corosync-cfgtool -s
run "HA manager status" on_host ha-manager status
# systemctl is-active succeeds when any one listed unit is active, so check
# each unit on its own.
printf '\n[CHECK] Core and app-ha unit states\n'
for unit in pve-cluster.service corosync.service pve-ha-lrm.service \
  pve-ha-crm.service keepalived.service app-ha-deferred-cleanup.timer \
  app-ha-haproxy-route-sync.timer; do
  unit_state="$(on_host systemctl is-active "$unit" 2>/dev/null)" || true
  if [[ "$unit_state" == active ]]; then
    printf '  [PASS] %s is active.\n' "$unit"
  else
    printf '  [ATTENTION] %s is %s.\n' "$unit" "${unit_state:-unknown}"
    ATTENTION=1
  fi
done
if ! failed_units="$(on_host systemctl --failed --no-legend --plain --no-pager)"; then
  printf '\n[ATTENTION] Failed systemd units could not be listed.\n'
  ATTENTION=1
elif [[ -n "$failed_units" ]]; then
  printf '\n[ATTENTION] Failed systemd units:\n%s\n' "$failed_units"
  ATTENTION=1
else
  printf '\n  [PASS] No failed systemd units.\n'
fi

section "Network" \
  "show the private bridge, the shared guest egress VIP, and Tailscale admin access."
run "IPv4 addresses" on_host ip -br -4 address
run "IPv4 routes" on_host ip -4 route show
run "Tailscale IPv4 address" on_host tailscale ip -4
if on_host ip -4 -o address show 2>/dev/null |
  awk -v vip="$GUEST_EGRESS_VIP" '$4 == vip { found=1 } END { exit !found }'; then
  printf '\n  %s currently holds the guest egress VIP %s.\n' "$HOST" "$GUEST_EGRESS_VIP"
else
  printf '\n  %s does not currently hold the guest egress VIP %s.\n' "$HOST" "$GUEST_EGRESS_VIP"
fi

section "Guests on this host and replicas stored here" \
  "show every guest this host runs, the production replicas it stores, and pending app-ha work for it."
collected=1
save_json "${RUN_DIR}/qemu.json" on_host pvesh get "/nodes/${HOST}/qemu" --output-format json || collected=0
save_json "${RUN_DIR}/lxc.json" on_host pvesh get "/nodes/${HOST}/lxc" --output-format json || collected=0
save_json "${RUN_DIR}/cluster-vms.json" on_host pvesh get /cluster/resources --type vm --output-format json || collected=0
save_json "${RUN_DIR}/ha.json" on_host pvesh get /cluster/ha/resources --output-format json || collected=0
save_json "${RUN_DIR}/replication.json" on_host pvesh get /cluster/replication --output-format json || collected=0
registry_note=""
if ! save_json "${RUN_DIR}/resources.json" on_host "$REMOTE_REGISTRY" \
  --state-dir "$CLUSTER_STATE_DIR" list --record-type resources ||
  ! save_json "${RUN_DIR}/cleanup.json" on_host "$REMOTE_REGISTRY" \
    --state-dir "$CLUSTER_STATE_DIR" list --record-type cleanup; then
  registry_note="The app-ha registry could not be read; roles and cleanup records are omitted."
  printf '[]\n' >"${RUN_DIR}/resources.json"
  printf '[]\n' >"${RUN_DIR}/cleanup.json"
fi
if ((collected)); then
  python3 - "$HOST" "$RUN_DIR" "$registry_note" <<'PY'
import json
from pathlib import Path
import sys

host, run_dir, registry_note = sys.argv[1], Path(sys.argv[2]), sys.argv[3]


def load(name):
    return json.loads((run_dir / name).read_text(encoding="utf-8"))


qemu, lxc = load("qemu.json"), load("lxc.json")
cluster_vms, ha = load("cluster-vms.json"), load("ha.json")
replication, resources, cleanup = (
    load("replication.json"), load("resources.json"), load("cleanup.json")
)
registry = {int(row["vmid"]): row for row in resources if row.get("vmid") is not None}
ha_state = {row.get("sid"): row.get("state", "?") for row in ha}
names = {int(row["vmid"]): row.get("name", "?") for row in cluster_vms}
nodes = {int(row["vmid"]): row.get("node", "?") for row in cluster_vms}


def table(headers, rows):
    cells = [[str(value) for value in row] for row in rows]
    widths = [
        max([len(header), *(len(row[index]) for row in cells)])
        for index, header in enumerate(headers)
    ]
    print("  ".join(h.ljust(widths[i]) for i, h in enumerate(headers)).rstrip())
    for row in cells:
        print("  ".join(v.ljust(widths[i]) for i, v in enumerate(row)).rstrip())


def gib(value):
    return f"{int(value) / 1024**3:.1f}" if value else "-"


attention = []
rows = []
guests = [("qemu", row) for row in qemu] + [("lxc", row) for row in lxc]
for kind, row in sorted(guests, key=lambda item: int(item[1]["vmid"])):
    vmid = int(row["vmid"])
    record = registry.get(vmid)
    if record is not None:
        role, state = record["kind"], record["state"]
    elif kind == "lxc" and str(row.get("name", "")).startswith("haproxy"):
        role, state = "haproxy ingress", "-"
    else:
        role, state = "unregistered", "-"
    sid = f"{'vm' if kind == 'qemu' else 'ct'}:{vmid}"
    rows.append(
        [
            vmid,
            row.get("name", "?"),
            kind,
            row.get("status", "?"),
            role,
            state,
            ha_state.get(sid, "-"),
            row.get("cpus", "-"),
            gib(row.get("maxmem")),
            gib(row.get("maxdisk")),
        ]
    )
    if record and record["kind"] == "production" and record["state"] == "active" \
            and row.get("status") != "running":
        attention.append(f"production {record['name']} is active in the registry but {row.get('status')} here")
running_production = [
    row for row in qemu
    if registry.get(int(row["vmid"]), {}).get("kind") == "production"
    and row.get("status") == "running"
]
running_staging = [
    row for row in qemu
    if registry.get(int(row["vmid"]), {}).get("kind") == "staging"
    and row.get("status") == "running"
]
if running_production and running_staging:
    attention.append(
        "staging is running beside production on this host: "
        + ", ".join(row.get("name", "?") for row in running_staging)
    )

print(f"Guests running on or assigned to {host}:")
if rows:
    table(
        ["VMID", "NAME", "TYPE", "STATUS", "ROLE", "REGISTRY", "HA", "CPUS", "RAM_GIB", "DISK_GIB"],
        rows,
    )
else:
    print("  none")

stored_here = [row for row in replication if row.get("target") == host]
print(f"\nProduction replicas stored on {host} (replication jobs targeting it):")
if stored_here:
    table(
        ["JOB", "GUEST", "RUNS_ON", "SCHEDULE"],
        [
            [
                row.get("id", "?"),
                f"{names.get(int(row['guest']), '?')} ({row['guest']})",
                nodes.get(int(row["guest"]), "?"),
                row.get("schedule", "*/15"),
            ]
            for row in stored_here
        ],
    )
else:
    print("  none")

sourced_here = [
    row for row in replication if nodes.get(int(row["guest"])) == host
]
print(f"\nReplication jobs for guests running on {host}:")
if sourced_here:
    table(
        ["JOB", "GUEST", "TARGET", "SCHEDULE"],
        [
            [
                row.get("id", "?"),
                f"{names.get(int(row['guest']), '?')} ({row['guest']})",
                row.get("target", "?"),
                row.get("schedule", "*/15"),
            ]
            for row in sourced_here
        ],
    )
else:
    print("  none")

pending = [
    row for row in cleanup if row.get("node") == host and row.get("state") == "pending"
]
print(f"\nPending deferred-cleanup records assigned to {host}:")
if pending:
    table(
        ["ACTION", "RESOURCE", "TARGET", "ATTEMPTS", "CREATED"],
        [
            [row["action"], row["resource"], row["target"], row.get("attempts", 0), row.get("created_at", "?")]
            for row in pending
        ],
    )
else:
    print("  none")
if registry_note:
    print(f"\n{registry_note}")

for item in attention:
    print(f"\n[ATTENTION] {item}")
raise SystemExit(1 if attention else 0)
PY
  (($? == 0)) || ATTENTION=1
else
  printf '[ATTENTION] Proxmox guest inventory could not be collected:\n'
  cat "${RUN_DIR}"/*.err 2>/dev/null
  ATTENTION=1
fi
run "Production-start reservations (present only while production is starting)" \
  on_host bash -c 'ls -l -- "$1" 2>/dev/null || echo "none"' bash "$RESERVATION_DIR"

section "Storage: rpool layout, disks, and zvols" \
  "show every vdev resolved to physical disk serials, the boot vdev, removal progress, unused disks, and per-zvol allocation."
storage_json="${RUN_DIR}/storage.json"
if mox_ssh "$HOST" python3 - collect <"$STORAGE_LIB" >"$storage_json" 2>"${storage_json}.err"; then
  render_args=(render "$storage_json" --exit-status)
  [[ -s "${RUN_DIR}/cluster-vms.json" ]] &&
    render_args+=(--guests "${RUN_DIR}/cluster-vms.json")
  for configured in "${configured_serials[@]}"; do
    render_args+=(--configured-serial "$configured")
  done
  python3 "$STORAGE_LIB" "${render_args[@]}"
  (($? == 0)) || ATTENTION=1
else
  printf '[ATTENTION] The rpool layout could not be collected:\n'
  cat "${storage_json}.err"
  ATTENTION=1
fi
run "Full rpool status" on_host zpool status -v rpool
run "Boot ESPs managed by proxmox-boot-tool" on_host proxmox-boot-tool status
run "LUKS unlock configuration (crypttab holds no key material)" \
  on_host bash -c 'cat /etc/crypttab 2>/dev/null || echo "no /etc/crypttab (unencrypted host)"'
run "Mounted filesystem capacity" on_host df -hT -x tmpfs -x devtmpfs

section "Proxmox host diagnostic verdict" \
  "summarize whether anything above needs an operator's attention."
if ((ATTENTION == 0)); then
  printf '  [OK] %s has no attention items.\n' "$HOST"
  exit 0
fi
printf '  [ATTENTION] %s has one or more attention items above.\n' "$HOST"
exit 1
