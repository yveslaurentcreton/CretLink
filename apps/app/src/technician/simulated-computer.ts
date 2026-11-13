// A customer computer held in memory, behind the two commands of the file
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

import type {
  ExplorerDrive,
  ExplorerEntry,
  ExplorerEvent,
  ExplorerListing,
} from "../shared/contract";

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

  /** `emit` delivers one step of a listing, as the `console-explorer` event does. */
  constructor(private readonly emit: (event: ExplorerEvent) => void) {}

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
}
