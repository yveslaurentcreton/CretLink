// @vitest-environment jsdom
import { describe, expect, it } from "vitest";
import {
  PROFILE_FILE_TRUST,
  addressHost,
  reachedAt,
  relayLabel,
  relayWords,
  trustFact,
  trustNote,
  trustWords,
  viaRelay,
} from "./relay";
import { h } from "./dom";

const PLAIN = {
  host: "relay.example.org",
  address: "relay.example.org:443",
  trust: "system_authorities",
} as const;

// What somebody who can write a profile would use to give a server of their
// own the name of another relay: another host, the relay's name, and a
// certificate file of their own.
const RENAMED = {
  host: "relay.example.org",
  address: "192.0.2.1:9970",
  trust: "profile_file",
} as const;

function text(children: ReturnType<typeof relayLabel>): string {
  return h("span", null, children).textContent ?? "";
}

describe("how a relay is named to a person (ST-M007-011)", () => {
  it("reads the host of an address, an IPv6 address without its brackets", () => {
    expect(addressHost("relay.example.org:443")).toBe("relay.example.org");
    expect(addressHost("192.0.2.1:9970")).toBe("192.0.2.1");
    expect(addressHost("[2001:db8::1]:443")).toBe("2001:db8::1");
    expect(addressHost("relay.example.org")).toBe("relay.example.org");
  });

  it("says where the copy connects only when that is another host than the name", () => {
    expect(reachedAt(PLAIN)).toBeUndefined();
    // Another port is not another host.
    expect(reachedAt({ host: "relay.example.org", address: "relay.example.org:9970" })).toBeUndefined();
    expect(reachedAt({ host: "2001:db8::1", address: "[2001:db8::1]:443" })).toBeUndefined();
    expect(reachedAt(RENAMED)).toBe("192.0.2.1:9970");
    expect(reachedAt({ host: "relay.example.org", address: "[2001:db8::1]:443" })).toBe(
      "[2001:db8::1]:443",
    );
    // The summary of an audit log from before the address was recorded.
    expect(reachedAt({ host: "relay.example.org" })).toBeUndefined();
  });

  it("names the relay in a sentence and on a screen, with the address when it differs", () => {
    expect(relayWords(PLAIN)).toBe("relay.example.org");
    expect(relayWords(RENAMED)).toBe("relay.example.org (reached at 192.0.2.1:9970)");
    expect(text(relayLabel(PLAIN))).toBe("relay.example.org");
    expect(text(relayLabel(RENAMED))).toBe("relay.example.org (reached at 192.0.2.1:9970)");
    const label = h("span", null, relayLabel(RENAMED));
    expect(label.querySelector(".relay-name")?.textContent).toBe("relay.example.org");
    expect(label.querySelector(".relay-address")?.textContent).toBe("192.0.2.1:9970");
  });

  it("says what vouches for the name only when it is a file of the profile", () => {
    expect(trustWords(PLAIN)).toBeUndefined();
    expect(trustNote(PLAIN)).toBeNull();
    expect(trustFact(PLAIN)).toBeNull();
    expect(trustWords({ host: "relay.example.org" })).toBeUndefined();
    expect(trustWords(RENAMED)).toBe(PROFILE_FILE_TRUST);
    expect(PROFILE_FILE_TRUST).toBe(
      "The relay's certificate is checked against a certificate file of this copy's relay settings, not against the certificate authorities of this computer.",
    );
    expect(trustNote(RENAMED)?.textContent).toBe(PROFILE_FILE_TRUST);
    expect(trustFact(RENAMED)).toEqual([
      "Relay certificate",
      "Checked against a certificate file of this copy's relay settings, not against the certificate authorities of this computer",
    ]);
  });

  it("says on every screen of a session which relay it runs through", () => {
    expect(viaRelay(undefined)).toBeNull();
    expect(viaRelay(null)).toBeNull();
    expect(viaRelay(PLAIN)?.textContent).toBe(
      "Through the relay relay.example.org · it only passes encrypted traffic on",
    );
    const renamed = viaRelay(RENAMED);
    expect(renamed?.textContent).toBe(
      `Through the relay relay.example.org (reached at 192.0.2.1:9970) · it only passes encrypted traffic on ${PROFILE_FILE_TRUST}`,
    );
    expect(renamed?.querySelector(".relay-trust")?.textContent).toBe(PROFILE_FILE_TRUST);
  });

  it("writes every text of a relay as text, never as markup", () => {
    const xss = '<img src=x onerror="alert(1)">';
    const line = viaRelay({ host: xss, address: `${xss}:443x`, trust: "profile_file" });
    expect(line?.querySelector("img")).toBeNull();
    expect(line?.textContent).toContain(xss);
  });
});
