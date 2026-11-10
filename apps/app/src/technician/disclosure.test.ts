import { describe, expect, it } from "vitest";
import rust from "../../../../crates/session/src/lib.rs?raw";
import { DISCLOSURE_TEXT } from "./disclosure";

// The technician's consent screen shows the disclosure exactly as the customer
// reads it (ST-M009-008). The console does not receive the text, so
// `DISCLOSURE_TEXT` must match `DisclosureSummary::DEFAULT_TEXT` verbatim;
// this test reads the Rust source and fails when the two drift apart.
describe("DISCLOSURE_TEXT", () => {
  it("matches DisclosureSummary::DEFAULT_TEXT", () => {
    const match = /pub const DEFAULT_TEXT: &'static str = "((?:[^"\\]|\\.)*)";/s.exec(rust);
    expect(match).not.toBeNull();
    // Rust string continuation: a backslash before a newline skips the newline
    // and the next line's leading whitespace.
    const text = (match?.[1] ?? "").replace(/\\\n\s*/g, "").replace(/\\"/g, '"');
    expect(DISCLOSURE_TEXT).toBe(text);
  });
});
