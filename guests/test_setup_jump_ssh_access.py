# Copyright (c) 2026 BEENTHERE VENTURES, INC.
# SPDX-License-Identifier: GPL-3.0-only

import json
import subprocess
import tempfile
import unittest
from pathlib import Path

SCRIPT = Path(__file__).resolve().parent / "setup_jump_ssh_access.sh"

HOST_KEY = (
    "ssh-ed25519 "
    "AAAAC3NzaC1lZDI1NTE5AAAAIBhE1+iRU5AP8b2hmfLfAHJQ/BVRAR0n4ddx0DFCnkg4"
)
OLD_KEY = (
    "ssh-ed25519 "
    "AAAAC3NzaC1lZDI1NTE5AAAAIOMqqnkVzrm0SdG6UOoqKLsabgH5C9okWi0dh2l1GKJl"
)

STALE_CONFIG = """\
# BEGIN app-ha managed production guest prod1
Host prod1
    HostName 10.213.0.31
    User root
    Port 22
    ProxyJump mox1
    HostKeyAlias prod1
    StrictHostKeyChecking yes
    PasswordAuthentication no
    KbdInteractiveAuthentication no
# END app-ha managed production guest prod1

Host mox3
    HostName 100.64.0.3
    User root

Host prod1
    User nate
"""


class SetupJumpSshAccessTest(unittest.TestCase):
    def run_sourced(
        self, body: str, *, stdin: str = "", expected: int = 0
    ) -> subprocess.CompletedProcess[str]:
        completed = subprocess.run(
            [
                "bash",
                "-c",
                'SETUP_JUMP_SSH_SOURCE_ONLY=1 source "$1"; shift; ' + body,
                "bash",
                str(SCRIPT),
            ],
            input=stdin,
            text=True,
            capture_output=True,
            check=False,
        )
        self.assertEqual(
            completed.returncode,
            expected,
            msg=f"stdout:\n{completed.stdout}\nstderr:\n{completed.stderr}",
        )
        return completed

    def configure_body(self, home: Path, run_dir: Path, ssh_result: int) -> str:
        return f"""
HOME={str(home)!r}
RUN_DIR={str(run_dir)!r}
RESOURCE_NAME=prod1
GUEST_KIND=production
GUEST_IP=10.213.0.31
JUMP_HOST=mox3
GUEST_KEY_CORE={HOST_KEY!r}
GUEST_FINGERPRINT=SHA256:test
ssh() {{
  if [[ "${{1:-}}" == -G ]]; then
    printf '%s\\n' 'hostname 10.213.0.31' 'user root' 'port 22' \\
      'proxyjump mox3' 'hostkeyalias prod1' 'stricthostkeychecking true'
    return 0
  fi
  return {ssh_result}
}}
configure_alias
"""

    def make_home(self, root: Path) -> tuple[Path, Path]:
        home = root / "home"
        run_dir = root / "run"
        (home / ".ssh").mkdir(parents=True)
        run_dir.mkdir()
        (home / ".ssh" / "config").write_text(STALE_CONFIG, encoding="utf-8")
        (home / ".ssh" / "known_hosts").write_text(
            f"mox3 {OLD_KEY}\nprod1 {OLD_KEY}\n", encoding="utf-8"
        )
        return home, run_dir

    def test_replaces_stale_jump_host_and_host_key_pin(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            home, run_dir = self.make_home(Path(temporary))
            self.run_sourced(
                self.configure_body(home, run_dir, 0), stdin="\n\n"
            )
            config = (home / ".ssh" / "config").read_text(encoding="utf-8")
            known_hosts = (home / ".ssh" / "known_hosts").read_text(
                encoding="utf-8"
            )
            self.assertTrue(
                config.startswith(
                    "# BEGIN app-ha managed production guest prod1\n"
                    "Host prod1\n"
                    "    HostName 10.213.0.31\n"
                    "    User root\n"
                    "    Port 22\n"
                    "    ProxyJump mox3\n"
                )
            )
            self.assertNotIn("ProxyJump mox1", config)
            self.assertEqual(
                config.count("# BEGIN app-ha managed production guest prod1"), 1
            )
            self.assertIn("Host mox3\n    HostName 100.64.0.3\n", config)
            self.assertIn("Host prod1\n    User nate\n", config)
            self.assertEqual(
                known_hosts, f"mox3 {OLD_KEY}\nprod1 {HOST_KEY}\n"
            )

    def test_failed_validation_restores_prior_files(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            home, run_dir = self.make_home(Path(temporary))
            completed = self.run_sourced(
                self.configure_body(home, run_dir, 255),
                stdin="\n\n",
                expected=1,
            )
            self.assertIn("authorized_keys", completed.stderr)
            self.assertEqual(
                (home / ".ssh" / "config").read_text(encoding="utf-8"),
                STALE_CONFIG,
            )
            self.assertEqual(
                (home / ".ssh" / "known_hosts").read_text(encoding="utf-8"),
                f"mox3 {OLD_KEY}\nprod1 {OLD_KEY}\n",
            )

    def test_declining_leaves_files_untouched(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            home, run_dir = self.make_home(Path(temporary))
            self.run_sourced(
                self.configure_body(home, run_dir, 0),
                stdin="\nn\n",
                expected=1,
            )
            self.assertEqual(
                (home / ".ssh" / "config").read_text(encoding="utf-8"),
                STALE_CONFIG,
            )

    def test_lists_production_and_staging_with_live_state(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            run_dir = Path(temporary)
            (run_dir / "resources.json").write_text(
                json.dumps(
                    [
                        {"kind": "staging", "name": "stage1prod1", "vmid": 200,
                         "ip": "10.213.0.61", "state": "stopped"},
                        {"kind": "production", "name": "prod1", "vmid": 100,
                         "ip": "10.213.0.31", "state": "active"},
                        {"kind": "production", "name": "prod2", "vmid": 101,
                         "ip": None, "state": "reserved"},
                    ]
                ),
                encoding="utf-8",
            )
            (run_dir / "cluster-vms.json").write_text(
                json.dumps(
                    [
                        {"type": "qemu", "vmid": 100, "name": "prod1",
                         "node": "mox3", "status": "running"},
                        {"type": "qemu", "vmid": 200, "name": "stage1prod1",
                         "node": "mox2", "status": "stopped"},
                    ]
                ),
                encoding="utf-8",
            )
            completed = self.run_sourced(
                f"""
RUN_DIR={str(run_dir)!r}
select_guest
printf 'SELECTED %s %s %s %s\\n' "$RESOURCE_NAME" "$GUEST_KIND" "$GUEST_NODE" "$GUEST_IP"
""",
                stdin="\n",
            )
            self.assertIn("stage1prod1", completed.stdout)
            self.assertNotIn("prod2", completed.stdout)
            self.assertIn("SELECTED prod1 production mox3 10.213.0.31", completed.stdout)

            stopped = self.run_sourced(
                f"""
RUN_DIR={str(run_dir)!r}
RESOURCE_NAME=stage1prod1
select_guest
""",
                expected=1,
            )
            self.assertIn("is not running", stopped.stderr)


if __name__ == "__main__":
    unittest.main()
