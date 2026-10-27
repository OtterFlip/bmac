# Copyright (c) 2026 BEENTHERE VENTURES, INC.
# SPDX-License-Identifier: GPL-3.0-only

"""Tests for scripts/user_callable/backups/rollback_restore.sh against a
stateful fake cluster: HA stops and starts the VM, replication jobs and their
replicas disappear when deleted, and a restore replaces the VM's config with
the one in the backup."""

from __future__ import annotations

import json
import os
from pathlib import Path
import shlex
import subprocess
import sys
import tempfile
import textwrap
import unittest

sys.path.insert(0, str(Path(__file__).resolve().parent))
from ui_test_driver import run_json, scripted  # noqa: E402


REPO_ROOT = Path(__file__).resolve().parents[2]
SCRIPT = REPO_ROOT / "scripts" / "user_callable" / "backups" / "rollback_restore.sh"
SNAPSHOT = "vm/100/2026-10-27T16:04:23Z"
NOTES = "bmac prod1 slug=pinbinapp fqdn=pinbinapp.com cluster=MyAppCloud guest=prod1"

CLUSTER_CONF = """PROXMOX_CLUSTER_NAME=MyAppCloud
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
PROD_VM_STORAGE=local-zfs
PROD_VM_REPLICATION_INTERVAL=*/15
CLUSTER_STATE_DIR=/etc/pve/priv/app-ha
"""

LIVE_CONFIG = {
    "agent": "enabled=1,fstrim_cloned_disks=1", "cores": "64", "memory": "75776",
    "description": "Production guest (pinbinapp)",
    "efidisk0": "local-zfs:vm-100-disk-0,efitype=4m,ms-cert=2023k,pre-enrolled-keys=1,size=1M",
    "hookscript": "local:snippets/app-ha-guest-role-hook.sh", "name": "prod1",
    "net0": "virtio=1a:b1:44:6a:01:1f,bridge=vmbr-private,firewall=1", "onboot": "0",
    "scsi0": "local-zfs:vm-100-disk-1,discard=on,iothread=1,replicate=1,size=1T,ssd=1",
    "tags": "app-ha-production;purpose-pinbinapp",
}

RECORD = {
    "kind": "production", "name": "prod1", "index": 1, "vmid": 100, "state": "active", "revision": 3,
    "placement": ["mox1", "mox2"], "owner_node": "mox1", "mac": "1a:b1:44:6a:01:1f",
    "purpose": {"slug": "pinbinapp"}, "domains": {"primary": "pinbinapp.com", "aliases": []},
    "spec": {"cores": 64, "memory_mb": 75776, "disk_gib": 1024},
    "proxmox": {"volume_id": "local-zfs:vm-100-disk-1", "ha_nodes": ["mox1", "mox2"],
                "replication_targets": ["mox2"]},
}

# Answers the commands the workflow sends to cluster hosts. Calls are
# `fake.py mox HOST ARGV...` (mox_ssh) or `fake.py node HOST ARGV...`
# (node_exec); every call and its stdin are recorded.
FAKE = r'''#!/usr/bin/env python3
import json, os, sys
from pathlib import Path

fake = Path(os.environ["FAKE"])
kind, host, argv = sys.argv[1], sys.argv[2], sys.argv[3:]
stdin = sys.stdin.read()
with open(fake / "calls.jsonl", "a") as log:
    log.write(json.dumps({"via": kind, "host": host, "argv": argv}) + "\n")

def load(name, default=None):
    path = fake / name
    return json.loads(path.read_text()) if path.exists() else default

def save(name, value):
    (fake / name).write_text(json.dumps(value))

def out(value):
    print(value if isinstance(value, str) else json.dumps(value))
    sys.exit(0)

state = load("state.json")
def commit():
    save("state.json", state)

if argv[:1] == ["bash"]:
    sys.exit(0)
if argv[:1] == ["/usr/local/lib/app-ha-proxmox/lib/cluster_registry.py"]:
    command = argv[3]
    if command == "list":
        out([state["record"]] + state.get("staging", []))
    if command == "get":
        out(state["record"])
    if command == "orchestration-get":
        out({"record_type": "orchestration", "install_phase": "network-finalized",
             "final_network_enabled": True, "startup_sha256": None,
             "source_iso_sha256": "0" * 64, "install_mode": "ubuntu-autoinstall"})
    if command in ("orchestration-acquire", "orchestration-release"):
        state.setdefault("lease", []).append(command)
        commit()
        out("{}")
    if command == "update":
        state["record"]["revision"] += 1
        state["updates"] = state.get("updates", []) + [argv[4:]]
        if "--volume-id" in argv:
            state["record"]["proxmox"]["volume_id"] = argv[argv.index("--volume-id") + 1]
        commit()
        out(state["record"])
if argv[:2] == ["pvesh", "get"]:
    path = argv[2]
    if path == "/cluster/status":
        out([{"type": "cluster", "name": "MyAppCloud", "quorate": 1}])
    if path == "/nodes":
        out([{"node": "mox1", "status": "online"}, {"node": "mox2", "status": "online"}])
    if path == "/cluster/resources":
        out([{"vmid": 100, "name": "prod1", "type": "qemu", "node": "mox1", "status": state["status"]}])
    if path == "/cluster/ha/resources":
        out([{"sid": "vm:100", "state": state["ha"], "failback": 0, "auto-rebalance": 0}])
    if path == "/cluster/replication":
        out([{"id": job, "guest": 100, "target": target, "schedule": "*/15"}
             for job, target in state["jobs"].items()])
    if path == "/storage/bmac-pbs":
        out({"storage": "bmac-pbs"})
    if path.endswith("/storage/bmac-pbs/status"):
        out({"active": 1})
    if path.endswith("/storage/bmac-pbs/content"):
        out(state["content"])
    if "/replication/" in path and path.endswith("/status"):
        job = path.split("/")[4]
        out({"id": job, "last_sync": state["synced"].get(job, 1) if job in state["jobs"] else 0})
    if "/tasks/" in path and path.endswith("/status"):
        out({"status": "stopped", "exitstatus": state.get("task_exit", "OK")})
    if "/tasks/" in path and path.endswith("/log"):
        out([])
if argv[:2] == ["pvesh", "create"]:
    path = argv[2]
    if path.endswith("/vzdump"):
        state["safety_notes"] = argv[argv.index("--notes-template") + 1]
        state["content"].append({"volid": "bmac-pbs:backup/vm/100/2026-10-28T09:00:00Z", "vmid": 100,
                                 "ctime": 4102444800, "notes": "bmac prod1 safety", "content": "backup"})
        commit()
        out('"UPID:mox1:1:2:3:vzdump:100:root@pam:"')
    if path.endswith("/qemu"):
        state["restore_args"] = argv[3:]
        if state.get("restore_fails"):
            state["task_exit"] = "restore failed"
        else:
            state["config"] = dict(state["backup_config"])
        commit()
        out('"UPID:mox1:1:2:3:qmrestore:100:root@pam:"')
if argv[:2] == ["ha-manager", "rules"]:
    out([{"rule": "production-prod1-100", "type": "node-affinity", "resources": "vm:100",
          "nodes": "mox1:1,mox2:1", "strict": 1}])
if argv[:2] == ["ha-manager", "set"]:
    wanted = argv[argv.index("--state") + 1]
    state["ha"] = wanted
    state["status"] = "stopped" if wanted == "stopped" else "running"
    commit()
    sys.exit(0)
if argv[:2] == ["qm", "config"]:
    out("\n".join(f"{k}: {v}" for k, v in sorted(state["config"].items())))
if argv[:2] == ["qm", "set"]:
    words = argv[3:]
    for key, value in zip(words[::2], words[1::2]):
        state["config"][key.lstrip("-")] = value
    commit()
    sys.exit(0)
if argv[:2] == ["qm", "resize"]:
    state["resized"] = argv[3:]
    commit()
    sys.exit(0)
if argv[:2] == ["pvesr", "delete"]:
    state["jobs"].pop(argv[2], None)
    commit()
    sys.exit(0)
if argv[:2] == ["pvesr", "create-local-job"]:
    state["jobs"][argv[2]] = argv[3]
    state["synced"][argv[2]] = 99
    commit()
    sys.exit(0)
if argv[:2] == ["pvesr", "schedule-now"]:
    sys.exit(0)
if argv[:2] == ["pvesm", "list"]:
    out("Volid Format Type Size VMID")
print(f"fake: unexpected command {argv}", file=sys.stderr)
sys.exit(97)
'''

MUTATIONS = ("ha-manager set", "pvesr delete", "pvesr create-local-job", "pvesh create", "qm set", "qm resize")


class RollbackRestoreTest(unittest.TestCase):
    def setUp(self) -> None:
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        root = Path(self.temporary.name)
        self.config = root / "config"
        self.config.mkdir()
        (self.config / "cluster.conf").write_text(CLUSTER_CONF, encoding="utf-8")
        (self.config / "cluster.conf").chmod(0o644)
        self.fake = root / "fake"
        self.fake.mkdir()
        self.program = self.fake / "fake.py"
        self.program.write_text(FAKE, encoding="utf-8")
        self.program.chmod(0o755)
        backup_config = dict(LIVE_CONFIG, cores="32", memory="65536", name="prod1")
        self.state = {
            "record": json.loads(json.dumps(RECORD)), "status": "running", "ha": "started",
            "jobs": {"100-0": "mox2"}, "synced": {"100-0": 50}, "config": dict(LIVE_CONFIG),
            "backup_config": backup_config,
            "content": [
                {"volid": f"bmac-pbs:backup/{SNAPSHOT}", "vmid": 100, "ctime": 1793112601, "notes": NOTES,
                 "content": "backup"},
                {"volid": "bmac-pbs:backup/vm/100/2026-10-20T00:00:00Z", "vmid": 100, "ctime": 1792500000,
                 "notes": "manual", "content": "backup"},
            ],
        }
        self.save()

    def save(self) -> None:
        (self.fake / "state.json").write_text(json.dumps(self.state))

    def load(self) -> dict:
        return json.loads((self.fake / "state.json").read_text())

    def run_rollback(self, *arguments: str, expected: int = 0, answers: str = "") -> subprocess.CompletedProcess[str]:
        program = shlex.quote(str(self.program))
        body = f"""
        ROLLBACK_RESTORE_SOURCE_ONLY=1 source {shlex.quote(str(SCRIPT))}
        first_reachable_mox() {{ printf 'mox1\\n'; }}
        mox_ssh() {{ {program} mox "$@"; }}
        # The real node_exec feeds its command on stdin, never the terminal's.
        node_exec() {{ {program} node "$@" </dev/null; }}
        sleep() {{ :; }}
        main {" ".join(shlex.quote(a) for a in arguments)}
        """
        environment = os.environ.copy()
        environment.update({"APP_HA_CONFIG_TEST_MODE": "1", "APP_HA_CONFIG_DIR": str(self.config),
                            "FAKE": str(self.fake), "TZ": "America/Denver"})
        completed = subprocess.run(["bash", "-c", textwrap.dedent(body)], env=environment, input=answers,
                                   capture_output=True, text=True, check=False)
        self.assertEqual(completed.returncode, expected,
                         msg=f"stdout:\n{completed.stdout}\nstderr:\n{completed.stderr}")
        return completed

    def run_dashboard(self, *arguments: str, replies=()):
        """Run as the dashboard does: --json, answering each request in turn."""
        program = shlex.quote(str(self.program))
        harness = self.fake / "harness.sh"
        harness.write_text(textwrap.dedent(f"""\
            #!/usr/bin/env bash
            ROLLBACK_RESTORE_SOURCE_ONLY=1 source {shlex.quote(str(SCRIPT))}
            first_reachable_mox() {{ printf 'mox1\\n'; }}
            mox_ssh() {{ {program} mox "$@" </dev/null; }}
            node_exec() {{ {program} node "$@" </dev/null; }}
            sleep() {{ :; }}
            bmac_ui_bootstrap "$@"
            main "$@"
            """))
        harness.chmod(0o755)
        environment = {k: v for k, v in os.environ.items() if not k.startswith("BMAC_UI")}
        environment.update({"APP_HA_CONFIG_TEST_MODE": "1", "APP_HA_CONFIG_DIR": str(self.config),
                            "FAKE": str(self.fake), "TZ": "America/Denver"})
        return run_json([str(harness), *arguments], scripted(*replies), env=environment, timeout=60)

    def test_the_dashboard_asks_for_every_choice_left_empty(self) -> None:
        # A second, protected restore point: the picker labels it.
        self.state["content"].append({"volid": "bmac-pbs:backup/vm/100/2026-10-26T10:00:00Z", "vmid": 100,
                                      "ctime": 1793000000, "notes": NOTES, "content": "backup", "protected": 1})
        self.save()
        seen = []

        def pick_older(request):
            seen.append(request)
            return {"values": {"choice": "vm/100/2026-10-26T10:00:00Z"}}

        def compute(request):
            seen.append(request)
            return {"values": {"cores": 16, "memory_gib": 32}}

        def no_safety_backup(request):
            seen.append(request)
            return {"values": {"answer": False}}

        def confirm(request):
            seen.append(request)
            return {"confirmed": True}

        run = self.run_dashboard("prod1", replies=(pick_older, compute, no_safety_backup, confirm))
        self.assertEqual(run.returncode, 0, run.describe())
        picker, form, question, confirmation = seen
        self.assertEqual([option["label"] for option in picker["field"]["options"]],
                         ["2026-10-27 10:04 MDT", "2026-10-26 04:00 MDT · protected"])
        self.assertEqual([field["id"] for field in form["fields"]], ["cores", "memory_gib"])
        self.assertEqual(question["field"]["default"], True)
        self.assertEqual(confirmation["confirmation_text"], "ROLLBACK prod1")
        state = self.load()
        self.assertIn("bmac-pbs:backup/vm/100/2026-10-26T10:00:00Z", state["restore_args"])
        self.assertEqual((state["config"]["cores"], state["config"]["memory"]), ("16", "32768"))

    def mutations(self) -> list[str]:
        calls = [json.loads(line) for line in (self.fake / "calls.jsonl").read_text().splitlines()]
        found = []
        for call in calls:
            text = " ".join(call["argv"])
            if text.startswith(MUTATIONS):
                found.append(text)
        return found

    def test_rolls_back_in_a_safe_order_and_keeps_identity(self) -> None:
        completed = self.run_rollback("prod1", "--snapshot", SNAPSHOT, "--safety-backup",
                                      answers="\n\nROLLBACK prod1\n")
        steps = [m.split(" --")[0].split(" vm:")[0] for m in self.mutations()]
        self.assertEqual(steps, [
            "pvesh create /nodes/mox1/vzdump",      # safety backup, before anything changes
            "ha-manager set",                        # HA stops the VM
            "pvesr delete 100-0",                    # replication and its replica go first
            "pvesh create /nodes/mox1/qemu",         # the restore
            "qm set 100",                            # identity and compute back
            "ha-manager set",                        # HA starts it
            "pvesr create-local-job 100-0 mox2",     # replication again, full sync
        ])
        state = self.load()
        self.assertTrue(state["safety_notes"].endswith(" | Before rollback to 2026-10-27 10:04 MDT"), state["safety_notes"])
        self.assertEqual(state["restore_args"][:2], ["--vmid", "100"])
        self.assertIn(f"bmac-pbs:backup/{SNAPSHOT}", state["restore_args"])
        self.assertNotIn("--unique", state["restore_args"])
        # The restore brought back 32 vCPUs; the defaults keep today's 64 and 74 GiB.
        self.assertEqual((state["config"]["cores"], state["config"]["memory"]), ("64", "75776"))
        for key in ("net0", "tags", "hookscript", "name"):
            self.assertEqual(state["config"][key], LIVE_CONFIG[key])
        self.assertEqual((state["status"], state["ha"]), ("running", "started"))
        self.assertEqual(state["jobs"], {"100-0": "mox2"})
        self.assertEqual(state["lease"], ["orchestration-acquire", "orchestration-release"])
        self.assertNotIn("resized", state)
        # The snapshot was taken at 16:04 UTC: shown in the workstation's zone.
        self.assertIn("rolled back to 2026-10-27 10:04 MDT", completed.stdout)

    def test_chosen_compute_and_a_regrown_disk(self) -> None:
        self.state["backup_config"]["scsi0"] = LIVE_CONFIG["scsi0"].replace("size=1T", "size=512G")
        self.save()
        self.run_rollback("prod1", "--snapshot", SNAPSHOT, "--no-safety-backup", "--cores", "8",
                          "--memory-gib", "16", answers="ROLLBACK prod1\n")
        state = self.load()
        self.assertEqual((state["config"]["cores"], state["config"]["memory"]), ("8", "16384"))
        # The registry records them too, for the owner-change memory check.
        self.assertIn(["prod1", "--expected-revision", "3", "--cores", "8", "--memory-mb", "16384"], state["updates"])
        self.assertEqual(state["resized"], ["scsi0", "1024G"])
        self.assertFalse(any("vzdump" in m for m in self.mutations()))

    def test_a_wrong_confirmation_changes_nothing(self) -> None:
        completed = self.run_rollback("prod1", "--snapshot", SNAPSHOT, "--no-safety-backup",
                                      answers="\n\nrollback prod1\n", expected=1)
        self.assertIn("Cancelled; nothing was changed", completed.stderr)
        self.assertIn("Nothing was changed on prod1", completed.stderr)
        self.assertEqual(self.mutations(), [])

    def test_only_this_vms_bmac_restore_points(self) -> None:
        completed = self.run_rollback("prod1", "--snapshot", "vm/100/2026-10-20T00:00:00Z",
                                      "--no-safety-backup", answers="ROLLBACK prod1\n", expected=1)
        self.assertIn("is not a BMAC restore point of prod1", completed.stderr)
        self.assertEqual(self.mutations(), [])

    def test_a_locked_vm_is_refused_before_anything_changes(self) -> None:
        self.state["config"]["lock"] = "backup"
        self.save()
        completed = self.run_rollback("prod1", "--snapshot", SNAPSHOT, "--no-safety-backup",
                                      answers="ROLLBACK prod1\n", expected=1)
        self.assertIn("is locked (backup)", completed.stderr)
        self.assertEqual(self.mutations(), [])

    def test_staging_dependents_are_refused(self) -> None:
        self.state["staging"] = [{"kind": "staging", "name": "stage1prod1", "source": "prod1"}]
        self.save()
        completed = self.run_rollback("prod1", "--snapshot", SNAPSHOT, "--no-safety-backup", expected=1)
        self.assertIn("has staging VMs derived from it (stage1prod1)", completed.stderr)
        self.assertEqual(self.mutations(), [])

    def test_a_failed_restore_says_what_state_the_vm_is_in(self) -> None:
        self.state["restore_fails"] = True
        self.save()
        completed = self.run_rollback("prod1", "--snapshot", SNAPSHOT, "--no-safety-backup",
                                      answers="\n\nROLLBACK prod1\n", expected=1)
        self.assertIn("The restore failed: restore failed", completed.stderr)
        self.assertIn("prod1 is stopped and still has its current disks", completed.stderr)
        self.assertIn("ha-manager set vm:100 --state started", completed.stderr)
        self.assertIn("Replication job 100-0 to mox2", completed.stderr)
        self.assertEqual(self.load()["lease"], ["orchestration-acquire", "orchestration-release"])


if __name__ == "__main__":
    unittest.main()
