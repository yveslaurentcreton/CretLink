//! Folds a live [`SessionEvent`] stream into the agent UI state machine
//! (ST-M002-009).
//!
//! [`crate::state::AgentState`] is the customer-side pure model with hand-checked
//! security invariants; the real transport in `cretlink-session` emits a
//! [`SessionEvent`] stream from the agent's [`cretlink_session::JoinHandle`].
//! This module is the single, reusable, cross-platform seam that turns each
//! observed transport event into the one legal state transition, so a real
//! session and the rendered agent model can no longer drift (the mirror of the
//! console's `driver` module, ADR-0011).
//!
//! # What it deliberately does *not* do
//!
//! - It never fabricates control. [`AgentPhase::FullControl`](crate::state::AgentPhase::FullControl) is reached only by
//!   folding a real [`SessionEvent::ConsentGranted`] — the customer's attended
//!   consent — through [`AgentState::grant_consent`] (REQ-S-001, REQ-S-003). No
//!   event short-circuits that path.
//! - It never adopts an identity from the wire. The agent presents the honest
//!   [`crate::privilege::EffectiveIdentity`] it captured at startup (REQ-S-011),
//!   so the [`TargetIdentity`](cretlink_protocol::TargetIdentity) carried by
//!   [`SessionEvent::ConsentGranted`] — which is the agent's own identity echoed
//!   back by the console — is deliberately ignored here. (This is the exact
//!   mirror of the console adapter, which *does* adopt that identity, because on
//!   the console side the wire identity is the remote target's honest
//!   self-report.)
//! - It is fail-closed. An event that cannot be applied from the current phase
//!   leaves the state untouched and surfaces a [`DriverError`]; it never forces
//!   an illegal transition (REQ-S-009).
//! - It stays out of command execution. Post-consent job activity
//!   ([`SessionEvent::CommandStarted`] and friends) is [`EventOutcome::Ignored`]
//!   here — those belong to the agent's command broker, not the
//!   session-lifecycle UI.
//!
//! The driver assumes the customer has already launched the agent — that is,
//! [`AgentState::announce`] has been called so the model is
//! [`AgentPhase::Announced`](crate::state::AgentPhase::Announced) — before events are pumped, mirroring how the
//! console adapter assumes the operator has dialed
//! ([`cretlink_console`](crate)-side `begin_connect`). The wire
//! [`SessionEvent::Connected`] then drives the pairing handshake, not the
//! initial announce, which is a local action and not a transport event.

use cretlink_protocol::{RevokeReason, SessionId};
use cretlink_session::SessionEvent;
use thiserror::Error;

use crate::state::{AgentError, AgentState};

/// The out-of-band value an event references but does not itself carry.
///
/// The negotiated session id is read straight off the live `JoinHandle` by the
/// driver task via its `session()` accessor (bound before
/// [`SessionEvent::ConsentGranted`] is emitted). Passing it in keeps this
/// adapter pure and unit testable without a transport.
#[derive(Debug, Clone, Copy, Default)]
pub struct EventContext {
    /// The negotiated session id, required to bind control at
    /// [`SessionEvent::ConsentGranted`].
    pub session: Option<SessionId>,
}

impl EventContext {
    /// A context carrying the session id (for the consent event).
    #[must_use]
    pub fn with_session(session: SessionId) -> Self {
        Self {
            session: Some(session),
        }
    }
}

/// What the driver should do after an event was folded into the state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventOutcome {
    /// The event advanced the live session but control is not (yet) active;
    /// keep pumping and re-render.
    Progressed,
    /// Full control is now active in the model; the agent may enable the
    /// control-bound surface.
    ControlActive,
    /// The session reached a terminal phase (revoked or failed); stop pumping.
    Terminated,
    /// The event has no agent session-lifecycle meaning at this layer
    /// (post-consent job or reboot-resume activity) and was ignored on purpose.
    Ignored,
}

/// A refused fold. Every variant is safe to show or log: it names the event and
/// a reason, and carries no secrets (REQ-N-015).
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum DriverError {
    /// [`SessionEvent::ConsentGranted`] arrived without a bound session id.
    /// Control is never granted without a session to bind it to (REQ-F-016), so
    /// the fold fails closed and the state is left untouched.
    #[error("consent event carried no session id to bind control to")]
    MissingSessionId,
    /// The event could not be applied from the current phase. The state is left
    /// exactly as it was (fail closed, REQ-S-009).
    #[error("cannot apply the {event} event: {source}")]
    Rejected {
        /// A short static name for the event that was refused.
        event: &'static str,
        /// The underlying state-machine rejection.
        source: AgentError,
    },
}

impl DriverError {
    fn rejected(event: &'static str, source: AgentError) -> Self {
        Self::Rejected { event, source }
    }
}

/// Folds one observed [`SessionEvent`] into `state`, performing the single legal
/// transition it implies and reporting what the driver should do next.
///
/// The agent-side event order (verified against the `cretlink-session` initiator
/// runtime) is `Connected → AwaitingConsent → ConsentGranted → Active`, ending
/// in exactly one of `Revoked` / `Failed`. This function assumes the customer
/// has already launched and announced the agent ([`AgentState::announce`])
/// before pumping events.
///
/// # Errors
///
/// Returns [`DriverError`] when the event cannot be folded — a missing session
/// id in `ctx`, or a transition the current phase refuses. In every error case
/// the state is left unchanged, so a malformed or out-of-order stream can never
/// move the agent into an unsafe phase.
pub fn apply_session_event(
    state: &mut AgentState,
    event: &SessionEvent,
    ctx: EventContext,
) -> Result<EventOutcome, DriverError> {
    match event {
        SessionEvent::Connected => {
            // The transport is up; the SAC pairing handshake now runs. On the
            // agent this is the announced -> pairing step (the initial announce
            // is a local launch action, not a wire event).
            state
                .begin_pairing()
                .map_err(|e| DriverError::rejected("connected", e))?;
            Ok(EventOutcome::Progressed)
        }
        SessionEvent::AwaitingApprovalCompare => {
            // Pairing is done and the Session Approval Code is on screen; hold in
            // the comparison window (still no control) until the technician
            // confirms the codes match (REQ-S-006, ST-M002-013).
            state
                .begin_compare()
                .map_err(|e| DriverError::rejected("awaiting_approval_compare", e))?;
            Ok(EventOutcome::Progressed)
        }
        SessionEvent::AwaitingConsent => {
            // The technician confirmed the codes match; only now is the customer
            // asked to grant control (still no control until they do).
            state
                .await_consent()
                .map_err(|e| DriverError::rejected("awaiting_consent", e))?;
            Ok(EventOutcome::Progressed)
        }
        SessionEvent::ConsentGranted { identity: _ } => {
            // The wire identity is the agent's own, echoed by the console; the
            // agent presents its startup-captured honest identity instead
            // (REQ-S-011), so it is deliberately not adopted here.
            let session = ctx.session.ok_or(DriverError::MissingSessionId)?;
            state
                .grant_consent(session)
                .map_err(|e| DriverError::rejected("consent_granted", e))?;
            Ok(EventOutcome::Progressed)
        }
        SessionEvent::Active => {
            // A confirmation of the control that consent already produced. If the
            // model is not under full control here the stream was inconsistent;
            // fail closed rather than pretend control is live.
            if state.control_granted() {
                Ok(EventOutcome::ControlActive)
            } else {
                Err(DriverError::rejected(
                    "active",
                    AgentError::InvalidTransition {
                        action: "activate",
                        phase: state.phase().label(),
                    },
                ))
            }
        }
        SessionEvent::Revoked { reason } => {
            // Revocation is a fail-closed teardown. The agent's stop methods are
            // idempotent and never restore control, so applying one to an
            // already-terminal agent keeps its first outcome.
            match reason {
                RevokeReason::TechnicianRequested => state.remote_stop(),
                RevokeReason::CustomerRequested => state.stop(),
                RevokeReason::Expired | RevokeReason::Error => state.disconnect(),
            }
            Ok(EventOutcome::Terminated)
        }
        SessionEvent::Failed { stage, message } => {
            state.fail(stage.clone(), message.clone());
            Ok(EventOutcome::Terminated)
        }
        // Desktop observation/control happens *within* granted control and does
        // not move the consent phase; the customer's live activity indicator
        // consumes this signal straight from the event stream (ST-M005-005),
        // so folding it through the phase machine would misread within-control
        // activity as a transition. Control stays exactly as consent left it.
        SessionEvent::DesktopActivity { .. } => Ok(EventOutcome::Ignored),
        // Post-consent job and reboot-resume activity is surfaced by the agent's
        // command broker, not the session-lifecycle UI, so it never drives a
        // phase transition.
        SessionEvent::CommandStarted { .. }
        | SessionEvent::ExecOutput { .. }
        | SessionEvent::ExecOutputDropped { .. }
        | SessionEvent::ExecFinished { .. }
        | SessionEvent::RebootResumeArmed { .. }
        | SessionEvent::RebootResumeDisarmed { .. } => Ok(EventOutcome::Ignored),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::privilege::{EffectiveIdentity, IntegrityLevel};
    use crate::state::{AgentPhase, StopReason};
    use cretlink_protocol::TargetIdentity;

    fn test_identity() -> EffectiveIdentity {
        EffectiveIdentity {
            username: "tester".to_string(),
            domain: None,
            elevated: false,
            integrity: IntegrityLevel::Medium,
            is_windows: false,
            note: None,
        }
    }

    /// The wire identity carried by `ConsentGranted` — the agent's own identity
    /// echoed back by the console. The adapter must ignore it.
    fn wire_identity() -> TargetIdentity {
        TargetIdentity {
            hostname: "CUSTOMER-PC".to_string(),
            os_version: "Windows 11".to_string(),
            arch: "x86_64".to_string(),
            elevated: false,
            simulated: false,
            agent_version: "0.0.0".to_string(),
        }
    }

    /// An agent that has launched and announced — the state the driver is in
    /// once the customer has started it and the event pump begins.
    fn announced_agent() -> AgentState {
        let mut agent = AgentState::new(test_identity());
        agent.announce().unwrap();
        agent
    }

    fn consent_event() -> SessionEvent {
        SessionEvent::ConsentGranted {
            identity: wire_identity(),
        }
    }

    fn drive_to_full_control(agent: &mut AgentState, session: SessionId) {
        let ctx = EventContext::with_session(session);
        apply_session_event(agent, &SessionEvent::Connected, ctx).unwrap();
        apply_session_event(agent, &SessionEvent::AwaitingApprovalCompare, ctx).unwrap();
        apply_session_event(agent, &SessionEvent::AwaitingConsent, ctx).unwrap();
        apply_session_event(agent, &consent_event(), ctx).unwrap();
    }

    #[test]
    fn happy_path_transcript_reaches_full_control() {
        let mut agent = announced_agent();
        let session = SessionId::new();
        let ctx = EventContext::with_session(session);

        assert_eq!(
            apply_session_event(&mut agent, &SessionEvent::Connected, ctx).unwrap(),
            EventOutcome::Progressed
        );
        assert_eq!(agent.phase(), &AgentPhase::Pairing);
        assert!(!agent.control_granted());

        // The compare gate comes first: the customer reads the approval code and
        // no consent is offered until the technician confirms it matches.
        assert_eq!(
            apply_session_event(&mut agent, &SessionEvent::AwaitingApprovalCompare, ctx).unwrap(),
            EventOutcome::Progressed
        );
        assert_eq!(agent.phase(), &AgentPhase::AwaitingApprovalCompare);
        assert!(!agent.control_granted());
        assert!(agent.session().is_none());

        assert_eq!(
            apply_session_event(&mut agent, &SessionEvent::AwaitingConsent, ctx).unwrap(),
            EventOutcome::Progressed
        );
        assert_eq!(agent.phase(), &AgentPhase::AwaitingConsent);
        // Still no control before consent.
        assert!(!agent.control_granted());
        assert!(agent.session().is_none());

        assert_eq!(
            apply_session_event(&mut agent, &consent_event(), ctx).unwrap(),
            EventOutcome::Progressed
        );
        assert_eq!(agent.phase(), &AgentPhase::FullControl { session });

        assert_eq!(
            apply_session_event(&mut agent, &SessionEvent::Active, ctx).unwrap(),
            EventOutcome::ControlActive
        );

        // Control is live, bound to the real session, and the agent still
        // reports its OWN honest identity, never the wire identity.
        assert!(agent.control_granted());
        assert!(agent.can_execute());
        assert_eq!(agent.session(), Some(session));
        assert_eq!(agent.identity(), &test_identity());
        assert_ne!(agent.identity().username, wire_identity().hostname);
    }

    #[test]
    fn revoke_tears_down_fail_closed() {
        let mut agent = announced_agent();
        let session = SessionId::new();
        drive_to_full_control(&mut agent, session);
        assert!(agent.control_granted());

        let outcome = apply_session_event(
            &mut agent,
            &SessionEvent::Revoked {
                reason: RevokeReason::CustomerRequested,
            },
            EventContext::default(),
        )
        .unwrap();
        assert_eq!(outcome, EventOutcome::Terminated);
        assert_eq!(
            agent.phase(),
            &AgentPhase::Stopped {
                reason: StopReason::CustomerRequested
            }
        );
        assert!(!agent.control_granted());
        assert!(agent.session().is_none());
    }

    #[test]
    fn failed_event_is_terminal_and_grants_no_control() {
        let mut agent = announced_agent();
        apply_session_event(
            &mut agent,
            &SessionEvent::Connected,
            EventContext::default(),
        )
        .unwrap();

        let outcome = apply_session_event(
            &mut agent,
            &SessionEvent::Failed {
                stage: "pairing".to_string(),
                message: "secure pairing failed".to_string(),
            },
            EventContext::default(),
        )
        .unwrap();
        assert_eq!(outcome, EventOutcome::Terminated);
        assert!(matches!(agent.phase(), AgentPhase::Failed { .. }));
        assert!(!agent.control_granted());
    }

    #[test]
    fn consent_without_session_id_fails_closed() {
        let mut agent = announced_agent();
        apply_session_event(
            &mut agent,
            &SessionEvent::Connected,
            EventContext::default(),
        )
        .unwrap();
        apply_session_event(
            &mut agent,
            &SessionEvent::AwaitingApprovalCompare,
            EventContext::default(),
        )
        .unwrap();
        apply_session_event(
            &mut agent,
            &SessionEvent::AwaitingConsent,
            EventContext::default(),
        )
        .unwrap();

        let err =
            apply_session_event(&mut agent, &consent_event(), EventContext::default()).unwrap_err();
        assert_eq!(err, DriverError::MissingSessionId);
        // No control was granted; the phase is unchanged.
        assert_eq!(agent.phase(), &AgentPhase::AwaitingConsent);
        assert!(!agent.control_granted());
    }

    #[test]
    fn consent_before_pairing_is_rejected() {
        // ConsentGranted straight out of Announced (no pairing / awaiting-consent
        // step) must be refused, leaving the state exactly as it was.
        let mut agent = announced_agent();
        let err = apply_session_event(
            &mut agent,
            &consent_event(),
            EventContext::with_session(SessionId::new()),
        )
        .unwrap_err();
        assert!(matches!(err, DriverError::Rejected { .. }));
        assert_eq!(agent.phase(), &AgentPhase::Announced);
        assert!(!agent.control_granted());
    }

    #[test]
    fn connected_before_announce_is_rejected() {
        // The pump must not run before the customer launches the agent: a
        // Connected event from Idle is refused (fail closed).
        let mut agent = AgentState::new(test_identity());
        let err = apply_session_event(
            &mut agent,
            &SessionEvent::Connected,
            EventContext::default(),
        )
        .unwrap_err();
        assert!(matches!(err, DriverError::Rejected { .. }));
        assert_eq!(agent.phase(), &AgentPhase::Idle);
    }

    #[test]
    fn active_before_full_control_is_rejected() {
        let mut agent = announced_agent();
        let err = apply_session_event(&mut agent, &SessionEvent::Active, EventContext::default())
            .unwrap_err();
        assert!(matches!(err, DriverError::Rejected { .. }));
        assert!(!agent.control_granted());
    }

    #[test]
    fn activity_events_are_ignored_and_change_nothing() {
        let mut agent = announced_agent();
        let session = SessionId::new();
        drive_to_full_control(&mut agent, session);
        let before = agent.phase().clone();

        for event in [
            SessionEvent::CommandStarted {
                job: cretlink_protocol::JobId(1),
                shell: cretlink_protocol::Shell::PowerShell,
                command: "Get-Process".to_string(),
                origin: cretlink_protocol::Origin::Console,
            },
            SessionEvent::ExecOutputDropped {
                job: cretlink_protocol::JobId(2),
                bytes: 42,
            },
        ] {
            assert_eq!(
                apply_session_event(&mut agent, &event, EventContext::default()).unwrap(),
                EventOutcome::Ignored
            );
            assert_eq!(
                agent.phase(),
                &before,
                "activity events must not mutate the model"
            );
            // Control is untouched: still live and bound to the same session.
            assert_eq!(agent.session(), Some(session));
        }
    }

    #[test]
    fn revoke_reason_maps_each_variant_to_its_stop() {
        for (reason, expected) in [
            (
                RevokeReason::TechnicianRequested,
                StopReason::TechnicianRequested,
            ),
            (
                RevokeReason::CustomerRequested,
                StopReason::CustomerRequested,
            ),
            (RevokeReason::Expired, StopReason::Disconnected),
            (RevokeReason::Error, StopReason::Disconnected),
        ] {
            let mut agent = announced_agent();
            drive_to_full_control(&mut agent, SessionId::new());
            let outcome = apply_session_event(
                &mut agent,
                &SessionEvent::Revoked { reason },
                EventContext::default(),
            )
            .unwrap();
            assert_eq!(outcome, EventOutcome::Terminated);
            assert_eq!(agent.phase(), &AgentPhase::Stopped { reason: expected });
        }
    }
}
