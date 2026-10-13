# Copyright (c) 2026 BEENTHERE VENTURES, INC.
# SPDX-License-Identifier: GPL-3.0-only

"""Snapshots, reset, links, and the carrier supervisor, with a fake qemu-img."""

from __future__ import annotations

import io
import json
import os
import shutil
import sys
import tempfile
import textwrap
import unittest
from pathlib import Path
from unittest import mock

LAB = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(LAB))

from bmaclab.lab import Lab, LabError  # noqa: E402
from bmaclab.paths import Paths  # noqa: E402
from bmaclab.supervisor import Watch  # noqa: E402
from bmaclab.topology import load  # noqa: E402

# Keeps each disk's snapshot tags in <disk>.tags so `-l` can list them.
FAKE_QEMU_IMG = textwrap.dedent("""\
    #!/usr/bin/env python3
    import json, sys
    from pathlib import Path
    args = sys.argv[1:]
    with open(Path(__file__).with_name("calls.log"), "a") as log:
        log.write(" ".join(args) + "\\n")
    if args[0] == "snapshot":
        op, disk = args[1], Path(args[-1])
        tags_file = disk.with_name(disk.name + ".tags")
        tags = json.loads(tags_file.read_text()) if tags_file.exists() else {}
        if op == "-l":
            print("Snapshot list:")
            print("ID        TAG               VM SIZE                DATE     VM CLOCK     ICOUNT")
            for n, tag in enumerate(tags, 1):
                print(f"{n}         {tag}                 0 B 2026-10-10 16:00:00 00:00:00.000          0")
        elif op == "-c":
            tags[args[2]] = disk.read_text()
        elif op == "-d":
            del tags[args[2]]
        elif op == "-a":
            if args[2] not in tags:
                sys.exit(f"qemu-img: Could not apply snapshot '{args[2]}'")
            disk.write_text(tags[args[2]])
        tags_file.write_text(json.dumps(tags))
""")


class LabTestCase(unittest.TestCase):
    def setUp(self) -> None:
        scratch = tempfile.TemporaryDirectory(prefix="bmac-lab-ops-")
        self.addCleanup(scratch.cleanup)
        root = Path(scratch.name)
        bin_dir = root / "bin"
        bin_dir.mkdir()
        fake = bin_dir / "qemu-img"
        fake.write_text(FAKE_QEMU_IMG)
        fake.chmod(0o755)
        self.calls = bin_dir / "calls.log"
        patcher = mock.patch.dict(os.environ, {"PATH": f"{bin_dir}{os.pathsep}{os.environ['PATH']}"})
        patcher.start()
        self.addCleanup(patcher.stop)

        self.config = root / "lab.toml"
        shutil.copyfile(LAB / "lab.toml", self.config)
        self.topology = load(self.config)
        self.paths = Paths(root / "lab")
        self.out = io.StringIO()
        self.lab = Lab(self.topology, self.paths, log=lambda line: print(line, file=self.out))
        self.lab.ensure_supervisor = lambda: None  # type: ignore[method-assign]
        self.lab.secrets = root / "secrets.env"
        for vm in self.topology.vms:
            self.paths.vm(vm.name).mkdir(parents=True)
            self.paths.vars(vm.name).write_text(f"vars of {vm.name} v1")
            for disk in vm.disks:
                self.paths.disk(disk).write_text(f"{disk.id} v1")
        self.paths.state.write_text(json.dumps({"topology_sha256": self.lab.topology_digest}))


class SnapshotTest(LabTestCase):
    def test_snapshot_and_reset_restore_every_disk_and_firmware_store(self) -> None:
        self.lab.snapshot("installed")
        self.assertEqual(self.lab.snapshot_names(), ["installed"])
        mox1 = self.topology.vm("mox1")
        disk = self.paths.disk(mox1.disks[0])
        disk.write_text("changed")
        self.paths.vars("mox1").write_text("changed vars")
        self.paths.link_held("mox1", "vlan").write_text("held")
        self.paths.carrier_until("mox1").write_text("123")

        self.lab.reset("installed")
        self.assertEqual(disk.read_text(), f"{mox1.disks[0].id} v1")
        self.assertEqual(self.paths.vars("mox1").read_text(), "vars of mox1 v1")
        self.assertFalse(self.paths.link_held("mox1", "vlan").exists())
        self.assertFalse(self.paths.carrier_until("mox1").exists())

    def test_a_checkpoint_puts_back_one_vm_only(self) -> None:
        mox3, mox1 = self.topology.vm("mox3"), self.topology.vm("mox1")
        self.lab.checkpoint(mox3, "before")
        for vm in (mox3, mox1):
            self.paths.disk(vm.disks[0]).write_text("reinstalled")
        self.paths.vars("mox3").write_text("new vars")
        self.paths.link_held("mox3", "uplink").write_text("held")
        self.lab.restore_checkpoint(mox3, "before")
        self.assertEqual(self.paths.disk(mox3.disks[0]).read_text(), f"{mox3.disks[0].id} v1")
        self.assertEqual(self.paths.vars("mox3").read_text(), "vars of mox3 v1")
        self.assertEqual(self.paths.disk(mox1.disks[0]).read_text(), "reinstalled")
        self.assertFalse(self.paths.link_held("mox3", "uplink").exists())
        self.assertIn(f"snapshot -d before {self.paths.disk(mox3.disks[0])}", self.calls.read_text().splitlines())
        with self.assertRaisesRegex(LabError, "no checkpoint named 'before'"):
            self.lab.restore_checkpoint(mox3, "before")

    def test_a_stage_outlives_restores_until_dropped(self) -> None:
        mox3, mox1 = self.topology.vm("mox3"), self.topology.vm("mox1")
        files = self.paths.root / "work"
        files.mkdir()
        (files / "card.txt").write_text("card v1")
        self.lab.save_stage(mox3, "cluster", files, {"password": "made-up"})
        self.assertEqual(self.lab.stage_names(mox3), ["cluster"])
        self.assertEqual(self.lab.stage_names(mox1), [])
        self.assertEqual((self.paths.vm("mox3") / "stages" / "cluster" / "stage.json").stat().st_mode & 0o777, 0o600)
        disk = str(self.paths.disk(mox3.disks[0]))
        self.assertIn(f"snapshot -c stage-cluster {disk}", self.calls.read_text().splitlines())

        (files / "card.txt").write_text("card v2")
        self.paths.vars("mox3").write_text("new vars")
        for _ in range(2):
            meta = self.lab.restore_stage(mox3, "cluster", files)
            self.assertEqual(meta["password"], "made-up")
            self.assertEqual((files / "card.txt").read_text(), "card v1")
            self.assertEqual(self.paths.vars("mox3").read_text(), "vars of mox3 v1")
        self.assertIn(f"snapshot -a stage-cluster {disk}", self.calls.read_text().splitlines())
        self.assertNotIn(f"snapshot -d stage-cluster {disk}", self.calls.read_text().splitlines())

        self.lab.drop_stages(mox3)
        self.assertEqual(self.lab.stage_names(mox3), [])
        self.assertIsNone(self.lab.stage(mox3, "cluster"))
        with self.assertRaisesRegex(LabError, "no stage named 'cluster'"):
            self.lab.restore_stage(mox3, "cluster", files)

    def test_a_vm_with_an_nvme_controller_is_not_saved_live(self) -> None:
        mox3, appliance = self.topology.vm("mox3"), self.topology.vm("appliance")
        self.assertFalse(self.lab.can_save_live(mox3))
        self.assertTrue(self.lab.can_save_live(appliance))
        self.lab.running = lambda vm: True
        with self.assertRaisesRegex(LabError, "can't save live"):
            self.lab.save_stage(mox3, "cluster", self.paths.root, {}, live=True)

    def test_a_live_stage_job_covers_the_disks_and_waits_for_the_job(self) -> None:
        mox3 = self.topology.vm("mox3")

        class Monitor:
            def __init__(self, error: str | None = None) -> None:
                self.calls: list[tuple[str, dict]] = []
                self.polls, self.error = 0, error

            def command(self, name: str, /, **arguments: object) -> object:
                self.calls.append((name, arguments))
                if name == "query-block":
                    return [{"device": "cd0"}, *({"device": f"d-{d.id}", "inserted": {"node-name": f"#n{i}"}}
                                                 for i, d in enumerate(mox3.disks))]
                if name == "query-jobs":
                    self.polls += 1
                    status = "concluded" if self.polls > 1 else "running"
                    return [{"id": "snapshot-save-mox3", "status": status, **({"error": self.error} if self.error else {})}]
                return {}

        monitor = Monitor()
        self.lab._snapshot_job(monitor, mox3, "snapshot-save", "stage-x")
        nodes = [f"#n{i}" for i in range(len(mox3.disks))]
        self.assertEqual(monitor.calls[1], ("snapshot-save", {"job-id": "snapshot-save-mox3", "tag": "stage-x",
                                                              "devices": nodes, "vmstate": "#n0"}))
        self.assertEqual(monitor.calls[-1], ("job-dismiss", {"id": "snapshot-save-mox3"}))
        with self.assertRaisesRegex(LabError, "snapshot-save of mox3: no space"):
            self.lab._snapshot_job(Monitor("no space"), mox3, "snapshot-save", "stage-x")

    def test_retaking_a_snapshot_replaces_it(self) -> None:
        self.lab.snapshot("base")
        self.lab.snapshot("base")
        calls = self.calls.read_text().splitlines()
        disk = str(self.paths.disk(self.topology.vms[0].disks[0]))
        self.assertIn(f"snapshot -d base {disk}", calls)
        self.assertEqual(calls.count(f"snapshot -c base {disk}"), 2)

    def test_delete_removes_tags_and_the_folder(self) -> None:
        self.lab.snapshot("old")
        self.lab.delete_snapshot("old")
        self.assertEqual(self.lab.snapshot_names(), [])
        self.assertFalse((self.paths.snapshots / "old").exists())

    def test_bad_and_unknown_names_are_refused(self) -> None:
        with self.assertRaisesRegex(LabError, "snapshot names"):
            self.lab.snapshot("Bad Name")
        with self.assertRaisesRegex(LabError, "no snapshot named 'nope'"):
            self.lab.reset("nope")

    def test_operations_need_a_created_lab(self) -> None:
        self.paths.state.unlink()
        with self.assertRaisesRegex(LabError, "run `lab/lab create` first"):
            self.lab.snapshot("x")

    def test_a_changed_topology_is_reported(self) -> None:
        self.config.write_text(self.config.read_text() + "\n# edited\n")
        self.lab.require_created()
        self.assertIn("changed since the lab was created", self.out.getvalue())


class LinkAndAccessTest(LabTestCase):
    def test_links_are_held_and_released_while_the_vm_is_off(self) -> None:
        mox2 = self.topology.vm("mox2")
        self.lab.link(mox2, "vlan", False)
        self.assertTrue(self.paths.link_held("mox2", "vlan").exists())
        self.assertEqual(self.lab.status()[1]["held"], ["vlan"])
        self.lab.link(mox2, "vlan", True)
        self.assertFalse(self.paths.link_held("mox2", "vlan").exists())
        with self.assertRaisesRegex(LabError, "has no vlan NIC"):
            self.lab.link(self.topology.vm("qdevice"), "vlan", False)

    def test_the_uplink_can_be_held_down(self) -> None:
        mox2 = self.topology.vm("mox2")
        self.lab.link(mox2, "uplink", False)
        self.lab.link(mox2, "vlan", False)
        self.assertEqual(self.lab.status()[1]["held"], ["uplink", "vlan"])
        self.lab.snapshot("base")
        self.lab.reset("base")
        self.assertFalse(self.paths.link_held("mox2", "uplink").exists())

    def test_carrier_delay_override(self) -> None:
        mox3 = self.topology.vm("mox3")
        self.assertEqual(self.lab.carrier_delay(mox3), 0)
        self.lab.set_carrier_delay(mox3, 45)
        self.assertEqual(self.lab.carrier_delay(mox3), 45)
        self.assertEqual(self.lab.status()[2]["carrier_delay"], 45)
        with self.assertRaisesRegex(LabError, "no VLAN NIC"):
            self.lab.set_carrier_delay(self.topology.vm("appliance"), 5)

    def test_vms_without_a_pidfile_are_not_running(self) -> None:
        mox1 = self.topology.vm("mox1")
        self.assertFalse(self.lab.running(mox1))
        self.paths.pidfile("mox1").write_text(f"{os.getpid()}\n")
        self.assertFalse(self.lab.running(mox1), "a live PID that is not this VM's QEMU does not count")
        with self.assertRaisesRegex(LabError, "not running"):
            self.lab.qmp(mox1)

    def test_ssh_uses_the_lab_key_and_forwarded_port(self) -> None:
        mox1 = self.topology.vm("mox1")
        args = self.lab.ssh_args(mox1, ["pveversion"])
        self.assertEqual(args[:5], ["ssh", "-i", str(self.paths.ssh_key), "-p", "22002"])
        self.assertEqual(args[-2:], ["root@127.0.0.1", "pveversion"])
        self.assertIn("BatchMode=yes", args)


class FakeMonitor:
    def __init__(self) -> None:
        self.commands: list[tuple[str, dict]] = []

    def command(self, name: str, /, **arguments: object) -> None:
        self.commands.append((name, arguments))


class SupervisorTest(LabTestCase):
    def watch(self, name: str) -> tuple[Watch, FakeMonitor, list[float]]:
        now = [1000.0]
        watch = Watch(self.lab, self.topology.vm(name), clock=lambda: now[0])
        monitor = FakeMonitor()
        watch.monitor = monitor  # type: ignore[assignment]
        return watch, monitor, now

    def test_a_reset_drops_the_carrier_until_the_delay_passes(self) -> None:
        self.lab.set_carrier_delay(self.topology.vm("mox2"), 30)
        watch, monitor, now = self.watch("mox2")
        watch.on_reset()
        self.assertEqual(monitor.commands, [("set_link", {"name": "nic-vlan", "up": False})])
        self.assertEqual(float(self.paths.last_reset("mox2").read_text()), 1000.0)
        now[0] = 1029.0
        watch.raise_when_due()
        self.assertEqual(len(monitor.commands), 1)
        now[0] = 1030.0
        watch.raise_when_due()
        self.assertEqual(monitor.commands[-1], ("set_link", {"name": "nic-vlan", "up": True}))
        self.assertFalse(self.paths.carrier_until("mox2").exists())

    def test_no_delay_leaves_the_link_alone(self) -> None:
        watch, monitor, _ = self.watch("mox1")
        watch.on_reset()
        self.assertEqual(monitor.commands, [])
        self.assertTrue(self.paths.last_reset("mox1").exists())

    def test_a_held_link_is_never_raised(self) -> None:
        self.lab.set_carrier_delay(self.topology.vm("mox3"), 10)
        watch, monitor, now = self.watch("mox3")
        self.paths.carrier_until("mox3").write_text("1005")
        self.lab.link(self.topology.vm("mox3"), "vlan", False)
        now[0] = 2000.0
        watch.raise_when_due()
        self.assertEqual(monitor.commands, [])
        self.assertFalse(self.paths.carrier_until("mox3").exists())


if __name__ == "__main__":
    unittest.main()
