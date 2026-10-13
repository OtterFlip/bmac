# Copyright (c) 2026 BEENTHERE VENTURES, INC.
# SPDX-License-Identifier: GPL-3.0-only

"""Every copy of the v3 release version must match v3/dev/VERSION."""

from __future__ import annotations

import importlib.util
import json
import shutil
import tempfile
import tomllib
import unittest
from pathlib import Path

V3_ROOT = Path(__file__).resolve().parent.parent


def member_manifests() -> list[str]:
    manifest = tomllib.loads((V3_ROOT / "Cargo.toml").read_text(encoding="utf-8"))
    return [f"{member}/Cargo.toml" for member in manifest["workspace"]["members"]]

# Loaded by path: v2's runner also collects this file, and there `dev` is v2's.
_spec = importlib.util.spec_from_file_location("v3_set_version", V3_ROOT / "dev" / "set_version.py")
assert _spec and _spec.loader
tool = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(tool)


class VersionTest(unittest.TestCase):
    def test_every_copy_matches_dev_version(self) -> None:
        self.assertEqual(tool.mismatches(), [], "run v3/dev/set_version.py VERSION")

    def test_every_crate_and_package_is_covered(self) -> None:
        paths = {path for path, _ in tool.copies()}
        self.assertIn("Cargo.toml", paths)
        self.assertIn("Cargo.lock", paths)
        for package in ("package.json", "ui/package.json", "webui/package.json", "applianceui/package.json", "deployer/package.json"):
            self.assertIn(package, paths)
        crates = tool.workspace_crates(V3_ROOT, ".")
        self.assertIn("bmacd", crates)
        self.assertIn("bmac-deployer-app", crates)
        manifests = {*(str(p.relative_to(V3_ROOT)) for p in (V3_ROOT / "crates").glob("*/Cargo.toml")), "deployer/src-tauri/Cargo.toml"}
        self.assertEqual(set(member_manifests()), manifests)

    def test_set_version_rewrites_only_bmac_versions(self) -> None:
        with tempfile.TemporaryDirectory(prefix="bmac-version-") as scratch:
            root = Path(scratch)
            needed = {"dev/VERSION", "pnpm-workspace.yaml", *(path for path, _ in tool.copies())}
            needed |= set(member_manifests())
            for path in needed:
                (root / path).parent.mkdir(parents=True, exist_ok=True)
                shutil.copy(V3_ROOT / path, root / path)
            before = {path: (root / path).read_text() for path, _ in tool.copies(root)}

            tool.set_version("9.8.7-rc.1", root)

            self.assertEqual(tool.read_version(root), "9.8.7-rc.1")
            self.assertEqual(tool.mismatches(root), [])
            self.assertEqual(json.loads((root / "webui/package.json").read_text())["version"], "9.8.7-rc.1")
            crate_count = len(tool.workspace_crates(root, "."))
            for path, text in before.items():
                changed = [
                    (old, new) for old, new in zip(text.splitlines(), (root / path).read_text().splitlines())
                    if old != new
                ]
                expected = crate_count if path.endswith("Cargo.lock") else 1
                self.assertEqual(len(changed), expected, path)
                for _, new in changed:
                    self.assertIn('"9.8.7-rc.1"', new)

    def test_set_version_rejects_non_semver(self) -> None:
        with self.assertRaises(ValueError):
            tool.set_version("v3", Path("/nonexistent"))


if __name__ == "__main__":
    unittest.main()
