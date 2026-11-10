// Home (`#t-home`, ST-M009-008): "Ready to help" with Start a session as the
// only primary action, the three most recent sessions and the AI clients. It
// lists no discovered computers: the technician never picks a target
// (REQ-F-004).

import type { AiClientStatus, SessionSummary } from "../../shared/contract";
import { h } from "../../shared/dom";
import { icon } from "../../shared/icons";
import { type TechnicianSnapshot, endedByWords, formatDay, formatDuration, plural } from "../model";
import { button, card, cleanupPill, pill, unavailable } from "./common";

export function homeView(s: TechnicianSnapshot): HTMLElement {
  return h(
    "div",
    { class: "screen home" },
    h(
      "div",
      { class: "card hero" },
      h("div", { class: "iconbox big" }, icon("tool", "xl")),
      h(
        "div",
        { class: "hero-text" },
        h("h2", null, "Ready to help"),
        h(
          "p",
          null,
          "Start a session, then ask the customer to open CretLink and choose ",
          h("b", null, "Get help"),
          ".",
        ),
      ),
      button("Start a session", "start", {
        variant: "primary big",
        id: "start-session",
        disabled: s.busy === "start",
        autofocus: true,
      }),
    ),
    h(
      "div",
      { class: "grid2" },
      card(
        h(
          "div",
          { class: "card-head" },
          h("h4", null, "Recent sessions"),
          s.sessions && s.sessions.length > 3
            ? h(
                "button",
                { type: "button", class: "linkbtn", "data-action": "nav", "data-arg": "sessions" },
                "All sessions",
              )
            : null,
        ),
        recentSessions(s),
      ),
      card(h("h4", null, "AI assistants"), clientsSummary(s.clients)),
    ),
  );
}

function recentSessions(s: TechnicianSnapshot): HTMLElement {
  if (s.sessions === null) return unavailable("The session history");
  if (s.sessions.length === 0) return h("p", { class: "muted" }, "No sessions yet.");
  return sessionList(s.sessions.slice(0, 3), s.now);
}

/** Sessions, each opening its end report. */
export function sessionList(sessions: readonly SessionSummary[], now: number): HTMLElement {
  return h(
    "ul",
    { class: "list sessions" },
    sessions.map((summary) =>
      h(
        "li",
        { class: "it", "data-key": `s-${summary.id}` },
        h(
          "button",
          {
            type: "button",
            class: "session-row",
            "data-action": "open-report",
            "data-arg": summary.id,
          },
          h("span", { class: `dot ${dotFor(summary)}`, "aria-hidden": "true" }),
          h(
            "span",
            { class: "session-text" },
            h("b", null, summary.hostname ?? "Unknown computer"),
            h(
              "span",
              { class: "muted" },
              [
                formatDay(summary.startedAt ?? summary.endedAt, now),
                formatDuration(summary.durationMs),
                plural(
                  summary.actions.ai + summary.actions.terminal + summary.actions.technician,
                  "action",
                ),
                endedByWords(summary.endedBy),
              ].join(" · "),
            ),
          ),
          h(
            "span",
            { class: "session-pills" },
            summary.integrity === "broken"
              ? pill("bad", "Audit file broken", "alert")
              : summary.integrity === "unsealed"
                ? pill("wait", "Not sealed", "alert")
                : null,
            cleanupPill(summary.cleanup),
          ),
        ),
      ),
    ),
  );
}

function dotFor(summary: SessionSummary): string {
  if (summary.integrity === "broken" || summary.cleanup.state === "failed") return "bad";
  if (summary.cleanup.state === "cleaned_up" && summary.integrity === "verified") return "";
  return "warn";
}

/** Each client's status in words, with Connect when present but not registered. */
function clientsSummary(clients: AiClientStatus[] | null): HTMLElement {
  if (clients === null) return unavailable("The AI assistant status");
  if (clients.length === 0) return h("p", { class: "muted" }, "No AI assistants are known.");
  return h(
    "ul",
    { class: "list" },
    clients.map((client) => {
      const status = clientStatusWords(client);
      return h(
        "li",
        { class: "it", "data-key": `c-${client.id}` },
        h("span", { class: `dot ${status.dot}`, "aria-hidden": "true" }),
        h("span", null, client.label),
        h("span", { class: "spacer" }),
        client.present && client.registration.state === "absent"
          ? button("Connect", "ai-connect", {
              arg: client.id,
              variant: "sm",
              label: `Connect ${client.label}`,
            })
          : h("span", { class: "muted" }, status.text),
      );
    }),
  );
}

export function clientStatusWords(client: AiClientStatus): { text: string; dot: string } {
  switch (client.registration.state) {
    case "this_executable":
      return {
        text: client.present ? "Connected" : "Connected · not found on PATH",
        dot: client.present ? "" : "warn",
      };
    case "other_command":
      return { text: "Registered to another program", dot: "warn" };
    case "unreadable":
      return { text: "Configuration can't be read", dot: "bad" };
    case "absent":
      return client.present
        ? { text: "Not connected", dot: "off" }
        : { text: "Not found", dot: "off" };
  }
}
