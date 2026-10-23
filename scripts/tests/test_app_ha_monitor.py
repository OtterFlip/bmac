#!/usr/bin/env python3

# Copyright (c) 2026 BEENTHERE VENTURES, INC.
# SPDX-License-Identifier: GPL-3.0-only

"""Tests for the monitor agent, from output captured on a live test cluster.

The fixtures follow docs/MONITORING_AND_ALERTING.md section 19.1, anonymized:
serials SERIALA0000000000 / SERIALB0000000000, Tailscale 100.64.0.x.
"""

from __future__ import annotations

import contextlib
import http.server
import io
import importlib.util
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import threading
import unittest
from unittest import mock


AGENT_PATH = Path(__file__).resolve().parent.parent / "host_runtime" / "app_ha_monitor.py"
SPEC = importlib.util.spec_from_file_location("app_ha_monitor", AGENT_PATH)
assert SPEC and SPEC.loader
monitor = importlib.util.module_from_spec(SPEC)
sys.modules["app_ha_monitor"] = monitor
SPEC.loader.exec_module(monitor)

T0 = 1792771213.0  # 2026-10-23, the captured postqueue arrival time

QUORUM_THREE_HOSTS = """Quorum information
------------------
Date:             Fri Oct 23 09:17:38 2026
Quorum provider:  corosync_votequorum
Nodes:            3
Node ID:          1
Ring ID:          1.3a
Quorate:          Yes

Votequorum information
----------------------
Expected votes:   3
Highest expected: 3
Total votes:      3
Quorum:           2
Flags:            Quorate

Membership information
----------------------
    Nodeid      Votes Name
         1          1 mox1 (local)
         2          1 mox2
         3          1 mox3
"""

QUORUM_QDEVICE_HEALTHY = """Quorum information
------------------
Date:             Fri Oct 23 09:53:15 2026
Quorum provider:  corosync_votequorum
Nodes:            2
Node ID:          1
Ring ID:          1.3e
Quorate:          Yes

Votequorum information
----------------------
Expected votes:   3
Highest expected: 3
Total votes:      3
Quorum:           2
Flags:            Quorate Qdevice

Membership information
----------------------
    Nodeid      Votes    Qdevice Name
         1          1    A,V,NMW mox1 (local)
         2          1    A,V,NMW mox2
         0          1            Qdevice
"""

QUORUM_QDEVICE_LOST = """Quorum information
------------------
Quorate:          Yes

Votequorum information
----------------------
Expected votes:   3
Highest expected: 3
Total votes:      2
Quorum:           2
Flags:            Quorate Qdevice

Membership information
----------------------
    Nodeid      Votes    Qdevice Name
         1          1   A,NV,NMW mox1 (local)
         2          1   A,NV,NMW mox2
         0          0            Qdevice (votes 1)
"""

QUORUM_NOT_QUORATE = """Quorum information
------------------
Quorate:          No

Votequorum information
----------------------
Expected votes:   3
Highest expected: 3
Total votes:      1
Quorum:           2 Activity blocked
Flags:

Membership information
----------------------
    Nodeid      Votes Name
         1          1 mox1 (local)
"""

QDEVICE_TOOL = """Qdevice information
-------------------
Model:\t\t\tNet
Node ID:\t\t1
Configuration version:\t7
Membership node list:\t1, 2

Qdevice-net information
----------------------
Cluster name:\t\tMyAppCloud
QNetd host:\t\t100.64.0.2:5403
State:\t\t\t{state}
"""

COROSYNC_CONF = """nodelist {
  node {
    name: mox1
    nodeid: 1
    quorum_votes: 1
    ring0_addr: 10.213.0.11
  }
  node {
    name: mox2
    nodeid: 2
    quorum_votes: 1
    ring0_addr: 10.213.0.12
  }
  node {
    name: mox3
    nodeid: 3
    quorum_votes: 1
    ring0_addr: 10.213.0.13
  }
}
"""

LINKS_ALL_UP = """Local node ID 1, transport knet
nodeid: 2 reachable
   LINK: 0 udp (10.213.0.11->10.213.0.12) enabled connected mtu: 1397
   LINK: 1 udp (100.64.0.11->100.64.0.12) enabled connected mtu: 1173

nodeid: 3 reachable
   LINK: 0 udp (10.213.0.11->10.213.0.13) enabled connected mtu: 1397
   LINK: 1 udp (100.64.0.11->100.64.0.13) enabled connected mtu: 1173
"""

QNETD_HEALTHY = """Cluster "MyAppCloud":
    Algorithm:\t\tFifty-Fifty split (KAP Tie-breaker)
    Tie-breaker:\tNode with lowest node ID
    Node ID 1:
        Client address:\t\t::ffff:100.64.0.11:58120
        Configuration version:\t7
        Configured node list:\t1, 2
        Membership node list:\t1, 2
        Vote:\t\t\tACK (ACK)
    Node ID 2:
        Client address:\t\t::ffff:100.64.0.12:45528
        Configuration version:\t7
        Configured node list:\t1, 2
        Membership node list:\t1, 2
        Vote:\t\t\tNo change (ACK)
"""

POSTQUEUE_DEFERRED = json.dumps({
    "queue_name": "deferred", "queue_id": "A0B19119BF", "arrival_time": 1792771213,
    "message_size": 489, "forced_expire": False, "sender": "root@mox1.pve.internal",
    "recipients": [{"address": "admin@example.com", "delay_reason": "deferred transport"}],
}) + "\n"

ZPOOL_HEALTHY_STATUS = """  pool: rpool
 state: ONLINE
config:

\tNAME                                                                   STATE     READ WRITE CKSUM
\trpool                                                                  ONLINE       0     0     0
\t  mirror-0                                                             ONLINE       0     0     0
\t    /dev/disk/by-id/nvme-INTEL_SSDPE2KX020T8_SERIALA0000000000-part3  ONLINE       0     0     0
\t    /dev/disk/by-id/nvme-INTEL_SSDPE2KX020T7_SERIALB0000000000-part3  ONLINE       0     0     0

errors: No known data errors
"""

# Not captured on the cluster; the shape OpenZFS prints for a faulted member.
ZPOOL_DEGRADED_STATUS = """  pool: rpool
 state: DEGRADED
status: One or more devices are faulted in response to persistent errors.
\tSufficient replicas exist for the pool to continue functioning in a
\tdegraded state.
action: Replace the faulted device, or use 'zpool clear' to mark the device
\trepaired.
config:

\tNAME                                                                   STATE     READ WRITE CKSUM
\trpool                                                                  DEGRADED     0     0     0
\t  mirror-0                                                             DEGRADED     0     0     0
\t    /dev/disk/by-id/nvme-INTEL_SSDPE2KX020T8_SERIALA0000000000-part3  ONLINE       0     0     0
\t    /dev/disk/by-id/nvme-INTEL_SSDPE2KX020T7_SERIALB0000000000-part3  FAULTED     12     0     0  too many errors

errors: No known data errors
"""

SMART_SCAN = json.dumps({
    "smartctl": {"argv": ["smartctl", "--scan", "-j"], "exit_status": 0},
    "devices": [
        {"name": "/dev/nvme0", "info_name": "/dev/nvme0", "type": "nvme", "protocol": "NVMe"},
        {"name": "/dev/nvme1", "info_name": "/dev/nvme1", "type": "nvme", "protocol": "NVMe"},
    ],
})


def nvme_report(serial: str = "SERIALA0000000000", **log_values) -> str:
    log_ = {
        "critical_warning": 0, "temperature": 30, "available_spare": 99,
        "available_spare_threshold": 10, "percentage_used": 3,
        "data_units_written": 413489848, "power_on_hours": 24589,
        "unsafe_shutdowns": 26, "media_errors": 0, "num_err_log_entries": 0,
    }
    passed = log_values.pop("passed", True)
    log_.update(log_values)
    return json.dumps({
        "smartctl": {"exit_status": 0},
        "device": {"name": "/dev/nvme0", "type": "nvme", "protocol": "NVMe"},
        "model_name": "INTEL SSDPE2KX020T8", "serial_number": serial,
        "firmware_version": "VDV10184",
        "smart_status": {"passed": passed, "nvme": {"value": 0}},
        "nvme_smart_health_information_log": log_,
    })


def ata_report(serial: str = "ATASERIAL0001", reallocated: int = 0, pending: int = 0,
               uncorrectable: int = 0, passed: bool = True) -> str:
    def row(identifier: int, name: str, raw: int) -> dict:
        return {"id": identifier, "name": name, "value": 100, "worst": 100, "thresh": 10,
                "raw": {"value": raw, "string": str(raw)}}

    return json.dumps({
        "smartctl": {"exit_status": 0},
        "device": {"name": "/dev/sda", "type": "sat", "protocol": "ATA"},
        "model_name": "Example SSD 1TB", "serial_number": serial,
        "smart_status": {"passed": passed},
        "ata_smart_attributes": {"revision": 16, "table": [
            row(5, "Reallocated_Sector_Ct", reallocated), row(9, "Power_On_Hours", 1000),
            row(197, "Current_Pending_Sector", pending), row(198, "Offline_Uncorrectable", uncorrectable),
        ]},
    })


TAILSCALE_STATUS = {
    "Self": {"HostName": "qdevice", "Tags": ["tag:proxmox-qdevice"]},
    "Peer": {
        "nodekey:1": {"HostName": "mox1", "TailscaleIPs": ["100.64.0.11", "fd7a:115c:a1e0::11"],
                      "Tags": ["tag:proxmox-host"], "LastSeen": "0001-01-01T00:00:00Z", "Online": True},
        "nodekey:2": {"HostName": "mox2", "TailscaleIPs": ["100.64.0.12", "fd7a:115c:a1e0::12"],
                      "Tags": ["tag:proxmox-host"], "LastSeen": "0001-01-01T00:00:00Z", "Online": True},
        "nodekey:3": {"HostName": "mox3", "TailscaleIPs": ["100.64.0.13", "fd7a:115c:a1e0::13"],
                      "Tags": ["tag:proxmox-host"], "LastSeen": "2026-10-23T15:20:00.1Z", "Online": False},
        "nodekey:4": {"HostName": "onyx", "TailscaleIPs": ["100.64.0.50"], "Online": True},
    },
}

DF_PROD1 = """Filesystem     Type          1-blocks        Used     Available Capacity Mounted on
tmpfs          tmpfs      15096426496     1445888   15094980608       1% /run
/dev/sda2      ext4     1079988703232 12111208448 1012941602816       2% /
efivarfs       efivarfs        262044       62668        194256      25% /sys/firmware/efi/efivars
/dev/sda1      vfat        1124995072     6606848    1118388224       1% /boot/efi
none           tmpfs          1048576        8192       1040384       1% /run/credentials/hello-todo.service
"""


class FakeRunner:
    """Answers commands from a table keyed by argv; records every call."""

    def __init__(self, table: dict[tuple[str, ...], monitor.Result] | None = None) -> None:
        self.table = dict(table or {})
        self.calls: list[tuple[str, ...]] = []

    def set(self, argv: tuple[str, ...], out: str = "", rc: int = 0, err: str = "") -> None:
        self.table[argv] = monitor.Result(rc, out, err)

    def __call__(self, argv, timeout=30, stdin=None) -> monitor.Result:
        key = tuple(argv)
        self.calls.append(key)
        if key in self.table:
            return self.table[key]
        return monitor.Result(127, "", f"{argv[0]} is not installed (fake)")


def host_config(**host) -> dict:
    return {
        "schema_version": 1, "cluster": "MyAppCloud", "node": "mox1", "role": "host",
        "ping_url": "https://hc-ping.com/00000000-0000-0000-0000-000000000000",
        "intervals": dict(monitor.DEFAULT_INTERVALS),
        "thresholds": dict(monitor.DEFAULT_THRESHOLDS),
        "host": {"expected_peers": ["mox2", "mox3"], "qdevice_expected": False, **host},
    }


def qdevice_config(**section) -> dict:
    return {
        "schema_version": 1, "cluster": "MyAppCloud", "node": "qdevice", "role": "qdevice",
        "ping_url": "https://hc-ping.com/00000000-0000-0000-0000-000000000000",
        "intervals": dict(monitor.DEFAULT_INTERVALS), "thresholds": dict(monitor.DEFAULT_THRESHOLDS),
        "qdevice": {
            "hosts": [{"name": "mox1", "tailscale_hostname": "mox1", "nodeid": 1},
                      {"name": "mox2", "tailscale_hostname": "mox2", "nodeid": 2}],
            "host_tag": "tag:proxmox-host", "qnetd_expected": True, "vote_needed": True,
            "probes": [], **section,
        },
    }


def by_key(findings) -> dict:
    return {finding.key: finding for finding in findings}


def context(config, runner, **options):
    files = options.pop("files", {"/etc/corosync/corosync.conf": COROSYNC_CONF})

    def read_text(path):
        if str(path) in files:
            return files[str(path)]
        raise FileNotFoundError(str(path))

    return monitor.Context(config, runner=runner, now=lambda: T0, read_text=read_text, **options)


class ParserTest(unittest.TestCase):
    def test_quorum_three_hosts_without_qdevice_column(self) -> None:
        parsed = monitor.parse_quorum(QUORUM_THREE_HOSTS)
        self.assertTrue(parsed["quorate"])
        self.assertEqual((parsed["expected_votes"], parsed["total_votes"], parsed["quorum"]), (3, 3, 2))
        self.assertFalse(parsed["has_qdevice_column"])
        self.assertEqual([(m["name"], m["local"], m["flags"]) for m in parsed["members"]],
                         [("mox1", True, []), ("mox2", False, []), ("mox3", False, [])])

    def test_quorum_with_qdevice_healthy_and_lost(self) -> None:
        healthy = monitor.parse_quorum(QUORUM_QDEVICE_HEALTHY)
        self.assertTrue(healthy["has_qdevice_column"])
        self.assertEqual([(m["name"], m["votes"], m["flags"], m["qdevice"]) for m in healthy["members"]], [
            ("mox1", 1, ["A", "V", "NMW"], False), ("mox2", 1, ["A", "V", "NMW"], False),
            ("Qdevice", 1, [], True),
        ])
        lost = monitor.parse_quorum(QUORUM_QDEVICE_LOST)
        self.assertEqual(lost["total_votes"], 2)
        self.assertEqual(lost["flags"], "Quorate Qdevice")
        self.assertEqual([(m["name"], m["votes"], m["flags"]) for m in lost["members"]], [
            ("mox1", 1, ["A", "NV", "NMW"]), ("mox2", 1, ["A", "NV", "NMW"]), ("Qdevice (votes 1)", 0, []),
        ])

    def test_links_and_corosync_nodes(self) -> None:
        self.assertEqual(monitor.parse_corosync_nodes(COROSYNC_CONF), {"mox1": 1, "mox2": 2, "mox3": 3})
        links = monitor.parse_links(LINKS_ALL_UP)
        self.assertEqual(links[2], {"reachable": True, "links": {0: True, 1: True}})
        down = monitor.parse_links(LINKS_ALL_UP.replace("1397\n\nnodeid: 3", "1397\n\nnodeid: 3").replace(
            "(100.64.0.11->100.64.0.13) enabled connected", "(100.64.0.11->100.64.0.13) enabled disconnected"))
        self.assertEqual(down[3]["links"], {0: True, 1: False})

    def test_qnetd_listing(self) -> None:
        self.assertEqual(monitor.parse_qnetd(QNETD_HEALTHY, "MyAppCloud"),
                         {1: "ACK (ACK)", 2: "No change (ACK)"})
        self.assertIsNone(monitor.parse_qnetd(QNETD_HEALTHY, "OtherCluster"))

    def test_qdevice_tool_state_uses_tab_separated_values(self) -> None:
        self.assertEqual(monitor.parse_key_values(QDEVICE_TOOL.format(state="Connect failed"))["State"],
                         "Connect failed")

    def test_df_keeps_only_block_device_filesystems(self) -> None:
        self.assertEqual([(row["mount"], row["type"]) for row in monitor.parse_df(DF_PROD1)],
                         [("/", "ext4"), ("/boot/efi", "vfat")])

    def test_enddate_allows_space_padded_day(self) -> None:
        expected = monitor.dt.datetime(2041, 9, 16, 23, 49, tzinfo=monitor.dt.timezone.utc).timestamp()
        self.assertEqual(monitor.parse_enddate("notAfter=Sep 16 23:49:00 2041 GMT\n"), expected)
        self.assertIsNotNone(monitor.parse_enddate("notAfter=Feb  3 01:02:03 2030 GMT"))
        self.assertIsNone(monitor.parse_enddate("garbage"))

    def test_pool_problem_members_name_the_faulted_mirror_member(self) -> None:
        header, entries = monitor.parse_pool(ZPOOL_DEGRADED_STATUS)
        self.assertEqual(header["state"], "DEGRADED")
        self.assertEqual(monitor.pool_problem_members(entries), [
            "mirror-0: /dev/disk/by-id/nvme-INTEL_SSDPE2KX020T7_SERIALB0000000000-part3 FAULTED"
        ])
        self.assertEqual(monitor.pool_problem_members(monitor.parse_pool(ZPOOL_HEALTHY_STATUS)[1]), [])

    def test_sanitize_neutralizes_tokens_and_control_characters(self) -> None:
        text = monitor.sanitize("unit BMACEVENT.service\nbmacclear bmacp1 xbmacp2y bmaccheck")
        for token in ("bmacevent", "bmacclear", "bmacp1", "bmacp2"):
            self.assertNotIn(token, text.lower().replace("bmac-", "#"))
        self.assertIn("bmaccheck", text)
        self.assertNotIn("\n", text)


class HostCheckTest(unittest.TestCase):
    def test_links_all_up_one_link_down_both_down_and_peer_missing(self) -> None:
        runner = FakeRunner()
        runner.set(("corosync-cfgtool", "-n"), LINKS_ALL_UP)
        found = by_key(monitor.check_links(context(host_config(), runner)))
        self.assertTrue(all(found[f"link{n}:{peer}"].ok for n in (0, 1) for peer in ("mox2", "mox3")))

        runner.set(("corosync-cfgtool", "-n"), LINKS_ALL_UP.replace(
            "(10.213.0.11->10.213.0.12) enabled connected", "(10.213.0.11->10.213.0.12) enabled disconnected"))
        found = by_key(monitor.check_links(context(host_config(), runner)))
        self.assertFalse(found["link0:mox2"].ok)
        self.assertEqual(found["link0:mox2"].priority, "p2")
        self.assertIn("private VLAN", found["link0:mox2"].message)
        self.assertTrue(found["link1:mox2"].ok)

        both = LINKS_ALL_UP.replace("enabled connected mtu: 1397\n   LINK: 1 udp (100.64.0.11->100.64.0.12) enabled connected",
                                    "enabled disconnected mtu: 1397\n   LINK: 1 udp (100.64.0.11->100.64.0.12) enabled disconnected", 1)
        runner.set(("corosync-cfgtool", "-n"), both)
        found = by_key(monitor.check_links(context(host_config(), runner)))
        self.assertEqual((found["link0:mox2"].ok, found["link1:mox2"].ok), (False, False))
        self.assertEqual((found["link0:mox2"].priority, found["link1:mox2"].priority), ("p1", "p1"))

        runner.set(("corosync-cfgtool", "-n"), LINKS_ALL_UP.split("\nnodeid: 3")[0])
        found = by_key(monitor.check_links(context(host_config(), runner)))
        self.assertFalse(found["corosync-peer:mox3"].ok)
        self.assertIsNone(found["link0:mox3"].ok)

    def test_quorum_without_qdevice(self) -> None:
        runner = FakeRunner()
        runner.set(("corosync-quorumtool", "-s"), QUORUM_THREE_HOSTS)
        found = by_key(monitor.check_quorum(context(host_config(), runner)))
        self.assertTrue(found["quorum"].ok and found["votes"].ok and found["qdevice-vote"].ok)

        runner.set(("corosync-quorumtool", "-s"), QUORUM_NOT_QUORATE, rc=2)
        found = by_key(monitor.check_quorum(context(host_config(), runner)))
        self.assertFalse(found["quorum"].ok)
        self.assertEqual(found["quorum"].priority, "p1")
        self.assertFalse(found["votes"].ok)
        self.assertIn("1 of 3", found["votes"].message)

    def test_qdevice_vote_lost_explains_the_missing_vote(self) -> None:
        runner = FakeRunner()
        runner.set(("corosync-quorumtool", "-s"), QUORUM_QDEVICE_HEALTHY)
        runner.set(("corosync-qdevice-tool", "-s"), QDEVICE_TOOL.format(state="Connected"))
        config = host_config(expected_peers=["mox2"], qdevice_expected=True)
        found = by_key(monitor.check_quorum(context(config, runner)))
        self.assertTrue(found["qdevice-vote"].ok and found["votes"].ok)

        runner.set(("corosync-quorumtool", "-s"), QUORUM_QDEVICE_LOST)
        runner.set(("corosync-qdevice-tool", "-s"), QDEVICE_TOOL.format(state="Connect failed"))
        found = by_key(monitor.check_quorum(context(config, runner)))
        self.assertFalse(found["qdevice-vote"].ok)
        self.assertEqual(found["qdevice-vote"].priority, "p1")
        self.assertIn("0 votes", found["qdevice-vote"].message)
        self.assertIn("not voting on mox1, mox2", found["qdevice-vote"].message)
        self.assertIn("Connect failed", found["qdevice-vote"].message)
        self.assertTrue(found["votes"].ok, "the QDevice check already explains the missing vote")

    def test_qdevice_missing_from_membership(self) -> None:
        runner = FakeRunner()
        runner.set(("corosync-quorumtool", "-s"), QUORUM_QDEVICE_HEALTHY.replace(
            "         0          1            Qdevice\n", ""))
        runner.set(("corosync-qdevice-tool", "-s"), QDEVICE_TOOL.format(state="Connected"))
        found = by_key(monitor.check_quorum(context(host_config(qdevice_expected=True), runner)))
        self.assertFalse(found["qdevice-vote"].ok)
        self.assertIn("missing", found["qdevice-vote"].message)

    def test_cloudflared_ready_refused_and_no_connections(self) -> None:
        def ready(status, body):
            return lambda url, timeout: (status, body)

        ok = monitor.check_cloudflared(context(host_config(), FakeRunner(), http_get=ready(
            200, '{"status":200,"readyConnections":4,"connectorId":"00000000-0000-0000-0000-000000000000"}')))
        self.assertTrue(ok[0].ok)
        zero = monitor.check_cloudflared(context(host_config(), FakeRunner(), http_get=ready(
            200, '{"status":200,"readyConnections":0}')))
        self.assertFalse(zero[0].ok)

        def refused(url, timeout):
            raise ConnectionRefusedError(111, "Connection refused")

        found = monitor.check_cloudflared(context(host_config(), FakeRunner(), http_get=refused))
        self.assertFalse(found[0].ok)
        self.assertIn("connection refused", found[0].message)

    def test_postfix_queue_empty_and_old_message(self) -> None:
        runner = FakeRunner()
        runner.set(("postqueue", "-j"), "")
        self.assertTrue(monitor.check_postfix(context(host_config(), runner))[0].ok)
        runner.set(("postqueue", "-j"), POSTQUEUE_DEFERRED)
        ctx = context(host_config(), runner)
        ctx.now = lambda: T0 + 30 * 60
        self.assertTrue(monitor.check_postfix(ctx)[0].ok, "younger than the limit")
        ctx.now = lambda: T0 + 2 * 3600
        found = monitor.check_postfix(ctx)[0]
        self.assertFalse(found.ok)
        self.assertIn("to example.com: deferred transport", found.message)
        self.assertNotIn("admin@", found.message)
        self.assertIn("queue deferred", found.message)

    def zfs_runner(self, healthy: bool = True, used: int = 9796521984, available: int = 1921358725120) -> FakeRunner:
        runner = FakeRunner()
        runner.set(("zpool", "list", "-H", "-o", "name"), "rpool\n")
        if healthy:
            runner.set(("zpool", "status", "-x", "rpool"), "pool 'rpool' is healthy\n")
            runner.set(("zpool", "status", "-P", "-p", "rpool"), ZPOOL_HEALTHY_STATUS)
        else:
            runner.set(("zpool", "status", "-x", "rpool"), ZPOOL_DEGRADED_STATUS)
            runner.set(("zpool", "status", "-P", "-p", "rpool"), ZPOOL_DEGRADED_STATUS)
        runner.set(("zfs", "list", "-H", "-p", "-o", "used,available", "rpool"), f"{used}\t{available}\n")
        return runner

    def test_zfs_healthy_and_degraded(self) -> None:
        self.assertTrue(monitor.check_zfs(context(host_config(), self.zfs_runner()))[0].ok)
        found = monitor.check_zfs(context(host_config(), self.zfs_runner(healthy=False)))[0]
        self.assertFalse(found.ok)
        self.assertEqual(found.priority, "p1")
        self.assertIn("pool rpool is DEGRADED (mirror-0: ", found.message)
        self.assertIn("SERIALB0000000000-part3 FAULTED", found.message)

    def test_capacity_thresholds(self) -> None:
        def capacity(free_percent: float):
            total = 1000 * 2**30
            available = int(total * free_percent / 100)
            runner = self.zfs_runner(used=total - available, available=available)
            return monitor.check_capacity(context(host_config(), runner))[0]

        self.assertTrue(capacity(99.5).ok)
        self.assertEqual((capacity(9.9).ok, capacity(9.9).priority), (False, "p2"))
        self.assertIsNone(capacity(11).ok)
        self.assertTrue(capacity(12.1).ok)
        self.assertEqual(capacity(4.9).priority, "p1")
        self.assertIn("below 10%", capacity(9.9).message)

    def smart_runner(self, nvme0: str, nvme1: str | None = None, rc0: int = 0) -> FakeRunner:
        runner = FakeRunner()
        runner.set(("smartctl", "--scan", "-j"), SMART_SCAN)
        runner.set(("smartctl", "-j", "-d", "nvme", "-i", "-H", "-A", "/dev/nvme0"), nvme0, rc=rc0)
        runner.set(("smartctl", "-j", "-d", "nvme", "-i", "-H", "-A", "/dev/nvme1"),
                   nvme1 if nvme1 is not None else nvme_report("SERIALB0000000000"))
        return runner

    def test_nvme_healthy_drives_set_media_error_baselines(self) -> None:
        ctx = context(host_config(), self.smart_runner(nvme_report()))
        found = by_key(monitor.check_smart(ctx))
        for serial in ("SERIALA0000000000", "SERIALB0000000000"):
            for prefix in ("smart", "smart-read", "nvme-warning", "nvme-spare", "nvme-wear"):
                self.assertTrue(found[f"{prefix}:{serial}"].ok, f"{prefix}:{serial}")
        self.assertEqual(ctx.baselines["SERIALA0000000000"], {"media_errors": 0})
        self.assertTrue(ctx.baselines_changed)

    def test_nvme_critical_warning_bits_are_decoded(self) -> None:
        for bit, label in monitor.NVME_WARNING_BITS.items():
            with self.subTest(bit=bit):
                found = by_key(monitor.check_smart(context(host_config(), self.smart_runner(
                    nvme_report(critical_warning=1 << bit)))))
                warning = found["nvme-warning:SERIALA0000000000"]
                self.assertFalse(warning.ok)
                self.assertEqual(warning.priority, "p1")
                self.assertIn(label, warning.message)

    def test_nvme_spare_threshold_and_early_warning(self) -> None:
        cases = {10: (False, "p1"), 15: (False, "p2"), 20: (False, "p2"), 21: (True, None)}
        for spare, (ok, priority) in cases.items():
            with self.subTest(spare=spare):
                found = by_key(monitor.check_smart(context(host_config(), self.smart_runner(
                    nvme_report(available_spare=spare)))))["nvme-spare:SERIALA0000000000"]
                self.assertEqual(found.ok, ok)
                if priority:
                    self.assertEqual(found.priority, priority)

    def test_nvme_wear_levels(self) -> None:
        for used, ok, priority in ((89, True, "p2"), (90, False, "p2"), (100, False, "p1"), (130, False, "p1")):
            with self.subTest(used=used):
                found = by_key(monitor.check_smart(context(host_config(), self.smart_runner(
                    nvme_report(percentage_used=used)))))["nvme-wear:SERIALA0000000000"]
                self.assertEqual((found.ok, found.priority), (ok, priority))
        message = by_key(monitor.check_smart(context(host_config(), self.smart_runner(
            nvme_report(percentage_used=95)))))["nvme-wear:SERIALA0000000000"].message
        self.assertIn("211.7 TB written", message)
        self.assertIn("24589 power-on hours", message)

    def test_nvme_media_errors_rise_from_baseline_and_health_failure(self) -> None:
        state = monitor.new_state()
        state["smart_baselines"] = {"SERIALA0000000000": {"media_errors": 0}}
        found = by_key(monitor.check_smart(context(host_config(), self.smart_runner(
            nvme_report(media_errors=3)), state=state)))["smart:SERIALA0000000000"]
        self.assertFalse(found.ok)
        self.assertEqual(found.priority, "p2")
        self.assertIn("media errors rose from 0 to 3", found.message)
        self.assertIn("--reset-smart-baseline mox1:SERIALA0000000000", found.message)
        failed = by_key(monitor.check_smart(context(host_config(), self.smart_runner(
            nvme_report(passed=False), rc0=8))))["smart:SERIALA0000000000"]
        self.assertEqual((failed.ok, failed.priority), (False, "p1"))

    def test_same_drive_twice_is_deduplicated_by_serial(self) -> None:
        runner = self.smart_runner(nvme_report(), nvme_report())
        found = monitor.check_smart(context(host_config(), runner))
        self.assertEqual(sum(1 for f in found if f.key == "smart:SERIALA0000000000"), 1)
        self.assertNotIn("smart:SERIALB0000000000", by_key(found))

    def test_smart_read_error_and_timeout(self) -> None:
        found = by_key(monitor.check_smart(context(host_config(), self.smart_runner(
            nvme_report(), rc0=2))))
        self.assertFalse(found["smart-read:SERIALA0000000000"].ok)
        self.assertIn("device open failed", found["smart-read:SERIALA0000000000"].message)
        runner = self.smart_runner("")
        runner.set(("smartctl", "-j", "-d", "nvme", "-i", "-H", "-A", "/dev/nvme0"), "", rc=124,
                   err="timed out after 30s")
        found = by_key(monitor.check_smart(context(host_config(), runner)))
        self.assertFalse(found["smart-read:/dev/nvme0"].ok)
        self.assertIn("timed out", found["smart-read:/dev/nvme0"].message)

    def test_baselines_of_removed_drives_are_forgotten_only_after_a_clean_scan(self) -> None:
        state = monitor.new_state()
        state["smart_baselines"] = {"OLDSERIAL": {"media_errors": 7}}
        ctx = context(host_config(), self.smart_runner(nvme_report()), state=state)
        monitor.check_smart(ctx)
        self.assertNotIn("OLDSERIAL", ctx.baselines)
        ctx = context(host_config(), self.smart_runner(nvme_report(), rc0=2), state=state)
        monitor.check_smart(ctx)
        self.assertIn("OLDSERIAL", ctx.baselines)

    def test_ata_counters_passed_failed_and_increases(self) -> None:
        def ata_runner(report: str, rc: int = 0) -> FakeRunner:
            runner = FakeRunner()
            runner.set(("smartctl", "--scan", "-j"), json.dumps(
                {"devices": [{"name": "/dev/sda", "type": "sat", "protocol": "ATA"}]}))
            runner.set(("smartctl", "-j", "-d", "sat", "-i", "-H", "-A", "/dev/sda"), report, rc=rc)
            return runner

        ctx = context(host_config(), ata_runner(ata_report()))
        self.assertTrue(by_key(monitor.check_smart(ctx))["smart:ATASERIAL0001"].ok)
        self.assertEqual(ctx.baselines["ATASERIAL0001"], {"5": 0, "197": 0, "198": 0})
        state = monitor.new_state()
        state["smart_baselines"] = dict(ctx.baselines)
        for name, report in (("Reallocated_Sector_Ct", ata_report(reallocated=8)),
                             ("Current_Pending_Sector", ata_report(pending=1)),
                             ("Offline_Uncorrectable", ata_report(uncorrectable=2))):
            with self.subTest(attribute=name):
                found = by_key(monitor.check_smart(context(host_config(), ata_runner(report), state=state)))
                self.assertFalse(found["smart:ATASERIAL0001"].ok)
                self.assertIn(name, found["smart:ATASERIAL0001"].message)
        failed = by_key(monitor.check_smart(context(host_config(), ata_runner(ata_report(passed=False), rc=8))))
        self.assertEqual(failed["smart:ATASERIAL0001"].priority, "p1")


class QDeviceCheckTest(unittest.TestCase):
    def tailscale_runner(self, failing=(), status=TAILSCALE_STATUS) -> FakeRunner:
        runner = FakeRunner()
        runner.set(("tailscale", "status", "--json"), json.dumps(status))
        for host, ip in (("mox1", "100.64.0.11"), ("mox2", "100.64.0.12"), ("mox3", "100.64.0.13")):
            for wait in ("3", "10"):
                runner.set(("ping", "-n", "-c", "1", "-W", wait, ip), rc=1 if host in failing else 0)
        return runner

    def test_tailscale_reachability_uses_membership_not_the_peer_list(self) -> None:
        runner = self.tailscale_runner()
        found = by_key(monitor.check_tailscale(context(qdevice_config(), runner)))
        self.assertEqual(set(found), {"tailscale:mox1", "tailscale:mox2"})
        self.assertTrue(all(f.ok for f in found.values()))
        pinged = {call[-1] for call in runner.calls if call[0] == "ping"}
        self.assertEqual(pinged, {"100.64.0.11", "100.64.0.12"}, "mox3 and onyx are ignored")

    def test_one_host_failing_is_p2_and_every_host_failing_is_p1(self) -> None:
        found = by_key(monitor.check_tailscale(context(qdevice_config(), self.tailscale_runner(["mox2"]))))
        self.assertEqual((found["tailscale:mox2"].ok, found["tailscale:mox2"].priority), (False, "p2"))
        self.assertIn("online=true", found["tailscale:mox2"].message)
        self.assertNotIn("last seen", found["tailscale:mox2"].message, "zero LastSeen is not applicable")
        found = by_key(monitor.check_tailscale(context(qdevice_config(), self.tailscale_runner(["mox1", "mox2"]))))
        self.assertTrue(all(f.priority == "p1" and not f.ok for f in found.values()))

    def test_offline_peer_message_and_missing_peer(self) -> None:
        config = qdevice_config(hosts=[{"name": "mox3", "nodeid": 3}, {"name": "mox4", "nodeid": 4},
                                       {"name": "mox1", "nodeid": 1}])
        found = by_key(monitor.check_tailscale(context(config, self.tailscale_runner(["mox3"]))))
        self.assertIn("online=false, last seen 2026-10-23T15:20:00.1Z", found["tailscale:mox3"].message)
        self.assertIn("not among the QDevice's Tailscale peers", found["tailscale:mox4"].message)

    def test_first_probe_after_start_uses_a_longer_timeout(self) -> None:
        runner = self.tailscale_runner()
        monitor.check_tailscale(context(qdevice_config(), runner, first_cycle=True))
        self.assertTrue(all(call[5] == "10" for call in runner.calls if call[0] == "ping"))

    def qnetd_runner(self, listing=QNETD_HEALTHY, active="active", rc=0) -> FakeRunner:
        runner = FakeRunner()
        runner.set(("systemctl", "is-active", "corosync-qnetd.service"), active + "\n", rc=0 if active == "active" else 3)
        runner.set(("corosync-qnetd-tool", "-l"), listing, rc=rc,
                   err="" if rc == 0 else "corosync-qnetd-tool: Can't connect to qnetd socket (is QNetd running?): No such file or directory")
        return runner

    def test_qnetd_healthy_missing_node_unacked_and_cluster_absent(self) -> None:
        found = by_key(monitor.check_qnetd(context(qdevice_config(), self.qnetd_runner())))
        self.assertTrue(all(f.ok for f in found.values()))
        missing = QNETD_HEALTHY.split("    Node ID 2:")[0]
        found = by_key(monitor.check_qnetd(context(qdevice_config(), self.qnetd_runner(missing))))
        self.assertFalse(found["qnetd:mox2"].ok)
        self.assertEqual(found["qnetd:mox2"].priority, "p1")
        unacked = QNETD_HEALTHY.replace("No change (ACK)", "ACK (NACK)")
        found = by_key(monitor.check_qnetd(context(qdevice_config(), self.qnetd_runner(unacked))))
        self.assertFalse(found["qnetd:mox2"].ok)
        self.assertIn("NACK", found["qnetd:mox2"].message)
        found = by_key(monitor.check_qnetd(context(qdevice_config(), self.qnetd_runner(
            QNETD_HEALTHY.replace("MyAppCloud", "Other")))))
        self.assertFalse(found["qnetd:service"].ok)
        self.assertIsNone(found["qnetd:mox1"].ok)

    def test_qnetd_stopped_reports_the_service_and_standby_skips_it(self) -> None:
        found = by_key(monitor.check_qnetd(context(qdevice_config(), self.qnetd_runner("", "inactive", 3))))
        self.assertFalse(found["qnetd:service"].ok)
        self.assertIn("inactive", found["qnetd:service"].message)
        self.assertIsNone(found["qnetd:mox1"].ok)
        runner = self.qnetd_runner("", "inactive", 3)
        found = monitor.check_qnetd(context(qdevice_config(qnetd_expected=False), runner))
        self.assertEqual([(f.key, f.ok) for f in found], [("qnetd:service", True)])
        self.assertEqual(runner.calls, [])

    def test_ingress_probe_status_text_errors_and_challenge(self) -> None:
        probes = [{"vm": "prod1", "fqdn": "prod1.example.com", "path": "/api/v1/healthz",
                   "expected_status": 200, "expected_text": "ok"}]
        seen_headers = {}

        def answer(status, body=b'{"status":"ok"}', headers=None, error=None):
            def https_get(fqdn, path, timeout, request_headers):
                seen_headers.update(request_headers)
                if error:
                    raise error
                return status, headers or {"server": "cloudflare"}, body
            return https_get

        config = qdevice_config(probes=probes)
        found = by_key(monitor.check_ingress(context(config, FakeRunner(), https_get=answer(200))))
        self.assertTrue(found["ingress:prod1.example.com"].ok)
        self.assertTrue(seen_headers["User-Agent"].startswith("bmac-monitor/"))
        self.assertEqual(seen_headers["Cache-Control"], "no-cache")
        found = by_key(monitor.check_ingress(context(config, FakeRunner(), https_get=answer(404))))
        self.assertFalse(found["ingress:prod1.example.com"].ok)
        self.assertIn("HTTP 404, expected 200", found["ingress:prod1.example.com"].message)
        found = by_key(monitor.check_ingress(context(config, FakeRunner(), https_get=answer(200, b"nope"))))
        self.assertIn("without the expected text", found["ingress:prod1.example.com"].message)
        found = by_key(monitor.check_ingress(context(config, FakeRunner(), https_get=answer(
            0, error=TimeoutError("timed out")))))
        self.assertFalse(found["ingress:prod1.example.com"].ok)
        found = by_key(monitor.check_ingress(context(config, FakeRunner(), https_get=answer(
            403, b"", {"cf-mitigated": "challenge"}))))
        self.assertIsNone(found["ingress:prod1.example.com"].ok)
        self.assertFalse(found["ingress-challenged:prod1.example.com"].ok)
        self.assertEqual(found["ingress-challenged:prod1.example.com"].priority, "p2")


class GuestCheckTest(unittest.TestCase):
    def guest(self, settings: dict | None) -> tuple[dict, dict]:
        config = {
            "schema_version": 1, "cluster": "MyAppCloud", "node": "prod1", "role": "guest",
            "ping_url": "https://hc-ping.com/x", "thresholds": dict(monitor.DEFAULT_THRESHOLDS),
            "guest": {"settings_file": "/etc/app-ha-monitor/guest.json"},
        }
        files = {} if settings is None else {"/etc/app-ha-monitor/guest.json": json.dumps(settings)}
        return config, files

    def test_services_certs_and_missing_settings(self) -> None:
        config, files = self.guest(None)
        self.assertEqual(monitor.check_services(context(config, FakeRunner(), files=files)), [])
        config, files = self.guest({"schema_version": 1, "services": ["hello-todo.service"],
                                    "cert_paths": ["/etc/hello-todo/cert.pem"]})
        runner = FakeRunner()
        runner.set(("systemctl", "is-active", "hello-todo.service"), "failed\n", rc=3)
        service = monitor.check_services(context(config, runner, files=files))[0]
        self.assertEqual((service.ok, service.priority, service.kind), (False, "p1", "service"))
        cert_argv = ("openssl", "x509", "-enddate", "-noout", "-in", "/etc/hello-todo/cert.pem")
        for days, ok, priority in ((400, True, None), (10, False, "p2"), (2, False, "p1"), (-1, False, "p1")):
            with self.subTest(days=days):
                stamp = monitor.dt.datetime.fromtimestamp(T0 + days * 86400, monitor.dt.timezone.utc)
                runner.set(cert_argv, f"notAfter={stamp.strftime('%b %e %H:%M:%S %Y')} GMT\n")
                cert = monitor.check_certs(context(config, runner, files=files))[0]
                self.assertEqual(cert.ok, ok)
                if priority:
                    self.assertEqual(cert.priority, priority)
        runner.set(cert_argv, "", rc=1, err="Could not open file or uri for loading certificate")
        cert = monitor.check_certs(context(config, runner, files=files))[0]
        self.assertEqual((cert.ok, cert.priority), (False, "p2"))
        self.assertIn("cannot be read", cert.message)

    def test_malformed_settings_become_a_check_error(self) -> None:
        config, _ = self.guest(None)
        files = {"/etc/app-ha-monitor/guest.json": '{"schema_version": 1, "services": ["bad unit"]}'}
        check = next(c for c in monitor.CHECKS if c.name == "services")
        findings, error = monitor.run_check(check, context(config, FakeRunner(), files=files))
        self.assertEqual(findings, [])
        self.assertIn("is invalid", error)

    def test_guest_disk_thresholds(self) -> None:
        config, files = self.guest(None)
        runner = FakeRunner()
        runner.set(("df", "-P", "-B1", "-T"), DF_PROD1)
        found = by_key(monitor.check_disk(context(config, runner, files=files)))
        self.assertEqual(set(found), {"disk:/", "disk:/boot/efi"})
        self.assertTrue(all(f.ok for f in found.values()))


class RecordingPinger:
    """A fake Healthchecks ping endpoint on 127.0.0.1 that records requests."""

    def __init__(self) -> None:
        self.requests: list[tuple[str, str]] = []
        self.fail_paths: set[str] = set()
        pinger = self

        class Handler(http.server.BaseHTTPRequestHandler):
            def do_POST(self) -> None:  # noqa: N802
                length = int(self.headers.get("Content-Length") or 0)
                body = self.rfile.read(length).decode("utf-8")
                pinger.requests.append((self.path, body))
                status = 500 if any(self.path.endswith(p) for p in pinger.fail_paths) else 200
                self.send_response(status)
                self.end_headers()
                self.wfile.write(b"OK")

            def log_message(self, *args) -> None:
                pass

        self.server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        self.thread = threading.Thread(target=self.server.serve_forever, daemon=True)
        self.thread.start()
        self.url = f"http://127.0.0.1:{self.server.server_address[1]}/0f0f0f0f-aaaa-bbbb-cccc-000000000001"

    def close(self) -> None:
        self.server.shutdown()
        self.server.server_close()

    def kinds(self) -> list[str]:
        result = []
        for path, body in self.requests:
            if path.endswith("/fail"):
                result.append("fail")
            elif body.startswith("bmacclear"):
                result.append("clear")
            else:
                result.append("heartbeat")
        return result


class Clock:
    def __init__(self, start: float = T0) -> None:
        self.value = start

    def __call__(self) -> float:
        return self.value

    def advance(self, seconds: float) -> None:
        self.value += seconds


class AgentTest(unittest.TestCase):
    def setUp(self) -> None:
        self.root = Path(tempfile.mkdtemp())
        self.addCleanup(shutil.rmtree, self.root, True)
        self.pinger = RecordingPinger()
        self.addCleanup(self.pinger.close)
        self.clock = Clock()
        self.runner = FakeRunner()
        self.runner.set(("df", "-P", "-B1", "-T"), DF_PROD1)
        self.runner.set(("openssl", "x509", "-enddate", "-noout", "-in", "/etc/app.pem"),
                        "notAfter=Sep 16 23:49:00 2041 GMT\n")
        self.service_state = "active"
        self.settings = self.root / "guest.json"
        self.settings.write_text(json.dumps({"schema_version": 1, "services": ["app.service"],
                                             "cert_paths": ["/etc/app.pem"]}))
        self.config_path = self.root / "config.json"
        self.write_config()
        self.sleeps: list[float] = []
        self.logs: list[str] = []

    def write_config(self, **thresholds) -> None:
        self.config_path.write_text(json.dumps({
            "schema_version": 1, "cluster": "MyAppCloud", "node": "prod1", "role": "guest",
            "ping_url": self.pinger.url,
            "thresholds": {**monitor.DEFAULT_THRESHOLDS, **thresholds},
            "guest": {"settings_file": str(self.settings)},
        }))

    def agent(self, hostname: str = "prod1") -> "monitor.Agent":
        self.runner.set(("systemctl", "is-active", "app.service"), self.service_state + "\n")
        sender = monitor.Sender(sleep=self.sleeps.append)
        return monitor.Agent(self.config_path, self.root / "state", runner=self.runner,
                             clock=self.clock, sender=sender, hostname=lambda: hostname)

    def set_service(self, agent, state: str) -> None:
        self.runner.set(("systemctl", "is-active", "app.service"), state + "\n")

    def cycle(self, agent, seconds: float = 30) -> None:
        agent.cycle()
        self.clock.advance(seconds)

    def test_service_opens_after_two_failures_and_pulse_is_fail_clear_heartbeat(self) -> None:
        agent = self.agent()
        self.cycle(agent)
        self.assertEqual(self.pinger.requests, [], "nothing to announce")
        self.set_service(agent, "failed")
        self.cycle(agent)
        self.assertNotIn("service:app.service", agent.state["problems"])
        self.cycle(agent)
        self.assertIn("service:app.service", agent.state["problems"])
        self.assertEqual(self.pinger.kinds(), ["fail", "clear", "heartbeat"])
        fail_body = self.pinger.requests[0][1]
        lines = fail_body.splitlines()
        self.assertEqual(lines[0], "bmacevent bmacp1")
        self.assertIn("cluster=MyAppCloud node=prod1 role=guest", lines[1])
        self.assertEqual(lines[4], "NEW")
        self.assertIn("[p1] service:app.service", lines[5])
        self.assertTrue(self.pinger.requests[1][1].startswith("bmacclear\nBMAC clear cluster=MyAppCloud node=prod1"))
        heartbeat = self.pinger.requests[2][1]
        self.assertTrue(heartbeat.startswith("BMAC heartbeat cluster=MyAppCloud node=prod1 role=guest"))
        self.assertIn("open_problems=1", heartbeat)
        self.assertFalse(monitor.TOKEN_RE.search(heartbeat))
        self.assertFalse(agent.state["pending_clear"])
        self.assertTrue(agent.state["problems"]["service:app.service"]["announced"])

        self.pinger.requests.clear()
        self.set_service(agent, "active")
        self.clock.advance(60)
        self.cycle(agent)
        self.assertNotIn("service:app.service", agent.state["problems"])
        self.assertEqual(self.pinger.kinds(), ["fail", "clear", "heartbeat"])
        body = self.pinger.requests[0][1]
        self.assertTrue(body.startswith("bmacevent bmacp2\n"), "only RESOLVED items is p2")
        self.assertIn("RESOLVED\n  [p1] service:app.service", body)
        self.assertIn("resolved (was open", body)

    def test_fast_debounce_fails_twice_then_passes_without_a_problem(self) -> None:
        agent = self.agent()
        check = monitor.Check("fake", "guest", "fast", ("link0",), lambda ctx: [])
        now = self.clock()
        for ok in (False, False, True):
            agent.apply(check, [monitor.Finding("link0:mox2", ok, "p2", "down", "fast")], None, now)
        self.assertEqual(agent.state["problems"], {})
        for _ in range(3):
            agent.apply(check, [monitor.Finding("link0:mox2", False, "p2", "down", "fast")], None, now)
        self.assertIn("link0:mox2", agent.state["problems"])
        for count in range(3):
            self.assertIn("link0:mox2", agent.state["problems"], count)
            agent.apply(check, [monitor.Finding("link0:mox2", True, "p2", "up", "fast")], None, now)
        self.assertNotIn("link0:mox2", agent.state["problems"])

    def test_slow_check_opens_on_first_detection_and_hysteresis_holds(self) -> None:
        agent = self.agent()
        check = next(c for c in monitor.CHECKS if c.role == "guest" and c.name == "disk")
        ctx = monitor.Context(agent.config, agent.state, runner=self.runner, now=self.clock)

        def df(free_percent: float) -> list:
            total = 10**12
            available = int(total * free_percent / 100)
            self.runner.set(("df", "-P", "-B1", "-T"), "header\n"
                            f"/dev/sda2 ext4 {total} {total - available} {available} 90% /\n")
            return check.func(ctx)

        agent.apply(check, df(9.9), None, self.clock())
        self.assertEqual(agent.state["problems"]["disk:/"]["priority"], "p2")
        agent.apply(check, df(11), None, self.clock())
        self.assertIn("disk:/", agent.state["problems"], "between 10% and 12% the state is kept")
        agent.apply(check, df(4.9), None, self.clock())
        self.assertEqual(agent.state["problems"]["disk:/"]["priority"], "p1")
        self.assertEqual([i["section"] + i["priority"] for i in agent.state["outbox"]], ["NEWp1"])
        agent.apply(check, df(12.1), None, self.clock())
        self.assertNotIn("disk:/", agent.state["problems"])
        self.assertEqual(agent.state["outbox"], [], "never-announced problems resolve silently")

    def test_a_crashing_check_becomes_an_agent_finding_and_keeps_its_problems(self) -> None:
        agent = self.agent()
        self.set_service(agent, "failed")
        self.cycle(agent)
        self.cycle(agent)
        self.settings.write_text("{not json")
        self.clock.advance(60)
        self.cycle(agent)
        self.assertIn("service:app.service", agent.state["problems"])
        self.assertIn("agent:services", agent.state["problems"])
        self.assertIn("services", agent.state["check_errors"])

    def test_first_start_announces_slow_problems_in_one_pulse(self) -> None:
        self.runner.set(("df", "-P", "-B1", "-T"), "header\n/dev/sda2 ext4 1000 990 10 99% /\n"
                        "/dev/sda1 vfat 1000 995 5 99% /boot/efi\n")
        agent = self.agent()
        self.assertTrue(agent.first_start)
        self.cycle(agent)
        self.assertEqual(self.pinger.kinds(), ["fail", "clear", "heartbeat"])
        body = self.pinger.requests[0][1]
        self.assertIn("disk:/ ", body)
        self.assertIn("disk:/boot/efi", body)

    def test_restart_with_state_does_not_reannounce(self) -> None:
        self.runner.set(("df", "-P", "-B1", "-T"), "header\n/dev/sda2 ext4 1000 990 10 99% /\n")
        self.cycle(self.agent())
        self.pinger.requests.clear()
        agent = self.agent()
        self.assertFalse(agent.first_start)
        self.cycle(agent)
        self.assertEqual(self.pinger.requests, [])
        self.assertIn("disk:/", agent.state["problems"])

    def test_corrupt_state_is_moved_aside(self) -> None:
        state_dir = self.root / "state"
        state_dir.mkdir()
        (state_dir / "state.json").write_text("{garbage")
        agent = self.agent()
        self.assertTrue(agent.first_start)
        self.assertTrue(list(state_dir.glob("state.json.corrupt-*")))
        agent.save()
        self.assertEqual(os.stat(state_dir / "state.json").st_mode & 0o777, 0o600)

    def test_failed_fail_keeps_items_queued_and_pending_clear_survives_restart(self) -> None:
        self.pinger.fail_paths = {"/fail"}
        agent = self.agent()
        self.set_service(agent, "failed")
        self.cycle(agent)
        self.cycle(agent)
        self.assertEqual(self.pinger.kinds(), ["fail"] * 4)
        self.assertEqual(self.sleeps, [2, 5, 10])
        self.assertTrue(agent.state["pending_clear"])
        self.assertEqual([item["key"] for item in agent.state["outbox"]], ["service:app.service"])
        self.pinger.requests.clear()
        self.service_state = "failed"
        restarted = self.agent()
        self.assertTrue(restarted.state["pending_clear"])
        restarted.send_heartbeat(self.clock())
        self.assertEqual(self.pinger.kinds(), ["clear", "heartbeat"])
        self.assertFalse(restarted.state["pending_clear"])
        self.pinger.fail_paths = set()
        self.pinger.requests.clear()
        self.clock.advance(60)
        self.cycle(restarted)
        self.assertEqual(self.pinger.kinds(), ["fail", "clear", "heartbeat"])
        self.assertIn("NEW\n  [p1] service:app.service", self.pinger.requests[0][1])
        self.assertEqual(restarted.state["outbox"], [])

    def test_rate_limits_merge_changes(self) -> None:
        agent = self.agent()
        check = monitor.Check("fake", "guest", "slow", ("cert",), lambda ctx: [])
        for index in range(14):
            agent.apply(check, [monitor.Finding(f"cert:/c{index}", False, "p2", "soon")], None, self.clock())
            agent.maybe_pulse(self.clock())
            self.clock.advance(30 if index == 0 else 61)
        fails = [body for path, body in self.pinger.requests if path.endswith("/fail")]
        self.assertEqual(len(fails), 12, "at most 12 pulses per hour")
        self.clock.advance(3600)
        agent.maybe_pulse(self.clock())
        last = [body for path, body in self.pinger.requests if path.endswith("/fail")][-1]
        self.assertIn("pulse rate limit reached; some changes were merged", last)
        self.assertIn("cert:/c12", last)
        self.assertIn("cert:/c13", last)

    def test_minimum_spacing_between_pulses(self) -> None:
        agent = self.agent()
        check = monitor.Check("fake", "guest", "slow", ("cert",), lambda ctx: [])
        agent.apply(check, [monitor.Finding("cert:/a", False, "p2", "soon")], None, self.clock())
        self.assertTrue(agent.maybe_pulse(self.clock()))
        self.clock.advance(30)
        agent.apply(check, [monitor.Finding("cert:/b", False, "p2", "soon")], None, self.clock())
        self.assertFalse(agent.maybe_pulse(self.clock()))
        self.clock.advance(31)
        self.assertTrue(agent.maybe_pulse(self.clock()))

    def test_reminders_after_the_configured_hours(self) -> None:
        self.write_config(reminder_hours=6)
        agent = self.agent()
        self.set_service(agent, "failed")
        self.cycle(agent)
        self.cycle(agent)
        self.pinger.requests.clear()
        self.clock.advance(5 * 3600)
        self.cycle(agent)
        self.assertEqual(self.pinger.requests, [])
        self.clock.advance(3600)
        self.cycle(agent)
        body = self.pinger.requests[0][1]
        self.assertIn("REMINDER\n  [p1] service:app.service", body)
        self.assertTrue(body.startswith("bmacevent bmacp1"))
        self.assertNotIn("ONGOING", body)

    def test_ongoing_lists_other_open_problems_and_body_is_truncated(self) -> None:
        config = json.loads(self.config_path.read_text())
        outbox = [{"section": "NEW", "key": "cert:/new", "priority": "p2", "message": "new problem"}]
        ongoing = [{"key": f"disk:/m{i}", "priority": "p2", "message": "x" * 290} for i in range(60)]
        body, carried = monitor.compose_event_body(config, outbox, ongoing, T0, "abcd1234")
        self.assertLessEqual(len(body.encode()), monitor.BODY_LIMIT_BYTES)
        self.assertIn("NEW\n  [p2] cert:/new", body)
        self.assertIn("ONGOING", body)
        self.assertRegex(body, r"\(\d+ more items omitted; see show_monitoring_state.sh\)")
        self.assertEqual(carried, outbox)

    def test_section_order_and_sanitized_messages(self) -> None:
        config = json.loads(self.config_path.read_text())
        outbox = [
            {"section": "RESOLVED", "key": "cert:/r", "priority": "p2", "message": "fixed"},
            {"section": "NEW", "key": "service:bmacclear.service", "priority": "p2",
             "message": "unit bmacevent bmacp1 is down"},
            {"section": "REMINDER", "key": "disk:/", "priority": "p1", "message": "full"},
        ]
        body, _ = monitor.compose_event_body(config, outbox, [], T0, "abcd1234")
        positions = [body.index(name + "\n") for name in ("NEW", "REMINDER", "RESOLVED")]
        self.assertEqual(positions, sorted(positions))
        self.assertTrue(body.startswith("bmacevent bmacp1\n"))
        self.assertEqual(len(monitor.TOKEN_RE.findall(body)), 2, "only the line-1 tokens remain")

    def test_staging_hostname_sends_nothing(self) -> None:
        agent = self.agent(hostname="stage1prod1")
        self.set_service(agent, "failed")
        for _ in range(3):
            self.cycle(agent)
        self.assertFalse(agent.send_heartbeat(self.clock()))
        self.assertEqual(self.pinger.requests, [])

    def test_ping_url_never_logged(self) -> None:
        self.pinger.fail_paths = {"/fail", "/0f0f0f0f-aaaa-bbbb-cccc-000000000001"}
        output = io.StringIO()
        with mock.patch.object(monitor.time, "sleep") as slept, \
                contextlib.redirect_stdout(output), contextlib.redirect_stderr(output):
            code = monitor.main(["--config", str(self.config_path),
                                 "--state-dir", str(self.root / "state2"), "test-pulse"])
        self.assertNotEqual(code, 0)
        self.assertEqual([c.args[0] for c in slept.call_args_list], [2, 5, 10])
        text = output.getvalue()
        self.assertIn("ping=<redacted>", text)
        self.assertNotIn("0f0f0f0f", text)
        self.assertNotIn("127.0.0.1", text)

    def test_test_pulse_sends_a_labelled_cleared_pulse(self) -> None:
        completed = subprocess.run(
            [sys.executable, str(AGENT_PATH), "--config", str(self.config_path),
             "--state-dir", str(self.root / "state2"), "test-pulse", "--priority", "p1"],
            capture_output=True, text=True, timeout=60,
        )
        self.assertEqual(completed.returncode, 0, completed.stderr)
        self.assertEqual(self.pinger.kinds(), ["fail", "clear", "heartbeat"])
        self.assertIn("NEW\n  [p1] test:alert", self.pinger.requests[0][1])
        self.assertTrue(self.pinger.requests[0][1].startswith("bmacevent bmacp1"))

    def test_reset_smart_baseline_request_is_applied_by_the_running_agent(self) -> None:
        agent = self.agent()
        agent.state["smart_baselines"] = {"SERIALA0000000000": {"media_errors": 0}}
        completed = subprocess.run(
            [sys.executable, str(AGENT_PATH), "--state-dir", str(self.root / "state"),
             "reset-smart-baseline", "SERIALA0000000000"],
            capture_output=True, text=True, timeout=60,
        )
        self.assertEqual(completed.returncode, 0, completed.stderr)
        self.cycle(agent)
        self.assertEqual(agent.state["smart_baselines"], {})


class CommandLineTest(unittest.TestCase):
    def setUp(self) -> None:
        self.root = Path(tempfile.mkdtemp())
        self.addCleanup(shutil.rmtree, self.root, True)

    def run_agent(self, *arguments: str) -> subprocess.CompletedProcess:
        return subprocess.run([sys.executable, str(AGENT_PATH), *arguments],
                              capture_output=True, text=True, timeout=60,
                              env={**os.environ, "PYTHONDONTWRITEBYTECODE": "1"})

    def test_help_works_without_root_or_config(self) -> None:
        completed = self.run_agent("--help")
        self.assertEqual(completed.returncode, 0, completed.stderr)
        for command in ("run", "check", "status", "test-pulse", "probe", "reset-smart-baseline",
                        "validate-config", "print-unit"):
            self.assertIn(command, completed.stdout)

    def test_validate_config_rejects_bad_configs(self) -> None:
        path = self.root / "config.json"
        good = qdevice_config()
        path.write_text(json.dumps(good))
        completed = self.run_agent("--config", str(path), "validate-config")
        self.assertEqual(completed.returncode, 0, completed.stderr)
        for change in ({"ping_url": "ftp://x"}, {"role": "nope"}, {"schema_version": 9},
                       {"thresholds": {"pool_min_free_percent": 12, "pool_clear_free_percent": 10}}):
            with self.subTest(change=change):
                path.write_text(json.dumps({**good, **change}))
                completed = self.run_agent("--config", str(path), "validate-config")
                self.assertEqual(completed.returncode, 2)
                self.assertIn("ERROR:", completed.stderr)
        completed = self.run_agent("--config", str(self.root / "missing.json"), "validate-config")
        self.assertEqual(completed.returncode, 2)

    def test_status_json_without_state(self) -> None:
        path = self.root / "config.json"
        path.write_text(json.dumps(host_config()))
        completed = self.run_agent("--config", str(path), "--state-dir", str(self.root / "s"), "status", "--json")
        self.assertEqual(completed.returncode, 0, completed.stderr)
        report = json.loads(completed.stdout)
        self.assertEqual((report["node"], report["state_present"], report["problems"]), ("mox1", False, {}))
        self.assertNotIn("ping_url", completed.stdout)

    def test_units_differ_only_in_the_staging_guard(self) -> None:
        guest = monitor.unit_text("guest", "/usr/local/lib/app-ha-monitor/app_ha_monitor.py")
        host = monitor.unit_text("host", "/usr/local/lib/app-ha-proxmox/lib/app_ha_monitor.py")
        self.assertIn("ExecCondition=", guest)
        self.assertNotIn("ExecCondition=", host)
        self.assertIn("ProtectSystem=strict", host)
        self.assertIn("ExecStart=/usr/bin/python3 -I /usr/local/lib/app-ha-proxmox/lib/app_ha_monitor.py run", host)
        with self.assertRaises(monitor.ConfigError):
            monitor.unit_text("host", "relative.py")


if __name__ == "__main__":
    unittest.main()
