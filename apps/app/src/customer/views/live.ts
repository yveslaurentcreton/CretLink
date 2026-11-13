// Under full control (`#c-live`, ST-M009-009, REQ-F-017, REQ-F-018,
// REQ-F-020, REQ-F-026, REQ-S-016).
//
// The band says who can operate this computer and since when. **Happening
// now** shows the running action with its purpose or plain description, its
// source and elapsed time, and the real operation beneath. Tiles count the
// screen use, files and commands. **What happened** lists every item newest
// first with a filter; a file that left this computer says so in words.
// **Stop now** (Escape) is a native button that is always on screen. There
// is no reboot-resume control (FND-0005).

import { h } from "../../shared/dom";
import { formatElapsed, formatTime, plural } from "../../shared/feed";
import { type IconName, icon } from "../../shared/icons";
import {
  type CustomerSnapshot,
  FILTERS,
  bandElapsed,
  capName,
  filtered,
  happeningNow,
  liveCounts,
  plainLine,
  relayHost,
  screenUse,
  screenWords,
  technicianName,
} from "../model";
import { button, operationLine, timelineItem, viaRelay, whoChip } from "./common";

/** "since 14:02" and the elapsed time, for the band. */
export function since(s: CustomerSnapshot): HTMLElement[] | null {
  const start = s.view?.control_since_ms;
  if (start === null || start === undefined) return null;
  return [
    h("span", null, `since ${formatTime(start)}`),
    h("br"),
    h("span", null, bandElapsed(Math.max(0, s.now - start))),
  ];
}

function now(s: CustomerSnapshot, name: string): HTMLElement {
  const item = happeningNow(s.activity);
  if (!item) {
    return h(
      "section",
      { class: "now idle", "aria-labelledby": "now-h" },
      h("div", { class: "h", id: "now-h" }, "Happening now"),
      h("div", { class: "t" }, "Nothing is running"),
      h("p", { class: "muted" }, `${capName(name)} has full control, but no action is running right now.`),
    );
  }
  const started = Date.parse(item.startedAt);
  return h(
    "section",
    { class: "now", "aria-labelledby": "now-h", "data-key": `now-${item.id}` },
    h(
      "div",
      { class: "h", id: "now-h" },
      h("span", { class: "pulse", "aria-hidden": "true" }),
      "Happening now",
      h(
        "span",
        { class: "elapsed" },
        Number.isNaN(started) ? "" : formatElapsed(Math.max(0, s.now - started)),
      ),
    ),
    h("div", { class: "t" }, plainLine(item)),
    h(
      "div",
      { class: "row m" },
      whoChip(item, name),
      item.purpose ? h("span", { class: "claim" }, "purpose as stated") : null,
      item.kind === "file_sent" ? h("span", { class: "flag" }, "leaving this computer") : null,
    ),
    operationLine(item),
  );
}

function tile(iconName: IconName, title: string, detail: string, hot: boolean): HTMLElement {
  return h(
    "div",
    { class: hot ? "tile hot" : "tile" },
    icon(iconName),
    h("div", null, h("b", null, title), h("small", null, detail)),
  );
}

function tiles(s: CustomerSnapshot): HTMLElement {
  const counts = liveCounts(s.activity);
  const use = screenUse(s.view, s.now);
  const active = use.state === "observed" || use.state === "controlled";
  return h(
    "div",
    { class: "tiles", role: "group", "aria-label": "This session so far" },
    tile(
      use.state === "controlled" ? "cursor" : "eye",
      "Screen",
      screenWords(use, (at) => formatTime(at)),
      active,
    ),
    tile(
      "out",
      "Files",
      `${counts.filesSent} sent · ${counts.filesReceived} received`,
      counts.filesSent > 0,
    ),
    tile(
      "term",
      plural(counts.commands, "command"),
      `${counts.failedCommands} failed`,
      false,
    ),
  );
}

function filterBar(s: CustomerSnapshot): HTMLElement {
  return h(
    "div",
    { class: "seg", role: "group", "aria-label": "Show" },
    FILTERS.map(({ filter, label }) =>
      h(
        "button",
        {
          type: "button",
          class: filter === s.filter ? "on" : null,
          "aria-pressed": filter === s.filter ? "true" : "false",
          "data-action": "filter",
          "data-arg": filter,
        },
        label,
      ),
    ),
  );
}

function grantItem(s: CustomerSnapshot): HTMLElement | null {
  const at = s.view?.control_since_ms;
  if (at === null || at === undefined || s.filter !== "all") return null;
  return h(
    "li",
    { class: "ti grant", "data-key": "tl-grant" },
    h("span", { class: "k" }, icon("shield")),
    h(
      "div",
      { class: "ti-body" },
      h("div", { class: "t" }, "You allowed full control"),
      h("div", { class: "m" }, h("span", { class: "who cust" }, icon("user"), "You"), "after comparing the code"),
    ),
    h("span", { class: "time" }, formatTime(at)),
  );
}

function timeline(s: CustomerSnapshot, name: string): HTMLElement {
  // The item under Happening now is not repeated here until it finishes.
  const current = happeningNow(s.activity)?.id;
  const entries = filtered(s.activity, s.filter).filter(
    (e) => e.entry === "gap" || e.item.id !== current,
  );
  const list = h("ol", {
    class: "tl",
    "aria-label": "What happened, newest first",
    tabindex: 0,
  });
  for (let i = entries.length - 1; i >= 0; i -= 1) {
    const entry = entries[i];
    if (entry) list.append(timelineItem(entry, name, s.now));
  }
  const grant = grantItem(s);
  if (grant) list.append(grant);
  if (list.childElementCount === 0) {
    list.append(
      h(
        "li",
        { class: "ti empty", "data-key": "tl-empty" },
        h("span", { class: "k" }, icon("check")),
        h("div", { class: "t muted" }, s.filter === "all" ? "Nothing has happened yet." : "Nothing of this kind yet."),
      ),
    );
  }
  return list;
}

export function liveView(s: CustomerSnapshot): HTMLElement[] {
  const name = technicianName(s);
  return [
    h(
      "main",
      { class: "cbody live", id: "main", tabindex: -1, "data-key": "main-live" },
      s.view?.simulated
        ? h(
            "p",
            { class: "note dev", role: "note" },
            icon("alert"),
            h("span", null, h("b", null, "Simulated demo. "), "No technician is connected and nobody controls this computer."),
            button("Simulate technician stop", "simulate-stop", { variant: "sm" }),
          )
        : null,
      now(s, name),
      tiles(s),
      viaRelay(relayHost(s)),
      h("div", { class: "tlhead" }, h("h4", { id: "tl-h" }, "What happened"), filterBar(s)),
      timeline(s, name),
      h(
        "div",
        { class: "row" },
        h("span", { class: "spacer" }),
        button("Make small", "make-small", {
          variant: "ghost sm",
          icon: "minimize",
          disabled: s.busy === "make-small",
        }),
      ),
    ),
    h(
      "div",
      { class: "cfoot stopfoot" },
      h(
        "button",
        {
          type: "button",
          class: "stopbig",
          id: "stop",
          "data-action": "stop",
          "aria-keyshortcuts": "Escape",
          "aria-describedby": "stop-note",
          disabled: s.busy === "stop",
        },
        icon("stop"),
        "Stop now",
        h("kbd", { "aria-hidden": "true" }, "Esc"),
      ),
      h(
        "span",
        { class: "muted", id: "stop-note" },
        `Control ends immediately. You don't need ${name} for this.`,
      ),
    ),
  ];
}
