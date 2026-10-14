# Copyright (c) 2026 BEENTHERE VENTURES, INC.
# SPDX-License-Identifier: GPL-3.0-only

"""The interim lab bootstrap (IMPLEMENTATION_PLAN.md, M2.13): turns the lab's
freshly installed Proxmox hosts into a cluster running a development build of
bmacd, so M2 to M5 can be tried on real hosts before the Deployer (M7) and the
setup job (M6) exist.

It does what v2's add_proxmox_host.sh does that bmacd relies on, the way v2
does it: the same `pvecm create` and `pvecm add` with the private VLAN as
link0 and the management network (the lab's stand-in for the tailnet) as
link1, the migration network, v2's registry initialized with v2's own
cluster_registry.py (in bmac's registry folder), and the v2 config files,
loaded into bmacd's cluster settings. In place of a real tailnet, every lab
VM gets a `tailscale` stand-in that answers `status --json` and `ip -4` from
the management network, and the tailnet names (`bmaclab-qdevice`) in
/etc/hosts. It skips what needs iDRAC or bare metal. Nothing here is
shipped, and every step can be rerun.
"""

from __future__ import annotations

import base64
import hashlib
import hmac
import io
import json
import secrets
import shlex
import struct
import subprocess
import tarfile
import tempfile
import time
from pathlib import Path

from .topology import Topology, Vm

V3 = Path(__file__).resolve().parents[2]
CLUSTER_NAME = "bmaclab"
REGISTRY = "/etc/pve/priv/bmac/registry"
INSTALL_ROOT = "/usr/lib/bmac"
BUILD_DIR = "/root/bmac-src"
# The workspace folders the build needs; everything else stays on the workstation.
# The Deployer's Tauri crate is a workspace member, so cargo needs it to load
# the workspace even though the hosts never build it.
SOURCE = ("Cargo.toml", "Cargo.lock", "crates", "deployer/src-tauri", "legacy", "notify/templates", "webui/dist")
# Lab-only accounts. alice also gets TOTP and recovery keys.
USERS = (
    ("alice@pve", "bmac-admin", "alice-lab-password"),
    ("bob@pve", "bmac-operator", "bob-lab-password"),
    ("vera@pve", "bmac-viewer", "vera-lab-password"),
)
GROUPS = ("bmac-admin", "bmac-operator", "bmac-viewer")
BUILD_TIMEOUT = 60 * 60


class BootstrapError(RuntimeError):
    pass


def host_number(vm: Vm) -> int:
    if not vm.name.startswith("mox") or not vm.name[3:].isdigit():
        raise BootstrapError(f"{vm.name} is not a BMAC host name (mox1 to mox32)")
    return int(vm.name[3:])


def _conf(values: list[tuple[str, str]]) -> str:
    lines = ["# Written by the BMAC lab bootstrap for bmacd import-settings."]
    lines += [f"{key}='{value}'" for key, value in values]
    return "\n".join(lines) + "\n"


def conf_files(topology: Topology) -> dict[str, str]:
    """v2's cluster.conf and moxN.conf for the lab, following v2's example plan."""
    hosts = topology.hosts
    vlan = topology.networks["vlan"]
    prefix = str(vlan.network_address).rsplit(".", 1)[0]
    qdevices = [vm for vm in topology.vms if vm.role == "qdevice"]
    cluster = [
        ("PROXMOX_CLUSTER_NAME", CLUSTER_NAME),
        ("PROXMOX_QDEVICE_HOST", topology.tailnet_name(qdevices[0]) if qdevices else "qdevice"),
        ("PROXMOX_INTERNAL_DOMAIN", topology.domain),
        ("MAX_MOX_HOSTS", str(len(hosts))),
        ("PRIVATE_SUBNET_CIDR", str(vlan)),
        ("PROXMOX_MIGRATION_NETWORK", f"{prefix}.240/28"),
        ("DATACENTER_PRIVATE_VLAN_GATEWAY", f"{prefix}.1"),
        ("GUEST_EGRESS_VIP", f"{prefix}.10/24"),
        ("MOX_IP_START", f"{prefix}.11"),
        ("MOX_IP_END", f"{prefix}.20"),
        ("MOX_REPLICATION_IP_START", f"{prefix}.241"),
        ("MOX_REPLICATION_IP_END", f"{prefix}.250"),
        ("HAPROXY_IP_START", f"{prefix}.21"),
        ("HAPROXY_IP_END", f"{prefix}.30"),
        ("PRODUCTION_IP_START", f"{prefix}.31"),
        ("PRODUCTION_IP_END", f"{prefix}.50"),
        ("STAGING_IP_START", f"{prefix}.51"),
        ("STAGING_IP_END", f"{prefix}.200"),
        ("PROXMOX_PRIVATE_BRIDGE", "vmbr-private"),
        ("PROD_VM_CORES", "2"),
        ("PROD_VM_MEMORY_GIB", "2"),
        ("PROD_VM_DISK_GB", "8"),
        ("PROD_VM_BRIDGE", "vmbr-private"),
        ("PROD_VM_CPU_TYPE", "x86-64-v2-AES"),
        ("PROD_VM_REPLICATION_INTERVAL", "*/15"),
        ("STAGING_VM_CORES", "1"),
        ("STAGING_VM_MEMORY_GIB", "1"),
        ("STAGING_VM_BRIDGE", "vmbr-private"),
        ("ISO_STORAGE_ID", "local"),
        ("SNIPPETS_STORAGE_ID", "local"),
        ("PROXMOX_TIMEZONE", "America/Denver"),
    ]
    files = {"cluster.conf": _conf(cluster)}
    uplink_cidr = "10.0.2.15/24"
    for vm in hosts:
        n = host_number(vm)
        uplink, private = vm.nic("uplink"), vm.nic("vlan")
        if uplink is None or private is None or vm.vlan_ip is None:
            raise BootstrapError(f"{vm.name} needs an uplink and a VLAN NIC")
        expected = f"{prefix}.{10 + n}"
        if vm.vlan_ip != expected:
            raise BootstrapError(f"{vm.name}'s VLAN address must be {expected}, as v2 derives it from the slot")
        facts = [
            ("PROXMOX_IP", uplink_cidr.split("/")[0]),
            ("PROXMOX_GATEWAY", "10.0.2.2"),
            ("PROXMOX_PREFIX", uplink_cidr.split("/")[1]),
            ("PROXMOX_PUBLIC_MAC", uplink.mac.upper()),
            ("PROXMOX_SECONDARY_MAC", private.mac.upper()),
            ("PROXMOX_SECONDARY_IP", f"{vm.vlan_ip}/24"),
            ("MAX_PROD_VM_COUNT_ON_THIS_HOST", "1"),
            ("MAX_STAGING_VM_COUNT_ON_THIS_HOST", "2"),
        ]
        # TAILSCALE_HOSTNAME is left out on purpose: v2 derives the bare slot
        # name, which the v2 test cluster already uses on the real tailnet.
        nvme = [d for d in vm.disks if d.bus == "nvme"][:2]
        for i, disk in enumerate(nvme, start=1):
            facts.append((f"NVME_MIRROR_0_SERIAL_{i}", disk.serial))
            facts.append((f"NVME_MIRROR_0_CAPACITY_BYTES_{i}", str(disk.size_gib << 30)))
        files[f"{vm.name}.conf"] = _conf(facts)
    return files


def hosts_file(topology: Topology) -> str:
    """Every host's name on its private VLAN address, as v2 requires for joins."""
    lines = ["127.0.0.1 localhost.localdomain localhost"]
    for vm in topology.hosts:
        lines.append(f"{vm.vlan_ip} {vm.name}.{topology.domain} {vm.name}")
    lines += [
        "",
        "::1     ip6-localhost ip6-loopback",
        "fe00::0 ip6-localnet",
        "ff00::0 ip6-mcastprefix",
        "ff02::1 ip6-allnodes",
        "ff02::2 ip6-allrouters",
    ]
    return "# Written by the BMAC lab bootstrap.\n" + "\n".join(lines) + "\n"


TAILNET_SUFFIX = "tailnet.invalid"
# Where the tailscale package puts it, since bmacd gives scripts a fixed PATH.
TAILSCALE_STANDIN = "/usr/bin/tailscale"
TAILNET_BEGIN = "# BEGIN bmaclab tailnet names"
TAILNET_END = "# END bmaclab tailnet names"


def tailnet_node(topology: Topology, vm: Vm) -> dict:
    name = topology.tailnet_name(vm)
    return {
        "HostName": name,
        "DNSName": f"{name}.{topology.name}.{TAILNET_SUFFIX}.",
        "TailscaleIPs": [vm.mgmt_ip],
        "Online": True,
        "OS": "linux",
    }


def tailscale_standin(topology: Topology, vm: Vm) -> str:
    """A `tailscale` command for vm that answers from the management network,
    the lab's stand-in for the tailnet: `status --json` and `ip -4` only."""
    status = {
        "BackendState": "Running",
        "TailscaleIPs": [vm.mgmt_ip],
        "Self": tailnet_node(topology, vm),
        "Peer": {f"nodekey:{peer.name}": tailnet_node(topology, peer) for peer in topology.vms if peer is not vm},
        "CurrentTailnet": {
            "Name": topology.name,
            "MagicDNSSuffix": f"{topology.name}.{TAILNET_SUFFIX}",
            "MagicDNSEnabled": True,
        },
    }
    return f"""#!/usr/bin/python3
# Copyright (c) 2026 BEENTHERE VENTURES, INC.
# SPDX-License-Identifier: GPL-3.0-only
# Written by the BMAC lab bootstrap: the lab's stand-in for the tailscale
# command, answering from the management network.
import json
import sys

STATUS = json.loads({json.dumps(json.dumps(status))})
args = sys.argv[1:]
if args == ["status", "--json"]:
    print(json.dumps(STATUS, indent=2))
elif args in (["ip"], ["ip", "-4"]):
    print({vm.mgmt_ip!r})
elif args == ["ip", "-6"]:
    pass
else:
    sys.exit("bmaclab tailscale stand-in: only 'status --json' and 'ip -4' are supported")
"""


def tailnet_hosts_block(topology: Topology) -> str:
    """Every lab VM's tailnet name on its management address, as MagicDNS would resolve it."""
    lines = [TAILNET_BEGIN]
    for vm in topology.vms:
        name = topology.tailnet_name(vm)
        lines.append(f"{vm.mgmt_ip} {name}.{topology.name}.{TAILNET_SUFFIX} {name}")
    lines.append(TAILNET_END)
    return "\n".join(lines) + "\n"


TAILNET_SCRIPT = f"""set -euo pipefail
block="$(cat)"
work="$(mktemp /etc/hosts.bmaclab.XXXXXX)"
awk '$0 == "{TAILNET_BEGIN}" {{ skip = 1; next }} $0 == "{TAILNET_END}" {{ skip = 0; next }} !skip' /etc/hosts >"$work"
printf '%s\\n' "$block" >>"$work"
chmod 0644 "$work"
cat "$work" >/etc/hosts
rm -f "$work"
"""

# v2's add_proxmox_host.sh: the enterprise repositories refuse an unsubscribed host.
REPOSITORIES_SCRIPT = """set -euo pipefail
for source in /etc/apt/sources.list.d/pve-enterprise.sources /etc/apt/sources.list.d/ceph.sources; do
  [[ ! -f "$source" ]] || mv "$source" "${source}.disabled"
done
cat >/etc/apt/sources.list.d/pve-no-subscription.sources <<'EOF'
Types: deb
URIs: http://download.proxmox.com/debian/pve
Suites: trixie
Components: pve-no-subscription
Signed-By: /usr/share/keyrings/proxmox-archive-keyring.gpg
EOF
"""


def replication_ip(topology: Topology, vm: Vm) -> str:
    """v2's MOX_REPLICATION_IP_START plus the slot, on the migration network."""
    prefix = str(topology.networks["vlan"].network_address).rsplit(".", 1)[0]
    return f"{prefix}.{240 + host_number(vm)}"


REPLICATION_SCRIPT = "/usr/local/sbin/bmaclab-replication-ip"
REPLICATION_UNIT = "/etc/systemd/system/bmaclab-replication-ip.service"


def replication_script(topology: Topology, vm: Vm) -> str:
    """Adds the host's replication address to its VLAN NIC, as v2 does on vmbr-private."""
    nic = vm.nic("vlan")
    assert nic is not None
    return f"""#!/bin/bash
# Copyright (c) 2026 BEENTHERE VENTURES, INC.
# SPDX-License-Identifier: GPL-3.0-only
# Written by the BMAC lab bootstrap.
set -euo pipefail
nic="$(ip -o link | awk -v mac={nic.mac} '$0 ~ "link/ether " mac {{ sub(":$", "", $2); print $2; exit }}')"
[[ -n "$nic" ]] || {{ echo "no NIC with MAC {nic.mac}" >&2; exit 1; }}
ip address replace {replication_ip(topology, vm)}/24 dev "$nic"
"""


def replication_unit() -> str:
    return f"""# Written by the BMAC lab bootstrap.
[Unit]
Description=BMAC lab: replication address on the private VLAN
After=network-online.target
Wants=network-online.target
Before=pve-cluster.service

[Service]
Type=oneshot
RemainAfterExit=yes
ExecStart={REPLICATION_SCRIPT}

[Install]
WantedBy=multi-user.target
"""


def totp(secret: str, at: float | None = None, step: int = 30, digits: int = 6) -> str:
    """RFC 6238 with SHA-1, as Proxmox's TOTP uses by default."""
    key = base64.b32decode(secret.upper() + "=" * (-len(secret) % 8))
    counter = int((time.time() if at is None else at) // step)
    digest = hmac.new(key, struct.pack(">Q", counter), hashlib.sha1).digest()
    offset = digest[-1] & 0x0F
    code = (struct.unpack(">I", digest[offset : offset + 4])[0] & 0x7FFFFFFF) % 10**digits
    return f"{code:0{digits}d}"


VENDOR_CONFIG = """\
[source.crates-io]
replace-with = "vendored-sources"

[source.vendored-sources]
directory = "vendor"
"""


def source_tar(root: Path = V3, vendor: Path | None = None) -> bytes:
    """The workspace sources the build needs, without build output, and the vendored crates when given."""
    buffer = io.BytesIO()

    def owned(info: tarfile.TarInfo) -> tarfile.TarInfo:
        info.uid = info.gid = 0
        info.uname = info.gname = "root"
        return info

    def skip(info: tarfile.TarInfo) -> tarfile.TarInfo | None:
        parts = Path(info.name).parts
        if "target" in parts or "node_modules" in parts or "__pycache__" in parts or "gen" in parts:
            return None
        return owned(info)

    with tarfile.open(fileobj=buffer, mode="w:gz") as tar:
        for name in SOURCE:
            path = root / name
            if path.exists():
                tar.add(path, arcname=name, filter=skip)
        if vendor is not None:
            tar.add(vendor, arcname="vendor", filter=owned)
            config = tarfile.TarInfo(".cargo/config.toml")
            config.size = len(VENDOR_CONFIG)
            config.mode = 0o644
            tar.addfile(config, io.BytesIO(VENDOR_CONFIG.encode()))
    return buffer.getvalue()


def payload_tar(binaries: dict[str, bytes], root: Path = V3) -> bytes:
    """What every host gets: the binaries, units, sysusers file, and legacy runtime."""
    units = root / "packaging/systemd"
    buffer = io.BytesIO()

    def add_bytes(tar: tarfile.TarFile, name: str, data: bytes, mode: int) -> None:
        info = tarfile.TarInfo(name)
        info.size, info.mode = len(data), mode
        tar.addfile(info, io.BytesIO(data))

    with tarfile.open(fileobj=buffer, mode="w") as tar:
        for name, data in binaries.items():
            add_bytes(tar, f"bin/{name}", data, 0o755)
        for unit in sorted(path for path in units.rglob("*") if path.is_file()):
            add_bytes(tar, f"systemd/{unit.relative_to(units)}", unit.read_bytes(), 0o644)
        add_bytes(tar, "sysusers/bmacd.conf", (root / "packaging/sysusers.d/bmacd.conf").read_bytes(), 0o644)
        add_bytes(tar, "bmacd.depends", (root / "packaging/bmacd.depends").read_bytes(), 0o644)
        for sub in ("scripts", "protocol"):
            tar.add(root / "legacy" / sub, arcname=f"legacy/{sub}", filter=lambda i: None if "__pycache__" in i.name else i)
        tar.add(root / "crates/bmacd/tests/mock-scripts", arcname="legacy/mock")
    return buffer.getvalue()


INSTALL_SCRIPT = r"""
set -Eeuo pipefail
stage="$(mktemp -d)"
trap 'rm -rf "$stage"' EXIT
tar -C "$stage" -xf -
find "$stage/legacy" -name '*.sh' -print0 | xargs -0 -n1 bash -n
mapfile -t depends < <(grep -Ev '^(#|$)' "$stage/bmacd.depends")
installed="$(dpkg-query -W -f='${{db:Status-Abbrev}}\\n' "${{depends[@]}}" 2>/dev/null | grep -c '^ii' || true)"
if [[ "$installed" -ne "${{#depends[@]}}" ]]; then
  apt-get -o DPkg::Lock::Timeout=60 update -q >/dev/null
  DEBIAN_FRONTEND=noninteractive apt-get -o DPkg::Lock::Timeout=60 install -y -q "${{depends[@]}}" >/dev/null
fi
install -D -m 0755 "$stage/bin/bmacd" {root}/bmacd
install -D -m 0755 "$stage/bin/bmac" /usr/sbin/bmac
install -D -m 0644 -t /usr/lib/systemd/system "$stage"/systemd/*.service "$stage"/systemd/*.timer
install -D -m 0644 -t /usr/lib/systemd/system/bmac-haproxy-ingress.service.d \
  "$stage"/systemd/bmac-haproxy-ingress.service.d/*.conf
install -D -m 0644 "$stage/sysusers/bmacd.conf" /usr/lib/sysusers.d/bmacd.conf
systemd-sysusers /usr/lib/sysusers.d/bmacd.conf
rm -rf {root}/legacy.new
mkdir -p {root}/legacy.new
cp -a "$stage/legacy/." {root}/legacy.new/
chown -R root:root {root}/legacy.new
rm -rf {root}/legacy
mv {root}/legacy.new {root}/legacy
content="$(pvesh get /storage/local --output-format json |
  python3 -c 'import json, sys; print(json.load(sys.stdin).get("content", ""))')"
case ",$content," in
  *,snippets,*) ;;
  *) pvesm set local --content "${{content:+$content,}}snippets" ;;
esac
install -D -m 0755 {root}/legacy/scripts/host_runtime/bmac-guest-role-hook.sh \
  /var/lib/vz/snippets/bmac-guest-role-hook.sh
if [[ ! -s /etc/bmac/tls/https.pem ]]; then
  install -d -m 0700 /etc/bmac /etc/bmac/tls
  openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:P-256 -nodes -days 825 \
    -subj "/CN={fqdn}" -addext "subjectAltName={san}" \
    -keyout /etc/bmac/tls/https.key -out /etc/bmac/tls/https.pem 2>/dev/null
  chmod 0600 /etc/bmac/tls/https.key /etc/bmac/tls/https.pem
fi
systemctl daemon-reload
systemctl enable bmacd.service >/dev/null 2>&1
systemctl restart bmacd.service
systemctl enable --now bmac-deferred-cleanup.timer >/dev/null 2>&1
"""


def install_script(topology: Topology, vm: Vm) -> str:
    fqdn = f"{vm.name}.{topology.domain}"
    san = f"DNS:{fqdn},DNS:{vm.name},IP:{vm.mgmt_ip},IP:{vm.vlan_ip}"
    return INSTALL_SCRIPT.format(root=INSTALL_ROOT, fqdn=fqdn, san=san)


def cluster_values(topology: Topology) -> dict[str, str]:
    lines = conf_files(topology)["cluster.conf"].splitlines()[1:]
    return {key: value.strip("'") for key, value in (line.split("=", 1) for line in lines)}


def registry_init_args(topology: Topology) -> list[str]:
    value = cluster_values(topology)
    return [
        "init",
        "--network", value["PRIVATE_SUBNET_CIDR"],
        "--guest-gateway", f"{value['DATACENTER_PRIVATE_VLAN_GATEWAY']}/24",
        "--mox-ip-start", value["MOX_IP_START"], "--mox-ip-end", value["MOX_IP_END"],
        "--haproxy-ip-start", value["HAPROXY_IP_START"], "--haproxy-ip-end", value["HAPROXY_IP_END"],
        "--production-ip-start", value["PRODUCTION_IP_START"], "--production-ip-end", value["PRODUCTION_IP_END"],
        "--staging-ip-start", value["STAGING_IP_START"], "--staging-ip-end", value["STAGING_IP_END"],
        "--max-hosts", value["MAX_MOX_HOSTS"],
    ]


class Bootstrap:
    def __init__(self, lab) -> None:
        self.lab = lab
        self.topology: Topology = lab.topology
        self.log = lab.log
        self.hosts = self.topology.hosts
        if not self.hosts:
            raise BootstrapError("the lab has no Proxmox hosts")
        for vm in self.hosts:
            host_number(vm)

    @property
    def users_file(self) -> Path:
        return self.lab.paths.root / "users.json"

    # SSH

    def run(self, vm: Vm, script: str, data: bytes | None = None, timeout: float = 600, check: bool = True) -> str:
        """Runs a bash script on vm, with data on its stdin when given."""
        result = subprocess.run(
            self.lab.ssh_args(vm, [f"bash -c {shlex.quote(script)}"]),
            input=data if data is not None else b"",
            capture_output=True,
            timeout=timeout,
        )
        if check and result.returncode != 0:
            detail = (result.stderr or result.stdout).decode(errors="replace").strip()
            raise BootstrapError(f"{vm.name}: {script.strip().splitlines()[0]}... failed ({result.returncode}): {detail}")
        return result.stdout.decode(errors="replace")

    def in_cluster(self, vm: Vm) -> bool:
        return bool(self.run(vm, "pvecm status >/dev/null 2>&1 && echo yes || true").strip())

    # Steps

    def require_hosts(self) -> None:
        for vm in self.hosts:
            if not self.lab.running(vm):
                raise BootstrapError(f"{vm.name} is not running; `lab/lab start` first")
            self.lab.wait_ssh(vm, 300)

    def names(self) -> None:
        text = hosts_file(self.topology)
        for vm in self.hosts:
            self.run(
                vm,
                f"set -e\ninstall -m 0755 /dev/stdin {REPLICATION_SCRIPT}\n"
                f"cat >{REPLICATION_UNIT} <<'UNIT'\n{replication_unit()}UNIT\n"
                "systemctl daemon-reload\nsystemctl enable bmaclab-replication-ip.service >/dev/null 2>&1\n"
                "systemctl restart bmaclab-replication-ip.service\n",
                replication_script(self.topology, vm).encode(),
            )
            if self.in_cluster(vm):
                continue
            self.log(f"{vm.name}: host names on the private VLAN")
            self.run(
                vm,
                "cat >/etc/hosts && systemctl restart pve-cluster && pvecm updatecerts --force >/dev/null "
                "&& systemctl restart pveproxy",
                text.encode(),
            )

    def repositories(self) -> None:
        for vm in self.hosts:
            self.run(vm, REPOSITORIES_SCRIPT)

    def tailnet(self) -> None:
        """The tailscale stand-in and the tailnet names on every running lab VM."""
        block = tailnet_hosts_block(self.topology).encode()
        for vm in self.topology.vms:
            if not self.lab.running(vm):
                continue
            self.lab.wait_ssh(vm, 300)
            self.run(
                vm,
                f"dpkg -S {TAILSCALE_STANDIN} >/dev/null 2>&1 || install -m 0755 /dev/stdin {TAILSCALE_STANDIN}",
                tailscale_standin(self.topology, vm).encode(),
            )
            self.run(vm, TAILNET_SCRIPT, block)

    def cluster(self) -> None:
        first, *others = self.hosts
        if not self.in_cluster(first):
            self.log(f"{first.name}: creating cluster {CLUSTER_NAME}")
            self.run(first, f"pvecm create {CLUSTER_NAME} --link0 address={first.vlan_ip},priority=100 --link1 address={first.mgmt_ip},priority=10")
        fingerprint = self.run(first, "openssl x509 -in /etc/pve/local/pve-ssl.pem -noout -sha256 -fingerprint").strip().split("=", 1)[1]
        for vm in others:
            if self.in_cluster(vm):
                continue
            self.log(f"{vm.name}: joining {first.name}")
            join = (
                f"pvecm add {first.name}.{self.topology.domain} --fingerprint {fingerprint} "
                f"--link0 address={vm.vlan_ip},priority=100 --link1 address={vm.mgmt_ip},priority=10"
            )
            # pvecm reads the root@pam password as one stdin line when stdin is not a terminal.
            output = self.run(vm, f"{join} 2>&1", f"{self.topology.root_password}\n".encode(), timeout=900)
            if not self.in_cluster(vm):
                raise BootstrapError(f"{vm.name} did not join {first.name}:\n{output.strip()}")
        self.wait_quorate(len(self.hosts))
        migration = cluster_values(self.topology)["PROXMOX_MIGRATION_NETWORK"]
        self.run(first, f"""
            config=/etc/pve/datacenter.cfg
            touch "$config"
            awk '!/^migration: / && !/^replication: /' "$config" >/tmp/datacenter.cfg
            printf 'migration: secure,network=%s\\nreplication: secure,network=%s\\n' {migration} {migration} >>/tmp/datacenter.cfg
            cat /tmp/datacenter.cfg >"$config" && rm -f /tmp/datacenter.cfg
        """)

    def wait_quorate(self, count: int, timeout: float = 300) -> None:
        deadline = time.monotonic() + timeout
        while True:
            status = self.run(self.hosts[0], "pvecm status 2>/dev/null || true")
            nodes = next((line.split()[-1] for line in status.splitlines() if line.startswith("Nodes:")), "0")
            if "Quorate:          Yes" in status and nodes == str(count):
                return
            if time.monotonic() > deadline:
                raise BootstrapError(f"the cluster did not reach {count} quorate nodes:\n{status}")
            time.sleep(5)

    def registry(self) -> None:
        first = self.hosts[0]
        if self.run(first, f"test -s {REGISTRY}/policy.json && echo yes || true").strip():
            return
        self.log("Initializing the registry with the legacy cluster_registry.py")
        script = (V3 / "legacy/scripts/host_runtime/cluster_registry.py").read_bytes()
        args = " ".join(shlex.quote(a) for a in registry_init_args(self.topology))
        self.run(first, f"cat >/root/cluster_registry.py && python3 /root/cluster_registry.py --state-dir {REGISTRY} {args} >/dev/null && rm /root/cluster_registry.py", script)

    def build(self) -> dict[str, bytes]:
        """Builds bmacd (with the lab's mock operations) and bmac on the first host, which runs the hosts' Debian."""
        first = self.hosts[0]
        self.log("Building the web app, which bmacd embeds")
        result = subprocess.run(["pnpm", "--dir", str(V3 / "webui"), "build"], capture_output=True, text=True)
        if result.returncode != 0:
            raise BootstrapError(f"the web app build failed:\n{result.stdout.strip()}\n{result.stderr.strip()}")
        self.log(f"{first.name}: preparing the Rust toolchain")
        self.run(first, """
            set -euo pipefail
            if ! command -v cc >/dev/null || ! command -v curl >/dev/null; then
              apt-get -o DPkg::Lock::Timeout=60 update -q >/dev/null
              DEBIAN_FRONTEND=noninteractive apt-get -o DPkg::Lock::Timeout=60 install -y -q build-essential curl >/dev/null
            fi
            if [[ ! -x /root/.cargo/bin/cargo ]]; then
              curl --proto '=https' --tlsv1.2 -sSf -o /root/rustup-init "https://static.rust-lang.org/rustup/dist/$(uname -m)-unknown-linux-gnu/rustup-init"
              chmod +x /root/rustup-init
              /root/rustup-init -y -q --profile minimal --default-toolchain stable >/dev/null
              rm -f /root/rustup-init
            fi
        """, timeout=1800)
        self.log(f"{first.name}: building bmacd and bmac (the first build takes a while)")
        # The crates come from this workstation's cargo cache, so the hosts need no access to crates.io.
        with tempfile.TemporaryDirectory(prefix="bmaclab-vendor-") as scratch:
            vendor = Path(scratch) / "vendor"
            result = subprocess.run(
                ["cargo", "vendor", "--offline", "--locked", "--manifest-path", str(V3 / "Cargo.toml"), str(vendor)],
                capture_output=True, text=True,
            )
            if result.returncode != 0:
                raise BootstrapError(f"cargo vendor failed: {result.stderr.strip()}")
            sources = source_tar(vendor=vendor)
        self.run(first, f"mkdir -p {BUILD_DIR} && find {BUILD_DIR} -mindepth 1 -maxdepth 1 ! -name target -exec rm -rf {{}} + && tar -C {BUILD_DIR} -xzf -", sources)
        self.run(first, f"cd {BUILD_DIR} && /root/.cargo/bin/cargo build -q --release --locked --offline -p bmacd -p bmac-cli --features bmacd/lab", timeout=BUILD_TIMEOUT)
        binaries = {}
        for name in ("bmacd", "bmac"):
            result = subprocess.run(self.lab.ssh_args(first, [f"cat {BUILD_DIR}/target/release/{name}"]), capture_output=True, timeout=300)
            if result.returncode != 0 or not result.stdout.startswith(b"\x7fELF"):
                raise BootstrapError(f"could not fetch {name} from {first.name}")
            binaries[name] = result.stdout
        return binaries

    def install(self, binaries: dict[str, bytes]) -> None:
        payload = payload_tar(binaries)
        for vm in self.hosts:
            self.log(f"{vm.name}: installing bmacd")
            self.run(vm, install_script(self.topology, vm), payload)

    def settings(self) -> None:
        first = self.hosts[0]
        self.log("Importing the cluster settings")
        archive = io.BytesIO()
        with tarfile.open(fileobj=archive, mode="w") as tar:
            for name, text in conf_files(self.topology).items():
                data = text.encode()
                info = tarfile.TarInfo(name)
                info.size, info.mode = len(data), 0o600
                tar.addfile(info, io.BytesIO(data))
        self.run(first, f"""
            dir="$(mktemp -d)"
            trap 'rm -rf "$dir"' EXIT
            tar -C "$dir" -xf -
            {INSTALL_ROOT}/bmacd import-settings "$dir"
        """, archive.getvalue())

    def users(self) -> dict:
        first = self.hosts[0]
        if self.users_file.exists():
            saved = json.loads(self.users_file.read_text())
            if self.run(first, "grep -q '^user:alice@pve:' /etc/pve/user.cfg && echo yes || true").strip():
                return saved
        self.log("Adding the lab's Proxmox users and groups")
        for group in GROUPS:
            self.run(first, f"pveum group list --output-format json | grep -q '\"{group}\"' || pveum group add {group}")
        for user, group, password in USERS:
            self.run(first, f"""
                if ! grep -q '^user:{user}:' /etc/pve/user.cfg; then
                  pvesh create /access/users --userid {user} --groups {group} --password {shlex.quote(password)} >/dev/null
                fi
            """)
        self.run(first, """
            pvesh get /access/tfa/alice@pve --output-format json |
              python3 -c 'import json, sys; print("\\n".join(e["id"] for e in json.load(sys.stdin)))' |
              while read -r id; do [[ -z "$id" ]] || pvesh delete "/access/tfa/alice@pve/$id" >/dev/null; done
        """)
        secret = base64.b32encode(secrets.token_bytes(20)).decode().rstrip("=")
        uri = f"otpauth://totp/{CLUSTER_NAME}:alice@pve?secret={secret}&issuer={CLUSTER_NAME}"
        self.run(first, f"pvesh create /access/tfa/alice@pve --type totp --description lab --totp {shlex.quote(uri)} --value {totp(secret)} >/dev/null")
        recovery = json.loads(self.run(first, "pvesh create /access/tfa/alice@pve --type recovery --description recovery --output-format json"))
        saved = {
            "users": {user: {"group": group, "password": password} for user, group, password in USERS},
            "alice_totp_secret": secret,
            "alice_recovery_keys": recovery.get("recovery", []),
        }
        self.users_file.write_text(json.dumps(saved, indent=2) + "\n")
        self.users_file.chmod(0o600)
        return saved

    def wait_engines(self, timeout: float = 120) -> None:
        for vm in self.hosts:
            deadline = time.monotonic() + timeout
            while self.run(vm, "systemctl is-active bmacd.service bmacd-web.service >/dev/null && test -S /run/bmac/engine.sock && echo yes || true").strip() != "yes":
                if time.monotonic() > deadline:
                    detail = self.run(vm, "journalctl -u bmacd -u bmacd-web -n 30 --no-pager", check=False)
                    raise BootstrapError(f"bmacd did not start on {vm.name}:\n{detail}")
                time.sleep(2)

    def all(self, build: bool = True) -> None:
        """With build=False, reuses the bmacd already installed on every host."""
        self.require_hosts()
        self.repositories()
        self.names()
        self.tailnet()
        self.cluster()
        self.registry()
        if build:
            self.install(self.build())
        self.settings()
        self.users()
        self.wait_engines()
        self.log(f"The lab cluster runs bmacd; the accounts are in {self.users_file}")
