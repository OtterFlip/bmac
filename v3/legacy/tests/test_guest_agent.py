#!/usr/bin/env python3

# Copyright (c) 2026 BEENTHERE VENTURES, INC.
# SPDX-License-Identifier: GPL-3.0-only

"""Tests for running guest commands through the QEMU guest agent."""

from __future__ import annotations

import json
from pathlib import Path
import subprocess
import tempfile
import unittest


SCRIPTS = Path(__file__).resolve().parents[1] / "scripts"
GUEST_AGENT = SCRIPTS / "lib" / "guest_agent.sh"

# Stands in for `qm guest exec` on the host: runs the argv after `--` and
# prints the agent's JSON result, forwarding stdin only with --pass-stdin 1.
FAKE_MOX_SSH = r'''
mox_ssh() {
  printf '%s\n' "$*" >>"$CALLS"
  [[ "$2 $3 $4" == "qm guest exec" ]] || return 1
  shift 5
  local pass=0
  while [[ "$1" != -- ]]; do
    case "$1" in
      --timeout) shift 2 ;;
      --pass-stdin) pass="$2"; shift 2 ;;
      *) return 1 ;;
    esac
  done
  shift
  [[ -z "${AGENT_RESULT:-}" ]] || { printf '%s' "$AGENT_RESULT"; return; }
  local input=""
  if ((pass)); then input="$(cat; printf x)"; input="${input%x}"; fi
  python3 - "$input" "$@" <<'PY'
import json, subprocess, sys
done = subprocess.run(sys.argv[2:], input=sys.argv[1], capture_output=True, text=True)
print(json.dumps({"exited": 1, "exitcode": done.returncode, "out-data": done.stdout, "err-data": done.stderr}))
PY
}
'''


class GuestAgentRunTest(unittest.TestCase):
    def run_shell(self, body: str, agent_result: str = "", stdin: str = "") -> tuple[subprocess.CompletedProcess[str], str]:
        with tempfile.TemporaryDirectory() as directory:
            calls = Path(directory) / "calls"
            completed = subprocess.run(
                ["bash", "-c", f"source {GUEST_AGENT}\n{FAKE_MOX_SSH}\n{body}"],
                env={"PATH": "/usr/bin:/bin", "CALLS": str(calls), "AGENT_RESULT": agent_result},
                input=stdin,
                text=True,
                capture_output=True,
                timeout=60,
                check=False,
            )
            return completed, calls.read_text(encoding="utf-8") if calls.exists() else ""

    def test_output_and_exit_status_come_from_the_guest_command(self) -> None:
        completed, calls = self.run_shell(
            "guest_agent_run mox2 101 30 bash -c 'echo out; echo err >&2; exit 3' || echo \"STATUS=$?\""
        )
        self.assertEqual(completed.stdout, "out\nSTATUS=3\n")
        self.assertEqual(completed.stderr, "err\n")
        self.assertIn("mox2 qm guest exec 101 --timeout 30 -- bash -c", calls)
        self.assertNotIn("--pass-stdin", calls)

    def test_stdin_reaches_the_guest_only_when_asked(self) -> None:
        completed, calls = self.run_shell(
            "guest_agent_run --stdin mox1 101 30 cat\nguest_agent_run mox1 101 30 cat; echo END",
            stdin="from the host\n",
        )
        self.assertEqual(completed.returncode, 0, completed.stderr)
        self.assertEqual(completed.stdout, "from the host\nEND\n")
        self.assertIn("--pass-stdin 1", calls)

    def test_unfinished_truncated_or_malformed_results_fail(self) -> None:
        for result, status in (
            (json.dumps({"exited": 0}), 124),
            (json.dumps({"exited": 1, "exitcode": 0, "out-truncated": True}), 124),
            ("not json", 125),
        ):
            with self.subTest(result=result):
                completed, _ = self.run_shell(
                    'guest_agent_run mox1 101 30 true || echo "STATUS=$?"', agent_result=result
                )
                self.assertIn(f"STATUS={status}", completed.stdout)

    def test_arguments_are_validated_before_any_call(self) -> None:
        for arguments in ("pve1 101 30 true", "mox1 0 30 true", "mox1 101 0 true", "mox1 101 30"):
            with self.subTest(arguments=arguments):
                completed, calls = self.run_shell(f'guest_agent_run {arguments} || echo "STATUS=$?"')
                self.assertIn("STATUS=2", completed.stdout)
                self.assertEqual(calls, "")


if __name__ == "__main__":
    unittest.main()
