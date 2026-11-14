// Make a customer copy (ST-M007-016), a step of Settings → Relay.
//
// A technician hands a customer one folder: the support program and the file
// that tells it which relay to use, with a key of that customer's own. The
// step asks for a label, the customer's key, the program (elsewhere than on
// Windows) and the place, and then writes the folder.
//
// - The technician's own key is no part of it: the key is typed for this
//   customer, generated, or left out for a relay that asks for none.
// - A generated key is shown once, as the line `label:key` for the relay's
//   key file, with Copy.
// - The step says what a key is and is not: it stands in the file in
//   readable form, admits a copy to the relay, gives no control over a
//   session, and can be withdrawn by the relay's operator.
// - Nothing leaves this computer: the folder is written and can be shown.
//
// Every text of a path, a label or a key is written as text, never as
// markup.

import type { CustomerCopyMade, CustomerCopyOptions } from "../../shared/contract";
import { type Child, h } from "../../shared/dom";
import { icon } from "../../shared/icons";
import { relayLabel, trustWords } from "../../shared/relay";
import {
  type CustomerCopyDraft,
  type TechnicianSnapshot,
  activeRelay,
  keyFileLine,
} from "../model";
import { button, kv, pill } from "./common";

const WHAT_IT_IS =
  "A folder for one customer: the support program, and the file that tells it which relay to use. You hand over the folder; the customer needs nothing else. CretLink writes it on this computer and sends nothing anywhere.";

/** What a key is and what it is not, said where the key is asked for. */
export const KEY_HONESTY =
  "The key stands in the file cretlink.toml of the copy in readable form. It only admits that copy to the relay and gives nobody control over a session. As the relay's operator you withdraw it by deleting its line from the relay's key file. Your own key is never written into a customer copy.";

/** The offer under the relay of this copy: **Make a customer copy…**. */
function offer(s: TechnicianSnapshot): HTMLElement {
  return h(
    "div",
    { class: "copy-offer", id: "copy-offer" },
    h(
      "p",
      { class: "muted" },
      "For a customer who is not on your network: a folder with the support program and this relay, ready to hand over.",
    ),
    button("Make a customer copy…", "copy-open", {
      id: "copy-open",
      icon: "folder",
      disabled: s.busy !== undefined || s.relayDraft.removing === true,
    }),
  );
}

/** The relay a copy will name, as the backend said it. */
function relayOfCopy(options: CustomerCopyOptions): HTMLElement {
  const trust = trustWords(options.relay);
  return kv([
    ["Relay", relayLabel(options.relay)],
    ["Address", h("span", { class: "mono" }, options.relay.address)],
    options.relay.ca
      ? ["Certificate file", h("span", { class: "mono" }, options.relay.ca)]
      : null,
    trust ? ["Certificate", trust] : null,
  ]);
}

/** Who the copy is for. The label is fixed while a generated key carries it. */
function labelField(options: CustomerCopyOptions, draft: CustomerCopyDraft): HTMLElement {
  const fixed = draft.generated !== undefined;
  return h(
    "div",
    { class: "field" },
    h("label", { for: "copy-label" }, "Label"),
    h("input", {
      id: "copy-label",
      class: "field-input",
      type: "text",
      autocomplete: "off",
      autocapitalize: "off",
      spellcheck: "false",
      maxlength: options.labelMax,
      placeholder: "acme-front-desk",
      readonly: fixed,
      value: draft.generated?.label,
      "aria-describedby": "copy-label-hint",
      "data-key": "copy-label",
    }),
    h(
      "p",
      { class: "muted", id: "copy-label-hint" },
      `Who the copy is for: letters, digits and hyphens, at most ${options.labelMax}. The folder is named ${options.folderPrefix} and the label.`,
      fixed
        ? " The generated key belongs to this label; discard the key to change it."
        : null,
    ),
  );
}

/** Where the program of the copy comes from. */
function programField(
  options: CustomerCopyOptions,
  draft: CustomerCopyDraft,
  busy: boolean,
): HTMLElement {
  if (options.program === "this_program") {
    return h(
      "div",
      { class: "field", role: "group", "aria-labelledby": "copy-program-label" },
      h("span", { class: "field-label", id: "copy-program-label" }, "Program"),
      h(
        "p",
        { id: "copy-program" },
        "This program itself, written under the name ",
        h("span", { class: "mono" }, options.programName),
        ". It is copied unchanged.",
      ),
    );
  }
  return h(
    "div",
    { class: "field", role: "group", "aria-labelledby": "copy-program-label" },
    h("span", { class: "field-label", id: "copy-program-label" }, "Program"),
    h(
      "div",
      { class: "relay-ca", id: "copy-program" },
      draft.program
        ? [h("span", { class: "mono" }, draft.program.name), " · chosen"]
        : "None chosen yet",
    ),
    h(
      "div",
      { class: "row wrap" },
      button(draft.program ? "Choose another file…" : "Choose cretlink.exe…", "copy-program", {
        id: "copy-program-choose",
        variant: "sm",
        icon: "folder",
        disabled: busy,
      }),
    ),
    h(
      "p",
      { class: "muted" },
      "The support program runs on Windows. Point at ",
      h("span", { class: "mono" }, "cretlink.exe"),
      " from the Windows archive of a CretLink release. It is copied unchanged as ",
      h("span", { class: "mono" }, options.programName),
      ", so a signed program stays signed.",
    ),
  );
}

/** The line `label:key` of a generated key, with Copy. */
function keyLine(generated: { label: string; key: string }, again: boolean): HTMLElement {
  return h(
    "div",
    { class: "copy-key-line", id: "copy-key-generated" },
    h(
      "p",
      { id: "copy-key-line-h" },
      again
        ? "The line for your relay's key file, once more:"
        : "A new key for this customer. The line for your relay's key file:",
    ),
    h(
      "div",
      { class: "row wrap" },
      h(
        "code",
        { class: "mono key-line", id: "copy-key-line", "aria-labelledby": "copy-key-line-h" },
        keyFileLine(generated.label, generated.key),
      ),
      button("Copy", "copy-key-line", {
        variant: "sm",
        label: "Copy the line for the key file",
      }),
    ),
    h(
      "p",
      { class: "muted" },
      again
        ? "Add it to the key file before the customer connects. It is not shown again: CretLink keeps the key nowhere but in the copy."
        : "Add it to the relay's key file; the relay takes it up without a restart. The key is shown here only: CretLink keeps it nowhere but in the copy you make.",
    ),
  );
}

/** The admission key of this customer: typed, generated, or none. */
function keyField(draft: CustomerCopyDraft, busy: boolean): HTMLElement {
  let entry: Child;
  if (draft.generated) {
    entry = [
      keyLine(draft.generated, false),
      h(
        "div",
        { class: "row wrap" },
        button("Discard this key", "copy-discard-key", {
          id: "copy-discard-key",
          variant: "sm ghost",
          disabled: busy,
        }),
      ),
    ];
  } else {
    entry = [
      h(
        "div",
        { class: "row wrap copy-key-entry" },
        h("input", {
          id: "copy-key",
          class: "field-input",
          type: "password",
          autocomplete: "off",
          spellcheck: "false",
          maxlength: 512,
          "aria-labelledby": "copy-key-label",
          "aria-describedby": "copy-key-hint",
          "data-key": "copy-key",
        }),
        button("Generate", "copy-generate", {
          id: "copy-generate",
          variant: "sm",
          disabled: busy,
          label: "Generate a new key for this customer",
        }),
      ),
      h(
        "p",
        { class: "muted", id: "copy-key-hint" },
        "Type or paste the key that the relay's key file lists for this customer, or generate a new one.",
      ),
    ];
  }
  return h(
    "div",
    { class: "field", role: "group", "aria-labelledby": "copy-key-label" },
    h("span", { class: "field-label", id: "copy-key-label" }, "Admission key of this customer"),
    entry,
    draft.generated
      ? null
      : h(
          "label",
          { class: "check", for: "copy-no-key" },
          h("input", { id: "copy-no-key", type: "checkbox", "data-key": "copy-no-key" }),
          "The relay asks for no key: make the copy without one",
        ),
    h("p", { class: "muted", id: "copy-key-honest" }, KEY_HONESTY),
  );
}

/** Where the folder is made. */
function placeField(
  options: CustomerCopyOptions,
  draft: CustomerCopyDraft,
  busy: boolean,
): HTMLElement {
  return h(
    "div",
    { class: "field", role: "group", "aria-labelledby": "copy-place-label" },
    h("span", { class: "field-label", id: "copy-place-label" }, "Where to make it"),
    h(
      "div",
      { class: "relay-ca", id: "copy-place" },
      draft.place
        ? [
            "The folder ",
            h("span", { class: "mono" }, `${options.folderPrefix}<label>`),
            " is made in ",
            h("span", { class: "mono" }, draft.place.name),
          ]
        : "None chosen yet",
    ),
    h(
      "div",
      { class: "row wrap" },
      button(draft.place ? "Choose another folder…" : "Choose a folder…", "copy-place", {
        id: "copy-place-choose",
        variant: "sm",
        icon: "folder",
        disabled: busy,
      }),
    ),
  );
}

/** "A folder of that name is there already. Replace it?" */
function replaceQuestion(s: TechnicianSnapshot, message: string): HTMLElement {
  return h(
    "div",
    { class: "confirm", id: "copy-replace-step", tabindex: -1 },
    h("p", { class: "confirm-head" }, "Replace the folder that is there?"),
    h(
      "p",
      null,
      message,
      " Replacing it removes that folder with everything in it and puts the new copy in its place.",
    ),
    h(
      "div",
      { class: "row" },
      button("Replace", "copy-replace", {
        variant: "danger",
        disabled: s.busy === "copy-make",
      }),
      button("Keep it", "copy-replace-cancel", { variant: "ghost", autofocus: true }),
    ),
  );
}

/** The copy that was written. */
function result(draft: CustomerCopyDraft, made: CustomerCopyMade, busy: boolean): HTMLElement {
  return h(
    "div",
    {
      class: "copy-step copy-made",
      id: "copy-step",
      tabindex: -1,
      role: "group",
      "aria-labelledby": "copy-head",
    },
    h("h5", { id: "copy-head" }, "Make a customer copy"),
    h(
      "div",
      { class: "relay-result ok", id: "copy-result", role: "status" },
      pill("ok", "The customer copy was written", "check"),
      h("p", null, "The folder ", h("span", { class: "mono", id: "copy-folder" }, made.folder), " holds:"),
      h(
        "ul",
        { class: "plan", id: "copy-files" },
        made.files.map((file) => h("li", { class: "mono" }, file)),
      ),
      h(
        "p",
        null,
        "Hand the whole folder to the customer and keep its files together. Nothing was sent anywhere.",
      ),
    ),
    draft.generated ? keyLine(draft.generated, true) : null,
    made.setAside
      ? h(
          "div",
          { class: "note", role: "note", id: "copy-set-aside" },
          icon("alert"),
          h(
            "p",
            null,
            "The folder that was there before could not be removed. It lies in ",
            h("span", { class: "mono" }, made.setAside),
            "; remove it yourself.",
          ),
        )
      : null,
    h(
      "div",
      { class: "row wrap" },
      button("Show in folder", "copy-show", { id: "copy-show", icon: "folder", disabled: busy }),
      button("Done", "copy-done", { variant: "ghost" }),
    ),
  );
}

/** The step, while it is open. */
function step(s: TechnicianSnapshot, draft: CustomerCopyDraft): HTMLElement {
  const options = draft.options;
  if (!options) {
    return h(
      "div",
      { class: "copy-step", id: "copy-step", tabindex: -1 },
      h("h5", { id: "copy-head" }, "Make a customer copy"),
      h(
        "p",
        { "aria-busy": "true" },
        h("span", { class: "spinner", "aria-hidden": "true" }),
        " Looking at the relay of this copy…",
      ),
    );
  }
  const busy = s.busy !== undefined || draft.replacing !== undefined;
  if (draft.made) return result(draft, draft.made, s.busy !== undefined);
  return h(
    "div",
    {
      class: "copy-step relay-form",
      id: "copy-step",
      tabindex: -1,
      role: "group",
      "aria-labelledby": "copy-head",
    },
    h("h5", { id: "copy-head" }, "Make a customer copy"),
    h("p", null, WHAT_IT_IS),
    relayOfCopy(options),
    labelField(options, draft),
    programField(options, draft, busy),
    keyField(draft, busy),
    placeField(options, draft, busy),
    draft.replacing
      ? replaceQuestion(s, draft.replacing)
      : h(
          "div",
          { class: "row wrap" },
          button("Make the copy", "copy-make", {
            id: "copy-make",
            variant: "primary",
            disabled: busy,
          }),
          button("Cancel", "copy-cancel", { variant: "ghost", disabled: s.busy !== undefined }),
        ),
  );
}

/**
 * The part of the Relay card about a customer copy: nothing without a relay
 * that can be used, the offer with one, and the step while it is open.
 */
export function customerCopyPart(s: TechnicianSnapshot): Child {
  if (s.customerCopy) return step(s, s.customerCopy);
  return activeRelay(s.relay) ? offer(s) : null;
}
