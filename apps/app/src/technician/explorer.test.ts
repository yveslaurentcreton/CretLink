// The explorer's state and its pure derivations (ST-M010-002): places and
// paths, the order of a folder, the pages of a listing, the selection, the
// keyboard, the part in view, and the honest states.

import { describe, expect, it } from "vitest";
import type {
  DeleteReport,
  ExplorerDrive,
  ExplorerEntry,
  ExplorerEvent,
  TransferCheck,
  TransferEntry,
} from "../shared/contract";
import {
  type ConflictQuestion,
  DRIVES,
  type ExplorerState,
  HISTORY_LIMIT,
  LEFT,
  LIST_COLUMN_WIDTH,
  OVERSCAN,
  type Place,
  QUEUE_ROWS,
  ROW_HEIGHT,
  actionsFor,
  answerConflict,
  applyDrives,
  applyEvent,
  applyFailure,
  applyListing,
  applyRemoval,
  applyTransfers,
  canChange,
  childPath,
  clearSelection,
  compareEntries,
  conflictQuestion,
  conflictsDecided,
  crumbs,
  deleteItems,
  deleteOutcome,
  deleteWords,
  directionWords,
  driveKindWords,
  driveName,
  endedWords,
  folder,
  indexOfKey,
  initialExplorer,
  isChosenFolder,
  isDeleteReport,
  isExplorerDrive,
  isExplorerEntry,
  isExplorerEvent,
  isExplorerListing,
  isOpen,
  isRemovalProgress,
  isTransferCheck,
  isTransferEntry,
  isTransferQueued,
  isTransfersEvent,
  isUploadOffer,
  keyAt,
  lastName,
  layoutOf,
  menuItems,
  mergeEntries,
  modifiedWords,
  moveFocus,
  notOpened,
  openTarget,
  opening,
  parentPlace,
  placeText,
  positionOf,
  queueRestWords,
  queueRows,
  queueWords,
  queuedWords,
  removalOf,
  removalWords,
  renameProblem,
  renameSelection,
  rowCount,
  samePlace,
  selectAll,
  selectByPrefix,
  selectOnly,
  selectTo,
  selectedKeys,
  sizeWords,
  sortEntries,
  statusWords,
  stoppedTransfers,
  toggleSelected,
  transferCounts,
  transferNames,
  transferPercent,
  transferWords,
  typeWords,
  visibleRange,
  withSort,
  withView,
  withViewport,
} from "./explorer";

const T0 = Date.UTC(2026, 10, 3, 14, 30, 0);

function file(name: string, len = 1024, modifiedMs = T0): ExplorerEntry {
  return { name, kind: "file", len, modifiedMs, readonly: false };
}

/** A file whose time the customer computer did not say. */
function untimed(name: string): ExplorerEntry {
  return { name, kind: "file", len: 1, readonly: false };
}

function dir(name: string): ExplorerEntry {
  return { name, kind: "dir", len: 0, modifiedMs: T0, readonly: false };
}

function link(name: string): ExplorerEntry {
  return { name, kind: "symlink", len: 0, modifiedMs: T0, readonly: false };
}

const C: ExplorerDrive = {
  root: "C:\\",
  kind: "fixed",
  label: "Windows",
  totalBytes: 512_000_000_000,
  freeBytes: 128_000_000_000,
};
const E: ExplorerDrive = { root: "E:\\", kind: "removable" };

const DOCUMENTS = "C:\\Users\\ann\\Documents";

function names(s: ExplorerState): string[] {
  return s.entries.map((entry) => entry.name);
}

/** Opens `path` from wherever `s` stands, with these pages. */
function open(
  s: ExplorerState,
  path: string,
  pages: ExplorerEntry[][],
  listing: number,
  options: { how?: "push" | "back" | "forward" | "replace"; select?: string; reveal?: string } = {},
): ExplorerState {
  let next = opening(s, {
    listing,
    place: folder(path),
    how: options.how ?? "push",
    ...(options.reveal ? { reveal: options.reveal } : {}),
  });
  const [first = [], ...rest] = pages;
  next = applyEvent(next, {
    kind: "opened",
    listing,
    path,
    ...(options.select ? { select: options.select } : {}),
    entries: first,
    end: rest.length === 0,
  });
  rest.forEach((entries, index) => {
    next = applyEvent(next, { kind: "page", listing, entries, end: index === rest.length - 1 });
  });
  return next;
}

/** A folder of `count` files, as one page. */
function many(count: number): ExplorerEntry[] {
  return Array.from({ length: count }, (_, n) => file(`file-${String(n).padStart(5, "0")}.txt`));
}

describe("places and paths", () => {
  it("takes a path apart into the parts of the path bar", () => {
    expect(crumbs(folder(DOCUMENTS))).toEqual([
      { label: "C:\\", place: folder("C:\\") },
      { label: "Users", place: folder("C:\\Users") },
      { label: "ann", place: folder("C:\\Users\\ann") },
      { label: "Documents", place: folder(DOCUMENTS) },
    ]);
    expect(crumbs(folder("D:\\"))).toEqual([{ label: "D:\\", place: folder("D:\\") }]);
    expect(crumbs(DRIVES)).toEqual([]);
    expect(crumbs(null)).toEqual([]);
  });

  it("goes up to the folder above, and from a drive's root to the drives", () => {
    expect(parentPlace(folder(DOCUMENTS))).toEqual(folder("C:\\Users\\ann"));
    expect(parentPlace(folder("C:\\Users"))).toEqual(folder("C:\\"));
    expect(parentPlace(folder("C:\\"))).toEqual(DRIVES);
    expect(parentPlace(DRIVES)).toBeNull();
    expect(parentPlace(null)).toBeNull();
  });

  it("names an entry in a folder and in a root", () => {
    expect(childPath("C:\\", "Users")).toBe("C:\\Users");
    expect(childPath("C:\\Users", "ann")).toBe("C:\\Users\\ann");
    expect(lastName(folder(DOCUMENTS))).toBe("Documents");
    expect(lastName(folder("C:\\"))).toBe("C:\\");
    expect(lastName(DRIVES)).toBeUndefined();
  });

  it("spells a place for the path field and compares places", () => {
    expect(placeText(folder(DOCUMENTS))).toBe(DOCUMENTS);
    expect(placeText(DRIVES)).toBe("");
    expect(placeText(null)).toBe("");
    expect(samePlace(folder("C:\\a"), folder("C:\\a"))).toBe(true);
    expect(samePlace(folder("C:\\a"), folder("C:\\A"))).toBe(false);
    expect(samePlace(DRIVES, { kind: "drives" })).toBe(true);
    expect(samePlace(DRIVES, folder("C:\\"))).toBe(false);
    expect(samePlace(null, null)).toBe(true);
    expect(samePlace(null, DRIVES)).toBe(false);
  });
});

describe("words", () => {
  it("writes a time, a type and a size the way the columns show them", () => {
    expect(modifiedWords(T0)).toBe("2026-11-03 14:30");
    expect(modifiedWords(undefined)).toBe("");
    expect(typeWords(dir("Invoices"))).toBe("File folder");
    expect(typeWords(link("Documents and Settings"))).toBe("Link");
    expect(typeWords(file("report.pdf"))).toBe("PDF file");
    expect(typeWords(file("archive.tar.gz"))).toBe("GZ file");
    expect(typeWords(file("README"))).toBe("File");
    expect(typeWords(file(".gitignore"))).toBe("File");
    expect(typeWords(file("notes.final version"))).toBe("File");
    expect(typeWords({ ...file("pipe"), kind: "other" })).toBe("Other");
    expect(sizeWords(file("a", 1_258_291))).toBe("1.2 MB");
    expect(sizeWords(file("empty", 0))).toBe("0 B");
    // A folder and a link have no size to show.
    expect(sizeWords(dir("Invoices"))).toBe("");
    expect(sizeWords(link("x"))).toBe("");
  });

  it("names a drive as Windows does, and its kind in words", () => {
    expect(driveName(C)).toBe("Windows (C:)");
    expect(driveName(E)).toBe("E:");
    expect(driveName({ ...E, label: "  " })).toBe("E:");
    expect(driveKindWords("fixed")).toBe("Local disk");
    expect(driveKindWords("removable")).toBe("Removable drive");
    expect(driveKindWords("network")).toBe("Network drive");
    expect(driveKindWords("optical")).toBe("CD or DVD drive");
    expect(driveKindWords("ram_disk")).toBe("RAM disk");
    expect(driveKindWords("unknown")).toBe("Drive");
  });
});

describe("the order of a folder", () => {
  const mixed = [
    file("zebra.txt", 10),
    dir("Windows"),
    file("file10.txt", 300),
    file("File2.txt", 20),
    dir("apps"),
    file("Apple.txt", 5000),
    link("link"),
    dir("Users"),
    file("file1.txt", 1),
  ];

  it("puts folders first and sorts names without regard to case, digits by value", () => {
    expect(sortEntries(mixed, { key: "name", descending: false }).map((e) => e.name)).toEqual([
      "apps",
      "Users",
      "Windows",
      "Apple.txt",
      "file1.txt",
      "File2.txt",
      "file10.txt",
      "link",
      "zebra.txt",
    ]);
  });

  it("keeps folders first when the order is turned around", () => {
    expect(sortEntries(mixed, { key: "name", descending: true }).map((e) => e.name)).toEqual([
      "Windows",
      "Users",
      "apps",
      "zebra.txt",
      "link",
      "file10.txt",
      "File2.txt",
      "file1.txt",
      "Apple.txt",
    ]);
  });

  it("sorts by size, by time and by type, with the name to tell equals apart", () => {
    const bySize = sortEntries(mixed, { key: "size", descending: true }).map((e) => e.name);
    expect(bySize.slice(0, 3)).toEqual(["Windows", "Users", "apps"]);
    expect(bySize.slice(3, 6)).toEqual(["Apple.txt", "file10.txt", "File2.txt"]);

    const timed = [
      file("old.txt", 1, T0 - 5000),
      file("new.txt", 1, T0),
      untimed("unknown.txt"),
      file("also-new.txt", 1, T0),
    ];
    expect(sortEntries(timed, { key: "modified", descending: true }).map((e) => e.name)).toEqual([
      "new.txt",
      "also-new.txt",
      "old.txt",
      "unknown.txt",
    ]);
    // An entry without a time comes last in either direction.
    expect(sortEntries(timed, { key: "modified", descending: false }).map((e) => e.name)).toEqual([
      "old.txt",
      "also-new.txt",
      "new.txt",
      "unknown.txt",
    ]);

    const typed = [file("b.txt"), file("a.pdf"), file("c.txt"), file("README"), dir("d")];
    expect(sortEntries(typed, { key: "type", descending: false }).map((e) => e.name)).toEqual([
      "d",
      "README",
      "a.pdf",
      "b.txt",
      "c.txt",
    ]);
  });

  it("tells two names apart that differ only in case", () => {
    const compare = compareEntries({ key: "name", descending: false });
    expect(compare(file("Readme"), file("readme"))).not.toBe(0);
    expect(compare(file("readme"), file("readme"))).toBe(0);
  });

  it("sorts a page into what is listed exactly as sorting everything would", () => {
    const all = Array.from({ length: 700 }, (_, n) =>
      n % 9 === 0
        ? dir(`Folder ${(n * 37) % 700}`)
        : file(`${n % 2 ? "Report" : "report"}-${(n * 7919) % 700}.txt`, (n * 31) % 5000, T0 + n),
    );
    for (const sort of [
      { key: "name", descending: false },
      { key: "size", descending: true },
      { key: "modified", descending: false },
      { key: "type", descending: true },
    ] as const) {
      let listed: ExplorerEntry[] = [];
      for (let from = 0; from < all.length; from += 256) {
        const page = all.slice(from, from + 256);
        const merged = mergeEntries(listed, page, sort);
        expect(merged.before).toHaveLength(page.length);
        // Each place is a place in the list before the page, in rising order.
        for (let i = 0; i < merged.before.length; i += 1) {
          const at = merged.before[i] as number;
          expect(at).toBeGreaterThanOrEqual(i === 0 ? 0 : (merged.before[i - 1] as number));
          expect(at).toBeLessThanOrEqual(listed.length);
        }
        listed = merged.entries;
      }
      expect(listed.map((e) => e.name)).toEqual(sortEntries(all, sort).map((e) => e.name));
    }
  });

  it("leaves the list alone for an empty page", () => {
    const listed = sortEntries([file("a"), file("b")], { key: "name", descending: false });
    const merged = mergeEntries(listed, [], { key: "name", descending: false });
    expect(merged.entries).toEqual(listed);
    expect(merged.before).toEqual([]);
  });
});

describe("the part in view", () => {
  const large = open(initialExplorer(), DOCUMENTS, [many(5000)], 1);

  it("draws only the rows in view and a few beyond, of thousands", () => {
    expect(rowCount(large)).toBe(5000);
    const top = visibleRange(large);
    expect(top).toEqual({ first: 0, last: Math.ceil(480 / ROW_HEIGHT) + OVERSCAN });
    expect(top.last - top.first).toBeLessThan(40);

    const middle = visibleRange(withViewport(large, { top: 2500 * ROW_HEIGHT }));
    expect(middle.first).toBe(2500 - OVERSCAN);
    expect(middle.last).toBe(2500 + Math.ceil(480 / ROW_HEIGHT) + OVERSCAN);

    const end = visibleRange(withViewport(large, { top: 5000 * ROW_HEIGHT - 480 }));
    expect(end.last).toBe(5000);
    expect(end.last - end.first).toBeLessThan(40);
    // A list that got shorter under a scroll position draws nothing wrong.
    const beyond = visibleRange(withViewport(large, { top: 9000 * ROW_HEIGHT }));
    expect(beyond).toEqual({ first: 5000, last: 5000 });
  });

  it("gives the whole list its height, so the bar shows where the part in view is", () => {
    expect(layoutOf(large).height).toBe(5000 * ROW_HEIGHT);
    expect(positionOf(large, 0)).toEqual({ left: 0, top: 0 });
    expect(positionOf(large, 4999)).toEqual({ left: 0, top: 4999 * ROW_HEIGHT });
  });

  it("runs the List view down a column and on in the next, and draws the columns in view", () => {
    const list = withView(large, "list");
    const perColumn = Math.floor(480 / ROW_HEIGHT);
    const layout = layoutOf(list);
    expect(layout.rowsPerColumn).toBe(perColumn);
    expect(layout.columns).toBe(Math.ceil(5000 / perColumn));
    expect(layout.width).toBe(layout.columns * LIST_COLUMN_WIDTH);
    expect(layout.height).toBe(perColumn * ROW_HEIGHT);
    expect(positionOf(list, 0)).toEqual({ left: 0, top: 0 });
    expect(positionOf(list, perColumn - 1)).toEqual({ left: 0, top: (perColumn - 1) * ROW_HEIGHT });
    expect(positionOf(list, perColumn)).toEqual({ left: LIST_COLUMN_WIDTH, top: 0 });

    // 800 pixels show three columns and a part of the fourth; one more is drawn.
    expect(visibleRange(list)).toEqual({ first: 0, last: 5 * perColumn });
    const scrolled = visibleRange(withViewport(list, { left: 20 * LIST_COLUMN_WIDTH }));
    expect(scrolled.first).toBe(19 * perColumn);
    expect(scrolled.last).toBe(25 * perColumn);
    // A short list is one column that is no taller than its rows.
    const short = withView(open(initialExplorer(), DOCUMENTS, [many(3)], 1), "list");
    expect(layoutOf(short)).toMatchObject({ columns: 1, height: 3 * ROW_HEIGHT });
  });

  it("takes what the technician scrolled and resized, and nothing when nothing changed", () => {
    const same = withViewport(large, { top: 0, height: 480 });
    expect(same).toBe(large);
    const resized = withViewport(large, { height: 900, width: 1200 });
    expect(resized.viewport).toEqual({ top: 0, left: 0, height: 900, width: 1200 });
    // Scrolling by hand is not a move of the model's own.
    expect(resized.scrollSeq).toBe(large.scrollSeq);
  });
});

describe("selection", () => {
  const s = open(
    initialExplorer(),
    DOCUMENTS,
    [[dir("Invoices"), file("a.txt"), file("b.txt"), file("c.txt"), file("d.txt")]],
    1,
  );

  it("selects one row with a click", () => {
    const one = selectOnly(s, "b.txt");
    expect([...one.selected]).toEqual(["b.txt"]);
    expect(one.focus).toBe("b.txt");
    expect(one.anchor).toBe("b.txt");
    expect([...selectOnly(one, "d.txt").selected]).toEqual(["d.txt"]);
  });

  it("adds and removes a row with Ctrl", () => {
    let next = toggleSelected(selectOnly(s, "a.txt"), "c.txt");
    expect([...next.selected].sort()).toEqual(["a.txt", "c.txt"]);
    expect(next.focus).toBe("c.txt");
    next = toggleSelected(next, "a.txt");
    expect([...next.selected]).toEqual(["c.txt"]);
    expect(next.anchor).toBe("a.txt");
  });

  it("selects a range with Shift, from where the selection started", () => {
    const from = selectOnly(s, "a.txt");
    const down = selectTo(from, "c.txt");
    expect([...down.selected]).toEqual(["a.txt", "b.txt", "c.txt"]);
    expect(down.focus).toBe("c.txt");
    expect(down.anchor).toBe("a.txt");
    // Another Shift click turns the range around its start, it does not add to it.
    const up = selectTo(down, "Invoices");
    expect([...up.selected]).toEqual(["Invoices", "a.txt"]);
    // Without a start the row itself is the range.
    expect([...selectTo(s, "d.txt").selected]).toEqual(["d.txt"]);
    // A row that is not listed selects nothing.
    expect(selectTo(from, "gone.txt")).toBe(from);
  });

  it("selects every row with Ctrl+A and none after a click beside them", () => {
    const all = selectAll(selectOnly(s, "b.txt"));
    expect(all.selected.size).toBe(5);
    expect(all.focus).toBe("b.txt");
    const none = clearSelection(all);
    expect(none.selected.size).toBe(0);
    expect(clearSelection(none)).toBe(none);
  });

  it("selects by name, so sorting moves no selection", () => {
    const picked = toggleSelected(selectOnly(s, "a.txt"), "Invoices");
    const turned = withSort(picked, "name");
    expect(names(turned)).toEqual(["Invoices", "d.txt", "c.txt", "b.txt", "a.txt"]);
    expect([...turned.selected].sort()).toEqual(["Invoices", "a.txt"]);
    expect(turned.focus).toBe("Invoices");
  });

  it("finds rows by key and says what a row opens", () => {
    expect(keyAt(s, 0)).toBe("Invoices");
    expect(indexOfKey(s, "c.txt")).toBe(3);
    expect(indexOfKey(s, "nowhere")).toBe(-1);
    expect(indexOfKey(s, undefined)).toBe(-1);
    expect(openTarget(s, "Invoices")).toEqual(folder(`${DOCUMENTS}\\Invoices`));
    // A file opens nothing here, and neither does a row that is not listed.
    expect(openTarget(s, "a.txt")).toBeNull();
    expect(openTarget(s, "gone")).toBeNull();
    expect(openTarget(s, undefined)).toBeNull();
    // A link is shown and never followed.
    const linked = open(initialExplorer(), "C:\\", [[link("Documents and Settings")]], 1);
    expect(openTarget(linked, "Documents and Settings")).toBeNull();
  });

  it("says why a row does not open, and takes the words away with the next step", () => {
    expect(notOpened(s, "Invoices")).toBeUndefined();
    expect(notOpened(s, "gone")).toBeUndefined();
    expect(notOpened(s, undefined)).toBeUndefined();
    expect(notOpened(s, "a.txt")).toBe("a.txt is a file. Folders open here, a file does not.");
    const linked = open(initialExplorer(), "C:\\", [[link("Documents and Settings")]], 1);
    expect(notOpened(linked, "Documents and Settings")).toBe(
      "Documents and Settings is a link. A link is shown here, not followed.",
    );
    const told: ExplorerState = { ...selectOnly(s, "a.txt"), hint: notOpened(s, "a.txt") };
    expect(statusWords(told)).toBe(
      "5 items · 1 selected · a.txt is a file. Folders open here, a file does not.",
    );
    expect(selectOnly(told, "b.txt").hint).toBeUndefined();
    expect(moveFocus(told, "down").hint).toBeUndefined();
    expect(clearSelection(told).hint).toBeUndefined();
    expect(opening(told, { listing: 9, place: DRIVES, how: "push" }).hint).toBeUndefined();
  });
});

describe("the keyboard", () => {
  const s = open(initialExplorer(), DOCUMENTS, [many(100)], 1);
  const name = (n: number): string => `file-${String(n).padStart(5, "0")}.txt`;

  it("starts at the first row, or at the last when going up", () => {
    expect(moveFocus(s, "down").focus).toBe(name(0));
    expect(moveFocus(s, "home").focus).toBe(name(0));
    expect(moveFocus(s, "up").focus).toBe(name(99));
    expect(moveFocus(s, "end").focus).toBe(name(99));
    // An empty folder has no row to go to.
    const empty = open(initialExplorer(), DOCUMENTS, [[]], 1);
    expect(moveFocus(empty, "down")).toBe(empty);
  });

  it("moves the selection with the focus, and stops at both ends", () => {
    let next = moveFocus(moveFocus(s, "down"), "down");
    expect(next.focus).toBe(name(1));
    expect([...next.selected]).toEqual([name(1)]);
    next = moveFocus(next, "up");
    next = moveFocus(next, "up");
    expect(next.focus).toBe(name(0));
    next = moveFocus(next, "end");
    expect(moveFocus(next, "down").focus).toBe(name(99));
  });

  it("pages by what fits in view", () => {
    const page = Math.floor(480 / ROW_HEIGHT) - 1;
    const next = moveFocus(moveFocus(s, "home"), "pagedown");
    expect(next.focus).toBe(name(page));
    expect(moveFocus(next, "pageup").focus).toBe(name(0));
  });

  it("extends the selection with Shift and keeps it with Ctrl", () => {
    let next = moveFocus(s, "down");
    next = moveFocus(next, "down", "extend");
    next = moveFocus(next, "down", "extend");
    expect([...next.selected]).toEqual([name(0), name(1), name(2)]);
    next = moveFocus(next, "up", "extend");
    expect([...next.selected]).toEqual([name(0), name(1)]);
    const kept = moveFocus(next, "end", "keep");
    expect(kept.focus).toBe(name(99));
    expect([...kept.selected]).toEqual([name(0), name(1)]);
  });

  it("brings the row in focus into view, and says that it moved the list", () => {
    const end = moveFocus(s, "end");
    expect(end.viewport.top).toBe(100 * ROW_HEIGHT - 480);
    expect(end.scrollSeq).toBe(s.scrollSeq + 1);
    const home = moveFocus(end, "home");
    expect(home.viewport.top).toBe(0);
    // A row that is in view already moves nothing.
    const still = moveFocus(home, "down");
    expect(still.viewport.top).toBe(0);
    expect(still.scrollSeq).toBe(home.scrollSeq);
  });

  it("steps a column to the side in the List view", () => {
    const list = withView(s, "list");
    const perColumn = layoutOf(list).rowsPerColumn;
    let next = moveFocus(moveFocus(list, "home"), "right");
    expect(next.focus).toBe(name(perColumn));
    next = moveFocus(next, "down");
    expect(next.focus).toBe(name(perColumn + 1));
    expect(moveFocus(next, "left").focus).toBe(name(1));
    // The last column comes into view when the focus goes there.
    const end = moveFocus(list, "end");
    expect(end.viewport.left).toBeGreaterThan(0);
    expect(visibleRange(end).last).toBe(100);
    // In Details a step to the side goes nowhere.
    expect(moveFocus(moveFocus(s, "down"), "right").focus).toBe(name(0));
  });

  it("goes to the first row that starts with what was typed", () => {
    const s2 = open(
      initialExplorer(),
      DOCUMENTS,
      [[dir("Invoices"), file("notes.txt"), file("Report.pdf"), file("readme.md")]],
      1,
    );
    expect(selectByPrefix(s2, "r").focus).toBe("readme.md");
    expect(selectByPrefix(s2, "REP").focus).toBe("Report.pdf");
    expect(selectByPrefix(s2, "zz")).toBe(s2);
    expect(selectByPrefix(s2, "")).toBe(s2);
    // A drive answers to its label and to its letter.
    const drives = applyDrives(opening(initialExplorer(), { listing: 1, place: DRIVES, how: "push" }), 1, [C, E]);
    expect(selectByPrefix(drives, "win").focus).toBe("C:\\");
    expect(selectByPrefix(drives, "e").focus).toBe("E:\\");
  });
});

describe("view and sort", () => {
  const s = open(
    initialExplorer(),
    DOCUMENTS,
    [[dir("Invoices"), file("a.txt", 30, T0), file("b.txt", 10, T0 + 1), file("c.txt", 20, T0 - 1)]],
    1,
  );

  it("sorts by a column on the first click and turns the order on the second", () => {
    expect(s.sort).toEqual({ key: "name", descending: false });
    const size = withSort(s, "size");
    // The largest and the newest come first on the first click.
    expect(size.sort).toEqual({ key: "size", descending: true });
    expect(names(size)).toEqual(["Invoices", "a.txt", "c.txt", "b.txt"]);
    expect(names(withSort(size, "size"))).toEqual(["Invoices", "b.txt", "c.txt", "a.txt"]);
    expect(withSort(s, "modified").sort).toEqual({ key: "modified", descending: true });
    expect(withSort(s, "type").sort).toEqual({ key: "type", descending: false });
    expect(withSort(withSort(s, "name"), "name").sort).toEqual({ key: "name", descending: false });
  });

  it("keeps the row in focus in view when the order changes", () => {
    const large = selectOnly(open(initialExplorer(), DOCUMENTS, [many(1000)], 1), "file-00003.txt");
    const turned = withSort(large, "name");
    expect(indexOfKey(turned, "file-00003.txt")).toBe(996);
    const { first, last } = visibleRange(turned);
    expect(first).toBeLessThanOrEqual(996);
    expect(last).toBeGreaterThan(996);
    // Without a row in focus the list starts at its top again.
    const scrolled = withViewport(open(initialExplorer(), DOCUMENTS, [many(1000)], 1), { top: 5000 });
    expect(withSort(scrolled, "size").viewport.top).toBe(0);
  });

  it("changes the view and starts it at its beginning, with the focus in view", () => {
    const list = withView(s, "list");
    expect(list.view).toBe("list");
    expect(withView(list, "list")).toBe(list);
    const large = moveFocus(open(initialExplorer(), DOCUMENTS, [many(1000)], 1), "end");
    const asList = withView(large, "list");
    expect(asList.viewport.top).toBe(0);
    expect(visibleRange(asList).last).toBe(1000);
  });
});

describe("going somewhere", () => {
  it("stays where it was until the first page of the next place has arrived", () => {
    const here = open(initialExplorer(), "C:\\Users", [[dir("ann"), dir("jdoe")]], 1);
    const next = "C:\\Users\\ann";
    const waiting = opening(here, { listing: 2, place: folder(next), how: "push" });
    expect(waiting.place).toEqual(folder("C:\\Users"));
    expect(names(waiting)).toEqual(["ann", "jdoe"]);
    expect(waiting.pending?.place).toEqual(folder(next));
    expect(statusWords(waiting)).toBe(`2 items · opening ${next}…`);

    const there = applyEvent(waiting, {
      kind: "opened",
      listing: 2,
      path: "C:\\Users\\ann",
      entries: [dir("Documents")],
      end: true,
    });
    expect(there.place).toEqual(folder("C:\\Users\\ann"));
    expect(names(there)).toEqual(["Documents"]);
    expect(there.pending).toBeUndefined();
    expect(there.back).toEqual([folder("C:\\Users")]);
    expect(there.content).toEqual({ state: "complete" });
  });

  it("takes the place as the customer computer names it, not as it was asked for", () => {
    const s = opening(initialExplorer(), { listing: 1, place: folder("c:/users/ANN"), how: "push" });
    const there = applyEvent(s, {
      kind: "opened",
      listing: 1,
      path: "C:\\Users\\ann",
      entries: [],
      end: true,
    });
    expect(there.place).toEqual(folder("C:\\Users\\ann"));
  });

  it("starts a new place with nothing selected, at its top", () => {
    let s = open(initialExplorer(), "C:\\Users", [many(500)], 1);
    s = moveFocus(s, "end");
    expect(s.viewport.top).toBeGreaterThan(0);
    const next = open(s, "C:\\Users\\ann", [[dir("Documents")]], 2);
    expect(next.selected.size).toBe(0);
    expect(next.focus).toBeUndefined();
    expect(next.viewport.top).toBe(0);
    expect(next.scrollSeq).toBeGreaterThan(s.scrollSeq);
  });

  it("keeps the places it came from, to go back and forward", () => {
    let s = open(initialExplorer(), "C:\\", [[dir("Users")]], 1);
    s = open(s, "C:\\Users", [[dir("ann")]], 2);
    s = open(s, "C:\\Users\\ann", [[]], 3);
    expect(s.back).toEqual([folder("C:\\"), folder("C:\\Users")]);
    expect(s.forward).toEqual([]);

    s = open(s, "C:\\Users", [[dir("ann")]], 4, { how: "back" });
    expect(s.back).toEqual([folder("C:\\")]);
    expect(s.forward).toEqual([folder("C:\\Users\\ann")]);

    s = open(s, "C:\\Users\\ann", [[]], 5, { how: "forward" });
    expect(s.back).toEqual([folder("C:\\"), folder("C:\\Users")]);
    expect(s.forward).toEqual([]);

    // Going somewhere new forgets where forward led.
    s = open(s, "C:\\Users", [[dir("ann")]], 6, { how: "back" });
    s = open(s, "C:\\Windows", [[]], 7);
    expect(s.forward).toEqual([]);
    expect(s.back).toEqual([folder("C:\\"), folder("C:\\Users")]);
  });

  it("bounds the history", () => {
    let s = initialExplorer();
    for (let n = 0; n < HISTORY_LIMIT + 20; n += 1) {
      s = open(s, `C:\\folder-${n}`, [[]], n + 1);
    }
    expect(s.back).toHaveLength(HISTORY_LIMIT);
    expect(s.back[0]).toEqual(folder("C:\\folder-19"));
  });

  it("selects the folder it came out of when going up, once that folder is listed", () => {
    let s = open(initialExplorer(), "C:\\Users\\ann", [[dir("Documents")]], 1);
    s = opening(s, { listing: 2, place: folder("C:\\Users"), how: "push", reveal: "ann" });
    s = applyEvent(s, {
      kind: "opened",
      listing: 2,
      path: "C:\\Users",
      entries: many(300),
      end: false,
    });
    // Not listed yet: nothing is selected in the meantime.
    expect(s.selected.size).toBe(0);
    expect(s.reveal).toBe("ann");
    s = applyEvent(s, { kind: "page", listing: 2, entries: [dir("ann"), dir("jdoe")], end: true });
    expect([...s.selected]).toEqual(["ann"]);
    expect(s.focus).toBe("ann");
    expect(s.reveal).toBeUndefined();

    // A folder that is no longer there is not selected, and not waited for.
    let gone = open(initialExplorer(), "C:\\Users\\ann", [[]], 1);
    gone = open(gone, "C:\\Users", [[dir("jdoe")]], 2, { reveal: "ann" });
    expect(gone.selected.size).toBe(0);
    expect(gone.reveal).toBeUndefined();
  });

  it("selects the file a path named and brings it into view", () => {
    const s = open(initialExplorer(), DOCUMENTS, [many(256), [file("report.pdf")]], 1, {
      select: "report.pdf",
    });
    expect([...s.selected]).toEqual(["report.pdf"]);
    const at = indexOfKey(s, "report.pdf");
    const { first, last } = visibleRange(s);
    expect(at).toBeGreaterThanOrEqual(first);
    expect(at).toBeLessThan(last);
  });

  it("ignores the pages of a place that was left", () => {
    let s = opening(initialExplorer(), { listing: 1, place: folder("C:\\Windows"), how: "push" });
    s = opening(s, { listing: 2, place: folder("C:\\Users"), how: "push" });
    // The first place opens late: it is no longer what was asked for.
    const late: ExplorerEvent = {
      kind: "opened",
      listing: 1,
      path: "C:\\Windows",
      entries: [dir("System32")],
      end: false,
    };
    expect(applyEvent(s, late)).toBe(s);
    s = applyEvent(s, { kind: "opened", listing: 2, path: "C:\\Users", entries: [dir("ann")], end: true });
    const stale: ExplorerEvent = { kind: "page", listing: 1, entries: [file("x.dll")], end: true };
    expect(applyEvent(s, stale)).toBe(s);
    expect(names(s)).toEqual(["ann"]);
    expect(applyListing(s, { listing: 1, entries: 1, state: "cancelled" })).toBe(s);
    expect(applyFailure(s, 1, { stage: "access_denied", message: "access was denied" })).toBe(s);
  });
});

describe("a folder that arrives in pages", () => {
  const firstPage = [file("Zebra.txt"), dir("Windows"), file("apple.txt")];
  const secondPage = [dir("apps"), file("Mango.txt"), dir("Users")];

  it("shows the first page at once and sorts each further page in", () => {
    let s = opening(initialExplorer(), { listing: 1, place: folder("C:\\"), how: "push" });
    s = applyEvent(s, { kind: "opened", listing: 1, path: "C:\\", entries: firstPage, end: false });
    expect(names(s)).toEqual(["Windows", "apple.txt", "Zebra.txt"]);
    expect(s.content).toEqual({ state: "loading" });
    expect(statusWords(s)).toBe("3 items so far · loading more…");
    s = applyEvent(s, { kind: "page", listing: 1, entries: secondPage, end: true });
    expect(names(s)).toEqual(["apps", "Users", "Windows", "apple.txt", "Mango.txt", "Zebra.txt"]);
    expect(s.content).toEqual({ state: "complete" });
    expect(statusWords(s)).toBe("6 items");
  });

  it("re-sorts by another column as more arrives", () => {
    let s = opening(initialExplorer(), { listing: 1, place: folder(DOCUMENTS), how: "push" });
    s = applyEvent(s, {
      kind: "opened",
      listing: 1,
      path: DOCUMENTS,
      entries: [file("a.txt", 10), file("b.txt", 500)],
      end: false,
    });
    s = withSort(s, "size");
    expect(names(s)).toEqual(["b.txt", "a.txt"]);
    s = applyEvent(s, {
      kind: "page",
      listing: 1,
      entries: [file("c.txt", 100), file("d.txt", 9000)],
      end: true,
    });
    expect(names(s)).toEqual(["d.txt", "b.txt", "c.txt", "a.txt"]);
  });

  it("fills an untouched list from its top", () => {
    let s = opening(initialExplorer(), { listing: 1, place: folder("C:\\"), how: "push" });
    s = applyEvent(s, { kind: "opened", listing: 1, path: "C:\\", entries: firstPage, end: false });
    const before = s.scrollSeq;
    s = applyEvent(s, { kind: "page", listing: 1, entries: secondPage, end: true });
    expect(s.viewport.top).toBe(0);
    expect(s.scrollSeq).toBe(before);
  });

  it("holds the first row in view where it is once the technician has scrolled", () => {
    let s = opening(initialExplorer(), { listing: 1, place: folder(DOCUMENTS), how: "push" });
    s = applyEvent(s, { kind: "opened", listing: 1, path: DOCUMENTS, entries: many(256), end: false });
    s = withViewport(s, { top: 100 * ROW_HEIGHT });
    const first = keyAt(s, 100);
    const before = s.scrollSeq;
    // Three folders sort in above every file, two files after them all.
    s = applyEvent(s, {
      kind: "page",
      listing: 1,
      entries: [dir("Alpha"), dir("Beta"), dir("Gamma"), file("zz-1.txt"), file("zz-2.txt")],
      end: true,
    });
    expect(keyAt(s, Math.floor(s.viewport.top / ROW_HEIGHT))).toBe(first);
    expect(s.viewport.top).toBe(103 * ROW_HEIGHT);
    expect(s.scrollSeq).toBe(before + 1);
  });

  it("holds the row in focus where it is, and keeps the selection on its entries", () => {
    let s = opening(initialExplorer(), { listing: 1, place: folder(DOCUMENTS), how: "push" });
    s = applyEvent(s, { kind: "opened", listing: 1, path: DOCUMENTS, entries: many(256), end: false });
    s = selectOnly(s, "file-00002.txt");
    s = toggleSelected(s, "file-00004.txt");
    const offset = indexOfKey(s, "file-00004.txt") * ROW_HEIGHT - s.viewport.top;
    s = applyEvent(s, {
      kind: "page",
      listing: 1,
      entries: [dir("Alpha"), dir("Beta"), file("file-00003a.txt")],
      end: true,
    });
    expect([...s.selected].sort()).toEqual(["file-00002.txt", "file-00004.txt"]);
    expect(s.focus).toBe("file-00004.txt");
    // Three rows went in above it, and it is still at the same height.
    expect(indexOfKey(s, "file-00004.txt")).toBe(7);
    expect(indexOfKey(s, "file-00004.txt") * ROW_HEIGHT - s.viewport.top).toBe(offset);
  });

  it("shows an empty folder as empty only when its listing is complete", () => {
    let s = opening(initialExplorer(), { listing: 1, place: folder("D:\\"), how: "push" });
    s = applyEvent(s, { kind: "opened", listing: 1, path: "D:\\", entries: [], end: true });
    expect(rowCount(s)).toBe(0);
    expect(s.content).toEqual({ state: "complete" });
    expect(statusWords(s)).toBe("0 items");
  });
});

describe("a listing that is not the whole folder", () => {
  function loading(): ExplorerState {
    let s = opening(initialExplorer(), { listing: 1, place: folder(DOCUMENTS), how: "push" });
    s = applyEvent(s, { kind: "opened", listing: 1, path: DOCUMENTS, entries: many(256), end: false });
    return s;
  }

  it("says that the folder holds more when the listing stopped at its bound", () => {
    let s = applyListing(loading(), { listing: 1, path: DOCUMENTS, entries: 256, state: "limit" });
    expect(s.content).toEqual({ state: "limit" });
    expect(statusWords(s)).toBe("The first 256 items · this folder holds more than are listed");
    // A page that was on its way arrives after the answer: the end stands.
    s = applyEvent(s, { kind: "page", listing: 1, entries: [file("late.txt")], end: false });
    expect(rowCount(s)).toBe(257);
    expect(s.content).toEqual({ state: "limit" });
  });

  it("says that the list is not complete when its session was lost", () => {
    const lost = applyFailure(loading(), 1, {
      stage: "disconnected",
      message: "the connection to the customer computer was lost before it answered",
    });
    expect(lost.content.state).toBe("interrupted");
    expect(lost.ended).toBe("The connection to the customer computer was lost. Browsing has stopped.");
    expect(statusWords(lost)).toBe("256 items listed · the list is not complete");
    expect(rowCount(lost)).toBe(256);

    const stopped = applyFailure(loading(), 1, {
      stage: "no_consented_session",
      message: "no session is under full control",
    });
    expect(stopped.ended).toBe("The session has ended. Browsing has stopped.");
    expect(stopped.content.state).toBe("interrupted");
    expect(stopped.refusal).toBeUndefined();
  });

  it("gives the reason when the customer computer stopped answering a listing", () => {
    const cut = applyFailure(loading(), 1, { stage: "access_denied", message: "access was denied" });
    expect(cut.content).toEqual({
      state: "interrupted",
      reason: "access was denied (access_denied)",
    });
    expect(cut.ended).toBeUndefined();
    // A listing that had ended is not cut short afterwards.
    const done = applyEvent(loading(), { kind: "page", listing: 1, entries: [], end: true });
    expect(applyFailure(done, 1, { stage: "unexpected", message: "x" })).toBe(done);
  });

  it("says so when a listing was left for a place that then did not open", () => {
    let s = opening(loading(), { listing: 2, place: folder("C:\\Secret"), how: "push" });
    s = applyListing(s, { listing: 1, path: DOCUMENTS, entries: 256, state: "cancelled" });
    expect(s.content).toEqual({ state: "interrupted", reason: LEFT });
    s = applyFailure(s, 2, { stage: "access_denied", message: "access was denied" });
    expect(s.place).toEqual(folder(DOCUMENTS));
    expect(rowCount(s)).toBe(256);
    expect(s.refusal?.title).toBe("C:\\Secret could not be opened");
    // A listing that was complete when it was "left" stays complete.
    const complete = applyEvent(loading(), { kind: "page", listing: 1, entries: [], end: true });
    expect(
      applyListing(complete, { listing: 1, path: DOCUMENTS, entries: 256, state: "cancelled" }),
    ).toBe(complete);
  });

  it("leaves a complete answer to the last page", () => {
    const s = loading();
    expect(applyListing(s, { listing: 1, path: DOCUMENTS, entries: 300, state: "complete" })).toBe(s);
  });
});

describe("a place that is refused", () => {
  const here = open(initialExplorer(), "C:\\Users", [[dir("ann"), dir("jdoe")]], 1);

  it("stays where it was and says the refusal in the application's words", () => {
    const waiting = opening(here, { listing: 2, place: folder("C:\\Users\\jdoe"), how: "push" });
    const refused = applyFailure(waiting, 2, { stage: "access_denied", message: "access was denied" });
    expect(refused.place).toEqual(folder("C:\\Users"));
    expect(names(refused)).toEqual(["ann", "jdoe"]);
    expect(refused.pending).toBeUndefined();
    expect(refused.back).toEqual([]);
    expect(refused.refusal).toEqual({
      title: "C:\\Users\\jdoe could not be opened",
      message: "access was denied (access_denied)",
      next: "You are still in C:\\Users. Check the path, or open the folder step by step from the drives.",
    });
    expect(refused.ended).toBeUndefined();
  });

  it("takes the refusal away once another place is open", () => {
    let s = opening(here, { listing: 2, place: folder("C:\\Nowhere"), how: "push" });
    s = applyFailure(s, 2, { stage: "path_not_found", message: "C:\\ lists no entry named Nowhere" });
    expect(s.refusal).toBeDefined();
    s = open(s, "C:\\Users\\ann", [[]], 3);
    expect(s.refusal).toBeUndefined();
  });

  it("says that nothing is open when the first place of all is refused", () => {
    const s = opening(initialExplorer(), { listing: 1, place: DRIVES, how: "push" });
    const refused = applyFailure(s, 1, { stage: "unsupported", message: "not supported on this computer" });
    expect(refused.place).toBeNull();
    expect(refused.refusal).toEqual({
      title: "The drives could not be listed",
      message: "not supported on this computer (unsupported)",
      next: "Nothing is open. Try again, or open another place. Press Refresh to try again.",
    });
  });

  it("says that the session ended instead of refusing, when that is why", () => {
    const waiting = opening(here, { listing: 2, place: folder("C:\\Users\\ann"), how: "push" });
    const ended = applyFailure(waiting, 2, {
      stage: "no_consented_session",
      message: "no session is under full control",
    });
    expect(ended.ended).toBe("The session has ended. Browsing has stopped.");
    expect(ended.refusal).toBeUndefined();
    expect(ended.pending).toBeUndefined();
    expect(ended.place).toEqual(folder("C:\\Users"));
  });

  it("keeps the history as it was when going back is refused", () => {
    let s = open(here, "C:\\Users\\ann", [[]], 2);
    s = opening(s, { listing: 3, place: folder("C:\\Users"), how: "back" });
    s = applyFailure(s, 3, { stage: "path_not_found", message: "C:\\ lists no entry named Users" });
    expect(s.place).toEqual(folder("C:\\Users\\ann"));
    expect(s.back).toEqual([folder("C:\\Users")]);
    expect(s.forward).toEqual([]);
  });
});

describe("refresh", () => {
  const first = [dir("Invoices"), file("a.txt"), file("b.txt"), file("c.txt")];
  const here = toggleSelected(
    selectOnly(open(initialExplorer(), DOCUMENTS, [first], 1), "a.txt"),
    "c.txt",
  );

  it("keeps what is on screen until the folder is listed again, then shows the new list", () => {
    let s = opening(here, { listing: 2, place: folder(DOCUMENTS), how: "replace" });
    s = applyEvent(s, {
      kind: "opened",
      listing: 2,
      path: DOCUMENTS,
      entries: [dir("Invoices"), file("a.txt")],
      end: false,
    });
    // The list from before is still there, whole.
    expect(names(s)).toEqual(["Invoices", "a.txt", "b.txt", "c.txt"]);
    expect(statusWords(s)).toBe("4 items · 2 selected · refreshing…");
    s = applyEvent(s, { kind: "page", listing: 2, entries: [file("d.txt")], end: true });
    expect(names(s)).toEqual(["Invoices", "a.txt", "d.txt"]);
    expect(s.fresh).toBeUndefined();
    expect(s.content).toEqual({ state: "complete" });
    // What is still listed stays selected; what is gone is not.
    expect([...s.selected]).toEqual(["a.txt"]);
    expect(s.focus).toBeUndefined();
    // A refresh is no step in the history.
    expect(s.back).toEqual([]);
  });

  it("keeps the list from before and says so when the folder cannot be listed again", () => {
    let s = opening(here, { listing: 2, place: folder(DOCUMENTS), how: "replace" });
    s = applyFailure(s, 2, { stage: "path_not_found", message: "C:\\Users\\ann lists no entry named Documents" });
    expect(names(s)).toEqual(["Invoices", "a.txt", "b.txt", "c.txt"]);
    expect(s.refusal?.title).toBe(`${DOCUMENTS} could not be opened`);

    // It fails after its first page: the pages that follow belong to no list.
    let cut = opening(here, { listing: 3, place: folder(DOCUMENTS), how: "replace" });
    cut = applyEvent(cut, { kind: "opened", listing: 3, path: DOCUMENTS, entries: [file("x.txt")], end: false });
    cut = applyFailure(cut, 3, { stage: "access_denied", message: "access was denied" });
    expect(names(cut)).toEqual(["Invoices", "a.txt", "b.txt", "c.txt"]);
    expect(cut.fresh).toBeUndefined();
    expect(cut.refusal).toEqual({
      title: `${DOCUMENTS} could not be listed again`,
      message: "access was denied (access_denied)",
      next: "What you see is the list from before. Press Refresh to try again.",
    });
    const late = applyEvent(cut, { kind: "page", listing: 3, entries: [file("y.txt")], end: true });
    expect(names(late)).toEqual(["Invoices", "a.txt", "b.txt", "c.txt"]);
  });

  it("drops a refresh that was left, and the pages that follow it", () => {
    let s = opening(here, { listing: 2, place: folder(DOCUMENTS), how: "replace" });
    s = applyEvent(s, { kind: "opened", listing: 2, path: DOCUMENTS, entries: [file("x.txt")], end: false });
    s = applyListing(s, { listing: 2, path: DOCUMENTS, entries: 1, state: "cancelled" });
    expect(s.fresh).toBeUndefined();
    s = applyEvent(s, { kind: "page", listing: 2, entries: [file("y.txt")], end: true });
    expect(names(s)).toEqual(["Invoices", "a.txt", "b.txt", "c.txt"]);
    expect(s.content).toEqual({ state: "complete" });
  });

  it("shows a refreshed list that stopped at its bound as that", () => {
    let s = opening(here, { listing: 2, place: folder(DOCUMENTS), how: "replace" });
    s = applyEvent(s, { kind: "opened", listing: 2, path: DOCUMENTS, entries: [file("x.txt")], end: false });
    s = applyListing(s, { listing: 2, path: DOCUMENTS, entries: 1, state: "limit" });
    expect(names(s)).toEqual(["x.txt"]);
    expect(s.content).toEqual({ state: "limit" });
  });
});

describe("the drives", () => {
  it("opens the drives as a place of its own", () => {
    const s = applyDrives(opening(initialExplorer(), { listing: 1, place: DRIVES, how: "push" }), 1, [C, E]);
    expect(s.place).toEqual(DRIVES);
    expect(rowCount(s)).toBe(2);
    expect(keyAt(s, 0)).toBe("C:\\");
    expect(s.content).toEqual({ state: "complete" });
    expect(statusWords(s)).toBe("2 drives");
    expect(openTarget(s, "C:\\")).toEqual(folder("C:\\"));
    expect(openTarget(s, "Z:\\")).toBeNull();
  });

  it("selects the drive it came out of when going up from its root", () => {
    let s = open(initialExplorer(), "E:\\", [[]], 1);
    s = opening(s, { listing: 2, place: DRIVES, how: "push", reveal: "E:\\" });
    s = applyDrives(s, 2, [C, E]);
    expect([...s.selected]).toEqual(["E:\\"]);
    expect(s.back).toEqual([folder("E:\\")]);
    expect(s.entries).toEqual([]);
  });

  it("keeps the selection of a drive that is still there after a refresh", () => {
    let s = applyDrives(opening(initialExplorer(), { listing: 1, place: DRIVES, how: "push" }), 1, [C, E]);
    s = selectOnly(s, "E:\\");
    s = applyDrives(opening(s, { listing: 2, place: DRIVES, how: "replace" }), 2, [C]);
    expect(rowCount(s)).toBe(1);
    expect(s.selected.size).toBe(0);
    expect(s.back).toEqual([]);
  });

  it("ignores a drive list that is not the one it waits for", () => {
    const s = opening(initialExplorer(), { listing: 2, place: DRIVES, how: "push" });
    expect(applyDrives(s, 1, [C])).toBe(s);
    expect(applyDrives(initialExplorer(), 1, [C]).place).toBeNull();
  });
});

describe("the context menu", () => {
  const s = open(initialExplorer(), DOCUMENTS, [[dir("Invoices"), file("a.txt"), link("link")]], 1);
  const menu = (keys: string[]): ExplorerState => ({ ...s, menu: { x: 10, y: 20, keys } });

  it("offers Open for one folder, and Refresh always", () => {
    expect(menuItems(menu(["Invoices"]))).toEqual([
      { action: "xp-menu-open", label: "Open", keys: "Enter", disabled: false },
      { action: "xp-download", label: "Download", disabled: false, separated: true },
      { action: "xp-download-to", label: "Download to…", disabled: false },
      { action: "xp-rename", label: "Rename", keys: "F2", disabled: false },
      { action: "xp-delete", label: "Delete", keys: "Delete", disabled: false },
      { action: "xp-upload-files", label: "Upload files…", disabled: false, separated: true },
      { action: "xp-upload-folder", label: "Upload a folder…", disabled: false },
      { action: "xp-refresh", label: "Refresh", keys: "F5", disabled: false, separated: true },
    ]);
    // A file and a link open nothing; neither do several rows, or none.
    for (const keys of [["a.txt"], ["link"], ["Invoices", "a.txt"], []]) {
      const items = menuItems(menu(keys));
      expect(items[0]?.disabled, keys.join()).toBe(true);
      expect(items.at(-1)?.action).toBe("xp-refresh");
      expect(items.at(-1)?.disabled).toBe(false);
    }
  });
});

describe("the status line", () => {
  it("says what is open, how complete it is and what is selected", () => {
    expect(statusWords(initialExplorer())).toBe("");
    expect(statusWords(opening(initialExplorer(), { listing: 1, place: DRIVES, how: "push" }))).toBe(
      "Opening…",
    );
    const one = open(initialExplorer(), DOCUMENTS, [[file("a.txt")]], 1);
    expect(statusWords(one)).toBe("1 item");
    expect(statusWords(selectOnly(one, "a.txt"))).toBe("1 item · 1 selected");
  });
});

describe("places", () => {
  it("has the drives as one place", () => {
    const place: Place = DRIVES;
    expect(place.kind).toBe("drives");
    expect(folder("C:\\")).toEqual({ kind: "folder", path: "C:\\" });
  });
});

describe("what the backend sends", () => {
  it("takes only a drive and an entry of the contract", () => {
    expect(isExplorerDrive(C)).toBe(true);
    expect(isExplorerDrive(E)).toBe(true);
    for (const bad of [
      null,
      "C:\\",
      { root: "", kind: "fixed" },
      { root: "C:\\", kind: "floppy" },
      { root: "C:\\", kind: "fixed", label: 7 },
      { root: "C:\\", kind: "fixed", totalBytes: "many" },
      { root: "C:\\", kind: "fixed", freeBytes: Number.NaN },
    ]) {
      expect(isExplorerDrive(bad)).toBe(false);
    }
    expect(isExplorerEntry(file("a.txt"))).toBe(true);
    expect(isExplorerEntry(untimed("a.txt"))).toBe(true);
    for (const bad of [
      null,
      "a.txt",
      { ...file("a.txt"), name: "" },
      { ...file("a.txt"), kind: "folder" },
      { ...file("a.txt"), len: "12" },
      { ...file("a.txt"), modifiedMs: "yesterday" },
      { name: "a.txt", kind: "file", len: 1 },
    ]) {
      expect(isExplorerEntry(bad)).toBe(false);
    }
  });

  it("takes only a step and an end of a listing of the contract", () => {
    const page = { kind: "page", listing: 3, entries: [file("a.txt")], end: false };
    expect(isExplorerEvent(page)).toBe(true);
    expect(isExplorerEvent({ ...page, kind: "opened", path: "C:\\Users" })).toBe(true);
    expect(isExplorerEvent({ ...page, kind: "opened", path: "C:\\Users", select: "a.txt" })).toBe(true);
    for (const bad of [
      null,
      { ...page, listing: "3" },
      { ...page, end: 0 },
      { ...page, entries: undefined },
      // One entry that is none spoils the page: a hole would look like a whole folder.
      { ...page, entries: [file("a.txt"), { name: "b" }] },
      { ...page, kind: "opened" },
      { ...page, kind: "opened", path: "" },
      { ...page, kind: "opened", path: "C:\\", select: 1 },
      { ...page, kind: "closed" },
    ]) {
      expect(isExplorerEvent(bad)).toBe(false);
    }
    expect(isExplorerListing({ listing: 3, path: "C:\\", entries: 4, state: "complete" })).toBe(true);
    expect(isExplorerListing({ listing: 3, entries: 0, state: "cancelled" })).toBe(true);
    for (const bad of [
      null,
      { listing: 3, entries: 4, state: "done" },
      { listing: "3", entries: 4, state: "limit" },
      { listing: 3, state: "limit" },
      { listing: 3, entries: 4, state: "limit", path: 1 },
    ]) {
      expect(isExplorerListing(bad)).toBe(false);
    }
  });
});

// --- The second part: send, copy to this computer, rename, delete ---

function entry(patch: Partial<TransferEntry> & { id: number }): TransferEntry {
  return {
    direction: "send",
    kind: "file",
    name: `f${patch.id}`,
    remote: `${DOCUMENTS}\\f${patch.id}`,
    local: `C:\\Users\\sam\\Desktop\\f${patch.id}`,
    done: 0,
    state: "waiting",
    ...patch,
  };
}

function ids(s: ExplorerState): number[] {
  return s.transfers.map((transfer) => transfer.id);
}

const documents = (): ExplorerState =>
  open(
    initialExplorer(),
    DOCUMENTS,
    [[dir("Invoices"), file("a.txt"), file("b.txt"), link("shortcut"), file("report.final.pdf")]],
    1,
  );

describe("the transfer queue", () => {
  it("starts empty, and takes what the backend tells by its number", () => {
    const empty = initialExplorer();
    expect(empty.transfers).toEqual([]);
    expect(applyTransfers(empty, [])).toBe(empty);

    let s = applyTransfers(empty, [entry({ id: 1 }), entry({ id: 2 }), entry({ id: 5 })]);
    expect(ids(s)).toEqual([1, 2, 5]);
    // An entry it has is replaced; the others stay the objects they were.
    const first = s.transfers[0];
    s = applyTransfers(s, [entry({ id: 2, state: "running", done: 10, bytes: 40 })]);
    expect(ids(s)).toEqual([1, 2, 5]);
    expect(s.transfers[1]?.state).toBe("running");
    expect(s.transfers[0]).toBe(first);
    // One that arrives late takes its place in the order of the numbers.
    s = applyTransfers(s, [entry({ id: 4 }), entry({ id: 3 }), entry({ id: 9 })]);
    expect(ids(s)).toEqual([1, 2, 3, 4, 5, 9]);
  });

  it("drops a folder whose files are queued, and never adds one", () => {
    let s = applyTransfers(initialExplorer(), [
      entry({ id: 1, kind: "folder", direction: "download", state: "running" }),
    ]);
    s = applyTransfers(s, [
      entry({ id: 1, kind: "folder", direction: "download", state: "listed" }),
      entry({ id: 2 }),
    ]);
    expect(ids(s)).toEqual([2]);
    expect(ids(applyTransfers(s, [entry({ id: 7, state: "listed" })]))).toEqual([2]);
  });

  it("takes nothing from the backend as done that the backend did not say", () => {
    // Every state arrives as the backend told it: none is turned into done.
    for (const state of ["waiting", "running", "failed", "cancelled", "left_out", "stopped"] as const) {
      const s = applyTransfers(initialExplorer(), [entry({ id: 1, state, done: 100, bytes: 100 })]);
      expect(s.transfers[0]?.state).toBe(state);
      expect(transferWords(s.transfers[0] as TransferEntry)).not.toMatch(/^Sent|^Copied/);
    }
  });

  it("counts the transfers of each state", () => {
    const counts = transferCounts([
      entry({ id: 1, state: "done" }),
      entry({ id: 2, state: "done" }),
      entry({ id: 3, state: "failed" }),
      entry({ id: 4, state: "left_out" }),
      entry({ id: 5, state: "running" }),
      entry({ id: 6 }),
      entry({ id: 7, state: "cancelled" }),
      entry({ id: 8, state: "stopped" }),
      entry({ id: 9, state: "listed" }),
    ]);
    expect(counts).toEqual({
      waiting: 1,
      running: 1,
      done: 2,
      failed: 1,
      cancelled: 1,
      left_out: 1,
      stopped: 1,
    });
    expect(isOpen(entry({ id: 1 }))).toBe(true);
    expect(isOpen(entry({ id: 1, state: "running" }))).toBe(true);
    expect(isOpen(entry({ id: 1, state: "done" }))).toBe(false);
  });

  it("shows what did not arrive first, then what runs and waits, then what ended", () => {
    const rows = queueRows([
      entry({ id: 1, state: "done" }),
      entry({ id: 2, state: "failed" }),
      entry({ id: 3, state: "cancelled" }),
      entry({ id: 4, state: "running" }),
      entry({ id: 5 }),
      entry({ id: 6, state: "left_out" }),
      entry({ id: 7 }),
      entry({ id: 8, state: "stopped" }),
    ]);
    const list = (entries: TransferEntry[]): number[] => entries.map((each) => each.id);
    // The last one queued first among those that did not arrive and that ended.
    expect(list(rows.attention)).toEqual([8, 6, 2]);
    expect(list(rows.running)).toEqual([4]);
    // What waits is in the order it will run.
    expect(list(rows.waiting)).toEqual([5, 7]);
    expect(list(rows.finished)).toEqual([3, 1]);
    expect([rows.attentionMore, rows.waitingMore, rows.finishedMore]).toEqual([0, 0, 0]);
  });

  it("draws a bounded number of rows of each part and counts the rest", () => {
    const many = (state: TransferEntry["state"], count: number, from: number): TransferEntry[] =>
      Array.from({ length: count }, (_, n) => entry({ id: from + n, state }));
    const rows = queueRows([
      ...many("done", QUEUE_ROWS.finished + 30, 1),
      ...many("failed", QUEUE_ROWS.attention + 7, 1000),
      ...many("waiting", QUEUE_ROWS.waiting + 400, 5000),
    ]);
    expect(rows.attention).toHaveLength(QUEUE_ROWS.attention);
    expect(rows.attentionMore).toBe(7);
    expect(rows.waiting).toHaveLength(QUEUE_ROWS.waiting);
    expect(rows.waitingMore).toBe(400);
    expect(rows.finished).toHaveLength(QUEUE_ROWS.finished);
    expect(rows.finishedMore).toBe(30);
    // The first that waits is the first that will run.
    expect(rows.waiting[0]?.id).toBe(5000);
  });

  it("says how far a transfer is, and only of a verified one that it was sent or copied", () => {
    const sent = entry({ id: 1, bytes: 2048, done: 2048, state: "done" });
    expect(transferWords(sent)).toBe("Sent · 2.0 KB");
    expect(transferWords({ ...sent, direction: "download" })).toBe("Copied · 2.0 KB");
    expect(transferWords(entry({ id: 1 }))).toBe("Waiting");
    const half = entry({ id: 1, bytes: 2048, done: 1024, state: "running" });
    expect(transferPercent(half)).toBe(50);
    expect(transferWords(half)).toBe("50% · 1.0 KB of 2.0 KB");
    // Nearly there is not there.
    expect(transferPercent(entry({ id: 1, bytes: 1000, done: 999, state: "running" }))).toBe(99);
    expect(transferPercent(entry({ id: 1, state: "running" }))).toBeUndefined();
    expect(transferWords(entry({ id: 1, state: "running" }))).toBe("Sending…");
    expect(transferWords(entry({ id: 1, state: "running", direction: "download" }))).toBe("Copying…");
    // An empty file is whole once it is done, and not before.
    expect(transferPercent(entry({ id: 1, bytes: 0, state: "running" }))).toBe(0);
    expect(transferPercent(entry({ id: 1, bytes: 0, state: "done" }))).toBe(100);
    expect(transferWords(entry({ id: 1, state: "failed" }))).toBe("Not sent");
    expect(transferWords(entry({ id: 1, state: "failed", direction: "download" }))).toBe("Not copied");
    expect(transferWords(entry({ id: 1, state: "failed", kind: "folder" }))).toBe("Not complete");
    expect(transferWords(entry({ id: 1, state: "cancelled", done: 5, bytes: 9 }))).toBe("Cancelled");
    expect(transferWords(entry({ id: 1, state: "left_out" }))).toBe("Left out");
    expect(transferWords(entry({ id: 1, state: "stopped" }))).toBe("Stopped with the session");
    expect(transferWords(entry({ id: 1, kind: "folder", state: "done" }))).toBe("Made");
    expect(transferWords(entry({ id: 1, kind: "folder", state: "running" }))).toBe("Making the folder…");
    expect(
      transferWords(entry({ id: 1, kind: "folder", state: "running", direction: "download" })),
    ).toBe("Listing the folder…");
    expect(directionWords(sent)).toBe("To the customer computer");
    expect(directionWords({ ...sent, direction: "download" })).toBe(
      "To this computer · leaves the customer computer",
    );
  });

  it("says the queue in one line, and how it came to rest", () => {
    const running = [entry({ id: 1, state: "running" }), entry({ id: 2 })];
    expect(queueWords(running)).toBe("1 running · 1 waiting");
    expect(queueWords([])).toBe("");
    const rested = [
      entry({ id: 1, state: "done" }),
      entry({ id: 2, state: "failed" }),
      entry({ id: 3, state: "cancelled" }),
      entry({ id: 4, state: "left_out" }),
    ];
    expect(queueWords(rested)).toBe("1 failed · 1 left out · 1 cancelled · 1 done");
    expect(queueRestWords(running, rested)).toBe(
      "Transfers ended: 1 done, 2 not completed, 1 cancelled.",
    );
    expect(queueRestWords(running, [entry({ id: 1, state: "done" })])).toBe(
      "Transfers ended: 1 done.",
    );
    // Nothing is said while something still waits, or when nothing was running.
    expect(queueRestWords(running, running)).toBeUndefined();
    expect(queueRestWords(rested, rested)).toBeUndefined();
    expect(queueRestWords([], rested)).toBeUndefined();
  });

  it("says how a send or a copy was queued", () => {
    expect(queuedWords("send", { queued: 1, leftOut: 0, skipped: 0 })).toBe("1 item queued to send.");
    expect(queuedWords("download", { queued: 3, leftOut: 0, skipped: 0 })).toBe(
      "3 items queued to copy to this computer.",
    );
    expect(queuedWords("send", { queued: 2, leftOut: 1, skipped: 4 })).toBe(
      "2 items queued to send, 4 skipped, 1 left out, see the transfers.",
    );
  });

  it("says what the end of full control cut short", () => {
    expect(stoppedTransfers([])).toBeUndefined();
    expect(stoppedTransfers([entry({ id: 1, state: "done" }), entry({ id: 2, state: "failed" })])).toBeUndefined();
    const one = stoppedTransfers([entry({ id: 1, name: "setup.exe", state: "running" })]);
    expect(one?.title).toBe("1 transfer stopped with the session");
    expect(one?.message).toBe("Not complete: setup.exe.");
    const many = stoppedTransfers([
      entry({ id: 1, name: "a", state: "stopped" }),
      entry({ id: 2, name: "b" }),
      entry({ id: 3, name: "c" }),
      entry({ id: 4, name: "d" }),
      entry({ id: 5, name: "e", state: "done" }),
    ]);
    expect(many?.title).toBe("4 transfers stopped with the session");
    expect(many?.message).toBe("Not complete: a, b, c and 1 more.");
    expect(many?.next).toContain("Nothing more is sent or copied");
  });
});

describe("a name that is taken", () => {
  const check = (names: Array<[string, boolean]>): TransferCheck => ({
    folder: DOCUMENTS,
    conflicts: names.map(([name, replace]) => ({ name, kind: "file", existing: "file", replace })),
  });
  const ask = (names: Array<[string, boolean]>): ConflictQuestion => {
    const question = conflictQuestion(check(names), { direction: "send", folder: DOCUMENTS, offer: 4 });
    if (!question) throw new Error("no question");
    return question;
  };

  it("asks nothing when no name is taken", () => {
    expect(conflictQuestion(check([]), { direction: "send", folder: DOCUMENTS, offer: 1 })).toBeUndefined();
  });

  it("asks about one name after the other and keeps each choice", () => {
    let q = ask([
      ["a.txt", true],
      ["b.txt", true],
      ["c.txt", true],
    ]);
    expect([q.at, q.all, q.where, q.offer]).toEqual([0, false, DOCUMENTS, 4]);
    q = answerConflict(q, "keep_both");
    expect(q.at).toBe(1);
    expect(conflictsDecided(q)).toBe(false);
    q = answerConflict(q, "replace");
    q = answerConflict(q, "skip");
    expect(conflictsDecided(q)).toBe(true);
    expect(q.choices).toEqual({ "a.txt": "keep_both", "b.txt": "replace", "c.txt": "skip" });
    // Nothing is left to answer.
    expect(answerConflict(q, "replace")).toBe(q);
  });

  it("applies a choice to all that follow when asked to", () => {
    const q = answerConflict(
      {
        ...ask([
          ["a.txt", true],
          ["b.txt", true],
          ["c.txt", true],
        ]),
        all: true,
      },
      "keep_both",
    );
    expect(conflictsDecided(q)).toBe(true);
    expect(q.choices).toEqual({ "a.txt": "keep_both", "b.txt": "keep_both", "c.txt": "keep_both" });
  });

  it("never replaces where Replace is not offered, also not for all", () => {
    const first = ask([
      ["a.txt", false],
      ["b.txt", true],
    ]);
    // Replace is not a choice for this name.
    expect(answerConflict(first, "replace")).toBe(first);
    // Replace for all stops at the name that cannot be replaced, and asks.
    const q = answerConflict(
      {
        ...ask([
          ["a.txt", true],
          ["b.txt", true],
          ["folder", false],
          ["d.txt", true],
        ]),
        all: true,
      },
      "replace",
    );
    expect(q.at).toBe(2);
    expect(conflictsDecided(q)).toBe(false);
    expect(q.choices).toEqual({ "a.txt": "replace", "b.txt": "replace" });
    const rest = answerConflict(q, "skip");
    expect(rest.choices).toEqual({
      "a.txt": "replace",
      "b.txt": "replace",
      folder: "skip",
      "d.txt": "skip",
    });
  });
});

describe("the actions of the selection", () => {
  const disabled = (s: ExplorerState, keys: string[]): Record<string, boolean> =>
    Object.fromEntries(actionsFor(s, keys).map((item) => [item.action, item.disabled]));

  it("offers what can be done with the rows it is asked for", () => {
    const s = documents();
    expect(canChange(s)).toBe(true);
    expect(disabled(s, ["a.txt"])).toEqual({
      "xp-menu-open": true,
      "xp-download": false,
      "xp-download-to": false,
      "xp-rename": false,
      "xp-delete": false,
      "xp-upload-files": false,
      "xp-upload-folder": false,
      "xp-refresh": false,
    });
    // Several rows are copied and deleted together; one at a time is renamed.
    const several = disabled(s, ["a.txt", "b.txt"]);
    expect([several["xp-download"], several["xp-rename"], several["xp-delete"]]).toEqual([
      false,
      true,
      false,
    ]);
    // Without a row there is only the place to send to and to refresh.
    const none = disabled(s, []);
    expect([none["xp-download"], none["xp-rename"], none["xp-delete"], none["xp-upload-files"]]).toEqual(
      [true, true, true, false],
    );
  });

  it("changes nothing at the drives, after the session, or while a delete runs", () => {
    const drives = applyDrives(opening(initialExplorer(), { listing: 1, place: DRIVES, how: "push" }), 1, [C]);
    expect(canChange(drives)).toBe(false);
    const atDrives = disabled(drives, ["C:\\"]);
    expect(atDrives["xp-menu-open"]).toBe(false);
    for (const action of ["xp-download", "xp-download-to", "xp-rename", "xp-delete", "xp-upload-files", "xp-upload-folder"]) {
      expect(atDrives[action], action).toBe(true);
    }
    const ended = { ...documents(), ended: "The session has ended. Browsing has stopped." };
    expect(canChange(ended)).toBe(false);
    expect(Object.values(disabled(ended, ["a.txt"])).filter((d) => !d)).toHaveLength(1);
    // One delete at a time; a copy and an upload go on.
    const deleting = { ...documents(), removal: removalOf(1, 2) };
    const during = disabled(deleting, ["a.txt"]);
    expect([during["xp-rename"], during["xp-delete"], during["xp-download"], during["xp-upload-files"]]).toEqual(
      [true, true, false, false],
    );
  });

  it("is the context menu, and the menu of the Upload button is its two ways to choose", () => {
    const s = documents();
    const menu = { ...s, menu: { x: 1, y: 1, keys: ["a.txt"] } };
    expect(menuItems(menu)).toEqual(actionsFor(s, ["a.txt"]));
    expect(menuItems({ ...s, menu: { x: 1, y: 1, keys: [], only: "upload" } })).toEqual([
      { action: "xp-upload-files", label: "Files…", disabled: false },
      { action: "xp-upload-folder", label: "A folder…", disabled: false },
    ]);
  });

  it("names the selected rows in the order of the list", () => {
    let s = documents();
    expect(selectedKeys(s)).toEqual([]);
    s = toggleSelected(toggleSelected(selectOnly(s, "report.final.pdf"), "Invoices"), "a.txt");
    expect(selectedKeys(s)).toEqual(["Invoices", "a.txt", "report.final.pdf"]);
    expect(deleteItems(s, selectedKeys(s))).toEqual([
      { name: "Invoices", kind: "dir" },
      { name: "a.txt", kind: "file" },
      { name: "report.final.pdf", kind: "file" },
    ]);
    // A copy is told a folder from everything else; what a name really is,
    // the backend finds out itself.
    expect(transferNames(s, ["Invoices", "a.txt", "shortcut", "gone"])).toEqual([
      { name: "Invoices", kind: "folder" },
      { name: "a.txt", kind: "file" },
      { name: "shortcut", kind: "file" },
    ]);
  });
});

describe("renaming", () => {
  it("selects the name without its extension, and a folder's whole name", () => {
    expect(renameSelection(file("report.final.pdf"))).toEqual([0, 12]);
    expect(renameSelection(file("README"))).toEqual([0, 6]);
    expect(renameSelection(file(".gitignore"))).toEqual([0, 10]);
    expect(renameSelection(dir("Photos.2026"))).toEqual([0, 11]);
  });

  it("says why a name cannot be used before anything is asked", () => {
    const s = documents();
    expect(renameProblem(s, "a.txt", "c.txt")).toBeUndefined();
    // Another letter case of its own name is a new name.
    expect(renameProblem(s, "a.txt", "A.TXT")).toBeUndefined();
    expect(renameProblem(s, "a.txt", "B.txt")).toBe("B.txt is already in this folder.");
    expect(renameProblem(s, "a.txt", "invoices")).toBe("invoices is already in this folder.");
    expect(renameProblem(s, "a.txt", "")).toBe("A name cannot be empty.");
    expect(renameProblem(s, "a.txt", "   ")).toBe("A name cannot be empty.");
    for (const bad of ["sub\\x", "a/b", "c:d", "what?", "a*b", 'say "x"', "<x>", "a|b", "tab\there"]) {
      expect(renameProblem(s, "a.txt", bad), bad).toBe(
        'A name cannot hold any of \\ / : * ? " < > |.',
      );
    }
    expect(renameProblem(s, "a.txt", "..")).toBe("This name stands for a folder, not for an entry.");
    expect(renameProblem(s, "a.txt", "name.")).toBe("A name cannot end in a dot or a space.");
    expect(renameProblem(s, "a.txt", "name ")).toBe("A name cannot end in a dot or a space.");
  });

  it("selects the new name once the folder is listed again", () => {
    let s: ExplorerState = { ...selectOnly(documents(), "a.txt"), reveal: "z.txt" };
    s = opening(s, { listing: 2, place: folder(DOCUMENTS), how: "replace" });
    s = applyEvent(s, {
      kind: "opened",
      listing: 2,
      path: DOCUMENTS,
      entries: [dir("Invoices"), file("b.txt"), file("z.txt")],
      end: true,
    });
    expect(names(s)).toEqual(["Invoices", "b.txt", "z.txt"]);
    expect([...s.selected]).toEqual(["z.txt"]);
    expect(s.focus).toBe("z.txt");
    expect(s.reveal).toBeUndefined();
  });

  it("closes the field of a rename when the explorer goes elsewhere", () => {
    const s: ExplorerState = { ...documents(), renaming: "a.txt" };
    expect(opening(s, { listing: 2, place: DRIVES, how: "push" }).renaming).toBeUndefined();
  });
});

describe("deleting", () => {
  it("asks with the number of items and says that there is no recycle bin", () => {
    const one = deleteWords({ kind: "delete", folder: DOCUMENTS, items: [{ name: "a.txt", kind: "file" }] });
    expect(one.title).toBe("Delete a.txt?");
    expect(one.lines).toEqual([
      `There is no recycle bin on this path: what is deleted from ${DOCUMENTS} is gone for good.`,
    ]);
    const folderOnly = deleteWords({
      kind: "delete",
      folder: DOCUMENTS,
      items: [{ name: "Invoices", kind: "dir" }],
    });
    expect(folderOnly.title).toBe("Delete Invoices?");
    expect(folderOnly.lines[0]).toBe("The folder is deleted with everything in it.");
    const many = deleteWords({
      kind: "delete",
      folder: DOCUMENTS,
      items: [
        { name: "A", kind: "dir" },
        { name: "B", kind: "dir" },
        ...["c", "d", "e", "f", "g"].map((name) => ({ name, kind: "file" as const })),
      ],
    });
    expect(many.title).toBe("Delete 7 items?");
    expect(many.lines).toEqual([
      "A, B, c, d, e and 2 more.",
      "2 folders are deleted with everything in them.",
      `There is no recycle bin on this path: what is deleted from ${DOCUMENTS} is gone for good.`,
    ]);
  });

  it("follows the delete that runs, and no other", () => {
    let s: ExplorerState = { ...documents(), removal: removalOf(3, 4) };
    expect(removalWords(removalOf(3, 4))).toBe("Deleting 4 items…");
    const step = { removal: 3, item: "Photos", at: 2, of: 4, removedFiles: 120, removedDirs: 1 };
    s = applyRemoval(s, step);
    expect(s.removal).toEqual({ id: 3, items: 4, item: "Photos", at: 2, removedFiles: 120, removedDirs: 1 });
    expect(removalWords(s.removal ?? removalOf(0, 0))).toBe(
      "Deleting item 2 of 4: Photos… 120 files and 1 folder removed so far.",
    );
    expect(applyRemoval(s, { ...step, removal: 9 })).toBe(s);
    expect(applyRemoval(documents(), step).removal).toBeUndefined();
    expect(removalWords({ ...removalOf(3, 4), cancelling: true })).toBe("Cancelling the delete…");
  });

  const report = (patch: Partial<DeleteReport>): DeleteReport => ({
    selected: 3,
    removed: 3,
    removedFiles: 3,
    removedDirs: 0,
    failed: [],
    failedMore: 0,
    cancelled: false,
    notTried: 0,
    ...patch,
  });

  it("says a delete that removed everything in a sentence, and nothing else as that", () => {
    expect(deleteOutcome(report({}))).toEqual({ notice: "3 items deleted." });
    expect(deleteOutcome(report({ selected: 1, removed: 1, removedFiles: 40, removedDirs: 3 }))).toEqual({
      notice: "1 item deleted (40 files and 3 folders in all).",
    });
  });

  it("names what could not be removed, and never says that it was", () => {
    const partial = deleteOutcome(
      report({
        removed: 1,
        removedFiles: 1,
        failed: [
          { path: `${DOCUMENTS}\\open.docx`, reason: "the file is in use" },
          { path: `${DOCUMENTS}\\Old`, reason: "the folder is not empty" },
        ],
        failedMore: 5,
      }),
    );
    expect(partial.notice).toBe("2 of 3 items could not be deleted.");
    expect(partial.refusal?.title).toBe("2 of 3 items could not be deleted");
    expect(partial.refusal?.message).toBe("1 of 3 items was deleted.");
    expect(partial.refusal?.details).toHaveLength(2);
    expect(partial.refusal?.more).toBe(5);
    expect(partial.refusal?.next).toContain("The list shows what is there now.");
    // Nothing was removed at all.
    const none = deleteOutcome(
      report({ removed: 0, removedFiles: 0, failed: [{ path: "x", reason: "access was denied" }] }),
    );
    expect(none.refusal?.title).toBe("3 of 3 items could not be deleted");
    expect(none.refusal?.message).toBe("0 of 3 items were deleted.");
  });

  it("says a cancelled delete and one the session stopped as that", () => {
    const cancelled = deleteOutcome(
      report({ removed: 1, removedFiles: 200, removedDirs: 4, cancelled: true, notTried: 1 }),
    );
    expect(cancelled.refusal?.title).toBe("The delete was cancelled");
    expect(cancelled.refusal?.message).toBe(
      "1 of 3 items was deleted (200 files and 4 folders in all). 1 item was not tried.",
    );
    const ended = deleteOutcome(
      report({ removed: 0, removedFiles: 0, notTried: 3, ended: "The session ended before this was complete." }),
    );
    expect(ended.refusal?.title).toBe("The delete stopped with the session");
    expect(ended.refusal?.message).toBe("0 of 3 items were deleted. 3 items were not tried.");
    expect(ended.refusal?.next).toBe("The session ended before this was complete.");
    // All were removed, and yet the session ended: still not a plain success.
    expect(deleteOutcome(report({ ended: "x" })).refusal).toBeDefined();
  });
});

describe("what the backend sends for a change", () => {
  it("takes only what the contract describes", () => {
    const transfer = entry({ id: 1, bytes: 10 });
    expect(isTransferEntry(transfer)).toBe(true);
    expect(isTransferEntry({ ...transfer, reason: "x" })).toBe(true);
    for (const bad of [
      { ...transfer, id: "1" },
      { ...transfer, direction: "sideways" },
      { ...transfer, kind: "link" },
      { ...transfer, state: "finished" },
      { ...transfer, name: 3 },
      { ...transfer, bytes: "10" },
      { ...transfer, done: Number.NaN },
      { ...transfer, reason: 7 },
      null,
      "transfer",
    ]) {
      expect(isTransferEntry(bad), JSON.stringify(bad)).toBe(false);
    }
    expect(isTransfersEvent({ entries: [transfer] })).toBe(true);
    expect(isTransfersEvent({ entries: [] })).toBe(true);
    // An event with an entry that is none is not taken at all.
    expect(isTransfersEvent({ entries: [transfer, { id: 2 }] })).toBe(false);
    expect(isTransfersEvent({})).toBe(false);

    expect(isUploadOffer({ offer: 1, items: 2, names: ["a", "b"] })).toBe(true);
    expect(isUploadOffer({ offer: 1, items: 2, names: [1] })).toBe(false);
    expect(isUploadOffer(null)).toBe(false);

    expect(
      isTransferCheck({
        folder: DOCUMENTS,
        conflicts: [{ name: "a", kind: "file", existing: "symlink", replace: false }],
      }),
    ).toBe(true);
    expect(isTransferCheck({ folder: DOCUMENTS, conflicts: [{ name: "a", kind: "file" }] })).toBe(false);
    expect(isTransferCheck({ conflicts: [] })).toBe(false);

    expect(isTransferQueued({ queued: 1, leftOut: 0, skipped: 0 })).toBe(true);
    expect(isTransferQueued({ queued: 1 })).toBe(false);
    expect(isChosenFolder({ folder: 1, path: "D:\\Kept" })).toBe(true);
    expect(isChosenFolder({ folder: "1", path: "D:\\Kept" })).toBe(false);
    expect(isChosenFolder(null)).toBe(false);

    const step = { removal: 1, item: "a", at: 1, of: 2, removedFiles: 0, removedDirs: 0 };
    expect(isRemovalProgress(step)).toBe(true);
    expect(isRemovalProgress({ ...step, item: 4 })).toBe(false);

    const done: DeleteReport = {
      selected: 1,
      removed: 1,
      removedFiles: 1,
      removedDirs: 0,
      failed: [],
      failedMore: 0,
      cancelled: false,
      notTried: 0,
    };
    expect(isDeleteReport(done)).toBe(true);
    expect(isDeleteReport({ ...done, ended: "x", failed: [{ path: "p", reason: "r" }] })).toBe(true);
    expect(isDeleteReport({ ...done, failed: [{ path: "p" }] })).toBe(false);
    expect(isDeleteReport({ ...done, cancelled: "no" })).toBe(false);
  });

  it("knows the failures that mean the session is over", () => {
    expect(endedWords("no_consented_session")).toBe("The session has ended. Browsing has stopped.");
    expect(endedWords("disconnected")).toContain("was lost");
    expect(endedWords("access_denied")).toBeUndefined();
  });
});

