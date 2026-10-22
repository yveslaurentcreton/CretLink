// CretLink application bootstrap (ST-M009-005, ADR-0018).
//
// One window, one role per process. The backend decides the role before the
// window opens — from the command line, the file name, the operating system or
// the start screen — and registers only that role's commands. This module asks
// which one it is and loads only that role's interface, so a customer window
// never even loads the technician's code paths, and the reverse.

import "./base.css";
import { invoke } from "@tauri-apps/api/core";

/** What the backend tells the window about this process (`get_role`). */
export interface RoleView {
  /** The interface this process shows. */
  screen: "start" | "technician" | "customer";
  /** The release version, for the start screen. */
  version: string;
  /** Whether Get help exists here (Windows only). */
  can_get_help: boolean;
  /** Set when the customer declined the UAC prompt of Get help. */
  declined: boolean;
}

/** Each role's skip link target, so keyboard users reach its controls first. */
const SKIP_LINKS: Record<RoleView["screen"], [string, string]> = {
  start: ["#choices", "Skip to the choices"],
  technician: ["#session", "Skip to session controls"],
  customer: ["#consent", "Skip to consent controls"],
};

function addSkipLink(screen: RoleView["screen"]): void {
  const [target, label] = SKIP_LINKS[screen];
  const link = document.createElement("a");
  link.className = "skip-link";
  link.href = target;
  link.textContent = label;
  document.body.prepend(link);
}

async function boot(): Promise<void> {
  const view = await invoke<RoleView>("get_role");
  addSkipLink(view.screen);
  switch (view.screen) {
    case "technician":
      document.title = "CretLink — Give help";
      await (await import("./technician/main")).start();
      break;
    case "customer":
      document.title = "CretLink — Get help";
      await (await import("./customer/main")).start();
      break;
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
