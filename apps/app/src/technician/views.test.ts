// Rendering tests for every technician screen and state (ST-M009-008),
// derived from the story's acceptance criteria. Each screen is a pure function
// of a snapshot, rendered in jsdom without Tauri.

import { afterEach, describe, expect, it } from "vitest";
import { DISCLOSURE_TEXT } from "./disclosure";
import { FIXTURES, XSS } from "./fixtures";
import type { TechnicianSnapshot } from "./model";
import { renderTechnician } from "./views/shell";

function show(name: string, patch: Partial<TechnicianSnapshot> = {}): HTMLElement {
  const fixture = FIXTURES[name];
  if (!fixture) throw new Error(`no fixture ${name}`);
  const root = renderTechnician({ ...fixture, ...patch });
  document.body.replaceChildren(root);
  return root;
}

// The visible text, with the cells of key/value lists, tables and stats
// separated by a space as a reader would see them.
function text(el: Element | null | undefined): string {
  if (!el) return "";
  const copy = el.cloneNode(true) as Element;
  for (const cell of copy.querySelectorAll("dt, dd, td, th, h4, .stats b")) cell.after(" ");
  return (copy.textContent ?? "").replace(/\s+/g, " ").trim();
}

function buttons(root: HTMLElement): HTMLButtonElement[] {
  return Array.from(root.querySelectorAll("button"));
}

function buttonNamed(root: HTMLElement, name: string | RegExp): HTMLButtonElement | undefined {
  return buttons(root).find((b) => {
    const label = b.getAttribute("aria-label") ?? text(b);
    return typeof name === "string"
      ? label === name || text(b) === name
      : name.test(label) || name.test(text(b));
  });
}

afterEach(() => document.body.replaceChildren());

describe("every screen", () => {
  it.each(Object.keys(FIXTURES))(
    "%s: renders with a skip link, a main region, alert and live regions",
    (name) => {
      const root = show(name);
      const skip = root.querySelector("a.skip-link");
      expect(skip?.getAttribute("href")).toBe("#main");
      expect(root.querySelector("#main")).not.toBeNull();
      expect(root.querySelector('[role="alert"]')).not.toBeNull();
      expect(root.querySelector('[aria-live="polite"]')?.textContent).not.toBe("");
      // Every control is a native, keyboard-reachable element.
      for (const control of root.querySelectorAll("[data-action]")) {
        expect(["BUTTON", "A"]).toContain(control.tagName);
      }
      // No state is shown by colour alone: every pill and band carries words.
      for (const marker of root.querySelectorAll(".pill, .band")) {
        expect(text(marker)).not.toBe("");
      }
    },
  );
});

describe("window", () => {
  it("has Home, Sessions, AI assistants and Settings in the navigation", () => {
    const root = show("home");
    const nav = root.querySelector("nav.side");
    expect(Array.from(nav?.querySelectorAll("button") ?? []).map(text)).toEqual([
      "Home",
      "Sessions",
      "AI assistants",
      "Settings",
    ]);
    expect(nav?.querySelector('[aria-current="page"]')?.textContent).toBe("Home");
  });

  it("has no bar of its own under the title bar on any screen, and draws no logo", () => {
    for (const name of Object.keys(FIXTURES)) {
      const root = show(name);
      expect(root.querySelector("header, .appbar, .logo"), name).toBeNull();
      // Only the skip link and the alert region come before the screen: the
      // navigation with the page, or the live session with its band.
      const children = Array.from(root.children);
      expect(children[0]?.classList.contains("skip-link"), name).toBe(true);
      expect(children[1]?.getAttribute("role"), name).toBe("alert");
      expect(children[2]?.matches(".tshell, main.live-screen"), name).toBe(true);
    }
  });

  it("names this computer and this copy in the foot of the navigation", () => {
    const foot = show("home").querySelector("nav.side .foot");
    // The host name with the operating system by name, then the version and
    // whether this copy is installed.
    expect(text(foot?.querySelector(".this-computer"))).toBe(
      "This computer: TECH-LAPTOP · Windows 11 Pro 25H2",
    );
    expect(text(foot?.querySelector(".this-copy"))).toBe("CretLink 0.22.0 · installed");
    // "This computer:" is for screen readers; the eye sees the two lines.
    expect(foot?.querySelector(".this-computer > .sr-only")?.textContent).toBe("This computer: ");
    // When a line has to break, its second part stays in one piece.
    expect(Array.from(foot?.querySelectorAll(".keep") ?? []).map(text)).toEqual([
      "Windows 11 Pro 25H2",
      "installed",
    ]);
    expect(text(show("settings-run-once").querySelector("nav.side .foot .this-copy"))).toBe(
      "CretLink 0.22.0 · runs once",
    );
  });

  it("falls back to the console host and the role version when the app info is unavailable", () => {
    const foot = show("home-unavailable").querySelector("nav.side .foot");
    expect(text(foot?.querySelector(".this-computer"))).toBe(
      "This computer: TECH-LAPTOP · Windows 11 Pro 25H2",
    );
    expect(text(foot?.querySelector(".this-copy"))).toBe("CretLink 0.22.0");
  });

  it("says just This computer until the backend has named it", () => {
    const fixture = FIXTURES["home-unavailable"]!;
    const root = show("home-unavailable", {
      console: { ...fixture.console, technician: { label: "This computer", hostname: "", os: "" } },
      version: "",
    });
    const foot = root.querySelector("nav.side .foot");
    expect(text(foot?.querySelector(".this-computer"))).toBe("This computer");
    expect(text(foot?.querySelector(".this-copy"))).toBe("CretLink");
  });

  it("keeps the full-control band at the top of the live screen, without a navigation", () => {
    const root = show("live");
    expect(root.querySelector("main.live-screen")?.firstElementChild?.id).toBe("control-band");
    expect(root.querySelector("nav.side")).toBeNull();
  });
});

describe("home (#t-home)", () => {
  it("offers Start a session as the only primary action", () => {
    const root = show("home");
    expect(text(root.querySelector("h2"))).toBe("Ready to help");
    const primaries = root.querySelectorAll(".btn.primary");
    expect(primaries).toHaveLength(1);
    expect(text(primaries[0])).toBe("Start a session");
    expect(primaries[0]?.getAttribute("data-action")).toBe("start");
  });

  it("lists the three most recent sessions with how they ended and cleanup in words", () => {
    const root = show("home", { sessions: FIXTURES.sessions?.sessions ?? null });
    const rows = root.querySelectorAll(".list.sessions .session-row");
    expect(rows).toHaveLength(3);
    expect(text(rows[0])).toContain("ACME-PC-07");
    expect(text(rows[0])).toContain("ended by the customer");
    expect(text(rows[0])).toContain("Cleaned up");
    expect(text(rows[1])).toContain("1 residual");
    expect(text(rows[1])).toContain("ended by you");
    expect(text(rows[2])).toContain("Cleanup failed");
    expect(text(rows[2])).toContain("the connection was lost");
    expect(rows[0]?.getAttribute("data-action")).toBe("open-report");
  });

  it("shows each AI client's status with Connect for a present, unregistered client", () => {
    const root = show("home");
    const card = Array.from(root.querySelectorAll(".card")).find(
      (c) => text(c.querySelector("h4")) === "AI assistants",
    );
    expect(text(card)).toContain("Claude Code");
    expect(text(card)).toContain("Connected");
    expect(text(card)).toContain("Registered to another program");
    const connect = buttonNamed(root, "Connect Gemini CLI");
    expect(connect?.dataset.action).toBe("ai-connect");
    expect(connect?.dataset.arg).toBe("gemini");
    expect(connect?.classList.contains("primary")).toBe(false);
  });

  it("has no list of discovered computers (REQ-F-004)", () => {
    const root = show("home", {
      console: { ...FIXTURES.compare!.console, phase: { phase: "idle" } },
    });
    expect(text(root)).not.toContain("10.0.0.57");
  });

  it("says calmly when history or clients are not available, and when there are none", () => {
    expect(text(show("home-unavailable"))).toContain("The session history is not available yet.");
    expect(text(show("home-unavailable"))).toContain(
      "The AI assistant status is not available yet.",
    );
    const empty = show("home-empty");
    expect(text(empty)).toContain("No sessions yet.");
    expect(text(empty)).toContain("Configuration can't be read");
    expect(text(empty)).toContain("Not found");
  });
});

describe("sessions", () => {
  it("lists every session newest first, each opening its report", () => {
    const root = show("sessions");
    const rows = Array.from(root.querySelectorAll(".session-row"));
    expect(rows).toHaveLength(5);
    expect(rows.map((r) => r.getAttribute("data-arg"))).toEqual(
      FIXTURES.sessions!.sessions!.map((s) => s.id),
    );
    expect(text(rows[3])).toContain("Not sealed");
    expect(text(rows[3])).toContain("Cleanup unknown");
    expect(text(rows[4])).toContain("Audit file broken");
    expect(text(rows[4])).toContain("Unknown computer");
  });

  it("says when history is unavailable or empty", () => {
    expect(text(show("sessions", { sessions: null }))).toContain("not available yet");
    expect(text(show("sessions", { sessions: [] }))).toContain("No sessions yet");
  });
});

describe("waiting (#t-wait)", () => {
  it("reads No access, offers Cancel (Esc), names what the customer picks and labels the code", () => {
    const root = show("wait");
    expect(text(root.querySelector(".pill"))).toContain("No access");
    const cancel = buttonNamed(root, /Cancel/);
    expect(cancel?.dataset.action).toBe("stop");
    expect(cancel?.getAttribute("aria-keyshortcuts")).toBe("Escape");
    expect(text(root)).toContain("select Sam · TECH-LAPTOP");
    expect(text(root)).toContain("only if their app doesn't find this computer");
    expect(text(root.querySelector(".conn-code"))).toBe("6RRR-2H8F");
    expect(text(root)).toContain("10.0.0.42:7000");
    const copy = buttonNamed(root, "Copy the connection code");
    expect(copy?.dataset.action).toBe("copy");
    expect(copy?.dataset.arg).toBe("6RRR-2H8F");
  });

  it("falls back to the hostname without a display name, and offers no relay (M007)", () => {
    const root = show("wait-no-name");
    expect(text(root)).toContain("select TECH-LAPTOP.");
    expect(text(root).toLowerCase()).not.toContain("relay");
  });

  it("keeps Cancel while connecting", () => {
    const root = show("connecting");
    expect(buttonNamed(root, /Cancel/)?.dataset.action).toBe("stop");
    expect(text(root.querySelector(".pill"))).toContain("No access");
  });

  it("names the adapter of the address it waits on (ST-M009-016)", () => {
    const root = show("wait");
    expect(text(root.querySelector(".conn-addr"))).toContain("Wi-Fi · 10.0.0.42:7000");
  });

  it("offers the other networks behind a button that says whether the list is open", () => {
    const closed = show("wait");
    const toggle = closed.querySelector<HTMLButtonElement>("#net-toggle");
    expect(text(toggle)).toBe("Wait on another network");
    expect(toggle?.dataset.action).toBe("networks");
    expect(toggle?.getAttribute("aria-expanded")).toBe("false");
    expect(toggle?.getAttribute("aria-controls")).toBe("net-list");
    expect(closed.querySelector("#net-list")).toBeNull();

    const open = show("wait-networks");
    expect(open.querySelector("#net-toggle")?.getAttribute("aria-expanded")).toBe("true");
    const rows = Array.from(open.querySelectorAll("#net-list li"));
    // The network it waits on is not offered again.
    expect(rows.map((row) => text(row.querySelector(".net-name")))).toEqual([
      "vEthernet (Default Switch) · 172.20.0.1",
      "VPN · 192.168.1.20",
    ]);
    const choose = rows[0]?.querySelector("button");
    expect(text(choose)).toBe("Wait here");
    expect(choose?.dataset.action).toBe("network");
    expect(choose?.dataset.arg).toBe("172.20.0.1");
    expect(choose?.getAttribute("aria-label")).toBe(
      "Wait on vEthernet (Default Switch), 172.20.0.1",
    );
    // A tunnel is named in words, and the list says what changing does.
    expect(text(rows[1])).toContain("point-to-point link");
    expect(text(rows[0])).not.toContain("point-to-point link");
    expect(text(open.querySelector("#net-list"))).toContain("The connection code changes with it");
  });

  it("offers no choice with one network, and shows the address alone when none is known", () => {
    const one = show("wait-one-network");
    expect(one.querySelector("#net-toggle")).toBeNull();
    expect(text(one.querySelector(".conn-addr"))).toContain("Wi-Fi · 10.0.0.42:7000");

    const unknown = show("wait-networks-unknown");
    expect(unknown.querySelector("#net-toggle")).toBeNull();
    expect(unknown.querySelector(".net-adapter")).toBeNull();
    expect(text(unknown.querySelector(".conn-addr"))).toContain("10.0.0.42:7000");
  });

  it("shows no code while the network changes", () => {
    const root = show("wait-switching");
    expect(root.querySelector(".screen.wait")).not.toBeNull();
    expect(text(root.querySelector('[role="status"].conn-switching'))).toBe("Changing the network…");
    expect(root.querySelector(".conn-code")).toBeNull();
    expect(buttonNamed(root, "Copy the connection code")).toBeUndefined();
    expect(root.querySelector("#net-toggle")).toBeNull();
  });

  it("writes an adapter name as text", () => {
    const root = show("wait-networks", {
      networks: [
        { address: "10.0.0.42", adapter: XSS, pointToPoint: false },
        { address: "172.20.0.1", adapter: XSS, pointToPoint: false },
      ],
    });
    expect(root.querySelector("img")).toBeNull();
    expect(root.querySelector("script")).toBeNull();
    expect(text(root.querySelector(".conn-addr"))).toContain(XSS);
  });
});

describe("compare (#t-sac)", () => {
  it("shows the code large and grouped 3 + 3 with the reported identity and both verdicts, without expiry", () => {
    const root = show("compare");
    const sac = root.querySelector(".sac");
    expect(sac?.getAttribute("aria-label")).toBe("Approval code 482 913");
    expect(
      Array.from(sac?.querySelectorAll("span") ?? [])
        .map(text)
        .join(""),
    ).toBe("482913");
    expect(sac?.querySelector("i")).not.toBeNull();
    expect(text(root)).toContain("ACME-PC-07 · Windows 11 Pro 24H2 · elevated");
    expect(buttonNamed(root, "Codes don't match")?.dataset.action).toBe("mismatch");
    expect(buttonNamed(root, "Codes match")?.dataset.action).toBe("compare");
    expect(text(root).toLowerCase()).not.toContain("expire");
    // Nothing invites a blind confirmation: Codes match is not focused first.
    expect(buttonNamed(root, "Codes match")?.hasAttribute("data-autofocus")).toBe(false);
  });
});

describe("customer deciding (#t-consent)", () => {
  it("says only the customer can grant control, shows the verbatim disclosure, and keeps Stop", () => {
    const root = show("consent");
    expect(text(root)).toContain("Only the customer can grant control");
    expect(root.querySelector(".disclose")?.textContent).toBe(DISCLOSURE_TEXT);
    expect(buttonNamed(root, /Stop/)?.dataset.action).toBe("stop");
    expect(buttonNamed(root, "Simulate customer consent")).toBeUndefined();
  });

  it("offers simulated consent only for a simulated target", () => {
    expect(
      buttonNamed(show("consent-simulated"), "Simulate customer consent")?.dataset.action,
    ).toBe("consent");
  });
});

describe("live session (#t-live)", () => {
  it("shows the full-control band, the identity line and Stop session (Esc)", () => {
    const root = show("live");
    const band = text(root.querySelector(".band.full"));
    expect(band).toContain("FULL CONTROL");
    expect(band).toContain("ACME-PC-07 · session 7f3a9c · since 14:02");
    expect(text(root.querySelector(".sessbar .meta"))).toBe(
      "Windows 11 Pro 24H2 · x64 · elevated · agent 0.22.0",
    );
    const stop = buttonNamed(root, "Stop the session with ACME-PC-07");
    expect(stop?.dataset.action).toBe("stop");
    expect(stop?.getAttribute("aria-keyshortcuts")).toBe("Escape");
    expect(root.querySelector("nav.side")).toBeNull();
  });

  it("shows the account when the agent reports one", () => {
    const fixture = FIXTURES.live!;
    const root = show("live", {
      last: { ...fixture.last!, target: { ...fixture.last!.target!, account: "ACME\\jdoe" } },
      console: { ...fixture.console, targets: [] },
    });
    expect(text(root.querySelector(".sessbar .meta"))).toContain("x64 · ACME\\jdoe · elevated");
  });

  it("has the four tabs with all panels kept in the DOM and only the active one visible", () => {
    const root = show("live-terminal");
    const tabs = Array.from(root.querySelectorAll('[role="tab"]'));
    expect(tabs.map(text)).toEqual(["Overview", "Terminal", "Files", "Audit"]);
    expect(tabs.map((t) => t.getAttribute("aria-selected"))).toEqual([
      "false",
      "true",
      "false",
      "false",
    ]);
    const panels = Array.from(root.querySelectorAll('[role="tabpanel"]'));
    expect(panels).toHaveLength(4);
    expect(panels.map((p) => p.hasAttribute("hidden"))).toEqual([true, false, true, true]);
    // The Desktop tab waits for M005 (exclusion of ST-M009-008).
    expect(root.querySelector("#tab-desktop")).toBeNull();
  });

  it("titles the terminal the way the customer sees it and leaves slots for the live xterm", () => {
    const root = show("live-terminal");
    expect(text(root.querySelector(".sh-bar"))).toContain("PowerShell · ACME-PC-07");
    expect(text(root.querySelector(".sh-bar"))).toContain(
      "shown to the customer as “Sam · terminal”",
    );
    expect(root.querySelector('[data-keep="terminal"]')).not.toBeNull();
    expect(root.querySelector('[data-keep="terminal-badge"]')).not.toBeNull();
  });

  it("overview shows the identity and the session counts", () => {
    const root = show("live");
    const overview = root.querySelector("#panel-overview");
    expect(text(overview)).toContain("Admin rights Yes (elevated)");
    expect(text(overview)).toContain("Actions · AI 4 · terminal 1 · you 2");
    expect(text(overview)).toContain("1 / 1");
    expect(text(overview)).toContain("Failed · 1 running");
    expect(text(overview)).toContain("3 items of this session's activity were not shown");
  });

  it("files lists what was sent and received this session", () => {
    const root = show("live-files");
    const rows = root.querySelectorAll("#panel-files .list.files .it");
    expect(rows).toHaveLength(2);
    expect(text(rows[0])).toContain("Intel-WiFi-23.60.exe");
    expect(text(rows[0])).toContain("Received");
    expect(text(rows[1])).toContain("setupapi.dev.log");
    expect(text(rows[1])).toContain("left the customer computer");
    expect(text(rows[1])).toContain("1.2 MB");
  });

  it("audit lists this session's audit records, newest first, and opens the audit log of the session", () => {
    const root = show("live-audit");
    const rows = root.querySelectorAll("#panel-audit tbody tr");
    // Every record, the lifecycle ones included — not the activity feed.
    expect(rows).toHaveLength(6);
    expect(text(rows[0])).toContain("Get-WinEvent");
    expect(text(rows[0])).toContain("AI assistant");
    expect(text(rows[0])).toContain("mcp remote exec start");
    expect(text(rows[1])).toContain("failed");
    expect(rows[1]?.classList.contains("not-ok")).toBe(true);
    expect(text(rows[3])).toContain("Customer computer");
    expect(text(rows[3])).toContain("consent granted");
    expect(text(rows[4])).toContain("code: [REDACTED]");
    expect(text(rows[5])).toContain("session opened");
    expect(text(root.querySelector("#panel-audit"))).toContain("6 entries in this session's audit log.");
    expect(text(root.querySelector(".statusbar"))).toContain("Audit: 6 entries");
    const open = buttonNamed(root, "Open audit log");
    expect(open?.dataset.action).toBe("open-audit");
    expect(open?.dataset.arg).toBe("7f3a9c51d2e84b6a");
  });

  it("rail shows the AI clients by label and the feed with purpose, source, state and operation", () => {
    const root = show("live");
    const rail = root.querySelector("aside.rail");
    expect(text(rail?.querySelector(".rail-ai"))).toContain("Claude Code");
    expect(text(rail?.querySelector(".rail-ai"))).toContain("Codex CLI");
    // The tool count is unknown here, so it is not shown.
    expect(text(rail)).not.toMatch(/tools available/);
    const items = Array.from(rail?.querySelectorAll(".feed > li") ?? []);
    expect(items).toHaveLength(8);
    const running = items[0];
    // The purpose is labelled as the AI's claim, with the real operation beneath.
    expect(text(running?.querySelector(".t"))).toBe(
      "Stated purpose · Reading the Windows system log",
    );
    expect(text(running?.querySelector(".who"))).toBe("Claude Code");
    expect(text(running?.querySelector(".res"))).toBe("running · 12 s");
    expect(text(running?.querySelector("code"))).toContain(
      "Get-WinEvent -LogName System -MaxEvents 50",
    );
    const words = items.map((li) => text(li.querySelector(".res")));
    expect(words).toContain("failed · exit 1");
    expect(words).toContain("cancelled");
    expect(words).toContain("timed out");
    expect(words).toContain("exit 0");
    expect(items.find((li) => li.classList.contains("gap"))?.textContent).toContain(
      "3 items were not shown",
    );
    const terminalItem = items.find((li) => text(li.querySelector(".who")) === "Terminal");
    expect(text(terminalItem?.querySelector(".t"))).toBe("Ran a command in the terminal");
    expect(items.find((li) => text(li.querySelector(".who")) === "You")).toBeDefined();
  });

  it("rail shows the AI clients that called over IPC and the number of tools available", () => {
    const root = show("live-bridge");
    const ai = root.querySelector("aside.rail .rail-ai");
    // From the bridge status, not the feed: Gemini CLI called but has no item.
    const labels = Array.from(ai?.querySelectorAll("li b") ?? []).map((b) => text(b));
    expect(labels).toEqual(["Claude Code", "Gemini CLI"]);
    expect(text(ai)).toContain("bound to ACME-PC-07");
    expect(text(ai?.querySelector(".rail-tools"))).toBe("24 remote tools available");
    expect(ai?.querySelector('[role="alert"]')).toBeNull();
    const one = show("live-bridge", {
      bridge: { endpoint: "ready", tools: 1, clients: ["Codex CLI"] },
    });
    expect(text(one.querySelector(".rail-ai h3"))).toBe("AI assistant");
    expect(text(one.querySelector(".rail-tools"))).toBe("1 remote tool available");
  });

  it("rail says in an assertive alert when AI assistants cannot connect, with the next step", () => {
    const root = show("live-bridge-unavailable");
    const alert = root.querySelector('aside.rail [role="alert"]');
    expect(text(alert?.querySelector("b"))).toBe("AI assistants cannot connect");
    expect(text(alert)).toContain("address in use");
    expect(text(alert)).toContain("close it, then stop and start the session again");
    expect(root.querySelector(".rail-tools")).toBeNull();
    expect(text(root.querySelector(".rail-ai"))).toContain(
      "No AI assistant has called in this session.",
    );
    // An unavailable endpoint without a reason still says so.
    const bare = show("live-bridge-unavailable", {
      bridge: { endpoint: "unavailable", tools: 24, clients: [] },
    });
    expect(text(bare.querySelector('aside.rail [role="alert"] p'))).toBe(
      "The local connection for AI assistants is not running.",
    );
  });

  it("rail writes client labels and the bridge message as text", () => {
    const root = show("live-bridge", {
      bridge: { endpoint: "unavailable", message: XSS, tools: 24, clients: [XSS] },
    });
    expect(root.querySelector(".rail-ai img, .rail-ai script")).toBeNull();
    expect(root.querySelector(".rail-ai li b")?.textContent).toBe(XSS);
    expect(root.querySelector('.rail-ai [role="alert"] p')?.textContent).toBe(XSS);
  });

  it("says when nothing has happened yet", () => {
    const root = show("live-empty");
    expect(text(root.querySelector("aside.rail"))).toContain(
      "No AI assistant has acted in this session yet.",
    );
    expect(text(root.querySelector("aside.rail"))).toContain("Nothing has happened yet.");
  });

  it("offers Simulate target lost only for a simulated target", () => {
    expect(buttonNamed(show("live"), "Simulate target lost")).toBeUndefined();
    expect(buttonNamed(show("live-simulated"), "Simulate target lost")?.dataset.action).toBe(
      "lost",
    );
    expect(text(show("live-simulated").querySelector(".sessbar"))).toContain("Simulated target");
  });

  it("renders untrusted operations, purposes and paths as text (REQ-S-016, REQ-S-018)", () => {
    const root = show("live-xss");
    expect(root.querySelector("img, script")).toBeNull();
    const item = root.querySelector(".feed > li");
    expect(item?.textContent).toContain(XSS);
    expect(text(item?.querySelector(".t"))).toContain(XSS);
    expect(text(item?.querySelector("code"))).toContain(XSS);
    expect(root.querySelector("#panel-audit")?.textContent).toContain(XSS);
  });
});

describe("end (#t-end)", () => {
  it("says who ended it, when and how long, and Done returns Home", () => {
    const root = show("end-by-technician");
    expect(text(root)).toContain("Session 7f3a9c · Ended by you at 14:30");
    expect(text(root.querySelector("h2"))).toBe("ACME-PC-07 · 42 min");
    expect(buttonNamed(root, "Done")?.dataset.action).toBe("done");
    expect(text(root)).toContain("Cleaned up.");
  });

  it("puts residuals first with a safe next step", () => {
    const root = show("end-by-customer");
    expect(text(root)).toContain("Ended by the customer");
    const lines = Array.from(root.querySelectorAll(".cleanup .okline")).map(text);
    expect(lines[0]).toContain("1 residual.");
    expect(lines[0]).toContain("Next step:");
    expect(lines[1]).toContain("Intel Wi-Fi driver 23.60 stays installed");
    expect(lines[2]).toContain("Session keys and channel removed");
  });

  it("never shows an unknown cleanup as success after a lost connection", () => {
    const root = show("end-disconnected");
    expect(text(root)).toContain("The connection was lost");
    expect(text(root)).toContain("Cleanup unknown.");
    expect(text(root)).not.toContain("Cleaned up");
    expect(text(root)).toContain("This audit file was not sealed.");
  });

  it("reports what happened: actions by source, files, the first failure and the audit log", () => {
    const root = show("end-by-customer");
    const what = Array.from(root.querySelectorAll(".card")).find(
      (c) => text(c.querySelector("h4")) === "What happened",
    );
    expect(text(what)).toContain("Actions 4 · AI assistant 3 · you 0 · terminal 1");
    expect(text(what)).toContain("Files 1 sent · 1 received");
    expect(text(what)).toContain("Failures 1 · first: netsh wlan show drivers · exit 1");
    expect(buttonNamed(root, "Open the audit log of this session")?.dataset.arg).toBe(
      "7f3a9c51d2e84b6a",
    );
  });

  it("shows a failed session with the failed cleanup and its next step", () => {
    const root = show("end-failed");
    expect(text(root.querySelector('[role="alert"]'))).toContain("The session failed");
    expect(text(root.querySelector('[role="alert"]'))).toContain(
      "The connection to the customer computer broke.",
    );
    expect(text(root)).toContain("Ended by a failure");
    expect(text(root)).toContain("Cleanup failed.");
    expect(text(root)).toContain("Next step:");
  });

  it("prepares the report from the live feed until the sealed file is read", () => {
    const root = show("end-preparing");
    expect(text(root)).toContain("Preparing the report…");
    expect(text(root)).toContain("Ended by the customer at 14:30");
    expect(text(root.querySelector("h2"))).toBe("ACME-PC-07 · 42 min");
    expect(text(root)).toContain("Actions 7 · AI assistant 4 · you 2 · terminal 1");
    expect(text(root)).toContain("Failures 2 · first: netsh wlan show drivers");
    expect(text(root)).toContain("Not shown 3 items");
    expect(text(root)).not.toContain("Cleaned up");
  });

  it("says the cleanup is unknown when the report cannot be read", () => {
    const root = show("end-unavailable");
    expect(text(root)).toContain("Cleanup unknown.");
    expect(text(root)).toContain("could not be read");
  });
});

describe("reports from the history", () => {
  it("residuals, failed, unknown and broken files each say so", () => {
    expect(text(show("report-residuals"))).toContain("1 residual.");
    expect(text(show("report-failed"))).toContain("Cleanup failed.");
    expect(text(show("report-unknown"))).toContain("Cleanup unknown.");
    const broken = show("report-broken");
    expect(text(broken)).toContain("This audit file is broken.");
    expect(text(broken)).toContain("Unknown computer · duration unknown");
    expect(text(broken)).toContain("who ended it is not recorded");
    expect(text(broken)).toContain("Started Yesterday 13:40");
    expect(buttonNamed(broken, "Back to sessions")?.dataset.action).toBe("close-report");
  });
});

describe("AI assistants (#t-ai)", () => {
  it("shows each client present or not and registered or not, with its configuration path", () => {
    const root = show("ai");
    const rows = Array.from(root.querySelectorAll(".list.clients > li"));
    expect(rows.map((r) => text(r.querySelector("b")))).toEqual([
      "Claude Code",
      "Codex CLI",
      "Gemini CLI",
    ]);
    expect(text(rows[0])).toContain("Connected · ~/.claude.json");
    expect(buttonNamed(root, "Disconnect Claude Code")?.dataset.action).toBe("ai-disconnect");
    expect(text(rows[2])).toContain("Found · not connected · ~/.gemini/settings.json");
    expect(buttonNamed(root, "Connect Gemini CLI")?.dataset.action).toBe("ai-connect");
  });

  it("shows a registration that points elsewhere with its command and offers Replace", () => {
    const root = show("ai");
    const codex = root.querySelectorAll(".list.clients > li")[1];
    expect(text(codex)).toContain("Registered to another program · ~/.codex/config.toml");
    expect(text(codex)).toContain("D:\\old\\cretlink-cli.exe mcp");
    expect(buttonNamed(root, "Replace the registration of Codex CLI")?.dataset.action).toBe(
      "ai-replace",
    );
  });

  it("has no separate Claude Desktop entry: the Claude Code entry says it also serves its Code tab", () => {
    const alsoServes =
      "Also serves the Code tab of Claude Desktop, which uses the same registration.";
    const root = show("ai");
    const rows = Array.from(root.querySelectorAll(".list.clients > li"));
    expect(rows).toHaveLength(3);
    expect(rows.map((r) => text(r.querySelector("b")))).not.toContain("Claude Desktop");
    expect(text(root)).not.toContain("Not yet supported");
    // One short line of the Claude Code entry, after its state and path.
    const lines = Array.from(rows[0]?.querySelectorAll(".client-text > .muted") ?? []).map(text);
    expect(lines).toEqual(["Connected. Connected · ~/.claude.json", alsoServes]);
    // No other entry says it.
    expect(text(rows[1])).not.toContain("Claude Desktop");
    expect(text(rows[2])).not.toContain("Claude Desktop");
    // It holds whatever the state of the registration: not connected, not
    // found, or pointing at another program.
    const claude = FIXTURES.ai!.clients![0]!;
    for (const patch of [
      { registration: { state: "absent" as const } },
      { present: false, registration: { state: "absent" as const } },
      { registration: { state: "other_command" as const, command: "D:\\old\\cretlink.exe mcp" } },
    ]) {
      const row = show("ai", { clients: [{ ...claude, ...patch }] }).querySelector(
        ".list.clients > li",
      );
      expect(text(row?.querySelector(".client-text > .muted:last-of-type"))).toBe(alsoServes);
    }
  });

  it("keeps the line above the exact command when Disconnect is being confirmed", () => {
    const row = show("ai-confirm-disconnect").querySelector(".list.clients > li");
    const parts = Array.from(row?.querySelectorAll(".client-text > div") ?? []);
    expect(parts.map((part) => part.className)).toEqual(["muted", "muted also-serves", "confirm"]);
  });

  it("shows the terminal equivalent", () => {
    const root = show("ai");
    const cli = text(root.querySelector("pre.cli[aria-label]"));
    expect(cli).toContain("cretlink mcp install --client gemini");
    expect(cli).toContain("cretlink mcp uninstall --client claude");
    expect(cli).toContain("cretlink mcp status");
  });

  it("shows the exact command before Connect, Replace and Disconnect run", () => {
    const connect = show("ai-confirm-connect");
    const step = connect.querySelector(".confirm");
    expect(text(step)).toContain("This runs exactly:");
    expect(text(step?.querySelector("pre"))).toBe(FIXTURES.ai!.clients![2]!.installPlan[0]!);
    expect(buttonNamed(connect, "Connect")?.dataset.action).toBe("ai-confirm");
    expect(buttonNamed(connect, "Cancel")?.dataset.action).toBe("ai-cancel");

    const replace = show("ai-confirm-replace");
    expect(text(replace.querySelector(".confirm pre"))).toContain("write [mcp_servers.cretlink]");
    expect(buttonNamed(replace, "Replace")?.dataset.action).toBe("ai-confirm");

    const disconnect = show("ai-confirm-disconnect");
    expect(text(disconnect.querySelector(".confirm pre"))).toBe(
      "claude mcp remove --scope user cretlink",
    );
    expect(buttonNamed(disconnect, "Disconnect")?.dataset.action).toBe("ai-confirm");
  });

  it("shows an unreadable configuration and a client that is not found", () => {
    const root = show("ai-unreadable");
    const rows = root.querySelectorAll(".list.clients > li");
    expect(text(rows[1])).toContain(
      "The configuration can't be read · ~/.codex/config.toml: expected `=` at line 12",
    );
    expect(text(rows[2])).toContain("Not found on this computer");
    expect(rows[1]?.querySelector("button")).toBeNull();
    expect(rows[2]?.querySelector("button")).toBeNull();
  });

  it("warns that a run-once registration breaks when the copy is moved or deleted", () => {
    const root = show("ai-run-once");
    expect(text(root.querySelector(".note.run-once"))).toContain(
      "stops working when it is moved or deleted",
    );
    expect(text(root.querySelector(".confirm"))).toContain("break when it is moved or deleted");
    expect(text(show("ai").querySelector(".note.run-once"))).toBe("");
  });

  it("says when the client status is unavailable", () => {
    expect(text(show("ai-unavailable"))).toContain("The AI assistant status is not available yet.");
  });
});

describe("settings", () => {
  it("shows the version, the install path, the audit folder with Open, and Uninstall when installed", () => {
    const root = show("settings");
    expect(text(root)).toContain("Version CretLink 0.22.0");
    expect(text(root)).toContain(
      "Installed in C:\\Users\\sam\\AppData\\Local\\Programs\\CretLink",
    );
    expect(text(root)).toContain("C:\\Users\\sam\\AppData\\Local\\CretLink\\audit");
    expect(buttonNamed(root, "Open the audit folder")?.dataset.action).toBe("open-audit");
    expect(buttonNamed(root, "Open the audit folder")?.dataset.arg).toBeUndefined();
    expect(buttonNamed(root, "Uninstall…")?.dataset.action).toBe("uninstall-plan");
  });

  it("shows the uninstall plan before anything is removed, and that the audit trail stays", () => {
    const root = show("settings-uninstall");
    expect(Array.from(root.querySelectorAll(".plan li")).map(text)).toHaveLength(4);
    expect(text(root)).toContain("The audit trail stays");
    expect(buttonNamed(root, "Uninstall")?.dataset.action).toBe("uninstall-confirm");
    expect(buttonNamed(root, "Cancel")?.dataset.action).toBe("uninstall-cancel");
  });

  it("offers no uninstall for a run-once copy and names the folder it runs from", () => {
    const root = show("settings-run-once");
    expect(text(root)).toContain("Runs from C:\\Users\\sam\\Downloads");
    expect(buttonNamed(root, /Uninstall/)).toBeUndefined();
  });

  it("says when the details are unavailable", () => {
    const root = show("settings-unavailable");
    expect(text(root)).toContain("Version CretLink 0.22.0");
    expect(text(root)).toContain("Not available yet");
  });
});

describe("errors and announcements", () => {
  it("states an error in plain language with a next step in an assertive alert", () => {
    const alert = show("error").querySelector('[role="alert"]');
    expect(text(alert)).toContain("The session could not start");
    expect(text(alert)).toContain(
      "Check that this computer is connected to the network, then try again.",
    );
    expect(alert?.querySelector('[data-action="dismiss-error"]')).not.toBeNull();
  });

  it("states a failed listener on Home", () => {
    const root = show("failed-no-session");
    expect(text(root.querySelector("h2"))).toBe("Ready to help");
    expect(text(root.querySelector('[role="alert"]'))).toContain(
      "The port 7000 is already in use.",
    );
  });

  it("announces the phase politely", () => {
    expect(text(show("compare").querySelector('[aria-live="polite"]'))).toContain(
      "Compare the approval code",
    );
    expect(text(show("live").querySelector('[aria-live="polite"]'))).toBe(
      "Full control of ACME-PC-07.",
    );
    expect(
      text(
        show("live", { notice: "Connection code copied." }).querySelector('[aria-live="polite"]'),
      ),
    ).toBe("Connection code copied.");
  });
});
