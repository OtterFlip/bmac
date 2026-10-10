// bmac-ui v2 protocol and engine types. These mirror engine/src/protocol.rs,
// engine/src/history.rs, and engine/src/registry.rs; Rust has already
// validated every value before it gets here.

export type FieldType =
  | "string"
  | "multiline"
  | "integer"
  | "number"
  | "boolean"
  | "password"
  | "select"
  | "multiselect"
  | "hostname"
  | "ip_address"
  | "cidr"
  | "mac_address"
  | "path"
  | "file"
  | "directory"
  | "ssh_public_key"
  | "duration"
  | "bytes";

export interface FieldOption {
  value: string;
  label: string;
  help?: string;
}

export interface Field {
  id: string;
  type: FieldType;
  label: string;
  help?: string;
  required: boolean;
  default?: unknown;
  placeholder?: string;
  suffix?: string;
  min?: number;
  max?: number;
  min_selected?: number;
  max_selected?: number;
  pattern?: string;
  sensitive: boolean;
  disabled: boolean;
  options?: FieldOption[];
  filters?: { name: string; extensions: string[] }[];
}

export type Severity = "normal" | "warning" | "destructive" | "critical";
export type PhaseStatus = "pending" | "running" | "complete" | "failed" | "skipped";
export type LogLevel = "debug" | "info" | "warning" | "error";
export type PlanKind = "create" | "update" | "remove" | "keep" | "check";

export interface PlanItem {
  kind: PlanKind;
  target: string;
  description: string;
}

export interface ProtocolEvent {
  type: "protocol";
  protocol: string;
  version: number;
  bmac_version?: string;
}
export interface WorkflowStartedEvent {
  type: "workflow_started";
  workflow: string;
  run_id: string;
  script?: string;
  argv: string[];
  pid?: number;
  started_at?: string;
}
export interface WorkflowMetadataEvent {
  type: "workflow_metadata";
  workflow?: string;
  title?: string;
  description?: string;
  category?: string;
  destructive?: boolean;
  supports_dry_run?: boolean;
  supports_cancel?: boolean;
}
export interface InputEvent {
  type: "input";
  request_id: string;
  title?: string;
  description?: string;
  context?: string;
  field: Field;
}
export interface InputGroupEvent {
  type: "input_group";
  request_id: string;
  title: string;
  description?: string;
  context?: string;
  layout?: string;
  fields: Field[];
}
export interface ConfirmEvent {
  type: "confirm";
  request_id: string;
  title: string;
  message: string;
  severity: Severity;
  confirm_label?: string;
  cancel_label?: string;
  confirmation_text?: string;
  context?: string;
  details?: string[];
}
/** A manual action step, optionally with a value the operator can copy.
 *  run_on names the host where they must run the script whose path is copy. */
export type Instruction = string | { text: string; copy: string; run_on?: string };
export interface ManualActionEvent {
  type: "manual_action";
  request_id: string;
  title: string;
  instructions: Instruction[];
  acknowledge_label?: string;
  context?: string;
}
export interface ProgressEvent {
  type: "progress";
  message: string;
  current?: number;
  total?: number;
  unit?: string;
  phase?: string;
}
export interface PhaseEvent {
  type: "phase";
  id: string;
  label: string;
  status: PhaseStatus;
  cancel_allowed?: boolean;
}
export interface PlanEvent {
  type: "plan";
  title: string;
  description?: string;
  items: PlanItem[];
  dry_run: boolean;
}
export interface InfoEvent {
  type: "info";
  message: string;
}
export interface WarningEvent {
  type: "warning";
  message: string;
}
export interface ErrorEvent {
  type: "error";
  code?: string;
  message: string;
  details?: string;
  recoverable: boolean;
}
export interface ValidationErrorEvent {
  type: "validation_error";
  request_id: string;
  field_errors: Record<string, string>;
  message?: string;
}
export interface LogEvent {
  type: "log";
  stream?: string;
  level: LogLevel;
  text: string;
  source?: string;
}
export interface ResultEvent {
  type: "result";
  data: unknown;
}
export interface NextStepEvent {
  type: "next_step";
  text: string;
  command?: string;
  workflow?: string;
  args?: Record<string, string>;
}
export interface CompletedEvent {
  type: "completed";
  status: "success" | "failed" | "cancelled";
  message?: string;
  exit_code?: number;
  finished_at?: string;
}

export type WorkflowEvent =
  | ProtocolEvent
  | WorkflowStartedEvent
  | WorkflowMetadataEvent
  | InputEvent
  | InputGroupEvent
  | ConfirmEvent
  | ManualActionEvent
  | ProgressEvent
  | PhaseEvent
  | PlanEvent
  | InfoEvent
  | WarningEvent
  | ErrorEvent
  | ValidationErrorEvent
  | LogEvent
  | ResultEvent
  | NextStepEvent
  | CompletedEvent;

export type RequestEvent = InputEvent | InputGroupEvent | ConfirmEvent | ManualActionEvent;

export function isRequest(event: WorkflowEvent): event is RequestEvent {
  switch (event.type) {
    case "input":
    case "input_group":
    case "confirm":
    case "manual_action":
      return true;
    default:
      return false;
  }
}

export type ResponsePayload =
  | { kind: "values"; values: Record<string, unknown> }
  | { kind: "confirm"; confirmed: boolean }
  | { kind: "acknowledge" }
  | { kind: "cancel" };

export interface RunEvent {
  seq: number;
  at: string;
  event: WorkflowEvent;
}

export type RunStatus =
  | "starting"
  | "running"
  | "waiting_input"
  | "waiting_confirmation"
  | "waiting_manual_action"
  | "cancelling"
  | "succeeded"
  | "failed"
  | "cancelled"
  | "interrupted";

export const TERMINAL_STATUSES: RunStatus[] = ["succeeded", "failed", "cancelled", "interrupted"];
export const isTerminal = (status: RunStatus) => TERMINAL_STATUSES.includes(status);
export const isWaiting = (status: RunStatus) =>
  status === "waiting_input" || status === "waiting_confirmation" || status === "waiting_manual_action";

export interface PhaseState {
  id: string;
  label: string;
  status: PhaseStatus;
  cancel_allowed: boolean;
  started_at?: string | null;
  ended_at?: string | null;
}

export interface NextStep {
  text: string;
  command?: string;
  workflow?: string;
  args?: Record<string, string>;
}

export interface ResponseRecord {
  request_id: string;
  at: string;
  title: string;
  kind: "values" | "confirm" | "acknowledge" | "cancel";
  values?: Record<string, unknown>;
  confirmed?: boolean;
}

export interface RunRecord {
  run_id: string;
  workflow_id: string;
  workflow_title: string;
  mode: "read_only" | "mutating";
  destructive: boolean;
  script: string;
  argv: string[];
  command_line: string;
  terminal_command: string;
  launch_values: Record<string, string>;
  target: string | null;
  dry_run: boolean;
  started_at: string;
  ended_at: string | null;
  status: RunStatus;
  message: string | null;
  exit_code: number | null;
  signal: number | null;
  pid: number | null;
  cancel: { requested_at: string; forced: boolean } | null;
  cancel_allowed: boolean;
  phases: PhaseState[];
  current_activity: string | null;
  pending_request: RequestEvent | null;
  result: unknown;
  next_steps: NextStep[];
  errors: string[];
  warnings: number;
  responses: ResponseRecord[];
  log_lines: number;
  last_log: string | null;
  event_count: number;
}

export interface RunDetails {
  record: RunRecord;
  events: RunEvent[];
  logs: RunEvent[];
  logs_truncated: boolean;
}

export type ParamType =
  | "flag"
  | "flag_choice"
  | "choice"
  | "host"
  | "production"
  | "staging"
  | "guest"
  | "integer"
  | "path"
  | "hostname";

export interface Param {
  id: string;
  label: string;
  type: ParamType;
  flag?: string;
  help?: string;
  required?: boolean;
  advanced?: boolean;
  default?: unknown;
  placeholder?: string;
  choices?: { value: string; label: string; flag?: string }[];
  suggest?: "free_host_slots";
}

export type Category = "diagnostics" | "hosts" | "production" | "staging" | "storage" | "qdevice";

export interface Workflow {
  id: string;
  title: string;
  summary: string;
  description: string;
  category: Category | string;
  script: string;
  mode: "read_only" | "mutating";
  destructive: boolean;
  concurrent?: boolean;
  supports_dry_run?: boolean;
  platforms: string[];
  requires?: { arch?: string; os_family?: string };
  platform_note?: string;
  notes?: string;
  params: Param[];
}

export interface WorkflowInfo extends Workflow {
  available: boolean;
  unavailable_reason?: string;
}

export interface PlatformInfo {
  os: string;
  arch: string;
  os_family: string | null;
  os_name: string | null;
  hostname: string | null;
}

export interface RepositoryInfo {
  root: string;
  /** The scripts bundled with an installed dashboard rather than a checkout. */
  bundled: boolean;
  config_dir: string;
  valid: boolean;
  problems: string[];
  git_commit: string | null;
  git_describe: string | null;
  git_branch: string | null;
  git_dirty: boolean | null;
  protocol_version: number | null;
  cluster_conf_present: boolean;
  secrets_env_present: boolean;
  cluster_settings: { key: string; value: string }[];
  host_configs: string[];
}

export interface AppInfo {
  name: string;
  version: string;
  protocol_version: number;
  history_dir: string;
  installed: boolean;
  first_launch: boolean;
}

export interface Settings {
  repository: string | null;
  refresh_interval_seconds: number;
  notifications: boolean;
  default_dry_run: boolean;
  seeded_version: string | null;
}

export interface PreflightCheck {
  id: string;
  label: string;
  status: "ok" | "warning" | "error";
  detail: string;
  fix?: string;
  /** The config file this check is about. */
  config_file?: string;
  link?: { label: string; url: string };
  /** A shell command that installs what is missing. */
  command?: string;
  /** One sentence on what BMAC uses this for. */
  purpose?: string;
}

export type ConfigFileKind = "config" | "example" | "secret";
export type ConfigFileState = "missing" | "unchanged" | "customized" | "no_example";

export interface ConfigFile {
  name: string;
  kind: ConfigFileKind;
  present: boolean;
  essential: boolean;
  example: string | null;
  user_file: string | null;
  state: ConfigFileState | null;
  size: number | null;
  modified: string | null;
}

export interface ConfigListing {
  dir: string;
  default_dir: string;
  /** Only an installed dashboard's config directory can move. */
  relocatable: boolean;
  problem: string | null;
  files: ConfigFile[];
}

export interface ConfigText {
  name: string;
  text: string;
  revision: string;
  read_only: boolean;
}

export interface DirEntry {
  name: string;
  path: string;
  kind: "directory" | "file" | "other";
  size: number | null;
  modified: string | null;
  symlink: boolean;
  hidden: boolean;
}

export interface DirListing {
  path: string;
  parent: string | null;
  entries: DirEntry[];
  truncated: boolean;
  shortcuts: { label: string; path: string }[];
}

export interface EngineError {
  code: string;
  message: string;
}
