// Typed bridge to the Rust backend (ADR-0011).
//
// These types mirror the `serde` types in `cretlink_support_agent` (its
// `state` and `privilege` modules) and the GUI crate's `ConsentView` /
// `CommandError`. The Rust side is authoritative: the frontend renders whatever
// `ConsentView` snapshot it is given and forwards the customer's decision
// through the narrow command set below. Keep these types in step with the Rust
// definitions when the state model changes.

import { invoke } from "@tauri-apps/api/core";

// Mirrors `cretlink_support_agent::privilege::IntegrityLevel` (snake_case).
export type IntegrityLevel =
  | "untrusted"
  | "low"
  | "medium"
  | "high"
  | "system"
  | "unknown";

// Mirrors `cretlink_support_agent::privilege::EffectiveIdentity`.
export interface EffectiveIdentity {
  username: string;
  domain: string | null;
  elevated: boolean;
  integrity: IntegrityLevel;
  is_windows: boolean;
  note: string | null;
}

// Mirrors `cretlink_support_agent::state::StopReason` (snake_case).
export type StopReason =
  | "technician_requested"
  | "customer_requested"
  | "disconnected"
  | "failed";

// The internally-tagged `AgentPhase` enum from Rust (`{ "phase": "..." }`).
export type AgentPhase =
  | { phase: "idle" }
  | { phase: "announced" }
  | { phase: "pairing" }
  // The wire's held compare gate: the customer and technician read the same
  // Session Approval Code aloud before consent. The live dialed session parks
  // here (not `pairing`, which is the momentary post-connect step) until the
  // technician confirms the codes match (ST-M002-013).
  | { phase: "awaiting_approval_compare" }
  | { phase: "awaiting_consent" }
  | { phase: "full_control"; session: string }
  | { phase: "stopping" }
  | { phase: "stopped"; reason: StopReason }
  | { phase: "failed"; stage: string; message: string };

// Mirrors `cretlink_support_agent::state::VisibleStatus`.
export interface VisibleStatus {
  phase: AgentPhase;
  identity: EffectiveIdentity;
  control_active: boolean;
}

// Mirrors the GUI crate's `ConsentView`.
export interface ConsentView {
  status: VisibleStatus;
  approval_code: string | null;
  disclosure: string;
  simulated: boolean;
  can_connect: boolean;
}

// Mirrors the GUI crate's `DiscoveredConsoleView`: one technician console found
// advertising itself on the LAN (ST-M002-012). This is a transient,
// unauthenticated candidate — never preselected or dialed automatically. Trust
// only ever comes from comparing the Session Approval Code after dialing.
export interface DiscoveredConsoleView {
  instance_name: string;
  display_name: string | null;
  address: string;
  protocol_version: string | null;
  dialable: boolean;
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
  getView: () => invoke<ConsentView>("get_view"),
  grantConsent: () => invoke<ConsentView>("grant_consent"),
  declineConsent: () => invoke<ConsentView>("decline_consent"),
  stopSession: () => invoke<ConsentView>("stop_session"),
  simulateTechnicianStop: () => invoke<ConsentView>("simulate_technician_stop"),
  resetDemo: () => invoke<ConsentView>("reset_demo"),
  // Dials a real listening technician console using the connection code the
  // technician reads out (ST-M002-009). The returned snapshot is the "reaching
  // out" state; live progress then arrives as `"agent-view"` events (see
  // `main.ts`), never as the return value here.
  startLiveSession: (code: string) =>
    invoke<ConsentView>("start_live_session", { code }),
  // Browses the local network for advertising technician consoles (ST-M002-012).
  // Returns a transient candidate list; it is not part of the authoritative
  // `ConsentView` because it carries no session authority.
  browseConsoles: () => invoke<DiscoveredConsoleView[]>("browse_consoles"),
  // Dials a console discovered on the LAN. `address` is the candidate's
  // "ip:port", which funnels through the same `SessionJoiner::join` as a typed
  // connection code — selecting a candidate grants no authority by itself.
  connectConsole: (address: string) =>
    invoke<ConsentView>("connect_console", { address }),
};

// The Tauri event name the backend emits a whole `ConsentView` on during a live
// session. Both sides must agree on this exact string.
export const AGENT_VIEW_EVENT = "agent-view";

// One non-authoritative activity delta, mirroring
// `cretlink_agent_gui::session_driver::ActivityDelta` (which serialises
// `camelCase`). It is the customer-visible mirror of exactly what the customer
// side of `cretlink-session` emits: a command starting (`started`, carrying the
// command text, shell label, and `origin` attribution) and a command ending
// (`finished`, carrying the exit code, honest outcome, and success flag). There
// is deliberately no per-line output or duration here — those are console-side
// only — so the mirror can never fabricate activity the customer never saw
// (REQ-F-017, REQ-S-002). Every string is backend-built; the frontend writes it
// with `textContent`, never as HTML (REQ-S-016, REQ-S-018).
export interface ActivityDelta {
  job: number;
  kind: "started" | "finished";
  command?: string;
  shell?: string;
  origin?: "console" | "mcp" | "terminal";
  exitCode?: number;
  outcome?: string;
  ok?: boolean;
}

// A separate channel for live activity deltas (ST-M004-008). Kept distinct from
// `"agent-view"` so the higher-frequency command stream never forces a whole
// `ConsentView` re-render. The backend emits one `ActivityDelta` per event.
export const AGENT_ACTIVITY_EVENT = "agent-activity";

// A coarse control level derived from the visible status, so the view can label
// authority without a second round-trip. `control_active` is authoritative for
// "full"; the pre-decision phases render as "pending".
export type ControlLevel = "none" | "pending" | "full";

export function controlLevel(view: ConsentView): ControlLevel {
  if (view.status.control_active) return "full";
  switch (view.status.phase.phase) {
    case "announced":
    case "pairing":
    case "awaiting_approval_compare":
    case "awaiting_consent":
    case "stopping":
      return "pending";
    default:
      return "none";
  }
}

// Whether the phase is a live session that a stop should still act on. Terminal
// phases (stopped, failed) are not.
export function sessionIsLive(view: ConsentView): boolean {
  switch (view.status.phase.phase) {
    case "stopped":
    case "failed":
      return false;
    default:
      return true;
  }
}

// Whether the customer is currently being asked to decide.
export function awaitingConsent(view: ConsentView): boolean {
  return view.status.phase.phase === "awaiting_consent";
}
