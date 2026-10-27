#!/usr/bin/env python3

# Copyright (c) 2026 BEENTHERE VENTURES, INC.
# SPDX-License-Identifier: GPL-3.0-only

"""Read-only checks of this cluster's Proxmox Backup Server account.

PVE's own tools cover backups and restores, but not the PBS API calls BMAC
needs: the token's effective privileges, the namespace's prune job (the
retention policy), and the snapshots already in the namespace with the key
fingerprint each was encrypted with. This helper makes those GET requests
with the cluster's API token.

The server's certificate is accepted only when its SHA-256 fingerprint equals
the configured one, so the token is never sent to anything else. With
--storage the connection settings come from the PVE storage's section in
/etc/pve/storage.cfg and the token from its root-only .pw file; otherwise
they come from the options and the token from the first line of stdin.

  pbs_api.py inspect --storage bmac-pbs
  pbs_api.py inspect --server H [--port P] --datastore D --namespace NS \\
      --username USER@REALM!TOKEN --fingerprint FP --token-stdin
  pbs_api.py groups --storage bmac-pbs

`inspect` prints one JSON object. `groups` prints each backup group in the
namespace with its newest backup time; the host monitor uses it to check that
PBS answers and that every scheduled VM's backups are current. Exit status: 0 when the server answered,
3 when its certificate fingerprint differs, 4 when it cannot be reached,
5 when it rejects the token, 2 for usage or configuration errors.
"""

from __future__ import annotations

import argparse
import hashlib
import http.client
import json
from pathlib import Path
import re
import socket
import ssl
import sys
from typing import Any, Callable, Sequence
from urllib.parse import quote


STORAGE_CFG = Path("/etc/pve/storage.cfg")
STORAGE_PRIV = Path("/etc/pve/priv/storage")
TIMEOUT_SECONDS = 15

# The token must hold both on its namespace: Backup to write and restore its
# own groups, Audit to list the namespace and read its prune job.
REQUIRED_PRIVILEGES = ("Datastore.Backup", "Datastore.Audit")
# Any of these would let a compromised cluster erase its backup history or
# change what others may do.
DANGEROUS_PRIVILEGES = (
    "Datastore.Modify",
    "Datastore.Prune",
    "Datastore.Allocate",
    "Permissions.Modify",
    "Sys.Modify",
)


class ApiError(Exception):
    def __init__(self, message: str, exit_code: int) -> None:
        super().__init__(message)
        self.exit_code = exit_code


def normalize_fingerprint(value: str) -> str:
    return value.replace(":", "").strip().lower()


def read_storage(storage_id: str, config: Path = STORAGE_CFG) -> dict[str, str]:
    """The key/value lines of one `pbs: <id>` section of storage.cfg."""

    section: dict[str, str] | None = None
    for line in config.read_text(encoding="utf-8").splitlines():
        header = re.fullmatch(r"(\S+):\s*(\S+)\s*", line)
        if header:
            if section is not None:
                break
            if header.group(1) == "pbs" and header.group(2) == storage_id:
                section = {}
            continue
        if section is not None and line.strip():
            key, _, value = line.strip().partition(" ")
            section[key] = value.strip()
    if section is None:
        raise ApiError(f"storage {storage_id} is not a PBS storage in {config}", 2)
    for key in ("server", "datastore", "username", "fingerprint"):
        if not section.get(key):
            raise ApiError(f"storage {storage_id} has no {key}", 2)
    return section


class Client:
    """GET requests to one PBS server over a fingerprint-pinned TLS connection."""

    def __init__(self, server: str, port: int, username: str, token: str, fingerprint: str) -> None:
        self.server = server
        self.port = port
        self.authorization = f"PBSAPIToken={username}:{token}"
        self.fingerprint = normalize_fingerprint(fingerprint)
        # Certificate chains are not checked: PBS normally has a self-signed
        # certificate, and the exact fingerprint is a stronger check.
        self.context = ssl.create_default_context()
        self.context.check_hostname = False
        self.context.verify_mode = ssl.CERT_NONE

    def get(self, path: str) -> tuple[int, Any]:
        try:
            raw = socket.create_connection((self.server, self.port), timeout=TIMEOUT_SECONDS)
            tls = self.context.wrap_socket(raw, server_hostname=self.server)
        except (OSError, ssl.SSLError) as error:
            raise ApiError(f"cannot reach PBS at {self.server}:{self.port}: {error}", 4) from error
        try:
            certificate = tls.getpeercert(binary_form=True) or b""
            actual = hashlib.sha256(certificate).hexdigest()
            if actual != self.fingerprint:
                raise ApiError(
                    f"the certificate of {self.server}:{self.port} has fingerprint "
                    f"{':'.join(actual[i:i + 2] for i in range(0, len(actual), 2))}, "
                    "not the configured one; nothing was sent",
                    3,
                )
            connection = http.client.HTTPSConnection(self.server, self.port, context=self.context, timeout=TIMEOUT_SECONDS)
            connection.sock = tls
            connection.request("GET", "/api2/json" + path, headers={"Authorization": self.authorization})
            response = connection.getresponse()
            body = response.read().decode("utf-8", "replace")
        except (OSError, http.client.HTTPException) as error:
            raise ApiError(f"PBS at {self.server}:{self.port} did not answer: {error}", 4) from error
        finally:
            tls.close()
        if response.status == 401:
            raise ApiError("PBS rejected the API token (wrong token ID or secret)", 5)
        try:
            data = json.loads(body).get("data")
        except (ValueError, AttributeError):
            data = body.strip()
        return response.status, data


def privilege_report(permissions: dict[str, dict[str, Any]], store: str, namespace: str) -> dict[str, Any]:
    target = f"/datastore/{store}/{namespace}"
    granted = {name for name, value in (permissions.get(target) or {}).items() if value}
    missing = [name for name in REQUIRED_PRIVILEGES if name not in granted]
    excess = []
    for path, privileges in sorted(permissions.items()):
        for name, value in sorted((privileges or {}).items()):
            if value and not (path == target and name in REQUIRED_PRIVILEGES):
                excess.append({"path": path, "privilege": name, "dangerous": name in DANGEROUS_PRIVILEGES})
    return {"path": target, "missing": missing, "excess": excess}


def inspect(client: Client, store: str, namespace: str, username: str) -> dict[str, Any]:
    report: dict[str, Any] = {
        "server": f"{client.server}:{client.port}",
        "datastore": store,
        "namespace": namespace,
        "username": username,
    }
    status, version = client.get("/version")
    report["version"] = version.get("version") if status == 200 and isinstance(version, dict) else None

    status, permissions = client.get(f"/access/permissions?auth-id={quote(username, safe='')}")
    if status != 200 or not isinstance(permissions, dict):
        raise ApiError(f"PBS refused to list the token's permissions (HTTP {status}): {permissions}", 5)
    report["privileges"] = privilege_report(permissions, store, namespace)

    status, jobs = client.get("/config/prune")
    if status == 200 and isinstance(jobs, list):
        matching = [job for job in jobs if job.get("store") == store and job.get("ns", "") == namespace]
        report["prune"] = {"readable": True, "jobs": matching}
    else:
        report["prune"] = {"readable": False, "jobs": [], "error": f"HTTP {status}: {jobs}"}

    ns = quote(namespace, safe="")
    status, snapshots = client.get(f"/admin/datastore/{quote(store, safe='')}/snapshots?ns={ns}")
    if status == 200 and isinstance(snapshots, list):
        report["snapshots"] = {
            "readable": True,
            "count": len(snapshots),
            "groups": sorted({f"{s.get('backup-type')}/{s.get('backup-id')}" for s in snapshots}),
            "key_fingerprints": sorted({s["fingerprint"] for s in snapshots if s.get("fingerprint")}),
            "unencrypted": sum(1 for s in snapshots if not s.get("fingerprint")),
        }
    else:
        report["snapshots"] = {"readable": False, "error": f"HTTP {status}: {snapshots}"}
    return report


def groups(client: Client, store: str, namespace: str) -> dict[str, Any]:
    ns = quote(namespace, safe="")
    status, rows = client.get(f"/admin/datastore/{quote(store, safe='')}/groups?ns={ns}")
    if status != 200 or not isinstance(rows, list):
        raise ApiError(f"PBS refused to list the namespace's backup groups (HTTP {status}): {rows}", 5)
    return {"groups": sorted(
        ({"group": f"{row.get('backup-type')}/{row.get('backup-id')}",
          "last_backup": row.get("last-backup"), "count": row.get("backup-count")}
         for row in rows if isinstance(row, dict)),
        key=lambda row: row["group"])}


def main(
    argv: Sequence[str] | None = None,
    *,
    client_factory: Callable[..., Client] = Client,
    stdin: Any = None,
    storage_cfg: Path = STORAGE_CFG,
    storage_priv: Path = STORAGE_PRIV,
) -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("command", choices=["inspect", "groups"])
    parser.add_argument("--storage", help="read the settings of this PVE PBS storage")
    parser.add_argument("--server")
    parser.add_argument("--port", type=int, default=8007)
    parser.add_argument("--datastore")
    parser.add_argument("--namespace", default="")
    parser.add_argument("--username")
    parser.add_argument("--fingerprint")
    parser.add_argument("--token-stdin", action="store_true", help="read the token secret from stdin")
    args = parser.parse_args(argv)

    try:
        if args.storage:
            settings = read_storage(args.storage, storage_cfg)
            password = storage_priv / f"{args.storage}.pw"
            try:
                token = password.read_text(encoding="utf-8").strip()
            except OSError as error:
                raise ApiError(f"cannot read the token of storage {args.storage}: {error}", 2) from error
            server, store = settings["server"], settings["datastore"]
            port = int(settings.get("port") or 8007)
            namespace, username = settings.get("namespace", ""), settings["username"]
            fingerprint = settings["fingerprint"]
        else:
            if not (args.server and args.datastore and args.username and args.fingerprint and args.token_stdin):
                parser.error("without --storage, give --server, --datastore, --username, --fingerprint and --token-stdin")
            token = (stdin or sys.stdin).readline().strip()
            server, port, store = args.server, args.port, args.datastore
            namespace, username, fingerprint = args.namespace, args.username, args.fingerprint
        if not token:
            raise ApiError("the API token secret is empty", 2)
        client = client_factory(server, port, username, token, fingerprint)
        report = inspect(client, store, namespace, username) if args.command == "inspect" else \
            groups(client, store, namespace)
        print(json.dumps(report, indent=2, sort_keys=True))
        return 0
    except ApiError as error:
        print(f"pbs_api.py: {error}", file=sys.stderr)
        return error.exit_code


if __name__ == "__main__":
    sys.exit(main())
