# Copyright (c) 2026 BEENTHERE VENTURES, INC.
# SPDX-License-Identifier: GPL-3.0-only

"""The lab topology: VMs, their disks and NICs, and every derived identity."""

from __future__ import annotations

import hashlib
import ipaddress
import re
import tomllib
from dataclasses import dataclass, field
from pathlib import Path

BUSES = ("nvme", "sata", "scsi", "virtio")
NETWORKS = ("uplink", "mgmt", "vlan")
ROLES = ("host", "appliance", "qdevice")
IMAGES = ("proxmox", "ubuntu")
# q35's AHCI controller has six ports; the lab keeps 0 and 1 for CD drives.
SATA_PORTS = 4
NAME = re.compile(r"^[a-z][a-z0-9-]{0,14}$")
# Ports forwarded from 127.0.0.1:<base + 10 * index + offset> to each VM.
FORWARDS = {"ssh": (2, 22), "pve": (6, 8006), "web": (3, 8443), "peer": (4, 8444), "source": (5, 8445)}
# QEMU's own OUI, used for MACs, WWNs, and EUI-64s alike.
OUI = "525400"


class TopologyError(ValueError):
    pass


@dataclass(frozen=True)
class Disk:
    vm: str
    bus: str
    index: int  # among this VM's disks on the same bus
    size_gib: int
    serial: str
    # 0x-prefixed 64-bit NAA 5 for SATA and SCSI, EUI-64 for NVMe, "" for virtio.
    # Linux ignores the NVMe EUI-64 (NVME_QUIRK_BOGUS_NID on QEMU's controller),
    # so guests identify the lab's NVMe disks by serial.
    wwn: str

    @property
    def id(self) -> str:
        return f"{self.bus}{self.index}"

    @property
    def file(self) -> str:
        return f"{self.id}.qcow2"


@dataclass(frozen=True)
class Nic:
    network: str
    mac: str

    @property
    def id(self) -> str:
        return f"nic-{self.network}"


@dataclass(frozen=True)
class Vm:
    name: str
    role: str
    index: int
    image: str
    cpus: int
    memory_gib: int
    disks: tuple[Disk, ...]
    nics: tuple[Nic, ...]
    mgmt_ip: str
    vlan_ip: str | None
    carrier_delay_seconds: int
    ports: dict[str, int] = field(hash=False)

    def nic(self, network: str) -> Nic | None:
        return next((n for n in self.nics if n.network == network), None)


@dataclass(frozen=True)
class Topology:
    name: str
    domain: str
    root_password: str
    base_port: int
    mcast_group: str
    mcast_ports: dict[str, int]
    networks: dict[str, ipaddress.IPv4Network]
    images: dict[str, dict[str, str]]
    tools: dict[str, dict[str, str]]
    vms: tuple[Vm, ...]
    source: Path
    tailnet_prefix: str
    reserved_tailnet_names: frozenset[str]
    # The NVMe disks are attached as SCSI disks with the same serials, so
    # QEMU can save the VMs live (it can't save a running NVMe controller).
    nvme_as_scsi: bool = False

    def tailnet_name(self, vm: Vm) -> str:
        """The only name a lab VM may use on a real tailnet."""
        return f"{self.tailnet_prefix}{vm.name}"

    def vm(self, name: str) -> Vm:
        for vm in self.vms:
            if vm.name == name:
                return vm
        raise TopologyError(f"no VM named {name!r}; the lab has {', '.join(v.name for v in self.vms)}")

    def select(self, names: list[str] | None) -> list[Vm]:
        return [self.vm(n) for n in names] if names else list(self.vms)

    @property
    def hosts(self) -> list[Vm]:
        return [vm for vm in self.vms if vm.role == "host"]


def disk_serial(lab: str, vm: str, bus: str, index: int) -> str:
    """Readable, unique within the lab, and at most 20 characters (the ATA and NVMe limit)."""
    tag = hashlib.sha256(f"{lab}/{vm}/{bus}{index}".encode()).hexdigest()[:4].upper()
    serial = f"{vm.upper()}{bus[0].upper()}{index}{tag}"
    return serial[:20]


def disk_wwn(lab: str, vm: str, bus: str, index: int) -> str:
    digest = hashlib.sha256(f"wwn:{lab}/{vm}/{bus}{index}".encode()).hexdigest()
    if bus in ("sata", "scsi"):
        return f"0x5{OUI}{digest[:9]}"
    if bus == "nvme":
        return f"0x{OUI}{digest[:10]}"
    return ""


def _int(table: dict, key: str, where: str, low: int, high: int) -> int:
    value = table.get(key)
    if not isinstance(value, int) or isinstance(value, bool) or not low <= value <= high:
        raise TopologyError(f"{where}: {key} must be an integer from {low} to {high}")
    return value


def _str(table: dict, key: str, where: str, default: str | None = None) -> str:
    value = table.get(key, default)
    if not isinstance(value, str) or not value:
        raise TopologyError(f"{where}: {key} must be a non-empty string")
    return value


def _address(value: object, network: ipaddress.IPv4Network, where: str) -> str:
    try:
        address = ipaddress.IPv4Address(value)  # type: ignore[arg-type]
    except ValueError as error:
        raise TopologyError(f"{where}: {value!r} is not an IPv4 address") from error
    if address not in network or address in (network.network_address, network.broadcast_address):
        raise TopologyError(f"{where}: {address} is not a usable address in {network}")
    return str(address)


def parse(data: dict, source: Path) -> Topology:
    lab = data.get("lab", {})
    name = _str(lab, "name", "[lab]")
    if not NAME.match(name):
        raise TopologyError("[lab]: name must be lowercase letters, digits, and hyphens, at most 15 characters")
    base_port = _int(lab, "base_port", "[lab]", 1024, 64000)
    mcast_base = _int(lab, "mcast_base_port", "[lab]", 1024, 65000)
    mcast_group = _str(lab, "mcast_group", "[lab]")
    if not ipaddress.IPv4Address(mcast_group).is_multicast:
        raise TopologyError("[lab]: mcast_group must be an IPv4 multicast address")
    mac_prefix = _str(lab, "mac_prefix", "[lab]")
    if not re.fullmatch(r"(?:[0-9a-f]{2}:){3}[0-9a-f]{2}", mac_prefix):
        raise TopologyError("[lab]: mac_prefix must be four lowercase hex octets, like 52:54:00:b3")

    networks = {}
    for net in ("mgmt", "vlan"):
        cidr = data.get("networks", {}).get(net, {}).get("cidr")
        try:
            networks[net] = ipaddress.IPv4Network(cidr)
        except (TypeError, ValueError) as error:
            raise TopologyError(f"[networks.{net}]: cidr must be an IPv4 network") from error

    images = {}
    for image in IMAGES:
        table = data.get("images", {}).get(image, {})
        url = _str(table, "url", f"[images.{image}]")
        sha = table.get("sha256", "")
        if not isinstance(sha, str) or (sha and not re.fullmatch(r"[0-9a-f]{64}", sha)):
            raise TopologyError(f"[images.{image}]: sha256 must be 64 lowercase hex digits or empty")
        images[image] = {"url": url, "sha256": sha}
    if not images["proxmox"]["sha256"]:
        raise TopologyError("[images.proxmox]: sha256 is required")
    assistant = data.get("tools", {}).get("auto_install_assistant", {})
    tools = {
        "auto_install_assistant": {
            "url": _str(assistant, "url", "[tools.auto_install_assistant]"),
            "sha256": _str(assistant, "sha256", "[tools.auto_install_assistant]"),
            "binary": _str(assistant, "binary", "[tools.auto_install_assistant]"),
        }
    }

    roles = data.get("roles", {})
    entries = data.get("vm", [])
    if not entries:
        raise TopologyError("the topology defines no [[vm]]")
    vms: list[Vm] = []
    seen_ips: dict[str, str] = {}
    for index, entry in enumerate(entries):
        where = f"[[vm]] #{index + 1}"
        vm_name = _str(entry, "name", where)
        where = f"[[vm]] {vm_name}"
        if not NAME.match(vm_name):
            raise TopologyError(f"{where}: name must be lowercase letters, digits, and hyphens, at most 15 characters")
        if any(v.name == vm_name for v in vms):
            raise TopologyError(f"{where}: duplicate VM name")
        role = _str(entry, "role", where)
        if role not in ROLES or role not in roles:
            raise TopologyError(f"{where}: role must be one of the [roles.*] tables: {', '.join(r for r in ROLES if r in roles)}")
        spec = {**roles[role], **{k: v for k, v in entry.items() if k in ("cpus", "memory_gib", "disks", "networks")}}
        rwhere = f"[roles.{role}]"
        image = _str(spec, "image", rwhere)
        if image not in IMAGES:
            raise TopologyError(f"{rwhere}: image must be one of {', '.join(IMAGES)}")
        cpus = _int(spec, "cpus", rwhere, 1, 64)
        memory = _int(spec, "memory_gib", rwhere, 1, 256)

        nets = spec.get("networks", [])
        if not isinstance(nets, list) or not nets or nets[0] != "uplink" or len(set(nets)) != len(nets) or any(n not in NETWORKS for n in nets):
            raise TopologyError(f"{rwhere}: networks must start with uplink and list each of {', '.join(NETWORKS)} at most once")
        nics = tuple(Nic(net, f"{mac_prefix}:{index + 1:02x}:{n:02x}") for n, net in enumerate(nets))

        mgmt_ip = vlan_ip = None
        if "mgmt" in nets:
            mgmt_ip = _address(entry.get("mgmt_ip"), networks["mgmt"], f"{where}: mgmt_ip")
        if "vlan" in nets:
            vlan_ip = _address(entry.get("vlan_ip"), networks["vlan"], f"{where}: vlan_ip")
        elif "vlan_ip" in entry:
            raise TopologyError(f"{where}: vlan_ip is set but the {role} role has no vlan network")
        for address in filter(None, (mgmt_ip, vlan_ip)):
            if address in seen_ips:
                raise TopologyError(f"{where}: {address} is also used by {seen_ips[address]}")
            seen_ips[address] = vm_name

        delay = entry.get("carrier_delay_seconds", 0)
        if not isinstance(delay, int) or isinstance(delay, bool) or not 0 <= delay <= 3600:
            raise TopologyError(f"{where}: carrier_delay_seconds must be an integer from 0 to 3600")
        if delay and "vlan" not in nets:
            raise TopologyError(f"{where}: carrier_delay_seconds needs a vlan network")

        disk_specs = spec.get("disks", [])
        if not isinstance(disk_specs, list) or not disk_specs:
            raise TopologyError(f"{rwhere}: disks must list at least one disk")
        counts = dict.fromkeys(BUSES, 0)
        disks = []
        for d in disk_specs:
            bus = d.get("bus") if isinstance(d, dict) else None
            if bus not in BUSES:
                raise TopologyError(f"{rwhere}: each disk's bus must be one of {', '.join(BUSES)}")
            size = _int(d, "size_gib", f"{rwhere} {bus} disk", 1, 4096)
            n = counts[bus]
            counts[bus] += 1
            disks.append(Disk(vm_name, bus, n, size, disk_serial(name, vm_name, bus, n), disk_wwn(name, vm_name, bus, n)))
        if counts["sata"] > SATA_PORTS:
            raise TopologyError(f"{rwhere}: at most {SATA_PORTS} SATA disks fit on the AHCI controller")
        if image == "ubuntu" and disks[0].bus != "virtio":
            raise TopologyError(f"{rwhere}: an ubuntu VM's first disk holds the cloud image and must be virtio")

        ports = {k: base_port + 10 * index + offset for k, (offset, _) in FORWARDS.items()}
        vms.append(Vm(vm_name, role, index, image, cpus, memory, tuple(disks), nics, mgmt_ip or "", vlan_ip, delay, ports))

    serials = [d.serial for vm in vms for d in vm.disks]
    if len(set(serials)) != len(serials):
        raise TopologyError("two disks derived the same serial; rename a VM")
    if base_port + 10 * len(vms) > 65535:
        raise TopologyError("[lab]: base_port leaves no room for every VM's forwarded ports")

    tailnet_prefix = _str(lab, "tailnet_prefix", "[lab]")
    if not re.fullmatch(r"[a-z][a-z0-9-]*-", tailnet_prefix):
        raise TopologyError("[lab]: tailnet_prefix must be lowercase letters, digits, and hyphens, ending in a hyphen")
    reserved = lab.get("reserved_tailnet_names", [])
    if not isinstance(reserved, list) or not all(isinstance(r, str) and r for r in reserved):
        raise TopologyError("[lab]: reserved_tailnet_names must be a list of names")
    reserved_names = frozenset(r.lower() for r in reserved)
    nvme_as_scsi = lab.get("nvme_as_scsi", False)
    if not isinstance(nvme_as_scsi, bool):
        raise TopologyError("[lab]: nvme_as_scsi must be true or false")
    for vm in vms:
        tailnet_name = f"{tailnet_prefix}{vm.name}"
        if tailnet_name in reserved_names:
            raise TopologyError(f"[[vm]] {vm.name}: tailnet name {tailnet_name} is reserved by [lab] reserved_tailnet_names")
        if len(tailnet_name) > 63:
            raise TopologyError(f"[[vm]] {vm.name}: tailnet name {tailnet_name} is longer than 63 characters")

    return Topology(
        name=name,
        domain=_str(lab, "domain", "[lab]"),
        root_password=_str(lab, "root_password", "[lab]"),
        base_port=base_port,
        mcast_group=mcast_group,
        mcast_ports={"mgmt": mcast_base, "vlan": mcast_base + 1},
        networks=networks,
        images=images,
        tools=tools,
        vms=tuple(vms),
        source=source,
        tailnet_prefix=tailnet_prefix,
        reserved_tailnet_names=reserved_names,
        nvme_as_scsi=nvme_as_scsi,
    )


def _merged(base: dict, overlay: dict) -> dict:
    merged = dict(base)
    for key, value in overlay.items():
        merged[key] = _merged(base[key], value) if isinstance(value, dict) and isinstance(base.get(key), dict) else value
    return merged


def load(path: Path) -> Topology:
    """Reads a topology. One whose [lab] names a `base` topology (relative
    to it) is that topology with its own tables merged over it."""
    def read(file: Path) -> dict:
        try:
            return tomllib.loads(file.read_text(encoding="utf-8"))
        except FileNotFoundError as error:
            raise TopologyError(f"{file} does not exist") from error
        except tomllib.TOMLDecodeError as error:
            raise TopologyError(f"{file}: {error}") from error

    data = read(path)
    base = data.get("lab", {}).pop("base", None)
    if base is not None:
        if not isinstance(base, str) or not base:
            raise TopologyError("[lab]: base must name a topology file")
        under = read(path.parent / base)
        if "base" in under.get("lab", {}):
            raise TopologyError(f"{base}: a base topology can't have a base of its own")
        data = _merged(under, data)
    return parse(data, path)
