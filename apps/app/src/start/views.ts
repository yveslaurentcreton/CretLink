// The start screen (`#start`) and the install screen (`#t-install`) as pure
// functions of a snapshot (ST-M009-005, ST-M009-006). Every string becomes a
// text node through `h()`; controls carry `data-action` and the controller in
// `start.ts` dispatches them.
//
// - Start: Get help (Windows only) and Give help. Give help offers **Install
//   for me** and **Run once without installing**; a copy that runs from the
//   install location says it is installed and offers only Give help.
// - Install: the fixed location, the Start-menu or desktop entry option, and
//   each AI client with its detection result and the change install would make
//   (the setup plan), each selectable. **Back** returns to the start screen.

import type {
  AiClientStatus,
  AppInfo,
  ClientId,
  InstallOptions,
  InstallOutcome,
} from "../shared/contract";
import { type Child, h } from "../shared/dom";
import { type IconName, icon } from "../shared/icons";

export type StartPage = "start" | "install";

export interface StartSnapshot {
  page: StartPage;
  version: string;
  // Whether Get help exists here (Windows only).
  canGetHelp: boolean;
  // This copy; `null` while unknown.
  app: AppInfo | null;
  // The install screen's data: `undefined` while loading, `null` when it
  // could not be read.
  options: InstallOptions | null | undefined;
  // The menu-entry choice.
  entry: boolean;
  // The clients to register.
  selected: ClientId[];
  busy?: "customer" | "technician" | "install" | "open";
  // A plain-language problem, announced assertively.
  alert?: string;
  // An install that finished with something to say.
  outcome?: InstallOutcome;
}

/** The clients selected by default: present, and either not registered or
 * already pointing at the installed program. A registration of another
 * program is only replaced when the technician asks for it. */
export function defaultSelection(options: InstallOptions): ClientId[] {
  return options.clients
    .filter(
      (client) =>
        client.present &&
        (client.registration.state === "absent" || client.registration.state === "this_executable"),
    )
    .map((client) => client.id);
}

function btn(
  text: Child,
  action: string,
  variant: string,
  options: { arg?: string; disabled?: boolean; autofocus?: boolean; label?: string } = {},
): HTMLButtonElement {
  return h(
    "button",
    {
      type: "button",
      class: `btn ${variant}`,
      "data-action": action,
      "data-arg": options.arg,
      disabled: options.disabled,
      "data-autofocus": options.autofocus,
      "aria-label": options.label,
    },
    text,
  );
}

function fact(name: IconName, text: string): HTMLElement {
  return h("span", null, icon(name), text);
}

function alertRegion(s: StartSnapshot): HTMLElement {
  return h("div", { class: "alert", role: "alert" }, s.alert ? h("p", null, s.alert) : null);
}

/** The whole window: a skip link and the current page. */
export function renderStart(s: StartSnapshot): HTMLElement {
  return h(
    "div",
    { class: "cl-ui tech start-root" },
    h(
      "a",
      { class: "skip-link", href: "#main", "data-action": "skip" },
      s.page === "install" ? "Skip to the install options" : "Skip to the choices",
    ),
    s.page === "install" ? installView(s) : startView(s),
  );
}

export function startView(s: StartSnapshot): HTMLElement {
  const installed = s.app?.install === "installed";
  const where = s.app
    ? installed
      ? ` · installed in ${s.app.location}`
      : ` · running from ${s.app.location}`
    : "";
  const busy = s.busy !== undefined;
  return h(
    "main",
    { id: "main", class: "start", tabindex: -1 },
    h("h1", null, "What do you want to do?"),
    h("p", { class: "muted where" }, `CretLink ${s.version}${where}`),
    alertRegion(s),
    h(
      "div",
      { class: "choice", id: "choices" },
      s.canGetHelp
        ? h(
            "section",
            { class: "card get", "aria-labelledby": "get-title" },
            h("div", { class: "iconbox c" }, icon("life", "lg")),
            h("h2", { id: "get-title" }, "Get help"),
            h(
              "p",
              null,
              "Someone you are talking to helps you with this computer. You approve every step and can stop at any time.",
            ),
            h(
              "div",
              { class: "facts" },
              fact("check", "Nothing is installed"),
              fact("shield", "Windows asks for administrator permission"),
            ),
            h("div", { class: "spacer" }),
            btn("Get help", "choose", "primary big block c", {
              arg: "customer",
              disabled: busy,
              autofocus: true,
            }),
          )
        : null,
      h(
        "section",
        { class: "card give", "aria-labelledby": "give-title" },
        h("div", { class: "iconbox t" }, icon("tool", "lg")),
        h("h2", { id: "give-title" }, "Give help"),
        h(
          "p",
          null,
          "Help another computer, yourself or through your AI assistant: Claude, Codex or Gemini.",
        ),
        h(
          "div",
          { class: "facts" },
          fact("check", "No administrator rights needed"),
          fact("spark", "Connects your AI assistants"),
        ),
        h("div", { class: "spacer" }),
        installed
          ? [
              h(
                "p",
                { class: "installed" },
                h("span", { class: "pill ok" }, icon("check", "sm"), "Installed"),
                " for your account",
              ),
              btn("Give help", "choose", "primary big block", {
                arg: "technician",
                disabled: busy,
                autofocus: !s.canGetHelp,
              }),
            ]
          : [
              btn("Install for me", "install-screen", "primary big block", {
                disabled: busy,
                autofocus: !s.canGetHelp,
              }),
              btn("Run once without installing", "choose", "ghost block", {
                arg: "technician",
                disabled: busy,
              }),
            ],
      ),
    ),
  );
}

function plan(commands: string[]): Child {
  return commands.map((command, index) => [
    index > 0 ? ", then " : null,
    h("code", { class: "mono" }, command),
  ]);
}

/** What install would do for one client, in words, with the exact commands. */
export function clientChange(client: AiClientStatus): Child[] {
  if (!client.present) return ["Not found · you can connect it later"];
  const reg = client.registration;
  switch (reg.state) {
    case "this_executable":
      return ["Found · already connected to the installed copy; nothing to run"];
    case "absent":
      return ["Found · runs ", plan(client.installPlan)];
    case "other_command":
      return [
        "Found · registered to another program, ",
        h("code", { class: "mono" }, reg.command),
        ". Selecting it replaces that: runs ",
        plan(client.installPlan),
      ];
    case "unreadable":
      return [
        `Found · its configuration can't be read (${reg.message}). Selecting it registers again: runs `,
        plan(client.installPlan),
      ];
  }
}

function checkLine(
  id: string,
  action: string,
  checked: boolean,
  disabled: boolean,
  title: string,
  detail: Child[],
  arg?: string,
): HTMLElement {
  return h(
    "label",
    { class: disabled ? "check off" : "check", for: id },
    h("input", {
      type: "checkbox",
      id,
      "data-action": action,
      "data-arg": arg,
      checked,
      disabled,
      "aria-describedby": `${id}-detail`,
    }),
    h(
      "span",
      { class: "check-text" },
      h("b", null, title),
      " ",
      h("small", { id: `${id}-detail` }, detail),
    ),
  );
}

function entryLabel(entry: NonNullable<InstallOptions["entry"]>): string {
  return entry === "start_menu" ? "Add to the Start menu" : "Add to the applications menu";
}

function outcomeNote(outcome: InstallOutcome): HTMLElement {
  return h(
    "div",
    { class: "note outcome", id: "outcome", tabindex: -1 },
    icon("alert"),
    h(
      "div",
      null,
      h(
        "p",
        null,
        h("b", null, "CretLink is installed in "),
        h("span", { class: "mono" }, outcome.location),
        h("b", null, ", but:"),
      ),
      h(
        "ul",
        null,
        outcome.failures.map((failure) => h("li", null, failure)),
      ),
      h("p", null, "You can connect an AI assistant later from AI assistants."),
    ),
  );
}

export function installView(s: StartSnapshot): HTMLElement {
  const options = s.options;
  const busy = s.busy !== undefined;
  const body: Child[] = [];
  if (options === undefined) {
    body.push(
      h(
        "p",
        { "aria-busy": "true" },
        h("span", { class: "spinner", "aria-hidden": "true" }),
        " Checking this computer…",
      ),
    );
  } else if (options === null) {
    body.push(
      h(
        "p",
        { class: "muted" },
        "The install options could not be read. Go back and run CretLink once instead, or try again.",
      ),
    );
  } else {
    if (options.running) {
      body.push(
        h(
          "div",
          { class: "note" },
          icon("alert"),
          h(
            "p",
            null,
            "The installed CretLink is running. Close it, and any AI assistant that uses it, then install again.",
          ),
        ),
      );
    }
    body.push(
      h(
        "div",
        { class: "card row location" },
        icon("folder"),
        h("span", { class: "sr-only" }, "Install location: "),
        h("span", { class: "mono" }, options.location),
      ),
    );
    if (options.update) {
      body.push(
        h(
          "p",
          { class: "muted" },
          "CretLink is installed there already; install replaces it with this version and keeps its AI assistant connections.",
        ),
      );
    }
    if (options.entry) {
      body.push(
        checkLine(
          "opt-entry",
          "toggle-entry",
          s.entry,
          busy,
          entryLabel(options.entry),
          options.entryPath ? [options.entryPath] : [],
        ),
      );
    }
    body.push(
      h(
        "fieldset",
        { class: "clients" },
        h("legend", { class: "label" }, "Connect AI assistants"),
        options.clients.map((client) =>
          h(
            "div",
            { "data-key": `c-${client.id}` },
            checkLine(
              `opt-${client.id}`,
              "toggle-client",
              client.present && s.selected.includes(client.id),
              busy || !client.present,
              client.label,
              clientChange(client),
              client.id,
            ),
          ),
        ),
      ),
    );
  }
  const done = s.outcome?.state === "installed";
  return h(
    "main",
    { id: "main", class: "start install", tabindex: -1 },
    h("h1", null, "Install CretLink for you"),
    h(
      "p",
      null,
      "For your user account only. No administrator rights, no background service and nothing that starts by itself.",
    ),
    alertRegion(s),
    body,
    s.outcome && done ? outcomeNote(s.outcome) : null,
    s.busy === "open" || s.outcome?.state === "started"
      ? h(
          "p",
          { "aria-busy": "true" },
          h("span", { class: "spinner", "aria-hidden": "true" }),
          " Starting the installed CretLink…",
        )
      : null,
    h("div", { class: "spacer" }),
    h(
      "div",
      { class: "row end" },
      done
        ? btn("Open CretLink", "open-installed", "primary", { disabled: busy, autofocus: true })
        : [
            btn("Back", "back", "ghost", { disabled: busy }),
            btn(s.busy === "install" ? "Installing…" : "Install", "install", "primary", {
              disabled: busy || !options || options.running,
            }),
          ],
    ),
  );
}
