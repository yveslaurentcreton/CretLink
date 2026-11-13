import { describe, expect, it } from "vitest";
import type { ActivityEntry } from "../shared/contract";
import {
  AUDIT,
  COPY_KEY,
  COPY_MADE,
  COPY_OPTIONS,
  COPY_OPTIONS_CHOOSE,
  FEED,
  FIXTURES,
  NETWORKS,
  NOW,
  RELAY,
  RELAY_CODE,
  RELAY_FILE,
  RELAY_FILE_PROBLEM,
  RELAY_NONE,
  RELAY_PRIVATE,
  RELAY_RUN_ONCE,
  RELAY_RUN_ONCE_FILE,
  RELAY_STORED,
  SESSION_ID,
  TARGET,
  VIA_RELAY,
  VIA_RELAY_ELSEWHERE,
  consoleIn,
} from "./fixtures";
import {
  type CustomerCopyDraft,
  activeRelay,
  applyConsole,
  auditActionWords,
  auditOutcomeWords,
  canStop,
  cleanupWords,
  customerCopyForm,
  formatDuration,
  groupCode,
  initialRelayDraft,
  initialSnapshot,
  isActivityEvent,
  isAuditEntry,
  isBridgeStatus,
  isChosenCa,
  isChosenForCopy,
  isConsoleNotice,
  isCopyLabel,
  isCustomerCopyMade,
  isCustomerCopyOptions,
  isGeneratedKey,
  isListenNetwork,
  isRelaySettings,
  isRelayTest,
  keyFileLine,
  listenHost,
  otherNetworks,
  phaseAnnouncement,
  relayCodeGroups,
  relayFailureTitle,
  relayForm,
  relayFromFile,
  relayLostWords,
  relayOffer,
  isRelayFailure,
  screenFor,
  sessionCounts,
  spokenCode,
  stateWords,
  technicianPick,
  type TechnicianSnapshot,
  trimFeed,
  upsertActivity,
  waitingNetwork,
  waitingRelay,
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

describe("the network to wait on (ST-M009-016)", () => {
  const waiting = FIXTURES.wait as TechnicianSnapshot;

  it("reads the host of a listen address with or without brackets", () => {
    expect(listenHost("10.0.0.42:7000")).toBe("10.0.0.42");
    expect(listenHost("[fd00::1]:7000")).toBe("fd00::1");
    expect(listenHost("fd00::1")).toBe("fd00::1");
    expect(listenHost("10.0.0.42")).toBe("10.0.0.42");
  });

  it("names the network the session waits on and lists the others", () => {
    expect(waitingNetwork(waiting)?.adapter).toBe("Wi-Fi");
    expect(otherNetworks(waiting).map((network) => network.adapter)).toEqual([
      "vEthernet (Default Switch)",
      "VPN",
    ]);
  });

  it("offers nothing when there is one network, none is known, or nobody waits", () => {
    expect(otherNetworks({ ...waiting, networks: NETWORKS.slice(0, 1) })).toEqual([]);
    expect(otherNetworks({ ...waiting, networks: null })).toEqual([]);
    expect(waitingNetwork({ ...waiting, networks: null })).toBeUndefined();
    const home = FIXTURES.home as TechnicianSnapshot;
    expect(otherNetworks(home)).toEqual([]);
    expect(waitingNetwork(home)).toBeUndefined();
    // An address that is not in the list has no adapter to name, and every
    // listed network is then another one.
    const elsewhere = {
      ...waiting,
      console: consoleIn({ phase: "listening", code: "6RRR-2H8F", address: "10.0.0.9:7000" }),
    };
    expect(waitingNetwork(elsewhere)).toBeUndefined();
    expect(otherNetworks(elsewhere)).toHaveLength(NETWORKS.length);
  });

  it("stays on the waiting screen while the network changes", () => {
    // Between the old listener and the new one the console reports a stop.
    const between = {
      ...waiting,
      console: consoleIn({ phase: "stopped", reason: "technician_requested" }),
    };
    expect(screenFor(between)).toBe("home");
    expect(screenFor({ ...between, busy: "network" })).toBe("wait");
    expect(screenFor({ ...between, busy: "network", nav: "settings" })).toBe("wait");
  });

  it("takes only a ListenNetwork of the contract", () => {
    expect(isListenNetwork(NETWORKS[0])).toBe(true);
    for (const bad of [
      null,
      "10.0.0.42",
      { address: "", adapter: "Wi-Fi", pointToPoint: false },
      { address: "10.0.0.42", adapter: 7, pointToPoint: false },
      { address: "10.0.0.42", adapter: "Wi-Fi" },
      { adapter: "Wi-Fi", pointToPoint: false },
    ]) {
      expect(isListenNetwork(bad)).toBe(false);
    }
  });
});

describe("the relay of this copy (ST-M007-011)", () => {
  const fields = (patch: Partial<Parameters<typeof relayForm>[0]> = {}) => ({
    address: "relay.example.org:443",
    key: "",
    removeKey: false,
    name: "",
    ...patch,
  });

  it("names the relay in use: the file next to the program before the settings", () => {
    expect(activeRelay(null)).toBeUndefined();
    expect(activeRelay(RELAY_NONE)).toBeUndefined();
    expect(activeRelay(RELAY_RUN_ONCE)).toBeUndefined();
    expect(activeRelay(RELAY_STORED)).toBe(RELAY);
    expect(activeRelay(RELAY_FILE)).toBe(RELAY_PRIVATE);
    // A file that cannot be used is not passed over for the settings.
    expect(activeRelay(RELAY_FILE_PROBLEM)).toBeUndefined();
  });

  it("keeps the kept key when its field is empty, and never sends a key back", () => {
    const draft = initialRelayDraft();
    expect(relayForm(fields(), draft, RELAY)).toEqual({
      address: "relay.example.org:443",
      keyMode: "keep",
      caMode: "none",
    });
    // Nothing is kept: an empty field means that the relay asks for no key.
    expect(relayForm(fields(), draft, undefined).keyMode).toBe("none");
    expect(relayForm(fields(), draft, { ...RELAY, keySet: false }).keyMode).toBe("none");
    // The technician asked to remove the key that is kept.
    expect(relayForm(fields({ removeKey: true }), draft, RELAY).keyMode).toBe("none");
    // A typed key replaces the kept one, whatever the checkbox says.
    expect(relayForm(fields({ key: "new-key", removeKey: true }), draft, RELAY)).toMatchObject({
      keyMode: "set",
      key: "new-key",
    });
  });

  it("takes the key as it was typed and trims the address and the name", () => {
    const form = relayForm(
      fields({ address: "  relay.example.org:443 ", key: " a key ", name: " host-01.example.org " }),
      initialRelayDraft(),
      undefined,
    );
    expect(form.address).toBe("relay.example.org:443");
    expect(form.name).toBe("host-01.example.org");
    // Not trimmed: the backend says that a key holds no blank.
    expect(form.key).toBe(" a key ");
    expect(relayForm(fields({ name: "   " }), initialRelayDraft(), undefined).name).toBeUndefined();
  });

  it("keeps, drops or replaces the certificate file", () => {
    const kept = relayForm(fields(), initialRelayDraft(), RELAY_PRIVATE);
    expect(kept.caMode).toBe("keep");
    expect(kept.caPick).toBeUndefined();
    // Nothing is kept that could be kept.
    expect(relayForm(fields(), initialRelayDraft(), RELAY).caMode).toBe("none");
    const none = relayForm(fields(), { ca: { mode: "none" }, round: 0 }, RELAY_PRIVATE);
    expect(none.caMode).toBe("none");
    const chosen = relayForm(
      fields(),
      { ca: { mode: "chosen", pick: 12, name: "authority.pem" }, round: 0 },
      RELAY_PRIVATE,
    );
    expect(chosen).toMatchObject({ caMode: "chosen", caPick: 12 });
    // The name of the file is for the words only.
    expect(JSON.stringify(chosen)).not.toContain("authority.pem");
  });

  it("takes only values of the contract from the backend", () => {
    for (const good of [RELAY_NONE, RELAY_STORED, RELAY_FILE, RELAY_FILE_PROBLEM, RELAY_RUN_ONCE]) {
      expect(isRelaySettings(good)).toBe(true);
    }
    for (const bad of [
      null,
      "relay",
      {},
      { ...RELAY_NONE, editable: "yes" },
      { ...RELAY_NONE, source: "registry" },
      { ...RELAY_NONE, file: undefined },
      { ...RELAY_NONE, file: { path: 1 } },
      { ...RELAY_STORED, stored: { path: "x", relay: { host: "h" } } },
      // A relay without what vouches for its name is no relay of the contract.
      {
        ...RELAY_STORED,
        stored: { path: "x", relay: { host: "h", address: "h:1", keySet: false } },
      },
      {
        ...RELAY_STORED,
        stored: {
          path: "x",
          relay: { host: "h", address: "h:1", trust: "anything", keySet: false },
        },
      },
      { ...RELAY_STORED, stored: { path: "x", problem: { message: 3 } } },
    ]) {
      expect(isRelaySettings(bad)).toBe(false);
    }
    expect(isRelayTest({ ok: true, kind: "admitted", message: "m", next: "" })).toBe(true);
    expect(isRelayTest({ ok: "yes", kind: "admitted", message: "m", next: "" })).toBe(false);
    expect(isRelayTest({ ok: true, kind: "admitted", message: "m" })).toBe(false);
    expect(isChosenCa({ pick: 1, name: "authority.pem" })).toBe(true);
    expect(isChosenCa({ pick: "1", name: "authority.pem" })).toBe(false);
    expect(isChosenCa(null)).toBe(false);
    expect(isConsoleNotice({ title: "t", message: "m", next: "n" })).toBe(true);
    expect(isConsoleNotice({ title: "t", message: "m" })).toBe(false);
  });

  it("starts every snapshot without a relay and with an empty draft", () => {
    const s = initialSnapshot(consoleIn({ phase: "idle" }), NOW);
    expect(s.relay).toBeNull();
    expect(s.relayDraft).toEqual({ ca: { mode: "keep" }, round: 0 });
    expect(initialRelayDraft(3).round).toBe(3);
  });
});

describe("a session through the relay (ST-M007-013)", () => {
  const onNetwork = consoleIn({ phase: "listening", code: "6RRR-2H8F", address: "10.0.0.42:7000" });
  const onRelay = consoleIn(
    { phase: "listening", code: RELAY_CODE, address: "relay.example.org:443" },
    { relay: VIA_RELAY },
  );

  it("knows the relay a console waits on, and only while it waits", () => {
    expect(waitingRelay(onNetwork)).toBeUndefined();
    expect(waitingRelay(onRelay)).toEqual(VIA_RELAY);
    // After the customer connected the relay is still named, but nothing
    // waits on it any more.
    const compare = consoleIn(
      { phase: "awaiting_approval_code", code: "482913" },
      { session: true, target: true, relay: VIA_RELAY },
    );
    expect(waitingRelay(compare)).toBeUndefined();
    expect(compare.relay).toEqual(VIA_RELAY);
  });

  it("reads a relay code in four groups and spells it for a screen reader", () => {
    expect(relayCodeGroups(RELAY_CODE)).toEqual(["53ST", "A71D", "JKKV", "0VGH"]);
    expect(relayCodeGroups("")).toEqual([]);
    expect(spokenCode(RELAY_CODE)).toBe("5 3 S T, A 7 1 D, J K K V, 0 V G H");
  });

  it("offers the relay this copy uses, names Settings without one, and says a profile's problem", () => {
    // The whole relay: its name, where this copy connects, and what vouches
    // for the name.
    expect(relayOffer(RELAY_STORED)).toEqual({ state: "ready", relay: RELAY });
    // A file next to the program wins over the settings. In the installed
    // copy the offer says that the relay comes from that file.
    expect(relayOffer(RELAY_FILE)).toEqual({
      state: "ready",
      relay: RELAY_PRIVATE,
      fromFile: RELAY_FILE.file.path,
    });
    // A copy that runs once has no other place for a relay than the file:
    // there is nothing to say about where it comes from.
    expect(relayOffer(RELAY_RUN_ONCE_FILE)).toEqual({ state: "ready", relay: RELAY });
    expect(relayOffer(RELAY_NONE)).toEqual({ state: "none", runOnce: false });
    expect(relayOffer(RELAY_RUN_ONCE)).toEqual({ state: "none", runOnce: true });
    // Until the backend has said: no relay, and nothing claimed about the copy.
    expect(relayOffer(null)).toEqual({ state: "none", runOnce: false });
    const problem = relayOffer(RELAY_FILE_PROBLEM);
    expect(problem.state).toBe("problem");
    if (problem.state === "problem") {
      expect(problem.file).toBe(RELAY_FILE_PROBLEM.file.problem?.file);
      expect(problem.message).toBe(RELAY_FILE_PROBLEM.file.problem?.message);
    }
    // Settings that cannot be read, with no file next to the program.
    const unreadable = relayOffer({
      editable: true,
      source: "settings",
      file: { path: "cretlink.toml" },
      stored: { path: "settings.toml", problem: { file: "settings.toml", message: "Line 2." } },
    });
    expect(unreadable).toEqual({ state: "problem", message: "Line 2.", file: "settings.toml" });
  });

  it("says that the installed copy takes its relay from the file only when it does", () => {
    expect(relayFromFile(RELAY_FILE)).toBe(RELAY_FILE.file.path);
    // From Settings, or no relay at all: nothing to say.
    expect(relayFromFile(RELAY_STORED)).toBeUndefined();
    expect(relayFromFile(RELAY_NONE)).toBeUndefined();
    expect(relayFromFile(null)).toBeUndefined();
    // A file that cannot be used gives no relay: its problem is said instead.
    expect(relayFromFile(RELAY_FILE_PROBLEM)).toBeUndefined();
    // A copy that runs once keeps no settings to prefer.
    expect(relayFromFile(RELAY_RUN_ONCE_FILE)).toBeUndefined();
    expect(relayFromFile(RELAY_RUN_ONCE)).toBeUndefined();
  });

  it("announces a relay with where this copy connects when that is another host", () => {
    const waiting = consoleIn(
      { phase: "listening", code: RELAY_CODE, address: "192.0.2.1:9970" },
      { relay: VIA_RELAY_ELSEWHERE },
    );
    expect(phaseAnnouncement(initialSnapshot(waiting, NOW))).toBe(
      "Waiting for the customer on the relay relay.example.org (reached at 192.0.2.1:9970). Read them the relay code. No access.",
    );
    const reaching = consoleIn(
      { phase: "listening", code: RELAY_CODE, address: "192.0.2.1:9970" },
      { relay: { ...VIA_RELAY_ELSEWHERE, admitted: false } },
    );
    expect(phaseAnnouncement(initialSnapshot(reaching, NOW))).toBe(
      "Connecting to the relay relay.example.org (reached at 192.0.2.1:9970). No access.",
    );
  });

  it("keeps the waiting screen while the wait changes to the relay", () => {
    const stopped = consoleIn({ phase: "stopped", reason: "technician_requested" });
    const s: TechnicianSnapshot = { ...initialSnapshot(stopped, NOW), busy: "relay" };
    expect(screenFor(s)).toBe("wait");
    expect(screenFor({ ...s, busy: undefined })).toBe("home");
    // Waiting on the relay is the waiting screen, and Escape stops it.
    expect(screenFor(initialSnapshot(onRelay, NOW))).toBe("wait");
    expect(canStop(onRelay)).toBe(true);
  });

  it("announces the wait on the relay in words, and whether the relay holds the code", () => {
    const say = (state: typeof onRelay): string => phaseAnnouncement(initialSnapshot(state, NOW));
    expect(say(onNetwork)).toBe("Waiting for the customer. No access.");
    expect(say(onRelay)).toBe(
      "Waiting for the customer on the relay relay.example.org. Read them the relay code. No access.",
    );
    expect(say({ ...onRelay, relay: { ...VIA_RELAY, admitted: false } })).toBe(
      "Connecting to the relay relay.example.org. No access.",
    );
  });

  it("starts a new feed when the wait changes from the network to the relay", () => {
    const start = { ...initialSnapshot(onNetwork, NOW), activity: FEED };
    const stopped = applyConsole(
      start,
      consoleIn({ phase: "stopped", reason: "technician_requested" }),
      NOW,
    );
    // No session existed, so nothing ended that needs a report.
    expect(stopped.snapshot.last).toBeUndefined();
    const waiting = applyConsole({ ...stopped.snapshot, busy: "relay" }, onRelay, NOW);
    expect(waiting.snapshot.activity).toEqual([]);
    expect(waiting.snapshot.console.relay).toEqual(VIA_RELAY);
    expect(waiting.ended).toBe(false);
  });

  it("names every relay failure in a few words of its own", () => {
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
    const titles = stages.map(relayFailureTitle);
    expect(new Set(titles).size).toBe(stages.length);
    for (const stage of stages) expect(isRelayFailure(stage)).toBe(true);
    expect(titles).not.toContain("The relay could not be used");
    // A stage this window does not know still reads as a relay matter.
    expect(relayFailureTitle("relay_something_new")).toBe("The relay could not be used");
    for (const stage of ["listen", "pairing", "compare", "relay"]) {
      expect(isRelayFailure(stage)).toBe(false);
    }
  });

  it("says a lost connection through a relay with the relay and a next step", () => {
    const words = relayLostWords("relay.example.org");
    expect(words.message).toContain("through the relay relay.example.org was lost");
    expect(words.message).toContain("The relay may have stopped");
    expect(words.next).toContain("Nobody has access");
    expect(words.next).toContain("new relay code");
  });
});

describe("make a customer copy (ST-M007-016)", () => {
  it("takes a label of letters, digits and hyphens within the bound", () => {
    for (const label of ["a", "ACME-07", "2026-front-desk", "-", "A".repeat(40)]) {
      expect(isCopyLabel(label, 40), label).toBe(true);
    }
    for (const label of ["", " ", "front desk", "../x", "a/b", "a\\b", "a.b", "a_b", "café", "A".repeat(41)]) {
      expect(isCopyLabel(label, 40), label).toBe(false);
    }
  });

  it("writes the line of the relay's key file as label and key", () => {
    expect(keyFileLine("acme-front-desk", COPY_KEY)).toBe(`acme-front-desk:${COPY_KEY}`);
  });

  it("sends the key that was typed for this customer, by the numbers of what was chosen", () => {
    const draft: CustomerCopyDraft = {
      options: COPY_OPTIONS_CHOOSE,
      program: { pick: 3, name: "cretlink.exe" },
      place: { pick: 4, name: "/home/sam/Customers" },
    };
    const typed = "cd".repeat(16);
    expect(customerCopyForm({ label: "  acme ", key: typed, noKey: false }, draft)).toEqual({
      label: "acme",
      keyMode: "set",
      key: typed,
      programPick: 3,
      placePick: 4,
    });
    // Neither a path nor a name of what was chosen travels: only numbers.
    const sent = JSON.stringify(customerCopyForm({ label: "acme", key: typed, noKey: false }, draft));
    expect(sent).not.toContain("cretlink.exe");
    expect(sent).not.toContain("/home/sam");
    // The technician's answer to "replace it?" is said only when it was given.
    expect(customerCopyForm({ label: "acme", key: typed, noKey: false }, draft, true).replace).toBe(true);
    expect("replace" in customerCopyForm({ label: "acme", key: typed, noKey: false }, draft)).toBe(false);
  });

  it("sends a generated key with the label it was generated for", () => {
    const draft: CustomerCopyDraft = {
      options: COPY_OPTIONS,
      generated: { label: "acme-front-desk", key: COPY_KEY },
    };
    // Whatever stands in the fields: the generated key and its label go.
    expect(customerCopyForm({ label: "something-else", key: "", noKey: true }, draft)).toEqual({
      label: "acme-front-desk",
      keyMode: "set",
      key: COPY_KEY,
    });
  });

  it("makes a copy without a key for a relay that asks for none, and has no other choice", () => {
    const draft: CustomerCopyDraft = { options: COPY_OPTIONS };
    const none = customerCopyForm({ label: "acme", key: "typed-and-ignored", noKey: true }, draft);
    expect(none).toEqual({ label: "acme", keyMode: "none" });
    // A form says "set" or "none": there is no word for the technician's own key.
    const empty = customerCopyForm({ label: "acme", key: "", noKey: false }, draft);
    expect(empty).toEqual({ label: "acme", keyMode: "set", key: "" });
  });

  it("recognises what the backend answers", () => {
    expect(isCustomerCopyOptions(COPY_OPTIONS)).toBe(true);
    expect(isCustomerCopyOptions(COPY_OPTIONS_CHOOSE)).toBe(true);
    for (const bad of [
      null,
      {},
      { ...COPY_OPTIONS, relay: { host: "h" } },
      { ...COPY_OPTIONS, program: "ask" },
      { ...COPY_OPTIONS, labelMax: "40" },
      { ...COPY_OPTIONS, folderPrefix: undefined },
    ]) {
      expect(isCustomerCopyOptions(bad)).toBe(false);
    }
    expect(isChosenForCopy({ pick: 1, name: "cretlink.exe" })).toBe(true);
    expect(isChosenForCopy({ pick: "1", name: "cretlink.exe" })).toBe(false);
    expect(isChosenForCopy({ pick: 1 })).toBe(false);
    expect(isGeneratedKey({ key: COPY_KEY })).toBe(true);
    expect(isGeneratedKey({ key: "" })).toBe(false);
    expect(isGeneratedKey({})).toBe(false);
    expect(isCustomerCopyMade(COPY_MADE)).toBe(true);
    expect(isCustomerCopyMade({ ...COPY_MADE, setAside: "x" })).toBe(true);
    expect(isCustomerCopyMade({ ...COPY_MADE, files: [1] })).toBe(false);
    expect(isCustomerCopyMade({ ...COPY_MADE, copy: undefined })).toBe(false);
    expect(isCustomerCopyMade({ ...COPY_MADE, setAside: 1 })).toBe(false);
  });
});
