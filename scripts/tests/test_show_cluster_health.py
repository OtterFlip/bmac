#!/usr/bin/env python3

# Copyright (c) 2026 BEENTHERE VENTURES, INC.
# SPDX-License-Identifier: GPL-3.0-only

"""Fake-SSH tests for the read-only cluster hardware and network health check."""

from __future__ import annotations

import copy
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest


DIAGNOSTICS_DIR = Path(__file__).resolve().parent
SCRIPT = DIAGNOSTICS_DIR / "show_cluster_health.sh"
sys.path.insert(0, str(DIAGNOSTICS_DIR.parent / "lib"))
from ui_test_driver import run_json, scripted  # noqa: E402

# The workstation's ssh. It answers the cluster discovery and QDevice
# commands itself and runs the real health collector locally against the fake
# host commands below. Any other remote command fails the test.
FAKE_SSH = r'''#!/usr/bin/env python3
import json
import os
import re
import shlex
import subprocess
import sys

args = sys.argv[1:]
position = 0
while args[position].startswith("-"):
    position += 2 if args[position] == "-o" else 1
host = args[position].split("@", 1)[1]
words = shlex.split(" ".join(args[position + 1:]))
payload = "" if sys.stdin.isatty() else sys.stdin.read()
scenario = json.load(open(os.environ["FAKE_SCENARIO"], encoding="utf-8"))
with open(os.environ["FAKE_SSH_CALLS"], "a", encoding="utf-8") as stream:
    stream.write(json.dumps([host, words]) + "\n")


def on_host(target, argv, stdin=None):
    environment = dict(
        os.environ,
        FAKE_HOST=target,
        PATH=os.environ["FAKE_REMOTE_BIN"] + ":" + os.environ["PATH"],
    )
    done = subprocess.run(argv, input=stdin, env=environment, text=True,
                          capture_output=True)
    sys.stdout.write(done.stdout)
    sys.stderr.write(done.stderr)
    raise SystemExit(done.returncode)


if host == "qdevice":
    qdevice = scenario["qdevice"]
    if not qdevice.get("reachable", True):
        raise SystemExit(255)
    if words == ["true"]:
        raise SystemExit(0)
    if words == ["bash", "-s"]:
        print("SSH ok")
        print(f"TAILSCALE {qdevice.get('tailscale', '100.64.0.9')}")
        print("UPTIME up 9 weeks")
        print(f"QNETD_ACTIVE {qdevice.get('active', 'active')}")
        print("LISTEN yes")
        print("QNETD_TOOL_BEGIN")
        if qdevice.get("serves", True):
            print('Cluster "MyAppCloud":')
        print("QNETD_TOOL_END")
        raise SystemExit(0)
    raise SystemExit(f"unexpected qdevice command: {words}")

state = scenario["hosts"].get(host)
if state is None or not state.get("up", True) or not state.get("tailscale_ssh", True):
    print(f"ssh: connect to host {host}: Connection timed out", file=sys.stderr)
    raise SystemExit(255)
if words == ["true"]:
    raise SystemExit(0)
if words[:2] == ["bash", "-c"] and "pvesh get /nodes" in words[2]:
    print(json.dumps([
        {"node": name, "status": "online" if value.get("up", True) else "offline"}
        for name, value in scenario["hosts"].items()
    ]))
    raise SystemExit(0)
if words == ["pvesh", "get", "/cluster/config/nodes", "--output-format", "json"]:
    print(json.dumps([
        {"name": name, "nodeid": str(index)}
        for index, name in enumerate(scenario["hosts"], start=1)
    ]))
    raise SystemExit(0)
if words == ["python3", "-"] and "def run(" in payload:
    on_host(host, words, payload)
if words == ["bash", "-s"] and "exec grep -Eq" in payload:
    raise SystemExit(0 if scenario.get("registered") else 1)
if words == ["bash", "-s"] and "exec awk" in payload:
    print(scenario["registered"])
    raise SystemExit(0)
if words[:1] == ["ssh"] and words[-2:] == ["bash", "-s"]:
    target = re.fullmatch(r"root@(mox\d+)\.pve\.internal", words[-3]).group(1)
    if not scenario["hosts"][target].get("up", True):
        raise SystemExit(255)
    if "exec python3 -c" not in payload:
        raise SystemExit(f"unexpected relayed command: {payload}")
    on_host(target, ["bash", "-s"], payload)
raise SystemExit(f"unexpected remote command on {host}: {words}")
'''

# One program installed as every host command the collector runs.
FAKE_HOST_COMMAND = r'''#!/usr/bin/env python3
import json
import os
import sys

command = os.path.basename(sys.argv[0])
args = sys.argv[1:]
scenario = json.load(open(os.environ["FAKE_SCENARIO"], encoding="utf-8"))
host = os.environ["FAKE_HOST"]
hosts = scenario["hosts"]
state = hosts[host]
names = list(hosts)
up = [name for name in names if hosts[name].get("up", True)]

if command == "uptime" and args == ["-p"]:
    print("up 2 weeks, 3 days")
elif command == "systemctl" and args[0] == "is-active":
    value = state.get("units", {}).get(args[1], "active")
    print(value)
    raise SystemExit(0 if value == "active" else 3)
elif command == "pvecm" and args == ["status"]:
    registered = bool(scenario.get("registered"))
    voting = registered and state.get("qdevice_voting", True)
    expected = len(names) + (1 if registered else 0)
    total = len(up) + (1 if voting else 0)
    print("Quorum information\n------------------")
    print(f"Quorate:          {'Yes' if total * 2 > expected else 'No'}\n")
    print("Votequorum information\n----------------------")
    print(f"Expected votes:   {expected}")
    print(f"Total votes:      {total}")
    print(f"Flags:            Quorate{' Qdevice' if voting else ''}\n")
    print("Membership information\n----------------------")
    print("    Nodeid      Votes    Qdevice Name")
    for name in up:
        flags = "A,V,NMW" if voting else "NA,NV,NMW"
        print(f"0x{names.index(name) + 1:08x}          1    {flags} {name}")
    if registered:
        print(f"0x00000000          {1 if voting else 0}            Qdevice")
elif command == "corosync-cfgtool" and args == ["-n"]:
    print(f"Local node ID {names.index(host) + 1}, transport knet")
    for name in names:
        if name == host:
            continue
        reachable = hosts[name].get("up", True)
        links = state.get("links", {}).get(name, [True, True]) if reachable else [False, False]
        print(f"nodeid: {names.index(name) + 1} {'reachable' if any(links) else 'unreachable'}")
        for link, connected in enumerate(links):
            print(f"   LINK: {link} udp (a->b) enabled {'connected' if connected else 'disconnected'} mtu: 1397")
        print()
elif command == "zpool" and args == ["list", "-H", "-o", "name"]:
    print("\n".join(state.get("pools", {"rpool": None})))
elif command == "zpool" and args[:3] == ["status", "-P", "-p"]:
    print(state.get("pools", {}).get(args[3]) or HEALTHY_POOL)
elif command == "zpool" and args[:2] == ["status", "-x"]:
    text = state.get("pools", {}).get(args[2]) or HEALTHY_POOL
    print(f"pool '{args[2]}' is healthy" if " state: ONLINE" in text else text)
elif command == "zfs" and args == ["list", "-H", "-p", "-o", "used,available", "rpool"]:
    used, available = state.get("rpool_space", [400 * 2**30, 1400 * 2**30])
    print(f"{used}\t{available}")
elif command == "lsblk":
    print(json.dumps({"blockdevices": [
        {"name": f"/dev/nvme{index}n1", "kname": f"/dev/nvme{index}n1", "type": "disk",
         "serial": f"{host.upper()}-S{index}", "model": "Fake NVMe 2TB",
         "children": [{"name": f"/dev/nvme{index}n1p3", "kname": f"/dev/nvme{index}n1p3",
                       "type": "part", "children": [
                           {"name": f"/dev/mapper/rpool-{letter}", "kname": f"/dev/dm-{index}",
                            "type": "crypt"}]}]}
        for index, letter in enumerate("ab")
    ] + [{"name": "/dev/zd0", "kname": "/dev/zd0", "type": "disk"}]}))
elif command == "smartctl" and args[0] == "-H":
    if args[1] == "/dev/zd0":
        raise SystemExit("smartctl must not query zvols")
    verdict = state.get("smart", {}).get(args[1], "PASSED")
    print(f"SMART overall-health self-assessment test result: {verdict}")
    raise SystemExit(8 if verdict.startswith("FAILED") else 0)
else:
    raise SystemExit(f"unexpected host command: {command} {args}")
'''

HEALTHY_POOL = """  pool: rpool
 state: ONLINE
  scan: scrub repaired 0B in 00:01:02 with 0 errors on Sun Sep 13 00:25:03 2026
config:

\tNAME                     STATE     READ WRITE CKSUM
\trpool                    ONLINE       0     0     0
\t  mirror-0               ONLINE       0     0     0
\t    /dev/mapper/rpool-a  ONLINE       0     0     0
\t    /dev/mapper/rpool-b  ONLINE       0     0     0

errors: No known data errors
"""

DEGRADED_POOL = """  pool: rpool
 state: DEGRADED
status: One or more devices are faulted in response to persistent errors.
\tSufficient replicas exist for the pool to continue functioning in a
\tdegraded state.
action: Replace the faulted device, or use 'zpool clear' to mark the device
\trepaired.
  scan: scrub repaired 0B in 00:01:02 with 0 errors on Sun Sep 13 00:25:03 2026
config:

\tNAME                     STATE     READ WRITE CKSUM
\trpool                    DEGRADED     0     0     0
\t  mirror-0               DEGRADED     0     0     0
\t    /dev/mapper/rpool-a  ONLINE       0     0     0
\t    /dev/mapper/rpool-b  FAULTED     12     3     0  too many errors

errors: No known data errors
"""


class ShowClusterHealthTest(unittest.TestCase):
    def setUp(self) -> None:
        self.temporary = tempfile.TemporaryDirectory()
        root = Path(self.temporary.name)
        env_dir, bin_dir, remote_bin = root / "env", root / "bin", root / "remote-bin"
        for path in (env_dir, bin_dir, remote_bin):
            path.mkdir()
        self.calls = root / "ssh-calls.jsonl"
        self.scenario_path = root / "scenario.json"
        known_hosts = root / "known_hosts"
        known_hosts.write_text("", encoding="utf-8")
        known_hosts.chmod(0o600)
        (env_dir / "cluster.conf").write_text(
            "\n".join(
                (
                    "PROXMOX_CLUSTER_NAME=MyAppCloud",
                    "PROXMOX_QDEVICE_HOST=qdevice",
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
                    "CLUSTER_STATE_DIR=/etc/pve/priv/app-ha",
                    "",
                )
            ),
            encoding="utf-8",
        )
        fake_ssh = bin_dir / "ssh"
        fake_ssh.write_text(FAKE_SSH, encoding="utf-8")
        fake_ssh.chmod(0o755)
        host_command = remote_bin / "fake-host-command"
        host_command.write_text(
            FAKE_HOST_COMMAND.replace(
                "import sys\n", f"import sys\n\nHEALTHY_POOL = {HEALTHY_POOL!r}\n", 1
            ),
            encoding="utf-8",
        )
        host_command.chmod(0o755)
        for name in ("uptime", "systemctl", "pvecm", "corosync-cfgtool", "zpool", "zfs",
                     "lsblk", "smartctl"):
            (remote_bin / name).symlink_to(host_command)

        self.scenario = {
            "hosts": {"mox1": {}, "mox2": {}},
            "registered": "100.64.0.9",
            "qdevice": {},
        }
        self.environment = os.environ.copy()
        self.environment.update(
            {
                "APP_HA_CONFIG_TEST_MODE": "1",
                "APP_HA_ENV_DIR": str(env_dir),
                "PROXMOX_SSH_KNOWN_HOSTS_FILE": str(known_hosts),
                "FAKE_SCENARIO": str(self.scenario_path),
                "FAKE_SSH_CALLS": str(self.calls),
                "FAKE_REMOTE_BIN": str(remote_bin),
                "PATH": f"{bin_dir}:{os.environ['PATH']}",
            }
        )

    def tearDown(self) -> None:
        self.temporary.cleanup()

    def run_script(self, *arguments: str, expected: int) -> str:
        self.scenario_path.write_text(json.dumps(self.scenario), encoding="utf-8")
        completed = subprocess.run(
            [str(SCRIPT), *arguments],
            env=self.environment,
            text=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            check=False,
        )
        self.assertEqual(
            completed.returncode,
            expected,
            msg=f"stdout:\n{completed.stdout}\nstderr:\n{completed.stderr}",
        )
        return completed.stdout

    def test_healthy_cluster_is_ok(self) -> None:
        output = self.run_script(expected=0)
        self.assertRegex(output, r"mox1\s+online\s+ok\s+yes\s+3/3\s+active\s+active\s+active, voting yes")
        self.assertRegex(output, r"mox1\s+-\s+L0 ok/L1 ok")
        self.assertRegex(output, r"mox2\s+L0 ok/L1 ok\s+-")
        self.assertIn("serving cluster MyAppCloud: yes", output)
        self.assertRegex(output, r"/dev/mapper/rpool-b\s+ONLINE\s+0\s+0\s+0\s+nvme1n1 MOX2-S1")
        self.assertRegex(output, r"nvme0n1\s+MOX1-S0\s+Fake NVMe 2TB\s+PASSED")
        self.assertNotIn("zd0", output)
        self.assertRegex(output, r"mox1\s+400\.0 GiB\s+1\.37 TiB\s+1\.76 TiB\s+77\.8%\s+ok")
        self.assertIn("OK: all 2 hosts", output)

    def test_json_mode_reports_phases_verdict_and_next_steps(self) -> None:
        self.scenario["hosts"]["mox2"] = {"rpool_space": [1700 * 2**30, 100 * 2**30]}
        self.scenario_path.write_text(json.dumps(self.scenario), encoding="utf-8")
        run = run_json([str(SCRIPT)], scripted(), env=self.environment, timeout=60)
        self.assertEqual(run.malformed, [], run.describe())
        self.assertEqual(run.completed["status"], "failed", run.describe())
        labels = [event["label"] for event in run.of("phase") if event["status"] == "running"]
        self.assertEqual(labels[0], "Find the cluster")
        self.assertIn("Collect health from mox2", labels)
        self.assertIn("Drive SMART health", labels)
        self.assertEqual(labels[-1], "Verdict")
        running = {e["id"] for e in run.of("phase") if e["status"] == "running"}
        complete = {e["id"] for e in run.of("phase") if e["status"] == "complete"}
        self.assertEqual(running, complete)
        result = run.of("result")[-1]["data"]
        self.assertEqual(result["verdict"], "attention")
        self.assertIn("mox2 needs more storage added", result["problems"][0])
        steps = run.of("next_step")
        self.assertIn({"host": "mox2"}, [step.get("args") for step in steps
                                         if step.get("workflow") == "add_new_disk_vdev"])
        self.assertEqual(steps[-1]["workflow"], "show_cluster_state")
        self.assertIn("ATTENTION: 1 problem found:", run.log_text)

    def test_low_rpool_free_space_asks_for_storage(self) -> None:
        self.scenario["hosts"]["mox2"] = {"rpool_space": [1700 * 2**30, 100 * 2**30]}
        self.scenario["hosts"]["mox3"] = {"rpool_space": [900 * 2**30, 100 * 2**30]}
        self.scenario["registered"] = None
        output = self.run_script(expected=1)
        self.assertRegex(output, r"mox2\s+1\.66 TiB\s+100\.0 GiB\s+1\.76 TiB\s+5\.6%\s+LOW: add storage")
        self.assertRegex(output, r"mox3\s+900\.0 GiB\s+100\.0 GiB\s+1000\.0 GiB\s+10\.0%\s+ok")
        self.assertIn(
            "- mox2: rpool has only 100.0 GiB free (5.6% of 1.76 TiB usable, below 10%); "
            "mox2 needs more storage added",
            output,
        )
        self.assertIn("ATTENTION: 1 problem found:", output)
        self.assertIn("hosts/add_new_disk_vdev.sh --host mox2", output)
        self.assertNotIn("show_proxmox_host_state.sh --host mox2", output)

    def test_unreadable_rpool_space_is_a_problem(self) -> None:
        self.scenario["hosts"]["mox2"] = {"rpool_space": ["-", "-"]}
        output = self.run_script(expected=1)
        self.assertRegex(output, r"mox2\s+\?\s+\?\s+\?\s+\?\s+unknown")
        self.assertIn(
            "- could not read rpool free space on mox2: unexpected zfs list output '-\\t-'",
            output,
        )

    def test_failed_drive_names_host_member_and_serial(self) -> None:
        self.scenario["hosts"]["mox2"] = {
            "pools": {"rpool": DEGRADED_POOL},
            "smart": {"/dev/nvme1n1": "FAILED!"},
        }
        output = self.run_script(expected=1)
        self.assertIn("Pool rpool: DEGRADED", output)
        self.assertIn("- mox2: pool rpool is DEGRADED", output)
        self.assertIn(
            "- mox2: rpool member /dev/mapper/rpool-b (nvme1n1 MOX2-S1) is FAULTED (too many errors)",
            output,
        )
        self.assertIn("read/write/checksum errors 12/3/0", output)
        self.assertIn("- mox2: drive nvme1n1 MOX2-S1 SMART health is FAILED", output)
        self.assertNotIn("mirror-0 is DEGRADED", output)
        self.assertNotIn("- mox1:", output)
        self.assertIn("show_proxmox_host_state.sh --host mox2", output)

    def test_one_down_link_is_named(self) -> None:
        self.scenario["hosts"]["mox1"] = {"links": {"mox2": [True, False]}}
        output = self.run_script(expected=1)
        self.assertRegex(output, r"mox1\s+-\s+L0 ok/L1 DOWN")
        self.assertIn("- mox1 cannot reach mox2 over corosync link1 (Tailscale)", output)
        self.assertNotIn("mox2 cannot reach", output)

    def test_down_host_is_reported_once_per_symptom(self) -> None:
        self.scenario["hosts"]["mox3"] = {"up": False}
        self.scenario["registered"] = None
        output = self.run_script(expected=1)
        self.assertIn("- mox3 is offline in the Proxmox cluster", output)
        self.assertIn("- mox3 cannot be reached over SSH from this workstation or from mox1", output)
        self.assertIn("- no inspected host can reach mox3 over any corosync link", output)
        self.assertRegex(output, r"mox3\s+no data\s+no data\s+-")
        self.assertNotIn("Collecting from qdevice", output)

    def test_host_without_workstation_access_is_inspected_through_a_member(self) -> None:
        self.scenario["hosts"]["mox2"] = {"tailscale_ssh": False}
        output = self.run_script(expected=1)
        self.assertIn("FAILED (inspected via mox1)", output)
        self.assertIn("this workstation cannot SSH to mox2", output)
        self.assertRegex(output, r"nvme0n1\s+MOX2-S0\s+Fake NVMe 2TB\s+PASSED")
        self.assertIn("ATTENTION: 1 problem found:", output)

    def test_qdevice_failures(self) -> None:
        self.scenario["qdevice"] = {"reachable": False}
        self.scenario["hosts"]["mox2"] = {
            "qdevice_voting": False,
            "units": {"corosync-qdevice.service": "failed"},
        }
        output = self.run_script(expected=1)
        self.assertIn("- ssh root@qdevice fails from this workstation", output)
        self.assertIn("- mox2 does not see the QDevice alive and voting", output)
        self.assertIn("- corosync-qdevice.service is failed on mox2", output)
        self.assertIn("Voting on hosts:    mox1 yes, mox2 no", output)
        self.assertIn("show_qdevice_state.sh explains", output)

        self.scenario = copy.deepcopy(self.scenario)
        self.scenario["hosts"]["mox2"] = {}
        self.scenario["qdevice"] = {"serves": False, "active": "inactive"}
        output = self.run_script(expected=1)
        self.assertIn("- corosync-qnetd is inactive on qdevice", output)
        self.assertIn("- corosync-qnetd on qdevice does not list cluster MyAppCloud", output)

    def test_only_read_only_commands_run(self) -> None:
        self.scenario["hosts"]["mox2"] = {"tailscale_ssh": False}
        self.run_script(expected=1)
        calls = [json.loads(line) for line in self.calls.read_text(encoding="utf-8").splitlines()]
        self.assertTrue(calls)
        for host, words in calls:
            self.assertTrue(
                words in (["true"], ["bash", "-s"],
                          ["pvesh", "get", "/cluster/config/nodes", "--output-format", "json"])
                or words == ["python3", "-"]
                or words[:2] == ["bash", "-c"]
                or words[:1] == ["ssh"],
                (host, words),
            )

    def test_usage_errors(self) -> None:
        self.assertIn("Usage:", self.run_script("--help", expected=0))
        self.run_script("mox1", expected=2)
        self.assertFalse(self.calls.exists())


if __name__ == "__main__":
    unittest.main()
