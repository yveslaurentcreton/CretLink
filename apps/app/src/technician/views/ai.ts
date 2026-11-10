// AI assistants (`#t-ai`, ST-M009-008): the GUI face of `cretlink mcp
// install | uninstall | status` (ST-M009-003). Each client shows whether it is
// present and registered, with its configuration path. Connect and Disconnect
// first show the exact commands (REQ-F-025), then run them through the setup
// interface; a registration pointing at another program offers Replace.
//
// Claude Desktop has no entry of its own: its Code tab runs Claude Code and
// reads the same registration, which the Claude Code entry says.

import type { AiClientStatus, ClientId } from "../../shared/contract";
import { h } from "../../shared/dom";
import { icon } from "../../shared/icons";
import type { TechnicianSnapshot } from "../model";
import { button, unavailable } from "./common";
import { clientStatusWords } from "./home";

export function aiView(s: TechnicianSnapshot): HTMLElement {
  const runOnce = s.app?.install === "run_once";
  return h(
    "div",
    { class: "screen ai" },
    h(
      "div",
      null,
      h("h2", null, "AI assistants"),
      h(
        "p",
        null,
        "Connect an assistant once. After that it sees CretLink's remote tools, but only for a session you start, and only after the customer allows it.",
      ),
    ),
    runOnce ? runOnceWarning(s.app?.location) : null,
    h(
      "div",
      { class: "card list-card" },
      s.clients === null
        ? unavailable("The AI assistant status")
        : h(
            "ul",
            { class: "list clients" },
            s.clients.map((client) => clientRow(s, client)),
          ),
    ),
    terminalEquivalent(s.clients),
  );
}

function runOnceWarning(location: string | undefined): HTMLElement {
  return h(
    "div",
    { class: "note run-once" },
    icon("alert"),
    h(
      "p",
      null,
      h("b", null, "This copy runs once. "),
      "A registration made from here points at this copy",
      location ? [" (", h("span", { class: "mono" }, location), ")"] : null,
      " and stops working when it is moved or deleted. Install CretLink to keep it working.",
    ),
  );
}

// What else a client's registration serves, in one line under its state.
const ALSO_SERVES: Partial<Record<ClientId, string>> = {
  claude: "Also serves the Code tab of Claude Desktop, which uses the same registration.",
};

function registrationText(client: AiClientStatus): string {
  const reg = client.registration;
  const where = client.configPath ? ` · ${client.configPath}` : "";
  switch (reg.state) {
    case "this_executable":
      return `Connected${where}`;
    case "other_command":
      return `Registered to another program${where}`;
    case "unreadable":
      return `The configuration can't be read${where}: ${reg.message}`;
    case "absent":
      return client.present ? `Found · not connected${where}` : "Not found on this computer";
  }
}

function clientRow(s: TechnicianSnapshot, client: AiClientStatus): HTMLElement {
  const status = clientStatusWords(client);
  const confirm = s.confirm?.client === client.id ? s.confirm : undefined;
  const busy = s.busy === `ai-${client.id}`;
  const reg = client.registration;
  const alsoServes = ALSO_SERVES[client.id];
  let action: HTMLElement | null = null;
  if (!confirm) {
    if (reg.state === "this_executable") {
      action = button("Disconnect", "ai-disconnect", {
        arg: client.id,
        variant: "sm",
        disabled: busy,
        label: `Disconnect ${client.label}`,
      });
    } else if (reg.state === "other_command") {
      action = button("Replace", "ai-replace", {
        arg: client.id,
        variant: "sm",
        disabled: busy || !client.present,
        label: `Replace the registration of ${client.label}`,
      });
    } else if (reg.state === "absent" && client.present) {
      action = button("Connect", "ai-connect", {
        arg: client.id,
        variant: "sm primary",
        disabled: busy,
        label: `Connect ${client.label}`,
      });
    }
  }
  return h(
    "li",
    { class: "it client", "data-key": `c-${client.id}` },
    h("span", { class: `dot ${status.dot}`, "aria-hidden": "true" }),
    h(
      "div",
      { class: "client-text" },
      h("b", null, client.label),
      h(
        "div",
        { class: "muted" },
        h("span", { class: "sr-only" }, `${status.text}. `),
        registrationText(client),
      ),
      reg.state === "other_command"
        ? h("div", { class: "muted" }, "It runs: ", h("code", { class: "mono" }, reg.command))
        : null,
      alsoServes ? h("div", { class: "muted also-serves" }, alsoServes) : null,
      confirm ? confirmStep(s, client, confirm.action, confirm.replace, busy) : null,
    ),
    action,
  );
}

function confirmStep(
  s: TechnicianSnapshot,
  client: AiClientStatus,
  action: "install" | "uninstall",
  replace: boolean,
  busy: boolean,
): HTMLElement {
  const plan = action === "install" ? client.installPlan : client.uninstallPlan;
  const verb = action === "uninstall" ? "Disconnect" : replace ? "Replace" : "Connect";
  return h(
    "div",
    {
      class: "confirm",
      id: "confirm-step",
      role: "group",
      tabindex: -1,
      "aria-label": `${verb} ${client.label}`,
      "data-autofocus": true,
    },
    h(
      "p",
      { class: "confirm-head" },
      plan.length > 0 ? "This runs exactly:" : "There is nothing to run.",
    ),
    plan.length > 0 ? h("pre", { class: "cli" }, plan.join("\n")) : null,
    action === "install" && s.app?.install === "run_once"
      ? h(
          "p",
          { class: "muted" },
          "The registration will point at this run-once copy and break when it is moved or deleted.",
        )
      : null,
    h(
      "div",
      { class: "row" },
      button(verb, "ai-confirm", {
        arg: client.id,
        variant: action === "uninstall" ? "sm" : "sm primary",
        disabled: busy || plan.length === 0,
      }),
      button("Cancel", "ai-cancel", { arg: client.id, variant: "sm ghost", disabled: busy }),
    ),
  );
}

/** The same from a terminal. */
function terminalEquivalent(clients: AiClientStatus[] | null): HTMLElement {
  const ids = clients ?? [];
  const lines: Array<HTMLElement | string> = [
    h("span", { class: "d" }, "# the same from a terminal"),
    "\n",
  ];
  if (ids.length === 0) {
    lines.push(
      h("span", { class: "p" }, "> "),
      "cretlink mcp install --client <claude|codex|gemini>",
      "\n",
    );
  }
  for (const client of ids) {
    const verb = client.registration.state === "this_executable" ? "uninstall" : "install";
    lines.push(h("span", { class: "p" }, "> "), `cretlink mcp ${verb} --client ${client.id}`, "\n");
  }
  lines.push(h("span", { class: "p" }, "> "), "cretlink mcp status");
  return h("pre", { class: "cli", "aria-label": "The equivalent terminal commands" }, lines);
}
