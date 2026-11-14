// Development-only page for a relay code in the customer's window
// (ST-M007-013): the real customer controller and views, driven against a
// copy, a relay and a technician that are held in memory instead of a
// backend. Tauri's own mocks answer the commands, so nothing here reads a
// file or opens a connection.
//
// `?copy=` picks what the profile file next to the program says:
//
// | Value     | The copy |
// |-----------|----------|
// | `relay`   | set up for the relay relay.example.org (the default) |
// | `renamed` | set up for a relay that the profile names relay.example.org, reaches at another host and checks against a certificate file of its own |
// | `none`    | no profile: a relay code cannot be used |
// | `problem` | a profile that cannot be used |
//
// `?outcome=` picks what comes of a relay code in a copy with a relay:
//
// | Value         | What happens |
// |---------------|--------------|
// | `pair`        | the technician is there: the approval code, then the decision (the default) |
// | `nobody`      | nobody waits on the code |
// | `unreachable` | the relay cannot be reached |
// | `untrusted`   | the relay's certificate is not trusted |
// | `key`         | the relay does not know the key of this copy |
// | `lost`        | the relay stops while the code is compared |
//
// Each step takes a moment (`?pause=`, in milliseconds), so the screens in
// between can be looked at. A code of the same network (`6RRR-2H8F`) or an
// address connects without a relay, as in the application.

import "../src/base.css";
import { emit } from "@tauri-apps/api/event";
import { mockIPC, mockWindows } from "@tauri-apps/api/mocks";
import type { AgentPhase, ConsentView } from "../src/customer/api";
import {
  LAUNCH_READY,
  VIA_RELAY,
  VIA_RELAY_ELSEWHERE,
  viewIn,
} from "../src/customer/fixtures";
import { relayWords } from "../src/shared/relay";
import { start } from "../src/customer/main";

const params = new URLSearchParams(window.location.search);
const copy = params.get("copy") ?? "relay";
const outcome = params.get("outcome") ?? "pair";
const pause = Number(params.get("pause") ?? 600);

/** Counts the dials, so that a late step of an old one is dropped. */
let generation = 0;
/** What the profile next to the program says, for a copy that has a relay. */
const relayOfCopy =
  copy === "relay" ? VIA_RELAY : copy === "renamed" ? VIA_RELAY_ELSEWHERE : null;
let view: ConsentView = viewIn({ phase: "idle" }, { relay_of_copy: relayOfCopy });

/** A refusal as the backend words one. */
function refuse(stage: string, message: string): never {
  throw { stage, message };
}

/** Sets the view and pushes it to the window, as the session's task does. */
function show(next: ConsentView): ConsentView {
  // What the copy is set up for belongs to the process, not to one session.
  view = { ...next, relay_of_copy: relayOfCopy };
  void emit("agent-view", view);
  return view;
}

/** Runs `step` after a pause, unless the dial it belongs to is over. */
function later(step: () => void): void {
  const mine = generation;
  window.setTimeout(() => {
    if (mine === generation) step();
  }, pause);
}

/** Whether what was typed has the length of a relay code. */
function isRelayCode(typed: string): boolean {
  return /^[0-9A-Za-z]{16}$/.test(typed.replace(/[-\s]/g, ""));
}

const FAILURES: Record<string, { phase: AgentPhase; next: string | null }> = {
  nobody: {
    phase: {
      phase: "failed",
      stage: "rendezvous",
      message: "nobody joined the rendezvous on the relay",
    },
    next: null,
  },
  unreachable: {
    phase: {
      phase: "failed",
      stage: "relay_unreachable",
      message:
        "CretLink could not connect to the relay at relay.example.org:443. The relay may be off, the address may be wrong, or this network does not let this computer reach it.",
    },
    next: "Check that this computer is online, then try again. If it stays like this, tell your technician that the relay relay.example.org cannot be reached. Nobody has access to this computer.",
  },
  untrusted: {
    phase: {
      phase: "failed",
      stage: "relay_untrusted",
      message:
        "The relay at relay.example.org:443 showed no certificate that this computer trusts for the name relay.example.org, so nothing was sent to it.",
    },
    next: "Tell your technician that this computer does not trust the certificate of the relay relay.example.org. Nobody has access to this computer.",
  },
  key: {
    phase: {
      phase: "failed",
      stage: "relay_key_refused",
      message:
        "The relay relay.example.org does not know the admission key of this copy. The key may be mistyped, or it was withdrawn.",
    },
    next: "Ask your technician for a new copy of CretLink, or to look at the keys of their relay.",
  },
};

/** **Connect** with what was typed into the code field. */
function connect(typed: string): ConsentView {
  const relayCode = isRelayCode(typed);
  if (relayCode && copy === "none") {
    refuse(
      "relay",
      "This is a relay code, for a technician who is not on your network. This copy of CretLink is not set up for a relay, so it connects only on the same network.",
    );
  }
  if (relayCode && copy === "problem") {
    refuse(
      "relay_profile",
      "This is a relay code, but the relay settings of this copy of CretLink cannot be used. Line 2: `address` needs a port; write the relay's host name and port, like relay.example.org:443. The file is E:\\cretlink.toml.",
    );
  }
  generation += 1;
  const relay = relayCode ? relayOfCopy : null;
  const reaching = show(viewIn({ phase: "announced" }, { technician: null, relay }));
  later(() => {
    const failure = relayCode ? FAILURES[outcome] : undefined;
    if (failure) {
      show(viewIn(failure.phase, { technician: null, relay, failure_next: failure.next }));
      return;
    }
    show(viewIn({ phase: "awaiting_approval_compare" }, { approval_code: "482913", relay }));
    later(() => {
      if (relayCode && outcome === "lost") {
        show(
          viewIn(
            {
              phase: "failed",
              stage: "relay_lost",
              message: `The connection through the relay ${relay ? relayWords(relay) : ""} was lost.`,
            },
            { relay },
          ),
        );
        return;
      }
      // The technician confirmed that the codes match.
      show(viewIn({ phase: "awaiting_consent" }, { relay }));
    });
  });
  return reaching;
}

mockWindows("main");
mockIPC(
  (cmd, args) => {
    const payload = (args ?? {}) as Record<string, unknown>;
    switch (cmd) {
      case "get_launch":
        return LAUNCH_READY;
      case "get_view":
        return view;
      case "get_activity":
        return [];
      case "browse_consoles":
        return [];
      case "start_live_session":
        return connect(String(payload.code ?? ""));
      case "grant_consent": {
        const relay = view.relay;
        later(() => show(viewIn({ phase: "full_control", session: "7f3a9c51" }, { relay })));
        return view;
      }
      case "decline_consent":
      case "stop_session":
        generation += 1;
        view = viewIn(
          { phase: "stopped", reason: "customer_requested" },
          { relay: view.relay, relay_of_copy: relayOfCopy },
        );
        return view;
      case "get_end_report":
        return null;
      default:
        // As Tauri answers a command the backend does not register.
        throw `Command ${cmd} not found`;
    }
  },
  { shouldMockEvents: true },
);

void start({ window: "main" });
