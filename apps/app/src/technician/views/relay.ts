// Settings → Relay (ST-M007-011): which relay this copy uses, and where that
// comes from.
//
// - The installed copy keeps a relay in its settings: a form with the
//   address, the admission key, the certificate authority file and the name
//   in the certificate, with Save, Test and Remove.
// - A file `cretlink.toml` next to the program wins over the settings, and
//   the screen says so: it then shows that relay, and what the settings hold
//   as the one that is not used.
// - A copy that runs once keeps no settings: the screen says where its relay
//   goes.
//
// The admission key is never shown: the screen says only whether one is set,
// and the key field is always empty. Every text of a profile is written as
// text, never as markup.

import type { RelayPlace, RelaySettings, RelayView } from "../../shared/contract";
import { type Child, h } from "../../shared/dom";
import { icon } from "../../shared/icons";
import type { RelayDraft, TechnicianSnapshot } from "../model";
import { button, card, kv, pill, unavailable } from "./common";

const INTRO =
  "A relay lets you help someone who is not on your network. You host it yourself. It only passes encrypted traffic on: it cannot look into a session and it grants nothing. The approval code and the customer's consent decide, as on one network.";

/** What a relay is checked against, in words. */
function trust(relay: RelayView): string {
  return relay.ca
    ? `The certificate file ${relay.ca}`
    : "The certificate authorities of this computer";
}

/** A relay as a list: its host name first, the key only as set or not. */
function relayFacts(relay: RelayView): HTMLElement {
  return kv([
    ["Relay", h("span", { class: "mono" }, relay.host)],
    ["Address", h("span", { class: "mono" }, relay.address)],
    ["Admission key", relay.keySet ? "Set (never shown)" : "None"],
    ["Certificate checked against", trust(relay)],
    relay.name ? ["Name in the certificate", h("span", { class: "mono" }, relay.name)] : null,
  ]);
}

/** A profile that cannot be used: what is wrong, the file, and a next step. */
function problemNote(place: RelayPlace, next: string): HTMLElement | null {
  const problem = place.problem;
  if (!problem) return null;
  return h(
    "div",
    { class: "note bad relay-problem", role: "note" },
    icon("alert"),
    h(
      "p",
      null,
      h("b", null, "These relay settings cannot be used. "),
      problem.message,
      " The file is ",
      h("span", { class: "mono" }, problem.file),
      ". ",
      next,
    ),
  );
}

/**
 * The result of a test, in a region that is always there and announced
 * politely (a `status`), so that it is read when it arrives.
 */
function testResult(draft: RelayDraft): HTMLElement {
  const region = h("div", { class: "relay-test", id: "relay-test", role: "status" });
  const test = draft.test;
  if (!test) return region;
  if (test.state === "running") {
    region.append(
      h(
        "p",
        { class: "muted row" },
        h("span", { class: "spinner", "aria-hidden": "true" }),
        "Reaching the relay…",
      ),
    );
    return region;
  }
  const result = test.result;
  region.append(
    h(
      "div",
      { class: result.ok ? "relay-result ok" : "relay-result bad" },
      result.ok
        ? pill("ok", "The relay admits this copy", "check")
        : pill("bad", "The relay cannot be used", "x"),
      h("p", null, result.message),
      result.next ? h("p", { class: "next" }, result.next) : null,
    ),
  );
  return region;
}

function testButton(s: TechnicianSnapshot): HTMLButtonElement {
  const test = button("Test", "relay-test", {
    icon: "link",
    disabled: s.busy !== undefined || s.relayDraft.removing === true,
    label: "Test the relay",
  });
  // While its test runs the button is marked as unavailable and not disabled:
  // a disabled button would lose the keyboard focus, and the technician who
  // pressed it would have to find their place again when the answer is in.
  if (s.relayDraft.test?.state === "running") test.setAttribute("aria-disabled", "true");
  return test;
}

/** One labelled field of the form, with its hint. */
function field(
  id: string,
  label: string,
  hint: string,
  attrs: Record<string, string | number | boolean | undefined>,
): HTMLElement {
  return h(
    "div",
    { class: "field" },
    h("label", { for: id }, label),
    h("input", {
      id,
      class: "field-input",
      type: "text",
      autocomplete: "off",
      autocapitalize: "off",
      spellcheck: "false",
      "aria-describedby": `${id}-hint`,
      ...attrs,
    }),
    h("p", { class: "muted", id: `${id}-hint` }, hint),
  );
}

/** Which certificate authority file the relay is checked against. */
function caField(draft: RelayDraft, stored: RelayView | undefined, busy: boolean): HTMLElement {
  const kept = draft.ca.mode === "keep" && stored?.ca !== undefined;
  const chosen = draft.ca.mode === "chosen" ? draft.ca.name : undefined;
  const state: Child = chosen
    ? [h("span", { class: "mono" }, chosen), " · chosen, not saved yet"]
    : kept
      ? [h("span", { class: "mono" }, stored?.ca ?? ""), " · kept with your settings"]
      : "The certificate authorities of this computer";
  return h(
    "div",
    { class: "field", role: "group", "aria-labelledby": "relay-ca-label" },
    h("span", { class: "field-label", id: "relay-ca-label" }, "Certificate authority"),
    h("div", { class: "relay-ca", id: "relay-ca" }, state),
    h(
      "div",
      { class: "row wrap" },
      button(chosen || kept ? "Choose another file…" : "Choose a file…", "relay-ca-choose", {
        id: "relay-ca-choose",
        variant: "sm",
        icon: "folder",
        disabled: busy,
      }),
      chosen || kept
        ? button("Use this computer's authorities", "relay-ca-none", {
            variant: "sm ghost",
            disabled: busy,
          })
        : null,
    ),
    h(
      "p",
      { class: "muted" },
      "Only for a relay whose certificate comes from an authority of your own: the PEM file of that authority. A copy of it is kept with your settings.",
    ),
  );
}

/** The form of the installed copy. */
function form(s: TechnicianSnapshot, settings: RelaySettings): HTMLElement {
  const stored = settings.stored?.relay;
  const draft = s.relayDraft;
  // While the question about removing is open, the form waits for its answer.
  const busy = s.busy !== undefined || draft.removing === true;
  return h(
    "div",
    {
      class: "relay-form",
      role: "group",
      "aria-labelledby": "relay-head",
      "data-key": `relay-form-${draft.round}`,
    },
    field(
      "relay-address",
      "Address",
      "The relay's host name and port, like relay.example.org:443. CretLink always uses TLS.",
      { value: stored?.address ?? "", placeholder: "relay.example.org:443", maxlength: 300 },
    ),
    h(
      "div",
      { class: "field" },
      h("label", { for: "relay-key" }, "Admission key"),
      h("input", {
        id: "relay-key",
        class: "field-input",
        type: "password",
        autocomplete: "off",
        spellcheck: "false",
        maxlength: 512,
        "aria-describedby": "relay-key-hint",
      }),
      h(
        "p",
        { class: "muted", id: "relay-key-hint" },
        stored?.keySet
          ? "A key is set. It is never shown. Leave this empty to keep it."
          : "The key the relay's operator gave this copy. Leave it empty when the relay asks for none.",
      ),
      stored?.keySet
        ? h(
            "label",
            { class: "check", for: "relay-key-remove" },
            h("input", { id: "relay-key-remove", type: "checkbox" }),
            "Remove the key that is set",
          )
        : null,
      h(
        "p",
        { class: "muted" },
        "A key only admits this copy to the relay. It gives nobody control over a session.",
      ),
    ),
    caField(draft, stored, busy),
    field(
      "relay-name",
      "Name in the certificate",
      "Only when the relay's certificate carries another name than the host of the address.",
      { value: stored?.name ?? "", placeholder: "relay.example.org", maxlength: 253 },
    ),
    h(
      "div",
      { class: "row wrap" },
      button("Save", "relay-save", { variant: "primary", disabled: busy }),
      testButton(s),
      settings.stored?.relay || settings.stored?.problem
        ? button("Remove…", "relay-remove", { variant: "ghost", disabled: busy })
        : null,
    ),
  );
}

/** "Remove the relay?", before the relay and its key are removed. */
function removeQuestion(s: TechnicianSnapshot, stored: RelayPlace | undefined): HTMLElement {
  return h(
    "div",
    { class: "confirm", id: "relay-remove-step", tabindex: -1 },
    h(
      "p",
      { class: "confirm-head" },
      stored?.relay
        ? `Remove the relay ${stored.relay.host} from the settings?`
        : "Remove the relay settings?",
    ),
    h(
      "p",
      null,
      "Its admission key and its certificate file are removed with it and cannot be shown again. The network to wait on stays.",
    ),
    h(
      "div",
      { class: "row" },
      button("Remove", "relay-remove-confirm", {
        variant: "danger",
        disabled: s.busy === "relay-remove",
      }),
      button("Cancel", "relay-remove-cancel", { variant: "ghost", autofocus: true }),
    ),
  );
}

/** The relay of the file next to the program, which wins over the settings. */
function fileRelay(s: TechnicianSnapshot, settings: RelaySettings): Child[] {
  const file = settings.file;
  const stored = settings.stored;
  return [
    h(
      "div",
      { class: "note relay-file", role: "note" },
      icon("alert"),
      h(
        "p",
        null,
        "This copy takes its relay from the file ",
        h("span", { class: "mono" }, file.path),
        " next to the program.",
        settings.editable ? " That file wins over what Settings holds." : null,
      ),
    ),
    file.relay ? relayFacts(file.relay) : null,
    problemNote(file, "Correct the file, or remove it."),
    file.relay ? h("div", { class: "row wrap" }, testButton(s)) : null,
    stored?.relay || stored?.problem
      ? h(
          "div",
          { class: "relay-unused" },
          h(
            "p",
            null,
            stored.relay
              ? [
                  "Settings also holds the relay ",
                  h("b", { class: "mono" }, stored.relay.host),
                  ". It is not used while the file is there.",
                ]
              : "Settings also holds relay settings that cannot be read. They are not used while the file is there.",
          ),
          s.relayDraft.removing
            ? removeQuestion(s, stored)
            : button("Remove from Settings…", "relay-remove", {
                variant: "ghost sm",
                disabled: s.busy !== undefined,
              }),
        )
      : null,
  ];
}

/** A copy that runs once and has no relay: where one goes. */
function runOnce(settings: RelaySettings): Child[] {
  return [
    h(
      "p",
      null,
      "This copy runs once and keeps no settings. To use a relay with it, put a file named ",
      h("span", { class: "mono" }, "cretlink.toml"),
      " next to the program:",
    ),
    h("p", { class: "mono relay-where" }, settings.file.path),
    h(
      "pre",
      { class: "relay-example", "aria-label": "An example of cretlink.toml" },
      '[relay]\naddress = "relay.example.org:443"\nkey = "the key of this copy"',
    ),
    h("p", { class: "muted" }, "The installed copy keeps its relay here in Settings."),
  ];
}

/** The Relay card of the Settings screen. */
export function relayCard(s: TechnicianSnapshot): HTMLElement {
  const settings = s.relay;
  const head = h("h4", { id: "relay-head" }, "Relay");
  if (!settings) return card(head, unavailable("The relay of this copy"));
  const intro = h("p", null, INTRO);
  if (settings.source === "file") {
    return card(head, intro, fileRelay(s, settings), testResult(s.relayDraft));
  }
  if (!settings.editable) return card(head, intro, runOnce(settings));
  return card(
    head,
    intro,
    problemNote(
      settings.stored ?? { path: "" },
      "Saving a relay below replaces them; Remove clears them.",
    ),
    form(s, settings),
    s.relayDraft.removing ? removeQuestion(s, settings.stored) : null,
    testResult(s.relayDraft),
  );
}
