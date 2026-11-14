// The customer's main window (ST-M009-009): a skip link, the state band in
// words on every screen, the assertive alert region, the screen of the
// snapshot, the close question, and a polite live region. `renderCustomer()`
// is a pure function of the snapshot; the controller patches the live DOM with
// its result (`morph`), so focus, the typed code and scroll positions survive.

import { type Child, append, h } from "../../shared/dom";
import { icon } from "../../shared/icons";
import {
  type CustomerSnapshot,
  RELAY_LOST_STAGE,
  RENDEZVOUS_STAGE,
  type Screen,
  type UiError,
  announcement,
  bandFor,
  capName,
  isRelayFailure,
  nobodyWaitsWords,
  relayFailureTitle,
  relayLostWords,
  relayNamed,
  screenFor,
  technicianName,
} from "../model";
import { adminView, declinedView } from "./admin";
import { band, button } from "./common";
import { endView } from "./end";
import { findView } from "./find";
import { keepView, removeView, removedView } from "./keep";
import { compareView, connectingView, consentView, stoppingView } from "./flow";
import { liveView, since } from "./live";

/** A failed session, in plain language with a next step. */
function phaseError(s: CustomerSnapshot): UiError | null {
  const phase = s.view?.status.phase;
  if (phase?.phase !== "failed") return null;
  const nobody = "Nobody has access to this computer.";
  // A relay code that nobody waits on (ST-M007-012, ST-M007-013).
  if (phase.stage === RENDEZVOUS_STAGE) return nobodyWaitsWords(relayNamed(s));
  // A session through a relay whose connection was lost before control.
  if (phase.stage === RELAY_LOST_STAGE) {
    const relay = relayNamed(s);
    return {
      title: "The connection through the relay was lost",
      ...(relay ? relayLostWords(relay) : { message: phase.message, next: nobody }),
    };
  }
  // A relay that could not be used: the backend's words for a customer, and
  // its next step.
  if (isRelayFailure(phase.stage)) {
    return {
      title: relayFailureTitle(phase.stage),
      message: phase.message,
      next: s.view?.failure_next ?? `${nobody} Tell your technician what this says.`,
    };
  }
  const connect = phase.stage === "connect";
  return {
    title: connect ? "CretLink could not reach your technician" : "The session failed",
    message: phase.message,
    next: connect
      ? "Nobody has access to this computer. Check the code with your technician and try again."
      : "Nobody has access to this computer any more. You can start again when your technician is ready.",
  };
}

function alertRegion(s: CustomerSnapshot, screen: Screen): HTMLElement {
  const region = h("div", { class: "alert", role: "alert", id: "alert" });
  const error = s.error ?? (screen === "find" || screen === "end" ? phaseError(s) : null);
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
        ? button("Dismiss", "dismiss-error", { variant: "sm ghost" })
        : null,
    ]);
  }
  return region;
}

function content(s: CustomerSnapshot, screen: Screen): Child {
  switch (screen) {
    case "loading":
      return h(
        "main",
        { class: "cbody center", id: "main", tabindex: -1, "data-key": "main-loading" },
        h("span", { class: "spinner lg", "aria-hidden": "true" }),
        h("p", null, "Starting CretLink…"),
      );
    case "admin":
      return adminView(s);
    case "declined":
      return declinedView(s);
    case "find":
      return findView(s);
    case "connecting":
      return connectingView(s);
    case "compare":
      return compareView(s);
    case "consent":
      return consentView(s);
    case "live":
      return liveView(s);
    case "stopping":
      return stoppingView();
    case "end":
      return endView(s);
    case "keep":
      return keepView(s);
    case "remove":
      return removeView(s);
    case "removed":
      return removedView(s);
  }
}

/** "Stop the session and close?", when a close was held during a session. */
function closeQuestion(s: CustomerSnapshot): HTMLElement | null {
  if (!s.confirmClose && !s.closing) return null;
  const name = technicianName(s);
  const body = s.closing
    ? [
        h("h3", { id: "close-title" }, "Stopping and closing"),
        h(
          "p",
          { class: "row" },
          h("span", { class: "spinner", "aria-hidden": "true" }),
          "Control has ended. CretLink is cleaning up and then closes.",
        ),
      ]
    : [
        h("h3", { id: "close-title" }, "Stop the session and close?"),
        h(
          "p",
          null,
          `Control ends at once and CretLink cleans up, then closes. ${capName(name)} can no longer reach this computer.`,
        ),
        h(
          "div",
          { class: "row end" },
          button("Keep the session", "cancel-close", { autofocus: true }),
          button("Stop and close", "confirm-close", { variant: "danger", icon: "stop" }),
        ),
      ];
  return h(
    "div",
    { class: "scrim", "data-key": "close-question" },
    h(
      "div",
      {
        class: "dialog card",
        role: "alertdialog",
        "aria-modal": "true",
        "aria-labelledby": "close-title",
        id: "close-dialog",
      },
      body,
    ),
  );
}

/** The whole main window for one snapshot. */
export function renderCustomer(s: CustomerSnapshot): HTMLElement {
  const screen = screenFor(s);
  const root = h("div", { class: "cl-ui cust capp", "data-screen": screen });
  root.append(
    h("a", { class: "skip-link", href: "#main", "data-action": "skip" }, "Skip to main content"),
    band(bandFor(s, screen), screen === "live" ? since(s) : null),
    alertRegion(s, screen),
  );
  append(root, [content(s, screen)]);
  const question = closeQuestion(s);
  if (question) root.append(question);
  root.append(
    h(
      "div",
      { class: "sr-only", role: "status", "aria-live": "polite", id: "announce" },
      s.notice ?? announcement(s),
    ),
  );
  return root;
}
