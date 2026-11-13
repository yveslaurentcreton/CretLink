//! Live-transport driver for the console GUI (ST-M002-009).
//!
//! This module folds a real `cretlink-session` [`SessionEvent`] stream from a
//! listening [`HostHandle`] into the shared [`ConsoleState`], re-emitting the
//! whole state after every fold so the Tauri frontend re-renders. It reuses the
//! cross-platform, unit-tested [`cretlink_console::apply_session_event`] adapter
//! (ADR-0011): the security-relevant state transitions live there. This module
//! only owns the transport plumbing — building the out-of-band [`EventContext`]
//! from the live handle and pumping the async stream — so no control decision
//! is duplicated here.
//!
//! It is deliberately fail-closed. A refused fold, or a transport that closes
//! before a terminal event, drives the model to [`Failed`](
//! cretlink_console::SessionPhase::Failed); no dropped connection can leave a
//! stale "under control" surface on screen (REQ-S-009).

use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use cretlink_console::{
    AuditServiceConfig, ChannelAudit, ConsoleRoute, ConsoleSessionService, ConsoleState,
    EventContext, EventOutcome, IpcAudit, IpcServer, LiveEcho, SHUTDOWN_GRACE, SessionAuditStart,
    SessionPhase, TargetId, TechnicianActivity, apply_session_event, open_session_log,
};
use cretlink_mcp::{IpcEndpoint, IpcListener};
use cretlink_protocol::{RevokeReason, SessionId, TargetIdentity};
use cretlink_session::{ActivityEntry, HostHandle, SessionEvent};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};
use tokio::sync::{broadcast, oneshot};

use super::bridge::{BridgeSlot, BridgeStatus, lock_bridge, tool_count};

/// Where the task of a session writes outside this process: the audit folder,
/// and the endpoint of the local MCP bridge. The application uses the user's
/// own ([`Place::of_this_user`]); a test names a folder and an endpoint of its
/// own, so that it never touches a technician's audit trail or bridge.
#[derive(Debug, Clone)]
pub(crate) struct Place {
    /// The audit service's configuration, with the folder of the session
    /// logs.
    pub(crate) audit: AuditServiceConfig,
    /// The endpoint the MCP bridge is hosted on while a session is under full
    /// control.
    pub(crate) bridge: IpcEndpoint,
}

impl Place {
    /// The audit folder and the bridge endpoint of the user who runs this
    /// copy.
    pub(crate) fn of_this_user() -> Self {
        Self {
            audit: AuditServiceConfig::default(),
            bridge: IpcEndpoint::user_default(),
        }
    }
}

/// The sender half of the button-intent channel, held by [`AppState`](super::AppState)
/// so the synchronous Tauri commands can hand a technician decision to the async
/// [`pump`] task that owns the live [`HostHandle`].
pub(crate) type IntentSender = UnboundedSender<Intent>;

/// Creates the intent channel that connects the Tauri commands (senders) to the
/// [`pump`] (the single receiver). Unbounded so a button press is never dropped
/// or blocked behind the network.
pub(crate) fn intent_channel() -> (IntentSender, UnboundedReceiver<Intent>) {
    unbounded_channel()
}

/// A shared slot holding the live [`ConsoleSessionService`] while a session is
/// under full control, so the Tauri terminal commands can reach the service the
/// [`pump`] created at hand-off (ST-M008-001). [`run_active`] fills it the moment
/// the service starts and clears it on the fail-closed teardown, so any terminal
/// command issued outside an active session finds `None` and refuses — the same
/// fail-closed contract the service itself enforces via `NoConsentedSession`.
pub(crate) type ServiceSlot = Arc<Mutex<Option<ConsoleSessionService>>>;

/// Locks the service slot, recovering from a poisoned mutex rather than
/// panicking — a technician terminal must never wedge a live session because a
/// prior task panicked while holding this lock.
pub(crate) fn lock_service(slot: &ServiceSlot) -> MutexGuard<'_, Option<ConsoleSessionService>> {
    slot.lock().unwrap_or_else(PoisonError::into_inner)
}

/// A technician decision routed from a synchronous Tauri command to the async
/// pump task that owns the live [`HostHandle`].
///
/// Every wire method needs the one live handle and [`HostHandle::next_event`]
/// borrows it mutably, so the pump owns the handle exclusively and the commands
/// send intents it performs. The compare-gate confirm/mismatch (ST-M002-013) and
/// the emergency stop (REQ-F-018) are the only technician actions that must reach
/// the live wire; consent is the customer's, and arrives over the wire.
#[derive(Debug)]
pub enum Intent {
    /// The technician confirmed the Session Approval Codes match on both screens;
    /// releases the compare gate via [`HostHandle::confirm_code`] (REQ-S-006).
    Confirm,
    /// The technician reported the codes do **not** match — a man-in-the-middle
    /// signature; tears the session down fail-closed via
    /// [`HostHandle::report_code_mismatch`] (REQ-S-009).
    Mismatch,
    /// The emergency stop / revoke (REQ-F-018, REQ-S-004).
    Stop(RevokeReason),
}

/// The current session's technician activity feed (ST-M009-011), shared by the
/// pump that folds into it and the `get_activity` command that reads it. It
/// survives the end of a session and is replaced only when the next session
/// starts, so the end screen and a reloaded frontend still see what happened.
pub(crate) type ActivitySlot = Arc<Mutex<TechnicianActivity>>;

/// Locks the activity feed, recovering from a poisoned mutex rather than
/// panicking (panics are denied in library code).
pub(crate) fn lock_activity(slot: &ActivitySlot) -> MutexGuard<'_, TechnicianActivity> {
    slot.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Emits the current bridge status, its lock released first.
fn emit_bridge(bridge: &BridgeSlot, sink: &dyn StateSink) {
    let status = lock_bridge(bridge).status(tool_count());
    sink.emit_bridge(&status);
}

/// The current time in Unix milliseconds (UTC), for activity timestamps.
pub(crate) fn now_unix_millis() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .and_then(|since| i64::try_from(since.as_millis()).ok())
        .unwrap_or(0)
}

/// Folds one live audit echo into the technician feed (ST-M009-011): first a
/// gap for the echoes the bounded live channel dropped right before it, at the
/// place they were dropped (REQ-N-008), then the record's item, when the record
/// is an action on the customer computer. Returns the entries to emit, in
/// order. Every attributable source is covered — the AI (with its client
/// label), the technician terminal, and manual console actions — because the
/// echo carries every such audit actor.
pub(crate) fn fold_live_echo(
    activity: &mut TechnicianActivity,
    echo: &LiveEcho,
    now: i64,
) -> Vec<ActivityEntry> {
    let mut changed = Vec::new();
    if echo.shed_before > 0 {
        changed.push(activity.shed(echo.shed_before, now));
    }
    changed.extend(activity.apply(&echo.entry, now));
    changed
}

/// The feed's honest end (ST-M009-011, REQ-N-008), once the bridge endpoint
/// has shut down: first the echoes still queued — the completion records the
/// shutdown let in-flight calls write — then a trailing gap for the echoes
/// dropped after the last one delivered (`shed`), then every action still
/// running is shown as ended with the session, never as done. Returns the
/// entries to emit, in order.
pub(crate) fn end_of_feed(
    activity: &mut TechnicianActivity,
    queued: &[LiveEcho],
    shed: u64,
    now: i64,
) -> Vec<ActivityEntry> {
    let mut changed = Vec::new();
    for echo in queued {
        changed.extend(fold_live_echo(activity, echo, now));
    }
    if shed > 0 {
        changed.push(activity.shed(shed, now));
    }
    changed.extend(activity.session_ended(now));
    changed
}

/// Receives whole-state snapshots to render.
///
/// The GUI implements this over a Tauri `AppHandle` (emitting a `"console-state"`
/// event); tests implement it with an in-memory collector so [`step`] is
/// verifiable without a transport.
pub trait StateSink: Send + Sync + 'static {
    /// Renders one snapshot of the console state.
    fn emit(&self, state: &ConsoleState);
    /// Renders one changed activity entry (an `ActivityEvent` in the frontend
    /// contract, ST-M009-011) on a separate channel. Defaults to a no-op so
    /// in-memory test sinks need not implement it. The GUI sink emits a
    /// `"console-activity"` Tauri event; routing activity apart from whole-state
    /// snapshots means a burst of actions never triggers a full
    /// `console-state` re-emit (REQ-N-006). An item whose id was emitted before
    /// replaces the earlier one in place.
    fn emit_activity(&self, _entry: &ActivityEntry) {}
    /// Renders a changed bridge status for the rail (ST-M009-008) on its own
    /// `"console-bridge"` channel. Defaults to a no-op for test sinks.
    fn emit_bridge(&self, _status: &BridgeStatus) {}
}

/// Locks the shared state, recovering the inner value if a prior holder panicked
/// (panics are denied in library code, so this is defence in depth, never an
/// expected path).
pub(crate) fn lock(shared: &Mutex<ConsoleState>) -> MutexGuard<'_, ConsoleState> {
    shared.lock().unwrap_or_else(PoisonError::into_inner)
}

/// True once the console model has reached a terminal phase and must not be
/// folded further. A local emergency stop moves the phase to `Stopped` before
/// the wire echoes its `Revoked`; without this guard a late event folded on top
/// would flip a clean `Stopped` into `Failed`.
pub(crate) fn is_terminal(state: &ConsoleState) -> bool {
    matches!(
        state.phase,
        SessionPhase::Stopped { .. } | SessionPhase::Failed { .. }
    )
}

/// Folds one observed event into `state`, emits the resulting snapshot, and
/// reports whether the session reached a terminal phase.
///
/// A refused fold is fail-closed: the state is driven to `Failed` (so no
/// authority can silently survive a malformed or out-of-order stream) and the
/// terminal snapshot is emitted. Returns `true` when the pump should stop.
pub fn step(
    state: &mut ConsoleState,
    event: &SessionEvent,
    ctx: EventContext<'_>,
    sink: &dyn StateSink,
) -> bool {
    // The console listens rather than dials (ST-M002-012), so the first honest
    // sign of a customer is `Connected` — emitted only after the Noise handshake,
    // with the approval code already set. Materialise the incoming target
    // just-in-time so the unchanged `Connecting → …` fold below applies. The CLI
    // path is already in `Connecting`, so this is a no-op there.
    if matches!(event, SessionEvent::Connected)
        && matches!(state.phase, SessionPhase::Listening { .. })
        && let Err(error) = state.customer_arrived(TargetId::new(), "Incoming customer computer")
    {
        // An arrival that cannot be recorded is fail-closed, not ignored.
        state.fail("session", error.to_string());
        sink.emit(state);
        return true;
    }

    let terminal = match apply_session_event(state, event, ctx) {
        Ok(outcome) => matches!(outcome, EventOutcome::Terminated),
        Err(error) => {
            // The adapter left the state untouched on rejection; surface the
            // refusal as an explicit terminal failure rather than silently
            // continuing to pump an inconsistent stream.
            state.fail("session", error.to_string());
            true
        }
    };
    sink.emit(state);
    terminal
}

/// Bound of the live-echo channel that forwards bridge-originated audit
/// start-lines to the Activity feed. Matches the console CLI's `LIVE_ECHO_CAPACITY`
/// (main.rs): a short burst of MCP calls is buffered, and the echo is
/// non-blocking so a slow renderer can never stall the audit path.
const LIVE_ECHO_CAPACITY: usize = 256;

/// Pumps `handle`'s live event stream into the shared console state until full
/// control is reached, then hands the live handle to the hosted session service
/// (ST-M004-007). Before full control this behaves exactly as the original
/// lifecycle pump; the difference is the handoff at `full_control`.
///
/// The out-of-band values each event references (the Session Approval Code and
/// the negotiated session id) are read straight off the live handle just before
/// each fold, exactly as the console CLI's report loop does. The `next_event`
/// and `intents.recv()` awaits both happen *before* the state lock is taken, so
/// the lock is never held across `.await` and the synchronous Tauri commands
/// never block on the network.
///
/// The pump also [`select!`](tokio::select)s on the technician's button intents
/// (compare confirm/mismatch and the emergency stop), performing the matching
/// wire action on the handle it exclusively owns. The console phase only moves
/// when the wire echoes the resulting lifecycle event and the event branch folds
/// it — a confirm releases the compare gate but full control still appears only
/// when the customer's real `ConsentGranted` echoes back (REQ-S-001).
///
/// Along the way it performs the audit handshake the console CLI does: the first
/// time the negotiated session id is known it opens the durable, hash-chained
/// session log and hands the audit service its start record over `start_tx`.
/// The GUI has no stdin, so — unlike the CLI's interactive prompt — a log that
/// cannot be opened is fail-closed: the session is revoked and the model driven
/// to `Failed` rather than run unaudited (REQ-F-017).
#[allow(clippy::too_many_arguments)]
pub async fn pump(
    mut handle: HostHandle,
    shared: Arc<Mutex<ConsoleState>>,
    sink: Arc<dyn StateSink>,
    mut intents: UnboundedReceiver<Intent>,
    audit: ChannelAudit,
    place: Place,
    start_tx: oneshot::Sender<SessionAuditStart>,
    service_slot: ServiceSlot,
    activity: ActivitySlot,
    bridge: BridgeSlot,
) {
    let Place {
        audit: audit_config,
        bridge: endpoint,
    } = place;
    // The consent-bound identity, captured from `ConsentGranted` before the fold,
    // so it can be moved into the hosted service at handoff (REQ-F-016).
    let mut identity: Option<TargetIdentity> = None;
    // Taken exactly once, the first time the session id is known.
    let mut start_tx = Some(start_tx);
    let mut terminated = false;
    loop {
        tokio::select! {
            maybe_event = handle.next_event() => {
                let Some(event) = maybe_event else {
                    // The stream ended without a terminal event: the transport
                    // dropped mid-session. Handled as fail-closed below.
                    break;
                };
                // The runtime emits `ConsentGranted` immediately before `Active`;
                // capture the honest identity now so the handoff below can bind it.
                if let SessionEvent::ConsentGranted { identity: bound } = &event {
                    identity = Some(bound.clone());
                }
                // Open the durable session log the first time the session id is
                // known and hand the audit service its start record. Fail closed
                // if it cannot be opened — the GUI cannot prompt to continue
                // unaudited the way the interactive CLI can.
                if start_tx.is_some()
                    && let Some(session) = handle.session()
                    && let Some(tx) = start_tx.take()
                {
                    match open_session_log(&audit_config, session) {
                        Ok(start) => {
                            let _ = tx.send(start);
                        }
                        Err(error) => {
                            let _ = handle.revoke(RevokeReason::TechnicianRequested);
                            let mut state = lock(&shared);
                            state.fail(
                                "audit",
                                format!("the session audit log could not be opened: {error}"),
                            );
                            sink.emit(&state);
                            return;
                        }
                    }
                }
                let code = handle.approval_code().map(|c| c.to_string());
                let ctx = EventContext {
                    approval_code: code.as_deref(),
                    session: handle.session(),
                };
                // Fold the event under the lock, then release the guard BEFORE any
                // await. The `MutexGuard` is `!Send`, so computing the decision in
                // a narrow scope (rather than holding it across the hand-off await)
                // is what keeps this spawned pump future `Send`.
                let decision = {
                    let mut state = lock(&shared);
                    if is_terminal(&state) {
                        // A local emergency stop already ended the model. Do not
                        // fold further wire events (a late event could flip a clean
                        // `Stopped` into `Failed`); the session is over.
                        sink.emit(&state);
                        FoldStep::Terminated
                    } else if step(&mut state, &event, ctx, sink.as_ref()) {
                        FoldStep::Terminated
                    } else if matches!(state.phase, SessionPhase::FullControl) {
                        // The fold reached full control: hand off below, outside the
                        // lock, to the hosted session service that drives the wire.
                        FoldStep::HandOff
                    } else {
                        FoldStep::Continue
                    }
                };
                match decision {
                    FoldStep::Terminated => {
                        terminated = true;
                        break;
                    }
                    FoldStep::Continue => {}
                    FoldStep::HandOff => {
                        let session = handle.session();
                        let target = identity.take();
                        match (session, target) {
                            (Some(session), Some(target)) => {
                                // Run the post-active loop that renders live
                                // activity (ST-M004-007); the guard is out of scope.
                                run_active(
                                    handle, session, target, shared, sink, intents,
                                    audit, service_slot, activity, bridge, endpoint,
                                )
                                .await;
                                return;
                            }
                            _ => {
                                // Full control without a session id or a
                                // consent-bound identity cannot happen on the real
                                // wire; fail closed rather than host an
                                // unattributable session.
                                let mut state = lock(&shared);
                                state.fail(
                                    "session",
                                    "full control was reached without a consent-bound identity",
                                );
                                sink.emit(&state);
                                return;
                            }
                        }
                    }
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
                    Intent::Confirm => {
                        let _ = handle.confirm_code();
                    }
                    Intent::Mismatch => {
                        let _ = handle.report_code_mismatch();
                    }
                    Intent::Stop(reason) => {
                        let _ = handle.revoke(reason);
                    }
                }
            }
        }
    }
    if !terminated {
        // The stream ended without a Revoked/Failed event: the transport dropped
        // mid-session. Fail closed so the UI shows an honest end, not a frozen
        // "connected" surface (REQ-S-009).
        let mut state = lock(&shared);
        if !is_terminal(&state) {
            state.fail("session", "the transport closed before the session ended");
        }
        sink.emit(&state);
    }
}

/// The outcome of folding one pre-active wire event under the state lock,
/// computed inside a narrow lock scope so the `!Send` [`std::sync::MutexGuard`]
/// is released before any `.await` — that is what keeps the spawned pump future
/// `Send`, which Tauri's runtime requires.
enum FoldStep {
    /// The model reached a terminal state; end the pump loop.
    Terminated,
    /// A non-terminal fold; keep pumping.
    Continue,
    /// Full control was reached; hand the live handle to the session service.
    HandOff,
}

/// Hosts the consented session after full control is reached (ST-M004-007).
///
/// This is the console GUI's equivalent of the CLI's `run_active`: it moves the
/// live [`HostHandle`] into a [`ConsoleSessionService`], so the one consented
/// GUI session is what the MCP bridge and any interactive terminal attach to,
/// then renders the shared live activity feed. It owns three sources:
///
/// - `service.subscribe()` — a `Revoked`/`Failed` lifecycle event is folded
///   through [`step`] and ends the session fail-closed.
/// - the live audit echo — every audit record of an action on the customer
///   computer (the AI, the technician terminal, manual console actions) is
///   folded into the shared [`TechnicianActivity`] feed and the changed
///   [`ActivityEntry`] is emitted the instant the call starts and again when it
///   finishes (ST-M009-011).
/// - the technician intent channel — an emergency Stop revokes the service,
///   whose `Revoked` echo the subscribe arm then folds (control is never
///   fabricated).
///
/// The local MCP bridge endpoint is bound best-effort: if a CLI console already
/// owns it the bind fails honestly and interactive control still works
/// (ST-M004-007 AC#5). On exit the service and the bridge are torn down within a
/// bounded grace window, and any non-terminal state is driven to a stopped/failed
/// surface so the UI never freezes "under control" (REQ-S-009).
#[allow(clippy::too_many_arguments)]
async fn run_active(
    handle: HostHandle,
    session: SessionId,
    target: TargetIdentity,
    shared: Arc<Mutex<ConsoleState>>,
    sink: Arc<dyn StateSink>,
    mut intents: UnboundedReceiver<Intent>,
    audit: ChannelAudit,
    service_slot: ServiceSlot,
    activity: ActivitySlot,
    bridge: BridgeSlot,
    endpoint: IpcEndpoint,
) {
    // Hand the live handle to the hosted session service; its driver task now
    // solely owns the handle and pumps every wire event into the broadcast.
    let (service, _driver) = ConsoleSessionService::start(handle, session, target);
    let mut events = service.subscribe();

    // Publish the live service so the Tauri terminal commands can open, feed and
    // resize a technician terminal on THIS consented session (ST-M008-001). The
    // slot is cleared on teardown below, so no terminal command outlives the
    // session; the service also fails closed on its own once `stop`ped.
    *lock_service(&service_slot) = Some(service.clone());

    // Echo bridge-originated audit start-lines to the Activity feed. One-shot;
    // the sender lives inside `audit`, which this function owns for its scope.
    let (live_tx, mut live_rx) = tokio::sync::mpsc::channel(LIVE_ECHO_CAPACITY);
    audit.set_live_sender(live_tx);
    let mut live_open = true;

    // Host the local MCP bridge endpoint so Claude routes through THIS consented
    // GUI session (REQ-F-013). Best-effort single-instance: a busy endpoint (a
    // CLI console already hosting) fails the bind honestly without crashing.
    let ipc_server = match IpcListener::bind(&endpoint) {
        Ok(listener) => {
            let route: Arc<dyn ConsoleRoute> = Arc::new(service.clone());
            let ipc_audit: Arc<dyn IpcAudit> = Arc::new(audit.clone());
            let in_flight = service.activity_counter();
            let (shutdown_tx, shutdown_rx) = oneshot::channel();
            Some((
                tokio::spawn(IpcServer::serve(
                    listener,
                    route,
                    ipc_audit,
                    in_flight,
                    shutdown_rx,
                )),
                shutdown_tx,
            ))
        }
        Err(error) => {
            // Not an action on the customer computer, so not an activity item;
            // the rail says so with a next step (ST-M009-008), and interactive
            // control still works without the bridge.
            eprintln!(
                "note: the MCP bridge endpoint is unavailable ({error}); interactive control still works"
            );
            lock_bridge(&bridge).failed(&error);
            None
        }
    };
    if ipc_server.is_some() {
        lock_bridge(&bridge).ready();
    }
    emit_bridge(&bridge, sink.as_ref());

    let mut terminated = false;
    loop {
        tokio::select! {
            recv = events.recv() => {
                match recv {
                    Ok(event) => {
                        if matches!(
                            event,
                            SessionEvent::Revoked { .. } | SessionEvent::Failed { .. }
                        ) {
                            // A lifecycle terminal: fold through the shared adapter
                            // so the model ends fail-closed and the terminal
                            // snapshot renders.
                            let mut state = lock(&shared);
                            if is_terminal(&state) {
                                sink.emit(&state);
                                terminated = true;
                                break;
                            }
                            let ctx = EventContext {
                                approval_code: None,
                                session: Some(session),
                            };
                            if step(&mut state, &event, ctx, sink.as_ref()) {
                                terminated = true;
                                break;
                            }
                        }
                        // Every other event is consumed elsewhere: job output by
                        // the terminal panel, and the actions themselves reach
                        // the feed from their audit records below (ST-M009-011).
                    }
                    // The consumer fell behind the bounded broadcast; some activity
                    // lines were missed, but the lifecycle stream is intact — keep
                    // rendering rather than tearing a live session down.
                    Err(broadcast::error::RecvError::Lagged(_)) => {}
                    // The driver ended (session lost or the last clone dropped);
                    // fail closed below.
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
            maybe_entry = live_rx.recv(), if live_open => {
                match maybe_entry {
                    Some(echo) => {
                        // An AI client calling for the first time this session
                        // joins the rail (ST-M009-008).
                        let new_client = lock_bridge(&bridge).note(&echo.entry);
                        if new_client {
                            emit_bridge(&bridge, sink.as_ref());
                        }
                        let changed =
                            fold_live_echo(&mut lock_activity(&activity), &echo, now_unix_millis());
                        for entry in &changed {
                            sink.emit_activity(entry);
                        }
                    }
                    // The audit sink was dropped; stop polling this branch so it
                    // does not spin. The session ends through the other arms.
                    None => live_open = false,
                }
            }
            maybe_intent = intents.recv() => {
                let Some(intent) = maybe_intent else {
                    // Every command handle was dropped; fail closed below.
                    break;
                };
                match intent {
                    // The emergency stop is the only technician action live under
                    // full control; revoke the service and let its `Revoked` echo
                    // fold through the subscribe arm (control is never fabricated).
                    Intent::Stop(reason) => service.revoke(reason),
                    // The compare-gate confirm/mismatch belong to the pre-active
                    // window and cannot occur here; ignore them defensively.
                    Intent::Confirm | Intent::Mismatch => {}
                }
            }
        }
    }

    // Fail-closed teardown. Stopping the service flips it inactive at once so no
    // routed call survives, and shutting the bridge down releases its route and
    // audit handles so the audit drain can seal the log (ST-M003-006).
    service.stop();
    // Drop the shared service so any terminal command racing in after teardown
    // finds an empty slot and refuses — defence in depth beside the service's own
    // `NoConsentedSession` fail-closed once stopped (ST-M008-001).
    *lock_service(&service_slot) = None;
    if let Some((task, shutdown_tx)) = ipc_server {
        let _ = shutdown_tx.send(());
        if tokio::time::timeout(SHUTDOWN_GRACE + Duration::from_secs(2), task)
            .await
            .is_err()
        {
            // The endpoint did not stop within its grace window; the process is
            // ending regardless, so this is a best-effort note only.
        }
    }
    // The echoes still queued: the completion records the endpoint's shutdown
    // let in-flight calls write after the loop stopped reading (ST-M009-011).
    let mut queued = Vec::new();
    if live_open {
        while let Ok(echo) = live_rx.try_recv() {
            queued.push(echo);
        }
    }
    for echo in &queued {
        // A client whose first call is among them still joins the rail.
        let _ = lock_bridge(&bridge).note(&echo.entry);
    }
    // The endpoint is released with the session (ST-M009-008).
    lock_bridge(&bridge).closed();
    emit_bridge(&bridge, sink.as_ref());
    // The feed's honest end: the queued echoes, then the trailing gap, then
    // what is still running ended with the session ([`end_of_feed`]).
    {
        let shed = audit.take_live_shed();
        let changed = end_of_feed(
            &mut lock_activity(&activity),
            &queued,
            shed,
            now_unix_millis(),
        );
        for entry in &changed {
            sink.emit_activity(entry);
        }
    }
    if !terminated {
        // The active loop ended without a Revoked/Failed fold (driver closed or
        // every intent handle dropped). Drive the model to an honest end rather
        // than leaving a frozen "under control" surface (REQ-S-009).
        let mut state = lock(&shared);
        if !is_terminal(&state) {
            state.fail("session", "the session ended before a terminal event");
        }
        sink.emit(&state);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use cretlink_console::{
        AuditEntry, DiscoveredTarget, DiscoverySource, SessionPhase, TargetId, TechnicianHost,
    };
    use cretlink_protocol::{RevokeReason, SessionId, TargetIdentity};
    use cretlink_session::{ActivitySource, ActivityState};
    // The audit vocabulary used to build sample bridge records for the
    // activity-mapping tests. `cretlink-console` re-exports `AuditEntry` but not
    // these building blocks, so the tests reach them from the shared crate (a
    // dev-dependency; see Cargo.toml).
    use cretlink_shared::{Actor, AuditValue, Outcome};

    /// Counts emitted snapshots so tests can assert the frontend is re-rendered
    /// after every fold, without unwrapping a lock.
    #[derive(Debug, Default)]
    struct CountingSink(AtomicUsize);

    impl CountingSink {
        fn count(&self) -> usize {
            self.0.load(Ordering::Relaxed)
        }
    }

    impl StateSink for CountingSink {
        fn emit(&self, _state: &ConsoleState) {
            self.0.fetch_add(1, Ordering::Relaxed);
        }
    }

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

    /// A console with one selected, connecting incoming target — the state the
    /// driver is in once the listener has bound and the event pump starts. This
    /// mirrors the live `start_live_session` shim exactly.
    fn connecting_console() -> ConsoleState {
        let mut state = ConsoleState::new(technician());
        let id = TargetId::new();
        state.upsert_target(DiscoveredTarget::new(
            id,
            "Incoming customer computer",
            "AB-CD-EF",
            DiscoverySource::Manual,
        ));
        assert!(state.select(id).is_ok());
        assert!(state.begin_connect().is_ok());
        state
    }

    #[test]
    fn happy_path_folds_to_full_control_and_emits_each_step() {
        let mut state = connecting_console();
        let session = SessionId::new();
        let sink = CountingSink::default();
        let ctx = EventContext {
            approval_code: Some("123456"),
            session: Some(session),
        };

        assert!(!step(&mut state, &SessionEvent::Connected, ctx, &sink));
        assert!(matches!(
            state.phase,
            SessionPhase::AwaitingApprovalCode { .. }
        ));

        assert!(!step(
            &mut state,
            &SessionEvent::AwaitingConsent,
            ctx,
            &sink
        ));
        assert_eq!(state.phase, SessionPhase::AwaitingConsent);
        // No control before consent, even mid-stream.
        assert!(!state.can_issue_actions());

        let granted = SessionEvent::ConsentGranted {
            identity: remote_identity(),
        };
        assert!(!step(&mut state, &granted, ctx, &sink));
        assert_eq!(state.phase, SessionPhase::FullControl);

        assert!(!step(&mut state, &SessionEvent::Active, ctx, &sink));
        // Control is live, bound to the real session and the honest wire identity.
        assert!(state.can_issue_actions());
        assert_eq!(state.session, Some(session));

        // A revoke is terminal and tears control down fail-closed.
        let revoked = SessionEvent::Revoked {
            reason: RevokeReason::CustomerRequested,
        };
        assert!(step(&mut state, &revoked, ctx, &sink));
        assert!(matches!(state.phase, SessionPhase::Stopped { .. }));
        assert!(!state.can_issue_actions());

        // One emit per fold: Connected, AwaitingConsent, ConsentGranted, Active,
        // Revoked.
        assert_eq!(sink.count(), 5);
    }

    #[test]
    fn out_of_order_event_fails_closed_and_is_terminal() {
        // `Active` before any consent must never fabricate control: the adapter
        // rejects it and `step` turns the refusal into a terminal failure.
        let mut state = connecting_console();
        let sink = CountingSink::default();
        let ctx = EventContext {
            approval_code: Some("123456"),
            session: Some(SessionId::new()),
        };

        assert!(step(&mut state, &SessionEvent::Active, ctx, &sink));
        assert!(matches!(state.phase, SessionPhase::Failed { .. }));
        assert!(!state.can_issue_actions());
        assert_eq!(sink.count(), 1);
    }

    /// A console that has bound its listener and is waiting for a customer — the
    /// real starting phase of the live `start_live_session` flow (ST-M002-012).
    fn listening_console() -> ConsoleState {
        let mut state = ConsoleState::new(technician());
        state.listen("6RRR-2H8F", "10.0.0.9:7000").unwrap();
        state
    }

    #[test]
    fn arrival_on_connected_materialises_target_and_reaches_full_control() {
        let mut state = listening_console();
        assert!(state.selected.is_none());
        let session = SessionId::new();
        let sink = CountingSink::default();
        let ctx = EventContext {
            approval_code: Some("123456"),
            session: Some(session),
        };

        // Connected while listening materialises the incoming target and folds
        // straight into the approval-code phase — no manual select/connect.
        assert!(!step(&mut state, &SessionEvent::Connected, ctx, &sink));
        assert!(state.selected.is_some());
        assert!(matches!(
            state.phase,
            SessionPhase::AwaitingApprovalCode { .. }
        ));

        assert!(!step(
            &mut state,
            &SessionEvent::AwaitingConsent,
            ctx,
            &sink
        ));
        let granted = SessionEvent::ConsentGranted {
            identity: remote_identity(),
        };
        assert!(!step(&mut state, &granted, ctx, &sink));
        assert!(!step(&mut state, &SessionEvent::Active, ctx, &sink));
        // Full control is bound to the real session and honest wire identity.
        assert!(state.can_issue_actions());
        assert_eq!(state.session, Some(session));
    }

    #[test]
    fn compare_gate_holds_the_code_window_before_consent() {
        // The real responder order is Connected → AwaitingApprovalCompare →
        // AwaitingConsent → ConsentGranted → Active (ST-M002-013). The compare
        // event must hold the approval-code window open (the humanly-perceptible
        // comparison gate) without granting any control, and full control must
        // still arrive only after the customer's real consent.
        let mut state = listening_console();
        let session = SessionId::new();
        let sink = CountingSink::default();
        let ctx = EventContext {
            approval_code: Some("123456"),
            session: Some(session),
        };

        assert!(!step(&mut state, &SessionEvent::Connected, ctx, &sink));
        assert!(matches!(
            state.phase,
            SessionPhase::AwaitingApprovalCode { .. }
        ));

        // The compare gate: the wire pauses here for the code comparison. The
        // phase must stay on the approval-code window and no control may appear.
        assert!(!step(
            &mut state,
            &SessionEvent::AwaitingApprovalCompare,
            ctx,
            &sink
        ));
        assert!(matches!(
            state.phase,
            SessionPhase::AwaitingApprovalCode { .. }
        ));
        assert!(!state.can_issue_actions());

        // Only once the technician's confirm releases the gate over the wire does
        // AwaitingConsent arrive; still no control until the customer grants it.
        assert!(!step(
            &mut state,
            &SessionEvent::AwaitingConsent,
            ctx,
            &sink
        ));
        assert_eq!(state.phase, SessionPhase::AwaitingConsent);
        assert!(!state.can_issue_actions());

        let granted = SessionEvent::ConsentGranted {
            identity: remote_identity(),
        };
        assert!(!step(&mut state, &granted, ctx, &sink));
        assert!(!step(&mut state, &SessionEvent::Active, ctx, &sink));
        assert!(state.can_issue_actions());
        assert_eq!(state.session, Some(session));
    }

    #[test]
    fn compare_mismatch_before_consent_fails_closed() {
        // A mismatch surfaces as a wire Failed(compare); the console must end at a
        // terminal, control-free failure, never advancing toward consent.
        let mut state = listening_console();
        let sink = CountingSink::default();
        let ctx = EventContext {
            approval_code: Some("123456"),
            session: Some(SessionId::new()),
        };

        assert!(!step(&mut state, &SessionEvent::Connected, ctx, &sink));
        assert!(!step(
            &mut state,
            &SessionEvent::AwaitingApprovalCompare,
            ctx,
            &sink
        ));

        let failed = SessionEvent::Failed {
            stage: "compare".to_string(),
            message: "the Session Approval Codes did not match".to_string(),
        };
        assert!(step(&mut state, &failed, ctx, &sink));
        assert!(matches!(state.phase, SessionPhase::Failed { .. }));
        assert!(!state.can_issue_actions());
    }

    #[test]
    fn first_failed_while_listening_fails_closed() {
        // A failure arriving before any customer (still listening) must be a
        // terminal, control-free end, not a silently ignored event.
        let mut state = listening_console();
        let sink = CountingSink::default();
        let failed = SessionEvent::Failed {
            stage: "pairing".to_string(),
            message: "secure pairing failed".to_string(),
        };
        assert!(step(&mut state, &failed, EventContext::default(), &sink));
        assert!(matches!(state.phase, SessionPhase::Failed { .. }));
        assert!(!state.can_issue_actions());
        assert_eq!(sink.count(), 1);
    }

    // --- Activity feed (ST-M009-011) ---------------------------------------
    //
    // The pump folds every live audit echo into the shared technician feed. The
    // fold is pure, so the contract the frontend renders is tested here without
    // a transport.

    fn echo(shed_before: u64, entry: AuditEntry) -> LiveEcho {
        LiveEcho { shed_before, entry }
    }

    #[test]
    fn an_ai_call_starts_running_and_finishes_in_place() {
        let mut feed = TechnicianActivity::new();
        let mut start = AuditEntry::event(Actor::Mcp, None, "mcp_remote_exec_start", Outcome::Ok);
        start.args.insert("call_id", AuditValue::Int(5));
        start
            .args
            .insert("command", AuditValue::text("Get-PSDrive C"));
        start
            .args
            .insert("purpose", AuditValue::text("Checking free disk space"));
        start
            .args
            .insert("client", AuditValue::text("codex-mcp-client"));
        let changed = fold_live_echo(&mut feed, &echo(0, start.clone()), 1_000);
        let [ActivityEntry::Item { item }] = changed.as_slice() else {
            panic!("expected one item, got {changed:?}");
        };
        assert_eq!(item.id, "call-5");
        assert_eq!(item.source, ActivitySource::Ai);
        assert_eq!(item.client_label.as_deref(), Some("Codex CLI"));
        assert_eq!(item.purpose.as_deref(), Some("Checking free disk space"));
        assert!(item.operation.contains("Get-PSDrive C"));
        assert_eq!(item.state, ActivityState::Running);

        let mut done = AuditEntry::event(Actor::Mcp, None, "mcp_remote_exec", Outcome::Ok);
        done.args.insert("call_id", AuditValue::Int(5));
        done.args.insert("exit_code", AuditValue::Int(0));
        done.duration_ms = Some(250);
        let changed = fold_live_echo(&mut feed, &echo(0, done), 1_250);
        let [ActivityEntry::Item { item }] = changed.as_slice() else {
            panic!("expected one item, got {changed:?}");
        };
        assert_eq!(item.id, "call-5");
        assert_eq!(item.state, ActivityState::Done { exit_code: Some(0) });
        assert_eq!(item.duration_ms, Some(250));
        assert_eq!(feed.entries().len(), 1, "updated in place");
    }

    #[test]
    fn read_only_ai_calls_are_in_the_feed() {
        // Unlike the old feed, read-only inspection calls are shown too.
        let mut feed = TechnicianActivity::new();
        let mut start =
            AuditEntry::event(Actor::Mcp, None, "mcp_remote_fs_stat_start", Outcome::Ok);
        start.args.insert("call_id", AuditValue::Int(1));
        start.args.insert("path", AuditValue::text("C:\\Logs"));
        assert_eq!(fold_live_echo(&mut feed, &echo(0, start), 0).len(), 1);
    }

    #[test]
    fn dropped_echoes_become_a_gap_before_the_next_item() {
        let mut feed = TechnicianActivity::new();
        let mut entry =
            AuditEntry::event(Actor::Mcp, None, "mcp_remote_list_jobs_start", Outcome::Ok);
        entry.args.insert("call_id", AuditValue::Int(2));
        let changed = fold_live_echo(&mut feed, &echo(3, entry), 0);
        assert!(matches!(changed[0], ActivityEntry::Gap { count: 3, .. }));
        assert!(matches!(changed[1], ActivityEntry::Item { .. }));
    }

    #[test]
    fn echoes_queued_at_the_end_are_folded_before_the_session_ends() {
        // An AI call was running when the session was revoked; its completion
        // record reached the echo channel after the loop stopped reading.
        let mut feed = TechnicianActivity::new();
        let mut start = AuditEntry::event(Actor::Mcp, None, "mcp_remote_exec_start", Outcome::Ok);
        start.args.insert("call_id", AuditValue::Int(9));
        start.args.insert("command", AuditValue::text("dir"));
        fold_live_echo(&mut feed, &echo(0, start), 1_000);
        let mut done = AuditEntry::event(Actor::Mcp, None, "mcp_remote_exec", Outcome::Ok);
        done.args.insert("call_id", AuditValue::Int(9));
        done.args.insert("exit_code", AuditValue::Int(0));
        let changed = end_of_feed(&mut feed, &[echo(1, done)], 2, 2_000);
        // In order: the gap before the queued record, its item, the trailing
        // gap; nothing left running to end.
        assert!(
            matches!(changed[0], ActivityEntry::Gap { count: 1, .. }),
            "{changed:?}"
        );
        let ActivityEntry::Item { item } = &changed[1] else {
            panic!("expected the item, got {changed:?}");
        };
        assert_eq!(item.state, ActivityState::Done { exit_code: Some(0) });
        assert!(
            matches!(changed[2], ActivityEntry::Gap { count: 2, .. }),
            "{changed:?}"
        );
        assert_eq!(changed.len(), 3, "{changed:?}");
        let items: Vec<ActivityState> = feed
            .entries()
            .into_iter()
            .filter_map(|entry| match entry {
                ActivityEntry::Item { item } => Some(item.state),
                ActivityEntry::Gap { .. } => None,
            })
            .collect();
        assert_eq!(items, vec![ActivityState::Done { exit_code: Some(0) }]);
    }

    #[test]
    fn lifecycle_records_do_not_reach_the_feed() {
        let mut feed = TechnicianActivity::new();
        let entry = AuditEntry::event(Actor::Console, None, "pairing_completed", Outcome::Ok);
        assert!(fold_live_echo(&mut feed, &echo(0, entry), 0).is_empty());
    }
}
