#!/usr/bin/env python3

# Copyright (c) 2026 BEENTHERE VENTURES, INC.
# SPDX-License-Identifier: GPL-3.0-only

"""Root-owned pmxcfs registry and deterministic IPAM for app-ha guests.

The default state directory is /etc/pve/priv/app-ha.  A different absolute
directory may be selected explicitly for non-destructive tests.  The registry
contains policy and orchestration metadata only; its schemas intentionally
have no fields for credentials, tokens, password hashes, or private keys.
"""

from __future__ import annotations

import argparse
import contextlib
import copy
import datetime as dt
import errno
import hashlib
import ipaddress
import json
import os
from pathlib import Path
import re
import socket
import subprocess
import sys
import time
import uuid
from typing import Any, Iterator, Sequence


SCHEMA_VERSION = 1
PMXCFS_ROOT = Path("/etc/pve")
PMXCFS_LOCK_DIR = PMXCFS_ROOT / "priv" / "lock"
DEFAULT_STATE_DIR = PMXCFS_ROOT / "priv" / "app-ha"
MAX_JSON_BYTES = 1024 * 1024
MAX_ROUTES = 4096
MAX_HISTORY_RECORDS = 256
MAX_COMPLETED_CLEANUP_RECORDS = 512
MAX_CLEANUP_RECORDS = 4096
MAX_INGRESS_GENERATIONS = 8
INGRESS_NODE_STATES = {"pending", "staged", "applied", "reject-only"}
INGRESS_GENERATION_KEYS = {
    "schema_version",
    "record_type",
    "generation",
    "bundle_sha256",
    "route_count",
    "created_at",
}
INGRESS_DESIRED_KEYS = {
    "schema_version",
    "record_type",
    "generation",
    "bundle_sha256",
    "route_count",
    "nodes",
    "created_at",
    "updated_at",
    "revision",
}
INGRESS_NODE_STATUS_KEYS = {
    "schema_version",
    "record_type",
    "node",
    "desired_generation",
    "applied_generation",
    "state",
    "created_at",
    "updated_at",
    "revision",
}
INSTALL_PHASES = {
    "unknown",
    "unstarted",
    "media-attached",
    "installer-started",
    "installed-confirmed",
    "guest-verified",
    "network-finalized",
}
INSTALL_PHASE_TRANSITIONS = {
    "unknown": {"media-attached", "installed-confirmed"},
    "unstarted": {"media-attached", "installed-confirmed"},
    "media-attached": {"installer-started"},
    "installer-started": {"media-attached", "installed-confirmed"},
    "installed-confirmed": {"guest-verified"},
    "guest-verified": {"network-finalized"},
    "network-finalized": set(),
}
ORCHESTRATION_KEYS = {
    "schema_version",
    "record_type",
    "resource",
    "resource_id",
    "install_phase",
    "lease",
    "created_at",
    "updated_at",
    "revision",
}
ORCHESTRATION_LEGACY_CONTRACT_KEYS = {
    "final_network_enabled",
    "startup_sha256",
}
ORCHESTRATION_CONTRACT_KEYS = ORCHESTRATION_LEGACY_CONTRACT_KEYS | {
    "source_iso_sha256",
    "install_mode",
}
LEASE_KEYS = {"nonce", "owner", "acquired_at", "expires_at"}

RESOURCE_STATES = {
    "reserved",
    "provisioning",
    "snapshotting",
    "replicating",
    "cloning",
    "patching",
    "stopped",
    "ready",
    "active",
    "stopping",
    "cleanup_pending",
    "failed",
}

STATE_TRANSITIONS: dict[str, dict[str, set[str]]] = {
    "production": {
        "reserved": {"provisioning", "failed", "cleanup_pending"},
        "provisioning": {"stopped", "replicating", "failed", "cleanup_pending"},
        "replicating": {"stopped", "ready", "failed", "cleanup_pending"},
        "stopped": {
            "provisioning",
            "replicating",
            "ready",
            "active",
            "failed",
            "cleanup_pending",
        },
        "ready": {"active", "stopped", "failed", "cleanup_pending"},
        "active": {"stopping", "stopped", "failed"},
        "stopping": {"stopped", "failed", "cleanup_pending"},
        "failed": {"provisioning", "stopped", "cleanup_pending"},
        "cleanup_pending": {"failed"},
    },
    "staging": {
        "reserved": {"snapshotting", "cloning", "failed", "cleanup_pending"},
        "snapshotting": {"replicating", "cloning", "failed", "cleanup_pending"},
        "replicating": {"cloning", "failed", "cleanup_pending"},
        "cloning": {"patching", "failed", "cleanup_pending"},
        "patching": {"stopped", "ready", "failed", "cleanup_pending"},
        "stopped": {"ready", "active", "failed", "cleanup_pending"},
        "ready": {"active", "stopped", "failed", "cleanup_pending"},
        "active": {"stopping", "stopped", "failed"},
        "stopping": {"stopped", "failed", "cleanup_pending"},
        "failed": {"snapshotting", "cloning", "patching", "stopped", "cleanup_pending"},
        "cleanup_pending": {"failed"},
    },
}

RELEASE_STATES = {"reserved", "stopped", "failed", "cleanup_pending"}
ROUTABLE_STATES = {"stopped", "ready", "active"}
CLEANUP_ACTIONS = {
    "delete-snapshot",
    "destroy-vm",
    "destroy-volume",
    "remove-replication",
    "remove-route",
}

RESOURCE_KEYS = {
    "schema_version",
    "record_type",
    "id",
    "allocation_id",
    "request_fingerprint",
    "kind",
    "name",
    "index",
    "source",
    "vmid",
    "ip",
    "mac",
    "state",
    "purpose",
    "domains",
    "placement",
    "initial_node",
    "owner_node",
    "routes_enabled",
    "spec",
    "proxmox",
    "created_at",
    "updated_at",
    "revision",
}

SECRET_KEY_RE = re.compile(
    r"(?:^|_)(?:password|passwd|secret|token|credential|private_key|auth_key)(?:$|_)",
    re.IGNORECASE,
)
SECRET_VALUE_RE = re.compile(
    r"(?:-----BEGIN [A-Z ]*PRIVATE KEY-----|tskey-(?:auth|client)-)",
    re.IGNORECASE,
)
PURPOSE_RE = re.compile(r"^[a-z][a-z0-9]*(?:-[a-z0-9]+)*$")
MOX_RE = re.compile(r"^mox([1-9]|10)$")
RESOURCE_RE = re.compile(
    r"^(?:prod(?P<production>[1-9][0-9]*)|"
    r"stage(?P<stage>[1-9][0-9]*)(?P<source>prod[1-9][0-9]*))$"
)
SAFE_ID_RE = re.compile(r"^[A-Za-z0-9][A-Za-z0-9_.:/@+-]{0,255}$")
SHA256_RE = re.compile(r"^[0-9a-f]{64}$")
SNAPSHOT_RE = re.compile(r"^[A-Za-z][A-Za-z0-9._-]{0,39}$")
GUID_RE = re.compile(r"^[0-9]{1,20}$")
MAC_RE = re.compile(r"^(?:[0-9a-f]{2}:){5}[0-9a-f]{2}$")
DOMAIN_LABEL_RE = re.compile(r"^[a-z0-9](?:[a-z0-9-]{0,61}[a-z0-9])?$")
VM_CLEANUP_TARGET_RE = re.compile(r"^vm:([1-9][0-9]*)$")
VOLUME_CLEANUP_TARGET_RE = re.compile(
    r"^[A-Za-z0-9][A-Za-z0-9_.+-]{0,63}:vm-([1-9][0-9]*)-disk-([0-9]+)$"
)
SNAPSHOT_CLEANUP_TARGET_RE = re.compile(
    r"^vm-([1-9][0-9]*)@([A-Za-z][A-Za-z0-9._-]{0,39})$"
)
REPLICATION_CLEANUP_TARGET_RE = re.compile(r"^([1-9][0-9]*)-([0-9]+)$")


class RegistryError(Exception):
    """A safe, user-facing registry error."""


def utc_now() -> str:
    return dt.datetime.now(dt.timezone.utc).replace(microsecond=0).isoformat()


def parse_timestamp(value: str) -> dt.datetime:
    try:
        parsed = dt.datetime.fromisoformat(value)
    except (TypeError, ValueError) as exc:
        raise RegistryError(f"invalid registry timestamp: {value!r}") from exc
    if parsed.tzinfo is None:
        raise RegistryError(f"registry timestamp lacks timezone: {value!r}")
    return parsed.astimezone(dt.timezone.utc)


def canonical_json(value: Any) -> bytes:
    return (
        json.dumps(value, sort_keys=True, separators=(",", ":"), ensure_ascii=True)
        + "\n"
    ).encode("utf-8")


def pretty_json(value: Any) -> str:
    return json.dumps(value, sort_keys=True, indent=2, ensure_ascii=True)


def output_json(value: Any) -> None:
    print(pretty_json(value))


def require_exact_keys(value: dict[str, Any], expected: set[str], context: str) -> None:
    unknown = sorted(set(value) - expected)
    missing = sorted(expected - set(value))
    if unknown:
        raise RegistryError(f"{context} has unknown fields: {', '.join(unknown)}")
    if missing:
        raise RegistryError(f"{context} is missing fields: {', '.join(missing)}")


def reject_secret_material(value: Any, context: str = "registry data") -> None:
    """Reject secret-shaped fields and unmistakable secret values."""

    if isinstance(value, dict):
        for key, nested in value.items():
            if SECRET_KEY_RE.search(str(key)):
                raise RegistryError(f"{context} may not contain secret field {key!r}")
            reject_secret_material(nested, context)
    elif isinstance(value, list):
        for nested in value:
            reject_secret_material(nested, context)
    elif isinstance(value, str) and SECRET_VALUE_RE.search(value):
        raise RegistryError(f"{context} contains secret material")


def normalize_domain(value: str) -> str:
    domain = value.strip().rstrip(".").lower()
    if not domain or domain.startswith("*."):
        raise RegistryError(f"domain must be an exact hostname, not a wildcard: {value!r}")
    if "://" in domain or "/" in domain or ":" in domain:
        raise RegistryError(f"domain is not a bare hostname: {value!r}")
    try:
        domain = domain.encode("idna").decode("ascii")
    except UnicodeError as exc:
        raise RegistryError(f"domain cannot be converted to IDNA: {value!r}") from exc
    if len(domain) > 253 or "." not in domain:
        raise RegistryError(f"domain must be a fully-qualified hostname: {value!r}")
    labels = domain.split(".")
    if any(not DOMAIN_LABEL_RE.fullmatch(label) for label in labels):
        raise RegistryError(f"domain contains an invalid label: {value!r}")
    try:
        ipaddress.ip_address(domain)
    except ValueError:
        pass
    else:
        raise RegistryError(f"domain may not be an IP address: {value!r}")
    return domain


def normalize_domains(values: Sequence[str]) -> list[str]:
    result: list[str] = []
    seen: set[str] = set()
    for value in values:
        domain = normalize_domain(value)
        if domain in seen:
            raise RegistryError(f"duplicate domain in request: {domain}")
        seen.add(domain)
        result.append(domain)
    return result


def normalize_purpose(value: str) -> str:
    purpose = value.strip().lower()
    if len(purpose) > 63 or not PURPOSE_RE.fullmatch(purpose):
        raise RegistryError(
            "purpose must be a lowercase slug of at most 63 characters "
            "(letters, digits, and internal hyphens)"
        )
    return purpose


def normalize_allocation_id(value: str | None) -> str | None:
    if value is None:
        return None
    value = value.strip()
    if not value or len(value) > 128 or not SAFE_ID_RE.fullmatch(value):
        raise RegistryError("allocation id contains unsafe characters")
    return value


def validate_vmid(vmid: int, policy: dict[str, Any]) -> None:
    if vmid < 100 or vmid > 999_999_999:
        raise RegistryError("VMID must be between 100 and 999999999")
    reserved = policy["network"]["haproxy_vmid_range"]
    if reserved["start"] <= vmid <= reserved["end"]:
        raise RegistryError(
            f"VMID {vmid} is reserved for fixed HAProxy LXCs "
            f"({reserved['start']}-{reserved['end']})"
        )


def flatten_nodes(values: Sequence[str] | None) -> list[str]:
    if not values:
        return []
    result: list[str] = []
    for value in values:
        result.extend(part.strip() for part in value.split(",") if part.strip())
    return result


def validate_nodes(
    values: Sequence[str],
    policy: dict[str, Any],
    *,
    allow_empty: bool = False,
    exactly_one: bool = False,
) -> list[str]:
    nodes = list(values)
    if not nodes and not allow_empty:
        raise RegistryError("at least one placement node is required")
    if exactly_one and len(nodes) != 1:
        raise RegistryError("exactly one placement node is required")
    seen: set[str] = set()
    for node in nodes:
        if not isinstance(node, str):
            raise RegistryError("node names must be strings")
        match = MOX_RE.fullmatch(node)
        if not match:
            raise RegistryError(f"node must be named mox1 through mox10: {node!r}")
        if int(match.group(1)) > policy["limits"]["max_hosts"]:
            raise RegistryError(
                f"{node} exceeds policy max_hosts={policy['limits']['max_hosts']}"
            )
        if node in seen:
            raise RegistryError(f"duplicate placement node: {node}")
        seen.add(node)
    return nodes


def validate_snapshot_name(value: str) -> str:
    if not SNAPSHOT_RE.fullmatch(value) or value.startswith("__replicate_"):
        raise RegistryError(
            "snapshot name is unsafe or uses Proxmox's reserved __replicate_ prefix"
        )
    return value


def validate_cleanup_target(action: str, value: str) -> str:
    target = validate_safe_identifier(value, "cleanup target")
    if action == "destroy-vm":
        if not VM_CLEANUP_TARGET_RE.fullmatch(target):
            raise RegistryError("destroy-vm cleanup target must be vm:VMID")
    elif action == "destroy-volume":
        if not VOLUME_CLEANUP_TARGET_RE.fullmatch(target):
            raise RegistryError(
                "destroy-volume cleanup target must be STORAGE:vm-VMID-disk-N"
            )
    elif action == "delete-snapshot":
        match = SNAPSHOT_CLEANUP_TARGET_RE.fullmatch(target)
        if match is None:
            if "@" in target:
                _, candidate_snapshot = target.split("@", 1)
                if candidate_snapshot.startswith("__replicate_"):
                    validate_snapshot_name(candidate_snapshot)
            raise RegistryError(
                "delete-snapshot cleanup target must be vm-VMID@SNAPSHOT"
            )
        validate_snapshot_name(match.group(2))
    elif action == "remove-replication":
        if not REPLICATION_CLEANUP_TARGET_RE.fullmatch(target):
            raise RegistryError(
                "remove-replication cleanup target must be VMID-JOB"
            )
    elif action == "remove-route":
        if not RESOURCE_RE.fullmatch(target):
            raise RegistryError("remove-route cleanup target must be a resource name")
    else:
        raise RegistryError(f"unsupported cleanup action: {action}")
    return target


def validate_safe_identifier(value: str, description: str) -> str:
    if not SAFE_ID_RE.fullmatch(value):
        raise RegistryError(f"{description} contains unsafe characters")
    return value


def validate_sha256(value: Any, description: str) -> str:
    if not isinstance(value, str) or not SHA256_RE.fullmatch(value):
        raise RegistryError(f"{description} must be a lowercase SHA-256")
    return value


def validate_ingress_generation_record(value: Any) -> None:
    if not isinstance(value, dict):
        raise RegistryError("ingress generation record must be an object")
    require_exact_keys(
        value, INGRESS_GENERATION_KEYS, "ingress generation record"
    )
    if value["schema_version"] != SCHEMA_VERSION:
        raise RegistryError("ingress generation schema version is unsupported")
    if value["record_type"] != "haproxy-generation":
        raise RegistryError("ingress generation has the wrong record_type")
    validate_sha256(value["generation"], "ingress generation")
    validate_sha256(value["bundle_sha256"], "ingress bundle digest")
    if (
        isinstance(value["route_count"], bool)
        or not isinstance(value["route_count"], int)
        or not 0 <= value["route_count"] <= MAX_ROUTES
    ):
        raise RegistryError(
            f"ingress route_count must be between 0 and {MAX_ROUTES}"
        )
    parse_timestamp(value["created_at"])


def validate_ingress_desired_record(
    value: Any, policy: dict[str, Any]
) -> None:
    if not isinstance(value, dict):
        raise RegistryError("ingress desired record must be an object")
    require_exact_keys(value, INGRESS_DESIRED_KEYS, "ingress desired record")
    if value["schema_version"] != SCHEMA_VERSION:
        raise RegistryError("ingress desired schema version is unsupported")
    if value["record_type"] != "haproxy-desired":
        raise RegistryError("ingress desired record has the wrong record_type")
    validate_sha256(value["generation"], "desired ingress generation")
    validate_sha256(value["bundle_sha256"], "desired ingress bundle digest")
    if (
        isinstance(value["route_count"], bool)
        or not isinstance(value["route_count"], int)
        or not 0 <= value["route_count"] <= MAX_ROUTES
    ):
        raise RegistryError(
            f"desired ingress route_count must be between 0 and {MAX_ROUTES}"
        )
    expected_nodes = [
        f"mox{index}"
        for index in range(1, policy["limits"]["max_hosts"] + 1)
    ]
    if value["nodes"] != expected_nodes:
        raise RegistryError(
            "desired ingress nodes must contain every configured mox in order"
        )
    if not isinstance(value["revision"], int) or value["revision"] < 1:
        raise RegistryError("ingress desired revision must be a positive integer")
    created = parse_timestamp(value["created_at"])
    updated = parse_timestamp(value["updated_at"])
    if updated < created:
        raise RegistryError("ingress desired updated_at predates created_at")


def validate_ingress_node_status(
    value: Any, policy: dict[str, Any]
) -> None:
    if not isinstance(value, dict):
        raise RegistryError("ingress node status must be an object")
    require_exact_keys(
        value, INGRESS_NODE_STATUS_KEYS, "ingress node status"
    )
    if value["schema_version"] != SCHEMA_VERSION:
        raise RegistryError("ingress node status schema version is unsupported")
    if value["record_type"] != "haproxy-node-status":
        raise RegistryError("ingress node status has the wrong record_type")
    validate_nodes([value["node"]], policy, exactly_one=True)
    validate_sha256(
        value["desired_generation"], "node desired ingress generation"
    )
    if value["applied_generation"] is not None:
        validate_sha256(
            value["applied_generation"], "node applied ingress generation"
        )
    if (
        not isinstance(value["state"], str)
        or value["state"] not in INGRESS_NODE_STATES
    ):
        raise RegistryError("ingress node status state is invalid")
    if value["state"] == "applied" and (
        value["applied_generation"] != value["desired_generation"]
    ):
        raise RegistryError(
            "applied ingress node status must match its desired generation"
        )
    if not isinstance(value["revision"], int) or value["revision"] < 1:
        raise RegistryError("ingress node status revision must be positive")
    created = parse_timestamp(value["created_at"])
    updated = parse_timestamp(value["updated_at"])
    if updated < created:
        raise RegistryError("ingress node status updated_at predates created_at")


def validate_orchestration_record(value: Any) -> None:
    if not isinstance(value, dict):
        raise RegistryError("orchestration record must be an object")
    unknown = sorted(
        set(value) - ORCHESTRATION_KEYS - ORCHESTRATION_CONTRACT_KEYS
    )
    missing = sorted(ORCHESTRATION_KEYS - set(value))
    if unknown:
        raise RegistryError(
            "orchestration record has unknown fields: " + ", ".join(unknown)
        )
    if missing:
        raise RegistryError(
            "orchestration record is missing fields: " + ", ".join(missing)
        )
    contract_fields = set(value) & ORCHESTRATION_CONTRACT_KEYS
    if contract_fields and contract_fields not in (
        ORCHESTRATION_LEGACY_CONTRACT_KEYS,
        ORCHESTRATION_CONTRACT_KEYS,
    ):
        raise RegistryError("orchestration record has a partial VM contract")
    if value["schema_version"] != SCHEMA_VERSION:
        raise RegistryError("orchestration record schema version is unsupported")
    if value["record_type"] != "orchestration":
        raise RegistryError("orchestration record has the wrong record_type")
    if not RESOURCE_RE.fullmatch(value["resource"]):
        raise RegistryError("orchestration record has an invalid resource name")
    try:
        uuid.UUID(value["resource_id"])
    except (TypeError, ValueError, AttributeError) as exc:
        raise RegistryError("orchestration resource_id must be a UUID") from exc
    if value["install_phase"] not in INSTALL_PHASES:
        raise RegistryError("orchestration install phase is invalid")
    if contract_fields:
        if not isinstance(value["final_network_enabled"], bool):
            raise RegistryError("final_network_enabled must be boolean")
        startup_sha256 = value["startup_sha256"]
        if startup_sha256 is not None and not re.fullmatch(
            r"[0-9a-f]{64}", str(startup_sha256)
        ):
            raise RegistryError("startup_sha256 must be lowercase SHA-256 or null")
        if contract_fields == ORCHESTRATION_CONTRACT_KEYS:
            if not re.fullmatch(
                r"[0-9a-f]{64}", str(value["source_iso_sha256"])
            ):
                raise RegistryError(
                    "source_iso_sha256 must be lowercase SHA-256"
                )
            if value["install_mode"] not in {"ubuntu-autoinstall", "manual"}:
                raise RegistryError("orchestration install_mode is invalid")
    if not isinstance(value["revision"], int) or value["revision"] < 1:
        raise RegistryError("orchestration revision must be a positive integer")
    created = parse_timestamp(value["created_at"])
    updated = parse_timestamp(value["updated_at"])
    if updated < created:
        raise RegistryError("orchestration updated_at predates created_at")
    lease = value["lease"]
    if lease is not None:
        if not isinstance(lease, dict):
            raise RegistryError("orchestration lease must be an object or null")
        require_exact_keys(lease, LEASE_KEYS, "orchestration lease")
        if not re.fullmatch(r"[0-9a-f]{32}", str(lease["nonce"])):
            raise RegistryError("orchestration lease nonce is invalid")
        validate_safe_identifier(str(lease["owner"]), "orchestration lease owner")
        acquired = parse_timestamp(lease["acquired_at"])
        expires = parse_timestamp(lease["expires_at"])
        if expires <= acquired:
            raise RegistryError("orchestration lease expiry must follow acquisition")


def deterministic_mac(registry_id: str, resource_name: str) -> str:
    digest = bytearray(
        hashlib.sha256(f"{registry_id}\0{resource_name}".encode("ascii")).digest()[:6]
    )
    digest[0] = (digest[0] | 0x02) & 0xFE
    return ":".join(f"{part:02x}" for part in digest)


def request_fingerprint(value: dict[str, Any]) -> str:
    return hashlib.sha256(canonical_json(value)).hexdigest()


def is_pmxcfs_path(path: Path) -> bool:
    """Return whether an absolute path belongs to the pmxcfs hierarchy."""

    try:
        path.relative_to(PMXCFS_ROOT)
    except ValueError:
        return False
    return True


def _fsync_directory(path: Path) -> None:
    # pmxcfs commits mutations through its distributed database and does not
    # provide normal local-filesystem fsync semantics.
    if is_pmxcfs_path(path):
        return
    try:
        descriptor = os.open(path, os.O_RDONLY | getattr(os, "O_DIRECTORY", 0))
    except OSError as exc:
        if exc.errno in {errno.EINVAL, errno.ENOTSUP, errno.EPERM}:
            return
        raise
    try:
        try:
            os.fsync(descriptor)
        except OSError as exc:
            if exc.errno not in {errno.EINVAL, errno.ENOTSUP, errno.EPERM}:
                raise
    finally:
        os.close(descriptor)


def _ensure_no_symlink_components(path: Path) -> None:
    current = Path(path.anchor)
    for part in path.parts[1:]:
        current = current / part
        if current.exists() or current.is_symlink():
            if current.is_symlink():
                raise RegistryError(f"state path may not contain symlink: {current}")


def ensure_directory(path: Path, mode: int = 0o700) -> None:
    if not path.is_absolute():
        raise RegistryError(f"state path must be absolute: {path}")
    _ensure_no_symlink_components(path)
    try:
        path.mkdir(mode=mode, parents=True, exist_ok=True)
    except OSError as exc:
        raise RegistryError(f"cannot create state directory {path}: {exc}") from exc
    if path.is_symlink() or not path.is_dir():
        raise RegistryError(f"state path is not a non-symlink directory: {path}")
    if not is_pmxcfs_path(path):
        try:
            path.chmod(mode)
        except OSError as exc:
            raise RegistryError(f"cannot set mode {mode:o} on {path}: {exc}") from exc


def read_json_file(path: Path, context: str) -> Any:
    try:
        stat_result = path.lstat()
    except FileNotFoundError as exc:
        raise RegistryError(f"{context} does not exist: {path}") from exc
    if path.is_symlink() or not path.is_file():
        raise RegistryError(f"{context} is not a non-symlink regular file: {path}")
    if stat_result.st_size > MAX_JSON_BYTES:
        raise RegistryError(f"{context} exceeds {MAX_JSON_BYTES} bytes: {path}")
    try:
        with path.open("r", encoding="utf-8") as handle:
            value = json.load(handle)
    except (OSError, UnicodeError, json.JSONDecodeError) as exc:
        raise RegistryError(f"cannot read {context} {path}: {exc}") from exc
    reject_secret_material(value, context)
    return value


def atomic_write_json(path: Path, value: Any) -> None:
    reject_secret_material(value, str(path))
    ensure_directory(path.parent)
    data = canonical_json(value)
    if len(data) > MAX_JSON_BYTES:
        raise RegistryError(f"refusing to write oversized JSON record: {path}")
    temporary = path.parent / f".{path.name}.{uuid.uuid4().hex}.tmp"
    flags = os.O_WRONLY | os.O_CREAT | os.O_TRUNC
    if hasattr(os, "O_NOFOLLOW"):
        flags |= os.O_NOFOLLOW
    descriptor: int | None = None
    pmxcfs = is_pmxcfs_path(path)
    try:
        descriptor = os.open(temporary, flags, 0o600)
        with os.fdopen(descriptor, "wb", closefd=True) as handle:
            descriptor = None
            handle.write(data)
            handle.flush()
            if not pmxcfs:
                os.fsync(handle.fileno())
        if not pmxcfs:
            os.chmod(temporary, 0o600)
        os.replace(temporary, path)
        if not pmxcfs:
            os.chmod(path, 0o600)
        _fsync_directory(path.parent)
    except OSError as exc:
        raise RegistryError(f"cannot atomically write {path}: {exc}") from exc
    finally:
        if descriptor is not None:
            os.close(descriptor)
        with contextlib.suppress(FileNotFoundError):
            temporary.unlink()


class AllocatorLock:
    """Cross-node mutation lock implemented with one empty atomic mkdir."""

    def __init__(
        self,
        root: Path,
        timeout: float,
        stale_after: float,
        break_stale: bool,
    ) -> None:
        self.pmxcfs = is_pmxcfs_path(root)
        if self.pmxcfs:
            relative = str(root.relative_to(PMXCFS_ROOT))
            lock_suffix = hashlib.sha256(relative.encode("utf-8")).hexdigest()[:16]
            self.path = PMXCFS_LOCK_DIR / f"app-ha-registry-{lock_suffix}"
        else:
            self.path = root / ".allocator.lock"
        self.owner_path = root / ".allocator-lock-owner.json"
        self.timeout = timeout
        self.stale_after = stale_after
        self.break_stale = break_stale
        self.token = uuid.uuid4().hex
        self.acquired = False

    def _owner(self) -> dict[str, Any]:
        return {
            "schema_version": SCHEMA_VERSION,
            "record_type": "allocator-lock",
            "nonce": self.token,
            "node": socket.gethostname(),
            "pid": os.getpid(),
            "created_at": utc_now(),
        }

    def _is_stale(self) -> bool:
        try:
            age = time.time() - self.path.stat().st_mtime
        except FileNotFoundError:
            return False
        return age >= self.stale_after

    def _break_stale(self) -> bool:
        """Request removal of an old empty lock after explicit operator opt-in."""

        if self.pmxcfs:
            # pmxcfs special-cases mtime zero below priv/lock as a safe,
            # checksum-guarded cluster-wide stale-lock break request. It uses
            # per-node observation age, not potentially skewed wall-clock
            # mtime, before deleting the directory.
            try:
                os.utime(self.path, (0, 0))
            except FileNotFoundError:
                return True
            except OSError as exc:
                raise RegistryError(
                    f"cannot request stale allocator lock removal: {exc}"
                ) from exc
            return not self.path.exists()

        try:
            entries = list(self.path.iterdir())
        except FileNotFoundError:
            return True
        except OSError as exc:
            raise RegistryError(f"cannot inspect stale allocator lock: {exc}") from exc
        if entries:
            raise RegistryError(
                f"refusing to remove non-empty allocator lock directory: {self.path}"
            )
        try:
            self.path.rmdir()
            return True
        except FileNotFoundError:
            return True
        except OSError as exc:
            raise RegistryError(f"cannot remove stale allocator lock: {exc}") from exc

    def _owner_description(self) -> str:
        if not self.owner_path.exists():
            return "unknown"
        try:
            data = read_json_file(self.owner_path, "allocator lock owner")
            return (
                f"{data.get('node', '?')} pid={data.get('pid', '?')} "
                f"since={data.get('created_at', '?')}"
            )
        except RegistryError:
            return "malformed"

    def __enter__(self) -> "AllocatorLock":
        ensure_directory(self.path.parent)
        deadline = time.monotonic() + self.timeout
        while True:
            try:
                os.mkdir(self.path, 0o700)
            except FileExistsError:
                if self.break_stale and (self.pmxcfs or self._is_stale()):
                    if self._break_stale():
                        continue
                if time.monotonic() >= deadline:
                    hint = ""
                    if not self.break_stale and (
                        self.pmxcfs or self._is_stale()
                    ):
                        hint = (
                            "; inspect it, then retry with --break-stale-lock "
                            "to request a safe stale-lock check"
                        )
                    raise RegistryError(
                        "timed out waiting for allocator lock "
                        f"(owner {self._owner_description()}){hint}"
                    )
                time.sleep(0.1)
                continue
            except OSError as exc:
                raise RegistryError(f"cannot create allocator lock: {exc}") from exc
            try:
                atomic_write_json(self.owner_path, self._owner())
            except Exception:
                with contextlib.suppress(FileNotFoundError):
                    self.path.rmdir()
                raise
            self.acquired = True
            return self

    def __exit__(self, exc_type: Any, exc: Any, traceback: Any) -> None:
        if not self.acquired:
            return
        try:
            owner = read_json_file(self.owner_path, "allocator lock owner")
            if owner.get("nonce") != self.token:
                raise RegistryError("allocator lock ownership changed while held")
            self.owner_path.unlink()
            self.path.rmdir()
            _fsync_directory(self.path.parent)
        except FileNotFoundError as cleanup_exc:
            if exc is None:
                raise RegistryError("allocator lock disappeared while held") from cleanup_exc
        finally:
            self.acquired = False


class Registry:
    def __init__(
        self,
        root: Path,
        *,
        lock_timeout: float,
        stale_lock_seconds: float,
        break_stale_lock: bool,
    ) -> None:
        if not root.is_absolute():
            raise RegistryError("--state-dir must be an absolute path")
        self.root = root
        self.policy_path = root / "policy.json"
        self.resources_dir = root / "resources"
        self.cleanup_dir = root / "deferred-cleanup"
        self.history_dir = root / "history"
        self.orchestration_dir = root / "orchestration"
        self.ingress_dir = root / "ingress"
        self.ingress_generations_dir = self.ingress_dir / "generations"
        self.ingress_nodes_dir = self.ingress_dir / "nodes"
        self.ingress_desired_path = self.ingress_dir / "desired.json"
        self.lock_timeout = lock_timeout
        self.stale_lock_seconds = stale_lock_seconds
        self.break_stale_lock = break_stale_lock

    def ensure_layout(self) -> None:
        ensure_directory(self.root)
        ensure_directory(self.resources_dir)
        ensure_directory(self.cleanup_dir)
        ensure_directory(self.history_dir)
        ensure_directory(self.orchestration_dir)
        ensure_directory(self.ingress_dir)
        ensure_directory(self.ingress_generations_dir)
        ensure_directory(self.ingress_nodes_dir)

    def lock(self) -> AllocatorLock:
        self.ensure_layout()
        return AllocatorLock(
            self.root,
            self.lock_timeout,
            self.stale_lock_seconds,
            self.break_stale_lock,
        )

    def policy(self) -> dict[str, Any]:
        value = read_json_file(self.policy_path, "registry policy")
        validate_policy(value)
        return value

    def resource_path(self, name: str) -> Path:
        if not RESOURCE_RE.fullmatch(name):
            raise RegistryError(f"invalid resource name: {name!r}")
        return self.resources_dir / f"{name}.json"

    def cleanup_path(self, cleanup_id: str) -> Path:
        validate_safe_identifier(cleanup_id, "cleanup id")
        return self.cleanup_dir / f"{cleanup_id}.json"

    def orchestration_path(self, name: str) -> Path:
        if not RESOURCE_RE.fullmatch(name):
            raise RegistryError(f"invalid resource name: {name!r}")
        return self.orchestration_dir / f"{name}.json"

    def ingress_generation_path(self, generation: str) -> Path:
        validate_sha256(generation, "ingress generation")
        return self.ingress_generations_dir / f"{generation}.json"

    def ingress_node_path(self, node: str) -> Path:
        if not MOX_RE.fullmatch(node):
            raise RegistryError(f"invalid ingress node name: {node!r}")
        return self.ingress_nodes_dir / f"{node}.json"

    def ingress_desired(self) -> dict[str, Any] | None:
        if not self.ingress_desired_path.exists():
            return None
        policy = self.policy()
        desired = read_json_file(
            self.ingress_desired_path, "desired ingress generation"
        )
        validate_ingress_desired_record(desired, policy)
        generation_path = self.ingress_generation_path(desired["generation"])
        generation = read_json_file(
            generation_path, "desired ingress generation metadata"
        )
        validate_ingress_generation_record(generation)
        if any(
            generation[field] != desired[field]
            for field in ("generation", "bundle_sha256", "route_count")
        ):
            raise RegistryError(
                "desired ingress pointer differs from generation metadata"
            )
        return desired

    def ingress_node_statuses(self) -> list[dict[str, Any]]:
        policy = self.policy()
        records: list[dict[str, Any]] = []
        if not self.ingress_nodes_dir.exists():
            return records
        for path in sorted(self.ingress_nodes_dir.glob("*.json")):
            record = read_json_file(path, "ingress node status")
            validate_ingress_node_status(record, policy)
            if path.name != f"{record['node']}.json":
                raise RegistryError(
                    f"ingress node status filename does not match record: {path}"
                )
            records.append(record)
        return sorted(records, key=lambda record: int(record["node"][3:]))

    def prune_ingress_generations(
        self,
        current_generation: str,
        keep: int = MAX_INGRESS_GENERATIONS,
    ) -> None:
        """Bound immutable ingress-generation metadata while retaining desired."""

        if keep < 1:
            raise RegistryError("ingress generation retention must be positive")
        validate_sha256(current_generation, "current ingress generation")
        records: list[tuple[str, str, Path]] = []
        for path in sorted(self.ingress_generations_dir.glob("*.json")):
            record = read_json_file(path, "ingress generation metadata")
            validate_ingress_generation_record(record)
            if path.name != f"{record['generation']}.json":
                raise RegistryError(
                    f"ingress generation filename does not match record: {path}"
                )
            records.append((record["created_at"], record["generation"], path))
        retained = {
            current_generation,
            *list(
                generation
                for _created, generation, _path in sorted(records, reverse=True)
                if generation != current_generation
            )[: keep - 1],
        }
        removed = False
        for _created, generation, path in records:
            if generation not in retained:
                path.unlink()
                removed = True
        if removed:
            _fsync_directory(self.ingress_generations_dir)

    def resources(self) -> list[dict[str, Any]]:
        policy = self.policy()
        if not self.resources_dir.exists():
            return []
        records: list[dict[str, Any]] = []
        for path in sorted(self.resources_dir.glob("*.json")):
            record = read_json_file(path, "resource record")
            validate_resource(record, policy)
            if path.name != f"{record['name']}.json":
                raise RegistryError(f"resource filename does not match record name: {path}")
            records.append(record)
        validate_registry_invariants(records)
        return sorted(records, key=resource_sort_key)

    def cleanup_records(self) -> list[dict[str, Any]]:
        if not self.cleanup_dir.exists():
            return []
        records: list[dict[str, Any]] = []
        for path in sorted(self.cleanup_dir.glob("*.json")):
            record = read_json_file(path, "deferred cleanup record")
            validate_cleanup_record(record)
            if path.name != f"{record['id']}.json":
                raise RegistryError(f"cleanup filename does not match record id: {path}")
            records.append(record)
        return sorted(records, key=lambda record: (record["created_at"], record["id"]))

    def prune_history(self, keep: int = MAX_HISTORY_RECORDS) -> None:
        """Keep only the newest bounded release-audit records."""

        if keep < 0:
            raise RegistryError("history retention may not be negative")
        paths = sorted(self.history_dir.glob("*.json"))
        remove = paths if keep == 0 else paths[:-keep]
        for path in remove:
            if path.is_symlink() or not path.is_file():
                raise RegistryError(f"history entry is not a regular file: {path}")
            path.unlink()
        if remove:
            _fsync_directory(self.history_dir)

    def prune_completed_cleanup(self, active_resource_ids: set[str]) -> None:
        """Bound completed records once their resource identity is gone."""

        completed = [
            record
            for record in self.cleanup_records()
            if record["state"] == "completed"
        ]
        excess = len(completed) - MAX_COMPLETED_CLEANUP_RECORDS
        if excess <= 0:
            return
        removable = [
            record
            for record in completed
            if record["resource_id"] not in active_resource_ids
        ]
        removed = 0
        for record in removable[:excess]:
            self.cleanup_path(record["id"]).unlink()
            removed += 1
        if removed:
            _fsync_directory(self.cleanup_dir)

    def get_resource(self, name: str) -> dict[str, Any]:
        policy = self.policy()
        record = read_json_file(self.resource_path(name), "resource record")
        validate_resource(record, policy)
        return record


def build_policy(args: argparse.Namespace) -> dict[str, Any]:
    try:
        network = ipaddress.ip_network(args.network, strict=True)
        gateway = ipaddress.ip_interface(args.guest_gateway)
    except ValueError as exc:
        raise RegistryError(f"invalid policy network: {exc}") from exc
    if not isinstance(network, ipaddress.IPv4Network) or network.prefixlen != 24:
        raise RegistryError("policy network must be an IPv4 /24")
    if gateway.network != network:
        raise RegistryError("guest gateway must belong to the policy /24")
    if not 1 <= args.max_hosts <= 10:
        raise RegistryError("max hosts must be between 1 and 10")
    if args.reservation_ttl_seconds < 60:
        raise RegistryError("reservation TTL must be at least 60 seconds")

    def address_octet(value: str, label: str) -> int:
        try:
            address = ipaddress.ip_address(value)
        except ValueError as exc:
            raise RegistryError(f"invalid {label}: {exc}") from exc
        if not isinstance(address, ipaddress.IPv4Address) or address not in network:
            raise RegistryError(f"{label} must belong to the policy /24")
        octet = int(address) - int(network.network_address)
        if not 1 <= octet <= 254:
            raise RegistryError(f"{label} must be a usable host address")
        return octet

    ranges: dict[str, dict[str, int]] = {}
    for name, start_value, end_value in (
        ("mox_host_range", args.mox_ip_start, args.mox_ip_end),
        ("haproxy_range", args.haproxy_ip_start, args.haproxy_ip_end),
        ("production_range", args.production_ip_start, args.production_ip_end),
        ("staging_range", args.staging_ip_start, args.staging_ip_end),
    ):
        start = address_octet(start_value, f"{name} start")
        end = address_octet(end_value, f"{name} end")
        if start > end:
            raise RegistryError(f"{name} must be an ascending range")
        ranges[name] = {"start": start, "end": end}

    range_names = list(ranges)
    for index, name in enumerate(range_names):
        current = set(range(ranges[name]["start"], ranges[name]["end"] + 1))
        for other in range_names[index + 1 :]:
            candidate = set(range(ranges[other]["start"], ranges[other]["end"] + 1))
            if current & candidate:
                raise RegistryError(f"{name} and {other} overlap")
    gateway_octet = int(gateway.ip) - int(network.network_address)
    if not 1 <= gateway_octet <= 254:
        raise RegistryError("guest gateway must be a usable host address")
    if any(
        value["start"] <= gateway_octet <= value["end"]
        for value in ranges.values()
    ):
        raise RegistryError("guest gateway overlaps an allocation range")
    if ranges["mox_host_range"]["end"] - ranges["mox_host_range"]["start"] + 1 < args.max_hosts:
        raise RegistryError("mox host range is smaller than max hosts")
    if ranges["haproxy_range"]["end"] - ranges["haproxy_range"]["start"] + 1 < args.max_hosts:
        raise RegistryError("HAProxy range is smaller than max hosts")

    tags = {
        "production": validate_safe_identifier(args.production_tag, "production tag"),
        "staging": validate_safe_identifier(args.staging_tag, "staging tag"),
        "evictable": validate_safe_identifier(args.evictable_tag, "evictable tag"),
    }
    if len(set(tags.values())) != 3:
        raise RegistryError("production, staging, and evictable tags must be distinct")

    policy = {
        "schema_version": SCHEMA_VERSION,
        "record_type": "policy",
        "registry_id": str(uuid.uuid4()),
        "network": {
            "cidr": str(network),
            "guest_gateway": str(gateway),
            **ranges,
            "haproxy_vmid_range": {"start": 9111, "end": 9120},
        },
        "limits": {
            "max_hosts": args.max_hosts,
            "max_production": (
                ranges["production_range"]["end"]
                - ranges["production_range"]["start"]
                + 1
            ),
            "max_staging": (
                ranges["staging_range"]["end"]
                - ranges["staging_range"]["start"]
                + 1
            ),
            "reservation_ttl_seconds": args.reservation_ttl_seconds,
        },
        "tags": tags,
        "defaults": {
            "production": {
                "cores": args.prod_cores,
                "memory_mb": args.prod_memory_mb,
                "disk_gib": args.prod_disk_gib,
                "disk_allocation": args.prod_disk_allocation,
                "replication_interval": args.replication_interval,
            },
            "staging": {
                "cores": args.staging_cores,
                "memory_mb": args.staging_memory_mb,
                "disk_gib": args.staging_disk_gib,
                "disk_allocation": "sparse",
                "replication_interval": args.replication_interval,
            },
        },
        "state_transitions": {
            kind: {
                state: sorted(destinations)
                for state, destinations in sorted(transitions.items())
            }
            for kind, transitions in sorted(STATE_TRANSITIONS.items())
        },
        "created_at": utc_now(),
    }
    validate_policy(policy)
    return policy


def validate_policy(policy: Any) -> None:
    if not isinstance(policy, dict):
        raise RegistryError("registry policy must be a JSON object")
    require_exact_keys(
        policy,
        {
            "schema_version",
            "record_type",
            "registry_id",
            "network",
            "limits",
            "tags",
            "defaults",
            "state_transitions",
            "created_at",
        },
        "registry policy",
    )
    if policy["schema_version"] != SCHEMA_VERSION or policy["record_type"] != "policy":
        raise RegistryError("unsupported registry policy schema")
    try:
        uuid.UUID(policy["registry_id"])
    except (TypeError, ValueError, AttributeError) as exc:
        raise RegistryError("policy registry_id is not a UUID") from exc
    parse_timestamp(policy["created_at"])

    network_data = policy["network"]
    if not isinstance(network_data, dict):
        raise RegistryError("policy network must be an object")
    require_exact_keys(
        network_data,
        {
            "cidr",
            "guest_gateway",
            "mox_host_range",
            "haproxy_range",
            "production_range",
            "staging_range",
            "haproxy_vmid_range",
        },
        "policy network",
    )
    try:
        network = ipaddress.ip_network(network_data["cidr"], strict=True)
        gateway = ipaddress.ip_interface(network_data["guest_gateway"])
    except (TypeError, ValueError) as exc:
        raise RegistryError(f"invalid policy network: {exc}") from exc
    if (
        not isinstance(network, ipaddress.IPv4Network)
        or network.prefixlen != 24
        or gateway.network != network
    ):
        raise RegistryError("policy network and guest gateway must share an IPv4 /24")
    allocation_ranges: dict[str, set[int]] = {}
    for name in (
        "mox_host_range",
        "haproxy_range",
        "production_range",
        "staging_range",
    ):
        value = network_data[name]
        if (
            not isinstance(value, dict)
            or set(value) != {"start", "end"}
            or isinstance(value["start"], bool)
            or isinstance(value["end"], bool)
            or not isinstance(value["start"], int)
            or not isinstance(value["end"], int)
            or not 1 <= value["start"] <= value["end"] <= 254
        ):
            raise RegistryError(f"policy {name} is not a usable host-octet range")
        allocation_ranges[name] = set(range(value["start"], value["end"] + 1))
    if network_data["haproxy_vmid_range"] != {"start": 9111, "end": 9120}:
        raise RegistryError("policy haproxy_vmid_range must be 9111-9120")
    range_names = list(allocation_ranges)
    for index, name in enumerate(range_names):
        for other in range_names[index + 1 :]:
            if allocation_ranges[name] & allocation_ranges[other]:
                raise RegistryError(f"policy {name} and {other} overlap")
    gateway_octet = int(gateway.ip) - int(network.network_address)
    if (
        not 1 <= gateway_octet <= 254
        or gateway.network.prefixlen != 24
        or any(gateway_octet in values for values in allocation_ranges.values())
    ):
        raise RegistryError("policy guest gateway is invalid or overlaps an allocation range")

    limits = policy["limits"]
    require_exact_keys(
        limits,
        {
            "max_hosts",
            "max_production",
            "max_staging",
            "reservation_ttl_seconds",
        },
        "policy limits",
    )
    if not isinstance(limits["max_hosts"], int) or not 1 <= limits["max_hosts"] <= 10:
        raise RegistryError("policy max_hosts must be between 1 and 10")
    if (
        len(allocation_ranges["mox_host_range"]) < limits["max_hosts"]
        or len(allocation_ranges["haproxy_range"]) < limits["max_hosts"]
    ):
        raise RegistryError("policy host allocation ranges are too small")
    if (
        limits["max_production"] != len(allocation_ranges["production_range"])
        or limits["max_staging"] != len(allocation_ranges["staging_range"])
    ):
        raise RegistryError("policy allocation limits do not match IP ranges")
    if (
        not isinstance(limits["reservation_ttl_seconds"], int)
        or limits["reservation_ttl_seconds"] < 60
    ):
        raise RegistryError("policy reservation TTL is invalid")

    tags = policy["tags"]
    require_exact_keys(tags, {"production", "staging", "evictable"}, "policy tags")
    for name, value in tags.items():
        validate_safe_identifier(value, f"{name} tag")
    if len(set(tags.values())) != 3:
        raise RegistryError("policy tags must be distinct")

    defaults = policy["defaults"]
    require_exact_keys(defaults, {"production", "staging"}, "policy defaults")
    for kind in ("production", "staging"):
        spec = defaults[kind]
        require_exact_keys(
            spec,
            {
                "cores",
                "memory_mb",
                "disk_gib",
                "disk_allocation",
                "replication_interval",
            },
            f"policy {kind} defaults",
        )
        validate_spec(spec)

    expected_transitions = {
        kind: {
            state: sorted(destinations)
            for state, destinations in sorted(transitions.items())
        }
        for kind, transitions in sorted(STATE_TRANSITIONS.items())
    }
    if policy["state_transitions"] != expected_transitions:
        raise RegistryError("policy state transitions differ from this schema version")
    reject_secret_material(policy, "registry policy")


def validate_spec(spec: Any) -> None:
    if not isinstance(spec, dict):
        raise RegistryError("resource spec must be an object")
    require_exact_keys(
        spec,
        {
            "cores",
            "memory_mb",
            "disk_gib",
            "disk_allocation",
            "replication_interval",
        },
        "resource spec",
    )
    for field in ("cores", "memory_mb", "disk_gib"):
        if not isinstance(spec[field], int) or spec[field] <= 0:
            raise RegistryError(f"resource spec {field} must be a positive integer")
    if spec["disk_allocation"] not in {"sparse", "reserved"}:
        raise RegistryError("disk allocation must be sparse or reserved")
    if not re.fullmatch(r"[1-9][0-9]*", str(spec["replication_interval"])):
        raise RegistryError("replication interval must be a positive minute count")


def validate_resource(resource: Any, policy: dict[str, Any]) -> None:
    if not isinstance(resource, dict):
        raise RegistryError("resource record must be a JSON object")
    require_exact_keys(resource, RESOURCE_KEYS, "resource record")
    if (
        resource["schema_version"] != SCHEMA_VERSION
        or resource["record_type"] != "resource"
    ):
        raise RegistryError("unsupported resource schema")
    try:
        uuid.UUID(resource["id"])
    except (TypeError, ValueError, AttributeError) as exc:
        raise RegistryError("resource id is not a UUID") from exc
    normalize_allocation_id(resource["allocation_id"])
    if not re.fullmatch(r"[0-9a-f]{64}", resource["request_fingerprint"]):
        raise RegistryError("resource request fingerprint is invalid")
    if resource["kind"] not in {"production", "staging"}:
        raise RegistryError("resource kind must be production or staging")
    if not isinstance(resource["index"], int) or resource["index"] <= 0:
        raise RegistryError("resource index must be a positive integer")
    match = RESOURCE_RE.fullmatch(resource["name"])
    if not match:
        raise RegistryError(f"invalid resource name: {resource['name']!r}")
    validate_vmid(resource["vmid"], policy)
    try:
        ip = ipaddress.ip_address(resource["ip"])
    except (TypeError, ValueError) as exc:
        raise RegistryError("resource IP is invalid") from exc
    network = ipaddress.ip_network(policy["network"]["cidr"])
    if ip not in network:
        raise RegistryError("resource IP is outside policy network")
    octet = int(ip) - int(network.network_address)
    if not MAC_RE.fullmatch(resource["mac"]):
        raise RegistryError("resource MAC is invalid or not normalized")
    if resource["state"] not in RESOURCE_STATES:
        raise RegistryError(f"unknown resource state: {resource['state']!r}")
    if not isinstance(resource["revision"], int) or resource["revision"] < 1:
        raise RegistryError("resource revision must be positive")
    parse_timestamp(resource["created_at"])
    parse_timestamp(resource["updated_at"])

    purpose = resource["purpose"]
    require_exact_keys(purpose, {"slug", "source_resource"}, "resource purpose")
    normalize_purpose(purpose["slug"])

    domains = resource["domains"]
    require_exact_keys(domains, {"primary", "aliases", "staging_base"}, "resource domains")
    primary = normalize_domain(domains["primary"])
    if primary != domains["primary"]:
        raise RegistryError("resource primary domain is not normalized")
    aliases = normalize_domains(domains["aliases"])
    if aliases != domains["aliases"] or primary in aliases:
        raise RegistryError("resource aliases are not normalized and unique")
    staging_base = normalize_domain(domains["staging_base"])
    if staging_base != domains["staging_base"]:
        raise RegistryError("resource staging base is not normalized")
    if staging_base in {primary, *aliases}:
        raise RegistryError("resource staging base must differ from routed domains")

    placement = validate_nodes(resource["placement"], policy)
    if placement != resource["placement"]:
        raise RegistryError("resource placement must be a JSON array")
    if resource["initial_node"] not in placement:
        raise RegistryError("resource initial node must belong to placement")
    if resource["owner_node"] is not None and resource["owner_node"] not in placement:
        raise RegistryError("resource owner node must belong to placement")
    if not isinstance(resource["routes_enabled"], bool):
        raise RegistryError("routes_enabled must be boolean")
    if resource["routes_enabled"] and resource["state"] not in ROUTABLE_STATES:
        raise RegistryError("routes may only be enabled for stopped, ready, or active guests")
    validate_spec(resource["spec"])

    proxmox = resource["proxmox"]
    require_exact_keys(
        proxmox,
        {"ha_nodes", "replication_targets", "volume_id", "snapshot"},
        "resource proxmox metadata",
    )
    ha_nodes = validate_nodes(proxmox["ha_nodes"], policy, allow_empty=True)
    replication_targets = validate_nodes(
        proxmox["replication_targets"], policy, allow_empty=True
    )
    if ha_nodes != proxmox["ha_nodes"] or replication_targets != proxmox["replication_targets"]:
        raise RegistryError("Proxmox node metadata must be JSON arrays")
    if any(node not in placement for node in ha_nodes + replication_targets):
        raise RegistryError("Proxmox HA/replication metadata exceeds placement")
    if proxmox["volume_id"] is not None:
        validate_safe_identifier(proxmox["volume_id"], "volume id")
    snapshot = proxmox["snapshot"]
    if snapshot is not None:
        if not isinstance(snapshot, dict):
            raise RegistryError("snapshot metadata must be an object or null")
        snapshot_keys = {"name", "owner_node", "guids"}
        if resource["kind"] == "staging":
            snapshot_keys.update(
                {
                    "source_resource",
                    "source_volume_id",
                    "source_volume_ids",
                    "volume_guids",
                    "verified",
                    "dependent_resources",
                    "refcount",
                }
            )
        require_exact_keys(snapshot, snapshot_keys, "snapshot metadata")
        validate_snapshot_name(snapshot["name"])
        validate_nodes([snapshot["owner_node"]], policy)
        if (
            resource["kind"] == "production"
            and snapshot["owner_node"] not in placement
        ):
            raise RegistryError("snapshot owner node must belong to placement")
        if not isinstance(snapshot["guids"], dict):
            raise RegistryError("snapshot GUIDs must be an object")
        for node, guid in snapshot["guids"].items():
            validate_nodes([node], policy)
            if (
                resource["kind"] == "production"
                and node not in placement
            ) or not GUID_RE.fullmatch(str(guid)):
                raise RegistryError("snapshot GUID metadata is invalid")
        if resource["kind"] == "staging":
            if snapshot["source_resource"] != resource["source"]:
                raise RegistryError(
                    "staging snapshot source must match the resource source"
                )
            validate_safe_identifier(
                snapshot["source_volume_id"], "snapshot source volume id"
            )
            source_volume_ids = snapshot["source_volume_ids"]
            if (
                not isinstance(source_volume_ids, list)
                or not source_volume_ids
                or len(source_volume_ids) != len(set(source_volume_ids))
            ):
                raise RegistryError(
                    "staging snapshot source volumes must be a non-empty unique array"
                )
            for volume_id in source_volume_ids:
                validate_safe_identifier(volume_id, "snapshot source volume id")
            if snapshot["source_volume_id"] not in source_volume_ids:
                raise RegistryError(
                    "staging root source volume must be among snapshotted volumes"
                )
            if not isinstance(snapshot["verified"], bool):
                raise RegistryError("staging snapshot verified flag must be boolean")
            volume_guids = snapshot["volume_guids"]
            if not isinstance(volume_guids, dict):
                raise RegistryError("staging snapshot volume GUIDs must be an object")
            if snapshot["verified"]:
                if set(volume_guids) != set(source_volume_ids):
                    raise RegistryError(
                        "verified staging snapshot GUIDs must cover every source volume"
                    )
                for volume_id, node_guids in volume_guids.items():
                    if not isinstance(node_guids, dict) or not node_guids:
                        raise RegistryError(
                            f"snapshot GUIDs for {volume_id} must be a non-empty object"
                        )
                    for node, guid in node_guids.items():
                        validate_nodes([node], policy)
                        if not GUID_RE.fullmatch(str(guid)):
                            raise RegistryError(
                                f"snapshot GUID metadata is invalid for {volume_id}"
                            )
                    if len(set(node_guids.values())) != 1:
                        raise RegistryError(
                            f"snapshot GUID differs between nodes for {volume_id}"
                        )
                if snapshot["guids"] != volume_guids[snapshot["source_volume_id"]]:
                    raise RegistryError(
                        "root snapshot GUIDs must match the root volume GUID map"
                    )
            elif snapshot["guids"] or volume_guids:
                raise RegistryError(
                    "unverified staging snapshot intent may not claim GUIDs"
                )
            dependents = snapshot["dependent_resources"]
            if not isinstance(dependents, list) or dependents != [resource["name"]]:
                raise RegistryError(
                    "a unique staging snapshot must name its one dependent resource"
                )
            if (
                isinstance(snapshot["refcount"], bool)
                or not isinstance(snapshot["refcount"], int)
                or snapshot["refcount"] != len(dependents)
                or snapshot["refcount"] != 1
            ):
                raise RegistryError(
                    "a unique staging snapshot refcount must be exactly one"
                )

    if resource["kind"] == "production":
        if match.group("production") is None or resource["source"] is not None:
            raise RegistryError("production resource has staging identity")
        if int(match.group("production")) != resource["index"]:
            raise RegistryError("production resource index does not match its name")
        production_range = policy["network"]["production_range"]
        expected_octet = production_range["start"] + resource["index"] - 1
        if expected_octet > production_range["end"] or octet != expected_octet:
            raise RegistryError("production resource name/IP formula is invalid")
        if purpose["source_resource"] is not None:
            raise RegistryError("production purpose may not have a source resource")
    else:
        if (
            match.group("stage") is None
            or resource["source"] != match.group("source")
        ):
            raise RegistryError("staging source/name relationship is invalid")
        if int(match.group("stage")) != resource["index"]:
            raise RegistryError("staging index does not match its name")
        staging_range = policy["network"]["staging_range"]
        if not staging_range["start"] <= octet <= staging_range["end"]:
            raise RegistryError("staging IP is outside its configured range")
        if purpose["source_resource"] != resource["source"]:
            raise RegistryError("staging purpose source does not match source resource")
        if len(placement) != 1:
            raise RegistryError("staging placement must contain exactly one node")
        if ha_nodes:
            raise RegistryError("staging resources may not have HA nodes")
    reject_secret_material(resource, f"resource {resource['name']}")


def validate_registry_invariants(resources: Sequence[dict[str, Any]]) -> None:
    unique_fields: dict[str, dict[Any, str]] = {
        "name": {},
        "vmid": {},
        "ip": {},
        "mac": {},
        "domain": {},
        "allocation_id": {},
        "production purpose": {},
    }
    by_name: dict[str, dict[str, Any]] = {}
    for resource in resources:
        name = resource["name"]
        by_name[name] = resource
        for field in ("name", "vmid", "ip", "mac"):
            value = resource[field]
            prior = unique_fields[field].get(value)
            if prior:
                raise RegistryError(f"duplicate {field} {value!r}: {prior} and {name}")
            unique_fields[field][value] = name
        allocation_id = resource["allocation_id"]
        if allocation_id is not None:
            prior = unique_fields["allocation_id"].get(allocation_id)
            if prior:
                raise RegistryError(
                    f"duplicate allocation id {allocation_id!r}: {prior} and {name}"
                )
            unique_fields["allocation_id"][allocation_id] = name
        if resource["kind"] == "production":
            purpose = resource["purpose"]["slug"]
            prior = unique_fields["production purpose"].get(purpose)
            if prior:
                raise RegistryError(
                    f"duplicate production purpose {purpose!r}: {prior} and {name}"
                )
            unique_fields["production purpose"][purpose] = name
        for domain in [resource["domains"]["primary"], *resource["domains"]["aliases"]]:
            prior = unique_fields["domain"].get(domain)
            if prior:
                raise RegistryError(f"duplicate domain {domain!r}: {prior} and {name}")
            unique_fields["domain"][domain] = name
    if len(unique_fields["domain"]) > MAX_ROUTES:
        raise RegistryError(
            f"registry route count exceeds renderer limit of {MAX_ROUTES}"
        )
    staging_snapshot_keys: dict[tuple[str, str], str] = {}
    for resource in resources:
        if resource["kind"] != "staging":
            continue
        source = by_name.get(resource["source"])
        if source is None:
            raise RegistryError(
                f"staging resource {resource['name']} has missing source {resource['source']}"
            )
        if source["kind"] != "production":
            raise RegistryError(
                f"staging resource {resource['name']} source is not production"
            )
        snapshot = resource["proxmox"]["snapshot"]
        if snapshot is None:
            continue
        if snapshot["source_resource"] != source["name"]:
            raise RegistryError("staging snapshot dependency source is inconsistent")
        if snapshot["source_volume_id"] != source["proxmox"]["volume_id"]:
            raise RegistryError(
                "staging snapshot dependency source volume differs from production"
            )
        if snapshot["owner_node"] not in source["placement"]:
            raise RegistryError(
                "staging snapshot owner is outside production placement"
            )
        for volume_id in snapshot["source_volume_ids"]:
            storage, _separator, _volume = volume_id.partition(":")
            if storage == "":
                raise RegistryError("staging snapshot source volume lacks storage id")
        if snapshot["verified"]:
            if set(snapshot["guids"]) != set(source["placement"]):
                raise RegistryError(
                    "staging snapshot GUIDs must cover every production placement node"
                )
            for volume_id, node_guids in snapshot["volume_guids"].items():
                if set(node_guids) != set(source["placement"]):
                    raise RegistryError(
                        f"snapshot GUIDs for {volume_id} must cover production placement"
                    )
        elif resource["state"] not in {
            "snapshotting",
            "replicating",
            "cleanup_pending",
            "failed",
        }:
            raise RegistryError(
                "unverified staging snapshot intent is invalid in the current state"
            )
        snapshot_key = (source["name"], snapshot["name"])
        prior = staging_snapshot_keys.get(snapshot_key)
        if prior is not None:
            raise RegistryError(
                f"staging snapshot {snapshot['name']!r} is shared by "
                f"{prior} and {resource['name']}; unique snapshots are required"
            )
        staging_snapshot_keys[snapshot_key] = resource["name"]


def validate_cleanup_record(record: Any) -> None:
    if not isinstance(record, dict):
        raise RegistryError("cleanup record must be an object")
    require_exact_keys(
        record,
        {
            "schema_version",
            "record_type",
            "id",
            "resource",
            "resource_id",
            "node",
            "action",
            "target",
            "reason",
            "state",
            "attempts",
            "created_at",
            "updated_at",
            "revision",
        },
        "cleanup record",
    )
    if (
        record["schema_version"] != SCHEMA_VERSION
        or record["record_type"] != "deferred-cleanup"
    ):
        raise RegistryError("unsupported cleanup record schema")
    validate_safe_identifier(record["id"], "cleanup id")
    if not RESOURCE_RE.fullmatch(record["resource"]):
        raise RegistryError("cleanup resource name is invalid")
    try:
        uuid.UUID(record["resource_id"])
    except (TypeError, ValueError, AttributeError) as exc:
        raise RegistryError("cleanup resource_id is not a UUID") from exc
    if not MOX_RE.fullmatch(record["node"]):
        raise RegistryError("cleanup node name is invalid")
    if record["action"] not in CLEANUP_ACTIONS:
        raise RegistryError("cleanup action is invalid")
    validate_cleanup_target(record["action"], record["target"])
    if (
        not isinstance(record["reason"], str)
        or len(record["reason"]) > 240
        or "\n" in record["reason"]
    ):
        raise RegistryError("cleanup reason is invalid")
    if record["state"] not in {"pending", "completed"}:
        raise RegistryError("cleanup state is invalid")
    if not isinstance(record["attempts"], int) or record["attempts"] < 0:
        raise RegistryError("cleanup attempts is invalid")
    if not isinstance(record["revision"], int) or record["revision"] < 1:
        raise RegistryError("cleanup revision is invalid")
    parse_timestamp(record["created_at"])
    parse_timestamp(record["updated_at"])
    reject_secret_material(record, "cleanup record")


def resource_sort_key(resource: dict[str, Any]) -> tuple[int, int, str]:
    match = RESOURCE_RE.fullmatch(resource["name"])
    assert match is not None
    index = match.group("production") or match.group("stage")
    assert index is not None
    return (
        0 if resource["kind"] == "production" else 1,
        int(index),
        resource["name"],
    )


def effective_spec(
    args: argparse.Namespace, policy: dict[str, Any], kind: str
) -> dict[str, Any]:
    defaults = policy["defaults"][kind]
    spec = {
        "cores": args.cores if args.cores is not None else defaults["cores"],
        "memory_mb": (
            args.memory_mb if args.memory_mb is not None else defaults["memory_mb"]
        ),
        "disk_gib": args.disk_gib if args.disk_gib is not None else defaults["disk_gib"],
        "disk_allocation": (
            args.disk_allocation
            if args.disk_allocation is not None
            else defaults["disk_allocation"]
        ),
        "replication_interval": (
            args.replication_interval
            if args.replication_interval is not None
            else defaults["replication_interval"]
        ),
    }
    validate_spec(spec)
    return spec


def base_resource(
    *,
    policy: dict[str, Any],
    kind: str,
    name: str,
    index: int,
    source: str | None,
    vmid: int,
    ip: str,
    purpose: dict[str, Any],
    domains: dict[str, Any],
    placement: list[str],
    initial_node: str,
    spec: dict[str, Any],
    allocation_id: str | None,
    fingerprint: str,
) -> dict[str, Any]:
    now = utc_now()
    return {
        "schema_version": SCHEMA_VERSION,
        "record_type": "resource",
        "id": str(uuid.uuid4()),
        "allocation_id": allocation_id,
        "request_fingerprint": fingerprint,
        "kind": kind,
        "name": name,
        "index": index,
        "source": source,
        "vmid": vmid,
        "ip": ip,
        "mac": deterministic_mac(policy["registry_id"], name),
        "state": "reserved",
        "purpose": purpose,
        "domains": domains,
        "placement": placement,
        "initial_node": initial_node,
        "owner_node": None,
        "routes_enabled": False,
        "spec": spec,
        "proxmox": {
            "ha_nodes": [],
            "replication_targets": [],
            "volume_id": None,
            "snapshot": None,
        },
        "created_at": now,
        "updated_at": now,
        "revision": 1,
    }


def find_idempotent_allocation(
    resources: Sequence[dict[str, Any]],
    allocation_id: str | None,
    fingerprint: str,
    kind: str,
) -> dict[str, Any] | None:
    if allocation_id is None:
        return None
    for resource in resources:
        if resource["allocation_id"] != allocation_id:
            continue
        if resource["kind"] != kind or resource["request_fingerprint"] != fingerprint:
            raise RegistryError(
                f"allocation id {allocation_id!r} was already used for a different request"
            )
        return resource
    return None


def command_init(registry: Registry, args: argparse.Namespace) -> Any:
    registry.ensure_layout()
    with registry.lock():
        if registry.policy_path.exists():
            policy = registry.policy()
            return {"created": False, "policy": policy}
        policy = build_policy(args)
        atomic_write_json(registry.policy_path, policy)
        return {"created": True, "policy": policy}


def command_list(registry: Registry, args: argparse.Namespace) -> Any:
    if args.record_type == "cleanup":
        records = registry.cleanup_records()
        if args.state:
            records = [record for record in records if record["state"] == args.state]
        return records
    if args.record_type == "history":
        registry.policy()
        result = []
        for path in sorted(registry.history_dir.glob("*.json")):
            result.append(read_json_file(path, "history record"))
        return result
    resources = registry.resources()
    if args.kind:
        resources = [record for record in resources if record["kind"] == args.kind]
    if args.state:
        resources = [record for record in resources if record["state"] == args.state]
    return resources


def command_get(registry: Registry, args: argparse.Namespace) -> Any:
    if args.cleanup:
        record = read_json_file(registry.cleanup_path(args.name), "cleanup record")
        validate_cleanup_record(record)
        return record
    return registry.get_resource(args.name)


def command_allocate_prod(registry: Registry, args: argparse.Namespace) -> Any:
    policy = registry.policy()
    placement = validate_nodes(flatten_nodes(args.placement), policy)
    initial_node = args.initial_node or placement[0]
    if initial_node not in placement:
        raise RegistryError("initial node must belong to production placement")
    purpose_slug = normalize_purpose(args.purpose)
    primary = normalize_domain(args.primary_domain)
    aliases = normalize_domains(args.alias or [])
    if primary in aliases:
        raise RegistryError("primary domain may not also be an alias")
    staging_base = normalize_domain(args.staging_dns_base or f"staging.{primary}")
    if staging_base in {primary, *aliases}:
        raise RegistryError("staging DNS base must differ from production routes")
    allocation_id = normalize_allocation_id(args.allocation_id)
    validate_vmid(args.vmid, policy)
    spec = effective_spec(args, policy, "production")
    request = {
        "kind": "production",
        "vmid": args.vmid,
        "purpose": purpose_slug,
        "primary": primary,
        "aliases": aliases,
        "staging_base": staging_base,
        "placement": placement,
        "initial_node": initial_node,
        "spec": spec,
    }
    fingerprint = request_fingerprint(request)

    with registry.lock():
        resources = registry.resources()
        existing = find_idempotent_allocation(
            resources, allocation_id, fingerprint, "production"
        )
        if existing:
            return {"created": False, "resource": existing}
        used_names = {record["name"] for record in resources}
        index = next(
            (
                candidate
                for candidate in range(1, policy["limits"]["max_production"] + 1)
                if f"prod{candidate}" not in used_names
            ),
            None,
        )
        if index is None:
            raise RegistryError("production allocation range is exhausted")
        network = ipaddress.ip_network(policy["network"]["cidr"])
        name = f"prod{index}"
        production_start = policy["network"]["production_range"]["start"]
        ip = str(network.network_address + production_start + index - 1)
        resource = base_resource(
            policy=policy,
            kind="production",
            name=name,
            index=index,
            source=None,
            vmid=args.vmid,
            ip=ip,
            purpose={"slug": purpose_slug, "source_resource": None},
            domains={
                "primary": primary,
                "aliases": aliases,
                "staging_base": staging_base,
            },
            placement=placement,
            initial_node=initial_node,
            spec=spec,
            allocation_id=allocation_id,
            fingerprint=fingerprint,
        )
        validate_resource(resource, policy)
        validate_registry_invariants([*resources, resource])
        atomic_write_json(registry.resource_path(name), resource)
        return {"created": True, "resource": resource}


def command_allocate_staging(registry: Registry, args: argparse.Namespace) -> Any:
    policy = registry.policy()
    placement = validate_nodes(
        flatten_nodes(args.placement), policy, exactly_one=True
    )
    validate_vmid(args.vmid, policy)
    allocation_id = normalize_allocation_id(args.allocation_id)

    with registry.lock():
        resources = registry.resources()
        by_name = {record["name"]: record for record in resources}
        source = by_name.get(args.source)
        if source is None or source["kind"] != "production":
            raise RegistryError(f"staging source is not a registered production guest: {args.source}")
        if placement[0] not in source["placement"]:
            raise RegistryError("staging node must belong to source production placement")
        if source["owner_node"] == placement[0]:
            raise RegistryError("staging node may not be the source production owner")

        requested_domain = normalize_domain(args.domain) if args.domain else None
        spec = effective_spec(args, policy, "staging")
        request = {
            "kind": "staging",
            "source": source["name"],
            "vmid": args.vmid,
            "domain": requested_domain,
            "placement": placement,
            "spec": spec,
        }
        fingerprint = request_fingerprint(request)
        existing = find_idempotent_allocation(
            resources, allocation_id, fingerprint, "staging"
        )
        if existing:
            return {"created": False, "resource": existing}

        if args.placement_limit <= 0:
            raise RegistryError("staging placement limit must be positive")
        placed_count = sum(
            1
            for record in resources
            if record["kind"] == "staging"
            and record["placement"] == placement
        )
        if placed_count >= args.placement_limit:
            raise RegistryError(
                f"{placement[0]} already has {placed_count} registered staging "
                f"guests (limit {args.placement_limit})"
            )

        used_suffixes = {
            record["index"]
            for record in resources
            if record["kind"] == "staging" and record["source"] == source["name"]
        }
        suffix = next(
            (
                candidate
                for candidate in range(1, args.placement_limit + 1)
                if candidate not in used_suffixes
            ),
            None,
        )
        if suffix is None:
            raise RegistryError(
                f"stage number range 1-{args.placement_limit} is exhausted "
                f"for source {source['name']}"
            )
        name = f"stage{suffix}{source['name']}"
        domain = (
            requested_domain
            if requested_domain is not None
            else normalize_domain(
                f"stage{suffix}{source['name']}.{source['domains']['primary']}"
            )
        )

        used_ips = {ipaddress.ip_address(record["ip"]) for record in resources}
        network = ipaddress.ip_network(policy["network"]["cidr"])
        staging_range = policy["network"]["staging_range"]
        ip = next(
            (
                network.network_address + octet
                for octet in range(staging_range["start"], staging_range["end"] + 1)
                if network.network_address + octet not in used_ips
            ),
            None,
        )
        if ip is None:
            raise RegistryError("staging IP allocation range is exhausted")
        resource = base_resource(
            policy=policy,
            kind="staging",
            name=name,
            index=suffix,
            source=source["name"],
            vmid=args.vmid,
            ip=str(ip),
            purpose={
                "slug": source["purpose"]["slug"],
                "source_resource": source["name"],
            },
            domains={
                "primary": domain,
                "aliases": [],
                "staging_base": source["domains"]["staging_base"],
            },
            placement=placement,
            initial_node=placement[0],
            spec=spec,
            allocation_id=allocation_id,
            fingerprint=fingerprint,
        )
        validate_resource(resource, policy)
        validate_registry_invariants([*resources, resource])
        atomic_write_json(registry.resource_path(name), resource)
        return {"created": True, "resource": resource}


def _parse_snapshot_guids(values: Sequence[str] | None) -> dict[str, str]:
    result: dict[str, str] = {}
    for value in values or []:
        if "=" not in value:
            raise RegistryError("snapshot GUID must use moxN=GUID")
        node, guid = value.split("=", 1)
        if node in result:
            raise RegistryError(f"duplicate snapshot GUID node: {node}")
        if not GUID_RE.fullmatch(guid):
            raise RegistryError(f"invalid ZFS GUID for {node}")
        result[node] = guid
    return result


def _parse_snapshot_volume_guids(
    values: Sequence[str] | None,
) -> dict[str, dict[str, str]]:
    result: dict[str, dict[str, str]] = {}
    for value in values or []:
        if "," not in value or "=" not in value:
            raise RegistryError(
                "snapshot volume GUID must use STORAGE:VOLUME,MOXN=GUID"
            )
        volume_id, node_guid = value.rsplit(",", 1)
        node, guid = node_guid.split("=", 1)
        validate_safe_identifier(volume_id, "snapshot source volume id")
        if not MOX_RE.fullmatch(node):
            raise RegistryError(f"invalid snapshot GUID node: {node}")
        if not GUID_RE.fullmatch(guid):
            raise RegistryError(f"invalid ZFS GUID for {volume_id} on {node}")
        node_guids = result.setdefault(volume_id, {})
        if node in node_guids:
            raise RegistryError(
                f"duplicate snapshot GUID for {volume_id} on {node}"
            )
        node_guids[node] = guid
    return result


def command_record_staging_snapshot_intent(
    registry: Registry, args: argparse.Namespace
) -> Any:
    """Persist cleanup-capable snapshot ownership before Proxmox is mutated."""

    policy = registry.policy()
    snapshot_name = validate_snapshot_name(args.snapshot_name)
    source_volume_id = validate_safe_identifier(
        args.source_volume_id, "snapshot source volume id"
    )
    source_volume_ids = [
        validate_safe_identifier(value, "snapshotted source volume id")
        for value in args.snapshotted_volume_id
    ]
    if (
        not source_volume_ids
        or len(source_volume_ids) != len(set(source_volume_ids))
        or source_volume_id not in source_volume_ids
    ):
        raise RegistryError(
            "snapshotted source volumes must be unique and include the root volume"
        )

    with registry.lock():
        resources = registry.resources()
        by_name = {record["name"]: record for record in resources}
        current = by_name.get(args.name)
        if current is None:
            raise RegistryError(f"resource does not exist: {args.name}")
        if current["kind"] != "staging":
            raise RegistryError(
                "snapshot intent may only be recorded on staging resources"
            )
        source = by_name.get(current["source"])
        if source is None or source["kind"] != "production":
            raise RegistryError("staging snapshot source production resource is missing")
        if source["proxmox"]["volume_id"] != source_volume_id:
            raise RegistryError(
                "snapshot root volume must equal the registered production volume"
            )
        if args.snapshot_owner_node not in source["placement"]:
            raise RegistryError("snapshot owner must belong to production placement")

        metadata = {
            "name": snapshot_name,
            "owner_node": args.snapshot_owner_node,
            "guids": {},
            "source_resource": source["name"],
            "source_volume_id": source_volume_id,
            "source_volume_ids": source_volume_ids,
            "volume_guids": {},
            "verified": False,
            "dependent_resources": [current["name"]],
            "refcount": 1,
        }
        if current["proxmox"]["snapshot"] == metadata:
            return current
        if current["proxmox"]["snapshot"] is not None:
            raise RegistryError(
                "staging resource already has a different snapshot dependency"
            )
        if (
            args.expected_revision is not None
            and current["revision"] != args.expected_revision
        ):
            raise RegistryError(
                f"revision conflict: expected {args.expected_revision}, "
                f"found {current['revision']}"
            )
        if current["state"] != "snapshotting":
            raise RegistryError(
                "staging snapshot intent may only be recorded while snapshotting"
            )
        for resource in resources:
            snapshot = resource["proxmox"]["snapshot"]
            if (
                resource["kind"] == "staging"
                and snapshot is not None
                and snapshot["source_resource"] == source["name"]
                and snapshot["name"] == snapshot_name
            ):
                raise RegistryError(
                    f"snapshot {snapshot_name!r} already belongs to "
                    f"{resource['name']}"
                )

        updated = copy.deepcopy(current)
        updated["proxmox"]["snapshot"] = metadata
        updated["updated_at"] = utc_now()
        updated["revision"] += 1
        validate_resource(updated, policy)
        revised_resources = [
            updated if record["name"] == updated["name"] else record
            for record in resources
        ]
        validate_registry_invariants(revised_resources)
        atomic_write_json(registry.resource_path(updated["name"]), updated)
        return updated


def command_record_staging_snapshot(
    registry: Registry, args: argparse.Namespace
) -> Any:
    """Record one verified, unique source snapshot dependency for staging."""

    policy = registry.policy()
    snapshot_name = validate_snapshot_name(args.snapshot_name)
    source_volume_id = validate_safe_identifier(
        args.source_volume_id, "snapshot source volume id"
    )
    guids = _parse_snapshot_guids(args.snapshot_guid)
    volume_guids = _parse_snapshot_volume_guids(args.snapshot_volume_guid)

    with registry.lock():
        resources = registry.resources()
        by_name = {record["name"]: record for record in resources}
        current = by_name.get(args.name)
        if current is None:
            raise RegistryError(f"resource does not exist: {args.name}")
        if current["kind"] != "staging":
            raise RegistryError(
                "snapshot dependencies may only be recorded on staging resources"
            )
        source = by_name.get(current["source"])
        if source is None or source["kind"] != "production":
            raise RegistryError("staging snapshot source production resource is missing")
        if source["proxmox"]["volume_id"] != source_volume_id:
            raise RegistryError(
                "snapshot source volume must equal the registered production volume"
            )
        if args.snapshot_owner_node not in source["placement"]:
            raise RegistryError("snapshot owner must belong to production placement")
        intent = current["proxmox"]["snapshot"]
        if intent is None:
            raise RegistryError(
                "snapshot intent must be recorded before snapshot verification"
            )
        if (
            intent["name"] != snapshot_name
            or intent["owner_node"] != args.snapshot_owner_node
            or intent["source_resource"] != source["name"]
            or intent["source_volume_id"] != source_volume_id
        ):
            raise RegistryError("verified snapshot does not match its durable intent")
        if set(volume_guids) != set(intent["source_volume_ids"]):
            raise RegistryError(
                "snapshot volume GUIDs must cover every intended source volume"
            )
        for volume_id, node_guids in volume_guids.items():
            if set(node_guids) != set(source["placement"]):
                raise RegistryError(
                    f"snapshot GUIDs for {volume_id} must cover production placement"
                )
            if len(set(node_guids.values())) != 1:
                raise RegistryError(
                    f"snapshot GUID differs between nodes for {volume_id}"
                )
        if guids != volume_guids[source_volume_id]:
            raise RegistryError(
                "root snapshot GUIDs must equal the root volume GUID map"
            )

        metadata = copy.deepcopy(intent)
        metadata["guids"] = guids
        metadata["volume_guids"] = volume_guids
        metadata["verified"] = True
        if intent == metadata:
            return current
        if intent["verified"]:
            raise RegistryError("staging snapshot is already verified differently")
        if (
            args.expected_revision is not None
            and current["revision"] != args.expected_revision
        ):
            raise RegistryError(
                f"revision conflict: expected {args.expected_revision}, "
                f"found {current['revision']}"
            )
        if current["state"] not in {"snapshotting", "replicating"}:
            raise RegistryError(
                "staging snapshot dependency may only be recorded while "
                "snapshotting or replicating"
            )
        updated = copy.deepcopy(current)
        updated["proxmox"]["snapshot"] = metadata
        updated["updated_at"] = utc_now()
        updated["revision"] += 1
        validate_resource(updated, policy)
        revised_resources = [
            updated if record["name"] == updated["name"] else record
            for record in resources
        ]
        validate_registry_invariants(revised_resources)
        atomic_write_json(registry.resource_path(updated["name"]), updated)
        return updated


def command_update(registry: Registry, args: argparse.Namespace) -> Any:
    policy = registry.policy()
    with registry.lock():
        resources = registry.resources()
        by_name = {record["name"]: record for record in resources}
        current = by_name.get(args.name)
        if current is None:
            raise RegistryError(f"resource does not exist: {args.name}")
        if args.expected_revision is not None and current["revision"] != args.expected_revision:
            raise RegistryError(
                f"revision conflict: expected {args.expected_revision}, "
                f"found {current['revision']}"
            )
        updated = copy.deepcopy(current)
        changed = False

        if args.state is not None and args.state != current["state"]:
            allowed = STATE_TRANSITIONS[current["kind"]].get(current["state"], set())
            if args.state not in allowed:
                raise RegistryError(
                    f"invalid {current['kind']} state transition "
                    f"{current['state']} -> {args.state}"
                )
            updated["state"] = args.state
            changed = True
        if args.placement is not None:
            placement = validate_nodes(flatten_nodes(args.placement), policy)
            if current["kind"] == "staging" and len(placement) != 1:
                raise RegistryError("staging placement must contain exactly one node")
            updated["placement"] = placement
            if updated["initial_node"] not in placement:
                updated["initial_node"] = placement[0]
            if updated["owner_node"] not in {None, *placement}:
                raise RegistryError("clear or change owner before removing it from placement")
            changed = True
        if args.initial_node is not None:
            if args.initial_node not in updated["placement"]:
                raise RegistryError("initial node must belong to placement")
            updated["initial_node"] = args.initial_node
            changed = True
        if args.clear_owner_node:
            updated["owner_node"] = None
            changed = True
        elif args.owner_node is not None:
            if args.owner_node not in updated["placement"]:
                raise RegistryError("owner node must belong to placement")
            updated["owner_node"] = args.owner_node
            changed = True
        if args.routes_enabled is not None:
            if args.routes_enabled and updated["state"] not in ROUTABLE_STATES:
                raise RegistryError(
                    "routes may only be enabled in stopped, ready, or active state"
                )
            updated["routes_enabled"] = args.routes_enabled
            changed = True
        if args.purpose is not None:
            if current["kind"] != "production":
                raise RegistryError("staging purpose is inherited from its production source")
            dependents = [
                record["name"] for record in resources if record["source"] == current["name"]
            ]
            if dependents:
                raise RegistryError("cannot change purpose while staging dependents exist")
            updated["purpose"]["slug"] = normalize_purpose(args.purpose)
            changed = True
        if args.primary_domain is not None:
            updated["domains"]["primary"] = normalize_domain(args.primary_domain)
            changed = True
        if args.alias is not None:
            aliases = normalize_domains(args.alias)
            if updated["domains"]["primary"] in aliases:
                raise RegistryError("primary domain may not also be an alias")
            updated["domains"]["aliases"] = aliases
            changed = True
        elif args.clear_aliases:
            updated["domains"]["aliases"] = []
            changed = True
        if args.staging_dns_base is not None:
            if current["kind"] != "production":
                raise RegistryError("only production resources define a staging DNS base")
            dependents = [
                record["name"] for record in resources if record["source"] == current["name"]
            ]
            if dependents:
                raise RegistryError(
                    "cannot change staging DNS base while staging dependents exist"
                )
            updated["domains"]["staging_base"] = normalize_domain(
                args.staging_dns_base
            )
            changed = True
        if args.ha_nodes is not None:
            ha_nodes = validate_nodes(
                flatten_nodes(args.ha_nodes), policy, allow_empty=True
            )
            if current["kind"] == "staging" and ha_nodes:
                raise RegistryError("staging resources may not be HA-managed")
            if any(node not in updated["placement"] for node in ha_nodes):
                raise RegistryError("HA nodes must be contained in placement")
            updated["proxmox"]["ha_nodes"] = ha_nodes
            changed = True
        if args.replication_targets is not None:
            targets = validate_nodes(
                flatten_nodes(args.replication_targets), policy, allow_empty=True
            )
            if any(node not in updated["placement"] for node in targets):
                raise RegistryError("replication targets must be contained in placement")
            updated["proxmox"]["replication_targets"] = targets
            changed = True
        if current["kind"] == "production" and (
            args.clear_volume or args.volume_id is not None
        ):
            requested_volume = (
                None
                if args.clear_volume
                else validate_safe_identifier(args.volume_id, "volume id")
            )
            snapshot_dependents = [
                record["name"]
                for record in resources
                if record["kind"] == "staging"
                and record["source"] == current["name"]
                and record["proxmox"]["snapshot"] is not None
            ]
            if (
                snapshot_dependents
                and requested_volume != current["proxmox"]["volume_id"]
            ):
                raise RegistryError(
                    "cannot change production volume while staging snapshot "
                    "dependencies exist: "
                    + ", ".join(snapshot_dependents)
                )
        if args.clear_volume:
            updated["proxmox"]["volume_id"] = None
            changed = True
        elif args.volume_id is not None:
            updated["proxmox"]["volume_id"] = validate_safe_identifier(
                args.volume_id, "volume id"
            )
            changed = True
        if args.clear_snapshot:
            updated["proxmox"]["snapshot"] = None
            changed = True
        elif (
            args.snapshot_name is not None
            or args.snapshot_owner_node is not None
            or args.snapshot_guid
        ):
            snapshot = copy.deepcopy(updated["proxmox"]["snapshot"])
            if snapshot is None:
                if args.snapshot_name is None or args.snapshot_owner_node is None:
                    raise RegistryError(
                        "new snapshot metadata requires --snapshot-name and "
                        "--snapshot-owner-node"
                    )
                snapshot = {
                    "name": validate_snapshot_name(args.snapshot_name),
                    "owner_node": args.snapshot_owner_node,
                    "guids": {},
                }
            if args.snapshot_name is not None:
                snapshot["name"] = validate_snapshot_name(args.snapshot_name)
            if args.snapshot_owner_node is not None:
                if args.snapshot_owner_node not in updated["placement"]:
                    raise RegistryError("snapshot owner must belong to placement")
                snapshot["owner_node"] = args.snapshot_owner_node
            snapshot["guids"].update(_parse_snapshot_guids(args.snapshot_guid))
            updated["proxmox"]["snapshot"] = snapshot
            changed = True

        if not changed:
            raise RegistryError("update did not specify any changes")
        updated["updated_at"] = utc_now()
        updated["revision"] += 1
        validate_resource(updated, policy)
        revised_resources = [
            updated if record["name"] == updated["name"] else record for record in resources
        ]
        validate_registry_invariants(revised_resources)
        atomic_write_json(registry.resource_path(updated["name"]), updated)
        return updated


def _orchestration_resource(
    registry: Registry, name: str
) -> tuple[dict[str, Any], Path]:
    resource = next(
        (record for record in registry.resources() if record["name"] == name),
        None,
    )
    if resource is None:
        raise RegistryError(f"resource does not exist: {name}")
    if resource["kind"] != "production":
        raise RegistryError("production orchestration is only valid for production")
    return resource, registry.orchestration_path(name)


def command_orchestration_get(
    registry: Registry, args: argparse.Namespace
) -> Any:
    resource, path = _orchestration_resource(registry, args.name)
    if not path.exists():
        return {
            "exists": False,
            "resource": resource["name"],
            "resource_id": resource["id"],
            "install_phase": "unknown",
            "final_network_enabled": None,
            "startup_sha256": None,
            "lease": None,
        }
    record = read_json_file(path, "orchestration record")
    validate_orchestration_record(record)
    if record["resource_id"] != resource["id"]:
        raise RegistryError("orchestration record belongs to an obsolete resource")
    return record


def command_orchestration_acquire(
    registry: Registry, args: argparse.Namespace
) -> Any:
    if not re.fullmatch(r"[0-9a-f]{32}", args.nonce):
        raise RegistryError("orchestration lease nonce must be 32 lowercase hex digits")
    owner = validate_safe_identifier(args.owner, "orchestration lease owner")
    if not 60 <= args.ttl_seconds <= 172800:
        raise RegistryError("orchestration lease TTL must be 60 through 172800 seconds")
    final_network_enabled = args.final_network_enabled == "yes"
    startup_sha256 = (
        None
        if args.startup_sha256 == "none"
        else str(args.startup_sha256).lower()
    )
    source_iso_sha256 = str(args.source_iso_sha256).lower()
    install_mode = str(args.install_mode)
    if startup_sha256 is not None and not re.fullmatch(
        r"[0-9a-f]{64}", startup_sha256
    ):
        raise RegistryError("--startup-sha256 must be lowercase SHA-256 or none")
    if not re.fullmatch(r"[0-9a-f]{64}", source_iso_sha256):
        raise RegistryError("--source-iso-sha256 must be lowercase SHA-256")

    with registry.lock():
        resource, path = _orchestration_resource(registry, args.name)
        now = dt.datetime.now(dt.timezone.utc).replace(microsecond=0)
        now_text = now.isoformat()
        if path.exists():
            record = read_json_file(path, "orchestration record")
            validate_orchestration_record(record)
            if record["resource_id"] != resource["id"]:
                raise RegistryError(
                    "orchestration record belongs to an obsolete resource"
                )
            lease = record["lease"]
            if (
                lease is not None
                and lease["nonce"] != args.nonce
                and parse_timestamp(lease["expires_at"]) > now
            ):
                raise RegistryError(
                    "resource orchestration lease is held by "
                    f"{lease['owner']} until {lease['expires_at']}"
                )
            updated = copy.deepcopy(record)
            if "final_network_enabled" not in updated:
                updated["final_network_enabled"] = final_network_enabled
                updated["startup_sha256"] = startup_sha256
            elif (
                updated["final_network_enabled"] != final_network_enabled
                or updated["startup_sha256"] != startup_sha256
            ):
                raise RegistryError(
                    "requested startup/network policy differs from the durable "
                    "production VM contract"
                )
            if "source_iso_sha256" not in updated:
                updated["source_iso_sha256"] = source_iso_sha256
                updated["install_mode"] = install_mode
            elif (
                updated["source_iso_sha256"] != source_iso_sha256
                or updated["install_mode"] != install_mode
            ):
                raise RegistryError(
                    "requested source ISO/install mode differs from the durable "
                    "production VM contract"
                )
            updated["revision"] += 1
            updated["updated_at"] = now_text
        else:
            updated = {
                "schema_version": SCHEMA_VERSION,
                "record_type": "orchestration",
                "resource": resource["name"],
                "resource_id": resource["id"],
                "install_phase": args.initial_install_phase,
                "final_network_enabled": final_network_enabled,
                "startup_sha256": startup_sha256,
                "source_iso_sha256": source_iso_sha256,
                "install_mode": install_mode,
                "lease": None,
                "created_at": now_text,
                "updated_at": now_text,
                "revision": 1,
            }
        updated["lease"] = {
            "nonce": args.nonce,
            "owner": owner,
            "acquired_at": now_text,
            "expires_at": (
                now + dt.timedelta(seconds=args.ttl_seconds)
            ).isoformat(),
        }
        validate_orchestration_record(updated)
        atomic_write_json(path, updated)
        return updated


def _require_active_orchestration_lease(
    record: dict[str, Any], nonce: str
) -> None:
    lease = record["lease"]
    now = dt.datetime.now(dt.timezone.utc)
    if lease is None or lease["nonce"] != nonce:
        raise RegistryError("caller does not own the resource orchestration lease")
    if parse_timestamp(lease["expires_at"]) <= now:
        raise RegistryError("resource orchestration lease has expired")


def command_orchestration_set_phase(
    registry: Registry, args: argparse.Namespace
) -> Any:
    if not re.fullmatch(r"[0-9a-f]{32}", args.nonce):
        raise RegistryError("orchestration lease nonce is invalid")
    with registry.lock():
        resource, path = _orchestration_resource(registry, args.name)
        if not path.exists():
            raise RegistryError("resource has no orchestration record")
        record = read_json_file(path, "orchestration record")
        validate_orchestration_record(record)
        if record["resource_id"] != resource["id"]:
            raise RegistryError("orchestration record belongs to an obsolete resource")
        _require_active_orchestration_lease(record, args.nonce)
        current = record["install_phase"]
        if args.phase == current:
            return record
        allowed = INSTALL_PHASE_TRANSITIONS[current]
        if args.phase not in allowed:
            raise RegistryError(
                f"invalid install phase transition {current} -> {args.phase}"
            )
        destructive_restart = (
            current == "unknown" and args.phase == "media-attached"
        ) or (
            current == "installer-started" and args.phase == "media-attached"
        )
        if destructive_restart and args.confirm_wipe != "WIPE":
            raise RegistryError(
                "destructive installer restart requires exact --confirm-wipe WIPE"
            )
        updated = copy.deepcopy(record)
        updated["install_phase"] = args.phase
        updated["updated_at"] = utc_now()
        updated["revision"] += 1
        validate_orchestration_record(updated)
        atomic_write_json(path, updated)
        return updated


def command_orchestration_release(
    registry: Registry, args: argparse.Namespace
) -> Any:
    if not re.fullmatch(r"[0-9a-f]{32}", args.nonce):
        raise RegistryError("orchestration lease nonce is invalid")
    with registry.lock():
        resource, path = _orchestration_resource(registry, args.name)
        if not path.exists():
            raise RegistryError("resource has no orchestration record")
        record = read_json_file(path, "orchestration record")
        validate_orchestration_record(record)
        if record["resource_id"] != resource["id"]:
            raise RegistryError("orchestration record belongs to an obsolete resource")
        lease = record["lease"]
        if lease is None:
            return record
        if lease["nonce"] != args.nonce:
            raise RegistryError("caller does not own the resource orchestration lease")
        updated = copy.deepcopy(record)
        updated["lease"] = None
        updated["updated_at"] = utc_now()
        updated["revision"] += 1
        validate_orchestration_record(updated)
        atomic_write_json(path, updated)
        return updated


def command_release(registry: Registry, args: argparse.Namespace) -> Any:
    policy = registry.policy()
    with registry.lock():
        resources = registry.resources()
        cleanup_records = registry.cleanup_records()
        by_name = {record["name"]: record for record in resources}
        resource = by_name.get(args.name)
        if resource is None:
            raise RegistryError(f"resource does not exist: {args.name}")
        orchestration_path = registry.orchestration_path(resource["name"])
        if orchestration_path.exists():
            orchestration = read_json_file(
                orchestration_path, "orchestration record"
            )
            validate_orchestration_record(orchestration)
            if orchestration["resource_id"] != resource["id"]:
                raise RegistryError(
                    "orchestration record belongs to an obsolete resource"
                )
            lease = orchestration["lease"]
            if (
                lease is not None
                and parse_timestamp(lease["expires_at"])
                > dt.datetime.now(dt.timezone.utc)
            ):
                raise RegistryError(
                    "cannot release a resource with an active orchestration lease"
                )
        dependents = [
            record["name"] for record in resources if record["source"] == resource["name"]
        ]
        if dependents:
            raise RegistryError(
                f"cannot release {resource['name']}; staging dependents exist: "
                f"{', '.join(dependents)}"
            )
        pending_cleanup = [
            record["id"]
            for record in cleanup_records
            if record["resource_id"] == resource["id"] and record["state"] == "pending"
        ]
        if pending_cleanup:
            raise RegistryError(
                f"cannot release {resource['name']}; deferred cleanup is pending: "
                f"{', '.join(pending_cleanup)}"
            )
        if not args.force:
            if resource["state"] not in RELEASE_STATES:
                raise RegistryError(
                    f"state {resource['state']} is not releasable; transition to a "
                    "stopped/failed/cleanup state or use --force"
                )
            if resource["routes_enabled"]:
                raise RegistryError("disable routes before releasing the resource")
            proxmox = resource["proxmox"]
            if (
                resource["owner_node"] is not None
                or proxmox["volume_id"] is not None
                or proxmox["snapshot"] is not None
                or proxmox["ha_nodes"]
                or proxmox["replication_targets"]
            ):
                raise RegistryError(
                    "clear owner, volume, snapshot, HA, and replication metadata "
                    "before releasing; use --force only after external verification"
                )
        now = utc_now()
        history = {
            "schema_version": SCHEMA_VERSION,
            "record_type": "release",
            "released_at": now,
            "forced": bool(args.force),
            "resource": resource,
        }
        history_path = registry.history_dir / (
            f"{now.replace(':', '').replace('+00:00', 'Z')}-"
            f"{resource['name']}-{uuid.uuid4().hex[:8]}.json"
        )
        registry.prune_history(MAX_HISTORY_RECORDS - 1)
        atomic_write_json(history_path, history)
        registry.resource_path(resource["name"]).unlink()
        with contextlib.suppress(FileNotFoundError):
            orchestration_path.unlink()
        _fsync_directory(registry.resources_dir)
        remaining_ids = {
            record["id"]
            for record in resources
            if record["id"] != resource["id"]
        }
        registry.prune_completed_cleanup(remaining_ids)
        return {"released": resource["name"], "history": str(history_path)}


def command_defer_cleanup(registry: Registry, args: argparse.Namespace) -> Any:
    policy = registry.policy()
    validate_nodes([args.node], policy)
    if args.action not in CLEANUP_ACTIONS:
        raise RegistryError(f"unsupported cleanup action: {args.action}")
    target = validate_cleanup_target(args.action, args.target)
    if len(args.reason) > 240 or "\n" in args.reason:
        raise RegistryError("cleanup reason must be one line of at most 240 characters")
    identity = {
        "resource": args.resource,
        "resource_id": None,
        "node": args.node,
        "action": args.action,
        "target": target,
    }
    with registry.lock():
        resources = {record["name"]: record for record in registry.resources()}
        if args.resource not in resources:
            raise RegistryError(f"cleanup resource is not registered: {args.resource}")
        identity["resource_id"] = resources[args.resource]["id"]
        cleanup_id = "cleanup-" + hashlib.sha256(canonical_json(identity)).hexdigest()[:24]
        path = registry.cleanup_path(cleanup_id)
        if path.exists():
            record = read_json_file(path, "deferred cleanup record")
            validate_cleanup_record(record)
            return {"created": False, "cleanup": record}
        registry.prune_completed_cleanup(
            {resource["id"] for resource in resources.values()}
        )
        if len(registry.cleanup_records()) >= MAX_CLEANUP_RECORDS:
            raise RegistryError(
                f"deferred cleanup registry reached its {MAX_CLEANUP_RECORDS}-record limit"
            )
        now = utc_now()
        record = {
            "schema_version": SCHEMA_VERSION,
            "record_type": "deferred-cleanup",
            "id": cleanup_id,
            "resource": args.resource,
            "resource_id": resources[args.resource]["id"],
            "node": args.node,
            "action": args.action,
            "target": target,
            "reason": args.reason,
            "state": "pending",
            "attempts": 0,
            "created_at": now,
            "updated_at": now,
            "revision": 1,
        }
        validate_cleanup_record(record)
        atomic_write_json(path, record)
        return {"created": True, "cleanup": record}


def command_finalize_staging_cleanup(
    registry: Registry, args: argparse.Namespace
) -> Any:
    """Atomically release staging after every recorded cleanup action completed."""

    policy = registry.policy()
    try:
        expected_resource_id = str(uuid.UUID(args.resource_id))
    except (TypeError, ValueError, AttributeError) as exc:
        raise RegistryError("--resource-id must be a UUID") from exc

    with registry.lock():
        resources = registry.resources()
        cleanup_records = registry.cleanup_records()
        by_name = {record["name"]: record for record in resources}
        resource = by_name.get(args.name)
        if resource is None:
            registry.prune_completed_cleanup(
                {record["id"] for record in resources}
            )
            return {
                "released": False,
                "reason": "resource-absent",
                "resource": args.name,
            }
        if resource["id"] != expected_resource_id:
            raise RegistryError(
                f"resource identity changed for {args.name}; refusing cleanup finalization"
            )
        if resource["kind"] != "staging":
            raise RegistryError("only staging resources may use cleanup finalization")
        if resource["state"] != "cleanup_pending":
            raise RegistryError(
                f"{args.name} is {resource['state']}, not cleanup_pending"
            )
        if resource["routes_enabled"]:
            raise RegistryError("staging routes must be disabled before finalization")

        owned_cleanup = [
            record
            for record in cleanup_records
            if record["resource_id"] == expected_resource_id
        ]
        pending = [
            record["id"]
            for record in owned_cleanup
            if record["state"] != "completed"
        ]
        if pending:
            raise RegistryError(
                "deferred cleanup remains pending: " + ", ".join(sorted(pending))
            )
        completed_identities = {
            (record["node"], record["action"], record["target"])
            for record in owned_cleanup
            if record["state"] == "completed"
        }
        cleanup_source = by_name.get(resource["source"])
        if cleanup_source is None or cleanup_source["kind"] != "production":
            raise RegistryError("staging cleanup source production is missing")
        route_target = resource["name"]
        missing_route_nodes = [
            node
            for node in cleanup_source["placement"]
            if (
                node,
                "remove-route",
                route_target,
            )
            not in completed_identities
        ]
        if missing_route_nodes:
            raise RegistryError(
                "HAProxy route removal is incomplete on: "
                + ", ".join(missing_route_nodes)
            )

        proxmox = resource["proxmox"]
        if resource["owner_node"] is not None:
            expected = (
                resource["owner_node"],
                "destroy-vm",
                f"vm:{resource['vmid']}",
            )
            if expected not in completed_identities:
                raise RegistryError(
                    "registered staging VM ownership lacks completed destruction"
                )
        if proxmox["volume_id"] is not None:
            owner = resource["owner_node"] or resource["placement"][0]
            expected = (owner, "destroy-volume", proxmox["volume_id"])
            if expected not in completed_identities:
                raise RegistryError(
                    "registered staging volume lacks completed destruction"
                )

        snapshot = proxmox["snapshot"]
        if snapshot is not None:
            source = cleanup_source
            target = f"vm-{source['vmid']}@{snapshot['name']}"
            missing_nodes = [
                node
                for node in sorted(source["placement"])
                if (node, "delete-snapshot", target) not in completed_identities
            ]
            if missing_nodes:
                raise RegistryError(
                    "staging snapshot cleanup is incomplete on: "
                    + ", ".join(missing_nodes)
                )

        remaining = [
            record for record in resources if record["id"] != expected_resource_id
        ]
        validate_registry_invariants(remaining)
        for record in remaining:
            validate_resource(record, policy)

        now = utc_now()
        history = {
            "schema_version": SCHEMA_VERSION,
            "record_type": "release",
            "released_at": now,
            "forced": False,
            "resource": resource,
        }
        history_path = registry.history_dir / (
            f"{now.replace(':', '').replace('+00:00', 'Z')}-"
            f"{resource['name']}-{uuid.uuid4().hex[:8]}.json"
        )
        registry.prune_history(MAX_HISTORY_RECORDS - 1)
        atomic_write_json(history_path, history)
        registry.resource_path(resource["name"]).unlink()
        _fsync_directory(registry.resources_dir)
        registry.prune_completed_cleanup(
            {record["id"] for record in remaining}
        )
        return {
            "released": True,
            "resource": resource["name"],
            "history": str(history_path),
        }


def build_routes(resources: Sequence[dict[str, Any]]) -> list[dict[str, Any]]:
    routes: list[dict[str, Any]] = []
    for resource in resources:
        if not resource["routes_enabled"] or resource["state"] not in ROUTABLE_STATES:
            continue
        for domain in [resource["domains"]["primary"], *resource["domains"]["aliases"]]:
            routes.append(
                {
                    "domain": domain,
                    "resource": resource["name"],
                    "kind": resource["kind"],
                    "purpose": resource["purpose"]["slug"],
                    "ip": resource["ip"],
                    "http_port": 80,
                    "https_port": 443,
                }
            )
            if len(routes) > MAX_ROUTES:
                raise RegistryError(
                    f"registry route count exceeds renderer limit of {MAX_ROUTES}"
                )
    return sorted(routes, key=lambda route: (route["domain"], route["resource"]))


def command_list_routes(registry: Registry, args: argparse.Namespace) -> Any:
    return build_routes(registry.resources())


def command_ingress_begin(registry: Registry, args: argparse.Namespace) -> Any:
    """Durably publish desired generation before changing any live ingress."""

    generation = validate_sha256(args.generation, "ingress generation")
    bundle_sha256 = validate_sha256(
        args.bundle_sha256, "ingress bundle digest"
    )
    if not 0 <= args.route_count <= MAX_ROUTES:
        raise RegistryError(
            f"ingress route count must be between 0 and {MAX_ROUTES}"
        )
    with registry.lock():
        policy = registry.policy()
        nodes = validate_nodes(flatten_nodes(args.node), policy)
        expected_nodes = [
            f"mox{index}"
            for index in range(1, policy["limits"]["max_hosts"] + 1)
        ]
        if nodes != expected_nodes:
            raise RegistryError(
                "ingress begin must name every configured mox in order"
            )
        now = utc_now()
        generation_path = registry.ingress_generation_path(generation)
        if generation_path.exists():
            generation_record = read_json_file(
                generation_path, "ingress generation metadata"
            )
            validate_ingress_generation_record(generation_record)
            if any(
                generation_record[field] != expected
                for field, expected in (
                    ("generation", generation),
                    ("bundle_sha256", bundle_sha256),
                    ("route_count", args.route_count),
                )
            ):
                raise RegistryError(
                    "ingress generation was already recorded with different metadata"
                )
        else:
            generation_record = {
                "schema_version": SCHEMA_VERSION,
                "record_type": "haproxy-generation",
                "generation": generation,
                "bundle_sha256": bundle_sha256,
                "route_count": args.route_count,
                "created_at": now,
            }
            validate_ingress_generation_record(generation_record)
            atomic_write_json(generation_path, generation_record)

        existing_desired = registry.ingress_desired()
        desired_unchanged = (
            existing_desired is not None
            and existing_desired["generation"] == generation
            and existing_desired["bundle_sha256"] == bundle_sha256
            and existing_desired["route_count"] == args.route_count
            and existing_desired["nodes"] == nodes
        )
        if desired_unchanged:
            desired = existing_desired
        else:
            desired = {
                "schema_version": SCHEMA_VERSION,
                "record_type": "haproxy-desired",
                "generation": generation,
                "bundle_sha256": bundle_sha256,
                "route_count": args.route_count,
                "nodes": nodes,
                "created_at": (
                    existing_desired["created_at"]
                    if existing_desired is not None
                    else now
                ),
                "updated_at": now,
                "revision": (
                    existing_desired["revision"] + 1
                    if existing_desired is not None
                    else 1
                ),
            }
            validate_ingress_desired_record(desired, policy)
            atomic_write_json(registry.ingress_desired_path, desired)

        statuses_by_node = {
            record["node"]: record for record in registry.ingress_node_statuses()
        }
        statuses: list[dict[str, Any]] = []
        for node in nodes:
            existing = statuses_by_node.get(node)
            if (
                existing is not None
                and existing["desired_generation"] == generation
            ):
                status = existing
            else:
                applied_generation = (
                    existing["applied_generation"]
                    if existing is not None
                    else None
                )
                status = {
                    "schema_version": SCHEMA_VERSION,
                    "record_type": "haproxy-node-status",
                    "node": node,
                    "desired_generation": generation,
                    "applied_generation": applied_generation,
                    "state": (
                        "applied"
                        if applied_generation == generation
                        else "pending"
                    ),
                    "created_at": (
                        existing["created_at"] if existing is not None else now
                    ),
                    "updated_at": now,
                    "revision": (
                        existing["revision"] + 1 if existing is not None else 1
                    ),
                }
                validate_ingress_node_status(status, policy)
                atomic_write_json(registry.ingress_node_path(node), status)
            statuses.append(status)

        registry.prune_ingress_generations(
            generation, MAX_INGRESS_GENERATIONS
        )
        return {"desired": desired, "nodes": statuses}


def command_ingress_mark(registry: Registry, args: argparse.Namespace) -> Any:
    generation = validate_sha256(args.generation, "ingress generation")
    with registry.lock():
        policy = registry.policy()
        desired = registry.ingress_desired()
        if desired is None:
            raise RegistryError("no desired ingress generation has been published")
        if desired["generation"] != generation:
            raise RegistryError(
                "refusing to mark a node for a superseded ingress generation"
            )
        node = validate_nodes([args.node], policy, exactly_one=True)[0]
        if node not in desired["nodes"]:
            raise RegistryError("ingress node is outside the desired node set")
        path = registry.ingress_node_path(node)
        now = utc_now()
        if path.exists():
            existing = read_json_file(path, "ingress node status")
            validate_ingress_node_status(existing, policy)
        else:
            existing = None
        applied_generation = (
            generation
            if args.state == "applied"
            else (
                existing["applied_generation"]
                if existing is not None
                else None
            )
        )
        if (
            existing is not None
            and existing["desired_generation"] == generation
            and existing["applied_generation"] == applied_generation
            and existing["state"] == args.state
        ):
            return existing
        status = {
            "schema_version": SCHEMA_VERSION,
            "record_type": "haproxy-node-status",
            "node": node,
            "desired_generation": generation,
            "applied_generation": applied_generation,
            "state": args.state,
            "created_at": (
                existing["created_at"] if existing is not None else now
            ),
            "updated_at": now,
            "revision": existing["revision"] + 1 if existing is not None else 1,
        }
        validate_ingress_node_status(status, policy)
        atomic_write_json(path, status)
        return status


def command_ingress_status(registry: Registry, args: argparse.Namespace) -> Any:
    desired = registry.ingress_desired()
    return {
        "desired": desired,
        "nodes": registry.ingress_node_statuses(),
    }


OBSERVED_KEYS = {
    "schema_version",
    "nodes",
    "vms",
    "ha_resources",
    "replication_jobs",
    "volumes",
    "snapshots",
    "routes",
    "cleanup_completed",
}


def read_observation(path_value: str) -> dict[str, Any]:
    if path_value == "-":
        try:
            observed = json.load(sys.stdin)
        except json.JSONDecodeError as exc:
            raise RegistryError(f"invalid observation JSON on stdin: {exc}") from exc
        reject_secret_material(observed, "reconcile observation")
    else:
        observed = read_json_file(Path(path_value), "reconcile observation")
    if not isinstance(observed, dict):
        raise RegistryError("reconcile observation must be an object")
    unknown = sorted(set(observed) - OBSERVED_KEYS)
    if unknown:
        raise RegistryError(f"observation has unknown fields: {', '.join(unknown)}")
    if observed.get("schema_version") != SCHEMA_VERSION:
        raise RegistryError("unsupported reconcile observation schema")
    return observed


def _run_pvesh_json(pvesh: Path, arguments: Sequence[str]) -> Any:
    if not pvesh.is_absolute():
        raise RegistryError(f"pvesh path must be absolute: {pvesh}")
    _ensure_no_symlink_components(pvesh)
    if pvesh.is_symlink() or not pvesh.is_file():
        raise RegistryError(f"pvesh must be an absolute, non-symlink file: {pvesh}")
    try:
        completed = subprocess.run(
            [str(pvesh), *arguments, "--output-format", "json"],
            stdin=subprocess.DEVNULL,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
            timeout=30,
            check=False,
        )
    except (OSError, subprocess.TimeoutExpired) as exc:
        raise RegistryError(f"cannot execute pvesh {' '.join(arguments)}: {exc}") from exc
    if completed.returncode != 0:
        detail = completed.stderr.strip().replace("\n", " ")[:500]
        raise RegistryError(
            f"pvesh {' '.join(arguments)} failed with exit "
            f"{completed.returncode}: {detail or 'no diagnostic'}"
        )
    try:
        value = json.loads(completed.stdout)
    except json.JSONDecodeError as exc:
        raise RegistryError(
            f"pvesh {' '.join(arguments)} returned invalid JSON"
        ) from exc
    if isinstance(value, dict) and set(value) == {"data"}:
        value = value["data"]
    return value


def collect_live_observation(
    registry: Registry, policy: dict[str, Any], pvesh: Path
) -> dict[str, Any]:
    """Collect the safe subset needed for local VM/membership reconciliation."""

    resources = registry.resources()
    raw_nodes = _run_pvesh_json(pvesh, ["get", "/nodes"])
    raw_vms = _run_pvesh_json(
        pvesh, ["get", "/cluster/resources", "--type", "vm"]
    )
    if not isinstance(raw_nodes, list) or not isinstance(raw_vms, list):
        raise RegistryError("pvesh node/resource responses must be arrays")

    nodes: list[dict[str, Any]] = []
    for raw_node in raw_nodes:
        if not isinstance(raw_node, dict):
            raise RegistryError("pvesh returned a malformed node entry")
        name = raw_node.get("node", raw_node.get("name"))
        if not isinstance(name, str):
            raise RegistryError("pvesh node entry has no node name")
        validate_nodes([name], policy)
        status = str(raw_node.get("status", "")).lower()
        online_value = raw_node.get("online")
        online = (
            bool(online_value)
            if isinstance(online_value, (bool, int))
            else status == "online"
        )
        nodes.append({"name": name, "online": online})

    cluster_vms: dict[int, dict[str, Any]] = {}
    for raw_vm in raw_vms:
        if not isinstance(raw_vm, dict):
            raise RegistryError("pvesh returned a malformed VM resource entry")
        vmid = raw_vm.get("vmid")
        if isinstance(vmid, str) and vmid.isdigit():
            vmid = int(vmid)
        if isinstance(vmid, int):
            cluster_vms[vmid] = raw_vm

    observed_vms: list[dict[str, Any]] = []
    observed_volumes: dict[str, dict[str, str]] = {}
    disk_key = re.compile(r"^(?:scsi|virtio|sata|ide)[0-9]+$")
    for resource in resources:
        raw_vm = cluster_vms.get(resource["vmid"])
        if raw_vm is None:
            continue
        node = raw_vm.get("node")
        if not isinstance(node, str):
            raise RegistryError(f"pvesh VM {resource['vmid']} has no owner node")
        validate_nodes([node], policy)
        vm_type = raw_vm.get("type")
        if vm_type not in {None, "qemu"}:
            raise RegistryError(
                f"registered VMID {resource['vmid']} is not a QEMU VM"
            )
        config = _run_pvesh_json(
            pvesh, ["get", f"/nodes/{node}/qemu/{resource['vmid']}/config"]
        )
        if not isinstance(config, dict):
            raise RegistryError(f"pvesh config for VM {resource['vmid']} is not an object")
        name = config.get("name", raw_vm.get("name"))
        if not isinstance(name, str):
            raise RegistryError(f"pvesh VM {resource['vmid']} has no name")

        tags_value = config.get("tags", "")
        if isinstance(tags_value, str):
            tags = sorted(
                {
                    tag
                    for tag in re.split(r"[;,]", tags_value)
                    if tag and SAFE_ID_RE.fullmatch(tag)
                }
            )
        else:
            raise RegistryError(f"pvesh VM {resource['vmid']} tags are malformed")

        mac: str | None = None
        net0 = config.get("net0")
        if isinstance(net0, str):
            mac_match = re.search(
                r"(?:^|,)(?:virtio|e1000|rtl8139|vmxnet3)=((?:[0-9A-Fa-f]{2}:){5}[0-9A-Fa-f]{2})(?:,|$)",
                net0,
            )
            if mac_match:
                mac = mac_match.group(1).lower()

        volume_ids: list[str] = []
        for key, raw_value in config.items():
            if not disk_key.fullmatch(str(key)) or not isinstance(raw_value, str):
                continue
            volume_id = raw_value.split(",", 1)[0]
            if volume_id in {"none", "cdrom", "cloudinit"}:
                continue
            validate_safe_identifier(volume_id, "pvesh VM volume id")
            volume_ids.append(volume_id)
            observed_volumes[volume_id] = {"id": volume_id}

        status = raw_vm.get("status", "unknown")
        if not isinstance(status, str):
            status = "unknown"
        observed_vms.append(
            {
                "vmid": resource["vmid"],
                "name": name,
                "node": node,
                "status": status,
                # Static guest netplan is not represented in QEMU config.
                "ip": None,
                "mac": mac,
                "tags": tags,
                "volume_ids": sorted(volume_ids),
            }
        )

    observation = {
        "schema_version": SCHEMA_VERSION,
        "nodes": sorted(nodes, key=lambda item: int(item["name"][3:])),
        "vms": sorted(observed_vms, key=lambda item: item["vmid"]),
        "volumes": sorted(observed_volumes.values(), key=lambda item: item["id"]),
    }
    reject_secret_material(observation, "live Proxmox observation")
    return observation


def _validate_observed_nodes(
    observed: dict[str, Any], policy: dict[str, Any]
) -> dict[str, bool] | None:
    if "nodes" not in observed:
        return None
    if not isinstance(observed["nodes"], list):
        raise RegistryError("observed nodes must be an array")
    result: dict[str, bool] = {}
    for item in observed["nodes"]:
        if not isinstance(item, dict):
            raise RegistryError("each observed node must be an object")
        require_exact_keys(item, {"name", "online"}, "observed node")
        validate_nodes([item["name"]], policy)
        if not isinstance(item["online"], bool):
            raise RegistryError("observed node online must be boolean")
        if item["name"] in result:
            raise RegistryError(f"duplicate observed node: {item['name']}")
        result[item["name"]] = item["online"]
    return result


def _index_observed(
    observed: dict[str, Any], key: str, identifier: str
) -> dict[Any, dict[str, Any]] | None:
    if key not in observed:
        return None
    values = observed[key]
    if not isinstance(values, list):
        raise RegistryError(f"observed {key} must be an array")
    result: dict[Any, dict[str, Any]] = {}
    for value in values:
        if not isinstance(value, dict) or identifier not in value:
            raise RegistryError(f"observed {key} entry lacks {identifier}")
        identity = value[identifier]
        if identity in result:
            raise RegistryError(f"duplicate observed {key} identity: {identity!r}")
        result[identity] = value
    return result


def issue(
    issues: list[dict[str, Any]],
    severity: str,
    code: str,
    message: str,
    resource: str | None = None,
) -> None:
    issues.append(
        {
            "severity": severity,
            "code": code,
            "resource": resource,
            "message": message,
        }
    )


def _observed_tags(value: Any) -> set[str]:
    if isinstance(value, list) and all(isinstance(item, str) for item in value):
        return set(value)
    raise RegistryError("observed VM tags must be an array of strings")


def reconcile_report(
    resources: list[dict[str, Any]],
    cleanup_records: list[dict[str, Any]],
    policy: dict[str, Any],
    observed: dict[str, Any],
    *,
    apply: bool,
) -> tuple[dict[str, Any], list[dict[str, Any]], list[dict[str, Any]]]:
    issues: list[dict[str, Any]] = []
    changes: list[dict[str, Any]] = []
    updated_resources = copy.deepcopy(resources)
    updated_cleanup = copy.deepcopy(cleanup_records)
    by_name = {record["name"]: record for record in updated_resources}
    checked_sections = sorted(set(observed) - {"schema_version"})

    observed_nodes = _validate_observed_nodes(observed, policy)
    vms = _index_observed(observed, "vms", "vmid")
    ha_resources = _index_observed(observed, "ha_resources", "vmid")
    volumes = _index_observed(observed, "volumes", "id")
    routes = _index_observed(observed, "routes", "domain")

    if vms is not None:
        allowed_vm_keys = {
            "vmid",
            "name",
            "node",
            "status",
            "ip",
            "mac",
            "tags",
            "volume_ids",
        }
        for vm in vms.values():
            require_exact_keys(vm, allowed_vm_keys, "observed VM")
            if not isinstance(vm["vmid"], int):
                raise RegistryError("observed VM vmid must be an integer")
            validate_safe_identifier(vm["name"], "observed VM name")
            validate_nodes([vm["node"]], policy)
            validate_safe_identifier(vm["status"], "observed VM status")
            if vm["ip"] is not None:
                try:
                    ipaddress.ip_address(vm["ip"])
                except (TypeError, ValueError) as exc:
                    raise RegistryError("observed VM IP is invalid") from exc
            if vm["mac"] is not None and not MAC_RE.fullmatch(str(vm["mac"]).lower()):
                raise RegistryError("observed VM MAC is invalid")
            _observed_tags(vm["tags"])
            if not isinstance(vm["volume_ids"], list):
                raise RegistryError("observed VM volume_ids must be an array")
            for volume_id in vm["volume_ids"]:
                validate_safe_identifier(volume_id, "observed VM volume id")

    if ha_resources is not None:
        for observed_ha in ha_resources.values():
            require_exact_keys(observed_ha, {"vmid", "nodes"}, "observed HA resource")
            if not isinstance(observed_ha["vmid"], int):
                raise RegistryError("observed HA VMID must be an integer")
            validate_nodes(observed_ha["nodes"], policy, allow_empty=True)

    if volumes is not None:
        for volume in volumes.values():
            require_exact_keys(volume, {"id"}, "observed volume")
            validate_safe_identifier(volume["id"], "observed volume id")

    if routes is not None:
        for observed_route in routes.values():
            require_exact_keys(
                observed_route, {"domain", "resource", "ip"}, "observed route"
            )
            domain = normalize_domain(observed_route["domain"])
            if domain != observed_route["domain"]:
                raise RegistryError("observed route domain is not normalized")
            if not RESOURCE_RE.fullmatch(observed_route["resource"]):
                raise RegistryError("observed route resource name is invalid")
            try:
                ipaddress.ip_address(observed_route["ip"])
            except (TypeError, ValueError) as exc:
                raise RegistryError("observed route IP is invalid") from exc

    replications_by_vmid: dict[int, list[dict[str, Any]]] | None = None
    if "replication_jobs" in observed:
        if not isinstance(observed["replication_jobs"], list):
            raise RegistryError("observed replication_jobs must be an array")
        replications_by_vmid = {}
        for replication in observed["replication_jobs"]:
            if not isinstance(replication, dict):
                raise RegistryError("observed replication job must be an object")
            require_exact_keys(
                replication, {"vmid", "target", "healthy"}, "observed replication job"
            )
            validate_nodes([replication["target"]], policy)
            if not isinstance(replication["vmid"], int) or not isinstance(
                replication["healthy"], bool
            ):
                raise RegistryError("observed replication job has invalid types")
            replications_by_vmid.setdefault(replication["vmid"], []).append(replication)

    snapshot_entries: list[dict[str, Any]] | None = None
    if "snapshots" in observed:
        if not isinstance(observed["snapshots"], list):
            raise RegistryError("observed snapshots must be an array")
        snapshot_entries = observed["snapshots"]
        seen_snapshots: set[tuple[str, str, str]] = set()
        for entry in snapshot_entries:
            if not isinstance(entry, dict):
                raise RegistryError("each observed snapshot must be an object")
            require_exact_keys(
                entry, {"resource", "snapshot", "node", "guid"}, "observed snapshot"
            )
            if not RESOURCE_RE.fullmatch(entry["resource"]):
                raise RegistryError("observed snapshot resource name is invalid")
            validate_snapshot_name(entry["snapshot"])
            validate_nodes([entry["node"]], policy)
            if not GUID_RE.fullmatch(str(entry["guid"])):
                raise RegistryError("observed snapshot GUID is invalid")
            identity = (entry["resource"], entry["snapshot"], entry["node"])
            if identity in seen_snapshots:
                raise RegistryError(f"duplicate observed snapshot identity: {identity}")
            seen_snapshots.add(identity)

    now = dt.datetime.now(dt.timezone.utc)
    updated_by_name = {resource["name"]: resource for resource in updated_resources}
    for resource in updated_resources:
        name = resource["name"]
        if observed_nodes is not None:
            missing_nodes = sorted(set(resource["placement"]) - set(observed_nodes))
            if missing_nodes:
                issue(
                    issues,
                    "error",
                    "placement-node-missing",
                    f"placement nodes absent from Proxmox API: {', '.join(missing_nodes)}",
                    name,
                )
            offline_nodes = sorted(
                node
                for node in resource["placement"]
                if node in observed_nodes and not observed_nodes[node]
            )
            if offline_nodes:
                issue(
                    issues,
                    "warning",
                    "placement-node-offline",
                    f"placement nodes reported offline: {', '.join(offline_nodes)}",
                    name,
                )

        vm = vms.get(resource["vmid"]) if vms is not None else None
        if vms is not None and vm is None:
            age = (now - parse_timestamp(resource["created_at"])).total_seconds()
            if (
                resource["state"] in {"reserved", "provisioning"}
                and age > policy["limits"]["reservation_ttl_seconds"]
            ):
                issue(
                    issues,
                    "error",
                    "stale-reservation",
                    "reservation exceeded policy TTL and no VM exists",
                    name,
                )
                if apply and "failed" in STATE_TRANSITIONS[resource["kind"]].get(
                    resource["state"], set()
                ):
                    old_state = resource["state"]
                    resource["state"] = "failed"
                    resource["updated_at"] = utc_now()
                    resource["revision"] += 1
                    changes.append(
                        {
                            "resource": name,
                            "change": "state",
                            "from": old_state,
                            "to": "failed",
                        }
                    )
            elif resource["state"] not in {"reserved", "provisioning", "failed"}:
                issue(
                    issues,
                    "error",
                    "vm-missing",
                    f"registered {resource['state']} resource has no observed VM",
                    name,
                )
        elif vm is not None:
            if vm.get("name") != name:
                issue(issues, "error", "vm-name-mismatch", "VM name differs from registry", name)
            if vm.get("ip") is not None and vm["ip"] != resource["ip"]:
                issue(issues, "error", "vm-ip-mismatch", "VM IP differs from registry", name)
            if vm.get("mac") is not None and str(vm["mac"]).lower() != resource["mac"]:
                issue(issues, "error", "vm-mac-mismatch", "VM MAC differs from registry", name)
            node = vm.get("node")
            if node is not None and node not in resource["placement"]:
                issue(
                    issues,
                    "error",
                    "vm-placement-mismatch",
                    f"VM is on {node}, outside placement",
                    name,
                )
            if "tags" in vm:
                observed_tags = _observed_tags(vm["tags"])
                expected_tags = (
                    {policy["tags"]["production"]}
                    if resource["kind"] == "production"
                    else {policy["tags"]["staging"], policy["tags"]["evictable"]}
                )
                missing = sorted(expected_tags - observed_tags)
                if missing:
                    issue(
                        issues,
                        "error",
                        "vm-tag-mismatch",
                        f"VM is missing required tags: {', '.join(missing)}",
                        name,
                    )
            if resource["state"] == "active" and vm.get("status") not in {None, "running"}:
                issue(
                    issues,
                    "error",
                    "vm-status-mismatch",
                    "active registry resource is not running",
                    name,
                )

        if ha_resources is not None:
            observed_ha = ha_resources.get(resource["vmid"])
            if resource["kind"] == "staging":
                if observed_ha is not None:
                    issue(
                        issues,
                        "error",
                        "staging-is-ha",
                        "staging VM must never be an HA resource",
                        name,
                    )
            elif observed_ha is None:
                if resource["state"] in {"ready", "active"}:
                    issue(issues, "error", "ha-resource-missing", "HA resource is absent", name)
            else:
                observed_ha_nodes = validate_nodes(observed_ha["nodes"], policy, allow_empty=True)
                if set(observed_ha_nodes) != set(resource["placement"]):
                    issue(
                        issues,
                        "error",
                        "ha-placement-mismatch",
                        "HA node-affinity set differs from registry placement",
                        name,
                    )

        if replications_by_vmid is not None and resource["kind"] == "production":
            current_node = vm.get("node") if vm is not None else resource["owner_node"]
            expected_targets = set(resource["placement"])
            if current_node in expected_targets:
                expected_targets.remove(current_node)
            observed_jobs = replications_by_vmid.get(resource["vmid"], [])
            healthy_targets = {
                job["target"] for job in observed_jobs if job["healthy"] is True
            }
            unhealthy_targets = {
                job["target"] for job in observed_jobs if job["healthy"] is False
            }
            if healthy_targets != expected_targets:
                issue(
                    issues,
                    "error",
                    "replication-placement-mismatch",
                    "healthy replication targets differ from placement minus current owner",
                    name,
                )
            if unhealthy_targets:
                issue(
                    issues,
                    "error",
                    "replication-unhealthy",
                    f"unhealthy targets: {', '.join(sorted(unhealthy_targets))}",
                    name,
                )

        volume_id = resource["proxmox"]["volume_id"]
        if volumes is not None and volume_id is not None and volume_id not in volumes:
            issue(issues, "error", "volume-missing", f"volume {volume_id} is absent", name)

        snapshot = resource["proxmox"]["snapshot"]
        if snapshot_entries is not None and snapshot is not None:
            matching = [
                entry
                for entry in snapshot_entries
                if entry.get("resource") == name and entry.get("snapshot") == snapshot["name"]
            ]
            observed_guids: dict[str, str] = {}
            for entry in matching:
                observed_guids[entry["node"]] = str(entry["guid"])
            snapshot_nodes = set(resource["placement"])
            if resource["kind"] == "staging":
                source = updated_by_name.get(resource["source"])
                if source is not None:
                    snapshot_nodes = set(source["placement"])
            missing = sorted(snapshot_nodes - set(observed_guids))
            if missing:
                issue(
                    issues,
                    "error",
                    "snapshot-replica-missing",
                    f"snapshot missing on placement nodes: {', '.join(missing)}",
                    name,
                )
            if len(set(observed_guids.values())) > 1:
                issue(
                    issues,
                    "error",
                    "snapshot-guid-mismatch",
                    "snapshot ZFS GUID differs across placement nodes",
                    name,
                )
            recorded = snapshot["guids"]
            mismatched = sorted(
                node
                for node, guid in recorded.items()
                if node in observed_guids and observed_guids[node] != guid
            )
            if mismatched:
                issue(
                    issues,
                    "error",
                    "snapshot-recorded-guid-mismatch",
                    f"observed GUID differs from registry on: {', '.join(mismatched)}",
                    name,
                )

    if vms is not None:
        registered_vmids = {resource["vmid"] for resource in updated_resources}
        for vmid, vm in vms.items():
            vm_name = vm.get("name")
            if (
                vmid not in registered_vmids
                and isinstance(vm_name, str)
                and RESOURCE_RE.fullmatch(vm_name)
            ):
                issue(
                    issues,
                    "warning",
                    "unregistered-guest",
                    f"observed orchestration-shaped VM {vm_name} ({vmid}) is unregistered",
                    vm_name,
                )

    if routes is not None:
        expected_routes = {route["domain"]: route for route in build_routes(updated_resources)}
        for domain, expected in expected_routes.items():
            actual = routes.get(domain)
            if actual is None:
                issue(
                    issues,
                    "error",
                    "route-missing",
                    f"expected route {domain} is absent",
                    expected["resource"],
                )
                continue
            if actual["resource"] != expected["resource"] or actual["ip"] != expected["ip"]:
                issue(
                    issues,
                    "error",
                    "route-mismatch",
                    f"route {domain} targets the wrong resource or IP",
                    expected["resource"],
                )
        for domain, actual in routes.items():
            if domain not in expected_routes:
                issue(
                    issues,
                    "warning",
                    "route-orphan",
                    f"observed route {domain} is not enabled in the registry",
                    actual.get("resource"),
                )

    completed = observed.get("cleanup_completed")
    if completed is not None:
        if not isinstance(completed, list) or not all(
            isinstance(cleanup_id, str) for cleanup_id in completed
        ):
            raise RegistryError("cleanup_completed must be an array of cleanup ids")
        completed_set = set(completed)
        known_cleanup = {record["id"] for record in updated_cleanup}
        unknown_completed = sorted(completed_set - known_cleanup)
        if unknown_completed:
            issue(
                issues,
                "warning",
                "cleanup-completion-unknown",
                f"unknown completed cleanup ids: {', '.join(unknown_completed)}",
            )
        if apply:
            for record in updated_cleanup:
                if record["id"] in completed_set and record["state"] != "completed":
                    record["state"] = "completed"
                    record["attempts"] += 1
                    record["updated_at"] = utc_now()
                    record["revision"] += 1
                    changes.append(
                        {
                            "cleanup": record["id"],
                            "change": "state",
                            "from": "pending",
                            "to": "completed",
                        }
                    )

    validate_registry_invariants(updated_resources)
    for resource in updated_resources:
        validate_resource(resource, policy)
    for record in updated_cleanup:
        validate_cleanup_record(record)
    report = {
        "schema_version": SCHEMA_VERSION,
        "record_type": "reconcile-report",
        "generated_at": utc_now(),
        "applied": apply,
        "checked_sections": checked_sections,
        "ok": not any(entry["severity"] == "error" for entry in issues),
        "issues": sorted(
            issues,
            key=lambda entry: (
                0 if entry["severity"] == "error" else 1,
                entry["code"],
                entry["resource"] or "",
            ),
        ),
        "changes": changes,
    }
    return report, updated_resources, updated_cleanup


def command_reconcile(registry: Registry, args: argparse.Namespace) -> Any:
    policy = registry.policy()
    observed = (
        collect_live_observation(registry, policy, args.pvesh)
        if args.live
        else read_observation(args.observed)
    )
    if args.apply:
        with registry.lock():
            resources = registry.resources()
            cleanup_records = registry.cleanup_records()
            report, updated_resources, updated_cleanup = reconcile_report(
                resources, cleanup_records, policy, observed, apply=True
            )
            original_resources = {record["name"]: record for record in resources}
            for record in updated_resources:
                if record != original_resources[record["name"]]:
                    atomic_write_json(registry.resource_path(record["name"]), record)
            original_cleanup = {record["id"]: record for record in cleanup_records}
            for record in updated_cleanup:
                if record != original_cleanup[record["id"]]:
                    atomic_write_json(registry.cleanup_path(record["id"]), record)
            registry.prune_completed_cleanup(
                {record["id"] for record in updated_resources}
            )
            return report
    resources = registry.resources()
    cleanup_records = registry.cleanup_records()
    report, _, _ = reconcile_report(
        resources, cleanup_records, policy, observed, apply=False
    )
    return report


def add_spec_arguments(parser: argparse.ArgumentParser) -> None:
    parser.add_argument("--cores", type=int, help="vCPU count (policy default if omitted)")
    parser.add_argument(
        "--memory-mb", type=int, help="memory in MiB (policy default if omitted)"
    )
    parser.add_argument(
        "--disk-gib", type=int, help="root disk size in GiB (policy default if omitted)"
    )
    parser.add_argument(
        "--disk-allocation",
        choices=("sparse", "reserved"),
        help="zvol allocation policy",
    )
    parser.add_argument(
        "--replication-interval",
        help="positive interval in minutes (policy default if omitted)",
    )


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(
        description=(
            "Schema-versioned pmxcfs registry and deterministic IPAM for "
            "production and staging Proxmox guests."
        )
    )
    parser.add_argument(
        "--state-dir",
        type=Path,
        default=DEFAULT_STATE_DIR,
        help=f"registry root (default: {DEFAULT_STATE_DIR})",
    )
    parser.add_argument(
        "--lock-timeout",
        type=float,
        default=15.0,
        help="seconds to wait for the atomic allocator lock",
    )
    parser.add_argument(
        "--stale-lock-seconds",
        type=float,
        default=900.0,
        help=(
            "minimum age before reaping a custom-root lock; pmxcfs applies "
            "its own cluster-wide expiry check"
        ),
    )
    parser.add_argument(
        "--break-stale-lock",
        action="store_true",
        help="explicitly request safe stale-lock removal while acquiring",
    )
    parser.add_argument("--version", action="version", version=f"%(prog)s schema {SCHEMA_VERSION}")
    subparsers = parser.add_subparsers(dest="command", required=True)

    init = subparsers.add_parser("init", help="initialize policy and registry directories")
    init.add_argument("--network", required=True)
    init.add_argument("--guest-gateway", required=True)
    init.add_argument("--mox-ip-start", required=True)
    init.add_argument("--mox-ip-end", required=True)
    init.add_argument("--haproxy-ip-start", required=True)
    init.add_argument("--haproxy-ip-end", required=True)
    init.add_argument("--production-ip-start", required=True)
    init.add_argument("--production-ip-end", required=True)
    init.add_argument("--staging-ip-start", required=True)
    init.add_argument("--staging-ip-end", required=True)
    init.add_argument("--max-hosts", type=int, default=10)
    init.add_argument("--production-tag", default="app-ha-production")
    init.add_argument("--staging-tag", default="app-ha-staging")
    init.add_argument("--evictable-tag", default="evictable")
    init.add_argument("--reservation-ttl-seconds", type=int, default=3600)
    init.add_argument("--prod-cores", type=int, default=4)
    init.add_argument("--prod-memory-mb", type=int, default=8192)
    init.add_argument("--prod-disk-gib", type=int, default=64)
    init.add_argument(
        "--prod-disk-allocation", choices=("sparse", "reserved"), default="reserved"
    )
    init.add_argument("--staging-cores", type=int, default=2)
    init.add_argument("--staging-memory-mb", type=int, default=4096)
    init.add_argument("--staging-disk-gib", type=int, default=64)
    init.add_argument("--replication-interval", default="15")
    init.set_defaults(handler=command_init)

    list_parser = subparsers.add_parser("list", help="list registry records")
    list_parser.add_argument(
        "--record-type",
        choices=("resources", "cleanup", "history"),
        default="resources",
    )
    list_parser.add_argument("--kind", choices=("production", "staging"))
    list_parser.add_argument("--state")
    list_parser.set_defaults(handler=command_list)

    get = subparsers.add_parser("get", help="get one resource or cleanup record")
    get.add_argument("name")
    get.add_argument("--cleanup", action="store_true")
    get.set_defaults(handler=command_get)

    prod = subparsers.add_parser(
        "allocate-prod", help="atomically reserve the next prodN and production-range IP"
    )
    prod.add_argument("--vmid", type=int, required=True)
    prod.add_argument("--purpose", required=True)
    prod.add_argument("--primary-domain", required=True)
    prod.add_argument("--alias", action="append")
    prod.add_argument("--staging-dns-base")
    prod.add_argument("--placement", action="append", required=True)
    prod.add_argument("--initial-node")
    prod.add_argument("--allocation-id")
    add_spec_arguments(prod)
    prod.set_defaults(handler=command_allocate_prod)

    staging = subparsers.add_parser(
        "allocate-staging",
        help="atomically reserve the next prodXsY and staging-range IP",
    )
    staging.add_argument("--source", required=True)
    staging.add_argument("--vmid", type=int, required=True)
    staging.add_argument("--placement", action="append", required=True)
    staging.add_argument("--placement-limit", type=int, required=True)
    staging.add_argument("--domain")
    staging.add_argument("--allocation-id")
    add_spec_arguments(staging)
    staging.set_defaults(handler=command_allocate_staging)

    staging_snapshot_intent = subparsers.add_parser(
        "record-staging-snapshot-intent",
        help=(
            "persist unique snapshot ownership and every source volume before "
            "creating the Proxmox snapshot"
        ),
    )
    staging_snapshot_intent.add_argument("name")
    staging_snapshot_intent.add_argument("--expected-revision", type=int)
    staging_snapshot_intent.add_argument("--snapshot-name", required=True)
    staging_snapshot_intent.add_argument("--snapshot-owner-node", required=True)
    staging_snapshot_intent.add_argument("--source-volume-id", required=True)
    staging_snapshot_intent.add_argument(
        "--snapshotted-volume-id",
        action="append",
        required=True,
        metavar="STORAGE:VOLUME",
    )
    staging_snapshot_intent.set_defaults(
        handler=command_record_staging_snapshot_intent
    )

    staging_snapshot = subparsers.add_parser(
        "record-staging-snapshot",
        help=(
            "record a verified unique production snapshot dependency and "
            "its cluster-wide ZFS GUID"
        ),
    )
    staging_snapshot.add_argument("name")
    staging_snapshot.add_argument("--expected-revision", type=int)
    staging_snapshot.add_argument("--snapshot-name", required=True)
    staging_snapshot.add_argument("--snapshot-owner-node", required=True)
    staging_snapshot.add_argument("--source-volume-id", required=True)
    staging_snapshot.add_argument(
        "--snapshot-guid",
        action="append",
        metavar="MOXN=GUID",
        required=True,
    )
    staging_snapshot.add_argument(
        "--snapshot-volume-guid",
        action="append",
        metavar="STORAGE:VOLUME,MOXN=GUID",
        required=True,
    )
    staging_snapshot.set_defaults(handler=command_record_staging_snapshot)

    update = subparsers.add_parser(
        "update", help="perform a validated resource state/metadata transition"
    )
    update.add_argument("name")
    update.add_argument("--expected-revision", type=int)
    update.add_argument("--state", choices=sorted(RESOURCE_STATES))
    update.add_argument("--placement", action="append")
    update.add_argument("--initial-node")
    owner = update.add_mutually_exclusive_group()
    owner.add_argument("--owner-node")
    owner.add_argument("--clear-owner-node", action="store_true")
    route_group = update.add_mutually_exclusive_group()
    route_group.add_argument(
        "--routes-enabled", dest="routes_enabled", action="store_true"
    )
    route_group.add_argument(
        "--routes-disabled", dest="routes_enabled", action="store_false"
    )
    update.set_defaults(routes_enabled=None)
    update.add_argument("--purpose", help="new unique production purpose slug")
    update.add_argument("--primary-domain")
    aliases = update.add_mutually_exclusive_group()
    aliases.add_argument(
        "--alias",
        action="append",
        help="replace aliases; repeat for multiple exact domains",
    )
    aliases.add_argument("--clear-aliases", action="store_true")
    update.add_argument("--staging-dns-base")
    update.add_argument("--ha-nodes", action="append")
    update.add_argument("--replication-targets", action="append")
    volume = update.add_mutually_exclusive_group()
    volume.add_argument("--volume-id")
    volume.add_argument("--clear-volume", action="store_true")
    snapshot = update.add_mutually_exclusive_group()
    snapshot.add_argument("--snapshot-name")
    snapshot.add_argument("--clear-snapshot", action="store_true")
    update.add_argument("--snapshot-owner-node")
    update.add_argument("--snapshot-guid", action="append", metavar="MOXN=GUID")
    update.set_defaults(handler=command_update)

    orchestration_get = subparsers.add_parser(
        "orchestration-get",
        help="read durable production install phase and lease metadata",
    )
    orchestration_get.add_argument("name")
    orchestration_get.set_defaults(handler=command_orchestration_get)

    orchestration_acquire = subparsers.add_parser(
        "orchestration-acquire",
        help="acquire or renew one production resource orchestration lease",
    )
    orchestration_acquire.add_argument("name")
    orchestration_acquire.add_argument("--nonce", required=True)
    orchestration_acquire.add_argument("--owner", required=True)
    orchestration_acquire.add_argument(
        "--ttl-seconds", type=int, default=86400
    )
    orchestration_acquire.add_argument(
        "--initial-install-phase",
        choices=("unknown", "unstarted"),
        default="unknown",
    )
    orchestration_acquire.add_argument(
        "--final-network-enabled",
        choices=("yes", "no"),
        default="yes",
    )
    orchestration_acquire.add_argument(
        "--startup-sha256",
        default="none",
    )
    orchestration_acquire.add_argument("--source-iso-sha256", required=True)
    orchestration_acquire.add_argument(
        "--install-mode",
        choices=("ubuntu-autoinstall", "manual"),
        required=True,
    )
    orchestration_acquire.set_defaults(handler=command_orchestration_acquire)

    orchestration_phase = subparsers.add_parser(
        "orchestration-set-phase",
        help="advance the durable production install phase under its lease",
    )
    orchestration_phase.add_argument("name")
    orchestration_phase.add_argument("--nonce", required=True)
    orchestration_phase.add_argument(
        "--phase", choices=sorted(INSTALL_PHASES), required=True
    )
    orchestration_phase.add_argument("--confirm-wipe")
    orchestration_phase.set_defaults(handler=command_orchestration_set_phase)

    orchestration_release = subparsers.add_parser(
        "orchestration-release",
        help="release one production resource orchestration lease",
    )
    orchestration_release.add_argument("name")
    orchestration_release.add_argument("--nonce", required=True)
    orchestration_release.set_defaults(handler=command_orchestration_release)

    release = subparsers.add_parser(
        "release", help="archive and free a stopped/failed allocation"
    )
    release.add_argument("name")
    release.add_argument("--force", action="store_true")
    release.set_defaults(handler=command_release)

    cleanup = subparsers.add_parser(
        "defer-cleanup", help="idempotently queue cleanup for an unreachable node"
    )
    cleanup.add_argument("--resource", required=True)
    cleanup.add_argument("--node", required=True)
    cleanup.add_argument("--action", required=True, choices=sorted(CLEANUP_ACTIONS))
    cleanup.add_argument("--target", required=True)
    cleanup.add_argument("--reason", required=True)
    cleanup.set_defaults(handler=command_defer_cleanup)

    finalize_cleanup = subparsers.add_parser(
        "finalize-staging-cleanup",
        help=(
            "atomically release cleanup_pending staging after every deferred "
            "action completed"
        ),
    )
    finalize_cleanup.add_argument("name")
    finalize_cleanup.add_argument(
        "--resource-id",
        required=True,
        help="expected staging resource UUID (guards name reuse)",
    )
    finalize_cleanup.set_defaults(handler=command_finalize_staging_cleanup)

    routes = subparsers.add_parser(
        "list-routes", help="render exact enabled Host/SNI registry routes as JSON"
    )
    routes.set_defaults(handler=command_list_routes)

    ingress_begin = subparsers.add_parser(
        "ingress-begin",
        help=(
            "durably publish one desired HAProxy generation before applying it"
        ),
    )
    ingress_begin.add_argument("--generation", required=True)
    ingress_begin.add_argument("--bundle-sha256", required=True)
    ingress_begin.add_argument("--route-count", type=int, required=True)
    ingress_begin.add_argument(
        "--node",
        action="append",
        required=True,
        help="configured mox node; repeat in mox1..moxN order",
    )
    ingress_begin.set_defaults(handler=command_ingress_begin)

    ingress_mark = subparsers.add_parser(
        "ingress-mark",
        help="atomically record one node's desired-generation apply status",
    )
    ingress_mark.add_argument("--generation", required=True)
    ingress_mark.add_argument("--node", required=True)
    ingress_mark.add_argument(
        "--state", choices=sorted(INGRESS_NODE_STATES), required=True
    )
    ingress_mark.set_defaults(handler=command_ingress_mark)

    ingress_status = subparsers.add_parser(
        "ingress-status",
        help="read desired HAProxy generation and per-node apply statuses",
    )
    ingress_status.set_defaults(handler=command_ingress_status)

    reconcile = subparsers.add_parser(
        "reconcile",
        help="compare registry reservations with a typed Proxmox observation",
    )
    observation_source = reconcile.add_mutually_exclusive_group(required=True)
    observation_source.add_argument(
        "--observed",
        help="observation JSON file, or - for stdin",
    )
    observation_source.add_argument(
        "--live",
        action="store_true",
        help="collect safe node/VM/volume facts from the local Proxmox API",
    )
    reconcile.add_argument(
        "--pvesh",
        type=Path,
        default=Path("/usr/bin/pvesh"),
        help="absolute pvesh path used with --live (default: /usr/bin/pvesh)",
    )
    reconcile.add_argument(
        "--apply",
        action="store_true",
        help="mark stale reservations failed and explicit cleanup ids completed",
    )
    reconcile.set_defaults(handler=command_reconcile)
    return parser


def require_privilege(state_dir: Path) -> None:
    """The real pmxcfs hierarchy is root-only; custom roots are test fixtures."""

    try:
        state_dir.relative_to(PMXCFS_ROOT)
    except ValueError:
        return
    if os.geteuid() != 0:
        raise RegistryError("registry operations under /etc/pve must run as root")


def main(argv: Sequence[str] | None = None) -> int:
    parser = build_parser()
    args = parser.parse_args(argv)
    if args.lock_timeout < 0:
        parser.error("--lock-timeout must not be negative")
    if args.stale_lock_seconds < 1:
        parser.error("--stale-lock-seconds must be at least 1")
    try:
        args.state_dir = Path(os.path.abspath(args.state_dir))
        require_privilege(args.state_dir)
        registry = Registry(
            args.state_dir,
            lock_timeout=args.lock_timeout,
            stale_lock_seconds=args.stale_lock_seconds,
            break_stale_lock=args.break_stale_lock,
        )
        result = args.handler(registry, args)
        output_json(result)
        return 0
    except RegistryError as exc:
        print(f"ERROR: {exc}", file=sys.stderr)
        return 1
    except BrokenPipeError:
        return 0


if __name__ == "__main__":
    raise SystemExit(main())
