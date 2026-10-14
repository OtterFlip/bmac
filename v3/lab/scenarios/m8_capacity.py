# Copyright (c) 2026 BEENTHERE VENTURES, INC.
# SPDX-License-Identifier: GPL-3.0-only

"""M8.5's capacity measurements in the lab: on mox1 of the three-host
cluster (m7_cluster.staged), a scratch pool of sparse files is made for each
raidz layout below (ashift 12, compression off), with one sparse zvol per
offered block size. Each zvol is filled with the same random data, and its
`used` is compared with what `bmac capacity --json` on that pool predicts
for it (`V x I(b) x (1 + m(b))`, STORAGE.md "Rules"). They must agree
within 1%.

The measurements are written to m8-capacity.json in the lab's folder, for
bmac-storage's tests (tests/fixtures/lab/capacity.json). The scratch pool
is destroyed after each layout, and mox1 is put back afterwards."""

from __future__ import annotations

import json
from pathlib import Path

from bmaclab.scenario import load

DESCRIPTION = "zvols of every offered block size on raidz1, raidz2, and raidz3 of several widths use what the capacity function predicts, within 1%"

cluster = load(Path(__file__).with_name("m7_cluster.py"))

LAYOUTS = [("raidz1", 3), ("raidz1", 4), ("raidz1", 5), ("raidz1", 6),
           ("raidz2", 4), ("raidz2", 6), ("raidz2", 8), ("raidz2", 10),
           ("raidz3", 5), ("raidz3", 7), ("raidz3", 9)]
SIZES = {"16K": 16 << 10, "32K": 32 << 10, "64K": 64 << 10, "128K": 128 << 10}
VOLSIZE = 512 << 20
TOLERANCE = 0.01
POOL = "bmaccap"
FOLDER = "/var/tmp/bmaccap"

MEASURE = r"""
set -euo pipefail
kind=$1 width=$2 volsize=$3
shift 3
pool=__POOL__ dir=__FOLDER__
zpool destroy -f "$pool" 2>/dev/null || true
rm -rf "$dir"
mkdir -p "$dir"
files=()
for i in $(seq 1 "$width"); do
  truncate -s 3G "$dir/d$i"
  files+=("$dir/d$i")
done
[[ -s /var/tmp/bmaccap.random ]] ||
  head -c "$volsize" /dev/urandom >/var/tmp/bmaccap.random
zpool create -f -o ashift=12 -O compression=off -O mountpoint=none "$pool" "$kind" "${files[@]}"
for size in "$@"; do
  zfs create -s -V "$volsize" -o volblocksize="${size%:*}" "$pool/v${size%:*}"
done
udevadm settle
for size in "$@"; do
  dd if=/var/tmp/bmaccap.random of="/dev/zvol/$pool/v${size%:*}" bs=1M oflag=direct status=none
done
zpool sync "$pool"
sleep 5
zpool sync "$pool"
for size in "$@"; do
  used=$(zfs get -Hp -o value used "$pool/v${size%:*}")
  predicted=$(bmac capacity --json --pool "$pool" --volsize "$volsize" --block-size "${size#*:}" || [[ $? == 3 ]])
  printf '%s\t%s\t%s\n' "${size%:*}" "$used" "$(python3 -c 'import json,sys; print(json.load(sys.stdin)["fit"]["needed"])' <<<"$predicted")"
done
zpool destroy "$pool"
rm -rf "$dir"
""".replace("__POOL__", POOL).replace("__FOLDER__", FOLDER)


def measure(s, host) -> list[dict]:
    rows = []
    sizes = " ".join(f"{name}:{size}" for name, size in SIZES.items())
    try:
        for kind, width in LAYOUTS:
            out = host.on_host(f"bash -c {quote(MEASURE)} measure {kind} {width} {VOLSIZE} {sizes}", timeout=1800)
            for line in out.splitlines():
                block, used, predicted = line.split("\t")
                row = {"kind": kind, "width": width, "ashift": 12, "volsize": VOLSIZE,
                       "block_size": SIZES[block], "used": int(used)}
                rows.append(row)
                error = (int(predicted) - row["used"]) / row["used"]
                s.check(f"{kind} of {width}, {block} blocks: used {row['used']}, predicted {predicted} "
                        f"({error:+.2%})", abs(error) <= TOLERANCE)
    finally:
        host.on_host(f"zpool destroy -f {POOL} 2>/dev/null; rm -rf {FOLDER} /var/tmp/bmaccap.random; true")
    return rows


def quote(text: str) -> str:
    return "'" + text.replace("'", "'\\''") + "'"


def then(hosts) -> None:
    mox1 = hosts["mox1"]
    s = mox1.s
    rows = measure(s, mox1)
    out = s.lab.paths.root / "m8-capacity.json"
    out.write_text(json.dumps(rows, indent=1) + "\n")
    s.note(f"the measurements are in {out}")


def run(s) -> None:
    cluster.staged(s, then)
