#!/usr/bin/env python3

# Copyright (c) 2026 BEENTHERE VENTURES, INC.
# SPDX-License-Identifier: GPL-3.0-only

"""Fake-SSH tests for the monitoring scripts operators run: the Production
page's Monitored services tile, the QDevice page's Health-check probes tile,
show_monitoring_state.sh, and send_test_alert.sh."""

from __future__ import annotations

import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import unittest


TESTS = Path(__file__).resolve().parent
SCRIPTS = TESTS.parent
sys.path.insert(0, str(TESTS))
from test_monitoring_workstation import API_KEY, CLUSTER_CONF, FakeHealthchecks  # noqa: E402
from ui_test_driver import run_json, scripted  # noqa: E402

MANAGE_VM = SCRIPTS / "user_callable" / "guests" / "prod" / "manage_prod_vm_monitoring.sh"
MANAGE_PROBES = SCRIPTS / "user_callable" / "qdevice" / "manage_qdevice_probes.sh"
SHOW_STATE = SCRIPTS / "user_callable" / "diagnostics" / "show_monitoring_state.sh"
SEND_TEST = SCRIPTS / "user_callable" / "diagnostics" / "send_test_alert.sh"

# Every ssh call is logged. Proxmox hosts answer from fixtures; the QDevice
# and production VMs run the command here, below FAKE/nodes/<name>, with the
# monitor agent replaced by fake_agent.py.
FAKE_SSH = r'''#!/usr/bin/env python3
import json, os, re, subprocess, sys
fake = os.environ["FAKE"]
args = sys.argv[1:]
with_value = {"-o", "-p", "-i", "-F", "-J", "-l", "-E", "-b", "-c", "-m", "-L", "-R", "-D", "-W", "-S"}
index = 0
while index < len(args) and args[index].startswith("-"):
    index += 2 if args[index] in with_value else 1
destination, command = args[index], " ".join(args[index + 1:])
host = destination.split("@")[-1].split(".")[0]
with open(os.path.join(fake, "ssh.log"), "a") as log:
    log.write(f"{host}\t{command}\n")
if host in os.environ.get("FAKE_UNREACHABLE", "").split():
    sys.exit(255)
if host.startswith("mox"):
    if command.strip() == "true":
        sys.exit(0)
    if "cluster_registry.py" in command:
        words = command.split()
        sub = words[words.index("--state-dir") + 2]
        if sub == "monitoring-probes":
            print(open(os.path.join(fake, "probes.json")).read()); sys.exit(0)
        if sub == "get":
            path = os.path.join(fake, f"record-{words[words.index('get') + 1]}.json")
            if not os.path.exists(path):
                print("ERROR: resource does not exist", file=sys.stderr); sys.exit(1)
            print(open(path).read()); sys.exit(0)
        if sub == "update":
            with open(os.path.join(fake, "registry-updates.log"), "a") as log:
                log.write(command + "\n")
            record = json.load(open(os.path.join(fake, f"record-{words[words.index('update') + 1]}.json")))
            if f"--expected-revision {record['revision']}" not in command:
                print("ERROR: revision conflict", file=sys.stderr); sys.exit(1)
            print("{}"); sys.exit(0)
        if sub == "list":
            print(open(os.path.join(fake, "productions-list.json")).read()); sys.exit(0)
    if "pvesh" in command:
        print(json.dumps([{"name": "mox1", "nodeid": "1"}, {"name": "mox2", "nodeid": "2"}])); sys.exit(0)
    if command.startswith("grep"):
        sys.exit(0)
    if "app_ha_monitor.py" in command:
        node_root = os.path.join(fake, "nodes", host)
        command = re.sub(r"\S*app_ha_monitor\.py", os.path.join(fake, "fake_agent.py"), command)
        sys.exit(subprocess.run(["bash", "-c", command], env={**os.environ, "NODE": host, "NODE_ROOT": node_root}).returncode)
    print(f"unexpected host command: {command}", file=sys.stderr); sys.exit(1)
node_root = os.path.join(fake, "nodes", host)
os.makedirs(node_root, exist_ok=True)
command = command.replace("/etc/app-ha-monitor", node_root + "/etc/app-ha-monitor")
command = re.sub(r"\S*app_ha_monitor\.py", os.path.join(fake, "fake_agent.py"), command)
sys.exit(subprocess.run(["bash", "-c", command], env={**os.environ, "NODE": host, "NODE_ROOT": node_root}).returncode)
'''

# Stands in for app_ha_monitor.py on a node; answers from FAKE/agent-*.json.
FAKE_AGENT = r'''#!/usr/bin/env python3
import json, os, sys
fake, node = os.environ["FAKE"], os.environ["NODE"]
args = [a for a in sys.argv[1:] if a != "-I"]
with open(os.path.join(fake, "agent.log"), "a") as log:
    log.write(node + " " + " ".join(args) + "\n")
command = args[0]
if command == "status":
    path = os.path.join(fake, f"agent-status-{node}.json")
    print(open(path).read() if os.path.exists(path) else json.dumps(
        {"node": node, "state_present": True, "problems": {}, "last_heartbeat_ok": "2026-10-23T16:00:00Z",
         "last_results": {}, "check_errors": {}}))
elif command == "probe":
    fqdn = args[args.index("--fqdn") + 1]
    ok = fqdn not in os.environ.get("FAKE_FAILING_FQDNS", "").split()
    print(json.dumps({"fqdn": fqdn, "ok": ok, "message": f"https://{fqdn} returned HTTP {200 if ok else 404}"}))
    sys.exit(0 if ok else 1)
elif command == "test-pulse":
    print(json.dumps({"pulse": "abcd1234", "priority": args[args.index("--priority") + 1], "cleared": True}))
elif command == "check":
    print(json.dumps({"node": node, "results": [], "errors": []}))
'''

FAKE_SYSTEMCTL = r'''#!/usr/bin/env bash
case "$1" in
  is-active) [[ "$2" == broken.service ]] && { echo failed; exit 3; }; echo active ;;
  cat) [[ "$2" == *.service && "$2" != missing.service ]] ;;
  list-unit-files) printf 'hello-todo.service enabled enabled\nssh.service enabled enabled\ngetty@.service enabled enabled\n' ;;
  *) exit 0 ;;
esac
'''


def production_record(name: str = "prod1", revision: int = 3, probe: dict | None = None) -> dict:
    record = {"name": name, "kind": "production", "state": "active", "revision": revision,
              "domains": {"primary": f"{name}.example.com", "aliases": [f"www.{name}.example.com"],
                          "staging_base": f"stage.{name}.example.com"}}
    if probe:
        record["monitoring"] = {"probe": probe}
    return record


class MonitoringScriptTest(unittest.TestCase):
    def setUp(self) -> None:
        self.root = Path(tempfile.mkdtemp())
        self.addCleanup(shutil.rmtree, self.root, True)
        self.fake = self.root / "fake"
        (self.fake / "bin").mkdir(parents=True)
        for name, text in (("ssh", FAKE_SSH), ("systemctl", FAKE_SYSTEMCTL)):
            (self.fake / "bin" / name).write_text(text)
            (self.fake / "bin" / name).chmod(0o755)
        (self.fake / "fake_agent.py").write_text(FAKE_AGENT)
        (self.fake / "fake_agent.py").chmod(0o755)
        home = self.root / "home"
        (home / ".ssh").mkdir(parents=True)
        (home / ".ssh" / "known_hosts").write_text("")
        (home / ".ssh" / "known_hosts").chmod(0o600)
        self.config = self.root / "config"
        self.config.mkdir()
        self.write_cluster_conf(monitoring=True)
        secrets = self.config / "secrets.env"
        secrets.write_text(f"HEALTHCHECKS_API_KEY={API_KEY}\n")
        secrets.chmod(0o600)
        self.cert = self.root / "cert.pem"
        subprocess.run(["openssl", "req", "-x509", "-newkey", "rsa:2048", "-nodes", "-subj", "/CN=prod1.example.com",
                        "-days", "30", "-keyout", str(self.root / "key.pem"), "-out", str(self.cert)],
                       check=True, capture_output=True)
        (self.fake / "record-prod1.json").write_text(json.dumps(production_record()))
        (self.fake / "probes.json").write_text(json.dumps([
            {"name": "prod1", "state": "active", "primary": "prod1.example.com",
             "aliases": ["www.prod1.example.com"],
             "probe": {"enabled": False, "path": None, "expected_status": None, "expected_text": None, "fqdns": []}},
        ]))
        self.env = {
            "PATH": f"{self.fake / 'bin'}:/usr/local/bin:/usr/bin:/bin", "HOME": str(home),
            "APP_HA_CONFIG_TEST_MODE": "1", "APP_HA_CONFIG_DIR": str(self.config), "FAKE": str(self.fake),
            "TMPDIR": str(self.root), "PYTHONDONTWRITEBYTECODE": "1",
        }

    def write_cluster_conf(self, monitoring: bool) -> None:
        text = CLUSTER_CONF.replace("MONITORING_ENABLED=true", f"MONITORING_ENABLED={str(monitoring).lower()}")
        (self.config / "cluster.conf").write_text(text)

    def run_script(self, script: Path, *arguments: str, **env: str) -> subprocess.CompletedProcess:
        return subprocess.run([str(script), *arguments], capture_output=True, text=True, timeout=120,
                              env={**self.env, **env}, stdin=subprocess.DEVNULL)

    def ssh_log(self) -> list[tuple[str, str]]:
        path = self.fake / "ssh.log"
        if not path.exists():
            return []
        return [tuple(line.split("\t", 1)) for line in path.read_text().splitlines()]

    def guest_settings(self) -> Path:
        return self.fake / "nodes" / "prod1" / "etc" / "app-ha-monitor" / "guest.json"


class ManageProdVmMonitoringTest(MonitoringScriptTest):
    def test_show_reads_the_vm_only_and_writes_nothing(self) -> None:
        run = run_json([str(MANAGE_VM), "prod1", "--show"], scripted(), env=self.env, timeout=120)
        self.assertEqual(run.completed["status"], "success", run.describe())
        data = run.of("result")[-1]["data"]
        self.assertEqual(data["settings"], {"schema_version": 1, "services": [], "cert_paths": []})
        self.assertFalse(data["exists"])
        self.assertIn("hello-todo.service", data["enabled_services"])
        self.assertNotIn("getty@.service", data["enabled_services"], "template units are not suggested")
        self.assertFalse(self.guest_settings().exists())
        self.assertEqual({host for host, _ in self.ssh_log()}, {"prod1"}, "never the registry or the QDevice")

    def test_add_and_remove_with_flags(self) -> None:
        completed = self.run_script(MANAGE_VM, "prod1", "--add-service", "hello-todo.service",
                                    "--add-cert", str(self.cert), "--yes")
        self.assertEqual(completed.returncode, 0, completed.stdout + completed.stderr)
        settings = json.loads(self.guest_settings().read_text())
        self.assertEqual(settings, {"schema_version": 1, "services": ["hello-todo.service"],
                                    "cert_paths": [str(self.cert)]})
        self.assertEqual(self.guest_settings().stat().st_mode & 0o777, 0o600)
        self.assertIn("hello-todo.service", completed.stdout)
        completed = self.run_script(MANAGE_VM, "prod1", "--remove-service", "hello-todo.service", "--yes")
        self.assertEqual(completed.returncode, 0, completed.stderr)
        self.assertEqual(json.loads(self.guest_settings().read_text())["services"], [])
        self.assertEqual({host for host, _ in self.ssh_log()}, {"prod1"})

    def test_invalid_unit_and_certificate_are_rejected(self) -> None:
        completed = self.run_script(MANAGE_VM, "prod1", "--add-service", "missing.service", "--yes")
        self.assertNotEqual(completed.returncode, 0)
        self.assertIn("no such unit on this VM", completed.stdout + completed.stderr)
        completed = self.run_script(MANAGE_VM, "prod1", "--add-service", "bad unit;rm", "--yes")
        self.assertNotEqual(completed.returncode, 0)
        completed = self.run_script(MANAGE_VM, "prod1", "--add-cert", str(self.root / "key.pem"), "--yes")
        self.assertNotEqual(completed.returncode, 0)
        self.assertIn("not a readable PEM certificate", completed.stdout + completed.stderr)
        self.assertFalse(self.guest_settings().exists())

    def test_json_form_reports_field_errors_then_saves(self) -> None:
        def form(request):
            if request.get("validation_error"):
                self.assertIn("services", request["validation_error"]["field_errors"])
                return {"values": {"services": "hello-todo.service", "cert_paths": str(self.cert)}}
            self.assertEqual(request["type"], "input_group")
            return {"values": {"services": "missing.service", "cert_paths": ""}}

        run = run_json([str(MANAGE_VM), "prod1"], scripted(form, form, {"confirmed": True}),
                       env=self.env, timeout=120)
        self.assertEqual(run.completed["status"], "success", run.describe())
        self.assertEqual(json.loads(self.guest_settings().read_text())["services"], ["hello-todo.service"])
        self.assertTrue(run.of("plan"))


class ManageQdeviceProbesTest(MonitoringScriptTest):
    def setUp(self) -> None:
        super().setUp()
        # Without monitoring the save does not also sync every node.
        self.write_cluster_conf(monitoring=False)

    def updates(self) -> list[str]:
        path = self.fake / "registry-updates.log"
        return path.read_text().splitlines() if path.exists() else []

    def test_show_lists_probes_and_the_latest_result(self) -> None:
        (self.fake / "probes.json").write_text(json.dumps([{
            "name": "prod1", "state": "active", "primary": "prod1.example.com", "aliases": [],
            "probe": {"enabled": True, "path": "/api/v1/healthz", "expected_status": 200, "expected_text": None,
                      "fqdns": ["prod1.example.com"]}}]))
        (self.fake / "agent-status-qdevice.json").write_text(json.dumps({"last_results": {
            "ingress:prod1.example.com": {"ok": False, "message": "returned HTTP 503", "at": "2026-10-23T16:00:00Z"}}}))
        completed = self.run_script(MANAGE_PROBES, "--show")
        self.assertEqual(completed.returncode, 0, completed.stderr)
        self.assertIn("GET /api/v1/healthz expecting HTTP 200", completed.stdout)
        self.assertIn("prod1.example.com: FAILING: returned HTTP 503", completed.stdout)
        self.assertEqual(self.updates(), [])

    def test_enable_with_defaults_runs_the_trial_probe_and_saves_with_the_revision(self) -> None:
        completed = self.run_script(MANAGE_PROBES, "--vm", "prod1", "--enable", "--yes")
        self.assertEqual(completed.returncode, 0, completed.stdout + completed.stderr)
        self.assertIn("ok: https://prod1.example.com returned HTTP 200", completed.stdout)
        [update] = self.updates()
        self.assertIn("--expected-revision 3", update)
        probe = json.loads(update.split("--monitoring-json ", 1)[1].replace("\\", ""))["probe"]
        self.assertEqual(probe, {"enabled": True, "path": "/api/v1/healthz", "expected_status": 200,
                                 "expected_text": None, "fqdns": ["prod1.example.com"]})

    def test_explicit_settings_and_alias_fqdn(self) -> None:
        completed = self.run_script(MANAGE_PROBES, "prod1", "--enable", "--path", "/api/v1/info",
                                    "--status", "200", "--text", "max_todos", "--fqdn", "www.prod1.example.com",
                                    "--yes")
        self.assertEqual(completed.returncode, 0, completed.stdout + completed.stderr)
        self.assertIn("www.prod1.example.com", self.updates()[0])
        self.assertIn("max_todos", self.updates()[0])

    def test_fqdn_that_is_not_the_vms_is_rejected(self) -> None:
        completed = self.run_script(MANAGE_PROBES, "--vm", "prod1", "--enable", "--fqdn", "evil.example.org", "--yes")
        self.assertNotEqual(completed.returncode, 0)
        self.assertIn("is not one of this VM's domains", completed.stderr)
        self.assertEqual(self.updates(), [])

    def test_bad_path_and_status_are_rejected(self) -> None:
        for arguments in (("--path", "api/no-slash"), ("--status", "42")):
            with self.subTest(arguments=arguments):
                completed = self.run_script(MANAGE_PROBES, "--vm", "prod1", "--enable", *arguments, "--yes")
                self.assertNotEqual(completed.returncode, 0)
        self.assertEqual(self.updates(), [])

    def test_failed_trial_probe_saves_only_when_asked(self) -> None:
        env = {"FAKE_FAILING_FQDNS": "prod1.example.com"}
        completed = self.run_script(MANAGE_PROBES, "--vm", "prod1", "--enable", "--yes", **env)
        self.assertNotEqual(completed.returncode, 0)
        self.assertIn("--save-if-probe-fails", completed.stderr)
        self.assertEqual(self.updates(), [])
        completed = self.run_script(MANAGE_PROBES, "--vm", "prod1", "--enable", "--yes", "--save-if-probe-fails", **env)
        self.assertEqual(completed.returncode, 0, completed.stderr)
        self.assertEqual(len(self.updates()), 1)

    def test_json_save_anyway_declined_saves_nothing(self) -> None:
        def form(request):
            self.assertEqual(request["type"], "input_group")
            return {"values": {"enabled": True, "fqdns": "prod1.example.com", "path": "/api/v1/healthz",
                               "status": 200, "text": ""}}

        run = run_json([str(MANAGE_PROBES), "prod1"], scripted(form, {"confirmed": False}),
                       env={**self.env, "FAKE_FAILING_FQDNS": "prod1.example.com"}, timeout=120)
        self.assertNotEqual(run.completed.get("status"), "failure", run.describe())
        self.assertEqual(self.updates(), [])
        run = run_json([str(MANAGE_PROBES), "prod1"], scripted(form, {"confirmed": True}, {"confirmed": True}),
                       env={**self.env, "FAKE_FAILING_FQDNS": "prod1.example.com"}, timeout=120)
        self.assertEqual(run.completed["status"], "success", run.describe())
        self.assertEqual(len(self.updates()), 1)

    def test_registry_revision_conflict_fails_clearly(self) -> None:
        (self.fake / "record-prod1.json").write_text(json.dumps(production_record(revision=3)))
        record = json.loads((self.fake / "record-prod1.json").read_text())
        script = f"""
import json, os
path = os.path.join(os.environ["FAKE"], "record-prod1.json")
record = json.load(open(path)); record["revision"] = 4
json.dump(record, open(path, "w"))
"""
        # The record changes between the read and the save.
        (self.fake / "fake_agent.py").write_text(
            FAKE_AGENT.replace('elif command == "probe":', 'elif command == "probe":\n    exec(' + repr(script) + ')'))
        completed = self.run_script(MANAGE_PROBES, "--vm", "prod1", "--enable", "--yes")
        self.assertNotEqual(completed.returncode, 0)
        self.assertIn("may have changed meanwhile", completed.stderr)
        self.assertEqual(record["revision"], 3)


class ShowMonitoringStateTest(MonitoringScriptTest):
    def setUp(self) -> None:
        super().setUp()
        self.api = FakeHealthchecks()
        self.addCleanup(self.api.close)
        self.env["BMAC_TEST_HEALTHCHECKS_API_URL"] = self.api.url
        for node in ("mox1", "mox2", "qdevice", "prod1"):
            self.api.add(f"MyAppCloud bmaccheck {node}")

    def test_disabled_monitoring_says_so(self) -> None:
        self.write_cluster_conf(monitoring=False)
        completed = self.run_script(SHOW_STATE)
        self.assertEqual(completed.returncode, 0)
        self.assertIn("Monitoring is disabled", completed.stdout)
        self.assertEqual(self.ssh_log(), [])
        self.assertEqual(self.api.calls, [])

    def test_everything_up(self) -> None:
        completed = self.run_script(SHOW_STATE)
        self.assertEqual(completed.returncode, 0, completed.stdout + completed.stderr)
        self.assertIn("OK: every node's check is up", completed.stdout)
        self.assertIn("Check budget: 4 of 20", completed.stdout)
        self.assertNotIn(API_KEY, completed.stdout + completed.stderr)
        self.assertNotIn("hc-ping.com", completed.stdout)

    def test_problems_obsolete_missing_and_unreachable(self) -> None:
        self.api.checks = [c for c in self.api.checks if not c["name"].endswith("mox2")]
        self.api.add("MyAppCloud bmaccheck mox3")
        self.api.checks[0]["status"] = "down"
        (self.fake / "agent-status-prod1.json").write_text(json.dumps({
            "state_present": True, "last_heartbeat_ok": "2026-10-23T16:00:00Z", "check_errors": {},
            "problems": {"service:hello-todo.service": {"priority": "p1", "message": "monitored service hello-todo.service is failed",
                                                        "first_seen": "2026-10-23T15:00:00Z"}}}))
        run = run_json([str(SHOW_STATE)], scripted(), env={**self.env, "FAKE_UNREACHABLE": "qdevice"}, timeout=120)
        data = run.of("result")[-1]["data"]
        problems = "\n".join(data["problems"])
        self.assertIn("mox1's check is down", problems)
        self.assertIn("mox2 has no Healthchecks check", problems)
        self.assertIn("qdevice's agent could not be read", problems)
        self.assertIn("prod1: [p1] service:hello-todo.service", problems)
        self.assertEqual(data["obsolete_checks"], ["MyAppCloud bmaccheck mox3"])
        self.assertEqual(data["verdict"], "attention")

    def test_reset_smart_baseline_asks_then_runs_on_the_host(self) -> None:
        run = run_json([str(SHOW_STATE), "--reset-smart-baseline", "mox2:SERIALA0000000000"],
                       scripted({"confirmed": True}), env=self.env, timeout=120)
        self.assertEqual(run.completed["status"], "success", run.describe())
        self.assertIn("mox2 reset-smart-baseline SERIALA0000000000", (self.fake / "agent.log").read_text())
        completed = self.run_script(SHOW_STATE, "--reset-smart-baseline", "nothost:x")
        self.assertEqual(completed.returncode, 2)


class SendTestAlertTest(MonitoringScriptTest):
    def test_sends_from_the_chosen_node_and_explains_the_emails(self) -> None:
        completed = self.run_script(SEND_TEST, "--node", "prod1", "--priority", "p1")
        self.assertEqual(completed.returncode, 0, completed.stdout + completed.stderr)
        self.assertIn("prod1 test-pulse --priority p1", (self.fake / "agent.log").read_text())
        self.assertIn('"DOWN | MyAppCloud bmaccheck prod1"', completed.stdout)
        self.assertIn("bmacevent bmacp1", completed.stdout)
        self.assertIn("filter F1 deletes it", completed.stdout)

    def test_qdevice_and_unknown_node_and_disabled(self) -> None:
        completed = self.run_script(SEND_TEST, "--node", "qdevice", "--priority", "p2")
        self.assertEqual(completed.returncode, 0, completed.stderr)
        self.assertIn("qdevice test-pulse --priority p2", (self.fake / "agent.log").read_text())
        completed = self.run_script(SEND_TEST, "--node", "mox9", "--priority", "p2")
        self.assertNotEqual(completed.returncode, 0)
        self.assertIn("is not a monitored node", completed.stderr)
        self.write_cluster_conf(monitoring=False)
        completed = self.run_script(SEND_TEST, "--node", "prod1", "--priority", "p2")
        self.assertNotEqual(completed.returncode, 0)
        self.assertIn("Monitoring is disabled", completed.stderr)

    def test_json_mode_asks_for_node_and_priority(self) -> None:
        run = run_json([str(SEND_TEST)], scripted({"values": {"choice": "mox2"}}, {"values": {"choice": "p2"}}),
                       env=self.env, timeout=120)
        self.assertEqual(run.completed["status"], "success", run.describe())
        self.assertEqual(run.of("result")[-1]["data"]["node"], "mox2")


class MembershipWorkflowTest(unittest.TestCase):
    """Membership workflows keep monitoring in step, without failing on it."""

    def text(self, *parts: str) -> str:
        return (SCRIPTS / "user_callable").joinpath(*parts).read_text(encoding="utf-8")

    def test_every_membership_workflow_syncs_monitoring_after_its_main_job(self) -> None:
        cases = {
            ("hosts", "add_proxmox_host.sh"): ('monitoring_membership_sync "$HOST_ID"', "enable_admin_ssh_for_target\n"),
            ("qdevice", "add_qdevice.sh"): ('monitoring_membership_sync "$HM_COORDINATOR"', "qd_add \"$NODE_COUNT\""),
            ("qdevice", "remove_qdevice.sh"): ('monitoring_membership_sync "$HM_COORDINATOR"', "__QDEVICE_PURGE_RESULT__"),
            ("guests", "prod", "add_prod_vm.sh"): ('monitoring_membership_sync "$COORDINATOR"', "  print_completion\n"),
            ("guests", "prod", "remove_prod_vm.sh"):
                ('monitoring_membership_sync "$COORDINATOR" --delete-node "$RESOURCE_NAME"',
                 'registry_cmd release "$RESOURCE_NAME"'),
        }
        for parts, (call, after) in cases.items():
            with self.subTest(script=parts[-1]):
                text = self.text(*parts)
                self.assertIn(call, text)
                self.assertLess(text.index(after), text.index(call))
        remove_host = self.text("hosts", "remove_proxmox_host.sh")
        self.assertEqual(remove_host.count('monitoring_membership_sync "$HM_COORDINATOR" --delete-node "$TARGET_HOST"'), 2)
        self.assertEqual(remove_host.count("show_monitoring_plan"), 3, "both removal plans list the check deletion")

    def test_removing_the_qdevice_never_deletes_its_check(self) -> None:
        text = self.text("qdevice", "remove_qdevice.sh")
        self.assertNotIn("--delete-node", text)
        self.assertIn("keeps its monitoring check and agent", text)

    def test_adding_workflows_stop_early_when_the_qdevice_is_unreachable(self) -> None:
        for parts in (("hosts", "add_proxmox_host.sh"), ("guests", "prod", "add_prod_vm.sh")):
            with self.subTest(script=parts[-1]):
                self.assertIn("monitoring_require_qdevice ||", self.text(*parts))

    def test_owner_and_placement_changes_do_not_touch_monitoring(self) -> None:
        for name in ("change_prod_vm_owner.sh", "change_prod_vm_placement.sh"):
            self.assertNotIn("monitoring", self.text("guests", "prod", name))

    def test_add_prod_vm_asks_no_monitoring_questions(self) -> None:
        text = self.text("guests", "prod", "add_prod_vm.sh")
        for prompt in ("manage_prod_vm_monitoring", "manage_qdevice_probes", "--add-service", "expected_status"):
            self.assertNotIn(prompt, text)

    def test_deploying_the_example_app_watches_its_service_and_certificate(self) -> None:
        text = self.text("guests", "prod", "deploy_hello_app_to_prod.sh")
        self.assertIn('("services", "hello-todo.service"), ("cert_paths", "/etc/hello-todo/cert.pem")', text)
        self.assertLess(text.index("  deploy_to_guest\n"), text.index("  add_app_to_monitoring\n"))
        self.assertIn("monitoring_enabled || return 0", text)


if __name__ == "__main__":
    unittest.main()
