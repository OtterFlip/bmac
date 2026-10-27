# Copyright (c) 2026 BEENTHERE VENTURES, INC.
# SPDX-License-Identifier: GPL-3.0-only

"""Tests for the backups kind of scripts/utilities/quick_state.py, behind
scripts/user_callable/backups/list_backups.sh."""

from __future__ import annotations

import copy
import importlib.util
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import textwrap
import time
import unittest

SCRIPTS_DIR = Path(__file__).resolve().parent.parent
LIB_DIR = SCRIPTS_DIR / "lib"
QUICK_STATE = SCRIPTS_DIR / "utilities" / "quick_state.py"
sys.path.insert(0, str(Path(__file__).resolve().parent))

from ui_test_driver import run_json, scripted  # noqa: E402

SPEC = importlib.util.spec_from_file_location("quick_state", QUICK_STATE)
assert SPEC and SPEC.loader
quick_state = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(quick_state)

NOW = 1_793_120_000
KEY = "24:a7:6e:9e:65:9f:c8:d6:b1:08:9a:2f:b5:dc:3a:61:a4:0b:98:6b:d5:2e:c1:c3:e2:dd:d5:2f:71:b6:89:d9"
NOTES = "bmac prod1 slug=pinbinapp fqdn=pinbinapp.com cluster=MyAppCloud guest=prod1"

# Shapes captured from PVE 9.2 and PBS 4.2 on the test cluster.
REPORT = {
    "probe": "mox1",
    "collected_at": NOW,
    "storage": {
        "content": "backup", "datastore": "pbs", "encryption-key": KEY,
        "fingerprint": "5d:17:0f:52:9d:03:4a:6d:14:78:51:f2:59:55:f5:e9:2c:2d:e4:52:7f:4f:84:c0:da:76:4b:ea:34:fa:07:8d",
        "master-pubkey": "1", "namespace": "BTV", "port": 8007, "prune-backups": "keep-all=1",
        "server": "202.69.147.234", "shared": 1, "storage": "bmac-pbs", "type": "pbs", "username": "btv@pbs!pve",
    },
    "storage_status": {"active": 1, "avail": 0, "content": "backup", "enabled": 1, "shared": 1, "total": 0,
                       "type": "pbs", "used": 0},
    "content": [
        {"content": "backup", "ctime": NOW - 3600, "encrypted": KEY, "format": "pbs-vm", "notes": NOTES,
         "protected": 1, "size": 1099512170236, "subtype": "qemu", "vmid": 100,
         "volid": "bmac-pbs:backup/vm/100/2026-10-27T14:50:01Z"},
        {"content": "backup", "ctime": NOW - 7200, "encrypted": KEY, "format": "pbs-vm", "notes": "manual test",
         "size": 1099512170236, "subtype": "qemu", "vmid": 100,
         "volid": "bmac-pbs:backup/vm/100/2026-10-27T13:50:01Z"},
    ],
    "inspect": {
        "datastore": "pbs", "namespace": "BTV", "server": "202.69.147.234:8007", "username": "btv@pbs!pve",
        "version": "4.2",
        "privileges": {"excess": [], "missing": [], "path": "/datastore/pbs/BTV"},
        "prune": {"readable": True, "jobs": [{"id": "prune-btv", "keep-daily": 3, "keep-last": 6, "keep-weekly": 4,
                                               "ns": "BTV", "schedule": "05:00", "store": "pbs"}]},
        "snapshots": {"count": 2, "groups": ["vm/100"], "key_fingerprints": [KEY], "readable": True, "unencrypted": 0},
    },
    "jobs": [
        {"comment": "managed-by=bmac; prod1", "enabled": 1, "fleecing": {"enabled": 1, "storage": "local-zfs"},
         "id": "bmac-prod1", "mode": "snapshot", "next-run": NOW + 1800,
         "notes-template": "bmac prod1 slug=pinbinapp fqdn=pinbinapp.com cluster={{cluster}} guest={{guestname}}",
         "prune-backups": {"keep-all": 1}, "repeat-missed": 1, "schedule": "0/4:05", "storage": "bmac-pbs",
         "type": "vzdump", "vmid": "100"},
        {"id": "backup-1a2b3c", "schedule": "21:00", "storage": "local", "vmid": "100", "enabled": 1},
    ],
    "resources": [
        {"kind": "production", "index": 1, "name": "prod1", "vmid": 100, "state": "active",
         "domains": {"primary": "pinbinapp.com", "aliases": []}, "purpose": {"slug": "pinbinapp"}},
        {"kind": "production", "index": 2, "name": "prod2", "vmid": 101, "state": "active",
         "domains": {"primary": "shop.example.com", "aliases": []}, "purpose": {"slug": "shop"}},
        {"kind": "staging", "index": 1, "name": "stage1prod1", "vmid": 201, "state": "active"},
    ],
    "errors": [],
}


# vzdump task logs as PVE 9.2 records them: the first backup of prod1 (no
# dirty bitmap yet, so the whole disk was read) and a later incremental one.
FIRST_BACKUP_LOG = """INFO: starting new backup job: vzdump 100 --fleecing '1,storage=local-zfs' --mode snapshot --storage bmac-pbs
INFO: Starting Backup of VM 100 (qemu)
INFO: Backup started at 2026-10-27 08:50:01
INFO: status = running
INFO: VM Name: prod1
INFO: include disk 'scsi0' 'local-zfs:vm-100-disk-1' 1T
INFO: include disk 'efidisk0' 'local-zfs:vm-100-disk-0' 1M
INFO: backup mode: snapshot
INFO: creating Proxmox Backup Server archive 'vm/100/2026-10-27T14:50:01Z'
INFO: enabling encryption with master key feature
INFO: efidisk0: dirty-bitmap status: created new
INFO: scsi0: dirty-bitmap status: created new
INFO:   0% (1.5 GiB of 1.0 TiB) in 3s, read: 516.0 MiB/s, write: 121.3 MiB/s
INFO: 100% (1.0 TiB of 1.0 TiB) in 6m 53s, read: 2.6 GiB/s, write: 2.8 MiB/s
INFO: backup is sparse: 1016.54 GiB (99%) total zero data
INFO: backup was done incrementally, reused 1018.59 GiB (99%)
INFO: transferred 1.00 TiB in 413 seconds (2.5 GiB/s)
INFO: adding notes to backup
INFO: Finished Backup of VM 100 (00:06:54)
INFO: Backup finished at 2026-10-27 08:56:55
INFO: Backup job finished successfully""".splitlines()

INCREMENTAL_LOG = """INFO: Starting Backup of VM 100 (qemu)
INFO: creating Proxmox Backup Server archive 'vm/100/2026-10-27T16:05:02Z'
INFO: efidisk0: dirty-bitmap status: OK (drive clean)
INFO: scsi0: dirty-bitmap status: OK (68.0 MiB of 1.0 TiB dirty)
INFO: using fast incremental mode (dirty-bitmap), 68.0 MiB dirty of 1.0 TiB total
INFO: backup was done incrementally, reused 1023.94 GiB (99%)
INFO: transferred 68.00 MiB in 1 seconds (68.0 MiB/s)
INFO: Finished Backup of VM 100 (00:00:03)""".splitlines()


class BackupLogTest(unittest.TestCase):
    def test_first_backup_reads_the_whole_disk(self) -> None:
        [(snapshot, metrics)] = quick_state.parse_vzdump_log(FIRST_BACKUP_LOG).items()
        self.assertEqual(snapshot, "vm/100/2026-10-27T14:50:01Z")
        self.assertEqual(metrics["mode"], "full-read")
        self.assertIn("no dirty bitmap yet", metrics["detail"])
        self.assertIsNone(metrics["dirty_bytes"])
        self.assertEqual(metrics["read_bytes"], 2**40)
        self.assertEqual(metrics["zero_bytes"], int(1016.54 * 2**30))
        self.assertEqual((metrics["reused_bytes"], metrics["reused_percent"]), (int(1018.59 * 2**30), 99))
        self.assertEqual((metrics["transfer_seconds"], metrics["duration_seconds"]), (413, 414))
        # The image is 1 TiB: about 1 TiB / 99% reused.
        self.assertAlmostEqual(metrics["new_bytes"] / 2**30, 1018.59 / 0.99 - 1018.59, delta=0.01)

    def test_incremental_backup_reads_only_the_changes(self) -> None:
        [(snapshot, metrics)] = quick_state.parse_vzdump_log(INCREMENTAL_LOG).items()
        self.assertEqual(snapshot, "vm/100/2026-10-27T16:05:02Z")
        self.assertEqual(metrics, {
            "mode": "fast-incremental",
            "detail": "only the blocks changed since the previous backup were read (dirty bitmap)",
            "dirty_bytes": 68 * 2**20, "read_bytes": 68 * 2**20, "zero_bytes": None,
            "reused_bytes": int(1023.94 * 2**30), "reused_percent": 99,
            "new_bytes": 2**40 - int(1023.94 * 2**30), "transfer_seconds": 1, "duration_seconds": 3,
        })

    def test_a_job_log_holds_every_vms_backup(self) -> None:
        second = [line.replace("VM 100", "VM 101").replace("vm/100/", "vm/101/") for line in INCREMENTAL_LOG]
        failed = ["INFO: Starting Backup of VM 102 (qemu)",
                  "INFO: creating Proxmox Backup Server archive 'vm/102/2026-10-27T16:05:09Z'",
                  "ERROR: Backup of VM 102 failed - interrupted"]
        found = quick_state.parse_vzdump_log(FIRST_BACKUP_LOG + second + failed)
        self.assertEqual(sorted(found), ["vm/100/2026-10-27T14:50:01Z", "vm/101/2026-10-27T16:05:02Z"])

    def test_an_invalid_bitmap_is_a_full_read(self) -> None:
        log = [line.replace("created new", "existing bitmap was invalid and has been cleared") for line in FIRST_BACKUP_LOG]
        [metrics] = quick_state.parse_vzdump_log(log).values()
        self.assertEqual(metrics["mode"], "full-read")
        self.assertIn("no longer matched", metrics["detail"])


class BackupsRenderTest(unittest.TestCase):
    def setUp(self) -> None:
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.env = {key: value for key, value in os.environ.items() if not key.startswith("BMAC_UI")}

    def render(self, data: dict, *extra: str):
        path = self.root / "report.json"
        path.write_text(json.dumps(data))
        harness = self.root / "render.sh"
        harness.write_text(
            "#!/usr/bin/env bash\n"
            f"source {LIB_DIR / 'ui_protocol.sh'}\n"
            'bmac_ui_bootstrap "$@"\n'
            f'exec python3 {QUICK_STATE} render "$@"\n'
        )
        harness.chmod(0o755)
        run = run_json([str(harness), "backups", "--report", str(path), *extra], scripted(), env=self.env, timeout=60)
        self.assertEqual(run.completed.get("status"), "success", run.describe())
        results = run.of("result")
        self.assertEqual(len(results), 1, run.describe())
        return results[0]["data"], run.of("next_step")

    def render_terminal(self, data: dict, *extra: str) -> str:
        path = self.root / "report.json"
        path.write_text(json.dumps(data))
        return subprocess.run(
            [sys.executable, str(QUICK_STATE), "render", "backups", "--report", str(path), *extra],
            capture_output=True, text=True, env=self.env, check=True,
        ).stdout

    def test_a_healthy_cluster(self) -> None:
        data, steps = self.render(REPORT)
        self.assertEqual(data["problems"], [])
        self.assertEqual(steps, [])
        storage = data["storage"]
        self.assertEqual((storage["configured"], storage["active"], storage["namespace"], storage["key_fingerprint"]),
                         (True, True, "BTV", KEY))
        self.assertTrue(storage["master_pubkey"])
        self.assertEqual(data["retention"], {"readable": True, "error": None, "jobs": [
            {"id": "prune-btv", "schedule": "05:00", "keep": {"daily": 3, "last": 6, "weekly": 4}}]})
        prod1, prod2 = data["vms"]
        self.assertEqual(prod1["job"], {"id": "bmac-prod1", "enabled": True, "schedule": "0/4:05",
                                        "every_hours": 4, "next_run": NOW + 1800, "bwlimit": None})
        self.assertEqual((prod1["latest_backup"], prod1["restore_points"]), (NOW - 3600, 1))
        self.assertEqual((prod2["job"], prod2["latest_backup"], prod2["restore_points"]), (None, None, 0))
        self.assertEqual(data["unmanaged_backups"], 1)
        [point] = data["restore_points"]
        self.assertEqual(point, {
            "volid": "bmac-pbs:backup/vm/100/2026-10-27T14:50:01Z", "vmid": 100, "time": NOW - 3600,
            "name": "prod1", "purpose": "pinbinapp", "fqdn": "pinbinapp.com", "cluster": "MyAppCloud",
            "notes": NOTES, "comment": None, "protected": True, "size": 1099512170236, "encrypted": KEY,
            "key_matches": True, "verified": None, "metrics": None,
        })

    def test_metrics_from_the_report_are_shown(self) -> None:
        fresh = quick_state.parse_vzdump_log(FIRST_BACKUP_LOG)
        report = {**REPORT, "metrics": {k: {**v, "node": "mox1", "task": "UPID:x"} for k, v in fresh.items()}}
        data, _ = self.render(report)
        self.assertEqual(data["restore_points"][0]["metrics"]["mode"], "full-read")
        self.assertEqual(data["restore_points"][0]["metrics"]["read_bytes"], 2**40)
        out = self.render_terminal(report)
        self.assertIn("full read", out)
        self.assertIn("1018.6 GiB (99%)", out)

    def test_restore_point_times_are_in_the_workstations_zone(self) -> None:
        self.env["TZ"] = "America/Denver"
        out = self.render_terminal(REPORT)
        taken = NOW - 3600
        self.assertIn(time.strftime("%Y-%m-%d %H:%M", time.gmtime(taken - 6 * 3600)) + " MDT", out)
        self.assertIn("TAKEN", out)

    def test_terminal_report(self) -> None:
        out = self.render_terminal(REPORT)
        self.assertIn("bmac-pbs: active, btv@pbs!pve at 202.69.147.234:8007, datastore pbs, namespace BTV", out)
        self.assertIn("Retention (prune-btv, runs 05:00 PBS time): keep-daily 3, keep-last 6, keep-weekly 4", out)
        self.assertIn("every 4h", out)
        self.assertIn("1 other backup(s) in the namespace were not made by BMAC", out)
        self.assertIn("OK: nothing needs attention.", out)

    def test_no_storage_suggests_setup_pbs(self) -> None:
        report = {**REPORT, "storage": None, "storage_status": None, "content": [], "inspect": None, "jobs": []}
        data, steps = self.render(report)
        self.assertFalse(data["storage"]["configured"])
        self.assertIsNone(data["storage"]["active"])
        self.assertEqual(data["problems"], [])
        self.assertEqual([step["workflow"] for step in steps], ["setup_pbs"])

    def test_an_overdue_backup_is_a_problem(self) -> None:
        report = copy.deepcopy(REPORT)
        report["content"][0]["ctime"] = NOW - 10 * 3600
        data, _ = self.render(report)
        self.assertEqual(data["problems"], ["prod1's newest backup is 10h ago, but it is backed up every 4h"])
        report["content"][0]["ctime"] = NOW - 8 * 3600
        data, _ = self.render(report)
        self.assertEqual(data["problems"], [])

    def test_storage_key_and_job_problems(self) -> None:
        report = copy.deepcopy(REPORT)
        report["storage_status"]["active"] = 0
        report["content"].append({**report["content"][0], "volid": "bmac-pbs:backup/vm/100/2026-10-20T00:00:00Z",
                                  "encrypted": "11:22", "protected": 0})
        report["content"].append({**report["content"][0], "volid": "bmac-pbs:backup/vm/100/2026-10-19T00:00:00Z",
                                  "encrypted": None})
        report["jobs"].append({"id": "bmac-prod3", "vmid": "105", "storage": "bmac-pbs", "schedule": "0/4:19"})
        report["jobs"][0]["vmid"] = "999"
        report["inspect"]["privileges"]["excess"] = [
            {"path": "/datastore/pbs/BTV", "privilege": "Datastore.Prune", "dangerous": True}]
        data, _ = self.render(report)
        problems = "\n".join(data["problems"])
        for text in ("storage bmac-pbs is not active on mox1",
                     "1 BMAC restore point(s) are not encrypted",
                     "1 restore point(s) were encrypted with another key",
                     "backup job bmac-prod1 does not back up prod1 (VMID 100)",
                     "backup job bmac-prod3 belongs to no registered production VM",
                     "Datastore.Prune on /datastore/pbs/BTV, which can delete backups"):
            self.assertIn(text, problems)
        others = [p for p in data["restore_points"] if not p["key_matches"]]
        self.assertEqual(sorted(str(p["encrypted"]) for p in others), ["11:22", "None"])

    def test_an_unreadable_policy_is_reported_not_guessed(self) -> None:
        report = copy.deepcopy(REPORT)
        report["inspect"] = {"error": "PBS rejected the API token"}
        data, _ = self.render(report)
        self.assertEqual(data["retention"]["jobs"], [])
        self.assertFalse(data["retention"]["readable"])
        self.assertIn("could not read the PBS account: PBS rejected the API token", data["problems"])


class BackupHelpersTest(unittest.TestCase):
    def test_notes(self) -> None:
        self.assertEqual(quick_state.backup_notes(NOTES), {
            "name": "prod1", "comment": None, "slug": "pinbinapp", "fqdn": "pinbinapp.com", "cluster": "MyAppCloud", "guest": "prod1"})
        self.assertIsNone(quick_state.backup_notes("manual test"))
        self.assertIsNone(quick_state.backup_notes(None))
        self.assertEqual(quick_state.backup_notes("bmac prod2\nsecond line")["name"], "prod2")
        self.assertIsNone(quick_state.backup_notes(NOTES)["comment"])
        commented = quick_state.backup_notes(NOTES + " | Before upgrading Postgres | really")
        self.assertEqual((commented["comment"], commented["guest"]), ("Before upgrading Postgres | really", "prod1"))

    def test_comments_are_listed(self) -> None:
        report = copy.deepcopy(REPORT)
        report["content"][0]["notes"] = NOTES + " | Before upgrading Postgres"
        render = BackupsRenderTest("test_terminal_report")
        render.setUp()
        try:
            data, _ = render.render(report)
            self.assertEqual(data["restore_points"][0]["comment"], "Before upgrading Postgres")
            self.assertEqual(data["restore_points"][0]["fqdn"], "pinbinapp.com")
            self.assertIn("prod1: Before upgrading Postgres", render.render_terminal(report))
        finally:
            render.temp.cleanup()

    def test_interval(self) -> None:
        for schedule, hours in (("0/4:05", 4), ("0/1:59", 1), ("01:05", 24), ("21:00", 24),
                                ("0/5:00", None), ("mon 02:00", None), ("", None)):
            with self.subTest(schedule=schedule):
                self.assertEqual(quick_state.backup_interval_hours(schedule), hours)


class BackupsCollectTest(unittest.TestCase):
    def test_collect_backups_uses_only_read_only_commands(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            bin_dir = root / "bin"
            bin_dir.mkdir()
            calls = root / "calls"
            node = os.uname().nodename.split(".")[0]
            upid = "UPID:mox1:003FD8EC:02CB1464:6AE0BA19:vzdump:100:root@pam:"
            answers = {
                "/storage": [REPORT["storage"], {"storage": "local", "type": "dir"}],
                f"/nodes/{node}/storage/bmac-pbs/status": REPORT["storage_status"],
                f"/nodes/{node}/storage/bmac-pbs/content": REPORT["content"],
                "/cluster/backup": REPORT["jobs"],
                "/nodes": [{"node": "mox1", "status": "online"}, {"node": "mox2", "status": "offline"}],
                "/nodes/mox1/tasks": [
                    {"upid": upid, "starttime": NOW - 3700, "endtime": NOW - 3290, "type": "vzdump", "status": "OK"},
                    {"upid": "UPID:other", "starttime": NOW - 90000, "endtime": NOW - 89000, "type": "vzdump"},
                ],
                f"/nodes/mox1/tasks/{upid}/log": [
                    {"n": i + 1, "t": line} for i, line in enumerate(FIRST_BACKUP_LOG)],
            }
            (root / "answers.json").write_text(json.dumps(answers))
            (bin_dir / "pvesh").write_text(textwrap.dedent(f"""\
                #!/usr/bin/env python3
                import json, sys
                open({str(calls)!r}, "a").write("pvesh " + " ".join(sys.argv[1:]) + "\\n")
                if sys.argv[1] != "get":
                    sys.exit(9)
                answers = json.load(open({str(root / "answers.json")!r}))
                if sys.argv[2] not in answers:
                    sys.exit(1)
                print(json.dumps(answers[sys.argv[2]]))
                """))
            (bin_dir / "registry").write_text(textwrap.dedent(f"""\
                #!/usr/bin/env python3
                import json, sys
                open({str(calls)!r}, "a").write("registry " + " ".join(sys.argv[1:]) + "\\n")
                print({json.dumps(json.dumps(REPORT["resources"]))})
                """))
            pbs_api = root / "pbs_api.py"
            pbs_api.write_text(textwrap.dedent(f"""\
                import json, sys
                open({str(calls)!r}, "a").write("pbs_api " + " ".join(sys.argv[1:]) + "\\n")
                print({json.dumps(json.dumps(REPORT["inspect"]))})
                """))
            for path in (bin_dir / "pvesh", bin_dir / "registry", pbs_api):
                path.chmod(0o755)
            cache = root / "var-lib-app-ha" / "backup-metrics.json"
            completed = subprocess.run(
                [sys.executable, "-", "collect", "backups", "--registry", str(bin_dir / "registry"),
                 "--state-dir", str(root), "--pbs-api", str(pbs_api), "--metrics-cache", str(cache)],
                input=QUICK_STATE.read_text(encoding="utf-8"), capture_output=True, text=True, check=True,
                env={**os.environ, "PATH": f"{bin_dir}:{os.environ['PATH']}"},
            )
            report = json.loads(completed.stdout)
            self.assertEqual(report["errors"], [])
            self.assertEqual(report["storage"]["storage"], "bmac-pbs")
            self.assertEqual(report["content"], REPORT["content"])
            self.assertEqual(report["inspect"]["prune"], REPORT["inspect"]["prune"])
            self.assertEqual(sorted(report["metrics"]), ["vm/100/2026-10-27T14:50:01Z"])
            self.assertEqual(report["metrics"]["vm/100/2026-10-27T14:50:01Z"]["mode"], "full-read")
            self.assertEqual(report["metrics"]["vm/100/2026-10-27T14:50:01Z"]["node"], "mox1")
            commands = calls.read_text().splitlines()
            # Only the task whose run covers the restore point had its log read.
            self.assertEqual([c for c in commands if c.endswith("/log --limit 100000 --output-format json")],
                             [f"pvesh get /nodes/mox1/tasks/{upid}/log --limit 100000 --output-format json"])
            stored = json.loads(cache.read_text())["restore_points"]
            self.assertEqual(sorted(stored), ["202.69.147.234/pbs/BTV/vm/100/2026-10-27T14:50:01Z"])
            self.assertEqual(cache.stat().st_mode & 0o777, 0o600)
            self.assertTrue(all(line.startswith(("pvesh get ", "registry ", "pbs_api inspect --storage bmac-pbs"))
                                for line in commands), commands)

    def test_collect_without_storage_skips_pbs(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            bin_dir = root / "bin"
            bin_dir.mkdir()
            (bin_dir / "pvesh").write_text("#!/usr/bin/env bash\nprintf '[]'\n")
            (bin_dir / "pvesh").chmod(0o755)
            completed = subprocess.run(
                [sys.executable, "-", "collect", "backups", "--pbs-api", str(root / "missing.py"),
                 "--metrics-cache", str(root / "cache.json")],
                input=QUICK_STATE.read_text(encoding="utf-8"), capture_output=True, text=True, check=True,
                env={**os.environ, "PATH": f"{bin_dir}:{os.environ['PATH']}"},
            )
            report = json.loads(completed.stdout)
            self.assertIsNone(report["storage"])
            self.assertIsNone(report["inspect"])
            self.assertEqual(report["metrics"], {})



class BackupMetricsCacheTest(unittest.TestCase):
    """collect_backup_metrics with this host's cache, other hosts' caches, and
    task logs faked."""

    STORAGE = REPORT["storage"]
    PREFIX = "202.69.147.234/pbs/BTV/"
    SNAPSHOT = "vm/100/2026-10-27T14:50:01Z"

    def setUp(self) -> None:
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.cache = Path(self.temporary.name) / "backup-metrics.json"
        self.calls: list[str] = []
        self.peers: dict[str, dict[str, object]] = {}
        self.logs: dict[str, list[str]] = {}
        originals = (quick_state.pvesh, quick_state.peer_metrics_cache)
        self.addCleanup(lambda: setattr(quick_state, "pvesh", originals[0]))
        self.addCleanup(lambda: setattr(quick_state, "peer_metrics_cache", originals[1]))
        quick_state.pvesh = self.fake_pvesh
        quick_state.peer_metrics_cache = self.fake_peer
        self.probe = os.uname().nodename.split(".")[0]

    def fake_pvesh(self, path, *extra):
        self.calls.append(path)
        if path == "/nodes":
            return [{"node": self.probe, "status": "online"}, {"node": "moxpeer", "status": "online"}], None
        if path.endswith("/tasks"):
            node = path.split("/")[2]
            return [{"upid": f"UPID:{node}", "starttime": NOW - 3700, "endtime": NOW - 3290}] if node in self.logs else [], None
        if path.endswith("/log"):
            node = path.split("/")[2]
            return [{"n": i + 1, "t": line} for i, line in enumerate(self.logs[node])], None
        raise AssertionError(path)

    def fake_peer(self, node, domain, path):
        self.calls.append(f"peer {node} {domain}")
        return self.peers.get(node, {})

    def collect(self) -> dict[str, object]:
        args = quick_state.argparse.Namespace(metrics_cache=str(self.cache), internal_domain="pve.internal")
        errors: list[str] = []
        metrics = quick_state.collect_backup_metrics(REPORT["content"], self.STORAGE, args, errors)
        self.assertEqual(errors, [])
        return metrics

    def stored(self) -> dict[str, object]:
        return json.loads(self.cache.read_text())["restore_points"]

    def test_logs_are_read_once_then_served_from_the_cache(self) -> None:
        self.logs["moxpeer"] = FIRST_BACKUP_LOG
        first = self.collect()
        self.assertEqual(first[self.SNAPSHOT]["node"], "moxpeer")
        self.assertIn("/nodes/moxpeer/tasks/UPID:moxpeer/log", self.calls)
        self.assertEqual(sorted(self.stored()), [self.PREFIX + self.SNAPSHOT])
        self.calls.clear()
        self.assertEqual(self.collect(), first)
        self.assertEqual(self.calls, [])

    def test_another_hosts_cache_comes_before_any_log(self) -> None:
        self.peers["moxpeer"] = {self.PREFIX + self.SNAPSHOT: {"mode": "fast-incremental", "node": "moxpeer"}}
        self.logs["moxpeer"] = FIRST_BACKUP_LOG
        metrics = self.collect()
        self.assertEqual(metrics[self.SNAPSHOT]["mode"], "fast-incremental")
        self.assertIn("peer moxpeer pve.internal", self.calls)
        self.assertFalse([call for call in self.calls if call.endswith("/log")])
        self.assertEqual(self.stored()[self.PREFIX + self.SNAPSHOT]["mode"], "fast-incremental")

    def test_pruned_points_leave_the_cache_and_other_storages_stay(self) -> None:
        other = "other.example/pbs/FB/vm/100/2026-10-01T00:00:00Z"
        quick_state.save_metrics_cache(str(self.cache), {
            self.PREFIX + "vm/100/2026-10-01T00:00:00Z": {"mode": "full-read"},
            self.PREFIX + self.SNAPSHOT: {"mode": "full-read"},
            other: {"mode": "full-read"},
        })
        self.collect()
        self.assertEqual(sorted(self.stored()), sorted([self.PREFIX + self.SNAPSHOT, other]))
        self.assertEqual(self.calls, [])

    def test_a_point_without_a_log_is_not_cached(self) -> None:
        self.assertEqual(self.collect(), {})
        self.assertFalse(self.cache.exists())


if __name__ == "__main__":
    unittest.main()
