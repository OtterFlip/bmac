#!/usr/bin/env python3

# Copyright (c) 2026 BEENTHERE VENTURES, INC.
# SPDX-License-Identifier: GPL-3.0-only

"""Fake-SSH tests for the read-only Proxmox host state report."""

from __future__ import annotations

import json
import os
from pathlib import Path
import shlex
import subprocess
import sys
import tempfile
import unittest


DIAGNOSTICS_DIR = Path(__file__).resolve().parent
LIB_DIR = DIAGNOSTICS_DIR.parent / "lib"
SCRIPT = DIAGNOSTICS_DIR / "show_proxmox_host_state.sh"
sys.path.insert(0, str(LIB_DIR))
import test_host_storage as storage_fixtures  # noqa: E402

# Every remote command the report may run. Anything else, such as qm, zpool
# remove, or a registry mutation, fails the fake and the test.
READ_ONLY_COMMANDS = {
    ("true",),
    ("pveversion",),
    ("uname", "-r"),
    ("uptime",),
    ("nproc",),
    ("free", "-h"),
    ("pvecm", "status"),
    ("corosync-cfgtool", "-s"),
    ("ha-manager", "status"),
    ("tailscale", "ip", "-4"),
    ("zpool", "status", "-v", "rpool"),
    ("proxmox-boot-tool", "status"),
    ("df", "-hT", "-x", "tmpfs", "-x", "devtmpfs"),
    ("ip", "-br", "-4", "address"),
    ("ip", "-4", "route", "show"),
    ("ip", "-4", "-o", "address", "show"),
    ("python3", "-", "collect"),
}

FAKE_SSH = r'''#!/usr/bin/env python3
import json
import os
from pathlib import Path
import shlex
import sys

args = sys.argv[1:]
destination = next(index for index, value in enumerate(args) if value == "root@mox1")
words = shlex.split(" ".join(args[destination + 1:]))
payload = "" if sys.stdin.isatty() else sys.stdin.read()
fixtures = Path(os.environ["FAKE_FIXTURES"])
with open(os.environ["FAKE_SSH_CALLS"], "a", encoding="utf-8") as stream:
    stream.write(json.dumps(words) + "\n")

def fixture(name):
    sys.stdout.write((fixtures / name).read_text(encoding="utf-8"))

allowed = {tuple(row) for row in json.loads(os.environ["FAKE_ALLOWED"])}
if words[:1] == ["systemctl"]:
    if "--failed" in words:
        sys.stdout.write(os.environ.get("FAKE_FAILED_UNITS", ""))
    elif words[-1] in os.environ.get("FAKE_INACTIVE_UNITS", "").split():
        print("inactive")
        raise SystemExit(3)
    else:
        print("active")
elif words[:2] == ["pvesh", "get"] and "--output-format" in words:
    fixture({
        "/nodes/mox1/qemu": "qemu.json",
        "/nodes/mox1/lxc": "lxc.json",
        "/cluster/resources": "cluster-vms.json",
        "/cluster/ha/resources": "ha.json",
        "/cluster/replication": "replication.json",
    }[words[2]])
elif words[:1] and words[0].endswith("/cluster_registry.py"):
    if words[3:] == ["list", "--record-type", "resources"]:
        fixture("resources.json")
    elif words[3:] == ["list", "--record-type", "cleanup"]:
        fixture("cleanup.json")
    else:
        raise SystemExit(f"registry call is not read-only: {words}")
elif words[:2] == ["bash", "-c"] and "ls -l" in words[2]:
    print("none")
elif words[:2] == ["bash", "-c"] and "/etc/crypttab" in words[2]:
    print("crypt-rpool-a UUID=1111 app-ha-rpool luks,initramfs,keyscript=decrypt_keyctl")
elif tuple(words) == ("python3", "-", "collect"):
    if "def collect(" not in payload:
        raise SystemExit("collect was not given lib/host_storage.py")
    fixture("storage.json")
elif tuple(words) == ("ip", "-4", "-o", "address", "show"):
    print("3: vmbr-private    inet 10.213.0.10/24 scope global secondary vmbr-private")
elif tuple(words) in allowed:
    print(f"fake output of {' '.join(words)}")
else:
    raise SystemExit(f"unexpected remote command: {words}")
'''


class ShowProxmoxHostStateTest(unittest.TestCase):
    def setUp(self) -> None:
        self.temporary = tempfile.TemporaryDirectory()
        root = Path(self.temporary.name)
        self.env_dir = root / "env"
        self.bin_dir = root / "bin"
        self.fixtures = root / "fixtures"
        for path in (self.env_dir, self.bin_dir, self.fixtures):
            path.mkdir()
        self.calls = root / "ssh-calls.jsonl"
        known_hosts = root / "known_hosts"
        known_hosts.write_text("", encoding="utf-8")
        known_hosts.chmod(0o600)

        (self.env_dir / "cluster.conf").write_text(
            "\n".join(
                (
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
                    "CLUSTER_STATE_DIR=/etc/pve/priv/app-ha",
                    "",
                )
            ),
            encoding="utf-8",
        )
        self.write_host_conf(
            [
                ("S0BOOTA", "S1BOOTB"),
                ("S2EXTRA1", "S3EXTRA2"),
                ("S4EXTRA3", "S5EXTRA4"),
            ]
        )

        fake_ssh = self.bin_dir / "ssh"
        fake_ssh.write_text(FAKE_SSH, encoding="utf-8")
        fake_ssh.chmod(0o755)

        self.write_fixture(
            "qemu.json",
            [
                {"vmid": 100, "name": "prod1", "status": "running", "cpus": 4,
                 "maxmem": 8589934592, "maxdisk": 1099511627776},
                {"vmid": 200, "name": "stage1prod2", "status": "stopped", "cpus": 2,
                 "maxmem": 4294967296, "maxdisk": 1099511627776},
            ],
        )
        self.write_fixture(
            "lxc.json",
            [{"vmid": 9111, "name": "haproxy1", "status": "running", "cpus": 1,
              "maxmem": 536870912, "maxdisk": 8589934592}],
        )
        self.write_fixture(
            "cluster-vms.json",
            [
                {"vmid": 100, "name": "prod1", "node": "mox1", "type": "qemu"},
                {"vmid": 101, "name": "prod2", "node": "mox2", "type": "qemu"},
                {"vmid": 200, "name": "stage1prod2", "node": "mox1", "type": "qemu"},
                {"vmid": 9111, "name": "haproxy1", "node": "mox1", "type": "lxc"},
            ],
        )
        self.write_fixture(
            "ha.json",
            [{"sid": "vm:100", "state": "started"}, {"sid": "vm:101", "state": "started"}],
        )
        self.write_fixture(
            "replication.json",
            [
                {"id": "100-0", "guest": 100, "target": "mox2", "schedule": "*/15"},
                {"id": "101-0", "guest": 101, "target": "mox1", "schedule": "*/15"},
            ],
        )
        self.write_fixture(
            "resources.json",
            [
                {"name": "prod1", "kind": "production", "vmid": 100, "state": "active"},
                {"name": "prod2", "kind": "production", "vmid": 101, "state": "active"},
                {"name": "stage1prod2", "kind": "staging", "vmid": 200, "state": "ready"},
            ],
        )
        self.write_fixture("cleanup.json", [])
        self.write_storage(storage_fixtures.luks_layout(
            status_text=storage_fixtures.LUKS_STATUS.replace(
                storage_fixtures.LUKS_STATUS[
                    storage_fixtures.LUKS_STATUS.index("remove:"):
                    storage_fixtures.LUKS_STATUS.index("config:")
                ],
                "",
            )
        ))

        self.environment = os.environ.copy()
        self.environment.update(
            {
                "APP_HA_CONFIG_TEST_MODE": "1",
                "APP_HA_ENV_DIR": str(self.env_dir),
                "PROXMOX_SSH_KNOWN_HOSTS_FILE": str(known_hosts),
                "FAKE_FIXTURES": str(self.fixtures),
                "FAKE_SSH_CALLS": str(self.calls),
                "FAKE_ALLOWED": json.dumps(sorted(READ_ONLY_COMMANDS)),
                "PATH": f"{self.bin_dir}:{os.environ['PATH']}",
            }
        )

    def tearDown(self) -> None:
        self.temporary.cleanup()

    def write_fixture(self, name: str, value) -> None:
        (self.fixtures / name).write_text(json.dumps(value), encoding="utf-8")

    def write_storage(self, layout) -> None:
        self.write_fixture("storage.json", layout)

    def write_host_conf(self, pairs) -> None:
        lines = []
        for pair, (first, second) in enumerate(pairs, start=1):
            lines += [
                f"NVME_MIRROR_{pair}_SERIAL_1={first}",
                f"NVME_MIRROR_{pair}_SERIAL_2={second}",
            ]
        lines += [
            "PROXMOX_IP=10.213.0.11",
            "PROXMOX_GATEWAY=10.213.0.1",
            "PROXMOX_PREFIX=24",
            "PROXMOX_PUBLIC_MAC=02:00:00:00:00:01",
            "PROXMOX_SECONDARY_MAC=02:00:00:00:01:01",
            "MAX_PROD_VM_COUNT_ON_THIS_HOST=1",
            "MAX_STAGING_VM_COUNT_ON_THIS_HOST=5",
            "",
        ]
        (self.env_dir / "mox1.conf").write_text("\n".join(lines), encoding="utf-8")

    def run_script(self, *arguments: str, expected: int) -> subprocess.CompletedProcess[str]:
        completed = subprocess.run(
            [str(SCRIPT), *arguments],
            env=self.environment,
            text=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            check=False,
        )
        self.assertEqual(
            completed.returncode,
            expected,
            msg=f"stdout:\n{completed.stdout}\nstderr:\n{completed.stderr}",
        )
        return completed

    def remote_commands(self) -> list[list[str]]:
        return [
            json.loads(line)
            for line in self.calls.read_text(encoding="utf-8").splitlines()
        ]

    def test_healthy_host_report_covers_guests_storage_and_serials(self) -> None:
        output = self.run_script("mox1", expected=0).stdout
        self.assertIn("Compared rpool with 6 serial(s) from env/mox1.conf.", output)
        self.assertIn("mox1 currently holds the guest egress VIP 10.213.0.10/24", output)
        self.assertIn("[PASS] No failed systemd units.", output)
        self.assertRegex(output, r"100\s+prod1\s+qemu\s+running\s+production\s+active\s+started")
        self.assertRegex(output, r"9111\s+haproxy1\s+lxc\s+running\s+haproxy ingress")
        self.assertRegex(output, r"101-0\s+prod2 \(101\)\s+mox2")
        self.assertIn("boot/ESP (never removable)", output)
        self.assertIn("prod1 (here)", output)
        self.assertIn("NVME_MIRROR_3_SERIAL_2  S5EXTRA4  mirror-2", output)
        self.assertIn("[OK] mox1 has no attention items.", output)
        self.assertIn("[PASS] pve-ha-lrm.service is active.", output)

    def test_a_stopped_required_service_needs_attention(self) -> None:
        # Stopped cleanly, so it is not a failed unit either.
        self.environment["FAKE_INACTIVE_UNITS"] = "pve-ha-lrm.service"
        output = self.run_script("mox1", expected=1).stdout
        self.assertIn("[ATTENTION] pve-ha-lrm.service is inactive.", output)
        self.assertIn("[PASS] corosync.service is active.", output)
        self.assertIn("[PASS] No failed systemd units.", output)
        self.assertIn("[ATTENTION] mox1 has one or more attention items above.", output)

    def test_report_runs_only_read_only_remote_commands(self) -> None:
        self.run_script("mox1", expected=0)
        for words in self.remote_commands():
            if words[:1] == ["systemctl"]:
                self.assertTrue({"is-active", "--failed"} & set(words), words)
            elif words[:2] == ["pvesh", "get"]:
                continue
            elif words[:1] and words[0].endswith("/cluster_registry.py"):
                self.assertEqual(words[3], "list", words)
            elif words[:2] == ["bash", "-c"]:
                self.assertTrue(
                    words[2].startswith("ls -l") or "cat /etc/crypttab" in words[2],
                    words,
                )
            else:
                self.assertIn(tuple(words), READ_ONLY_COMMANDS)

    def test_attention_items_set_exit_status(self) -> None:
        self.write_storage(storage_fixtures.luks_layout())
        self.write_host_conf([("S0BOOTA", "S1BOOTB"), ("S2EXTRA1", "S2GONE")])
        self.environment["FAKE_FAILED_UNITS"] = "keepalived.service loaded failed failed\n"
        output = self.run_script("mox1", expected=1).stdout
        self.assertIn("[ATTENTION] Failed systemd units:", output)
        self.assertIn("a top-level vdev removal is in progress", output)
        self.assertIn(
            "NVME_MIRROR_2_SERIAL_2=S2GONE is configured but is not an rpool member",
            output,
        )
        self.assertIn("[ATTENTION] mox1 has one or more attention items above.", output)

    def test_missing_host_conf_skips_serial_comparison(self) -> None:
        (self.env_dir / "mox1.conf").unlink()
        output = self.run_script("mox1", expected=0).stdout
        self.assertIn("env/mox1.conf is not present on this workstation", output)
        self.assertNotIn("Serials recorded in the host's .conf file", output)

    def test_usage_errors(self) -> None:
        self.run_script(expected=2)
        self.run_script("pve1", expected=2)
        self.run_script("mox1", "mox2", expected=2)
        self.assertIn("Usage:", self.run_script("--help", expected=0).stdout)
        self.assertFalse(self.calls.exists())


if __name__ == "__main__":
    unittest.main()
