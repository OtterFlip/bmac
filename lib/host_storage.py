#!/usr/bin/env python3

# Copyright (c) 2026 BEENTHERE VENTURES, INC.
# SPDX-License-Identifier: GPL-3.0-only

"""Read-only rpool storage layout for one Proxmox host.

``collect`` runs on the Proxmox host as root (the workstation pipes this file
to ``python3 - collect``). It runs only read-only zpool, zfs, and lsblk
queries and prints one JSON document: pool capacity, every top-level vdev with
its member devices resolved through LUKS to physical disk serials, which vdev
holds the ESPs, device-removal progress, disks that are not part of rpool, and
per-zvol allocation and snapshot usage.

``render`` runs on the workstation and prints that JSON for an operator.
"""

from __future__ import annotations

import argparse
import json
import os
from pathlib import Path
import re
import subprocess
import sys
import time
from typing import Any, Callable, Sequence


SCHEMA_VERSION = 1
POOL = "rpool"
BOOT_UUIDS_FILE = Path("/etc/kernel/proxmox-boot-uuids")
CRYPTTAB_FILE = Path("/etc/crypttab")
RPOOL_MAPPER_RE = re.compile(r"^crypt-rpool-(?:a|b|mirror[2-5]-[12])$")
LSBLK_COLUMNS = (
    "NAME,KNAME,TYPE,SIZE,SERIAL,MODEL,WWN,TRAN,FSTYPE,UUID,MOUNTPOINTS"
)
STATUS_KEYS = (
    "pool",
    "state",
    "status",
    "action",
    "see",
    "scan",
    "remove",
    "checkpoint",
    "expand",
    "config",
    "errors",
)
STATUS_KEY_RE = re.compile(r"^\s{0,8}(" + "|".join(STATUS_KEYS) + r"):\s?(.*)$")
POOL_SECTIONS = {"logs", "cache", "spares", "special", "dedup"}
EVACUATING_RE = re.compile(r"Evacuation of (\S+) in progress")
VOLUME_VMID_RE = re.compile(r"/(?:vm|base)-([1-9][0-9]*)-disk-[0-9]+$")
REPLICATION_SNAPSHOT_PREFIX = "__replicate_"
STAGING_SNAPSHOT_PREFIX = "stg-base-"
CAPACITY_ATTENTION_PERCENT = 80


class StorageError(RuntimeError):
    pass


# ---------------------------------------------------------------------------
# Parsers. Each takes command output text so tests can feed fixtures.


def _optional_int(value: str) -> int | None:
    value = value.strip()
    return int(value) if value.isdigit() else None


def parse_zpool_status(text: str) -> dict[str, Any]:
    """Parse ``zpool status -P <pool>`` for one pool."""

    header: dict[str, list[str]] = {}
    config_lines: list[str] = []
    current: str | None = None
    for line in text.splitlines():
        match = STATUS_KEY_RE.match(line)
        if match and not (current == "config" and line.startswith("\t")):
            current = match.group(1)
            if current != "config":
                header.setdefault(current, [])
                if match.group(2).strip():
                    header[current].append(match.group(2).strip())
            continue
        if current == "config":
            config_lines.append(line)
        elif current is not None and line.strip():
            header[current].append(line.strip())

    if not header.get("pool"):
        raise StorageError("zpool status output has no pool line")

    sections: dict[str, list[dict[str, Any]]] = {"data": []}
    section = None
    stack: list[tuple[int, dict[str, Any]]] = []
    for line in config_lines:
        if not line.strip():
            continue
        body = line[1:] if line.startswith("\t") else line
        tokens = body.split()
        if tokens[:2] == ["NAME", "STATE"]:
            continue
        indent = len(body) - len(body.lstrip(" "))
        name = tokens[0]
        state = tokens[1] if len(tokens) > 1 else None
        if indent == 0:
            stack = []
            if name == header["pool"][0]:
                section = "data"
            elif name in POOL_SECTIONS:
                section = name
                sections.setdefault(section, [])
            else:
                raise StorageError(f"unexpected zpool config entry: {name}")
            continue
        if section is None:
            raise StorageError("zpool config lists devices before the pool")
        node = {"name": name, "state": state, "children": []}
        while stack and stack[-1][0] >= indent:
            stack.pop()
        if stack:
            stack[-1][1]["children"].append(node)
        else:
            sections[section].append(node)
        stack.append((indent, node))

    removal_text = "\n".join(header.get("remove", []))
    evacuating = EVACUATING_RE.search(removal_text)

    def leaves(node: dict[str, Any]) -> list[dict[str, Any]]:
        if not node["children"]:
            return [{"path": node["name"], "state": node["state"]}]
        found: list[dict[str, Any]] = []
        for child in node["children"]:
            found.extend(leaves(child))
        return found

    def top_level(node: dict[str, Any]) -> dict[str, Any]:
        if node["children"]:
            kind = re.sub(r"-[0-9]+$", "", node["name"])
        else:
            kind = "disk"
        return {
            "name": node["name"],
            "type": kind,
            "state": node["state"],
            "removing": bool(evacuating and evacuating.group(1) == node["name"]),
            "members": leaves(node),
        }

    return {
        "pool": header["pool"][0],
        "state": (header.get("state") or [None])[0],
        "status": " ".join(header.get("status", [])) or None,
        "scan": "\n".join(header.get("scan", [])) or None,
        "remove": removal_text or None,
        "removal_in_progress": bool(evacuating),
        "errors": " ".join(header.get("errors", [])) or None,
        "vdevs": [top_level(node) for node in sections["data"]],
        "auxiliary": {
            name: [top_level(node) for node in nodes]
            for name, nodes in sections.items()
            if name != "data"
        },
    }


def parse_zpool_list(text: str) -> dict[str, dict[str, Any]]:
    """Parse ``zpool list -v -H -p -P -o name,size,allocated,free,health``."""

    rows: dict[str, dict[str, Any]] = {}
    for line in text.splitlines():
        # Vdev and member rows may be indented; the name is still one field.
        fields = [field.strip() for field in line.strip().split("\t")]
        if len(fields) != 5 or not fields[0]:
            continue
        name, size, allocated, free, health = fields
        rows[name] = {
            "size": _optional_int(size),
            "allocated": _optional_int(allocated),
            "free": _optional_int(free),
            "health": None if health == "-" else health,
        }
    return rows


def parse_properties(text: str) -> dict[str, str]:
    """Parse ``zpool get``/``zfs get -H -p -o property,value`` output."""

    values: dict[str, str] = {}
    for line in text.splitlines():
        if "\t" in line:
            key, value = line.split("\t", 1)
            values[key.strip()] = value.strip()
    return values


def parse_lsblk(text: str) -> dict[str, dict[str, Any]]:
    """Index ``lsblk -J -b -p`` output by kernel name with disk ancestry."""

    data = json.loads(text)
    devices: dict[str, dict[str, Any]] = {}

    def visit(node: dict[str, Any], parent: str | None, disk: str | None) -> None:
        kname = node.get("kname") or node.get("name")
        if not kname:
            return
        kind = node.get("type")
        if disk is None and kind == "disk":
            disk = kname
        mountpoints = [
            value for value in (node.get("mountpoints") or []) if value
        ]
        size = node.get("size")
        devices[kname] = {
            "name": node.get("name"),
            "kname": kname,
            "type": kind,
            "size": int(size) if size is not None else None,
            "serial": (node.get("serial") or "").strip() or None,
            "model": (node.get("model") or "").strip() or None,
            "wwn": node.get("wwn"),
            "tran": node.get("tran"),
            "fstype": node.get("fstype"),
            "uuid": node.get("uuid"),
            "mountpoints": mountpoints,
            "parent": parent,
            "disk": disk,
            "children": [
                child.get("kname") or child.get("name")
                for child in node.get("children") or []
            ],
        }
        for child in node.get("children") or []:
            visit(child, kname, disk)

    for root in data.get("blockdevices", []):
        visit(root, None, None)
    return devices


def parse_boot_uuids(text: str) -> set[str]:
    return {
        line.strip().upper()
        for line in text.splitlines()
        if line.strip() and not line.startswith("#")
    }


def parse_crypttab(text: str) -> list[dict[str, Any]]:
    """rpool entries of /etc/crypttab (no key material is ever stored there)."""

    entries = []
    for line in text.splitlines():
        fields = line.split()
        if not fields or fields[0].startswith("#") or not RPOOL_MAPPER_RE.match(fields[0]):
            continue
        options = fields[3] if len(fields) > 3 else ""
        entries.append(
            {
                "mapper": fields[0],
                "shared_unlock": len(fields) > 2
                and fields[2] == "app-ha-rpool"
                and "keyscript=decrypt_keyctl" in options.split(","),
            }
        )
    return entries


def parse_volumes(volume_text: str, snapshot_text: str) -> list[dict[str, Any]]:
    """Summarize zvols and their snapshots from tab-separated ``zfs list -Hp``."""

    volumes: dict[str, dict[str, Any]] = {}
    for line in volume_text.splitlines():
        fields = line.split("\t")
        if len(fields) != 5:
            continue
        name, volsize, used, refreservation, by_snapshots = fields
        match = VOLUME_VMID_RE.search(name)
        volumes[name] = {
            "name": name,
            "vmid": int(match.group(1)) if match else None,
            "volsize": _optional_int(volsize),
            "used": _optional_int(used),
            "refreservation": _optional_int(refreservation),
            "used_by_snapshots": _optional_int(by_snapshots),
            "snapshots": 0,
            "replication_snapshots": 0,
            "staging_snapshots": [],
            "latest_replication_epoch": None,
        }
    for line in snapshot_text.splitlines():
        fields = line.split("\t")
        if len(fields) != 3 or "@" not in fields[0]:
            continue
        dataset, snapshot = fields[0].split("@", 1)
        volume = volumes.get(dataset)
        if volume is None:
            continue
        volume["snapshots"] += 1
        creation = _optional_int(fields[2])
        if snapshot.startswith(REPLICATION_SNAPSHOT_PREFIX):
            volume["replication_snapshots"] += 1
            latest = volume["latest_replication_epoch"]
            if creation is not None and (latest is None or creation > latest):
                volume["latest_replication_epoch"] = creation
        elif snapshot.startswith(STAGING_SNAPSHOT_PREFIX):
            volume["staging_snapshots"].append(snapshot)
    return sorted(volumes.values(), key=lambda row: row["name"])


# ---------------------------------------------------------------------------
# Layout assembly.


def build_layout(
    *,
    status_text: str,
    list_text: str,
    pool_properties_text: str,
    dataset_properties_text: str,
    lsblk_text: str,
    boot_uuids_text: str,
    volume_text: str,
    snapshot_text: str,
    realpath: Callable[[str], str],
    hostname: str,
    collected_at: int,
    crypttab_text: str = "",
) -> dict[str, Any]:
    status = parse_zpool_status(status_text)
    sizes = parse_zpool_list(list_text)
    pool_properties = parse_properties(pool_properties_text)
    dataset_properties = parse_properties(dataset_properties_text)
    devices = parse_lsblk(lsblk_text)
    boot_uuids = parse_boot_uuids(boot_uuids_text)

    esp_partitions = sorted(
        kname
        for kname, device in devices.items()
        if device["uuid"] and device["uuid"].upper() in boot_uuids
    )
    esp_disks = {devices[kname]["disk"] for kname in esp_partitions}
    member_disks: set[str] = set()

    def resolve_member(member: dict[str, Any]) -> dict[str, Any]:
        path = member["path"]
        kname = realpath(path) if path.startswith("/dev/") else path
        device = devices.get(kname)
        row: dict[str, Any] = {
            "path": path,
            "state": member["state"],
            "device": kname,
            "luks": False,
            "mapper": None,
            "backing": None,
            "disk": None,
            "serial": None,
            "model": None,
            "disk_size": None,
            "member_size": sizes.get(path, {}).get("size"),
        }
        if device is None:
            return row
        if device["type"] == "crypt":
            row["luks"] = True
            row["mapper"] = device["name"]
            row["backing"] = device["parent"]
        else:
            row["backing"] = kname
        disk = devices.get(device["disk"] or "")
        if disk is not None:
            member_disks.add(disk["kname"])
            row.update(
                disk=disk["kname"],
                serial=disk["serial"],
                model=disk["model"],
                disk_size=disk["size"],
            )
        return row

    vdevs = []
    for vdev in status["vdevs"]:
        members = [resolve_member(member) for member in vdev["members"]]
        size_row = sizes.get(vdev["name"], {})
        vdevs.append(
            {
                "name": vdev["name"],
                "type": vdev["type"],
                "state": vdev["state"],
                "removing": vdev["removing"],
                "size": size_row.get("size"),
                "allocated": size_row.get("allocated"),
                "free": size_row.get("free"),
                "holds_esp": any(
                    member["disk"] in esp_disks for member in members
                ),
                "luks": bool(members) and all(member["luks"] for member in members),
                "members": members,
            }
        )
    auxiliary = {
        name: [
            {**vdev, "members": [resolve_member(m) for m in vdev["members"]]}
            for vdev in rows
        ]
        for name, rows in status["auxiliary"].items()
    }

    def subtree_mounted(kname: str) -> bool:
        device = devices[kname]
        return bool(device["mountpoints"]) or any(
            subtree_mounted(child) for child in device["children"] if child in devices
        )

    unassigned = []
    for kname, device in sorted(devices.items()):
        if device["type"] != "disk" or device["parent"] is not None:
            continue
        # zvols are block disks too; they belong to guests, not to the host.
        if re.fullmatch(r"/dev/zd[0-9]+", kname):
            continue
        if kname in member_disks or kname in esp_disks:
            continue
        unassigned.append(
            {
                "disk": kname,
                "serial": device["serial"],
                "model": device["model"],
                "size": device["size"],
                "tran": device["tran"],
                "fstype": device["fstype"],
                "partitions_or_holders": len(device["children"]),
                "mounted": subtree_mounted(kname),
            }
        )

    pool_devices = {
        member["device"]
        for vdev in vdevs
        for member in vdev["members"]
        if member["device"]
    }
    luks_mappings = []
    for kname, device in sorted(devices.items()):
        name = (device["name"] or "").rsplit("/", 1)[-1]
        if device["type"] != "crypt" or not RPOOL_MAPPER_RE.match(name):
            continue
        disk = devices.get(device["disk"] or "") or {}
        luks_mappings.append(
            {
                "mapper": name,
                "backing": device["parent"],
                "disk": disk.get("kname"),
                "serial": disk.get("serial"),
                "size": disk.get("size"),
                "in_pool": kname in pool_devices,
            }
        )

    def prop_int(values: dict[str, str], key: str) -> int | None:
        return _optional_int(values.get(key, ""))

    return {
        "schema_version": SCHEMA_VERSION,
        "host": hostname,
        "collected_at": collected_at,
        "pool": {
            "name": status["pool"],
            "state": status["state"],
            "health": pool_properties.get("health"),
            "size": prop_int(pool_properties, "size"),
            "allocated": prop_int(pool_properties, "allocated"),
            "free": prop_int(pool_properties, "free"),
            "capacity_percent": prop_int(pool_properties, "capacity"),
            "fragmentation_percent": prop_int(pool_properties, "fragmentation"),
            "dataset_available": prop_int(dataset_properties, "available"),
            "dataset_used": prop_int(dataset_properties, "used"),
            "status": status["status"],
            "scan": status["scan"],
            "remove": status["remove"],
            "removal_in_progress": status["removal_in_progress"],
            "errors": status["errors"],
        },
        "vdevs": vdevs,
        "auxiliary_vdevs": auxiliary,
        "esp_partitions": esp_partitions,
        "unassigned_disks": unassigned,
        "luks_mappings": luks_mappings,
        "crypttab": parse_crypttab(crypttab_text),
        "volumes": parse_volumes(volume_text, snapshot_text),
    }


def run_command(argv: Sequence[str]) -> str:
    completed = subprocess.run(
        list(argv),
        stdin=subprocess.DEVNULL,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
        check=False,
    )
    if completed.returncode != 0:
        raise StorageError(
            f"{' '.join(argv)} failed: {completed.stderr.strip() or completed.returncode}"
        )
    return completed.stdout


def collect(pool: str = POOL) -> dict[str, Any]:
    boot_uuids = ""
    if BOOT_UUIDS_FILE.is_file():
        boot_uuids = BOOT_UUIDS_FILE.read_text(encoding="utf-8")
    crypttab = ""
    if CRYPTTAB_FILE.is_file():
        crypttab = CRYPTTAB_FILE.read_text(encoding="utf-8")
    return build_layout(
        status_text=run_command(["zpool", "status", "-P", pool]),
        list_text=run_command(
            [
                "zpool", "list", "-v", "-H", "-p", "-P",
                "-o", "name,size,allocated,free,health", pool,
            ]
        ),
        pool_properties_text=run_command(
            [
                "zpool", "get", "-H", "-p", "-o", "property,value",
                "size,allocated,free,capacity,fragmentation,health", pool,
            ]
        ),
        dataset_properties_text=run_command(
            ["zfs", "get", "-H", "-p", "-o", "property,value", "available,used", pool]
        ),
        lsblk_text=run_command(["lsblk", "-J", "-b", "-p", "-o", LSBLK_COLUMNS]),
        boot_uuids_text=boot_uuids,
        volume_text=run_command(
            [
                "zfs", "list", "-H", "-p", "-t", "volume", "-r",
                "-o", "name,volsize,used,refreservation,usedbysnapshots", pool,
            ]
        ),
        snapshot_text=run_command(
            [
                "zfs", "list", "-H", "-p", "-t", "snapshot", "-r",
                "-o", "name,used,creation", pool,
            ]
        ),
        realpath=os.path.realpath,
        hostname=os.uname().nodename.split(".", 1)[0],
        collected_at=int(time.time()),
        crypttab_text=crypttab,
    )


# ---------------------------------------------------------------------------
# Rendering.


def format_bytes(value: int | None) -> str:
    if value is None:
        return "-"
    units = ("B", "KiB", "MiB", "GiB", "TiB", "PiB")
    amount = float(value)
    for unit in units:
        if abs(amount) < 1024 or unit == units[-1]:
            return f"{amount:.0f} {unit}" if unit == "B" else f"{amount:.2f} {unit}"
        amount /= 1024
    raise AssertionError("unreachable")


def format_byte_units(value: int | None) -> str:
    """One value as bytes, MiB, and GiB, for quick comprehension."""

    if value is None:
        return "unknown"
    return (
        f"{value:,} bytes = {value / 1024**2:,.2f} MiB = {value / 1024**3:,.2f} GiB"
    )


def table(headers: Sequence[str], rows: Sequence[Sequence[Any]]) -> str:
    cells = [[str(value) for value in row] for row in rows]
    widths = [
        max([len(header), *(len(row[index]) for row in cells)])
        for index, header in enumerate(headers)
    ]
    lines = [
        "  ".join(header.ljust(widths[index]) for index, header in enumerate(headers)).rstrip()
    ]
    for row in cells:
        lines.append(
            "  ".join(value.ljust(widths[index]) for index, value in enumerate(row)).rstrip()
        )
    return "\n".join(lines)


def age(epoch: int | None, now: int) -> str:
    if epoch is None:
        return "-"
    seconds = max(0, now - epoch)
    if seconds < 3600:
        return f"{seconds // 60}m ago"
    if seconds < 86400:
        return f"{seconds // 3600}h{(seconds % 3600) // 60:02d}m ago"
    return f"{seconds // 86400}d ago"


def attention_items(
    layout: dict[str, Any], configured: Sequence[tuple[int, int, str]]
) -> list[str]:
    items: list[str] = []
    pool = layout["pool"]
    if pool["health"] != "ONLINE":
        items.append(f"{pool['name']} health is {pool['health']}")
    if pool["errors"] and pool["errors"] != "No known data errors":
        items.append(f"{pool['name']} reports errors: {pool['errors']}")
    capacity = pool["capacity_percent"]
    if capacity is not None and capacity >= CAPACITY_ATTENTION_PERCENT:
        items.append(
            f"{pool['name']} is {capacity}% allocated (attention at "
            f"{CAPACITY_ATTENTION_PERCENT}%); sparse production zvols can exhaust it"
        )
    if pool["removal_in_progress"]:
        items.append(
            "a top-level vdev removal is in progress: "
            + pool["remove"].splitlines()[0]
        )
    for vdev in layout["vdevs"]:
        if vdev["state"] != "ONLINE":
            items.append(f"vdev {vdev['name']} is {vdev['state']}")
        for member in vdev["members"]:
            if member["serial"] is None:
                items.append(
                    f"member {member['path']} of {vdev['name']} could not be "
                    "resolved to a physical disk serial"
                )
    if not any(vdev["holds_esp"] for vdev in layout["vdevs"]):
        items.append("no rpool vdev could be matched to a proxmox-boot-tool ESP")
    if configured:
        live = {
            member["serial"]
            for vdev in layout["vdevs"]
            for member in vdev["members"]
            if member["serial"]
        }
        wanted = {serial for _, _, serial in configured}
        for pair, member, serial in configured:
            if serial not in live:
                items.append(
                    f"NVME_MIRROR_{pair}_SERIAL_{member}={serial} is configured "
                    "but is not an rpool member"
                )
        for serial in sorted(live - wanted):
            items.append(
                f"rpool member disk {serial} is not recorded in the host's .conf file"
            )
    return items


def guest_label(
    vmid: int | None, guests: Sequence[dict[str, Any]], host: str
) -> str:
    """Name a zvol's guest and whether it runs here or is a replica."""

    if vmid is None:
        return "-"
    rows = [row for row in guests if str(row.get("vmid")) == str(vmid)]
    if len(rows) != 1:
        return "unknown VMID" if guests else "-"
    row = rows[0]
    where = "here" if row.get("node") == host else f"replica; runs on {row.get('node')}"
    return f"{row.get('name', '?')} ({where})"


def render(
    layout: dict[str, Any],
    configured: Sequence[tuple[int, int, str]] = (),
    now: int | None = None,
    guests: Sequence[dict[str, Any]] = (),
) -> str:
    now = int(time.time()) if now is None else now
    pool = layout["pool"]
    out: list[str] = []

    out.append(f"Pool {pool['name']} on {layout['host']}: state {pool['state']}, "
               f"health {pool['health']}")
    out.append(
        f"  size {format_bytes(pool['size'])}, allocated "
        f"{format_bytes(pool['allocated'])} ({pool['capacity_percent']}%), "
        f"free {format_bytes(pool['free'])}, fragmentation "
        f"{pool['fragmentation_percent']}%"
    )
    out.append(
        "  ZFS available (what new data and vdev removal can use):"
    )
    out.append(f"    {format_byte_units(pool['dataset_available'])}")
    for key in ("scan", "remove", "errors"):
        if pool[key]:
            first, *rest = pool[key].splitlines()
            out.append(f"  {key}: {first}")
            out.extend(f"          {line}" for line in rest)

    out.append("")
    out.append("Top-level vdevs:")
    rows = []
    for vdev in layout["vdevs"]:
        flags = []
        if vdev["holds_esp"]:
            flags.append("boot/ESP (never removable)")
        if vdev["removing"]:
            flags.append("REMOVAL IN PROGRESS")
        rows.append(
            [
                vdev["name"],
                vdev["type"],
                vdev["state"],
                "LUKS" if vdev["luks"] else "clear",
                format_bytes(vdev["size"]),
                format_bytes(vdev["allocated"]),
                format_bytes(vdev["free"]),
                ", ".join(flags) or "-",
            ]
        )
    out.append(
        table(
            ["VDEV", "TYPE", "STATE", "ENCRYPTION", "SIZE", "ALLOC", "FREE", "NOTES"],
            rows,
        )
    )

    out.append("")
    out.append("Vdev members and physical disks:")
    rows = []
    for vdev in layout["vdevs"]:
        for member in vdev["members"]:
            rows.append(
                [
                    vdev["name"],
                    member["path"],
                    member["state"],
                    member["disk"] or "?",
                    member["serial"] or "?",
                    member["model"] or "-",
                    member["disk_size"] if member["disk_size"] is not None else "?",
                ]
            )
    out.append(
        table(
            ["VDEV", "MEMBER", "STATE", "DISK", "SERIAL", "MODEL", "CAPACITY_BYTES"],
            rows,
        )
    )
    if layout["esp_partitions"]:
        out.append("ESP partitions (proxmox-boot-tool): " + ", ".join(layout["esp_partitions"]))

    for name, vdevs in layout["auxiliary_vdevs"].items():
        out.append("")
        out.append(f"{name} vdevs: " + ", ".join(vdev["name"] for vdev in vdevs))

    out.append("")
    out.append("Disks that are not part of rpool:")
    if layout["unassigned_disks"]:
        out.append(
            table(
                ["DISK", "SERIAL", "MODEL", "CAPACITY_BYTES", "TRANSPORT", "CONTENTS"],
                [
                    [
                        disk["disk"],
                        disk["serial"] or "?",
                        disk["model"] or "-",
                        disk["size"] if disk["size"] is not None else "?",
                        disk["tran"] or "-",
                        "mounted"
                        if disk["mounted"]
                        else (
                            "has partitions/signatures"
                            if disk["partitions_or_holders"] or disk["fstype"]
                            else "blank"
                        ),
                    ]
                    for disk in layout["unassigned_disks"]
                ],
            )
        )
    else:
        out.append("  none")

    out.append("")
    out.append("Guest zvols on rpool:")
    volumes = layout["volumes"]
    if volumes:
        out.append(
            table(
                [
                    "ZVOL",
                    "GUEST",
                    "VOLSIZE",
                    "USED",
                    "ALLOCATION",
                    "SNAPSHOTS",
                    "SNAP_USED",
                    "LAST_REPLICATION",
                    "STAGING_SNAPSHOTS",
                ],
                [
                    [
                        volume["name"],
                        guest_label(volume["vmid"], guests, layout["host"]),
                        format_bytes(volume["volsize"]),
                        format_bytes(volume["used"]),
                        "sparse"
                        if not volume["refreservation"]
                        else f"reserved {format_bytes(volume['refreservation'])}",
                        volume["snapshots"],
                        format_bytes(volume["used_by_snapshots"]),
                        age(volume["latest_replication_epoch"], now),
                        len(volume["staging_snapshots"]),
                    ]
                    for volume in volumes
                ],
            )
        )
    else:
        out.append("  none")

    if configured:
        out.append("")
        out.append("Serials recorded in the host's .conf file:")
        live_by_serial = {
            member["serial"]: vdev["name"]
            for vdev in layout["vdevs"]
            for member in vdev["members"]
            if member["serial"]
        }
        out.append(
            table(
                ["KEY", "SERIAL", "RPOOL_VDEV"],
                [
                    [
                        f"NVME_MIRROR_{pair}_SERIAL_{member}",
                        serial,
                        live_by_serial.get(serial, "NOT IN RPOOL"),
                    ]
                    for pair, member, serial in configured
                ],
            )
        )

    items = attention_items(layout, configured)
    out.append("")
    out.append("Storage attention items:")
    if items:
        out.extend(f"  - {item}" for item in items)
    else:
        out.append("  none")
    return "\n".join(out) + "\n"


def parse_configured(values: Sequence[str]) -> list[tuple[int, int, str]]:
    configured = []
    for value in values:
        match = re.fullmatch(r"([1-9][0-9]*):([12]):(\S+)", value)
        if match is None:
            raise StorageError(f"invalid --configured-serial value: {value!r}")
        configured.append((int(match.group(1)), int(match.group(2)), match.group(3)))
    return configured


def main(argv: Sequence[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    subparsers = parser.add_subparsers(dest="command", required=True)
    collect_parser = subparsers.add_parser(
        "collect", help="print the rpool layout of this host as JSON"
    )
    collect_parser.add_argument("--pool", default=POOL)
    render_parser = subparsers.add_parser(
        "render", help="print a collected layout for an operator"
    )
    render_parser.add_argument("layout", help="collected JSON file, or - for stdin")
    render_parser.add_argument(
        "--configured-serial",
        action="append",
        default=[],
        metavar="PAIR:MEMBER:SERIAL",
        help="a serial recorded in the host's .conf file, to compare with rpool",
    )
    render_parser.add_argument(
        "--guests",
        help="pvesh /cluster/resources --type vm JSON, to name each zvol's guest",
    )
    render_parser.add_argument(
        "--exit-status",
        action="store_true",
        help="exit 1 when there are storage attention items",
    )
    args = parser.parse_args(argv)
    try:
        if args.command == "collect":
            json.dump(collect(args.pool), sys.stdout, indent=2, sort_keys=True)
            sys.stdout.write("\n")
            return 0
        text = (
            sys.stdin.read()
            if args.layout == "-"
            else Path(args.layout).read_text(encoding="utf-8")
        )
        layout = json.loads(text)
        if layout.get("schema_version") != SCHEMA_VERSION:
            raise StorageError("unsupported storage layout schema")
        configured = parse_configured(args.configured_serial)
        guests: list[dict[str, Any]] = []
        if args.guests:
            guests = json.loads(Path(args.guests).read_text(encoding="utf-8"))
            if not isinstance(guests, list):
                raise StorageError("--guests must be a JSON array")
        sys.stdout.write(render(layout, configured, guests=guests))
        if args.exit_status and attention_items(layout, configured):
            return 1
        return 0
    except (StorageError, json.JSONDecodeError, OSError) as exc:
        print(f"ERROR: {exc}", file=sys.stderr)
        return 2


if __name__ == "__main__":
    raise SystemExit(main())
