#!/usr/bin/env python3

# Copyright (c) 2026 BEENTHERE VENTURES, INC.
# SPDX-License-Identifier: GPL-3.0-only

"""Safely rewrite an offline guest tree without following guest symlinks."""

from __future__ import annotations

import argparse
import fnmatch
import json
import os
import stat
import sys
from pathlib import Path


class UnsafeGuestPath(RuntimeError):
    pass


class MissingGuestPath(UnsafeGuestPath):
    pass


class GuestTree:
    def __init__(self, root: str) -> None:
        flags = os.O_RDONLY | os.O_DIRECTORY | getattr(os, "O_NOFOLLOW", 0)
        self.root_fd = os.open(root, flags)

    def close(self) -> None:
        os.close(self.root_fd)

    @staticmethod
    def parts(path: str) -> list[str]:
        values = [part for part in path.strip("/").split("/") if part]
        if not values or any(part in {".", ".."} for part in values):
            raise UnsafeGuestPath(f"unsafe guest path: {path!r}")
        return values

    def directory(self, path: str, *, create: bool = False, mode: int = 0o755) -> int:
        current = os.dup(self.root_fd)
        try:
            for part in self.parts(path):
                try:
                    next_fd = os.open(
                        part,
                        os.O_RDONLY
                        | os.O_DIRECTORY
                        | getattr(os, "O_NOFOLLOW", 0),
                        dir_fd=current,
                    )
                except FileNotFoundError:
                    if not create:
                        raise MissingGuestPath(
                            f"required guest directory is missing: /{path.strip('/')}"
                        )
                    os.mkdir(part, mode, dir_fd=current)
                    next_fd = os.open(
                        part,
                        os.O_RDONLY
                        | os.O_DIRECTORY
                        | getattr(os, "O_NOFOLLOW", 0),
                        dir_fd=current,
                    )
                except OSError as exc:
                    raise UnsafeGuestPath(
                        f"guest path is not a real directory: /{path.strip('/')}"
                    ) from exc
                os.close(current)
                current = next_fd
            return current
        except Exception:
            os.close(current)
            raise

    def parent(self, path: str, *, create: bool = False) -> tuple[int, str]:
        parts = self.parts(path)
        if len(parts) == 1:
            return os.dup(self.root_fd), parts[0]
        return self.directory("/".join(parts[:-1]), create=create), parts[-1]

    @staticmethod
    def _stat(fd: int, name: str) -> os.stat_result | None:
        try:
            return os.stat(name, dir_fd=fd, follow_symlinks=False)
        except FileNotFoundError:
            return None

    def read(self, path: str, *, required: bool = True) -> tuple[bytes, os.stat_result | None]:
        parent_fd, name = self.parent(path)
        try:
            info = self._stat(parent_fd, name)
            if info is None:
                if required:
                    raise UnsafeGuestPath(f"required guest file is missing: /{path}")
                return b"", None
            if not stat.S_ISREG(info.st_mode):
                raise UnsafeGuestPath(f"guest path is not a regular file: /{path}")
            fd = os.open(
                name,
                os.O_RDONLY | getattr(os, "O_NOFOLLOW", 0),
                dir_fd=parent_fd,
            )
            try:
                chunks = []
                remaining = 4 * 1024 * 1024 + 1
                while remaining:
                    chunk = os.read(fd, min(remaining, 64 * 1024))
                    if not chunk:
                        break
                    chunks.append(chunk)
                    remaining -= len(chunk)
                data = b"".join(chunks)
                if len(data) > 4 * 1024 * 1024:
                    raise UnsafeGuestPath(f"guest file is unexpectedly large: /{path}")
                return data, info
            finally:
                os.close(fd)
        finally:
            os.close(parent_fd)

    def write(
        self,
        path: str,
        data: bytes,
        mode: int,
        *,
        owner: tuple[int, int] | None = None,
        create_parent: bool = False,
    ) -> None:
        parent_fd, name = self.parent(path, create=create_parent)
        temporary = f".{name}.app-ha-new-{os.getpid()}"
        try:
            existing = self._stat(parent_fd, name)
            if existing is not None and not stat.S_ISREG(existing.st_mode):
                raise UnsafeGuestPath(
                    f"refusing to replace non-regular guest path: /{path}"
                )
            flags = (
                os.O_WRONLY
                | os.O_CREAT
                | os.O_EXCL
                | getattr(os, "O_NOFOLLOW", 0)
            )
            fd = os.open(temporary, flags, mode, dir_fd=parent_fd)
            try:
                offset = 0
                while offset < len(data):
                    offset += os.write(fd, data[offset:])
                os.fchmod(fd, mode)
                if owner is not None:
                    os.fchown(fd, *owner)
                os.fsync(fd)
            finally:
                os.close(fd)
            os.rename(temporary, name, src_dir_fd=parent_fd, dst_dir_fd=parent_fd)
            os.fsync(parent_fd)
        finally:
            try:
                os.unlink(temporary, dir_fd=parent_fd)
            except FileNotFoundError:
                pass
            os.close(parent_fd)

    def unlink(self, path: str, *, missing_ok: bool = True) -> None:
        try:
            parent_fd, name = self.parent(path)
        except MissingGuestPath:
            if missing_ok:
                return
            raise
        try:
            info = self._stat(parent_fd, name)
            if info is None:
                if missing_ok:
                    return
                raise UnsafeGuestPath(f"guest path is missing: /{path}")
            if stat.S_ISDIR(info.st_mode):
                raise UnsafeGuestPath(f"refusing to unlink guest directory: /{path}")
            os.unlink(name, dir_fd=parent_fd)
        finally:
            os.close(parent_fd)

    def symlink(self, path: str, target: str) -> None:
        parent_fd, name = self.parent(path, create=True)
        try:
            info = self._stat(parent_fd, name)
            if info is not None:
                if stat.S_ISDIR(info.st_mode):
                    raise UnsafeGuestPath(
                        f"refusing to replace guest directory with symlink: /{path}"
                    )
                os.unlink(name, dir_fd=parent_fd)
            os.symlink(target, name, dir_fd=parent_fd)
        finally:
            os.close(parent_fd)

    def _remove_entry(self, parent_fd: int, name: str) -> None:
        info = self._stat(parent_fd, name)
        if info is None:
            return
        if stat.S_ISDIR(info.st_mode):
            child_fd = os.open(
                name,
                os.O_RDONLY | os.O_DIRECTORY | getattr(os, "O_NOFOLLOW", 0),
                dir_fd=parent_fd,
            )
            try:
                for child in os.listdir(child_fd):
                    self._remove_entry(child_fd, child)
            finally:
                os.close(child_fd)
            os.rmdir(name, dir_fd=parent_fd)
        else:
            os.unlink(name, dir_fd=parent_fd)

    def remove_tree(self, path: str) -> None:
        try:
            parent_fd, name = self.parent(path)
        except MissingGuestPath:
            return
        try:
            self._remove_entry(parent_fd, name)
        finally:
            os.close(parent_fd)

    def clear_directory(self, path: str, *, create: bool = False) -> None:
        directory_fd = self.directory(path, create=create)
        try:
            for name in os.listdir(directory_fd):
                self._remove_entry(directory_fd, name)
        finally:
            os.close(directory_fd)

    def remove_matching(
        self, path: str, patterns: tuple[str, ...], *, recursive: bool = False
    ) -> None:
        try:
            directory_fd = self.directory(path)
        except MissingGuestPath:
            return
        try:
            self._remove_matching_fd(directory_fd, patterns, recursive)
        finally:
            os.close(directory_fd)

    def _remove_matching_fd(
        self, directory_fd: int, patterns: tuple[str, ...], recursive: bool
    ) -> None:
        for name in os.listdir(directory_fd):
            info = self._stat(directory_fd, name)
            if info is None:
                continue
            if any(fnmatch.fnmatch(name.lower(), pattern.lower()) for pattern in patterns):
                self._remove_entry(directory_fd, name)
            elif recursive and stat.S_ISDIR(info.st_mode):
                child_fd = os.open(
                    name,
                    os.O_RDONLY
                    | os.O_DIRECTORY
                    | getattr(os, "O_NOFOLLOW", 0),
                    dir_fd=directory_fd,
                )
                try:
                    self._remove_matching_fd(child_fd, patterns, True)
                finally:
                    os.close(child_fd)


def patch(tree: GuestTree, args: argparse.Namespace) -> None:
    for path in (
        "etc",
        "etc/systemd",
        "var",
        "var/lib",
        "var/log",
        "usr",
        "usr/local",
        "usr/local/lib",
    ):
        fd = tree.directory(path)
        os.close(fd)

    tree.write("etc/hostname", f"{args.name}\n".encode(), 0o644)
    hosts_data, _ = tree.read("etc/hosts", required=False)
    kept = []
    for line in hosts_data.decode("utf-8").splitlines():
        words = line.strip().split()
        if words and not words[0].startswith("#") and words[0] == "127.0.1.1":
            continue
        kept.append(line)
    if not any(line.strip().split()[:1] == ["127.0.0.1"] for line in kept):
        kept.insert(0, "127.0.0.1 localhost")
    kept.append(f"127.0.1.1 {args.name}")
    tree.write("etc/hosts", ("\n".join(kept) + "\n").encode(), 0o644)

    netplan_fd = tree.directory("etc/netplan", create=True)
    try:
        for name in os.listdir(netplan_fd):
            if name.lower().endswith((".yaml", ".yml")):
                info = tree._stat(netplan_fd, name)
                if info is not None and stat.S_ISDIR(info.st_mode):
                    raise UnsafeGuestPath(f"netplan entry is a directory: {name}")
                tree._remove_entry(netplan_fd, name)
    finally:
        os.close(netplan_fd)
    document = {
        "network": {
            "version": 2,
            "ethernets": {
                "private": {
                    "match": {"macaddress": args.mac.lower()},
                    "set-name": "lan0",
                    "dhcp4": False,
                    "dhcp6": False,
                    "addresses": [args.address],
                    "routes": [{"to": "default", "via": args.gateway}],
                    "nameservers": {"addresses": args.dns.split(",")},
                }
            },
        }
    }
    tree.write(
        "etc/netplan/90-app-ha-staging.yaml",
        (json.dumps(document, sort_keys=True, indent=2) + "\n").encode(),
        0o600,
    )

    shadow_data, shadow_stat = tree.read("etc/shadow")
    assert shadow_stat is not None
    shadow_mode = stat.S_IMODE(shadow_stat.st_mode)
    if shadow_mode & 0o117:
        raise UnsafeGuestPath("guest /etc/shadow has unsafe permissions")
    root_hash = Path(args.root_hash_file).read_text(encoding="utf-8").rstrip("\n")
    output = []
    matches = 0
    for line in shadow_data.decode("utf-8").splitlines():
        fields = line.split(":")
        if fields[0] == "root":
            if len(fields) != 9:
                raise UnsafeGuestPath("root shadow entry is malformed")
            fields[1] = root_hash
            line = ":".join(fields)
            matches += 1
        output.append(line)
    if matches != 1:
        raise UnsafeGuestPath("guest shadow must contain exactly one root entry")
    tree.write(
        "etc/shadow",
        ("\n".join(output) + "\n").encode(),
        shadow_mode,
        owner=(shadow_stat.st_uid, shadow_stat.st_gid),
    )

    tree.write("etc/machine-id", b"", 0o444)
    tree.unlink("var/lib/dbus/machine-id")
    tree.remove_matching("etc/ssh", ("ssh_host_*_key*",))
    tree.clear_directory("var/lib/cloud", create=True)
    tree.write("etc/cloud/cloud-init.disabled", b"", 0o644)
    tree.remove_matching(
        "etc/cloud/cloud.cfg.d", ("*network*", "*datasource*")
    )
    tree.remove_matching("var/log", ("cloud-init*.log", "cloud-init-output*.log"))
    for path in (
        "var/lib/dhcp",
        "var/lib/NetworkManager",
        "var/lib/systemd/network",
    ):
        tree.remove_matching(path, ("*lease*", "*.leases"), recursive=True)

    ssh_key_unit = b"""[Unit]
Description=Generate unique SSH host keys for this staging guest
DefaultDependencies=no
After=local-fs.target
Before=ssh.service sshd.service ssh.socket

[Service]
Type=oneshot
ExecStart=/usr/bin/ssh-keygen -A
RemainAfterExit=yes
"""
    tree.write(
        "etc/systemd/system/app-ha-ssh-host-keys.service", ssh_key_unit, 0o644
    )
    dependency = b"""[Unit]
Requires=app-ha-ssh-host-keys.service
After=app-ha-ssh-host-keys.service
"""
    for unit in ("ssh.service", "sshd.service", "ssh.socket"):
        tree.write(
            f"etc/systemd/system/{unit}.d/50-app-ha-host-keys.conf",
            dependency,
            0o644,
            create_parent=True,
        )
    tree.symlink(
        "etc/systemd/system/multi-user.target.wants/app-ha-ssh-host-keys.service",
        "../app-ha-ssh-host-keys.service",
    )

    library = "usr/local/lib/app-ha"
    state = "var/lib/app-ha"
    systemd = "etc/systemd/system"
    for path in (library, state):
        directory_fd = tree.directory(path, create=True)
        os.close(directory_fd)
    tree.unlink(f"{state}/staging-sanitizer-started")
    tree.unlink(f"{state}/staging-sanitizer-complete")
    sanitizer_unit = f"{systemd}/app-ha-staging-sanitizer.service"
    sanitizer_dropin = (
        f"{systemd}/network-pre.target.d/50-app-ha-staging-sanitizer.conf"
    )
    if args.sanitizer:
        sanitizer = Path(args.sanitizer).read_bytes()
        tree.write(f"{library}/staging-sanitizer", sanitizer, 0o700)
        runner = b"""#!/bin/bash
set -Eeuo pipefail
state=/var/lib/app-ha
if [[ -e "$state/staging-sanitizer-complete" ]]; then exit 0; fi
if [[ -e "$state/staging-sanitizer-started" ]]; then
  echo "staging sanitizer previously started without completing; refusing automatic rerun" >&2
  exit 1
fi
/usr/bin/install -m 0600 /dev/null "$state/staging-sanitizer-started"
/usr/local/lib/app-ha/staging-sanitizer
/usr/bin/install -m 0600 /dev/null "$state/staging-sanitizer-complete"
"""
        tree.write(f"{library}/run-staging-sanitizer", runner, 0o700)
        unit = b"""[Unit]
Description=Run the local staging sanitizer exactly once before networking
DefaultDependencies=no
After=local-fs.target
Before=network-pre.target network.target systemd-networkd.service NetworkManager.service networking.service

[Service]
Type=oneshot
User=root
ExecStart=/usr/local/lib/app-ha/run-staging-sanitizer
RemainAfterExit=yes
"""
        tree.write(sanitizer_unit, unit, 0o644)
        dropin = b"""[Unit]
Requires=app-ha-staging-sanitizer.service
After=app-ha-staging-sanitizer.service
"""
        tree.write(sanitizer_dropin, dropin, 0o644, create_parent=True)
        for unit_name in (
            "network.target",
            "systemd-networkd.service",
            "NetworkManager.service",
            "networking.service",
        ):
            tree.write(
                f"{systemd}/{unit_name}.d/50-app-ha-staging-sanitizer.conf",
                dropin,
                0o644,
                create_parent=True,
            )
    else:
        for path in (
            f"{library}/staging-sanitizer",
            f"{library}/run-staging-sanitizer",
            sanitizer_unit,
            sanitizer_dropin,
        ):
            tree.unlink(path)
        for unit_name in (
            "network.target",
            "systemd-networkd.service",
            "NetworkManager.service",
            "networking.service",
        ):
            tree.unlink(
                f"{systemd}/{unit_name}.d/50-app-ha-staging-sanitizer.conf"
            )


def inspect_ubuntu_root(root: str) -> tuple[bool, str]:
    tree = GuestTree(root)
    try:
        os_release, os_release_stat = tree.read(
            "usr/lib/os-release", required=False
        )
        os_release_path = "/usr/lib/os-release"
        if os_release_stat is None:
            os_release, _ = tree.read("etc/os-release")
            os_release_path = "/etc/os-release"
        tree.read("etc/fstab")
        tree.read("etc/shadow")
        os_release_text = os_release.decode("utf-8")
    except (OSError, UnicodeError, UnsafeGuestPath) as exc:
        return False, f"{type(exc).__name__}: {exc}"
    finally:
        tree.close()
    values: set[str] = set()
    for line in os_release_text.splitlines():
        key, separator, value = line.partition("=")
        if separator and key in {"ID", "ID_LIKE"}:
            values.update(value.strip("\"'").lower().split())
    if "ubuntu" not in values:
        return (
            False,
            f"{os_release_path} ID/ID_LIKE does not identify Ubuntu",
        )
    return True, f"Ubuntu identity verified through {os_release_path}"


def probe_ubuntu_root(root: str) -> bool:
    accepted, _reason = inspect_ubuntu_root(root)
    return accepted


def main() -> int:
    if len(sys.argv) == 3 and sys.argv[1] == "--probe-ubuntu-root":
        return 0 if probe_ubuntu_root(sys.argv[2]) else 1
    if len(sys.argv) == 3 and sys.argv[1] == "--diagnose-ubuntu-root":
        accepted, reason = inspect_ubuntu_root(sys.argv[2])
        print(reason, file=sys.stderr)
        return 0 if accepted else 1
    parser = argparse.ArgumentParser()
    parser.add_argument("--root", required=True)
    parser.add_argument("--name", required=True)
    parser.add_argument("--address", required=True)
    parser.add_argument("--gateway", required=True)
    parser.add_argument("--mac", required=True)
    parser.add_argument("--dns", required=True)
    parser.add_argument("--root-hash-file", required=True)
    parser.add_argument("--sanitizer")
    args = parser.parse_args()
    tree = GuestTree(args.root)
    try:
        patch(tree, args)
    finally:
        tree.close()
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
