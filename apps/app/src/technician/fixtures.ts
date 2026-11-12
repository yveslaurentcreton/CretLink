// Snapshot fixtures of every technician screen and state (ST-M009-008), used
// by the rendering tests and the dev-only gallery (`dev/gallery.html`).

import type {
  ActivityEntry,
  ActivityItem,
  AiClientStatus,
  AppInfo,
  AuditEntryView,
  ListenNetwork,
  SessionSummary,
} from "../shared/contract";
import type { ConsoleState, SessionPhase, TargetIdentity } from "./api";
import { type TechnicianSnapshot, initialSnapshot } from "./model";

/** 2026-11-03 14:30:00 UTC: the fixtures' "now". */
export const NOW = Date.UTC(2026, 10, 3, 14, 30, 0);
const minutes = (n: number): number => n * 60_000;
const iso = (ms: number): string => new Date(ms).toISOString();

export const TARGET: TargetIdentity = {
  hostname: "ACME-PC-07",
  os_version: "Windows 11 Pro 24H2",
  arch: "x64",
  elevated: true,
  simulated: false,
  agent_version: "0.22.0",
};

export const SESSION_ID = "7f3a9c51d2e84b6a";

export function consoleIn(
  phase: SessionPhase,
  options: { session?: boolean; target?: boolean; simulated?: boolean; displayName?: string } = {},
): ConsoleState {
  const withTarget = options.target ?? false;
  const identity: TargetIdentity = options.simulated
    ? {
        ...TARGET,
        hostname: "SIMULATED-TARGET",
        os_version: "Windows 11 (simulated)",
        simulated: true,
      }
    : TARGET;
  return {
    technician: {
      label: "This computer (technician)",
      hostname: "TECH-LAPTOP",
      os: "Windows 11 Pro 25H2",
      ...(options.displayName ? { display_name: options.displayName } : {}),
    },
    targets: withTarget
      ? [
          {
            id: "t1",
            display_name: identity.hostname,
            address: "10.0.0.57:51234",
            source: options.simulated ? "simulated" : "discovered",
            stale: false,
            identity,
          },
        ]
      : [],
    ...(withTarget ? { selected: "t1" } : {}),
    ...(options.session ? { session: SESSION_ID } : {}),
    phase,
  };
}

/** This computer on its local network, a virtual adapter and a VPN (ST-M009-016). */
export const NETWORKS: ListenNetwork[] = [
  { address: "10.0.0.42", adapter: "Wi-Fi", pointToPoint: false },
  { address: "172.20.0.1", adapter: "vEthernet (Default Switch)", pointToPoint: false },
  { address: "192.168.1.20", adapter: "VPN", pointToPoint: true },
];

export const APP_INSTALLED: AppInfo = {
  hostname: "TECH-LAPTOP",
  os: "Windows 11 Pro 25H2",
  version: "0.22.0",
  install: "installed",
  location: "C:\\Users\\sam\\AppData\\Local\\Programs\\CretLink",
  auditDir: "C:\\Users\\sam\\AppData\\Local\\CretLink\\audit",
};

export const APP_RUN_ONCE: AppInfo = {
  ...APP_INSTALLED,
  install: "run_once",
  location: "C:\\Users\\sam\\Downloads",
};

export const CLIENTS: AiClientStatus[] = [
  {
    id: "claude",
    label: "Claude Code",
    present: true,
    configPath: "~/.claude.json",
    registration: {
      state: "this_executable",
      command: "C:\\Users\\sam\\AppData\\Local\\Programs\\CretLink\\cretlink.exe mcp",
    },
    installPlan: [
      "claude mcp add --scope user cretlink -- C:\\Users\\sam\\AppData\\Local\\Programs\\CretLink\\cretlink.exe mcp",
    ],
    uninstallPlan: ["claude mcp remove --scope user cretlink"],
  },
  {
    id: "codex",
    label: "Codex CLI",
    present: true,
    configPath: "~/.codex/config.toml",
    registration: { state: "other_command", command: "D:\\old\\cretlink-cli.exe mcp" },
    installPlan: [
      'write [mcp_servers.cretlink] to ~/.codex/config.toml: command = "C:\\Users\\sam\\AppData\\Local\\Programs\\CretLink\\cretlink.exe", args = ["mcp"]',
    ],
    uninstallPlan: ["remove [mcp_servers.cretlink] from ~/.codex/config.toml"],
  },
  {
    id: "gemini",
    label: "Gemini CLI",
    present: true,
    configPath: "~/.gemini/settings.json",
    registration: { state: "absent" },
    installPlan: [
      "gemini mcp add --scope user cretlink C:\\Users\\sam\\AppData\\Local\\Programs\\CretLink\\cretlink.exe mcp",
    ],
    uninstallPlan: [],
  },
];

export const CLIENTS_UNREADABLE: AiClientStatus[] = [
  CLIENTS[0] as AiClientStatus,
  {
    id: "codex",
    label: "Codex CLI",
    present: true,
    configPath: "~/.codex/config.toml",
    registration: { state: "unreadable", message: "expected `=` at line 12" },
    installPlan: [],
    uninstallPlan: [],
  },
  {
    id: "gemini",
    label: "Gemini CLI",
    present: false,
    registration: { state: "absent" },
    installPlan: [],
    uninstallPlan: [],
  },
];

function summary(partial: Partial<SessionSummary> & Pick<SessionSummary, "id">): SessionSummary {
  return {
    file: `C:\\Users\\sam\\AppData\\Local\\CretLink\\audit\\${partial.id}.jsonl`,
    integrity: "verified",
    hostname: "ACME-PC-07",
    os: "Windows 11 Pro 24H2",
    startedAt: iso(NOW - minutes(60 * 24 + 50)),
    endedAt: iso(NOW - minutes(60 * 24 + 8)),
    durationMs: minutes(42),
    endedBy: "customer",
    actions: { ai: 13, terminal: 2, technician: 3 },
    filesSent: 1,
    filesReceived: 1,
    failures: 0,
    cleanup: {
      state: "cleaned_up",
      residuals: 0,
      notes: [
        { ok: true, text: "Session keys and channel removed" },
        { ok: true, text: "Running jobs stopped (2)" },
        { ok: true, text: "Staging folder removed (2 files)" },
      ],
    },
    ...partial,
  };
}

export const SUMMARY_CLEAN = summary({ id: "a1b2c3d4e5f6" });

export const SUMMARY_RESIDUAL = summary({
  id: "b2c3d4e5f6a1",
  hostname: "FRONTDESK-2",
  startedAt: iso(NOW - minutes(60 * 24 * 2 + 30)),
  endedAt: iso(NOW - minutes(60 * 24 * 2 + 18)),
  durationMs: minutes(12),
  endedBy: "technician",
  actions: { ai: 3, terminal: 1, technician: 0 },
  failures: 1,
  firstFailure: "netsh wlan show drivers · exit 1",
  cleanup: {
    state: "residuals",
    residuals: 1,
    notes: [
      { ok: true, text: "Session keys and channel removed" },
      {
        ok: false,
        text: "Intel Wi-Fi driver 23.60 stays installed. It was installed on purpose during the session.",
      },
    ],
  },
});

export const SUMMARY_FAILED = summary({
  id: "c3d4e5f6a1b2",
  hostname: "MOM-LAPTOP",
  startedAt: iso(NOW - minutes(60 * 24 * 14)),
  endedAt: iso(NOW - minutes(60 * 24 * 14 - 25)),
  durationMs: minutes(25),
  endedBy: "connection_lost",
  cleanup: {
    state: "failed",
    residuals: 0,
    notes: [
      { ok: true, text: "Session keys and channel removed" },
      { ok: false, text: "The staging folder could not be removed: access denied" },
    ],
  },
});

export const SUMMARY_UNKNOWN = summary({
  id: "d4e5f6a1b2c3",
  hostname: "RECEPTION",
  endedBy: "connection_lost",
  integrity: "unsealed",
  cleanup: { state: "unknown", residuals: 0, notes: [] },
});

export const SUMMARY_BROKEN = summary({
  id: "e5f6a1b2c3d4",
  hostname: undefined,
  integrity: "broken",
  endedBy: undefined,
  durationMs: undefined,
  cleanup: { state: "unknown", residuals: 0, notes: [] },
});

export const SESSIONS: SessionSummary[] = [
  SUMMARY_CLEAN,
  SUMMARY_RESIDUAL,
  SUMMARY_FAILED,
  SUMMARY_UNKNOWN,
  SUMMARY_BROKEN,
];

function item(partial: Partial<ActivityItem> & Pick<ActivityItem, "id">): ActivityEntry {
  return {
    entry: "item",
    item: {
      source: "ai",
      clientLabel: "Claude Code",
      description: "Ran a command",
      kind: "command",
      operation: "Get-Service",
      state: { state: "done", exitCode: 0 },
      startedAt: iso(NOW - minutes(5)),
      durationMs: 1200,
      ...partial,
    },
  };
}

/** A live feed, oldest first: done, failed, cancelled, timed out, a gap, files, running. */
export const FEED: ActivityEntry[] = [
  item({
    id: "i1",
    source: "terminal",
    clientLabel: undefined,
    description: "Ran a command in the terminal",
    operation: "pnputil /enum-drivers /class Net",
    startedAt: iso(NOW - minutes(20)),
  }),
  item({
    id: "i2",
    purpose: "Checked the Wi-Fi driver",
    operation: "netsh wlan show drivers",
    state: { state: "failed", exitCode: 1 },
    startedAt: iso(NOW - minutes(18)),
  }),
  item({
    id: "i3",
    source: "technician",
    clientLabel: undefined,
    description: "Cancelled a job",
    kind: "job_control",
    operation: "cancel job 4",
    state: { state: "cancelled" },
    startedAt: iso(NOW - minutes(15)),
  }),
  { entry: "gap", count: 3, at: iso(NOW - minutes(14)) },
  item({
    id: "i4",
    clientLabel: "Codex CLI",
    purpose: "Waited for Windows Update",
    operation: "Get-WindowsUpdateLog",
    state: { state: "timed_out" },
    startedAt: iso(NOW - minutes(12)),
  }),
  item({
    id: "i5",
    purpose: "Copied the driver setup log to look for errors",
    description: "Sent a file",
    kind: "file_sent",
    operation: "download C:\\Windows\\INF\\setupapi.dev.log",
    path: "C:\\Windows\\INF\\setupapi.dev.log",
    bytes: 1_258_291,
    startedAt: iso(NOW - minutes(8)),
  }),
  item({
    id: "i6",
    source: "technician",
    clientLabel: undefined,
    description: "Received a file",
    kind: "file_received",
    operation: "upload Intel-WiFi-23.60.exe",
    path: "C:\\Users\\jdoe\\AppData\\Local\\Temp\\CretLink\\Intel-WiFi-23.60.exe",
    bytes: 38_100_000,
    startedAt: iso(NOW - minutes(6)),
  }),
  item({
    id: "i7",
    purpose: "Reading the Windows system log",
    operation: "Get-WinEvent -LogName System -MaxEvents 50",
    state: { state: "running" },
    startedAt: iso(NOW - 12_000),
  }),
];

export const XSS = '<img src=x onerror="alert(1)"><script>alert(2)</script>';

export const FEED_XSS: ActivityEntry[] = [
  item({ id: "x1", purpose: XSS, operation: XSS, path: XSS, description: XSS }),
];

// This session's audit log as `get_session_audit` returns it, oldest first.
export const AUDIT: AuditEntryView[] = [
  { seq: 0, at: iso(NOW - minutes(30)), actor: "system", action: "session_opened", outcome: "ok", summary: "" },
  { seq: 1, at: iso(NOW - minutes(29)), actor: "technician", action: "pairing_started", outcome: "ok", summary: "code: [REDACTED]" },
  { seq: 2, at: iso(NOW - minutes(28)), actor: "agent", action: "consent_granted", outcome: "ok", summary: "" },
  { seq: 3, at: iso(NOW - minutes(28)), actor: "system", action: "target_identity", outcome: "ok", summary: "hostname: ACME-PC-07 · os: Windows 11 Pro 23H2" },
  { seq: 4, at: iso(NOW - minutes(20)), actor: "ai", action: "mcp_remote_exec", outcome: "failed", summary: "netsh wlan show drivers" },
  { seq: 5, at: iso(NOW - minutes(2)), actor: "ai", action: "mcp_remote_exec_start", outcome: "ok", summary: "Get-WinEvent -LogName System -MaxEvents 50" },
];

export const AUDIT_XSS: AuditEntryView[] = [
  { seq: 0, at: iso(NOW), actor: "ai", action: XSS, outcome: "unknown", summary: XSS },
];

function base(console: ConsoleState): TechnicianSnapshot {
  return {
    ...initialSnapshot(console, NOW),
    app: APP_INSTALLED,
    version: "0.22.0",
    clients: CLIENTS,
    sessions: SESSIONS.slice(0, 3),
    networks: NETWORKS,
  };
}

const idle = consoleIn({ phase: "idle" });
const last = {
  id: SESSION_ID,
  target: TARGET,
  since: NOW - minutes(42),
  endedAt: NOW,
};

/** Every screen and state, by name. */
export const FIXTURES: Record<string, TechnicianSnapshot> = {
  home: base(idle),
  "home-empty": { ...base(idle), sessions: [], clients: CLIENTS_UNREADABLE },
  "home-unavailable": { ...initialSnapshot(idle, NOW), version: "0.22.0" },
  sessions: { ...base(idle), nav: "sessions", sessions: SESSIONS },
  "report-residuals": {
    ...base(idle),
    nav: "sessions",
    sessions: SESSIONS,
    openReport: SUMMARY_RESIDUAL.id,
  },
  "report-failed": {
    ...base(idle),
    nav: "sessions",
    sessions: SESSIONS,
    openReport: SUMMARY_FAILED.id,
  },
  "report-unknown": {
    ...base(idle),
    nav: "sessions",
    sessions: SESSIONS,
    openReport: SUMMARY_UNKNOWN.id,
  },
  "report-broken": {
    ...base(idle),
    nav: "sessions",
    sessions: SESSIONS,
    openReport: SUMMARY_BROKEN.id,
  },
  wait: base(
    consoleIn(
      { phase: "listening", code: "6RRR-2H8F", address: "10.0.0.42:7000" },
      { displayName: "Sam" },
    ),
  ),
  "wait-no-name": base(
    consoleIn({ phase: "listening", code: "6RRR-2H8F", address: "10.0.0.42:7000" }),
  ),
  // The other networks of this computer, listed (ST-M009-016).
  "wait-networks": {
    ...base(
      consoleIn(
        { phase: "listening", code: "6RRR-2H8F", address: "10.0.0.42:7000" },
        { displayName: "Sam" },
      ),
    ),
    networksOpen: true,
  },
  // One network only: nothing to choose.
  "wait-one-network": {
    ...base(consoleIn({ phase: "listening", code: "6RRR-2H8F", address: "10.0.0.42:7000" })),
    networks: NETWORKS.slice(0, 1),
  },
  // The backend has not listed the networks: the address stands alone.
  "wait-networks-unknown": {
    ...base(consoleIn({ phase: "listening", code: "6RRR-2H8F", address: "10.0.0.42:7000" })),
    networks: null,
  },
  // Between the old listener and the new one.
  "wait-switching": {
    ...base(consoleIn({ phase: "stopped", reason: "technician_requested" })),
    busy: "network",
  },
  connecting: base(consoleIn({ phase: "connecting" }, { session: true })),
  compare: base(
    consoleIn({ phase: "awaiting_approval_code", code: "482913" }, { session: true, target: true }),
  ),
  consent: base(consoleIn({ phase: "awaiting_consent" }, { session: true, target: true })),
  "consent-simulated": base(
    consoleIn({ phase: "awaiting_consent" }, { session: true, target: true, simulated: true }),
  ),
  live: {
    ...base(
      consoleIn({ phase: "full_control" }, { session: true, target: true, displayName: "Sam" }),
    ),
    activity: FEED,
    last: { id: SESSION_ID, target: TARGET, since: NOW - minutes(28) },
  },
  "live-bridge": {
    ...base(
      consoleIn({ phase: "full_control" }, { session: true, target: true, displayName: "Sam" }),
    ),
    activity: FEED,
    bridge: { endpoint: "ready", tools: 24, clients: ["Claude Code", "Gemini CLI"] },
    last: { id: SESSION_ID, target: TARGET, since: NOW - minutes(28) },
  },
  "live-bridge-unavailable": {
    ...base(consoleIn({ phase: "full_control" }, { session: true, target: true })),
    bridge: {
      endpoint: "unavailable",
      message:
        "AI assistants cannot reach this session: the local connection for them could not start (address in use). Another CretLink console on this computer may hold it; close it, then stop and start the session again. The terminal still works.",
      tools: 24,
      clients: [],
    },
    last: { id: SESSION_ID, target: TARGET, since: NOW - minutes(1) },
  },
  "live-empty": {
    ...base(consoleIn({ phase: "full_control" }, { session: true, target: true })),
    last: { id: SESSION_ID, target: TARGET, since: NOW - minutes(1) },
  },
  "live-terminal": {
    ...base(
      consoleIn({ phase: "full_control" }, { session: true, target: true, displayName: "Sam" }),
    ),
    activity: FEED,
    tab: "terminal",
    last: { id: SESSION_ID, target: TARGET, since: NOW - minutes(28) },
  },
  "live-files": {
    ...base(consoleIn({ phase: "full_control" }, { session: true, target: true })),
    activity: FEED,
    tab: "files",
    last: { id: SESSION_ID, target: TARGET, since: NOW - minutes(28) },
  },
  "live-audit": {
    ...base(consoleIn({ phase: "full_control" }, { session: true, target: true })),
    activity: FEED,
    audit: AUDIT,
    tab: "audit",
    last: { id: SESSION_ID, target: TARGET, since: NOW - minutes(28) },
  },
  "live-simulated": {
    ...base(consoleIn({ phase: "full_control" }, { session: true, target: true, simulated: true })),
    last: { id: SESSION_ID, since: NOW - minutes(2) },
  },
  "live-xss": {
    ...base(consoleIn({ phase: "full_control" }, { session: true, target: true })),
    activity: FEED_XSS,
    audit: AUDIT_XSS,
    last: { id: SESSION_ID, target: TARGET, since: NOW - minutes(2) },
  },
  stopping: base(consoleIn({ phase: "stopping" }, { session: true, target: true })),
  "end-by-technician": {
    ...base(consoleIn({ phase: "stopped", reason: "technician_requested" })),
    sessions: [
      { ...SUMMARY_CLEAN, id: SESSION_ID, endedBy: "technician", endedAt: iso(NOW) },
      ...SESSIONS,
    ],
    activity: FEED,
    last: { ...last, reason: "technician_requested" },
  },
  "end-by-customer": {
    ...base(consoleIn({ phase: "stopped", reason: "customer_requested" })),
    sessions: [
      {
        ...SUMMARY_RESIDUAL,
        id: SESSION_ID,
        hostname: "ACME-PC-07",
        endedBy: "customer",
        endedAt: iso(NOW),
      },
      ...SESSIONS,
    ],
    activity: FEED,
    last: { ...last, reason: "customer_requested" },
  },
  "end-disconnected": {
    ...base(consoleIn({ phase: "stopped", reason: "disconnected" })),
    sessions: [
      { ...SUMMARY_UNKNOWN, id: SESSION_ID, hostname: "ACME-PC-07", endedAt: iso(NOW) },
      ...SESSIONS,
    ],
    activity: FEED,
    last: { ...last, reason: "disconnected" },
  },
  "end-preparing": {
    ...base(consoleIn({ phase: "stopped", reason: "customer_requested" })),
    activity: FEED,
    reportPending: true,
    last: { ...last, reason: "customer_requested" },
  },
  "end-unavailable": {
    ...base(consoleIn({ phase: "stopped", reason: "disconnected" })),
    activity: FEED,
    reportPending: false,
    last: { ...last, reason: "disconnected" },
  },
  "end-failed": {
    ...base(
      consoleIn({
        phase: "failed",
        stage: "transport",
        message: "The connection to the customer computer broke.",
      }),
    ),
    sessions: [
      {
        ...SUMMARY_FAILED,
        id: SESSION_ID,
        hostname: "ACME-PC-07",
        endedBy: "failure",
        endedAt: iso(NOW),
      },
      ...SESSIONS,
    ],
    activity: FEED,
    last: {
      ...last,
      failure: { stage: "transport", message: "The connection to the customer computer broke." },
    },
  },
  "failed-no-session": base(
    consoleIn({ phase: "failed", stage: "listen", message: "The port 7000 is already in use." }),
  ),
  ai: { ...base(idle), nav: "ai" },
  "ai-confirm-connect": {
    ...base(idle),
    nav: "ai",
    confirm: { client: "gemini", action: "install", replace: false },
  },
  "ai-confirm-replace": {
    ...base(idle),
    nav: "ai",
    confirm: { client: "codex", action: "install", replace: true },
  },
  "ai-confirm-disconnect": {
    ...base(idle),
    nav: "ai",
    confirm: { client: "claude", action: "uninstall", replace: false },
  },
  "ai-unreadable": { ...base(idle), nav: "ai", clients: CLIENTS_UNREADABLE },
  "ai-run-once": {
    ...base(idle),
    nav: "ai",
    app: APP_RUN_ONCE,
    confirm: { client: "gemini", action: "install", replace: false },
  },
  "ai-unavailable": { ...base(idle), nav: "ai", clients: null },
  settings: { ...base(idle), nav: "settings" },
  "settings-uninstall": {
    ...base(idle),
    nav: "settings",
    uninstall: {
      plan: [
        "C:\\Users\\sam\\AppData\\Local\\Programs\\CretLink",
        "Start menu shortcut CretLink",
        "Uninstall entry CretLink",
        "Claude Code registration cretlink",
      ],
    },
  },
  "settings-run-once": { ...base(idle), nav: "settings", app: APP_RUN_ONCE },
  "settings-unavailable": { ...base(idle), nav: "settings", app: null },
  error: {
    ...base(idle),
    error: {
      title: "The session could not start",
      message: "The address is already in use (listen)",
      next: "Check that this computer is connected to the network, then try again.",
    },
  },
};
