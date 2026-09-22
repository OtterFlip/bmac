#!/usr/bin/env python3

# Copyright (c) 2026 BEENTHERE VENTURES, INC.
# SPDX-License-Identifier: GPL-3.0-only

"""Mock-friendly tests for production VM creation and ISO rendering."""

from __future__ import annotations

import hashlib
import importlib.util
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest


# The config loader and the registry reject paths with symlinked components.
# macOS keeps TMPDIR under /var -> /private/var, so hand out resolved temp
# paths. A no-op where the temp directory is already a real path.
tempfile.tempdir = str(Path(tempfile.gettempdir()).resolve())

PROD_DIR = Path(__file__).resolve().parent
SCRIPT = PROD_DIR / "create_prod_vm.sh"
BUILDER_PATH = PROD_DIR / "build_ubuntu_autoinstall.py"
PREPARER_PATH = PROD_DIR / "prepare_prod_iso.sh"

SPEC = importlib.util.spec_from_file_location("build_ubuntu_autoinstall", BUILDER_PATH)
assert SPEC and SPEC.loader
builder = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(builder)


def request_fixture() -> dict:
    return {
        "hostname": "prod1",
        "address": "10.213.0.31/24",
        "gateway": "10.213.0.10",
        "production_ip_start": "10.213.0.31",
        "production_ip_end": "10.213.0.50",
        "nameservers": ["1.1.1.1", "2606:4700:4700::1111"],
        "mac": "02:11:22:33:44:55",
        "root_password_hash": "$6$testsalt$not-a-plaintext-password-value",
        "authorized_keys": [
            "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIAAAAAAAAAAAAAAAAAAAAA admin-one",
            "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIBBBBBBBBBBBBBBBBBBBBB admin-two",
        ],
        "startup_script": None,
    }


class AutoinstallBuilderTest(unittest.TestCase):
    def test_rendered_config_is_static_root_key_only_and_has_qga(self) -> None:
        rendered = builder.build_user_data(request_fixture())
        self.assertTrue(rendered.startswith("#cloud-config\n"))
        document = json.loads(rendered.split("\n", 1)[1])
        autoinstall = document["autoinstall"]

        network = autoinstall["network"]["ethernets"]["private"]
        self.assertEqual(network["match"]["macaddress"], "02:11:22:33:44:55")
        self.assertEqual(network["set-name"], "lan0")
        self.assertEqual(network["addresses"], ["10.213.0.31/24"])
        self.assertEqual(
            network["routes"], [{"to": "default", "via": "10.213.0.10"}]
        )
        self.assertFalse(network["dhcp4"])
        self.assertIn("qemu-guest-agent", autoinstall["packages"])
        self.assertFalse(autoinstall["ssh"]["allow-pw"])
        self.assertEqual(autoinstall["shutdown"], "poweroff")

        root = autoinstall["user-data"]["users"][0]
        self.assertEqual(root["name"], "root")
        self.assertFalse(root["lock_passwd"])
        self.assertEqual(
            root["hashed_passwd"],
            request_fixture()["root_password_hash"],
        )
        self.assertNotIn("passwd", root)
        self.assertEqual(len(root["ssh_authorized_keys"]), 2)
        sshd = autoinstall["user-data"]["write_files"][0]["content"]
        self.assertIn("PermitRootLogin prohibit-password", sshd)
        self.assertIn("PasswordAuthentication no", sshd)
        self.assertNotIn("tailscale", rendered.lower())

    def test_startup_script_is_installed_as_retry_safe_once_service(self) -> None:
        request = request_fixture()
        request["startup_script"] = "#!/bin/sh\n# Must be idempotent.\ntouch /root/started\n"
        rendered = builder.build_user_data(request)
        autoinstall = json.loads(rendered.split("\n", 1)[1])["autoinstall"]
        commands = "\n".join(autoinstall["late-commands"])
        self.assertIn("/cdrom/nocloud/startup.sh", commands)
        self.assertIn("production-startup-once.service", commands)
        self.assertIn("Before=network-pre.target", builder.STARTUP_UNIT)
        self.assertIn("ConditionPathExists=!", builder.STARTUP_UNIT)
        self.assertIn("MUST be idempotent", builder.STARTUP_RUNNER)
        self.assertIn("state_dir=/var/lib/production-startup", builder.STARTUP_RUNNER)
        self.assertIn('marker="${state_dir}/completed"', builder.STARTUP_RUNNER)

    def test_request_rejects_wrong_network_hash_and_key_count(self) -> None:
        invalid = request_fixture()
        invalid["gateway"] = "10.213.1.1"
        with self.assertRaisesRegex(builder.BuildError, "in the guest /24"):
            builder.validate_request(invalid)

        invalid = request_fixture()
        invalid["root_password_hash"] = "plaintext"
        with self.assertRaisesRegex(builder.BuildError, "SHA-512"):
            builder.validate_request(invalid)

        invalid = request_fixture()
        invalid["authorized_keys"] = invalid["authorized_keys"][:1]
        with self.assertRaisesRegex(builder.BuildError, "exactly two"):
            builder.validate_request(invalid)

        invalid = request_fixture()
        invalid["hostname"] = "prod2"
        with self.assertRaisesRegex(builder.BuildError, "registry formula"):
            builder.validate_request(invalid)

    def test_grub_entry_uses_escaped_local_nocloud_seed(self) -> None:
        result = builder.inject_autoinstall_menu(
            "set default=1\nset timeout=30\nmenuentry 'Try Ubuntu' {}\n"
        )
        self.assertTrue(result.startswith("set default=0\nset timeout=5\n"))
        self.assertIn(
            r"autoinstall ds=nocloud\;s=/cdrom/nocloud/ ---",
            result,
        )
        self.assertEqual(result.count("set default="), 1)
        self.assertEqual(result.count("set timeout="), 1)

    def test_media_check_manifest_tracks_customized_files(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            grub = root / "grub.cfg"
            user_data = root / "user-data"
            grub.write_text("new grub\n", encoding="utf-8")
            user_data.write_text("new seed\n", encoding="utf-8")
            manifest = builder.update_md5_manifest(
                "0" * 32 + "  ./boot/grub/grub.cfg\n",
                {
                    "boot/grub/grub.cfg": grub,
                    "nocloud/user-data": user_data,
                },
            )
            self.assertIn(
                f"{builder.md5_file(grub)}  ./boot/grub/grub.cfg", manifest
            )
            self.assertIn(
                f"{builder.md5_file(user_data)}  ./nocloud/user-data", manifest
            )

    def test_iso_build_replays_boot_metadata_and_is_atomic(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            source = root / "ubuntu-26.04.1-live-server-amd64.iso"
            source.write_bytes(b"synthetic source image")
            expected = hashlib.sha256(source.read_bytes()).hexdigest()
            output = root / "artifacts" / "prod1.iso"
            xorriso = root / "xorriso"
            xorriso.write_text("#!/bin/sh\nexit 0\n", encoding="utf-8")
            xorriso.chmod(0o755)
            calls: list[list[str]] = []

            def fake_run(
                command: list[str], **_: object
            ) -> subprocess.CompletedProcess[str]:
                calls.append(command)
                if "-extract" in command:
                    destination = Path(command[-1])
                    destination.write_text(
                        "set timeout=30\nmenuentry 'Ubuntu' {}\n",
                        encoding="utf-8",
                    )
                    # Real Ubuntu ISO files carry read-only Rock Ridge modes.
                    destination.chmod(0o444)
                if "-outdev" in command:
                    candidate = Path(command[command.index("-outdev") + 1])
                    candidate.write_bytes(b"synthetic custom image")
                return subprocess.CompletedProcess(command, 0, "", "")

            builder.build_iso(
                source=source,
                expected_sha256=expected,
                output=output,
                request=request_fixture(),
                xorriso=xorriso,
                runner=fake_run,
            )
            self.assertEqual(output.read_bytes(), b"synthetic custom image")
            self.assertEqual(output.stat().st_mode & 0o777, 0o600)
            build_call = next(command for command in calls if "-outdev" in command)
            self.assertIn(
                ["-boot_image", "any", "replay"],
                [build_call[index : index + 3] for index in range(len(build_call) - 2)],
            )
            self.assertIn("/nocloud", build_call)
            self.assertIn("/boot/grub/grub.cfg", build_call)
            self.assertIn("/md5sum.txt", build_call)
            self.assertFalse(any(output.parent.glob(".*-iso.*")))

    def test_source_type_and_hash_are_checked_before_build(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            wrong_name = root / "ubuntu-live-server-amd64.img"
            wrong_name.write_bytes(b"image")
            digest = hashlib.sha256(b"image").hexdigest()
            with self.assertRaisesRegex(builder.BuildError, "ISO file"):
                builder.validate_source_iso(wrong_name, digest)

            source = root / f"{digest}.iso"
            source.write_bytes(b"image")
            with self.assertRaisesRegex(builder.BuildError, "mismatch"):
                builder.validate_source_iso(source, "0" * 64)


# prepare_prod_iso.sh only ever runs on a Proxmox host and needs findmnt, flock,
# GNU stat, and GNU df. Gate on the platform, not on one tool being present: on
# Linux a missing prerequisite must fail loudly rather than skip this test.
@unittest.skipUnless(
    sys.platform.startswith("linux"), "prepare_prod_iso.sh is Proxmox-host only"
)
class ProductionIsoPreparerTest(unittest.TestCase):
    def test_downloads_verifies_and_reuses_hash_addressed_cache(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            bin_dir = root / "bin"
            cache = root / "cache"
            output = root / "iso-storage" / "prod1.iso"
            request = root / "request.json"
            source = root / "download-source.iso"
            calls = root / "curl-calls"
            builder_script = root / "builder"
            bin_dir.mkdir()
            output.parent.mkdir()
            request.write_text("{}\n", encoding="utf-8")
            source.write_bytes(b"reviewed source ISO bytes")
            expected = hashlib.sha256(source.read_bytes()).hexdigest()

            fake_curl = bin_dir / "curl"
            fake_curl.write_text(
                """#!/usr/bin/env python3
import os
from pathlib import Path
import shutil
import sys

args = sys.argv[1:]
with open(os.environ["FAKE_CURL_CALLS"], "a", encoding="utf-8") as stream:
    stream.write("called\\n")
if os.environ.get("FAKE_CURL_FAIL") == "1":
    raise SystemExit(9)
output = Path(args[args.index("--output") + 1])
shutil.copyfile(os.environ["FAKE_SOURCE_ISO"], output)
""",
                encoding="utf-8",
            )
            fake_curl.chmod(0o755)
            fake_xorriso = bin_dir / "xorriso"
            fake_xorriso.write_text("#!/bin/sh\nexit 0\n", encoding="utf-8")
            fake_xorriso.chmod(0o755)
            builder_script.write_text(
                """#!/usr/bin/env python3
from pathlib import Path
import sys

args = sys.argv[1:]
source = Path(args[args.index("--source-iso") + 1])
output = Path(args[args.index("--output-iso") + 1])
assert source.is_file()
output.write_bytes(b"custom autoinstall ISO")
""",
                encoding="utf-8",
            )
            builder_script.chmod(0o755)

            environment = os.environ.copy()
            environment.update(
                {
                    "PATH": f"{bin_dir}:{environment['PATH']}",
                    "APP_HA_PREPARE_ISO_TEST_MODE": "1",
                    "APP_HA_PREPARE_ISO_CACHE_ROOT": str(cache),
                    "APP_HA_PREPARE_ISO_BUILDER": str(builder_script),
                    "FAKE_CURL_CALLS": str(calls),
                    "FAKE_SOURCE_ISO": str(source),
                }
            )
            command = [
                str(PREPARER_PATH),
                "--source-url",
                "https://example.test/ubuntu-live-server.iso",
                "--expected-sha256",
                expected,
                "--install-mode",
                "ubuntu-autoinstall",
                "--request",
                str(request),
                "--output-iso",
                str(output),
            ]

            first = subprocess.run(
                command,
                env=environment,
                text=True,
                capture_output=True,
                check=False,
            )
            self.assertEqual(first.returncode, 0, first.stderr)
            first_result = json.loads(first.stdout)
            cached_iso = cache / f"{expected}.iso"
            sidecar = cache / f"{expected}.iso.sha256"
            self.assertEqual(cached_iso.read_bytes(), source.read_bytes())
            self.assertEqual(
                sidecar.read_text(encoding="utf-8"),
                f"{expected}  {expected}.iso\n",
            )
            self.assertFalse(first_result["cache_reused"])
            self.assertEqual(calls.read_text(encoding="utf-8"), "called\n")

            output.unlink()
            environment["FAKE_CURL_FAIL"] = "1"
            second = subprocess.run(
                command,
                env=environment,
                text=True,
                capture_output=True,
                check=False,
            )
            self.assertEqual(second.returncode, 0, second.stderr)
            second_result = json.loads(second.stdout)
            self.assertTrue(second_result["cache_reused"])
            self.assertEqual(calls.read_text(encoding="utf-8"), "called\n")

            cached_iso.write_bytes(b"corrupt cache")
            output.unlink()
            environment.pop("FAKE_CURL_FAIL")
            third = subprocess.run(
                command,
                env=environment,
                text=True,
                capture_output=True,
                check=False,
            )
            self.assertEqual(third.returncode, 0, third.stderr)
            third_result = json.loads(third.stdout)
            self.assertFalse(third_result["cache_reused"])
            self.assertEqual(cached_iso.read_bytes(), source.read_bytes())
            self.assertEqual(calls.read_text(encoding="utf-8"), "called\ncalled\n")

            output.unlink()
            builder_script.write_text("#!/bin/sh\nexit 9\n", encoding="utf-8")
            builder_script.chmod(0o755)
            manual_command = list(command)
            manual_command[manual_command.index("ubuntu-autoinstall")] = "manual"
            manual = subprocess.run(
                manual_command,
                env=environment,
                text=True,
                capture_output=True,
                check=False,
            )
            self.assertEqual(manual.returncode, 0, manual.stderr)
            manual_result = json.loads(manual.stdout)
            self.assertEqual(manual_result["install_mode"], "manual")
            self.assertTrue(manual_result["cache_reused"])
            self.assertEqual(output.read_bytes(), source.read_bytes())

    def test_rejects_non_https_source_url(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            request = root / "request.json"
            output = root / "output.iso"
            builder_script = root / "builder"
            request.write_text("{}\n", encoding="utf-8")
            builder_script.write_text("#!/bin/sh\nexit 0\n", encoding="utf-8")
            builder_script.chmod(0o755)
            environment = os.environ.copy()
            environment.update(
                {
                    "APP_HA_PREPARE_ISO_TEST_MODE": "1",
                    "APP_HA_PREPARE_ISO_CACHE_ROOT": str(root / "cache"),
                    "APP_HA_PREPARE_ISO_BUILDER": str(builder_script),
                }
            )
            completed = subprocess.run(
                [
                    str(PREPARER_PATH),
                    "--source-url",
                    "http://example.test/source.iso",
                    "--expected-sha256",
                    "0" * 64,
                    "--install-mode",
                    "ubuntu-autoinstall",
                    "--request",
                    str(request),
                    "--output-iso",
                    str(output),
                ],
                env=environment,
                text=True,
                capture_output=True,
                check=False,
            )
            self.assertNotEqual(completed.returncode, 0)
            self.assertIn("source URL is invalid", completed.stderr)


class ProductionVmShellTest(unittest.TestCase):
    def run_sourced(
        self, body: str, *, expected: int = 0
    ) -> subprocess.CompletedProcess[str]:
        completed = subprocess.run(
            [
                "bash",
                "-c",
                'PRODUCTION_VM_SOURCE_ONLY=1 source "$1"; shift; ' + body,
                "bash",
                str(SCRIPT),
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

    def test_openssl_without_sha512_crypt_stops_before_config_or_ssh(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            touched = Path(temporary) / "touched"
            completed = self.run_sourced(
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
            "openssl() { printf '%s\\n' '$6$appha0probe$synthetic'; }\n"
            "require_openssl_sha512_crypt && echo accepted\n"
        )
        self.assertIn("accepted", accepted.stdout)

    def test_shell_syntax(self) -> None:
        completed = subprocess.run(
            ["bash", "-n", str(SCRIPT)],
            text=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            check=False,
        )
        self.assertEqual(completed.returncode, 0, completed.stderr)

    def test_live_nodes_must_be_contiguous_and_placement_needs_two(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            nodes = Path(temporary) / "nodes.json"
            nodes.write_text(
                json.dumps(
                    [
                        {"node": "mox1", "status": "online"},
                        {"node": "mox2", "status": "online"},
                        {"node": "mox3", "status": "offline"},
                    ]
                ),
                encoding="utf-8",
            )
            completed = subprocess.run(
                [
                    "bash",
                    "-c",
                    'PRODUCTION_VM_SOURCE_ONLY=1 source "$1"; '
                    'mapfile -t ONLINE_NODES < <(parse_online_nodes_json "$2" 3); '
                    'validate_placement_csv "mox2,mox1"',
                    "bash",
                    str(SCRIPT),
                    str(nodes),
                ],
                text=True,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                check=False,
            )
            self.assertEqual(completed.returncode, 0, completed.stderr)
            self.assertEqual(completed.stdout.strip(), "mox2,mox1")

            nodes.write_text(
                json.dumps(
                    [
                        {"node": "mox1", "status": "online"},
                        {"node": "mox2", "status": "offline"},
                        {"node": "mox3", "status": "online"},
                    ]
                ),
                encoding="utf-8",
            )
            invalid = subprocess.run(
                [
                    "bash",
                    "-c",
                    'PRODUCTION_VM_SOURCE_ONLY=1 source "$1"; '
                    'parse_online_nodes_json "$2" 3',
                    "bash",
                    str(SCRIPT),
                    str(nodes),
                ],
                text=True,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                check=False,
            )
            self.assertNotEqual(invalid.returncode, 0)
            self.assertIn("not contiguous", invalid.stderr)

        too_few = self.run_sourced(
            'ONLINE_NODES=(mox1 mox2); validate_placement_csv "mox1"',
            expected=1,
        )
        self.assertIn("at least two", too_few.stderr)

    def test_production_capacity_is_enforced_on_every_placement_node(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            resources = [
                {
                    "kind": "production",
                    "name": "prod1",
                    "placement": ["mox1", "mox2"],
                },
                {
                    "kind": "staging",
                    "name": "stage1prod1",
                    "placement": ["mox2"],
                },
            ]
            (root / "resources.json").write_text(
                json.dumps(resources), encoding="utf-8"
            )
            config = root / "config.sh"
            config.write_text(
                """
load_proxmox_config() {
  case "$2" in
    mox1) MAX_PROD_VM_COUNT_ON_THIS_HOST=2 ;;
    mox2) MAX_PROD_VM_COUNT_ON_THIS_HOST=1 ;;
    *) return 1 ;;
  esac
}
""",
                encoding="utf-8",
            )
            body = f"""
RUN_DIR={str(root)!r}
CONFIG_LIB={str(config)!r}
PLACEMENT_NODES=(mox1 mox2)
validate_prod_host_capacity
"""
            full = self.run_sourced(body, expected=1)
            self.assertIn("mox1: 1/2 used", full.stdout)
            self.assertIn(
                "mox2 already has 1 registered production guests (limit 1)",
                full.stderr,
            )

    def test_request_writer_persists_hash_but_no_plaintext(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            completed = self.run_sourced(
                f"""
RUN_DIR={temporary!r}
RESOURCE_NAME=prod1
PROD_PRIVATE_IP=10.213.0.31
GUEST_GATEWAY=10.213.0.10
PRODUCTION_IP_START=10.213.0.31
PRODUCTION_IP_END=10.213.0.50
VM_MAC=02:11:22:33:44:55
ROOT_PASSWORD_HASH='$6$testsalt$hashonly'
ADMIN_1_PUBLIC_SSH_KEY='ssh-ed25519 AAAAone admin1'
ADMIN_2_PUBLIC_SSH_KEY='ssh-ed25519 AAAAtwo admin2'
DNS_SERVERS=(1.1.1.1 1.0.0.1)
request="$(write_iso_request)"
cat "$request"
"""
            )
            request = json.loads(completed.stdout)
            self.assertEqual(request["root_password_hash"], "$6$testsalt$hashonly")
            self.assertNotIn("plaintext", completed.stdout)
            self.assertEqual(request["nameservers"], ["1.1.1.1", "1.0.0.1"])

    def test_workstation_jump_ssh_setup_writes_strict_managed_alias(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            home = root / "home"
            run_dir = root / "run"
            home.mkdir()
            run_dir.mkdir()
            host_key = (
                "ssh-ed25519 "
                "AAAAC3NzaC1lZDI1NTE5AAAAIBhE1+iRU5AP8b2hmfLfAHJQ/BVRAR0n4ddx0DFCnkg4"
            )
            completed = self.run_sourced(
                f"""
HOME={str(home)!r}
RUN_DIR={str(run_dir)!r}
RESOURCE_NAME=prod1
PROD_PRIVATE_IP=10.213.0.31
COORDINATOR=mox1
OWNER_NODE=mox1
VMID=100
prompt_yes() {{ return 0; }}
prompt_with_default() {{ printf -v "$1" '%s' prod1; }}
node_exec() {{
  printf '%s\\n' '{{"exited":1,"exitcode":0,"out-data":"{host_key} root@prod1\\\\n"}}'
}}
ssh() {{
  if [[ "${{1:-}}" == -G ]]; then
    printf '%s\\n' 'hostname prod1' 'user nate' 'proxyjump none'
    return 0
  fi
  return 0
}}
setup_workstation_jump_ssh
"""
            )
            self.assertEqual(completed.returncode, 0, completed.stderr)
            known_hosts = (home / ".ssh" / "known_hosts").read_text(
                encoding="utf-8"
            )
            config = (home / ".ssh" / "config").read_text(encoding="utf-8")
            self.assertEqual(known_hosts, f"prod1 {host_key}\n")
            self.assertIn("Host prod1\n", config)
            self.assertIn("    HostName 10.213.0.31\n", config)
            self.assertIn("    ProxyJump mox1\n", config)
            self.assertIn("    HostKeyAlias prod1\n", config)
            self.assertIn("    StrictHostKeyChecking yes\n", config)

    def test_node_exec_preserves_argv_without_evaluation(self) -> None:
        completed = self.run_sourced(
            """
mox_ssh() { shift; "$@"; }
COORDINATOR=mox1
node_exec mox1 python3 -c \
  'import json, sys; print(json.dumps(sys.argv[1:]))' \
  'space ; $(must-not-run)' 'plain'
"""
        )
        self.assertEqual(
            json.loads(completed.stdout),
            ["space ; $(must-not-run)", "plain"],
        )

    def test_created_vm_identity_retries_transient_cluster_rows(self) -> None:
        completed = self.run_sourced(
            """
calls=0
locate_vm() {
  ((calls += 1))
  ((calls >= 3)) && return 0
  return 4
}
sleep() { :; }
wait_for_created_vm_identity
printf '%s\\n' "$calls"
"""
        )
        self.assertEqual(completed.stdout.strip(), "3")

    def test_preserved_unstarted_vm_completes_disk_allocation(self) -> None:
        completed = self.run_sourced(
            """
apply_disk_allocation() { printf 'apply\\n'; }
validate_disk_allocation() { printf 'validate\\n'; }
RESOURCE_STATE=reserved
INSTALL_PHASE=unstarted
REGISTERED_OWNER_NODE=
REGISTERED_ROOT_VOLUME=
REGISTRY_REPLICATION_CSV=
REGISTRY_HA_CSV=
ROUTES_ENABLED=0
validate_or_complete_existing_disk_allocation
RESOURCE_STATE=provisioning
validate_or_complete_existing_disk_allocation
"""
        )
        self.assertEqual(
            completed.stdout.splitlines(),
            [
                "    Completing disk allocation policy for the preserved unstarted VM",
                "apply",
                "validate",
            ],
        )

    def test_missing_non_reserved_vm_is_never_recreated(self) -> None:
        completed = self.run_sourced(
            """
renew_orchestration_lease() { :; }
locate_vm() { OWNER_NODE=""; VM_LIVE_STATUS=absent; return 1; }
build_and_upload_iso() { printf 'UNSAFE CREATE\\n'; }
RESOURCE_NAME=prod1
VMID=100
RESOURCE_STATE=ready
INSTALL_PHASE=guest-verified
install_or_resume_os
""",
            expected=1,
        )
        self.assertIn("ready VM 100 is missing", completed.stderr)
        self.assertNotIn("UNSAFE CREATE", completed.stdout)

    def test_resumed_vm_requires_exact_disk_and_secure_boot_contract(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            config = Path(temporary) / "qm.conf"
            config.write_text(
                "\n".join(
                    (
                        "name: prod1",
                        "cores: 4",
                        "memory: 8192",
                        "cpu: x86-64-v2-AES",
                        "bios: ovmf",
                        "machine: q35",
                        "efidisk0: local-zfs:vm-100-disk-0,efitype=4m,ms-cert=2023k,pre-enrolled-keys=1,size=1M",
                        "sockets: 1",
                        "balloon: 0",
                        "ostype: l26",
                        "scsihw: virtio-scsi-single",
                        "hookscript: local:snippets/app-ha-guest-role-hook.sh",
                        "onboot: 0",
                        "agent: enabled=1,fstrim_cloned_disks=1",
                        "net0: virtio=02:11:22:33:44:55,bridge=vmbr-private,firewall=1",
                        "scsi0: local-zfs:vm-100-disk-1,discard=on,iothread=1,replicate=1,size=128G,ssd=1",
                        "boot: order=scsi0",
                        "tags: app-ha-production;purpose-myapp",
                        "",
                    )
                ),
                encoding="utf-8",
            )
            body = f"""
RUN_DIR={temporary!r}
OWNER_NODE=mox1
VMID=100
RESOURCE_NAME=prod1
VM_CORES=4
VM_MEMORY_MB=8192
PROD_VM_CPU_TYPE=x86-64-v2-AES
PROD_VM_STORAGE=local-zfs
PROD_VM_BRIDGE=vmbr-private
VM_MAC=02:11:22:33:44:55
HOOK_REF=local:snippets/app-ha-guest-role-hook.sh
PRODUCTION_VM_TAG=app-ha-production
PURPOSE_TAG=purpose-myapp
ISO_STORAGE_ID=local
ISO_FILENAME=prod1.iso
ROOT_VOLUME=local-zfs:vm-100-disk-1
VM_DISK_GIB=128
node_exec() {{ cat {str(config)!r}; }}
verify_vm_config
printf '%s\\n' "$ROOT_VOLUME"
"""
            valid = self.run_sourced(body)
            self.assertEqual(
                valid.stdout.strip(), "local-zfs:vm-100-disk-1"
            )

            config.write_text(
                config.read_text(encoding="utf-8").replace(
                    "bridge=vmbr-private,firewall=1",
                    "bridge=vmbr-private,firewall=1,link_down=1",
                ),
                encoding="utf-8",
            )
            disabled_body = body.replace(
                "node_exec()",
                "INSTALL_PHASE=network-finalized\n"
                "FINAL_NETWORK_ENABLED=false\n"
                "node_exec()",
            )
            self.run_sourced(disabled_body)

            config.write_text(
                config.read_text(encoding="utf-8").replace(",link_down=1", ""),
                encoding="utf-8",
            )
            invalid_network = self.run_sourced(disabled_body, expected=1)
            self.assertIn("does not match", invalid_network.stderr)

            config.write_text(
                config.read_text(encoding="utf-8").replace(
                    ",ms-cert=2023k", ""
                ),
                encoding="utf-8",
            )
            invalid = self.run_sourced(body, expected=1)
            self.assertIn("does not match", invalid.stderr)

    def test_quorum_votes_and_qdevice_follow_even_odd_membership(self) -> None:
        def render_status(
            node_count: int,
            *,
            qdevice: bool,
            unhealthy_node: int | None = None,
            expected_votes: int | None = None,
        ) -> str:
            votes = expected_votes
            if votes is None:
                votes = node_count + (1 if qdevice else 0)
            flags = "Quorate Qdevice" if qdevice else "Quorate"
            rows = []
            for index in range(1, node_count + 1):
                voter_flags = "NA,NV" if index == unhealthy_node else "A,V,NMW"
                local = " (local)" if index == 1 else ""
                rows.append(
                    f"0x{index:08x}          1    {voter_flags} "
                    f"mox{index}{local}"
                )
            if qdevice:
                rows.append("0x00000000          1            Qdevice")
            return "\n".join(
                (
                    "Name:             production-test",
                    f"Nodes:            {node_count}",
                    "Quorate:          Yes",
                    f"Expected votes:   {votes}",
                    f"Total votes:      {votes}",
                    f"Flags:            {flags}",
                    "    Nodeid      Votes    Qdevice Name",
                    *rows,
                    "",
                )
            )

        with tempfile.TemporaryDirectory() as temporary:
            status = Path(temporary) / "pvecm-status.txt"
            for node_count in range(2, 11):
                status.write_text(
                    render_status(
                        node_count,
                        qdevice=node_count % 2 == 0,
                    ),
                    encoding="utf-8",
                )
                self.run_sourced(
                    f"validate_pvecm_status_file {str(status)!r} "
                    f"production-test {node_count}"
                )

            invalid_cases = (
                (render_status(4, qdevice=False), 4, "QDevice"),
                (render_status(3, qdevice=True), 3, "no QDevice"),
                (
                    render_status(4, qdevice=True, unhealthy_node=3),
                    4,
                    "alive and voting",
                ),
                (
                    render_status(4, qdevice=True, expected_votes=4),
                    4,
                    "QDevice",
                ),
                (render_status(11, qdevice=False), 11, "inconsistent"),
            )
            for contents, node_count, message in invalid_cases:
                status.write_text(contents, encoding="utf-8")
                invalid = self.run_sourced(
                    f"validate_pvecm_status_file {str(status)!r} "
                    f"production-test {node_count}",
                    expected=1,
                )
                self.assertIn(message, invalid.stderr)

    def test_dry_run_uses_live_reads_without_remote_mutation(self) -> None:
        artifacts = PROD_DIR / "artifacts"
        artifacts_preexisted = artifacts.exists()
        runs_before = set(artifacts.glob("run-*")) if artifacts.exists() else set()
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            env_dir = root / "env"
            bin_dir = root / "bin"
            env_dir.mkdir()
            bin_dir.mkdir()
            cluster = env_dir / "cluster.conf"
            secrets = env_dir / "secrets.env"
            calls = root / "ssh-calls.jsonl"

            cluster.write_text(
                "\n".join(
                    (
                        "PROXMOX_CLUSTER_NAME=app-ha-test",
                        "MAX_MOX_HOSTS=2",
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
                        "PROD_GUEST_OS_ISO_URL=https://example.test/ubuntu-live-server-amd64.iso",
                        f"PROD_GUEST_OS_ISO_SHA256={'0' * 64}",
                        "ADMIN_1_PUBLIC_SSH_KEY=ssh-ed25519 AAAAone admin1",
                        "ADMIN_2_PUBLIC_SSH_KEY=ssh-ed25519 AAAAtwo admin2",
                        "PRODUCTION_VM_TAG=app-ha-production",
                        "PURPOSE_TAG_PREFIX=purpose-",
                        "GUEST_ROLE_HOOK_PATH=hosts/app-ha-guest-role-hook.sh",
                        "CLUSTER_STATE_DIR=/etc/pve/priv/app-ha",
                        "PROD_VM_CORES=4",
                        "PROD_VM_MEMORY_GIB=8",
                        "PROD_VM_DISK_GB=128",
                        "PROD_VM_STORAGE=local-zfs",
                        "PROD_VM_BRIDGE=vmbr-private",
                        "PROD_VM_CPU_TYPE=x86-64-v2-AES",
                        "PROD_VM_REPLICATION_INTERVAL=*/15",
                        "ISO_STORAGE_ID=local",
                        "SNIPPETS_STORAGE_ID=local",
                        "GUEST_DNS_SERVERS=1.1.1.1,1.0.0.1",
                        "",
                    )
                ),
                encoding="utf-8",
            )
            cluster_text = cluster.read_text(encoding="utf-8")
            self.assertIn("PROD_VM_MEMORY_GIB=8", cluster_text)
            self.assertNotIn("PROD_VM_MEMORY_MB=", cluster_text)
            secrets.write_text(
                "PROD_GUEST_VM_ROOT_PASSWORD=synthetic-test-value\n",
                encoding="utf-8",
            )
            for index in (1, 2):
                (env_dir / f"mox{index}.conf").write_text(
                    "\n".join(
                        (
                            f"NVME_MIRROR_1_SERIAL_1=test-mox{index}-a",
                            f"NVME_MIRROR_1_SERIAL_2=test-mox{index}-b",
                            f"PROXMOX_IP=10.213.0.{10 + index}",
                            "PROXMOX_GATEWAY=10.213.0.1",
                            "PROXMOX_PREFIX=24",
                            f"PROXMOX_PUBLIC_MAC=02:00:00:00:00:{index:02x}",
                            f"PROXMOX_SECONDARY_MAC=02:00:00:00:01:{index:02x}",
                            "MAX_PROD_VM_COUNT_ON_THIS_HOST=1",
                            "MAX_STAGING_VM_COUNT_ON_THIS_HOST=5",
                            "",
                        )
                    ),
                    encoding="utf-8",
                )
            cluster.chmod(0o644)
            secrets.chmod(0o600)

            fake_ssh = bin_dir / "ssh"
            fake_ssh.write_text(
                """#!/usr/bin/env python3
import json
import os
import shlex
import sys

args = sys.argv[1:]
payload = sys.stdin.read()
command = args
for value in reversed(args):
    try:
        joined = shlex.split(value)
    except ValueError:
        continue
    if len(joined) > 1 and (
        "pvesh" in joined
        or any(part.endswith("/cluster_registry.py") for part in joined)
    ):
        command = joined
        break
with open(os.environ["FAKE_SSH_CALLS"], "a", encoding="utf-8") as stream:
    stream.write(json.dumps({
        "args": args,
        "command": command,
        "stdin": payload,
        "secret_exported": "PROD_GUEST_VM_ROOT_PASSWORD" in os.environ,
    }) + "\\n")

if "pvesh" in command:
    index = command.index("pvesh")
    remote = command[index:]
    path = remote[2] if len(remote) > 2 and remote[1] == "get" else ""
    if path == "/nodes":
        print(json.dumps([
            {"node": "mox1", "status": "online"},
            {"node": "mox2", "status": "online"},
        ]))
    elif path == "/cluster/status":
        print(json.dumps([
            {"type": "cluster", "name": "app-ha-test", "quorate": 1},
        ]))
    elif path == "/cluster/nextid":
        print("100")
    elif path == "/cluster/resources":
        print("[]")
    else:
        raise SystemExit(f"unexpected pvesh path: {path}")
elif any(value.endswith("/cluster_registry.py") for value in command):
    if command[-1] != "list":
        raise SystemExit("dry run attempted a registry mutation")
    print("[]")
elif payload or command[-1:] == ["true"]:
    pass
else:
    raise SystemExit(f"unexpected ssh command: {command}")
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
                [str(SCRIPT), "--dry-run"],
                input="\n\nexample.com\n" + "\n" * 10,
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
            self.assertIn("Dry run complete", completed.stdout)
            self.assertIn("not reserved", completed.stdout)
            self.assertNotIn(
                "synthetic-test-value", completed.stdout + completed.stderr
            )
            calls_text = calls.read_text(encoding="utf-8")
            self.assertNotIn('"secret_exported": true', calls_text)
            for mutation in (
                "allocate-prod",
                "qm create",
                "pvesr create-local-job",
                "ha-manager add",
            ):
                self.assertNotIn(mutation, calls_text)

        runs_after = set(artifacts.glob("run-*")) if artifacts.exists() else set()
        self.assertEqual(runs_after - runs_before, set(), "dry run leaked artifacts")
        if not artifacts_preexisted and artifacts.exists():
            artifacts.rmdir()

    def test_live_mutations_are_guarded_and_no_destructive_rollback_exists(self) -> None:
        text = SCRIPT.read_text(encoding="utf-8")
        required = (
            "allocate-prod",
            "orchestration-acquire",
            "orchestration-set-phase",
            "orchestration-release",
            "qm create",
            "pvesr create-local-job",
            "pvesr schedule-now",
            "ha-manager rules add node-affinity",
            "ha-manager add",
            "--strict 1",
            "--failback 0",
            "--auto-rebalance 0",
            "--routes-enabled",
            "ms-cert=2023k",
            'row["last_sync"] > int(sys.argv[4])',
            "Qdevice",
            "findmnt",
            '"WIPE"',
            "hashlib.sha256(rows[0]",
            "app-ha-prod-root-v1:",
            'openssl passwd -6 -salt "$password_salt" -stdin',
            "REMOTE_HAPROXY_SYNC",
            "--lock-timeout 240",
            "Waiting up to 240 seconds for any current HAProxy route sync",
            "REMOTE_ISO_PREPARER",
            "SOURCE_ISO_CACHE_PATH",
            "PROD_GUEST_OS_ISO_URL",
            "PROD_GUEST_OS_INSTALL_MODE",
            "copy_request_to_node",
            "Manual guest installation contract",
            "Manual install mode cannot attest the exact root console password hash",
            "setup_workstation_jump_ssh",
            "Set up permanent jump SSH from this workstation",
            "QGA-attested guest Ed25519 fingerprint",
            "StrictHostKeyChecking yes",
            "restoring the prior workstation files",
            "--dry-run",
        )
        for value in required:
            self.assertIn(value, text)
        self.assertNotIn('df -PB1 --output=avail "$directory"', text)
        forbidden = (
            "qm destroy",
            "ha-manager remove",
            "registry_cmd release",
            "pvesr delete",
            "-o StrictHostKeyChecking=no",
            "PROD_GUEST_UBUNTU_ISO_FILE_FULL_PATH",
            "PROD_GUEST_UBUNTU_ISO_FILE_URL",
            "stream_iso_to_node",
            "require_command_local xorriso",
            "--affinity positive",
        )
        for value in forbidden:
            self.assertNotIn(value, text)
        self.assertNotIn("--routes-disabled", text)
        self.assertIn('affinity_nodes+=("${node}:1")', text)
        self.assertNotIn('affinity_nodes+=("$node")', text)
        self.assertLess(
            text.index('if [[ "$DRY_RUN" == true ]]'),
            text.index('allocation_result="$(registry_cmd'),
        )

    def test_lifecycle_hook_is_attached_only_after_installer_boots(self) -> None:
        text = SCRIPT.read_text(encoding="utf-8")
        create_body = text.split("create_vm() {", 1)[1].split(
            "verify_vm_config() {", 1
        )[0]
        finalize_body = text.split("apply_final_network_policy() {", 1)[1].split(
            "ensure_replicating_state() {", 1
        )[0]
        self.assertNotIn("--hookscript", create_body)
        self.assertIn('--hookscript "$HOOK_REF"', finalize_body)


if __name__ == "__main__":
    unittest.main()
