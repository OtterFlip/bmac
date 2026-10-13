# Copyright (c) 2026 BEENTHERE VENTURES, INC.
# SPDX-License-Identifier: GPL-3.0-only

"""Every v3 source file carries the copyright and license header."""

from __future__ import annotations

import os
import unittest
from pathlib import Path

V3_ROOT = Path(__file__).resolve().parent.parent
SKIPPED_DIRS = {".git", "node_modules", "target", "dist", "__pycache__"}
# Formats without comments, generated files, and binary files.
EXEMPT_SUFFIXES = {".json", ".lock", ".png", ".jpg", ".svg", ".ico", ".woff2"}
EXEMPT_NAMES = {"pnpm-lock.yaml", "VERSION", ".nvmrc"}
HEADER = "SPDX-License-Identifier: GPL-3.0-only"
COPYRIGHT = "Copyright (c) 2026 BEENTHERE VENTURES, INC."


def source_files() -> list[Path]:
    found = []
    for directory, subdirs, files in os.walk(V3_ROOT):
        subdirs[:] = [d for d in subdirs if d not in SKIPPED_DIRS]
        for name in files:
            path = Path(directory, name)
            if name not in EXEMPT_NAMES and path.suffix not in EXEMPT_SUFFIXES:
                found.append(path)
    return found


class LicenseHeaderTest(unittest.TestCase):
    def test_every_source_file_has_the_header(self) -> None:
        missing = []
        for path in source_files():
            head = path.read_text(encoding="utf-8", errors="replace").splitlines()[:6]
            if not any(HEADER in line for line in head) or not any(COPYRIGHT in line for line in head):
                missing.append(str(path.relative_to(V3_ROOT)))
        self.assertEqual(sorted(missing), [], "add the copyright and SPDX header to these files")


if __name__ == "__main__":
    unittest.main()
