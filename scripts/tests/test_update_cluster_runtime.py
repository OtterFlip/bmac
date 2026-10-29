# Copyright (c) 2026 BEENTHERE VENTURES, INC.
# SPDX-License-Identifier: GPL-3.0-only

import os
import shutil
import subprocess
import tempfile
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
            "pbs_api.py",
            "backup_link.sh",
            "instant_switch.py",
        ):
            self.assertIn(f"/usr/local/lib/app-ha-proxmox/lib/{name}", text)
            self.assertTrue((runtime / name).is_file(), name)
        # The production VM ISO builder lives outside lib/, and add_prod_vm.sh
        # runs the installed copy, so builder changes must ship here too.
        files = text[text.index("RUNTIME_FILES=(") : text.index(")", text.index("RUNTIME_FILES=("))]
        for name in ("build_ubuntu_autoinstall.py", "prepare_prod_iso.sh"):
            self.assertIn(f"guests/prod/{name}", files.split())
            self.assertIn(f"/usr/local/lib/app-ha-proxmox/guests/prod/{name}", text)
            self.assertTrue((runtime / name).is_file(), name)
        self.assertIn("GUEST_ROLE_HOOK_PATH", text)
        self.assertNotIn("haproxy", text.lower())

    def test_runtime_files_map_to_their_sources_and_unique_staged_names(self) -> None:
        listed = subprocess.run(
            ["bash", "-c", f'source {SCRIPT}; for name in "${{RUNTIME_FILES[@]}}"; do '
             'printf "%s %s\\n" "$name" "$(runtime_source "$name")"; done'],
            env={**os.environ, "APP_HA_RUNTIME_UPDATE_SOURCE_ONLY": "1"},
            capture_output=True, text=True, check=True,
        ).stdout.splitlines()
        repo = SCRIPT.parent.parent.parent.parent
        names = []
        for line in listed:
            path, source = line.split(" ", 1)
            self.assertRegex(path, r"^(lib|guests/prod)/[A-Za-z0-9._-]+$")
            self.assertEqual(Path(source).name, Path(path).name)
            self.assertTrue(Path(source).is_file(), source)
            names.append(Path(path).name)
        self.assertEqual(len(names), len(set(names)))
        self.assertEqual(
            Path(dict(line.split(" ", 1) for line in listed)["lib/config.sh"]),
            repo / "scripts" / "lib" / "config.sh",
        )

    def test_config_parser_is_updated_and_must_accept_the_installed_cluster_conf(self) -> None:
        # The hook and tunnel sync parse the installed cluster.conf with the
        # installed config.sh, so a new cluster.conf key needs a new parser.
        text = SCRIPT.read_text(encoding="utf-8")
        files = text[text.index("RUNTIME_FILES=(") : text.index(")", text.index("RUNTIME_FILES=("))]
        self.assertIn("lib/config.sh", files.split())
        self.assertIn("/usr/local/lib/app-ha-proxmox/lib/config.sh", text)
        self.assertIn('config.sh) printf \'%s\\n\' "$CONFIG_LIB" ;;', text)
        self.assertTrue((SCRIPT.parent.parent.parent / "lib" / "config.sh").is_file())
        commit = text[text.index("commit_node() {") : text.index("rollback_node() {")]
        self.assertLess(
            commit.index('mv -f "${installed}.new" "$installed"'),
            commit.index('"${install_root}/lib/config.sh" --check --no-secrets'),
        )

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

    def test_cloudflared_is_planned_up_front_and_upgraded_last_one_host_at_a_time(self) -> None:
        text = SCRIPT.read_text(encoding="utf-8")
        main = text[text.index("main() {") :]
        self.assertLess(main.index("plan_cloudflared"), main.index('if [[ "$DRY_RUN" == true ]]'))
        self.assertLess(main.index("plan_cloudflared"), main.index("confirm_update"))
        self.assertLess(main.index('verify_node "$node"'), main.index("upgrade_cloudflared"))
        self.assertLess(main.index("update_monitoring"), main.index("upgrade_cloudflared"))
        upgrade = text[text.index("upgrade_cloudflared() {") :]
        self.assertLess(upgrade.index('another_replica_ready "$node"'), upgrade.index('cloudflared_exec "$node" upgrade'))
        self.assertIn("--skip-cloudflared)", text)


# Fakes for the host tools the cloudflared step uses. FAKE holds the state:
# installed (version), candidate, bad (a version whose replica never
# connects), no-downgrade (the repository lacks older versions).
CLOUDFLARED_FAKES = {
    "dpkg-query": 'cat "$FAKE/installed" 2>/dev/null\n',
    "apt-cache": 'printf "cloudflared:\\n  Installed: %s\\n  Candidate: %s\\n" "$(cat "$FAKE/installed")" "$(cat "$FAKE/candidate")"\n',
    "dpkg": (
        '[[ "$1" == --compare-versions && "$3" == gt ]] || exit 2\n'
        '[[ "$2" != "$4" && "$(printf "%s\\n%s\\n" "$2" "$4" | sort -V | tail -n 1)" == "$2" ]]\n'
    ),
    "apt-get": (
        'printf "%s\\n" "$*" >>"$FAKE/apt.log"\n'
        'for word in "$@"; do\n'
        '  case "$word" in\n'
        '    cloudflared=*) [[ ! -f "$FAKE/no-downgrade" ]] || exit 100; printf "%s" "${word#cloudflared=}" >"$FAKE/installed" ;;\n'
        '    cloudflared) cp "$FAKE/candidate" "$FAKE/installed" ;;\n'
        '  esac\n'
        'done\n'
    ),
    "systemctl": (
        'printf "%s\\n" "$*" >>"$FAKE/systemctl.log"\n'
        'if [[ "$(cat "$FAKE/installed")" == "$(cat "$FAKE/bad" 2>/dev/null)" ]]; then rm -f "$FAKE/ready"; else : >"$FAKE/ready"; fi\n'
    ),
    "curl": '[[ -f "$FAKE/ready" ]] || exit 7\necho \'{"status":200,"readyConnections":4}\'\n',
    "sleep": ":\n",
}


class CloudflaredUpgradeScriptTest(unittest.TestCase):
    def setUp(self) -> None:
        self.fake = Path(tempfile.mkdtemp())
        self.addCleanup(shutil.rmtree, self.fake, True)
        bin_dir = self.fake / "bin"
        bin_dir.mkdir()
        for name, body in CLOUDFLARED_FAKES.items():
            (bin_dir / name).write_text("#!/usr/bin/env bash\n" + body)
            (bin_dir / name).chmod(0o755)
        (self.fake / "installed").write_text("2026.10.3")
        (self.fake / "candidate").write_text("2026.10.4")
        (self.fake / "ready").write_text("")
        self.env = {**os.environ, "PATH": f"{bin_dir}:{os.environ['PATH']}", "FAKE": str(self.fake),
                    "APP_HA_RUNTIME_UPDATE_SOURCE_ONLY": "1"}
        # The remote script exactly as the updater sends it, minus the real
        # apt lock helpers, which read this machine's /proc/locks.
        self.script = subprocess.run(
            ["bash", "-c", f'source {SCRIPT}; printf "%s" "$CLOUDFLARED_REMOTE"'],
            env=self.env, capture_output=True, text=True, check=True,
        ).stdout

    def remote(self, *arguments: str) -> subprocess.CompletedProcess:
        return subprocess.run(["bash", "-c", "apt_wait_for_locks() { :; }\n" + self.script, "bash", *arguments],
                              env=self.env, capture_output=True, text=True, timeout=60)

    def test_plan_reports_whether_a_newer_version_exists(self) -> None:
        self.assertEqual(self.remote("plan").stdout.strip(), "2026.10.3 2026.10.4 yes")
        (self.fake / "candidate").write_text("2026.10.3")
        self.assertEqual(self.remote("plan").stdout.strip(), "2026.10.3 2026.10.3 no")
        (self.fake / "installed").unlink()
        self.assertEqual(self.remote("plan").stdout.strip(), "none 2026.10.3 no")

    def test_upgrade_restarts_the_replica_and_waits_for_it(self) -> None:
        completed = self.remote("upgrade")
        self.assertEqual(completed.returncode, 0, completed.stderr)
        self.assertEqual(completed.stdout.strip(), "upgraded 2026.10.3 2026.10.4")
        self.assertIn("--only-upgrade", (self.fake / "apt.log").read_text())
        self.assertIn("restart app-ha-cloudflared.service", (self.fake / "systemctl.log").read_text())

    def test_a_replica_that_does_not_reconnect_gets_its_previous_version_back(self) -> None:
        (self.fake / "bad").write_text("2026.10.4")
        completed = self.remote("upgrade")
        self.assertEqual(completed.returncode, 3, completed.stdout + completed.stderr)
        self.assertEqual((self.fake / "installed").read_text(), "2026.10.3")
        self.assertIn("--allow-downgrades cloudflared=2026.10.3", (self.fake / "apt.log").read_text())
        self.assertTrue((self.fake / "ready").exists())

    def test_failure_without_an_older_package_is_reported(self) -> None:
        (self.fake / "bad").write_text("2026.10.4")
        (self.fake / "no-downgrade").write_text("")
        self.assertEqual(self.remote("upgrade").returncode, 4)

    def test_ready_fails_while_the_replica_is_down(self) -> None:
        self.assertEqual(self.remote("ready", "2").returncode, 0)
        (self.fake / "ready").unlink()
        self.assertNotEqual(self.remote("ready", "2").returncode, 0)


if __name__ == "__main__":
    unittest.main()
