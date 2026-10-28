# Copyright (c) 2026 BEENTHERE VENTURES, INC.
# SPDX-License-Identifier: GPL-3.0-only

"""Tests for scripts/user_callable/backups/instant_restore_existing.sh against
the stateful fake cluster of test_rollback_restore.py, extended with the
registry's instant_restore marker, backup_link.sh, instant_switch.py, and the
overlay storage."""

from __future__ import annotations

import json
import os
from pathlib import Path
import shlex
import subprocess
import sys
import tempfile
import textwrap
import unittest

sys.path.insert(0, str(Path(__file__).resolve().parent))
import test_rollback_restore as rollback  # noqa: E402


SCRIPT = rollback.REPO_ROOT / "scripts" / "user_callable" / "backups" / "instant_restore_existing.sh"
SNAPSHOT = rollback.SNAPSHOT
LIB = "/usr/local/lib/app-ha-proxmox/lib/"
OVERLAY = "bmac-overlay:100/vm-100-disk-1.qcow2"
OVERLAY_PATH = "/var/lib/bmac-overlay/images/100/vm-100-disk-1.qcow2"

# Handled before the rollback fake's own commands.
EXTRA = r'''
def size_of(drive):
    return [part for part in drive.split(",") if part.startswith("size=")][0]

record = state["record"]
if argv[:1] == [LIB + "cluster_registry.py"] and argv[3] == "update":
    words = argv[4:]
    if "--instant-restore-snapshot" in words and "instant_restore" not in record:
        record["instant_restore"] = {"storage": words[words.index("--instant-restore-storage") + 1],
                                     "snapshot": words[words.index("--instant-restore-snapshot") + 1],
                                     "phase": "planned"}
    if "--instant-restore-phase" in words:
        record["instant_restore"]["phase"] = words[words.index("--instant-restore-phase") + 1]
    if "--clear-instant-restore" in words:
        record.pop("instant_restore", None)
    if "--disk-bytes" in words:
        record["spec"]["disk_bytes"] = int(words[words.index("--disk-bytes") + 1])
    record["revision"] += 1
    state["updates"] = state.get("updates", []) + [words]
    commit()
    out(record)
if argv[:2] == ["pvesh", "get"] and argv[2] == "/nodes/mox1/vzdump/extractconfig":
    out(json.dumps("\n".join(f"{k}: {v}" for k, v in sorted(state["backup_config"].items()))))
if argv[:2] == ["pvesh", "get"] and argv[2] == "/storage/bmac-overlay":
    out({"type": "dir", "path": "/var/lib/bmac-overlay", "content": "images"})
if argv[:1] == [LIB + "backup_link.sh"]:
    action = argv[1]
    if action == "status":
        out(state["link"])
    if action == "map":
        state["link"] = "alive"
    if action == "unmap":
        state["link"] = "absent"
    if action == "create-overlay":
        state["overlay"] = True
    commit()
    sys.exit(0)
if argv[:2] == ["python3", LIB + "instant_switch.py"]:
    action = argv[2]
    scsi0 = state["config"]["scsi0"]
    if action == "attach":
        state["config"]["scsi0"] = scsi0.replace("local-zfs:vm-100-disk-1", OVERLAY).replace(
            size_of(scsi0), size_of(state["backup_config"]["scsi0"]))
    elif action == "state":
        if state["status"] != "running":
            out("stopped")
        out("switched" if scsi0.startswith("local-zfs:") else state.get("disk_state", "backup"))
    elif action == "switch":
        state["config"]["scsi0"] = scsi0.replace(OVERLAY, "local-zfs:vm-100-disk-1")
    commit()
    sys.exit(0)
if argv[:2] == ["pvesm", "path"]:
    out(OVERLAY_PATH if argv[2] == OVERLAY else "/dev/zvol/rpool/data/" + argv[2].split(":", 1)[1])
if argv[:2] == ["pvesm", "free"]:
    state["overlay"] = False
    commit()
    sys.exit(0)
if argv[:1] == ["test"]:
    sys.exit(0 if argv[1] == "-x" or state.get("overlay") else 1)
if argv[:1] == ["blkdiscard"]:
    sys.exit(0)
'''

FAKE = rollback.FAKE.replace(
    "if argv[:1] == [\"bash\"]:",
    f"LIB = {LIB!r}\nOVERLAY = {OVERLAY!r}\nOVERLAY_PATH = {OVERLAY_PATH!r}\n" + EXTRA + "\nif argv[:1] == [\"bash\"]:",
    1,
)

MUTATIONS = ("ha-manager set", "pvesr delete", "pvesr create-local-job", "pvesh create", "qm set", "qm resize",
             "blkdiscard", "bash -c", "pvesm free", LIB + "backup_link.sh", "python3 " + LIB + "instant_switch.py attach",
             "python3 " + LIB + "instant_switch.py switch")


class InstantRestoreExistingTest(unittest.TestCase):
    def setUp(self) -> None:
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        root = Path(self.temporary.name)
        self.config = root / "config"
        self.config.mkdir()
        (self.config / "cluster.conf").write_text(rollback.CLUSTER_CONF, encoding="utf-8")
        (self.config / "cluster.conf").chmod(0o644)
        self.fake = root / "fake"
        self.fake.mkdir()
        self.program = self.fake / "fake.py"
        self.program.write_text(FAKE, encoding="utf-8")
        self.program.chmod(0o755)
        backup_config = dict(rollback.LIVE_CONFIG, cores="32", memory="65536")
        backup_config["scsi0"] = backup_config["scsi0"].replace("size=1T", "size=512G")
        self.state = {
            "record": json.loads(json.dumps(rollback.RECORD)), "status": "running", "ha": "started",
            "jobs": {"100-0": "mox2"}, "synced": {"100-0": 50}, "config": dict(rollback.LIVE_CONFIG),
            "backup_config": backup_config, "link": "absent",
            "content": [{"volid": f"bmac-pbs:backup/{SNAPSHOT}", "vmid": 100, "ctime": 1793112601,
                         "notes": rollback.NOTES, "content": "backup"}],
        }
        self.state["record"]["spec"]["replication_interval"] = "15"
        self.save()

    def save(self) -> None:
        (self.fake / "state.json").write_text(json.dumps(self.state))

    def load(self) -> dict:
        return json.loads((self.fake / "state.json").read_text())

    def run_script(self, *arguments: str, expected: int = 0, answers: str = "") -> subprocess.CompletedProcess[str]:
        program = shlex.quote(str(self.program))
        body = f"""
        INSTANT_RESTORE_EXISTING_SOURCE_ONLY=1 source {shlex.quote(str(SCRIPT))}
        first_reachable_mox() {{ printf 'mox1\\n'; }}
        mox_ssh() {{ {program} mox "$@"; }}
        node_exec() {{ {program} node "$@" </dev/null; }}
        sleep() {{ :; }}
        main {" ".join(shlex.quote(a) for a in arguments)}
        """
        environment = os.environ.copy()
        environment.update({"APP_HA_CONFIG_TEST_MODE": "1", "APP_HA_CONFIG_DIR": str(self.config),
                            "FAKE": str(self.fake), "TZ": "America/Denver"})
        completed = subprocess.run(["bash", "-c", textwrap.dedent(body)], env=environment, input=answers,
                                   capture_output=True, text=True, check=False)
        self.assertEqual(completed.returncode, expected,
                         msg=f"stdout:\n{completed.stdout}\nstderr:\n{completed.stderr}")
        return completed

    def mutations(self) -> list[str]:
        calls = [json.loads(line) for line in (self.fake / "calls.jsonl").read_text().splitlines()]
        found = []
        for call in calls:
            text = " ".join(call["argv"])
            if text.startswith("bash -c for command"):
                continue
            if text.startswith(MUTATIONS):
                found.append(text.replace(LIB, ""))
            elif call["argv"][3:4] == ["update"]:
                found.append("registry " + " ".join(call["argv"][7:]))
        return found

    def test_a_new_restore_boots_from_the_overlay_then_switches_and_replicates(self) -> None:
        completed = self.run_script("prod1", "--snapshot", SNAPSHOT, "--no-safety-backup",
                                    answers="\n\n\nRESTORE prod1\n")
        self.assertEqual(self.mutations(), [
            "registry --cores 64 --memory-mb 75776 --replication-interval 15 "
            f"--instant-restore-storage bmac-pbs --instant-restore-snapshot {SNAPSHOT}",
            "ha-manager set vm:100 --state stopped",
            "pvesr delete 100-0",
            # From here the zvol's data is gone: the hook boots only the overlay.
            "registry --instant-restore-phase freed",
            f"backup_link.sh map 100 {SNAPSHOT} --storage bmac-pbs",
            "backup_link.sh restore 100 drive-efidisk0.img.fidx /dev/zvol/rpool/data/vm-100-disk-0",
            "blkdiscard -f /dev/zvol/rpool/data/vm-100-disk-1",
            "bash -c zfs set \"volsize=$2\" \"$1\" && udevadm settle bash rpool/data/vm-100-disk-1 549755813888",
            f"backup_link.sh create-overlay 100 {OVERLAY}",
            f"python3 instant_switch.py attach 100 {OVERLAY} local-zfs:vm-100-disk-1",
            "qm set 100 --cores 64 --memory 75776",
            "ha-manager set vm:100 --state started",
            "blkdiscard -f /dev/zvol/rpool/data/vm-100-disk-1",
            "backup_link.sh restore 100 drive-scsi0.img.fidx /dev/zvol/rpool/data/vm-100-disk-1 --skip-zero",
            f"python3 instant_switch.py switch 100 {OVERLAY} local-zfs:vm-100-disk-1",
            "registry --instant-restore-phase switched",
            "backup_link.sh unmap 100",
            "backup_link.sh forget 100",
            "pvesm free " + OVERLAY,
            # The restore point's 512 GiB disk grows back to the registered 1 TiB.
            "qm resize 100 scsi0 1099511627776",
            "pvesr create-local-job 100-0 mox2 --schedule */15 --comment Production prod1 replica",
            "registry --owner-node mox1 --replication-targets mox2 --clear-instant-restore",
        ])
        state = self.load()
        self.assertNotIn("instant_restore", state["record"])
        self.assertEqual((state["status"], state["ha"], state["jobs"]), ("running", "started", {"100-0": "mox2"}))
        self.assertTrue(state["config"]["scsi0"].startswith("local-zfs:vm-100-disk-1,"))
        self.assertEqual(state["lease"], ["orchestration-acquire", "orchestration-release"])
        self.assertIn("prod1 is restored to 2026-10-27 10:04 MDT", completed.stdout)

    def test_a_copy_delay_keeps_the_vm_on_pbs_first_and_the_comment_stays_out_of_the_time(self) -> None:
        self.state["content"][0]["notes"] = rollback.NOTES + " | test comment"
        self.save()
        completed = self.run_script("prod1", "--snapshot", SNAPSHOT, "--no-safety-backup", "--copy-delay-seconds", "90",
                                    answers="\n\n\nRESTORE prod1\n")
        self.assertIn("Waiting 90s before the copy (--copy-delay-seconds): prod1 runs from its restore point on PBS",
                      completed.stdout)
        self.assertIn("30s until the copy starts", completed.stdout)
        steps = self.mutations()
        started = steps.index("ha-manager set vm:100 --state started")
        # The mapping is checked after the wait, before the zvol is touched.
        self.assertEqual(steps[started + 1:started + 3],
                         ["backup_link.sh status 100", "blkdiscard -f /dev/zvol/rpool/data/vm-100-disk-1"])
        self.assertIn("prod1 is restored to 2026-10-27 10:04 MDT\n", completed.stdout)
        self.assertIn("AFTER 2026-10-27 10:04 MDT IS LOST", completed.stdout)
        self.run_script("prod1", "--copy-delay-seconds", "3601", expected=1)

    def test_a_restore_stopped_after_the_free_resumes_from_its_overlay(self) -> None:
        # The run died during the copy, and the mapping died with its host.
        self.state["record"]["instant_restore"] = {"storage": "bmac-pbs", "snapshot": SNAPSHOT, "phase": "freed"}
        self.state["config"]["scsi0"] = self.state["config"]["scsi0"].replace("local-zfs:vm-100-disk-1", OVERLAY)
        self.state["jobs"] = {}
        self.state["link"] = "dead"
        self.state["overlay"] = True
        self.save()
        self.run_script("prod1", answers="y\n")
        steps = self.mutations()
        self.assertEqual(steps[:5], [
            "backup_link.sh status 100",
            "ha-manager set vm:100 --state stopped",
            f"backup_link.sh map 100 {SNAPSHOT} --storage bmac-pbs",
            "ha-manager set vm:100 --state started",
            "blkdiscard -f /dev/zvol/rpool/data/vm-100-disk-1",
        ])
        self.assertFalse(any("attach" in step or "efidisk0" in step for step in steps), steps)
        self.assertNotIn("instant_restore", self.load()["record"])

    def test_a_zvol_the_overlay_already_reads_is_only_committed(self) -> None:
        self.state["record"]["instant_restore"] = {"storage": "bmac-pbs", "snapshot": SNAPSHOT, "phase": "freed"}
        self.state["config"]["scsi0"] = self.state["config"]["scsi0"].replace("local-zfs:vm-100-disk-1", OVERLAY)
        self.state["jobs"] = {}
        self.state["link"] = "alive"
        self.state["disk_state"] = "target"
        self.save()
        self.run_script("prod1", answers="y\n")
        steps = self.mutations()
        self.assertFalse(any(step.startswith(("blkdiscard", "backup_link.sh restore", "ha-manager")) for step in steps), steps)
        self.assertIn(f"python3 instant_switch.py switch 100 {OVERLAY} local-zfs:vm-100-disk-1", steps)

    def test_after_the_switch_only_the_cleanup_and_replication_remain(self) -> None:
        self.state["record"]["instant_restore"] = {"storage": "bmac-pbs", "snapshot": SNAPSHOT, "phase": "switched"}
        self.state["jobs"] = {}
        self.save()
        self.run_script("prod1", answers="y\n")
        self.assertEqual([step.split(" --")[0] for step in self.mutations()], [
            "backup_link.sh unmap 100", "backup_link.sh forget 100", "pvesr create-local-job 100-0 mox2",
            "registry"])

    def test_a_resume_keeps_its_restore_point(self) -> None:
        self.state["record"]["instant_restore"] = {"storage": "bmac-pbs", "snapshot": SNAPSHOT, "phase": "freed"}
        self.save()
        completed = self.run_script("prod1", "--snapshot", "vm/100/2026-10-26T10:00:00Z", expected=1)
        self.assertIn(f"under way is from {SNAPSHOT}", completed.stderr)
        self.assertEqual(self.mutations(), [])

    def test_a_freed_vm_running_from_its_zvol_is_never_used(self) -> None:
        self.state["record"]["instant_restore"] = {"storage": "bmac-pbs", "snapshot": SNAPSHOT, "phase": "freed"}
        self.state["jobs"] = {}
        self.save()
        completed = self.run_script("prod1", answers="y\n", expected=1)
        self.assertIn("runs from its freed zvol", completed.stderr)
        self.assertIn("zvol no longer holds its previous data", completed.stderr)

    def test_only_prod_linked_staging_vms_are_refused(self) -> None:
        self.state["staging"] = [{"kind": "staging", "name": "stage1prod1", "source": "prod1"}]
        self.save()
        completed = self.run_script("prod1", "--snapshot", SNAPSHOT, "--no-safety-backup", expected=1)
        self.assertIn("has prod-linked staging VMs (stage1prod1)", completed.stderr)
        self.assertEqual(self.mutations(), [])

    def test_a_wrong_confirmation_changes_nothing(self) -> None:
        completed = self.run_script("prod1", "--snapshot", SNAPSHOT, "--no-safety-backup",
                                    answers="\n\n\nrestore prod1\n", expected=1)
        self.assertIn("Cancelled; nothing was changed", completed.stderr)
        self.assertEqual(self.mutations(), [])


if __name__ == "__main__":
    unittest.main()
