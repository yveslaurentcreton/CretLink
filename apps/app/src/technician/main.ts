// The technician window's controller (ST-M009-008, ADR-0011).
//
// The Rust backend owns the session. This module turns Tauri command results
// and events into a `TechnicianSnapshot`, renders it with the pure views and
// patches the live DOM (`morph`), so focus, scroll positions, the live regions
// and the embedded terminal survive every update. User intents arrive as
// `data-action` clicks and keys and become the narrow command set of `api`.

import "./styles.css";
import { listen } from "@tauri-apps/api/event";
import type { ActivityEntry, AiClientStatus, ClientId } from "../shared/contract";
import { type ActionEvent, bindActions } from "../shared/dom";
import { morph } from "../shared/morph";
import {
  CONSOLE_ACTIVITY_EVENT,
  CONSOLE_BRIDGE_EVENT,
  CONSOLE_STATE_EVENT,
  type ConsoleState,
  api,
  isCommandError,
} from "./api";
import {
  type LiveTab,
  type NavPage,
  type TechnicianSnapshot,
  type UiError,
  applyConsole,
  canStop,
  initialSnapshot,
  isActivityEvent,
  isAuditEntry,
  isBridgeStatus,
  isListenNetwork,
  listenHost,
  placeholderConsole,
  screenFor,
  sessionCounts,
  summaryFor,
  trimFeed,
  upsertActivity,
} from "./model";
import { ConsoleTerminal } from "./terminal";
import { renderTechnician } from "./views/shell";

/** What the bootstrap knows about this process (`get_role`). */
export interface StartOptions {
  version?: string;
}

// Where a running controller keeps the handle that removes its document-wide
// listeners, so that one started again (a reload, the tests) replaces them.
const LISTENERS = Symbol.for("cretlink.technician.listeners");

const SESSIONS_RETRY_MS = 2000;
// The Audit tab asks for the session's entries at most this often while the
// session changes (ST-M009-008).
const AUDIT_REFRESH_MS = 1000;
const NAV_PAGES: readonly NavPage[] = ["home", "sessions", "ai", "settings"];
const TABS: readonly LiveTab[] = ["overview", "terminal", "files", "audit"];
const CLIENT_IDS: readonly ClientId[] = ["claude", "codex", "gemini"];

let root: HTMLElement | null = null;
let live: HTMLElement | null = null;
let snap: TechnicianSnapshot = initialSnapshot(placeholderConsole(), Date.now());
let lastScreen = "";
// An element to focus after the next render (a confirm step that just opened).
let focusNext: string | null = null;
let renderQueued = false;
let ticker: number | null = null;
const terminal = new ConsoleTerminal();

// Bumped every time the live pump pushes a snapshot over `"console-state"`.
// A command's return value is adopted only if no push arrived while it was in
// flight, so a stale return can never clobber a newer pushed state (e.g.
// `confirm_code` returns the pre-confirm view; the pushed `awaiting_consent`
// view is authoritative, ST-M002-013).
let pushGeneration = 0;

function set(patch: Partial<TechnicianSnapshot>): void {
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
  const next = renderTechnician(snap);
  if (!live) {
    root.replaceChildren(next);
    root.removeAttribute("aria-busy");
    live = next;
  } else {
    live = morph(live, next) as HTMLElement;
  }
  const screen = screenFor(snap);
  if (screen === "live") {
    const badge = live.querySelector<HTMLElement>('[data-keep="terminal-badge"]');
    const body = live.querySelector<HTMLElement>('[data-keep="terminal"]');
    if (badge && body) {
      const visible = snap.tab === "terminal";
      // xterm measures its container, so attach once the layout is current.
      requestAnimationFrame(() => terminal.attach(badge, body, visible));
    }
  }
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

// A new screen takes focus to its first natural control (or its main region),
// so keyboard users are never left on a removed element (REQ-N-014). A
// re-render of the same screen never moves focus.
function focusScreen(): void {
  if (!live) return;
  const active = document.activeElement;
  if (active && active !== document.body && live.contains(active)) return;
  const target =
    live.querySelector<HTMLElement>("[data-autofocus]:not([disabled])") ??
    live.querySelector<HTMLElement>("#main");
  target?.focus({ preventScroll: true });
}

// Elapsed times of running items tick once a second during a session.
function updateTicker(): void {
  const wanted =
    snap.console.phase.phase === "full_control" || sessionCounts(snap.activity).running > 0;
  if (wanted && ticker === null) {
    ticker = window.setInterval(() => set({}), 1000);
  } else if (!wanted && ticker !== null) {
    window.clearInterval(ticker);
    ticker = null;
  }
}

// --- State from the backend ---

function adoptConsole(next: ConsoleState): void {
  const before = snap.console.phase.phase;
  if (before !== next.phase.phase) {
    // Pairing, consent and the end are audit records the feed does not show.
    if (next.phase.phase === "listening") auditGeneration += 1;
    scheduleAuditRefresh();
  }
  // Waiting on an address the list does not name yet: ask which networks this
  // computer has, so the screen can name the adapter and offer the others
  // (ST-M009-016).
  if (next.phase.phase === "listening") {
    const host = listenHost(next.phase.address);
    const previous = snap.console.phase;
    if (previous.phase !== "listening" || listenHost(previous.address) !== host) {
      void loadNetworks();
    }
  }
  const wasFull = before === "full_control";
  const { snapshot, ended } = applyConsole(snap, next, Date.now());
  // A phase change is announced in words; an older notice would hide it.
  snap = before === next.phase.phase ? snapshot : { ...snapshot, notice: undefined };
  if (!wasFull && next.phase.phase === "full_control") {
    // A new session's bridge endpoint starts now: forget the last status
    // until the backend reports this one (ST-M009-008).
    bridgeGeneration += 1;
    snap = { ...snap, bridge: null };
    void loadBridge();
  }
  if (wasFull && next.phase.phase !== "full_control") {
    // Leaving full control tears the terminal down: the remote pty is closed
    // and the shell never outlives its session.
    terminal.dispose();
  }
  scheduleRender();
  if (ended) void refreshSessionsAfterEnd();
}

async function refreshSessions(): Promise<void> {
  try {
    const sessions = await api.listSessions();
    set({ sessions: Array.isArray(sessions) ? sessions : null });
  } catch {
    set({ sessions: snap.sessions ?? null });
  }
}

// The audit file is sealed a moment after the end, so a summary that is not
// there yet is asked for once more after ~2 s.
async function refreshSessionsAfterEnd(): Promise<void> {
  const id = snap.last?.id;
  await refreshSessions();
  if (id !== undefined && !summaryFor(snap, id)) {
    await new Promise((resolve) => window.setTimeout(resolve, SESSIONS_RETRY_MS));
    await refreshSessions();
  }
  if (snap.last?.id === id) set({ reportPending: false });
  // The end records are written by now.
  void loadAudit();
}

async function refreshClients(): Promise<void> {
  try {
    const clients = await api.aiClients();
    set({ clients: Array.isArray(clients) ? clients : null });
  } catch {
    set({ clients: null });
  }
}

async function loadAppInfo(): Promise<void> {
  try {
    set({ app: await api.getAppInfo() });
  } catch {
    set({ app: null });
  }
}

// Bumped by every `console-bridge` event and every new session, so an older
// `get_bridge_status` answer never overwrites a newer status.
let bridgeGeneration = 0;

async function loadBridge(): Promise<void> {
  const generation = bridgeGeneration;
  try {
    const status = await api.getBridgeStatus();
    if (generation === bridgeGeneration && isBridgeStatus(status)) set({ bridge: status });
  } catch {
    // Not offered: the rail falls back to the clients seen in the feed.
  }
}

// The networks this computer can wait on (ST-M009-016). The newest answer
// wins: adapters come and go, for example when a VPN connects.
let networksGeneration = 0;

async function loadNetworks(): Promise<void> {
  networksGeneration += 1;
  const generation = networksGeneration;
  try {
    const networks = await api.listenNetworks();
    if (generation === networksGeneration) {
      set({ networks: Array.isArray(networks) ? networks.filter(isListenNetwork) : null });
    }
  } catch {
    // Not offered: the screen shows the address without its adapter.
    if (generation === networksGeneration) set({ networks: null });
  }
}

/** Moves the waiting session to another network of this computer. */
async function switchNetwork(address: string): Promise<void> {
  if (snap.console.phase.phase !== "listening" || snap.busy !== undefined) return;
  const wanted = snap.networks?.find((network) => network.address === address);
  set({ busy: "network", error: undefined, networksOpen: false });
  try {
    const next = await api.switchListenNetwork(address);
    // The answer is the new waiting state, or the honest end when the address
    // could not be used. States were pushed meanwhile; the answer is adopted
    // unless a customer has already started to connect, which is newer.
    snap = { ...snap, busy: undefined };
    const now = snap.console.phase.phase;
    if (now === "stopped" || now === "listening" || now === "idle" || now === "failed") {
      adoptConsole(next);
    }
    if (snap.console.phase.phase === "listening") {
      set({
        notice: wanted
          ? `Now waiting on ${wanted.adapter}. The connection code changed.`
          : "The network changed. The connection code changed.",
      });
    } else {
      set({});
    }
  } catch (error) {
    set({
      busy: undefined,
      error: {
        title: "The network could not be changed",
        message: describe(error),
        next: "Start the session again and choose a network your customer's computer is on.",
      },
    });
    // The old listener may be gone already: show what the backend says now.
    try {
      adoptConsole(await api.getState());
    } catch {
      // The state stays as it was pushed.
    }
  }
  void loadNetworks();
}

// Bumped when a new session starts, so an older `get_session_audit` answer
// never fills the new session's Audit tab.
let auditGeneration = 0;
let auditTimer: number | null = null;

/** Refreshes the Audit tab at most once per `AUDIT_REFRESH_MS`. */
function scheduleAuditRefresh(): void {
  if (auditTimer !== null) return;
  auditTimer = window.setTimeout(() => {
    auditTimer = null;
    void loadAudit();
  }, AUDIT_REFRESH_MS);
}

async function loadAudit(): Promise<void> {
  const generation = auditGeneration;
  try {
    const entries = await api.getSessionAudit();
    if (generation === auditGeneration && Array.isArray(entries)) {
      set({ audit: entries.filter(isAuditEntry) });
    }
  } catch {
    // Not offered: the tab says it has nothing to show yet.
  }
}

async function loadActivity(): Promise<void> {
  try {
    const feed = await api.getActivity();
    if (Array.isArray(feed))
      set({ activity: trimFeed(feed.filter(isActivityEvent) as ActivityEntry[]) });
  } catch {
    // Not offered yet: the feed fills from live events.
  }
}

// --- Intents ---

/** Runs a session command and adopts its returned state unless a push won. */
async function sessionCommand(
  busy: string,
  call: () => Promise<ConsoleState>,
  failure: Omit<UiError, "message">,
): Promise<void> {
  const startGen = pushGeneration;
  set({ busy, error: undefined });
  try {
    const next = await call();
    if (pushGeneration === startGen) adoptConsole(next);
    set({ busy: undefined });
  } catch (error) {
    set({ busy: undefined, error: { ...failure, message: describe(error) } });
  }
}

function stop(): void {
  if (!canStop(snap.console)) return;
  void sessionCommand("stop", api.stopSession, {
    title: "The session could not be stopped from here",
    next: "Ask the customer to press Stop on their computer, or close CretLink to end everything.",
  });
}

function onAction(event: ActionEvent): void {
  const { action, arg } = event;
  switch (action) {
    case "skip":
      document.getElementById("main")?.focus();
      break;
    case "start":
      void sessionCommand("start", api.startLiveSession, {
        title: "The session could not start",
        next: "Check that this computer is connected to the network, then try again.",
      });
      break;
    case "stop":
      stop();
      break;
    case "compare":
      void sessionCommand("compare", api.confirmCode, {
        title: "The code could not be confirmed",
        next: "Stop the session and start a new one.",
      });
      break;
    case "mismatch":
      void sessionCommand("mismatch", api.reportCodeMismatch, {
        title: "The mismatch could not be reported",
        next: "Press Stop (Esc) to end the session.",
      });
      break;
    case "consent":
      void sessionCommand("consent", api.simulateConsent, {
        title: "Simulated consent did not work",
        next: "Stop the session and start a new one.",
      });
      break;
    case "lost":
      void sessionCommand("lost", api.simulateTargetLost, {
        title: "The simulated loss did not work",
        next: "Stop the session with Esc.",
      });
      break;
    case "nav":
      if (arg && (NAV_PAGES as readonly string[]).includes(arg)) {
        set({
          nav: arg as NavPage,
          openReport: undefined,
          endDismissed: true,
          confirm: undefined,
          uninstall: undefined,
        });
      }
      break;
    case "tab":
      if (arg && (TABS as readonly string[]).includes(arg)) {
        set({ tab: arg as LiveTab });
        if (arg === "terminal") requestAnimationFrame(() => terminal.refit());
        if (arg === "audit") void loadAudit();
      }
      break;
    case "open-report":
      if (arg) set({ nav: "sessions", openReport: arg, endDismissed: true });
      break;
    case "close-report":
      set({ openReport: undefined });
      break;
    case "done":
      set({ endDismissed: true, nav: "home", openReport: undefined });
      break;
    case "dismiss-error":
      set({ error: undefined });
      break;
    case "copy":
      if (arg) void copy(arg);
      break;
    case "networks":
      // Adapters come and go: read them again whenever the list opens.
      if (!snap.networksOpen) void loadNetworks();
      set({ networksOpen: !snap.networksOpen });
      break;
    case "network":
      if (arg) void switchNetwork(arg);
      break;
    case "open-audit":
      void openAudit(arg);
      break;
    case "ai-connect":
    case "ai-replace":
    case "ai-disconnect":
      if (isClientId(arg)) {
        focusNext = "#confirm-step";
        set({
          nav: "ai",
          openReport: undefined,
          endDismissed: true,
          confirm: {
            client: arg,
            action: action === "ai-disconnect" ? "uninstall" : "install",
            replace: action === "ai-replace",
          },
        });
      }
      break;
    case "ai-cancel":
      set({ confirm: undefined });
      break;
    case "ai-confirm":
      void runClientChange();
      break;
    case "uninstall-plan":
      void loadUninstallPlan();
      break;
    case "uninstall-cancel":
      set({ uninstall: undefined });
      break;
    case "uninstall-confirm":
      void uninstall();
      break;
    default:
      break;
  }
}

// A pure clipboard convenience; no backend authority is involved.
async function copy(text: string): Promise<void> {
  try {
    if (!navigator.clipboard) throw new Error("no clipboard");
    await navigator.clipboard.writeText(text);
    set({ notice: "Connection code copied." });
  } catch {
    set({ notice: "The code could not be copied. Read it out instead." });
  }
}

function isClientId(value: string | undefined): value is ClientId {
  return value !== undefined && (CLIENT_IDS as readonly string[]).includes(value);
}

async function openAudit(session: string | undefined): Promise<void> {
  try {
    await api.openAudit(session || undefined);
  } catch (error) {
    set({
      error: {
        title: "The audit log could not be opened",
        message: describe(error),
        next: snap.app
          ? `Open the folder ${snap.app.auditDir} in your file manager.`
          : "Try again in a moment.",
      },
    });
  }
}

async function runClientChange(): Promise<void> {
  const confirm = snap.confirm;
  if (!confirm) return;
  const client = snap.clients?.find((c) => c.id === confirm.client);
  const label = client?.label ?? confirm.client;
  set({ busy: `ai-${confirm.client}`, error: undefined });
  try {
    const clients: AiClientStatus[] =
      confirm.action === "install"
        ? await api.aiClientInstall(confirm.client)
        : await api.aiClientUninstall(confirm.client);
    set({
      clients: Array.isArray(clients) ? clients : snap.clients,
      confirm: undefined,
      busy: undefined,
      notice: confirm.action === "install" ? `${label} is connected.` : `${label} is disconnected.`,
    });
  } catch (error) {
    set({
      busy: undefined,
      error: {
        title:
          confirm.action === "install"
            ? `${label} could not be connected`
            : `${label} could not be disconnected`,
        message: describe(error),
        next: `Nothing else was changed. You can run the command shown on this page in a terminal instead.`,
      },
    });
    void refreshClients();
  }
}

async function loadUninstallPlan(): Promise<void> {
  set({ uninstall: { plan: null }, busy: "uninstall-plan", error: undefined });
  try {
    const plan = await api.uninstallPlan();
    focusNext = "#uninstall-step";
    set({ uninstall: { plan: Array.isArray(plan) ? plan : [] }, busy: undefined });
  } catch (error) {
    set({
      uninstall: undefined,
      busy: undefined,
      error: {
        title: "CretLink could not check what to uninstall",
        message: describe(error),
        next: "Nothing was changed. Try again in a moment.",
      },
    });
  }
}

async function uninstall(): Promise<void> {
  set({ busy: "uninstall", error: undefined });
  try {
    await api.uninstallApp();
    set({
      busy: undefined,
      uninstall: undefined,
      uninstalled: true,
      notice: "CretLink was uninstalled.",
    });
  } catch (error) {
    set({
      busy: undefined,
      error: {
        title: "Uninstalling did not finish",
        message: describe(error),
        next: "Check the list above for what is left, then try again.",
      },
    });
  }
}

// --- Keys ---

function onKeydown(event: KeyboardEvent): void {
  if (event.key === "Escape") {
    // An open list of networks closes first; Escape then stops as always.
    if (snap.networksOpen) {
      event.preventDefault();
      set({ networksOpen: false });
      queueMicrotask(() => document.getElementById("net-toggle")?.focus());
      return;
    }
    // Escape is the always-available stop in every live phase (REQ-F-018).
    if (canStop(snap.console)) {
      event.preventDefault();
      stop();
    } else if (snap.confirm) {
      set({ confirm: undefined });
    } else if (snap.uninstall) {
      set({ uninstall: undefined });
    }
    return;
  }
  // Arrow keys move between the session tabs.
  const target = event.target;
  if (target instanceof HTMLElement && target.getAttribute("role") === "tab") {
    const index = TABS.indexOf(snap.tab);
    let next = -1;
    if (event.key === "ArrowRight") next = (index + 1) % TABS.length;
    if (event.key === "ArrowLeft") next = (index - 1 + TABS.length) % TABS.length;
    if (event.key === "Home") next = 0;
    if (event.key === "End") next = TABS.length - 1;
    const tab = TABS[next];
    if (tab) {
      event.preventDefault();
      set({ tab });
      queueMicrotask(() => document.getElementById(`tab-${tab}`)?.focus());
      if (tab === "terminal") requestAnimationFrame(() => terminal.refit());
    }
  }
}

function describe(error: unknown): string {
  if (isCommandError(error)) return `${error.message} (${error.stage})`;
  if (error instanceof Error) return error.message;
  if (typeof error === "string") return error;
  return "Something went wrong.";
}

/** Starts this role's window; called by the bootstrap in `../main.ts` once the
 * backend has said which role this process runs (ST-M009-005). */
export async function start(options: StartOptions = {}): Promise<void> {
  root = document.getElementById("app");
  if (!root) return;
  snap = { ...snap, version: options.version ?? null };
  bindActions(root, onAction);
  // Document-wide keys, so Escape stops wherever the focus is. A controller
  // started again (a reload, the tests) replaces the previous listeners:
  // an earlier one would otherwise answer the same key a second time.
  const store = globalThis as { [LISTENERS]?: AbortController };
  store[LISTENERS]?.abort();
  const listeners = new AbortController();
  store[LISTENERS] = listeners;
  document.addEventListener("keydown", onKeydown, { signal: listeners.signal });
  // Subscribe before the first state so no live transition is missed.
  await listen<ConsoleState>(CONSOLE_STATE_EVENT, (event) => {
    pushGeneration += 1;
    adoptConsole(event.payload);
  });
  await listen<unknown>(CONSOLE_ACTIVITY_EVENT, (event) => {
    // Only `ActivityEvent`s of the contract are taken; anything else (an older
    // backend's line format) is ignored rather than guessed at.
    if (isActivityEvent(event.payload)) {
      set({ activity: upsertActivity(snap.activity, event.payload) });
      scheduleAuditRefresh();
    }
  });
  await listen<unknown>(CONSOLE_BRIDGE_EVENT, (event) => {
    if (isBridgeStatus(event.payload)) {
      bridgeGeneration += 1;
      set({ bridge: event.payload });
    }
  });
  try {
    adoptConsole(await api.getState());
  } catch (error) {
    set({
      error: {
        title: "CretLink could not read the session state",
        message: describe(error),
        next: "Close CretLink and open it again.",
      },
    });
  }
  render();
  await Promise.all([
    loadAppInfo(),
    loadActivity(),
    loadAudit(),
    loadBridge(),
    refreshSessions(),
    refreshClients(),
  ]);
}
