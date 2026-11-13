// The file explorer of the live session: its state and pure derivations
// (ST-M010-002, REQ-F-028).
//
// The Files tab is a function of one `ExplorerState`: the place it stands in
// (the drives, or a folder by the path the customer computer gives it), what
// is listed there and how complete that is, the history, the view, the sort,
// the selection, and the part of the list that is in view. Nothing here talks
// to Tauri or to the DOM, so all of it is tested in jsdom.
//
// Three rules shape it:
//
// - **The explorer stays where it was until the next place is open.** A place
//   the technician asks for is `pending` first. Only the first page of its
//   listing moves the explorer there; a refusal leaves everything as it is
//   and is shown as that refusal.
// - **Nothing looks complete that is not** (REQ-N-008). What is listed is
//   `loading`, `complete`, cut at a `limit`, or `interrupted` with a reason,
//   and the view words each of them. An empty folder is a complete listing
//   without entries, and nothing else.
// - **What the technician works with does not move.** A folder arrives in
//   pages, in the order the customer computer lists it, and each page is
//   sorted into what is there. Rows are selected by name, so the selection
//   stays on the same entries, and the row in focus, or else the first row in
//   view, stays where it is on screen.

import type {
  ExplorerDrive,
  ExplorerDriveKind,
  ExplorerEntry,
  ExplorerEvent,
  ExplorerListing,
} from "../shared/contract";
import { formatBytes, plural } from "../shared/feed";

/** The height of one row of the list, in pixels; the stylesheet agrees. */
export const ROW_HEIGHT = 28;
/** The width of one column of the List view, in pixels. */
export const LIST_COLUMN_WIDTH = 260;
/** The rows drawn beyond the part in view, on each side. */
export const OVERSCAN = 8;
/** The size of the list before it has been measured. */
export const DEFAULT_VIEWPORT = { height: 480, width: 800 };
/** The most places kept to go back or forward to. */
export const HISTORY_LIMIT = 100;

export type ExplorerView = "details" | "list";

export type SortKey = "name" | "modified" | "type" | "size";

export interface Sort {
  key: SortKey;
  descending: boolean;
}

/** A place the explorer stands in: the drives, or a folder by its real path. */
export type Place = { kind: "drives" } | { kind: "folder"; path: string };

export const DRIVES: Place = { kind: "drives" };

export function folder(path: string): Place {
  return { kind: "folder", path };
}

/** How complete the listing of the place on screen is. */
export type Content =
  | { state: "loading" }
  | { state: "complete" }
  // The folder holds more entries than the explorer lists.
  | { state: "limit" }
  // The listing ended before the end of the folder, for this reason.
  | { state: "interrupted"; reason: string };

/** How a place that was asked for joins the history once it is open. */
export type How = "push" | "back" | "forward" | "replace";

/** A place the technician asked for, on its way. */
export interface Pending {
  listing: number;
  place: Place;
  how: How;
  // The row to select and bring into view there: the folder that was left
  // for the one above it.
  reveal?: string;
}

/** A place that could not be opened, in the application's words. */
export interface Refusal {
  title: string;
  message: string;
  next: string;
}

/** The context menu: where it opened, and for which rows (none: the place). */
export interface Menu {
  x: number;
  y: number;
  keys: string[];
}

/** The part of the list in view, in pixels. */
export interface Viewport {
  top: number;
  left: number;
  height: number;
  width: number;
}

export interface ExplorerState {
  // Where the explorer stands; `null` before the first place is open.
  place: Place | null;
  drives: ExplorerDrive[];
  // The entries of the folder, in the order they are shown.
  entries: ExplorerEntry[];
  content: Content;
  // The listing whose pages fill `entries`, or `fresh` during a refresh.
  listing?: number;
  // A refresh: the folder as it is being listed again. What is on screen
  // stays until this is complete, then it takes its place.
  fresh?: ExplorerEntry[];
  pending?: Pending;
  refusal?: Refusal;
  // The session ended: browsing has stopped, in words.
  ended?: string;
  back: Place[];
  forward: Place[];
  view: ExplorerView;
  sort: Sort;
  // The selected rows, by key: an entry's name, a drive's root.
  selected: ReadonlySet<string>;
  // Where a Shift selection starts.
  anchor?: string;
  // The row the keyboard is on.
  focus?: string;
  // A row to bring into view as soon as it is listed.
  reveal?: string;
  viewport: Viewport;
  // Counts the times the model itself moved the list, so the controller
  // knows when to move the real one.
  scrollSeq: number;
  menu?: Menu;
  // The path field is open for typing, starting from this text.
  editing?: string;
}

export function initialExplorer(): ExplorerState {
  return {
    place: null,
    drives: [],
    entries: [],
    content: { state: "loading" },
    back: [],
    forward: [],
    view: "details",
    sort: { key: "name", descending: false },
    selected: new Set(),
    viewport: { top: 0, left: 0, ...DEFAULT_VIEWPORT },
    scrollSeq: 0,
  };
}

// --- Places and paths ---

export function samePlace(a: Place | null, b: Place | null): boolean {
  if (a === null || b === null) return a === b;
  if (a.kind === "drives" || b.kind === "drives") return a.kind === b.kind;
  return a.path === b.path;
}

/** A place as the path field spells it: a folder's path, nothing for the drives. */
export function placeText(place: Place | null): string {
  return place?.kind === "folder" ? place.path : "";
}

/** The path of the entry `name` in the folder `path`. */
export function childPath(path: string, name: string): string {
  return path.endsWith("\\") ? `${path}${name}` : `${path}\\${name}`;
}

/** The names a path is made of: `C:\` and then each folder. */
function parts(path: string): string[] {
  const names = path.split("\\").filter((part) => part !== "");
  const drive = names.shift();
  return drive === undefined ? [] : [`${drive}\\`, ...names];
}

/**
 * The parts of the path bar under the computer itself: each with the place
 * a click on it opens. The path is the one the customer computer gave, so
 * every part of it is a real folder.
 */
export function crumbs(place: Place | null): Array<{ label: string; place: Place }> {
  if (place?.kind !== "folder") return [];
  const out: Array<{ label: string; place: Place }> = [];
  let path = "";
  for (const part of parts(place.path)) {
    path = path === "" ? part : childPath(path, part);
    out.push({ label: part, place: folder(path) });
  }
  return out;
}

/** The place above: the folder that holds this one, the drives above a root. */
export function parentPlace(place: Place | null): Place | null {
  if (place?.kind !== "folder") return null;
  const above = crumbs(place);
  above.pop();
  return above[above.length - 1]?.place ?? DRIVES;
}

/** The name of a folder in the place above it: its last part, a root for a drive. */
export function lastName(place: Place): string | undefined {
  if (place.kind !== "folder") return undefined;
  const names = parts(place.path);
  return names[names.length - 1];
}

// --- Words ---

function pad(n: number): string {
  return String(n).padStart(2, "0");
}

/** "2026-11-03 14:30" in local time; nothing when the time is not known. */
export function modifiedWords(ms: number | undefined): string {
  if (ms === undefined) return "";
  const date = new Date(ms);
  if (Number.isNaN(date.getTime())) return "";
  return (
    `${date.getFullYear()}-${pad(date.getMonth() + 1)}-${pad(date.getDate())} ` +
    `${pad(date.getHours())}:${pad(date.getMinutes())}`
  );
}

/** What an entry is, in words: "File folder", "Link", "PDF file", "File". */
export function typeWords(entry: ExplorerEntry): string {
  switch (entry.kind) {
    case "dir":
      return "File folder";
    case "symlink":
      return "Link";
    case "other":
      return "Other";
    case "file": {
      const dot = entry.name.lastIndexOf(".");
      const extension = dot > 0 ? entry.name.slice(dot + 1) : "";
      return extension === "" || extension.length > 12 || /\s/.test(extension)
        ? "File"
        : `${extension.toUpperCase()} file`;
    }
  }
}

/** The size of a file; a folder or a link has none to show. */
export function sizeWords(entry: ExplorerEntry): string {
  return entry.kind === "file" ? formatBytes(entry.len) : "";
}

/** A drive as Windows names it: "Windows (C:)", or "C:" without a label. */
export function driveName(drive: ExplorerDrive): string {
  const letter = drive.root.replace(/\\+$/, "");
  const label = drive.label?.trim();
  return label ? `${label} (${letter})` : letter;
}

const DRIVE_KINDS: Record<ExplorerDriveKind, string> = {
  fixed: "Local disk",
  removable: "Removable drive",
  network: "Network drive",
  optical: "CD or DVD drive",
  ram_disk: "RAM disk",
  unknown: "Drive",
};

export function driveKindWords(kind: ExplorerDriveKind): string {
  return DRIVE_KINDS[kind] ?? DRIVE_KINDS.unknown;
}

// --- Rows ---

/** How many rows the place on screen has. */
export function rowCount(s: ExplorerState): number {
  return s.place?.kind === "drives" ? s.drives.length : s.entries.length;
}

/** The key of the row at `index`: an entry's name, a drive's root. */
export function keyAt(s: ExplorerState, index: number): string | undefined {
  return s.place?.kind === "drives" ? s.drives[index]?.root : s.entries[index]?.name;
}

/** The index of the row with `key`, or -1. */
export function indexOfKey(s: ExplorerState, key: string | undefined): number {
  if (key === undefined) return -1;
  return s.place?.kind === "drives"
    ? s.drives.findIndex((drive) => drive.root === key)
    : s.entries.findIndex((entry) => entry.name === key);
}

/** The entry with the name `key`, when the place is a folder that lists it. */
export function entryOf(s: ExplorerState, key: string): ExplorerEntry | undefined {
  return s.place?.kind === "folder" ? s.entries.find((entry) => entry.name === key) : undefined;
}

/**
 * The place a row opens: a drive opens its root, a folder itself. A file and
 * a link open nothing: a link is shown and never followed.
 */
export function openTarget(s: ExplorerState, key: string | undefined): Place | null {
  if (key === undefined || s.place === null) return null;
  if (s.place.kind === "drives") {
    return s.drives.some((drive) => drive.root === key) ? folder(key) : null;
  }
  const entry = entryOf(s, key);
  return entry?.kind === "dir" ? folder(childPath(s.place.path, entry.name)) : null;
}

// --- Order ---

// One order of names for everybody: digits by their value ("file2" before
// "file10"), letters without regard to case or accent, and the exact
// spelling to tell two such names apart.
const collator = new Intl.Collator("en", { numeric: true, sensitivity: "base" });

function byName(a: string, b: string): number {
  return collator.compare(a, b) || (a < b ? -1 : a > b ? 1 : 0);
}

/** The column a first click sorts downward: the newest and the largest first. */
const DOWNWARD_FIRST: readonly SortKey[] = ["modified", "size"];

/**
 * The order of the entries: folders first, then by the column, then by name.
 * It is a total order, since no two entries of a folder share a name.
 */
export function compareEntries(sort: Sort): (a: ExplorerEntry, b: ExplorerEntry) => number {
  const direction = sort.descending ? -1 : 1;
  const column = (a: ExplorerEntry, b: ExplorerEntry): number => {
    switch (sort.key) {
      case "name":
        return 0;
      case "modified":
        // An entry without a time comes after those that have one.
        if (a.modifiedMs === undefined || b.modifiedMs === undefined) {
          return a.modifiedMs === b.modifiedMs
            ? 0
            : (a.modifiedMs === undefined ? 1 : -1) * direction;
        }
        return a.modifiedMs - b.modifiedMs;
      case "type":
        return byName(typeWords(a), typeWords(b));
      case "size":
        return a.len - b.len;
    }
  };
  return (a, b) => {
    const folders = Number(b.kind === "dir") - Number(a.kind === "dir");
    if (folders !== 0) return folders;
    return (column(a, b) || byName(a.name, b.name)) * direction;
  };
}

export function sortEntries(entries: readonly ExplorerEntry[], sort: Sort): ExplorerEntry[] {
  return entries.slice().sort(compareEntries(sort));
}

/**
 * Sorts a page into entries that are sorted already. `before[i]` says before
 * which of the old entries the i-th new one went, so a caller knows how far a
 * row moved. Each new entry is placed by a search, so a page costs little in
 * a folder of any size.
 */
export function mergeEntries(
  sorted: readonly ExplorerEntry[],
  page: readonly ExplorerEntry[],
  sort: Sort,
): { entries: ExplorerEntry[]; before: number[] } {
  if (page.length === 0) return { entries: sorted.slice(), before: [] };
  const compare = compareEntries(sort);
  const incoming = page.slice().sort(compare);
  const before: number[] = [];
  let from = 0;
  for (const entry of incoming) {
    let low = from;
    let high = sorted.length;
    while (low < high) {
      const middle = (low + high) >> 1;
      if (compare(sorted[middle] as ExplorerEntry, entry) <= 0) low = middle + 1;
      else high = middle;
    }
    before.push(low);
    from = low;
  }
  const entries: ExplorerEntry[] = new Array(sorted.length + incoming.length);
  let read = 0;
  let write = 0;
  for (let i = 0; i < incoming.length; i += 1) {
    const stop = before[i] as number;
    while (read < stop) entries[write++] = sorted[read++] as ExplorerEntry;
    entries[write++] = incoming[i] as ExplorerEntry;
  }
  while (read < sorted.length) entries[write++] = sorted[read++] as ExplorerEntry;
  return { entries, before };
}

// --- The part in view ---

export interface Layout {
  // The List view runs down a column and goes on in the next.
  rowsPerColumn: number;
  columns: number;
  // The size of everything, in pixels.
  width: number;
  height: number;
}

export function layoutOf(s: ExplorerState, count: number = rowCount(s)): Layout {
  if (s.view === "details") {
    return { rowsPerColumn: count, columns: 1, width: 0, height: count * ROW_HEIGHT };
  }
  const rowsPerColumn = Math.max(1, Math.floor(s.viewport.height / ROW_HEIGHT));
  const columns = Math.ceil(count / rowsPerColumn);
  return {
    rowsPerColumn,
    columns,
    width: columns * LIST_COLUMN_WIDTH,
    height: Math.min(count, rowsPerColumn) * ROW_HEIGHT,
  };
}

/** Where the row at `index` lies, in pixels from the start of the list. */
export function positionOf(s: ExplorerState, index: number): { left: number; top: number } {
  if (s.view === "details") return { left: 0, top: index * ROW_HEIGHT };
  const { rowsPerColumn } = layoutOf(s);
  return {
    left: Math.floor(index / rowsPerColumn) * LIST_COLUMN_WIDTH,
    top: (index % rowsPerColumn) * ROW_HEIGHT,
  };
}

/** The rows to draw: those in view and a few beyond, as `[first, last)`. */
export function visibleRange(s: ExplorerState): { first: number; last: number } {
  const count = rowCount(s);
  if (s.view === "details") {
    const first = Math.max(0, Math.floor(s.viewport.top / ROW_HEIGHT) - OVERSCAN);
    const last = Math.min(
      count,
      Math.ceil((s.viewport.top + s.viewport.height) / ROW_HEIGHT) + OVERSCAN,
    );
    return { first: Math.min(first, count), last: Math.max(last, Math.min(first, count)) };
  }
  const { rowsPerColumn, columns } = layoutOf(s, count);
  const firstColumn = Math.max(0, Math.floor(s.viewport.left / LIST_COLUMN_WIDTH) - 1);
  const lastColumn = Math.min(
    columns,
    Math.ceil((s.viewport.left + s.viewport.width) / LIST_COLUMN_WIDTH) + 1,
  );
  const first = Math.min(count, firstColumn * rowsPerColumn);
  return { first, last: Math.max(first, Math.min(count, lastColumn * rowsPerColumn)) };
}

/** Whether the row at `index` lies wholly in view. */
function inView(s: ExplorerState, index: number): boolean {
  if (index < 0) return false;
  const { left, top } = positionOf(s, index);
  if (s.view === "details") {
    return top >= s.viewport.top && top + ROW_HEIGHT <= s.viewport.top + s.viewport.height;
  }
  return (
    left >= s.viewport.left && left + LIST_COLUMN_WIDTH <= s.viewport.left + s.viewport.width
  );
}

/** The list moved so far that the row at `index` is in view. */
function brought(s: ExplorerState, index: number): ExplorerState {
  if (index < 0 || inView(s, index)) return s;
  const { left, top } = positionOf(s, index);
  const viewport = { ...s.viewport };
  if (s.view === "details") {
    viewport.top =
      top < viewport.top ? top : Math.max(0, top + ROW_HEIGHT - viewport.height);
  } else {
    viewport.left =
      left < viewport.left ? left : Math.max(0, left + LIST_COLUMN_WIDTH - viewport.width);
  }
  return { ...s, viewport, scrollSeq: s.scrollSeq + 1 };
}

/** Selects the row to reveal and brings it into view, once it is listed. */
function revealed(s: ExplorerState): ExplorerState {
  const key = s.reveal;
  if (key === undefined) return s;
  if (indexOfKey(s, key) < 0) {
    // Not listed, and nothing more is coming: there is nothing to reveal.
    return s.content.state === "loading" ? s : { ...s, reveal: undefined };
  }
  return selectOnly({ ...s, reveal: undefined }, key);
}

/** The list as the technician scrolled or resized it. */
export function withViewport(s: ExplorerState, viewport: Partial<Viewport>): ExplorerState {
  const next = { ...s.viewport, ...viewport };
  const same =
    next.top === s.viewport.top &&
    next.left === s.viewport.left &&
    next.height === s.viewport.height &&
    next.width === s.viewport.width;
  return same ? s : { ...s, viewport: next };
}

// --- Selection and the keyboard ---

function withFocus(s: ExplorerState, key: string): ExplorerState {
  return brought({ ...s, focus: key }, indexOfKey(s, key));
}

/** A click: this row alone. */
export function selectOnly(s: ExplorerState, key: string): ExplorerState {
  return withFocus({ ...s, selected: new Set([key]), anchor: key }, key);
}

/** A click with Ctrl: this row in or out of the selection. */
export function toggleSelected(s: ExplorerState, key: string): ExplorerState {
  const selected = new Set(s.selected);
  if (!selected.delete(key)) selected.add(key);
  return withFocus({ ...s, selected, anchor: key }, key);
}

/** A click with Shift: every row from where the selection started to this one. */
export function selectTo(s: ExplorerState, key: string): ExplorerState {
  const to = indexOfKey(s, key);
  if (to < 0) return s;
  const anchor = indexOfKey(s, s.anchor ?? s.focus);
  const from = anchor < 0 ? to : anchor;
  const selected = new Set<string>();
  for (let i = Math.min(from, to); i <= Math.max(from, to); i += 1) {
    const each = keyAt(s, i);
    if (each !== undefined) selected.add(each);
  }
  return withFocus({ ...s, selected, anchor: keyAt(s, from) }, key);
}

/** Ctrl+A: every row that is listed. */
export function selectAll(s: ExplorerState): ExplorerState {
  const selected = new Set<string>();
  for (let i = 0; i < rowCount(s); i += 1) {
    const key = keyAt(s, i);
    if (key !== undefined) selected.add(key);
  }
  return { ...s, selected };
}

export function clearSelection(s: ExplorerState): ExplorerState {
  return s.selected.size === 0 ? s : { ...s, selected: new Set(), anchor: undefined };
}

export type Move = "up" | "down" | "left" | "right" | "home" | "end" | "pageup" | "pagedown";

/** The index a key moves the focus to; from nowhere, to the first row. */
function moved(s: ExplorerState, move: Move): number {
  const count = rowCount(s);
  if (count === 0) return -1;
  const at = indexOfKey(s, s.focus);
  if (at < 0) return move === "end" || move === "up" ? count - 1 : 0;
  const { rowsPerColumn } = layoutOf(s, count);
  const page = Math.max(1, Math.floor(s.viewport.height / ROW_HEIGHT) - 1);
  // In the List view a column is a step to the side, and a page is the
  // columns in view.
  const columnsInView = Math.max(1, Math.floor(s.viewport.width / LIST_COLUMN_WIDTH));
  const step: Record<Move, number> =
    s.view === "details"
      ? { up: -1, down: 1, left: 0, right: 0, home: -count, end: count, pageup: -page, pagedown: page }
      : {
          up: -1,
          down: 1,
          left: -rowsPerColumn,
          right: rowsPerColumn,
          home: -count,
          end: count,
          pageup: -rowsPerColumn * columnsInView,
          pagedown: rowsPerColumn * columnsInView,
        };
  return Math.min(count - 1, Math.max(0, at + step[move]));
}

/**
 * An arrow or paging key. Alone it moves the selection with the focus; with
 * Shift it extends the selection from where it started; with Ctrl it moves
 * the focus and leaves the selection.
 */
export function moveFocus(
  s: ExplorerState,
  move: Move,
  mode: "select" | "extend" | "keep" = "select",
): ExplorerState {
  const key = keyAt(s, moved(s, move));
  if (key === undefined) return s;
  switch (mode) {
    case "select":
      return selectOnly(s, key);
    case "extend":
      return selectTo(s, key);
    case "keep":
      return withFocus(s, key);
  }
}

/** Typing letters goes to the first row whose name starts with them. */
export function selectByPrefix(s: ExplorerState, typed: string): ExplorerState {
  const wanted = typed.toLowerCase();
  if (wanted === "") return s;
  for (let i = 0; i < rowCount(s); i += 1) {
    const key = keyAt(s, i);
    if (key === undefined) continue;
    // A drive answers to its label and to its letter.
    const drive = s.place?.kind === "drives" ? s.drives[i] : undefined;
    const names = drive ? [driveName(drive), key] : [key];
    if (names.some((name) => name.toLowerCase().startsWith(wanted))) return selectOnly(s, key);
  }
  return s;
}

// --- View and sort ---

export function withView(s: ExplorerState, view: ExplorerView): ExplorerState {
  if (s.view === view) return s;
  const next: ExplorerState = {
    ...s,
    view,
    viewport: { ...s.viewport, top: 0, left: 0 },
    scrollSeq: s.scrollSeq + 1,
  };
  return brought(next, indexOfKey(next, next.focus));
}

/** A click on a column: sort by it, or turn the order around. */
export function withSort(s: ExplorerState, key: SortKey): ExplorerState {
  const sort: Sort =
    s.sort.key === key
      ? { key, descending: !s.sort.descending }
      : { key, descending: DOWNWARD_FIRST.includes(key) };
  const next: ExplorerState = {
    ...s,
    sort,
    entries: sortEntries(s.entries, sort),
    viewport: { ...s.viewport, top: 0, left: 0 },
    scrollSeq: s.scrollSeq + 1,
  };
  if (s.fresh) next.fresh = sortEntries(s.fresh, sort);
  // The row the technician was on stays in view; without one, the list
  // starts at its top.
  return brought(next, indexOfKey(next, next.focus));
}

// --- Going somewhere ---

function trimmed(places: Place[]): Place[] {
  return places.length > HISTORY_LIMIT ? places.slice(places.length - HISTORY_LIMIT) : places;
}

/** The technician asked for a place: it is pending until it is open. */
export function opening(s: ExplorerState, pending: Pending): ExplorerState {
  return { ...s, pending, menu: undefined, editing: undefined };
}

/** The history once the place of `pending` is open. */
function arrived(
  s: ExplorerState,
  place: Place,
  how: How,
): Pick<ExplorerState, "back" | "forward"> {
  const from = s.place;
  if (from === null || samePlace(from, place)) return { back: s.back, forward: s.forward };
  switch (how) {
    case "push":
      return { back: trimmed([...s.back, from]), forward: [] };
    case "back":
      return { back: s.back.slice(0, -1), forward: trimmed([...s.forward, from]) };
    case "forward":
      return { back: trimmed([...s.back, from]), forward: s.forward.slice(0, -1) };
    case "replace":
      return { back: s.back, forward: s.forward };
  }
}

/** The explorer in a new place, with nothing of the place before it. */
function movedTo(
  s: ExplorerState,
  place: Place,
  pending: Pending,
  reveal: string | undefined,
): ExplorerState {
  return {
    ...s,
    ...arrived(s, place, pending.how),
    place,
    drives: [],
    entries: [],
    fresh: undefined,
    listing: pending.listing,
    pending: undefined,
    refusal: undefined,
    selected: new Set(),
    anchor: undefined,
    focus: undefined,
    reveal,
    viewport: { ...s.viewport, top: 0, left: 0 },
    scrollSeq: s.scrollSeq + 1,
    menu: undefined,
  };
}

/** Only the rows that are still listed stay selected and in focus. */
function kept(s: ExplorerState): ExplorerState {
  const present = new Set<string>();
  for (let i = 0; i < rowCount(s); i += 1) {
    const key = keyAt(s, i);
    if (key !== undefined) present.add(key);
  }
  const selected = new Set<string>();
  for (const key of s.selected) if (present.has(key)) selected.add(key);
  return {
    ...s,
    selected,
    anchor: s.anchor !== undefined && present.has(s.anchor) ? s.anchor : undefined,
    focus: s.focus !== undefined && present.has(s.focus) ? s.focus : undefined,
  };
}

/** The drive list arrived: the explorer stands at the drives. */
export function applyDrives(
  s: ExplorerState,
  listing: number,
  drives: readonly ExplorerDrive[],
): ExplorerState {
  const pending = s.pending;
  if (pending?.listing !== listing) return s;
  if (samePlace(s.place, DRIVES)) {
    // A refresh: the same place, with what is selected there kept.
    return kept({
      ...s,
      drives: drives.slice(),
      content: { state: "complete" },
      listing,
      pending: undefined,
      refusal: undefined,
    });
  }
  const next = movedTo(s, DRIVES, pending, pending.reveal);
  return revealed({ ...next, drives: drives.slice(), content: { state: "complete" } });
}

/** A refresh is complete: the folder as it was listed again takes its place. */
function refreshed(s: ExplorerState, content: Content): ExplorerState {
  if (!s.fresh) return s;
  return kept({ ...s, entries: s.fresh, fresh: undefined, content });
}

/**
 * The row that must stay where it is when rows are sorted in above it: the
 * row in focus when it is in view, else the first row in view once the
 * technician has scrolled. At the very top of a list nobody has touched,
 * nothing is held: the list fills from its top.
 */
function heldRow(s: ExplorerState): number {
  if (s.view !== "details") return -1;
  const focus = indexOfKey(s, s.focus);
  if (inView(s, focus)) return focus;
  return s.viewport.top > 0 ? Math.floor(s.viewport.top / ROW_HEIGHT) : -1;
}

/** Sorts one page into what is listed, without moving what is in view. */
function withPage(s: ExplorerState, page: readonly ExplorerEntry[], end: boolean): ExplorerState {
  if (s.fresh) {
    const fresh = mergeEntries(s.fresh, page, s.sort).entries;
    const next = { ...s, fresh };
    return end ? refreshed(next, { state: "complete" }) : next;
  }
  const held = heldRow(s);
  const { entries, before } = mergeEntries(s.entries, page, s.sort);
  let viewport = s.viewport;
  let scrollSeq = s.scrollSeq;
  if (held >= 0) {
    const above = before.filter((index) => index <= held).length;
    if (above > 0) {
      viewport = { ...viewport, top: viewport.top + above * ROW_HEIGHT };
      scrollSeq += 1;
    }
  }
  // A page can arrive after the answer that said how the listing ended; it
  // is part of what was listed and changes nothing about that end.
  const content: Content = end ? { state: "complete" } : s.content;
  return revealed({ ...s, entries, viewport, scrollSeq, content });
}

/** One step of a listing arrived on the event. */
export function applyEvent(s: ExplorerState, event: ExplorerEvent): ExplorerState {
  if (event.kind === "page") {
    return event.listing === s.listing ? withPage(s, event.entries, event.end) : s;
  }
  const pending = s.pending;
  if (pending?.listing !== event.listing) return s;
  const place = folder(event.path);
  if (pending.how === "replace" && samePlace(s.place, place)) {
    // A refresh: what is on screen stays until the folder is listed again.
    const next: ExplorerState = {
      ...s,
      fresh: [],
      listing: event.listing,
      pending: undefined,
      refusal: undefined,
    };
    return withPage(next, event.entries, event.end);
  }
  const next = movedTo(s, place, pending, event.select ?? pending.reveal);
  return withPage({ ...next, content: { state: "loading" } }, event.entries, event.end);
}

/** The words for a listing that was left for a place that then did not open. */
export const LEFT = "Loading stopped when you opened another place.";

/** `explorer_open` answered: how the listing ended. */
export function applyListing(s: ExplorerState, report: ExplorerListing): ExplorerState {
  if (report.listing !== s.listing) return s;
  switch (report.state) {
    case "complete":
      // The last page says so itself; this covers an answer that overtook it.
      return s;
    case "limit":
      return s.fresh ? refreshed(s, { state: "limit" }) : { ...s, content: { state: "limit" } };
    case "cancelled":
      // A refresh that was left: its pages belong to no list any more.
      if (s.fresh) return { ...s, fresh: undefined, listing: undefined };
      return s.content.state === "loading"
        ? revealed({ ...s, content: { state: "interrupted", reason: LEFT } })
        : s;
  }
}

/** A command of the explorer failed: `stage` is the category, in a word. */
export interface Failure {
  stage: string;
  message: string;
}

/** The categories that mean the session is over, each in words. */
const ENDED: Record<string, string> = {
  no_consented_session: "The session has ended. Browsing has stopped.",
  disconnected: "The connection to the customer computer was lost. Browsing has stopped.",
};

function failureWords(failure: Failure): string {
  return `${failure.message} (${failure.stage})`;
}

/** What a place is called in a sentence. */
export function placeWords(place: Place | null): string {
  return place?.kind === "folder" ? place.path : "the drives";
}

/**
 * A command of the listing `listing` failed. A place that never opened is a
 * refusal, and the explorer stays where it was; a listing that had begun is
 * cut short and says so; the end of the session is said as that.
 */
export function applyFailure(s: ExplorerState, listing: number, failure: Failure): ExplorerState {
  const ended = ENDED[failure.stage];
  if (s.pending?.listing === listing) {
    const wanted = s.pending.place;
    const next: ExplorerState = { ...s, pending: undefined };
    if (ended !== undefined) return { ...next, ended };
    const stays =
      s.place === null
        ? "Nothing is open. Try again, or open another place."
        : `You are still in ${placeWords(s.place)}.`;
    return {
      ...next,
      refusal: {
        title:
          wanted.kind === "drives"
            ? "The drives could not be listed"
            : `${wanted.path} could not be opened`,
        message: failureWords(failure),
        next:
          wanted.kind === "drives"
            ? `${stays} Press Refresh to try again.`
            : `${stays} Check the path, or open the folder step by step from the drives.`,
      },
    };
  }
  if (s.listing !== listing) return s;
  if (s.fresh) {
    // A refresh that failed: what is on screen is the listing from before,
    // and the pages of this one belong to no list any more.
    const next: ExplorerState = { ...s, fresh: undefined, listing: undefined };
    if (ended !== undefined) return { ...next, ended };
    return {
      ...next,
      refusal: {
        title: `${placeWords(s.place)} could not be listed again`,
        message: failureWords(failure),
        next: "What you see is the list from before. Press Refresh to try again.",
      },
    };
  }
  if (s.content.state !== "loading") return s;
  const next: ExplorerState = revealed({
    ...s,
    content: { state: "interrupted", reason: ended ?? failureWords(failure) },
  });
  return ended === undefined ? next : { ...next, ended };
}

// --- The context menu ---

/** One thing the explorer can do with the selection or with the place. */
export interface MenuItem {
  // The action it dispatches.
  action: string;
  label: string;
  // The key that does the same, as `aria-keyshortcuts` names it.
  keys?: string;
  disabled: boolean;
}

/**
 * What the context menu offers, in order. The menu is this list and nothing
 * else: an action that is added here appears in it, with its key.
 */
export function menuItems(s: ExplorerState): MenuItem[] {
  const keys = s.menu?.keys ?? [];
  const one = keys.length === 1 ? keys[0] : undefined;
  return [
    {
      action: "xp-menu-open",
      label: "Open",
      keys: "Enter",
      disabled: openTarget(s, one) === null,
    },
    { action: "xp-refresh", label: "Refresh", keys: "F5", disabled: s.place === null },
  ];
}

// --- The status line ---

/** What is listed and how complete it is, in words. */
export function statusWords(s: ExplorerState): string {
  if (s.place === null) return s.pending ? "Opening…" : "";
  const count = rowCount(s);
  const parts: string[] = [];
  if (s.place.kind === "drives") {
    parts.push(plural(count, "drive"));
  } else {
    switch (s.content.state) {
      case "loading":
        parts.push(`${plural(count, "item")} so far`, "loading more…");
        break;
      case "complete":
        parts.push(plural(count, "item"));
        break;
      case "limit":
        parts.push(`The first ${plural(count, "item")}`, "this folder holds more than are listed");
        break;
      case "interrupted":
        parts.push(`${plural(count, "item")} listed`, "the list is not complete");
        break;
    }
  }
  if (s.selected.size > 0) parts.push(`${s.selected.size} selected`);
  if (s.fresh) parts.push("refreshing…");
  if (s.pending) parts.push(`opening ${placeWords(s.pending.place)}…`);
  return parts.join(" · ");
}

// --- What the backend sends ---

const DRIVE_KIND_NAMES: readonly string[] = Object.keys(DRIVE_KINDS);
const ENTRY_KINDS: readonly string[] = ["file", "dir", "symlink", "other"];

function optionalNumber(value: unknown): boolean {
  return value === undefined || (typeof value === "number" && Number.isFinite(value));
}

/** Whether a value is an `ExplorerDrive` of the contract; anything else is dropped. */
export function isExplorerDrive(value: unknown): value is ExplorerDrive {
  if (typeof value !== "object" || value === null) return false;
  const drive = value as Record<string, unknown>;
  return (
    typeof drive.root === "string" &&
    drive.root !== "" &&
    DRIVE_KIND_NAMES.includes(drive.kind as string) &&
    (drive.label === undefined || typeof drive.label === "string") &&
    optionalNumber(drive.totalBytes) &&
    optionalNumber(drive.freeBytes)
  );
}

/** Whether a value is an `ExplorerEntry` of the contract. */
export function isExplorerEntry(value: unknown): value is ExplorerEntry {
  if (typeof value !== "object" || value === null) return false;
  const entry = value as Record<string, unknown>;
  return (
    typeof entry.name === "string" &&
    entry.name !== "" &&
    ENTRY_KINDS.includes(entry.kind as string) &&
    typeof entry.len === "number" &&
    Number.isFinite(entry.len) &&
    optionalNumber(entry.modifiedMs) &&
    typeof entry.readonly === "boolean"
  );
}

/**
 * Whether a value is an `ExplorerEvent` of the contract. An event with an
 * entry that is none is not taken at all: a page with a hole in it would make
 * a folder look complete that is not.
 */
export function isExplorerEvent(value: unknown): value is ExplorerEvent {
  if (typeof value !== "object" || value === null) return false;
  const event = value as Record<string, unknown>;
  const page =
    typeof event.listing === "number" &&
    typeof event.end === "boolean" &&
    Array.isArray(event.entries) &&
    event.entries.every(isExplorerEntry);
  if (!page) return false;
  if (event.kind === "page") return true;
  return (
    event.kind === "opened" &&
    typeof event.path === "string" &&
    event.path !== "" &&
    (event.select === undefined || typeof event.select === "string")
  );
}

/** Whether a value is an `ExplorerListing` of the contract. */
export function isExplorerListing(value: unknown): value is ExplorerListing {
  if (typeof value !== "object" || value === null) return false;
  const listing = value as Record<string, unknown>;
  return (
    typeof listing.listing === "number" &&
    typeof listing.entries === "number" &&
    (listing.path === undefined || typeof listing.path === "string") &&
    (listing.state === "complete" || listing.state === "cancelled" || listing.state === "limit")
  );
}
