#!/usr/bin/env python3

# Copyright (c) 2026 BEENTHERE VENTURES, INC.
# SPDX-License-Identifier: GPL-3.0-only

"""Keep a workstation's env/moxN.conf in step with the host's rpool mirrors.

``show`` prints the active NVME_MIRROR_N_* entries as JSON. ``assign`` writes
the serial and byte-capacity entries of a newly added mirror pair. ``retire``
comments out the entries of a mirror whose vdev removal completed, so the
file still records the hardware that used to be there. Every other line is
kept byte for byte, and the file is replaced atomically with its mode kept.
The workflow scripts reload the file with lib/config.sh afterwards.
"""

from __future__ import annotations

import argparse
import datetime
import json
import os
from pathlib import Path
import re
import sys
import tempfile
from typing import Any, Sequence


PAIRS = (1, 2, 3, 4, 5)
KEY_RE = re.compile(r"^NVME_MIRROR_([1-5])_(SERIAL|CAPACITY_BYTES)_([12])$")
ACTIVE_RE = re.compile(
    r"^\s*(NVME_MIRROR_([1-5])_(SERIAL|CAPACITY_BYTES)_([12]))\s*=\s*(.*?)\s*$"
)
SERIAL_RE = re.compile(r"^[A-Za-z0-9._:+-]+$")


class ConfError(RuntimeError):
    pass


def unquote(value: str) -> str:
    if len(value) >= 2 and value[0] == value[-1] and value[0] in "'\"":
        return value[1:-1]
    return value


def read_lines(path: Path) -> list[str]:
    if path.is_symlink() or not path.is_file():
        raise ConfError(f"{path} must be a regular file")
    return path.read_text(encoding="utf-8").splitlines(keepends=True)


def active_pairs(lines: Sequence[str]) -> dict[int, dict[str, str]]:
    pairs: dict[int, dict[str, str]] = {}
    for line in lines:
        match = ACTIVE_RE.match(line)
        if match:
            field = f"{match.group(3).lower().replace('capacity_bytes', 'capacity')}_{match.group(4)}"
            pairs.setdefault(int(match.group(2)), {})[field] = unquote(match.group(5))
    return pairs


def write_lines(path: Path, lines: Sequence[str]) -> None:
    mode = path.stat().st_mode & 0o777
    handle, temporary = tempfile.mkstemp(prefix=f".{path.name}.", dir=path.parent)
    try:
        with os.fdopen(handle, "w", encoding="utf-8") as stream:
            stream.writelines(lines)
            stream.flush()
            os.fsync(stream.fileno())
        os.chmod(temporary, mode)
        os.replace(temporary, path)
    except BaseException:
        Path(temporary).unlink(missing_ok=True)
        raise


def placeholder_pair(line: str) -> int | None:
    """Pair number of an empty commented template line such as
    ``#NVME_MIRROR_2_SERIAL_1=``, else None."""

    match = re.match(r"^\s*#\s*(NVME_MIRROR_([1-5])_[A-Z_]+_[12])\s*=\s*$", line)
    if match and KEY_RE.match(match.group(1)):
        return int(match.group(2))
    return None


def assign(
    lines: list[str],
    pair: int,
    serials: tuple[str, str],
    capacities: tuple[int, int],
    note: str,
) -> list[str]:
    if pair not in PAIRS[1:]:
        raise ConfError("only extra mirror pairs 2 through 5 can be assigned")
    for serial in serials:
        if not SERIAL_RE.fullmatch(serial):
            raise ConfError(f"unsafe disk serial: {serial}")
    if serials[0] == serials[1]:
        raise ConfError("the two serials must differ")
    pairs = active_pairs(lines)
    if pair in pairs:
        raise ConfError(f"NVME_MIRROR_{pair} is already configured")
    for other, values in pairs.items():
        for serial in serials:
            if serial in (values.get("serial_1"), values.get("serial_2")):
                raise ConfError(f"serial {serial} is already configured in NVME_MIRROR_{other}")
    if not lines or not lines[-1].endswith("\n"):
        lines = [*lines[:-1], lines[-1] + "\n"] if lines else []
    block = [
        f"# {note}\n",
        f"NVME_MIRROR_{pair}_SERIAL_1={serials[0]}\n",
        f"NVME_MIRROR_{pair}_SERIAL_2={serials[1]}\n",
        f"NVME_MIRROR_{pair}_CAPACITY_BYTES_1={capacities[0]}\n",
        f"NVME_MIRROR_{pair}_CAPACITY_BYTES_2={capacities[1]}\n",
    ]
    placeholders = [index for index, line in enumerate(lines) if placeholder_pair(line) == pair]
    if placeholders:
        first = placeholders[0]
        kept = [line for index, line in enumerate(lines) if index not in placeholders]
        return kept[:first] + block + kept[first:]
    mirror_lines = [index for index, line in enumerate(lines) if "NVME_MIRROR_" in line]
    at = mirror_lines[-1] + 1 if mirror_lines else len(lines)
    return lines[:at] + block + lines[at:]


def retire(lines: list[str], serials: Sequence[str], note: str) -> tuple[list[str], int]:
    wanted = set(serials)
    matches = [
        pair
        for pair, values in active_pairs(lines).items()
        if wanted & {values.get("serial_1"), values.get("serial_2")}
    ]
    if not matches:
        raise LookupError("no configured NVMe mirror pair holds " + ", ".join(serials))
    if len(matches) != 1:
        raise ConfError("the serials belong to more than one configured mirror pair")
    pair = matches[0]
    if pair == 1:
        raise ConfError("mirror 1 holds the boot ESPs and is never decommissioned")
    retired = []
    for line in lines:
        match = ACTIVE_RE.match(line)
        if match and int(match.group(2)) == pair:
            retired.append(f"# {note}: {line.lstrip()}")
        else:
            retired.append(line)
    return retired, pair


def main(argv: Sequence[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    subparsers = parser.add_subparsers(dest="command", required=True)
    show = subparsers.add_parser("show")
    show.add_argument("conf", type=Path)
    assign_parser = subparsers.add_parser("assign")
    assign_parser.add_argument("conf", type=Path)
    assign_parser.add_argument("--pair", type=int, required=True)
    assign_parser.add_argument("--serial", action="append", required=True)
    assign_parser.add_argument("--capacity", action="append", type=int, required=True)
    assign_parser.add_argument("--note", required=True)
    retire_parser = subparsers.add_parser("retire")
    retire_parser.add_argument("conf", type=Path)
    retire_parser.add_argument("--serial", action="append", required=True)
    retire_parser.add_argument("--note", required=True)
    args = parser.parse_args(argv)
    try:
        lines = read_lines(args.conf)
        if args.command == "show":
            json.dump(
                {str(pair): values for pair, values in sorted(active_pairs(lines).items())},
                sys.stdout,
                indent=2,
                sort_keys=True,
            )
            sys.stdout.write("\n")
            return 0
        if "\n" in args.note:
            raise ConfError("--note must be one line")
        if args.command == "assign":
            if len(args.serial) != 2 or len(args.capacity) != 2:
                raise ConfError("assign needs exactly two --serial and two --capacity values")
            if min(args.capacity) <= 0:
                raise ConfError("capacities must be positive byte counts")
            write_lines(
                args.conf,
                assign(lines, args.pair, tuple(args.serial), tuple(args.capacity), args.note),
            )
            print(f"Recorded NVME_MIRROR_{args.pair} in {args.conf}")
            return 0
        updated, pair = retire(lines, args.serial, args.note)
        write_lines(args.conf, updated)
        print(f"Commented out NVME_MIRROR_{pair} in {args.conf}")
        return 0
    except LookupError as exc:
        print(f"NOTICE: {exc}", file=sys.stderr)
        return 3
    except (ConfError, OSError) as exc:
        print(f"ERROR: {exc}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
