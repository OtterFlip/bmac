#!/usr/bin/env python3

# Copyright (c) 2026 BEENTHERE VENTURES, INC.
# SPDX-License-Identifier: GPL-3.0-only

"""Build and sign BMAC release bundles (design/DEPLOYMENT.md, "Upgrades").

    v3/dev/release.py build --key KEY [--out DIR] [--check-reproducible]
        builds the release packages (bmacd, bmac-backupd, and the Deployer,
        which carries that bmacd, bmacd alone as a bundle signed with KEY
        for installed hosts to keep, and a static bmac-inventory), without
        the dev-signing feature, and packs them into DIR/bmac-<version>.bundle
        signed with KEY. --check-reproducible builds everything twice from
        scratch and refuses to pack if any package differs.

    v3/dev/release.py bundle --key KEY --out FILE DEB...
        packs packages already built into a bundle signed with KEY.

    v3/dev/release.py verify --public PUB FILE
        checks a bundle's signature and every package's hash.

A bundle is a ustar archive: manifest.json, manifest.sig (the raw Ed25519
signature over manifest.json's bytes), then exactly the packages the
manifest lists with their SHA-256. bmacd reads it with crates/bmac-release.

The private key is only ever passed to `openssl pkeyutl` by path; it is
never read, copied, or printed here. The release key is kept offline; the
development key (~/.config/bmac/signing/bmac-dev-signing.key) is trusted
only by builds with the dev-signing feature, which dev/build_deb.py makes.
"""

from __future__ import annotations

import argparse
import hashlib
import importlib.util
import io
import json
import os
import re
import shutil
import subprocess
import sys
import tarfile
import tempfile
import time
from pathlib import Path

V3_ROOT = Path(__file__).resolve().parent.parent
BUNDLE_FORMAT = 1
MANIFEST = "manifest.json"
SIGNATURE = "manifest.sig"
DEV_KEY = Path.home() / ".config" / "bmac" / "signing" / "bmac-dev-signing.key"
STATIC_TARGET = "x86_64-unknown-linux-gnu"
DEPLOYER = "bmac-deployer"
RECORD_RS = V3_ROOT / "crates" / "bmac-pmxcfs" / "src" / "record.rs"

_spec = importlib.util.spec_from_file_location("v3_build_deb", V3_ROOT / "dev" / "build_deb.py")
assert _spec and _spec.loader
build_deb = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(build_deb)


class ReleaseError(Exception):
    pass


def record_schemas(text: str | None = None) -> tuple[int, int]:
    """The record schema this source writes and the oldest it reads."""
    text = RECORD_RS.read_text(encoding="utf-8") if text is None else text
    found = {}
    for name in ("SCHEMA_VERSION", "OLDEST_READ_SCHEMA"):
        match = re.search(rf"pub const {name}: u64 = (\d+);", text)
        if not match:
            raise ReleaseError(f"{RECORD_RS} has no {name}")
        found[name] = int(match.group(1))
    return found["SCHEMA_VERSION"], found["OLDEST_READ_SCHEMA"]


def sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as f:
        for chunk in iter(lambda: f.read(1 << 20), b""):
            digest.update(chunk)
    return digest.hexdigest()


def deb_fields(path: Path, dpkg_deb: str = "dpkg-deb") -> dict[str, str]:
    out = subprocess.run([dpkg_deb, "-f", str(path), "Package", "Version", "Architecture"],
                         check=True, capture_output=True, text=True).stdout
    fields = {}
    for line in out.splitlines():
        key, _, value = line.partition(": ")
        fields[key] = value.strip()
    return fields


def manifest(version: str, debs: list[Path], built_at_s: int, schemas: tuple[int, int],
             dpkg_deb: str = "dpkg-deb") -> bytes:
    packages = []
    for deb in debs:
        fields = deb_fields(deb, dpkg_deb)
        packages.append({
            "name": fields["Package"],
            "file": deb.name,
            "version": fields["Version"],
            "architecture": fields["Architecture"],
            "sha256": sha256_file(deb),
            "size": deb.stat().st_size,
        })
    names = [p["name"] for p in packages]
    if len(set(names)) != len(names):
        raise ReleaseError(f"a package is given twice: {names}")
    body = {
        "format": BUNDLE_FORMAT,
        "version": version,
        "built_at_s": built_at_s,
        "record_schema": schemas[0],
        "reads_record_schema_from": schemas[1],
        "packages": packages,
    }
    return (json.dumps(body, indent=2) + "\n").encode()


def sign(message: bytes, key: Path) -> bytes:
    """The raw Ed25519 signature over message, by openssl with the key file."""
    if not key.is_file():
        raise ReleaseError(f"no signing key at {key}")
    with tempfile.TemporaryDirectory(prefix="bmac-sign-") as scratch:
        data = Path(scratch) / MANIFEST
        data.write_bytes(message)
        signature = subprocess.run(["openssl", "pkeyutl", "-sign", "-rawin", "-inkey", str(key), "-in", str(data)],
                                   check=True, capture_output=True).stdout
    if len(signature) != 64:
        raise ReleaseError(f"openssl gave a {len(signature)}-byte signature; is {key} an Ed25519 key?")
    return signature


def verify_signature(message: bytes, signature: bytes, public: Path) -> bool:
    with tempfile.TemporaryDirectory(prefix="bmac-verify-") as scratch:
        data, sig = Path(scratch) / MANIFEST, Path(scratch) / SIGNATURE
        data.write_bytes(message)
        sig.write_bytes(signature)
        result = subprocess.run(["openssl", "pkeyutl", "-verify", "-rawin", "-pubin", "-inkey", str(public),
                                 "-in", str(data), "-sigfile", str(sig)], capture_output=True)
    return result.returncode == 0


def _member(tar: tarfile.TarFile, name: str, size: int, mtime: int, data) -> None:
    info = tarfile.TarInfo(name)
    info.size, info.mtime, info.mode = size, mtime, 0o644
    info.uid = info.gid = 0
    info.uname = info.gname = ""
    tar.addfile(info, data)


def write_bundle(out: Path, manifest_bytes: bytes, signature: bytes, debs: list[Path], mtime: int) -> Path:
    out.parent.mkdir(parents=True, exist_ok=True)
    partial = out.with_suffix(out.suffix + ".partial")
    with tarfile.open(partial, "w", format=tarfile.USTAR_FORMAT) as tar:
        _member(tar, MANIFEST, len(manifest_bytes), mtime, io.BytesIO(manifest_bytes))
        _member(tar, SIGNATURE, len(signature), mtime, io.BytesIO(signature))
        for deb in debs:
            with deb.open("rb") as f:
                _member(tar, deb.name, deb.stat().st_size, mtime, f)
    partial.replace(out)
    return out


def bundle(version: str, debs: list[Path], key: Path, out: Path, built_at_s: int | None = None) -> Path:
    built_at_s = int(os.environ.get("SOURCE_DATE_EPOCH", time.time())) if built_at_s is None else built_at_s
    text = manifest(version, debs, built_at_s, record_schemas())
    return write_bundle(out, text, sign(text, key), debs, built_at_s)


def verify(path: Path, public: Path) -> dict:
    """The manifest, once the signature and every package check out."""
    with tarfile.open(path, "r:") as tar:
        members = tar.getmembers()
        names = [m.name for m in members]
        if names[:2] != [MANIFEST, SIGNATURE]:
            raise ReleaseError(f"{path} doesn't start with {MANIFEST} and {SIGNATURE}")
        text = tar.extractfile(members[0]).read()
        signature = tar.extractfile(members[1]).read()
        if not verify_signature(text, signature, public):
            raise ReleaseError(f"{path} isn't signed by {public}")
        body = json.loads(text)
        listed = {p["file"]: p for p in body["packages"]}
        if sorted(names[2:]) != sorted(listed):
            raise ReleaseError(f"{path} holds {names[2:]}, its manifest lists {sorted(listed)}")
        for member in members[2:]:
            data = tar.extractfile(member).read()
            if hashlib.sha256(data).hexdigest() != listed[member.name]["sha256"]:
                raise ReleaseError(f"{member.name} doesn't match the manifest")
    return body


def source_date_epoch() -> int:
    out = subprocess.run(["git", "log", "-1", "--format=%ct"], cwd=V3_ROOT, check=True,
                         capture_output=True, text=True).stdout.strip()
    return int(out)


def build_packages(target: Path, out: Path, version: str, env: dict[str, str], epoch: int, key: Path) -> list[Path]:
    """One release build into target: bmacd, bmac-backupd, and the Deployer."""
    run = lambda args, **kw: subprocess.run(args, cwd=V3_ROOT, check=True, env=env, **kw)  # noqa: E731
    run(["pnpm", "--filter", "@bmac/webui", "--filter", "@bmac/applianceui", "build"])
    run(["cargo", "build", "--release", "--locked", "-p", "bmacd", "-p", "bmac-cli", "-p", "bmac-backupd",
         "--target-dir", str(target)])
    static_env = dict(env, RUSTFLAGS="-C target-feature=+crt-static")
    subprocess.run(["cargo", "build", "--release", "--locked", "-p", "bmac-inventory", "--target", STATIC_TARGET,
                    "--target-dir", str(target / "static")], cwd=V3_ROOT, check=True, env=static_env)
    release = target / "release"
    out.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix="bmac-release-") as scratch:
        tree = Path(scratch) / "bmacd"
        build_deb.stage(tree, {"bmacd": release / "bmacd", "bmac": release / "bmac"}, version, release=True)
        bmacd = build_deb.build_deb(tree, out, version, epoch=epoch)
        tree = Path(scratch) / "appliance"
        build_deb.stage_appliance(tree, release / "bmac-backupd", version, release=True)
        appliance = build_deb.build_deb(tree, out, version, package=build_deb.APPLIANCE, epoch=epoch)
    resources = target / "deployer-bundle"
    shutil.rmtree(resources, ignore_errors=True)
    (resources / "packages").mkdir(parents=True)
    shutil.copy2(bmacd, resources / "packages" / bmacd.name)
    bundle(version, [bmacd], key, resources / "packages" / f"{build_deb.HOST_BUNDLE}-{version}.bundle", epoch)
    shutil.copy2(target / "static" / STATIC_TARGET / "release" / "bmac-inventory", resources / "bmac-inventory")
    config = json.dumps({"bundle": {"resources": {f"{resources}/": "bundle/"}}})
    run(["pnpm", "--filter", "@bmac/deployer", "tauri", "build", "--bundles", "deb", "--config", config,
         "--", "--locked", "--target-dir", str(target)])
    built = sorted((target / "release" / "bundle" / "deb").glob("*.deb"))
    if len(built) != 1:
        raise ReleaseError(f"expected one Deployer package, found {built}")
    deployer = repack(built[0], out, version, epoch)
    return [bmacd, appliance, deployer]


def repack(deb: Path, out: Path, version: str, epoch: int) -> Path:
    """Tauri stamps its package with the time of the build, and its version
    as given, where a pre-release's "-" would make dpkg sort it after the
    release; the same files packed again by build_deb carry the epoch and
    the Debian version the other packages have."""
    with tempfile.TemporaryDirectory(prefix="bmac-repack-") as scratch:
        tree = Path(scratch) / "deployer"
        subprocess.run(["dpkg-deb", "--raw-extract", str(deb), str(tree)], check=True)
        control = tree / "DEBIAN" / "control"
        text = re.sub(r"(?m)^Version: .*$", f"Version: {build_deb.debian_version(version)}",
                      control.read_text(encoding="utf-8"))
        control.write_text(text, encoding="utf-8")
        return build_deb.build_deb(tree, out, version, package=DEPLOYER, epoch=epoch)


def build(key: Path, out: Path, check: bool) -> Path:
    version = (V3_ROOT / "dev" / "VERSION").read_text(encoding="utf-8").strip()
    epoch = source_date_epoch()
    env = dict(os.environ, SOURCE_DATE_EPOCH=str(epoch), BMAC_VERSION=version)
    env.pop("CARGO_TARGET_DIR", None)
    target = V3_ROOT / "target" / "release-build"
    runs = 2 if check else 1
    results: list[list[Path]] = []
    for n in range(runs):
        shutil.rmtree(target, ignore_errors=True)
        results.append(build_packages(target, out / f"run{n + 1}", version, env, epoch, key))
    if check:
        differ = [a.name for a, b in zip(*results) if sha256_file(a) != sha256_file(b)]
        if differ:
            raise ReleaseError(f"the two builds differ in {', '.join(differ)}; nothing was released")
        print("both builds are identical")
    debs = []
    for deb in results[0]:
        shutil.copy2(deb, out / deb.name)
        debs.append(out / deb.name)
    for n in range(runs):
        shutil.rmtree(out / f"run{n + 1}")
    return bundle(version, debs, key, out / f"bmac-{version}.bundle", epoch)


def main(argv: list[str]) -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = parser.add_subparsers(dest="command", required=True)
    b = sub.add_parser("build", help="build, check, and sign a release")
    b.add_argument("--key", type=Path, required=True)
    b.add_argument("--out", type=Path, default=V3_ROOT / "target" / "release-bundle")
    b.add_argument("--check-reproducible", action="store_true")
    p = sub.add_parser("bundle", help="pack and sign packages already built")
    p.add_argument("--key", type=Path, required=True)
    p.add_argument("--out", type=Path, required=True)
    p.add_argument("--version", default=None)
    p.add_argument("debs", type=Path, nargs="+")
    v = sub.add_parser("verify", help="check a bundle")
    v.add_argument("--public", type=Path, required=True)
    v.add_argument("bundle", type=Path)
    args = parser.parse_args(argv)
    try:
        if args.command == "build":
            print(build(args.key, args.out, args.check_reproducible))
        elif args.command == "bundle":
            version = args.version or (V3_ROOT / "dev" / "VERSION").read_text(encoding="utf-8").strip()
            print(bundle(version, args.debs, args.key, args.out))
        else:
            body = verify(args.bundle, args.public)
            print(f"{args.bundle}: BMAC {body['version']}, " + ", ".join(
                f"{p['name']} {p['version']}" for p in body["packages"]))
    except (ReleaseError, subprocess.CalledProcessError) as e:
        print(f"release: {e}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
