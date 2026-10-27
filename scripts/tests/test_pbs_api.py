# Copyright (c) 2026 BEENTHERE VENTURES, INC.
# SPDX-License-Identifier: GPL-3.0-only

"""Tests for scripts/host_runtime/pbs_api.py."""

from __future__ import annotations

import contextlib
import hashlib
import http.server
import importlib.util
import io
import json
from pathlib import Path
import shutil
import ssl
import subprocess
import tempfile
import threading
import unittest


PROGRAM = Path(__file__).resolve().parent.parent / "host_runtime" / "pbs_api.py"
SPEC = importlib.util.spec_from_file_location("pbs_api", PROGRAM)
assert SPEC and SPEC.loader
pbs_api = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(pbs_api)

TOKEN = "0d6a2f4e-9b1c-4d3e-8f7a-1234567890ab"
FINGERPRINT = ":".join(["5d"] * 32)

STORAGE_CFG = """dir: local
\tpath /var/lib/vz
\tcontent iso,backup

pbs: bmac-pbs
\tdatastore pbs
\tserver 203.0.113.7
\tcontent backup
\tfingerprint {fingerprint}
\tnamespace BTV
\tprune-backups keep-all=1
\tusername btv@pbs!pve

zfspool: local-zfs
\tpool rpool/data
"""

# Real answers from a PBS 4.2 server for a token holding DatastoreBackup and
# DatastoreAudit on its namespace, plus snapshot rows in the documented shape.
PERMISSIONS = {"/datastore/pbs/BTV": {"Datastore.Audit": True, "Datastore.Backup": True}}
PRUNE_JOBS = [
    {"id": "prune-btv", "keep-daily": 3, "keep-last": 6, "keep-weekly": 4, "ns": "BTV", "schedule": "05:00", "store": "pbs"},
    {"id": "prune-btv-sub", "keep-last": 1, "ns": "BTV/sub", "schedule": "05:00", "store": "pbs"},
]
SNAPSHOTS = [
    {"backup-type": "vm", "backup-id": "100", "backup-time": 1793080800, "fingerprint": "aa:bb", "protected": False},
    {"backup-type": "vm", "backup-id": "100", "backup-time": 1793095200, "fingerprint": "aa:bb", "protected": True},
    {"backup-type": "vm", "backup-id": "101", "backup-time": 1793095200},
]


class FakeClient:
    def __init__(self, answers: dict[str, tuple[int, object]]) -> None:
        self.answers = answers
        self.server, self.port = "203.0.113.7", 8007
        self.paths: list[str] = []

    def get(self, path: str) -> tuple[int, object]:
        self.paths.append(path)
        for prefix, answer in self.answers.items():
            if path.startswith(prefix):
                return answer
        raise AssertionError(f"unexpected request {path}")


def answers(**overrides: tuple[int, object]) -> dict[str, tuple[int, object]]:
    base = {
        "/version": (200, {"release": "8", "version": "4.2"}),
        "/access/permissions": (200, PERMISSIONS),
        "/config/prune": (200, PRUNE_JOBS),
        "/admin/datastore/pbs/snapshots": (200, SNAPSHOTS),
    }
    base.update(overrides)
    return base


class StorageConfigTest(unittest.TestCase):
    def test_reads_only_the_named_pbs_section(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            config = Path(temporary) / "storage.cfg"
            config.write_text(STORAGE_CFG.format(fingerprint=FINGERPRINT), encoding="utf-8")
            settings = pbs_api.read_storage("bmac-pbs", config)
            self.assertEqual(settings["server"], "203.0.113.7")
            self.assertEqual(settings["namespace"], "BTV")
            self.assertEqual(settings["username"], "btv@pbs!pve")
            self.assertNotIn("pool", settings)
            with self.assertRaises(pbs_api.ApiError) as missing:
                pbs_api.read_storage("local", config)
            self.assertEqual(missing.exception.exit_code, 2)


class PrivilegeTest(unittest.TestCase):
    def test_exact_roles_on_the_namespace_pass(self) -> None:
        report = pbs_api.privilege_report(PERMISSIONS, "pbs", "BTV")
        self.assertEqual(report, {"path": "/datastore/pbs/BTV", "missing": [], "excess": []})

    def test_missing_roles_and_excess_privileges_are_reported(self) -> None:
        permissions = {
            "/datastore/pbs/BTV": {"Datastore.Backup": True, "Datastore.Prune": True, "Datastore.Audit": False},
            "/datastore/pbs": {"Datastore.Read": True},
        }
        report = pbs_api.privilege_report(permissions, "pbs", "BTV")
        self.assertEqual(report["missing"], ["Datastore.Audit"])
        self.assertEqual(
            report["excess"],
            [
                {"path": "/datastore/pbs", "privilege": "Datastore.Read", "dangerous": False},
                {"path": "/datastore/pbs/BTV", "privilege": "Datastore.Prune", "dangerous": True},
            ],
        )


class InspectTest(unittest.TestCase):
    def test_reports_policy_snapshots_and_key_fingerprints(self) -> None:
        client = FakeClient(answers())
        report = pbs_api.inspect(client, "pbs", "BTV", "btv@pbs!pve")
        self.assertEqual(report["version"], "4.2")
        self.assertEqual([job["id"] for job in report["prune"]["jobs"]], ["prune-btv"])
        self.assertTrue(report["prune"]["readable"])
        self.assertEqual(report["snapshots"]["count"], 3)
        self.assertEqual(report["snapshots"]["groups"], ["vm/100", "vm/101"])
        self.assertEqual(report["snapshots"]["key_fingerprints"], ["aa:bb"])
        self.assertEqual(report["snapshots"]["unencrypted"], 1)
        self.assertIn("/access/permissions?auth-id=btv%40pbs%21pve", client.paths)
        self.assertIn("/admin/datastore/pbs/snapshots?ns=BTV", client.paths)

    def test_an_unreadable_policy_is_reported_not_guessed(self) -> None:
        client = FakeClient(answers(**{"/config/prune": (403, "permission check failed")}))
        report = pbs_api.inspect(client, "pbs", "BTV", "btv@pbs!pve")
        self.assertEqual(report["prune"]["jobs"], [])
        self.assertFalse(report["prune"]["readable"])
        self.assertIn("403", report["prune"]["error"])

    def test_refused_permission_listing_is_an_error(self) -> None:
        client = FakeClient(answers(**{"/access/permissions": (403, "permission check failed")}))
        with self.assertRaises(pbs_api.ApiError) as raised:
            pbs_api.inspect(client, "pbs", "BTV", "btv@pbs!pve")
        self.assertEqual(raised.exception.exit_code, 5)


class CommandLineTest(unittest.TestCase):
    def run_main(self, argv: list[str], **kwargs: object) -> tuple[int, str, str]:
        out, err = io.StringIO(), io.StringIO()
        with contextlib.redirect_stdout(out), contextlib.redirect_stderr(err):
            status = pbs_api.main(argv, **kwargs)
        return status, out.getvalue(), err.getvalue()

    def test_storage_mode_reads_the_token_file_and_settings(self) -> None:
        created: list[tuple[object, ...]] = []

        def factory(*arguments: object) -> FakeClient:
            created.append(arguments)
            return FakeClient(answers())

        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            (root / "storage.cfg").write_text(STORAGE_CFG.format(fingerprint=FINGERPRINT), encoding="utf-8")
            (root / "priv").mkdir()
            (root / "priv" / "bmac-pbs.pw").write_text(TOKEN + "\n", encoding="utf-8")
            status, out, err = self.run_main(
                ["inspect", "--storage", "bmac-pbs"], client_factory=factory,
                storage_cfg=root / "storage.cfg", storage_priv=root / "priv",
            )
        self.assertEqual(status, 0, msg=err)
        self.assertEqual(created, [("203.0.113.7", 8007, "btv@pbs!pve", TOKEN, FINGERPRINT)])
        self.assertEqual(json.loads(out)["namespace"], "BTV")
        self.assertNotIn(TOKEN, out + err)

    def test_groups_lists_each_group_with_its_newest_backup(self) -> None:
        rows = [{"backup-type": "vm", "backup-id": "101", "last-backup": 1793200000, "backup-count": 2},
                {"backup-type": "vm", "backup-id": "100", "last-backup": 1793210000, "backup-count": 7}]
        client = FakeClient({"/admin/datastore/pbs/groups": (200, rows)})
        status, out, err = self.run_main(
            ["groups", "--server", "h", "--datastore", "pbs", "--namespace", "BTV", "--username", "u@pbs!t",
             "--fingerprint", FINGERPRINT, "--token-stdin"],
            client_factory=lambda *_: client, stdin=io.StringIO(TOKEN + "\n"))
        self.assertEqual(status, 0, msg=err)
        self.assertEqual(client.paths, ["/admin/datastore/pbs/groups?ns=BTV"])
        self.assertEqual(json.loads(out), {"groups": [
            {"group": "vm/100", "last_backup": 1793210000, "count": 7},
            {"group": "vm/101", "last_backup": 1793200000, "count": 2}]})
        client = FakeClient({"/admin/datastore/pbs/groups": (403, "permission check failed")})
        status, _, err = self.run_main(
            ["groups", "--server", "h", "--datastore", "pbs", "--namespace", "BTV", "--username", "u@pbs!t",
             "--fingerprint", FINGERPRINT, "--token-stdin"],
            client_factory=lambda *_: client, stdin=io.StringIO(TOKEN + "\n"))
        self.assertEqual(status, 5)
        self.assertIn("HTTP 403", err)

    def test_option_mode_reads_the_token_from_stdin(self) -> None:
        created: list[tuple[object, ...]] = []

        def factory(*arguments: object) -> FakeClient:
            created.append(arguments)
            return FakeClient(answers())

        status, out, err = self.run_main(
            ["inspect", "--server", "pbs.example.com", "--port", "18007", "--datastore", "pbs",
             "--namespace", "BTV", "--username", "btv@pbs!pve", "--fingerprint", FINGERPRINT,
             "--token-stdin"],
            client_factory=factory, stdin=io.StringIO(TOKEN + "\n"),
        )
        self.assertEqual(status, 0, msg=err)
        self.assertEqual(created[0][:4], ("pbs.example.com", 18007, "btv@pbs!pve", TOKEN))
        self.assertNotIn(TOKEN, out + err)

    def test_empty_token_and_missing_storage_are_configuration_errors(self) -> None:
        status, _, err = self.run_main(
            ["inspect", "--server", "h", "--datastore", "pbs", "--username", "u@pbs!t",
             "--fingerprint", FINGERPRINT, "--token-stdin"],
            client_factory=lambda *a: FakeClient(answers()), stdin=io.StringIO("\n"),
        )
        self.assertEqual(status, 2)
        self.assertIn("empty", err)
        with tempfile.TemporaryDirectory() as temporary:
            config = Path(temporary) / "storage.cfg"
            config.write_text("dir: local\n\tpath /var/lib/vz\n", encoding="utf-8")
            status, _, err = self.run_main(["inspect", "--storage", "bmac-pbs"], storage_cfg=config)
        self.assertEqual(status, 2)
        self.assertIn("not a PBS storage", err)


@unittest.skipUnless(shutil.which("openssl"), "openssl is needed to make a test certificate")
class PinnedConnectionTest(unittest.TestCase):
    """A real TLS server: the pin decides whether the token is ever sent."""

    def setUp(self) -> None:
        self.temporary = tempfile.TemporaryDirectory()
        root = Path(self.temporary.name)
        self.cert, key = root / "cert.pem", root / "key.pem"
        subprocess.run(
            ["openssl", "req", "-x509", "-newkey", "rsa:2048", "-nodes", "-days", "1",
             "-subj", "/CN=pbs-test", "-keyout", str(key), "-out", str(self.cert)],
            check=True, capture_output=True, stdin=subprocess.DEVNULL,
        )
        der = ssl.PEM_cert_to_DER_cert(self.cert.read_text(encoding="utf-8"))
        digest = hashlib.sha256(der).hexdigest()
        self.fingerprint = ":".join(digest[i:i + 2] for i in range(0, 64, 2)).upper()
        self.seen: list[str | None] = []
        seen = self.seen

        class Handler(http.server.BaseHTTPRequestHandler):
            def do_GET(self) -> None:  # noqa: N802 (http.server API)
                seen.append(self.headers.get("Authorization"))
                body = json.dumps({"data": {"version": "4.2", "path": self.path}}).encode()
                self.send_response(200)
                self.send_header("Content-Length", str(len(body)))
                self.end_headers()
                self.wfile.write(body)

            def log_message(self, *args: object) -> None:
                pass

        self.server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        context = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
        context.load_cert_chain(self.cert, key)
        self.server.socket = context.wrap_socket(self.server.socket, server_side=True)
        self.thread = threading.Thread(target=self.server.serve_forever, daemon=True)
        self.thread.start()
        self.port = self.server.server_address[1]

    def tearDown(self) -> None:
        self.server.shutdown()
        self.server.server_close()
        self.thread.join(timeout=5)
        self.temporary.cleanup()

    def test_matching_fingerprint_sends_the_token(self) -> None:
        client = pbs_api.Client("127.0.0.1", self.port, "btv@pbs!pve", TOKEN, self.fingerprint)
        status, data = client.get("/version")
        self.assertEqual(status, 200)
        self.assertEqual(data["path"], "/api2/json/version")
        self.assertEqual(self.seen, [f"PBSAPIToken=btv@pbs!pve:{TOKEN}"])

    def test_other_fingerprint_sends_nothing(self) -> None:
        client = pbs_api.Client("127.0.0.1", self.port, "btv@pbs!pve", TOKEN, FINGERPRINT)
        with self.assertRaises(pbs_api.ApiError) as raised:
            client.get("/version")
        self.assertEqual(raised.exception.exit_code, 3)
        self.assertIn("nothing was sent", str(raised.exception))
        self.assertEqual(self.seen, [])

    def test_unreachable_server_is_exit_code_4(self) -> None:
        self.server.shutdown()
        self.server.server_close()
        client = pbs_api.Client("127.0.0.1", self.port, "btv@pbs!pve", TOKEN, self.fingerprint)
        with self.assertRaises(pbs_api.ApiError) as raised:
            client.get("/version")
        self.assertEqual(raised.exception.exit_code, 4)


if __name__ == "__main__":
    unittest.main()
