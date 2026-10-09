# Copyright (c) 2026 BEENTHERE VENTURES, INC.
# SPDX-License-Identifier: GPL-3.0-only

"""Tests for scripts/utilities/quick_state.py, the engine behind
scripts/user_callable/diagnostics/list_*.sh."""

from __future__ import annotations

import json
import os
import subprocess
import sys
import tempfile
import textwrap
import unittest
from pathlib import Path

SCRIPTS_DIR = Path(__file__).resolve().parent.parent
LIB_DIR = SCRIPTS_DIR / "lib"
QUICK_STATE = SCRIPTS_DIR / "utilities" / "quick_state.py"
sys.path.insert(0, str(Path(__file__).resolve().parent))

from ui_test_driver import run_json, scripted  # noqa: E402

NOW = 1_800_000_000
GIB = 2**30

HOSTS_REPORT = {
    "probe": "mox1",
    "collected_at": NOW,
    "nodes": [
        {"node": "mox2", "status": "offline", "maxcpu": 8, "maxmem": 64 * GIB},
        {"node": "mox1", "status": "online", "uptime": 90061, "cpu": 0.25, "maxcpu": 8,
         "mem": 16 * GIB, "maxmem": 64 * GIB},
    ],
    "cluster_status": [
        {"type": "cluster", "name": "bmac", "quorate": 1},
        {"type": "node", "name": "mox1", "nodeid": 1, "ip": "10.0.0.1"},
        {"type": "node", "name": "mox2", "nodeid": 2, "ip": "10.0.0.2"},
    ],
    "pvecm_status": {"rc": 0, "out": "Quorate:          Yes\nExpected votes:   2\nTotal votes:      1\n"
                                     "Quorum:           1\nFlags:            Quorate\n", "err": ""},
    "corosync_qdevice": {"registered": False},
    "host_slots": [{"node": "mox1", "state": "member"}, {"node": "mox2", "state": "member"}],
    "control": {"control_node": "mox1"},
    "errors": [],
}

GUESTS_REPORT = {
    "probe": "mox1",
    "collected_at": NOW,
    "resources": [
        {"kind": "production", "index": 1, "name": "prod1", "vmid": 101, "state": "active",
         "ip": "10.1.0.11", "placement": ["mox1", "mox2"], "owner_node": "mox1",
         "routes_enabled": 1, "domains": {"primary": "example.com", "aliases": []},
         "purpose": {"slug": "production"}, "spec": {"cores": 2, "memory_mb": 4096, "disk_gib": 32}},
        {"kind": "staging", "index": 1, "name": "stage1prod1", "vmid": 201, "state": "failed",
         "ip": "10.1.0.51", "placement": ["mox2"], "source": "prod1",
         "domains": {"primary": "stage1prod1.example.com"}},
    ],
    "routes": [{"resource": "prod1"}],
    "live": [
        {"vmid": 101, "type": "qemu", "status": "stopped", "node": "mox1"},
        {"vmid": 900, "type": "qemu", "status": "running", "node": "mox2", "name": "stray"},
    ],
    "ha_resources": [{"sid": "vm:101", "state": "started"}],
    "ha_status": [{"sid": "vm:101", "state": "stopped", "node": "mox1"}],
    "nodes": [{"node": "mox1", "status": "online"}],
    "replication": [{"id": "101-0", "guest": 101, "source": "mox1", "target": "mox2",
                     "last_sync": NOW - 600, "fail_count": 2, "error": "dataset is busy"}],
    "errors": [],
}

STORAGE_REPORT = {
    "probe": "mox1",
    "collected_at": NOW,
    "hosts": [
        {"node": "mox1", "online": True, "errors": [],
         "pools": [{"name": "rpool", "health": "DEGRADED", "size": 100 * GIB, "alloc": 95 * GIB,
                    "free": 5 * GIB, "frag": "12%"}],
         "storages": [{"storage": "local-zfs", "type": "zfspool", "active": 1, "used": 95 * GIB,
                       "avail": 5 * GIB}]},
        {"node": "mox2", "online": False, "errors": [], "pools": [], "storages": []},
    ],
    "errors": [],
}


class QuickStateTest(unittest.TestCase):
    def setUp(self) -> None:
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.env = {key: value for key, value in os.environ.items() if not key.startswith("BMAC_UI")}

    def report(self, data: dict) -> str:
        path = self.root / "report.json"
        path.write_text(json.dumps(data))
        return str(path)

    def render_terminal(self, kind: str, data: dict, *extra: str) -> str:
        completed = subprocess.run(
            [sys.executable, str(QUICK_STATE), "render", kind, "--report", self.report(data), *extra],
            capture_output=True, text=True, env=self.env, check=True,
        )
        self.assertEqual(completed.stderr, "")
        return completed.stdout

    def render_json(self, kind: str, data: dict, *extra: str):
        harness = self.root / "render.sh"
        harness.write_text(
            "#!/usr/bin/env bash\n"
            f"source {LIB_DIR / 'ui_protocol.sh'}\n"
            'bmac_ui_bootstrap "$@"\n'
            f'exec python3 {QUICK_STATE} render "$@"\n'
        )
        harness.chmod(0o755)
        run = run_json([str(harness), kind, "--report", self.report(data), *extra],
                       scripted(), env=self.env, timeout=60)
        self.assertEqual(run.completed.get("status"), "success", run.describe())
        results = run.of("result")
        self.assertEqual(len(results), 1, run.describe())
        return results[0]["data"], run.of("next_step"), run

    def test_hosts_report_offline_host_and_missing_qdevice(self) -> None:
        text = self.render_terminal("hosts", HOSTS_REPORT)
        self.assertIn("CLUSTER bmac - read through mox1", text)
        self.assertIn("OFFLINE", text)
        self.assertIn("ATTENTION: 2 items need attention:", text)
        self.assertNotIn('"type"', text)

        summary, steps, _ = self.render_json("hosts", HOSTS_REPORT)
        self.assertEqual([host["name"] for host in summary["hosts"]], ["mox1", "mox2"])
        self.assertTrue(summary["hosts"][0]["is_control"])
        self.assertEqual(summary["cluster"]["online_count"], 1)
        self.assertTrue(summary["qdevice"]["needed"])
        self.assertEqual(len(summary["problems"]), 2)
        self.assertEqual({step["workflow"] for step in steps}, {"show_cluster_health", "show_qdevice_state"})

    def test_hosts_ssh_check_is_reported(self) -> None:
        healthy = json.loads(json.dumps(HOSTS_REPORT))
        healthy["nodes"][0].update(status="online", uptime=10)
        healthy["corosync_qdevice"] = {"registered": True, "address": "100.64.0.9"}
        healthy["pvecm_status"]["out"] += "0x00000000          1    Qdevice\n"
        healthy["pvecm_status"]["out"] = healthy["pvecm_status"]["out"].replace(
            "Flags:            Quorate", "Flags:            Quorate Qdevice")
        ssh = self.root / "ssh.tsv"
        ssh.write_text("mox1\tok\nmox2\tfailed\n")
        summary, steps, _ = self.render_json("hosts", healthy, "--ssh-file", str(ssh))
        self.assertTrue(summary["qdevice"]["voting"])
        self.assertEqual(summary["problems"], ["this workstation cannot SSH to mox2"])
        self.assertEqual([step["workflow"] for step in steps], ["show_cluster_health"])

    def test_guests_report_problems_and_suggest_scoped_workflows(self) -> None:
        summary, steps, _ = self.render_json("guests", GUESTS_REPORT)
        prod = summary["production"][0]
        self.assertEqual(prod["live_status"], "stopped")
        self.assertEqual(prod["replication"]["failing"], 1)
        self.assertEqual(prod["replication"]["errors"], ["dataset is busy"])
        self.assertEqual(summary["staging"][0]["url"], "https://stage1prod1.example.com")
        self.assertEqual(summary["unregistered"], [{"vmid": 900, "name": "stray", "node": "mox2",
                                                    "status": "running"}])
        self.assertIn("prod1 is registered active but is stopped", summary["problems"])
        self.assertIn("staging stage1prod1 is failed", summary["problems"])
        self.assertEqual(
            [(step["workflow"], step["args"]) for step in steps],
            [("show_prod_vm_state", {"resource": "prod1"}),
             ("remove_staging_vm", {"resource": "stage1prod1"})],
        )

        self.assertEqual(prod["disk_bytes"], 32 * GIB)

        grown = json.loads(json.dumps(GUESTS_REPORT))
        grown["resources"][0]["spec"]["disk_bytes"] = 48 * GIB
        grown_summary, _, _ = self.render_json("guests", grown)
        self.assertEqual(grown_summary["production"][0]["disk_gib"], 32)
        self.assertEqual(grown_summary["production"][0]["disk_bytes"], 48 * GIB)

        filtered, _, _ = self.render_json("guests", GUESTS_REPORT, "--kind-filter", "staging")
        self.assertEqual(filtered["production"], [])
        self.assertNotIn("PRODUCTION VMS", self.render_terminal("guests", GUESTS_REPORT, "--kind-filter", "staging"))

    def test_replication_filter_by_guest(self) -> None:
        summary, steps, _ = self.render_json("replication", GUESTS_REPORT, "--guest", "prod1")
        self.assertEqual([job["id"] for job in summary["jobs"]], ["101-0"])
        self.assertEqual(summary["jobs"][0]["guest_name"], "prod1")
        self.assertEqual(steps[0]["args"], {"resource": "prod1"})
        empty, _, _ = self.render_json("replication", GUESTS_REPORT, "--guest", "prod9")
        self.assertEqual(empty["jobs"], [])

    def test_storage_flags_unhealthy_and_full_pools(self) -> None:
        summary, steps, _ = self.render_json("storage", STORAGE_REPORT)
        pool = summary["hosts"][0]["pools"][0]
        self.assertAlmostEqual(pool["free_fraction"], 0.05)
        self.assertEqual(pool["frag"], 12)
        self.assertEqual(
            summary["problems"],
            ["mox1: pool rpool is DEGRADED", "mox1: pool rpool has only 5% free",
             "mox2 is offline; its storage was not inspected"],
        )
        self.assertEqual([(s["workflow"], s["args"]) for s in steps],
                         [("show_proxmox_host_state", {"host": "mox1"})])

    def test_unreadable_report_fails(self) -> None:
        completed = subprocess.run(
            [sys.executable, str(QUICK_STATE), "render", "hosts", "--report", str(self.root / "missing")],
            capture_output=True, text=True, env=self.env,
        )
        self.assertEqual(completed.returncode, 1)
        self.assertIn("could not read the collected state", completed.stderr)

    def test_collect_hosts_uses_only_read_only_commands(self) -> None:
        bin_dir = self.root / "bin"
        bin_dir.mkdir()
        calls = self.root / "calls"
        responses = {
            "/nodes": [{"node": "mox1", "status": "online"}],
            "/cluster/status": [{"type": "cluster", "name": "bmac", "quorate": 1}],
        }
        for name, body in {
            "pvesh": f"""
                printf 'pvesh %s\\n' "$*" >>{calls}
                [[ "$1" == get ]] || exit 9
                case "$2" in
                  /nodes) printf '%s' '{json.dumps(responses["/nodes"])}' ;;
                  /cluster/status) printf '%s' '{json.dumps(responses["/cluster/status"])}' ;;
                  *) exit 1 ;;
                esac
            """,
            "pvecm": f"""
                printf 'pvecm %s\\n' "$*" >>{calls}
                printf 'Quorate: Yes\\n'
            """,
            "registry": f"""
                printf 'registry %s\\n' "$*" >>{calls}
                case "$3" in
                  host-list) printf '[]' ;;
                  control-get) printf '{{"control_node":"mox1"}}' ;;
                esac
            """,
        }.items():
            path = bin_dir / name
            path.write_text("#!/usr/bin/env bash\n" + textwrap.dedent(body))
            path.chmod(0o755)
        completed = subprocess.run(
            [sys.executable, "-", "collect", "hosts", "--registry", str(bin_dir / "registry"),
             "--state-dir", str(self.root)],
            input=QUICK_STATE.read_text(encoding="utf-8"),
            capture_output=True, text=True, check=True,
            env={**self.env, "PATH": f"{bin_dir}:{os.environ['PATH']}"},
        )
        report = json.loads(completed.stdout)
        self.assertEqual(report["errors"], [])
        self.assertEqual(report["nodes"], responses["/nodes"])
        self.assertEqual(report["control"], {"control_node": "mox1"})
        self.assertEqual(report["pvecm_status"]["rc"], 0)
        commands = calls.read_text().splitlines()
        self.assertTrue(all(line.startswith(("pvesh get ", "pvecm status", "registry ")) for line in commands),
                        commands)


if __name__ == "__main__":
    unittest.main()
