# Copyright (c) 2026 BEENTHERE VENTURES, INC.
# SPDX-License-Identifier: GPL-3.0-only

"""The interim bootstrap's generated files: v2 config files that v3's config.sh
loads, the hosts file, TOTP codes, the registry arguments, and the payloads."""

from __future__ import annotations

import io
import json
import subprocess
import sys
import tarfile
import tempfile
import unittest
from pathlib import Path

LAB = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(LAB))

from bmaclab import bootstrap  # noqa: E402
from bmaclab.topology import load  # noqa: E402

CONFIG_SH = bootstrap.V3 / "legacy/scripts/lib/config.sh"


def load_config(folder: Path, host: str, keys: list[str]) -> dict[str, str]:
    script = """
        source "$1" || exit 2
        load_proxmox_config --host "$2" || exit 3
        shift 2
        for key in "$@"; do printf '%s=%s\\n' "$key" "${!key-}"; done
    """
    result = subprocess.run(
        ["bash", "-c", script, "bash", str(CONFIG_SH), host, *keys],
        capture_output=True, text=True, stdin=subprocess.DEVNULL,
        env={"PATH": "/usr/bin:/bin", "HOME": str(folder), "BMAC_CONFIG_TEST_MODE": "1", "BMAC_CONFIG_DIR": str(folder)},
    )
    if result.returncode != 0:
        raise AssertionError(result.stderr)
    return dict(line.split("=", 1) for line in result.stdout.splitlines())


class ConfFilesTest(unittest.TestCase):
    def setUp(self) -> None:
        self.topology = load(LAB / "lab.toml")
        self.files = bootstrap.conf_files(self.topology)

    def test_config_sh_loads_every_host(self) -> None:
        self.assertEqual(sorted(self.files), ["cluster.conf", "mox1.conf", "mox2.conf", "mox3.conf"])
        with tempfile.TemporaryDirectory() as scratch:
            folder = Path(scratch)
            folder.chmod(0o700)
            for name, text in self.files.items():
                (folder / name).write_text(text)
                (folder / name).chmod(0o600)
            for vm in self.topology.hosts:
                loaded = load_config(folder, vm.name, ["PROXMOX_CLUSTER_NAME", "PROXMOX_SECONDARY_IP", "PROXMOX_SECONDARY_MAC", "NVME_MIRROR_0_SERIAL_2"])
                nvme = [d for d in vm.disks if d.bus == "nvme"]
                self.assertEqual(loaded, {
                    "PROXMOX_CLUSTER_NAME": "bmaclab",
                    "PROXMOX_SECONDARY_IP": f"{vm.vlan_ip}/24",
                    "PROXMOX_SECONDARY_MAC": vm.nic("vlan").mac.upper(),
                    "NVME_MIRROR_0_SERIAL_2": nvme[1].serial,
                })

    def test_no_bare_tailnet_name(self) -> None:
        for text in self.files.values():
            self.assertNotIn("TAILSCALE_HOSTNAME", text)

    def test_vlan_addresses_must_follow_the_slot(self) -> None:
        source = (LAB / "lab.toml").read_text().replace('vlan_ip = "10.213.0.12"', 'vlan_ip = "10.213.0.42"')
        with tempfile.TemporaryDirectory() as scratch:
            path = Path(scratch, "lab.toml")
            path.write_text(source)
            with self.assertRaisesRegex(bootstrap.BootstrapError, "mox2's VLAN address must be 10.213.0.12"):
                bootstrap.conf_files(load(path))


class FilesTest(unittest.TestCase):
    def setUp(self) -> None:
        self.topology = load(LAB / "lab.toml")

    def test_hosts_resolve_to_the_private_vlan(self) -> None:
        text = bootstrap.hosts_file(self.topology)
        self.assertIn("10.213.0.11 mox1.lab.internal mox1\n", text)
        self.assertIn("10.213.0.13 mox3.lab.internal mox3\n", text)
        self.assertNotIn("10.0.2.15", text)

    def test_the_qdevice_setting_is_its_tailnet_name(self) -> None:
        self.assertIn("PROXMOX_QDEVICE_HOST='bmaclab-qdevice'\n", bootstrap.conf_files(self.topology)["cluster.conf"])

    def test_tailscale_standin_answers_from_the_management_network(self) -> None:
        mox1 = self.topology.vm("mox1")
        with tempfile.TemporaryDirectory() as scratch:
            command = Path(scratch, "tailscale")
            command.write_text(bootstrap.tailscale_standin(self.topology, mox1))

            def tailscale(*args: str) -> subprocess.CompletedProcess:
                return subprocess.run([sys.executable, str(command), *args], capture_output=True, text=True)

            self.assertEqual(tailscale("ip", "-4").stdout, "100.64.0.11\n")
            status = json.loads(tailscale("status", "--json").stdout)
            self.assertEqual(status["BackendState"], "Running")
            self.assertEqual(status["Self"]["HostName"], "bmaclab-mox1")
            peers = {p["HostName"]: p for p in status["Peer"].values()}
            self.assertNotIn("bmaclab-mox1", peers)
            self.assertEqual(peers["bmaclab-qdevice"]["TailscaleIPs"], ["100.64.0.101"])
            self.assertTrue(peers["bmaclab-qdevice"]["DNSName"].startswith("bmaclab-qdevice."))
            self.assertNotEqual(tailscale("up").returncode, 0)

    def test_tailnet_names_replace_their_block_in_the_hosts_file(self) -> None:
        block = bootstrap.tailnet_hosts_block(self.topology)
        self.assertIn("100.64.0.101 bmaclab-qdevice.bmaclab.tailnet.invalid bmaclab-qdevice\n", block)
        self.assertNotIn(" qdevice\n", block)
        with tempfile.TemporaryDirectory() as scratch:
            hosts = Path(scratch, "hosts")
            hosts.write_text(f"127.0.0.1 localhost\n{bootstrap.TAILNET_BEGIN}\n100.64.0.9 stale\n{bootstrap.TAILNET_END}\n::1 ip6-localhost\n")
            script = bootstrap.TAILNET_SCRIPT.replace("/etc/hosts", str(hosts))
            for _ in range(2):
                subprocess.run(["bash", "-c", script], input=block, text=True, check=True)
            text = hosts.read_text()
            self.assertNotIn("stale", text)
            self.assertEqual(text.count(bootstrap.TAILNET_BEGIN), 1)
            self.assertTrue(text.startswith("127.0.0.1 localhost\n::1 ip6-localhost\n"))
            self.assertIn(block, text)

    def test_totp_matches_rfc_6238(self) -> None:
        secret = "GEZDGNBVGY3TQOJQGEZDGNBVGY3TQOJQ"  # "12345678901234567890"
        self.assertEqual(bootstrap.totp(secret, at=59, digits=8), "94287082")
        self.assertEqual(bootstrap.totp(secret, at=1111111109, digits=8), "07081804")
        self.assertEqual(bootstrap.totp(secret, at=59), "287082")

    def test_registry_init_uses_the_settings(self) -> None:
        args = bootstrap.registry_init_args(self.topology)
        pairs = dict(zip(args[1::2], args[2::2]))
        self.assertEqual(args[0], "init")
        self.assertEqual(pairs["--network"], "10.213.0.0/24")
        self.assertEqual(pairs["--guest-gateway"], "10.213.0.1/24")
        self.assertEqual(pairs["--max-hosts"], "3")

    def test_install_script_parses_and_names_the_host(self) -> None:
        script = bootstrap.install_script(self.topology, self.topology.vm("mox2"))
        self.assertIn("subjectAltName=DNS:mox2.lab.internal,DNS:mox2,IP:100.64.0.12,IP:10.213.0.12", script)
        self.assertIn("/var/lib/vz/snippets/bmac-guest-role-hook.sh", script)
        self.assertIn("systemctl enable --now bmac-deferred-cleanup.timer", script)
        self.assertLess(script.index("bash -n"), script.index("install -D"))
        subprocess.run(["bash", "-n"], input=script, text=True, check=True)

    def test_replication_address_follows_the_slot(self) -> None:
        vm = self.topology.vm("mox3")
        script = bootstrap.replication_script(self.topology, vm)
        self.assertIn(f"awk -v mac={vm.nic('vlan').mac} ", script)
        self.assertIn("ip address replace 10.213.0.243/24 dev", script)
        subprocess.run(["bash", "-n"], input=script, text=True, check=True)
        self.assertIn(f"ExecStart={bootstrap.REPLICATION_SCRIPT}\n", bootstrap.replication_unit())

    def test_payload_holds_the_runtime(self) -> None:
        payload = bootstrap.payload_tar({"bmacd": b"\x7fELF1", "bmac": b"\x7fELF2"})
        with tarfile.open(fileobj=io.BytesIO(payload)) as tar:
            names = set(tar.getnames())
            self.assertEqual(tar.getmember("bin/bmacd").mode, 0o755)
        for expected in ("bin/bmacd", "bin/bmac", "systemd/bmacd.service", "systemd/bmacd-web.service",
                         "systemd/bmac-deferred-cleanup.timer", "systemd/bmac-haproxy-route-sync.timer",
                         "systemd/bmac-haproxy-ingress.service.d/route-sync.conf",
                         "sysusers/bmacd.conf", "bmacd.depends", "legacy/scripts/lib/ui_protocol.sh",
                         "legacy/scripts/host_runtime/bmac-guest-role-hook.sh",
                         "legacy/scripts/utilities/ui_json_run.sh", "legacy/mock/interactive.sh"):
            self.assertIn(expected, names)

    def test_source_leaves_out_build_output(self) -> None:
        with tempfile.TemporaryDirectory() as scratch:
            root = Path(scratch)
            for name in ("Cargo.toml", "Cargo.lock", "crates/a/src/lib.rs", "crates/a/target/x", "legacy/node_modules/y", "lab/lab.toml"):
                (root / name).parent.mkdir(parents=True, exist_ok=True)
                (root / name).write_text("x")
            with tarfile.open(fileobj=io.BytesIO(bootstrap.source_tar(root))) as tar:
                names = {m.name for m in tar.getmembers() if m.isfile()}
        self.assertEqual(names, {"Cargo.toml", "Cargo.lock", "crates/a/src/lib.rs"})

    def test_vendored_crates_are_kept_whole_and_used(self) -> None:
        with tempfile.TemporaryDirectory() as scratch:
            root, vendor = Path(scratch) / "src", Path(scratch) / "vendor"
            for path in (root / "Cargo.toml", vendor / "a/src/target/mod.rs", vendor / "a/.cargo-checksum.json"):
                path.parent.mkdir(parents=True, exist_ok=True)
                path.write_text("x")
            with tarfile.open(fileobj=io.BytesIO(bootstrap.source_tar(root, vendor))) as tar:
                names = {m.name for m in tar.getmembers() if m.isfile()}
                config = tar.extractfile(".cargo/config.toml").read().decode()
        self.assertEqual(names, {"Cargo.toml", "vendor/a/src/target/mod.rs", "vendor/a/.cargo-checksum.json", ".cargo/config.toml"})
        self.assertIn('replace-with = "vendored-sources"', config)
        self.assertIn('directory = "vendor"', config)


if __name__ == "__main__":
    unittest.main()
