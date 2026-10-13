# Copyright (c) 2026 BEENTHERE VENTURES, INC.
# SPDX-License-Identifier: GPL-3.0-only

"""Lab secrets and removing lab devices from the tailnet, with a fake API."""

from __future__ import annotations

import io
import json
import sys
import tempfile
import unittest
import urllib.error
from pathlib import Path

LAB = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(LAB))

from bmaclab.tailnet import Device, Tailscale, TailnetError, lab_devices, read_secrets, remove, verify_lab_account  # noqa: E402

KEY = "tskey-api-not-a-real-key"


class FakeApi:
    def __init__(self, devices: list[dict], status: int = 200, policy: dict | None = None) -> None:
        self.devices = devices
        self.status = status
        self.policy = policy or {}
        self.calls: list[tuple[str, str, str | None]] = []

    def __call__(self, request, timeout):
        self.calls.append((request.get_method(), request.full_url, request.get_header("Authorization")))
        if self.status != 200:
            raise urllib.error.HTTPError(request.full_url, self.status, "error", {}, None)
        if request.full_url.endswith("/acl"):
            body = json.dumps(self.policy).encode()
        else:
            body = json.dumps({"devices": self.devices}).encode() if request.get_method() == "GET" else b""
        return io.BytesIO(body)


def device(name: str, tags: list[str], node: str | None = None) -> dict:
    return {"id": "1", "nodeId": node or f"n-{name}", "name": f"{name}.example.ts.net", "hostname": name, "tags": tags, "connectedToControl": False}


class SecretsTest(unittest.TestCase):
    def setUp(self) -> None:
        scratch = tempfile.TemporaryDirectory(prefix="bmac-lab-tailnet-")
        self.addCleanup(scratch.cleanup)
        self.path = Path(scratch.name) / "secrets.env"

    def write(self, text: str, mode: int = 0o600) -> None:
        self.path.write_text(text)
        self.path.chmod(mode)

    def test_reads_names_values_and_quoted_values(self) -> None:
        self.write("# comment\n\nA=one\nB='two words'\nC=\n")
        self.assertEqual(read_secrets(self.path), {"A": "one", "B": "two words", "C": ""})

    def test_refuses_a_file_others_can_read(self) -> None:
        self.write("A=one\n", 0o644)
        with self.assertRaisesRegex(TailnetError, "chmod 600"):
            read_secrets(self.path)

    def test_errors_name_the_line_but_never_the_value(self) -> None:
        self.write(f"A = {KEY}\n")
        with self.assertRaises(TailnetError) as caught:
            read_secrets(self.path)
        self.assertIn(":1:", str(caught.exception))
        self.assertNotIn(KEY, str(caught.exception))


class DevicesTest(unittest.TestCase):
    DEVICES = [
        device("bmaclab-mox1", ["tag:bmac-proxmox-host"]),
        device("bmaclab-mox1-1", ["tag:bmac-proxmox-host"]),
        device("bmaclab-qdevice", ["tag:bmac-proxmox-qdevice"]),
        device("bmaclab-discover-x", ["tag:bmac-discovery"]),
        device("bmac-discover-y", ["tag:bmac-discovery"]),
        device("mox1", ["tag:bmac-proxmox-host"]),
        device("bmaclab-mox2", ["tag:bmac-proxmox-host", "tag:server"]),
        device("bmaclab-laptop", []),
        device("workstation", ["tag:bmac-proxmox-host"]),
    ]

    def labels(self, devices: list[Device]) -> list[str]:
        return [d.label for d in devices]

    def test_only_prefixed_devices_tagged_solely_with_bmac_tags_are_lab_devices(self) -> None:
        api = Tailscale(KEY, FakeApi(self.DEVICES))
        self.assertEqual(self.labels(lab_devices(api, "bmaclab-")), ["bmaclab-mox1", "bmaclab-mox1-1", "bmaclab-qdevice", "bmaclab-discover-x"])

    def test_a_vm_selects_its_name_and_the_suffixed_names_tailscale_gives_rejoins(self) -> None:
        api = Tailscale(KEY, FakeApi(self.DEVICES))
        self.assertEqual(self.labels(lab_devices(api, "bmaclab-", {"bmaclab-mox1"})), ["bmaclab-mox1", "bmaclab-mox1-1"])

    def test_remove_deletes_by_node_id_with_the_api_key(self) -> None:
        fake = FakeApi(self.DEVICES)
        api = Tailscale(KEY, fake)
        lines: list[str] = []
        remove(api, lab_devices(api, "bmaclab-", {"bmaclab-qdevice"}), lines.append)
        self.assertEqual(fake.calls[-1], ("DELETE", "https://api.tailscale.com/api/v2/device/n-bmaclab-qdevice", f"Bearer {KEY}"))
        self.assertEqual(lines, ["Removed bmaclab-qdevice from the tailnet"])

    def test_remove_refuses_a_device_with_any_other_tag(self) -> None:
        fake = FakeApi([])
        with self.assertRaisesRegex(TailnetError, "refusing"):
            remove(Tailscale(KEY, fake), [Device("n", "mox1.example.ts.net", frozenset({"tag:server"}), True)], print)
        self.assertEqual(fake.calls, [])

    def test_a_refused_api_key_is_reported_without_its_value(self) -> None:
        with self.assertRaises(TailnetError) as caught:
            Tailscale(KEY, FakeApi([], status=401)).devices()
        self.assertIn("TAILSCALE_LAB_API_KEY", str(caught.exception))
        self.assertNotIn(KEY, str(caught.exception))


if __name__ == "__main__":
    unittest.main()


class LabAccountTest(unittest.TestCase):
    POLICY = {"groups": {"group:bmac-admin": ["Lab.Admin@example.com"]}}
    SECRETS = {"TAILSCALE_LAB_ADMIN_EMAIL": "lab.admin@example.com"}
    WORKSTATION = {"Self": {"ID": "n-workstation", "UserID": 7}, "User": {"7": {"LoginName": "lab.admin@example.com"}}}

    def api(self, policy: dict | None = None) -> Tailscale:
        return Tailscale(KEY, FakeApi([device("workstation", [])], policy=self.POLICY if policy is None else policy))

    def test_the_lab_account_with_this_workstation_on_it_passes(self) -> None:
        verify_lab_account(self.api(), self.SECRETS, self.WORKSTATION)

    def test_a_key_whose_policy_lacks_the_lab_admin_is_refused(self) -> None:
        with self.assertRaisesRegex(TailnetError, "production"):
            verify_lab_account(self.api({"groups": {"group:bmac-admin": ["prod@example.com"]}}), self.SECRETS, self.WORKSTATION)

    def test_a_workstation_on_another_tailnet_is_refused(self) -> None:
        with self.assertRaisesRegex(TailnetError, "different tailnet"):
            verify_lab_account(self.api(), self.SECRETS, {**self.WORKSTATION, "Self": {"ID": "n-elsewhere", "UserID": 7}})

    def test_a_workstation_logged_in_as_someone_else_is_refused(self) -> None:
        other = {**self.WORKSTATION, "User": {"7": {"LoginName": "prod@example.com"}}}
        with self.assertRaisesRegex(TailnetError, "prod@example.com"):
            verify_lab_account(self.api(), self.SECRETS, other)

    def test_the_admin_email_is_required(self) -> None:
        with self.assertRaisesRegex(TailnetError, "TAILSCALE_LAB_ADMIN_EMAIL"):
            verify_lab_account(self.api(), {}, self.WORKSTATION)
