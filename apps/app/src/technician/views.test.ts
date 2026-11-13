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

  it("falls back to the hostname without a display name", () => {
    const root = show("wait-no-name");
    expect(text(root)).toContain("select TECH-LAPTOP.");
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

describe("not on the same network (ST-M007-013)", () => {
  const offer = (root: HTMLElement): HTMLElement | null => root.querySelector("#relay-offer");

  it("offers Use relay with the relay's host name when this copy has a relay", () => {
    const root = show("wait-relay-ready");
    const card = offer(root);
    expect(card?.getAttribute("role")).toBe("group");
    expect(text(card?.querySelector("#relay-offer-h"))).toBe("Not on the same network?");
    expect(card?.getAttribute("aria-labelledby")).toBe("relay-offer-h");
    expect(text(card)).toContain("relay.example.org");
    expect(text(card)).toContain("only passes encrypted traffic on and cannot look into the session");
    const use = buttonNamed(root, "Use the relay relay.example.org");
    expect(text(use)).toBe("Use relay…");
    expect(use?.dataset.action).toBe("use-relay");
    expect(use?.disabled).toBe(false);
    // The connection code of this network is still there: LAN first.
    expect(text(root.querySelector(".conn-code"))).toBe("6RRR-2H8F");
    expect(root.querySelector(".relay-code")).toBeNull();
  });

  it("names Settings when this copy has no relay, and offers nothing to press but that", () => {
    const root = show("wait");
    const card = offer(root);
    expect(text(card)).toContain("Not on the same network?");
    expect(text(card)).toContain("This copy has no relay.");
    expect(text(card)).toContain("set up your relay in Settings, under Relay");
    expect(card?.querySelector('[data-action="use-relay"]')).toBeNull();
    const settings = card?.querySelector<HTMLButtonElement>("button");
    expect(text(settings)).toBe("Open Settings");
    expect(settings?.dataset.action).toBe("nav");
    expect(settings?.dataset.arg).toBe("settings");
    // Until the backend has said which relay this copy uses, the same.
    expect(text(offer(show("wait-relay-unknown")))).toContain("set up your relay in Settings");
  });

  it("says where the relay of a copy that runs once goes", () => {
    const card = offer(show("wait-relay-run-once"));
    expect(text(card)).toContain("put a file cretlink.toml for your relay next to the program");
    expect(text(card)).toContain("Settings, under Relay, shows how");
    expect(card?.querySelector('[data-action="use-relay"]')).toBeNull();
  });

  it("says that a relay profile cannot be used, with what is wrong and its file", () => {
    const card = offer(show("wait-relay-problem"));
    expect(text(card)).toContain("The relay settings of this copy cannot be used.");
    expect(text(card)).toContain("cretlink.toml");
    expect(card?.querySelector('[data-action="use-relay"]')).toBeNull();
    expect(card?.querySelector<HTMLButtonElement>("button")?.dataset.arg).toBe("settings");
  });

  it("shows no code and no offer while the wait changes to the relay", () => {
    const root = show("wait-relay-changing");
    expect(root.dataset.screen).toBe("wait");
    expect(text(root.querySelector('[role="status"].conn-switching'))).toBe(
      "Changing to your relay…",
    );
    expect(root.querySelector(".conn-code")).toBeNull();
    expect(root.querySelector(".relay-code")).toBeNull();
    expect(offer(root)).toBeNull();
    expect(buttonNamed(root, /Cancel/)?.dataset.action).toBe("stop");
  });

  it("keeps the offer away while the network changes and while another action runs", () => {
    expect(offer(show("wait-switching"))).toBeNull();
    const busy = show("wait-relay-ready", { busy: "stop" });
    expect(buttonNamed(busy, "Use the relay relay.example.org")?.disabled).toBe(true);
  });
});

describe("waiting on the relay (ST-M007-013)", () => {
  it("shows the relay's host name, the relay code with Copy, the state in words and Cancel (Esc)", () => {
    const root = show("relay-wait");
    expect(root.dataset.screen).toBe("wait");
    expect(text(root.querySelector(".pill"))).toContain("No access");
    expect(text(root.querySelector("h2"))).toBe("Waiting for the customer on your relay");
    expect(text(root.querySelector("#relay-host"))).toBe("relay.example.org");
    const cancel = buttonNamed(root, /Cancel/);
    expect(cancel?.id).toBe("stop");
    expect(cancel?.dataset.action).toBe("stop");
    expect(cancel?.getAttribute("aria-keyshortcuts")).toBe("Escape");
    const copy = buttonNamed(root, "Copy the relay code");
    expect(copy?.dataset.action).toBe("copy");
    expect(copy?.dataset.arg).toBe("53ST-A71D-JKKV-0VGH");
    const state = root.querySelector("#relay-state");
    expect(state?.getAttribute("role")).toBe("status");
    expect(text(state)).toBe(
      "The relay holds this code. It works once, for as long as this screen is open.",
    );
    expect(text(root)).toContain("type this code into the code field");
    // What follows is said: the relay code is not the permission.
    expect(text(root)).toContain(
      "You compare the approval code and the customer gives permission next, as on one network.",
    );
    expect(text(root.querySelector("#announce"))).toBe(
      "Waiting for the customer on the relay relay.example.org. Read them the relay code. No access.",
    );
  });

  it("names and draws the relay code differently from the approval code and the connection code", () => {
    const root = show("relay-wait");
    const code = root.querySelector("#relay-code");
    expect(code?.classList.contains("relay-code")).toBe(true);
    // Four groups of four, each a box of its own.
    expect(Array.from(code?.querySelectorAll("span") ?? []).map(text)).toEqual([
      "53ST",
      "A71D",
      "JKKV",
      "0VGH",
    ]);
    expect(text(root.querySelector("#relay-code-label"))).toBe(
      "Relay code · read it to the customer",
    );
    expect(code?.getAttribute("role")).toBe("img");
    expect(code?.getAttribute("aria-label")).toBe(
      "Relay code 5 3 S T, A 7 1 D, J K K V, 0 V G H",
    );
    // Neither of the two other codes is on this screen, nor their drawing.
    expect(root.querySelector(".sac")).toBeNull();
    expect(root.querySelector(".conn-code")).toBeNull();
    expect(text(root)).not.toContain("Connection code");
    expect(text(root)).not.toContain("Approval code");
    // And the approval code is not drawn as a relay code.
    const compare = show("relay-compare");
    expect(compare.querySelector(".relay-code")).toBeNull();
    expect(compare.querySelector(".sac")?.getAttribute("aria-label")).toBe("Approval code 482 913");
  });

  it("says that the relay is still being reached until it holds the code", () => {
    const root = show("relay-wait-connecting");
    expect(text(root.querySelector("#relay-state"))).toBe(
      "Connecting to the relay relay.example.org…",
    );
    // The code is drawn on this side and can be read out already.
    expect(root.querySelector("#relay-code")).not.toBeNull();
    expect(text(root.querySelector("#announce"))).toBe(
      "Connecting to the relay relay.example.org. No access.",
    );
  });

  it("offers neither another network nor another relay while it waits", () => {
    const root = show("relay-wait", { networksOpen: true });
    expect(root.querySelector("#net-toggle")).toBeNull();
    expect(root.querySelector("#net-list")).toBeNull();
    expect(root.querySelector("#relay-offer")).toBeNull();
    expect(root.querySelector('[data-action="use-relay"]')).toBeNull();
  });
});

describe("a relay that cannot be used (ST-M007-013)", () => {
  const alert = (root: HTMLElement): HTMLElement | null => root.querySelector("#alert");

  it.each([
    ["relay-failed-unreachable", "The relay cannot be reached", "could not connect to the relay", "Check that the relay is running"],
    ["relay-failed-untrusted", "The relay's certificate is not trusted", "showed no certificate that this computer trusts", "give its certificate file"],
    ["relay-failed-key", "The relay refused the admission key", "does not know the admission key", "Enter the key again"],
  ])("%s: says what happened in its own words, with its own next step", (name, title, message, next) => {
    const root = show(name);
    // No session ever existed: the technician is back where a session starts.
    expect(root.dataset.screen).toBe("home");
    const region = alert(root);
    expect(region?.getAttribute("role")).toBe("alert");
    expect(text(region?.querySelector("b"))).toBe(title);
    expect(text(region)).toContain(message);
    expect(text(region?.querySelector(".next"))).toContain(next);
    expect(text(region?.querySelector(".next"))).toContain("Settings, under Relay");
    // It does not read as a session that failed for no known reason, and the
    // stage is not shown as a code.
    expect(text(region)).not.toContain("The session failed");
    expect(text(region)).not.toMatch(/\(relay_/);
    expect(text(root.querySelector("#announce"))).toBe("The session failed. No access.");
  });

  it("keeps the generic words for a failure that names no next step", () => {
    const root = show("failed-no-session");
    expect(text(alert(root)?.querySelector("b"))).toBe("The session failed");
    expect(text(alert(root))).toContain("The port 7000 is already in use. (listen)");
    expect(text(alert(root)?.querySelector(".next"))).toBe(
      "Nobody has access to the customer computer. Start a new session when you are ready.",
    );
  });

  it("says that the relay may have stopped when the connection is lost before control", () => {
    const root = show("relay-lost-early");
    const region = alert(root);
    expect(text(region?.querySelector("b"))).toBe("The connection through the relay was lost");
    expect(text(region)).toContain(
      "The connection through the relay relay.example.org was lost. The relay may have stopped, or one of the two computers went offline.",
    );
    expect(text(region?.querySelector(".next"))).toBe(
      "Nobody has access to the customer computer any more. Start a new session when you are ready; it gets a new relay code.",
    );
    expect(text(region)).not.toContain("The session failed");
  });
});

describe("a session through the relay (ST-M007-013)", () => {
  it.each(["relay-connecting", "relay-compare", "relay-consent"])(
    "%s: says that the session runs through the relay, by its host name",
    (name) => {
      const line = show(name).querySelector(".via-relay");
      expect(text(line)).toBe(
        "Through the relay relay.example.org · it only passes encrypted traffic on",
      );
    },
  );

  it("leaves the compare and consent screens of a relayed session what they are on one network", () => {
    const compare = show("relay-compare");
    expect(text(compare.querySelector(".pill"))).toBe("◐ Connected · no access");
    expect(buttonNamed(compare, "Codes match")?.dataset.action).toBe("compare");
    expect(buttonNamed(compare, "Codes don't match")?.dataset.action).toBe("mismatch");
    const consent = show("relay-consent");
    expect(text(consent)).toContain("Only the customer can grant control.");
    expect(text(consent.querySelector(".disclose"))).toBe(DISCLOSURE_TEXT);
    expect(buttonNamed(consent, /Stop/)?.dataset.action).toBe("stop");
  });

  it("names the relay under the identity of the live session and in the overview", () => {
    const root = show("relay-live");
    expect(text(root.querySelector("#session-relay"))).toBe(
      "Through the relay relay.example.org · it only passes encrypted traffic on",
    );
    expect(text(root.querySelector("#panel-overview"))).toContain(
      "Connection Through the relay relay.example.org",
    );
    // The band, the identity and Stop are those of any session.
    expect(text(root.querySelector("#control-band"))).toContain("FULL CONTROL");
    expect(buttonNamed(root, "Stop the session with ACME-PC-07")?.dataset.action).toBe("stop");
  });

  it("says nothing about a relay in a session on one network", () => {
    for (const name of ["connecting", "compare", "consent", "live", "end-by-technician"]) {
      const root = show(name);
      expect(root.querySelector(".via-relay")).toBeNull();
      expect(root.querySelector("#session-relay")).toBeNull();
      expect(root.querySelector(".relay-lost")).toBeNull();
    }
    expect(text(show("live").querySelector("#panel-overview"))).toContain(
      "Connection On this network",
    );
    expect(text(show("end-by-technician"))).toContain("Connection On one network");
  });

  it("names the relay in the end report", () => {
    const root = show("relay-end");
    expect(root.dataset.screen).toBe("end");
    expect(text(root)).toContain("Connection Through the relay relay.example.org");
    expect(root.querySelector(".relay-lost")).toBeNull();
  });

  it("says that the relay may have stopped when the connection of a relayed session is lost", () => {
    for (const name of ["relay-end-lost", "relay-end-lost-preparing"]) {
      const root = show(name);
      expect(root.dataset.screen).toBe("end");
      const note = root.querySelector(".relay-lost");
      expect(note?.getAttribute("role")).toBe("note");
      expect(text(note)).toBe(
        "The connection through the relay relay.example.org was lost. The relay may have stopped, or one of the two computers went offline. Nobody has access to the customer computer any more. Start a new session when you are ready; it gets a new relay code.",
      );
      expect(text(root)).toContain("Connection Through the relay relay.example.org");
    }
    // The cleanup of a lost connection is still not shown as a success.
    expect(text(show("relay-end-lost"))).toContain("Cleanup unknown");
    // On one network the same end says nothing about a relay.
    expect(show("end-disconnected").querySelector(".relay-lost")).toBeNull();
  });

  it("names the relay in a report from the history", () => {
    const root = show("relay-end", { nav: "sessions", openReport: "7f3a9c51d2e84b6a", endDismissed: true });
    expect(root.dataset.screen).toBe("report");
    expect(text(root)).toContain("Connection Through the relay relay.example.org");
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

describe("settings: the relay (ST-M007-011)", () => {
  const relayCard = (root: HTMLElement): HTMLElement => {
    const head = root.querySelector("#relay-head");
    const found = head?.closest<HTMLElement>(".card");
    if (!found) throw new Error("no relay card");
    return found;
  };
  const input = (root: HTMLElement, id: string): HTMLInputElement => {
    const found = root.querySelector<HTMLInputElement>(`#${id}`);
    if (!found) throw new Error(`no #${id}`);
    return found;
  };

  it("says what a relay is and that it grants nothing", () => {
    const card = relayCard(show("settings"));
    expect(text(card)).toContain("You host it yourself.");
    expect(text(card)).toContain(
      "It only passes encrypted traffic on: it cannot look into a session and it grants nothing.",
    );
    expect(text(card)).toContain("The approval code and the customer's consent decide");
  });

  it("gives the installed copy a form with the four settings, Save and Test", () => {
    const root = show("settings");
    const card = relayCard(root);
    // Every field has a visible label and a hint that says what it is.
    for (const [id, label] of [
      ["relay-address", "Address"],
      ["relay-key", "Admission key"],
      ["relay-name", "Name in the certificate"],
    ] as const) {
      const field = input(root, id);
      expect(text(root.querySelector(`label[for="${id}"]`))).toBe(label);
      const hint = field.getAttribute("aria-describedby");
      expect(text(root.querySelector(`#${hint}`))).not.toBe("");
    }
    expect(text(card.querySelector("#relay-ca-label"))).toBe("Certificate authority");
    expect(text(card.querySelector("#relay-ca"))).toBe(
      "The certificate authorities of this computer",
    );
    expect(text(card)).toContain("CretLink always uses TLS.");
    expect(input(root, "relay-address").getAttribute("value")).toBe("");
    expect(buttonNamed(card, "Save")?.dataset.action).toBe("relay-save");
    expect(buttonNamed(card, "Test the relay")?.dataset.action).toBe("relay-test");
    expect(buttonNamed(card, "Choose a file…")?.dataset.action).toBe("relay-ca-choose");
    // Nothing is kept yet, so there is nothing to remove.
    expect(buttonNamed(card, /Remove/)).toBeUndefined();
    expect(card.querySelector("#relay-key-remove")).toBeNull();
  });

  it("shows the kept relay without ever showing its key", () => {
    const root = show("settings-relay");
    const card = relayCard(root);
    expect(input(root, "relay-address").getAttribute("value")).toBe("relay.example.org:443");
    const key = input(root, "relay-key");
    expect(key.type).toBe("password");
    expect(key.hasAttribute("value")).toBe(false);
    expect(key.value).toBe("");
    expect(text(card.querySelector("#relay-key-hint"))).toBe(
      "A key is set. It is never shown. Leave this empty to keep it; it stays with this relay and is not taken to another one.",
    );
    expect(text(card)).toContain(
      "A key only admits this copy to the relay. It gives nobody control over a session.",
    );
    expect(text(root.querySelector('label[for="relay-key-remove"]'))).toBe(
      "Remove the key that is set",
    );
    expect(buttonNamed(card, "Remove…")?.dataset.action).toBe("relay-remove");
  });

  it("names the certificate file that is kept, and the one that was just chosen", () => {
    const kept = relayCard(show("settings-relay-private"));
    expect(text(kept.querySelector("#relay-ca"))).toBe("relay-ca.pem · kept with your settings");
    expect(buttonNamed(kept, "Choose another file…")?.dataset.action).toBe("relay-ca-choose");
    expect(buttonNamed(kept, "Use this computer's authorities")?.dataset.action).toBe(
      "relay-ca-none",
    );
    expect(kept.querySelector<HTMLInputElement>("#relay-name")?.getAttribute("value")).toBe(
      "host-01.example.org",
    );

    const chosen = relayCard(show("settings-relay-chosen"));
    expect(text(chosen.querySelector("#relay-ca"))).toBe("authority.pem · chosen, not saved yet");
  });

  it("reports a test in words, in a region that is announced politely", () => {
    const running = show("settings-relay-testing");
    const region = running.querySelector("#relay-test");
    expect(region?.getAttribute("role")).toBe("status");
    expect(text(region)).toBe("Reaching the relay…");
    // Unavailable while it runs, and still where the keyboard is.
    const test = buttonNamed(running, "Test the relay");
    expect(test?.getAttribute("aria-disabled")).toBe("true");
    expect(test?.disabled).toBe(false);
    expect(buttonNamed(show("settings-relay"), "Test the relay")?.hasAttribute("aria-disabled")).toBe(
      false,
    );

    const admitted = show("settings-relay-admitted").querySelector("#relay-test");
    expect(text(admitted)).toContain("The relay admits this copy");
    expect(text(admitted)).toContain("The relay relay.example.org answered and admits this copy.");

    const refused = show("settings-relay-refused").querySelector("#relay-test");
    // The state is in words, not in a colour alone, and has a next step.
    expect(text(refused?.querySelector(".pill"))).toBe("The relay cannot be used");
    expect(text(refused)).toContain("does not know the admission key of this copy");
    expect(text(refused?.querySelector(".next"))).toBe(
      "Enter the key again, or ask the relay's operator for a new one.",
    );
    // Without a test the region is there, and empty.
    expect(text(show("settings-relay").querySelector("#relay-test"))).toBe("");
  });

  it("asks before the relay and its key are removed", () => {
    const root = show("settings-relay-removing");
    const step = root.querySelector<HTMLElement>("#relay-remove-step");
    expect(text(step)).toContain("Remove the relay relay.example.org from the settings?");
    expect(text(step)).toContain(
      "Its admission key and its certificate file are removed with it and cannot be shown again. The network to wait on stays.",
    );
    const cancel = buttonNamed(step as HTMLElement, "Cancel");
    expect(cancel?.dataset.action).toBe("relay-remove-cancel");
    expect(cancel?.hasAttribute("data-autofocus")).toBe(true);
    expect(buttonNamed(step as HTMLElement, "Remove")?.dataset.action).toBe(
      "relay-remove-confirm",
    );
    // The form waits for the answer.
    expect(buttonNamed(root, "Save")?.disabled).toBe(true);
  });

  it("explains settings that cannot be read with the setting and the file", () => {
    const root = show("settings-relay-unreadable");
    const note = relayCard(root).querySelector(".relay-problem");
    expect(text(note)).toBe(
      "These relay settings cannot be used. Line 3: the value of `key` must be text in double quotes. The file is C:\\Users\\sam\\AppData\\Local\\Programs\\CretLink\\settings.toml. Saving a relay below replaces them; Remove clears them.",
    );
    // The form and Remove are still there to put it right.
    expect(root.querySelector("#relay-address")).not.toBeNull();
    expect(buttonNamed(root, "Remove…")?.dataset.action).toBe("relay-remove");
  });

  it("says that a file next to the program wins over Settings", () => {
    const root = show("settings-relay-file");
    const card = relayCard(root);
    expect(text(card.querySelector(".relay-file"))).toBe(
      "This copy takes its relay from the file C:\\Users\\sam\\AppData\\Local\\Programs\\CretLink\\cretlink.toml next to the program. That file wins over what Settings holds.",
    );
    expect(text(card)).toContain("Relay host-01.example.org");
    expect(text(card)).toContain("Address 192.0.2.1:9970");
    expect(text(card)).toContain("Admission key None");
    expect(text(card)).toContain("Certificate checked against The certificate file relay-ca.pem");
    expect(text(card)).toContain("Name in the certificate host-01.example.org");
    // The relay of the settings is named as the one that is not used.
    expect(text(card.querySelector(".relay-unused"))).toContain(
      "Settings also holds the relay relay.example.org. It is not used while the file is there.",
    );
    expect(buttonNamed(card, "Remove from Settings…")?.dataset.action).toBe("relay-remove");
    expect(buttonNamed(card, "Test the relay")?.dataset.action).toBe("relay-test");
    // The file is not edited here: no form.
    expect(card.querySelector("input")).toBeNull();
    expect(buttonNamed(card, "Save")).toBeUndefined();
  });

  it("explains a file that cannot be used, and offers no test of it", () => {
    const card = relayCard(show("settings-relay-file-problem"));
    expect(text(card.querySelector(".relay-problem"))).toContain(
      "These relay settings cannot be used. Line 2: `address` needs a port;",
    );
    expect(text(card.querySelector(".relay-problem"))).toContain(
      "Correct the file, or remove it.",
    );
    expect(buttonNamed(card, "Test the relay")).toBeUndefined();
  });

  it("tells a copy that runs once where its relay goes, and keeps nothing", () => {
    const card = relayCard(show("settings-run-once"));
    expect(text(card)).toContain(
      "This copy runs once and keeps no settings. To use a relay with it, put a file named cretlink.toml next to the program:",
    );
    expect(text(card.querySelector(".relay-where"))).toBe("C:\\Users\\sam\\Downloads\\cretlink.toml");
    expect(card.querySelector(".relay-example")?.textContent).toBe(
      '[relay]\naddress = "relay.example.org:443"\nkey = "the key of this copy"',
    );
    expect(card.querySelector("input")).toBeNull();
    expect(buttonNamed(card, "Save")).toBeUndefined();

    const withFile = relayCard(show("settings-relay-run-once-file"));
    expect(text(withFile)).toContain("Relay relay.example.org");
    expect(text(withFile)).toContain("Admission key Set (never shown)");
    expect(text(withFile)).toContain(
      "Certificate checked against The certificate authorities of this computer",
    );
    // No settings to win over.
    expect(text(withFile)).not.toContain("wins over");
    expect(buttonNamed(withFile, "Test the relay")).toBeDefined();
  });

  it("says when the relay of this copy is not known yet", () => {
    expect(text(relayCard(show("settings-unavailable")))).toContain(
      "The relay of this copy is not available yet.",
    );
  });

  it("writes the texts of a profile as text, never as markup", () => {
    const root = show("settings-relay-xss");
    expect(root.querySelector("img, script, b.bold")).toBeNull();
    expect(text(relayCard(root))).toContain(XSS);
  });

  it("says in the uninstall text that the settings go too", () => {
    expect(text(show("settings"))).toContain("its settings with the relay and its key");
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
