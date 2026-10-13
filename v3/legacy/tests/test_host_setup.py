#!/usr/bin/env python3

# Copyright (c) 2026 BEENTHERE VENTURES, INC.
# SPDX-License-Identifier: GPL-3.0-only

"""Tests for the host setup job's scripts in scripts/host_setup/."""

from __future__ import annotations

from pathlib import Path
import subprocess
import tempfile
import unittest


SETUP_DIR = Path(__file__).resolve().parent.parent / "scripts" / "host_setup"
ORCHESTRATE = SETUP_DIR / "orchestrate_host.sh"
VERIFY = SETUP_DIR / "verify_host.sh"


class HostSetupScriptTest(unittest.TestCase):
    def test_the_scripts_parse(self) -> None:
        for script in (ORCHESTRATE, VERIFY):
            with self.subTest(script=script.name):
                subprocess.run(["bash", "-n", str(script)], check=True)

    def test_verification_needs_the_encryption_choice(self) -> None:
        for args in ([], ["maybe"], ["luks", "none"]):
            with self.subTest(args=args), tempfile.TemporaryDirectory() as work:
                result = subprocess.run(
                    ["bash", str(VERIFY), *args],
                    cwd=work,
                    env={"PATH": "/usr/bin:/bin", "BMAC_HOST": "mox1"},
                    stdin=subprocess.DEVNULL,
                    capture_output=True,
                    text=True,
                    check=False,
                )
                self.assertEqual(result.returncode, 2, result.stderr)
                self.assertIn("Usage: verify_host.sh luks|none", result.stderr)

    def test_ssh_never_reads_a_loops_input(self) -> None:
        # An ssh in a `while read` loop otherwise swallows the rest of the
        # list, and the loop silently stops after one remote host.
        for script in (ORCHESTRATE, VERIFY):
            with self.subTest(script=script.name):
                for number, line in enumerate(script.read_text(encoding="utf-8").splitlines(), 1):
                    words = line.split()
                    if words[:1] == ["ssh"]:
                        self.assertTrue("-n" in words or "</dev/null" in line,
                                        f"{script.name}:{number}: {line.strip()}")

    def test_no_v2_names_remain(self) -> None:
        for script in (ORCHESTRATE, VERIFY):
            with self.subTest(script=script.name):
                text = script.read_text(encoding="utf-8")
                for name in ("app-ha", "APP_HA", "app_ha"):
                    self.assertNotIn(name, text)


if __name__ == "__main__":
    unittest.main()
