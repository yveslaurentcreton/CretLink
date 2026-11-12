//! Versioned CretLink protocol schemas (ST-M001-003).
//!
//! This crate defines the session, request, response, event, error,
//! cancellation, and capability message types exchanged between the console
//! and the support agent (real or simulated), together with their size
//! limits, identifier rules, compatibility policy, and unknown-field policy
//! (REQ-N-012).
//!
//! The crate is transport-agnostic: length-prefix framing and the Noise
//! secure channel live in `cretlink-transport` (M002, ADR-0003). This crate
//! contributes the transcript-binding pieces of pairing that both endpoints
//! must compute identically: the frozen [`pairing_prologue`] and the
//! commit-reveal Session Approval Code round ([`PairingMessage`],
//! [`sac_commitment`], [`derive_session_approval_code`]). It invents no
//! cryptography — those helpers compose SHA-256 and HKDF-SHA256 exactly as
//! ADR-0003 specifies (ST-M002-004, REQ-S-005 through REQ-S-008).
//! Wire-encoding decisions are recorded in ADR-0010.
//!
//! # Wire encoding (ADR-0010)
//!
//! Control frames are JSON produced by [`Message::encode`] and parsed by
//! [`Message::decode`]. Binary payloads (job output, stdin data) travel as
//! base64 strings via [`B64Bytes`] and are capped at
//! [`limits::MAX_CHUNK_BYTES`]. File transfer reuses these bounded chunks in
//! the commit-gated request/response schema (ADR-0012, [`CAP_FS_V1`]) rather
//! than adding dedicated binary frames. The drives of the target and the
//! removal of a folder with its content follow the same bounded
//! request/response model ([`CAP_FS_V2`], ST-M010-001).
//!
//! # Compatibility policy (REQ-N-012)
//!
//! - Every message and struct rejects unknown fields
//!   (`deny_unknown_fields`). Adding a field is therefore a schema change
//!   that requires a **minor** version bump, never a silent extension.
//! - Two peers are compatible when their **major** versions are equal
//!   ([`ProtocolVersion::is_compatible_with`]). The responder answers
//!   `Hello` with its **own** minor version in `HelloAck`
//!   ([`ProtocolVersion::negotiate`]); peers must tolerate a different
//!   minor from a same-major peer.
//! - Removing or renaming a field, variant, or semantic is a **major**
//!   bump and breaks compatibility by design.
//!
//! # Bounded inputs
//!
//! [`Message::validate`] enforces every limit in [`limits`];
//! [`Message::decode`] checks the frame size **before** parsing and
//! validates after parsing, so oversized or out-of-bounds input fails
//! closed with a mapped [`ErrorCode`].
//!
//! # Terminal output
//!
//! [`EventKind::JobOutput`] carries raw output bytes. ConPTY virtual
//! terminal (VT) escape sequences are passed through unmodified (ADR-0010);
//! consumers render or strip them. The simulator emits plain text. Output
//! bytes are untrusted input from the remote target.

mod account;
mod b64;
mod display_name;
mod error;
mod ids;
pub mod limits;
mod message;
mod pairing;
mod purpose;
mod reboot_resume;
mod version;

pub use account::{AccountName, AccountNameError};
pub use b64::B64Bytes;
pub use display_name::{DisplayName, DisplayNameError};
pub use error::ProtocolError;
pub use ids::{JobId, RequestId, SessionId};
pub use message::{
    CleanupFailure, CleanupPolicy, CleanupTrigger, ControlMessage, DesktopAccess, DesktopFrameMeta,
    DesktopState, DigestAlgo, DriveEntry, DriveKind, ErrorCode, ErrorStage, EventKind, FileEntry,
    FileKind, ImageFormat, JobOutcome, JobSummary, KeyAction, Message, OpResult, Operation, Origin,
    OutputStream, PairingMessage, PointerAction, PointerButton, RemotePath, RevokeReason, Role,
    Shell, StagedEntry, TargetIdentity, TreeFailure, TreeState, validate_remote_absolute_path,
};
pub use pairing::{
    PAIRING_PROLOGUE_MAGIC, SAC_COMMITMENT_LEN, SAC_DIGITS, SAC_HKDF_SALT, SAC_MODULUS,
    SAC_NONCE_LEN, SessionApprovalCode, derive_session_approval_code, pairing_prologue,
    sac_commitment,
};
pub use purpose::{Purpose, PurposeError};
pub use reboot_resume::{
    ArmedTicket, AtRestSealer, REBOOT_RESUME_HKDF_SALT, REBOOT_RESUME_KEY_LEN,
    REBOOT_RESUME_NONCE_LEN, REBOOT_RESUME_PROOF_LEN, REBOOT_RESUME_TARGET_LEN,
    REBOOT_RESUME_TARGET_MAGIC, REBOOT_RESUME_TICKET_ID_LEN, RebootResumeKey, RebootResumeProof,
    RebootResumeRefusal, RebootResumeVault, SealError, TargetBinding, TicketId,
    derive_reboot_resume_key,
};
pub use version::{PROTOCOL_V1, ProtocolVersion};

/// Capability string advertising the additive file-operation and transfer
/// schema (ADR-0012, protocol minor `1`).
///
/// An agent lists this in its `Hello` capabilities when it can service the
/// [`Operation`] file variants (`FsStat`, `FsList`, `FsRead`, `FsWrite`,
/// `FsCommit`, and the rest). A console must not issue those operations to a
/// peer that omits it; a peer that receives one anyway answers
/// [`ErrorCode::Unsupported`], so same-major peers on different minors
/// interoperate (REQ-N-012).
pub const CAP_FS_V1: &str = "fs.v1";

/// Capability string advertising the additive interactive-terminal operations
/// (ADR-0013, protocol minor `4`).
///
/// An agent lists this in its `Hello` capabilities when it can service the
/// terminal [`Operation`] variants (`TermOpen`, `TermResize`, `TermClose`). A
/// peer that receives one without advertising it answers
/// [`ErrorCode::Unsupported`], so same-major peers on different minors
/// interoperate (REQ-N-012). Unlike [`CAP_FS_V1`], this gate is **enforced** by
/// the agent runtime (`cretlink-session`), not merely advertised.
pub const CAP_TERM_V1: &str = "term.v1";

/// Capability string advertising the additive reboot-resume arming reflection
/// (ADR-0014, protocol minor `5`).
///
/// An agent lists this in its `Hello` capabilities when it can arm a single-use
/// reboot-resume ticket and reflect that armed state to the console via
/// [`ControlMessage::RebootResumeArmed`] / [`ControlMessage::RebootResumeDisarmed`].
/// The messages carry no key material — only the ticket id, bound target
/// identity, and expiry (REQ-S-018). Arming itself is granted only by an
/// attended, local gesture on the agent; it is never enabled from the console or
/// the MCP surface (REQ-S-001, REQ-S-003). A peer that omits this capability
/// never sends or expects the reflection messages, so same-major peers on
/// different minors interoperate (REQ-N-012).
pub const CAP_REBOOT_RESUME_V1: &str = "reboot_resume.v1";

/// Capability string advertising the additive interactive-desktop operations
/// (ADR-0015, protocol minor `6`).
///
/// An agent lists this in its `Hello` capabilities when it can service the
/// desktop [`Operation`] variants (`DesktopCapture`, `DesktopPointer`,
/// `DesktopScroll`, `DesktopKey`, `DesktopType`, `DesktopHotkey`,
/// `DesktopWait`, `ClipboardGet`, `ClipboardSet`). A peer that receives one
/// without advertising it answers [`ErrorCode::Unsupported`], so same-major
/// peers on different minors interoperate (REQ-N-012). Like [`CAP_TERM_V1`],
/// this gate is **enforced** by the agent runtime (`cretlink-session`): capture
/// and input are refused unless the capability was advertised, and the agent
/// only advertises it on a platform whose capture and input backend is present
/// (REQ-S-002, REQ-S-009).
pub const CAP_DESKTOP_V1: &str = "desktop.v1";

/// Capability string advertising the additive file operations a file explorer
/// needs (ADR-0012, ST-M010-001, protocol minor `12`).
///
/// An agent lists this in its served capabilities when it can service
/// [`Operation::FsDrives`] and [`Operation::RemoveTree`]. A peer that receives
/// one of them without advertising it answers [`ErrorCode::Unsupported`], so
/// same-major peers on different minors interoperate (REQ-N-012). Like
/// [`CAP_TERM_V1`], this gate is **enforced** by the agent runtime
/// (`cretlink-session`): nothing is listed and nothing is removed unless the
/// capability was advertised. It adds to [`CAP_FS_V1`] and does not replace it;
/// an agent that serves this one serves that one too.
pub const CAP_FS_V2: &str = "fs.v2";

/// The frozen, version-canonical capability set that **both** endpoints hash
/// into the pairing prologue ([`pairing_prologue`]).
///
/// The prologue commits both peers to an *identical* capability set; a mismatch
/// is treated as a silent downgrade and fails the Noise handshake closed
/// (REQ-S-008). That tripwire only distinguishes an attack from an honest
/// session when both endpoints compute the **same** set — so the set fed into
/// the prologue must be a pure function of the negotiated protocol version, not
/// a host-detected subset. The console serves no operations and an agent serves
/// a host-dependent subset (a machine without a desktop backend omits
/// [`CAP_DESKTOP_V1`], and so on); if each hashed *its own* served set into the
/// prologue, two honest peers would present different sets and could never pair
/// (ST-M002-008).
///
/// Per-host availability is not lost: the agent keeps its host-detected served
/// set locally and enforces it per operation, answering [`ErrorCode::Unsupported`]
/// for anything this host does not serve. That enforcement rides the encrypted,
/// integrity-protected channel — it does not belong in the prologue, whose only
/// job is to bind the version and pattern both peers agreed to.
///
/// The returned set contains every capability defined up to `version.minor`
/// within protocol major `1` (each capability was added by exactly one minor:
/// [`CAP_FS_V1`] at `1`, [`CAP_TERM_V1`] at `4`, [`CAP_REBOOT_RESUME_V1`] at
/// `5`, [`CAP_DESKTOP_V1`] at `6`, [`CAP_FS_V2`] at `12`). Because a capability
/// maps one-to-one to a minor and the prologue also binds the full version,
/// this set never contradicts the version already committed there. Order is
/// the frozen order below; [`pairing_prologue`] additionally sorts and
/// dedupes, so order is not load-bearing.
#[must_use]
pub fn canonical_capabilities(version: ProtocolVersion) -> Vec<String> {
    // Capabilities are a major-1 concept; a future major would define its own
    // set and its own version constant. Guard on the major so a hypothetical
    // major-2 peer never inherits these tokens by minor number alone.
    if version.major != 1 {
        return Vec::new();
    }
    let mut caps = Vec::new();
    if version.minor >= 1 {
        caps.push(CAP_FS_V1.to_string());
    }
    if version.minor >= 4 {
        caps.push(CAP_TERM_V1.to_string());
    }
    if version.minor >= 5 {
        caps.push(CAP_REBOOT_RESUME_V1.to_string());
    }
    if version.minor >= 6 {
        caps.push(CAP_DESKTOP_V1.to_string());
    }
    if version.minor >= 12 {
        caps.push(CAP_FS_V2.to_string());
    }
    caps
}

#[cfg(test)]
mod tests;
