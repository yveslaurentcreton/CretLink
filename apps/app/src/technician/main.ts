// The technician window's controller (ST-M009-008, ADR-0011).
//
// The Rust backend owns the session. This module turns Tauri command results
// and events into a `TechnicianSnapshot`, renders it with the pure views and
// patches the live DOM (`morph`), so focus, scroll positions, the live regions
// and the embedded terminal survive every update. User intents arrive as
// `data-action` clicks and keys and become the narrow command set of `api`.
// The file explorer of the Files tab has a controller of its own
// (`./explorer-control`, ST-M010-002), which reads and writes its part of the
// snapshot through this one.

import "./styles.css";
import { listen } from "@tauri-apps/api/event";
import type {
  ActivityEntry,
  AiClientStatus,
  ClientId,
  RelayForm,
  RelaySettings,
} from "../shared/contract";
import { type ActionEvent, bindActions } from "../shared/dom";
import { morph } from "../shared/morph";
import {
  CONSOLE_ACTIVITY_EVENT,
  CONSOLE_BRIDGE_EVENT,
  CONSOLE_EXPLORER_DROP_EVENT,
  CONSOLE_EXPLORER_EVENT,
  CONSOLE_NOTICE_EVENT,
  CONSOLE_REMOVAL_EVENT,
  CONSOLE_STATE_EVENT,
  CONSOLE_TRANSFERS_EVENT,
  type ConsoleState,
  api,
  isCommandError,
} from "./api";
import { stoppedTransfers } from "./explorer";
import { createExplorer } from "./explorer-control";
import {
  type LiveTab,
  type NavPage,
  type TechnicianSnapshot,
  type UiError,
  applyConsole,
  canStop,
  initialRelayDraft,
  initialSnapshot,
  isActivityEvent,
  isAuditEntry,
  isBridgeStatus,
  isChosenCa,
  isConsoleNotice,
  isListenNetwork,
  isRelaySettings,
  isRelayTest,
  isUninstallPlan,
  listenHost,
  placeholderConsole,
  relayForm,
  screenFor,
  sessionCounts,
  summaryFor,
  targetName,
  trimFeed,
  upsertActivity,
} from "./model";
import { ConsoleTerminal } from "./terminal";
import { explorerPanel, explorerRows } from "./views/explorer";
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
// What the drawing that is queued covers: the whole window, or the Files tab
// alone, which a folder that arrives in pages changes often (ST-M010-002).
let renderQueued: "window" | "panel" | null = null;
let ticker: number | null = null;
const terminal = new ConsoleTerminal();

/** Whether the Files tab of the live session is on screen. */
function explorerShown(): boolean {
  return screenFor(snap) === "live" && snap.tab === "files";
}

// The file explorer of the Files tab (ST-M010-002).
const explorer = createExplorer({
  get: () => snap.explorer,
  set: (next, draw) => {
    if (next === snap.explorer) return;
    snap = { ...snap, explorer: next };
    if (draw === "window") scheduleRender();
    else if (draw === "panel") scheduleRender("panel");
    // The rows alone are drawn at once, unless more is about to be drawn.
    else if (draw === "rows" && renderQueued === null) renderRows();
  },
  shown: explorerShown,
  root: () => root,
  announce: (text) => set({ notice: text }),
  host: () => targetName(snap),
  live: () => snap.console.phase.phase === "full_control",
  alert: (problem) => set({ error: problem }),
});

// The events of the webview itself while files are dragged over the window
// (ST-M010-002). They only show where a drop would go: what was dropped
// arrives from the backend, which keeps the paths.
const DRAG_ENTER_EVENT = "tauri://drag-enter";
const DRAG_LEAVE_EVENT = "tauri://drag-leave";
const DRAG_DROP_EVENT = "tauri://drag-drop";

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

function scheduleRender(scope: "window" | "panel" = "window"): void {
  const queued = renderQueued !== null;
  if (scope === "window" || renderQueued === null) renderQueued = scope;
  if (queued) return;
  queueMicrotask(() => {
    const queuedScope = renderQueued;
    renderQueued = null;
    if (queuedScope === "panel") renderPanel();
    else render();
  });
}

/** Draws the Files tab alone; the rest of the window is as it was. */
function renderPanel(): void {
  const panel = explorerShown() ? live?.querySelector("#xp") : null;
  if (!panel) {
    render();
    return;
  }
  morph(panel, explorerPanel(snap));
  explorer.afterRender();
}

/** Draws the rows of the explorer's list alone, as it scrolls. */
function renderRows(): void {
  const rows = live?.querySelector("#xp-rows");
  if (rows) morph(rows, explorerRows(snap.explorer));
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
  explorer.afterRender();
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
  // (ST-M009-016). A wait on a relay listens on no network of this computer.
  if (next.phase.phase === "listening" && !next.relay) {
    const host = listenHost(next.phase.address);
    const previous = snap.console.phase;
    if (previous.phase !== "listening" || listenHost(previous.address) !== host) {
      void loadNetworks();
    }
    // The waiting screen offers the relay of this copy: read which one that
    // is now, a profile file may have been put next to the program meanwhile
    // (ST-M007-013).
    if (previous.phase !== "listening") void loadRelay();
  }
  const wasFull = before === "full_control";
  // Transfers that the end of full control cuts short are said in the
  // window's own alert: the explorer that listed them is dropped with the
  // session (ST-M010-002).
  const cut =
    wasFull && next.phase.phase !== "full_control"
      ? stoppedTransfers(snap.explorer.transfers)
      : undefined;
  const { snapshot, ended } = applyConsole(snap, next, Date.now());
  // A phase change is announced in words; an older notice would hide it.
  snap = before === next.phase.phase ? snapshot : { ...snapshot, notice: undefined };
  if (cut) snap = { ...snap, error: cut };
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
  if (wasFull !== (next.phase.phase === "full_control")) {
    // The explorer belongs to one session's full control (`applyConsole`
    // dropped what it listed): nothing of it is waited for any more.
    explorer.reset();
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

// --- Settings → Relay (ST-M007-011) ---

// The newest answer about the relay wins: a save, a removal and a look at
// the settings may be on their way at once.
let relayGeneration = 0;

/** Which relay this copy uses, as the backend reads it now. */
async function loadRelay(): Promise<void> {
  relayGeneration += 1;
  const generation = relayGeneration;
  try {
    const relay = await api.relaySettings();
    if (generation === relayGeneration) set({ relay: isRelaySettings(relay) ? relay : null });
  } catch {
    // Not offered: the card says that it is not available yet.
    if (generation === relayGeneration) set({ relay: null });
  }
}

// The field of the form that a refusal of the backend is about.
const RELAY_FIELDS: Record<string, string> = {
  relay_address: "#relay-address",
  relay_key: "#relay-key",
  relay_ca: "#relay-ca-choose",
  relay_name: "#relay-name",
};

/** A refusal about the relay: its words, and the field they are about. */
function relayRefused(error: unknown, title: string, otherwise: string): void {
  const field = isCommandError(error) ? RELAY_FIELDS[error.stage] : undefined;
  if (field) focusNext = field;
  set({
    busy: undefined,
    error: {
      title,
      // The backend's sentence stands alone: it names the setting already.
      message: isCommandError(error) ? error.message : describe(error),
      next: field ? "Correct it, then try again." : otherwise,
    },
  });
}

/** The form of Settings → Relay as it stands; none when no form is shown. */
function readRelayForm(): RelayForm | undefined {
  const input = (id: string): HTMLInputElement | null | undefined =>
    live?.querySelector<HTMLInputElement>(`#${id}`);
  const address = input("relay-address");
  if (!address) return undefined;
  return relayForm(
    {
      address: address.value,
      key: input("relay-key")?.value ?? "",
      removeKey: input("relay-key-remove")?.checked ?? false,
      name: input("relay-name")?.value ?? "",
    },
    snap.relayDraft,
    snap.relay?.stored?.relay,
  );
}

/**
 * Adopts what the backend says the settings hold now, with an empty form.
 * The form is drawn anew, which would leave the keyboard nowhere: `focus`
 * names where it goes instead.
 */
function relayChanged(relay: RelaySettings, notice: string, focus: string): void {
  relayGeneration += 1;
  focusNext = focus;
  set({
    busy: undefined,
    relay,
    // A new form: no typed key stays in a field.
    relayDraft: initialRelayDraft(snap.relayDraft.round + 1),
    notice,
  });
}

async function saveRelay(): Promise<void> {
  const form = readRelayForm();
  if (!form || snap.busy !== undefined) return;
  set({ busy: "relay-save", error: undefined });
  try {
    const relay = await api.saveRelay(form);
    if (!isRelaySettings(relay)) throw new Error("The settings could not be read back.");
    const host = relay.stored?.relay?.host;
    relayChanged(
      relay,
      host ? `The relay ${host} was saved.` : "The relay was saved.",
      // Where the technician was: on Save.
      '[data-action="relay-save"]',
    );
  } catch (error) {
    // The backend's sentence says whether anything was changed.
    relayRefused(error, "The relay was not saved", "Try again.");
  }
}

async function removeRelay(): Promise<void> {
  if (snap.busy !== undefined) return;
  set({ busy: "relay-remove", error: undefined });
  try {
    const relay = await api.removeRelay();
    if (!isRelaySettings(relay)) throw new Error("The settings could not be read back.");
    relayChanged(
      relay,
      "The relay was removed from the settings.",
      // The question is gone: on to the empty form, or to the relay of the
      // file that is in use.
      '#relay-address, [data-action="relay-test"]',
    );
  } catch (error) {
    relayRefused(error, "The relay was not removed", "Try again.");
  }
}

/** Closes "Remove the relay?" and gives the focus back to what opened it. */
function closeRelayQuestion(): void {
  focusNext = '[data-action="relay-remove"]';
  set({ relayDraft: { ...snap.relayDraft, removing: false } });
}

/** Reaches the relay of the form as it stands, or the one this copy uses. */
async function testRelay(): Promise<void> {
  if (snap.busy !== undefined || snap.relayDraft.test?.state === "running") return;
  const form = readRelayForm();
  const round = snap.relayDraft.round;
  set({ relayDraft: { ...snap.relayDraft, test: { state: "running" } }, error: undefined });
  try {
    const result = await api.testRelay(form);
    // The form was saved or removed meanwhile: the result is of another relay.
    if (snap.relayDraft.round !== round) return;
    if (!isRelayTest(result)) throw new Error("The test gave no result.");
    set({ relayDraft: { ...snap.relayDraft, test: { state: "done", result } } });
  } catch (error) {
    if (snap.relayDraft.round !== round) return;
    set({ relayDraft: { ...snap.relayDraft, test: undefined } });
    relayRefused(error, "The relay could not be tested", "Check the relay settings.");
  }
}

/** The system's dialog for the certificate authority file of the relay. */
async function chooseRelayCa(): Promise<void> {
  if (snap.busy !== undefined) return;
  set({ busy: "relay-ca", error: undefined });
  try {
    const chosen = await api.chooseRelayCa();
    if (chosen === null) {
      set({ busy: undefined });
      return;
    }
    if (!isChosenCa(chosen)) throw new Error("The file could not be read.");
    set({
      busy: undefined,
      relayDraft: {
        ...snap.relayDraft,
        ca: { mode: "chosen", pick: chosen.pick, name: chosen.name },
        test: undefined,
      },
      notice: `${chosen.name} is chosen. It is kept when you save.`,
    });
  } catch (error) {
    relayRefused(
      error,
      "That certificate file cannot be used",
      "Choose the PEM file of the authority that signed the relay's certificate.",
    );
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

/**
 * **Use relay** on the waiting screen (ST-M007-013): stop waiting on the
 * network and wait for the customer on the relay of this copy.
 */
async function useRelay(): Promise<void> {
  const phase = snap.console.phase;
  if (phase.phase !== "listening" || snap.console.relay || snap.busy !== undefined) return;
  set({ busy: "relay", error: undefined, networksOpen: false });
  try {
    const next = await api.useRelay();
    // The answer is the waiting state on the relay. States were pushed
    // meanwhile; the answer is adopted unless the relay has already answered
    // or failed, which is newer.
    snap = { ...snap, busy: undefined };
    const now = snap.console.phase.phase;
    const newer = snap.console.relay !== undefined && now !== "stopped";
    if (!newer && (now === "stopped" || now === "listening" || now === "idle")) {
      adoptConsole(next);
    }
    // The keyboard was on **Use relay**, which is gone: on to Cancel.
    focusNext = "#stop";
    set({});
    // The backend read the profile again for this wait: so does the screen,
    // which says where the relay comes from (ST-M007-011).
    void loadRelay();
  } catch (error) {
    // Nothing changed, or the listener is gone already: show what the backend
    // says now, and why the relay was not used.
    set({
      busy: undefined,
      error: {
        title: "The relay was not used",
        message: isCommandError(error) ? error.message : describe(error),
        next: relayRefusalNext(error),
      },
    });
    try {
      adoptConsole(await api.getState());
    } catch {
      // The state stays as it was pushed.
    }
    void loadRelay();
  }
}

/** What to do when **Use relay** was refused, by why it was. */
function relayRefusalNext(error: unknown): string {
  const stage = isCommandError(error) ? error.stage : "";
  switch (stage) {
    case "relay_none":
    case "relay_profile":
      return "Open Settings, under Relay, to set up or correct the relay of this copy. The session on this network is unchanged.";
    default:
      return "Try again. If the session is no longer waiting, start a new one.";
  }
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
          relayDraft: { ...snap.relayDraft, removing: false },
        });
        // A profile file may have been put next to the program meanwhile.
        if (arg === "settings") void loadRelay();
      }
      break;
    case "tab":
      if (arg && (TABS as readonly string[]).includes(arg)) {
        set({ tab: arg as LiveTab });
        if (arg === "terminal") requestAnimationFrame(() => terminal.refit());
        if (arg === "audit") void loadAudit();
        if (arg === "files") explorer.shown();
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
    case "use-relay":
      void useRelay();
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
    case "relay-save":
      void saveRelay();
      break;
    case "relay-test":
      void testRelay();
      break;
    case "relay-ca-choose":
      void chooseRelayCa();
      break;
    case "relay-ca-none":
      // This button goes away with the file: the focus moves to its neighbour.
      focusNext = "#relay-ca-choose";
      set({ relayDraft: { ...snap.relayDraft, ca: { mode: "none" }, test: undefined } });
      break;
    case "relay-remove":
      focusNext = "#relay-remove-step [data-autofocus]";
      set({ relayDraft: { ...snap.relayDraft, removing: true } });
      break;
    case "relay-remove-cancel":
      closeRelayQuestion();
      break;
    case "relay-remove-confirm":
      void removeRelay();
      break;
    default:
      // The controls of the file explorer.
      if (action.startsWith("xp-")) explorer.onAction(event);
      break;
  }
}

// A pure clipboard convenience; no backend authority is involved.
async function copy(text: string): Promise<void> {
  // Said by its name: a relay code is not the connection code.
  const what = snap.console.relay ? "Relay code" : "Connection code";
  try {
    if (!navigator.clipboard) throw new Error("no clipboard");
    await navigator.clipboard.writeText(text);
    set({ notice: `${what} copied.` });
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
    if (!isUninstallPlan(plan)) throw new Error("The plan could not be read.");
    focusNext = "#uninstall-step";
    set({ uninstall: { plan: plan.removes, stays: plan.stays }, busy: undefined });
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
    // So do the explorer's open question, its menu, a name that is being
    // typed and its path field (ST-M010-002).
    if (explorerShown() && explorer.escape()) {
      event.preventDefault();
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
    } else if (snap.relayDraft.removing) {
      closeRelayQuestion();
    }
    return;
  }
  // The keys of the file explorer, while its tab is on screen.
  if (explorerShown() && explorer.onKeydown(event)) return;
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
      if (tab === "files") explorer.shown();
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
  // The mouse on the explorer's list, and its scrolling.
  explorer.bind(root, listeners.signal);
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
  // What the backend has to say that is not the failure of a command, such
  // as a network choice that could not be saved (ST-M007-011).
  await listen<unknown>(CONSOLE_NOTICE_EVENT, (event) => {
    if (isConsoleNotice(event.payload)) set({ error: event.payload });
  });
  // The pages of a folder the explorer opened (ST-M010-002).
  await listen<unknown>(CONSOLE_EXPLORER_EVENT, (event) => explorer.onEvent(event.payload));
  // What was dropped on the window, the transfer queue and a delete that runs.
  await listen<unknown>(CONSOLE_EXPLORER_DROP_EVENT, (event) => explorer.onDrop(event.payload));
  await listen<unknown>(CONSOLE_TRANSFERS_EVENT, (event) => explorer.onTransfers(event.payload));
  await listen<unknown>(CONSOLE_REMOVAL_EVENT, (event) => explorer.onRemoval(event.payload));
  await listen<unknown>(DRAG_ENTER_EVENT, () => explorer.onDrag(true));
  await listen<unknown>(DRAG_LEAVE_EVENT, () => explorer.onDrag(false));
  await listen<unknown>(DRAG_DROP_EVENT, () => explorer.onDrag(false));
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
    loadRelay(),
    refreshSessions(),
    refreshClients(),
  ]);
}
