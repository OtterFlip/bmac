# Copyright (c) 2026 BEENTHERE VENTURES, INC.
# SPDX-License-Identifier: GPL-3.0-only

"""Every copy of the release version must match dev/VERSION."""

from __future__ import annotations

import json
import shutil
import tempfile
import unittest
from pathlib import Path

from dev import set_version as tool

REPO_ROOT = Path(__file__).resolve().parent.parent


class VersionTest(unittest.TestCase):
    def test_every_copy_matches_dev_version(self) -> None:
        self.assertEqual(tool.mismatches(), [], "run dev/set_version.py VERSION")

    def test_tauri_takes_the_cargo_version(self) -> None:
        config = json.loads((REPO_ROOT / "dashboard/src-tauri/tauri.conf.json").read_text())
        self.assertNotIn("version", config)

    def test_set_version_rewrites_only_bmac_versions(self) -> None:
        with tempfile.TemporaryDirectory(prefix="bmac-version-") as scratch:
            root = Path(scratch)
            for path in {"dev/VERSION", *(path for path, _ in tool.COPIES)}:
                (root / path).parent.mkdir(parents=True, exist_ok=True)
                shutil.copy(REPO_ROOT / path, root / path)
            before = {path: (root / path).read_text() for path, _ in tool.COPIES}

            tool.set_version("9.8.7-rc.1", root)

            self.assertEqual(tool.read_version(root), "9.8.7-rc.1")
            self.assertEqual(tool.mismatches(root), [])
            self.assertEqual(json.loads((root / "dashboard/package.json").read_text())["version"], "9.8.7-rc.1")
            for path, text in before.items():
                changed = [
                    (old, new) for old, new in zip(text.splitlines(), (root / path).read_text().splitlines())
                    if old != new
                ]
                expected = 2 if path.endswith("Cargo.lock") else 1
                self.assertEqual(len(changed), expected, path)
                for _, new in changed:
                    self.assertIn('"9.8.7-rc.1"', new)

    def test_set_version_rejects_non_semver(self) -> None:
        with self.assertRaises(ValueError):
            tool.set_version("v2", Path("/nonexistent"))


if __name__ == "__main__":
    unittest.main()
