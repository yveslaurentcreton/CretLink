// Development-only page for the relay screens (ST-M007-011): the real
// technician controller and views, driven against a copy and a relay that
// are held in memory instead of a backend. Tauri's own mocks answer the
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
// `?relay=` picks how the relay answers a test: `admit` (the default), `key`
// (it does not know the key) or `down` (it cannot be reached). The relay
// takes a moment to answer, so the waiting state can be looked at.
//
// The simulated backend follows the rules of the real one where the screens
// depend on them: an address needs a port, a key is never sent back, the
// certificate file is named by a number, and a copy that runs once refuses
// to keep anything.

import "../src/base.css";
import { mockIPC, mockWindows } from "@tauri-apps/api/mocks";
import type {
  AppInfo,
  ChosenCa,
  RelayForm,
  RelaySettings,
  RelayTest,
  RelayView,
} from "../src/shared/contract";
import {
  APP_INSTALLED,
  APP_RUN_ONCE,
  RELAY_FILE,
  RELAY_FILE_PROBLEM,
  RELAY_NONE,
  RELAY_RUN_ONCE,
  RELAY_STORED,
  consoleIn,
} from "../src/technician/fixtures";
import { start } from "../src/technician/main";

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

const state = consoleIn({ phase: "idle" }, { displayName: "Sam" });

mockWindows("main");
mockIPC(
  (cmd, args) => {
    const payload = (args ?? {}) as Record<string, unknown>;
    switch (cmd) {
      case "get_state":
        return state;
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
