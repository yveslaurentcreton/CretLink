// The technician interface's state snapshot and its pure derivations
// (ST-M009-008).
//
// Every screen is a function of one `TechnicianSnapshot`. The Rust backend
// stays authoritative for the session (`ConsoleState`); the snapshot adds what
// the window itself remembers: the activity feed of the current or last
// session, the session history, the AI clients, this copy's details, which
// page or tab is shown, a pending confirmation, and where the file explorer
// stands (`./explorer`, ST-M010-002). Nothing here talks to Tauri, so all of
// it is tested in jsdom.

import type {
  ActivityEntry,
  ActivityItem,
  AiClientStatus,
  AppInfo,
  AuditActor,
  AuditEntryView,
  AuditOutcome,
  BridgeStatus,
  CleanupResult,
  ChosenCa,
  ClientId,
  ConsoleNotice,
  EndedBy,
  ListenNetwork,
  RelayForm,
  RelayPlace,
  RelaySettings,
  RelayTest,
  RelayView,
  SessionSummary,
} from "../shared/contract";
import {
  FEED_LIMIT,
  formatBytes,
  formatClock,
  formatElapsed,
  formatTime,
  groupCode,
  isActivityEvent,
  isFailure,
  plural,
  trimFeed,
  upsertActivity,
} from "../shared/feed";
import {
  type ConsoleState,
  type RelayRoute,
  type StopReason,
  type TargetIdentity,
  selectedTarget,
} from "./api";
import { type ExplorerState, initialExplorer } from "./explorer";

// The role-neutral feed helpers live in `../shared/feed` (ST-M009-009).
export {
  FEED_LIMIT,
  formatBytes,
  formatClock,
  formatElapsed,
  formatTime,
  groupCode,
  isActivityEvent,
  isFailure,
  plural,
  trimFeed,
  upsertActivity,
};

export type NavPage = "home" | "sessions" | "ai" | "settings";

export type LiveTab = "overview" | "terminal" | "files" | "audit";

/** A plain-language error with a next step, shown in an assertive alert. */
export interface UiError {
  title: string;
  message: string;
  next: string;
}

/** What the window remembers about the current or most recent session. */
export interface LastSession {
  id: string;
  target?: TargetIdentity;
  // Local clock (ms) when full control began and when the session ended.
  since?: number;
  endedAt?: number;
  reason?: StopReason;
  failure?: { stage: string; message: string };
}

/** The confirm step of the AI page: the exact commands, before anything runs. */
export interface AiConfirm {
  client: ClientId;
  action: "install" | "uninstall";
  // Installing over a registration that points at another program.
  replace: boolean;
}

/**
 * What the form of Settings → Relay holds beside its typed fields: which
 * certificate authority file the relay is checked against, the result of the
 * last test, and the question before a relay is removed (ST-M007-011).
 */
export interface RelayDraft {
  // `keep` the file the settings hold, use `none`, or the file just `chosen`.
  ca: { mode: "keep" } | { mode: "none" } | { mode: "chosen"; pick: number; name: string };
  test?: { state: "running" } | { state: "done"; result: RelayTest };
  // "Remove the relay?" is being asked.
  removing?: boolean;
  // Counts what was saved or removed: a new number draws the form anew, so
  // that no typed key stays in a field.
  round: number;
}

export function initialRelayDraft(round = 0): RelayDraft {
  return { ca: { mode: "keep" }, round };
}

export interface TechnicianSnapshot {
  console: ConsoleState;
  // The feed of the current or last session, oldest first.
  activity: ActivityEntry[];
  // The audit entries of the current or last session, oldest first
  // (ST-M009-008); refreshed, throttled, as the feed changes.
  audit: AuditEntryView[];
  // `null` while unavailable (the backend has no such command yet, or it
  // failed): the screens then say so calmly instead of guessing.
  sessions: SessionSummary[] | null;
  clients: AiClientStatus[] | null;
  // The local MCP bridge for the rail (ST-M009-008); `null` until the
  // backend has said, and again while a new session's endpoint starts.
  bridge: BridgeStatus | null;
  // The networks this computer can wait on (ST-M009-016); `null` until the
  // backend has said.
  networks: ListenNetwork[] | null;
  // The list of the other networks is open on the waiting screen.
  networksOpen?: boolean;
  app: AppInfo | null;
  // The release version from `get_role`, when `app` is unavailable.
  version: string | null;
  nav: NavPage;
  tab: LiveTab;
  // A report opened from the Sessions page.
  openReport?: string;
  last?: LastSession;
  // Done was pressed on the end report of `last`.
  endDismissed: boolean;
  // The end report is still being read from the sealed audit file.
  reportPending: boolean;
  confirm?: AiConfirm;
  // The uninstall plan, shown before anything is removed; `null` while loading.
  uninstall?: { plan: string[] | null };
  uninstalled?: boolean;
  // Which relay this copy uses and where that comes from (ST-M007-011);
  // `null` until the backend has said.
  relay: RelaySettings | null;
  relayDraft: RelayDraft;
  // The file explorer of the Files tab (ST-M010-002): where it stands in the
  // customer computer's files, and what is listed there. It belongs to one
  // session: it starts empty with full control and is dropped when that ends.
  explorer: ExplorerState;
  // An action in flight, to disable its control.
  busy?: string;
  // A polite announcement of the last change (aria-live).
  notice?: string;
  error?: UiError;
  now: number;
}

/** The screen a snapshot shows. */
export type Screen =
  | "home"
  | "sessions"
  | "report"
  | "ai"
  | "settings"
  | "wait"
  | "connecting"
  | "compare"
  | "consent"
  | "live"
  | "stopping"
  | "end";


export function initialSnapshot(console: ConsoleState, now: number): TechnicianSnapshot {
  return {
    console,
    activity: [],
    audit: [],
    sessions: null,
    clients: null,
    bridge: null,
    networks: null,
    app: null,
    version: null,
    nav: "home",
    tab: "overview",
    endDismissed: false,
    reportPending: false,
    relay: null,
    relayDraft: initialRelayDraft(),
    explorer: initialExplorer(),
    now,
  };
}

/** A console state for the moment before `get_state` has answered. */
export function placeholderConsole(): ConsoleState {
  return {
    technician: { label: "This computer", hostname: "", os: "" },
    targets: [],
    phase: { phase: "idle" },
  };
}

// --- Phases ---

type PhaseName = ConsoleState["phase"]["phase"];

const SESSION_PHASES: readonly PhaseName[] = [
  "listening",
  "connecting",
  "awaiting_approval_code",
  "awaiting_consent",
  "full_control",
  "stopping",
];

/** Phases in which `Escape` and Stop end the session (REQ-F-018). */
const STOPPABLE_PHASES: readonly PhaseName[] = [
  "listening",
  "connecting",
  "awaiting_approval_code",
  "awaiting_consent",
  "full_control",
];

export function inSession(console: ConsoleState): boolean {
  return SESSION_PHASES.includes(console.phase.phase);
}

export function canStop(console: ConsoleState): boolean {
  return STOPPABLE_PHASES.includes(console.phase.phase);
}

function isRestPhase(phase: PhaseName): boolean {
  return phase === "idle" || phase === "discovered" || phase === "stopped" || phase === "failed";
}

function isEndPhase(phase: PhaseName): boolean {
  return phase === "stopped" || phase === "failed";
}

export function screenFor(s: TechnicianSnapshot): Screen {
  const phase = s.console.phase.phase;
  // A change of network stops waiting on one address and starts on another:
  // the screen stays the waiting screen in between (ST-M009-016). So does the
  // change from the network to the relay (ST-M007-013).
  if (s.busy === "network" || s.busy === "relay") return "wait";
  if (phase === "full_control") return "live";
  if (phase === "stopping") return "stopping";
  if (SESSION_PHASES.includes(phase) && s.nav === "home") {
    switch (phase) {
      case "listening":
        return "wait";
      case "connecting":
        return "connecting";
      case "awaiting_approval_code":
        return "compare";
      default:
        return "consent";
    }
  }
  if (isEndPhase(phase) && s.last && !s.endDismissed) return "end";
  if (s.nav === "sessions" && s.openReport !== undefined) return "report";
  return s.nav;
}

/**
 * Folds a new `ConsoleState` into the snapshot. Returns the new snapshot and
 * whether the session just ended (the caller then refreshes the history).
 *
 * - A new session (a rest phase giving way to `listening`) clears the feed of
 *   the last one; nothing else does (the feed survives tab switches and the
 *   end of the session).
 * - The session id and the target identity are remembered, because the
 *   console clears them when the session ends.
 * - A phase change of a session brings the session screen back to the front.
 * - The file explorer belongs to one session's full control: it starts
 *   empty when full control begins and is dropped when it ends, so no file
 *   name of one customer computer outlives its session (ST-M010-002).
 */
export function applyConsole(
  s: TechnicianSnapshot,
  next: ConsoleState,
  now: number,
): { snapshot: TechnicianSnapshot; ended: boolean } {
  const before = s.console.phase.phase;
  const after = next.phase.phase;
  let out: TechnicianSnapshot = { ...s, console: next, now };
  let ended = false;
  if (isRestPhase(before) && after === "listening") {
    out = {
      ...out,
      activity: [],
      audit: [],
      last: undefined,
      endDismissed: false,
      reportPending: false,
      openReport: undefined,
      tab: "overview",
    };
  }
  if (next.session) {
    const target = selectedTarget(next)?.identity;
    const last: LastSession =
      out.last && out.last.id === next.session ? { ...out.last } : { id: next.session };
    if (target) last.target = target;
    out.last = last;
  } else if (out.last) {
    const target = selectedTarget(next)?.identity;
    if (target && !out.last.target) out.last = { ...out.last, target };
  }
  if (after === "full_control" && before !== "full_control" && out.last) {
    out.last = { ...out.last, since: out.last.since ?? now };
    out.tab = "overview";
  }
  if ((after === "full_control") !== (before === "full_control")) {
    out.explorer = initialExplorer();
  }
  if (isEndPhase(after) && !isEndPhase(before)) {
    ended = true;
    if (out.last) {
      const last: LastSession = { ...out.last, endedAt: now };
      if (next.phase.phase === "stopped") last.reason = next.phase.reason;
      if (next.phase.phase === "failed") {
        last.failure = { stage: next.phase.stage, message: next.phase.message };
      }
      out.last = last;
      out.endDismissed = false;
      out.reportPending = true;
    }
  }
  if (before !== after && SESSION_PHASES.includes(after)) {
    out.nav = "home";
    out.openReport = undefined;
  }
  return { snapshot: out, ended };
}

// --- Activity feed (ST-M009-011) ---

export function feedItems(feed: readonly ActivityEntry[]): ActivityItem[] {
  const items: ActivityItem[] = [];
  for (const entry of feed) if (entry.entry === "item") items.push(entry.item);
  return items;
}

export interface SessionCounts {
  total: number;
  ai: number;
  terminal: number;
  technician: number;
  filesSent: number;
  filesReceived: number;
  failed: number;
  running: number;
  // Entries that were not shown (gap markers).
  notShown: number;
  // The AI clients that acted in this session, by label.
  clients: string[];
  firstFailure?: ActivityItem;
}

export function sessionCounts(feed: readonly ActivityEntry[]): SessionCounts {
  const counts: SessionCounts = {
    total: 0,
    ai: 0,
    terminal: 0,
    technician: 0,
    filesSent: 0,
    filesReceived: 0,
    failed: 0,
    running: 0,
    notShown: 0,
    clients: [],
  };
  for (const entry of feed) {
    if (entry.entry === "gap") {
      counts.notShown += entry.count;
      continue;
    }
    const item = entry.item;
    counts.total += 1;
    counts[item.source] += 1;
    // Only a finished transfer counts as a file sent or received, as in the
    // history reader and the customer's counts (ST-M009-012).
    const finished = item.state.state === "done";
    if (finished && item.kind === "file_sent") counts.filesSent += 1;
    if (finished && item.kind === "file_received") counts.filesReceived += 1;
    if (isFailure(item.state)) {
      counts.failed += 1;
      counts.firstFailure ??= item;
    }
    if (item.state.state === "running") counts.running += 1;
    if (item.source === "ai") {
      const label = clientLabel(item);
      if (!counts.clients.includes(label)) counts.clients.push(label);
    }
  }
  return counts;
}

export function clientLabel(item: ActivityItem): string {
  return item.clientLabel?.trim() || "AI assistant";
}

/** Who started an action, in words. */
export function sourceLabel(item: ActivityItem): string {
  switch (item.source) {
    case "ai":
      return clientLabel(item);
    case "terminal":
      return "Terminal";
    case "technician":
      return "You";
  }
}

const AUDIT_ACTORS: readonly AuditActor[] = ["ai", "technician", "terminal", "agent", "system"];
const AUDIT_OUTCOMES: readonly AuditOutcome[] = ["ok", "failed", "cancelled", "timeout", "unknown"];

/** Whether `value` is an audit entry of the contract; anything else is dropped. */
export function isAuditEntry(value: unknown): value is AuditEntryView {
  if (typeof value !== "object" || value === null) return false;
  const v = value as Partial<Record<keyof AuditEntryView, unknown>>;
  return (
    typeof v.seq === "number" &&
    typeof v.at === "string" &&
    typeof v.action === "string" &&
    typeof v.summary === "string" &&
    AUDIT_ACTORS.includes(v.actor as AuditActor) &&
    AUDIT_OUTCOMES.includes(v.outcome as AuditOutcome)
  );
}

/** Who an audit record is attributed to, in words. */
export function auditActorWords(actor: AuditActor): string {
  switch (actor) {
    case "ai":
      return "AI assistant";
    case "technician":
      return "You";
    case "terminal":
      return "Terminal";
    case "agent":
      return "Customer computer";
    case "system":
      return "CretLink";
  }
}

/** A record kind in words: `consent_granted` reads "consent granted". */
export function auditActionWords(action: string): string {
  return action.replace(/_/g, " ");
}

/** An audit outcome in words; an unknown one never reads as a success. */
export function auditOutcomeWords(outcome: AuditOutcome): string {
  switch (outcome) {
    case "ok":
      return "ok";
    case "failed":
      return "failed";
    case "cancelled":
      return "cancelled";
    case "timeout":
      return "timed out";
    case "unknown":
      return "unknown, not confirmed";
  }
}

/** The result of an action in words; never "done" for unknown work. */
export function stateWords(item: ActivityItem, now: number): string {
  const state = item.state;
  switch (state.state) {
    case "running": {
      const started = Date.parse(item.startedAt);
      return Number.isNaN(started)
        ? "running"
        : `running · ${formatElapsed(Math.max(0, now - started))}`;
    }
    case "done":
      return state.exitCode === undefined ? "done" : `exit ${state.exitCode}`;
    case "failed":
      if (state.exitCode !== undefined) return `failed · exit ${state.exitCode}`;
      return state.message ? `failed: ${state.message}` : "failed";
    case "cancelled":
      return "cancelled";
    case "timed_out":
      return "timed out";
  }
}

// --- Words for the end of a session and its cleanup (ST-M009-012) ---

export function cleanupWords(cleanup: CleanupResult): string {
  switch (cleanup.state) {
    case "cleaned_up":
      return "Cleaned up";
    case "residuals":
      return `${cleanup.residuals} ${cleanup.residuals === 1 ? "residual" : "residuals"}`;
    case "failed":
      return "Cleanup failed";
    case "unknown":
      return "Cleanup unknown";
  }
}

export function endedByWords(endedBy: EndedBy | undefined): string {
  switch (endedBy) {
    case "technician":
      return "ended by you";
    case "customer":
      return "ended by the customer";
    case "connection_lost":
      return "the connection was lost";
    case "failure":
      return "ended by a failure";
    default:
      return "end not recorded";
  }
}

export function stopReasonEndedBy(last: LastSession | undefined): EndedBy | undefined {
  if (!last) return undefined;
  if (last.failure) return "failure";
  switch (last.reason) {
    case "technician_requested":
      return "technician";
    case "customer_requested":
      return "customer";
    case "target_stale":
    case "disconnected":
      return "connection_lost";
    default:
      return undefined;
  }
}

/** The summary of `id` in the history, if it has been read yet. */
export function summaryFor(
  s: TechnicianSnapshot,
  id: string | undefined,
): SessionSummary | undefined {
  if (id === undefined || !s.sessions) return undefined;
  return s.sessions.find((summary) => summary.id === id);
}

// --- Names and formatting ---

export function shortId(id: string): string {
  return id.slice(0, 6);
}

/** What the customer picks in their list: the display name and hostname. */
/** The host of a listen address: `10.0.0.42:7000` and `[fd00::1]:7000` without the port. */
export function listenHost(address: string): string {
  const bracketed = /^\[([^\]]+)\](?::\d+)?$/.exec(address);
  if (bracketed?.[1]) return bracketed[1];
  const colon = address.lastIndexOf(":");
  return colon > 0 && address.indexOf(":") === colon ? address.slice(0, colon) : address;
}

/** The network the session waits on, when the backend listed it (ST-M009-016). */
export function waitingNetwork(s: TechnicianSnapshot): ListenNetwork | undefined {
  const phase = s.console.phase;
  if (phase.phase !== "listening") return undefined;
  const host = listenHost(phase.address);
  return s.networks?.find((network) => network.address === host);
}

/** The networks this computer could wait on instead of the current one. */
export function otherNetworks(s: TechnicianSnapshot): ListenNetwork[] {
  const phase = s.console.phase;
  if (phase.phase !== "listening" || !s.networks) return [];
  const host = listenHost(phase.address);
  return s.networks.filter((network) => network.address !== host);
}

export function technicianPick(console: ConsoleState): string {
  const host = console.technician.hostname;
  const name = console.technician.display_name?.trim();
  if (name && host) return `${name} · ${host}`;
  return name || host || "this computer";
}

/** How the customer's terminal header names the technician. */
export function technicianName(console: ConsoleState): string {
  return console.technician.display_name?.trim() || console.technician.hostname || "Technician";
}

export function targetName(s: TechnicianSnapshot): string {
  return (
    selectedTarget(s.console)?.identity?.hostname ??
    s.last?.target?.hostname ??
    selectedTarget(s.console)?.display_name ??
    "the customer computer"
  );
}

export function currentTarget(s: TechnicianSnapshot): TargetIdentity | undefined {
  return selectedTarget(s.console)?.identity ?? s.last?.target;
}

export function formatDuration(ms: number | undefined): string {
  if (ms === undefined || Number.isNaN(ms)) return "duration unknown";
  const minutes = Math.round(ms / 60000);
  if (minutes < 1) return "less than a minute";
  if (minutes < 60) return `${minutes} min`;
  const hours = Math.floor(minutes / 60);
  return minutes % 60 === 0 ? `${hours} h` : `${hours} h ${minutes % 60} min`;
}

const WEEKDAYS = ["Sunday", "Monday", "Tuesday", "Wednesday", "Thursday", "Friday", "Saturday"];
const MONTHS = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];

/** "Today 14:02", "Yesterday", "Monday", "3 Oct 2026". */
export function formatDay(at: string | undefined, now: number): string {
  if (at === undefined) return "Date unknown";
  const date = new Date(at);
  if (Number.isNaN(date.getTime())) return "Date unknown";
  const startOfDay = (d: Date): number =>
    new Date(d.getFullYear(), d.getMonth(), d.getDate()).getTime();
  const days = Math.round((startOfDay(new Date(now)) - startOfDay(date)) / 86400000);
  if (days === 0) return `Today ${formatTime(at)}`;
  if (days === 1) return "Yesterday";
  if (days > 1 && days < 7) return WEEKDAYS[date.getDay()] ?? "";
  return `${date.getDate()} ${MONTHS[date.getMonth()] ?? ""} ${date.getFullYear()}`;
}

/** "Today 14:02", "Yesterday 09:15", "Monday 16:40", "3 Oct 2026 11:00". */
export function formatWhen(at: string | undefined, now: number): string {
  const day = formatDay(at, now);
  if (at === undefined || day === "Date unknown" || day.startsWith("Today")) return day;
  return `${day} ${formatTime(at)}`;
}

/** What a polite live region says for the current phase (REQ-N-013). */
export function phaseAnnouncement(s: TechnicianSnapshot): string {
  const phase = s.console.phase;
  switch (phase.phase) {
    case "listening": {
      const relay = s.console.relay;
      if (!relay) return "Waiting for the customer. No access.";
      return relay.admitted
        ? `Waiting for the customer on the relay ${relay.host}. Read them the relay code. No access.`
        : `Connecting to the relay ${relay.host}. No access.`;
    }
    case "connecting":
      return "The customer is connecting. No access yet.";
    case "awaiting_approval_code":
      return `Compare the approval code with the customer. ${targetName(s)} has no session yet.`;
    case "awaiting_consent":
      return `Codes match. ${targetName(s)} is deciding whether to grant full control.`;
    case "full_control":
      return `Full control of ${targetName(s)}.`;
    case "stopping":
      return "Ending the session.";
    case "stopped":
      return "The session has ended. No access.";
    case "failed":
      return "The session failed. No access.";
    default:
      return "Ready to help. No session.";
  }
}

/** Whether a value is a `ListenNetwork` of the contract (ST-M009-016). */
export function isListenNetwork(value: unknown): value is ListenNetwork {
  if (typeof value !== "object" || value === null) return false;
  const network = value as Record<string, unknown>;
  return (
    typeof network.address === "string" &&
    network.address !== "" &&
    typeof network.adapter === "string" &&
    typeof network.pointToPoint === "boolean"
  );
}

// --- A session through the relay (ST-M007-013) ---

/** The relay the console waits on, while it waits for the customer there. */
export function waitingRelay(console: ConsoleState): RelayRoute | undefined {
  return console.phase.phase === "listening" ? console.relay : undefined;
}

/** A relay code as the groups it is read in: `53ST-A71D-JKKV-0VGH` is four. */
export function relayCodeGroups(code: string): string[] {
  return code.split("-").filter((group) => group !== "");
}

/** A relay code as a screen reader says it: every character on its own. */
export function spokenCode(code: string): string {
  return relayCodeGroups(code)
    .map((group) => Array.from(group).join(" "))
    .join(", ");
}

/** What the waiting screen can say about a relay for this copy. */
export type RelayOffer =
  // A relay that can be used, by its host name.
  | { state: "ready"; host: string }
  // No relay. `runOnce`: this copy keeps no settings, so a relay goes into
  // the file next to the program.
  | { state: "none"; runOnce: boolean }
  // A profile that cannot be used: what is wrong, and the file.
  | { state: "problem"; message: string; file: string };

export function relayOffer(settings: RelaySettings | null): RelayOffer {
  const relay = activeRelay(settings);
  if (relay) return { state: "ready", host: relay.host };
  const place = settings?.source === "file" ? settings.file : settings?.stored;
  if (place?.problem) {
    return { state: "problem", message: place.problem.message, file: place.problem.file };
  }
  return { state: "none", runOnce: settings !== null && !settings.editable };
}

/** The stage of a relay that could not be used: `relay_unreachable`, … */
export function isRelayFailure(stage: string): boolean {
  return stage.startsWith("relay_");
}

/**
 * The stage of a session through a relay that lost its connection before it
 * was under control. The backend takes it from the session's own verdict.
 */
export const RELAY_LOST_STAGE = "relay_lost";

const RELAY_FAILURE_TITLES: Record<string, string> = {
  relay_unreachable: "The relay cannot be reached",
  relay_name_not_found: "The relay was not found",
  relay_untrusted: "The relay's certificate is not trusted",
  relay_no_answer: "The relay did not answer",
  relay_key_required: "The relay asks for an admission key",
  relay_key_refused: "The relay refused the admission key",
  relay_rate_limited: "The relay asks you to wait",
  relay_busy: "The relay is full",
  relay_unavailable: "The relay admits nobody right now",
  relay_version: "The relay and this copy do not fit together",
  relay_settings: "The relay settings cannot be used",
};

/** What a relay failure is called, in a few words. */
export function relayFailureTitle(stage: string): string {
  return RELAY_FAILURE_TITLES[stage] ?? "The relay could not be used";
}

/**
 * Why a session through a relay ended, when its connection was lost: said
 * with the relay, so that it does not read as a lost session of unknown
 * cause (REQ-N-015). The relay may have stopped; either computer may have
 * gone offline. Nothing connects again by itself.
 */
export function relayLostWords(host: string): { message: string; next: string } {
  return {
    message: `The connection through the relay ${host} was lost. The relay may have stopped, or one of the two computers went offline.`,
    next: "Nobody has access to the customer computer any more. Start a new session when you are ready; it gets a new relay code.",
  };
}

// --- The relay of this copy (ST-M007-011) ---

/** The relay this copy uses, when its profile can be used. */
export function activeRelay(relay: RelaySettings | null): RelayView | undefined {
  if (!relay) return undefined;
  if (relay.source === "file") return relay.file.relay;
  if (relay.source === "settings") return relay.stored?.relay;
  return undefined;
}

/**
 * Reads the form of Settings → Relay as the backend takes it. An empty key
 * field keeps the key that is kept, unless the technician asked to remove
 * it; without a kept key it means that the relay asks for none.
 */
export function relayForm(
  fields: { address: string; key: string; removeKey: boolean; name: string },
  draft: RelayDraft,
  stored: RelayView | undefined,
): RelayForm {
  const form: RelayForm = {
    address: fields.address.trim(),
    keyMode: "none",
    caMode: draft.ca.mode,
  };
  if (fields.key !== "") {
    form.keyMode = "set";
    form.key = fields.key;
  } else if (stored?.keySet && !fields.removeKey) {
    form.keyMode = "keep";
  }
  if (draft.ca.mode === "chosen") form.caPick = draft.ca.pick;
  // Nothing is kept that could be kept: the system's authorities.
  if (draft.ca.mode === "keep" && !stored?.ca) form.caMode = "none";
  const name = fields.name.trim();
  if (name !== "") form.name = name;
  return form;
}

function isText(value: unknown): value is string {
  return typeof value === "string";
}

function isRelayView(value: unknown): value is RelayView {
  if (typeof value !== "object" || value === null) return false;
  const view = value as Record<string, unknown>;
  return (
    isText(view.host) &&
    isText(view.address) &&
    typeof view.keySet === "boolean" &&
    (view.ca === undefined || isText(view.ca)) &&
    (view.name === undefined || isText(view.name))
  );
}

function isRelayPlace(value: unknown): value is RelayPlace {
  if (typeof value !== "object" || value === null) return false;
  const place = value as Record<string, unknown>;
  const problem = place.problem as Record<string, unknown> | undefined;
  return (
    isText(place.path) &&
    (place.relay === undefined || isRelayView(place.relay)) &&
    (problem === undefined ||
      (typeof problem === "object" &&
        problem !== null &&
        isText(problem.file) &&
        isText(problem.message)))
  );
}

/** Whether a value is a `RelaySettings` of the contract (ST-M007-011). */
export function isRelaySettings(value: unknown): value is RelaySettings {
  if (typeof value !== "object" || value === null) return false;
  const settings = value as Record<string, unknown>;
  return (
    typeof settings.editable === "boolean" &&
    (settings.source === "file" || settings.source === "settings" || settings.source === "none") &&
    isRelayPlace(settings.file) &&
    (settings.stored === undefined || isRelayPlace(settings.stored))
  );
}

/** Whether a value is a `RelayTest` of the contract (ST-M007-011). */
export function isRelayTest(value: unknown): value is RelayTest {
  if (typeof value !== "object" || value === null) return false;
  const test = value as Record<string, unknown>;
  return (
    typeof test.ok === "boolean" && isText(test.kind) && isText(test.message) && isText(test.next)
  );
}

/** Whether a value is a `ChosenCa` of the contract (ST-M007-011). */
export function isChosenCa(value: unknown): value is ChosenCa {
  if (typeof value !== "object" || value === null) return false;
  const chosen = value as Record<string, unknown>;
  return typeof chosen.pick === "number" && isText(chosen.name);
}

/** Whether a value is a `ConsoleNotice` of the contract (ST-M007-011). */
export function isConsoleNotice(value: unknown): value is ConsoleNotice {
  if (typeof value !== "object" || value === null) return false;
  const notice = value as Record<string, unknown>;
  return isText(notice.title) && isText(notice.message) && isText(notice.next);
}

/** Whether a value is a `BridgeStatus` of the contract (ST-M009-008). */
export function isBridgeStatus(value: unknown): value is BridgeStatus {
  if (typeof value !== "object" || value === null) return false;
  const status = value as Record<string, unknown>;
  return (
    (status.endpoint === "ready" || status.endpoint === "unavailable") &&
    typeof status.tools === "number" &&
    Array.isArray(status.clients) &&
    status.clients.every((label) => typeof label === "string") &&
    (status.message === undefined || typeof status.message === "string")
  );
}
