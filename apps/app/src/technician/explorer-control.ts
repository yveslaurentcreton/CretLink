// The controller of the file explorer (ST-M010-002).
//
// It turns what the technician does in the Files tab into the two commands of
// the backend, and what the backend answers into the explorer's state
// (`./explorer`). The window's controller (`./main`) owns the snapshot and
// the drawing; this module is told where to read and write the explorer's
// part of it (`ExplorerHost`).
//
// - **Going somewhere** asks the backend for the drives or for a folder under
//   a number of its own. The explorer stays where it is until the place is
//   open; what arrives under another number than the one waited for belongs
//   to a place that was left and is ignored.
// - **The list** takes clicks, double clicks, the context menu and keys on
//   the grid itself: rows are drawn and dropped as the list scrolls, so they
//   carry no handlers of their own.
// - **Scrolling** redraws the rows and nothing else. Nothing here waits for a
//   frame or for time: a change is drawn in the same turn, or with the next
//   drawing of the window.

import type { ActionEvent } from "../shared/dom";
import { plural } from "../shared/feed";
import { api, isCommandError } from "./api";
import {
  DRIVES,
  type ExplorerState,
  type ExplorerView,
  type How,
  type Move,
  type Place,
  type SortKey,
  applyDrives,
  applyEvent,
  applyFailure,
  applyListing,
  clearSelection,
  crumbs,
  entryOf,
  folder,
  indexOfKey,
  isExplorerDrive,
  isExplorerEvent,
  isExplorerListing,
  menuItems,
  moveFocus,
  openTarget,
  opening,
  parentPlace,
  placeText,
  positionOf,
  rowCount,
  samePlace,
  selectAll,
  selectByPrefix,
  selectOnly,
  selectTo,
  toggleSelected,
  visibleRange,
  withSort,
  withView,
  withViewport,
} from "./explorer";

/** How much of the window a change of the explorer needs drawn again. */
export type Draw = "window" | "panel" | "rows" | "none";

/** What the explorer's controller needs from the window's controller. */
export interface ExplorerHost {
  /** The explorer's state now. */
  get(): ExplorerState;
  /** Takes the next state and draws it. */
  set(next: ExplorerState, draw: Draw): void;
  /** Whether the Files tab of a live session is on screen. */
  shown(): boolean;
  /** The element the window is drawn in. */
  root(): HTMLElement | null;
  /** Says something politely to a screen reader. */
  announce(text: string): void;
  /** The customer computer's name, for the words about the drives. */
  host(): string;
}

/** The letters typed to go to a row, and when the last one came. */
interface Typed {
  text: string;
  at: number;
}

/** After this long without a letter, the next one starts a new word. */
const TYPING_PAUSE_MS = 900;

const SORT_KEYS: readonly SortKey[] = ["name", "modified", "type", "size"];
const SORT_WORDS: Record<SortKey, string> = {
  name: "name",
  modified: "date modified",
  type: "type",
  size: "size",
};

const MOVES: Record<string, Move> = {
  ArrowUp: "up",
  ArrowDown: "down",
  ArrowLeft: "left",
  ArrowRight: "right",
  Home: "home",
  End: "end",
  PageUp: "pageup",
  PageDown: "pagedown",
};

export interface ExplorerControl {
  /** A click on a control of the explorer (`data-action="xp-…"`). */
  onAction(event: ActionEvent): void;
  /** A key, while the Files tab is on screen. `true` when it was the explorer's. */
  onKeydown(event: KeyboardEvent): boolean;
  /** Escape: closes the menu or the path field. `false` when neither was open. */
  escape(): boolean;
  /** One payload of the `console-explorer` event. */
  onEvent(payload: unknown): void;
  /** The Files tab came on screen. */
  shown(): void;
  /** The window was drawn: measure the list and move it where the state has it. */
  afterRender(): void;
  /** The session's full control ended: nothing of it is waited for any more. */
  reset(): void;
  /** Listens on the window's root for the mouse and for scrolling. */
  bind(root: HTMLElement, signal: AbortSignal): void;
}

export function createExplorer(host: ExplorerHost): ExplorerControl {
  // The number of the last listing asked for. It is never set back, so an
  // answer that comes late is never taken for a later listing's.
  let seq = 0;
  // The paths that were typed, by the listing that asked for them: a typed
  // path that is refused comes back into the field to be corrected.
  const typedPaths = new Map<number, string>();
  // The scroll position of the state that the list on screen has.
  let appliedScroll = -1;
  let appliedTo: Element | null = null;
  // What to give the focus to once the window is drawn.
  let focusNext: "grid" | "path" | "menu" | "edit" | null = null;
  let typed: Typed = { text: "", at: 0 };

  const state = (): ExplorerState => host.get();

  function element<T extends HTMLElement = HTMLElement>(selector: string): T | null {
    return host.root()?.querySelector<T>(selector) ?? null;
  }

  // --- Going somewhere ---

  function fail(listing: number, error: unknown): void {
    const failure = isCommandError(error)
      ? { stage: String(error.stage), message: String(error.message) }
      : {
          stage: "unavailable",
          message: typeof error === "string" ? error : "The explorer is not available.",
        };
    const before = state();
    let next = applyFailure(before, listing, failure);
    const text = typedPaths.get(listing);
    typedPaths.delete(listing);
    // A typed path that was refused goes back into the field.
    if (text !== undefined && next.refusal && before.pending?.listing === listing) {
      next = { ...next, editing: text };
      focusNext = "path";
    }
    host.set(next, "window");
  }

  function go(place: Place, how: How, reveal?: string, text?: string): void {
    seq += 1;
    const listing = seq;
    if (text !== undefined) typedPaths.set(listing, text);
    host.set(
      opening(state(), { listing, place, how, ...(reveal === undefined ? {} : { reveal }) }),
      "window",
    );
    if (place.kind === "drives") {
      api.explorerDrives().then(
        (answer) => {
          const before = state();
          if (before.pending?.listing !== listing) return;
          const drives = Array.isArray(answer) ? answer.filter(isExplorerDrive) : [];
          arrived(before, applyDrives(before, listing, drives));
        },
        (error: unknown) => fail(listing, error),
      );
      return;
    }
    api.explorerOpen(place.path, listing).then(
      (answer) => {
        typedPaths.delete(listing);
        if (isExplorerListing(answer)) host.set(applyListing(state(), answer), "panel");
      },
      (error: unknown) => fail(listing, error),
    );
  }

  /** Takes a state in which a place may just have opened, and says so. */
  function arrived(before: ExplorerState, next: ExplorerState): void {
    if (next === before) return;
    const moved = !samePlace(before.place, next.place);
    if (moved) {
      // The control that was clicked may be gone: the list takes the focus,
      // unless the technician is typing somewhere.
      const active = document.activeElement;
      if (!(active instanceof HTMLInputElement)) focusNext = "grid";
    }
    host.set(next, moved || before.pending !== next.pending ? "window" : "panel");
    const opened = moved || (before.content.state === "loading" && next.content.state !== "loading");
    if (!opened || next.place === null) return;
    const where =
      next.place.kind === "drives" ? `The drives of ${host.host()}` : next.place.path;
    if (next.place.kind === "drives") {
      host.announce(`${where}: ${plural(rowCount(next), "drive")}.`);
    } else if (next.content.state === "loading") {
      host.announce(`${where}: loading…`);
    } else if (next.content.state === "complete") {
      host.announce(`${where}: ${plural(rowCount(next), "item")}.`);
    } else {
      host.announce(`${where}: ${plural(rowCount(next), "item")} listed. The list is not complete.`);
    }
  }

  /** The row to select in the place above: the one the explorer came out of. */
  function revealIn(target: Place): string | undefined {
    const from = state().place;
    if (from?.kind !== "folder") return undefined;
    if (target.kind === "drives") return crumbs(from)[0]?.label;
    const below = crumbs(from).find((part) => samePlace(parentPlace(part.place), target));
    return below?.label;
  }

  function refresh(): void {
    const place = state().place;
    if (place === null) go(DRIVES, "push");
    else go(place, "replace");
  }

  function up(): void {
    const above = parentPlace(state().place);
    if (above !== null) go(above, "push", revealIn(above));
  }

  function back(): void {
    const target = state().back[state().back.length - 1];
    if (target) go(target, "back", revealIn(target));
  }

  function forward(): void {
    const target = state().forward[state().forward.length - 1];
    if (target) go(target, "forward");
  }

  /** Enter or a double click: a folder or a drive opens; a file and a link say why not. */
  function open(key: string | undefined): void {
    if (key === undefined) return;
    const s = state();
    const target = openTarget(s, key);
    if (target) {
      go(target, "push");
      return;
    }
    const entry = entryOf(s, key);
    if (entry?.kind === "symlink") {
      host.announce(`${entry.name} is a link. A link is shown here, not followed.`);
    } else if (entry) {
      host.announce(`${entry.name} is a file. Folders open here, a file does not.`);
    }
  }

  // --- The path field ---

  function editPath(): void {
    focusNext = "path";
    host.set({ ...state(), editing: placeText(state().place), menu: undefined }, "window");
  }

  function leavePath(): void {
    focusNext = "edit";
    host.set({ ...state(), editing: undefined }, "window");
  }

  function goToTypedPath(): void {
    const text = element<HTMLInputElement>("#xp-path-input")?.value.trim() ?? "";
    if (text === "") {
      leavePath();
      return;
    }
    focusNext = "grid";
    go(folder(text), "push", undefined, text);
  }

  // --- The context menu ---

  function closeMenu(focus = true): boolean {
    if (!state().menu) return false;
    if (focus) focusNext = "grid";
    host.set({ ...state(), menu: undefined }, "window");
    return true;
  }

  function openMenu(x: number, y: number, keys: string[]): void {
    // Kept inside the window: the menu is about 200 pixels wide and a row high
    // for each of its actions.
    const next = { ...state(), menu: { x, y, keys } };
    const height = menuItems(next).length * 34 + 12;
    next.menu = {
      x: Math.max(0, Math.min(x, window.innerWidth - 210)),
      y: Math.max(0, Math.min(y, window.innerHeight - height)),
      keys,
    };
    focusNext = "menu";
    host.set(next, "window");
  }

  /** Arrow keys in the open menu go from one action that can run to the next. */
  function menuKey(event: KeyboardEvent): boolean {
    const items = Array.from(
      host.root()?.querySelectorAll<HTMLButtonElement>("#xp-menu button:not([disabled])") ?? [],
    );
    const at = items.indexOf(document.activeElement as HTMLButtonElement);
    let to = -1;
    if (event.key === "ArrowDown") to = at < 0 ? 0 : (at + 1) % items.length;
    if (event.key === "ArrowUp") to = at < 0 ? items.length - 1 : (at - 1 + items.length) % items.length;
    if (event.key === "Home") to = 0;
    if (event.key === "End") to = items.length - 1;
    if (to >= 0) {
      items[to]?.focus();
      return true;
    }
    if (event.key === "Tab") {
      closeMenu();
      return true;
    }
    return false;
  }

  // --- Keys ---

  function gridKey(event: KeyboardEvent): boolean {
    const s = state();
    const move = MOVES[event.key];
    if (move !== undefined && !event.altKey) {
      const mode = event.shiftKey ? "extend" : event.ctrlKey || event.metaKey ? "keep" : "select";
      host.set(moveFocus(s, move, mode), "panel");
      return true;
    }
    if (event.key === " ") {
      if (s.focus !== undefined) {
        host.set(
          event.ctrlKey || event.metaKey ? toggleSelected(s, s.focus) : selectOnly(s, s.focus),
          "panel",
        );
      }
      return true;
    }
    if (event.key === "Enter") {
      open(s.focus ?? (s.selected.size === 1 ? [...s.selected][0] : undefined));
      return true;
    }
    if ((event.key === "a" || event.key === "A") && (event.ctrlKey || event.metaKey)) {
      host.set(selectAll(s), "panel");
      host.announce(`${plural(rowCount(s), "item")} selected.`);
      return true;
    }
    if (event.key === "ContextMenu" || (event.key === "F10" && event.shiftKey)) {
      const at = indexOfKey(s, s.focus);
      const row = at < 0 ? null : element(`#xp-row-${at}`);
      const box = (row ?? element("#xp-scroll"))?.getBoundingClientRect();
      const keys =
        s.focus !== undefined && s.selected.has(s.focus)
          ? [...s.selected]
          : s.focus !== undefined
            ? [s.focus]
            : [];
      openMenu((box?.left ?? 0) + 24, (box?.top ?? 0) + 24, keys);
      return true;
    }
    // A letter goes to the first row that starts with what was typed.
    if (event.key.length === 1 && !event.ctrlKey && !event.metaKey && !event.altKey) {
      const now = Date.now();
      typed = { text: now - typed.at > TYPING_PAUSE_MS ? event.key : typed.text + event.key, at: now };
      host.set(selectByPrefix(s, typed.text), "panel");
      return true;
    }
    return false;
  }

  function onKeydown(event: KeyboardEvent): boolean {
    const target = event.target instanceof HTMLElement ? event.target : null;
    const inPanel = target?.closest("#xp") != null;
    const inField = target instanceof HTMLInputElement;
    if (inField && target.id === "xp-path-input") {
      if (event.key === "Enter") {
        event.preventDefault();
        goToTypedPath();
        return true;
      }
      // Every other key is the field's own.
      return false;
    }
    if (target?.closest("#xp-menu")) {
      if (menuKey(event)) {
        event.preventDefault();
        return true;
      }
      return false;
    }
    let handled = false;
    if (event.key === "F5") {
      refresh();
      handled = true;
    } else if ((event.key === "l" || event.key === "L") && event.ctrlKey) {
      editPath();
      handled = true;
    } else if (event.altKey && event.key === "ArrowLeft") {
      back();
      handled = true;
    } else if (event.altKey && event.key === "ArrowRight") {
      forward();
      handled = true;
    } else if (event.altKey && event.key === "ArrowUp") {
      up();
      handled = true;
    } else if (event.key === "Backspace" && inPanel && !inField) {
      up();
      handled = true;
    } else if (target?.id === "xp-grid") {
      handled = gridKey(event);
    }
    if (handled) event.preventDefault();
    return handled;
  }

  function escape(): boolean {
    if (closeMenu()) return true;
    if (state().editing !== undefined) {
      leavePath();
      return true;
    }
    return false;
  }

  // --- Clicks on controls ---

  function onAction(event: ActionEvent): void {
    const { action, arg } = event;
    const inMenu = event.element.closest("#xp-menu") !== null;
    const menuKeys = state().menu?.keys ?? [];
    if (inMenu) closeMenu();
    switch (action) {
      case "xp-back":
        back();
        break;
      case "xp-forward":
        forward();
        break;
      case "xp-up":
        up();
        break;
      case "xp-refresh":
        refresh();
        break;
      case "xp-drives":
        go(DRIVES, "push", revealIn(DRIVES));
        break;
      case "xp-go":
        if (arg) go(folder(arg), "push", revealIn(folder(arg)));
        break;
      case "xp-edit":
        editPath();
        break;
      case "xp-path-go":
        goToTypedPath();
        break;
      case "xp-path-cancel":
        leavePath();
        break;
      case "xp-view":
        if (arg === "details" || arg === "list") {
          host.set(withView(state(), arg as ExplorerView), "window");
          host.announce(arg === "details" ? "Details view." : "List view.");
        }
        break;
      case "xp-sort":
        if ((SORT_KEYS as readonly string[]).includes(arg ?? "")) {
          const next = withSort(state(), arg as SortKey);
          host.set(next, "window");
          host.announce(
            `Sorted by ${SORT_WORDS[next.sort.key]}, ${next.sort.descending ? "descending" : "ascending"}. Folders first.`,
          );
        }
        break;
      case "xp-dismiss":
        host.set({ ...state(), refusal: undefined }, "window");
        break;
      case "xp-menu-open":
        open(menuKeys.length === 1 ? menuKeys[0] : undefined);
        break;
      default:
        break;
    }
  }

  // --- The mouse on the list ---

  function rowOf(target: EventTarget | null): string | undefined {
    if (!(target instanceof Element)) return undefined;
    const row = target.closest<HTMLElement>("#xp-grid [data-row]");
    return row?.dataset.row;
  }

  /** Whether a click fell on the rows or beside them, not on the header. */
  function inList(target: EventTarget | null): boolean {
    return (
      target instanceof Element &&
      target.closest("#xp-scroll") !== null &&
      target.closest(".xp-head") === null
    );
  }

  function onClick(event: MouseEvent): void {
    if (!host.shown()) return;
    if (state().menu && !(event.target instanceof Element && event.target.closest("#xp-menu"))) {
      closeMenu(false);
    }
    if (!inList(event.target)) return;
    element("#xp-grid")?.focus({ preventScroll: true });
    const key = rowOf(event.target);
    const s = state();
    if (key === undefined) {
      // A click beside the rows selects nothing.
      host.set(clearSelection(s), "panel");
    } else if (event.shiftKey) {
      host.set(selectTo(s, key), "panel");
    } else if (event.ctrlKey || event.metaKey) {
      host.set(toggleSelected(s, key), "panel");
    } else {
      host.set(selectOnly(s, key), "panel");
    }
  }

  function onDoubleClick(event: MouseEvent): void {
    if (!host.shown()) return;
    open(rowOf(event.target));
  }

  function onContextMenu(event: MouseEvent): void {
    if (!host.shown() || !inList(event.target)) return;
    event.preventDefault();
    element("#xp-grid")?.focus({ preventScroll: true });
    const key = rowOf(event.target);
    let s = state();
    if (key === undefined) {
      s = clearSelection(s);
    } else if (!s.selected.has(key)) {
      s = selectOnly(s, key);
    }
    host.set(s, "none");
    openMenu(event.clientX, event.clientY, key === undefined ? [] : [...s.selected]);
  }

  // --- Scrolling and size ---

  /** What of the list is in view, read from the list itself. */
  function measured(list: HTMLElement): ExplorerState {
    const s = state();
    // The header lies in the box that scrolls and covers its top.
    const head = list.querySelector<HTMLElement>(".xp-head")?.offsetHeight ?? 0;
    return withViewport(s, {
      top: list.scrollTop,
      left: list.scrollLeft,
      height: list.clientHeight > head ? list.clientHeight - head : s.viewport.height,
      width: list.clientWidth > 0 ? list.clientWidth : s.viewport.width,
    });
  }

  function sameRows(a: ExplorerState, b: ExplorerState): boolean {
    const before = visibleRange(a);
    const after = visibleRange(b);
    return (
      before.first === after.first &&
      before.last === after.last &&
      positionOf(a, 1).left === positionOf(b, 1).left &&
      positionOf(a, 1).top === positionOf(b, 1).top
    );
  }

  function onScroll(event: Event): void {
    const list = event.target;
    if (!(list instanceof HTMLElement) || list.id !== "xp-scroll" || !host.shown()) return;
    const before = state();
    let next = measured(list);
    if (next === before) return;
    // The menu belongs to rows that are scrolling away.
    if (next.menu) next = { ...next, menu: undefined };
    host.set(next, next.menu !== before.menu ? "window" : sameRows(before, next) ? "none" : "rows");
  }

  function afterRender(): void {
    if (!host.shown()) return;
    const list = element("#xp-scroll");
    if (list) {
      // A list that was drawn anew, or that the tab hid, starts at its top:
      // it is put where the state has it.
      if (list !== appliedTo || state().scrollSeq !== appliedScroll) {
        appliedTo = list;
        appliedScroll = state().scrollSeq;
        list.scrollTop = state().viewport.top;
        list.scrollLeft = state().viewport.left;
      }
      const before = state();
      const next = measured(list);
      if (next !== before) host.set(next, sameRows(before, next) ? "none" : "rows");
    }
    // The end of a long path is what is of interest.
    const path = element(".xp-path ol");
    if (path) path.scrollLeft = path.scrollWidth;
    const wanted = focusNext;
    focusNext = null;
    switch (wanted) {
      case "grid":
        element("#xp-grid")?.focus({ preventScroll: true });
        break;
      case "path": {
        const field = element<HTMLInputElement>("#xp-path-input");
        field?.focus();
        field?.select();
        break;
      }
      case "edit":
        element("#xp-edit")?.focus();
        break;
      case "menu":
        element("#xp-menu button:not([disabled])")?.focus();
        break;
      default:
        break;
    }
  }

  // --- From the backend ---

  function onEvent(payload: unknown): void {
    if (!isExplorerEvent(payload)) return;
    const before = state();
    arrived(before, applyEvent(before, payload));
  }

  function shown(): void {
    // The list was hidden with its tab and is put back where it was.
    appliedTo = null;
    const s = state();
    if (s.place === null && !s.pending && s.ended === undefined) go(DRIVES, "push");
  }

  function reset(): void {
    typedPaths.clear();
    appliedTo = null;
    appliedScroll = -1;
    focusNext = null;
    typed = { text: "", at: 0 };
  }

  function bind(root: HTMLElement, signal: AbortSignal): void {
    root.addEventListener("click", onClick, { signal });
    root.addEventListener("dblclick", onDoubleClick, { signal });
    root.addEventListener("contextmenu", onContextMenu, { signal });
    // A scroll does not bubble; it is heard on its way down.
    root.addEventListener("scroll", onScroll, { capture: true, signal });
    window.addEventListener("resize", afterRender, { signal });
  }

  return { onAction, onKeydown, escape, onEvent, shown, afterRender, reset, bind };
}
