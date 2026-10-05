#!/usr/bin/env python3

# Copyright (c) 2026 BEENTHERE VENTURES, INC.
# SPDX-License-Identifier: GPL-3.0-only

"""Mock-friendly tests for QDevice diagnostics, addition, and purge."""

from __future__ import annotations

import base64
from pathlib import Path
import re
import shlex
import subprocess
import tempfile
import textwrap
import unittest


tempfile.tempdir = str(Path(tempfile.gettempdir()).resolve())

QDEVICE_DIR = Path(__file__).resolve().parent
REPO_ROOT = QDEVICE_DIR.parent
SHOW_SCRIPT = REPO_ROOT / "diagnostics" / "show_qdevice_state.sh"
ADD_SCRIPT = QDEVICE_DIR / "add_qdevice.sh"
PURGE_SCRIPT = QDEVICE_DIR / "purge_qdevice.sh"
MEMBERSHIP_LIB = REPO_ROOT / "lib" / "host_membership.sh"
CONTROL_LIB = REPO_ROOT / "lib" / "cluster_control.sh"

BEGIN = "# BEGIN app-ha managed qdevice host key"
END = "# END app-ha managed qdevice host key"
KEY = "AAAAC3NzaC1lZDI1NTE5AAAAIAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"


def run_bash(body: str, expected: int = 0, stdin: str = "") -> subprocess.CompletedProcess[str]:
    completed = subprocess.run(
        ["bash", "-c", textwrap.dedent(body)],
        input=stdin,
        check=False,
        capture_output=True,
        text=True,
    )
    if completed.returncode != expected:
        raise AssertionError(
            f"exit {completed.returncode} != {expected}\n"
            f"stdout:\n{completed.stdout}\nstderr:\n{completed.stderr}"
        )
    return completed


class QdeviceVerdictTest(unittest.TestCase):
    def verdict(self, expected: int, **state: str) -> str:
        assignments = "\n".join(f"{key}={value}" for key, value in state.items())
        completed = run_bash(
            f"""
            SHOW_QDEVICE_STATE_SOURCE_ONLY=1 source {shlex.quote(str(SHOW_SCRIPT))}
            PROXMOX_QDEVICE_HOST=qdevice
            {assignments}
            qdevice_verdict
            """,
            expected=expected,
        )
        return completed.stdout

    def test_odd_cluster_needs_no_qdevice(self) -> None:
        out = self.verdict(0, MEMBER_COUNT=3)
        self.assertIn("correctly has no QDevice", out)
        out = self.verdict(1, MEMBER_COUNT=3, REGISTERED=1, REGISTERED_ADDRESS="100.64.0.9")
        self.assertIn("must not use one", out)

    def test_even_cluster_without_qdevice_points_to_add_qdevice(self) -> None:
        out = self.verdict(1, MEMBER_COUNT=2, QDEVICE_REACHABLE=1)
        self.assertIn("needs a QDevice and has none registered", out)
        self.assertIn("qdevice/add_qdevice.sh", out)
        self.assertIn("qdevice is reachable", out)

    def test_functional_qdevice_is_ok(self) -> None:
        out = self.verdict(
            0,
            MEMBER_COUNT=2,
            REGISTERED=1,
            REGISTERED_ADDRESS="100.64.0.9",
            MEMBERS_HEALTHY=1,
            QDEVICE_REACHABLE=1,
            QDEVICE_TAILSCALE_IP="100.64.0.9",
        )
        self.assertIn("alive and voting on all 2 members (3 expected votes)", out)

    def test_inaccessible_qdevice_points_to_purge_then_add(self) -> None:
        out = self.verdict(
            1,
            MEMBER_COUNT=2,
            REGISTERED=1,
            REGISTERED_ADDRESS="100.64.0.9",
            MEMBERS_HEALTHY=0,
            QDEVICE_REACHABLE=0,
        )
        self.assertIn("ssh root@qdevice fails", out)
        purge = out.index("qdevice/purge_qdevice.sh")
        manual = out.index("QDEVICE_MANUAL_SETUP.md")
        add = out.index("qdevice/add_qdevice.sh")
        self.assertLess(purge, manual)
        self.assertLess(manual, add)
        self.assertIn("remove the old", out)

        replaced = self.verdict(
            1,
            MEMBER_COUNT=2,
            REGISTERED=1,
            REGISTERED_ADDRESS="100.64.0.9",
            QDEVICE_REACHABLE=1,
            QDEVICE_TAILSCALE_IP="100.64.0.10",
        )
        self.assertIn("is a different machine (Tailscale address 100.64.0.10)", replaced)

    def test_reachable_but_unhealthy_qdevice_lists_problems(self) -> None:
        completed = run_bash(
            f"""
            SHOW_QDEVICE_STATE_SOURCE_ONLY=1 source {shlex.quote(str(SHOW_SCRIPT))}
            PROXMOX_QDEVICE_HOST=qdevice
            MEMBER_COUNT=2 REGISTERED=1 REGISTERED_ADDRESS=100.64.0.9
            QDEVICE_REACHABLE=1 QDEVICE_TAILSCALE_IP=100.64.0.9
            PROBLEMS=("corosync-qnetd is inactive on qdevice")
            qdevice_verdict
            """,
            expected=1,
        )
        self.assertIn("- corosync-qnetd is inactive on qdevice", completed.stdout)
        self.assertIn("restart corosync-qnetd", completed.stdout)


class QdeviceHelpersTest(unittest.TestCase):
    def setUp(self) -> None:
        self.directory = Path(tempfile.mkdtemp())
        self.addCleanup(lambda: subprocess.run(["rm", "-rf", str(self.directory)], check=False))

    def pin(self, trust: str, known_hosts: Path, expected: int = 0) -> None:
        encoded = base64.b64encode(trust.encode()).decode()
        run_bash(
            f"""
            source {shlex.quote(str(MEMBERSHIP_LIB))}
            bash -c "$HM_QDEVICE_PIN_SCRIPT" bash {shlex.quote(encoded)} {shlex.quote(str(known_hosts))}
            """,
            expected=expected,
        )

    def test_pin_replaces_only_the_managed_block(self) -> None:
        known_hosts = self.directory / "ssh" / "known_hosts"
        known_hosts.parent.mkdir()
        known_hosts.write_text(
            f"mox2 ssh-ed25519 {KEY}\n{BEGIN}\nqdevice,100.64.0.9 ssh-ed25519 OLD\n{END}\n",
            encoding="utf-8",
        )
        trust = f"qdevice,100.64.0.10 ssh-ed25519 {KEY}"
        self.pin(trust, known_hosts)
        self.pin(trust, known_hosts)
        self.assertEqual(
            known_hosts.read_text(encoding="utf-8"),
            f"mox2 ssh-ed25519 {KEY}\n{BEGIN}\n{trust}\n{END}\n",
        )
        self.assertEqual(known_hosts.stat().st_mode & 0o777, 0o600)
        self.pin(f"qdevice ssh-rsa {KEY}", known_hosts, expected=1)

    def test_cluster_conf_value_is_replaced_in_place(self) -> None:
        conf = self.directory / "cluster.conf"
        conf.write_text("# c\nPROXMOX_QDEVICE_HOST=qdevice\nMAX_MOX_HOSTS=10\n", encoding="utf-8")
        conf.chmod(0o664)
        run_bash(
            f"""
            source {shlex.quote(str(CONTROL_LIB))}
            MAX_MOX_HOSTS=10
            cluster_conf_set_value {shlex.quote(str(conf))} PROXMOX_QDEVICE_HOST qdevice2
            control_rewrite_cluster_conf {shlex.quote(str(conf))} mox3
            ! cluster_conf_set_value {shlex.quote(str(conf))} PROXMOX_QDEVICE_HOST 'bad value'
            """
        )
        self.assertEqual(
            conf.read_text(encoding="utf-8"),
            "# c\nPROXMOX_QDEVICE_HOST=qdevice2\nPROXMOX_CONTROL_NODE=mox3\nMAX_MOX_HOSTS=10\n",
        )
        self.assertEqual(conf.stat().st_mode & 0o777, 0o664)

    def test_purge_node_cleanup_filters_qdevice_entries(self) -> None:
        source = PURGE_SCRIPT.read_text(encoding="utf-8")
        body = re.search(
            r"NODE_CLEANUP_BODY=\$\(cat <<'REMOTE'\n(.*?)\nREMOTE\n\)", source, re.S
        ).group(1)
        function = re.search(r"(filter_known_hosts\(\) \{.*?\n\})", body, re.S).group(1)
        known_hosts = self.directory / "known_hosts"
        known_hosts.write_text(
            "\n".join(
                [
                    f"mox1,mox1.pve.internal ssh-ed25519 {KEY}",
                    BEGIN,
                    f"qdevice,100.64.0.9 ssh-ed25519 {KEY}",
                    END,
                    f"[100.64.0.9]:22 ssh-ed25519 {KEY}",
                    f"qdevice ssh-rsa {KEY}",
                    f"qdevice-other ssh-rsa {KEY}",
                    "# comment",
                ]
            )
            + "\n",
            encoding="utf-8",
        )
        run_bash(
            "begin=" + shlex.quote(BEGIN) + "\nend=" + shlex.quote(END)
            + "\nqdevice_name=qdevice\nqdevice_addr=100.64.0.9\n"
            + function
            + f"\nfilter_known_hosts {shlex.quote(str(known_hosts))}\n"
        )
        self.assertEqual(
            known_hosts.read_text(encoding="utf-8"),
            f"mox1,mox1.pve.internal ssh-ed25519 {KEY}\nqdevice-other ssh-rsa {KEY}\n# comment\n",
        )


class AddQdeviceTest(unittest.TestCase):
    def need(self, body: str, expected: int) -> subprocess.CompletedProcess[str]:
        return run_bash(
            f"""
            ADD_QDEVICE_SOURCE_ONLY=1 source {shlex.quote(str(ADD_SCRIPT))}
            HM_COORDINATOR=mox1
            {textwrap.dedent(body)}
            require_qdevice_needed
            printf 'CONTINUES\\n'
            """,
            expected=expected,
        )

    def test_odd_cluster_or_working_qdevice_stops_without_change(self) -> None:
        odd = self.need("NODE_COUNT=3\nhm_qdevice_configured() { return 1; }", 0)
        self.assertIn("does not use a QDevice", odd.stdout)
        self.assertNotIn("CONTINUES", odd.stdout)
        working = self.need(
            "NODE_COUNT=2\nhm_qdevice_configured() { return 0; }\n"
            "hm_qdevice_layout_problem() { return 0; }",
            0,
        )
        self.assertIn("already has a functional QDevice", working.stdout)
        self.assertNotIn("CONTINUES", working.stdout)

    def test_failed_registered_qdevice_must_be_purged_first(self) -> None:
        failed = self.need(
            "NODE_COUNT=2\nhm_qdevice_configured() { return 0; }\n"
            "hm_qdevice_layout_problem() { printf 'mox2 does not report it\\n'; return 1; }",
            1,
        )
        self.assertIn("not healthy (mox2 does not report it)", failed.stderr)
        self.assertIn("qdevice/purge_qdevice.sh first", failed.stderr)
        unreadable = self.need("NODE_COUNT=2\nhm_qdevice_configured() { return 255; }", 1)
        self.assertIn("Could not read /etc/pve/corosync.conf", unreadable.stderr)

    def test_even_cluster_without_qdevice_continues(self) -> None:
        completed = self.need("NODE_COUNT=2\nhm_qdevice_configured() { return 1; }", 0)
        self.assertIn("needs a QDevice", completed.stdout)
        self.assertIn("CONTINUES", completed.stdout)

    def test_add_follows_setup_order_under_the_lock(self) -> None:
        source = ADD_SCRIPT.read_text(encoding="utf-8")
        main = source[source.index("main() {") :]
        order = [
            "resolve_control_node",
            "require_qdevice_needed",
            "print_setup_instructions",
            "hm_verify_qdevice_access",
            "inspect_qdevice_host",
            "hm_confirm_phrase",
            "hm_acquire_control_plane_lock",
            "load_cluster_shape",
            "hm_add_qdevice",
        ]
        positions = []
        for step in order:
            positions.append(main.index(step, positions[-1] if positions else 0))
        self.assertEqual(positions, sorted(positions))
        library = MEMBERSHIP_LIB.read_text(encoding="utf-8")
        add = library[library.index("hm_add_qdevice() {") :]
        self.assertLess(add.index("hm_pin_qdevice_host_key"), add.index("pvecm qdevice setup"))


class PurgeQdeviceTest(unittest.TestCase):
    def test_hosts_default_to_cluster_conf(self) -> None:
        root = Path(tempfile.mkdtemp())
        self.addCleanup(lambda: subprocess.run(["rm", "-rf", str(root)], check=False))
        env_dir, bin_dir = root / "env", root / "bin"
        env_dir.mkdir()
        bin_dir.mkdir()
        (env_dir / "cluster.conf").write_text(
            "\n".join(
                (
                    "PROXMOX_CLUSTER_NAME=MyAppCloud",
                    "PROXMOX_QDEVICE_HOST=qdevice2",
                    "PROXMOX_CONTROL_NODE=mox3",
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
            ),
            encoding="utf-8",
        )
        calls = root / "ssh-calls"
        fake_ssh = bin_dir / "ssh"
        fake_ssh.write_text(
            '#!/usr/bin/env bash\nprintf \'%s\\n\' "$*" >>"$FAKE_SSH_CALLS"\nexit 255\n',
            encoding="utf-8",
        )
        fake_ssh.chmod(0o755)
        completed = subprocess.run(
            [str(PURGE_SCRIPT)],
            input="GO\n\n\n",
            env={
                "PATH": f"{bin_dir}:/usr/bin:/bin",
                "HOME": str(root),
                "APP_HA_CONFIG_TEST_MODE": "1",
                "APP_HA_ENV_DIR": str(env_dir),
                "FAKE_SSH_CALLS": str(calls),
            },
            check=False,
            capture_output=True,
            text=True,
        )
        self.assertNotEqual(completed.returncode, 0)
        self.assertIn("QDevice host : qdevice2", completed.stdout)
        self.assertIn("Proxmox host : mox3", completed.stdout)
        recorded = calls.read_text(encoding="utf-8").splitlines()
        self.assertEqual(len(recorded), 1)
        self.assertTrue(recorded[0].endswith(" mox3 true"), recorded[0])

    def test_unreachable_qdevice_is_unregistered_without_contacting_it(self) -> None:
        source = PURGE_SCRIPT.read_text(encoding="utf-8")
        start = source.index("unregister_without_qdevice_host() {")
        end = source.index("\n  exit 0\n}\n", start)
        function = source[start:end]
        self.assertNotIn('run_remote_root "$QDEVICE_HOST"', function)
        self.assertIn('"REMOVED FROM TAILSCALE"', function)
        self.assertLess(
            function.index("REMOVED FROM TAILSCALE"),
            function.index('run_remote_root "$PVE_HOST" "$PVE_REMOVE_PAYLOAD"'),
        )
        self.assertIn("must never be able to communicate with the cluster again", function)
        branch = source.index("if (( QDEVICE_REACHABLE == 0 )); then\n  unregister_without_qdevice_host")
        self.assertLess(branch, source.index("Purging QDevice software and state from"))
        self.assertLess(branch, source.index('echo "Checking/removing the QDevice from the Proxmox cluster..."'))


if __name__ == "__main__":
    unittest.main()
