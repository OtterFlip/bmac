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
from collections.abc import Callable
from pathlib import Path

from bmaclab import tailnet
from bmaclab.bootstrap import V3, conf_files
from bmaclab.lab import BOOT_TIMEOUT, LabError
from bmaclab.qmp import QmpError
from bmaclab.scenario import load

DESCRIPTION = "the Deployer installs mox3 from discovery through the host ISO to a matching Dashboard certificate and pinned SSH, with the development bmacd package"
HOST_KEY = "TAILSCALE_LAB_AUTH_KEY_PROXMOX_HOST"
DISCOVERY_KEY = "TAILSCALE_LAB_AUTH_KEY_DISCOVERY"
CHECKPOINT = "before-m6"
# A stage is saved shut down only after this many live tries fail.
LIVE_SAVE_ATTEMPTS = 2
# The LRM waits for the CRM master to freeze its services; a master that
# took over waits out the old one's 120-second lock.
HA_STOP_TIMEOUT = 360
# Stage fields a host restored from the stage keeps into the stages saved from it.
CARRIED = ("luks_passphrase", "prod_root_password")
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

    def __init__(self, argv: list[str], log: Path, env: dict[str, str] | None = None) -> None:
        self.process = subprocess.Popen(argv, stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                                        stderr=log.open("w"), text=True, bufsize=1, env=env)
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
        # Its name in the SSH config the Deployer wrote: its slot.
        self.alias = vm.name
        # The stage the host was restored from instead of installed, if any.
        self.reused = reused
        # The stages saved from this host.
        self.saved: list[str] = []
        # Types an encrypted root's passphrase at the console during a boot.
        self.unlock: Callable[[], None] | None = None
        # Fields of the stage it came from that the host still needs.
        self.carried: dict = {}
        # Resumed from a live stage, its clock still at the save until stepped.
        self.resumed_live = False

    def save_stage(self, name: str, live: bool = False, extra: dict | None = None, settle: bool = True) -> None:
        """Saves the host as stage name for later runs to start from (see
        staged), with extra in its record. A live stage keeps running; any
        other is shut down for the save and started again. Without settle,
        a host saved while paused isn't asked to answer afterwards."""
        self.s.note(f"saving {self.vm.name} as stage {name}" + (" live" if live else ""))
        meta = {"password": self.password, "package_sha256": package_digest(), **self.carried, **(extra or {})}
        for attempt in range(LIVE_SAVE_ATTEMPTS if live else 0):
            try:
                self.lab.save_stage(self.vm, name, self.work, meta, live=True)
                break
            except (LabError, QmpError) as error:
                last = attempt + 1 == LIVE_SAVE_ATTEMPTS or not self.lab.running(self.vm)
                self.s.note(f"{self.vm.name} couldn't be saved live ({error}); "
                            + ("saving it shut down instead" if last else "trying again"))
                if last:
                    live = False
                    break
        if not live:
            with booting(self.s, self.unlock):
                self.lab.save_stage(self.vm, name, self.work, meta)
        self.saved.append(name)
        if settle:
            self.answers_after_save()

    def answers_after_save(self) -> None:
        # The guard drops SSH on the public address, so only the tailnet reaches it.
        self.s.require(f"{self.host_name} answers SSH on the tailnet after its stage was saved",
                       lambda: wait_until(lambda: self.on_host("true") == "", BOOT_TIMEOUT))

    def ssh_args(self, extra: list[str] | None = None) -> list[str]:
        return ["ssh", "-F", str(self.work / "ssh" / "config"), "-o", "BatchMode=yes",
                *(extra or []), self.alias]

    def on_host(self, command: str, timeout: float = 120) -> str:
        done = subprocess.run([*self.ssh_args(), command], capture_output=True, text=True,
                              timeout=timeout, stdin=subprocess.DEVNULL)
        if done.returncode != 0:
            raise RuntimeError(done.stderr.strip() or f"exit {done.returncode}")
        return done.stdout.strip()


def run(s) -> None:
    install(s)


def boot_serials(vm) -> list[str]:
    """The disks the boot mirror goes on: the topology's NVMe disks, however
    they are attached."""
    return [disk.serial for disk in vm.disks if disk.bus == "nvme"]


@contextlib.contextmanager
def booting(s, unlock: Callable[[], None] | None):
    """Runs unlock alongside the block, which boots the host, so the
    passphrase is typed when the boot asks for it."""
    if unlock is None:
        yield
        return
    failures: list[str] = []

    def run() -> None:
        try:
            unlock()
        except Exception as e:  # reported below
            failures.append(f"{type(e).__name__}: {e}")

    thread = threading.Thread(target=run, daemon=True)
    thread.start()
    yield
    thread.join(BOOT_TIMEOUT)
    s.require(f"the boot was unlocked ({'; '.join(failures) or 'ok'})", not failures and not thread.is_alive())


def is_boot_disk(vm, option: dict) -> bool:
    """Whether a disk choice is one of the boot disks. A SCSI disk with a
    WWN gets its by-id name, and maybe its reported serial, from the WWN."""
    text = f"{option['value']} {option['label']}"
    return any(disk.serial in text or disk.wwn[2:] in text for disk in vm.disks if disk.bus == "nvme")


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


PACKAGE_SOURCES = ("legacy/scripts", "legacy/protocol", "packaging", "crates", "webui/src")


def newer_than_package(root: Path = V3) -> Path | None:
    """The newest file the package is built from, if it changed after the
    package was built."""
    built = package().stat().st_mtime
    newest = max((p for sub in PACKAGE_SOURCES for p in (root / sub).rglob("*")
                  if p.is_file() and not {"__pycache__", "tests"} & set(p.parts) and ".test." not in p.name),
                 key=lambda p: p.stat().st_mtime, default=None)
    return newest if newest is not None and newest.stat().st_mtime > built else None


def work_dir(lab, vm) -> Path:
    return lab.paths.images.parent / ("m6" if vm.name == "mox3" else f"m6-{vm.name}")


def preflight(s) -> dict:
    lab = s.lab
    keys = tailnet.read_secrets(lab.secrets)
    for name in (HOST_KEY, DISCOVERY_KEY, tailnet.API_KEY, tailnet.ADMIN_EMAIL):
        s.require(f"lab/secrets.env has {name}", lambda: keys.get(name))
    s.require("the API key and this workstation are on the lab's Tailscale account", lambda: lab.tailscale() and True)
    s.require("the development bmacd package is built (dev/build_deb.py)", lambda: list(PACKAGES.glob("bmacd_*.deb")))
    stale = newer_than_package()
    s.require("the bmacd package is newer than its sources"
              + (f" ({stale.relative_to(V3)} changed since; run dev/build_deb.py)" if stale else ""), stale is None)
    return keys


def staged(s, stage: str, build, then, layout: dict = NVME_MIRROR, vm_name: str = "mox3",
           unlock: Callable | None = None) -> None:
    """Installs vm, runs `build(installed)` to reach the stage, saves the
    host as that stage (unless build saved it), and carries on with
    `then(installed)`, the part being worked on. Later runs skip the build
    and start from the stage. A lab with nvme_as (lab/lab-scsi.toml,
    lab/lab-sata.toml) saves it live, so it resumes in seconds; the NVMe lab shuts the host
    down to save it, and it boots when restored, with `unlock(lab, vm,
    passphrase)` typing the stage's luks_passphrase if it has one.
    BMAC_LAB_FRESH=1 builds the stage again."""
    lab = s.lab
    vm = lab.topology.vm(vm_name)
    if lab.stage(vm, stage) and os.environ.get("BMAC_LAB_FRESH") != "1":
        reuse(s, vm, stage, then, unlock)
        return
    failed = s.failed

    def build_then(installed) -> None:
        build(installed)
        s.require(f"stage {stage} passed", s.failed == failed)
        if stage not in installed.saved:
            installed.save_stage(stage, live=lab.can_save_live(vm))
        then(installed)

    s.note(f"building stage {stage}")
    install(s, layout, build_then, vm_name)


def reuse(s, vm, stage: str, then, unlock: Callable | None = None) -> None:
    """Starts vm from stage instead of installing it, with the current
    bmacd package, and carries on with `then` before putting it back."""
    with restored(s, vm, stage, unlock) as installed:
        then(installed)


def save_cluster_stage(s, hosts: list, stage: str) -> None:
    """Saves every host as stage. Saved live one after another, a host that
    ran on would send the others records it no longer has once restored,
    and they would refuse its next ones; so every host is paused before any
    is saved, and they resume together."""
    lab = s.lab
    if not all(lab.can_save_live(h.vm) for h in hosts):
        for host in hosts:
            host.save_stage(stage)
        return
    lab.pause([h.vm for h in hosts])
    try:
        for host in hosts:
            host.save_stage(stage, live=True, settle=False)
    finally:
        lab.resume([h.vm for h in hosts])
    for host in hosts:
        host.answers_after_save()


def has_stage(lab, vm_names: list[str], stage: str) -> bool:
    """Whether every VM has the stage, and BMAC_LAB_FRESH doesn't ask for it
    to be built again."""
    return os.environ.get("BMAC_LAB_FRESH") != "1" and all(lab.stage(lab.topology.vm(n), stage) for n in vm_names)


def reuse_cluster(s, vm_names: list[str], stage: str, then, unlock: Callable | None = None) -> None:
    """Starts every VM in vm_names from stage, saved together, and carries on
    with `then({name: installed})` before putting them all back."""
    with contextlib.ExitStack() as stack:
        hosts = {name: stack.enter_context(restored(s, s.lab.topology.vm(name), stage, unlock, step_clock=False))
                 for name in vm_names}
        step_clocks([h for h in hosts.values() if h.resumed_live])
        then(hosts)


def step_clocks(hosts: list) -> None:
    """Sets the clocks of hosts resumed from a live stage, still at the time
    it was saved. watchdog-mux times its clients by the wall clock, so
    stepping it fences a host whose HA manager has a service; stopped first,
    the LRM freezes its services, which needs the cluster's CRM master. So
    every host is resumed before any of them is stepped, and the CRMs stop
    only once every LRM has."""
    ha = [h for h in hosts if h.on_host("systemctl is-active --quiet pve-ha-lrm.service && echo yes || true") == "yes"]
    for host in ha:
        host.on_host("systemctl stop pve-ha-lrm.service", timeout=HA_STOP_TIMEOUT)
    for host in ha:
        host.on_host("systemctl stop pve-ha-crm.service", timeout=HA_STOP_TIMEOUT)
    for host in hosts:
        host.on_host(f"date -s @{time.time():.3f} >/dev/null")
    for host in ha:
        host.on_host("systemctl start pve-ha-crm.service pve-ha-lrm.service")


@contextlib.contextmanager
def restored(s, vm, stage: str, unlock: Callable | None = None, step_clock: bool = True):
    """vm started from stage with the current bmacd package, put back as it
    was when the block ends."""
    lab = s.lab
    preflight(s)
    work, host_name = work_dir(lab, vm), f"{lab.topology.tailnet_prefix}{vm.name}"
    lab.stop([vm])
    lab.checkpoint(vm, CHECKPOINT)
    with contextlib.ExitStack() as cleanup:
        def put_back() -> None:
            lab.restore_checkpoint(vm, CHECKPOINT)
            lab.start([vm])
        cleanup.callback(put_back)
        meta = lab.restore_stage(vm, stage, work)
        hours = (time.time() - meta["taken"]) / 3600
        s.note(f"started {vm.name} from stage {stage}, saved {hours:.1f} h ago" + (" live" if meta.get("live") else "")
               + ", instead of installing it; BMAC_LAB_FRESH=1 builds the stage again")
        installed = Installed(s, vm, work, meta["password"], host_name, reused=stage)
        installed.carried = {k: meta[k] for k in CARRIED if k in meta}
        if unlock and meta.get("luks_passphrase"):
            installed.unlock = lambda: unlock(lab, vm, meta["luks_passphrase"])
        with booting(s, None if meta.get("live") else installed.unlock):
            lab.start([vm])
        s.require(f"{host_name} answers SSH on the tailnet", lambda: wait_until(
            lambda: installed.on_host("true") == "", BOOT_TIMEOUT))
        installed.resumed_live = bool(meta.get("live"))
        if installed.resumed_live and step_clock:
            step_clocks([installed])
        if meta.get("package_sha256") != package_digest():
            s.note(f"installing the current bmacd package on {vm.name}")
            with package().open("rb") as deb:
                done = subprocess.run([*installed.ssh_args(), "cat > /tmp/bmacd.deb && dpkg -i /tmp/bmacd.deb >/dev/null "
                                       "&& rm /tmp/bmacd.deb && systemctl restart bmacd.service bmacd-web.service"],
                                      stdin=deb, capture_output=True, text=True, timeout=300)
            if done.returncode != 0:
                s.note("bmacd's journal:\n" + installed.on_host(
                    "journalctl -u bmacd.service -n 30 --no-pager -o cat || true"))
            s.require(f"the current bmacd package is installed on {vm.name} ({done.stderr.strip()[-200:]})",
                      done.returncode == 0)
        s.require("bmacd and bmacd-web are running", lambda: wait_until(lambda: installed.on_host(
            "systemctl is-active bmacd.service bmacd-web.service").split() == ["active", "active"]))
        yield installed
    s.check(f"{vm.name} is back as it was", lambda: lab.wait_ssh(vm, BOOT_TIMEOUT) or True)


def install(s, layout: dict = NVME_MIRROR, then=None, vm_name: str = "mox3",
            bundle: Path | None = None, slot: str | None = None, tailnet_prefix: str | None = None) -> None:
    """The task end to end; `layout` answers the pool layout form beyond the
    boot disks, and `then(installed)` carries on before the host is put
    back. With a join `bundle`, the host joins that cluster instead of
    starting one. A new cluster's host is `slot` (vm's name by default) on
    the tailnet as `tailnet_prefix` and the slot; a prefix under the lab's
    keeps a spare's name apart from the stage hosts'. Installing drops vm's
    stages; a spare is put back powered off."""
    lab = s.lab
    vm = lab.topology.vm(vm_name)
    slot = slot or vm.name
    prefix = tailnet_prefix or lab.topology.tailnet_prefix
    if not prefix.startswith(lab.topology.tailnet_prefix):
        raise ValueError(f"{prefix} isn't under the lab's tailnet prefix {lab.topology.tailnet_prefix}")
    spare = vm.role == "spare"
    keys = preflight(s)
    base = lab.paths.images.parent
    work = work_dir(lab, vm)
    host_name = f"{prefix}{slot}"
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
    saved: list[str] = []
    with contextlib.ExitStack() as cleanup:
        def put_back() -> None:
            stopping.set()
            if spare:
                lab.stop([vm], force=True)
            lab.restore_checkpoint(vm, CHECKPOINT)
            if not spare:
                lab.start([vm])
        cleanup.callback(put_back)
        isos: dict[str, str] = {}
        # A host saved as a stage keeps its tailnet device.
        cleanup.callback(lambda: lab.forget_tailnet(
            names={*isos.values(), *(() if saved else (host_name,))}, required=False))

        task = Task([str(driver), "--kind", "host", "--work", str(work), "--cache", str(cache),
                     "--packages", str(PACKAGES), "--inventory", str(inventory)], work / "driver.log",
                    dict(os.environ, BMAC_LAB_TAILSCALE_PREFIX=prefix))
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
                answer.update(slot=slot, MAX_PROD_VM_COUNT_ON_THIS_HOST=1, MAX_STAGING_VM_COUNT_ON_THIS_HOST=2)
            elif title == f"{slot}'s hardware":
                answer.update(source="discover")
            elif title == "Discovery settings":
                answer.update(tailscale_key=keys[DISCOVERY_KEY], method="remote_console")
            elif title == "Network ports":
                vlan = vm.nic("vlan").mac.lower()
                answer.update(private_nic=choice(form, "private_nic", lambda c: c["value"].lower() == vlan)[0])
            elif title == "Pool layout":
                answer.update(boot=choice(form, "boot", lambda c: is_boot_disk(vm, c)))
                answer.update({k: (v(form, vm) if callable(v) else v) for k, v in layout.items()})
            elif title == "Root password and Tailscale key":
                answer.update(root_password=password, root_password_again=password, tailscale_key=keys[HOST_KEY])
            elif title == f"How will you boot {slot}?":
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
                    # Another lab's prefix would reach that lab's machines.
                    if not iso["target"].startswith(lab.topology.tailnet_prefix):
                        raise TaskFailed(f"the discovery ISO joins as {iso['target']}, outside this lab's "
                                         f"{lab.topology.tailnet_prefix} names")
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
                    elif form["title"] == f"Install {slot}":
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
        s.check(f"ssh {slot} was set up", any(o == f"ssh {slot} now reaches the host." for o in outputs))

        installed = Installed(s, vm, work, password, host_name)
        installed.alias = slot
        installed.saved = saved
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
        pool = on_host("for p in $(zpool status -P rpool | awk '$1 ~ /^\\/dev\\// {print $1}'); do "
                       "lsblk -ndo SERIAL \"/dev/$(lsblk -ndo PKNAME \"$(readlink -f \"$p\")\")\"; done").split()
        s.check(f"rpool mirrors the boot disks ({' '.join(pool)})", sorted(pool) == sorted(boot_serials(vm)))
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
    if not spare:
        s.check(f"{vm.name} is back as it was", lambda: lab.wait_ssh(vm, BOOT_TIMEOUT) or True)
