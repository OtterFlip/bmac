#!/usr/bin/env python3

# Copyright (c) 2026 BEENTHERE VENTURES, INC.
# SPDX-License-Identifier: GPL-3.0-only

"""Workstation-side disk inventory: pool vdevs and disks outside every pool.

Both commands read what the host-side collectors print: the layout from
``lib/host_storage.py collect`` and the durable records from
``lib/storage_state.py show``. Nothing here touches a host.

``pending-removals LAYOUT STATE`` prints one tab-separated line per requested
vdev removal: id, vdev, status, member serials, and LUKS mapper names. The
status is evacuating, still-in-pool, needs-retire, or complete; this is the
classification hosts/inventory_disks.sh acts on.

``render --run-dir DIR`` reads the per-host files diagnostics/list_disks.sh
collected, prints a terminal report, and in JSON mode emits the same state as
one structured ``result`` event plus ``next_step`` suggestions.
"""

from __future__ import annotations

import argparse
import json
import os
from pathlib import Path
import re
import sys
import time
from typing import Any

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from quick_state import emitter, finish, heading, node_sort_key, size, table  # noqa: E402

EVACUATING_RE = re.compile(r"Evacuation of (\S+) in progress")
NODE_RE = re.compile(r"^mox([1-9]|10)$")

DISK_LABELS = {
    "awaiting_finalization": "Awaiting finalization",
    "evacuating": "Evacuating",
    "pending_replacement": "Replacement in progress",
    "pending_addition": "New mirror in progress",
    "in_use": "In use",
    "no_serial": "No serial",
    "available": "Available",
}


# ---------------------------------------------------------------------------
# Classification.


def pending_removals(layout: dict[str, Any], state: dict[str, Any]) -> list[dict[str, Any]]:
    """Every requested vdev removal and how far it has come.

    The host-side record is authoritative after ZFS forgets the member serials
    of an evacuated vdev.
    """

    pool = layout["pool"]
    in_pool = {m["serial"] for v in layout["vdevs"] for m in v["members"] if m["serial"]}
    evacuating = EVACUATING_RE.search(pool["remove"] or "")
    rows = []
    for row in state.get("removals", []):
        if row["state"] != "requested":
            continue
        # A member whose disk was already gone at removal has no serial.
        serials = [m["serial"] for m in row["members"] if m["serial"]]
        mappers = [
            m["mapper"].rsplit("/", 1)[-1]
            for m in row["members"] if m["luks"] and m["mapper"]
        ]
        if pool["removal_in_progress"] and evacuating and evacuating.group(1) == row["vdev"]:
            status = "evacuating"
        elif set(serials) & in_pool:
            status = "still-in-pool"
        elif mappers:
            status = "needs-retire"
        else:
            status = "complete"
        rows.append({
            "id": row["id"],
            "vdev": row["vdev"],
            "status": status,
            "serials": serials,
            "mappers": mappers,
            "requested_at": row.get("requested_at"),
        })
    return rows


def encryption(members: list[dict[str, Any]]) -> str:
    resolved = [member for member in members if member["device"] and member["disk"]]
    if not resolved:
        return "unknown"
    if all(member["luks"] for member in resolved):
        return "luks"
    if not any(member["luks"] for member in resolved):
        return "none"
    return "mixed"


def summarize_vdevs(layout: dict[str, Any]) -> list[dict[str, Any]]:
    """Every imported pool's top-level vdevs, with rpool's extra details."""

    rpool = layout["pool"]["name"]
    details = {vdev["name"]: vdev for vdev in layout["vdevs"]}
    vdevs = []
    for vdev in layout.get("imported_pool_vdevs", []):
        extra = details.get(vdev["name"], {}) if vdev["pool"] == rpool else {}
        members = [
            {
                "path": member["path"],
                "state": member["state"],
                "disk": member["disk"],
                "serial": member["serial"],
                "model": member["model"],
                "size": member["disk_size"],
                "luks": member["luks"],
                "mapper": (member["mapper"] or "").rsplit("/", 1)[-1] or None,
                "missing": member["disk"] is None,
            }
            for member in vdev["members"]
        ]
        if extra.get("removing"):
            status = "evacuating"
        elif extra.get("replacing"):
            status = "resilvering"
        elif vdev["state"] == "ONLINE":
            status = "online"
        elif vdev["state"] == "DEGRADED":
            status = "degraded"
        else:
            status = "faulted"
        vdevs.append({
            "pool": vdev["pool"],
            "name": vdev["name"],
            "type": vdev["type"],
            "state": vdev["state"],
            "status": status,
            "encryption": encryption(vdev["members"]),
            "holds_esp": bool(extra.get("holds_esp")),
            "size": extra.get("size"),
            "allocated": extra.get("allocated"),
            "free": extra.get("free"),
            "members": members,
        })
    return vdevs


def contents(disk: dict[str, Any]) -> str:
    if disk["mounted"]:
        return "mounted"
    if disk["partitions_or_holders"] or disk["fstype"]:
        return "partitions or signatures"
    return "blank"


def classify_disk(
    disk: dict[str, Any],
    *,
    esp_only: bool,
    removals: dict[str, dict[str, Any]],
    retired: dict[str, dict[str, Any]],
    replacements: dict[str, dict[str, Any]],
    additions: dict[str, str],
    open_mappers: dict[str, str],
    other_pools: dict[str, str],
) -> tuple[str, str]:
    """(status, detail) for one physical disk outside rpool."""

    serial = disk["serial"]
    removal = removals.get(serial or "")
    if removal and removal["status"] == "evacuating":
        return "evacuating", f"ZFS is still copying data off {removal['vdev']}; not safe to pull yet."
    if removal:
        what = (
            f"closes LUKS mappings {', '.join(removal['mappers'])} and releases the disks"
            if removal["mappers"] else "releases the disks"
        )
        return (
            "awaiting_finalization",
            f"Decommissioned from {removal['vdev']}; not safe to pull yet. "
            f"Run Inventory disks to finalize: it {what}.",
        )
    if serial in replacements:
        return (
            "pending_replacement",
            f"Chosen to replace a member of {replacements[serial]['vdev']} but has not joined it. "
            "Rerun Replace a failed disk to finish.",
        )
    if esp_only:
        return (
            "pending_replacement",
            "Holds a registered boot ESP but has not joined rpool. "
            "Rerun Replace a failed disk to finish.",
        )
    if serial in additions:
        return (
            "pending_addition",
            f"Prepared for extra mirror {additions[serial]} but not added to rpool yet. "
            "Rerun Add a new disk vdev to resume.",
        )
    if serial in open_mappers:
        return (
            "pending_replacement",
            f"LUKS mapping {open_mappers[serial]} is open but not in rpool; "
            "an add or replace was interrupted.",
        )
    if disk["in_use_reasons"]:
        reasons = list(disk["in_use_reasons"])
        if disk["disk"] in other_pools:
            reasons = [
                f"member of pool {other_pools[disk['disk']]}"
                if reason == "member of an imported ZFS pool" else reason
                for reason in reasons
            ]
        text = "; ".join(reasons)
        return "in_use", text[:1].upper() + text[1:] + "."
    if not serial:
        return "no_serial", "Reports no serial number, so the disk workflows cannot select it safely."
    detail = (
        "Blank."
        if contents(disk) == "blank"
        else "Has old partitions or signatures; they are erased when the disk is used."
    )
    if serial in retired:
        detail = f"Retired from {retired[serial]['vdev']}. " + detail
    return "available", detail + " Safe to pull, or to use in a new vdev or replacement."


def summarize_disks(
    layout: dict[str, Any], state: dict[str, Any], removals: list[dict[str, Any]]
) -> list[dict[str, Any]]:
    rpool = layout["pool"]["name"]
    by_serial = {serial: row for row in removals for serial in row["serials"]}
    retired: dict[str, dict[str, Any]] = {}
    for row in sorted(
        (row for row in state.get("removals", []) if row["state"] == "retired"),
        key=lambda row: row.get("updated_at") or 0,
    ):
        for member in row["members"]:
            if member["serial"]:
                retired[member["serial"]] = row
    replacements = {
        record["serial"]: record for record in (state.get("replacements") or {}).values()
    }
    additions = {
        serial: pair
        for pair, record in (state.get("additions") or {}).items()
        for serial in record.get("serials", [])
    }
    open_mappers = {
        row["serial"]: row["mapper"]
        for row in layout.get("luks_mappings", [])
        if row["serial"] and not row["in_pool"]
    }
    other_pools = {
        member["disk"]: f"{vdev['pool']} ({vdev['name']})"
        for vdev in layout.get("imported_pool_vdevs", [])
        if vdev["pool"] != rpool
        for member in vdev["members"]
        if member["disk"]
    }
    disks = []
    rows = [(disk, False) for disk in layout["unassigned_disks"]]
    rows += [(disk, True) for disk in layout.get("esp_only_disks", [])]
    for disk, esp_only in sorted(rows, key=lambda row: row[0]["disk"]):
        status, detail = classify_disk(
            disk,
            esp_only=esp_only,
            removals=by_serial,
            retired=retired,
            replacements=replacements,
            additions=additions,
            open_mappers=open_mappers,
            other_pools=other_pools,
        )
        disks.append({
            "disk": disk["disk"],
            "serial": disk["serial"],
            "model": disk["model"],
            "size": disk["size"],
            "tran": disk["tran"],
            "contents": contents(disk),
            "status": status,
            "label": DISK_LABELS[status],
            "detail": detail,
            "in_use_reasons": disk["in_use_reasons"],
            "removable": not disk["in_use_reasons"] and status not in (
                "awaiting_finalization", "evacuating"
            ),
        })
    return disks


def summarize_host(node: str, layout: dict[str, Any], state: dict[str, Any]) -> dict[str, Any]:
    pool = layout["pool"]
    removals = pending_removals(layout, state)
    pools = [{
        "name": pool["name"],
        "state": pool["state"],
        "health": pool["health"],
        "scan": pool["scan"],
        "remove": pool["remove"],
        "errors": pool["errors"],
    }] if pool["state"] != "UNAVAILABLE" else []
    for name in sorted({vdev["pool"] for vdev in layout.get("imported_pool_vdevs", [])}):
        if name != pool["name"]:
            pools.append({"name": name, "state": None, "health": None, "scan": None,
                          "remove": None, "errors": None})
    return {
        "node": node,
        "online": True,
        "readable": True,
        "error": None,
        "pools": pools,
        "vdevs": summarize_vdevs(layout),
        "disks": summarize_disks(layout, state, removals),
        "removals": [
            {key: row[key] for key in ("vdev", "status", "serials", "requested_at")}
            for row in removals
        ],
    }


def host_attention(host: dict[str, Any]) -> tuple[list[str], list[tuple]]:
    node = host["node"]
    problems: list[str] = []
    steps: list[tuple] = []
    if not host["online"]:
        return [f"{node} is offline; its disks were not inspected"], []
    if not host["readable"]:
        return [f"{node}: its disks could not be read: {host['error']}"], []

    def step(text: str, workflow: str, command: str) -> None:
        steps.append((True, text, f"{command} --host {node}", workflow, {"host": node}))

    for vdev in host["vdevs"]:
        if vdev["state"] != "ONLINE":
            problems.append(f"{node}: {vdev['pool']} {vdev['name']} is {vdev['state']}")
            if vdev["pool"] == "rpool" and any(
                member["missing"] or member["state"] != "ONLINE" for member in vdev["members"]
            ):
                step(f"Replace the failed disk of {vdev['name']} on {node}.",
                     "add_replacement_disk", "hosts/add_replacement_disk.sh")
    for removal in host["removals"]:
        if removal["status"] == "still-in-pool":
            problems.append(
                f"{node}: the removal of {removal['vdev']} was requested, but it is still "
                "in rpool and no removal is running"
            )
            step(f"Record the stalled removal of {removal['vdev']} on {node}.",
                 "inventory_disks", "hosts/inventory_disks.sh")
    waiting = [disk for disk in host["disks"] if disk["status"] == "awaiting_finalization"]
    if waiting:
        problems.append(
            f"{node}: disk{'s' if len(waiting) > 1 else ''} "
            + ", ".join(disk["serial"] or disk["disk"] for disk in waiting)
            + " finished evacuating and await retirement finalization"
        )
        step(f"Finalize the retirement of the decommissioned disks on {node}.",
             "inventory_disks", "hosts/inventory_disks.sh")
    for status, purpose, workflow, command, text in (
        ("pending_replacement", "a mirror replacement", "add_replacement_disk",
         "hosts/add_replacement_disk.sh", "Finish the interrupted disk replacement on {node}."),
        ("pending_addition", "a new mirror", "add_new_disk_vdev",
         "hosts/add_new_disk_vdev.sh", "Resume adding the new mirror on {node}."),
    ):
        rows = [disk for disk in host["disks"] if disk["status"] == status]
        if rows:
            problems.append(
                f"{node}: disk{'s' if len(rows) > 1 else ''} "
                + ", ".join(disk["serial"] or disk["disk"] for disk in rows)
                + f" {'were' if len(rows) > 1 else 'was'} prepared for {purpose} "
                "but never joined rpool"
            )
            step(text.format(node=node), workflow, command)
    return problems, steps


# ---------------------------------------------------------------------------
# Rendering.


def read_json(path: Path) -> Any:
    return json.loads(path.read_text(encoding="utf-8"))


def last_error(path: Path, fallback: str) -> str:
    try:
        lines = [line.strip() for line in path.read_text(encoding="utf-8").splitlines()]
    except OSError:
        return fallback
    lines = [line for line in lines if line]
    return (lines[-1].removeprefix("ERROR: ") if lines else fallback)[:500]


def load_host(run_dir: Path, node: str, online: bool) -> dict[str, Any]:
    empty = {"node": node, "online": online, "readable": False, "error": None,
             "pools": [], "vdevs": [], "disks": [], "removals": []}
    if not online:
        return empty
    try:
        layout = read_json(run_dir / f"{node}.layout.json")
    except (OSError, ValueError):
        return {**empty, "error": last_error(run_dir / f"{node}.layout.err",
                                             "the disk layout could not be collected")}
    try:
        state = read_json(run_dir / f"{node}.state.json")
    except (OSError, ValueError):
        return {**empty, "error": last_error(run_dir / f"{node}.state.err",
                                             "the storage records could not be read")}
    try:
        return summarize_host(node, layout, state)
    except (KeyError, TypeError, AttributeError) as exc:
        return {**empty, "error": f"the collected disk layout is malformed ({exc})"}


def member_label(member: dict[str, Any]) -> str:
    return member["disk"] or "missing"


def render(run_dir: Path) -> int:
    ui = emitter()
    hosts = []
    for line in (run_dir / "hosts.tsv").read_text(encoding="utf-8").splitlines():
        fields = line.split("\t")
        if len(fields) != 2 or not NODE_RE.fullmatch(fields[0]):
            continue
        hosts.append(load_host(run_dir, fields[0], fields[1] == "online"))
    hosts.sort(key=lambda host: node_sort_key(host["node"]))

    problems: list[str] = []
    steps: list[tuple] = []
    for host in hosts:
        host_problems, host_steps = host_attention(host)
        problems.extend(host_problems)
        steps.extend(host_steps)

        heading(f"DISKS ON {host['node']}")
        if not host["online"]:
            print("  OFFLINE; not inspected.")
            continue
        if not host["readable"]:
            print(f"  Could not be read: {host['error']}")
            continue
        for pool in host["pools"]:
            for key in ("scan", "remove"):
                if pool[key]:
                    print(f"  {pool['name']} {key}: {pool[key].splitlines()[0]}")
        print("\n  Pool vdevs and member disks:")
        rows = [
            [vdev["pool"], vdev["name"], vdev["status"].upper(), vdev["encryption"].upper(),
             "boot/ESP" if vdev["holds_esp"] else "-", member["state"],
             member_label(member), member["serial"] or "?", size(member["size"])]
            for vdev in host["vdevs"] for member in vdev["members"]
        ]
        if rows:
            table(["POOL", "VDEV", "STATUS", "ENCRYPTION", "ROLE", "MEMBER", "DISK",
                   "SERIAL", "SIZE"], rows, indent="    ")
        else:
            print("    none")
        print("\n  Disks outside every pool:")
        if host["disks"]:
            table(
                ["DISK", "SERIAL", "SIZE", "MODEL", "STATUS", "DETAIL"],
                [[disk["disk"], disk["serial"] or "?", size(disk["size"]),
                  disk["model"] or "-", disk["label"], disk["detail"]]
                 for disk in host["disks"]],
                indent="    ",
            )
        else:
            print("    none")

    summary = {"collected_at": int(time.time()), "hosts": hosts, "problems": problems}
    unique = []
    for row in steps:
        if row[1:] not in [kept[1:] for kept in unique]:
            unique.append(row)
    finish(summary, problems, ui, unique)
    return 0


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    sub = parser.add_subparsers(dest="command", required=True)
    pending = sub.add_parser("pending-removals", help="classify requested vdev removals")
    pending.add_argument("layout")
    pending.add_argument("state")
    rendered = sub.add_parser("render", help="report the disks list_disks.sh collected")
    rendered.add_argument("--run-dir", required=True, type=Path)
    args = parser.parse_args(argv)
    try:
        if args.command == "pending-removals":
            for row in pending_removals(read_json(Path(args.layout)), read_json(Path(args.state))):
                print("\t".join([row["id"], row["vdev"], row["status"],
                                 ",".join(row["serials"]), ",".join(row["mappers"])]))
            return 0
        return render(args.run_dir)
    except (OSError, ValueError, KeyError) as exc:
        print(f"ERROR: {exc}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
