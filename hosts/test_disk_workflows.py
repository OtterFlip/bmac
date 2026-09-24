#!/usr/bin/env python3

# Copyright (c) 2026 BEENTHERE VENTURES, INC.
# SPDX-License-Identifier: GPL-3.0-only

"""Fake-SSH tests for the disk workflows: add_new_disk_vdev.sh,
decommission_disks.sh, and list_disks_ready_for_physically_removal.sh.

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
    serials = [value for value in rest if not value.startswith("-") and not value.isdigit()]
    if command == "check-new":
        for serial in serials:
            disk = next(d for d in layout["unassigned_disks"] if d["serial"] == serial)
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
elif words[:2] == ["pvesr", "schedule-now"]:
    act("pvesr", words[2], node)
    state["replication_status"].setdefault(words[2], {"last_sync": 100})["last_sync"] += 60
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
    layout["pool"]["removal_in_progress"] = True
    layout["pool"]["remove"] = f"Evacuation of {words[3]} in progress since Thu Sep 25 04:00:00 2026"
    for vdev in layout["vdevs"]:
        if vdev["name"] == words[3]:
            vdev["removing"] = True
    save()
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

    def test_add_refuses_a_sixth_mirror(self) -> None:
        layout = self.fake()["layout"]
        layout["vdevs"] += [dict(layout["vdevs"][2], name="mirror-3"),
                            dict(layout["vdevs"][2], name="mirror-4")]
        self.update_fake(layout=layout)
        completed = self.run_script(ADD, "", expected=1)
        self.assertIn("already has 5 mirror vdevs", completed.stderr)

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
            self.actions("tool"),
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
        self.assertEqual(len(self.actions("tool")), 1)
        self.assertIn("S2EXTRA1", rerun.stdout)
        layout = self.fake()["layout"]
        layout["unassigned_disks"] = [
            d for d in layout["unassigned_disks"] if d["serial"] not in ("S2EXTRA1", "S3EXTRA2")
        ]
        self.update_fake(layout=layout)
        pulled = self.run_script(LIST, "")
        self.assertIn("Already pulled", pulled.stdout)

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
        self.assertEqual(self.actions("tool"), [])
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
        for script in (ADD, DECOMMISSION, LIST, LIB_DIR / "disk_workflows.sh"):
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
