#!/usr/bin/env python3

# Copyright (c) 2026 BEENTHERE VENTURES, INC.
# SPDX-License-Identifier: GPL-3.0-only

"""scripts/host_runtime/backup_link.sh: the PBS mapping behind a
backup-linked staging VM, against fake systemd, PBS client, and PVE tools."""

from __future__ import annotations

import json
import os
from pathlib import Path
import subprocess
import tempfile
import unittest


HELPER = Path(__file__).resolve().parents[1] / "host_runtime" / "backup_link.sh"
SNAPSHOT = "vm/100/2026-10-27T16:04:23Z"
TOKEN = "11111111-2222-3333-4444-555555555555"

STORAGE_CFG = """dir: local
	path /var/lib/vz

pbs: bmac-pbs
	datastore pbs
	server 203.0.113.7
	content backup
	fingerprint 5d:17:0f
	namespace BTV
	username btv@pbs!pve
"""

# systemctl, systemd-run, proxmox-backup-client, pvesm, and qemu-img over one
# JSON state: active scopes, mappings by loop device, and recorded calls.
FAKE = r'''#!/usr/bin/env python3
import json, os, sys
from pathlib import Path

command = Path(sys.argv[0]).name
args = sys.argv[1:]
state_path = Path(os.environ["FAKE_STATE"])
state = json.loads(state_path.read_text())
link_dir = Path(os.environ["APP_HA_BACKUP_LINK_DIR"])

def save():
    state_path.write_text(json.dumps(state))

state.setdefault("calls", []).append([command, *args])
save()
if command == "systemctl":
    unit = args[-1]
    if args[0] == "is-active":
        sys.exit(0 if unit in state["scopes"] else 3)
    if args[0] == "stop":
        if unit in state["scopes"]:
            state["scopes"].remove(unit)
        save()
        sys.exit(0)
elif command == "systemd-run":
    unit = args[args.index("--unit") + 1]
    state["map_env"] = {key: os.environ.get(key) for key in ("PBS_PASSWORD", "PBS_REPOSITORY", "PBS_FINGERPRINT")}
    state["map_args"] = args[args.index("proxmox-backup-client") + 1:]
    loop = link_dir / f"fake-loop{state['next_loop']}"
    loop.write_text("")
    state["next_loop"] += 1
    state["scopes"].append(unit)
    state["maps"][str(loop)] = state["map_args"][1]
    save()
    print(f"Image 'btv@pbs!pve@203.0.113.7:8007:pbs:{state['map_args'][1]}/drive-scsi0.img' mapped on {loop}",
          file=sys.stderr)
elif command == "proxmox-backup-client":
    if args == ["unmap"]:
        for loop, snapshot in state["maps"].items():
            print(f"{loop}:\tbtv@pbs!pve@203.0.113.7:8007:pbs:{snapshot}/drive-scsi0.img", file=sys.stderr)
    elif args[0] == "unmap":
        state["maps"].pop(args[1], None)
        save()
elif command == "pvesm":
    print(state["paths"][args[1]])
elif command == "qemu-img":
    Path(args[-1]).write_text("qcow2")
else:
    sys.exit(f"unexpected {command} {args}")
'''


class BackupLinkTest(unittest.TestCase):
    def setUp(self) -> None:
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        root = Path(self.temporary.name)
        self.root = root
        self.links = root / "links"
        self.links.mkdir()
        (root / "storage.cfg").write_text(STORAGE_CFG)
        (root / "priv").mkdir()
        (root / "priv" / "bmac-pbs.pw").write_text(TOKEN + "\n")
        (root / "priv" / "bmac-pbs.enc").write_text("{}")
        (root / "bin").mkdir()
        fake = root / "bin" / "fake"
        fake.write_text(FAKE)
        fake.chmod(0o755)
        for name in ("systemctl", "systemd-run", "proxmox-backup-client", "pvesm", "qemu-img"):
            (root / "bin" / name).symlink_to(fake)
        self.overlay = root / "overlay" / "images" / "901" / "vm-901-disk-1.qcow2"
        self.save({"scopes": [], "maps": {}, "next_loop": 0,
                   "paths": {"bmac-overlay:901/vm-901-disk-1.qcow2": str(self.overlay)}})

    def save(self, value: dict) -> None:
        (self.root / "state.json").write_text(json.dumps(value))

    def load(self) -> dict:
        return json.loads((self.root / "state.json").read_text())

    def helper(self, *arguments: str, expected: int = 0) -> subprocess.CompletedProcess[str]:
        environment = {
            **{k: v for k, v in os.environ.items() if not k.startswith("PBS_")},
            "APP_HA_BACKUP_LINK_TEST_MODE": "1", "APP_HA_BACKUP_LINK_DIR": str(self.links),
            "APP_HA_STORAGE_CFG": str(self.root / "storage.cfg"), "APP_HA_STORAGE_PRIV": str(self.root / "priv"),
            "FAKE_STATE": str(self.root / "state.json"), "PATH": f"{self.root / 'bin'}:{os.environ['PATH']}",
        }
        completed = subprocess.run(["bash", str(HELPER), *arguments], env=environment, text=True,
                                   capture_output=True, check=False)
        self.assertEqual(completed.returncode, expected, msg=f"stdout:\n{completed.stdout}\nstderr:\n{completed.stderr}")
        return completed

    def test_map_uses_the_storage_and_passes_the_token_only_in_the_environment(self) -> None:
        completed = self.helper("map", "901", SNAPSHOT)
        link = self.links / "901.img"
        self.assertEqual(completed.stdout.strip(), str(link))
        self.assertEqual(os.readlink(link), str(self.links / "fake-loop0"))
        state = self.load()
        self.assertEqual(state["map_env"], {"PBS_PASSWORD": TOKEN, "PBS_REPOSITORY": "btv@pbs!pve@203.0.113.7:8007:pbs",
                                            "PBS_FINGERPRINT": "5d:17:0f"})
        self.assertEqual(state["map_args"], ["map", SNAPSHOT, "drive-scsi0.img", "--ns", "BTV",
                                             "--keyfile", str(self.root / "priv" / "bmac-pbs.enc")])
        self.assertFalse(any(TOKEN in " ".join(call) for call in state["calls"]))
        self.assertIn("bmac-backup-link-901.scope", state["scopes"])
        self.assertEqual(self.helper("status", "901").stdout.strip(), "alive")

    def test_a_second_map_reuses_the_live_mapping_and_refuses_another_point(self) -> None:
        self.helper("map", "901", SNAPSHOT)
        self.helper("map", "901", SNAPSHOT)
        self.assertEqual(self.load()["next_loop"], 1)
        completed = self.helper("map", "901", "vm/100/2026-10-26T10:00:00Z", expected=1)
        self.assertIn("already has a mapping of another restore point", completed.stderr)

    def test_status_after_the_process_dies_and_after_a_reboot(self) -> None:
        self.assertEqual(self.helper("status", "901").stdout.strip(), "absent")
        self.helper("map", "901", SNAPSHOT)
        state = self.load()
        state["scopes"] = []
        self.save(state)
        self.assertEqual(self.helper("status", "901").stdout.strip(), "dead")
        # A dead mapping is replaced by a fresh one on the next map.
        self.helper("map", "901", SNAPSHOT)
        self.assertEqual(self.helper("status", "901").stdout.strip(), "alive")

    def test_unmap_removes_only_this_vms_mapping(self) -> None:
        self.helper("map", "901", SNAPSHOT)
        self.helper("unmap", "901")
        state = self.load()
        self.assertEqual(state["maps"], {})
        self.assertNotIn("bmac-backup-link-901.scope", state["scopes"])
        self.assertFalse((self.links / "901.img").exists())
        self.assertEqual(self.helper("status", "901").stdout.strip(), "absent")
        # After a dead mapping its loop number may belong to another restore point: never unmap that one.
        self.helper("map", "901", SNAPSHOT)
        state = self.load()
        state["scopes"] = []
        state["maps"] = {str(self.links / "fake-loop1"): "vm/200/2026-10-27T00:00:00Z"}
        self.save(state)
        self.helper("unmap", "901")
        self.assertEqual(self.load()["maps"], {str(self.links / "fake-loop1"): "vm/200/2026-10-27T00:00:00Z"})
        self.helper("unmap", "901")

    def test_create_overlay_needs_a_live_mapping_and_backs_onto_the_stable_path(self) -> None:
        completed = self.helper("create-overlay", "901", "bmac-overlay:901/vm-901-disk-1.qcow2", expected=1)
        self.assertIn("has no live mapping", completed.stderr)
        self.helper("map", "901", SNAPSHOT)
        self.helper("create-overlay", "901", "bmac-overlay:901/vm-901-disk-1.qcow2")
        create = [call for call in self.load()["calls"] if call[0] == "qemu-img"][0]
        self.assertEqual(create[1:], ["create", "-q", "-f", "qcow2", "-F", "raw", "-b", str(self.links / "901.img"),
                                      str(self.overlay)])
        completed = self.helper("create-overlay", "901", "bmac-overlay:901/vm-901-disk-1.qcow2", expected=1)
        self.assertIn("already exists", completed.stderr)
        completed = self.helper("create-overlay", "901", "bmac-overlay:902/vm-902-disk-1.qcow2", expected=1)
        self.assertIn("VOLUME must be STORAGE:901/vm-901-disk-N.qcow2", completed.stderr)


if __name__ == "__main__":
    unittest.main()
