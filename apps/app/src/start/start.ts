// The start screen's controller (ST-M009-005, ST-M009-006, mock `#start` and
// `#t-install`).
//
// It only reports choices. The backend starts the chosen role in a new
// process and closes this one, so the role stays fixed for the life of each
// process (ADR-0018). Get help opens the customer window, which explains the
// UAC prompt before Windows shows it (`#c-admin`, ST-M009-009). **Run once**
// starts this copy in the technician role; **Install** copies it to the fixed
// per-user location, connects the selected AI clients, starts the installed
// copy and closes this one.

import "./start.css";
import { invoke } from "@tauri-apps/api/core";
import type { AppInfo, ClientId, InstallOptions, InstallOutcome } from "../shared/contract";
import { type ActionEvent, bindActions } from "../shared/dom";
import { morph } from "../shared/morph";
import type { RoleView } from "../main";
import { type StartSnapshot, defaultSelection, renderStart } from "./views";

/** The backend's answer to `choose_role`. */
interface ChooseOutcome {
  outcome: "started" | "failed";
  message: string;
}

let root: HTMLElement | null = null;
let live: HTMLElement | null = null;
let snap: StartSnapshot = {
  page: "start",
  version: "",
  canGetHelp: false,
  app: null,
  options: undefined,
  entry: true,
  selected: [],
};
let lastPage = "";

function set(patch: Partial<StartSnapshot>): void {
  snap = { ...snap, ...patch };
  render();
}

function render(): void {
  if (!root) return;
  const next = renderStart(snap);
  if (!live) {
    root.replaceChildren(next);
    root.removeAttribute("aria-busy");
    live = next;
  } else {
    live = morph(live, next) as HTMLElement;
  }
  // `morph` copies attributes; a checkbox's state is a property.
  for (const box of live.querySelectorAll<HTMLInputElement>('input[type="checkbox"]')) {
    box.checked = box.hasAttribute("checked");
  }
  if (snap.page !== lastPage) {
    lastPage = snap.page;
    const target =
      live.querySelector<HTMLElement>("[data-autofocus]:not([disabled])") ??
      live.querySelector<HTMLElement>("#main");
    target?.focus({ preventScroll: true });
  }
}

function describe(error: unknown): string {
  if (typeof error === "object" && error !== null && "message" in error) {
    return String((error as { message: unknown }).message);
  }
  if (typeof error === "string") return error;
  return "Something went wrong.";
}

async function choose(role: "technician" | "customer"): Promise<void> {
  set({ busy: role, alert: undefined });
  try {
    const result = await invoke<ChooseOutcome>("choose_role", { role });
    if (result.outcome === "failed") set({ busy: undefined, alert: result.message });
    // `started`: this window is closing.
  } catch (error) {
    set({ busy: undefined, alert: `CretLink could not start that: ${describe(error)}` });
  }
}

async function loadApp(): Promise<void> {
  try {
    set({ app: await invoke<AppInfo>("get_app_info") });
  } catch {
    set({ app: null });
  }
}

async function openInstall(): Promise<void> {
  set({ page: "install", alert: undefined, outcome: undefined, options: undefined });
  try {
    const options = await invoke<InstallOptions>("install_options");
    set({ options, selected: defaultSelection(options), entry: options.entry !== undefined });
  } catch (error) {
    set({ options: null, alert: `The install options could not be read: ${describe(error)}` });
  }
}

async function install(): Promise<void> {
  set({ busy: "install", alert: undefined, outcome: undefined });
  try {
    const outcome = await invoke<InstallOutcome>("install_app", {
      entry: snap.entry,
      clients: snap.selected,
    });
    // `started`: the installed copy is starting and this window closes.
    set({ busy: outcome.state === "started" ? "open" : undefined, outcome });
    if (outcome.state === "installed") live?.querySelector<HTMLElement>("#outcome")?.focus();
  } catch (error) {
    set({
      busy: undefined,
      alert: `CretLink was not installed: ${describe(error)} You can run it once without installing instead.`,
    });
  }
}

async function openInstalled(): Promise<void> {
  set({ busy: "open", alert: undefined });
  try {
    await invoke<void>("open_installed");
  } catch (error) {
    set({ busy: undefined, alert: describe(error) });
  }
}

function toggleClient(id: string | undefined): void {
  const client = snap.options?.clients.find((c) => c.id === id);
  if (!client || !client.present) return;
  const selected = snap.selected.includes(client.id)
    ? snap.selected.filter((c) => c !== client.id)
    : [...snap.selected, client.id as ClientId];
  set({ selected });
}

function onAction(event: ActionEvent): void {
  if (snap.busy !== undefined && event.action !== "skip") return;
  switch (event.action) {
    case "skip":
      document.getElementById("main")?.focus();
      break;
    case "choose":
      if (event.arg === "technician" || (event.arg === "customer" && snap.canGetHelp)) {
        void choose(event.arg);
      }
      break;
    case "install-screen":
      void openInstall();
      break;
    case "back":
      set({ page: "start", alert: undefined, outcome: undefined });
      break;
    case "toggle-entry":
      set({ entry: !snap.entry });
      break;
    case "toggle-client":
      toggleClient(event.arg);
      break;
    case "install":
      void install();
      break;
    case "open-installed":
      void openInstalled();
      break;
    default:
      break;
  }
}

export function start(view: RoleView): void {
  root = document.getElementById("app");
  if (!root) return;
  snap = { ...snap, version: view.version, canGetHelp: view.can_get_help };
  bindActions(root, onAction);
  document.addEventListener("keydown", (event) => {
    if (event.key === "Escape" && snap.page === "install" && snap.busy === undefined) {
      set({ page: "start", alert: undefined, outcome: undefined });
    }
  });
  render();
  void loadApp();
}
