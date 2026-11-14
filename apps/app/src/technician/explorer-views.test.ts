// Rendering tests of the file explorer (ST-M010-002), from the story's
// acceptance criteria: the place, the navigation, the two views, the grid a
// screen reader reads, the rows in view of thousands, and each honest state.
// The Files tab is a pure function of the snapshot, rendered in jsdom.

import { afterEach, describe, expect, it } from "vitest";
import { OVERSCAN, QUEUE_ROWS, ROW_HEIGHT, applyTransfers } from "./explorer";
import { DOWNLOADS, EXPLORERS, FEED, FIXTURES, XSS, XSS_NAME, transfer } from "./fixtures";
import type { TechnicianSnapshot } from "./model";
import { explorerRows } from "./views/explorer";
import { renderTechnician } from "./views/shell";

function show(name: string, patch: Partial<TechnicianSnapshot> = {}): HTMLElement {
  const fixture = FIXTURES[name];
  if (!fixture) throw new Error(`no fixture ${name}`);
  const root = renderTechnician({ ...fixture, ...patch });
  document.body.replaceChildren(root);
  return root;
}

function text(el: Element | null | undefined): string {
  return (el?.textContent ?? "").replace(/\s+/g, " ").trim();
}

/** The rows that are drawn, in the order of the list, each as its cells. */
function rows(root: HTMLElement): string[][] {
  return Array.from(root.querySelectorAll<HTMLElement>("#xp-rows [role='row']"))
    .sort((a, b) => Number(a.getAttribute("aria-rowindex")) - Number(b.getAttribute("aria-rowindex")))
    .map((row) => Array.from(row.querySelectorAll("[role='gridcell']")).map(text));
}

function names(root: HTMLElement): string[] {
  return rows(root).map((cells) => cells[0] ?? "");
}

function buttonNamed(root: HTMLElement, name: string): HTMLButtonElement | undefined {
  return Array.from(root.querySelectorAll("button")).find(
    (b) => (b.getAttribute("aria-label") ?? text(b)) === name,
  );
}

afterEach(() => document.body.replaceChildren());

describe("the place", () => {
  it("makes the Files tab the explorer, with the transfers of the session under it", () => {
    const root = show("files-details");
    const panel = root.querySelector("#panel-files");
    expect(panel?.querySelector("#xp")).not.toBeNull();
    expect(panel?.hasAttribute("hidden")).toBe(false);
    expect(panel?.querySelector("#xp-grid")).not.toBeNull();
    const transfers = panel?.querySelector(".xp-transfers");
    expect(text(transfers?.querySelector("h3"))).toBe("Transfers");
    const moved = transfers?.querySelectorAll(".list.files .it") ?? [];
    expect(moved).toHaveLength(2);
    expect(text(moved[0])).toContain("Received");
    expect(text(moved[1])).toContain("left the customer computer");
    // Without transfers the panel says so, and the explorer is still there.
    const none = show("files-drives");
    expect(text(none.querySelector(".xp-transfers"))).toContain(
      "No files were sent or received in this session.",
    );
  });

  it("is in the window whichever tab is shown, and hidden with its tab", () => {
    const root = show("files-details", { tab: "overview" });
    expect(root.querySelector("#panel-files")?.hasAttribute("hidden")).toBe(true);
    expect(root.querySelector("#xp-grid")).not.toBeNull();
  });
});

describe("navigation", () => {
  it("names the place in a path bar of buttons, under the customer computer's name", () => {
    const root = show("files-details");
    const bar = root.querySelector("nav.xp-path");
    expect(bar?.getAttribute("aria-label")).toBe("Place on ACME-PC-07");
    const parts = Array.from(bar?.querySelectorAll<HTMLButtonElement>(".xp-crumb") ?? []);
    expect(parts.map(text)).toEqual(["ACME-PC-07", "C:\\", "Users", "ann", "Documents"]);
    expect(parts.map((part) => part.tagName)).toEqual(Array(5).fill("BUTTON"));
    expect(parts[0]?.dataset.action).toBe("xp-drives");
    expect(parts[0]?.getAttribute("aria-label")).toBe("The drives of ACME-PC-07");
    expect(parts.slice(1).map((part) => [part.dataset.action, part.dataset.arg])).toEqual([
      ["xp-go", "C:\\"],
      ["xp-go", "C:\\Users"],
      ["xp-go", "C:\\Users\\ann"],
      ["xp-go", "C:\\Users\\ann\\Documents"],
    ]);
    // Where the explorer stands is marked in words for a screen reader.
    expect(parts.map((part) => part.getAttribute("aria-current"))).toEqual([
      null,
      null,
      null,
      null,
      "location",
    ]);
    const edit = buttonNamed(root, "Type a path");
    expect(edit?.dataset.action).toBe("xp-edit");
    expect(edit?.getAttribute("aria-keyshortcuts")).toBe("Control+L");

    const drives = show("files-drives");
    expect(Array.from(drives.querySelectorAll(".xp-crumb")).map(text)).toEqual(["ACME-PC-07"]);
    expect(drives.querySelector(".xp-crumb")?.getAttribute("aria-current")).toBe("location");
  });

  it("can be typed in: a field with the path, Go and Cancel", () => {
    const root = show("files-path");
    expect(root.querySelector("nav.xp-path")).toBeNull();
    const field = root.querySelector<HTMLInputElement>("#xp-path-input");
    expect(field?.value).toBe("C:\\Users\\ann\\Documents");
    expect(text(root.querySelector('label[for="xp-path-input"]'))).toBe("Path on ACME-PC-07");
    expect(text(root.querySelector(`#${field?.getAttribute("aria-describedby")}`))).toBe(
      "Press Enter to go there, Escape to leave the field.",
    );
    expect(buttonNamed(root, "Go")?.dataset.action).toBe("xp-path-go");
    expect(buttonNamed(root, "Cancel")?.dataset.action).toBe("xp-path-cancel");
  });

  it("offers back, forward, up and refresh, each saying where it leads", () => {
    const root = show("files-details");
    const toolbar = root.querySelector('[role="toolbar"]');
    expect(toolbar?.getAttribute("aria-label")).toBe("Go to another place");
    const back = buttonNamed(root, "Back to the drives");
    expect(back?.dataset.action).toBe("xp-back");
    expect(back?.disabled).toBe(false);
    expect(back?.getAttribute("aria-keyshortcuts")).toBe("Alt+ArrowLeft");
    const forward = buttonNamed(root, "Forward");
    expect(forward?.dataset.action).toBe("xp-forward");
    expect(forward?.disabled).toBe(true);
    const up = buttonNamed(root, "Up to C:\\Users\\ann");
    expect(up?.dataset.action).toBe("xp-up");
    expect(up?.getAttribute("aria-keyshortcuts")).toBe("Backspace");
    const refresh = buttonNamed(root, "Refresh");
    expect(refresh?.dataset.action).toBe("xp-refresh");
    expect(refresh?.getAttribute("aria-keyshortcuts")).toBe("F5");

    // At the drives there is nothing above and nothing behind.
    const drives = show("files-drives");
    expect(buttonNamed(drives, "Up")?.disabled).toBe(true);
    expect(buttonNamed(drives, "Back")?.disabled).toBe(true);
    // From a drive's root, up leads to the drives.
    const root2 = show("files-empty", {
      explorer: { ...EXPLORERS["files-empty"]!, place: { kind: "folder", path: "E:\\" } },
    });
    expect(buttonNamed(root2, "Up to the drives")?.disabled).toBe(false);
  });

  it("lists the drives with their label and letter, their kind in words and their space", () => {
    const root = show("files-drives");
    expect(root.querySelector("#xp-grid")?.getAttribute("aria-label")).toBe("Drives of ACME-PC-07");
    expect(Array.from(root.querySelectorAll('[role="columnheader"]')).map(text)).toEqual([
      "Name",
      "Type",
      "Total size",
      "Free space",
    ]);
    // The drives are not sorted: they come in the customer computer's order.
    expect(root.querySelector('[role="columnheader"] button')).toBeNull();
    expect(rows(root)).toEqual([
      ["Windows (C:)", "Local disk", "475.9 GB", "119.6 GB"],
      // What the customer computer could not read is said to be unknown, never zero.
      ["D:", "CD or DVD drive", "Not known", "Not known"],
      ["TOOLS (E:)", "Removable drive", "29.7 GB", "8.6 GB"],
      ["Z:", "Network drive", "Not known", "Not known"],
    ]);
    expect(text(root.querySelector("#xp-status"))).toBe("4 drives");
  });
});

describe("views", () => {
  it("shows name, date modified, type and size in Details, folders first", () => {
    const root = show("files-details");
    expect(Array.from(root.querySelectorAll('[role="columnheader"]')).map(text)).toEqual([
      "Name▲",
      "Date modified",
      "Type",
      "Size",
    ]);
    expect(rows(root)).toEqual([
      ["Invoices", "2026-11-03 09:30", "File folder", ""],
      ["Photos", "2026-10-04 14:30", "File folder", ""],
      ["Scans", "2026-11-01 14:30", "File folder", ""],
      ["Budget 2026.xlsx", "2026-10-31 14:30", "XLSX file", "47.1 KB"],
      ["desktop.ini", "2024-12-03 14:30", "INI file", "402 B"],
      ["letter to the bank.docx", "2026-11-03 13:00", "DOCX file", "18.5 KB"],
      ["notes.txt", "2026-11-03 14:18", "TXT file", "1.2 KB"],
      ["Old laptop", "2025-09-29 14:30", "Link", ""],
      ["report.pdf", "2026-11-02 14:30", "PDF file", "1.2 MB"],
      // A number in a name counts by its value: 2 before 10.
      ["setup-log-2.txt", "2026-11-01 12:30", "TXT file", "89.2 KB"],
      ["setup-log-10.txt", "2026-11-02 12:30", "TXT file", "86.0 KB"],
    ]);
  });

  it("sorts by the column that was clicked and says so on the column", () => {
    const root = show("files-details");
    const heads = Array.from(root.querySelectorAll('[role="columnheader"]'));
    expect(heads.map((head) => head.getAttribute("aria-sort"))).toEqual([
      "ascending",
      "none",
      "none",
      "none",
    ]);
    expect(heads.map((head) => head.querySelector("button")?.dataset.action)).toEqual(
      Array(4).fill("xp-sort"),
    );
    expect(heads.map((head) => head.querySelector("button")?.dataset.arg)).toEqual([
      "name",
      "modified",
      "type",
      "size",
    ]);

    const bySize = show("files-sorted");
    const sorted = Array.from(bySize.querySelectorAll('[role="columnheader"]'));
    expect(sorted.map((head) => head.getAttribute("aria-sort"))).toEqual([
      "none",
      "none",
      "none",
      "descending",
    ]);
    expect(text(sorted[3])).toBe("Size▼");
    // The arrow is for the eye; the direction is in `aria-sort`.
    expect(sorted[3]?.querySelector(".xp-arrow")?.getAttribute("aria-hidden")).toBe("true");
    // Folders stay first; the largest file leads the files.
    expect(names(bySize).slice(0, 5)).toEqual([
      "Scans",
      "Photos",
      "Invoices",
      "report.pdf",
      "setup-log-2.txt",
    ]);
  });

  it("shows the names alone in List, running down a column and on in the next", () => {
    const root = show("files-list");
    const grid = root.querySelector("#xp-grid");
    expect(grid?.classList.contains("list")).toBe(true);
    expect(root.querySelector('[role="columnheader"]')).toBeNull();
    expect(grid?.getAttribute("aria-colcount")).toBe("1");
    expect(grid?.getAttribute("aria-rowcount")).toBe("11");
    expect(rows(root).every((cells) => cells.length === 1)).toBe(true);
    expect(names(root).slice(0, 4)).toEqual(["Invoices", "Photos", "Scans", "Budget 2026.xlsx"]);
    // The choice between the two views says which one is on.
    const choice = Array.from(root.querySelectorAll<HTMLButtonElement>('[data-action="xp-view"]'));
    expect(choice.map((b) => [text(b), b.dataset.arg, b.getAttribute("aria-pressed")])).toEqual([
      ["Details", "details", "false"],
      ["List", "list", "true"],
    ]);
    expect(choice[0]?.parentElement?.getAttribute("aria-label")).toBe("View");

    const large = show("files-large-list");
    const perColumn = Math.floor(480 / ROW_HEIGHT);
    const drawn = Array.from(large.querySelectorAll<HTMLElement>("#xp-rows [role='row']"));
    // Three columns and a part of the fourth are in view; one more is drawn.
    expect(drawn).toHaveLength(5 * perColumn);
    expect(drawn[0]?.getAttribute("style")).toBe("top:0px;left:0px");
    expect(drawn[perColumn]?.getAttribute("style")).toBe("top:0px;left:260px");
    expect(large.querySelector("#xp-rows")?.getAttribute("style")).toBe(
      `height:${perColumn * ROW_HEIGHT}px;width:${Math.ceil(5000 / perColumn) * 260}px`,
    );
  });
});

describe("the grid", () => {
  it("is a grid a screen reader can read: rows, cells, and the place of each among all", () => {
    const root = show("files-details");
    const grid = root.querySelector("#xp-grid");
    expect(grid?.getAttribute("role")).toBe("grid");
    expect(grid?.getAttribute("aria-label")).toBe("Files and folders in C:\\Users\\ann\\Documents");
    // The header is a row too.
    expect(grid?.getAttribute("aria-rowcount")).toBe("12");
    expect(grid?.getAttribute("aria-colcount")).toBe("4");
    expect(grid?.getAttribute("aria-multiselectable")).toBe("true");
    expect(grid?.getAttribute("aria-busy")).toBe("false");
    // It takes the keyboard itself, and says which row the keys are on.
    expect(grid?.getAttribute("tabindex")).toBe("0");
    expect(grid?.getAttribute("aria-describedby")).toBe("xp-status");

    const head = grid?.querySelector(".xp-head");
    expect(head?.getAttribute("role")).toBe("row");
    expect(head?.getAttribute("aria-rowindex")).toBe("1");
    const drawn = Array.from(grid?.querySelectorAll<HTMLElement>("#xp-rows [role='row']") ?? []);
    expect(drawn.map((row) => row.getAttribute("aria-rowindex"))).toEqual(
      Array.from({ length: 11 }, (_, n) => String(n + 2)),
    );
    const cells = Array.from(drawn[3]?.querySelectorAll('[role="gridcell"]') ?? []);
    expect(cells.map((cell) => cell.getAttribute("aria-colindex"))).toEqual(["1", "2", "3", "4"]);
    // Every row is in a group of rows inside the grid.
    expect(drawn.every((row) => row.closest('[role="rowgroup"]')?.closest('[role="grid"]') === grid)).toBe(
      true,
    );
  });

  it("says which rows are selected and which one the keyboard is on", () => {
    const root = show("files-details");
    const drawn = Array.from(root.querySelectorAll<HTMLElement>("#xp-rows [role='row']"));
    const selected = drawn.filter((row) => row.getAttribute("aria-selected") === "true");
    expect(selected.map((row) => row.dataset.row)).toEqual(["Invoices", "report.pdf"]);
    // Every other row says that it is not.
    expect(drawn.filter((row) => row.getAttribute("aria-selected") === "false")).toHaveLength(9);
    // A selected row is marked by more than its colour: a bar, and the count in words.
    expect(selected.every((row) => row.classList.contains("sel"))).toBe(true);
    expect(text(root.querySelector("#xp-status"))).toBe("11 items · 2 selected");
    const focus = root.querySelector("#xp-grid")?.getAttribute("aria-activedescendant");
    expect(root.querySelector(`#${focus}`)?.getAttribute("data-row")).toBe("report.pdf");
    expect(root.querySelector(`#${focus}`)?.classList.contains("focus")).toBe(true);
    // Without a row in focus the grid names none.
    expect(show("files-drives").querySelector("#xp-grid")?.hasAttribute("aria-activedescendant")).toBe(
      false,
    );
  });

  it("draws only the rows in view of a folder with thousands, at their place in a box as tall as the list", () => {
    const root = show("files-large");
    expect(text(root.querySelector("#xp-status"))).toBe("5000 items");
    expect(root.querySelector("#xp-grid")?.getAttribute("aria-rowcount")).toBe("5001");
    const drawn = Array.from(root.querySelectorAll<HTMLElement>("#xp-rows [role='row']"));
    expect(drawn).toHaveLength(Math.ceil(480 / ROW_HEIGHT) + 2 * OVERSCAN);
    // The list was scrolled to its 2500th row.
    expect(drawn[0]?.dataset.row).toBe(`driver-${2500 - OVERSCAN}.dll`.replace("-2", "-02"));
    expect(drawn[0]?.getAttribute("style")).toBe(`top:${(2500 - OVERSCAN) * ROW_HEIGHT}px`);
    expect(drawn[0]?.getAttribute("aria-rowindex")).toBe(String(2500 - OVERSCAN + 2));
    expect(root.querySelector("#xp-rows")?.getAttribute("style")).toBe(
      `height:${5000 * ROW_HEIGHT}px`,
    );
    // Rows are told apart by name, so one that stays in view is not drawn anew.
    expect(drawn[0]?.dataset.key).toBe(`r-${drawn[0]?.dataset.row}`);
  });

  it("draws the rows alone exactly as it draws them in the tab", () => {
    for (const name of ["files-details", "files-large", "files-large-list", "files-drives"]) {
      const root = show(name);
      const alone = explorerRows(EXPLORERS[name]!);
      expect(alone.outerHTML, name).toBe(root.querySelector("#xp-rows")?.outerHTML);
    }
  });
});

describe("honest states", () => {
  const state = (root: HTMLElement): string => text(root.querySelector("#xp-state"));
  const alert = (root: HTMLElement): string => text(root.querySelector("#xp-alert"));

  it("says that nothing was asked yet before the tab is used", () => {
    const root = show("files-closed");
    expect(state(root)).toContain("Look through the files of ACME-PC-07.");
    expect(state(root)).toContain("The customer sees that you are browsing, and where.");
    expect(buttonNamed(root, "Show the drives")?.dataset.action).toBe("xp-drives");
    expect(text(root.querySelector("#xp-status"))).toBe("");
  });

  it("says that it is opening or loading, never that a folder is empty", () => {
    const opening = show("files-opening-first");
    expect(state(opening)).toBe("Reading the drives of ACME-PC-07…");
    expect(opening.querySelector("#xp-grid")?.getAttribute("aria-busy")).toBe("true");
    expect(text(opening.querySelector("#xp-status"))).toBe("Opening…");

    const loading = show("files-loading");
    expect(state(loading)).toBe("Loading…");
    expect(loading.querySelector("#xp-state .spinner")).not.toBeNull();
    expect(loading.querySelector("#xp-grid")?.getAttribute("aria-busy")).toBe("true");
    expect(text(loading.querySelector("#xp-status"))).toBe("0 items so far · loading more…");

    const more = show("files-loading-more");
    expect(more.querySelector("#xp-state")).toBeNull();
    expect(text(more.querySelector("#xp-status"))).toBe("256 items so far · loading more…");
    expect(more.querySelector("#xp-grid")?.getAttribute("aria-busy")).toBe("true");
  });

  it("says that a folder is empty only when its whole listing is empty", () => {
    const empty = show("files-empty");
    expect(state(empty)).toBe("This folder is empty.");
    expect(empty.querySelector("#xp-grid")?.getAttribute("aria-busy")).toBe("false");
    expect(text(empty.querySelector("#xp-status"))).toBe("0 items");
    // No other state of the explorer reads as an empty folder.
    for (const name of Object.keys(EXPLORERS)) {
      if (name === "files-empty") continue;
      expect(text(show(name)), name).not.toContain("This folder is empty.");
    }
  });

  it("shows a place that was refused as that refusal, and stays where it was", () => {
    const root = show("files-refused");
    const region = root.querySelector("#xp-alert");
    expect(region?.getAttribute("role")).toBe("alert");
    expect(text(region?.querySelector("b"))).toBe("C:\\Users\\jdoe could not be opened");
    expect(alert(root)).toContain("access was denied (access_denied)");
    expect(text(region?.querySelector(".next"))).toBe(
      "You are still in C:\\Users\\ann\\Documents. Check the path, or open the folder step by step from the drives.",
    );
    expect(region?.querySelector('[data-action="xp-dismiss"]')).not.toBeNull();
    // The folder it was in is still on screen, whole.
    expect(names(root)).toHaveLength(11);
    expect(text(root.querySelector('.xp-crumb[aria-current="location"]'))).toBe("Documents");

    const first = show("files-refused-first");
    expect(alert(first)).toContain("The drives could not be listed");
    expect(alert(first)).toContain("not supported on this computer (unsupported)");
    expect(state(first)).toContain("Nothing is open.");
    expect(buttonNamed(first, "Show the drives")).toBeDefined();
    // The region is there, empty, when nothing was refused.
    expect(alert(show("files-details"))).toBe("");
    expect(show("files-details").querySelector("#xp-alert")?.getAttribute("role")).toBe("alert");
  });

  it("says that a list is not the whole folder, with why", () => {
    const cut = show("files-incomplete");
    const note = cut.querySelector("#xp-incomplete");
    expect(text(note?.querySelector("p"))).toBe(
      "This is not the whole folder. access was denied (access_denied)",
    );
    expect(text(note?.querySelector("button"))).toBe("Refresh");
    expect(note?.querySelector("button")?.dataset.action).toBe("xp-refresh");
    expect(text(cut.querySelector("#xp-status"))).toBe("256 items listed · the list is not complete");
    expect(cut.querySelector("#xp-grid")?.getAttribute("aria-busy")).toBe("false");

    const limit = show("files-limit");
    expect(text(limit.querySelector("#xp-incomplete"))).toBe(
      "This is not the whole folder. It holds more than the 300 entries CretLink lists. Open a subfolder, or use the terminal for the rest.",
    );
    expect(text(limit.querySelector("#xp-status"))).toBe(
      "The first 300 items · this folder holds more than are listed",
    );
    expect(show("files-details").querySelector("#xp-incomplete")).toBeNull();
  });

  it("says that browsing has stopped when the session ended", () => {
    const root = show("files-ended");
    expect(text(root.querySelector("#xp-alert b"))).toBe("Browsing has stopped");
    expect(alert(root)).toContain("The session has ended. Browsing has stopped.");
    expect(text(root.querySelector("#xp-status"))).toBe("256 items listed · the list is not complete");
    // Nothing can be listed again without a session: no Refresh is offered there.
    expect(root.querySelector("#xp-incomplete button")).toBeNull();
    expect(root.querySelector('#xp-alert [data-action="xp-dismiss"]')).toBeNull();
  });

  it("says where it is going while the next place opens, with the place before it still shown", () => {
    const root = show("files-opening");
    expect(text(root.querySelector("#xp-status"))).toBe(
      "11 items · opening C:\\Users\\ann\\Documents\\Invoices…",
    );
    expect(root.querySelector("#xp-grid")?.getAttribute("aria-busy")).toBe("true");
    expect(names(root)).toHaveLength(11);

    const refreshing = show("files-refreshing");
    expect(text(refreshing.querySelector("#xp-status"))).toBe("11 items · 1 selected · refreshing…");
    expect(names(refreshing)).toHaveLength(11);
    expect(refreshing.querySelector("#xp-grid")?.getAttribute("aria-busy")).toBe("true");
  });
});

describe("the context menu", () => {
  it("offers what the explorer can do with a folder, with the key for each", () => {
    const root = show("files-menu");
    const menu = root.querySelector("#xp-menu");
    expect(menu?.getAttribute("role")).toBe("menu");
    expect(menu?.getAttribute("aria-label")).toBe("Actions for Invoices");
    expect(menu?.getAttribute("style")).toBe("left:320px;top:260px");
    const items = Array.from(menu?.querySelectorAll<HTMLButtonElement>('[role="menuitem"]') ?? []);
    expect(items.map((item) => text(item.querySelector("span")))).toEqual([
      "Open",
      "Download",
      "Download to…",
      "Rename",
      "Delete",
      "Upload files…",
      "Upload a folder…",
      "Refresh",
    ]);
    expect(items.map((item) => item.dataset.action)).toEqual([
      "xp-menu-open",
      "xp-download",
      "xp-download-to",
      "xp-rename",
      "xp-delete",
      "xp-upload-files",
      "xp-upload-folder",
      "xp-refresh",
    ]);
    expect(items.map((item) => item.getAttribute("aria-keyshortcuts"))).toEqual([
      "Enter",
      null,
      null,
      "F2",
      "Delete",
      null,
      null,
      "F5",
    ]);
    expect(items.map((item) => item.disabled)).toEqual(Array(8).fill(false));
    expect(items.every((item) => item.parentElement?.getAttribute("role") === "none")).toBe(true);
    // A line parts what is done with the rows from what is done with the place.
    expect(
      items.filter((item) => item.parentElement?.classList.contains("sep")).map((item) => item.dataset.action),
    ).toEqual(["xp-download", "xp-upload-files", "xp-refresh"]);
  });

  it("offers Refresh alone for a file and for the place itself", () => {
    const file = show("files-menu-file");
    expect(
      Array.from(file.querySelectorAll<HTMLButtonElement>('#xp-menu [role="menuitem"]')).map(
        (item) => item.disabled,
      ),
    ).toEqual([true, false, false, false, false, false, false, false]);
    const place = show("files-menu-place");
    expect(place.querySelector("#xp-menu")?.getAttribute("aria-label")).toBe(
      "Actions for C:\\Users\\ann\\Documents",
    );
    // Without a row there is nothing to open, copy, rename or delete; the
    // place still takes an upload and a refresh.
    expect(
      Array.from(place.querySelectorAll<HTMLButtonElement>('#xp-menu [role="menuitem"]')).map(
        (item) => item.disabled,
      ),
    ).toEqual([true, true, true, true, true, false, false, false]);
    expect(show("files-details").querySelector("#xp-menu")).toBeNull();
  });
});

describe("what comes from the customer computer", () => {
  it("writes names and paths as text, never as markup (REQ-S-016, REQ-S-018)", () => {
    const root = show("files-xss");
    expect(root.querySelector("#xp img, #xp script, #xp b:not(.alert-body b)")).toBeNull();
    expect(names(root)).toEqual([`${XSS}.dir`, XSS]);
    expect(root.querySelector(".xp-name")?.getAttribute("title")).toBe(`${XSS}.dir`);
    expect(Array.from(root.querySelectorAll(".xp-crumb")).map(text)).toEqual([
      "ACME-PC-07",
      "C:\\",
      XSS_NAME,
    ]);
    expect(root.querySelector("#xp-grid")?.getAttribute("aria-label")).toBe(
      `Files and folders in C:\\${XSS_NAME}`,
    );
    // A refusal and a drive label are text too.
    const refused = show("files-refused", {
      explorer: {
        ...EXPLORERS["files-refused"]!,
        refusal: { title: XSS, message: XSS, next: XSS },
      },
    });
    expect(refused.querySelector("#xp-alert img, #xp-alert script")).toBeNull();
    expect(text(refused.querySelector("#xp-alert p"))).toBe(XSS);
    const drives = show("files-drives", {
      explorer: {
        ...EXPLORERS["files-drives"]!,
        drives: [{ root: "C:\\", kind: "fixed", label: XSS }],
      },
    });
    expect(drives.querySelector("#xp img, #xp script")).toBeNull();
    expect(names(drives)).toEqual([`${XSS} (C:)`]);
  });

  it("makes every control of the explorer a native button with a name", () => {
    for (const name of Object.keys(EXPLORERS)) {
      const root = show(name);
      for (const control of root.querySelectorAll<HTMLElement>("#xp [data-action]")) {
        expect(control.tagName, name).toBe("BUTTON");
        expect(control.getAttribute("aria-label") ?? text(control), name).not.toBe("");
      }
      // The rows take no handlers of their own: the grid reads the mouse and the keys.
      expect(root.querySelector("#xp-rows [data-action]"), name).toBeNull();
    }
  });
});

// --- The second part: send, copy to this computer, rename, delete ---

const FOLDER = "C:\\Users\\ann\\Documents";

/** The rows of the queue as they are drawn: name and state in words. */
function queued(root: HTMLElement): string[] {
  return Array.from(root.querySelectorAll<HTMLElement>("#xp-queue > li.q")).map(
    (row) => `${text(row.querySelector("b"))}: ${text(row.querySelector(".res"))}`,
  );
}

function queuedRow(root: HTMLElement, name: string): HTMLElement {
  const found = Array.from(root.querySelectorAll<HTMLElement>("#xp-queue > li.q")).find(
    (row) => text(row.querySelector("b")) === name,
  );
  if (!found) throw new Error(`no transfer ${name}`);
  return found;
}

describe("the actions of the explorer", () => {
  it("offers upload, download, rename and delete as buttons with their keys", () => {
    const root = show("files-details");
    const bar = root.querySelector(".xp-actions");
    expect(bar?.getAttribute("role")).toBe("toolbar");
    expect(bar?.getAttribute("aria-label")).toBe("Send, copy and change");
    const buttons = Array.from(bar?.querySelectorAll<HTMLButtonElement>("button") ?? []);
    expect(buttons.map((b) => b.dataset.action)).toEqual([
      "xp-upload",
      "xp-download",
      "xp-download-to",
      "xp-rename",
      "xp-delete",
    ]);
    expect(buttons.map((b) => b.getAttribute("aria-keyshortcuts"))).toEqual([
      null,
      null,
      null,
      "F2",
      "Delete",
    ]);
    // Two rows are selected: both can be copied and deleted, one is renamed.
    expect(buttons.map((b) => b.disabled)).toEqual([false, false, false, true, false]);
    expect(buttons.map((b) => b.tagName)).toEqual(Array(5).fill("BUTTON"));
    expect(buttons[0]?.getAttribute("aria-haspopup")).toBe("menu");
    expect(buttons[0]?.getAttribute("aria-expanded")).toBe("false");
  });

  it("offers nothing to change without a selection, at the drives, or after the session", () => {
    const state = (name: string): boolean[] =>
      Array.from(show(name).querySelectorAll<HTMLButtonElement>(".xp-actions button")).map(
        (b) => b.disabled,
      );
    // Nothing is selected: the open folder still takes an upload.
    expect(state("files-sorted")).toEqual([false, true, true, true, true]);
    expect(state("files-menu")).toEqual([false, false, false, false, false]);
    expect(state("files-drives")).toEqual([true, true, true, true, true]);
    expect(state("files-ended")).toEqual([true, true, true, true, true]);
    // One delete at a time; a copy is still offered.
    expect(state("files-removal")).toEqual([false, false, false, true, true]);
  });

  it("opens the two ways to choose what to send as a menu of the Upload button", () => {
    const root = show("files-upload-menu");
    expect(root.querySelector("#xp-upload")?.getAttribute("aria-expanded")).toBe("true");
    const menu = root.querySelector("#xp-menu");
    expect(menu?.getAttribute("role")).toBe("menu");
    expect(menu?.getAttribute("aria-label")).toBe(`Upload to ${FOLDER}`);
    const items = Array.from(menu?.querySelectorAll<HTMLButtonElement>('[role="menuitem"]') ?? []);
    expect(items.map(text)).toEqual(["Files…", "A folder…"]);
    expect(items.map((item) => item.dataset.action)).toEqual([
      "xp-upload-files",
      "xp-upload-folder",
    ]);
  });
});

describe("the transfer queue", () => {
  it("puts what did not arrive first, then what runs and waits, then what ended", () => {
    const root = show("files-queue");
    expect(queued(root)).toEqual([
      "Photos\\Old laptop: Left out",
      "Budget 2026.xlsx: Not sent",
      "Photos\\2026\\holiday.jpg: 45% · 1.7 MB of 3.8 MB",
      "Photos\\2026\\garden.jpg: Waiting",
      "Photos\\2026\\cat.jpg: Waiting",
      "Tools: Made",
      "drivers\\chipset.zip: Cancelled",
      "report.pdf: Copied · 1.2 MB",
      "setup.exe: Sent · 50.0 MB",
    ]);
    expect(text(root.querySelector("#xp-queue-words"))).toBe(
      "1 running · 2 waiting · 1 failed · 1 left out · 1 cancelled · 3 done",
    );
    expect(root.querySelector("#xp-queue")?.getAttribute("aria-label")).toBe(
      "Transfers of the explorer",
    );
    expect(text(root.querySelector(".xp-transfers h3"))).toBe("Transfers");
  });

  it("says why a transfer did not arrive, in words", () => {
    const root = show("files-queue");
    expect(text(queuedRow(root, "Budget 2026.xlsx").querySelector(".why"))).toBe(
      "The file is in use.",
    );
    expect(text(queuedRow(root, "Photos\\Old laptop").querySelector(".why"))).toBe(
      "A link is not followed.",
    );
    // A transfer that arrived has nothing to explain.
    expect(queuedRow(root, "setup.exe").querySelector(".why")).toBeNull();
  });

  it("shows the progress of the running transfer as a bar that says its number", () => {
    const root = show("files-queue");
    const running = queuedRow(root, "Photos\\2026\\holiday.jpg");
    const bar = running.querySelector("progress");
    expect(bar?.getAttribute("max")).toBe("100");
    expect(bar?.getAttribute("value")).toBe("45");
    expect(bar?.getAttribute("aria-label")).toBe("Photos\\2026\\holiday.jpg: 45%");
    expect(root.querySelectorAll("#xp-queue progress")).toHaveLength(1);
  });

  it("names both places of a transfer, and which way it goes", () => {
    const root = show("files-queue");
    const copied = queuedRow(root, "report.pdf");
    expect(text(copied)).toContain("To this computer · leaves the customer computer");
    expect(text(copied)).toContain(`On the customer computer: ${FOLDER}\\report.pdf`);
    expect(text(copied)).toContain(`On this computer: ${DOWNLOADS}\\report.pdf`);
    expect(copied.querySelector(".k")?.classList.contains("out")).toBe(true);
    const sent = queuedRow(root, "setup.exe");
    expect(text(sent)).toContain("To the customer computer");
    expect(text(sent)).not.toContain("On this computer");
    expect(sent.querySelector(".k")?.classList.contains("out")).toBe(false);
  });

  it("cancels a transfer that waits or runs, and shows the folder of a copy that arrived", () => {
    const root = show("files-queue");
    const actions = (name: string): Array<[string | undefined, string | null]> =>
      Array.from(queuedRow(root, name).querySelectorAll<HTMLButtonElement>("button")).map((b) => [
        b.dataset.action,
        b.getAttribute("aria-label"),
      ]);
    expect(actions("Photos\\2026\\holiday.jpg")).toEqual([
      ["xp-cancel-transfer", "Cancel Photos\\2026\\holiday.jpg"],
    ]);
    expect(actions("Photos\\2026\\garden.jpg")).toEqual([
      ["xp-cancel-transfer", "Cancel Photos\\2026\\garden.jpg"],
    ]);
    expect(actions("report.pdf")).toEqual([["xp-show-folder", "Show report.pdf in its folder"]]);
    expect(
      queuedRow(root, "report.pdf").querySelector<HTMLElement>("button")?.dataset.arg,
    ).toBe("2");
    for (const ended of ["setup.exe", "Budget 2026.xlsx", "drivers\\chipset.zip", "Tools"]) {
      expect(actions(ended), ended).toEqual([]);
    }
    const all = root.querySelector<HTMLButtonElement>('[data-action="xp-cancel-transfers"]');
    expect(text(all)).toBe("Cancel all");
    expect(all?.getAttribute("aria-label")).toBe("Cancel all 3 transfers");
  });

  it("draws a bounded number of rows of thousands of transfers, and counts the rest", () => {
    const root = show("files-queue-many");
    const rows = root.querySelectorAll("#xp-queue > li.q");
    // 5000 transfers: 200 done, one running, 49 failed, 4750 waiting.
    expect(rows.length).toBe(49 + 1 + QUEUE_ROWS.waiting + QUEUE_ROWS.finished);
    const more = Array.from(root.querySelectorAll("#xp-queue > li.more")).map(text);
    expect(more).toEqual([
      "and 4730 more waiting",
      "and 150 earlier ones; the Audit tab lists every transfer",
    ]);
    // What did not arrive first, the last one queued at the top.
    expect(queued(root)[0]).toBe("Drivers\\driver-04947.dll: Not sent");
    expect(queued(root)[49]).toMatch(/^Drivers\\driver-00200\.dll: 0%/);
    expect(queued(root)[50]).toBe("Drivers\\driver-00201.dll: Waiting");
    expect(text(root.querySelector("#xp-queue-words"))).toBe(
      "1 running · 4750 waiting · 49 failed · 200 done",
    );
  });

  it("counts what did not arrive beyond the rows it draws", () => {
    const failed = Array.from({ length: QUEUE_ROWS.attention + 12 }, (_, n) =>
      transfer({ id: n + 1, state: "failed", reason: "Access was denied." }),
    );
    const fixture = FIXTURES["files-details"];
    if (!fixture) throw new Error("no fixture");
    const root = show("files-details", {
      explorer: applyTransfers(fixture.explorer, failed),
    });
    expect(root.querySelectorAll("#xp-queue > li.q")).toHaveLength(QUEUE_ROWS.attention);
    expect(text(root.querySelector("#xp-queue > li.more"))).toBe(
      "and 12 more that did not arrive",
    );
  });

  it("keeps the files the activity knows of besides the queue under it", () => {
    // The AI's transfers have no row in the queue: they stay listed.
    const root = show("files-queue", { activity: FEED });
    const listed = root.querySelectorAll(".xp-transfers .list.files:not(.queue) .it");
    expect(listed).toHaveLength(2);
    // The technician's own transfer that the queue shows is not listed twice.
    const own = show("files-queue", {
      activity: [
        {
          entry: "item",
          item: {
            id: "action-4",
            source: "technician",
            description: "Copied a file to the customer computer",
            kind: "file_received",
            operation: `copy a file to the customer computer at ${FOLDER}\\setup.exe`,
            path: `${FOLDER}\\setup.exe`,
            bytes: 52_428_800,
            state: { state: "done" },
            startedAt: "2026-11-03T14:00:00.000Z",
          },
        },
      ],
    });
    expect(own.querySelectorAll(".xp-transfers .list.files:not(.queue) .it")).toHaveLength(0);
    expect(text(own.querySelector(".xp-transfers"))).not.toContain(
      "No files were sent or received",
    );
  });

  it("writes the names and reasons of a transfer as text, never as markup", () => {
    const root = show("files-queue-xss");
    expect(root.querySelector("#xp-queue img, #xp-queue script")).toBeNull();
    const row = root.querySelector("#xp-queue > li.q");
    expect(text(row?.querySelector("b"))).toBe(XSS);
    expect(text(row?.querySelector(".why"))).toBe(XSS);
    expect(row?.querySelector("progress")).toBeNull();
  });

  it("says that the session stopped the transfers, in the alert", () => {
    const root = show("files-ended-queue");
    const alert = root.querySelector("#xp-alert");
    expect(text(alert?.querySelector("b"))).toBe("Browsing has stopped");
    expect(text(alert)).toContain("2 transfers were stopped and are not complete.");
    expect(queued(root)).toEqual([
      "driver.zip: Stopped with the session",
      "setup.exe: Stopped with the session",
      "notes.txt: Sent · 1.2 KB",
    ]);
    expect(text(queuedRow(root, "setup.exe").querySelector(".why"))).toBe(
      "The session ended before this was complete.",
    );
    expect(root.querySelector('[data-action="xp-cancel-transfers"]')).toBeNull();
  });
});

describe("a name that is taken", () => {
  it("asks in a dialog that a screen reader reads: replace, keep both or skip", () => {
    const root = show("files-conflict");
    const dialog = root.querySelector("#xp-question");
    expect(dialog?.getAttribute("role")).toBe("dialog");
    expect(dialog?.getAttribute("aria-modal")).toBe("true");
    expect(text(root.querySelector(`#${dialog?.getAttribute("aria-labelledby")}`))).toBe(
      "A file named report.pdf is already there",
    );
    const said = text(root.querySelector(`#${dialog?.getAttribute("aria-describedby")}`));
    expect(said).toContain(`In ${FOLDER} on ACME-PC-07.`);
    expect(said).toContain("Replace puts the one you are sending in its place.");
    expect(said).toContain("Keep both gives the new one another name, such as name (2).ext.");
    expect(said).toContain("Name 1 of 3 that is taken.");
    const buttons = Array.from(
      dialog?.querySelectorAll<HTMLButtonElement>(".row.end button") ?? [],
    );
    expect(buttons.map(text)).toEqual(["Cancel the upload", "Skip", "Keep both", "Replace"]);
    expect(buttons.map((b) => b.dataset.arg)).toEqual([undefined, "skip", "keep_both", "replace"]);
    expect(buttons.every((b) => !b.disabled)).toBe(true);
    // The choice that loses nothing is the one the focus goes to.
    expect(buttons[2]?.hasAttribute("data-autofocus")).toBe(true);
    // A button that stays pressed: the next choice then holds for the rest.
    const all = dialog?.querySelector<HTMLButtonElement>("#xp-conflict-all");
    expect(all?.tagName).toBe("BUTTON");
    expect(all?.getAttribute("aria-pressed")).toBe("false");
    expect(text(all)).toBe("Do the same for the next 2 names that are taken");
    expect(all?.dataset.action).toBe("xp-conflict-all");
  });

  it("shows that the choice holds for the names that follow, in words and as a state", () => {
    const fixture = FIXTURES["files-conflict"];
    const question = fixture?.explorer.question;
    if (!fixture || question?.kind !== "conflicts") throw new Error("no fixture");
    const root = show("files-conflict", {
      explorer: { ...fixture.explorer, question: { ...question, at: 1, all: true } },
    });
    const all = root.querySelector("#xp-conflict-all");
    expect(all?.getAttribute("aria-pressed")).toBe("true");
    expect(text(all)).toBe("Do the same for the next name that is taken");
  });

  it("says what Replace means for a folder, and that the choice holds for the rest", () => {
    const root = show("files-conflict-folder");
    expect(text(root.querySelector("#xp-question-title"))).toBe(
      "A folder named Photos is already there",
    );
    const said = text(root.querySelector("#xp-question-text"));
    expect(said).toContain(
      "Replace sends into the folder that is there: files of the same name are replaced, everything else in it stays.",
    );
    expect(said).toContain("such as Photos (2)");
    expect(said).toContain("Name 2 of 2 that is taken.");
    // The last conflict: nothing follows to do the same for.
    expect(root.querySelector("#xp-conflict-all")).toBeNull();
  });

  it("does not offer Replace where the kinds differ, and says why", () => {
    const root = show("files-conflict-kinds");
    expect(text(root.querySelector("#xp-question-title"))).toBe(
      "A folder named Invoices is already there",
    );
    expect(text(root.querySelector("#xp-question-text"))).toContain(
      "Replace is not offered: a file cannot take the place of a folder.",
    );
    const replace = Array.from(root.querySelectorAll<HTMLButtonElement>("#xp-question button")).find(
      (b) => text(b) === "Replace",
    );
    expect(replace?.disabled).toBe(true);
    expect(text(root.querySelector("#xp-question-text"))).not.toContain("that is taken.");
  });

  it("asks the same way for a copy to this computer", () => {
    const root = show("files-conflict-download");
    const said = text(root.querySelector("#xp-question-text"));
    expect(said).toContain(`In ${DOWNLOADS} on this computer.`);
    expect(said).toContain("Replace puts the one you are copying in its place.");
    expect(
      Array.from(root.querySelectorAll("#xp-question button")).map(text)[0],
    ).toBe("Cancel the download");
  });
});

describe("deleting", () => {
  it("asks first, with the number of items and that there is no recycle bin", () => {
    const root = show("files-delete");
    const dialog = root.querySelector("#xp-question");
    expect(dialog?.getAttribute("role")).toBe("alertdialog");
    expect(dialog?.getAttribute("aria-modal")).toBe("true");
    expect(text(root.querySelector(`#${dialog?.getAttribute("aria-labelledby")}`))).toBe(
      "Delete 3 items?",
    );
    const said = text(root.querySelector(`#${dialog?.getAttribute("aria-describedby")}`));
    expect(said).toContain("Invoices, notes.txt, report.pdf.");
    expect(said).toContain("1 folder is deleted with everything in it.");
    expect(said).toContain(
      `There is no recycle bin on this path: what is deleted from ${FOLDER} is gone for good.`,
    );
    const buttons = Array.from(dialog?.querySelectorAll<HTMLButtonElement>("button") ?? []);
    expect(buttons.map(text)).toEqual(["Cancel", "Delete 3 items"]);
    expect(buttons.map((b) => b.dataset.action)).toEqual([
      "xp-question-cancel",
      "xp-delete-confirm",
    ]);
    // The focus goes to Cancel: a key pressed too soon deletes nothing.
    expect(buttons[0]?.hasAttribute("data-autofocus")).toBe(true);
    expect(buttons[1]?.classList.contains("danger")).toBe(true);

    const one = show("files-delete-one");
    expect(text(one.querySelector("#xp-question-title"))).toBe("Delete notes.txt?");
    expect(
      Array.from(one.querySelectorAll("#xp-question button")).map(text),
    ).toEqual(["Cancel", "Delete"]);
  });

  it("shows a delete that runs, with how far it is and a way to cancel it", () => {
    const root = show("files-removal");
    const note = root.querySelector("#xp-removal");
    expect(note?.getAttribute("role")).toBe("status");
    expect(text(note)).toContain(
      "Deleting item 2 of 3: Photos… 1204 files and 31 folders removed so far.",
    );
    const cancel = note?.querySelector<HTMLButtonElement>("button");
    expect(cancel?.dataset.action).toBe("xp-cancel-delete");
    expect(cancel?.getAttribute("aria-label")).toBe("Cancel the delete");
    expect(cancel?.disabled).toBe(false);
    const cancelling = show("files-removal-cancelling");
    expect(text(cancelling.querySelector("#xp-removal"))).toContain("Cancelling the delete…");
    expect(cancelling.querySelector<HTMLButtonElement>("#xp-removal button")?.disabled).toBe(true);
    expect(show("files-details").querySelector("#xp-removal")).toBeNull();
  });

  it("names what could not be removed, each with its reason, in an assertive alert", () => {
    const root = show("files-problem");
    const alert = root.querySelector("#xp-alert");
    expect(alert?.getAttribute("role")).toBe("alert");
    expect(text(alert?.querySelector("b"))).toBe("2 of 3 items could not be deleted");
    expect(text(alert)).toContain("1 of 3 items was deleted (40 files and 3 folders in all).");
    expect(Array.from(alert?.querySelectorAll(".xp-details li") ?? []).map(text)).toEqual([
      `${FOLDER}\\Scans\\open.xlsx — the file is in use`,
      `${FOLDER}\\Scans — the folder is not empty`,
      "and 4 more that could not be removed",
    ]);
    expect(text(alert?.querySelector(".next"))).toContain("The list shows what is there now.");
    expect(buttonNamed(root, "Dismiss")?.dataset.action).toBe("xp-dismiss");
    // The list is still there under it.
    expect(names(root)).toContain("Scans");
  });

  it("writes what could not be done as text, never as markup", () => {
    const root = show("files-problem-xss");
    expect(root.querySelector("#xp-alert img, #xp-alert script")).toBeNull();
    expect(text(root.querySelector("#xp-alert b"))).toBe(XSS);
    expect(text(root.querySelector("#xp-alert .xp-details code"))).toBe(XSS);
  });
});

describe("renaming", () => {
  it("turns the name of the row into a field that says what it is for", () => {
    const root = show("files-renaming");
    const field = root.querySelector<HTMLInputElement>("#xp-rename-input");
    expect(field?.getAttribute("value")).toBe("report.pdf");
    expect(field?.getAttribute("aria-label")).toBe("New name for report.pdf");
    expect(text(root.querySelector(`#${field?.getAttribute("aria-describedby")}`))).toBe(
      "Press Enter to rename, Escape to leave the name as it is.",
    );
    expect(field?.closest("[role='row']")?.getAttribute("data-row")).toBe("report.pdf");
    expect(field?.closest("[role='gridcell']")).not.toBeNull();
    // The other rows keep their names as text.
    expect(root.querySelectorAll("#xp-rows input")).toHaveLength(1);
    expect(show("files-details").querySelector("#xp-rename-hint")).toBeNull();
  });
});

describe("a drop", () => {
  it("says where the files would go while they are dragged over the window", () => {
    const root = show("files-dropping");
    expect(text(root.querySelector("#xp-drop"))).toBe(`Drop to send to ${FOLDER}`);
    expect(root.querySelector("#xp-drop")?.classList.contains("no")).toBe(false);
    const drives = show("files-dropping-drives");
    expect(text(drives.querySelector("#xp-drop"))).toBe(
      "Open a folder first: files are sent to the folder that is open.",
    );
    expect(drives.querySelector("#xp-drop")?.classList.contains("no")).toBe(true);
    expect(show("files-details").querySelector("#xp-drop")).toBeNull();
  });

  it("says in the status line what it is finding out before it asks", () => {
    expect(text(show("files-preparing").querySelector("#xp-status"))).toBe(
      `11 items · looking at ${FOLDER} for names that are taken…`,
    );
  });
});

describe("every state of the second part", () => {
  it("renders, with the list behind it", () => {
    for (const name of Object.keys(EXPLORERS)) {
      const root = show(name);
      expect(root.querySelector("#xp"), name).not.toBeNull();
      expect(root.querySelector(".xp-actions"), name).not.toBeNull();
      expect(root.querySelector("#xp-grid"), name).not.toBeNull();
    }
  });
});

