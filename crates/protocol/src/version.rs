//! Protocol version and the major/minor negotiation rule (REQ-N-012).

use core::fmt;

use serde::{Deserialize, Serialize};

use crate::error::ProtocolError;

/// The protocol version this crate implements.
///
/// Minor `1` adds the additive file-operation and transfer schema (ADR-0012),
/// advertised by the [`crate::CAP_FS_V1`] capability. A peer that does not
/// advertise it answers file operations with [`crate::ErrorCode::Unsupported`],
/// so same-major peers on different minors interoperate (REQ-N-012).
///
/// Minor `2` makes the `exec_started` result report the **effective working
/// directory** the job actually received, and subjects `Exec.working_dir` to
/// the same adversarial path rules as absolute remote paths (ST-M003-003,
/// REQ-F-009).
///
/// Minor `3` adds the additive [`crate::Origin`] field on the request envelope
/// so a console→agent [`crate::Message::Request`] carries whether the AI (via
/// the MCP bridge) or the technician drives it (ST-M004-006, REQ-F-017). It
/// defaults to `console` and is omitted at that default, so a same-major peer
/// on an older minor still decodes every console-origin request (REQ-N-012).
///
/// Minor `4` adds the additive interactive-terminal operations (`TermOpen`,
/// `TermResize`, `TermClose` and the `TermOpened` result), advertised by the
/// [`crate::CAP_TERM_V1`] capability (ADR-0013, ST-M003-009). A terminal is a
/// job: its merged output reuses [`crate::EventKind::JobOutput`] and its exit
/// reuses [`crate::EventKind::JobExited`]. An agent that does not advertise
/// [`crate::CAP_TERM_V1`] answers these operations with
/// [`crate::ErrorCode::Unsupported`], so same-major peers on different minors
/// interoperate (REQ-N-012).
///
/// Minor `5` adds the additive reboot-resume arming reflection messages
/// ([`crate::ControlMessage::RebootResumeArmed`],
/// [`crate::ControlMessage::RebootResumeDisarmed`]), advertised by the
/// [`crate::CAP_REBOOT_RESUME_V1`] capability (ADR-0014, ST-M006-001). They
/// carry no key material — only the single-use ticket id, the bound target
/// identity, and the expiry — so both endpoints can show that a reboot-resume
/// ticket is armed and until when. They are agent→console reflections of an
/// attended, local arming gesture; the console cannot arm remotely. A peer that
/// does not advertise the capability never sends or expects them, so same-major
/// peers on different minors interoperate (REQ-N-012).
///
/// Minor `6` adds the additive interactive-desktop operations
/// ([`crate::Operation::DesktopCapture`], `DesktopPointer`, `DesktopScroll`,
/// `DesktopKey`, `DesktopType`, `DesktopHotkey`, `DesktopWait`, `ClipboardGet`,
/// `ClipboardSet`) with the [`crate::OpResult::DesktopFrame`] and
/// [`crate::OpResult::ClipboardContents`] results, advertised by the
/// [`crate::CAP_DESKTOP_V1`] capability (ADR-0015, ST-M005-001 through
/// ST-M005-003). A capture reports honest desktop state
/// ([`crate::DesktopState`]) and never fabricates a frame for a locked,
/// switched, disconnected, or secure desktop. An agent that does not advertise
/// [`crate::CAP_DESKTOP_V1`] answers these operations with
/// [`crate::ErrorCode::Unsupported`], so same-major peers on different minors
/// interoperate (REQ-N-012).
///
/// Minor `7` adds the additive [`crate::EventKind::DesktopActivity`] event,
/// which the agent streams after it observes or controls the interactive
/// desktop so the console reflects the same observation/control state the
/// customer sees (ADR-0015, ST-M005-005). It carries the
/// [`crate::DesktopAccess`] class only — never a captured pixel, keystroke, or
/// clipboard text — and rides the [`crate::CAP_DESKTOP_V1`] capability: an agent
/// that does not advertise desktop support never emits it, and a console on an
/// older minor ignores an event it does not model, so same-major peers on
/// different minors interoperate (REQ-N-012).
///
/// Minor `8` adds the additive fieldless [`crate::ControlMessage::ApprovalCompareConfirmed`]
/// control frame, which the console sends the agent after the technician has
/// confirmed — during a humanly-perceptible pause — that the Session Approval
/// Code matches on both endpoints, before any full-control consent is requested
/// (REQ-S-006, ST-M002-013). It carries no key material and no approval code:
/// the code is compared out of band, and this frame only signals that the
/// human comparison passed so the agent may leave its compare-wait and present
/// the consent decision. Because [`crate::ControlMessage`] rejects unknown
/// variants, an agent on an older minor that receives it fails the session
/// closed rather than silently skipping the gate — so the compare step cannot
/// be downgraded away, and same-major peers that both speak minor 8 interoperate
/// (REQ-S-008, REQ-S-009, REQ-N-012).
///
/// Minor `9` adds the additive, optional [`crate::Purpose`] field on
/// [`crate::Message::Request`] and [`crate::Message::Cancel`] — the AI client's
/// stated, plain-language reason for the action, shown to the attended customer
/// beside the real operation (REQ-F-026, ST-M009-007) — and the additive
/// [`crate::Origin`] on `Cancel`. Both are omitted on the wire at their
/// defaults (no purpose, console origin), so a request or cancellation without
/// a purpose keeps its minor-8 wire form and an older-minor decoder still
/// accepts it (REQ-N-012). A purpose itself only ever reaches a minor-9 agent:
/// the pairing prologue ([`crate::pairing_prologue`]) commits both endpoints to
/// the identical version, so a console and an agent on different minors never
/// reach the request phase. The agent never treats a purpose as authority. It
/// is validated on decode (trimmed, single line, 1 to
/// [`crate::limits::MAX_PURPOSE_CHARS`] characters), so a malformed one fails
/// the frame closed like any other out-of-contract field.
///
/// Minor `10` adds the additive, optional [`crate::DisplayName`] on the
/// console's [`crate::PairingMessage::Nonce`]: the name the technician's
/// operating-system account reports, which the customer side shows as what the
/// technician's computer says (ST-M009-011). It is an unverified label, not an
/// identity: it grants no authority and is not part of the Session Approval
/// Code derivation, which still alone establishes trust (REQ-S-006). It is
/// omitted when `None`, so a nonce without a name keeps its minor-9 wire form,
/// and validated on decode (trimmed, single line, 1 to
/// [`crate::limits::MAX_DISPLAY_NAME_CHARS`] characters). The same minor is the
/// IPC contract minor at which the MCP bridge passes the AI client's
/// `clientInfo.name` to the console; that label stays on the technician
/// computer and is never sent to the customer computer.
///
/// Minor `11` adds the agent-to-console [`crate::ControlMessage::CleanupReported`]:
/// the agent's honest teardown report, sent while the channel is still up at
/// the end of an active session, so the console can record what was cleaned up
/// or left behind on the customer computer in the session's audit file
/// (ST-M009-012, REQ-F-019). Its notes are bounded in number and size and must
/// be single lines; a console that receives none records the cleanup as
/// unknown. The same minor adds the optional `account` of
/// [`crate::TargetIdentity`]: the account the agent's process runs as
/// (`DOMAIN\user`, [`crate::AccountName`]), the customer's own account name,
/// shown to the technician in the live identity line (ST-M009-008). It is
/// omitted when unknown, so an identity without it keeps its minor-10 wire
/// form, and validated on decode (trimmed, single line, 1 to
/// [`crate::limits::MAX_ACCOUNT_NAME_CHARS`] characters).
///
/// Minor `12` adds the additive file operations a file explorer needs
/// (ST-M010-001, ADR-0012), advertised by the [`crate::CAP_FS_V2`] capability.
/// [`crate::Operation::FsDrives`] is answered [`crate::OpResult::Drives`]: each
/// drive of the target with its root, kind, label and total and free space,
/// where a value the target cannot read is absent rather than invented.
/// [`crate::Operation::RemoveTree`] is answered [`crate::OpResult::TreeRemoval`]
/// and removes a folder with everything in it **one bounded step at a time**
/// (at most [`crate::limits::MAX_TREE_STEP_ENTRIES`] entries and about
/// [`crate::limits::MAX_TREE_STEP_MS`] milliseconds per request): the caller
/// repeats the request while the answer is [`crate::TreeState::More`], the
/// agent keeps no state between two steps, and so a stop, a revocation or a
/// cancellation takes effect between them. One removal is about one folder:
/// an answer that says more remains carries a `folder_id` (at most
/// [`crate::limits::MAX_FOLDER_ID_BYTES`] bytes) that the next request gives
/// back, and a step that finds another folder at the path, or a path that
/// leads to the folder through a link, removes nothing. A step never leaves
/// the folder — a link is removed as a link and never walked — and it names
/// the first entries it could not remove with their reasons; only
/// [`crate::TreeState::Removed`] is a success (REQ-N-008). An agent that does
/// not advertise [`crate::CAP_FS_V2`] answers both operations with
/// [`crate::ErrorCode::Unsupported`] before it lists or removes anything
/// (REQ-N-012); a peer on an older minor does not get as far as a request,
/// because the pairing prologue commits both endpoints to the identical
/// version. Sending a file to a folder on any drive needs no addition:
/// `FsWrite` and `FsCommit` take an absolute destination since minor `1`, and
/// the part of such a transfer lies beside its destination, so the verified
/// rename never crosses a volume.
pub const PROTOCOL_V1: ProtocolVersion = ProtocolVersion {
    major: 1,
    minor: 12,
};

/// A `major.minor` protocol version.
///
/// Equal majors are compatible; minors signal additive schema evolution.
/// Because every message rejects unknown fields, any schema addition
/// requires a minor bump (see the crate-level compatibility policy).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProtocolVersion {
    /// Incompatible-change counter; peers must match exactly.
    pub major: u16,
    /// Additive-change counter; peers may differ.
    pub minor: u16,
}

impl ProtocolVersion {
    /// Creates a version from its parts.
    #[must_use]
    pub const fn new(major: u16, minor: u16) -> Self {
        Self { major, minor }
    }

    /// Two versions are compatible when their majors are equal.
    #[must_use]
    pub const fn is_compatible_with(self, other: Self) -> bool {
        self.major == other.major
    }

    /// Negotiates against the version a peer offered in `Hello`.
    ///
    /// On success the responder answers with its **own** version (so the
    /// minor in `HelloAck` is the responder's, not the offerer's). On a
    /// major mismatch this fails with
    /// [`ProtocolError::IncompatibleVersion`], which maps to
    /// [`crate::ErrorCode::IncompatibleVersion`].
    pub fn negotiate(self, offered: Self) -> Result<Self, ProtocolError> {
        if self.is_compatible_with(offered) {
            Ok(self)
        } else {
            Err(ProtocolError::IncompatibleVersion {
                local: self,
                peer: offered,
            })
        }
    }
}

impl fmt::Display for ProtocolVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}", self.major, self.minor)
    }
}
