// Tests of the customer snapshot's pure derivations (ST-M009-009).

import { describe, expect, it } from "vitest";
import type { ActivityItem } from "../shared/contract";
import {
  CONSOLES,
  FEED,
  FEED_DONE,
  LAUNCH_READY,
  NOW,
  REPORT,
  VIA_RELAY,
  viewIn,
} from "./fixtures";
import {
  type CustomerSnapshot,
  SCREEN_RECENT_MS,
  announcement,
  applyView,
  canStop,
  consoleName,
  filtered,
  happeningNow,
  initialSnapshot,
  isRelayFailure,
  liveCounts,
  nobodyWaitsWords,
  outcomeWords,
  relayFailureTitle,
  relayHost,
  relayLostWords,
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

describe("a session through the relay (ST-M007-013)", () => {
  const reaching = viewIn({ phase: "announced" }, { technician: null, relay: VIA_RELAY });

  it("knows the relay of a session, and none on one network", () => {
    expect(relayHost(snapshot(reaching))).toBe("relay.example.org");
    expect(relayHost(snapshot(viewIn({ phase: "announced" })))).toBeUndefined();
    expect(relayHost(snapshot())).toBeUndefined();
  });

  it("shows the connecting screen for a relay code and announces the relay", () => {
    const s = snapshot(reaching);
    expect(screenFor(s)).toBe("connecting");
    expect(canStop(s)).toBe(true);
    expect(announcement(s)).toBe("Connecting through the relay relay.example.org. No access.");
  });

  it("returns to the code field when nobody waits or the relay cannot be used", () => {
    let s = applyView(snapshot(), reaching, NOW);
    s = applyView(
      s,
      viewIn(
        { phase: "failed", stage: "rendezvous", message: "nobody joined the rendezvous on the relay" },
        { relay: VIA_RELAY },
      ),
      NOW,
    );
    expect(screenFor(s)).toBe("find");
    // A failure is said in the alert, not as how a session ended.
    expect(s.ended).toBe("");
    expect(canStop(s)).toBe(false);
  });

  it("says with the relay that a connection was lost before anyone had access", () => {
    let s = applyView(snapshot(), reaching, NOW);
    s = applyView(s, viewIn({ phase: "stopped", reason: "disconnected" }, { relay: VIA_RELAY }), NOW);
    expect(s.ended).toBe(
      "The connection through the relay relay.example.org was lost before anyone got access. Nothing was shared.",
    );
    // On one network the sentence is the one it was.
    let lan = applyView(snapshot(), viewIn({ phase: "announced" }), NOW);
    lan = applyView(lan, viewIn({ phase: "stopped", reason: "disconnected" }), NOW);
    expect(lan.ended).toBe("The connection was lost before anyone got access. Nothing was shared.");
  });

  it("starts the next dial without what the last one said about a relay", () => {
    let s = applyView(snapshot(), reaching, NOW);
    s = applyView(s, viewIn({ phase: "stopped", reason: "disconnected" }, { relay: VIA_RELAY }), NOW);
    s = applyView(s, viewIn({ phase: "announced" }), NOW);
    expect(s.ended).toBeUndefined();
    expect(relayHost(s)).toBeUndefined();
  });

  it("names every relay failure in a few words for a customer", () => {
    const stages = [
      "relay_unreachable",
      "relay_name_not_found",
      "relay_untrusted",
      "relay_no_answer",
      "relay_key_required",
      "relay_key_refused",
      "relay_rate_limited",
      "relay_busy",
      "relay_unavailable",
      "relay_version",
      "relay_settings",
    ];
    for (const stage of stages) {
      expect(isRelayFailure(stage)).toBe(true);
      expect(relayFailureTitle(stage)).not.toBe("CretLink could not use the relay");
      // A customer is not told about keys: that is the technician's matter.
      expect(relayFailureTitle(stage).toLowerCase()).not.toContain("key");
    }
    expect(relayFailureTitle("relay_something_new")).toBe("CretLink could not use the relay");
    for (const stage of ["connect", "rendezvous", "pairing", "relay"]) {
      expect(isRelayFailure(stage)).toBe(false);
    }
  });

  it("words a code that nobody waits on and a lost connection with a next step", () => {
    const nobody = nobodyWaitsWords("relay.example.org");
    expect(nobody.title).toBe("Nobody is waiting on that code");
    expect(nobody.message).toContain("The relay relay.example.org found nobody waiting");
    expect(nobody.next).toContain("type it again");
    expect(nobodyWaitsWords(undefined).message).toContain("Nobody was waiting on the code you typed.");
    const lost = relayLostWords("relay.example.org");
    expect(lost.message).toContain("The relay may have stopped");
    expect(lost.next).toContain("Nothing connects again by itself");
  });
});
