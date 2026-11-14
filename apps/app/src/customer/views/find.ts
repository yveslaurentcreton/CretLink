// Choose your technician (`#c-find`, ST-M009-009, REQ-F-004, ADR-0007).
//
// Nearby consoles are an untrusted list: nothing is preselected and nothing
// connects on its own. Each entry shows the name and host name it reports,
// its address, and "this network" or "unreachable". A field for the
// technician's code is always offered. This computer's host name, signed-in
// account and admin rights are shown truthfully (REQ-S-011).

import { h } from "../../shared/dom";
import { icon } from "../../shared/icons";
import { relayLabel, trustWords } from "../../shared/relay";
import type { DiscoveredConsoleView } from "../api";
import {
  type CustomerSnapshot,
  adminRightsWords,
  consoleName,
  signedInAs,
} from "../model";
import { button, nothingInstalled } from "./common";

/** The longest code or address the field takes (bounded input). */
export const CODE_MAX = 64;

function pick(console: DiscoveredConsoleView, index: number, s: CustomerSnapshot): HTMLElement {
  const name = consoleName(console);
  const initial = (console.display_name?.trim() || console.instance_name.trim() || "?")
    .charAt(0)
    .toUpperCase();
  const reach = console.dialable ? "this network" : "unreachable";
  return h(
    "li",
    {
      class: console.dialable ? "pick" : "pick off",
      "data-key": `pick-${index}-${console.instance_name}-${console.address}`,
    },
    h("span", { class: "av", "aria-hidden": "true" }, console.dialable ? initial : "?"),
    h(
      "div",
      { class: "pick-text" },
      h("b", null, name),
      h("span", { class: "muted" }, [console.address || "no address", reach].join(" · ")),
    ),
    button("Connect", "connect", {
      arg: console.address,
      variant: console.dialable ? "primary" : undefined,
      disabled: !console.dialable || s.busy !== undefined,
      label: console.dialable ? `Connect to ${name}` : `${name} is unreachable`,
    }),
  );
}

function consoles(s: CustomerSnapshot): HTMLElement {
  const { discovery } = s;
  const list = h("ul", {
    class: "picks",
    "aria-label": "Technicians nearby, as their computers name themselves",
  });
  discovery.consoles.forEach((console, index) => list.append(pick(console, index, s)));
  const searching = discovery.state === "searching";
  return h(
    "section",
    { class: "nearby", "aria-labelledby": "nearby-h", "aria-busy": searching ? "true" : null },
    h(
      "div",
      { class: "row" },
      h("h4", { id: "nearby-h" }, "Nearby"),
      h("span", { class: "spacer" }),
      searching
        ? h("span", { class: "muted row" }, h("span", { class: "spinner xs", "aria-hidden": "true" }), "Searching…")
        : button("Search again", "search", { variant: "ghost sm", icon: "restart" }),
    ),
    discovery.consoles.length > 0
      ? list
      : searching
        ? null
        : h(
            "p",
            { class: "muted" },
            discovery.problem ??
              "No technician was found on this network. Ask them for their code and enter it below.",
          ),
  );
}

function codeField(s: CustomerSnapshot): HTMLElement {
  // A copy that is set up for a relay says through which one a relay code
  // connects, before any code is typed (ST-M007-011, ST-M007-013): its name,
  // where this copy would connect when that is another host, and what
  // vouches for the name when it is a file of this copy's profile.
  const relay = s.view?.relay_of_copy ?? undefined;
  return h(
    "div",
    { class: "code-field" },
    h("label", { class: "label", for: "code" }, "Or enter the code your technician gives you"),
    h(
      "div",
      { class: "row" },
      h("input", {
        id: "code",
        class: "input",
        type: "text",
        autocomplete: "off",
        autocapitalize: "characters",
        spellcheck: "false",
        maxlength: CODE_MAX,
        placeholder: "6RRR-2H8F",
        "data-key": "code",
        "aria-describedby": relay ? "code-hint relay-ready" : "code-hint",
      }),
      button("Connect", "connect-code", { disabled: s.busy !== undefined }),
    ),
    h(
      "p",
      { class: "muted", id: "code-hint" },
      "The code only says where to connect. It grants nothing.",
    ),
    relay
      ? h(
          "p",
          { class: "muted relay-ready", id: "relay-ready", "data-key": "relay-ready" },
          "If your technician is not on this network, they give you a longer relay code. This copy then connects through the relay ",
          relayLabel(relay),
          ", which only passes encrypted traffic on.",
          trustWords(relay) ? [" ", trustWords(relay)] : null,
        )
      : null,
  );
}

function thisComputer(s: CustomerSnapshot): HTMLElement | null {
  const view = s.view;
  if (!view) return null;
  return h(
    "div",
    { class: "card flat computer" },
    h(
      "dl",
      { class: "kv" },
      h("dt", null, "This computer"),
      h("dd", null, view.hostname),
      h("dt", null, "Signed in as"),
      h("dd", null, signedInAs(view)),
      h("dt", null, "Admin rights"),
      h("dd", null, adminRightsWords(view)),
    ),
  );
}

// Files from an earlier session that could not be removed at start
// (ST-M009-010): said plainly, with the command that removes them. Never
// silent, and nothing to do with the session about to start.
export function leftovers(s: CustomerSnapshot): HTMLElement | null {
  const cleanup = s.view?.startup_cleanup;
  if (!cleanup || cleanup.failed <= 0) return null;
  const files = cleanup.failed === 1 ? "1 file" : `${cleanup.failed} files`;
  return h(
    "div",
    { class: "note leftovers", role: "note", "data-key": "leftovers" },
    icon("alert"),
    h(
      "p",
      null,
      h("b", null, `${files} from an earlier CretLink session could not be removed. `),
      `They stay on this computer until they are removed. To remove them, open Windows PowerShell${
        cleanup.administrator ? " as administrator" : ""
      } and run: `,
      h("code", null, cleanup.command),
    ),
  );
}

function demo(s: CustomerSnapshot): HTMLElement | null {
  if (!s.launch?.demo) return null;
  return h(
    "p",
    { class: "note dev", role: "note" },
    icon("alert"),
    h("span", null, h("b", null, "Development build. "), "Try the screens without a technician: "),
    button("Simulated demo", "demo", { variant: "sm", disabled: s.busy !== undefined }),
  );
}

export function findView(s: CustomerSnapshot): HTMLElement[] {
  return [
    h(
      "main",
      { class: "cbody", id: "main", tabindex: -1, "data-key": "main-find" },
      h(
        "div",
        null,
        h("h3", null, "Who is helping you?"),
        h("p", { class: "lead" }, "Choose only the person you are talking to right now."),
      ),
      leftovers(s),
      s.ended ? h("p", { class: "note info", role: "note" }, icon("check"), s.ended) : null,
      consoles(s),
      codeField(s),
      demo(s),
      h("div", { class: "spacer" }),
      thisComputer(s),
    ),
    nothingInstalled(),
  ];
}
