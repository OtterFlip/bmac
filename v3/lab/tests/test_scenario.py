# Copyright (c) 2026 BEENTHERE VENTURES, INC.
# SPDX-License-Identifier: GPL-3.0-only

"""The scenario runner's PASS/FAIL report and exit status, and the CLI."""

from __future__ import annotations

import io
import os
import subprocess
import sys
import tempfile
import textwrap
import unittest
from pathlib import Path

LAB = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(LAB))

from bmaclab import scenario  # noqa: E402


class Clock:
    def __init__(self) -> None:
        self.now = 0.0

    def __call__(self) -> float:
        self.now += 0.5
        return self.now


class ScenarioRunnerTest(unittest.TestCase):
    def setUp(self) -> None:
        scratch = tempfile.TemporaryDirectory(prefix="bmac-lab-scenario-")
        self.addCleanup(scratch.cleanup)
        self.folder = Path(scratch.name)

    def run_scenario(self, body: str) -> tuple[int, str]:
        path = self.folder / "demo.py"
        path.write_text('DESCRIPTION = "a demo"\n\n' + textwrap.dedent(body))
        out = io.StringIO()
        code = scenario.run(lab=object(), name=str(path), out=out, clock=Clock())
        return code, out.getvalue()

    def test_all_passing_checks_exit_zero(self) -> None:
        code, out = self.run_scenario("""
            def run(s):
                s.check("one", True)
                s.check("two", lambda: "yes")
        """)
        self.assertEqual(code, 0)
        self.assertEqual(out.splitlines()[0], "demo: a demo")
        self.assertIn("  PASS  one (0.5s)", out)
        self.assertIn("demo: 2 passed, 0 failed", out)

    def test_failures_carry_on_and_show_their_detail(self) -> None:
        code, out = self.run_scenario("""
            def run(s):
                s.check("falsy", lambda: 0, detail="line one\\nline two")
                s.check("raises", lambda: 1 / 0)
                s.check("after", True)
        """)
        self.assertEqual(code, 1)
        self.assertIn("  FAIL  falsy", out)
        self.assertIn("        line two", out)
        self.assertIn("        ZeroDivisionError: division by zero", out)
        self.assertIn("  PASS  after", out)
        self.assertIn("1 passed, 2 failed", out)

    def test_a_failed_requirement_stops_the_scenario(self) -> None:
        code, out = self.run_scenario("""
            def run(s):
                s.require("needed", False)
                s.check("never", True)
        """)
        self.assertEqual(code, 1)
        self.assertNotIn("never", out)
        self.assertIn("0 passed, 1 failed; stopped after needed", out)

    def test_a_crash_is_a_failure_with_its_traceback(self) -> None:
        code, out = self.run_scenario("""
            def run(s):
                s.check("fine", True)
                raise RuntimeError("boom")
        """)
        self.assertEqual(code, 1)
        self.assertIn("FAIL  the scenario crashed", out)
        self.assertIn("RuntimeError: boom", out)

    def test_a_scenario_with_no_checks_fails(self) -> None:
        code, _ = self.run_scenario("def run(s):\n    pass\n")
        self.assertEqual(code, 1)

    def test_unknown_names_list_the_available_scenarios(self) -> None:
        with self.assertRaisesRegex(FileNotFoundError, "available: .*smoke"):
            scenario.resolve("nope")
        self.assertEqual(scenario.resolve("smoke").name, "smoke.py")


class CliTest(unittest.TestCase):
    def lab(self, *args: str, root: Path) -> subprocess.CompletedProcess:
        env = {**os.environ, "BMAC_LAB_DIR": str(root)}
        return subprocess.run([sys.executable, str(LAB / "lab"), *args], capture_output=True, text=True, env=env, stdin=subprocess.DEVNULL, timeout=60)

    def test_status_without_a_lab_and_errors_go_to_stderr(self) -> None:
        with tempfile.TemporaryDirectory(prefix="bmac-lab-cli-") as scratch:
            root = Path(scratch, "lab")
            status = self.lab("status", root=root)
            self.assertEqual(status.returncode, 0, status.stderr)
            self.assertIn(f"No lab at {root}", status.stdout)
            reset = self.lab("reset", "installed", root=root)
            self.assertEqual(reset.returncode, 1)
            self.assertIn("lab: no lab at", reset.stderr)
            unknown = self.lab("--config", str(Path(scratch, "none.toml")), "status", root=root)
            self.assertIn("none.toml does not exist", unknown.stderr)


if __name__ == "__main__":
    unittest.main()
