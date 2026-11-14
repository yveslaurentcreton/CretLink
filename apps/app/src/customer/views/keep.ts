// Keeping the support program for next time (ST-M009-017, REQ-F-024).
//
// A customer who gets help more than once can keep the support program on
// the computer. The list of technicians and the end report carry one card
// about it (`keepCard`), never a screen of a session: it offers **Keep
// CretLink Support on this computer**, or to update what is kept, or, in the
// kept program itself, **Remove from this computer**. Each leads to a screen
// that says in plain words what will change, before anything changes:
//
// - `keepView`: what is added and where, that nothing starts by itself and
//   that nobody can connect unless the customer starts the program and
//   allows a session, and how to remove it. A copy that brings relay
//   settings names the relay first: the settings may have been written by
//   somebody else.
// - `removeView`: what is removed, and that a saved report stays.
// - `removedView`: that it is gone.
//
// Neither choice is focused when its screen opens. Every path and name is
// written as text.

import { type Child, h } from "../../shared/dom";
import { icon } from "../../shared/icons";
import { relayLabel, trustWords } from "../../shared/relay";
import { type CustomerSnapshot, type KeepCard, isKept, isKeptProgram, keepCard } from "../model";
import { button } from "./common";

function version(text: string | undefined): string {
  return text ? `version ${text}` : "a version that cannot be read";
}

function cardText(card: KeepCard, keptNow: boolean): { title: string; words: string } {
  switch (card.kind) {
    case "offer":
      return {
        title: "Getting help more often?",
        words:
          "Keep the support program on this computer and start it from the Start menu next time. Nothing runs until you start it.",
      };
    case "update":
      return {
        title: `A newer version than the one kept on this computer`,
        words: `CretLink Support is kept on this computer in ${version(card.kept)}. This copy is version ${card.version} and can replace it.`,
      };
    case "installed":
      return {
        title: "CretLink Support is installed on this computer",
        words: `Version ${card.version}. It starts only when you start it, and every session asks for your permission again.`,
      };
    case "kept":
      if (keptNow) {
        return {
          title: "CretLink Support is now kept on this computer",
          words: `Version ${card.version}. Next time, start CretLink Support from the Start menu. You no longer need this copy.`,
        };
      }
      switch (card.relation) {
        case "same":
          return {
            title: "CretLink Support is kept on this computer",
            words: `In this version, ${card.version}. Next time, you can start it from the Start menu.`,
          };
        case "older":
          return {
            title: "A newer CretLink Support is kept on this computer",
            words: `It is ${version(card.kept)}; this copy is the older version ${card.version} and does not replace it. Next time, start CretLink Support from the Start menu.`,
          };
        case "newer":
          return {
            title: "CretLink Support is kept on this computer",
            words: `It is ${version(card.kept)}; this copy is version ${card.version}. Replacing it needs administrator rights, which this window does not have.`,
          };
        case "unknown":
          return {
            title: "CretLink Support is kept on this computer",
            words: "Its version cannot be compared with this copy, so this copy does not replace it.",
          };
      }
  }
}

/**
 * The card about keeping on the list of technicians and on the end report;
 * nothing while a session exists, and nothing where this window has nothing
 * to say about it.
 */
export function keepCardView(s: CustomerSnapshot): HTMLElement | null {
  const card = keepCard(s);
  if (!card) return null;
  const { title, words } = cardText(card, s.keptNow);
  const busy = s.busy !== undefined;
  let action: HTMLElement | null = null;
  if (card.kind === "offer") {
    action = button("Keep CretLink Support on this computer", "keep-open", {
      variant: "sm",
      disabled: busy,
    });
  } else if (card.kind === "update") {
    action = button("Update the kept program", "keep-open", { variant: "sm", disabled: busy });
  } else if (card.kind === "installed" && card.canRemove) {
    action = button("Remove from this computer", "remove-open", { variant: "sm", disabled: busy });
  }
  return h(
    "section",
    {
      class: "card flat keep-card",
      "aria-labelledby": "keep-card-h",
      "data-key": "keep-card",
      "data-keep-card": card.kind,
    },
    h(
      "div",
      { class: "row" },
      icon(card.kind === "offer" || card.kind === "update" ? "home" : "check"),
      h(
        "div",
        { class: "record-text" },
        h("b", { id: "keep-card-h" }, title),
        h("div", { class: "muted" }, words),
      ),
    ),
    action ? h("div", { class: "row end" }, action) : null,
  );
}

/** The footer of the screens before a session: what is on this computer. */
export function installedWords(s: CustomerSnapshot): string {
  if (isKeptProgram(s)) return "This program is kept on this computer · closing it ends everything";
  if (isKept(s)) return "CretLink Support is kept on this computer · closing CretLink ends everything";
  return "Nothing is installed · closing CretLink ends everything";
}

function added(...children: Child[]): HTMLElement {
  return h("li", { class: "okline" }, h("span", { class: "i", "aria-hidden": "true" }, icon("check")), h("div", null, children));
}

/** **Keep CretLink Support on this computer**: what it adds, before it is added. */
export function keepView(s: CustomerSnapshot): HTMLElement[] {
  if (s.panel.kind !== "keep") return [];
  const plan = s.panel.plan;
  const update = plan.replaces !== undefined;
  const relay = plan.relay;
  const trust = relay ? trustWords(relay) : undefined;
  const busy = s.busy !== undefined;
  return [
    h(
      "main",
      { class: "cbody", id: "main", tabindex: -1, "data-key": "main-keep" },
      h(
        "div",
        null,
        h(
          "h3",
          { id: "keep-title" },
          update ? "Update CretLink Support on this computer?" : "Keep CretLink Support on this computer?",
        ),
        h(
          "p",
          { class: "lead" },
          update
            ? `Version ${plan.replaces} is kept on this computer. This copy is version ${plan.version} and replaces it.`
            : "You can then start it from the Start menu the next time you want help.",
        ),
      ),
      h(
        "section",
        { class: "card flat keep-adds", "aria-labelledby": "keep-adds-h" },
        h("h4", { id: "keep-adds-h" }, update ? "What changes on this computer" : "What is added to this computer"),
        h(
          "ul",
          { class: "adds" },
          added(
            `The program, version ${plan.version}, in `,
            h("code", { class: "path" }, plan.location),
            ", where only an administrator can change it.",
          ),
          added("An entry ", h("b", null, "CretLink Support"), " in the Start menu, for everyone who uses this computer."),
          added("An entry in ", h("b", null, "Installed apps"), " of Windows, where you can remove it again."),
          relay
            ? h(
                "li",
                { class: "okline", "data-key": "keep-relay" },
                h("span", { class: "i", "aria-hidden": "true" }, icon("check")),
                h(
                  "div",
                  null,
                  "The relay settings of this copy. When your technician is not on your network, the kept program connects through the relay ",
                  relayLabel(relay),
                  ", which only passes encrypted traffic on.",
                  trust ? [" ", trust] : null,
                  plan.relayReplaces ? " They replace the relay settings that are kept now." : null,
                ),
              )
            : null,
        ),
        plan.relayProblem
          ? h(
              "p",
              { class: "note", role: "note", "data-key": "keep-relay-problem" },
              icon("alert"),
              h(
                "span",
                null,
                h("b", null, "The relay settings next to this copy are not kept. "),
                `They cannot be used: ${plan.relayProblem}`,
              ),
            )
          : null,
        h(
          "label",
          { class: "check", for: "keep-desktop" },
          h("input", {
            type: "checkbox",
            id: "keep-desktop",
            "data-key": "keep-desktop",
            checked: plan.desktopKept ? true : null,
          }),
          h("span", null, "Also put a shortcut on the desktop"),
        ),
      ),
      h(
        "div",
        { class: "note info", role: "note" },
        icon("shield"),
        h(
          "p",
          null,
          h("b", null, "Nothing starts by itself. "),
          "Nobody can connect to this computer unless you start the program and allow a session. Every time, Windows asks for permission, you choose your technician, you compare the code and you decide, exactly as today.",
        ),
      ),
      h(
        "p",
        { class: "muted" },
        "To remove it again, open Settings in Windows, then Apps, then Installed apps, and uninstall CretLink Support. Or start it and choose Remove from this computer.",
      ),
    ),
    h(
      "div",
      { class: "cfoot" },
      button("Back", "panel-back", { variant: "ghost", disabled: busy, keys: "Escape" }),
      h("span", { class: "spacer" }),
      button(update ? "Update on this computer" : "Keep on this computer", "keep-confirm", {
        variant: "primary big",
        disabled: busy,
      }),
    ),
  ];
}

function lines(title: string, id: string, items: readonly string[]): HTMLElement | null {
  if (items.length === 0) return null;
  return h(
    "section",
    { class: "card flat", "aria-labelledby": id },
    h("h4", { id }, title),
    h("ul", { class: "plain-list" }, items.map((line) => h("li", null, line))),
  );
}

/** **Remove from this computer**: what goes and what stays, before it goes. */
export function removeView(s: CustomerSnapshot): HTMLElement[] {
  if (s.panel.kind !== "remove") return [];
  const plan = s.panel.plan;
  const busy = s.busy !== undefined;
  return [
    h(
      "main",
      { class: "cbody", id: "main", tabindex: -1, "data-key": "main-remove" },
      h(
        "div",
        null,
        h("h3", { id: "remove-title" }, "Remove CretLink Support from this computer?"),
        h(
          "p",
          { class: "lead" },
          "Everything that keeping added goes. You can still get help with a copy from your technician.",
        ),
      ),
      lines("What is removed", "remove-goes-h", plan.removes),
      lines("What stays", "remove-stays-h", plan.stays),
      plan.elevates
        ? h(
            "p",
            { class: "note", role: "note", "data-key": "remove-elevates" },
            icon("alert"),
            h(
              "span",
              null,
              h("b", null, "Windows will ask for administrator rights. "),
              "CretLink then shows this list once more and asks before it removes anything, and this window closes.",
            ),
          )
        : plan.afterExit
          ? h(
              "p",
              { class: "muted", "data-key": "remove-after-exit" },
              "The program you are looking at cannot remove its own file while it runs: its folder goes when this window has closed.",
            )
          : null,
    ),
    h(
      "div",
      { class: "cfoot" },
      button("Back", "panel-back", { variant: "ghost", disabled: busy, keys: "Escape" }),
      h("span", { class: "spacer" }),
      button("Remove from this computer", "remove-confirm", {
        variant: "danger",
        icon: "trash",
        disabled: busy,
      }),
    ),
  ];
}

/** The kept program was removed: only closing is left. */
export function removedView(s: CustomerSnapshot): HTMLElement[] {
  if (s.panel.kind !== "removed") return [];
  const outcome = s.panel.outcome;
  return [
    h(
      "main",
      { class: "cbody", id: "main", tabindex: -1, "data-key": "main-removed" },
      h(
        "div",
        null,
        h("h3", { id: "removed-title" }, "CretLink Support was removed from this computer"),
        h(
          "p",
          { class: "lead" },
          outcome.state === "pending"
            ? "The Start-menu entry and the entry in Installed apps are gone. The folder of the program goes when this window has closed."
            : "The program, its Start-menu entry and its entry in Installed apps are gone.",
        ),
      ),
      lines("What stays", "removed-stays-h", outcome.left),
      h("p", { class: "muted" }, "A session report that you saved stays where you saved it."),
    ),
    h(
      "div",
      { class: "cfoot" },
      h("span", { class: "spacer" }),
      button("Close CretLink", "close-app", {
        variant: "primary big",
        autofocus: true,
        disabled: s.closing,
      }),
    ),
  ];
}
