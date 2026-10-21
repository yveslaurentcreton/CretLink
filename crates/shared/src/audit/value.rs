//! Structured audit argument values with structural secret exclusion
//! (REQ-S-017) and capture-time redaction/truncation (REQ-S-018).
//!
//! [`AuditValue`] is the only type that can appear inside an audit record's
//! `args`. Its variants deliberately cannot hold raw key material: bytes are
//! representable only as a SHA-256 [`AuditValue::Digest`] reference, and a
//! known secret can enter the log only via [`redact`], which yields an
//! [`AuditValue::Redacted`] marker. There is no `Bytes(Vec<u8>)` or similar
//! live-key variant.

use std::collections::BTreeMap;
use std::fmt;

use serde::{Deserialize, Serialize};

use crate::audit::sha256_hex;

/// Per-field size cap (32 KiB) from ADR-0008. Values whose UTF-8 length exceeds
/// this are replaced by an [`AuditValue::Truncated`] marker by
/// [`truncate_field`].
pub const MAX_FIELD_BYTES: usize = 32 * 1024;

/// Minimum original length, in bytes, for [`redact`] to attach a correlation
/// digest.
///
/// A redaction digest is a *reversible* correlation aid: for a low-entropy
/// secret an unsalted SHA-256 is brute-forceable, so its digest would be a
/// plaintext-recoverable preimage. The Session Approval Code is only ~6 decimal
/// digits (~20 bits), so hashing the 10^6 candidates recovers it in
/// milliseconds. [`redact`] therefore withholds the digest for inputs shorter
/// than this floor, and callers holding a known low-entropy secret should use
/// [`redact_no_digest`] regardless (REQ-S-017).
pub const MIN_DIGEST_LEN: usize = 16;

/// A single structured audit value.
///
/// Serialized externally tagged (`{"string":"..."}`, `{"redacted":{...}}`),
/// which round-trips cleanly and keeps each variant's shape explicit. Nested
/// containers use `BTreeMap`, so `serde_json` output is deterministic — a
/// prerequisite for the hash chain.
///
/// # Secret exclusion (REQ-S-017)
///
/// No variant carries raw secret bytes. Opaque bytes are recorded only as a
/// [`AuditValue::Digest`] (hash + length); a value known to be sensitive is
/// recorded only as [`AuditValue::Redacted`] via [`redact`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuditValue {
    /// A plain, non-sensitive string (already within [`MAX_FIELD_BYTES`]).
    String(String),
    /// A signed integer (counts, exit codes, sizes, durations).
    Int(i64),
    /// A boolean flag.
    Bool(bool),
    /// A reference to opaque bytes by SHA-256 digest and length. The bytes
    /// themselves are never stored — this is how a file or output excerpt is
    /// attested without being reproduced.
    Digest {
        /// Lowercase-hex SHA-256 of the referenced bytes.
        sha256: String,
        /// Length in bytes of the referenced content.
        len: usize,
    },
    /// A value redacted at capture time. Renders as `[REDACTED]`.
    Redacted {
        /// Human-readable reason (e.g. `"command output may contain secrets"`).
        reason: String,
        /// Optional lowercase-hex SHA-256 of the original bytes, to correlate
        /// occurrences without revealing the value. Present only for
        /// high-entropy inputs (>= [`MIN_DIGEST_LEN`]); omitted for
        /// low-entropy secrets whose digest would be brute-forceable.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        sha256: Option<String>,
        /// Byte length of the original (pre-redaction) value.
        original_len: usize,
    },
    /// A value truncated to [`MAX_FIELD_BYTES`]; keeps a prefix and records the
    /// original length.
    Truncated {
        /// The retained UTF-8 prefix (on a char boundary, `<= MAX_FIELD_BYTES`).
        kept: String,
        /// Byte length of the original (pre-truncation) value.
        original_len: usize,
    },
    /// A nested, ordered map of named values.
    Map(BTreeMap<String, AuditValue>),
    /// A nested list of values.
    List(Vec<AuditValue>),
}

impl AuditValue {
    /// Convenience constructor for a non-sensitive string value that also
    /// enforces the [`MAX_FIELD_BYTES`] cap (delegates to [`truncate_field`]).
    #[must_use]
    pub fn text(value: &str) -> Self {
        truncate_field(value)
    }
}

impl fmt::Display for AuditValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::String(s) => f.write_str(s),
            Self::Int(n) => write!(f, "{n}"),
            Self::Bool(b) => write!(f, "{b}"),
            Self::Digest { sha256, len } => write!(f, "[DIGEST sha256={sha256} len={len}]"),
            Self::Redacted { .. } => f.write_str("[REDACTED]"),
            Self::Truncated { kept, original_len } => {
                write!(f, "{kept}[TRUNCATED original_len={original_len}]")
            }
            Self::Map(_) => f.write_str("{…}"),
            Self::List(_) => f.write_str("[…]"),
        }
    }
}

/// Replaces a known-sensitive value with an [`AuditValue::Redacted`] marker at
/// capture time (REQ-S-018).
///
/// This is the *only* way for a caller holding a secret to reference it in the
/// audit log: the raw bytes are dropped and replaced by the reason, the byte
/// length, and — **only for inputs of at least [`MIN_DIGEST_LEN`] bytes** — a
/// correlating SHA-256 of the original. The digest lets a reviewer tell whether
/// two redactions covered the same value without ever exposing it; the value
/// itself never reaches the log.
///
/// The digest is withheld for short inputs because an unsalted SHA-256 over a
/// low-entropy value (the Session Approval Code, a PIN) is brute-forceable and
/// would effectively disclose it (REQ-S-017). A caller that *knows* it holds a
/// low-entropy secret should use [`redact_no_digest`] so no digest is emitted
/// regardless of length.
#[must_use]
pub fn redact(reason: impl Into<String>, original: &[u8]) -> AuditValue {
    // Withhold the correlation digest below the entropy floor: for a value
    // drawn from a small space, the digest is a recoverable preimage.
    let sha256 = if original.len() >= MIN_DIGEST_LEN {
        Some(sha256_hex(original))
    } else {
        None
    };
    AuditValue::Redacted {
        reason: reason.into(),
        sha256,
        original_len: original.len(),
    }
}

/// Replaces a known low-entropy secret with an [`AuditValue::Redacted`] marker
/// that carries **no** correlation digest (REQ-S-017).
///
/// Use this whenever the secret's value space is small enough that an unsalted
/// SHA-256 would be brute-forceable — the Session Approval Code, a PIN, a short
/// token. Only the reason and the original length are recorded, so nothing in
/// the log can be turned back into the value. The caller passes `original_len`
/// rather than the bytes, so the secret need never be materialized here.
#[must_use]
pub fn redact_no_digest(reason: impl Into<String>, original_len: usize) -> AuditValue {
    AuditValue::Redacted {
        reason: reason.into(),
        sha256: None,
        original_len,
    }
}

/// Enforces the [`MAX_FIELD_BYTES`] cap on a string field.
///
/// Returns [`AuditValue::String`] unchanged when the value fits, or an
/// [`AuditValue::Truncated`] marker keeping a prefix (cut on a UTF-8 char
/// boundary) and the original byte length when it does not.
#[must_use]
pub fn truncate_field(value: &str) -> AuditValue {
    let original_len = value.len();
    if original_len <= MAX_FIELD_BYTES {
        return AuditValue::String(value.to_string());
    }
    // Back off to the nearest char boundary at or below the cap so we never
    // split a multi-byte code point.
    let mut end = MAX_FIELD_BYTES;
    while end > 0 && !value.is_char_boundary(end) {
        end -= 1;
    }
    AuditValue::Truncated {
        kept: value[..end].to_string(),
        original_len,
    }
}

/// The `args` payload of an audit record: an ordered map of named
/// [`AuditValue`]s.
///
/// Serialized transparently as a JSON object. The `BTreeMap` backing keeps keys
/// sorted so the serialized bytes — and therefore the chain hashes — are
/// deterministic.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct AuditArgs(pub BTreeMap<String, AuditValue>);

impl AuditArgs {
    /// An empty argument set.
    #[must_use]
    pub fn new() -> Self {
        Self(BTreeMap::new())
    }

    /// Inserts `value` under `key`, returning `self` for chaining.
    #[must_use]
    pub fn with(mut self, key: impl Into<String>, value: AuditValue) -> Self {
        self.0.insert(key.into(), value);
        self
    }

    /// Inserts `value` under `key` in place.
    pub fn insert(&mut self, key: impl Into<String>, value: AuditValue) {
        self.0.insert(key.into(), value);
    }

    /// Whether there are no arguments.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl FromIterator<(String, AuditValue)> for AuditArgs {
    fn from_iter<I: IntoIterator<Item = (String, AuditValue)>>(iter: I) -> Self {
        Self(iter.into_iter().collect())
    }
}
