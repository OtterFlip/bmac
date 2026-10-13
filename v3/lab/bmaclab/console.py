# Copyright (c) 2026 BEENTHERE VENTURES, INC.
# SPDX-License-Identifier: GPL-3.0-only

"""Drives a VM's serial console: send keystrokes, wait for text."""

from __future__ import annotations

import re
import socket
import time
from pathlib import Path

# CSI and OSC sequences, and the other two-byte escapes, which installers and
# getty paint the screen with.
ANSI = re.compile(rb"\x1b(?:\[[0-?]*[ -/]*[@-~]|\][^\x07\x1b]*(?:\x07|\x1b\\)|[@-Z\\-_])")


class ConsoleTimeout(TimeoutError):
    def __init__(self, pattern: str, tail: str) -> None:
        super().__init__(f"timed out waiting for {pattern!r}; the console last showed:\n{tail}")
        self.tail = tail


class Console:
    def __init__(self, path: Path, connect_timeout: float = 30.0) -> None:
        deadline = time.monotonic() + connect_timeout
        while True:
            self.sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
            try:
                self.sock.connect(str(path))
                break
            except OSError:
                self.sock.close()
                if time.monotonic() > deadline:
                    raise
                time.sleep(0.2)
        self.raw = b""
        self.text = ""  # everything seen, escapes removed
        self.consumed = 0  # expect() searches only past this point

    def __enter__(self) -> Console:
        return self

    def __exit__(self, *exc: object) -> None:
        self.close()

    def close(self) -> None:
        self.sock.close()

    def send(self, text: str) -> None:
        self.sock.sendall(text.encode())

    def line(self, text: str = "") -> None:
        self.send(text + "\r")

    def expect(self, pattern: str, timeout: float = 60.0) -> re.Match[str]:
        regex = re.compile(pattern)
        deadline = time.monotonic() + timeout
        while True:
            match = regex.search(self.text, self.consumed)
            if match:
                self.consumed = match.end()
                return match
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                raise ConsoleTimeout(pattern, self.text[-800:])
            self.sock.settimeout(min(remaining, 1.0))
            try:
                chunk = self.sock.recv(65536)
            except TimeoutError:
                continue
            if not chunk:
                raise ConnectionError("the console closed")
            # Cleaned as a whole, so an escape split across reads is still removed.
            self.raw += chunk
            self.text = ANSI.sub(b"", self.raw).replace(b"\r", b"").decode("utf-8", "replace")
