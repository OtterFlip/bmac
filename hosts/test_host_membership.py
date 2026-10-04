#!/usr/bin/env python3

# Copyright (c) 2026 BEENTHERE VENTURES, INC.
# SPDX-License-Identifier: GPL-3.0-only

"""Mock-friendly tests for host removal, host purge, and their shared library."""

from __future__ import annotations

import json
from pathlib import Path
import shlex
import subprocess
import tempfile
import textwrap
import unittest


tempfile.tempdir = str(Path(tempfile.gettempdir()).resolve())

HOSTS_DIR = Path(__file__).resolve().parent
REPO_ROOT = HOSTS_DIR.parent
MEMBERSHIP_LIB = REPO_ROOT / "lib" / "host_membership.sh"
REMOVE_SCRIPT = HOSTS_DIR / "remove_host_from_cluster.sh"
PURGE_SCRIPT = HOSTS_DIR / "purge_host_from_cluster.sh"


def production(name: str, vmid: int, placement: list[str], owner: str, **extra) -> dict:
    row = {
        "kind": "production",
        "name": name,
        "id": f"id-{name}",
        "index": vmid - 99,
        "vmid": vmid,
        "state": "active",
        "revision": 4,
        "source": None,
        "placement": placement,
        "initial_node": placement[0],
        "owner_node": owner,
        "proxmox": {
            "ha_nodes": list(placement),
            "replication_targets": [node for node in placement if node != owner],
            "volume_id": f"local-zfs:vm-{vmid}-disk-1",
            "snapshot": None,
        },
    }
    row.update(extra)
    return row


def staging(name: str, vmid: int, node: str, source: str, source_vmid: int) -> dict:
    return {
        "kind": "staging",
        "name": name,
        "id": f"id-{name}",
        "vmid": vmid,
        "state": "active",
        "revision": 7,
        "source": source,
        "placement": [node],
        "initial_node": node,
        "owner_node": node,
        "proxmox": {
            "ha_nodes": [],
            "replication_targets": [],
            "volume_id": f"local-zfs:vm-{vmid}-disk-1",
            "snapshot": {
                "name": f"stg-base-{name}-x",
                "owner_node": "mox1",
                "guids": {},
            },
        },
    }


class HostMembershipTest(unittest.TestCase):
    def run_bash(
        self, body: str, expected: int = 0, stdin: str = ""
    ) -> subprocess.CompletedProcess[str]:
        completed = subprocess.run(
            ["bash", "-c", textwrap.dedent(body)],
            input=stdin,
            check=False,
            capture_output=True,
            text=True,
        )
        self.assertEqual(
            completed.returncode,
            expected,
            msg=f"stdout:\n{completed.stdout}\nstderr:\n{completed.stderr}",
        )
        return completed

    def test_qdevice_vote_checks_match_host_setup(self) -> None:
        healthy = textwrap.dedent(
            """\
            Quorate:          Yes
            Expected votes:   3
            Total votes:      3
            Flags:            Quorate Qdevice
                Nodeid      Votes    Qdevice Name
            0x00000001          1    A,V,NMW mox1 (local)
            0x00000003          1    A,V,NMW mox3
            0x00000000          1            Qdevice
            """
        )
        absent = "Quorate:          Yes\nExpected votes:   3\nTotal votes:      3\nFlags:            Quorate\n"
        self.run_bash(
            f"""
            source {shlex.quote(str(MEMBERSHIP_LIB))}
            hm_qdevice_status_is_healthy {shlex.quote(healthy)} 2
            ! hm_qdevice_status_is_healthy {shlex.quote(healthy.replace("A,V,NMW mox3", "A,NV,NMW mox3"))} 2
            hm_has_qdevice {shlex.quote(healthy)}
            hm_qdevice_absent_status_is_healthy {shlex.quote(absent)} 3
            ! hm_qdevice_absent_status_is_healthy {shlex.quote(healthy)} 3
            ! hm_qdevice_absent_status_is_healthy {shlex.quote(absent.replace("Yes", "No"))} 3
            """
        )

    def test_new_control_node_must_be_a_remaining_online_member(self) -> None:
        completed = self.run_bash(
            f"""
            source {shlex.quote(str(REPO_ROOT / "lib" / "cluster_control.sh"))}
            source {shlex.quote(str(MEMBERSHIP_LIB))}
            hm_choose_new_control_node mox1 chosen mox3 mox4
            printf 'CHOSEN=%s\\n' "$chosen"
            """,
            stdin="mox1\nmox4\n",
        )
        self.assertIn("Choose one of: mox3 mox4", completed.stderr)
        self.assertIn("CHOSEN=mox4", completed.stdout)
        defaulted = self.run_bash(
            f"""
            source {shlex.quote(str(REPO_ROOT / "lib" / "cluster_control.sh"))}
            source {shlex.quote(str(MEMBERSHIP_LIB))}
            hm_choose_new_control_node mox1 chosen mox3 mox4
            printf 'CHOSEN=%s\\n' "$chosen"
            """,
            stdin="\n",
        )
        self.assertIn("CHOSEN=mox3", defaulted.stdout)

    def test_remove_blocks_hosts_that_anything_still_uses(self) -> None:
        vms = [
            {"vmid": 9112, "type": "lxc", "node": "mox2", "name": "haproxy2"},
            {"vmid": 100, "type": "qemu", "node": "mox1", "name": "prod1"},
            {"vmid": 300, "type": "qemu", "node": "mox3", "name": "other"},
        ]
        rules = [{"rule": "production-prod1-100", "nodes": "mox1:1,mox3:1", "resources": "vm:100"}]
        replication = [{"id": "100-0", "guest": 100, "target": "mox3"}]
        body = f"""
            REMOVE_HOST_SOURCE_ONLY=1 source {shlex.quote(str(REMOVE_SCRIPT))}
            LIVE_VMS_JSON={shlex.quote(json.dumps(vms))}
            LIVE_RULES_JSON={shlex.quote(json.dumps(rules))}
            LIVE_REPLICATION_JSON={shlex.quote(json.dumps(replication))}
            hm_registry() {{
              if [[ "$2" == mox3 ]]; then
                printf '{{"node":"mox3","control_node":null,"references":["prod1"]}}\\n'
              else
                printf '{{"node":"%s","control_node":null,"references":[]}}\\n' "$2"
              fi
            }}
            printf 'MOX2<%s>\\n' "$(removal_blockers mox2)"
            removal_blockers mox3
            """
        completed = self.run_bash(body)
        self.assertIn("MOX2<>", completed.stdout)
        for expected in (
            "registry record prod1 references mox3",
            "guest 300 (other, qemu) is on mox3",
            "HA rule production-prod1-100 includes mox3",
            "replication job 100-0 targets mox3",
        ):
            self.assertIn(expected, completed.stdout)

    def build_purge_plan(
        self, resources: list, vms: list, *, cleanup: list | None = None,
        rules: list | None = None, replication: list | None = None,
    ) -> tuple[subprocess.CompletedProcess[str], dict]:
        directory = Path(tempfile.mkdtemp())
        self.addCleanup(lambda: subprocess.run(["rm", "-rf", str(directory)], check=False))
        for name, value in (
            ("resources", resources),
            ("cleanup", cleanup or []),
            ("vms", vms),
            ("rules", rules if rules is not None else [
                {"rule": "production-prod1-100", "nodes": "mox1:1,mox2:1", "resources": "vm:100"},
            ]),
            ("replication", replication if replication is not None else [
                {"id": "100-0", "guest": 100, "target": "mox2"},
            ]),
        ):
            (directory / f"{name}.json").write_text(json.dumps(value), encoding="utf-8")
        completed = self.run_bash(
            f"""
            PURGE_HOST_SOURCE_ONLY=1 source {shlex.quote(str(PURGE_SCRIPT))}
            build_plan {shlex.quote(str(directory))} mox2 'mox1 mox3'
            """
        )
        plan = json.loads((directory / "plan.json").read_text(encoding="utf-8"))
        return completed, plan

    def test_purge_plan_narrows_production_and_destroys_dependent_staging(self) -> None:
        resources = [
            production("prod1", 100, ["mox1", "mox2"], "mox2"),
            production("prod2", 101, ["mox1", "mox3"], "mox1"),
            staging("stage1prod1", 200, "mox2", "prod1", 100),
            staging("stage2prod2", 201, "mox3", "prod2", 101),
        ]
        vms = [
            {"vmid": 100, "type": "qemu", "node": "mox1", "name": "prod1", "status": "running"},
            {"vmid": 101, "type": "qemu", "node": "mox1", "name": "prod2", "status": "running"},
            {"vmid": 200, "type": "qemu", "node": "mox2", "name": "stage1prod1"},
            {"vmid": 9112, "type": "lxc", "node": "mox2", "name": "haproxy2"},
        ]
        cleanup = [
            {"id": "c1", "node": "mox2", "state": "pending"},
            {"id": "c2", "node": "mox2", "state": "completed"},
            {"id": "c3", "node": "mox1", "state": "pending"},
        ]
        completed, plan = self.build_purge_plan(resources, vms, cleanup=cleanup)
        self.assertEqual(completed.stdout, "")
        self.assertEqual([row["name"] for row in plan["productions"]], ["prod1"])
        prod1 = plan["productions"][0]
        self.assertEqual(prod1["placement"], ["mox1"])
        self.assertEqual(prod1["owner"], "mox1")
        self.assertEqual(prod1["registry_owner"], "mox2")
        self.assertEqual(prod1["replication_targets"], [])
        self.assertEqual(prod1["dead_jobs"], ["100-0"])
        self.assertEqual(prod1["rule"], "production-prod1-100")
        self.assertEqual([row["name"] for row in plan["staging"]], ["stage1prod1"])
        self.assertEqual(plan["staging"][0]["source_placement"], ["mox1", "mox2"])
        self.assertEqual(plan["abandoned_cleanup"], 1)

    def test_purge_refuses_unrecovered_production_and_unregistered_guests(self) -> None:
        resources = [production("prod1", 100, ["mox1", "mox2"], "mox2")]
        vms = [
            {"vmid": 100, "type": "qemu", "node": "mox2", "name": "prod1", "status": "running"},
            {"vmid": 555, "type": "qemu", "node": "mox2", "name": "hand-made"},
        ]
        completed, plan = self.build_purge_plan(resources, vms)
        self.assertIn("prod1 is still on mox2; wait for Proxmox HA", completed.stdout)
        self.assertIn("unregistered guests are configured on mox2: 555", completed.stdout)
        self.assertEqual(plan["productions"], [])

        only_dead = [production("prod1", 100, ["mox2"], "mox2")]
        completed, _ = self.build_purge_plan(only_dead, [])
        self.assertIn("placed only on mox2", completed.stdout)

    def test_purge_candidates_cover_offline_members_leftovers_and_joining_slots(self) -> None:
        completed = self.run_bash(
            f"""
            PURGE_HOST_SOURCE_ONLY=1 source {shlex.quote(str(PURGE_SCRIPT))}
            MEMBERS=(mox1 mox2 mox3)
            ONLINE_MEMBERS=(mox1 mox3)
            hm_registry() {{
              printf '[{{"node":"mox1","state":"member"}},{{"node":"mox5","state":"joining"}},{{"node":"mox6","state":"member"}}]\\n'
            }}
            hm_exec() {{ printf 'mox1\\nmox2\\nmox3\\nmox4\\n'; }}
            purge_candidates
            """
        )
        lines = completed.stdout.splitlines()
        self.assertEqual(
            lines,
            [
                "mox2 offline cluster member",
                "mox4 not a member; /etc/pve/nodes/mox4 remains",
                "mox5 reserved slot that never finished joining",
                "mox6 registry slot recorded for a host that is no longer a member",
            ],
        )

    def test_purge_warning_uses_the_required_wording(self) -> None:
        source = PURGE_SCRIPT.read_text(encoding="utf-8")
        for phrase in (
            "The purpose of this script is to enable the removal of a cluster host that is\n"
            "no longer functioning and has been physically disconnected from the cluster,\n"
            "permanently.",
            "The purged machine must never be allowed to communicate with the cluster via\n"
            "the network in any way after it has been purged from the cluster.",
        ):
            self.assertIn(phrase, source)
        self.assertIn('"PURGE ${TARGET_HOST}"', source)

    def test_purge_deletes_the_dead_member_before_removing_the_qdevice(self) -> None:
        source = PURGE_SCRIPT.read_text(encoding="utf-8")
        member = source[source.index("delete_dead_member() {") :]
        member = member[: member.index("\n}\n")]
        self.assertNotIn("qdevice", member.lower())
        self.assertIn('hm_delete_cluster_node "$TARGET_HOST" "$((${#MEMBERS[@]} - 1))"', member)
        main = source[source.index("main() {") :]
        self.assertLess(main.index("delete_dead_member"), main.index("finish_cluster_cleanup"))
        cleanup = source[source.index("finish_cluster_cleanup() {") :]
        self.assertIn('hm_reconcile_qdevice "${#MEMBERS[@]}"', cleanup[: cleanup.index("\n}\n")])

    def test_remove_keeps_the_documented_order(self) -> None:
        source = REMOVE_SCRIPT.read_text(encoding="utf-8")
        main = source[source.index("main() {") :]
        order = [
            "hm_verify_qdevice_access",
            "resolve_control_node",
            "validate_eligibility",
            "confirm_cloudflare",
            "choose_control_replacement",
            'hm_confirm_phrase',
            "switch_control_node",
            'hm_acquire_control_plane_lock "$HM_COORDINATOR"',
            "remove_qdevice_before_change",
            "power_off_target",
            "delete_target",
            "reconcile_after_removal",
            "release_slot",
        ]
        positions = [main.index(step) for step in order]
        self.assertEqual(positions, sorted(positions))


if __name__ == "__main__":
    unittest.main()
