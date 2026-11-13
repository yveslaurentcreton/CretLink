// The file explorer driven through the window's controller (ST-M010-002),
// against a simulated customer computer: a tree of folders behind the two
// commands of the backend, which names every place by its own spelling and
// delivers a folder in pages on the `console-explorer` event, like the real
// one. Tauri and the xterm terminal are replaced by fakes.
//
// Nothing here waits for a frame or for time: a test delivers the pages it
// wants delivered and then lets the queued work run.

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import type { ExplorerDrive, ExplorerEntry } from "../shared/contract";
import { type TimerTracker, trackTimers } from "../shared/test-timers";
import type { ConsoleState } from "./api";
import { ROW_HEIGHT } from "./explorer";
import { consoleIn } from "./fixtures";

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
    attach(): void {}
    refit(): void {}
    dispose(): void {}
  },
}));

// --- The simulated customer computer ---

const T0 = Date.UTC(2026, 10, 3, 14, 30, 0);

function file(name: string, len = 1024): ExplorerEntry {
  return { name, kind: "file", len, modifiedMs: T0, readonly: false };
}

function dir(name: string): ExplorerEntry {
  return { name, kind: "dir", len: 0, modifiedMs: T0, readonly: false };
}

function link(name: string): ExplorerEntry {
  return { name, kind: "symlink", len: 0, modifiedMs: T0, readonly: false };
}

function many(count: number): ExplorerEntry[] {
  return Array.from({ length: count }, (_, n) =>
    file(`driver-${String(n).padStart(5, "0")}.dll`, 1000 + ((n * 7919) % 5000)),
  );
}

interface Listing {
  listing: number;
  path: string;
  pages: ExplorerEntry[][];
  sent: number;
  select?: string;
  resolve: (answer: unknown) => void;
}

/** A customer computer held in memory, behind the commands of the backend. */
class Target {
  drives: ExplorerDrive[] = [
    { root: "C:\\", kind: "fixed", label: "Windows", totalBytes: 500e9, freeBytes: 100e9 },
    { root: "E:\\", kind: "removable" },
  ];
  // Every folder by its real path, with its entries in the order it lists them.
  folders = new Map<string, ExplorerEntry[]>([
    ["C:\\", [dir("Users"), dir("Windows"), link("Documents and Settings"), file("pagefile.sys")]],
    ["C:\\Users", [dir("ann"), dir("jdoe")]],
    ["C:\\Users\\ann", [dir("Documents"), dir("Downloads"), file("NTUSER.DAT")]],
    [
      "C:\\Users\\ann\\Documents",
      [dir("Invoices"), file("a.txt", 30), file("b.txt", 10), file("c.txt", 20), file("report.pdf", 5000)],
    ],
    ["C:\\Users\\ann\\Documents\\Invoices", []],
    ["C:\\Users\\ann\\Downloads", [file("setup.exe")]],
    ["C:\\Windows", [dir("System32")]],
    ["C:\\Windows\\System32", many(5000)],
    ["E:\\", []],
  ]);
  refused = new Map<string, { stage: string; message: string }>([
    ["C:\\Users\\jdoe", { stage: "access_denied", message: "access was denied" }],
  ]);
  pageSize = 256;
  // When set, a listing delivers a page only when the test says so.
  manual = false;
  open: Listing[] = [];

  /** The real spelling of a path, whatever its letter case. */
  private real(asked: string): string | undefined {
    const wanted = asked.replace(/\//g, "\\").toLowerCase();
    for (const path of this.folders.keys()) if (path.toLowerCase() === wanted) return path;
    for (const path of this.refused.keys()) if (path.toLowerCase() === wanted) return path;
    return undefined;
  }

  /** A newer place leaves every listing that is still running. */
  private leave(): void {
    for (const listing of this.open.splice(0)) {
      listing.resolve({
        listing: listing.listing,
        path: listing.path,
        entries: listing.pages.slice(0, listing.sent).flat().length,
        state: "cancelled",
      });
    }
  }

  handlers(): Record<string, Handler> {
    return {
      explorer_drives: () => {
        this.leave();
        return this.drives;
      },
      explorer_open: (args) => {
        this.leave();
        const asked = String(args?.path);
        const listing = Number(args?.listing);
        let path = this.real(asked);
        let select: string | undefined;
        if (path === undefined) {
          // A path to a file opens the folder that holds it.
          const cut = asked.lastIndexOf("\\");
          const parent = this.real(asked.slice(0, cut) || asked);
          const name = asked.slice(cut + 1);
          const entry = parent
            ? this.folders.get(parent)?.find((e) => e.name.toLowerCase() === name.toLowerCase())
            : undefined;
          if (!parent || !entry) {
            throw {
              stage: "path_not_found",
              message: `${parent ?? "the customer computer"} lists no entry named ${name}`,
            };
          }
          path = parent;
          select = entry.name;
        }
        const refusal = this.refused.get(path);
        if (refusal) throw refusal;
        const entries = this.folders.get(path) ?? [];
        const pages: ExplorerEntry[][] = [];
        for (let from = 0; from < entries.length; from += this.pageSize) {
          pages.push(entries.slice(from, from + this.pageSize));
        }
        if (pages.length === 0) pages.push([]);
        return new Promise((resolve) => {
          const running: Listing = { listing, path: path as string, pages, sent: 0, resolve };
          if (select !== undefined) running.select = select;
          this.open.push(running);
          if (!this.manual) while (this.open.includes(running)) this.step();
        });
      },
    };
  }

  /** Delivers the next page of the listing that runs; the last one ends it. */
  step(): void {
    const running = this.open[0];
    if (!running) throw new Error("no listing is running");
    const entries = running.pages[running.sent] ?? [];
    running.sent += 1;
    const end = running.sent === running.pages.length;
    const emit = backend.listeners["console-explorer"];
    emit?.({
      payload:
        running.sent === 1
          ? {
              kind: "opened",
              listing: running.listing,
              path: running.path,
              ...(running.select === undefined ? {} : { select: running.select }),
              entries,
              end,
            }
          : { kind: "page", listing: running.listing, entries, end },
    });
    if (end) {
      this.open.shift();
      running.resolve({
        listing: running.listing,
        path: running.path,
        entries: running.pages.flat().length,
        state: "complete",
      });
    }
  }
}

// --- The window ---

let state: ConsoleState = consoleIn({ phase: "idle" });
let timers: TimerTracker;
let target: Target;

const FULL = consoleIn({ phase: "full_control" }, { session: true, target: true });
const LISTENING = consoleIn({ phase: "listening", code: "6RRR-2H8F", address: "10.0.0.42:7000" });

function push(next: ConsoleState): void {
  state = next;
  backend.listeners["console-state"]?.({ payload: next });
}

const flush = async (): Promise<void> => {
  for (let i = 0; i < 5; i += 1) await new Promise((resolve) => setTimeout(resolve, 0));
};

function app(): HTMLElement {
  return document.getElementById("app") as HTMLElement;
}

function $(selector: string): HTMLElement {
  const el = app().querySelector<HTMLElement>(selector);
  if (!el) throw new Error(`no ${selector}`);
  return el;
}

function click(selector: string, init: MouseEventInit = {}): void {
  $(selector).dispatchEvent(new MouseEvent("click", { bubbles: true, cancelable: true, ...init }));
}

/** A double click as the mouse makes it: two clicks, then the double click. */
function doubleClick(selector: string): void {
  click(selector);
  click(selector);
  $(selector).dispatchEvent(new MouseEvent("dblclick", { bubbles: true, cancelable: true }));
}

function row(name: string): string {
  return `#xp-grid [data-row="${name.replace(/\\/g, "\\\\")}"]`;
}

function key(key: string, init: KeyboardEventInit = {}, on: string = "#xp-grid"): void {
  $(on).dispatchEvent(new KeyboardEvent("keydown", { key, bubbles: true, cancelable: true, ...init }));
}

/** The names of the rows that are drawn, in the order of the list. */
function drawn(): string[] {
  return Array.from(app().querySelectorAll<HTMLElement>("#xp-rows [data-row]"))
    .sort((a, b) => Number(a.getAttribute("aria-rowindex")) - Number(b.getAttribute("aria-rowindex")))
    .map((el) => el.dataset.row ?? "");
}

function selected(): string[] {
  return Array.from(app().querySelectorAll<HTMLElement>('#xp-rows [aria-selected="true"]')).map(
    (el) => el.dataset.row ?? "",
  );
}

function crumbs(): string[] {
  return Array.from(app().querySelectorAll(".xp-path .xp-crumb")).map((el) => el.textContent ?? "");
}

function status(): string {
  return $("#xp-status").textContent ?? "";
}

function asked(cmd: string): Array<Record<string, unknown> | undefined> {
  return backend.calls.filter(([name]) => name === cmd).map(([, args]) => args);
}

/** The paths the explorer asked the backend to open, in order. */
function opened(): string[] {
  return asked("explorer_open").map((args) => String(args?.path));
}

async function boot(): Promise<void> {
  backend.handlers = {
    get_state: () => state,
    stop_session: () => consoleIn({ phase: "stopped", reason: "technician_requested" }),
    ...target.handlers(),
  };
  const { start } = await import("./main");
  await start({ version: "0.22.0" });
  await flush();
}

/** A session under full control with the Files tab on screen. */
async function files(): Promise<void> {
  await boot();
  push(LISTENING);
  push(FULL);
  await flush();
  click("#tab-files");
  await flush();
}

/** Opens `path` by typing it, and waits until it is on screen. */
async function goTo(path: string): Promise<void> {
  click("#xp-edit");
  await flush();
  ($("#xp-path-input") as HTMLInputElement).value = path;
  key("Enter", {}, "#xp-path-input");
  await flush();
}

beforeEach(() => {
  timers = trackTimers();
  vi.resetModules();
  backend.handlers = {};
  backend.listeners = {};
  backend.calls = [];
  target = new Target();
  state = consoleIn({ phase: "idle" });
  document.body.innerHTML = '<div id="app" aria-busy="true"></div>';
});

afterEach(() => {
  timers.clear();
  document.body.replaceChildren();
});

describe("the Files tab", () => {
  it("asks nothing of the customer computer until the tab is opened, then lists the drives", async () => {
    await boot();
    push(LISTENING);
    push(FULL);
    await flush();
    expect(asked("explorer_drives")).toHaveLength(0);
    expect(opened()).toHaveLength(0);

    click("#tab-files");
    await flush();
    expect(asked("explorer_drives")).toHaveLength(1);
    expect(drawn()).toEqual(["C:\\", "E:\\"]);
    expect($(row("C:\\")).textContent).toContain("Windows (C:)");
    expect(status()).toBe("2 drives");
    expect($("#announce").textContent).toBe("The drives of ACME-PC-07: 2 drives.");
    expect(crumbs()).toEqual(["ACME-PC-07"]);

    // Coming back to the tab asks nothing again.
    click("#tab-overview");
    click("#tab-files");
    await flush();
    expect(asked("explorer_drives")).toHaveLength(1);
  });

  it("keeps the files sent and received this session as its transfers panel", async () => {
    await files();
    expect($(".xp-transfers").textContent).toContain(
      "No files were sent or received in this session.",
    );
    backend.listeners["console-activity"]?.({
      payload: {
        entry: "item",
        item: {
          id: "t1",
          source: "technician",
          description: "Copied a file to the customer computer",
          kind: "file_received",
          operation: "copy a file to the customer computer at C:\\Users\\ann\\Downloads\\setup.exe",
          path: "C:\\Users\\ann\\Downloads\\setup.exe",
          bytes: 4096,
          state: { state: "done" },
          startedAt: new Date(T0).toISOString(),
        },
      },
    });
    await flush();
    expect(app().querySelectorAll(".xp-transfers .list.files .it")).toHaveLength(1);
    expect($(".xp-transfers").textContent).toContain("C:\\Users\\ann\\Downloads\\setup.exe");
    // The list of drives is still there beside it.
    expect(drawn()).toEqual(["C:\\", "E:\\"]);
  });
});

describe("navigation", () => {
  it("opens a drive with Enter and a folder with a double click", async () => {
    await files();
    $("#xp-grid").focus();
    key("ArrowDown");
    await flush();
    expect(selected()).toEqual(["C:\\"]);
    key("Enter");
    await flush();
    expect(opened()).toEqual(["C:\\"]);
    // Folders first, then the rest by name.
    expect(drawn()).toEqual(["Users", "Windows", "Documents and Settings", "pagefile.sys"]);
    expect(crumbs()).toEqual(["ACME-PC-07", "C:\\"]);
    expect($("#announce").textContent).toBe("C:\\: 4 items.");

    doubleClick(row("Users"));
    await flush();
    expect(opened()).toEqual(["C:\\", "C:\\Users"]);
    expect(drawn()).toEqual(["ann", "jdoe"]);
    expect(crumbs()).toEqual(["ACME-PC-07", "C:\\", "Users"]);
    expect(app().querySelector('.xp-crumb[aria-current="location"]')?.textContent).toBe("Users");
    // The list keeps the keyboard.
    expect(document.activeElement).toBe($("#xp-grid"));
  });

  it("opens nothing for a file or a link, and says why", async () => {
    await files();
    await goTo("C:\\");
    const before = opened().length;
    click(row("pagefile.sys"));
    key("Enter");
    await flush();
    expect($("#announce").textContent).toBe(
      "pagefile.sys is a file. Folders open here, a file does not.",
    );
    doubleClick(row("Documents and Settings"));
    await flush();
    expect($("#announce").textContent).toBe(
      "Documents and Settings is a link. A link is shown here, not followed.",
    );
    expect(opened()).toHaveLength(before);
    expect(crumbs()).toEqual(["ACME-PC-07", "C:\\"]);
  });

  it("takes a typed path and shows the place as the customer computer names it", async () => {
    await files();
    click("#xp-edit");
    await flush();
    const field = $("#xp-path-input") as HTMLInputElement;
    expect(document.activeElement).toBe(field);
    expect(field.value).toBe("");
    field.value = "  c:/users/ANN/documents ";
    key("Enter", {}, "#xp-path-input");
    await flush();
    expect(opened()).toEqual(["c:/users/ANN/documents"]);
    // Not the spelling that was typed.
    expect(crumbs()).toEqual(["ACME-PC-07", "C:\\", "Users", "ann", "Documents"]);
    expect($("#xp-grid").getAttribute("aria-label")).toBe(
      "Files and folders in C:\\Users\\ann\\Documents",
    );
    expect(app().querySelector("#xp-path-input")).toBeNull();
    expect(document.activeElement).toBe($("#xp-grid"));

    // The field then starts from the real path.
    click("#xp-edit");
    await flush();
    expect(($("#xp-path-input") as HTMLInputElement).value).toBe("C:\\Users\\ann\\Documents");
  });

  it("opens the folder of a file whose path was typed, with the file selected", async () => {
    await files();
    await goTo("c:\\users\\ann\\documents\\REPORT.PDF");
    expect(crumbs()).toEqual(["ACME-PC-07", "C:\\", "Users", "ann", "Documents"]);
    expect(selected()).toEqual(["report.pdf"]);
    expect($("#xp-grid").getAttribute("aria-activedescendant")).toBe($(row("report.pdf")).id);
  });

  it("stays where it was when a place is refused, and says the refusal", async () => {
    await files();
    await goTo("C:\\Users");
    doubleClick(row("jdoe"));
    await flush();
    const alert = $("#xp-alert");
    expect(alert.getAttribute("role")).toBe("alert");
    expect(alert.textContent).toContain("C:\\Users\\jdoe could not be opened");
    expect(alert.textContent).toContain("access was denied (access_denied)");
    expect(alert.textContent).toContain("You are still in C:\\Users.");
    // Nothing moved: the place, the rows, the history.
    expect(crumbs()).toEqual(["ACME-PC-07", "C:\\", "Users"]);
    expect(drawn()).toEqual(["ann", "jdoe"]);
    expect(status()).toBe("2 items · 1 selected");

    click('[data-action="xp-dismiss"]');
    await flush();
    expect($("#xp-alert").textContent).toBe("");
  });

  it("puts a typed path that was refused back into the field to be corrected", async () => {
    await files();
    await goTo("C:\\Users");
    await goTo("C:\\Usres\\ann");
    expect($("#xp-alert").textContent).toContain("C:\\Usres\\ann could not be opened");
    expect($("#xp-alert").textContent).toContain("lists no entry named ann (path_not_found)");
    const field = $("#xp-path-input") as HTMLInputElement;
    expect(field.value).toBe("C:\\Usres\\ann");
    expect(document.activeElement).toBe(field);
    expect(drawn()).toEqual(["ann", "jdoe"]);
  });

  it("goes up with Backspace, back and forward, and selects the folder it came out of", async () => {
    await files();
    await goTo("C:\\Users\\ann\\Documents");
    expect($('[data-action="xp-forward"]').hasAttribute("disabled")).toBe(true);
    expect($('[data-action="xp-up"]').getAttribute("aria-label")).toBe("Up to C:\\Users\\ann");

    key("Backspace");
    await flush();
    expect(opened().at(-1)).toBe("C:\\Users\\ann");
    expect(crumbs()).toEqual(["ACME-PC-07", "C:\\", "Users", "ann"]);
    expect(selected()).toEqual(["Documents"]);

    click('[data-action="xp-back"]');
    await flush();
    expect(crumbs().at(-1)).toBe("Documents");
    expect($('[data-action="xp-forward"]').getAttribute("aria-label")).toBe(
      "Forward to C:\\Users\\ann",
    );
    key("ArrowRight", { altKey: true });
    await flush();
    expect(crumbs().at(-1)).toBe("ann");
    key("ArrowLeft", { altKey: true });
    await flush();
    expect(crumbs().at(-1)).toBe("Documents");

    // A part of the path opens that folder, with the one below it selected.
    click('[data-action="xp-go"][data-arg="C:\\\\Users"]');
    await flush();
    expect(crumbs()).toEqual(["ACME-PC-07", "C:\\", "Users"]);
    expect(selected()).toEqual(["ann"]);

    // From a root, up leads to the drives, with that drive selected.
    await goTo("E:\\");
    expect($("#xp-state").textContent).toBe("This folder is empty.");
    key("ArrowUp", { altKey: true });
    await flush();
    expect(asked("explorer_drives")).toHaveLength(2);
    expect(selected()).toEqual(["E:\\"]);
    expect($('[data-action="xp-up"]').hasAttribute("disabled")).toBe(true);
  });

  it("refreshes with F5 and keeps the list on screen until the folder is listed again", async () => {
    await files();
    await goTo("C:\\Users\\ann\\Documents");
    click(row("a.txt"));
    await flush();
    // A file is gone and one is new on the customer computer.
    target.folders.set("C:\\Users\\ann\\Documents", [dir("Invoices"), file("a.txt"), file("new.txt")]);
    target.manual = true;
    key("F5");
    await flush();
    expect(opened().at(-1)).toBe("C:\\Users\\ann\\Documents");
    expect(drawn()).toEqual(["Invoices", "a.txt", "b.txt", "c.txt", "report.pdf"]);
    expect($("#xp-grid").getAttribute("aria-busy")).toBe("true");
    target.step();
    await flush();
    expect(drawn()).toEqual(["Invoices", "a.txt", "new.txt"]);
    expect(selected()).toEqual(["a.txt"]);
    expect($("#xp-grid").getAttribute("aria-busy")).toBe("false");
    // A refresh is no step back.
    expect($('[data-action="xp-back"]').getAttribute("aria-label")).toBe("Back to the drives");
  });

  it("lists the drives when Refresh is pressed with nothing open", async () => {
    target.refused.set("drives", { stage: "x", message: "x" });
    await boot();
    backend.handlers.explorer_drives = () => {
      throw { stage: "unsupported", message: "not supported on this computer" };
    };
    push(LISTENING);
    push(FULL);
    await flush();
    click("#tab-files");
    await flush();
    expect($("#xp-alert").textContent).toContain("The drives could not be listed");
    expect($("#xp-alert").textContent).toContain("not supported on this computer (unsupported)");
    expect($("#xp-state").textContent).toContain("Nothing is open.");
    backend.handlers.explorer_drives = () => target.drives;
    click('[data-action="xp-refresh"]');
    await flush();
    expect(drawn()).toEqual(["C:\\", "E:\\"]);
    expect($("#xp-alert").textContent).toBe("");
  });
});

describe("a folder with thousands of entries", () => {
  it("shows the first page at once and fills the list while the technician works in it", async () => {
    await files();
    target.manual = true;
    target.pageSize = 256;
    target.folders.set("C:\\Windows\\System32", [...many(700), dir("drivers"), dir("Config")]);
    click("#xp-edit");
    await flush();
    ($("#xp-path-input") as HTMLInputElement).value = "C:\\Windows\\System32";
    key("Enter", {}, "#xp-path-input");
    await flush();
    // Asked for, and not there yet: the drives are still on screen.
    expect(crumbs()).toEqual(["ACME-PC-07"]);
    expect(status()).toBe("2 drives · opening C:\\Windows\\System32…");

    target.step();
    await flush();
    expect(crumbs().at(-1)).toBe("System32");
    expect(status()).toBe("256 items so far · loading more…");
    expect($("#xp-grid").getAttribute("aria-busy")).toBe("true");
    expect($("#xp-grid").getAttribute("aria-rowcount")).toBe("257");
    expect(drawn()[0]).toBe("driver-00000.dll");

    // The technician selects two rows and sorts by size while it loads.
    click(row("driver-00003.dll"));
    click(row("driver-00005.dll"), { ctrlKey: true });
    click('[data-action="xp-sort"][data-arg="size"]');
    await flush();
    expect($('[role="columnheader"][aria-sort="descending"]').textContent).toContain("Size");
    // Where the row the technician is on lies in the part in view.
    const list = $("#xp-scroll");
    const height = (): number => {
      const top = /top:(\d+)px/.exec($(row("driver-00005.dll")).getAttribute("style") ?? "");
      return Number(top?.[1]) - list.scrollTop;
    };
    const held = height();

    target.step();
    await flush();
    expect(status()).toBe("512 items so far · loading more… · 2 selected");
    // Rows were sorted in above it, and it is still at the same height.
    expect(height()).toBe(held);
    target.step();
    await flush();
    expect(height()).toBe(held);
    expect(status()).toBe("702 items · 2 selected");
    expect($("#xp-grid").getAttribute("aria-busy")).toBe("false");
    expect($("#xp-grid").getAttribute("aria-rowcount")).toBe("703");
    expect($("#announce").textContent).toBe("C:\\Windows\\System32: 702 items.");
    expect(selected()).toContain("driver-00005.dll");

    // At the top of the list: the last page brought two folders, which come
    // first, and the order by size holds over everything that arrived.
    list.scrollTop = 0;
    list.dispatchEvent(new Event("scroll"));
    const top = drawn().slice(0, 12);
    expect(top.slice(0, 2).sort()).toEqual(["Config", "drivers"]);
    const sizes = new Map(
      (target.folders.get("C:\\Windows\\System32") ?? []).map((e) => [e.name, e.len]),
    );
    const listed = top.slice(2).map((name) => sizes.get(name) ?? -1);
    expect(listed).toEqual(listed.slice().sort((a, b) => b - a));
    expect(listed[0]).toBe(Math.max(...sizes.values()));

    // Back in the order of the names, both rows are in view: the selection
    // stayed on the same two entries through every page and every order.
    click('[data-action="xp-sort"][data-arg="name"]');
    await flush();
    expect(selected()).toEqual(["driver-00003.dll", "driver-00005.dll"]);
  });

  it("draws only the rows in view, and other rows as the list is scrolled", async () => {
    await files();
    await goTo("C:\\Windows\\System32");
    expect(status()).toBe("5000 items");
    expect($("#xp-grid").getAttribute("aria-rowcount")).toBe("5001");
    const atTop = drawn();
    expect(atTop.length).toBeLessThan(40);
    expect(atTop[0]).toBe("driver-00000.dll");
    // The box of the rows is as tall as the whole list.
    expect($("#xp-rows").getAttribute("style")).toBe(`height:${5000 * ROW_HEIGHT}px`);

    const feed = app().querySelector(".rail");
    const list = $("#xp-scroll");
    list.scrollTop = 3000 * ROW_HEIGHT;
    list.dispatchEvent(new Event("scroll"));
    // Drawn in the same turn: scrolling waits for nothing.
    const scrolled = drawn();
    expect(scrolled.length).toBeLessThan(40);
    expect(scrolled).toContain("driver-03000.dll");
    expect(scrolled).not.toContain("driver-00000.dll");
    expect($(row("driver-03000.dll")).getAttribute("style")).toBe(`top:${3000 * ROW_HEIGHT}px`);
    expect($(row("driver-03000.dll")).getAttribute("aria-rowindex")).toBe("3002");
    // Only the rows were drawn again.
    expect(app().querySelector(".rail")).toBe(feed);

    // End goes to the last row and brings it into view.
    $("#xp-grid").focus();
    key("End");
    await flush();
    expect(selected()).toEqual(["driver-04999.dll"]);
    expect(list.scrollTop).toBe(5000 * ROW_HEIGHT - 480);
    expect(drawn().at(-1)).toBe("driver-04999.dll");
  });

  it("keeps the row in focus drawn wherever the list is scrolled to", async () => {
    await files();
    await goTo("C:\\Windows\\System32");
    click(row("driver-00002.dll"));
    await flush();
    const list = $("#xp-scroll");
    list.scrollTop = 4000 * ROW_HEIGHT;
    list.dispatchEvent(new Event("scroll"));
    await flush();
    const focus = $("#xp-grid").getAttribute("aria-activedescendant");
    expect(focus).toBe("xp-row-2");
    expect(app().querySelector(`#${focus}`)?.getAttribute("data-row")).toBe("driver-00002.dll");
  });

  it("ignores the pages of a folder that was left for another", async () => {
    await files();
    target.manual = true;
    click("#xp-edit");
    await flush();
    ($("#xp-path-input") as HTMLInputElement).value = "C:\\Windows\\System32";
    key("Enter", {}, "#xp-path-input");
    await flush();
    target.step();
    await flush();
    expect(status()).toBe("256 items so far · loading more…");
    const left = target.open[0] as Listing;

    // The technician goes elsewhere before the folder is listed to its end.
    target.manual = false;
    click('[data-action="xp-go"][data-arg="C:\\\\"]');
    await flush();
    expect(crumbs()).toEqual(["ACME-PC-07", "C:\\"]);
    expect(drawn()).toEqual(["Users", "Windows", "Documents and Settings", "pagefile.sys"]);
    // A page of the folder that was left arrives late.
    backend.listeners["console-explorer"]?.({
      payload: { kind: "page", listing: left.listing, entries: many(3), end: true },
    });
    await flush();
    expect(drawn()).toEqual(["Users", "Windows", "Documents and Settings", "pagefile.sys"]);
    expect(status()).toBe("4 items · 1 selected");
  });

  it("takes nothing from an event that is not of the contract", async () => {
    await files();
    await goTo("C:\\Users");
    const emit = backend.listeners["console-explorer"];
    for (const payload of [
      null,
      "page",
      { kind: "page", listing: 1 },
      { kind: "page", listing: 99, entries: [{ name: "x" }], end: true },
      { kind: "opened", listing: 99, entries: [], end: true },
    ]) {
      emit?.({ payload });
    }
    await flush();
    expect(drawn()).toEqual(["ann", "jdoe"]);
  });
});

describe("selection and keys", () => {
  it("selects with a click, with Ctrl, with Shift and with Ctrl+A", async () => {
    await files();
    await goTo("C:\\Users\\ann\\Documents");
    click(row("a.txt"));
    await flush();
    expect(selected()).toEqual(["a.txt"]);
    expect($(row("a.txt")).classList.contains("sel")).toBe(true);
    expect(document.activeElement).toBe($("#xp-grid"));

    click(row("c.txt"), { ctrlKey: true });
    await flush();
    expect(selected()).toEqual(["a.txt", "c.txt"]);
    click(row("a.txt"), { ctrlKey: true });
    await flush();
    expect(selected()).toEqual(["c.txt"]);

    click(row("Invoices"));
    click(row("b.txt"), { shiftKey: true });
    await flush();
    expect(selected()).toEqual(["Invoices", "a.txt", "b.txt"]);
    expect(status()).toBe("5 items · 3 selected");

    key("a", { ctrlKey: true });
    await flush();
    expect(selected()).toHaveLength(5);
    expect($("#announce").textContent).toBe("5 items selected.");

    // A click beside the rows selects nothing.
    click("#xp-scroll");
    await flush();
    expect(selected()).toEqual([]);
    // A click on a column sorts and leaves the selection alone.
    click(row("b.txt"));
    click('[data-action="xp-sort"][data-arg="name"]');
    await flush();
    expect(selected()).toEqual(["b.txt"]);
    expect(drawn()).toEqual(["Invoices", "report.pdf", "c.txt", "b.txt", "a.txt"]);
    expect($("#announce").textContent).toBe("Sorted by name, descending. Folders first.");
  });

  it("moves with the arrow keys, extends with Shift, and goes to a row by its letters", async () => {
    await files();
    await goTo("C:\\Users\\ann\\Documents");
    $("#xp-grid").focus();
    key("ArrowDown");
    key("ArrowDown");
    await flush();
    expect(selected()).toEqual(["a.txt"]);
    expect($("#xp-grid").getAttribute("aria-activedescendant")).toBe($(row("a.txt")).id);
    key("ArrowDown", { shiftKey: true });
    key("ArrowDown", { shiftKey: true });
    await flush();
    expect(selected()).toEqual(["a.txt", "b.txt", "c.txt"]);
    key("Home");
    await flush();
    expect(selected()).toEqual(["Invoices"]);
    // Ctrl moves the focus and keeps the selection; the space bar adds the row.
    key("End", { ctrlKey: true });
    key(" ", { ctrlKey: true });
    await flush();
    expect(selected()).toEqual(["Invoices", "report.pdf"]);

    key("r");
    key("e");
    await flush();
    expect(selected()).toEqual(["report.pdf"]);
    key("Enter");
    await flush();
    // Enter on a file opens nothing.
    expect(crumbs().at(-1)).toBe("Documents");
  });

  it("switches between Details and List and keeps the choice while the session lasts", async () => {
    await files();
    await goTo("C:\\Users\\ann\\Documents");
    expect($("#xp-grid").classList.contains("details")).toBe(true);
    expect(app().querySelectorAll('[role="columnheader"]')).toHaveLength(4);
    click('[data-action="xp-view"][data-arg="list"]');
    await flush();
    expect($("#xp-grid").classList.contains("list")).toBe(true);
    expect(app().querySelector('[role="columnheader"]')).toBeNull();
    expect($('[data-action="xp-view"][data-arg="list"]').getAttribute("aria-pressed")).toBe("true");
    expect($("#xp-grid").getAttribute("aria-colcount")).toBe("1");
    expect($("#announce").textContent).toBe("List view.");

    // Another folder and another tab later it is still the List view.
    doubleClick(row("Invoices"));
    await flush();
    click("#tab-audit");
    click("#tab-files");
    await flush();
    expect($("#xp-grid").classList.contains("list")).toBe(true);
    expect(crumbs().at(-1)).toBe("Invoices");
  });
});

describe("the context menu", () => {
  function contextMenu(selector: string): void {
    $(selector).dispatchEvent(
      new MouseEvent("contextmenu", { bubbles: true, cancelable: true, clientX: 120, clientY: 90 }),
    );
  }

  it("offers Open and Refresh for a folder, and opens it", async () => {
    await files();
    await goTo("C:\\Users\\ann\\Documents");
    contextMenu(row("Invoices"));
    await flush();
    const menu = $("#xp-menu");
    expect(menu.getAttribute("role")).toBe("menu");
    expect(menu.getAttribute("aria-label")).toBe("Actions for Invoices");
    expect(menu.getAttribute("style")).toBe("left:120px;top:90px");
    const items = Array.from(menu.querySelectorAll<HTMLButtonElement>('[role="menuitem"]'));
    expect(items.map((item) => item.textContent)).toEqual(["OpenEnter", "RefreshF5"]);
    expect(items.map((item) => item.disabled)).toEqual([false, false]);
    // The row under the pointer is the selection, and the menu has the focus.
    expect(selected()).toEqual(["Invoices"]);
    expect(document.activeElement).toBe(items[0]);

    items[0]?.click();
    await flush();
    expect(app().querySelector("#xp-menu")).toBeNull();
    expect(crumbs().at(-1)).toBe("Invoices");
  });

  it("offers Refresh alone for a file and for the place", async () => {
    await files();
    await goTo("C:\\Users\\ann\\Documents");
    contextMenu(row("a.txt"));
    await flush();
    let items = Array.from($("#xp-menu").querySelectorAll<HTMLButtonElement>('[role="menuitem"]'));
    expect(items.map((item) => item.disabled)).toEqual([true, false]);
    // The focus is on the first action that can run.
    expect(document.activeElement).toBe(items[1]);

    // Beside the rows: the menu is the place's, and nothing is selected.
    contextMenu("#xp-scroll");
    await flush();
    expect($("#xp-menu").getAttribute("aria-label")).toBe("Actions for C:\\Users\\ann\\Documents");
    expect(selected()).toEqual([]);
    items = Array.from($("#xp-menu").querySelectorAll<HTMLButtonElement>('[role="menuitem"]'));
    const before = opened().length;
    items[1]?.click();
    await flush();
    expect(opened()).toHaveLength(before + 1);
    expect(opened().at(-1)).toBe("C:\\Users\\ann\\Documents");
  });

  it("keeps a selection of several rows when the menu opens on one of them", async () => {
    await files();
    await goTo("C:\\Users\\ann\\Documents");
    click(row("a.txt"));
    click(row("c.txt"), { shiftKey: true });
    contextMenu(row("b.txt"));
    await flush();
    expect(selected()).toEqual(["a.txt", "b.txt", "c.txt"]);
    expect($("#xp-menu").getAttribute("aria-label")).toBe("Actions for 3 items");
  });

  it("opens from the keyboard, moves with the arrows and closes with Escape without stopping", async () => {
    await files();
    await goTo("C:\\Users\\ann\\Documents");
    $("#xp-grid").focus();
    key("ArrowDown");
    key("F10", { shiftKey: true });
    await flush();
    const items = Array.from($("#xp-menu").querySelectorAll<HTMLButtonElement>('[role="menuitem"]'));
    expect(document.activeElement).toBe(items[0]);
    items[0]?.dispatchEvent(new KeyboardEvent("keydown", { key: "ArrowDown", bubbles: true }));
    expect(document.activeElement).toBe(items[1]);
    items[1]?.dispatchEvent(new KeyboardEvent("keydown", { key: "ArrowDown", bubbles: true }));
    expect(document.activeElement).toBe(items[0]);

    document.activeElement?.dispatchEvent(
      new KeyboardEvent("keydown", { key: "Escape", bubbles: true }),
    );
    await flush();
    expect(app().querySelector("#xp-menu")).toBeNull();
    expect(document.activeElement).toBe($("#xp-grid"));
    expect(asked("stop_session")).toHaveLength(0);
    // The next Escape stops the session, as always (REQ-F-018).
    key("Escape");
    await flush();
    expect(asked("stop_session")).toHaveLength(1);
  });

  it("closes when the technician clicks elsewhere", async () => {
    await files();
    await goTo("C:\\Users\\ann\\Documents");
    contextMenu(row("Invoices"));
    await flush();
    click(row("a.txt"));
    await flush();
    expect(app().querySelector("#xp-menu")).toBeNull();
    expect(selected()).toEqual(["a.txt"]);
  });
});

describe("the path field", () => {
  it("opens with Ctrl+L, and Escape leaves it before Escape stops the session", async () => {
    await files();
    await goTo("C:\\Users");
    key("l", { ctrlKey: true });
    await flush();
    const field = $("#xp-path-input") as HTMLInputElement;
    expect(field.value).toBe("C:\\Users");
    expect(document.activeElement).toBe(field);
    field.value = "C:\\Windows";
    key("Escape", {}, "#xp-path-input");
    await flush();
    expect(app().querySelector("#xp-path-input")).toBeNull();
    expect(asked("stop_session")).toHaveLength(0);
    expect(crumbs().at(-1)).toBe("Users");
    expect(document.activeElement).toBe($("#xp-edit"));
    // Typing in the field is the field's own: a letter selects no row and
    // Backspace goes nowhere.
    key("l", { ctrlKey: true });
    await flush();
    const before = opened().length;
    key("Backspace", {}, "#xp-path-input");
    key("a", { ctrlKey: true }, "#xp-path-input");
    await flush();
    expect(opened()).toHaveLength(before);
    expect(selected()).toEqual([]);
    // An empty path goes nowhere.
    ($("#xp-path-input") as HTMLInputElement).value = "   ";
    click('[data-action="xp-path-go"]');
    await flush();
    expect(opened()).toHaveLength(before);
    expect(app().querySelector("#xp-path-input")).toBeNull();
  });
});

describe("the end of the session", () => {
  it("drops what was listed when the session stops, and asks nothing more", async () => {
    await files();
    target.manual = true;
    click("#xp-edit");
    await flush();
    ($("#xp-path-input") as HTMLInputElement).value = "C:\\Windows\\System32";
    key("Enter", {}, "#xp-path-input");
    await flush();
    target.step();
    await flush();
    expect(status()).toBe("256 items so far · loading more…");
    const running = target.open[0] as Listing;

    // The customer stops the session while the folder is still arriving.
    push(consoleIn({ phase: "stopped", reason: "customer_requested" }));
    await flush();
    expect(app().querySelector('[data-screen="end"]')).not.toBeNull();
    expect(app().textContent).toContain("Ended by the customer");
    expect(app().querySelector("#xp")).toBeNull();
    const calls = backend.calls.length;
    // What still arrives of the listing belongs to no session.
    target.step();
    running.resolve({ listing: running.listing, entries: 512, state: "cancelled" });
    await flush();
    expect(backend.calls.filter(([cmd]) => cmd.startsWith("explorer_"))).toHaveLength(
      backend.calls.slice(0, calls).filter(([cmd]) => cmd.startsWith("explorer_")).length,
    );

    // The next session starts with an explorer that knows nothing of this one.
    target = new Target();
    Object.assign(backend.handlers, target.handlers());
    click('[data-action="done"]');
    push(LISTENING);
    push(FULL);
    await flush();
    const before = asked("explorer_drives").length;
    click("#tab-files");
    await flush();
    expect(asked("explorer_drives")).toHaveLength(before + 1);
    expect(drawn()).toEqual(["C:\\", "E:\\"]);
    expect($("#xp-grid").classList.contains("details")).toBe(true);
    expect($('[data-action="xp-back"]').hasAttribute("disabled")).toBe(true);
  });

  it("says that browsing has stopped when a command fails closed", async () => {
    await files();
    await goTo("C:\\Users");
    // The session is gone on the backend's side; the window has not heard yet.
    backend.handlers.explorer_open = () => {
      throw { stage: "no_consented_session", message: "no session is under full control" };
    };
    doubleClick(row("ann"));
    await flush();
    const alert = $("#xp-alert");
    expect(alert.textContent).toContain("Browsing has stopped");
    expect(alert.textContent).toContain("The session has ended. Browsing has stopped.");
    // It is not shown as a folder that could not be opened.
    expect(alert.textContent).not.toContain("could not be opened");
    expect(drawn()).toEqual(["ann", "jdoe"]);
  });

  it("says that a list is not the whole folder when the connection is lost on the way", async () => {
    await files();
    target.manual = true;
    let lose: (error: unknown) => void = () => undefined;
    const open = backend.handlers.explorer_open as Handler;
    backend.handlers.explorer_open = (args) => {
      void open(args);
      return new Promise((_, reject) => {
        lose = reject;
      });
    };
    click("#xp-edit");
    await flush();
    ($("#xp-path-input") as HTMLInputElement).value = "C:\\Windows\\System32";
    key("Enter", {}, "#xp-path-input");
    await flush();
    target.step();
    await flush();
    lose({
      stage: "disconnected",
      message: "the connection to the customer computer was lost before it answered",
    });
    await flush();
    expect(status()).toBe("256 items listed · the list is not complete");
    expect($("#xp-incomplete").textContent).toContain("This is not the whole folder.");
    expect($("#xp-alert").textContent).toContain(
      "The connection to the customer computer was lost. Browsing has stopped.",
    );
    expect($("#xp-grid").getAttribute("aria-busy")).toBe("false");
  });
});
