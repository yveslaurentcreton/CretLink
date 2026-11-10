// Controller tests (ST-M009-008): commands and events in, rendered window out,
// with Tauri and the xterm terminal replaced by fakes.

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import type { ActivityEvent } from "../shared/contract";
import type { ConsoleState } from "./api";
import { AUDIT, CLIENTS, FEED, SESSIONS, SESSION_ID, consoleIn } from "./fixtures";

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

  it("keeps the feed across tab switches and the terminal alive across renders", async () => {
    await boot();
    push(consoleIn({ phase: "listening", code: "6RRR-2H8F", address: "10.0.0.42:7000" }));
    push(consoleIn({ phase: "full_control" }, { session: true, target: true }));
    await flush();
    for (const event of FEED) activity(event);
    await flush();
    expect(app().querySelectorAll(".feed > li")).toHaveLength(FEED.length);

    click("#tab-terminal");
    await flush();
    await new Promise((resolve) => requestAnimationFrame(() => resolve(undefined)));
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
    await new Promise((resolve) => requestAnimationFrame(() => resolve(undefined)));
    expect(app().querySelector(".fake-xterm")).toBe(xterm);
    expect(backend.attached).toHaveLength(1);
    expect(app().querySelectorAll(".feed > li")).toHaveLength(FEED.length);
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
