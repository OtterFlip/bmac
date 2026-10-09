#!/usr/bin/env python3

# Copyright (c) 2026 BEENTHERE VENTURES, INC.
# SPDX-License-Identifier: GPL-3.0-only

"""Check or set the BMAC release version.

    dev/set_version.py          report any file that disagrees with dev/VERSION
    dev/set_version.py 2.1.0    write 2.1.0 to dev/VERSION and every copy

dev/VERSION is the single source of truth. Cargo and npm cannot read a
version from another file, so this tool writes it into the manifests that
need a literal copy. Everything else reads dev/VERSION or those manifests at
build time: the Tauri bundle takes the Cargo version (tauri.conf.json has no
version of its own), the app reports CARGO_PKG_VERSION, and the browser mock
backend gets it from vite.config.ts.
"""

from __future__ import annotations

import re
import sys
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parent.parent
SEMVER = re.compile(r"^\d+\.\d+\.\d+(?:-[0-9A-Za-z.-]+)?(?:\+[0-9A-Za-z.-]+)?$")

# (path, pattern) pairs; group 2 of each pattern is the version.
COPIES = [
    ("dashboard/Cargo.toml",
     re.compile(r'(\[workspace\.package\][^\[]*?^version = ")([^"]*)(")', re.M)),
    ("dashboard/package.json",
     re.compile(r'(\A\{[^{]*?^  "version": ")([^"]*)(")', re.M)),
    ("dashboard/Cargo.lock",
     re.compile(r'(^name = "bmac-dashboard"\nversion = ")([^"]*)(")', re.M)),
    ("dashboard/Cargo.lock",
     re.compile(r'(^name = "bmac-engine"\nversion = ")([^"]*)(")', re.M)),
]


def read_version(root: Path = REPO_ROOT) -> str:
    return (root / "dev" / "VERSION").read_text(encoding="utf-8").strip()


def mismatches(root: Path = REPO_ROOT) -> list[str]:
    """Describe every copy that does not match dev/VERSION."""
    expected = read_version(root)
    problems = []
    if not SEMVER.match(expected):
        problems.append(f"dev/VERSION: {expected!r} is not a semantic version")
    for path, pattern in COPIES:
        found = pattern.search((root / path).read_text(encoding="utf-8"))
        if found is None:
            problems.append(f"{path}: no version matching {pattern.pattern!r}")
        elif found.group(2) != expected:
            problems.append(f"{path}: {found.group(2)} (expected {expected})")
    return problems


def set_version(version: str, root: Path = REPO_ROOT) -> None:
    if not SEMVER.match(version):
        raise ValueError(f"{version!r} is not a semantic version")
    texts = {path: (root / path).read_text(encoding="utf-8") for path, _ in COPIES}
    for path, pattern in COPIES:
        texts[path], count = pattern.subn(rf"\g<1>{version}\g<3>", texts[path], count=1)
        if count != 1:
            raise ValueError(f"{path}: no version matching {pattern.pattern!r}")
    for path, text in texts.items():
        (root / path).write_text(text, encoding="utf-8")
    (root / "dev" / "VERSION").write_text(version + "\n", encoding="utf-8")


def main(argv: list[str]) -> int:
    if len(argv) > 1 or (argv and argv[0].startswith("-")):
        print(__doc__.strip(), file=sys.stderr)
        return 2
    if argv:
        try:
            set_version(argv[0])
        except ValueError as error:
            print(f"ERROR: {error}", file=sys.stderr)
            return 1
    problems = mismatches()
    for problem in problems:
        print(f"MISMATCH {problem}", file=sys.stderr)
    if problems:
        return 1
    print(f"BMAC version {read_version()}")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
