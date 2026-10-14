# Copyright (c) 2026 BEENTHERE VENTURES, INC.
# SPDX-License-Identifier: GPL-3.0-only

"""M9.3's lab proof: bmac-source on the hosts and bmac-backupd on the lab's
appliance VM, end to end. It starts from the cluster with production VMs
(m8_storage.staged) and backs up the "small" VM, placed on mox1 and mox3.

- The appliance joins the tailnet as a deployed one would (for its
  administrator only: the lab's backup data never goes over Tailscale, so
  the cluster reaches it on its VLAN address), installs the
  bmac-backupd package, binds a pool by GUID, and enrolls with a bundle the
  Dashboard issued. The administrator approves it after comparing the
  certificate fingerprint the appliance printed with the Dashboard's.
- Two captures: the first is pulled in full, the second incrementally from
  the first, each with the hosts' snapshot GUIDs, published under a hold.
- `BACKUP.md` lab item 5: `bmac-source` runs as its own user with only
  `send`, `hold`, and `release` delegated, and admits no other certificate.
- The appliance's UI on 8443 presents the enrolled certificate, logs the
  administrator in with a bearer token (never a cookie), and shows the PITs.
- Revoking the appliance stops its check-ins.

The appliance and the hosts are put back as they were."""

from __future__ import annotations

import ipaddress
import json
import re
import secrets
import subprocess
import time
from pathlib import Path

from bmaclab import tailnet
from bmaclab.api import ApiError
from bmaclab.bootstrap import V3
from bmaclab.lab import BOOT_TIMEOUT
from bmaclab.scenario import load

DESCRIPTION = "bmac-source and bmac-backupd back up a production VM: enroll, full and incremental pulls, delegation"

storage = load(Path(__file__).with_name("m8_storage.py"))
m6, join = storage.m6, storage.join

CHECKPOINT = "before-m9"
POOL = "bk"
APPLIANCE_NAME = "vault"
APPLIANCE_PACKAGES = V3 / "target" / "deployer-bundle" / "appliance-packages"
# A seed of the small VM's 3 GB crosses the lab's user-mode network to the
# tailnet at a few MiB/s.
SEED_TIMEOUT = 3600
PULL_TIMEOUT = 900
REVOKE_TIMEOUT = 180
UI_PROBE = "/root/ui_probe.py"
# Runs on the appliance against its own UI on 8443. The password comes on
# stdin; it prints a summary, never the session token.
UI_PROBE_SCRIPT = r'''
import hashlib, http.client, json, socket, ssl, sys

password = sys.stdin.readline().rstrip("\n")
host, port = "127.0.0.1", 8443
context = ssl.create_default_context()
context.check_hostname = False
context.verify_mode = ssl.CERT_NONE

with socket.create_connection((host, port), timeout=15) as raw:
    with context.wrap_socket(raw) as tls:
        der = tls.getpeercert(binary_form=True)
digest = hashlib.sha256(der).hexdigest().upper()
out = {"fingerprint": ":".join(digest[i:i + 2] for i in range(0, 64, 2)), "cookies": 0}

def call(method, path, body=None, token=None, origin=None):
    connection = http.client.HTTPSConnection(host, port, context=context, timeout=30)
    headers = {}
    if body is not None:
        headers["Content-Type"] = "application/json"
    if token:
        headers["Authorization"] = "Bearer " + token
    if origin:
        headers["Origin"] = origin
    connection.request(method, path, body=None if body is None else json.dumps(body), headers=headers)
    response = connection.getresponse()
    data = response.read()
    out["cookies"] += len(response.headers.get_all("Set-Cookie") or [])
    connection.close()
    return response, data

page, html = call("GET", "/")
csp = page.getheader("Content-Security-Policy") or ""
out["page"] = page.status
out["nonce"] = b"__BMAC_STYLE_NONCE__" not in html and "'nonce-" in csp
out["no_store"] = call("GET", "/api/v1/hello")[0].getheader("Cache-Control") == "no-store"
hello = json.loads(call("GET", "/api/v1/hello")[1])
out["hello"] = {k: hello.get(k) for k in ("name", "locked", "no_password", "fingerprint")}
out["anonymous"] = call("GET", "/api/v1/status")[0].status
out["wrong_password"] = call("POST", "/api/v1/login", {"password": password + "x"})[0].status
out["other_site"] = call("POST", "/api/v1/login", {"password": password}, origin="https://elsewhere.example")[0].status
response, data = call("POST", "/api/v1/login", {"password": password}, origin=f"https://{host}:{port}")
out["login"] = response.status
token = json.loads(data).get("token") if response.status == 200 else None
if token:
    status = json.loads(call("GET", "/api/v1/status", token=token)[1])
    out["pool_health"] = (status.get("pool") or {}).get("health")
    pits = json.loads(call("GET", "/api/v1/pits", token=token)[1])
    out["pits"] = len(pits.get("pits") or [])
    throughput = json.loads(call("GET", "/api/v1/throughput", token=token)[1])
    out["transfers"] = len(throughput.get("recent") or [])
    retention = json.loads(call("GET", "/api/v1/retention", token=token)[1])
    out["retention"] = retention.get("policy")
    out["logout"] = call("POST", "/api/v1/logout", {}, token=token)[0].status
    out["after_logout"] = call("GET", "/api/v1/status", token=token)[0].status
print(json.dumps(out))
'''


class Backup:
    def __init__(self, s, hosts) -> None:
        self.s, self.lab, self.hosts = s, s.lab, hosts
        self.appliance = self.lab.topology.vm("appliance")
        self.tailnet_name = f"{self.lab.topology.tailnet_prefix}appliance"
        self.dashboard = join.Dashboard(hosts["mox3"])
        self.vm: dict = {}
        self.appliance_uuid = ""
        self.fingerprint = ""
        self.disk = ""
        # Where the hosts and restore jobs reach the appliance, suggested at
        # enrollment: on the VLAN, never Tailscale (`lab_address`).
        self.address = self.appliance.vlan_ip
        self.local_retention: dict | None = None
        # Made up for this run and never printed.
        self.passphrase = secrets.token_urlsafe(24)

    # Plumbing

    def app(self, command: str, timeout: float = 300, input: bytes | None = None) -> str:
        done = subprocess.run(self.lab.ssh_args(self.appliance, [command]), capture_output=True,
                              input=input, stdin=None if input is not None else subprocess.DEVNULL, timeout=timeout)
        if done.returncode != 0:
            raise RuntimeError(f"appliance: {command}: {done.stderr.decode(errors='replace').strip()[-400:]}")
        return done.stdout.decode(errors="replace").strip()

    def at_console(self, command: str, answers: str, timeout: float = 600) -> str:
        """Runs a console-only command as if typed at the appliance's console,
        with the answers to its prompts on stdin."""
        done = subprocess.run(
            self.lab.ssh_args(self.appliance, [f"env -u SSH_CONNECTION -u SSH_CLIENT -u SSH_TTY {command} 2>&1"]),
            capture_output=True, input=answers.encode(), timeout=timeout)
        out = done.stdout.decode(errors="replace").strip()
        if done.returncode != 0:
            raise RuntimeError(f"appliance: {command}: exit {done.returncode}: {out[-600:]}")
        return out

    def on(self, name: str, command: str, timeout: float = 300) -> str:
        return self.hosts[name].on_host(command, timeout=timeout)

    def journal(self) -> dict:
        text = self.app("cat /var/lib/bmac-backupd/metadata/journal.json 2>/dev/null || echo '{}'")
        return json.loads(text)

    def pits(self) -> list[dict]:
        return self.journal().get("pits") or []

    def lab_address(self, address: str | None) -> str:
        """The lab's rule: an appliance's address is on the hosts' VLAN, so
        backup data never goes over Tailscale. BMAC itself accepts any."""
        vlan = self.lab.topology.networks["vlan"]
        try:
            on_vlan = address is not None and ipaddress.ip_address(address) in vlan
        except ValueError:
            on_vlan = False
        self.s.require(f"the lab gives the appliance an address on the hosts' VLAN {vlan}, never a "
                       f"Tailscale one ({address})", on_vlan)
        return str(address)

    def connections(self) -> set[str]:
        """Where the backup connections open on the appliance's 8445 come from."""
        peers = set()
        for line in self.app("ss -Htn state established '( sport = :8445 )'").splitlines():
            fields = line.split()
            if len(fields) >= 4:
                host = fields[3].rsplit(":", 1)[0].strip("[]").removeprefix("::ffff:")
                peers.add(host)
        return peers

    def over_the_vlan(self, label: str) -> None:
        """Every connection to the appliance's 8445 comes from a host's VLAN
        address: the lab's backup data never goes over Tailscale."""
        hosts = {vm.vlan_ip for vm in self.lab.topology.hosts}
        seen: set[str] = set()

        def connected() -> bool:
            seen.clear()
            seen.update(self.connections())
            return bool(seen) and seen <= hosts
        self.s.check(f"{label}: every connection to the appliance's 8445 comes from a host's VLAN address "
                     f"({sorted(seen)})", lambda: m6.wait_until(connected, 120))

    def wait(self, what: str, test, timeout: float) -> bool:
        return self.s.check(what, lambda: m6.wait_until(test, timeout))

    # Steps

    def prepare(self) -> None:
        lab, vm = self.lab, self.appliance
        deb = sorted(APPLIANCE_PACKAGES.glob("bmac-backupd_*.deb"))
        self.s.require("the bmac-backupd package is built (dev/build_deb.py)", bool(deb))
        lab.forget_tailnet(names={self.tailnet_name}, required=False)
        lab.stop([vm])
        lab.checkpoint(vm, CHECKPOINT)
        lab.start([vm])
        self.s.require("the appliance answers SSH", lambda: lab.wait_ssh(vm, BOOT_TIMEOUT) or True)
        self.s.require("the appliance installs OpenZFS and Tailscale", lambda: self.app(
            "export DEBIAN_FRONTEND=noninteractive; apt-get -qq update && apt-get -qq install -y zfsutils-linux curl >/dev/null "
            "&& (command -v tailscale >/dev/null || curl -fsSL https://tailscale.com/install.sh | sh >/dev/null 2>&1) "
            "&& zfs version | head -1 && tailscale version | head -1", timeout=1200) != "")
        key = tailnet.require(tailnet.read_secrets(lab.secrets), tailnet.AUTH_KEYS["backup-appliance"])
        self.app("install -m 0600 /dev/stdin /root/tailscale-auth-key", input=key.encode())
        self.app(f"tailscale up --auth-key=file:/root/tailscale-auth-key --hostname={self.tailnet_name}; "
                 "status=$?; rm -f /root/tailscale-auth-key; exit $status", timeout=180)
        self.s.check(f"the appliance is on the tailnet as {self.tailnet_name}", lambda: m6.wait_until(
            lambda: self.app("tailscale status --json | python3 -c 'import json,sys; print(json.load(sys.stdin)[\"BackendState\"])'")
            == "Running", 120))
        with deb[-1].open("rb") as package:
            self.app("cat > /tmp/bmac-backupd.deb && DEBIAN_FRONTEND=noninteractive apt-get -qq install -y "
                     "/tmp/bmac-backupd.deb >/dev/null && rm /tmp/bmac-backupd.deb", input=package.read(), timeout=600)
        self.s.check("the bmac-backupd package installs", lambda: self.app("bmac-backupd --version").startswith("bmac-backupd "))
        disk = next(d for d in vm.disks if d.bus == "scsi")
        name = next(row.split()[0] for row in self.app("lsblk -dno NAME,SERIAL").splitlines()
                    if row.split()[1:] == [disk.serial])
        self.disk = self.app(f"for l in /dev/disk/by-id/*; do case $l in *-part*) continue;; esac; "
                             f"[ \"$(readlink -f $l)\" = /dev/{name} ] && echo $l; done | head -1")
        self.s.require(f"the pool disk has a /dev/disk/by-id name ({self.disk})", self.disk.startswith("/dev/disk/by-id/"))
        self.s.check("create-pool refuses to run over SSH", lambda: "console" in self.app(
            f"bmac-backupd create-pool {POOL} stripe {self.disk} 2>&1 || true"))
        out = self.at_console(f"bmac-backupd create-pool {POOL} stripe {self.disk}",
                              f"{POOL}\n{self.passphrase}\n{self.passphrase}\n")
        self.s.check(f"create-pool encrypts the disk and makes {POOL}", "is encrypted and ready" in out)
        self.s.check(f"{POOL} sits on a LUKS2 mapping of the disk", lambda: self.app(
            f"cryptsetup luksDump {self.disk} | sed -n 's/^Version:[[:space:]]*//p'") == "2"
            and self.app(f"zpool list -H -o name {POOL}") == POOL)

    def find_vm(self) -> None:
        self.vm = storage.guest(self.hosts["mox1"], "small.lab.internal")
        self.s.require(f"the small VM is there ({self.vm.get('name')})", bool(self.vm.get("vmid")) and bool(self.vm.get("name")))

    def enroll(self) -> None:
        issued = self.dashboard.call("POST", "/appliances", {})
        bundle = json.loads(issued["content"])
        self.s.check("the bundle pins the cluster CA and names every host", len(bundle["hosts"]) == 3
                     and re.fullmatch(r"[0-9a-f]{64}", bundle["proxmox_ca_fingerprint"]) is not None)
        self.app("install -m 0600 /dev/stdin /root/enroll.json", input=issued["content"].encode())
        out = self.app(f"bmac-backupd init {APPLIANCE_NAME} {POOL}")
        printed = re.search(r"certificate fingerprint: ([0-9A-F:]{95})", out)
        self.s.require("init prints the appliance's certificate fingerprint", printed is not None)
        fingerprint = self.fingerprint = printed.group(1)
        self.s.check("init binds the pool by GUID", lambda: self.app(
            "python3 -c 'import json; print(json.load(open(\"/var/lib/bmac-backupd/pool.json\"))[\"pool_guid\"])'")
            == self.app(f"zpool get -Hp -o value guid {POOL}"))
        self.s.check("the metadata dataset is mounted in the state directory", lambda: self.app(
            f"zfs get -H -o value mounted,mountpoint {POOL}/bmac/metadata").split() == ["yes", "/var/lib/bmac-backupd/metadata"])
        address = self.lab_address(self.address)
        out = self.app(f"bmac-backupd enroll /root/enroll.json --address {address}; status=$?; "
                       "rm -f /root/enroll.json; exit $status",
                       timeout=180)
        self.s.check("enroll names the same fingerprint", fingerprint in out)

        listed = self.dashboard.call("GET", "/appliances")
        requests = [r for r in listed["requests"] if r.get("name") == APPLIANCE_NAME]
        self.s.require("the Dashboard lists the enrollment request", len(requests) == 1)
        request = requests[0]
        self.s.check("the Dashboard shows the fingerprint the appliance printed", request["fingerprint"] == fingerprint)
        wrong = "00:" * 31 + "00"
        try:
            self.dashboard.call("POST", f"/appliances/requests/{request['request_id']}/approve", {"fingerprint": wrong})
            refused = False
        except ApiError:
            refused = True
        self.s.check("approval with another fingerprint is refused", refused)
        approved = self.dashboard.call("POST", f"/appliances/requests/{request['request_id']}/approve",
                                       {"fingerprint": fingerprint})
        self.appliance_uuid = approved["appliance_uuid"]
        self.s.check(f"the appliance is approved with the address it suggested ({approved.get('address')})", any(
            a["appliance_uuid"] == self.appliance_uuid and a["enrollment"] == "approved"
            and a.get("address") == address for a in self.dashboard.call("GET", "/appliances")["appliances"]))
        self.app("systemctl start bmac-backupd.service")
        self.wait("the appliance checks in", lambda: any(
            a["appliance_uuid"] == self.appliance_uuid and a.get("last_checkin_ms")
            for a in self.dashboard.call("GET", "/appliances")["appliances"]), 180)

    def policy(self) -> None:
        body = {"vm": self.vm["name"], "appliances": [self.appliance_uuid], "enabled": True}
        if self.local_retention:
            body["local_retention"] = self.local_retention
        saved = self.dashboard.call("POST", "/backups/policies", body)
        self.s.check(f"{self.vm['name']} has a backup policy for {APPLIANCE_NAME}", saved.get("appliances") == [self.appliance_uuid])

    def capture(self, label: str) -> None:
        summary, _ = join.follow(self.s, self.dashboard, self.dashboard.start("backup.capture", {"vm": self.vm["name"]}),
                                 lambda form: {"kind": "confirm", "confirmed": True})
        self.s.check(f"{label}: Back up now completes ({summary.get('state')}: {summary.get('message') or ''})",
                     summary.get("state") == "COMPLETED")

    def volumes(self) -> list[str]:
        return [v for v in self.on("mox1", "zfs list -H -o name -t volume").splitlines()
                if v.rsplit("/", 1)[-1].startswith(f"vm-{self.vm['vmid']}-disk-")]

    def diagnose(self) -> None:
        """What a stuck pull leaves behind, for the log."""
        for title, command in (
            ("the appliance's journal", "cat /var/lib/bmac-backupd/metadata/journal.json"),
            ("zfs processes on the appliance", "ps -eo pid,etime,args | grep '[z]fs ' || true"),
            ("the appliance's service log", "journalctl -u bmac-backupd.service --no-pager -o cat | tail -30"),
        ):
            try:
                self.s.note(f"{title}:\n{self.app(command)}")
            except Exception as e:  # diagnostics only
                self.s.note(f"{title}: {e}")
        for name in ("mox1", "mox3"):
            self.s.note(f"bmac-source on {name}:\n" + self.on(
                name, "journalctl -u bmac-source.service -u bmacd.service --no-pager -o cat --since '-20 min' "
                      "| grep -i -e backup -e source | tail -20 || true"))

    def pulled(self, label: str, count: int, timeout: float = PULL_TIMEOUT) -> dict:
        if not self.wait(f"{label}: the appliance publishes the PIT", lambda: len(self.pits()) >= count, timeout):
            self.diagnose()
        pits = self.pits()
        if len(pits) < count:
            return {}
        pit = sorted(pits, key=lambda p: p["published_at_ms"])[count - 1]
        snapshot = pit["name"]
        for volume in pit["volumes"]:
            name = f"{volume['dataset']}@{snapshot}"
            self.s.check(f"{label}: {name} has the hosts' GUID", lambda: self.app(
                f"zfs get -Hp -o value guid {name}") == str(volume["guid"]))
            self.s.check(f"{label}: {name} is held", lambda: "bmac-retain" in self.app(f"zfs holds -H {name}"))
            self.s.check(f"{label}: {volume['dataset']} has no device node (volmode=none)", lambda: self.app(
                f"zfs get -H -o value volmode {volume['dataset']}") == "none")
        # The hosts may have released the snapshot already; its bookmark stays.
        on_host = lambda v: self.on("mox1", f"zfs get -Hp -o value guid {v}@{snapshot} 2>/dev/null "
                                            f"|| zfs get -Hp -o value guid {v}#{snapshot} 2>/dev/null || true")
        self.s.check(f"{label}: mox1 has the PIT with the same GUIDs", lambda: sorted(
            str(v["guid"]) for v in pit["volumes"]) == sorted(on_host(v) for v in self.volumes()))
        return pit

    def delegation(self) -> None:
        """BACKUP.md lab item 5."""
        for name in ("mox1", "mox3"):
            user = lambda unit: self.on(
                name, f"p=$(systemctl show -p MainPID --value {unit}); "
                f"if [ \"${{p:-0}}\" = 0 ]; then echo 'not running'; else stat -c %U /proc/$p || echo gone; fi").strip()
            users = (user("bmac-source.service"), user("bmacd.service"))
            self.s.check(f"item 5: bmac-source runs as its own user on {name} "
                         f"(bmac-source: {users[0]}, bmacd: {users[1]})",
                         users == ("bmac-source", "root"))
            pools = self.on(name, "awk '/^zfspool:/{z=1;next} /^[a-z]+:/{z=0} z&&$1==\"pool\"{print $2}' /etc/pve/storage.cfg | sort -u").split()
            self.s.require(f"{name} has zfspool storages ({' '.join(pools)})", bool(pools))
            for pool in pools:
                delegated = lambda: sorted(self.on(
                    name, f"zfs allow {pool} | sed -n 's/^[[:space:]]*user bmac-source //p'").replace(" ", "").split(","))
                self.s.check(f"item 5: {pool} delegates only send, hold, and release to bmac-source on {name}",
                             lambda: delegated() == ["hold", "release", "send"])
        for name in ("mox1", "mox2", "mox3"):
            self.s.check(f"item 5: bmac-source listens on nothing on {name}; it dials the appliance", lambda: self.on(
                name, "ss -Htln '( sport = :8445 )'") == "")

    def ui(self) -> None:
        """The appliance's UI on 8443: its own certificate, a login with the
        administrator's password, a bearer token and never a cookie."""
        password = secrets.token_urlsafe(18)
        self.app("bmac-backupd admin-password", input=f"{password}\n{password}\n".encode())
        self.app(f"install -m 0600 /dev/stdin {UI_PROBE}", input=UI_PROBE_SCRIPT.encode())
        try:
            seen = json.loads(self.app(f"python3 {UI_PROBE}", input=f"{password}\n".encode(), timeout=120))
        finally:
            self.app(f"rm -f {UI_PROBE}")
        self.s.check("the UI on 8443 presents the certificate the appliance enrolled with",
                     seen["fingerprint"] == self.fingerprint and seen["hello"]["fingerprint"] == self.fingerprint)
        self.s.check(f"the UI names the appliance and is unlocked with a password ({seen['hello']})",
                     seen["hello"] == {"name": APPLIANCE_NAME, "locked": False, "no_password": False,
                                       "fingerprint": self.fingerprint})
        self.s.check("the UI page carries a fresh style nonce and its CSP", seen["page"] == 200 and seen["nonce"])
        self.s.check("the UI's API answers are no-store", seen["no_store"])
        self.s.check(f"the UI refuses an anonymous request ({seen['anonymous']})", seen["anonymous"] == 401)
        self.s.check(f"the UI refuses a wrong password ({seen['wrong_password']})", seen["wrong_password"] == 401)
        self.s.check(f"the UI refuses another site's login ({seen['other_site']})", seen["other_site"] == 403)
        self.s.check(f"the administrator logs in ({seen['login']})", seen["login"] == 200)
        self.s.check(f"the UI shows the pool ({seen.get('pool_health')}) and both PITs ({seen.get('pits')})",
                     seen.get("pool_health") == "ONLINE" and seen.get("pits") == 2)
        self.s.check(f"the UI shows both transfers ({seen.get('transfers')})", seen.get("transfers") == 2)
        self.s.check(f"the UI shows the retention policy ({seen.get('retention')})", bool(seen.get("retention")))
        self.s.check("after logout the token is refused", seen.get("logout") == 200 and seen.get("after_logout") == 401)
        self.s.check(f"the UI sets no cookie ({seen['cookies']})", seen["cookies"] == 0)
        self.s.check("the UI login and logout are audited", lambda: self.app(
            "grep -c -e '\"login\"' -e '\"logout\"' /var/lib/bmac-backupd/metadata/audit.jsonl || true") == "2")

    def hello(self) -> dict:
        try:
            return json.loads(self.app("curl -sk --max-time 10 https://127.0.0.1:8443/api/v1/hello || true"))
        except ValueError:
            return {}

    def boot_id(self) -> str:
        try:
            return self.app("cat /proc/sys/kernel/random/boot_id", timeout=15)
        except (RuntimeError, subprocess.TimeoutExpired):
            return ""

    def reboot_locked(self) -> None:
        """After a reboot the pool stays locked until its passphrase is typed
        at the console; the PITs are all there once it is."""
        before = self.boot_id()
        self.app("systemd-run --on-active=2 systemctl reboot >/dev/null")
        self.s.require("the appliance reboots", lambda: m6.wait_until(
            lambda: self.boot_id() not in ("", before), BOOT_TIMEOUT))
        self.s.check(f"after the reboot {POOL} isn't imported", lambda: self.app(
            f"zpool list -H -o name {POOL} 2>/dev/null || true") == "")
        self.s.check("the UI says the pool is locked", lambda: m6.wait_until(
            lambda: self.hello().get("locked") is True, 120))
        self.s.check("the locked appliance still checks in, and the Dashboard shows it locked",
                     lambda: m6.wait_until(lambda: self.listed().get("locked") is True, 180))
        out = self.at_console("bmac-backupd unlock", f"not-{self.passphrase}\n{self.passphrase}\n")
        self.s.check("unlock refuses a wrong passphrase, then opens the pool",
                     "doesn't open the pool; try again" in out and "is unlocked" in out)
        self.s.check(f"{POOL} is imported by its GUID with the metadata mounted", lambda: self.app(
            f"zfs get -H -o value mounted {POOL}/bmac/metadata") == "yes")
        self.s.check("the UI says the pool is unlocked", lambda: m6.wait_until(
            lambda: self.hello().get("locked") is False, 180))
        self.s.check("both PITs are still published and held", lambda: len(self.pits()) == 2 and all(
            "bmac-retain" in self.app(f"zfs holds -H {v['dataset']}@{p['name']}")
            for p in self.pits() for v in p["volumes"]))
        self.wait("the Dashboard shows the appliance unlocked", lambda: self.listed().get("locked") is False, 180)

    def listed(self) -> dict:
        return next((a for a in self.dashboard.call("GET", "/appliances")["appliances"]
                     if a["appliance_uuid"] == self.appliance_uuid), {})

    def last_word(self) -> str:
        lines = [l for l in self.app("journalctl -u bmac-backupd.service --no-pager -o cat").splitlines()
                 if l.startswith("bmac-backupd: cluster ")]
        return lines[-1] if lines else ""

    def revoke(self) -> None:
        self.dashboard.call("POST", f"/appliances/{self.appliance_uuid}/revoke", {})
        self.wait("after the revoke, the appliance's check-ins are refused", lambda: self.last_word() != ""
                  and "checked in" not in self.last_word(), REVOKE_TIMEOUT)

def then(hosts) -> None:
    s = hosts["mox3"].s
    backup = Backup(s, hosts)
    try:
        backup.prepare()
        backup.find_vm()
        join.ready(s, backup.dashboard)
        backup.enroll()
        backup.policy()
        backup.capture("P1")
        first = backup.pulled("P1", 1, SEED_TIMEOUT)
        s.check("P1 is pulled in full", bool(first) and first.get("received_from") is None)
        backup.capture("P2")
        second = backup.pulled("P2", 2)
        s.check("P2 is pulled incrementally from P1", bool(second) and second.get("received_from") == first.get("pit_uuid"))
        s.check("P2 moved less than P1", bool(second) and second.get("bytes", 0) < first.get("bytes", 0))
        backup.over_the_vlan("P1 and P2")
        s.check("the appliance's audit log records both publishes", lambda: backup.app(
            "grep -c '\"publish\"' /var/lib/bmac-backupd/metadata/audit.jsonl || true") == "2")
        backup.delegation()
        backup.ui()
        backup.reboot_locked()
        backup.revoke()
    finally:
        s.note("the appliance's service log:\n" + backup.app(
            "journalctl -u bmac-backupd.service --no-pager -o cat 2>/dev/null | tail -40 || true"))
        if (s.lab.paths.vm(backup.appliance.name) / f"checkpoint-{CHECKPOINT}.fd").exists():
            s.lab.restore_checkpoint(backup.appliance, CHECKPOINT)
        s.lab.forget_tailnet(names={backup.tailnet_name}, required=False)


def run(s) -> None:
    storage.staged(s, then)
