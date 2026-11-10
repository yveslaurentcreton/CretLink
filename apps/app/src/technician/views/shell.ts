// The technician window (ST-M009-008): the navigation, whose foot names this
// computer and this copy, the alert and live regions, and the screen of the
// snapshot. The window draws no bar of its own: the operating system's title
// bar is the only one, and it carries the name and the icon.
// `renderTechnician()` is a pure function of the snapshot; the controller
// patches the live DOM with its result (`morph`), so the alert and live
// regions, focus, scroll positions and the terminal survive.

import { type Child, append, h } from "../../shared/dom";
import { type IconName, icon } from "../../shared/icons";
import {
  type NavPage,
  type Screen,
  type TechnicianSnapshot,
  phaseAnnouncement,
  screenFor,
} from "../model";
import { aiView } from "./ai";
import { flowView } from "./flow";
import { homeView } from "./home";
import { liveView } from "./live";
import { endView, reportPage } from "./report";
import { sessionsView } from "./sessions";
import { settingsView } from "./settings";

const NAV: ReadonlyArray<{ page: NavPage; label: string; icon: IconName }> = [
  { page: "home", label: "Home", icon: "home" },
  { page: "sessions", label: "Sessions", icon: "history" },
  { page: "ai", label: "AI assistants", icon: "spark" },
  { page: "settings", label: "Settings", icon: "gear" },
];

/**
 * This computer and this copy, in words: the host name, the operating system
 * as the backend names it (such as "Windows 11 Pro 25H2"), the product with
 * its version, and whether this copy is installed or runs once. What the
 * backend has not said yet is empty.
 */
export function thisComputer(s: TechnicianSnapshot): {
  hostname: string;
  os: string;
  product: string;
  install: string;
} {
  const version = s.app?.version ?? s.version;
  return {
    hostname: s.app?.hostname || s.console.technician.hostname,
    os: s.app?.os || s.console.technician.os,
    product: version ? `CretLink ${version}` : "CretLink",
    install:
      s.app?.install === "installed"
        ? "installed"
        : s.app?.install === "run_once"
          ? "runs once"
          : "",
  };
}

/** "<first> · <second>"; the second part stays in one piece when the line breaks. */
function pair(first: string, second: string): Child[] {
  return [
    first || null,
    first && second ? " · " : null,
    second ? h("span", { class: "keep" }, second) : null,
  ];
}

/**
 * The foot of the navigation (mock `.side .foot`): this computer as
 * "<host> · <operating system>", and this copy as "CretLink <version> ·
 * <installed or runs once>" on the next line. A screen reader is told what
 * the first line is.
 */
function navFoot(s: TechnicianSnapshot): HTMLElement {
  const { hostname, os, product, install } = thisComputer(s);
  return h(
    "div",
    { class: "foot" },
    hostname || os
      ? h(
          "div",
          { class: "this-computer" },
          h("span", { class: "sr-only" }, "This computer: "),
          pair(hostname, os),
        )
      : h("div", { class: "this-computer" }, "This computer"),
    h("div", { class: "this-copy" }, pair(product, install)),
  );
}

function sideNav(s: TechnicianSnapshot, screen: Screen): HTMLElement {
  const current: NavPage =
    screen === "report"
      ? "sessions"
      : screen === "ai" || screen === "settings" || screen === "sessions"
        ? screen
        : "home";
  return h(
    "nav",
    { class: "side", "aria-label": "Main" },
    NAV.map((item) =>
      h(
        "button",
        {
          type: "button",
          class: item.page === current ? "on" : null,
          "data-action": "nav",
          "data-arg": item.page,
          "aria-current": item.page === current ? "page" : null,
        },
        icon(item.icon),
        item.label,
      ),
    ),
    navFoot(s),
  );
}

/** The assertive alert region; it stays in the DOM so it is announced (REQ-N-015). */
function alertRegion(s: TechnicianSnapshot): HTMLElement {
  const region = h("div", { class: "alert", role: "alert", id: "alert" });
  const error = s.error ?? phaseError(s);
  if (error) {
    append(region, [
      icon("alert"),
      h(
        "div",
        { class: "alert-body" },
        h("b", null, error.title),
        h("p", null, error.message),
        h("p", { class: "next" }, error.next),
      ),
      s.error
        ? h(
            "button",
            { type: "button", class: "btn sm ghost", "data-action": "dismiss-error" },
            "Dismiss",
          )
        : null,
    ]);
  }
  return region;
}

/** A failed session, in plain language with a next step. */
function phaseError(
  s: TechnicianSnapshot,
): { title: string; message: string; next: string } | null {
  const phase = s.console.phase;
  if (phase.phase !== "failed") return null;
  return {
    title: "The session failed",
    message: `${phase.message} (${phase.stage})`,
    next: "Nobody has access to the customer computer. Start a new session when you are ready.",
  };
}

function screenContent(s: TechnicianSnapshot, screen: Screen): Child {
  switch (screen) {
    case "home":
      return homeView(s);
    case "sessions":
      return sessionsView(s);
    case "report":
      return reportPage(s);
    case "ai":
      return aiView(s);
    case "settings":
      return settingsView(s);
    case "end":
      return endView(s);
    case "wait":
    case "connecting":
    case "compare":
    case "consent":
    case "stopping":
      return flowView(s, screen);
    case "live":
      return null;
  }
}

/** The whole technician window for one snapshot. */
export function renderTechnician(s: TechnicianSnapshot): HTMLElement {
  const screen = screenFor(s);
  const root = h("div", { class: "cl-ui tech tapp", "data-screen": screen });
  root.append(
    h("a", { class: "skip-link", href: "#main", "data-action": "skip" }, "Skip to main content"),
    alertRegion(s),
  );
  if (screen === "live") {
    root.append(liveView(s));
  } else {
    root.append(
      h(
        "div",
        { class: "tshell" },
        sideNav(s, screen),
        h(
          "main",
          { class: "tmain", id: "main", tabindex: -1, "data-key": `main-${screen}` },
          screenContent(s, screen),
        ),
      ),
    );
  }
  root.append(
    h(
      "div",
      { class: "sr-only", role: "status", "aria-live": "polite", id: "announce" },
      s.notice ?? phaseAnnouncement(s),
    ),
  );
  return root;
}
