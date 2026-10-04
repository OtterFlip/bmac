#!/usr/bin/env bash

# Copyright (c) 2026 BEENTHERE VENTURES, INC.
# SPDX-License-Identifier: GPL-3.0-only

# Read-only hardware and network health check for the cluster: whether every
# Proxmox host and the QDevice are up and reachable, whether every host's
# corosync links reach every other host, whether every ZFS pool, vdev member,
# and drive is healthy, and whether every host's rpool has at least 10% of its
# usable space free. Guests are not inspected.

set -u
set -o pipefail
umask 077

if ((BASH_VERSINFO[0] < 4 || (BASH_VERSINFO[0] == 4 && BASH_VERSINFO[1] < 4))); then
  printf 'ERROR: show_cluster_health.sh requires Bash 4.4 or newer (found %s).\n' \
    "${BASH_VERSION:-unknown}" >&2
  exit 2
fi

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
REPO_ROOT="$(cd -- "${SCRIPT_DIR}/.." && pwd -P)"
CONFIG_LIB="${REPO_ROOT}/lib/config.sh"
CONTROL_LIB="${REPO_ROOT}/lib/cluster_control.sh"
MEMBERSHIP_LIB="${REPO_ROOT}/lib/host_membership.sh"

for library in "$CONFIG_LIB" "$CONTROL_LIB" "$MEMBERSHIP_LIB"; do
  [[ -f "$library" && ! -L "$library" ]] || {
    printf 'ERROR: required library is unavailable: %s\n' "$library" >&2
    exit 2
  }
done
# shellcheck source=../lib/config.sh
source "$CONFIG_LIB"
# shellcheck source=../lib/cluster_control.sh
source "$CONTROL_LIB"
# shellcheck source=../lib/host_membership.sh
source "$MEMBERSHIP_LIB"

PROBE=""
REGISTERED=0
REGISTERED_ADDRESS=""
RUN_DIR=""

usage() {
  cat <<'EOF'
Usage: diagnostics/show_cluster_health.sh

Quickly check the cluster's hardware and networking:

  - every Proxmox host: online in the cluster, reachable over SSH from this
    workstation, quorate, and running corosync and pve-cluster;
  - every corosync link (link0 private VLAN, link1 Tailscale) from each host
    to each other host, as corosync itself reports it;
  - the QDevice: needed, registered, reachable, serving the cluster, and
    voting on every host;
  - every ZFS pool and vdev member on every host, including read, write, and
    checksum error counts, and each member's physical disk serial;
  - free space on every host's rpool, flagging a host that needs more
    storage when less than 10% of rpool's usable space is free;
  - the SMART overall health of every physical drive.

Guests are not inspected. A host this workstation cannot reach is inspected
through another member over cluster SSH when possible. For full detail, use
show_cluster_state.sh, show_proxmox_host_state.sh, or show_qdevice_state.sh.

Nothing is changed. Commands create normal SSH and command-access log entries.

Exit status: 0 when nothing needs attention, 1 when something does, and 2
for usage or configuration errors.
EOF
}

# Runs on each Proxmox host as root and prints one JSON report of raw command
# output; the workstation parses it.
HEALTH_COLLECTOR="$(
  cat <<'PY'
import json
import os
import re
import socket
import subprocess


def run(argv, timeout=60):
    try:
        done = subprocess.run(
            argv,
            stdin=subprocess.DEVNULL,
            capture_output=True,
            text=True,
            timeout=timeout,
            env=dict(os.environ, LC_ALL="C"),
        )
    except FileNotFoundError:
        return {"rc": 127, "out": "", "err": f"{argv[0]} is not installed"}
    except subprocess.TimeoutExpired:
        return {"rc": 124, "out": "", "err": f"timed out after {timeout}s"}
    return {"rc": done.returncode, "out": done.stdout, "err": done.stderr.strip()}


report = {
    "hostname": socket.gethostname().split(".")[0],
    "uptime": run(["uptime", "-p"]),
    "pvecm_status": run(["pvecm", "status"]),
    "corosync_nodes": run(["corosync-cfgtool", "-n"]),
    "units": {
        unit: run(["systemctl", "is-active", unit])["out"].strip() or "unknown"
        for unit in ("corosync.service", "pve-cluster.service", "corosync-qdevice.service")
    },
    "zpool_list": run(["zpool", "list", "-H", "-o", "name"]),
    "rpool_space": run(["zfs", "list", "-H", "-p", "-o", "used,available", "rpool"]),
    "pools": {},
    "realpaths": {},
    "lsblk": run(["lsblk", "-J", "-p", "-o", "NAME,KNAME,TYPE,SERIAL,MODEL"]),
    "smart": {},
}
if report["zpool_list"]["rc"] == 0:
    for pool in report["zpool_list"]["out"].split():
        status = run(["zpool", "status", "-P", "-p", pool])
        report["pools"][pool] = {
            "status": status,
            "x": run(["zpool", "status", "-x", pool]),
        }
        for leaf in re.findall(r"^\s+(/dev/\S+)", status["out"], re.M):
            report["realpaths"][leaf] = os.path.realpath(leaf)
try:
    devices = json.loads(report["lsblk"]["out"])["blockdevices"]
except (ValueError, KeyError, TypeError):
    devices = []
for device in devices:
    name = device.get("name") or ""
    if device.get("type") == "disk" and not re.match(r"/dev/(zd|rbd|nbd|zram|ram)", name):
        report["smart"][name] = run(["smartctl", "-H", name], timeout=30)
print(json.dumps(report))
PY
)"

qdevice_ssh() {
  ssh -o BatchMode=yes -o StrictHostKeyChecking=yes -o CheckHostIP=yes \
    -o ConnectTimeout=10 "root@${PROXMOX_QDEVICE_HOST}" "$@"
}

json_field() {
  python3 -c '
import json, sys
value = json.load(open(sys.argv[1], encoding="utf-8"))
for key in sys.argv[2:]:
    value = value[key]
print(value)
' "$@"
}

find_cluster() {
  local status=0
  control_find_cluster || status=$?
  if ((status == 1)); then
    printf '\nATTENTION: no cluster member (mox1 through mox%s) is reachable over strict SSH\n' \
      "$MAX_MOX_HOSTS"
    printf 'from this workstation, or none of the reachable hosts belongs to a cluster.\n'
    printf 'Check this workstation'"'"'s Tailscale connection, then the hosts themselves (for\n'
    printf 'example through iDRAC).\n'
    exit 1
  elif ((status != 0)); then
    printf 'ERROR: cluster membership reported through %s is malformed\n' \
      "$CONTROL_PROBE_NODE" >&2
    exit 1
  fi
  PROBE="$CONTROL_PROBE_NODE"
  # shellcheck disable=SC2034 # Read by the lib/host_membership.sh helpers.
  HM_COORDINATOR="$PROBE"
  printf 'Read cluster membership through %s: %s\n' "$PROBE" "${CONTROL_MEMBER_NODES[*]}"

  mox_ssh "$PROBE" pvesh get /cluster/config/nodes --output-format json \
    </dev/null >"${RUN_DIR}/config-nodes.json" 2>/dev/null ||
    printf '[]\n' >"${RUN_DIR}/config-nodes.json"

  local registered_status=0
  hm_qdevice_configured 2>/dev/null || registered_status=$?
  case "$registered_status" in
    0)
      REGISTERED=1
      REGISTERED_ADDRESS="$(hm_registered_qdevice_address 2>/dev/null)"
      ;;
    1) ;;
    *)
      printf 'ERROR: could not read /etc/pve/corosync.conf on %s\n' "$PROBE" >&2
      exit 1
      ;;
  esac
}

# Collect one host's report directly, or through PROBE over cluster SSH when
# this workstation cannot reach it. Appends a row to members.tsv.
collect_host() {
  local node="$1" state=offline access=none via=- collected=0 voting=- status
  local out="${RUN_DIR}/${node}.json"
  control_list_contains "$node" "${CONTROL_ONLINE_NODES[@]}" && state=online
  printf 'Collecting from %s...\n' "$node"
  if mox_is_reachable "$node"; then
    access=direct
    mox_ssh "$node" python3 - <<<"$HEALTH_COLLECTOR" >"$out" 2>"${out}.err" &&
      collected=1
  elif [[ "$node" != "$PROBE" ]] &&
    hm_exec "$node" python3 -c "$HEALTH_COLLECTOR" >"$out" 2>"${out}.err"; then
    access=relay
    via="$PROBE"
    collected=1
  fi
  if ((collected)) &&
    ! python3 -c 'import json, sys; json.load(open(sys.argv[1]))' "$out" 2>/dev/null; then
    collected=0
  fi
  local member_count="${#CONTROL_MEMBER_NODES[@]}"
  if ((collected && REGISTERED && member_count % 2 == 0)); then
    status="$(json_field "$out" pvecm_status out)"
    voting=no
    hm_qdevice_status_is_healthy "$status" "$member_count" && voting=yes
  fi
  printf '%s\t%s\t%s\t%s\t%s\t%s\n' "$node" "$state" "$access" "$via" "$collected" "$voting" \
    >>"${RUN_DIR}/members.tsv"
}

# The QDevice host is checked when the cluster has or needs a QDevice.
probe_qdevice() {
  local report="${RUN_DIR}/qdevice.txt"
  ((REGISTERED || ${#CONTROL_MEMBER_NODES[@]} % 2 == 0)) || return 0
  printf 'Collecting from %s...\n' "$PROXMOX_QDEVICE_HOST"
  if ! qdevice_ssh true </dev/null >/dev/null 2>&1; then
    printf 'SSH failed\n' >"$report"
    return 0
  fi
  # shellcheck disable=SC2016 # Evaluated on the QDevice host.
  qdevice_ssh bash -s >"$report" 2>/dev/null <<'REMOTE'
printf 'SSH ok\n'
printf 'TAILSCALE %s\n' "$(tailscale ip -4 2>/dev/null | head -n 1)"
printf 'UPTIME %s\n' "$(uptime -p 2>/dev/null)"
printf 'QNETD_ACTIVE %s\n' "$(systemctl is-active corosync-qnetd 2>/dev/null || true)"
if ss -lnt 2>/dev/null | awk '$4 ~ /:5403$/ { found=1 } END { exit !found }'; then
  printf 'LISTEN yes\n'
else
  printf 'LISTEN no\n'
fi
printf 'QNETD_TOOL_BEGIN\n'
corosync-qnetd-tool -l 2>&1 || true
printf 'QNETD_TOOL_END\n'
REMOTE
  [[ -s "$report" ]] || printf 'SSH ok\n' >"$report"
}

render_report() {
  python3 - "$RUN_DIR" "$PROXMOX_CLUSTER_NAME" "$PROBE" "$REGISTERED" \
    "$REGISTERED_ADDRESS" "$PROXMOX_QDEVICE_HOST" <<'PY'
import json
from pathlib import Path
import re
import sys

run_dir = Path(sys.argv[1])
cluster, probe, registered, registered_address, qdevice_host = sys.argv[2:7]
registered = registered == "1"
LINK_NAMES = {0: "private VLAN", 1: "Tailscale"}
RPOOL_MIN_FREE_FRACTION = 0.10
problems = []
disk_problem_hosts = set()
storage_hosts = set()


def hr():
    print("=" * 96)


def section(title, why):
    print()
    hr()
    print(title)
    print(f"Why: {why}")
    hr()


def table(headers, rows, indent="  "):
    cells = [[str(value) for value in row] for row in rows]
    widths = [
        max([len(header), *(len(row[index]) for row in cells)])
        for index, header in enumerate(headers)
    ]

    def line(values):
        return indent + "  ".join(
            value.ljust(widths[index]) for index, value in enumerate(values)
        ).rstrip()

    print(line(headers))
    for row in cells:
        print(line(row))


def first_line(text):
    return next((line.strip() for line in text.splitlines() if line.strip()), "")


def status_value(text, key):
    match = re.search(rf"^{re.escape(key)}:\s*(.*?)\s*$", text, re.M)
    return match.group(1) if match else None


members = []
for line in (run_dir / "members.tsv").read_text(encoding="utf-8").splitlines():
    node, state, access, via, collected, voting = line.split("\t")
    report = None
    if collected == "1":
        report = json.loads((run_dir / f"{node}.json").read_text(encoding="utf-8"))
    members.append(
        {"node": node, "state": state, "access": access, "via": via,
         "voting": voting, "report": report}
    )
member_names = [member["node"] for member in members]
member_count = len(members)
reports = {member["node"]: member["report"] for member in members if member["report"]}

try:
    config_nodes = json.loads((run_dir / "config-nodes.json").read_text(encoding="utf-8"))
except ValueError:
    config_nodes = []
node_names = {
    int(row["nodeid"]): row.get("name", row.get("node"))
    for row in config_nodes
    if str(row.get("nodeid", "")).isdigit()
}

# Hosts ------------------------------------------------------------------------
section(
    "PROXMOX HOSTS",
    "a host that is offline, unreachable, or not quorate cannot run or protect guests.",
)
rows = []
for member in members:
    node, report = member["node"], member["report"]
    if member["state"] != "online":
        problems.append(f"{node} is offline in the Proxmox cluster")
    if member["access"] == "direct":
        ssh = "ok"
    elif member["access"] == "relay":
        ssh = f"FAILED (inspected via {member['via']})"
        problems.append(
            f"this workstation cannot SSH to {node}; it was inspected through "
            f"{member['via']} over cluster SSH (check {node}'s Tailscale connection)"
        )
    else:
        ssh = "FAILED"
        problems.append(
            f"{node} cannot be reached over SSH from this workstation or from {probe}"
        )
    if report is None:
        if member["access"] == "direct":
            problems.append(f"could not collect health data from {node}")
        rows.append([node, member["state"], ssh, "?", "?", "?", "?", "?", "?"])
        continue
    pvecm = report["pvecm_status"]
    quorate = votes = "?"
    if pvecm["rc"] == 0:
        quorate = (status_value(pvecm["out"], "Quorate") or "?").lower()
        votes = (
            f"{status_value(pvecm['out'], 'Total votes') or '?'}/"
            f"{status_value(pvecm['out'], 'Expected votes') or '?'}"
        )
        if quorate != "yes":
            problems.append(f"{node} is not quorate")
    else:
        problems.append(
            f"pvecm status failed on {node}: {first_line(pvecm['err'] or pvecm['out'])}"
        )
    units = report["units"]
    for unit in ("corosync.service", "pve-cluster.service"):
        if units.get(unit) != "active":
            problems.append(f"{unit} is {units.get(unit, 'unknown')} on {node}")
    qdevice_client = "-"
    if registered:
        qdevice_client = units.get("corosync-qdevice.service", "unknown")
        if qdevice_client != "active":
            problems.append(f"corosync-qdevice.service is {qdevice_client} on {node}")
    if member["voting"] == "no":
        problems.append(f"{node} does not see the QDevice alive and voting")
    rows.append(
        [
            node,
            member["state"],
            ssh,
            quorate,
            votes,
            units.get("corosync.service", "?"),
            units.get("pve-cluster.service", "?"),
            qdevice_client if member["voting"] == "-" else f"{qdevice_client}, voting {member['voting']}",
            first_line(report["uptime"]["out"]).removeprefix("up ") or "?",
        ]
    )
table(
    ["HOST", "PROXMOX", "SSH FROM HERE", "QUORATE", "VOTES", "COROSYNC",
     "PVE-CLUSTER", "QDEVICE CLIENT", "UPTIME"],
    rows,
)

# Corosync links ---------------------------------------------------------------
section(
    "COROSYNC LINKS BETWEEN HOSTS",
    "corosync carries quorum and cluster traffic; each host must reach every other host.",
)


def parse_links(text):
    local, peers, current = None, {}, None
    for raw in text.splitlines():
        line = raw.strip()
        match = re.match(r"Local node ID (\d+)", line)
        if match:
            local = int(match.group(1))
            continue
        match = re.match(r"nodeid:?\s*(\d+)\s*:?\s*(reachable|unreachable)?", line)
        if match:
            current = int(match.group(1))
            peers[current] = {
                "reachable": None if match.group(2) is None else match.group(2) == "reachable",
                "links": {},
            }
            continue
        match = re.match(r"LINK:?\s*(\d+)\b(.*)$", line)
        if match and current is not None:
            rest = match.group(2)
            peers[current]["links"][int(match.group(1))] = (
                "disabled" not in rest
                and "disconnected" not in rest
                and "connected" in rest
            )
    if local is not None:
        peers.pop(local, None)
    return peers


names_by_id = {nodeid: name for nodeid, name in node_names.items() if name in member_names}
ids_by_name = {name: nodeid for nodeid, name in names_by_id.items()}
views = {}
for node in member_names:
    report = reports.get(node)
    if report is None:
        continue
    result = report["corosync_nodes"]
    if result["rc"] != 0:
        problems.append(
            f"corosync-cfgtool -n failed on {node}: {first_line(result['err'] or result['out'])}"
        )
        continue
    views[node] = {
        names_by_id.get(nodeid, f"nodeid {nodeid}"): peer
        for nodeid, peer in parse_links(result["out"]).items()
    }

print("Each cell is the row host's view of the column host. Links: "
      "link0 = private VLAN, link1 = Tailscale.")
print("'ok' means the link is connected; 'DOWN' means it is not; 'no data' means")
print("the row host could not be inspected.\n")
down = {}
rows = []
for observer in member_names:
    row = [observer]
    for peer in member_names:
        if peer == observer:
            row.append("-")
            continue
        if observer not in views:
            row.append("no data")
            continue
        view = views[observer].get(peer)
        if view is None:
            row.append("missing")
            problems.append(f"corosync on {observer} does not list {peer}")
            continue
        links = view["links"]
        if view["reachable"] is False or (links and not any(links.values())):
            row.append("DOWN")
            down.setdefault(peer, {})[observer] = None
            continue
        row.append("/".join(
            f"L{link} {'ok' if connected else 'DOWN'}" for link, connected in sorted(links.items())
        ) or "reachable")
        failed = [link for link, connected in sorted(links.items()) if not connected]
        if failed:
            down.setdefault(peer, {})[observer] = failed
    rows.append(row)
table(["FROM \\ TO", *member_names], rows)

for peer, observers in down.items():
    fully = [observer for observer, links in observers.items() if links is None]
    others = [observer for observer in views if observer != peer]
    if fully and sorted(fully) == sorted(others):
        problems.append(f"no inspected host can reach {peer} over any corosync link")
        continue
    for observer in fully:
        problems.append(f"{observer} cannot reach {peer} over any corosync link")
    for observer, links in observers.items():
        for link in links or []:
            problems.append(
                f"{observer} cannot reach {peer} over corosync link{link} "
                f"({LINK_NAMES.get(link, 'unknown network')})"
            )

# QDevice ----------------------------------------------------------------------
section(
    f"QDEVICE {qdevice_host}",
    "with an even number of hosts, the QDevice's tie-breaking vote keeps quorum when one host fails.",
)
needed = member_count % 2 == 0
print(f"Hosts in cluster:   {member_count} ({'even: a QDevice is required' if needed else 'odd: no QDevice is used'})")
print(f"Registered QDevice: {(registered_address or 'unknown address') if registered else 'none'}")
if needed and not registered:
    problems.append(
        f"the {member_count}-host cluster needs a QDevice and none is registered "
        "(see diagnostics/show_qdevice_state.sh)"
    )
if registered and not needed:
    problems.append(
        f"a QDevice is registered but the {member_count}-host cluster must not use one "
        "(see diagnostics/show_qdevice_state.sh)"
    )
qdevice_file = run_dir / "qdevice.txt"
if qdevice_file.exists():
    text = qdevice_file.read_text(encoding="utf-8")
    values = {}
    for line in text.splitlines():
        key, _, value = line.partition(" ")
        values.setdefault(key, value.strip())
    reachable = values.get("SSH") == "ok"
    print(f"SSH from here:      {'ok' if reachable else 'FAILED'}")
    if not reachable:
        if registered:
            problems.append(
                f"ssh root@{qdevice_host} fails from this workstation "
                "(see diagnostics/show_qdevice_state.sh)"
            )
    else:
        tool = text.partition("QNETD_TOOL_BEGIN\n")[2].partition("QNETD_TOOL_END")[0]
        serves = f'Cluster "{cluster}":' in tool
        print(f"Uptime:             {values.get('UPTIME', '').removeprefix('up ') or '?'}")
        print(f"Tailscale address:  {values.get('TAILSCALE') or '?'}")
        print(f"corosync-qnetd:     {values.get('QNETD_ACTIVE') or 'unknown'}, "
              f"listening on TCP 5403: {values.get('LISTEN', '?')}, "
              f"serving cluster {cluster}: {'yes' if serves else 'no'}")
        if registered:
            if values.get("TAILSCALE") and values["TAILSCALE"] != registered_address:
                problems.append(
                    f"{qdevice_host} ({values['TAILSCALE']}) is not the registered QDevice "
                    f"({registered_address})"
                )
            if values.get("QNETD_ACTIVE") != "active":
                problems.append(
                    f"corosync-qnetd is {values.get('QNETD_ACTIVE') or 'unknown'} on {qdevice_host}"
                )
            if values.get("LISTEN") != "yes":
                problems.append(f"corosync-qnetd is not listening on TCP 5403 on {qdevice_host}")
            if not serves:
                problems.append(f"corosync-qnetd on {qdevice_host} does not list cluster {cluster}")
if registered and needed:
    print("Voting on hosts:    " + ", ".join(
        f"{member['node']} {member['voting'] if member['voting'] != '-' else 'unknown'}"
        for member in members
    ))

# ZFS --------------------------------------------------------------------------
section(
    "ZFS POOLS AND VDEV MEMBERS",
    "a faulted, missing, or erroring member means a drive, cable, or controller needs attention.",
)
HEADER = re.compile(
    r"^\s*(pool|state|status|action|see|scan|remove|checkpoint|expand|config|errors):\s?(.*)$"
)


def parse_pool(text):
    header, entries, key, in_config = {}, [], None, False
    for raw in text.splitlines():
        match = HEADER.match(raw)
        if match and not (in_config and raw.startswith("\t")):
            key = match.group(1)
            header[key] = match.group(2).strip()
            in_config = key == "config"
            continue
        if in_config:
            body = raw[1:] if raw.startswith("\t") else raw
            tokens = body.split()
            if not tokens or (tokens[0] == "NAME" and "STATE" in tokens):
                continue
            counts = tokens[2:5] if len(tokens) >= 5 else None
            entries.append(
                {
                    "name": tokens[0],
                    "depth": (len(body) - len(body.lstrip(" "))) // 2,
                    "state": tokens[1] if len(tokens) > 1 else None,
                    "counts": counts,
                    "note": " ".join(tokens[5:]),
                }
            )
        elif key and raw.startswith("\t") and raw.strip():
            header[key] += " " + raw.strip()
    return header, entries


def disk_index(report):
    index = {}
    try:
        devices = json.loads(report["lsblk"]["out"])["blockdevices"]
    except (ValueError, KeyError, TypeError):
        return index

    def walk(device, disk):
        if device.get("type") == "disk" and disk is None:
            disk = device
        for key in ("name", "kname"):
            if device.get(key):
                index.setdefault(device[key], disk)
        for child in device.get("children") or []:
            walk(child, disk)

    for device in devices:
        walk(device, None)
    return index


def disk_label(disk):
    if not disk:
        return "?"
    return f"{disk['name'].removeprefix('/dev/')} {(disk.get('serial') or 'no-serial').strip()}"


def nonzero(counts):
    return counts is not None and any(count != "0" for count in counts)


for node in member_names:
    report = reports.get(node)
    print(f"\n[{node}]")
    if report is None:
        print("  not inspected")
        continue
    listing = report["zpool_list"]
    if listing["rc"] != 0:
        print(f"  zpool list failed: {first_line(listing['err'] or listing['out'])}")
        problems.append(f"zpool list failed on {node}")
        disk_problem_hosts.add(node)
        continue
    if not report["pools"]:
        print("  no ZFS pools")
        problems.append(f"{node} has no ZFS pools")
        disk_problem_hosts.add(node)
        continue
    index = disk_index(report)
    for pool, result in sorted(report["pools"].items()):
        before = len(problems)
        header, entries = parse_pool(result["status"]["out"])
        state = header.get("state") or "?"
        print(f"  Pool {pool}: {state}")
        for key in ("status", "action", "scan", "remove", "errors"):
            if header.get(key):
                print(f"    {key + ':':<8}{header[key]}")
        if result["status"]["rc"] != 0 or not entries:
            problems.append(
                f"zpool status {pool} failed on {node}: "
                f"{first_line(result['status']['err'] or result['status']['out'])}"
            )
            continue
        if state != "ONLINE":
            problems.append(f"{node}: pool {pool} is {state}")
        rows, group = [], None
        for position, entry in enumerate(entries):
            following = entries[position + 1] if position + 1 < len(entries) else None
            has_children = following is not None and following["depth"] > entry["depth"]
            if entry["depth"] == 0:
                group = None if entry["state"] else entry["name"]
            leaf = not has_children and entry["state"] is not None
            disk = None
            if leaf:
                disk = index.get(entry["name"]) or index.get(
                    report["realpaths"].get(entry["name"], "")
                )
            counts = entry["counts"] or ["", "", ""]
            rows.append(
                [
                    "  " * entry["depth"] + entry["name"],
                    entry["state"] or "",
                    *counts,
                    disk_label(disk) if leaf else "",
                    entry["note"],
                ]
            )
            if entry["state"] is None or entry["depth"] == 0:
                continue
            ok_states = {"AVAIL", "INUSE"} if group == "spares" else {"ONLINE"}
            label = entry["name"] + (f" ({disk_label(disk)})" if leaf else "")
            kind = "member" if leaf else "vdev"
            if entry["state"] not in ok_states and (leaf or entry["state"] != "DEGRADED"):
                note = f" ({entry['note']})" if entry["note"] else ""
                problems.append(f"{node}: {pool} {kind} {label} is {entry['state']}{note}")
            if nonzero(entry["counts"]):
                problems.append(
                    f"{node}: {pool} {kind} {label} has read/write/checksum errors "
                    + "/".join(entry["counts"])
                )
        table(["NAME", "STATE", "READ", "WRITE", "CKSUM", "DISK", "NOTE"], rows, indent="    ")
        errors = header.get("errors", "")
        if errors and not errors.startswith("No known data errors"):
            problems.append(f"{node}: pool {pool} reports data errors: {errors}")
        scan = header.get("scan", "")
        if "resilver in progress" in scan:
            problems.append(f"{node}: pool {pool} is resilvering; redundancy is reduced until it finishes")
        match = re.search(r"with (\d+) errors", scan)
        if match and int(match.group(1)) > 0:
            problems.append(f"{node}: the last scan of pool {pool} found {match.group(1)} errors")
        if len(problems) == before and f"pool '{pool}' is healthy" not in result["x"]["out"]:
            problems.append(
                f"{node}: zpool status -x reports pool {pool} needs attention: "
                f"{header.get('status') or first_line(result['x']['out'])}"
            )
        if len(problems) > before:
            disk_problem_hosts.add(node)

# rpool space ------------------------------------------------------------------
section(
    "RPOOL FREE SPACE",
    f"guest disks live on rpool; below {RPOOL_MIN_FREE_FRACTION:.0%} free, ZFS slows "
    "and guest writes can fail.",
)


def size(value):
    if value >= 2**40:
        return f"{value / 2**40:.2f} TiB"
    return f"{value / 2**30:.1f} GiB"


print("USABLE is rpool's used plus available space as ZFS reports it, after "
      "redundancy and reserved slop.\n")
rows = []
for node in member_names:
    report = reports.get(node)
    if report is None:
        rows.append([node, "?", "?", "?", "?", "not inspected"])
        continue
    result = report.get("rpool_space") or {"rc": 1, "out": "", "err": "not collected"}
    fields = result["out"].split()
    if result["rc"] != 0 or len(fields) != 2 or not all(field.isdigit() for field in fields):
        rows.append([node, "?", "?", "?", "?", "unknown"])
        reason = (
            first_line(result["err"] or result["out"]) if result["rc"] != 0
            else f"unexpected zfs list output {result['out'].strip()!r}"
        )
        problems.append(f"could not read rpool free space on {node}: {reason}")
        continue
    used, free = (int(field) for field in fields)
    usable = used + free
    fraction = free / usable if usable else 0.0
    status = "ok"
    if fraction < RPOOL_MIN_FREE_FRACTION:
        status = "LOW: add storage"
        storage_hosts.add(node)
        problems.append(
            f"{node}: rpool has only {size(free)} free ({fraction:.1%} of {size(usable)} "
            f"usable, below {RPOOL_MIN_FREE_FRACTION:.0%}); {node} needs more storage added"
        )
    rows.append([node, size(used), size(free), size(usable), f"{fraction:.1%}", status])
table(["HOST", "USED", "FREE", "USABLE", "FREE %", "STATUS"], rows)

# SMART ------------------------------------------------------------------------
section(
    "DRIVE SMART HEALTH",
    "a drive's own health assessment can report failure before ZFS faults it.",
)
for node in member_names:
    report = reports.get(node)
    print(f"\n[{node}]")
    if report is None:
        print("  not inspected")
        continue
    if not report["smart"]:
        print("  no physical drives found")
        continue
    if all(result["rc"] == 127 for result in report["smart"].values()):
        print("  smartctl is not installed; drive health was not checked")
        continue
    index = disk_index(report)
    rows = []
    for disk, result in sorted(report["smart"].items()):
        device = index.get(disk) or {"name": disk}
        output = result["out"]
        match = re.search(r"self-assessment test result:\s*([A-Z]+)", output) or re.search(
            r"SMART Health Status:\s*(\S+)", output
        )
        verdict = match.group(1) if match else None
        failing = result["rc"] not in (124, 127) and result["rc"] & 8
        if result["rc"] == 124:
            health = "TIMED OUT"
            problems.append(f"{node}: smartctl timed out on {disk_label(device)}")
        elif verdict in ("PASSED", "OK") and not failing:
            health = verdict
        elif verdict or failing:
            health = verdict or "FAILING"
            problems.append(f"{node}: drive {disk_label(device)} SMART health is {health}")
        else:
            health = f"unavailable (smartctl exit {result['rc']})"
        if health not in ("PASSED", "OK") and not health.startswith("unavailable"):
            disk_problem_hosts.add(node)
        rows.append(
            [
                disk.removeprefix("/dev/"),
                (device.get("serial") or "?").strip(),
                (device.get("model") or "?").strip(),
                health,
            ]
        )
    table(["DRIVE", "SERIAL", "MODEL", "HEALTH"], rows)

# Verdict ----------------------------------------------------------------------
section("VERDICT", "whether any host, link, QDevice, pool, or drive needs attention.")
if not problems:
    print(f"OK: all {member_count} hosts, their corosync links, "
          f"{'the QDevice, ' if registered else ''}every ZFS pool and vdev member, "
          "every rpool's free space, and every drive are healthy.")
    raise SystemExit(0)
print(f"ATTENTION: {len(problems)} problem{'s' if len(problems) != 1 else ''} found:")
for problem in problems:
    print(f"  - {problem}")
print("\nNext steps:")
for node in sorted(disk_problem_hosts, key=lambda name: int(name.removeprefix("mox"))):
    print(f"  - {node}: diagnostics/show_proxmox_host_state.sh --host {node} shows every rpool")
    print("    member's disk serial; hosts/add_replacement_disk.sh replaces a pulled mirror member.")
for node in sorted(storage_hosts, key=lambda name: int(name.removeprefix("mox"))):
    print(f"  - {node}: add storage to rpool by installing two same-capacity disks and running")
    print(f"    hosts/add_new_disk_vdev.sh --host {node}.")
if any("QDevice" in problem or "qnetd" in problem or qdevice_host in problem
       for problem in problems):
    print("  - QDevice: diagnostics/show_qdevice_state.sh explains how to repair or replace it.")
print("  - For full cluster detail, run diagnostics/show_cluster_state.sh.")
raise SystemExit(1)
PY
}

main() {
  while (($#)); do
    case "$1" in
      -h | --help)
        usage
        exit 0
        ;;
      *)
        usage >&2
        exit 2
        ;;
    esac
  done
  command -v python3 >/dev/null 2>&1 || {
    printf 'ERROR: python3 is required on this workstation\n' >&2
    exit 2
  }
  load_proxmox_config --no-secrets >/dev/null || {
    printf 'ERROR: cluster configuration is invalid\n' >&2
    exit 2
  }
  RUN_DIR="$(mktemp -d "${TMPDIR:-/tmp}/show-cluster-health.XXXXXX")"
  chmod 0700 "$RUN_DIR"
  trap 'rm -rf -- "$RUN_DIR"' EXIT
  : >"${RUN_DIR}/members.tsv"

  printf 'show_cluster_health.sh - read-only hardware and network health for cluster %s\n' \
    "$PROXMOX_CLUSTER_NAME"
  printf 'Started: %s\n' "$(date +%Y-%m-%dT%H:%M:%S%z)"
  find_cluster
  local node
  for node in "${CONTROL_MEMBER_NODES[@]}"; do
    collect_host "$node"
  done
  probe_qdevice
  render_report
}

if [[ "${SHOW_CLUSTER_HEALTH_SOURCE_ONLY:-0}" != 1 ]]; then
  main "$@"
fi
