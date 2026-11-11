// Controller tests (ST-M009-009): commands and events in, rendered window
// out, with Tauri replaced by fakes.

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import type { ActivityEvent } from "../shared/contract";
import { type TimerTracker, trackTimers } from "../shared/test-timers";
import type { ConsentView, LaunchView } from "./api";
import { CONSOLES, FEED, LAUNCH_READY, REPORT, viewIn } from "./fixtures";

type Handler = (args?: Record<string, unknown>) => unknown;

const backend = vi.hoisted(() => ({
  handlers: {} as Record<string, Handler>,
  listeners: {} as Record<string, (event: { payload: unknown }) => void>,
  calls: [] as Array<[string, Record<string, unknown> | undefined]>,
}));

vi.mock("@tauri-apps/api/core", () => ({
  invoke: vi.fn(async (cmd: string, args?: Record<string, unknown>) => {
    backend.calls.push([cmd, args]);
    const handler = backend.handlers[cmd];
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

let view: ConsentView = viewIn({ phase: "idle" });
let launch: LaunchView = LAUNCH_READY;
// Cancels what a test leaves running, so it cannot call the fake during a later test.
let timers: TimerTracker;

function push(next: ConsentView): void {
  view = next;
  backend.listeners["agent-view"]?.({ payload: next });
}

function activity(event: ActivityEvent): void {
  backend.listeners["agent-activity"]?.({ payload: event });
}

const flush = async (): Promise<void> => {
  for (let i = 0; i < 6; i += 1) await new Promise((resolve) => setTimeout(resolve, 0));
};

function app(): HTMLElement {
  return document.getElementById("app") as HTMLElement;
}

function screen(): string | null {
  return app().firstElementChild?.getAttribute("data-screen") ?? null;
}

function click(selector: string): void {
  const el = app().querySelector<HTMLElement>(selector);
  if (!el) throw new Error(`no ${selector}`);
  el.click();
}

function called(cmd: string): Array<Record<string, unknown> | undefined> {
  return backend.calls.filter(([c]) => c === cmd).map(([, args]) => args);
}

function key(name: string, target: EventTarget = document): void {
  target.dispatchEvent(new KeyboardEvent("keydown", { key: name, bubbles: true }));
}

async function boot(handlers: Record<string, Handler> = {}, window: "main" | "mini" = "main"): Promise<void> {
  backend.handlers = {
    get_view: () => view,
    get_launch: () => launch,
    get_activity: () => [],
    browse_consoles: () => CONSOLES,
    stop_session: () => {
      view = viewIn({ phase: "stopped", reason: "customer_requested" });
      return view;
    },
    ...handlers,
  };
  const { start } = await import("./main");
  await start({ window });
  await flush();
}

beforeEach(() => {
  timers = trackTimers();
  vi.resetModules();
  backend.handlers = {};
  backend.listeners = {};
  backend.calls = [];
  view = viewIn({ phase: "idle" });
  launch = LAUNCH_READY;
  document.body.innerHTML = '<div id="app" aria-busy="true"></div>';
});

afterEach(() => {
  timers.clear();
  document.body.replaceChildren();
});

describe("customer controller", () => {
  it("searches once on the list, and never connects on its own", async () => {
    await boot();
    expect(app().getAttribute("aria-busy")).toBeNull();
    expect(called("browse_consoles")).toHaveLength(1);
    expect(app().querySelectorAll(".pick")).toHaveLength(2);
    expect(called("connect_console")).toHaveLength(0);
    expect(called("start_live_session")).toHaveLength(0);
  });

  it("does nothing on Escape without a session", async () => {
    await boot();
    key("Escape");
    await flush();
    expect(called("stop_session")).toHaveLength(0);
    expect(screen()).toBe("find");
  });

  it("connects the picked console, or the typed code on Enter", async () => {
    await boot({
      connect_console: () => viewIn({ phase: "announced" }),
      start_live_session: () => viewIn({ phase: "announced" }),
    });
    click('[data-action="connect"][data-arg="10.0.0.42:51234"]');
    await flush();
    expect(called("connect_console")).toEqual([{ address: "10.0.0.42:51234" }]);
    expect(screen()).toBe("connecting");
    push(viewIn({ phase: "stopped", reason: "customer_requested" }));
    await flush();
    const input = app().querySelector<HTMLInputElement>("#code");
    if (!input) throw new Error("no code field");
    input.value = "  6RRR-2H8F  ";
    key("Enter", input);
    await flush();
    expect(called("start_live_session")).toEqual([{ code: "6RRR-2H8F" }]);
  });

  it("explains the prompt first; Continue relaunches, a decline is said honestly, then going on unelevated", async () => {
    launch = { ...LAUNCH_READY, step: "admin", elevated: false };
    await boot({
      relaunch_elevated: () => ({
        outcome: "declined",
        message: "",
        launch: { ...launch, step: "declined" },
      }),
      continue_without_admin: () => ({ ...launch, step: "ready" }),
    });
    expect(screen()).toBe("admin");
    // Nothing is searched or dialed before the step is passed.
    expect(called("browse_consoles")).toHaveLength(0);
    click('[data-action="continue"]');
    await flush();
    expect(called("relaunch_elevated")).toHaveLength(1);
    expect(screen()).toBe("declined");
    click('[data-action="without-admin"]');
    await flush();
    expect(screen()).toBe("find");
    expect(called("browse_consoles")).toHaveLength(1);
  });

  it("goes back from the admin step through the backend", async () => {
    launch = { ...LAUNCH_READY, step: "admin", elevated: false };
    await boot({ go_back: () => undefined });
    click('[data-action="back"]');
    await flush();
    expect(called("go_back")).toHaveLength(1);
  });

  it("keeps a pushed full-control view over a stale grant return", async () => {
    view = viewIn({ phase: "awaiting_consent" });
    await boot({
      grant_consent: async () => {
        const stale = viewIn({ phase: "awaiting_consent" });
        push(viewIn({ phase: "full_control", session: "s" }));
        return stale;
      },
    });
    expect(screen()).toBe("consent");
    click('[data-action="allow"]');
    await flush();
    expect(screen()).toBe("live");
  });

  it("stops on Escape and Stop now during a session, then shows the end report", async () => {
    view = viewIn({ phase: "full_control", session: "s" });
    let report: typeof REPORT | null = null;
    await boot({ get_end_report: () => report });
    for (const entry of FEED) activity(entry);
    await flush();
    expect(app().querySelector(".now .t")?.textContent).toBe("Reading the Windows system log");
    key("Escape");
    await flush();
    expect(called("stop_session")).toHaveLength(1);
    expect(screen()).toBe("end");
    expect(app().querySelector(".cleanup")?.textContent).toContain("Cleaning up");
    report = REPORT;
    push(viewIn({ phase: "stopped", reason: "customer_requested" }));
    await flush();
    expect(app().querySelector("#end-title")?.textContent).toBe("You stopped the session");
  });

  it("saves a copy, then opens only that path", async () => {
    view = viewIn({ phase: "stopped", reason: "customer_requested" });
    const path = "C:\\Users\\jdoe\\Documents\\CretLink\\2026-11-03 ACME-PC-07.html";
    await boot({
      get_end_report: () => REPORT,
      save_report: () => path,
      open_saved_report: () => undefined,
    });
    // A window opened after the end has no memory of control: the report says so.
    push(viewIn({ phase: "full_control", session: "s" }));
    push(viewIn({ phase: "stopped", reason: "customer_requested" }));
    await flush();
    click('[data-action="save"]');
    await flush();
    expect(app().querySelector(".record")?.textContent).toContain(path);
    click('[data-action="open-report"]');
    await flush();
    expect(called("open_saved_report")).toEqual([{ path }]);
  });

  it("asks before closing during a session; confirming stops and closes", async () => {
    view = viewIn({ phase: "full_control", session: "s" });
    await boot({ stop_and_close: () => undefined });
    backend.listeners["customer-close-requested"]?.({ payload: null });
    await flush();
    const dialog = app().querySelector('[role="alertdialog"]');
    expect(dialog?.textContent).toContain("Stop the session and close?");
    click('[data-action="cancel-close"]');
    await flush();
    expect(app().querySelector('[role="alertdialog"]')).toBeNull();
    backend.listeners["customer-close-requested"]?.({ payload: null });
    await flush();
    click('[data-action="confirm-close"]');
    await flush();
    expect(called("stop_and_close")).toHaveLength(1);
    expect(app().querySelector('[role="alertdialog"]')?.textContent).toContain("cleaning up");
  });

  it("ignores a close question outside a session", async () => {
    await boot();
    backend.listeners["customer-close-requested"]?.({ payload: null });
    await flush();
    expect(app().querySelector('[role="alertdialog"]')).toBeNull();
  });

  it("makes small and filters the timeline", async () => {
    view = viewIn({ phase: "full_control", session: "s" });
    await boot({ make_small: () => undefined, get_activity: () => FEED });
    click('[data-action="filter"][data-arg="files"]');
    await flush();
    expect(app().querySelectorAll(".tl .ti:not(.grant)")).toHaveLength(2);
    click('[data-action="make-small"]');
    await flush();
    expect(called("make_small")).toHaveLength(1);
  });

  it("renders the compact bar in the mini window, with Stop and Open", async () => {
    view = viewIn({ phase: "full_control", session: "s" });
    await boot({ open_main: () => undefined, drag_window: () => undefined }, "mini");
    expect(app().querySelector(".minibar")).not.toBeNull();
    // The bar never searches the network.
    expect(called("browse_consoles")).toHaveLength(0);
    click('[data-action="open-main"]');
    await flush();
    expect(called("open_main")).toHaveLength(1);
    app().querySelector(".minibar b")?.dispatchEvent(new MouseEvent("mousedown", { bubbles: true, button: 0 }));
    await flush();
    expect(called("drag_window")).toHaveLength(1);
    click('[data-action="stop"]');
    await flush();
    expect(called("stop_session")).toHaveLength(1);
    expect(app().querySelector('[data-action="stop"]')).toBeNull();
  });

  it("states a backend refusal in an assertive alert", async () => {
    await boot({
      start_live_session: () => {
        throw { stage: "connect", message: "unrecognised connection code" };
      },
    });
    const input = app().querySelector<HTMLInputElement>("#code");
    if (input) input.value = "nope";
    click('[data-action="connect-code"]');
    await flush();
    const alert = app().querySelector('[role="alert"]')?.textContent ?? "";
    expect(alert).toContain("That code did not work");
    expect(alert).toContain("unrecognised connection code");
  });
});
