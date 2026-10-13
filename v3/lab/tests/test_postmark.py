# Copyright (c) 2026 BEENTHERE VENTURES, INC.
# SPDX-License-Identifier: GPL-3.0-only

"""`lab/lab postmark` hands the lab's Postmark token to the template tool
through its environment, never its arguments."""

from __future__ import annotations

import subprocess
import sys
import tempfile
import unittest
from pathlib import Path
from unittest import mock

LAB = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(LAB))

from bmaclab import cli  # noqa: E402
from bmaclab.tailnet import TailnetError  # noqa: E402

TOKEN = "not-a-real-postmark-token"


class PostmarkTest(unittest.TestCase):
    def secrets(self, text: str) -> Path:
        scratch = tempfile.TemporaryDirectory(prefix="bmac-lab-postmark-")
        self.addCleanup(scratch.cleanup)
        path = Path(scratch.name) / "secrets.env"
        path.write_text(text)
        path.chmod(0o600)
        return path

    def test_the_token_goes_in_the_environment(self) -> None:
        path = self.secrets(f"{cli.POSTMARK_KEY}={TOKEN}\n")
        with mock.patch.object(subprocess, "run", return_value=subprocess.CompletedProcess([], 3)) as run:
            self.assertEqual(cli.postmark(path, ["list"]), 3)
        argv = run.call_args.args[0]
        self.assertEqual(argv[1:], [str(cli.POSTMARK_TOOL), "list"])
        self.assertNotIn(TOKEN, " ".join(argv))
        self.assertEqual(run.call_args.kwargs["env"]["POSTMARK_SERVER_TOKEN"], TOKEN)
        self.assertTrue(cli.POSTMARK_TOOL.is_file())

    def test_the_confirmed_sender_goes_in_the_environment(self) -> None:
        path = self.secrets(f"{cli.POSTMARK_KEY}={TOKEN}\n{cli.POSTMARK_SENDER}=lab@example.com\n")
        with mock.patch.object(subprocess, "run", return_value=subprocess.CompletedProcess([], 0)) as run:
            cli.postmark(path, ["send", "bmac-test-v1"])
        self.assertEqual(run.call_args.kwargs["env"]["POSTMARK_SENDER"], "lab@example.com")

    def test_a_missing_key_is_named(self) -> None:
        path = self.secrets("OTHER=x\n")
        with self.assertRaisesRegex(TailnetError, cli.POSTMARK_KEY):
            cli.postmark(path, ["list"])


if __name__ == "__main__":
    unittest.main()
