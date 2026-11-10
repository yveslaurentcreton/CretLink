// The full-control disclosure exactly as the customer reads it (ST-M009-008,
// REQ-S-003, REQ-S-016). The backend does not send the text to the console,
// so this is a verbatim copy of `DisclosureSummary::DEFAULT_TEXT` in
// `crates/session/src/lib.rs`; `disclosure.test.ts` fails when the two differ.
export const DISCLOSURE_TEXT =
  "Granting full control lets the remote technician operate this computer as if " +
  "seated at it for this session: run commands and scripts, install or remove " +
  "software and drivers, change system settings, read, modify, and delete files, " +
  "and restart the computer. Command output, screen contents, and files may be " +
  "sent to the technician and processed by their AI service. Control lasts only " +
  "until this session ends, and either of us can stop it at any time.";
