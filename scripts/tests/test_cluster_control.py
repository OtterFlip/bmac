#!/usr/bin/env python3

# Copyright (c) 2026 BEENTHERE VENTURES, INC.
# SPDX-License-Identifier: GPL-3.0-only

"""Tests for finding the cluster through the host slots that answer SSH."""

from __future__ import annotations

from pathlib import Path
import subprocess
import tempfile
import textwrap
import time
import unittest


tempfile.tempdir = str(Path(tempfile.gettempdir()).resolve())

REPO_ROOT = Path(__file__).resolve().parents[2]
CONTROL_LIB = REPO_ROOT / "scripts" / "lib" / "cluster_control.sh"


class ControlFindClusterTest(unittest.TestCase):
    def run_find(self, max_hosts: int, timeout: int, reachable: str) -> tuple[subprocess.CompletedProcess[str], float]:
        with tempfile.TemporaryDirectory() as directory:
            script = textwrap.dedent(
                f"""
                set -euo pipefail
                source {CONTROL_LIB}
                WORK={directory}
                MAX_MOX_HOSTS={max_hosts}
                CONTROL_PROBE_TIMEOUT_SECONDS={timeout}
                mox_is_reachable() {{
                {textwrap.indent(textwrap.dedent(reachable), "  ")}
                }}
                mox_ssh() {{
                  local node="$1"
                  printf '[{{"node":"%s","status":"online"}},{{"node":"mox%s","status":"offline"}}]' \\
                    "$node" "$MAX_MOX_HOSTS"
                }}
                status=0
                control_find_cluster || status=$?
                printf 'STATUS=%s PROBE=%s MEMBERS=%s ONLINE=%s\\n' \\
                  "$status" "$CONTROL_PROBE_NODE" "${{CONTROL_MEMBER_NODES[*]}}" "${{CONTROL_ONLINE_NODES[*]}}"
                for pid_file in "$WORK"/hung.*.pid; do
                  [[ -f "$pid_file" ]] || continue
                  ! kill -0 "$(cat "$pid_file")" 2>/dev/null || echo HUNG_PROBE_SURVIVED
                done
                """
            )
            started = time.monotonic()
            completed = subprocess.run(
                ["bash", "-c", script], text=True, capture_output=True, timeout=60, check=False
            )
            return completed, time.monotonic() - started

    def test_slots_are_probed_at_once(self) -> None:
        completed, elapsed = self.run_find(10, 15, 'sleep 1; [[ "$1" == mox7 ]]')
        self.assertEqual(completed.returncode, 0, completed.stderr)
        self.assertIn("STATUS=0 PROBE=mox7 MEMBERS=mox7 mox10 ONLINE=mox7", completed.stdout)
        self.assertLess(elapsed, 6, "ten one-second probes must not run one after another")

    def test_no_reachable_slot_reports_no_cluster(self) -> None:
        completed, _ = self.run_find(3, 15, "return 1")
        self.assertEqual(completed.returncode, 0, completed.stderr)
        self.assertIn("STATUS=1 PROBE= ", completed.stdout)

    def test_a_hung_probe_is_stopped_at_the_deadline(self) -> None:
        completed, elapsed = self.run_find(
            4,
            1,
            """
            case "$1" in
              mox1) sleep 30 & echo "$!" >"$WORK/hung.$1.pid"; wait ;;
              mox3) return 0 ;;
              *) return 1 ;;
            esac
            """,
        )
        self.assertEqual(completed.returncode, 0, completed.stderr)
        self.assertIn("STATUS=0 PROBE=mox3", completed.stdout)
        self.assertNotIn("HUNG_PROBE_SURVIVED", completed.stdout)
        self.assertEqual(completed.stderr, "")
        self.assertLess(elapsed, 10)

    def test_a_reachable_low_slot_does_not_wait_for_later_slots(self) -> None:
        completed, elapsed = self.run_find(
            4,
            15,
            """
            [[ "$1" != mox1 ]] || return 0
            sleep 30 & echo "$!" >"$WORK/hung.$1.pid"; wait
            """,
        )
        self.assertEqual(completed.returncode, 0, completed.stderr)
        self.assertIn("STATUS=0 PROBE=mox1", completed.stdout)
        self.assertNotIn("HUNG_PROBE_SURVIVED", completed.stdout)
        self.assertEqual(completed.stderr, "")
        self.assertLess(elapsed, 10)

    def test_lowest_reachable_slot_is_used(self) -> None:
        completed, _ = self.run_find(5, 15, '[[ "$1" == mox2 || "$1" == mox4 ]]')
        self.assertEqual(completed.returncode, 0, completed.stderr)
        self.assertIn("PROBE=mox2", completed.stdout)


if __name__ == "__main__":
    unittest.main()
