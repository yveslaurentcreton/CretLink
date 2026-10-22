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
pub const PROTOCOL_V1: ProtocolVersion = ProtocolVersion { major: 1, minor: 9 };

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
