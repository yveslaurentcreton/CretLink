// The data the new interfaces render (ST-M009-008, ST-M009-009), shared by
// both roles. Every type here mirrors a `serde` type on the Rust side, which is
// authoritative; field names are camelCase on the wire. The backend builds
// every string; the frontend writes untrusted text (operations, paths, notes)
// with `textContent`, never as HTML (REQ-S-016, REQ-S-018).

// --- Activity items (ST-M009-011) ---

// Who started an action. `ai` is an MCP client through the bridge, `terminal`
// the technician's embedded terminal, `technician` any other console action
// (a manual transfer, a stop).
export type ActivitySource = "ai" | "terminal" | "technician";

// What an action does, from the structured `RequestHandled` kind. A peer
// without the structured fields yields `command` for executions and
// `file_operation` otherwise, with the operation text as the only detail.
export type ActivityKind =
  | "command"
  | "file_sent" // left the customer computer (REQ-S-016)
  | "file_received" // arrived on the customer computer
  | "file_operation"
  | "screen_observed"
  | "screen_controlled"
  | "clipboard_read"
  | "clipboard_written"
  | "system_information"
  | "job_control";

export type ActivityState =
  | { state: "running" }
  | { state: "done"; exitCode?: number }
  | { state: "failed"; exitCode?: number; message?: string }
  | { state: "cancelled" }
  | { state: "timed_out" };

export interface ActivityItem {
  // Stable for the life of the item, so a running item is updated in place
  // when it finishes.
  id: string;
  source: ActivitySource;
  // Technician side only: "Claude Code", "Codex CLI", "Gemini CLI" or
  // "AI assistant". Never sent to the customer computer.
  clientLabel?: string;
  // The AI's stated purpose: an unverified claim, shown beside `operation`,
  // never instead of it (REQ-F-026). AI actions only.
  purpose?: string;
  // A plain description for actions without a purpose ("Ran a command in the
  // terminal", "Sent a file"). Always present.
  description: string;
  kind: ActivityKind;
  // The real operation in plain words, bounded and free of file or typed
  // content.
  operation: string;
  path?: string;
  bytes?: number;
  state: ActivityState;
  // RFC 3339, UTC.
  startedAt: string;
  durationMs?: number;
}

// One entry of a feed: an item, or a marker that `count` items were shed
// under pressure at this place (no silent gaps, ST-M009-011).
export type ActivityEntry =
  | { entry: "item"; item: ActivityItem }
  | { entry: "gap"; count: number; at: string };

// The live event both roles emit per change; an `item` with a known `id`
// replaces the earlier one in place.
export type ActivityEvent = ActivityEntry;

// --- Session history and end reports (ST-M009-012) ---

export type EndedBy = "technician" | "customer" | "connection_lost" | "failure";

export interface CleanupNote {
  ok: boolean;
  text: string;
}

export interface CleanupResult {
  // `unknown` when no cleanup report arrived (a lost connection, an
  // interrupted session): never shown as success (REQ-N-015).
  state: "cleaned_up" | "residuals" | "failed" | "unknown";
  residuals: number;
  // Failures and residuals first.
  notes: CleanupNote[];
}

export interface ActionCounts {
  ai: number;
  terminal: number;
  technician: number;
}

export interface SessionSummary {
  // The audit file's session id (short form for display is the first 6
  // characters).
  id: string;
  // The audit file this summary was read from.
  file: string;
  // A broken or unsealed file is listed as such, never skipped.
  integrity: "verified" | "unsealed" | "broken";
  hostname?: string;
  os?: string;
  startedAt?: string;
  endedAt?: string;
  durationMs?: number;
  endedBy?: EndedBy;
  actions: ActionCounts;
  filesSent: number;
  filesReceived: number;
  failures: number;
  firstFailure?: string;
  cleanup: CleanupResult;
}

// One record of this session's audit log, as the technician's Live tab
// **Audit** shows it (ST-M009-008): `get_session_audit` returns the current
// or last session's, oldest first, at most 500. Pairing, consent, cleanup and
// end records are included. `summary` is one secret-free line built from the
// record (a redaction reads `[REDACTED]`, bytes only as a digest); hash-chain
// fields and payloads are never sent.
export type AuditActor = "ai" | "technician" | "terminal" | "agent" | "system";
export type AuditOutcome = "ok" | "failed" | "cancelled" | "timeout" | "unknown";

export interface AuditEntryView {
  seq: number;
  // RFC 3339 UTC.
  at: string;
  actor: AuditActor;
  // The record kind, e.g. "consent_granted".
  action: string;
  outcome: AuditOutcome;
  summary: string;
}

// The customer's end report (ST-M009-012): built in memory from what the
// customer saw — the activity feed, the agent's own teardown report, this
// computer's hostname and what the technician's computer said its user is
// called — and nothing else; no session secret. Written to disk only by
// `save_report`.
export interface CustomerCounts {
  commands: number;
  // Failed, timed out, or with an unknown result.
  failedCommands: number;
  // Completed transfers that left this computer (REQ-S-016).
  filesSent: number;
  filesReceived: number;
  screenshots: number;
}

export interface CustomerReport {
  // From the customer's point of view: `customer` is "you stopped it".
  // Absent when the session task did not report its end.
  endedBy?: EndedBy;
  // What the technician's computer says its user is called (unverified).
  technicianName?: string;
  // This computer.
  hostname: string;
  controlGranted: boolean;
  // RFC 3339, UTC: when pairing completed, and when the session ended.
  startedAt: string;
  endedAt: string;
  durationMs: number;
  counts: CustomerCounts;
  // Failures and residuals first; `unknown` when no report arrived.
  cleanup: CleanupResult;
  // Every item the customer saw, oldest first.
  activity: ActivityEntry[];
}

// --- AI clients (ST-M009-003, ST-M009-006) ---

export type ClientId = "claude" | "codex" | "gemini";

export type ClientRegistration =
  | { state: "absent" }
  | { state: "this_executable"; command: string }
  | { state: "other_command"; command: string }
  | { state: "unreadable"; message: string };

export interface AiClientStatus {
  id: ClientId;
  label: string; // "Claude Code", "Codex CLI", "Gemini CLI"
  // Whether the client's command line is on PATH.
  present: boolean;
  configPath?: string;
  registration: ClientRegistration;
  // The exact commands install and uninstall would run, for display before
  // anything changes (REQ-F-025).
  installPlan: string[];
  uninstallPlan: string[];
}

// --- The local MCP bridge, for the technician's rail (ST-M009-008) ---

// `get_bridge_status`, and the `console-bridge` event on every change.
export interface BridgeStatus {
  // `ready` while the IPC endpoint is bound to the consented session;
  // `unavailable` before full control, after the session, or when the bind
  // failed (`message` then says why, with the next step).
  endpoint: "ready" | "unavailable";
  message?: string;
  // The number of remote tools the bridge offers.
  tools: number;
  // The labels of the AI clients that called over local IPC during this
  // session, in the order they first called.
  clients: string[];
}

// --- The file explorer of the live session (ST-M010-002) ---

// One drive of the customer computer (`explorer_drives`). A value the
// customer computer could not read is absent, never zero.
export type ExplorerDriveKind =
  | "fixed"
  | "removable"
  | "network"
  | "optical"
  | "ram_disk"
  | "unknown";

export interface ExplorerDrive {
  // The drive's root folder, such as `C:\`.
  root: string;
  kind: ExplorerDriveKind;
  label?: string;
  totalBytes?: number;
  freeBytes?: number;
}

// One entry of a folder. A link is described and never followed. Every text
// is the customer computer's own: written as text, never as markup.
export type ExplorerEntryKind = "file" | "dir" | "symlink" | "other";

export interface ExplorerEntry {
  name: string;
  kind: ExplorerEntryKind;
  // Size in bytes.
  len: number;
  // Last modified, in Unix milliseconds, when the customer computer says.
  modifiedMs?: number;
  readonly: boolean;
}

// One step of a listing on the `console-explorer` event, while
// `explorer_open` runs. `listing` is the number the window gave the listing.
// `opened` comes once, first, with the folder as the customer computer names
// it and the first page; `page` brings each further page. `end` says that the
// folder ends with this page.
export type ExplorerEvent =
  | {
      kind: "opened";
      listing: number;
      path: string;
      // The file that the asked path named, in that folder.
      select?: string;
      entries: ExplorerEntry[];
      end: boolean;
    }
  | { kind: "page"; listing: number; entries: ExplorerEntry[]; end: boolean };

// How a listing ended: the answer of `explorer_open`. Only `complete` is the
// whole folder: `cancelled` was left for another place, `limit` stopped at
// the most entries the explorer lists.
export interface ExplorerListing {
  listing: number;
  // Absent when the listing was left before the folder was found.
  path?: string;
  entries: number;
  state: "complete" | "cancelled" | "limit";
}

// --- The network the technician waits on (ST-M009-016) ---

// `get_listen_networks`: this computer's private addresses, each with the
// name of its network adapter, in the order the waiting screen lists them.
export interface ListenNetwork {
  // The private address, without a port.
  address: string;
  // The adapter's name as the operating system gives it ("Wi-Fi", "en0").
  adapter: string;
  // A point-to-point link, as many VPN tunnels are.
  pointToPoint: boolean;
}

// --- This copy of the application (ST-M009-006, ST-M009-008) ---

export interface AppInfo {
  hostname: string;
  os: string;
  version: string;
  // `installed` when this copy runs from the per-user install location.
  install: "installed" | "run_once";
  // The install path, or the folder this copy runs from.
  location: string;
  auditDir: string;
}

// --- Run once or install (ST-M009-006, `#start`, `#t-install`) ---

// What the install screen offers (`install_options`).
export interface InstallOptions {
  // Where install puts CretLink: fixed per operating system.
  location: string;
  // The menu entry install can add. Absent on macOS, where the bundle in
  // ~/Applications is the entry.
  entry?: "start_menu" | "desktop_entry";
  entryPath?: string;
  // Each client with the change install would make, planned against the
  // installed program rather than this copy.
  clients: AiClientStatus[];
  // A copy is installed already: install replaces it (an update in place).
  update: boolean;
  // The installed copy runs now: install would stop and say so.
  running: boolean;
}

// How `install_app({ entry, clients })` ended. `started`: the installed copy
// is starting and this window closes. `installed`: installed, but something
// needs the technician first (`failures`, in words).
export interface InstallOutcome {
  state: "started" | "installed";
  location: string;
  failures: string[];
}
