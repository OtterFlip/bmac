#!/usr/bin/env python3

# Copyright (c) 2026 BEENTHERE VENTURES, INC.
# SPDX-License-Identifier: GPL-3.0-only

"""Unit tests for the Cloudflare Tunnel config renderer and the VM slots."""

from __future__ import annotations

import importlib.util
import json
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest


RENDERER_PATH = Path(__file__).resolve().parent.parent / "host_runtime" / "tunnel_config.py"
SPEC = importlib.util.spec_from_file_location("tunnel_config", RENDERER_PATH)
assert SPEC is not None and SPEC.loader is not None
tunnel_config = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = tunnel_config
SPEC.loader.exec_module(tunnel_config)

TUNNEL_ID = "0521f49c-7aa2-48d3-9a8a-b8b0b24a1ca4"


def slots(max_prod: int = 10, max_staging: int = 9) -> "tunnel_config.Slots":
    return tunnel_config.Slots("10.213.0.0/24", "10.213.0.110", max_prod, max_staging)


def production(number: int = 1, primary: str = "example.com", aliases=()) -> dict:
    return {
        "name": f"prod{number}",
        "ip": str(slots().production(number)),
        "primary": primary,
        "aliases": list(aliases),
    }


class SlotTest(unittest.TestCase):
    def test_slot_formula(self) -> None:
        layout = slots()
        self.assertEqual(str(layout.production(1)), "10.213.0.110")
        self.assertEqual(str(layout.production(10)), "10.213.0.200")
        self.assertEqual(str(layout.staging(1, 1)), "10.213.0.111")
        self.assertEqual(str(layout.staging(3, 9)), "10.213.0.139")
        self.assertEqual(str(layout.last), "10.213.0.209")
        with self.assertRaises(tunnel_config.TunnelConfigError):
            layout.production(11)
        with self.assertRaises(tunnel_config.TunnelConfigError):
            layout.staging(1, 10)

    def test_invalid_slot_settings_are_refused(self) -> None:
        for args in (
            ("10.213.0.0/24", "10.213.0.115", 10, 9),  # start must end in 0
            ("10.213.0.0/24", "10.213.0.110", 11, 9),  # at most 10 production
            ("10.213.0.0/24", "10.213.0.110", 10, 10),  # at most 9 staging
            ("10.213.0.0/24", "10.213.0.250", 1, 9),  # .250-.259 leaves the /24
            ("10.213.0.0/24", "10.214.0.10", 1, 9),  # outside the network
        ):
            with self.subTest(args=args), self.assertRaises(tunnel_config.TunnelConfigError):
                tunnel_config.Slots(*args)


class RouteTest(unittest.TestCase):
    def test_every_fqdn_of_a_production_guest_and_its_staging_slots(self) -> None:
        rows = tunnel_config.routes(
            [production(2, "shop.example.net", ["www.shop.example.net", "example.org"])],
            slots(max_staging=3),
        )
        self.assertEqual(
            [(row["fqdn"], row["resource"], row["ip"]) for row in rows],
            [
                ("shop.example.net", "prod2", "10.213.0.120"),
                ("www.shop.example.net", "prod2", "10.213.0.120"),
                ("example.org", "prod2", "10.213.0.120"),
                ("stage1prod2.shop.example.net", "stage1prod2", "10.213.0.121"),
                ("stage2prod2.shop.example.net", "stage2prod2", "10.213.0.122"),
                ("stage3prod2.shop.example.net", "stage3prod2", "10.213.0.123"),
            ],
        )

    def test_production_must_sit_in_its_own_slot(self) -> None:
        row = production(1)
        row["ip"] = "10.213.0.111"
        with self.assertRaisesRegex(tunnel_config.TunnelConfigError, "is not its slot"):
            tunnel_config.routes([row], slots())

    def test_an_fqdn_may_belong_to_only_one_guest(self) -> None:
        with self.assertRaisesRegex(tunnel_config.TunnelConfigError, "claimed by both"):
            tunnel_config.routes(
                [production(1, "a.example.com"), production(2, "b.example.com", ["a.example.com"])],
                slots(),
            )

    def test_invalid_names_and_domains_are_refused(self) -> None:
        for row in (
            {**production(1), "name": "stage1prod1"},
            {**production(1), "primary": "Example.com"},
            {**production(1), "primary": "10.0.0.1"},
            {**production(1), "aliases": ["bad_label.example.com"]},
        ):
            with self.subTest(row=row), self.assertRaises(tunnel_config.TunnelConfigError):
                tunnel_config.routes([row], slots())


class RenderTest(unittest.TestCase):
    def test_rendered_config_pins_http2_and_ends_with_a_404(self) -> None:
        text = tunnel_config.render([production(1)], slots(max_staging=1), tunnel_id=TUNNEL_ID)
        lines = text.splitlines()
        self.assertTrue(lines[0].startswith(tunnel_config.GENERATION_PREFIX))
        self.assertIn(f'tunnel: "{TUNNEL_ID}"', lines)
        # QUIC runs over UDP, which FiberState throttles.
        self.assertIn('protocol: "http2"', lines)
        self.assertIn('  - hostname: "stage1prod1.example.com"', lines)
        self.assertIn('    service: "https://10.213.0.111:443"', lines)
        self.assertIn('      originServerName: "stage1prod1.example.com"', lines)
        self.assertEqual(lines[-1], '  - service: "http_status:404"')

    def test_render_is_deterministic_and_carries_its_generation(self) -> None:
        productions = [production(2, "b.example.com"), production(1, "a.example.com")]
        first = tunnel_config.render(productions, slots(), tunnel_id=TUNNEL_ID)
        second = tunnel_config.render(list(reversed(productions)), slots(), tunnel_id=TUNNEL_ID)
        self.assertEqual(first, second)
        generation = tunnel_config.generation_of(first)
        self.assertRegex(generation or "", r"^[0-9a-f]{64}$")
        changed = tunnel_config.render(productions[:1], slots(), tunnel_id=TUNNEL_ID)
        self.assertNotEqual(tunnel_config.generation_of(changed), generation)

    def test_empty_registry_renders_only_the_catch_all(self) -> None:
        text = tunnel_config.render([], slots(), tunnel_id=TUNNEL_ID)
        self.assertNotIn("hostname:", text)
        self.assertIn('  - service: "http_status:404"', text)

    def test_tunnel_id_must_be_a_uuid(self) -> None:
        with self.assertRaises(tunnel_config.TunnelConfigError):
            tunnel_config.render([], slots(), tunnel_id="not-a-uuid")


class CredentialsTest(unittest.TestCase):
    def test_credentials_file_matches_cloudflared_format(self) -> None:
        secret = "a" * 43 + "="
        text = tunnel_config.credentials("0123456789abcdef0123456789abcdef", TUNNEL_ID, secret)
        self.assertEqual(
            json.loads(text),
            {
                "AccountTag": "0123456789abcdef0123456789abcdef",
                "TunnelSecret": secret,
                "TunnelID": TUNNEL_ID,
            },
        )
        for account, tunnel, value in (
            ("ABC", TUNNEL_ID, secret),
            ("0123456789abcdef0123456789abcdef", "bad", secret),
            ("0123456789abcdef0123456789abcdef", TUNNEL_ID, "short"),
        ):
            with self.assertRaises(tunnel_config.TunnelConfigError):
                tunnel_config.credentials(account, tunnel, value)

    def test_cli_reads_the_secret_from_the_environment_and_writes_mode_0640(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            output = Path(temporary) / "creds.json"
            secret = "b" * 44
            completed = subprocess.run(
                [
                    sys.executable, "-I", str(RENDERER_PATH), "credentials",
                    "--account-id", "0123456789abcdef0123456789abcdef",
                    "--tunnel-id", TUNNEL_ID, "--output", str(output),
                ],
                env={"CLOUDFLARE_TUNNEL_SECRET": secret, "PATH": "/usr/bin:/bin"},
                capture_output=True,
                text=True,
                check=False,
            )
            self.assertEqual(completed.returncode, 0, completed.stderr)
            self.assertNotIn(secret, completed.stdout + completed.stderr)
            self.assertEqual(output.stat().st_mode & 0o777, 0o640)
            self.assertEqual(json.loads(output.read_text())["TunnelSecret"], secret)

    def test_cli_render_and_generation_round_trip(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            output = Path(temporary) / "config.yml"
            rendered = subprocess.run(
                [
                    sys.executable, "-I", str(RENDERER_PATH), "render",
                    "--network", "10.213.0.0/24", "--slot-start", "10.213.0.110",
                    "--max-prod", "10", "--max-staging", "9",
                    "--productions", "-", "--tunnel-id", TUNNEL_ID, "--output", str(output),
                ],
                input=json.dumps([production(1)]),
                capture_output=True,
                text=True,
                check=True,
            )
            generation = subprocess.run(
                [sys.executable, "-I", str(RENDERER_PATH), "generation", str(output)],
                capture_output=True,
                text=True,
                check=True,
            )
            self.assertEqual(rendered.stdout.strip(), generation.stdout.strip())


if __name__ == "__main__":
    unittest.main()
