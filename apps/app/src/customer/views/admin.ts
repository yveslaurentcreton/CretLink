// Before the UAC prompt (`#c-admin`, ST-M009-009, REQ-F-002): four steps,
// nothing happens before step 4 and nothing is installed. After a declined
// prompt it says honestly that administrator tasks will not work, and offers
// Try again and Continue without admin rights.

import { h } from "../../shared/dom";
import { icon } from "../../shared/icons";
import type { CustomerSnapshot } from "../model";
import { button } from "./common";

const STEPS = [
  "Windows asks: allow CretLink?",
  "You choose your technician",
  "You compare a code together",
  "You decide whether to allow control",
];

function steps(): HTMLElement {
  return h(
    "ol",
    { class: "steps card flat", "aria-label": "What happens next" },
    STEPS.map((step, index) =>
      h(
        "li",
        { class: "okline" },
        h("span", { class: "i n", "aria-hidden": "true" }, String(index + 1)),
        h("span", null, h("span", { class: "sr-only" }, `Step ${index + 1}: `), step),
      ),
    ),
  );
}

function devNote(s: CustomerSnapshot): HTMLElement | null {
  if (s.launch?.uac !== false) return null;
  return h(
    "p",
    { class: "note dev", role: "note" },
    icon("alert"),
    h(
      "span",
      null,
      h("b", null, "Development build. "),
      "This system has no Windows permission prompt. Continue opens a new window without administrator rights.",
    ),
  );
}

function backLabel(s: CustomerSnapshot): string {
  return s.launch?.back === "start" ? "Back" : "Close";
}

export function adminView(s: CustomerSnapshot): HTMLElement[] {
  return [
    h(
      "main",
      { class: "cbody", id: "main", tabindex: -1, "data-key": "main-admin" },
      h("h3", { id: "admin-title" }, "Windows will ask for permission"),
      h(
        "p",
        null,
        "Your technician usually needs administrator rights to fix things. In the next window, Windows asks whether CretLink may make changes.",
      ),
      steps(),
      h("p", { class: "muted" }, "Nothing happens to this computer before step 4, and nothing is installed."),
      devNote(s),
    ),
    h(
      "div",
      { class: "cfoot" },
      button(backLabel(s), "back", {
        variant: "ghost",
        disabled: s.busy !== undefined,
        label: s.launch?.back === "start" ? "Back to the start screen" : "Close CretLink",
      }),
      h("span", { class: "spacer" }),
      button("Continue", "continue", {
        variant: "primary big",
        autofocus: true,
        disabled: s.busy !== undefined,
      }),
    ),
  ];
}

export function declinedView(s: CustomerSnapshot): HTMLElement[] {
  return [
    h(
      "main",
      { class: "cbody", id: "main", tabindex: -1, "data-key": "main-declined" },
      h("h3", null, "Windows did not get permission"),
      h(
        "div",
        { class: "note" },
        icon("alert"),
        h(
          "p",
          null,
          h("b", null, "Administrator tasks will not work. "),
          "Without administrator rights, your technician cannot install drivers or software for all users, change system settings or repair Windows. Those tasks will fail.",
        ),
      ),
      h(
        "p",
        null,
        "Try again and answer ",
        h("b", null, "Yes"),
        " when Windows asks. Or continue without administrator rights: you can still get help with everything your own account can do.",
      ),
      h("p", { class: "muted" }, "Nothing has happened to this computer, and nothing is installed."),
      devNote(s),
    ),
    h(
      "div",
      { class: "cfoot" },
      button(backLabel(s), "back", { variant: "ghost", disabled: s.busy !== undefined }),
      h("span", { class: "spacer" }),
      button("Continue without admin rights", "without-admin", {
        disabled: s.busy !== undefined,
      }),
      button("Try again", "continue", {
        variant: "primary",
        autofocus: true,
        disabled: s.busy !== undefined,
      }),
    ),
  ];
}
