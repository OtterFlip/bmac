#!/usr/bin/env python3

# Copyright (c) 2026 BEENTHERE VENTURES, INC.
# SPDX-License-Identifier: GPL-3.0-only

"""Render BMAC's monitoring from cluster membership (workstation only).

The inventory is a JSON file that scripts/lib/monitoring.sh builds from the
cluster: its hosts with corosync node IDs, whether a QDevice is registered,
and every production VM's registry record with its ingress probe. Settings
come from the MONITOR_* values config.sh exports (with the same defaults).

  monitor_config.py nodes   --inventory FILE        node, role, SSH target per line
  monitor_config.py desired --inventory FILE        the Healthchecks checks to sync
  monitor_config.py render  --inventory FILE --pings FILE --out-dir DIR
                                                    one agent config per node (0600)

The QDevice's node is always named "qdevice", whatever PROXMOX_QDEVICE_HOST
is, so its check keeps its name if the SSH alias changes.
"""

from __future__ import annotations

import argparse
import json
import os
from pathlib import Path
import re
import sys
from typing import Any, Mapping, Sequence


QDEVICE_NODE = "qdevice"
SETTINGS = {
    "MONITOR_ALIVE_PERIOD_SECONDS": 60,
    "MONITOR_ALIVE_GRACE_SECONDS": 180,
    "MONITOR_FAST_INTERVAL_SECONDS": 30,
    "MONITOR_FAST_FAIL_THRESHOLD": 3,
    "MONITOR_SLOW_INTERVAL_SECONDS": 1800,
    "MONITOR_POOL_MIN_FREE_PERCENT": 10,
    "MONITOR_POOL_CLEAR_FREE_PERCENT": 12,
    "MONITOR_REMINDER_HOURS": 6,
    "MONITOR_POSTFIX_QUEUE_MAX_AGE_MINUTES": 60,
    "MONITOR_CERT_WARN_DAYS": 14,
    "MONITOR_NVME_WEAR_WARN_PERCENT": 90,
}
SERVICE_FAIL_THRESHOLD = 2
PROBE_INTERVAL_SECONDS = 60
GUEST_SETTINGS_FILE = "/etc/app-ha-monitor/guest.json"


class InventoryError(Exception):
    pass


def settings(environ: Mapping[str, str] = os.environ) -> dict[str, int]:
    values = {}
    for name, default in SETTINGS.items():
        raw = environ.get(name) or str(default)
        if not raw.isdigit():
            raise InventoryError(f"{name} must be an integer")
        values[name] = int(raw)
    return values


def cluster_tag(cluster: str) -> str:
    return "bmac-cluster-" + re.sub(r"[^a-z0-9-]+", "-", cluster.lower()).strip("-")


def check_slug(cluster: str, node: str) -> str:
    return re.sub(r"[^a-z0-9-]+", "-", f"{cluster.lower()}-{node.lower()}").strip("-")


def check_name(cluster: str, node: str) -> str:
    return f"{cluster} bmaccheck {node}"


def load_inventory(path: Path) -> dict[str, Any]:
    value = json.loads(path.read_text(encoding="utf-8"))
    if not isinstance(value, dict) or not isinstance(value.get("cluster"), str) or not value["cluster"]:
        raise InventoryError("inventory has no cluster name")
    hosts = value.get("hosts")
    if not isinstance(hosts, list) or not hosts:
        raise InventoryError("inventory has no hosts")
    for host in hosts:
        if not (isinstance(host, dict) and re.fullmatch(r"mox([1-9]|10)", str(host.get("name")))
                and isinstance(host.get("nodeid"), int)):
            raise InventoryError(f"malformed host entry: {host!r}")
    if not isinstance(value.get("productions", []), list):
        raise InventoryError("productions must be a list")
    return value


def monitored_productions(inventory: dict[str, Any]) -> list[dict[str, Any]]:
    """Active production VMs; staging VMs are never monitored."""

    rows = []
    for row in inventory.get("productions", []):
        name = str(row.get("name", ""))
        if re.fullmatch(r"prod[1-9][0-9]*", name) and row.get("state") == "active":
            rows.append(row)
    return sorted(rows, key=lambda row: int(row["name"][4:]))


def nodes(inventory: dict[str, Any]) -> list[dict[str, str]]:
    """Every monitored node: name, role, and how this workstation reaches it."""

    result = [{"node": host["name"], "role": "host", "target": host["name"]}
              for host in sorted(inventory["hosts"], key=lambda h: int(h["name"][3:]))]
    result.append({"node": QDEVICE_NODE, "role": "qdevice",
                   "target": inventory.get("qdevice", {}).get("host") or QDEVICE_NODE})
    result += [{"node": row["name"], "role": "guest", "target": row["name"]}
               for row in monitored_productions(inventory)]
    return result


def desired_checks(inventory: dict[str, Any], environ: Mapping[str, str] = os.environ) -> dict[str, Any]:
    values = settings(environ)
    cluster = inventory["cluster"]
    tag = cluster_tag(cluster)
    checks = []
    for node in nodes(inventory):
        checks.append({
            "node": node["node"],
            "role": node["role"],
            "name": check_name(cluster, node["node"]),
            "slug": check_slug(cluster, node["node"]),
            "tags": f"bmac {tag} bmac-role-{node['role']}",
            "desc": f"BMAC {node['role']} {node['node']} in cluster {cluster}. "
                    "Managed by BMAC; do not rename or edit.",
            "timeout": values["MONITOR_ALIVE_PERIOD_SECONDS"],
            "grace": values["MONITOR_ALIVE_GRACE_SECONDS"],
        })
    return {"tag": tag, "checks": checks}


def probes(inventory: dict[str, Any]) -> list[dict[str, Any]]:
    result = []
    for row in monitored_productions(inventory):
        probe = row.get("probe") or {}
        if not probe.get("enabled"):
            continue
        for fqdn in probe.get("fqdns") or [row.get("primary")]:
            result.append({"vm": row["name"], "fqdn": fqdn, "path": probe["path"],
                           "expected_status": probe["expected_status"],
                           "expected_text": probe.get("expected_text")})
    return result


def node_config(
    inventory: dict[str, Any], node: dict[str, str], ping_url: str,
    environ: Mapping[str, str] = os.environ,
) -> dict[str, Any]:
    values = settings(environ)
    hosts = sorted(inventory["hosts"], key=lambda h: int(h["name"][3:]))
    registered = bool(inventory.get("qdevice", {}).get("registered"))
    config: dict[str, Any] = {
        "schema_version": 1,
        "cluster": inventory["cluster"],
        "node": node["node"],
        "role": node["role"],
        "ping_url": ping_url,
        "intervals": {
            "alive": values["MONITOR_ALIVE_PERIOD_SECONDS"],
            "fast": values["MONITOR_FAST_INTERVAL_SECONDS"],
            "slow": values["MONITOR_SLOW_INTERVAL_SECONDS"],
            "probe": PROBE_INTERVAL_SECONDS,
        },
        "thresholds": {
            "fast_fail": values["MONITOR_FAST_FAIL_THRESHOLD"],
            "service_fail": SERVICE_FAIL_THRESHOLD,
            "pool_min_free_percent": values["MONITOR_POOL_MIN_FREE_PERCENT"],
            "pool_clear_free_percent": values["MONITOR_POOL_CLEAR_FREE_PERCENT"],
            "postfix_queue_max_age_minutes": values["MONITOR_POSTFIX_QUEUE_MAX_AGE_MINUTES"],
            "cert_warn_days": values["MONITOR_CERT_WARN_DAYS"],
            "nvme_wear_warn_percent": values["MONITOR_NVME_WEAR_WARN_PERCENT"],
            "reminder_hours": values["MONITOR_REMINDER_HOURS"],
        },
    }
    if node["role"] == "host":
        config["host"] = {
            "expected_peers": [host["name"] for host in hosts if host["name"] != node["node"]],
            "qdevice_expected": registered,
        }
    elif node["role"] == "qdevice":
        config["qdevice"] = {
            "hosts": [{"name": host["name"], "tailscale_hostname": host["name"], "nodeid": host["nodeid"]}
                      for host in hosts],
            "host_tag": inventory.get("host_tag") or "tag:proxmox-host",
            "qnetd_expected": registered,
            "vote_needed": len(hosts) % 2 == 0,
            "probes": probes(inventory),
        }
    else:
        config["guest"] = {"settings_file": GUEST_SETTINGS_FILE}
    return config


def write_private_json(path: Path, value: Any) -> None:
    descriptor = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o600)
    with os.fdopen(descriptor, "w", encoding="utf-8") as handle:
        json.dump(value, handle, indent=2, sort_keys=True)
        handle.write("\n")


def main(argv: Sequence[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = parser.add_subparsers(dest="command", required=True)
    for name in ("nodes", "desired", "render"):
        command = sub.add_parser(name)
        command.add_argument("--inventory", type=Path, required=True)
        if name == "render":
            command.add_argument("--pings", type=Path, required=True)
            command.add_argument("--out-dir", type=Path, required=True)
    args = parser.parse_args(argv)
    try:
        inventory = load_inventory(args.inventory)
        if args.command == "nodes":
            for node in nodes(inventory):
                print(f"{node['node']}\t{node['role']}\t{node['target']}")
        elif args.command == "desired":
            print(json.dumps(desired_checks(inventory), indent=2))
        else:
            pings = json.loads(args.pings.read_text(encoding="utf-8")).get("nodes", {})
            rendered, missing = [], []
            for node in nodes(inventory):
                ping = (pings.get(node["node"]) or {}).get("ping_url")
                if not ping:
                    missing.append(node["node"])
                    continue
                write_private_json(args.out_dir / f"{node['node']}.json", node_config(inventory, node, ping))
                rendered.append(node["node"])
            print(json.dumps({"rendered": rendered, "missing": missing}))
            if missing:
                return 1
    except (InventoryError, ValueError, KeyError, OSError) as exc:
        print(f"ERROR: {exc}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
