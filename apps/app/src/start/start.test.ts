// Rendering tests for the start screen (`#start`) and the install screen
// (`#t-install`), derived from the acceptance criteria of ST-M009-006. Both are
// pure functions of a snapshot, rendered in jsdom without Tauri.

import { afterEach, describe, expect, it } from "vitest";
import type { AiClientStatus, AppInfo, InstallOptions } from "../shared/contract";
import { type StartSnapshot, defaultSelection, renderStart } from "./views";

const XSS = '<img src=x onerror="alert(1)">';

const RUN_ONCE: AppInfo = {
  hostname: "TECH-LAPTOP",
  os: "Windows 11 Pro 25H2",
  version: "0.22.0",
  install: "run_once",
  location: "C:\\Users\\sam\\Downloads",
  auditDir: "C:\\Users\\sam\\AppData\\Local\\CretLink\\audit",
};

const INSTALLED: AppInfo = {
  ...RUN_ONCE,
  install: "installed",
  location: "C:\\Users\\sam\\AppData\\Local\\Programs\\CretLink",
};

const EXE = "C:\\Users\\sam\\AppData\\Local\\Programs\\CretLink\\cretlink.exe";

const CLIENTS: AiClientStatus[] = [
  {
    id: "claude",
    label: "Claude Code",
    present: true,
    configPath: "C:\\Users\\sam\\.claude.json",
    registration: { state: "absent" },
    installPlan: [`claude mcp add --scope user cretlink -- ${EXE} mcp`],
    uninstallPlan: [],
  },
  {
    id: "codex",
    label: "Codex CLI",
    present: true,
    configPath: "C:\\Users\\sam\\.codex\\config.toml",
    registration: { state: "other_command", command: "D:\\old\\cretlink.exe mcp" },
    installPlan: [`codex mcp add cretlink -- ${EXE} mcp`],
    uninstallPlan: ["codex mcp remove cretlink"],
  },
  {
    id: "gemini",
    label: "Gemini CLI",
    present: false,
    configPath: "C:\\Users\\sam\\.gemini\\settings.json",
    registration: { state: "absent" },
    installPlan: [],
    uninstallPlan: [],
  },
];

const OPTIONS: InstallOptions = {
  location: "C:\\Users\\sam\\AppData\\Local\\Programs\\CretLink",
  entry: "start_menu",
  entryPath:
    "C:\\Users\\sam\\AppData\\Roaming\\Microsoft\\Windows\\Start Menu\\Programs\\CretLink.lnk",
  clients: CLIENTS,
  update: false,
  running: false,
};

function snapshot(patch: Partial<StartSnapshot> = {}): StartSnapshot {
  return {
    page: "start",
    version: "0.22.0",
    canGetHelp: true,
    app: RUN_ONCE,
    options: undefined,
    entry: true,
    selected: [],
    ...patch,
  };
}

function show(patch: Partial<StartSnapshot> = {}): HTMLElement {
  const root = renderStart(snapshot(patch));
  document.body.replaceChildren(root);
  return root;
}

function text(el: Element | null | undefined): string {
  return (el?.textContent ?? "").replace(/\s+/g, " ").trim();
}

function button(root: HTMLElement, name: string): HTMLButtonElement | undefined {
  return Array.from(root.querySelectorAll("button")).find((b) => text(b) === name);
}

function checkbox(root: HTMLElement, id: string): HTMLInputElement {
  const box = root.querySelector<HTMLInputElement>(`#${id}`);
  if (!box) throw new Error(`no checkbox ${id}`);
  return box;
}

afterEach(() => document.body.replaceChildren());

describe("start screen (#start)", () => {
  it("offers Get help and, for Give help, Install for me and Run once", () => {
    const root = show();
    expect(text(root.querySelector("h1"))).toBe("What do you want to do?");
    expect(text(root.querySelector("#get-title"))).toBe("Get help");
    const install = button(root, "Install for me");
    const once = button(root, "Run once without installing");
    expect(install?.dataset.action).toBe("install-screen");
    expect(once?.dataset.action).toBe("choose");
    expect(once?.dataset.arg).toBe("technician");
    expect(button(root, "Get help")?.dataset.arg).toBe("customer");
  });

  it("shows the version and the folder it runs from", () => {
    const root = show();
    expect(text(root.querySelector(".where"))).toBe(
      "CretLink 0.22.0 · running from C:\\Users\\sam\\Downloads",
    );
  });

  it("an installed copy says so and does not offer to install again", () => {
    const root = show({ app: INSTALLED });
    expect(text(root.querySelector(".where"))).toBe(
      "CretLink 0.22.0 · installed in C:\\Users\\sam\\AppData\\Local\\Programs\\CretLink",
    );
    expect(text(root.querySelector(".installed"))).toContain("Installed");
    expect(button(root, "Install for me")).toBeUndefined();
    expect(button(root, "Run once without installing")).toBeUndefined();
    expect(button(root, "Give help")?.dataset.arg).toBe("technician");
    // Get help stays available on Windows.
    expect(button(root, "Get help")).toBeDefined();
  });

  it("on macOS and Linux shows Give help only", () => {
    const root = show({ canGetHelp: false });
    expect(root.querySelector(".card.get")).toBeNull();
    expect(button(root, "Get help")).toBeUndefined();
    expect(button(root, "Install for me")?.hasAttribute("data-autofocus")).toBe(true);
    expect(button(root, "Run once without installing")).toBeDefined();
  });

  it("before this copy is known, still offers both ways", () => {
    const root = show({ app: null });
    expect(text(root.querySelector(".where"))).toBe("CretLink 0.22.0");
    expect(button(root, "Install for me")).toBeDefined();
  });

  it("explains a failed start in an alert, and disables the choices while busy", () => {
    const root = show({ alert: "CretLink could not start", busy: "customer" });
    expect(text(root.querySelector('[role="alert"]'))).toBe("CretLink could not start");
    for (const b of root.querySelectorAll("button")) expect(b.disabled).toBe(true);
  });

  it("has a skip link, a main region and only native controls", () => {
    for (const page of ["start", "install"] as const) {
      const root = show({ page, options: OPTIONS, selected: ["claude"] });
      expect(root.classList.contains("cl-ui")).toBe(true);
      expect(root.querySelector("a.skip-link")?.getAttribute("href")).toBe("#main");
      expect(root.querySelector("#main")).not.toBeNull();
      expect(root.querySelector('[role="alert"]')).not.toBeNull();
      for (const control of root.querySelectorAll("[data-action]")) {
        expect(["BUTTON", "A", "INPUT"]).toContain(control.tagName);
      }
    }
  });
});

describe("install screen (#t-install)", () => {
  function install(patch: Partial<StartSnapshot> = {}): HTMLElement {
    return show({
      page: "install",
      options: OPTIONS,
      selected: defaultSelection(OPTIONS),
      ...patch,
    });
  }

  it("shows the location and the Start-menu option", () => {
    const root = install();
    expect(text(root.querySelector("h1"))).toBe("Install CretLink for you");
    expect(text(root.querySelector(".location"))).toContain(OPTIONS.location);
    const entry = checkbox(root, "opt-entry");
    expect(entry.hasAttribute("checked")).toBe(true);
    expect(text(entry.closest("label"))).toContain("Add to the Start menu");
    expect(text(entry.closest("label"))).toContain("CretLink.lnk");
    expect(entry.dataset.action).toBe("toggle-entry");
  });

  it("offers a desktop entry on Linux and no entry on macOS", () => {
    const linux = install({
      options: {
        ...OPTIONS,
        entry: "desktop_entry",
        entryPath: "/home/ann/.local/share/applications/cretlink.desktop",
      },
    });
    expect(text(checkbox(linux, "opt-entry").closest("label"))).toContain(
      "Add to the applications menu",
    );
    const { entry: _entry, entryPath: _path, ...mac } = OPTIONS;
    const macos = install({ options: mac });
    expect(macos.querySelector("#opt-entry")).toBeNull();
  });

  it("shows each client with its detection result and the change install makes", () => {
    const root = install();
    const claude = checkbox(root, "opt-claude");
    expect(text(claude.closest("label"))).toBe(
      `Claude Code Found · runs claude mcp add --scope user cretlink -- ${EXE} mcp`,
    );
    expect(claude.hasAttribute("checked")).toBe(true);
    expect(claude.disabled).toBe(false);
    expect(claude.dataset.arg).toBe("claude");

    const codex = checkbox(root, "opt-codex");
    expect(text(codex.closest("label"))).toContain(
      "registered to another program, D:\\old\\cretlink.exe mcp",
    );
    expect(text(codex.closest("label"))).toContain(`runs codex mcp add cretlink -- ${EXE} mcp`);
    // Replacing another program's registration is opt-in.
    expect(codex.hasAttribute("checked")).toBe(false);

    const gemini = checkbox(root, "opt-gemini");
    expect(text(gemini.closest("label"))).toBe("Gemini CLI Not found · you can connect it later");
    expect(gemini.disabled).toBe(true);
    expect(gemini.hasAttribute("checked")).toBe(false);

    // Each client is selectable: the snapshot's selection is what is shown.
    const chosen = install({ selected: ["codex"] });
    expect(checkbox(chosen, "opt-codex").hasAttribute("checked")).toBe(true);
    expect(checkbox(chosen, "opt-claude").hasAttribute("checked")).toBe(false);
  });

  it("selects present clients that are not registered elsewhere by default", () => {
    expect(defaultSelection(OPTIONS)).toEqual(["claude"]);
    const already: InstallOptions = {
      ...OPTIONS,
      clients: [
        {
          ...(CLIENTS[0] as AiClientStatus),
          registration: { state: "this_executable", command: `${EXE} mcp` },
          installPlan: [],
        },
      ],
    };
    expect(defaultSelection(already)).toEqual(["claude"]);
    const root = install({ options: already, selected: ["claude"] });
    expect(text(checkbox(root, "opt-claude").closest("label"))).toContain(
      "already connected to the installed copy",
    );
  });

  it("Back returns to the start screen; Install installs", () => {
    const root = install();
    expect(button(root, "Back")?.dataset.action).toBe("back");
    expect(button(root, "Install")?.dataset.action).toBe("install");
    expect(button(root, "Install")?.disabled).toBe(false);
  });

  it("says when the installed copy is running and does not offer Install", () => {
    const root = install({ options: { ...OPTIONS, running: true } });
    expect(text(root.querySelector(".note"))).toContain("The installed CretLink is running");
    expect(button(root, "Install")?.disabled).toBe(true);
    expect(button(root, "Back")?.disabled).toBe(false);
  });

  it("says that an update replaces the installed copy", () => {
    const root = install({ options: { ...OPTIONS, update: true } });
    expect(text(root.querySelector("main"))).toContain("install replaces it with this version");
  });

  it("is honest while loading and when the options could not be read", () => {
    const loading = install({ options: undefined });
    expect(loading.querySelector('[aria-busy="true"]')).not.toBeNull();
    expect(button(loading, "Install")?.disabled).toBe(true);
    const failed = install({ options: null, alert: "The install options could not be read: x" });
    expect(text(failed.querySelector('[role="alert"]'))).toContain("could not be read");
    expect(button(failed, "Install")?.disabled).toBe(true);
    expect(button(failed, "Back")?.disabled).toBe(false);
  });

  it("disables every choice while installing", () => {
    const root = install({ busy: "install" });
    expect(button(root, "Installing…")?.disabled).toBe(true);
    for (const box of root.querySelectorAll("input")) expect(box.disabled).toBe(true);
  });

  it("lists what did not work after an install and offers to open the installed copy", () => {
    const root = install({
      outcome: {
        state: "installed",
        location: OPTIONS.location,
        failures: ["Codex CLI: 'codex mcp add' failed: denied"],
      },
    });
    const note = root.querySelector("#outcome");
    expect(text(note)).toContain(`CretLink is installed in ${OPTIONS.location}, but:`);
    expect(text(note)).toContain("Codex CLI: 'codex mcp add' failed: denied");
    expect(button(root, "Open CretLink")?.dataset.action).toBe("open-installed");
    expect(button(root, "Install")).toBeUndefined();
  });

  it("writes untrusted text as text", () => {
    const root = install({
      options: {
        ...OPTIONS,
        location: XSS,
        clients: [
          {
            ...(CLIENTS[1] as AiClientStatus),
            registration: { state: "other_command", command: XSS },
            installPlan: [XSS],
          },
        ],
      },
    });
    expect(root.querySelector("img")).toBeNull();
    expect(root.textContent).toContain(XSS);
  });
});
