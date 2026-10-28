#!/usr/bin/env python3

# Copyright (c) 2026 BEENTHERE VENTURES, INC.
# SPDX-License-Identifier: GPL-3.0-only

"""The deferred-cleanup worker and backup-linked staging VMs: evicting one
whose PBS mapping died, and destroying its VM, overlay, and mapping."""

from __future__ import annotations

import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import unittest


# The config loader and the registry reject paths with symlinked components.
tempfile.tempdir = str(Path(tempfile.gettempdir()).resolve())

SCRIPTS_DIR = Path(__file__).resolve().parent.parent
WORKER_SOURCE = SCRIPTS_DIR / "host_runtime" / "process_deferred_cleanup.sh"
REGISTRY_SOURCE = SCRIPTS_DIR / "host_runtime" / "cluster_registry.py"
SNAPSHOT = "vm/100/2026-10-27T16:04:23Z"
OVERLAY = "bmac-overlay:901/vm-901-disk-1.qcow2"

CLUSTER_CONF = """MAX_MOX_HOSTS=10
PROXMOX_INTERNAL_DOMAIN=pve.internal
PRIVATE_SUBNET_CIDR=10.213.0.0/24
PROXMOX_MIGRATION_NETWORK=10.213.0.240/28
DATACENTER_PRIVATE_VLAN_GATEWAY=10.213.0.1
GUEST_GATEWAY_IP=10.213.0.10
GUEST_GATEWAY_MAC=02:00:0a:d5:00:0a
MOX_IP_START=10.213.0.11
MOX_IP_END=10.213.0.20
MOX_REPLICATION_IP_START=10.213.0.241
MOX_REPLICATION_IP_END=10.213.0.250
VM_SLOT_START=10.213.0.110
MAX_PROD_VMS=10
MAX_STAGING_VMS_PER_PROD=9
CLOUDFLARE_ACCOUNT_ID=0123456789abcdef0123456789abcdef
CLOUDFLARE_TUNNEL_ID=00000000-0000-0000-0000-000000000000
PRODUCTION_VM_TAG=app-ha-production
STAGING_VM_TAG=app-ha-staging
EVICTABLE_VM_TAG=evictable
PURPOSE_TAG_PREFIX=purpose-
CLUSTER_STATE_DIR={state}
"""

# Fake host commands and a fake backup_link.sh, sharing one JSON state file.
FAKE = r'''#!/usr/bin/env python3
import json, os, sys
from pathlib import Path

command = Path(sys.argv[0]).name
args = sys.argv[1:]
state_path = Path(os.environ["FAKE_STATE"])
state = json.loads(state_path.read_text())

def save():
    state_path.write_text(json.dumps(state))

def record():
    state.setdefault("actions", []).append([command, *args])
    save()

def overlay_path(volume):
    storage, rest = volume.split(":", 1)
    return str(Path(os.environ["FAKE_OVERLAY_ROOT"]) / "images" / rest)

vms = state["vms"]
if command == "qm":
    vmid = args[1]
    if args[0] == "status":
        if vmid not in vms:
            sys.exit(2)
        print(f"status: {vms[vmid]['status']}")
    elif args[0] == "config":
        print("\n".join(f"{k}: {v}" for k, v in vms[vmid]["config"].items()))
    elif args[0] == "stop":
        record()
        vms[vmid]["status"] = "stopped"
        save()
    elif args[0] == "destroy":
        record()
        # --purge removes the disks the config referenced, the overlay among them.
        for value in vms[vmid]["config"].values():
            if value.split(",")[0].endswith(".qcow2"):
                Path(overlay_path(value.split(",")[0])).unlink(missing_ok=True)
        del vms[vmid]
        save()
    else:
        sys.exit(f"unexpected qm {args}")
elif command == "pvesh":
    if args[:2] == ["get", "/cluster/resources"]:
        print(json.dumps([{"vmid": int(k), "type": "qemu", "name": v["config"]["name"], "status": v["status"]}
                          for k, v in vms.items()]))
    elif args[:2] == ["get", "/cluster/ha/resources"]:
        print("[]")
    else:
        sys.exit(f"unexpected pvesh {args}")
elif command == "pvesm":
    if args[0] == "path":
        print(overlay_path(args[1]))
    elif args[0] == "free":
        record()
        Path(overlay_path(args[1])).unlink()
    else:
        sys.exit(f"unexpected pvesm {args}")
elif command == "backup_link.sh":
    if args[0] == "status":
        print(state["links"].get(args[1], "absent"))
    elif args[0] == "unmap":
        record()
        state["links"].pop(args[1], None)
        save()
    else:
        sys.exit(f"unexpected backup_link.sh {args}")
elif command in ("logger", "findmnt", "fuser", "lsblk", "zfs", "pvesr", "flock-unused"):
    sys.exit(0 if command == "logger" else 1)
else:
    sys.exit(f"unexpected fake command {command} {args}")
'''


@unittest.skipUnless(sys.platform.startswith("linux"), "process_deferred_cleanup.sh is Proxmox-host only")
class BackupLinkCleanupTest(unittest.TestCase):
    def setUp(self) -> None:
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        root = Path(self.temporary.name)
        self.root = root
        self.state_dir = root / "registry"
        for name in ("config", "bin", "lib", "locks", "reservations", "work", "overlay/images/901"):
            (root / name).mkdir(parents=True)
        (root / "config" / "cluster.conf").write_text(CLUSTER_CONF.format(state=self.state_dir))
        (root / "config" / "cluster.conf").chmod(0o644)
        shutil.copy2(SCRIPTS_DIR / "lib" / "config.sh", root / "lib" / "config.sh")
        shutil.copy2(WORKER_SOURCE, root / "lib" / WORKER_SOURCE.name)
        self.worker = root / "lib" / WORKER_SOURCE.name
        (root / "lib" / "cluster_registry.py").symlink_to(REGISTRY_SOURCE)
        fake = root / "bin" / "fake"
        fake.write_text(FAKE)
        fake.chmod(0o755)
        for name in ("qm", "pvesh", "pvesm", "logger", "findmnt", "fuser", "lsblk", "zfs", "pvesr", "backup_link.sh"):
            (root / "bin" / name).symlink_to(fake)
        self.overlay_file = root / "overlay" / "images" / "901" / "vm-901-disk-1.qcow2"
        self.overlay_file.write_bytes(b"QFI\xfb")
        self.environment = {
            **os.environ,
            "APP_HA_CLEANUP_TEST_MODE": "1", "APP_HA_LOCK_DIR": str(root / "locks"),
            "APP_HA_RESERVATION_DIR": str(root / "reservations"), "APP_HA_WORK_DIR": str(root / "work"),
            "APP_HA_LOCAL_NODE": "mox2", "APP_HA_CONFIG_TEST_MODE": "1",
            "APP_HA_CONFIG_DIR": str(root / "config"), "APP_HA_BACKUP_LINK": str(root / "bin" / "backup_link.sh"),
            "FAKE_STATE": str(root / "state.json"), "FAKE_OVERLAY_ROOT": str(root / "overlay"),
            "PATH": f"{root / 'bin'}:{os.environ['PATH']}",
        }
        self.save({
            "links": {"901": "alive"},
            "vms": {"901": {"status": "running", "config": {
                "name": "stage1prod1", "onboot": "0", "tags": "app-ha-staging;evictable;purpose-myapp",
                "efidisk0": "local-zfs:vm-901-disk-0,efitype=4m,size=1M",
                "scsi0": f"{OVERLAY},discard=on,iothread=1,ssd=1",
            }}},
        })
        self.registry("init", "--network", "10.213.0.0/24", "--guest-gateway", "10.213.0.10",
                      "--mox-ip-start", "10.213.0.11", "--mox-ip-end", "10.213.0.20",
                      "--vm-slot-start", "10.213.0.110", "--max-production", "10",
                      "--max-staging-per-production", "9")
        self.registry("allocate-prod", "--vmid", "100", "--purpose", "myapp", "--primary-domain", "example.com",
                      "--placement", "mox1,mox2", "--initial-node", "mox1")
        self.registry("update", "prod1", "--owner-node", "mox1")
        self.registry("allocate-staging", "--source", "prod1", "--vmid", "901", "--placement", "mox2",
                      "--placement-limit", "5", "--backup-storage", "bmac-pbs", "--backup-snapshot", SNAPSHOT)
        self.registry("update", "stage1prod1", "--state", "cloning", "--owner-node", "mox2", "--volume-id", OVERLAY)
        for state in ("patching", "stopped", "active"):
            self.registry("update", "stage1prod1", "--state", state)

    def save(self, value: dict) -> None:
        (self.root / "state.json").write_text(json.dumps(value))

    def load(self) -> dict:
        return json.loads((self.root / "state.json").read_text())

    def registry(self, *arguments: str):
        completed = subprocess.run([str(REGISTRY_SOURCE), "--state-dir", str(self.state_dir), *arguments],
                                   text=True, capture_output=True, check=False)
        self.assertEqual(completed.returncode, 0, msg=completed.stderr)
        return json.loads(completed.stdout) if completed.stdout.strip() else None

    def run_worker(self) -> subprocess.CompletedProcess[str]:
        completed = subprocess.run([str(self.worker), "--max-seconds", "30"], env=self.environment,
                                   text=True, capture_output=True, check=False)
        self.assertEqual(completed.returncode, 0, msg=f"stdout:\n{completed.stdout}\nstderr:\n{completed.stderr}")
        return completed

    def names(self) -> list[str]:
        return [row["name"] for row in self.registry("list", "--record-type", "resources")]

    def test_a_live_mapping_is_left_alone(self) -> None:
        self.run_worker()
        self.assertEqual(self.names(), ["prod1", "stage1prod1"])
        self.assertNotIn("actions", self.load())
        self.assertTrue(self.overlay_file.exists())

    def test_a_dead_mapping_stops_and_evicts_the_vm(self) -> None:
        state = self.load()
        state["links"]["901"] = "dead"
        self.save(state)
        completed = self.run_worker()
        self.assertIn("the PBS mapping of backup-linked staging VM stage1prod1 is dead; evicting it", completed.stderr)
        actions = [" ".join(action[:2]) for action in self.load()["actions"]]
        # Stopped first, its config destroyed (which removes the overlay), then its mapping removed.
        self.assertEqual(actions, ["qm stop", "qm destroy", "backup_link.sh unmap"])
        self.assertFalse(self.overlay_file.exists())
        self.assertEqual(self.names(), ["prod1"])

    def test_after_a_reboot_a_stopped_vm_is_evicted_without_a_stop(self) -> None:
        state = self.load()
        state["links"] = {}
        state["vms"]["901"]["status"] = "stopped"
        self.save(state)
        self.run_worker()
        actions = [" ".join(action[:2]) for action in self.load()["actions"]]
        self.assertEqual(actions, ["qm destroy", "backup_link.sh unmap"])
        self.assertEqual(self.names(), ["prod1"])

    def test_an_overlay_left_behind_is_freed(self) -> None:
        # The VM config no longer references the overlay, so qm destroy leaves it.
        state = self.load()
        state["links"]["901"] = "dead"
        del state["vms"]["901"]["config"]["scsi0"]
        self.save(state)
        self.run_worker()
        actions = [" ".join(action[:2]) for action in self.load()["actions"]]
        self.assertEqual(actions, ["qm stop", "qm destroy", "backup_link.sh unmap", "pvesm free"])
        self.assertFalse(self.overlay_file.exists())
        self.assertEqual(self.names(), ["prod1"])

    def test_a_vm_being_created_is_not_evicted(self) -> None:
        # The workflow maps the restore point while the record is cloning or patching.
        self.registry("update", "stage1prod1", "--state", "stopping")
        self.registry("update", "stage1prod1", "--state", "failed")
        self.registry("update", "stage1prod1", "--state", "patching")
        state = self.load()
        state["links"] = {}
        self.save(state)
        self.run_worker()
        self.assertEqual(self.names(), ["prod1", "stage1prod1"])
        self.assertNotIn("actions", self.load())


if __name__ == "__main__":
    unittest.main()
