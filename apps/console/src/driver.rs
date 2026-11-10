//! Folds a live [`SessionEvent`] stream into the console UI state machine
//! (ST-M002-009).
//!
//! [`crate::state::ConsoleState`] is a pure model with hand-checked security
//! invariants; the real transport in `cretlink-session` emits a
//! [`SessionEvent`] stream. This adapter is the single, reusable,
//! cross-platform seam that turns each observed transport event into the one
//! legal state transition, so a real session and the rendered model cannot
//! drift.
//!
//! # What it deliberately does *not* do
//!
//! - It never fabricates control. [`SessionPhase::FullControl`](crate::state::SessionPhase::FullControl) is reached only
//!   by folding a real [`SessionEvent::ConsentGranted`] — the customer's
//!   attended consent — through [`ConsoleState::grant_full_control`]
//!   (REQ-F-006, REQ-S-001). No event short-circuits that path.
//! - It is fail-closed. An event that cannot be applied from the current phase
//!   leaves the state untouched and surfaces a [`DriverError`]; it never forces
//!   an illegal transition (REQ-S-009).
//! - It stays out of command execution. Post-consent job activity
//!   ([`SessionEvent::CommandStarted`] and friends) is [`EventOutcome::Ignored`]
//!   here — those belong to the command loop and the MCP bridge, not the
//!   session-lifecycle UI.
//!
//! The operator's Session Approval Code comparison (REQ-F-005, REQ-S-006) is a
//! real, humanly-perceptible gate, not a cosmetic one (ST-M002-013). The runtime
//! emits [`SessionEvent::AwaitingApprovalCompare`] and then *pauses* — the
//! console's `AwaitingApprovalCode` phase is that window — until the technician
//! confirms the codes match, which releases the gate over the wire; only then
//! does [`SessionEvent::AwaitingConsent`] arrive and the model advance past its
//! approval-code phase. The GUI-level "codes match" affordance drives
//! `HostHandle::confirm_code`; it does not fabricate control, because no control
//! exists until [`SessionEvent::ConsentGranted`].

use cretlink_protocol::{RevokeReason, SessionId};
use cretlink_session::SessionEvent;
use thiserror::Error;

use crate::state::{ConsoleState, StateError, StopReason};

/// The out-of-band values an event references but does not itself carry.
///
/// Both are read straight off the live `HostHandle` by the driver task:
/// [`Self::approval_code`] from `approval_code()` (ready once
/// [`SessionEvent::Connected`] is observed) and [`Self::session`] from
/// `session()` (bound before [`SessionEvent::ConsentGranted`] is emitted, as the
/// console runtime adopts the agent's session id when it records
/// `ConsentRequested`). Passing them in keeps this adapter pure and unit
/// testable without a transport.
#[derive(Debug, Clone, Copy, Default)]
pub struct EventContext<'a> {
    /// The six-character Session Approval Code, required to render
    /// [`SessionEvent::Connected`].
    pub approval_code: Option<&'a str>,
    /// The negotiated session id, required to bind control at
    /// [`SessionEvent::ConsentGranted`].
    pub session: Option<SessionId>,
}

impl<'a> EventContext<'a> {
    /// A context carrying just the approval code (for the pairing events).
    #[must_use]
    pub fn with_approval_code(code: &'a str) -> Self {
        Self {
            approval_code: Some(code),
            session: None,
        }
    }

    /// A context carrying just the session id (for the consent event).
    #[must_use]
    pub fn with_session(session: SessionId) -> Self {
        Self {
            approval_code: None,
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
    /// Full control is now active in the model; the driver may enable the
    /// control-bound surface.
    ControlActive,
    /// The session reached a terminal phase (revoked or failed); stop pumping.
    Terminated,
    /// The event has no console session-lifecycle meaning at this layer
    /// (post-consent job or reboot-resume activity) and was ignored on purpose.
    Ignored,
}

/// A refused fold. Every variant is safe to show or log: it names the event and
/// a reason, and carries no secrets (REQ-N-015).
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum DriverError {
    /// [`SessionEvent::Connected`] arrived without a Session Approval Code in
    /// the context. The event guarantees the code is ready, so this is a caller
    /// wiring bug rather than a remote condition; the state is left untouched.
    #[error("connected event carried no approval code to present")]
    MissingApprovalCode,
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
        source: StateError,
    },
}

impl DriverError {
    fn rejected(event: &'static str, source: StateError) -> Self {
        Self::Rejected { event, source }
    }
}

/// Folds one observed [`SessionEvent`] into `state`, performing the single legal
/// transition it implies and reporting what the driver should do next.
///
/// The console-side event order (verified against the `cretlink-session`
/// responder runtime) is `Connected → AwaitingConsent → ConsentGranted →
/// Active`, ending in exactly one of `Revoked` / `Failed`. This function assumes
/// the driver has already taken the operator dial action
/// ([`ConsoleState::begin_connect`]) before pumping events, mirroring the CLI.
///
/// # Errors
///
/// Returns [`DriverError`] when the event cannot be folded — a missing approval
/// code or session id in `ctx`, or a transition the current phase refuses. In
/// every error case the state is left unchanged, so a malformed or out-of-order
/// stream can never move the console into an unsafe phase.
pub fn apply_session_event(
    state: &mut ConsoleState,
    event: &SessionEvent,
    ctx: EventContext<'_>,
) -> Result<EventOutcome, DriverError> {
    match event {
        SessionEvent::Connected => {
            let code = ctx.approval_code.ok_or(DriverError::MissingApprovalCode)?;
            state
                .present_approval_code(code)
                .map_err(|e| DriverError::rejected("connected", e))?;
            Ok(EventOutcome::Progressed)
        }
        SessionEvent::AwaitingApprovalCompare => {
            // The secure channel is up and the code is presented; the runtime has
            // paused here for the humanly-perceptible comparison window
            // (REQ-S-006, ST-M002-013). The console's `AwaitingApprovalCode`
            // phase *is* that window — it already shows the code and, in the GUI,
            // the "codes match" control that releases the gate via
            // `HostHandle::confirm_code`. No phase change is implied; we only
            // fail closed if the stream reached the compare window without the
            // code ever being presented (an out-of-order stream).
            if matches!(
                state.phase,
                crate::state::SessionPhase::AwaitingApprovalCode { .. }
            ) {
                Ok(EventOutcome::Progressed)
            } else {
                Err(DriverError::rejected(
                    "awaiting_approval_compare",
                    StateError::InvalidTransition {
                        action: "open the approval-compare window",
                        phase: "not showing the approval code",
                    },
                ))
            }
        }
        SessionEvent::AwaitingConsent => {
            // The wire says the comparison window has closed and the customer is
            // being asked; advance past the approval-code phase (no control).
            state
                .confirm_approval_code()
                .map_err(|e| DriverError::rejected("awaiting_consent", e))?;
            Ok(EventOutcome::Progressed)
        }
        SessionEvent::ConsentGranted { identity } => {
            let session = ctx.session.ok_or(DriverError::MissingSessionId)?;
            state
                .grant_full_control(session, identity.clone())
                .map_err(|e| DriverError::rejected("consent_granted", e))?;
            Ok(EventOutcome::Progressed)
        }
        SessionEvent::Active => {
            // A confirmation of the state consent already produced. If the model
            // is not under full control here the stream was inconsistent; fail
            // closed rather than pretend control is live.
            if matches!(state.phase, crate::state::SessionPhase::FullControl) {
                Ok(EventOutcome::ControlActive)
            } else {
                Err(DriverError::rejected(
                    "active",
                    StateError::InvalidTransition {
                        action: "activate",
                        phase: "not under full control",
                    },
                ))
            }
        }
        SessionEvent::Revoked { reason } => {
            // Revocation is a fail-closed teardown; a missing selection means
            // there is already no session to tear down, which is the same
            // terminal outcome. Either way no authority survives.
            let _ = state.stop(stop_reason_for(*reason));
            Ok(EventOutcome::Terminated)
        }
        SessionEvent::Failed { stage, message } => {
            state.fail(stage.clone(), message.clone());
            Ok(EventOutcome::Terminated)
        }
        // Desktop observation/control happens within full control and does not
        // move the console phase; the technician's live observation/control
        // display consumes this signal straight from the event stream
        // (ST-M005-005), so folding it through the phase machine would misread
        // within-control activity as a transition.
        SessionEvent::DesktopActivity { .. } => Ok(EventOutcome::Ignored),
        // Post-consent job and reboot-resume activity is surfaced by the command
        // loop and the MCP bridge, not the session-lifecycle UI, so it never
        // drives a phase transition.
        SessionEvent::CommandStarted { .. }
        | SessionEvent::RequestHandled { .. }
        | SessionEvent::ActivityShed { .. }
        | SessionEvent::ExecOutput { .. }
        | SessionEvent::ExecOutputDropped { .. }
        | SessionEvent::ExecFinished { .. }
        | SessionEvent::RebootResumeArmed { .. }
        | SessionEvent::RebootResumeDisarmed { .. } => Ok(EventOutcome::Ignored),
    }
}

/// Maps a wire [`RevokeReason`] onto the console's [`StopReason`].
///
/// The console's `StopReason` has no dedicated "expired" or "error" variant;
/// both are non-attended teardowns and are surfaced as a disconnect so the UI
/// shows an honest, fail-closed end rather than a fabricated clean stop. A
/// richer `StopReason` would be a separate UI-state change with its own story.
fn stop_reason_for(reason: RevokeReason) -> StopReason {
    match reason {
        RevokeReason::TechnicianRequested => StopReason::TechnicianRequested,
        RevokeReason::CustomerRequested => StopReason::CustomerRequested,
        RevokeReason::Expired | RevokeReason::Error => StopReason::Disconnected,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{
        ConsoleState, ControlLevel, DiscoveredTarget, DiscoverySource, SessionPhase, TargetId,
        TechnicianHost,
    };
    use cretlink_protocol::TargetIdentity;

    fn technician() -> TechnicianHost {
        TechnicianHost {
            label: "This computer (technician)".to_string(),
            hostname: "TECH-PC".to_string(),
            os: "Windows 11".to_string(),
            display_name: Some("Sam".to_string()),
        }
    }

    fn remote_identity() -> TargetIdentity {
        TargetIdentity {
            hostname: "CUSTOMER-PC".to_string(),
            os_version: "Windows 11".to_string(),
            arch: "x86_64".to_string(),
            elevated: true,
            simulated: false,
            agent_version: "0.0.0".to_string(),
            account: None,
        }
    }

    /// A console with one selected, connecting target — the state the driver is
    /// in once the operator has dialed and the event pump starts.
    fn connecting_console() -> ConsoleState {
        let mut state = ConsoleState::new(technician());
        let id = TargetId::new();
        state.upsert_target(DiscoveredTarget::new(
            id,
            "Customer laptop",
            "10.0.0.5:7000",
            DiscoverySource::Discovered,
        ));
        state.select(id).unwrap();
        state.begin_connect().unwrap();
        state
    }

    fn connected_ctx(session: SessionId) -> EventContext<'static> {
        EventContext {
            approval_code: Some("42-17"),
            session: Some(session),
        }
    }

    #[test]
    fn happy_path_transcript_reaches_full_control() {
        let mut state = connecting_console();
        let session = SessionId::new();
        let ctx = connected_ctx(session);

        assert_eq!(
            apply_session_event(&mut state, &SessionEvent::Connected, ctx).unwrap(),
            EventOutcome::Progressed
        );
        assert!(matches!(
            state.phase,
            SessionPhase::AwaitingApprovalCode { .. }
        ));

        assert_eq!(
            apply_session_event(&mut state, &SessionEvent::AwaitingConsent, ctx).unwrap(),
            EventOutcome::Progressed
        );
        assert_eq!(state.phase, SessionPhase::AwaitingConsent);
        // Still no control before consent.
        assert_eq!(state.control_level(), ControlLevel::Connected);
        assert!(!state.can_issue_actions());

        let granted = SessionEvent::ConsentGranted {
            identity: remote_identity(),
        };
        assert_eq!(
            apply_session_event(&mut state, &granted, ctx).unwrap(),
            EventOutcome::Progressed
        );
        assert_eq!(state.phase, SessionPhase::FullControl);

        assert_eq!(
            apply_session_event(&mut state, &SessionEvent::Active, ctx).unwrap(),
            EventOutcome::ControlActive
        );

        // Control is live, bound to the real session and honest remote identity.
        assert!(state.can_issue_actions());
        assert_eq!(state.session, Some(session));
        let target = state.selected_target().unwrap();
        assert_eq!(target.identity.as_ref().unwrap().hostname, "CUSTOMER-PC");
        assert_ne!(
            target.identity.as_ref().unwrap().hostname,
            state.technician.hostname
        );
    }

    #[test]
    fn revoke_tears_down_fail_closed() {
        let mut state = connecting_console();
        let session = SessionId::new();
        let ctx = connected_ctx(session);
        apply_session_event(&mut state, &SessionEvent::Connected, ctx).unwrap();
        apply_session_event(&mut state, &SessionEvent::AwaitingConsent, ctx).unwrap();
        apply_session_event(
            &mut state,
            &SessionEvent::ConsentGranted {
                identity: remote_identity(),
            },
            ctx,
        )
        .unwrap();
        assert!(state.can_issue_actions());

        let outcome = apply_session_event(
            &mut state,
            &SessionEvent::Revoked {
                reason: RevokeReason::CustomerRequested,
            },
            ctx,
        )
        .unwrap();
        assert_eq!(outcome, EventOutcome::Terminated);
        assert_eq!(
            state.phase,
            SessionPhase::Stopped {
                reason: StopReason::CustomerRequested
            }
        );
        assert!(state.session.is_none());
        assert!(!state.can_issue_actions());
    }

    #[test]
    fn failed_event_is_terminal_and_grants_no_control() {
        let mut state = connecting_console();
        let ctx = connected_ctx(SessionId::new());
        apply_session_event(&mut state, &SessionEvent::Connected, ctx).unwrap();

        let outcome = apply_session_event(
            &mut state,
            &SessionEvent::Failed {
                stage: "pairing".to_string(),
                message: "secure pairing failed".to_string(),
            },
            ctx,
        )
        .unwrap();
        assert_eq!(outcome, EventOutcome::Terminated);
        assert!(matches!(state.phase, SessionPhase::Failed { .. }));
        assert_eq!(state.control_level(), ControlLevel::None);
    }

    #[test]
    fn connected_without_code_fails_closed() {
        let mut state = connecting_console();
        let err = apply_session_event(
            &mut state,
            &SessionEvent::Connected,
            EventContext::default(),
        )
        .unwrap_err();
        assert_eq!(err, DriverError::MissingApprovalCode);
        // The state did not advance out of connecting.
        assert_eq!(state.phase, SessionPhase::Connecting);
    }

    #[test]
    fn consent_without_session_id_fails_closed() {
        let mut state = connecting_console();
        apply_session_event(
            &mut state,
            &SessionEvent::Connected,
            EventContext::with_approval_code("42-17"),
        )
        .unwrap();
        apply_session_event(
            &mut state,
            &SessionEvent::AwaitingConsent,
            EventContext::default(),
        )
        .unwrap();

        let err = apply_session_event(
            &mut state,
            &SessionEvent::ConsentGranted {
                identity: remote_identity(),
            },
            EventContext::default(),
        )
        .unwrap_err();
        assert_eq!(err, DriverError::MissingSessionId);
        // No control was granted; the phase is unchanged.
        assert_eq!(state.phase, SessionPhase::AwaitingConsent);
        assert_eq!(state.control_level(), ControlLevel::Connected);
    }

    #[test]
    fn consent_before_pairing_is_rejected() {
        // ConsentGranted straight out of Connecting (no approval-code phase)
        // must be refused, leaving the state exactly as it was.
        let mut state = connecting_console();
        let err = apply_session_event(
            &mut state,
            &SessionEvent::ConsentGranted {
                identity: remote_identity(),
            },
            connected_ctx(SessionId::new()),
        )
        .unwrap_err();
        assert!(matches!(err, DriverError::Rejected { .. }));
        assert_eq!(state.phase, SessionPhase::Connecting);
        assert_eq!(state.control_level(), ControlLevel::Connected);
    }

    #[test]
    fn active_before_full_control_is_rejected() {
        let mut state = connecting_console();
        let err = apply_session_event(&mut state, &SessionEvent::Active, EventContext::default())
            .unwrap_err();
        assert!(matches!(err, DriverError::Rejected { .. }));
        assert!(!state.can_issue_actions());
    }

    #[test]
    fn activity_events_are_ignored_and_change_nothing() {
        let mut state = connecting_console();
        let session = SessionId::new();
        let ctx = connected_ctx(session);
        apply_session_event(&mut state, &SessionEvent::Connected, ctx).unwrap();
        apply_session_event(&mut state, &SessionEvent::AwaitingConsent, ctx).unwrap();
        apply_session_event(
            &mut state,
            &SessionEvent::ConsentGranted {
                identity: remote_identity(),
            },
            ctx,
        )
        .unwrap();
        let before = state.clone();

        for event in [
            SessionEvent::CommandStarted {
                job: cretlink_protocol::JobId(1),
                shell: cretlink_protocol::Shell::PowerShell,
                command: "Get-Process".to_string(),
                origin: cretlink_protocol::Origin::Console,
                purpose: None,
            },
            SessionEvent::RequestHandled {
                id: 1,
                origin: cretlink_protocol::Origin::Mcp,
                purpose: cretlink_protocol::Purpose::new("Checking free disk space").ok(),
                kind: cretlink_session::ActivityKind::SystemInformation,
                operation: "read system information".to_string(),
                path: None,
                bytes: None,
                outcome: cretlink_session::ActionOutcome::Succeeded,
                duration_ms: Some(3),
            },
            SessionEvent::ActivityShed { count: 2 },
            SessionEvent::ExecOutputDropped {
                job: cretlink_protocol::JobId(2),
                bytes: 42,
            },
        ] {
            assert_eq!(
                apply_session_event(&mut state, &event, ctx).unwrap(),
                EventOutcome::Ignored
            );
            assert_eq!(state, before, "activity events must not mutate the model");
        }
    }

    #[test]
    fn revoke_reason_maps_expired_to_disconnected() {
        assert_eq!(
            stop_reason_for(RevokeReason::TechnicianRequested),
            StopReason::TechnicianRequested
        );
        assert_eq!(
            stop_reason_for(RevokeReason::CustomerRequested),
            StopReason::CustomerRequested
        );
        assert_eq!(
            stop_reason_for(RevokeReason::Expired),
            StopReason::Disconnected
        );
        assert_eq!(
            stop_reason_for(RevokeReason::Error),
            StopReason::Disconnected
        );
    }
}
