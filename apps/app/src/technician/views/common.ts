// Small building blocks of the technician screens (ST-M009-008). Every string
// becomes a text node through `h()`; nothing is parsed as markup.

import type {
  ActivityEntry,
  ActivityItem,
  ActivityKind,
  CleanupResult,
} from "../../shared/contract";
import { type Child, h } from "../../shared/dom";
import { type IconName, icon } from "../../shared/icons";
import {
  cleanupWords,
  formatBytes,
  formatClock,
  isFailure,
  plural,
  sourceLabel,
  stateWords,
} from "../model";

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
export function button(
  text: Child,
  action: string,
  options: ButtonOptions = {},
): HTMLButtonElement {
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

/** A state pill: the state is always in words, the colour only repeats it. */
export function pill(variant: string, text: string, iconName?: IconName): HTMLElement {
  return h("span", { class: `pill ${variant}` }, iconName ? icon(iconName, "sm") : null, text);
}

export function kv(rows: ReadonlyArray<readonly [string, Child] | null | false>): HTMLElement {
  const list = h("dl", { class: "kv" });
  for (const row of rows) {
    if (!row) continue;
    list.append(h("dt", null, row[0]), h("dd", null, row[1]));
  }
  return list;
}

export function card(...children: Child[]): HTMLElement {
  return h("div", { class: "card" }, ...children);
}

/** A calm "not available yet" line for data the backend does not offer yet. */
export function unavailable(what: string): HTMLElement {
  return h("p", { class: "muted unavailable" }, `${what} is not available yet.`);
}

export function cleanupPill(cleanup: CleanupResult): HTMLElement {
  switch (cleanup.state) {
    case "cleaned_up":
      return pill("ok", cleanupWords(cleanup), "check");
    case "residuals":
      return pill("wait", cleanupWords(cleanup), "alert");
    case "failed":
      return pill("bad", cleanupWords(cleanup), "x");
    case "unknown":
      return pill("wait", cleanupWords(cleanup), "alert");
  }
}

// --- Activity items (ST-M009-011) ---

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

export function whoChip(item: ActivityItem): HTMLElement {
  switch (item.source) {
    case "ai":
      return h("span", { class: "who ai" }, icon("bot"), sourceLabel(item));
    case "terminal":
      return h("span", { class: "who tech" }, icon("term"), sourceLabel(item));
    case "technician":
      return h("span", { class: "who tech" }, icon("user"), sourceLabel(item));
  }
}

export function resultBadge(item: ActivityItem, now: number): HTMLElement {
  const words = stateWords(item, now);
  switch (item.state.state) {
    case "running":
      return h(
        "span",
        { class: "res run" },
        h("span", { class: "spinner xs", "aria-hidden": "true" }),
        words,
      );
    case "done":
      return h("span", { class: "res done" }, icon("check"), words);
    case "failed":
    case "timed_out":
      return h("span", { class: "res fail" }, icon("x"), words);
    case "cancelled":
      return h("span", { class: "res stop" }, icon("x"), words);
  }
}

function itemClass(item: ActivityItem): string {
  if (isFailure(item.state)) return "ev fail";
  if (item.state.state === "running") return "ev run";
  if (item.kind === "file_sent") return "ev out";
  return "ev";
}

/**
 * One feed entry: the stated purpose (or the plain description), who started
 * it, its state, and the real operation beneath (REQ-F-017, REQ-F-026). The
 * purpose is the AI's unverified claim, so it never replaces the operation.
 */
export function activityEntry(entry: ActivityEntry, now: number, keyPrefix = "ev"): HTMLElement {
  if (entry.entry === "gap") {
    return h(
      "li",
      { class: "ev gap", "data-key": `${keyPrefix}-gap-${entry.at}-${entry.count}` },
      h("span", { class: "k" }, icon("alert")),
      h(
        "div",
        null,
        h(
          "div",
          { class: "t" },
          `${plural(entry.count, "item")} ${entry.count === 1 ? "was" : "were"} not shown`,
        ),
        entry.at
          ? h("div", { class: "m" }, h("span", { class: "time" }, formatClock(entry.at)))
          : null,
      ),
    );
  }
  const item = entry.item;
  const title = item.purpose
    ? [h("span", { class: "claim" }, "Stated purpose · "), item.purpose]
    : [item.description];
  const time = formatClock(item.startedAt);
  return h(
    "li",
    { class: itemClass(item), "data-key": `${keyPrefix}-${item.id}` },
    h("span", { class: "k" }, icon(KIND_ICON[item.kind] ?? "term")),
    h(
      "div",
      null,
      h("div", { class: "t" }, title),
      h(
        "div",
        { class: "m" },
        whoChip(item),
        resultBadge(item, now),
        item.kind === "file_sent"
          ? h("span", { class: "flag" }, "left the customer computer")
          : null,
        item.bytes !== undefined ? h("span", null, formatBytes(item.bytes)) : null,
        time ? h("span", { class: "time" }, time) : null,
      ),
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
    ),
  );
}

/** A feed, newest first. */
export function feedList(
  entries: readonly ActivityEntry[],
  now: number,
  options: { label: string; empty: string; keyPrefix?: string; className?: string },
): HTMLElement {
  if (entries.length === 0) {
    return h("p", { class: "muted feed-empty" }, options.empty);
  }
  const list = h("ol", {
    class: options.className ?? "feed",
    "aria-label": options.label,
    tabindex: 0,
  });
  for (let index = entries.length - 1; index >= 0; index -= 1) {
    const entry = entries[index];
    if (entry) list.append(activityEntry(entry, now, options.keyPrefix));
  }
  return list;
}
