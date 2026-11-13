// End reports (`#t-end`, ST-M009-008, ST-M009-012): who ended the session,
// when and how long it lasted; the cleanup result with failures and residuals
// first and a safe next step; what happened; the audit log. Unknown cleanup is
// never shown as success (REQ-N-015).

import type { CleanupNote, CleanupResult, EndedBy, SessionSummary } from "../../shared/contract";
import { type Child, h } from "../../shared/dom";
import { icon } from "../../shared/icons";
import { type NamedRelay, relayLabel, relayWords, trustFact } from "../../shared/relay";
import {
  type TechnicianSnapshot,
  endedByWords,
  formatDuration,
  formatTime,
  formatWhen,
  plural,
  relayLostWords,
  sessionCounts,
  shortId,
  stopReasonEndedBy,
  summaryFor,
  targetName,
} from "../model";
import { button, card, cleanupPill, kv, pill } from "./common";

/** The end of the session that just finished; Done returns Home. */
export function endView(s: TechnicianSnapshot): HTMLElement {
  const last = s.last;
  const summary = summaryFor(s, last?.id);
  const body = summary ? reportBody(summary, "end", s.now) : preparingReport(s);
  return h(
    "div",
    { class: "screen end" },
    body,
    h(
      "div",
      { class: "row" },
      h("span", { class: "spacer" }),
      button("Done", "done", { variant: "primary", autofocus: true }),
    ),
  );
}

/** A report opened from the Sessions page. */
export function reportPage(s: TechnicianSnapshot): HTMLElement {
  const summary = summaryFor(s, s.openReport);
  return h(
    "div",
    { class: "screen report" },
    h(
      "div",
      { class: "row" },
      button("Back to sessions", "close-report", { variant: "ghost sm", autofocus: true }),
    ),
    summary
      ? reportBody(summary, "history", s.now)
      : h("p", { class: "muted" }, "This session is no longer in the audit folder."),
  );
}

function endedLine(endedBy: EndedBy | undefined, endedAt: number | string | undefined): string {
  const at = formatTime(endedAt);
  if (endedBy === undefined) {
    return at === "—"
      ? "When and how it ended is not recorded"
      : `Ended at ${at} · who ended it is not recorded`;
  }
  const who = endedByWords(endedBy);
  const sentence = who.charAt(0).toUpperCase() + who.slice(1);
  return at === "—" ? sentence : `${sentence} at ${at}`;
}

export function reportBody(
  summary: SessionSummary,
  context: "end" | "history",
  now: number,
): HTMLElement {
  const host = summary.hostname ?? "Unknown computer";
  const total = summary.actions.ai + summary.actions.terminal + summary.actions.technician;
  return h(
    "div",
    { class: "report-body" },
    h(
      "div",
      { class: "row wrap" },
      pill("none", "○ No access"),
      h(
        "span",
        { class: "muted" },
        `Session ${shortId(summary.id)} · ${endedLine(summary.endedBy, summary.endedAt)}`,
      ),
    ),
    h("h2", null, `${host} · ${formatDuration(summary.durationMs)}`),
    integrityNote(summary),
    relayLostNote(summaryRelay(summary), summary.endedBy),
    h(
      "div",
      { class: "grid2" },
      card(
        h(
          "div",
          { class: "card-head" },
          h("h4", null, `Cleanup on ${host}`),
          cleanupPill(summary.cleanup),
        ),
        cleanupLines(summary.cleanup, host),
      ),
      card(
        h("h4", null, "What happened"),
        kv([
          [
            "Actions",
            `${total} · AI assistant ${summary.actions.ai} · you ${summary.actions.technician} · terminal ${summary.actions.terminal}`,
          ],
          ["Files", `${summary.filesSent} sent · ${summary.filesReceived} received`],
          [
            "Failures",
            summary.failures === 0
              ? "None"
              : summary.firstFailure
                ? `${summary.failures} · first: ${summary.firstFailure}`
                : String(summary.failures),
          ],
          ["Started", formatWhen(summary.startedAt, now)],
          ...connectionRows(summaryRelay(summary)),
          [
            "Audit",
            h(
              "button",
              {
                type: "button",
                class: "linkbtn",
                "data-action": "open-audit",
                "data-arg": summary.id,
                "aria-label":
                  context === "end"
                    ? "Open the audit log of this session"
                    : `Open the audit log of ${host}`,
              },
              "Open audit log",
            ),
          ],
        ]),
      ),
    ),
  );
}

/**
 * The relay of a session as its audit log recorded it: the name, where the
 * technician's copy connected, and what vouched for the name (ST-M007-013).
 */
function summaryRelay(summary: SessionSummary): NamedRelay | undefined {
  if (summary.relay === undefined) return undefined;
  const relay: NamedRelay = { host: summary.relay };
  if (summary.relayAddress !== undefined) relay.address = summary.relayAddress;
  if (summary.relayTrust !== undefined) relay.trust = summary.relayTrust;
  return relay;
}

/**
 * Which way the session went, as rows of the report: on one network, or
 * through a relay by its name, with where the copy connected when that is
 * another host, and what its certificate was checked against when that was
 * a file of the profile (ST-M007-011).
 */
function connectionRows(
  relay: NamedRelay | undefined,
): ReadonlyArray<readonly [string, Child] | null> {
  if (!relay) return [["Connection", "On one network"]];
  return [["Connection", ["Through the relay ", relayLabel(relay)]], trustFact(relay)];
}

/**
 * A session through a relay whose connection was lost says so with the relay
 * and a next step, so that it does not read as a lost session of unknown
 * cause (ST-M007-013, REQ-N-015).
 */
function relayLostNote(relay: NamedRelay | undefined, endedBy: EndedBy | undefined): Child {
  if (!relay || endedBy !== "connection_lost") return null;
  const words = relayLostWords(relayWords(relay));
  return h(
    "div",
    { class: "note relay-lost", role: "note" },
    icon("alert"),
    h("p", null, h("b", null, `${words.message} `), words.next),
  );
}

function integrityNote(summary: SessionSummary): Child {
  if (summary.integrity === "broken") {
    return h(
      "div",
      { class: "note bad" },
      icon("alert"),
      h(
        "p",
        null,
        h("b", null, "This audit file is broken. "),
        "Its records do not verify, so this report may be incomplete or altered. Keep the file for review.",
      ),
    );
  }
  if (summary.integrity === "unsealed") {
    return h(
      "div",
      { class: "note" },
      icon("alert"),
      h(
        "p",
        null,
        h("b", null, "This audit file was not sealed. "),
        "The session was interrupted, so this report may be incomplete.",
      ),
    );
  }
  return null;
}

/** The notes with failures and residuals first, then a safe next step. */
function cleanupLines(cleanup: CleanupResult, host: string): HTMLElement {
  const notes = [...cleanup.notes].sort((a, b) => Number(a.ok) - Number(b.ok));
  const box = h("div", { class: "cleanup" });
  box.append(cleanupHeadline(cleanup, host));
  for (const note of notes) box.append(noteLine(note));
  return box;
}

function noteLine(note: CleanupNote): HTMLElement {
  return h(
    "div",
    { class: "okline" },
    h("span", { class: note.ok ? "i" : "i w" }, icon(note.ok ? "check" : "alert")),
    h("div", null, h("span", { class: "sr-only" }, note.ok ? "Done: " : "Not done: "), note.text),
  );
}

function cleanupHeadline(cleanup: CleanupResult, host: string): HTMLElement {
  switch (cleanup.state) {
    case "cleaned_up":
      return h(
        "div",
        { class: "okline" },
        h("span", { class: "i" }, icon("check")),
        h("div", null, h("b", null, "Cleaned up. "), `Nothing of this session is left on ${host}.`),
      );
    case "residuals":
      return h(
        "div",
        { class: "okline" },
        h("span", { class: "i w" }, icon("alert")),
        h(
          "div",
          null,
          h("b", null, `${plural(cleanup.residuals, "residual")}. `),
          "Something of this session stays on the customer computer, listed below.",
          h(
            "div",
            { class: "muted" },
            "Next step: tell the customer what stays; if it should go, remove it in a new session. Each residual is recorded in the audit log.",
          ),
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
          "Some temporary helpers or access paths may still be on the customer computer.",
          h(
            "div",
            { class: "muted" },
            "Next step: start a new session with the customer to check and remove the items listed below. The audit log records what was attempted.",
          ),
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
          h("b", null, "Cleanup unknown. "),
          "No cleanup report arrived, so nothing is known about what was removed.",
          h(
            "div",
            { class: "muted" },
            "Next step: don't assume the computer is clean. Start a new session with the customer to check it.",
          ),
        ),
      );
  }
}

/** Until the sealed audit file has been read: the live feed and the stop reason. */
function preparingReport(s: TechnicianSnapshot): HTMLElement {
  const last = s.last;
  const host = targetName(s);
  const counts = sessionCounts(s.activity);
  const endedBy = stopReasonEndedBy(last);
  const duration =
    last?.since !== undefined && last.endedAt !== undefined
      ? formatDuration(last.endedAt - last.since)
      : null;
  return h(
    "div",
    { class: "report-body preparing" },
    h(
      "div",
      { class: "row wrap" },
      pill("none", "○ No access"),
      h(
        "span",
        { class: "muted" },
        `${last ? `Session ${shortId(last.id)} · ` : ""}${endedLine(endedBy, last?.endedAt)}`,
      ),
    ),
    h("h2", null, duration ? `${host} · ${duration}` : host),
    relayLostNote(s.console.relay, endedBy),
    h(
      "div",
      { class: "grid2" },
      card(
        h("h4", null, `Cleanup on ${host}`),
        s.reportPending
          ? h(
              "div",
              { class: "okline", "aria-busy": "true" },
              h("span", { class: "spinner", "aria-hidden": "true" }),
              h(
                "div",
                null,
                h("b", null, "Preparing the report… "),
                "The cleanup result is read from the audit log once it is sealed.",
              ),
            )
          : h(
              "div",
              { class: "okline" },
              h("span", { class: "i w" }, icon("alert")),
              h(
                "div",
                null,
                h("b", null, "Cleanup unknown. "),
                "The report of this session could not be read from the audit log.",
                h(
                  "div",
                  { class: "muted" },
                  "Next step: open the audit folder to check it, and don't assume the computer is clean.",
                ),
              ),
            ),
      ),
      card(
        h("h4", null, "What happened"),
        kv([
          [
            "Actions",
            `${counts.total} · AI assistant ${counts.ai} · you ${counts.technician} · terminal ${counts.terminal}`,
          ],
          ["Files", `${counts.filesSent} sent · ${counts.filesReceived} received`],
          [
            "Failures",
            counts.failed === 0
              ? "None"
              : `${counts.failed} · first: ${counts.firstFailure?.operation ?? ""}`,
          ],
          counts.notShown > 0 ? ["Not shown", plural(counts.notShown, "item")] : null,
          last?.failure ? ["Failure", `${last.failure.message} (${last.failure.stage})`] : null,
          ...connectionRows(s.console.relay),
          [
            "Audit",
            h(
              "button",
              {
                type: "button",
                class: "linkbtn",
                "data-action": "open-audit",
                "data-arg": last?.id,
              },
              "Open audit log",
            ),
          ],
        ]),
      ),
    ),
  );
}
