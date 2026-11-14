// Session ended (`#c-end`, ST-M009-009, ST-M009-012, REQ-F-019, REQ-F-027).
//
// The heading says who ended the session; then the counts, each cleanup step
// from the agent's own report with failures first (unknown is never shown as
// success, REQ-N-015), **Save a copy** and **Close CretLink**. Nothing is
// written to disk unless the customer saves a copy.

import type { CleanupNote, CleanupResult, CustomerReport, EndedBy } from "../../shared/contract";
import { h } from "../../shared/dom";
import { plural } from "../../shared/feed";
import { icon } from "../../shared/icons";
import { viaRelay } from "../../shared/relay";
import {
  type CustomerSnapshot,
  capName,
  liveCounts,
  relayLostWords,
  relayNamed,
  sessionRelay,
  technicianName,
} from "../model";
import { button } from "./common";
import { keepCardView } from "./keep";

export function endHeading(endedBy: EndedBy | undefined, name: string): string {
  switch (endedBy) {
    case "customer":
      return "You stopped the session";
    case "technician":
      return `${capName(name)} ended the session`;
    case "connection_lost":
      return "The connection was lost";
    case "failure":
      return "The session ended because of a problem";
    default:
      return "The session has ended";
  }
}

function stats(s: CustomerSnapshot, report: CustomerReport | null): HTMLElement {
  const counts = report?.counts ?? liveCounts(s.activity);
  return h(
    "div",
    { class: "stats", role: "group", "aria-label": "What happened" },
    h("div", null, h("b", null, String(counts.commands)), h("small", null, counts.failedCommands > 0 ? `commands · ${counts.failedCommands} failed` : "commands")),
    h(
      "div",
      null,
      h("b", null, `${counts.filesSent} · ${counts.filesReceived}`),
      h("small", null, "files sent · received"),
    ),
    h("div", null, h("b", null, String(counts.screenshots)), h("small", null, "screenshots")),
  );
}

function headline(cleanup: CleanupResult): HTMLElement {
  switch (cleanup.state) {
    case "cleaned_up":
      return h(
        "div",
        { class: "okline" },
        h("span", { class: "i" }, icon("check")),
        h("div", null, h("b", null, "Cleaned up. "), "Nothing of this session is left on this computer."),
      );
    case "residuals":
      return h(
        "div",
        { class: "okline" },
        h("span", { class: "i w" }, icon("alert")),
        h(
          "div",
          null,
          h("b", null, `${plural(cleanup.residuals, "thing")} stayed on this computer. `),
          "Each is listed below. Ask your technician about it, or keep the report.",
        ),
      );
    case "failed":
      return h(
        "div",
        { class: "okline" },
        h("span", { class: "i b" }, icon("x")),
        h(
          "div",
          null,
          h("b", null, "Cleanup failed. "),
          "Some temporary files or helpers may still be on this computer, listed below. Save a copy of this report and show it to your technician.",
        ),
      );
    case "unknown":
      return h(
        "div",
        { class: "okline" },
        h("span", { class: "i w" }, icon("alert")),
        h(
          "div",
          null,
          h("b", null, "Cleanup not confirmed. "),
          "No cleanup report arrived, so it is not known what was removed. Don't assume this computer is clean.",
        ),
      );
  }
}

function noteLine(note: CleanupNote): HTMLElement {
  return h(
    "div",
    { class: "okline" },
    h("span", { class: note.ok ? "i" : "i w" }, icon(note.ok ? "check" : "alert")),
    h("div", null, h("span", { class: "sr-only" }, note.ok ? "Done: " : "Not done: "), note.text),
  );
}

function cleanup(report: CustomerReport | null): HTMLElement {
  if (!report) {
    return h(
      "section",
      { class: "card flat cleanup", "aria-labelledby": "cleanup-h", "aria-busy": "true" },
      h("h4", { id: "cleanup-h" }, "Cleaned up"),
      h(
        "div",
        { class: "okline" },
        h("span", { class: "spinner", "aria-hidden": "true" }),
        h("div", null, h("b", null, "Cleaning up… "), "This computer is stopping what ran and removing temporary files."),
      ),
    );
  }
  // Failures and residuals first.
  const notes = [...report.cleanup.notes].sort((a, b) => Number(a.ok) - Number(b.ok));
  return h(
    "section",
    { class: "card flat cleanup", "aria-labelledby": "cleanup-h" },
    h("h4", { id: "cleanup-h" }, "Cleaned up"),
    headline(report.cleanup),
    notes.map(noteLine),
  );
}

function record(s: CustomerSnapshot, report: CustomerReport | null): HTMLElement {
  return h(
    "section",
    { class: "card flat row record", "aria-labelledby": "record-h" },
    icon("doc"),
    h(
      "div",
      { class: "record-text" },
      h("b", { id: "record-h" }, "Record of this session"),
      s.saved
        ? h("div", { class: "muted mono", title: s.saved }, s.saved)
        : h("div", { class: "muted" }, "Nothing is saved unless you choose to. A copy is one web page."),
    ),
    s.saved
      ? button("Open", "open-report", { variant: "sm", arg: s.saved, label: "Open the saved record" })
      : button("Save a copy", "save", {
          variant: "sm",
          disabled: !report || s.busy === "save",
        }),
  );
}

/**
 * A session through a relay whose connection was lost says so with the
 * relay, and that nothing connects again by itself (ST-M007-013).
 */
function relayLost(s: CustomerSnapshot): HTMLElement | null {
  const relay = relayNamed(s);
  const phase = s.view?.status.phase;
  const lost =
    s.report?.endedBy === "connection_lost" ||
    (phase?.phase === "stopped" && phase.reason === "disconnected");
  if (!relay || !lost) return null;
  const words = relayLostWords(relay);
  return h(
    "p",
    { class: "note relay-lost", role: "note", "data-key": "relay-lost" },
    icon("alert"),
    h("span", null, h("b", null, `${words.message} `), words.next),
  );
}

/**
 * A support program that the customer kept is on this computer on purpose:
 * the report says so beside the cleanup, so that it is not taken for
 * something this session left behind (ST-M009-017, REQ-F-019).
 */
function keptOnPurpose(report: CustomerReport | null): HTMLElement | null {
  const kept = report?.keptProgram;
  if (!kept) return null;
  return h(
    "p",
    { class: "note info kept-on-purpose", role: "note", "data-key": "kept-on-purpose" },
    icon("check"),
    h(
      "span",
      null,
      h("b", null, "CretLink Support is installed on this computer because you chose to keep it. "),
      "It was not left behind by this session. It lies in ",
      h("code", { class: "path" }, kept.location),
      kept.version ? `, in version ${kept.version}.` : ".",
    ),
  );
}

export function endView(s: CustomerSnapshot): HTMLElement[] {
  const report = s.report;
  const name = technicianName(s);
  return [
    h(
      "main",
      { class: "cbody", id: "main", tabindex: -1, "data-key": "main-end" },
      h(
        "div",
        null,
        h("h3", { id: "end-title" }, report ? endHeading(report.endedBy, name) : "The session has ended"),
        h(
          "p",
          { class: "lead" },
          `${capName(name)} can no longer reach this computer. A new session needs a new code and your permission again.`,
        ),
        viaRelay(sessionRelay(s)),
      ),
      relayLost(s),
      stats(s, report),
      cleanup(report),
      keptOnPurpose(report),
      record(s, report),
      // Offered again once the session is over, never during it
      // (ST-M009-017).
      keepCardView(s),
    ),
    h(
      "div",
      { class: "cfoot" },
      h("span", { class: "spacer" }),
      button("Close CretLink", "close-app", {
        variant: "primary big",
        autofocus: true,
        disabled: s.closing,
      }),
    ),
  ];
}
