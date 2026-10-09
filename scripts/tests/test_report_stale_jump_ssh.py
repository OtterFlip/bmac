# Copyright (c) 2026 BEENTHERE VENTURES, INC.
# SPDX-License-Identifier: GPL-3.0-only

import subprocess
import tempfile
import unittest
from pathlib import Path

SCRIPT = Path(__file__).resolve().parent.parent / "utilities" / "report_stale_jump_ssh.sh"

CONFIG = """\
# BEGIN app-ha managed production guest prod1
Host prod1
    HostName 10.213.0.31
    User root
    ProxyJump mox1
# END app-ha managed production guest prod1

# BEGIN app-ha managed staging guest stage1prod1
Host stage1prod1
    HostName 10.213.0.61
    ProxyJump mox2
# END app-ha managed staging guest stage1prod1

# BEGIN app-ha managed production guest p2
Host p2
    HostName 10.213.0.32
    ProxyJump root@mox1.internal:22
# END app-ha managed production guest p2

# BEGIN app-ha managed production guest prod4
Host prod4
    HostName 10.213.0.34
    ProxyJump mox10
# END app-ha managed production guest prod4

Host prod3
    HostName 10.213.0.33
    ProxyJump=qdevice,mox1

Host web
    ProxyJump mox1
"""


class ReportStaleJumpSshTest(unittest.TestCase):
    def run_report(self, home: Path, host: str) -> subprocess.CompletedProcess[str]:
        completed = subprocess.run(
            ["bash", str(SCRIPT), host],
            env={"HOME": str(home), "PATH": "/usr/bin:/bin"},
            text=True,
            capture_output=True,
            check=False,
        )
        self.assertEqual(completed.returncode, 0, completed.stderr)
        return completed

    def test_lists_only_guest_aliases_jumping_through_removed_host(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            home = Path(temporary)
            (home / ".ssh").mkdir()
            (home / ".ssh" / "config").write_text(CONFIG, encoding="utf-8")
            output = self.run_report(home, "mox1").stdout
            self.assertIn("ACTION NEEDED", output)
            self.assertIn("scripts/user_callable/guests/setup_jump_ssh_access.sh prod1\n", output)
            self.assertIn("scripts/user_callable/guests/setup_jump_ssh_access.sh prod3\n", output)
            self.assertIn("pick the guest at 10.213.0.32, enter alias p2", output)
            self.assertNotIn("stage1prod1", output)
            self.assertNotIn("prod4", output)
            self.assertNotIn("web", output)

            clean = self.run_report(home, "mox3").stdout
            self.assertIn("No ~/.ssh/config guest aliases", clean)
            self.assertNotIn("ACTION NEEDED", clean)

    def test_missing_config_is_silent_success(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            completed = self.run_report(Path(temporary), "mox1")
            self.assertEqual(completed.stdout, "")


if __name__ == "__main__":
    unittest.main()
