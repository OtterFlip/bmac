#!/usr/bin/env bash

# Copyright (c) 2026 BEENTHERE VENTURES, INC.
# SPDX-License-Identifier: GPL-3.0-only

# Run bash -n and the complete unittest suite inside a local Lima VM built
# from dev/lima-bmac.yaml. The VM is created and started on first use. See
# DEVELOPMENT.md.
#
#   dev/run-tests-in-vm.sh                            everything
#   dev/run-tests-in-vm.sh -v -k reservation          extra unittest args
#   dev/run-tests-in-vm.sh lib.test_haproxy_routes    specific targets
#
# BMAC_LIMA_INSTANCE overrides the instance name (default: bmac).

set -Eeuo pipefail

REPO_ROOT="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd -P)"
INSTANCE="${BMAC_LIMA_INSTANCE:-bmac}"
TEMPLATE="${REPO_ROOT}/dev/lima-bmac.yaml"

die() {
  printf 'ERROR: %s\n' "$*" >&2
  exit 1
}

command -v limactl >/dev/null 2>&1 ||
  die "limactl not found; install Lima with: brew install lima"
[[ "$REPO_ROOT" != *'"'* && "$REPO_ROOT" != *'\'* ]] ||
  die "checkout path must not contain quotes or backslashes: $REPO_ROOT"

if ! limactl list --quiet 2>/dev/null | grep -qxF -- "$INSTANCE"; then
  echo "Creating Lima instance '$INSTANCE' (first run downloads Debian 13)..."
  limactl create --tty=false --name="$INSTANCE" \
    --set ".mounts = [{\"location\": \"${REPO_ROOT}\", \"writable\": true}]" \
    "$TEMPLATE"
fi
if [[ "$(limactl list --format '{{.Status}}' "$INSTANCE")" != Running ]]; then
  limactl start --tty=false "$INSTANCE"
fi

limactl shell "$INSTANCE" -- test -f "${REPO_ROOT}/dev/run-tests-in-vm.sh" ||
  die "instance '$INSTANCE' does not mount ${REPO_ROOT}; recreate it with:
  limactl delete --force $INSTANCE && dev/run-tests-in-vm.sh"

# Passed with bash -c rather than on stdin so that no command in it can
# consume the rest of the script. Empty file lists are fatal: bash -n with no
# operands reads stdin and "passes" without checking anything.
read -r -d '' VM_SCRIPT <<'VM' || true
set -Eeuo pipefail

# virtiofs has briefly reported this checkout as foreign right after boot,
# and Git then refuses to run; the checkout is the caller's own.
git_files() { git -c safe.directory="$PWD" ls-files -- "$@"; }

scripts_list="$(git_files '*.sh')"
[[ -n "$scripts_list" ]] || { echo "ERROR: no tracked *.sh files" >&2; exit 1; }
mapfile -t scripts <<<"$scripts_list"
bash -n "${scripts[@]}" </dev/null
echo "bash -n: ${#scripts[@]} scripts OK"

# Explicit targets look like lib.test_x, lib/test_x.py, or a dotted test id;
# anything else (-v, -k PATTERN, -f) is passed through with the full suite.
has_target=false
for arg in "$@"; do
  [[ "$arg" == *test_* && ( "$arg" == */* || "$arg" == *.* ) ]] &&
    has_target=true
done
if [[ "$has_target" == true ]]; then
  exec python3 -m unittest "$@" </dev/null
fi
tests_list="$(git_files 'test_*.py' '*/test_*.py')"
[[ -n "$tests_list" ]] || { echo "ERROR: no tracked test_*.py files" >&2; exit 1; }
mapfile -t tests <<<"$tests_list"
exec python3 -m unittest "$@" "${tests[@]}" </dev/null
VM

# /tmp is tmpfs on Debian 13; prepare_prod_iso.sh refuses a tmpfs ISO cache.
limactl shell --workdir "$REPO_ROOT" "$INSTANCE" -- \
  env TMPDIR=/var/tmp PYTHONDONTWRITEBYTECODE=1 \
  bash -c "$VM_SCRIPT" run-tests-in-vm "$@"
