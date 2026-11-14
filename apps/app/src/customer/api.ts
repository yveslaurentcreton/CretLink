// Typed bridge to the customer role's Rust backend (ADR-0011, ST-M009-009).
//
// These types mirror the `serde` types of the customer module of the
// application crate (`ConsentView`, `DiscoveredConsoleView`, `LaunchView`,
// `RelaunchOutcome`, `CommandError`) and of `cretlink_support_agent` (its
// `state` and `privilege` modules). The Rust side is authoritative: the
// frontend renders whatever snapshot it is given and forwards the customer's
// decisions through the narrow command set below. The activity feed and the
// end report use the shared contract (`../shared/contract`).

import { invoke } from "@tauri-apps/api/core";
import type { ActivityEntry, CustomerReport, RelayTrust } from "../shared/contract";

// Mirrors `cretlink_support_agent::privilege::IntegrityLevel` (snake_case).
export type IntegrityLevel = "untrusted" | "low" | "medium" | "high" | "system" | "unknown";

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
  // The wire's held compare gate: both sides read the same Session Approval
  // Code aloud before consent (ST-M002-013).
  | { phase: "awaiting_approval_compare" }
  | { phase: "awaiting_consent" }
  | { phase: "full_control"; session: string }
  | { phase: "stopping" }
  | { phase: "stopped"; reason: StopReason }
  | { phase: "failed"; stage: string; message: string };

export type PhaseName = AgentPhase["phase"];

// Mirrors `cretlink_support_agent::state::VisibleStatus`.
export interface VisibleStatus {
  phase: AgentPhase;
  identity: EffectiveIdentity;
  control_active: boolean;
}

// The technician's display name from the pairing handshake: what their
// computer says, never a verified identity (ST-M009-011).
export interface TechnicianNameView {
  display_name: string;
  provenance: string;
}

// When the screen was last observed or controlled, Unix ms, from the agent's
// `DesktopActivity` signals (ST-M009-009).
export interface ScreenUseView {
  observed_at_ms: number | null;
  controlled_at_ms: number | null;
}

// Files from an earlier session that the startup sweep could not remove, with
// the command that lists and removes them (ST-M009-010). Mirrors
// `StartupCleanupView`.
export interface StartupCleanupView {
  failed: number;
  // Runnable as written in Windows PowerShell, with the full path:
  // `& '…\cretlink-cli.exe' recover --remove` when the console build sits
  // next to this program, else `& '…\<this program>.exe' recover --remove |
  // Out-Host` (a GUI program PowerShell waits for only when its output is piped).
  command: string;
  // Whether PowerShell is to be opened as administrator for it: what was kept
  // lies where only administrators can write. Otherwise it lies in the folder
  // of this account, where a run as administrator does not look (ST-M006-020).
  administrator: boolean;
}

// Mirrors the application crate's `ConsentView`.
export interface ConsentView {
  status: VisibleStatus;
  // This computer's host name.
  hostname: string;
  // Whether a session exists: Stop, Escape, the compact bar and the close
  // question exist only then (ST-M009-009).
  session_active: boolean;
  // When full control was granted, Unix ms.
  control_since_ms: number | null;
  screen: ScreenUseView;
  approval_code: string | null;
  // `DisclosureSummary::DEFAULT_TEXT`, verbatim (REQ-S-016).
  disclosure: string;
  simulated: boolean;
  can_connect: boolean;
  technician: TechnicianNameView | null;
  // `null` when the startup sweep kept nothing (or has not finished).
  startup_cleanup: StartupCleanupView | null;
  // The relay this session runs through: its name, where this copy connects
  // and what vouches for the name; `null` for a session on one network
  // (ST-M007-013). It stays after the end, until the next dial.
  relay: RelayUseView | null;
  // What to do next about the failure this dial ended with, when the failure
  // has a next step of its own (a relay that cannot be reached, a key the
  // relay refuses, ...); `null` otherwise.
  failure_next: string | null;
  // The relay this copy is set up for, from the profile file next to the
  // program; `null` without a profile that can be used (ST-M007-011). Shown
  // beside the code field: the customer sees the relay's name, where this
  // copy would connect and what vouches for the name before connecting
  // through it.
  relay_of_copy: RelayUseView | null;
}

// The relay of a session (ST-M007-013, ADR-0016): a label. The relay passes
// the encrypted session on, cannot look inside and grants nothing.
//
// `host` is the name the relay's certificate carries, `address` is where
// this copy connects, and `trust` is what vouches for the name. The window
// shows the address beside the name when its host is another one, and says
// when the certificate is checked against a file of the profile
// (`shared/relay.ts`): whoever wrote the profile file chose both.
export interface RelayUseView {
  host: string;
  address: string;
  trust: RelayTrust;
}

// One technician console found on the LAN (ADR-0007). Untrusted: never
// preselected or dialed automatically; trust comes only from comparing the
// Session Approval Code after dialing.
export interface DiscoveredConsoleView {
  instance_name: string;
  display_name: string | null;
  address: string;
  protocol_version: string | null;
  dialable: boolean;
}

// The admin step before the UAC prompt (`#c-admin`, ST-M009-009).
export interface LaunchView {
  // `admin`: explain the prompt; `declined`: the customer declined it;
  // `ready`: past the step.
  step: "admin" | "declined" | "ready";
  // Where Back goes: the start screen, or close this copy.
  back: "start" | "close";
  elevated: boolean;
  // Whether Continue shows the real UAC prompt. `false` only in a
  // development build off Windows, where it is simulated.
  uac: boolean;
  // Whether this is a development build with the simulated demo.
  demo: boolean;
}

export interface RelaunchOutcome {
  outcome: "started" | "declined" | "failed";
  message: string;
  launch: LaunchView;
}

// --- Keeping the support program for next time (ST-M009-017) ---

// How this copy relates to what is kept on this computer.
export type KeepRelation =
  | "not_kept"
  // This copy is the kept program.
  | "this"
  // The kept program has the version of this copy.
  | "same"
  // This copy is newer: it can update the kept program.
  | "newer"
  // The kept program is newer: this copy never replaces it.
  | "older"
  // The kept version cannot be compared: it is not replaced.
  | "unknown";

// Mirrors the application crate's `KeepView`: what is kept on this computer,
// and what this window offers. The backend decides the offer: to keep only
// with administrator rights, to remove only in the kept program itself, and
// nothing during a session.
export interface KeepView {
  state: KeepRelation;
  canKeep: boolean;
  canRemove: boolean;
  // Removing first asks Windows for administrator rights.
  removeElevates: boolean;
  thisVersion: string;
  // Absent when nothing is kept or the record does not say.
  keptVersion?: string;
  // The folder of the kept program; absent where no program can be kept.
  location?: string;
}

// Mirrors `KeepPlanView`: what keeping would add, shown before the customer
// chooses. `relay` names the relay that the settings of this copy bring, as
// every screen names a relay: its name, where the copy connects and what
// vouches for the name. The settings may have been written by somebody else.
export interface KeepPlanView {
  version: string;
  // The version it would replace, when a program is kept already.
  replaces?: string;
  location: string;
  program: string;
  // The kept program has a desktop shortcut already.
  desktopKept: boolean;
  relay?: RelayUseView;
  // Relay settings that are kept already would be replaced.
  relayReplaces: boolean;
  // Why the relay settings next to this copy are not kept, as a sentence.
  relayProblem?: string;
}

// Mirrors `RemovePlanView`: what removing would remove and what stays.
export interface RemovePlanView {
  removes: string[];
  stays: string[];
  // The folder goes only once this window has closed.
  afterExit: boolean;
  // Windows asks for administrator rights first.
  elevates: boolean;
}

// Mirrors `RemoveOutcome`.
export interface RemoveOutcome {
  // `removed`: everything is gone; `pending`: the folder goes once this
  // window has closed; `elevating`: Windows was asked for administrator
  // rights and this window is closing.
  state: "removed" | "pending" | "elevating";
  left: string[];
}

export interface CommandError {
  stage: string;
  message: string;
}

export function isCommandError(value: unknown): value is CommandError {
  return typeof value === "object" && value !== null && "stage" in value && "message" in value;
}

export const api = {
  getView: () => invoke<ConsentView>("get_view"),
  getActivity: () => invoke<ActivityEntry[]>("get_activity"),
  getLaunch: () => invoke<LaunchView>("get_launch"),
  relaunchElevated: () => invoke<RelaunchOutcome>("relaunch_elevated"),
  continueWithoutAdmin: () => invoke<LaunchView>("continue_without_admin"),
  goBack: () => invoke<void>("go_back"),
  grantConsent: () => invoke<ConsentView>("grant_consent"),
  declineConsent: () => invoke<ConsentView>("decline_consent"),
  stopSession: () => invoke<ConsentView>("stop_session"),
  // Dev-only commands (`dev-simulate`); a release build does not register them.
  simulateTechnicianStop: () => invoke<ConsentView>("simulate_technician_stop"),
  resetDemo: () => invoke<ConsentView>("reset_demo"),
  // Dials the technician from the connection code they read out. Live
  // progress then arrives as `agent-view` events.
  startLiveSession: (code: string) => invoke<ConsentView>("start_live_session", { code }),
  browseConsoles: () => invoke<DiscoveredConsoleView[]>("browse_consoles"),
  // Dials a picked console through the same path as a typed code.
  connectConsole: (address: string) => invoke<ConsentView>("connect_console", { address }),
  getEndReport: () => invoke<CustomerReport | null>("get_end_report"),
  saveReport: () => invoke<string | null>("save_report"),
  openSavedReport: (path: string) => invoke<void>("open_saved_report", { path }),
  makeSmall: () => invoke<void>("make_small"),
  openMain: () => invoke<void>("open_main"),
  dragWindow: () => invoke<void>("drag_window"),
  stopAndClose: () => invoke<void>("stop_and_close"),
  // Keeping the support program (ST-M009-017). Nothing is kept or removed
  // without the customer's click: `keepProgram` carries out the plan that
  // `keepPlan` showed, and no other.
  getKeep: () => invoke<KeepView>("get_keep"),
  keepPlan: () => invoke<KeepPlanView>("keep_plan"),
  keepProgram: (desktop: boolean) => invoke<KeepView>("keep_program", { desktop }),
  removePlan: () => invoke<RemovePlanView>("remove_plan"),
  removeProgram: () => invoke<RemoveOutcome>("remove_program"),
};

// The events of the backend, to both windows.
export const AGENT_VIEW_EVENT = "agent-view";
export const AGENT_ACTIVITY_EVENT = "agent-activity";
// To the main window, when a close or an exit was held because a session
// exists.
export const CLOSE_REQUESTED_EVENT = "customer-close-requested";
