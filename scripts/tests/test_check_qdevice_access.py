#!/usr/bin/env python3

# Copyright (c) 2026 BEENTHERE VENTURES, INC.
# SPDX-License-Identifier: GPL-3.0-only

"""Fake-SSH tests for the QDevice access check the dashboard runs before the first host."""

from __future__ import annotations

from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import unittest


SCRIPT = Path(__file__).resolve().parent.parent / "user_callable" / "diagnostics" / "check_qdevice_access.sh"
sys.path.insert(0, str(Path(__file__).resolve().parent))
from ui_test_driver import run_json, scripted  # noqa: E402

CLUSTER_CONF = "\n".join(
    (
        "PROXMOX_CLUSTER_NAME=MyAppCloud",
        "PROXMOX_QDEVICE_HOST=qdevice2",
        "PROXMOX_CONTROL_NODE=mox1",
        "MAX_MOX_HOSTS=10",
        "PROXMOX_INTERNAL_DOMAIN=pve.internal",
        "PRIVATE_SUBNET_CIDR=10.213.0.0/24",
        "PROXMOX_MIGRATION_NETWORK=10.213.0.240/28",
        "DATACENTER_PRIVATE_VLAN_GATEWAY=10.213.0.1",
        "GUEST_EGRESS_VIP=10.213.0.10/24",
        "MOX_IP_START=10.213.0.11",
        "MOX_IP_END=10.213.0.20",
        "MOX_REPLICATION_IP_START=10.213.0.241",
        "MOX_REPLICATION_IP_END=10.213.0.250",
        "HAPROXY_IP_START=10.213.0.21",
        "HAPROXY_IP_END=10.213.0.30",
        "PRODUCTION_IP_START=10.213.0.31",
        "PRODUCTION_IP_END=10.213.0.50",
        "STAGING_IP_START=10.213.0.51",
        "STAGING_IP_END=10.213.0.200",
        "",
    )
)

# Records its arguments and exits with FAKE_SSH_STATUS.
FAKE_SSH = """#!/usr/bin/env bash
printf '%s\\n' "$*" >>"$FAKE_SSH_CALLS"
[[ "$FAKE_SSH_STATUS" == 0 ]] || printf 'ssh: connect to host qdevice2 port 22: Connection timed out\\n' >&2
exit "$FAKE_SSH_STATUS"
"""


class CheckQdeviceAccessTest(unittest.TestCase):
    def setUp(self) -> None:
        root = Path(tempfile.mkdtemp())
        self.addCleanup(shutil.rmtree, root, True)
        config_dir, bin_dir = root / "config", root / "bin"
        config_dir.mkdir()
        bin_dir.mkdir()
        (config_dir / "cluster.conf").write_text(CLUSTER_CONF, encoding="utf-8")
        fake_ssh = bin_dir / "ssh"
        fake_ssh.write_text(FAKE_SSH, encoding="utf-8")
        fake_ssh.chmod(0o755)
        self.calls = root / "ssh-calls"
        self.env = {
            "PATH": f"{bin_dir}:/usr/bin:/bin",
            "HOME": str(root),
            "APP_HA_CONFIG_TEST_MODE": "1",
            "APP_HA_CONFIG_DIR": str(config_dir),
            "FAKE_SSH_CALLS": str(self.calls),
        }

    def run_check(self, ssh_status: int):
        return run_json([str(SCRIPT)], scripted(), env={**self.env, "FAKE_SSH_STATUS": str(ssh_status)}, timeout=60)

    def test_accessible_qdevice_reports_success_over_strict_root_ssh(self) -> None:
        run = self.run_check(0)
        self.assertEqual(run.completed["status"], "success", run.describe())
        self.assertEqual(run.of("result")[-1]["data"], {"configured_host": "qdevice2", "accessible": True}, run.describe())
        self.assertIn("OK: ssh root@qdevice2 works", run.log_text)
        call = self.calls.read_text(encoding="utf-8").splitlines()
        self.assertEqual(len(call), 1, call)
        self.assertIn("BatchMode=yes", call[0])
        self.assertIn("StrictHostKeyChecking=yes", call[0])
        self.assertTrue(call[0].endswith("root@qdevice2 true"), call[0])

    def test_inaccessible_qdevice_still_completes_with_the_reason(self) -> None:
        run = self.run_check(255)
        self.assertEqual(run.completed["status"], "success", run.describe())
        data = run.of("result")[-1]["data"]
        self.assertFalse(data["accessible"])
        self.assertEqual(data["configured_host"], "qdevice2")
        self.assertIn("'ssh root@qdevice2' failed: ssh: connect to host qdevice2", data["problem"])
        self.assertIn("docs/QDEVICE_MANUAL_SETUP.md", run.of("next_step")[-1]["text"])

    def test_terminal_mode_rejects_unknown_arguments(self) -> None:
        completed = subprocess.run(
            [str(SCRIPT), "--bogus"], env={**self.env, "FAKE_SSH_STATUS": "0"},
            capture_output=True, text=True, check=False, timeout=60,
        )
        self.assertEqual(completed.returncode, 2)
        self.assertIn("Usage:", completed.stderr)
        self.assertFalse(self.calls.exists())


if __name__ == "__main__":
    unittest.main()
