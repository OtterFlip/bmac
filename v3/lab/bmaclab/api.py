# Copyright (c) 2026 BEENTHERE VENTURES, INC.
# SPDX-License-Identifier: GPL-3.0-only

"""A client for bmacd's HTTPS API on a lab host, for scenarios.

bmacd listens only on a host's private and tailnet addresses, so the client
reaches it through an SSH tunnel to the host's management address. It pins
the host's certificate by its SHA-256 fingerprint, read over SSH, and keeps
the session token in memory only, as the Dashboard does.
"""

from __future__ import annotations

import hashlib
import http.client
import json
import socket
import ssl
import subprocess
import time

from .bootstrap import totp
from .topology import Vm

DASHBOARD_PORT = 8443


class Codes:
    """Fresh TOTP codes: each 30-second code is used once, and never one an earlier run may have used."""

    def __init__(self, secret: str) -> None:
        self.secret = secret
        self.last: int | None = int(time.time() // 30)

    def __call__(self) -> str:
        while int(time.time() // 30) == self.last:
            time.sleep(1)
        self.last = int(time.time() // 30)
        return totp(self.secret)


class ApiError(RuntimeError):
    def __init__(self, status: int, body: dict) -> None:
        super().__init__(f"HTTP {status}: {body.get('message') or body}")
        self.status = status
        self.body = body


def free_port() -> int:
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        return sock.getsockname()[1]


class Tunnel:
    """`ssh -L` from a free port on 127.0.0.1 to vm's management address."""

    def __init__(self, lab, vm: Vm, port: int = DASHBOARD_PORT) -> None:
        self.lab, self.vm, self.remote = lab, vm, port
        self.local = free_port()
        self.process: subprocess.Popen | None = None

    def __enter__(self) -> Tunnel:
        args = self.lab.ssh_args(self.vm, [])
        forward = ["-N", "-o", "ExitOnForwardFailure=yes", "-L", f"127.0.0.1:{self.local}:{self.vm.mgmt_ip}:{self.remote}"]
        self.process = subprocess.Popen([*args[:-1], *forward, args[-1]], stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL, stderr=subprocess.PIPE)
        deadline = time.monotonic() + 20
        while True:
            if self.process.poll() is not None:
                raise RuntimeError(f"the SSH tunnel to {self.vm.name} failed: {self.process.stderr.read().decode().strip()}")
            try:
                socket.create_connection(("127.0.0.1", self.local), timeout=1).close()
                return self
            except OSError:
                if time.monotonic() > deadline:
                    raise RuntimeError(f"the SSH tunnel to {self.vm.name} did not open") from None
                time.sleep(0.2)

    def __exit__(self, *exc) -> None:
        if self.process:
            self.process.terminate()
            self.process.wait(10)


def pinned_fingerprint(lab, vm: Vm) -> str:
    out = lab.ssh(vm, "openssl x509 -in /etc/bmac/tls/https.pem -noout -fingerprint -sha256").stdout
    return out.strip().split("=", 1)[1].replace(":", "").lower()


class Api:
    def __init__(self, port: int, fingerprint: str, name: str = "") -> None:
        self.port, self.fingerprint, self.name = port, fingerprint, name
        self.token: str | None = None

    def request(self, method: str, path: str, body: dict | None = None) -> dict:
        context = ssl.SSLContext(ssl.PROTOCOL_TLS_CLIENT)
        context.check_hostname = False
        context.verify_mode = ssl.CERT_NONE
        connection = http.client.HTTPSConnection("127.0.0.1", self.port, context=context, timeout=30)
        try:
            connection.connect()
            seen = hashlib.sha256(connection.sock.getpeercert(binary_form=True)).hexdigest()
            if seen != self.fingerprint:
                raise RuntimeError(f"{self.name}: the certificate's fingerprint is {seen}, not the pinned {self.fingerprint}")
            headers = {"Accept": "application/json"}
            data = None
            if body is not None:
                data = json.dumps(body).encode()
                headers["Content-Type"] = "application/json"
            if self.token:
                headers["Authorization"] = f"Bearer {self.token}"
            connection.request(method, f"/api/v1{path}", body=data, headers=headers)
            response = connection.getresponse()
            payload = response.read()
            parsed = json.loads(payload) if payload else {}
            if response.status >= 400:
                raise ApiError(response.status, parsed)
            return parsed
        finally:
            connection.close()

    def login(self, user: str, password: str, totp=None) -> dict:
        reply = self.request("POST", "/login", {"step": "password", "user": user, "password": password})
        if reply.get("step") == "second_factor":
            if totp is None:
                raise RuntimeError(f"{user} needs a second factor")
            reply = self.request("POST", "/login", {"step": "second_factor", "challenge": reply["challenge"], "method": "totp", "response": totp()})
        self.token = reply["token"]
        return reply

    def logout(self) -> None:
        if self.token:
            try:
                self.request("POST", "/logout", {})
            finally:
                self.token = None
