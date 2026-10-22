#!/usr/bin/env python3

# Copyright (c) 2026 BEENTHERE VENTURES, INC.
# SPDX-License-Identifier: GPL-3.0-only

"""Count this mox host's monthly public-interface traffic, in and out.

Some providers cap each server's monthly traffic (FiberState counts inbound
and outbound together) and cut its internet connection at the cap. A timer
runs `tick` every minute: it adds the bytes received and sent on the public
NIC since the last reading to the running total for the current period.

The period starts on the configured reset day (1-28; larger values are
treated as 28). The state file records the period's start date, so the total
resets exactly once when a new period begins, whatever restarts or reboots
happen. A changed boot ID or a counter smaller than the last reading means
the NIC counters restarted, and the new reading is counted from zero.

  transfer_counter.py tick      update the total (run by the timer)
  transfer_counter.py status    print the total, the cap and the next reset
"""

from __future__ import annotations

import argparse
import datetime as dt
import json
import os
from pathlib import Path
import sys
from typing import Any, Callable, Sequence


SETTINGS = Path("/etc/app-ha/guest-network.env")
STATE_FILE = Path("/var/lib/app-ha/transfer.json")
BOOT_ID = Path("/proc/sys/kernel/random/boot_id")
GIB = 1024**3


def effective_reset_day(value: str | int | None) -> int:
    if value in (None, ""):
        return 1
    day = int(value)
    if day < 1:
        raise ValueError("the transfer reset day must be at least 1")
    return min(day, 28)


def period_start(today: dt.date, reset_day: int) -> dt.date:
    """The most recent reset date on or before today."""

    if today.day >= reset_day:
        return today.replace(day=reset_day)
    previous_month = today.replace(day=1) - dt.timedelta(days=1)
    return previous_month.replace(day=reset_day)


def next_reset(today: dt.date, reset_day: int) -> dt.date:
    start = period_start(today, reset_day)
    following = (start.replace(day=1) + dt.timedelta(days=32)).replace(day=reset_day)
    return following


def tick(
    state: dict[str, Any] | None,
    *,
    today: dt.date,
    reset_day: int,
    rx_bytes: int,
    tx_bytes: int,
    boot_id: str,
    now: str,
) -> dict[str, Any]:
    """Return the new state after one reading."""

    start = period_start(today, reset_day).isoformat()
    reading = {"rx": rx_bytes, "tx": tx_bytes, "boot_id": boot_id}
    if not state:
        return {
            "period_start": start,
            "last_reset_date": None,
            "total_bytes": 0,
            "last_reading": reading,
            "updated_at": now,
        }
    total = int(state.get("total_bytes", 0))
    last = state.get("last_reading") or {}
    if last.get("boot_id") == boot_id and rx_bytes >= last.get("rx", 0) and tx_bytes >= last.get("tx", 0):
        delta = (rx_bytes - last["rx"]) + (tx_bytes - last["tx"])
    else:
        # The interface counters restarted (reboot, driver reload, wrap).
        delta = rx_bytes + tx_bytes
    updated = dict(state)
    if state.get("period_start") != start:
        # A new period began since the last reading. The bytes since then
        # can't be split from the ones before, so they go to the new period.
        updated["period_start"] = start
        updated["last_reset_date"] = today.isoformat()
        total = 0
    updated["total_bytes"] = total + delta
    updated["last_reading"] = reading
    updated["updated_at"] = now
    return updated


def load_settings(path: Path = SETTINGS) -> dict[str, str]:
    settings: dict[str, str] = {}
    for line in path.read_text(encoding="utf-8").splitlines():
        if line and not line.startswith("#"):
            key, _, value = line.partition("=")
            settings[key] = value
    if not settings.get("BMAC_PUBLIC_NIC"):
        raise SystemExit(f"transfer_counter: BMAC_PUBLIC_NIC is missing from {path}")
    return settings


def load_state(path: Path = STATE_FILE) -> dict[str, Any] | None:
    try:
        return json.loads(path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError):
        return None


def save_state(state: dict[str, Any], path: Path = STATE_FILE) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    temporary = path.with_name(f".{path.name}.tmp")
    temporary.write_text(json.dumps(state, indent=2, sort_keys=True) + "\n", encoding="utf-8")
    os.replace(temporary, path)


def status(state: dict[str, Any] | None, settings: dict[str, str], today: dt.date) -> dict[str, Any]:
    reset_day = effective_reset_day(settings.get("BMAC_TRANSFER_RESET_DAY"))
    limit = settings.get("BMAC_TRANSFER_LIMIT_GIB") or None
    total = int((state or {}).get("total_bytes", 0))
    result: dict[str, Any] = {
        "interface": settings.get("BMAC_PUBLIC_NIC"),
        "period_start": (state or {}).get("period_start"),
        "next_reset": next_reset(today, reset_day).isoformat(),
        "reset_day": reset_day,
        "total_bytes": total,
        "total_gib": round(total / GIB, 3),
        "limit_gib": int(limit) if limit else None,
        "used_percent": round(100 * total / (int(limit) * GIB), 2) if limit else None,
        "updated_at": (state or {}).get("updated_at"),
    }
    return result


def main(argv: Sequence[str] | None = None, *, clock: Callable[[], dt.datetime] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("command", choices=("tick", "status"))
    parser.add_argument("--settings", type=Path, default=SETTINGS)
    parser.add_argument("--state", type=Path, default=STATE_FILE)
    parser.add_argument("--sysfs-root", type=Path, default=Path("/sys/class/net"))
    parser.add_argument("--boot-id-file", type=Path, default=BOOT_ID)
    args = parser.parse_args(argv)
    now = (clock or (lambda: dt.datetime.now(dt.timezone.utc)))()
    settings = load_settings(args.settings)
    today = now.astimezone().date()
    state = load_state(args.state)
    if args.command == "status":
        print(json.dumps(status(state, settings, today), indent=2))
        return 0
    nic = settings["BMAC_PUBLIC_NIC"]
    statistics = args.sysfs_root / nic / "statistics"
    rx_bytes = int((statistics / "rx_bytes").read_text().strip())
    tx_bytes = int((statistics / "tx_bytes").read_text().strip())
    boot_id = args.boot_id_file.read_text().strip()
    try:
        reset_day = effective_reset_day(settings.get("BMAC_TRANSFER_RESET_DAY"))
    except ValueError as exc:
        print(f"transfer_counter: {exc}", file=sys.stderr)
        return 2
    save_state(
        tick(
            state,
            today=today,
            reset_day=reset_day,
            rx_bytes=rx_bytes,
            tx_bytes=tx_bytes,
            boot_id=boot_id,
            now=now.replace(microsecond=0).isoformat(),
        ),
        args.state,
    )
    return 0


if __name__ == "__main__":
    sys.exit(main())
