//! Post-reboot reboot-resume redemption runtime (ADR-0014 §3, ST-M006-002).
//!
//! In the reboot-resume design, after a single customer reboot an OS
//! auto-start entry relaunches the agent, which dials the console and runs a
//! **fresh** Noise XX handshake — yielding a fresh transcript hash `h'` that
//! neither side could predict before the reboot. Over that fresh encrypted
//! channel the two endpoints run one continuity challenge/response:
//!
//! 1. the console sends [`ControlMessage::RebootResumeChallenge`] with a fresh
//!    32-byte nonce `Nc'`;
//! 2. the returning agent answers [`ControlMessage::RebootResumeProof`] with the
//!    ticket id and `HMAC-SHA256(K_rr, h' ‖ ticket_id ‖ Nc')`, produced by
//!    [`RebootResumeVault::prove`](cretlink_protocol::RebootResumeVault::prove)
//!    against its own sealed copy; and
//! 3. the console verifies that proof **constant-time** against its own sealed
//!    copy via
//!    [`RebootResumeVault::redeem`](cretlink_protocol::RebootResumeVault::redeem).
//!
//! This module is the transport-generic driver pair that runs steps 1–3 over a
//! pure in-memory [`ControlMessage`] transport with an **injectable clock**, so
//! the whole fail-closed matrix is exercised without hardware. Nothing in this
//! codebase binds the driver into a live post-reboot dial or registers the
//! per-user OS auto-start entry; the end-to-end reboot scenario is manual test
//! MT-8 in `docs/manual-tests.md`.
//!
//! # Redemption never restores control (REQ-S-001, REQ-S-003, ADR-0014)
//!
//! Proving continuity only **authenticates the returning peer**; it grants no
//! capability. [`RedemptionOutcome::ContinuityProven`] carries the honest
//! [`TargetIdentity`] and nothing else — no channel, no session authority — and
//! [`RedemptionOutcome::grants_control`] is *always* `false`. Attended
//! full-control consent (REQ-F-006) still runs afterward on the fresh channel,
//! exactly as it does for a first connection.
//!
//! # Fail-closed toward the peer (REQ-S-007 … REQ-S-009)
//!
//! Every way redemption can fail — an unknown/already-spent ticket, an expired
//! ticket, a target mismatch, a blob that will not unseal in this environment, a
//! proof that does not verify, a malformed or absent proof, or a dropped
//! transport — collapses to a single [`RedemptionOutcome::NoConsentedSession`]
//! toward the peer. The specific [`RedemptionRefusal`] is retained for honest
//! **local** audit only; it never travels the wire, so a caller can never learn
//! which check failed or observe a partial success.

use cretlink_protocol::{AtRestSealer, B64Bytes};
use cretlink_protocol::{
    ControlMessage, REBOOT_RESUME_NONCE_LEN, RebootResumeRefusal, RebootResumeVault, TargetBinding,
    TargetIdentity, TicketId,
};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};

use crate::reboot::RebootResumeTicketId;

/// Samples the current wall clock as Unix milliseconds (UTC), injected so the
/// redemption drivers read a **live** yet deterministic "now" under test.
///
/// Production wires the real clock; the fail-closed matrix injects fixed values
/// to drive the expiry edges from either side independently (ADR-0014 §3).
pub type RedemptionClock = std::sync::Arc<dyn Fn() -> i64 + Send + Sync>;

/// Why a redemption did not authenticate the returning peer, retained for honest
/// **local** audit only (ADR-0014).
///
/// This granularity never reaches the peer: [`console_redeem`] maps every
/// variant to one [`RedemptionOutcome::NoConsentedSession`] on the wire, so the
/// returning side can never learn which check failed (REQ-S-007 … REQ-S-009).
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum RedemptionRefusal {
    /// The authoritative vault refused: unknown/spent ticket, expired, target
    /// mismatch, unseal failure, or a proof that did not verify.
    Vault(RebootResumeRefusal),
    /// The returning peer sent no proof before the transport closed — a refusing
    /// or interrupted agent. Fails closed exactly like an invalid proof.
    NoProof,
    /// The peer named a ticket id of the wrong length, so it cannot address any
    /// armed record. Rejected before the vault is consulted.
    MalformedTicketId,
    /// The peer sent an unexpected control message instead of a proof. The
    /// redemption exchange admits no other traffic; fail closed.
    UnexpectedMessage,
}

impl core::fmt::Display for RedemptionRefusal {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Vault(inner) => write!(f, "reboot-resume vault refused: {inner}"),
            Self::NoProof => f.write_str("no reboot-resume proof before the transport closed"),
            Self::MalformedTicketId => {
                f.write_str("reboot-resume proof named a malformed ticket id")
            }
            Self::UnexpectedMessage => {
                f.write_str("unexpected control message during reboot-resume redemption")
            }
        }
    }
}

/// The outcome of a console-side redemption, presented to the local caller.
///
/// Toward the peer only two shapes exist — proven or not — so a returning side
/// can never observe *why* a redemption failed. Locally the caller learns the
/// specific [`RedemptionRefusal`] for honest audit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RedemptionOutcome {
    /// The returning peer's continuity proof verified: the peer is the same
    /// agent that armed the ticket, on the same target.
    ///
    /// This authenticates the peer and **nothing more** — no control is granted
    /// or restored (ADR-0014). The caller must still run attended full-control
    /// consent (REQ-F-006) on the fresh channel before any command surface
    /// exists.
    ContinuityProven {
        /// The honest target the ticket was bound to, recovered from the sealed
        /// (authenticated) copy via the verified proof.
        target: TargetIdentity,
    },
    /// Redemption did not authenticate the returning peer. Toward the peer this
    /// is the sole failure shape; `local_reason` is for local audit only and
    /// never travels the wire.
    NoConsentedSession {
        /// The specific refusal, for honest local reporting (never sent).
        local_reason: RedemptionRefusal,
    },
}

impl RedemptionOutcome {
    /// Whether this outcome grants or restores any control capability.
    ///
    /// **Always `false`.** Reboot-resume authenticates continuity; it never
    /// restores `control_active` (ADR-0014, REQ-S-001). Encoded as a method so
    /// the invariant is asserted directly in the fail-closed matrix.
    #[must_use]
    pub fn grants_control(&self) -> bool {
        false
    }

    /// Whether the returning peer's continuity proof verified.
    #[must_use]
    pub fn is_proven(&self) -> bool {
        matches!(self, Self::ContinuityProven { .. })
    }
}

/// The outcome of the agent-side answer, for honest local reporting.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AnswerOutcome {
    /// The agent produced and sent a continuity proof for the challenge. Sending
    /// a proof restores no control on the agent either.
    Proved,
    /// The agent refused to answer — no armed ticket, expired, or a blob that
    /// would not unseal in this environment (e.g. a sealed record lifted to a
    /// foreign machine). No proof was sent; the console fails closed.
    Refused {
        /// The specific vault refusal, for honest local reporting (never sent).
        local_reason: RebootResumeRefusal,
    },
    /// The transport closed before a challenge arrived.
    NoChallenge,
}

/// Runs the **console** side of one post-reboot redemption over an in-memory
/// [`ControlMessage`] transport (ADR-0014 §3, ST-M006-002).
///
/// Sends [`ControlMessage::RebootResumeChallenge`] with `nc_prime`, awaits the
/// returning agent's [`ControlMessage::RebootResumeProof`], and verifies it
/// constant-time against `vault` for the presented `target` and the fresh
/// transcript hash `h_prime`. `now` is sampled once, at verification, from the
/// injected [`RedemptionClock`], so the console enforces expiry from its own
/// sealed copy regardless of what the peer claims.
///
/// The `tx`/`rx` halves are taken **by value**: when the returning agent refuses
/// and drops its sender, this driver observes the closed channel and fails
/// closed as [`RedemptionRefusal::NoProof`] rather than hanging.
///
/// Every failure — vault refusal, malformed/absent proof, unexpected traffic —
/// collapses to [`RedemptionOutcome::NoConsentedSession`] toward the peer.
pub async fn console_redeem<S: AtRestSealer>(
    vault: &mut RebootResumeVault<S>,
    tx: UnboundedSender<ControlMessage>,
    mut rx: UnboundedReceiver<ControlMessage>,
    target: &TargetIdentity,
    h_prime: &[u8; REBOOT_RESUME_NONCE_LEN],
    nc_prime: &[u8; REBOOT_RESUME_NONCE_LEN],
    now: &RedemptionClock,
) -> RedemptionOutcome {
    // Step 1: challenge the returning peer with a fresh nonce. A send failure
    // means the fresh channel is already gone — fail closed.
    if tx
        .send(ControlMessage::RebootResumeChallenge {
            nc_prime: B64Bytes(nc_prime.to_vec()),
        })
        .is_err()
    {
        return RedemptionOutcome::NoConsentedSession {
            local_reason: RedemptionRefusal::NoProof,
        };
    }

    // Step 2: await exactly one proof. A closed channel (refusing/interrupted
    // agent) or any other message fails closed.
    let (ticket_bytes, proof) = match rx.recv().await {
        Some(ControlMessage::RebootResumeProof { ticket_id, proof }) => (ticket_id, proof),
        Some(_) => {
            return RedemptionOutcome::NoConsentedSession {
                local_reason: RedemptionRefusal::UnexpectedMessage,
            };
        }
        None => {
            return RedemptionOutcome::NoConsentedSession {
                local_reason: RedemptionRefusal::NoProof,
            };
        }
    };

    let ticket_id = match RebootResumeTicketId::try_from(ticket_bytes.as_bytes()) {
        Ok(bytes) => TicketId(bytes),
        Err(_) => {
            return RedemptionOutcome::NoConsentedSession {
                local_reason: RedemptionRefusal::MalformedTicketId,
            };
        }
    };

    // Step 3: authoritative, constant-time verification against our own sealed
    // copy. `redeem` consumes the record first (single use) whatever the result.
    match vault.redeem(
        &ticket_id,
        (now)(),
        &TargetBinding::of(target),
        h_prime,
        nc_prime,
        proof.as_bytes(),
    ) {
        Ok(()) => RedemptionOutcome::ContinuityProven {
            target: target.clone(),
        },
        Err(refusal) => RedemptionOutcome::NoConsentedSession {
            local_reason: RedemptionRefusal::Vault(refusal),
        },
    }
}

/// Runs the **agent** side of one post-reboot redemption: await the console's
/// challenge and answer with a single-use continuity proof (ADR-0014 §3).
///
/// Awaits [`ControlMessage::RebootResumeChallenge`], then produces the proof via
/// [`RebootResumeVault::prove`](cretlink_protocol::RebootResumeVault::prove) for
/// the agent's own `ticket_id` and the fresh transcript hash `h_prime`, sampling
/// `now` from the injected clock. On success it sends
/// [`ControlMessage::RebootResumeProof`]; on **any** vault refusal it sends
/// nothing and returns [`AnswerOutcome::Refused`], so a stolen or stale sealed
/// blob yields no usable proof and the console fails closed. Producing a proof
/// restores no control on the agent either — attended consent still runs.
///
/// `tx` is taken by value so that a refusal drops the sender, letting the
/// console observe the closed channel deterministically instead of hanging.
pub async fn agent_answer_challenge<S: AtRestSealer>(
    vault: &mut RebootResumeVault<S>,
    tx: UnboundedSender<ControlMessage>,
    mut rx: UnboundedReceiver<ControlMessage>,
    ticket_id: &RebootResumeTicketId,
    h_prime: &[u8; REBOOT_RESUME_NONCE_LEN],
    now: &RedemptionClock,
) -> AnswerOutcome {
    let nc_prime = match rx.recv().await {
        Some(ControlMessage::RebootResumeChallenge { nc_prime }) => nc_prime,
        // Anything other than the expected challenge — including a closed
        // channel — means there is nothing to answer. Fail closed silently.
        _ => return AnswerOutcome::NoChallenge,
    };

    let nc_prime: [u8; REBOOT_RESUME_NONCE_LEN] = match nc_prime.as_bytes().try_into() {
        Ok(bytes) => bytes,
        // A malformed challenge nonce is answered with silence: no proof exists
        // for a nonce we cannot even parse. The console fails closed on no proof.
        Err(_) => return AnswerOutcome::NoChallenge,
    };

    match vault.prove(&TicketId(*ticket_id), (now)(), h_prime, &nc_prime) {
        Ok(proof) => {
            // A closed console channel is indistinguishable from a refusal to the
            // caller: either way no control is granted.
            let _ = tx.send(ControlMessage::RebootResumeProof {
                ticket_id: B64Bytes(ticket_id.to_vec()),
                proof: B64Bytes(proof.as_bytes().to_vec()),
            });
            AnswerOutcome::Proved
        }
        Err(refusal) => AnswerOutcome::Refused {
            local_reason: refusal,
        },
    }
}

#[cfg(test)]
mod tests;
