// Snapshot fixtures of every customer screen and state (ST-M009-009), used by
// the rendering tests and the dev-only gallery (`dev/gallery.html`).

import type { ActivityEntry, ActivityItem, CustomerReport } from "../shared/contract";
import { DISCLOSURE_TEXT } from "../technician/disclosure";
import type { AgentPhase, ConsentView, DiscoveredConsoleView, LaunchView } from "./api";
import { type CustomerSnapshot, initialSnapshot } from "./model";

/** 2026-11-03 14:44:00 UTC: the fixtures' "now". */
export const NOW = Date.UTC(2026, 10, 3, 14, 44, 0);
const minutes = (n: number): number => n * 60_000;
const iso = (ms: number): string => new Date(ms).toISOString();
const SINCE = NOW - minutes(42);

export const XSS = '<img src=x onerror="alert(1)"><script>alert(2)</script>';

export function viewIn(phase: AgentPhase, patch: Partial<ConsentView> = {}): ConsentView {
  const live =
    phase.phase !== "idle" && phase.phase !== "stopped" && phase.phase !== "failed";
  return {
    status: {
      phase,
      identity: {
        username: "jdoe",
        domain: "ACME",
        elevated: true,
        integrity: "high",
        is_windows: true,
        note: null,
      },
      control_active: phase.phase === "full_control",
    },
    hostname: "ACME-PC-07",
    session_active: live,
    control_since_ms: phase.phase === "full_control" ? SINCE : null,
    screen: { observed_at_ms: null, controlled_at_ms: null },
    approval_code: null,
    disclosure: DISCLOSURE_TEXT,
    simulated: false,
    can_connect: !live,
    technician:
      phase.phase === "idle"
        ? null
        : { display_name: "Sam", provenance: "as the technician's computer reports it" },
    startup_cleanup: null,
    ...patch,
  };
}

export const LAUNCH_READY: LaunchView = {
  step: "ready",
  back: "start",
  elevated: true,
  uac: true,
  demo: false,
};

export const CONSOLES: DiscoveredConsoleView[] = [
  {
    instance_name: "TECH-LAPTOP",
    display_name: "Sam",
    address: "10.0.0.42:51234",
    protocol_version: "1.11",
    dialable: true,
  },
  {
    instance_name: "CONSOLE-3F1",
    display_name: null,
    address: "",
    protocol_version: null,
    dialable: false,
  },
];

function item(patch: Partial<ActivityItem> & { id: string }): ActivityEntry {
  return {
    entry: "item",
    item: {
      source: "ai",
      description: "Ran a command",
      kind: "command",
      operation: "run a command in PowerShell: Get-Process",
      state: { state: "done", exitCode: 0 },
      startedAt: iso(NOW - minutes(10)),
      durationMs: 1200,
      ...patch,
    },
  };
}

/** The mock's live feed, oldest first. */
export const FEED: ActivityEntry[] = [
  item({
    id: "job-1",
    source: "terminal",
    description: "Used the terminal",
    purpose: undefined,
    operation: "run a command in PowerShell: pnputil /enum-drivers /class Net",
    startedAt: iso(NOW - minutes(16)),
  }),
  item({
    id: "job-2",
    purpose: "Checked the Wi-Fi driver",
    operation: "run a command in cmd: netsh wlan show drivers",
    state: { state: "failed", exitCode: 1 },
    startedAt: iso(NOW - minutes(15)),
  }),
  item({
    id: "req-3",
    purpose: "Sent setupapi.dev.log to Sam",
    description: "Copied a file from the customer computer",
    kind: "file_sent",
    operation: "read a file",
    path: "C:\\Windows\\INF\\setupapi.dev.log",
    bytes: 1_258_291,
    state: { state: "done" },
    startedAt: iso(NOW - minutes(14)),
  }),
  item({
    id: "req-4",
    source: "technician",
    description: "Copied a file to the customer computer",
    kind: "file_received",
    operation: "save a file",
    path: "C:\\Users\\jdoe\\AppData\\Local\\Temp\\cretlink\\Intel-WiFi-23.60.zip",
    bytes: 19_293_798,
    state: { state: "done" },
    startedAt: iso(NOW - minutes(13)),
  }),
  item({
    id: "req-5",
    purpose: "Took a picture of your screen",
    description: "Took a picture of the screen",
    kind: "screen_observed",
    operation: "take a screenshot",
    state: { state: "done" },
    startedAt: iso(NOW - minutes(12)),
  }),
  item({
    id: "job-6",
    purpose: "Reading the Windows system log",
    operation: "run a command in PowerShell: Get-WinEvent -LogName System -MaxEvents 50",
    state: { state: "running" },
    startedAt: iso(NOW - 12_000),
    durationMs: undefined,
  }),
];

export const FEED_DONE: ActivityEntry[] = FEED.map((e) =>
  e.entry === "item" && e.item.state.state === "running"
    ? { entry: "item", item: { ...e.item, state: { state: "done", exitCode: 0 } } }
    : e,
);

export const FEED_GAP: ActivityEntry[] = [
  ...FEED_DONE.slice(0, 2),
  { entry: "gap", count: 12, at: iso(NOW - minutes(14)) },
  ...FEED_DONE.slice(2),
];

export const FEED_XSS: ActivityEntry[] = [
  item({ id: "x1", purpose: XSS, operation: XSS, path: XSS, description: XSS, state: { state: "running" } }),
  item({ id: "x2", kind: "file_sent", purpose: XSS, operation: XSS, path: XSS, startedAt: iso(NOW - minutes(1)) }),
];

const COUNTS = { commands: 3, failedCommands: 1, filesSent: 1, filesReceived: 1, screenshots: 1 };

export const REPORT: CustomerReport = {
  endedBy: "customer",
  technicianName: "Sam",
  hostname: "ACME-PC-07",
  controlGranted: true,
  startedAt: iso(SINCE),
  endedAt: iso(NOW),
  durationMs: minutes(42),
  counts: COUNTS,
  cleanup: {
    state: "cleaned_up",
    residuals: 0,
    notes: [
      { ok: true, text: "connection closed" },
      { ok: true, text: "2 running jobs stopped" },
      { ok: true, text: "staging area removed (3 files)" },
    ],
  },
  activity: FEED_DONE,
};

export const REPORT_RESIDUALS: CustomerReport = {
  ...REPORT,
  endedBy: "technician",
  cleanup: {
    state: "residuals",
    residuals: 1,
    notes: [
      { ok: true, text: "connection closed" },
      { ok: false, text: "staged file left: C:\\Users\\jdoe\\AppData\\Local\\Temp\\cretlink\\driver.zip (in use)" },
      { ok: true, text: "2 running jobs stopped" },
    ],
  },
};

export const REPORT_FAILED: CustomerReport = {
  ...REPORT,
  endedBy: "failure",
  cleanup: {
    state: "failed",
    residuals: 0,
    notes: [
      { ok: true, text: "connection closed" },
      { ok: false, text: "staging cleanup failed: access denied" },
    ],
  },
};

export const REPORT_UNKNOWN: CustomerReport = {
  ...REPORT,
  endedBy: "connection_lost",
  cleanup: {
    state: "unknown",
    residuals: 0,
    notes: [{ ok: false, text: "no cleanup report arrived before the connection closed" }],
  },
};

function base(view: ConsentView, patch: Partial<CustomerSnapshot> = {}): CustomerSnapshot {
  return {
    ...initialSnapshot("main", NOW),
    view,
    launch: LAUNCH_READY,
    ...patch,
  };
}

const idle = viewIn({ phase: "idle" });
const idleUnelevated = viewIn(
  { phase: "idle" },
  {
    status: {
      ...idle.status,
      identity: { ...idle.status.identity, elevated: false, integrity: "medium" },
    },
  },
);
const live = viewIn({ phase: "full_control", session: "7f3a9c51" });
const stoppedByCustomer = viewIn({ phase: "stopped", reason: "customer_requested" });

export const FIXTURES: Record<string, CustomerSnapshot> = {
  admin: base(idleUnelevated, { launch: { ...LAUNCH_READY, step: "admin", elevated: false } }),
  "admin-support-copy": base(idleUnelevated, {
    launch: { ...LAUNCH_READY, step: "admin", back: "close", elevated: false },
  }),
  "admin-declined": base(idleUnelevated, {
    launch: { ...LAUNCH_READY, step: "declined", elevated: false },
  }),
  "admin-dev": base(idleUnelevated, {
    launch: { ...LAUNCH_READY, step: "admin", elevated: false, uac: false, demo: true },
  }),
  find: base(idle, { discovery: { state: "done", consoles: CONSOLES } }),
  "find-leftovers": base(
    { ...idle, startup_cleanup: {
        failed: 2,
        command: "& 'C:\\Users\\ann\\Downloads\\CretLink-Support.exe' recover --remove | Out-Host",
      }, },
    { discovery: { state: "done", consoles: CONSOLES } },
  ),
  "find-searching": base(idle, { discovery: { state: "searching", consoles: [] } }),
  "find-none": base(idleUnelevated, {
    discovery: {
      state: "done",
      consoles: [],
      problem: "No private network was found to search; ask the technician to read out the connection code instead",
    },
  }),
  "find-after-decline": base(stoppedByCustomer, {
    discovery: { state: "done", consoles: CONSOLES },
    ended: "You stopped before anyone got access. Nothing was shared.",
  }),
  "find-connect-failed": base(
    viewIn({ phase: "failed", stage: "connect", message: "could not reach the technician: connection refused" }),
    { discovery: { state: "done", consoles: CONSOLES }, ended: "" },
  ),
  connecting: base(viewIn({ phase: "announced" }, { technician: null })),
  sac: base(viewIn({ phase: "awaiting_approval_compare" }, { approval_code: "482913" })),
  consent: base(viewIn({ phase: "awaiting_consent" })),
  "live-running": base(live, {
    activity: FEED,
    hadControl: true,
    view: { ...live, screen: { observed_at_ms: NOW - 5_000, controlled_at_ms: NOW - 2_000 } },
  }),
  "live-none": base(live, { activity: FEED_DONE, hadControl: true }),
  "live-empty": base(live, { activity: [], hadControl: true }),
  "live-gap": base(live, { activity: FEED_GAP, hadControl: true }),
  "live-files": base(live, { activity: FEED_DONE, hadControl: true, filter: "files" }),
  "live-xss": base(
    { ...live, technician: { display_name: XSS, provenance: "as the technician's computer reports it" } },
    { activity: FEED_XSS, hadControl: true },
  ),
  "live-close": base(live, { activity: FEED, hadControl: true, confirmClose: true }),
  "live-closing": base(viewIn({ phase: "stopped", reason: "customer_requested" }), {
    activity: FEED_DONE,
    hadControl: true,
    confirmClose: true,
    closing: true,
  }),
  stopping: base(viewIn({ phase: "stopping" }), { activity: FEED_DONE, hadControl: true }),
  "end-pending": base(stoppedByCustomer, { activity: FEED_DONE, hadControl: true }),
  "end-by-customer": base(stoppedByCustomer, { activity: FEED_DONE, hadControl: true, report: REPORT }),
  "end-by-technician": base(viewIn({ phase: "stopped", reason: "technician_requested" }), {
    activity: FEED_DONE,
    hadControl: true,
    report: REPORT_RESIDUALS,
  }),
  "end-lost": base(viewIn({ phase: "stopped", reason: "disconnected" }), {
    activity: FEED_DONE,
    hadControl: true,
    report: REPORT_UNKNOWN,
  }),
  "end-failed": base(viewIn({ phase: "failed", stage: "session", message: "the transport closed before the session ended" }), {
    activity: FEED_DONE,
    hadControl: true,
    report: REPORT_FAILED,
  }),
  "end-saved": base(stoppedByCustomer, {
    activity: FEED_DONE,
    hadControl: true,
    report: REPORT,
    saved: "C:\\Users\\jdoe\\Documents\\CretLink\\2026-11-03 ACME-PC-07.html",
  }),
};

/** The compact bar's fixtures. */
export const MINI_FIXTURES: Record<string, CustomerSnapshot> = {
  "mini-running": { ...FIXTURES["live-running"]!, window: "mini" },
  "mini-idle": { ...FIXTURES["live-none"]!, window: "mini" },
  "mini-consent": { ...FIXTURES.consent!, window: "mini" },
};
