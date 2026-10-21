//! The customer-side agent session lifecycle (ST-M002-001).
//!
//! This is the support agent's mirror of the technician console state model
//! (`cretlink-console`'s `state` module). It exists so the customer-facing UI
//! renders one authoritative model and the security invariants live in one
//! reviewed place instead of being reimplemented per widget.
//!
//! # Invariants enforced here
//!
//! - **No control before consent (REQ-S-001, REQ-S-003).**
//!   [`AgentState::control_granted`] (and its alias [`AgentState::can_execute`])
//!   is `true` only in [`AgentPhase::FullControl`], which is reachable *only*
//!   through [`AgentState::grant_consent`] from [`AgentPhase::AwaitingConsent`].
//!   Every earlier phase reports no control.
//! - **Honest full control (REQ-S-011).** Granting consent records the
//!   negotiated [`SessionId`], and the agent presents the honest
//!   [`EffectiveIdentity`] captured at startup; it never advertises authority it
//!   lacks.
//! - **Bilateral, fail-closed stop (REQ-S-004, REQ-S-009).** Either
//!   [`AgentState::stop`] (customer) or [`AgentState::remote_stop`] /
//!   [`AgentState::disconnect`] (technician) collapses to
//!   [`AgentPhase::Stopped`] and revokes control. Stopping is idempotent, and no
//!   transition restores [`AgentPhase::FullControl`]: a reconnect must re-pair
//!   and re-consent.
//! - **No stealth (REQ-S-002).** [`AgentState::visible_status`] returns a plain
//!   [`VisibleStatus`] the UI always renders, so phase, identity, and whether
//!   control is active are never hidden.

use serde::{Deserialize, Serialize};
use thiserror::Error;

use cretlink_protocol::SessionId;

use crate::privilege::EffectiveIdentity;

/// Why an agent session stopped, recorded on [`AgentPhase::Stopped`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    /// The technician ended the session from the console.
    TechnicianRequested,
    /// The customer pressed Stop on the agent.
    CustomerRequested,
    /// The connection dropped.
    Disconnected,
    /// The session ended because of a failure.
    Failed,
}

/// The lifecycle phase of the agent's single session.
///
/// `Serialize`/`Deserialize` use an internally tagged `phase` field so the UI
/// snapshot reads as, for example, `{"phase":"full_control","session":"..."}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "phase", rename_all = "snake_case")]
pub enum AgentPhase {
    /// Launched, not yet advertising.
    Idle,
    /// Advertising and reachable on the LAN, before any pairing (no control).
    Announced,
    /// A pairing handshake is in progress (no control).
    Pairing,
    /// Pairing is done and the Session Approval Code is on screen; the customer
    /// is asked to read it aloud and compare it with the technician's, and the
    /// session pauses here until the technician confirms the codes match. No
    /// full-control consent is offered yet, and no control exists (REQ-F-005,
    /// REQ-S-006, ST-M002-013).
    AwaitingApprovalCompare,
    /// The Session Approval Code has been compared and confirmed to match;
    /// awaiting the customer's single full-control consent (still no control).
    AwaitingConsent,
    /// Attended full control is active for the negotiated session.
    FullControl {
        /// The negotiated session this control is bound to.
        session: SessionId,
    },
    /// A stop is in progress; control is already revoked.
    Stopping,
    /// The session has ended; no new actions are accepted.
    Stopped {
        /// Why the session stopped.
        reason: StopReason,
    },
    /// The session failed. `stage` and `message` are actionable and free of
    /// secrets.
    Failed {
        /// Which stage failed (for example "pairing" or "consent").
        stage: String,
        /// A plain-language, secret-free description.
        message: String,
    },
}

impl AgentPhase {
    /// A short static label, safe to show and free of any variant payload.
    #[must_use]
    pub fn label(&self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Announced => "announced",
            Self::Pairing => "pairing",
            Self::AwaitingApprovalCompare => "comparing approval code",
            Self::AwaitingConsent => "awaiting consent",
            Self::FullControl { .. } => "under full control",
            Self::Stopping => "stopping",
            Self::Stopped { .. } => "stopped",
            Self::Failed { .. } => "failed",
        }
    }

    /// Whether this is a terminal phase (a stopped or failed session).
    #[must_use]
    fn is_terminal(&self) -> bool {
        matches!(self, Self::Stopped { .. } | Self::Failed { .. })
    }
}

/// A refused agent state transition. Every variant is safe to show: it names
/// the attempted action and the current phase without leaking any payload
/// (REQ-N-015).
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum AgentError {
    /// The requested transition is not legal from the current phase.
    #[error("cannot {action} while the agent is {phase}")]
    InvalidTransition {
        /// The action that was attempted.
        action: &'static str,
        /// The phase it was attempted from.
        phase: &'static str,
    },
}

/// A plain, fully-serialisable snapshot the agent UI renders (REQ-S-002).
///
/// It carries nothing hidden: the current phase, the honest identity, and
/// whether full control is active right now.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VisibleStatus {
    /// The current lifecycle phase.
    pub phase: AgentPhase,
    /// The agent's honestly-reported effective identity (REQ-S-011).
    pub identity: EffectiveIdentity,
    /// Whether attended full control is active at this moment.
    pub control_active: bool,
}

/// The customer-side agent's session state: its honest identity and the single
/// session lifecycle phase.
///
/// The phase is private and mutated only through the transition methods, so the
/// no-control-before-consent and fail-closed-stop invariants cannot be bypassed
/// by a caller.
#[derive(Debug, Clone, Serialize)]
pub struct AgentState {
    identity: EffectiveIdentity,
    phase: AgentPhase,
}

impl AgentState {
    /// Creates an idle agent bound to its honestly-reported identity.
    #[must_use]
    pub fn new(identity: EffectiveIdentity) -> Self {
        Self {
            identity,
            phase: AgentPhase::Idle,
        }
    }

    /// The agent's honest effective identity (REQ-S-011).
    #[must_use]
    pub fn identity(&self) -> &EffectiveIdentity {
        &self.identity
    }

    /// The current lifecycle phase.
    #[must_use]
    pub fn phase(&self) -> &AgentPhase {
        &self.phase
    }

    /// Whether attended full control is active — `true` only in
    /// [`AgentPhase::FullControl`], reached via [`Self::grant_consent`]
    /// (REQ-S-001, REQ-S-003).
    #[must_use]
    pub fn control_granted(&self) -> bool {
        matches!(self.phase, AgentPhase::FullControl { .. })
    }

    /// Alias for [`Self::control_granted`]: whether remote actions may run.
    #[must_use]
    pub fn can_execute(&self) -> bool {
        self.control_granted()
    }

    /// The session identifier while under full control, else `None`.
    #[must_use]
    pub fn session(&self) -> Option<SessionId> {
        match &self.phase {
            AgentPhase::FullControl { session } => Some(*session),
            _ => None,
        }
    }

    /// A plain, renderable snapshot of the whole visible state (REQ-S-002).
    #[must_use]
    pub fn visible_status(&self) -> VisibleStatus {
        VisibleStatus {
            phase: self.phase.clone(),
            identity: self.identity.clone(),
            control_active: self.control_granted(),
        }
    }

    /// Begins advertising on the LAN: [`AgentPhase::Idle`] ->
    /// [`AgentPhase::Announced`].
    ///
    /// # Errors
    ///
    /// [`AgentError::InvalidTransition`] unless the agent is idle.
    pub fn announce(&mut self) -> Result<(), AgentError> {
        self.require_phase("announce", matches!(self.phase, AgentPhase::Idle))?;
        self.phase = AgentPhase::Announced;
        Ok(())
    }

    /// Starts pairing: [`AgentPhase::Announced`] -> [`AgentPhase::Pairing`].
    ///
    /// # Errors
    ///
    /// [`AgentError::InvalidTransition`] unless the agent is announced.
    pub fn begin_pairing(&mut self) -> Result<(), AgentError> {
        self.require_phase("begin pairing", matches!(self.phase, AgentPhase::Announced))?;
        self.phase = AgentPhase::Pairing;
        Ok(())
    }

    /// Enters the approval-code comparison window: [`AgentPhase::Pairing`] ->
    /// [`AgentPhase::AwaitingApprovalCompare`]. The customer sees the Session
    /// Approval Code and is asked to compare it with the technician's before any
    /// consent is offered; the session holds here until the technician confirms
    /// the codes match (REQ-F-005, REQ-S-006, ST-M002-013). Still no control.
    ///
    /// # Errors
    ///
    /// [`AgentError::InvalidTransition`] unless the agent is pairing.
    pub fn begin_compare(&mut self) -> Result<(), AgentError> {
        self.require_phase("begin compare", matches!(self.phase, AgentPhase::Pairing))?;
        self.phase = AgentPhase::AwaitingApprovalCompare;
        Ok(())
    }

    /// Records that the Session Approval Code was confirmed to match and the
    /// customer is now being asked to consent:
    /// [`AgentPhase::AwaitingApprovalCompare`] -> [`AgentPhase::AwaitingConsent`].
    ///
    /// # Errors
    ///
    /// [`AgentError::InvalidTransition`] unless the agent is comparing the
    /// approval code.
    pub fn await_consent(&mut self) -> Result<(), AgentError> {
        self.require_phase(
            "await consent",
            matches!(self.phase, AgentPhase::AwaitingApprovalCompare),
        )?;
        self.phase = AgentPhase::AwaitingConsent;
        Ok(())
    }

    /// Grants one attended full-control consent, binding it to the negotiated
    /// session: [`AgentPhase::AwaitingConsent`] -> [`AgentPhase::FullControl`]
    /// (REQ-S-001, REQ-S-003). This is the only path to control.
    ///
    /// # Errors
    ///
    /// [`AgentError::InvalidTransition`] unless the agent is awaiting consent.
    pub fn grant_consent(&mut self, session: SessionId) -> Result<(), AgentError> {
        self.require_phase(
            "grant consent",
            matches!(self.phase, AgentPhase::AwaitingConsent),
        )?;
        self.phase = AgentPhase::FullControl { session };
        Ok(())
    }

    /// Marks teardown as in progress, revoking control: any live phase ->
    /// [`AgentPhase::Stopping`]. A no-op once terminal.
    pub fn begin_stopping(&mut self) {
        if !self.phase.is_terminal() {
            self.phase = AgentPhase::Stopping;
        }
    }

    /// Customer-initiated stop (REQ-S-004).
    pub fn stop(&mut self) {
        self.stop_with(StopReason::CustomerRequested);
    }

    /// Technician-initiated stop from the console (REQ-S-004).
    pub fn remote_stop(&mut self) {
        self.stop_with(StopReason::TechnicianRequested);
    }

    /// Stop caused by a dropped connection (REQ-S-004).
    pub fn disconnect(&mut self) {
        self.stop_with(StopReason::Disconnected);
    }

    /// Fails the session at a named stage; `message` must be secret-free
    /// (fail-closed, REQ-S-009). A no-op once already terminal, so a failure
    /// never overwrites a clean stop.
    pub fn fail(&mut self, stage: impl Into<String>, message: impl Into<String>) {
        if !self.phase.is_terminal() {
            self.phase = AgentPhase::Failed {
                stage: stage.into(),
                message: message.into(),
            };
        }
    }

    /// Fail-closed, idempotent stop: any non-terminal phase collapses to
    /// [`AgentPhase::Stopped`] and control is revoked; an already-terminal agent
    /// keeps its first outcome. A second stop therefore never re-opens control,
    /// and a reconnect cannot silently restore full control (REQ-S-009).
    fn stop_with(&mut self, reason: StopReason) {
        if !self.phase.is_terminal() {
            self.phase = AgentPhase::Stopped { reason };
        }
    }

    /// Returns `Ok` when the guard holds, else a secret-free
    /// [`AgentError::InvalidTransition`] naming the action and current phase.
    fn require_phase(&self, action: &'static str, ok: bool) -> Result<(), AgentError> {
        if ok {
            Ok(())
        } else {
            Err(AgentError::InvalidTransition {
                action,
                phase: self.phase.label(),
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::privilege::IntegrityLevel;

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

    fn idle_agent() -> AgentState {
        AgentState::new(test_identity())
    }

    fn drive_to_full_control(agent: &mut AgentState) -> SessionId {
        let session = SessionId::new();
        agent.announce().unwrap();
        agent.begin_pairing().unwrap();
        agent.begin_compare().unwrap();
        agent.await_consent().unwrap();
        agent.grant_consent(session).unwrap();
        session
    }

    #[test]
    fn no_control_before_consent() {
        let mut agent = idle_agent();
        assert!(!agent.control_granted());
        assert!(!agent.can_execute());

        agent.announce().unwrap();
        assert!(!agent.control_granted());
        agent.begin_pairing().unwrap();
        assert!(!agent.control_granted());
        agent.begin_compare().unwrap();
        assert!(!agent.control_granted());
        assert!(agent.session().is_none());
        agent.await_consent().unwrap();
        assert!(!agent.control_granted());
        assert!(agent.session().is_none());

        agent.grant_consent(SessionId::new()).unwrap();
        assert!(agent.control_granted());
        assert!(agent.can_execute());
    }

    #[test]
    fn compare_gate_precedes_consent_and_offers_no_control() {
        // The approval-code comparison is a real, ordered phase: consent cannot
        // be reached without first passing through it (REQ-S-006, ST-M002-013).
        let mut agent = idle_agent();
        agent.announce().unwrap();
        agent.begin_pairing().unwrap();

        // Consent cannot be offered straight from pairing; the compare gate is
        // mandatory and is where the customer reads the code aloud.
        let err = agent.await_consent().unwrap_err();
        assert!(matches!(err, AgentError::InvalidTransition { .. }));

        agent.begin_compare().unwrap();
        assert_eq!(agent.phase(), &AgentPhase::AwaitingApprovalCompare);
        assert!(!agent.control_granted());
        // Control cannot be granted while still comparing.
        assert!(agent.grant_consent(SessionId::new()).is_err());

        agent.await_consent().unwrap();
        assert_eq!(agent.phase(), &AgentPhase::AwaitingConsent);
    }

    #[test]
    fn consent_is_the_only_path_to_control_and_records_the_session() {
        let mut agent = idle_agent();
        // Consent cannot be granted out of order.
        let err = agent.grant_consent(SessionId::new()).unwrap_err();
        assert!(matches!(err, AgentError::InvalidTransition { .. }));
        assert!(!agent.control_granted());

        let session = drive_to_full_control(&mut agent);
        assert_eq!(agent.session(), Some(session));
        assert_eq!(agent.phase(), &AgentPhase::FullControl { session });
    }

    #[test]
    fn either_side_can_stop_and_stop_is_idempotent_and_fail_closed() {
        // Customer stop from full control.
        let mut agent = idle_agent();
        drive_to_full_control(&mut agent);
        agent.stop();
        assert!(!agent.control_granted());
        assert_eq!(
            agent.phase(),
            &AgentPhase::Stopped {
                reason: StopReason::CustomerRequested
            }
        );
        // A second stop (from the other side) neither re-opens nor changes it.
        agent.remote_stop();
        assert_eq!(
            agent.phase(),
            &AgentPhase::Stopped {
                reason: StopReason::CustomerRequested
            }
        );
        assert!(!agent.control_granted());

        // Technician stop from full control.
        let mut agent = idle_agent();
        drive_to_full_control(&mut agent);
        agent.remote_stop();
        assert_eq!(
            agent.phase(),
            &AgentPhase::Stopped {
                reason: StopReason::TechnicianRequested
            }
        );

        // Disconnect from full control.
        let mut agent = idle_agent();
        drive_to_full_control(&mut agent);
        agent.disconnect();
        assert_eq!(
            agent.phase(),
            &AgentPhase::Stopped {
                reason: StopReason::Disconnected
            }
        );
        assert!(!agent.can_execute());
    }

    #[test]
    fn reconnect_cannot_silently_restore_full_control() {
        let mut agent = idle_agent();
        drive_to_full_control(&mut agent);
        agent.stop();

        // Re-granting consent from a stopped session is refused: control only
        // returns via a fresh announce -> pairing -> consent sequence.
        let err = agent.grant_consent(SessionId::new()).unwrap_err();
        assert!(matches!(err, AgentError::InvalidTransition { .. }));
        assert!(!agent.control_granted());
        // Even re-announcing is refused from a terminal phase; a new session
        // starts from a fresh AgentState.
        assert!(agent.announce().is_err());
    }

    #[test]
    fn stopping_revokes_control_then_stops() {
        let mut agent = idle_agent();
        drive_to_full_control(&mut agent);
        agent.begin_stopping();
        assert_eq!(agent.phase(), &AgentPhase::Stopping);
        assert!(!agent.control_granted());
        agent.stop();
        assert_eq!(
            agent.phase(),
            &AgentPhase::Stopped {
                reason: StopReason::CustomerRequested
            }
        );
    }

    #[test]
    fn failure_is_terminal_secret_free_and_revokes_control() {
        let mut agent = idle_agent();
        drive_to_full_control(&mut agent);
        agent.fail("pairing", "code mismatch");
        assert!(!agent.control_granted());
        assert_eq!(
            agent.phase(),
            &AgentPhase::Failed {
                stage: "pairing".to_string(),
                message: "code mismatch".to_string(),
            }
        );
        // A later stop does not overwrite the recorded failure.
        agent.stop();
        assert!(matches!(agent.phase(), AgentPhase::Failed { .. }));
    }

    #[test]
    fn visible_status_hides_nothing() {
        let mut agent = idle_agent();
        let status = agent.visible_status();
        assert_eq!(status.phase, AgentPhase::Idle);
        assert!(!status.control_active);
        assert_eq!(status.identity, test_identity());

        let session = drive_to_full_control(&mut agent);
        let status = agent.visible_status();
        assert!(status.control_active);
        assert_eq!(status.phase, AgentPhase::FullControl { session });
    }

    #[test]
    fn visible_status_serialises_for_the_ui() {
        let mut agent = idle_agent();
        let session = drive_to_full_control(&mut agent);
        let status = agent.visible_status();
        let json = serde_json::to_string(&status).unwrap();
        assert!(json.contains("\"phase\":\"full_control\""));
        assert!(json.contains("\"control_active\":true"));
        let back: VisibleStatus = serde_json::from_str(&json).unwrap();
        assert_eq!(back, status);
        assert_eq!(back.phase, AgentPhase::FullControl { session });

        // StopReason serialises in snake_case alongside its phase tag.
        let stopped = AgentPhase::Stopped {
            reason: StopReason::TechnicianRequested,
        };
        let json = serde_json::to_string(&stopped).unwrap();
        assert!(json.contains("\"phase\":\"stopped\""));
        assert!(json.contains("\"reason\":\"technician_requested\""));
    }
}
