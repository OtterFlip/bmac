#!/usr/bin/env python3

# Copyright (c) 2026 BEENTHERE VENTURES, INC.
# SPDX-License-Identifier: GPL-3.0-only

"""Tests for the host-side shared guest gateway, egress rules, and tunnel tools."""

from __future__ import annotations

import os
from pathlib import Path
import subprocess
import tempfile
import unittest


SCRIPTS_DIR = Path(__file__).resolve().parent.parent
RUNTIME = SCRIPTS_DIR / "host_runtime"
GUEST_NETWORK = RUNTIME / "guest_network.sh"
SETTINGS_LIB = SCRIPTS_DIR / "lib" / "guest_network_settings.sh"
SHELL_SCRIPTS = (
    GUEST_NETWORK,
    RUNTIME / "tunnel_sync.sh",
    RUNTIME / "cloudflared_setup.sh",
    SETTINGS_LIB,
    SCRIPTS_DIR / "user_callable" / "hosts" / "sync_cluster_network.sh",
)

SETTINGS = """\
BMAC_HOST=mox2
BMAC_MOX_INDEX=2
BMAC_PRIVATE_BRIDGE=vmbr-private
BMAC_PRIVATE_NIC=enp65s0f1
BMAC_PRIVATE_IP=10.213.0.12
BMAC_PUBLIC_IF=vmbr0
BMAC_PUBLIC_NIC=enp65s0f0
BMAC_PUBLIC_IP=192.0.2.20
BMAC_PUBLIC_GATEWAY=192.0.2.1
BMAC_GUEST_GATEWAY_IP=10.213.0.10
BMAC_GUEST_GATEWAY_MAC=02:00:0A:D5:00:0A
BMAC_SLOT_FIRST=10.213.0.110
BMAC_SLOT_LAST=10.213.0.209
BMAC_MOX_FIRST=10.213.0.11
BMAC_MAX_MOX_HOSTS=3
BMAC_TRANSFER_LIMIT_GIB=
BMAC_TRANSFER_RESET_DAY=1
"""


class GuestNetworkTest(unittest.TestCase):
    def setUp(self) -> None:
        self.temporary = tempfile.TemporaryDirectory()
        self.settings = Path(self.temporary.name) / "guest-network.env"
        self.settings.write_text(SETTINGS, encoding="utf-8")

    def tearDown(self) -> None:
        self.temporary.cleanup()

    def render(self, settings: str | None = None, expected: int = 0) -> subprocess.CompletedProcess[str]:
        if settings is not None:
            self.settings.write_text(settings, encoding="utf-8")
        completed = subprocess.run(
            ["bash", str(GUEST_NETWORK), "render-nft", str(self.settings)],
            capture_output=True,
            text=True,
            check=False,
        )
        self.assertEqual(completed.returncode, expected, completed.stderr)
        return completed

    def test_shell_syntax(self) -> None:
        for script in SHELL_SCRIPTS:
            with self.subTest(script=script.name):
                subprocess.run(["bash", "-n", str(script)], check=True)

    def test_gateway_ip_and_mac_never_cross_the_private_vlan_port(self) -> None:
        rules = self.render().stdout
        bridge = rules[rules.index("table bridge app_ha_guest_gateway") : rules.index("table ip ")]
        # The MAC is normalized to lowercase for nft.
        for rule in (
            'oifname "enp65s0f1" ether saddr 02:00:0a:d5:00:0a counter drop',
            'oifname "enp65s0f1" arp saddr ip 10.213.0.10 counter drop',
            'oifname "enp65s0f1" arp daddr ip 10.213.0.10 counter drop',
            'iifname "enp65s0f1" ether daddr 02:00:0a:d5:00:0a counter drop',
            'iifname "enp65s0f1" arp saddr ip 10.213.0.10 counter drop',
            'iifname "enp65s0f1" arp daddr ip 10.213.0.10 counter drop',
        ):
            self.assertIn(rule, bridge)
        # Only the physical VLAN port is filtered; guest ports are untouched.
        self.assertNotIn("vmbr-private", bridge.split("{", 1)[1])

    def test_egress_masquerades_every_slot_and_handed_over_traffic(self) -> None:
        rules = self.render().stdout
        self.assertIn("elements = { 10.213.0.110-10.213.0.209, 10.213.0.11-10.213.0.13 }", rules)
        self.assertIn('oifname "vmbr0" ip saddr @guest_sources counter masquerade', rules)
        self.assertIn('iifname "bmacgw0" ip saddr 10.213.0.110-10.213.0.209 counter accept', rules)
        self.assertIn(
            'iifname "vmbr-private" oifname "vmbr0" ip saddr @guest_sources counter accept', rules
        )

    def test_invalid_settings_are_refused(self) -> None:
        for old, new in (
            ("BMAC_GUEST_GATEWAY_MAC=02:00:0A:D5:00:0A", "BMAC_GUEST_GATEWAY_MAC=01:00:0a:d5:00:0a"),
            ("BMAC_MOX_INDEX=2", "BMAC_MOX_INDEX=3"),
            ("BMAC_PRIVATE_NIC=enp65s0f1", "BMAC_PRIVATE_NIC=bad name"),
            ("BMAC_SLOT_FIRST=10.213.0.110", "BMAC_SLOT_FIRST=ten"),
            ("BMAC_PUBLIC_IF=vmbr0\n", ""),
        ):
            with self.subTest(new=new):
                self.render(SETTINGS.replace(old, new), expected=1)

    def test_shared_settings_library_renders_from_loaded_config(self) -> None:
        environment = {
            "PATH": os.environ["PATH"],
            "MOX_HOSTNAME": "mox2",
            "MOX_INDEX": "2",
            "PROXMOX_PRIVATE_BRIDGE": "vmbr-private",
            "PROXMOX_SECONDARY_IP": "10.213.0.12/24",
            "PROXMOX_IP": "192.0.2.20",
            "PROXMOX_GATEWAY": "192.0.2.1",
            "GUEST_GATEWAY_IP": "10.213.0.10",
            "GUEST_GATEWAY_MAC": "02:00:0A:D5:00:0A",
            "VM_SLOT_START": "10.213.0.110",
            "VM_SLOT_END": "10.213.0.209",
            "MOX_IP_START": "10.213.0.11",
            "MAX_MOX_HOSTS": "3",
            "PROXMOX_HOST_TRANSFER_RESET_DAY_EFFECTIVE": "1",
        }
        completed = subprocess.run(
            ["bash", "-c", f'source "{SETTINGS_LIB}"; guest_network_settings'],
            env=environment,
            capture_output=True,
            text=True,
            check=True,
        )
        expected = {
            line for line in SETTINGS.splitlines()
            if not line.startswith(("BMAC_PRIVATE_NIC=", "BMAC_PUBLIC_IF=", "BMAC_PUBLIC_NIC="))
        }
        self.assertEqual(
            set(completed.stdout.splitlines()),
            {line.replace("02:00:0A:D5:00:0A", "02:00:0a:d5:00:0a") for line in expected},
        )

    def test_list_slots_offline_shows_every_slot_from_cluster_conf(self) -> None:
        config = Path(self.temporary.name) / "config"
        config.mkdir()
        checked_in = (SCRIPTS_DIR.parent / "config" / "cluster_dot_conf").read_text(encoding="utf-8")
        (config / "cluster.conf").write_text(
            checked_in.replace("MAX_PROD_VMS=10", "MAX_PROD_VMS=2").replace(
                "MAX_STAGING_VMS_PER_PROD=9", "MAX_STAGING_VMS_PER_PROD=2"
            ),
            encoding="utf-8",
        )
        completed = subprocess.run(
            [str(SCRIPTS_DIR / "user_callable" / "diagnostics" / "list_slots.sh"), "--offline"],
            env={**os.environ, "APP_HA_CONFIG_TEST_MODE": "1", "APP_HA_CONFIG_DIR": str(config)},
            capture_output=True,
            text=True,
            check=False,
        )
        self.assertEqual(completed.returncode, 0, completed.stderr)
        slots = [line.split()[:2] for line in completed.stdout.splitlines() if line.startswith(("prod", "stage"))]
        self.assertEqual(
            slots,
            [
                ["prod1", "10.213.0.110"], ["stage1prod1", "10.213.0.111"], ["stage2prod1", "10.213.0.112"],
                ["prod2", "10.213.0.120"], ["stage1prod2", "10.213.0.121"], ["stage2prod2", "10.213.0.122"],
            ],
        )
        self.assertIn("none: no production VM is registered", completed.stdout)

    def test_sync_cluster_network_help_names_every_setting_it_pushes(self) -> None:
        completed = subprocess.run(
            [str(SCRIPTS_DIR / "user_callable" / "hosts" / "sync_cluster_network.sh"), "--help"],
            capture_output=True,
            text=True,
            check=True,
        )
        for text in ("VM slot", "guest gateway", "transfer cap", "probe target", "Tunnel"):
            self.assertIn(text, completed.stdout)


if __name__ == "__main__":
    unittest.main()
