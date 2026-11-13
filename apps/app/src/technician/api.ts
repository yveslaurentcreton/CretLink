// Typed bridge to the Rust backend (ADR-0011).
//
// These types mirror the `serde` types in `cretlink_console::state` and the
// GUI crate's `CommandError`. The Rust side is authoritative: the frontend
// renders whatever `ConsoleState` snapshot it is given and forwards intents
// through the narrow command set below. Keep these types in step with the Rust
// definitions when the state model changes.

import { invoke } from "@tauri-apps/api/core";
import type {
  ActivityEntry,
  AiClientStatus,
  AppInfo,
  AuditEntryView,
  BridgeStatus,
  ChosenFolder,
  ClientId,
  ConflictChoice,
  DeleteReport,
  ExplorerDrive,
  ExplorerEntryKind,
  ExplorerListing,
  ListenNetwork,
  SessionSummary,
  TransferCheck,
  TransferEntry,
  TransferKind,
  TransferQueued,
  UploadOffer,
} from "../shared/contract";

export type DiscoverySource = "discovered" | "manual" | "simulated";

export type ControlLevel = "none" | "connected" | "full_control";

export type StopReason =
  | "technician_requested"
  | "customer_requested"
  | "target_stale"
  | "disconnected";

export interface TargetIdentity {
  hostname: string;
  os_version: string;
  arch: string;
  elevated: boolean;
  simulated: boolean;
  agent_version: string;
  // The account the customer's agent runs as (`DOMAIN\user`), protocol
  // minor 11; absent when the agent could not read it (ST-M009-008).
  account?: string;
}

export interface TechnicianHost {
  label: string;
  hostname: string;
  os: string;
  // The technician's display name from the operating-system account, which
  // the customer sees in the discovery list (ST-M009-011). Optional until the
  // backend sends it; the interface falls back to the hostname.
  display_name?: string;
}

export interface DiscoveredTarget {
  id: string;
  display_name: string;
  address: string;
  source: DiscoverySource;
  stale: boolean;
  identity?: TargetIdentity;
}

// The internally-tagged `SessionPhase` enum from Rust (`{ "phase": "..." }`).
export type SessionPhase =
  | { phase: "idle" }
  | { phase: "listening"; code: string; address: string }
  | { phase: "discovered" }
  | { phase: "connecting" }
  | { phase: "awaiting_approval_code"; code: string }
  | { phase: "awaiting_consent" }
  | { phase: "full_control" }
  | { phase: "stopping" }
  | { phase: "stopped"; reason: StopReason }
  | { phase: "failed"; stage: string; message: string };

export interface ConsoleState {
  technician: TechnicianHost;
  targets: DiscoveredTarget[];
  selected?: string;
  session?: string;
  phase: SessionPhase;
}

// The interactive terminal handle returned by `open_terminal`, mirroring the
// Rust `TerminalInfo` (camelCase). `term` is the opaque job id addressing this
// terminal for input/resize/close; `session` is the consented session id for
// display. The console GUI is the host, so the panel attaches in-process — no
// IPC round-trip (ST-M008-001, builds on ST-M004-007).
export interface TerminalInfo {
  term: number;
  session: string;
}

// One streamed terminal frame on the `"console-terminal"` event, mirroring the
// Rust `TerminalFrame` (camelCase). `output` carries raw VT bytes as a JSON
// number array rebuilt into a `Uint8Array` and written to xterm; the terminal
// ends honestly on `exited`/`revoked`, and `dropped` surfaces an overrun
// consumer rather than a silent stall (REQ-N-008).
export interface TerminalFrame {
  term: number;
  kind: "output" | "exited" | "revoked" | "dropped";
  data?: number[];
  exitCode?: number;
  outcome?: string;
}

export interface CommandError {
  stage: string;
  message: string;
}

// A selected entry of the open folder, for a copy to this computer: its
// name, and what the listing showed it as. The backend finds out what it is
// from a listing of its own.
export interface TransferName {
  name: string;
  kind: TransferKind;
}

// A selected entry of the open folder, for a delete: its name and the kind
// the listing gave, which chooses how it is removed.
export interface DeleteItem {
  name: string;
  kind: ExplorerEntryKind;
}

export function isCommandError(value: unknown): value is CommandError {
  return (
    typeof value === "object" &&
    value !== null &&
    "stage" in value &&
    "message" in value
  );
}

export const api = {
  getState: () => invoke<ConsoleState>("get_state"),
  simulateConsent: () => invoke<ConsoleState>("simulate_consent"),
  simulateTargetLost: () => invoke<ConsoleState>("simulate_target_lost"),
  stopSession: () => invoke<ConsoleState>("stop_session"),
  // The technician's Session Approval Code comparison verdict (REQ-S-006,
  // REQ-S-009, ST-M002-013). Both are valid only while the code is on screen
  // (the `awaiting_approval_code` phase *is* the compare window). In a live
  // session they release/tear the gate over the wire; the model advances only
  // when the wire echoes back, so neither fabricates control locally.
  confirmCode: () => invoke<ConsoleState>("confirm_code"),
  reportCodeMismatch: () => invoke<ConsoleState>("report_code_mismatch"),
  // Opens a real LAN listener and drives the live session (ST-M002-009). The
  // returned snapshot is the pre-listen state; live progress then arrives as
  // `"console-state"` events (see `main.ts`), never as the return value here.
  startLiveSession: () => invoke<ConsoleState>("start_live_session"),
  // The networks this computer can wait on, and the move of the waiting
  // session to another one (ST-M009-016). The move answers with the new
  // waiting state: another address and a new connection code.
  listenNetworks: () => invoke<ListenNetwork[]>("get_listen_networks"),
  switchListenNetwork: (address: string) =>
    invoke<ConsoleState>("switch_listen_network", { address }),
  // Interactive terminal panel (ST-M008-001). All four fail closed with
  // `no_consented_session` when no session is under full control — the panel
  // never fabricates a shell. `open_terminal` streams output on the
  // `"console-terminal"` event; keystrokes go back as raw UTF-8 bytes so the
  // remote pty (not the frontend) owns the echo. Param names are single-word,
  // so they match the Rust signatures with no case ambiguity.
  openTerminal: (cols: number, rows: number, shell?: string) =>
    invoke<TerminalInfo>("open_terminal", { cols, rows, shell }),
  termInput: (term: number, data: number[]) =>
    invoke<void>("term_input", { term, data }),
  termResize: (term: number, cols: number, rows: number) =>
    invoke<void>("term_resize", { term, cols, rows }),
  termClose: (term: number) => invoke<void>("term_close", { term }),
  // The file explorer of the live session (ST-M010-002). Both fail closed
  // with `no_consented_session` outside full control. `explorer_open` takes
  // a path on the customer computer and a number for this listing; the folder
  // arrives on the `"console-explorer"` event, and the answer says how the
  // listing ended. A place that is refused rejects, and nothing arrives.
  explorerDrives: () => invoke<ExplorerDrive[]>("explorer_drives"),
  explorerOpen: (path: string, listing: number) =>
    invoke<ExplorerListing>("explorer_open", { path, listing }),
  // What the explorer sends, copies, renames and deletes (ST-M010-002). All
  // fail closed with `no_consented_session` outside full control, except the
  // four that only look at what the backend holds: the queue, a cancel, and
  // the folder of a copy. A folder is a path on the customer computer; no
  // command takes a path of this computer: files to send are named by the
  // number of an offer, and a folder to copy to by the number it was chosen
  // under.
  explorerPickUpload: (kind: "files" | "folder") =>
    invoke<UploadOffer | null>("explorer_pick_upload", { kind }),
  explorerSendCheck: (offer: number, folder: string) =>
    invoke<TransferCheck>("explorer_send_check", { offer, folder }),
  explorerSend: (offer: number, folder: string, choices: Record<string, ConflictChoice>) =>
    invoke<TransferQueued>("explorer_send", { offer, folder, choices }),
  explorerChooseFolder: () => invoke<ChosenFolder | null>("explorer_choose_folder"),
  explorerDownloadCheck: (names: TransferName[], to?: number) =>
    invoke<TransferCheck>("explorer_download_check", to === undefined ? { names } : { names, to }),
  explorerDownload: (
    folder: string,
    names: TransferName[],
    to: number | undefined,
    choices: Record<string, ConflictChoice>,
  ) =>
    invoke<TransferQueued>(
      "explorer_download",
      to === undefined ? { folder, names, choices } : { folder, names, to, choices },
    ),
  explorerTransfers: () => invoke<TransferEntry[]>("explorer_transfers"),
  explorerCancelTransfer: (transfer: number) =>
    invoke<void>("explorer_cancel_transfer", { transfer }),
  explorerCancelTransfers: () => invoke<void>("explorer_cancel_transfers"),
  explorerShowFolder: (transfer: number) => invoke<void>("explorer_show_folder", { transfer }),
  explorerRename: (folder: string, name: string, to: string) =>
    invoke<void>("explorer_rename", { folder, name, to }),
  explorerDelete: (folder: string, items: DeleteItem[], removal: number) =>
    invoke<DeleteReport>("explorer_delete", { folder, items, removal }),
  explorerCancelDelete: (removal: number) =>
    invoke<void>("explorer_cancel_delete", { removal }),

  // The interfaces of ST-M009-008, ST-M009-006 and ST-M009-012. A command
  // the backend does not offer yet rejects; the controller then shows the
  // data as unavailable instead of failing.
  getAppInfo: () => invoke<AppInfo>("get_app_info"),
  getActivity: () => invoke<ActivityEntry[]>("get_activity"),
  // The local MCP bridge for the rail (ST-M009-008); changes arrive as
  // `console-bridge` events.
  getBridgeStatus: () => invoke<BridgeStatus>("get_bridge_status"),
  // Newest first, within the audit retention.
  listSessions: () => invoke<SessionSummary[]>("list_sessions"),
  // This session's audit entries, oldest first (the Live tab Audit).
  getSessionAudit: () => invoke<AuditEntryView[]>("get_session_audit"),
  aiClients: () => invoke<AiClientStatus[]>("ai_clients"),
  aiClientInstall: (client: ClientId) =>
    invoke<AiClientStatus[]>("ai_client_install", { client }),
  aiClientUninstall: (client: ClientId) =>
    invoke<AiClientStatus[]>("ai_client_uninstall", { client }),
  // Without a session: opens the audit folder; with one: that session's file.
  openAudit: (session?: string) =>
    invoke<void>("open_audit", session === undefined ? {} : { session }),
  uninstallPlan: () => invoke<string[]>("uninstall_plan"),
  uninstallApp: () => invoke<void>("uninstall_app"),
};

// The Tauri event name the backend emits a whole `ConsoleState` on during a
// live session. Both sides must agree on this exact string.
export const CONSOLE_STATE_EVENT = "console-state";

// A separate channel for live activity (ST-M004-007). Kept distinct from
// `"console-state"` so streaming job output never forces a whole-state
// re-render. Each event carries one `ActivityEvent` of the shared contract
// (ST-M009-011): an item, upserted by id, or a gap marker.
export const CONSOLE_ACTIVITY_EVENT = "console-activity";

// The local MCP bridge's status for the rail (ST-M009-008): one
// `BridgeStatus` of the shared contract per change.
export const CONSOLE_BRIDGE_EVENT = "console-bridge";

// The pages of a folder the file explorer opened (ST-M010-002): one
// `ExplorerEvent` of the shared contract per step of a listing.
export const CONSOLE_EXPLORER_EVENT = "console-explorer";

// What the technician dropped on the window (ST-M010-002): one `UploadOffer`
// of the shared contract. The paths stay in the backend.
export const CONSOLE_EXPLORER_DROP_EVENT = "console-explorer-drop";

// The transfer queue's changes (ST-M010-002): one `TransfersEvent` each.
export const CONSOLE_TRANSFERS_EVENT = "console-transfers";

// Where a delete stands (ST-M010-002): one `RemovalProgress` each.
export const CONSOLE_REMOVAL_EVENT = "console-removal";

// The raw terminal frame channel (ST-M008-001). Distinct again so high-frequency
// VT output never re-renders state or the activity feed; the terminal panel
// subscribes to this itself and filters frames by its own `term` id.
export const CONSOLE_TERMINAL_EVENT = "console-terminal";

// A control level derived from the phase, mirroring `ConsoleState::control_level`
// so the view can label authority without a second round-trip.
export function controlLevel(state: ConsoleState): ControlLevel {
  switch (state.phase.phase) {
    case "full_control":
      return "full_control";
    case "connecting":
    case "awaiting_approval_code":
    case "awaiting_consent":
    case "stopping":
      return "connected";
    default:
      return "none";
  }
}

export function selectedTarget(state: ConsoleState): DiscoveredTarget | undefined {
  if (!state.selected) return undefined;
  return state.targets.find((t) => t.id === state.selected);
}

// Whether the selected target is driven by the simulator rather than a real
// customer computer. The simulate-* affordances are gated on this: firing them
// during a live session would fabricate local control that then collides with
// the real wire events, so they must appear only for a simulated target.
export function selectedIsSimulated(state: ConsoleState): boolean {
  const target = selectedTarget(state);
  if (!target) return false;
  return target.source === "simulated" || target.identity?.simulated === true;
}

// Whether the current phase represents a live session that stop should act on.
// Listening counts: the console has bound a real LAN listener, so Stop must be
// able to close it before any customer arrives (ST-M002-012).
export function sessionIsActive(state: ConsoleState): boolean {
  switch (state.phase.phase) {
    case "listening":
    case "connecting":
    case "awaiting_approval_code":
    case "awaiting_consent":
    case "full_control":
    case "stopping":
      return true;
    default:
      return false;
  }
}
