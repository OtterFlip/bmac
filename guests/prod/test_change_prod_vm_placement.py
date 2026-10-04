#!/usr/bin/env python3

# Copyright (c) 2026 BEENTHERE VENTURES, INC.
# SPDX-License-Identifier: GPL-3.0-only

"""Mock-friendly tests for production placement and owner changes."""

from __future__ import annotations

from pathlib import Path
import shlex
import subprocess
import tempfile
import textwrap
import unittest


tempfile.tempdir = str(Path(tempfile.gettempdir()).resolve())

PROD_DIR = Path(__file__).resolve().parent
PLACEMENT_SCRIPT = PROD_DIR / "change_prod_vm_placement.sh"
OWNER_SCRIPT = PROD_DIR / "change_prod_vm_owner.sh"

# Live state of prod1 (VMID 100) placed on mox1,mox2 and running on mox1, with
# every remote command recorded instead of executed.
STUBS = r"""
RUN_DIR="$(mktemp -d)"
trap 'rm -rf "$RUN_DIR"' EXIT
CALLS="${RUN_DIR}/calls"
: >"$CALLS"
RESOURCE_NAME=prod1
VMID=100
OWNER_NODE=mox1
REGISTRY_OWNER=mox1
COORDINATOR=mox1
HA_RULE_ID=production-prod1-100
PROD_VM_STORAGE=local-zfs
PROD_VM_REPLICATION_INTERVAL='*/5'
REPLICATION_SCHEDULE='*/5'
CLUSTER_NODES=(mox1 mox2 mox3)
ONLINE_NODES=(mox1 mox2 mox3)
PLACEMENT_NODES=(mox1 mox2)
REGISTRY_HA_NODES=(mox1 mox2)
REGISTRY_TARGETS=(mox2)
RULE_NODES=(mox1 mox2)
REPLICATION_JOBS=(100-0)
declare -A REPLICATION_TARGET=([100-0]=mox2)
node_exec() { printf 'node_exec %s\n' "$*" >>"$CALLS"; }
registry_update_resource() { printf 'registry %s\n' "$*" >>"$CALLS"; }
wait_for_replication() { printf 'wait %s\n' "$*" >>"$CALLS"; }
replication_job_state() { printf 'idle 1700000000\n'; }
pvesh_get() { printf '[]\n'; }
prod_acquire_lease() { printf 'lease\n' >>"$CALLS"; }
prod_release_lease() { printf 'release\n' >>"$CALLS"; }
prod_require_unchanged() { :; }
"""


class PlacementTest(unittest.TestCase):
    def run_script(
        self, script: Path, guard: str, body: str, expected: int = 0, stdin: str = ""
    ) -> subprocess.CompletedProcess[str]:
        completed = subprocess.run(
            [
                "bash",
                "-c",
                f"{guard}=1 source {shlex.quote(str(script))}\n"
                + STUBS
                + textwrap.dedent(body),
            ],
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

    def placement(self, body: str, **kwargs) -> subprocess.CompletedProcess[str]:
        return self.run_script(
            PLACEMENT_SCRIPT, "CHANGE_PROD_PLACEMENT_SOURCE_ONLY", body, **kwargs
        )

    def owner(self, body: str, **kwargs) -> subprocess.CompletedProcess[str]:
        return self.run_script(OWNER_SCRIPT, "CHANGE_PROD_OWNER_SOURCE_ONLY", body, **kwargs)

    def test_node_lists_are_parsed_sorted_and_restricted(self) -> None:
        completed = self.placement(
            """
            parse_node_list 'mox10, mox3' mox3 mox10 mox4
            ! parse_node_list 'mox3,mox3' mox3 2>/dev/null
            ! parse_node_list 'mox5' mox3 2>/dev/null
            ! parse_node_list '  ' mox3 2>/dev/null
            """
        )
        self.assertEqual(completed.stdout.split(), ["mox3", "mox10"])

    def test_add_requires_count_room_and_ten_percent_pool_reserve(self) -> None:
        completed = self.placement(
            """
            POOL_NAME=rpool
            HOOK_REF=local:snippets/app-ha-guest-role.sh
            production_limit_on() { printf '2\\n'; }
            production_count_on() { [[ "$1" == mox4 ]] && printf '2\\n' || printf '1\\n'; }
            node_exec() { printf '1000\\t300\\t0\\n'; }
            ONLINE_NODES=(mox1 mox2 mox3 mox4)
            REPLICA_BYTES=150
            printf 'A<%s>\\n' "$(add_blocker mox3)"
            REPLICA_BYTES=250
            printf 'B<%s>\\n' "$(add_blocker mox3)"
            printf 'C<%s>\\n' "$(add_blocker mox4)"
            printf 'D<%s>\\n' "$(add_blocker mox5)"
            node_exec() { printf '1000\\t300\\t1\\n'; }
            REPLICA_BYTES=10
            printf 'E<%s>\\n' "$(add_blocker mox3)"
            """
        )
        out = completed.stdout
        self.assertIn("A<OK 50>", out)
        self.assertIn(
            "B<pool rpool: 300 bytes available, replica needs 250 bytes, 10% reserve is 100 bytes>",
            out,
        )
        self.assertIn("C<already holds 2 of 2 production VMs", out)
        self.assertIn("D<offline>", out)
        self.assertIn("E<already has 1 volume(s) for VMID 100", out)

    def test_add_replicates_before_ha_may_use_the_host(self) -> None:
        completed = self.placement(
            """
            add_hosts mox3
            cat "$CALLS"
            printf 'RULE=%s\\n' "${RULE_NODES[*]}"
            """
        )
        calls = completed.stdout.splitlines()
        registry = next(i for i, c in enumerate(calls) if c.startswith("registry --placement mox1,mox2,mox3"))
        create = next(i for i, c in enumerate(calls) if "pvesr create-local-job 100-1 mox3 --schedule */5" in c)
        wait = next(i for i, c in enumerate(calls) if c.startswith("wait mox1 100-1 mox3 0"))
        rule = next(i for i, c in enumerate(calls) if "rules set node-affinity production-prod1-100 --nodes mox1:1,mox2:1,mox3:1" in c)
        self.assertLess(registry, create)
        self.assertLess(create, wait)
        self.assertLess(wait, rule)
        self.assertIn("node_exec mox1 pvesr create-local-job", calls[create])
        self.assertIn("--replication-targets mox2,mox3", calls[registry])
        self.assertIn("RULE=mox1 mox2 mox3", calls)

    def test_remove_stops_ha_first_and_never_removes_the_owner(self) -> None:
        completed = self.placement(
            """
            PLACEMENT_NODES=(mox1 mox2 mox3)
            REGISTRY_HA_NODES=(mox1 mox2 mox3)
            RULE_NODES=(mox1 mox2 mox3)
            REPLICATION_JOBS=(100-0 100-1)
            REPLICATION_TARGET[100-1]=mox3
            remove_hosts mox2
            cat "$CALLS"
            printf 'JOBS=%s\\n' "${REPLICATION_JOBS[*]}"
            ( remove_hosts mox1 ) 2>&1 | grep -o 'cannot be removed' || true
            """
        )
        calls = completed.stdout.splitlines()
        rule = next(i for i, c in enumerate(calls) if "--nodes mox1:1,mox3:1" in c)
        registry = next(i for i, c in enumerate(calls) if c.startswith("registry --placement mox1,mox3"))
        delete = next(i for i, c in enumerate(calls) if "pvesr delete 100-0" in c)
        self.assertLess(rule, registry)
        self.assertLess(registry, delete)
        self.assertIn("--replication-targets mox3", calls[registry])
        self.assertIn("JOBS=100-1", calls)
        self.assertIn("cannot be removed", calls)

    def test_remove_keeps_at_least_two_hosts(self) -> None:
        completed = self.placement(
            "choose_remove_hosts\n", expected=1
        )
        self.assertIn("at least 2 must remain", completed.stderr)
        completed = self.placement(
            """
            PLACEMENT_NODES=(mox1 mox2 mox3)
            choose_remove_hosts
            printf 'REMOVE=%s\\n' "${REQUESTED_NODES[*]}"
            """,
            stdin="mox1\nmox2,mox3\nmox3\n",
        )
        self.assertIn("not offered: mox1", completed.stderr)
        self.assertIn("At least 2 placement hosts must remain", completed.stderr)
        self.assertIn("REMOVE=mox3", completed.stdout)

    def test_consistent_state_needs_no_repair_and_partial_add_is_finished(self) -> None:
        clean = self.placement("repair_partial_change\ncat \"$CALLS\"\n")
        self.assertEqual(clean.stdout, "")
        partial = self.placement(
            """
            PLACEMENT_NODES=(mox1 mox2 mox3)
            REGISTRY_HA_NODES=(mox1 mox2 mox3)
            REGISTRY_TARGETS=(mox2 mox3)
            repair_partial_change
            cat "$CALLS"
            """,
            stdin="GO\na\n",
        )
        self.assertIn("Registry placement includes mox3, but the HA rule does not", partial.stdout)
        self.assertIn("pvesr create-local-job 100-1 mox3", partial.stdout)
        self.assertIn("--nodes mox1:1,mox2:1,mox3:1", partial.stdout)
        self.assertTrue(partial.stdout.splitlines()[-1] == "release")

    def test_repair_refuses_rule_hosts_outside_the_registry_placement(self) -> None:
        completed = self.placement(
            "RULE_NODES=(mox1 mox2 mox3)\nrepair_partial_change\n", expected=1
        )
        self.assertIn("outside the registry placement", completed.stderr)

    def test_owner_change_requires_rule_jobs_and_placement_to_agree(self) -> None:
        self.owner("validate_strict_layout\n")
        mismatch = self.owner(
            "RULE_NODES=(mox1 mox2 mox3)\nvalidate_strict_layout\n", expected=1
        )
        self.assertIn("finish the placement change", mismatch.stderr)
        offline = self.owner("ONLINE_NODES=(mox1)\nvalidate_strict_layout\n", expected=1)
        self.assertIn("mox2 is offline", offline.stderr)
        stale = self.owner(
            "REGISTRY_TARGETS=()\nvalidate_strict_layout\ncat \"$CALLS\"\n"
        )
        self.assertIn("registry --replication-targets mox2", stale.stdout)

    def test_owner_records_new_owner_and_reversed_targets(self) -> None:
        completed = self.owner(
            """
            OWNER_NODE=mox2
            record_registry_owner
            cat "$CALLS"
            """
        )
        self.assertIn("registry --owner-node mox2 --replication-targets mox1", completed.stdout)
        source = OWNER_SCRIPT.read_text(encoding="utf-8")
        self.assertIn('ha-manager relocate "vm:${VMID}" "$TARGET_NODE"', source)


if __name__ == "__main__":
    unittest.main()
