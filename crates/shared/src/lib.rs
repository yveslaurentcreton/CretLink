//! Cross-cutting CretLink utilities.
//!
//! Holds small building blocks shared by the console, support agent, MCP
//! bridge, and simulator: identifier types, time handling, redaction helpers,
//! and audit primitives (REQ-S-017, REQ-N-009). Anything protocol-shaped
//! belongs in `cretlink-protocol` instead.

pub mod audit;
mod version;

pub use audit::{
    AUDIT_SCHEMA_VERSION, Actor, AuditArgs, AuditError, AuditFileMeta, AuditRecord, AuditValue,
    AuditWriter, ChainBreak, ChainReport, Clock, DEFAULT_SESSION_SOFT_CAP_BYTES, FixedClock,
    MAX_FIELD_BYTES, MIN_DIGEST_LEN, Outcome, RetentionPolicy, SystemClock, ZERO_PREV,
    files_to_prune, format_rfc3339_millis, redact, redact_no_digest, session_file_name,
    truncate_field, verify_chain, verify_sealed,
};
pub use version::RELEASE_VERSION;
