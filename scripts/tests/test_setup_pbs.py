# Copyright (c) 2026 BEENTHERE VENTURES, INC.
# SPDX-License-Identifier: GPL-3.0-only

"""Tests for scripts/user_callable/backups/setup_pbs.sh with a fake cluster host."""

from __future__ import annotations

import base64
import hashlib
import json
import os
from pathlib import Path
import shlex
import shutil
import stat
import subprocess
import tempfile
import textwrap
import unittest


REPO_ROOT = Path(__file__).resolve().parents[2]
SCRIPT = REPO_ROOT / "scripts" / "user_callable" / "backups" / "setup_pbs.sh"
TOKEN = "0d6a2f4e-9b1c-4d3e-8f7a-1234567890ab"
PBS_FINGERPRINT = ":".join(["5d"] * 32)
KEY_FINGERPRINT = "aa:bb:cc:dd:ee:ff:00:11:22:33:44:55:66:77:88:99:aa:bb:cc:dd:ee:ff:00:11:22:33:44:55:66:77:88:99"
OTHER_FINGERPRINT = "11:" * 31 + "11"

CLUSTER_CONF = f"""PROXMOX_CLUSTER_NAME=mycluster
MAX_MOX_HOSTS=10
PROXMOX_INTERNAL_DOMAIN=pve.internal
PRIVATE_SUBNET_CIDR=10.213.0.0/24
PROXMOX_MIGRATION_NETWORK=10.213.0.240/28
DATACENTER_PRIVATE_VLAN_GATEWAY=10.213.0.1
GUEST_GATEWAY_IP=10.213.0.10
GUEST_GATEWAY_MAC=02:00:0a:d5:00:0a
MOX_IP_START=10.213.0.11
MOX_IP_END=10.213.0.20
MOX_REPLICATION_IP_START=10.213.0.241
MOX_REPLICATION_IP_END=10.213.0.250
VM_SLOT_START=10.213.0.110
MAX_PROD_VMS=10
MAX_STAGING_VMS_PER_PROD=9
CLOUDFLARE_ACCOUNT_ID=0123456789abcdef0123456789abcdef
CLOUDFLARE_TUNNEL_ID=00000000-0000-0000-0000-000000000000
PRODUCTION_VM_TAG=app-ha-production
STAGING_VM_TAG=app-ha-staging
EVICTABLE_VM_TAG=evictable
PROXMOX_PBS_SERVER_HOST=203.0.113.7
PROXMOX_PBS_DATASTORE=pbs
PROXMOX_PBS_NAMESPACE=BTV
PROXMOX_PBS_USERNAME=btv@pbs!pve
PROXMOX_PBS_FINGERPRINT={PBS_FINGERPRINT}
"""

# Stands in for `mox_ssh mox1 ARGV...`: answers each remote command from the
# state files in FAKE and records what the script sent. It always drains
# stdin, as a real ssh session would.
FAKE_HOST = r'''#!/usr/bin/env python3
import base64, hashlib, json, os, sys
from pathlib import Path

fake = Path(os.environ["FAKE"])
argv = sys.argv[1:]
stdin = sys.stdin.read()
with open(fake / "calls.jsonl", "a") as log:
    log.write(json.dumps({"argv": argv, "stdin": stdin}) + "\n")

def state(name, default=""):
    path = fake / name
    return path.read_text() if path.exists() else default

key_path = "/etc/pve/priv/storage/bmac-pbs.enc"
master_path = "/etc/pve/priv/storage/bmac-pbs.master.pem"
if argv[:2] == ["test", "-x"]:
    sys.exit(0)
if argv[:2] == ["test", "-f"]:
    sys.exit(0 if (fake / "stored-key.json").exists() else 1)
if argv[:3] == ["pvesh", "get", "/storage"]:
    print(state("storages.json", "[]"))
    sys.exit(0)
if argv[:3] == ["python3", "-I", "/usr/local/lib/app-ha-proxmox/lib/pbs_api.py"]:
    (fake / "token-seen").write_text(stdin)
    code = int(state("inspect.rc", "0"))
    if code:
        print("pbs_api.py: simulated failure", file=sys.stderr)
        sys.exit(code)
    print(state("inspect.json"))
    sys.exit(0)
if argv[:2] == ["bash", "-c"]:
    script, arguments = argv[2], argv[4:]
    if "key show" in script:
        print(json.dumps({"fingerprint": json.loads(stdin)["fingerprint"], "kdf": "none"}))
        sys.exit(0)
    if "pvesm add" in script:
        lines = stdin.split("\n")
        token, master = lines[0], base64.b64decode(lines[1])
        key = base64.b64decode(lines[2]).decode() if len(lines) > 2 and lines[2] else ""
        (fake / "added.json").write_text(json.dumps({"arguments": arguments, "token": token, "key": key,
                                                     "script": script}))
        stored = key or json.dumps({"kdf": None, "data": "bmV3", "fingerprint": state("generated-fp")})
        (fake / "stored-key.json").write_text(stored)
        # PVE need not store the PEM byte for byte; a test can make it differ.
        stored_master = master + state("master-suffix").encode()
        if (fake / "master-other").exists():
            stored_master = (fake / "master-other").read_bytes()
        (fake / "stored-master.pem").write_bytes(stored_master)
        sys.exit(0)
if argv[:3] == ["pvesm", "status", "--storage"]:
    if (fake / "added.json").exists() and not (fake / "inactive").exists():
        print("Name Type Status Total Used Available %")
        print("bmac-pbs pbs active 100 0 100 0.00%")
    sys.exit(0)
if argv[:2] == ["pvesm", "remove"]:
    (fake / "removed").write_text(argv[2])
    sys.exit(0)
if argv[:3] == ["proxmox-backup-client", "key", "show"] and argv[3] == key_path:
    stored = json.loads(state("stored-key.json"))
    print(json.dumps({"fingerprint": stored["fingerprint"], "kdf": "none"}))
    sys.exit(0)
if argv[:3] == ["proxmox-backup-client", "key", "paperkey"]:
    print("-----BEGIN PROXMOX BACKUP KEY-----\nfake paper key")
    sys.exit(0)
if argv == ["cat", key_path]:
    sys.stdout.write(state("stored-key.json"))
    sys.exit(0)
if argv[0] == "sha256sum" and argv[1] == key_path:
    print(hashlib.sha256(state("stored-key.json").encode()).hexdigest() + "  " + key_path)
    sys.exit(0)
if argv == ["cat", master_path]:
    sys.stdout.write((fake / "stored-master.pem").read_text())
    sys.exit(0)
print(f"fake host: unexpected command {argv}", file=sys.stderr)
sys.exit(97)
'''


def inspect_report(*, snapshots: int = 0, fingerprints: list[str] | None = None,
                   missing: list[str] | None = None, excess: list[dict[str, object]] | None = None) -> dict[str, object]:
    return {
        "server": "203.0.113.7:8007", "datastore": "pbs", "namespace": "BTV",
        "username": "btv@pbs!pve", "version": "4.2",
        "privileges": {"path": "/datastore/pbs/BTV", "missing": missing or [], "excess": excess or []},
        "prune": {"readable": True, "jobs": [{"id": "prune-btv", "keep-last": 6, "keep-daily": 3,
                                              "keep-weekly": 4, "ns": "BTV", "store": "pbs", "schedule": "05:00"}]},
        "snapshots": {"readable": True, "count": snapshots, "groups": ["vm/100"] if snapshots else [],
                      "key_fingerprints": fingerprints or [], "unencrypted": 0},
    }


@unittest.skipUnless(shutil.which("openssl"), "openssl is needed to make a master key pair")
class SetupPbsTest(unittest.TestCase):
    def setUp(self) -> None:
        self.temporary = tempfile.TemporaryDirectory()
        root = Path(self.temporary.name)
        self.config = root / "config"
        self.config.mkdir()
        (self.config / "cluster.conf").write_text(CLUSTER_CONF, encoding="utf-8")
        (self.config / "cluster.conf").chmod(0o644)
        (self.config / "secrets.env").write_text(f"PROXMOX_PBS_TOKEN={TOKEN}\n", encoding="utf-8")
        (self.config / "secrets.env").chmod(0o600)
        private = root / "master-private.pem"
        subprocess.run(["openssl", "genpkey", "-algorithm", "RSA", "-pkeyopt", "rsa_keygen_bits:2048",
                        "-out", str(private)], check=True, capture_output=True, stdin=subprocess.DEVNULL)
        self.private = private
        self.master = self.config / "pbs-master-public.pem"
        subprocess.run(["openssl", "pkey", "-in", str(private), "-pubout", "-out", str(self.master)],
                       check=True, capture_output=True, stdin=subprocess.DEVNULL)
        self.fake = root / "fake"
        self.fake.mkdir()
        self.host = self.fake / "host.py"
        self.host.write_text(FAKE_HOST, encoding="utf-8")
        self.host.chmod(0o755)
        (self.fake / "generated-fp").write_text(KEY_FINGERPRINT)
        self.export = root / "export"
        self.export.mkdir()
        self.set_inspect(inspect_report())

    def tearDown(self) -> None:
        self.temporary.cleanup()

    def set_inspect(self, report: dict[str, object]) -> None:
        (self.fake / "inspect.json").write_text(json.dumps(report))

    def run_setup(self, *arguments: str, expected: int = 0, stdin: str | None = None) -> subprocess.CompletedProcess[str]:
        body = f"""
        SETUP_PBS_SOURCE_ONLY=1 source {shlex.quote(str(SCRIPT))}
        first_reachable_mox() {{ printf 'mox1\\n'; }}
        mox_ssh() {{ shift; {shlex.quote(str(self.host))} "$@"; }}
        main {" ".join(shlex.quote(a) for a in arguments)}
        """
        environment = os.environ.copy()
        environment.update({
            "APP_HA_CONFIG_TEST_MODE": "1", "APP_HA_CONFIG_DIR": str(self.config),
            "FAKE": str(self.fake), "HOME": str(self.export),
        })
        completed = subprocess.run(["bash", "-c", textwrap.dedent(body)], env=environment,
                                   capture_output=True, text=True, check=False,
                                   **({"input": stdin} if stdin is not None else {"stdin": subprocess.DEVNULL}))
        self.assertEqual(completed.returncode, expected,
                         msg=f"stdout:\n{completed.stdout}\nstderr:\n{completed.stderr}")
        return completed

    def calls(self) -> list[dict[str, object]]:
        path = self.fake / "calls.jsonl"
        if not path.exists():
            return []
        return [json.loads(line) for line in path.read_text().splitlines()]

    def assert_secrets_only_on_stdin(self, key_text: str = "") -> None:
        for call in self.calls():
            argv = json.dumps(call["argv"])
            self.assertNotIn(TOKEN, argv)
            if key_text:
                self.assertNotIn(json.loads(key_text)["data"], argv)

    def added(self) -> dict[str, object] | None:
        path = self.fake / "added.json"
        return json.loads(path.read_text()) if path.exists() else None

    def test_empty_namespace_generates_a_key_and_saves_a_named_copy(self) -> None:
        completed = self.run_setup("--yes", "--no-paper-key")
        added = self.added()
        self.assertIsNotNone(added)
        assert added is not None
        self.assertEqual(added["token"], TOKEN)
        self.assertEqual(added["key"], "")
        self.assertEqual(added["arguments"], [
            "bmac-pbs", "203.0.113.7", "8007", "pbs", "BTV", "btv@pbs!pve", PBS_FINGERPRINT])
        self.assertEqual((self.fake / "stored-master.pem").read_bytes(), self.master.read_bytes())
        # pvesm reads both keys from files (its CLI takes paths, not contents).
        self.assertIn('--encryption-key "$encryption" --master-pubkey "$dir/master-public.pem"', added["script"])
        self.assertIn("encryption=autogen", added["script"])
        self.assertIn('shred -u', added["script"])
        self.assertEqual((self.fake / "token-seen").read_text(), TOKEN + "\n")
        self.assert_secrets_only_on_stdin()
        copy = self.export / "mycluster-BTV-pbs-key-aabbccdd.enc"
        self.assertEqual(copy.read_text(), (self.fake / "stored-key.json").read_text())
        self.assertEqual(stat.S_IMODE(copy.stat().st_mode), 0o600)
        self.assertIn("Store " + str(copy) + " offline", completed.stdout)
        self.assertNotIn(TOKEN, completed.stdout + completed.stderr)

    def test_the_stored_master_key_may_differ_in_whitespace(self) -> None:
        (self.fake / "master-suffix").write_text("\n\n")
        self.run_setup("--yes", "--no-paper-key")
        self.assertFalse((self.fake / "removed").exists())

    def test_a_different_stored_master_key_removes_the_storage(self) -> None:
        other = self.export / "other-public.pem"
        subprocess.run(["openssl", "genpkey", "-algorithm", "RSA", "-pkeyopt", "rsa_keygen_bits:2048",
                        "-out", str(self.export / "other.pem")], check=True, capture_output=True,
                       stdin=subprocess.DEVNULL)
        subprocess.run(["openssl", "pkey", "-in", str(self.export / "other.pem"), "-pubout", "-out", str(other)],
                       check=True, capture_output=True, stdin=subprocess.DEVNULL)
        (self.fake / "master-other").write_bytes(other.read_bytes())
        completed = self.run_setup("--yes", "--no-paper-key", "--export-dir", str(self.fake), expected=1)
        self.assertIn("is not the key in", completed.stderr)
        self.assertIn("the stored file: ", completed.stderr)
        self.assertIn("starting '-----BEGIN PUBLIC KEY-----'", completed.stderr)
        self.assertEqual((self.fake / "removed").read_text(), "bmac-pbs")

    def test_check_only_changes_nothing(self) -> None:
        completed = self.run_setup("--check")
        self.assertIsNone(self.added())
        self.assertIn("Retention (prune-btv, runs 05:00): keep-daily 3, keep-last 6, keep-weekly 4", completed.stdout)
        self.assertEqual(list(self.export.iterdir()), [])

    def test_a_namespace_with_backups_refuses_to_generate_a_key(self) -> None:
        self.set_inspect(inspect_report(snapshots=4, fingerprints=[KEY_FINGERPRINT]))
        completed = self.run_setup("--yes", expected=1)
        self.assertIn("give their key with --key-file", completed.stderr)
        self.assertIsNone(self.added())

    def test_the_existing_key_is_used_when_its_fingerprint_matches(self) -> None:
        self.set_inspect(inspect_report(snapshots=4, fingerprints=[KEY_FINGERPRINT]))
        key = self.export / "saved.enc"
        key_text = json.dumps({"kdf": None, "data": "c2VjcmV0LWtleQ==", "fingerprint": KEY_FINGERPRINT})
        key.write_text(key_text)
        self.run_setup("--yes", "--no-paper-key", "--key-file", str(key))
        added = self.added()
        assert added is not None
        self.assertEqual(added["key"], key_text)
        self.assert_secrets_only_on_stdin(key_text)
        for call in self.calls():
            self.assertNotIn(base64.b64encode(key_text.encode()).decode(), json.dumps(call["argv"]))
        self.assertTrue((self.export / "mycluster-BTV-pbs-key-aabbccdd.enc").is_file())

    def test_a_key_that_encrypted_none_of_the_backups_is_refused(self) -> None:
        self.set_inspect(inspect_report(snapshots=4, fingerprints=[OTHER_FINGERPRINT]))
        key = self.export / "wrong.enc"
        key.write_text(json.dumps({"kdf": None, "data": "eA==", "fingerprint": KEY_FINGERPRINT}))
        completed = self.run_setup("--yes", "--key-file", str(key), expected=1)
        self.assertIn("did not encrypt any backup", completed.stderr)
        self.assertIsNone(self.added())

    def test_a_passphrase_protected_key_is_refused(self) -> None:
        key = self.export / "protected.enc"
        key.write_text(json.dumps({"kdf": {"Scrypt": {}}, "data": "eA=="}))
        completed = self.run_setup("--yes", "--key-file", str(key), expected=1)
        self.assertIn("passphrase-protected", completed.stderr)
        self.assertIsNone(self.added())

    def test_a_missing_privilege_stops_before_any_change(self) -> None:
        self.set_inspect(inspect_report(missing=["Datastore.Audit"]))
        completed = self.run_setup("--yes", expected=1)
        self.assertIn("The token lacks Datastore.Audit on /datastore/pbs/BTV", completed.stderr)
        self.assertIsNone(self.added())

    def test_a_privilege_that_can_delete_backups_is_warned_about(self) -> None:
        self.set_inspect(inspect_report(excess=[
            {"path": "/datastore/pbs/BTV", "privilege": "Datastore.Prune", "dangerous": True}]))
        completed = self.run_setup("--yes", "--no-paper-key")
        self.assertIn("Datastore.Prune on /datastore/pbs/BTV, which lets anyone with this cluster's token delete its backups",
                      completed.stderr)
        self.assertIsNotNone(self.added())

    def test_a_certificate_mismatch_is_explained(self) -> None:
        (self.fake / "inspect.rc").write_text("3")
        completed = self.run_setup("--yes", expected=1)
        self.assertIn("does not match PROXMOX_PBS_FINGERPRINT, so the token was not sent", completed.stderr)
        self.assertIsNone(self.added())

    def test_a_missing_default_master_key_is_asked_for(self) -> None:
        elsewhere = self.export / "master-public.pem"
        self.master.rename(elsewhere)
        completed = self.run_setup("--check", stdin=f"{elsewhere}\n")
        self.assertIn("No master public key was given", completed.stdout)
        self.assertIn(f"{elsewhere}: RSA public key", completed.stdout)

    def test_a_missing_default_master_key_stops_a_run_with_yes(self) -> None:
        self.master.unlink()
        completed = self.run_setup("--yes", expected=1)
        self.assertIn("Give the master public key with --master-pubkey", completed.stderr)
        self.assertEqual(self.calls(), [])

    def test_a_private_master_key_is_refused(self) -> None:
        completed = self.run_setup("--yes", "--master-pubkey", str(self.private), expected=1)
        self.assertIn("contains a PRIVATE key", completed.stderr)
        self.assertEqual(self.calls(), [])

    def test_an_existing_matching_storage_only_exports_the_key(self) -> None:
        (self.fake / "storages.json").write_text(json.dumps([
            {"storage": "local", "type": "dir"},
            {"storage": "bmac-pbs", "type": "pbs", "server": "203.0.113.7", "datastore": "pbs",
             "namespace": "BTV", "username": "btv@pbs!pve", "fingerprint": PBS_FINGERPRINT.upper()},
        ]))
        (self.fake / "stored-key.json").write_text(json.dumps({"kdf": None, "data": "b2xk", "fingerprint": KEY_FINGERPRINT}))
        completed = self.run_setup("--yes", "--paper-key")
        self.assertIn("already exists with these settings", completed.stdout)
        self.assertIsNone(self.added())
        self.assertTrue((self.export / "mycluster-BTV-pbs-key-aabbccdd.enc").is_file())
        self.assertIn("fake paper key", (self.export / "mycluster-BTV-pbs-key-aabbccdd.paperkey.txt").read_text())

    def test_an_existing_storage_with_other_settings_stops(self) -> None:
        (self.fake / "storages.json").write_text(json.dumps([
            {"storage": "bmac-pbs", "type": "pbs", "server": "203.0.113.7", "datastore": "pbs",
             "namespace": "OTHER", "username": "btv@pbs!pve", "fingerprint": PBS_FINGERPRINT},
        ]))
        completed = self.run_setup("--yes", expected=1)
        self.assertIn("namespace is 'OTHER', cluster.conf says 'BTV'", completed.stderr)
        self.assertIsNone(self.added())

    def test_another_storage_on_the_same_namespace_is_warned_about(self) -> None:
        (self.fake / "storages.json").write_text(json.dumps([
            {"storage": "pbs-btv", "type": "pbs", "server": "203.0.113.7", "datastore": "pbs",
             "namespace": "BTV", "username": "btv@pbs!pve", "fingerprint": PBS_FINGERPRINT},
        ]))
        completed = self.run_setup("--yes", "--no-paper-key")
        self.assertIn("PVE storage 'pbs-btv' also writes to namespace BTV", completed.stderr)

    def test_a_storage_that_does_not_activate_is_removed_again(self) -> None:
        (self.fake / "inactive").write_text("")
        completed = self.run_setup("--yes", expected=1)
        self.assertIn("was removed again because the storage is not active", completed.stderr)
        self.assertEqual((self.fake / "removed").read_text(), "bmac-pbs")
        self.assertEqual(list(self.export.iterdir()), [])

    def test_an_existing_key_copy_is_never_overwritten(self) -> None:
        target = self.export / "mycluster-BTV-pbs-key-aabbccdd.enc"
        target.write_text("keep me")
        completed = self.run_setup("--yes", "--no-paper-key", expected=1)
        self.assertIn("already exists", completed.stderr)
        self.assertEqual(target.read_text(), "keep me")


class AddStorageHostScriptTest(unittest.TestCase):
    """Runs the host side of add_storage with a fake pvesm."""

    def host_script(self, directory: Path) -> str:
        text = SCRIPT.read_text(encoding="utf-8")
        body = text[text.index("add_storage() {"):]
        start = body.index("on_coordinator bash -c '") + len("on_coordinator bash -c '")
        script = body[start:body.index("' bash \"$STORAGE_ID\"", start)]
        return script.replace("/run/", f"{directory}/")

    def run_host(self, key: bytes | None) -> tuple[dict[str, object], Path]:
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        root = Path(temporary.name)
        bin_dir = root / "bin"
        bin_dir.mkdir()
        record = root / "pvesm.json"
        # The fake pvesm reads the files it is given, as the real one does.
        (bin_dir / "pvesm").write_text(textwrap.dedent(f"""\
            #!/usr/bin/env python3
            import json, sys
            argv = sys.argv[1:]
            def value(flag):
                return argv[argv.index(flag) + 1]
            encryption = value("--encryption-key")
            json.dump({{"argv": argv,
                       "master": open(value("--master-pubkey")).read(),
                       "key": None if encryption == "autogen" else open(encryption).read()}},
                      open({str(record)!r}, "w"))
            """))
        (bin_dir / "pvesm").chmod(0o755)
        master = b"-----BEGIN PUBLIC KEY-----\nMIIB\n-----END PUBLIC KEY-----\n"
        stdin = TOKEN + "\n" + base64.b64encode(master).decode() + "\n"
        if key is not None:
            stdin += base64.b64encode(key).decode() + "\n"
        environment = os.environ.copy()
        environment["PATH"] = f"{bin_dir}:{environment['PATH']}"
        completed = subprocess.run(
            ["bash", "-c", self.host_script(root), "bash", "bmac-pbs", "203.0.113.7", "8007",
             "pbs", "BTV", "btv@pbs!pve", PBS_FINGERPRINT],
            input=stdin, env=environment, capture_output=True, text=True, check=False)
        self.assertEqual(completed.returncode, 0, msg=completed.stderr)
        result = json.loads(record.read_text())
        self.assertEqual(result["master"], master.decode())
        self.assertEqual(list(root.glob("bmac-pbs-setup.*")), [], "the temporary key files remain")
        return result, root

    def test_a_generated_key_uses_autogen_and_the_master_key_file(self) -> None:
        result, _ = self.run_host(None)
        argv = result["argv"]
        self.assertIsNone(result["key"])
        self.assertEqual(argv[:3], ["add", "pbs", "bmac-pbs"])
        self.assertEqual(argv[argv.index("--encryption-key") + 1], "autogen")
        self.assertEqual(argv[argv.index("--password") + 1], TOKEN)
        self.assertEqual(argv[argv.index("--namespace") + 1], "BTV")
        self.assertEqual(argv[argv.index("--prune-backups") + 1], "keep-all=1")

    def test_an_existing_key_is_passed_as_a_file_never_as_an_argument(self) -> None:
        key = json.dumps({"kdf": None, "data": "c2VjcmV0", "fingerprint": KEY_FINGERPRINT}).encode()
        result, root = self.run_host(key)
        self.assertEqual(result["key"], key.decode())
        path = result["argv"][result["argv"].index("--encryption-key") + 1]
        self.assertTrue(path.startswith(str(root) + "/bmac-pbs-setup."))
        self.assertNotIn("c2VjcmV0", json.dumps(result["argv"]))


if __name__ == "__main__":
    unittest.main()
