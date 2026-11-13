// A customer computer held in memory, behind the commands of the file
// explorer's backend (ST-M010-002). Development and tests only: the tests of
// the explorer's controller drive it, and so does the development page
// `dev/explorer.html`; the application never loads it.
//
// It answers the way the real backend does: every place is named by the
// computer's own spelling, whatever was asked for; a folder arrives as steps
// of a listing, the first with the place and the first page; a path to a
// file opens the folder that holds it; a place that is refused answers with
// the refusal and delivers nothing; and a newer listing, or the drive list,
// leaves the listing that is still running.
//
// It also sends, copies, renames and deletes the way the backend does: what
// is dropped or picked is an offer with a number; a name that is taken is
// found by a look and decided by the choices; the queue runs one transfer at
// a time and tells each change; a link is left out; a name that could leave
// its folder is not copied; a delete says what it could not remove; and the
// end of the session stops what runs and what waits.

import type {
  ChosenFolder,
  ConflictChoice,
  DeleteReport,
  ExplorerDrive,
  ExplorerEntry,
  ExplorerEvent,
  ExplorerListing,
  NameConflict,
  RemovalProgress,
  TransferCheck,
  TransferEntry,
  TransferQueued,
  TransfersEvent,
  UploadOffer,
} from "../shared/contract";
import type { DeleteItem, TransferName } from "./api";

/** A refusal as the backend answers it: the category in a word, and why. */
export interface SimulatedRefusal {
  stage: string;
  message: string;
}

/** A listing that is running: what it has delivered of its pages. */
interface Running {
  listing: number;
  path: string;
  pages: ExplorerEntry[][];
  sent: number;
  select?: string;
  resolve: (answer: ExplorerListing) => void;
}

/** A file, a folder or a link on the technician computer: what an offer holds. */
export interface SimulatedLocal {
  name: string;
  kind: "file" | "folder" | "link";
  len?: number;
  children?: SimulatedLocal[];
}

/** Where the simulated backend delivers what it tells the window by itself. */
export interface SimulatedSignals {
  /** The `console-transfers` event. */
  transfers?: (event: TransfersEvent) => void;
  /** The `console-removal` event. */
  removal?: (step: RemovalProgress) => void;
  /** The `console-explorer-drop` event. */
  drop?: (offer: UploadOffer) => void;
}

/** What the queue still has to do for an entry. */
interface Job {
  /** A send: the folder it goes into and the name it gets there. */
  into?: string;
  as?: string;
  replace?: boolean;
  /** A copy: where it lands on the technician computer. */
  at?: string;
  /** A folder of the customer computer whose files are queued when it runs. */
  walk?: { from: string; to: string; shown: string; replace: boolean };
  /** A folder to make on the customer computer. */
  make?: string;
}

const SESSION_ENDED = "The session ended before this was complete.";
const LINK_LEFT_OUT = "A link is not followed.";

/** Whether a name of the customer computer may be a name on the technician computer. */
function plainName(name: string): boolean {
  return (
    name !== "" &&
    name !== "." &&
    name !== ".." &&
    !/[\\/:*?"<>|]/.test(name) &&
    !/[. ]$/.test(name) &&
    !/^(con|prn|aux|nul|com[1-9]|lpt[1-9])(\.|$)/i.test(name)
  );
}

function join(folder: string, name: string): string {
  return folder.endsWith("\\") ? `${folder}${name}` : `${folder}\\${name}`;
}

/** `report (2).pdf`, then `report (3).pdf`: the first that is not taken. */
function keptBeside(name: string, isFolder: boolean, taken: (name: string) => boolean): string {
  const dot = name.lastIndexOf(".");
  const [stem, extension] =
    !isFolder && dot > 0 ? [name.slice(0, dot), name.slice(dot)] : [name, ""];
  for (let n = 2; ; n += 1) {
    const candidate = `${stem} (${n})${extension}`;
    if (!taken(candidate)) return candidate;
  }
}

const T0 = Date.UTC(2026, 10, 3, 14, 30, 0);

export function simulatedFile(name: string, len = 1024, modifiedMs = T0): ExplorerEntry {
  return { name, kind: "file", len, modifiedMs, readonly: false };
}

export function simulatedFolder(name: string, modifiedMs = T0): ExplorerEntry {
  return { name, kind: "dir", len: 0, modifiedMs, readonly: false };
}

export function simulatedLink(name: string): ExplorerEntry {
  return { name, kind: "symlink", len: 0, modifiedMs: T0, readonly: false };
}

/** `count` files as a system folder holds them, with sizes that differ. */
export function simulatedFiles(count: number): ExplorerEntry[] {
  return Array.from({ length: count }, (_, n) =>
    simulatedFile(
      `driver-${String(n).padStart(5, "0")}.dll`,
      1000 + ((n * 7919) % 5000),
      T0 - n * 60_000,
    ),
  );
}

export class SimulatedComputer {
  drives: ExplorerDrive[] = [
    { root: "C:\\", kind: "fixed", label: "Windows", totalBytes: 500e9, freeBytes: 100e9 },
    { root: "E:\\", kind: "removable" },
  ];

  // Every folder by its real path, with its entries in the order it lists them.
  folders = new Map<string, ExplorerEntry[]>([
    [
      "C:\\",
      [
        simulatedFolder("Users"),
        simulatedFolder("Windows"),
        simulatedLink("Documents and Settings"),
        simulatedFile("pagefile.sys"),
      ],
    ],
    ["C:\\Users", [simulatedFolder("ann"), simulatedFolder("jdoe")]],
    [
      "C:\\Users\\ann",
      [simulatedFolder("Documents"), simulatedFolder("Downloads"), simulatedFile("NTUSER.DAT")],
    ],
    [
      "C:\\Users\\ann\\Documents",
      [
        simulatedFolder("Invoices"),
        simulatedFile("a.txt", 30),
        simulatedFile("b.txt", 10),
        simulatedFile("c.txt", 20),
        simulatedFile("report.pdf", 5000),
      ],
    ],
    ["C:\\Users\\ann\\Documents\\Invoices", []],
    ["C:\\Users\\ann\\Downloads", [simulatedFile("setup.exe")]],
    ["C:\\Windows", [simulatedFolder("System32")]],
    ["C:\\Windows\\System32", simulatedFiles(5000)],
    ["E:\\", []],
  ]);

  // The folders this computer refuses to list, each with its refusal.
  refused = new Map<string, SimulatedRefusal>([
    ["C:\\Users\\jdoe", { stage: "access_denied", message: "access was denied" }],
  ]);

  // The most entries of one page.
  pageSize = 256;

  // When set, a listing delivers a page only when `step()` is called.
  manual = false;

  // The listings that are running, the oldest first.
  open: Running[] = [];

  // --- What is sent, copied, renamed and deleted ---

  // What the system's dialog answers next: the files, or the folder, that
  // were picked; `null` closes it without a choice.
  picked: { files: SimulatedLocal[] | null; folder: SimulatedLocal[] | null } = {
    files: null,
    folder: null,
  };

  // The folder of the technician computer the system's dialog answers next.
  chosen: string | null = null;

  // Where a copy goes when no folder was chosen.
  downloads = "C:\\Users\\sam\\Downloads\\CretLink\\ACME-PC-07";

  // The files of the technician computer, each by its path with its length.
  local = new Map<string, number>();

  // Transfers that fail, by their path on the customer computer, each with
  // the reason the queue gives.
  failing = new Map<string, string>();

  // Entries that cannot be removed, by their path, each with the reason.
  held = new Map<string, string>();

  // The session has ended this way: every command that needs it says so.
  ended: SimulatedRefusal | null = null;

  // When set, the queue moves only when `stepTransfers()` is called.
  manualTransfers = false;

  // When set, a delete answers only when `finishDelete()` is called.
  manualDelete = false;

  // The transfer queue, in the order of its numbers.
  queue: TransferEntry[] = [];

  // The folders that were shown in the file manager, in order.
  shownFolders: string[] = [];

  private jobs = new Map<number, Job>();
  private offers = new Map<number, SimulatedLocal[]>();
  private chosenFolders = new Map<number, string>();
  private lastId = 0;
  private lastOffer = 0;
  private lastChosen = 0;
  private removal: { id: number; cancelled: boolean; finish?: () => void } | null = null;

  /**
   * `emit` delivers one step of a listing, as the `console-explorer` event
   * does; `signals` deliver the events of the queue, of a delete and of a
   * drop.
   */
  constructor(
    private readonly emit: (event: ExplorerEvent) => void,
    private readonly signals: SimulatedSignals = {},
  ) {}

  /** The real spelling of a path, whatever its letter case and its slashes. */
  private real(asked: string): string | undefined {
    const wanted = asked.replace(/\//g, "\\").toLowerCase();
    for (const path of this.folders.keys()) if (path.toLowerCase() === wanted) return path;
    for (const path of this.refused.keys()) if (path.toLowerCase() === wanted) return path;
    return undefined;
  }

  /** A newer place leaves every listing that is still running. */
  private leave(): void {
    for (const running of this.open.splice(0)) {
      running.resolve({
        listing: running.listing,
        path: running.path,
        entries: running.pages.slice(0, running.sent).flat().length,
        state: "cancelled",
      });
    }
  }

  /** `explorer_drives`. */
  listDrives(): ExplorerDrive[] {
    this.leave();
    return this.drives;
  }

  /** `explorer_open`: rejects with a `SimulatedRefusal` for a place that is refused. */
  openPath(asked: string, listing: number): Promise<ExplorerListing> {
    this.leave();
    let path = this.real(asked);
    let select: string | undefined;
    if (path === undefined) {
      // A path to a file opens the folder that holds it.
      const spelled = asked.replace(/\//g, "\\");
      const cut = spelled.lastIndexOf("\\");
      const parent = cut < 0 ? undefined : this.real(spelled.slice(0, cut) || spelled);
      const name = spelled.slice(cut + 1);
      const entry = parent
        ? this.folders.get(parent)?.find((e) => e.name.toLowerCase() === name.toLowerCase())
        : undefined;
      if (!parent || !entry) {
        const refusal: SimulatedRefusal = {
          stage: "path_not_found",
          message: `${parent ?? "the customer computer"} lists no entry named ${name}`,
        };
        return Promise.reject(refusal);
      }
      path = parent;
      select = entry.name;
    }
    const refusal = this.refused.get(path);
    if (refusal) return Promise.reject(refusal);
    const entries = this.folders.get(path) ?? [];
    const pages: ExplorerEntry[][] = [];
    for (let from = 0; from < entries.length; from += this.pageSize) {
      pages.push(entries.slice(from, from + this.pageSize));
    }
    if (pages.length === 0) pages.push([]);
    const place = path;
    return new Promise((resolve) => {
      const running: Running = { listing, path: place, pages, sent: 0, resolve };
      if (select !== undefined) running.select = select;
      this.open.push(running);
      if (!this.manual) while (this.open.includes(running)) this.step();
    });
  }

  /** Delivers the next page of the oldest listing that runs; its last page ends it. */
  step(): void {
    const running = this.open[0];
    if (!running) throw new Error("no listing is running");
    const entries = running.pages[running.sent] ?? [];
    running.sent += 1;
    const end = running.sent === running.pages.length;
    this.emit(
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
    );
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

  // --- What is sent, copied, renamed and deleted (ST-M010-002) ---

  private refuseIfEnded(): void {
    if (this.ended) throw this.ended;
  }

  /** The names of `folder`, each by its spelling in small letters. */
  private taken(folder: string): Map<string, ExplorerEntry> {
    const names = new Map<string, ExplorerEntry>();
    for (const entry of this.folders.get(folder) ?? []) names.set(entry.name.toLowerCase(), entry);
    return names;
  }

  private tell(entries: TransferEntry[]): void {
    if (entries.length > 0) this.signals.transfers?.({ entries: entries.map((e) => ({ ...e })) });
  }

  private add(entry: Omit<TransferEntry, "id" | "done">, job: Job = {}): TransferEntry {
    this.lastId += 1;
    const added: TransferEntry = { ...entry, id: this.lastId, done: 0 };
    this.queue.push(added);
    this.jobs.set(added.id, job);
    return added;
  }

  /** Files and folders are dropped on the window: an offer, told on the drop event. */
  drop(items: SimulatedLocal[]): UploadOffer {
    this.lastOffer += 1;
    this.offers.set(this.lastOffer, items);
    const offer: UploadOffer = {
      offer: this.lastOffer,
      items: items.length,
      names: items.slice(0, 5).map((item) => item.name),
    };
    this.signals.drop?.(offer);
    return offer;
  }

  /** `explorer_pick_upload`. */
  pickUpload(kind: "files" | "folder"): UploadOffer | null {
    this.refuseIfEnded();
    const items = this.picked[kind];
    if (!items) return null;
    this.lastOffer += 1;
    this.offers.set(this.lastOffer, items);
    return {
      offer: this.lastOffer,
      items: items.length,
      names: items.slice(0, 5).map((item) => item.name),
    };
  }

  /** `explorer_send_check`. */
  sendCheck(offer: number, folder: string): TransferCheck {
    this.refuseIfEnded();
    const items = this.offers.get(offer);
    if (!items) throw { stage: "refused", message: "These files are no longer kept for sending." };
    const refusal = this.refused.get(folder);
    if (refusal) throw refusal;
    if (!this.folders.has(folder)) {
      throw { stage: "path_not_found", message: "the path does not exist" };
    }
    const taken = this.taken(folder);
    const conflicts: NameConflict[] = [];
    for (const item of items) {
      const existing = taken.get(item.name.toLowerCase());
      if (!existing || item.kind === "link") continue;
      conflicts.push({
        name: item.name,
        kind: item.kind,
        existing: existing.kind,
        replace:
          (item.kind === "file" && existing.kind === "file") ||
          (item.kind === "folder" && existing.kind === "dir"),
      });
    }
    return { folder, conflicts };
  }

  /** `explorer_send`. */
  send(offer: number, folder: string, choices: Record<string, ConflictChoice>): TransferQueued {
    this.refuseIfEnded();
    const items = this.offers.get(offer);
    if (!items) throw { stage: "refused", message: "These files are no longer kept for sending." };
    this.offers.delete(offer);
    const taken = this.taken(folder);
    const used = new Set<string>();
    const queued: TransferQueued = { queued: 0, leftOut: 0, skipped: 0 };
    const added: TransferEntry[] = [];
    const local = (name: string): string => `C:\\Users\\sam\\Desktop\\${name}`;
    const leave = (name: string, reason: string): void => {
      added.push(
        this.add({
          direction: "send",
          kind: "file",
          name,
          remote: join(folder, name),
          local: local(name),
          state: "left_out",
          reason,
        }),
      );
      queued.leftOut += 1;
    };
    for (const item of items) {
      if (item.kind === "link") {
        leave(item.name, LINK_LEFT_OUT);
        continue;
      }
      let name = item.name;
      let replace = false;
      const existing = taken.get(name.toLowerCase());
      if (existing) {
        const choice = choices[name];
        if (choice === undefined) {
          leave(
            name,
            "A name that is taken needs your choice: replace, keep both or skip. It was not sent.",
          );
          continue;
        }
        if (choice === "skip") {
          queued.skipped += 1;
          continue;
        }
        if (choice === "replace") {
          replace = true;
          name = existing.name;
        } else {
          name = keptBeside(
            name,
            item.kind === "folder",
            (candidate) => taken.has(candidate.toLowerCase()) || used.has(candidate.toLowerCase()),
          );
        }
      }
      used.add(name.toLowerCase());
      if (item.kind === "file") {
        added.push(
          this.add(
            {
              direction: "send",
              kind: "file",
              name,
              remote: join(folder, name),
              local: local(item.name),
              bytes: item.len ?? 0,
              state: "waiting",
            },
            { into: folder, as: name, replace },
          ),
        );
        queued.queued += 1;
        continue;
      }
      // A folder goes file by file; one that holds no file is made, and has
      // a row that says so.
      const top = join(folder, name);
      const walk = (children: SimulatedLocal[], into: string, shown: string): number => {
        let files = 0;
        for (const child of children) {
          if (child.kind === "link") {
            leave(`${shown}\\${child.name}`, LINK_LEFT_OUT);
          } else if (child.kind === "folder") {
            files += walk(
              child.children ?? [],
              join(into, child.name),
              `${shown}\\${child.name}`,
            );
          } else {
            added.push(
              this.add(
                {
                  direction: "send",
                  kind: "file",
                  name: `${shown}\\${child.name}`,
                  remote: join(into, child.name),
                  local: local(`${shown}\\${child.name}`),
                  bytes: child.len ?? 0,
                  state: "waiting",
                },
                { into, as: child.name, replace },
              ),
            );
            files += 1;
          }
        }
        return files;
      };
      const files = walk(item.children ?? [], top, name);
      queued.queued += files;
      if (files === 0) {
        added.push(
          this.add(
            {
              direction: "send",
              kind: "folder",
              name,
              remote: top,
              local: local(item.name),
              state: "waiting",
            },
            { make: top },
          ),
        );
        queued.queued += 1;
      }
    }
    this.tell(added);
    this.run();
    return queued;
  }

  /** `explorer_choose_folder`. */
  chooseFolder(): ChosenFolder | null {
    this.refuseIfEnded();
    if (this.chosen === null) return null;
    this.lastChosen += 1;
    this.chosenFolders.set(this.lastChosen, this.chosen);
    return { folder: this.lastChosen, path: this.chosen };
  }

  private root(to: number | undefined): string {
    if (to === undefined) return this.downloads;
    const chosen = this.chosenFolders.get(to);
    if (chosen === undefined) {
      throw { stage: "refused", message: "the folder that was chosen is no longer kept" };
    }
    return chosen;
  }

  /** `explorer_download_check`. */
  downloadCheck(names: TransferName[], to?: number): TransferCheck {
    this.refuseIfEnded();
    const root = this.root(to);
    const conflicts: NameConflict[] = [];
    for (const { name, kind } of names) {
      const at = join(root, name);
      const isFile = this.local.has(at);
      const isFolder = [...this.local.keys()].some((path) => path.startsWith(`${at}\\`));
      if (!isFile && !isFolder) continue;
      conflicts.push({
        name,
        kind,
        existing: isFile ? "file" : "dir",
        replace: (kind === "file") === isFile,
      });
    }
    return { folder: root, conflicts };
  }

  /** `explorer_download`. */
  download(
    folder: string,
    names: TransferName[],
    to: number | undefined,
    choices: Record<string, ConflictChoice>,
  ): TransferQueued {
    this.refuseIfEnded();
    const root = this.root(to);
    const listed = new Map<string, ExplorerEntry>();
    for (const entry of this.folders.get(folder) ?? []) listed.set(entry.name, entry);
    const taken = new Set(this.downloadCheck(names, to).conflicts.map((c) => c.name));
    const queued: TransferQueued = { queued: 0, leftOut: 0, skipped: 0 };
    const added: TransferEntry[] = [];
    for (const { name, kind } of names) {
      const remote = join(folder, name);
      const row = (state: TransferEntry["state"], reason: string): void => {
        added.push(
          this.add({ direction: "download", kind, name, remote, local: root, state, reason }),
        );
        queued.leftOut += 1;
      };
      if (!plainName(name)) {
        row("left_out", "Its name cannot be a name on this computer. It was not copied.");
        continue;
      }
      let as = name;
      let replace = false;
      if (taken.has(name)) {
        const choice = choices[name];
        if (choice === undefined) {
          row(
            "left_out",
            "A name that is taken needs your choice: replace, keep both or skip. It was not copied.",
          );
          continue;
        }
        if (choice === "skip") {
          queued.skipped += 1;
          continue;
        }
        replace = choice === "replace";
        if (choice === "keep_both") {
          as = keptBeside(name, kind === "folder", (candidate) => {
            const at = join(root, candidate);
            return (
              this.local.has(at) ||
              [...this.local.keys()].some((path) => path.startsWith(`${at}\\`))
            );
          });
        }
      }
      const entry = listed.get(name);
      if (!entry) {
        row("failed", "It is no longer in the folder.");
        continue;
      }
      if (entry.kind === "symlink") {
        row("left_out", LINK_LEFT_OUT);
        continue;
      }
      if (entry.kind === "dir") {
        added.push(
          this.add(
            {
              direction: "download",
              kind: "folder",
              name,
              remote,
              local: join(root, as),
              state: "waiting",
            },
            { walk: { from: remote, to: join(root, as), shown: name, replace } },
          ),
        );
      } else {
        added.push(
          this.add(
            {
              direction: "download",
              kind: "file",
              name,
              remote,
              local: join(root, as),
              bytes: entry.len,
              state: "waiting",
            },
            { at: join(root, as), replace },
          ),
        );
      }
      queued.queued += 1;
    }
    this.tell(added);
    this.run();
    return queued;
  }

  /** `explorer_transfers`. */
  transfers(): TransferEntry[] {
    return this.queue.filter((entry) => entry.state !== "listed").map((entry) => ({ ...entry }));
  }

  /** `explorer_cancel_transfer`: a waiting one at once, a running one at its next step. */
  cancelTransfer(id: number): void {
    const entry = this.queue.find((each) => each.id === id);
    if (!entry) return;
    if (entry.state === "waiting") {
      entry.state = "cancelled";
      this.tell([entry]);
    } else if (entry.state === "running") {
      this.cancelling.add(id);
    }
  }

  /** `explorer_cancel_transfers`. */
  cancelTransfers(): void {
    for (const entry of this.queue) this.cancelTransfer(entry.id);
  }

  private cancelling = new Set<number>();

  /** `explorer_show_folder`. */
  showFolder(id: number): void {
    const entry = this.queue.find((each) => each.id === id);
    if (!entry || entry.direction !== "download" || entry.state !== "done") {
      throw {
        stage: "refused",
        message: "only the folder of a copy that arrived can be shown",
      };
    }
    this.shownFolders.push(entry.local.slice(0, entry.local.lastIndexOf("\\")));
  }

  /** Runs the queue to its end, unless a test moves it by hand. */
  private run(): void {
    if (this.manualTransfers) return;
    while (this.queue.some((entry) => entry.state === "waiting" || entry.state === "running")) {
      this.stepTransfers();
    }
  }

  /**
   * Moves the queue one step: the next waiting transfer begins, or the one
   * that runs gets half of its bytes, or it ends. Returns `false` when
   * nothing waits or runs.
   */
  stepTransfers(): boolean {
    const running = this.queue.find((entry) => entry.state === "running");
    if (!running) {
      const next = this.queue.find((entry) => entry.state === "waiting");
      if (!next) return false;
      next.state = "running";
      this.tell([next]);
      return true;
    }
    const job = this.jobs.get(running.id) ?? {};
    if (this.cancelling.delete(running.id)) {
      running.state = "cancelled";
      this.tell([running]);
      return true;
    }
    if (job.walk) {
      // A folder of the customer computer: its files are queued, and its own
      // row gives way to them.
      const added: TransferEntry[] = [];
      const walk = (from: string, to: string, shown: string): void => {
        for (const entry of this.folders.get(from) ?? []) {
          const name = `${shown}\\${entry.name}`;
          const remote = join(from, entry.name);
          const note = (reason: string): void => {
            added.push(
              this.add({
                direction: "download",
                kind: "file",
                name,
                remote,
                local: to,
                state: "left_out",
                reason,
              }),
            );
          };
          if (!plainName(entry.name)) {
            note("Its name cannot be a name on this computer. It was not copied.");
          } else if (entry.kind === "symlink") {
            note(LINK_LEFT_OUT);
          } else if (entry.kind === "dir") {
            walk(remote, join(to, entry.name), name);
          } else if (entry.kind === "file") {
            added.push(
              this.add(
                {
                  direction: "download",
                  kind: "file",
                  name,
                  remote,
                  local: join(to, entry.name),
                  bytes: entry.len,
                  state: "waiting",
                },
                { at: join(to, entry.name), replace: job.walk?.replace ?? false },
              ),
            );
          }
        }
      };
      walk(job.walk.from, job.walk.to, job.walk.shown);
      const files = added.some((entry) => entry.state === "waiting");
      running.state = files ? "listed" : "done";
      this.tell([running, ...added]);
      return true;
    }
    if (job.make !== undefined) {
      this.makeFolder(job.make);
      running.state = "done";
      this.tell([running]);
      return true;
    }
    const failure = this.failing.get(running.remote);
    if (failure !== undefined) {
      running.state = "failed";
      running.reason = failure;
      this.tell([running]);
      return true;
    }
    const bytes = running.bytes ?? 0;
    if (running.done < bytes) {
      running.done = Math.min(bytes, running.done + Math.ceil(bytes / 2));
      this.tell([running]);
      return true;
    }
    if (running.direction === "send" && job.into !== undefined && job.as !== undefined) {
      this.makeFolder(job.into);
      const entries = this.folders.get(job.into) ?? [];
      const at = entries.findIndex((e) => e.name.toLowerCase() === job.as?.toLowerCase());
      if (at >= 0 && !job.replace) {
        running.state = "failed";
        running.reason = "The destination already exists.";
        this.tell([running]);
        return true;
      }
      if (at >= 0) entries.splice(at, 1);
      entries.push(simulatedFile(job.as, bytes));
    } else if (job.at !== undefined) {
      if (this.local.has(job.at) && !job.replace) {
        running.state = "failed";
        running.reason = `${job.at} is already there. It was not replaced.`;
        this.tell([running]);
        return true;
      }
      this.local.set(job.at, bytes);
    }
    running.state = "done";
    this.tell([running]);
    return true;
  }

  /** Makes the folder `path` and the folders above it, as a send does on its way. */
  private makeFolder(path: string): void {
    if (this.folders.has(path)) return;
    const cut = path.lastIndexOf("\\");
    const above = path.slice(0, cut).includes("\\") ? path.slice(0, cut) : `${path.slice(0, cut)}\\`;
    this.makeFolder(above);
    this.folders.set(path, []);
    this.folders.get(above)?.push(simulatedFolder(path.slice(cut + 1)));
  }

  /**
   * The session ends: what runs and what waits is stopped, with the reason,
   * and every later command says that no session is under full control.
   */
  endSession(refusal: SimulatedRefusal = { stage: "no_consented_session", message: "no session is under full control" }): void {
    this.ended = refusal;
    const stopped = this.queue.filter(
      (entry) => entry.state === "waiting" || entry.state === "running",
    );
    for (const entry of stopped) {
      entry.state = "stopped";
      entry.reason = SESSION_ENDED;
    }
    this.tell(stopped);
    if (this.removal) {
      this.removal.cancelled = true;
      this.removal.finish?.();
    }
    this.leave();
  }

  /** `explorer_rename`: in its folder, and never onto a name that is taken. */
  rename(folder: string, name: string, to: string): void {
    this.refuseIfEnded();
    if (/[\\/:*?"<>|]/.test(to) || to.trim() === "") {
      throw { stage: "invalid_path", message: "a name cannot hold any of \\ / : * ? \" < > |" };
    }
    const entries = this.folders.get(folder);
    const entry = entries?.find((each) => each.name === name);
    if (!entries || !entry) throw { stage: "path_not_found", message: "the path does not exist" };
    const refusal = this.held.get(join(folder, name));
    if (refusal !== undefined) throw { stage: "file_in_use", message: refusal };
    if (entries.some((each) => each !== entry && each.name.toLowerCase() === to.toLowerCase())) {
      throw { stage: "already_exists", message: "the destination already exists" };
    }
    if (entry.kind === "dir") {
      const from = join(folder, name);
      for (const path of [...this.folders.keys()]) {
        if (path !== from && !path.startsWith(`${from}\\`)) continue;
        const held = this.folders.get(path) ?? [];
        this.folders.delete(path);
        this.folders.set(join(folder, to) + path.slice(from.length), held);
      }
    }
    entry.name = to;
  }

  /** Removes the folder `path` with everything in it that is not held. */
  private removeTree(path: string, report: DeleteReport): boolean {
    let gone = true;
    for (const entry of (this.folders.get(path) ?? []).slice()) {
      const at = join(path, entry.name);
      const reason = this.held.get(at);
      if (reason !== undefined) {
        report.failed.push({ path: at, reason });
        gone = false;
        continue;
      }
      if (entry.kind === "dir" && !this.removeTree(at, report)) {
        gone = false;
        continue;
      }
      if (entry.kind !== "dir") report.removedFiles += 1;
      const entries = this.folders.get(path) ?? [];
      entries.splice(entries.indexOf(entry), 1);
    }
    if (!gone) {
      report.failed.push({ path, reason: "the folder is not empty" });
      return false;
    }
    this.folders.delete(path);
    report.removedDirs += 1;
    return true;
  }

  /** `explorer_delete`. */
  delete(folder: string, items: DeleteItem[], removal: number): Promise<DeleteReport> {
    if (this.ended) return Promise.reject(this.ended);
    const report: DeleteReport = {
      selected: items.length,
      removed: 0,
      removedFiles: 0,
      removedDirs: 0,
      failed: [],
      failedMore: 0,
      cancelled: false,
      notTried: 0,
    };
    const running = { id: removal, cancelled: false } as {
      id: number;
      cancelled: boolean;
      finish?: () => void;
    };
    this.removal = running;
    const work = (): DeleteReport => {
      this.removal = null;
      if (this.ended) {
        return { ...report, ended: SESSION_ENDED, notTried: items.length };
      }
      const entries = this.folders.get(folder) ?? [];
      items.forEach((item, index) => {
        if (running.cancelled) {
          report.cancelled = true;
          report.notTried += 1;
          return;
        }
        const at = join(folder, item.name);
        this.signals.removal?.({
          removal,
          item: item.name,
          at: index + 1,
          of: items.length,
          removedFiles: report.removedFiles,
          removedDirs: report.removedDirs,
        });
        const entry = entries.find((each) => each.name === item.name);
        const reason = this.held.get(at);
        if (!entry) {
          report.failed.push({ path: at, reason: "the path does not exist" });
        } else if (reason !== undefined) {
          report.failed.push({ path: at, reason });
        } else if ((entry.kind === "dir") !== (item.kind === "dir")) {
          // The kind that was listed is out of date: nothing is removed.
          report.failed.push({ path: at, reason: "the path is not the expected kind" });
        } else if (entry.kind === "dir") {
          if (this.removeTree(at, report)) {
            entries.splice(entries.indexOf(entry), 1);
            report.removed += 1;
          }
        } else {
          entries.splice(entries.indexOf(entry), 1);
          report.removedFiles += 1;
          report.removed += 1;
        }
      });
      return report;
    };
    if (!this.manualDelete) return Promise.resolve(work());
    return new Promise((resolve) => {
      running.finish = () => resolve(work());
    });
  }

  /** `explorer_cancel_delete`. */
  cancelDelete(removal: number): void {
    if (this.removal?.id === removal) this.removal.cancelled = true;
  }

  /** Lets the delete that waits (`manualDelete`) do its work and answer. */
  finishDelete(): void {
    this.removal?.finish?.();
  }
}
