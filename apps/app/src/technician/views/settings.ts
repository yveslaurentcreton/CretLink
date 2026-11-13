// Settings (ST-M009-008): the version, the install path or the folder this
// copy runs from, the audit folder (Open), the relay of this copy
// (ST-M007-011, `./relay`), and Uninstall when installed (ST-M009-006).
// Uninstall first shows what it will remove.

import { h } from "../../shared/dom";
import { icon } from "../../shared/icons";
import type { TechnicianSnapshot } from "../model";
import { button, card, kv } from "./common";
import { relayCard } from "./relay";

export function settingsView(s: TechnicianSnapshot): HTMLElement {
  const app = s.app;
  const version = app?.version ?? s.version;
  return h(
    "div",
    { class: "screen settings" },
    h("h2", null, "Settings"),
    card(
      h("h4", null, "This copy"),
      kv([
        ["Version", version ? `CretLink ${version}` : "Unknown"],
        app
          ? [
              app.install === "installed" ? "Installed in" : "Runs from",
              h("span", { class: "mono" }, app.location),
            ]
          : ["Location", "Not available yet"],
        app
          ? [
              "Kind",
              app.install === "installed"
                ? "Installed for your account"
                : "Runs once, without installing",
            ]
          : null,
        [
          "Audit folder",
          app
            ? h(
                "span",
                { class: "with-action" },
                h("span", { class: "mono" }, app.auditDir),
                button("Open", "open-audit", {
                  variant: "sm",
                  icon: "folder",
                  label: "Open the audit folder",
                }),
              )
            : "Not available yet",
        ],
      ]),
    ),
    relayCard(s),
    app?.install === "installed" ? uninstallCard(s) : null,
    app?.install === "run_once"
      ? h(
          "p",
          { class: "muted" },
          "This copy writes nothing but the audit trail, so there is nothing to uninstall. Delete the file when you no longer need it.",
        )
      : null,
  );
}

function uninstallCard(s: TechnicianSnapshot): HTMLElement {
  if (s.uninstalled) {
    return card(
      h("h4", null, "Uninstall"),
      h(
        "p",
        null,
        "CretLink was uninstalled. Close this window to finish. The audit trail stays in the audit folder.",
      ),
    );
  }
  const step = s.uninstall;
  if (!step) {
    return card(
      h("h4", null, "Uninstall"),
      h(
        "p",
        null,
        "Removes this installed copy, its settings with the relay and its key, its shortcut, its uninstall entry and the AI assistant registrations that point at it.",
      ),
      button("Uninstall…", "uninstall-plan", { disabled: s.busy === "uninstall-plan" }),
    );
  }
  if (step.plan === null) {
    return card(
      h("h4", null, "Uninstall"),
      h(
        "p",
        { "aria-busy": "true" },
        h("span", { class: "spinner", "aria-hidden": "true" }),
        " Checking what would be removed…",
      ),
    );
  }
  return h(
    "div",
    { class: "card", id: "uninstall-step", tabindex: -1, "data-autofocus": true },
    h("h4", null, "Uninstall"),
    h("p", null, step.plan.length > 0 ? "This removes:" : "Nothing would be removed."),
    step.plan.length > 0
      ? h(
          "ul",
          { class: "plan" },
          step.plan.map((line) => h("li", { class: "mono" }, line)),
        )
      : null,
    h(
      "div",
      { class: "note" },
      icon("alert"),
      h(
        "p",
        null,
        "The audit trail stays",
        s.app ? [" in ", h("span", { class: "mono" }, s.app.auditDir)] : null,
        ".",
      ),
    ),
    h(
      "div",
      { class: "row" },
      button("Uninstall", "uninstall-confirm", {
        variant: "danger",
        disabled: s.busy === "uninstall",
      }),
      button("Cancel", "uninstall-cancel", { variant: "ghost" }),
    ),
  );
}
