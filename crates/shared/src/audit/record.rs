//! The audit record schema, version 1 (ADR-0008).
//!
//! One [`AuditRecord`] is serialized as exactly one JSON object per line. The
//! field set is fixed and `deny_unknown_fields`: adding a field is a schema
//! change requiring a version bump, never a silent extension.

use std::collections::BTreeMap;

use cretlink_protocol::SessionId;
use serde::{Deserialize, Serialize};

use crate::audit::value::AuditArgs;

/// The audit record schema version written into every record's `v` field.
pub const AUDIT_SCHEMA_VERSION: u16 = 1;

/// The `prev` value used by the session-header record: 64 zero hex characters
/// (a zeroed SHA-256), marking the start of the chain.
pub const ZERO_PREV: &str = "0000000000000000000000000000000000000000000000000000000000000000";

/// Who originated an audited action.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Actor {
    /// The MCP bridge acting on the AI agent's behalf.
    Mcp,
    /// The technician operating the console UI directly.
    Console,
    /// The customer-side support agent.
    Agent,
    /// The system itself (session lifecycle: header, seal, internal errors).
    System,
    /// The technician operating an interactive remote terminal (ST-M003-009).
    ///
    /// Recorded for technician-terminal actions (`term_open`, `term_close`
    /// and the console's terminal IPC events, ST-M003-009). Added additively;
    /// the serialized form of earlier variants is unchanged (see the frozen
    /// golden-chain vectors in `tests.rs`).
    Terminal,
}

/// The result of an audited action.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    /// Completed successfully.
    Ok,
    /// Attempted and failed.
    Failed,
    /// Cancelled before completion.
    Cancelled,
    /// Timed out.
    Timeout,
    /// Outcome could not be determined (e.g. after a disconnect). Never report
    /// an unknown result as success (AGENTS.md).
    Unknown,
}

/// A single audit record (schema v1).
///
/// Field order matters only for human readability; `serde_json` emits struct
/// fields in declaration order, and every map field is a `BTreeMap`, so the
/// serialized bytes are deterministic and the hash chain is reproducible.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AuditRecord {
    /// Schema version. Always [`AUDIT_SCHEMA_VERSION`] for records this module
    /// writes.
    pub v: u16,
    /// Monotonic per-session sequence number, starting at 0 for the header.
    pub seq: u64,
    /// RFC 3339 UTC timestamp with millisecond precision.
    pub ts: String,
    /// The session this record belongs to.
    pub session: SessionId,
    /// Which side originated the action.
    pub actor: Actor,
    /// Stable event name, e.g. `discovered`, `pairing_started`,
    /// `code_confirmed`, `consent_granted`, `state_changed`, `revoked`,
    /// `error`, `cleanup`, `session_opened`, `session_closed`.
    pub action: String,
    /// Structured, redaction-aware arguments. Never carries a live secret.
    pub args: AuditArgs,
    /// The result of the action.
    pub outcome: Outcome,
    /// Optional named SHA-256 digests (e.g. transferred-file or output
    /// digests). Omitted from the serialized line when empty.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub digests: BTreeMap<String, String>,
    /// Optional wall-clock duration of the action in milliseconds. Omitted when
    /// absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
    /// SHA-256 (lowercase hex) of the previous line's exact serialized bytes.
    /// The header record uses [`ZERO_PREV`].
    pub prev: String,
}
