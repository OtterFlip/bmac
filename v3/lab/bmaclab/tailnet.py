# Copyright (c) 2026 BEENTHERE VENTURES, INC.
# SPDX-License-Identifier: GPL-3.0-only

"""The lab's real-tailnet housekeeping: secrets, and removing lab devices.

Lab VMs join with reusable auth keys, and only the discovery key is
ephemeral, so every other lab device stays on the tailnet after its VM is
destroyed or reinstalled. The lab removes those devices itself through the
Tailscale API. It removes a device only if every tag on it is a lab tag; a
device with no tags, or with any other tag, is never touched.

The lab uses the production tags on a Tailscale account of its own, so
before it uses the tailnet it proves that both the API key and this
workstation are on that account (`verify_lab_account`).
"""

from __future__ import annotations

import json
import re
import subprocess
import urllib.error
import urllib.request
from dataclasses import dataclass
from pathlib import Path
from typing import Callable

SECRETS = Path(__file__).resolve().parent.parent / "secrets.env"
API = "https://api.tailscale.com/api/v2"
API_KEY = "TAILSCALE_LAB_API_KEY"
ADMIN_EMAIL = "TAILSCALE_LAB_ADMIN_EMAIL"
ADMIN_GROUP = "group:bmac-admin"
AUTH_KEYS = {
    "proxmox-host": "TAILSCALE_LAB_AUTH_KEY_PROXMOX_HOST",
    "qdevice": "TAILSCALE_LAB_AUTH_KEY_QDEVICE",
    "backup-appliance": "TAILSCALE_LAB_AUTH_KEY_BACKUP_APPLIANCE",
    "discovery": "TAILSCALE_LAB_AUTH_KEY_DISCOVERY",
}
# The production tags: the lab runs on a tailnet of its own (README.md).
LAB_TAGS = frozenset({"tag:bmac-proxmox-host", "tag:bmac-proxmox-qdevice", "tag:bmac-backup-appliance", "tag:bmac-discovery"})


class TailnetError(RuntimeError):
    pass


def read_secrets(path: Path = SECRETS) -> dict[str, str]:
    """Parse secrets.env. Values are returned, never echoed: errors name the
    line number only."""
    if not path.exists():
        raise TailnetError(f"{path} is missing; copy secrets_dot_env there and fill it in")
    if path.stat().st_mode & 0o077:
        raise TailnetError(f"{path} must be readable only by you: chmod 600 {path}")
    values: dict[str, str] = {}
    for number, raw in enumerate(path.read_text().splitlines(), 1):
        line = raw.strip()
        if not line or line.startswith("#"):
            continue
        match = re.fullmatch(r"([A-Z][A-Z0-9_]*)=(.*)", line)
        if not match:
            raise TailnetError(f"{path}:{number}: expected NAME=value with no spaces around '='")
        name, value = match.groups()
        if len(value) >= 2 and value[0] == value[-1] == "'":
            value = value[1:-1]
        elif " " in value or "'" in value:
            raise TailnetError(f"{path}:{number}: enclose a value containing spaces in single quotes")
        values[name] = value
    return values


def require(secrets: dict[str, str], name: str) -> str:
    value = secrets.get(name, "")
    if not value:
        raise TailnetError(f"{name} is not set in lab/secrets.env")
    return value


@dataclass(frozen=True)
class Device:
    id: str
    name: str
    tags: frozenset[str]
    online: bool

    @property
    def label(self) -> str:
        """The device's MagicDNS name without the tailnet suffix."""
        return self.name.split(".", 1)[0]


def is_lab_device(device: Device, prefix: str) -> bool:
    if not device.tags or not device.tags <= LAB_TAGS:
        return False
    return device.label.startswith(prefix)


Opener = Callable[[urllib.request.Request, float], object]


class Tailscale:
    def __init__(self, api_key: str, opener: Opener | None = None) -> None:
        self._key = api_key
        self._open = opener or (lambda request, timeout: urllib.request.urlopen(request, timeout=timeout))

    def _call(self, method: str, path: str, accept: str | None = None) -> bytes:
        headers = {"Authorization": f"Bearer {self._key}"}
        if accept:
            headers["Accept"] = accept
        request = urllib.request.Request(f"{API}{path}", method=method, headers=headers)
        try:
            with self._open(request, 30) as response:
                return response.read()
        except urllib.error.HTTPError as error:
            if error.code in (401, 403):
                raise TailnetError(f"Tailscale refused {API_KEY} (HTTP {error.code}); it may have expired. Make a new API key and replace it in lab/secrets.env") from None
            raise TailnetError(f"Tailscale API {method} {path} failed: HTTP {error.code}") from None
        except urllib.error.URLError as error:
            raise TailnetError(f"cannot reach the Tailscale API: {error.reason}") from None

    def devices(self) -> list[Device]:
        data = json.loads(self._call("GET", "/tailnet/-/devices"))
        return [
            Device(
                id=str(d.get("nodeId") or d["id"]),
                name=str(d.get("name") or d.get("hostname") or ""),
                tags=frozenset(d.get("tags") or ()),
                online=bool(d.get("connectedToControl")),
            )
            for d in data.get("devices", [])
        ]

    def delete(self, device: Device) -> None:
        self._call("DELETE", f"/device/{device.id}")

    def policy(self) -> dict:
        """The tailnet policy file, as JSON rather than HuJSON."""
        return json.loads(self._call("GET", "/tailnet/-/acl", accept="application/json"))


def workstation_status() -> dict:
    """This workstation's `tailscale status --json`."""
    try:
        done = subprocess.run(["tailscale", "status", "--json"], capture_output=True, text=True, timeout=30)
    except (OSError, subprocess.TimeoutExpired) as error:
        raise TailnetError(f"cannot run tailscale status on this workstation: {error}") from None
    if done.returncode != 0:
        raise TailnetError("tailscale status failed on this workstation; is Tailscale running and logged in?")
    try:
        return json.loads(done.stdout)
    except json.JSONDecodeError:
        raise TailnetError("tailscale status --json printed something that isn't JSON") from None


def verify_lab_account(api: Tailscale, secrets: dict[str, str], status: dict) -> None:
    """Refuse unless the API key's tailnet is the lab's (its policy puts
    TAILSCALE_LAB_ADMIN_EMAIL in group:bmac-admin) and this workstation is
    logged in as that address to that same tailnet (its own node is one of
    its devices). The
    lab and production share tags, so either key or login pointing at
    production would let the lab make or remove production devices."""
    email = require(secrets, ADMIN_EMAIL).strip().lower()
    admins = [str(m).strip().lower() for m in (api.policy().get("groups") or {}).get(ADMIN_GROUP) or []]
    if email not in admins:
        raise TailnetError(
            f"the tailnet of {API_KEY} doesn't list {ADMIN_EMAIL} in {ADMIN_GROUP}, so it may be the "
            "production account; the lab won't use it")
    me = status.get("Self") or {}
    node = str(me.get("ID") or "")
    if not node:
        raise TailnetError("this workstation isn't logged in to Tailscale")
    login = str(((status.get("User") or {}).get(str(me.get("UserID"))) or {}).get("LoginName") or "")
    if login.strip().lower() != email:
        raise TailnetError(
            f"this workstation is logged in to Tailscale as {login or 'an unknown user'}, not "
            f"{ADMIN_EMAIL}, perhaps on the production account; log in to the lab account "
            "(tailscale switch) before running the lab")
    if node not in {d.id for d in api.devices()}:
        raise TailnetError(
            f"this workstation is logged in to a different tailnet than {API_KEY}'s, perhaps the "
            "production account; log in to the lab account (tailscale switch) before running the lab")


def lab_devices(api: Tailscale, prefix: str, labels: set[str] | None = None) -> list[Device]:
    """Lab devices, optionally only those named LABEL or LABEL-<n> (the name
    Tailscale gives a second device that joins under a name in use)."""
    found = [d for d in api.devices() if is_lab_device(d, prefix)]
    if labels is None:
        return found
    return [d for d in found if any(re.fullmatch(re.escape(label) + r"(-\d+)?", d.label) for label in labels)]


def remove(api: Tailscale, devices: list[Device], log: Callable[[str], None]) -> None:
    for device in devices:
        if not device.tags or not device.tags <= LAB_TAGS:
            raise TailnetError(f"refusing to remove {device.label}: it has tags other than the lab's")
        api.delete(device)
        log(f"Removed {device.label} from the tailnet")
