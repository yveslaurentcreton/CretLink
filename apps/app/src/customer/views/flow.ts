// The screens between choosing the technician and full control
// (ST-M009-009): connecting, comparing the code (`#c-sac`, REQ-F-005), your
// decision (`#c-consent`, REQ-F-006, REQ-S-003, REQ-S-016), and stopping.

import { h } from "../../shared/dom";
import { groupCode } from "../../shared/feed";
import { type IconName, icon } from "../../shared/icons";
import { relayLabel, trustWords, viaRelay } from "../../shared/relay";
import { type CustomerSnapshot, capName, sessionRelay, technicianName } from "../model";
import { button } from "./common";

function stopFooter(label: string, s: CustomerSnapshot, variant = "danger block big"): HTMLElement {
  return h(
    "div",
    { class: "cfoot" },
    button(label, "stop", {
      variant,
      icon: "x",
      id: "stop",
      kbd: "Esc",
      keys: "Escape",
      disabled: s.busy === "stop",
    }),
  );
}

function provenance(s: CustomerSnapshot): HTMLElement | null {
  const technician = s.view?.technician;
  if (!technician) return null;
  return h("p", { class: "muted provenance" }, `Name ${technician.provenance}.`);
}

export function connectingView(s: CustomerSnapshot): HTMLElement[] {
  // A relay code: the connection goes through the relay of this copy
  // (ST-M007-013). The screen says through which, where this copy connects
  // when that is another host than the name says, what vouches for the name
  // when it is a file of this copy's profile, and what a relay is.
  const relay = sessionRelay(s);
  return [
    h(
      "main",
      { class: "cbody center", id: "main", tabindex: -1, "data-key": "main-connecting" },
      h("span", { class: "spinner lg", "aria-hidden": "true" }),
      relay
        ? h(
            "h3",
            { id: "connecting-h" },
            "Connecting through the relay ",
            h("span", { class: "relay-host" }, relayLabel(relay)),
          )
        : h("h3", { id: "connecting-h" }, "Connecting to your technician"),
      relay
        ? h(
            "p",
            { class: "relay-explained" },
            "The relay only passes encrypted traffic on. It cannot look into the session, and it gives nobody access to this computer.",
          )
        : null,
      relay && trustWords(relay)
        ? h("p", { class: "muted relay-trust", id: "relay-trust" }, trustWords(relay))
        : null,
      h("p", null, "Setting up an encrypted connection. Nothing is shared yet."),
    ),
    stopFooter("Cancel", s, "block big"),
  ];
}

export function compareView(s: CustomerSnapshot): HTMLElement[] {
  const name = technicianName(s);
  const code = (s.view?.approval_code ?? "").replace(/[^0-9A-Za-z]/g, "");
  const [first, second] = groupCode(code);
  const digits = (group: string): HTMLElement[] => Array.from(group).map((c) => h("span", null, c));
  return [
    h(
      "main",
      { class: "cbody center", id: "main", tabindex: -1, "data-key": "main-compare" },
      h("h3", null, `Does ${name} read you this code?`),
      provenance(s),
      h("p", null, "It proves you are connected to the right person."),
      h(
        "p",
        { class: "sac", role: "img", "aria-label": `Approval code ${first} ${second}` },
        digits(first),
        h("i", { "aria-hidden": "true" }),
        digits(second),
      ),
      h(
        "p",
        { class: "muted row waiting" },
        h("span", { class: "spinner", "aria-hidden": "true" }),
        `Waiting for ${name} to confirm…`,
      ),
      viaRelay(sessionRelay(s)),
    ),
    stopFooter("It doesn't match — stop", s),
  ];
}

const RISKS: ReadonlyArray<[IconName, string]> = [
  ["term", "run commands, install or remove software and drivers"],
  ["folder", "change settings, and read, change or delete files"],
  ["eye", "see and use your screen, and restart this computer"],
  ["bot", "send output, files and screenshots to their AI service"],
];

export function consentView(s: CustomerSnapshot): HTMLElement[] {
  const name = technicianName(s);
  const busy = s.busy === "allow" || s.busy === "decline";
  return [
    h(
      "main",
      { class: "cbody", id: "main", tabindex: -1, "data-key": "main-consent" },
      h("h3", { id: "consent-title" }, `Allow ${name} full control?`),
      provenance(s),
      h(
        "div",
        { class: "risk", "aria-labelledby": "risk-head" },
        h("b", { id: "risk-head" }, `${capName(name)} and their AI assistant could:`),
        h(
          "ul",
          null,
          RISKS.map(([iconName, text]) => h("li", null, icon(iconName), text)),
        ),
      ),
      h(
        "div",
        { class: "okline" },
        h("span", { class: "i" }, icon("check")),
        "You see everything they do, and you can stop at any time.",
      ),
      viaRelay(sessionRelay(s)),
      h("h4", { id: "disclosure-head" }, "What you agree to"),
      // The canonical disclosure, verbatim from the backend; it stays visible.
      h(
        "p",
        { class: "disclose", id: "disclosure", "aria-labelledby": "disclosure-head" },
        s.view?.disclosure ?? "",
      ),
    ),
    h(
      "div",
      { class: "cfoot decide", role: "group", "aria-labelledby": "consent-title" },
      // Decline is as prominent as Allow, and neither is focused by default.
      button("Decline", "decline", { variant: "big grow", disabled: busy }),
      button("Allow full control", "allow", { variant: "danger big grow", disabled: busy }),
    ),
  ];
}

export function stoppingView(): HTMLElement[] {
  return [
    h(
      "main",
      { class: "cbody center", id: "main", tabindex: -1, "data-key": "main-stopping" },
      h("span", { class: "spinner lg", "aria-hidden": "true" }),
      h("h3", null, "Ending the session"),
      h("p", null, "Control has ended. CretLink is cleaning up on this computer."),
    ),
  ];
}

