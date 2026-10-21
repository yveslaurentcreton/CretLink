//! Agent-side reboot-resume arming surface (ADR-0014, ST-M006-001).
//!
//! Reboot-resume lets a technician continue **one** authorized session across a
//! single customer reboot without re-running the whole pairing dance — while
//! never restoring control from stored state (ADR-0014). The cryptographic
//! authority (key derivation, the single-use expiring target-bound vault, the
//! at-rest sealer) lives in [`cretlink_protocol::reboot_resume`]. This module is
//! the thin runtime seam the session task drives: a [`RebootResumeArmer`] the
//! agent injects, the [`ArmCommand`] a local gesture routes to the session task,
//! and the clear ticket metadata both endpoints reflect.
//!
//! # Off by default, attended-only
//!
//! [`SessionJoiner::join`](crate::SessionJoiner::join) and its audited/configured
//! variants install [`NoRebootResume`], which refuses every arm — so an agent
//! that never opts in cannot arm anything. A real armer is injected only through
//! [`SessionJoiner::join_with_reboot_resume`](crate::SessionJoiner::join_with_reboot_resume),
//! and the runtime honors an arm request only inside a live, consented session.
//! Arming is always driven by an attended **local** gesture on the agent
//! ([`JoinHandle::arm_reboot_resume`](crate::JoinHandle::arm_reboot_resume)); it
//! is never reachable from the console or the MCP surface (REQ-S-001, REQ-S-003).

use core::fmt;
use std::time::Duration;

use cretlink_protocol::{REBOOT_RESUME_TICKET_ID_LEN, TargetIdentity};

/// A single-use reboot-resume ticket id (clear mirror): the 16-byte identifier
/// bound into the continuity proof (ADR-0014).
///
/// The session layer only carries and reflects this value; it never treats it as
/// a security decision. The authoritative copy is sealed at rest inside the
/// [`cretlink_protocol::RebootResumeVault`].
pub type RebootResumeTicketId = [u8; REBOOT_RESUME_TICKET_ID_LEN];

/// The outcome of arming one reboot-resume ticket.
///
/// It carries **no key material** — only the clear ticket id both endpoints
/// reflect and the expiry an operator surface renders as "armed until".
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArmOutcome {
    /// The freshly minted single-use ticket id, reflected to both endpoints.
    pub ticket_id: RebootResumeTicketId,
    /// Expiry as Unix milliseconds (UTC).
    pub not_after_unix_millis: i64,
}

/// Why arming a reboot-resume ticket failed.
///
/// Deliberately coarse: the agent never leaks *why* to the peer (a refused arm
/// is simply not reflected). This granularity exists for honest **local**
/// reporting only.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum RebootResumeArmError {
    /// Reboot-resume is not available on this agent (off by default, ADR-0014):
    /// no armer was injected, so nothing can be armed.
    Unavailable,
    /// The at-rest sealing backend refused, or the armed ticket could not be
    /// persisted to the owned-resource inventory.
    Backend,
}

impl fmt::Display for RebootResumeArmError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unavailable => f.write_str("reboot-resume is not available on this agent"),
            Self::Backend => f.write_str("reboot-resume at-rest sealing or persistence failed"),
        }
    }
}

impl std::error::Error for RebootResumeArmError {}

/// Arms and disarms single-use reboot-resume tickets on the agent side
/// (ADR-0014).
///
/// The runtime calls [`arm`](RebootResumeArmer::arm) only inside a live,
/// consented session, passing the armed session's Noise handshake hash `h` (kept
/// local to the session task and never stored on shared state) and the honest
/// consent-bound [`TargetIdentity`]. An implementation derives `K_rr` from `h`
/// and fresh nonces, seals it at rest behind an
/// [`AtRestSealer`](cretlink_protocol::AtRestSealer), persists the resulting
/// [`ArmedTicket`](cretlink_protocol::ArmedTicket) to the owned-resource
/// inventory (ST-M006-003), and returns the clear ticket id and expiry.
///
/// Arming by itself grants no access and never restores control: attended
/// full-control consent (REQ-F-006) still runs after any post-reboot reconnect.
pub trait RebootResumeArmer: Send + Sync + fmt::Debug {
    /// Arms a single-use ticket for `target`, live for `lifetime` from now.
    ///
    /// # Errors
    ///
    /// [`RebootResumeArmError::Unavailable`] when reboot-resume is off, or
    /// [`RebootResumeArmError::Backend`] when sealing or persistence fails.
    fn arm(
        &self,
        handshake_hash: &[u8; 32],
        target: &TargetIdentity,
        lifetime: Duration,
    ) -> Result<ArmOutcome, RebootResumeArmError>;

    /// Disarms a previously armed ticket (arm-then-cancel), returning whether a
    /// record was present. Leaves no ticket behind.
    fn disarm(&self, ticket_id: &RebootResumeTicketId) -> bool;
}

/// The default armer: reboot-resume **off**, every arm refused (ADR-0014).
///
/// Installed by the standard [`SessionJoiner`](crate::SessionJoiner) entry
/// points so an agent that never opts in cannot arm continuity state.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoRebootResume;

impl RebootResumeArmer for NoRebootResume {
    fn arm(
        &self,
        _handshake_hash: &[u8; 32],
        _target: &TargetIdentity,
        _lifetime: Duration,
    ) -> Result<ArmOutcome, RebootResumeArmError> {
        Err(RebootResumeArmError::Unavailable)
    }

    fn disarm(&self, _ticket_id: &RebootResumeTicketId) -> bool {
        false
    }
}

/// A local arming gesture routed to the agent session task over an unbounded
/// channel (matching the consent/revoke command channels).
///
/// Only the attended agent-side handle produces these; the console and MCP
/// surface never can (REQ-S-001, REQ-S-003).
pub(crate) enum ArmCommand {
    /// Arm a single-use ticket, live for `lifetime` from now.
    Arm {
        /// How long the armed ticket stays redeemable.
        lifetime: Duration,
    },
    /// Disarm a previously armed ticket before reboot.
    Disarm {
        /// The ticket id to disarm.
        ticket_id: RebootResumeTicketId,
    },
}
