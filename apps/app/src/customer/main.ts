// The customer windows' controller (ST-M009-009, ADR-0011).
//
// The Rust backend owns the session and the admin step. This module turns
// Tauri command results and events into a `CustomerSnapshot`, renders it with
// the pure views — the main window, or the compact bar in the window labelled
// `mini` — and patches the live DOM (`morph`), so focus, the typed code and
// the live regions survive every update. Intents arrive as `data-action`
// clicks and keys and become the narrow command set of `api`.

import "./styles.css";
import { listen } from "@tauri-apps/api/event";
import type { ActivityEntry } from "../shared/contract";
import { type ActionEvent, bindActions } from "../shared/dom";
import { isActivityEvent, trimFeed, upsertActivity } from "../shared/feed";
import { morph } from "../shared/morph";
import {
  AGENT_ACTIVITY_EVENT,
  AGENT_VIEW_EVENT,
  CLOSE_REQUESTED_EVENT,
  type ConsentView,
  api,
  isCommandError,
} from "./api";
import {
  type CustomerSnapshot,
  FILTERS,
  type Filter,
  type UiError,
  type WindowKind,
  applyView,
  canStop,
  initialSnapshot,
  liveCounts,
  screenFor,
} from "./model";
import { CODE_MAX } from "./views/find";
import { renderMini } from "./views/mini";
import { renderCustomer } from "./views/shell";

/** What the bootstrap knows about this window. */
export interface StartOptions {
  window?: WindowKind;
}

const REPORT_RETRY_MS = 1000;
const LISTENERS = Symbol.for("cretlink.customer.listeners");

let root: HTMLElement | null = null;
let live: HTMLElement | null = null;
let snap: CustomerSnapshot = initialSnapshot("main", Date.now());
let lastScreen = "";
let focusNext: string | null = null;
let renderQueued = false;
let ticker: number | null = null;
let reportTimer: number | null = null;

// Bumped every time the backend pushes a view over `agent-view`. A command's
// return value is adopted only if no push arrived while it was in flight, so
// a stale return can never clobber a newer pushed view (e.g. `grant_consent`
// returns the pre-grant view; the pushed full-control view wins,
// ST-M002-013).
let pushGeneration = 0;

function set(patch: Partial<CustomerSnapshot>): void {
  snap = { ...snap, ...patch, now: Date.now() };
  scheduleRender();
}

function scheduleRender(): void {
  if (renderQueued) return;
  renderQueued = true;
  queueMicrotask(() => {
    renderQueued = false;
    render();
  });
}

function render(): void {
  if (!root) return;
  const next = snap.window === "mini" ? renderMini(snap) : renderCustomer(snap);
  if (!live) {
    root.replaceChildren(next);
    root.removeAttribute("aria-busy");
    live = next;
  } else {
    live = morph(live, next) as HTMLElement;
  }
  const screen = screenFor(snap);
  if (screen !== lastScreen) {
    lastScreen = screen;
    focusScreen();
  }
  if (focusNext) {
    const target = live.querySelector<HTMLElement>(focusNext);
    focusNext = null;
    target?.focus();
  }
  updateTicker();
}

// A new screen takes focus to its natural first control, or to its main
// region — on the consent screen neither Allow nor Decline is focused by
// default (ST-M009-009). A re-render of the same screen never moves focus.
function focusScreen(): void {
  if (!live || snap.window === "mini") return;
  const active = document.activeElement;
  if (active && active !== document.body && live.contains(active) && active.isConnected) return;
  const target =
    live.querySelector<HTMLElement>("main [data-autofocus]:not([disabled])") ??
    live.querySelector<HTMLElement>(".cfoot [data-autofocus]:not([disabled])") ??
    live.querySelector<HTMLElement>("#main");
  target?.focus({ preventScroll: true });
}

// Elapsed times tick once a second under full control and while anything runs.
function updateTicker(): void {
  const wanted =
    snap.view?.status.phase.phase === "full_control" || liveCounts(snap.activity).running > 0;
  if (wanted && ticker === null) {
    ticker = window.setInterval(() => set({}), 1000);
  } else if (!wanted && ticker !== null) {
    window.clearInterval(ticker);
    ticker = null;
  }
}

// --- State from the backend ---

function adoptView(next: ConsentView): void {
  const before = snap.view?.status.phase.phase;
  const after = next.status.phase.phase;
  const out = applyView(snap, next, Date.now());
  // A phase change is announced in words; an older notice would hide it.
  snap = before === after ? out : { ...out, notice: undefined };
  scheduleRender();
  if (snap.hadControl && (after === "stopped" || after === "failed") && !snap.report) {
    void loadReport();
  }
  // A session that ended before control returns to the list: search again.
  const ended = after === "stopped" || after === "failed";
  const wasEnded = before === "stopped" || before === "failed";
  if (before !== undefined && ended && !wasEnded && !snap.hadControl) void browse();
}

// The end report is built once the session task's cleanup is in (bounded on
// the backend), which then pushes its last view; until then it is asked for
// again, and at once on every pushed view.
async function loadReport(): Promise<void> {
  if (reportTimer !== null) {
    window.clearTimeout(reportTimer);
    reportTimer = null;
  }
  try {
    const report = await api.getEndReport();
    if (report) {
      set({ report });
      return;
    }
  } catch {
    // Not built yet, or not offered: the end screen says it is cleaning up.
  }
  reportTimer = window.setTimeout(() => {
    reportTimer = null;
    if (snap.hadControl && !snap.report && !snap.view?.session_active) void loadReport();
  }, REPORT_RETRY_MS);
}

async function loadActivity(): Promise<void> {
  try {
    const feed = await api.getActivity();
    if (Array.isArray(feed)) {
      set({ activity: trimFeed(feed.filter(isActivityEvent) as ActivityEntry[]) });
    }
  } catch {
    // The feed fills from live events.
  }
}

async function loadLaunch(): Promise<void> {
  try {
    set({ launch: await api.getLaunch() });
  } catch {
    // An older backend without the admin step: go straight on.
    set({ launch: null });
  }
}

// One LAN search; the list is transient and untrusted, never part of the
// authoritative view, and nothing is preselected or dialed (ADR-0007).
async function browse(): Promise<void> {
  if (snap.window === "mini") return;
  set({ discovery: { ...snap.discovery, state: "searching", problem: undefined } });
  try {
    const consoles = await api.browseConsoles();
    set({ discovery: { state: "done", consoles: Array.isArray(consoles) ? consoles : [] } });
  } catch (error) {
    set({ discovery: { state: "done", consoles: [], problem: capitalise(describe(error)) } });
  }
}

// --- Intents ---

/** Runs a session command and adopts its returned view unless a push won. */
async function viewCommand(
  busy: string,
  call: () => Promise<ConsentView>,
  // What to say around the backend's message, or how to choose that from the
  // refusal itself.
  failure: Omit<UiError, "message"> | ((error: unknown) => Omit<UiError, "message">),
): Promise<void> {
  const startGen = pushGeneration;
  set({ busy, error: undefined });
  try {
    const next = await call();
    if (pushGeneration === startGen) adoptView(next);
    set({ busy: undefined });
  } catch (error) {
    const around = typeof failure === "function" ? failure(error) : failure;
    set({ busy: undefined, error: { ...around, message: describe(error) } });
  }
}

function stop(): void {
  if (!canStop(snap)) return;
  set({ confirmClose: false });
  void viewCommand("stop", api.stopSession, {
    title: "The session could not be stopped from here",
    next: "Close CretLink to end everything.",
  });
}

function connect(address: string): void {
  void viewCommand("connect", () => api.connectConsole(address), {
    title: "CretLink could not connect",
    next: "Choose your technician again, or enter the code they give you.",
  });
}

function connectCode(): void {
  const input = live?.querySelector<HTMLInputElement>("#code");
  const code = (input?.value ?? "").trim().slice(0, CODE_MAX);
  if (!code) {
    focusNext = "#code";
    set({
      error: {
        title: "Enter the code first",
        message: "The code field is empty.",
        next: "Type the code your technician reads out, then press Connect.",
      },
    });
    return;
  }
  void viewCommand("connect", () => api.startLiveSession(code), (error) =>
    // A relay code is not a mistyped code: typing it again would not help
    // (ST-M007-012).
    isCommandError(error) && error.stage === "relay"
      ? {
          title: "This copy cannot use a relay code",
          next: "Ask your technician for a copy of CretLink that is set up for their relay, or connect while you are on the same network.",
        }
      : {
          title: "That code did not work",
          next: "Check the code with your technician and type it again.",
        },
  );
}

async function relaunch(): Promise<void> {
  set({ busy: "continue", error: undefined });
  try {
    const result = await api.relaunchElevated();
    set({ busy: undefined, launch: result.launch });
    if (result.outcome === "failed") {
      set({
        error: {
          title: "CretLink could not ask Windows for permission",
          message: result.message,
          next: "Try again, or close CretLink and open it again.",
        },
      });
    }
    // `started`: this window is closing; `declined`: the screen says so.
  } catch (error) {
    set({
      busy: undefined,
      error: {
        title: "CretLink could not ask Windows for permission",
        message: describe(error),
        next: "Try again, or close CretLink and open it again.",
      },
    });
  }
}

async function withoutAdmin(): Promise<void> {
  set({ busy: "without-admin", error: undefined });
  try {
    set({ busy: undefined, launch: await api.continueWithoutAdmin() });
    void browse();
  } catch (error) {
    set({
      busy: undefined,
      error: { title: "That did not work", message: describe(error), next: "Try again." },
    });
  }
}

async function plain(busy: string, call: () => Promise<unknown>, title: string, next: string): Promise<void> {
  set({ busy, error: undefined });
  try {
    await call();
    set({ busy: undefined });
  } catch (error) {
    set({ busy: undefined, error: { title, message: describe(error), next } });
  }
}

async function save(): Promise<void> {
  set({ busy: "save", error: undefined });
  try {
    const path = await api.saveReport();
    set(path ? { busy: undefined, saved: path, notice: `Saved to ${path}.` } : { busy: undefined });
  } catch (error) {
    set({
      busy: undefined,
      error: {
        title: "The copy was not saved",
        message: describe(error),
        next: "Try again, or choose another folder.",
      },
    });
  }
}

function closeApp(confirmed: boolean): void {
  if (snap.closing) return;
  set({ closing: confirmed, confirmClose: confirmed });
  void plain("close", api.stopAndClose, "CretLink could not close", "Press Stop now, then close the window.");
}

function onAction(event: ActionEvent): void {
  const { action, arg } = event;
  switch (action) {
    case "skip":
      document.getElementById("main")?.focus();
      break;
    case "continue":
      void relaunch();
      break;
    case "back":
      void plain("back", api.goBack, "CretLink could not go back", "Close this window instead.");
      break;
    case "without-admin":
      void withoutAdmin();
      break;
    case "search":
      void browse();
      break;
    case "connect":
      if (arg) connect(arg);
      break;
    case "connect-code":
      connectCode();
      break;
    case "allow":
      void viewCommand("allow", api.grantConsent, {
        title: "Control was not granted",
        next: "Nothing changed. Ask your technician to start again if you still want help.",
      });
      break;
    case "decline":
      void viewCommand("decline", api.declineConsent, {
        title: "The decision did not reach CretLink",
        next: "Press Stop or close CretLink; nobody has control.",
      });
      break;
    case "stop":
      stop();
      break;
    case "demo":
      void viewCommand("demo", api.resetDemo, {
        title: "The demo did not start",
        next: "This needs a development build.",
      });
      break;
    case "simulate-stop":
      void viewCommand("simulate-stop", api.simulateTechnicianStop, {
        title: "The simulated stop did not work",
        next: "Press Stop now instead.",
      });
      break;
    case "filter":
      if (arg && FILTERS.some((f) => f.filter === arg)) set({ filter: arg as Filter });
      break;
    case "make-small":
      void plain("make-small", api.makeSmall, "The compact bar did not open", "Keep this window open instead.");
      break;
    case "open-main":
      void plain("open-main", api.openMain, "The window did not open", "Use the taskbar instead.");
      break;
    case "save":
      void save();
      break;
    case "open-report":
      if (arg) void plain("open-report", () => api.openSavedReport(arg), "The copy did not open", "Open it from the Documents folder.");
      break;
    case "close-app":
      closeApp(false);
      break;
    case "confirm-close":
      closeApp(true);
      break;
    case "cancel-close":
      set({ confirmClose: false });
      break;
    case "dismiss-error":
      set({ error: undefined });
      break;
    default:
      break;
  }
}

// --- Keys ---

function onKeydown(event: KeyboardEvent): void {
  if (event.key === "Escape") {
    // Escape is the stop in every phase with a session (REQ-F-018); on the
    // resting window it does nothing.
    if (canStop(snap)) {
      event.preventDefault();
      stop();
    } else if (snap.confirmClose && !snap.closing) {
      set({ confirmClose: false });
    }
    return;
  }
  const target = event.target;
  if (event.key === "Enter" && target instanceof HTMLInputElement && target.id === "code") {
    event.preventDefault();
    connectCode();
  }
}

// The compact bar has no system frame: it is moved by dragging the bar.
function onMouseDown(event: MouseEvent): void {
  if (event.button !== 0) return;
  const target = event.target;
  if (!(target instanceof Element)) return;
  if (target.closest("button, a, input")) return;
  if (!target.closest('[data-drag="true"], .mininow')) return;
  void api.dragWindow().catch(() => undefined);
}

function describe(error: unknown): string {
  if (isCommandError(error)) return error.message;
  if (error instanceof Error) return error.message;
  if (typeof error === "string") return error;
  return "Something went wrong.";
}

function capitalise(text: string): string {
  return text.charAt(0).toUpperCase() + text.slice(1);
}

/** Starts this window; called by the bootstrap in `../main.ts` once the
 * backend has said that this process runs the customer role (ST-M009-005). */
export async function start(options: StartOptions = {}): Promise<void> {
  root = document.getElementById("app");
  if (!root) return;
  snap = initialSnapshot(options.window ?? "main", Date.now());
  document.documentElement.classList.toggle("mini-window", snap.window === "mini");
  bindActions(root, onAction);
  // Document-wide keys, so Escape stops wherever the focus is. A controller
  // started again (a reload, the tests) replaces the previous listeners.
  const store = globalThis as { [LISTENERS]?: AbortController };
  store[LISTENERS]?.abort();
  const listeners = new AbortController();
  store[LISTENERS] = listeners;
  document.addEventListener("keydown", onKeydown, { signal: listeners.signal });
  if (snap.window === "mini") {
    document.addEventListener("mousedown", onMouseDown, { signal: listeners.signal });
  }
  // Subscribe before the first view so no live transition is missed.
  await listen<ConsentView>(AGENT_VIEW_EVENT, (event) => {
    pushGeneration += 1;
    adoptView(event.payload);
  });
  await listen<unknown>(AGENT_ACTIVITY_EVENT, (event) => {
    if (isActivityEvent(event.payload)) {
      set({ activity: upsertActivity(snap.activity, event.payload) });
    }
  });
  if (snap.window === "main") {
    await listen<unknown>(CLOSE_REQUESTED_EVENT, () => {
      if (snap.view?.session_active && !snap.closing) {
        focusNext = "#close-dialog [data-autofocus]";
        set({ confirmClose: true });
      }
    });
  }
  await loadLaunch();
  try {
    adoptView(await api.getView());
  } catch (error) {
    set({
      error: {
        title: "CretLink could not read its state",
        message: describe(error),
        next: "Close CretLink and open it again.",
      },
    });
  }
  render();
  await loadActivity();
  if (snap.window === "main" && screenFor(snap) === "find") void browse();
}
