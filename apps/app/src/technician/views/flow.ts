// The screens before full control (ST-M009-008): waiting for the customer
// (`#t-wait`), connecting, comparing the approval code (`#t-sac`), the
// customer deciding (`#t-consent`), and stopping. Each shows the access state
// in words, and Stop (Escape) is always available (REQ-F-018).
//
// A customer who is not on the technician's network is reached through the
// relay of this copy (ST-M007-013): the waiting screen offers **Use relay**,
// then shows the relay's host name and a relay code to read out. The relay
// only lets the two copies find each other; every screen after it is the one
// of a session on one network, with a line that says which relay the session
// runs through.

import { type Child, h } from "../../shared/dom";
import { icon } from "../../shared/icons";
import { type RelayRoute, selectedIsSimulated } from "../api";
import { DISCLOSURE_TEXT } from "../disclosure";
import type { ListenNetwork } from "../../shared/contract";
import {
  type Screen,
  type TechnicianSnapshot,
  currentTarget,
  groupCode,
  otherNetworks,
  relayCodeGroups,
  relayOffer,
  spokenCode,
  targetName,
  technicianPick,
  waitingNetwork,
  waitingRelay,
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

/**
 * That the session runs through a relay, and through which: on every screen
 * from the comparison of the approval code to the end (ST-M007-013). The
 * relay passes the encrypted session on; it grants nothing.
 */
export function viaRelay(relay: RelayRoute | undefined): HTMLElement | null {
  if (!relay) return null;
  return h(
    "p",
    { class: "via-relay", "data-key": "via-relay" },
    icon("wifi", "sm"),
    h("span", null, "Through the relay ", h("b", { class: "mono" }, relay.host)),
    h("span", { class: "muted" }, " · it only passes encrypted traffic on"),
  );
}

// **Not on the same network?** under the waiting card (ST-M007-013). With a
// relay it offers **Use relay**; without one it names Settings, where a relay
// is set up; a profile that cannot be used is said as such, with its file.
function relayOfferCard(s: TechnicianSnapshot): HTMLElement {
  const offer = relayOffer(s.relay);
  const busy = s.busy !== undefined;
  const settings = button("Open Settings", "nav", { arg: "settings", variant: "sm" });
  let text: Child;
  let action: HTMLElement;
  switch (offer.state) {
    case "ready":
      text = [
        "Use your relay ",
        h("b", { class: "mono" }, offer.host),
        ". It only passes encrypted traffic on and cannot look into the session.",
      ];
      action = button("Use relay…", "use-relay", {
        id: "use-relay",
        disabled: busy,
        label: `Use the relay ${offer.host}`,
      });
      break;
    case "problem":
      text = [
        h("b", null, "The relay settings of this copy cannot be used. "),
        offer.message,
        " The file is ",
        h("span", { class: "mono" }, offer.file),
        ". Settings, under Relay, shows it.",
      ];
      action = settings;
      break;
    default:
      text = offer.runOnce
        ? [
            "This copy has no relay. To help someone on another network, put a file ",
            h("span", { class: "mono" }, "cretlink.toml"),
            " for your relay next to the program; Settings, under Relay, shows how.",
          ]
        : "This copy has no relay. To help someone on another network, set up your relay in Settings, under Relay.";
      action = settings;
  }
  return h(
    "div",
    {
      class: "card row relay-offer",
      id: "relay-offer",
      role: "group",
      "aria-labelledby": "relay-offer-h",
    },
    icon("wifi"),
    h(
      "div",
      { class: "relay-offer-text" },
      h("b", { id: "relay-offer-h" }, "Not on the same network?"),
      h("div", { class: "muted" }, text),
    ),
    action,
  );
}

// Waiting on the relay (ST-M007-013): the relay's host name, the relay code
// to read out with Copy, and in words whether the relay holds it yet. The
// code is drawn in four outlined groups and named a relay code, so that it is
// not taken for the approval code that follows.
function relayWaitView(s: TechnicianSnapshot, relay: RelayRoute): HTMLElement {
  const phase = s.console.phase;
  const code = phase.phase === "listening" ? phase.code : "";
  return h(
    "div",
    { class: "screen wait relay-wait" },
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
      h("h2", null, "Waiting for the customer on your relay"),
      h(
        "p",
        null,
        "Ask them to open CretLink, choose ",
        h("b", null, "Get help"),
        " and type this code into the code field.",
      ),
      h("div", { class: "label", id: "relay-code-label" }, "Relay code · read it to the customer"),
      h(
        "p",
        {
          class: "relay-code",
          id: "relay-code",
          role: "img",
          "aria-label": `Relay code ${spokenCode(code)}`,
        },
        relayCodeGroups(code).map((group) => h("span", null, group)),
      ),
      h(
        "p",
        { class: "muted conn-addr" },
        "Relay ",
        h("b", { class: "mono", id: "relay-host" }, relay.host),
        " · ",
        h(
          "button",
          {
            type: "button",
            class: "linkbtn",
            "data-action": "copy",
            "data-arg": code,
            "aria-label": "Copy the relay code",
          },
          "Copy",
        ),
      ),
      h(
        "p",
        { class: "relay-state", id: "relay-state", role: "status" },
        relay.admitted
          ? "The relay holds this code. It works once, for as long as this screen is open."
          : `Connecting to the relay ${relay.host}…`,
      ),
      h(
        "p",
        { class: "muted relay-note" },
        "The code only says where to meet. You compare the approval code and the customer gives permission next, as on one network.",
      ),
    ),
  );
}

function waitView(s: TechnicianSnapshot): HTMLElement {
  const phase = s.console.phase;
  // The change to the relay stops the listener first: until the relay's
  // waiting state is there, the screen shows no code at all.
  if (s.busy === "relay") return changingView(s, "Changing to your relay…");
  const relay = waitingRelay(s.console);
  if (relay) return relayWaitView(s, relay);
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
    switching ? null : relayOfferCard(s),
  );
}

/** The waiting screen while one way of waiting gives way to another. */
function changingView(s: TechnicianSnapshot, words: string): HTMLElement {
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
      h("p", { class: "muted conn-switching", role: "status" }, words),
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
      viaRelay(s.console.relay),
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
    viaRelay(s.console.relay),
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
      viaRelay(s.console.relay),
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
