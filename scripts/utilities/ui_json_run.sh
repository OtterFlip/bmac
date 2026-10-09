#!/usr/bin/env bash

# Copyright (c) 2026 BEENTHERE VENTURES, INC.
# SPDX-License-Identifier: GPL-3.0-only

# Run one BMAC script in bmac-ui v2 JSON mode. Scripts reach this through
# bmac_ui_bootstrap when they are called with --json; it is not normally run
# by hand:
#
#   scripts/utilities/ui_json_run.sh /abs/path/to/script.sh [SCRIPT ARGS...]
#
# stdout carries only NDJSON: a `protocol` event, `workflow_started`, every
# line the script prints (as `log` events, or the script's own protocol
# events), and finally one `completed` event. stdin carries NDJSON responses.
#
# Exit status is the script's. `completed` reports success for 0, cancelled
# when the operator declined or cancelled (or the script was interrupted),
# and failed otherwise, with the script's last ERROR line as its message.

set -u
set -o pipefail

RUNNER_LIB_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../lib" && pwd -P)"
# shellcheck source=../lib/ui_protocol.sh
source "${RUNNER_LIB_DIR}/ui_protocol.sh"

(($# >= 1)) || {
  printf 'Usage: ui_json_run.sh SCRIPT [ARGS...]\n' >&2
  exit 2
}
SCRIPT="$1"
shift
[[ -f "$SCRIPT" && ! -L "$SCRIPT" ]] || {
  printf 'ERROR: script is missing or is a symlink: %s\n' "$SCRIPT" >&2
  exit 2
}

# A script started by a script that is already in JSON mode shares its
# parent's protocol stream.
if bmac_ui_is_json; then
  exec bash "$SCRIPT" "$@"
fi

command -v python3 >/dev/null 2>&1 || {
  printf '{"type":"protocol","protocol":"%s","version":%s}\n' "$BMAC_UI_PROTOCOL" "$BMAC_UI_PROTOCOL_VERSION"
  printf '{"type":"error","code":"python_missing","message":"python3 is required for JSON mode","recoverable":false}\n'
  printf '{"type":"completed","status":"failed","message":"python3 is required for JSON mode","exit_code":2}\n'
  exit 2
}

WORKFLOW="$(basename -- "$SCRIPT" .sh)"
REPO_ROOT="$(cd -- "${RUNNER_LIB_DIR}/../.." && pwd -P)"
SCRIPT_REL="${SCRIPT#"${REPO_ROOT}/"}"
RUN_ID="${BMAC_UI_RUN_ID:-}"
[[ "$RUN_ID" =~ ^[A-Za-z0-9_.:-]{1,80}$ ]] ||
  RUN_ID="$(python3 -c 'import uuid; print(uuid.uuid4())')"
TOKEN="$(od -An -N12 -tx1 /dev/urandom | tr -d ' \n')"
STATE_DIR="$(mktemp -d "${TMPDIR:-/tmp}/bmac-ui.XXXXXX")"
chmod 0700 "$STATE_DIR"
LAST_ERROR_FILE="${STATE_DIR}/last-error"

exec {STDOUT_FILTER_FD}> >(exec python3 "${RUNNER_LIB_DIR}/ui_protocol.py" filter \
  --token "$TOKEN" --stream stdout)
STDOUT_FILTER_PID=$!
exec {STDERR_FILTER_FD}> >(exec python3 "${RUNNER_LIB_DIR}/ui_protocol.py" filter \
  --token "$TOKEN" --stream stderr --last-error-file "$LAST_ERROR_FILE")
STDERR_FILTER_PID=$!
exec {INPUT_FD}<&0

export BMAC_UI_JSON=1
export BMAC_UI_TOKEN="$TOKEN"
export BMAC_UI_EVENT_FD="$STDOUT_FILTER_FD"
export BMAC_UI_INPUT_FD="$INPUT_FD"
export BMAC_UI_RUN_ID="$RUN_ID"
export BMAC_UI_WORKFLOW="$WORKFLOW"
export BMAC_UI_STATE_DIR="$STATE_DIR"
# Children of the script must not try to colorize or page for a terminal.
export TERM=dumb NO_COLOR=1 PAGER=cat SYSTEMD_PAGER=cat GIT_PAGER=cat

bmac_ui_emit_raw "$(bmac_ui_json_obj type protocol protocol "$BMAC_UI_PROTOCOL" \
  version:int "$BMAC_UI_PROTOCOL_VERSION")"
bmac_ui_emit_raw "$(bmac_ui_json_obj type workflow_started workflow "$WORKFLOW" \
  run_id "$RUN_ID" script "$SCRIPT_REL" argv:raw "$(bmac_ui_json_array "$@")" \
  pid:int "$$" started_at "$(date -u +%Y-%m-%dT%H:%M:%SZ)")"

# SIGINT/SIGTERM reach the whole process group. The script handles them
# itself; the runner only records them and waits for the script to finish so
# it can report the outcome.
INTERRUPTED=""
trap 'INTERRUPTED=SIGINT' INT
trap 'INTERRUPTED=SIGTERM' TERM
trap 'INTERRUPTED=SIGHUP' HUP

# The script runs in the foreground: a background job would ignore SIGINT.
bash "$SCRIPT" "$@" </dev/null >&"$STDOUT_FILTER_FD" 2>&"$STDERR_FILTER_FD"
STATUS=$?

# A background process the script left behind can keep a filter's pipe open,
# so waits are bounded.
bounded_wait() {
  local pid="$1" tenths=0
  while kill -0 "$pid" 2>/dev/null && ((tenths < 50)); do
    sleep 0.1
    tenths=$((tenths + 1))
  done
}

# Let the stderr filter drain so its last error line is available.
exec {STDERR_FILTER_FD}>&-
bounded_wait "$STDERR_FILTER_PID"
MESSAGE=""
[[ ! -s "$LAST_ERROR_FILE" ]] || MESSAGE="$(<"$LAST_ERROR_FILE")"

if ((STATUS == 0)); then
  RESULT=success
  MESSAGE=""
elif [[ -e "${STATE_DIR}/declined" ]] || ((STATUS == BMAC_UI_EXIT_CANCELLED)); then
  RESULT=cancelled
  MESSAGE="${MESSAGE:-Cancelled by the operator.}"
elif [[ -n "$INTERRUPTED" ]] || ((STATUS == 130 || STATUS == 143 || STATUS == 129)); then
  RESULT=cancelled
  MESSAGE="Interrupted by ${INTERRUPTED:-a signal}${MESSAGE:+: $MESSAGE}"
else
  RESULT=failed
  MESSAGE="${MESSAGE:-The script exited with status ${STATUS}.}"
fi

bmac_ui_emit_raw "$(bmac_ui_json_obj type completed status "$RESULT" \
  "message?" "$MESSAGE" exit_code:int "$STATUS" \
  finished_at "$(date -u +%Y-%m-%dT%H:%M:%SZ)")"
exec {STDOUT_FILTER_FD}>&-
bounded_wait "$STDOUT_FILTER_PID"
rm -rf -- "$STATE_DIR"
exit "$STATUS"
