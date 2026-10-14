#!/usr/bin/env python3

# Copyright (c) 2026 BEENTHERE VENTURES, INC.
# SPDX-License-Identifier: GPL-3.0-only

"""Build the development .debs, unsigned: bmacd for the Deployer's host ISOs and
bmac-backupd for backup appliances.

    v3/dev/build_deb.py                    build the web app and binaries, then the package
    v3/dev/build_deb.py --no-build         package the binaries already in target/release
    v3/dev/build_deb.py --out DIR          write bmacd to DIR (default target/deployer-bundle/packages)
    v3/dev/build_deb.py --appliance-out DIR
                                           write bmac-backupd to DIR (default
                                           target/deployer-bundle/appliance-packages)

The default output directory is the one `BMAC_DEPLOYER_BUNDLE=target/deployer-bundle`
points the Deployer at; every package in it goes onto a host's ISO, so the
appliance's package is kept apart. Signed release packages and their bundle are M10's.

A host installs the package at first boot, before its network is up, so
`dpkg -i` must succeed on a fresh Proxmox VE install: the package depends
only on what Proxmox VE itself provides, and the legacy runtime's other
commands (packaging/bmacd.depends) are Recommends for host setup to install.
The appliance runs Ubuntu 24.04 LTS with its own OpenZFS.
"""

from __future__ import annotations

import argparse
import os
import re
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path

V3_ROOT = Path(__file__).resolve().parent.parent
PACKAGE = "bmacd"
APPLIANCE = "bmac-backupd"
# Proxmox VE 9 is Debian trixie; a binary needing a newer glibc wouldn't start.
MAX_GLIBC = (2, 41)
# Ubuntu 24.04 LTS, the appliance's system.
APPLIANCE_MAX_GLIBC = (2, 39)
LIB = "usr/lib/bmac"
HOOK = "legacy/scripts/host_runtime/bmac-guest-role-hook.sh"
SNIPPETS = "/var/lib/vz/snippets"

POSTINST = f"""#!/bin/sh
set -e
if [ "$1" = configure ]; then
  systemd-sysusers /usr/lib/sysusers.d/bmacd.conf
  # The lifecycle hook is referenced as local:snippets/, which is storage
  # rather than a package path, so it is copied there.
  install -D -m 0755 /{LIB}/{HOOK} {SNIPPETS}/bmac-guest-role-hook.sh
  if [ -d /run/systemd/system ]; then
    systemctl daemon-reload
    systemctl enable bmacd.service bmac-deferred-cleanup.timer >/dev/null 2>&1
    # On first install the host's first boot starts bmacd once its files are
    # in place; on reinstall or upgrade it restarts with the new binaries.
    if [ -n "$2" ]; then
      systemctl try-restart bmacd.service || true
    fi
  fi
fi
"""

PRERM = """#!/bin/sh
set -e
if [ "$1" = remove ] && [ -d /run/systemd/system ]; then
  systemctl disable --now bmac-deferred-cleanup.timer bmac-haproxy-route-sync.timer >/dev/null 2>&1 || true
  systemctl disable --now bmacd.service >/dev/null 2>&1 || true
fi
"""

POSTRM = f"""#!/bin/sh
set -e
if [ "$1" = remove ] || [ "$1" = purge ]; then
  rm -f {SNIPPETS}/bmac-guest-role-hook.sh
  if [ -d /run/systemd/system ]; then
    systemctl daemon-reload || true
  fi
fi
if [ "$1" = purge ]; then
  rm -rf /var/lib/bmac /etc/bmac /run/bmac
fi
"""

APPLIANCE_POSTINST = """#!/bin/sh
set -e
if [ "$1" = configure ]; then
  install -d -m 0700 /var/lib/bmac-backupd
  if [ -d /run/systemd/system ]; then
    systemctl daemon-reload
    # It starts once bmac-backupd init has bound a pool.
    systemctl enable bmac-backupd.service >/dev/null 2>&1
    if [ -n "$2" ]; then
      systemctl try-restart bmac-backupd.service || true
    fi
  fi
fi
"""

APPLIANCE_PRERM = """#!/bin/sh
set -e
if [ "$1" = remove ] && [ -d /run/systemd/system ]; then
  systemctl disable --now bmac-backupd.service >/dev/null 2>&1 || true
fi
"""

# The pool and its metadata dataset hold the backups; purging the package
# never touches them, nor the identity they are bound to.
APPLIANCE_POSTRM = """#!/bin/sh
set -e
if [ "$1" = remove ] || [ "$1" = purge ]; then
  if [ -d /run/systemd/system ]; then
    systemctl daemon-reload || true
  fi
fi
"""


def debian_version(version: str) -> str:
    """3.0.0-dev.0 becomes 3.0.0~dev.0, which dpkg sorts before 3.0.0."""
    if not re.fullmatch(r"\d+\.\d+\.\d+(?:-[0-9A-Za-z.]+)?", version):
        raise ValueError(f"{version!r} is not a BMAC version")
    return version.replace("-", "~", 1)


def recommends(root: Path = V3_ROOT) -> list[str]:
    text = (root / "packaging" / "bmacd.depends").read_text(encoding="utf-8")
    return [line.strip() for line in text.splitlines() if line.strip() and not line.startswith("#")]


def glibc_needed(binary: bytes) -> tuple[int, int] | None:
    """The newest GLIBC_x.y symbol version a binary references."""
    found = [(int(a), int(b)) for a, b in re.findall(rb"GLIBC_(\d+)\.(\d+)", binary)]
    return max(found) if found else None


def control(version: str, installed_kib: int, root: Path = V3_ROOT) -> str:
    return (
        f"Package: {PACKAGE}\n"
        f"Version: {debian_version(version)}\n"
        "Architecture: amd64\n"
        "Maintainer: BEENTHERE VENTURES, INC.\n"
        "Homepage: https://github.com/OtterFlip/bmac\n"
        f"Installed-Size: {installed_kib}\n"
        "Section: admin\n"
        "Priority: optional\n"
        "Depends: systemd, pve-manager\n"
        f"Recommends: {', '.join(recommends(root))}\n"
        "Description: BMAC engine and Web Dashboard for Proxmox VE clusters\n"
        " bmacd runs on every Proxmox VE host of a BMAC cluster: the engine, as\n"
        " root, and the HTTPS front end, as bmac-web. This is a development build.\n"
    )


def appliance_control(version: str, installed_kib: int) -> str:
    return (
        f"Package: {APPLIANCE}\n"
        f"Version: {debian_version(version)}\n"
        "Architecture: amd64\n"
        "Maintainer: BEENTHERE VENTURES, INC.\n"
        "Homepage: https://github.com/OtterFlip/bmac\n"
        f"Installed-Size: {installed_kib}\n"
        "Section: admin\n"
        "Priority: optional\n"
        "Depends: systemd, zfsutils-linux, cryptsetup-bin\n"
        "Description: BMAC backup appliance\n"
        " bmac-backupd checks in with the BMAC clusters it is enrolled with and\n"
        " pulls their backups into its own ZFS pool. This is a development build.\n"
    )


def check_glibc(path: Path, data: bytes, ceiling: tuple[int, int], system: str) -> None:
    needed = glibc_needed(data)
    if needed and needed > ceiling:
        raise SystemExit(
            f"{path} needs glibc {needed[0]}.{needed[1]}, newer than {system}'s "
            f"{ceiling[0]}.{ceiling[1]}; build on an older system."
        )


def _write(path: Path, data: bytes | str, mode: int) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_bytes(data.encode() if isinstance(data, str) else data)
    path.chmod(mode)


def stage(tree: Path, binaries: dict[str, Path], version: str, root: Path = V3_ROOT) -> None:
    """Lays out the package's files and its DEBIAN directory under tree."""
    for name, path in binaries.items():
        check_glibc(path, path.read_bytes(), MAX_GLIBC, "Proxmox VE")
    _write(tree / LIB / "bmacd", binaries["bmacd"].read_bytes(), 0o755)
    _write(tree / "usr/sbin/bmac", binaries["bmac"].read_bytes(), 0o755)
    units = root / "packaging" / "systemd"
    for unit in sorted(p for p in units.rglob("*") if p.is_file()):
        _write(tree / "usr/lib/systemd/system" / unit.relative_to(units), unit.read_bytes(), 0o644)
    _write(tree / "usr/lib/sysusers.d/bmacd.conf", (root / "packaging/sysusers.d/bmacd.conf").read_bytes(), 0o644)
    for sub in ("scripts", "protocol"):
        source = root / "legacy" / sub
        for path in sorted(p for p in source.rglob("*") if p.is_file() and "__pycache__" not in p.parts):
            mode = 0o755 if os.access(path, os.X_OK) or path.suffix == ".sh" else 0o644
            _write(tree / LIB / "legacy" / sub / path.relative_to(source), path.read_bytes(), mode)
    doc = tree / "usr/share/doc" / PACKAGE
    _write(doc / "README.md", (root / "packaging" / "README.md").read_bytes(), 0o644)
    _write(
        doc / "copyright",
        "Format: https://www.debian.org/doc/packaging-manuals/copyright-format/1.0/\n"
        "Upstream-Name: BMAC\n\nFiles: *\nCopyright: 2026 BEENTHERE VENTURES, INC.\nLicense: GPL-3.0-only\n",
        0o644,
    )
    size = sum(p.stat().st_size for p in tree.rglob("*") if p.is_file())
    debian = tree / "DEBIAN"
    _write(debian / "control", control(version, (size + 1023) // 1024, root), 0o644)
    for name, text in (("postinst", POSTINST), ("prerm", PRERM), ("postrm", POSTRM)):
        _write(debian / name, text, 0o755)
    for path in sorted(tree.rglob("*"), reverse=True):
        if path.is_dir():
            path.chmod(0o755)


def stage_appliance(tree: Path, binary: Path, version: str, root: Path = V3_ROOT) -> None:
    """Lays out the appliance package under tree."""
    data = binary.read_bytes()
    check_glibc(binary, data, APPLIANCE_MAX_GLIBC, "Ubuntu 24.04")
    _write(tree / "usr/sbin/bmac-backupd", data, 0o755)
    units = root / "packaging" / "appliance" / "systemd"
    for unit in sorted(p for p in units.rglob("*") if p.is_file()):
        _write(tree / "usr/lib/systemd/system" / unit.relative_to(units), unit.read_bytes(), 0o644)
    doc = tree / "usr/share/doc" / APPLIANCE
    _write(doc / "README.md", (root / "packaging" / "README.md").read_bytes(), 0o644)
    size = sum(p.stat().st_size for p in tree.rglob("*") if p.is_file())
    debian = tree / "DEBIAN"
    _write(debian / "control", appliance_control(version, (size + 1023) // 1024), 0o644)
    for name, text in (("postinst", APPLIANCE_POSTINST), ("prerm", APPLIANCE_PRERM), ("postrm", APPLIANCE_POSTRM)):
        _write(debian / name, text, 0o755)
    for path in sorted(tree.rglob("*"), reverse=True):
        if path.is_dir():
            path.chmod(0o755)


def build_deb(tree: Path, out_dir: Path, version: str, dpkg_deb: str = "dpkg-deb", package: str = PACKAGE) -> Path:
    out_dir.mkdir(parents=True, exist_ok=True)
    out = out_dir / f"{package}_{debian_version(version)}_amd64.deb"
    epoch = os.environ.get("SOURCE_DATE_EPOCH")
    if epoch:
        stamp = int(epoch)
        for path in tree.rglob("*"):
            os.utime(path, (stamp, stamp), follow_symlinks=False)
    subprocess.run([dpkg_deb, "--root-owner-group", "-Zxz", "--build", str(tree), str(out)], check=True, stdout=subprocess.DEVNULL)
    return out


def main(argv: list[str]) -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--no-build", action="store_true", help="package the existing release binaries")
    parser.add_argument("--out", type=Path, default=V3_ROOT / "target/deployer-bundle/packages")
    parser.add_argument("--appliance-out", type=Path, default=V3_ROOT / "target/deployer-bundle/appliance-packages")
    args = parser.parse_args(argv)
    if not shutil.which("dpkg-deb"):
        print("dpkg-deb is needed: sudo apt install dpkg", file=sys.stderr)
        return 1
    version = (V3_ROOT / "dev" / "VERSION").read_text(encoding="utf-8").strip()
    if not args.no_build:
        subprocess.run(["pnpm", "--filter", "@bmac/webui", "--filter", "@bmac/applianceui", "build"], cwd=V3_ROOT, check=True)
        subprocess.run(["cargo", "build", "--release", "--locked", "-p", "bmacd", "-p", "bmac-cli", "-p", "bmac-backupd"], cwd=V3_ROOT, check=True)
    for app, server in (("webui", "bmacd"), ("applianceui", "bmac-backupd")):
        if not (V3_ROOT / app / "dist/index.html").is_file():
            print(f"{app}/dist has no build, so {server} would serve a placeholder; build it first", file=sys.stderr)
            return 1
    target = Path(os.environ.get("CARGO_TARGET_DIR", V3_ROOT / "target")) / "release"
    binaries = {"bmacd": target / "bmacd", "bmac": target / "bmac"}
    appliance = target / "bmac-backupd"
    for path in [*binaries.values(), appliance]:
        if not path.is_file():
            print(f"{path} is missing; run without --no-build", file=sys.stderr)
            return 1
    for package, out in ((PACKAGE, args.out), (APPLIANCE, args.out), (APPLIANCE, args.appliance_out)):
        for stale in out.glob(f"{package}_*.deb") if out.is_dir() else []:
            stale.unlink()
    with tempfile.TemporaryDirectory(prefix="bmacd-deb-") as scratch:
        tree = Path(scratch) / "root"
        stage(tree, binaries, version)
        print(build_deb(tree, args.out, version))
        tree = Path(scratch) / "appliance"
        stage_appliance(tree, appliance, version)
        print(build_deb(tree, args.appliance_out, version, package=APPLIANCE))
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
