#!/usr/bin/env python3

# Copyright (c) 2026 BEENTHERE VENTURES, INC.
# SPDX-License-Identifier: GPL-3.0-only

"""Mock-friendly tests for guarded staging VM creation and offline patching."""

from __future__ import annotations

import json
import os
from pathlib import Path
import subprocess
import tempfile
import unittest


# The config loader and the registry reject paths with symlinked components.
# macOS keeps TMPDIR under /var -> /private/var, so hand out resolved temp
# paths. A no-op where the temp directory is already a real path.
tempfile.tempdir = str(Path(tempfile.gettempdir()).resolve())

STAGING_DIR = Path(__file__).resolve().parent
CREATE_SCRIPT = STAGING_DIR / "create_staging_vm.sh"
PATCH_SCRIPT = STAGING_DIR / "patch_staging_clone.sh"
TREE_HELPER = STAGING_DIR / "patch_staging_guest_tree.py"


class StagingShellUnitTest(unittest.TestCase):
    def run_sourced(
        self,
        script: Path,
        source_guard: str,
        body: str,
        *,
        expected: int = 0,
    ) -> subprocess.CompletedProcess[str]:
        completed = subprocess.run(
            [
                "bash",
                "-c",
                f'{source_guard}=1 source "$1"; shift; {body}',
                "bash",
                str(script),
            ],
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

    def test_shell_syntax(self) -> None:
        completed = subprocess.run(
            ["bash", "-n", str(CREATE_SCRIPT), str(PATCH_SCRIPT)],
            text=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            check=False,
        )
        self.assertEqual(completed.returncode, 0, completed.stderr)

    def test_node_exec_preserves_argv_without_evaluation(self) -> None:
        completed = self.run_sourced(
            CREATE_SCRIPT,
            "APP_HA_STAGING_VM_SOURCE_ONLY",
            r"""
mox_ssh() { shift; "$@"; }
COORDINATOR=mox1
node_exec mox1 python3 -c \
  'import json, sys; print(json.dumps(sys.argv[1:]))' \
  'space ; $(must-not-run)' 'plain'
""",
        )
        self.assertEqual(
            json.loads(completed.stdout),
            ["space ; $(must-not-run)", "plain"],
        )

    def test_offline_patch_functions_replace_identity_and_install_sanitizer(
        self,
    ) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary) / "root"
            for relative in (
                "etc/netplan",
                "etc/cloud/cloud.cfg.d",
                "etc/ssh",
                "etc/apt/sources.list.d",
                "etc/apt/keyrings",
                "etc/systemd/system/multi-user.target.wants",
                "var/lib/cloud/instances/production",
                "var/lib/dbus",
                "var/lib/dhcp",
                "var/lib/NetworkManager",
                "var/lib/systemd/network",
                "var/lib/tailscale",
                "var/log",
                "usr/share/keyrings",
                "usr/local/lib",
            ):
                (root / relative).mkdir(parents=True, exist_ok=True)
            (root / "etc/hostname").write_text("prod1\n", encoding="utf-8")
            (root / "etc/hosts").write_text(
                "127.0.0.1 localhost\n127.0.1.1 prod1\n",
                encoding="utf-8",
            )
            (root / "etc/shadow").write_text(
                "root:$6$old$hash:20000:0:99999:7:::\n"
                "daemon:*:20000:0:99999:7:::\n",
                encoding="utf-8",
            )
            (root / "etc/shadow").chmod(0o640)
            (root / "etc/netplan/50-production.yaml").write_text(
                "production: true\n", encoding="utf-8"
            )
            (root / "etc/machine-id").write_text(
                "production-machine-id\n", encoding="utf-8"
            )
            (root / "var/lib/dbus/machine-id").write_text(
                "production-machine-id\n", encoding="utf-8"
            )
            (root / "etc/ssh/ssh_host_ed25519_key").write_text(
                "host-private-key\n", encoding="utf-8"
            )
            (root / "var/lib/cloud/instances/production/state").write_text(
                "old\n", encoding="utf-8"
            )
            (root / "var/lib/dhcp/dhclient.leases").write_text(
                "lease\n", encoding="utf-8"
            )
            (root / "var/lib/tailscale/tailscaled.state").write_text(
                "state\n", encoding="utf-8"
            )
            (root / "etc/apt/sources.list.d/tailscale.list").write_text(
                "repo\n", encoding="utf-8"
            )
            (root / "usr/share/keyrings/tailscale.gpg").write_text(
                "key\n", encoding="utf-8"
            )
            hash_file = Path(temporary) / "root.hash"
            hash_file.write_text("$6$staging$hashonly\n", encoding="utf-8")
            hash_file.chmod(0o600)
            sanitizer = Path(temporary) / "sanitizer.sh"
            sanitizer.write_text(
                "#!/usr/bin/env bash\nset -Eeuo pipefail\nexit 0\n",
                encoding="utf-8",
            )

            body = f"""
MOUNT_DIR={str(root)!r}
STAGING_NAME=stage1prod1
ADDRESS=10.213.0.51/24
GATEWAY=10.213.0.10
MAC=02:11:22:33:44:55
DNS_CSV=1.1.1.1,1.0.0.1
ROOT_HASH_FILE={str(hash_file)!r}
SANITIZER_FILE={str(sanitizer)!r}
patch_guest
"""
            completed = self.run_sourced(
                PATCH_SCRIPT,
                "APP_HA_STAGING_PATCH_SOURCE_ONLY",
                body,
            )
            self.assertNotIn("hashonly", completed.stdout + completed.stderr)
            self.assertEqual(
                (root / "etc/hostname").read_text(encoding="utf-8"),
                "stage1prod1\n",
            )
            hosts = (root / "etc/hosts").read_text(encoding="utf-8")
            self.assertIn("127.0.1.1 stage1prod1", hosts)
            self.assertNotIn("127.0.1.1 prod1\n", hosts)
            shadow = (root / "etc/shadow").read_text(encoding="utf-8")
            self.assertIn("root:$6$staging$hashonly:", shadow)
            self.assertEqual(
                (root / "etc/machine-id").read_text(encoding="utf-8"), ""
            )
            self.assertFalse((root / "etc/ssh/ssh_host_ed25519_key").exists())
            self.assertFalse(
                (root / "var/lib/cloud/instances/production").exists()
            )
            self.assertTrue((root / "etc/cloud/cloud-init.disabled").exists())
            self.assertFalse((root / "var/lib/dhcp/dhclient.leases").exists())
            self.assertTrue((root / "var/lib/tailscale").exists())
            self.assertTrue(
                (root / "etc/apt/sources.list.d/tailscale.list").exists()
            )

            netplan = json.loads(
                (root / "etc/netplan/90-app-ha-staging.yaml").read_text(
                    encoding="utf-8"
                )
            )
            private = netplan["network"]["ethernets"]["private"]
            self.assertEqual(private["addresses"], ["10.213.0.51/24"])
            self.assertEqual(
                private["routes"],
                [{"to": "default", "via": "10.213.0.10"}],
            )
            self.assertEqual(
                private["nameservers"]["addresses"], ["1.1.1.1", "1.0.0.1"]
            )
            self.assertFalse(private["dhcp4"])

            unit = (
                root
                / "etc/systemd/system/app-ha-staging-sanitizer.service"
            ).read_text(encoding="utf-8")
            self.assertIn("Before=network-pre.target network.target", unit)
            self.assertIn(
                "ExecStart=/usr/local/lib/app-ha/run-staging-sanitizer", unit
            )
            dropin = (
                root
                / "etc/systemd/system/network.target.d/"
                "50-app-ha-staging-sanitizer.conf"
            ).read_text(encoding="utf-8")
            self.assertIn(
                "Requires=app-ha-staging-sanitizer.service", dropin
            )
            ssh_unit = (
                root / "etc/systemd/system/app-ha-ssh-host-keys.service"
            ).read_text(encoding="utf-8")
            self.assertIn("ExecStart=/usr/bin/ssh-keygen -A", ssh_unit)
            self.assertIn("Before=ssh.service sshd.service ssh.socket", ssh_unit)
            runner = (
                root / "usr/local/lib/app-ha/run-staging-sanitizer"
            ).read_text(encoding="utf-8")
            self.assertLess(
                runner.index("staging-sanitizer-started"),
                runner.index("/usr/local/lib/app-ha/staging-sanitizer"),
            )

    def test_guest_tree_rejects_intermediate_symlink_escape(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary) / "root"
            outside = Path(temporary) / "outside"
            outside.mkdir()
            (outside / "machine-id").write_text("host-value\n", encoding="utf-8")
            for relative in (
                "etc/netplan",
                "etc/cloud/cloud.cfg.d",
                "etc/ssh",
                "etc/systemd",
                "var/lib",
                "var/log",
                "usr/local/lib",
            ):
                (root / relative).mkdir(parents=True, exist_ok=True)
            (root / "var/lib/dbus").symlink_to(outside, target_is_directory=True)
            (root / "etc/hostname").write_text("prod1\n", encoding="utf-8")
            (root / "etc/hosts").write_text("127.0.0.1 localhost\n", encoding="utf-8")
            (root / "etc/shadow").write_text(
                "root:$6$old$hash:20000:0:99999:7:::\n", encoding="utf-8"
            )
            (root / "etc/shadow").chmod(0o640)
            hash_file = Path(temporary) / "root.hash"
            hash_file.write_text("$6$staging$hashonly\n", encoding="utf-8")
            completed = subprocess.run(
                [
                    "python3",
                    str(TREE_HELPER),
                    "--root",
                    str(root),
                    "--name",
                    "stage1prod1",
                    "--address",
                    "10.213.0.51/24",
                    "--gateway",
                    "10.213.0.10",
                    "--mac",
                    "02:11:22:33:44:55",
                    "--dns",
                    "1.1.1.1",
                    "--root-hash-file",
                    str(hash_file),
                ],
                text=True,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                check=False,
            )
            self.assertNotEqual(completed.returncode, 0)
            self.assertEqual(
                (outside / "machine-id").read_text(encoding="utf-8"),
                "host-value\n",
            )

    def test_root_probe_uses_canonical_usr_lib_os_release(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            (root / "etc").mkdir()
            (root / "usr/lib").mkdir(parents=True)
            (root / "usr/lib/os-release").write_text(
                'ID=ubuntu\nID_LIKE="debian"\n',
                encoding="utf-8",
            )
            (root / "etc/os-release").symlink_to("../usr/lib/os-release")
            (root / "etc/fstab").write_text(
                "UUID=test / ext4 defaults 0 1\n",
                encoding="utf-8",
            )
            (root / "etc/shadow").write_text(
                "root:*:20000:0:99999:7:::\n",
                encoding="utf-8",
            )
            probe = subprocess.run(
                [
                    "python3",
                    str(TREE_HELPER),
                    "--probe-ubuntu-root",
                    str(root),
                ],
                check=False,
            )
            self.assertEqual(probe.returncode, 0)
            diagnose = subprocess.run(
                [
                    "python3",
                    str(TREE_HELPER),
                    "--diagnose-ubuntu-root",
                    str(root),
                ],
                check=False,
                capture_output=True,
                text=True,
            )
            self.assertEqual(diagnose.returncode, 0)
            self.assertIn("/usr/lib/os-release", diagnose.stderr)

    def test_failed_unmount_preserves_mount_state_and_is_loud(self) -> None:
        body = """
MOUNTED=true
MOUNT_DIR=/run/app-ha-test-mounted-root
DEVICE=/dev/zvol/rpool/data/vm-200-disk-0
RESOLVED_DEVICE=/dev/zd999
FINISHED=false
sync() { :; }
umount() { return 1; }
rmdir() { return 0; }
set +e
( exit 7 )
emergency_cleanup
"""
        completed = self.run_sourced(
            PATCH_SCRIPT,
            "APP_HA_STAGING_PATCH_SOURCE_ONLY",
            body,
            expected=7,
        )
        self.assertIn("mount state is preserved for recovery", completed.stderr)
        self.assertIn("manual recovery is required", completed.stderr)

    def test_offline_mapping_rejects_stopped_vm_disk_reference(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            bin_dir = Path(temporary) / "bin"
            bin_dir.mkdir()
            calls = Path(temporary) / "pvesh-calls"
            pvesh = bin_dir / "pvesh"
            pvesh.write_text(
                "#!/usr/bin/env bash\n"
                f"printf '%s\\n' \"$*\" >>{str(calls)!r}\n"
                "case \"$2\" in\n"
                "  /cluster/resources)\n"
                "    printf '%s\\n' "
                "'[{\"vmid\":300,\"type\":\"qemu\",\"node\":\"mox1\"}]'\n"
                "    ;;\n"
                "  /nodes/mox1/qemu/300/config)\n"
                "    printf '%s\\n' "
                "'{\"name\":\"stopped-guest\","
                "\"unused0\":\"local-zfs:vm-200-disk-0,size=1G\"}'\n"
                "    ;;\n"
                "  *) exit 9 ;;\n"
                "esac\n",
                encoding="utf-8",
            )
            pvesh.chmod(0o755)
            completed = self.run_sourced(
                PATCH_SCRIPT,
                "APP_HA_STAGING_PATCH_SOURCE_ONLY",
                f"""
PATH={str(bin_dir)!r}:$PATH
VOLUME_ID=local-zfs:vm-200-disk-0
assert_not_referenced_by_vm
""",
                expected=1,
            )
            self.assertIn(
                "VM 300 still references local-zfs:vm-200-disk-0",
                completed.stderr,
            )
            self.assertIn(
                "get /nodes/mox1/qemu/300/config --output-format json",
                calls.read_text(encoding="utf-8"),
            )

    def test_creation_order_and_rollback_guards_are_explicit(self) -> None:
        text = CREATE_SCRIPT.read_text(encoding="utf-8")
        for required in (
            "qm snapshot",
            "record-staging-snapshot-intent",
            "pvesr schedule-now",
            "record-staging-snapshot",
            "zfs clone",
            "qm create",
            "link_down=1",
            "--routes-enabled",
            "REMOTE_HAPROXY_SYNC",
            "qm destroy",
            "qm delsnapshot",
            "defer-cleanup",
            "ALLOW UNSANITIZED STAGING NETWORK",
        ):
            self.assertIn(required, text)
        self.assertNotIn("zfs snapshot", text)
        self.assertNotIn("--destroy-unreferenced-disks 1", text)
        self.assertIn("--destroy-unreferenced-disks 0", text)
        snapshot_body = text[
            text.index("create_source_snapshot() {") :
            text.index("replication_status_target() {")
        ]
        self.assertLess(
            snapshot_body.index("record-staging-snapshot-intent"),
            snapshot_body.index("qm snapshot"),
        )
        self.assertIn("--efidisk0", text)
        self.assertIn('info "LAN name: lan0"', text)
        self.assertLess(
            text.index("patch_clone_offline\n"),
            text.index("attach_patched_disk\n"),
        )
        self.assertLess(
            text.index("attach_patched_disk\n"),
            text.index("offer_start\n"),
        )
        self.assertIn('[[ "$PATCH_SUCCEEDED" == true ]]', text)
        self.assertIn(
            'node_exec "$STAGING_NODE" bash "${args[@]}"',
            text,
        )
        self.assertIn(
            "currently expected "
            'stage${STAGING_CANDIDATE_INDEX}${SOURCE_NAME}.'
            "${SOURCE_PRIMARY_DOMAIN}",
            text,
        )
        self.assertIn(
            'state = "busy" if row.get("pid") else "ready"',
            text,
        )
        self.assertIn(
            "waiting for it to finish",
            text,
        )
        self.assertIn(
            "Start VM automatically after it has been created?",
            text,
        )
        self.assertIn(
            "Set up permanent jump SSH after the VM has started?",
            text,
        )
        self.assertNotIn(
            'prompt_yes "Start ${STAGING_NAME} now?"',
            text,
        )
        self.assertNotIn(
            "Set up permanent jump SSH from this workstation",
            text,
        )
        self.assertIn(
            "Waiting up to 180 seconds for QGA",
            text,
        )
        self.assertNotIn(
            "blank defaults to stageN",
            text,
        )

    def test_failure_before_allocation_does_not_claim_resources_were_removed(
        self,
    ) -> None:
        completed = self.run_sourced(
            CREATE_SCRIPT,
            "APP_HA_STAGING_VM_SOURCE_ONLY",
            """
CURRENT_PHASE="validating live source"
REGISTRY_RESERVED=false
SNAPSHOT_MAY_EXIST=false
CLONE_MAY_EXIST=false
VM_MAY_EXIST=false
ROUTES_ENABLED=false
rollback_failed_creation
""",
        )
        self.assertIn(
            "No staging resources had been allocated",
            completed.stderr,
        )
        self.assertNotIn(
            "Rollback removed the VM",
            completed.stderr,
        )

    def validate_sanitizer(
        self, sanitizer: Path, prelude: str = "", *, expected: int = 0
    ) -> subprocess.CompletedProcess[str]:
        return self.run_sourced(
            CREATE_SCRIPT,
            "APP_HA_STAGING_VM_SOURCE_ONLY",
            f"{prelude}\nSANITIZER_FILE={str(sanitizer)!r}\nvalidate_sanitizer_file\n",
            expected=expected,
        )

    def test_sanitizer_size_limit_is_exact_and_fails_closed(self) -> None:
        header = b"#!/bin/bash\n"
        with tempfile.TemporaryDirectory() as temporary:
            sanitizer = Path(temporary) / "startup.sh"

            sanitizer.write_bytes(header + b"#" * (1048576 - len(header) - 1) + b"\n")
            self.assertEqual(sanitizer.stat().st_size, 1048576)
            self.validate_sanitizer(sanitizer)

            sanitizer.write_bytes(header + b"#" * (1048576 - len(header)) + b"\n")
            self.assertEqual(sanitizer.stat().st_size, 1048577)
            too_large = self.validate_sanitizer(sanitizer, expected=1)
            self.assertIn("exceeds the 1 MiB limit", too_large.stderr)

            sanitizer.write_bytes(header + b"true\n")
            # BSD wc pads its count with spaces.
            self.validate_sanitizer(sanitizer, "wc() { printf '      17\\n'; }")
            padded_large = self.validate_sanitizer(
                sanitizer, "wc() { printf ' 1048577\\n'; }", expected=1
            )
            self.assertIn("exceeds the 1 MiB limit", padded_large.stderr)

            # A wc that fails or prints nothing must never read as size 0.
            for broken in (
                "wc() { return 1; }",
                "wc() { printf '\\n'; }",
                "wc() { printf '12+5\\n'; }",
            ):
                with self.subTest(wc=broken):
                    rejected = self.validate_sanitizer(
                        sanitizer, broken, expected=1
                    )
                    self.assertIn(
                        "Could not measure the sanitizer size", rejected.stderr
                    )

    def test_openssl_without_sha512_crypt_stops_before_config_or_ssh(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            touched = Path(temporary) / "touched"
            completed = self.run_sourced(
                CREATE_SCRIPT,
                "APP_HA_STAGING_VM_SOURCE_ONLY",
                f"""
openssl() {{
  echo "unknown option '-6'" >&2
  return 1
}}
ssh() {{ echo ssh >>{str(touched)!r}; }}
load_and_validate_config() {{ echo config >>{str(touched)!r}; }}
main --dry-run
""",
                expected=1,
            )
            self.assertIn("openssl passwd -6", completed.stderr)
            self.assertIn("brew install openssl@3", completed.stderr)
            self.assertFalse(touched.exists())

        accepted = self.run_sourced(
            CREATE_SCRIPT,
            "APP_HA_STAGING_VM_SOURCE_ONLY",
            "openssl() { printf '%s\\n' '$6$appha0probe$synthetic'; }\n"
            "require_openssl_sha512_crypt && echo accepted\n",
        )
        self.assertIn("accepted", accepted.stdout)

    def test_generated_snapshot_name_fits_proxmox_limit(self) -> None:
        text = CREATE_SCRIPT.read_text(encoding="utf-8")
        representative = "stg-base-stage10prod10-260920T203000Z"
        self.assertLess(len(representative), 40)
        self.assertIn(
            'SNAPSHOT_NAME="stg-base-${STAGING_NAME}-$(date -u +%y%m%dT%H%M%SZ)"',
            text,
        )
        self.assertIn(
            "Proxmox's 40-character limit",
            text,
        )

    def test_offline_shell_contains_no_guest_tree_mutators(self) -> None:
        text = PATCH_SCRIPT.read_text(encoding="utf-8")
        for removed_function in (
            "atomic_install()",
            "write_hostname()",
            "patch_hosts()",
            "patch_shadow()",
            "write_netplan()",
            "clear_identity_state()",
            "remove_tailscale_state()",
            "install_sanitizer_service()",
        ):
            self.assertNotIn(removed_function, text)
        self.assertIn("--probe-ubuntu-root", text)
        self.assertIn('python3 "$GUEST_TREE_HELPER" "${helper_args[@]}"', text)
        self.assertIn('blkid -p -o value -s TYPE -- "$record"', text)
        self.assertNotIn("partx", text)
        self.assertIn("offline patch failure diagnostics", text)
        self.assertIn("--diagnose-ubuntu-root", text)
        create_text = CREATE_SCRIPT.read_text(encoding="utf-8")
        self.assertIn("-o volmode=full", create_text)
        self.assertIn('zfs set volmode=dev "$dataset"', create_text)

    def test_route_cleanup_uses_only_discovered_cluster_nodes(self) -> None:
        completed = self.run_sourced(
            CREATE_SCRIPT,
            "APP_HA_STAGING_VM_SOURCE_ONLY",
            r"""
declare -a CLEANUP_CALLS=()
registry_cmd() { CLEANUP_CALLS+=("$*"); }
CLUSTER_NODES=(mox1 mox2)
REGISTRY_RESERVED=true
STAGING_NAME=stage1prod1
record_deferred_route_cleanup
printf '%s\n' "${CLEANUP_CALLS[@]}"
""",
        )
        self.assertEqual(completed.stdout.count("--action remove-route"), 2)
        self.assertIn("--node mox1", completed.stdout)
        self.assertIn("--node mox2", completed.stdout)
        self.assertNotIn("--node mox3", completed.stdout)


class StagingDryRunTest(unittest.TestCase):
    def test_dry_run_reads_live_state_without_mutation_or_secret_export(
        self,
    ) -> None:
        artifacts = STAGING_DIR / "artifacts"
        artifacts_preexisted = artifacts.exists()
        runs_before = set(artifacts.glob("run-*")) if artifacts.exists() else set()
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            env_dir = root / "env"
            bin_dir = root / "bin"
            env_dir.mkdir()
            bin_dir.mkdir()
            calls = root / "ssh-calls.jsonl"
            cluster = env_dir / "cluster.conf"
            secrets = env_dir / "secrets.env"
            cluster.write_text(
                "\n".join(
                    (
                        "PROXMOX_CLUSTER_NAME=app-ha-test",
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
                        "PRODUCTION_VM_TAG=app-ha-production",
                        "STAGING_VM_TAG=app-ha-staging",
                        "EVICTABLE_VM_TAG=evictable",
                        "PURPOSE_TAG_PREFIX=purpose-",
                        "GUEST_ROLE_HOOK_PATH=hosts/app-ha-guest-role-hook.sh",
                        "CLUSTER_STATE_DIR=/etc/pve/priv/app-ha",
                        "PROD_VM_STORAGE=local-zfs",
                        "PROD_VM_CPU_TYPE=x86-64-v2-AES",
                        "STAGING_VM_CORES=2",
                        "STAGING_VM_MEMORY_GIB=4",
                        "STAGING_VM_STORAGE=local-zfs",
                        "STAGING_VM_BRIDGE=vmbr-private",
                        "SNIPPETS_STORAGE_ID=local",
                        "GUEST_DNS_SERVERS=1.1.1.1,1.0.0.1",
                        "",
                    )
                ),
                encoding="utf-8",
            )
            secrets.write_text(
                "STAGING_GUEST_VM_ROOT_PASSWORD=synthetic-secret-value\n",
                encoding="utf-8",
            )
            for index in (1, 2):
                (env_dir / f"mox{index}.conf").write_text(
                    "\n".join(
                        (
                            f"NVME_MIRROR_1_SERIAL_1=test-mox{index}-a",
                            f"NVME_MIRROR_1_SERIAL_2=test-mox{index}-b",
                            f"PROXMOX_IP=192.0.2.{index}",
                            "PROXMOX_GATEWAY=192.0.2.254",
                            "PROXMOX_PREFIX=24",
                            f"TAILSCALE_HOSTNAME=mox{index}",
                            f"PROXMOX_PUBLIC_MAC=02:00:00:00:00:{index:02x}",
                            f"PROXMOX_SECONDARY_MAC=02:00:00:00:01:{index:02x}",
                            f"HAPROXY_LXC_VMID={9110 + index}",
                            f"HAPROXY_LXC_HOSTNAME=haproxy{index}",
                            f"HAPROXY_LXC_IP=10.213.0.{20 + index}/24",
                            f"HAPROXY_LXC_GATEWAY=10.213.0.{10 + index}",
                            f"VRRP_PRIORITY={200 - index}",
                            "MAX_PROD_VM_COUNT_ON_THIS_HOST=1",
                            "MAX_STAGING_VM_COUNT_ON_THIS_HOST=5",
                            "",
                        )
                    ),
                    encoding="utf-8",
                )
            cluster.chmod(0o644)
            secrets.chmod(0o600)

            production = {
                "schema_version": 1,
                "record_type": "resource",
                "id": "00000000-0000-4000-8000-000000000001",
                "allocation_id": "prod-test",
                "request_fingerprint": "a" * 64,
                "kind": "production",
                "name": "prod1",
                "index": 1,
                "source": None,
                "vmid": 100,
                "ip": "10.213.0.31",
                "mac": "02:11:22:33:44:55",
                "state": "active",
                "purpose": {"slug": "myapp", "source_resource": None},
                "domains": {
                    "primary": "myapp.com",
                    "aliases": [],
                    "staging_base": "staging.myapp.com",
                },
                "placement": ["mox1", "mox2"],
                "initial_node": "mox1",
                "owner_node": "mox1",
                "routes_enabled": True,
                "spec": {
                    "cores": 4,
                    "memory_mb": 8192,
                    "disk_gib": 128,
                    "disk_allocation": "reserved",
                    "replication_interval": "15",
                },
                "proxmox": {
                    "ha_nodes": ["mox1", "mox2"],
                    "replication_targets": ["mox2"],
                    "volume_id": "local-zfs:vm-100-disk-0",
                    "snapshot": None,
                },
                "created_at": "2026-09-14T20:00:00+00:00",
                "updated_at": "2026-09-14T20:00:00+00:00",
                "revision": 5,
            }
            fake_ssh = bin_dir / "ssh"
            fake_ssh.write_text(
                f"""#!/usr/bin/env python3
import json
import os
import shlex
import sys

args = sys.argv[1:]
command_args = []
for argument in args:
    command_args.extend(shlex.split(argument))
command_text = " ".join(command_args)
payload = sys.stdin.read()
with open(os.environ["FAKE_SSH_CALLS"], "a", encoding="utf-8") as stream:
    stream.write(json.dumps({{
        "args": args,
        "stdin": payload,
        "secret_exported": "STAGING_GUEST_VM_ROOT_PASSWORD" in os.environ,
    }}) + "\\n")

if command_args[-1:] == ["true"]:
    raise SystemExit(0)

if "pvesh" in command_args:
    index = command_args.index("pvesh")
    command = command_args[index + 1 :]
    path = command[1] if len(command) > 1 else ""
    if path == "/cluster/status":
        value = [{{"type": "cluster", "name": "app-ha-test", "quorate": 1}}]
    elif path == "/nodes":
        value = [
            {{"node": "mox1", "status": "online"}},
            {{"node": "mox2", "status": "online"}},
        ]
    elif path == "/cluster/resources":
        value = [
            {{
                "vmid": 100,
                "name": "prod1",
                "node": "mox1",
                "status": "running",
                "type": "qemu",
            }}
        ]
    elif path == "/cluster/ha/resources":
        value = [{{
            "sid": "vm:100",
            "state": "started",
            "failback": 0,
            "auto-rebalance": 0,
        }}]
    elif path == "/cluster/replication":
        value = [{{"id": "100-0", "guest": 100, "target": "mox2"}}]
    elif path == "/nodes/mox1/replication/100-0/status":
        value = {{
            "id": "100-0",
            "target": "mox2",
            "last_sync": 1,
            "fail_count": 0,
        }}
    elif path == "/cluster/nextid":
        value = 200
    else:
        raise SystemExit(f"unexpected pvesh path: {{path}}")
    print(json.dumps(value))
elif any(value.endswith("/cluster_registry.py") for value in command_args):
    if (
        "record-staging-snapshot --help" in command_text
        or "record-staging-snapshot-intent --help" in command_text
    ):
        print("record staging snapshot help")
    elif command_args[-1:] == ["list"]:
        print({json.dumps([production])!r})
    else:
        raise SystemExit(f"dry run attempted registry mutation: {{args}}")
elif "ha-manager" in payload and "rules" in payload:
    print(json.dumps([
        {{
            "id": "app-ha-prod1-100",
            "type": "node-affinity",
            "resources": ["vm:100"],
            "nodes": ["mox1:1", "mox2:1"],
            "strict": 1,
            "affinity": "positive",
        }}
    ]))
elif "qm config" in payload:
    print("\\n".join([
        "name: prod1",
        "bios: ovmf",
        "machine: q35",
        "agent: enabled=1,fstrim_cloned_disks=1",
        "tags: app-ha-production;purpose-myapp",
        "net0: virtio=02:11:22:33:44:55,bridge=vmbr-private,firewall=1",
        "scsi0: local-zfs:vm-100-disk-0,discard=on,replicate=1,size=128G",
        "efidisk0: local-zfs:vm-100-disk-1,efitype=4m,replicate=1,size=4M",
    ]))
elif "qm agent" in payload and " ping" in payload:
    pass
elif "qm guest exec" in payload and "lsblk" in payload:
    layout = {{
        "blockdevices": [
            {{
                "name": "/dev/sda",
                "type": "disk",
                "fstype": None,
                "mountpoints": [None],
                "children": [
                    {{
                        "name": "/dev/sda1",
                        "type": "part",
                        "fstype": "vfat",
                        "mountpoints": ["/boot/efi"],
                    }},
                    {{
                        "name": "/dev/sda2",
                        "type": "part",
                        "fstype": "ext4",
                        "mountpoints": ["/"],
                    }},
                ],
            }}
        ]
    }}
    print(json.dumps({{
        "exited": 1,
        "exitcode": 0,
        "out-data": json.dumps(layout),
    }}))
elif "pvesm status" in payload:
    pass
elif "uname -m" in payload:
    print("x86_64")
elif "pveversion" in payload and "for command" not in payload:
    print("pve-manager/9.0.1/test")
elif payload:
    pass
else:
    raise SystemExit(f"unexpected ssh command: {{args}}")
""",
                encoding="utf-8",
            )
            fake_ssh.chmod(0o755)
            environment = os.environ.copy()
            environment.update(
                {
                    "PATH": f"{bin_dir}:{environment['PATH']}",
                "APP_HA_CONFIG_TEST_MODE": "1",
                "APP_HA_ENV_DIR": str(env_dir),
                    "FAKE_SSH_CALLS": str(calls),
                }
            )
            completed = subprocess.run(
                [str(CREATE_SCRIPT), "--dry-run"],
                input=(
                    "\n"  # default prod1
                    "\n"  # default vCPU cores
                    "\n"  # default RAM GiB
                    "\n"  # start automatically
                    "\n"  # set up jump SSH
                    "\n"  # no domain override
                    "\n"  # network enabled
                    "\n"  # no optional sanitizer
                    "GO\n"
                ),
                text=True,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                env=environment,
                check=False,
            )
            self.assertEqual(
                completed.returncode,
                0,
                msg=f"stdout:\n{completed.stdout}\nstderr:\n{completed.stderr}",
            )
            self.assertIn("Registered production resources", completed.stdout)
            self.assertIn("myapp.com", completed.stdout)
            self.assertIn("Candidate identity (not reserved)", completed.stdout)
            self.assertIn("stage1prod1 / 10.213.0.51", completed.stdout)
            self.assertIn("stage1prod1.myapp.com", completed.stdout)
            self.assertIn("NIC link-down: false", completed.stdout)
            self.assertIn("Compute: 2 vCPU / 4 GiB RAM", completed.stdout)
            self.assertIn("Start after creation: true", completed.stdout)
            self.assertIn("Set up workstation jump SSH: true", completed.stdout)
            self.assertIn(
                "NO application sanitizer was supplied", completed.stderr
            )
            self.assertNotIn(
                "synthetic-secret-value", completed.stdout + completed.stderr
            )
            calls_text = calls.read_text(encoding="utf-8")
            self.assertNotIn('"secret_exported": true', calls_text)
            self.assertNotIn("synthetic-secret-value", calls_text)
            for mutation in (
                "allocate-staging",
                "qm snapshot",
                "pvesr schedule-now",
                "zfs clone",
                "qm create",
                "qm set",
            ):
                self.assertNotIn(mutation, calls_text)

        runs_after = set(artifacts.glob("run-*")) if artifacts.exists() else set()
        self.assertEqual(runs_after - runs_before, set(), "dry run leaked artifacts")
        if not artifacts_preexisted and artifacts.exists():
            artifacts.rmdir()


if __name__ == "__main__":
    unittest.main()
