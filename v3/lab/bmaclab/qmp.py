# Copyright (c) 2026 BEENTHERE VENTURES, INC.
# SPDX-License-Identifier: GPL-3.0-only

"""A minimal QEMU Machine Protocol client over a Unix socket."""

from __future__ import annotations

import json
import socket
from pathlib import Path


class QmpError(RuntimeError):
    pass


class Qmp:
    def __init__(self, path: Path, timeout: float = 10.0) -> None:
        self.timeout = timeout
        self.sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self.sock.settimeout(timeout)
        try:
            self.sock.connect(str(path))
        except OSError as error:
            self.sock.close()
            raise QmpError(f"cannot connect to {path}: {error}") from error
        self.buffer = b""
        self.pending_events: list[dict] = []
        try:
            greeting = self._read(timeout)
            if greeting is None or "QMP" not in greeting:
                raise QmpError(f"unexpected QMP greeting: {greeting}")
            self.command("qmp_capabilities")
        except BaseException:
            self.sock.close()
            raise

    def __enter__(self) -> Qmp:
        return self

    def __exit__(self, *exc: object) -> None:
        self.close()

    def close(self) -> None:
        self.sock.close()

    def _read(self, timeout: float | None) -> dict | None:
        """One message, or None when `timeout` passes first. A recv timeout
        leaves the buffer intact, so the connection stays usable."""
        self.sock.settimeout(timeout)
        while b"\n" not in self.buffer:
            try:
                chunk = self.sock.recv(65536)
            except TimeoutError:
                return None
            if not chunk:
                raise QmpError("QEMU closed the QMP connection")
            self.buffer += chunk
        line, self.buffer = self.buffer.split(b"\n", 1)
        return json.loads(line)

    def command(self, name: str, /, **arguments: object) -> object:
        message: dict = {"execute": name}
        if arguments:
            message["arguments"] = arguments
        self.sock.settimeout(self.timeout)
        self.sock.sendall(json.dumps(message).encode() + b"\n")
        while True:
            reply = self._read(self.timeout)
            if reply is None:
                raise QmpError(f"{name}: no reply within {self.timeout} seconds")
            if "event" in reply:
                self.pending_events.append(reply)
                continue
            if "error" in reply:
                raise QmpError(f"{name}: {reply['error'].get('desc', reply['error'])}")
            return reply.get("return")

    def event(self, timeout: float | None = None) -> dict | None:
        """The next asynchronous event, or None when `timeout` passes first."""
        if self.pending_events:
            return self.pending_events.pop(0)
        while True:
            message = self._read(timeout)
            if message is None or "event" in message:
                return message
