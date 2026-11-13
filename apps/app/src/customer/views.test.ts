// Rendering tests for every customer screen and state (ST-M009-009), derived
// from the story's acceptance criteria. Each screen is a pure function of a
// snapshot, rendered in jsdom without Tauri.

import { afterEach, describe, expect, it } from "vitest";
import type { ActivityEntry } from "../shared/contract";
import { upsertActivity } from "../shared/feed";
import { DISCLOSURE_TEXT } from "../technician/disclosure";
import { FEED_DONE, FIXTURES, MINI_FIXTURES, NOW, XSS, viewIn } from "./fixtures";
import type { CustomerSnapshot } from "./model";
import { renderMini } from "./views/mini";
import { renderCustomer } from "./views/shell";

function show(name: string, patch: Partial<CustomerSnapshot> = {}): HTMLElement {
  const fixture = FIXTURES[name] ?? MINI_FIXTURES[name];
  if (!fixture) throw new Error(`no fixture ${name}`);
  const snapshot = { ...fixture, ...patch };
  const root = snapshot.window === "mini" ? renderMini(snapshot) : renderCustomer(snapshot);
  document.body.replaceChildren(root);
  return root;
}

// The text a reader gets: decorative (aria-hidden) parts left out, and the
// parts of a line separated by a space.
function text(el: Element | null | undefined): string {
  if (!el) return "";
  const copy = el.cloneNode(true) as Element;
  for (const hidden of copy.querySelectorAll('[aria-hidden="true"]')) hidden.remove();
  for (const part of copy.querySelectorAll("b, small, span, div, p, dt, dd, h3, h4, li, code, br")) {
    part.after(" ");
  }
  return (copy.textContent ?? "").replace(/\s+/g, " ").trim();
}

function buttons(root: HTMLElement): HTMLButtonElement[] {
  return Array.from(root.querySelectorAll("button"));
}

function buttonNamed(root: HTMLElement, name: string | RegExp): HTMLButtonElement | undefined {
  return buttons(root).find((b) => {
    const label = b.getAttribute("aria-label") ?? "";
    const own = text(b);
    return typeof name === "string"
      ? label === name || own === name
      : name.test(label) || name.test(own);
  });
}

function band(root: HTMLElement): string {
  return text(root.querySelector("#state-band"));
}

afterEach(() => document.body.replaceChildren());

describe("every main-window screen", () => {
  it.each(Object.keys(FIXTURES))(
    "%s: a skip link, the state band in words, alert and live regions, native controls",
    (name) => {
      const root = show(name);
      expect(root.classList.contains("cl-ui")).toBe(true);
      expect(root.classList.contains("cust")).toBe(true);
      expect(root.querySelector("a.skip-link")?.getAttribute("href")).toBe("#main");
      expect(root.querySelector("#main")).not.toBeNull();
      expect(root.querySelector('[role="alert"]')).not.toBeNull();
      expect(root.querySelector('[aria-live="polite"]')?.textContent).not.toBe("");
      // The state starts every screen, in words — never by colour alone.
      expect(root.firstElementChild?.nextElementSibling?.id).toBe("state-band");
      expect(band(root)).toMatch(/^(NO ACCESS|YOUR DECISION|UNDER FULL CONTROL) \S/);
      for (const control of root.querySelectorAll("[data-action]")) {
        expect(["BUTTON", "A"]).toContain(control.tagName);
      }
      // No reboot-resume control anywhere (FND-0005).
      for (const b of buttons(root)) expect(text(b)).not.toMatch(/restart|reconnect|resume/i);
    },
  );
});

describe("state band", () => {
  it("says no access, your decision or under full control with a short explanation", () => {
    expect(band(show("find"))).toBe("NO ACCESS Nobody can see or control this computer");
    expect(band(show("sac"))).toBe("NO ACCESS Connected to Sam, but no access");
    expect(band(show("consent"))).toBe("YOUR DECISION No control has been granted");
    expect(band(show("live-running"))).toContain("UNDER FULL CONTROL Sam can operate this computer now");
    expect(band(show("end-by-customer"))).toBe("NO ACCESS The session has ended");
  });

  it("shows when control began and how long it has lasted", () => {
    expect(text(show("live-running").querySelector("#state-band .since"))).toBe("since 14:02 42 min");
  });
});

describe("before the UAC prompt (#c-admin)", () => {
  it("explains the prompt in four steps; nothing happens before step 4", () => {
    const root = show("admin");
    expect(text(root)).toContain("Windows will ask for permission");
    expect(root.querySelectorAll(".steps li")).toHaveLength(4);
    expect(text(root.querySelector(".steps"))).toContain("You decide whether to allow control");
    expect(text(root)).toContain("Nothing happens to this computer before step 4, and nothing is installed.");
    const cont = buttonNamed(root, "Continue");
    expect(cont?.dataset.action).toBe("continue");
    expect(cont?.hasAttribute("data-autofocus")).toBe(true);
    expect(buttonNamed(root, "Back to the start screen")?.dataset.action).toBe("back");
    // No Stop before a session exists.
    expect(root.querySelector('[data-action="stop"]')).toBeNull();
  });

  it("closes a CretLink-Support copy instead of going back", () => {
    const root = show("admin-support-copy");
    expect(buttonNamed(root, "Close CretLink")?.dataset.action).toBe("back");
  });

  it("after a declined prompt says admin tasks will not work, with Try again and Continue without admin rights", () => {
    const root = show("admin-declined");
    expect(text(root)).toContain("Windows did not get permission");
    expect(text(root)).toContain("Administrator tasks will not work.");
    expect(buttonNamed(root, "Try again")?.dataset.action).toBe("continue");
    expect(buttonNamed(root, "Continue without admin rights")?.dataset.action).toBe("without-admin");
  });

  it("marks the simulated prompt of a development build", () => {
    expect(text(show("admin-dev"))).toContain("Development build.");
    expect(text(show("admin"))).not.toContain("Development build.");
  });
});

describe("choose your technician (#c-find)", () => {
  it("lists nearby consoles untrusted: nothing preselected, each with name, host, address and reach", () => {
    const root = show("find");
    const picks = root.querySelectorAll(".pick");
    expect(picks).toHaveLength(2);
    expect(text(picks[0])).toContain("Sam · TECH-LAPTOP");
    expect(text(picks[0])).toContain("10.0.0.42:51234 · this network");
    expect(text(picks[1])).toContain("unreachable");
    // Nothing connects on its own: no control is focused, pressed or selected.
    for (const b of buttons(root)) {
      expect(b.hasAttribute("data-autofocus")).toBe(false);
      expect(b.getAttribute("aria-pressed")).toBeNull();
    }
    expect(buttonNamed(root, "Connect to Sam · TECH-LAPTOP")?.dataset.arg).toBe("10.0.0.42:51234");
    expect(buttonNamed(root, "CONSOLE-3F1 is unreachable")?.disabled).toBe(true);
  });

  it("always offers the code field, bounded", () => {
    for (const name of ["find", "find-none", "find-searching"]) {
      const input = show(name).querySelector<HTMLInputElement>("input#code");
      expect(input).not.toBeNull();
      expect(input?.getAttribute("maxlength")).toBe("64");
      expect(document.querySelector('label[for="code"]')).not.toBeNull();
    }
  });

  it("says when nothing was found, and why", () => {
    const root = show("find-none");
    expect(root.querySelectorAll(".pick")).toHaveLength(0);
    expect(text(root)).toContain("ask the technician to read out the connection code instead");
  });

  it("shows this computer truthfully, elevated or not", () => {
    expect(text(show("find").querySelector(".computer"))).toBe(
      "This computer ACME-PC-07 Signed in as ACME\\jdoe Admin rights Yes, approved in Windows",
    );
    expect(text(show("find-none").querySelector(".computer"))).toContain("Admin rights No");
  });

  it("says that nothing is installed and closing ends everything", () => {
    expect(text(show("find").querySelector(".cfoot"))).toBe(
      "Nothing is installed · closing CretLink ends everything",
    );
  });

  it("after a session that never reached control, says how it ended", () => {
    expect(text(show("find-after-decline"))).toContain("You stopped before anyone got access.");
    const failed = show("find-connect-failed");
    expect(text(failed.querySelector('[role="alert"]'))).toContain(
      "CretLink could not reach your technician",
    );
    expect(text(failed.querySelector('[role="alert"]'))).toContain("connection refused");
  });

  it("says plainly when files from an earlier session could not be removed, with the recovery command", () => {
    const note = show("find-leftovers").querySelector(".leftovers");
    expect(note?.getAttribute("role")).toBe("note");
    expect(text(note)).toBe(
      "2 files from an earlier CretLink session could not be removed. They stay on this computer until they are removed. To remove them, open Windows PowerShell as administrator and run: & 'C:\\Users\\ann\\Downloads\\CretLink-Support.exe' recover --remove | Out-Host",
    );
    // The command stands alone, exactly as it is run: nothing appended.
    expect(note?.querySelector("code")?.textContent).toBe(
      "& 'C:\\Users\\ann\\Downloads\\CretLink-Support.exe' recover --remove | Out-Host",
    );
    // One file, next to the console build.
    const command = "& 'C:\\Program Files\\CretLink\\cretlink-cli.exe' recover --remove";
    const one = show("find", {
      view: viewIn({ phase: "idle" }, { startup_cleanup: { failed: 1, command, administrator: true } }),
    });
    expect(text(one.querySelector(".leftovers"))).toContain("1 file from an earlier CretLink session");
    expect(text(one.querySelector(".leftovers code"))).toBe(command);
    // What a window without administrator rights kept lies in the folder of
    // its account: the command is then not to be run as administrator.
    const own = show("find", {
      view: viewIn({ phase: "idle" }, { startup_cleanup: { failed: 1, command, administrator: false } }),
    });
    expect(text(own.querySelector(".leftovers"))).toContain(
      "To remove them, open Windows PowerShell and run: ",
    );
    expect(text(own.querySelector(".leftovers"))).not.toContain("as administrator");
    // Nothing kept: no notice.
    expect(show("find").querySelector(".leftovers")).toBeNull();
  });

  it("writes the recovery command as text, never as markup", () => {
    const root = show("find", {
      view: viewIn({ phase: "idle" }, { startup_cleanup: { failed: 1, command: XSS, administrator: true } }),
    });
    expect(root.querySelector(".leftovers img, .leftovers script")).toBeNull();
    expect(root.querySelector(".leftovers code")?.textContent).toBe(XSS);
  });

  it("never offers Stop on the resting window", () => {
    for (const name of ["find", "find-none", "admin", "admin-declined"]) {
      expect(show(name).querySelector('[data-action="stop"]')).toBeNull();
    }
  });
});

describe("a relay code (ST-M007-013)", () => {
  const alert = (root: HTMLElement): HTMLElement | null => root.querySelector("#alert");

  it("names the relay of this copy beside the code field, before any code is typed", () => {
    const root = show("find-relay-ready");
    expect(root.dataset.screen).toBe("find");
    const line = root.querySelector("#relay-ready");
    expect(text(line)).toBe(
      "If your technician is not on this network, they give you a longer relay code. This copy then connects through the relay relay.example.org , which only passes encrypted traffic on.",
    );
    // It is read with the field, and the field is the one of every code.
    const field = root.querySelector<HTMLInputElement>("#code");
    expect(field?.getAttribute("aria-describedby")).toBe("code-hint relay-ready");
    expect(text(root.querySelector("#code-hint"))).toBe(
      "The code only says where to connect. It grants nothing.",
    );
    expect(buttons(root).filter((b) => b.dataset.action === "connect-code")).toHaveLength(1);
    expect(band(root)).toContain("NO ACCESS");
    // A copy without a relay says nothing about one.
    const plain = show("find");
    expect(plain.querySelector("#relay-ready")).toBeNull();
    expect(plain.querySelector("#code")?.getAttribute("aria-describedby")).toBe("code-hint");
    expect(text(plain).toLowerCase()).not.toContain("relay");
  });

  it("says Connecting through the relay, by its host name, and what a relay is", () => {
    const root = show("relay-connecting");
    expect(root.dataset.screen).toBe("connecting");
    expect(text(root.querySelector("#connecting-h"))).toBe(
      "Connecting through the relay relay.example.org",
    );
    expect(text(root.querySelector(".relay-explained"))).toBe(
      "The relay only passes encrypted traffic on. It cannot look into the session, and it gives nobody access to this computer.",
    );
    // The band still says that nobody has access, and Cancel is there.
    expect(band(root)).toContain("NO ACCESS");
    const cancel = buttonNamed(root, /Cancel/);
    expect(cancel?.dataset.action).toBe("stop");
    expect(cancel?.getAttribute("aria-keyshortcuts")).toBe("Escape");
    expect(text(root.querySelector("#announce"))).toBe(
      "Connecting through the relay relay.example.org. No access.",
    );
    // On one network the screen is what it was.
    const lan = show("connecting");
    expect(text(lan.querySelector("#connecting-h"))).toBe("Connecting to your technician");
    expect(lan.querySelector(".relay-explained")).toBeNull();
    expect(text(lan.querySelector("#announce"))).toBe("Connecting. No access.");
  });

  it("says that nobody waits on the code, in plain words with a next step", () => {
    const root = show("find-nobody-waits");
    expect(root.dataset.screen).toBe("find");
    const region = alert(root);
    expect(region?.getAttribute("role")).toBe("alert");
    expect(text(region?.querySelector("b"))).toBe("Nobody is waiting on that code");
    expect(text(region)).toContain(
      "The relay relay.example.org found nobody waiting on the code you typed. It may be mistyped or already used, or your technician stopped waiting.",
    );
    expect(text(region?.querySelector(".next"))).toBe(
      "Nobody has access to this computer. Ask your technician for the code on their screen and type it again. A code works once.",
    );
    // Neither the backend's word for it nor a session of unknown cause.
    expect(text(region)).not.toContain("rendezvous");
    expect(text(region)).not.toContain("The session failed");
    // The code field is there to type the code again.
    expect(root.querySelector("#code")).not.toBeNull();
    expect(band(root)).toContain("NO ACCESS");
  });

  it.each([
    ["find-relay-unreachable", "CretLink could not reach the relay", "could not connect to the relay at relay.example.org:443", "Check that this computer is online"],
    ["find-relay-untrusted", "This computer does not trust the relay", "showed no certificate that this computer trusts", "Tell your technician that this computer does not trust the certificate"],
    ["find-relay-key", "The relay did not let this copy in", "does not know the admission key of this copy", "Ask your technician for a new copy of CretLink"],
  ])("%s: says what happened in its own words, with the next step for a customer", (name, title, message, next) => {
    const root = show(name);
    expect(root.dataset.screen).toBe("find");
    const region = alert(root);
    expect(text(region?.querySelector("b"))).toBe(title);
    expect(text(region)).toContain(message);
    expect(text(region?.querySelector(".next"))).toContain(next);
    expect(text(region)).not.toContain("The session failed");
    expect(text(region)).not.toContain("CretLink could not reach your technician");
    expect(root.querySelector("#stop")).toBeNull();
  });

  it("falls back to telling the technician when a relay failure brings no next step", () => {
    const root = show("find-relay-key", {
      view: { ...FIXTURES["find-relay-key"]!.view!, failure_next: null },
    });
    expect(text(alert(root)?.querySelector(".next"))).toBe(
      "Nobody has access to this computer. Tell your technician what this says.",
    );
  });

  it("says that the relay may have stopped when the connection is lost before anyone had control", () => {
    const failed = show("find-relay-lost");
    const region = alert(failed);
    expect(text(region?.querySelector("b"))).toBe("The connection through the relay was lost");
    expect(text(region)).toContain(
      "The connection through the relay relay.example.org was lost. The relay may have stopped, or one of the two computers went offline.",
    );
    expect(text(region?.querySelector(".next"))).toBe(
      "Nobody has access to this computer any more. Nothing connects again by itself: ask your technician for a new code when you want to start again.",
    );
    // A connection that ended without a failure says it with the relay too.
    const stopped = show("find-relay-lost-before-code");
    expect(text(stopped)).toContain(
      "The connection through the relay relay.example.org was lost before anyone got access. Nothing was shared.",
    );
  });

  it.each(["relay-sac", "relay-consent", "relay-live", "relay-end"])(
    "%s: says that the session runs through the relay, by its host name",
    (name) => {
      expect(text(show(name).querySelector(".via-relay"))).toBe(
        "Through the relay relay.example.org · it only passes encrypted traffic on",
      );
    },
  );

  it("leaves the code, the decision and Stop of a relayed session what they are on one network", () => {
    const sac = show("relay-sac");
    expect(sac.querySelector(".sac")?.getAttribute("aria-label")).toBe("Approval code 482 913");
    expect(buttonNamed(sac, /It doesn't match/)?.dataset.action).toBe("stop");
    const consent = show("relay-consent");
    expect(text(consent.querySelector("#disclosure"))).toBe(DISCLOSURE_TEXT);
    expect(buttonNamed(consent, "Decline")?.dataset.action).toBe("decline");
    expect(buttonNamed(consent, "Allow full control")?.dataset.action).toBe("allow");
    const live = show("relay-live");
    expect(band(live)).toContain("UNDER FULL CONTROL");
    expect(live.querySelector<HTMLButtonElement>("#stop")?.dataset.action).toBe("stop");
  });

  it("says that the relay may have stopped when a relayed session loses its connection", () => {
    const root = show("relay-end-lost");
    expect(root.dataset.screen).toBe("end");
    expect(text(root.querySelector("#end-title"))).toBe("The connection was lost");
    const note = root.querySelector(".relay-lost");
    expect(note?.getAttribute("role")).toBe("note");
    expect(text(note)).toBe(
      "The connection through the relay relay.example.org was lost. The relay may have stopped, or one of the two computers went offline. Nobody has access to this computer any more. Nothing connects again by itself: ask your technician for a new code when you want to start again.",
    );
    // The cleanup is still not shown as a success.
    expect(text(root)).toContain("Cleanup not confirmed.");
    // A session the customer stopped says nothing of the kind.
    expect(show("relay-end").querySelector(".relay-lost")).toBeNull();
  });

  it("says nothing about a relay in a session on one network", () => {
    for (const name of ["connecting", "sac", "consent", "live-running", "end-by-customer", "end-lost"]) {
      const root = show(name);
      expect(root.querySelector(".via-relay")).toBeNull();
      expect(root.querySelector(".relay-lost")).toBeNull();
      expect(text(root).toLowerCase()).not.toContain("relay");
    }
  });
});

describe("compare (#c-sac)", () => {
  it("asks whether the technician reads this code, shows it grouped, and offers It doesn't match — stop", () => {
    const root = show("sac");
    expect(text(root)).toContain("Does Sam read you this code?");
    expect(text(root)).toContain("Name as the technician's computer reports it.");
    expect(root.querySelector(".sac")?.getAttribute("aria-label")).toBe("Approval code 482 913");
    expect(text(root)).toContain("Waiting for Sam to confirm…");
    const stop = buttonNamed(root, "It doesn't match — stop");
    expect(stop?.dataset.action).toBe("stop");
    expect(stop?.getAttribute("aria-keyshortcuts")).toBe("Escape");
  });
});

describe("your decision (#c-consent)", () => {
  it("shows the scannable summary above the verbatim disclosure", () => {
    const root = show("consent");
    expect(text(root)).toContain("Allow Sam full control?");
    const risk = root.querySelector(".risk");
    const disclosure = root.querySelector("#disclosure");
    expect(risk?.querySelectorAll("li")).toHaveLength(4);
    expect(text(risk)).toContain("send output, files and screenshots to their AI service");
    expect(disclosure?.textContent).toBe(DISCLOSURE_TEXT);
    // The summary comes first; the disclosure stays visible.
    expect(risk && disclosure && risk.compareDocumentPosition(disclosure)).toBe(
      Node.DOCUMENT_POSITION_FOLLOWING,
    );
    expect(disclosure?.closest("[hidden]")).toBeNull();
  });

  it("makes Decline as prominent as Allow, and focuses neither", () => {
    const root = show("consent");
    const decline = buttonNamed(root, "Decline");
    const allow = buttonNamed(root, "Allow full control");
    expect(decline?.dataset.action).toBe("decline");
    expect(allow?.dataset.action).toBe("allow");
    expect(decline?.classList.contains("big")).toBe(true);
    expect(allow?.classList.contains("big")).toBe(true);
    expect(decline?.classList.contains("grow")).toBe(true);
    expect(allow?.classList.contains("grow")).toBe(true);
    expect(decline?.hasAttribute("data-autofocus")).toBe(false);
    expect(allow?.hasAttribute("data-autofocus")).toBe(false);
  });
});

describe("under full control (#c-live)", () => {
  it("shows what is happening now: purpose, source, elapsed time and the real operation", () => {
    const now = show("live-running").querySelector(".now");
    expect(text(now?.querySelector(".t"))).toBe("Reading the Windows system log");
    expect(text(now)).toContain("Sam's AI assistant");
    expect(text(now?.querySelector(".elapsed"))).toBe("12 s");
    expect(now?.querySelector("code")?.textContent).toContain(
      "Get-WinEvent -LogName System -MaxEvents 50",
    );
  });

  it("says Nothing is running when nothing runs", () => {
    expect(text(show("live-none").querySelector(".now .t"))).toBe("Nothing is running");
  });

  it("has Screen, Files and Commands tiles in words", () => {
    const tiles = Array.from(show("live-running").querySelectorAll(".tile")).map(text);
    expect(tiles).toEqual([
      "Screen being controlled",
      "Files 1 sent · 1 received",
      "3 commands 1 failed",
    ]);
    expect(text(show("live-none").querySelector(".tile"))).toBe("Screen not used");
    const observed = show("live-none", {
      view: {
        ...FIXTURES["live-none"]!.view!,
        screen: { observed_at_ms: NOW - 1000, controlled_at_ms: null },
      },
    });
    expect(text(observed.querySelector(".tile"))).toBe("Screen being observed");
    const earlier = show("live-none", {
      view: {
        ...FIXTURES["live-none"]!.view!,
        screen: { observed_at_ms: NOW - 600_000, controlled_at_ms: null },
      },
    });
    expect(text(earlier.querySelector(".tile"))).toBe("Screen not in use now · last 14:34");
  });

  it("lists what happened newest first, each with plain line, source, outcome, operation and time", () => {
    const items = Array.from(show("live-none").querySelectorAll(".tl .ti"));
    const lines = items.map((li) => text(li.querySelector(".t")));
    expect(lines).toEqual([
      "Reading the Windows system log",
      "Took a picture of your screen",
      "Copied a file to this computer",
      "Sent setupapi.dev.log to Sam",
      "Checked the Wi-Fi driver",
      "Used the terminal",
      "You allowed full control",
    ]);
    const failed = items[4];
    expect(failed?.classList.contains("fail")).toBe(true);
    expect(text(failed)).toContain("failed · error code 1");
    expect(text(failed)).toContain("netsh wlan show drivers");
    expect(text(items[5])).toContain("Sam · terminal");
    expect(text(items[2])).toMatch(/Sam finished 18\.4 MB/);
    expect(text(items[0])).toContain("14:43:48");
  });

  it("shows that the technician looks through files, with the folder, as one item that follows along (ST-M010-002)", () => {
    // The item as the customer's side builds it from the technician's looks:
    // one id for as long as the looks follow one another.
    const looking = (folder: string): ActivityEntry => ({
      entry: "item",
      item: {
        id: "browse-1",
        source: "technician",
        description: "Looked through files and folders",
        kind: "file_operation",
        operation: `list the folder ${folder}`,
        path: folder,
        state: { state: "done" },
        startedAt: new Date(NOW - 180_000).toISOString(),
        durationMs: 170_000,
      },
    });
    let feed = upsertActivity(FEED_DONE, looking("C:\\Users\\ann"));
    feed = upsertActivity(feed, looking("C:\\Users\\ann\\Documents"));
    expect(feed).toHaveLength(FEED_DONE.length + 1);

    const root = show("live-none", { activity: feed });
    const items = Array.from(root.querySelectorAll(".tl .ti"));
    // One item, the newest, and it names the folder that is looked at now.
    expect(items.filter((li) => text(li).includes("Looked through files and folders"))).toHaveLength(1);
    const item = items[0];
    expect(text(item?.querySelector(".t"))).toBe("Looked through files and folders");
    expect(text(item?.querySelector(".who"))).toBe("Sam");
    expect(text(item?.querySelector(".res"))).toBe("finished");
    expect(text(item?.querySelector("code"))).toBe(
      "Operation: list the folder C:\\Users\\ann\\Documents",
    );
    expect(text(item)).not.toContain("left this computer");
    // It is among the files, and it is not something that is running.
    const files = Array.from(show("live-none", { activity: feed, filter: "files" }).querySelectorAll(".tl .ti"));
    expect(text(files[0])).toContain("Looked through files and folders");
    expect(text(root.querySelector(".now"))).toContain("Nothing is running");
  });

  it("says a file left this computer in words, and marks it amber", () => {
    const sent = Array.from(show("live-none").querySelectorAll(".tl .ti")).find((li) =>
      text(li).includes("setupapi"),
    );
    expect(sent?.classList.contains("out")).toBe(true);
    expect(text(sent?.querySelector(".flag"))).toBe("left this computer");
    expect(text(sent)).toContain("C:\\Windows\\INF\\setupapi.dev.log");
  });

  it("does not repeat the running item below Happening now", () => {
    const lines = Array.from(show("live-running").querySelectorAll(".tl .ti .t")).map(text);
    expect(lines).not.toContain("Reading the Windows system log");
  });

  it("counts the items that were not shown", () => {
    const gap = show("live-gap").querySelector(".tl .ti.gap");
    expect(text(gap)).toContain("12 items were not shown");
  });

  it("filters by commands, files and screen", () => {
    const lines = (name: string, filter: CustomerSnapshot["filter"]): string[] =>
      Array.from(show(name, { filter }).querySelectorAll(".tl .ti .t")).map(text);
    expect(lines("live-none", "files")).toEqual([
      "Copied a file to this computer",
      "Sent setupapi.dev.log to Sam",
    ]);
    expect(lines("live-none", "screen")).toEqual(["Took a picture of your screen"]);
    expect(lines("live-none", "commands")).toEqual([
      "Reading the Windows system log",
      "Checked the Wi-Fi driver",
      "Used the terminal",
    ]);
    // Gaps are kept under every filter.
    expect(lines("live-gap", "screen")).toContain("12 items were not shown");
    const pressed = show("live-files").querySelectorAll('[data-action="filter"][aria-pressed="true"]');
    expect(Array.from(pressed).map(text)).toEqual(["Files"]);
  });

  it("has Stop now (Escape) as a native button with the note, and Make small", () => {
    const root = show("live-running");
    const stop = root.querySelector<HTMLButtonElement>("button#stop");
    expect(stop?.tagName).toBe("BUTTON");
    expect(text(stop)).toBe("Stop now");
    expect(stop?.querySelector("kbd")?.textContent).toBe("Esc");
    expect(stop?.getAttribute("aria-keyshortcuts")).toBe("Escape");
    expect(text(root.querySelector("#stop-note"))).toBe(
      "Control ends immediately. You don't need Sam for this.",
    );
    expect(buttonNamed(root, "Make small")?.dataset.action).toBe("make-small");
  });

  it("asks Stop the session and close? when a close was held", () => {
    const root = show("live-close");
    const dialog = root.querySelector('[role="alertdialog"]');
    expect(text(dialog?.querySelector("h3"))).toBe("Stop the session and close?");
    expect(dialog?.getAttribute("aria-modal")).toBe("true");
    expect(buttonNamed(root, "Stop and close")?.dataset.action).toBe("confirm-close");
    expect(buttonNamed(root, "Keep the session")?.dataset.action).toBe("cancel-close");
    const closing = show("live-closing");
    expect(text(closing.querySelector('[role="alertdialog"]'))).toContain("cleaning up and then closes");
  });
});

describe("untrusted text", () => {
  it("renders names, purposes, operations and paths as text, never as markup", () => {
    const root = show("live-xss");
    expect(root.querySelector("img")).toBeNull();
    expect(root.querySelector("script")).toBeNull();
    expect(text(root)).toContain(XSS);
    const find = show("find", {
      discovery: {
        state: "done",
        consoles: [
          { instance_name: XSS, display_name: XSS, address: XSS, protocol_version: null, dialable: true },
        ],
      },
    });
    expect(find.querySelector("img, script")).toBeNull();
    expect(text(find.querySelector(".pick"))).toContain(XSS);
    const mini = renderMini({ ...FIXTURES["live-xss"]!, window: "mini" });
    expect(mini.querySelector("img, script")).toBeNull();
  });
});

describe("session ended (#c-end)", () => {
  it.each([
    ["end-by-customer", "You stopped the session"],
    ["end-by-technician", "Sam ended the session"],
    ["end-lost", "The connection was lost"],
    ["end-failed", "The session ended because of a problem"],
  ])("%s: the heading says who ended it", (name, heading) => {
    expect(text(show(name).querySelector("#end-title"))).toBe(heading);
  });

  it("shows the counts", () => {
    expect(text(show("end-by-customer").querySelector(".stats"))).toBe(
      "3 commands · 1 failed 1 · 1 files sent · received 1 screenshots",
    );
  });

  it("lists each cleanup step, failures first", () => {
    const lines = Array.from(show("end-by-technician").querySelectorAll(".cleanup .okline")).map(text);
    expect(lines[0]).toContain("1 thing stayed on this computer.");
    expect(lines[1]).toContain("Not done: staged file left");
    expect(lines.slice(2)).toEqual(["Done: connection closed", "Done: 2 running jobs stopped"]);
    expect(text(show("end-failed").querySelector(".cleanup"))).toContain("Cleanup failed.");
  });

  it("never shows an unknown cleanup as a success", () => {
    const cleanup = text(show("end-lost").querySelector(".cleanup"));
    expect(cleanup).toContain("Cleanup not confirmed.");
    expect(cleanup).not.toContain("Cleaned up. Nothing");
  });

  it("says it is cleaning up until the report arrives", () => {
    const root = show("end-pending");
    expect(text(root.querySelector(".cleanup"))).toContain("Cleaning up…");
    expect(buttonNamed(root, "Save a copy")?.disabled).toBe(true);
  });

  it("offers Save a copy, then the saved path with Open, and Close CretLink", () => {
    const root = show("end-by-customer");
    expect(buttonNamed(root, "Save a copy")?.dataset.action).toBe("save");
    expect(buttonNamed(root, "Close CretLink")?.dataset.action).toBe("close-app");
    const saved = show("end-saved");
    expect(text(saved.querySelector(".record"))).toContain(
      "C:\\Users\\jdoe\\Documents\\CretLink\\2026-11-03 ACME-PC-07.html",
    );
    const open = buttonNamed(saved, "Open the saved record");
    expect(open?.dataset.action).toBe("open-report");
    expect(open?.dataset.arg).toBe("C:\\Users\\jdoe\\Documents\\CretLink\\2026-11-03 ACME-PC-07.html");
    expect(root.querySelector('[data-action="stop"]')).toBeNull();
  });
});

describe("compact bar (#c-mini)", () => {
  it("shows the state, the current action, the counts, Stop and Open", () => {
    const root = show("mini-running");
    expect(text(root.querySelector(".minibar"))).toContain("Sam has full control");
    expect(text(root.querySelector(".minibar small"))).toBe(
      "screen controlled · 3 commands · 1 file sent",
    );
    expect(text(root.querySelector(".mininow b"))).toBe("Reading the Windows system log");
    expect(buttonNamed(root, "Stop the session now")?.dataset.action).toBe("stop");
    expect(buttonNamed(root, "Open the CretLink window")?.dataset.action).toBe("open-main");
    expect(root.querySelector('[aria-live="polite"]')?.textContent).not.toBe("");
    // The state is in words for screen readers too.
    expect(root.querySelector(".minibar")?.getAttribute("aria-label")).toBe("Under full control");
  });

  it("says when nothing is running, and outside full control what is pending", () => {
    expect(text(show("mini-idle").querySelector(".mininow"))).toContain("Nothing is running");
    expect(text(show("mini-consent").querySelector(".minibar"))).toContain("Your decision is waiting");
  });

  it("has no Stop without a session", () => {
    const root = show("mini-idle", { view: viewIn({ phase: "stopped", reason: "customer_requested" }) });
    expect(root.querySelector('[data-action="stop"]')).toBeNull();
  });
});
