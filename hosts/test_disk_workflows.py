#!/usr/bin/env python3

# Copyright (c) 2026 BEENTHERE VENTURES, INC.
# SPDX-License-Identifier: GPL-3.0-only

"""Fake-SSH tests for the disk workflows: add_new_disk_vdev.sh,
add_replacement_disk.sh, decommission_disks.sh, and
list_disks_ready_for_physically_removal.sh.

One fake `ssh` plays the Proxmox host (the storage layout, registry,
replication, zpool, and the installed rpool mirror tool) and the production
guests reached as `ssh prodN`. The host's storage state file is handled by the
real lib/storage_state.py.
"""

from __future__ import annotations

import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import time
import unittest


HOSTS_DIR = Path(__file__).resolve().parent
LIB_DIR = HOSTS_DIR.parent / "lib"
ADD = HOSTS_DIR / "add_new_disk_vdev.sh"
REPLACE = HOSTS_DIR / "add_replacement_disk.sh"
DECOMMISSION = HOSTS_DIR / "decommission_disks.sh"
LIST = HOSTS_DIR / "list_disks_ready_for_physically_removal.sh"
sys.path.insert(0, str(LIB_DIR))
import test_host_storage as fixtures  # noqa: E402

REMOTE_TOOL = "/fake/sbin/app-ha-rpool-mirror"

FAKE_SSH = r'''#!/usr/bin/env python3
import hashlib
import json
import os
from pathlib import Path
import shlex
import subprocess
import sys
import time

state_path = Path(os.environ["FAKE_STATE"])
state = json.loads(state_path.read_text())
args = sys.argv[1:]
index = 0
while index < len(args) and args[index].startswith("-"):
    index += 2 if args[index] in ("-o", "-i", "-p") else 1
destination = args[index]
if "@" in destination:
    # mox_ssh sends %q-quoted words (including $'...'); split them as the
    # remote bash would.
    words = subprocess.run(
        ["bash", "-c", 'eval "set -- $1"; printf "%s\\0" "$@"', "_", " ".join(args[index + 1:])],
        check=True, capture_output=True, text=True,
    ).stdout.split("\0")[:-1]
else:
    words = shlex.split(" ".join(args[index + 1:]))
stdin = "" if sys.stdin.isatty() else sys.stdin.read()


def save():
    state_path.write_text(json.dumps(state, indent=1, sort_keys=True))


def act(*row):
    state["actions"].append([destination, *row])
    save()


layout = state["layout"]


def find_disk(serial):
    for disk in layout["unassigned_disks"] + layout["esp_only_disks"]:
        if disk["serial"] == serial:
            return disk
    for vdev in layout["vdevs"]:
        for member in vdev["members"]:
            if member["serial"] == serial:
                return {"disk": member["disk"], "serial": serial,
                        "model": member["model"], "size": member["disk_size"]}
    raise SystemExit(f"no disk with serial {serial}")

if "@" not in destination:  # a production guest reached as `ssh prodN`
    if destination in state.get("guest_ssh_fail", []):
        raise SystemExit(255)
    if words == ["true"]:
        raise SystemExit(0)
    if words == ["fstrim", "-v", "/"]:
        act("fstrim", destination)
        print("/: 12.5 GiB (13421772800 bytes) trimmed")
        raise SystemExit(0)
    raise SystemExit(f"unexpected guest command {destination} {words}")

node = destination.split("@", 1)[1]
if words == ["true"]:
    raise SystemExit(0)
if words == ["python3", "-", "collect"]:
    assert "def collect(" in stdin
    print(json.dumps(layout))
elif words[:2] == ["python3", "-"]:
    assert "Durable storage-maintenance records" in stdin
    completed = subprocess.run(
        [sys.executable, "-", *words[2:]], input=stdin, text=True, capture_output=True
    )
    sys.stdout.write(completed.stdout)
    sys.stderr.write(completed.stderr)
    raise SystemExit(completed.returncode)
elif words[0] == "sha256sum":
    content = state["remote_files"].get(words[1])
    if content is None:
        raise SystemExit(1)
    print(hashlib.sha256(content.encode()).hexdigest(), words[1])
elif words[:2] == ["bash", "-c"] and 'cat >"$temporary"' in words[2]:
    state["remote_files"][words[4]] = stdin
    act("install-tool", words[4])
elif words[:2] == ["bash", "-c"] and "luks-prepare" in words[2]:
    state["remote_files"][words[4]] = "console helper"
    act("write-helper", *words[4:])
elif words[:2] == ["bash", "-c"] and "ls -l" in words[2]:
    print("none")
elif words[0] == os.environ["APP_HA_REMOTE_RPOOL_MIRROR"]:
    command, rest = words[1], words[2:]
    act("tool", command, *rest)
    serials, skip = [], False
    for value in rest:
        if skip:
            skip = False
        elif value in ("--pair", "--expect-bytes", "--match", "--member", "--survivor"):
            skip = True
        elif not value.startswith("-"):
            serials.append(value)
    options = dict(zip(rest, rest[1:]))
    if command == "check-new":
        for serial in serials:
            disk = next(d for d in layout["unassigned_disks"] if d["serial"] == serial)
            if disk["mounted"] or serial in state.get("in_other_pool", []):
                print(f"ERROR: disk {serial} is in use", file=sys.stderr)
                raise SystemExit(1)
            print(f"{serial}\t{disk['disk']}\t{disk['size']}\tfresh")
    elif command == "luks-check-prepared":
        if not state.get("console_ran"):
            print("ERROR: crypt-rpool-mirror not open", file=sys.stderr)
            raise SystemExit(1)
        pair = rest[rest.index("--pair") + 1]
        for member in (1, 2):
            state["remote_files"][f"/fake/root/luks-header-mirror{pair}-{member}.bin"] = f"header {member}"
        save()
    elif command in ("luks-add", "clear-add"):
        luks = command == "luks-add"
        members = []
        for serial in serials:
            disk = next(d for d in layout["unassigned_disks"] if d["serial"] == serial)
            members.append({
                "path": f"/dev/mapper/new-{serial}" if luks else f"/dev/disk/by-id/nvme-{serial}",
                "state": "ONLINE", "device": disk["disk"], "luks": luks,
                "mapper": f"/dev/mapper/new-{serial}" if luks else None,
                "backing": disk["disk"], "disk": disk["disk"], "serial": serial,
                "model": disk["model"], "disk_size": disk["size"], "member_size": None,
            })
        layout["vdevs"].append({
            "name": f"mirror-{len(layout['vdevs']) + 1}", "type": "mirror", "state": "ONLINE",
            "removing": False, "size": members[0]["disk_size"], "allocated": 0,
            "free": members[0]["disk_size"], "holds_esp": False, "luks": luks,
            "members": members,
        })
        layout["unassigned_disks"] = [
            d for d in layout["unassigned_disks"] if d["serial"] not in serials
        ]
        layout["luks_mappings"] = [
            row for row in layout["luks_mappings"] if row["serial"] not in serials
        ]
        save()
    elif command == "boot-partition":
        pass
    elif command == "boot-esp":
        disk = find_disk(serials[0])
        layout["unassigned_disks"] = [d for d in layout["unassigned_disks"] if d is not disk
                                      and d["serial"] != serials[0]]
        if all(d["serial"] != serials[0] for d in layout["esp_only_disks"]):
            layout["esp_only_disks"].append(disk)
        save()
    elif command in ("luks-check-member", "luks-backup-headers"):
        member = options["--member"]
        name = f"luks-header-{member}.bin" if member in ("A", "B") else f"luks-header-mirror{member}.bin"
        header = f"/fake/root/{name}"
        if command == "luks-backup-headers":
            # Succeeds only when the member's mapping is already open.
            if not state.get("mapping_open"):
                raise SystemExit(1)
            state["remote_files"][header] = f"header {member}"
        elif header in state["remote_files"]:
            pass
        elif state.get("console_after_checks", 0) > 0:
            state["console_after_checks"] -= 1
            save()
            print("ERROR: crypt-rpool mapping is not open", file=sys.stderr)
            raise SystemExit(1)
        else:
            state["remote_files"][header] = f"header {member}"
        save()
    elif command == "replace-member":
        if state.get("replace_fails"):
            print("ERROR: cannot replace", file=sys.stderr)
            raise SystemExit(1)
        survivor, new = options["--survivor"], serials[0]
        disk = find_disk(new)
        for vdev in layout["vdevs"]:
            if any(m["serial"] == survivor for m in vdev["members"]):
                vdev["replacing"] = True
                vdev["members"].append({
                    "path": f"/dev/new-{new}", "state": "ONLINE", "device": disk["disk"],
                    "luks": "--member" in options, "mapper": None, "backing": disk["disk"],
                    "disk": disk["disk"], "serial": new, "model": disk["model"],
                    "disk_size": disk["size"], "member_size": None,
                })
        layout["unassigned_disks"] = [d for d in layout["unassigned_disks"] if d["serial"] != new]
        layout["esp_only_disks"] = [d for d in layout["esp_only_disks"] if d["serial"] != new]
        save()
    elif command == "retire-luks":
        layout["luks_mappings"] = [
            row for row in layout["luks_mappings"] if row["mapper"] not in rest
        ]
        layout["crypttab"] = [row for row in layout["crypttab"] if row["mapper"] not in rest]
        save()
    else:
        raise SystemExit(f"unexpected tool command {words}")
elif words[0] == "cat":
    sys.stdout.write(state["remote_files"][words[1]])
elif words[:2] == ["rm", "-f"]:
    for path in words[3:]:
        state["remote_files"].pop(path, None)
    act("rm", *words[3:])
elif words[0].endswith("/cluster_registry.py"):
    kind = words[-1]
    print(json.dumps(state["registry"][kind]))
elif words[:2] == ["pvesh", "get"]:
    path = words[2]
    if path == "/nodes":
        print(json.dumps([{"node": "mox1", "status": "online"}, {"node": "mox2", "status": "online"}]))
    elif path == "/cluster/resources":
        print(json.dumps(state["cluster_vms"]))
    elif path == "/cluster/replication":
        print(json.dumps(state["replication"]))
    elif path == "/nodes/mox1/qemu":
        print(json.dumps(state.get("host_qemu", [])))
    elif path.endswith("/status") and "/replication/" in path:
        job = path.split("/")[4]
        print(json.dumps(state["replication_status"].setdefault(job, {"last_sync": 100})))
        save()
    else:
        raise SystemExit(f"unexpected pvesh path {path}")
elif words == ["date", "+%s"]:
    print(int(time.time()) - state.get("clock_skew", 0))
elif words[:2] == ["pvesr", "schedule-now"]:
    act("pvesr", words[2], node)
    if not state.get("replication_stuck"):
        state["replication_status"].setdefault(words[2], {})["last_sync"] = int(time.time())
    save()
elif words == ["zpool", "scrub", "-w", "rpool"] or words == ["zpool", "wait", "-t", "scrub", "rpool"]:
    act("zpool", *words[1:])
    layout["pool"]["scan"] = "scrub repaired 0B in 03:12:44 with 0 errors on Thu Sep 25 03:12:44 2026"
    save()
elif words[:3] == ["zpool", "remove", "rpool"]:
    act("zpool", *words[1:])
    if state.get("remove_fails"):
        print("cannot remove mirror-1: out of space", file=sys.stderr)
        raise SystemExit(1)
    fail_after_start = state.get("remove_starts_then_fails")
    layout["pool"]["removal_in_progress"] = True
    layout["pool"]["remove"] = f"Evacuation of {words[3]} in progress since Thu Sep 25 04:00:00 2026"
    for vdev in layout["vdevs"]:
        if vdev["name"] == words[3]:
            vdev["removing"] = True
    save()
    if fail_after_start:
        print("Connection to mox1 closed by remote host.", file=sys.stderr)
        raise SystemExit(255)
else:
    raise SystemExit(f"unexpected host command {words}")
'''


def strip_remove(text: str) -> str:
    return text.replace(
        text[text.index("remove:"):text.index("config:")], ""
    )


SHARED_CRYPTTAB = "".join(
    f"{mapper}\tUUID={mapper}\tapp-ha-rpool\tluks,initramfs,nofail,keyscript=decrypt_keyctl\n"
    for mapper in (
        "crypt-rpool-a",
        "crypt-rpool-b",
        "crypt-rpool-mirror2-1",
        "crypt-rpool-mirror2-2",
        "crypt-rpool-mirror3-1",
        "crypt-rpool-mirror3-2",
    )
)
NO_STAGING_SNAPSHOTS = "\n".join(
    line for line in fixtures.SNAPSHOTS.splitlines() if "stg-base-" not in line
)


def degraded_layout(
    gone: str, pulled_serial: str, new_serial: str, new_size: int = 2000398934016
) -> dict:
    """The disk behind mapping GONE was pulled and NEW_SERIAL was installed."""
    status = strip_remove(fixtures.LUKS_STATUS).replace(
        f"\t    /dev/mapper/{gone}".ljust(40) + "ONLINE",
        "\t    8093158935163316232".ljust(40) + "UNAVAIL",
    )
    assert "UNAVAIL" in status
    devices = json.loads(fixtures.LUKS_LSBLK)
    devices["blockdevices"] = [
        disk for disk in devices["blockdevices"] if disk["serial"] != pulled_serial
    ]
    devices["blockdevices"].append(fixtures.disk("/dev/nvme8n1", new_serial, new_size))
    return healthy_layout(status_text=status, lsblk_text=json.dumps(devices))


def make_clear(layout: dict) -> dict:
    for vdev in layout["vdevs"]:
        vdev["luks"] = False
        for member in vdev["members"]:
            member["luks"] = False
            member["mapper"] = None
    layout["luks_mappings"] = []
    layout["crypttab"] = []
    return layout


def healthy_layout(**overrides) -> dict:
    arguments = {
        "status_text": strip_remove(fixtures.LUKS_STATUS),
        "crypttab_text": SHARED_CRYPTTAB,
        "snapshot_text": NO_STAGING_SNAPSHOTS,
        "collected_at": int(time.time()),
    }
    arguments.update(overrides)
    return fixtures.luks_layout(**arguments)


class DiskWorkflowTest(unittest.TestCase):
    def setUp(self) -> None:
        self.temporary = tempfile.TemporaryDirectory()
        self.root = Path(self.temporary.name)
        self.env_dir = self.root / "env"
        self.bin_dir = self.root / "bin"
        self.artifacts = self.root / "artifacts"
        for path in (self.env_dir, self.bin_dir, self.artifacts):
            path.mkdir()
        known_hosts = self.root / "known_hosts"
        known_hosts.write_text("")
        known_hosts.chmod(0o600)
        (self.env_dir / "cluster.conf").write_text(
            fixtures_cluster_conf(), encoding="utf-8"
        )
        self.conf = self.env_dir / "mox1.conf"
        self.conf.write_text(MOX1_CONF, encoding="utf-8")
        self.conf.chmod(0o600)
        self.host_state = self.root / "host-storage-state.json"
        self.fake_state = self.root / "fake.json"
        self.write_fake(
            {
                "actions": [],
                "remote_files": {},
                "layout": healthy_layout(),
                "registry": {
                    "resources": [
                        {"name": "prod1", "kind": "production", "vmid": 100,
                         "state": "active", "placement": ["mox1", "mox2"]},
                        {"name": "prod2", "kind": "production", "vmid": 101,
                         "state": "active", "placement": ["mox1", "mox2"]},
                        {"name": "prod3", "kind": "production", "vmid": 102,
                         "state": "active", "placement": ["mox2", "mox3"]},
                    ],
                    "cleanup": [],
                },
                "cluster_vms": [
                    {"vmid": 100, "name": "prod1", "node": "mox1", "type": "qemu"},
                    {"vmid": 101, "name": "prod2", "node": "mox2", "type": "qemu"},
                    {"vmid": 102, "name": "prod3", "node": "mox2", "type": "qemu"},
                ],
                "replication": [
                    {"id": "100-0", "guest": 100, "target": "mox2"},
                    {"id": "101-0", "guest": 101, "target": "mox1"},
                    {"id": "102-0", "guest": 102, "target": "mox3"},
                ],
                "replication_status": {},
            }
        )
        fake = self.bin_dir / "ssh"
        fake.write_text(FAKE_SSH, encoding="utf-8")
        fake.chmod(0o755)
        self.environment = os.environ.copy()
        self.environment.update(
            {
                "APP_HA_CONFIG_TEST_MODE": "1",
                "APP_HA_ENV_DIR": str(self.env_dir),
                "APP_HA_DISK_TEST_MODE": "1",
                "APP_HA_REMOTE_RPOOL_MIRROR": REMOTE_TOOL,
                "APP_HA_REMOTE_ROOT": "/fake/root",
                "APP_HA_ARTIFACTS_DIR": str(self.artifacts),
                "APP_HA_STORAGE_STATE_FILE": str(self.host_state),
                "APP_HA_REPLICATION_POLL_SECONDS": "0",
                "PROXMOX_SSH_KNOWN_HOSTS_FILE": str(known_hosts),
                "FAKE_STATE": str(self.fake_state),
                "PATH": f"{self.bin_dir}:{os.environ['PATH']}",
            }
        )

    def tearDown(self) -> None:
        self.temporary.cleanup()

    def write_fake(self, value: dict) -> None:
        self.fake_state.write_text(json.dumps(value, sort_keys=True), encoding="utf-8")

    def fake(self) -> dict:
        return json.loads(self.fake_state.read_text(encoding="utf-8"))

    def update_fake(self, **changes) -> None:
        value = self.fake()
        value.update(changes)
        self.write_fake(value)

    def host_storage_state(self) -> dict:
        return json.loads(self.host_state.read_text(encoding="utf-8"))

    def seed_host_state(self, **fields) -> None:
        state = {"schema_version": 1, "host": "mox1", "trims": {}, "scrubs": [], "removals": []}
        state.update(fields)
        self.host_state.write_text(json.dumps(state), encoding="utf-8")

    def run_script(
        self, script: Path, stdin: str, expected: int = 0
    ) -> subprocess.CompletedProcess[str]:
        completed = subprocess.run(
            [str(script), "--host", "mox1"],
            input=stdin,
            env=self.environment,
            text=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            check=False,
            timeout=120,
        )
        self.assertEqual(
            completed.returncode,
            expected,
            msg=f"stdout:\n{completed.stdout}\nstderr:\n{completed.stderr}",
        )
        return completed

    def actions(self, kind: str) -> list[list[str]]:
        return [row[1:] for row in self.fake()["actions"] if row[1] == kind]

    def retirements(self) -> list[list[str]]:
        return [row for row in self.actions("tool") if row[1] == "retire-luks"]

    # -- add_new_disk_vdev.sh ------------------------------------------------

    def test_add_luks_mirror_via_console_passphrase(self) -> None:
        self.update_fake(console_ran=True)
        completed = self.run_script(ADD, "1\n2\nGO\nGO\ny\n")
        output = completed.stdout
        helper = self.actions("write-helper")
        self.assertEqual(
            helper, [["write-helper", "/fake/root/app-ha-add-mirror-4", REMOTE_TOOL,
                      "4", "S6BLANK", "S7USED"]],
        )
        self.assertIn("MANUAL LUKS ACTION REQUIRED", output)
        self.assertIn("it never passes through this workstation or SSH", output)
        tool = [row[1] for row in self.actions("tool")]
        self.assertEqual(tool, ["check-new", "luks-check-prepared", "luks-add"])
        self.assertIn(["tool", "luks-add", "--pair", "4", "S6BLANK", "S7USED"],
                      self.actions("tool"))
        for member in (1, 2):
            header = self.artifacts / "mox1" / "luks-headers" / f"rpool-mirror4-{member}.bin"
            self.assertEqual(header.read_text(), f"header {member}")
            self.assertEqual(header.stat().st_mode & 0o777, 0o600)
        self.assertNotIn("/fake/root/app-ha-add-mirror-4", self.fake()["remote_files"])
        conf = self.conf.read_text()
        self.assertIn("NVME_MIRROR_4_SERIAL_1=S6BLANK\n", conf)
        self.assertIn("NVME_MIRROR_4_CAPACITY_BYTES_2=4000787030016\n", conf)
        self.assertIn("NVME_MIRROR_4_SERIAL_2=S7USED", output)
        self.assertIn("gracefully migrate every production guest off mox1", output)
        self.assertFalse(any("reboot" in " ".join(row) for row in self.fake()["actions"]))
        self.assertEqual(self.host_storage_state()["additions"], {})

    def test_add_records_a_pair_an_earlier_run_added(self) -> None:
        # zpool add succeeded, then the session ended before moxN.conf changed.
        layout = self.fake()["layout"]
        members = []
        for disk in layout["unassigned_disks"][:2]:
            members.append({
                "path": f"/dev/mapper/crypt-rpool-mirror4-{len(members) + 1}",
                "state": "ONLINE", "device": disk["disk"], "luks": True,
                "mapper": None, "backing": disk["disk"], "disk": disk["disk"],
                "serial": disk["serial"], "model": disk["model"],
                "disk_size": disk["size"], "member_size": None,
            })
        layout["vdevs"].append(dict(layout["vdevs"][2], name="mirror-3", members=members))
        layout["unassigned_disks"] = layout["unassigned_disks"][2:]
        self.update_fake(layout=layout)
        self.seed_host_state(additions={"4": {
            "serials": ["S6BLANK", "S7USED"],
            "capacities": [4000787030016, 4000787030016], "recorded_at": 1,
        }})
        completed = self.run_script(ADD, "")
        self.assertIn("Recording NVME_MIRROR_4, which an earlier run added to rpool", completed.stdout)
        self.assertIn("NVME_MIRROR_4_SERIAL_2=S7USED\n", self.conf.read_text())
        self.assertEqual(self.host_storage_state()["additions"], {})
        self.assertEqual(self.actions("tool"), [])

    def test_add_stops_until_the_console_helper_succeeds(self) -> None:
        completed = self.run_script(ADD, "1\n2\nGO\nGO\nq\n", expected=1)
        self.assertIn("rerun /fake/root/app-ha-add-mirror-4 at the console", completed.stdout)
        self.assertNotIn("luks-add", [row[1] for row in self.actions("tool")])
        self.assertNotIn("NVME_MIRROR_4_SERIAL_1=S6BLANK", self.conf.read_text())

    def test_add_refuses_disks_of_different_capacity(self) -> None:
        layout = self.fake()["layout"]
        layout["unassigned_disks"][1]["size"] = 4000787030016 + 512
        self.update_fake(layout=layout)
        completed = self.run_script(ADD, "1\n2\nq\n", expected=1)
        self.assertIn("differ in capacity", completed.stdout)
        self.assertEqual(self.actions("tool"), [])

    def test_add_clear_mirror_on_unencrypted_host(self) -> None:
        layout = self.fake()["layout"]
        for vdev in layout["vdevs"]:
            vdev["luks"] = False
            for member in vdev["members"]:
                member["luks"] = False
                member["mapper"] = None
        layout["luks_mappings"] = []
        layout["crypttab"] = []
        self.update_fake(layout=layout)
        completed = self.run_script(ADD, "1\n2\nGO\n")
        self.assertEqual(
            [row[1] for row in self.actions("tool")], ["check-new", "clear-add"]
        )
        self.assertEqual(self.actions("write-helper"), [])
        self.assertIn("NVME_MIRROR_4_SERIAL_1=S6BLANK\n", self.conf.read_text())
        self.assertNotIn("reboot", completed.stdout)

    def test_add_resumes_a_prepared_pair(self) -> None:
        layout = self.fake()["layout"]
        layout["luks_mappings"] += [
            {"mapper": "crypt-rpool-mirror4-1", "backing": "/dev/nvme6n1",
             "disk": "/dev/nvme6n1", "serial": "S6BLANK", "size": 4000787030016,
             "in_pool": False},
            {"mapper": "crypt-rpool-mirror4-2", "backing": "/dev/nvme7n1",
             "disk": "/dev/nvme7n1", "serial": "S7USED", "size": 4000787030016,
             "in_pool": False},
        ]
        self.update_fake(layout=layout, console_ran=True)
        completed = self.run_script(ADD, "y\nGO\ny\n")
        self.assertIn("was prepared by an earlier run", completed.stdout)
        self.assertEqual(
            [row[1] for row in self.actions("tool")], ["luks-check-prepared", "luks-add"]
        )
        self.assertIn("NVME_MIRROR_4_SERIAL_2=S7USED\n", self.conf.read_text())

    def test_add_ignores_mappings_of_a_removed_vdev_awaiting_retirement(self) -> None:
        # mirror-1 finished its removal; its mappings stay open until the list
        # script retires them, so they must not look like a prepared addition.
        layout = self.completed_removal_layout()
        for row in layout["luks_mappings"]:
            if row["mapper"].startswith("crypt-rpool-mirror2"):
                self.assertFalse(row["in_pool"])
        self.update_fake(layout=layout, console_ran=True)
        self.record_mirror_1_removal()
        completed = self.run_script(ADD, "1\n2\nGO\nGO\ny\n")
        self.assertNotIn("was prepared by an earlier run", completed.stdout)
        self.assertIn(["tool", "luks-add", "--pair", "4", "S6BLANK", "S7USED"],
                      self.actions("tool"))

    def test_add_refuses_a_sixth_mirror(self) -> None:
        layout = self.fake()["layout"]
        layout["vdevs"] += [dict(layout["vdevs"][2], name="mirror-3"),
                            dict(layout["vdevs"][2], name="mirror-4")]
        self.update_fake(layout=layout)
        completed = self.run_script(ADD, "", expected=1)
        self.assertIn("already has 5 mirror vdevs", completed.stderr)

    # -- add_replacement_disk.sh ---------------------------------------------

    def test_replace_pulled_luks_boot_member(self) -> None:
        self.update_fake(
            layout=degraded_layout("crypt-rpool-b", "S1BOOTB", "S8NEWBOOT"),
            console_after_checks=1,
        )
        completed = self.run_script(REPLACE, "1\nGO\ny\nGO\ny\n")
        output = completed.stdout
        self.assertIn(
            "mirror-0 (boot mirror): member 8093158935163316232 is gone; "
            "surviving disk S0BOOTA has 2000398934016 bytes",
            output,
        )
        self.assertEqual(
            [row[1] for row in self.actions("tool")],
            ["check-new", "boot-partition", "boot-esp", "luks-check-member",
             "luks-backup-headers", "luks-check-member", "replace-member"],
        )
        self.assertIn(
            ["tool", "check-new", "--expect-bytes", "2000398934016", "--match", "exact",
             "S8NEWBOOT"],
            self.actions("tool"),
        )
        self.assertIn(
            ["tool", "boot-esp", "--survivor", "S0BOOTA", "S8NEWBOOT"], self.actions("tool")
        )
        self.assertIn(
            ["tool", "replace-member", "--survivor", "S0BOOTA", "--member", "B", "S8NEWBOOT"],
            self.actions("tool"),
        )
        self.assertEqual(
            self.actions("write-helper"),
            [["write-helper", "/fake/root/app-ha-replace-member-B", REMOTE_TOOL, "B",
              "S8NEWBOOT", ""]],
        )
        self.assertIn("it never passes through this workstation or SSH", output)
        header = self.artifacts / "mox1" / "luks-headers" / "rpool-B.bin"
        self.assertEqual(header.read_text(), "header B")
        self.assertEqual(header.stat().st_mode & 0o777, 0o600)
        self.assertNotIn("/fake/root/luks-header-B.bin", self.fake()["remote_files"])
        conf = self.conf.read_text()
        self.assertIn("NVME_MIRROR_1_SERIAL_2=S8NEWBOOT\n", conf)
        self.assertIn(": NVME_MIRROR_1_SERIAL_2=S1BOOTB\n", conf)
        self.assertIn("NVME_MIRROR_1_CAPACITY_BYTES_2=2000398934016\n", conf)
        self.assertIn("NVME_MIRROR_1_SERIAL_1=S0BOOTA\n", conf)
        self.assertIn("REPLACING A MEMBER (resilver)", output)
        self.assertIn("this script does not wait for it", output)
        self.assertIn("either disk can boot mox1 alone", output)
        self.assertIn("enter the\nshared passphrase once", output)
        self.assertFalse(any("reboot" in " ".join(row) for row in self.fake()["actions"]))
        self.assertEqual(self.host_storage_state()["replacements"], {})

        # The mirror now shows the replacement resilvering; a rerun leaves it.
        rerun = self.run_script(REPLACE, "")
        self.assertIn("mirror-0 is already resilvering a replacement member", rerun.stdout)
        self.assertIn("nothing to replace", rerun.stdout)

    def test_replace_resumes_a_prepared_boot_disk(self) -> None:
        layout = degraded_layout("crypt-rpool-b", "S1BOOTB", "S8NEWBOOT")
        disk = next(d for d in layout["unassigned_disks"] if d["serial"] == "S8NEWBOOT")
        layout["unassigned_disks"].remove(disk)
        layout["esp_only_disks"].append(disk)
        layout["luks_mappings"].append(
            {"mapper": "crypt-rpool-b", "backing": "/dev/nvme8n1p3", "disk": "/dev/nvme8n1",
             "serial": "S8NEWBOOT", "size": 2000398934016, "in_pool": False}
        )
        self.update_fake(layout=layout)
        completed = self.run_script(REPLACE, "y\ny\ny\n")
        self.assertIn("was prepared for mirror-0 by an earlier run", completed.stdout)
        self.assertIn("is already open on disk S8NEWBOOT", completed.stdout)
        self.assertEqual(
            [row[1] for row in self.actions("tool")],
            ["boot-partition", "boot-esp", "luks-check-member", "replace-member"],
        )
        self.assertEqual(self.actions("write-helper"), [])
        self.assertIn("NVME_MIRROR_1_SERIAL_2=S8NEWBOOT\n", self.conf.read_text())

    def test_replace_stops_until_the_console_helper_succeeds(self) -> None:
        self.update_fake(
            layout=degraded_layout("crypt-rpool-mirror2-2", "S3EXTRA2", "S9NEWEXTRA"),
            console_after_checks=5,
        )
        completed = self.run_script(REPLACE, "1\nGO\nGO\nq\n", expected=1)
        self.assertIn("rerun /fake/root/app-ha-replace-member-2-2 at the console", completed.stdout)
        self.assertEqual(
            self.actions("write-helper"),
            [["write-helper", "/fake/root/app-ha-replace-member-2-2", REMOTE_TOOL, "2-2",
              "S9NEWEXTRA", "--expect-bytes 2000398934016 --match exact"]],
        )
        self.assertNotIn("replace-member", [row[1] for row in self.actions("tool")])
        self.assertNotIn("boot-partition", [row[1] for row in self.actions("tool")])
        record = self.host_storage_state()["replacements"]["S2EXTRA1"]
        self.assertEqual((record["serial"], record["vdev"]), ("S9NEWEXTRA", "mirror-1"))
        self.assertIn("NVME_MIRROR_2_SERIAL_2=S3EXTRA2\n", self.conf.read_text())

    def test_replace_resumes_after_reusing_the_pulled_members_mapping(self) -> None:
        # The console helper closed the pulled disk's leftover crypt-rpool-b,
        # opened the new disk under that name, and the run stopped before
        # zpool replace: rpool shows crypt-rpool-b OFFLINE on the new disk.
        status = strip_remove(fixtures.LUKS_STATUS).replace(
            "\t    /dev/mapper/crypt-rpool-b          ONLINE",
            "\t    /dev/mapper/crypt-rpool-b          OFFLINE",
        )
        devices = json.loads(fixtures.LUKS_LSBLK)
        devices["blockdevices"][1] = fixtures.boot_disk(
            1, "S8NEWBOOT", "CCCC-3333", "crypt-rpool-b"
        )
        self.update_fake(layout=healthy_layout(
            status_text=status, lsblk_text=json.dumps(devices),
            boot_uuids_text="AAAA-1111\nCCCC-3333\n",
        ))
        # Without the host's record it is indistinguishable from a failed disk.
        refused = self.run_script(REPLACE, "")
        self.assertIn("(disk S8NEWBOOT) is OFFLINE but still installed", refused.stdout)
        self.assertEqual(self.actions("tool"), [])

        self.seed_host_state(replacements={
            "S0BOOTA": {"serial": "S8NEWBOOT", "vdev": "mirror-0", "recorded_at": 1},
        })
        completed = self.run_script(REPLACE, "y\ny\ny\n")
        self.assertIn("Disk S8NEWBOOT was prepared for mirror-0 by an earlier run", completed.stdout)
        self.assertEqual(
            [row[1] for row in self.actions("tool")],
            ["boot-partition", "boot-esp", "luks-check-member", "replace-member"],
        )
        self.assertIn("NVME_MIRROR_1_SERIAL_2=S8NEWBOOT\n", self.conf.read_text())
        self.assertEqual(self.host_storage_state()["replacements"], {})

    def test_replace_resumes_a_recorded_disk_after_a_reboot(self) -> None:
        # The console helper encrypted S9NEWEXTRA, then the host rebooted
        # before replace-member: its mapping is closed and it holds LUKS.
        layout = degraded_layout("crypt-rpool-mirror2-2", "S3EXTRA2", "S9NEWEXTRA")
        disk = next(d for d in layout["unassigned_disks"] if d["serial"] == "S9NEWEXTRA")
        disk["fstype"] = "crypto_LUKS"
        self.update_fake(layout=layout, console_after_checks=1)
        self.seed_host_state(replacements={
            "S2EXTRA1": {"serial": "S9NEWEXTRA", "vdev": "mirror-1", "recorded_at": 1},
        })
        completed = self.run_script(REPLACE, "y\nGO\ny\n")
        self.assertIn("Disk S9NEWEXTRA was prepared for mirror-1 by an earlier run", completed.stdout)
        self.assertEqual(
            [row[1] for row in self.actions("tool")],
            ["luks-check-member", "luks-backup-headers", "luks-check-member",
             "replace-member"],
        )
        self.assertEqual(len(self.actions("write-helper")), 1)
        self.assertIn("NVME_MIRROR_2_SERIAL_2=S9NEWEXTRA\n", self.conf.read_text())

    def test_replace_refreshes_a_missing_header_backup_instead_of_the_console(self) -> None:
        # An earlier run opened the new member, then stopped before its header
        # backup; preparing it again would refuse the open mapping.
        self.update_fake(
            layout=degraded_layout("crypt-rpool-mirror2-2", "S3EXTRA2", "S9NEWEXTRA"),
            mapping_open=True, console_after_checks=5,
        )
        self.seed_host_state(replacements={
            "S2EXTRA1": {"serial": "S9NEWEXTRA", "vdev": "mirror-1", "recorded_at": 1},
        })
        completed = self.run_script(REPLACE, "y\ny\n")
        self.assertIn("its header backup was refreshed", completed.stdout)
        self.assertEqual(
            [row[1] for row in self.actions("tool")],
            ["luks-check-member", "luks-backup-headers", "luks-check-member",
             "replace-member"],
        )
        self.assertEqual(self.actions("write-helper"), [])

    def test_replace_records_a_disk_that_already_joined(self) -> None:
        # The run stopped after zpool replace, before env/moxN.conf changed.
        layout = degraded_layout("crypt-rpool-b", "S1BOOTB", "S8NEWBOOT")
        disk = next(d for d in layout["unassigned_disks"] if d["serial"] == "S8NEWBOOT")
        layout["unassigned_disks"].remove(disk)
        layout["vdevs"][0]["replacing"] = True
        layout["vdevs"][0]["members"].append({
            "path": "/dev/mapper/crypt-rpool-b", "state": "ONLINE", "device": "/dev/dm-8",
            "luks": True, "mapper": "/dev/mapper/crypt-rpool-b", "backing": "/dev/nvme8n1p3",
            "disk": "/dev/nvme8n1", "serial": "S8NEWBOOT", "model": "Dell Ent NVMe",
            "disk_size": 2000398934016, "member_size": None,
        })
        self.update_fake(layout=layout)
        self.seed_host_state(replacements={
            "S0BOOTA": {"serial": "S8NEWBOOT", "vdev": "mirror-0", "recorded_at": 1},
        })
        completed = self.run_script(REPLACE, "")
        self.assertIn("Disk S8NEWBOOT already joined mirror-0", completed.stdout)
        self.assertIn("nothing to replace", completed.stdout)
        self.assertEqual(self.actions("tool"), [])
        self.assertIn("NVME_MIRROR_1_SERIAL_2=S8NEWBOOT\n", self.conf.read_text())
        self.assertEqual(self.host_storage_state()["replacements"], {})

    def test_replace_clear_extra_member(self) -> None:
        self.update_fake(
            layout=make_clear(degraded_layout("crypt-rpool-mirror2-2", "S3EXTRA2", "S9NEWEXTRA"))
        )
        completed = self.run_script(REPLACE, "1\nGO\n")
        self.assertEqual(
            self.actions("tool"),
            [["tool", "check-new", "--expect-bytes", "2000398934016", "--match", "exact",
              "S9NEWEXTRA"],
             ["tool", "replace-member", "--survivor", "S2EXTRA1", "S9NEWEXTRA"]],
        )
        self.assertEqual(self.actions("write-helper"), [])
        conf = self.conf.read_text()
        self.assertIn("NVME_MIRROR_2_SERIAL_2=S9NEWEXTRA\n", conf)
        self.assertIn("NVME_MIRROR_2_SERIAL_1=S2EXTRA1\n", conf)
        self.assertNotIn("either disk can boot", completed.stdout)
        self.assertNotIn("passphrase", completed.stdout)

    def test_replace_offers_only_identical_capacity(self) -> None:
        self.update_fake(
            layout=degraded_layout("crypt-rpool-b", "S1BOOTB", "S8NEWBOOT", 2000398934016 + 4096)
        )
        completed = self.run_script(REPLACE, "", expected=1)
        self.assertIn("no unused, unmounted disk on mox1 has exactly 2000398934016 bytes",
                      completed.stderr)
        self.assertEqual(self.actions("tool"), [])

    def test_replace_leaves_failed_disks_that_are_still_installed(self) -> None:
        status = strip_remove(fixtures.LUKS_STATUS).replace(
            "\t    /dev/mapper/crypt-rpool-mirror2-2  ONLINE",
            "\t    /dev/mapper/crypt-rpool-mirror2-2  FAULTED",
        )
        self.update_fake(layout=healthy_layout(status_text=status))
        completed = self.run_script(REPLACE, "")
        self.assertIn(
            "mirror-1 member /dev/mapper/crypt-rpool-mirror2-2 (disk S3EXTRA2) is FAULTED "
            "but still installed",
            completed.stdout,
        )
        self.assertIn("nothing to replace", completed.stdout)
        self.assertEqual(self.actions("tool"), [])

    def test_replace_waits_for_a_running_removal(self) -> None:
        self.update_fake(layout=healthy_layout(status_text=fixtures.LUKS_STATUS))
        completed = self.run_script(REPLACE, "", expected=1)
        self.assertIn("a vdev removal is in progress on mox1", completed.stderr)

    # -- decommission_disks.sh ------------------------------------------------

    def test_decommission_full_run(self) -> None:
        completed = self.run_script(DECOMMISSION, "Y\ny\ny\ny\n100\n1\nGO\n")
        output = completed.stdout
        self.assertIn("prod1    VMID 100    on mox1   runs on mox1", output)
        self.assertIn("prod2    VMID 101    on mox2   replicates to mox1", output)
        self.assertNotIn("prod3", output)
        self.assertEqual(
            sorted(row[1] for row in self.actions("fstrim")), ["prod1", "prod2"]
        )
        self.assertEqual(
            sorted(row[1:] for row in self.actions("pvesr")),
            [["100-0", "mox1"], ["101-0", "mox2"]],
        )
        self.assertIn(["zpool", "scrub", "-w", "rpool"], self.actions("zpool"))
        self.assertIn("4,600,000,000,000 bytes = 4,386,901.86 MiB = 4,284.08 GiB", output)
        self.assertIn("(never: boot mirror holding the ESPs)", output)
        self.assertIn(["zpool", "remove", "rpool", "mirror-1"], self.actions("zpool"))
        state = self.host_storage_state()
        self.assertEqual(sorted(state["trims"]), ["prod1", "prod2"])
        self.assertIn("with 0 errors", state["scrubs"][-1]["result"])
        removal = state["removals"][-1]
        self.assertEqual(removal["vdev"], "mirror-1")
        self.assertEqual(removal["state"], "requested")
        self.assertEqual(removal["conf_pair"], 2)
        self.assertEqual(
            [m["serial"] for m in removal["members"]], ["S2EXTRA1", "S3EXTRA2"]
        )
        self.assertIn("Evacuation of mirror-1 in progress", output)
        self.assertIn("list_disks_ready_for_physically_removal.sh --host mox1", output)

    def test_decommission_skips_trims_and_scrub_done_within_a_day(self) -> None:
        recent = int(time.time()) - 3600
        self.seed_host_state(
            trims={"prod1": {"completed_at": recent}, "prod2": {"completed_at": recent}},
            scrubs=[{"completed_at": recent, "result": "scrub repaired 0B in 01:00:00 with 0 errors on x"}],
        )
        completed = self.run_script(DECOMMISSION, "Y\ny\n100\nq\n")
        self.assertIn("prod1 was trimmed 60 minutes ago; treating that as sufficient", completed.stdout)
        self.assertIn("A clean scrub completed 60 minutes ago; treating it as sufficient", completed.stdout)
        self.assertEqual(self.actions("fstrim"), [])
        self.assertEqual(self.actions("zpool"), [])
        self.assertEqual(len(self.actions("pvesr")), 2)
        self.assertIn("No vdev was removed.", completed.stdout)

    def test_decommission_old_trim_is_repeated(self) -> None:
        stale = int(time.time()) - 90000
        self.seed_host_state(trims={"prod1": {"completed_at": stale}})
        self.run_script(DECOMMISSION, "Y\ny\ny\ny\n100\nq\n")
        self.assertEqual(
            sorted(row[1] for row in self.actions("fstrim")), ["prod1", "prod2"]
        )

    def test_decommission_waits_for_a_replication_run_started_after_the_trims(self) -> None:
        # A scheduled run that started before the trims finishes (last_sync is
        # its start time), but no run starts after them.
        recent = int(time.time()) - 60
        self.seed_host_state(scrubs=[{"completed_at": recent, "result": "scrub repaired 0B with 0 errors"}])
        self.environment["APP_HA_REPLICATION_TIMEOUT_SECONDS"] = "1"
        self.update_fake(
            replication_stuck=True,
            replication_status={"100-0": {"last_sync": int(time.time()) - 30}},
        )
        completed = self.run_script(DECOMMISSION, "Y\ny\ny\ny\n", expected=1)
        self.assertIn("timed out waiting for replication job 100-0", completed.stderr)
        self.assertNotIn("replicated", completed.stdout)

    def test_decommission_requires_related_staging_to_be_destroyed(self) -> None:
        fake = self.fake()
        fake["registry"]["resources"].append(
            {"name": "stage1prod2", "kind": "staging", "vmid": 200, "state": "ready",
             "source": "prod2", "owner_node": "mox2", "placement": ["mox2"]}
        )
        fake["registry"]["resources"].append(
            {"name": "stage1prod3", "kind": "staging", "vmid": 201, "state": "ready",
             "source": "prod3", "owner_node": "mox3", "placement": ["mox3"]}
        )
        self.write_fake(fake)
        completed = self.run_script(DECOMMISSION, "", expected=1)
        self.assertIn(
            "staging VM stage1prod2 (VMID 200, ready): cloned from prod2, whose disk is on mox1",
            completed.stdout,
        )
        self.assertNotIn("stage1prod3", completed.stdout)
        self.assertIn("guests/staging/destroy_staging_vm.sh stageNprodN", completed.stdout)
        self.assertEqual(self.actions("fstrim"), [])

    def test_decommission_flags_leftover_staging_snapshots(self) -> None:
        self.update_fake(layout=healthy_layout(snapshot_text=fixtures.SNAPSHOTS))
        completed = self.run_script(DECOMMISSION, "", expected=1)
        self.assertIn(
            "staging base snapshot rpool/data/vm-100-disk-0@stg-base-stage1prod1-260914T220000Z",
            completed.stdout,
        )

    def test_decommission_stops_when_files_are_not_deleted(self) -> None:
        completed = self.run_script(DECOMMISSION, "n\n")
        self.assertIn("Delete the unwanted files inside each guest first", completed.stdout)
        self.assertEqual(self.actions("fstrim"), [])

    def test_decommission_requires_ssh_to_every_guest(self) -> None:
        self.update_fake(guest_ssh_fail=["prod2"])
        completed = self.run_script(DECOMMISSION, "Y\n", expected=1)
        self.assertIn("[PASS] ssh prod1", completed.stdout)
        self.assertIn("[FAIL] ssh prod2", completed.stdout)
        self.assertEqual(self.actions("fstrim"), [])

    def test_decommission_validates_minimum_free_space(self) -> None:
        recent = int(time.time()) - 60
        self.seed_host_state(
            trims={"prod1": {"completed_at": recent}, "prod2": {"completed_at": recent}},
            scrubs=[{"completed_at": recent, "result": "scrub repaired 0B with 0 errors"}],
        )
        completed = self.run_script(DECOMMISSION, "Y\ny\n10\nlots\n5000\n")
        self.assertEqual(
            completed.stdout.count("Enter a number of GiB that is at least 50."), 2
        )
        self.assertIn("exceeds what rpool can offer", completed.stdout)
        self.assertEqual(self.actions("zpool"), [])

    def test_decommission_offers_only_vdevs_that_fit(self) -> None:
        recent = int(time.time()) - 60
        self.seed_host_state(
            trims={"prod1": {"completed_at": recent}, "prod2": {"completed_at": recent}},
            scrubs=[{"completed_at": recent, "result": "scrub repaired 0B with 0 errors"}],
        )
        # 4600000000000 - 2500 GiB leaves less than either extra mirror's size.
        completed = self.run_script(DECOMMISSION, "Y\ny\n2500\n")
        self.assertIn("no: its capacity exceeds removable_space", completed.stdout)
        self.assertIn("No vdev fits within removable_space", completed.stdout)

    def test_decommission_refuses_while_a_removal_runs(self) -> None:
        self.update_fake(layout=healthy_layout(status_text=fixtures.LUKS_STATUS))
        completed = self.run_script(DECOMMISSION, "", expected=1)
        self.assertIn("A vdev removal is already in progress", completed.stdout)

    def test_decommission_marks_a_failed_zpool_remove(self) -> None:
        recent = int(time.time()) - 60
        self.seed_host_state(
            trims={"prod1": {"completed_at": recent}, "prod2": {"completed_at": recent}},
            scrubs=[{"completed_at": recent, "result": "scrub repaired 0B with 0 errors"}],
        )
        self.update_fake(remove_fails=True)
        completed = self.run_script(DECOMMISSION, "Y\ny\n100\n1\nGO\n", expected=1)
        self.assertIn("out of space", completed.stderr)
        self.assertEqual(self.host_storage_state()["removals"][-1]["state"], "failed")

    def test_decommission_keeps_a_removal_the_host_started_despite_an_error(self) -> None:
        # zpool remove ran on the host, but the SSH session failed afterwards.
        recent = int(time.time()) - 60
        self.seed_host_state(
            trims={"prod1": {"completed_at": recent}, "prod2": {"completed_at": recent}},
            scrubs=[{"completed_at": recent, "result": "scrub repaired 0B with 0 errors"}],
        )
        self.update_fake(remove_starts_then_fails=True)
        completed = self.run_script(DECOMMISSION, "Y\ny\n100\n1\nGO\n", expected=1)
        self.assertIn("may be removing it anyway", completed.stderr)
        self.assertIn("list_disks_ready_for_physically_removal.sh --host mox1", completed.stdout)
        self.assertEqual(self.host_storage_state()["removals"][-1]["state"], "requested")

    # -- list_disks_ready_for_physically_removal.sh --------------------------

    def record_mirror_1_removal(self) -> None:
        layout = healthy_layout()
        members = [
            {key: member[key] for key in
             ("path", "mapper", "luks", "disk", "serial", "model", "disk_size")}
            for member in layout["vdevs"][1]["members"]
        ]
        self.seed_host_state(
            removals=[{
                "id": "removal-1-mirror-1", "vdev": "mirror-1", "members": members,
                "conf_pair": 2, "state": "requested", "requested_at": 1,
                "updated_at": 1, "notes": [],
            }]
        )

    def completed_removal_layout(self) -> dict:
        status = strip_remove(fixtures.LUKS_STATUS).replace(
            "\t  mirror-1                             ONLINE       0     0     0\n"
            "\t    /dev/mapper/crypt-rpool-mirror2-1  ONLINE       0     0     0\n"
            "\t    /dev/mapper/crypt-rpool-mirror2-2  ONLINE       0     0     0\n",
            "",
        )
        return healthy_layout(status_text=status)

    def test_list_reports_evacuation_in_progress(self) -> None:
        self.record_mirror_1_removal()
        self.update_fake(layout=healthy_layout(status_text=fixtures.LUKS_STATUS))
        completed = self.run_script(LIST, "")
        self.assertIn("ZFS is still copying data off mirror-1", completed.stdout)
        self.assertIn("53.75% done", completed.stdout)
        self.assertEqual(self.actions("tool"), [])
        self.assertEqual(self.host_storage_state()["removals"][0]["state"], "requested")

    def test_list_retires_completed_luks_removal(self) -> None:
        self.record_mirror_1_removal()
        self.update_fake(layout=self.completed_removal_layout())
        completed = self.run_script(LIST, "y\n")
        self.assertEqual(
            self.retirements(),
            [["tool", "retire-luks", "crypt-rpool-mirror2-1", "crypt-rpool-mirror2-2"]],
        )
        conf = self.conf.read_text()
        self.assertIn(": NVME_MIRROR_2_SERIAL_1=S2EXTRA1\n", conf)
        self.assertNotIn("\nNVME_MIRROR_2_SERIAL_1=", conf)
        self.assertEqual(self.host_storage_state()["removals"][0]["state"], "retired")
        output = completed.stdout
        self.assertIn("S2EXTRA1", output.split("safe to pull ====", 1)[1])
        self.assertIn("S3EXTRA2", output.split("safe to pull ====", 1)[1])
        self.assertNotIn("wipe", output)

        # A later run just lists them; once pulled they are reported as such.
        rerun = self.run_script(LIST, "")
        self.assertEqual(len(self.retirements()), 1)
        self.assertIn("S2EXTRA1", rerun.stdout)
        layout = self.fake()["layout"]
        layout["unassigned_disks"] = [
            d for d in layout["unassigned_disks"] if d["serial"] not in ("S2EXTRA1", "S3EXTRA2")
        ]
        self.update_fake(layout=layout)
        pulled = self.run_script(LIST, "")
        self.assertIn("Already pulled", pulled.stdout)

    def test_list_always_lets_the_tool_finish_a_luks_retirement(self) -> None:
        # An interrupted retirement already closed the mappings and edited
        # crypttab; only the tool can tell whether the initramfs was rebuilt.
        self.record_mirror_1_removal()
        layout = self.completed_removal_layout()
        layout["luks_mappings"] = [
            row for row in layout["luks_mappings"] if not row["mapper"].startswith("crypt-rpool-mirror2")
        ]
        layout["crypttab"] = [
            row for row in layout["crypttab"] if not row["mapper"].startswith("crypt-rpool-mirror2")
        ]
        self.update_fake(layout=layout)
        self.run_script(LIST, "y\n")
        self.assertEqual(
            self.retirements(),
            [["tool", "retire-luks", "crypt-rpool-mirror2-1", "crypt-rpool-mirror2-2"]],
        )
        self.assertEqual(self.host_storage_state()["removals"][0]["state"], "retired")

    def test_list_does_not_offer_a_retired_disk_that_is_mounted_again(self) -> None:
        self.record_mirror_1_removal()
        layout = self.completed_removal_layout()
        for disk in layout["unassigned_disks"]:
            if disk["serial"] == "S3EXTRA2":
                disk["mounted"] = True
        self.update_fake(layout=layout)
        output = self.run_script(LIST, "y\n").stdout
        ready = output.split("safe to pull ====", 1)[1].split("NOT safe to pull", 1)[0]
        self.assertIn("S2EXTRA1", ready)
        self.assertNotIn("S3EXTRA2", ready)
        self.assertIn("S3EXTRA2 (/dev/nvme3n1, from mirror-1)", output.split("NOT safe to pull", 1)[1])

    def test_list_does_not_offer_a_retired_disk_in_another_pool(self) -> None:
        self.record_mirror_1_removal()
        self.update_fake(layout=self.completed_removal_layout(), in_other_pool=["S2EXTRA1"])
        output = self.run_script(LIST, "y\n").stdout
        ready = output.split("safe to pull ====", 1)[1].split("NOT safe to pull", 1)[0]
        self.assertIn("S3EXTRA2", ready)
        self.assertNotIn("S2EXTRA1", ready)
        self.assertIn("S2EXTRA1 (/dev/nvme2n1, from mirror-1)", output.split("NOT safe to pull", 1)[1])

    def test_list_mentions_wiping_only_for_unencrypted_disks(self) -> None:
        self.record_mirror_1_removal()
        state = self.host_storage_state()
        for member in state["removals"][0]["members"]:
            member["luks"] = False
            member["mapper"] = None
        self.host_state.write_text(json.dumps(state))
        layout = self.completed_removal_layout()
        layout["luks_mappings"] = []
        layout["crypttab"] = []
        self.update_fake(layout=layout)
        completed = self.run_script(LIST, "")
        self.assertEqual(self.retirements(), [])
        self.assertIn("were not encrypted and still hold old rpool data", completed.stdout)

    def test_list_marks_a_canceled_removal_failed(self) -> None:
        self.record_mirror_1_removal()
        completed = self.run_script(LIST, "")
        self.assertIn("removal was canceled or failed", completed.stdout)
        self.assertEqual(self.host_storage_state()["removals"][0]["state"], "failed")

    def test_list_without_records(self) -> None:
        completed = self.run_script(LIST, "")
        self.assertIn("No vdev removals are recorded on mox1", completed.stdout)

    def test_shell_syntax(self) -> None:
        for script in (ADD, REPLACE, DECOMMISSION, LIST, LIB_DIR / "disk_workflows.sh"):
            completed = subprocess.run(
                ["bash", "-n", str(script)], text=True, capture_output=True, check=False
            )
            self.assertEqual(completed.returncode, 0, completed.stderr)


MOX1_CONF = """\
NVME_MIRROR_1_SERIAL_1=S0BOOTA
NVME_MIRROR_1_SERIAL_2=S1BOOTB
NVME_MIRROR_1_CAPACITY_BYTES_1=2000398934016
NVME_MIRROR_1_CAPACITY_BYTES_2=2000398934016
NVME_MIRROR_2_SERIAL_1=S2EXTRA1
NVME_MIRROR_2_SERIAL_2=S3EXTRA2
NVME_MIRROR_2_CAPACITY_BYTES_1=2000398934016
NVME_MIRROR_2_CAPACITY_BYTES_2=2000398934016
NVME_MIRROR_3_SERIAL_1=S4EXTRA3
NVME_MIRROR_3_SERIAL_2=S5EXTRA4
NVME_MIRROR_3_CAPACITY_BYTES_1=2000398934016
NVME_MIRROR_3_CAPACITY_BYTES_2=2000398934016
#NVME_MIRROR_4_SERIAL_1=
#NVME_MIRROR_4_SERIAL_2=
PROXMOX_IP=10.213.0.11
PROXMOX_GATEWAY=10.213.0.1
PROXMOX_PREFIX=24
PROXMOX_PUBLIC_MAC=02:00:00:00:00:01
PROXMOX_SECONDARY_MAC=02:00:00:00:01:01
MAX_PROD_VM_COUNT_ON_THIS_HOST=1
MAX_STAGING_VM_COUNT_ON_THIS_HOST=5
"""


def fixtures_cluster_conf() -> str:
    return "\n".join(
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
            "STAGING_VM_TAG=app-ha-staging",
            "CLUSTER_STATE_DIR=/etc/pve/priv/app-ha",
            "",
        )
    )


if __name__ == "__main__":
    unittest.main()
