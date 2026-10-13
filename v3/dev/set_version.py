#!/usr/bin/env python3

# Copyright (c) 2026 BEENTHERE VENTURES, INC.
# SPDX-License-Identifier: GPL-3.0-only

"""Check or set the BMAC v3 release version.

    v3/dev/set_version.py              report any file that disagrees with v3/dev/VERSION
    v3/dev/set_version.py 3.1.0        write 3.1.0 to v3/dev/VERSION and every copy

v3/dev/VERSION is the single source of truth. Cargo and npm cannot read a
version from another file, so this tool writes it into the manifests that
need a literal copy: each Cargo workspace's [workspace.package] version, the
workspace's own crates in each Cargo.lock, and every package.json of the pnpm
workspace. Crates take the version with `version.workspace = true`.
"""

from __future__ import annotations

import re
import sys
import tomllib
from pathlib import Path

V3_ROOT = Path(__file__).resolve().parent.parent
SEMVER = re.compile(r"^\d+\.\d+\.\d+(?:-[0-9A-Za-z.-]+)?(?:\+[0-9A-Za-z.-]+)?$")

# Cargo workspaces, relative to v3/. The Deployer's Tauri crate is a member of
# the main one.
CARGO_WORKSPACES = ["."]
WORKSPACE_VERSION = re.compile(r'(\[workspace\.package\][^\[]*?^version = ")([^"]*)(")', re.M)
PACKAGE_JSON_VERSION = re.compile(r'(\A\{[^{]*?^  "version": ")([^"]*)(")', re.M)


def lock_version(crate: str) -> re.Pattern[str]:
    return re.compile(rf'(^name = "{re.escape(crate)}"\nversion = ")([^"]*)(")', re.M)


def workspace_crates(root: Path, workspace: str) -> list[str]:
    manifest = tomllib.loads((root / workspace / "Cargo.toml").read_text(encoding="utf-8"))
    names = []
    for member in manifest["workspace"]["members"]:
        crate = tomllib.loads((root / workspace / member / "Cargo.toml").read_text(encoding="utf-8"))
        names.append(crate["package"]["name"])
    return names


def pnpm_packages(root: Path) -> list[str]:
    text = (root / "pnpm-workspace.yaml").read_text(encoding="utf-8")
    members = re.findall(r"^  - (\S+)$", text, re.M)
    return ["package.json", *(f"{member}/package.json" for member in members)]


def copies(root: Path = V3_ROOT) -> list[tuple[str, re.Pattern[str]]]:
    """(path relative to v3/, pattern) pairs; group 2 of each pattern is the version."""
    found: list[tuple[str, re.Pattern[str]]] = []
    for workspace in CARGO_WORKSPACES:
        prefix = "" if workspace == "." else f"{workspace}/"
        found.append((f"{prefix}Cargo.toml", WORKSPACE_VERSION))
        for crate in workspace_crates(root, workspace):
            found.append((f"{prefix}Cargo.lock", lock_version(crate)))
    for path in pnpm_packages(root):
        found.append((path, PACKAGE_JSON_VERSION))
    return found


def read_version(root: Path = V3_ROOT) -> str:
    return (root / "dev" / "VERSION").read_text(encoding="utf-8").strip()


def mismatches(root: Path = V3_ROOT) -> list[str]:
    """Describe every copy that does not match v3/dev/VERSION."""
    expected = read_version(root)
    problems = []
    if not SEMVER.match(expected):
        problems.append(f"dev/VERSION: {expected!r} is not a semantic version")
    for path, pattern in copies(root):
        found = pattern.search((root / path).read_text(encoding="utf-8"))
        if found is None:
            problems.append(f"{path}: no version matching {pattern.pattern!r}")
        elif found.group(2) != expected:
            problems.append(f"{path}: {found.group(2)} (expected {expected})")
    return problems


def set_version(version: str, root: Path = V3_ROOT) -> None:
    if not SEMVER.match(version):
        raise ValueError(f"{version!r} is not a semantic version")
    pairs = copies(root)
    texts = {path: (root / path).read_text(encoding="utf-8") for path, _ in pairs}
    for path, pattern in pairs:
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
