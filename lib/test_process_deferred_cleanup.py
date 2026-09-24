#!/usr/bin/env python3

# Copyright (c) 2026 BEENTHERE VENTURES, INC.
# SPDX-License-Identifier: GPL-3.0-only

"""Mock tests for bounded deferred staging cleanup."""

from __future__ import annotations

import fcntl
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import unittest


# The config loader and the registry reject paths with symlinked components.
# macOS keeps TMPDIR under /var -> /private/var, so hand out resolved temp
# paths. A no-op where the temp directory is already a real path.
tempfile.tempdir = str(Path(tempfile.gettempdir()).resolve())

LIB_DIR = Path(__file__).resolve().parent
HELPER_SOURCE = LIB_DIR / "process_deferred_cleanup.sh"
REGISTRY_SOURCE = LIB_DIR / "cluster_registry.py"


# process_deferred_cleanup.sh only ever runs on a Proxmox host (flock, zfs).
# Gate on the platform so a Linux run can never silently skip it.
@unittest.skipUnless(
    sys.platform.startswith("linux"),
    "process_deferred_cleanup.sh is Proxmox-host only",
)
class DeferredCleanupTest(unittest.TestCase):
    def setUp(self) -> None:
        self.temporary = tempfile.TemporaryDirectory()
        self.root = Path(self.temporary.name)
        self.env_dir = self.root / "env"
        self.bin_dir = self.root / "bin"
        self.install_lib = self.root / "install" / "lib"
        self.lock_dir = self.root / "locks"
        self.reservation_dir = self.root / "reservations"
        self.work_dir = self.root / "work"
        self.state_dir = self.root / "registry"
        for path in (
            self.env_dir,
            self.bin_dir,
            self.install_lib,
            self.lock_dir,
            self.reservation_dir,
            self.work_dir,
        ):
            path.mkdir(parents=True)
        cluster = self.env_dir / "cluster.conf"
        cluster.write_text(
            "\n".join(
                (
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
                    "PRODUCTION_VM_TAG=custom-production",
                    "STAGING_VM_TAG=custom-staging",
                    "EVICTABLE_VM_TAG=custom-evictable",
                    "PURPOSE_TAG_PREFIX=purpose-",
                    f"CLUSTER_STATE_DIR={self.state_dir}",
                    "",
                )
            ),
            encoding="utf-8",
        )
        cluster.chmod(0o644)

        for source in (LIB_DIR / "config.sh", HELPER_SOURCE):
            shutil.copy2(source, self.install_lib / source.name)
        self.helper = self.install_lib / HELPER_SOURCE.name
        self.helper.chmod(0o755)
        registry_wrapper = self.install_lib / "cluster_registry.py"
        registry_wrapper.write_text(
            f"""#!/usr/bin/env python3
import json
import os
import sys
with open(os.environ["FAKE_ACTIONS"], "a", encoding="utf-8") as stream:
    stream.write(json.dumps(["registry", *sys.argv[1:]]) + "\\n")
os.execv({str(REGISTRY_SOURCE)!r}, [{str(REGISTRY_SOURCE)!r}, *sys.argv[1:]])
""",
            encoding="utf-8",
        )
        registry_wrapper.chmod(0o755)
        sync = self.install_lib / "sync_haproxy_routes.sh"
        sync.write_text(
            """#!/usr/bin/env bash
set -Eeuo pipefail
python3 - "$FAKE_ACTIONS" <<'PY'
import json
import sys
with open(sys.argv[1], "a", encoding="utf-8") as stream:
    stream.write(json.dumps(["haproxy-sync"]) + "\\n")
PY
[[ "${FAKE_HAPROXY_FAIL:-0}" != 1 ]]
""",
            encoding="utf-8",
        )
        sync.chmod(0o755)

        self.actions = self.root / "actions.jsonl"
        self.actions.write_text("", encoding="utf-8")
        self.fake_state = self.root / "fake-state.json"
        self.write_fake_state(
            {
                "nodes": {"mox1": "online", "mox2": "online"},
                "vms": {
                    "100": {
                        "name": "prod1",
                        "node": "mox1",
                        "status": "running",
                        "type": "qemu",
                    }
                },
                "snapshot_config": {
                    "stg-base-stage1prod1-260914T220000Z": (
                        "app-ha staging base for stage1prod1"
                    )
                },
                "zfs": {
                    "mox1": {
                        (
                            "rpool/data/vm-100-disk-0@"
                            "stg-base-stage1prod1-260914T220000Z"
                        ): {
                            "type": "snapshot",
                            "guid": "123456",
                            "clones": "-",
                        },
                        (
                            "rpool/data/vm-100-disk-1@"
                            "stg-base-stage1prod1-260914T220000Z"
                        ): {
                            "type": "snapshot",
                            "guid": "654321",
                            "clones": "-",
                        },
                    },
                    "mox2": {
                        (
                            "rpool/data/vm-100-disk-0@"
                            "stg-base-stage1prod1-260914T220000Z"
                        ): {
                            "type": "snapshot",
                            "guid": "123456",
                            "clones": "-",
                        },
                        (
                            "rpool/data/vm-100-disk-1@"
                            "stg-base-stage1prod1-260914T220000Z"
                        ): {
                            "type": "snapshot",
                            "guid": "654321",
                            "clones": "-",
                        },
                    },
                },
            }
        )
        fake_command = self.bin_dir / "fake-command"
        fake_command.write_text(
            r"""#!/usr/bin/env python3
import json
import os
from pathlib import Path
import sys

command = Path(sys.argv[0]).name
args = sys.argv[1:]
node = os.environ["APP_HA_LOCAL_NODE"]
state_path = Path(os.environ["FAKE_STATE"])
actions_path = Path(os.environ["FAKE_ACTIONS"])

def load():
    return json.loads(state_path.read_text(encoding="utf-8"))

def save(value):
    state_path.write_text(json.dumps(value, sort_keys=True), encoding="utf-8")

def record():
    with actions_path.open("a", encoding="utf-8") as stream:
        stream.write(json.dumps([command, node, *args]) + "\n")

state = load()
if command == "pvesh":
    operation, path = args[:2]
    if operation == "get" and path == "/nodes":
        print(json.dumps([
            {"node": name, "status": status}
            for name, status in state["nodes"].items()
        ]))
    elif operation == "get" and path == "/cluster/resources":
        print(json.dumps([
            {
                "vmid": int(vmid),
                "name": vm["name"],
                "node": vm["node"],
                "status": vm["status"],
                "type": vm["type"],
            }
            for vmid, vm in state["vms"].items()
        ]))
    elif operation == "get" and path.endswith("/qemu/100/snapshot"):
        print(json.dumps([
            {"name": name, "description": description}
            for name, description in state["snapshot_config"].items()
        ]))
    elif operation == "get" and path == "/cluster/ha/resources":
        print("[]")
    elif operation == "delete" and "/qemu/100/snapshot/" in path:
        snapshot = path.rsplit("/", 1)[1]
        if snapshot not in state["snapshot_config"]:
            raise SystemExit(1)
        record()
        del state["snapshot_config"][snapshot]
        for object_name in list(state["zfs"][node]):
            if object_name.endswith("@" + snapshot):
                del state["zfs"][node][object_name]
        save(state)
    elif operation == "get" and path == "/cluster/replication":
        print(json.dumps([{"id": "100-0", "guest": 100, "target": "mox2"}]))
    else:
        raise SystemExit(f"unexpected pvesh operation: {args}")
elif command == "pvesm":
    if args[0] != "path":
        raise SystemExit(f"unexpected pvesm operation: {args}")
    storage, name = args[1].split(":", 1)
    if storage != "local-zfs":
        raise SystemExit(1)
    print(f"/dev/zvol/rpool/data/{name}")
elif command == "zfs":
    operation = args[0]
    target = args[-1]
    objects = state["zfs"][node]
    if operation == "list":
        value = objects.get(target)
        if value is None:
            raise SystemExit(1)
        if "type" in args:
            print(value["type"])
        else:
            print(target)
    elif operation == "get":
        prop = args[-2]
        value = objects.get(target)
        if value is None or prop not in value:
            raise SystemExit(1)
        print(value[prop])
    elif operation == "destroy":
        if target not in objects:
            raise SystemExit(1)
        record()
        del objects[target]
        save(state)
    else:
        raise SystemExit(f"unexpected zfs operation: {args}")
elif command == "pvesr":
    if args[0] != "schedule-now":
        raise SystemExit(f"unexpected pvesr operation: {args}")
    record()
    for target_node, objects in state["zfs"].items():
        if target_node == node:
            continue
        for object_name in list(objects):
            snapshot = object_name.rsplit("@", 1)[-1]
            if snapshot not in state["snapshot_config"]:
                del objects[object_name]
    save(state)
elif command == "qm":
    if args[0] == "status" and args[1] not in state["vms"]:
        raise SystemExit(1)
    raise SystemExit(f"unexpected qm operation: {args}")
elif command == "findmnt":
    raise SystemExit(1)
elif command == "fuser":
    raise SystemExit(1)
elif command == "lsblk":
    raise SystemExit(0)
elif command == "logger":
    raise SystemExit(0)
else:
    raise SystemExit(f"unexpected fake command: {command} {args}")
""",
            encoding="utf-8",
        )
        fake_command.chmod(0o755)
        for command in (
            "findmnt",
            "fuser",
            "logger",
            "lsblk",
            "pvesh",
            "pvesm",
            "pvesr",
            "qm",
            "zfs",
        ):
            (self.bin_dir / command).symlink_to(fake_command)

        self.base_environment = os.environ.copy()
        self.base_environment.update(
            {
                "APP_HA_CLEANUP_TEST_MODE": "1",
                "APP_HA_LOCK_DIR": str(self.lock_dir),
                "APP_HA_RESERVATION_DIR": str(self.reservation_dir),
                "APP_HA_WORK_DIR": str(self.work_dir),
                "APP_HA_CONFIG_TEST_MODE": "1",
                "APP_HA_ENV_DIR": str(self.env_dir),
                "FAKE_STATE": str(self.fake_state),
                "FAKE_ACTIONS": str(self.actions),
                "PATH": f"{self.bin_dir}:{os.environ['PATH']}",
            }
        )
        self.create_cleanup_registry()

    def tearDown(self) -> None:
        self.temporary.cleanup()

    def write_fake_state(self, value: dict) -> None:
        self.fake_state.write_text(
            json.dumps(value, sort_keys=True), encoding="utf-8"
        )

    def read_fake_state(self) -> dict:
        return json.loads(self.fake_state.read_text(encoding="utf-8"))

    def registry(self, *arguments: str) -> dict | list:
        completed = subprocess.run(
            [str(REGISTRY_SOURCE), "--state-dir", str(self.state_dir), *arguments],
            text=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            check=False,
        )
        self.assertEqual(
            completed.returncode,
            0,
            msg=f"stdout:\n{completed.stdout}\nstderr:\n{completed.stderr}",
        )
        return json.loads(completed.stdout)

    def create_cleanup_registry(self) -> None:
        self.registry(
            "init",
            "--network",
            "10.213.0.0/24",
            "--guest-gateway",
            "10.213.0.10/24",
            "--mox-ip-start",
            "10.213.0.11",
            "--mox-ip-end",
            "10.213.0.20",
            "--haproxy-ip-start",
            "10.213.0.21",
            "--haproxy-ip-end",
            "10.213.0.30",
            "--production-ip-start",
            "10.213.0.31",
            "--production-ip-end",
            "10.213.0.50",
            "--staging-ip-start",
            "10.213.0.51",
            "--staging-ip-end",
            "10.213.0.200",
        )
        self.registry(
            "allocate-prod",
            "--vmid",
            "100",
            "--purpose",
            "myapp",
            "--primary-domain",
            "myapp.com",
            "--placement",
            "mox1,mox2",
            "--initial-node",
            "mox1",
        )
        self.registry(
            "update",
            "prod1",
            "--state",
            "provisioning",
            "--owner-node",
            "mox1",
            "--volume-id",
            "local-zfs:vm-100-disk-0",
        )
        self.registry(
            "update",
            "prod1",
            "--state",
            "stopped",
            "--ha-nodes",
            "mox1,mox2",
            "--replication-targets",
            "mox2",
        )
        self.registry(
            "allocate-staging",
            "--source",
            "prod1",
            "--vmid",
            "200",
            "--placement",
            "mox2",
            "--placement-limit",
            "5",
        )
        self.registry("update", "stage1prod1", "--state", "snapshotting")
        self.registry(
            "record-staging-snapshot-intent",
            "stage1prod1",
            "--snapshot-name",
            "stg-base-stage1prod1-260914T220000Z",
            "--snapshot-owner-node",
            "mox1",
            "--source-volume-id",
            "local-zfs:vm-100-disk-0",
            "--snapshotted-volume-id",
            "local-zfs:vm-100-disk-0",
            "--snapshotted-volume-id",
            "local-zfs:vm-100-disk-1",
        )
        self.registry(
            "record-staging-snapshot",
            "stage1prod1",
            "--snapshot-name",
            "stg-base-stage1prod1-260914T220000Z",
            "--snapshot-owner-node",
            "mox1",
            "--source-volume-id",
            "local-zfs:vm-100-disk-0",
            "--snapshot-guid",
            "mox1=123456",
            "--snapshot-guid",
            "mox2=123456",
            "--snapshot-volume-guid",
            "local-zfs:vm-100-disk-0,mox1=123456",
            "--snapshot-volume-guid",
            "local-zfs:vm-100-disk-0,mox2=123456",
            "--snapshot-volume-guid",
            "local-zfs:vm-100-disk-1,mox1=654321",
            "--snapshot-volume-guid",
            "local-zfs:vm-100-disk-1,mox2=654321",
        )
        self.registry("update", "stage1prod1", "--state", "cloning")
        self.registry(
            "update",
            "stage1prod1",
            "--state",
            "patching",
            "--owner-node",
            "mox2",
            "--volume-id",
            "local-zfs:vm-200-disk-0",
        )
        self.registry(
            "update",
            "stage1prod1",
            "--state",
            "stopped",
            "--routes-enabled",
        )
        self.registry("update", "stage1prod1", "--state", "ready")
        self.registry("update", "stage1prod1", "--state", "active")
        self.registry(
            "update",
            "stage1prod1",
            "--state",
            "stopping",
            "--routes-disabled",
        )
        self.registry(
            "defer-cleanup",
            "--resource",
            "stage1prod1",
            "--node",
            "mox1",
            "--action",
            "delete-snapshot",
            "--target",
            "vm-100@stg-base-stage1prod1-260914T220000Z",
            "--reason",
            "test cleanup",
        )
        self.registry(
            "defer-cleanup",
            "--resource",
            "stage1prod1",
            "--node",
            "mox2",
            "--action",
            "delete-snapshot",
            "--target",
            "vm-100@stg-base-stage1prod1-260914T220000Z",
            "--reason",
            "test cleanup",
        )
        self.registry(
            "defer-cleanup",
            "--resource",
            "stage1prod1",
            "--node",
            "mox1",
            "--action",
            "remove-route",
            "--target",
            "stage1prod1",
            "--reason",
            "test route cleanup",
        )
        self.registry(
            "defer-cleanup",
            "--resource",
            "stage1prod1",
            "--node",
            "mox2",
            "--action",
            "remove-route",
            "--target",
            "stage1prod1",
            "--reason",
            "test route cleanup",
        )
        self.registry(
            "update",
            "stage1prod1",
            "--state",
            "cleanup_pending",
            "--clear-owner-node",
            "--clear-volume",
        )

    def run_helper(
        self, node: str, *, expected: int = 0
    ) -> subprocess.CompletedProcess[str]:
        environment = self.base_environment.copy()
        environment["APP_HA_LOCAL_NODE"] = node
        completed = subprocess.run(
            [str(self.helper), "--max-seconds", "30"],
            env=environment,
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
        return completed

    def action_rows(self) -> list[list[str]]:
        return [
            json.loads(line)
            for line in self.actions.read_text(encoding="utf-8").splitlines()
            if line
        ]

    def test_reachable_snapshot_and_route_cleanup_then_remote_finalize(self) -> None:
        self.run_helper("mox1")
        cleanup = self.registry("list", "--record-type", "cleanup")
        states = {(row["node"], row["action"]): row["state"] for row in cleanup}
        self.assertEqual(states[("mox1", "delete-snapshot")], "completed")
        self.assertEqual(states[("mox1", "remove-route")], "completed")
        self.assertEqual(states[("mox2", "delete-snapshot")], "pending")
        self.registry("get", "stage1prod1")

        rows = self.action_rows()
        pve_delete = next(
            index
            for index, row in enumerate(rows)
            if row[:3] == ["pvesh", "mox1", "delete"]
        )
        replication_cleanup = next(
            index
            for index, row in enumerate(rows)
            if row[:3] == ["pvesr", "mox1", "schedule-now"]
        )
        route_sync = next(
            index for index, row in enumerate(rows) if row == ["haproxy-sync"]
        )
        self.assertLess(route_sync, pve_delete)
        self.assertLess(pve_delete, replication_cleanup)
        self.assertFalse(any(row[0] == "zfs" and "destroy" in row for row in rows))

        self.run_helper("mox2")
        missing = subprocess.run(
            [
                str(REGISTRY_SOURCE),
                "--state-dir",
                str(self.state_dir),
                "get",
                "stage1prod1",
            ],
            text=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            check=False,
        )
        self.assertEqual(missing.returncode, 1)
        self.assertIn("does not exist", missing.stderr)
        state = self.read_fake_state()
        for node in ("mox1", "mox2"):
            self.assertEqual(state["zfs"][node], {})
        self.assertEqual(state["snapshot_config"], {})

    def test_offline_source_leaves_snapshot_pending(self) -> None:
        state = self.read_fake_state()
        state["nodes"]["mox1"] = "offline"
        self.write_fake_state(state)
        completed = self.run_helper("mox2")
        self.assertIn("production source prod1 is offline", completed.stderr)
        cleanup = self.registry("list", "--record-type", "cleanup")
        target = next(
            row
            for row in cleanup
            if row["node"] == "mox2" and row["action"] == "delete-snapshot"
        )
        self.assertEqual(target["state"], "pending")
        self.assertFalse(
            any(row[:3] == ["zfs", "mox2", "destroy"] for row in self.action_rows())
        )

    def test_lifecycle_lock_does_not_block_route_or_snapshot_convergence(self) -> None:
        lock_path = self.lock_dir / "app-ha-guest-role-hook.lock"
        with lock_path.open("w", encoding="utf-8") as lock:
            fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
            self.run_helper("mox1")
        cleanup = self.registry("list", "--record-type", "cleanup")
        states = {
            (row["node"], row["action"]): row["state"] for row in cleanup
        }
        self.assertEqual(states[("mox1", "remove-route")], "completed")
        self.assertEqual(states[("mox1", "delete-snapshot")], "completed")

    def test_failed_haproxy_sync_keeps_route_and_resource_pending(self) -> None:
        self.base_environment["FAKE_HAPROXY_FAIL"] = "1"
        self.run_helper("mox1")
        cleanup = self.registry("list", "--record-type", "cleanup")
        route = next(
            row
            for row in cleanup
            if row["node"] == "mox1" and row["action"] == "remove-route"
        )
        self.assertEqual(route["state"], "pending")
        self.assertEqual(self.registry("get", "stage1prod1")["state"], "cleanup_pending")

    def test_guid_mismatch_is_never_destroyed(self) -> None:
        state = self.read_fake_state()
        snapshot = next(iter(state["zfs"]["mox1"]))
        state["zfs"]["mox1"][snapshot]["guid"] = "999999"
        self.write_fake_state(state)
        completed = self.run_helper("mox1")
        self.assertIn("snapshot GUID differs", completed.stderr)
        self.assertIn(snapshot, self.read_fake_state()["zfs"]["mox1"])
        cleanup = self.registry("list", "--record-type", "cleanup")
        target = next(
            row
            for row in cleanup
            if row["node"] == "mox1" and row["action"] == "delete-snapshot"
        )
        self.assertEqual(target["state"], "pending")
        self.assertFalse(
            any(row[:3] == ["pvesh", "mox1", "delete"] for row in self.action_rows())
        )
        self.assertIn(
            "stg-base-stage1prod1-260914T220000Z",
            self.read_fake_state()["snapshot_config"],
        )

    def test_first_host_without_registry_is_safe_noop(self) -> None:
        shutil.rmtree(self.state_dir)
        completed = self.run_helper("mox1")
        self.assertIn("registry is not initialized", completed.stderr)
        self.assertEqual(self.action_rows(), [])

    def test_completed_records_finalize_after_worker_restart(self) -> None:
        cleanup = self.registry("list", "--record-type", "cleanup")
        completed = subprocess.run(
            [
                str(REGISTRY_SOURCE),
                "--state-dir",
                str(self.state_dir),
                "reconcile",
                "--observed",
                "-",
                "--apply",
            ],
            input=json.dumps(
                {
                    "schema_version": 1,
                    "cleanup_completed": [row["id"] for row in cleanup],
                }
            ),
            text=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            check=False,
        )
        self.assertEqual(completed.returncode, 0, completed.stderr)
        state = self.read_fake_state()
        state["snapshot_config"] = {}
        state["zfs"] = {"mox1": {}, "mox2": {}}
        self.write_fake_state(state)
        self.run_helper("mox1")
        missing = subprocess.run(
            [
                str(REGISTRY_SOURCE),
                "--state-dir",
                str(self.state_dir),
                "get",
                "stage1prod1",
            ],
            text=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            check=False,
        )
        self.assertEqual(missing.returncode, 1)

    def test_shell_syntax(self) -> None:
        completed = subprocess.run(
            ["bash", "-n", str(HELPER_SOURCE)],
            text=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            check=False,
        )
        self.assertEqual(completed.returncode, 0, completed.stderr)


if __name__ == "__main__":
    unittest.main()
