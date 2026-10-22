// Agent consent frontend bootstrap (ST-M002-005, ADR-0011).
//
// The Rust backend owns the state; this module mounts the accessible skeleton,
// forwards the customer's decision to the narrow command set, and re-renders
// from the `ConsentView` snapshot each command returns.

import "./styles.css";
import { listen } from "@tauri-apps/api/event";
import {
  AGENT_ACTIVITY_EVENT,
  AGENT_VIEW_EVENT,
  type ActivityDelta,
  api,
  isCommandError,
  type ConsentView,
} from "./api";
import {
  type Intent,
  mount,
  pushActivity,
  requestFocus,
  setDiscovery,
  showError,
  update,
} from "./ui";

let current: ConsentView | null = null;

// Monotonic counter bumped every time the live pump pushes a snapshot over the
// `"agent-view"` event stream. `apply()` samples it around each command so a
// stale command return can never clobber a newer pushed snapshot:
// e.g. `grant_consent` returns the pre-grant `awaiting_consent` view because the
// wire only advances when `ConsentGranted` echoes back through the pump — if that
// echo lands during the await, the pushed `full_control` view is authoritative
// and the command return is dropped (ST-M002-013).
let pushGeneration = 0;

async function run(): Promise<void> {
  const root = document.getElementById("app");
  if (!root) return;
  mount(root, handle);
  // A live dialed session runs asynchronously in the backend and pushes each new
  // snapshot as an `"agent-view"` event. Subscribe before the first render so no
  // live transition (pairing, consent echo, teardown) is missed.
  await listen<ConsentView>(AGENT_VIEW_EVENT, (event) => {
    pushGeneration += 1;
    current = event.payload;
    showError(null);
    update(current);
  });
  // The non-authoritative activity mirror rides its own channel so the
  // higher-frequency command stream never forces a whole-view re-render
  // (ST-M004-008). Each delta patches the feed in place; the feed is shown only
  // under full control and cleared by `update()` on any other phase, so it can
  // never outlive the session (fail-closed).
  await listen<ActivityDelta>(AGENT_ACTIVITY_EVENT, (event) => {
    pushActivity(event.payload);
  });
  try {
    current = await api.getView();
    update(current);
    // Land focus on the primary decision when one is pending.
    if (current.status.phase.phase === "awaiting_consent") {
      requestFocus("action:grant");
      update(current);
    } else if (current.can_connect) {
      // Kick a single LAN browse so the technician's console can appear on its
      // own (ST-M002-012); the customer can re-run it with "Search again".
      void browse();
    }
  } catch (error) {
    showError(describe(error));
  }
}

function handle(intent: Intent): void {
  // Browsing returns a transient candidate list, not a `ConsentView`, so it does
  // not flow through the snapshot-applying path below.
  if (intent.kind === "browse") {
    void browse();
    return;
  }
  void apply(intent);
}

// Runs one LAN discovery round-trip and feeds the transient candidate list to the
// view (ST-M002-012). The list is never merged into the authoritative
// `ConsentView`; a browsed console carries no authority until the dialed
// session's approval code is compared.
async function browse(): Promise<void> {
  setDiscovery("searching", []);
  try {
    const consoles = await api.browseConsoles();
    setDiscovery("done", consoles);
  } catch (error) {
    setDiscovery("done", []);
    showError(describe(error));
  }
}

async function apply(intent: Intent): Promise<void> {
  const startGen = pushGeneration;
  try {
    const next = await invokeFor(intent);
    showError(null);
    // If the live pump pushed a snapshot while this command was in flight, that
    // snapshot reflects a later wire event than the command's return value (which
    // is captured before the wire echoes back). Adopting the stale return would
    // regress the UI — e.g. flip a just-granted full-control view back to
    // "awaiting your decision". Keep the pushed snapshot instead.
    if (pushGeneration === startGen) {
      current = next;
      update(next);
    }
    focusAfter(intent, current ?? next);
  } catch (error) {
    showError(describe(error));
  }
}

function invokeFor(intent: Intent): Promise<ConsentView> {
  switch (intent.kind) {
    case "grant":
      return api.grantConsent();
    case "decline":
      return api.declineConsent();
    case "stop":
      return api.stopSession();
    case "simulate-stop":
      return api.simulateTechnicianStop();
    case "reset":
      return api.resetDemo();
    case "start-live":
      return api.startLiveSession(intent.code);
    case "connect-console":
      // A discovered candidate dials through the same joiner as a typed code.
      return api.connectConsole(intent.address);
    default:
      return Promise.reject(new Error("unknown intent"));
  }
}

// Moves focus to the most relevant next control so the flow is keyboard-driven
// (REQ-N-014).
function focusAfter(intent: Intent, next: ConsentView): void {
  switch (intent.kind) {
    case "grant":
      // Under control, the stop action is what matters most next.
      requestFocus("action:stop");
      break;
    case "reset":
      if (next.status.phase.phase === "awaiting_consent") {
        requestFocus("action:grant");
      }
      break;
    default:
      break;
  }
}

function describe(error: unknown): string {
  if (isCommandError(error)) {
    return `${error.stage}: ${error.message}`;
  }
  if (error instanceof Error) {
    return error.message;
  }
  return "Something went wrong. Try again.";
}

/** Starts this role's window; called by the bootstrap in `../main.ts` once the
 * backend has said which role this process runs (ST-M009-005). */
export function start(): Promise<void> {
  return run();
}
