// Development-only page for the file explorer (ST-M010-002): the real
// technician controller and views, driven against a customer computer held
// in memory (`simulated-computer.ts`) instead of a backend. Tauri's own
// mocks answer the commands and carry the events, so nothing here needs the
// application, a session or a customer.
//
// The simulated computer has a small tree of an account's folders, a system
// folder of five thousand files, one of sixty thousand, a folder it refuses,
// a link, and an empty drive. A folder arrives page by page with a pause
// between two pages, as over a network, so the loading can be watched.
//
// It also sends, copies, renames and deletes. A browser cannot hand this page
// the files of a real drop, so the bar above the window drops prepared ones:
// two files, a folder with a link in it, a file whose name is taken. The
// transfers move step by step with the same pause; one of them fails, and
// one file of the tree cannot be deleted, so each honest state can be looked
// at. **End the session** stops what runs, as a stop on either side does.

import "../src/base.css";
import { emit } from "@tauri-apps/api/event";
import { mockIPC, mockWindows } from "@tauri-apps/api/mocks";
import type { ConflictChoice, ExplorerEvent } from "../src/shared/contract";
import type { DeleteItem, TransferName } from "../src/technician/api";
import { consoleIn } from "../src/technician/fixtures";
import { start } from "../src/technician/main";
import {
  SimulatedComputer,
  type SimulatedLocal,
  simulatedFile,
  simulatedFiles,
  simulatedFolder,
  simulatedLink,
} from "../src/technician/simulated-computer";

/** The pause between two pages of a folder, in milliseconds (`?pause=`). */
const pause = Number(new URLSearchParams(window.location.search).get("pause") ?? 20);
/** The pause between two steps of a transfer, in milliseconds (`?step=`). */
const step = Number(new URLSearchParams(window.location.search).get("step") ?? 400);

const DOCUMENTS = "C:\\Users\\ann\\Documents";

const computer = new SimulatedComputer(
  (event: ExplorerEvent) => {
    void emit("console-explorer", event);
  },
  {
    transfers: (event) => void emit("console-transfers", event),
    removal: (progress) => void emit("console-removal", progress),
    drop: (offer) => void emit("console-explorer-drop", offer),
  },
);
computer.manual = true;
computer.manualTransfers = true;
computer.folders.set("C:\\Windows", [simulatedFolder("System32"), simulatedFolder("WinSxS")]);
computer.folders.set("C:\\Windows\\WinSxS", simulatedFiles(60_000));
// What the changes can be tried on: a folder to copy with a link in it, a
// file that is in use, and a transfer that fails.
computer.folders.set(`${DOCUMENTS}\\Invoices`, [
  simulatedFile("2026-01.pdf", 48_000),
  simulatedFile("2026-02.pdf", 51_000),
  simulatedFolder("2025"),
  simulatedLink("Shortcut to archive"),
]);
computer.folders.set(`${DOCUMENTS}\\Invoices\\2025`, [simulatedFile("2025-12.pdf", 44_000)]);
computer.held.set(`${DOCUMENTS}\\report.pdf`, "the file is in use");
computer.failing.set(`${DOCUMENTS}\\locked.docx`, "The file is in use.");
computer.picked.files = [{ name: "notes for Ann.txt", kind: "file", len: 1_200 }];
computer.picked.folder = [
  {
    name: "Drivers",
    kind: "folder",
    children: [
      { name: "chipset.zip", kind: "file", len: 9_400_000 },
      { name: "readme.txt", kind: "file", len: 800 },
    ],
  },
];
computer.chosen = "C:\\Users\\sam\\Documents\\From customers";

/** Delivers the pages of the listing that runs, one after each pause. */
function deliver(): void {
  if (computer.open.length === 0) return;
  computer.step();
  if (computer.open.length > 0) window.setTimeout(deliver, pause);
}

let moving = false;

/** Moves the queue one step after each pause, while something waits or runs. */
function move(): void {
  if (moving) return;
  moving = true;
  const next = (): void => {
    if (computer.stepTransfers()) window.setTimeout(next, step);
    else moving = false;
  };
  window.setTimeout(next, step);
}

const state = consoleIn(
  { phase: "full_control" },
  { session: true, target: true, displayName: "Sam" },
);

mockWindows("main");
mockIPC(
  (cmd, args) => {
    const payload = (args ?? {}) as Record<string, unknown>;
    switch (cmd) {
      case "get_state":
        return state;
      case "explorer_drives":
        return computer.listDrives();
      case "explorer_open": {
        const answer = computer.openPath(String(payload.path), Number(payload.listing));
        window.setTimeout(deliver, pause);
        return answer;
      }
      case "explorer_pick_upload":
        return computer.pickUpload(payload.kind as "files" | "folder");
      case "explorer_send_check":
        return computer.sendCheck(Number(payload.offer), String(payload.folder));
      case "explorer_send": {
        const queued = computer.send(
          Number(payload.offer),
          String(payload.folder),
          payload.choices as Record<string, ConflictChoice>,
        );
        move();
        return queued;
      }
      case "explorer_choose_folder":
        return computer.chooseFolder();
      case "explorer_download_check":
        return computer.downloadCheck(
          payload.names as TransferName[],
          payload.to as number | undefined,
        );
      case "explorer_download": {
        const queued = computer.download(
          String(payload.folder),
          payload.names as TransferName[],
          payload.to as number | undefined,
          payload.choices as Record<string, ConflictChoice>,
        );
        move();
        return queued;
      }
      case "explorer_transfers":
        return computer.transfers();
      case "explorer_cancel_transfer":
        return computer.cancelTransfer(Number(payload.transfer));
      case "explorer_cancel_transfers":
        return computer.cancelTransfers();
      case "explorer_show_folder":
        return computer.showFolder(Number(payload.transfer));
      case "explorer_rename":
        return computer.rename(String(payload.folder), String(payload.name), String(payload.to));
      case "explorer_delete":
        return computer.delete(
          String(payload.folder),
          payload.items as DeleteItem[],
          Number(payload.removal),
        );
      case "explorer_cancel_delete":
        return computer.cancelDelete(Number(payload.removal));
      default:
        // As Tauri answers a command the backend does not register.
        throw `Command ${cmd} not found`;
    }
  },
  { shouldMockEvents: true },
);

/** What the bar above the window drops, by the name of its button. */
const DROPS: Record<string, SimulatedLocal[]> = {
  "Drop two files": [
    { name: "setup.exe", kind: "file", len: 52_000_000 },
    { name: "locked.docx", kind: "file", len: 18_000 },
  ],
  "Drop a folder": [
    {
      name: "Tools",
      kind: "folder",
      children: [
        { name: "diag.exe", kind: "file", len: 3_100_000 },
        { name: "notes.txt", kind: "file", len: 900 },
        { name: "Shortcut", kind: "link" },
        { name: "logs", kind: "folder", children: [{ name: "run.log", kind: "file", len: 64_000 }] },
      ],
    },
  ],
  "Drop names that are taken": [
    { name: "report.pdf", kind: "file", len: 5_000 },
    { name: "a.txt", kind: "file", len: 30 },
    { name: "Invoices", kind: "folder", children: [{ name: "2026-03.pdf", kind: "file", len: 47_000 }] },
  ],
};

const bar = document.getElementById("dev-bar");
if (bar) {
  for (const [label, items] of Object.entries(DROPS)) {
    const button = document.createElement("button");
    button.type = "button";
    button.textContent = label;
    button.addEventListener("click", () => {
      void emit("tauri://drag-enter", {});
      window.setTimeout(() => {
        void emit("tauri://drag-drop", {});
        computer.drop(items);
      }, 600);
    });
    bar.append(button);
  }
  const end = document.createElement("button");
  end.type = "button";
  end.textContent = "End the session";
  end.addEventListener("click", () => computer.endSession());
  bar.append(end);
}

void start({ version: "dev" });
