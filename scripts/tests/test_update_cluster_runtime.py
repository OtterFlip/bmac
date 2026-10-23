# Copyright (c) 2026 BEENTHERE VENTURES, INC.
# SPDX-License-Identifier: GPL-3.0-only

import subprocess
import unittest
from pathlib import Path


SCRIPT = Path(__file__).resolve().parent.parent / "user_callable" / "hosts" / "update_cluster_runtime.sh"


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
        runtime = SCRIPT.parent.parent.parent / "host_runtime"
        for name in (
            "cluster_registry.py",
            "process_deferred_cleanup.sh",
            "app-ha-guest-role-hook.sh",
            "guest_network.sh",
            "egress_health.py",
            "egress_probe_targets.txt",
            "transfer_counter.py",
            "tunnel_config.py",
            "tunnel_sync.sh",
            "cloudflared_setup.sh",
            "app_ha_monitor.py",
        ):
            self.assertIn(f"/usr/local/lib/app-ha-proxmox/lib/{name}", text)
            self.assertTrue((runtime / name).is_file(), name)
        self.assertIn("GUEST_ROLE_HOOK_PATH", text)
        self.assertNotIn("haproxy", text.lower())

    def test_cluster_wide_safety_order_and_rollback_are_explicit(self) -> None:
        text = SCRIPT.read_text(encoding="utf-8")
        main = text[text.index("main() {") :]
        self.assertLess(
            main.index('stage_node "$node"'),
            main.index('commit_node "$node"'),
        )
        self.assertLess(
            main.index('commit_node "$node"'),
            main.index('"$REMOTE_TUNNEL_SYNC" --all'),
        )
        self.assertLess(
            main.index('"$REMOTE_TUNNEL_SYNC" --all'),
            main.index('verify_node "$node"'),
        )
        self.assertIn("rollback_committed_nodes", text)
        self.assertIn('row.get("status") != "online"', text)
        self.assertIn('mox_is_reachable "$node"', text)
        # Installs and rollbacks are atomic renames, and a file that did not
        # exist before the update is removed again on rollback.
        self.assertIn('mv -f "${installed}.new" "$installed"', text)
        self.assertIn('mv -f "${installed}.rollback" "$installed"', text)
        self.assertIn('if [[ -f "$stage/${name}.absent" ]]; then', text)
        self.assertIn("systemctl try-restart app-ha-egress-health.service", text)
        self.assertIn('"$REMOTE_TUNNEL_SYNC" --check', text)

    def test_monitoring_is_planned_before_changes_and_applied_after_hosts(self) -> None:
        text = SCRIPT.read_text(encoding="utf-8")
        main = text[text.index("main() {") :]
        # The preflight (every node reachable, API key works) and the plan
        # come before the confirmation and before any node is staged.
        self.assertLess(main.index("plan_monitoring"), main.index('if [[ "$DRY_RUN" == true ]]'))
        self.assertLess(main.index("plan_monitoring"), main.index("confirm_update"))
        self.assertLess(main.index('verify_node "$node"'), main.index("update_monitoring"))
        self.assertIn('monitoring_sync "$COORDINATOR" --dry-run', text)
        self.assertIn("monitoring_disable_everywhere", text)
        self.assertIn("monitoring_require_qdevice", text)
        # A running monitor picks up its new program on commit and rollback.
        self.assertEqual(text.count("systemctl try-restart app-ha-monitor.service"), 2)
        self.assertIn("--delete-obsolete-checks", text)

    def test_monitoring_files_are_shipped_with_the_workstation_tools(self) -> None:
        lib = SCRIPT.parent.parent.parent / "lib"
        for name in ("monitoring.sh", "monitor_node_install.sh", "healthchecks.py", "monitor_config.py"):
            self.assertTrue((lib / name).is_file(), name)
        add_host = (SCRIPT.parent / "add_proxmox_host.sh").read_text(encoding="utf-8")
        start = add_host.index("RUNTIME_LIB_FILES=(")
        self.assertIn("app_ha_monitor.py", add_host[start : add_host.index(")", start)])

if __name__ == "__main__":
    unittest.main()
