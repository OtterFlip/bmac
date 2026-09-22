#!/usr/bin/env python3

# Copyright (c) 2026 BEENTHERE VENTURES, INC.
# SPDX-License-Identifier: GPL-3.0-only

"""Build a per-VM Ubuntu autoinstall ISO without plaintext secrets.

The request contains a passwd-compatible root password hash, never the
plaintext password.  The source ISO is verified before xorriso opens it, and
the output is atomically installed only after xorriso has replayed the source
image's BIOS/UEFI boot metadata.
"""

from __future__ import annotations

import argparse
import hashlib
import ipaddress
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import sys
import tempfile
from typing import Any, Callable, Sequence


HOSTNAME_RE = re.compile(r"^prod[1-9][0-9]*$")
MAC_RE = re.compile(r"^(?:[0-9a-f]{2}:){5}[0-9a-f]{2}$")
SSH_KEY_RE = re.compile(
    r"^(?:ssh-(?:ed25519|rsa)|ecdsa-sha2-[^ ]+|sk-ssh-[^ ]+|"
    r"sk-ecdsa-sha2-[^ ]+) [A-Za-z0-9+/=]+(?: [^\r\n]+)?$"
)
SHA512_CRYPT_RE = re.compile(r"^\$6\$(?:rounds=[0-9]+\$)?[^$\r\n]+\$[^$\r\n]+$")
REQUEST_KEYS = {
    "hostname",
    "address",
    "gateway",
    "production_ip_start",
    "production_ip_end",
    "nameservers",
    "mac",
    "root_password_hash",
    "authorized_keys",
    "startup_script",
}
MAX_STARTUP_SCRIPT_BYTES = 256 * 1024
STARTUP_RUNNER = """#!/bin/sh
# Runs startup.sh until one successful completion, then records a durable
# marker. startup.sh MUST be idempotent because a failed attempt is retried.
set -eu
state_dir=/var/lib/production-startup
marker="${state_dir}/completed"
[ ! -e "${marker}" ] || exit 0
install -d -m 0700 "${state_dir}"
/usr/local/libexec/production-startup.sh
temporary="${state_dir}/.completed.$$"
: >"${temporary}"
chmod 0600 "${temporary}"
mv -f "${temporary}" "${marker}"
"""
STARTUP_UNIT = """[Unit]
Description=Run operator startup.sh once before networking
DefaultDependencies=no
After=local-fs.target
Before=network-pre.target network.target network-online.target
ConditionPathExists=!/var/lib/production-startup/completed

[Service]
Type=oneshot
ExecStart=/usr/local/libexec/run-production-startup-once
RemainAfterExit=yes

[Install]
WantedBy=network-pre.target
"""


class BuildError(ValueError):
    """A safe, user-facing build error."""


def _expect_exact_keys(value: dict[str, Any]) -> None:
    unknown = sorted(set(value) - REQUEST_KEYS)
    missing = sorted(REQUEST_KEYS - set(value))
    if unknown:
        raise BuildError(f"request has unknown fields: {', '.join(unknown)}")
    if missing:
        raise BuildError(f"request is missing fields: {', '.join(missing)}")


def validate_request(raw: Any) -> dict[str, Any]:
    """Validate and normalize the secret-free ISO request."""

    if not isinstance(raw, dict):
        raise BuildError("request must be a JSON object")
    _expect_exact_keys(raw)

    hostname = raw["hostname"]
    if not isinstance(hostname, str) or not HOSTNAME_RE.fullmatch(hostname):
        raise BuildError("hostname must use prodN naming")

    try:
        address = ipaddress.ip_interface(raw["address"])
        gateway = ipaddress.ip_address(raw["gateway"])
    except (TypeError, ValueError) as exc:
        raise BuildError(f"invalid guest network address: {exc}") from exc
    if not isinstance(address, ipaddress.IPv4Interface) or address.network.prefixlen != 24:
        raise BuildError("guest address must be an IPv4 /24")
    if not isinstance(gateway, ipaddress.IPv4Address) or gateway not in address.network:
        raise BuildError("gateway must be IPv4 in the guest /24")
    try:
        production_start = ipaddress.ip_address(raw["production_ip_start"])
        production_end = ipaddress.ip_address(raw["production_ip_end"])
    except (TypeError, ValueError) as exc:
        raise BuildError(f"invalid production address range: {exc}") from exc
    if (
        not isinstance(production_start, ipaddress.IPv4Address)
        or not isinstance(production_end, ipaddress.IPv4Address)
        or production_start not in address.network
        or production_end not in address.network
        or int(production_start) > int(production_end)
    ):
        raise BuildError("production address range must be ascending within the guest /24")
    host_octet = int(address.ip) - int(address.network.network_address)
    production_start_octet = int(production_start) - int(address.network.network_address)
    production_end_octet = int(production_end) - int(address.network.network_address)
    if not production_start_octet <= host_octet <= production_end_octet:
        raise BuildError("production guest address is outside its configured range")
    if host_octet != production_start_octet + int(hostname.removeprefix("prod")) - 1:
        raise BuildError("production hostname and IP do not follow the registry formula")

    mac = raw["mac"]
    if not isinstance(mac, str) or not MAC_RE.fullmatch(mac):
        raise BuildError("MAC must be normalized lowercase hexadecimal")
    if int(mac.split(":", 1)[0], 16) & 0b11 != 0b10:
        raise BuildError("MAC must be a locally administered unicast address")

    nameservers = raw["nameservers"]
    if not isinstance(nameservers, list) or not nameservers:
        raise BuildError("at least one DNS server is required")
    normalized_nameservers: list[str] = []
    for value in nameservers:
        try:
            parsed = ipaddress.ip_address(value)
        except (TypeError, ValueError) as exc:
            raise BuildError(f"invalid DNS server: {value!r}") from exc
        text = str(parsed)
        if text in normalized_nameservers:
            raise BuildError(f"duplicate DNS server: {text}")
        normalized_nameservers.append(text)

    root_password_hash = raw["root_password_hash"]
    if not isinstance(root_password_hash, str) or not SHA512_CRYPT_RE.fullmatch(
        root_password_hash
    ):
        raise BuildError("root password must be a SHA-512 crypt ($6$) hash")

    authorized_keys = raw["authorized_keys"]
    if (
        not isinstance(authorized_keys, list)
        or len(authorized_keys) != 2
        or not all(isinstance(value, str) for value in authorized_keys)
    ):
        raise BuildError("exactly two administrator public SSH keys are required")
    normalized_keys: list[str] = []
    for value in authorized_keys:
        key = value.strip()
        if not SSH_KEY_RE.fullmatch(key):
            raise BuildError("administrator key is not a single OpenSSH public key")
        if key in normalized_keys:
            raise BuildError("administrator public SSH keys must be distinct")
        normalized_keys.append(key)

    startup_script = raw["startup_script"]
    if startup_script is not None:
        if not isinstance(startup_script, str):
            raise BuildError("startup_script must be UTF-8 text or null")
        if "\x00" in startup_script:
            raise BuildError("startup_script may not contain NUL")
        if not startup_script.startswith("#!"):
            raise BuildError("startup_script must begin with a shebang")
        if not 0 < len(startup_script.encode("utf-8")) <= MAX_STARTUP_SCRIPT_BYTES:
            raise BuildError(
                f"startup_script must contain 1 through "
                f"{MAX_STARTUP_SCRIPT_BYTES} UTF-8 bytes"
            )

    return {
        "hostname": hostname,
        "address": str(address),
        "gateway": str(gateway),
        "production_ip_start": str(production_start),
        "production_ip_end": str(production_end),
        "nameservers": normalized_nameservers,
        "mac": mac,
        "root_password_hash": root_password_hash,
        "authorized_keys": normalized_keys,
        "startup_script": startup_script,
    }


def build_user_data(request: dict[str, Any]) -> str:
    """Render cloud-config as JSON, which is valid YAML and avoids quoting bugs."""

    request = validate_request(request)
    sshd_policy = "\n".join(
        (
            "# Managed by production VM autoinstall.",
            "PubkeyAuthentication yes",
            "PasswordAuthentication no",
            "KbdInteractiveAuthentication no",
            "PermitRootLogin prohibit-password",
            "",
        )
    )
    late_commands = [
        "curtin in-target --target=/target -- "
        "systemctl enable qemu-guest-agent.service"
    ]
    if request["startup_script"] is not None:
        late_commands.extend(
            (
                "install -D -m 0700 /cdrom/nocloud/startup.sh "
                "/target/usr/local/libexec/production-startup.sh",
                "install -D -m 0700 /cdrom/nocloud/run-startup-once "
                "/target/usr/local/libexec/run-production-startup-once",
                "install -D -m 0644 /cdrom/nocloud/startup-once.service "
                "/target/etc/systemd/system/production-startup-once.service",
                "curtin in-target --target=/target -- "
                "systemctl enable production-startup-once.service",
            )
        )
    document = {
        "autoinstall": {
            "version": 1,
            "refresh-installer": {"update": False},
            "locale": "en_US.UTF-8",
            "keyboard": {"layout": "us"},
            "network": {
                "version": 2,
                "ethernets": {
                    "private": {
                        "match": {"macaddress": request["mac"]},
                        "set-name": "lan0",
                        "dhcp4": False,
                        "dhcp6": False,
                        "addresses": [request["address"]],
                        "routes": [
                            {
                                "to": "default",
                                "via": request["gateway"],
                            }
                        ],
                        "nameservers": {"addresses": request["nameservers"]},
                    }
                },
            },
            # The VM has one data disk. "direct" avoids a second LVM data layer
            # and lets the guest consume the Proxmox-managed ZFS zvol.
            "storage": {"layout": {"name": "direct"}},
            "ssh": {
                "install-server": True,
                "allow-pw": False,
            },
            "packages": ["openssh-server", "qemu-guest-agent"],
            "updates": "security",
            "shutdown": "poweroff",
            "late-commands": late_commands,
            "user-data": {
                "hostname": request["hostname"],
                "manage_etc_hosts": True,
                "disable_root": False,
                "ssh_pwauth": False,
                "users": [
                    {
                        "name": "root",
                        "lock_passwd": False,
                        # root already exists in the target image. cloud-init's
                        # hashed_passwd field explicitly updates existing users;
                        # the older passwd field is creation-only.
                        "hashed_passwd": request["root_password_hash"],
                        "shell": "/bin/bash",
                        "ssh_authorized_keys": request["authorized_keys"],
                    }
                ],
                "write_files": [
                    {
                        "path": "/etc/ssh/sshd_config.d/60-production-root-key-only.conf",
                        "owner": "root:root",
                        "permissions": "0644",
                        "content": sshd_policy,
                    }
                ],
                "runcmd": [
                    ["systemctl", "enable", "--now", "qemu-guest-agent.service"],
                    ["systemctl", "restart", "ssh.service"],
                ],
            },
        }
    }
    return "#cloud-config\n" + json.dumps(
        document, sort_keys=True, indent=2, ensure_ascii=True
    ) + "\n"


def build_meta_data(request: dict[str, Any]) -> str:
    request = validate_request(request)
    instance = request["mac"].replace(":", "")
    return (
        f"instance-id: production-{request['hostname']}-{instance}\n"
        f"local-hostname: {request['hostname']}\n"
    )


def inject_autoinstall_menu(original: str) -> str:
    """Prepend a deterministic unattended entry to the source GRUB menu."""

    if "\x00" in original:
        raise BuildError("source GRUB configuration contains NUL")
    filtered = "\n".join(
        line
        for line in original.splitlines()
        if not re.match(r"^[ \t]*set[ \t]+(?:default|timeout)=", line)
    )
    menu = """set default=0
set timeout=5

menuentry "Production unattended Ubuntu Server install" {
    set gfxpayload=keep
    linux /casper/vmlinuz quiet autoinstall ds=nocloud\\;s=/cdrom/nocloud/ ---
    initrd /casper/initrd
}

"""
    return menu + filtered.lstrip() + ("\n" if filtered else "")


def sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        for chunk in iter(lambda: stream.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def md5_file(path: Path) -> str:
    try:
        digest = hashlib.md5(usedforsecurity=False)
    except TypeError:  # pragma: no cover - compatibility with older Python.
        digest = hashlib.md5()
    with path.open("rb") as stream:
        for chunk in iter(lambda: stream.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def update_md5_manifest(original: str, replacements: dict[str, Path]) -> str:
    """Update Ubuntu's media-check manifest for mapped and added files."""

    remaining = dict(replacements)
    result: list[str] = []
    for line in original.splitlines():
        match = re.fullmatch(r"([0-9a-fA-F]{32})([ \t]+[*]?)(.+)", line)
        if not match:
            result.append(line)
            continue
        displayed_path = match.group(3)
        normalized_path = displayed_path.removeprefix("./").lstrip("/")
        replacement = remaining.pop(normalized_path, None)
        if replacement is None:
            result.append(line)
        else:
            result.append(
                f"{md5_file(replacement)}{match.group(2)}{displayed_path}"
            )
    for normalized_path, replacement in sorted(remaining.items()):
        result.append(f"{md5_file(replacement)}  ./{normalized_path}")
    return "\n".join(result) + "\n"


def validate_source_iso(path: Path, expected_sha256: str) -> None:
    if (
        not path.is_absolute()
        or path.is_symlink()
        or not path.is_file()
        or path.suffix.lower() != ".iso"
    ):
        raise BuildError(
            "source must be an absolute, regular, non-symlink ISO file"
        )
    if not re.fullmatch(r"[0-9a-fA-F]{64}", expected_sha256):
        raise BuildError("expected source SHA-256 must contain 64 hexadecimal characters")
    observed = sha256_file(path)
    if observed != expected_sha256.lower():
        raise BuildError(
            "Ubuntu source ISO SHA-256 mismatch; refusing to build "
            f"(expected {expected_sha256.lower()}, observed {observed})"
        )


RunCommand = Callable[..., subprocess.CompletedProcess[str]]


def _run(
    command: Sequence[str],
    *,
    description: str,
    runner: RunCommand = subprocess.run,
) -> subprocess.CompletedProcess[str]:
    try:
        completed = runner(
            list(command),
            stdin=subprocess.DEVNULL,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
            check=False,
        )
    except OSError as exc:
        raise BuildError(f"cannot run {description}: {exc}") from exc
    if completed.returncode != 0:
        detail = completed.stderr.strip().replace("\n", " ")[:600]
        raise BuildError(
            f"{description} failed with exit {completed.returncode}: "
            f"{detail or 'no diagnostic'}"
        )
    return completed


def _extract_iso_file(
    xorriso: Path,
    source: Path,
    iso_path: str,
    destination: Path,
    *,
    required: bool,
    runner: RunCommand,
) -> bool:
    try:
        _run(
            (
                str(xorriso),
                "-abort_on",
                "FAILURE",
                "-osirrox",
                "on",
                "-indev",
                str(source),
                "-extract",
                iso_path,
                str(destination),
            ),
            description=f"extracting {iso_path}",
            runner=runner,
        )
    except BuildError:
        if required:
            raise
        return False
    # Rock Ridge permissions are preserved by xorriso. Ubuntu installation
    # media records these files as 0444, so make the private extracted copy
    # writable before callers replace its contents.
    destination.chmod(0o600)
    return True


def build_iso(
    *,
    source: Path,
    expected_sha256: str,
    output: Path,
    request: dict[str, Any],
    xorriso: Path = Path("/usr/bin/xorriso"),
    runner: RunCommand = subprocess.run,
) -> None:
    """Build and atomically install a bootable per-VM ISO."""

    request = validate_request(request)
    validate_source_iso(source, expected_sha256)
    if not output.is_absolute() or output == source or output.is_symlink():
        raise BuildError("output ISO must be a distinct absolute non-symlink path")
    output.parent.mkdir(mode=0o700, parents=True, exist_ok=True)
    if output.parent.is_symlink() or not output.parent.is_dir():
        raise BuildError("output ISO parent must be a non-symlink directory")
    if not xorriso.is_file() or not os.access(xorriso, os.X_OK):
        raise BuildError(f"xorriso must be an executable regular file: {xorriso}")

    temporary_root = Path(
        tempfile.mkdtemp(prefix=f".{request['hostname']}-iso.", dir=output.parent)
    )
    temporary_root.chmod(0o700)
    candidate = temporary_root / "candidate.iso"
    try:
        seed = temporary_root / "nocloud"
        seed.mkdir(mode=0o700)
        user_data = seed / "user-data"
        meta_data = seed / "meta-data"
        user_data.write_text(build_user_data(request), encoding="utf-8")
        meta_data.write_text(build_meta_data(request), encoding="utf-8")
        user_data.chmod(0o600)
        meta_data.chmod(0o600)
        startup_assets: dict[str, Path] = {}
        if request["startup_script"] is not None:
            startup_assets = {
                "nocloud/startup.sh": seed / "startup.sh",
                "nocloud/run-startup-once": seed / "run-startup-once",
                "nocloud/startup-once.service": seed / "startup-once.service",
            }
            startup_assets["nocloud/startup.sh"].write_text(
                request["startup_script"], encoding="utf-8"
            )
            startup_assets["nocloud/run-startup-once"].write_text(
                STARTUP_RUNNER, encoding="utf-8"
            )
            startup_assets["nocloud/startup-once.service"].write_text(
                STARTUP_UNIT, encoding="utf-8"
            )
            startup_assets["nocloud/startup.sh"].chmod(0o700)
            startup_assets["nocloud/run-startup-once"].chmod(0o700)
            startup_assets["nocloud/startup-once.service"].chmod(0o600)

        grub = temporary_root / "grub.cfg"
        _extract_iso_file(
            xorriso,
            source,
            "/boot/grub/grub.cfg",
            grub,
            required=True,
            runner=runner,
        )
        grub.write_text(
            inject_autoinstall_menu(grub.read_text(encoding="utf-8")),
            encoding="utf-8",
        )
        grub.chmod(0o600)

        loopback = temporary_root / "loopback.cfg"
        has_loopback = _extract_iso_file(
            xorriso,
            source,
            "/boot/grub/loopback.cfg",
            loopback,
            required=False,
            runner=runner,
        )
        if has_loopback:
            loopback.write_text(
                inject_autoinstall_menu(loopback.read_text(encoding="utf-8")),
                encoding="utf-8",
            )
            loopback.chmod(0o600)

        md5_manifest = temporary_root / "md5sum.txt"
        has_md5_manifest = _extract_iso_file(
            xorriso,
            source,
            "/md5sum.txt",
            md5_manifest,
            required=False,
            runner=runner,
        )
        if has_md5_manifest:
            replacements = {
                "boot/grub/grub.cfg": grub,
                "nocloud/user-data": user_data,
                "nocloud/meta-data": meta_data,
            }
            replacements.update(startup_assets)
            if has_loopback:
                replacements["boot/grub/loopback.cfg"] = loopback
            md5_manifest.write_text(
                update_md5_manifest(
                    md5_manifest.read_text(encoding="utf-8"),
                    replacements,
                ),
                encoding="utf-8",
            )
            md5_manifest.chmod(0o600)

        command = [
            str(xorriso),
            "-abort_on",
            "FAILURE",
            "-indev",
            str(source),
            "-outdev",
            str(candidate),
            "-boot_image",
            "any",
            "replay",
            "-overwrite",
            "on",
            "-map",
            str(seed),
            "/nocloud",
            "-map",
            str(grub),
            "/boot/grub/grub.cfg",
        ]
        if has_loopback:
            command.extend(
                ("-map", str(loopback), "/boot/grub/loopback.cfg")
            )
        if has_md5_manifest:
            command.extend(("-map", str(md5_manifest), "/md5sum.txt"))
        command.extend(("-commit", "-end"))
        _run(command, description="building custom autoinstall ISO", runner=runner)
        if not candidate.is_file() or candidate.stat().st_size == 0:
            raise BuildError("xorriso did not create a non-empty output ISO")
        candidate.chmod(0o600)
        os.replace(candidate, output)
        output.chmod(0o600)
    finally:
        shutil.rmtree(temporary_root, ignore_errors=True)


def read_request(path_value: str) -> dict[str, Any]:
    try:
        if path_value == "-":
            value = json.load(sys.stdin)
        else:
            path = Path(path_value)
            if path.is_symlink() or not path.is_file():
                raise BuildError("request must be a regular, non-symlink JSON file")
            value = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as exc:
        raise BuildError(f"cannot read request JSON: {exc}") from exc
    return validate_request(value)


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(
        description=(
            "Verify and customize the configured Ubuntu live-server ISO "
            "for one production VM."
        )
    )
    parser.add_argument("--source-iso", type=Path, required=True)
    parser.add_argument("--expected-sha256", required=True)
    parser.add_argument("--output-iso", type=Path, required=True)
    parser.add_argument(
        "--request",
        default="-",
        help="secret-free request JSON path, or - for stdin (default: -)",
    )
    parser.add_argument("--xorriso", type=Path, default=Path("/usr/bin/xorriso"))
    return parser


def main(argv: Sequence[str] | None = None) -> int:
    os.umask(0o077)
    parser = build_parser()
    args = parser.parse_args(argv)
    try:
        build_iso(
            source=Path(os.path.abspath(args.source_iso)),
            expected_sha256=args.expected_sha256,
            output=Path(os.path.abspath(args.output_iso)),
            request=read_request(args.request),
            xorriso=Path(os.path.abspath(args.xorriso)),
        )
        print(f"Created verified Ubuntu autoinstall ISO: {args.output_iso}")
        return 0
    except BuildError as exc:
        print(f"ERROR: {exc}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
