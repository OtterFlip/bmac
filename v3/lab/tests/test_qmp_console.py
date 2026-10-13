# Copyright (c) 2026 BEENTHERE VENTURES, INC.
# SPDX-License-Identifier: GPL-3.0-only

"""The QMP client and the serial console driver, against fake QEMU sockets."""

from __future__ import annotations

import json
import socket
import sys
import tempfile
import threading
import unittest
from pathlib import Path

LAB = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(LAB))

from bmaclab.console import Console, ConsoleTimeout  # noqa: E402
from bmaclab.qmp import Qmp, QmpError  # noqa: E402


class FakeServer:
    """Accepts one connection on a Unix socket and runs `handler(conn)` on a thread."""

    def __init__(self, path: Path, handler) -> None:
        self.listener = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self.listener.bind(str(path))
        self.listener.listen(1)
        self.received: list[bytes] = []
        self.thread = threading.Thread(target=self._serve, args=(handler,), daemon=True)
        self.thread.start()

    def _serve(self, handler) -> None:
        conn, _ = self.listener.accept()
        with conn:
            handler(self, conn)

    def close(self) -> None:
        self.listener.close()
        self.thread.join(timeout=5)


def read_line(conn: socket.socket) -> dict:
    data = b""
    while not data.endswith(b"\n"):
        chunk = conn.recv(1)
        if not chunk:
            raise ConnectionError
        data += chunk
    return json.loads(data)


def send(conn: socket.socket, message: dict) -> None:
    conn.sendall(json.dumps(message).encode() + b"\n")


class QmpTest(unittest.TestCase):
    def setUp(self) -> None:
        scratch = tempfile.TemporaryDirectory(prefix="bmac-lab-qmp-")
        self.addCleanup(scratch.cleanup)
        self.path = Path(scratch.name, "qmp.sock")

    def serve(self, script) -> FakeServer:
        def handler(server: FakeServer, conn: socket.socket) -> None:
            send(conn, {"QMP": {"version": {}, "capabilities": []}})
            self.assertEqual(read_line(conn), {"execute": "qmp_capabilities"})
            send(conn, {"return": {}})
            script(server, conn)

        server = FakeServer(self.path, handler)
        self.addCleanup(server.close)
        return server

    def test_command_returns_its_reply_and_queues_interleaved_events(self) -> None:
        release = threading.Event()

        def script(server: FakeServer, conn: socket.socket) -> None:
            server.received.append(json.dumps(read_line(conn)).encode())
            send(conn, {"event": "RESET", "data": {"guest": True}})
            send(conn, {"return": {"status": "running"}})
            release.wait(5)

        server = self.serve(script)
        with Qmp(self.path) as monitor:
            self.assertEqual(monitor.command("query-status"), {"status": "running"})
            self.assertEqual(monitor.event(timeout=0)["event"], "RESET")
            release.set()
        self.assertEqual(json.loads(server.received[0]), {"execute": "query-status"})

    def test_arguments_are_sent_and_errors_raise(self) -> None:
        seen = []

        def script(server: FakeServer, conn: socket.socket) -> None:
            seen.append(read_line(conn))
            send(conn, {"error": {"class": "DeviceNotFound", "desc": "Device 'nic-x' not found"}})

        self.serve(script)
        with Qmp(self.path) as monitor, self.assertRaisesRegex(QmpError, "set_link: Device 'nic-x' not found"):
            monitor.command("set_link", name="nic-x", up=False)
        self.assertEqual(seen, [{"execute": "set_link", "arguments": {"name": "nic-x", "up": False}}])

    def test_an_event_timeout_leaves_the_connection_usable(self) -> None:
        go = threading.Event()

        def script(server: FakeServer, conn: socket.socket) -> None:
            go.wait(5)
            send(conn, {"event": "SHUTDOWN", "data": {}})
            read_line(conn)
            send(conn, {"return": {}})

        self.serve(script)
        with Qmp(self.path) as monitor:
            self.assertIsNone(monitor.event(timeout=0.05))
            go.set()
            self.assertEqual(monitor.event(timeout=5)["event"], "SHUTDOWN")
            self.assertEqual(monitor.command("cont"), {})

    def test_a_reset_connection_is_a_qmp_error(self) -> None:
        def handler(server: FakeServer, conn: socket.socket) -> None:
            send(conn, {"QMP": {"version": {}, "capabilities": []}})
            # Closing with the command unread resets the connection, as a
            # VM killed mid-handshake does.
            conn.recv(1, socket.MSG_PEEK)

        server = FakeServer(self.path, handler)
        self.addCleanup(server.close)
        with self.assertRaises(QmpError):
            Qmp(self.path)

    def test_no_server_is_a_qmp_error(self) -> None:
        with self.assertRaisesRegex(QmpError, "cannot connect"):
            Qmp(self.path)


class ConsoleTest(unittest.TestCase):
    def setUp(self) -> None:
        scratch = tempfile.TemporaryDirectory(prefix="bmac-lab-console-")
        self.addCleanup(scratch.cleanup)
        self.path = Path(scratch.name, "console.sock")

    def test_expect_sees_text_through_escapes_split_across_reads(self) -> None:
        got = []
        done = threading.Event()

        def handler(server: FakeServer, conn: socket.socket) -> None:
            got.append(conn.recv(100))
            conn.sendall(b"\x1b[2J\x1b[1;1HDebian GNU/Linux 13 mox1 ttyS0\r\n\r\nmox1 lo\x1b[")
            conn.sendall(b"0mgin: ")
            done.wait(5)

        server = FakeServer(self.path, handler)
        self.addCleanup(server.close)
        with Console(self.path, connect_timeout=5) as console:
            console.line()
            match = console.expect(r"(\S+) login:", timeout=5)
            self.assertEqual(match.group(1), "mox1")
            self.assertNotIn("\x1b", console.text)
            done.set()
        self.assertEqual(got, [b"\r"])

    def test_expect_times_out_with_the_screen_tail(self) -> None:
        done = threading.Event()

        def handler(server: FakeServer, conn: socket.socket) -> None:
            conn.sendall(b"Please unlock disk rpool1: ")
            done.wait(5)

        server = FakeServer(self.path, handler)
        self.addCleanup(server.close)
        with Console(self.path, connect_timeout=5) as console:
            with self.assertRaises(ConsoleTimeout) as caught:
                console.expect("login:", timeout=0.3)
            self.assertIn("Please unlock disk rpool1", caught.exception.tail)
            self.assertEqual(console.expect(r"unlock disk (\w+)", timeout=1).group(1), "rpool1")
            done.set()


if __name__ == "__main__":
    unittest.main()
