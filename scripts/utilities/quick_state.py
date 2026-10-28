#!/usr/bin/env python3

# Copyright (c) 2026 BEENTHERE VENTURES, INC.
# SPDX-License-Identifier: GPL-3.0-only

"""Fast, read-only cluster state for the
scripts/user_callable/diagnostics/list_*.sh scripts.

`collect KIND` runs on one cluster member as root, fed over SSH as
`python3 - collect KIND ...`. It runs only read-only Proxmox API (pvesh),
pvecm, and registry queries and prints one JSON document. The one write is
the backups kind's own cache of what each backup's task log recorded, in
/var/lib/app-ha/ on that host. `render KIND` runs
on the workstation: it prints a terminal report and, in JSON mode, emits the
same state as one structured `result` event plus `next_step` suggestions.

KIND is hosts, guests, replication, storage, or backups.
"""

import argparse
import calendar
import json
import os
import re
import subprocess
import sys
import time

GIB = 1024**3

# The PVE storage Setup PBS creates, and the marker that starts the notes of
# every snapshot a BMAC backup job or Back up now writes.
BACKUP_STORAGE = "bmac-pbs"
BACKUP_NOTES_PREFIX = "bmac "
BACKUP_JOB_PREFIX = "bmac-"


# ---------------------------------------------------------------------------
# Collection (runs on a Proxmox host).


def run(argv, timeout=45):
    try:
        done = subprocess.run(
            argv,
            stdin=subprocess.DEVNULL,
            capture_output=True,
            text=True,
            timeout=timeout,
            env=dict(os.environ, LC_ALL="C"),
        )
    except FileNotFoundError:
        return {"rc": 127, "out": "", "err": f"{argv[0]} is not installed"}
    except subprocess.TimeoutExpired:
        return {"rc": 124, "out": "", "err": f"timed out after {timeout}s"}
    return {"rc": done.returncode, "out": done.stdout, "err": done.stderr.strip()}


def pvesh(path, *extra):
    result = run(["pvesh", "get", path, *extra, "--output-format", "json"])
    if result["rc"] != 0:
        return None, result["err"] or f"pvesh get {path} failed"
    try:
        return json.loads(result["out"] or "null"), None
    except ValueError as exc:
        return None, f"pvesh get {path} returned invalid JSON: {exc}"


def registry(args, *command):
    if not args.registry or not os.access(args.registry, os.X_OK):
        return None, "the BMAC cluster registry is not installed on this host"
    result = run([args.registry, "--state-dir", args.state_dir, *command])
    if result["rc"] != 0:
        return None, result["err"] or f"registry {' '.join(command)} failed"
    try:
        return json.loads(result["out"] or "null"), None
    except ValueError as exc:
        return None, f"registry {' '.join(command)} returned invalid JSON: {exc}"


def online_nodes(nodes):
    return sorted(
        (row.get("node") for row in nodes or []
         if str(row.get("status", "")).lower() == "online"),
        key=node_sort_key,
    )


def node_sort_key(name):
    match = re.fullmatch(r"mox(\d+)", str(name or ""))
    return (0, int(match.group(1))) if match else (1, str(name))


def qdevice_host_from_corosync():
    try:
        text = open("/etc/pve/corosync.conf", encoding="utf-8").read()
    except OSError:
        return {"readable": False, "registered": None, "address": None}
    registered = bool(re.search(r"^\s*device\s*\{", text, re.M))
    address = None
    if registered:
        tail = text[re.search(r"^\s*device\s*\{", text, re.M).end():]
        match = re.search(r"^\s*host:\s*(\S+)", tail, re.M)
        address = match.group(1) if match else None
    return {"readable": True, "registered": registered, "address": address}


def collect_hosts(args):
    errors = []
    nodes, err = pvesh("/nodes")
    if err:
        errors.append(err)
    status, err = pvesh("/cluster/status")
    if err:
        errors.append(err)
    slots, err = registry(args, "host-list")
    if err:
        errors.append(err)
    control, err = registry(args, "control-get")
    if err:
        errors.append(err)
    return {
        "nodes": nodes or [],
        "cluster_status": status or [],
        "pvecm_status": run(["pvecm", "status"]),
        "corosync_qdevice": qdevice_host_from_corosync(),
        "host_slots": slots,
        "control": control,
        "errors": errors,
    }


def collect_replication_status(nodes, errors):
    jobs = []
    for node in online_nodes(nodes):
        rows, err = pvesh(f"/nodes/{node}/replication")
        if err:
            errors.append(f"{node}: {err}")
            continue
        for row in rows or []:
            row = dict(row)
            row["source"] = row.get("source") or node
            jobs.append(row)
    return jobs


def collect_guests(args):
    errors = []
    resources, err = registry(args, "list")
    if err:
        errors.append(err)
    live, err = pvesh("/cluster/resources", "--type", "vm")
    if err:
        errors.append(err)
    ha_resources, err = pvesh("/cluster/ha/resources")
    if err:
        errors.append(err)
    ha_status, err = pvesh("/cluster/ha/status/current")
    if err:
        errors.append(err)
    nodes, err = pvesh("/nodes")
    if err:
        errors.append(err)
    return {
        "resources": resources or [],
        "live": live or [],
        "ha_resources": ha_resources or [],
        "ha_status": ha_status or [],
        "nodes": nodes or [],
        "replication": collect_replication_status(nodes, errors),
        "errors": errors,
    }


def collect_replication(args):
    errors = []
    nodes, err = pvesh("/nodes")
    if err:
        errors.append(err)
    resources, err = registry(args, "list")
    if err:
        errors.append(err)
    return {
        "nodes": nodes or [],
        "resources": resources or [],
        "replication": collect_replication_status(nodes, errors),
        "errors": errors,
    }


def collect_storage(args):
    errors = []
    nodes, err = pvesh("/nodes")
    if err:
        errors.append(err)
    hosts = []
    for row in sorted(nodes or [], key=lambda value: node_sort_key(value.get("node"))):
        node = row.get("node")
        entry = {"node": node, "online": str(row.get("status", "")).lower() == "online",
                 "pools": [], "storages": [], "errors": []}
        if args.host and node != args.host:
            continue
        if entry["online"]:
            pools, err = pvesh(f"/nodes/{node}/disks/zfs")
            if err:
                entry["errors"].append(err)
            entry["pools"] = pools or []
            storages, err = pvesh(f"/nodes/{node}/storage")
            if err:
                entry["errors"].append(err)
            entry["storages"] = storages or []
        hosts.append(entry)
    return {"hosts": hosts, "errors": errors}


UNITS = {"B": 1, "KiB": 2**10, "MiB": 2**20, "GiB": 2**30, "TiB": 2**40, "PiB": 2**50}
SIZE_RE = r"([0-9.]+) (B|KiB|MiB|GiB|TiB|PiB)"


def size_bytes(number, unit):
    return int(float(number) * UNITS[unit])


def parse_vzdump_log(lines):
    """Per-snapshot metrics from one vzdump task log, which may hold several
    VMs' backups: {"vm/100/2026-...Z": {...}}."""
    found = {}
    current = None
    for text in lines:
        text = re.sub(r"^INFO:\s*", "", str(text))
        if text.startswith("Starting Backup of VM "):
            current = {"bitmaps": [], "dirty_bytes": None}
            continue
        if current is None:
            continue
        match = re.match(r"creating Proxmox Backup Server archive '([^']+)'", text)
        if match:
            current["snapshot"] = match.group(1)
            continue
        match = re.match(r"(\S+): dirty-bitmap status: (.*)$", text)
        if match:
            current["bitmaps"].append({"drive": match.group(1), "status": match.group(2)})
            continue
        match = re.match(rf"using fast incremental mode \(dirty-bitmap\), {SIZE_RE} dirty of {SIZE_RE} total", text)
        if match:
            current["fast"] = True
            current["dirty_bytes"] = size_bytes(match.group(1), match.group(2))
            current["total_bytes"] = size_bytes(match.group(3), match.group(4))
            continue
        match = re.match(rf"backup is sparse: {SIZE_RE} \((\d+)%\) total zero data", text)
        if match:
            current["zero_bytes"] = size_bytes(match.group(1), match.group(2))
            continue
        match = re.match(rf"backup was done incrementally, reused {SIZE_RE} \((\d+)%\)", text)
        if match:
            current["reused_bytes"] = size_bytes(match.group(1), match.group(2))
            current["reused_percent"] = int(match.group(3))
            continue
        match = re.match(rf"transferred {SIZE_RE} in (\d+) seconds", text)
        if match:
            current["read_bytes"] = size_bytes(match.group(1), match.group(2))
            current["transfer_seconds"] = int(match.group(3))
            continue
        match = re.match(r"Finished Backup of VM \d+ \((\d+):(\d+):(\d+)\)", text)
        if match:
            hours, minutes, seconds = (int(value) for value in match.groups())
            current["duration_seconds"] = hours * 3600 + minutes * 60 + seconds
            if current.get("snapshot"):
                found[current.pop("snapshot")] = backup_metrics(current)
            current = None
    return found


def backup_metrics(raw):
    """How a backup was made, from the lines parse_vzdump_log collected."""
    statuses = [row["status"] for row in raw["bitmaps"]]
    if raw.get("fast"):
        mode, detail = "fast-incremental", "only the blocks changed since the previous backup were read (dirty bitmap)"
    elif any("invalid" in status for status in statuses):
        mode, detail = "full-read", "the whole disk was read: the dirty bitmap no longer matched the previous backup"
    elif any("created new" in status or "recreated" in status for status in statuses):
        mode, detail = "full-read", ("the whole disk was read: no dirty bitmap yet (the first backup, or after "
                                     "the VM restarted, failed over, or its disk changed)")
    else:
        mode, detail = "full-read", "the whole disk was read"
    total = raw.get("total_bytes")
    reused = raw.get("reused_bytes")
    if total is None and reused is not None and raw.get("reused_percent"):
        total = int(reused * 100 / raw["reused_percent"])
    return {
        "mode": mode,
        "detail": detail,
        "dirty_bytes": raw.get("dirty_bytes"),
        "read_bytes": raw.get("read_bytes"),
        "zero_bytes": raw.get("zero_bytes"),
        "reused_bytes": reused,
        "reused_percent": raw.get("reused_percent"),
        # Data PBS did not have yet: the image size less what it reused.
        "new_bytes": max(total - reused, 0) if total is not None and reused is not None else None,
        "transfer_seconds": raw.get("transfer_seconds"),
        "duration_seconds": raw.get("duration_seconds"),
    }


# Metrics read from a task log once are cached on the host that answered the
# listing, outside pmxcfs, so every workstation sees the same metrics and PVE
# pruning a task log loses nothing already read.
METRICS_CACHE = "/var/lib/app-ha/backup-metrics.json"


def metrics_cache_key(storage, snapshot):
    return "/".join([str(storage.get("server") or ""), str(storage.get("datastore") or ""),
                     str(storage.get("namespace") or ""), snapshot])


def parse_metrics_cache(text):
    try:
        data = json.loads(text)
    except (TypeError, ValueError):
        return {}
    entries = data.get("restore_points") if isinstance(data, dict) else None
    return entries if isinstance(entries, dict) else {}


def load_metrics_cache(path):
    try:
        return parse_metrics_cache(open(path, encoding="utf-8").read())
    except OSError:
        return {}


def save_metrics_cache(path, entries):
    """Write the cache atomically, readable only by root."""
    os.makedirs(os.path.dirname(path) or ".", mode=0o755, exist_ok=True)
    temporary = f"{path}.{os.getpid()}.tmp"
    descriptor = os.open(temporary, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o600)
    with os.fdopen(descriptor, "w", encoding="utf-8") as stream:
        json.dump({"schema_version": 1, "restore_points": entries}, stream, indent=1, sort_keys=True)
    os.replace(temporary, path)


def peer_metrics_cache(node, domain, path):
    """Another host's cache, read over the cluster's own SSH host trust."""
    known_hosts = f"/etc/pve/nodes/{node}/ssh_known_hosts"
    if not domain or not os.path.isfile(known_hosts):
        return {}
    result = run(["ssh", "-o", "BatchMode=yes", "-o", "ConnectTimeout=5", "-o", "StrictHostKeyChecking=yes",
                  "-o", "CheckHostIP=no", "-o", f"HostKeyAlias={node}", "-o", f"UserKnownHostsFile={known_hosts}",
                  "-o", "GlobalKnownHostsFile=none", f"root@{node}.{domain}", "cat", path], timeout=15)
    return parse_metrics_cache(result["out"]) if result["rc"] == 0 else {}


def collect_backup_metrics(points, storage, args, errors):
    """Metrics for every BMAC restore point: from this host's cache, then the
    other online hosts' caches, then the vzdump task logs of every online
    host, where a log is fetched only when its task's run covers a restore
    point still missing. This host's cache is then rewritten without the
    restore points PBS has pruned."""
    path = args.metrics_cache or METRICS_CACHE
    probe = os.uname().nodename.split(".")[0]
    cache = load_metrics_cache(path)
    prefix = metrics_cache_key(storage, "")
    times = {}
    for row in points:
        snapshot = str(row.get("volid", "")).split(":backup/", 1)[-1]
        if backup_notes(row.get("notes")) is not None:
            times[snapshot] = int_or_none(row.get("ctime")) or 0
    metrics = {snapshot: cache[prefix + snapshot] for snapshot in times if prefix + snapshot in cache}
    nodes = []
    if len(metrics) < len(times):
        listed, err = pvesh("/nodes")
        if err:
            errors.append(err)
        online = online_nodes(listed)
        for node in online:
            if node == probe or len(metrics) == len(times):
                continue
            peer = peer_metrics_cache(node, args.internal_domain, path)
            for snapshot in times:
                if snapshot not in metrics and prefix + snapshot in peer:
                    metrics[snapshot] = peer[prefix + snapshot]
        # This host's logs first: it most often made the backup.
        nodes = sorted(online, key=lambda node: node != probe)
    wanted = {snapshot: when for snapshot, when in times.items() if snapshot not in metrics}
    if wanted:
        metrics.update(backup_metrics_from_logs(nodes, wanted, errors))
    entries = {key: value for key, value in cache.items() if not key.startswith(prefix)}
    entries.update({prefix + snapshot: value for snapshot, value in metrics.items()})
    if entries != cache:
        try:
            save_metrics_cache(path, entries)
        except OSError as error:
            errors.append(f"could not save the backup metrics cache {path}: {error}")
    return metrics


def backup_metrics_from_logs(nodes, wanted, errors):
    """Metrics of the restore points in `wanted` (snapshot -> time) found in
    the vzdump task logs of `nodes`."""
    metrics = {}
    for node in nodes:
        tasks, err = pvesh(f"/nodes/{node}/tasks", "--typefilter", "vzdump", "--limit", "1000", "--source", "all")
        if err:
            errors.append(f"{node}: could not list backup tasks: {err}")
            continue
        for task in tasks or []:
            start = int_or_none(task.get("starttime")) or 0
            end = int_or_none(task.get("endtime")) or int(time.time())
            if not any(start - 60 <= when <= end + 60 for snapshot, when in wanted.items() if snapshot not in metrics):
                continue
            log, err = pvesh(f"/nodes/{node}/tasks/{task.get('upid')}/log", "--limit", "100000")
            if err:
                continue
            for snapshot, values in parse_vzdump_log(row.get("t", "") for row in log or []).items():
                if snapshot in wanted:
                    metrics[snapshot] = {**values, "node": node, "task": task.get("upid")}
        if len(metrics) == len(wanted):
            break
    return metrics


def collect_backups(args):
    errors = []
    probe = os.uname().nodename.split(".")[0]
    storages, err = pvesh("/storage")
    if err:
        errors.append(err)
    storage = next((row for row in storages or [] if row.get("storage") == BACKUP_STORAGE), None)
    status = content = inspect = None
    if storage:
        status, err = pvesh(f"/nodes/{probe}/storage/{BACKUP_STORAGE}/status")
        if err:
            errors.append(err)
        content, err = pvesh(f"/nodes/{probe}/storage/{BACKUP_STORAGE}/content")
        if err:
            errors.append(f"could not list the restore points: {err}")
        if args.pbs_api and os.access(args.pbs_api, os.X_OK):
            result = run(["python3", "-I", args.pbs_api, "inspect", "--storage", BACKUP_STORAGE])
            try:
                inspect = json.loads(result["out"]) if result["rc"] == 0 else {"error": result["err"]}
            except ValueError:
                inspect = {"error": "pbs_api.py returned invalid JSON"}
        else:
            inspect = {"error": "pbs_api.py is not installed on this host; run update_cluster_runtime.sh"}
    jobs, err = pvesh("/cluster/backup")
    if err:
        errors.append(err)
    resources, err = registry(args, "list")
    if err:
        errors.append(err)
    metrics = collect_backup_metrics(content or [], storage, args, errors) if storage else {}
    return {
        "metrics": metrics,
        "storage": storage,
        "storage_status": status,
        "content": content or [],
        "inspect": inspect,
        "jobs": jobs or [],
        "resources": resources or [],
        "errors": errors,
    }


COLLECTORS = {
    "hosts": collect_hosts,
    "guests": collect_guests,
    "replication": collect_replication,
    "storage": collect_storage,
    "backups": collect_backups,
}


def command_collect(args):
    report = COLLECTORS[args.kind](args)
    report["probe"] = os.uname().nodename.split(".")[0]
    report["collected_at"] = int(time.time())
    print(json.dumps(report))
    return 0


# ---------------------------------------------------------------------------
# Rendering (runs on the workstation).


def emitter():
    sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "lib"))
    import ui_protocol  # noqa: E402  (lives in scripts/lib)

    return ui_protocol


def table(headers, rows, indent="  "):
    cells = [[("" if value is None else str(value)) for value in row] for row in rows]
    widths = [
        max([len(header), *(len(row[index]) for row in cells)])
        for index, header in enumerate(headers)
    ]

    def line(values):
        return indent + "  ".join(
            value.ljust(widths[index]) for index, value in enumerate(values)
        ).rstrip()

    print(line(headers))
    for row in cells:
        print(line(row))


def heading(title):
    print()
    print("=" * 96)
    print(title)
    print("=" * 96)


def size(value):
    try:
        value = int(value)
    except (TypeError, ValueError):
        return "?"
    for unit, scale in (("TiB", 2**40), ("GiB", 2**30), ("MiB", 2**20)):
        if value >= scale:
            return f"{value / scale:.1f} {unit}"
    return f"{value} B"


def ago(epoch, now):
    try:
        delta = int(now) - int(epoch)
    except (TypeError, ValueError):
        return "never"
    if int(epoch) <= 0:
        return "never"
    if delta < 90:
        return f"{max(delta, 0)}s ago"
    if delta < 5400:
        return f"{delta // 60}m ago"
    if delta < 172800:
        return f"{delta // 3600}h ago"
    return f"{delta // 86400}d ago"


def until(epoch, now):
    if not epoch:
        return "-"
    delta = int(epoch) - int(now)
    if delta <= 0:
        return "due"
    return "in " + ago(now - delta, now).removesuffix(" ago") if delta >= 90 else f"in {delta}s"


def duration(seconds):
    try:
        seconds = int(seconds)
    except (TypeError, ValueError):
        return "?"
    days, rest = divmod(seconds, 86400)
    hours, rest = divmod(rest, 3600)
    minutes = rest // 60
    if days:
        return f"{days}d {hours}h"
    if hours:
        return f"{hours}h {minutes}m"
    return f"{minutes}m"


def status_value(text, key):
    match = re.search(rf"^{re.escape(key)}:\s*(.*?)\s*$", text or "", re.M)
    return match.group(1) if match else None


def int_or_none(value):
    try:
        return int(value)
    except (TypeError, ValueError):
        return None


def read_transfer(directory, name):
    """The host's transfer_counter.py status, as fetched over SSH, or None."""
    if not directory or not name:
        return None
    try:
        data = json.load(open(os.path.join(directory, f"transfer-{name}.json"), encoding="utf-8"))
    except (OSError, ValueError):
        return None
    if not isinstance(data, dict) or not isinstance(data.get("total_bytes"), int):
        return None
    return {
        "total_bytes": data["total_bytes"],
        "limit_gib": int_or_none(data.get("limit_gib")),
        "used_percent": data.get("used_percent"),
        "period_start": data.get("period_start"),
        "next_reset": data.get("next_reset"),
    }


def transfer_text(transfer):
    if not transfer:
        return "-"
    text = f"{transfer['total_bytes'] / GIB:.1f} GiB"
    if transfer["limit_gib"]:
        text += f" of {transfer['limit_gib']} GiB"
    return text


def render_hosts(report, args, ui):
    now = report.get("collected_at") or int(time.time())
    problems = []
    ssh = {}
    if args.ssh_file and os.path.exists(args.ssh_file):
        for line in open(args.ssh_file, encoding="utf-8"):
            node, _, state = line.strip().partition("\t")
            if node:
                ssh[node] = state == "ok"
    pvecm = report.get("pvecm_status") or {}
    pvecm_out = pvecm.get("out", "") if pvecm.get("rc") == 0 else ""
    quorate_text = (status_value(pvecm_out, "Quorate") or "").lower()
    flags = status_value(pvecm_out, "Flags") or ""
    cluster_row = next(
        (row for row in report.get("cluster_status") or [] if row.get("type") == "cluster"),
        {},
    )
    quorate = None
    if quorate_text:
        quorate = quorate_text == "yes"
    elif cluster_row:
        quorate = cluster_row.get("quorate") in (1, True, "1")
    member_rows = {
        row.get("name"): row
        for row in report.get("cluster_status") or []
        if row.get("type") == "node"
    }
    slots = {row.get("node"): row.get("state") for row in report.get("host_slots") or []}
    control_node = (report.get("control") or {}).get("control_node") if isinstance(
        report.get("control"), dict) else None
    hosts = []
    for row in sorted(report.get("nodes") or [], key=lambda value: node_sort_key(value.get("node"))):
        name = row.get("node")
        online = str(row.get("status", "")).lower() == "online"
        member = member_rows.get(name, {})
        host = {
            "name": name,
            "nodeid": int_or_none(member.get("nodeid")),
            "ip": member.get("ip"),
            "online": online,
            "uptime_seconds": int_or_none(row.get("uptime")) if online else None,
            "cpu_fraction": row.get("cpu") if online else None,
            "cores": int_or_none(row.get("maxcpu")),
            "memory_used": int_or_none(row.get("mem")) if online else None,
            "memory_total": int_or_none(row.get("maxmem")),
            "disk_used": int_or_none(row.get("disk")) if online else None,
            "disk_total": int_or_none(row.get("maxdisk")),
            "slot_state": slots.get(name),
            "is_control": name == control_node,
            "is_probe": name == report.get("probe"),
            "ssh_from_here": ssh.get(name) if ssh else None,
            "transfer": read_transfer(args.transfer_dir, name) if ssh.get(name) else None,
        }
        hosts.append(host)
        if not online:
            problems.append(f"{name} is offline in the Proxmox cluster")
        elif ssh and ssh.get(name) is False:
            problems.append(f"this workstation cannot SSH to {name}")
    count = len(hosts)
    qd = report.get("corosync_qdevice") or {}
    needed = count % 2 == 0 if count else None
    voting = None
    if qd.get("registered") and pvecm_out:
        voting = "Qdevice" in flags and bool(
            re.search(r"^0x00000000\s+1\s+Qdevice\s*$", pvecm_out, re.M)
        )
    if quorate is False:
        problems.append("the cluster is not quorate")
    if needed and not qd.get("registered"):
        problems.append(f"the {count}-host cluster needs a QDevice and none is registered")
    if qd.get("registered") and needed is False:
        problems.append(f"a QDevice is registered but the {count}-host cluster must not use one")
    if qd.get("registered") and needed and voting is False:
        problems.append("the registered QDevice is not voting")
    problems.extend(report.get("errors") or [])
    summary = {
        "collected_at": now,
        "probe": report.get("probe"),
        "cluster": {
            "name": cluster_row.get("name"),
            "quorate": quorate,
            "expected_votes": int_or_none(status_value(pvecm_out, "Expected votes")),
            "total_votes": int_or_none(status_value(pvecm_out, "Total votes")),
            "quorum": int_or_none(status_value(pvecm_out, "Quorum")),
            "host_count": count,
            "online_count": sum(1 for host in hosts if host["online"]),
            "control_node": control_node,
        },
        "qdevice": {
            "configured_host": args.qdevice_host or None,
            "registered": qd.get("registered"),
            "address": qd.get("address"),
            "needed": needed,
            "voting": voting,
        },
        "hosts": hosts,
        "problems": problems,
    }

    heading(f"CLUSTER {summary['cluster']['name'] or '?'} - read through {report.get('probe')}")
    votes = summary["cluster"]
    print(f"  Quorate:       {'yes' if quorate else 'NO' if quorate is False else 'unknown'}")
    print(f"  Votes:         {votes['total_votes'] or '?'} of {votes['expected_votes'] or '?'} expected"
          f" (quorum {votes['quorum'] or '?'})")
    print(f"  Hosts online:  {votes['online_count']} of {count}")
    print(f"  Control node:  {control_node or 'not recorded'}")
    qd_state = "not registered"
    if qd.get("registered"):
        qd_state = f"registered ({qd.get('address') or 'unknown address'})"
        if voting is not None:
            qd_state += ", voting" if voting else ", NOT voting"
    print(f"  QDevice:       {qd_state}; "
          f"{'needed (even host count)' if needed else 'not needed (odd host count)' if needed is False else ''}")
    heading("HOSTS")
    table(
        ["HOST", "STATE", "SSH FROM HERE", "UPTIME", "CPU", "MEMORY", "TRANSFER", "SLOT", "ROLE"],
        [
            [
                host["name"],
                "online" if host["online"] else "OFFLINE",
                "-" if host["ssh_from_here"] is None else "ok" if host["ssh_from_here"] else "FAILED",
                duration(host["uptime_seconds"]) if host["online"] else "-",
                f"{(host['cpu_fraction'] or 0) * 100:.0f}% of {host['cores'] or '?'}" if host["online"] else "-",
                f"{size(host['memory_used'])} / {size(host['memory_total'])}" if host["online"] else "-",
                transfer_text(host["transfer"]),
                host["slot_state"] or "-",
                ", ".join(role for role, on in (("control", host["is_control"]), ("probe", host["is_probe"])) if on) or "-",
            ]
            for host in hosts
        ],
    )
    finish(summary, problems, ui, [
        (any("offline" in p or "SSH" in p for p in problems),
         "Run the full health check to see why a host is offline or unreachable.",
         "scripts/user_callable/diagnostics/show_cluster_health.sh", "show_cluster_health", {}),
        (any("QDevice" in p for p in problems),
         "Inspect the QDevice and follow its repair instructions.",
         "scripts/user_callable/diagnostics/show_qdevice_state.sh", "show_qdevice_state", {}),
    ])


def first_ha(report, sid):
    for row in report.get("ha_status") or []:
        if row.get("sid") == sid:
            return row
    return None


STAGING_SNAPSHOT_STAMP_RE = re.compile(r"-([0-9]{6}T[0-9]{6}Z)$")


def staging_snapshot(resource, resources):
    """The source snapshot a staging VM was cloned from.

    created_at comes from the UTC stamp add_staging_vm.sh puts at the end of
    every snapshot name. shared_with lists the other staging VMs cloned from
    the same snapshot; the snapshot is deleted only with the last of them.
    """
    snapshot = (resource.get("proxmox") or {}).get("snapshot")
    if not isinstance(snapshot, dict) or not snapshot.get("name"):
        return None
    name = snapshot["name"]
    created_at = None
    match = STAGING_SNAPSHOT_STAMP_RE.search(name)
    if match:
        try:
            created_at = calendar.timegm(
                time.strptime(match.group(1), "%y%m%dT%H%M%SZ")
            )
        except ValueError:
            created_at = None
    shared_with = sorted(
        (
            row.get("name")
            for row in resources
            if row.get("kind") == "staging"
            and row.get("name") != resource.get("name")
            and row.get("source") == resource.get("source")
            and ((row.get("proxmox") or {}).get("snapshot") or {}).get("name") == name
        ),
        key=lambda value: str(value),
    )
    return {
        "name": name,
        "created_at": created_at,
        "verified": snapshot.get("verified") is True,
        "shared_with": shared_with,
    }


def staging_restore_point(resource):
    """A backup-linked staging VM's restore point, with the time in its name."""
    backup = resource.get("backup")
    if not isinstance(backup, dict) or not backup.get("snapshot"):
        return None
    snapshot = str(backup["snapshot"])
    try:
        created_at = calendar.timegm(time.strptime(snapshot.rsplit("/", 1)[-1], "%Y-%m-%dT%H:%M:%SZ"))
    except ValueError:
        created_at = None
    return {"snapshot": snapshot, "storage": backup.get("storage"), "created_at": created_at}


def snapshot_taken_text(epoch):
    if epoch is None:
        return "-"
    return time.strftime("%Y-%m-%d %H:%M UTC", time.gmtime(epoch))


def render_guests(report, args, ui):
    now = report.get("collected_at") or int(time.time())
    problems = []
    live_by_vmid = {
        int(row["vmid"]): row for row in report.get("live") or []
        if str(row.get("vmid", "")).isdigit()
    }
    replication_by_guest = {}
    for job in report.get("replication") or []:
        guest = int_or_none(job.get("guest"))
        if guest is not None:
            replication_by_guest.setdefault(guest, []).append(job)
    ha_config = {row.get("sid"): row for row in report.get("ha_resources") or []}
    production, staging = [], []
    for resource in sorted(
        report.get("resources") or [],
        key=lambda row: (row.get("kind", ""), int_or_none(row.get("index")) or 0),
    ):
        vmid = int_or_none(resource.get("vmid"))
        observed = live_by_vmid.get(vmid, {})
        domains = resource.get("domains") or {}
        spec = resource.get("spec") or {}
        common = {
            "name": resource.get("name"),
            "vmid": vmid,
            "registry_state": resource.get("state"),
            "live_status": observed.get("status", "absent"),
            "node": observed.get("node"),
            "ip": resource.get("ip"),
            "placement": resource.get("placement") or [],
            "domain": domains.get("primary"),
            # Proxmox's own figures (here and for the disk below), so a rollback
            # or a change made in its web UI shows at once; the registry's spec
            # is the fallback.
            "cores": int_or_none(observed.get("maxcpu")) or spec.get("cores"),
            "memory_mb": (int_or_none(observed.get("maxmem")) or 0) // 2**20 or spec.get("memory_mb"),
            "disk_gib": spec.get("disk_gib"),
            "disk_bytes": int_or_none(observed.get("maxdisk"))
            or int_or_none(spec.get("disk_bytes"))
            or (int_or_none(spec.get("disk_gib")) or 0) * 2**30 or None,
            "uptime_seconds": int_or_none(observed.get("uptime")),
        }
        if resource.get("kind") == "production":
            sid = f"vm:{vmid}"
            jobs = replication_by_guest.get(vmid, [])
            failing = [job for job in jobs if int_or_none(job.get("fail_count")) or job.get("error")]
            syncs = [int_or_none(job.get("last_sync")) or 0 for job in jobs]
            ha_row = first_ha(report, sid)
            entry = dict(common)
            entry.update({
                "purpose": (resource.get("purpose") or {}).get("slug"),
                "aliases": domains.get("aliases") or [],
                "owner_node": resource.get("owner_node"),
                "ha": {
                    "configured": sid in ha_config,
                    "requested_state": (ha_config.get(sid) or {}).get("state"),
                    "state": (ha_row or {}).get("state"),
                    "node": (ha_row or {}).get("node"),
                },
                "replication": {
                    "jobs": len(jobs),
                    "failing": len(failing),
                    "targets": sorted({job.get("target") for job in jobs if job.get("target")},
                                      key=node_sort_key),
                    "oldest_last_sync": min(syncs) if syncs else None,
                    "errors": [str(job.get("error")) for job in failing if job.get("error")][:3],
                },
            })
            production.append(entry)
            name = entry["name"]
            if entry["registry_state"] == "active" and entry["live_status"] != "running":
                problems.append(f"{name} is registered active but is {entry['live_status']}")
            if failing:
                problems.append(f"{name} has {len(failing)} failing replication job(s)")
            if entry["registry_state"] == "active" and not entry["ha"]["configured"]:
                problems.append(f"{name} has no HA resource")
            if entry["owner_node"] and entry["node"] and entry["owner_node"] != entry["node"]:
                problems.append(
                    f"{name} runs on {entry['node']} but the registry owner is {entry['owner_node']}"
                )
        else:
            entry = dict(common)
            entry.update({
                "source": resource.get("source"),
                "url": f"https://{common['domain']}" if common["domain"] else None,
                "snapshot": staging_snapshot(resource, report.get("resources") or []),
                # backup: its disk is an overlay over a restore point on PBS.
                "link": "backup" if "backup" in resource else "prod",
                "restore_point": staging_restore_point(resource),
            })
            staging.append(entry)
            if entry["registry_state"] in ("cleanup_pending", "failed"):
                problems.append(f"staging {entry['name']} is {entry['registry_state']}")
    registered = {item["vmid"] for item in production + staging}
    unregistered = [
        {"vmid": int(row["vmid"]), "name": row.get("name"), "node": row.get("node"),
         "status": row.get("status")}
        for row in report.get("live") or []
        if row.get("type") == "qemu" and str(row.get("vmid", "")).isdigit()
        and int(row["vmid"]) not in registered
    ]
    problems.extend(report.get("errors") or [])
    if args.kind_filter == "production":
        staging = []
    elif args.kind_filter == "staging":
        production = []
    summary = {
        "collected_at": now,
        "probe": report.get("probe"),
        "production": production,
        "staging": staging,
        "unregistered": unregistered,
        "problems": problems,
    }
    if args.kind_filter != "staging":
        heading("PRODUCTION VMS")
        if production:
            table(
                ["NAME", "VMID", "REGISTRY", "LIVE", "NODE", "PLACEMENT", "HA", "REPLICATION", "IP", "DOMAIN"],
                [
                    [
                        row["name"], row["vmid"], row["registry_state"], row["live_status"],
                        row["node"] or "-", ",".join(row["placement"]),
                        row["ha"]["state"] or ("configured" if row["ha"]["configured"] else "none"),
                        (f"{row['replication']['jobs'] - row['replication']['failing']}/{row['replication']['jobs']} ok, "
                         f"{ago(row['replication']['oldest_last_sync'], now)}")
                        if row["replication"]["jobs"] else "none",
                        row["ip"], row["domain"] or "-",
                    ]
                    for row in production
                ],
            )
        else:
            print("  No production VMs are registered.")
    if args.kind_filter != "production":
        heading("STAGING VMS")
        if staging:
            table(
                ["NAME", "VMID", "SOURCE", "REGISTRY", "LIVE", "NODE", "IP", "URL",
                 "SNAPSHOT", "TAKEN", "SHARED WITH"],
                [
                    [row["name"], row["vmid"], row["source"], row["registry_state"],
                     row["live_status"], row["node"] or "-", row["ip"], row["url"] or "-",
                     f"restore point {row['restore_point']['snapshot']}" if row["restore_point"]
                     else (row["snapshot"] or {}).get("name") or "-",
                     snapshot_taken_text((row["restore_point"] or row["snapshot"] or {}).get("created_at")),
                     ", ".join((row["snapshot"] or {}).get("shared_with") or []) or "-"]
                    for row in staging
                ],
            )
        else:
            print("  No staging VMs are registered.")
    if unregistered:
        heading("UNREGISTERED QEMU VMS")
        table(["VMID", "NAME", "NODE", "STATUS"],
              [[row["vmid"], row["name"], row["node"], row["status"]] for row in unregistered])
    steps = []
    for row in production:
        if row["replication"]["failing"] or (
            row["registry_state"] == "active" and row["live_status"] != "running"
        ):
            steps.append((True, f"Inspect {row['name']} across HA, replication, routing, and SSH.",
                          f"scripts/user_callable/diagnostics/show_prod_vm_state.sh {row['name']}",
                          "show_prod_vm_state", {"resource": row["name"]}))
    moved = [row["name"] for row in production
             if row["owner_node"] and row["node"] and row["owner_node"] != row["node"]]
    if moved:
        steps.append((True, f"Record where {', '.join(moved)} now run{'s' if len(moved) == 1 else ''} (moves nothing).",
                      "scripts/user_callable/guests/prod/reconcile_prod_vm_owner.sh",
                      "reconcile_prod_vm_owner", {}))
    for row in staging:
        if row["registry_state"] in ("cleanup_pending", "failed"):
            steps.append((True, f"Finish removing {row['name']}.",
                          f"scripts/user_callable/guests/staging/remove_staging_vm.sh {row['name']}",
                          "remove_staging_vm", {"resource": row["name"]}))
    finish(summary, problems, ui, steps)


def render_replication(report, args, ui):
    now = report.get("collected_at") or int(time.time())
    names = {
        int_or_none(row.get("vmid")): row.get("name") for row in report.get("resources") or []
    }
    jobs = []
    problems = []
    for job in sorted(report.get("replication") or [], key=lambda row: str(row.get("id"))):
        guest = int_or_none(job.get("guest"))
        entry = {
            "id": job.get("id"),
            "guest": guest,
            "guest_name": names.get(guest),
            "source": job.get("source"),
            "target": job.get("target"),
            "schedule": job.get("schedule"),
            "last_sync": int_or_none(job.get("last_sync")),
            "last_try": int_or_none(job.get("last_try")),
            "next_sync": int_or_none(job.get("next_sync")),
            "duration": job.get("duration"),
            "fail_count": int_or_none(job.get("fail_count")) or 0,
            "error": job.get("error"),
            "disabled": bool(job.get("disable")),
        }
        if args.guest and entry["guest_name"] != args.guest:
            continue
        jobs.append(entry)
        if entry["fail_count"] or entry["error"]:
            problems.append(
                f"replication job {entry['id']} ({entry['guest_name'] or entry['guest']} "
                f"{entry['source']} -> {entry['target']}) is failing: {entry['error'] or 'see pvesr status'}"
            )
    problems.extend(report.get("errors") or [])
    summary = {"collected_at": now, "probe": report.get("probe"), "jobs": jobs, "problems": problems}
    heading("REPLICATION JOBS")
    if jobs:
        table(
            ["JOB", "GUEST", "SOURCE", "TARGET", "SCHEDULE", "LAST SYNC", "NEXT", "FAILS", "ERROR"],
            [
                [row["id"], row["guest_name"] or row["guest"], row["source"], row["target"],
                 row["schedule"] or "-", ago(row["last_sync"], now),
                 until(row["next_sync"], now),
                 row["fail_count"], (row["error"] or "")[:60]]
                for row in jobs
            ],
        )
    else:
        print("  No replication jobs were found.")
    steps = []
    for name in sorted({row["guest_name"] for row in jobs if row["fail_count"] and row["guest_name"]}):
        steps.append((True, f"Inspect {name}'s replication and HA state.",
                      f"scripts/user_callable/diagnostics/show_prod_vm_state.sh {name}",
                      "show_prod_vm_state", {"resource": name}))
    finish(summary, problems, ui, steps)


def render_storage(report, args, ui):
    now = report.get("collected_at") or int(time.time())
    problems = []
    hosts = []
    for host in report.get("hosts") or []:
        pools = []
        for pool in host.get("pools") or []:
            total = int_or_none(pool.get("size")) or 0
            alloc = int_or_none(pool.get("alloc")) or 0
            free = int_or_none(pool.get("free")) or 0
            entry = {
                "name": pool.get("name"),
                "health": pool.get("health"),
                "size": total,
                "alloc": alloc,
                "free": free,
                "frag": int_or_none(str(pool.get("frag", "")).rstrip("%")),
                "free_fraction": (free / total) if total else None,
            }
            pools.append(entry)
            if entry["health"] and entry["health"] != "ONLINE":
                problems.append(f"{host['node']}: pool {entry['name']} is {entry['health']}")
            if entry["free_fraction"] is not None and entry["free_fraction"] < 0.10:
                problems.append(
                    f"{host['node']}: pool {entry['name']} has only {entry['free_fraction']:.0%} free"
                )
        storages = [
            {
                "storage": row.get("storage"),
                "type": row.get("type"),
                "active": bool(row.get("active")),
                "enabled": row.get("enabled", 1) not in (0, "0", False),
                "total": int_or_none(row.get("total")),
                "used": int_or_none(row.get("used")),
                "avail": int_or_none(row.get("avail")),
                "content": row.get("content"),
            }
            for row in host.get("storages") or []
        ]
        if not host.get("online"):
            problems.append(f"{host['node']} is offline; its storage was not inspected")
        for error in host.get("errors") or []:
            problems.append(f"{host['node']}: {error}")
        hosts.append({"node": host.get("node"), "online": host.get("online"),
                      "pools": pools, "storages": storages})
    problems.extend(report.get("errors") or [])
    summary = {"collected_at": now, "probe": report.get("probe"), "hosts": hosts, "problems": problems}
    heading("ZFS POOLS")
    rows = []
    for host in hosts:
        if not host["online"]:
            rows.append([host["node"], "-", "OFFLINE", "-", "-", "-", "-"])
        for pool in host["pools"]:
            rows.append([
                host["node"], pool["name"], pool["health"], size(pool["size"]),
                size(pool["alloc"]), size(pool["free"]),
                f"{pool['free_fraction']:.0%}" if pool["free_fraction"] is not None else "?",
            ])
    table(["HOST", "POOL", "HEALTH", "SIZE", "ALLOCATED", "FREE", "FREE %"], rows)
    heading("PROXMOX STORAGE")
    table(
        ["HOST", "STORAGE", "TYPE", "ACTIVE", "USED", "AVAILABLE"],
        [
            [host["node"], row["storage"], row["type"], "yes" if row["active"] else "no",
             size(row["used"]), size(row["avail"])]
            for host in hosts for row in host["storages"]
        ],
    )
    steps = []
    for host in hosts:
        if any(problem.startswith(f"{host['node']}: pool") for problem in problems):
            steps.append((True, f"Inspect {host['node']}'s pool members and disk serials.",
                          f"scripts/user_callable/diagnostics/show_proxmox_host_state.sh --host {host['node']}",
                          "show_proxmox_host_state", {"host": host["node"]}))
    finish(summary, problems, ui, steps)


def backup_notes(text):
    """The fields of BMAC's snapshot notes, `bmac prodN key=value ... [| comment]`,
    or None."""
    first = str(text or "").splitlines()[0] if text else ""
    if not first.startswith(BACKUP_NOTES_PREFIX):
        return None
    first, _, comment = first.partition(" | ")
    words = first.split()
    fields = {"name": words[1] if len(words) > 1 and "=" not in words[1] else None,
              "comment": comment.strip() or None}
    for word in words[1:]:
        key, sep, value = word.partition("=")
        if sep:
            fields[key] = value
    return fields


def backup_interval_hours(schedule):
    """Hours between runs of a BMAC schedule (`0/N:MM` or `HH:MM`), or None."""
    schedule = str(schedule or "").strip()
    match = re.fullmatch(r"0/([0-9]{1,2}):[0-5][0-9]", schedule)
    if match and int(match.group(1)) in (1, 2, 3, 4, 6, 8, 12):
        return int(match.group(1))
    if re.fullmatch(r"(?:[01]?[0-9]|2[0-3]):[0-5][0-9]", schedule):
        return 24
    return None


def keep_settings(job):
    return {key.removeprefix("keep-"): value for key, value in sorted(job.items())
            if key.startswith("keep-")}


def render_backups(report, args, ui):
    now = report.get("collected_at") or int(time.time())
    problems = list(report.get("errors") or [])
    storage = report.get("storage")
    status = report.get("storage_status") or {}
    inspect = report.get("inspect") or {}
    key = (storage or {}).get("encryption-key")
    summary_storage = {
        "configured": storage is not None,
        "id": BACKUP_STORAGE,
        "active": bool(status.get("active")) if storage else None,
        "server": (storage or {}).get("server"),
        "port": int_or_none((storage or {}).get("port")) or (8007 if storage else None),
        "datastore": (storage or {}).get("datastore"),
        "namespace": (storage or {}).get("namespace"),
        "username": (storage or {}).get("username"),
        "key_fingerprint": key,
        "master_pubkey": bool((storage or {}).get("master-pubkey")),
    }
    if storage and not summary_storage["active"]:
        problems.append(f"storage {BACKUP_STORAGE} is not active on {report.get('probe')}: "
                        "PBS may be unreachable")
    if storage and not key:
        problems.append(f"storage {BACKUP_STORAGE} has no encryption key")

    prune = inspect.get("prune") or {}
    retention = {
        "readable": bool(prune.get("readable")),
        "jobs": [{"id": job.get("id"), "schedule": job.get("schedule"), "keep": keep_settings(job)}
                 for job in prune.get("jobs") or []],
        "error": inspect.get("error") or prune.get("error"),
    }
    if storage and inspect.get("error"):
        problems.append(f"could not read the PBS account: {inspect['error']}")
    privileges = inspect.get("privileges") or {}
    for missing in privileges.get("missing") or []:
        problems.append(f"the PBS token lacks {missing} on {privileges.get('path')}")
    for row in privileges.get("excess") or []:
        if row.get("dangerous"):
            problems.append(f"the PBS token holds {row.get('privilege')} on {row.get('path')}, "
                            "which can delete backups")

    resources = report.get("resources") or []
    productions = sorted((row for row in resources if row.get("kind") == "production"),
                         key=lambda row: int_or_none(row.get("index")) or 0)
    registered = {row.get("name"): row for row in productions}

    points = []
    unmanaged = 0
    for row in report.get("content") or []:
        if row.get("content") != "backup":
            continue
        notes = backup_notes(row.get("notes"))
        if notes is None:
            unmanaged += 1
            continue
        # PVE reports the key fingerprint, or "1" when PBS does not say which.
        encrypted = row.get("encrypted")
        encrypted = None if encrypted in (None, "", 0, "0") else str(encrypted)
        points.append({
            "volid": row.get("volid"),
            "vmid": int_or_none(row.get("vmid")),
            "time": int_or_none(row.get("ctime")),
            "name": notes.get("name"),
            "purpose": notes.get("slug"),
            "fqdn": notes.get("fqdn"),
            "cluster": notes.get("cluster"),
            "notes": str(row.get("notes") or "").splitlines()[0],
            "comment": notes.get("comment"),
            "protected": bool(row.get("protected")),
            "size": int_or_none(row.get("size")),
            "encrypted": encrypted,
            "key_matches": encrypted is not None and encrypted.lower() == str(key or "").lower(),
            "verified": (row.get("verification") or {}).get("state"),
        })
    points.sort(key=lambda point: (point["time"] or 0), reverse=True)

    metrics = report.get("metrics") or {}
    for point in points:
        point["metrics"] = metrics.get(str(point["volid"] or "").split(":backup/", 1)[-1])

    unencrypted = [point for point in points if not point["encrypted"]]
    other_key = [point for point in points if point["encrypted"] and not point["key_matches"]]
    if unencrypted:
        problems.append(f"{len(unencrypted)} BMAC restore point(s) are not encrypted")
    if other_key:
        problems.append(f"{len(other_key)} restore point(s) were encrypted with another key; "
                        "this cluster cannot restore them")

    jobs = {row.get("id"): row for row in report.get("jobs") or []
            if str(row.get("id", "")).startswith(BACKUP_JOB_PREFIX)}
    vms = []
    for production in productions:
        name = production.get("name")
        vmid = int_or_none(production.get("vmid"))
        job = jobs.pop(f"{BACKUP_JOB_PREFIX}{name}", None)
        own = [point for point in points if point["vmid"] == vmid and point["name"] == name]
        entry = {
            "name": name,
            "vmid": vmid,
            "purpose": (production.get("purpose") or {}).get("slug"),
            "fqdn": (production.get("domains") or {}).get("primary"),
            "state": production.get("state"),
            "job": None,
            "latest_backup": own[0]["time"] if own else None,
            "restore_points": len(own),
        }
        if job:
            hours = backup_interval_hours(job.get("schedule"))
            entry["job"] = {
                "id": job.get("id"),
                "enabled": job.get("enabled", 1) not in (0, "0", False),
                "schedule": job.get("schedule"),
                "every_hours": hours,
                "next_run": int_or_none(job.get("next-run")),
                "bwlimit": int_or_none(job.get("bwlimit")),
            }
            if job.get("storage") != BACKUP_STORAGE or str(job.get("vmid")) != str(vmid):
                problems.append(f"backup job {job.get('id')} does not back up {name} (VMID {vmid}) "
                                f"to {BACKUP_STORAGE}; change {name}'s backup schedule to repair it")
            # Two missed runs, plus an hour for a slow one, before it is overdue.
            newest = entry["latest_backup"]
            if entry["job"]["enabled"] and hours and newest is not None and \
                    now - newest > (2 * hours + 1) * 3600:
                problems.append(f"{name}'s newest backup is {ago(newest, now)}, "
                                f"but it is backed up every {hours}h")
        vms.append(entry)
    for job in jobs.values():
        problems.append(f"backup job {job.get('id')} belongs to no registered production VM "
                        f"(it backs up VMID {job.get('vmid')})")

    summary = {
        "collected_at": now,
        "probe": report.get("probe"),
        "storage": summary_storage,
        "retention": retention,
        "vms": vms,
        "restore_points": points,
        "unmanaged_backups": unmanaged,
        "problems": problems,
    }

    heading("BACKUP STORAGE")
    if not storage:
        print(f"  PBS is not set up: the cluster has no {BACKUP_STORAGE} storage.")
    else:
        print(f"  {BACKUP_STORAGE}: {'active' if summary_storage['active'] else 'NOT ACTIVE'}, "
              f"{summary_storage['username']} at {summary_storage['server']}:{summary_storage['port']}, "
              f"datastore {summary_storage['datastore']}, namespace {summary_storage['namespace']}")
        print(f"  Encryption key: {key or 'none'}"
              f"{' (with master public key)' if summary_storage['master_pubkey'] else ''}")
        if not retention["readable"]:
            print(f"  Retention: not readable with this token ({retention['error'] or 'unknown error'})")
        elif not retention["jobs"]:
            print("  Retention: the PBS server has no prune job for this namespace; it keeps every backup")
        for job in retention["jobs"]:
            keeps = ", ".join(f"keep-{name} {value}" for name, value in job["keep"].items())
            print(f"  Retention ({job['id']}, runs {job['schedule'] or '?'} PBS time): {keeps or 'keep everything'}")

    heading("PRODUCTION VM BACKUPS")
    if vms:
        table(
            ["VM", "VMID", "PURPOSE", "SCHEDULE", "NEXT RUN", "NEWEST BACKUP", "RESTORE POINTS"],
            [
                [vm["name"], vm["vmid"], vm["purpose"] or "-",
                 ("-" if not vm["job"] else
                  ("disabled" if not vm["job"]["enabled"] else
                   (f"every {vm['job']['every_hours']}h" if vm["job"]["every_hours"]
                    else vm["job"]["schedule"]))),
                 until(vm["job"]["next_run"], now) if vm["job"] and vm["job"]["enabled"] else "-",
                 ago(vm["latest_backup"], now) if vm["latest_backup"] else "none",
                 vm["restore_points"]]
                for vm in vms
            ],
        )
    else:
        print("  No production VMs are registered.")

    heading("RESTORE POINTS")
    if points:
        table(
            ["TAKEN", "VM", "VMID", "PROTECTED", "KEY", "BACKUP", "REUSED", "NEW", "TOOK", "VOLUME"],
            [
                [time.strftime("%Y-%m-%d %H:%M %Z", time.localtime(point["time"])) if point["time"] else "?",
                 point["name"] or "?", point["vmid"], "yes" if point["protected"] else "",
                 "ok" if point["key_matches"] else ("none" if not point["encrypted"] else "OTHER"),
                 *metrics_columns(point["metrics"]),
                 point["volid"]]
                for point in points
            ],
        )
    else:
        print("  No BMAC restore points." if storage else "  -")
    if unmanaged:
        print(f"\n  {unmanaged} other backup(s) in the namespace were not made by BMAC and are not listed.")
    commented = [point for point in points if point["comment"]]
    if commented:
        print("\n  Comments:")
        for point in commented:
            taken = time.strftime("%Y-%m-%d %H:%M %Z", time.localtime(point["time"])) if point["time"] else "?"
            print(f"    {taken}  {point['name'] or '?'}: {point['comment']}")

    steps = []
    if not storage:
        steps.append((True, "Connect the cluster to its Proxmox Backup Server.",
                      "scripts/user_callable/backups/setup_pbs.sh", "setup_pbs", {}))
    finish(summary, problems, ui, steps)


def metrics_columns(metrics):
    if not metrics:
        return ["no log", "-", "-", "-"]
    mode = "incremental" if metrics["mode"] == "fast-incremental" else "full read"
    reused = (f"{size(metrics['reused_bytes'])} ({metrics['reused_percent']}%)"
              if metrics.get("reused_bytes") is not None else "?")
    new = f"~{size(metrics['new_bytes'])}" if metrics.get("new_bytes") is not None else "?"
    took = duration(metrics["duration_seconds"]) if metrics.get("duration_seconds") is not None else "?"
    if metrics.get("duration_seconds") is not None and metrics["duration_seconds"] < 60:
        took = f"{metrics['duration_seconds']}s"
    return [mode, reused, new, took]


def finish(summary, problems, ui, steps):
    print()
    if problems:
        print(f"ATTENTION: {len(problems)} item{'s' if len(problems) != 1 else ''} need attention:")
        for problem in problems:
            print(f"  - {problem}")
    else:
        print("OK: nothing needs attention.")
    chosen = [step for step in steps if step[0]]
    if chosen:
        print("\nNext steps:")
        for _, text, command, _, _ in chosen:
            print(f"  - {text}\n    {command}")
    ui.emit_event({"type": "result", "data": summary})
    for _, text, command, workflow, args in chosen:
        ui.emit_next_step(text, command, workflow, args)


RENDERERS = {
    "hosts": render_hosts,
    "guests": render_guests,
    "replication": render_replication,
    "storage": render_storage,
    "backups": render_backups,
}


def command_render(args):
    try:
        report = json.load(open(args.report, encoding="utf-8"))
    except (OSError, ValueError) as exc:
        print(f"ERROR: could not read the collected state: {exc}", file=sys.stderr)
        return 1
    RENDERERS[args.kind](report, args, emitter())
    return 0


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    sub = parser.add_subparsers(dest="command", required=True)
    collect = sub.add_parser("collect")
    collect.add_argument("kind", choices=sorted(COLLECTORS))
    collect.add_argument("--registry", default="")
    collect.add_argument("--state-dir", default="/etc/pve/priv/app-ha")
    collect.add_argument("--host", default="")
    collect.add_argument("--pbs-api", default="")
    collect.add_argument("--metrics-cache", default="", help=f"backups only; default {METRICS_CACHE}")
    collect.add_argument("--internal-domain", default="", help="backups only; to read other hosts' caches")
    collect.set_defaults(handler=command_collect)
    render = sub.add_parser("render")
    render.add_argument("kind", choices=sorted(RENDERERS))
    render.add_argument("--report", required=True)
    render.add_argument("--ssh-file", default="")
    render.add_argument("--transfer-dir", default="")
    render.add_argument("--qdevice-host", default="")
    render.add_argument("--kind-filter", default="", choices=("", "production", "staging"))
    render.add_argument("--guest", default="")
    render.set_defaults(handler=command_render)
    args = parser.parse_args(argv)
    return args.handler(args)


if __name__ == "__main__":
    raise SystemExit(main())
