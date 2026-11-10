import { describe, expect, it } from "vitest";
import type { ActivityEntry } from "../shared/contract";
import { AUDIT, FEED, NOW, SESSION_ID, TARGET, consoleIn } from "./fixtures";
import {
  applyConsole,
  auditActionWords,
  auditOutcomeWords,
  canStop,
  cleanupWords,
  formatDuration,
  groupCode,
  initialSnapshot,
  isActivityEvent,
  isAuditEntry,
  isBridgeStatus,
  screenFor,
  sessionCounts,
  stateWords,
  technicianPick,
  type TechnicianSnapshot,
  trimFeed,
  upsertActivity,
} from "./model";

const item = (id: string, state: "running" | "done" = "done"): ActivityEntry => ({
  entry: "item",
  item: {
    id,
    source: "ai",
    description: "Ran a command",
    kind: "command",
    operation: `op ${id}`,
    state: state === "running" ? { state: "running" } : { state: "done", exitCode: 0 },
    startedAt: new Date(NOW).toISOString(),
  },
});

describe("applyConsole", () => {
  it("clears the feed only when the next session starts", () => {
    let s: TechnicianSnapshot = {
      ...initialSnapshot(
        consoleIn({ phase: "full_control" }, { session: true, target: true }),
        NOW,
      ),
      activity: FEED,
      last: { id: SESSION_ID, target: TARGET },
    };
    // The session ends: the feed stays, the end is reported once.
    const ended = applyConsole(
      s,
      consoleIn({ phase: "stopped", reason: "customer_requested" }),
      NOW + 1000,
    );
    expect(ended.ended).toBe(true);
    expect(ended.snapshot.activity).toBe(FEED);
    expect(ended.snapshot.last).toMatchObject({
      id: SESSION_ID,
      target: TARGET,
      reason: "customer_requested",
      endedAt: NOW + 1000,
    });
    expect(ended.snapshot.reportPending).toBe(true);
    expect(screenFor(ended.snapshot)).toBe("end");
    s = ended.snapshot;
    // A tab switch or a nav change does not touch it; only listening again does.
    expect(
      applyConsole(s, consoleIn({ phase: "stopped", reason: "customer_requested" }), NOW).ended,
    ).toBe(false);
    const next = applyConsole(
      s,
      consoleIn({ phase: "listening", code: "AB", address: "a" }),
      NOW + 2000,
    ).snapshot;
    expect(next.activity).toEqual([]);
    expect(next.last).toBeUndefined();
    expect(screenFor(next)).toBe("wait");
  });

  it("remembers the session id and target the console clears at the end", () => {
    const s0 = initialSnapshot(consoleIn({ phase: "idle" }), NOW);
    const s1 = applyConsole(
      s0,
      consoleIn({ phase: "listening", code: "AB", address: "a" }),
      NOW,
    ).snapshot;
    const s2 = applyConsole(
      s1,
      consoleIn(
        { phase: "awaiting_approval_code", code: "123456" },
        { session: true, target: true },
      ),
      NOW,
    ).snapshot;
    const s3 = applyConsole(
      s2,
      consoleIn({ phase: "full_control" }, { session: true, target: true }),
      NOW + 5000,
    ).snapshot;
    expect(s3.last).toMatchObject({ id: SESSION_ID, since: NOW + 5000, target: TARGET });
    const s4 = applyConsole(
      s3,
      consoleIn({ phase: "stopped", reason: "disconnected" }),
      NOW + 9000,
    ).snapshot;
    expect(s4.last).toMatchObject({
      id: SESSION_ID,
      since: NOW + 5000,
      endedAt: NOW + 9000,
      reason: "disconnected",
    });
  });

  it("brings the session screen back when its phase changes", () => {
    const s = {
      ...initialSnapshot(consoleIn({ phase: "listening", code: "AB", address: "a" }), NOW),
      nav: "ai" as const,
    };
    expect(screenFor(s)).toBe("ai");
    const next = applyConsole(
      s,
      consoleIn({ phase: "awaiting_approval_code", code: "123456" }, { session: true }),
      NOW,
    ).snapshot;
    expect(screenFor(next)).toBe("compare");
  });
});

describe("audit entries", () => {
  it("accepts only contract entries", () => {
    for (const entry of AUDIT) expect(isAuditEntry(entry)).toBe(true);
    expect(isAuditEntry({ ...AUDIT[0], actor: "customer" })).toBe(false);
    expect(isAuditEntry({ ...AUDIT[0], outcome: "done" })).toBe(false);
    expect(isAuditEntry({ ...AUDIT[0], summary: 3 })).toBe(false);
    expect(isAuditEntry(null)).toBe(false);
  });

  it("words the record and never calls an unknown outcome a success", () => {
    expect(auditActionWords("consent_granted")).toBe("consent granted");
    expect(auditOutcomeWords("unknown")).toBe("unknown, not confirmed");
    expect(auditOutcomeWords("timeout")).toBe("timed out");
  });

  it("starts a new session with no audit entries of the last one", () => {
    const s = {
      ...initialSnapshot(consoleIn({ phase: "stopped", reason: "technician_requested" }), NOW),
      audit: AUDIT,
    };
    const next = applyConsole(
      s,
      consoleIn({ phase: "listening", code: "AB", address: "a" }),
      NOW,
    ).snapshot;
    expect(next.audit).toEqual([]);
  });
});

describe("canStop", () => {
  it("is true in every live phase (Escape stops, REQ-F-018)", () => {
    expect(canStop(consoleIn({ phase: "listening", code: "a", address: "b" }))).toBe(true);
    expect(canStop(consoleIn({ phase: "connecting" }))).toBe(true);
    expect(canStop(consoleIn({ phase: "awaiting_approval_code", code: "1" }))).toBe(true);
    expect(canStop(consoleIn({ phase: "awaiting_consent" }))).toBe(true);
    expect(canStop(consoleIn({ phase: "full_control" }))).toBe(true);
    expect(canStop(consoleIn({ phase: "idle" }))).toBe(false);
    expect(canStop(consoleIn({ phase: "stopped", reason: "technician_requested" }))).toBe(false);
  });
});

describe("activity feed", () => {
  it("upserts an item by id and appends gaps", () => {
    let feed = upsertActivity([], item("a", "running"));
    feed = upsertActivity(feed, item("b"));
    feed = upsertActivity(feed, item("a"));
    feed = upsertActivity(feed, { entry: "gap", count: 2, at: "x" });
    expect(feed).toHaveLength(3);
    expect(feed[0]).toMatchObject({ entry: "item", item: { id: "a", state: { state: "done" } } });
    expect(feed[2]).toEqual({ entry: "gap", count: 2, at: "x" });
  });

  it("folds entries past the limit into one gap marker, never silently", () => {
    let feed: ActivityEntry[] = [];
    for (let i = 0; i < 6; i += 1) feed = upsertActivity(feed, item(String(i)), 4);
    expect(feed).toHaveLength(4);
    expect(feed[0]).toMatchObject({ entry: "gap", count: 3 });
    feed = upsertActivity(feed, item("6"), 4);
    expect(feed[0]).toMatchObject({ entry: "gap", count: 4 });
    expect(trimFeed(feed.slice(), 10)).toHaveLength(4);
  });

  it("accepts only contract events", () => {
    expect(isActivityEvent(item("a"))).toBe(true);
    expect(isActivityEvent({ entry: "gap", count: 1, at: "" })).toBe(true);
    expect(isActivityEvent({ source: "claude", kind: "start", text: "x" })).toBe(false);
    expect(isActivityEvent(null)).toBe(false);
  });

  it("counts actions by source, files, failures and clients", () => {
    const counts = sessionCounts(FEED);
    expect(counts).toMatchObject({
      total: 7,
      ai: 4,
      terminal: 1,
      technician: 2,
      filesSent: 1,
      filesReceived: 1,
      failed: 2,
      running: 1,
      notShown: 3,
      clients: ["Claude Code", "Codex CLI"],
    });
    expect(counts.firstFailure?.operation).toBe("netsh wlan show drivers");
  });

  it("counts only finished transfers as files sent or received", () => {
    const transfer = (FEED[5] as Extract<ActivityEntry, { entry: "item" }>).item;
    expect(transfer.kind).toBe("file_sent");
    const feed: ActivityEntry[] = [
      { entry: "item", item: { ...transfer, id: "t1", state: { state: "done" } } },
      { entry: "item", item: { ...transfer, id: "t2", state: { state: "failed", message: "denied" } } },
      { entry: "item", item: { ...transfer, id: "t3", state: { state: "running" } } },
      { entry: "item", item: { ...transfer, id: "t4", kind: "file_received", state: { state: "cancelled" } } },
      { entry: "item", item: { ...transfer, id: "t5", kind: "file_received", state: { state: "timed_out" } } },
    ];
    const counts = sessionCounts(feed);
    expect(counts.filesSent).toBe(1);
    expect(counts.filesReceived).toBe(0);
    expect(counts.total).toBe(5);
  });
});

describe("words", () => {
  it("states results without calling unknown work a success", () => {
    const base = (FEED[7] as Extract<ActivityEntry, { entry: "item" }>).item;
    expect(stateWords(base, NOW)).toBe("running · 12 s");
    expect(stateWords({ ...base, state: { state: "done", exitCode: 0 } }, NOW)).toBe("exit 0");
    expect(stateWords({ ...base, state: { state: "done" } }, NOW)).toBe("done");
    expect(stateWords({ ...base, state: { state: "failed", exitCode: 1 } }, NOW)).toBe(
      "failed · exit 1",
    );
    expect(stateWords({ ...base, state: { state: "failed", message: "denied" } }, NOW)).toBe(
      "failed: denied",
    );
    expect(stateWords({ ...base, state: { state: "cancelled" } }, NOW)).toBe("cancelled");
    expect(stateWords({ ...base, state: { state: "timed_out" } }, NOW)).toBe("timed out");
  });

  it("states cleanup in words", () => {
    expect(cleanupWords({ state: "cleaned_up", residuals: 0, notes: [] })).toBe("Cleaned up");
    expect(cleanupWords({ state: "residuals", residuals: 1, notes: [] })).toBe("1 residual");
    expect(cleanupWords({ state: "residuals", residuals: 3, notes: [] })).toBe("3 residuals");
    expect(cleanupWords({ state: "failed", residuals: 0, notes: [] })).toBe("Cleanup failed");
    expect(cleanupWords({ state: "unknown", residuals: 0, notes: [] })).toBe("Cleanup unknown");
  });

  it("groups the approval code 3 + 3 and formats durations", () => {
    expect(groupCode("482913")).toEqual(["482", "913"]);
    expect(groupCode("482 913")).toEqual(["482", "913"]);
    expect(formatDuration(42 * 60_000)).toBe("42 min");
    expect(formatDuration(65 * 60_000)).toBe("1 h 5 min");
    expect(formatDuration(undefined)).toBe("duration unknown");
  });

  it("names the technician as the customer sees it", () => {
    expect(technicianPick(consoleIn({ phase: "idle" }, { displayName: "Sam" }))).toBe(
      "Sam · TECH-LAPTOP",
    );
    expect(technicianPick(consoleIn({ phase: "idle" }))).toBe("TECH-LAPTOP");
  });
});

describe("the bridge status (ST-M009-008)", () => {
  it("takes only a BridgeStatus of the contract", () => {
    expect(isBridgeStatus({ endpoint: "ready", tools: 24, clients: ["Claude Code"] })).toBe(true);
    expect(
      isBridgeStatus({ endpoint: "unavailable", message: "why", tools: 24, clients: [] }),
    ).toBe(true);
    for (const bad of [
      null,
      "ready",
      { endpoint: "starting", tools: 1, clients: [] },
      { endpoint: "ready", tools: "24", clients: [] },
      { endpoint: "ready", tools: 1, clients: [1] },
      { endpoint: "ready", tools: 1 },
      { endpoint: "unavailable", message: 3, tools: 1, clients: [] },
    ]) {
      expect(isBridgeStatus(bad)).toBe(false);
    }
  });
});
