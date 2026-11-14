// Development-only page for keeping the support program (ST-M009-017): the
// real customer controller and views, driven against a computer that is
// held in memory instead of a backend. Tauri's own mocks answer the
// commands, so nothing here is read, written, kept or removed.
//
// `?copy=` picks the copy that runs, and what is kept on the computer:
//
// | Value             | The copy |
// |-------------------|----------|
// | `portable`        | a downloaded copy with administrator rights; nothing is kept (the default) |
// | `update`          | version 1.1.0, while version 1.0.0 is kept |
// | `same`            | the version that is kept already |
// | `older`           | version 1.0.0, while version 1.1.0 is kept |
// | `kept`            | the kept program itself, with administrator rights |
// | `unelevated`      | a downloaded copy that went on without administrator rights |
// | `kept-unelevated` | the kept program without administrator rights |
//
// `?relay=` picks the relay settings next to the copy (`none`, `plain`,
// `renamed`: named otherwise than the host it connects to and checked
// against a certificate file of its own, `problem`: settings that cannot be
// used). `?fail=` picks what the computer refuses: `plan`, `keep` (the kept
// program is running), `remove`, `declined` (the elevation prompt).
//
// A technician is nearby, so a session can be started and stopped: the page
// shows that nothing about keeping is offered while one exists, and that the
// end report offers it again. What the computer holds is written under the
// window, so that a person, or a test with real input, can see what a click
// changed.

import "../src/base.css";
import { emit } from "@tauri-apps/api/event";
import { mockIPC, mockWindows } from "@tauri-apps/api/mocks";
import type {
  ConsentView,
  KeepPlanView,
  KeepRelation,
  KeepView,
  RemoveOutcome,
  RemovePlanView,
} from "../src/customer/api";
import {
  CONSOLES,
  KEPT_FOLDER,
  LAUNCH_READY,
  REPORT,
  VIA_RELAY,
  VIA_RELAY_ELSEWHERE,
  viewIn,
} from "../src/customer/fixtures";
import { start } from "../src/customer/main";

const params = new URLSearchParams(window.location.search);
const copy = params.get("copy") ?? "portable";
const relayChoice = params.get("relay") ?? "none";
const fail = params.get("fail") ?? "";

const elevated = copy !== "unelevated" && copy !== "kept-unelevated";
const isKeptProgram = copy === "kept" || copy === "kept-unelevated";
const thisVersion = copy === "older" ? "1.0.0" : "1.1.0";

/** What the computer holds of a kept program. */
interface Kept {
  version: string;
  desktop: boolean;
  relay: string | null;
}

let kept: Kept | null =
  copy === "update"
    ? { version: "1.0.0", desktop: true, relay: null }
    : copy === "same" || isKeptProgram || copy === "older"
      ? { version: "1.1.0", desktop: false, relay: null }
      : null;
/** The plan that was shown last: only that one is carried out. */
let shown: KeepPlanView | null = null;
/** Whether the kept program's folder waits for this window to close. */
let pendingRemoval = false;
let closed = false;
let view: ConsentView = viewIn({ phase: "idle" });
if (!elevated) {
  view = {
    ...view,
    status: { ...view.status, identity: { ...view.status.identity, elevated: false, integrity: "medium" } },
  };
}

/** A refusal as the backend words one. */
function refuse(stage: string, message: string): never {
  throw { stage, message };
}

function sessionExists(): boolean {
  return view.session_active;
}

function relation(): KeepRelation {
  if (!kept) return "not_kept";
  if (isKeptProgram) return "this";
  if (kept.version === thisVersion) return "same";
  return thisVersion > kept.version ? "newer" : "older";
}

function keepView(): KeepView {
  const state = relation();
  const session = sessionExists();
  const canRemove = !session && state === "this";
  return {
    state,
    canKeep: !session && elevated && (state === "not_kept" || state === "newer"),
    canRemove,
    removeElevates: canRemove && !elevated,
    thisVersion,
    ...(kept ? { keptVersion: kept.version } : {}),
    location: KEPT_FOLDER,
  };
}

function ensureIdle(): void {
  if (sessionExists()) {
    refuse(
      "keep_session",
      "A session is in progress. Keeping or removing the program is offered before a session and after it, never during one.",
    );
  }
}

function keepPlan(): KeepPlanView {
  ensureIdle();
  shown = null;
  if (!elevated || fail === "plan") {
    refuse("keep_administrator", "This needs administrator rights, and this window runs without them.");
  }
  const state = relation();
  if (state !== "not_kept" && state !== "newer") {
    refuse("keep_older", "The support program that is kept on this computer is not older than this copy; it is not replaced.");
  }
  const relay =
    relayChoice === "plain" ? VIA_RELAY : relayChoice === "renamed" ? VIA_RELAY_ELSEWHERE : undefined;
  shown = {
    version: thisVersion,
    ...(kept ? { replaces: kept.version } : {}),
    location: KEPT_FOLDER,
    program: `${KEPT_FOLDER}\\CretLink-Support.exe`,
    desktopKept: kept?.desktop ?? false,
    ...(relay ? { relay } : {}),
    relayReplaces: relay !== undefined && kept?.relay != null,
    ...(relayChoice === "problem"
      ? {
          relayProblem:
            "Line 2: `address` needs a port; write the relay's host name and port, like relay.example.org:443. The file is C:\\Users\\ann\\Downloads\\cretlink.toml.",
        }
      : {}),
  };
  return shown;
}

function keepProgram(desktop: boolean): KeepView {
  ensureIdle();
  const plan = shown;
  shown = null;
  if (!plan) refuse("keep_plan", "Look at what keeping adds first; then choose to keep the program.");
  if (fail === "keep") {
    refuse(
      "keep_running",
      `The kept support program is running (${KEPT_FOLDER}\\CretLink-Support.exe). Close it, then try again.`,
    );
  }
  kept = { version: plan.version, desktop, relay: plan.relay?.host ?? kept?.relay ?? null };
  return keepView();
}

function removePlan(): RemovePlanView {
  ensureIdle();
  if (!kept) refuse("keep_absent", "CretLink Support is not kept on this computer.");
  const after = isKeptProgram ? " (removed once CretLink has closed)" : "";
  return {
    removes: [
      `${KEPT_FOLDER}\\CretLink-Support.exe${after}`,
      ...(kept.relay ? [`${KEPT_FOLDER}\\cretlink.toml${after}`] : []),
      `${KEPT_FOLDER}\\install.json${after}`,
      `The folder ${KEPT_FOLDER}${after}`,
      "The shortcut C:\\ProgramData\\Microsoft\\Windows\\Start Menu\\Programs\\CretLink Support.lnk",
      ...(kept.desktop ? ["The shortcut CretLink Support.lnk on the desktop of all users"] : []),
      "The entry in Installed apps (HKLM\\Software\\Microsoft\\Windows\\CurrentVersion\\Uninstall\\CretLink Support)",
    ],
    stays: ["A session report that you saved stays where you saved it"],
    afterExit: isKeptProgram,
    elevates: !elevated,
  };
}

function removeProgram(): RemoveOutcome {
  ensureIdle();
  if (fail === "declined") refuse("keep_declined", "Windows was not given permission. Nothing was removed.");
  if (fail === "remove") {
    refuse("keep_incomplete", `These could not be removed: ${KEPT_FOLDER}\\CretLink-Support.exe: access is denied.`);
  }
  if (!elevated) {
    // Windows asks, and the elevated program removes it: this window closes.
    kept = null;
    closed = true;
    return { state: "elevating", left: [] };
  }
  kept = null;
  pendingRemoval = isKeptProgram;
  return { state: isKeptProgram ? "pending" : "removed", left: [] };
}

/** Sets the view and pushes it to the window, as the session's task does. */
function show(next: ConsentView): ConsentView {
  view = elevated
    ? next
    : { ...next, status: { ...next.status, identity: { ...next.status.identity, elevated: false, integrity: "medium" } } };
  void emit("agent-view", view);
  return view;
}

/** What the computer holds, in words, for whoever looks at this page. */
function report(): void {
  let state = document.getElementById("computer");
  if (!state) {
    state = document.createElement("pre");
    state.id = "computer";
    state.setAttribute("aria-label", "What this computer holds (development)");
    state.style.cssText = "margin:0;padding:8px 12px;font:12px/1.4 monospace;background:#111;color:#ddd;white-space:pre-wrap";
    document.body.append(state);
  }
  state.textContent = JSON.stringify({ copy, elevated, thisVersion, kept, pendingRemoval, closed, session: sessionExists() });
}

mockWindows("main");
mockIPC(
  (cmd, args) => {
    const payload = (args ?? {}) as Record<string, unknown>;
    try {
      switch (cmd) {
        case "get_launch":
          return { ...LAUNCH_READY, elevated, back: "close" };
        case "get_view":
          return view;
        case "get_activity":
          return [];
        case "browse_consoles":
          return CONSOLES;
        case "get_keep":
          return keepView();
        case "keep_plan":
          return keepPlan();
        case "keep_program":
          return keepProgram(payload.desktop === true);
        case "remove_plan":
          return removePlan();
        case "remove_program":
          return removeProgram();
        case "connect_console":
        case "start_live_session":
          // The technician is there: straight to the decision.
          return show(viewIn({ phase: "awaiting_consent" }));
        case "grant_consent":
          return show(viewIn({ phase: "full_control", session: "7f3a9c51" }));
        case "decline_consent":
        case "stop_session":
          return show(viewIn({ phase: "stopped", reason: "customer_requested" }));
        case "get_end_report":
          return kept ? { ...REPORT, keptProgram: { location: KEPT_FOLDER, version: kept.version } } : REPORT;
        case "stop_and_close":
          closed = true;
          return undefined;
        default:
          // As Tauri answers a command the backend does not register.
          throw `Command ${cmd} not found`;
      }
    } finally {
      report();
    }
  },
  { shouldMockEvents: true },
);

report();
void start({ window: "main" });
