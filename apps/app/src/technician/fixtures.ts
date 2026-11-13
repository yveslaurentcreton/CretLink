// Snapshot fixtures of every technician screen and state (ST-M009-008), used
// by the rendering tests and the dev-only gallery (`dev/gallery.html`).

import type {
  ActivityEntry,
  ActivityItem,
  AiClientStatus,
  AppInfo,
  AuditEntryView,
  ExplorerDrive,
  ExplorerEntry,
  ListenNetwork,
  RelaySettings,
  RelayView,
  SessionSummary,
  TransferEntry,
} from "../shared/contract";
import type { ConsoleState, SessionPhase, TargetIdentity } from "./api";
import {
  DRIVES,
  type ExplorerState,
  ROW_HEIGHT,
  applyDrives,
  applyEvent,
  applyFailure,
  applyListing,
  applyTransfers,
  folder,
  initialExplorer,
  opening,
  removalOf,
  selectOnly,
  toggleSelected,
  withSort,
  withView,
  withViewport,
} from "./explorer";
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

// --- The relay of this copy (ST-M007-011) ---

/** The relay Sam hosts. Its key is never part of a view. */
export const RELAY: RelayView = {
  host: "relay.example.org",
  address: "relay.example.org:443",
  keySet: true,
};

/** A relay with a certificate from an authority of the technician's own. */
export const RELAY_PRIVATE: RelayView = {
  host: "host-01.example.org",
  address: "192.0.2.1:9970",
  keySet: false,
  ca: "relay-ca.pem",
  name: "host-01.example.org",
};

const SETTINGS_PATH = "C:\\Users\\sam\\AppData\\Local\\Programs\\CretLink\\settings.toml";
const PROFILE_PATH = "C:\\Users\\sam\\AppData\\Local\\Programs\\CretLink\\cretlink.toml";
const PORTABLE_PROFILE_PATH = "C:\\Users\\sam\\Downloads\\cretlink.toml";

/** The installed copy without a relay. */
export const RELAY_NONE: RelaySettings = {
  editable: true,
  source: "none",
  file: { path: PROFILE_PATH },
  stored: { path: SETTINGS_PATH },
};

/** The installed copy with a relay in its settings. */
export const RELAY_STORED: RelaySettings = {
  ...RELAY_NONE,
  source: "settings",
  stored: { path: SETTINGS_PATH, relay: RELAY },
};

/** A file next to the program, which wins over the relay of the settings. */
export const RELAY_FILE: RelaySettings = {
  editable: true,
  source: "file",
  file: { path: PROFILE_PATH, relay: RELAY_PRIVATE },
  stored: { path: SETTINGS_PATH, relay: RELAY },
};

/** A file next to the program that cannot be used. */
export const RELAY_FILE_PROBLEM: RelaySettings = {
  editable: true,
  source: "file",
  file: {
    path: PROFILE_PATH,
    problem: {
      file: PROFILE_PATH,
      field: "address",
      message:
        "Line 2: `address` needs a port; write the relay's host name and port, like relay.example.org:443.",
    },
  },
  stored: { path: SETTINGS_PATH },
};

/** A copy that runs once: no settings, and no file next to it. */
export const RELAY_RUN_ONCE: RelaySettings = {
  editable: false,
  source: "none",
  file: { path: PORTABLE_PROFILE_PATH },
};

/** A copy that runs once with a profile file next to it. */
export const RELAY_RUN_ONCE_FILE: RelaySettings = {
  editable: false,
  source: "file",
  file: { path: PORTABLE_PROFILE_PATH, relay: RELAY },
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

/** A folder name that holds markup: a name is whatever the customer computer has. */
export const XSS_NAME = "<b>bold</b>&amp;";

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

// --- The file explorer (ST-M010-002) ---

/** The drives of the customer computer: a disk, a drive without a disc, a stick, a share. */
export const EXPLORER_DRIVES: ExplorerDrive[] = [
  {
    root: "C:\\",
    kind: "fixed",
    label: "Windows",
    totalBytes: 511_000_000_000,
    freeBytes: 128_400_000_000,
  },
  { root: "D:\\", kind: "optical" },
  { root: "E:\\", kind: "removable", label: "TOOLS", totalBytes: 31_900_000_000, freeBytes: 9_200_000_000 },
  { root: "Z:\\", kind: "network" },
];

export const EXPLORER_FOLDER = "C:\\Users\\ann\\Documents";

function entry(
  name: string,
  kind: ExplorerEntry["kind"],
  len: number,
  ago: number,
  readonly = false,
): ExplorerEntry {
  return { name, kind, len, modifiedMs: NOW - minutes(ago), readonly };
}

/** A customer's Documents folder, in the order the customer computer lists it. */
export const EXPLORER_ENTRIES: ExplorerEntry[] = [
  entry("Budget 2026.xlsx", "file", 48_213, 60 * 24 * 3),
  entry("Invoices", "dir", 0, 60 * 5),
  entry("Old laptop", "symlink", 0, 60 * 24 * 400),
  entry("Photos", "dir", 0, 60 * 24 * 30),
  entry("Scans", "dir", 0, 60 * 24 * 2),
  entry("desktop.ini", "file", 402, 60 * 24 * 700, true),
  entry("letter to the bank.docx", "file", 18_944, 90),
  entry("notes.txt", "file", 1_204, 12),
  entry("report.pdf", "file", 1_258_291, 60 * 24),
  entry("setup-log-10.txt", "file", 88_100, 60 * 26),
  entry("setup-log-2.txt", "file", 91_300, 60 * 50),
];

/** A folder of `count` files, as the pages of a listing bring them. */
export function explorerFiles(count: number): ExplorerEntry[] {
  return Array.from({ length: count }, (_, n) =>
    entry(`driver-${String(n).padStart(5, "0")}.dll`, "file", 4096 + ((n * 7919) % 900_000), n),
  );
}

/** The explorer standing at the drives. */
export function explorerAtDrives(): ExplorerState {
  return applyDrives(
    opening(initialExplorer(), { listing: 1, place: DRIVES, how: "push" }),
    1,
    EXPLORER_DRIVES,
  );
}

/** The explorer standing in `path`, opened from the drives with these entries. */
export function explorerIn(
  path: string,
  entries: ExplorerEntry[],
  end = true,
  from: ExplorerState = explorerAtDrives(),
): ExplorerState {
  const listing = (from.listing ?? 0) + 1;
  return applyEvent(opening(from, { listing, place: folder(path), how: "push" }), {
    kind: "opened",
    listing,
    path,
    entries,
    end,
  });
}

const documents = explorerIn(EXPLORER_FOLDER, EXPLORER_ENTRIES);

/** Where a copy of this customer computer goes on the technician computer. */
export const DOWNLOADS = "C:\\Users\\sam\\Downloads\\CretLink\\ACME-PC-07";

const SESSION_ENDED = "The session ended before this was complete.";

/** One entry of the transfer queue; a file sent to the documents folder unless said otherwise. */
export function transfer(patch: Partial<TransferEntry> & { id: number }): TransferEntry {
  const name = patch.name ?? `file-${patch.id}.txt`;
  const direction = patch.direction ?? "send";
  return {
    direction,
    kind: "file",
    name,
    remote: `${EXPLORER_FOLDER}\\${name}`,
    local:
      direction === "send"
        ? `C:\\Users\\sam\\Desktop\\${name}`
        : `${DOWNLOADS}\\${name}`,
    done: 0,
    state: "waiting",
    ...patch,
  };
}

/** A queue in every state: what did not arrive, what runs and waits, what ended. */
export const TRANSFERS: TransferEntry[] = [
  transfer({ id: 1, name: "setup.exe", bytes: 52_428_800, done: 52_428_800, state: "done" }),
  transfer({
    id: 2,
    direction: "download",
    name: "report.pdf",
    bytes: 1_258_291,
    done: 1_258_291,
    state: "done",
  }),
  transfer({
    id: 3,
    name: "Budget 2026.xlsx",
    bytes: 48_213,
    state: "failed",
    reason: "The file is in use.",
  }),
  transfer({
    id: 4,
    direction: "download",
    name: "Photos\\Old laptop",
    state: "left_out",
    reason: "A link is not followed.",
  }),
  transfer({ id: 5, name: "drivers\\chipset.zip", bytes: 9000, done: 3000, state: "cancelled" }),
  transfer({
    id: 6,
    direction: "download",
    name: "Photos\\2026\\holiday.jpg",
    bytes: 4_000_000,
    done: 1_800_000,
    state: "running",
  }),
  transfer({ id: 7, direction: "download", name: "Photos\\2026\\garden.jpg", bytes: 2_500_000 }),
  transfer({ id: 8, direction: "download", name: "Photos\\2026\\cat.jpg", bytes: 1_100_000 }),
  transfer({ id: 9, name: "Tools", kind: "folder", state: "done" }),
];

/** A queue of `count` transfers: a folder of that many files, the first ones done. */
export function manyTransfers(count: number): TransferEntry[] {
  return Array.from({ length: count }, (_, n) =>
    transfer({
      id: n + 1,
      name: `Drivers\\driver-${String(n).padStart(5, "0")}.dll`,
      bytes: 4096,
      done: n < 200 ? 4096 : 0,
      state: n < 200 ? "done" : n === 200 ? "running" : n % 97 === 0 ? "failed" : "waiting",
      ...(n > 200 && n % 97 === 0 ? { reason: "Access was denied." } : {}),
    }),
  );
}

/** Every state of the explorer, by name. */
export const EXPLORERS: Record<string, ExplorerState> = {
  // The tab was never opened: nothing was asked of the customer computer.
  "files-closed": initialExplorer(),
  "files-opening-first": opening(initialExplorer(), { listing: 1, place: DRIVES, how: "push" }),
  "files-drives": explorerAtDrives(),
  "files-details": toggleSelected(selectOnly(documents, "Invoices"), "report.pdf"),
  "files-list": withView(selectOnly(documents, "notes.txt"), "list"),
  "files-sorted": withSort(documents, "size"),
  "files-empty": explorerIn("E:\\Backup", []),
  "files-loading": explorerIn("C:\\Windows\\System32", [], false),
  "files-loading-more": explorerIn("C:\\Windows\\System32", explorerFiles(256), false),
  "files-large": withViewport(explorerIn("C:\\Windows\\System32", explorerFiles(5000)), {
    top: 2500 * ROW_HEIGHT,
  }),
  "files-large-list": withView(explorerIn("C:\\Windows\\System32", explorerFiles(5000)), "list"),
  "files-opening": opening(documents, {
    listing: 9,
    place: folder("C:\\Users\\ann\\Documents\\Invoices"),
    how: "push",
  }),
  "files-refused": applyFailure(
    opening(documents, { listing: 9, place: folder("C:\\Users\\jdoe"), how: "push" }),
    9,
    { stage: "access_denied", message: "access was denied" },
  ),
  "files-refused-first": applyFailure(
    opening(initialExplorer(), { listing: 1, place: DRIVES, how: "push" }),
    1,
    { stage: "unsupported", message: "not supported on this computer" },
  ),
  "files-incomplete": applyFailure(
    explorerIn("C:\\Windows\\System32", explorerFiles(256), false),
    2,
    { stage: "access_denied", message: "access was denied" },
  ),
  "files-limit": applyListing(explorerIn("C:\\Windows\\System32", explorerFiles(300), false), {
    listing: 2,
    path: "C:\\Windows\\System32",
    entries: 300,
    state: "limit",
  }),
  "files-ended": applyFailure(
    explorerIn("C:\\Windows\\System32", explorerFiles(256), false),
    2,
    { stage: "no_consented_session", message: "no session is under full control" },
  ),
  "files-refreshing": applyEvent(
    opening(selectOnly(documents, "notes.txt"), {
      listing: 9,
      place: folder(EXPLORER_FOLDER),
      how: "replace",
    }),
    { kind: "opened", listing: 9, path: EXPLORER_FOLDER, entries: EXPLORER_ENTRIES.slice(0, 4), end: false },
  ),
  "files-menu": { ...selectOnly(documents, "Invoices"), menu: { x: 320, y: 260, keys: ["Invoices"] } },
  "files-menu-file": {
    ...selectOnly(documents, "report.pdf"),
    menu: { x: 320, y: 300, keys: ["report.pdf"] },
  },
  "files-menu-place": { ...documents, menu: { x: 400, y: 380, keys: [] } },
  "files-path": { ...documents, editing: EXPLORER_FOLDER },
  "files-xss": explorerIn(`C:\\${XSS_NAME}`, [
    entry(XSS, "file", 12, 5),
    entry(`${XSS}.dir`, "dir", 0, 5),
  ]),
  // The second part: what the explorer sends, copies, renames and deletes.
  "files-queue": applyTransfers(selectOnly(documents, "report.pdf"), TRANSFERS),
  "files-queue-many": applyTransfers(documents, manyTransfers(5000)),
  "files-queue-xss": applyTransfers(documents, [
    transfer({ id: 1, name: XSS, remote: XSS, local: XSS, state: "failed", reason: XSS }),
  ]),
  "files-conflict": {
    ...documents,
    question: {
      kind: "conflicts",
      direction: "send",
      folder: EXPLORER_FOLDER,
      where: EXPLORER_FOLDER,
      offer: 3,
      conflicts: [
        { name: "report.pdf", kind: "file", existing: "file", replace: true },
        { name: "notes.txt", kind: "file", existing: "file", replace: true },
        { name: "Photos", kind: "folder", existing: "dir", replace: true },
      ],
      at: 0,
      choices: {},
      all: false,
    },
  },
  "files-conflict-folder": {
    ...documents,
    question: {
      kind: "conflicts",
      direction: "send",
      folder: EXPLORER_FOLDER,
      where: EXPLORER_FOLDER,
      offer: 3,
      conflicts: [
        { name: "report.pdf", kind: "file", existing: "file", replace: true },
        { name: "Photos", kind: "folder", existing: "dir", replace: true },
      ],
      at: 1,
      choices: { "report.pdf": "keep_both" },
      all: true,
    },
  },
  "files-conflict-kinds": {
    ...documents,
    question: {
      kind: "conflicts",
      direction: "send",
      folder: EXPLORER_FOLDER,
      where: EXPLORER_FOLDER,
      offer: 4,
      conflicts: [{ name: "Invoices", kind: "file", existing: "dir", replace: false }],
      at: 0,
      choices: {},
      all: false,
    },
  },
  "files-conflict-download": {
    ...selectOnly(documents, "report.pdf"),
    question: {
      kind: "conflicts",
      direction: "download",
      folder: EXPLORER_FOLDER,
      where: DOWNLOADS,
      names: [{ name: "report.pdf", kind: "file" }],
      conflicts: [{ name: "report.pdf", kind: "file", existing: "file", replace: true }],
      at: 0,
      choices: {},
      all: false,
    },
  },
  "files-delete": {
    ...toggleSelected(toggleSelected(selectOnly(documents, "Invoices"), "notes.txt"), "report.pdf"),
    question: {
      kind: "delete",
      folder: EXPLORER_FOLDER,
      items: [
        { name: "Invoices", kind: "dir" },
        { name: "notes.txt", kind: "file" },
        { name: "report.pdf", kind: "file" },
      ],
    },
  },
  "files-delete-one": {
    ...selectOnly(documents, "notes.txt"),
    question: {
      kind: "delete",
      folder: EXPLORER_FOLDER,
      items: [{ name: "notes.txt", kind: "file" }],
    },
  },
  "files-removal": {
    ...selectOnly(documents, "Photos"),
    removal: { ...removalOf(2, 3), item: "Photos", at: 2, removedFiles: 1204, removedDirs: 31 },
  },
  "files-removal-cancelling": {
    ...documents,
    removal: { ...removalOf(2, 3), cancelling: true },
  },
  "files-renaming": { ...selectOnly(documents, "report.pdf"), renaming: "report.pdf" },
  "files-problem": {
    ...documents,
    problem: {
      title: "2 of 3 items could not be deleted",
      message: "1 of 3 items was deleted (40 files and 3 folders in all).",
      next: "The list shows what is there now. Close what holds a file open, then delete again.",
      details: [
        { path: `${EXPLORER_FOLDER}\\Scans\\open.xlsx`, reason: "the file is in use" },
        { path: `${EXPLORER_FOLDER}\\Scans`, reason: "the folder is not empty" },
      ],
      more: 4,
    },
  },
  "files-problem-xss": {
    ...documents,
    problem: { title: XSS, message: XSS, next: XSS, details: [{ path: XSS, reason: XSS }] },
  },
  "files-dropping": { ...documents, dropping: true },
  "files-dropping-drives": { ...explorerAtDrives(), dropping: true },
  "files-upload-menu": { ...documents, menu: { x: 12, y: 96, keys: [], only: "upload" } },
  "files-preparing": {
    ...documents,
    preparing: `looking at ${EXPLORER_FOLDER} for names that are taken…`,
  },
  "files-ended-queue": {
    ...applyTransfers(documents, [
      transfer({ id: 1, name: "setup.exe", state: "stopped", reason: SESSION_ENDED }),
      transfer({ id: 2, name: "driver.zip", state: "stopped", reason: SESSION_ENDED }),
      transfer({ id: 3, name: "notes.txt", state: "done", bytes: 1204, done: 1204 }),
    ]),
    ended: "The session has ended. Browsing has stopped.",
  },
};

function base(console: ConsoleState): TechnicianSnapshot {
  return {
    ...initialSnapshot(console, NOW),
    app: APP_INSTALLED,
    version: "0.22.0",
    clients: CLIENTS,
    sessions: SESSIONS.slice(0, 3),
    networks: NETWORKS,
    relay: RELAY_NONE,
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
  "settings-run-once": {
    ...base(idle),
    nav: "settings",
    app: APP_RUN_ONCE,
    relay: RELAY_RUN_ONCE,
  },
  "settings-unavailable": { ...base(idle), nav: "settings", app: null, relay: null },
  // Settings → Relay (ST-M007-011).
  "settings-relay": { ...base(idle), nav: "settings", relay: RELAY_STORED },
  "settings-relay-private": {
    ...base(idle),
    nav: "settings",
    relay: { ...RELAY_STORED, stored: { path: SETTINGS_PATH, relay: RELAY_PRIVATE } },
  },
  "settings-relay-chosen": {
    ...base(idle),
    nav: "settings",
    relay: RELAY_STORED,
    relayDraft: { ca: { mode: "chosen", pick: 4, name: "authority.pem" }, round: 0 },
  },
  "settings-relay-testing": {
    ...base(idle),
    nav: "settings",
    relay: RELAY_STORED,
    relayDraft: { ca: { mode: "keep" }, round: 0, test: { state: "running" } },
  },
  "settings-relay-admitted": {
    ...base(idle),
    nav: "settings",
    relay: RELAY_STORED,
    relayDraft: {
      ca: { mode: "keep" },
      round: 0,
      test: {
        state: "done",
        result: {
          ok: true,
          kind: "admitted",
          message:
            "The relay relay.example.org answered and admits this copy. Its certificate is valid for relay.example.org, checked against the certificate authorities of this computer.",
          next: "",
        },
      },
    },
  },
  "settings-relay-refused": {
    ...base(idle),
    nav: "settings",
    relay: RELAY_STORED,
    relayDraft: {
      ca: { mode: "keep" },
      round: 0,
      test: {
        state: "done",
        result: {
          ok: false,
          kind: "key_refused",
          message:
            "The relay relay.example.org does not know the admission key of this copy. The key may be mistyped, or it was withdrawn.",
          next: "Enter the key again, or ask the relay's operator for a new one.",
        },
      },
    },
  },
  "settings-relay-removing": {
    ...base(idle),
    nav: "settings",
    relay: RELAY_STORED,
    relayDraft: { ca: { mode: "keep" }, round: 0, removing: true },
  },
  "settings-relay-unreadable": {
    ...base(idle),
    nav: "settings",
    relay: {
      ...RELAY_NONE,
      source: "settings",
      stored: {
        path: SETTINGS_PATH,
        problem: {
          file: SETTINGS_PATH,
          field: "key",
          message: "Line 3: the value of `key` must be text in double quotes.",
        },
      },
    },
  },
  "settings-relay-file": { ...base(idle), nav: "settings", relay: RELAY_FILE },
  "settings-relay-file-problem": { ...base(idle), nav: "settings", relay: RELAY_FILE_PROBLEM },
  "settings-relay-run-once-file": {
    ...base(idle),
    nav: "settings",
    app: APP_RUN_ONCE,
    relay: RELAY_RUN_ONCE_FILE,
  },
  // A profile's texts come from a file that somebody else may have written.
  "settings-relay-xss": {
    ...base(idle),
    nav: "settings",
    relay: {
      editable: true,
      source: "file",
      file: {
        path: `C:\\${XSS_NAME}\\cretlink.toml`,
        relay: { host: XSS, address: XSS, keySet: true, ca: XSS, name: XSS },
      },
      stored: {
        path: SETTINGS_PATH,
        problem: { file: XSS, message: XSS },
      },
    },
  },
  error: {
    ...base(idle),
    error: {
      title: "The session could not start",
      message: "The address is already in use (listen)",
      next: "Check that this computer is connected to the network, then try again.",
    },
  },
  // The Files tab in every state of the explorer (ST-M010-002).
  ...Object.fromEntries(
    Object.entries(EXPLORERS).map(([name, explorer]): [string, TechnicianSnapshot] => [
      name,
      {
        ...base(
          consoleIn({ phase: "full_control" }, { session: true, target: true, displayName: "Sam" }),
        ),
        activity: name === "files-details" ? FEED : [],
        tab: "files",
        explorer,
        last: { id: SESSION_ID, target: TARGET, since: NOW - minutes(28) },
      },
    ]),
  ),
};
