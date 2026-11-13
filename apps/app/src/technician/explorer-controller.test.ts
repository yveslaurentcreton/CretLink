// The file explorer driven through the window's controller (ST-M010-002),
// against a simulated customer computer: a tree of folders behind the two
// commands of the backend, which names every place by its own spelling and
// delivers a folder in pages on the `console-explorer` event, like the real
// one. Tauri and the xterm terminal are replaced by fakes.
//
// Nothing here waits for a frame or for time: a test delivers the pages it
// wants delivered and then lets the queued work run.

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import type { ConflictChoice } from "../shared/contract";
import { type TimerTracker, trackTimers } from "../shared/test-timers";
import type { ConsoleState, DeleteItem, TransferName } from "./api";
import { ROW_HEIGHT } from "./explorer";
import { consoleIn } from "./fixtures";
import {
  SimulatedComputer,
  type SimulatedLocal,
  simulatedFile,
  simulatedFiles,
  simulatedFolder,
  simulatedLink,
} from "./simulated-computer";

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

const file = simulatedFile;
const dir = simulatedFolder;
const many = simulatedFiles;

/** The customer computer of a test, delivering on the events the window listens to. */
function computer(): SimulatedComputer {
  return new SimulatedComputer(
    (event) => backend.listeners["console-explorer"]?.({ payload: event }),
    {
      transfers: (event) => backend.listeners["console-transfers"]?.({ payload: event }),
      removal: (step) => backend.listeners["console-removal"]?.({ payload: step }),
      drop: (offer) => backend.listeners["console-explorer-drop"]?.({ payload: offer }),
    },
  );
}

/** The commands of the backend, answered by `target`. */
function commands(target: SimulatedComputer): Record<string, Handler> {
  return {
    explorer_drives: () => target.listDrives(),
    explorer_open: (args) => target.openPath(String(args?.path), Number(args?.listing)),
    explorer_pick_upload: (args) => target.pickUpload(args?.kind as "files" | "folder"),
    explorer_send_check: (args) => target.sendCheck(Number(args?.offer), String(args?.folder)),
    explorer_send: (args) =>
      target.send(
        Number(args?.offer),
        String(args?.folder),
        args?.choices as Record<string, ConflictChoice>,
      ),
    explorer_choose_folder: () => target.chooseFolder(),
    explorer_download_check: (args) =>
      target.downloadCheck(args?.names as TransferName[], args?.to as number | undefined),
    explorer_download: (args) =>
      target.download(
        String(args?.folder),
        args?.names as TransferName[],
        args?.to as number | undefined,
        args?.choices as Record<string, ConflictChoice>,
      ),
    explorer_transfers: () => target.transfers(),
    explorer_cancel_transfer: (args) => target.cancelTransfer(Number(args?.transfer)),
    explorer_cancel_transfers: () => target.cancelTransfers(),
    explorer_show_folder: (args) => target.showFolder(Number(args?.transfer)),
    explorer_rename: (args) =>
      target.rename(String(args?.folder), String(args?.name), String(args?.to)),
    explorer_delete: (args) =>
      target.delete(String(args?.folder), args?.items as DeleteItem[], Number(args?.removal)),
    explorer_cancel_delete: (args) => target.cancelDelete(Number(args?.removal)),
  };
}

// --- The window ---

let state: ConsoleState = consoleIn({ phase: "idle" });
let timers: TimerTracker;
let target: SimulatedComputer;

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
    ...commands(target),
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
  target = computer();
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
    // It is said under the list too, where the technician looks.
    expect(status()).toBe(
      "4 items · 1 selected · pagefile.sys is a file. Folders open here, a file does not.",
    );
    doubleClick(row("Documents and Settings"));
    await flush();
    expect($("#announce").textContent).toBe(
      "Documents and Settings is a link. A link is shown here, not followed.",
    );
    expect(status()).toContain("A link is shown here, not followed.");
    expect(opened()).toHaveLength(before);
    expect(crumbs()).toEqual(["ACME-PC-07", "C:\\"]);
    // The next step takes the words away.
    click(row("Users"));
    await flush();
    expect(status()).toBe("4 items · 1 selected");
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
    // The button went with the refusal: the list has the keyboard.
    expect(document.activeElement).toBe($("#xp-grid"));
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
    const left = target.open[0];
    if (!left) throw new Error("the listing should still run");

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
    expect(items.map((item) => item.textContent)).toEqual([
      "OpenEnter",
      "Download",
      "Download to…",
      "RenameF2",
      "DeleteDel",
      "Upload files…",
      "Upload a folder…",
      "RefreshF5",
    ]);
    expect(items.map((item) => item.disabled)).toEqual(Array(8).fill(false));
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
    expect(items.map((item) => item.disabled)).toEqual([true, ...Array(7).fill(false)]);
    // The focus is on the first action that can run.
    expect(document.activeElement).toBe(items[1]);

    // Beside the rows: the menu is the place's, and nothing is selected.
    contextMenu("#xp-scroll");
    await flush();
    expect($("#xp-menu").getAttribute("aria-label")).toBe("Actions for C:\\Users\\ann\\Documents");
    expect(selected()).toEqual([]);
    items = Array.from($("#xp-menu").querySelectorAll<HTMLButtonElement>('[role="menuitem"]'));
    const before = opened().length;
    expect(items.at(-1)?.dataset.action).toBe("xp-refresh");
    items.at(-1)?.click();
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
    items[1]?.dispatchEvent(new KeyboardEvent("keydown", { key: "End", bubbles: true }));
    expect(document.activeElement).toBe(items.at(-1));
    // From the last action the arrow goes round to the first.
    items.at(-1)?.dispatchEvent(new KeyboardEvent("keydown", { key: "ArrowDown", bubbles: true }));
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
    const running = target.open[0];
    if (!running) throw new Error("the listing should still run");

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
    target = computer();
    Object.assign(backend.handlers, commands(target));
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

// --- The second part: send, copy to this computer, rename, delete ---

const DOCUMENTS = "C:\\Users\\ann\\Documents";

function local(name: string, len = 100): SimulatedLocal {
  return { name, kind: "file", len };
}

function localFolder(name: string, children: SimulatedLocal[]): SimulatedLocal {
  return { name, kind: "folder", children };
}

/** The Files tab in the documents folder of the customer computer. */
async function documents(): Promise<void> {
  await files();
  await goTo(DOCUMENTS);
}

/** Files and folders are dropped on the window. */
async function drop(items: SimulatedLocal[]): Promise<void> {
  target.drop(items);
  await flush();
}

/** The rows of the queue, in the order they are shown: name and state. */
function queue(): string[] {
  return Array.from(app().querySelectorAll<HTMLElement>("#xp-queue > li.q")).map(
    (row) =>
      `${row.querySelector("b")?.textContent ?? ""}: ${row.querySelector(".res")?.textContent ?? ""}`,
  );
}

function queueRow(name: string): HTMLElement {
  const found = Array.from(app().querySelectorAll<HTMLElement>("#xp-queue > li.q")).find(
    (row) => row.querySelector("b")?.textContent === name,
  );
  if (!found) throw new Error(`no transfer ${name}: ${queue().join(" | ")}`);
  return found;
}

function dialog(): HTMLElement {
  return $("#xp-question");
}

function dialogButton(label: string): HTMLButtonElement {
  const found = Array.from(dialog().querySelectorAll("button")).find(
    (button) => button.textContent === label,
  );
  if (!found) throw new Error(`no button ${label}`);
  return found;
}

function action(name: string): HTMLButtonElement {
  return $(`.xp-actions [data-action="${name}"]`) as HTMLButtonElement;
}

function announced(): string {
  return $("#announce").textContent ?? "";
}

describe("sending to the open folder", () => {
  it("sends dropped files to the open folder, each with its progress, and shows them there", async () => {
    await documents();
    target.manualTransfers = true;
    await drop([local("new.txt", 2048), local("other.txt", 10)]);

    // The folder is looked at first, by the number of the offer: no path of
    // this computer goes through the window.
    expect(asked("explorer_send_check")).toEqual([{ offer: 1, folder: DOCUMENTS }]);
    expect(asked("explorer_send")).toEqual([{ offer: 1, folder: DOCUMENTS, choices: {} }]);
    expect(app().querySelector("#xp-question")).toBeNull();
    expect(queue()).toEqual(["new.txt: Waiting", "other.txt: Waiting"]);
    expect(announced()).toBe("2 items queued to send.");
    expect($("#xp-queue-words").textContent).toBe("2 waiting");

    target.stepTransfers();
    await flush();
    expect(queue()).toEqual(["new.txt: 0% · 0 B of 2.0 KB", "other.txt: Waiting"]);
    target.stepTransfers();
    await flush();
    expect(queue()[0]).toBe("new.txt: 50% · 1.0 KB of 2.0 KB");
    const progress = queueRow("new.txt").querySelector("progress");
    expect(progress?.getAttribute("value")).toBe("50");
    expect(progress?.getAttribute("aria-label")).toBe("new.txt: 50%");
    expect(queueRow("new.txt").textContent).toContain(`${DOCUMENTS}\\new.txt`);
    // It is not done before the customer computer has it.
    expect(drawn()).not.toContain("new.txt");

    while (target.stepTransfers()) await flush();
    await flush();
    // What ended is listed newest first.
    expect(queue()).toEqual(["other.txt: Sent · 10 B", "new.txt: Sent · 2.0 KB"]);
    expect($("#xp-queue-words").textContent).toBe("2 done");
    // The folder is listed again, and the files are in it.
    expect(drawn()).toContain("new.txt");
    expect(drawn()).toContain("other.txt");
    expect(app().querySelector('[data-action="xp-cancel-transfers"]')).toBeNull();
  });

  it("asks about each name that is taken: replace, keep both or skip", async () => {
    await documents();
    await drop([local("a.txt"), local("b.txt"), local("fresh.txt")]);

    expect(asked("explorer_send")).toHaveLength(0);
    const question = dialog();
    expect(question.getAttribute("role")).toBe("dialog");
    expect(question.getAttribute("aria-modal")).toBe("true");
    expect($(`#${question.getAttribute("aria-labelledby")}`).textContent).toBe(
      "A file named a.txt is already there",
    );
    const said = $(`#${question.getAttribute("aria-describedby")}`).textContent ?? "";
    expect(said).toContain(`In ${DOCUMENTS} on ACME-PC-07.`);
    expect(said).toContain("Name 1 of 2 that is taken.");
    // The safe choice has the focus.
    expect(document.activeElement).toBe(dialogButton("Keep both"));

    dialogButton("Keep both").click();
    await flush();
    expect($("#xp-question-title").textContent).toBe("A file named b.txt is already there");
    expect($("#xp-question-text").textContent).toContain("Name 2 of 2 that is taken.");
    expect(document.activeElement).toBe(dialogButton("Keep both"));
    dialogButton("Replace").click();
    await flush();

    expect(app().querySelector("#xp-question")).toBeNull();
    expect(asked("explorer_send")).toEqual([
      { offer: 1, folder: DOCUMENTS, choices: { "a.txt": "keep_both", "b.txt": "replace" } },
    ]);
    expect(queue().sort()).toEqual([
      "a (2).txt: Sent · 100 B",
      "b.txt: Sent · 100 B",
      "fresh.txt: Sent · 100 B",
    ]);
    // What was there keeps its place; the second copy lies beside it.
    expect(drawn()).toEqual([
      "Invoices",
      "a (2).txt",
      "a.txt",
      "b.txt",
      "c.txt",
      "fresh.txt",
      "report.pdf",
    ]);
    expect(document.activeElement).toBe($("#xp-grid"));
  });

  it("applies a choice to the names that follow when asked to", async () => {
    await documents();
    await drop([local("a.txt"), local("b.txt"), local("c.txt")]);
    const all = $("#xp-conflict-all");
    expect(all.textContent).toBe("Do the same for the next 2 names that are taken");
    expect(all.getAttribute("aria-pressed")).toBe("false");
    all.click();
    await flush();
    expect($("#xp-conflict-all").getAttribute("aria-pressed")).toBe("true");
    dialogButton("Skip").click();
    await flush();
    expect(asked("explorer_send")).toEqual([
      { offer: 1, folder: DOCUMENTS, choices: { "a.txt": "skip", "b.txt": "skip", "c.txt": "skip" } },
    ]);
    expect(announced()).toBe("0 items queued to send, 3 skipped.");
    expect(app().querySelector("#xp-queue")).toBeNull();
  });

  it("does not offer Replace where a folder would take the place of a file", async () => {
    await documents();
    await drop([localFolder("a.txt", [local("inside.txt")]), local("b.txt")]);
    expect($("#xp-question-title").textContent).toBe("A file named a.txt is already there");
    expect(dialogButton("Replace").disabled).toBe(true);
    expect($("#xp-question-text").textContent).toContain(
      "Replace is not offered: a folder cannot take the place of a file.",
    );
    // Replace for all that follow still asks where it cannot apply.
    $("#xp-conflict-all").click();
    await flush();
    dialogButton("Skip").click();
    await flush();
    expect(asked("explorer_send")[0]?.choices).toEqual({ "a.txt": "skip", "b.txt": "skip" });
  });

  it("leaves the question with Escape without sending, and only then stops the session", async () => {
    await documents();
    await drop([local("a.txt")]);
    expect(app().querySelector("#xp-question")).not.toBeNull();
    key("Escape", {}, "#xp-question button");
    await flush();
    expect(app().querySelector("#xp-question")).toBeNull();
    expect(asked("explorer_send")).toHaveLength(0);
    expect(asked("stop_session")).toHaveLength(0);
    expect(announced()).toBe("Nothing was sent.");
    expect(document.activeElement).toBe($("#xp-grid"));
    key("Escape");
    await flush();
    expect(asked("stop_session")).toHaveLength(1);
  });

  it("keeps the keyboard inside the question while it is open", async () => {
    await documents();
    click(row("a.txt"));
    await drop([local("a.txt")]);
    const controls = Array.from(dialog().querySelectorAll<HTMLButtonElement>("button:not([disabled])"));
    expect(controls.map((control) => control.textContent)).toEqual([
      "Cancel the upload",
      "Skip",
      "Keep both",
      "Replace",
    ]);
    controls.at(-1)?.focus();
    const tab = new KeyboardEvent("keydown", { key: "Tab", bubbles: true, cancelable: true });
    controls.at(-1)?.dispatchEvent(tab);
    expect(tab.defaultPrevented).toBe(true);
    expect(document.activeElement).toBe(controls[0]);
    controls[0]?.dispatchEvent(
      new KeyboardEvent("keydown", { key: "Tab", shiftKey: true, bubbles: true, cancelable: true }),
    );
    expect(document.activeElement).toBe(controls.at(-1));
    // The list behind the question takes no key: nothing is deleted or moved.
    key("Delete", {}, "#xp-question button");
    key("ArrowDown", {}, "#xp-question button");
    key("F5", {}, "#xp-question button");
    await flush();
    expect($("#xp-question-title").textContent).toBe("A file named a.txt is already there");
    expect(selected()).toEqual(["a.txt"]);
    dialogButton("Cancel the upload").click();
    await flush();
    expect(app().querySelector("#xp-question")).toBeNull();
    expect(asked("explorer_send")).toHaveLength(0);
  });

  it("offers files or a folder from the system's dialog with Upload…", async () => {
    await documents();
    const upload = $("#xp-upload") as HTMLButtonElement;
    expect(upload.getAttribute("aria-haspopup")).toBe("menu");
    expect(upload.getAttribute("aria-expanded")).toBe("false");
    upload.click();
    await flush();
    expect($("#xp-upload").getAttribute("aria-expanded")).toBe("true");
    const menu = $("#xp-menu");
    expect(menu.getAttribute("aria-label")).toBe(`Upload to ${DOCUMENTS}`);
    const items = Array.from(menu.querySelectorAll<HTMLButtonElement>('[role="menuitem"]'));
    expect(items.map((item) => item.textContent)).toEqual(["Files…", "A folder…"]);
    expect(document.activeElement).toBe(items[0]);
    // Escape closes the menu, and the focus is back on the button.
    key("Escape", {}, "#xp-menu button");
    await flush();
    expect(app().querySelector("#xp-menu")).toBeNull();
    expect(document.activeElement).toBe($("#xp-upload"));
    expect(asked("stop_session")).toHaveLength(0);

    // The dialog is closed without a choice: nothing is sent.
    $("#xp-upload").click();
    await flush();
    ($("#xp-menu").querySelector('[data-action="xp-upload-files"]') as HTMLElement).click();
    await flush();
    expect(asked("explorer_pick_upload")).toEqual([{ kind: "files" }]);
    expect(asked("explorer_send_check")).toHaveLength(0);

    // A folder is picked: it goes file by file.
    target.picked.folder = [
      localFolder("Project", [local("readme.txt", 10), localFolder("src", [local("main.rs", 20)])]),
    ];
    $("#xp-upload").click();
    await flush();
    ($("#xp-menu").querySelector('[data-action="xp-upload-folder"]') as HTMLElement).click();
    await flush();
    expect(asked("explorer_pick_upload").at(-1)).toEqual({ kind: "folder" });
    expect(queue().sort()).toEqual([
      "Project\\readme.txt: Sent · 10 B",
      "Project\\src\\main.rs: Sent · 20 B",
    ]);
    expect(drawn()).toContain("Project");
    // The same two ways are in the context menu of the place.
    $("#xp-scroll").dispatchEvent(
      new MouseEvent("contextmenu", { bubbles: true, cancelable: true, clientX: 9, clientY: 9 }),
    );
    await flush();
    target.picked.files = [local("picked.txt")];
    ($("#xp-menu").querySelector('[data-action="xp-upload-files"]') as HTMLElement).click();
    await flush();
    expect(queue()).toContain("picked.txt: Sent · 100 B");
  });

  it("leaves a link out of a dropped folder and says so first", async () => {
    await documents();
    await drop([
      localFolder("Folder", [local("in.txt"), { name: "shortcut", kind: "link" }]),
      { name: "dropped-link", kind: "link" },
    ]);
    expect(queue()).toEqual([
      "dropped-link: Left out",
      "Folder\\shortcut: Left out",
      "Folder\\in.txt: Sent · 100 B",
    ]);
    expect(queueRow("Folder\\shortcut").querySelector(".why")?.textContent).toBe(
      "A link is not followed.",
    );
    expect(announced()).toBe("Transfers ended: 1 done, 2 not completed.");
  });

  it("sends nothing where no folder is open, and says why", async () => {
    await files();
    expect(($("#xp-upload") as HTMLButtonElement).disabled).toBe(true);
    await drop([local("a.txt")]);
    expect(asked("explorer_send_check")).toHaveLength(0);
    expect(status()).toContain("Open a folder first: files are sent to the folder that is open.");
    // Nor from another tab.
    await goTo(DOCUMENTS);
    click("#tab-overview");
    await drop([local("b.txt")]);
    expect(asked("explorer_send_check")).toHaveLength(0);
    expect(announced()).toContain("Files are sent from the Files tab");
  });

  it("shows where a drop goes while files are dragged over the window", async () => {
    await files();
    backend.listeners["tauri://drag-enter"]?.({ payload: { paths: ["ignored"] } });
    await flush();
    expect($("#xp-drop").textContent).toBe(
      "Open a folder first: files are sent to the folder that is open.",
    );
    backend.listeners["tauri://drag-leave"]?.({ payload: null });
    await flush();
    expect(app().querySelector("#xp-drop")).toBeNull();
    await goTo(DOCUMENTS);
    backend.listeners["tauri://drag-enter"]?.({ payload: {} });
    await flush();
    expect($("#xp-drop").textContent).toBe(`Drop to send to ${DOCUMENTS}`);
    backend.listeners["tauri://drag-drop"]?.({ payload: {} });
    await flush();
    expect(app().querySelector("#xp-drop")).toBeNull();
  });

  it("shows a folder that cannot be looked at as that refusal, and sends nothing", async () => {
    await documents();
    target.refused.set(DOCUMENTS, { stage: "access_denied", message: "access was denied" });
    await drop([local("a.txt")]);
    expect($("#xp-alert").textContent).toContain("The files could not be sent");
    expect($("#xp-alert").textContent).toContain("access was denied (access_denied)");
    expect(asked("explorer_send")).toHaveLength(0);
    expect(app().querySelector("#xp-queue")).toBeNull();
  });
});

describe("copying to this computer", () => {
  it("copies the selection to the folder of this customer computer, and shows it there", async () => {
    await documents();
    expect(action("xp-download").disabled).toBe(true);
    click(row("report.pdf"));
    await flush();
    expect(action("xp-download").disabled).toBe(false);
    action("xp-download").click();
    await flush();
    expect(asked("explorer_download_check")).toEqual([
      { names: [{ name: "report.pdf", kind: "file" }] },
    ]);
    expect(asked("explorer_download")).toEqual([
      { folder: DOCUMENTS, names: [{ name: "report.pdf", kind: "file" }], choices: {} },
    ]);
    expect(queue()).toEqual(["report.pdf: Copied · 4.9 KB"]);
    const copied = queueRow("report.pdf");
    expect(copied.textContent).toContain("leaves the customer computer");
    expect(copied.textContent).toContain(`${target.downloads}\\report.pdf`);
    expect(announced()).toBe("Transfers ended: 1 done.");

    const show = copied.querySelector('[data-action="xp-show-folder"]') as HTMLButtonElement;
    expect(show.getAttribute("aria-label")).toBe("Show report.pdf in its folder");
    show.click();
    await flush();
    expect(target.shownFolders).toEqual([target.downloads]);
    // A sent file has no folder on this computer to show.
    await drop([local("up.txt")]);
    expect(queueRow("up.txt").querySelector('[data-action="xp-show-folder"]')).toBeNull();
  });

  it("asks for a folder with Download to…, and copies nothing when none is chosen", async () => {
    await documents();
    click(row("a.txt"));
    click(row("b.txt"), { ctrlKey: true });
    await flush();
    action("xp-download-to").click();
    await flush();
    expect(asked("explorer_choose_folder")).toHaveLength(1);
    expect(asked("explorer_download_check")).toHaveLength(0);

    target.chosen = "D:\\Kept";
    action("xp-download-to").click();
    await flush();
    const names = [
      { name: "a.txt", kind: "file" },
      { name: "b.txt", kind: "file" },
    ];
    expect(asked("explorer_download_check")).toEqual([{ names, to: 1 }]);
    expect(asked("explorer_download")).toEqual([{ folder: DOCUMENTS, names, to: 1, choices: {} }]);
    expect([...target.local.keys()].sort()).toEqual(["D:\\Kept\\a.txt", "D:\\Kept\\b.txt"]);
  });

  it("asks the same way when a name is taken on this computer", async () => {
    await documents();
    target.local.set(`${target.downloads}\\report.pdf`, 1);
    click(row("report.pdf"));
    await flush();
    action("xp-download").click();
    await flush();
    expect($("#xp-question-title").textContent).toBe("A file named report.pdf is already there");
    expect($("#xp-question-text").textContent).toContain(`In ${target.downloads} on this computer.`);
    expect(dialogButton("Cancel the download")).not.toBeNull();
    dialogButton("Keep both").click();
    await flush();
    expect(asked("explorer_download")[0]?.choices).toEqual({ "report.pdf": "keep_both" });
    expect(queueRow("report.pdf").textContent).toContain(`${target.downloads}\\report (2).pdf`);
    expect(target.local.get(`${target.downloads}\\report.pdf`)).toBe(1);
  });

  it("copies a folder file by file, leaves a link out, and drops the folder's own row", async () => {
    await documents();
    target.manualTransfers = true;
    target.folders.set(`${DOCUMENTS}\\Invoices`, [
      file("2026-01.pdf", 100),
      dir("old"),
      simulatedLink("shortcut"),
    ]);
    target.folders.set(`${DOCUMENTS}\\Invoices\\old`, [file("2025-12.pdf", 50)]);
    // From the context menu of the row.
    $(row("Invoices")).dispatchEvent(
      new MouseEvent("contextmenu", { bubbles: true, cancelable: true, clientX: 9, clientY: 9 }),
    );
    await flush();
    ($("#xp-menu").querySelector('[data-action="xp-download"]') as HTMLElement).click();
    await flush();
    expect(asked("explorer_download")[0]?.names).toEqual([{ name: "Invoices", kind: "folder" }]);
    expect(queue()).toEqual(["Invoices: Waiting"]);
    target.stepTransfers();
    await flush();
    expect(queue()).toEqual(["Invoices: Listing the folder…"]);
    target.stepTransfers();
    await flush();
    // Its files stand for it now; what was left out comes first.
    expect(queue()).toEqual([
      "Invoices\\shortcut: Left out",
      "Invoices\\2026-01.pdf: Waiting",
      "Invoices\\old\\2025-12.pdf: Waiting",
    ]);
    while (target.stepTransfers()) await flush();
    await flush();
    expect([...target.local.keys()].sort()).toEqual([
      `${target.downloads}\\Invoices\\2026-01.pdf`,
      `${target.downloads}\\Invoices\\old\\2025-12.pdf`,
    ]);
  });

  it("copies nothing whose name would leave the folder, and says so", async () => {
    await documents();
    target.folders.set(`${DOCUMENTS}\\Invoices`, [
      file("..\\..\\evil.txt", 5),
      file("C:evil.txt", 5),
      file("NUL", 5),
      file("fine.txt", 5),
    ]);
    click(row("Invoices"));
    await flush();
    action("xp-download").click();
    await flush();
    expect(queue()).toEqual([
      "Invoices\\NUL: Left out",
      "Invoices\\C:evil.txt: Left out",
      "Invoices\\..\\..\\evil.txt: Left out",
      "Invoices\\fine.txt: Copied · 5 B",
    ]);
    expect([...target.local.keys()]).toEqual([`${target.downloads}\\Invoices\\fine.txt`]);
  });
});

describe("the transfer queue", () => {
  it("puts what did not arrive first with its reason, and cancels one or all", async () => {
    await documents();
    target.manualTransfers = true;
    target.failing.set(`${DOCUMENTS}\\locked.docx`, "The file is in use.");
    await drop([local("first.bin", 4000), local("locked.docx"), local("third.bin"), local("fourth.bin")]);
    expect(queue()).toEqual([
      "first.bin: Waiting",
      "locked.docx: Waiting",
      "third.bin: Waiting",
      "fourth.bin: Waiting",
    ]);
    const cancelAll = $('[data-action="xp-cancel-transfers"]') as HTMLButtonElement;
    expect(cancelAll.textContent).toBe("Cancel all");
    expect(cancelAll.getAttribute("aria-label")).toBe("Cancel all 4 transfers");

    // One waiting transfer is cancelled: it never runs.
    const cancel = queueRow("fourth.bin").querySelector('[data-action="xp-cancel-transfer"]');
    expect(cancel?.getAttribute("aria-label")).toBe("Cancel fourth.bin");
    (cancel as HTMLElement).click();
    await flush();
    expect(asked("explorer_cancel_transfer")).toHaveLength(1);
    expect(queue().at(-1)).toBe("fourth.bin: Cancelled");

    // The first runs to its end, the second fails.
    for (let i = 0; i < 6; i += 1) {
      target.stepTransfers();
      await flush();
    }
    // What did not arrive comes first, then what waits, then what ended, the
    // last one queued first.
    expect(queue()).toEqual([
      "locked.docx: Not sent",
      "third.bin: Waiting",
      "fourth.bin: Cancelled",
      "first.bin: Sent · 3.9 KB",
    ]);
    expect(queueRow("locked.docx").querySelector(".why")?.textContent).toBe("The file is in use.");
    expect($("#xp-queue-words").textContent).toBe("1 waiting · 1 failed · 1 cancelled · 1 done");
    expect(queueRow("locked.docx").querySelector("button")).toBeNull();

    // The running one is cancelled with all that wait.
    target.stepTransfers();
    await flush();
    expect(queue()[1]).toBe("third.bin: 0% · 0 B of 100 B");
    ($('[data-action="xp-cancel-transfers"]') as HTMLElement).click();
    await flush();
    while (target.stepTransfers()) await flush();
    await flush();
    expect(queue()).toEqual([
      "locked.docx: Not sent",
      "fourth.bin: Cancelled",
      "third.bin: Cancelled",
      "first.bin: Sent · 3.9 KB",
    ]);
    // Nothing that was cancelled or failed is in the folder.
    expect(drawn()).toContain("first.bin");
    expect(drawn()).not.toContain("third.bin");
    expect(drawn()).not.toContain("locked.docx");
    expect(announced()).toBe("Transfers ended: 1 done, 1 not completed, 2 cancelled.");
  });

  it("reads the queue of the session when the tab opens, and keeps the AI's transfers beside it", async () => {
    await boot();
    push(LISTENING);
    push(FULL);
    await flush();
    // A transfer queued before this window drew the tab.
    target.manualTransfers = true;
    target.picked.files = [local("early.txt")];
    const offer = target.pickUpload("files");
    target.sendCheck(offer?.offer ?? 0, DOCUMENTS);
    target.send(offer?.offer ?? 0, DOCUMENTS, {});
    backend.listeners["console-activity"]?.({
      payload: {
        entry: "item",
        item: {
          id: "call-9",
          source: "ai",
          clientLabel: "Claude Code",
          description: "Copied a file to the customer computer",
          kind: "file_received",
          operation: "copy a file to the customer computer at C:\\Users\\ann\\Downloads\\tool.exe",
          path: "C:\\Users\\ann\\Downloads\\tool.exe",
          bytes: 4096,
          state: { state: "done" },
          startedAt: new Date(T0).toISOString(),
        },
      },
    });
    click("#tab-files");
    await flush();
    expect(asked("explorer_transfers")).toHaveLength(1);
    expect(queue()).toEqual(["early.txt: Waiting"]);
    expect(app().querySelectorAll(".xp-transfers .list.files:not(.queue) .it")).toHaveLength(1);
    expect($(".xp-transfers").textContent).toContain("Claude Code");

    // The technician's own transfer is one row, also once the activity names it.
    while (target.stepTransfers()) await flush();
    backend.listeners["console-activity"]?.({
      payload: {
        entry: "item",
        item: {
          id: "action-3",
          source: "technician",
          description: "Copied a file to the customer computer",
          kind: "file_received",
          operation: `copy a file to the customer computer at ${DOCUMENTS}\\early.txt`,
          path: `${DOCUMENTS}\\early.txt`,
          bytes: 100,
          state: { state: "done" },
          startedAt: new Date(T0).toISOString(),
        },
      },
    });
    await flush();
    expect(queue()).toEqual(["early.txt: Sent · 100 B"]);
    expect(app().querySelectorAll(".xp-transfers .list.files:not(.queue) .it")).toHaveLength(1);
    // Coming back to the tab asks nothing again.
    click("#tab-overview");
    click("#tab-files");
    await flush();
    expect(asked("explorer_transfers")).toHaveLength(1);
  });

  it("ends every transfer with the session, and the screen says so", async () => {
    await documents();
    target.manualTransfers = true;
    await drop([local("running.bin", 4000), local("waiting.bin")]);
    target.stepTransfers();
    target.stepTransfers();
    await flush();
    expect(queue()[0]).toBe("running.bin: 50% · 2.0 KB of 3.9 KB");

    target.endSession();
    await flush();
    expect(queue()).toEqual([
      "waiting.bin: Stopped with the session",
      "running.bin: Stopped with the session",
    ]);
    expect(queueRow("running.bin").querySelector(".why")?.textContent).toBe(
      "The session ended before this was complete.",
    );
    const alert = $("#xp-alert").textContent ?? "";
    expect(alert).toContain("Browsing has stopped");
    expect(alert).toContain("2 transfers were stopped and are not complete.");
    // Nothing can be changed any more.
    expect(($("#xp-upload") as HTMLButtonElement).disabled).toBe(true);
    expect(app().querySelector('[data-action="xp-cancel-transfers"]')).toBeNull();
    await drop([local("late.txt")]);
    expect(asked("explorer_send_check")).toHaveLength(1);

    // The explorer goes with the session; the window still says what stopped.
    push(consoleIn({ phase: "stopped", reason: "customer_requested" }));
    await flush();
    const said = $("#alert").textContent ?? "";
    expect(said).toContain("2 transfers stopped with the session");
    expect(said).toContain("Not complete: running.bin, waiting.bin.");
  });

  it("says in the window what full control cut short, also before the queue could", async () => {
    await documents();
    target.manualTransfers = true;
    await drop([local("one.bin"), local("two.bin"), local("three.bin"), local("four.bin")]);
    // The phase changes before any transfer was told as stopped.
    push(consoleIn({ phase: "stopped", reason: "technician_requested" }));
    await flush();
    const said = $("#alert").textContent ?? "";
    expect(said).toContain("4 transfers stopped with the session");
    expect(said).toContain("Not complete: one.bin, two.bin, three.bin and 1 more.");
    // What the queue tells afterwards belongs to no explorer any more.
    target.endSession();
    await flush();
    push(LISTENING);
    push(FULL);
    await flush();
    target = computer();
    Object.assign(backend.handlers, commands(target));
    click("#tab-files");
    await flush();
    expect(app().querySelector("#xp-queue")).toBeNull();
  });
});

describe("deleting", () => {
  it("asks first, naming how many items and that there is no recycle bin", async () => {
    await documents();
    click(row("a.txt"));
    click(row("b.txt"), { ctrlKey: true });
    click(row("Invoices"), { ctrlKey: true });
    await flush();
    key("Delete");
    await flush();

    expect(asked("explorer_delete")).toHaveLength(0);
    const question = dialog();
    expect(question.getAttribute("role")).toBe("alertdialog");
    expect(question.getAttribute("aria-modal")).toBe("true");
    expect($("#xp-question-title").textContent).toBe("Delete 3 items?");
    const said = $("#xp-question-text").textContent ?? "";
    expect(said).toContain("Invoices, a.txt, b.txt.");
    expect(said).toContain("1 folder is deleted with everything in it.");
    expect(said).toContain(
      `There is no recycle bin on this path: what is deleted from ${DOCUMENTS} is gone for good.`,
    );
    // Cancel has the focus: Enter deletes nothing.
    expect(document.activeElement).toBe(dialogButton("Cancel"));

    dialogButton("Delete 3 items").click();
    await flush();
    expect(asked("explorer_delete")).toEqual([
      {
        folder: DOCUMENTS,
        items: [
          { name: "Invoices", kind: "dir" },
          { name: "a.txt", kind: "file" },
          { name: "b.txt", kind: "file" },
        ],
        removal: 1,
      },
    ]);
    // The list is what is really there.
    expect(drawn()).toEqual(["c.txt", "report.pdf"]);
    expect(announced()).toBe("3 items deleted (2 files and 1 folder in all).");
    expect($("#xp-alert").textContent).toBe("");
    expect(document.activeElement).toBe($("#xp-grid"));
  });

  it("deletes nothing when the question is cancelled", async () => {
    await documents();
    click(row("a.txt"));
    await flush();
    action("xp-delete").click();
    await flush();
    expect($("#xp-question-title").textContent).toBe("Delete a.txt?");
    expect(dialogButton("Delete")).not.toBeNull();
    dialogButton("Cancel").click();
    await flush();
    expect(app().querySelector("#xp-question")).toBeNull();
    expect(asked("explorer_delete")).toHaveLength(0);
    expect(drawn()).toContain("a.txt");
    expect(announced()).toBe("Nothing was deleted.");
  });

  it("reports what could not be removed, and why", async () => {
    await documents();
    target.folders.set(`${DOCUMENTS}\\Invoices`, [file("open.xlsx"), file("old.pdf")]);
    target.held.set(`${DOCUMENTS}\\Invoices\\open.xlsx`, "the file is in use");
    target.held.set(`${DOCUMENTS}\\c.txt`, "access was denied");
    click(row("Invoices"));
    click(row("c.txt"), { ctrlKey: true });
    click(row("b.txt"), { ctrlKey: true });
    await flush();
    // From the context menu, on the selection.
    $(row("b.txt")).dispatchEvent(
      new MouseEvent("contextmenu", { bubbles: true, cancelable: true, clientX: 9, clientY: 9 }),
    );
    await flush();
    expect($("#xp-menu").getAttribute("aria-label")).toBe("Actions for 3 items");
    ($("#xp-menu").querySelector('[data-action="xp-delete"]') as HTMLElement).click();
    await flush();
    dialogButton("Delete 3 items").click();
    await flush();

    const alert = $("#xp-alert");
    expect(alert.querySelector("b")?.textContent).toBe("2 of 3 items could not be deleted");
    expect(alert.textContent).toContain("1 of 3 items was deleted");
    const left = Array.from(alert.querySelectorAll(".xp-details li")).map((li) => li.textContent);
    expect(left).toEqual([
      `${DOCUMENTS}\\Invoices\\open.xlsx — the file is in use`,
      `${DOCUMENTS}\\Invoices — the folder is not empty`,
      `${DOCUMENTS}\\c.txt — access was denied`,
    ]);
    // Never "deleted" for what is still there.
    expect(drawn()).toEqual(["Invoices", "a.txt", "c.txt", "report.pdf"]);
    expect(announced()).toBe("2 of 3 items could not be deleted.");
  });

  it("can be cancelled while it runs, and nothing else is deleted or renamed meanwhile", async () => {
    await documents();
    target.manualDelete = true;
    click(row("a.txt"));
    click(row("b.txt"), { ctrlKey: true });
    await flush();
    key("Delete");
    await flush();
    dialogButton("Delete 2 items").click();
    await flush();
    const note = $("#xp-removal");
    expect(note.getAttribute("role")).toBe("status");
    expect(note.textContent).toContain("Deleting 2 items…");
    expect(action("xp-delete").disabled).toBe(true);
    expect(action("xp-rename").disabled).toBe(true);
    key("Delete");
    key("F2");
    await flush();
    expect(app().querySelector("#xp-question")).toBeNull();
    expect(app().querySelector("#xp-rename-input")).toBeNull();

    (note.querySelector('[data-action="xp-cancel-delete"]') as HTMLElement).click();
    await flush();
    expect(asked("explorer_cancel_delete")).toEqual([{ removal: 1 }]);
    expect($("#xp-removal").textContent).toContain("Cancelling the delete…");
    target.finishDelete();
    await flush();
    expect(app().querySelector("#xp-removal")).toBeNull();
    expect($("#xp-alert b").textContent).toBe("The delete was cancelled");
    expect($("#xp-alert").textContent).toContain("0 of 2 items were deleted. 2 items were not tried.");
    expect(drawn()).toContain("a.txt");
  });

  it("follows a delete that runs, item by item", async () => {
    await documents();
    target.manualDelete = true;
    click(row("a.txt"));
    await flush();
    key("Delete");
    await flush();
    dialogButton("Delete").click();
    await flush();
    backend.listeners["console-removal"]?.({
      payload: { removal: 1, item: "a.txt", at: 1, of: 1, removedFiles: 1200, removedDirs: 30 },
    });
    // A step of another delete is not this one's.
    backend.listeners["console-removal"]?.({
      payload: { removal: 7, item: "zzz", at: 9, of: 9, removedFiles: 1, removedDirs: 1 },
    });
    await flush();
    expect($("#xp-removal").textContent).toContain(
      "Deleting item 1 of 1: a.txt… 1200 files and 30 folders removed so far.",
    );
    target.finishDelete();
    await flush();
  });

  it("says what a delete came to when the session ended on its way", async () => {
    await documents();
    target.manualDelete = true;
    click(row("a.txt"));
    await flush();
    key("Delete");
    await flush();
    dialogButton("Delete").click();
    await flush();
    // Full control ends, and with it the explorer; then the delete answers.
    push(consoleIn({ phase: "stopped", reason: "customer_requested" }));
    await flush();
    target.endSession();
    await flush();
    const said = $("#alert").textContent ?? "";
    expect(said).toContain("The delete stopped with the session");
    expect(said).toContain("0 of 1 item were deleted. 1 item was not tried.");
  });

  it("deletes nothing at the drives", async () => {
    await files();
    click(row("C:\\"));
    await flush();
    expect(action("xp-delete").disabled).toBe(true);
    expect(action("xp-rename").disabled).toBe(true);
    key("Delete");
    key("F2");
    await flush();
    expect(app().querySelector("#xp-question")).toBeNull();
    expect(app().querySelector("#xp-rename-input")).toBeNull();
  });
});

describe("renaming", () => {
  function field(): HTMLInputElement {
    return $("#xp-rename-input") as HTMLInputElement;
  }

  it("renames in place with F2 and Enter, and selects the new name", async () => {
    await documents();
    click(row("report.pdf"));
    await flush();
    key("F2");
    await flush();
    const input = field();
    expect(input.value).toBe("report.pdf");
    expect(input.getAttribute("aria-label")).toBe("New name for report.pdf");
    expect(document.activeElement).toBe(input);
    // The name without its extension is selected to be typed anew.
    expect([input.selectionStart, input.selectionEnd]).toEqual([0, 6]);
    expect($(`#${input.getAttribute("aria-describedby")}`).textContent).toBe(
      "Press Enter to rename, Escape to leave the name as it is.",
    );
    // A letter is the field's own: it does not jump to another row.
    key("c", {}, "#xp-rename-input");
    key("Backspace", {}, "#xp-rename-input");
    await flush();
    expect(selected()).toEqual(["report.pdf"]);
    expect(crumbs().at(-1)).toBe("Documents");

    input.value = "report 2026.pdf";
    key("Enter", {}, "#xp-rename-input");
    await flush();
    expect(asked("explorer_rename")).toEqual([
      { folder: DOCUMENTS, name: "report.pdf", to: "report 2026.pdf" },
    ]);
    expect(app().querySelector("#xp-rename-input")).toBeNull();
    expect(drawn()).toContain("report 2026.pdf");
    expect(drawn()).not.toContain("report.pdf");
    expect(selected()).toEqual(["report 2026.pdf"]);
    expect(announced()).toBe("report.pdf is now report 2026.pdf.");
    expect(document.activeElement).toBe($("#xp-grid"));
  });

  it("leaves the name as it was with Escape, and takes it with a click elsewhere", async () => {
    await documents();
    click(row("a.txt"));
    await flush();
    action("xp-rename").click();
    await flush();
    field().value = "z.txt";
    key("Escape", {}, "#xp-rename-input");
    await flush();
    expect(app().querySelector("#xp-rename-input")).toBeNull();
    expect(asked("explorer_rename")).toHaveLength(0);
    expect(asked("stop_session")).toHaveLength(0);
    expect(document.activeElement).toBe($("#xp-grid"));

    // A folder's whole name is selected.
    $(row("Invoices")).dispatchEvent(
      new MouseEvent("contextmenu", { bubbles: true, cancelable: true, clientX: 9, clientY: 9 }),
    );
    await flush();
    ($("#xp-menu").querySelector('[data-action="xp-rename"]') as HTMLElement).click();
    await flush();
    expect([field().selectionStart, field().selectionEnd]).toEqual([0, 8]);
    // A click in the field is the field's own.
    click("#xp-rename-input");
    await flush();
    expect(app().querySelector("#xp-rename-input")).not.toBeNull();
    // A click on another row takes the name, as in the Windows explorer,
    // and goes on to select that row.
    field().value = "Bills";
    click(row("b.txt"));
    await flush();
    expect(app().querySelector("#xp-rename-input")).toBeNull();
    expect(asked("explorer_rename")).toEqual([{ folder: DOCUMENTS, name: "Invoices", to: "Bills" }]);
    expect(drawn()).toContain("Bills");
    expect(drawn()).not.toContain("Invoices");
    expect(selected()).toEqual(["b.txt"]);
    expect(announced()).toBe("Invoices is now Bills.");
    // The same name is no rename, by Enter or by a click.
    click(row("a.txt"));
    key("F2");
    await flush();
    key("Enter", {}, "#xp-rename-input");
    await flush();
    expect(app().querySelector("#xp-rename-input")).toBeNull();
    key("F2");
    await flush();
    click(row("c.txt"));
    await flush();
    expect(app().querySelector("#xp-rename-input")).toBeNull();
    expect(selected()).toEqual(["c.txt"]);
    expect(asked("explorer_rename")).toHaveLength(1);
    // A click outside the explorer is not the explorer's: the field stays.
    click(row("a.txt"));
    key("F2");
    await flush();
    field().value = "elsewhere.txt";
    app().dispatchEvent(new MouseEvent("click", { bubbles: true, cancelable: true }));
    await flush();
    expect(app().querySelector("#xp-rename-input")).not.toBeNull();
    expect(asked("explorer_rename")).toHaveLength(1);
  });

  it("does not also press a control with the click that sent the new name", async () => {
    await documents();
    click(row("a.txt"));
    await flush();
    key("F2");
    await flush();
    field().value = "z.txt";
    // What Delete would act on is being renamed at that moment.
    action("xp-delete").click();
    await flush();
    expect(asked("explorer_rename")).toEqual([{ folder: DOCUMENTS, name: "a.txt", to: "z.txt" }]);
    expect(app().querySelector("#xp-question")).toBeNull();
    expect(asked("explorer_delete")).toHaveLength(0);
    expect(drawn()).toContain("z.txt");
    expect(selected()).toEqual(["z.txt"]);
    // The next click is the control's own, for the entry under its new name.
    action("xp-delete").click();
    await flush();
    expect($("#xp-question").textContent).toContain("z.txt");

    // A right click elsewhere takes the name too, and opens no menu with it.
    click("#xp-question [data-action=\"xp-question-cancel\"]");
    await flush();
    click(row("b.txt"));
    key("F2");
    await flush();
    field().value = "y.txt";
    $(row("c.txt")).dispatchEvent(
      new MouseEvent("contextmenu", { bubbles: true, cancelable: true, clientX: 9, clientY: 9 }),
    );
    await flush();
    expect(asked("explorer_rename")).toHaveLength(2);
    expect(asked("explorer_rename")[1]).toEqual({ folder: DOCUMENTS, name: "b.txt", to: "y.txt" });
    expect(app().querySelector("#xp-menu")).toBeNull();
  });

  it("leaves the name and says why when a click elsewhere finds it cannot be used", async () => {
    await documents();
    click(row("a.txt"));
    await flush();
    key("F2");
    await flush();
    field().value = "B.TXT";
    click(row("c.txt"));
    await flush();
    expect(asked("explorer_rename")).toHaveLength(0);
    expect(app().querySelector("#xp-rename-input")).toBeNull();
    expect($("#xp-alert b").textContent).toBe("a.txt was not renamed");
    expect($("#xp-alert").textContent).toContain("B.TXT is already in this folder.");
    expect($("#xp-alert").textContent).toContain("The name is as it was.");
    expect(drawn()).toContain("a.txt");
    // The click went on to what it was aimed at.
    expect(selected()).toEqual(["c.txt"]);
    // A control is pressed as usual when no new name was sent.
    click(row("a.txt"));
    key("F2");
    await flush();
    field().value = "";
    action("xp-delete").click();
    await flush();
    expect(asked("explorer_rename")).toHaveLength(0);
    expect($("#xp-question").textContent).toContain("a.txt");
  });

  it("says why a name cannot be used and keeps the field open", async () => {
    await documents();
    click(row("a.txt"));
    await flush();
    key("F2");
    await flush();
    for (const [typed, why] of [
      ["B.TXT", "B.TXT is already in this folder."],
      ["sub\\a.txt", 'A name cannot hold any of \\ / : * ? " < > |.'],
      ["", "A name cannot be empty."],
      ["a.", "A name cannot end in a dot or a space."],
    ] as const) {
      field().value = typed;
      key("Enter", {}, "#xp-rename-input");
      await flush();
      expect($("#xp-alert b").textContent, typed).toBe("That name cannot be used");
      expect($("#xp-alert").textContent, typed).toContain(why);
      expect(app().querySelector("#xp-rename-input"), typed).not.toBeNull();
    }
    expect(asked("explorer_rename")).toHaveLength(0);
    field().value = "fine.txt";
    key("Enter", {}, "#xp-rename-input");
    await flush();
    expect(asked("explorer_rename")).toHaveLength(1);
    expect($("#xp-alert").textContent).toBe("");
  });

  it("shows a refusal of the customer computer and changes nothing", async () => {
    await documents();
    target.held.set(`${DOCUMENTS}\\a.txt`, "the file is in use");
    click(row("a.txt"));
    await flush();
    key("F2");
    await flush();
    field().value = "moved.txt";
    key("Enter", {}, "#xp-rename-input");
    await flush();
    expect($("#xp-alert b").textContent).toBe("a.txt could not be renamed");
    expect($("#xp-alert").textContent).toContain("the file is in use (file_in_use)");
    expect($("#xp-alert").textContent).toContain("Nothing was changed.");
    expect(drawn()).toContain("a.txt");
    expect(app().querySelector("#xp-rename-input")).toBeNull();
  });

  it("renames one entry only: not several, and not without one", async () => {
    await documents();
    expect(action("xp-rename").disabled).toBe(true);
    click(row("a.txt"));
    click(row("b.txt"), { ctrlKey: true });
    await flush();
    expect(action("xp-rename").disabled).toBe(true);
    key("F2");
    await flush();
    expect(app().querySelector("#xp-rename-input")).toBeNull();
  });
});

describe("changes and the session", () => {
  it("says that the session has ended when a change fails closed, and offers no more", async () => {
    await documents();
    target.ended = { stage: "no_consented_session", message: "no session is under full control" };
    click(row("a.txt"));
    await flush();
    key("F2");
    await flush();
    ($("#xp-rename-input") as HTMLInputElement).value = "z.txt";
    key("Enter", {}, "#xp-rename-input");
    await flush();
    const alert = $("#xp-alert").textContent ?? "";
    expect(alert).toContain("Browsing has stopped");
    expect(alert).toContain("The session has ended. Browsing has stopped.");
    expect(alert).not.toContain("could not be renamed");
    for (const name of ["xp-download", "xp-download-to", "xp-rename", "xp-delete"]) {
      expect(action(name).disabled, name).toBe(true);
    }
    expect(($("#xp-upload") as HTMLButtonElement).disabled).toBe(true);
    key("Delete");
    await flush();
    expect(app().querySelector("#xp-question")).toBeNull();
  });

  it("offers each action as a button, in the menu and by its key alike", async () => {
    await documents();
    click(row("a.txt"));
    await flush();
    const buttons = Array.from(
      app().querySelectorAll<HTMLButtonElement>(".xp-actions button"),
    ).map((button) => [button.textContent, button.getAttribute("aria-keyshortcuts")]);
    expect(buttons).toEqual([
      ["Upload…", null],
      ["Download", null],
      ["Download to…", null],
      ["RenameF2", "F2"],
      ["DeleteDel", "Delete"],
    ]);
    expect($(".xp-actions").getAttribute("role")).toBe("toolbar");
    key("F10", { shiftKey: true });
    await flush();
    const inMenu = Array.from(
      $("#xp-menu").querySelectorAll<HTMLButtonElement>('[role="menuitem"]'),
    ).map((item) => item.dataset.action);
    for (const button of app().querySelectorAll<HTMLButtonElement>(".xp-actions button")) {
      const name = button.dataset.action ?? "";
      if (name !== "xp-upload") expect(inMenu, name).toContain(name);
    }
    expect(inMenu).toContain("xp-upload-files");
    expect(inMenu).toContain("xp-upload-folder");
  });
});

