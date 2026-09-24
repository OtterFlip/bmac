#!/usr/bin/env python3

# Copyright (c) 2026 BEENTHERE VENTURES, INC.
# SPDX-License-Identifier: GPL-3.0-only

"""Tests for moxN.conf mirror bookkeeping and the host storage state file."""

from __future__ import annotations

import contextlib
import io
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest


LIB_DIR = Path(__file__).resolve().parent
sys.path.insert(0, str(LIB_DIR))
import mox_conf_mirrors  # noqa: E402
import storage_state  # noqa: E402


CLUSTER_CONF = "\n".join(
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
        "",
    )
)

MOX_CONF = """\
# Mirror 1 is the boot pair.
NVME_MIRROR_1_SERIAL_1=BOOTA
NVME_MIRROR_1_SERIAL_2=BOOTB
NVME_MIRROR_1_CAPACITY_BYTES_1=2000398934016
NVME_MIRROR_1_CAPACITY_BYTES_2=2000398934016
NVME_MIRROR_2_SERIAL_1=OLD1
NVME_MIRROR_2_SERIAL_2=OLD2
NVME_MIRROR_2_CAPACITY_BYTES_1=2000398934016
NVME_MIRROR_2_CAPACITY_BYTES_2=2000398934016
#NVME_MIRROR_3_SERIAL_1=
#NVME_MIRROR_3_SERIAL_2=
#NVME_MIRROR_3_CAPACITY_BYTES_1=
#NVME_MIRROR_3_CAPACITY_BYTES_2=
PROXMOX_IP=10.213.0.11
PROXMOX_GATEWAY=10.213.0.1
PROXMOX_PREFIX=24
PROXMOX_PUBLIC_MAC=02:00:00:00:00:01
PROXMOX_SECONDARY_MAC=02:00:00:00:01:01
MAX_PROD_VM_COUNT_ON_THIS_HOST=1
MAX_STAGING_VM_COUNT_ON_THIS_HOST=5
"""


class MoxConfMirrorsTest(unittest.TestCase):
    def setUp(self) -> None:
        self.temporary = tempfile.TemporaryDirectory()
        self.env_dir = Path(self.temporary.name)
        (self.env_dir / "cluster.conf").write_text(CLUSTER_CONF, encoding="utf-8")
        self.conf = self.env_dir / "mox1.conf"
        self.conf.write_text(MOX_CONF, encoding="utf-8")
        self.conf.chmod(0o600)

    def tearDown(self) -> None:
        self.temporary.cleanup()

    def cli(self, *arguments: str) -> tuple[int, str, str]:
        stdout, stderr = io.StringIO(), io.StringIO()
        with contextlib.redirect_stdout(stdout), contextlib.redirect_stderr(stderr):
            code = mox_conf_mirrors.main(list(arguments))
        return code, stdout.getvalue(), stderr.getvalue()

    def load_with_config_sh(self) -> subprocess.CompletedProcess[str]:
        return subprocess.run(
            [
                "bash",
                "-c",
                'set -Eeuo pipefail; source "$1"; '
                'load_proxmox_config --host mox1 --no-secrets >/dev/null; '
                'for pair in 1 2 3 4 5; do n="NVME_MIRROR_${pair}_SERIAL_1"; '
                'printf "%s=%s\\n" "$pair" "${!n:-}"; done',
                "bash",
                str(LIB_DIR / "config.sh"),
            ],
            env={
                **os.environ,
                "APP_HA_CONFIG_TEST_MODE": "1",
                "APP_HA_ENV_DIR": str(self.env_dir),
            },
            text=True,
            capture_output=True,
            check=False,
        )

    def test_show_lists_only_active_pairs(self) -> None:
        code, output, _ = self.cli("show", str(self.conf))
        self.assertEqual(code, 0)
        pairs = json.loads(output)
        self.assertEqual(sorted(pairs), ["1", "2"])
        self.assertEqual(pairs["2"]["serial_2"], "OLD2")
        self.assertEqual(pairs["2"]["capacity_1"], "2000398934016")

    def test_assign_replaces_template_placeholders(self) -> None:
        code, _, _ = self.cli(
            "assign", str(self.conf), "--pair", "3",
            "--serial", "NEW1", "--serial", "NEW2",
            "--capacity", "4000787030016", "--capacity", "4000787030016",
            "--note", "Added by hosts/add_new_disk_vdev.sh on 2026-09-24",
        )
        self.assertEqual(code, 0)
        text = self.conf.read_text(encoding="utf-8")
        self.assertNotIn("#NVME_MIRROR_3_SERIAL_1=\n", text)
        self.assertIn(
            "# Added by hosts/add_new_disk_vdev.sh on 2026-09-24\n"
            "NVME_MIRROR_3_SERIAL_1=NEW1\n"
            "NVME_MIRROR_3_SERIAL_2=NEW2\n"
            "NVME_MIRROR_3_CAPACITY_BYTES_1=4000787030016\n"
            "NVME_MIRROR_3_CAPACITY_BYTES_2=4000787030016\n"
            "PROXMOX_IP=",
            text,
        )
        self.assertEqual(self.conf.stat().st_mode & 0o777, 0o600)
        loaded = self.load_with_config_sh()
        self.assertEqual(loaded.returncode, 0, loaded.stderr)
        self.assertIn("3=NEW1", loaded.stdout)

    def test_assign_refuses_configured_pair_or_serial(self) -> None:
        base = ["--capacity", "1", "--capacity", "1", "--note", "n"]
        code, _, error = self.cli(
            "assign", str(self.conf), "--pair", "2", "--serial", "A", "--serial", "B", *base
        )
        self.assertEqual(code, 1)
        self.assertIn("already configured", error)
        code, _, error = self.cli(
            "assign", str(self.conf), "--pair", "3", "--serial", "OLD1", "--serial", "B", *base
        )
        self.assertEqual(code, 1)
        self.assertIn("already configured in NVME_MIRROR_2", error)
        code, _, _ = self.cli(
            "assign", str(self.conf), "--pair", "1", "--serial", "A", "--serial", "B", *base
        )
        self.assertEqual(code, 1)
        self.assertEqual(self.conf.read_text(encoding="utf-8"), MOX_CONF)

    def test_retire_comments_out_the_pair_and_config_accepts_the_gap(self) -> None:
        self.cli(
            "assign", str(self.conf), "--pair", "3",
            "--serial", "NEW1", "--serial", "NEW2",
            "--capacity", "4000787030016", "--capacity", "4000787030016",
            "--note", "added",
        )
        code, output, _ = self.cli(
            "retire", str(self.conf), "--serial", "OLD1", "--serial", "OLD2",
            "--note", "Removed from rpool 2026-09-25",
        )
        self.assertEqual(code, 0, output)
        text = self.conf.read_text(encoding="utf-8")
        self.assertIn("# Removed from rpool 2026-09-25: NVME_MIRROR_2_SERIAL_1=OLD1\n", text)
        self.assertIn(
            "# Removed from rpool 2026-09-25: NVME_MIRROR_2_CAPACITY_BYTES_2=2000398934016\n",
            text,
        )
        loaded = self.load_with_config_sh()
        self.assertEqual(loaded.returncode, 0, loaded.stderr)
        self.assertIn("2=\n", loaded.stdout)
        self.assertIn("3=NEW1", loaded.stdout)

        # A later disk pair can take the freed slot again.
        code, _, _ = self.cli(
            "assign", str(self.conf), "--pair", "2",
            "--serial", "NEXT1", "--serial", "NEXT2",
            "--capacity", "1", "--capacity", "1", "--note", "added again",
        )
        self.assertEqual(code, 0)
        self.assertEqual(self.load_with_config_sh().returncode, 0)

    def test_retire_refuses_boot_pair_and_reports_unknown_serials(self) -> None:
        code, _, error = self.cli(
            "retire", str(self.conf), "--serial", "BOOTA", "--note", "n"
        )
        self.assertEqual(code, 1)
        self.assertIn("never decommissioned", error)
        code, _, error = self.cli(
            "retire", str(self.conf), "--serial", "UNKNOWN", "--note", "n"
        )
        self.assertEqual(code, 3)
        self.assertIn("no configured NVMe mirror pair", error)
        self.assertEqual(self.conf.read_text(encoding="utf-8"), MOX_CONF)

    def test_symlinked_conf_is_refused(self) -> None:
        link = self.env_dir / "mox2.conf"
        link.symlink_to(self.conf)
        code, _, error = self.cli("show", str(link))
        self.assertEqual(code, 1)
        self.assertIn("regular file", error)


class StorageStateTest(unittest.TestCase):
    def setUp(self) -> None:
        self.temporary = tempfile.TemporaryDirectory()
        self.path = Path(self.temporary.name) / "state" / "state.json"

    def tearDown(self) -> None:
        self.temporary.cleanup()

    def cli(self, *arguments: str, stdin: str = "") -> tuple[int, str, str]:
        stdout, stderr = io.StringIO(), io.StringIO()
        old_stdin = sys.stdin
        sys.stdin = io.StringIO(stdin)
        try:
            with contextlib.redirect_stdout(stdout), contextlib.redirect_stderr(stderr):
                code = storage_state.main(["--state-file", str(self.path), *arguments])
        finally:
            sys.stdin = old_stdin
        return code, stdout.getvalue(), stderr.getvalue()

    def show(self) -> dict:
        code, output, error = self.cli("show")
        self.assertEqual(code, 0, error)
        return json.loads(output)

    def member(self, serial: str) -> dict:
        return {
            "path": f"/dev/mapper/crypt-rpool-mirror2-{serial[-1]}",
            "mapper": f"/dev/mapper/crypt-rpool-mirror2-{serial[-1]}",
            "luks": True,
            "disk": "/dev/nvme2n1",
            "serial": serial,
            "model": "Dell Ent NVMe",
            "disk_size": 2000398934016,
        }

    def test_empty_state_then_trims_and_scrubs(self) -> None:
        state = self.show()
        self.assertEqual((state["trims"], state["scrubs"], state["removals"]), ({}, [], []))
        self.assertIn("now", state)
        self.assertEqual(self.cli("record-trim", "prod1", "prod7")[0], 0)
        self.assertEqual(self.cli("record-scrub", "--result", "repaired 0B with 0 errors")[0], 0)
        state = self.show()
        self.assertEqual(sorted(state["trims"]), ["prod1", "prod7"])
        self.assertEqual(state["scrubs"][-1]["result"], "repaired 0B with 0 errors")
        self.assertEqual(self.path.stat().st_mode & 0o777, 0o600)
        self.assertEqual(self.cli("record-trim", "stage1prod1")[0], 1)

    def test_removal_records_are_one_at_a_time(self) -> None:
        request = json.dumps(
            {"vdev": "mirror-1", "members": [self.member("S1"), self.member("S2")], "conf_pair": 2}
        )
        code, output, error = self.cli("record-removal", "--request", request)
        self.assertEqual(code, 0, error)
        removal_id = output.strip()
        self.assertTrue(removal_id.startswith("removal-") and removal_id.endswith("-mirror-1"))
        code, _, error = self.cli("record-removal", "--request", request)
        self.assertEqual(code, 1)
        self.assertIn("still recorded as in progress", error)
        self.assertEqual(
            self.cli("set-removal", removal_id, "--state", "retired", "--note", "closed LUKS")[0], 0
        )
        removal = self.show()["removals"][0]
        self.assertEqual(removal["state"], "retired")
        self.assertEqual(removal["notes"][0]["note"], "closed LUKS")
        self.assertEqual(self.cli("record-removal", "--request", request)[0], 0)

    def test_malformed_removal_requests_are_rejected(self) -> None:
        for request in (
            {"vdev": "rpool", "members": [self.member("S1")], "conf_pair": None},
            {"vdev": "mirror-1", "members": [], "conf_pair": None},
            {"vdev": "mirror-1", "members": [{**self.member("S1"), "serial": ""}], "conf_pair": None},
            {"vdev": "mirror-1", "members": [self.member("S1")], "conf_pair": 1},
        ):
            self.assertEqual(self.cli("record-removal", "--request", json.dumps(request))[0], 1)
        self.assertEqual(self.show()["removals"], [])


if __name__ == "__main__":
    unittest.main()
