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
    AuditEntry, AuditServiceConfig, ChannelAudit, ConsoleRoute, ConsoleSessionService,
    ConsoleState, EventContext, EventOutcome, IpcAudit, IpcServer, SHUTDOWN_GRACE,
    SessionAuditStart, SessionPhase, TargetId, apply_session_event, open_session_log,
};
use cretlink_mcp::{IpcEndpoint, IpcListener};
use cretlink_protocol::{DesktopAccess, JobOutcome, RevokeReason, SessionId, TargetIdentity};
use cretlink_session::{HostHandle, SessionEvent};
use serde::Serialize;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};
use tokio::sync::{broadcast, oneshot};

/// The sender half of the button-intent channel, held by [`AppState`](crate::AppState)
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

/// One live activity line for the console's Activity view (ST-M004-007,
/// REQ-F-017). It is the data form of the CLI's `[ai]`/`[job …]` renderers: the
/// hosted backbone maps every attributable signal — a bridge call starting, a
/// job finishing, output shed to stay bounded, desktop access — to one of these
/// and the frontend appends it. Every string is backend-built; the frontend
/// writes it with `textContent`, never as HTML (REQ-S-016, REQ-S-018).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ActivityLine {
    /// Who drove this line: `"claude"` for MCP bridge activity, `"session"` for
    /// a job outcome or desktop access observed on the wire, `"note"` for a
    /// backbone status line. Never fabricated: the live echo carries only
    /// `Actor::Mcp` records, and job outcomes on the console side carry no
    /// origin, so they are attributed neutrally to the session.
    pub source: &'static str,
    /// A coarse kind so the frontend can style outcomes distinctly: `"start"`,
    /// `"finished"`, `"dropped"`, `"desktop"`, or `"note"`.
    pub kind: &'static str,
    /// The backend-built, single-line human text.
    pub text: String,
    /// The job this line concerns, when it concerns one.
    pub job: Option<u64>,
    /// The honest process exit code, when a finished job carried one.
    pub exit_code: Option<i32>,
    /// Whether a finished job's outcome was a clean success, for ok/bad styling.
    pub ok: Option<bool>,
}

impl ActivityLine {
    /// A backbone status note (bridge endpoint ready or unavailable). Attributed
    /// to the session itself, never to Claude or the technician.
    fn note(text: String) -> Self {
        Self {
            source: "note",
            kind: "note",
            text,
            job: None,
            exit_code: None,
            ok: None,
        }
    }
}

/// Maps a capture-time bridge audit record to a live activity line, mirroring
/// the CLI's `render_ai_activity` exactly (REQ-F-017). The live echo carries
/// only `Actor::Mcp` records (`ChannelAudit::record_entry`), so every line here
/// is Claude's; read-only inspection calls return `None` and stay in the durable
/// log only, keeping the feed readable.
fn ai_activity_line(entry: &AuditEntry) -> Option<ActivityLine> {
    let text = match entry.action {
        "mcp_remote_exec_start" => format!("exec started: {}", arg_preview(entry, "command")),
        "mcp_remote_write_stdin_start" => {
            format!("stdin -> job {}", arg_preview(entry, "job_id"))
        }
        "mcp_remote_cancel_start" => format!("cancel job {}", arg_preview(entry, "job_id")),
        "mcp_remote_make_dir_start" => format!("make-dir {}", arg_preview(entry, "path")),
        "mcp_remote_move_start" => format!(
            "move {} -> {}",
            arg_preview(entry, "path"),
            arg_preview(entry, "to")
        ),
        "mcp_remote_delete_start" => format!("delete {}", arg_preview(entry, "path")),
        "mcp_remote_remove_dir_start" => format!("remove-dir {}", arg_preview(entry, "path")),
        "mcp_remote_upload_start" => format!("upload -> {}", arg_preview(entry, "path")),
        "mcp_remote_download_start" => format!("download {}", arg_preview(entry, "path")),
        "mcp_remote_cleanup_start" => format!("cleanup ({})", arg_preview(entry, "trigger")),
        "mcp_remote_write_stdin"
        | "mcp_remote_cancel"
        | "mcp_remote_make_dir"
        | "mcp_remote_move"
        | "mcp_remote_delete"
        | "mcp_remote_remove_dir"
        | "mcp_remote_upload"
        | "mcp_remote_download"
        | "mcp_remote_cleanup" => format!(
            "{} finished: {:?}",
            entry.action.trim_start_matches("mcp_remote_"),
            entry.outcome
        ),
        _ => return None,
    };
    let kind = if entry.action.ends_with("_start") {
        "start"
    } else {
        "finished"
    };
    Some(ActivityLine {
        source: "claude",
        kind,
        text,
        job: None,
        exit_code: None,
        ok: None,
    })
}

/// Maps one post-active session event to an activity line, mirroring the CLI's
/// `report_active_event` finish/drop/desktop rendering (REQ-F-017). Raw
/// `ExecOutput` bytes are deliberately not rendered here — they belong to the
/// terminal panel (ST-M008-001); the Activity feed shows attributable outcomes.
/// Lifecycle terminals (Revoked/Failed) are folded through [`step`], not here.
/// The console side sees no per-job origin on `ExecFinished`, so the outcome is
/// attributed neutrally to the session rather than fabricated as Claude's.
fn session_activity_line(event: &SessionEvent) -> Option<ActivityLine> {
    match event {
        SessionEvent::ExecOutputDropped { job, bytes } => Some(ActivityLine {
            source: "session",
            kind: "dropped",
            text: format!(
                "cretlink: {bytes} bytes of output from job {job} were dropped to stay within the console buffer"
            ),
            job: Some(job.0),
            exit_code: None,
            ok: None,
        }),
        SessionEvent::ExecFinished {
            job,
            exit_code,
            outcome,
        } => {
            let text = match exit_code {
                Some(code) => format!("job {job} finished: {outcome:?} (exit code {code})"),
                None => format!("job {job} finished: {outcome:?}"),
            };
            let ok = matches!(outcome, JobOutcome::Completed) && exit_code.is_none_or(|c| c == 0);
            Some(ActivityLine {
                source: "session",
                kind: "finished",
                text,
                job: Some(job.0),
                exit_code: *exit_code,
                ok: Some(ok),
            })
        }
        SessionEvent::DesktopActivity { access } => {
            let text = match access {
                DesktopAccess::Observe => {
                    "screen observation active — a screenshot was captured".to_string()
                }
                DesktopAccess::Control => {
                    "remote input active — controlling the customer desktop".to_string()
                }
            };
            Some(ActivityLine {
                source: "session",
                kind: "desktop",
                text,
                job: None,
                exit_code: None,
                ok: None,
            })
        }
        _ => None,
    }
}

/// A short, single-line preview of one audit arg for live rendering, ported
/// verbatim from the console CLI: it uses the audit value's display form
/// (`[REDACTED]`/`[TRUNCATED]` markers preserved) and clamps it on a char
/// boundary so a long command or path never floods the feed.
fn arg_preview(entry: &AuditEntry, key: &str) -> String {
    const PREVIEW_BYTES: usize = 120;
    let Some(value) = entry.args.0.get(key) else {
        return "?".to_string();
    };
    let text = value.to_string().replace(['\r', '\n'], " ");
    if text.len() <= PREVIEW_BYTES {
        return text;
    }
    let mut end = PREVIEW_BYTES;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &text[..end])
}

/// Receives whole-state snapshots to render.
///
/// The GUI implements this over a Tauri `AppHandle` (emitting a `"console-state"`
/// event); tests implement it with an in-memory collector so [`step`] is
/// verifiable without a transport.
pub trait StateSink: Send + Sync + 'static {
    /// Renders one snapshot of the console state.
    fn emit(&self, state: &ConsoleState);
    /// Renders one live activity line to a separate channel. Defaults to a no-op
    /// so in-memory test sinks need not implement it. The GUI sink emits a
    /// `"console-activity"` Tauri event; routing activity apart from whole-state
    /// snapshots means high-frequency job output never triggers a full
    /// `console-state` re-emit (REQ-N-006).
    fn emit_activity(&self, _line: &ActivityLine) {}
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
    audit_config: AuditServiceConfig,
    start_tx: oneshot::Sender<SessionAuditStart>,
    service_slot: ServiceSlot,
) {
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
                                    audit, service_slot,
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
/// - `service.subscribe()` — job outcomes, output-drop markers, and desktop
///   activity become [`ActivityLine`]s on the separate activity channel; a
///   `Revoked`/`Failed` lifecycle event is folded through [`step`] and ends the
///   session fail-closed.
/// - the live audit echo — bridge-originated (`Actor::Mcp`) start-lines become
///   Claude-attributed [`ActivityLine`]s the instant the call starts.
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
    let endpoint = IpcEndpoint::user_default();
    let ipc_server = match IpcListener::bind(&endpoint) {
        Ok(listener) => {
            let route: Arc<dyn ConsoleRoute> = Arc::new(service.clone());
            let ipc_audit: Arc<dyn IpcAudit> = Arc::new(audit.clone());
            let in_flight = service.activity_counter();
            let (shutdown_tx, shutdown_rx) = oneshot::channel();
            sink.emit_activity(&ActivityLine::note(
                "MCP bridge endpoint ready — Claude can attach to this session".to_string(),
            ));
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
            sink.emit_activity(&ActivityLine::note(format!(
                "MCP bridge endpoint unavailable ({error}); interactive control still works"
            )));
            None
        }
    };

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
                        } else if let Some(line) = session_activity_line(&event) {
                            // Non-lifecycle activity is routed apart from the
                            // whole-state channel, so high-frequency job output
                            // never triggers a `console-state` re-emit (REQ-N-006).
                            sink.emit_activity(&line);
                        }
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
                    Some(entry) => {
                        if let Some(line) = ai_activity_line(&entry) {
                            sink.emit_activity(&line);
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
        DiscoveredTarget, DiscoverySource, SessionPhase, TargetId, TechnicianHost,
    };
    use cretlink_protocol::{JobId, RevokeReason, SessionId, TargetIdentity};
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

    // --- Activity mapping (ST-M004-007, plan §5g) ---------------------------
    //
    // The two pure mapping functions the pump fans out to the Activity feed take
    // one sample event/record and must yield an `ActivityLine` with the right
    // source, kind, attribution and — where present — exit code and ok verdict.
    // No transport, no I/O: the mapping *is* the contract the frontend renders.

    #[test]
    fn exec_finished_maps_to_a_completed_session_line() {
        // A clean completion is attributed to the *session* (the console sees no
        // per-job origin on `ExecFinished`) and marked ok, carrying the job id
        // and exit code so the feed can show "done" with the code.
        let line = session_activity_line(&SessionEvent::ExecFinished {
            job: JobId(3),
            exit_code: Some(0),
            outcome: JobOutcome::Completed,
        })
        .expect("ExecFinished maps to an activity line");
        assert_eq!(line.source, "session");
        assert_eq!(line.kind, "finished");
        assert_eq!(line.job, Some(3));
        assert_eq!(line.exit_code, Some(0));
        assert_eq!(line.ok, Some(true));
        assert!(line.text.contains("job 3 finished"));
    }

    #[test]
    fn exec_finished_with_a_nonzero_code_is_not_ok() {
        // A non-zero exit under an otherwise-Completed outcome is a failure
        // verdict, never reported as success (AGENTS.md honesty).
        let line = session_activity_line(&SessionEvent::ExecFinished {
            job: JobId(7),
            exit_code: Some(1),
            outcome: JobOutcome::Completed,
        })
        .expect("ExecFinished maps to an activity line");
        assert_eq!(line.ok, Some(false));
        assert_eq!(line.exit_code, Some(1));
    }

    #[test]
    fn a_failed_outcome_is_not_ok_even_without_an_exit_code() {
        // No exit code (e.g. the job died before reporting one) plus a Failed
        // outcome must still read as a failure, not an absent/ok verdict.
        let line = session_activity_line(&SessionEvent::ExecFinished {
            job: JobId(9),
            exit_code: None,
            outcome: JobOutcome::Failed,
        })
        .expect("ExecFinished maps to an activity line");
        assert_eq!(line.ok, Some(false));
        assert_eq!(line.exit_code, None);
    }

    #[test]
    fn dropped_output_maps_to_an_honest_truncation_line() {
        // Dropped bytes are surfaced with their count, never silently swallowed
        // (REQ-N-008). The verdict stays `None` — a drop is not a pass or fail.
        let line = session_activity_line(&SessionEvent::ExecOutputDropped {
            job: JobId(4),
            bytes: 2048,
        })
        .expect("ExecOutputDropped maps to an activity line");
        assert_eq!(line.source, "session");
        assert_eq!(line.kind, "dropped");
        assert_eq!(line.job, Some(4));
        assert_eq!(line.ok, None);
        assert!(line.text.contains("2048 bytes"));
        assert!(line.text.contains("dropped"));
    }

    #[test]
    fn raw_exec_output_is_not_an_activity_line() {
        // Streaming stdout belongs to the terminal panel (ST-M008-001), not the
        // attributable Activity feed, so non-outcome events map to nothing.
        assert!(session_activity_line(&SessionEvent::Active).is_none());
    }

    #[test]
    fn mcp_exec_start_maps_to_a_claude_start_line() {
        // A bridge exec-start record is Claude's; the feed shows the command
        // preview under a "start" line, sourced "claude".
        let mut entry = AuditEntry::event(Actor::Mcp, None, "mcp_remote_exec_start", Outcome::Ok);
        entry
            .args
            .insert("command", AuditValue::text("Get-Process"));
        let line = ai_activity_line(&entry).expect("exec-start maps to an activity line");
        assert_eq!(line.source, "claude");
        assert_eq!(line.kind, "start");
        assert!(line.text.starts_with("exec started:"));
        assert!(line.text.contains("Get-Process"));
    }

    #[test]
    fn mcp_finish_record_maps_to_a_finished_line() {
        // The paired finish record folds to a "finished" line naming the action.
        let entry = AuditEntry::event(Actor::Mcp, None, "mcp_remote_delete", Outcome::Ok);
        let line = ai_activity_line(&entry).expect("finish record maps to an activity line");
        assert_eq!(line.source, "claude");
        assert_eq!(line.kind, "finished");
        assert!(line.text.starts_with("delete finished:"));
    }

    #[test]
    fn read_only_bridge_records_stay_out_of_the_feed() {
        // Read-only inspection calls (stat/list/system-info) are deliberately
        // unmapped: they live in the durable log only, keeping the feed readable.
        let entry = AuditEntry::event(Actor::Mcp, None, "mcp_remote_stat", Outcome::Ok);
        assert!(ai_activity_line(&entry).is_none());
    }
}
