// Sessions (ST-M009-008): every session within the audit retention, newest
// first (ST-M009-012); each opens its end report.

import { h } from "../../shared/dom";
import type { TechnicianSnapshot } from "../model";
import { unavailable } from "./common";
import { sessionList } from "./home";

export function sessionsView(s: TechnicianSnapshot): HTMLElement {
  return h(
    "div",
    { class: "screen sessions-page" },
    h(
      "div",
      null,
      h("h2", null, "Sessions"),
      h(
        "p",
        null,
        "Every session kept in the audit folder, newest first. Open one to see its report.",
      ),
    ),
    h(
      "div",
      { class: "card list-card" },
      s.sessions === null
        ? unavailable("The session history")
        : s.sessions.length === 0
          ? h(
              "p",
              { class: "muted" },
              "No sessions yet. They appear here once a session has ended.",
            )
          : sessionList(s.sessions, s.now),
    ),
  );
}
