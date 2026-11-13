// The controller of the file explorer (ST-M010-002).
//
// It turns what the technician does in the Files tab into the commands of
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
// - **A change** — a send, a copy to this computer, a rename, a delete — is
//   asked of the backend only after what the technician must decide is
//   decided: a name that is taken, a delete to confirm. What was dropped or
//   picked to send is an offer the backend keeps; this module only ever
//   holds its number.
// - **The queue** is whatever the backend says it is. Each of its events
//   replaces the entries it names; nothing here guesses how a transfer ended.

import type { ConflictChoice, TransferQueued, UploadOffer } from "../shared/contract";
import type { ActionEvent } from "../shared/dom";
import { plural } from "../shared/feed";
import { type TransferName, api, isCommandError } from "./api";
import {
  type ConflictQuestion,
  DRIVES,
  type DeleteQuestion,
  type ExplorerState,
  type ExplorerView,
  type How,
  type Move,
  type Place,
  type SortKey,
  answerConflict,
  applyDrives,
  applyEvent,
  applyFailure,
  applyListing,
  applyRemoval,
  applyTransfers,
  canChange,
  clearSelection,
  conflictQuestion,
  conflictsDecided,
  crumbs,
  deleteItems,
  deleteOutcome,
  endedWords,
  entryOf,
  failureWords,
  folder,
  indexOfKey,
  isChosenFolder,
  isDeleteReport,
  isExplorerDrive,
  isExplorerEvent,
  isExplorerListing,
  isOpen,
  isRemovalProgress,
  isTransferCheck,
  isTransferEntry,
  isTransferQueued,
  isTransfersEvent,
  isUploadOffer,
  menuItems,
  moveFocus,
  notOpened,
  openTarget,
  opening,
  parentPlace,
  placeText,
  positionOf,
  queueRestWords,
  queuedWords,
  removalOf,
  renameProblem,
  renameSelection,
  rowCount,
  samePlace,
  selectAll,
  selectByPrefix,
  selectOnly,
  selectTo,
  selectedKeys,
  toggleSelected,
  transferNames,
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
  /** Whether a session is under full control: the explorer's commands work. */
  live(): boolean;
  /**
   * Says something in the window's own alert, which stays when the screen
   * changes: what a change came to after the explorer itself was dropped.
   */
  alert(problem: { title: string; message: string; next: string }): void;
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
  /** One payload of the `console-explorer-drop` event: what was dropped. */
  onDrop(payload: unknown): void;
  /** One payload of the `console-transfers` event: entries that changed. */
  onTransfers(payload: unknown): void;
  /** One payload of the `console-removal` event: where a delete stands. */
  onRemoval(payload: unknown): void;
  /** Files are dragged over the window, or no longer. */
  onDrag(over: boolean): void;
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
  let focusNext:
    | "grid"
    | "path"
    | "menu"
    | "edit"
    | "rename"
    | "question"
    | "upload"
    | null = null;
  let typed: Typed = { text: "", at: 0 };
  // The number of the last send or copy that was begun: an answer for an
  // older one, or for one the technician left, is not taken.
  let flow = 0;
  // The number of the last delete.
  let removals = 0;
  // Whether the queue of this session was asked for.
  let queueLoaded = false;
  // A file arrived in the open folder since the queue last came to rest.
  let arrivedHere = false;

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
    // Said where the technician looks, and to a screen reader.
    const why = notOpened(s, key);
    if (why !== undefined) {
      host.set({ ...s, hint: why }, "panel");
      host.announce(why);
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
    const menu = state().menu;
    if (!menu) return false;
    // The focus goes back to where the menu was opened from.
    if (focus) focusNext = menu.only === "upload" ? "upload" : "grid";
    host.set({ ...state(), menu: undefined }, "window");
    return true;
  }

  function openMenu(x: number, y: number, keys: string[], only?: "upload"): void {
    // Kept inside the window: the menu is about 200 pixels wide and a row high
    // for each of its actions.
    const menu = only === undefined ? { x, y, keys } : { x, y, keys, only };
    const next: ExplorerState = { ...state(), menu };
    const height = menuItems(next).length * 34 + 24;
    next.menu = {
      ...menu,
      x: Math.max(0, Math.min(x, window.innerWidth - 230)),
      y: Math.max(0, Math.min(y, window.innerHeight - height)),
    };
    focusNext = "menu";
    host.set(next, "window");
  }

  // --- What could not be done ---

  function failureOf(error: unknown): { stage: string; message: string } {
    return isCommandError(error)
      ? { stage: String(error.stage), message: String(error.message) }
      : {
          stage: "unavailable",
          message: typeof error === "string" ? error : "The explorer is not available.",
        };
  }

  /**
   * A change did not happen: the end of the session is said as that,
   * anything else as a refusal with its reason. What was being asked or
   * prepared is closed.
   */
  function changeFailed(title: string, error: unknown, next: string): void {
    const failure = failureOf(error);
    const closed: ExplorerState = { ...state(), preparing: undefined, question: undefined };
    const ended = endedWords(failure.stage);
    if (ended !== undefined) {
      host.set({ ...closed, ended }, "window");
      return;
    }
    host.set(
      { ...closed, problem: { title, message: failureWords(failure), next } },
      "window",
    );
  }

  // --- Sending ---

  /** The offer `offer` goes to the open folder, once its names are decided. */
  function offered(offer: UploadOffer): void {
    const s = state();
    if (!canChange(s)) {
      const why = "Open a folder first: files are sent to the folder that is open.";
      host.set({ ...s, hint: why, dropping: undefined }, "panel");
      host.announce(why);
      return;
    }
    if (s.question || s.preparing !== undefined) {
      host.announce("Answer the question that is open first, then drop the files again.");
      return;
    }
    if (s.place?.kind !== "folder") return;
    const into = s.place.path;
    flow += 1;
    const mine = flow;
    host.set(
      {
        ...s,
        preparing: `looking at ${into} for names that are taken…`,
        menu: undefined,
        hint: undefined,
        dropping: undefined,
        problem: undefined,
      },
      "window",
    );
    api.explorerSendCheck(offer.offer, into).then(
      (check) => {
        if (mine !== flow) return;
        if (!isTransferCheck(check)) {
          changeFailed("The files could not be sent", "The answer was not understood.", NOTHING_SENT);
          return;
        }
        const question = conflictQuestion(check, {
          direction: "send",
          folder: into,
          offer: offer.offer,
        });
        if (question) ask(question);
        else send(offer.offer, into, {});
      },
      (error: unknown) => {
        if (mine === flow) changeFailed("The files could not be sent", error, NOTHING_SENT);
      },
    );
  }

  function send(offer: number, into: string, choices: Record<string, ConflictChoice>): void {
    flow += 1;
    const mine = flow;
    host.set({ ...state(), question: undefined, preparing: "queueing…" }, "window");
    api.explorerSend(offer, into, choices).then(
      (queued) => {
        if (mine !== flow) return;
        host.set({ ...state(), preparing: undefined }, "window");
        if (isTransferQueued(queued)) saidQueued("send", queued);
      },
      (error: unknown) => {
        if (mine === flow) changeFailed("The files could not be sent", error, NOTHING_SENT);
      },
    );
  }

  /**
   * Says how a send or a copy was queued, unless the queue has come to rest
   * already and said how it ended: that is the newer word.
   */
  function saidQueued(direction: "send" | "download", queued: TransferQueued): void {
    if (queued.queued === 0 || state().transfers.some(isOpen)) {
      host.announce(queuedWords(direction, queued));
    }
  }

  /** The system's dialog for files, or for a folder, to send. */
  function pick(kind: "files" | "folder"): void {
    if (!canChange(state())) return;
    api.explorerPickUpload(kind).then(
      (offer) => {
        // Closed without a choice: nothing is sent.
        if (isUploadOffer(offer)) offered(offer);
      },
      (error: unknown) => changeFailed("The files could not be chosen", error, NOTHING_SENT),
    );
  }

  // --- Copying to this computer ---

  /** Copies the rows `keys` to this computer: to the folder of this customer computer, or to one that is chosen. */
  function download(keys: readonly string[], choose: boolean): void {
    const s = state();
    if (!canChange(s) || s.place?.kind !== "folder" || keys.length === 0) return;
    if (s.question || s.preparing !== undefined) return;
    const names = transferNames(s, keys);
    if (names.length === 0) return;
    const from = s.place.path;
    flow += 1;
    const mine = flow;
    const check = (to: number | undefined): void => {
      host.set(
        {
          ...state(),
          preparing: "looking for names that are taken on this computer…",
          menu: undefined,
          problem: undefined,
        },
        "window",
      );
      api.explorerDownloadCheck(names, to).then(
        (answer) => {
          if (mine !== flow) return;
          if (!isTransferCheck(answer)) {
            changeFailed("The copy could not begin", "The answer was not understood.", NOTHING_COPIED);
            return;
          }
          const question = conflictQuestion(answer, {
            direction: "download",
            folder: from,
            names,
            ...(to === undefined ? {} : { to }),
          });
          if (question) ask(question);
          else copy(from, names, to, {});
        },
        (error: unknown) => {
          if (mine === flow) changeFailed("The copy could not begin", error, NOTHING_COPIED);
        },
      );
    };
    if (!choose) {
      check(undefined);
      return;
    }
    api.explorerChooseFolder().then(
      (chosen) => {
        // Closed without a choice: nothing is copied.
        if (mine === flow && isChosenFolder(chosen)) check(chosen.folder);
      },
      (error: unknown) => {
        if (mine === flow) changeFailed("The folder could not be chosen", error, NOTHING_COPIED);
      },
    );
  }

  function copy(
    from: string,
    names: TransferName[],
    to: number | undefined,
    choices: Record<string, ConflictChoice>,
  ): void {
    flow += 1;
    const mine = flow;
    host.set({ ...state(), question: undefined, preparing: "queueing…" }, "window");
    api.explorerDownload(from, names, to, choices).then(
      (queued) => {
        if (mine !== flow) return;
        host.set({ ...state(), preparing: undefined }, "window");
        if (isTransferQueued(queued)) saidQueued("download", queued);
      },
      (error: unknown) => {
        if (mine === flow) changeFailed("The copy could not begin", error, NOTHING_COPIED);
      },
    );
  }

  // --- A question before a change ---

  function ask(question: ConflictQuestion | DeleteQuestion): void {
    focusNext = "question";
    host.set({ ...state(), preparing: undefined, menu: undefined, question }, "window");
  }

  /** Leaves the question that is open: nothing is sent, copied or deleted. */
  function cancelQuestion(): boolean {
    const question = state().question;
    if (!question) return false;
    flow += 1;
    focusNext = "grid";
    host.set({ ...state(), question: undefined, preparing: undefined }, "window");
    host.announce(
      question.kind === "delete"
        ? "Nothing was deleted."
        : question.direction === "send"
          ? "Nothing was sent."
          : "Nothing was copied.",
    );
    return true;
  }

  /** The technician chose for the name that is asked about. */
  function chose(choice: ConflictChoice): void {
    const question = state().question;
    if (question?.kind !== "conflicts") return;
    const next = answerConflict(question, choice);
    if (!conflictsDecided(next)) {
      focusNext = "question";
      host.set({ ...state(), question: next }, "window");
      return;
    }
    focusNext = "grid";
    if (next.direction === "send" && next.offer !== undefined) {
      send(next.offer, next.folder, next.choices);
    } else if (next.names) {
      copy(next.folder, next.names, next.to, next.choices);
    }
  }

  // --- Deleting ---

  function askDelete(keys: readonly string[]): void {
    const s = state();
    if (!canChange(s) || s.place?.kind !== "folder" || s.removal || s.question) return;
    const items = deleteItems(s, keys);
    if (items.length === 0) return;
    ask({ kind: "delete", folder: s.place.path, items });
  }

  function runDelete(question: DeleteQuestion): void {
    removals += 1;
    const id = removals;
    const count = question.items.length;
    focusNext = "grid";
    host.set(
      {
        ...state(),
        question: undefined,
        problem: undefined,
        removal: removalOf(id, count),
      },
      "window",
    );
    host.announce(`Deleting ${plural(count, "item")}…`);
    api.explorerDelete(question.folder, question.items, id).then(
      (report) => {
        if (!isDeleteReport(report)) {
          deleteFailed(id, "The answer was not understood.");
          return;
        }
        const outcome = deleteOutcome(report);
        const s = state();
        if (s.removal?.id !== id) {
          // The explorer of that session is gone: what the delete came to is
          // said where the technician is now.
          if (outcome.refusal) host.alert(outcome.refusal);
          return;
        }
        let next: ExplorerState = { ...s, removal: undefined, problem: outcome.refusal };
        if (report.ended !== undefined && next.ended === undefined) {
          next = { ...next, ended: endedWords("no_consented_session") };
        }
        host.set(next, "window");
        host.announce(outcome.notice);
        // The list as it really is now.
        if (report.ended === undefined && samePlace(state().place, folder(question.folder))) {
          refresh();
        }
      },
      (error: unknown) => deleteFailed(id, error),
    );
  }

  function deleteFailed(id: number, error: unknown): void {
    if (state().removal?.id !== id) return;
    host.set({ ...state(), removal: undefined }, "none");
    changeFailed(
      "The delete did not run",
      error,
      "Nothing was deleted. Refresh the folder and try again.",
    );
  }

  function cancelDelete(): void {
    const removal = state().removal;
    if (!removal || removal.cancelling) return;
    host.set({ ...state(), removal: { ...removal, cancelling: true } }, "panel");
    // How it ended arrives with the delete's own answer.
    api.explorerCancelDelete(removal.id).catch(() => undefined);
  }

  // --- Renaming ---

  function startRename(key: string | undefined): void {
    const s = state();
    if (key === undefined || !canChange(s) || s.removal || !entryOf(s, key)) return;
    focusNext = "rename";
    host.set({ ...selectOnly(s, key), renaming: key, menu: undefined, problem: undefined }, "window");
  }

  function cancelRename(focus = true): boolean {
    if (state().renaming === undefined) return false;
    if (focus) focusNext = "grid";
    host.set({ ...state(), renaming: undefined }, "window");
    return true;
  }

  function commitRename(): void {
    const s = state();
    const key = s.renaming;
    if (key === undefined || s.place?.kind !== "folder") return;
    const to = element<HTMLInputElement>("#xp-rename-input")?.value ?? key;
    if (to === key) {
      cancelRename();
      return;
    }
    const problem = renameProblem(s, key, to);
    if (problem !== undefined) {
      // The field stays open: the name is corrected where it was typed.
      host.set(
        {
          ...s,
          problem: {
            title: "That name cannot be used",
            message: problem,
            next: "Type another name, or press Escape to leave the name as it is.",
          },
        },
        "window",
      );
      return;
    }
    const where = s.place;
    focusNext = "grid";
    host.set(
      { ...s, renaming: undefined, problem: undefined, preparing: `renaming ${key}…` },
      "window",
    );
    api.explorerRename(where.path, key, to).then(
      () => {
        const now = state();
        const here = samePlace(now.place, where);
        // The new name is selected once the folder is listed again.
        host.set({ ...now, preparing: undefined, ...(here ? { reveal: to } : {}) }, "window");
        host.announce(`${key} is now ${to}.`);
        if (here) refresh();
      },
      (error: unknown) =>
        changeFailed(
          `${key} could not be renamed`,
          error,
          "Nothing was changed. Check the name, or refresh the folder and try again.",
        ),
    );
  }

  // --- The queue ---

  function cancelTransfer(id: number): void {
    // How it ended arrives on the queue's own event.
    api.explorerCancelTransfer(id).catch(() => undefined);
  }

  function showFolder(id: number): void {
    const entry = state().transfers.find((transfer) => transfer.id === id);
    api.explorerShowFolder(id).catch((error: unknown) => {
      changeFailed(
        "The folder could not be shown",
        error,
        entry ? `The copy is at ${entry.local}.` : "Open the folder in your file manager.",
      );
    });
  }

  function loadQueue(): void {
    if (queueLoaded) return;
    queueLoaded = true;
    api.explorerTransfers().then(
      (entries) => {
        if (!Array.isArray(entries) || !host.live()) return;
        host.set(applyTransfers(state(), entries.filter(isTransferEntry)), "panel");
      },
      () => {
        // Not offered: the queue fills from its events.
      },
    );
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
    if (event.key === "F2") {
      const keys = selectedKeys(s);
      startRename(keys.length === 1 ? keys[0] : undefined);
      return true;
    }
    if (event.key === "Delete") {
      askDelete(selectedKeys(s));
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

  /** Tab stays inside the question that is open, going round its controls. */
  function trapTab(event: KeyboardEvent): void {
    const controls = Array.from(
      host
        .root()
        ?.querySelectorAll<HTMLElement>(
          "#xp-question button:not([disabled]), #xp-question input:not([disabled])",
        ) ?? [],
    );
    if (controls.length === 0) return;
    const at = controls.indexOf(document.activeElement as HTMLElement);
    const to = event.shiftKey
      ? at <= 0
        ? controls.length - 1
        : at - 1
      : at < 0 || at === controls.length - 1
        ? 0
        : at + 1;
    event.preventDefault();
    controls[to]?.focus();
  }

  function onKeydown(event: KeyboardEvent): boolean {
    const target = event.target instanceof HTMLElement ? event.target : null;
    const inPanel = target?.closest("#xp") != null;
    const inField = target instanceof HTMLInputElement;
    // Nothing of the explorer acts behind a question: its own buttons take
    // Enter and Space, and Tab goes round them.
    if (state().question) {
      if (event.key === "Tab") trapTab(event);
      return true;
    }
    if (inField && target.id === "xp-path-input") {
      if (event.key === "Enter") {
        event.preventDefault();
        goToTypedPath();
        return true;
      }
      // Every other key is the field's own.
      return false;
    }
    if (inField && target.id === "xp-rename-input") {
      if (event.key === "Enter") {
        event.preventDefault();
        commitRename();
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
    if (cancelQuestion()) return true;
    if (closeMenu()) return true;
    if (cancelRename()) return true;
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
    // An action of the menu is for the rows the menu opened on; a button and
    // a key are for the selection.
    const keys = inMenu && state().menu?.only === undefined ? menuKeys : selectedKeys(state());
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
        // The button goes with what it dismisses: the list takes the focus,
        // unless a name is being typed.
        if (state().renaming === undefined) focusNext = "grid";
        host.set(
          state().problem ? { ...state(), problem: undefined } : { ...state(), refusal: undefined },
          "window",
        );
        break;
      case "xp-menu-open":
        open(menuKeys.length === 1 ? menuKeys[0] : undefined);
        break;
      case "xp-upload": {
        // The two ways to choose what to send, as a menu under the button.
        const box = event.element.getBoundingClientRect();
        if (state().menu?.only === "upload") closeMenu();
        else openMenu(box.left, box.bottom + 4, [], "upload");
        break;
      }
      case "xp-upload-files":
        pick("files");
        break;
      case "xp-upload-folder":
        pick("folder");
        break;
      case "xp-download":
        download(keys, false);
        break;
      case "xp-download-to":
        download(keys, true);
        break;
      case "xp-rename":
        startRename(keys.length === 1 ? keys[0] : undefined);
        break;
      case "xp-delete":
        askDelete(keys);
        break;
      case "xp-conflict":
        if (arg === "replace" || arg === "keep_both" || arg === "skip") chose(arg);
        break;
      case "xp-conflict-all": {
        const question = state().question;
        if (question?.kind === "conflicts") {
          host.set({ ...state(), question: { ...question, all: !question.all } }, "window");
        }
        break;
      }
      case "xp-question-cancel":
        cancelQuestion();
        break;
      case "xp-delete-confirm": {
        const question = state().question;
        if (question?.kind === "delete") runDelete(question);
        break;
      }
      case "xp-cancel-delete":
        cancelDelete();
        break;
      case "xp-cancel-transfer":
        if (arg !== undefined && Number.isFinite(Number(arg))) cancelTransfer(Number(arg));
        break;
      case "xp-cancel-transfers":
        api.explorerCancelTransfers().catch(() => undefined);
        break;
      case "xp-show-folder":
        if (arg !== undefined && Number.isFinite(Number(arg))) showFolder(Number(arg));
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

  /** Whether the mouse is on the field of a rename, which takes it itself. */
  function inRename(target: EventTarget | null): boolean {
    return target instanceof Element && target.closest("#xp-rename-input") !== null;
  }

  function onClick(event: MouseEvent): void {
    if (!host.shown()) return;
    if (inRename(event.target)) return;
    const within = (selector: string): boolean =>
      event.target instanceof Element && event.target.closest(selector) !== null;
    if (state().menu && !within("#xp-menu") && !within("#xp-upload")) {
      closeMenu(false);
    }
    // A click elsewhere leaves a name that is being typed as it was: only
    // Enter renames. The click that opened the field is not one of them, and
    // neither is one that dismisses what was said about the name.
    if (
      state().renaming !== undefined &&
      within("#xp") &&
      !within('[data-action="xp-rename"]') &&
      !within('[data-action="xp-dismiss"]')
    ) {
      cancelRename(false);
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
    if (!host.shown() || inRename(event.target)) return;
    open(rowOf(event.target));
  }

  function onContextMenu(event: MouseEvent): void {
    if (!host.shown() || !inList(event.target) || inRename(event.target)) return;
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
      case "rename": {
        const field = element<HTMLInputElement>("#xp-rename-input");
        const s = state();
        const entry = s.renaming === undefined ? undefined : entryOf(s, s.renaming);
        field?.focus({ preventScroll: true });
        // The name without its extension is what is usually typed anew.
        if (field && entry) field.setSelectionRange(...renameSelection(entry));
        break;
      }
      case "question":
        (
          element("#xp-question [data-autofocus]:not([disabled])") ??
          element("#xp-question button:not([disabled])")
        )?.focus();
        break;
      case "upload":
        element("#xp-upload")?.focus();
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

  function onDrop(payload: unknown): void {
    if (!isUploadOffer(payload) || !host.live()) return;
    if (!host.shown()) {
      host.announce("Files are sent from the Files tab: open it and a folder, then drop them there.");
      return;
    }
    offered(payload);
  }

  function onTransfers(payload: unknown): void {
    if (!isTransfersEvent(payload) || !host.live()) return;
    const before = state();
    let next = applyTransfers(before, payload.entries);
    // A transfer the session's end stopped: said where the technician looks.
    const stopped = payload.entries.find((entry) => entry.state === "stopped");
    if (stopped && next.ended === undefined) {
      next = {
        ...next,
        ended: endedWords(
          stopped.reason?.includes("connection") ? "disconnected" : "no_consented_session",
        ),
      };
    }
    host.set(next, stopped ? "window" : "panel");
    // A file that arrived in the folder that is open belongs in its list.
    const here = next.place?.kind === "folder" ? next.place.path.toLowerCase() : undefined;
    if (
      here !== undefined &&
      payload.entries.some(
        (entry) =>
          entry.direction === "send" &&
          entry.state === "done" &&
          entry.remote.toLowerCase().startsWith(`${here.replace(/\\$/, "")}\\`),
      )
    ) {
      arrivedHere = true;
    }
    const rest = queueRestWords(before.transfers, next.transfers);
    if (rest === undefined) return;
    host.announce(rest);
    const s = state();
    if (arrivedHere && s.renaming === undefined && !s.question && !s.pending && s.ended === undefined) {
      arrivedHere = false;
      refresh();
    }
  }

  function onRemoval(payload: unknown): void {
    if (!isRemovalProgress(payload)) return;
    host.set(applyRemoval(state(), payload), "panel");
  }

  function onDrag(over: boolean): void {
    const s = state();
    const dropping = over && host.shown() && host.live() ? true : undefined;
    if (s.dropping === dropping) return;
    host.set({ ...s, dropping }, "panel");
  }

  function shown(): void {
    // The list was hidden with its tab and is put back where it was.
    appliedTo = null;
    const s = state();
    if (s.place === null && !s.pending && s.ended === undefined) go(DRIVES, "push");
    loadQueue();
  }

  function reset(): void {
    typedPaths.clear();
    appliedTo = null;
    appliedScroll = -1;
    focusNext = null;
    typed = { text: "", at: 0 };
    // An answer for a send or a copy of the session before is not taken.
    flow += 1;
    queueLoaded = false;
    arrivedHere = false;
  }

  function bind(root: HTMLElement, signal: AbortSignal): void {
    root.addEventListener("click", onClick, { signal });
    root.addEventListener("dblclick", onDoubleClick, { signal });
    root.addEventListener("contextmenu", onContextMenu, { signal });
    // A scroll does not bubble; it is heard on its way down.
    root.addEventListener("scroll", onScroll, { capture: true, signal });
    window.addEventListener("resize", afterRender, { signal });
  }

  return {
    onAction,
    onKeydown,
    escape,
    onEvent,
    onDrop,
    onTransfers,
    onRemoval,
    onDrag,
    shown,
    afterRender,
    reset,
    bind,
  };
}

const NOTHING_SENT = "Nothing was sent. Refresh the folder, then drop or choose the files again.";
const NOTHING_COPIED = "Nothing was copied. Refresh the folder and try again.";
