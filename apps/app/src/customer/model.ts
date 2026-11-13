// The customer interface's state snapshot and its pure derivations
// (ST-M009-009).
//
// Every screen of both customer windows — the main window and the compact
// bar — is a function of one `CustomerSnapshot`. The Rust backend stays
// authoritative for the session (`ConsentView`) and the admin step
// (`LaunchView`); the snapshot adds what the window itself remembers: the
// activity feed of the current or last session, the consoles found nearby,
// the timeline filter, the end report and its saved copy, and a pending close
// question. Nothing here talks to Tauri, so all of it is tested in jsdom.

import type { ActivityEntry, ActivityItem, ActivityKind, CustomerReport } from "../shared/contract";
import { formatElapsed, isFailure } from "../shared/feed";
import type { ConsentView, DiscoveredConsoleView, LaunchView, PhaseName } from "./api";

export type WindowKind = "main" | "mini";

export type Filter = "all" | "commands" | "files" | "screen";

export const FILTERS: ReadonlyArray<{ filter: Filter; label: string }> = [
  { filter: "all", label: "All" },
  { filter: "commands", label: "Commands" },
  { filter: "files", label: "Files" },
  { filter: "screen", label: "Screen" },
];

/** A plain-language error with a next step, shown in an assertive alert. */
export interface UiError {
  title: string;
  message: string;
  next: string;
}

export interface Discovery {
  state: "idle" | "searching" | "done";
  consoles: DiscoveredConsoleView[];
  // A browse that failed, in words (the code always works instead).
  problem?: string;
}

export interface CustomerSnapshot {
  window: WindowKind;
  // `null` until the backend has answered.
  view: ConsentView | null;
  launch: LaunchView | null;
  // The feed of the current or last session, oldest first.
  activity: ActivityEntry[];
  discovery: Discovery;
  filter: Filter;
  // This session reached full control, so its end shows the end report.
  hadControl: boolean;
  // The end report, once the backend has built it.
  report: CustomerReport | null;
  // Where the report copy was saved.
  saved?: string;
  // The main window asks "Stop the session and close?".
  confirmClose: boolean;
  // Stop and close was confirmed: cleaning up, then CretLink closes.
  closing: boolean;
  // What the last session that never reached control ended with, in words.
  ended?: string;
  // An action in flight, to disable its control.
  busy?: string;
  // A polite announcement of the last change (aria-live).
  notice?: string;
  error?: UiError;
  now: number;
}

export type Screen =
  | "loading"
  | "admin"
  | "declined"
  | "find"
  | "connecting"
  | "compare"
  | "consent"
  | "live"
  | "stopping"
  | "end";

export function initialSnapshot(window: WindowKind, now: number): CustomerSnapshot {
  return {
    window,
    view: null,
    launch: null,
    activity: [],
    discovery: { state: "idle", consoles: [] },
    filter: "all",
    hadControl: false,
    report: null,
    confirmClose: false,
    closing: false,
    now,
  };
}

// --- Phases ---

export function phaseOf(s: CustomerSnapshot): PhaseName {
  return s.view?.status.phase.phase ?? "idle";
}

function isEnd(phase: PhaseName): boolean {
  return phase === "stopped" || phase === "failed";
}

/** Whether a session exists: Stop and `Escape` act only then (ST-M009-009). */
export function canStop(s: CustomerSnapshot): boolean {
  return s.view?.session_active === true;
}

export function screenFor(s: CustomerSnapshot): Screen {
  const view = s.view;
  if (!view) return "loading";
  if (s.launch && s.launch.step !== "ready" && !view.session_active) {
    return s.launch.step === "declined" ? "declined" : "admin";
  }
  switch (view.status.phase.phase) {
    case "idle":
      return "find";
    case "announced":
    case "pairing":
      return view.approval_code ? "compare" : "connecting";
    case "awaiting_approval_compare":
      return "compare";
    case "awaiting_consent":
      return "consent";
    case "full_control":
      return "live";
    case "stopping":
      return "stopping";
    case "stopped":
    case "failed":
      return s.hadControl || s.report?.controlGranted ? "end" : "find";
  }
}

/** The state band of every screen, in words (REQ-N-013). */
export interface Band {
  kind: "none" | "decide" | "full";
  glyph: string;
  title: string;
  explanation: string;
}

export function bandFor(s: CustomerSnapshot, screen: Screen = screenFor(s)): Band {
  const name = technicianName(s);
  const none = (explanation: string): Band => ({
    kind: "none",
    glyph: "○",
    title: "No access",
    explanation,
  });
  switch (screen) {
    case "connecting":
      return none("Connecting · nobody can see or control this computer");
    case "compare":
      return none(`Connected to ${name}, but no access`);
    case "consent":
      return {
        kind: "decide",
        glyph: "◐",
        title: "Your decision",
        explanation: "No control has been granted",
      };
    case "live":
      return {
        kind: "full",
        glyph: "●",
        title: "Under full control",
        explanation: `${capName(name)} can operate this computer now`,
      };
    case "stopping":
      return none("Control has ended · cleaning up");
    case "end":
      return none("The session has ended");
    default:
      return none("Nobody can see or control this computer");
  }
}

/**
 * Folds a new `ConsentView` into the snapshot. A new dial (a rest phase giving
 * way to a session) clears what the window remembers of the last one; a
 * session that reaches full control is remembered, so its end shows the end
 * report; a session that ended before control says how, in words.
 */
export function applyView(s: CustomerSnapshot, next: ConsentView, now: number): CustomerSnapshot {
  const before = phaseOf(s);
  const after = next.status.phase.phase;
  let out: CustomerSnapshot = { ...s, view: next, now };
  const rest = before === "idle" || isEnd(before);
  if (s.view && rest && !isEnd(after) && after !== "idle") {
    out = {
      ...out,
      activity: [],
      hadControl: false,
      report: null,
      saved: undefined,
      ended: undefined,
      filter: "all",
      confirmClose: false,
    };
  }
  if (after === "full_control" || next.status.control_active) out.hadControl = true;
  if (isEnd(after) && !isEnd(before) && !out.hadControl) {
    out.ended = endedBeforeControl(next, technicianName(out));
  }
  if (!next.session_active) out.confirmClose = out.closing;
  return out;
}

function endedBeforeControl(view: ConsentView, name: string): string {
  const phase = view.status.phase;
  if (phase.phase === "failed") return "";
  switch (phase.phase === "stopped" ? phase.reason : undefined) {
    case "customer_requested":
      return "You stopped before anyone got access. Nothing was shared.";
    case "technician_requested":
      return `${capName(name)} ended the connection before getting access. Nothing was shared.`;
    case "disconnected":
      return view.relay
        ? `The connection through the relay ${view.relay.host} was lost before anyone got access. Nothing was shared.`
        : "The connection was lost before anyone got access. Nothing was shared.";
    default:
      return "The connection ended before anyone got access. Nothing was shared.";
  }
}

// --- A session through the relay (ST-M007-013) ---

/** The host name of the relay this session runs through, if it runs through one. */
export function relayHost(s: CustomerSnapshot): string | undefined {
  return s.view?.relay?.host;
}

/** The stage of a code that nobody waits on: mistyped, used, or given up. */
export const RENDEZVOUS_STAGE = "rendezvous";

/**
 * The stage of a session through a relay that lost its connection before
 * anyone had control. The backend takes it from the session's own verdict.
 */
export const RELAY_LOST_STAGE = "relay_lost";

/** The stage of a relay that could not be used: `relay_unreachable`, … */
export function isRelayFailure(stage: string): boolean {
  return stage.startsWith("relay_");
}

const RELAY_FAILURE_TITLES: Record<string, string> = {
  relay_unreachable: "CretLink could not reach the relay",
  relay_name_not_found: "The relay was not found",
  relay_untrusted: "This computer does not trust the relay",
  relay_no_answer: "The relay did not answer",
  relay_key_required: "The relay did not let this copy in",
  relay_key_refused: "The relay did not let this copy in",
  relay_rate_limited: "The relay asks you to wait",
  relay_busy: "The relay is full right now",
  relay_unavailable: "The relay lets nobody in right now",
  relay_version: "The relay and this copy do not fit together",
  relay_settings: "The relay settings of this copy cannot be used",
};

/** What a relay failure is called, in a few words for the customer. */
export function relayFailureTitle(stage: string): string {
  return RELAY_FAILURE_TITLES[stage] ?? "CretLink could not use the relay";
}

/**
 * A code that nobody waits on (ST-M007-012): the relay took the connection
 * in, and no technician was there. Nobody had any access.
 */
export function nobodyWaitsWords(host: string | undefined): UiError {
  return {
    title: "Nobody is waiting on that code",
    message: host
      ? `The relay ${host} found nobody waiting on the code you typed. It may be mistyped or already used, or your technician stopped waiting.`
      : "Nobody was waiting on the code you typed. It may be mistyped or already used, or your technician stopped waiting.",
    next: "Nobody has access to this computer. Ask your technician for the code on their screen and type it again. A code works once.",
  };
}

/**
 * A session through a relay whose connection was lost, said with the relay,
 * so that it does not read as a lost session of unknown cause (REQ-N-015).
 */
export function relayLostWords(host: string): { message: string; next: string } {
  return {
    message: `The connection through the relay ${host} was lost. The relay may have stopped, or one of the two computers went offline.`,
    next: "Nobody has access to this computer any more. Nothing connects again by itself: ask your technician for a new code when you want to start again.",
  };
}

// --- Names ---

/** The neutral name when the technician's computer sent none. */
export const NO_NAME = "your technician";

/** What the technician's computer says its user is called, or a neutral name
 * for the middle of a sentence. */
export function technicianName(s: CustomerSnapshot): string {
  return s.view?.technician?.display_name.trim() || s.report?.technicianName?.trim() || NO_NAME;
}

/** The name at the start of a sentence: a reported name is shown as it was
 * reported; only the neutral name is capitalised. */
export function capName(name: string): string {
  return name === NO_NAME ? "Your technician" : name;
}

/** "Sam's", for "<name>'s AI assistant". */
export function possessive(name: string): string {
  return `${capName(name)}'s`;
}

/** Who started an action, as the customer sees it. */
export function sourceWords(item: ActivityItem, name: string): string {
  switch (item.source) {
    case "ai":
      return `${possessive(name)} AI assistant`;
    case "terminal":
      return `${capName(name)} · terminal`;
    case "technician":
      return capName(name);
  }
}

/** The account this window runs as: `DOMAIN\user`. */
export function signedInAs(view: ConsentView): string {
  const { username, domain } = view.status.identity;
  return domain ? `${domain}\\${username}` : username || "Unknown";
}

export function adminRightsWords(view: ConsentView): string {
  const identity = view.status.identity;
  if (identity.elevated) return "Yes, approved in Windows";
  if (!identity.is_windows) return "No (not Windows: this is a development build)";
  return "No";
}

/** A discovered console's name and host name, as it reports itself. */
export function consoleName(console: DiscoveredConsoleView): string {
  const name = console.display_name?.trim();
  const host = console.instance_name.trim();
  if (name && host && name !== host) return `${name} · ${host}`;
  return name || host || "Unnamed console";
}

// --- The feed, for the customer ---

const FILTER_KINDS: Record<Exclude<Filter, "all">, readonly ActivityKind[]> = {
  commands: ["command", "job_control", "system_information"],
  files: ["file_sent", "file_received", "file_operation"],
  screen: ["screen_observed", "screen_controlled", "clipboard_read", "clipboard_written"],
};

/** The entries a filter shows, oldest first. Gaps show under every filter. */
export function filtered(feed: readonly ActivityEntry[], filter: Filter): ActivityEntry[] {
  if (filter === "all") return feed.slice();
  const kinds = FILTER_KINDS[filter];
  return feed.filter((e) => e.entry === "gap" || kinds.includes(e.item.kind));
}

export interface LiveCounts {
  commands: number;
  failedCommands: number;
  // Transfers that completed, as the end report counts them.
  filesSent: number;
  filesReceived: number;
  screenshots: number;
  running: number;
  notShown: number;
}

export function liveCounts(feed: readonly ActivityEntry[]): LiveCounts {
  const counts: LiveCounts = {
    commands: 0,
    failedCommands: 0,
    filesSent: 0,
    filesReceived: 0,
    screenshots: 0,
    running: 0,
    notShown: 0,
  };
  for (const entry of feed) {
    if (entry.entry === "gap") {
      counts.notShown += entry.count;
      continue;
    }
    const item = entry.item;
    const done = item.state.state === "done";
    if (item.state.state === "running") counts.running += 1;
    if (item.kind === "command") {
      counts.commands += 1;
      if (isFailure(item.state)) counts.failedCommands += 1;
    }
    if (item.kind === "file_sent" && done) counts.filesSent += 1;
    if (item.kind === "file_received" && done) counts.filesReceived += 1;
    if (item.kind === "screen_observed" && done) counts.screenshots += 1;
  }
  return counts;
}

/** The running item to show under Happening now: the most recent one. */
export function happeningNow(feed: readonly ActivityEntry[]): ActivityItem | undefined {
  for (let i = feed.length - 1; i >= 0; i -= 1) {
    const entry = feed[i];
    if (entry?.entry === "item" && entry.item.state.state === "running") return entry.item;
  }
  return undefined;
}

// The plain descriptions the backend builds name "the customer computer";
// on the customer's own screen that is this computer.
const CUSTOMER_WORDS: Partial<Record<ActivityKind, string>> = {
  file_sent: "Copied a file from this computer",
  file_received: "Copied a file to this computer",
};

/** The plain line of an item: the AI's stated purpose, or the description. */
export function plainLine(item: ActivityItem): string {
  return item.purpose?.trim() || CUSTOMER_WORDS[item.kind] || item.description;
}

/** An action's outcome in words; never "finished" for unknown work. */
export function outcomeWords(item: ActivityItem, now: number): string {
  const state = item.state;
  switch (state.state) {
    case "running": {
      const started = Date.parse(item.startedAt);
      return Number.isNaN(started) ? "running" : `running · ${formatElapsed(Math.max(0, now - started))}`;
    }
    case "done":
      return state.exitCode === undefined || state.exitCode === 0
        ? "finished"
        : `finished · exit code ${state.exitCode}`;
    case "failed":
      if (state.exitCode !== undefined) return `failed · error code ${state.exitCode}`;
      return state.message ? `failed: ${state.message}` : "failed";
    case "cancelled":
      return "cancelled";
    case "timed_out":
      return "timed out";
  }
}

// --- The screen tile ---

/** How long a desktop signal counts as "now". */
export const SCREEN_RECENT_MS = 30_000;

export type ScreenUse =
  | { state: "not_used" }
  | { state: "controlled" | "observed" }
  | { state: "earlier"; at: number };

/** The screen tile, from the agent's `DesktopActivity` signals (REQ-F-020). */
export function screenUse(view: ConsentView | null, now: number): ScreenUse {
  const observed = view?.screen.observed_at_ms ?? null;
  const controlled = view?.screen.controlled_at_ms ?? null;
  if (controlled !== null && now - controlled < SCREEN_RECENT_MS) return { state: "controlled" };
  if (observed !== null && now - observed < SCREEN_RECENT_MS) return { state: "observed" };
  const last = Math.max(observed ?? -1, controlled ?? -1);
  return last >= 0 ? { state: "earlier", at: last } : { state: "not_used" };
}

/** The short form, for the compact bar. */
export function screenShort(use: ScreenUse): string {
  switch (use.state) {
    case "not_used":
      return "screen not used";
    case "observed":
      return "screen observed";
    case "controlled":
      return "screen controlled";
    case "earlier":
      return "screen not in use now";
  }
}

/** The elapsed time of the band: "42 min", "1 h 5 min". */
export function bandElapsed(ms: number): string {
  const minutes = Math.floor(ms / 60_000);
  if (minutes < 1) return `${Math.floor(ms / 1000)} s`;
  if (minutes < 60) return `${minutes} min`;
  return `${Math.floor(minutes / 60)} h ${minutes % 60} min`;
}

export function screenWords(use: ScreenUse, time: (at: number) => string): string {
  switch (use.state) {
    case "not_used":
      return "not used";
    case "observed":
      return "being observed";
    case "controlled":
      return "being controlled";
    case "earlier":
      return `not in use now · last ${time(use.at)}`;
  }
}

// --- Announcements ---

/** What the polite live region says for the current screen (REQ-N-013). */
export function announcement(s: CustomerSnapshot): string {
  const name = technicianName(s);
  switch (screenFor(s)) {
    case "loading":
      return "Starting CretLink.";
    case "admin":
      return "No access. Windows will ask for permission when you continue.";
    case "declined":
      return "No access. Windows did not get permission.";
    case "find":
      return s.ended ? `No access. ${s.ended}` : "No access. Choose your technician.";
    case "connecting": {
      const relay = relayHost(s);
      return relay ? `Connecting through the relay ${relay}. No access.` : "Connecting. No access.";
    }
    case "compare":
      return `Connected to ${name}, but no access. Compare the code.`;
    case "consent":
      return `Your decision: allow ${name} full control? No control has been granted.`;
    case "live":
      return `Under full control. ${capName(name)} can operate this computer now.`;
    case "stopping":
      return "Control has ended. Cleaning up.";
    case "end":
      return s.report ? "The session has ended. The report is ready." : "The session has ended. Cleaning up.";
  }
}

export { formatElapsed };
