# Copyright (c) 2026 BEENTHERE VENTURES, INC.
# SPDX-License-Identifier: GPL-3.0-only

"""Tests for scripts/lib/backups.sh and the backup schedule, Back up now, and
restore-point protection workflows, run against a fake cluster host."""

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
from ui_test_driver import run_json, scripted  # noqa: E402


REPO_ROOT = Path(__file__).resolve().parents[2]
BACKUPS_LIB = REPO_ROOT / "scripts" / "lib" / "backups.sh"
WORKFLOWS = REPO_ROOT / "scripts" / "user_callable" / "backups"
KEY = "24:a7:6e:9e:65:9f:c8:d6"
NOTES = "bmac prod1 slug=pinbinapp fqdn=pinbinapp.com cluster=MyAppCloud guest=prod1"
UPID = "UPID:mox2:003FD8EC:02CB1464:6AE0BA19:vzdump:100:root@pam:"

CLUSTER_CONF = """PROXMOX_CLUSTER_NAME=MyAppCloud
MAX_MOX_HOSTS=10
PROXMOX_INTERNAL_DOMAIN=pve.internal
PRIVATE_SUBNET_CIDR=10.213.0.0/24
PROXMOX_MIGRATION_NETWORK=10.213.0.240/28
DATACENTER_PRIVATE_VLAN_GATEWAY=10.213.0.1
GUEST_GATEWAY_IP=10.213.0.10
GUEST_GATEWAY_MAC=02:00:0a:d5:00:0a
MOX_IP_START=10.213.0.11
MOX_IP_END=10.213.0.20
MOX_REPLICATION_IP_START=10.213.0.241
MOX_REPLICATION_IP_END=10.213.0.250
VM_SLOT_START=10.213.0.110
MAX_PROD_VMS=10
MAX_STAGING_VMS_PER_PROD=9
CLOUDFLARE_ACCOUNT_ID=0123456789abcdef0123456789abcdef
CLOUDFLARE_TUNNEL_ID=00000000-0000-0000-0000-000000000000
PRODUCTION_VM_TAG=app-ha-production
STAGING_VM_TAG=app-ha-staging
EVICTABLE_VM_TAG=evictable
PROD_VM_STORAGE=local-zfs
CLUSTER_STATE_DIR=/etc/pve/priv/app-ha
"""

RESOURCES = [
    {"kind": "production", "index": 1, "name": "prod1", "vmid": 100, "state": "active",
     "domains": {"primary": "pinbinapp.com", "aliases": []}, "purpose": {"slug": "pinbinapp"}},
    {"kind": "production", "index": 3, "name": "prod3", "vmid": 105, "state": "active",
     "domains": {"primary": "shop.example.com", "aliases": []}, "purpose": {"slug": "shop"}},
    {"kind": "staging", "index": 1, "name": "stage1prod1", "vmid": 201},
]

# Answers `mox_ssh HOST ARGV...` from the JSON state files in FAKE and records
# every call. It always drains stdin, as an ssh session would.
FAKE_HOST = r'''#!/usr/bin/env python3
import json, os, sys
from pathlib import Path

fake = Path(os.environ["FAKE"])
host, argv = sys.argv[1], sys.argv[2:]
sys.stdin.read()
with open(fake / "calls.jsonl", "a") as log:
    log.write(json.dumps({"host": host, "argv": argv}) + "\n")

def load(name, default):
    path = fake / name
    return json.loads(path.read_text()) if path.exists() else default

def save(name, value):
    (fake / name).write_text(json.dumps(value))

def options(words):
    result, key = {}, None
    for word in words:
        if word.startswith("--"):
            key = word[2:]
            result.setdefault(key, [])
        elif key:
            result[key].append(word)
    return {k: (v[0] if len(v) == 1 else (v or True)) for k, v in result.items()}

if argv[:1] == ["/usr/local/lib/app-ha-proxmox/lib/cluster_registry.py"]:
    resources = load("resources.json", [])
    if argv[3] == "list":
        print(json.dumps(resources))
        sys.exit(0)
    if argv[3] == "get":
        for row in resources:
            if row["name"] == argv[4]:
                print(json.dumps(row))
                sys.exit(0)
        sys.exit(1)
if argv[:3] == ["python3", "-I", "/usr/local/lib/app-ha-proxmox/lib/pbs_api.py"]:
    print(json.dumps({"prune": {"readable": True, "jobs": [
        {"id": "prune-btv", "keep-last": 6, "keep-daily": 3, "keep-weekly": 4, "ns": "BTV", "store": "pbs"}]}}))
    sys.exit(0)
verb, path, rest = (argv[1], argv[2], argv[3:]) if argv[:1] == ["pvesh"] else (None, None, None)
if verb == "get" and path == "/storage/bmac-pbs":
    sys.exit(0 if (fake / "storage").exists() else 1)
if verb == "get" and path.endswith("/storage/bmac-pbs/status"):
    print(json.dumps({"active": 0 if (fake / "inactive").exists() else 1, "type": "pbs"}))
    sys.exit(0)
if path == "/cluster/backup" and verb == "get":
    print(json.dumps(load("jobs.json", [])))
    sys.exit(0)
if path == "/cluster/backup" and verb == "create":
    jobs, settings = load("jobs.json", []), options(rest)
    settings.pop("output-format", None)
    job = {k: v for k, v in settings.items()}
    job["enabled"] = int(job.get("enabled", 1))
    job["next-run"] = 1793130000
    jobs.append(job)
    save("jobs.json", jobs)
    sys.exit(0)
if path and path.startswith("/cluster/backup/"):
    job_id, jobs = path.rsplit("/", 1)[1], load("jobs.json", [])
    job = next((j for j in jobs if j["id"] == job_id), None)
    if job is None:
        sys.exit(1)
    if verb == "delete":
        save("jobs.json", [j for j in jobs if j["id"] != job_id])
        sys.exit(0)
    if verb == "set":
        settings = options(rest)
        if (fake / "ignore-set").exists():
            sys.exit(0)
        for key in str(settings.pop("delete", "")).split(","):
            job.pop(key, None)
        for key, value in settings.items():
            job[key] = int(value) if key == "enabled" else value
        save("jobs.json", jobs)
        sys.exit(0)
if verb == "get" and path == "/cluster/resources":
    print(json.dumps(load("vms.json", [])))
    sys.exit(0)
if verb == "create" and path.endswith("/vzdump"):
    save("vzdump.json", {"host": host, "path": path, "options": options(rest)})
    print(json.dumps(os.environ.get("FAKE_UPID", "UPID:" + path.split("/")[2] + ":003FD8EC:02CB1464:6AE0BA19:vzdump:100:root@pam:")))
    sys.exit(0)
if verb == "get" and "/tasks/" in path and path.endswith("/status"):
    polls = load("polls.json", 0) + 1
    save("polls.json", polls)
    running = polls < 2
    print(json.dumps({"status": "running" if running else "stopped",
                      "exitstatus": None if running else load("exitstatus.json", "OK")}))
    sys.exit(0)
if verb == "get" and "/tasks/" in path and path.endswith("/log"):
    lines = ["INFO: starting new backup job: vzdump 100", "INFO:  50% (512.0 GiB of 1.0 TiB) in 3m, read: 2.7 GiB/s, write: 0 B/s",
             "INFO: Backup job finished successfully"]
    start = int(options(rest).get("start", 0))
    print(json.dumps([{"n": i + 1, "t": t} for i, t in enumerate(lines) if i + 1 > start]))
    sys.exit(0)
if "/storage/bmac-pbs/content" in (path or ""):
    content = load("content.json", [])
    if verb == "get":
        vmid = options(rest).get("vmid")
        if vmid:
            content = [row for row in content if str(row.get("vmid")) == vmid]
        if (fake / "add-on-list").exists():
            content = content + [{"volid": "bmac-pbs:backup/vm/100/2026-10-28T09:00:00Z", "vmid": 100,
                                  "ctime": 4102444800, "notes": "x", "content": "backup"}]
        print(json.dumps(content))
        sys.exit(0)
    if verb == "set":
        volid = path.split("/content/", 1)[1]
        for row in content:
            if row["volid"] == volid:
                row["protected"] = int(options(rest)["protected"])
        save("content.json", content)
        sys.exit(0)
print(f"fake host: unexpected command {argv}", file=sys.stderr)
sys.exit(97)
'''


class BackupsLibraryTest(unittest.TestCase):
    def run_lib(self, body: str) -> str:
        return subprocess.run(
            ["bash", "-c", f"source {shlex.quote(str(BACKUPS_LIB))}\n{textwrap.dedent(body)}"],
            capture_output=True, text=True, check=True,
        ).stdout

    def test_schedules_are_staggered_by_slot(self) -> None:
        out = self.run_lib("""
            backup_schedule 4 1; backup_schedule 4 2; backup_schedule 1 9
            backup_schedule 24 1; backup_schedule 12 10
        """)
        self.assertEqual(out.split(), ["0/4:05", "0/4:12", "0/1:01", "01:05", "0/12:08"])

    def test_every_and_notes(self) -> None:
        out = self.run_lib("""
            for h in 1 2 3 4 5 6 8 12 24 48; do backup_valid_every "$h" && printf '%s ' "$h"; done; echo
            backup_notes_template prod1 pinbinapp pinbinapp.com
            backup_job_id prod1
        """)
        self.assertEqual(out.splitlines(), [
            "1 2 3 4 6 8 12 24 ",
            "bmac prod1 slug=pinbinapp fqdn=pinbinapp.com cluster={{cluster}} guest={{guestname}}",
            "bmac-prod1",
        ])


class WorkflowTestCase(unittest.TestCase):
    script = ""
    source_only = ""

    def setUp(self) -> None:
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        root = Path(self.temporary.name)
        self.config = root / "config"
        self.config.mkdir()
        (self.config / "cluster.conf").write_text(CLUSTER_CONF, encoding="utf-8")
        (self.config / "cluster.conf").chmod(0o644)
        self.fake = root / "fake"
        self.fake.mkdir()
        self.host = self.fake / "host.py"
        self.host.write_text(FAKE_HOST, encoding="utf-8")
        self.host.chmod(0o755)
        (self.fake / "storage").write_text("")
        self.save("resources.json", RESOURCES)

    def save(self, name: str, value: object) -> None:
        (self.fake / name).write_text(json.dumps(value))

    def load(self, name: str) -> object:
        return json.loads((self.fake / name).read_text())

    def run_workflow(self, *arguments: str, expected: int = 0) -> subprocess.CompletedProcess[str]:
        body = f"""
        {self.source_only}=1 source {shlex.quote(str(WORKFLOWS / self.script))}
        first_reachable_mox() {{ printf 'mox1\\n'; }}
        mox_ssh() {{ {shlex.quote(str(self.host))} "$@"; }}
        sleep() {{ :; }}
        main {" ".join(shlex.quote(a) for a in arguments)}
        """
        environment = os.environ.copy()
        environment.update({"APP_HA_CONFIG_TEST_MODE": "1", "APP_HA_CONFIG_DIR": str(self.config),
                            "FAKE": str(self.fake), "TZ": "America/Denver"})
        completed = subprocess.run(["bash", "-c", textwrap.dedent(body)], env=environment,
                                   capture_output=True, text=True, stdin=subprocess.DEVNULL, check=False)
        self.assertEqual(completed.returncode, expected,
                         msg=f"stdout:\n{completed.stdout}\nstderr:\n{completed.stderr}")
        return completed

    def run_dashboard(self, *arguments: str, replies=()):
        """Run as the dashboard does: --json, answering each request in turn."""
        harness = self.fake / "harness.sh"
        harness.write_text(textwrap.dedent(f"""\
            #!/usr/bin/env bash
            {self.source_only}=1 source {shlex.quote(str(WORKFLOWS / self.script))}
            first_reachable_mox() {{ printf 'mox1\\n'; }}
            mox_ssh() {{ {shlex.quote(str(self.host))} "$@" </dev/null; }}
            sleep() {{ :; }}
            bmac_ui_bootstrap "$@"
            main "$@"
            """))
        harness.chmod(0o755)
        environment = {k: v for k, v in os.environ.items() if not k.startswith("BMAC_UI")}
        environment.update({"APP_HA_CONFIG_TEST_MODE": "1", "APP_HA_CONFIG_DIR": str(self.config),
                            "FAKE": str(self.fake), "TZ": "America/Denver"})
        run = run_json([str(harness), *arguments], scripted(*replies), env=environment, timeout=60)
        self.assertEqual(run.returncode, 0, run.describe())
        return run

    def calls(self) -> list[dict[str, object]]:
        path = self.fake / "calls.jsonl"
        return [json.loads(line) for line in path.read_text().splitlines()] if path.exists() else []

    def changing_calls(self) -> list[list[str]]:
        return [call["argv"] for call in self.calls()
                if call["argv"][:1] == ["pvesh"] and call["argv"][1] in ("create", "set", "delete")]


class ManageBackupScheduleTest(WorkflowTestCase):
    script = "manage_backup_schedule.sh"
    source_only = "MANAGE_BACKUP_SCHEDULE_SOURCE_ONLY"

    def job(self) -> dict[str, object]:
        [job] = self.load("jobs.json")
        return job

    def test_creates_the_job_with_bmac_settings(self) -> None:
        completed = self.run_workflow("prod1", "--every", "4", "--yes")
        self.assertEqual(self.job(), {
            "id": "bmac-prod1", "schedule": "0/4:05", "storage": "bmac-pbs", "vmid": "100",
            "mode": "snapshot", "enabled": 1, "repeat-missed": "1", "prune-backups": "keep-all=1",
            "fleecing": "enabled=1,storage=local-zfs",
            "notes-template": "bmac prod1 slug=pinbinapp fqdn=pinbinapp.com cluster={{cluster}} guest={{guestname}}",
            "comment": "managed-by=bmac; prod1", "next-run": 1793130000,
        })
        self.assertIn("at every 4h, keep-last 6 covers the last 24 hours", completed.stdout)

    def test_changes_frequency_and_limit_of_an_existing_job(self) -> None:
        self.run_workflow("--vm", "prod3", "--every", "24", "--yes")
        self.run_workflow("--vm", "prod3", "--every", "6", "--bwlimit-mib", "50", "--yes")
        self.assertEqual((self.job()["schedule"], self.job()["bwlimit"], self.job()["vmid"]), ("0/6:19", "51200", "105"))
        self.run_workflow("--vm", "prod3", "--every", "6", "--no-bwlimit", "--yes")
        self.assertNotIn("bwlimit", self.job())
        self.assertEqual([argv[1] for argv in self.changing_calls()], ["create", "set", "set"])

    def test_disable_and_remove_keep_backups(self) -> None:
        self.run_workflow("prod1", "--every", "4", "--yes")
        self.run_workflow("prod1", "--disable", "--yes")
        self.assertEqual(self.job()["enabled"], 0)
        completed = self.run_workflow("prod1", "--remove", "--yes")
        self.assertEqual(self.load("jobs.json"), [])
        self.assertIn("existing backups stay on PBS", completed.stdout)
        self.assertFalse(any("content" in " ".join(argv) for argv in self.changing_calls()))

    def test_the_dashboard_asks_for_the_vm_and_the_frequency(self) -> None:
        seen = []

        def answer(value):
            def reply(request):
                seen.append(request)
                return {"values": {"choice": value}}
            return reply

        def confirm(request):
            seen.append(request)
            return {"confirmed": True}

        self.run_dashboard(replies=(answer("prod3"), answer("8"), confirm))
        vm, frequency, _ = seen
        self.assertEqual([o["value"] for o in vm["field"]["options"]], ["prod1", "prod3"])
        self.assertEqual([o["label"] for o in frequency["field"]["options"]][:4],
                         ["Every 1 hour", "Every 2 hours", "Every 3 hours", "Every 4 hours (recommended)"])
        self.assertEqual(frequency["field"]["default"], "4")
        self.assertEqual(self.job()["schedule"], "0/8:19")

    def test_disable_without_a_job_is_refused(self) -> None:
        completed = self.run_workflow("prod1", "--disable", "--yes", expected=1)
        self.assertIn("prod1 has no backup job to disable", completed.stderr)

    def test_without_pbs_nothing_is_changed(self) -> None:
        (self.fake / "storage").unlink()
        completed = self.run_workflow("prod1", "--every", "4", "--yes", expected=1)
        self.assertIn("Run Setup PBS first", completed.stderr)
        self.assertEqual(self.changing_calls(), [])

    def test_only_the_offered_frequencies_and_production_vms(self) -> None:
        self.assertIn("--every must be one of 1 2 3 4 6 8 12 24", self.run_workflow("prod1", "--every", "5", expected=1).stderr)
        self.assertIn("prod9 is not a registered production VM",
                      self.run_workflow("prod9", "--every", "4", "--yes", expected=1).stderr)

    def test_a_job_that_does_not_take_the_settings_fails(self) -> None:
        self.run_workflow("prod1", "--every", "4", "--yes")
        (self.fake / "ignore-set").write_text("")
        completed = self.run_workflow("prod1", "--every", "8", "--yes", expected=1)
        self.assertIn("does not hold the new settings", completed.stderr)


class BackupNowTest(WorkflowTestCase):
    script = "backup_now.sh"
    source_only = "BACKUP_NOW_SOURCE_ONLY"

    def setUp(self) -> None:
        super().setUp()
        self.save("vms.json", [{"vmid": 100, "node": "mox2", "status": "running", "name": "prod1"}])

    def test_backs_up_on_the_vms_host_and_follows_the_task(self) -> None:
        (self.fake / "add-on-list").write_text("")
        completed = self.run_workflow("prod1", "--yes")
        started = self.load("vzdump.json")
        self.assertEqual(started["host"], "mox2")
        self.assertEqual(started["path"], "/nodes/mox2/vzdump")
        self.assertEqual({k: started["options"][k] for k in ("vmid", "storage", "mode", "fleecing", "notes-template")}, {
            "vmid": "100", "storage": "bmac-pbs", "mode": "snapshot", "fleecing": "enabled=1,storage=local-zfs",
            "notes-template": "bmac prod1 slug=pinbinapp fqdn=pinbinapp.com cluster={{cluster}} guest={{guestname}}"})
        self.assertIn("Backup job finished successfully", completed.stderr)
        self.assertEqual(completed.stderr.count("INFO: starting new backup job"), 1)
        self.assertIn("Restore point: bmac-pbs:backup/vm/100/2026-10-28T09:00:00Z", completed.stdout)

    def test_a_comment_goes_into_the_notes(self) -> None:
        self.run_workflow("prod1", "--yes", "--comment", "Before upgrading Postgres")
        self.assertEqual(self.load("vzdump.json")["options"]["notes-template"],
                         "bmac prod1 slug=pinbinapp fqdn=pinbinapp.com cluster={{cluster}} guest={{guestname}}"
                         " | Before upgrading Postgres")

    def test_a_comment_vzdump_would_misread_is_refused(self) -> None:
        for comment in ("{{cluster}}", "a | b", "back\\slash", "x" * 201):
            with self.subTest(comment=comment):
                completed = self.run_workflow("prod1", "--yes", "--comment", comment, expected=1)
                self.assertIn("at most 200 characters, without { } \\ or |", completed.stderr)
        self.assertFalse((self.fake / "vzdump.json").exists())

    def test_the_dashboard_asks_for_an_optional_comment(self) -> None:
        seen = []

        def comment(text):
            def reply(request):
                seen.append(request)
                return {"values": {"comment": text}}
            return reply

        def confirm(request):
            seen.append(request)
            return {"confirmed": True}

        self.run_dashboard("prod1", replies=(comment("a {bad} one"), comment("Before the upgrade"), confirm))
        first, retry, _ = seen
        self.assertEqual(first["field"]["id"], "comment")
        self.assertFalse(first["field"]["required"])
        self.assertIn("validation_error", retry)
        self.assertTrue(self.load("vzdump.json")["options"]["notes-template"].endswith(" | Before the upgrade"))

    def test_a_failed_task_fails_the_workflow(self) -> None:
        self.save("exitstatus.json", "job errors")
        completed = self.run_workflow("prod1", "--yes", expected=1)
        self.assertIn("The backup of prod1 failed: job errors", completed.stderr)

    def test_a_vm_on_no_host_is_refused(self) -> None:
        self.save("vms.json", [])
        completed = self.run_workflow("prod1", "--yes", expected=1)
        self.assertIn("VMID 100 (prod1) is not on any cluster host", completed.stderr)
        self.assertFalse((self.fake / "vzdump.json").exists())

    def test_an_inactive_storage_starts_nothing(self) -> None:
        (self.fake / "inactive").write_text("")
        completed = self.run_workflow("prod1", "--yes", expected=1)
        self.assertIn("is not active on mox1", completed.stderr)
        self.assertFalse((self.fake / "vzdump.json").exists())


class ProtectRestorePointTest(WorkflowTestCase):
    script = "protect_restore_point.sh"
    source_only = "PROTECT_RESTORE_POINT_SOURCE_ONLY"

    def setUp(self) -> None:
        super().setUp()
        self.save("content.json", [
            {"content": "backup", "ctime": 1793112601, "encrypted": KEY, "notes": NOTES, "vmid": 100,
             "volid": "bmac-pbs:backup/vm/100/2026-10-27T14:50:01Z"},
            {"content": "backup", "ctime": 1793100000, "encrypted": KEY, "notes": "manual", "vmid": 100,
             "volid": "bmac-pbs:backup/vm/100/2026-10-27T11:20:00Z"},
        ])

    def protected(self) -> object:
        return self.load("content.json")[0].get("protected")

    def test_protect_and_clear(self) -> None:
        self.run_workflow("--snapshot", "vm/100/2026-10-27T14:50:01Z", "--yes")
        self.assertEqual(self.protected(), 1)
        completed = self.run_workflow("--snapshot", "vm/100/2026-10-27T14:50:01Z", "--unprotect", "--yes")
        self.assertEqual(self.protected(), 0)
        self.assertIn("Protection cleared", completed.stdout)

    def test_the_picker_shows_a_restore_points_comment(self) -> None:
        content = self.load("content.json")
        content[0]["notes"] = NOTES + " | Before upgrading Postgres to version 18 and migrating every table"
        self.save("content.json", content)
        seen = []

        def pick(request):
            seen.append(request)
            return {"values": {"choice": "vm/100/2026-10-27T14:50:01Z"}}

        self.run_dashboard("--protect", replies=(pick, lambda request: {"confirmed": True}))
        [label] = [o["label"] for o in seen[0]["field"]["options"]]
        # Long comments are cut to 48 characters.
        self.assertEqual(label, "2026-10-27 08:50 MDT · prod1 · Before upgrading Postgres to version 18 and mig…")
        self.assertEqual(len(label.split(" · ", 2)[2]), 48)

    def test_the_dashboard_asks_for_the_restore_point(self) -> None:
        seen = []

        def pick(request):
            seen.append(request)
            return {"values": {"choice": "vm/100/2026-10-27T14:50:01Z"}}

        def confirm(request):
            seen.append(request)
            return {"confirmed": True}

        self.run_dashboard("--protect", replies=(pick, confirm))
        picker = seen[0]
        self.assertEqual([(o["value"], o["label"]) for o in picker["field"]["options"]],
                         [("vm/100/2026-10-27T14:50:01Z", "2026-10-27 08:50 MDT · prod1")])
        self.assertEqual(self.protected(), 1)

    def test_nothing_to_change(self) -> None:
        completed = self.run_workflow("--snapshot", "vm/100/2026-10-27T14:50:01Z", "--unprotect", "--yes")
        self.assertIn("Nothing to change", completed.stdout)
        self.assertEqual(self.changing_calls(), [])

    def test_only_bmac_restore_points(self) -> None:
        completed = self.run_workflow("--snapshot", "vm/100/2026-10-27T11:20:00Z", "--protect", "--yes", expected=1)
        self.assertIn("is not a BMAC restore point", completed.stderr)
        completed = self.run_workflow("--snapshot", "vm/100/../../x", "--yes", expected=1)
        self.assertIn("--snapshot must look like", completed.stderr)
        self.assertEqual(self.changing_calls(), [])


if __name__ == "__main__":
    unittest.main()
