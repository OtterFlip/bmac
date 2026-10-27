# Copyright (c) 2026 BEENTHERE VENTURES, INC.
# SPDX-License-Identifier: GPL-3.0-only

"""Tests for scripts/user_callable/backups/full_recovery.sh. Each test
sources the script and replaces the cluster calls of the step it checks with
recording fakes; add_prod_vm.sh's own steps have their own tests."""

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


SCRIPT = Path(__file__).resolve().parents[1] / "user_callable" / "backups" / "full_recovery.sh"
KEY = "24:a7:6e:9e:65:9f:c8:d6"
NOTES = "bmac prod1 slug=pinbinapp fqdn=pinbinapp.com cluster=MyAppCloud guest=prod1"

CONTENT = [
    {"volid": "bmac-pbs:backup/vm/100/2026-10-27T16:04:23Z", "vmid": 100, "content": "backup",
     "encrypted": KEY, "notes": NOTES + " | Before upgrading Postgres"},
    {"volid": "bmac-pbs:backup/vm/100/2026-10-26T10:00:00Z", "vmid": 100, "content": "backup",
     "encrypted": KEY, "notes": NOTES},
    {"volid": "bmac-pbs:backup/vm/100/2026-10-28T00:00:00Z", "vmid": 100, "content": "backup",
     "encrypted": KEY, "notes": "made by hand"},
    {"volid": "bmac-pbs:backup/vm/101/2026-10-28T01:00:00Z", "vmid": 101, "content": "backup",
     "encrypted": "11:22", "notes": "bmac prod2 slug=shop fqdn=shop.example.com"},
]

BACKUP_CONFIG = "\n".join([
    "#Production guest (pinbinapp)", "agent: enabled=1,fstrim_cloned_disks=1", "balloon: 0", "bios: ovmf",
    "boot: order=scsi0", "cores: 64", "cpu: x86-64-v2-AES",
    "efidisk0: local-zfs:vm-100-disk-0,efitype=4m,ms-cert=2023k,pre-enrolled-keys=1,size=1M",
    "hookscript: local:snippets/app-ha-guest-role-hook.sh", "machine: q35", "memory: 75776", "name: prod1",
    "net0: virtio=1a:b1:44:6a:01:1f,bridge=vmbr-private,firewall=1", "onboot: 0", "ostype: l26",
    "scsi0: local-zfs:vm-100-disk-1,discard=on,iothread=1,replicate=1,size=1T,ssd=1",
    "scsihw: virtio-scsi-single", "tags: app-ha-production;purpose-pinbinapp",
    "#qmdump#map:efidisk0:drive-efidisk0:local-zfs:raw:", "#qmdump#map:scsi0:drive-scsi0:local-zfs:raw:", "",
])

RECORD = {
    "name": "prod1", "vmid": 100, "ip": "10.213.0.110", "mac": "02:00:00:00:00:01", "state": "reserved",
    "revision": 1, "purpose": {"slug": "pinbinapp"},
    "domains": {"primary": "pinbinapp.com", "aliases": [], "staging_base": "staging.pinbinapp.com"},
    "placement": ["mox1", "mox2"], "initial_node": "mox1", "owner_node": None,
    "spec": {"cores": 64, "memory_mb": 75776, "disk_gib": 1024, "disk_allocation": "sparse",
             "replication_interval": "15"},
    "proxmox": {"volume_id": None, "replication_targets": [], "ha_nodes": []},
    "allocation_id": "recover-production-pinbinapp",
}


class FullRecoveryTest(unittest.TestCase):
    def setUp(self) -> None:
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        self.calls = self.root / "calls"
        self.calls.write_text("")

    def run_body(self, body: str, *, expected: int = 0, stdin: str = "") -> subprocess.CompletedProcess[str]:
        script = textwrap.dedent(f"""
            FULL_RECOVERY_SOURCE_ONLY=1 source {shlex.quote(str(SCRIPT))}
            RUN_DIR={shlex.quote(str(self.root))}
            CALLS={shlex.quote(str(self.calls))}
            record() {{ printf '%s\\n' "$*" >>"$CALLS"; }}
            COORDINATOR=mox1
            BACKUP_STORAGE_ID=bmac-pbs
            """) + textwrap.dedent(body)
        completed = subprocess.run(["bash", "-c", script], input=stdin, capture_output=True, text=True,
                                   env={**os.environ, "TZ": "America/Denver"}, check=False)
        self.assertEqual(completed.returncode, expected,
                         msg=f"stdout:\n{completed.stdout}\nstderr:\n{completed.stderr}")
        return completed

    def recorded(self) -> list[str]:
        return self.calls.read_text().splitlines()

    def test_restore_points_this_cluster_can_decrypt_newest_first(self) -> None:
        (self.root / "content.json").write_text(json.dumps(CONTENT))
        completed = self.run_body(f"""
            pvesh_get() {{
              case "$1" in
                /storage/bmac-pbs) printf '%s' '{json.dumps({"encryption-key": KEY})}' ;;
                *) cat {shlex.quote(str(self.root / "content.json"))} ;;
              esac
            }}
            list_recovery_points | tr '\\037' '|'
            SNAPSHOT=vm/100/2026-10-26T10:00:00Z
            choose_snapshot
            printf 'chosen %s %s %s %s %s [%s]\\n' "$SNAPSHOT" "$BACKUP_VMID" "$BACKUP_NAME" "$BACKUP_PURPOSE" "$BACKUP_FQDN" "$BACKUP_TIME"
            """)
        lines = completed.stdout.splitlines()
        # Hand-made backups and those of another key are left out.
        self.assertEqual(lines[:2], [
            "vm/100/2026-10-27T16:04:23Z|100|prod1|pinbinapp|pinbinapp.com|"
            "2026-10-27 10:04 MDT · prod1 · pinbinapp.com · Before upgrading Postgres",
            "vm/100/2026-10-26T10:00:00Z|100|prod1|pinbinapp|pinbinapp.com|"
            "2026-10-26 04:00 MDT · prod1 · pinbinapp.com",
        ])
        self.assertEqual(lines[2], "chosen vm/100/2026-10-26T10:00:00Z 100 prod1 pinbinapp pinbinapp.com [2026-10-26 04:00 MDT]")

    def test_the_backed_up_vm_config(self) -> None:
        def read(config: str, expected: int = 0) -> subprocess.CompletedProcess[str]:
            (self.root / "config.json").write_text(json.dumps(config))
            return self.run_body(f"""
                pvesh_get() {{ cat {shlex.quote(str(self.root / "config.json"))}; }}
                SNAPSHOT=vm/100/2026-10-27T16:04:23Z
                read_backup_config
                printf '%s %s %s\\n' "$BACKUP_CORES" "$BACKUP_MEMORY_MB" "$BACKUP_DISK_BYTES"
                """, expected=expected)

        self.assertEqual(read(BACKUP_CONFIG).stdout.strip(), f"64 75776 {2**40}")
        newer = BACKUP_CONFIG.replace("memory: 75776", "memory: current=32768").replace("size=1T", "size=1100G")
        self.assertEqual(read(newer).stdout.strip(), f"64 32768 {1100 * 2**30}")
        extra_disk = BACKUP_CONFIG + "scsi1: local-zfs:vm-100-disk-2,size=8G\n"
        self.assertIn("not one EFI disk and one scsi0 root disk", read(extra_disk, expected=1).stderr)

    def test_the_vmid_continues_the_backup_group_when_free(self) -> None:
        def choose(live: list, registered: list) -> str:
            (self.root / "live.json").write_text(json.dumps(live))
            (self.root / "resources.json").write_text(json.dumps(registered))
            (self.root / "content.json").write_text(json.dumps(CONTENT))
            return self.run_body(f"""
                pvesh_get() {{
                  case "$1" in
                    /cluster/resources) cat {shlex.quote(str(self.root / "live.json"))} ;;
                    /cluster/nextid) printf '"100"' ;;
                    *) cat {shlex.quote(str(self.root / "content.json"))} ;;
                  esac
                }}
                BACKUP_VMID=100
                choose_vmid
                printf '%s\\n' "$VMID"
                """).stdout.strip()

        self.assertEqual(choose([], []), "100")
        # 100 is live: the next VMID that no VM and no backup group uses.
        self.assertEqual(choose([{"vmid": 100}, {"vmid": 102}], []), "103")
        self.assertEqual(choose([], [{"name": "prod1", "vmid": 100}]), "102")

    def allocation_body(self, registered: list, allocated_name: str, existing: str = "") -> str:
        (self.root / "resources.json").write_text(json.dumps(registered))
        record = dict(RECORD, name=allocated_name)
        (self.root / "allocation.json").write_text(json.dumps({"created": True, "resource": record}))
        (self.root / "existing.json").write_text(existing)
        return f"""
            find_existing_purpose() {{ cat {shlex.quote(str(self.root / "existing.json"))}; }}
            collect_recovery_request() {{
              PLACEMENT_NODES=(mox1 mox2); INITIAL_NODE=mox1; PRIMARY_DOMAIN=pinbinapp.com; ALIAS_DOMAINS=()
              STAGING_DNS_BASE=staging.pinbinapp.com; VM_CORES=64; VM_MEMORY_MB=75776; VM_DISK_GIB=1024
              DISK_ALLOCATION=sparse; REPLICATION_MINUTES=15
            }}
            validate_prod_host_capacity() {{ :; }}
            choose_vmid() {{ VMID=100; }}
            registry_cmd() {{ record registry "$@"; cat {shlex.quote(str(self.root / "allocation.json"))}; }}
            registry_update() {{ record update "$@"; }}
            refresh_resource() {{ :; }}
            BACKUP_NAME=prod1; BACKUP_PURPOSE=pinbinapp; BACKUP_DISK_BYTES={1024 * 2**30}
            reserve_or_resume_recovery
            printf 'result %s %s\\n' "$RESOURCE_NAME" "$NEW_IDENTITY"
            """

    def test_a_free_original_slot_is_reused(self) -> None:
        completed = self.run_body(self.allocation_body([], "prod1"), stdin="pinbinapp\n")
        [allocate] = [line for line in self.recorded() if line.startswith("registry allocate-prod")]
        self.assertIn("--index 1", allocate)
        self.assertIn("--allocation-id recover-production-pinbinapp", allocate)
        self.assertIn("result prod1 false", completed.stdout)

    def test_a_taken_slot_gets_a_new_identity(self) -> None:
        completed = self.run_body(self.allocation_body([{"name": "prod1", "vmid": 100}], "prod2"),
                                  stdin="pinbinapp-copy\n")
        [allocate] = [line for line in self.recorded() if line.startswith("registry allocate-prod")]
        self.assertNotIn("--index", allocate)
        self.assertIn("result prod2 true", completed.stdout)
        self.assertIn("gets a new machine-id and SSH host keys", completed.stdout)

    def test_an_unfinished_recovery_resumes_and_another_vms_purpose_is_refused(self) -> None:
        resumed = self.run_body(self.allocation_body([], "prod1", json.dumps(RECORD)), stdin="pinbinapp\n")
        self.assertIn("Resuming the recovery of prod1", resumed.stdout)
        self.assertFalse([line for line in self.recorded() if line.startswith("registry")])
        other = dict(RECORD, allocation_id="create-production-pinbinapp")
        refused = self.run_body(self.allocation_body([], "prod1", json.dumps(other)), stdin="pinbinapp\n",
                                expected=1)
        self.assertIn("Purpose pinbinapp belongs to production VM prod1; choose another purpose", refused.stderr)

    def test_the_restore_applies_the_slot_identity_and_drops_the_hook(self) -> None:
        (self.root / "config.txt").write_text(BACKUP_CONFIG)
        self.run_body(f"""
            renew_orchestration_lease() {{ :; }}
            # The restore runs in $(...), a subshell: a file marks it done.
            locate_vm() {{ [[ -e "$RUN_DIR/restored" ]] || return 1; OWNER_NODE=mox1; VM_LIVE_STATUS=stopped; }}
            mox_ssh() {{ record mox "$@"; : >"$RUN_DIR/restored"; printf '"UPID:mox1:1:2:3:qmrestore:105:root@pam:"'; }}
            pve_task_follow() {{ :; }}
            node_exec() {{
              record node "$@"
              [[ "$3 $4" != "config 105" ]] || cat {shlex.quote(str(self.root / "config.txt"))}
            }}
            verify_vm_config() {{ record verify; }}
            apply_disk_allocation() {{ record allocation; }}
            ensure_registry_provisioning() {{ record provisioning; }}
            refresh_resource() {{ :; }}
            RESOURCE_STATE=reserved; VMID=105; RESOURCE_NAME=prod3; INITIAL_NODE=mox1; SNAPSHOT=vm/100/2026-10-27T16:04:23Z
            PROD_VM_STORAGE=local-zfs; VM_CORES=8; VM_MEMORY_MB=16384; PROD_VM_CPU_TYPE=x86-64-v2-AES
            VM_MAC=02:00:00:00:00:03; PROD_VM_BRIDGE=vmbr-private; VM_TAGS="app-ha-production;purpose-shop"
            PURPOSE_SLUG=shop; NEW_IDENTITY=true
            restore_vm
            """)
        calls = self.recorded()
        restore = next(line for line in calls if line.startswith("mox mox1 pvesh create /nodes/mox1/qemu"))
        for option in ("--vmid 105", "--archive bmac-pbs:backup/vm/100/2026-10-27T16:04:23Z", "--force 0",
                       "--name prod3", "--cores 8", "--memory 16384", "--cpu x86-64-v2-AES",
                       "--net0 virtio=02:00:00:00:00:03,bridge=vmbr-private,firewall=1",
                       "--tags app-ha-production;purpose-shop", "--onboot 0"):
            self.assertIn(option, restore)
        self.assertNotIn("--unique", restore)
        self.assertIn("node mox1 qm set 105 --delete hookscript", calls)
        self.assertTrue(any(line.startswith("node mox1 qm set 105 --smbios1 uuid=") for line in calls))
        self.assertEqual(calls[-3:], ["verify", "allocation", "provisioning"])

    def test_the_offline_patch(self) -> None:
        def patch(new_identity: str, patch_status: int) -> subprocess.CompletedProcess[str]:
            self.calls.write_text("")
            return self.run_body(f"""
                locate_vm() {{ OWNER_NODE=mox2; VM_LIVE_STATUS=stopped; }}
                upload_to_owner() {{ record upload "$(basename "$1")" "$2" "$3"; [[ "$1" != *authorized_keys ]] || record keys "$(cat "$1")"; }}
                node_exec() {{
                  record node "$@"
                  case "$2" in
                    pvesm) printf '/dev/zvol/rpool/data/vm-105-disk-1\\n' ;;
                    zfs) [[ "$3" != get ]] || printf 'default\\tdefault\\n' ;;
                    bash) [[ "$3" != *patch_staging_clone.sh ]] || return {patch_status} ;;
                  esac
                }}
                set_install_phase() {{ record phase "$1"; INSTALL_PHASE="$1"; }}
                VMID=105; RESOURCE_NAME=prod3; ROOT_VOLUME=local-zfs:vm-105-disk-1; PROD_PRIVATE_IP=10.213.0.130
                GUEST_GATEWAY=10.213.0.10; VM_MAC=02:00:00:00:00:03; DNS_SERVERS=(1.1.1.1 1.0.0.1)
                ROOT_PASSWORD_HASH='$6$x$y'; ADMIN_1_PUBLIC_SSH_KEY='ssh-ed25519 AAAAone admin1'; ADMIN_2_PUBLIC_SSH_KEY=''
                NEW_IDENTITY={new_identity}
                patch_offline
                """, expected=1 if patch_status else 0)

        patch("true", 0)
        calls = self.recorded()
        run = next(line for line in calls if "patch_staging_clone.sh --mode production" in line)
        for option in ("--device /dev/zvol/rpool/data/vm-105-disk-1", "--volume-id local-zfs:vm-105-disk-1",
                       "--name prod3", "--owner-vmid 105", "--address 10.213.0.130/24", "--mac 02:00:00:00:00:03",
                       "--dns 1.1.1.1,1.0.0.1", "--new-identity"):
            self.assertIn(option, run)
        self.assertIn("keys ssh-ed25519 AAAAone admin1", calls)
        volmode = [line for line in calls if " zfs " in f" {line} " or "volmode" in line]
        self.assertTrue(any("zfs set volmode=full" in line for line in volmode), volmode)
        self.assertIn("node mox2 zfs inherit volmode rpool/data/vm-105-disk-1", calls)
        self.assertLess(calls.index(run), calls.index("node mox2 zfs inherit volmode rpool/data/vm-105-disk-1"))
        self.assertEqual(calls[-1], "phase installed-confirmed")

        failed = patch("false", 1)
        calls = self.recorded()
        self.assertNotIn("--new-identity", next(line for line in calls if "patch_staging_clone.sh" in line and "--mode" in line))
        self.assertIn("The offline patch failed; the VM was not started", failed.stderr)
        self.assertIn("node mox2 zfs inherit volmode rpool/data/vm-105-disk-1", calls)
        self.assertNotIn("phase installed-confirmed", calls)


class FullRecoveryOrderTest(unittest.TestCase):
    def test_it_finishes_like_add_prod_vm(self) -> None:
        text = SCRIPT.read_text(encoding="utf-8")
        main = text[text.index("main() {"):]
        steps = ["restore_vm", "patch_offline", "verify_restored_guest", "apply_final_network_policy",
                 "ensure_replication_jobs", "ensure_ha_resource", "register_ingress_and_finalize",
                 "setup_workstation_jump_ssh", "monitoring_membership_sync"]
        positions = [main.index(step) for step in steps]
        self.assertEqual(positions, sorted(positions))


class FullRecoveryDashboardTest(unittest.TestCase):
    """The purpose, the request form, and the confirmation, driven as the
    dashboard drives them."""

    def test_the_dashboard_form_defaults_to_the_restore_point(self) -> None:
        self.check_form([])

    def test_domains_another_vm_routes_are_refused_in_the_form(self) -> None:
        prod1 = {"name": "prod1", "vmid": 100,
                 "domains": {"primary": "pinbinapp.com", "aliases": ["www.pinbinapp.com"]}}
        self.check_form([prod1])

    def check_form(self, registered: list) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            calls = root / "calls"
            (root / "resources.json").write_text(json.dumps(registered))
            (root / "allocation.json").write_text(json.dumps({"created": True, "resource": RECORD}))
            harness = root / "harness.sh"
            harness.write_text(textwrap.dedent(f"""\
                #!/usr/bin/env bash
                FULL_RECOVERY_SOURCE_ONLY=1 source {shlex.quote(str(SCRIPT))}
                bmac_ui_bootstrap "$@"
                RUN_DIR={shlex.quote(str(root))}
                ONLINE_NODES=(mox1 mox2)
                DEFAULT_REPLICATION_MINUTES=15
                find_existing_purpose() {{ :; }}
                validate_prod_host_capacity() {{ :; }}
                choose_vmid() {{ VMID=100; }}
                registry_cmd() {{ printf '%s\\n' "$*" >>{shlex.quote(str(calls))}; cat {shlex.quote(str(root / "allocation.json"))}; }}
                registry_update() {{ :; }}
                refresh_resource() {{ :; }}
                SNAPSHOT=vm/100/2026-10-27T16:04:23Z; BACKUP_TIME="2026-10-27 10:04 MDT"
                BACKUP_NAME=prod1; BACKUP_PURPOSE=pinbinapp; BACKUP_FQDN=pinbinapp.com
                BACKUP_CORES=64; BACKUP_MEMORY_MB=75776; BACKUP_DISK_BYTES={2**40}
                reserve_or_resume_recovery
                confirm_recovery
                bmac_ui_result cores "$VM_CORES" memory_gib "$VM_MEMORY_GIB" fqdn "$PRIMARY_DOMAIN"
                """))
            harness.chmod(0o755)
            seen = []

            def answer(reply):
                def respond(request):
                    seen.append(request)
                    return reply(request) if callable(reply) else reply
                return respond

            def defaults_with_new_compute(request):
                values = {field["id"]: field.get("default") for field in request["fields"]}
                values.update(cores=8, memory_gib=16, aliases="")
                return {"values": values}

            def taken_domains(request):
                values = {field["id"]: field.get("default") for field in request["fields"]}
                values.update(primary_domain="stage2prod1.pinbinapp.com", aliases="www.pinbinapp.com")
                return {"values": values}

            def free_domains(request):
                errors = request["validation_error"]["field_errors"]
                self.assertEqual(errors["primary_domain"],
                                 "stage2prod1.pinbinapp.com already routes to prod1; enter another FQDN")
                self.assertEqual(errors["aliases"], "Alias www.pinbinapp.com already routes to prod1")
                values = {field["id"]: field.get("default") for field in request["fields"]}
                values.update(primary_domain="test.pinbinapp.com", aliases="", cores=8, memory_gib=16)
                return {"values": values}

            taken = bool(registered)
            forms = [answer(taken_domains), answer(free_domains)] if taken else [answer(defaults_with_new_compute)]
            run = run_json([str(harness)], scripted(
                answer({"values": {"purpose": "pinbinapp-copy" if taken else "pinbinapp"}}),
                *forms,
                answer({"confirmed": True}),
            ), env={k: v for k, v in os.environ.items() if not k.startswith("BMAC_UI")}, timeout=60)
            self.assertEqual(run.returncode, 0, run.describe())
            purpose, form, *_, confirmation = seen
            self.assertEqual(purpose["field"]["default"], "pinbinapp")
            fields = {field["id"]: field for field in form["fields"]}
            if taken:
                # prod1 still routes the restore point's FQDN, so it isn't offered.
                self.assertFalse(fields["primary_domain"].get("default"))
                self.assertIn("The restore point's FQDN, pinbinapp.com, routes to prod1",
                              fields["primary_domain"]["help"])
                self.assertIn("--primary-domain test.pinbinapp.com", calls.read_text())
                return
            self.assertEqual(fields["primary_domain"]["default"], "pinbinapp.com")
            self.assertEqual((fields["cores"]["default"], fields["memory_gib"]["default"]), (64, 74))
            self.assertNotIn("disk_gib", fields)
            self.assertEqual(confirmation["title"], "Recover prod1?")
            # The registry is asked for the vCPUs and RAM chosen in the form.
            allocate = calls.read_text()
            for option in ("--index 1", "--cores 8", "--memory-mb 16384", "--disk-gib 1024",
                           "--primary-domain pinbinapp.com"):
                self.assertIn(option, allocate)


if __name__ == "__main__":
    unittest.main()
