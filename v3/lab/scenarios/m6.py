# Copyright (c) 2026 BEENTHERE VENTURES, INC.
# SPDX-License-Identifier: GPL-3.0-only

"""M6 done-when in the lab: the Deployer's "Install a Proxmox host" task,
played as an operator would, takes mox3 from a discovery session to a
Proxmox VE install from the host ISO, waits for it on the tailnet as
bmaclab-mox3, checks its Dashboard certificate against the one it made,
and sets up this workstation's SSH, pinned. The installed host runs the
development bmacd package.

The Deployer side is `examples/lab_task.rs`, which runs the task itself.
mox3 is put back as it was afterwards. The Tailscale keys come from
`lab/secrets.env`, reach the task only as answers on stdin, and are never
printed; the root password is made up for the run."""

from __future__ import annotations

import contextlib
import hashlib
import json
import os
import queue
import secrets as tokens
import shutil
import socket
import ssl
import subprocess
import threading
import time
from pathlib import Path

from bmaclab import tailnet
from bmaclab.bootstrap import V3, conf_files
from bmaclab.lab import BOOT_TIMEOUT
from bmaclab.scenario import load

DESCRIPTION = "the Deployer installs mox3 from discovery through the host ISO to a matching Dashboard certificate and pinned SSH, with the development bmacd package"
HOST_KEY = "TAILSCALE_LAB_AUTH_KEY_PROXMOX_HOST"
DISCOVERY_KEY = "TAILSCALE_LAB_AUTH_KEY_DISCOVERY"
CHECKPOINT = "before-m6"
INSTALL_TIMEOUT = 2400
TASK_TIMEOUT = 5400
PACKAGES = V3 / "target" / "deployer-bundle" / "packages"
# The lab's downloads, under the names the Deployer's cache keeps them.
CACHED = {
    "source-iso": "proxmox-ve_9.2-1.iso",
    "discovery-iso": "alpine-extended-3.24.2-x86_64.iso",
    "tailscale": "tailscale_1.104.1_amd64.tgz",
}

discovery = load(Path(__file__).with_name("m6_discovery.py"))


class TaskFailed(Exception):
    pass


class Task:
    """The driver process: task events in, answers out."""

    def __init__(self, argv: list[str], log: Path) -> None:
        self.process = subprocess.Popen(argv, stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                                        stderr=log.open("w"), text=True, bufsize=1)
        self.lines: queue.Queue[dict | None] = queue.Queue()
        threading.Thread(target=self._read, daemon=True).start()

    def _read(self) -> None:
        assert self.process.stdout is not None
        for line in self.process.stdout:
            with contextlib.suppress(json.JSONDecodeError):
                self.lines.put(json.loads(line))
        self.lines.put(None)

    def next(self, deadline: float) -> dict | None:
        try:
            return self.lines.get(timeout=max(1, deadline - time.monotonic()))
        except queue.Empty:
            raise TaskFailed("the task stopped making progress") from None

    def answer(self, request_id: str, answer: dict) -> None:
        assert self.process.stdin is not None
        self.process.stdin.write(json.dumps({"request_id": request_id, "answer": answer}) + "\n")
        self.process.stdin.flush()

    def close(self) -> None:
        if self.process.poll() is None:
            self.process.kill()
        self.process.wait()


def defaults(form: dict) -> dict:
    return {f["id"]: f["default"] for f in form.get("fields", []) if f.get("default") is not None}


def choice(form: dict, field: str, test) -> list[str]:
    spec = next(f for f in form["fields"] if f["id"] == field)
    return [c["value"] for c in spec.get("options", []) if test(c)]


def seed_cache(images: Path, cache: Path) -> None:
    for sub, name in CACHED.items():
        target = cache / sub / name
        if target.exists() or not (images / name).exists():
            continue
        target.parent.mkdir(parents=True, exist_ok=True)
        try:
            os.link(images / name, target)
        except OSError:
            shutil.copyfile(images / name, target)


def cluster_conf(lab) -> str:
    conf = conf_files(lab.topology)["cluster.conf"]
    admin_key = lab.paths.ssh_key.with_name(lab.paths.ssh_key.name + ".pub").read_text().strip()
    extra = {
        "PROXMOX_DNS_SERVER": "10.0.2.3",
        "PROXMOX_ADMIN_EMAIL": "root@lab.internal",
        "PROXMOX_COUNTRY": "us",
        "PROXMOX_KEYBOARD": "en-us",
        "ADMIN_1_PUBLIC_SSH_KEY": admin_key,
    }
    return conf + "".join(f'{key}="{value}"\n' for key, value in extra.items())


def public_ssh(lab, vm) -> bool:
    """Whether the lab's forwarded port, which reaches the public address, logs in."""
    try:
        return lab.ssh(vm, "true", timeout=20).returncode == 0
    except subprocess.TimeoutExpired:
        return False


def public_tls(vm, forward: str) -> bool:
    """Whether a TLS handshake completes through the lab's forward to the
    public address. QEMU's user network accepts the connection itself, so
    only the handshake shows whether the guest answered."""
    context = ssl.create_default_context()
    context.check_hostname = False
    context.verify_mode = ssl.CERT_NONE
    try:
        with socket.create_connection(("127.0.0.1", vm.ports[forward]), timeout=15) as raw:
            with context.wrap_socket(raw):
                return True
    except (OSError, ssl.SSLError):
        return False


NVME_MIRROR = {"data_kind": "none", "encryption": "none"}


class Installed:
    """The host the task installed, for a scenario that carries on with it."""

    def __init__(self, s, vm, work: Path, password: str, host_name: str, reused: str | None = None) -> None:
        self.s, self.lab, self.vm, self.work = s, s.lab, vm, work
        self.password, self.host_name = password, host_name
        # The stage the host was restored from instead of installed, if any.
        self.reused = reused

    def save_stage(self, name: str) -> None:
        """Saves the running host live as stage name, for later runs to
        start from (see install's stage); it resumes where it was, so an
        encrypted root needs no passphrase."""
        self.s.note(f"saving {self.vm.name} as stage {name}")
        self.lab.save_stage(self.vm, name, self.work, {"password": self.password, "package_sha256": package_digest()})

    def ssh_args(self, extra: list[str] | None = None) -> list[str]:
        return ["ssh", "-F", str(self.work / "ssh" / "config"), "-o", "BatchMode=yes",
                *(extra or []), self.vm.name]

    def on_host(self, command: str, timeout: float = 120) -> str:
        done = subprocess.run([*self.ssh_args(), command], capture_output=True, text=True,
                              timeout=timeout, stdin=subprocess.DEVNULL)
        if done.returncode != 0:
            raise RuntimeError(done.stderr.strip() or f"exit {done.returncode}")
        return done.stdout.strip()


def run(s) -> None:
    install(s)


def wait_until(test, timeout: float = 180) -> bool:
    deadline = time.monotonic() + timeout
    while True:
        try:
            if test():
                return True
        except (RuntimeError, OSError, subprocess.TimeoutExpired):
            pass
        if time.monotonic() > deadline:
            return False
        time.sleep(3)


def package() -> Path:
    return max(PACKAGES.glob("bmacd_*.deb"), key=lambda p: p.stat().st_mtime)


def package_digest() -> str:
    return hashlib.sha256(package().read_bytes()).hexdigest()


def reuse(s, vm, stage: str, work: Path, host_name: str, then) -> None:
    """Starts vm from stage instead of installing it, with the current
    bmacd package, and carries on with `then` before putting it back."""
    lab = s.lab
    lab.stop([vm])
    lab.checkpoint(vm, CHECKPOINT)
    with contextlib.ExitStack() as cleanup:
        def put_back() -> None:
            lab.restore_checkpoint(vm, CHECKPOINT)
            lab.start([vm])
        cleanup.callback(put_back)
        meta = lab.restore_stage(vm, stage, work)
        hours = (time.time() - meta["taken"]) / 3600
        s.note(f"started {vm.name} from stage {stage}, saved {hours:.1f} h ago, instead of installing it; "
               "BMAC_LAB_FRESH=1 installs it from scratch")
        lab.start([vm])
        lab.wait_ssh(vm, BOOT_TIMEOUT)
        installed = Installed(s, vm, work, meta["password"], host_name, reused=stage)
        s.require(f"{host_name} answers SSH on the tailnet", lambda: wait_until(lambda: installed.on_host("true") == ""))
        if meta.get("package_sha256") != package_digest():
            s.note(f"installing the current bmacd package on {vm.name}")
            with package().open("rb") as deb:
                done = subprocess.run([*installed.ssh_args(), "cat > /tmp/bmacd.deb && dpkg -i /tmp/bmacd.deb >/dev/null "
                                       "&& rm /tmp/bmacd.deb && systemctl restart bmacd.service bmacd-web.service"],
                                      stdin=deb, capture_output=True, text=True, timeout=300)
            s.require(f"the current bmacd package is installed on {vm.name} ({done.stderr.strip()[-200:]})",
                      done.returncode == 0)
        s.require("bmacd and bmacd-web are running", lambda: installed.on_host(
            "systemctl is-active bmacd.service bmacd-web.service").split() == ["active", "active"])
        then(installed)
    s.check(f"{vm.name} is back as it was", lambda: lab.wait_ssh(vm, BOOT_TIMEOUT) or True)


def install(s, layout: dict = NVME_MIRROR, then=None, vm_name: str = "mox3",
            bundle: Path | None = None, stage: str | None = None) -> None:
    """The task end to end; `layout` answers the pool layout form beyond the
    boot disks, and `then(installed)` carries on before the host is put
    back. With a join `bundle`, the host joins that cluster instead of
    starting one. When vm has a saved `stage` (see Installed.save_stage),
    it starts from that instead, unless BMAC_LAB_FRESH=1 is set; `then`
    sees the stage's name in installed.reused."""
    lab = s.lab
    vm = lab.topology.vm(vm_name)
    keys = tailnet.read_secrets(lab.secrets)
    for name in (HOST_KEY, DISCOVERY_KEY, tailnet.API_KEY, tailnet.ADMIN_EMAIL):
        s.require(f"lab/secrets.env has {name}", lambda: keys.get(name))
    s.require("the API key and this workstation are on the lab's Tailscale account", lambda: lab.tailscale() and True)
    s.require("the development bmacd package is built (dev/build_deb.py)", lambda: list(PACKAGES.glob("bmacd_*.deb")))

    base = lab.paths.images.parent
    work = base / ("m6" if vm.name == "mox3" else f"m6-{vm.name}")
    host_name = f"{lab.topology.tailnet_prefix}{vm.name}"
    if then is not None and stage is not None and os.environ.get("BMAC_LAB_FRESH") != "1" and lab.stage(vm, stage):
        reuse(s, vm, stage, work, host_name, then)
        return
    shutil.rmtree(work, ignore_errors=True)
    (work / "ssh").mkdir(parents=True, mode=0o700)
    cache = base / "deployer-cache"
    seed_cache(lab.paths.images, cache)
    conf = work / "cluster.conf"
    conf.write_text(cluster_conf(lab))
    # The lab key logs in to the installed host as the administrator's key would.
    (work / "ssh" / "config").write_text(f"Host *\n  IdentityFile {lab.paths.ssh_key}\n  IdentitiesOnly yes\n")

    s.note("building the lab driver and a static bmac-inventory")
    # Its own target dir: a workspace build would replace it without the lab feature.
    driver = discovery.cargo_executable(["-p", "bmac-deployer", "--features", "lab", "--example", "lab_task",
                                         "--target-dir", str(base / "driver-target")])
    static_env = dict(os.environ, RUSTFLAGS="-C target-feature=+crt-static")
    inventory = discovery.cargo_executable(
        ["--release", "-p", "bmac-inventory", "--target", discovery.STATIC_TARGET,
         "--target-dir", str(base / "discovery" / "static-target")], static_env)

    lab.stop([vm])
    # A stage's host keeps its tailnet device; installing anew removes it.
    lab.drop_stages(vm)
    lab.forget_tailnet(names={host_name}, required=False)
    lab.checkpoint(vm, CHECKPOINT)
    password = tokens.token_urlsafe(18)
    stopping = threading.Event()
    with contextlib.ExitStack() as cleanup:
        def put_back() -> None:
            stopping.set()
            lab.restore_checkpoint(vm, CHECKPOINT)
            lab.start([vm])
        cleanup.callback(put_back)
        isos: dict[str, str] = {}
        cleanup.callback(lambda: lab.forget_tailnet(
            names={*isos.values(), *(() if lab.stage_names(vm) else (host_name,))}, required=False))

        task = Task([str(driver), "--kind", "host", "--work", str(work), "--cache", str(cache),
                     "--packages", str(PACKAGES), "--inventory", str(inventory)], work / "driver.log")
        cleanup.callback(task.close)
        paths: dict[str, str] = {}
        result: dict = {}
        outputs: list[str] = []
        warnings: list[str] = []
        installing: list[threading.Thread] = []

        def install(iso: str) -> None:
            lab.wait_exit(vm, 300)
            lab.start([vm], media=Path(iso), no_reboot=True)
            deadline = time.monotonic() + INSTALL_TIMEOUT
            while lab.running(vm) and time.monotonic() < deadline and not stopping.is_set():
                time.sleep(2)
            if not lab.running(vm) and not stopping.is_set():
                # As a one-time boot leaves it: from the disk, the ISO still
                # attached for first boot to read the packages from.
                lab.start([vm])
                lab.media(vm, Path(iso))

        def values(form: dict) -> dict:
            title = form["title"]
            answer = defaults(form)
            if title == "New cluster or another host?":
                if bundle is None:
                    answer.update(origin="new", file=str(conf))
                else:
                    answer.update(origin="join", file=str(bundle))
            elif title == "Host settings" and bundle is None:
                answer.update(slot=vm.name, MAX_PROD_VM_COUNT_ON_THIS_HOST=1, MAX_STAGING_VM_COUNT_ON_THIS_HOST=2)
            elif title == f"{vm.name}'s hardware":
                answer.update(source="discover")
            elif title == "Discovery settings":
                answer.update(tailscale_key=keys[DISCOVERY_KEY], method="remote_console")
            elif title == "Network ports":
                vlan = vm.nic("vlan").mac.lower()
                answer.update(private_nic=choice(form, "private_nic", lambda c: c["value"].lower() == vlan)[0])
            elif title == "Pool layout":
                answer.update(boot=choice(form, "boot", lambda c: c["label"].startswith("nvme")))
                answer.update({k: (v(form) if callable(v) else v) for k, v in layout.items()})
            elif title == "Root password and Tailscale key":
                answer.update(root_password=password, root_password_again=password, tailscale_key=keys[HOST_KEY])
            elif title == f"How will you boot {vm.name}?":
                answer.update(method="remote_console")
            elif title != "Clear old rpool labels?":
                raise TaskFailed(f"unexpected form {title!r}")
            return answer

        deadline = time.monotonic() + TASK_TIMEOUT
        ended = None
        while ended is None:
            line = task.next(deadline)
            if line is None:
                raise TaskFailed(f"the driver exited; see {work / 'driver.log'}")
            if "iso" in line:
                iso = line["iso"]
                paths[iso["kind"]] = iso["path"]
                if iso["kind"] == "discovery":
                    isos["discovery"] = iso["target"]
            if "answer_error" in line:
                raise TaskFailed(line["answer_error"])
            if "ended" in line:
                ended = line["ended"]
            event = line.get("event")
            if not event:
                continue
            kind = event["type"]
            if kind == "output":
                outputs.append(event["text"])
                if event["text"].startswith(f"{host_name} is on the tailnet") and lab.running(vm):
                    lab.media(vm, None)
            elif kind == "warning":
                warnings.append(event["message"])
                s.note(f"warning: {event['message']}")
            elif kind in ("error", "failed"):
                s.note(f"error: {event['message']}" + (f"\n{event['details']}" if event.get("details") else ""))
            elif kind == "result":
                result = event["data"]
            elif kind == "validation_error":
                raise TaskFailed(f"an answer was refused: {event.get('message')} {event.get('field_errors')}")
            elif kind == "phase" and event["status"] == "running":
                s.note(f"phase: {event['label']}")
            elif kind == "request":
                rid, form = event["request_id"], event
                if form["kind"] == "input_group":
                    task.answer(rid, {"kind": "values", "values": values(form)})
                elif form["kind"] == "confirm":
                    task.answer(rid, {"kind": "confirm", "confirmed": True})
                elif form["kind"] == "manual_action":
                    if form["title"].startswith("Boot the machine"):
                        lab.start([vm], media=Path(paths["discovery"]))
                    elif form["title"] == f"Install {vm.name}":
                        thread = threading.Thread(target=install, args=(paths["host"],), daemon=True)
                        thread.start()
                        installing.append(thread)
                    else:
                        raise TaskFailed(f"unexpected step {form['title']!r}")
                    task.answer(rid, {"kind": "acknowledge"})
                else:
                    raise TaskFailed(f"unexpected request {form['kind']}")

        s.require(f"the task completes ({ended.get('message') or ended['state']})", ended["state"] == "COMPLETED")
        s.check("the discovery ISO was built and shredded once read",
                "discovery" in paths and not Path(paths["discovery"]).exists())
        s.check("the host ISO was built and shredded once the host was on the tailnet",
                "host" in paths and not Path(paths["host"]).exists())
        s.check(f"{host_name} joined the tailnet", any(f"{host_name} is on the tailnet" in o for o in outputs))
        s.check("the Dashboard presents the certificate the Deployer made",
                any("presents the certificate this Deployer made" in o for o in outputs))
        s.check(f"the result links the Dashboard at its tailnet name ({result.get('open_url')})",
                str(result.get("open_url", "")).startswith(f"https://{host_name}.") and result.get("fingerprint"))
        s.check("the host card is written", Path(result.get("card_file", "/nonexistent")).exists())
        s.check(f"ssh {vm.name} was set up", any(o == f"ssh {vm.name} now reaches the host." for o in outputs))

        installed = Installed(s, vm, work, password, host_name)
        on_host = installed.on_host

        version = on_host("dpkg-query -W -f='${Version}' bmacd")
        s.check(f"the development bmacd package is installed ({version})", version.startswith("3.0.0~dev"))
        s.check("bmacd and bmacd-web are running",
                lambda: on_host("systemctl is-active bmacd.service bmacd-web.service").split() == ["active", "active"])
        s.check("the bmac CLI runs", lambda: on_host("bmac --version"))
        s.check("the first-boot Tailscale enrollment finished",
                lambda: on_host("test -e /var/lib/bmac-bootstrap-complete && echo done") == "done")
        card = Path(result.get("card_file", "/nonexistent"))
        s.check("the host's SSH key is the one on the host card", lambda: on_host(
            "ssh-keygen -lf /etc/ssh/ssh_host_ed25519_key.pub").split()[1] in card.read_text())
        pool = on_host("zpool status -P rpool | awk '/nvme/ {print $1}'").split()
        s.check(f"rpool is the NVMe mirror ({len(pool)} members)", len(pool) == 2)
        s.check("SSH on the public address is dropped by the guard", lambda: not public_ssh(lab, vm))
        s.check("bmacd answers TLS on the tailnet", lambda: on_host(
            "timeout 10 openssl s_client -connect \"$(tailscale ip -4 | head -n 1)\":8443 </dev/null "
            ">/dev/null 2>&1 && echo yes") == "yes")
        for forward, port in (("web", 8443), ("peer", 8444), ("source", 8445)):
            s.check(f"port {port} doesn't answer on the public address", lambda f=forward: not public_tls(vm, f))
        for thread in installing:
            thread.join(timeout=5)
        if then is not None:
            then(installed)
    s.check(f"{vm.name} is back as it was", lambda: lab.wait_ssh(vm, BOOT_TIMEOUT) or True)
