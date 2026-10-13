# Copyright (c) 2026 BEENTHERE VENTURES, INC.
# SPDX-License-Identifier: GPL-3.0-only

from pathlib import Path
import subprocess
import unittest


SCRIPT = (
    Path(__file__).resolve().parent.parent
    / "scripts"
    / "user_callable"
    / "diagnostics"
    / "show_prod_vm_state.sh"
)


class ShowProdVmStateTest(unittest.TestCase):
    def test_shell_syntax_and_help(self) -> None:
        syntax = subprocess.run(
            ["bash", "-n", str(SCRIPT)], text=True, capture_output=True, check=False
        )
        self.assertEqual(syntax.returncode, 0, syntax.stderr)
        completed = subprocess.run(
            [str(SCRIPT), "--help"], text=True, capture_output=True, check=False
        )
        self.assertEqual(completed.returncode, 0, completed.stderr)
        self.assertIn("QEMU guest agent", completed.stdout)

    def test_the_guest_is_inspected_only_through_the_guest_agent(self) -> None:
        text = SCRIPT.read_text(encoding="utf-8")
        for absent in ("ssh_via_mox", "known_hosts", "ssh -o", "this workstation"):
            self.assertNotIn(absent, text)
        self.assertEqual(text.count("guest_agent_run --stdin"), 2)
        self.assertIn('guest_agent_ping "$OWNER_NODE" "$VMID"', text)


if __name__ == "__main__":
    unittest.main()
