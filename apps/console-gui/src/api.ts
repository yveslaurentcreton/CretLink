// Typed bridge to the Rust backend (ADR-0011).
//
// These types mirror the `serde` types in `cretlink_console::state` and the
// GUI crate's `CommandError`. The Rust side is authoritative: the frontend
// renders whatever `ConsoleState` snapshot it is given and forwards intents
// through the narrow command set below. Keep these types in step with the Rust
// definitions when the state model changes.

import { invoke } from "@tauri-apps/api/core";

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
}

export interface TechnicianHost {
  label: string;
  hostname: string;
  os: string;
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

// One live activity line, mirroring `cretlink_console_gui::session_driver::ActivityLine`
// (which serialises `camelCase`). The backend builds every string; the frontend
// writes it with `textContent`, never as HTML, because job output and paths are
// untrusted remote input (REQ-S-016, REQ-S-018).
export interface ActivityLine {
  source: "claude" | "session" | "note";
  kind: "start" | "finished" | "dropped" | "desktop" | "note";
  text: string;
  job?: number;
  exitCode?: number;
  ok?: boolean;
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
};

// The Tauri event name the backend emits a whole `ConsoleState` on during a
// live session. Both sides must agree on this exact string.
export const CONSOLE_STATE_EVENT = "console-state";

// A separate channel for live activity lines (ST-M004-007). Kept distinct from
// `"console-state"` so streaming job output never forces a whole-state
// re-render. The backend emits one `ActivityLine` per event.
export const CONSOLE_ACTIVITY_EVENT = "console-activity";

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
