// The start screen: Get help or Give help (ST-M009-005, mock `#start`).
//
// It only reports the choice. The backend starts the chosen role in a new
// process — elevated through UAC for Get help — and closes this one, so the
// role stays fixed for the life of each process (ADR-0018).

import "./start.css";
import { invoke } from "@tauri-apps/api/core";
import type { RoleView } from "../main";

/** The backend's answer to `choose_role`. */
interface ChooseOutcome {
  outcome: "started" | "declined" | "failed";
  message: string;
}

const DECLINED =
  "Windows did not get permission, so nothing was started. To get help, choose Get help again and answer Yes when Windows asks.";

function el<K extends keyof HTMLElementTagNameMap>(
  tag: K,
  className?: string,
  text?: string,
): HTMLElementTagNameMap[K] {
  const node = document.createElement(tag);
  if (className) node.className = className;
  if (text !== undefined) node.textContent = text;
  return node;
}

function fact(text: string): HTMLLIElement {
  return el("li", undefined, text);
}

export function start(view: RoleView): void {
  const root = document.getElementById("app");
  if (!root) return;
  root.removeAttribute("aria-busy");
  document.body.classList.add("app-start");

  const main = el("main", "start");
  main.append(el("h1", undefined, "What do you want to do?"));
  main.append(el("p", "muted", `CretLink ${view.version}`));

  const notice = el("p", "notice");
  notice.setAttribute("role", "alert");
  notice.hidden = true;
  main.append(notice);
  const say = (message: string | null): void => {
    notice.textContent = message ?? "";
    notice.hidden = message === null;
  };
  if (view.declined) say(DECLINED);

  const choices = el("div", "choices");
  choices.id = "choices";

  const buttons: HTMLButtonElement[] = [];
  const choose = async (role: "technician" | "customer"): Promise<void> => {
    for (const button of buttons) button.disabled = true;
    say(null);
    try {
      const result = await invoke<ChooseOutcome>("choose_role", { role });
      if (result.outcome === "declined") say(DECLINED);
      if (result.outcome === "failed") say(result.message);
    } catch (error) {
      say(`CretLink could not start that: ${String(error)}`);
    } finally {
      for (const button of buttons) button.disabled = false;
    }
  };

  if (view.can_get_help) {
    const card = el("section", "card get");
    card.setAttribute("aria-labelledby", "get-title");
    const title = el("h2", undefined, "Get help");
    title.id = "get-title";
    card.append(title);
    card.append(
      el(
        "p",
        undefined,
        "Someone you are talking to helps you with this computer. You approve the connection, see everything that happens, and can stop at any time.",
      ),
    );
    const facts = el("ul", "facts");
    facts.append(fact("Nothing is installed"), fact("Windows asks for administrator permission"));
    card.append(facts);
    const button = el("button", "btn primary customer", "Get help");
    button.type = "button";
    button.addEventListener("click", () => void choose("customer"));
    buttons.push(button);
    card.append(button);
    choices.append(card);
  }

  const card = el("section", "card give");
  card.setAttribute("aria-labelledby", "give-title");
  const title = el("h2", undefined, "Give help");
  title.id = "give-title";
  card.append(title);
  card.append(
    el(
      "p",
      undefined,
      "Help another computer yourself, or through your AI assistant: Claude, Codex or Gemini.",
    ),
  );
  const facts = el("ul", "facts");
  facts.append(fact("No administrator rights needed"), fact("Connects your AI assistants"));
  card.append(facts);
  const button = el("button", "btn primary technician", "Give help");
  button.type = "button";
  button.addEventListener("click", () => void choose("technician"));
  buttons.push(button);
  card.append(button);
  choices.append(card);

  main.append(choices);
  root.replaceChildren(main);
  buttons[0]?.focus();
}
