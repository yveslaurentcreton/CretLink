//! The agent-side session entry point and handle (ST-M002-005/006).

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use cretlink_protocol::{ProtocolVersion, SessionApprovalCode, TargetIdentity};
use tokio::sync::mpsc::{self, UnboundedSender, unbounded_channel};
use tokio::task::JoinHandle as SpawnedTask;

use crate::reboot::ArmCommand;
use crate::runtime::ConsentDecision;
use crate::{
    CleanupReport, Dial, DirectDial, DisclosureSummary, NoAudit, NoRebootResume, RebootProbe,
    RebootResumeArmer, RebootResumeTicketId, RevokeReason, SessionAudit, SessionError,
    SessionEvent, SessionId, Shared, runtime,
};

/// The agent side of a session (the Noise **initiator** that dials, ADR-0003).
///
/// [`SessionJoiner::join`] dials the console, pairs, and (mirroring the agent
/// state machine) announces the informed disclosure but grants **no control**
/// until the customer calls [`JoinHandle::grant_consent`].
#[derive(Debug, Clone, Copy, Default)]
pub struct SessionJoiner;

impl SessionJoiner {
    /// Dials `target` and runs one agent-side session in the background.
    ///
    /// `identity` is the honest, effective-identity-derived target identity
    /// bound into the consent grant (REQ-F-016, REQ-S-011); the support agent
    /// derives it from its real process token before calling `join`. Pairing
    /// completes in the background and surfaces as [`SessionEvent::Connected`];
    /// the customer then decides with [`JoinHandle::grant_consent`] /
    /// [`JoinHandle::deny_consent`].
    ///
    /// # Errors
    ///
    /// [`SessionError::Io`] if the target cannot be dialed.
    pub async fn join(
        target: SocketAddr,
        identity: TargetIdentity,
        version: ProtocolVersion,
        capabilities: Vec<String>,
    ) -> Result<JoinHandle, SessionError> {
        Self::join_with_audit(target, identity, version, capabilities, Arc::new(NoAudit)).await
    }

    /// Like [`SessionJoiner::join`], but records lifecycle outcomes to `audit`
    /// (REQ-F-017, REQ-S-017).
    ///
    /// # Errors
    ///
    /// [`SessionError::Io`] if the target cannot be dialed.
    pub async fn join_with_audit(
        target: SocketAddr,
        identity: TargetIdentity,
        version: ProtocolVersion,
        capabilities: Vec<String>,
        audit: Arc<dyn SessionAudit>,
    ) -> Result<JoinHandle, SessionError> {
        // Without an injected probe the agent honestly reports "no reboot
        // signal observed" (the documented best-effort `false`, ST-M003-003).
        Self::join_configured(
            target,
            identity,
            version,
            capabilities,
            audit,
            Arc::new(|| false),
        )
        .await
    }

    /// Like [`SessionJoiner::join_with_audit`], but also injects the live
    /// reboot-pending probe the agent answers `SystemInfo` requests with
    /// (ST-M003-003). The support agent wires the real Windows registry probe
    /// here; tests inject a fixed value for determinism.
    ///
    /// # Errors
    ///
    /// [`SessionError::Io`] if the target cannot be dialed.
    pub async fn join_configured(
        target: SocketAddr,
        identity: TargetIdentity,
        version: ProtocolVersion,
        capabilities: Vec<String>,
        audit: Arc<dyn SessionAudit>,
        reboot_probe: RebootProbe,
    ) -> Result<JoinHandle, SessionError> {
        // Reboot-resume is off by default (ADR-0014): install the refusing armer
        // so nothing can be armed unless the agent explicitly opts in via
        // [`SessionJoiner::join_with_reboot_resume`].
        Self::join_inner(
            DirectDial::new(target),
            identity,
            version,
            capabilities,
            audit,
            reboot_probe,
            Arc::new(NoRebootResume),
        )
        .await
    }

    /// Like [`SessionJoiner::join_configured`], but injects a real
    /// [`RebootResumeArmer`] so an attended local gesture can arm a single-use
    /// reboot-resume ticket (ADR-0014, ST-M006-001).
    ///
    /// This is the **only** entry point that enables arming. Arming is still
    /// gated on a live, consented session and driven exclusively by
    /// [`JoinHandle::arm_reboot_resume`]; it is never reachable from the console
    /// or the MCP surface (REQ-S-001, REQ-S-003). The agent must advertise
    /// [`CAP_REBOOT_RESUME_V1`](cretlink_protocol::CAP_REBOOT_RESUME_V1) in
    /// `capabilities` for a matched console to accept the armed-state reflection.
    ///
    /// # Errors
    ///
    /// [`SessionError::Io`] if the target cannot be dialed.
    pub async fn join_with_reboot_resume(
        target: SocketAddr,
        identity: TargetIdentity,
        version: ProtocolVersion,
        capabilities: Vec<String>,
        audit: Arc<dyn SessionAudit>,
        reboot_probe: RebootProbe,
        armer: Arc<dyn RebootResumeArmer>,
    ) -> Result<JoinHandle, SessionError> {
        Self::join_inner(
            DirectDial::new(target),
            identity,
            version,
            capabilities,
            audit,
            reboot_probe,
            armer,
        )
        .await
    }

    /// Like [`SessionJoiner::join_with_reboot_resume`], but obtains the session
    /// stream from an arbitrary [`Dial`] rather than a direct LAN connect — the
    /// relay-mode agent entry (ST-M007-008, ADR-0016 §5). Pass a
    /// [`RelayDial`](crate::RelayDial) to reach the console across the internet
    /// relay: the agent dials the relay outbound, writes its rendezvous preamble,
    /// and the pairing, consent, attribution, revocation, and cleanup that follow
    /// are byte-identical to the LAN path (REQ-F-005, REQ-F-016..019). Selecting
    /// the relay is a transport choice, not a session-logic change (ADR-0007).
    ///
    /// # Errors
    ///
    /// [`SessionError::Io`] if the dialer cannot obtain the stream — an
    /// unreachable relay, a rejected preamble, or a never-matched peer all fail
    /// closed here and never fabricate a session (REQ-N-008, REQ-N-015).
    ///
    #[allow(clippy::too_many_arguments)]
    pub async fn join_with_reboot_resume_over<D: Dial>(
        dialer: D,
        identity: TargetIdentity,
        version: ProtocolVersion,
        capabilities: Vec<String>,
        audit: Arc<dyn SessionAudit>,
        reboot_probe: RebootProbe,
        armer: Arc<dyn RebootResumeArmer>,
    ) -> Result<JoinHandle, SessionError> {
        Self::join_inner(
            dialer,
            identity,
            version,
            capabilities,
            audit,
            reboot_probe,
            armer,
        )
        .await
    }

    /// Dials via `dialer` and spawns the agent-side session task with the given
    /// reboot-resume armer wired in (the shared body of the public join entry
    /// points). Generic over the [`Dial`] so the LAN direct path
    /// ([`DirectDial`]) and the relay-rendezvous path
    /// ([`RelayDial`](crate::RelayDial)) share one body (ST-M007-008).
    #[allow(clippy::too_many_arguments)]
    async fn join_inner<D: Dial>(
        dialer: D,
        identity: TargetIdentity,
        version: ProtocolVersion,
        capabilities: Vec<String>,
        audit: Arc<dyn SessionAudit>,
        reboot_probe: RebootProbe,
        armer: Arc<dyn RebootResumeArmer>,
    ) -> Result<JoinHandle, SessionError> {
        let stream = dialer.dial().await?;

        let disclosure = DisclosureSummary::default();
        let shared = Arc::new(Shared::new());
        // Bounded to match the console side and keep one event type across the
        // shared runtime (REQ-N-006); the agent publishes only low-volume
        // lifecycle events, so the bound is slack here. Control channels stay
        // unbounded so consent/revoke/arm never block.
        let (event_tx, event_rx) = mpsc::channel(runtime::SESSION_EVENT_CHANNEL_CAPACITY);
        let (consent_tx, consent_rx) = unbounded_channel();
        let (revoke_tx, revoke_rx) = unbounded_channel();
        let (arm_tx, arm_rx) = unbounded_channel();

        let task = tokio::spawn(runtime::join_task(
            stream,
            identity,
            reboot_probe,
            disclosure.text.clone(),
            version,
            capabilities,
            Arc::clone(&shared),
            event_tx,
            consent_rx,
            revoke_rx,
            arm_rx,
            armer,
            audit,
        ));

        Ok(JoinHandle {
            shared,
            events: event_rx,
            consent_tx,
            revoke_tx,
            arm_tx,
            disclosure,
            task,
        })
    }
}

/// A handle to a running agent-side session.
///
/// Dropping the handle tears the session down fail-closed: the background task
/// is aborted, dropping the [`SecureChannel`](cretlink_transport::SecureChannel)
/// and letting the console observe the disconnect.
#[derive(Debug)]
pub struct JoinHandle {
    shared: Arc<Shared>,
    events: mpsc::Receiver<SessionEvent>,
    consent_tx: UnboundedSender<ConsentDecision>,
    revoke_tx: UnboundedSender<RevokeReason>,
    arm_tx: UnboundedSender<ArmCommand>,
    disclosure: DisclosureSummary,
    task: SpawnedTask<()>,
}

impl JoinHandle {
    /// The full-control disclosure shown to the customer and sent to the console
    /// (REQ-S-003, REQ-S-016).
    #[must_use]
    pub fn disclosure(&self) -> &DisclosureSummary {
        &self.disclosure
    }

    /// The six-digit Session Approval Code, available once
    /// [`SessionEvent::Connected`] has been observed (REQ-F-005).
    #[must_use]
    pub fn approval_code(&self) -> Option<SessionApprovalCode> {
        self.shared.approval_code()
    }

    /// Whether attended full control is active right now (REQ-S-001). `false`
    /// before consent and after any teardown.
    #[must_use]
    pub fn control_active(&self) -> bool {
        self.shared.control_active()
    }

    /// The negotiated session id, available once control is active. Mirrors
    /// [`crate::HostHandle::session`] so the agent side can bind its own state
    /// to the same session the console sees (REQ-F-016, REQ-S-012).
    #[must_use]
    pub fn session(&self) -> Option<SessionId> {
        self.shared.session()
    }

    /// The number of heartbeats received from the console so far.
    #[must_use]
    pub fn heartbeats_received(&self) -> u64 {
        self.shared.heartbeats()
    }

    /// The honest cleanup report, available once the session has torn down
    /// (REQ-F-019).
    #[must_use]
    pub fn cleanup_report(&self) -> Option<CleanupReport> {
        self.shared.cleanup()
    }

    /// Awaits the next lifecycle event, or `None` once the session task has
    /// ended and its events are drained.
    pub async fn next_event(&mut self) -> Option<SessionEvent> {
        self.events.recv().await
    }

    /// The customer grants one attended full-control consent — the **only** path
    /// to control (REQ-S-001, REQ-S-003). No control exists before this call.
    ///
    /// # Errors
    ///
    /// [`SessionError::Disconnected`] if the session has already ended.
    pub fn grant_consent(&self) -> Result<(), SessionError> {
        self.consent_tx
            .send(ConsentDecision::Grant)
            .map_err(|_| SessionError::Disconnected)
    }

    /// The customer declines full-control consent; no control is granted.
    ///
    /// # Errors
    ///
    /// [`SessionError::Disconnected`] if the session has already ended.
    pub fn deny_consent(&self, reason: impl Into<String>) -> Result<(), SessionError> {
        self.consent_tx
            .send(ConsentDecision::Deny(reason.into()))
            .map_err(|_| SessionError::Disconnected)
    }

    /// Revokes the session with an explicit reason, tearing it down fail-closed
    /// (REQ-S-004, REQ-S-009).
    ///
    /// # Errors
    ///
    /// [`SessionError::Disconnected`] if the session has already ended.
    pub fn revoke(&self, reason: RevokeReason) -> Result<(), SessionError> {
        self.revoke_tx
            .send(reason)
            .map_err(|_| SessionError::Disconnected)
    }

    /// Customer-initiated stop (REQ-S-004).
    ///
    /// # Errors
    ///
    /// [`SessionError::Disconnected`] if the session has already ended.
    pub fn stop(&self) -> Result<(), SessionError> {
        self.revoke(RevokeReason::CustomerRequested)
    }

    /// Requests that the agent arm a single-use reboot-resume ticket for this
    /// live, consented session, redeemable for `lifetime` from now (ADR-0014,
    /// ST-M006-001).
    ///
    /// This models an attended **local** gesture on the agent: it is honored only
    /// while control is active, and only when the agent was started with a real
    /// armer via [`SessionJoiner::join_with_reboot_resume`]. It never restores
    /// control by itself — attended consent still runs after any reconnect
    /// (REQ-S-001). The outcome (armed ticket id and expiry, or a refusal) is
    /// surfaced as a [`SessionEvent::RebootResumeArmed`] or recorded in the audit
    /// log; it is never reachable from the console or the MCP surface
    /// (REQ-S-001, REQ-S-003).
    ///
    /// # Errors
    ///
    /// [`SessionError::Disconnected`] if the session has already ended.
    pub fn arm_reboot_resume(&self, lifetime: Duration) -> Result<(), SessionError> {
        self.arm_tx
            .send(ArmCommand::Arm { lifetime })
            .map_err(|_| SessionError::Disconnected)
    }

    /// Requests that the agent disarm a previously armed reboot-resume ticket
    /// (arm-then-cancel), so nothing survives to a reboot (ADR-0014).
    ///
    /// # Errors
    ///
    /// [`SessionError::Disconnected`] if the session has already ended.
    pub fn disarm_reboot_resume(
        &self,
        ticket_id: RebootResumeTicketId,
    ) -> Result<(), SessionError> {
        self.arm_tx
            .send(ArmCommand::Disarm { ticket_id })
            .map_err(|_| SessionError::Disconnected)
    }

    /// The expiry (Unix milliseconds, UTC) of the currently armed reboot-resume
    /// ticket, or `None` when nothing is armed. Display-only; carries no key
    /// material (ADR-0014).
    #[must_use]
    pub fn reboot_resume_armed_until(&self) -> Option<i64> {
        self.shared.armed_not_after()
    }
}

impl Drop for JoinHandle {
    fn drop(&mut self) {
        // Fail-closed teardown on drop (REQ-S-009): see `HostHandle::drop`.
        self.task.abort();
    }
}
