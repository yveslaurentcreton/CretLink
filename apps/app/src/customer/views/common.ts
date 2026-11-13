// Building blocks of the customer screens (ST-M009-009). Every string becomes
// a text node through `h()`; nothing is parsed as markup, so an operation,
// path, name or purpose from the technician's side stays text (REQ-S-018).

import type { ActivityEntry, ActivityItem, ActivityKind } from "../../shared/contract";
import { type Child, h } from "../../shared/dom";
import { formatBytes, formatClock, isFailure, plural } from "../../shared/feed";
import { type IconName, icon } from "../../shared/icons";
import { type Band, outcomeWords, plainLine, sourceWords } from "../model";

export interface ButtonOptions {
  arg?: string | undefined;
  variant?: string;
  id?: string;
  disabled?: boolean;
  label?: string;
  keys?: string;
  icon?: IconName;
  kbd?: string;
  autofocus?: boolean;
}

/** A native button that dispatches `action` (and `arg`) when pressed. */
export function button(text: Child, action: string, options: ButtonOptions = {}): HTMLButtonElement {
  return h(
    "button",
    {
      type: "button",
      class: options.variant ? `btn ${options.variant}` : "btn",
      id: options.id,
      "data-action": action,
      "data-arg": options.arg,
      "data-autofocus": options.autofocus,
      disabled: options.disabled,
      "aria-label": options.label,
      "aria-keyshortcuts": options.keys,
    },
    options.icon ? icon(options.icon, "sm") : null,
    text,
    options.kbd ? h("kbd", { "aria-hidden": "true" }, options.kbd) : null,
  );
}

/** The state band that starts every screen: the state in words, a short
 * explanation, and on the live screen when control began. */
export function band(b: Band, since?: Child): HTMLElement {
  return h(
    "div",
    { class: `band ${b.kind}`, id: "state-band", role: "group", "aria-label": "Access" },
    h("span", { class: "g", "aria-hidden": "true" }, b.glyph),
    h("div", null, h("b", null, b.title.toUpperCase()), h("small", null, b.explanation)),
    since ? h("div", { class: "since" }, since) : null,
  );
}

/**
 * That the session runs through a relay, and through which: on every screen
 * from the comparison of the code to the end (ST-M007-013). The relay passes
 * the encrypted session on; it cannot look inside.
 */
export function viaRelay(host: string | undefined): HTMLElement | null {
  if (!host) return null;
  return h(
    "p",
    { class: "via-relay", "data-key": "via-relay" },
    icon("wifi", "sm"),
    h("span", null, "Through the relay ", h("b", { class: "mono" }, host)),
    h("span", { class: "muted" }, " · it only passes encrypted traffic on"),
  );
}

/** The footer of the screens before a session. */
export function nothingInstalled(): HTMLElement {
  return h(
    "div",
    { class: "cfoot" },
    icon("check", "sm"),
    h("span", { class: "muted" }, "Nothing is installed · closing CretLink ends everything"),
  );
}

const KIND_ICON: Record<ActivityKind, IconName> = {
  command: "term",
  file_sent: "out",
  file_received: "in",
  file_operation: "folder",
  screen_observed: "eye",
  screen_controlled: "cursor",
  clipboard_read: "doc",
  clipboard_written: "doc",
  system_information: "gauge",
  job_control: "restart",
};

export function kindIcon(item: ActivityItem): SVGSVGElement {
  return icon(isFailure(item.state) ? "x" : (KIND_ICON[item.kind] ?? "term"));
}

/** Who started an action: "<name>'s AI assistant", "<name> · terminal",
 * "<name>". */
export function whoChip(item: ActivityItem, name: string): HTMLElement {
  const words = sourceWords(item, name);
  switch (item.source) {
    case "ai":
      return h("span", { class: "who ai" }, icon("bot"), words);
    case "terminal":
      return h("span", { class: "who tech" }, icon("term"), words);
    case "technician":
      return h("span", { class: "who tech" }, icon("user"), words);
  }
}

function itemClass(item: ActivityItem): string {
  if (isFailure(item.state)) return "ti fail";
  if (item.kind === "file_sent") return "ti out";
  if (item.state.state === "running") return "ti run";
  return "ti";
}

/**
 * One timeline item: its plain line (the AI's stated purpose, else the plain
 * description), source, outcome in words and time, with the real operation
 * or path beneath. A file that left this computer says so in words, as well
 * as being amber (REQ-S-016, REQ-F-026).
 */
export function timelineItem(entry: ActivityEntry, name: string, now: number): HTMLElement {
  if (entry.entry === "gap") {
    return h(
      "li",
      { class: "ti gap", "data-key": `tl-gap-${entry.at}-${entry.count}` },
      h("span", { class: "k" }, icon("alert")),
      h(
        "div",
        null,
        h(
          "div",
          { class: "t" },
          `${plural(entry.count, "item")} ${entry.count === 1 ? "was" : "were"} not shown`,
        ),
        h("div", { class: "m" }, "Too much happened at once to show every item here."),
      ),
      h("span", { class: "time" }, formatClock(entry.at)),
    );
  }
  const item = entry.item;
  const sent = item.kind === "file_sent";
  return h(
    "li",
    { class: itemClass(item), "data-key": `tl-${item.id}` },
    h("span", { class: "k" }, kindIcon(item)),
    h(
      "div",
      { class: "ti-body" },
      h("div", { class: "t" }, plainLine(item)),
      h(
        "div",
        { class: "m" },
        whoChip(item, name),
        item.purpose ? h("span", { class: "claim" }, "purpose as stated") : null,
        sent ? h("span", { class: "flag" }, "left this computer") : null,
        h("span", { class: `res ${resultClass(item)}` }, outcomeWords(item, now)),
        item.bytes !== undefined ? h("span", null, formatBytes(item.bytes)) : null,
      ),
      operationLine(item),
    ),
    h("span", { class: "time" }, formatClock(item.startedAt)),
  );
}

function resultClass(item: ActivityItem): string {
  if (isFailure(item.state)) return "fail";
  switch (item.state.state) {
    case "running":
      return "run";
    case "done":
      return "done";
    default:
      return "stop";
  }
}

/** The real operation, and the path when the operation does not name it. */
export function operationLine(item: ActivityItem): Child[] {
  return [
    h(
      "code",
      { title: item.operation },
      h("span", { class: "sr-only" }, "Operation: "),
      item.operation,
    ),
    item.path && !item.operation.includes(item.path)
      ? h(
          "code",
          { class: "path", title: item.path },
          h("span", { class: "sr-only" }, "Path: "),
          item.path,
        )
      : null,
  ];
}
