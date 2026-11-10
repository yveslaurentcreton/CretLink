// Tests of the customer snapshot's pure derivations (ST-M009-009).

import { describe, expect, it } from "vitest";
import type { ActivityItem } from "../shared/contract";
import { CONSOLES, FEED, FEED_DONE, LAUNCH_READY, NOW, REPORT, viewIn } from "./fixtures";
import {
  type CustomerSnapshot,
  SCREEN_RECENT_MS,
  applyView,
  canStop,
  consoleName,
  filtered,
  happeningNow,
  initialSnapshot,
  liveCounts,
  outcomeWords,
  screenFor,
  screenUse,
  sourceWords,
  technicianName,
} from "./model";

function snapshot(view = viewIn({ phase: "idle" })): CustomerSnapshot {
  return { ...initialSnapshot("main", NOW), view, launch: LAUNCH_READY };
}

function item(patch: Partial<ActivityItem>): ActivityItem {
  return {
    id: "i",
    source: "ai",
    description: "Ran a command",
    kind: "command",
    operation: "run a command",
    state: { state: "done" },
    startedAt: new Date(NOW - 5000).toISOString(),
    ...patch,
  };
}

describe("screens", () => {
  it("shows the admin step before anything else while it is pending", () => {
    const s = snapshot();
    expect(screenFor({ ...s, launch: { ...LAUNCH_READY, step: "admin" } })).toBe("admin");
    expect(screenFor({ ...s, launch: { ...LAUNCH_READY, step: "declined" } })).toBe("declined");
    expect(screenFor(s)).toBe("find");
    expect(screenFor({ ...s, view: null })).toBe("loading");
  });

  it("follows the agent phase", () => {
    expect(screenFor(snapshot(viewIn({ phase: "announced" })))).toBe("connecting");
    expect(screenFor(snapshot(viewIn({ phase: "pairing" }, { approval_code: "123456" })))).toBe("compare");
    expect(screenFor(snapshot(viewIn({ phase: "awaiting_approval_compare" })))).toBe("compare");
    expect(screenFor(snapshot(viewIn({ phase: "awaiting_consent" })))).toBe("consent");
    expect(screenFor(snapshot(viewIn({ phase: "full_control", session: "s" })))).toBe("live");
    expect(screenFor(snapshot(viewIn({ phase: "stopping" })))).toBe("stopping");
  });

  it("ends on the report after control, and back on the list before it", () => {
    const stopped = viewIn({ phase: "stopped", reason: "customer_requested" });
    expect(screenFor(snapshot(stopped))).toBe("find");
    expect(screenFor({ ...snapshot(stopped), hadControl: true })).toBe("end");
    expect(screenFor({ ...snapshot(stopped), report: REPORT })).toBe("end");
  });
});

describe("stop availability", () => {
  it("exists only while a session exists, never on the resting window", () => {
    expect(canStop(snapshot())).toBe(false);
    expect(canStop(snapshot(viewIn({ phase: "stopped", reason: "customer_requested" })))).toBe(false);
    for (const phase of [
      { phase: "announced" },
      { phase: "awaiting_approval_compare" },
      { phase: "awaiting_consent" },
      { phase: "full_control", session: "s" },
    ] as const) {
      expect(canStop(snapshot(viewIn(phase))), phase.phase).toBe(true);
    }
  });
});

describe("applyView", () => {
  it("remembers that control was reached, and clears the last session when a new dial starts", () => {
    let s = snapshot(viewIn({ phase: "full_control", session: "s" }));
    s = applyView({ ...s, activity: FEED }, viewIn({ phase: "full_control", session: "s" }), NOW);
    expect(s.hadControl).toBe(true);
    s = applyView(s, viewIn({ phase: "stopped", reason: "customer_requested" }), NOW);
    expect(s.hadControl).toBe(true);
    expect(s.activity).toHaveLength(FEED.length);
    s = applyView({ ...s, report: REPORT, saved: "x" }, viewIn({ phase: "announced" }), NOW);
    expect(s.hadControl).toBe(false);
    expect(s.activity).toEqual([]);
    expect(s.report).toBeNull();
    expect(s.saved).toBeUndefined();
  });

  it("says how a session ended before control", () => {
    let s = snapshot(viewIn({ phase: "awaiting_consent" }));
    s = applyView(s, viewIn({ phase: "stopped", reason: "technician_requested" }), NOW);
    expect(s.ended).toBe("Sam ended the connection before getting access. Nothing was shared.");
  });

  it("drops a pending close question once the session is over", () => {
    const s = { ...snapshot(viewIn({ phase: "full_control", session: "s" })), confirmClose: true };
    expect(applyView(s, viewIn({ phase: "stopped", reason: "technician_requested" }), NOW).confirmClose).toBe(false);
  });
});

describe("words", () => {
  it("names the source as the customer sees it", () => {
    expect(sourceWords(item({ source: "ai" }), "Sam")).toBe("Sam's AI assistant");
    expect(sourceWords(item({ source: "terminal" }), "Sam")).toBe("Sam · terminal");
    expect(sourceWords(item({ source: "technician" }), "Sam")).toBe("Sam");
    expect(technicianName(snapshot())).toBe("your technician");
    expect(sourceWords(item({ source: "ai" }), technicianName(snapshot()))).toBe(
      "Your technician's AI assistant",
    );
  });

  it("never calls unknown or failed work finished", () => {
    expect(outcomeWords(item({ state: { state: "done" } }), NOW)).toBe("finished");
    expect(outcomeWords(item({ state: { state: "done", exitCode: 3 } }), NOW)).toBe("finished · exit code 3");
    expect(outcomeWords(item({ state: { state: "failed", message: "the result is not known" } }), NOW)).toBe(
      "failed: the result is not known",
    );
    expect(outcomeWords(item({ state: { state: "timed_out" } }), NOW)).toBe("timed out");
    expect(outcomeWords(item({ state: { state: "running" } }), NOW)).toBe("running · 5 s");
  });

  it("names a discovered console as it reports itself", () => {
    expect(consoleName(CONSOLES[0]!)).toBe("Sam · TECH-LAPTOP");
    expect(consoleName(CONSOLES[1]!)).toBe("CONSOLE-3F1");
  });
});

describe("feed", () => {
  it("counts like the end report: completed transfers, every command", () => {
    expect(liveCounts(FEED)).toMatchObject({
      commands: 3,
      failedCommands: 1,
      filesSent: 1,
      filesReceived: 1,
      screenshots: 1,
      running: 1,
    });
    const running = liveCounts([{ entry: "item", item: item({ kind: "file_sent", state: { state: "running" } }) }]);
    expect(running.filesSent).toBe(0);
  });

  it("finds the running item", () => {
    expect(happeningNow(FEED)?.id).toBe("job-6");
    expect(happeningNow(FEED_DONE)).toBeUndefined();
  });

  it("filters and keeps gaps", () => {
    const gap = { entry: "gap" as const, count: 2, at: "" };
    expect(filtered([...FEED, gap], "files").map((e) => (e.entry === "item" ? e.item.id : "gap"))).toEqual([
      "req-3",
      "req-4",
      "gap",
    ]);
  });
});

describe("screen use", () => {
  it("is not used, being observed, being controlled, or used earlier", () => {
    const view = viewIn({ phase: "full_control", session: "s" });
    expect(screenUse(view, NOW)).toEqual({ state: "not_used" });
    const observed = { ...view, screen: { observed_at_ms: NOW - 1000, controlled_at_ms: null } };
    expect(screenUse(observed, NOW)).toEqual({ state: "observed" });
    const both = { ...view, screen: { observed_at_ms: NOW - 1000, controlled_at_ms: NOW - 2000 } };
    expect(screenUse(both, NOW)).toEqual({ state: "controlled" });
    expect(screenUse(both, NOW + SCREEN_RECENT_MS)).toEqual({ state: "earlier", at: NOW - 1000 });
  });
});
