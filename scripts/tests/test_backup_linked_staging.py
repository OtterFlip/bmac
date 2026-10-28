#!/usr/bin/env python3

# Copyright (c) 2026 BEENTHERE VENTURES, INC.
# SPDX-License-Identifier: GPL-3.0-only

"""scripts/user_callable/guests/staging/add_backup_linked_staging_vm.sh
against a fake cluster whose registry calls run the real registry."""

from __future__ import annotations

import json
import os
from pathlib import Path
import shlex
import subprocess
import tempfile
import textwrap
import unittest


# The registry rejects paths with symlinked components (macOS /var).
tempfile.tempdir = str(Path(tempfile.gettempdir()).resolve())

REPO_ROOT = Path(__file__).resolve().parents[2]
SCRIPT = REPO_ROOT / "scripts" / "user_callable" / "guests" / "staging" / "add_backup_linked_staging_vm.sh"
REGISTRY = REPO_ROOT / "scripts" / "host_runtime" / "cluster_registry.py"
SNAPSHOT = "vm/100/2026-10-27T16:04:23Z"
OVERLAY = "bmac-overlay:200/vm-200-disk-1.qcow2"

CLUSTER_CONF = """PROXMOX_CLUSTER_NAME=app-ha-test
MAX_MOX_HOSTS=10
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
GUEST_ROLE_HOOK_PATH=hosts/app-ha-guest-role-hook.sh
CLUSTER_STATE_DIR=/etc/pve/priv/app-ha
PROD_VM_STORAGE=local-zfs
PROD_VM_CPU_TYPE=x86-64-v2-AES
STAGING_VM_CORES=2
STAGING_VM_MEMORY_GIB=4
STAGING_VM_STORAGE=local-zfs
STAGING_VM_BRIDGE=vmbr-private
SNIPPETS_STORAGE_ID=local
GUEST_DNS_SERVERS=1.1.1.1,1.0.0.1
"""

# Answers what the workflow sends to hosts: `fake.py mox|node HOST ARGV...`.
# The installed registry is the real one, on the test's state directory.
FAKE = r'''#!/usr/bin/env python3
import json, os, subprocess, sys
from pathlib import Path

fake = Path(os.environ["FAKE"])
via, host, argv = sys.argv[1], sys.argv[2], sys.argv[3:]
sys.stdin.read() if not sys.stdin.isatty() else None
state = json.loads((fake / "state.json").read_text())

def save():
    (fake / "state.json").write_text(json.dumps(state))

def record():
    state.setdefault("calls", []).append([host, *argv])
    save()

def out(value):
    print(value if isinstance(value, str) else json.dumps(value))
    sys.exit(0)

if argv[0] == "/usr/local/lib/app-ha-proxmox/lib/cluster_registry.py":
    rest = argv[1:]
    if rest[:1] == ["--state-dir"]:
        rest = rest[2:]
    if rest[:1] not in (["list"], ["get"]) and "--help" not in rest:
        argv = [argv[0], *rest]
        record()
    completed = subprocess.run([os.environ["REAL_REGISTRY"], "--state-dir", os.environ["STATE_DIR"], *rest],
                               text=True, capture_output=True)
    sys.stdout.write(completed.stdout)
    sys.stderr.write(completed.stderr)
    sys.exit(completed.returncode)
if argv[0] == "/usr/local/lib/app-ha-proxmox/lib/backup_link.sh":
    record()
    if argv[1] == "map" and state.get("map_fails"):
        sys.exit(1)
    if argv[1] == "create-overlay":
        out("/var/lib/bmac-overlay/images/200/vm-200-disk-1.qcow2")
    out("/run/bmac-backup-link/200.img")
if argv[:2] == ["test", "-x"] or argv[0] in ("install", "rm", "chmod", "mv"):
    sys.exit(0)
if argv[:2] == ["bash", "-c"]:
    sys.exit(0)
if argv[0] == "bash":
    record()
    sys.exit(1 if state.get("patch_fails") else 0)
if argv[:2] == ["pvesh", "get"]:
    path = argv[2]
    if path == "/cluster/status":
        out([{"type": "cluster", "name": "app-ha-test", "quorate": 1}])
    if path == "/nodes":
        out([{"node": "mox1", "status": "online"}, {"node": "mox2", "status": "online"}])
    if path == "/storage/bmac-pbs":
        out({"storage": "bmac-pbs", "type": "pbs"})
    if path.endswith("/storage/bmac-pbs/status"):
        out({"active": 1})
    if path.endswith("/storage/bmac-pbs/content"):
        out(state["content"])
    if path == "/storage/bmac-overlay":
        if not state.get("overlay_storage"):
            sys.exit(2)
        storage = {"storage": "bmac-overlay", "type": "dir", "path": "/var/lib/bmac-overlay", "content": "images"}
        if state.get("overlay_mkdir"):
            storage["mkdir"] = 1
        out(storage)
    if path == "/cluster/resources":
        out([{"vmid": int(vmid), "type": "qemu", "node": vm["node"], "name": vm["config"]["name"],
              "status": vm["status"]} for vmid, vm in state["vms"].items()])
    if path == "/cluster/nextid":
        out('"200"')
    if path == "/cluster/ha/resources":
        out([{"sid": "vm:100"}])
if argv[:3] == ["pvesm", "add", "dir"]:
    record()
    state["overlay_storage"] = True
    save()
    sys.exit(0)
if argv[:2] == ["pvesm", "set"]:
    record()
    state.pop("overlay_mkdir", None)
    save()
    sys.exit(0)
if argv[:2] == ["pvesm", "path"]:
    out("/var/lib/vz/snippets/app-ha-guest-role-hook.sh")
if argv[:2] == ["pvesm", "free"]:
    record()
    sys.exit(0)
if argv[0] == "qm":
    vmid = argv[2] if argv[1] == "create" else argv[1] if False else argv[2] if len(argv) > 2 and argv[1] in () else None
    operation, vmid = argv[1], argv[2]
    vm = state["vms"].get(vmid)
    if operation == "create":
        record()
        words = argv[3:]
        config = {key.lstrip("-"): value for key, value in zip(words[::2], words[1::2])}
        config["efidisk0"] = f"local-zfs:vm-{vmid}-disk-0,efitype=4m,pre-enrolled-keys=1,size=1M"
        state["vms"][vmid] = {"node": host, "status": "stopped", "config": config}
        save()
        sys.exit(0)
    if vm is None:
        sys.exit(2)
    if operation == "config":
        out("\n".join(f"{k}: {v}" for k, v in vm["config"].items()))
    if operation == "status":
        out(f"status: {vm['status']}")
    record()
    if operation == "set":
        words = argv[3:]
        vm["config"].update({key.lstrip("-"): value for key, value in zip(words[::2], words[1::2])})
    elif operation == "start":
        vm["status"] = "running"
    elif operation == "stop":
        vm["status"] = "stopped"
    elif operation == "destroy":
        del state["vms"][vmid]
    save()
    sys.exit(0)
print(f"fake: unexpected command {argv}", file=sys.stderr)
sys.exit(97)
'''


class BackupLinkedStagingTest(unittest.TestCase):
    def setUp(self) -> None:
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        root = Path(self.temporary.name)
        self.root = root
        self.config = root / "config"
        self.config.mkdir()
        (self.config / "cluster.conf").write_text(CLUSTER_CONF)
        (self.config / "cluster.conf").chmod(0o644)
        (self.config / "secrets.env").write_text("STAGING_GUEST_VM_ROOT_PASSWORD=synthetic-secret-value\n")
        (self.config / "secrets.env").chmod(0o600)
        for index in (1, 2):
            (self.config / f"mox{index}.conf").write_text(textwrap.dedent(f"""\
                NVME_MIRROR_0_SERIAL_1=test-mox{index}-a
                NVME_MIRROR_0_SERIAL_2=test-mox{index}-b
                PROXMOX_IP=192.0.2.{index}
                PROXMOX_GATEWAY=192.0.2.254
                PROXMOX_PREFIX=24
                TAILSCALE_HOSTNAME=mox{index}
                PROXMOX_PUBLIC_MAC=02:00:00:00:00:{index:02x}
                PROXMOX_SECONDARY_MAC=02:00:00:00:01:{index:02x}
                MAX_PROD_VM_COUNT_ON_THIS_HOST=1
                MAX_STAGING_VM_COUNT_ON_THIS_HOST=5
                """))
        self.fake = root / "fake"
        self.fake.mkdir()
        self.program = self.fake / "fake.py"
        self.program.write_text(FAKE)
        self.program.chmod(0o755)
        self.state_dir = root / "registry"
        self.registry("init", "--network", "10.213.0.0/24", "--guest-gateway", "10.213.0.10",
                      "--mox-ip-start", "10.213.0.11", "--mox-ip-end", "10.213.0.20",
                      "--vm-slot-start", "10.213.0.110", "--max-production", "10",
                      "--max-staging-per-production", "9")
        self.registry("allocate-prod", "--vmid", "100", "--purpose", "myapp", "--primary-domain", "example.com",
                      "--placement", "mox1,mox2", "--initial-node", "mox1")
        self.registry("update", "prod1", "--owner-node", "mox1")
        notes = "bmac prod1 slug=myapp fqdn=example.com cluster=app-ha-test guest=prod1"
        self.save({
            "content": [
                {"volid": f"bmac-pbs:backup/{SNAPSHOT}", "vmid": 100, "size": 1099511627776, "notes": notes},
                {"volid": "bmac-pbs:backup/vm/100/2026-10-20T00:00:00Z", "vmid": 100, "size": 1, "notes": "manual"},
            ],
            "vms": {"100": {"node": "mox1", "status": "running", "config": {"name": "prod1"}}},
        })

    def save(self, value: dict) -> None:
        (self.fake / "state.json").write_text(json.dumps(value))

    def load(self) -> dict:
        return json.loads((self.fake / "state.json").read_text())

    def registry(self, *arguments: str):
        completed = subprocess.run([str(REGISTRY), "--state-dir", str(self.state_dir), *arguments],
                                   text=True, capture_output=True, check=False)
        self.assertEqual(completed.returncode, 0, msg=completed.stderr)
        return json.loads(completed.stdout) if completed.stdout.strip() else None

    def run_workflow(self, *arguments: str, expected: int = 0,
                     answers: str = "\n\n\n") -> subprocess.CompletedProcess[str]:
        program = shlex.quote(str(self.program))
        body = f"""
        APP_HA_BACKUP_LINKED_SOURCE_ONLY=1 source {shlex.quote(str(SCRIPT))}
        fakes() {{
          first_reachable_mox() {{ printf 'mox1\\n'; }}
          mox_ssh() {{ {program} mox "$@"; }}
        }}
        # The workflow sources config.sh, which defines mox_ssh, while loading
        # its configuration; the fakes go back on top afterwards.
        eval "real_$(declare -f load_and_validate_config)"
        load_and_validate_config() {{ real_load_and_validate_config; fakes; }}
        fakes
        node_exec() {{ {program} node "$@" </dev/null; }}
        upload_file_to_staging_node() {{ :; }}
        sleep() {{ :; }}
        main {" ".join(shlex.quote(a) for a in arguments)}
        """
        environment = {**os.environ, "APP_HA_CONFIG_TEST_MODE": "1", "APP_HA_CONFIG_DIR": str(self.config),
                       "FAKE": str(self.fake), "REAL_REGISTRY": str(REGISTRY), "STATE_DIR": str(self.state_dir),
                       "TZ": "America/Denver"}
        completed = subprocess.run(["bash", "-c", textwrap.dedent(body)], env=environment, input=answers,
                                   capture_output=True, text=True, check=False)
        self.assertEqual(completed.returncode, expected, msg=f"stdout:\n{completed.stdout}\nstderr:\n{completed.stderr}")
        return completed

    def calls(self) -> list[str]:
        return [" ".join(call[1:4]) for call in self.load().get("calls", [])]

    def test_boots_a_restore_point_in_a_staging_slot(self) -> None:
        completed = self.run_workflow("--snapshot", SNAPSHOT, "--no-jump-ssh", answers="\n\n\nGO\n")
        calls = self.load()["calls"]
        # The patcher runs from a run directory named after the workflow's PID.
        steps = ["bash patch_staging_clone.sh" if call[1] == "bash" and call[2].endswith("/patch_staging_clone.sh")
                 else " ".join(call[1:3]) for call in calls]
        registry = "/usr/local/lib/app-ha-proxmox/lib/cluster_registry.py"
        link = "/usr/local/lib/app-ha-proxmox/lib/backup_link.sh"
        self.assertEqual(steps, [
            "pvesm add",                    # the overlay storage, once per cluster
            f"{registry} allocate-staging",
            "qm create",
            f"{registry} update",           # cloning, with its owner and overlay
            f"{link} map",
            f"{link} create-overlay",
            f"{registry} update",           # patching
            "bash patch_staging_clone.sh",  # the offline patch of the overlay
            "qm set",
            f"{registry} update",           # stopped
            "qm start",
            f"{registry} update",           # active
        ])
        allocate = calls[1]
        self.assertEqual(allocate[allocate.index("--backup-snapshot") + 1], SNAPSHOT)
        self.assertEqual(allocate[allocate.index("--placement") + 1], "mox2")
        patch = calls[7]
        self.assertEqual(patch[patch.index("--qcow2") + 1], "/var/lib/bmac-overlay/images/200/vm-200-disk-1.qcow2")
        self.assertEqual(patch[patch.index("--volume-id") + 1], OVERLAY)
        self.assertEqual(patch[patch.index("--name") + 1], "stage1prod1")
        self.assertEqual(calls[8][calls[8].index("--scsi0") + 1], f"{OVERLAY},discard=on,iothread=1,ssd=1")
        stage = self.registry("get", "stage1prod1")
        self.assertEqual((stage["state"], stage["owner_node"], stage["proxmox"]["volume_id"]), ("active", "mox2", OVERLAY))
        self.assertEqual(stage["backup"], {"storage": "bmac-pbs", "snapshot": SNAPSHOT})
        self.assertEqual(stage["spec"]["disk_gib"], 1024)
        vm = self.load()["vms"]["200"]
        self.assertEqual(vm["config"]["tags"], "app-ha-staging;evictable;purpose-myapp")
        self.assertIn("restore point vm/100/2026-10-27T16:04:23Z", vm["config"]["description"])
        self.assertIn("Backup-linked staging VM created", completed.stdout)

    def test_the_overlay_storage_never_uses_the_deprecated_mkdir_option(self) -> None:
        self.run_workflow("--snapshot", SNAPSHOT, "--no-jump-ssh", "--link-down", "--no-start")
        add = [call for call in self.load()["calls"] if call[1:3] == ["pvesm", "add"]][0]
        self.assertNotIn("--mkdir", add)
        # A storage the first release created loses it on the next run.
        self.registry("update", "stage1prod1", "--state", "cleanup_pending")
        state = self.load()
        state["calls"] = []
        state["overlay_mkdir"] = True
        del state["vms"]["200"]
        self.save(state)
        self.run_workflow("--snapshot", SNAPSHOT, "--no-jump-ssh", "--link-down", "--no-start")
        sets = [call for call in self.load()["calls"] if call[1:3] == ["pvesm", "set"]]
        self.assertEqual(sets, [["mox1", "pvesm", "set", "bmac-overlay", "--delete", "mkdir"]])

    def test_a_failed_patch_removes_everything_it_made(self) -> None:
        state = self.load()
        state["patch_fails"] = True
        self.save(state)
        completed = self.run_workflow("--snapshot", SNAPSHOT, "--no-jump-ssh", "--link-down", expected=1)
        self.assertIn("Offline overlay patching failed", completed.stderr)
        self.assertIn("Rollback removed the VM, overlay, PBS mapping, and registry allocation", completed.stderr)
        steps = [" ".join(call[1:3]) for call in self.load()["calls"]]
        self.assertIn("qm destroy", steps)
        self.assertIn("/usr/local/lib/app-ha-proxmox/lib/backup_link.sh unmap", steps)
        self.assertNotIn("200", self.load()["vms"])
        self.assertEqual([row["name"] for row in self.registry("list", "--record-type", "resources")], ["prod1"])

    def test_only_bmac_restore_points_of_registered_production_vms(self) -> None:
        completed = self.run_workflow("--snapshot", "vm/100/2026-10-20T00:00:00Z", expected=1)
        self.assertIn("is not a BMAC restore point of a registered production VM", completed.stderr)
        self.assertEqual(self.load().get("calls", []), [])

    def test_no_host_running_a_production_vm(self) -> None:
        state = self.load()
        state["vms"]["100"]["node"] = "mox2"
        self.save(state)
        self.registry("update", "prod1", "--owner-node", "mox2")
        # prod1 now runs on mox2, its owner, so mox1 is eligible.
        self.run_workflow("--snapshot", SNAPSHOT, "--no-jump-ssh", "--link-down", "--no-start")
        self.assertEqual(self.registry("get", "stage1prod1")["owner_node"], "mox1")

    def test_the_production_vm_need_not_be_running(self) -> None:
        state = self.load()
        state["vms"]["100"]["status"] = "stopped"
        self.save(state)
        self.run_workflow("--snapshot", SNAPSHOT, "--no-jump-ssh", "--link-down")
        self.assertEqual(self.registry("get", "stage1prod1")["state"], "active")


if __name__ == "__main__":
    unittest.main()
