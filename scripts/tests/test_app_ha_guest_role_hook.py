#!/usr/bin/env python3

# Copyright (c) 2026 BEENTHERE VENTURES, INC.
# SPDX-License-Identifier: GPL-3.0-only

"""Mock tests for the destructive app-ha QEMU lifecycle hook."""

from __future__ import annotations

import fcntl
import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import unittest


HOSTS_DIR = Path(__file__).resolve().parent
LIB_DIR = HOSTS_DIR.parent / "lib"
HOOK = HOSTS_DIR / "app-ha-guest-role-hook.sh"
REGISTRY = LIB_DIR / "cluster_registry.py"
CLEANUP_WORKER = LIB_DIR / "process_deferred_cleanup.sh"


class GuestRoleHookTest(unittest.TestCase):
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
                    "PROD_VM_BRIDGE=vmbr-private",
                    f"CLUSTER_STATE_DIR={self.state_dir}",
                    "",
                )
            ),
            encoding="utf-8",
        )
        cluster.chmod(0o644)
        shutil.copy2(LIB_DIR / "config.sh", self.install_lib / "config.sh")
        registry_wrapper = self.install_lib / "cluster_registry.py"
        registry_wrapper.write_text(
            f"""#!/usr/bin/env python3
import json
import os
import sys

with open(os.environ["FAKE_ACTIONS"], "a", encoding="utf-8") as stream:
    stream.write(json.dumps(["registry", *sys.argv[1:]]) + "\\n")
os.execv({str(REGISTRY)!r}, [{str(REGISTRY)!r}, *sys.argv[1:]])
""",
            encoding="utf-8",
        )
        registry_wrapper.chmod(0o755)
        # The real deferred-cleanup worker runs against the same fakes so the
        # tests can prove when queued staging destruction actually happens.
        self.worker = self.install_lib / CLEANUP_WORKER.name
        shutil.copy2(CLEANUP_WORKER, self.worker)
        self.worker.chmod(0o755)
        route_sync = self.install_lib / "sync_haproxy_routes.sh"
        route_sync.write_text(
            "#!/usr/bin/env bash\n"
            "exec python3 -c 'import json,os; "
            'open(os.environ["FAKE_ACTIONS"], "a").write('
            'json.dumps(["haproxy-sync"]) + "\\n")\'\n',
            encoding="utf-8",
        )
        route_sync.chmod(0o755)

        self.actions = self.root / "actions.jsonl"
        self.actions.write_text("", encoding="utf-8")
        self.fake_state = self.root / "fake-state.json"
        self.write_fake_state(
            {
                "nodes": {"mox1": "online", "mox2": "online"},
                "ha": ["vm:100"],
                "vms": {
                    "100": {
                        "node": "mox1",
                        "status": "stopped",
                        "config": {
                            "name": "prod1",
                            "tags": "custom-production;purpose-myapp",
                            "onboot": "0",
                        },
                    }
                },
                "zfs": {},
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
state_path = Path(os.environ["FAKE_STATE"])
actions_path = Path(os.environ["FAKE_ACTIONS"])

def load():
    return json.loads(state_path.read_text(encoding="utf-8"))

def save(value):
    state_path.write_text(json.dumps(value, sort_keys=True), encoding="utf-8")

def record():
    with actions_path.open("a", encoding="utf-8") as stream:
        stream.write(json.dumps([command, *args]) + "\n")

state = load()

if command == "qm":
    operation = args[0]
    vmid = args[1]
    vm = state["vms"].get(vmid)
    if operation == "config":
        if vm is None:
            raise SystemExit(1)
        for key, value in vm["config"].items():
            print(f"{key}: {value}")
    elif operation == "status":
        if vm is None:
            raise SystemExit(1)
        print(f"status: {vm['status']}")
    elif operation == "stop":
        if vm is None:
            raise SystemExit(1)
        record()
        vm["status"] = "stopped"
        save(state)
    elif operation == "destroy":
        if vm is None:
            raise SystemExit(1)
        record()
        del state["vms"][vmid]
        save(state)
    else:
        raise SystemExit(f"unexpected qm operation: {args}")
elif command == "pvesh":
    operation = args[0]
    path = args[1]
    if operation != "get":
        raise SystemExit(f"unexpected pvesh operation: {args}")
    if path == "/nodes/mox1/qemu":
        print(json.dumps([
            {"vmid": int(vmid), "type": "qemu"}
            for vmid, vm in state["vms"].items()
            if vm["node"] == "mox1"
        ]))
    elif path == "/cluster/resources":
        print(json.dumps([
            {
                "vmid": int(vmid),
                "type": "qemu",
                "node": vm["node"],
                "name": vm["config"]["name"],
                "status": vm["status"],
            }
            for vmid, vm in state["vms"].items()
        ]))
    elif path == "/cluster/ha/resources":
        print(json.dumps([{"sid": sid} for sid in state["ha"]]))
    elif path == "/nodes":
        print(json.dumps([
            {"node": node, "status": status}
            for node, status in state["nodes"].items()
        ]))
    else:
        raise SystemExit(f"unexpected pvesh path: {path}")
elif command == "pvesm":
    if args[0] != "path":
        raise SystemExit(f"unexpected pvesm operation: {args}")
    volume = args[1]
    storage, name = volume.split(":", 1)
    if storage != "local-zfs":
        raise SystemExit(1)
    print(f"/dev/zvol/rpool/data/{name}")
elif command == "zfs":
    operation = args[0]
    target = args[-1]
    if operation == "list":
        value = state["zfs"].get(target)
        if value is None:
            raise SystemExit(1)
        if "type" in args:
            print(value["type"])
        else:
            print(target)
    elif operation == "get":
        prop = args[-2]
        value = state["zfs"].get(target)
        if value is None or prop not in value:
            raise SystemExit(1)
        print(value[prop])
    elif operation == "destroy":
        if target not in state["zfs"]:
            raise SystemExit(1)
        record()
        del state["zfs"][target]
        snapshot = target.split("@", 1)[0]
        for value in state["zfs"].values():
            if value.get("clones") == target:
                value["clones"] = "-"
        save(state)
    else:
        raise SystemExit(f"unexpected zfs operation: {args}")
elif command == "findmnt":
    raise SystemExit(1)
elif command == "fuser":
    raise SystemExit(1)
elif command == "lsblk":
    raise SystemExit(0)
elif command in {"logger", "pvesr", "systemctl"}:
    record()
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
            "systemctl",
            "zfs",
        ):
            (self.bin_dir / command).symlink_to(fake_command)

        self.environment = os.environ.copy()
        self.environment.update(
            {
                "APP_HA_HOOK_TEST_MODE": "1",
                "APP_HA_CLEANUP_TEST_MODE": "1",
                "APP_HA_INSTALL_LIB": str(self.install_lib),
                "APP_HA_LOCK_DIR": str(self.lock_dir),
                "APP_HA_RESERVATION_DIR": str(self.reservation_dir),
                "APP_HA_WORK_DIR": str(self.work_dir),
                "APP_HA_LOCAL_NODE": "mox1",
                "APP_HA_CONFIG_TEST_MODE": "1",
                "APP_HA_ENV_DIR": str(self.env_dir),
                "FAKE_STATE": str(self.fake_state),
                "FAKE_ACTIONS": str(self.actions),
                "PATH": f"{self.bin_dir}:{os.environ['PATH']}",
            }
        )
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
            "mox2",
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
        production = self.registry("get", "prod1")
        fake_state = self.read_fake_state()
        fake_state["vms"]["100"]["config"].update(
            {
                "net0": (
                    f"virtio={production['mac']},bridge=vmbr-private,firewall=1"
                ),
                "scsi0": "local-zfs:vm-100-disk-0,replicate=1,size=128G",
            }
        )
        self.write_fake_state(fake_state)

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
            [str(REGISTRY), "--state-dir", str(self.state_dir), *arguments],
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

    def run_hook(
        self, vmid: int, phase: str, *, expected: int = 0
    ) -> subprocess.CompletedProcess[str]:
        completed = subprocess.run(
            [str(HOOK), str(vmid), phase],
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
        return completed

    def run_worker(self) -> subprocess.CompletedProcess[str]:
        completed = subprocess.run(
            [str(self.worker), "--max-seconds", "30"],
            env=self.environment,
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
        return completed

    def action_rows(self) -> list[list[str]]:
        return [
            json.loads(line)
            for line in self.actions.read_text(encoding="utf-8").splitlines()
            if line
        ]

    def test_pre_stop_is_lock_free_for_force_stop_cleanup(self) -> None:
        lock_path = self.lock_dir / "app-ha-guest-role-hook.lock"
        with lock_path.open("w", encoding="utf-8") as lock:
            fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
            completed = subprocess.run(
                [str(HOOK), "200", "pre-stop"],
                env=self.environment,
                text=True,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                check=False,
                timeout=2,
            )
        self.assertEqual(
            completed.returncode,
            0,
            msg=f"stdout:\n{completed.stdout}\nstderr:\n{completed.stderr}",
        )

    def add_registered_staging(self, index: int = 1) -> None:
        vmid = 199 + index
        name = f"stage{index}prod1"
        snapshot_name = f"stg-base-{name}-260914T220000Z"
        guid = str(123455 + index)
        self.registry(
            "allocate-staging",
            "--source",
            "prod1",
            "--vmid",
            str(vmid),
            "--placement",
            "mox1",
            "--placement-limit",
            "5",
        )
        self.registry("update", name, "--state", "snapshotting")
        self.registry(
            "record-staging-snapshot-intent",
            name,
            "--snapshot-name",
            snapshot_name,
            "--snapshot-owner-node",
            "mox2",
            "--source-volume-id",
            "local-zfs:vm-100-disk-0",
            "--snapshotted-volume-id",
            "local-zfs:vm-100-disk-0",
        )
        self.registry(
            "record-staging-snapshot",
            name,
            "--snapshot-name",
            snapshot_name,
            "--snapshot-owner-node",
            "mox2",
            "--source-volume-id",
            "local-zfs:vm-100-disk-0",
            "--snapshot-guid",
            f"mox1={guid}",
            "--snapshot-guid",
            f"mox2={guid}",
            "--snapshot-volume-guid",
            f"local-zfs:vm-100-disk-0,mox1={guid}",
            "--snapshot-volume-guid",
            f"local-zfs:vm-100-disk-0,mox2={guid}",
        )
        self.registry("update", name, "--state", "cloning")
        self.registry(
            "update",
            name,
            "--state",
            "patching",
            "--owner-node",
            "mox1",
            "--volume-id",
            f"local-zfs:vm-{vmid}-disk-0",
        )
        self.registry(
            "update",
            name,
            "--state",
            "stopped",
            "--routes-enabled",
        )
        self.registry("update", name, "--state", "ready")
        self.registry("update", name, "--state", "active")
        state = self.read_fake_state()
        state["vms"][str(vmid)] = {
            "node": "mox1",
            "status": "running",
            "config": {
                "name": name,
                "tags": "custom-staging;custom-evictable;purpose-myapp",
                "template": "0",
                "onboot": "0",
                "scsi0": (
                    f"local-zfs:vm-{vmid}-disk-0,discard=on,iothread=1,"
                    "replicate=0"
                ),
                "efidisk0": (
                    f"local-zfs:vm-{vmid}-disk-1,efitype=4m,"
                    "pre-enrolled-keys=1,size=4M"
                ),
            },
        }
        snapshot = f"rpool/data/vm-100-disk-0@{snapshot_name}"
        clone = f"rpool/data/vm-{vmid}-disk-0"
        state["zfs"].update(
            {
                "rpool/data/vm-100-disk-0": {"type": "volume"},
                snapshot: {
                    "type": "snapshot",
                    "guid": guid,
                    "clones": clone,
                },
                clone: {
                    "type": "volume",
                    "origin": snapshot,
                },
            }
        )
        self.write_fake_state(state)

    def test_wrong_name_and_wrong_tags_are_never_destroyed(self) -> None:
        state = self.read_fake_state()
        state["vms"]["201"] = {
            "node": "mox1",
            "status": "running",
            "config": {
                "name": "not-stage2prod1",
                "tags": "custom-staging;custom-evictable",
            },
        }
        state["vms"]["202"] = {
            "node": "mox1",
            "status": "running",
            "config": {
                "name": "stage2prod1",
                "tags": "custom-staging",
            },
        }
        self.write_fake_state(state)
        self.run_hook(100, "pre-start")
        destructive = [
            row for row in self.action_rows() if row[0] in {"qm", "zfs"}
        ]
        self.assertEqual(destructive, [])
        self.assertTrue((self.reservation_dir / "100.reservation").exists())

    def test_production_without_registry_never_gets_destructive_authority(
        self,
    ) -> None:
        shutil.rmtree(self.state_dir)
        completed = self.run_hook(100, "pre-start", expected=1)
        self.assertIn("registry is not initialized", completed.stderr)
        self.assertFalse((self.reservation_dir / "100.reservation").exists())
        self.assertFalse(
            any(row[:2] == ["qm", "destroy"] for row in self.action_rows())
        )

    def test_ha_staging_blocks_before_any_destruction(self) -> None:
        self.add_registered_staging()
        state = self.read_fake_state()
        state["ha"] = ["vm:100", "vm:200"]
        self.write_fake_state(state)
        completed = self.run_hook(100, "pre-start", expected=1)
        self.assertIn("HA-managed", completed.stderr)
        self.assertTrue("200" in self.read_fake_state()["vms"])
        self.assertFalse((self.reservation_dir / "100.reservation").exists())
        self.assertFalse(
            any(row[:2] == ["qm", "stop"] for row in self.action_rows())
        )

    def test_registry_volume_mismatch_blocks_destruction(self) -> None:
        self.add_registered_staging()
        state = self.read_fake_state()
        state["vms"]["200"]["config"]["scsi0"] = (
            "local-zfs:vm-299-disk-0,replicate=0"
        )
        self.write_fake_state(state)
        completed = self.run_hook(100, "pre-start", expected=1)
        self.assertIn("registry identity", completed.stderr)
        self.assertTrue("200" in self.read_fake_state()["vms"])
        self.assertFalse(
            any(row[:2] == ["qm", "destroy"] for row in self.action_rows())
        )

    def test_auxiliary_disk_blocks_destruction(self) -> None:
        self.add_registered_staging()
        state = self.read_fake_state()
        state["vms"]["200"]["config"]["ide0"] = (
            "local-zfs:vm-200-disk-9,size=1G"
        )
        self.write_fake_state(state)
        completed = self.run_hook(100, "pre-start", expected=1)
        self.assertIn("registry identity", completed.stderr)
        self.assertIn("200", self.read_fake_state()["vms"])
        self.assertFalse(
            any(row[:2] == ["qm", "destroy"] for row in self.action_rows())
        )

    def test_non_ha_production_has_no_destructive_authority(self) -> None:
        state = self.read_fake_state()
        state["ha"] = []
        self.write_fake_state(state)
        completed = self.run_hook(100, "pre-start", expected=1)
        self.assertIn("not HA-managed", completed.stderr)
        self.assertFalse((self.reservation_dir / "100.reservation").exists())

    def test_stale_cleanup_bookkeeping_does_not_block_production_start(self) -> None:
        self.registry(
            "allocate-staging",
            "--source",
            "prod1",
            "--vmid",
            "201",
            "--placement",
            "mox1",
            "--placement-limit",
            "5",
        )
        self.registry(
            "defer-cleanup",
            "--resource",
            "stage1prod1",
            "--node",
            "mox1",
            "--action",
            "destroy-volume",
            "--target",
            "local-zfs:vm-201-disk-0",
            "--reason",
            "stale bookkeeping test",
        )
        self.run_hook(100, "pre-start")
        self.assertTrue((self.reservation_dir / "100.reservation").exists())

    def test_pre_start_stops_and_queues_destruction_for_after_start(self) -> None:
        self.add_registered_staging()
        state = self.read_fake_state()
        state["nodes"]["mox2"] = "offline"
        self.write_fake_state(state)

        self.run_hook(100, "pre-start")
        rows = self.action_rows()

        def first_index(predicate) -> int:
            return next(index for index, row in enumerate(rows) if predicate(row))

        stop_index = first_index(lambda row: row[:2] == ["qm", "stop"])
        route_index = first_index(
            lambda row: row[0] == "registry"
            and "update" in row
            and "--routes-disabled" in row
        )
        defer_index = first_index(
            lambda row: row[0] == "registry" and "defer-cleanup" in row
        )
        self.assertLess(stop_index, route_index)
        self.assertLess(route_index, defer_index)
        self.assertFalse(
            any(row[:2] in (["qm", "destroy"], ["zfs", "destroy"]) for row in rows),
            "pre-start must not destroy staging; that waits for post-start",
        )

        # The staging guest is off but intact while production starts.
        state = self.read_fake_state()
        self.assertEqual(state["vms"]["200"]["status"], "stopped")
        self.assertIn("rpool/data/vm-200-disk-0", state["zfs"])
        stage = self.registry("get", "stage1prod1")
        self.assertEqual(stage["state"], "cleanup_pending")
        self.assertFalse(stage["routes_enabled"])
        self.assertEqual(stage["owner_node"], "mox1")
        self.assertEqual(stage["proxmox"]["volume_id"], "local-zfs:vm-200-disk-0")
        cleanup = self.registry("list", "--record-type", "cleanup")
        local_payload = {
            (row["action"], row["target"], row["state"])
            for row in cleanup
            if row["node"] == "mox1"
            and row["action"] in {"destroy-vm", "destroy-volume"}
        }
        self.assertEqual(
            local_payload,
            {
                ("destroy-vm", "vm:200", "pending"),
                ("destroy-volume", "local-zfs:vm-200-disk-0", "pending"),
            },
        )
        pending_nodes = {
            row["node"]
            for row in cleanup
            if row["action"] == "delete-snapshot"
        }
        self.assertEqual(pending_nodes, {"mox1", "mox2"})
        route_nodes = {
            row["node"] for row in cleanup if row["action"] == "remove-route"
        }
        self.assertEqual(route_nodes, {"mox1", "mox2"})
        self.assertTrue((self.reservation_dir / "100.reservation").exists())

        # A timer-driven worker run before post-start leaves the guest alone.
        completed = self.run_worker()
        self.assertIn("a production start is pending on mox1", completed.stderr)
        state = self.read_fake_state()
        self.assertIn("200", state["vms"])
        self.assertIn("rpool/data/vm-200-disk-0", state["zfs"])

        self.run_hook(100, "post-start")
        self.assertFalse((self.reservation_dir / "100.reservation").exists())
        self.assertTrue(
            any(
                row[:4]
                == [
                    "systemctl",
                    "start",
                    "--no-block",
                    "app-ha-deferred-cleanup.service",
                ]
                for row in self.action_rows()
            )
        )

        # The run that post-start triggers destroys the config, then the clone.
        self.run_worker()
        state = self.read_fake_state()
        self.assertNotIn("200", state["vms"])
        self.assertNotIn("rpool/data/vm-200-disk-0", state["zfs"])
        rows = self.action_rows()
        config_index = first_index(lambda row: row[:3] == ["qm", "destroy", "200"])
        zvol_index = first_index(
            lambda row: row[:2] == ["zfs", "destroy"]
            and row[-1] == "rpool/data/vm-200-disk-0"
        )
        self.assertLess(config_index, zvol_index)

    def test_every_staging_guest_is_off_before_any_is_queued(self) -> None:
        self.add_registered_staging(1)
        self.add_registered_staging(2)

        self.run_hook(100, "pre-start")
        rows = self.action_rows()
        stops = [
            index for index, row in enumerate(rows) if row[:2] == ["qm", "stop"]
        ]
        self.assertEqual(sorted(rows[index][2] for index in stops), ["200", "201"])
        first_queue_step = next(
            index
            for index, row in enumerate(rows)
            if row[0] == "registry"
            and ("defer-cleanup" in row or "--routes-disabled" in row)
        )
        self.assertLess(max(stops), first_queue_step)
        state = self.read_fake_state()
        for vmid in ("200", "201"):
            self.assertEqual(state["vms"][vmid]["status"], "stopped")
        for name in ("stage1prod1", "stage2prod1"):
            self.assertEqual(self.registry("get", name)["state"], "cleanup_pending")

    def test_expired_reservation_does_not_defer_staging_destruction(self) -> None:
        self.add_registered_staging()
        self.run_hook(100, "pre-start")
        reservation = self.reservation_dir / "100.reservation"
        expired = reservation.stat().st_mtime - 901
        os.utime(reservation, (expired, expired))

        completed = self.run_worker()
        self.assertNotIn("a production start is pending", completed.stderr)
        state = self.read_fake_state()
        self.assertNotIn("200", state["vms"])
        self.assertNotIn("rpool/data/vm-200-disk-0", state["zfs"])

    def test_staging_refuses_a_production_reservation(self) -> None:
        self.run_hook(100, "pre-start")
        self.add_registered_staging()
        completed = self.run_hook(200, "pre-start", expected=1)
        self.assertIn("production VM 100 is running or reserved", completed.stderr)

    def test_shell_syntax(self) -> None:
        completed = subprocess.run(
            ["bash", "-n", str(HOOK)],
            text=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            check=False,
        )
        self.assertEqual(completed.returncode, 0, completed.stderr)


if __name__ == "__main__":
    unittest.main()
