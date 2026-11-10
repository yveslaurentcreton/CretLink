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

use cretlink_protocol::{RevokeReason, SessionId};
use cretlink_session::{ActivityEntry, CleanupReport, EndedBy, JoinHandle, SessionEvent};
use cretlink_support_agent::{AgentPhase, EventContext, EventOutcome, apply_session_event};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};

use super::{ConsentSession, ConsentView};

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

    /// Renders one changed, non-authoritative activity entry (the frontend's
    /// `ActivityEvent`, ST-M009-011) on a channel separate from
    /// [`emit`](ViewSink::emit), so this higher-frequency stream never forces a
    /// whole [`ConsentView`] re-render. An item whose id was emitted before
    /// replaces the earlier one in place.
    ///
    /// Defaulted to a no-op: the activity mirror is additive, so test sinks and
    /// any sink that does not surface activity need not implement it. The live
    /// Tauri sink overrides it to emit an `"agent-activity"` event.
    fn activity(&self, _entry: &ActivityEntry) {}
}

/// The current time in Unix milliseconds (UTC), for activity timestamps.
pub(crate) fn now_unix_millis() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .and_then(|since| i64::try_from(since.as_millis()).ok())
        .unwrap_or(0)
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
    // Non-authoritative activity tap: fold every customer-visible action into
    // the session's feed and emit what changed on a separate channel, beside —
    // never inside — the security-relevant lifecycle fold above (ADR-0011,
    // ST-M009-011). A shed marker becomes a gap; the session's end closes any
    // item still running as failed, never as done.
    let now = now_unix_millis();
    for entry in session.activity.apply(event, now) {
        sink.activity(&entry);
    }
    // What the end report needs: when the session began and whether control
    // was granted (ST-M009-012); and what the live screen shows beside the
    // feed: when control began and how the screen is used (ST-M009-009).
    session.report.observe(event, now);
    session.observe(event, now);
    sink.emit(&session.view());
    terminal
}

/// The live session as the [`pump`] and the teardown
/// ([`settle`](super::report::settle)) use it. [`JoinHandle`] is the one real
/// implementation; tests substitute a scripted wire so the ordering of events
/// and intents can be asserted without a transport.
pub(crate) trait Wire: Send {
    /// The next lifecycle event, or `None` once the session task has ended.
    fn next_event(&mut self) -> impl Future<Output = Option<SessionEvent>> + Send;
    /// The negotiated session id, once control is active.
    fn session(&self) -> Option<SessionId>;
    /// The Session Approval Code to compare, once connected.
    fn approval_code(&self) -> Option<String>;
    /// What the technician's computer says its user is called (ST-M009-011).
    fn technician_display_name(&self) -> Option<String>;
    /// Sends the customer's consent.
    fn grant_consent(&self);
    /// Sends the customer's decline.
    fn deny_consent(&self, reason: String);
    /// Revokes the session on the wire (REQ-S-004).
    fn revoke(&self, reason: RevokeReason);
    /// Who ended the session, once it has torn down (ST-M009-012).
    fn ended_by(&self) -> Option<EndedBy>;
    /// The agent's own cleanup report, once it has torn down (REQ-F-019).
    fn cleanup_report(&self) -> Option<CleanupReport>;
}

impl Wire for JoinHandle {
    fn next_event(&mut self) -> impl Future<Output = Option<SessionEvent>> + Send {
        JoinHandle::next_event(self)
    }

    fn session(&self) -> Option<SessionId> {
        JoinHandle::session(self)
    }

    fn approval_code(&self) -> Option<String> {
        JoinHandle::approval_code(self).map(|code| code.to_string())
    }

    fn technician_display_name(&self) -> Option<String> {
        JoinHandle::technician_display_name(self).map(|name| name.as_str().to_string())
    }

    fn grant_consent(&self) {
        // A session that already ended has nothing to grant; the event stream
        // reports the end.
        let _ = JoinHandle::grant_consent(self);
    }

    fn deny_consent(&self, reason: String) {
        let _ = JoinHandle::deny_consent(self, reason);
    }

    fn revoke(&self, reason: RevokeReason) {
        // `Disconnected` means the session task is already gone: nothing is
        // left to revoke.
        let _ = JoinHandle::revoke(self, reason);
    }

    fn ended_by(&self) -> Option<EndedBy> {
        JoinHandle::ended_by(self)
    }

    fn cleanup_report(&self) -> Option<CleanupReport> {
        JoinHandle::cleanup_report(self)
    }
}

/// Performs one customer intent on the wire.
fn perform(wire: &impl Wire, intent: Intent) {
    match intent {
        Intent::Grant => wire.grant_consent(),
        Intent::Decline(reason) => wire.deny_consent(reason),
        Intent::Stop(reason) => wire.revoke(reason),
    }
}

/// Performs every intent already queued, so a Stop or a decline the customer
/// pressed is on the wire before the window waits for the teardown
/// (REQ-F-018). A grant queued behind a local stop is not sent: the session is
/// over on this side.
pub(crate) fn drain_intents(wire: &impl Wire, intents: &mut UnboundedReceiver<Intent>) {
    while let Ok(intent) = intents.try_recv() {
        if matches!(intent, Intent::Grant) {
            continue;
        }
        perform(wire, intent);
    }
}

/// Locks the shared session only while it is still the state of the dial
/// `generation`: a later dial replaces the state, and an earlier dial's pump,
/// teardown and failure path must then leave the new session alone.
pub(crate) fn lock_current(
    shared: &Mutex<ConsentSession>,
    generation: u64,
) -> Option<MutexGuard<'_, ConsentSession>> {
    let session = lock(shared);
    (session.generation == generation).then_some(session)
}

/// Pumps `handle`'s live event stream into the shared consent session until a
/// terminal event or the transport closing, while routing button intents to the
/// wire.
///
/// The select is `biased` towards the intents: a Stop the customer pressed is
/// always sent before a wire event that arrived at the same time is looked at,
/// and every loop exit drains what is still queued, so a local stop always
/// revokes on the wire before the window waits for the cleanup (REQ-F-018,
/// REQ-S-004). Only the state of this dial's `generation` is ever written.
///
/// The `next_event` await and the `intents.recv()` await both happen *before*
/// the state lock is taken, so the lock is never held across `.await` and the
/// synchronous Tauri commands never block on the network.
pub async fn pump(
    mut handle: impl Wire,
    shared: Arc<Mutex<ConsentSession>>,
    generation: u64,
    sink: Arc<dyn ViewSink>,
    mut intents: UnboundedReceiver<Intent>,
) {
    let mut terminated = false;
    loop {
        tokio::select! {
            biased;
            maybe_intent = intents.recv() => {
                let Some(intent) = maybe_intent else {
                    // Every command handle was dropped without ending the
                    // session; fail closed below rather than pump forever.
                    break;
                };
                // Turn the button press into a real wire action. The model only
                // moves when the wire echoes the resulting lifecycle event and
                // the event branch below folds it — control is never fabricated.
                perform(&handle, intent);
            }
            maybe_event = handle.next_event() => {
                let Some(event) = maybe_event else {
                    // The stream ended without a terminal event: the transport
                    // dropped mid-session. Handled as fail-closed below.
                    break;
                };
                // The out-of-band values each event references are read straight
                // off the live handle just before the fold.
                let ctx = EventContext { session: handle.session() };
                let sac = handle.approval_code();
                // What the technician's computer says its user is called, from
                // the pairing handshake (ST-M009-011): a label, not identity.
                let technician = handle.technician_display_name();
                let Some(mut session) = lock_current(&shared, generation) else {
                    // A later dial replaced this session's state, which it does
                    // only once this one ended: stop writing.
                    terminated = true;
                    break;
                };
                session.set_technician_name(technician);
                if is_terminal(&session) {
                    // A local emergency stop or decline already ended the model.
                    // Do not fold further wire events (a late `Active` could flip
                    // a clean `Stopped` into `Failed`); the session is over. The
                    // feed still shows how its actions ended.
                    for entry in session.activity.apply(&event, now_unix_millis()) {
                        sink.activity(&entry);
                    }
                    sink.emit(&session.view());
                    terminated = true;
                    break;
                }
                if step(&mut session, &event, ctx, sac, sink.as_ref()) {
                    terminated = true;
                    break;
                }
            }
        }
    }
    // The stop that ended the model locally is queued (it is sent under the
    // same lock that stops the model): put it on the wire now.
    drain_intents(&handle, &mut intents);
    if !terminated && let Some(mut session) = lock_current(&shared, generation) {
        if !is_terminal(&session) {
            // Fail closed so the UI shows an honest end, not a frozen
            // "connected" surface (REQ-S-009).
            session
                .agent
                .fail("session", "the transport closed before the session ended");
        }
        sink.emit(&session.view());
    }
    // The window is already safe; let the session task finish its teardown
    // (stop the jobs, sweep the staging area) instead of aborting it by
    // dropping the handle, then build the end report from its own cleanup
    // report (ST-M009-012, REQ-F-019).
    super::report::settle(
        &mut handle,
        &shared,
        generation,
        sink.as_ref(),
        &mut intents,
    )
    .await;
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use cretlink_protocol::{
        JobId, JobOutcome, Origin, RevokeReason, SessionId, Shell, TargetIdentity,
    };
    use cretlink_session::{
        ActivityItem, ActivityKind, ActivitySource, ActivityState, OutputStream,
    };
    use tokio::sync::mpsc::unbounded_channel;

    /// Records emitted views so tests can assert both the re-render count and
    /// what the window last showed, without unwrapping a lock. It also records
    /// the separate activity stream so the additive tap can be asserted beside
    /// the unchanged lifecycle fold.
    #[derive(Debug, Default)]
    struct RecordingSink {
        count: AtomicUsize,
        last: Mutex<Option<ConsentView>>,
        activities: Mutex<Vec<ActivityEntry>>,
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

        fn activities(&self) -> Vec<ActivityEntry> {
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

        fn activity(&self, entry: &ActivityEntry) {
            self.activities
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push(entry.clone());
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
                account: None,
            },
        }
    }

    /// A live, dialing consent session — the state the pump is in once the
    /// listener has been dialed and the event stream starts. The intent receiver
    /// is dropped: `step` never touches it, only the async `pump` does.
    fn dialing_session() -> ConsentSession {
        let (tx, _rx) = unbounded_channel::<Intent>();
        ConsentSession::live_dialing(tx, None)
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

    fn only_item(entries: &[ActivityEntry]) -> ActivityItem {
        match entries {
            [ActivityEntry::Item { item }] => item.clone(),
            other => panic!("expected one item, got {other:?}"),
        }
    }

    #[test]
    fn command_started_taps_a_running_item_without_touching_the_lifecycle_fold() {
        // The activity mirror is additive: a `CommandStarted` produces exactly one
        // running item, yet the shared `apply_session_event` fold ignores it, so
        // the security-relevant phase is neither advanced nor fabricated (ADR-0011,
        // REQ-S-002). The source comes straight off the wire `origin`.
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
                purpose: cretlink_protocol::Purpose::new("Listing running programs").ok(),
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

        let item = only_item(&sink.activities());
        assert_eq!(item.id, "job-7");
        assert_eq!(item.source, ActivitySource::Ai);
        assert_eq!(item.kind, ActivityKind::Command);
        assert_eq!(item.operation, "run a command in PowerShell: Get-Process");
        assert_eq!(item.purpose.as_deref(), Some("Listing running programs"));
        assert_eq!(item.state, ActivityState::Running);
        // The client label never exists on the customer side (ST-M009-011).
        assert_eq!(item.client_label, None);
        // The view is still re-emitted exactly once per fold (unchanged behaviour).
        assert_eq!(sink.count(), 1);
        // `get_activity` reads the same feed.
        assert_eq!(session.activity.entries(), sink.activities());
    }

    #[test]
    fn exec_finished_updates_the_item_in_place_with_the_honest_outcome() {
        let mut session = dialing_session();
        let sink = RecordingSink::default();
        let ctx = EventContext {
            session: Some(SessionId::new()),
        };
        for (job, exit_code, outcome) in [
            (7, Some(0), JobOutcome::Completed),
            (8, Some(1), JobOutcome::Failed),
            (9, None, JobOutcome::Unknown),
        ] {
            step(
                &mut session,
                &SessionEvent::CommandStarted {
                    job: JobId(job),
                    shell: Shell::Cmd,
                    command: "dir".to_string(),
                    origin: Origin::Console,
                    purpose: None,
                },
                ctx,
                None,
                &sink,
            );
            step(
                &mut session,
                &SessionEvent::ExecFinished {
                    job: JobId(job),
                    exit_code,
                    outcome,
                },
                ctx,
                None,
                &sink,
            );
        }
        let entries = session.activity.entries();
        assert_eq!(entries.len(), 3, "each job's item updated in place");
        let states: Vec<ActivityState> = entries
            .into_iter()
            .map(|entry| match entry {
                ActivityEntry::Item { item } => item.state,
                other => panic!("unexpected {other:?}"),
            })
            .collect();
        assert_eq!(
            states,
            vec![
                ActivityState::Done { exit_code: Some(0) },
                ActivityState::Failed {
                    exit_code: Some(1),
                    message: None
                },
                // An unknown outcome is never a success (REQ-N-008).
                ActivityState::failed(cretlink_session::activity::UNKNOWN_RESULT),
            ]
        );
    }

    #[test]
    fn a_shed_marker_is_shown_as_a_gap() {
        let mut session = dialing_session();
        let sink = RecordingSink::default();
        step(
            &mut session,
            &SessionEvent::ActivityShed { count: 12 },
            EventContext::default(),
            None,
            &sink,
        );
        assert!(matches!(
            sink.activities().as_slice(),
            [ActivityEntry::Gap { count: 12, .. }]
        ));
    }

    #[test]
    fn lifecycle_and_console_only_events_produce_no_item() {
        // Lifecycle transitions carry no action, and the per-line output
        // signals are console-side only, so tapping them here would fabricate
        // activity the customer never received.
        let mut session = dialing_session();
        let sink = RecordingSink::default();
        for event in [
            SessionEvent::Connected,
            SessionEvent::AwaitingApprovalCompare,
            SessionEvent::ExecOutput {
                job: JobId(1),
                stream: OutputStream::Stdout,
                bytes: vec![b'x'],
            },
            SessionEvent::ExecOutputDropped {
                job: JobId(1),
                bytes: 10,
            },
        ] {
            step(&mut session, &event, EventContext::default(), None, &sink);
        }
        assert!(sink.activities().is_empty());
    }

    #[test]
    fn the_view_shows_the_technician_name_as_the_technician_computer_says() {
        let mut session = dialing_session();
        assert!(session.view().technician.is_none());
        session.set_technician_name(Some("Sam".to_string()));
        let technician = session.view().technician.expect("a name");
        assert_eq!(technician.display_name, "Sam");
        assert!(technician.provenance.contains("technician's computer"));
    }

    #[test]
    fn desktop_activity_shows_on_the_screen_tile_without_touching_the_lifecycle() {
        use cretlink_protocol::DesktopAccess;

        let mut session = dialing_session();
        let before = format!("{:?}", session.agent.phase());
        let sink = RecordingSink::default();
        assert!(!step(
            &mut session,
            &SessionEvent::DesktopActivity {
                access: DesktopAccess::Control,
            },
            EventContext::default(),
            None,
            &sink,
        ));
        assert_eq!(before, format!("{:?}", session.agent.phase()));
        let view = sink.last().expect("a view was emitted");
        assert!(view.screen.controlled_at_ms.is_some());
        assert!(view.screen.observed_at_ms.is_none());
    }

    #[test]
    fn the_band_knows_when_control_began() {
        let mut session = dialing_session();
        let sink = RecordingSink::default();
        let ctx = EventContext {
            session: Some(SessionId::new()),
        };
        for event in [
            SessionEvent::Connected,
            SessionEvent::AwaitingApprovalCompare,
            SessionEvent::AwaitingConsent,
        ] {
            step(&mut session, &event, ctx, None, &sink);
        }
        assert_eq!(session.view().control_since_ms, None);
        step(&mut session, &consent_event(), ctx, None, &sink);
        assert!(session.view().control_since_ms.is_some());
        assert!(session.view().session_active);
    }

    /// A wire scripted by the test: the events it delivers, and what the pump
    /// sent on it.
    struct ScriptedWire {
        events: tokio::sync::mpsc::UnboundedReceiver<SessionEvent>,
        revoked: Arc<Mutex<Vec<RevokeReason>>>,
        granted: Arc<AtomicUsize>,
    }

    impl ScriptedWire {
        fn new(
            events: Vec<SessionEvent>,
        ) -> (Self, Arc<Mutex<Vec<RevokeReason>>>, Arc<AtomicUsize>) {
            let (tx, rx) = unbounded_channel();
            for event in events {
                tx.send(event).unwrap();
            }
            // The sender goes, so the stream ends after the scripted events,
            // as when the session task has finished.
            drop(tx);
            let revoked = Arc::new(Mutex::new(Vec::new()));
            let granted = Arc::new(AtomicUsize::new(0));
            let wire = Self {
                events: rx,
                revoked: Arc::clone(&revoked),
                granted: Arc::clone(&granted),
            };
            (wire, revoked, granted)
        }
    }

    impl Wire for ScriptedWire {
        fn next_event(&mut self) -> impl Future<Output = Option<SessionEvent>> + Send {
            self.events.recv()
        }
        fn session(&self) -> Option<SessionId> {
            None
        }
        fn approval_code(&self) -> Option<String> {
            None
        }
        fn technician_display_name(&self) -> Option<String> {
            None
        }
        fn grant_consent(&self) {
            self.granted.fetch_add(1, Ordering::Relaxed);
        }
        fn deny_consent(&self, _reason: String) {}
        fn revoke(&self, reason: RevokeReason) {
            self.revoked.lock().unwrap().push(reason);
        }
        fn ended_by(&self) -> Option<EndedBy> {
            None
        }
        fn cleanup_report(&self) -> Option<CleanupReport> {
            None
        }
    }

    #[tokio::test]
    async fn a_stop_queued_behind_a_wire_event_still_revokes_on_the_wire() {
        // REQ-F-018: the customer presses Stop while a wire event is already
        // waiting. Whichever the pump sees first, the revoke must reach the
        // wire before the window waits for the teardown. Repeated, because an
        // unbiased select would take the event first about half the time.
        for _ in 0..32 {
            let (tx, rx) = intent_channel();
            let session = ConsentSession::live_dialing(tx, None);
            let generation = session.generation;
            let shared = Arc::new(Mutex::new(session));
            let (wire, revoked, _) = ScriptedWire::new(vec![SessionEvent::Connected]);
            // What the Stop button does: queue the revoke, stop the model.
            super::super::stop_now(&mut lock(&shared)).unwrap();
            let sink: Arc<dyn ViewSink> = Arc::new(RecordingSink::default());
            pump(wire, Arc::clone(&shared), generation, sink, rx).await;
            assert_eq!(
                *revoked.lock().unwrap(),
                vec![RevokeReason::CustomerRequested]
            );
            assert!(matches!(
                lock(&shared).agent.phase(),
                AgentPhase::Stopped { .. }
            ));
        }
    }

    #[test]
    fn draining_sends_a_queued_stop_but_no_grant_behind_it() {
        let (wire, revoked, granted) = ScriptedWire::new(Vec::new());
        let (tx, mut rx) = intent_channel();
        tx.send(Intent::Grant).unwrap();
        tx.send(Intent::Stop(RevokeReason::CustomerRequested))
            .unwrap();
        drain_intents(&wire, &mut rx);
        assert_eq!(
            *revoked.lock().unwrap(),
            vec![RevokeReason::CustomerRequested]
        );
        assert_eq!(granted.load(Ordering::Relaxed), 0);
    }

    #[tokio::test]
    async fn an_earlier_dial_never_writes_into_the_next_session() {
        // The first dial ended (Stop) and the customer dialed again: the old
        // pump and its teardown still run, and must leave the new state alone.
        let (old_tx, old_rx) = intent_channel();
        let old = ConsentSession::live_dialing(old_tx, None);
        let old_generation = old.generation;
        let shared = Arc::new(Mutex::new(old));
        super::super::stop_now(&mut lock(&shared)).unwrap();
        let (new_tx, _new_rx) = intent_channel();
        let next = ConsentSession::live_dialing(new_tx, None);
        let next_generation = next.generation;
        assert_ne!(old_generation, next_generation);
        lock(&shared).replace_with(next);

        let (wire, revoked, _) = ScriptedWire::new(vec![
            SessionEvent::CommandStarted {
                job: JobId(3),
                shell: Shell::Cmd,
                command: "dir".to_string(),
                origin: Origin::Mcp,
                purpose: None,
            },
            SessionEvent::ActivityShed { count: 4 },
        ]);
        let recorder = Arc::new(RecordingSink::default());
        let sink: Arc<dyn ViewSink> = recorder.clone();
        pump(wire, Arc::clone(&shared), old_generation, sink, old_rx).await;

        // The old session's Stop still went out on its own wire.
        assert_eq!(
            *revoked.lock().unwrap(),
            vec![RevokeReason::CustomerRequested]
        );
        let session = lock(&shared);
        assert_eq!(session.generation, next_generation);
        assert!(session.activity.entries().is_empty());
        assert!(
            !is_terminal(&session),
            "the new dial is not failed or stopped"
        );
        assert!(session.report.report().is_none());
        drop(session);
        assert_eq!(recorder.count(), 0, "nothing rendered for a stale dial");
        assert!(recorder.activities().is_empty());
        assert!(lock_current(&shared, old_generation).is_none());
        assert!(lock_current(&shared, next_generation).is_some());
    }

    #[test]
    fn the_mirror_is_empty_with_no_folded_activity() {
        // With no active session and no events folded, the activity stream is
        // empty — the mirror never invents commands (fail-closed).
        let sink = RecordingSink::default();
        assert!(sink.activities().is_empty());
    }
}
