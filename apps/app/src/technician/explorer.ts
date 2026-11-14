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
//
// The second part of the explorer changes things: it sends files, copies
// them to this computer, renames and deletes. Its state is here too:
//
// - **The transfer queue** (`transfers`) is what the backend says it is: an
//   entry is replaced by its number each time the backend tells its state.
//   `done` is the backend's word for a transfer that was verified; nothing
//   here turns another state into it.
// - **A question before a change** (`question`): a name that is taken is
//   decided by the technician, one conflict after the other, and a delete is
//   confirmed once, with the number of items and the sentence that there is
//   no recycle bin.
// - **What could not be done** is a `Refusal` like a place that did not open,
//   with the entries it is about and the reason of each.

import type {
  ChosenFolder,
  ConflictChoice,
  DeleteReport,
  ExplorerDrive,
  ExplorerDriveKind,
  ExplorerEntry,
  ExplorerEvent,
  ExplorerListing,
  NameConflict,
  RemovalProgress,
  TransferCheck,
  TransferEntry,
  TransferQueued,
  TransferState,
  TransfersEvent,
  UploadOffer,
} from "../shared/contract";
import { formatBytes, plural } from "../shared/feed";
import type { DeleteItem, TransferName } from "./api";

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

/**
 * A place that could not be opened, or a change that could not be made, in
 * the application's words.
 */
export interface Refusal {
  title: string;
  message: string;
  next: string;
  // The entries it is about, each with its reason, when there are any.
  details?: Array<{ path: string; reason: string }>;
  // How many more there are than `details` names.
  more?: number;
}

/** The context menu: where it opened, and for which rows (none: the place). */
export interface Menu {
  x: number;
  y: number;
  keys: string[];
  // The menu of the Upload button: only the two ways to choose what to send.
  only?: "upload";
}

/** A name that is taken: asked about one conflict after the other. */
export interface ConflictQuestion {
  kind: "conflicts";
  direction: "send" | "download";
  // The open folder on the customer computer.
  folder: string;
  // The folder the items go to, as text.
  where: string;
  // A send: the offer that was dropped or picked.
  offer?: number;
  // A copy: the selection, and the folder that was chosen for it, if one was.
  names?: TransferName[];
  to?: number;
  conflicts: NameConflict[];
  // The conflict that is asked about now; past the last one, all are decided.
  at: number;
  choices: Record<string, ConflictChoice>;
  // The next choice holds for every conflict that follows.
  all: boolean;
}

/** A delete, to be confirmed once for the whole selection. */
export interface DeleteQuestion {
  kind: "delete";
  folder: string;
  items: DeleteItem[];
}

export type Question = ConflictQuestion | DeleteQuestion;

/** A delete that runs. */
export interface Removal {
  // The number the window gave it, to follow and to cancel it.
  id: number;
  items: number;
  // The item that is being removed, and its place among the items.
  item?: string;
  at?: number;
  removedFiles: number;
  removedDirs: number;
  // The technician asked to cancel it.
  cancelling?: boolean;
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
  // Why the last thing the technician tried did nothing, in words: a file
  // does not open here. Gone with the next step.
  hint?: string;
  // The transfers of this session, in the order of the queue (ST-M010-002).
  transfers: TransferEntry[];
  // What the explorer asks before it sends, copies or deletes.
  question?: Question;
  // What the explorer is finding out before it can ask or queue, in words.
  preparing?: string;
  // The row whose name is being typed anew.
  renaming?: string;
  // The delete that runs.
  removal?: Removal;
  // Files are being dragged over the window.
  dropping?: boolean;
  // What a change could not do, or did only in part: shown until it is
  // dismissed or the next change begins. A place that opens leaves it there.
  problem?: Refusal;
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
    transfers: [],
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
  return brought({ ...s, focus: key, hint: undefined }, indexOfKey(s, key));
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
  if (s.selected.size === 0 && s.hint === undefined) return s;
  return { ...s, selected: new Set(), anchor: undefined, hint: undefined };
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
  return {
    ...s,
    pending,
    menu: undefined,
    editing: undefined,
    hint: undefined,
    renaming: undefined,
  };
}

/**
 * Why a row does not open, in words: a folder and a drive open, a file does
 * not, and a link is shown and never followed. Nothing for a row that opens.
 */
export function notOpened(s: ExplorerState, key: string | undefined): string | undefined {
  if (key === undefined || openTarget(s, key) !== null) return undefined;
  const entry = entryOf(s, key);
  if (entry === undefined) return undefined;
  return entry.kind === "symlink"
    ? `${entry.name} is a link. A link is shown here, not followed.`
    : `${entry.name} is a file. Folders open here, a file does not.`;
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
  // A row to reveal is selected once the folder is listed again: the new
  // name of an entry that was renamed.
  return revealed(kept({ ...s, entries: s.fresh, fresh: undefined, content }));
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

/** What a failure says about the session: that it is over, or nothing. */
export function endedWords(stage: string): string | undefined {
  return ENDED[stage];
}

/** A failure as the explorer shows it: its words, and its category. */
export function failureWords(failure: Failure): string {
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
  // A line separates it from the action before it.
  separated?: boolean;
}

/**
 * Whether the explorer can change something where it stands: in a folder of
 * a session that has not ended. The drives hold no entry to change.
 */
export function canChange(s: ExplorerState): boolean {
  return s.place?.kind === "folder" && s.ended === undefined;
}

/**
 * What the explorer can do with the rows `keys` — the rows of the menu, or
 * the selection for the buttons and the keys — in order. The context menu,
 * the buttons above the list and the keys are all this one list: an action
 * that is added here appears in the menu and as a button, with its key.
 */
export function actionsFor(s: ExplorerState, keys: readonly string[]): MenuItem[] {
  const one = keys.length === 1 ? keys[0] : undefined;
  const change = canChange(s);
  const some = change && keys.length > 0;
  // One delete at a time: what it removes is not renamed or deleted meanwhile.
  const free = s.removal === undefined;
  return [
    {
      action: "xp-menu-open",
      label: "Open",
      keys: "Enter",
      disabled: openTarget(s, one) === null,
    },
    { action: "xp-download", label: "Download", disabled: !some, separated: true },
    { action: "xp-download-to", label: "Download to…", disabled: !some },
    {
      action: "xp-rename",
      label: "Rename",
      keys: "F2",
      disabled: !(change && free && one !== undefined),
    },
    { action: "xp-delete", label: "Delete", keys: "Delete", disabled: !(some && free) },
    { action: "xp-upload-files", label: "Upload files…", disabled: !change, separated: true },
    { action: "xp-upload-folder", label: "Upload a folder…", disabled: !change },
    {
      action: "xp-refresh",
      label: "Refresh",
      keys: "F5",
      disabled: s.place === null,
      separated: true,
    },
  ];
}

/**
 * What the open menu offers, in order. The menu is this list and nothing
 * else. The menu of the Upload button offers the two ways to choose what to
 * send.
 */
export function menuItems(s: ExplorerState): MenuItem[] {
  if (s.menu?.only === "upload") {
    const disabled = !canChange(s);
    return [
      { action: "xp-upload-files", label: "Files…", disabled },
      { action: "xp-upload-folder", label: "A folder…", disabled },
    ];
  }
  return actionsFor(s, s.menu?.keys ?? []);
}

/** The selected rows, in the order of the list. */
export function selectedKeys(s: ExplorerState): string[] {
  if (s.selected.size === 0) return [];
  const keys: string[] = [];
  for (let i = 0; i < rowCount(s); i += 1) {
    const key = keyAt(s, i);
    if (key !== undefined && s.selected.has(key)) keys.push(key);
  }
  return keys;
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
  if (s.preparing !== undefined) parts.push(s.preparing);
  if (s.hint !== undefined) parts.push(s.hint);
  return parts.join(" · ");
}

// --- The transfer queue (ST-M010-002) ---

/** Whether a transfer waits or runs. */
export function isOpen(entry: TransferEntry): boolean {
  return entry.state === "waiting" || entry.state === "running";
}

/**
 * Takes entries the backend told into the queue: an entry with a number the
 * queue has replaces it, another one is added at its place in the order of
 * the numbers, and a folder that says its files are queued is dropped.
 */
export function applyTransfers(
  s: ExplorerState,
  entries: readonly TransferEntry[],
): ExplorerState {
  if (entries.length === 0) return s;
  const queue = s.transfers.slice();
  for (const entry of entries) {
    // The queue is kept by number: the place of this one is searched.
    let low = 0;
    let high = queue.length;
    while (low < high) {
      const middle = (low + high) >> 1;
      if ((queue[middle] as TransferEntry).id < entry.id) low = middle + 1;
      else high = middle;
    }
    const known = queue[low]?.id === entry.id;
    if (entry.state === "listed") {
      if (known) queue.splice(low, 1);
    } else if (known) {
      queue[low] = entry;
    } else {
      queue.splice(low, 0, entry);
    }
  }
  return { ...s, transfers: queue };
}

/** How many transfers are in each state. */
export type TransferCounts = Record<Exclude<TransferState, "listed">, number>;

export function transferCounts(entries: readonly TransferEntry[]): TransferCounts {
  const counts: TransferCounts = {
    waiting: 0,
    running: 0,
    done: 0,
    failed: 0,
    cancelled: 0,
    left_out: 0,
    stopped: 0,
  };
  for (const entry of entries) if (entry.state !== "listed") counts[entry.state] += 1;
  return counts;
}

/** The most rows of each part of the queue that are drawn; the rest are counted. */
export const QUEUE_ROWS = { attention: 100, waiting: 20, finished: 50 } as const;

/** The queue as the transfers panel shows it. */
export interface QueueRows {
  // What did not arrive, the newest first: failed, left out, stopped.
  attention: TransferEntry[];
  attentionMore: number;
  running: TransferEntry[];
  // In the order they will run.
  waiting: TransferEntry[];
  waitingMore: number;
  // What ended otherwise, the newest first: done, cancelled.
  finished: TransferEntry[];
  finishedMore: number;
}

/**
 * The rows of the queue in the order they are shown: what did not arrive
 * first, then what runs and what waits, then what ended. Each part is
 * bounded, so a folder of thousands of files costs what a screenful costs.
 */
export function queueRows(entries: readonly TransferEntry[]): QueueRows {
  const rows: QueueRows = {
    attention: [],
    attentionMore: 0,
    running: [],
    waiting: [],
    waitingMore: 0,
    finished: [],
    finishedMore: 0,
  };
  for (const entry of entries) {
    if (entry.state === "running") rows.running.push(entry);
    else if (entry.state === "waiting") {
      if (rows.waiting.length < QUEUE_ROWS.waiting) rows.waiting.push(entry);
      else rows.waitingMore += 1;
    }
  }
  for (let i = entries.length - 1; i >= 0; i -= 1) {
    const entry = entries[i] as TransferEntry;
    if (entry.state === "failed" || entry.state === "left_out" || entry.state === "stopped") {
      if (rows.attention.length < QUEUE_ROWS.attention) rows.attention.push(entry);
      else rows.attentionMore += 1;
    } else if (entry.state === "done" || entry.state === "cancelled") {
      if (rows.finished.length < QUEUE_ROWS.finished) rows.finished.push(entry);
      else rows.finishedMore += 1;
    }
  }
  return rows;
}

/** How far a transfer is, in percent, when its length is known. */
export function transferPercent(entry: TransferEntry): number | undefined {
  if (entry.bytes === undefined) return undefined;
  if (entry.bytes === 0) return entry.state === "done" ? 100 : 0;
  return Math.min(100, Math.floor((entry.done / entry.bytes) * 100));
}

/** The state of a transfer in words. Only a verified transfer reads as sent or copied. */
export function transferWords(entry: TransferEntry): string {
  const send = entry.direction === "send";
  const folder = entry.kind === "folder";
  switch (entry.state) {
    case "waiting":
      return "Waiting";
    case "running": {
      if (folder) return send ? "Making the folder…" : "Listing the folder…";
      const percent = transferPercent(entry);
      return percent === undefined
        ? send
          ? "Sending…"
          : "Copying…"
        : `${percent}% · ${formatBytes(entry.done)} of ${formatBytes(entry.bytes)}`;
    }
    case "done":
      if (folder) return send ? "Made" : "Copied · an empty folder";
      return `${send ? "Sent" : "Copied"} · ${formatBytes(entry.bytes ?? entry.done)}`;
    case "failed":
      return folder ? "Not complete" : send ? "Not sent" : "Not copied";
    case "cancelled":
      return "Cancelled";
    case "left_out":
      return "Left out";
    case "stopped":
      return "Stopped with the session";
    case "listed":
      return "";
  }
}

/** Which way a transfer goes, in the words the customer's side knows it by. */
export function directionWords(entry: TransferEntry): string {
  return entry.direction === "send"
    ? "To the customer computer"
    : "To this computer · leaves the customer computer";
}

/** The queue in one line: what runs, waits, did not arrive and is done. */
export function queueWords(entries: readonly TransferEntry[]): string {
  const counts = transferCounts(entries);
  const parts: string[] = [];
  if (counts.running > 0) parts.push(`${counts.running} running`);
  if (counts.waiting > 0) parts.push(`${counts.waiting} waiting`);
  if (counts.failed > 0) parts.push(`${counts.failed} failed`);
  if (counts.stopped > 0) parts.push(`${counts.stopped} stopped`);
  if (counts.left_out > 0) parts.push(`${counts.left_out} left out`);
  if (counts.cancelled > 0) parts.push(`${counts.cancelled} cancelled`);
  if (counts.done > 0) parts.push(`${counts.done} done`);
  return parts.join(" · ");
}

/**
 * What to say when the queue comes to rest: how many arrived and how many
 * did not. Nothing while a transfer still waits or runs.
 */
export function queueRestWords(
  before: readonly TransferEntry[],
  after: readonly TransferEntry[],
): string | undefined {
  if (!before.some(isOpen) || after.some(isOpen)) return undefined;
  const counts = transferCounts(after);
  const not = counts.failed + counts.left_out + counts.stopped;
  const parts = [`Transfers ended: ${counts.done} done`];
  if (not > 0) parts.push(`${not} not completed`);
  if (counts.cancelled > 0) parts.push(`${counts.cancelled} cancelled`);
  return `${parts.join(", ")}.`;
}

/** How a send or a copy was queued, in a sentence. */
export function queuedWords(direction: "send" | "download", queued: TransferQueued): string {
  const parts = [
    direction === "send"
      ? `${plural(queued.queued, "item")} queued to send`
      : `${plural(queued.queued, "item")} queued to copy to this computer`,
  ];
  if (queued.skipped > 0) parts.push(`${queued.skipped} skipped`);
  if (queued.leftOut > 0) parts.push(`${queued.leftOut} left out, see the transfers`);
  return `${parts.join(", ")}.`;
}

/**
 * What the window says when full control ends while transfers were not
 * complete: which ones, and that nothing more is sent or copied.
 */
export function stoppedTransfers(entries: readonly TransferEntry[]): Refusal | undefined {
  const cut = entries.filter((entry) => isOpen(entry) || entry.state === "stopped");
  if (cut.length === 0) return undefined;
  const names = cut.slice(0, 3).map((entry) => entry.name);
  const more = cut.length - names.length;
  return {
    title:
      cut.length === 1
        ? "1 transfer stopped with the session"
        : `${cut.length} transfers stopped with the session`,
    message: `Not complete: ${names.join(", ")}${more > 0 ? ` and ${more} more` : ""}.`,
    next: "Nothing more is sent or copied, and no unfinished file was kept under its name. Start a new session to send or copy them again.",
  };
}

// --- A name that is taken ---

/** The question for the names `check` found taken; none when no name is. */
export function conflictQuestion(
  check: TransferCheck,
  from: Pick<ConflictQuestion, "direction" | "folder" | "offer" | "names" | "to">,
): ConflictQuestion | undefined {
  if (check.conflicts.length === 0) return undefined;
  return {
    kind: "conflicts",
    ...from,
    where: check.folder,
    conflicts: check.conflicts,
    at: 0,
    choices: {},
    all: false,
  };
}

/**
 * The technician chose for the conflict that is asked about. With `all` the
 * choice also holds for the conflicts that follow, as far as it can: a name
 * that cannot be replaced is still asked about.
 */
export function answerConflict(q: ConflictQuestion, choice: ConflictChoice): ConflictQuestion {
  const current = q.conflicts[q.at];
  if (!current || (choice === "replace" && !current.replace)) return q;
  const choices = { ...q.choices, [current.name]: choice };
  let at = q.at + 1;
  if (q.all) {
    for (; at < q.conflicts.length; at += 1) {
      const next = q.conflicts[at] as NameConflict;
      if (choice === "replace" && !next.replace) break;
      choices[next.name] = choice;
    }
  }
  return { ...q, choices, at };
}

/** Whether every conflict of the question is decided. */
export function conflictsDecided(q: ConflictQuestion): boolean {
  return q.at >= q.conflicts.length;
}

/** What lies under a name, in a word. */
export function kindWord(kind: ExplorerEntry["kind"] | "folder"): string {
  switch (kind) {
    case "dir":
    case "folder":
      return "folder";
    case "file":
      return "file";
    case "symlink":
      return "link";
    case "other":
      return "entry";
  }
}

// --- Rename ---

/** The characters no name on Windows may hold. */
const NAME_FORBIDDEN = /[\\/:*?"<>|\u0000-\u001f]/;

/**
 * Why `to` cannot be the new name of the row `key`, in a sentence; nothing
 * when it can. The customer computer has the last word: this only saves a
 * request for what it would refuse.
 */
export function renameProblem(s: ExplorerState, key: string, to: string): string | undefined {
  if (to.trim() === "") return "A name cannot be empty.";
  if (NAME_FORBIDDEN.test(to)) return 'A name cannot hold any of \\ / : * ? " < > |.';
  if (to === "." || to === "..") return "This name stands for a folder, not for an entry.";
  if (/[. ]$/.test(to)) return "A name cannot end in a dot or a space.";
  const wanted = to.toLowerCase();
  const taken = s.entries.some(
    (entry) => entry.name !== key && entry.name.toLowerCase() === wanted,
  );
  return taken ? `${to} is already in this folder.` : undefined;
}

/**
 * The part of a name that is selected when its field opens: the whole name
 * of a folder, a file's name without its extension.
 */
export function renameSelection(entry: ExplorerEntry): [number, number] {
  const dot = entry.name.lastIndexOf(".");
  return entry.kind === "file" && dot > 0 ? [0, dot] : [0, entry.name.length];
}

// --- Delete ---

/** The entries `keys` name, as a delete takes them. */
export function deleteItems(s: ExplorerState, keys: readonly string[]): DeleteItem[] {
  const items: DeleteItem[] = [];
  for (const key of keys) {
    const entry = entryOf(s, key);
    if (entry) items.push({ name: entry.name, kind: entry.kind });
  }
  return items;
}

/** The entries `keys` name, as a copy to this computer takes them. */
export function transferNames(s: ExplorerState, keys: readonly string[]): TransferName[] {
  return deleteItems(s, keys).map((item) => ({
    name: item.name,
    kind: item.kind === "dir" ? "folder" : "file",
  }));
}

/** The words of the question before a delete. */
export function deleteWords(q: DeleteQuestion): { title: string; lines: string[] } {
  const count = q.items.length;
  const first = q.items[0];
  const folders = q.items.filter((item) => item.kind === "dir").length;
  const names = q.items.slice(0, 5).map((item) => item.name);
  const more = count - names.length;
  const lines: string[] = [];
  if (count > 1) lines.push(`${names.join(", ")}${more > 0 ? ` and ${more} more` : ""}.`);
  if (folders > 0) {
    lines.push(
      folders === 1 && count === 1
        ? "The folder is deleted with everything in it."
        : `${plural(folders, "folder")} ${folders === 1 ? "is" : "are"} deleted with everything in ${folders === 1 ? "it" : "them"}.`,
    );
  }
  lines.push(
    `There is no recycle bin on this path: what is deleted from ${q.folder} is gone for good.`,
  );
  return {
    title: count === 1 && first ? `Delete ${first.name}?` : `Delete ${count} items?`,
    lines,
  };
}

/** A delete that began: nothing removed yet. */
export function removalOf(id: number, items: number): Removal {
  return { id, items, removedFiles: 0, removedDirs: 0 };
}

/** Where the delete `removal` stands, from one step the backend told. */
export function applyRemoval(s: ExplorerState, step: RemovalProgress): ExplorerState {
  if (!s.removal || s.removal.id !== step.removal) return s;
  return {
    ...s,
    removal: {
      ...s.removal,
      item: step.item,
      at: step.at,
      removedFiles: step.removedFiles,
      removedDirs: step.removedDirs,
    },
  };
}

/** A delete that runs, in words. */
export function removalWords(removal: Removal): string {
  const parts = [
    removal.cancelling
      ? "Cancelling the delete…"
      : removal.at !== undefined && removal.item !== undefined
        ? `Deleting item ${removal.at} of ${removal.items}: ${removal.item}…`
        : `Deleting ${plural(removal.items, "item")}…`,
  ];
  const removed = removal.removedFiles + removal.removedDirs;
  if (removed > 0) {
    parts.push(
      `${plural(removal.removedFiles, "file")} and ${plural(removal.removedDirs, "folder")} removed so far.`,
    );
  }
  return parts.join(" ");
}

/**
 * What a delete came to. A delete that removed everything is said in a
 * sentence; anything else is a refusal that names what could not be removed,
 * with the reason of each, and never says that it was.
 */
export function deleteOutcome(report: DeleteReport): { notice: string; refusal?: Refusal } {
  const total = report.removedFiles + report.removedDirs;
  const inAll =
    report.removedDirs > 0
      ? ` (${plural(report.removedFiles, "file")} and ${plural(report.removedDirs, "folder")} in all)`
      : "";
  const whole =
    report.removed === report.selected &&
    !report.cancelled &&
    report.ended === undefined &&
    report.failed.length === 0;
  if (whole) return { notice: `${plural(report.removed, "item")} deleted${inAll}.` };
  const left = report.selected - report.removed;
  const title =
    report.ended !== undefined
      ? "The delete stopped with the session"
      : report.cancelled
        ? "The delete was cancelled"
        : `${left} of ${plural(report.selected, "item")} could not be deleted`;
  const parts = [
    `${report.removed} of ${plural(report.selected, "item")} ${report.removed === 1 ? "was" : "were"} deleted${total > 0 ? inAll : ""}.`,
  ];
  if (report.notTried > 0) {
    parts.push(`${plural(report.notTried, "item")} ${report.notTried === 1 ? "was" : "were"} not tried.`);
  }
  const refusal: Refusal = {
    title,
    message: parts.join(" "),
    next:
      report.ended ??
      "The list shows what is there now. Close what holds a file open, then delete again.",
  };
  if (report.failed.length > 0) refusal.details = report.failed;
  if (report.failedMore > 0) refusal.more = report.failedMore;
  return { notice: `${title}.`, refusal };
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

const TRANSFER_STATES: readonly string[] = [
  "waiting",
  "running",
  "done",
  "failed",
  "cancelled",
  "left_out",
  "stopped",
  "listed",
];

/** Whether a value is a `TransferEntry` of the contract. */
export function isTransferEntry(value: unknown): value is TransferEntry {
  if (typeof value !== "object" || value === null) return false;
  const entry = value as Record<string, unknown>;
  return (
    typeof entry.id === "number" &&
    Number.isFinite(entry.id) &&
    (entry.direction === "send" || entry.direction === "download") &&
    (entry.kind === "file" || entry.kind === "folder") &&
    typeof entry.name === "string" &&
    typeof entry.remote === "string" &&
    typeof entry.local === "string" &&
    optionalNumber(entry.bytes) &&
    typeof entry.done === "number" &&
    Number.isFinite(entry.done) &&
    TRANSFER_STATES.includes(entry.state as string) &&
    (entry.reason === undefined || typeof entry.reason === "string")
  );
}

/**
 * Whether a value is a `TransfersEvent` of the contract. An event with an
 * entry that is none is not taken at all.
 */
export function isTransfersEvent(value: unknown): value is TransfersEvent {
  if (typeof value !== "object" || value === null) return false;
  const event = value as Record<string, unknown>;
  return Array.isArray(event.entries) && event.entries.every(isTransferEntry);
}

/** Whether a value is an `UploadOffer` of the contract. */
export function isUploadOffer(value: unknown): value is UploadOffer {
  if (typeof value !== "object" || value === null) return false;
  const offer = value as Record<string, unknown>;
  return (
    typeof offer.offer === "number" &&
    typeof offer.items === "number" &&
    Array.isArray(offer.names) &&
    offer.names.every((name) => typeof name === "string")
  );
}

/** Whether a value is a `TransferCheck` of the contract. */
export function isTransferCheck(value: unknown): value is TransferCheck {
  if (typeof value !== "object" || value === null) return false;
  const check = value as Record<string, unknown>;
  return (
    typeof check.folder === "string" &&
    Array.isArray(check.conflicts) &&
    check.conflicts.every((conflict: unknown) => {
      if (typeof conflict !== "object" || conflict === null) return false;
      const c = conflict as Record<string, unknown>;
      return (
        typeof c.name === "string" &&
        (c.kind === "file" || c.kind === "folder") &&
        ENTRY_KINDS.includes(c.existing as string) &&
        typeof c.replace === "boolean"
      );
    })
  );
}

/** Whether a value is a `TransferQueued` of the contract. */
export function isTransferQueued(value: unknown): value is TransferQueued {
  if (typeof value !== "object" || value === null) return false;
  const queued = value as Record<string, unknown>;
  return (
    typeof queued.queued === "number" &&
    typeof queued.leftOut === "number" &&
    typeof queued.skipped === "number"
  );
}

/** Whether a value is a `ChosenFolder` of the contract. */
export function isChosenFolder(value: unknown): value is ChosenFolder {
  if (typeof value !== "object" || value === null) return false;
  const chosen = value as Record<string, unknown>;
  return typeof chosen.folder === "number" && typeof chosen.path === "string";
}

/** Whether a value is a `RemovalProgress` of the contract. */
export function isRemovalProgress(value: unknown): value is RemovalProgress {
  if (typeof value !== "object" || value === null) return false;
  const step = value as Record<string, unknown>;
  return (
    typeof step.removal === "number" &&
    typeof step.item === "string" &&
    typeof step.at === "number" &&
    typeof step.of === "number" &&
    typeof step.removedFiles === "number" &&
    typeof step.removedDirs === "number"
  );
}

/** Whether a value is a `DeleteReport` of the contract. */
export function isDeleteReport(value: unknown): value is DeleteReport {
  if (typeof value !== "object" || value === null) return false;
  const report = value as Record<string, unknown>;
  return (
    typeof report.selected === "number" &&
    typeof report.removed === "number" &&
    typeof report.removedFiles === "number" &&
    typeof report.removedDirs === "number" &&
    typeof report.failedMore === "number" &&
    typeof report.cancelled === "boolean" &&
    typeof report.notTried === "number" &&
    (report.ended === undefined || typeof report.ended === "string") &&
    Array.isArray(report.failed) &&
    report.failed.every((left: unknown) => {
      if (typeof left !== "object" || left === null) return false;
      const l = left as Record<string, unknown>;
      return typeof l.path === "string" && typeof l.reason === "string";
    })
  );
}
