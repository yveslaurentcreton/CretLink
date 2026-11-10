// CretLink application bootstrap (ST-M009-005, ADR-0018).
//
// One window, one role per process. The backend decides the role before the
// window opens — from the command line, the file name, the operating system or
// the start screen — and registers only that role's commands. This module asks
// which one it is and loads only that role's interface, so a customer window
// never even loads the technician's code paths, and the reverse.

import "./base.css";
import { invoke } from "@tauri-apps/api/core";
import { getCurrentWindow } from "@tauri-apps/api/window";

/** What the backend tells the window about this process (`get_role`). */
export interface RoleView {
  /** The interface this process shows. */
  screen: "start" | "technician" | "customer";
  /** The release version, for the start screen. */
  version: string;
  /** Whether Get help exists here (Windows only). */
  can_get_help: boolean;
}

/** The customer's second window, the compact bar (ST-M009-009), loads this
 * same frontend; its label selects the compact view. Every interface renders
 * its own skip link. */
function windowLabel(): string {
  try {
    return getCurrentWindow().label;
  } catch {
    return "main";
  }
}

async function boot(): Promise<void> {
  const view = await invoke<RoleView>("get_role");
  switch (view.screen) {
    case "technician":
      document.title = "CretLink — Give help";
      await (await import("./technician/main")).start({ version: view.version });
      break;
    case "customer": {
      const mini = windowLabel() === "mini";
      document.title = mini ? "CretLink — compact bar" : "CretLink — Get help";
      await (await import("./customer/main")).start({ window: mini ? "mini" : "main" });
      break;
    }
    case "start":
      document.title = "CretLink";
      (await import("./start/start")).start(view);
      break;
  }
}

document.addEventListener("DOMContentLoaded", () => {
  boot().catch((error: unknown) => {
    const root = document.getElementById("app");
    if (root) {
      root.removeAttribute("aria-busy");
      const message = document.createElement("p");
      message.setAttribute("role", "alert");
      message.textContent = `CretLink could not start: ${String(error)}`;
      root.replaceChildren(message);
    }
  });
});
