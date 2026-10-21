//! Live-transport driver for the agent consent window (ST-M002-009).
//!
//! This is the *initiator* mirror of the console GUI's `session_driver`. It
//! folds a real `cretlink-session` [`SessionEvent`] stream from a dialed
//! [`JoinHandle`] into the shared [`ConsentSession`], re-emitting the whole
//! [`ConsentView`] after every fold so the Tauri frontend re-renders. It reuses
//! the cross-platform, unit-tested agent adapter
//! [`apply_session_event`](cretlink_support_agent::apply_session_event)
//! (ADR-0011): the security-relevant state transitions live there. This module
//! only owns the transport plumbing — building the out-of-band [`EventContext`]
//! from the live handle, routing button intents to the wire, and pumping the
//! async stream.
//!
//! # Why the buttons send intents instead of acting directly
//!
//! Every wire control method ([`JoinHandle::grant_consent`],
//! [`deny_consent`](JoinHandle::deny_consent), [`revoke`](JoinHandle::revoke))
//! needs the one live handle, and [`JoinHandle::next_event`] borrows it
//! mutably. The pump task therefore owns the handle exclusively and
//! [`select!`](tokio::select)s between the next lifecycle event and the next
//! customer [`Intent`]; the consent/decline/stop commands send an intent and the
//! pump performs the matching wire action. Control is never fabricated locally:
//! full control appears only when the customer's real
//! [`grant_consent`](JoinHandle::grant_consent) echoes back as
//! [`SessionEvent::ConsentGranted`] and is folded here (REQ-S-001, REQ-S-003).
//!
//! It is deliberately fail-closed. A refused fold, a transport that closes
//! before a terminal event, or a dropped intent channel all drive the model to
//! a terminal phase; no dropped connection can leave a stale "under control"
//! surface on screen (REQ-S-009).

use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use cretlink_protocol::{JobOutcome, Origin, RevokeReason, Shell};
use cretlink_session::{JoinHandle, SessionEvent};
use cretlink_support_agent::{AgentPhase, EventContext, EventOutcome, apply_session_event};
use serde::Serialize;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};

use crate::{ConsentSession, ConsentView};

/// The sender half of the button-intent channel, held by the consent window's
/// [`ConsentSession`] so the synchronous Tauri commands can hand a decision to
/// the async [`pump`] task that owns the live [`JoinHandle`].
pub(crate) type IntentSender = UnboundedSender<Intent>;

/// Creates the intent channel that connects the Tauri commands (senders) to the
/// [`pump`] (the single receiver). Unbounded so a button press is never dropped
/// or blocked behind the network.
pub(crate) fn intent_channel() -> (IntentSender, UnboundedReceiver<Intent>) {
    unbounded_channel()
}

/// A customer decision routed from a synchronous Tauri command to the async
/// pump task that owns the live [`JoinHandle`].
#[derive(Debug)]
pub enum Intent {
    /// The customer's single attended full-control consent — the only path to
    /// control (REQ-S-001, REQ-S-003).
    Grant,
    /// The customer declined; the session ends with no control ever granted.
    Decline(String),
    /// The emergency stop / revoke (REQ-F-018, REQ-S-004).
    Stop(RevokeReason),
}

/// Receives whole [`ConsentView`] snapshots to render.
///
/// The GUI implements this over a Tauri `AppHandle` (emitting an `"agent-view"`
/// event); tests implement it with an in-memory recorder so [`step`] is
/// verifiable without a transport.
pub trait ViewSink: Send + Sync + 'static {
    /// Renders one snapshot of the consent view.
    fn emit(&self, view: &ConsentView);

    /// Renders one non-authoritative activity delta (a command starting or
    /// finishing) on a channel separate from [`emit`](ViewSink::emit), so this
    /// higher-frequency stream never forces a whole [`ConsentView`] re-render.
    ///
    /// Defaulted to a no-op: the activity mirror is additive, so test sinks and
    /// any sink that does not surface activity need not implement it. The live
    /// Tauri sink overrides it to emit an `"agent-activity"` event.
    fn activity(&self, _delta: &ActivityDelta) {}
}

/// One non-authoritative activity delta tapped off the live event stream so the
/// attending customer sees which commands run and how they end (REQ-F-017,
/// REQ-S-002).
///
/// This mirrors **only** what the customer side of `cretlink-session` actually
/// emits locally: [`SessionEvent::CommandStarted`] (the command, its shell, and
/// its `origin` attribution) and [`SessionEvent::ExecFinished`] (exit code and
/// outcome). Per-line `ExecOutput` and the coalesced `ExecOutputDropped` marker
/// are console-side only, and the customer-side `ExecFinished` carries no
/// duration (see `crates/session/src/runtime.rs`), so the mirror never fabricates
/// a stream, a truncation marker, or a duration it did not receive. The command
/// text is untrusted remote input; the frontend writes it with `textContent`,
/// never as HTML (REQ-S-016, REQ-S-018).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ActivityDelta {
    /// The job this delta belongs to; the frontend keys one card per job.
    pub job: u64,
    /// Whether the job is `started` (opens the card) or `finished` (closes it).
    pub kind: ActivityKind,
    /// The command line, present on `started`. Untrusted remote text.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    /// A short shell badge label (`"PowerShell"` / `"Command Prompt"`), present
    /// on `started`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub shell: Option<String>,
    /// Who drove the job — the attribution chip. Present on `started`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub origin: Option<Origin>,
    /// The process exit code when one exists, present on `finished`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    /// The honest terminal outcome label, present on `finished`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub outcome: Option<String>,
    /// Whether the job ended successfully, present on `finished`, so the frontend
    /// picks the `ok`/`bad` status style without re-parsing `outcome`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ok: Option<bool>,
}

/// Whether an [`ActivityDelta`] opens a job card or closes it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ActivityKind {
    /// A command started running.
    Started,
    /// A command ended, with its honest outcome.
    Finished,
}

/// Builds the activity delta for the two events the customer side mirrors, or
/// `None` for lifecycle, desktop, and console-only events that carry no job card.
/// Pure and side-effect free, so it is unit-tested directly.
pub(crate) fn activity_delta(event: &SessionEvent) -> Option<ActivityDelta> {
    match event {
        SessionEvent::CommandStarted {
            job,
            shell,
            command,
            origin,
        } => Some(ActivityDelta {
            job: job.0,
            kind: ActivityKind::Started,
            command: Some(command.clone()),
            shell: Some(shell_label(*shell).to_string()),
            origin: Some(*origin),
            exit_code: None,
            outcome: None,
            ok: None,
        }),
        SessionEvent::ExecFinished {
            job,
            exit_code,
            outcome,
        } => Some(ActivityDelta {
            job: job.0,
            kind: ActivityKind::Finished,
            command: None,
            shell: None,
            origin: None,
            exit_code: *exit_code,
            outcome: Some(outcome_label(*outcome).to_string()),
            ok: Some(matches!(outcome, JobOutcome::Completed)),
        }),
        _ => None,
    }
}

/// A short, human-readable badge for the shell that runs a command.
fn shell_label(shell: Shell) -> &'static str {
    match shell {
        Shell::PowerShell => "PowerShell",
        Shell::Cmd => "Command Prompt",
    }
}

/// The honest, plain-language label for a job's terminal outcome — never a
/// fabricated success (REQ-N-008).
fn outcome_label(outcome: JobOutcome) -> &'static str {
    match outcome {
        JobOutcome::Completed => "completed",
        JobOutcome::Failed => "failed",
        JobOutcome::Cancelled => "cancelled",
        JobOutcome::TimedOut => "timed out",
        JobOutcome::Unknown => "unknown",
    }
}

/// Locks the shared session, recovering the inner value if a prior holder
/// panicked (panics are denied in library code, so this is defence in depth,
/// never an expected path).
pub(crate) fn lock(shared: &Mutex<ConsentSession>) -> MutexGuard<'_, ConsentSession> {
    shared.lock().unwrap_or_else(PoisonError::into_inner)
}

/// True once the agent model has reached a terminal phase and must not be folded
/// further.
pub(crate) fn is_terminal(session: &ConsentSession) -> bool {
    matches!(
        session.agent.phase(),
        AgentPhase::Stopped { .. } | AgentPhase::Failed { .. }
    )
}

/// Folds one observed event into `session.agent`, refreshes the display-only
/// Session Approval Code from the wire, emits the resulting view, and reports
/// whether the session reached a terminal phase.
///
/// A refused fold is fail-closed: the model is driven to `Failed` (so no
/// authority can silently survive a malformed or out-of-order stream) and the
/// terminal view is emitted. Returns `true` when the pump should stop.
pub fn step(
    session: &mut ConsentSession,
    event: &SessionEvent,
    ctx: EventContext,
    sac: Option<String>,
    sink: &dyn ViewSink,
) -> bool {
    // Show the real wire SAC to compare, never a locally-invented one. It is a
    // display comparison value, not key material.
    session.set_approval_code(sac);
    let terminal = match apply_session_event(&mut session.agent, event, ctx) {
        Ok(outcome) => matches!(outcome, EventOutcome::Terminated),
        Err(error) => {
            // The adapter left the model untouched on rejection; surface the
            // refusal as an explicit terminal failure rather than silently
            // continuing to pump an inconsistent stream.
            session.agent.fail("session", error.to_string());
            true
        }
    };
    // Non-authoritative activity tap: mirror the customer-visible commands and
    // their outcomes onto a separate channel, beside — never inside — the
    // security-relevant lifecycle fold above (ADR-0011). Only the two events the
    // customer side actually emits produce a card; everything else is `None`.
    if let Some(delta) = activity_delta(event) {
        sink.activity(&delta);
    }
    sink.emit(&session.view());
    terminal
}

/// Pumps `handle`'s live event stream into the shared consent session until a
/// terminal event or the transport closing, while routing button intents to the
/// wire.
///
/// The `next_event` await and the `intents.recv()` await both happen *before*
/// the state lock is taken, so the lock is never held across `.await` and the
/// synchronous Tauri commands never block on the network.
pub async fn pump(
    mut handle: JoinHandle,
    shared: Arc<Mutex<ConsentSession>>,
    sink: Arc<dyn ViewSink>,
    mut intents: UnboundedReceiver<Intent>,
) {
    let mut terminated = false;
    loop {
        tokio::select! {
            maybe_event = handle.next_event() => {
                let Some(event) = maybe_event else {
                    // The stream ended without a terminal event: the transport
                    // dropped mid-session. Handled as fail-closed below.
                    break;
                };
                // The out-of-band values each event references are read straight
                // off the live handle just before the fold.
                let ctx = EventContext { session: handle.session() };
                let sac = handle.approval_code().map(|code| code.to_string());
                let mut session = lock(&shared);
                if is_terminal(&session) {
                    // A local emergency stop or decline already ended the model.
                    // Do not fold further wire events (a late `Active` could flip
                    // a clean `Stopped` into `Failed`); the session is over.
                    sink.emit(&session.view());
                    terminated = true;
                    break;
                }
                if step(&mut session, &event, ctx, sac, sink.as_ref()) {
                    terminated = true;
                    break;
                }
            }
            maybe_intent = intents.recv() => {
                let Some(intent) = maybe_intent else {
                    // Every command handle was dropped without ending the
                    // session; fail closed below rather than pump forever.
                    break;
                };
                // Turn the button press into a real wire action. The model only
                // moves when the wire echoes the resulting lifecycle event and
                // the event branch above folds it — control is never fabricated.
                match intent {
                    Intent::Grant => {
                        let _ = handle.grant_consent();
                    }
                    Intent::Decline(reason) => {
                        let _ = handle.deny_consent(reason);
                    }
                    Intent::Stop(reason) => {
                        let _ = handle.revoke(reason);
                    }
                }
            }
        }
    }
    if !terminated {
        let mut session = lock(&shared);
        if !is_terminal(&session) {
            // Fail closed so the UI shows an honest end, not a frozen
            // "connected" surface (REQ-S-009).
            session
                .agent
                .fail("session", "the transport closed before the session ended");
        }
        sink.emit(&session.view());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use cretlink_protocol::{JobId, RevokeReason, SessionId, TargetIdentity};
    use cretlink_session::OutputStream;
    use tokio::sync::mpsc::unbounded_channel;

    /// Records emitted views so tests can assert both the re-render count and
    /// what the window last showed, without unwrapping a lock. It also records
    /// the separate activity stream so the additive tap can be asserted beside
    /// the unchanged lifecycle fold.
    #[derive(Debug, Default)]
    struct RecordingSink {
        count: AtomicUsize,
        last: Mutex<Option<ConsentView>>,
        activities: Mutex<Vec<ActivityDelta>>,
    }

    impl RecordingSink {
        fn count(&self) -> usize {
            self.count.load(Ordering::Relaxed)
        }

        fn last(&self) -> Option<ConsentView> {
            self.last
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .clone()
        }

        fn activities(&self) -> Vec<ActivityDelta> {
            self.activities
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .clone()
        }
    }

    impl ViewSink for RecordingSink {
        fn emit(&self, view: &ConsentView) {
            self.count.fetch_add(1, Ordering::Relaxed);
            *self.last.lock().unwrap_or_else(PoisonError::into_inner) = Some(view.clone());
        }

        fn activity(&self, delta: &ActivityDelta) {
            self.activities
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push(delta.clone());
        }
    }

    /// The wire identity carried by `ConsentGranted` — the agent's own identity
    /// echoed back by the console, which the adapter deliberately ignores.
    fn consent_event() -> SessionEvent {
        SessionEvent::ConsentGranted {
            identity: TargetIdentity {
                hostname: "CUSTOMER-PC".to_string(),
                os_version: "Windows 11".to_string(),
                arch: "x86_64".to_string(),
                elevated: false,
                simulated: false,
                agent_version: "0.0.0".to_string(),
            },
        }
    }

    /// A live, dialing consent session — the state the pump is in once the
    /// listener has been dialed and the event stream starts. The intent receiver
    /// is dropped: `step` never touches it, only the async `pump` does.
    fn dialing_session() -> ConsentSession {
        let (tx, _rx) = unbounded_channel::<Intent>();
        ConsentSession::live_dialing(tx)
    }

    #[test]
    fn live_happy_path_folds_to_full_control_and_shows_the_wire_code() {
        let mut session = dialing_session();
        let sid = SessionId::new();
        let sink = RecordingSink::default();
        let ctx = EventContext { session: Some(sid) };

        assert!(!step(
            &mut session,
            &SessionEvent::Connected,
            ctx,
            Some("11-22-33".to_string()),
            &sink,
        ));
        let view = session.view();
        // The real wire SAC is shown while pairing, and this is not a demo.
        assert_eq!(view.approval_code.as_deref(), Some("11-22-33"));
        assert!(!view.simulated);
        assert!(!view.status.control_active);

        // The compare gate holds here — the code is on screen and no consent is
        // offered until the technician confirms it matches (REQ-S-006,
        // ST-M002-013). The wire SAC stays visible for the comparison.
        assert!(!step(
            &mut session,
            &SessionEvent::AwaitingApprovalCompare,
            ctx,
            Some("11-22-33".to_string()),
            &sink,
        ));
        let view = session.view();
        assert_eq!(view.approval_code.as_deref(), Some("11-22-33"));
        assert!(!view.status.control_active);
        assert!(!session.agent.control_granted());

        assert!(!step(
            &mut session,
            &SessionEvent::AwaitingConsent,
            ctx,
            Some("11-22-33".to_string()),
            &sink,
        ));
        assert!(!session.agent.control_granted());

        // Consent folds to full control, bound to the real wire session.
        assert!(!step(&mut session, &consent_event(), ctx, None, &sink));
        let view = session.view();
        assert!(view.status.control_active);
        // Once under control the pre-decision code is no longer shown.
        assert!(view.approval_code.is_none());

        assert!(!step(&mut session, &SessionEvent::Active, ctx, None, &sink));
        assert!(session.agent.control_granted());

        // A revoke is terminal and tears control down fail-closed.
        let revoked = SessionEvent::Revoked {
            reason: RevokeReason::CustomerRequested,
        };
        assert!(step(&mut session, &revoked, ctx, None, &sink));
        assert!(!session.agent.control_granted());

        // One emit per fold: Connected, AwaitingApprovalCompare, AwaitingConsent,
        // ConsentGranted, Active, Revoked.
        assert_eq!(sink.count(), 6);
    }

    #[test]
    fn out_of_order_active_fails_closed_and_is_terminal() {
        // `Active` straight out of the announced/dialing state must never
        // fabricate control: the adapter rejects it and `step` turns the refusal
        // into a terminal failure.
        let mut session = dialing_session();
        let sink = RecordingSink::default();
        let ctx = EventContext {
            session: Some(SessionId::new()),
        };

        assert!(step(&mut session, &SessionEvent::Active, ctx, None, &sink));
        assert!(matches!(session.agent.phase(), AgentPhase::Failed { .. }));
        assert!(!session.agent.control_granted());
        let view = sink.last().expect("a terminal view was emitted");
        assert!(!view.status.control_active);
        assert_eq!(sink.count(), 1);
    }

    #[test]
    fn consent_without_a_session_id_fails_closed() {
        // A `ConsentGranted` with no negotiated session id to bind must be
        // refused, leaving no control (REQ-F-016).
        let mut session = dialing_session();
        let sink = RecordingSink::default();
        let empty = EventContext { session: None };

        assert!(!step(
            &mut session,
            &SessionEvent::Connected,
            empty,
            None,
            &sink
        ));
        assert!(!step(
            &mut session,
            &SessionEvent::AwaitingApprovalCompare,
            empty,
            None,
            &sink,
        ));
        assert!(!step(
            &mut session,
            &SessionEvent::AwaitingConsent,
            empty,
            None,
            &sink,
        ));
        // No session id in context: the fold fails closed and is terminal.
        assert!(step(&mut session, &consent_event(), empty, None, &sink));
        assert!(matches!(session.agent.phase(), AgentPhase::Failed { .. }));
        assert!(!session.agent.control_granted());
    }

    #[test]
    fn command_started_taps_a_started_card_without_touching_the_lifecycle_fold() {
        // The activity mirror is additive: a `CommandStarted` produces exactly one
        // `started` card, yet the shared `apply_session_event` fold ignores it, so
        // the security-relevant phase is neither advanced nor fabricated (ADR-0011,
        // REQ-S-002). The attribution chip comes straight off the wire `origin`.
        let mut session = dialing_session();
        let before = format!("{:?}", session.agent.phase());
        let sink = RecordingSink::default();
        let ctx = EventContext {
            session: Some(SessionId::new()),
        };

        let terminal = step(
            &mut session,
            &SessionEvent::CommandStarted {
                job: JobId(7),
                shell: Shell::PowerShell,
                command: "Get-Process".to_string(),
                origin: Origin::Mcp,
            },
            ctx,
            None,
            &sink,
        );

        // The lifecycle fold is untouched: not terminal, phase unchanged, no control.
        assert!(!terminal, "an ignored activity event is never terminal");
        assert_eq!(
            before,
            format!("{:?}", session.agent.phase()),
            "the lifecycle fold must be unchanged by the activity tap",
        );
        assert!(!session.agent.control_granted());

        // Exactly one `started` card, carrying the command, shell, and origin.
        let deltas = sink.activities();
        assert_eq!(deltas.len(), 1);
        let delta = &deltas[0];
        assert_eq!(delta.job, 7);
        assert_eq!(delta.kind, ActivityKind::Started);
        assert_eq!(delta.command.as_deref(), Some("Get-Process"));
        assert_eq!(delta.shell.as_deref(), Some("PowerShell"));
        assert_eq!(delta.origin, Some(Origin::Mcp));
        // A start carries none of the terminal fields yet.
        assert!(delta.exit_code.is_none());
        assert!(delta.outcome.is_none());
        assert!(delta.ok.is_none());
        // The view is still re-emitted exactly once per fold (unchanged behaviour).
        assert_eq!(sink.count(), 1);
    }

    #[test]
    fn exec_finished_taps_a_finished_card_with_the_honest_outcome() {
        // A finish closes the card with the real exit code and outcome; the `ok`
        // flag reflects only a clean `Completed`, so a failure is never dressed up
        // as success (REQ-N-008).
        let mut session = dialing_session();
        let sink = RecordingSink::default();
        let ctx = EventContext {
            session: Some(SessionId::new()),
        };

        assert!(!step(
            &mut session,
            &SessionEvent::ExecFinished {
                job: JobId(7),
                exit_code: Some(0),
                outcome: JobOutcome::Completed,
            },
            ctx,
            None,
            &sink,
        ));
        assert!(!step(
            &mut session,
            &SessionEvent::ExecFinished {
                job: JobId(8),
                exit_code: Some(1),
                outcome: JobOutcome::Failed,
            },
            ctx,
            None,
            &sink,
        ));

        let deltas = sink.activities();
        assert_eq!(deltas.len(), 2);

        assert_eq!(deltas[0].kind, ActivityKind::Finished);
        assert_eq!(deltas[0].exit_code, Some(0));
        assert_eq!(deltas[0].outcome.as_deref(), Some("completed"));
        assert_eq!(deltas[0].ok, Some(true));
        // A finish carries none of the start-only fields.
        assert!(deltas[0].command.is_none());
        assert!(deltas[0].shell.is_none());
        assert!(deltas[0].origin.is_none());

        assert_eq!(deltas[1].exit_code, Some(1));
        assert_eq!(deltas[1].outcome.as_deref(), Some("failed"));
        assert_eq!(deltas[1].ok, Some(false));
    }

    #[test]
    fn unknown_outcome_is_mirrored_honestly_never_a_false_success() {
        // A connection lost mid-job yields `Unknown` with no exit code; the mirror
        // surfaces it as-is and never marks it ok (REQ-N-008).
        let delta = activity_delta(&SessionEvent::ExecFinished {
            job: JobId(3),
            exit_code: None,
            outcome: JobOutcome::Unknown,
        })
        .expect("ExecFinished always produces a finished card");
        assert_eq!(delta.kind, ActivityKind::Finished);
        assert_eq!(delta.outcome.as_deref(), Some("unknown"));
        assert_eq!(delta.ok, Some(false));
        assert!(delta.exit_code.is_none());
    }

    #[test]
    fn lifecycle_desktop_and_console_only_events_produce_no_card() {
        // Only the two events the customer side actually emits become cards.
        // Lifecycle transitions carry no job, and the per-line output signals are
        // console-side only (see `crates/session/src/runtime.rs`), so tapping them
        // here would fabricate activity the customer never received.
        assert!(activity_delta(&SessionEvent::Connected).is_none());
        assert!(activity_delta(&SessionEvent::AwaitingApprovalCompare).is_none());
        assert!(activity_delta(&SessionEvent::AwaitingConsent).is_none());
        assert!(activity_delta(&SessionEvent::Active).is_none());
        assert!(
            activity_delta(&SessionEvent::ExecOutput {
                job: JobId(1),
                stream: OutputStream::Stdout,
                bytes: vec![b'x'],
            })
            .is_none(),
            "per-line output is console-side only, never mirrored here",
        );
        assert!(
            activity_delta(&SessionEvent::ExecOutputDropped {
                job: JobId(1),
                bytes: 10,
            })
            .is_none(),
            "drop markers are console-side only, never mirrored here",
        );
    }

    #[test]
    fn the_mirror_is_empty_with_no_folded_activity() {
        // With no active session and no events folded, the activity stream is
        // empty — the mirror never invents commands (fail-closed).
        let sink = RecordingSink::default();
        assert!(sink.activities().is_empty());
    }
}
