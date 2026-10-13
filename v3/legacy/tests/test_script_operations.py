# Copyright (c) 2026 BEENTHERE VENTURES, INC.
# SPDX-License-Identifier: GPL-3.0-only

"""script-operations.json must match the adapted scripts it launches.

Ported from v2's dashboard/test_workflow_registry.py. The checks that read
the scripts skip until M4.1 copies them into legacy/scripts/user_callable/.
"""

from __future__ import annotations

import json
import os
import re
import subprocess
import unittest
from pathlib import Path

LEGACY = Path(__file__).resolve().parent.parent
REGISTRY = json.loads((LEGACY / "script-operations.json").read_text())
OPERATIONS = REGISTRY["operations"]
ROLES = ["viewer", "operator", "admin"]


def flags(operation: dict) -> list[str]:
    found = []
    for param in operation.get("params", []):
        if param.get("flag"):
            found.append(param["flag"])
        found += [choice["flag"] for choice in param.get("choices", []) if choice.get("flag")]
    if operation.get("supports_dry_run"):
        found.append("--dry-run")
    return found


def scripts_copied() -> bool:
    return (LEGACY / "scripts" / "user_callable").is_dir()


class ScriptOperationsTest(unittest.TestCase):
    def test_ids_and_scripts_are_unique(self) -> None:
        ids = [operation["id"] for operation in OPERATIONS]
        self.assertEqual(len(ids), len(set(ids)))
        for operation_id in ids:
            self.assertRegex(operation_id, r"^[a-z][a-z0-9_]*(\.[a-z][a-z0-9_]*)+$")
        scripts = [Path(operation["script"]).name for operation in OPERATIONS]
        self.assertEqual(len(scripts), len(set(scripts)))

    def test_destructive_operations_are_mutating_and_for_administrators(self) -> None:
        for operation in OPERATIONS:
            with self.subTest(operation=operation["id"]):
                if operation["destructive"]:
                    self.assertTrue(operation["mutating"])
                    self.assertEqual(operation["min_role"], "admin")
                if operation["mutating"]:
                    self.assertGreaterEqual(ROLES.index(operation["min_role"]), 1)
                    self.assertTrue(operation["locks"], "a mutating operation holds a lock")
                else:
                    self.assertFalse(operation.get("locks"))

    def test_script_operations_are_never_resumed(self) -> None:
        for operation in OPERATIONS:
            self.assertEqual(operation["kind"], "script")
            self.assertEqual(operation["resume"], "never", operation["id"])
            self.assertEqual(operation["cancel"], {"rule": "script"}, operation["id"])

    def test_no_parameter_is_a_path(self) -> None:
        for operation in OPERATIONS:
            for param in operation.get("params", []):
                self.assertNotEqual(param["type"], "path", f"{operation['id']}.{param['id']}")

    def test_every_script_exists_and_supports_json_mode(self) -> None:
        if not scripts_copied():
            self.skipTest("M4.1 copies the scripts into legacy/scripts/user_callable/")
        for operation in OPERATIONS:
            with self.subTest(operation=operation["id"]):
                script = LEGACY / operation["script"]
                self.assertTrue(script.is_file() and not script.is_symlink())
                self.assertTrue(os.access(script, os.X_OK), f"{script} is not executable")
                self.assertRegex(script.read_text(), r"(?m)^\s*bmac_ui_bootstrap \"\$@\"$")

    def test_every_flag_is_parsed_by_its_script(self) -> None:
        if not scripts_copied():
            self.skipTest("M4.1 copies the scripts into legacy/scripts/user_callable/")
        for operation in OPERATIONS:
            text = (LEGACY / operation["script"]).read_text()
            for flag in flags(operation):
                with self.subTest(operation=operation["id"], flag=flag):
                    self.assertRegex(text, rf"(?m)^\s*(\S+ \| )*{re.escape(flag)}( \| \S+)*\)")

    def test_every_script_passes_bash_syntax_check(self) -> None:
        if not scripts_copied():
            self.skipTest("M4.1 copies the scripts into legacy/scripts/user_callable/")
        for operation in OPERATIONS:
            with self.subTest(operation=operation["id"]):
                completed = subprocess.run(
                    ["bash", "-n", str(LEGACY / operation["script"])],
                    capture_output=True, text=True, timeout=30,
                )
                self.assertEqual(completed.returncode, 0, completed.stderr)


if __name__ == "__main__":
    unittest.main()
