#!/usr/bin/env python3

# Copyright (c) 2026 BEENTHERE VENTURES, INC.
# SPDX-License-Identifier: GPL-3.0-only

"""Structure checks for the full read-only cluster report."""

from __future__ import annotations

from pathlib import Path
import re
import subprocess
import unittest

SCRIPT = (
    Path(__file__).resolve().parent.parent
    / "scripts" / "user_callable" / "diagnostics" / "show_cluster_state.sh"
)


class ShowClusterStateTest(unittest.TestCase):
    def setUp(self) -> None:
        self.source = SCRIPT.read_text(encoding="utf-8")
        self.top = re.sub(r"<<'(\w+)'\n.*?\n\1\n", "\n", self.source, flags=re.S)

    def test_syntax(self) -> None:
        subprocess.run(["bash", "-n", str(SCRIPT)], check=True)

    def test_members_come_from_this_hosts_cluster_view(self) -> None:
        self.assertIn('for host in "${CONTROL_MEMBER_NODES[@]}"; do', self.top)
        self.assertNotIn("mox{1..10}", self.top)

    def test_hosts_and_qdevice_are_reached_only_through_the_shared_helpers(self) -> None:
        for line in self.top.splitlines():
            if re.match(r"\s*(\S+=\S+\s+)*ssh\s|.*\$\(ssh\s", line):
                self.fail(f"direct ssh outside a heredoc: {line.strip()}")
        self.assertIn("qd_ssh 'bash -s' <<'QDEVICE'", self.source)
        self.assertIn('mox_ssh "$host" bash -s --', self.source)
        self.assertIn('mox_ssh "$registry_host" bash -s', self.source)

    def test_runs_without_a_terminal_or_an_acknowledgement(self) -> None:
        self.assertNotIn("-t 0", self.top)
        self.assertNotIn("bmac_ui_manual_action", self.top)
        self.assertNotIn("read -r", self.top)


if __name__ == "__main__":
    unittest.main()
