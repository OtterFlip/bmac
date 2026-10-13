#!/usr/bin/env python3

# Copyright (c) 2026 BEENTHERE VENTURES, INC.
# SPDX-License-Identifier: GPL-3.0-only

"""Tests for reading the cluster's membership on the host the script runs on."""

from __future__ import annotations

from pathlib import Path
import subprocess
import tempfile
import textwrap
import unittest


REPO_ROOT = Path(__file__).resolve().parents[1]
CONTROL_LIB = REPO_ROOT / "scripts" / "lib" / "cluster_control.sh"


class ControlFindClusterTest(unittest.TestCase):
    def run_find(self, this_host: str, nodes: str, in_cluster: bool = True) -> subprocess.CompletedProcess[str]:
        script = textwrap.dedent(
            f"""
            set -euo pipefail
            source {CONTROL_LIB}
            MAX_MOX_HOSTS=4
            this_mox() {{ [[ -n {this_host!r} ]] && printf '%s\\n' {this_host!r}; }}
            mox_ssh() {{
              printf '%s\\n' "$1" >>"$CALLS"
              {'true' if in_cluster else 'return 1'}
              printf '%s' {nodes!r}
            }}
            status=0
            control_find_cluster || status=$?
            printf 'STATUS=%s PROBE=%s MEMBERS=%s ONLINE=%s\\n' \\
              "$status" "$CONTROL_PROBE_NODE" "${{CONTROL_MEMBER_NODES[*]}}" "${{CONTROL_ONLINE_NODES[*]}}"
            printf 'CALLS=%s\\n' "$(paste -sd, "$CALLS" 2>/dev/null)"
            """
        )
        with tempfile.TemporaryDirectory() as directory:
            return subprocess.run(
                ["bash", "-c", script],
                env={"PATH": "/usr/bin:/bin", "CALLS": f"{directory}/calls"},
                text=True,
                capture_output=True,
                timeout=60,
                check=False,
            )

    def test_membership_is_read_on_this_host_only(self) -> None:
        completed = self.run_find(
            "mox3", '[{"node":"mox3","status":"online"},{"node":"mox1","status":"offline"}]'
        )
        self.assertEqual(completed.returncode, 0, completed.stderr)
        self.assertIn("STATUS=0 PROBE=mox3 MEMBERS=mox1 mox3 ONLINE=mox3", completed.stdout)
        self.assertIn("CALLS=mox3\n", completed.stdout)

    def test_a_host_outside_a_cluster_reports_no_cluster(self) -> None:
        completed = self.run_find("mox2", "[]", in_cluster=False)
        self.assertEqual(completed.returncode, 0, completed.stderr)
        self.assertIn("STATUS=1 PROBE= ", completed.stdout)

    def test_unknown_host_or_malformed_membership_is_an_error(self) -> None:
        unknown = self.run_find("", '[{"node":"mox1","status":"online"}]')
        self.assertIn("STATUS=2 PROBE= ", unknown.stdout)
        self.assertIn("CALLS=\n", unknown.stdout)
        foreign = self.run_find("mox1", '[{"node":"pve9","status":"online"}]')
        self.assertIn("STATUS=2", foreign.stdout)


if __name__ == "__main__":
    unittest.main()
