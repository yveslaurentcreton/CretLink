// The live session (`#t-live`, ST-M009-008): an always-visible full-control
// band, the target identity, Stop (Escape), the Overview / Terminal / Files /
// Audit tabs and a rail with the AI clients and the activity feed
// (REQ-F-006, REQ-F-016, REQ-F-017, REQ-F-018).
//
// All four tab panels are always rendered and the inactive ones hidden, so the
// embedded terminal (owned by the controller inside `data-keep` slots) lives
// across tab switches, and the feed keeps its scroll position.

import type { ActivityEntry } from "../../shared/contract";
import { h } from "../../shared/dom";
import { type IconName, icon } from "../../shared/icons";
import { selectedIsSimulated } from "../api";
import {
  type LiveTab,
  type TechnicianSnapshot,
  auditActionWords,
  auditActorWords,
  auditOutcomeWords,
  currentTarget,
  formatBytes,
  formatClock,
  formatTime,
  plural,
  sessionCounts,
  shortId,
  targetName,
  technicianName,
} from "../model";
import { button, feedList, kv, pill, resultBadge, whoChip } from "./common";

const TABS: ReadonlyArray<{ tab: LiveTab; label: string; icon: IconName }> = [
  { tab: "overview", label: "Overview", icon: "gauge" },
  { tab: "terminal", label: "Terminal", icon: "term" },
  { tab: "files", label: "Files", icon: "folder" },
  { tab: "audit", label: "Audit", icon: "doc" },
];

export function liveView(s: TechnicianSnapshot): HTMLElement {
  const host = targetName(s);
  const id = s.console.session ?? s.last?.id ?? "";
  const since = s.last?.since;
  const target = currentTarget(s);
  const simulated = selectedIsSimulated(s.console);
  return h(
    "main",
    { class: "live-screen", id: "main", tabindex: -1, "data-key": "main-live" },
    h(
      "div",
      { class: "band full slim", id: "control-band" },
      h("span", { class: "pulse", "aria-hidden": "true" }),
      h("span", null, "FULL CONTROL"),
      h("span", null, `${host} · session ${shortId(id)} · since ${formatTime(since)}`),
      h("span", { class: "band-note" }, "Either side can stop at any time"),
    ),
    h(
      "div",
      { class: "sessbar" },
      h("div", { class: "iconbox" }, icon("desktop")),
      h(
        "div",
        { class: "sess-id" },
        h("h2", { class: "host" }, host),
        h("div", { class: "meta" }, identityLine(s)),
      ),
      target?.simulated || simulated ? pill("wait", "Simulated target") : null,
      h("span", { class: "spacer" }),
      simulated
        ? button("Simulate target lost", "lost", {
            variant: "ghost sm",
            disabled: s.busy === "lost",
          })
        : null,
      button("Stop session", "stop", {
        variant: "danger",
        id: "stop",
        icon: "stop",
        kbd: "Esc",
        keys: "Escape",
        label: `Stop the session with ${host}`,
        disabled: s.busy === "stop",
      }),
    ),
    h(
      "div",
      { class: "t-live" },
      h(
        "div",
        { class: "workpane" },
        h(
          "div",
          { class: "tabs", role: "tablist", "aria-label": "Session views" },
          TABS.map((t) =>
            h(
              "button",
              {
                type: "button",
                role: "tab",
                id: `tab-${t.tab}`,
                class: t.tab === s.tab ? "on" : null,
                "aria-selected": t.tab === s.tab ? "true" : "false",
                "aria-controls": `panel-${t.tab}`,
                "data-action": "tab",
                "data-arg": t.tab,
              },
              icon(t.icon),
              t.label,
            ),
          ),
        ),
        panel("overview", s, overviewPanel(s)),
        panel("terminal", s, terminalPanel(s)),
        panel("files", s, filesPanel(s)),
        panel("audit", s, auditPanel(s)),
        statusBar(s),
      ),
      rail(s),
    ),
  );
}

function panel(tab: LiveTab, s: TechnicianSnapshot, content: HTMLElement): HTMLElement {
  return h(
    "section",
    {
      class: `tabpanel panel-${tab}`,
      role: "tabpanel",
      id: `panel-${tab}`,
      "aria-labelledby": `tab-${tab}`,
      hidden: tab !== s.tab,
    },
    content,
  );
}

/** Hostname, Windows version, architecture, account, elevation, agent. */
export function identityLine(s: TechnicianSnapshot): string {
  const target = currentTarget(s);
  if (!target) return "Identity not reported";
  return [
    target.os_version,
    target.arch,
    target.account,
    target.elevated ? "elevated" : "not elevated",
    `agent ${target.agent_version}`,
  ]
    .filter((part) => part !== undefined && part !== "")
    .join(" · ");
}

function overviewPanel(s: TechnicianSnapshot): HTMLElement {
  const target = currentTarget(s);
  const counts = sessionCounts(s.activity);
  const id = s.console.session ?? s.last?.id ?? "";
  return h(
    "div",
    { class: "pane-body" },
    h(
      "div",
      { class: "stats" },
      h(
        "div",
        null,
        h("b", null, String(counts.total)),
        h(
          "small",
          null,
          `Actions · AI ${counts.ai} · terminal ${counts.terminal} · you ${counts.technician}`,
        ),
      ),
      h(
        "div",
        null,
        h("b", null, `${counts.filesSent} / ${counts.filesReceived}`),
        h("small", null, "Files sent / received"),
      ),
      h(
        "div",
        null,
        h("b", null, String(counts.failed)),
        h("small", null, counts.running > 0 ? `Failed · ${counts.running} running` : "Failed"),
      ),
    ),
    h(
      "div",
      { class: "card" },
      h("h4", null, "Customer computer"),
      target
        ? kv([
            ["Computer", target.hostname],
            ["Windows", target.os_version],
            ["Architecture", target.arch],
            target.account ? ["Account", target.account] : null,
            ["Admin rights", target.elevated ? "Yes (elevated)" : "No"],
            ["Agent", target.agent_version],
            ["Session", shortId(id)],
            ["Since", formatTime(s.last?.since)],
          ])
        : h("p", { class: "muted" }, "The customer computer has not reported its identity."),
    ),
    counts.notShown > 0
      ? h(
          "p",
          { class: "muted" },
          `${plural(counts.notShown, "item")} of this session's activity ${counts.notShown === 1 ? "was" : "were"} not shown.`,
        )
      : null,
  );
}

/** The embedded terminal, titled the way the customer sees it. */
function terminalPanel(s: TechnicianSnapshot): HTMLElement {
  const host = targetName(s);
  const shownAs = `${technicianName(s.console)} · terminal`;
  return h(
    "div",
    { class: "shell" },
    h(
      "div",
      { class: "sh-bar" },
      h("span", { class: "sh-title" }, "PowerShell · ", h("b", null, host)),
      h("span", { class: "sh-sub" }, `shown to the customer as “${shownAs}”`),
      h("span", { class: "spacer" }),
      h("span", { class: "sh-status", "data-keep": "terminal-badge" }),
    ),
    h("div", { class: "sh-body", "data-keep": "terminal" }),
    h(
      "p",
      { class: "sh-legend" },
      "Keystrokes run on the customer computer. Press Esc to stop the session.",
    ),
  );
}

function fileEntries(s: TechnicianSnapshot): ActivityEntry[] {
  return s.activity.filter(
    (e) => e.entry === "item" && (e.item.kind === "file_sent" || e.item.kind === "file_received"),
  );
}

function filesPanel(s: TechnicianSnapshot): HTMLElement {
  const files = fileEntries(s);
  if (files.length === 0) {
    return h(
      "div",
      { class: "pane-body" },
      h("p", { class: "muted" }, "No files were sent or received in this session."),
    );
  }
  const rows = h("ul", { class: "list files" });
  for (let i = files.length - 1; i >= 0; i -= 1) {
    const entry = files[i];
    if (!entry || entry.entry !== "item") continue;
    const item = entry.item;
    const sent = item.kind === "file_sent";
    rows.append(
      h(
        "li",
        { class: sent ? "it out" : "it", "data-key": `f-${item.id}` },
        h("span", { class: sent ? "k out" : "k" }, icon(sent ? "out" : "in")),
        h(
          "div",
          { class: "file-text" },
          h("b", { class: "mono" }, item.path ?? item.operation),
          h(
            "div",
            { class: "m" },
            sent
              ? h("span", { class: "flag" }, "Sent · left the customer computer")
              : h("span", { class: "who sys" }, "Received · arrived on the customer computer"),
            item.bytes !== undefined ? h("span", null, formatBytes(item.bytes)) : null,
            whoChip(item),
            resultBadge(item, s.now),
          ),
        ),
      ),
    );
  }
  return h("div", { class: "pane-body" }, rows);
}

// This session's audit records (ST-M009-008): every record of its audit log,
// pairing, consent, cleanup and end included, newest first, each as one
// secret-free line the backend built. Written as text only.
function auditPanel(s: TechnicianSnapshot): HTMLElement {
  const id = s.console.session ?? s.last?.id;
  const entries = s.audit;
  const table = h(
    "table",
    { class: "audit" },
    h("caption", { class: "sr-only" }, "This session's audit entries, newest first"),
    h(
      "thead",
      null,
      h(
        "tr",
        null,
        h("th", { scope: "col" }, "Time"),
        h("th", { scope: "col" }, "Who"),
        h("th", { scope: "col" }, "Record"),
        h("th", { scope: "col" }, "Details"),
        h("th", { scope: "col" }, "Result"),
      ),
    ),
  );
  const body = h("tbody");
  for (let i = entries.length - 1; i >= 0; i -= 1) {
    const entry = entries[i];
    if (!entry) continue;
    body.append(
      h(
        "tr",
        { "data-key": `a-${entry.seq}`, class: entry.outcome === "ok" ? undefined : "not-ok" },
        h("td", { class: "mono" }, formatClock(entry.at)),
        h("td", null, auditActorWords(entry.actor)),
        h("td", null, auditActionWords(entry.action)),
        h("td", { class: "mono op" }, entry.summary),
        h("td", null, auditOutcomeWords(entry.outcome)),
      ),
    );
  }
  table.append(body);
  return h(
    "div",
    { class: "pane-body" },
    h(
      "div",
      { class: "row" },
      h(
        "p",
        { class: "muted" },
        `${plural(entries.length, "entry", "entries")} in this session's audit log.`,
      ),
      h("span", { class: "spacer" }),
      button("Open audit log", "open-audit", { variant: "sm", icon: "doc", arg: id }),
    ),
    entries.length > 0
      ? h("div", { class: "audit-wrap" }, table)
      : h("p", { class: "muted" }, "Nothing has been recorded in this session yet."),
  );
}

function statusBar(s: TechnicianSnapshot): HTMLElement {
  const counts = sessionCounts(s.activity);
  return h(
    "div",
    { class: "statusbar" },
    h("span", null, icon("lock", "sm"), "Encrypted · code compared"),
    h("span", null, icon("doc", "sm"), `Audit: ${plural(s.audit.length, "entry", "entries")}`),
    h(
      "span",
      null,
      icon("folder", "sm"),
      `Files: ${counts.filesSent} sent · ${counts.filesReceived} received`,
    ),
  );
}

// The local MCP bridge (ST-M009-008): the AI clients that called over local
// IPC this session, by label, and the number of remote tools on offer, from
// `get_bridge_status`. An endpoint that could not start is an assertive alert
// with the next step; until the backend has said, the clients seen in the
// feed are shown and the tool count is left out rather than guessed.
function railAi(s: TechnicianSnapshot): HTMLElement {
  const bridge = s.bridge;
  const clients = bridge ? bridge.clients : sessionCounts(s.activity).clients;
  const host = targetName(s);
  const unavailable = bridge?.endpoint === "unavailable";
  return h(
    "section",
    { class: "rail-ai" },
    h("h3", { class: "rail-h" }, clients.length === 1 ? "AI assistant" : "AI assistants"),
    unavailable
      ? h(
          "div",
          { class: "alert rail-alert", role: "alert", "data-key": "bridge-alert" },
          icon("alert"),
          h(
            "div",
            { class: "alert-body" },
            h("b", null, "AI assistants cannot connect"),
            h(
              "p",
              null,
              bridge?.message ?? "The local connection for AI assistants is not running.",
            ),
          ),
        )
      : null,
    clients.length === 0
      ? h(
          "p",
          { class: "muted" },
          unavailable
            ? "No AI assistant has called in this session."
            : "No AI assistant has acted in this session yet.",
        )
      : h(
          "ul",
          { class: "list" },
          clients.map((label) =>
            h(
              "li",
              { class: "it", "data-key": `rc-${label}` },
              h("span", { class: "dot", "aria-hidden": "true" }),
              h("b", null, label),
              h("span", { class: "spacer" }),
              h("span", { class: "muted" }, `bound to ${host}`),
            ),
          ),
        ),
    bridge && !unavailable
      ? h(
          "p",
          { class: "muted rail-tools" },
          `${plural(bridge.tools, "remote tool", "remote tools")} available`,
        )
      : bridge
        ? null
        : h("p", { class: "muted rail-tools" }, "Checking the connection for AI assistants…"),
  );
}

function rail(s: TechnicianSnapshot): HTMLElement {
  const counts = sessionCounts(s.activity);
  return h(
    "aside",
    { class: "rail", "aria-label": "AI assistants and activity" },
    railAi(s),
    h(
      "section",
      { class: "rail-feed-head" },
      h("h3", { class: "rail-h" }, "Activity"),
      counts.running > 0 ? h("span", { class: "muted" }, `${counts.running} running`) : null,
    ),
    feedList(s.activity, s.now, {
      label: "Activity of this session, newest first",
      empty:
        "Nothing has happened yet. Actions by the AI assistant, the terminal and you appear here.",
    }),
  );
}
