#!/usr/bin/env python3

# Copyright (c) 2026 BEENTHERE VENTURES, INC.
# SPDX-License-Identifier: GPL-3.0-only

"""scripts/host_runtime/instant_switch.py: moving a running VM from its
overlay to its zvol, against a fake QEMU block graph and fake PVE tools."""

from __future__ import annotations

import hashlib
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest


HELPER = Path(__file__).resolve().parents[1] / "host_runtime" / "instant_switch.py"
sys.path.insert(0, str(HELPER.parent))
import instant_switch  # noqa: E402

VMID = "120"
OVERLAY = f"bmac-overlay:{VMID}/vm-{VMID}-disk-1.qcow2"
TARGET = f"local-zfs:vm-{VMID}-disk-1"
SIZE = 32 << 30


def name(prefix: str, volume: str) -> str:
    return prefix + hashlib.sha256(f"drive=scsi0,volid={volume}".encode()).hexdigest()[:30]


# The backend (QMP and config), qm, and pvesm over one JSON state. The block
# graph keeps format nodes with their file and backing children; a commit job
# turns ready on the next query, and job-complete makes its base the disk.
FAKE = r'''#!/usr/bin/env python3
import hashlib, json, os, shlex, sys
from pathlib import Path

command = Path(sys.argv[0]).name
args = sys.argv[1:]
state_path = Path(os.environ["FAKE_STATE"])
state = json.loads(state_path.read_text())

def save():
    state_path.write_text(json.dumps(state))

def name(prefix, volume):
    return prefix + hashlib.sha256(f"drive=scsi0,volid={volume}".encode()).hexdigest()[:30]

def blockdev(drive):
    volume = drive.split(",")[0]
    if volume.startswith("bmac-overlay:"):
        fmt, driver, path = "qcow2", "file", state["overlay_path"]
    else:
        fmt, driver, path = "raw", "host_device", state["target_path"]
    return {"cache": {"direct": True, "no-flush": False}, "detect-zeroes": "unmap", "discard": "unmap",
            "driver": fmt, "node-name": state.get("name_override") or name("f", volume), "read-only": False,
            "file": {"aio": "io_uring", "cache": {"direct": True, "no-flush": False}, "detect-zeroes": "unmap",
                     "discard": "unmap", "driver": driver, "filename": path, "node-name": name("e", volume),
                     "read-only": False}}

def add(node):
    file = node["file"]
    state["nodes"][file["node-name"]] = {"file": file["filename"], "children": {}}
    state["nodes"][node["node-name"]] = {"file": None, "children": {"file": file["node-name"]}}

def nodes():
    return [{"node-name": key, "file": value["file"],
             "children": [{"child": role, "node-name": child} for role, child in value["children"].items()]}
            for key, value in state["nodes"].items()]

state.setdefault("calls", []).append([command, *args[:3]])
save()
if command == "qm":
    if args[0] == "status":
        print(f"status: {state['status']}")
    elif args[0] == "showcmd":
        print("/usr/bin/kvm -id 120 -blockdev " + shlex.quote(json.dumps(
            {"driver": "throttle", "node-name": "drive-scsi0", "file": blockdev(state["config"]["scsi0"])})))
elif command == "pvesm":
    print(state["target_path"])
elif command == "backend":
    action, vmid = args[0], args[1]
    if action == "config":
        print(json.dumps({"scsi0": state["config"]["scsi0"], "lock": state["config"].get("lock"), "pending": None}))
    elif action == "set-scsi0":
        assert state["config"]["scsi0"] == args[2]
        state["config"]["scsi0"] = args[3]
        save()
    elif action == "qmp":
        qmp, arguments = args[2], json.loads(args[3])
        state.setdefault("qmp", []).append([qmp, arguments])
        job = state.get("job")
        result = {}
        if qmp == "query-named-block-nodes":
            result = nodes()
        elif qmp == "blockdev-add":
            add(arguments)
        elif qmp == "blockdev-reopen":
            options = arguments["options"][0]
            state["nodes"][options["node-name"]]["children"]["backing"] = options["backing"]
        elif qmp == "block-commit":
            state["job"] = {"id": arguments["job-id"], "status": "running", "base": arguments["base-node"],
                            "current-progress": 0, "total-progress": 100}
        elif qmp == "query-jobs":
            result = [job] if job else []
            if job and job["status"] == "running":
                job["status"] = state.get("job_outcome", "ready")
                if job["status"] == "concluded":
                    job["error"] = "Input/output error"
        elif qmp == "job-complete":
            job["status"] = "concluded"
            top = state["nodes"]["drive-scsi0"]["children"]["file"]
            state["nodes"]["drive-scsi0"]["children"]["file"] = job["base"]
            for node in (top, state["nodes"][top]["children"]["file"]):
                state["nodes"].pop(node)
        elif qmp == "job-dismiss":
            state["job"] = None
        save()
        print(json.dumps(result))
'''


class InstantSwitchTest(unittest.TestCase):
    def setUp(self) -> None:
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        root = Path(self.temporary.name)
        self.root = root
        (root / "bin").mkdir()
        fake = root / "bin" / "fake"
        fake.write_text(FAKE)
        fake.chmod(0o755)
        for command in ("qm", "pvesm", "backend"):
            (root / "bin" / command).symlink_to(fake)
        self.target_path = root / "zvol"
        with open(self.target_path, "wb") as device:
            device.truncate(SIZE)
        overlay_path = f"/var/lib/bmac-overlay/images/{VMID}/vm-{VMID}-disk-1.qcow2"
        self.save({
            "status": "running", "overlay_path": overlay_path, "target_path": str(self.target_path), "job": None,
            "config": {"scsi0": f"{OVERLAY},discard=on,iothread=1,replicate=1,size=32G,ssd=1"},
            "nodes": {
                "drive-scsi0": {"file": None, "children": {"file": name("f", OVERLAY)}},
                name("f", OVERLAY): {"file": None, "children": {"file": name("e", OVERLAY), "backing": "#block101"}},
                name("e", OVERLAY): {"file": overlay_path, "children": {}},
                "#block101": {"file": None, "children": {"file": "#block100"}},
                "#block100": {"file": f"/run/bmac-backup-link/{VMID}.img", "children": {}},
            },
        })

    def save(self, value: dict) -> None:
        (self.root / "state.json").write_text(json.dumps(value))

    def load(self) -> dict:
        return json.loads((self.root / "state.json").read_text())

    def helper(self, command: str, expected: int = 0) -> subprocess.CompletedProcess[str]:
        environment = {**os.environ, "FAKE_STATE": str(self.root / "state.json"),
                       "APP_HA_INSTANT_SWITCH_BACKEND": str(self.root / "bin" / "backend"),
                       "PATH": f"{self.root / 'bin'}:{os.environ['PATH']}"}
        completed = subprocess.run([sys.executable, str(HELPER), command, VMID, OVERLAY, TARGET], env=environment,
                                   text=True, capture_output=True, check=False)
        self.assertEqual(completed.returncode, expected, msg=f"stdout:\n{completed.stdout}\nstderr:\n{completed.stderr}")
        return completed

    def qmp_calls(self) -> list[str]:
        return [call[0] for call in self.load().get("qmp", []) if not call[0].startswith("query-")]

    def test_switch_adds_the_zvol_reopens_commits_and_then_points_the_config_at_it(self) -> None:
        self.assertEqual(self.helper("state").stdout.strip(), "backup")
        self.assertEqual(self.helper("switch").stdout.strip(), "switched")
        state = self.load()
        self.assertEqual(self.qmp_calls(), ["blockdev-add", "blockdev-reopen", "block-commit", "job-complete",
                                            "job-dismiss"])
        calls = {call[0]: call[1] for call in state["qmp"]}
        added = calls["blockdev-add"]
        self.assertEqual((added["driver"], added["node-name"]), ("raw", name("f", TARGET)))
        self.assertEqual(added["file"], {"aio": "io_uring", "cache": {"direct": True, "no-flush": False},
                                         "detect-zeroes": "unmap", "discard": "unmap", "driver": "host_device",
                                         "filename": str(self.target_path), "node-name": name("e", TARGET),
                                         "read-only": False})
        reopened = calls["blockdev-reopen"]["options"][0]
        self.assertEqual((reopened["driver"], reopened["node-name"], reopened["file"], reopened["backing"]),
                         ("qcow2", name("f", OVERLAY), name("e", OVERLAY), name("f", TARGET)))
        self.assertEqual(calls["block-commit"], {
            "job-id": f"bmac-switch-{VMID}", "device": "drive-scsi0", "top-node": name("f", OVERLAY),
            "base-node": name("f", TARGET), "auto-finalize": True, "auto-dismiss": False})
        self.assertEqual(state["config"]["scsi0"], f"{TARGET},discard=on,iothread=1,replicate=1,size=32G,ssd=1")
        self.assertEqual(self.helper("state").stdout.strip(), "switched")
        # Repeating it changes nothing.
        self.helper("switch")
        self.assertEqual(len(self.qmp_calls()), 5)

    def test_the_config_keeps_the_overlay_until_the_commit_completes(self) -> None:
        state = self.load()
        state["job_outcome"] = "concluded"
        self.save(state)
        completed = self.helper("switch", expected=1)
        self.assertIn("failed: Input/output error", completed.stderr)
        state = self.load()
        self.assertTrue(state["config"]["scsi0"].startswith(OVERLAY + ","))
        self.assertEqual(self.helper("state").stdout.strip(), "target")
        # A rerun neither re-adds nor reopens: it commits again from where QEMU is.
        state["job_outcome"] = "ready"
        state["qmp"] = []
        self.save(state)
        self.helper("switch")
        self.assertEqual(self.qmp_calls(), ["block-commit", "job-complete", "job-dismiss"])
        self.assertTrue(self.load()["config"]["scsi0"].startswith(TARGET + ","))

    def test_a_job_left_ready_by_an_interrupted_run_is_completed(self) -> None:
        state = self.load()
        state["nodes"][name("f", TARGET)] = {"file": None, "children": {"file": name("e", TARGET)}}
        state["nodes"][name("e", TARGET)] = {"file": str(self.target_path), "children": {}}
        state["nodes"][name("f", OVERLAY)]["children"]["backing"] = name("f", TARGET)
        state["job"] = {"id": f"bmac-switch-{VMID}", "status": "ready", "base": name("f", TARGET)}
        self.save(state)
        self.helper("switch")
        self.assertEqual(self.qmp_calls(), ["job-complete", "job-dismiss"])

    def test_nothing_changes_when_proxmox_names_its_nodes_differently(self) -> None:
        state = self.load()
        top = state["nodes"].pop(name("f", OVERLAY))
        state["nodes"]["fsomethingelse"] = top
        state["nodes"]["drive-scsi0"]["children"]["file"] = "fsomethingelse"
        self.save(state)
        completed = self.helper("switch", expected=1)
        self.assertIn("names its block nodes differently", completed.stderr)
        self.assertEqual(self.qmp_calls(), [])

    def test_a_stopped_vm_is_reported_and_not_switched(self) -> None:
        state = self.load()
        state["status"] = "stopped"
        self.save(state)
        self.assertEqual(self.helper("state").stdout.strip(), "stopped")
        self.assertIn("is not running", self.helper("switch", expected=1).stderr)

    def test_a_locked_vm_or_foreign_disk_is_refused(self) -> None:
        state = self.load()
        state["config"]["lock"] = "backup"
        self.save(state)
        self.assertIn("is locked (backup)", self.helper("switch", expected=1).stderr)
        state["config"] = {"scsi0": "local-zfs:vm-120-disk-7,size=32G"}
        self.save(state)
        self.assertIn("neither the overlay nor the target", self.helper("state", expected=1).stderr)

    def test_switched_drive_keeps_options_and_sets_the_zvols_size(self) -> None:
        drive = f"{OVERLAY},discard=on,iothread=1,replicate=1,size=40G,ssd=1"
        self.assertEqual(instant_switch.switched_drive(drive, TARGET, SIZE),
                         f"{TARGET},discard=on,iothread=1,replicate=1,size=32G,ssd=1")
        self.assertEqual(instant_switch.format_size(3 << 40), "3T")
        self.assertEqual(instant_switch.format_size((1 << 30) + (512 << 20)), "1536M")
        self.assertEqual(instant_switch.format_size(1000), "1000")


if __name__ == "__main__":
    unittest.main()
