// Accessible console rendering in the shared CretLink design language
// (ST-M002-011, refines ST-M002-002; REQ-N-013/N-014/F-016/S-002).
//
// A stable skeleton is built once by `mount()`; `update()` refreshes regions in
// place so the `aria-live` status region persists (a replaced live region is
// not reliably announced). Every target-/identity-derived string is written via
// `textContent`, never innerHTML, because remote-influenced strings are
// untrusted input. This module is presentation only.

import { listen, type UnlistenFn } from "@tauri-apps/api/event";
import { FitAddon } from "@xterm/addon-fit";
import { Terminal } from "@xterm/xterm";
import "@xterm/xterm/css/xterm.css";
import {
  CONSOLE_TERMINAL_EVENT,
  type ActivityLine,
  type ConsoleState,
  type TargetIdentity,
  type TerminalFrame,
  api,
  controlLevel,
  isCommandError,
  selectedIsSimulated,
  selectedTarget,
  sessionIsActive,
} from "./api";

// Intent labels emitted by interactive elements, dispatched centrally.
//
// The console listens; it never picks or dials a target (ST-M002-012), so the
// former select/connect intents are gone. `copy` is a pure frontend clipboard
// action (no backend round-trip) handled inline in `onClick`. `compare` and
// `mismatch` are the technician's Session Approval Code verdict, released only
// during the real compare window (ST-M002-013): `compare` confirms the codes
// match (releasing the gate over the wire), `mismatch` reports a difference and
// tears the session down fail-closed.
export type Intent =
  | { kind: "consent" }
  | { kind: "lost" }
  | { kind: "stop" }
  | { kind: "start-live" }
  | { kind: "compare" }
  | { kind: "mismatch" };

type IntentHandler = (intent: Intent) => void;

// The element to focus on the next `update()`, set by the intent that triggered
// a state change so focus moves deterministically (REQ-N-014).
let pendingFocus: string | null = null;

export function requestFocus(key: string): void {
  pendingFocus = key;
}

let onIntent: IntentHandler = () => {};

function el<K extends keyof HTMLElementTagNameMap>(
  tag: K,
  className?: string,
  textContent?: string,
): HTMLElementTagNameMap[K] {
  const node = document.createElement(tag);
  if (className) node.className = className;
  if (textContent !== undefined) node.textContent = textContent;
  return node;
}

// Builds the persistent skeleton and wires delegated event handlers once.
export function mount(root: HTMLElement, handler: IntentHandler): void {
  onIntent = handler;
  document.body.classList.add("app-console");
  root.replaceChildren();

  const wrap = el("div", "wrap");

  // Identity strip: this machine + live session state pill.
  const head = el("header", "app-head");
  const brand = el("div", "brand");
  const dot = el("span", "brand-dot");
  dot.setAttribute("aria-hidden", "true");
  const name = el("div", "app-name", "CretLink Console");
  const meta = el("small");
  meta.id = "tech-meta";
  name.append(meta);
  brand.append(dot, name);

  const pill = el("span", "pill idle");
  pill.id = "phase-pill";
  const pillDot = el("span", "pd");
  pillDot.setAttribute("aria-hidden", "true");
  const pillText = el("span", undefined, "Starting…");
  pillText.id = "phase-pill-text";
  pill.append(pillDot, pillText);
  head.append(brand, pill);

  const error = el("p", "error");
  error.id = "error";
  error.setAttribute("role", "alert");
  error.setAttribute("aria-live", "assertive");
  error.hidden = true;

  // Session panel. The console listens rather than choosing a target
  // (ST-M002-012), so there is no discovered-targets list to render.
  const session = el("section", "card session");
  session.id = "session";
  const status = el("p", "status");
  status.id = "status";
  status.setAttribute("role", "status");
  status.setAttribute("aria-live", "polite");
  const control = el("p", "control-level level-none");
  control.id = "control";
  const flow = el("div", "flow");
  flow.id = "flow";
  const actions = el("div", "f-actions");
  actions.id = "actions";
  const stopBar = el("div", "stop-bar");
  stopBar.id = "stop-bar";
  session.append(status, control, flow, actions, stopBar);

  wrap.append(head, error, session);
  root.append(wrap);

  root.addEventListener("click", onClick);
  document.addEventListener("keydown", onGlobalKeydown);
}

function onClick(event: MouseEvent): void {
  const target = (event.target as HTMLElement).closest<HTMLElement>("[data-action]");
  if (!target) return;
  const action = target.dataset.action;
  switch (action) {
    case "copy": {
      // Pure clipboard convenience; no backend authority is involved.
      const text = target.dataset.copy;
      if (text) void navigator.clipboard?.writeText(text);
      break;
    }
    case "tab": {
      // Pure frontend view state: switch the full-control tab and re-render the
      // flow region. The hosted session is untouched — no backend round-trip.
      const tab = target.dataset.tab as SessionTab | undefined;
      if (tab && tab !== activeTab && currentState) {
        activeTab = tab;
        renderFlow(currentState);
      }
      break;
    }
    case "consent":
      onIntent({ kind: "consent" });
      break;
    case "lost":
      onIntent({ kind: "lost" });
      break;
    case "stop":
      onIntent({ kind: "stop" });
      break;
    case "start-live":
      onIntent({ kind: "start-live" });
      break;
    case "compare":
      onIntent({ kind: "compare" });
      break;
    case "mismatch":
      onIntent({ kind: "mismatch" });
      break;
    default:
      break;
  }
}

function onGlobalKeydown(event: KeyboardEvent): void {
  // Escape is the always-available emergency stop (REQ-F-018).
  if (event.key === "Escape") {
    event.preventDefault();
    onIntent({ kind: "stop" });
  }
}

let currentState: ConsoleState | null = null;

export function showError(message: string | null): void {
  const error = document.getElementById("error");
  if (!error) return;
  if (message) {
    error.textContent = message;
    error.hidden = false;
  } else {
    error.textContent = "";
    error.hidden = true;
  }
}

export function update(state: ConsoleState): void {
  currentState = state;
  renderTechnician(state);
  renderPill(state);
  renderStatus(state);
  renderControl(state);
  renderFlow(state);
  renderActions(state);
  renderStop(state);
  applyPendingFocus();
}

function renderTechnician(state: ConsoleState): void {
  const meta = document.getElementById("tech-meta");
  if (!meta) return;
  meta.textContent = `${state.technician.label} · ${state.technician.hostname} · ${state.technician.os}`;
}

interface PillSpec {
  kind: "idle" | "pending" | "live";
  text: string;
}

function pillFor(phase: ConsoleState["phase"]): PillSpec {
  switch (phase.phase) {
    case "idle":
      return { kind: "idle", text: "No access" };
    case "listening":
      return { kind: "pending", text: "Listening" };
    case "discovered":
      return { kind: "idle", text: "Ready to connect" };
    case "connecting":
      return { kind: "pending", text: "Connecting" };
    case "awaiting_approval_code":
      return { kind: "pending", text: "Compare code" };
    case "awaiting_consent":
      return { kind: "pending", text: "Awaiting consent" };
    case "full_control":
      return { kind: "live", text: "FULL CONTROL" };
    case "stopping":
      return { kind: "pending", text: "Stopping" };
    case "stopped":
      return { kind: "idle", text: "Ended" };
    case "failed":
      return { kind: "idle", text: "Failed" };
    default:
      return { kind: "idle", text: "No access" };
  }
}

function renderPill(state: ConsoleState): void {
  const pill = document.getElementById("phase-pill");
  const text = document.getElementById("phase-pill-text");
  if (!pill || !text) return;
  const spec = pillFor(state.phase);
  pill.className = `pill ${spec.kind}`;
  text.textContent = spec.text;
}

function renderStatus(state: ConsoleState): void {
  const status = document.getElementById("status");
  if (!status) return;
  status.textContent = statusText(state);
}

function statusText(state: ConsoleState): string {
  const name = selectedTarget(state)?.display_name ?? "the target";
  switch (state.phase.phase) {
    case "idle":
      return "No session. Start a live session, then wait for the customer to connect.";
    case "listening":
      return "Listening on the local network. The customer appears here automatically once they connect. No access yet.";
    case "discovered":
      return `${name} is selected but not connected — no access yet.`;
    case "connecting":
      return `Connecting to ${name}. No control yet.`;
    case "awaiting_approval_code":
      return `Compare the approval code aloud with the customer. No control yet.`;
    case "awaiting_consent":
      return `Code confirmed. Waiting for the customer to grant full control. No control yet.`;
    case "full_control":
      return `FULL CONTROL is active on ${name}. Commands can change this remote computer.`;
    case "stopping":
      return `Stopping the session with ${name}…`;
    case "stopped":
      return `Session with ${name} stopped (${state.phase.reason.replace(/_/g, " ")}). No access.`;
    case "failed":
      return `Session failed at ${state.phase.stage}: ${state.phase.message}. No access.`;
    default:
      return "";
  }
}

function renderControl(state: ConsoleState): void {
  const control = document.getElementById("control");
  if (!control) return;
  control.classList.remove("level-none", "level-connected", "level-full");
  const level = controlLevel(state);
  if (level === "full_control") {
    control.textContent = "● FULL CONTROL";
    control.classList.add("level-full");
  } else if (level === "connected") {
    control.textContent = "◐ Connected — no control";
    control.classList.add("level-connected");
  } else {
    control.textContent = "○ No access";
    control.classList.add("level-none");
  }
}

// ── Live activity feed + full-control tabs (ST-M004-007) ─────────────────────
// The Activity tab renders a flat chronological buffer of `ActivityLine`s that
// arrive on the separate `"console-activity"` Tauri channel (never on
// `"console-state"`), so high-frequency job output never forces a whole-state
// re-render. Every line is backend-built and written with `textContent`
// (REQ-S-016/S-018) — the feed is real session data, never fabricated.
type SessionTab = "overview" | "terminal" | "activity";

const TABS: ReadonlyArray<{ id: SessionTab; label: string }> = [
  { id: "overview", label: "Overview" },
  { id: "terminal", label: "Terminal" },
  { id: "activity", label: "Activity" },
];

// Bound so a long session cannot grow the buffer without limit; the oldest
// lines fall off the top, matching the scroll container.
const MAX_ACTIVITY = 500;

const activityLog: ActivityLine[] = [];
let activeTab: SessionTab = "overview";
// Edge flag: true only while the full-control tab-strip is mounted. Used to
// reset the tab and clear the feed exactly once on entering/leaving full
// control, without fighting the technician's tab choice on every re-render.
let inFullControl = false;

// Appends a live activity line. Called by `main.ts` on each `"console-activity"`
// event. When the Activity tab is already mounted, the node is appended in place
// (and the empty-state note removed) so the feed streams without a full render.
export function pushActivity(line: ActivityLine): void {
  activityLog.push(line);
  if (activityLog.length > MAX_ACTIVITY) activityLog.shift();
  if (activeTab !== "activity") return;
  const feed = document.getElementById("activity-feed");
  if (!feed) return;
  feed.querySelector(".empty")?.remove();
  feed.append(activityNode(line));
  feed.scrollTop = feed.scrollHeight;
}

function fullControlView(state: ConsoleState): HTMLElement {
  const view = el("div", "session-live");
  view.append(tabStrip(), tabPanel(state));
  return view;
}

function tabStrip(): HTMLElement {
  const strip = el("div", "stabs");
  const group = el("div", "stab-group");
  group.setAttribute("role", "tablist");
  for (const tab of TABS) {
    const btn = el("button", `stab${tab.id === activeTab ? " active" : ""}`, tab.label);
    btn.type = "button";
    btn.dataset.action = "tab";
    btn.dataset.tab = tab.id;
    btn.setAttribute("role", "tab");
    btn.setAttribute("aria-selected", String(tab.id === activeTab));
    group.append(btn);
  }
  const live = el("span", "stab-live");
  const dot = el("span", "pd");
  dot.setAttribute("aria-hidden", "true");
  live.append(dot, el("span", undefined, "FULL CONTROL"));
  strip.append(group, live);
  return strip;
}

function tabPanel(state: ConsoleState): HTMLElement {
  switch (activeTab) {
    case "terminal":
      return terminalPanel(state);
    case "activity":
      return activityPanel(state);
    default:
      return overviewPanel(state);
  }
}

function overviewPanel(state: ConsoleState): HTMLElement {
  const panel = el("div", "stab-panel");
  const banner = el("div", "live-banner");
  const dot = el("span", "pd");
  dot.setAttribute("aria-hidden", "true");
  const name = selectedTarget(state)?.display_name ?? "the target";
  banner.append(dot, el("span", undefined, `Full control is live on ${name}.`));
  panel.append(banner);
  const id = selectedTarget(state)?.identity;
  if (id) panel.append(identityBlock(id));
  panel.append(
    el(
      "p",
      "muted",
      "Commands can change this remote computer. Press Stop (Esc) to end control at any time.",
    ),
  );
  return panel;
}

function activityPanel(state: ConsoleState): HTMLElement {
  const panel = el("div", "stab-panel");
  panel.append(feedHead("Activity", `session ${sessionShort(state)}`));
  const feed = el("div", "activity-feed");
  feed.id = "activity-feed";
  if (activityLog.length === 0) {
    feed.append(
      el(
        "p",
        "empty",
        "No activity yet. Actions by Claude and the technician appear here as they happen.",
      ),
    );
  } else {
    for (const line of activityLog) feed.append(activityNode(line));
  }
  panel.append(feed);
  panel.append(
    el(
      "p",
      "term-legend",
      "Attributed live: each line is a real action by Claude (MCP) or the technician — nothing is hidden.",
    ),
  );
  return panel;
}

// ── Interactive terminal (ST-M008-001) ──────────────────────────────────────
// The console GUI hosts the session backbone (ST-M004-007), so the terminal
// panel attaches in-process: `open_terminal` opens a remote pty on the consented
// session and streams raw VT bytes back on the `"console-terminal"` event. xterm
// renders them; keystrokes go back as raw UTF-8 bytes so the *remote* pty owns
// the echo — the panel never echoes locally and never fabricates a shell. Every
// backend command fails closed with `no_consented_session` outside full control.
//
// One shell per full-control session, kept alive across tab switches: the
// xterm host div is reused, so leaving and re-entering the Terminal tab
// re-parents the same live terminal.
// The shell is torn down only when full control ends (`dispose()`), which closes
// the remote pty — fail-closed, with no residue into the next session.
type TerminalStatus = "connecting" | "live" | "closed" | "revoked" | "unavailable";

const TERM_BADGE: Record<TerminalStatus, { cls: string; label: string }> = {
  connecting: { cls: "warn", label: "connecting…" },
  live: { cls: "", label: "live" },
  closed: { cls: "bad", label: "shell closed" },
  revoked: { cls: "bad", label: "revoked" },
  unavailable: { cls: "bad", label: "unavailable" },
};

class ConsoleTerminal {
  private term: Terminal | null = null;
  private fit: FitAddon | null = null;
  private host: HTMLDivElement | null = null;
  private termId: number | null = null;
  private starting = false;
  private ended = false;
  private status: TerminalStatus = "connecting";
  private unlisten: UnlistenFn | null = null;
  private badge: HTMLElement | null = null;
  private resizeHooked = false;

  // The persistent div xterm opens into. Reused across tab switches so the live
  // terminal survives a panel rebuild; recreated fresh after `dispose()`.
  hostElement(): HTMLDivElement {
    if (!this.host) {
      const host = document.createElement("div");
      host.className = "sh-xterm";
      this.host = host;
    }
    return this.host;
  }

  // Called (idempotently) via rAF once the host is in the live DOM. Creates the
  // xterm instance on first use, fits it, and lazily opens the remote pty.
  mount(badge: HTMLElement): void {
    this.badge = badge;
    this.paintBadge();
    const host = this.host;
    if (!host || !host.isConnected) return;
    if (!this.term) {
      const term = new Terminal({
        convertEol: false,
        cursorBlink: true,
        fontFamily: "'Cascadia Code', 'Consolas', ui-monospace, monospace",
        fontSize: 12,
        scrollback: 2000,
        theme: { background: "#0b1220", foreground: "#d6deec", cursor: "#7fe0b0" },
      });
      const fit = new FitAddon();
      term.loadAddon(fit);
      term.open(host);
      term.onData((data) => this.sendInput(data));
      term.onResize(({ cols, rows }) => this.sendResize(cols, rows));
      this.term = term;
      this.fit = fit;
      if (!this.resizeHooked) {
        window.addEventListener("resize", () => this.refit());
        this.resizeHooked = true;
      }
    }
    this.refit();
    if (this.termId === null && !this.starting && !this.ended) {
      void this.start();
    }
  }

  private refit(): void {
    if (this.term && this.host?.isConnected) {
      try {
        this.fit?.fit();
      } catch {
        // A zero-size container (mid-transition) throws; the next mount refits.
      }
    }
  }

  private async start(): Promise<void> {
    this.starting = true;
    this.setStatus("connecting");
    try {
      const cols = this.term?.cols ?? 80;
      const rows = this.term?.rows ?? 24;
      const info = await api.openTerminal(cols, rows);
      this.termId = info.term;
      // Subscribe before syncing size so no early output is missed; frames are
      // filtered by our own `term` id, so a stale terminal's frames are ignored.
      this.unlisten = await listen<TerminalFrame>(CONSOLE_TERMINAL_EVENT, (event) => {
        this.onFrame(event.payload);
      });
      this.setStatus("live");
      // Sync the pty to the fitted window so full-screen TUIs render correctly.
      this.sendResize(cols, rows);
    } catch (error) {
      this.ended = true;
      this.setStatus("unavailable");
      this.term?.writeln(`\r\n\x1b[31m[terminal unavailable: ${describeErr(error)}]\x1b[0m`);
    } finally {
      this.starting = false;
    }
  }

  private onFrame(frame: TerminalFrame): void {
    // Ignore frames for a terminal that is not ours (or after we finalized).
    if (this.termId === null || frame.term !== this.termId) return;
    switch (frame.kind) {
      case "output":
        if (frame.data && frame.data.length > 0) {
          this.term?.write(new Uint8Array(frame.data));
        }
        break;
      case "exited": {
        const code = frame.exitCode != null ? ` (exit ${frame.exitCode})` : "";
        const how = frame.outcome ? ` ${frame.outcome}` : "";
        this.term?.writeln(`\r\n\x1b[90m[shell ended${how}${code}]\x1b[0m`);
        this.finalize("closed");
        break;
      }
      case "revoked":
        this.term?.writeln("\r\n\x1b[31m[session revoked — terminal closed]\x1b[0m");
        this.finalize("revoked");
        break;
      case "dropped":
        this.term?.writeln(
          "\r\n\x1b[33m[output dropped — consumer overran; terminal detached]\x1b[0m",
        );
        this.finalize("closed");
        break;
    }
  }

  private sendInput(data: string): void {
    if (this.termId === null) return;
    const bytes = Array.from(new TextEncoder().encode(data));
    void api.termInput(this.termId, bytes).catch((error) => this.onCommandFailure(error));
  }

  private sendResize(cols: number, rows: number): void {
    if (this.termId === null) return;
    void api.termResize(this.termId, cols, rows).catch((error) => this.onCommandFailure(error));
  }

  // A rejected input/resize means the session dropped under us (fail-closed):
  // stop treating the shell as live so no further keystrokes are attempted.
  private onCommandFailure(error: unknown): void {
    if (isCommandError(error) && error.stage === "no_consented_session") {
      this.finalize("revoked");
    }
  }

  private finalize(status: TerminalStatus): void {
    this.termId = null;
    this.ended = true;
    if (this.unlisten) {
      this.unlisten();
      this.unlisten = null;
    }
    this.setStatus(status);
  }

  private setStatus(status: TerminalStatus): void {
    this.status = status;
    this.paintBadge();
  }

  private paintBadge(): void {
    const badge = this.badge;
    if (!badge) return;
    const spec = TERM_BADGE[this.status];
    badge.className = `sh-badge${spec.cls ? ` ${spec.cls}` : ""}`;
    badge.replaceChildren();
    const dot = el("span", "pd");
    dot.setAttribute("aria-hidden", "true");
    badge.append(dot, el("span", undefined, spec.label));
  }

  // Full control ended: close the remote pty and drop every reference so the
  // next session starts from a clean slate (fail-closed, no residue).
  dispose(): void {
    const id = this.termId;
    this.termId = null;
    this.ended = false;
    this.starting = false;
    this.status = "connecting";
    if (this.unlisten) {
      this.unlisten();
      this.unlisten = null;
    }
    if (id !== null) {
      void api.termClose(id).catch(() => {
        // A pty that already exited maps to an honest error we can drop here.
      });
    }
    if (this.term) {
      this.term.dispose();
      this.term = null;
    }
    this.fit = null;
    this.host = null;
    this.badge = null;
  }
}

const consoleTerminal = new ConsoleTerminal();

// Formats a backend error for a one-line terminal notice, preferring the
// fail-closed stage the service drew.
function describeErr(error: unknown): string {
  if (isCommandError(error)) return `${error.stage}: ${error.message}`;
  if (error instanceof Error) return error.message;
  return "unknown error";
}

function terminalPanel(state: ConsoleState): HTMLElement {
  const panel = el("div", "stab-panel");
  panel.append(feedHead("Terminal", "shared interactive shell"));

  // Terminal chrome (`.shell`): traffic-light dots, a target-identifying
  // title, and a live/closed status badge driven honestly by the controller.
  const shell = el("div", "shell");
  const bar = el("div", "sh-bar");
  const dots = el("div", "sh-dots");
  dots.setAttribute("aria-hidden", "true");
  dots.append(el("i", "r"), el("i", "y"), el("i", "g"));
  const hostname =
    selectedTarget(state)?.identity?.hostname ??
    selectedTarget(state)?.display_name ??
    "remote";
  const title = el("div", "sh-title", "PowerShell · ");
  title.append(el("b", undefined, hostname));
  const badge = el("span", "sh-badge");
  bar.append(dots, title, badge);

  const body = el("div", "sh-body");
  body.append(consoleTerminal.hostElement());
  shell.append(bar, body);
  panel.append(shell);

  panel.append(
    el(
      "p",
      "term-legend",
      `Session ${sessionShort(state)} · keystrokes run on the remote computer. Press Esc to stop the session.`,
    ),
  );

  // Mount after the host is in the live DOM so xterm can measure and fit.
  requestAnimationFrame(() => consoleTerminal.mount(badge));
  return panel;
}

function feedHead(title: string, sub: string): HTMLElement {
  const head = el("div", "feed-head");
  head.append(el("span", "fh-title", title), el("span", "fh-sub", sub));
  return head;
}

function sessionShort(state: ConsoleState): string {
  return state.session ? state.session.slice(0, 8) : "—";
}

// One activity row: a source chip, the backend-built text, and — when the line
// carries an outcome — a status pill. Untrusted preview text is written with
// `textContent` via `el()` (REQ-S-016/S-018).
function activityNode(line: ActivityLine): HTMLElement {
  const card = el("div", "job");
  const head = el("div", "job-head");
  const chip = chipFor(line.source);
  head.append(el("span", `job-src ${chip.cls}`, chip.label));
  head.append(el("span", "cmd-inline", line.text));
  head.append(el("span", "spacer"));
  const status = statusPill(line);
  if (status) head.append(status);
  card.append(head);
  // The AI's stated purpose sits under the real operation it describes, never
  // in its place (REQ-F-026, ST-M009-007). Untrusted text, via `textContent`.
  if (line.purpose) {
    card.append(el("div", "job-purpose", `Stated purpose (from the AI): ${line.purpose}`));
  }
  return card;
}

function chipFor(source: ActivityLine["source"]): { cls: string; label: string } {
  switch (source) {
    case "claude":
      return { cls: "src-mcp", label: "Claude" };
    case "session":
      return { cls: "src-terminal", label: "Session" };
    default:
      return { cls: "src-terminal", label: "cretlink" };
  }
}

function statusPill(line: ActivityLine): HTMLElement | null {
  switch (line.kind) {
    case "start":
      return el("span", "job-status run", "running");
    case "finished":
      return el(
        "span",
        `job-status ${line.ok === false ? "bad" : "ok"}`,
        line.ok === false ? "failed" : "done",
      );
    case "dropped":
      return el("span", "job-status bad", "truncated");
    default:
      return null;
  }
}

// The phase-specific middle of the session panel: the SAC compare block, the
// waiting spinner, the live banner, and the selected target's identity table.
function renderFlow(state: ConsoleState): void {
  const flow = document.getElementById("flow");
  if (!flow) return;
  const nowFull = state.phase.phase === "full_control";
  if (nowFull !== inFullControl) {
    // Entering or leaving full control: reset to Overview and clear the feed so
    // a new session never shows a prior session's activity (fail-closed).
    inFullControl = nowFull;
    activeTab = "overview";
    activityLog.length = 0;
    // Leaving full control tears the terminal down: close the remote pty and
    // drop the xterm instance so the shell never outlives its session.
    if (!nowFull) consoleTerminal.dispose();
  }
  flow.replaceChildren();

  switch (state.phase.phase) {
    case "listening":
      flow.append(listeningBlock(state.phase.code, state.phase.address));
      break;
    case "connecting":
      flow.append(waiting("Establishing the encrypted channel…"));
      break;
    case "awaiting_approval_code":
      flow.append(approvalBlock(state.phase.code));
      break;
    case "awaiting_consent":
      flow.append(
        waiting("Waiting for the customer to grant full control on their own computer…"),
      );
      break;
    case "full_control":
      flow.append(fullControlView(state));
      break;
    case "discovered": {
      const id = selectedTarget(state)?.identity;
      if (id) flow.append(identityBlock(id));
      break;
    }
    case "stopping":
      flow.append(waiting("Closing the session and releasing control…"));
      break;
    default:
      break;
  }
}

function waiting(text: string): HTMLElement {
  const row = el("div", "waiting");
  const spin = el("span", "spin");
  spin.setAttribute("aria-hidden", "true");
  row.append(spin, el("span", undefined, text));
  return row;
}

// The listening surface: the console advertises itself and shows the shareable
// connection code and its listen address (both backend-provided, ST-M002-012).
// The customer's app browses and dials in; nothing here is chosen or typed by
// the technician.
function listeningBlock(code: string, address: string): HTMLElement {
  const block = el("div", "approval");
  block.append(
    el("p", "approval-label", "Your connection code"),
    el("p", "code-chip", code),
  );

  const addrRow = el("div", "addr-row");
  addrRow.append(el("span", "addr", address));
  const copy = el("button", "mini", "Copy");
  copy.type = "button";
  copy.dataset.action = "copy";
  copy.dataset.copy = address;
  addrRow.append(copy);
  block.append(addrRow);

  block.append(
    el(
      "p",
      "approval-hint",
      "Read the code to the customer only if their app cannot find you automatically.",
    ),
    waiting("Waiting for the customer to connect…"),
  );
  return block;
}

// The compare gate (ST-M002-013): the encrypted channel is up and both screens
// now show the same Session Approval Code. The session is paused here until the
// technician confirms the codes match aloud — the humanly-perceptible check that
// no attacker sits in the middle (REQ-S-006). The code string is backend-derived
// from the Noise handshake and written via `textContent`.
function approvalBlock(code: string): HTMLElement {
  const block = el("div", "approval");
  block.append(
    el("p", "f-eyebrow", "Session Approval Code"),
    el("p", "f-title", "Compare this code aloud"),
    el("div", "sac-chip", code),
    el(
      "p",
      "f-help",
      "Read it to the customer. If it matches their screen, continue. If it differs, stop — someone may be in the middle.",
    ),
  );
  return block;
}

function identityBlock(id: TargetIdentity): HTMLElement {
  const wrap = el("div", "identity");
  wrap.append(el("p", "region-tag", "Remote target"));
  const dl = el("dl", "idtable");
  const row = (label: string, value: string): void => {
    dl.append(el("dt", undefined, label), el("dd", undefined, value));
  };
  row("Hostname", id.hostname);
  row("OS", id.os_version);
  row("Arch", id.arch);
  row("Elevated", id.elevated ? "Yes" : "No");
  row("Agent", id.agent_version);
  if (id.simulated) row("Mode", "Simulated");
  wrap.append(dl);
  return wrap;
}

function renderActions(state: ConsoleState): void {
  const actions = document.getElementById("actions");
  if (!actions) return;
  actions.replaceChildren();

  const simulated = selectedIsSimulated(state);

  switch (state.phase.phase) {
    case "idle":
      actions.append(button("Start live session", "start-live", "action:start-live", "primary"));
      break;
    case "listening":
      actions.append(
        note("Waiting for the customer to connect. They appear here automatically — no target is chosen from this side."),
      );
      break;
    case "awaiting_approval_code":
      // The compare gate is a real pause (ST-M002-013): the runtime waits here
      // until the technician confirms the codes match, which releases the gate
      // over the wire — only then is the customer asked to grant control. This is
      // not a local shortcut to control: no control exists until the customer's
      // real consent echoes back. A mismatch tears the session down fail-closed.
      actions.append(
        button("Codes match — continue", "compare", "action:compare", "good"),
        button("Codes don't match — stop", "mismatch", "action:mismatch", "danger"),
      );
      break;
    case "awaiting_consent":
      if (simulated) {
        actions.append(
          note("Consent is the customer's action on their own computer (simulated here)."),
          button("Simulate customer consent", "consent", "action:consent", "primary"),
        );
      } else {
        actions.append(
          note("Waiting for the customer to grant full control. This cannot be done from here."),
        );
      }
      break;
    case "full_control":
      if (simulated) {
        actions.append(button("Simulate target lost", "lost", "action:lost", "ghost"));
      } else {
        actions.append(note("Full control is active. Use Stop (Esc) to end it at any time."));
      }
      break;
    case "stopped":
    case "failed":
      actions.append(button("Start live session", "start-live", "action:start-live", "primary"));
      break;
    default:
      break;
  }
}

function note(text: string): HTMLElement {
  return el("p", "muted", text);
}

function renderStop(state: ConsoleState): void {
  const bar = document.getElementById("stop-bar");
  if (!bar) return;
  bar.replaceChildren();
  const active = sessionIsActive(state);
  const name = selectedTarget(state)?.display_name;

  const stop = el("button", "fbtn danger solid", "Stop session (Esc)");
  stop.id = "stop-button";
  stop.type = "button";
  stop.dataset.action = "stop";
  stop.dataset.focusKey = "action:stop";
  stop.setAttribute("aria-keyshortcuts", "Escape");
  if (active && name) stop.setAttribute("aria-label", `Stop session with ${name}`);
  if (!active) {
    stop.setAttribute("aria-disabled", "true");
    stop.setAttribute("disabled", "true");
  }
  const hint = el(
    "p",
    "stop-hint",
    active ? "Press Escape at any time to stop." : "No active session to stop.",
  );
  bar.append(stop, hint);
}

function button(
  label: string,
  action: string,
  focusKey: string,
  variant?: string,
): HTMLButtonElement {
  const btn = el("button", variant ? `fbtn ${variant}` : "fbtn");
  btn.type = "button";
  btn.textContent = label;
  btn.dataset.action = action;
  btn.dataset.focusKey = focusKey;
  return btn;
}

function applyPendingFocus(): void {
  if (!pendingFocus) return;
  const target = document.querySelector<HTMLElement>(
    `[data-focus-key="${cssEscape(pendingFocus)}"]`,
  );
  pendingFocus = null;
  if (target && target.getAttribute("aria-disabled") !== "true") {
    target.focus();
  }
}

// Minimal CSS attribute-selector escaping for our known focus keys.
function cssEscape(value: string): string {
  return value.replace(/["\\]/g, "\\$&");
}
