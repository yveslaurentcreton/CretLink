// Controller tests (ST-M009-008): commands and events in, rendered window out,
// with Tauri and the xterm terminal replaced by fakes.

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import type { ActivityEvent } from "../shared/contract";
import { type TimerTracker, trackTimers } from "../shared/test-timers";
import type { ConsoleState } from "./api";
import {
  APP_INSTALLED,
  AUDIT,
  CLIENTS,
  COPY_KEY,
  COPY_MADE,
  COPY_OPTIONS,
  COPY_OPTIONS_CHOOSE,
  FEED,
  NETWORKS,
  RELAY,
  RELAY_CODE,
  RELAY_FILE,
  RELAY_NONE,
  RELAY_PRIVATE,
  RELAY_RUN_ONCE,
  RELAY_STORED,
  SESSIONS,
  SESSION_ID,
  VIA_RELAY,
  consoleIn,
} from "./fixtures";

type Handler = (args?: Record<string, unknown>) => unknown;

const backend = vi.hoisted(() => ({
  handlers: {} as Record<string, Handler>,
  listeners: {} as Record<string, (event: { payload: unknown }) => void>,
  calls: [] as Array<[string, Record<string, unknown> | undefined]>,
  attached: [] as HTMLElement[],
  disposed: 0,
}));

vi.mock("@tauri-apps/api/core", () => ({
  invoke: vi.fn(async (cmd: string, args?: Record<string, unknown>) => {
    backend.calls.push([cmd, args]);
    const handler = backend.handlers[cmd];
    // Tauri rejects a command the backend does not register.
    if (!handler) throw `Command ${cmd} not found`;
    return handler(args);
  }),
}));

vi.mock("@tauri-apps/api/event", () => ({
  listen: vi.fn(async (name: string, callback: (event: { payload: unknown }) => void) => {
    backend.listeners[name] = callback;
    return () => undefined;
  }),
}));

vi.mock("./terminal", () => ({
  ConsoleTerminal: class {
    private host: HTMLElement | null = null;
    attach(_badge: HTMLElement, body: HTMLElement): void {
      if (!this.host) {
        this.host = document.createElement("div");
        this.host.className = "fake-xterm";
        backend.attached.push(this.host);
      }
      if (this.host.parentElement !== body) body.append(this.host);
    }
    refit(): void {}
    dispose(): void {
      backend.disposed += 1;
      this.host = null;
    }
  },
}));

let state: ConsoleState = consoleIn({ phase: "idle" });
// Cancels what a test leaves running, so it cannot call the fake during a later test.
let timers: TimerTracker;

function push(next: ConsoleState): void {
  state = next;
  backend.listeners["console-state"]?.({ payload: next });
}

function activity(event: ActivityEvent): void {
  backend.listeners["console-activity"]?.({ payload: event });
}

const flush = async (): Promise<void> => {
  for (let i = 0; i < 5; i += 1) await new Promise((resolve) => setTimeout(resolve, 0));
};

function app(): HTMLElement {
  return document.getElementById("app") as HTMLElement;
}

function click(selector: string): void {
  const el = app().querySelector<HTMLElement>(selector);
  if (!el) throw new Error(`no ${selector}`);
  el.click();
}

async function boot(handlers: Record<string, Handler> = {}): Promise<void> {
  backend.handlers = {
    get_state: () => state,
    stop_session: () => consoleIn({ phase: "stopped", reason: "technician_requested" }),
    ...handlers,
  };
  const { start } = await import("./main");
  await start({ version: "0.22.0" });
  await flush();
}

beforeEach(() => {
  timers = trackTimers();
  vi.resetModules();
  backend.handlers = {};
  backend.listeners = {};
  backend.calls = [];
  backend.attached = [];
  backend.disposed = 0;
  state = consoleIn({ phase: "idle" });
  document.body.innerHTML = '<div id="app" aria-busy="true"></div>';
});

afterEach(() => {
  timers.clear();
  document.body.replaceChildren();
});

describe("technician controller", () => {
  it("shows data as unavailable when the new commands do not exist yet, without crashing", async () => {
    await boot();
    const text = app().textContent ?? "";
    expect(text).toContain("Ready to help");
    expect(text).toContain("The session history is not available yet.");
    expect(text).toContain("The AI assistant status is not available yet.");
    expect(text).toContain("CretLink 0.22.0");
    expect(app().querySelector('[role="alert"]')?.textContent).toBe("");
    expect(app().hasAttribute("aria-busy")).toBe(false);
  });

  it("loads the history and the AI clients on start", async () => {
    await boot({ list_sessions: () => SESSIONS, ai_clients: () => CLIENTS });
    expect(app().querySelectorAll(".session-row")).toHaveLength(3);
    expect(app().textContent).toContain("Claude Code");
  });

  it("stops with Escape in every live phase, and not at rest", async () => {
    await boot();
    document.dispatchEvent(new KeyboardEvent("keydown", { key: "Escape" }));
    await flush();
    expect(backend.calls.filter(([cmd]) => cmd === "stop_session")).toHaveLength(0);
    const live: ConsoleState[] = [
      consoleIn({ phase: "listening", code: "6RRR-2H8F", address: "10.0.0.42:7000" }),
      consoleIn({ phase: "connecting" }, { session: true }),
      consoleIn(
        { phase: "awaiting_approval_code", code: "482913" },
        { session: true, target: true },
      ),
      consoleIn({ phase: "awaiting_consent" }, { session: true, target: true }),
      consoleIn({ phase: "full_control" }, { session: true, target: true }),
    ];
    for (const phase of live) {
      push(phase);
      await flush();
      backend.calls = [];
      document.dispatchEvent(new KeyboardEvent("keydown", { key: "Escape" }));
      await flush();
      expect(backend.calls.map(([cmd]) => cmd)).toContain("stop_session");
    }
  });

  it("answers Escape once, however often the controller was started before", async () => {
    // Every test loads the controller anew. An earlier one must not hear the
    // key as well, or one Escape would stop the session several times over.
    const waiting = consoleIn({ phase: "listening", code: "6RRR-2H8F", address: "10.0.0.42:7000" });
    await boot();
    push(waiting);
    await flush();
    // A second controller, as the next test would load it; the first one is
    // still waiting and would stop on Escape too.
    vi.resetModules();
    document.body.innerHTML = '<div id="app" aria-busy="true"></div>';
    await boot();
    push(waiting);
    await flush();
    backend.calls = [];
    document.dispatchEvent(new KeyboardEvent("keydown", { key: "Escape" }));
    await flush();
    expect(backend.calls.filter(([cmd]) => cmd === "stop_session")).toHaveLength(1);
  });

  it("keeps the feed across tab switches and the terminal alive across renders", async () => {
    // The terminal is mounted on the next frame. A frame of the test
    // environment comes from a timer, which can be late on a busy machine or
    // held back by another test's fake timers, and then this test ran into
    // its time limit. Here a frame is the next microtask.
    const frames = vi
      .spyOn(window, "requestAnimationFrame")
      .mockImplementation((callback: FrameRequestCallback) => {
        queueMicrotask(() => callback(0));
        return 0;
      });
    await boot();
    push(consoleIn({ phase: "listening", code: "6RRR-2H8F", address: "10.0.0.42:7000" }));
    push(consoleIn({ phase: "full_control" }, { session: true, target: true }));
    await flush();
    for (const event of FEED) activity(event);
    await flush();
    expect(app().querySelectorAll(".feed > li")).toHaveLength(FEED.length);

    click("#tab-terminal");
    await flush();
    await flush();
    const xterm = app().querySelector(".fake-xterm");
    expect(xterm).not.toBeNull();

    // Focus stays where it was when new activity arrives.
    const tab = app().querySelector<HTMLElement>("#tab-terminal");
    tab?.focus();
    activity({
      entry: "item",
      item: {
        ...(FEED[7] as Extract<ActivityEvent, { entry: "item" }>).item,
        state: { state: "done", exitCode: 0 },
      },
    });
    await flush();
    expect(document.activeElement).toBe(tab);
    expect(app().querySelector(".fake-xterm")).toBe(xterm);
    expect(app().querySelectorAll(".feed > li")).toHaveLength(FEED.length);

    click("#tab-files");
    await flush();
    click("#tab-terminal");
    await flush();
    await flush();
    expect(app().querySelector(".fake-xterm")).toBe(xterm);
    expect(backend.attached).toHaveLength(1);
    expect(app().querySelectorAll(".feed > li")).toHaveLength(FEED.length);
    frames.mockRestore();
  });

  it("keeps the feed after the end, refreshes the history, and clears it when the next session starts", async () => {
    let listed = 0;
    await boot({
      list_sessions: () => {
        listed += 1;
        return [{ ...SESSIONS[0], id: SESSION_ID }];
      },
    });
    push(consoleIn({ phase: "listening", code: "6RRR-2H8F", address: "10.0.0.42:7000" }));
    push(consoleIn({ phase: "full_control" }, { session: true, target: true }));
    for (const event of FEED) activity(event);
    await flush();
    const before = listed;
    push(consoleIn({ phase: "stopped", reason: "customer_requested" }));
    await flush();
    expect(listed).toBeGreaterThan(before);
    expect(backend.disposed).toBe(1);
    expect(app().querySelector('[data-screen="end"]')).not.toBeNull();
    expect(app().textContent).toContain("Cleaned up.");
    click('[data-action="done"]');
    await flush();
    expect(app().querySelector('[data-screen="home"]')).not.toBeNull();
    push(consoleIn({ phase: "listening", code: "6RRR-2H8F", address: "10.0.0.42:7000" }));
    push(consoleIn({ phase: "full_control" }, { session: true, target: true }));
    await flush();
    expect(app().querySelectorAll(".feed > li")).toHaveLength(0);
  });

  it("shows the exact command before connecting a client, then installs it", async () => {
    let installed: unknown;
    await boot({
      ai_clients: () => CLIENTS,
      ai_client_install: (args) => {
        installed = args;
        return CLIENTS.map((c) =>
          c.id === "gemini"
            ? { ...c, registration: { state: "this_executable", command: "x" } }
            : c,
        );
      },
    });
    click('[data-action="ai-connect"][data-arg="gemini"]');
    await flush();
    expect(app().querySelector('[data-screen="ai"]')).not.toBeNull();
    expect(app().querySelector(".confirm pre")?.textContent).toBe(CLIENTS[2]?.installPlan[0]);
    expect(installed).toBeUndefined();
    click('[data-action="ai-confirm"]');
    await flush();
    expect(installed).toEqual({ client: "gemini" });
    expect(app().querySelector(".confirm")).toBeNull();
    expect(app().querySelector('[aria-live="polite"]')?.textContent).toBe(
      "Gemini CLI is connected.",
    );
  });

  it("states a failed command in an assertive alert with a next step", async () => {
    await boot({
      start_live_session: () => {
        throw { stage: "listen", message: "The address is already in use" };
      },
    });
    click('[data-action="start"]');
    await flush();
    const alert = app().querySelector('[role="alert"]')?.textContent ?? "";
    expect(alert).toContain("The session could not start");
    expect(alert).toContain("The address is already in use (listen)");
    expect(alert).toContain("try again");
  });

  it("opens the audit log of a session", async () => {
    await boot({ list_sessions: () => SESSIONS, open_audit: () => undefined });
    click(`[data-action="open-report"][data-arg="${SESSIONS[1]?.id}"]`);
    await flush();
    click('[data-action="open-audit"]');
    await flush();
    expect(backend.calls).toContainEqual(["open_audit", { session: SESSIONS[1]?.id }]);
  });

  it("shows this session's audit records in the Audit tab and refreshes them as the feed changes", async () => {
    let entries = AUDIT.slice(0, 3);
    await boot({ get_session_audit: () => entries });
    push(consoleIn({ phase: "listening", code: "6RRR-2H8F", address: "10.0.0.42:7000" }));
    push(consoleIn({ phase: "full_control" }, { session: true, target: true }));
    await flush();
    click('[data-action="tab"][data-arg="audit"]');
    await flush();
    expect(app().querySelectorAll("#panel-audit tbody tr")).toHaveLength(3);
    // Let the refresh the phase changes scheduled run out first.
    await new Promise((resolve) => setTimeout(resolve, 1100));
    await flush();
    // The feed changes: one refresh, throttled.
    vi.useFakeTimers();
    try {
      entries = AUDIT;
      const asked = () => backend.calls.filter(([cmd]) => cmd === "get_session_audit").length;
      const before = asked();
      for (const entry of FEED) if (entry.entry === "item") activity(entry);
      expect(asked()).toBe(before);
      await vi.advanceTimersByTimeAsync(1100);
      expect(asked()).toBe(before + 1);
    } finally {
      vi.useRealTimers();
    }
    await flush();
    expect(app().querySelectorAll("#panel-audit tbody tr")).toHaveLength(AUDIT.length);
  });

  it("ignores activity events in an older format", async () => {
    await boot();
    push(consoleIn({ phase: "listening", code: "6RRR-2H8F", address: "10.0.0.42:7000" }));
    push(consoleIn({ phase: "full_control" }, { session: true, target: true }));
    backend.listeners["console-activity"]?.({
      payload: { source: "claude", kind: "start", text: "x" },
    });
    await flush();
    expect(app().querySelectorAll(".feed > li")).toHaveLength(0);
  });
});

describe("the network to wait on (ST-M009-016)", () => {
  const listening = (address = "10.0.0.42:7000", code = "6RRR-2H8F"): ConsoleState =>
    consoleIn({ phase: "listening", code, address }, { displayName: "Sam" });
  const asked = (cmd: string): number => backend.calls.filter(([name]) => name === cmd).length;
  const escape = (): void => {
    document.dispatchEvent(new KeyboardEvent("keydown", { key: "Escape" }));
  };

  it("asks which networks this computer has when it starts to wait, and names the adapter", async () => {
    await boot({ get_listen_networks: () => NETWORKS });
    expect(asked("get_listen_networks")).toBe(0);
    push(listening());
    await flush();
    expect(asked("get_listen_networks")).toBe(1);
    expect(app().querySelector(".conn-addr")?.textContent).toContain("Wi-Fi · 10.0.0.42:7000");
    // The same waiting state again is no reason to ask again.
    push(listening());
    await flush();
    expect(asked("get_listen_networks")).toBe(1);
  });

  it("shows the address alone when the backend does not list networks", async () => {
    await boot();
    push(listening());
    await flush();
    expect(app().querySelector(".conn-addr")?.textContent).toContain("10.0.0.42:7000");
    expect(app().querySelector(".net-adapter")).toBeNull();
    expect(app().querySelector("#net-toggle")).toBeNull();
  });

  it("moves the waiting session to the chosen network and announces the new code", async () => {
    await boot({
      get_listen_networks: () => NETWORKS,
      switch_listen_network: () => {
        state = listening("192.168.1.20:7001", "7SSS-3J9G");
        return state;
      },
    });
    push(listening());
    await flush();
    click("#net-toggle");
    await flush();
    expect(app().querySelector("#net-toggle")?.getAttribute("aria-expanded")).toBe("true");
    expect(app().querySelectorAll("#net-list li")).toHaveLength(2);

    click('[data-action="network"][data-arg="192.168.1.20"]');
    await flush();
    expect(backend.calls.find(([cmd]) => cmd === "switch_listen_network")?.[1]).toEqual({
      address: "192.168.1.20",
    });
    expect(app().querySelector(".conn-code")?.textContent).toBe("7SSS-3J9G");
    expect(app().querySelector(".conn-addr")?.textContent).toContain("VPN · 192.168.1.20:7001");
    expect(app().querySelector("#net-list")).toBeNull();
    expect(app().querySelector("#announce")?.textContent).toBe(
      "Now waiting on VPN. The connection code changed.",
    );

    // The list now offers the network that was left.
    click("#net-toggle");
    await flush();
    const offered = Array.from(app().querySelectorAll("#net-list .net-name")).map(
      (name) => name.textContent,
    );
    expect(offered).toEqual(["Wi-Fi · 10.0.0.42", "vEthernet (Default Switch) · 172.20.0.1"]);
  });

  it("keeps the waiting screen, without a code, while the network changes", async () => {
    let finish: (next: ConsoleState) => void = () => undefined;
    await boot({
      get_listen_networks: () => NETWORKS,
      switch_listen_network: () =>
        new Promise<ConsoleState>((resolve) => {
          finish = resolve;
        }),
    });
    push(listening());
    await flush();
    click("#net-toggle");
    await flush();
    click('[data-action="network"][data-arg="172.20.0.1"]');
    await flush();
    // The backend stops the old listener before it starts the new one.
    push(consoleIn({ phase: "stopped", reason: "technician_requested" }));
    await flush();
    expect(app().querySelector(".screen.wait")).not.toBeNull();
    expect(app().querySelector(".conn-switching")?.textContent).toBe("Changing the network…");
    expect(app().querySelector(".conn-code")).toBeNull();

    const moved = listening("172.20.0.1:7002", "8TTT-4K2H");
    push(moved);
    finish(moved);
    await flush();
    expect(app().querySelector(".conn-code")?.textContent).toBe("8TTT-4K2H");
    expect(app().querySelector(".conn-addr")?.textContent).toContain(
      "vEthernet (Default Switch) · 172.20.0.1:7002",
    );
  });

  it("does not step back when a customer connects before the answer arrives", async () => {
    let finish: (next: ConsoleState) => void = () => undefined;
    await boot({
      get_listen_networks: () => NETWORKS,
      switch_listen_network: () =>
        new Promise<ConsoleState>((resolve) => {
          finish = resolve;
        }),
    });
    push(listening());
    await flush();
    click("#net-toggle");
    await flush();
    click('[data-action="network"][data-arg="172.20.0.1"]');
    await flush();
    const moved = listening("172.20.0.1:7002", "8TTT-4K2H");
    push(consoleIn({ phase: "stopped", reason: "technician_requested" }));
    push(moved);
    push(consoleIn({ phase: "connecting" }, { session: true }));
    finish(moved);
    await flush();
    expect(app().textContent).toContain("The customer is connecting");
    expect(app().querySelector(".conn-code")).toBeNull();
  });

  it("says so when the network cannot be changed, and shows what the backend says now", async () => {
    await boot({
      get_listen_networks: () => NETWORKS,
      switch_listen_network: () => {
        throw {
          stage: "network",
          message:
            "this computer no longer has that address; choose one of the networks listed",
        };
      },
    });
    push(listening());
    await flush();
    click("#net-toggle");
    await flush();
    click('[data-action="network"][data-arg="172.20.0.1"]');
    await flush();
    const alert = app().querySelector('[role="alert"]')?.textContent ?? "";
    expect(alert).toContain("The network could not be changed");
    expect(alert).toContain("no longer has that address");
    expect(alert).toContain("(network)");
    // Still waiting on the old address, as `get_state` says.
    expect(app().querySelector(".conn-code")?.textContent).toBe("6RRR-2H8F");
    // The networks were read again, in case one went away.
    expect(asked("get_listen_networks")).toBeGreaterThanOrEqual(2);
  });

  it("closes the open list with Escape before Escape stops the session", async () => {
    await boot({ get_listen_networks: () => NETWORKS });
    push(listening());
    await flush();
    click("#net-toggle");
    await flush();
    expect(app().querySelector("#net-list")).not.toBeNull();

    escape();
    await flush();
    expect(app().querySelector("#net-list")).toBeNull();
    // Still waiting: this Escape closed the list and did not stop the session.
    expect(app().querySelector(".conn-code")?.textContent).toBe("6RRR-2H8F");
    expect(asked("stop_session")).toBe(0);

    // Now Escape stops, as it always does (REQ-F-018).
    escape();
    await flush();
    expect(asked("stop_session")).toBe(1);
    expect(app().querySelector(".conn-code")).toBeNull();
  });
});

describe("a session through the relay (ST-M007-013)", () => {
  const listening = (): ConsoleState =>
    consoleIn(
      { phase: "listening", code: "6RRR-2H8F", address: "10.0.0.42:7000" },
      { displayName: "Sam" },
    );
  const onRelay = (admitted: boolean, code = RELAY_CODE): ConsoleState =>
    consoleIn(
      { phase: "listening", code, address: "relay.example.org:443" },
      { displayName: "Sam", relay: { ...VIA_RELAY, admitted } },
    );
  const asked = (cmd: string): number => backend.calls.filter(([name]) => name === cmd).length;
  const escape = (): void => {
    document.dispatchEvent(new KeyboardEvent("keydown", { key: "Escape" }));
  };
  const alert = (): string => app().querySelector('[role="alert"]')?.textContent ?? "";

  it("reads which relay this copy uses when the waiting screen opens, and offers it", async () => {
    let relay = RELAY_NONE;
    await boot({ get_relay_settings: () => relay, get_listen_networks: () => NETWORKS });
    expect(asked("get_relay_settings")).toBe(1);
    // A profile was set up after CretLink started.
    relay = RELAY_STORED;
    push(listening());
    await flush();
    expect(asked("get_relay_settings")).toBe(2);
    const use = app().querySelector<HTMLButtonElement>("#use-relay");
    expect(use?.textContent).toBe("Use relay…");
    expect(app().querySelector("#relay-offer")?.textContent).toContain("relay.example.org");
    // The same waiting state again is no reason to ask again.
    push(listening());
    await flush();
    expect(asked("get_relay_settings")).toBe(2);
  });

  it("changes the wait to the relay: no code in between, then the relay code and the relay's state", async () => {
    let finish: (next: ConsoleState) => void = () => undefined;
    await boot({
      get_relay_settings: () => RELAY_STORED,
      get_listen_networks: () => NETWORKS,
      use_relay: () =>
        new Promise<ConsoleState>((resolve) => {
          finish = resolve;
        }),
    });
    push(listening());
    await flush();
    app().querySelector<HTMLElement>("#use-relay")?.focus();
    click("#use-relay");
    await flush();
    expect(asked("use_relay")).toBe(1);
    expect(backend.calls.find(([cmd]) => cmd === "use_relay")?.[1]).toBeUndefined();
    // The backend stops the listener of this network first.
    push(consoleIn({ phase: "stopped", reason: "technician_requested" }));
    await flush();
    expect(app().querySelector(".screen.wait")).not.toBeNull();
    expect(app().querySelector(".conn-switching")?.textContent).toBe("Changing to your relay…");
    expect(app().querySelector(".conn-code")).toBeNull();
    expect(app().querySelector(".relay-code")).toBeNull();
    // A second press while it changes does nothing.
    expect(app().querySelector("#use-relay")).toBeNull();

    // The answer: waiting on the relay, which is still being reached.
    finish(onRelay(false));
    await flush();
    expect(app().querySelector("#relay-code")?.textContent).toBe("53STA71DJKKV0VGH");
    expect(app().querySelector("#relay-host")?.textContent).toBe("relay.example.org");
    expect(app().querySelector("#relay-state")?.textContent).toBe(
      "Connecting to the relay relay.example.org…",
    );
    // The button that was pressed is gone: the keyboard is on Cancel.
    expect(document.activeElement?.id).toBe("stop");
    expect(alert()).toBe("");

    // The relay holds the code.
    push(onRelay(true));
    await flush();
    expect(app().querySelector("#relay-state")?.textContent).toContain("The relay holds this code.");
    expect(app().querySelector("#announce")?.textContent).toBe(
      "Waiting for the customer on the relay relay.example.org. Read them the relay code. No access.",
    );
    // Nothing about networks was asked for a wait on the relay.
    expect(asked("get_listen_networks")).toBe(1);
  });

  it("does not step back when the relay answers before the command does", async () => {
    let finish: (next: ConsoleState) => void = () => undefined;
    await boot({
      get_relay_settings: () => RELAY_STORED,
      use_relay: () =>
        new Promise<ConsoleState>((resolve) => {
          finish = resolve;
        }),
    });
    push(listening());
    await flush();
    click("#use-relay");
    await flush();
    push(consoleIn({ phase: "stopped", reason: "technician_requested" }));
    push(onRelay(false));
    push(onRelay(true));
    // The command's answer is the older state.
    finish(onRelay(false));
    await flush();
    expect(app().querySelector("#relay-state")?.textContent).toContain("The relay holds this code.");
  });

  it("shows a relay that cannot be reached with its own words, although the command answered first", async () => {
    const failed = consoleIn(
      {
        phase: "failed",
        stage: "relay_unreachable",
        message: "CretLink could not connect to the relay at relay.example.org:443.",
        next: "Check that the relay is running.",
      },
      { relay: { ...VIA_RELAY, admitted: false } },
    );
    await boot({
      get_relay_settings: () => RELAY_STORED,
      use_relay: () => {
        state = onRelay(false);
        return state;
      },
    });
    push(listening());
    await flush();
    click("#use-relay");
    await flush();
    expect(app().querySelector("#relay-code")).not.toBeNull();
    push(failed);
    await flush();
    // No session existed: back where a session starts, with the reason.
    expect(app().querySelector("#start-session")).not.toBeNull();
    expect(alert()).toContain("The relay cannot be reached");
    expect(alert()).toContain("could not connect to the relay at relay.example.org:443");
    expect(alert()).toContain("Check that the relay is running.");
    expect(alert()).not.toContain("The session failed");
    expect(app().querySelector(".relay-code")).toBeNull();
    // Nothing is tried again by itself.
    expect(asked("use_relay")).toBe(1);
    expect(asked("start_live_session")).toBe(0);
  });

  it("says why the relay was not used when the backend refuses, and keeps waiting on this network", async () => {
    await boot({
      get_relay_settings: () => RELAY_STORED,
      use_relay: () => {
        throw {
          stage: "relay_profile",
          message: "Line 3: `address` needs a port, such as relay.example.org:443.",
        };
      },
    });
    state = listening();
    push(state);
    await flush();
    click("#use-relay");
    await flush();
    expect(alert()).toContain("The relay was not used");
    expect(alert()).toContain("Line 3: `address` needs a port");
    expect(alert()).toContain("Open Settings, under Relay");
    // The session on this network is what the backend still says it is.
    expect(app().querySelector(".conn-code")?.textContent).toBe("6RRR-2H8F");
    // The relay of this copy was read again: it may have changed.
    expect(asked("get_relay_settings")).toBeGreaterThanOrEqual(3);
  });

  it("cancels the wait on the relay with Escape and with Cancel", async () => {
    await boot({ get_relay_settings: () => RELAY_STORED });
    push(onRelay(true));
    await flush();
    expect(app().querySelector("#relay-code")).not.toBeNull();
    escape();
    await flush();
    expect(asked("stop_session")).toBe(1);
    expect(app().querySelector(".relay-code")).toBeNull();
    expect(app().querySelector("#start-session")).not.toBeNull();

    push(onRelay(true, "6TTT-A71D-JKKV-0VGH"));
    await flush();
    click("#stop");
    await flush();
    expect(asked("stop_session")).toBe(2);
    expect(app().querySelector(".relay-code")).toBeNull();
  });

  it("copies the relay code and says so by its name", async () => {
    const written: string[] = [];
    Object.defineProperty(navigator, "clipboard", {
      configurable: true,
      value: { writeText: async (text: string) => void written.push(text) },
    });
    await boot({ get_relay_settings: () => RELAY_STORED });
    push(onRelay(true));
    await flush();
    click('[data-action="copy"]');
    await flush();
    expect(written).toEqual([RELAY_CODE]);
    expect(app().querySelector("#announce")?.textContent).toBe("Relay code copied.");
    // On one network it is the connection code that was copied.
    push(listening());
    await flush();
    click('[data-action="copy"]');
    await flush();
    expect(written).toEqual([RELAY_CODE, "6RRR-2H8F"]);
    expect(app().querySelector("#announce")?.textContent).toBe("Connection code copied.");
    Reflect.deleteProperty(navigator, "clipboard");
  });

  it("goes from the relay code to the approval code, consent and control as on one network", async () => {
    await boot({
      get_relay_settings: () => RELAY_STORED,
      confirm_code: () => state,
    });
    push(onRelay(true));
    await flush();
    state = consoleIn(
      { phase: "awaiting_approval_code", code: "482913" },
      { session: true, target: true, relay: VIA_RELAY },
    );
    push(state);
    await flush();
    expect(app().querySelector(".sac")?.getAttribute("aria-label")).toBe("Approval code 482 913");
    expect(app().querySelector(".relay-code")).toBeNull();
    expect(app().querySelector(".via-relay")?.textContent).toContain("relay.example.org");
    click('[data-action="compare"]');
    await flush();
    expect(asked("confirm_code")).toBe(1);
    push(
      consoleIn({ phase: "awaiting_consent" }, { session: true, target: true, relay: VIA_RELAY }),
    );
    await flush();
    expect(app().textContent).toContain("Only the customer can grant control.");
    push(consoleIn({ phase: "full_control" }, { session: true, target: true, relay: VIA_RELAY }));
    await flush();
    expect(app().querySelector("#control-band")?.textContent).toContain("FULL CONTROL");
    expect(app().querySelector("#session-relay")?.textContent).toContain("relay.example.org");
    // Escape stops, as in every session.
    escape();
    await flush();
    expect(asked("stop_session")).toBe(1);
  });

  it("offers Open Settings when this copy has no relay, and comes back to the waiting screen", async () => {
    await boot({ get_relay_settings: () => RELAY_NONE });
    push(listening());
    await flush();
    expect(app().querySelector("#use-relay")).toBeNull();
    click('#relay-offer [data-action="nav"]');
    await flush();
    expect(app().querySelector("#relay-head")?.textContent).toBe("Relay");
    // The session keeps waiting meanwhile, and Home shows it again.
    expect(asked("stop_session")).toBe(0);
    click('nav.side [data-arg="home"]');
    await flush();
    expect(app().querySelector(".conn-code")?.textContent).toBe("6RRR-2H8F");
  });
});

describe("Settings → Relay (ST-M007-011)", () => {
  const asked = (cmd: string): number => backend.calls.filter(([name]) => name === cmd).length;
  const sent = (cmd: string): Record<string, unknown> | undefined =>
    backend.calls.filter(([name]) => name === cmd).at(-1)?.[1];
  const escape = (): void => {
    document.dispatchEvent(new KeyboardEvent("keydown", { key: "Escape" }));
  };
  const input = (id: string): HTMLInputElement => {
    const found = app().querySelector<HTMLInputElement>(`#${id}`);
    if (!found) throw new Error(`no #${id}`);
    return found;
  };
  const type = (id: string, value: string): void => {
    input(id).value = value;
  };
  const alert = (): string => app().querySelector('[role="alert"]')?.textContent ?? "";
  const announced = (): string => app().querySelector("#announce")?.textContent ?? "";
  // A sample value that no scanner takes for a real key.
  const KEY = "ab".repeat(32);

  /** Starts with `relay` as what the backend holds, and opens Settings. */
  async function settings(
    relay: unknown = RELAY_STORED,
    handlers: Record<string, Handler> = {},
  ): Promise<void> {
    await boot({ get_relay_settings: () => relay, ...handlers });
    click('nav.side [data-arg="settings"]');
    await flush();
  }

  it("asks which relay this copy uses on start, and again when Settings opens", async () => {
    await settings();
    expect(asked("get_relay_settings")).toBe(2);
    expect(input("relay-address").value).toBe("relay.example.org:443");
  });

  it("says that the relay is not available when the backend does not offer it", async () => {
    await boot();
    click('nav.side [data-arg="settings"]');
    await flush();
    expect(app().textContent).toContain("The relay of this copy is not available yet.");
    expect(app().querySelector("#relay-address")).toBeNull();
  });

  it("saves the form and keeps the kept key when its field is left empty", async () => {
    const saved = {
      ...RELAY_STORED,
      stored: { ...RELAY_STORED.stored, relay: { ...RELAY, address: "host-01.example.org:9970" } },
    };
    await settings(RELAY_STORED, { save_relay: () => saved });
    type("relay-address", "  host-01.example.org:9970 ");
    click('[data-action="relay-save"]');
    await flush();
    expect(sent("save_relay")).toEqual({
      form: { address: "host-01.example.org:9970", keyMode: "keep", caMode: "none" },
    });
    expect(announced()).toBe("The relay relay.example.org was saved.");
    expect(input("relay-address").value).toBe("host-01.example.org:9970");
    expect(alert()).toBe("");
    // The form was drawn anew; the keyboard is where it was, on Save.
    expect((document.activeElement as HTMLElement | null)?.dataset.action).toBe("relay-save");
  });

  it("sends a typed key to the backend and keeps none of it in the window", async () => {
    await settings(RELAY_NONE, { save_relay: () => RELAY_STORED });
    type("relay-address", "relay.example.org:443");
    type("relay-key", KEY);
    type("relay-name", " relay.example.org ");
    click('[data-action="relay-save"]');
    await flush();
    expect(sent("save_relay")).toEqual({
      form: {
        address: "relay.example.org:443",
        keyMode: "set",
        key: KEY,
        caMode: "none",
        name: "relay.example.org",
      },
    });
    // The form is drawn anew: the key field is empty, and the key is nowhere.
    expect(input("relay-key").value).toBe("");
    expect(app().innerHTML).not.toContain(KEY);
    expect(app().textContent).toContain("A key is set. It is never shown.");
  });

  it("uses no key when the field is empty and nothing is kept, or when asked to remove it", async () => {
    await settings(RELAY_NONE, { save_relay: () => RELAY_NONE });
    type("relay-address", "relay.example.org:443");
    click('[data-action="relay-save"]');
    await flush();
    expect(sent("save_relay")).toEqual({
      form: { address: "relay.example.org:443", keyMode: "none", caMode: "none" },
    });

    document.body.innerHTML = '<div id="app" aria-busy="true"></div>';
    vi.resetModules();
    backend.calls = [];
    await settings(RELAY_STORED, { save_relay: () => RELAY_STORED });
    input("relay-key-remove").click();
    click('[data-action="relay-save"]');
    await flush();
    expect((sent("save_relay")?.form as { keyMode: string }).keyMode).toBe("none");
  });

  it("names the setting a refusal is about, moves the focus there and keeps what was typed", async () => {
    await settings(RELAY_STORED, {
      save_relay: () => {
        throw {
          stage: "relay_address",
          message:
            "`address` needs a port; write the relay's host name and port, like relay.example.org:443.",
        };
      },
    });
    type("relay-address", "relay.example.org");
    type("relay-key", KEY);
    click('[data-action="relay-save"]');
    await flush();
    expect(alert()).toContain("The relay was not saved");
    expect(alert()).toContain("`address` needs a port;");
    expect(alert()).toContain("Correct it, then try again.");
    // The sentence stands alone, without the name of a stage.
    expect(alert()).not.toContain("relay_address");
    expect(document.activeElement?.id).toBe("relay-address");
    expect(input("relay-address").value).toBe("relay.example.org");
    expect(input("relay-key").value).toBe(KEY);
  });

  it("asks for the key of another relay instead of taking the kept one there", async () => {
    await settings(RELAY_STORED, {
      save_relay: () => {
        throw {
          stage: "relay_key",
          message:
            "The key that is kept is the key of the relay relay.example.org; enter the key of this relay, or remove the key.",
        };
      },
    });
    type("relay-address", "host-01.example.org:443");
    click('[data-action="relay-save"]');
    await flush();
    expect(sent("save_relay")?.form).toMatchObject({ keyMode: "keep" });
    expect(alert()).toContain("The relay was not saved");
    expect(alert()).toContain("is the key of the relay relay.example.org");
    // The focus is where the key of the other relay goes.
    expect(document.activeElement?.id).toBe("relay-key");
  });

  it("tests the form as it stands and shows the result in words, without saving", async () => {
    let answer: (value: unknown) => void = () => undefined;
    await settings(RELAY_STORED, {
      test_relay: () =>
        new Promise((resolve) => {
          answer = resolve;
        }),
    });
    type("relay-address", "host-01.example.org:9970");
    click('[data-action="relay-test"]');
    await flush();
    expect(sent("test_relay")).toEqual({
      form: { address: "host-01.example.org:9970", keyMode: "keep", caMode: "none" },
    });
    expect(app().querySelector("#relay-test")?.textContent).toBe("Reaching the relay…");
    // The button waits for the answer: a second press starts no second test.
    // It keeps the focus meanwhile, so it is marked and not disabled.
    const test = app().querySelector<HTMLButtonElement>('[data-action="relay-test"]');
    expect(test?.getAttribute("aria-disabled")).toBe("true");
    expect(test?.disabled).toBe(false);
    test?.focus();
    click('[data-action="relay-test"]');
    await flush();
    expect(asked("test_relay")).toBe(1);

    answer({
      ok: false,
      kind: "unreachable",
      message: "CretLink could not connect to the relay at host-01.example.org:9970.",
      next: "Check that the relay is running.",
    });
    await flush();
    const region = app().querySelector("#relay-test")?.textContent ?? "";
    expect(region).toContain("The relay cannot be used");
    expect(region).toContain("CretLink could not connect to the relay at host-01.example.org:9970.");
    expect(region).toContain("Check that the relay is running.");
    // A relay that cannot be reached is the result of the test, not a failed command.
    expect(alert()).toBe("");
    expect(asked("test_relay")).toBe(1);
    expect(asked("save_relay")).toBe(0);
    // What was typed is still there to be corrected.
    expect(input("relay-address").value).toBe("host-01.example.org:9970");
    // The button is available again, and the keyboard never left it.
    expect(test?.isConnected).toBe(true);
    expect(test?.hasAttribute("aria-disabled")).toBe(false);
    expect(document.activeElement).toBe(test);
  });

  it("drops the result of a test when the relay was removed meanwhile", async () => {
    let answer: (value: unknown) => void = () => undefined;
    await settings(RELAY_STORED, {
      test_relay: () =>
        new Promise((resolve) => {
          answer = resolve;
        }),
      remove_relay: () => RELAY_NONE,
    });
    click('[data-action="relay-test"]');
    await flush();
    click('[data-action="relay-remove"]');
    await flush();
    click('[data-action="relay-remove-confirm"]');
    await flush();
    expect(announced()).toBe("The relay was removed from the settings.");
    // The answer comes late: it is about a relay that is no longer there.
    answer({ ok: true, kind: "admitted", message: "Admitted.", next: "" });
    await flush();
    expect(app().querySelector("#relay-test")?.textContent).toBe("");
  });

  it("chooses the certificate file through the backend and names it by its number", async () => {
    let picked: unknown = null;
    await settings(RELAY_STORED, {
      choose_relay_ca: () => picked,
      save_relay: () => ({
        ...RELAY_STORED,
        stored: { ...RELAY_STORED.stored, relay: RELAY_PRIVATE },
      }),
    });
    // The dialog was closed: nothing changes.
    click('[data-action="relay-ca-choose"]');
    await flush();
    expect(app().querySelector("#relay-ca")?.textContent).toBe(
      "The certificate authorities of this computer",
    );

    picked = { pick: 7, name: "authority.pem" };
    click('[data-action="relay-ca-choose"]');
    await flush();
    expect(app().querySelector("#relay-ca")?.textContent).toBe(
      "authority.pem · chosen, not saved yet",
    );
    expect(announced()).toBe("authority.pem is chosen. It is kept when you save.");
    click('[data-action="relay-save"]');
    await flush();
    expect(sent("save_relay")?.form).toMatchObject({ caMode: "chosen", caPick: 7 });
    // Saved: the kept file is named, and the next save leaves it alone.
    expect(app().querySelector("#relay-ca")?.textContent).toBe(
      "relay-ca.pem · kept with your settings",
    );
    click('[data-action="relay-save"]');
    await flush();
    expect(sent("save_relay")?.form).toMatchObject({ caMode: "keep" });
    expect((sent("save_relay")?.form as Record<string, unknown>).caPick).toBeUndefined();

    // Back to the authorities of this computer. The button that was pressed
    // goes away with the file; the focus moves to its neighbour.
    click('[data-action="relay-ca-none"]');
    await flush();
    expect(document.activeElement?.id).toBe("relay-ca-choose");
    click('[data-action="relay-save"]');
    await flush();
    expect(sent("save_relay")?.form).toMatchObject({ caMode: "none" });
  });

  it("says why a chosen file cannot be used", async () => {
    await settings(RELAY_STORED, {
      choose_relay_ca: () => {
        throw { stage: "relay_ca", message: "The certificate file holds no certificate." };
      },
    });
    click('[data-action="relay-ca-choose"]');
    await flush();
    expect(alert()).toContain("That certificate file cannot be used");
    expect(alert()).toContain("The certificate file holds no certificate.");
    expect(document.activeElement?.id).toBe("relay-ca-choose");
  });

  it("asks before it removes the relay; Cancel and Escape leave it", async () => {
    await settings(RELAY_STORED, { remove_relay: () => RELAY_NONE });
    click('[data-action="relay-remove"]');
    await flush();
    expect(app().querySelector("#relay-remove-step")).not.toBeNull();
    // The safe answer has the focus.
    expect(document.activeElement?.textContent).toBe("Cancel");
    escape();
    await flush();
    expect(app().querySelector("#relay-remove-step")).toBeNull();
    expect(asked("remove_relay")).toBe(0);
    // The focus is back on what opened the question.
    expect((document.activeElement as HTMLElement | null)?.dataset.action).toBe("relay-remove");

    click('[data-action="relay-remove"]');
    await flush();
    click('[data-action="relay-remove-cancel"]');
    await flush();
    expect(asked("remove_relay")).toBe(0);
    expect(input("relay-address").value).toBe("relay.example.org:443");
    expect((document.activeElement as HTMLElement | null)?.dataset.action).toBe("relay-remove");

    click('[data-action="relay-remove"]');
    await flush();
    click('[data-action="relay-remove-confirm"]');
    await flush();
    expect(asked("remove_relay")).toBe(1);
    expect(announced()).toBe("The relay was removed from the settings.");
    expect(input("relay-address").value).toBe("");
    expect(app().querySelector('[data-action="relay-remove"]')).toBeNull();
    // The question is gone; the keyboard goes on to the empty form.
    expect(document.activeElement?.id).toBe("relay-address");
  });

  it("tests the relay of the file next to the program, which has no form", async () => {
    await settings(RELAY_FILE, {
      test_relay: () => ({ ok: true, kind: "admitted", message: "Admitted.", next: "" }),
      remove_relay: () => ({ ...RELAY_FILE, stored: { path: RELAY_FILE.stored?.path ?? "" } }),
    });
    expect(app().textContent).toContain("That file wins over what Settings holds.");
    click('[data-action="relay-test"]');
    await flush();
    // Without a form: the backend tests the relay this copy uses.
    expect(sent("test_relay")).toEqual({});
    expect(app().querySelector("#relay-test")?.textContent).toContain("Admitted.");
    expect(asked("save_relay")).toBe(0);

    // The relay of the settings can still be removed.
    click('[data-action="relay-remove"]');
    await flush();
    click('[data-action="relay-remove-confirm"]');
    await flush();
    expect(asked("remove_relay")).toBe(1);
    expect(app().querySelector(".relay-unused")).toBeNull();
    // No form to go to: the focus is on the test of the relay in use.
    expect((document.activeElement as HTMLElement | null)?.dataset.action).toBe("relay-test");
  });

  it("keeps nothing in a copy that runs once", async () => {
    await settings(RELAY_RUN_ONCE);
    expect(app().textContent).toContain("This copy runs once and keeps no settings.");
    expect(app().querySelector('[data-action="relay-save"]')).toBeNull();
    expect(asked("save_relay")).toBe(0);
  });

  it("shows what the backend has to say in the alert, with its next step", async () => {
    await boot({ get_relay_settings: () => RELAY_NONE });
    backend.listeners["console-notice"]?.({
      payload: {
        title: "The network choice could not be saved",
        message: "The settings could not be written: disk full.",
        next: "It holds until CretLink closes.",
      },
    });
    await flush();
    expect(alert()).toContain("The network choice could not be saved");
    expect(alert()).toContain("The settings could not be written: disk full.");
    expect(alert()).toContain("It holds until CretLink closes.");
    // Anything else on that channel is not shown.
    click('[data-action="dismiss-error"]');
    await flush();
    backend.listeners["console-notice"]?.({ payload: { title: 1 } });
    await flush();
    expect(alert()).toBe("");
  });
});

describe("Settings: what uninstall leaves in place (ST-M007-011)", () => {
  const asked = (cmd: string): number => backend.calls.filter(([name]) => name === cmd).length;
  const alert = (): string => app().querySelector('[role="alert"]')?.textContent ?? "";
  const lines = (selector: string): string[] =>
    Array.from(app().querySelectorAll(selector)).map((line) => line.textContent ?? "");

  async function openUninstall(plan: unknown): Promise<void> {
    await boot({
      get_app_info: () => APP_INSTALLED,
      get_relay_settings: () => RELAY_FILE,
      uninstall_plan: () => plan,
    });
    click('nav.side [data-arg="settings"]');
    await flush();
    click('[data-action="uninstall-plan"]');
    await flush();
  }

  it("shows what goes and what stays before anything is removed", async () => {
    const stays =
      "The relay profile C:\\Users\\sam\\AppData\\Local\\Programs\\CretLink\\cretlink.toml, which CretLink did not write and does not remove. It holds the admission key of a relay in readable form: remove the file yourself when nobody else should be able to read that key";
    await openUninstall({ removes: ["C:\\Users\\sam\\AppData\\Local\\Programs\\CretLink"], stays: [stays] });
    expect(asked("uninstall_plan")).toBe(1);
    expect(lines(".plan:not(.stays) li")).toEqual(["C:\\Users\\sam\\AppData\\Local\\Programs\\CretLink"]);
    expect(lines(".plan.stays li")).toEqual([stays]);
    // Nothing was removed by looking, and the question is where the focus is.
    expect(asked("uninstall_app")).toBe(0);
    expect((document.activeElement as HTMLElement | null)?.id).toBe("uninstall-step");
  });

  it("says nothing about what stays when nothing does but the audit trail", async () => {
    await openUninstall({ removes: ["C:\\Users\\sam\\AppData\\Local\\Programs\\CretLink"], stays: [] });
    expect(app().querySelector(".plan.stays")).toBeNull();
    expect(app().textContent).toContain("The audit trail stays");
  });

  it("does not show a plan that it cannot read", async () => {
    // What an earlier backend answered: the lines to remove alone.
    await openUninstall(["C:\\Users\\sam\\AppData\\Local\\Programs\\CretLink"]);
    expect(app().querySelector("#uninstall-step")).toBeNull();
    expect(alert()).toContain("CretLink could not check what to uninstall");
    expect(alert()).toContain("Nothing was changed.");
    expect(asked("uninstall_app")).toBe(0);
  });
});

describe("Settings: make a customer copy (ST-M007-016)", () => {
  const asked = (cmd: string): number => backend.calls.filter(([name]) => name === cmd).length;
  const sent = (cmd: string): Record<string, unknown> | undefined =>
    backend.calls.filter(([name]) => name === cmd).at(-1)?.[1];
  const alert = (): string => app().querySelector('[role="alert"]')?.textContent ?? "";
  const announced = (): string => app().querySelector("#announce")?.textContent ?? "";
  const focused = (): string => (document.activeElement as HTMLElement | null)?.id ?? "";
  const input = (id: string): HTMLInputElement => {
    const found = app().querySelector<HTMLInputElement>(`#${id}`);
    if (!found) throw new Error(`no #${id}`);
    return found;
  };
  const type = (id: string, value: string): void => {
    input(id).value = value;
  };
  // A sample value that no scanner takes for a real key.
  const TYPED = "cd".repeat(16);
  const PROGRAM = { pick: 3, name: "cretlink.exe" };
  const PLACE = { pick: 4, name: "C:\\Users\\sam\\Documents\\Customers" };

  /** Settings with the step open, on a system where the program is chosen. */
  async function open(handlers: Record<string, Handler> = {}, options = COPY_OPTIONS_CHOOSE): Promise<void> {
    await boot({
      get_app_info: () => APP_INSTALLED,
      get_relay_settings: () => RELAY_STORED,
      customer_copy_options: () => options,
      choose_customer_program: () => PROGRAM,
      choose_customer_place: () => PLACE,
      generate_customer_key: () => ({ key: COPY_KEY }),
      make_customer_copy: () => COPY_MADE,
      show_customer_copy: () => null,
      ...handlers,
    });
    click('nav.side [data-arg="settings"]');
    await flush();
    click("#copy-open");
    await flush();
  }

  it("opens on the label, with the relay the backend says a copy would name", async () => {
    await open();
    expect(asked("customer_copy_options")).toBe(1);
    expect(app().querySelector("#copy-step")?.textContent).toContain("host-01.example.org");
    expect(focused()).toBe("copy-label");
    // The offer gave way to the step, and nothing was written by looking.
    expect(app().querySelector("#copy-open")).toBeNull();
    expect(asked("make_customer_copy")).toBe(0);
  });

  it("says why no copy can be made when this copy has no relay to name, and closes the step", async () => {
    await open({
      customer_copy_options: () => {
        throw { stage: "relay_profile", message: "Line 2: `address` needs a port. (C:\\cretlink.toml)" };
      },
    });
    expect(app().querySelector("#copy-step")).toBeNull();
    expect(alert()).toContain("A customer copy cannot be made");
    expect(alert()).toContain("Line 2: `address` needs a port.");
    expect(focused()).toBe("copy-open");
  });

  it("makes the copy with the typed key and the numbers of what was chosen, and shows the result", async () => {
    await open();
    type("copy-label", "acme-front-desk");
    type("copy-key", TYPED);
    click("#copy-program-choose");
    await flush();
    click("#copy-place-choose");
    await flush();
    expect(app().querySelector("#copy-program")?.textContent).toBe("cretlink.exe · chosen");
    expect(app().querySelector("#copy-place")?.textContent).toContain(PLACE.name);
    // What was typed stays in the fields while choices come in.
    expect(input("copy-label").value).toBe("acme-front-desk");
    expect(input("copy-key").value).toBe(TYPED);

    click("#copy-make");
    await flush();
    expect(sent("make_customer_copy")).toEqual({
      form: {
        label: "acme-front-desk",
        keyMode: "set",
        key: TYPED,
        programPick: 3,
        placePick: 4,
      },
    });
    // The result, where the keyboard and a screen reader land.
    expect(app().querySelector("#copy-folder")?.textContent).toBe(COPY_MADE.folder);
    expect(focused()).toBe("copy-step");
    expect(announced()).toBe(`The customer copy was written to ${COPY_MADE.folder}.`);
    // The typed key is in no field any more, and no key line is shown: none
    // was generated.
    expect(app().querySelector("#copy-key")).toBeNull();
    expect(app().querySelector("#copy-key-line")).toBeNull();
    expect(app().textContent).not.toContain(TYPED);

    // Show in folder names the copy by its number.
    click("#copy-show");
    await flush();
    expect(sent("show_customer_copy")).toEqual({ copy: 7 });
    // Done closes the step and offers the next copy.
    click('[data-action="copy-done"]');
    await flush();
    expect(app().querySelector("#copy-step")).toBeNull();
    expect(focused()).toBe("copy-open");
  });

  it("generates a key only for a label, shows its line once, and sends that key", async () => {
    const written: string[] = [];
    Object.defineProperty(navigator, "clipboard", {
      configurable: true,
      value: { writeText: async (text: string) => void written.push(text) },
    });
    try {
      await open({}, COPY_OPTIONS);
      // Without a label there is no line to show: nothing is asked.
      click("#copy-generate");
      await flush();
      expect(asked("generate_customer_key")).toBe(0);
      expect(alert()).toContain("Give the copy a label first");
      expect(focused()).toBe("copy-label");

      type("copy-label", "acme-front-desk");
      click("#copy-generate");
      await flush();
      expect(asked("generate_customer_key")).toBe(1);
      expect(alert()).toBe("");
      expect(app().querySelector("#copy-key-line")?.textContent).toBe(
        `acme-front-desk:${COPY_KEY}`,
      );
      // The keyboard is on Copy; the label is fixed and still says what was typed.
      expect((document.activeElement as HTMLElement | null)?.dataset.action).toBe("copy-key-line");
      expect(input("copy-label").hasAttribute("readonly")).toBe(true);
      expect(input("copy-label").value).toBe("acme-front-desk");
      click('[data-action="copy-key-line"]');
      await flush();
      expect(written).toEqual([`acme-front-desk:${COPY_KEY}`]);
      expect(announced()).toBe("The line for the key file was copied.");

      click("#copy-place-choose");
      await flush();
      click("#copy-make");
      await flush();
      expect(sent("make_customer_copy")).toEqual({
        form: { label: "acme-front-desk", keyMode: "set", key: COPY_KEY, placePick: 4 },
      });
      // The line once more with the result, and gone with Done.
      expect(app().querySelector("#copy-key-line")?.textContent).toBe(
        `acme-front-desk:${COPY_KEY}`,
      );
      click('[data-action="copy-done"]');
      await flush();
      expect(app().textContent).not.toContain(COPY_KEY);
    } finally {
      Reflect.deleteProperty(navigator, "clipboard");
    }
  });

  it("forgets a generated key when it is discarded or the page is left", async () => {
    await open({}, COPY_OPTIONS);
    type("copy-label", "acme");
    click("#copy-generate");
    await flush();
    expect(app().textContent).toContain(COPY_KEY);
    click("#copy-discard-key");
    await flush();
    expect(app().textContent).not.toContain(COPY_KEY);
    // The label can be changed again, and a key typed instead.
    expect(input("copy-label").hasAttribute("readonly")).toBe(false);
    expect(input("copy-label").value).toBe("acme");
    expect(focused()).toBe("copy-key");

    click("#copy-generate");
    await flush();
    expect(app().textContent).toContain(COPY_KEY);
    // Another page: the step does not wait there with the key in it.
    click('nav.side [data-arg="home"]');
    await flush();
    click('nav.side [data-arg="settings"]');
    await flush();
    expect(app().textContent).not.toContain(COPY_KEY);
    expect(app().querySelector("#copy-step")).toBeNull();
    expect(app().querySelector("#copy-open")).not.toBeNull();
  });

  it("makes a copy without a key when the relay asks for none", async () => {
    await open({}, COPY_OPTIONS);
    type("copy-label", "acme");
    type("copy-key", "left-in-the-field");
    input("copy-no-key").checked = true;
    click("#copy-place-choose");
    await flush();
    click("#copy-make");
    await flush();
    expect(sent("make_customer_copy")).toEqual({
      form: { label: "acme", keyMode: "none", placePick: 4 },
    });
  });

  it("asks before a folder that is there is replaced, and replaces only on that answer", async () => {
    let calls = 0;
    await open(
      {
        make_customer_copy: (args) => {
          calls += 1;
          const form = (args as { form: { replace?: boolean } }).form;
          if (!form.replace) {
            throw {
              stage: "copy_exists",
              message: `A folder ${COPY_MADE.folder} is there already. Nothing was changed.`,
            };
          }
          return COPY_MADE;
        },
      },
      COPY_OPTIONS,
    );
    type("copy-label", "acme-front-desk");
    type("copy-key", TYPED);
    click("#copy-place-choose");
    await flush();
    click("#copy-make");
    await flush();
    // The question, not an error; the keyboard is on the answer that keeps.
    expect(alert()).toBe("");
    expect(app().querySelector("#copy-replace-step")?.textContent).toContain("is there already");
    expect((document.activeElement as HTMLElement | null)?.dataset.action).toBe(
      "copy-replace-cancel",
    );
    // Keep it: nothing more is asked of the backend, and the form is back.
    click('[data-action="copy-replace-cancel"]');
    await flush();
    expect(calls).toBe(1);
    expect(app().querySelector("#copy-replace-step")).toBeNull();
    expect(focused()).toBe("copy-make");
    expect(input("copy-key").value).toBe(TYPED);

    // Asked again and answered with Replace: the same form, with the answer.
    click("#copy-make");
    await flush();
    click('[data-action="copy-replace"]');
    await flush();
    expect(calls).toBe(3);
    expect(sent("make_customer_copy")).toEqual({
      form: {
        label: "acme-front-desk",
        keyMode: "set",
        key: TYPED,
        placePick: 4,
        replace: true,
      },
    });
    expect(app().querySelector("#copy-folder")?.textContent).toBe(COPY_MADE.folder);
  });

  it.each([
    ["copy_label", "copy-label"],
    ["copy_key", "copy-key"],
    ["copy_program", "copy-program-choose"],
    ["copy_place", "copy-place-choose"],
  ])("puts the keyboard on the field a refusal is about (%s)", async (stage, field) => {
    await open({
      make_customer_copy: () => {
        throw { stage, message: "What the backend says about this field." };
      },
    });
    type("copy-label", "acme");
    click("#copy-make");
    await flush();
    expect(alert()).toContain("The customer copy was not made");
    expect(alert()).toContain("What the backend says about this field.");
    expect(alert()).toContain("Correct it, then try again.");
    expect(focused()).toBe(field);
    // The step is still open with what was typed.
    expect(input("copy-label").value).toBe("acme");
  });

  it("says when a copy could not be written, and that nothing was left behind", async () => {
    await open({
      make_customer_copy: () => {
        throw {
          stage: "copy_write",
          message:
            "The customer copy could not be written: the disk is full. Nothing of it was left behind.",
        };
      },
    });
    type("copy-label", "acme");
    click("#copy-make");
    await flush();
    expect(alert()).toContain("Nothing of it was left behind.");
    expect(alert()).toContain("Try again.");
    expect(app().querySelector("#copy-result")).toBeNull();
    expect(app().querySelector("#copy-make")).not.toBeNull();
  });

  it("keeps the step as it is when a dialog is closed without a choice, or a file is refused", async () => {
    await open({
      choose_customer_place: () => null,
      choose_customer_program: () => {
        throw {
          stage: "copy_program",
          message:
            "That file is not a Windows program. A customer copy holds cretlink.exe from the Windows archive of a CretLink release.",
        };
      },
    });
    click("#copy-place-choose");
    await flush();
    expect(app().querySelector("#copy-place")?.textContent).toBe("None chosen yet");
    expect(alert()).toBe("");
    click("#copy-program-choose");
    await flush();
    expect(alert()).toContain("That file cannot be the program of a copy");
    expect(alert()).toContain("That file is not a Windows program.");
    expect(app().querySelector("#copy-program")?.textContent).toBe("None chosen yet");
    expect(focused()).toBe("copy-program-choose");
  });

  it("closes the step when the relay it would name is saved anew or removed", async () => {
    await open({ remove_relay: () => RELAY_NONE }, COPY_OPTIONS);
    expect(app().querySelector("#copy-step")).not.toBeNull();
    click('[data-action="relay-remove"]');
    await flush();
    click('[data-action="relay-remove-confirm"]');
    await flush();
    expect(app().querySelector("#copy-step")).toBeNull();
    // Without a relay there is no copy to offer.
    expect(app().querySelector("#copy-open")).toBeNull();
  });
});
