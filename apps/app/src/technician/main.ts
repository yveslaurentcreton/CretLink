// Console frontend bootstrap (ST-M002-002, ADR-0011).
//
// The Rust backend owns the state; this module mounts the accessible skeleton,
// forwards user intents to the narrow command set, and re-renders from the
// `ConsoleState` snapshot each command returns.

import "./styles.css";
import { listen } from "@tauri-apps/api/event";
import {
  CONSOLE_ACTIVITY_EVENT,
  CONSOLE_STATE_EVENT,
  type ActivityLine,
  type ConsoleState,
  api,
  isCommandError,
} from "./api";
import { type Intent, mount, pushActivity, requestFocus, showError, update } from "./ui";

let current: ConsoleState | null = null;

// Monotonic counter bumped every time the live pump pushes a snapshot over the
// `"console-state"` event stream. `apply()` samples it around each command so a
// stale command return can never clobber a newer pushed snapshot: e.g.
// `confirm_code` returns the pre-confirm `awaiting_approval_code` view because
// the wire only advances when the compare-confirm echoes back through the pump —
// if that echo lands during the await, the pushed `awaiting_consent` view is
// authoritative and the command return is dropped (ST-M002-013).
let pushGeneration = 0;

async function run(): Promise<void> {
  const root = document.getElementById("app");
  if (!root) return;
  mount(root, handle);
  // The live session runs asynchronously in the backend and pushes each new
  // snapshot as a `"console-state"` event. Subscribe before the first render so
  // no live transition is missed.
  await listen<ConsoleState>(CONSOLE_STATE_EVENT, (event) => {
    pushGeneration += 1;
    current = event.payload;
    showError(null);
    update(current);
  });
  // Live activity streams on its own channel so high-frequency job output never
  // triggers a whole-state re-render (ST-M004-007). Each line is appended to the
  // Activity feed; the UI writes every string with `textContent`.
  await listen<ActivityLine>(CONSOLE_ACTIVITY_EVENT, (event) => {
    pushActivity(event.payload);
  });
  try {
    current = await api.getState();
    update(current);
  } catch (error) {
    showError(describe(error));
  }
}

function handle(intent: Intent): void {
  void apply(intent);
}

async function apply(intent: Intent): Promise<void> {
  const startGen = pushGeneration;
  try {
    const next = await invokeFor(intent);
    showError(null);
    // If the live pump pushed a snapshot while this command was in flight, that
    // snapshot reflects a later wire event than the command's return value.
    // Adopting the stale return would regress the UI, so keep the pushed one.
    if (pushGeneration === startGen) {
      current = next;
      update(next);
    }
    focusAfter(intent);
  } catch (error) {
    showError(describe(error));
  }
}

function invokeFor(intent: Intent): Promise<ConsoleState> {
  switch (intent.kind) {
    case "consent":
      return api.simulateConsent();
    case "lost":
      return api.simulateTargetLost();
    case "stop":
      return api.stopSession();
    case "start-live":
      return api.startLiveSession();
    case "compare":
      return api.confirmCode();
    case "mismatch":
      return api.reportCodeMismatch();
    default:
      return Promise.reject(new Error("unknown intent"));
  }
}

// Moves focus to the most relevant next control so the flow is keyboard-driven
// (REQ-N-014). Starting a live session and simulating consent both make Stop
// the natural next control, so focus lands on it (Escape stays reachable).
function focusAfter(intent: Intent): void {
  switch (intent.kind) {
    case "start-live":
    case "consent":
    case "compare":
    case "mismatch":
      // Confirming or rejecting the code both make Stop the natural next control:
      // on a match the session continues toward consent, on a mismatch it is torn
      // down. Stop stays reachable either way (disabled targets are skipped).
      requestFocus("action:stop");
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
