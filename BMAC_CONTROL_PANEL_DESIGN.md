# BMAC Control Panel Design

## Status

Proposed design for a local desktop GUI for BMAC.

This document is intended to be implementation guidance for a coding agent. It defines the recommended architecture, technology stack, script-to-GUI protocol, user experience, security boundaries, testing strategy, migration plan, and implementation phases.

The primary design goal is to add a graphical control panel without creating a second implementation of BMAC's operational logic.

---

# 1. Executive Summary

BMAC currently exposes its operator-facing workflows as Bash scripts. These scripts are a valuable, tested operational layer:

- They have extensive automated test coverage.
- They have been tested from an x64 Ubuntu administrator workstation.
- The major workflows have also been exercised against real BMAC infrastructure.
- They already encode BMAC's safety checks, state validation, recovery behavior, sequencing, Proxmox operations, ZFS operations, SSH behavior, HA behavior, replication behavior, host management, staging management, and related infrastructure logic.

The BMAC GUI should therefore wrap these scripts rather than reimplementing their behavior in Rust, TypeScript, or another language.

The recommended stack is:

```text
Desktop shell:
  Tauri 2

Native application layer:
  Rust
  Tokio
  serde
  serde_json
  tokio::process::Command

Frontend:
  React
  TypeScript
  Vite
  Tailwind CSS
  shadcn/ui

Terminal / raw output display:
  xterm.js

Operational engine:
  Existing BMAC Bash scripts

Script / GUI interface:
  A standard JSON mode implemented by operator-callable scripts
  NDJSON event stream from script to GUI
  NDJSON response stream from GUI to script
```

The central architectural principle is:

> The GUI must be a second interface to BMAC, not a second implementation of BMAC.

The existing Bash scripts remain authoritative for operational behavior.

---

# 2. Goals

The BMAC Control Panel should:

1. Make BMAC understandable and approachable without requiring the operator to memorize script names, arguments, environment files, or workflow order.
2. Preserve the existing Bash scripts as the authoritative implementation of BMAC workflows.
3. Reuse the existing script test coverage and real-world validation.
4. Provide a consistent GUI around workflows that currently use interactive terminal prompts.
5. Support both individual inputs and groups of related inputs that should be collected together on one GUI form.
6. Provide clear progress information for long-running workflows.
7. Expose detailed script output for advanced operators without forcing ordinary users to read terminal output.
8. Make destructive operations visually distinct and difficult to trigger accidentally.
9. Support dry-run / preview workflows where scripts support them.
10. Keep secrets out of the webview whenever possible.
11. Avoid introducing a required network service, API daemon, or Internet-facing management endpoint.
12. Preserve direct CLI use of every script.
13. Keep Linux as the primary supported operator platform while allowing workflows already compatible with macOS to remain usable there.
14. Make the JSON protocol useful for future automation consumers in addition to the GUI.

---

# 3. Non-Goals

The first version of the Control Panel should not:

- Rewrite BMAC workflows in Rust.
- Rewrite BMAC workflows in TypeScript.
- Replace the existing CLI.
- Require an Axum, Hyper, or other HTTP server.
- Expose a general-purpose remote management API.
- Provide arbitrary shell execution from the frontend.
- Move BMAC secrets into browser-local storage or React state unnecessarily.
- Attempt to duplicate Proxmox, ZFS, Corosync, HA, SSH, replication, or registry logic in the GUI.
- Infer workflow success by scraping human-readable terminal text.
- Depend on regex matching of existing interactive prompts.
- Hide the underlying script output from advanced users.
- Make the desktop GUI a prerequisite for using BMAC.

---

# 4. Why a Desktop Application

BMAC's operator model is already workstation-oriented. The administrator runs BMAC from a workstation and reaches Proxmox hosts and guests using SSH and other existing mechanisms.

A local desktop control panel fits this model better than a local or remote web service.

Advantages include:

- No new cluster-side daemon.
- No additional management port.
- No additional authentication service.
- No browser session lifecycle.
- No CSRF concerns.
- No need to expose a management API.
- Direct local process supervision.
- Direct access to the BMAC checkout and its configuration.
- Straightforward streaming of subprocess output.
- Straightforward OS file dialogs.
- Better handling of long-running workflows.
- Better integration with desktop notifications later if desired.

Tauri provides a small native application shell while allowing the user interface to be implemented using normal web technologies.

---

# 5. Recommended Technology Stack

## 5.1 Tauri 2

Use Tauri 2 as the desktop application framework.

Responsibilities:

- Application lifecycle.
- Native application packaging.
- Native window management.
- Secure bridge between React and Rust.
- OS dialogs and other native integrations.
- Event delivery from Rust to the frontend.

Do not embed a local HTTP API merely to communicate between the frontend and Rust. Prefer Tauri commands and events.

## 5.2 Rust

Use Rust for the trusted native layer.

Recommended libraries:

```text
tokio
serde
serde_json
thiserror or anyhow
uuid
chrono
```

Potential additional libraries should be added only when needed.

The Rust layer should be intentionally thin. Its primary responsibilities are:

- Discover the BMAC repository.
- Validate supported platform prerequisites.
- Spawn scripts.
- Pass arguments without invoking a shell.
- Provide JSON-mode stdin.
- Read JSON-mode stdout.
- Capture stderr.
- Stream events to React.
- Track workflow process state.
- Handle cancellation.
- Prevent multiple conflicting operations where required.
- Enforce the allowed-script boundary.
- Keep secrets and privileged filesystem access outside the webview.
- Maintain a local run history if this feature is implemented.

Rust should not duplicate workflow business logic.

## 5.3 Tokio

Use Tokio for asynchronous process management.

Important functionality includes:

- `tokio::process::Command`
- Asynchronous stdout reads.
- Asynchronous stderr reads.
- Asynchronous stdin writes.
- Cancellation handling.
- Multiple concurrent read-only operations where safe.
- Long-running process supervision without blocking the UI.

## 5.4 React and TypeScript

Use React and TypeScript for the frontend.

Recommended frontend stack:

```text
React
TypeScript
Vite
Tailwind CSS
shadcn/ui
```

Reasons:

- Familiar ecosystem.
- Excellent form support.
- Strong component ecosystem.
- Good fit for dashboards and administrative applications.
- Easy display of structured workflow state.
- Easy mapping of protocol-defined fields to UI controls.

The React layer is presentation and interaction logic. It must not become the source of truth for BMAC operational rules.

## 5.5 xterm.js

Use xterm.js for optional detailed output.

Every running or completed workflow should be able to expose a "Details" or "Console" area showing the detailed output associated with the script execution.

The primary GUI should present structured state, progress, warnings, forms, and results. The terminal view is the operator's detailed escape hatch.

## 5.6 Bash

Bash remains the workflow engine.

The existing operator-callable scripts continue to own:

- Validation.
- State checks.
- SSH execution.
- Proxmox commands.
- ZFS operations.
- Replication.
- HA configuration.
- Registry updates.
- Host membership.
- QDevice handling.
- Guest creation and removal.
- Staging management.
- Disk workflows.
- Recovery logic.
- Any other operational behavior currently implemented by BMAC.

---

# 6. High-Level Architecture

```text
+----------------------------------------------------------+
|                    BMAC Control Panel                    |
|                                                          |
|  React + TypeScript                                      |
|  ------------------------------------------------------  |
|  Dashboard                                               |
|  Hosts                                                   |
|  Production VMs                                          |
|  Staging VMs                                             |
|  Storage                                                 |
|  QDevice                                                 |
|  Diagnostics                                             |
|  Operations                                              |
|  Settings                                                |
|                                                          |
+---------------------------+------------------------------+
                            |
                            | Tauri commands / events
                            v
+----------------------------------------------------------+
|                       Tauri / Rust                       |
|                                                          |
|  Workflow registry                                       |
|  Script launcher                                         |
|  Process supervisor                                      |
|  NDJSON parser                                           |
|  NDJSON stdin writer                                     |
|  stdout / stderr capture                                 |
|  Cancellation                                            |
|  Platform validation                                     |
|  Run state                                               |
|  Security boundary                                       |
|                                                          |
+---------------------------+------------------------------+
                            |
                            | argv + stdin/stdout/stderr
                            v
+----------------------------------------------------------+
|                  Existing BMAC Scripts                   |
|                                                          |
|  hosts/                                                  |
|  guests/prod/                                            |
|  guests/staging/                                         |
|  qdevice/                                                |
|  diagnostics/                                            |
|  app/                                                    |
|  other operator workflows                                |
|                                                          |
+---------------------------+------------------------------+
                            |
                            v
+----------------------------------------------------------+
|              Proxmox / ZFS / SSH / HA / etc.            |
+----------------------------------------------------------+
```

---

# 7. Repository Organization

Do not overload the existing `app/` directory if that directory already represents application deployment workflows.

A dedicated GUI directory is clearer.

Recommended layout:

```text
bmac/
  control-panel/
    package.json
    vite.config.ts
    tsconfig.json

    src/
      app/
      components/
      features/
        dashboard/
        hosts/
        production/
        staging/
        storage/
        qdevice/
        diagnostics/
        operations/
        settings/
      protocol/
      hooks/
      lib/
      types/

    src-tauri/
      Cargo.toml
      tauri.conf.json
      src/
        main.rs
        commands/
        workflow/
          mod.rs
          registry.rs
          process.rs
          protocol.rs
          validation.rs
        platform/
        history/
        errors.rs

  hosts/
  guests/
  qdevice/
  diagnostics/
  lib/
  env/
  ...
```

The Control Panel may live in the same BMAC repository so that the packaged application can understand the repository layout and development can evolve the scripts and GUI together.

A future release process may choose either:

1. Ship the GUI inside the normal BMAC release archive.
2. Produce separate native GUI packages that contain or locate the matching BMAC scripts.

Do not solve packaging prematurely. First establish the application and protocol architecture.

---

# 8. Script Interface Design

## 8.1 Preserve Existing CLI Behavior

Existing script invocation must continue to work.

Example:

```bash
./guests/prod/add_prod_vm.sh
```

The GUI adds a new machine-readable mode rather than replacing the existing human-readable mode.

Recommended standard option:

```bash
--json
```

Example:

```bash
./guests/prod/add_prod_vm.sh --json
```

`--json` means that the script is being controlled by another program.

Human-readable interactive behavior remains the default when `--json` is absent.

## 8.2 JSON Mode Uses NDJSON

Use newline-delimited JSON, also called NDJSON or JSON Lines.

Each line written to stdout is exactly one complete JSON object.

Example:

```json
{"type":"protocol","protocol":"bmac-ui","version":1}
{"type":"workflow_started","workflow":"add_prod_vm","run_id":"550e8400-e29b-41d4-a716-446655440000"}
{"type":"progress","message":"Validating cluster state"}
{"type":"progress","message":"Selecting placement"}
{"type":"completed","status":"success"}
```

Do not emit one giant JSON object at the end of a workflow.

BMAC workflows can be long-running and interactive. The GUI needs events as they happen.

## 8.3 stdout Contract

In JSON mode:

- stdout is reserved for protocol events.
- Each stdout line must be valid JSON.
- Each stdout line must contain exactly one event.
- No banners, shell tracing, progress bars, ANSI escape sequences, or human-only output should appear directly on stdout outside a JSON event.

If the script wants to expose raw command output to the GUI, wrap that output in a protocol event.

Example:

```json
{"type":"log","stream":"stdout","text":"NAME STATE READ WRITE CKSUM"}
```

## 8.4 stderr Contract

stderr may be captured separately by Rust for debugging and unexpected failures.

Prefer emitting expected, user-relevant errors as protocol events before exiting.

Example:

```json
{"type":"error","code":"cluster_not_quorate","message":"The cluster is not quorate.","recoverable":false}
```

Unexpected shell or command errors that escape the protocol should still appear in captured stderr so the operator can inspect them.

## 8.5 stdin Contract

In JSON mode, the GUI responds to requests by writing one NDJSON object per line to the script's stdin.

Example:

```json
{"type":"response","request_id":"req-17","values":{"memory_gib":32,"cores":8}}
```

The script must never assume a human terminal is attached when `--json` is active.

## 8.6 Protocol Version

Every JSON-mode workflow must begin with a protocol event.

Example:

```json
{
  "type": "protocol",
  "protocol": "bmac-ui",
  "version": 1
}
```

The GUI must reject unsupported major protocol versions cleanly.

This enables the script protocol to evolve without silently misinterpreting events.

---

# 9. Input Requests

Inputs are a first-class part of the protocol.

A script may request:

- A single field.
- A group of related fields.
- A single selection.
- A multiple selection.
- A confirmation.
- A destructive confirmation.
- A secret value.
- A file.
- A directory.
- A manual action acknowledgment.

The script defines what information it needs. The GUI determines the appropriate visual controls.

---

# 10. Batched Input Groups

## 10.1 Requirement

Some BMAC workflows ask for several values in sequence.

The GUI must not force the user through a series of one-field popup dialogs when those inputs form one logical step.

The protocol therefore supports an `input_group` request containing an array of fields.

Example:

```json
{
  "type": "input_group",
  "request_id": "req-prod-resources",
  "title": "Production VM Resources",
  "description": "Choose the initial resources for the new production VM.",
  "fields": [
    {
      "id": "cores",
      "type": "integer",
      "label": "CPU cores",
      "required": true,
      "min": 1,
      "default": 8
    },
    {
      "id": "memory_gib",
      "type": "integer",
      "label": "Memory",
      "suffix": "GiB",
      "required": true,
      "min": 1,
      "default": 32
    },
    {
      "id": "disk_gib",
      "type": "integer",
      "label": "Disk capacity",
      "suffix": "GiB",
      "required": true,
      "min": 16,
      "default": 500
    }
  ]
}
```

The GUI should render this as one form:

```text
Production VM Resources

Choose the initial resources for the new production VM.

CPU cores        [  8 ]
Memory           [ 32 ] GiB
Disk capacity    [500 ] GiB

                         [Cancel] [Continue]
```

The response is one object:

```json
{
  "type": "response",
  "request_id": "req-prod-resources",
  "values": {
    "cores": 8,
    "memory_gib": 32,
    "disk_gib": 500
  }
}
```

## 10.2 Grouping Rule

Fields should be batched when they belong to one conceptual decision or setup step.

Good examples:

- VM CPU, RAM, and disk size.
- Host network settings.
- A set of disk selections.
- Cloudflare certificate and key paths.
- Placement hosts plus ownership preference.
- Several related configuration values.

Do not create huge forms merely because batching is possible.

A workflow should still be broken into meaningful steps when later questions depend on earlier answers or when a checkpoint is useful to the user.

## 10.3 Scripts Remain Authoritative

The GUI may perform obvious immediate validation such as checking that an integer field contains an integer.

The script remains authoritative for semantic validation.

For example, the GUI may verify that `memory_gib` is a positive integer, but the script decides whether that amount of RAM is valid for the selected host placement.

If validation fails, the script emits a validation event.

Example:

```json
{
  "type": "validation_error",
  "request_id": "req-prod-resources",
  "field_errors": {
    "memory_gib": "Selected placement does not have sufficient available memory.",
    "disk_gib": "Disk capacity must be at least 64 GiB."
  }
}
```

The GUI redisplays the same form and associates the errors with the affected fields.

---

# 11. Field Schema

A standard field schema should support common BMAC input types.

Recommended base shape:

```json
{
  "id": "field_name",
  "type": "string",
  "label": "Human-readable label",
  "help": "Optional explanatory text.",
  "required": true,
  "default": null,
  "sensitive": false,
  "disabled": false
}
```

Recommended field types:

```text
string
multiline
integer
number
boolean
password
select
multiselect
hostname
ip_address
cidr
mac_address
path
file
directory
ssh_public_key
duration
bytes
```

Not every type requires a custom React component immediately. Types can initially map onto a smaller set of controls.

The type still has value because it expresses intent and allows richer controls later.

## 11.1 Select Field

Example:

```json
{
  "id": "owner",
  "type": "select",
  "label": "Production owner",
  "required": true,
  "options": [
    {"value":"mox1","label":"mox1"},
    {"value":"mox2","label":"mox2"}
  ]
}
```

## 11.2 Multiselect Field

Example:

```json
{
  "id": "placement_hosts",
  "type": "multiselect",
  "label": "Placement hosts",
  "required": true,
  "min_selected": 2,
  "options": [
    {"value":"mox1","label":"mox1"},
    {"value":"mox2","label":"mox2"},
    {"value":"mox3","label":"mox3"}
  ]
}
```

## 11.3 File Field

Example:

```json
{
  "id": "iso_path",
  "type": "file",
  "label": "Proxmox installer ISO",
  "required": true,
  "filters": [
    {"name":"ISO images","extensions":["iso"]}
  ]
}
```

The GUI should use an OS-native file chooser.

## 11.4 Secret Field

Example:

```json
{
  "id": "tailscale_auth_key",
  "type": "password",
  "label": "Tailscale auth key",
  "required": true,
  "sensitive": true
}
```

Sensitive values must not be:

- Written to normal logs.
- Included in run-history detail.
- Reflected back in later events.
- Stored in React persistent storage.
- Sent to analytics or telemetry.

---

# 12. Protocol Event Types

Keep the protocol small and stable.

Recommended initial event set:

```text
protocol
workflow_started
workflow_metadata
input
input_group
confirm
manual_action
progress
phase
plan
info
warning
error
validation_error
log
result
completed
```

More types should be added only when a real workflow requires them.

---

# 13. Workflow Metadata

Scripts may emit metadata early in the run.

Example:

```json
{
  "type": "workflow_metadata",
  "workflow": "remove_prod_vm",
  "title": "Remove Production VM",
  "description": "Remove a production VM and its BMAC-managed resources.",
  "category": "production",
  "destructive": true,
  "supports_dry_run": true,
  "supports_cancel": true
}
```

This metadata may also be represented in a static Rust workflow registry where appropriate.

Do not make the GUI depend on discovering all workflows dynamically in version 1. A curated workflow registry is safer and easier to reason about.

---

# 14. Single Input Request

For a simple one-off question, use `input`.

Example:

```json
{
  "type": "input",
  "request_id": "req-prod-name",
  "title": "Production VM",
  "field": {
    "id": "prod_name",
    "type": "string",
    "label": "Production VM name",
    "required": true
  }
}
```

The GUI may render this inline, in a dialog, or as part of the current workflow panel.

Do not require every input to appear as a modal popup.

---

# 15. Confirmation Requests

Use a dedicated confirmation event instead of representing confirmation as a boolean input field.

Example:

```json
{
  "type": "confirm",
  "request_id": "req-confirm",
  "title": "Create production VM?",
  "message": "BMAC is ready to create prod2.",
  "severity": "normal",
  "confirm_label": "Create prod2",
  "cancel_label": "Cancel"
}
```

Response:

```json
{
  "type": "response",
  "request_id": "req-confirm",
  "confirmed": true
}
```

Recommended severities:

```text
normal
warning
destructive
critical
```

---

# 16. Destructive Confirmations

Destructive operations require stronger visual treatment.

Example:

```json
{
  "type": "confirm",
  "request_id": "req-delete-prod1",
  "title": "Remove prod1?",
  "message": "This operation removes the production VM and BMAC-managed resources associated with it.",
  "severity": "destructive",
  "confirm_label": "Remove prod1",
  "confirmation_text": "prod1"
}
```

If `confirmation_text` is present, the GUI requires the operator to type that exact text before enabling the destructive action.

The script should still perform its own final safety checks.

---

# 17. Manual Action Requests

Some infrastructure workflows cannot be fully automated and require the operator to perform a physical or external action.

Examples:

- Boot a host.
- Verify a VLAN.
- Enter a LUKS password at the physical/virtual console.
- Move a cable.
- Replace a failed disk.
- Confirm an iDRAC operation.
- Complete an external provider step.

Represent these explicitly.

Example:

```json
{
  "type": "manual_action",
  "request_id": "req-vlan-check",
  "title": "Verify private network",
  "instructions": [
    "Confirm that the private VLAN is configured on the provider side.",
    "Confirm that the host can reach the other cluster hosts on the private network."
  ],
  "acknowledge_label": "Verified - Continue"
}
```

Response:

```json
{
  "type": "response",
  "request_id": "req-vlan-check",
  "acknowledged": true
}
```

---

# 18. Progress and Phases

Long-running workflows should expose meaningful phases.

Example:

```json
{"type":"phase","id":"validate","label":"Validate cluster","status":"running"}
{"type":"phase","id":"validate","label":"Validate cluster","status":"complete"}
{"type":"phase","id":"create_vm","label":"Create VM","status":"running"}
{"type":"progress","phase":"create_vm","message":"Creating VM 101"}
{"type":"phase","id":"install_os","label":"Install Ubuntu","status":"running"}
```

The GUI may render:

```text
Create Production VM

[done] Validate cluster
[done] Reserve production slot
[....] Create VM
[    ] Install Ubuntu
[    ] Configure guest
[    ] Configure replication
[    ] Configure HA
[    ] Publish routing
```

Do not fabricate percentage progress unless the script can genuinely provide it.

A spinner plus phases is better than fake percentages.

If a script can provide real progress, it may include:

```json
{
  "type": "progress",
  "phase": "replication",
  "current": 734003200,
  "total": 2147483648,
  "unit": "bytes",
  "message": "Initial replication"
}
```

---

# 19. Plans and Dry-Run

BMAC already uses dry-run behavior in multiple workflows. The GUI should make this a major safety and usability feature.

Where supported, the preferred interaction is:

```text
collect inputs
  ->
script validates inputs
  ->
script produces plan / dry-run
  ->
GUI renders plan
  ->
operator confirms
  ->
script performs real operation
```

A `plan` event should be structured.

Example:

```json
{
  "type": "plan",
  "title": "Remove prod1",
  "items": [
    {
      "kind": "remove",
      "target": "VM 100",
      "description": "Remove production VM"
    },
    {
      "kind": "remove",
      "target": "HA resource",
      "description": "Remove HA configuration"
    },
    {
      "kind": "remove",
      "target": "replication jobs",
      "description": "Remove replication jobs"
    },
    {
      "kind": "update",
      "target": "BMAC registry",
      "description": "Release production allocation"
    }
  ]
}
```

The GUI should not parse a dry-run's human-readable text to create the plan.

If an existing script already implements `--dry-run`, its JSON mode should expose equivalent information through structured events.

---

# 20. Logging

Structured GUI events and raw detailed output are different concerns.

Use `log` events for detailed output.

Example:

```json
{
  "type": "log",
  "level": "info",
  "source": "zfs",
  "text": "sending from @rep_BMAC_..."
}
```

Supported levels may initially be:

```text
debug
info
warning
error
```

The GUI can:

- Show normal structured workflow information prominently.
- Store detailed logs for the run.
- Display detailed logs in the expandable xterm.js console.
- Allow copy-to-clipboard.
- Optionally allow save-to-file.

Never include sensitive values in logs.

---

# 21. Error Handling

Errors should have stable machine-readable codes when practical.

Example:

```json
{
  "type": "error",
  "code": "cluster_not_quorate",
  "message": "The Proxmox cluster is not quorate.",
  "details": "Expected quorum before modifying HA configuration.",
  "recoverable": false
}
```

A recoverable error might be:

```json
{
  "type": "validation_error",
  "request_id": "req-placement",
  "field_errors": {
    "placement_hosts": "At least two eligible placement hosts are required."
  }
}
```

The GUI must not infer success solely from process exit code.

Recommended rule:

A successful JSON workflow should:

1. Emit a `completed` event with `status: "success"`.
2. Exit with status 0.

A failed workflow should:

1. Emit an `error` event when possible.
2. Emit `completed` with `status: "failed"` when possible.
3. Exit non-zero.

If the process dies without a completion event, Rust should classify the run as an unexpected process failure.

---

# 22. Suggested Exit Code Convention

Do not force immediate conversion of all scripts if they already use meaningful codes, but new JSON-aware workflow code should converge on a simple convention.

Suggested baseline:

```text
0   success
1   workflow failure
2   invalid invocation / arguments
3   user cancelled
4   prerequisite failure
5   validation failure
```

Machine-readable event codes are more important than creating a large exit-code taxonomy.

---

# 23. Cancellation

The GUI should support cancellation only where the script declares it safe.

A workflow may emit:

```json
{
  "type": "workflow_metadata",
  "supports_cancel": true
}
```

Recommended Rust cancellation sequence:

1. Mark the UI as "Cancelling".
2. Send SIGINT to the workflow process or process group.
3. Give the script an opportunity to perform cleanup.
4. If the process does not exit and escalation is safe, use a stronger termination mechanism.
5. Record whether cancellation was clean or forced.

Scripts with critical non-interruptible phases should be able to emit:

```json
{
  "type": "phase",
  "id": "zfs_receive_finalize",
  "label": "Finalizing ZFS receive",
  "status": "running",
  "cancel_allowed": false
}
```

The GUI should temporarily disable its Cancel button.

Do not assume arbitrary process termination is safe for infrastructure operations.

---

# 24. Non-Interactive Arguments

The JSON protocol solves interactive communication, but scripts should also gradually gain explicit non-interactive arguments where doing so makes sense.

Example:

```bash
./guests/prod/add_prod_vm.sh \
  --non-interactive \
  --placement mox1,mox2 \
  --memory-gib 32 \
  --cores 8 \
  --disk-gib 500
```

This is useful for:

- Tests.
- Automation.
- CI.
- Future agents.
- Reproducibility.
- Debugging outside the GUI.

However, the GUI does not need to translate every interaction into command-line flags before the protocol is usable.

The initial GUI can communicate through JSON stdin after launching a script with `--json`.

Over time, reusable script logic should be structured so both human prompting and machine-driven input feed the same underlying workflow functions.

---

# 25. Recommended Script Internal Structure

Where practical, evolve scripts toward this conceptual structure:

```text
main
  |
  +-- parse command-line options
  |
  +-- initialize interaction mode
  |
  +-- collect required inputs
  |     |
  |     +-- human prompt implementation
  |     |
  |     +-- JSON request implementation
  |
  +-- validate inputs
  |
  +-- execute workflow implementation
  |
  +-- emit result
```

Do not maintain separate human and GUI workflow implementations.

Preferred:

```text
human prompts -----+
                   |
                   v
              workflow data
                   |
JSON responses ----+
                   |
                   v
          same workflow functions
```

---

# 26. Shared Bash Protocol Library

Create a shared Bash library for JSON-mode behavior rather than hand-writing protocol formatting in every script.

Suggested file:

```text
lib/ui_protocol.sh
```

Potential responsibilities:

```text
bmac_ui_init
bmac_ui_is_json
bmac_ui_emit
bmac_ui_info
bmac_ui_warning
bmac_ui_error
bmac_ui_log
bmac_ui_phase
bmac_ui_progress
bmac_ui_request_input
bmac_ui_request_input_group
bmac_ui_request_confirm
bmac_ui_request_manual_action
bmac_ui_read_response
bmac_ui_validation_error
bmac_ui_complete
```

Use a reliable JSON encoder.

Do not construct JSON by unsafe string concatenation such as:

```bash
echo '{"message":"'"$value"'"}'
```

Values can contain quotes, backslashes, newlines, and other characters.

If BMAC already has an available JSON utility in its supported environment, use it consistently. Otherwise choose one explicit supported dependency and document it.

The protocol library should be heavily unit tested.

---

# 27. Workflow Registry in Rust

Do not expose an arbitrary "run any file" capability to React.

Rust should maintain an allowlisted workflow registry.

Conceptual example:

```rust
WorkflowDefinition {
    id: "add_prod_vm",
    category: "production",
    script: "guests/prod/add_prod_vm.sh",
    platforms: ["linux", "macos"],
    destructive: false,
}
```

Another example:

```rust
WorkflowDefinition {
    id: "add_proxmox_host",
    category: "hosts",
    script: "hosts/add_proxmox_host.sh",
    platforms: ["linux"],
    destructive: true,
}
```

The React frontend asks Rust to run:

```text
add_prod_vm
```

It must not send:

```text
../../whatever.sh
```

or:

```text
bash -c "arbitrary command"
```

---

# 28. Never Use `sh -c` for GUI-Supplied Commands

Bad architecture:

```rust
Command::new("sh")
    .arg("-c")
    .arg(user_supplied_string)
```

Preferred architecture:

```rust
Command::new(script_path)
    .arg("--json")
    .arg("--some-option")
    .arg(validated_value)
```

Arguments must be passed as separate argv values.

This prevents quoting errors and command injection.

The GUI must never expose a generic arbitrary shell command API.

---

# 29. Secrets Boundary

BMAC secrets should remain outside the React webview whenever possible.

Rules:

- Do not load the full contents of `env/secrets.env` into React.
- Do not store secrets in browser localStorage.
- Do not store secrets in sessionStorage.
- Do not add secrets to normal logs.
- Do not include secret values in workflow history.
- Do not echo secret input values back to the frontend after submission.
- Do not expose a general filesystem API to React.

If the GUI needs to edit secrets in a later phase, implement a narrowly scoped Rust command that reads or writes only the required configuration with explicit semantics.

Where the existing scripts can consume `env/secrets.env` directly, prefer leaving that behavior unchanged.

---

# 30. Platform Awareness

The GUI should detect the local platform.

At minimum:

```text
Linux x86_64
Linux other architecture
macOS x86_64
macOS arm64
unsupported
```

Workflows can declare platform restrictions.

Example UI:

```text
Add Proxmox Host

Unavailable on this workstation.

This workflow requires a Debian-based Linux x86_64 workstation because
BMAC must build a customized Proxmox installer ISO.
```

Do not hide unavailable workflows entirely. Showing them disabled helps users understand BMAC's capabilities and why an operation is unavailable.

---

# 31. Main Navigation

Recommended top-level navigation:

```text
Dashboard
Hosts
Production
Staging
Storage
QDevice
Diagnostics
Operations
Settings
```

This maps infrastructure concepts to user intent instead of making users navigate by script filename.

---

# 32. Dashboard

The Dashboard should answer:

- Is my cluster healthy?
- Is quorum healthy?
- Is the QDevice healthy?
- Which hosts are online?
- Where is each production VM running?
- Are replicas current?
- Is HA configured?
- Are staging VMs present?
- Is storage healthy?
- Are there active or failed operations?

Possible layout:

```text
BMAC
------------------------------------------------------------

Cluster      Healthy
Quorum       Healthy
QDevice      Online
Replication  Healthy

Hosts
------------------------------------------------------------
mox1   Healthy   prod1 owner     rpool ...
mox2   Healthy   standby         rpool ...
mox3   Healthy   prod2 owner     rpool ...

Production
------------------------------------------------------------
prod1  Running  mox1  HA OK  Replication OK
prod2  Running  mox3  HA OK  Replication OK

Staging
------------------------------------------------------------
stage1prod1  Running  mox2

Recent Operations
------------------------------------------------------------
Add staging VM         Success
Force replication      Success
Cluster health check   Success
```

The Dashboard should obtain state from existing BMAC diagnostic logic, not reproduce the diagnostic queries in React.

Where necessary, add JSON output to diagnostic scripts such as cluster state, host state, production VM state, QDevice state, and disk inventory workflows.

---

# 33. Hosts Page

The Hosts page should show registered cluster hosts and relevant state.

Possible actions:

- Add Proxmox host.
- Remove healthy host.
- Purge permanently dead host.
- Update cluster runtime.
- Inspect host state.
- Inspect disks.
- Perform supported disk workflows.
- Change control node where supported.

A host detail view may contain:

```text
Overview
Networking
Storage
Cluster
Guests
Diagnostics
Operations
```

Do not directly modify host state from React. Every change maps to an existing or explicitly created BMAC workflow script.

---

# 34. Production Page

Show all production VMs and important state.

Possible actions:

- Add production VM.
- Remove production VM.
- Change placement.
- Change owner / active location where supported.
- Extend disk.
- Force replication.
- Inspect VM state.
- Open relevant Proxmox UI link.
- Open SSH instructions or invoke an existing supported connection action later.

A production VM detail page should make current placement and replication topology obvious.

Example:

```text
prod1
------------------------------------------------------------
VMID              100
State             Running
Current host      mox1
Placement         mox1, mox2
HA                Healthy
Latest replica    mox2 - current
```

---

# 35. Staging Page

Staging should emphasize the relationship between a staging VM and its production source.

Possible actions:

- Add staging VM.
- Remove staging VM.
- Inspect staging state.

Example:

```text
stage1prod1
------------------------------------------------------------
Source production VM    prod1
Host                    mox2
State                   Running
Storage                  Linked clone
HTTPS                    Available
```

---

# 36. Storage Page

Storage workflows can be particularly dangerous and deserve a dedicated area.

Possible content:

- ZFS pool health.
- Vdev layout.
- Physical disk inventory.
- Capacity.
- Replication storage state.
- Disk expansion workflows.
- Vdev retirement / decommission workflows.
- Disk replacement workflows if present.

For any operation that could destroy data, require an explicit destructive confirmation and show the script-generated plan.

---

# 37. QDevice Page

Display:

- Configured QDevice.
- Reachability.
- Voting status.
- Expected votes.
- Current quorum state.
- Health.
- Replacement/removal state where appropriate.

Possible actions:

- Add QDevice.
- Gracefully remove active QDevice.
- Purge / evict failed QDevice.
- Replace QDevice using existing workflows.

The GUI should clearly distinguish removal of a reachable device from eviction of a permanently failed device if BMAC currently supports both cases.

---

# 38. Diagnostics Page

Diagnostics should collect read-only tools in one place.

Examples based on current BMAC concepts include:

- Cluster health.
- Cluster state.
- Proxmox host state.
- Production VM state.
- QDevice state.
- Disk inventory.

Read-only diagnostics are a good early target for JSON support because they are low risk and immediately useful for the Dashboard.

---

# 39. Operations Page

Every script invocation initiated through the GUI should become an operation record.

Possible columns:

```text
Time
Workflow
Target
Status
Duration
```

Statuses:

```text
Running
Waiting for input
Waiting for manual action
Succeeded
Failed
Cancelled
Interrupted
```

Selecting an operation should show:

```text
Summary
Timeline
Inputs, with sensitive values redacted
Plan
Structured events
Detailed log
Result
Error details
```

Do not imply that operation history is an audit-grade immutable log unless such guarantees are deliberately implemented.

For version 1, local history is primarily a usability and debugging feature.

---

# 40. Workflow UI Model

Avoid representing every script as a modal dialog.

A better workflow UI is a persistent task panel or workflow page.

Example:

```text
Add Production VM
------------------------------------------------------------

Step 1 of 4 - Placement

[form fields]

                                             [Continue]
```

After submission:

```text
Step 2 of 4 - Resources

CPU cores       [8]
Memory          [32] GiB
Disk            [500] GiB

                                 [Back] [Continue]
```

Then:

```text
Review

Production VM: prod2
Placement:     mox1, mox2
CPU:           8 cores
Memory:        32 GiB
Disk:          500 GiB

[script-generated plan]

                                  [Cancel] [Create prod2]
```

During execution:

```text
Create prod2

[done] Validate cluster
[done] Reserve ID
[....] Install Ubuntu
[    ] Configure guest
[    ] Configure replication
[    ] Configure HA

[Details v]
```

This is much better than a sequence of popups.

Modal dialogs should be reserved for concise confirmations, important warnings, or naturally modal interactions.

---

# 41. GUI Rendering Rules for Input Groups

Given an `input_group` event, the frontend should:

1. Create one cohesive form.
2. Preserve field order from the script.
3. Render `title` and `description`.
4. Render each field using its type.
5. Render help text adjacent to the relevant field.
6. Validate simple syntax client-side.
7. Submit all values in one response.
8. Keep the form available if script-side validation fails.
9. Associate `field_errors` with specific controls.
10. Avoid sending unchanged secret values back into logs or history.

An input group may optionally specify a layout hint:

```json
{
  "layout": "form"
}
```

Possible future hints:

```text
form
grid
table
```

Do not over-design layout metadata initially. The frontend should own most visual layout decisions.

---

# 42. Dependencies Between Fields

Some forms contain dependent fields.

Example:

- Selecting a host determines available disks.
- Selecting a production VM determines eligible staging hosts.
- Selecting encryption may reveal password-related fields.

Version 1 can solve this by using multiple input groups:

```text
request placement
  ->
script validates / computes
  ->
request resources based on placement
```

Do not initially create a complex bidirectional reactive form protocol unless a real workflow requires it.

This keeps the script authoritative and the protocol simple.

---

# 43. Back Navigation

Interactive scripts are naturally forward-moving, while graphical forms often encourage Back navigation.

Do not promise arbitrary Back navigation into an already-running script.

Instead:

- Before execution begins, the GUI may collect locally known configuration in a multi-step form if that information is defined statically.
- Once the live script has requested and consumed values, assume the workflow moves forward.
- If the user wants to revise an earlier decision, cancel and restart unless the script explicitly supports revision.

Avoid trying to rewind Bash execution.

---

# 44. Workflow Concurrency

Infrastructure workflows can conflict.

The Rust layer should support workflow locking.

Initial conservative policy:

- Allow multiple read-only diagnostic operations.
- Allow only one mutating workflow at a time by default.

Later, BMAC may define lock scopes such as:

```text
cluster
host:mox1
prod:prod1
storage:mox2
qdevice
```

Then independent operations could run concurrently where proven safe.

Do not optimize concurrency before safety requirements are known.

---

# 45. Preflight Checks

On application launch, Rust should perform lightweight checks such as:

- BMAC repository located.
- Expected directories exist.
- Required script files exist.
- Supported OS / architecture.
- Bash available.
- SSH available.
- Required configuration files present where applicable.
- Tailscale status if necessary for cluster access.
- Git/release compatibility information if useful.

Do not perform invasive cluster operations merely to launch the GUI.

The Settings or Diagnostics area can expose a more comprehensive environment check.

---

# 46. BMAC Repository Selection

During development, the GUI can assume it is launched from or configured against a BMAC checkout.

Later, support:

```text
BMAC repository
/home/nate/src/bmac
```

The selected root must be validated by Rust.

Validation should check known BMAC files/directories rather than trusting a user-supplied path.

Store the path in native application configuration, not in arbitrary frontend-controlled filesystem access.

---

# 47. Version Compatibility

The GUI and scripts evolve together.

The GUI should know:

- Its own application version.
- The BMAC repository version if available.
- The script protocol version.

If a packaged GUI is used against an incompatible script checkout, show an explicit error.

Do not silently attempt to run an unsupported protocol.

Future scripts may expose:

```json
{
  "type": "protocol",
  "protocol": "bmac-ui",
  "version": 1,
  "bmac_version": "0.2.0"
}
```

---

# 48. Rust-to-React API

Keep Tauri commands narrow.

Conceptual commands:

```text
get_app_info
get_platform_info
get_repository_info
list_workflows
start_workflow
respond_to_workflow
cancel_workflow
get_active_runs
get_run_history
get_run_details
```

Events from Rust to React may include:

```text
workflow_event
workflow_stderr
workflow_process_exited
```

Do not expose:

```text
run_shell_command
read_any_file
write_any_file
ssh_execute_arbitrary
```

The webview should not become a general-purpose privileged control surface.

---

# 49. TypeScript Protocol Types

Create explicit TypeScript discriminated unions.

Conceptual example:

```ts
type WorkflowEvent =
  | ProtocolEvent
  | WorkflowStartedEvent
  | WorkflowMetadataEvent
  | InputEvent
  | InputGroupEvent
  | ConfirmEvent
  | ManualActionEvent
  | PhaseEvent
  | ProgressEvent
  | PlanEvent
  | InfoEvent
  | WarningEvent
  | ErrorEvent
  | ValidationErrorEvent
  | LogEvent
  | ResultEvent
  | CompletedEvent;
```

Use exhaustive `switch` handling where practical so new protocol event types cause compile-time attention.

---

# 50. Rust Protocol Types

Mirror the event contract with Serde-tagged enums.

Conceptual shape:

```rust
#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum WorkflowEvent {
    Protocol { protocol: String, version: u32 },
    WorkflowStarted { workflow: String, run_id: String },
    Input { /* ... */ },
    InputGroup { /* ... */ },
    Confirm { /* ... */ },
    Progress { /* ... */ },
    Error { /* ... */ },
    Completed { /* ... */ },
}
```

Do not pass unvalidated arbitrary JSON directly from a script into React.

Rust should parse and validate protocol objects first.

---

# 51. Protocol Schema

Create a machine-readable JSON Schema for protocol events.

Suggested location:

```text
control-panel/protocol/bmac-ui-v1.schema.json
```

or:

```text
protocol/bmac-ui-v1.schema.json
```

The schema should cover:

- Events.
- Requests.
- Responses.
- Input fields.
- Input groups.
- Plans.
- Errors.
- Results.

Use it in tests.

The schema becomes a contract shared between Bash, Rust, TypeScript, and future consumers.

---

# 52. Testing Strategy

The GUI should add testing without discarding existing tests.

## 52.1 Existing Script Tests

All existing script tests remain authoritative and must continue passing in normal CLI mode.

## 52.2 Protocol Library Unit Tests

Add focused tests for `lib/ui_protocol.sh`.

Test:

- JSON escaping.
- Single inputs.
- Input groups.
- Secret fields.
- Select fields.
- Multiselect fields.
- Confirmations.
- Validation errors.
- Progress.
- Error events.
- Completion events.
- Malformed input responses.
- EOF.
- Cancellation handling where applicable.

## 52.3 Script JSON-Mode Tests

For each converted operator workflow, test:

- Normal CLI mode still works.
- `--json` emits only valid NDJSON on stdout.
- First event identifies the protocol.
- Requests have unique request IDs.
- Responses are correctly consumed.
- Input groups return complete values.
- Validation failures are machine-readable.
- Success produces a completion event and exit 0.
- Failure produces appropriate error information and non-zero exit.
- Secrets are not emitted.

## 52.4 Rust Tests

Test:

- Workflow allowlist.
- Repository path validation.
- argv construction.
- NDJSON parser.
- Unsupported protocol version.
- Malformed JSON.
- Unexpected process exit.
- stderr capture.
- Cancellation.
- Concurrent read-only workflow behavior.
- Mutating workflow lock.
- Secret redaction.

## 52.5 React Tests

Test:

- Every field type renders.
- Input groups render as one form.
- Validation errors attach to fields.
- Confirmations render correct severity.
- Destructive confirmation requires typed value when specified.
- Phase progress renders correctly.
- Logs render.
- Unsupported events fail visibly rather than silently.
- Sensitive inputs are not retained in persistent state.

## 52.6 End-to-End Tests

Create mock scripts that implement the protocol.

Mock cases should include:

```text
successful read-only workflow
single input
batched inputs
validation error then retry
warning then continue
manual action
long-running progress
destructive confirmation
script failure
malformed JSON
unexpected exit
cancellation
stderr noise
```

This allows the GUI to be tested extensively without modifying a real cluster.

---

# 53. Do Not Make Real-Cluster Testing Explode Again

One reason to keep the scripts authoritative is to avoid repeating the expensive real-world workflow validation already performed.

The GUI testing strategy should prove that:

```text
GUI input
  ->
protocol response
  ->
existing script
```

behaves equivalently to:

```text
terminal input
  ->
existing script
```

The operational logic below that boundary remains the same.

Real-cluster GUI testing is still necessary, but it should primarily validate the adapter and user experience rather than re-prove every low-level BMAC operation from scratch.

---

# 54. Initial Workflows to Implement

Do not convert every script before proving the architecture.

Start with three workflows that exercise different protocol requirements.

## 54.1 Read-Only Diagnostic Workflow

Use a cluster-state or cluster-health diagnostic workflow.

This proves:

- Process launch.
- JSON event parsing.
- Structured results.
- Dashboard rendering.
- Error handling.
- No destructive behavior.

## 54.2 Add Staging VM

Use the staging VM creation workflow.

This proves:

- Interactive input.
- Batched input where appropriate.
- Select lists.
- Long-running operations.
- Progress phases.
- Detailed logs.
- Result display.
- Cancellation semantics.

## 54.3 Remove Production VM

Use production VM removal.

This proves:

- Destructive workflows.
- Dry-run / plan.
- Typed confirmation.
- Strong warning UI.
- Failure behavior.
- Safe operation completion.

If these three workflows are clean, the architecture should be capable of handling most of the remaining BMAC workflows.

---

# 55. Migration Strategy for Existing Scripts

Convert scripts incrementally.

For each script:

1. Identify every user prompt.
2. Group related prompts into logical input groups.
3. Identify any dynamic dependencies between prompt groups.
4. Identify warnings and confirmations.
5. Identify destructive confirmation points.
6. Identify long-running phases.
7. Identify meaningful result data.
8. Identify current dry-run output.
9. Add `--json`.
10. Reuse existing workflow functions.
11. Route human interaction through normal prompt helpers.
12. Route JSON interaction through `ui_protocol.sh`.
13. Add JSON-mode tests.
14. Add the workflow to the Rust allowlist.
15. Add the frontend workflow experience.
16. Run existing tests.
17. Run protocol tests.
18. Perform targeted real-cluster validation.

Do not convert the underlying operational commands unless a separate bug or refactor requires it.

---

# 56. Preferred Interaction Helper Pattern

Instead of scripts directly using `read` everywhere, gradually centralize interaction.

Conceptual Bash API:

```bash
value="$(bmac_prompt_string \
  --id hostname \
  --label 'Host name' \
  --required)"
```

The helper behaves differently by mode:

```text
normal mode:
  prints human prompt and reads terminal input

JSON mode:
  emits input request and reads NDJSON response
```

Likewise for grouped inputs:

```bash
bmac_prompt_group_begin "vm_resources" "VM Resources"
bmac_prompt_group_add_integer "cores" "CPU cores" 8
bmac_prompt_group_add_integer "memory_gib" "Memory" 32
bmac_prompt_group_add_integer "disk_gib" "Disk capacity" 500
bmac_prompt_group_execute
```

The exact Bash API can differ. The important rule is that scripts should express interaction through shared helpers rather than duplicate protocol handling.

---

# 57. Standard Script Options

Where practical, standardize these options across operator-callable scripts:

```text
--json
--dry-run
--non-interactive
--yes
--help
```

Not every workflow must support every option.

Meaning:

`--json`
: Use BMAC machine interaction protocol.

`--dry-run`
: Validate and describe intended changes without performing them.

`--non-interactive`
: Never prompt. Fail if required values are missing.

`--yes`
: Accept supported confirmations. Destructive workflows may deliberately refuse this option if unattended confirmation would be unsafe.

`--help`
: Human-readable usage.

Do not change an existing option's semantics merely for GUI convenience.

---

# 58. GUI Safety Model

The GUI should improve safety, not weaken the CLI's existing protections.

Rules:

1. The script remains the final authority.
2. The GUI must not bypass script checks.
3. Destructive actions are visually distinct.
4. Dry-run plans are shown when supported.
5. Typed confirmation is used for high-impact destructive actions.
6. Secret data is redacted.
7. Unsupported platforms disable affected workflows.
8. Only allowlisted scripts can execute.
9. No arbitrary shell API.
10. No generic SSH command API.
11. No generic filesystem write API from React.
12. Mutating workflows are serialized initially.
13. Cancellation is disabled during script-declared unsafe phases.
14. Unexpected process termination is shown as failure, not success.
15. The GUI exposes detailed logs so operators can investigate failures.

---

# 59. Visual Design Principles

BMAC is infrastructure software. Prefer clarity over decorative complexity.

Recommended principles:

- Dense enough for technical operators.
- Clear hierarchy.
- Strong use of labels and status.
- Avoid excessive modal dialogs.
- Avoid animations that obscure state.
- Avoid relying on color alone to communicate status.
- Use text labels and icons in addition to color.
- Use explicit terms such as Healthy, Failed, Offline, Running, Waiting.
- Keep destructive actions separated from routine actions.
- Show identifiers such as VMID, host name, pool name, or dataset where useful.
- Make detailed technical output easy to reveal.

Because color alone is not sufficient for all users, status components should always include text or iconography independent of hue.

---

# 60. Status Components

Prefer:

```text
[check] Healthy
[x] Failed
[!] Warning
[...] Running
[-] Offline
```

Do not rely on:

```text
green dot
red dot
yellow dot
```

without accompanying semantics.

---

# 61. Terminal Details

The xterm.js panel should be secondary.

Default:

```text
[Details v]
```

Expanded:

```text
Detailed Output
------------------------------------------------------------
...
```

Useful controls:

```text
Copy
Save log
Clear view
Follow output
```

"Clear view" should not destroy stored run history unless explicitly requested.

The terminal should render only sanitized workflow output. Secret values must be redacted before reaching it.

---

# 62. Dashboard Refresh

Do not continuously hammer the cluster.

Possible initial policy:

- Refresh when Dashboard opens.
- Refresh after a mutating workflow completes.
- Provide a Refresh button.
- Optionally refresh on a moderate interval while Dashboard is visible.

Use the existing diagnostic workflows.

Later, a configurable polling interval may be added.

---

# 63. Links to Existing Administrative Interfaces

The GUI may provide convenience links to existing tools, for example:

- Proxmox web UI.
- Relevant HTTPS staging URL.
- Application URL.

Use normal OS browser opening.

Do not embed the Proxmox administrative UI inside the Tauri webview unless there is a strong reason.

---

# 64. Local Notifications

Optional future feature:

- Notify when a long workflow completes.
- Notify when a workflow fails.
- Notify when manual input is required.

This is useful for host installation or replication operations that may take significant time.

It is not required for the first milestone.

---

# 65. Run Persistence

Version 1 may store lightweight local operation history.

A simple local file or SQLite database is sufficient.

Possible fields:

```text
run_id
workflow_id
start_time
end_time
status
target_summary
input_summary_redacted
result_summary
log_path
```

Do not persist secrets.

Do not introduce PostgreSQL or any remote database for a local desktop application.

If durable local structured history becomes useful, SQLite is appropriate.

---

# 66. Application State

Keep frontend state simple.

Recommended categories:

```text
navigation state
current dashboard data
active workflow state
workflow form state
local UI preferences
```

Do not make React state the authoritative representation of cluster state.

Cluster state must be refreshed from BMAC diagnostic logic.

---

# 67. Failure Recovery

A workflow can fail after partially modifying infrastructure.

The GUI must not invent recovery behavior.

If the existing script supports resume or recovery, expose that behavior.

A failure page can show:

```text
Operation failed

Reason:
...

The workflow may have made partial changes.

Recommended next action:
...

[View Details]
[Run Diagnostic]
[Retry / Resume if supported]
```

Any suggested recovery action should come from stable workflow metadata or script output, not speculative frontend logic.

---

# 68. Protocol Example: Batched Inputs

Full example:

Script emits:

```json
{"type":"protocol","protocol":"bmac-ui","version":1}
{"type":"workflow_started","workflow":"add_prod_vm","run_id":"run-123"}
{"type":"input_group","request_id":"req-1","title":"Production VM Resources","description":"Choose the initial resources for the VM.","fields":[{"id":"cores","type":"integer","label":"CPU cores","required":true,"min":1,"default":8},{"id":"memory_gib","type":"integer","label":"Memory","suffix":"GiB","required":true,"min":1,"default":32},{"id":"disk_gib","type":"integer","label":"Disk capacity","suffix":"GiB","required":true,"min":64,"default":500}]}
```

GUI responds:

```json
{"type":"response","request_id":"req-1","values":{"cores":8,"memory_gib":32,"disk_gib":500}}
```

Script validates.

If invalid:

```json
{"type":"validation_error","request_id":"req-1","field_errors":{"memory_gib":"mox2 does not have enough available RAM for this configuration."}}
```

The GUI redisplays the existing form and highlights `memory_gib`.

User corrects the field.

GUI responds again:

```json
{"type":"response","request_id":"req-1","values":{"cores":8,"memory_gib":16,"disk_gib":500}}
```

Script continues.

---

# 69. Protocol Example: Selection Plus Dependent Form

Script:

```json
{
  "type":"input_group",
  "request_id":"req-placement",
  "title":"Placement",
  "fields":[
    {
      "id":"placement_hosts",
      "type":"multiselect",
      "label":"Placement hosts",
      "required":true,
      "min_selected":2,
      "options":[
        {"value":"mox1","label":"mox1"},
        {"value":"mox2","label":"mox2"},
        {"value":"mox3","label":"mox3"}
      ]
    }
  ]
}
```

GUI:

```json
{
  "type":"response",
  "request_id":"req-placement",
  "values":{"placement_hosts":["mox1","mox2"]}
}
```

The script can now inspect those hosts and issue the next form with defaults or limits appropriate for that placement.

This is intentionally preferred over implementing a complex frontend rules engine.

---

# 70. Protocol Example: Destructive Workflow

Script:

```json
{"type":"protocol","protocol":"bmac-ui","version":1}
{"type":"workflow_started","workflow":"remove_prod_vm","run_id":"run-456"}
{"type":"phase","id":"validate","label":"Validate production VM","status":"running"}
{"type":"phase","id":"validate","label":"Validate production VM","status":"complete"}
{"type":"plan","title":"Remove prod1","items":[{"kind":"remove","target":"VM 100","description":"Remove production VM"},{"kind":"remove","target":"HA configuration","description":"Remove prod1 HA resource"},{"kind":"remove","target":"replication","description":"Remove prod1 replication jobs"}]}
{"type":"confirm","request_id":"req-remove","title":"Remove prod1?","message":"Review the plan before continuing.","severity":"destructive","confirm_label":"Remove prod1","confirmation_text":"prod1"}
```

GUI:

```json
{"type":"response","request_id":"req-remove","confirmed":true}
```

Script continues with its existing safety checks and removal workflow.

---

# 71. Protocol Example: Long-Running Workflow

```json
{"type":"phase","id":"replication","label":"Initial replication","status":"running"}
{"type":"progress","phase":"replication","message":"Sending production disk to mox2"}
{"type":"log","level":"info","source":"zfs","text":"incremental stream started"}
{"type":"progress","phase":"replication","current":1073741824,"total":2147483648,"unit":"bytes","message":"Initial replication"}
{"type":"phase","id":"replication","label":"Initial replication","status":"complete"}
```

The GUI should update in real time.

---

# 72. Protocol Example: Manual Action

```json
{
  "type":"manual_action",
  "request_id":"req-boot",
  "title":"Boot the server",
  "instructions":[
    "Boot the server from the generated Proxmox installer media.",
    "Wait until the installer reaches the expected startup point."
  ],
  "acknowledge_label":"Server is ready"
}
```

This is preferable to printing a paragraph and waiting for Enter.

---

# 73. Protocol Example: Result

Successful workflows should return useful structured result data when appropriate.

Example:

```json
{
  "type":"result",
  "data":{
    "guest":"prod2",
    "vmid":101,
    "host":"mox1",
    "placement":["mox1","mox2"]
  }
}
```

Then:

```json
{
  "type":"completed",
  "status":"success",
  "message":"prod2 was created successfully."
}
```

The GUI can use result data to update navigation and refresh relevant state.

---

# 74. CLI and GUI Must Stay Equivalent

The following two paths should call the same workflow logic:

```text
CLI user
  ->
human prompt helper
  ->
validated workflow data
  ->
BMAC implementation
```

and:

```text
GUI
  ->
JSON protocol helper
  ->
validated workflow data
  ->
BMAC implementation
```

This is the most important maintainability requirement in the design.

---

# 75. Future Automation Benefits

The JSON protocol is useful beyond the desktop application.

Once stable, the same interface could support:

- CI/CD.
- Automated infrastructure checks.
- AI coding agents.
- An MCP server.
- A future CLI with richer machine-readable output.
- Integration tests.
- External orchestration tools.

Those are future possibilities, not requirements for version 1.

Do not distort the first version of the protocol to satisfy hypothetical consumers.

---

# 76. Why Not Rewrite the Scripts in Rust

A rewrite would create:

- A second implementation during transition.
- New operational bugs.
- Revalidation of destructive behavior.
- New real-cluster testing requirements.
- Risk of behavioral differences between CLI and GUI.
- Loss of confidence accumulated in the existing scripts.

Rust should supervise the scripts, not replace them.

Individual pieces of logic may move into Rust in the future only when there is an independent reason to do so.

---

# 77. Why Not Electron

Electron is viable and process management is easy through Node.js, but it carries a larger runtime and application footprint.

BMAC does not need a bundled Chromium plus Node environment merely to provide a local control panel.

Tauri is a better fit for a lightweight native administrative application.

---

# 78. Why Not Axum + HTMX

Rust + Tokio + Axum + Hyper + rustls + SQLx + Askama + HTMX is a strong stack for an Internet-accessible web application.

It is not necessary for this desktop control panel.

Using a localhost web server would add concerns such as:

- Server lifecycle.
- Port management.
- Browser lifecycle.
- Local authentication assumptions.
- CSRF.
- Multiple sessions.
- HTTP transport between components that already reside in one desktop application.

Tauri commands and events are a cleaner local boundary.

---

# 79. Why Not Python + Qt

Python + PySide6 / Qt is a reasonable rapid-prototype option and QProcess is well suited to wrapping scripts.

However, Tauri provides:

- A more natural path to a polished HTML/CSS interface.
- A Rust security/process boundary.
- Good cross-platform packaging.
- A smaller and cleaner long-term dependency story for this project.

Python + Qt remains a reasonable fallback if development speed proves substantially more important than the preferred architecture.

---

# 80. Implementation Phases

## Phase 1 - Protocol Foundation

Implement:

- `--json` convention.
- NDJSON stdout.
- NDJSON stdin responses.
- Protocol version event.
- Shared Bash protocol library.
- Basic event types.
- Batched input groups.
- Error events.
- Completion events.
- Protocol tests.

No full GUI is required yet.

Create mock scripts and exercise protocol behavior from the terminal.

## Phase 2 - Tauri Skeleton

Implement:

- Tauri application.
- React frontend.
- Rust workflow registry.
- Process launcher.
- stdout NDJSON parser.
- stderr capture.
- event forwarding.
- response writer.
- cancellation framework.
- platform detection.

Use mock workflows first.

## Phase 3 - First Read-Only Workflow

Convert one diagnostic workflow.

Implement:

- Start workflow.
- Structured result.
- Dashboard display.
- Failure handling.
- Detailed logs.

## Phase 4 - Interactive Workflow

Convert staging VM creation.

Implement:

- Input.
- Input groups.
- Select.
- Multiselect.
- Progress.
- Long-running operation display.
- Result summary.

## Phase 5 - Destructive Workflow

Convert production VM removal.

Implement:

- Dry-run / plan.
- Warning UI.
- Destructive confirmation.
- Typed confirmation.
- Failure presentation.

## Phase 6 - Core Dashboard

Convert enough diagnostics to show:

- Cluster health.
- Hosts.
- Production VMs.
- Staging VMs.
- QDevice.
- Replication.
- Storage health.

## Phase 7 - Remaining Operator Workflows

Convert scripts category by category.

Suggested order:

```text
Diagnostics
Staging
Production
QDevice
Hosts
Storage
App-related helper workflows
```

Actual order should follow operational value and risk.

## Phase 8 - Packaging

Add:

- Linux package / AppImage decision.
- Release build.
- Application icon.
- Version reporting.
- Update documentation.

macOS packaging can follow when enough supported workflows justify it.

---

# 81. Definition of Done for a Converted Workflow

A workflow is considered GUI-ready only when:

- Existing CLI invocation still works.
- Existing tests still pass.
- `--json` emits valid NDJSON only on stdout.
- Protocol version is emitted.
- Every interactive prompt is represented structurally.
- Related sequential prompts are grouped where appropriate.
- Script-side validation works in JSON mode.
- Destructive actions use structured confirmation.
- Long-running steps expose meaningful phase/progress information where practical.
- Errors are machine-readable where practical.
- Secrets are redacted.
- Completion state is explicit.
- Rust allowlist entry exists.
- React UI exists.
- Protocol tests pass.
- GUI tests pass.
- Targeted integration testing has been performed.

---

# 82. Coding Guidelines

## Bash

- Keep operational logic in existing workflow functions.
- Centralize interaction helpers.
- Centralize JSON emission.
- Quote shell variables correctly.
- Never manually interpolate unescaped values into JSON.
- Keep stdout protocol-clean in JSON mode.
- Preserve current normal CLI output outside JSON mode.
- Fail loudly on protocol errors.
- Redact secrets.

## Rust

- Treat scripts as untrusted subprocess output until parsed.
- Parse protocol into typed structures.
- Validate protocol version.
- Use allowlisted workflow IDs.
- Use argv, never shell command strings.
- Keep filesystem permissions narrow.
- Keep secrets out of frontend events.
- Handle unexpected process death.
- Model active run lifecycle explicitly.

## TypeScript / React

- Treat Rust events as typed data.
- Use discriminated unions.
- Avoid operational business logic.
- Avoid duplicating script validation beyond basic form usability.
- Do not assume color conveys state.
- Keep destructive actions explicit.
- Keep long operations visible.
- Never silently ignore an unknown protocol event.

---

# 83. Open Design Decisions

These decisions can be deferred until implementation reaches them:

1. Exact native Linux packaging format.
2. Whether local operation history uses JSON files or SQLite.
3. Whether the GUI is distributed inside the BMAC release archive or as a separate binary package.
4. Whether protocol JSON Schema lives at repository root or under `control-panel/`.
5. Exact names of Bash protocol helper functions.
6. Exact field type set for protocol v1.
7. Whether the first macOS release is x86_64 only, arm64 only, or universal.
8. Whether the GUI later offers editing of `env/*.conf`.
9. Whether future workflow discovery remains static or becomes script-described.
10. Whether desktop notifications are enabled by default.

None of these should block the initial protocol and Tauri architecture.

---

# 84. Decisions That Should Not Be Reopened Without a Strong Reason

The following are core architectural decisions:

1. Existing BMAC scripts remain the authoritative workflow implementation.
2. The GUI wraps scripts rather than reimplementing their infrastructure logic.
3. Tauri 2 is the desktop shell.
4. Rust is the trusted process/security layer.
5. React + TypeScript is the primary UI layer.
6. Script communication uses a versioned structured JSON protocol.
7. Streaming communication uses NDJSON.
8. Related input values can be requested together using first-class input groups.
9. The GUI must not scrape human-readable prompt text.
10. The GUI must not expose arbitrary shell execution.
11. Secrets remain outside the React webview whenever possible.
12. Existing CLI behavior remains supported.
13. Scripts retain final authority for semantic validation and safety checks.
14. Read-only diagnostics should feed the dashboard rather than being reimplemented in the frontend.

---

# 85. Final Architectural Principle

The Control Panel should make BMAC easier to operate without changing what BMAC fundamentally is.

The desired result is:

```text
                 +----------------------+
                 |     BMAC Workflow    |
                 |        Logic         |
                 +-----------+----------+
                             |
                  same implementation
                             |
             +---------------+---------------+
             |                               |
             v                               v
      Terminal interface              Desktop interface
      Bash prompts/output              Tauri + React
             |                               |
             +---------------+---------------+
                             |
                             v
                   Proxmox / ZFS / SSH
```

The user should be able to choose either interface with confidence that both invoke the same tested BMAC implementation.

The GUI exists to improve discoverability, visualization, input collection, progress reporting, safety presentation, and operator usability.

It must not become a parallel infrastructure engine.
