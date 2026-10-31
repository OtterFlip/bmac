#!/usr/bin/env python3

# Copyright (c) 2026 BEENTHERE VENTURES, INC.
# SPDX-License-Identifier: GPL-3.0-only

"""BMAC's monitor agent: one per Proxmox host, QDevice, and production VM,
and optionally one on the Proxmox Backup Server (role pbs).

Every monitored node runs this agent, bound to the node's own Healthchecks.io
check. It sends a plain success ping every minute, so a node that dies or
loses its internet connection goes silent and Healthchecks reports it DOWN
(the dead-man switch). When the agent finds a problem it can see (on its own
node, or a peer's link, or an app's ingress), it sends a pulse: a /fail ping
whose body describes every change (tokens bmacevent and bmacp1/bmacp2 on line
1), immediately followed by a clearing success ping (bmacclear) and a plain
heartbeat, so the check is up again at once and the dead-man switch keeps
working. Gmail filters on those tokens route the resulting emails.

The agent only checks its own node. It never logs in anywhere; it observes
peers through local tools (corosync, qnetd, tailscale) and network probes. A
host reads its own cluster's PBS namespace with the PVE storage's token, the
same read-only call the Backups page uses.

The pbs role runs on a PBS server shared by several clusters, installed by
hand (README.md, "Monitoring the PBS server"). It never reaches into any
cluster: it watches the pool, its member disks, the PBS services, the
datastore, PBS's own task results, and each namespace's newest backup.

  app_ha_monitor.py run                    the service loop (systemd)
  app_ha_monitor.py check --once [--json]  run every check once; send nothing
  app_ha_monitor.py status [--json]        show the agent's state
  app_ha_monitor.py test-pulse [--priority p1|p2]
                                           send one labelled test pulse
  app_ha_monitor.py probe --fqdn F --path P --status N [--text T] [--json]
                                           run one ingress probe; send nothing
  app_ha_monitor.py reset-smart-baseline SERIAL
                                           accept a drive's current counters
  app_ha_monitor.py validate-config        load and validate the config
  app_ha_monitor.py print-unit --role R --program PATH
                                           print the systemd unit

Standard library only; Python 3.11 or newer.
"""

from __future__ import annotations

import argparse
import concurrent.futures
import contextlib
import dataclasses
import datetime as dt
import fcntl
import http.client
import json
import os
from pathlib import Path
import re
import socket
import ssl
import subprocess
import sys
import threading
import time
from typing import Any, Callable, Iterator, Sequence
import urllib.error
import urllib.parse
import urllib.request


AGENT_VERSION = "2.1.1"
MIN_PYTHON = (3, 11)
CONFIG_SCHEMA = 1
STATE_SCHEMA = 1
GUEST_SETTINGS_SCHEMA = 1

DEFAULT_CONFIG = Path("/etc/app-ha-monitor/config.json")
DEFAULT_STATE_DIR = Path("/var/lib/app-ha-monitor")
DEFAULT_GUEST_SETTINGS = Path("/etc/app-ha-monitor/guest.json")
COROSYNC_CONF = (Path("/etc/corosync/corosync.conf"), Path("/etc/pve/corosync.conf"))
CLOUDFLARED_READY_URL = "http://127.0.0.1:20241/ready"
UNIT_NAME = "app-ha-monitor.service"
ROLES = ("host", "qdevice", "guest", "pbs")

# Gmail search ignores most punctuation, so tokens are single plain words.
TOKEN_EVENT = "bmacevent"
TOKEN_CLEAR = "bmacclear"
TOKEN_RE = re.compile(r"bmac(event|clear|p1|p2)", re.IGNORECASE)
PRIORITIES = ("p1", "p2")

BODY_LIMIT_BYTES = 8000
MESSAGE_LIMIT = 300
HTTP_TIMEOUT_SECONDS = 10
RETRY_WAITS = (2, 5, 10)
PULSE_MIN_SPACING_SECONDS = 60
PULSE_HOURLY_LIMIT = 12
SMART_TIMEOUT_SECONDS = 30
SERVICE_FAIL_THRESHOLD = 2
P1_FREE_PERCENT = 5
CERT_P1_DAYS = 3
NVME_SPARE_EARLY_MARGIN = 10
SMART_ATA_COUNTERS = ("5", "197", "198")
SKIPPED_FILESYSTEMS = {"tmpfs", "devtmpfs", "efivarfs", "overlay", "squashfs"}
LINK_NAMES = {0: "private VLAN", 1: "Tailscale"}
NVME_WARNING_BITS = {
    0: "available spare below threshold",
    1: "temperature out of range",
    2: "reliability degraded",
    3: "read-only mode",
    4: "volatile memory backup failed",
}

# The PBS API answers each host's backup check every five minutes.
DEFAULT_INTERVALS = {"alive": 60, "fast": 30, "slow": 1800, "probe": 60, "backup": 300}
# Backups (host role): the PVE storage and helper that Setup PBS installs.
BACKUP_STORAGE = "bmac-pbs"
PBS_API = Path(__file__).resolve().with_name("pbs_api.py")
PVE_STORAGE_CFG = Path("/etc/pve/storage.cfg")
PVE_JOBS_CFG = Path("/etc/pve/jobs.cfg")
# The pbs role.
PBS_SERVICES = ("proxmox-backup.service", "proxmox-backup-proxy.service")
PBS_TASK_LABELS = {
    "garbage_collection": "garbage collection",
    "prunejob": "prune job",
    "prune": "prune",
    "verificationjob": "verify job",
    "verify": "verify",
    "verify_group": "verify",
    "verify_snapshot": "verify",
}
PBS_TASK_LIMIT = 1000
# Raw counters watched against a baseline on the pool's ATA disks; Samsung
# SSDs lack 197 and 198, and a missing attribute does not apply.
PBS_SMART_COUNTERS = ("5", "179", "183", "187", "197", "198", "199")
PBS_DEFAULTS = {"namespace_max_age_hours": 8, "scrub_max_age_days": 40}
PBS_NAMESPACE_RE = re.compile(r"[A-Za-z0-9_][A-Za-z0-9._-]*(/[A-Za-z0-9_][A-Za-z0-9._-]*){0,7}")
DEFAULT_THRESHOLDS = {
    "fast_fail": 3,
    "service_fail": SERVICE_FAIL_THRESHOLD,
    "pool_min_free_percent": 10,
    "pool_clear_free_percent": 12,
    "postfix_queue_max_age_minutes": 60,
    "cert_warn_days": 14,
    "nvme_wear_warn_percent": 90,
    "reminder_hours": 6,
}


class ConfigError(Exception):
    """The agent config or a settings file is missing or invalid."""


class CheckError(Exception):
    """A check could not be performed; becomes an agent:<check> finding."""


def utc_iso(epoch: float) -> str:
    return dt.datetime.fromtimestamp(epoch, dt.timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")


def parse_iso(value: str | None) -> float | None:
    if not value:
        return None
    try:
        parsed = dt.datetime.fromisoformat(value.replace("Z", "+00:00"))
    except ValueError:
        return None
    if parsed.tzinfo is None:
        parsed = parsed.replace(tzinfo=dt.timezone.utc)
    return parsed.timestamp()


def log(message: str) -> None:
    print(f"app_ha_monitor: {message}", flush=True)


def first_line(text: str) -> str:
    return next((line.strip() for line in text.splitlines() if line.strip()), "")


def human_duration(seconds: float) -> str:
    seconds = max(0, int(seconds))
    if seconds < 120:
        return f"{seconds}s"
    minutes = seconds // 60
    if minutes < 120:
        return f"{minutes}m"
    hours, minutes = divmod(minutes, 60)
    if hours < 48:
        return f"{hours}h{minutes:02d}m"
    return f"{hours // 24}d{hours % 24}h"


def human_bytes(value: float) -> str:
    if value >= 2**40:
        return f"{value / 2**40:.1f} TiB"
    return f"{value / 2**30:.1f} GiB"


def sanitize(text: Any) -> str:
    """Neutralize marker tokens and flatten a message to one bounded line."""

    value = re.sub(r"[\x00-\x1f\x7f]+", " ", str(text))
    value = TOKEN_RE.sub(lambda match: "bmac-" + match.group(1).lower(), value)
    value = re.sub(r" {2,}", " ", value).strip()
    if len(value) > MESSAGE_LIMIT:
        value = value[: MESSAGE_LIMIT - 3].rstrip() + "..."
    return value


def redact_url(text: str) -> str:
    return re.sub(r"https?://[^\s\"']*", "<redacted-url>", text)


# Commands ---------------------------------------------------------------------


@dataclasses.dataclass
class Result:
    rc: int
    out: str
    err: str


def run(argv: Sequence[str], timeout: float = 30, stdin: str | None = None) -> Result:
    """Run one command with a timeout; never raises for command failures."""

    try:
        done = subprocess.run(
            list(argv),
            input=stdin,
            stdin=None if stdin is not None else subprocess.DEVNULL,
            capture_output=True,
            text=True,
            timeout=timeout,
            env=dict(os.environ, LC_ALL="C"),
        )
    except FileNotFoundError:
        return Result(127, "", f"{argv[0]} is not installed")
    except subprocess.TimeoutExpired:
        return Result(124, "", f"timed out after {timeout:g}s")
    except OSError as exc:
        return Result(126, "", str(exc))
    return Result(done.returncode, done.stdout, done.stderr.strip())


Runner = Callable[..., Result]


# Parsers (shared with show_cluster_health.sh) ---------------------------------


def status_value(text: str, key: str) -> str | None:
    match = re.search(rf"^\s*{re.escape(key)}:\s*(.*?)\s*$", text, re.M)
    return match.group(1) if match else None


def parse_key_values(text: str) -> dict[str, str]:
    """`Key:<spaces or tabs>value` lines; first occurrence wins."""

    values: dict[str, str] = {}
    for raw in text.splitlines():
        match = re.match(r"^\s*([^:]+):\s*(.*?)\s*$", raw)
        if match and match.group(2):
            values.setdefault(match.group(1).strip(), match.group(2))
    return values


def parse_links(text: str) -> dict[int, dict[str, Any]]:
    """corosync-cfgtool -n: per peer node ID, reachability and link states."""

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


ZPOOL_HEADER = re.compile(
    r"^\s*(pool|state|status|action|see|scan|remove|checkpoint|expand|config|errors):\s?(.*)$"
)


def parse_pool(text: str) -> tuple[dict[str, str], list[dict[str, Any]]]:
    """zpool status -P -p POOL: header fields and config entries with depth."""

    header: dict[str, str] = {}
    entries: list[dict[str, Any]] = []
    key, in_config = None, False
    for raw in text.splitlines():
        match = ZPOOL_HEADER.match(raw)
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
            entries.append(
                {
                    "name": tokens[0],
                    "depth": (len(body) - len(body.lstrip(" "))) // 2,
                    "state": tokens[1] if len(tokens) > 1 else None,
                    "counts": tokens[2:5] if len(tokens) >= 5 else None,
                    "note": " ".join(tokens[5:]),
                }
            )
        elif key and raw.startswith("\t") and raw.strip():
            header[key] += " " + raw.strip()
    return header, entries


def pool_problem_members(entries: list[dict[str, Any]]) -> list[str]:
    """Describe vdevs and members that are not ONLINE (spares may be AVAIL)."""

    problems, parents, group = [], [], None
    for position, entry in enumerate(entries):
        following = entries[position + 1] if position + 1 < len(entries) else None
        has_children = following is not None and following["depth"] > entry["depth"]
        depth = entry["depth"]
        del parents[depth:]
        parents.append(entry["name"])
        if depth == 0:
            group = None if entry["state"] else entry["name"]
            continue
        if entry["state"] is None:
            continue
        ok_states = {"AVAIL", "INUSE"} if group == "spares" else {"ONLINE"}
        if entry["state"] in ok_states:
            continue
        if has_children and entry["state"] == "DEGRADED":
            continue
        parent = parents[-2] if len(parents) >= 2 else ""
        problems.append(f"{parent}: {entry['name']} {entry['state']}" if parent else
                        f"{entry['name']} {entry['state']}")
    return problems


def parse_corosync_nodes(text: str) -> dict[str, int]:
    """corosync.conf nodelist: node name to node ID."""

    nodes: dict[str, int] = {}
    for block in re.findall(r"\bnode\s*\{([^{}]*)\}", text):
        name = re.search(r"^\s*name:\s*(\S+)\s*$", block, re.M)
        nodeid = re.search(r"^\s*nodeid:\s*(\d+)\s*$", block, re.M)
        if name and nodeid:
            nodes[name.group(1)] = int(nodeid.group(1))
    return nodes


def parse_quorum(text: str) -> dict[str, Any]:
    """corosync-quorumtool -s: the key lines and the membership table.

    The table has a Qdevice column only when a QDevice is configured, and the
    QDevice's own row has no value in it, so rows are split at the header's
    Name column rather than on whitespace.
    """

    head, _, table = text.partition("Membership information")
    values = parse_key_values(head)

    def integer(key: str) -> int | None:
        value = values.get(key, "")
        return int(value) if value.isdigit() else None

    members: list[dict[str, Any]] = []
    lines = table.splitlines()
    header_index = next((i for i, line in enumerate(lines) if "Nodeid" in line and "Name" in line), None)
    has_qdevice_column = False
    if header_index is not None:
        header = lines[header_index]
        name_column = header.index("Name")
        has_qdevice_column = "Qdevice" in header[:name_column]
        for line in lines[header_index + 1:]:
            if not line.strip() or len(line) <= name_column:
                continue
            prefix = line[:name_column].split()
            name = line[name_column:].strip()
            if len(prefix) < 2 or not prefix[0].isdigit() or not prefix[1].isdigit():
                continue
            local = name.endswith("(local)")
            name = name.removesuffix("(local)").strip()
            flags = prefix[2] if len(prefix) > 2 else ""
            members.append(
                {
                    "nodeid": int(prefix[0]),
                    "votes": int(prefix[1]),
                    "flags": flags.split(",") if flags else [],
                    "name": name,
                    "local": local,
                    "qdevice": name.startswith("Qdevice"),
                }
            )
    quorate = values.get("Quorate")
    return {
        "quorate": None if quorate is None else quorate == "Yes",
        "expected_votes": integer("Expected votes"),
        "total_votes": integer("Total votes"),
        "quorum": integer("Quorum"),
        "flags": values.get("Flags", ""),
        "has_qdevice_column": has_qdevice_column,
        "members": members,
    }


def parse_qnetd(text: str, cluster: str) -> dict[int, str] | None:
    """corosync-qnetd-tool -l: node ID to its Vote line, for one cluster.

    Returns None when the cluster is not listed at all.
    """

    votes: dict[int, str] = {}
    in_cluster, current = False, None
    for raw in text.splitlines():
        match = re.match(r'^Cluster "(.*)":\s*$', raw)
        if match:
            in_cluster = match.group(1) == cluster
            current = None
            continue
        if not in_cluster:
            continue
        match = re.match(r"^\s*Node ID (\d+):\s*$", raw)
        if match:
            current = int(match.group(1))
            votes[current] = ""
            continue
        match = re.match(r"^\s*Vote:\s*(.*?)\s*$", raw)
        if match and current is not None:
            votes[current] = match.group(1)
    if not any(re.match(rf'^Cluster "{re.escape(cluster)}":\s*$', line) for line in text.splitlines()):
        return None
    return votes


def parse_postqueue(text: str) -> list[dict[str, Any]]:
    messages = []
    for line in text.splitlines():
        line = line.strip()
        if not line:
            continue
        try:
            value = json.loads(line)
        except ValueError:
            continue
        if isinstance(value, dict):
            messages.append(value)
    return messages


def parse_used_available(text: str) -> tuple[int, int] | None:
    fields = text.split()
    if len(fields) != 2 or not all(field.isdigit() for field in fields):
        return None
    return int(fields[0]), int(fields[1])


def parse_df(text: str) -> list[dict[str, Any]]:
    """df -P -B1 -T: real (block device) filesystems only."""

    rows = []
    for line in text.splitlines()[1:]:
        fields = line.split(None, 6)
        if len(fields) != 7:
            continue
        source, fstype, size, used, available, _, mount = fields
        if not source.startswith("/dev/") or fstype in SKIPPED_FILESYSTEMS:
            continue
        if not (size.isdigit() and used.isdigit() and available.isdigit()):
            continue
        rows.append(
            {"source": source, "type": fstype, "size": int(size), "used": int(used),
             "available": int(available), "mount": mount}
        )
    return rows


def parse_enddate(text: str) -> float | None:
    """openssl x509 -enddate -noout: notAfter=Sep 16 23:49:00 2041 GMT."""

    match = re.search(r"notAfter=(.+?)\s*$", text, re.M)
    if not match:
        return None
    value = re.sub(r"\s+", " ", match.group(1).strip())
    try:
        parsed = dt.datetime.strptime(value, "%b %d %H:%M:%S %Y GMT")
    except ValueError:
        return None
    return parsed.replace(tzinfo=dt.timezone.utc).timestamp()


def decode_critical_warning(value: int) -> list[str]:
    return [label for bit, label in NVME_WARNING_BITS.items() if value & (1 << bit)] or [
        f"unknown warning bits {value:#x}"
    ]


def tailscale_hosts(status: dict[str, Any], tag: str) -> dict[str, dict[str, Any]]:
    """Tailscale peers carrying the host tag, by HostName."""

    peers = status.get("Peer") or {}
    found: dict[str, dict[str, Any]] = {}
    for peer in peers.values() if isinstance(peers, dict) else []:
        if not isinstance(peer, dict) or tag not in (peer.get("Tags") or []):
            continue
        name = peer.get("HostName")
        ipv4 = next((ip for ip in peer.get("TailscaleIPs") or [] if re.fullmatch(r"\d+\.\d+\.\d+\.\d+", ip)), None)
        if name:
            found[name] = {
                "ip": ipv4,
                "online": peer.get("Online"),
                "last_seen": peer.get("LastSeen"),
            }
    return found


# Findings and checks ----------------------------------------------------------


@dataclasses.dataclass
class Finding:
    """One evaluated problem key. ok=None holds the current state."""

    key: str
    ok: bool | None
    priority: str = "p2"
    message: str = ""
    kind: str = "once"  # fast | service | once


class Context:
    """What a check may use: config, read-only state, and injectable I/O."""

    def __init__(
        self,
        config: dict[str, Any],
        state: dict[str, Any] | None = None,
        *,
        runner: Runner = run,
        now: Callable[[], float] = time.time,
        http_get: Callable[[str, float], tuple[int, str]] | None = None,
        https_get: Callable[..., tuple[int, dict[str, str], bytes]] | None = None,
        read_text: Callable[[Path], str] | None = None,
        first_cycle: bool = False,
    ) -> None:
        self.config = config
        self.state = state or new_state()
        self.run = runner
        self.now = now
        self.http_get = http_get or default_http_get
        self.https_get = https_get or default_https_get
        self.read_text = read_text or (lambda path: path.read_text(encoding="utf-8"))
        self.first_cycle = first_cycle
        self.baselines = json.loads(json.dumps(self.state.get("smart_baselines") or {}))
        self.baselines_changed = False

    @property
    def node(self) -> str:
        return self.config["node"]

    def threshold(self, name: str) -> int:
        return int(self.config.get("thresholds", {}).get(name, DEFAULT_THRESHOLDS[name]))


def default_http_get(url: str, timeout: float) -> tuple[int, str]:
    request = urllib.request.Request(url, headers={"User-Agent": f"bmac-monitor/{AGENT_VERSION}"})
    try:
        with urllib.request.urlopen(request, timeout=timeout) as response:
            return response.status, response.read(65536).decode("utf-8", "replace")
    except urllib.error.HTTPError as exc:
        return exc.code, exc.read(65536).decode("utf-8", "replace")


def default_https_get(
    fqdn: str, path: str, timeout: float, headers: dict[str, str]
) -> tuple[int, dict[str, str], bytes]:
    """One HTTPS GET; redirects are not followed."""

    connection = http.client.HTTPSConnection(
        fqdn, 443, timeout=timeout, context=ssl.create_default_context()
    )
    try:
        connection.request("GET", path, headers=headers)
        response = connection.getresponse()
        body = response.read(65536)
        return response.status, {k.lower(): v for k, v in response.getheaders()}, body
    finally:
        connection.close()


def capacity_finding(
    ctx: Context, key: str, label: str, used: int, available: int, kind: str = "once"
) -> Finding:
    """Free-space hysteresis: open below the minimum, resolve above the clear level."""

    total = used + available
    free = 100.0 * available / total if total else 0.0
    minimum = ctx.threshold("pool_min_free_percent")
    clear = ctx.threshold("pool_clear_free_percent")
    priority = "p1" if free < P1_FREE_PERCENT else "p2"
    message = (
        f"{label} {free:.1f}% free ({human_bytes(available)} of {human_bytes(total)}), "
        f"below {minimum}%"
    )
    if free < minimum:
        ok: bool | None = False
    elif free > clear:
        ok, message = True, f"{label} {free:.1f}% free"
    else:
        ok = None
        message = f"{label} {free:.1f}% free (resolves above {clear}%)"
    return Finding(key, ok, priority, message, kind)


def check_links(ctx: Context) -> list[Finding]:
    peers = ctx.config["host"].get("expected_peers") or []
    if not peers:
        return []
    names: dict[str, int] = {}
    for path in COROSYNC_CONF:
        try:
            names = parse_corosync_nodes(ctx.read_text(path))
        except OSError:
            continue
        if names:
            break
    result = ctx.run(["corosync-cfgtool", "-n"], timeout=15)
    if result.rc != 0:
        raise CheckError(f"corosync-cfgtool -n failed: {first_line(result.err or result.out)}")
    view = parse_links(result.out)
    findings: list[Finding] = []
    for peer in peers:
        nodeid = names.get(peer)
        entry = view.get(nodeid) if nodeid is not None else None
        if entry is None:
            why = "is not in corosync.conf" if nodeid is None else "is not listed by corosync-cfgtool -n"
            findings += [
                Finding(f"corosync-peer:{peer}", False, "p1", f"corosync peer {peer} {why}", "fast"),
                Finding(f"link0:{peer}", None, kind="fast"),
                Finding(f"link1:{peer}", None, kind="fast"),
            ]
            continue
        findings.append(Finding(f"corosync-peer:{peer}", True, kind="fast"))
        links = entry["links"]
        up = {
            link: entry["reachable"] is not False and bool(links.get(link, False))
            for link in (0, 1)
        }
        priority = "p1" if not any(up.values()) else "p2"
        for link in (0, 1):
            message = f"corosync link{link} ({LINK_NAMES[link]}) to {peer} is " + (
                "up" if up[link] else "DOWN"
            )
            findings.append(Finding(f"link{link}:{peer}", up[link], priority, message, "fast"))
    return findings


def check_quorum(ctx: Context) -> list[Finding]:
    result = ctx.run(["corosync-quorumtool", "-s"], timeout=15)
    if "Quorate:" not in result.out:
        raise CheckError(f"corosync-quorumtool -s failed: {first_line(result.err or result.out)}")
    quorum = parse_quorum(result.out)
    expected, total = quorum["expected_votes"], quorum["total_votes"]
    votes_text = f"{total if total is not None else '?'} of {expected if expected is not None else '?'} expected votes"
    findings = [
        Finding(
            "quorum", quorum["quorate"] is True, "p1",
            f"the cluster is not quorate as seen from {ctx.node} ({votes_text}, quorum {quorum['quorum']})",
            "fast",
        )
    ]
    explained = 0
    if ctx.config["host"].get("qdevice_expected"):
        qdevice = next((m for m in quorum["members"] if m["qdevice"]), None)
        reasons = []
        if qdevice is None:
            reasons.append("the Qdevice is missing from the membership list")
            explained = 1
        elif qdevice["votes"] == 0:
            match = re.search(r"votes (\d+)", qdevice["name"])
            explained = int(match.group(1)) if match else 1
            reasons.append("the Qdevice contributes 0 votes")
        not_voting = [m["name"] for m in quorum["members"] if not m["qdevice"] and "NV" in m["flags"]]
        if not_voting:
            reasons.append("not voting on " + ", ".join(not_voting))
        message = "QDevice vote lost: " + "; ".join(reasons)
        if reasons:
            tool = ctx.run(["corosync-qdevice-tool", "-s"], timeout=15)
            state = parse_key_values(tool.out).get("State") if tool.rc == 0 else None
            message += f" (qdevice client state: {state or 'unknown'})"
        findings.append(Finding("qdevice-vote", not reasons, "p1", message, "fast"))
    else:
        findings.append(Finding("qdevice-vote", True, kind="fast"))
    if expected is not None and total is not None:
        missing = expected - total - explained
        findings.append(
            Finding("votes", missing <= 0, "p2",
                    f"{votes_text} present as seen from {ctx.node}", "fast")
        )
    return findings


def check_cloudflared(ctx: Context) -> list[Finding]:
    try:
        status, body = ctx.http_get(CLOUDFLARED_READY_URL, 5)
    except (OSError, http.client.HTTPException) as exc:
        reason = "connection refused" if isinstance(exc, ConnectionRefusedError) or "refused" in str(exc) else sanitize(exc)
        return [Finding("cloudflared", False, "p2",
                        f"this host's Cloudflare Tunnel replica is not ready ({reason})", "fast")]
    connections = None
    try:
        connections = int(json.loads(body).get("readyConnections"))
    except (ValueError, TypeError, AttributeError):
        pass
    if status != 200:
        return [Finding("cloudflared", False, "p2",
                        f"this host's Cloudflare Tunnel replica is not ready (HTTP {status})", "fast")]
    if not connections:
        return [Finding("cloudflared", False, "p2",
                        "this host's Cloudflare Tunnel replica has no connections to Cloudflare", "fast")]
    return [Finding("cloudflared", True, kind="fast")]


def check_postfix(ctx: Context) -> list[Finding]:
    result = ctx.run(["postqueue", "-j"], timeout=20)
    if result.rc != 0:
        raise CheckError(f"postqueue -j failed: {first_line(result.err or result.out)}")
    limit = ctx.threshold("postfix_queue_max_age_minutes") * 60
    now = ctx.now()
    old = [
        message for message in parse_postqueue(result.out)
        if isinstance(message.get("arrival_time"), (int, float)) and now - message["arrival_time"] > limit
    ]
    if not old:
        return [Finding("postfix-queue", True)]
    oldest = min(old, key=lambda message: message["arrival_time"])
    recipients = oldest.get("recipients") or [{}]
    address = str(recipients[0].get("address") or "")
    domain = address.rpartition("@")[2] or "unknown"
    reason = recipients[0].get("delay_reason") or "no reason given"
    return [
        Finding(
            "postfix-queue", False, "p2",
            f"{len(old)} message{'s' if len(old) != 1 else ''} older than "
            f"{limit // 60} minutes in the Postfix queue (oldest {human_duration(now - oldest['arrival_time'])}, "
            f"queue {oldest.get('queue_name', '?')}, to {domain}: {reason}); "
            "Proxmox's own alert email is not leaving this host",
        )
    ]


def zpool_names(ctx: Context) -> list[str]:
    result = ctx.run(["zpool", "list", "-H", "-o", "name"], timeout=30)
    if result.rc != 0:
        raise CheckError(f"zpool list failed: {first_line(result.err or result.out)}")
    return result.out.split()


def check_zfs(ctx: Context) -> list[Finding]:
    findings = []
    for pool in zpool_names(ctx):
        healthy = ctx.run(["zpool", "status", "-x", pool], timeout=30)
        if healthy.rc == 0 and "is healthy" in healthy.out:
            findings.append(Finding(f"zfs:{pool}", True))
            continue
        detail = ctx.run(["zpool", "status", "-P", "-p", pool], timeout=30)
        header, entries = parse_pool(detail.out)
        members = pool_problem_members(entries)
        message = f"pool {pool} is {header.get('state') or 'unhealthy'}"
        if members:
            message += " (" + ", ".join(members) + ")"
        elif header.get("status"):
            message += f": {header['status']}"
        elif healthy.out or healthy.err:
            message += f": {first_line(healthy.out or healthy.err)}"
        findings.append(Finding(f"zfs:{pool}", False, "p1", message))
    return findings


def check_capacity(ctx: Context) -> list[Finding]:
    findings = []
    for pool in zpool_names(ctx):
        result = ctx.run(["zfs", "list", "-H", "-p", "-o", "used,available", pool], timeout=30)
        space = parse_used_available(result.out) if result.rc == 0 else None
        if space is None:
            raise CheckError(f"could not read free space of pool {pool}: {first_line(result.err or result.out)}")
        findings.append(capacity_finding(ctx, f"capacity:{pool}", pool, *space))
    return findings


def check_smart(ctx: Context) -> list[Finding]:
    scan = ctx.run(["smartctl", "--scan", "-j"], timeout=30)
    try:
        devices = json.loads(scan.out).get("devices") or []
    except (ValueError, AttributeError):
        raise CheckError(f"smartctl --scan -j failed: {first_line(scan.err or scan.out)}")
    findings: list[Finding] = []
    seen: set[str] = set()
    all_read = True
    for device in devices:
        name = device.get("name")
        if not isinstance(name, str) or not name.startswith("/dev/"):
            continue
        argv = ["smartctl", "-j", "-i", "-H", "-A", name]
        if device.get("type"):
            argv[2:2] = ["-d", str(device["type"])]
        result = ctx.run(argv, timeout=SMART_TIMEOUT_SECONDS)
        try:
            data = json.loads(result.out) if result.out.strip() else {}
        except ValueError:
            data = {}
        serial = str(data.get("serial_number") or "").strip()
        model = str(data.get("model_name") or "").strip() or "drive"
        identity = serial or name
        if serial and serial in seen:
            continue
        if serial:
            seen.add(serial)
        label = f"{model} {serial or name}"
        if result.rc == 124 or result.rc == 127 or not data or result.rc & 0b110:
            all_read = False
            why = result.err or first_line(result.out) or f"exit status {result.rc}"
            if result.rc not in (124, 127) and result.rc & 0b110:
                why = "device open failed" if result.rc & 0b10 else "a SMART command failed"
            findings.append(Finding(f"smart-read:{identity}", False, "p2",
                                    f"smartctl could not read {label} ({name}): {sanitize(why)}"))
            continue
        findings.append(Finding(f"smart-read:{identity}", True))
        findings += smart_findings(ctx, serial or name, label, data, result.rc)
    if all_read:
        for serial in list(ctx.baselines):
            if serial not in seen:
                del ctx.baselines[serial]
                ctx.baselines_changed = True
    return findings


def smart_findings(
    ctx: Context, serial: str, label: str, data: dict[str, Any], rc: int
) -> list[Finding]:
    findings: list[Finding] = []
    health_problems: list[str] = []
    counter_problems: list[str] = []
    passed = (data.get("smart_status") or {}).get("passed")
    if passed is False or rc & 0b1000:
        health_problems.append("SMART overall health FAILED")
    baseline = ctx.baselines.get(serial)
    log_ = data.get("nvme_smart_health_information_log")
    if isinstance(log_, dict):
        warning = int(log_.get("critical_warning") or 0)
        findings.append(Finding(
            f"nvme-warning:{serial}", warning == 0, "p1",
            f"NVMe {label}: critical warning " + ", ".join(decode_critical_warning(warning)),
        ))
        spare, spare_threshold = log_.get("available_spare"), log_.get("available_spare_threshold")
        if isinstance(spare, int) and isinstance(spare_threshold, int):
            if spare <= spare_threshold:
                findings.append(Finding(f"nvme-spare:{serial}", False, "p1",
                                        f"NVMe {label}: available spare {spare}% is at or below its threshold {spare_threshold}%"))
            elif spare <= spare_threshold + NVME_SPARE_EARLY_MARGIN:
                findings.append(Finding(f"nvme-spare:{serial}", False, "p2",
                                        f"NVMe {label}: available spare {spare}% is within "
                                        f"{NVME_SPARE_EARLY_MARGIN} points of its threshold {spare_threshold}%"))
            else:
                findings.append(Finding(f"nvme-spare:{serial}", True))
        used = log_.get("percentage_used")
        if isinstance(used, int):
            warn = ctx.threshold("nvme_wear_warn_percent")
            written_tb = int(log_.get("data_units_written") or 0) * 512_000 / 1e12
            findings.append(Finding(
                f"nvme-wear:{serial}", used < warn, "p1" if used >= 100 else "p2",
                f"NVMe {label}: {used}% of rated endurance used ({written_tb:.1f} TB written, "
                f"{log_.get('power_on_hours', '?')} power-on hours); plan a replacement",
            ))
        media = log_.get("media_errors")
        if isinstance(media, int):
            if not isinstance(baseline, dict) or "media_errors" not in baseline:
                ctx.baselines[serial] = {"media_errors": media}
                ctx.baselines_changed = True
            elif media > baseline["media_errors"]:
                counter_problems.append(f"media errors rose from {baseline['media_errors']} to {media}")
            elif media < baseline["media_errors"]:
                ctx.baselines[serial] = {"media_errors": media}
                ctx.baselines_changed = True
    table = (data.get("ata_smart_attributes") or {}).get("table")
    if isinstance(table, list):
        current = {}
        for row in table:
            if str(row.get("id")) in SMART_ATA_COUNTERS:
                raw = (row.get("raw") or {}).get("value")
                if isinstance(raw, int):
                    current[str(row["id"])] = (raw, row.get("name") or f"attribute {row['id']}")
        if current:
            if not isinstance(baseline, dict) or not set(current) <= set(baseline):
                ctx.baselines[serial] = {key: value for key, (value, _) in current.items()}
                ctx.baselines_changed = True
            else:
                for key, (value, name) in sorted(current.items(), key=lambda item: int(item[0])):
                    if value > baseline[key]:
                        counter_problems.append(f"{name} rose from {baseline[key]} to {value}")
    problems = health_problems + counter_problems
    findings.append(Finding(
        f"smart:{serial}", not problems, "p1" if health_problems else "p2",
        f"drive {label}: " + "; ".join(problems) + ("" if not counter_problems else reset_hint(ctx, serial)),
    ))
    return findings


def reset_hint(ctx: Context, serial: str) -> str:
    if ctx.config["role"] == "pbs":
        return f" (stays open until app_ha_monitor.py reset-smart-baseline {serial})"
    return f" (stays open until show_monitoring_state.sh --reset-smart-baseline {ctx.node}:{serial})"


def ping(ctx: Context, address: str, wait: int) -> bool:
    return ctx.run(["ping", "-n", "-c", "1", "-W", str(wait), address], timeout=wait + 5).rc == 0


def check_tailscale(ctx: Context) -> list[Finding]:
    section = ctx.config["qdevice"]
    hosts = section.get("hosts") or []
    if not hosts:
        return []
    result = ctx.run(["tailscale", "status", "--json"], timeout=20)
    try:
        status = json.loads(result.out)
    except ValueError:
        raise CheckError(f"tailscale status --json failed: {first_line(result.err or result.out)}")
    peers = tailscale_hosts(status, section.get("host_tag") or "tag:proxmox-host")
    wait = 10 if ctx.first_cycle else 3
    targets = {host["name"]: peers.get(host.get("tailscale_hostname") or host["name"]) for host in hosts}
    with concurrent.futures.ThreadPoolExecutor(max(1, len(targets))) as pool:
        futures = {
            name: pool.submit(ping, ctx, peer["ip"], wait)
            for name, peer in targets.items() if peer and peer.get("ip")
        }
        reached = {name: future.result() for name, future in futures.items()}
    failed = [name for name in targets if not reached.get(name)]
    priority = "p1" if failed and len(failed) == len(targets) else "p2"
    findings = []
    for name, peer in targets.items():
        if reached.get(name):
            findings.append(Finding(f"tailscale:{name}", True, kind="fast"))
            continue
        if peer is None:
            message = f"{name} is not among the QDevice's Tailscale peers"
        else:
            context = f"tailscale online={str(peer.get('online')).lower()}"
            last_seen = peer.get("last_seen")
            if last_seen and not str(last_seen).startswith("0001-"):
                context += f", last seen {last_seen}"
            message = f"ICMP over Tailscale from the QDevice to {name} ({peer.get('ip') or 'no IPv4'}) fails ({context})"
        if priority == "p1" and len(targets) > 1:
            message += "; every host fails at once (QDevice-side Tailscale problem or site outage)"
        findings.append(Finding(f"tailscale:{name}", False, priority, message, "fast"))
    return findings


def check_qnetd(ctx: Context) -> list[Finding]:
    section = ctx.config["qdevice"]
    hosts = section.get("hosts") or []
    if not section.get("qnetd_expected"):
        return [Finding("qnetd:service", True, kind="fast")]
    priority = "p1" if section.get("vote_needed", True) else "p2"
    active = ctx.run(["systemctl", "is-active", "corosync-qnetd.service"], timeout=10).out.strip() or "unknown"
    tool = ctx.run(["corosync-qnetd-tool", "-l"], timeout=15)
    if active != "active" or tool.rc != 0:
        why = f"corosync-qnetd is {active}" if active != "active" else (
            f"corosync-qnetd-tool -l failed: {first_line(tool.err or tool.out)}")
        return [Finding("qnetd:service", False, priority, why, "fast")] + [
            Finding(f"qnetd:{host['name']}", None, kind="fast") for host in hosts
        ]
    cluster = ctx.config["cluster"]
    votes = parse_qnetd(tool.out, cluster)
    if votes is None:
        return [Finding("qnetd:service", False, priority,
                        f"corosync-qnetd does not list cluster {cluster}", "fast")] + [
            Finding(f"qnetd:{host['name']}", None, kind="fast") for host in hosts
        ]
    findings = [Finding("qnetd:service", True, kind="fast")]
    for host in hosts:
        nodeid = host.get("nodeid")
        vote = votes.get(nodeid) if isinstance(nodeid, int) else None
        if vote is None:
            findings.append(Finding(f"qnetd:{host['name']}", False, priority,
                                    f"{host['name']} is not connected to corosync-qnetd", "fast"))
        elif not vote.endswith("(ACK)"):
            findings.append(Finding(f"qnetd:{host['name']}", False, priority,
                                    f"corosync-qnetd's current vote for {host['name']} is not ACK ({vote})", "fast"))
        else:
            findings.append(Finding(f"qnetd:{host['name']}", True, kind="fast"))
    return findings


def run_probe(
    ctx: Context, fqdn: str, path: str, expected_status: int, expected_text: str | None
) -> dict[str, Any]:
    """One ingress probe through Cloudflare; never raises."""

    headers = {
        "User-Agent": f"bmac-monitor/{AGENT_VERSION}",
        "Cache-Control": "no-cache",
        "Accept": "*/*",
    }
    started = time.monotonic()
    try:
        status, response_headers, body = ctx.https_get(fqdn, path, HTTP_TIMEOUT_SECONDS, headers)
    except (OSError, ssl.SSLError, http.client.HTTPException) as exc:
        return {"fqdn": fqdn, "path": path, "ok": False, "challenged": False, "status": None,
                "message": f"https://{fqdn}{path} failed: {sanitize(exc) or type(exc).__name__}"}
    elapsed = round(time.monotonic() - started, 3)
    challenged = response_headers.get("cf-mitigated", "").lower() == "challenge"
    result = {"fqdn": fqdn, "path": path, "status": status, "challenged": challenged,
              "seconds": elapsed, "ok": False}
    if challenged:
        result["message"] = (f"https://{fqdn}{path} was answered with a Cloudflare challenge, so the "
                             "probe is inconclusive; put the health endpoint under /api/")
    elif status != expected_status:
        result["message"] = f"https://{fqdn}{path} returned HTTP {status}, expected {expected_status}"
    elif expected_text and expected_text not in body.decode("utf-8", "replace"):
        result["message"] = f"https://{fqdn}{path} returned HTTP {status} without the expected text"
    else:
        result["ok"] = True
        result["message"] = f"https://{fqdn}{path} returned HTTP {status}"
    return result


def check_ingress(ctx: Context) -> list[Finding]:
    probes = ctx.config["qdevice"].get("probes") or []
    if not probes:
        return []
    with concurrent.futures.ThreadPoolExecutor(min(8, len(probes))) as pool:
        results = list(pool.map(
            lambda probe: run_probe(ctx, probe["fqdn"], probe["path"],
                                    int(probe["expected_status"]), probe.get("expected_text")),
            probes,
        ))
    findings = []
    for probe, result in zip(probes, results):
        fqdn = probe["fqdn"]
        if result["challenged"]:
            findings.append(Finding(f"ingress:{fqdn}", None, "p1", kind="fast"))
            findings.append(Finding(f"ingress-challenged:{fqdn}", False, "p2", result["message"], "fast"))
            continue
        findings.append(Finding(f"ingress-challenged:{fqdn}", True, kind="fast"))
        message = result["message"] + (f" ({probe['vm']})" if probe.get("vm") else "")
        findings.append(Finding(f"ingress:{fqdn}", result["ok"], "p1", message, "fast"))
    return findings


def check_disk(ctx: Context) -> list[Finding]:
    result = ctx.run(["df", "-P", "-B1", "-T"], timeout=30)
    rows = parse_df(result.out)
    if not rows:
        raise CheckError(f"df -P -B1 -T failed: {first_line(result.err or result.out)}")
    return [
        capacity_finding(ctx, f"disk:{row['mount']}", f"filesystem {row['mount']}", row["used"], row["available"])
        for row in rows
    ]


def guest_settings(ctx: Context) -> dict[str, Any]:
    path = Path(ctx.config.get("guest", {}).get("settings_file") or DEFAULT_GUEST_SETTINGS)
    try:
        text = ctx.read_text(path)
    except FileNotFoundError:
        return {"schema_version": GUEST_SETTINGS_SCHEMA, "services": [], "cert_paths": []}
    except OSError as exc:
        raise CheckError(f"cannot read {path}: {exc}")
    try:
        return validate_guest_settings(json.loads(text))
    except (ValueError, ConfigError) as exc:
        raise CheckError(f"{path} is invalid: {exc}")


UNIT_RE = re.compile(r"^[A-Za-z0-9@._:-]+$")


def validate_guest_settings(value: Any) -> dict[str, Any]:
    if not isinstance(value, dict) or value.get("schema_version") != GUEST_SETTINGS_SCHEMA:
        raise ConfigError("unsupported guest settings schema")
    services = value.get("services", [])
    cert_paths = value.get("cert_paths", [])
    if not isinstance(services, list) or not all(isinstance(s, str) and UNIT_RE.fullmatch(s) for s in services):
        raise ConfigError("services must be a list of systemd unit names")
    if not isinstance(cert_paths, list) or not all(isinstance(p, str) and p.startswith("/") for p in cert_paths):
        raise ConfigError("cert_paths must be a list of absolute paths")
    return {"schema_version": GUEST_SETTINGS_SCHEMA, "services": services, "cert_paths": cert_paths}


def check_services(ctx: Context) -> list[Finding]:
    findings = []
    for unit in guest_settings(ctx)["services"]:
        state = ctx.run(["systemctl", "is-active", unit], timeout=10).out.strip() or "unknown"
        findings.append(Finding(f"service:{unit}", state == "active", "p1",
                                f"monitored service {unit} is {state}", "service"))
    return findings


def check_certs(ctx: Context) -> list[Finding]:
    warn_days = ctx.threshold("cert_warn_days")
    findings = []
    for path in guest_settings(ctx)["cert_paths"]:
        result = ctx.run(["openssl", "x509", "-enddate", "-noout", "-in", path], timeout=15)
        expires = parse_enddate(result.out) if result.rc == 0 else None
        if expires is None:
            findings.append(Finding(f"cert:{path}", False, "p2",
                                    f"certificate {path} cannot be read: {sanitize(first_line(result.err or result.out))}"))
            continue
        days = (expires - ctx.now()) / 86400
        when = utc_iso(expires)
        if days <= 0:
            findings.append(Finding(f"cert:{path}", False, "p1", f"certificate {path} expired at {when}"))
        elif days <= warn_days:
            findings.append(Finding(f"cert:{path}", False, "p1" if days <= CERT_P1_DAYS else "p2",
                                    f"certificate {path} expires in {days:.1f} days ({when})"))
        else:
            findings.append(Finding(f"cert:{path}", True))
    return findings


# Backups (host role) ----------------------------------------------------------


def pve_sections(text: str, kind: str) -> dict[str, dict[str, str]]:
    """The `KIND: ID` sections of a PVE section config such as storage.cfg or
    jobs.cfg; a flag without a value (`disable`) maps to ""."""

    sections: dict[str, dict[str, str]] = {}
    current: dict[str, str] | None = None
    for line in text.splitlines():
        if not line.strip() or line.lstrip().startswith("#"):
            continue
        if not line[0].isspace():
            header = re.fullmatch(r"(\S+):\s*(\S+)\s*", line)
            current = sections.setdefault(header.group(2), {}) if header and header.group(1) == kind else None
            continue
        if current is not None:
            key, _, value = line.strip().partition(" ")
            current[key] = value.strip()
    return sections


def backup_interval_hours(schedule: str | None) -> int | None:
    """Hours between runs of a BMAC schedule (`0/N:MM` or `HH:MM`), or None."""

    schedule = str(schedule or "").strip()
    match = re.fullmatch(r"0/([0-9]{1,2}):[0-5][0-9]", schedule)
    if match and int(match.group(1)) in (1, 2, 3, 4, 6, 8, 12):
        return int(match.group(1))
    if re.fullmatch(r"(?:[01]?[0-9]|2[0-3]):[0-5][0-9]", schedule):
        return 24
    return None


def check_backups(ctx: Context) -> list[Finding]:
    """pbs:storage, whether this host can use the cluster's PBS namespace, and
    backup:<prodN> for each scheduled VM this host runs, so a VM's age is
    reported once, by its owner."""

    try:
        storage = pve_sections(ctx.read_text(PVE_STORAGE_CFG), "pbs").get(BACKUP_STORAGE)
    except FileNotFoundError:
        storage = None
    if storage is None or "disable" in storage:
        return []
    try:
        jobs = pve_sections(ctx.read_text(PVE_JOBS_CFG), "vzdump")
    except FileNotFoundError:
        jobs = {}
    scheduled = []
    for job_id, job in sorted(jobs.items()):
        match = re.fullmatch(r"bmac-(prod[1-9][0-9]*)", job_id)
        vmid = job.get("vmid", "")
        if (not match or job.get("enabled", "1") == "0" or job.get("storage") != BACKUP_STORAGE
                or not re.fullmatch(r"[1-9][0-9]*", vmid)):
            continue
        try:
            ctx.read_text(Path(f"/etc/pve/nodes/{ctx.node}/qemu-server/{vmid}.conf"))
        except FileNotFoundError:
            continue
        scheduled.append((match.group(1), vmid, job_id, backup_interval_hours(job.get("schedule"))))

    result = ctx.run([sys.executable, "-I", str(PBS_API), "groups", "--storage", BACKUP_STORAGE], timeout=60)
    groups = None
    if result.rc == 0:
        try:
            groups = {row["group"]: row for row in json.loads(result.out)["groups"]}
        except (ValueError, KeyError, TypeError):
            pass
    if groups is None:
        why = first_line(result.err or result.out).removeprefix("pbs_api.py: ") or f"exit status {result.rc}"
        # Ages can't be read; keep their state until PBS answers again.
        return [Finding("pbs:storage", False, "p2",
                        f"this host cannot use PBS storage {BACKUP_STORAGE}: {sanitize(why)}", "fast"),
                *(Finding(f"backup:{name}", None) for name, *_ in scheduled)]
    findings = [Finding("pbs:storage", True, kind="fast")]
    now = ctx.now()
    for name, vmid, job_id, hours in scheduled:
        newest = (groups.get(f"vm/{vmid}") or {}).get("last_backup")
        if hours is None or not isinstance(newest, (int, float)) or isinstance(newest, bool):
            # A schedule BMAC didn't write, or no backup yet: nothing to compare.
            findings.append(Finding(f"backup:{name}", None))
            continue
        # Two missed runs, plus an hour for a slow one, as the Backups page does.
        limit = 2 * hours + 1
        findings.append(Finding(
            f"backup:{name}", now - newest <= limit * 3600, "p2",
            f"{name}'s newest backup is {human_duration(now - newest)} old, but job {job_id} backs "
            f"it up every {hours}h (VMID {vmid}; alert after {limit}h)",
        ))
    return findings


# PBS server (pbs role) --------------------------------------------------------


def check_pbs_services(ctx: Context) -> list[Finding]:
    findings = []
    for unit in PBS_SERVICES:
        state = ctx.run(["systemctl", "is-active", unit], timeout=10).out.strip() or "unknown"
        findings.append(Finding(f"service:{unit}", state == "active", "p1",
                                f"PBS service {unit} is {state}", "service"))
    return findings


def pool_status(ctx: Context, pool: str) -> tuple[dict[str, str], list[dict[str, Any]]]:
    result = ctx.run(["zpool", "status", "-P", pool], timeout=30)
    if result.rc != 0:
        raise CheckError(f"zpool status {pool} failed: {first_line(result.err or result.out)}")
    return parse_pool(result.out)


SCRUB_DONE_RE = re.compile(r"scrub repaired .* on ([A-Z][a-z]{2} [A-Z][a-z]{2} [0-9]{1,2} [0-9:]{8} [0-9]{4})$")


def check_scrub(ctx: Context) -> list[Finding]:
    pool = ctx.config["pbs"]["pool"]
    limit_days = ctx.config["pbs"]["scrub_max_age_days"]
    header, _ = pool_status(ctx, pool)
    scan = " ".join(header.get("scan", "").split())
    key = f"scrub:{pool}"
    if "in progress" in scan:
        return [Finding(key, None)]
    match = SCRUB_DONE_RE.search(scan)
    if match:
        # zpool prints local time, in the C locale's day and month names.
        finished = time.mktime(time.strptime(match.group(1), "%a %b %d %H:%M:%S %Y"))
        age = ctx.now() - finished
        message = f"pool {pool} was last scrubbed {human_duration(age)} ago"
    else:
        created = ctx.run(["zfs", "get", "-H", "-p", "-o", "value", "creation", pool], timeout=30)
        if created.rc != 0 or not created.out.strip().isdigit():
            raise CheckError(f"cannot read when pool {pool} was created: {first_line(created.err or created.out)}")
        age = ctx.now() - int(created.out.strip())
        message = (f"pool {pool} has no completed scrub on record (last scan: {scan or 'none'}); "
                   f"it was created {human_duration(age)} ago")
    return [Finding(key, age <= limit_days * 86400, "p2",
                    f"{message}, more than {limit_days} days; run zpool scrub {pool} and check "
                    "that the monthly scrub timer or cron job is enabled")]


def pool_member_disks(ctx: Context, pool: str) -> list[str]:
    """The whole disks under the pool's members, from the pool itself, so a
    VM's virtual OS disk is never included and nothing depends on device
    letters."""

    _, entries = pool_status(ctx, pool)
    disks: list[str] = []
    for member in (entry["name"] for entry in entries if entry["name"].startswith("/dev/")):
        parent = ctx.run(["lsblk", "-ndo", "PKNAME", member], timeout=10)
        name = first_line(parent.out) if parent.rc == 0 else ""
        disk = f"/dev/{name}" if name else member
        if disk not in disks:
            disks.append(disk)
    return disks


def read_smart(ctx: Context, disk: str) -> tuple[dict[str, Any] | None, str]:
    """smartctl's report with automatic device-type detection, then with
    -d sat, which disks passed through to a VM as SCSI LUNs need."""

    why = ""
    for argv in (["smartctl", "-j", "-i", "-H", "-A", disk], ["smartctl", "-j", "-d", "sat", "-i", "-H", "-A", disk]):
        result = ctx.run(argv, timeout=SMART_TIMEOUT_SECONDS)
        try:
            data = json.loads(result.out) if result.out.strip() else {}
        except ValueError:
            data = {}
        identified = isinstance(data, dict) and (
            isinstance((data.get("ata_smart_attributes") or {}).get("table"), list)
            or isinstance(data.get("nvme_smart_health_information_log"), dict))
        # Exit-status bits 0-2: bad command line, open failed, SMART command failed.
        if result.rc not in (124, 127) and not result.rc & 0b111 and identified:
            return data, ""
        if result.rc in (124, 127):
            why = result.err or f"exit status {result.rc}"
        elif result.rc & 0b111:
            why = "device open failed" if result.rc & 0b10 else "a SMART command failed"
        else:
            why = "no SMART attributes in the report"
    return None, why


def pbs_smart_findings(ctx: Context, serial: str, label: str, data: dict[str, Any]) -> list[Finding]:
    """Health from attributes, never from the overall-health bit, which LUN
    passthrough cannot carry (smartctl's PASSED is then only an attribute
    check)."""

    if isinstance(data.get("nvme_smart_health_information_log"), dict):
        return smart_findings(ctx, serial, label, data, 0)
    rows = {str(row.get("id")): row for row in data["ata_smart_attributes"]["table"] if isinstance(row, dict)}
    failing = []
    for row in rows.values():
        value, threshold = row.get("value"), row.get("thresh")
        if isinstance(value, int) and isinstance(threshold, int) and threshold > 0 and value <= threshold:
            failing.append(f"{row.get('name') or 'attribute ' + str(row.get('id'))} is {value}, "
                           f"at or below its failure threshold {threshold}")
    current = {}
    for key in PBS_SMART_COUNTERS:
        raw = (rows.get(key, {}).get("raw") or {}).get("value")
        if isinstance(raw, int):
            current[key] = (raw, rows[key].get("name") or f"attribute {key}")
    counter_problems = []
    baseline = ctx.baselines.get(serial)
    if current:
        if not isinstance(baseline, dict) or not set(current) <= set(baseline):
            ctx.baselines[serial] = {key: value for key, (value, _) in current.items()}
            ctx.baselines_changed = True
        else:
            for key, (value, name) in sorted(current.items(), key=lambda item: int(item[0])):
                if value > baseline[key]:
                    counter_problems.append(f"{name} rose from {baseline[key]} to {value}")
    problems = failing + counter_problems
    findings = [Finding(f"smart:{serial}", not problems, "p1" if failing else "p2",
                        f"drive {label}: " + "; ".join(problems)
                        + ("" if not counter_problems else reset_hint(ctx, serial)))]

    warn = ctx.threshold("nvme_wear_warn_percent")
    wear = []
    level = rows.get("177", {}).get("value")
    if isinstance(level, int) and 100 - level >= warn:
        wear.append(f"Wear_Leveling_Count is {level} ({100 - level}% of its rated wear used)")
    rating = ctx.config["pbs"].get("tbw_rating_tb")
    lbas = (rows.get("241", {}).get("raw") or {}).get("value")
    if rating and isinstance(lbas, int):
        written_tb = lbas * int(data.get("logical_block_size") or 512) / 1e12
        if written_tb >= rating * warn / 100:
            wear.append(f"{written_tb:.0f} TB written of its {rating} TB rating")
    findings.append(Finding(f"smart-wear:{serial}", not wear, "p2",
                            f"drive {label}: " + "; ".join(wear) + "; plan a replacement"))
    return findings


def check_pool_smart(ctx: Context) -> list[Finding]:
    findings: list[Finding] = []
    seen: set[str] = set()
    all_read = True
    for disk in pool_member_disks(ctx, ctx.config["pbs"]["pool"]):
        data, why = read_smart(ctx, disk)
        serial = str((data or {}).get("serial_number") or "").strip()
        if data is None or not serial:
            all_read = False
            findings.append(Finding(f"smart-read:{disk}", False, "p2",
                                    f"smartctl could not read pool disk {disk}: {sanitize(why or 'no serial number')} "
                                    "(in a VM, check the LUN passthrough's rawio and the host's AppArmor rule)"))
            continue
        if serial in seen:
            continue
        seen.add(serial)
        findings.append(Finding(f"smart-read:{disk}", True))
        label = f"{str(data.get('model_name') or 'drive').strip()} {serial}"
        findings += pbs_smart_findings(ctx, serial, label, data)
    if all_read:
        for serial in list(ctx.baselines):
            if serial not in seen:
                del ctx.baselines[serial]
                ctx.baselines_changed = True
    return findings


def datastore_path(ctx: Context, name: str) -> str | None:
    result = ctx.run(["proxmox-backup-manager", "datastore", "list", "--output-format", "json"], timeout=30)
    try:
        rows = json.loads(result.out) if result.rc == 0 else None
    except ValueError:
        rows = None
    if not isinstance(rows, list):
        raise CheckError(f"proxmox-backup-manager datastore list failed: {first_line(result.err or result.out)}")
    for row in rows:
        if isinstance(row, dict) and row.get("name") == name and isinstance(row.get("path"), str):
            return row["path"]
    return None


def newest_snapshot(directory: Path) -> float | None:
    """The newest complete snapshot under a namespace's directory: its
    <type>/<id>/<time> folders, complete once the manifest index.json.blob
    is written. Child namespaces (ns/) are not included."""

    newest = None
    for kind in ("vm", "ct", "host"):
        try:
            groups = [entry.path for entry in os.scandir(directory / kind) if entry.is_dir()]
        except (FileNotFoundError, NotADirectoryError):
            continue
        for group in groups:
            for snapshot in sorted(os.listdir(group), reverse=True):
                when = parse_iso(snapshot) if re.fullmatch(r"[0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9:]{8}Z", snapshot) else None
                if when is None or (newest is not None and when <= newest):
                    continue
                if os.path.exists(os.path.join(group, snapshot, "index.json.blob")):
                    newest = when
                    break
    return newest


def check_datastore(ctx: Context) -> list[Finding]:
    """datastore:<name>, mounted and initialized, and namespace:<NS>, whose
    newest backup must be recent; read from the directory tree, so no
    credentials are needed."""

    section = ctx.config["pbs"]
    name = section["datastore"]
    path = datastore_path(ctx, name)
    namespaces = section["namespaces"]
    problem = None
    if path is None:
        problem = f"PBS has no datastore named {name}"
    else:
        mount = ctx.run(["findmnt", "-n", "-o", "TARGET", "--target", path], timeout=10)
        if mount.rc != 0 or first_line(mount.out) == "/":
            problem = f"datastore {name}'s path {path} is not on a mounted pool (is pool {section['pool']} imported?)"
        elif not os.path.isdir(os.path.join(path, ".chunks")):
            problem = f"datastore {name}'s path {path} has no .chunks directory"
    if problem:
        # Namespace ages can't be read; keep their state.
        return [Finding(f"datastore:{name}", False, "p1", problem),
                *(Finding(f"namespace:{ns['name']}", None) for ns in namespaces)]
    findings = [Finding(f"datastore:{name}", True)]
    now = ctx.now()
    for ns in namespaces:
        directory = Path(path, "ns", *"/ns/".join(ns["name"].split("/")).split("/"))
        key, limit = f"namespace:{ns['name']}", ns["max_age_hours"]
        if not directory.is_dir():
            findings.append(Finding(key, False, "p2", f"namespace {ns['name']} does not exist in datastore {name}"))
            continue
        newest = newest_snapshot(directory)
        if newest is None:
            findings.append(Finding(key, False, "p2", f"namespace {ns['name']} has no complete backup"))
            continue
        findings.append(Finding(key, now - newest <= limit * 3600, "p2",
                                f"namespace {ns['name']}'s newest backup is {human_duration(now - newest)} old "
                                f"(limit {limit}h); check that cluster's backup jobs"))
    return findings


def check_pbs_tasks(ctx: Context) -> list[Finding]:
    """pbs-task:<type>:<worker>: the newest finished garbage collection,
    prune, or verify task of each job; a later success resolves it."""

    result = ctx.run(["proxmox-backup-manager", "task", "list", "--all", "--limit", str(PBS_TASK_LIMIT),
                      "--output-format", "json"], timeout=60)
    try:
        tasks = json.loads(result.out) if result.rc == 0 else None
    except ValueError:
        tasks = None
    if not isinstance(tasks, list):
        raise CheckError(f"proxmox-backup-manager task list failed: {first_line(result.err or result.out)}")
    latest: dict[str, dict[str, Any]] = {}
    for task in tasks:
        if not (isinstance(task, dict) and task.get("worker_type") in PBS_TASK_LABELS
                and isinstance(task.get("endtime"), int) and task.get("status")):
            continue
        key = f"pbs-task:{task['worker_type']}:{task.get('worker_id') or '-'}"
        if key not in latest or task["endtime"] > latest[key]["endtime"]:
            latest[key] = task
    findings = []
    for key, task in sorted(latest.items()):
        status = str(task["status"])
        what = f"{PBS_TASK_LABELS[task['worker_type']]} {task.get('worker_id') or ''}".strip()
        findings.append(Finding(key, status == "OK", "p2",
                                f"PBS {what} ended {utc_iso(task['endtime'])} with: {status} "
                                "(its log is under Administration > Tasks in the PBS web UI)"))
    # A job whose newest run has left the listing keeps its state until it runs again.
    for key in ctx.state.get("problems", {}):
        if key.startswith("pbs-task:") and key not in latest:
            findings.append(Finding(key, None))
    return findings


@dataclasses.dataclass(frozen=True)
class Check:
    name: str
    role: str
    speed: str  # fast | slow | probe | backup
    prefixes: tuple[str, ...]
    func: Callable[[Context], list[Finding]]

    def owns(self, key: str) -> bool:
        return any(key == prefix or key.startswith(prefix + ":") for prefix in self.prefixes)


CHECKS = (
    Check("links", "host", "fast", ("link0", "link1", "corosync-peer"), check_links),
    Check("quorum", "host", "fast", ("quorum", "votes", "qdevice-vote"), check_quorum),
    Check("cloudflared", "host", "fast", ("cloudflared",), check_cloudflared),
    Check("postfix", "host", "fast", ("postfix-queue",), check_postfix),
    Check("zfs", "host", "slow", ("zfs",), check_zfs),
    Check("capacity", "host", "slow", ("capacity",), check_capacity),
    Check("smart", "host", "slow",
          ("smart", "smart-read", "nvme-warning", "nvme-spare", "nvme-wear"), check_smart),
    Check("backups", "host", "backup", ("pbs", "backup"), check_backups),
    Check("tailscale", "qdevice", "fast", ("tailscale",), check_tailscale),
    Check("qnetd", "qdevice", "fast", ("qnetd",), check_qnetd),
    Check("ingress", "qdevice", "probe", ("ingress", "ingress-challenged"), check_ingress),
    Check("disk", "qdevice", "slow", ("disk",), check_disk),
    Check("services", "guest", "fast", ("service",), check_services),
    Check("disk", "guest", "slow", ("disk",), check_disk),
    Check("certs", "guest", "slow", ("cert",), check_certs),
    Check("services", "pbs", "fast", ("service",), check_pbs_services),
    Check("zfs", "pbs", "slow", ("zfs",), check_zfs),
    Check("capacity", "pbs", "slow", ("capacity",), check_capacity),
    Check("scrub", "pbs", "slow", ("scrub",), check_scrub),
    Check("smart", "pbs", "slow", ("smart", "smart-read", "smart-wear", "nvme-warning", "nvme-spare", "nvme-wear"),
          check_pool_smart),
    Check("datastore", "pbs", "slow", ("datastore", "namespace"), check_datastore),
    Check("tasks", "pbs", "slow", ("pbs-task",), check_pbs_tasks),
)


def checks_for(role: str) -> list[Check]:
    return [check for check in CHECKS if check.role == role]


def run_check(check: Check, ctx: Context) -> tuple[list[Finding], str | None]:
    """Run one check; an exception becomes an error string, not a crash."""

    try:
        return check.func(ctx), None
    except CheckError as exc:
        return [], sanitize(exc)
    except Exception as exc:  # noqa: BLE001 - a broken check must not stop the agent
        return [], sanitize(f"{type(exc).__name__}: {exc}")


# Config and state -------------------------------------------------------------


def require_python() -> None:
    if sys.version_info < MIN_PYTHON:
        raise ConfigError(
            f"python3 {sys.version_info[0]}.{sys.version_info[1]} is older than "
            f"{MIN_PYTHON[0]}.{MIN_PYTHON[1]}"
        )


def validate_ping_url(value: Any) -> str:
    if not isinstance(value, str):
        raise ConfigError("ping_url must be a string")
    parts = urllib.parse.urlsplit(value)
    local = parts.scheme == "http" and parts.hostname in {"127.0.0.1", "localhost"}
    if not (parts.scheme == "https" or local) or not parts.hostname or parts.query or parts.fragment:
        raise ConfigError("ping_url must be an https URL")
    return value.rstrip("/")


def validate_config(value: Any) -> dict[str, Any]:
    if not isinstance(value, dict) or value.get("schema_version") != CONFIG_SCHEMA:
        raise ConfigError("unsupported config schema")
    for key in ("cluster", "node", "role"):
        if not isinstance(value.get(key), str) or not value[key]:
            raise ConfigError(f"{key} is missing")
    if value["role"] not in ROLES:
        raise ConfigError(f"unknown role {value['role']!r}")
    value["ping_url"] = validate_ping_url(value.get("ping_url"))
    intervals = {**DEFAULT_INTERVALS, **(value.get("intervals") or {})}
    thresholds = {**DEFAULT_THRESHOLDS, **(value.get("thresholds") or {})}
    for name, number in (*intervals.items(), *thresholds.items()):
        if not isinstance(number, int) or isinstance(number, bool) or number < 1:
            raise ConfigError(f"{name} must be a positive integer")
    if intervals["alive"] < 60 or intervals["fast"] < 10:
        raise ConfigError("intervals are too short")
    if thresholds["pool_clear_free_percent"] <= thresholds["pool_min_free_percent"]:
        raise ConfigError("pool_clear_free_percent must exceed pool_min_free_percent")
    value["intervals"], value["thresholds"] = intervals, thresholds
    section = value.get(value["role"])
    if not isinstance(section, dict):
        raise ConfigError(f"the {value['role']} section is missing")
    if value["role"] == "host":
        peers = section.get("expected_peers", [])
        if not isinstance(peers, list) or not all(isinstance(p, str) and p for p in peers):
            raise ConfigError("host.expected_peers must be a list of names")
    elif value["role"] == "qdevice":
        for host in section.get("hosts", []):
            if not isinstance(host, dict) or not isinstance(host.get("name"), str):
                raise ConfigError("qdevice.hosts entries need a name")
        for probe in section.get("probes", []):
            if not (isinstance(probe, dict) and isinstance(probe.get("fqdn"), str)
                    and isinstance(probe.get("path"), str) and probe["path"].startswith("/")
                    and isinstance(probe.get("expected_status"), int)):
                raise ConfigError("qdevice.probes entries need fqdn, path, and expected_status")
    elif value["role"] == "pbs":
        value["pbs"] = validate_pbs_section(section)
    return value


def validate_pbs_section(section: dict[str, Any]) -> dict[str, Any]:
    result = {**PBS_DEFAULTS, **section}
    for key in ("pool", "datastore"):
        if not isinstance(result.get(key), str) or not re.fullmatch(r"[A-Za-z0-9][A-Za-z0-9._-]*", result[key]):
            raise ConfigError(f"pbs.{key} must be a ZFS pool or datastore name")
    for key in ("namespace_max_age_hours", "scrub_max_age_days"):
        number = result[key]
        if not isinstance(number, int) or isinstance(number, bool) or number < 1:
            raise ConfigError(f"pbs.{key} must be a positive integer")
    rating = result.get("tbw_rating_tb")
    if rating is not None and (not isinstance(rating, int) or isinstance(rating, bool) or rating < 1):
        raise ConfigError("pbs.tbw_rating_tb must be a positive integer (terabytes)")
    if not isinstance(result.get("namespaces", []), list):
        raise ConfigError("pbs.namespaces must be a list")
    namespaces = []
    for entry in result.get("namespaces", []):
        entry = {"name": entry} if isinstance(entry, str) else entry
        hours = entry.get("max_age_hours", result["namespace_max_age_hours"]) if isinstance(entry, dict) else None
        if not (isinstance(entry, dict) and isinstance(entry.get("name"), str)
                and PBS_NAMESPACE_RE.fullmatch(entry["name"])
                and isinstance(hours, int) and not isinstance(hours, bool) and hours >= 1):
            raise ConfigError("pbs.namespaces entries need a namespace name and an optional positive max_age_hours")
        namespaces.append({"name": entry["name"], "max_age_hours": hours})
    result["namespaces"] = namespaces
    return result


def load_config(path: Path) -> dict[str, Any]:
    try:
        return validate_config(json.loads(path.read_text(encoding="utf-8")))
    except OSError as exc:
        raise ConfigError(f"cannot read {path}: {exc.strerror or exc}")
    except ValueError as exc:
        raise ConfigError(f"{path} is not valid JSON: {exc}")


def new_state() -> dict[str, Any]:
    return {
        "schema_version": STATE_SCHEMA,
        "agent_version": AGENT_VERSION,
        "problems": {},
        "debounce": {},
        "smart_baselines": {},
        "outbox": [],
        "pending_clear": False,
        "pulse_times": [],
        "rate_limited": False,
        "last_heartbeat_ok": None,
        "last_results": {},
        "check_errors": {},
    }


def write_json_atomic(path: Path, value: Any) -> None:
    path.parent.mkdir(parents=True, exist_ok=True, mode=0o700)
    temporary = path.with_name(f".{path.name}.{os.getpid()}.tmp")
    descriptor = os.open(temporary, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o600)
    with os.fdopen(descriptor, "w", encoding="utf-8") as handle:
        handle.write(json.dumps(value, indent=2, sort_keys=True) + "\n")
        handle.flush()
        os.fsync(handle.fileno())
    os.replace(temporary, path)


def load_state(path: Path, now: float) -> tuple[dict[str, Any], bool]:
    """The saved state, and whether this is a first start."""

    try:
        text = path.read_text(encoding="utf-8")
    except FileNotFoundError:
        return new_state(), True
    try:
        state = json.loads(text)
        if not isinstance(state, dict) or state.get("schema_version") != STATE_SCHEMA:
            raise ValueError("incompatible schema")
        if not isinstance(state.get("problems"), dict) or not isinstance(state.get("outbox"), list):
            raise ValueError("malformed")
    except ValueError:
        aside = path.with_name(f"{path.name}.corrupt-{time.strftime('%Y%m%dT%H%M%SZ', time.gmtime(now))}")
        os.replace(path, aside)
        log(f"moved an unreadable state file aside to {aside.name}; starting fresh")
        return new_state(), True
    merged = {**new_state(), **state}
    merged["agent_version"] = AGENT_VERSION
    return merged, False


# Pulse bodies -----------------------------------------------------------------

SECTION_ORDER = ("NEW", "REMINDER", "RESOLVED", "ONGOING")


def item_line(item: dict[str, Any]) -> str:
    return f"  [{item['priority']}] {sanitize(item['key']):<18} {sanitize(item['message'])}"


def compose_event_body(
    config: dict[str, Any],
    outbox: list[dict[str, Any]],
    ongoing: list[dict[str, Any]],
    now: float,
    pulse_id: str,
    rate_limited: bool = False,
) -> tuple[str, list[dict[str, Any]]]:
    """The /fail body and the outbox items it actually carries."""

    announce = [item for item in outbox if item["section"] in ("NEW", "REMINDER")]
    priority = "p1" if any(item["priority"] == "p1" for item in announce) else "p2"
    header = [
        f"{TOKEN_EVENT} bmac{priority}",
        sanitize(f"BMAC event cluster={config['cluster']} node={config['node']} "
                 f"role={config['role']} pulse={pulse_id}"),
        f"time={utc_iso(now)} agent={AGENT_VERSION}",
    ]
    if rate_limited:
        header.append("pulse rate limit reached; some changes were merged")
    sections = {name: [] for name in SECTION_ORDER}
    for item in outbox:
        sections[item["section"]].append(item)
    sections["ONGOING"] = list(ongoing)
    # Drop from the end of the least important sections until it fits.
    included = {name: list(items) for name, items in sections.items()}
    omitted = 0

    def render() -> str:
        lines = list(header)
        for name in SECTION_ORDER:
            if included[name]:
                lines.append("")
                lines.append(name)
                lines += [item_line(item) for item in included[name]]
        if omitted:
            lines.append("")
            lines.append(f"({omitted} more items omitted; see show_monitoring_state.sh)")
        return "\n".join(lines) + "\n"

    body = render()
    for name in ("ONGOING", "RESOLVED", "REMINDER", "NEW"):
        while len(body.encode("utf-8")) > BODY_LIMIT_BYTES and included[name]:
            included[name].pop()
            omitted += 1
            body = render()
    carried = [item for name in ("NEW", "REMINDER", "RESOLVED") for item in included[name]]
    # A pulse must announce something; with only ONGOING left it would be noise.
    if not carried:
        return "", []
    if not any(item["section"] in ("NEW", "REMINDER") for item in carried):
        body = body.replace(f"{TOKEN_EVENT} bmac{priority}", f"{TOKEN_EVENT} bmacp2", 1)
    elif priority == "p1" and not any(item["priority"] == "p1" and item["section"] in ("NEW", "REMINDER")
                                      for item in carried):
        body = body.replace(f"{TOKEN_EVENT} bmacp1", f"{TOKEN_EVENT} bmacp2", 1)
    return body, carried


def compose_clear_body(config: dict[str, Any], pulse_id: str) -> str:
    return (f"{TOKEN_CLEAR}\n" + sanitize(
        f"BMAC clear cluster={config['cluster']} node={config['node']} pulse={pulse_id}") + "\n")


def compose_heartbeat_body(config: dict[str, Any], now: float, open_problems: int) -> str:
    return (
        sanitize(f"BMAC heartbeat cluster={config['cluster']} node={config['node']} role={config['role']}")
        + f"\ntime={utc_iso(now)} agent={AGENT_VERSION} open_problems={open_problems}\n"
    )


# Sending ----------------------------------------------------------------------


def http_post(url: str, body: str, timeout: float) -> int:
    request = urllib.request.Request(
        url, data=body.encode("utf-8"), method="POST",
        headers={"Content-Type": "text/plain; charset=utf-8",
                 "User-Agent": f"bmac-monitor/{AGENT_VERSION}"},
    )
    try:
        with urllib.request.urlopen(request, timeout=timeout) as response:
            response.read(1024)
            return response.status
    except urllib.error.HTTPError as exc:
        return exc.code


class Sender:
    """POSTs pings with retries. Never logs the ping URL."""

    def __init__(
        self,
        post: Callable[[str, str, float], int] = http_post,
        sleep: Callable[[float], None] | None = None,
        dry_run: bool = False,
    ) -> None:
        self.post = post
        self.sleep = sleep or (lambda seconds: time.sleep(seconds))
        self.dry_run = dry_run

    def send(self, ping_url: str, suffix: str, body: str, what: str) -> bool:
        if self.dry_run:
            log(f"dry run: would send {what} ping=<redacted>{suffix} ({len(body.encode())} bytes)")
            for line in body.splitlines():
                log(f"dry run:   {line}")
            return True
        url = ping_url + suffix
        for attempt, wait in enumerate((0, *RETRY_WAITS)):
            if wait:
                self.sleep(wait)
            try:
                status = self.post(url, body, HTTP_TIMEOUT_SECONDS)
            except (OSError, http.client.HTTPException, ValueError) as exc:
                reason = sanitize(redact_url(str(exc))) or type(exc).__name__
            else:
                if status == 200:
                    return True
                reason = f"HTTP {status}"
            log(f"{what} attempt {attempt + 1} failed ({reason}) ping=<redacted>{suffix}")
        return False


@contextlib.contextmanager
def send_lock(state_dir: Path) -> Iterator[None]:
    """Serialize ping sequences across processes (the service and test-pulse)."""

    state_dir.mkdir(parents=True, exist_ok=True, mode=0o700)
    descriptor = os.open(state_dir / "send.lock", os.O_RDWR | os.O_CREAT, 0o600)
    try:
        fcntl.flock(descriptor, fcntl.LOCK_EX)
        yield
    finally:
        fcntl.flock(descriptor, fcntl.LOCK_UN)
        os.close(descriptor)


# The agent --------------------------------------------------------------------


def is_staging(hostname: str) -> bool:
    return hostname.split(".")[0].lower().startswith("stage")


class Agent:
    """Debounce, problem state, reminders, pulses, and heartbeats for one node."""

    def __init__(
        self,
        config_path: Path = DEFAULT_CONFIG,
        state_dir: Path = DEFAULT_STATE_DIR,
        *,
        runner: Runner = run,
        clock: Callable[[], float] = time.time,
        sender: Sender | None = None,
        hostname: Callable[[], str] = socket.gethostname,
        context_options: dict[str, Any] | None = None,
    ) -> None:
        self.config_path = config_path
        self.state_dir = state_dir
        self.state_path = state_dir / "state.json"
        self.runner = runner
        self.clock = clock
        self.sender = sender or Sender()
        self.hostname = hostname
        self.context_options = context_options or {}
        self.lock = threading.RLock()
        self.config = load_config(config_path)
        self.state, self.first_start = load_state(self.state_path, clock())
        self.last_run: dict[str, float] = {}
        self.cycles = 0
        self.force_slow = False

    # -- config ----------------------------------------------------------------

    def reload_config(self) -> None:
        try:
            config = load_config(self.config_path)
        except ConfigError as exc:
            log(f"keeping the previous config: {exc}")
            return
        if config.get("ping_url") != self.config.get("ping_url"):
            log("the check's ping URL changed (ping=<redacted>)")
        self.config = config

    def save(self) -> None:
        with self.lock:
            write_json_atomic(self.state_path, self.state)

    # -- checks ----------------------------------------------------------------

    def interval(self, check: Check) -> int:
        return int(self.config["intervals"][check.speed])

    def due_checks(self, now: float) -> list[Check]:
        due = []
        for check in checks_for(self.config["role"]):
            last = self.last_run.get(check.name)
            if last is None or now - last >= self.interval(check) or (self.force_slow and check.speed == "slow"):
                due.append(check)
        return due

    def context(self) -> Context:
        return Context(self.config, self.state, runner=self.runner, now=self.clock,
                       first_cycle=self.cycles == 0, **self.context_options)

    def thresholds_for(self, kind: str) -> tuple[int, int]:
        if kind == "fast":
            value = int(self.config["thresholds"]["fast_fail"])
            return value, value
        if kind == "service":
            return int(self.config["thresholds"]["service_fail"]), 1
        return 1, 1

    def apply(self, check: Check, findings: list[Finding], error: str | None, now: float) -> list[str]:
        """Turn one check run into problem transitions; returns their names."""

        transitions: list[str] = []
        state = self.state
        error_key = f"agent:{check.name}"
        error_findings = [Finding(error_key, error is None, "p2",
                                  f"the {check.name} check failed: {error}" if error else "")]
        if error is None:
            seen = {finding.key for finding in findings}
            for key in list(state["problems"]):
                if check.owns(key) and key not in seen:
                    findings.append(Finding(key, True, message="no longer present"))
            for key in list(state["debounce"]):
                if check.owns(key) and key not in seen and key not in state["problems"]:
                    del state["debounce"][key]
        else:
            findings = []
        for finding in [*findings, *error_findings]:
            record = {"ok": finding.ok, "priority": finding.priority,
                      "message": sanitize(finding.message), "at": utc_iso(now)}
            if finding.key != error_key or finding.ok is False:
                state["last_results"][finding.key] = record
            elif finding.key in state["last_results"]:
                del state["last_results"][finding.key]
            transitions += self.track(finding, now)
        return transitions

    def track(self, finding: Finding, now: float) -> list[str]:
        state = self.state
        key = finding.key
        problem = state["problems"].get(key)
        if finding.ok is None:
            if problem is not None and finding.message:
                problem["message"] = sanitize(finding.message)
            return []
        fail_after, clear_after = self.thresholds_for(finding.kind)
        counters = state["debounce"].setdefault(key, {"fails": 0, "passes": 0})
        if finding.ok:
            counters["fails"], counters["passes"] = 0, counters["passes"] + 1
        else:
            counters["fails"], counters["passes"] = counters["fails"] + 1, 0
        if problem is None and not finding.ok and counters["fails"] >= fail_after:
            state["problems"][key] = {
                "priority": finding.priority, "message": sanitize(finding.message),
                "first_seen": utc_iso(now), "announced": False, "announced_at": None,
                "last_reminder": None,
            }
            self.queue("NEW", key, finding.priority, finding.message)
            log(f"opened [{finding.priority}] {key}: {sanitize(finding.message)}")
            return ["opened"]
        if problem is not None and not finding.ok:
            problem["message"] = sanitize(finding.message)
            if finding.priority == "p1" and problem["priority"] == "p2":
                problem["priority"] = "p1"
                self.queue("NEW", key, "p1", finding.message)
                log(f"escalated [p1] {key}: {sanitize(finding.message)}")
                return ["escalated"]
            if finding.priority == "p2" and problem["priority"] == "p1":
                problem["priority"] = "p2"
            return []
        if problem is not None and finding.ok and counters["passes"] >= clear_after:
            del state["problems"][key]
            del state["debounce"][key]
            state["outbox"] = [item for item in state["outbox"] if item["key"] != key]
            if problem["announced"]:
                since = parse_iso(problem["first_seen"]) or now
                self.queue("RESOLVED", key, problem["priority"],
                           f"resolved (was open {human_duration(now - since)}): {problem['message']}")
            log(f"resolved {key}")
            return ["resolved"]
        if problem is None and finding.ok and counters["fails"] == 0:
            state["debounce"].pop(key, None)
        return []

    def queue(self, section: str, key: str, priority: str, message: str) -> None:
        outbox = self.state["outbox"]
        outbox[:] = [item for item in outbox if not (item["key"] == key and item["section"] in ("NEW", "REMINDER"))]
        if section == "RESOLVED":
            outbox[:] = [item for item in outbox if item["key"] != key]
        outbox.append({"section": section, "key": key, "priority": priority, "message": sanitize(message)})

    def queue_reminders(self, now: float) -> None:
        period = int(self.config["thresholds"]["reminder_hours"]) * 3600
        pending = {item["key"] for item in self.state["outbox"]}
        for key, problem in self.state["problems"].items():
            if not problem["announced"] or key in pending:
                continue
            last = parse_iso(problem.get("last_reminder") or problem.get("announced_at")) or now
            if now - last >= period:
                self.queue("REMINDER", key, problem["priority"], problem["message"])

    def process_requests(self) -> None:
        directory = self.state_dir / "requests"
        try:
            requests = sorted(directory.iterdir())
        except FileNotFoundError:
            return
        for request in requests:
            match = re.fullmatch(r"reset-smart-(.+)", request.name)
            if match:
                serial = match.group(1)
                self.state["smart_baselines"].pop(serial, None)
                self.force_slow = True
                log(f"SMART baseline reset requested for {serial}")
            request.unlink(missing_ok=True)

    # -- sending ---------------------------------------------------------------

    def rate_allows(self, now: float) -> bool:
        times = [t for t in (parse_iso(v) for v in self.state["pulse_times"]) if t and now - t < 3600]
        self.state["pulse_times"] = [utc_iso(t) for t in times]
        if times and now - max(times) < PULSE_MIN_SPACING_SECONDS:
            return False
        if len(times) >= PULSE_HOURLY_LIMIT:
            self.state["rate_limited"] = True
            return False
        return True

    def send_heartbeat(self, now: float) -> bool:
        """The routine ping; clears a pending pulse first."""

        with self.lock, send_lock(self.state_dir):
            if is_staging(self.hostname()):
                return False
            config = self.config
            if self.state["pending_clear"]:
                pulse = self.state.get("pending_pulse") or "retry"
                if self.sender.send(config["ping_url"], "", compose_clear_body(config, pulse), "clear"):
                    self.state["pending_clear"] = False
                    self.save()
            ok = self.sender.send(config["ping_url"], "",
                                  compose_heartbeat_body(config, now, len(self.state["problems"])), "heartbeat")
            if ok:
                self.state["last_heartbeat_ok"] = utc_iso(now)
                self.save()
            return ok

    def maybe_pulse(self, now: float) -> bool:
        with self.lock:
            if not self.state["outbox"] or is_staging(self.hostname()) or not self.rate_allows(now):
                return False
            outbox_keys = {item["key"] for item in self.state["outbox"] if item["section"] in ("NEW", "REMINDER")}
            ongoing = [
                {"key": key, "priority": problem["priority"], "message": problem["message"]}
                for key, problem in sorted(self.state["problems"].items())
                if key not in outbox_keys
            ]
            pulse_id = os.urandom(4).hex()
            body, carried = compose_event_body(self.config, self.state["outbox"], ongoing, now, pulse_id,
                                               bool(self.state.get("rate_limited")))
            if not carried:
                return False
            with send_lock(self.state_dir):
                self.state["pending_clear"] = True
                self.state["pending_pulse"] = pulse_id
                self.save()
                ping_url = self.config["ping_url"]
                if not self.sender.send(ping_url, "/fail", body, f"pulse {pulse_id}"):
                    log(f"pulse {pulse_id} was not delivered; its items stay queued")
                    self.save()
                    return False
                self.state["pulse_times"].append(utc_iso(now))
                self.state["rate_limited"] = False
                delivered = {id(item) for item in carried}
                self.state["outbox"] = [item for item in self.state["outbox"] if id(item) not in delivered]
                for item in carried:
                    problem = self.state["problems"].get(item["key"])
                    if problem is None:
                        continue
                    if item["section"] == "NEW":
                        problem["announced"] = True
                        problem["announced_at"] = utc_iso(now)
                    elif item["section"] == "REMINDER":
                        problem["last_reminder"] = utc_iso(now)
                log(f"pulse {pulse_id} sent with {len(carried)} items ping=<redacted>")
                self.save()
                if self.sender.send(ping_url, "", compose_clear_body(self.config, pulse_id), "clear"):
                    self.state["pending_clear"] = False
                    self.save()
                if self.sender.send(ping_url, "", compose_heartbeat_body(
                        self.config, now, len(self.state["problems"])), "heartbeat"):
                    self.state["last_heartbeat_ok"] = utc_iso(now)
                self.save()
            return True

    # -- loop ------------------------------------------------------------------

    def cycle(self) -> None:
        """One scheduler tick: run due checks, apply them, pulse if needed."""

        self.reload_config()
        if is_staging(self.hostname()):
            return
        now = self.clock()
        with self.lock:
            self.process_requests()
        due = self.due_checks(now)
        ctx = self.context()
        results = [(check, *run_check(check, ctx)) for check in due]
        now = self.clock()
        with self.lock:
            for check, findings, error in results:
                self.last_run[check.name] = now
                if error:
                    self.state["check_errors"][check.name] = error
                else:
                    self.state["check_errors"].pop(check.name, None)
                self.apply(check, findings, error, now)
            if ctx.baselines_changed:
                self.state["smart_baselines"] = ctx.baselines
            if any(check.speed == "slow" for check in due):
                self.force_slow = False
            self.cycles += 1
            self.queue_reminders(now)
            self.save()
        self.maybe_pulse(now)

    def run_forever(self, stop: threading.Event | None = None) -> None:
        stop = stop or threading.Event()
        if self.first_start:
            log("first start: every open problem found by the first full run is announced")
        heartbeat = threading.Thread(target=self.heartbeat_loop, args=(stop,), daemon=True)
        heartbeat.start()
        while not stop.is_set():
            started = time.monotonic()
            try:
                self.cycle()
            except Exception as exc:  # noqa: BLE001 - keep the service alive
                log(f"cycle failed: {sanitize(exc)}")
            tick = min(int(self.config["intervals"]["fast"]), int(self.config["intervals"]["probe"]))
            stop.wait(max(1.0, tick - (time.monotonic() - started)))

    def heartbeat_loop(self, stop: threading.Event) -> None:
        while not stop.is_set():
            started = time.monotonic()
            try:
                self.send_heartbeat(self.clock())
            except Exception as exc:  # noqa: BLE001 - liveness must continue
                log(f"heartbeat failed: {sanitize(redact_url(str(exc)))}")
            stop.wait(max(1.0, int(self.config["intervals"]["alive"]) - (time.monotonic() - started)))


# systemd unit -----------------------------------------------------------------


def unit_text(role: str, program: str) -> str:
    if role not in ROLES or not re.fullmatch(r"/[A-Za-z0-9/._-]+\.py", program):
        raise ConfigError("print-unit needs a known role and a safe absolute program path")
    condition = ""
    if role == "guest":
        # Staging clones of a production VM inherit this unit; never start there.
        # systemd turns $$ into a literal $ for the shell.
        condition = ("ExecCondition=/bin/sh -c 'case \"$$(hostname)\" in stage*) exit 1;; "
                     "*) exit 0;; esac'\n")
    # proxmox-backup-manager takes lock files under /etc and /var even to
    # read, so the PBS agent cannot have a read-only file system.
    sandbox = "" if role == "pbs" else "ProtectSystem=strict\nProtectHome=yes\nReadWritePaths=/var/lib/app-ha-monitor\n"
    return f"""[Unit]
Description=BMAC monitor agent ({role})
Wants=network-online.target
After=network-online.target

[Service]
Type=simple
{condition}ExecStart=/usr/bin/python3 -I {program} run
Restart=always
RestartSec=10
User=root
NoNewPrivileges=yes
PrivateTmp=yes
{sandbox}StateDirectory=app-ha-monitor
StateDirectoryMode=0700

[Install]
WantedBy=multi-user.target
"""


# Commands ---------------------------------------------------------------------


def command_check(args: argparse.Namespace) -> int:
    config = load_config(args.config)
    state, _ = load_state_read_only(args.state_dir / "state.json")
    ctx = Context(config, state, first_cycle=True)
    rows, errors = [], []
    for check in checks_for(config["role"]):
        findings, error = run_check(check, ctx)
        if error:
            errors.append({"check": check.name, "error": error})
        for finding in findings:
            rows.append({"check": check.name, "key": finding.key, "ok": finding.ok,
                         "priority": finding.priority, "message": sanitize(finding.message)})
    report = {"node": config["node"], "role": config["role"], "agent_version": AGENT_VERSION,
              "results": rows, "errors": errors}
    if args.json:
        print(json.dumps(report, indent=2))
    else:
        for row in rows:
            mark = "ok  " if row["ok"] else ("hold" if row["ok"] is None else "FAIL")
            print(f"{mark} {row['key']:<32} {row['message'] if row['ok'] is not True else ''}".rstrip())
        for error in errors:
            print(f"ERR  agent:{error['check']:<26} {error['error']}")
    return 0


def load_state_read_only(path: Path) -> tuple[dict[str, Any], bool]:
    try:
        state = json.loads(path.read_text(encoding="utf-8"))
        if isinstance(state, dict) and state.get("schema_version") == STATE_SCHEMA:
            return {**new_state(), **state}, False
    except (OSError, ValueError):
        pass
    return new_state(), True


def command_status(args: argparse.Namespace) -> int:
    state, missing = load_state_read_only(args.state_dir / "state.json")
    try:
        config = load_config(args.config)
        identity = {"node": config["node"], "role": config["role"], "cluster": config["cluster"]}
    except ConfigError as exc:
        identity = {"config_error": str(exc)}
    requests = sorted(p.name for p in (args.state_dir / "requests").glob("*")) if (
        args.state_dir / "requests").is_dir() else []
    report = {
        **identity,
        "agent_version": AGENT_VERSION,
        "state_present": not missing,
        "problems": state["problems"],
        "pending_clear": state["pending_clear"],
        "queued_items": len(state["outbox"]),
        "last_heartbeat_ok": state["last_heartbeat_ok"],
        "last_results": state["last_results"],
        "check_errors": state["check_errors"],
        "smart_baselines": state["smart_baselines"],
        "pending_requests": requests,
    }
    if args.json:
        print(json.dumps(report, indent=2, sort_keys=True))
        return 0
    print(f"node {report.get('node', '?')} role {report.get('role', '?')} agent {AGENT_VERSION}")
    print(f"last heartbeat delivered: {state['last_heartbeat_ok'] or 'never'}")
    print(f"queued items: {len(state['outbox'])}; pending clear: {state['pending_clear']}")
    if not state["problems"]:
        print("no open problems")
    for key, problem in sorted(state["problems"].items()):
        print(f"[{problem['priority']}] {key}: {problem['message']} (since {problem['first_seen']})")
    for name, error in sorted(state["check_errors"].items()):
        print(f"check {name} failed: {error}")
    return 0


def command_test_pulse(args: argparse.Namespace) -> int:
    config = load_config(args.config)
    if is_staging(socket.gethostname()):
        print("ERROR: this is a staging clone; it never sends anything", file=sys.stderr)
        return 1
    now = time.time()
    pulse_id = os.urandom(4).hex()
    item = {"section": "NEW", "key": "test:alert", "priority": args.priority,
            "message": "test alert requested from the workstation"}
    body, _ = compose_event_body(config, [item], [], now, pulse_id)
    sender = Sender(dry_run=args.dry_run_send)
    with send_lock(args.state_dir):
        if not sender.send(config["ping_url"], "/fail", body, f"test pulse {pulse_id}"):
            print("ERROR: the test pulse could not be delivered", file=sys.stderr)
            return 1
        cleared = sender.send(config["ping_url"], "", compose_clear_body(config, pulse_id), "clear")
        sender.send(config["ping_url"], "", compose_heartbeat_body(config, now, 0), "heartbeat")
    print(json.dumps({"pulse": pulse_id, "priority": args.priority, "cleared": cleared}))
    return 0 if cleared else 1


def command_probe(args: argparse.Namespace) -> int:
    ctx = Context({"node": "probe", "cluster": "", "role": "qdevice"})
    result = run_probe(ctx, args.fqdn, args.path, args.status, args.text)
    if args.json:
        print(json.dumps(result))
    else:
        print(("ok: " if result["ok"] else "FAILED: ") + result["message"])
    return 0 if result["ok"] else 1


def command_reset_smart_baseline(args: argparse.Namespace) -> int:
    if not re.fullmatch(r"[A-Za-z0-9._:/-]{1,128}", args.serial):
        print("ERROR: unsafe serial", file=sys.stderr)
        return 2
    directory = args.state_dir / "requests"
    directory.mkdir(parents=True, exist_ok=True, mode=0o700)
    (directory / f"reset-smart-{args.serial.replace('/', '_')}").write_text("1\n", encoding="utf-8")
    print(f"requested a SMART baseline reset for {args.serial}; the running agent applies it on its next cycle")
    return 0


def command_validate_config(args: argparse.Namespace) -> int:
    require_python()
    config = load_config(args.config)
    if config["role"] == "guest":
        path = Path(config.get("guest", {}).get("settings_file") or DEFAULT_GUEST_SETTINGS)
        if path.exists():
            validate_guest_settings(json.loads(path.read_text(encoding="utf-8")))
    print(f"ok: {config['role']} {config['node']} in cluster {config['cluster']}")
    return 0


def command_run(args: argparse.Namespace) -> int:
    require_python()
    agent = Agent(args.config, args.state_dir, sender=Sender(dry_run=args.dry_run_send))
    log(f"starting {agent.config['role']} agent {AGENT_VERSION} for {agent.config['node']}")
    agent.run_forever()
    return 0


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--config", type=Path, default=DEFAULT_CONFIG)
    parser.add_argument("--state-dir", type=Path, default=DEFAULT_STATE_DIR)
    parser.add_argument("--dry-run-send", action="store_true", help="log pings instead of sending them")
    sub = parser.add_subparsers(dest="command", required=True)
    sub.add_parser("run").set_defaults(handler=command_run)
    check = sub.add_parser("check")
    check.add_argument("--once", action="store_true", required=True)
    check.add_argument("--json", action="store_true")
    check.set_defaults(handler=command_check)
    status = sub.add_parser("status")
    status.add_argument("--json", action="store_true")
    status.set_defaults(handler=command_status)
    pulse = sub.add_parser("test-pulse")
    pulse.add_argument("--priority", choices=PRIORITIES, default="p2")
    pulse.set_defaults(handler=command_test_pulse)
    probe = sub.add_parser("probe")
    probe.add_argument("--fqdn", required=True)
    probe.add_argument("--path", required=True)
    probe.add_argument("--status", type=int, required=True)
    probe.add_argument("--text")
    probe.add_argument("--json", action="store_true")
    probe.set_defaults(handler=command_probe)
    reset = sub.add_parser("reset-smart-baseline")
    reset.add_argument("serial")
    reset.set_defaults(handler=command_reset_smart_baseline)
    sub.add_parser("validate-config").set_defaults(handler=command_validate_config)
    unit = sub.add_parser("print-unit")
    unit.add_argument("--role", choices=ROLES, required=True)
    unit.add_argument("--program", required=True)
    unit.set_defaults(handler=lambda args: print(unit_text(args.role, args.program), end="") or 0)
    return parser


def main(argv: Sequence[str] | None = None) -> int:
    args = build_parser().parse_args(argv)
    try:
        return int(args.handler(args) or 0)
    except ConfigError as exc:
        print(f"ERROR: {exc}", file=sys.stderr)
        return 2
    except BrokenPipeError:
        return 0


if __name__ == "__main__":
    sys.exit(main())
