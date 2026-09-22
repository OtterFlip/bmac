# Copyright (c) 2026 BEENTHERE VENTURES, INC.
# SPDX-License-Identifier: GPL-3.0-only

import subprocess
import unittest
from pathlib import Path


SCRIPT = Path(__file__).with_name("update_cluster_runtime.sh")


class UpdateClusterRuntimeTests(unittest.TestCase):
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
        self.assertIn("--yes", completed.stdout)

    def test_bundle_contains_every_runtime_name_and_install_target(self) -> None:
        text = SCRIPT.read_text(encoding="utf-8")
        for value in (
            "lib/cluster_registry.py",
            "lib/haproxy_routes.py",
            "lib/process_deferred_cleanup.sh",
            "app-ha-guest-role-hook.sh",
            "/usr/local/lib/app-ha-proxmox",
            "GUEST_ROLE_HOOK_PATH",
            "sync_haproxy_routes.sh",
        ):
            self.assertIn(value, text)

    def test_cluster_wide_safety_order_and_rollback_are_explicit(self) -> None:
        text = SCRIPT.read_text(encoding="utf-8")
        main = text[text.index("main() {") :]
        self.assertLess(
            main.index('stage_node "$node"'),
            main.index('commit_node "$node"'),
        )
        self.assertLess(
            main.index('commit_node "$node"'),
            main.index('"$REMOTE_ROUTE_SYNC" --lock-timeout 240'),
        )
        self.assertLess(
            main.index('"$REMOTE_ROUTE_SYNC" --lock-timeout 240'),
            main.index('verify_node "$node"'),
        )
        self.assertIn("rollback_committed_nodes", text)
        self.assertIn('row.get("status") != "online"', text)
        self.assertIn('mox_is_reachable "$node"', text)
        self.assertIn('mv -f "${registry}.new" "$registry"', text)
        self.assertIn('mv -f "${registry}.rollback" "$registry"', text)
        self.assertIn('mv -f "${cleanup}.new" "$cleanup"', text)
        self.assertIn('mv -f "${cleanup}.rollback" "$cleanup"', text)


if __name__ == "__main__":
    unittest.main()
