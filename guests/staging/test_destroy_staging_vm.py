# Copyright (c) 2026 BEENTHERE VENTURES, INC.
# SPDX-License-Identifier: GPL-3.0-only

from __future__ import annotations

import json
import os
import shlex
import subprocess
import tempfile
import textwrap
import unittest
from pathlib import Path


SCRIPT = Path(__file__).with_name("destroy_staging_vm.sh")


class DestroyStagingVmTests(unittest.TestCase):
    def run_sourced(
        self,
        body: str,
        *,
        environment: dict[str, str] | None = None,
        expected: int | None = 0,
    ) -> subprocess.CompletedProcess[str]:
        env = os.environ.copy()
        env["APP_HA_DESTROY_STAGING_SOURCE_ONLY"] = "1"
        if environment:
            env.update(environment)
        completed = subprocess.run(
            [
                "bash",
                "-c",
                f"source {SCRIPT!s}\n{textwrap.dedent(body)}",
            ],
            check=False,
            capture_output=True,
            text=True,
            env=env,
        )
        if expected is not None:
            self.assertEqual(
                completed.returncode,
                expected,
                msg=f"stdout:\n{completed.stdout}\nstderr:\n{completed.stderr}",
            )
        return completed

    def test_help_and_shell_syntax(self) -> None:
        subprocess.run(["bash", "-n", str(SCRIPT)], check=True)
        completed = subprocess.run(
            [str(SCRIPT), "--help"],
            check=False,
            capture_output=True,
            text=True,
        )
        self.assertEqual(completed.returncode, 0, msg=completed.stderr)
        self.assertIn("--dry-run", completed.stdout)
        self.assertIn("[stageNprodN]", completed.stdout)
        self.assertIn("No other production snapshot is touched", completed.stdout)

    def test_resource_listing_is_interactive_and_staging_only(self) -> None:
        resources = [
            {
                "kind": "production",
                "name": "prod1",
                "source": None,
                "index": 1,
                "state": "active",
                "placement": ["mox1", "mox2"],
                "domains": {"primary": "example.com"},
            },
            {
                "kind": "staging",
                "name": "stage1prod1",
                "source": "prod1",
                "index": 1,
                "state": "active",
                "placement": ["mox2"],
                "domains": {"primary": "stage1prod1.example.com"},
            },
        ]
        completed = self.run_sourced(
            f"""
RESOURCE_NAME=stage1prod1
select_resource {shlex.quote(json.dumps(resources))}
"""
        )
        self.assertIn("stage1prod1", completed.stdout)
        self.assertFalse(
            any(
                line.strip().startswith("prod1 ")
                for line in completed.stdout.splitlines()
            )
        )

    CLEANUP_ROW = {
        "name": "stage1prod1",
        "id": "11111111-2222-4333-8444-555555555555",
        "state": "cleanup_pending",
        "routes_enabled": False,
    }

    def run_cleanup_wait(
        self, registry_body: str
    ) -> subprocess.CompletedProcess[str]:
        # sleep advances Bash's SECONDS, so the 1200 second deadline is reached
        # in a few polls without waiting.
        return self.run_sourced(
            f"""
CLUSTER_NODES=(mox1)
RESOURCE_NAME=stage1prod1
RESOURCE_ID={self.CLEANUP_ROW["id"]}
node_exec() {{ return 0; }}
sleep() {{ SECONDS=$((SECONDS + 500)); }}
registry_cmd() {{
{registry_body}
}}
wait_for_durable_cleanup
echo "RETURNED=$?"
""",
            expected=None,
        )

    def test_cleanup_wait_accepts_only_a_successful_listing_without_the_name(
        self,
    ) -> None:
        other = dict(self.CLEANUP_ROW, name="stage2prod1", state="active")
        completed = self.run_cleanup_wait(
            f"printf '%s\\n' {shlex.quote(json.dumps([other]))}"
        )
        self.assertIn("RETURNED=0", completed.stdout)

        empty = self.run_cleanup_wait("printf '[]\\n'")
        self.assertIn("RETURNED=0", empty.stdout)

    def test_cleanup_wait_never_treats_a_failed_query_as_deleted(self) -> None:
        failures = {
            "ssh failure": "return 255",
            "registry error": 'echo "ERROR: lock timeout" >&2; return 1',
            "empty success": "return 0",
            "not json": "printf 'not json\\n'",
            "not a list": "printf '{}\\n'",
        }
        for label, body in failures.items():
            with self.subTest(failure=label):
                completed = self.run_cleanup_wait(body)
                self.assertNotIn("RETURNED=0", completed.stdout)
                self.assertNotEqual(completed.returncode, 0)
                self.assertIn(
                    "Timed out without confirming staging cleanup",
                    completed.stderr,
                )
                self.assertIn("cleanup is unconfirmed", completed.stderr)

    def test_cleanup_wait_polls_through_pending_and_transient_failures(
        self,
    ) -> None:
        pending = shlex.quote(json.dumps([self.CLEANUP_ROW]))
        with tempfile.TemporaryDirectory() as temporary:
            # registry_cmd runs in a command substitution, so count polls in a
            # file: pending, then a dropped connection, then really gone.
            polls = shlex.quote(str(Path(temporary) / "polls"))
            completed = self.run_cleanup_wait(
                f"""
  [[ "$1" == list && $# == 1 ]] || return 2
  echo x >>{polls}
  case "$(grep -c x {polls})" in
    1) printf '%s\\n' {pending} ;;
    2) return 255 ;;
    *) printf '[]\\n' ;;
  esac
"""
            )
        self.assertIn("RETURNED=0", completed.stdout)
        self.assertIn("cleanup is unconfirmed", completed.stderr)

    def test_cleanup_wait_dies_when_the_name_now_means_something_else(
        self,
    ) -> None:
        for label, changed in {
            "reused name": dict(self.CLEANUP_ROW, id="99999999-2222-4333-8444-555555555555"),
            "left cleanup": dict(self.CLEANUP_ROW, state="active"),
            "routes back": dict(self.CLEANUP_ROW, routes_enabled=True),
        }.items():
            with self.subTest(change=label):
                completed = self.run_cleanup_wait(
                    f"printf '%s\\n' {shlex.quote(json.dumps([changed]))}"
                )
                self.assertNotIn("RETURNED=0", completed.stdout)
                self.assertIn(
                    "resource identity or state changed", completed.stderr
                )

    def test_pending_cleanup_times_out_as_pending_not_unconfirmed(self) -> None:
        completed = self.run_cleanup_wait(
            f"printf '%s\\n' {shlex.quote(json.dumps([self.CLEANUP_ROW]))}"
        )
        self.assertNotIn("RETURNED=0", completed.stdout)
        self.assertIn(
            "Timed out waiting for durable staging cleanup", completed.stderr
        )
        self.assertNotIn("without confirming", completed.stderr)

    def test_destructive_order_and_exact_snapshot_scope(self) -> None:
        text = SCRIPT.read_text(encoding="utf-8")
        main = text[text.index("main() {") :]
        self.assertLess(
            main.index('registry_update --state stopping --routes-disabled'),
            main.index("queue_cleanup_plan"),
        )
        self.assertLess(
            main.index("queue_cleanup_plan"),
            main.index("wait_for_durable_cleanup"),
        )
        self.assertIn(
            '--target "vm-${SOURCE_VMID}@${SNAPSHOT_NAME}"',
            text,
        )
        self.assertIn("--action destroy-vm", text)
        self.assertIn("--action destroy-volume", text)
        self.assertIn("--action delete-snapshot", text)
        self.assertIn("--action remove-route", text)
        self.assertIn("--state cleanup_pending", text)
        self.assertIn("app-ha-deferred-cleanup.service", text)
        self.assertIn("replicated copies", text)
        self.assertNotIn("zfs destroy -r", text)
        self.assertNotIn("zfs destroy -R", text)
        self.assertNotIn('qm delsnapshot "$SOURCE_VMID"', main)

    def test_managed_workstation_alias_removal_preserves_unrelated_config(
        self,
    ) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            home = Path(temporary) / "home"
            ssh_dir = home / ".ssh"
            ssh_dir.mkdir(parents=True, mode=0o700)
            config = ssh_dir / "config"
            config.write_text(
                "# BEGIN app-ha managed staging guest stage1prod1\n"
                "Host stage1prod1\n"
                "    HostName 10.213.0.51\n"
                "# END app-ha managed staging guest stage1prod1\n"
                "\n"
                "Host unrelated\n"
                "    HostName 192.0.2.10\n",
                encoding="utf-8",
            )
            run_dir = Path(temporary) / "run"
            run_dir.mkdir()
            self.run_sourced(
                f"""
RUN_DIR={str(run_dir)!r}
SSH_ALIAS=stage1prod1
remove_workstation_alias
""",
                environment={"HOME": str(home)},
            )
            updated = config.read_text(encoding="utf-8")
            self.assertNotIn("app-ha managed staging guest", updated)
            self.assertNotIn("Host stage1prod1", updated)
            self.assertIn("Host unrelated", updated)


if __name__ == "__main__":
    unittest.main()
