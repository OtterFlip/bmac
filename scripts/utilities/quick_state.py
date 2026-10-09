#!/usr/bin/env python3

# Copyright (c) 2026 BEENTHERE VENTURES, INC.
# SPDX-License-Identifier: GPL-3.0-only

"""Fast, read-only cluster state for the
scripts/user_callable/diagnostics/list_*.sh scripts.

`collect KIND` runs on one cluster member as root, fed over SSH as
`python3 - collect KIND ...`. It runs only read-only Proxmox API (pvesh),
pvecm, and registry queries and prints one JSON document. `render KIND` runs
on the workstation: it prints a terminal report and, in JSON mode, emits the
same state as one structured `result` event plus `next_step` suggestions.

KIND is hosts, guests, replication, or storage.
"""

import argparse
import json
import os
import re
import subprocess
import sys
import time


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
    routes, err = registry(args, "list-routes")
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
        "routes": routes or [],
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


COLLECTORS = {
    "hosts": collect_hosts,
    "guests": collect_guests,
    "replication": collect_replication,
    "storage": collect_storage,
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
        ["HOST", "STATE", "SSH FROM HERE", "UPTIME", "CPU", "MEMORY", "SLOT", "ROLE"],
        [
            [
                host["name"],
                "online" if host["online"] else "OFFLINE",
                "-" if host["ssh_from_here"] is None else "ok" if host["ssh_from_here"] else "FAILED",
                duration(host["uptime_seconds"]) if host["online"] else "-",
                f"{(host['cpu_fraction'] or 0) * 100:.0f}% of {host['cores'] or '?'}" if host["online"] else "-",
                f"{size(host['memory_used'])} / {size(host['memory_total'])}" if host["online"] else "-",
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


def render_guests(report, args, ui):
    now = report.get("collected_at") or int(time.time())
    problems = []
    live_by_vmid = {
        int(row["vmid"]): row for row in report.get("live") or []
        if str(row.get("vmid", "")).isdigit()
    }
    route_counts = {}
    for route in report.get("routes") or []:
        name = route.get("resource")
        route_counts[name] = route_counts.get(name, 0) + 1
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
            "routes": route_counts.get(resource.get("name"), 0),
            "routes_enabled": bool(resource.get("routes_enabled")),
            "domain": domains.get("primary"),
            "cores": spec.get("cores"),
            "memory_mb": spec.get("memory_mb"),
            "disk_gib": spec.get("disk_gib"),
            "disk_bytes": int_or_none(spec.get("disk_bytes"))
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
                ["NAME", "VMID", "REGISTRY", "LIVE", "NODE", "PLACEMENT", "HA", "REPLICATION", "ROUTES", "DOMAIN"],
                [
                    [
                        row["name"], row["vmid"], row["registry_state"], row["live_status"],
                        row["node"] or "-", ",".join(row["placement"]),
                        row["ha"]["state"] or ("configured" if row["ha"]["configured"] else "none"),
                        (f"{row['replication']['jobs'] - row['replication']['failing']}/{row['replication']['jobs']} ok, "
                         f"{ago(row['replication']['oldest_last_sync'], now)}")
                        if row["replication"]["jobs"] else "none",
                        row["routes"], row["domain"] or "-",
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
                ["NAME", "VMID", "SOURCE", "REGISTRY", "LIVE", "NODE", "IP", "URL"],
                [
                    [row["name"], row["vmid"], row["source"], row["registry_state"],
                     row["live_status"], row["node"] or "-", row["ip"], row["url"] or "-"]
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
    collect.set_defaults(handler=command_collect)
    render = sub.add_parser("render")
    render.add_argument("kind", choices=sorted(RENDERERS))
    render.add_argument("--report", required=True)
    render.add_argument("--ssh-file", default="")
    render.add_argument("--qdevice-host", default="")
    render.add_argument("--kind-filter", default="", choices=("", "production", "staging"))
    render.add_argument("--guest", default="")
    render.set_defaults(handler=command_render)
    args = parser.parse_args(argv)
    return args.handler(args)


if __name__ == "__main__":
    raise SystemExit(main())
