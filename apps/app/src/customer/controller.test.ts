// Controller tests (ST-M009-009): commands and events in, rendered window
// out, with Tauri replaced by fakes.

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import type { ActivityEvent } from "../shared/contract";
import { type TimerTracker, trackTimers } from "../shared/test-timers";
import type { ConsentView, LaunchView } from "./api";
import {
  CONSOLES,
  FEED,
  KEEP_INSTALLED,
  KEEP_OFFER,
  KEEP_PLAN,
  KEEP_SAME,
  KEEP_UPDATE,
  LAUNCH_READY,
  REMOVE_PLAN,
  REPORT,
  VIA_RELAY,
  VIA_RELAY_ELSEWHERE,
  viewIn,
} from "./fixtures";

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

  it("does not ask to retype a relay code that this copy cannot use", async () => {
    await boot({
      start_live_session: () => {
        throw {
          stage: "relay",
          message: "This is a relay code, for a technician who is not on your network.",
        };
      },
    });
    const input = app().querySelector<HTMLInputElement>("#code");
    if (input) input.value = "53ST-A71D-JKKV-0VGH";
    click('[data-action="connect-code"]');
    await flush();
    expect(called("start_live_session")).toEqual([{ code: "53ST-A71D-JKKV-0VGH" }]);
    const alert = app().querySelector('[role="alert"]')?.textContent ?? "";
    expect(alert).toContain("This copy cannot use a relay code");
    expect(alert).toContain("This is a relay code");
    expect(alert).toContain("set up for their relay");
    expect(alert).not.toContain("type it again");
  });

  it("says what is wrong with the relay settings of this copy, and what to tell the technician", async () => {
    // The profile file next to the program cannot be used (ST-M007-011).
    const message =
      "This is a relay code, but the relay settings of this copy of CretLink cannot be used. Line 2: `address` needs a port. The file is E:\\cretlink.toml.";
    await boot({
      start_live_session: () => {
        throw { stage: "relay_profile", message };
      },
    });
    const input = app().querySelector<HTMLInputElement>("#code");
    if (input) input.value = "53ST-A71D-JKKV-0VGH";
    click('[data-action="connect-code"]');
    await flush();
    const alert = app().querySelector('[role="alert"]')?.textContent ?? "";
    expect(alert).toContain("The relay settings of this copy cannot be used");
    expect(alert).toContain(message);
    expect(alert).toContain("ask for a new copy of CretLink");
    expect(alert).toContain("You can still connect while you are on the same network.");
    expect(alert).not.toContain("type it again");
    // The window is where it was: nothing was dialled.
    expect(app().querySelector("#code")).not.toBeNull();
  });

  it("connects through the relay of this copy when a relay code is typed, and says so at once", async () => {
    const reaching = viewIn({ phase: "announced" }, { technician: null, relay: VIA_RELAY });
    await boot({ start_live_session: () => reaching });
    const input = app().querySelector<HTMLInputElement>("#code");
    // Typed as people type what is read to them; the backend reads it.
    if (input) input.value = " 53st a71d jkkv 0vgh ";
    click('[data-action="connect-code"]');
    await flush();
    expect(called("start_live_session")).toEqual([{ code: "53st a71d jkkv 0vgh" }]);
    expect(app().querySelector("#connecting-h")?.textContent).toBe(
      "Connecting through the relay relay.example.org",
    );
    expect(app().textContent).toContain("The relay only passes encrypted traffic on.");
    expect(app().querySelector("#announce")?.textContent).toBe(
      "Connecting through the relay relay.example.org. No access.",
    );
    expect(app().querySelector('[role="alert"]')?.textContent).toBe("");
    // Cancel (Escape) is there while it connects.
    document.dispatchEvent(new KeyboardEvent("keydown", { key: "Escape" }));
    await flush();
    expect(called("stop_session")).toHaveLength(1);
  });

  it("says that nobody waits on a relay code, and searches no network for it", async () => {
    await boot({
      start_live_session: () =>
        viewIn({ phase: "announced" }, { technician: null, relay: VIA_RELAY }),
    });
    const input = app().querySelector<HTMLInputElement>("#code");
    if (input) input.value = "53ST-A71D-JKKV-0VGH";
    click('[data-action="connect-code"]');
    await flush();
    // Ten seconds later, on the backend: nobody was heard.
    push(
      viewIn(
        { phase: "failed", stage: "rendezvous", message: "nobody joined the rendezvous on the relay" },
        { technician: null, relay: VIA_RELAY },
      ),
    );
    await flush();
    const alert = app().querySelector('[role="alert"]')?.textContent ?? "";
    expect(alert).toContain("Nobody is waiting on that code");
    expect(alert).toContain("The relay relay.example.org found nobody waiting on the code you typed.");
    expect(alert).toContain("A code works once.");
    // Not the jargon of the backend, and not a session that failed.
    expect(alert).not.toContain("rendezvous");
    expect(alert).not.toContain("The session failed");
    // Back at the code field, to type the code again.
    expect(app().querySelector("#code")).not.toBeNull();
    // Nothing tries again by itself.
    expect(called("start_live_session")).toHaveLength(1);
  });

  it("shows why the relay could not be used with the backend's next step", async () => {
    await boot({
      start_live_session: () =>
        viewIn({ phase: "announced" }, { technician: null, relay: VIA_RELAY }),
    });
    const input = app().querySelector<HTMLInputElement>("#code");
    if (input) input.value = "53ST-A71D-JKKV-0VGH";
    click('[data-action="connect-code"]');
    await flush();
    push(
      viewIn(
        {
          phase: "failed",
          stage: "relay_untrusted",
          message: "The relay at relay.example.org:443 showed no certificate that this computer trusts.",
        },
        {
          technician: null,
          relay: VIA_RELAY,
          failure_next:
            "Tell your technician that this computer does not trust the certificate of the relay relay.example.org. Nobody has access to this computer.",
        },
      ),
    );
    await flush();
    const alert = app().querySelector('[role="alert"]')?.textContent ?? "";
    expect(alert).toContain("This computer does not trust the relay");
    expect(alert).toContain("showed no certificate that this computer trusts");
    expect(alert).toContain("Tell your technician that this computer does not trust the certificate");
    expect(alert).not.toContain("The session failed");
    expect(called("start_live_session")).toHaveLength(1);
  });

  it("goes from a relay code to the approval code and the decision as on one network", async () => {
    await boot({
      start_live_session: () =>
        viewIn({ phase: "announced" }, { technician: null, relay: VIA_RELAY }),
      grant_consent: () => view,
    });
    const input = app().querySelector<HTMLInputElement>("#code");
    if (input) input.value = "53ST-A71D-JKKV-0VGH";
    click('[data-action="connect-code"]');
    await flush();
    push(viewIn({ phase: "awaiting_approval_compare" }, { approval_code: "482913", relay: VIA_RELAY }));
    await flush();
    expect(app().querySelector(".sac")?.getAttribute("aria-label")).toBe("Approval code 482 913");
    expect(app().querySelector(".via-relay")?.textContent).toContain("relay.example.org");
    push(viewIn({ phase: "awaiting_consent" }, { relay: VIA_RELAY }));
    await flush();
    // The decision is the customer's, and neither button is focused.
    expect(app().querySelector('[data-action="allow"]')).not.toBeNull();
    expect(app().querySelector('[data-action="decline"]')).not.toBeNull();
    expect(document.activeElement?.getAttribute("data-action")).toBeNull();
    click('[data-action="allow"]');
    await flush();
    expect(called("grant_consent")).toHaveLength(1);
    push(viewIn({ phase: "full_control", session: "s" }, { relay: VIA_RELAY }));
    await flush();
    expect(app().querySelector("#state-band")?.textContent).toContain("UNDER FULL CONTROL");
    expect(app().querySelector(".via-relay")?.textContent).toContain("relay.example.org");
  });
});

describe("keeping the support program (ST-M009-017)", () => {
  const card = (): HTMLElement | null => app().querySelector('[data-key="keep-card"]');
  const alert = (): string => app().querySelector('[role="alert"]')?.textContent ?? "";

  it("keeps nothing without the customer's clicks: first what it adds, then the choice", async () => {
    let kept = false;
    await boot({
      get_keep: () => (kept ? KEEP_SAME : KEEP_OFFER),
      keep_plan: () => KEEP_PLAN,
      keep_program: () => {
        kept = true;
        return KEEP_SAME;
      },
    });
    // Starting the window asks what is kept, and plans and keeps nothing.
    expect(called("get_keep")).toHaveLength(1);
    expect(called("keep_plan")).toHaveLength(0);
    expect(called("keep_program")).toHaveLength(0);
    expect(card()?.textContent).toContain("Getting help more often?");

    click('[data-action="keep-open"]');
    await flush();
    expect(screen()).toBe("keep");
    expect(called("keep_plan")).toHaveLength(1);
    // Looking at what it adds keeps nothing, and neither choice has the focus.
    expect(called("keep_program")).toHaveLength(0);
    expect(document.activeElement?.id).toBe("main");

    click('[data-action="keep-confirm"]');
    await flush();
    // Without the wish, no desktop shortcut.
    expect(called("keep_program")).toEqual([{ desktop: false }]);
    expect(screen()).toBe("find");
    expect(card()?.textContent).toContain("CretLink Support is now kept on this computer");
    expect(card()?.querySelector("button")).toBeNull();
    expect(app().querySelector("#installed-words")?.textContent).toBe(
      "CretLink Support is kept on this computer · closing CretLink ends everything",
    );
    expect(app().querySelector("#announce")?.textContent).toContain(
      "CretLink Support is now kept on this computer.",
    );
    // Nothing was dialled or stopped on the way.
    expect(called("start_live_session")).toHaveLength(0);
    expect(called("connect_console")).toHaveLength(0);
  });

  it("passes the wish for a desktop shortcut, and an update keeps the one that is there", async () => {
    await boot({
      get_keep: () => KEEP_UPDATE,
      keep_plan: () => ({ ...KEEP_PLAN, replaces: "1.0.0", desktopKept: true, relay: VIA_RELAY_ELSEWHERE }),
      keep_program: () => KEEP_SAME,
    });
    expect(card()?.textContent).toContain("in version 1.0.0. This copy is version 1.1.0");
    click('[data-action="keep-open"]');
    await flush();
    expect(app().querySelector("#keep-title")?.textContent).toBe("Update CretLink Support on this computer?");
    // The relay of the copy is named before the choice.
    expect(app().querySelector('[data-key="keep-relay"]')?.textContent).toContain(
      "relay.example.org (reached at 192.0.2.1:9970)",
    );
    const desktop = app().querySelector<HTMLInputElement>("#keep-desktop");
    expect(desktop?.checked).toBe(true);
    // The customer no longer wants it.
    desktop?.click();
    expect(desktop?.checked).toBe(false);
    // A re-render in between leaves the choice as the customer made it.
    push(view);
    await flush();
    expect(app().querySelector<HTMLInputElement>("#keep-desktop")?.checked).toBe(false);
    desktop?.click();
    click('[data-action="keep-confirm"]');
    await flush();
    expect(called("keep_program")).toEqual([{ desktop: true }]);
    expect(app().querySelector("#announce")?.textContent).toBe("CretLink Support was updated on this computer.");
  });

  it("Back and Escape leave the screen and change nothing", async () => {
    await boot({ get_keep: () => KEEP_OFFER, keep_plan: () => KEEP_PLAN });
    click('[data-action="keep-open"]');
    await flush();
    expect(screen()).toBe("keep");
    click('[data-action="panel-back"]');
    await flush();
    expect(screen()).toBe("find");
    // The focus returns to where the customer came from.
    expect(document.activeElement?.getAttribute("data-action")).toBe("keep-open");

    click('[data-action="keep-open"]');
    await flush();
    expect(screen()).toBe("keep");
    key("Escape");
    await flush();
    expect(screen()).toBe("find");
    expect(called("keep_program")).toHaveLength(0);
    expect(called("stop_session")).toHaveLength(0);
    expect(card()?.textContent).toContain("Getting help more often?");
  });

  it("offers nothing during a session, and a session that starts closes the screen", async () => {
    await boot({ get_keep: () => KEEP_OFFER, keep_plan: () => KEEP_PLAN });
    click('[data-action="keep-open"]');
    await flush();
    expect(screen()).toBe("keep");
    push(viewIn({ phase: "awaiting_consent" }));
    await flush();
    expect(screen()).toBe("consent");
    expect(app().querySelector('[data-action^="keep-"]')).toBeNull();
    push(viewIn({ phase: "full_control", session: "s" }));
    await flush();
    expect(screen()).toBe("live");
    expect(card()).toBeNull();
    // Escape is the stop of the session again, and nothing was kept.
    expect(called("keep_program")).toHaveLength(0);
    // The session ends with a report: there it is offered again.
    push(viewIn({ phase: "stopped", reason: "customer_requested" }));
    await flush();
    expect(screen()).toBe("end");
    expect(card()?.querySelector('[data-action="keep-open"]')).not.toBeNull();
    expect(called("get_keep").length).toBeGreaterThanOrEqual(2);
  });

  it("offers nothing where the backend offers nothing", async () => {
    // No administrator rights.
    await boot({ get_keep: () => ({ ...KEEP_OFFER, canKeep: false }) });
    expect(card()).toBeNull();
    expect(app().querySelector("#installed-words")?.textContent).toContain("Nothing is installed");
  });

  it("offers nothing when the backend has no answer", async () => {
    await boot();
    expect(called("get_keep")).toHaveLength(1);
    expect(card()).toBeNull();
    expect(screen()).toBe("find");
    expect(alert()).toBe("");
  });

  it("says a refusal in plain words, and nothing looks kept", async () => {
    await boot({
      get_keep: () => KEEP_OFFER,
      keep_plan: () => KEEP_PLAN,
      keep_program: () => {
        throw {
          stage: "keep_running",
          message: "The kept support program is running (C:\\Program Files\\CretLink Support\\CretLink-Support.exe). Close it, then try again.",
        };
      },
    });
    click('[data-action="keep-open"]');
    await flush();
    click('[data-action="keep-confirm"]');
    await flush();
    expect(screen()).toBe("find");
    expect(alert()).toContain("CretLink Support was not kept");
    expect(alert()).toContain("The kept support program is running");
    expect(card()?.textContent).toContain("Getting help more often?");
    expect(card()?.textContent).not.toContain("now kept");
    // The plan was used up: keeping again starts with what it adds.
    click('[data-action="keep-open"]');
    await flush();
    expect(called("keep_plan")).toHaveLength(2);
    expect(screen()).toBe("keep");
  });

  it("says when the plan itself is refused, and opens no screen", async () => {
    await boot({
      get_keep: () => KEEP_OFFER,
      keep_plan: () => {
        throw { stage: "keep_administrator", message: "this needs administrator rights, and this window runs without them." };
      },
    });
    click('[data-action="keep-open"]');
    await flush();
    expect(screen()).toBe("find");
    expect(alert()).toContain("CretLink Support cannot be kept from here");
    expect(alert()).toContain("This needs administrator rights");
    expect(alert()).toContain("Nothing was changed on this computer.");
  });

  it("the kept program removes itself: first what goes, then the choice, then only closing", async () => {
    await boot({
      get_keep: () => KEEP_INSTALLED,
      remove_plan: () => REMOVE_PLAN,
      remove_program: () => ({ state: "pending", left: [] }),
      stop_and_close: () => undefined,
    });
    expect(card()?.textContent).toContain("CretLink Support is installed on this computer");
    expect(app().querySelector('[data-action="keep-open"]')).toBeNull();
    click('[data-action="remove-open"]');
    await flush();
    expect(screen()).toBe("remove");
    expect(called("remove_plan")).toHaveLength(1);
    expect(called("remove_program")).toHaveLength(0);
    expect(app().querySelectorAll('[aria-labelledby="remove-goes-h"] li')).toHaveLength(REMOVE_PLAN.removes.length);
    expect(document.activeElement?.id).toBe("main");

    click('[data-action="remove-confirm"]');
    await flush();
    expect(called("remove_program")).toHaveLength(1);
    expect(screen()).toBe("removed");
    expect(app().querySelectorAll("button")).toHaveLength(1);
    // Escape does not bring the removed program back to the list.
    key("Escape");
    await flush();
    expect(screen()).toBe("removed");
    click('[data-action="close-app"]');
    await flush();
    expect(called("stop_and_close")).toHaveLength(1);
  });

  it("without administrator rights, removing asks Windows and this window closes", async () => {
    await boot({
      get_keep: () => ({ ...KEEP_INSTALLED, removeElevates: true }),
      remove_plan: () => ({ ...REMOVE_PLAN, elevates: true }),
      remove_program: () => ({ state: "elevating", left: [] }),
    });
    click('[data-action="remove-open"]');
    await flush();
    expect(app().querySelector('[data-key="remove-elevates"]')?.textContent).toContain(
      "Windows will ask for administrator rights.",
    );
    click('[data-action="remove-confirm"]');
    await flush();
    expect(called("remove_program")).toHaveLength(1);
    // The window is closing: nothing more can be pressed, and it says why.
    expect(app().querySelector<HTMLButtonElement>('[data-action="remove-confirm"]')?.disabled).toBe(true);
    expect(app().querySelector("#announce")?.textContent).toBe(
      "Windows asks for administrator rights. This window closes.",
    );
  });

  it("says when removing was refused, and the program stays offered for removal", async () => {
    await boot({
      get_keep: () => KEEP_INSTALLED,
      remove_plan: () => REMOVE_PLAN,
      remove_program: () => {
        throw { stage: "keep_declined", message: "Windows was not given permission. Nothing was removed." };
      },
    });
    click('[data-action="remove-open"]');
    await flush();
    click('[data-action="remove-confirm"]');
    await flush();
    expect(alert()).toContain("CretLink Support was not removed");
    expect(alert()).toContain("Windows was not given permission. Nothing was removed.");
    expect(screen()).toBe("remove");
    expect(app().querySelector<HTMLButtonElement>('[data-action="remove-confirm"]')?.disabled).toBe(false);
  });

  it("offers it on the end report of a session, with the report still there", async () => {
    view = viewIn({ phase: "full_control", session: "s" });
    await boot({
      get_keep: () => KEEP_OFFER,
      keep_plan: () => KEEP_PLAN,
      keep_program: () => KEEP_SAME,
      get_end_report: () => REPORT,
    });
    expect(screen()).toBe("live");
    expect(card()).toBeNull();
    push(viewIn({ phase: "stopped", reason: "customer_requested" }));
    await flush();
    expect(screen()).toBe("end");
    click('[data-action="keep-open"]');
    await flush();
    expect(screen()).toBe("keep");
    click('[data-action="keep-confirm"]');
    await flush();
    // Back on the report, which says that the program is kept now.
    expect(screen()).toBe("end");
    expect(card()?.textContent).toContain("CretLink Support is now kept on this computer");
    expect(app().querySelector("#end-title")?.textContent).toBe("You stopped the session");
    expect(app().querySelector('[data-action="save"]')).not.toBeNull();
  });

  it("the compact bar asks nothing about keeping", async () => {
    view = viewIn({ phase: "full_control", session: "s" });
    await boot({ get_keep: () => KEEP_OFFER }, "mini");
    expect(called("get_keep")).toHaveLength(0);
    expect(app().querySelector('[data-action^="keep-"]')).toBeNull();
  });
});

