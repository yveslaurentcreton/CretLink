//! Per-session audit log: typed JSON Lines with a SHA-256 hash chain
//! (ST-M002-007, ADR-0008).
//!
//! # What this module is
//!
//! The technician console must keep a complete, append-oriented record of
//! every session action — discovery, pairing, consent, state changes,
//! revocation, errors, and cleanup — without ever writing a secret to disk
//! (REQ-F-017, REQ-S-017, REQ-S-018). This module implements the audit format
//! decided in ADR-0008: one JSON object per line, one file per session, each
//! line carrying the SHA-256 of the previous line so accidental corruption or
//! casual edits are evident.
//!
//! The log is written **only** by [`AuditWriter`], a dedicated typed writer.
//! Diagnostics (`tracing`) are a separate concern and never author the
//! `.jsonl`.
//!
//! # Structural secret exclusion (REQ-S-017)
//!
//! There is no serialization path for key material, the Session Approval Code,
//! nonces, or bearer tokens. Arguments are an [`AuditValue`] tree whose only
//! byte-bearing variants are digests and length markers — no variant can carry
//! a live key. A caller that holds a secret can put it into the log **only** by
//! passing it through [`redact`], which replaces it with an
//! [`AuditValue::Redacted`] marker. See the `secret_is_representable_only_as_marker`
//! test.
//!
//! # Redaction and truncation (REQ-S-018)
//!
//! Redaction happens at capture time: [`redact`] yields a marker rendered as
//! `[REDACTED]`. Oversized fields are capped at [`MAX_FIELD_BYTES`] (32 KiB) by
//! [`truncate_field`], which keeps a prefix and records the original length.
//! Retroactive line editing is deliberately unsupported because it would break
//! the hash chain; the remedy is to **append** a later redaction-request
//! record (or to delete the whole session file), never to edit a written line.
//!
//! # Hash chain: tamper *evidence*, not tamper-*proofing*
//!
//! Each line's `prev` field is the SHA-256 (lowercase hex) of the previous
//! line's exact serialized bytes (the JSON object, excluding the terminating
//! newline). The session-header record uses [`ZERO_PREV`]. This makes
//! accidental corruption and casual edits detectable via [`verify_chain`], and
//! a dropped tail detectable via [`verify_sealed`], which additionally requires
//! the trailing `session_closed` seal and checks its sealed record count
//! against the number of records verified. It is **not** protection against a
//! compromised technician machine, which can rewrite the entire chain —
//! ADR-0008 is explicit that the local trust model concedes that case:
//! tampering that rebuilds a fully consistent, correctly sealed chain is out of
//! scope by design.
//!
//! # Determinism
//!
//! Timestamps come from an injected [`Clock`]; tests use [`FixedClock`] and a
//! fixed [`cretlink_protocol::SessionId`] so the golden chain hashes are
//! reproducible. Production uses [`SystemClock`]. Every nested map is a
//! `BTreeMap`, so `serde_json` emits stable key ordering and the chain hashes
//! are a pure function of the inputs.

mod clock;
mod record;
mod retention;
mod value;
mod writer;

pub use clock::{Clock, FixedClock, SystemClock, format_rfc3339_millis, parse_rfc3339_millis};
pub use record::{AUDIT_SCHEMA_VERSION, Actor, AuditRecord, Outcome, ZERO_PREV};
pub use retention::{
    AuditFileMeta, DEFAULT_SESSION_SOFT_CAP_BYTES, RetentionPolicy, files_to_prune,
    session_file_name,
};
pub use value::{
    AuditArgs, AuditValue, MAX_FIELD_BYTES, MIN_DIGEST_LEN, redact, redact_no_digest,
    truncate_field,
};
pub use writer::{AuditError, AuditWriter, ChainBreak, ChainReport, verify_chain, verify_sealed};

/// Computes the lowercase-hex SHA-256 of `bytes` using the workspace `sha2`
/// (0.10 line, shared with the crypto stack — ST-M002-004).
///
/// This is the single digest primitive used for chain links, redaction
/// digests, and any file/output digests recorded in [`AuditRecord::digests`],
/// matching ADR-0008's "one audited algorithm" rationale.
#[must_use]
pub(crate) fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};

    let mut hasher = Sha256::new();
    hasher.update(bytes);
    let digest = hasher.finalize();
    let mut out = String::with_capacity(digest.len() * 2);
    for byte in digest {
        // Two lowercase hex digits per byte; `write!` to a String is
        // infallible, so format directly without an unwrap.
        out.push(char::from_digit((byte >> 4) as u32, 16).unwrap_or('0'));
        out.push(char::from_digit((byte & 0x0f) as u32, 16).unwrap_or('0'));
    }
    out
}

#[cfg(test)]
mod tests;
