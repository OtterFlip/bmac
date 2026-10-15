# Copyright (c) 2026 BEENTHERE VENTURES, INC.
# SPDX-License-Identifier: GPL-3.0-only

"""Snapshots, reset, links, and the carrier supervisor, with a fake qemu-img."""

from __future__ import annotations

import io
import json
import os
import shutil
import socket
import subprocess
import sys
import tempfile
import textwrap
import threading
import time
import unittest
from pathlib import Path
from unittest import mock

LAB = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(LAB))

from bmaclab.lab import Lab, LabError, process_gone  # noqa: E402
from bmaclab.paths import Paths  # noqa: E402
from bmaclab.supervisor import Switch, Watch  # noqa: E402
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
    elif args[0] == "create":
        Path(args[-2]).write_text("created")
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


class ProcessGoneTest(unittest.TestCase):
    def test_a_live_process_is_there_and_a_zombie_or_reaped_one_is_gone(self) -> None:
        self.assertFalse(process_gone(os.getpid()))
        child = subprocess.Popen(["true"])
        deadline = time.monotonic() + 10
        while not process_gone(child.pid) and time.monotonic() < deadline:
            time.sleep(0.01)
        self.assertTrue(process_gone(child.pid))
        child.wait()
        self.assertTrue(process_gone(child.pid))


class ScenarioLockTest(LabTestCase):
    def test_a_second_scenario_on_the_lab_is_refused_until_the_first_ends(self) -> None:
        other = Lab(self.topology, self.paths, log=lambda line: None)
        with self.lab.scenario_lock("m7_cluster"):
            with self.assertRaisesRegex(LabError, rf"another scenario is running on this lab: m7_cluster \(pid {os.getpid()}\)"):
                with other.scenario_lock("m7_join"):
                    pass
        with other.scenario_lock("m7_join"):
            pass

    def test_sigterm_unwinds_a_scenario_so_its_cleanup_runs(self) -> None:
        root = self.paths.root.parent
        script = root / "sleepy.py"
        script.write_text(textwrap.dedent(f"""\
            import time
            from pathlib import Path
            def run(s):
                try:
                    Path({str(root / "started")!r}).touch()
                    time.sleep(60)
                finally:
                    Path({str(root / "cleaned")!r}).touch()
        """))
        child = subprocess.Popen([sys.executable, str(LAB / "lab"), "--config", str(self.config), "scenario", str(script)],
                                 env={**os.environ, "BMAC_LAB_DIR": str(root / "empty-lab")},
                                 stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        self.addCleanup(child.kill)
        deadline = time.monotonic() + 30
        while not (root / "started").exists() and time.monotonic() < deadline:
            time.sleep(0.05)
        child.terminate()
        self.assertEqual(child.wait(timeout=30), 130)
        self.assertTrue((root / "cleaned").exists())


class SwitchTest(LabTestCase):
    def test_a_frame_reaches_every_other_vm_on_its_network_and_not_its_sender(self) -> None:
        members = [vm.name for vm in self.topology.vms if vm.nic("vlan")]
        self.assertGreaterEqual(len(members), 3)
        nics = {}
        for name in members:
            nic = socket.socket(socket.AF_UNIX, socket.SOCK_DGRAM)
            nic.bind(str(self.paths.nic(name, "vlan")))
            nic.settimeout(5)
            self.addCleanup(nic.close)
            nics[name] = nic
        switch = Switch(self.lab)
        self.addCleanup(switch.close)
        stopping = threading.Event()
        self.addCleanup(stopping.set)
        threading.Thread(target=switch.serve, args=(stopping,), daemon=True).start()
        sender, *others = members
        frame = bytes.fromhex("ffffffffffff525400000001") + b"\x08\x06" + b"arp" * 20
        nics[sender].sendto(frame, str(self.paths.switch("vlan")))
        for name in others:
            self.assertEqual(nics[name].recv(65536), frame, name)
        nics[sender].settimeout(0.5)
        with self.assertRaises(TimeoutError):
            nics[sender].recv(65536)

    def test_a_vm_that_stops_reading_does_not_silence_the_others(self) -> None:
        sender, stuck, reader = [vm.name for vm in self.topology.vms if vm.nic("vlan")][:3]
        nics = {}
        for name in (sender, stuck, reader):
            nic = socket.socket(socket.AF_UNIX, socket.SOCK_DGRAM)
            nic.bind(str(self.paths.nic(name, "vlan")))
            self.addCleanup(nic.close)
            nics[name] = nic
        switch = Switch(self.lab)
        self.addCleanup(switch.close)
        stopping = threading.Event()
        self.addCleanup(stopping.set)
        threading.Thread(target=switch.serve, args=(stopping,), daemon=True).start()
        last = b"\xff" * 12 + b"\x88\xb5" + b"last"
        seen = threading.Event()

        def read() -> None:
            nics[reader].settimeout(10)
            while nics[reader].recv(65536) != last:
                pass
            seen.set()

        threading.Thread(target=read, daemon=True).start()
        frame = b"\xff" * 12 + b"\x88\xb5" + b"x" * 1000
        for n in range(600):
            nics[sender].sendto(frame, str(self.paths.switch("vlan")))
            if n % 20 == 0:
                time.sleep(0.002)
        time.sleep(0.2)
        nics[sender].sendto(last, str(self.paths.switch("vlan")))
        self.assertTrue(seen.wait(10), f"{reader} heard nothing once {stuck} stopped reading")


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


class UpdateTest(LabTestCase):
    def test_an_added_appliance_gets_disks_and_the_labs_snapshots_and_every_ubuntu_vm_a_new_seed(self) -> None:
        self.lab.snapshot("installed")
        added = self.topology.vm("applianceb")
        shutil.rmtree(self.paths.vm(added.name))
        (self.paths.snapshots / "installed" / f"{added.name}.fd").unlink()
        firmware = self.paths.root / "OVMF_VARS.fd"
        firmware.write_text("blank vars")
        self.lab.firmware = (self.paths.root / "OVMF_CODE.fd", firmware)
        self.lab.require_tools = lambda: None  # type: ignore[method-assign]
        (self.paths.ssh_key.with_suffix(".pub")).write_text("ssh-ed25519 AAAA lab\n")
        seeded = []
        with mock.patch("bmaclab.provision.fetch", return_value=self.paths.root / "ubuntu.img"), \
                mock.patch("bmaclab.provision.password_hash", return_value="$6$x"), \
                mock.patch("bmaclab.provision.build_seed", side_effect=lambda t, vm, *a: seeded.append(vm.name)):
            self.lab.update()
        calls = self.calls.read_text().splitlines()
        system, backup = (self.paths.disk(d) for d in added.disks)
        self.assertIn(f"create -q -f qcow2 -F qcow2 -b {self.paths.root / 'ubuntu.img'} {system} 40G", calls)
        self.assertIn(f"create -q -f qcow2 {backup} 64G", calls)
        for disk in (system, backup):
            self.assertIn(f"snapshot -c installed {disk}", calls)
        self.assertEqual((self.paths.snapshots / "installed" / f"{added.name}.fd").read_text(), "blank vars")
        self.assertEqual(sorted(seeded), ["appliance", "applianceb", "qdevice"])
        self.assertFalse(any("create" in c and "mox1" in c for c in calls), "no host is touched")
        self.lab.reset("installed")
        self.assertEqual(json.loads(self.paths.state.read_text())["topology_sha256"], self.lab.topology_digest)

    def test_a_running_supervisor_is_started_again_with_the_topology(self) -> None:
        running = [41]
        events = []
        self.lab.supervisor_pid = lambda: running[0] if running else None  # type: ignore[method-assign]

        def stop() -> None:
            events.append("stop")
            running.clear()

        def start() -> None:
            events.append("start")
            running.append(42)
        self.lab.stop_supervisor = stop  # type: ignore[method-assign]
        self.lab.ensure_supervisor = start  # type: ignore[method-assign]
        self.lab.restart_supervisor()
        self.assertEqual((events, running), (["stop", "start"], [42]))
        running.clear()
        events.clear()
        self.lab.restart_supervisor()
        self.assertEqual(events, [], "a supervisor that isn't running isn't started")

    def test_a_new_host_needs_a_rebuild(self) -> None:
        shutil.rmtree(self.paths.vm("mox3"))
        self.lab.firmware = (self.paths.root / "code", self.paths.root / "vars")
        self.lab.require_tools = lambda: None  # type: ignore[method-assign]
        (self.paths.ssh_key.with_suffix(".pub")).write_text("ssh-ed25519 AAAA lab\n")
        with mock.patch("bmaclab.provision.password_hash", return_value="$6$x"):
            with self.assertRaisesRegex(LabError, "mox3 is a new Proxmox host"):
                self.lab.update()


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
            self.lab.set_carrier_delay(self.topology.vm("qdevice"), 5)

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


class PauseTest(LabTestCase):
    def test_every_vm_is_paused_before_any_resumes_and_a_stopped_one_is_skipped(self) -> None:
        sent: list[tuple[str, str]] = []

        class Monitor(FakeMonitor):
            def __init__(self, name: str) -> None:
                super().__init__()
                self.name = name

            def __enter__(self):
                return self

            def __exit__(self, *_: object) -> None:
                pass

            def command(self, name: str, /, **arguments: object) -> None:
                sent.append((self.name, name))

        vms = [self.topology.vm(n) for n in ("mox3", "mox1", "mox2")]
        running = {"mox3", "mox1", "mox2"}
        self.lab.running = lambda vm: vm.name in running  # type: ignore[method-assign]
        self.lab.qmp = lambda vm: Monitor(vm.name)  # type: ignore[method-assign,assignment]
        self.lab.pause(vms)
        running.discard("mox1")
        self.lab.resume(vms)
        self.assertEqual(sent, [("mox3", "stop"), ("mox1", "stop"), ("mox2", "stop"),
                                ("mox3", "cont"), ("mox2", "cont")])


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
