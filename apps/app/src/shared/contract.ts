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
  // The host name of the relay the session ran through; absent for a session
  // on one network (ST-M007-013).
  relay?: string;
  // Where the technician's copy connected to reach that relay, and what the
  // relay's certificate was checked against, as the audit recorded them.
  // Absent in a log from before they were recorded.
  relayAddress?: string;
  relayTrust?: RelayTrust;
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

// `uninstall_plan`: what uninstalling this copy would remove and what it
// would leave in place, one line each, before anything changes. What stays
// includes a relay profile next to the installed program, with whether it
// holds an admission key (ST-M007-011). The audit trail stays too.
export interface UninstallPlan {
  removes: string[];
  stays: string[];
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

// --- What the explorer sends, copies, renames and deletes (ST-M010-002) ---

// What the technician dropped on the window or picked in the system's
// dialog (`explorer_pick_upload`, and the `console-explorer-drop` event): a
// number to send it by, and names for the words. The window never holds a
// path of the technician computer.
export interface UploadOffer {
  offer: number;
  // How many files and folders were dropped or picked.
  items: number;
  // The names of the first few.
  names: string[];
}

export type TransferKind = "file" | "folder";

// A name that is taken where a transfer would put it.
export interface NameConflict {
  name: string;
  // What is to be copied.
  kind: TransferKind;
  // What lies under that name now.
  existing: ExplorerEntryKind;
  // Whether Replace can be chosen: a file for a file, a folder into a folder.
  replace: boolean;
}

// What a look at the folder found (`explorer_send_check`,
// `explorer_download_check`).
export interface TransferCheck {
  // The folder the transfers go to, as text for the question.
  folder: string;
  conflicts: NameConflict[];
}

// What the technician chose for a name that is taken. `keep_both` gives the
// new one a name in the pattern Windows uses, `name (2).ext`.
export type ConflictChoice = "replace" | "keep_both" | "skip";

// How a send or a copy was queued (`explorer_send`, `explorer_download`).
export interface TransferQueued {
  queued: number;
  // Left out or not complete, each with a row that says why.
  leftOut: number;
  skipped: number;
}

// Only `done` is a transfer that arrived and was verified. `left_out` was
// never copied (a link, a name that cannot be used), `stopped` ended with the
// session, and `listed` is a folder whose files are queued: the window drops
// it.
export type TransferState =
  | "waiting"
  | "running"
  | "done"
  | "failed"
  | "cancelled"
  | "left_out"
  | "stopped"
  | "listed";

// One entry of the transfer queue (`explorer_transfers`, and the
// `console-transfers` event). Every text may hold names of the customer
// computer: written as text, never as markup.
export interface TransferEntry {
  // Rising in the order of the queue.
  id: number;
  direction: "send" | "download";
  kind: TransferKind;
  // What is copied: a name, or a path below a folder that was dropped or
  // selected.
  name: string;
  // Where it is on the customer computer.
  remote: string;
  // Where it is on the technician computer.
  local: string;
  bytes?: number;
  // The bytes confirmed so far.
  done: number;
  state: TransferState;
  // Why it is in this state, as sentences.
  reason?: string;
}

// Entries of the queue that changed.
export interface TransfersEvent {
  entries: TransferEntry[];
}

// A folder of the technician computer chosen in the system's dialog
// (`explorer_choose_folder`): the number a copy names it by, and its path
// for the words.
export interface ChosenFolder {
  folder: number;
  path: string;
}

// How a delete ended (`explorer_delete`). An item counts as removed only
// when the customer computer said so.
export interface DeleteReport {
  selected: number;
  removed: number;
  // Files and links, and folders, removed in all.
  removedFiles: number;
  removedDirs: number;
  // The first entries that could not be removed, each with its reason.
  failed: Array<{ path: string; reason: string }>;
  failedMore: number;
  cancelled: boolean;
  // The session ended on the way: what to say about it.
  ended?: string;
  notTried: number;
}

// Where a delete stands (`console-removal`).
export interface RemovalProgress {
  // The number the window gave the delete.
  removal: number;
  item: string;
  at: number;
  of: number;
  removedFiles: number;
  removedDirs: number;
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

// --- The relay of this copy (ST-M007-011) ---

// What the certificate of a relay is checked against: the certificate
// authorities of the computer, or the certificate file that the relay
// profile names and nothing else. A screen that names a relay says so when
// it is the file (`shared/relay.ts`).
export type RelayTrust = "system_authorities" | "profile_file";

// A relay as a screen shows it. A relay profile is addressing only: it says
// where two copies meet and grants nothing. The view never holds the
// admission key, only whether there is one.
export interface RelayView {
  // The relay's host name: the name its certificate must carry.
  host: string;
  // Where it is reached: `host:port`.
  address: string;
  trust: RelayTrust;
  keySet: boolean;
  // The certificate authority file the profile names, next to it.
  ca?: string;
  // The name in the certificate, when the profile gives one.
  name?: string;
}

export type RelayField = "address" | "key" | "ca" | "name";

// A relay profile that cannot be used: the file, the setting and what is
// wrong as a sentence. It never holds text from the file.
export interface RelayProblem {
  file: string;
  field?: RelayField;
  message: string;
}

// What one place says about a relay: the profile file next to the program,
// or the settings of the installed copy.
export interface RelayPlace {
  path: string;
  relay?: RelayView;
  problem?: RelayProblem;
}

// `get_relay_settings`, and the answer of `save_relay` and `remove_relay`.
export interface RelaySettings {
  // Whether this copy keeps settings: only the installed copy does. A copy
  // that runs once takes its relay from the file next to the program.
  editable: boolean;
  // Which profile this copy uses. A file next to the program wins over the
  // settings, also when that file cannot be used.
  source: "file" | "settings" | "none";
  file: RelayPlace;
  // What the settings hold; absent in a copy that keeps none.
  stored?: RelayPlace;
}

// The relay as the form of Settings → Relay gives it (`save_relay`,
// `test_relay`). A key travels to the backend only; none ever comes back:
// `keep` leaves the kept key alone. The certificate authority file is chosen
// in the system's dialog, opened by the backend, and named here by its
// number: the window never holds a path of this computer.
export interface RelayForm {
  address: string;
  keyMode: "keep" | "none" | "set";
  key?: string;
  caMode: "keep" | "none" | "chosen";
  caPick?: number;
  name?: string;
}

// `choose_relay_ca`: the number a form names the chosen file by, and its
// file name for the words. `null` when the dialog was closed.
export interface ChosenCa {
  pick: number;
  name: string;
}

// `test_relay`: what reaching the relay once came to, in words. `kind` is
// `admitted`, or the name of the failure.
export interface RelayTest {
  ok: boolean;
  kind: string;
  message: string;
  // What to do next; empty when the relay admits this copy.
  next: string;
}

// --- Make a customer copy (ST-M007-016) ---

// `customer_copy_options`: what a customer copy would hold, before anything
// is typed. `relay` is the relay of this technician copy without any key:
// the technician's own key is never part of a customer copy.
export interface CustomerCopyOptions {
  relay: RelayView;
  // Where the program comes from: on Windows the copy is this program
  // itself; elsewhere the technician points at `cretlink.exe` of a release.
  program: "this_program" | "choose";
  // The longest label: letters, digits and hyphens.
  labelMax: number;
  // The folder is named `folderPrefix` and the label; `programName` is the
  // name of the program in it.
  folderPrefix: string;
  programName: string;
}

// `choose_customer_program` and `choose_customer_place`: what the technician
// chose in the system's dialog, opened by the backend. The window names it
// by its number and never sends a path of this computer; `name` is the
// file's name or the folder's path, for the words. `null` when the dialog
// was closed.
export interface ChosenForCopy {
  pick: number;
  name: string;
}

// `generate_customer_key`: a new random admission key, drawn by the backend
// from the system's random source. The window shows it once, with the label,
// as the line `label:key` of the relay's key file; nothing keeps it but the
// copy that is made with it.
export interface GeneratedKey {
  key: string;
}

// `make_customer_copy`. The key is the one the technician typed or had
// generated for this customer; there is no way to ask for the technician's
// own key. `replace` is the technician's answer to "a folder of that name is
// there already".
export interface CustomerCopyForm {
  label: string;
  keyMode: "set" | "none";
  key?: string;
  programPick?: number;
  placePick?: number;
  replace?: boolean;
}

// The answer of `make_customer_copy`: the folder that was written and the
// names of its files. `copy` is the number `show_customer_copy` takes.
// `setAside` is the folder that was replaced, when it could not be removed
// and still lies beside the copy under another name.
export interface CustomerCopyMade {
  copy: number;
  folder: string;
  files: string[];
  setAside?: string;
}

// The `console-notice` event: something the technician should know that is
// not the failure of a command, with a next step.
export interface ConsoleNotice {
  title: string;
  message: string;
  next: string;
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
