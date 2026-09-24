#!/usr/bin/env python3

# Copyright (c) 2026 BEENTHERE VENTURES, INC.
# SPDX-License-Identifier: GPL-3.0-only

"""Durable storage-maintenance records kept on one Proxmox host.

The disk workflows pipe this file to ``python3 - COMMAND`` on the host. It
keeps one root-only JSON document, by default
/var/lib/app-ha-storage/state.json, holding:

- when each production guest's filesystem was last trimmed for this host;
- when rpool was last scrubbed, and the result;
- one record per top-level vdev removal, with its member disks' serials,
  because ZFS forgets which disks a vdev used once its removal completes.

Every change takes an exclusive lock and replaces the file atomically. Times
are the host's epoch seconds; ``show`` includes the host's current time so
callers compare ages on one clock.
"""

from __future__ import annotations

import argparse
import fcntl
import json
import os
from pathlib import Path
import re
import socket
import sys
import tempfile
import time
from typing import Any, Callable, Sequence


SCHEMA_VERSION = 1
DEFAULT_STATE_FILE = Path(
    os.environ.get("APP_HA_STORAGE_STATE_FILE", "/var/lib/app-ha-storage/state.json")
)
GUEST_RE = re.compile(r"^prod[1-9][0-9]*$")
VDEV_RE = re.compile(r"^mirror-[0-9]+$")
REMOVAL_STATES = ("requested", "failed", "retired")
MAX_SCRUBS = 20
MAX_REMOVALS = 200


class StateError(RuntimeError):
    pass


def empty_state() -> dict[str, Any]:
    return {
        "schema_version": SCHEMA_VERSION,
        "host": socket.gethostname().split(".", 1)[0],
        "trims": {},
        "scrubs": [],
        "removals": [],
    }


def validate(state: Any) -> dict[str, Any]:
    if not isinstance(state, dict) or state.get("schema_version") != SCHEMA_VERSION:
        raise StateError("unsupported storage state schema")
    for key, kind in (("trims", dict), ("scrubs", list), ("removals", list)):
        if not isinstance(state.get(key), kind):
            raise StateError(f"storage state {key} is malformed")
    return state


def read_state(path: Path) -> dict[str, Any]:
    if path.is_symlink():
        raise StateError(f"refusing symlinked storage state {path}")
    if not path.exists():
        return empty_state()
    return validate(json.loads(path.read_text(encoding="utf-8")))


def write_state(path: Path, state: dict[str, Any]) -> None:
    validate(state)
    path.parent.mkdir(mode=0o700, parents=True, exist_ok=True)
    handle, temporary = tempfile.mkstemp(prefix=".state.", dir=path.parent)
    try:
        with os.fdopen(handle, "w", encoding="utf-8") as stream:
            json.dump(state, stream, indent=2, sort_keys=True)
            stream.write("\n")
            stream.flush()
            os.fsync(stream.fileno())
        os.chmod(temporary, 0o600)
        os.replace(temporary, path)
    except BaseException:
        Path(temporary).unlink(missing_ok=True)
        raise


def mutate(path: Path, change: Callable[[dict[str, Any]], Any]) -> Any:
    path.parent.mkdir(mode=0o700, parents=True, exist_ok=True)
    lock_path = path.with_name(path.name + ".lock")
    with open(lock_path, "a", encoding="utf-8") as lock:
        os.chmod(lock_path, 0o600)
        fcntl.flock(lock, fcntl.LOCK_EX)
        state = read_state(path)
        result = change(state)
        write_state(path, state)
        return result


def record_trims(state: dict[str, Any], guests: Sequence[str], now: int) -> None:
    for guest in guests:
        if not GUEST_RE.fullmatch(guest):
            raise StateError(f"invalid production guest name: {guest}")
        state["trims"][guest] = {"completed_at": now}


def record_scrub(state: dict[str, Any], result: str, now: int) -> None:
    if not result or "\n" in result or len(result) > 500:
        raise StateError("scrub result must be one line of at most 500 characters")
    state["scrubs"].append({"completed_at": now, "result": result})
    del state["scrubs"][:-MAX_SCRUBS]


MEMBER_KEYS = {"path", "mapper", "luks", "disk", "serial", "model", "disk_size"}


def record_removal(state: dict[str, Any], request: Any, now: int) -> str:
    if not isinstance(request, dict) or set(request) != {"vdev", "members", "conf_pair"}:
        raise StateError("removal request needs exactly vdev, members, and conf_pair")
    vdev = request["vdev"]
    if not isinstance(vdev, str) or not VDEV_RE.fullmatch(vdev):
        raise StateError(f"invalid vdev name: {vdev!r}")
    members = request["members"]
    if not isinstance(members, list) or len(members) < 1:
        raise StateError("removal members must be a non-empty list")
    for member in members:
        if not isinstance(member, dict) or set(member) != MEMBER_KEYS:
            raise StateError("each removal member needs exactly " + ", ".join(sorted(MEMBER_KEYS)))
        if not member["serial"]:
            raise StateError("every removed member must have a disk serial")
    pair = request["conf_pair"]
    if pair is not None and pair not in (2, 3, 4, 5):
        raise StateError("conf_pair must be 2 through 5 or null")
    if any(row["state"] == "requested" for row in state["removals"]):
        raise StateError("another vdev removal is still recorded as in progress")
    removal_id = f"removal-{now}-{vdev}"
    state["removals"].append(
        {
            "id": removal_id,
            "vdev": vdev,
            "members": members,
            "conf_pair": pair,
            "state": "requested",
            "requested_at": now,
            "updated_at": now,
            "notes": [],
        }
    )
    del state["removals"][:-MAX_REMOVALS]
    return removal_id


def set_removal(
    state: dict[str, Any], removal_id: str, new_state: str, note: str | None, now: int
) -> None:
    if new_state not in REMOVAL_STATES:
        raise StateError(f"removal state must be one of {', '.join(REMOVAL_STATES)}")
    rows = [row for row in state["removals"] if row["id"] == removal_id]
    if len(rows) != 1:
        raise StateError(f"unknown removal record {removal_id}")
    row = rows[0]
    row["state"] = new_state
    row["updated_at"] = now
    if note:
        if "\n" in note or len(note) > 500:
            raise StateError("notes must be one line of at most 500 characters")
        row["notes"].append({"at": now, "note": note})


def main(argv: Sequence[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--state-file", type=Path, default=DEFAULT_STATE_FILE)
    subparsers = parser.add_subparsers(dest="command", required=True)
    subparsers.add_parser("show", help="print the state and the host's current time")
    trim = subparsers.add_parser("record-trim", help="record completed guest trims")
    trim.add_argument("guests", nargs="+")
    scrub = subparsers.add_parser("record-scrub", help="record a completed scrub")
    scrub.add_argument("--result", required=True)
    removal_request = subparsers.add_parser(
        "record-removal", help="record a requested vdev removal"
    )
    removal_request.add_argument("--request", required=True, help="removal request JSON")
    removal = subparsers.add_parser("set-removal", help="update a removal record")
    removal.add_argument("removal_id")
    removal.add_argument("--state", required=True, choices=REMOVAL_STATES)
    removal.add_argument("--note")
    args = parser.parse_args(argv)
    now = int(time.time())
    try:
        if args.command == "show":
            state = read_state(args.state_file)
            json.dump({**state, "now": now}, sys.stdout, indent=2, sort_keys=True)
            sys.stdout.write("\n")
        elif args.command == "record-trim":
            mutate(args.state_file, lambda state: record_trims(state, args.guests, now))
        elif args.command == "record-scrub":
            mutate(args.state_file, lambda state: record_scrub(state, args.result, now))
        elif args.command == "record-removal":
            request = json.loads(args.request)
            print(mutate(args.state_file, lambda state: record_removal(state, request, now)))
        elif args.command == "set-removal":
            mutate(
                args.state_file,
                lambda state: set_removal(state, args.removal_id, args.state, args.note, now),
            )
        return 0
    except (StateError, json.JSONDecodeError, OSError) as exc:
        print(f"ERROR: {exc}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
