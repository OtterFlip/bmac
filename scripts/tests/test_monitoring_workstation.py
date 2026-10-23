#!/usr/bin/env python3

# Copyright (c) 2026 BEENTHERE VENTURES, INC.
# SPDX-License-Identifier: GPL-3.0-only

"""Tests for BMAC's workstation-side monitoring: the Healthchecks client,
config rendering, the per-node installer, and the cluster-wide sync."""

from __future__ import annotations

import http.server
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import threading
import unittest
import urllib.parse
import uuid


SCRIPTS = Path(__file__).resolve().parent.parent
HEALTHCHECKS = SCRIPTS / "lib" / "healthchecks.py"
MONITOR_CONFIG = SCRIPTS / "lib" / "monitor_config.py"
NODE_INSTALL = SCRIPTS / "lib" / "monitor_node_install.sh"
AGENT = SCRIPTS / "host_runtime" / "app_ha_monitor.py"
API_KEY = "hcw_test_key_must_not_appear"
TAG = "bmac-cluster-myappcloud"

sys.path.insert(0, str(SCRIPTS / "lib"))
import monitor_config  # noqa: E402

CLUSTER_CONF = "\n".join((
    "PROXMOX_CLUSTER_NAME=MyAppCloud",
    "PROXMOX_QDEVICE_HOST=qdevice",
    "PROXMOX_CONTROL_NODE=mox1",
    "MAX_MOX_HOSTS=10",
    "PROXMOX_INTERNAL_DOMAIN=pve.internal",
    "PRIVATE_SUBNET_CIDR=10.213.0.0/24",
    "PROXMOX_MIGRATION_NETWORK=10.213.0.240/28",
    "DATACENTER_PRIVATE_VLAN_GATEWAY=10.213.0.1",
    "GUEST_GATEWAY_IP=10.213.0.10",
    "GUEST_GATEWAY_MAC=02:00:0a:d5:00:0a",
    "MOX_IP_START=10.213.0.11",
    "MOX_IP_END=10.213.0.20",
    "MOX_REPLICATION_IP_START=10.213.0.241",
    "MOX_REPLICATION_IP_END=10.213.0.250",
    "VM_SLOT_START=10.213.0.110",
    "MAX_PROD_VMS=10",
    "MAX_STAGING_VMS_PER_PROD=9",
    "CLOUDFLARE_ACCOUNT_ID=0123456789abcdef0123456789abcdef",
    "CLOUDFLARE_TUNNEL_ID=00000000-0000-0000-0000-000000000000",
    "CLUSTER_STATE_DIR=/etc/pve/priv/app-ha",
    "TAILSCALE_TAG=tag:proxmox-host",
    "MONITORING_ENABLED=true",
    "",
))


class FakeHealthchecks:
    """Enough of the v3 Management API, on 127.0.0.1, recording every call."""

    def __init__(self) -> None:
        self.checks: list[dict] = []
        self.calls: list[tuple[str, str, dict | None, str]] = []
        self.ping_on_list = False
        fake = self

        class Handler(http.server.BaseHTTPRequestHandler):
            def reply(self, status: int, body) -> None:
                data = json.dumps(body).encode()
                self.send_response(status)
                self.send_header("Content-Type", "application/json")
                self.send_header("Content-Length", str(len(data)))
                self.end_headers()
                self.wfile.write(data)

            def handle_any(self, method: str) -> None:
                length = int(self.headers.get("Content-Length") or 0)
                body = json.loads(self.rfile.read(length)) if length else None
                fake.calls.append((method, self.path, body, self.headers.get("X-Api-Key", "")))
                if self.headers.get("X-Api-Key") != API_KEY:
                    return self.reply(401, {"error": "missing api key"})
                parts = urllib.parse.urlsplit(self.path)
                if method == "GET" and parts.path == "/api/v3/checks/":
                    tags = urllib.parse.parse_qs(parts.query).get("tag", [])
                    if fake.ping_on_list:
                        for check in fake.checks:
                            check["status"], check["last_ping"] = "up", "2099-01-01T00:00:00+00:00"
                    listed = [c for c in fake.checks if all(t in c["tags"].split() for t in tags)]
                    return self.reply(200, {"checks": listed})
                if method == "POST" and parts.path == "/api/v3/checks/":
                    existing = next((c for c in fake.checks if c["name"] == body["name"]), None)
                    if existing:
                        existing.update({k: v for k, v in body.items() if k not in ("unique", "channels")})
                        return self.reply(200, existing)
                    check_uuid = str(uuid.uuid4())
                    check = {k: v for k, v in body.items() if k not in ("unique", "channels")}
                    check.update({"uuid": check_uuid, "ping_url": f"https://hc-ping.com/{check_uuid}",
                                  "update_url": f"https://healthchecks.io/api/v3/checks/{check_uuid}",
                                  "status": "new", "last_ping": None, "n_pings": 0})
                    fake.checks.append(check)
                    return self.reply(201, check)
                if method == "DELETE" and parts.path.startswith("/api/v3/checks/"):
                    check_uuid = parts.path.rsplit("/", 1)[1]
                    found = [c for c in fake.checks if c["uuid"] == check_uuid]
                    fake.checks = [c for c in fake.checks if c["uuid"] != check_uuid]
                    return self.reply(200 if found else 404, found[0] if found else {"error": "not found"})
                return self.reply(404, {"error": "no route"})

            def do_GET(self) -> None:  # noqa: N802
                self.handle_any("GET")

            def do_POST(self) -> None:  # noqa: N802
                self.handle_any("POST")

            def do_DELETE(self) -> None:  # noqa: N802
                self.handle_any("DELETE")

            def log_message(self, *args) -> None:
                pass

        self.server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        threading.Thread(target=self.server.serve_forever, daemon=True).start()
        self.url = f"http://127.0.0.1:{self.server.server_address[1]}"

    def close(self) -> None:
        self.server.shutdown()
        self.server.server_close()

    def add(self, name: str, tags: str = f"bmac {TAG} bmac-role-host", **fields) -> dict:
        check_uuid = str(uuid.uuid4())
        check = {"name": name, "slug": name.lower().replace(" ", "-"), "tags": tags, "desc": "",
                 "timeout": 60, "grace": 180, "uuid": check_uuid,
                 "ping_url": f"https://hc-ping.com/{check_uuid}", "status": "up",
                 "last_ping": "2026-10-23T16:07:07+00:00", "n_pings": 5, **fields}
        self.checks.append(check)
        return check

    def mutating_calls(self) -> list:
        return [call for call in self.calls if call[0] != "GET"]


def inventory(hosts=("mox1", "mox2"), registered=True, productions=None) -> dict:
    return {
        "cluster": "MyAppCloud",
        "hosts": [{"name": name, "nodeid": int(name[3:])} for name in hosts],
        "qdevice": {"host": "qdevice", "registered": registered},
        "host_tag": "tag:proxmox-host",
        "productions": productions if productions is not None else [
            {"name": "prod1", "state": "active", "primary": "prod1.example.com", "aliases": [],
             "probe": {"enabled": False, "path": None, "expected_status": None, "expected_text": None, "fqdns": []}},
        ],
    }


class HealthchecksClientTest(unittest.TestCase):
    def setUp(self) -> None:
        self.api = FakeHealthchecks()
        self.addCleanup(self.api.close)
        self.root = Path(tempfile.mkdtemp())
        self.addCleanup(shutil.rmtree, self.root, True)
        self.desired = self.root / "desired.json"
        self.desired.write_text(json.dumps(monitor_config.desired_checks(inventory(), {})))

    def hc(self, *arguments: str, key: str = API_KEY) -> subprocess.CompletedProcess:
        env = {**os.environ, "HEALTHCHECKS_API_KEY": key}
        env.pop("HEALTHCHECKS_API_URL", None)
        completed = subprocess.run(
            [sys.executable, str(HEALTHCHECKS), "--api-url", self.api.url, *arguments],
            capture_output=True, text=True, timeout=60, env=env,
        )
        self.assertNotIn(API_KEY, completed.stdout + completed.stderr)
        for check in self.api.checks:
            self.assertNotIn(check["uuid"], completed.stdout + completed.stderr)
        return completed

    def sync(self, *extra: str) -> tuple[subprocess.CompletedProcess, dict]:
        completed = self.hc("sync", "--desired", str(self.desired), "--tag", TAG,
                            "--out", str(self.root / "pings.json"), *extra)
        return completed, json.loads(completed.stdout) if completed.stdout.strip() else {}

    def test_create_then_idempotent_resync(self) -> None:
        completed, report = self.sync()
        self.assertEqual(completed.returncode, 0, completed.stderr)
        self.assertEqual(report["created"], ["MyAppCloud bmaccheck mox1", "MyAppCloud bmaccheck mox2",
                                             "MyAppCloud bmaccheck qdevice", "MyAppCloud bmaccheck prod1"])
        posts = [call for call in self.api.calls if call[0] == "POST"]
        self.assertEqual(len(posts), 4)
        body = posts[0][2]
        self.assertEqual(body["unique"], ["name"])
        self.assertEqual(body["channels"], "*")
        self.assertEqual(body["slug"], "myappcloud-mox1")
        self.assertEqual(body["tags"], f"bmac {TAG} bmac-role-host")
        self.assertIn("do not rename or edit", body["desc"])
        self.assertEqual((body["timeout"], body["grace"]), (60, 180))
        self.assertTrue(all(call[3] == API_KEY for call in self.api.calls), "key sent in X-Api-Key")
        pings = self.root / "pings.json"
        self.assertEqual(pings.stat().st_mode & 0o777, 0o600)
        self.assertEqual(set(json.loads(pings.read_text())["nodes"]), {"mox1", "mox2", "qdevice", "prod1"})
        self.assertEqual(report["budget"], {"used": 4, "max": 20})

        self.api.calls.clear()
        completed, report = self.sync()
        self.assertEqual(report["unchanged"], report["unchanged"][:4])
        self.assertEqual(len(report["unchanged"]), 4)
        self.assertEqual(self.api.mutating_calls(), [])
        self.assertIn("ping_url", json.loads(pings.read_text())["nodes"]["mox1"])

    def test_changed_settings_update_the_existing_check(self) -> None:
        self.sync()
        self.api.checks[0]["grace"] = 600
        self.api.calls.clear()
        _, report = self.sync()
        self.assertEqual(report["updated"], ["MyAppCloud bmaccheck mox1"])
        self.assertEqual(len(self.api.checks), 4)

    def test_obsolete_checks_are_deleted_only_with_the_flag(self) -> None:
        self.api.add("MyAppCloud bmaccheck mox3")
        self.api.add("Other bmaccheck mox1", tags="bmac bmac-cluster-other")
        _, report = self.sync()
        self.assertEqual(report["obsolete"], ["MyAppCloud bmaccheck mox3"])
        self.assertEqual(report["deleted"], [])
        self.assertEqual(len(self.api.checks), 6)
        _, report = self.sync("--delete-obsolete")
        self.assertEqual(report["deleted"], ["MyAppCloud bmaccheck mox3"])
        self.assertEqual(sorted(c["name"] for c in self.api.checks if "Other" in c["name"]),
                         ["Other bmaccheck mox1"], "other clusters' checks are never touched")

    def test_budget_counts_every_project_check_and_refuses_the_rest(self) -> None:
        for index in range(18):
            self.api.add(f"unrelated {index}", tags="")
        completed, report = self.sync()
        self.assertEqual(completed.returncode, 2)
        self.assertEqual(report["created"], ["MyAppCloud bmaccheck mox1", "MyAppCloud bmaccheck mox2"])
        self.assertEqual(report["refused"], ["MyAppCloud bmaccheck qdevice", "MyAppCloud bmaccheck prod1"])
        self.assertEqual(len(self.api.checks), 20)

    def test_dry_run_makes_no_mutating_calls_and_writes_no_ping_file(self) -> None:
        self.api.add("MyAppCloud bmaccheck mox9")
        completed, report = self.sync("--dry-run", "--delete-obsolete")
        self.assertEqual(completed.returncode, 0, completed.stderr)
        self.assertTrue(report["dry_run"])
        self.assertEqual(len(report["created"]), 4)
        self.assertEqual(report["obsolete"], ["MyAppCloud bmaccheck mox9"])
        self.assertEqual(report["deleted"], [])
        self.assertEqual(self.api.mutating_calls(), [])
        self.assertFalse((self.root / "pings.json").exists())

    def test_status_and_delete_name_checks_without_secrets(self) -> None:
        self.sync()
        completed = self.hc("status", "--tag", TAG)
        report = json.loads(completed.stdout)
        self.assertEqual(report["project_checks"], 4)
        self.assertNotIn("hc-ping.com", completed.stdout)
        self.assertEqual({c["status"] for c in report["checks"]}, {"new"})
        completed = self.hc("delete", "--tag", TAG, "--name", "MyAppCloud bmaccheck prod1", "--name", "nope")
        self.assertEqual(json.loads(completed.stdout), {"deleted": ["MyAppCloud bmaccheck prod1"], "missing": ["nope"]})
        self.assertEqual(len(self.api.checks), 3)

    def test_wait_ping_success_and_timeout(self) -> None:
        self.sync()
        completed = self.hc("wait-ping", "--tag", TAG, "--name", "MyAppCloud bmaccheck mox1",
                            "--since", "2026-10-23T00:00:00Z", "--timeout", "0.5", "--interval", "0.1")
        self.assertEqual(completed.returncode, 1)
        self.assertFalse(json.loads(completed.stdout)["MyAppCloud bmaccheck mox1"]["ok"])
        self.api.ping_on_list = True
        completed = self.hc("wait-ping", "--tag", TAG, "--name", "MyAppCloud bmaccheck mox1",
                            "--since", "2026-10-23T00:00:00Z", "--timeout", "5", "--interval", "0.1")
        self.assertEqual(completed.returncode, 0, completed.stdout)

    def test_bad_key_reports_the_api_error(self) -> None:
        completed = self.hc("status", "--tag", TAG, key="wrong")
        self.assertEqual(completed.returncode, 1)
        self.assertIn("HTTP 401: missing api key", completed.stderr)
        completed = subprocess.run([sys.executable, str(HEALTHCHECKS), "status", "--tag", TAG],
                                   capture_output=True, text=True, timeout=30,
                                   env={k: v for k, v in os.environ.items() if k != "HEALTHCHECKS_API_KEY"})
        self.assertEqual(completed.returncode, 2)
        self.assertIn("HEALTHCHECKS_API_KEY is not set", completed.stderr)


class MonitorConfigTest(unittest.TestCase):
    def test_hosts_expect_every_other_host_and_registered_qdevice(self) -> None:
        inv = inventory(hosts=("mox1", "mox2", "mox3"), registered=False)
        node = {"node": "mox2", "role": "host", "target": "mox2"}
        config = monitor_config.node_config(inv, node, "https://hc-ping.com/x", {})
        self.assertEqual(config["host"], {"expected_peers": ["mox1", "mox3"], "qdevice_expected": False})
        config = monitor_config.node_config(inventory(), node, "https://hc-ping.com/x", {})
        self.assertTrue(config["host"]["qdevice_expected"])

    def test_qdevice_expectations_for_odd_and_even_host_counts(self) -> None:
        node = {"node": "qdevice", "role": "qdevice", "target": "qdevice"}
        even = monitor_config.node_config(inventory(), node, "https://hc-ping.com/x", {})["qdevice"]
        self.assertEqual((even["qnetd_expected"], even["vote_needed"]), (True, True))
        self.assertEqual(even["hosts"][1], {"name": "mox2", "tailscale_hostname": "mox2", "nodeid": 2})
        odd = monitor_config.node_config(inventory(hosts=("mox1", "mox2", "mox3"), registered=False),
                                         node, "https://hc-ping.com/x", {})["qdevice"]
        self.assertEqual((odd["qnetd_expected"], odd["vote_needed"]), (False, False))

    def test_probes_from_records_with_and_without_monitoring(self) -> None:
        productions = [
            {"name": "prod1", "state": "active", "primary": "prod1.example.com", "aliases": ["www.example.com"],
             "probe": {"enabled": True, "path": "/api/v1/healthz", "expected_status": 200,
                       "expected_text": None, "fqdns": ["prod1.example.com", "www.example.com"]}},
            {"name": "prod2", "state": "active", "primary": "two.example.com", "aliases": [],
             "probe": {"enabled": False}},
            {"name": "prod3", "state": "provisioning", "primary": "three.example.com", "aliases": [],
             "probe": {"enabled": True, "path": "/", "expected_status": 200, "expected_text": None,
                       "fqdns": ["three.example.com"]}},
        ]
        inv = inventory(productions=productions)
        node = {"node": "qdevice", "role": "qdevice", "target": "qdevice"}
        probes = monitor_config.node_config(inv, node, "https://hc-ping.com/x", {})["qdevice"]["probes"]
        self.assertEqual([(p["vm"], p["fqdn"]) for p in probes],
                         [("prod1", "prod1.example.com"), ("prod1", "www.example.com")])
        self.assertEqual([n["node"] for n in monitor_config.nodes(inv)], ["mox1", "mox2", "qdevice", "prod1", "prod2"])

    def test_guest_config_and_check_names(self) -> None:
        config = monitor_config.node_config(inventory(), {"node": "prod1", "role": "guest", "target": "prod1"},
                                            "https://hc-ping.com/x", {"MONITOR_ALIVE_PERIOD_SECONDS": "120"})
        self.assertEqual(config["guest"], {"settings_file": "/etc/app-ha-monitor/guest.json"})
        self.assertEqual(config["intervals"]["alive"], 120)
        names = [c["name"] for c in monitor_config.desired_checks(inventory(), {})["checks"]]
        self.assertTrue(all(name.startswith("MyAppCloud bmaccheck ") for name in names))

    def test_rendered_configs_validate_with_the_agent(self) -> None:
        root = Path(tempfile.mkdtemp())
        self.addCleanup(shutil.rmtree, root, True)
        (root / "inventory.json").write_text(json.dumps(inventory()))
        (root / "pings.json").write_text(json.dumps({"nodes": {
            node: {"ping_url": f"https://hc-ping.com/{node}"} for node in ("mox1", "mox2", "qdevice", "prod1")}}))
        (root / "out").mkdir()
        completed = subprocess.run([sys.executable, str(MONITOR_CONFIG), "render", "--inventory",
                                    str(root / "inventory.json"), "--pings", str(root / "pings.json"),
                                    "--out-dir", str(root / "out")], capture_output=True, text=True, timeout=30)
        self.assertEqual(completed.returncode, 0, completed.stderr)
        for path in (root / "out").iterdir():
            self.assertEqual(path.stat().st_mode & 0o777, 0o600)
            completed = subprocess.run([sys.executable, str(AGENT), "--config", str(path), "validate-config"],
                                       capture_output=True, text=True, timeout=30)
            self.assertEqual(completed.returncode, 0, completed.stderr)


FAKE_SYSTEMCTL = """#!/usr/bin/env bash
root="${APP_HA_MONITOR_TEST_ROOT:?}"
printf '%s\\n' "$*" >>"$root/systemctl.log"
words=()
for word in "$@"; do [[ "$word" == --* ]] || words+=("$word"); done
case "${words[0]}" in
  is-active) [[ -f "$root/active" ]] ;;
  is-enabled) [[ -f "$root/enabled" ]] ;;
  restart|start) : >"$root/active" ;;
  stop) rm -f "$root/active" ;;
  enable) : >"$root/enabled"; [[ " $* " != *" --now "* ]] || : >"$root/active" ;;
  disable) rm -f "$root/enabled"; [[ " $* " != *" --now "* ]] || rm -f "$root/active" ;;
  *) exit 0 ;;
esac
"""

FAKE_TOOLS = ("smartctl", "zpool", "zfs", "corosync-cfgtool", "corosync-quorumtool", "postqueue",
              "tailscale", "ping")


class FakeNodes:
    """Per-node roots and a bin directory of fake system tools."""

    def __init__(self, root: Path) -> None:
        self.root = root
        self.bin = root / "bin"
        self.bin.mkdir()
        (self.bin / "systemctl").write_text(FAKE_SYSTEMCTL)
        for tool in FAKE_TOOLS:
            (self.bin / tool).write_text("#!/usr/bin/env bash\nexit 0\n")
        for tool in self.bin.iterdir():
            tool.chmod(0o755)

    def node(self, name: str) -> Path:
        path = self.root / "nodes" / name
        path.mkdir(parents=True, exist_ok=True)
        return path

    def env(self, node_root: Path) -> dict:
        return {**os.environ, "PATH": f"{self.bin}:{os.environ['PATH']}",
                "APP_HA_MONITOR_TEST_ROOT": str(node_root), "PYTHONDONTWRITEBYTECODE": "1"}


class NodeInstallTest(unittest.TestCase):
    def setUp(self) -> None:
        self.root = Path(tempfile.mkdtemp())
        self.addCleanup(shutil.rmtree, self.root, True)
        self.fakes = FakeNodes(self.root)
        self.node = self.fakes.node("prod1")
        self.stage = self.node / "run" / "app-ha-monitor-stage.abc"
        self.make_stage()

    def make_stage(self, role: str = "guest", config: dict | None = None) -> None:
        self.stage.mkdir(parents=True, exist_ok=True)
        node = {"node": "prod1", "role": role, "target": "prod1"} if role == "guest" else \
            {"node": "mox1", "role": "host", "target": "mox1"}
        value = config or monitor_config.node_config(inventory(), node, "https://hc-ping.com/x", {})
        (self.stage / "config.json").write_text(json.dumps(value))
        program = "/usr/local/lib/app-ha-monitor/app_ha_monitor.py" if role == "guest" else \
            "/usr/local/lib/app-ha-proxmox/lib/app_ha_monitor.py"
        unit = subprocess.run([sys.executable, str(AGENT), "print-unit", "--role", role, "--program", program],
                              capture_output=True, text=True, check=True).stdout
        (self.stage / "app-ha-monitor.service").write_text(unit)
        if role == "guest":
            shutil.copy(AGENT, self.stage / "app_ha_monitor.py")

    def install(self, *arguments: str) -> subprocess.CompletedProcess:
        return subprocess.run(["bash", str(NODE_INSTALL), *arguments], capture_output=True, text=True,
                              timeout=60, env=self.fakes.env(self.node))

    def test_fresh_install_then_config_only_update_then_rollback(self) -> None:
        program = "/usr/local/lib/app-ha-monitor/app_ha_monitor.py"
        completed = self.install("install", str(self.stage), "guest", program)
        self.assertEqual(completed.returncode, 0, completed.stderr)
        self.assertIn("__MONITOR_INSTALL__ restarted", completed.stdout)
        config = self.node / "etc/app-ha-monitor/config.json"
        self.assertEqual(config.stat().st_mode & 0o777, 0o600)
        guest = self.node / "etc/app-ha-monitor/guest.json"
        self.assertEqual(json.loads(guest.read_text()), {"schema_version": 1, "services": [], "cert_paths": []})
        self.assertTrue((self.node / program.lstrip("/")).is_file())
        self.assertIn("ExecCondition=", (self.node / "etc/systemd/system/app-ha-monitor.service").read_text())
        self.assertTrue((self.node / "active").exists() and (self.node / "enabled").exists())

        rollback = self.install("rollback", str(self.stage), program)
        self.assertEqual(rollback.returncode, 0, rollback.stderr)
        self.assertFalse(config.exists())
        self.assertFalse(guest.exists())
        self.assertFalse((self.node / program.lstrip("/")).exists())
        self.assertFalse((self.node / "etc/systemd/system/app-ha-monitor.service").exists())
        self.assertFalse((self.node / "active").exists())

    def test_second_install_with_same_program_only_rewrites_config_and_keeps_guest_json(self) -> None:
        program = "/usr/local/lib/app-ha-monitor/app_ha_monitor.py"
        self.install("install", str(self.stage), "guest", program)
        guest = self.node / "etc/app-ha-monitor/guest.json"
        guest.write_text(json.dumps({"schema_version": 1, "services": ["hello-todo.service"], "cert_paths": []}))
        shutil.rmtree(self.stage)
        self.make_stage()
        completed = self.install("install", str(self.stage), "guest", program)
        self.assertEqual(completed.returncode, 0, completed.stderr)
        self.assertIn("__MONITOR_INSTALL__ config-only", completed.stdout)
        self.assertIn("hello-todo.service", guest.read_text(), "an existing guest.json is never overwritten")
        rollback = self.install("rollback", str(self.stage), program)
        self.assertEqual(rollback.returncode, 0, rollback.stderr)
        self.assertTrue((self.node / "etc/app-ha-monitor/config.json").exists(), "restored, not removed")
        self.assertTrue((self.node / "active").exists())

    def test_host_install_needs_the_runtime_program_and_never_stages_one(self) -> None:
        shutil.rmtree(self.stage)
        self.make_stage("host")
        program = "/usr/local/lib/app-ha-proxmox/lib/app_ha_monitor.py"
        completed = self.install("install", str(self.stage), "host", program)
        self.assertNotEqual(completed.returncode, 0)
        self.assertIn("run update_cluster_runtime.sh first", completed.stderr)
        installed = self.node / program.lstrip("/")
        installed.parent.mkdir(parents=True)
        shutil.copy(AGENT, installed)
        completed = self.install("install", str(self.stage), "host", program)
        self.assertEqual(completed.returncode, 0, completed.stderr)
        self.assertNotIn("ExecCondition", (self.node / "etc/systemd/system/app-ha-monitor.service").read_text())

    def test_invalid_config_and_unsafe_paths_change_nothing(self) -> None:
        (self.stage / "config.json").write_text('{"schema_version": 1}')
        completed = self.install("install", str(self.stage), "guest", "/usr/local/lib/app-ha-monitor/app_ha_monitor.py")
        self.assertNotEqual(completed.returncode, 0)
        self.assertFalse((self.node / "etc/app-ha-monitor").exists())
        completed = self.install("install", "/etc", "guest", "/usr/local/lib/app-ha-monitor/app_ha_monitor.py")
        self.assertIn("unsafe staging directory", completed.stderr)
        completed = self.install("install", str(self.stage), "guest", "/usr/bin/evil.py")
        self.assertIn("unsafe program path", completed.stderr)

    def test_disable_and_installed(self) -> None:
        self.assertEqual(self.install("installed").stdout.strip(), "no")
        self.install("install", str(self.stage), "guest", "/usr/local/lib/app-ha-monitor/app_ha_monitor.py")
        self.assertEqual(self.install("installed").stdout.strip(), "yes")
        self.assertIn("__MONITOR_DISABLE__ stopped", self.install("disable").stdout)
        self.assertFalse((self.node / "active").exists())


# Sourced by the sync harness: in-process stand-ins for SSH, SCP, and the
# coordinator. Every node is a directory under FAKE_ROOT/nodes.
SYNC_HARNESS = r"""
set -Eeuo pipefail
source "$SCRIPTS/lib/config.sh"
source "$SCRIPTS/lib/ui_protocol.sh"
source "$SCRIPTS/lib/monitoring.sh"
load_proxmox_config --no-secrets >/dev/null

mox_ssh() {
  local node="$1"; shift
  printf 'mox_ssh %s %s\n' "$node" "$*" >>"$FAKE_ROOT/calls.log"
  case "$1" in
    pvesh) cat "$FAKE_ROOT/config-nodes.json" ;;
    grep) [[ -f "$FAKE_ROOT/registered" ]] ;;
    */cluster_registry.py) cat "$FAKE_ROOT/productions.json" ;;
    *) printf 'unexpected mox_ssh: %s\n' "$*" >&2; return 1 ;;
  esac
}

fake_translate() {
  local root="$1" arg
  shift
  FAKE_ARGV=()
  for arg; do
    if [[ "$arg" == /run/app-ha-monitor-stage* || "$arg" == /usr/local/lib/app-ha-* ]]; then
      FAKE_ARGV+=("$root$arg")
    else
      FAKE_ARGV+=("$arg")
    fi
  done
}

monitoring_exec() {
  local role="$1" target="$2" root
  shift 2
  root="$FAKE_ROOT/nodes/$target"
  printf 'exec %s %s %s\n' "$role" "$target" "$*" >>"$FAKE_ROOT/calls.log"
  [[ " ${FAKE_UNREACHABLE:-} " != *" $target "* ]] || return 255
  mkdir -p "$root/run"
  case "$1" in
    true) return 0 ;;
    mktemp) mktemp -d "$root/run/app-ha-monitor-stage.XXXXXX" | sed "s#^$root##"; return ;;
    bash)
      # bash STAGE/monitor_node_install.sh ACTION STAGE ROLE PROGRAM
      fake_translate "$root" "$2" "$3" "$4"
      APP_HA_MONITOR_TEST_ROOT="$root" bash "${FAKE_ARGV[@]}" "${@:5}"
      return ;;
    python3)
      fake_translate "$root" "$@"
      "${FAKE_ARGV[@]}"
      return ;;
  esac
  fake_translate "$root" "$@"
  "${FAKE_ARGV[@]}"
}

mon_copy() {
  local role="$1" target="$2" source="$3" destination="$4"
  [[ " ${FAKE_COPY_FAIL:-} " != *" $target "* ]] || return 1
  cp -- "$source" "$FAKE_ROOT/nodes/$target$destination"
}
"""


class MonitoringSyncTest(unittest.TestCase):
    def setUp(self) -> None:
        self.root = Path(tempfile.mkdtemp())
        self.addCleanup(shutil.rmtree, self.root, True)
        self.api = FakeHealthchecks()
        self.addCleanup(self.api.close)
        self.fakes = FakeNodes(self.root)
        config_dir = self.root / "config"
        config_dir.mkdir()
        (config_dir / "cluster.conf").write_text(CLUSTER_CONF)
        secrets = config_dir / "secrets.env"
        secrets.write_text(f"HEALTHCHECKS_API_KEY={API_KEY}\n")
        secrets.chmod(0o600)
        (self.root / "config-nodes.json").write_text(json.dumps([
            {"name": "mox1", "nodeid": "1"}, {"name": "mox2", "nodeid": "2"}]))
        (self.root / "registered").write_text("")
        (self.root / "productions.json").write_text(json.dumps([
            {"name": "prod1", "state": "active", "primary": "prod1.example.com", "aliases": [],
             "probe": {"enabled": True, "path": "/api/v1/healthz", "expected_status": 200,
                       "expected_text": None, "fqdns": ["prod1.example.com"]}},
            {"name": "prod2", "state": "active", "primary": "prod2.example.com", "aliases": [],
             "probe": {"enabled": False, "path": None, "expected_status": None, "expected_text": None, "fqdns": []}},
        ]))
        for host in ("mox1", "mox2"):
            program = self.fakes.node(host) / "usr/local/lib/app-ha-proxmox/lib/app_ha_monitor.py"
            program.parent.mkdir(parents=True)
            shutil.copy(AGENT, program)
        for node in ("qdevice", "prod1", "prod2"):
            self.fakes.node(node)
        self.env = {
            **os.environ, "PATH": f"{self.fakes.bin}:{os.environ['PATH']}",
            "APP_HA_CONFIG_TEST_MODE": "1", "APP_HA_CONFIG_DIR": str(config_dir),
            "HEALTHCHECKS_API_URL": self.api.url, "FAKE_ROOT": str(self.root),
            "SCRIPTS": str(SCRIPTS), "MONITORING_PING_TIMEOUT": "1", "MONITORING_PING_INTERVAL": "0.2",
            "PYTHONDONTWRITEBYTECODE": "1", "TMPDIR": str(self.root),
        }
        self.env.pop("APP_HA_MONITOR_TEST_ROOT", None)
        # cluster.conf cannot carry an http API URL; the harness passes it through.
        self.harness = SYNC_HARNESS.replace(
            "load_proxmox_config --no-secrets >/dev/null",
            "load_proxmox_config --no-secrets >/dev/null\nHEALTHCHECKS_API_URL=\"$FAKE_API_URL\"")
        self.env["FAKE_API_URL"] = self.api.url

    def run_bash(self, command: str, **env: str) -> subprocess.CompletedProcess:
        completed = subprocess.run(["bash", "-c", self.harness + "\n" + command], capture_output=True,
                                   text=True, timeout=180, env={**self.env, **env})
        output = completed.stdout + completed.stderr
        self.assertNotIn(API_KEY, output)
        self.assertNotIn("hc-ping.com", output)
        return completed

    def node_file(self, node: str, path: str) -> Path:
        return self.root / "nodes" / node / path.lstrip("/")

    def test_enabled_happy_path_installs_every_node(self) -> None:
        completed = self.run_bash("monitoring_sync mox1 --no-verify")
        self.assertEqual(completed.returncode, 0, completed.stdout + completed.stderr)
        self.assertEqual(sorted(c["name"] for c in self.api.checks), sorted(
            f"MyAppCloud bmaccheck {n}" for n in ("mox1", "mox2", "qdevice", "prod1", "prod2")))
        qdevice = json.loads(self.node_file("qdevice", "/etc/app-ha-monitor/config.json").read_text())
        self.assertEqual([p["fqdn"] for p in qdevice["qdevice"]["probes"]], ["prod1.example.com"])
        self.assertTrue(qdevice["qdevice"]["qnetd_expected"])
        mox1 = json.loads(self.node_file("mox1", "/etc/app-ha-monitor/config.json").read_text())
        self.assertEqual(mox1["host"]["expected_peers"], ["mox2"])
        self.assertTrue(mox1["ping_url"].startswith("https://hc-ping.com/"))
        self.assertTrue(self.node_file("prod2", "/etc/app-ha-monitor/guest.json").is_file())
        self.assertTrue(self.node_file("qdevice", "/usr/local/lib/app-ha-monitor/app_ha_monitor.py").is_file())
        self.assertFalse(list((self.root / "nodes" / "prod1" / "run").iterdir()), "stages are cleaned up")

    def test_dry_run_changes_nothing(self) -> None:
        completed = self.run_bash("monitoring_sync mox1 --dry-run")
        self.assertEqual(completed.returncode, 0, completed.stdout + completed.stderr)
        self.assertIn("create    MyAppCloud bmaccheck prod2", completed.stdout)
        self.assertEqual(self.api.mutating_calls(), [])
        self.assertFalse(self.node_file("mox1", "/etc/app-ha-monitor").exists())

    def test_unreachable_production_vm_stops_before_any_change(self) -> None:
        completed = self.run_bash("monitoring_sync mox1 --no-verify", FAKE_UNREACHABLE="prod2")
        self.assertNotEqual(completed.returncode, 0)
        self.assertIn("prod2 is not reachable over SSH", completed.stderr)
        self.assertEqual(self.api.mutating_calls(), [])
        self.assertFalse(self.node_file("mox1", "/etc/app-ha-monitor").exists())

    def test_failed_guest_install_rolls_back_that_phase_only(self) -> None:
        completed = self.run_bash("monitoring_sync mox1 --no-verify", FAKE_COPY_FAIL="prod2")
        self.assertNotEqual(completed.returncode, 0)
        self.assertIn("Monitoring was not updated on prod2", completed.stderr)
        self.assertFalse(self.node_file("prod1", "/etc/app-ha-monitor/config.json").exists(), "rolled back")
        self.assertFalse(self.node_file("prod1", "/usr/local/lib/app-ha-monitor/app_ha_monitor.py").exists())
        self.assertTrue(self.node_file("mox1", "/etc/app-ha-monitor/config.json").exists(), "earlier phases stay")
        self.assertTrue(self.node_file("qdevice", "/etc/app-ha-monitor/config.json").exists())
        self.assertEqual(len(self.api.checks), 5, "checks created by a failed run are kept")

    def test_removed_node_check_is_deleted_and_other_obsolete_checks_kept(self) -> None:
        self.api.add("MyAppCloud bmaccheck prod3", tags=f"bmac {TAG} bmac-role-guest")
        self.api.add("MyAppCloud bmaccheck mox3")
        completed = self.run_bash("monitoring_sync mox1 --no-verify --delete-node mox3")
        self.assertEqual(completed.returncode, 0, completed.stdout + completed.stderr)
        names = {c["name"] for c in self.api.checks}
        self.assertNotIn("MyAppCloud bmaccheck mox3", names)
        self.assertIn("MyAppCloud bmaccheck prod3", names)

    def test_missing_first_ping_fails_verification(self) -> None:
        completed = self.run_bash("monitoring_sync mox1")
        self.assertNotEqual(completed.returncode, 0)
        self.assertIn("received no ping", completed.stderr)
        self.assertIn("NO PING", completed.stdout)

    def test_membership_sync_failure_is_a_warning_with_a_next_step(self) -> None:
        completed = self.run_bash("monitoring_membership_sync mox1 --no-verify; echo rc=$?",
                                  FAKE_UNREACHABLE="qdevice")
        self.assertIn("rc=0", completed.stdout)
        self.assertIn("monitoring could not be updated", completed.stderr)

    def test_disabled_monitoring_does_nothing_without_installed_agents(self) -> None:
        completed = self.run_bash("MONITORING_ENABLED=false; monitoring_membership_sync mox1; "
                                  "monitoring_disable_everywhere mox1; echo done")
        self.assertIn("done", completed.stdout)
        self.assertEqual(self.api.calls, [])
        calls = (self.root / "calls.log").read_text() if (self.root / "calls.log").exists() else ""
        self.assertNotIn("exec qdevice", calls)
        self.assertNotIn("exec guest", calls)


if __name__ == "__main__":
    unittest.main()
