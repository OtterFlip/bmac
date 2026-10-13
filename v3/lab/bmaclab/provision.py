# Copyright (c) 2026 BEENTHERE VENTURES, INC.
# SPDX-License-Identifier: GPL-3.0-only

"""Downloads, the Proxmox auto-install ISO, and cloud-init seed images."""

from __future__ import annotations

import hashlib
import json
import shutil
import subprocess
import tempfile
import urllib.request
from pathlib import Path, PurePosixPath
from urllib.parse import urlsplit

from .paths import Paths
from .topology import Topology, Vm


class ProvisionError(RuntimeError):
    pass


def run(args: list[str], **kwargs) -> subprocess.CompletedProcess:
    try:
        return subprocess.run(args, check=True, text=True, capture_output=True, **kwargs)
    except FileNotFoundError as error:
        raise ProvisionError(f"{args[0]} is not installed") from error
    except subprocess.CalledProcessError as error:
        detail = (error.stderr or error.stdout or "").strip()
        raise ProvisionError(f"{' '.join(args[:2])} failed: {detail}") from error


def sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        for block in iter(lambda: stream.read(1 << 20), b""):
            digest.update(block)
    return digest.hexdigest()


def url_name(url: str) -> str:
    name = PurePosixPath(urlsplit(url).path).name
    if not name:
        raise ProvisionError(f"{url} does not end in a file name")
    return name


def published_sha256(url: str, opener=urllib.request.urlopen) -> str:
    """Looks the file up in the SHA256SUMS published next to it."""
    sums_url = url.rsplit("/", 1)[0] + "/SHA256SUMS"
    with opener(sums_url, timeout=60) as response:
        text = response.read().decode()
    name = url_name(url)
    for line in text.splitlines():
        parts = line.split()
        if len(parts) == 2 and parts[1].lstrip("*") == name:
            return parts[0].lower()
    raise ProvisionError(f"{sums_url} does not list {name}")


def fetch(url: str, sha256: str, folder: Path, log=print, opener=urllib.request.urlopen) -> Path:
    """Returns the cached file when it matches, otherwise downloads and verifies it."""
    if not url.startswith("https://") and not sha256:
        raise ProvisionError(f"{url} is not HTTPS, so it needs a pinned sha256")
    expected = sha256 or published_sha256(url, opener)
    folder.mkdir(parents=True, exist_ok=True)
    target = folder / url_name(url)
    if target.exists() and sha256_file(target) == expected:
        return target
    log(f"Downloading {url}")
    partial = target.with_name(target.name + ".partial")
    with opener(url, timeout=60) as response, partial.open("wb") as out:
        shutil.copyfileobj(response, out, 1 << 20)
    actual = sha256_file(partial)
    if actual != expected:
        partial.unlink()
        raise ProvisionError(f"{target.name}: SHA-256 mismatch: expected {expected}, got {actual}")
    partial.replace(target)
    return target


def assistant(topology: Topology, paths: Paths, log=print, opener=urllib.request.urlopen) -> Path:
    tool = topology.tools["auto_install_assistant"]
    deb = fetch(tool["url"], tool["sha256"], paths.tools, log, opener)
    folder = paths.tools / deb.stem
    binary = folder / tool["binary"]
    if not binary.is_file():
        shutil.rmtree(folder, ignore_errors=True)
        run(["dpkg-deb", "-x", str(deb), str(folder)])
    if not binary.is_file():
        raise ProvisionError(f"{deb.name} has no {tool['binary']}")
    return binary


def password_hash(topology: Topology) -> str:
    salt = hashlib.sha256(f"bmac-lab:{topology.name}".encode()).hexdigest()[:16]
    hashed = run(["openssl", "passwd", "-6", "-salt", salt, "-stdin"], input=topology.root_password).stdout.strip()
    if not hashed.startswith("$6$"):
        raise ProvisionError("openssl did not return a SHA-512 crypt hash")
    return hashed


def _toml(value: str) -> str:
    return json.dumps(value)


def answer_toml(topology: Topology, vm: Vm, hashed_password: str, public_key: str) -> str:
    """Proxmox automated installation answer file (pve.proxmox.com/wiki/Automated_Installation)."""
    nvme = [d for d in vm.disks if d.bus == "nvme"][:2]
    if len(nvme) != 2:
        raise ProvisionError(f"{vm.name} needs two NVMe disks for the rpool mirror")
    uplink = vm.nic("uplink")
    assert uplink is not None
    lines = [
        "[global]",
        'keyboard = "en-us"',
        'country = "us"',
        f"fqdn = {_toml(f'{vm.name}.{topology.domain}')}",
        f"mailto = {_toml(f'root@{topology.domain}')}",
        'timezone = "UTC"',
        f"root-password-hashed = {_toml(hashed_password)}",
        f"root-ssh-keys = [{_toml(public_key)}]",
        "reboot-on-error = false",
        'reboot-mode = "power-off"',
        "",
        "[network]",
        'source = "from-answer"',
        # QEMU user networking always gives the guest these addresses.
        'cidr = "10.0.2.15/24"',
        'dns = "10.0.2.3"',
        'gateway = "10.0.2.2"',
        f"filter.ID_NET_NAME_MAC = {_toml('*' + uplink.mac.replace(':', ''))}",
        "",
        "[disk-setup]",
        'filesystem = "zfs"',
        'filter-match = "any"',
        *([
            # A SCSI disk with a WWN reports it as ID_SERIAL; its serial is
            # ID_SCSI_SERIAL, and only the boot disks' start with <VM>N.
            f"filter.ID_SCSI_SERIAL = {_toml(nvme[0].serial[:len(vm.name) + 1] + '*')}",
        ] if topology.nvme_as_scsi else [
            f"filter.ID_SERIAL_SHORT = {_toml(nvme[0].serial)}",
            f"filter.ID_SERIAL = {_toml('*' + nvme[1].serial + '*')}",
        ]),
        'zfs.raid = "raid1"',
        "",
        "[first-boot]",
        'source = "from-iso"',
        'ordering = "before-network"',
        "",
    ]
    return "\n".join(lines)


def first_boot_script(topology: Topology, vm: Vm) -> str:
    """Addresses the management and VLAN NICs by MAC and turns on the serial console."""
    stanzas = []
    for network, address in (("mgmt", vm.mgmt_ip), ("vlan", vm.vlan_ip)):
        nic = vm.nic(network)
        if nic and address:
            prefix = topology.networks[network].prefixlen
            stanzas.append(f"configure {nic.mac} {address}/{prefix}")
    body = "\n".join(stanzas)
    return f"""#!/usr/bin/env bash
# Copyright (c) 2026 BEENTHERE VENTURES, INC.
# SPDX-License-Identifier: GPL-3.0-only
# Written by the BMAC lab; runs once on the first boot of {vm.name}.
set -Eeuo pipefail

configure() {{
  local mac=$1 address=$2 name
  name="$(ip -o link | awk -v mac="$mac" '$0 ~ "link/ether " mac {{ sub(":$", "", $2); print $2; exit }}')"
  [[ -n "$name" ]] || {{ printf 'No NIC with MAC %s\\n' "$mac" >&2; exit 1; }}
  printf '\\nauto %s\\niface %s inet static\\n\\taddress %s\\n' "$name" "$name" "$address" >>/etc/network/interfaces
}}

{body}

if [[ -f /etc/kernel/cmdline ]] && ! grep -q 'console=ttyS0' /etc/kernel/cmdline; then
  sed -i 's/$/ console=tty0 console=ttyS0,115200/' /etc/kernel/cmdline
  proxmox-boot-tool refresh
fi
if [[ -f /etc/default/grub ]] && ! grep -q 'console=ttyS0' /etc/default/grub; then
  sed -i 's/^GRUB_CMDLINE_LINUX="\\(.*\\)"/GRUB_CMDLINE_LINUX="\\1 console=tty0 console=ttyS0,115200"/' /etc/default/grub
  update-grub || true
fi
systemctl enable serial-getty@ttyS0.service
"""


def build_install_iso(topology: Topology, vm: Vm, paths: Paths, source_iso: Path, assistant_binary: Path, hashed_password: str, public_key: str) -> Path:
    folder = paths.vm(vm.name)
    answer = folder / "answer.toml"
    first_boot = folder / "first-boot.sh"
    answer.write_text(answer_toml(topology, vm, hashed_password, public_key), encoding="utf-8")
    first_boot.write_text(first_boot_script(topology, vm), encoding="utf-8")
    first_boot.chmod(0o755)
    output = paths.install_iso(vm.name)
    output.unlink(missing_ok=True)
    run([str(assistant_binary), "validate-answer", str(answer)])
    run([
        str(assistant_binary), "prepare-iso", str(source_iso),
        "--fetch-from", "iso",
        "--answer-file", str(answer),
        "--on-first-boot", str(first_boot),
        "--output", str(output),
    ])
    return output


def cloud_init(topology: Topology, vm: Vm, hashed_password: str, public_key: str) -> dict[str, str]:
    """NoCloud files. JSON is valid YAML, so no YAML library is needed."""
    user = {
        "hostname": vm.name,
        "fqdn": f"{vm.name}.{topology.domain}",
        "disable_root": False,
        "ssh_pwauth": False,
        "users": [{"name": "root", "lock_passwd": False, "hashed_passwd": hashed_password, "ssh_authorized_keys": [public_key]}],
        "growpart": {"mode": "auto", "devices": ["/"]},
    }
    ethernets: dict[str, dict] = {}
    for nic in vm.nics:
        entry: dict = {"match": {"macaddress": nic.mac}}
        if nic.network == "uplink":
            entry["dhcp4"] = True
        else:
            address = vm.mgmt_ip if nic.network == "mgmt" else vm.vlan_ip
            entry["addresses"] = [f"{address}/{topology.networks[nic.network].prefixlen}"]
        ethernets[nic.network] = entry
    return {
        "user-data": "#cloud-config\n" + json.dumps(user, indent=2) + "\n",
        "meta-data": json.dumps({"instance-id": f"{topology.name}-{vm.name}", "local-hostname": vm.name}) + "\n",
        "network-config": json.dumps({"version": 2, "ethernets": ethernets}, indent=2) + "\n",
    }


def build_seed(topology: Topology, vm: Vm, paths: Paths, hashed_password: str, public_key: str) -> Path:
    output = paths.seed(vm.name)
    with tempfile.TemporaryDirectory(prefix="bmac-lab-seed-") as scratch:
        names = []
        for name, text in cloud_init(topology, vm, hashed_password, public_key).items():
            Path(scratch, name).write_text(text, encoding="utf-8")
            names.append(name)
        output.unlink(missing_ok=True)
        run(["xorriso", "-as", "mkisofs", "-quiet", "-output", str(output), "-volid", "cidata", "-joliet", "-rock", *names], cwd=scratch)
    return output
