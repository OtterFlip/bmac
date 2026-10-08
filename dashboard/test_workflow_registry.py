# Copyright (c) 2026 BEENTHERE VENTURES, INC.
# SPDX-License-Identifier: GPL-3.0-only

"""The dashboard's workflow registry must match the scripts it launches."""

from __future__ import annotations

import json
import os
import re
import subprocess
import unittest
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parent.parent
REGISTRY = json.loads((REPO_ROOT / "dashboard" / "engine" / "workflows.json").read_text())


def flags(param: dict) -> list[str]:
    found = [param["flag"]] if param.get("flag") else []
    found += [choice["flag"] for choice in param.get("choices", []) if choice.get("flag")]
    return found


class WorkflowRegistryTest(unittest.TestCase):
    def test_ids_are_unique_and_match_script_names(self) -> None:
        ids = [workflow["id"] for workflow in REGISTRY["workflows"]]
        self.assertEqual(len(ids), len(set(ids)))
        for workflow in REGISTRY["workflows"]:
            self.assertEqual(Path(workflow["script"]).stem, workflow["id"])

    def test_every_script_exists_and_supports_json_mode(self) -> None:
        for workflow in REGISTRY["workflows"]:
            with self.subTest(workflow=workflow["id"]):
                script = REPO_ROOT / workflow["script"]
                self.assertTrue(script.is_file() and not script.is_symlink())
                self.assertTrue(os.access(script, os.X_OK), f"{script} is not executable")
                self.assertRegex(script.read_text(), r"(?m)^\s*bmac_ui_bootstrap \"\$@\"$")

    def test_every_flag_is_parsed_by_its_script(self) -> None:
        for workflow in REGISTRY["workflows"]:
            text = (REPO_ROOT / workflow["script"]).read_text()
            for param in workflow.get("params", []):
                for flag in flags(param):
                    with self.subTest(workflow=workflow["id"], flag=flag):
                        self.assertRegex(text, rf"(?m)^\s*(\S+ \| )*{re.escape(flag)}( \| \S+)*\)")

    def test_destructive_workflows_are_mutating(self) -> None:
        for workflow in REGISTRY["workflows"]:
            if workflow.get("destructive"):
                self.assertNotEqual(workflow["mode"], "read_only", workflow["id"])

    def test_every_script_passes_bash_syntax_check(self) -> None:
        for workflow in REGISTRY["workflows"]:
            with self.subTest(workflow=workflow["id"]):
                completed = subprocess.run(
                    ["bash", "-n", str(REPO_ROOT / workflow["script"])],
                    capture_output=True, text=True, timeout=30,
                )
                self.assertEqual(completed.returncode, 0, completed.stderr)


if __name__ == "__main__":
    unittest.main()
