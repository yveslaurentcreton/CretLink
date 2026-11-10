// The compact bar (`#c-mini`, ST-M009-009, REQ-S-002, REQ-F-018): a small,
// always-on-top window that shows the state, what is happening right now, the
// counts, Stop and Open. It is moved by dragging the bar; it cannot be closed
// while a session exists (the backend refuses).

import { h } from "../../shared/dom";
import { plural } from "../../shared/feed";
import { icon } from "../../shared/icons";
import {
  type CustomerSnapshot,
  announcement,
  bandFor,
  capName,
  happeningNow,
  liveCounts,
  plainLine,
  screenFor,
  screenShort,
  screenUse,
  technicianName,
} from "../model";
import { button } from "./common";

function title(s: CustomerSnapshot): string {
  const name = technicianName(s);
  switch (screenFor(s)) {
    case "live":
      return `${capName(name)} has full control`;
    case "consent":
      return "Your decision is waiting";
    case "compare":
    case "connecting":
      return `Connected to ${name} · no access`;
    default:
      return "No access";
  }
}

export function renderMini(s: CustomerSnapshot): HTMLElement {
  const screen = screenFor(s);
  const band = bandFor(s, screen);
  const counts = liveCounts(s.activity);
  const use = screenUse(s.view, s.now);
  const item = happeningNow(s.activity);
  const full = screen === "live";
  const detail = full
    ? [
        screenShort(use),
        plural(counts.commands, "command"),
        `${plural(counts.filesSent, "file")} sent`,
      ].join(" · ")
    : band.explanation;
  return h(
    "div",
    { class: `cl-ui cust mini ${band.kind}`, "data-screen": `mini-${screen}` },
    h(
      "div",
      { class: "minibar", "data-drag": "true", role: "group", "aria-label": band.title },
      full ? h("span", { class: "pulse", "aria-hidden": "true" }) : h("span", { class: "g", "aria-hidden": "true" }, band.glyph),
      h(
        "div",
        { class: "mini-text" },
        h("span", { class: "sr-only" }, `${band.title}. `),
        h("b", null, title(s)),
        h("small", null, full ? icon(use.state === "controlled" ? "cursor" : "eye", "sm") : null, detail),
      ),
      s.view?.session_active
        ? button("Stop", "stop", {
            id: "stop",
            icon: "stop",
            keys: "Escape",
            label: "Stop the session now",
            disabled: s.busy === "stop",
          })
        : null,
    ),
    h(
      "div",
      { class: "mininow" },
      item
        ? [
            h("span", { class: "spinner xs", "aria-hidden": "true" }),
            h("b", null, plainLine(item)),
            h("code", { title: item.operation }, h("span", { class: "sr-only" }, "Operation: "), item.operation),
          ]
        : h("span", { class: "muted" }, full ? "Nothing is running" : announcement(s)),
      h("span", { class: "spacer" }),
      h(
        "button",
        { type: "button", class: "linkbtn", "data-action": "open-main", "aria-label": "Open the CretLink window" },
        "Open",
      ),
    ),
    h("div", { class: "sr-only", role: "status", "aria-live": "polite", id: "announce" }, announcement(s)),
  );
}
