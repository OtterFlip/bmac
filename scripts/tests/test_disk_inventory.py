#!/usr/bin/env python3

# Copyright (c) 2026 BEENTHERE VENTURES, INC.
# SPDX-License-Identifier: GPL-3.0-only

"""Tests for scripts/utilities/disk_inventory.py, behind scripts/user_callable/diagnostics/list_disks.sh."""

from __future__ import annotations

import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

SCRIPTS_DIR = Path(__file__).resolve().parent.parent
LIB_DIR = SCRIPTS_DIR / "lib"
DISK_INVENTORY = SCRIPTS_DIR / "utilities" / "disk_inventory.py"
LIST_DISKS = SCRIPTS_DIR / "user_callable" / "diagnostics" / "list_disks.sh"

# Plays every host for list_disks.sh: answers mox_ssh with the files in
# FAKE_ROOT, and fails like a timed-out connection for hosts in `unreachable`.
FAKE_SSH = r'''#!/usr/bin/env python3
import os
import subprocess
import sys

args = sys.argv[1:]
index = 0
while index < len(args) and args[index].startswith("-"):
    index += 2 if args[index] in ("-o", "-i", "-p") else 1
node = args[index].split("@", 1)[1]
words = subprocess.run(
    ["bash", "-c", 'eval "set -- $1"; printf "%s\\0" "$@"', "_", " ".join(args[index + 1:])],
    check=True, capture_output=True, text=True,
).stdout.split("\0")[:-1]
root = os.environ["FAKE_ROOT"]
if node in open(f"{root}/unreachable").read().split():
    sys.exit(f"ssh: connect to host {node} port 22: Connection timed out")
stdin = sys.stdin.read()
with open(f"{root}/commands", "a") as log:
    log.write(f"{node} {' '.join(words[:3])}\n")
if words == ["true"]:
    sys.exit(0)
if words[:2] == ["bash", "-c"] and "pvesh get /nodes" in words[2]:
    print(open(f"{root}/nodes.json").read())
elif words[:3] == ["python3", "-", "collect"]:
    assert "def collect(" in stdin
    print(open(f"{root}/{node}.layout.json").read())
elif words[:3] == ["python3", "-", "show"]:
    assert "Durable storage-maintenance records" in stdin
    print(open(f"{root}/{node}.state.json").read())
else:
    sys.exit(f"unexpected command on {node}: {words}")
'''
sys.path.insert(0, str(SCRIPTS_DIR / "utilities"))
sys.path.insert(0, str(Path(__file__).resolve().parent))

import disk_inventory  # noqa: E402
from test_disk_workflows import fixtures_cluster_conf  # noqa: E402
import test_host_storage as fixtures  # noqa: E402
from ui_test_driver import run_json, scripted  # noqa: E402

REMOVE_BLOCK = (
    "remove: Evacuation of mirror-1 in progress since Wed Sep 24 10:00:00 2026\n"
    "\t21.5G copied out of 40.0G at 120M/s, 53.75% done, 0h2m to go\n"
    "\t4.21M memory used for removed device mappings\n"
)
MIRROR_1 = (
    "\t  mirror-1                             ONLINE       0     0     0\n"
    "\t    /dev/mapper/crypt-rpool-mirror1-1  ONLINE       0     0     0\n"
    "\t    /dev/mapper/crypt-rpool-mirror1-2  ONLINE       0     0     0\n"
)
EVACUATED_STATUS = fixtures.LUKS_STATUS.replace(REMOVE_BLOCK, "").replace(MIRROR_1, "")
DEGRADED_STATUS = (
    fixtures.LUKS_STATUS.replace(REMOVE_BLOCK, "")
    .replace(" state: ONLINE", " state: DEGRADED")
    .replace("\t  mirror-0                             ONLINE",
             "\t  mirror-0                             DEGRADED")
    .replace("\t    /dev/mapper/crypt-rpool-b          ONLINE",
             "\t    /dev/disk/by-id/nvme-gone-part3    UNAVAIL")
)


def removal(state: str = "requested", luks: bool = True) -> dict:
    return {
        "id": "removal-1-mirror-1",
        "vdev": "mirror-1",
        "state": state,
        "requested_at": 1,
        "updated_at": 2,
        "notes": [],
        "members": [
            {"path": f"/dev/mapper/{mapper}", "mapper": mapper if luks else None, "luks": luks,
             "disk": disk, "serial": serial, "model": "Dell Ent NVMe", "disk_size": 2000398934016}
            for mapper, disk, serial in (
                ("crypt-rpool-mirror1-1", "/dev/nvme2n1", "S2EXTRA1"),
                ("crypt-rpool-mirror1-2", "/dev/nvme3n1", "S3EXTRA2"),
            )
        ],
    }


def storage_state(**extra) -> dict:
    return {"schema_version": 1, "host": "mox1", "trims": {}, "scrubs": [], "removals": [],
            "now": fixtures.NOW, **extra}


def disks_by_serial(host: dict) -> dict:
    return {disk["serial"]: disk for disk in host["disks"]}


class ClassificationTest(unittest.TestCase):
    def test_vdevs_report_luks_boot_and_evacuation(self) -> None:
        layout = fixtures.luks_layout()
        host = disk_inventory.summarize_host("mox1", layout, storage_state(removals=[removal()]))
        vdevs = {vdev["name"]: vdev for vdev in host["vdevs"]}
        self.assertEqual(list(vdevs), ["mirror-0", "mirror-1", "mirror-2"])
        self.assertEqual(vdevs["mirror-0"]["encryption"], "luks")
        self.assertTrue(vdevs["mirror-0"]["holds_esp"])
        self.assertEqual(vdevs["mirror-1"]["status"], "evacuating")
        self.assertEqual(vdevs["mirror-2"]["status"], "online")
        member = vdevs["mirror-0"]["members"][0]
        self.assertEqual(
            (member["disk"], member["serial"], member["size"], member["mapper"]),
            ("/dev/nvme0n1", "S0BOOTA", 2000398934016, "crypt-rpool-a"),
        )
        self.assertEqual(host["removals"][0]["status"], "evacuating")
        problems, steps = disk_inventory.host_attention(host)
        self.assertEqual((problems, steps), ([], []))

    def test_disks_outside_rpool_are_available(self) -> None:
        host = disk_inventory.summarize_host("mox1", fixtures.luks_layout(), storage_state())
        disks = disks_by_serial(host)
        self.assertEqual(set(disks), {"S6BLANK", "S7USED"})
        self.assertEqual(disks["S6BLANK"]["status"], "available")
        self.assertEqual(disks["S6BLANK"]["contents"], "blank")
        self.assertTrue(disks["S6BLANK"]["removable"])
        self.assertEqual(disks["S7USED"]["contents"], "partitions or signatures")
        self.assertIn("erased when the disk is used", disks["S7USED"]["detail"])
        self.assertEqual(disks["S6BLANK"]["size"], 4000787030016)

    def test_evacuated_luks_removal_awaits_finalization(self) -> None:
        layout = fixtures.luks_layout(status_text=EVACUATED_STATUS)
        state = storage_state(removals=[removal()])
        self.assertEqual(
            [(row["vdev"], row["status"], row["mappers"])
             for row in disk_inventory.pending_removals(layout, state)],
            [("mirror-1", "needs-retire", ["crypt-rpool-mirror1-1", "crypt-rpool-mirror1-2"])],
        )
        host = disk_inventory.summarize_host("mox1", layout, state)
        disks = disks_by_serial(host)
        for serial in ("S2EXTRA1", "S3EXTRA2"):
            self.assertEqual(disks[serial]["status"], "awaiting_finalization")
            self.assertFalse(disks[serial]["removable"])
            self.assertIn("crypt-rpool-mirror1-1, crypt-rpool-mirror1-2", disks[serial]["detail"])
        problems, steps = disk_inventory.host_attention(host)
        self.assertEqual(
            problems,
            ["mox1: disks S2EXTRA1, S3EXTRA2 finished evacuating and await retirement finalization"],
        )
        self.assertEqual([(step[3], step[4]) for step in steps], [("inventory_disks", {"host": "mox1"})])

    def test_unencrypted_removal_is_complete(self) -> None:
        layout = fixtures.luks_layout(status_text=EVACUATED_STATUS)
        rows = disk_inventory.pending_removals(layout, storage_state(removals=[removal(luks=False)]))
        self.assertEqual(rows[0]["status"], "complete")

    def test_retired_disk_is_available_and_says_so(self) -> None:
        layout = fixtures.luks_layout()
        layout["unassigned_disks"][0]["serial"] = "S2EXTRA1"
        host = disk_inventory.summarize_host(
            "mox1", layout, storage_state(removals=[removal(state="retired")])
        )
        disk = disks_by_serial(host)["S2EXTRA1"]
        self.assertEqual(disk["status"], "available")
        self.assertTrue(disk["detail"].startswith("Retired from mirror-1."))

    def test_interrupted_replacement_and_addition(self) -> None:
        state = storage_state(
            replacements={"S1BOOTB": {"serial": "S6BLANK", "vdev": "mirror-0", "recorded_at": 1}},
            additions={"3": {"serials": ["S7USED", "S9OTHER"], "capacities": [1, 1], "recorded_at": 1}},
        )
        host = disk_inventory.summarize_host("mox1", fixtures.luks_layout(), state)
        disks = disks_by_serial(host)
        self.assertEqual(disks["S6BLANK"]["status"], "pending_replacement")
        self.assertEqual(disks["S7USED"]["status"], "pending_addition")
        self.assertIn("extra mirror 3", disks["S7USED"]["detail"])
        problems, steps = disk_inventory.host_attention(host)
        self.assertIn("mox1: disk S6BLANK was prepared for a mirror replacement but never joined rpool",
                      problems)
        self.assertEqual({step[3] for step in steps}, {"add_replacement_disk", "add_new_disk_vdev"})

    def test_mounted_disk_is_in_use(self) -> None:
        layout = fixtures.luks_layout()
        disk = layout["unassigned_disks"][1]
        disk["mounted"] = True
        disk["in_use_reasons"] = ["mounted filesystem or active swap"]
        host = disk_inventory.summarize_host("mox1", layout, storage_state())
        row = disks_by_serial(host)["S7USED"]
        self.assertEqual((row["status"], row["detail"]), ("in_use", "Mounted filesystem or active swap."))
        self.assertFalse(row["removable"])

    def test_degraded_vdev_suggests_replacement(self) -> None:
        layout = fixtures.luks_layout(status_text=DEGRADED_STATUS)
        layout["esp_only_disks"] = []  # the failed disk was pulled
        host = disk_inventory.summarize_host("mox1", layout, storage_state())
        mirror = host["vdevs"][0]
        self.assertEqual(mirror["status"], "degraded")
        gone = mirror["members"][1]
        self.assertTrue(gone["missing"])
        self.assertEqual(gone["state"], "UNAVAIL")
        problems, steps = disk_inventory.host_attention(host)
        self.assertEqual(problems, ["mox1: rpool mirror-0 is DEGRADED"])
        self.assertEqual([step[3] for step in steps], ["add_replacement_disk"])


class RenderTest(unittest.TestCase):
    def setUp(self) -> None:
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.run_dir = Path(self.temp.name)
        self.env = {key: value for key, value in os.environ.items() if not key.startswith("BMAC_UI")}
        layout = fixtures.luks_layout(status_text=EVACUATED_STATUS)
        (self.run_dir / "mox1.layout.json").write_text(json.dumps(layout))
        (self.run_dir / "mox1.state.json").write_text(json.dumps(storage_state(removals=[removal()])))
        (self.run_dir / "mox3.layout.err").write_text("ssh: connect to host mox3 port 22: timed out\n")
        (self.run_dir / "hosts.tsv").write_text("mox3\tonline\nmox1\tonline\nmox2\toffline\n")

    def test_terminal_report(self) -> None:
        completed = subprocess.run(
            [sys.executable, str(DISK_INVENTORY), "render", "--run-dir", str(self.run_dir)],
            capture_output=True, text=True, env=self.env, check=True,
        )
        text = completed.stdout
        self.assertLess(text.index("DISKS ON mox1"), text.index("DISKS ON mox2"))
        self.assertIn("Awaiting finalization", text)
        self.assertIn("S6BLANK", text)
        self.assertIn("ssh: connect to host mox3 port 22: timed out", text)
        self.assertIn("ATTENTION: 3 items need attention:", text)

    def test_json_result(self) -> None:
        harness = self.run_dir / "render.sh"
        harness.write_text(
            "#!/usr/bin/env bash\n"
            f"source {LIB_DIR / 'ui_protocol.sh'}\n"
            'bmac_ui_bootstrap "$@"\n'
            f'exec python3 {DISK_INVENTORY} render "$@"\n'
        )
        harness.chmod(0o755)
        run = run_json([str(harness), "--run-dir", str(self.run_dir)], scripted(),
                       env=self.env, timeout=60)
        self.assertEqual(run.completed.get("status"), "success", run.describe())
        summary = run.of("result")[0]["data"]
        self.assertEqual([host["node"] for host in summary["hosts"]], ["mox1", "mox2", "mox3"])
        mox1, mox2, mox3 = summary["hosts"]
        self.assertEqual([vdev["name"] for vdev in mox1["vdevs"]], ["mirror-0", "mirror-2"])
        self.assertFalse(mox2["online"])
        self.assertEqual(mox3["error"], "ssh: connect to host mox3 port 22: timed out")
        self.assertEqual(
            summary["problems"],
            ["mox1: disks S2EXTRA1, S3EXTRA2 finished evacuating and await retirement finalization",
             "mox2 is offline; its disks were not inspected",
             "mox3: its disks could not be read: ssh: connect to host mox3 port 22: timed out"],
        )
        self.assertEqual([step["workflow"] for step in run.of("next_step")], ["inventory_disks"])

    def test_pending_removals_cli(self) -> None:
        completed = subprocess.run(
            [sys.executable, str(DISK_INVENTORY), "pending-removals",
             str(self.run_dir / "mox1.layout.json"), str(self.run_dir / "mox1.state.json")],
            capture_output=True, text=True, check=True,
        )
        self.assertEqual(
            completed.stdout,
            "removal-1-mirror-1\tmirror-1\tneeds-retire\tS2EXTRA1,S3EXTRA2\t"
            "crypt-rpool-mirror1-1,crypt-rpool-mirror1-2\n",
        )


class ListDisksScriptTest(unittest.TestCase):
    def setUp(self) -> None:
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        root = Path(self.temp.name)
        self.root = root
        for name in ("config", "bin", "fake"):
            (root / name).mkdir()
        (root / "config" / "cluster.conf").write_text(
            fixtures_cluster_conf().replace("MAX_MOX_HOSTS=10", "MAX_MOX_HOSTS=3")
        )
        known_hosts = root / "known_hosts"
        known_hosts.write_text("")
        known_hosts.chmod(0o600)
        ssh = root / "bin" / "ssh"
        ssh.write_text(FAKE_SSH)
        ssh.chmod(0o755)
        fake = root / "fake"
        (fake / "nodes.json").write_text(json.dumps([
            {"node": "mox1", "status": "online"},
            {"node": "mox2", "status": "offline"},
            {"node": "mox3", "status": "online"},
        ]))
        (fake / "unreachable").write_text("mox3\n")
        layout = fixtures.luks_layout(status_text=EVACUATED_STATUS)
        (fake / "mox1.layout.json").write_text(json.dumps(layout))
        (fake / "mox1.state.json").write_text(json.dumps(storage_state(removals=[removal()])))
        self.env = {key: value for key, value in os.environ.items() if not key.startswith("BMAC_UI")}
        self.env.update({
            "APP_HA_CONFIG_TEST_MODE": "1",
            "APP_HA_CONFIG_DIR": str(root / "config"),
            "PROXMOX_SSH_KNOWN_HOSTS_FILE": str(known_hosts),
            "FAKE_ROOT": str(fake),
            "PATH": f"{root / 'bin'}:{os.environ['PATH']}",
        })

    def test_reads_every_online_host_without_changing_anything(self) -> None:
        run = run_json([str(LIST_DISKS)], scripted(), env=self.env, timeout=60)
        self.assertEqual(run.completed.get("status"), "success", run.describe())
        summary = run.of("result")[0]["data"]
        mox1, mox2, mox3 = summary["hosts"]
        self.assertEqual(
            {disk["serial"]: disk["status"] for disk in mox1["disks"]},
            {"S2EXTRA1": "awaiting_finalization", "S3EXTRA2": "awaiting_finalization",
             "S6BLANK": "available", "S7USED": "available"},
        )
        self.assertFalse(mox2["online"])
        self.assertIn("Connection timed out", mox3["error"])
        commands = (self.root / "fake" / "commands").read_text().splitlines()
        self.assertIn("mox1 python3 - collect", commands)
        self.assertIn("mox1 python3 - show", commands)
        self.assertFalse(any(line.startswith("mox2 ") for line in commands))

    def test_host_filter(self) -> None:
        completed = subprocess.run(
            [str(LIST_DISKS), "--host", "mox1"], capture_output=True, text=True,
            env=self.env, timeout=60,
        )
        self.assertEqual(completed.returncode, 0, completed.stderr)
        self.assertIn("DISKS ON mox1", completed.stdout)
        self.assertNotIn("DISKS ON mox3", completed.stdout)

        unknown = subprocess.run(
            [str(LIST_DISKS), "--host", "mox9"], capture_output=True, text=True,
            env=self.env, timeout=60,
        )
        self.assertEqual(unknown.returncode, 1)
        self.assertIn("mox9 is not a member of the cluster", unknown.stderr)


if __name__ == "__main__":
    unittest.main()
