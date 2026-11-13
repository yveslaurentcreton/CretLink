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

import "../src/base.css";
import { emit } from "@tauri-apps/api/event";
import { mockIPC, mockWindows } from "@tauri-apps/api/mocks";
import type { ExplorerEvent } from "../src/shared/contract";
import { consoleIn } from "../src/technician/fixtures";
import { start } from "../src/technician/main";
import {
  SimulatedComputer,
  simulatedFiles,
  simulatedFolder,
} from "../src/technician/simulated-computer";

/** The pause between two pages of a folder, in milliseconds (`?pause=`). */
const pause = Number(new URLSearchParams(window.location.search).get("pause") ?? 20);

const computer = new SimulatedComputer((event: ExplorerEvent) => {
  void emit("console-explorer", event);
});
computer.manual = true;
computer.folders.set("C:\\Windows", [simulatedFolder("System32"), simulatedFolder("WinSxS")]);
computer.folders.set("C:\\Windows\\WinSxS", simulatedFiles(60_000));

/** Delivers the pages of the listing that runs, one after each pause. */
function deliver(): void {
  if (computer.open.length === 0) return;
  computer.step();
  if (computer.open.length > 0) window.setTimeout(deliver, pause);
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
      default:
        // As Tauri answers a command the backend does not register.
        throw `Command ${cmd} not found`;
    }
  },
  { shouldMockEvents: true },
);

void start({ version: "dev" });
