// Accessible agent consent rendering in the shared CretLink design language
// (ST-M002-011, refines ST-M002-005; REQ-N-013/N-014/F-016/S-002).
//
// A stable skeleton is built once by `mount()`; `update()` refreshes regions in
// place so the `aria-live` control region persists (a replaced live region is
// not reliably announced). All identity- and status-derived text is written via
// `textContent`, never innerHTML, because process- and session-derived strings
// are treated as untrusted input. The control-state indicator never relies on
// colour alone: it always pairs a glyph and a text label (REQ-N-013). This
// module is presentation only — the command set, intent vocabulary, and phase
// model are unchanged from ST-M002-005.

import {
  type ActivityDelta,
  type ConsentView,
  type DiscoveredConsoleView,
  type EffectiveIdentity,
  type AgentPhase,
  controlLevel,
  sessionIsLive,
  awaitingConsent,
} from "./api";

// Intent labels emitted by interactive elements and dispatched centrally.
export type Intent =
  | { kind: "grant" }
  | { kind: "decline" }
  | { kind: "stop" }
  | { kind: "simulate-stop" }
  | { kind: "reset" }
  | { kind: "start-live"; code: string }
  // Browse the LAN for advertising consoles, and dial one that was found. A
  // discovered address carries no authority until the approval code is compared
  // (ST-M002-012).
  | { kind: "browse" }
  | { kind: "connect-console"; address: string };

type IntentHandler = (intent: Intent) => void;

// The element to focus after the next `update()`, set by whatever triggered the
// state change so focus moves deterministically (REQ-N-014).
let pendingFocus: string | null = null;

export function requestFocus(key: string): void {
  pendingFocus = key;
}

let onIntent: IntentHandler = () => {};

// Transient LAN discovery state (ST-M002-012). This is deliberately NOT part of
// the authoritative `ConsentView`: a browsed console carries no session
// authority, so it lives here and is rendered only as an unauthenticated
// suggestion. Trust still comes solely from comparing the Session Approval Code
// after dialing.
type BrowseStatus = "idle" | "searching" | "done";

interface Discovery {
  status: BrowseStatus;
  consoles: DiscoveredConsoleView[];
}

let discovery: Discovery = { status: "idle", consoles: [] };

// The last rendered view, kept so a discovery update can refresh the
// browse-aware regions (pill, discovery body) without a backend round-trip.
let lastView: ConsentView | null = null;

// Replaces the discovery snapshot and refreshes the browse-aware regions. Called
// by `main.ts` around each `browse_consoles` round-trip; it never fabricates
// candidates — an empty list stays empty.
export function setDiscovery(
  status: BrowseStatus,
  consoles: DiscoveredConsoleView[],
): void {
  discovery = { status, consoles };
  if (lastView) {
    renderPill(lastView);
    renderDiscovery(lastView);
  }
}

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

// Builds the persistent skeleton and wires the delegated event handlers once.
export function mount(root: HTMLElement, handler: IntentHandler): void {
  onIntent = handler;
  document.body.classList.add("app-agent");
  root.setAttribute("aria-busy", "false");
  root.replaceChildren();

  const wrap = el("div", "wrap");

  // Identity strip: this computer + live session state pill.
  const head = el("header", "app-head");
  const brand = el("div", "brand");
  const dot = el("span", "brand-dot");
  dot.setAttribute("aria-hidden", "true");
  const name = el("div", "app-name", "CretLink Support");
  const meta = el("small");
  meta.id = "agent-meta";
  meta.textContent = "This computer";
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

  const demo = el("p", "demo-banner");
  demo.id = "demo-banner";
  demo.setAttribute("role", "note");
  demo.hidden = true;

  const error = el("p", "error");
  error.id = "error";
  error.setAttribute("role", "alert");
  error.setAttribute("aria-live", "assertive");
  error.hidden = true;

  // Connect card (ST-M002-012): the customer first sees the technician's console
  // discovered on the LAN and picks it; the typed connection code stays as an
  // always-available fallback. Both paths dial the same real session
  // (ST-M002-009). Shown only when a live dial may start; the discovery region is
  // rebuilt from transient browse results by `renderDiscovery`.
  const connect = el("section", "card connect");
  connect.id = "connect";
  connect.setAttribute("aria-labelledby", "connect-heading");
  const connectEyebrow = el("p", "f-eyebrow", "Connect to your technician");
  connectEyebrow.id = "connect-heading";
  const connectTitle = el("p", "f-title");
  connectTitle.id = "connect-title";
  const connectHelp = el("p", "f-help");
  connectHelp.id = "connect-help";
  // Discovery body: the browse-derived pick-list or its searching/empty state.
  const discoBody = el("div", "disco-body");
  discoBody.id = "disco-body";
  const orSep = el("div", "or-sep");
  orSep.append(el("span", undefined, "or paste a code"));
  const fld = el("div", "fld");
  const codeLabel = el("label", undefined, "Connection code");
  codeLabel.setAttribute("for", "connect-code");
  const input = el("input", "connect-input");
  input.id = "connect-code";
  input.type = "text";
  input.autocomplete = "off";
  input.spellcheck = false;
  input.setAttribute("inputmode", "text");
  input.placeholder = "e.g. 6RRR-2H8F  or  192.168.1.20:7000";
  input.addEventListener("keydown", onConnectKeydown);
  fld.append(codeLabel, input);
  const connectActions = el("div", "f-actions");
  connectActions.append(
    actionButton("Connect", "start-live", "action:start-live", "brand"),
  );
  connect.append(
    connectEyebrow,
    connectTitle,
    connectHelp,
    discoBody,
    orSep,
    fld,
    connectActions,
  );

  // Session card.
  const session = el("section", "card consent");
  session.id = "consent";

  // The always-visible control-state indicator (REQ-N-013): glyph + label, and
  // an `aria-live` region so a change of control is announced.
  const control = el("p", "control-level level-none");
  control.id = "control";
  control.setAttribute("role", "status");
  control.setAttribute("aria-live", "polite");

  const status = el("p", "status");
  status.id = "status";

  const flow = el("div", "flow");
  flow.id = "flow";

  // The live activity mirror (ST-M004-008). It is a persistent region — NOT part
  // of `flow`, which is rebuilt on every snapshot — so the higher-frequency
  // `"agent-activity"` stream can patch job cards in place without a whole-view
  // re-render. It is hidden outside full control and cleared by `update()` on any
  // other phase, so it can never outlive the session (fail-closed).
  const activity = el("section", "activity");
  activity.id = "activity-feed";
  activity.hidden = true;
  activity.setAttribute("aria-label", "Commands run in this session");
  const feedHead = el("div", "feed-head");
  feedHead.append(
    el("span", "fh-title", "Activity"),
    el("span", "fh-sub", "What your technician runs"),
  );
  const jobs = el("div", "jobs");
  jobs.id = "job-list";
  activity.append(feedHead, jobs);

  const actions = el("div", "f-actions");
  actions.id = "actions";

  const stopBar = el("div", "stop-bar");
  stopBar.id = "stop-bar";

  session.append(control, status, flow, activity, actions, stopBar);

  const footer = el("p", "muted footer");
  footer.id = "footer";

  wrap.append(head, demo, error, connect, session, footer);
  root.append(wrap);

  root.addEventListener("click", onClick);
  document.addEventListener("keydown", onGlobalKeydown);
}

function onClick(event: MouseEvent): void {
  const target = event.target;
  if (!(target instanceof HTMLElement)) return;
  const actionEl = target.closest<HTMLElement>("[data-action]");
  if (!actionEl || actionEl.getAttribute("aria-disabled") === "true") return;
  dispatch(actionEl);
}

function dispatch(actionEl: HTMLElement): void {
  switch (actionEl.dataset.action) {
    case "grant":
      onIntent({ kind: "grant" });
      break;
    case "decline":
      onIntent({ kind: "decline" });
      break;
    case "stop":
      onIntent({ kind: "stop" });
      break;
    case "simulate-stop":
      onIntent({ kind: "simulate-stop" });
      break;
    case "reset":
      onIntent({ kind: "reset" });
      break;
    case "start-live":
      submitConnect();
      break;
    case "browse":
      onIntent({ kind: "browse" });
      break;
    case "connect-console": {
      // The address is a discovered candidate the backend reported; it grants no
      // authority by itself — the dial still funnels through the same joiner and
      // approval-code comparison as a typed code (ST-M002-012).
      const address = actionEl.dataset.address;
      if (address) onIntent({ kind: "connect-console", address });
      break;
    }
    default:
      break;
  }
}

// Reads the connection-code field and dispatches a live dial. An empty field is
// not an error — it just refocuses the input rather than dialing nowhere.
function submitConnect(): void {
  const input = document.getElementById("connect-code");
  if (!(input instanceof HTMLInputElement)) return;
  const code = input.value.trim();
  if (!code) {
    input.focus();
    return;
  }
  onIntent({ kind: "start-live", code });
}

// Enter inside the code field submits the dial, so the flow stays keyboard-only
// (REQ-N-014).
function onConnectKeydown(event: KeyboardEvent): void {
  if (event.key === "Enter") {
    event.preventDefault();
    submitConnect();
  }
}

function onGlobalKeydown(event: KeyboardEvent): void {
  // Escape is the always-available emergency stop while a session is live.
  if (event.key === "Escape") {
    const stop = document.getElementById("stop-button");
    if (stop && stop.getAttribute("aria-disabled") !== "true") {
      event.preventDefault();
      onIntent({ kind: "stop" });
    }
  }
}

// Shows or clears the assertive error banner (REQ-N-015). Passing `null` hides
// it after a subsequent successful action.
export function showError(message: string | null): void {
  const error = document.getElementById("error");
  if (!error) return;
  if (message) {
    error.textContent = `⚠ ${message}`;
    error.hidden = false;
  } else {
    error.textContent = "";
    error.hidden = true;
  }
}

export function update(view: ConsentView): void {
  lastView = view;
  renderDemoBanner(view);
  renderPill(view);
  renderConnect(view);
  renderDiscovery(view);
  renderControl(view);
  renderStatus(view);
  renderFlow(view);
  syncActivity(view);
  renderActions(view);
  renderStop(view);
  renderFooter(view);
  applyPendingFocus();
}

function renderDemoBanner(view: ConsentView): void {
  const demo = document.getElementById("demo-banner");
  if (!demo) return;
  demo.hidden = !view.simulated;
  if (view.simulated) {
    demo.textContent =
      "DEMO — SIMULATED. No technician is connected and granting confers no real control. This window previews exactly what a customer would see.";
  }
}

interface PillSpec {
  kind: "idle" | "pending" | "live";
  text: string;
}

function pillFor(phase: AgentPhase): PillSpec {
  switch (phase.phase) {
    case "idle":
      return { kind: "idle", text: "Not connected" };
    case "announced":
      return { kind: "pending", text: "Reaching out" };
    case "pairing":
    case "awaiting_approval_compare":
      return { kind: "pending", text: "Compare code" };
    case "awaiting_consent":
      return { kind: "pending", text: "Your decision" };
    case "full_control":
      return { kind: "live", text: "UNDER FULL CONTROL" };
    case "stopping":
      return { kind: "pending", text: "Stopping" };
    case "stopped":
      return { kind: "idle", text: "Ended" };
    case "failed":
      return { kind: "idle", text: "Failed" };
    default:
      return { kind: "idle", text: "Not connected" };
  }
}

function renderPill(view: ConsentView): void {
  const pill = document.getElementById("phase-pill");
  const text = document.getElementById("phase-pill-text");
  if (!pill || !text) return;
  const spec = pillSpec(view);
  pill.className = `pill ${spec.kind}`;
  text.textContent = spec.text;
}

// The pill spec, made browse-aware while idle: before a session the customer is
// actively looking for the technician's console, so the pill reflects the LAN
// browse rather than a flat "Not connected" (ST-M002-012).
function pillSpec(view: ConsentView): PillSpec {
  if (view.status.phase.phase === "idle" && view.can_connect) {
    if (discovery.status === "searching") {
      return { kind: "pending", text: "Searching…" };
    }
    if (discovery.consoles.length > 0) {
      return { kind: "pending", text: "Console found" };
    }
  }
  return pillFor(view.status.phase);
}

// Shows the connect card only when a live dial may start: no session yet, or a
// finished one that can be retried. During a live session it is hidden so the
// customer's attention stays on the pairing/consent flow.
function renderConnect(view: ConsentView): void {
  const connect = document.getElementById("connect");
  if (!connect) return;
  connect.hidden = !view.can_connect;
}

// Rebuilds the discovery region of the connect card from the transient browse
// snapshot (ST-M002-012). Every candidate string is backend-derived and written
// via `textContent`; nothing here is preselected or dialed automatically, and a
// non-dialable candidate is shown but not actionable.
function renderDiscovery(view: ConsentView): void {
  const title = document.getElementById("connect-title");
  const help = document.getElementById("connect-help");
  const body = document.getElementById("disco-body");
  if (!title || !help || !body) return;

  // The whole card is hidden outside a startable state; skip the rebuild then.
  if (!view.can_connect) {
    body.replaceChildren();
    return;
  }

  const consoles = discovery.consoles;
  if (consoles.length > 0) {
    title.textContent = "Choose your technician’s console";
    help.hidden = true;
    const list = el("div", "disco-list");
    for (const found of consoles) list.append(discoItem(found));
    body.replaceChildren(list);
    return;
  }

  // No candidates yet: show the searching spinner, or "none found" with a retry.
  title.textContent = "Looking for your technician’s console…";
  help.hidden = false;
  help.textContent =
    "Ask your technician to start a live session. Their console appears here by itself on this network.";
  const empty = el("div", "disco-empty");
  if (discovery.status === "done") {
    empty.append(
      el("span", undefined, "No console found yet on this network"),
    );
    body.replaceChildren(
      empty,
      actionButton("Search again", "browse", "action:browse", "ghost"),
    );
  } else {
    const spin = el("span", "spin");
    spin.setAttribute("aria-hidden", "true");
    empty.append(
      spin,
      el("span", undefined, "Looking for a console on this network…"),
    );
    body.replaceChildren(empty);
  }
}

// One discovered console rendered as a pick-list row. Dialable candidates are
// buttons that emit a `connect-console` intent carrying the reported address;
// non-dialable ones (no reachable address) are shown greyed and inert so the
// customer sees them without being able to dial nowhere.
function discoItem(found: DiscoveredConsoleView): HTMLElement {
  const item = el("button", "disco-item");
  item.type = "button";
  const dialable = found.dialable && found.address.length > 0;
  if (dialable) {
    item.dataset.action = "connect-console";
    item.dataset.address = found.address;
  } else {
    item.classList.add("is-stale");
    item.setAttribute("aria-disabled", "true");
  }

  const dot = el("span", "di-dot");
  dot.setAttribute("aria-hidden", "true");

  const main = el("span", "di-main");
  main.append(document.createTextNode(found.display_name ?? found.instance_name));
  main.append(
    el(
      "small",
      undefined,
      dialable ? `${found.address} · this network` : "not reachable on this network",
    ),
  );

  const go = el("span", "di-go", dialable ? "Connect →" : "Unreachable");
  item.append(dot, main, go);
  return item;
}

function renderControl(view: ConsentView): void {
  const control = document.getElementById("control");
  if (!control) return;
  control.classList.remove("level-none", "level-connected", "level-full");
  const level = controlLevel(view);
  let label: string;
  if (level === "full") {
    label = "● UNDER FULL CONTROL — the technician can operate this computer now";
    control.classList.add("level-full");
  } else if (level === "pending") {
    label = "◐ AWAITING YOUR DECISION — no control has been granted";
    control.classList.add("level-connected");
  } else {
    label = "○ NO ACCESS — the technician cannot control this computer";
    control.classList.add("level-none");
  }
  control.textContent = label;
}

function renderStatus(view: ConsentView): void {
  const status = document.getElementById("status");
  if (!status) return;
  status.textContent = statusText(view.status.phase);
}

function statusText(phase: AgentPhase): string {
  switch (phase.phase) {
    case "idle":
      return "Enter the technician’s connection code to connect. No control has been granted.";
    case "announced":
      return "Reaching out to the technician. No control has been granted.";
    case "pairing":
    case "awaiting_approval_compare":
      return "A technician is connecting. Compare the approval code below. No control yet.";
    case "awaiting_consent":
      return "Review the request below. You decide whether to grant full control.";
    case "full_control":
      return "You granted full control. You can stop it at any time.";
    case "stopping":
      return "Stopping the session…";
    case "stopped":
      return `The session has ended (${phase.reason.replace(/_/g, " ")}). The technician has no access.`;
    case "failed":
      return `The session ended at ${phase.stage}: ${phase.message}. The technician has no access.`;
    default:
      return "";
  }
}

// The phase-specific middle of the session card: the SAC compare block, the
// risk disclosure, the live banner, and this computer's identity table.
function renderFlow(view: ConsentView): void {
  const flow = document.getElementById("flow");
  if (!flow) return;
  flow.replaceChildren();

  const phase = view.status.phase.phase;
  switch (phase) {
    case "announced":
      flow.append(waiting("Reaching your technician over the local network…"));
      flow.append(identityBlock(view.status.identity));
      break;
    case "pairing":
    case "awaiting_approval_compare":
      if (view.approval_code) flow.append(approvalBlock(view.approval_code));
      // The affirmative is passive: once the technician confirms the codes match,
      // the runtime echoes `AwaitingConsent` and this window advances on its own
      // (ST-M002-013). Make that wait explicit so the customer is not left
      // wondering whether their "it matches" needs a click.
      flow.append(
        waiting(
          "If it matches, wait here — your technician confirms next, then you decide on control.",
        ),
      );
      flow.append(identityBlock(view.status.identity));
      break;
    case "awaiting_consent":
      flow.append(riskBox(view.disclosure));
      flow.append(identityBlock(view.status.identity));
      break;
    case "full_control": {
      const banner = el("div", "live-banner");
      const dot = el("span", "pd");
      dot.setAttribute("aria-hidden", "true");
      banner.append(
        dot,
        el("span", undefined, "Full control is live — the technician can operate this computer now."),
      );
      flow.append(banner);
      flow.append(identityBlock(view.status.identity));
      break;
    }
    case "stopping":
      flow.append(waiting("Ending the session and revoking control…"));
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

// The agent's Session Approval Code compare frame (ST-M002-013). The
// same six digits are shown on the technician's console; the customer reads them
// aloud and compares. The customer's "it matches" is deliberately *passive*: the
// wire only advances when the technician confirms the match and the runtime
// echoes `AwaitingConsent` back (see `renderFlow`/`renderActions`), so this frame
// offers no affirmative button that would fake a wire action — only the honest
// mismatch escape. Trust comes solely from this comparison (REQ-S-006).
function approvalBlock(code: string): HTMLElement {
  const block = el("div", "approval");
  block.append(
    el("p", "f-eyebrow", "Session Approval Code"),
    el("p", "f-title", "Does this match what your technician says?"),
    el("div", "sac-chip", code),
    el(
      "p",
      "f-help",
      "Your technician will read a code out loud. Only continue if it is exactly the same. If it differs, stop — someone may be in the middle.",
    ),
  );
  return block;
}

// The canonical wire disclosure, rendered verbatim inside the risk box.
function riskBox(disclosure: string): HTMLElement {
  const box = el("div", "riskbox");
  box.append(
    el("p", "rb-title", "What granting full control means"),
    el("p", "rb-text", disclosure),
  );
  return box;
}

function identityBlock(id: EffectiveIdentity): HTMLElement {
  const wrap = el("div", "identity");
  wrap.setAttribute("aria-label", "This computer's identity");
  wrap.append(el("p", "region-tag", "This computer"));

  const account = id.domain ? `${id.domain}\\${id.username}` : id.username;
  const dl = el("dl", "idtable");
  const rows: Array<[string, string]> = [
    ["Signed in as", account],
    ["Administrator rights", id.elevated ? "Yes (elevated)" : "No (standard)"],
    ["Integrity level", id.integrity],
    ["Platform", id.is_windows ? "Windows" : "Non-Windows (identity limited)"],
  ];
  for (const [term, value] of rows) {
    dl.append(el("dt", undefined, term), el("dd", undefined, value));
  }
  wrap.append(dl);
  if (id.note) {
    wrap.append(el("p", "muted", id.note));
  }
  return wrap;
}

// --- Live activity mirror (ST-M004-008) ------------------------------------
//
// Fed by the separate `"agent-activity"` channel, the mirror reflects only what
// the customer side of `cretlink-session` actually emits: a command starting and
// a command finishing. There is deliberately no per-line output or duration —
// those are console-side only (see `crates/session/src/runtime.rs`) — so the
// mirror can never show the attending customer activity they did not receive
// over the wire (REQ-F-017, REQ-S-002). Every string is backend-built and
// written via `textContent`, never as HTML (REQ-S-016, REQ-S-018).

interface JobCard {
  status: HTMLElement;
  meta: HTMLElement;
}

// Job id → its live card, so a later `finished` delta patches the same card the
// `started` delta opened instead of appending a duplicate.
const jobCards = new Map<number, JobCard>();

// Patches the activity feed from one delta. A `started` opens a fresh collapsible
// job card; a `finished` closes the matching card with the honest exit code and
// outcome. Exported so `main.ts` can wire it to the `"agent-activity"` listener.
export function pushActivity(delta: ActivityDelta): void {
  const list = document.getElementById("job-list");
  if (!list) return;
  if (delta.kind === "started") {
    // A repeated start for one job id would be anomalous; drop the stale card and
    // reopen so the feed never shows two cards for a single job.
    jobCards.delete(delta.job);
    list.append(makeJobCard(delta));
  } else {
    finishJobCard(delta);
  }
}

// Clears the mirror and forgets every tracked card. Called whenever the session
// is not under full control, so no activity from a past session can linger on
// screen (fail-closed).
function clearActivity(): void {
  jobCards.clear();
  const list = document.getElementById("job-list");
  if (list) list.replaceChildren();
}

// Shows the mirror only under full control and clears it otherwise. The feed is
// persistent DOM (see `mount`), so this toggles visibility rather than rebuilding
// it — the job cards are patched in place by `pushActivity`.
function syncActivity(view: ConsentView): void {
  const feed = document.getElementById("activity-feed");
  if (!feed) return;
  const live = controlLevel(view) === "full";
  feed.hidden = !live;
  if (!live) clearActivity();
}

// Builds one collapsible job card for a `started` delta and registers the parts a
// later `finished` delta will patch. The command is untrusted remote text written
// via `textContent`, never as HTML.
function makeJobCard(delta: ActivityDelta): HTMLElement {
  const card = el("details", "job");
  card.open = true;

  const summary = el("summary", "job-head");
  summary.append(el("span", "shell-badge", delta.shell ?? "shell"));
  summary.append(srcChip(delta.origin));
  summary.append(el("span", "cmd-inline", delta.command ?? ""));
  summary.append(el("span", "spacer"));
  const status = el("span", "job-status run", "running…");
  summary.append(status);
  const caret = el("span", "disclosure", "▾");
  caret.setAttribute("aria-hidden", "true");
  summary.append(caret);

  // The disclosure body shows the full (un-truncated) command and, once the job
  // ends, a single honest outcome line — never fabricated per-line output.
  const body = el("div", "term");
  body.append(el("div", "line", delta.command ?? ""));
  const meta = el("div", "line meta", "Running…");
  body.append(meta);

  card.append(summary, body);
  jobCards.set(delta.job, { status, meta });
  return card;
}

// Patches the matching card's status pill and outcome line from a `finished`
// delta. If no card was opened (a finish with no observed start), a minimal card
// is synthesised so the outcome is still shown honestly rather than dropped.
function finishJobCard(delta: ActivityDelta): void {
  let card = jobCards.get(delta.job);
  if (!card) {
    const list = document.getElementById("job-list");
    if (!list) return;
    list.append(makeJobCard({ job: delta.job, kind: "started" }));
    card = jobCards.get(delta.job);
    if (!card) return;
  }
  card.status.className = `job-status ${delta.ok ? "ok" : "bad"}`;
  card.status.textContent = statusLabel(delta);
  card.meta.className = "line meta";
  card.meta.textContent = outcomeLine(delta);
}

// A short status-pill label: the exit code when the process reported one, else
// the honest terminal outcome word (e.g. "timed out", "unknown") — never a
// fabricated success (REQ-N-008).
function statusLabel(delta: ActivityDelta): string {
  if (typeof delta.exitCode === "number") return `exit ${delta.exitCode}`;
  return delta.outcome ?? "ended";
}

// The disclosure-body outcome line, pairing the outcome word with the exit code
// when one exists, so the honest result is legible without relying on colour.
function outcomeLine(delta: ActivityDelta): string {
  const outcome = delta.outcome ?? (delta.ok ? "completed" : "ended");
  return typeof delta.exitCode === "number"
    ? `→ ${outcome} (exit ${delta.exitCode})`
    : `→ ${outcome}`;
}

// Maps the wire `origin` to an attribution chip: the AI via the MCP bridge shows
// as "Claude", the technician's own console action as "Technician", and a
// terminal-driven command as "Terminal". Honest attribution, not
// decoration (ST-M004-006, REQ-F-017, REQ-S-002).
function srcChip(origin?: ActivityDelta["origin"]): HTMLElement {
  if (origin === "mcp") return el("span", "job-src src-mcp", "Claude");
  if (origin === "terminal") {
    return el("span", "job-src src-terminal", "Terminal");
  }
  return el("span", "job-src src-console", "Technician");
}

function renderActions(view: ConsentView): void {
  const actions = document.getElementById("actions");
  if (!actions) return;
  actions.replaceChildren();

  const phase = view.status.phase.phase;
  if (phase === "pairing" || phase === "awaiting_approval_compare") {
    // During the compare window the only customer-side action is the honest
    // escape: if the codes differ, stop immediately. There is no affirmative
    // button — the match is confirmed by the technician over the wire, not faked
    // locally (ST-M002-013). This routes through `stop` (a revoke), not
    // `decline`, because there is no pending consent to decline yet.
    actions.append(
      actionButton(
        "It doesn’t match — stop",
        "stop",
        "action:compare-stop",
        "danger",
      ),
    );
    return;
  }
  if (awaitingConsent(view)) {
    actions.append(
      actionButton("Grant full control", "grant", "action:grant", "brand"),
      actionButton("Decline", "decline", "action:decline", "ghost"),
    );
    return;
  }
  if (phase === "full_control") {
    actions.append(
      el(
        "p",
        "muted",
        "Full control is active. Use Stop below (or press Escape) to end it immediately.",
      ),
    );
    if (view.simulated) {
      actions.append(
        actionButton(
          "Simulate technician ending the session",
          "simulate-stop",
          "action:simulate-stop",
          "ghost",
        ),
      );
    }
    return;
  }
  if (phase === "stopped" || phase === "failed") {
    actions.append(
      el(
        "p",
        "muted",
        "This session is over. A new session would require pairing and consent again.",
      ),
    );
    if (view.simulated) {
      actions.append(actionButton("Restart demo", "reset", "action:reset", "ghost"));
    }
  }
}

function renderStop(view: ConsentView): void {
  const bar = document.getElementById("stop-bar");
  if (!bar) return;
  bar.replaceChildren();
  const active = sessionIsLive(view);

  const button = el("button", "fbtn danger solid", "Stop / revoke control now");
  button.id = "stop-button";
  button.type = "button";
  button.dataset.action = "stop";
  button.dataset.focusKey = "action:stop";
  button.setAttribute("aria-keyshortcuts", "Escape");
  button.setAttribute("aria-label", "Stop the session and revoke control now");
  if (!active) {
    button.setAttribute("aria-disabled", "true");
    button.setAttribute("disabled", "true");
  }
  const hint = el(
    "p",
    "stop-hint",
    active
      ? "You can stop at any time — press Escape or this button."
      : "No active session to stop.",
  );
  bar.append(button, hint);
}

function renderFooter(view: ConsentView): void {
  const footer = document.getElementById("footer");
  if (!footer) return;
  footer.textContent = view.simulated
    ? "CretLink attended remote support — local demo. This window grants no real control."
    : "CretLink attended remote support. Control lasts only for this session and either side can stop it.";
}

function actionButton(
  label: string,
  action: string,
  focusKey: string,
  variant?: string,
): HTMLButtonElement {
  const button = el("button", variant ? `fbtn ${variant}` : "fbtn");
  button.type = "button";
  button.textContent = label;
  button.dataset.action = action;
  button.dataset.focusKey = focusKey;
  return button;
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
