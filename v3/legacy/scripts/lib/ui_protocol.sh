#!/usr/bin/env bash

# Copyright (c) 2026 BEENTHERE VENTURES, INC.
# SPDX-License-Identifier: GPL-3.0-only

# Shared helpers for the bmac-ui v2 machine interface ("JSON mode").
#
# Every operator-callable script accepts --json. The script sources this file
# and calls bmac_ui_bootstrap "$@" before parsing its arguments. With --json
# present, bootstrap re-executes the script under scripts/utilities/ui_json_run.sh, which
# emits the protocol header, turns all of the script's ordinary output into
# NDJSON `log` events, reads NDJSON responses, and finishes with exactly one
# `completed` event. Without --json nothing changes: the emitters below are
# no-ops and scripts keep prompting on the terminal as before.
#
# In JSON mode the script's stdin is /dev/null so that no SSH or other child
# can consume protocol responses; responses arrive on BMAC_UI_INPUT_FD. Events
# go to BMAC_UI_EVENT_FD, which shares one pipe with stdout so events and log
# lines stay in order and command substitution never captures an event.
#
# This file deliberately does not enable shell options in its caller.

# shellcheck disable=SC2034 # Read by callers and by ui_json_run.sh.
BMAC_UI_PROTOCOL="bmac-ui"
# shellcheck disable=SC2034
BMAC_UI_PROTOCOL_VERSION=2
BMAC_UI_LIB_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"

# Exit status used when the operator cancels a request from the GUI.
BMAC_UI_EXIT_CANCELLED=3

bmac_ui_is_json() {
  [[ "${BMAC_UI_JSON:-0}" == 1 && "${BMAC_UI_EVENT_FD:-}" =~ ^[0-9]+$ &&
    "${BMAC_UI_INPUT_FD:-}" =~ ^[0-9]+$ && -n "${BMAC_UI_TOKEN:-}" ]]
}

# Re-execute the calling script in JSON mode when --json is among its
# arguments. Call at top level as: bmac_ui_bootstrap "$@"
bmac_ui_bootstrap() {
  local _bui_arg _bui_json=0 _bui_script
  local -a _bui_rest=()
  for _bui_arg in "$@"; do
    if [[ "$_bui_arg" == --json ]]; then
      _bui_json=1
    else
      _bui_rest+=("$_bui_arg")
    fi
  done
  ((_bui_json)) || return 0
  _bui_script="$(cd -- "$(dirname -- "$0")" && pwd -P)/$(basename -- "$0")"
  exec bash "${BMAC_UI_LIB_DIR}/../utilities/ui_json_run.sh" "$_bui_script" "${_bui_rest[@]}"
}

# ---------------------------------------------------------------------------
# JSON encoding. Values are escaped completely; nothing is interpolated raw.

bmac_ui_json_string() {
  local _bui_s="$1" _bui_code _bui_char _bui_hex
  _bui_s="${_bui_s//\\/\\\\}"
  _bui_s="${_bui_s//\"/\\\"}"
  _bui_s="${_bui_s//$'\n'/\\n}"
  _bui_s="${_bui_s//$'\r'/\\r}"
  _bui_s="${_bui_s//$'\t'/\\t}"
  _bui_s="${_bui_s//$'\b'/\\b}"
  _bui_s="${_bui_s//$'\f'/\\f}"
  for ((_bui_code = 1; _bui_code < 32; _bui_code += 1)); do
    printf -v _bui_hex '%02x' "$_bui_code"
    # shellcheck disable=SC2059 # The format is the \xHH escape itself.
    printf -v _bui_char "\\x${_bui_hex}"
    [[ "$_bui_s" == *"$_bui_char"* ]] || continue
    _bui_s="${_bui_s//"$_bui_char"/\\u00${_bui_hex}}"
  done
  _bui_s="${_bui_s//$'\x7f'/\\u007f}"
  printf '"%s"' "$_bui_s"
}

# Print one JSON object from KEY VALUE pairs. A key may carry a suffix:
#   key        string value
#   key:int    integer (null when not an integer)
#   key:num    number (null when not a number)
#   key:bool   true/false (anything other than true/1/yes is false)
#   key:raw    value is already valid JSON
#   key?       (any form) omit the pair entirely when the value is empty
bmac_ui_json_obj() {
  local _bui_out="{" _bui_sep="" _bui_key _bui_value _bui_kind _bui_optional
  while (($# >= 2)); do
    _bui_key="$1" _bui_value="$2"
    shift 2
    _bui_optional=0
    if [[ "$_bui_key" == *'?' ]]; then
      _bui_optional=1
      _bui_key="${_bui_key%'?'}"
    fi
    _bui_kind=string
    if [[ "$_bui_key" == *:* ]]; then
      _bui_kind="${_bui_key##*:}"
      _bui_key="${_bui_key%:*}"
    fi
    ((_bui_optional)) && [[ -z "$_bui_value" ]] && continue
    _bui_out+="${_bui_sep}$(bmac_ui_json_string "$_bui_key"):"
    case "$_bui_kind" in
      int)
        if [[ "$_bui_value" =~ ^-?(0|[1-9][0-9]*)$ ]]; then
          _bui_out+="$_bui_value"
        else
          _bui_out+=null
        fi
        ;;
      num)
        if [[ "$_bui_value" =~ ^-?(0|[1-9][0-9]*)(\.[0-9]+)?([eE][-+]?[0-9]+)?$ ]]; then
          _bui_out+="$_bui_value"
        else
          _bui_out+=null
        fi
        ;;
      bool)
        case "${_bui_value,,}" in
          true | 1 | yes) _bui_out+=true ;;
          *) _bui_out+=false ;;
        esac
        ;;
      raw) _bui_out+="${_bui_value:-null}" ;;
      *) _bui_out+="$(bmac_ui_json_string "$_bui_value")" ;;
    esac
    _bui_sep=","
  done
  printf '%s}' "$_bui_out"
}

# Print a JSON array of strings.
bmac_ui_json_array() {
  local _bui_out="[" _bui_sep="" _bui_item
  for _bui_item in "$@"; do
    _bui_out+="${_bui_sep}$(bmac_ui_json_string "$_bui_item")"
    _bui_sep=","
  done
  printf '%s]' "$_bui_out"
}

# Print a JSON array from items that are already valid JSON.
bmac_ui_json_raw_array() {
  local IFS=,
  printf '[%s]' "$*"
}

# ---------------------------------------------------------------------------
# Emitters. All are no-ops outside JSON mode.

bmac_ui_emit_raw() {
  bmac_ui_is_json || return 0
  printf '\036bmac-ui:%s %s\n' "$BMAC_UI_TOKEN" "$1" >&"$BMAC_UI_EVENT_FD"
}

# bmac_ui_event TYPE [KEY VALUE ...]
bmac_ui_event() {
  bmac_ui_is_json || return 0
  local _bui_type="$1"
  shift
  bmac_ui_emit_raw "$(bmac_ui_json_obj type "$_bui_type" "$@")"
}

bmac_ui_info() {
  bmac_ui_event info message "$*"
}

bmac_ui_warning() {
  bmac_ui_event warning message "$*"
}

# bmac_ui_error CODE MESSAGE [DETAILS]
bmac_ui_error() {
  bmac_ui_event error code "$1" message "$2" "details?" "${3:-}" \
    recoverable:bool false
}

# bmac_ui_phase ID LABEL STATUS [--no-cancel]
# STATUS is pending, running, complete, failed, or skipped.
bmac_ui_phase() {
  bmac_ui_is_json || return 0
  local _bui_cancel=true
  [[ "${4:-}" != --no-cancel ]] || _bui_cancel=false
  bmac_ui_event phase id "$1" label "$2" status "$3" \
    cancel_allowed:bool "$_bui_cancel"
}

# Announce the next sequential step: completes the previous step started by
# this helper (in this shell) and marks the new one running.
BMAC_UI__STEP_ID=""
BMAC_UI__STEP_LABEL=""
BMAC_UI__STEP_COUNT=0
bmac_ui_step() {
  bmac_ui_is_json || return 0
  local _bui_label="$*"
  if [[ -n "$BMAC_UI__STEP_ID" ]]; then
    bmac_ui_phase "$BMAC_UI__STEP_ID" "$BMAC_UI__STEP_LABEL" complete
  fi
  BMAC_UI__STEP_COUNT=$((BMAC_UI__STEP_COUNT + 1))
  BMAC_UI__STEP_ID="step-${BMAC_UI__STEP_COUNT}"
  BMAC_UI__STEP_LABEL="$_bui_label"
  bmac_ui_phase "$BMAC_UI__STEP_ID" "$_bui_label" running
}

# Complete the step that bmac_ui_step last started, if any.
bmac_ui_step_done() {
  bmac_ui_is_json || return 0
  [[ -n "$BMAC_UI__STEP_ID" ]] || return 0
  bmac_ui_phase "$BMAC_UI__STEP_ID" "$BMAC_UI__STEP_LABEL" complete
  BMAC_UI__STEP_ID=""
  BMAC_UI__STEP_LABEL=""
}

# bmac_ui_progress MESSAGE [CURRENT TOTAL UNIT]
bmac_ui_progress() {
  bmac_ui_event progress message "$1" "current:int?" "${2:-}" \
    "total:int?" "${3:-}" "unit?" "${4:-}" "phase?" "${BMAC_UI__STEP_ID:-}"
}

# Plans: bmac_ui_plan_begin TITLE [DESCRIPTION]; bmac_ui_plan_item KIND
# TARGET DESCRIPTION (KIND is create, update, remove, keep, or check);
# bmac_ui_plan_end emits one `plan` event.
declare -ag BMAC_UI__PLAN_ITEMS=()
BMAC_UI__PLAN_TITLE=""
BMAC_UI__PLAN_DESCRIPTION=""
bmac_ui_plan_begin() {
  BMAC_UI__PLAN_TITLE="$1"
  BMAC_UI__PLAN_DESCRIPTION="${2:-}"
  BMAC_UI__PLAN_ITEMS=()
}

bmac_ui_plan_item() {
  BMAC_UI__PLAN_ITEMS+=("$(bmac_ui_json_obj kind "$1" target "$2" description "$3")")
}

bmac_ui_plan_end() {
  bmac_ui_event plan title "$BMAC_UI__PLAN_TITLE" \
    "description?" "$BMAC_UI__PLAN_DESCRIPTION" \
    items:raw "$(bmac_ui_json_raw_array "${BMAC_UI__PLAN_ITEMS[@]}")" \
    dry_run:bool "${BMAC_UI_DRY_RUN:-false}"
  BMAC_UI__PLAN_ITEMS=()
}

# bmac_ui_result_json JSON_OBJECT: structured result data for the GUI.
bmac_ui_result_json() {
  bmac_ui_event result data:raw "$1"
}

# bmac_ui_result KEY VALUE ...: a flat result of strings.
bmac_ui_result() {
  bmac_ui_result_json "$(bmac_ui_json_obj "$@")"
}

# bmac_ui_result_file PATH: emit a result whose data is a JSON file. A file
# that is missing or is not valid JSON emits nothing.
bmac_ui_result_file() {
  bmac_ui_is_json || return 0
  [[ -s "$1" ]] || return 0
  local _bui_data
  _bui_data="$(python3 -c '
import json, sys
print(json.dumps(json.load(open(sys.argv[1], encoding="utf-8")), separators=(",", ":")))
' "$1" 2>/dev/null)" || return 0
  bmac_ui_result_json "$_bui_data"
}

# Suggest something for the operator to do next.
#   bmac_ui_next_step TEXT [--command CMD] [--workflow ID] [--arg NAME=VALUE]...
# --workflow names a dashboard workflow ID and --arg its launch arguments, so
# the dashboard can offer to run it directly.
bmac_ui_next_step() {
  bmac_ui_is_json || return 0
  local _bui_text="$1" _bui_command="" _bui_workflow="" _bui_args_json=""
  local _bui_sep="" _bui_pair
  shift
  while (($#)); do
    case "$1" in
      --command) _bui_command="$2"; shift 2 ;;
      --workflow) _bui_workflow="$2"; shift 2 ;;
      --arg)
        _bui_pair="$2"
        _bui_args_json+="${_bui_sep}$(bmac_ui_json_string "${_bui_pair%%=*}"):$(bmac_ui_json_string "${_bui_pair#*=}")"
        _bui_sep=","
        shift 2
        ;;
      *) shift ;;
    esac
  done
  [[ -z "$_bui_workflow" ]] || _bui_args_json="{${_bui_args_json}}"
  bmac_ui_event next_step text "$_bui_text" "command?" "$_bui_command" \
    "workflow?" "$_bui_workflow" "args:raw?" "$_bui_args_json"
}

# ---------------------------------------------------------------------------
# Requests and responses.

BMAC_UI__REQUEST_COUNT=0
bmac_ui__request_id() {
  BMAC_UI__REQUEST_COUNT=$((BMAC_UI__REQUEST_COUNT + 1))
  printf 'req-%s-%s%s-%s' "$BMAC_UI__REQUEST_COUNT" "$RANDOM" "$RANDOM" \
    "${1//[^A-Za-z0-9_-]/_}"
}

# Record that the operator declined or cancelled, so the runner reports the
# run as cancelled rather than failed when the script then stops. Answering a
# later request clears the record: the script went on after the "no".
bmac_ui__mark_declined() {
  [[ -n "${BMAC_UI_STATE_DIR:-}" && -d "$BMAC_UI_STATE_DIR" ]] || return 0
  : >"${BMAC_UI_STATE_DIR}/declined" 2>/dev/null || true
}

bmac_ui__clear_declined() {
  [[ -n "${BMAC_UI_STATE_DIR:-}" ]] || return 0
  rm -f -- "${BMAC_UI_STATE_DIR}/declined" 2>/dev/null || true
}

bmac_ui__cancelled_exit() {
  bmac_ui__mark_declined
  printf 'ERROR: Cancelled by the operator.\n' >&2
  exit "$BMAC_UI_EXIT_CANCELLED"
}

# bmac_ui__await REQUEST_ID KIND: wait for the response to one request and
# fill BMAC_UI_RESPONSE (an associative array of field values) and
# BMAC_UI_RESPONSE_STATUS (ok, confirmed, declined, acknowledged).
declare -Ag BMAC_UI_RESPONSE=()
BMAC_UI_RESPONSE_STATUS=""
bmac_ui__await() {
  local _bui_id="$1" _bui_kind="$2" _bui_line _bui_index
  local -a _bui_parts=()
  bmac_ui_is_json || {
    printf 'ERROR: bmac-ui requests are only available in JSON mode (%s).\n' "$_bui_id" >&2
    exit 2
  }
  while true; do
    if ! IFS= read -r _bui_line <&"$BMAC_UI_INPUT_FD"; then
      bmac_ui_error input_closed "The controlling program closed the protocol input."
      printf 'ERROR: Input ended while waiting for request %s.\n' "$_bui_id" >&2
      exit 1
    fi
    [[ -n "${_bui_line//[[:space:]]/}" ]] || continue
    mapfile -d '' -t _bui_parts < <(
      python3 "${BMAC_UI_LIB_DIR}/ui_protocol.py" response \
        --request-id "$_bui_id" --kind "$_bui_kind" <<<"$_bui_line"
    )
    case "${_bui_parts[0]:-malformed}" in
      ignore) continue ;;
      cancelled) bmac_ui__cancelled_exit ;;
      malformed)
        bmac_ui_error malformed_response "${_bui_parts[1]:-The response could not be parsed.}"
        continue
        ;;
    esac
    BMAC_UI_RESPONSE_STATUS="${_bui_parts[0]}"
    bmac_ui__clear_declined
    BMAC_UI_RESPONSE=()
    for ((_bui_index = 1; _bui_index + 1 < ${#_bui_parts[@]}; _bui_index += 2)); do
      BMAC_UI_RESPONSE["${_bui_parts[_bui_index]}"]="${_bui_parts[_bui_index + 1]}"
    done
    return 0
  done
}

# Field options shared by bmac_ui_input and bmac_ui_group_add:
#   --id ID --label TEXT [--type TYPE] [--help TEXT] [--default VALUE]
#   [--placeholder TEXT] [--suffix TEXT] [--min N] [--max N]
#   [--min-selected N] [--max-selected N] [--pattern REGEX]
#   [--required] [--sensitive]
#   [--option VALUE LABEL [--option-help TEXT]]...
# TYPE is one of the protocol field types (string, multiline, integer,
# number, boolean, password, select, multiselect, hostname, ip_address, cidr,
# mac_address, path, file, directory, ssh_public_key, duration, bytes).
# The parsed field JSON is left in BMAC_UI__FIELD_JSON and its id in
# BMAC_UI__FIELD_ID.
BMAC_UI__FIELD_JSON=""
BMAC_UI__FIELD_ID=""
BMAC_UI__FIELD_TYPE=""
BMAC_UI__FIELD_TITLE=""
BMAC_UI__FIELD_DESCRIPTION=""
bmac_ui__parse_field() {
  local _bui_id="" _bui_label="" _bui_type=string _bui_help="" _bui_default=""
  local _bui_has_default=0 _bui_placeholder="" _bui_suffix="" _bui_min=""
  local _bui_max="" _bui_min_sel="" _bui_max_sel="" _bui_pattern=""
  local _bui_required=false _bui_sensitive=false _bui_default_json
  local -a _bui_options=()
  local _bui_opt_value="" _bui_opt_label="" _bui_opt_help=""
  BMAC_UI__FIELD_TITLE=""
  BMAC_UI__FIELD_DESCRIPTION=""
  _bui_flush_option() {
    [[ -n "$_bui_opt_value$_bui_opt_label" ]] || return 0
    _bui_options+=("$(bmac_ui_json_obj value "$_bui_opt_value" label "${_bui_opt_label:-$_bui_opt_value}" "help?" "$_bui_opt_help")")
    _bui_opt_value="" _bui_opt_label="" _bui_opt_help=""
  }
  while (($#)); do
    case "$1" in
      --id) _bui_id="$2"; shift 2 ;;
      --label) _bui_label="$2"; shift 2 ;;
      --type) _bui_type="$2"; shift 2 ;;
      --help) _bui_help="$2"; shift 2 ;;
      --default) _bui_default="$2"; _bui_has_default=1; shift 2 ;;
      --placeholder) _bui_placeholder="$2"; shift 2 ;;
      --suffix) _bui_suffix="$2"; shift 2 ;;
      --min) _bui_min="$2"; shift 2 ;;
      --max) _bui_max="$2"; shift 2 ;;
      --min-selected) _bui_min_sel="$2"; shift 2 ;;
      --max-selected) _bui_max_sel="$2"; shift 2 ;;
      --pattern) _bui_pattern="$2"; shift 2 ;;
      --required) _bui_required=true; shift ;;
      --sensitive) _bui_sensitive=true; shift ;;
      --title) BMAC_UI__FIELD_TITLE="$2"; shift 2 ;;
      --description) BMAC_UI__FIELD_DESCRIPTION="$2"; shift 2 ;;
      --option)
        _bui_flush_option
        _bui_opt_value="$2" _bui_opt_label="$3"
        shift 3
        ;;
      --option-help) _bui_opt_help="$2"; shift 2 ;;
      *)
        printf 'ERROR: bmac-ui field option is unknown: %s\n' "$1" >&2
        return 2
        ;;
    esac
  done
  _bui_flush_option
  unset -f _bui_flush_option
  [[ "$_bui_id" =~ ^[A-Za-z0-9_.-]+$ && -n "$_bui_label" ]] || {
    printf 'ERROR: bmac-ui fields need a safe --id and a --label\n' >&2
    return 2
  }
  [[ "$_bui_type" != password ]] || _bui_sensitive=true
  _bui_default_json=""
  if ((_bui_has_default)) && [[ "$_bui_sensitive" != true ]]; then
    case "$_bui_type" in
      integer | number | bytes)
        if [[ "$_bui_default" =~ ^-?[0-9]+(\.[0-9]+)?$ ]]; then
          _bui_default_json="$_bui_default"
        else
          _bui_default_json="$(bmac_ui_json_string "$_bui_default")"
        fi
        ;;
      boolean)
        case "${_bui_default,,}" in
          true | 1 | yes | y) _bui_default_json=true ;;
          *) _bui_default_json=false ;;
        esac
        ;;
      multiselect)
        local -a _bui_defaults=()
        [[ -z "$_bui_default" ]] || IFS=',' read -r -a _bui_defaults <<<"$_bui_default"
        _bui_default_json="$(bmac_ui_json_array "${_bui_defaults[@]}")"
        ;;
      *) _bui_default_json="$(bmac_ui_json_string "$_bui_default")" ;;
    esac
  fi
  BMAC_UI__FIELD_ID="$_bui_id"
  # shellcheck disable=SC2034 # For callers that branch on the field type.
  BMAC_UI__FIELD_TYPE="$_bui_type"
  BMAC_UI__FIELD_JSON="$(bmac_ui_json_obj \
    id "$_bui_id" type "$_bui_type" label "$_bui_label" \
    "help?" "$_bui_help" "default:raw?" "$_bui_default_json" \
    "placeholder?" "$_bui_placeholder" "suffix?" "$_bui_suffix" \
    "min:num?" "$_bui_min" "max:num?" "$_bui_max" \
    "min_selected:int?" "$_bui_min_sel" "max_selected:int?" "$_bui_max_sel" \
    "pattern?" "$_bui_pattern" \
    required:bool "$_bui_required" sensitive:bool "$_bui_sensitive" \
    "options:raw?" "$( ((${#_bui_options[@]})) && bmac_ui_json_raw_array "${_bui_options[@]}")")"
}

# Recent context text shown above a request, typically the menu or listing
# the script just printed for a terminal user.
BMAC_UI_CONTEXT=""

# bmac_ui_input VARIABLE [--title TITLE] [--description TEXT] FIELD-OPTIONS...
# Ask one question and store the answer in VARIABLE. JSON mode only.
bmac_ui_input() {
  local _bui_var="$1" _bui_request
  shift
  bmac_ui__parse_field "$@" || exit 2
  _bui_request="$(bmac_ui__request_id "$BMAC_UI__FIELD_ID")"
  bmac_ui_emit_raw "$(bmac_ui_json_obj type input request_id "$_bui_request" \
    "title?" "$BMAC_UI__FIELD_TITLE" "description?" "$BMAC_UI__FIELD_DESCRIPTION" \
    "context?" "$BMAC_UI_CONTEXT" field:raw "$BMAC_UI__FIELD_JSON")"
  BMAC_UI_CONTEXT=""
  BMAC_UI_LAST_REQUEST_ID="$_bui_request"
  BMAC_UI__LAST_FIELD_ID="$BMAC_UI__FIELD_ID"
  bmac_ui__await "$_bui_request" values
  printf -v "$_bui_var" '%s' "${BMAC_UI_RESPONSE[$BMAC_UI__FIELD_ID]-}"
}

# bmac_ui_reask VARIABLE MESSAGE: reject the answer to the last bmac_ui_input
# with MESSAGE shown beside its field, and wait for a corrected answer.
BMAC_UI__LAST_FIELD_ID=""
bmac_ui_reask() {
  local _bui_var="$1"
  bmac_ui_emit_raw "$(bmac_ui_json_obj type validation_error \
    request_id "$BMAC_UI_LAST_REQUEST_ID" \
    field_errors:raw "{$(bmac_ui_json_string "$BMAC_UI__LAST_FIELD_ID"):$(bmac_ui_json_string "$2")}")"
  bmac_ui__await "$BMAC_UI_LAST_REQUEST_ID" values
  printf -v "$_bui_var" '%s' "${BMAC_UI_RESPONSE[$BMAC_UI__LAST_FIELD_ID]-}"
}

# Input groups. Several related fields are requested together:
#   bmac_ui_group_begin ID TITLE [DESCRIPTION]
#   bmac_ui_group_add VARIABLE FIELD-OPTIONS...
#   while true; do
#     bmac_ui_group_request          # first call emits, later calls re-wait
#     [[ valid ]] || bmac_ui_field_error FIELD_ID "why"
#     bmac_ui_group_check && break   # emits validation_error when needed
#   done
BMAC_UI__GROUP_ID=""
BMAC_UI__GROUP_TITLE=""
BMAC_UI__GROUP_DESCRIPTION=""
BMAC_UI__GROUP_REQUEST=""
BMAC_UI__GROUP_SENT=0
declare -ag BMAC_UI__GROUP_FIELDS=()
declare -ag BMAC_UI__GROUP_VARS=()
declare -ag BMAC_UI__GROUP_IDS=()
declare -Ag BMAC_UI__FIELD_ERRORS=()
BMAC_UI__FORM_ERROR=""
bmac_ui_group_begin() {
  # shellcheck disable=SC2034 # Kept for debugging alongside the request id.
  BMAC_UI__GROUP_ID="$1"
  BMAC_UI__GROUP_TITLE="$2"
  BMAC_UI__GROUP_DESCRIPTION="${3:-}"
  BMAC_UI__GROUP_SENT=0
  BMAC_UI__GROUP_FIELDS=()
  BMAC_UI__GROUP_VARS=()
  BMAC_UI__GROUP_IDS=()
  BMAC_UI__FIELD_ERRORS=()
  BMAC_UI__FORM_ERROR=""
  BMAC_UI__GROUP_REQUEST="$(bmac_ui__request_id "$1")"
}

bmac_ui_group_add() {
  local _bui_var="$1"
  shift
  bmac_ui__parse_field "$@" || exit 2
  BMAC_UI__GROUP_FIELDS+=("$BMAC_UI__FIELD_JSON")
  BMAC_UI__GROUP_VARS+=("$_bui_var")
  BMAC_UI__GROUP_IDS+=("$BMAC_UI__FIELD_ID")
}

bmac_ui_group_request() {
  local _bui_index
  if ((BMAC_UI__GROUP_SENT == 0)); then
    bmac_ui_emit_raw "$(bmac_ui_json_obj type input_group \
      request_id "$BMAC_UI__GROUP_REQUEST" title "$BMAC_UI__GROUP_TITLE" \
      "description?" "$BMAC_UI__GROUP_DESCRIPTION" "context?" "$BMAC_UI_CONTEXT" \
      layout form fields:raw "$(bmac_ui_json_raw_array "${BMAC_UI__GROUP_FIELDS[@]}")")"
    BMAC_UI_CONTEXT=""
    BMAC_UI__GROUP_SENT=1
  fi
  BMAC_UI_LAST_REQUEST_ID="$BMAC_UI__GROUP_REQUEST"
  bmac_ui__await "$BMAC_UI__GROUP_REQUEST" values
  for ((_bui_index = 0; _bui_index < ${#BMAC_UI__GROUP_VARS[@]}; _bui_index += 1)); do
    printf -v "${BMAC_UI__GROUP_VARS[_bui_index]}" '%s' \
      "${BMAC_UI_RESPONSE[${BMAC_UI__GROUP_IDS[_bui_index]}]-}"
  done
}

# bmac_ui_field_error FIELD_ID MESSAGE (FIELD_ID may be empty for a form-level
# message).
bmac_ui_field_error() {
  if [[ -z "$1" ]]; then
    BMAC_UI__FORM_ERROR="$2"
  else
    BMAC_UI__FIELD_ERRORS["$1"]="$2"
  fi
}

# Return 0 when no field errors were recorded since the last response;
# otherwise emit one validation_error for the outstanding request and return 1.
bmac_ui_group_check() {
  local _bui_request="${1:-${BMAC_UI_LAST_REQUEST_ID:-$BMAC_UI__GROUP_REQUEST}}"
  local _bui_errors="{" _bui_sep="" _bui_key
  if ((${#BMAC_UI__FIELD_ERRORS[@]} == 0)) && [[ -z "$BMAC_UI__FORM_ERROR" ]]; then
    return 0
  fi
  for _bui_key in "${!BMAC_UI__FIELD_ERRORS[@]}"; do
    _bui_errors+="${_bui_sep}$(bmac_ui_json_string "$_bui_key"):$(bmac_ui_json_string "${BMAC_UI__FIELD_ERRORS[$_bui_key]}")"
    _bui_sep=","
  done
  _bui_errors+="}"
  bmac_ui_emit_raw "$(bmac_ui_json_obj type validation_error \
    request_id "$_bui_request" field_errors:raw "$_bui_errors" \
    "message?" "$BMAC_UI__FORM_ERROR")"
  BMAC_UI__FIELD_ERRORS=()
  BMAC_UI__FORM_ERROR=""
  return 1
}

# bmac_ui_confirm --title TITLE --message TEXT [--id ID] [--severity S]
#   [--confirm-label L] [--cancel-label L] [--text PHRASE] [--detail LINE]...
#   [--question]
# Return 0 when the operator confirmed and 1 when they declined. With --text
# the dashboard requires that exact phrase to be typed. S is normal, warning,
# destructive, or critical. --question marks a yes/no question whose "no" is
# an ordinary answer rather than a cancellation of the run.
bmac_ui_confirm() {
  local _bui_id=confirm _bui_title="" _bui_message="" _bui_severity=normal
  local _bui_confirm_label="" _bui_cancel_label="" _bui_text="" _bui_request
  local _bui_question=0
  local -a _bui_details=()
  while (($#)); do
    case "$1" in
      --question) _bui_question=1; shift ;;
      --id) _bui_id="$2"; shift 2 ;;
      --title) _bui_title="$2"; shift 2 ;;
      --message) _bui_message="$2"; shift 2 ;;
      --severity) _bui_severity="$2"; shift 2 ;;
      --confirm-label) _bui_confirm_label="$2"; shift 2 ;;
      --cancel-label) _bui_cancel_label="$2"; shift 2 ;;
      --text) _bui_text="$2"; shift 2 ;;
      --detail) _bui_details+=("$2"); shift 2 ;;
      *) shift ;;
    esac
  done
  _bui_request="$(bmac_ui__request_id "$_bui_id")"
  bmac_ui_emit_raw "$(bmac_ui_json_obj type confirm request_id "$_bui_request" \
    title "${_bui_title:-Continue?}" message "$_bui_message" \
    severity "$_bui_severity" "confirm_label?" "$_bui_confirm_label" \
    "cancel_label?" "$_bui_cancel_label" "confirmation_text?" "$_bui_text" \
    "context?" "$BMAC_UI_CONTEXT" \
    "details:raw?" "$( ((${#_bui_details[@]})) && bmac_ui_json_array "${_bui_details[@]}")")"
  BMAC_UI_CONTEXT=""
  bmac_ui__await "$_bui_request" confirm
  if [[ "$BMAC_UI_RESPONSE_STATUS" == confirmed ]]; then
    return 0
  fi
  ((_bui_question)) || bmac_ui__mark_declined
  return 1
}

# bmac_ui_ask PROMPT [YES-LABEL NO-LABEL]: a yes/no question; 0 for yes. A
# "no" that ends the script reports the run as cancelled.
bmac_ui_ask() {
  bmac_ui_confirm --id question --title "$1" --message "" \
    --confirm-label "${2:-Yes}" --cancel-label "${3:-No}"
}

# bmac_ui_confirm_go MESSAGE [SEVERITY]: the JSON-mode form of "Type GO to
# continue"; 0 when confirmed. Without SEVERITY, a MESSAGE that erases,
# destroys, wipes, or permanently removes something is destructive.
bmac_ui_confirm_go() {
  local _bui_severity="${2:-}"
  if [[ -z "$_bui_severity" ]]; then
    _bui_severity=warning
    [[ ! "${1,,}" =~ (erase|destroy|wipe|permanent|irreversib|cannot\ be\ undone) ]] ||
      _bui_severity=destructive
  fi
  bmac_ui_confirm --id go --title "Ready to proceed?" --message "$1" \
    --severity "$_bui_severity" --text GO --confirm-label Proceed
}

# bmac_ui_manual_action --title TITLE --instruction LINE [--copy VALUE]...
#   [--run-script HOST PATH] [--id ID] [--ack-label LABEL]
# Wait until the operator acknowledges that they performed the action.
# --copy gives the preceding instruction a button that copies VALUE, such as
# a path the operator must paste elsewhere. --run-script marks the preceding
# instruction as a script the operator must run by hand on HOST: the dashboard
# emphasizes it, offers to copy PATH, and says the workflow waits on them.
bmac_ui_manual_action() {
  local _bui_id=manual _bui_title="" _bui_ack="" _bui_request _bui_last
  local -a _bui_lines=() _bui_items=()
  while (($#)); do
    case "$1" in
      --id) _bui_id="$2"; shift 2 ;;
      --title) _bui_title="$2"; shift 2 ;;
      --instruction)
        _bui_lines+=("$2")
        _bui_items+=("$(bmac_ui_json_string "$2")")
        shift 2
        ;;
      --copy)
        if ((${#_bui_items[@]})); then
          _bui_last=$((${#_bui_items[@]} - 1))
          _bui_items[_bui_last]="$(bmac_ui_json_obj text "${_bui_lines[_bui_last]}" copy "$2")"
        fi
        shift 2
        ;;
      --run-script)
        if ((${#_bui_items[@]})); then
          _bui_last=$((${#_bui_items[@]} - 1))
          _bui_items[_bui_last]="$(bmac_ui_json_obj text "${_bui_lines[_bui_last]}" copy "${3-}" run_on "${2-}")"
        fi
        shift $(($# < 3 ? $# : 3))
        ;;
      --ack-label) _bui_ack="$2"; shift 2 ;;
      *) shift ;;
    esac
  done
  _bui_request="$(bmac_ui__request_id "$_bui_id")"
  bmac_ui_emit_raw "$(bmac_ui_json_obj type manual_action \
    request_id "$_bui_request" title "${_bui_title:-Manual action required}" \
    instructions:raw "$(bmac_ui_json_raw_array "${_bui_items[@]}")" \
    "acknowledge_label?" "$_bui_ack" "context?" "$BMAC_UI_CONTEXT")"
  BMAC_UI_CONTEXT=""
  bmac_ui__await "$_bui_request" ack
}
# ---------------------------------------------------------------------------
# Shorthands for the common prompt shapes. Like every request helper they are
# for JSON mode only; a script keeps its own terminal prompt for human mode:
#   if bmac_ui_is_json; then bmac_ui_yes_no ...; else read -r -p ...; fi

# bmac_ui_yes_no PROMPT DEFAULT(true|false) [HELP]: 0 for yes, 1 for no.
bmac_ui_yes_no() {
  local _bui_prompt="$1" _bui_default="${2:-false}" _bui_answer
  bmac_ui_input _bui_answer --id answer --type boolean --label "$_bui_prompt" \
    --default "$_bui_default" ${3:+--help "$3"}
  [[ "$_bui_answer" == true ]]
}

# bmac_ui_text VARIABLE PROMPT [DEFAULT] [EXTRA FIELD OPTIONS...]
bmac_ui_text() {
  local _bui_var="$1" _bui_prompt="$2" _bui_default="${3-}"
  shift 2
  (($# == 0)) || shift
  if [[ -n "$_bui_default" ]]; then
    bmac_ui_input "$_bui_var" --id value --label "$_bui_prompt" \
      --default "$_bui_default" "$@"
  else
    bmac_ui_input "$_bui_var" --id value --label "$_bui_prompt" "$@"
  fi
}

# bmac_ui_choose VARIABLE PROMPT DEFAULT VALUE LABEL [VALUE LABEL]...
bmac_ui_choose() {
  local _bui_var="$1" _bui_prompt="$2" _bui_default="$3"
  local -a _bui_opts=()
  shift 3
  while (($# >= 2)); do
    _bui_opts+=(--option "$1" "$2")
    shift 2
  done
  if [[ -n "$_bui_default" ]]; then
    bmac_ui_input "$_bui_var" --id choice --type select --label "$_bui_prompt" \
      --required --default "$_bui_default" "${_bui_opts[@]}"
  else
    bmac_ui_input "$_bui_var" --id choice --type select --label "$_bui_prompt" \
      --required "${_bui_opts[@]}"
  fi
}

# bmac_ui_number_choice VARIABLE PROMPT LABEL...: the JSON-mode form of a
# numbered terminal menu. VARIABLE gets the 1-based number of the chosen
# LABEL; cancelling the request takes the place of answering q.
bmac_ui_number_choice() {
  local _bui_var="$1" _bui_prompt="$2" _bui_index=1 _bui_label
  local -a _bui_opts=()
  shift 2
  for _bui_label in "$@"; do
    _bui_opts+=(--option "$_bui_index" "$_bui_label")
    _bui_index=$((_bui_index + 1))
  done
  bmac_ui_input "$_bui_var" --id choice --type select --label "$_bui_prompt" \
    --required "${_bui_opts[@]}"
}

# bmac_ui_press_enter TITLE [INSTRUCTION]...: a pause that waited for ENTER.
bmac_ui_press_enter() {
  local _bui_title="$1"
  local -a _bui_args=()
  shift
  local _bui_line
  for _bui_line in "$@"; do
    _bui_args+=(--instruction "$_bui_line")
  done
  ((${#_bui_args[@]})) || _bui_args=(--instruction "$_bui_title")
  bmac_ui_manual_action --title "$_bui_title" "${_bui_args[@]}" \
    --ack-label Continue
}
