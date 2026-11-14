// How a relay is named to a person (ST-M007-011, ST-M007-013), for both
// roles.
//
// A relay profile can give the relay a name that differs from the host the
// copy connects to, and a certificate file of its own to check that name
// against. Whoever can write the profile could give a server of their own the
// name of another relay. They would gain the place of a hostile relay, which
// sees encrypted traffic only, and not the session; but both people would
// read the wrong name. So wherever a relay is named:
//
// - the address stands beside the name when its host is another one
//   (`relayLabel`, `relayWords`), and
// - it is said when the certificate is checked against a file of the profile
//   and not against the certificate authorities of the computer
//   (`trustWords`).
//
// Every text of a relay is written as text, never as markup.

import type { RelayTrust } from "./contract";
import { type Child, h } from "./dom";
import { icon } from "./icons";

/** A relay as a screen is given it. */
export interface NamedRelay {
  // The relay's host name: the name its certificate carries.
  host: string;
  // Where the copy connects to reach it: `host:port`, an IPv6 address in
  // square brackets. Absent in the summary of an audit log from before it
  // was recorded.
  address?: string;
  // What the relay's certificate is checked against. Absent likewise.
  trust?: RelayTrust;
}

/** The host of `host:port`, without the square brackets of an IPv6 address. */
export function addressHost(address: string): string {
  const colon = address.lastIndexOf(":");
  const host = colon < 0 ? address : address.slice(0, colon);
  return host.startsWith("[") && host.endsWith("]") ? host.slice(1, -1) : host;
}

/**
 * Where the copy connects, when that is another host than the relay's name
 * says; nothing when the name is the host of the address. Another port is
 * not another host.
 */
export function reachedAt(relay: NamedRelay): string | undefined {
  const address = relay.address;
  if (!address) return undefined;
  return addressHost(address) === relay.host ? undefined : address;
}

/** The relay in a sentence: its name, and where it is reached when that differs. */
export function relayWords(relay: NamedRelay): string {
  const at = reachedAt(relay);
  return at ? `${relay.host} (reached at ${at})` : relay.host;
}

/**
 * The relay on a screen: its name set apart, and the address beside it when
 * the copy connects to another host than the name says.
 */
export function relayLabel(relay: NamedRelay): Child[] {
  const at = reachedAt(relay);
  return [
    h("b", { class: "mono relay-name" }, relay.host),
    at ? [" (reached at ", h("span", { class: "mono relay-address" }, at), ")"] : null,
  ];
}

/** What is said when a relay's certificate is checked against a file of the profile. */
export const PROFILE_FILE_TRUST =
  "The relay's certificate is checked against a certificate file of this copy's relay settings, not against the certificate authorities of this computer.";

/**
 * That the relay's certificate is checked against a file of the profile and
 * not against the system's authorities; nothing when the system's
 * authorities vouch for it, which is what a person expects.
 */
export function trustWords(relay: NamedRelay): string | undefined {
  return relay.trust === "profile_file" ? PROFILE_FILE_TRUST : undefined;
}

/**
 * The row of a list of facts about a session that says what the relay's
 * certificate was checked against; nothing when it was the system's
 * authorities.
 */
export function trustFact(relay: NamedRelay): readonly [string, string] | null {
  return relay.trust === "profile_file"
    ? [
        "Relay certificate",
        "Checked against a certificate file of this copy's relay settings, not against the certificate authorities of this computer",
      ]
    : null;
}

/** `trustWords` as a line of its own, or nothing. */
export function trustNote(relay: NamedRelay): HTMLElement | null {
  const words = trustWords(relay);
  return words ? h("span", { class: "muted relay-trust" }, words) : null;
}

/**
 * That the session runs through a relay, and through which: on every screen
 * from the comparison of the approval code to the end (ST-M007-013). The
 * relay passes the encrypted session on; it cannot look inside and grants
 * nothing.
 */
export function viaRelay(relay: NamedRelay | null | undefined): HTMLElement | null {
  if (!relay) return null;
  return h(
    "p",
    { class: "via-relay", "data-key": "via-relay" },
    icon("wifi", "sm"),
    h("span", null, "Through the relay ", relayLabel(relay)),
    h("span", { class: "muted" }, " · it only passes encrypted traffic on"),
    // On a line of its own; the blank keeps the two apart for a reader that
    // takes the text in one piece.
    trustWords(relay) ? [" ", trustNote(relay)] : null,
  );
}
