// The screens before full control (ST-M009-008): waiting for the customer
// (`#t-wait`), connecting, comparing the approval code (`#t-sac`), the
// customer deciding (`#t-consent`), and stopping. Each shows the access state
// in words, and Stop (Escape) is always available (REQ-F-018).

import { h } from "../../shared/dom";
import { selectedIsSimulated } from "../api";
import { DISCLOSURE_TEXT } from "../disclosure";
import type { ListenNetwork } from "../../shared/contract";
import {
  type Screen,
  type TechnicianSnapshot,
  currentTarget,
  groupCode,
  otherNetworks,
  targetName,
  technicianPick,
  waitingNetwork,
} from "../model";
import { button, card, pill } from "./common";

export function flowView(s: TechnicianSnapshot, screen: Screen): HTMLElement {
  switch (screen) {
    case "wait":
      return waitView(s);
    case "connecting":
      return connectingView(s);
    case "compare":
      return compareView(s);
    case "consent":
      return consentView(s);
    default:
      return stoppingView(s);
  }
}

function stopButton(label: string, s: TechnicianSnapshot): HTMLButtonElement {
  return button(label, "stop", {
    id: "stop",
    kbd: "Esc",
    keys: "Escape",
    disabled: s.busy === "stop",
  });
}

/** A network as the waiting screen names it: the adapter, then its address. */
function networkName(network: ListenNetwork): HTMLElement {
  return h(
    "span",
    { class: "net-name" },
    h("b", null, network.adapter),
    " · ",
    h("span", { class: "mono" }, network.address),
  );
}

// The other networks this computer could wait on (ST-M009-016). A computer on
// a VPN or with a virtual adapter has several private addresses, and the
// customer can reach only one. The list is a plain disclosure: a button that
// says whether it is open, and one button for each network.
function networkChoice(s: TechnicianSnapshot): HTMLElement | null {
  const others = otherNetworks(s);
  if (others.length === 0) return null;
  const open = s.networksOpen === true;
  const switching = s.busy === "network";
  return h(
    "div",
    { class: "net" },
    h(
      "button",
      {
        type: "button",
        class: "linkbtn",
        id: "net-toggle",
        "data-action": "networks",
        "aria-expanded": open ? "true" : "false",
        "aria-controls": "net-list",
        disabled: switching,
      },
      "Wait on another network",
    ),
    open
      ? h(
          "div",
          { class: "net-panel", id: "net-list" },
          h(
            "p",
            { class: "muted", id: "net-hint" },
            "Choose the network your customer's computer is on. The connection code changes with it.",
          ),
          h(
            "ul",
            { class: "net-list", "aria-labelledby": "net-hint" },
            others.map((network) =>
              h(
                "li",
                { "data-key": network.address },
                networkName(network),
                network.pointToPoint ? h("span", { class: "muted" }, "point-to-point link") : null,
                button("Wait here", "network", {
                  arg: network.address,
                  variant: "sm",
                  disabled: switching,
                  label: `Wait on ${network.adapter}, ${network.address}`,
                }),
              ),
            ),
          ),
        )
      : null,
  );
}

function waitView(s: TechnicianSnapshot): HTMLElement {
  const phase = s.console.phase;
  // While the network changes there is no listener for a moment: the screen
  // then shows no code at all rather than the one that no longer works.
  const switching = s.busy === "network" || phase.phase !== "listening";
  const code = phase.phase === "listening" ? phase.code : "";
  const address = phase.phase === "listening" ? phase.address : "";
  const network = waitingNetwork(s);
  return h(
    "div",
    { class: "screen wait" },
    h(
      "div",
      { class: "row" },
      pill("none", "○ No access"),
      h("span", { class: "spacer" }),
      stopButton("Cancel", s),
    ),
    h(
      "div",
      { class: "card center" },
      h("span", { class: "spinner", "aria-hidden": "true" }),
      h("h2", null, "Waiting for the customer"),
      h(
        "p",
        null,
        "Ask them to open CretLink, choose ",
        h("b", null, "Get help"),
        " and select ",
        h("b", null, technicianPick(s.console)),
        ".",
      ),
      h(
        "div",
        { class: "label", id: "conn-label" },
        "Connection code · only if their app doesn't find this computer",
      ),
      switching
        ? h("p", { class: "muted conn-switching", role: "status" }, "Changing the network…")
        : h("div", { class: "conn-code", "aria-labelledby": "conn-label", role: "group" }, code),
      switching
        ? null
        : h(
            "p",
            { class: "muted conn-addr" },
            // The adapter says which network this is; the address alone does not.
            network ? h("b", { class: "net-adapter" }, network.adapter) : null,
            network ? " · " : null,
            h("span", { class: "mono" }, address),
            " · ",
            h(
              "button",
              {
                type: "button",
                class: "linkbtn",
                "data-action": "copy",
                "data-arg": code,
                "aria-label": "Copy the connection code",
              },
              "Copy",
            ),
          ),
      switching ? null : networkChoice(s),
    ),
  );
}

function connectingView(s: TechnicianSnapshot): HTMLElement {
  return h(
    "div",
    { class: "screen wait" },
    h(
      "div",
      { class: "row" },
      pill("none", "○ No access"),
      h("span", { class: "spacer" }),
      stopButton("Cancel", s),
    ),
    h(
      "div",
      { class: "card center" },
      h("span", { class: "spinner", "aria-hidden": "true" }),
      h("h2", null, "The customer is connecting"),
      h("p", null, "Setting up the encrypted connection. Nothing is shared yet."),
    ),
  );
}

function compareView(s: TechnicianSnapshot): HTMLElement {
  const phase = s.console.phase;
  const code = phase.phase === "awaiting_approval_code" ? phase.code : "";
  const [first, second] = groupCode(code);
  const target = currentTarget(s);
  const digits = (group: string): HTMLElement[] => Array.from(group).map((c) => h("span", null, c));
  return h(
    "div",
    { class: "screen compare center-col" },
    pill("wait", "◐ Connected · no access"),
    h("h2", null, "Read this code aloud to the customer"),
    h("p", null, "If nobody is in between, their screen shows the same six digits."),
    h(
      "p",
      { class: "sac", role: "img", "aria-label": `Approval code ${first} ${second}` },
      digits(first),
      h("i", { "aria-hidden": "true" }),
      digits(second),
    ),
    h(
      "p",
      { class: "muted reports" },
      "The customer computer reports ",
      h("b", null, target?.hostname ?? targetName(s)),
      target
        ? ` · ${target.os_version} · ${target.elevated ? "elevated" : "not elevated"}`
        : " · identity not reported yet",
    ),
    h(
      "div",
      { class: "row" },
      button("Codes don't match", "mismatch", {
        variant: "big",
        icon: "x",
        disabled: s.busy === "mismatch",
      }),
      button("Codes match", "compare", {
        variant: "primary big",
        icon: "check",
        disabled: s.busy === "compare",
      }),
    ),
    h("p", { class: "muted" }, "Press Esc to stop at any time."),
  );
}

function consentView(s: TechnicianSnapshot): HTMLElement {
  const host = targetName(s);
  return h(
    "div",
    { class: "screen consent" },
    h(
      "div",
      { class: "row" },
      pill("wait", "◐ Codes match · waiting for consent"),
      h("span", { class: "spacer" }),
      stopButton("Stop", s),
    ),
    h(
      "div",
      { class: "card center" },
      h("span", { class: "spinner", "aria-hidden": "true" }),
      h("h2", null, `${host} is deciding`),
      h("p", null, "Only the customer can grant control. It can't be done from here."),
      selectedIsSimulated(s.console)
        ? button("Simulate customer consent", "consent", {
            variant: "sm",
            disabled: s.busy === "consent",
          })
        : null,
    ),
    card(
      h("h4", { id: "disclosure-head" }, "What the customer reads"),
      h("p", { class: "disclose", "aria-labelledby": "disclosure-head" }, DISCLOSURE_TEXT),
    ),
  );
}

function stoppingView(s: TechnicianSnapshot): HTMLElement {
  return h(
    "div",
    { class: "screen wait" },
    h("div", { class: "row" }, pill("wait", "◐ Ending the session")),
    h(
      "div",
      { class: "card center" },
      h("span", { class: "spinner", "aria-hidden": "true" }),
      h("h2", null, "Ending the session"),
      h("p", null, `Closing the connection to ${targetName(s)}.`),
    ),
  );
}
