// Development-only page for the relay screens (ST-M007-011, ST-M007-013): the
// real technician controller and views, driven against a copy and a relay
// that are held in memory instead of a backend. Tauri's own mocks answer the
// commands, so nothing here reads a file, keeps a key or opens a connection.
//
// `?copy=` picks what the copy holds when the page opens:
//
// | Value          | The copy |
// |----------------|----------|
// | `installed`    | installed, no relay yet (the default) |
// | `kept`         | installed, with a relay in its settings |
// | `file`         | installed, with a profile file next to the program that wins |
// | `file-problem` | installed, with a profile file that cannot be used |
// | `run-once`     | runs once, and keeps no settings |
//
// `?relay=` picks how the relay answers a test and a session: `admit` (the
// default), `key` (it does not know the key) or `down` (it cannot be
// reached). The relay takes a moment to answer (`?pause=`, in milliseconds),
// so the waiting states can be looked at.
//
// A session through the relay (ST-M007-013) starts from **Start a session**
// and **Use relay** on the waiting screen: the simulated backend stops the
// wait on the network, answers with a relay code, and a moment later says
// that the relay holds it, or why it cannot be used. With `?customer=arrives`
// a customer then types the code: the approval code follows, **Codes match**
// leads to the customer's decision, and the customer allows. `?customer=lost`
// lets the relay stop while the code is compared.
//
// The simulated backend follows the rules of the real one where the screens
// depend on them: an address needs a port, a key is never sent back, the
// certificate file is named by a number, and a copy that runs once refuses
// to keep anything.

import "../src/base.css";
import { emit } from "@tauri-apps/api/event";
import { mockIPC, mockWindows } from "@tauri-apps/api/mocks";
import type {
  AppInfo,
  ChosenCa,
  RelayForm,
  RelaySettings,
  RelayTest,
  RelayView,
} from "../src/shared/contract";
import type { ConsoleState } from "../src/technician/api";
import {
  APP_INSTALLED,
  APP_RUN_ONCE,
  NETWORKS,
  RELAY_FILE,
  RELAY_FILE_PROBLEM,
  RELAY_NONE,
  RELAY_RUN_ONCE,
  RELAY_STORED,
  consoleIn,
} from "../src/technician/fixtures";
import { start } from "../src/technician/main";
import { activeRelay } from "../src/technician/model";

const params = new URLSearchParams(window.location.search);
const copy = params.get("copy") ?? "installed";
const answers = params.get("relay") ?? "admit";
/** How long the simulated relay takes to answer a test, in milliseconds. */
const pause = Number(params.get("pause") ?? 600);

const COPIES: Record<string, RelaySettings> = {
  installed: RELAY_NONE,
  kept: RELAY_STORED,
  file: RELAY_FILE,
  "file-problem": RELAY_FILE_PROBLEM,
  "run-once": RELAY_RUN_ONCE,
};

let settings: RelaySettings = COPIES[copy] ?? RELAY_NONE;
const app: AppInfo = settings.editable ? APP_INSTALLED : APP_RUN_ONCE;
/** Whether the copy keeps a key. The key itself is not kept on this page. */
let keySet = settings.stored?.relay?.keySet ?? false;
let nextPick = 1;
let picked: ChosenCa | null = null;

/** A refusal as the backend words one. */
function refuse(stage: string, message: string): never {
  throw { stage, message };
}

/** The relay of a form, as the backend would keep it. */
function relayOf(form: RelayForm): RelayView {
  const address = form.address.trim();
  const colon = address.lastIndexOf(":");
  if (address === "") {
    refuse(
      "relay_address",
      "`address` is empty; write the relay's host name and port, like relay.example.org:443.",
    );
  }
  if (address.includes("/")) {
    refuse(
      "relay_address",
      "`address` is the relay's host name and port only, like relay.example.org:443, without wss:// and without a path; CretLink always uses TLS.",
    );
  }
  if (colon <= 0 || !/^\d{1,5}$/.test(address.slice(colon + 1))) {
    refuse(
      "relay_address",
      "`address` needs a port; write the relay's host name and port, like relay.example.org:443.",
    );
  }
  if (form.keyMode === "set" && /\s/.test(form.key ?? "")) {
    refuse("relay_key", "`key` holds a blank or a character that is not plain text.");
  }
  if (form.caMode === "chosen" && form.caPick !== picked?.pick) {
    refuse(
      "relay_ca",
      "The certificate file that was chosen is no longer at hand; choose it again.",
    );
  }
  const stored = settings.stored?.relay;
  const host = (form.name ?? address.slice(0, colon)).toLowerCase();
  if (form.keyMode === "keep" && keySet && stored && stored.host !== host) {
    refuse(
      "relay_key",
      `The key that is kept is the key of the relay ${stored.host}; enter the key of this relay, or remove the key.`,
    );
  }
  const relay: RelayView = {
    host,
    address: address.toLowerCase(),
    keySet: form.keyMode === "set" || (form.keyMode === "keep" && keySet),
  };
  if (form.caMode === "chosen" || (form.caMode === "keep" && stored?.ca)) {
    relay.ca = "relay-ca.pem";
  }
  if (form.name) relay.name = form.name.toLowerCase();
  return relay;
}

function runOnce(): never {
  return refuse(
    "relay_run_once",
    "This copy runs once and keeps no settings. To give it a relay, put a file cretlink.toml next to the program.",
  );
}

/** What the simulated relay says to a test of `relay`. */
function test(relay: RelayView): RelayTest {
  switch (answers) {
    case "key":
      return {
        ok: false,
        kind: relay.keySet ? "key_refused" : "key_required",
        message: relay.keySet
          ? `The relay ${relay.host} does not know the admission key of this copy. The key may be mistyped, or it was withdrawn.`
          : `The relay ${relay.host} asks for an admission key, and this copy has none.`,
        next: relay.keySet
          ? "Enter the key again, or ask the relay's operator for a new one."
          : "Enter the key that the relay's operator gave this copy.",
      };
    case "down":
      return {
        ok: false,
        kind: "unreachable",
        message: `CretLink could not connect to the relay at ${relay.address}. The relay may be off, the address may be wrong, or this network does not let this computer reach it.`,
        next: "Check that the relay is running and that its address and port are right, then try again.",
      };
    default:
      return {
        ok: true,
        kind: "admitted",
        message: `The relay ${relay.host} answered and admits this copy. Its certificate is valid for ${relay.host}, checked against ${relay.ca ? "the certificate file of this profile" : "the certificate authorities of this computer"}.`,
        next: "",
      };
  }
}

// --- A session, on this network or through the relay (ST-M007-013) ---

/** What the simulated customer does once the relay holds the code. */
const customer = params.get("customer") ?? "";
/** The relay codes of this page, one for every wait, as the backend draws one. */
const CODES = ["53ST-A71D-JKKV-0VGH", "7KQM-2D8R-XW4H-91TC", "B6ZN-QF03-M5YJ-8E2A"];
let waits = 0;
/** Counts the waits and sessions, so that a late step of an old one is dropped. */
let generation = 0;

let state: ConsoleState = consoleIn({ phase: "idle" }, { displayName: "Sam" });

/** Sets the state and pushes it to the window, as the session's task does. */
function show(next: ConsoleState): ConsoleState {
  state = next;
  void emit("console-state", next);
  return next;
}

/** Runs `step` after the relay's pause, unless the wait it belongs to is over. */
function later(step: () => void, times = 1): void {
  const mine = generation;
  window.setTimeout(() => {
    if (mine === generation) step();
  }, pause * times);
}

function listening(): ConsoleState {
  return consoleIn(
    { phase: "listening", code: "6RRR-2H8F", address: "10.0.0.42:7000" },
    { displayName: "Sam" },
  );
}

/** The words of a relay that cannot be used, as the backend gives them. */
function relayFailure(relay: RelayView): { stage: string; message: string; next: string } {
  const where = " You can change and test the relay in Settings, under Relay.";
  const result = test(relay);
  return { stage: `relay_${result.kind}`, message: result.message, next: result.next + where };
}

/** **Use relay**: the wait on the network ends, and the wait on the relay begins. */
function useRelay(): Promise<ConsoleState> {
  if (state.phase.phase !== "listening" || state.relay) {
    refuse("relay", "A relay can be chosen only while waiting for the customer.");
  }
  const relay = activeRelay(settings);
  if (!relay) {
    const problem = settings.source === "file" ? settings.file.problem : settings.stored?.problem;
    if (problem) refuse("relay_profile", `${problem.message} (${problem.file})`);
    refuse(
      "relay_none",
      settings.editable
        ? "This copy has no relay. Set one up in Settings, under Relay."
        : "This copy has no relay. Put a file cretlink.toml next to the program; Settings, Relay shows where and what it holds.",
    );
  }
  generation += 1;
  // The listener of this network goes first, as in the application.
  show(consoleIn({ phase: "stopped", reason: "technician_requested" }, { displayName: "Sam" }));
  const code = CODES[waits % CODES.length] ?? "";
  waits += 1;
  const route = { host: relay.host, admitted: false };
  const waiting = (admitted: boolean): ConsoleState =>
    consoleIn(
      { phase: "listening", code, address: relay.address },
      { displayName: "Sam", relay: { ...route, admitted } },
    );
  const mine = generation;
  return new Promise((resolve) => {
    window.setTimeout(() => {
      if (mine !== generation) {
        resolve(state);
        return;
      }
      resolve(show(waiting(false)));
      later(() => {
        if (answers !== "admit") {
          const failure = relayFailure(relay);
          show(
            consoleIn(
              { phase: "failed", ...failure },
              { displayName: "Sam", relay: { ...route, admitted: false } },
            ),
          );
          return;
        }
        const held = { ...route, admitted: true };
        show(waiting(true));
        if (customer === "") return;
        later(() => {
          show(
            consoleIn(
              { phase: "awaiting_approval_code", code: "482913" },
              { session: true, target: true, displayName: "Sam", relay: held },
            ),
          );
          if (customer !== "lost") return;
          later(() => {
            show(
              consoleIn(
                {
                  phase: "failed",
                  stage: "relay_lost",
                  message: `The connection through the relay ${relay.host} was lost.`,
                },
                { displayName: "Sam", relay: held },
              ),
            );
          });
        });
      });
    }, pause);
  });
}

/** **Codes match**: the customer decides, and here allows. */
function confirmCode(): ConsoleState {
  if (state.phase.phase !== "awaiting_approval_code") {
    refuse("compare", "there is no approval code to confirm right now");
  }
  const relay = state.relay;
  const next = show(
    consoleIn(
      { phase: "awaiting_consent" },
      { session: true, target: true, displayName: "Sam", ...(relay ? { relay } : {}) },
    ),
  );
  later(() => {
    show(
      consoleIn(
        { phase: "full_control" },
        { session: true, target: true, displayName: "Sam", ...(relay ? { relay } : {}) },
      ),
    );
  });
  return next;
}

/** **Cancel**, **Stop** and Escape. */
function stopSession(): ConsoleState {
  generation += 1;
  const relay = state.relay;
  state = consoleIn(
    { phase: "stopped", reason: "technician_requested" },
    { displayName: "Sam", ...(relay ? { relay } : {}) },
  );
  return state;
}

mockWindows("main");
mockIPC(
  (cmd, args) => {
    const payload = (args ?? {}) as Record<string, unknown>;
    switch (cmd) {
      case "get_state":
        return state;
      case "start_live_session":
        generation += 1;
        return show(listening());
      case "get_listen_networks":
        return NETWORKS;
      case "use_relay":
        return useRelay();
      case "confirm_code":
        return confirmCode();
      case "stop_session":
        return stopSession();
      case "get_app_info":
        return app;
      case "get_relay_settings":
        return settings;
      case "choose_relay_ca":
        // The system's dialog cannot open here: a file is "chosen" at once.
        picked = { pick: nextPick, name: "authority.pem" };
        nextPick += 1;
        return picked;
      case "save_relay": {
        if (!settings.editable) runOnce();
        const form = payload.form as RelayForm;
        const relay = relayOf(form);
        keySet = relay.keySet;
        picked = null;
        settings = {
          ...settings,
          source: settings.source === "file" ? "file" : "settings",
          stored: { path: settings.stored?.path ?? "", relay },
        };
        return settings;
      }
      case "remove_relay":
        if (!settings.editable) runOnce();
        keySet = false;
        picked = null;
        settings = {
          ...settings,
          source: settings.source === "file" ? "file" : "none",
          stored: { path: settings.stored?.path ?? "" },
        };
        return settings;
      case "test_relay": {
        const form = payload.form as RelayForm | undefined;
        const relay = form
          ? relayOf(form)
          : settings.source === "file"
            ? settings.file.relay
            : settings.stored?.relay;
        if (!relay) refuse("relay_none", "This copy has no relay to test.");
        return new Promise((resolve) => {
          window.setTimeout(() => resolve(test(relay)), pause);
        });
      }
      default:
        // As Tauri answers a command the backend does not register.
        throw `Command ${cmd} not found`;
    }
  },
  { shouldMockEvents: true },
);

void start({ version: "dev" });
