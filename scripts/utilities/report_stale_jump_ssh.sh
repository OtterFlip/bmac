#!/usr/bin/env bash

# Copyright (c) 2026 BEENTHERE VENTURES, INC.
# SPDX-License-Identifier: GPL-3.0-only

# Report production and staging guest aliases in this workstation's
# ~/.ssh/config whose ProxyJump goes through a mox host that has left the
# cluster, and tell the operator how to repair them. Read-only; always exits 0
# so callers can run it as a final advisory step.

set -uo pipefail

usage() {
  cat <<'EOF'
Usage: lib/report_stale_jump_ssh.sh moxN

Scan ~/.ssh/config for production and staging guest aliases that jump through
moxN and print the guests/setup_jump_ssh_access.sh command that repairs each.
EOF
}

if (($# != 1)) || [[ "$1" == -h || "$1" == --help ]]; then
  usage
  exit 0
fi
removed_host="$1"
[[ "$removed_host" =~ ^mox([1-9]|10)$ ]] || exit 0
[[ -n "${HOME:-}" && "$HOME" == /* ]] || exit 0
ssh_config="${HOME}/.ssh/config"
[[ -f "$ssh_config" ]] || exit 0
command -v python3 >/dev/null 2>&1 || {
  printf '\nCould not check ~/.ssh/config for jump SSH aliases through %s (python3 is unavailable).\n' \
    "$removed_host"
  exit 0
}

repairs=""
if [[ "${BMAC_UI_JSON:-}" == 1 ]]; then
  # shellcheck source=ui_protocol.sh
  source "$(dirname -- "${BASH_SOURCE[0]}")/ui_protocol.sh"
  repairs="$(mktemp)" || repairs=""
fi

python3 - "$ssh_config" "$removed_host" "$repairs" <<'PY' || true
import re
import sys

path, removed, repairs = sys.argv[1:]
begin = re.compile(r"# BEGIN app-ha managed (production|staging) guest (\S+)$")
option = re.compile(r"^\s*([A-Za-z]+)\s*(?:=\s*|\s+)(.*?)\s*$")
guest_name = re.compile(r"(prod|stage[1-9][0-9]*prod)[1-9][0-9]*")


def hop_host(hop):
    hop = hop.strip()
    if "://" in hop:
        hop = hop.split("://", 1)[1]
    hop = hop.rsplit("@", 1)[-1]
    if hop.startswith("["):
        hop = hop[1:].split("]", 1)[0]
    else:
        hop = hop.split(":", 1)[0]
    return hop.lower()


def uses_removed(value):
    for hop in value.split(","):
        host = hop_host(hop)
        if host == removed or host.startswith(removed + "."):
            return True
    return False


try:
    lines = open(path, encoding="utf-8").read().splitlines()
except (OSError, UnicodeDecodeError) as error:
    print(f"\nCould not read ~/.ssh/config to check jump SSH aliases: {error}")
    raise SystemExit(0)

found = {}
managed = None
patterns = []
for raw in lines:
    stripped = raw.strip()
    match = begin.fullmatch(stripped)
    if match:
        managed = {"kind": match.group(1), "alias": match.group(2)}
        continue
    if managed and stripped.startswith("# END app-ha managed "):
        managed = None
        continue
    if not stripped or stripped.startswith("#"):
        continue
    parsed = option.match(raw)
    if not parsed:
        continue
    keyword, value = parsed.group(1).lower(), parsed.group(2)
    if keyword == "host":
        patterns = value.split()
        continue
    if keyword == "match":
        patterns = []
        continue
    if keyword == "hostname":
        address = value
        targets = [managed["alias"]] if managed else [
            p for p in patterns if guest_name.fullmatch(p)
        ]
        for alias in targets:
            found.setdefault(alias, {}).setdefault("address", address)
        continue
    if keyword != "proxyjump" or not uses_removed(value):
        continue
    if managed:
        entry = found.setdefault(managed["alias"], {})
        entry["kind"] = f"{managed['kind']} guest"
    else:
        for alias in (p for p in patterns if guest_name.fullmatch(p)):
            entry = found.setdefault(alias, {})
            entry.setdefault("kind", "hand-written entry")
    for alias in ([managed["alias"]] if managed else patterns):
        if alias in found:
            found[alias].setdefault("jump", value)

stale = {alias: entry for alias, entry in found.items() if "jump" in entry}

print()
if not stale:
    print(f"==> No ~/.ssh/config guest aliases on this workstation jump through {removed}.")
    raise SystemExit(0)

print(f"==> ACTION NEEDED: workstation jump SSH aliases still use {removed}")
print(f"    These ~/.ssh/config entries jump through {removed}, which is no longer")
print("    in the cluster, so 'ssh <alias>' to them will hang or fail:")
for alias in sorted(stale):
    entry = stale[alias]
    details = ", ".join(
        part for part in (
            entry.get("kind"),
            entry.get("address"),
            f"ProxyJump {entry['jump']}",
        ) if part
    )
    print(f"      {alias} ({details})")
print("    Repair each one from this workstation:")
for alias in sorted(stale):
    if guest_name.fullmatch(alias):
        print(f"      guests/setup_jump_ssh_access.sh {alias}")
    else:
        address = stale[alias].get("address", "its private IP")
        print(
            "      guests/setup_jump_ssh_access.sh   "
            f"(pick the guest at {address}, enter alias {alias})"
        )
print("    Other administrators' workstations need the same repair.")
if repairs:
    with open(repairs, "w", encoding="utf-8") as out:
        for alias in sorted(stale):
            registered = 1 if guest_name.fullmatch(alias) else 0
            address = stale[alias].get("address", "its private IP")
            out.write(f"{alias}\t{registered}\t{address}\n")
PY

if [[ -n "$repairs" ]]; then
  while IFS=$'\t' read -r alias registered address; do
    if [[ "$registered" == 1 ]]; then
      bmac_ui_next_step "Repair the jump SSH alias $alias; it still jumps through $removed_host." \
        --command "guests/setup_jump_ssh_access.sh $alias" \
        --workflow setup_jump_ssh_access --arg "resource=$alias"
    else
      bmac_ui_next_step "Repair the hand-written jump SSH alias $alias: pick the guest at $address and enter alias $alias." \
        --command "guests/setup_jump_ssh_access.sh" --workflow setup_jump_ssh_access
    fi
  done <"$repairs"
  rm -f -- "$repairs"
fi
exit 0
