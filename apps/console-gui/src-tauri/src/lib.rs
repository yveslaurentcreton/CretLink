//! CretLink Console GUI shell (Tauri v2) — ST-M002-002.
//!
//! This crate is a thin renderer over the Rust-owned session-UI state model in
//! [`cretlink_console::state`] (ADR-0011). It holds one [`ConsoleState`] behind
//! a mutex, exposes a small, schema-validated set of Tauri commands over it, and
//! serializes the whole state back to the plain-TypeScript frontend on every
//! change. The frontend holds no session authority of its own.
//!
//! The shipped release boots **live and idle** (ST-M002-009): this technician
//! host with no targets, ready to open a real listening Noise_XX session via
//! [`start_live_session`]. Every pairing, consent, and teardown then flows from
//! the real transport through the shared session adapter — no fabricated
//! control.
//!
//! The clearly-labelled **simulated** seed targets and the fabricated-consent
//! command exist only for local development and are compiled out unless the
//! `dev-simulate` cargo feature is enabled. The release exe therefore has no way
//! to reach a simulated driver (AC #4).

use std::collections::BTreeMap;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use cretlink_console::{
    AUDIT_CHANNEL_CAPACITY, AuditServiceConfig, ChannelAudit, ConsoleSessionService, ConsoleState,
    SessionPhase, StopReason, TechnicianHost, TermAttachment, TermEvent, prune_audit_dir,
    report_prune, spawn_audit_service,
};
// The simulated seed targets are developer-only: the shipped console listens and
// never picks a target, so these compile only under `dev-simulate`.
#[cfg(feature = "dev-simulate")]
use cretlink_console::{DiscoveredTarget, DiscoverySource, TargetId};
use cretlink_discovery::{Advertiser, ConnectionCode, private_interface_addrs};
// The technician terminal (ST-M008-001) addresses a hosted terminal by `JobId`,
// picks a `Shell`, and reports its end via `JobOutcome`; `TargetError` is the
// service's fail-closed error surface, mapped straight to a frontend
// `CommandError` so `no_consented_session` stays legible in the UI.
use cretlink_mcp::TargetError;
use cretlink_protocol::{JobId, JobOutcome, PROTOCOL_V1, RevokeReason, Shell};
// Only the fabricated-consent developer command binds a synthetic session id and
// simulated identity; they leave the shipped build entirely (ST-M002-009).
#[cfg(feature = "dev-simulate")]
use cretlink_protocol::{SessionId, TargetIdentity};
use cretlink_session::{ConsoleHints, SessionHost};
use serde::Serialize;
use tauri::{AppHandle, Emitter, State};
use tokio::sync::oneshot;

mod session_driver;
use session_driver::{
    ActivityLine, Intent, IntentSender, ServiceSlot, StateSink, intent_channel, lock, lock_service,
    pump,
};

/// The console's single source of truth, shared across command invocations and
/// the background live-session pump. Both hold the same `Arc<Mutex<..>>`, so the
/// synchronous commands and the async event stream can never disagree about the
/// current state.
///
/// `live` carries the intent sender to the running pump task while a live session
/// is in flight (ST-M002-013). The compare-gate confirm/mismatch buttons and the
/// emergency stop hand their decision to the pump through it; a stale sender left
/// after a session ends simply fails its `send` (the receiver is gone), which the
/// commands surface honestly, and the next `start_live_session` overwrites it.
#[derive(Debug, Clone)]
pub struct AppState {
    inner: Arc<Mutex<ConsoleState>>,
    live: Arc<Mutex<Option<IntentSender>>>,
    /// The live session service while a session is under full control, published
    /// by the pump at hand-off so the terminal commands can attach in-process
    /// (ST-M008-001). `None` outside an active session, so a terminal command
    /// then fails closed exactly like the service's own `NoConsentedSession`.
    service: ServiceSlot,
}

impl AppState {
    /// Builds the shipped initial console: this technician host, live and idle,
    /// with no targets. Real discovery or [`start_live_session`] populate it; the
    /// UI never boots into a simulated driver.
    #[must_use]
    pub fn live_idle() -> Self {
        Self {
            inner: Arc::new(Mutex::new(ConsoleState::new(technician_host()))),
            live: Arc::new(Mutex::new(None)),
            service: Arc::new(Mutex::new(None)),
        }
    }

    /// Builds a console seeded with clearly-labelled simulated targets for local
    /// development. Compiled only under `dev-simulate`, so the shipped exe can
    /// never boot into the simulator (ST-M002-009, AC #4).
    #[cfg(feature = "dev-simulate")]
    #[must_use]
    pub fn seeded() -> Self {
        Self {
            inner: Arc::new(Mutex::new(seed_state())),
            live: Arc::new(Mutex::new(None)),
            service: Arc::new(Mutex::new(None)),
        }
    }

    fn guard(&self) -> MutexGuard<'_, ConsoleState> {
        lock(&self.inner)
    }

    /// A clone of the live session service while a session is under full control,
    /// or `None` otherwise. The terminal commands reach the hosted service
    /// through this; an empty slot means no consented session, so they refuse —
    /// fail-closed by construction (ST-M008-001). The clone is cheap (`Arc`) and
    /// keeps no lock across the subsequent `.await`.
    fn service(&self) -> Option<ConsoleSessionService> {
        lock_service(&self.service).clone()
    }

    /// Sends a technician [`Intent`] to the running pump task, if a session is
    /// live. Returns `false` when there is no live session or the pump has already
    /// ended (a dropped receiver), so callers can decide honestly.
    fn send_intent(&self, intent: Intent) -> bool {
        let live = self.live.lock().unwrap_or_else(PoisonError::into_inner);
        live.as_ref().is_some_and(|tx| tx.send(intent).is_ok())
    }
}

/// The initial console for both [`run`] and [`Default`]: the live-idle console in
/// the shipped build, and the simulated seed only under `dev-simulate`.
#[cfg(feature = "dev-simulate")]
fn default_app_state() -> AppState {
    AppState::seeded()
}

/// The initial console for both [`run`] and [`Default`]: live and idle.
#[cfg(not(feature = "dev-simulate"))]
fn default_app_state() -> AppState {
    AppState::live_idle()
}

/// Bounded window to let the audit drain seal the session log after the pump
/// ends, mirroring the console CLI (main.rs). An unsealed log stays verifiable
/// but reports `Unsealed`, so we note that rather than exit silently
/// (ST-M003-006, ST-M004-007).
const AUDIT_SEAL_TIMEOUT: Duration = Duration::from_secs(10);

/// Renders console-state snapshots to the Tauri frontend by emitting a
/// `"console-state"` event carrying the whole serialized state, and live
/// activity lines on a separate `"console-activity"` event. The frontend holds
/// no session authority of its own; it only reflects what it receives.
#[derive(Debug)]
struct TauriSink {
    app: AppHandle,
}

impl StateSink for TauriSink {
    fn emit(&self, state: &ConsoleState) {
        // A dropped frontend (e.g. window closed) is not an error the backend
        // must act on; the session lifecycle continues to be driven and audited.
        let _ = self.app.emit("console-state", state);
    }

    fn emit_activity(&self, line: &ActivityLine) {
        // Live activity rides a separate event so a burst of job output never
        // forces a whole-`console-state` re-render (REQ-N-006); a dropped
        // frontend is likewise not a backend error.
        let _ = self.app.emit("console-activity", line);
    }
}

impl Default for AppState {
    fn default() -> Self {
        default_app_state()
    }
}

/// A command failure shaped for the frontend: a stage and a plain-language,
/// secret-free message (REQ-N-015).
#[derive(Debug, Clone, Serialize)]
pub struct CommandError {
    /// Which stage failed, for display and triage.
    pub stage: String,
    /// A safe, actionable description. Never carries key material or codes.
    pub message: String,
}

impl CommandError {
    fn new(stage: &str, message: impl std::fmt::Display) -> Self {
        Self {
            stage: stage.to_string(),
            message: message.to_string(),
        }
    }
}

/// Returns the current console state to the frontend.
#[tauri::command]
fn get_state(state: State<'_, AppState>) -> ConsoleState {
    state.guard().clone()
}

/// The technician confirms the Session Approval Codes match on both screens,
/// releasing the compare gate (REQ-S-006, ST-M002-013).
///
/// Only valid while the code is on screen (the `AwaitingApprovalCode` phase *is*
/// the compare window). In a live session this does **not** advance local state —
/// it sends the wire confirm ([`HostHandle::confirm_code`](
/// cretlink_session::HostHandle::confirm_code)) and the console moves to await
/// consent only when the wire echoes `AwaitingConsent` and the pump folds it, so
/// no control is fabricated locally. In the `dev-simulate` seeded flow (no live
/// pump) it advances the local model directly.
#[tauri::command]
fn confirm_code(state: State<'_, AppState>) -> Result<ConsoleState, CommandError> {
    {
        let console = state.guard();
        if !matches!(console.phase, SessionPhase::AwaitingApprovalCode { .. }) {
            return Err(CommandError::new(
                "compare",
                "there is no approval code to confirm right now",
            ));
        }
    }
    // Live: hand the confirm to the pump, which releases the gate over the wire.
    // The local phase advances only when the wire echoes `AwaitingConsent`.
    if state.send_intent(Intent::Confirm) {
        return Ok(state.guard().clone());
    }
    // No live pump (the seeded developer flow): advance the local model directly.
    let mut console = state.guard();
    console
        .confirm_approval_code()
        .map_err(|e| CommandError::new("compare", e))?;
    Ok(console.clone())
}

/// The technician reports the Session Approval Codes do **not** match — the
/// signature of a man-in-the-middle. Tears the session down fail-closed with no
/// control ever granted (REQ-S-009, ST-M002-013).
///
/// Only valid while the code is on screen. In a live session this sends the wire
/// mismatch ([`HostHandle::report_code_mismatch`](
/// cretlink_session::HostHandle::report_code_mismatch)), which revokes over the
/// wire and drives both endpoints to a `Failed(compare)` end that the pump folds.
/// In the seeded developer flow (no live pump) it fails the local model directly.
#[tauri::command]
fn report_code_mismatch(state: State<'_, AppState>) -> Result<ConsoleState, CommandError> {
    {
        let console = state.guard();
        if !matches!(console.phase, SessionPhase::AwaitingApprovalCode { .. }) {
            return Err(CommandError::new(
                "compare",
                "there is no approval code to compare right now",
            ));
        }
    }
    // Live: the pump revokes over the wire and the `Failed(compare)` echo folds
    // in. Otherwise fail the local (seeded) model straight away — fail closed.
    if !state.send_intent(Intent::Mismatch) {
        let mut console = state.guard();
        console.fail("compare", "the Session Approval Codes did not match");
    }
    Ok(state.guard().clone())
}

/// Simulates the customer granting one full-control consent (the real consent
/// is the agent's action in ST-M002-005), binding a simulated identity.
///
/// Developer-only: compiled out of the shipped exe (ST-M002-009). In a release
/// build full control can appear only when the real wire `ConsentGranted`
/// echoes back through the live pump.
#[cfg(feature = "dev-simulate")]
#[tauri::command]
fn simulate_consent(state: State<'_, AppState>) -> Result<ConsoleState, CommandError> {
    let mut console = state.guard();
    console
        .grant_full_control(SessionId::new(), simulated_identity())
        .map_err(|e| CommandError::new("consent", e))?;
    Ok(console.clone())
}

/// Simulates the selected target dropping off the network, exercising the
/// fail-closed stale-target path (REQ-F-018, REQ-S-009).
///
/// Developer-only: compiled out of the shipped exe (ST-M002-009). The live path
/// reaches the same fail-closed terminal state from a real dropped transport.
#[cfg(feature = "dev-simulate")]
#[tauri::command]
fn simulate_target_lost(state: State<'_, AppState>) -> Result<ConsoleState, CommandError> {
    let mut console = state.guard();
    let selected = console
        .selected
        .ok_or_else(|| CommandError::new("session", "no target is selected"))?;
    console.set_stale(selected, true);
    Ok(console.clone())
}

/// The emergency stop: revokes the session and rejects new actions
/// (REQ-F-018). Fail-closed and idempotent.
///
/// In a live session it sends the wire revoke to the pump (which tears the Noise
/// channel down and drops the listener) and ends the local model at once, so the
/// surface goes safe immediately even if the wire acknowledgement is delayed. The
/// pump's terminal guard then absorbs the `Revoked` echo without reopening the
/// model. Without a live pump this is a purely local stop.
#[tauri::command]
fn stop_session(state: State<'_, AppState>) -> Result<ConsoleState, CommandError> {
    // Best-effort wire revoke; the local stop below is authoritative and safe.
    let _ = state.send_intent(Intent::Stop(RevokeReason::TechnicianRequested));
    let mut console = state.guard();
    console
        .stop(StopReason::TechnicianRequested)
        .map_err(|e| CommandError::new("stop", e))?;
    Ok(console.clone())
}

/// Starts a real listening LAN session and drives it live (ST-M002-009).
///
/// The console is the *listening* side (ADR-0003): it binds a private-interface
/// listener (never `0.0.0.0`, REQ-S-010), advertises itself over mDNS
/// (best-effort; grants no authority), and waits for the customer's agent to
/// dial in with the printed connection code. Every pairing, consent, and
/// teardown event then flows through the shared session adapter, so the model
/// on screen tracks the real transport — no fabricated control (REQ-F-006,
/// REQ-S-001).
///
/// The console enters a dedicated `Listening` phase carrying the
/// connection code to read out and the private listen address; it shows **no**
/// target list (ST-M002-012). The customer materialises as a target only when a
/// real `Connected` event arrives over the pump, so the console never displays a
/// peer it has not actually heard from. Refusing to start while a session is in
/// progress keeps exactly one live endpoint per console.
#[tauri::command]
fn start_live_session(
    app: AppHandle,
    state: State<'_, AppState>,
) -> Result<ConsoleState, CommandError> {
    {
        let console = state.guard();
        if !matches!(
            console.phase,
            SessionPhase::Idle | SessionPhase::Stopped { .. } | SessionPhase::Failed { .. }
        ) {
            return Err(CommandError::new(
                "listen",
                "a session is already in progress; stop it before starting another",
            ));
        }
    }

    let shared = state.inner.clone();
    let bind_addr = SocketAddr::new(private_bind_ip(), 0);

    // Open the intent channel so the compare-gate confirm/mismatch buttons and
    // the emergency stop can reach the live pump task, which exclusively owns the
    // handle (ST-M002-013). The pump takes the receiver; the sender lives in
    // `state.live` and replaces any stale sender from a prior session.
    let (intent_tx, intent_rx) = intent_channel();
    *state.live.lock().unwrap_or_else(PoisonError::into_inner) = Some(intent_tx);
    // Hand the pump the shared service slot so it can publish the hosted service
    // at full-control hand-off, making the technician terminal reachable in-process
    // (ST-M008-001). Any stale service from a prior session was cleared on that
    // session's fail-closed teardown.
    let service_slot = state.service.clone();

    tauri::async_runtime::spawn(async move {
        let sink: Arc<dyn StateSink> = Arc::new(TauriSink { app });

        // Stand up the audit backbone the console CLI runs (ST-M003-006): one
        // bounded channel feeds one drain that owns the durable, hash-chained
        // per-session log. Retention pruning runs first so the log directory
        // stays bounded. The GUI is now the real host, so it — not only the CLI
        // — owns this spine (ST-M004-007).
        let audit_config = AuditServiceConfig::default();
        report_prune(prune_audit_dir(
            &audit_config.dir,
            &audit_config.retention,
            None,
        ));
        let (audit, audit_rx) = ChannelAudit::bounded(AUDIT_CHANNEL_CAPACITY);
        let (start_tx, start_rx) = oneshot::channel();
        let audit_task = spawn_audit_service(audit_config.clone(), audit_rx, start_rx);

        // 1. Bind the listener on a private interface, recording lifecycle
        //    outcomes (pairing, consent, cleanup) to that same log via
        //    `host_with_audit` (REQ-F-017). A bind failure is an honest terminal
        //    state, never a silent no-op.
        let handle = match SessionHost::host_with_audit(
            bind_addr,
            ConsoleHints {
                label: "This computer (technician)".to_string(),
            },
            PROTOCOL_V1,
            Vec::new(),
            Arc::new(audit.clone()),
        )
        .await
        {
            Ok(handle) => handle,
            Err(error) => {
                let mut console = lock(&shared);
                console.fail("listen", format!("could not start the listener: {error}"));
                sink.emit(&console);
                return;
            }
        };
        let local_addr = match handle.local_addr() {
            Some(addr) => addr,
            None => {
                let mut console = lock(&shared);
                console.fail(
                    "listen",
                    "the listener reported no local address".to_string(),
                );
                sink.emit(&console);
                return;
            }
        };

        // 2. Best-effort mDNS advertising. Discovery is convenience only and
        //    grants no authority; the manual code below always works.
        let _advertiser = Advertiser::advertise(
            "cretlink-console",
            local_addr.port(),
            PROTOCOL_V1,
            "CretLink Console",
        )
        .ok();

        // 3. Enter the listening phase carrying the connection code the customer
        //    types in and the private listen address to read out. No target is
        //    fabricated here: the customer materialises only when a real
        //    `Connected` event arrives over the pump (ST-M002-012), so the
        //    console never shows a target it has not actually heard from.
        let code = match local_addr {
            SocketAddr::V4(v4) => ConnectionCode::encode(v4).to_string(),
            SocketAddr::V6(addr) => addr.to_string(),
        };
        {
            let mut console = lock(&shared);
            let technician = console.technician.clone();
            *console = ConsoleState::new(technician);
            if let Err(error) = console.listen(code, local_addr.to_string()) {
                console.fail("listen", error.to_string());
                sink.emit(&console);
                return;
            }
            sink.emit(&console);
        }

        // 4. Fold the live event stream into the shared state until it ends,
        //    dispatching compare/stop intents from the UI to the handle. At
        //    full control the pump hands the handle to the hosted service so the
        //    MCP bridge and the interactive terminal both route through this one
        //    consented session (ST-M004-007).
        pump(
            handle,
            shared,
            sink,
            intent_rx,
            audit.clone(),
            audit_config,
            start_tx,
            service_slot,
        )
        .await;

        // Seal the durable log: drop the last sender so the drain observes
        // channel close, then wait a bounded moment for the seal record to land.
        // A timeout is recorded honestly — the log verifies as unsealed rather
        // than the exit blocking forever (REQ-N cleanup honesty).
        drop(audit);
        if tokio::time::timeout(AUDIT_SEAL_TIMEOUT, audit_task)
            .await
            .is_err()
        {
            eprintln!(
                "note: the audit log could not be sealed before exit; it will verify as unsealed"
            );
        }
    });

    Ok(state.guard().clone())
}

// --- Interactive terminal (ST-M008-001) ---
//
// The console GUI now hosts the session backbone (ST-M004-007), so a technician
// terminal attaches to the hosted `ConsoleSessionService` IN-PROCESS — no IPC
// round-trip. Every command reaches the service through `state.service()`, which
// is `None` unless a session is under full control, so any terminal command
// outside a consented session fails closed exactly like the service's own
// `NoConsentedSession` guard. Untrusted terminal output is streamed as raw bytes
// and written to xterm by the frontend, never interpreted as markup
// (REQ-S-016/S-018).

/// The identity handed back when a terminal opens, so the panel can label which
/// consented session it is attached to (REQ-F-016).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct TerminalInfo {
    /// The opaque job id addressing this terminal for input/resize/close.
    term: u64,
    /// The consented session id this terminal belongs to, for display.
    session: String,
}

/// One frame streamed to the frontend on the `"console-terminal"` event. `output`
/// carries raw VT bytes (serialized as a JSON number array, reconstructed as a
/// `Uint8Array` by xterm); the terminal ends honestly on `exited`/`revoked`, and
/// a `dropped` frame surfaces an overrun consumer rather than a silent stall
/// (REQ-N-008).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct TerminalFrame {
    /// The terminal this frame belongs to, so a stale frame can be ignored.
    term: u64,
    /// `"output" | "exited" | "revoked" | "dropped"`.
    kind: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    data: Option<Vec<u8>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    exit_code: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    outcome: Option<String>,
}

impl TerminalFrame {
    fn output(term: u64, data: Vec<u8>) -> Self {
        Self {
            term,
            kind: "output",
            data: Some(data),
            exit_code: None,
            outcome: None,
        }
    }

    fn exited(term: u64, exit_code: Option<i32>, outcome: JobOutcome) -> Self {
        Self {
            term,
            kind: "exited",
            data: None,
            exit_code,
            outcome: Some(format!("{outcome:?}")),
        }
    }

    fn revoked(term: u64) -> Self {
        Self {
            term,
            kind: "revoked",
            data: None,
            exit_code: None,
            outcome: None,
        }
    }

    fn dropped(term: u64) -> Self {
        Self {
            term,
            kind: "dropped",
            data: None,
            exit_code: None,
            outcome: None,
        }
    }
}

/// The fail-closed error when a terminal command runs without a session under
/// full control. Uses the service's own `no_consented_session` kind so the UI
/// can tell it apart from a transport error.
fn no_session() -> CommandError {
    CommandError::new("no_consented_session", "no session is under full control")
}

/// Maps a service [`TargetError`] to a frontend [`CommandError`], preserving the
/// error kind as the stage (`no_consented_session`, `disconnected`, …) so the UI
/// keeps the fail-closed distinction the service drew.
fn terminal_error(err: TargetError) -> CommandError {
    CommandError::new(err.kind(), err)
}

/// Drains a terminal attachment's bounded event channel to the frontend on the
/// `"console-terminal"` event until the terminal exits, is revoked, or the
/// channel closes. Output frames carry raw bytes; an overrun (dropped) consumer
/// is surfaced honestly rather than stalling silently (REQ-N-008). Detaches the
/// consumer on the way out so the registry slot is freed (ST-M003-009).
fn spawn_terminal_stream(
    app: AppHandle,
    service: ConsoleSessionService,
    attachment: TermAttachment,
) {
    let term = attachment.term;
    let consumer = attachment.consumer;
    let overrun = attachment.overrun;
    let mut events = attachment.events;
    tauri::async_runtime::spawn(async move {
        while let Some(event) = events.recv().await {
            match event {
                TermEvent::Output(bytes) => {
                    let _ = app.emit("console-terminal", TerminalFrame::output(term.0, bytes));
                }
                TermEvent::Exited { exit_code, outcome } => {
                    let _ = app.emit(
                        "console-terminal",
                        TerminalFrame::exited(term.0, exit_code, outcome),
                    );
                    break;
                }
                TermEvent::Revoked => {
                    let _ = app.emit("console-terminal", TerminalFrame::revoked(term.0));
                    break;
                }
            }
        }
        // A consumer that overran its bounded window was dropped upstream; tell
        // the panel rather than leaving it looking live but frozen (REQ-N-008).
        if overrun.load(Ordering::SeqCst) {
            let _ = app.emit("console-terminal", TerminalFrame::dropped(term.0));
        }
        // Free the registry consumer slot. A terminal that already exited was
        // removed, so this detach is a no-op then (ST-M003-009).
        service.term_detach(term, consumer);
    });
}

/// Opens an interactive terminal on the consented session and starts streaming
/// its output to the frontend. Fails closed when no session is under full
/// control. Defaults to PowerShell; `"cmd"` selects Command Prompt.
#[tauri::command]
async fn open_terminal(
    app: AppHandle,
    state: State<'_, AppState>,
    cols: u16,
    rows: u16,
    shell: Option<String>,
) -> Result<TerminalInfo, CommandError> {
    let service = state.service().ok_or_else(no_session)?;
    let shell = match shell.as_deref() {
        Some("cmd") => Shell::Cmd,
        _ => Shell::PowerShell,
    };
    // The service stamps `Origin::Terminal`, keeping the technician terminal
    // distinct from Claude's MCP bridge in the audit log (ST-M003-009).
    let term = service
        .open_terminal(shell, cols.max(1), rows.max(1), None, BTreeMap::new())
        .await
        .map_err(terminal_error)?;
    // Attach one local consumer for this panel; attaching grants no new authority
    // (ST-M003-009). The bounded event channel is drained by the spawned task.
    let attachment = service.term_attach(term).map_err(terminal_error)?;
    let info = TerminalInfo {
        term: term.0,
        session: attachment.session.to_string(),
    };
    spawn_terminal_stream(app, service, attachment);
    Ok(info)
}

/// Feeds keystrokes to the terminal as raw bytes. The remote pty echoes them
/// back through the output stream, so the frontend never echoes locally.
#[tauri::command]
async fn term_input(
    state: State<'_, AppState>,
    term: u64,
    data: Vec<u8>,
) -> Result<(), CommandError> {
    let service = state.service().ok_or_else(no_session)?;
    service
        .term_input(JobId(term), data)
        .await
        .map_err(terminal_error)
}

/// Resizes the terminal's pty window so full-screen TUIs render correctly.
#[tauri::command]
async fn term_resize(
    state: State<'_, AppState>,
    term: u64,
    cols: u16,
    rows: u16,
) -> Result<(), CommandError> {
    let service = state.service().ok_or_else(no_session)?;
    service
        .term_resize(JobId(term), cols.max(1), rows.max(1))
        .await
        .map_err(terminal_error)
}

/// Closes the terminal, ending its remote pty. A terminal that already exited
/// maps to an honest error the panel drops.
#[tauri::command]
async fn term_close(state: State<'_, AppState>, term: u64) -> Result<(), CommandError> {
    let service = state.service().ok_or_else(no_session)?;
    service
        .term_close(JobId(term))
        .await
        .map_err(terminal_error)
}

/// Picks a private, non-loopback interface address to bind the listener to,
/// mirroring the console CLI (REQ-S-010: never a public or wildcard address).
/// The candidate list from `private_interface_addrs` is preference-ordered —
/// routable-private (RFC1918 / ULA) ahead of automatically-assigned link-local,
/// which is dropped unless it is the only option — so the GUI binds and
/// advertises the reachable LAN address instead of a stray `169.254` APIPA one
/// (ST-M002-010). Falls back to loopback only when no private
/// interface is found, so a bind always targets a concrete private address.
fn private_bind_ip() -> IpAddr {
    let candidates = private_interface_addrs().unwrap_or_default();
    candidates
        .iter()
        .find(|ip| !ip.is_loopback())
        .copied()
        .or_else(|| candidates.first().copied())
        .unwrap_or(IpAddr::V4(Ipv4Addr::LOCALHOST))
}

/// Runs the console application.
pub fn run() {
    let builder = tauri::Builder::default().manage(default_app_state());

    // The shipped exe exposes only the real-transport command surface. The
    // simulate-* developer commands are added only under `dev-simulate`, so a
    // release build has no invocable fabricated-consent path (ST-M002-009).
    #[cfg(feature = "dev-simulate")]
    let builder = builder.invoke_handler(tauri::generate_handler![
        get_state,
        confirm_code,
        report_code_mismatch,
        simulate_consent,
        simulate_target_lost,
        stop_session,
        start_live_session,
        open_terminal,
        term_input,
        term_resize,
        term_close
    ]);
    #[cfg(not(feature = "dev-simulate"))]
    let builder = builder.invoke_handler(tauri::generate_handler![
        get_state,
        confirm_code,
        report_code_mismatch,
        stop_session,
        start_live_session,
        open_terminal,
        term_input,
        term_resize,
        term_close
    ]);

    if let Err(error) = builder.run(tauri::generate_context!()) {
        eprintln!("fatal: the CretLink console failed to start: {error}");
        std::process::exit(1);
    }
}

/// The identity a simulated target reports once "connected". Uses the protocol
/// convention (`SIMULATED-TARGET`, `simulated: true`) so it can never be
/// mistaken for a real customer computer. Developer-only (ST-M002-009).
#[cfg(feature = "dev-simulate")]
fn simulated_identity() -> TargetIdentity {
    TargetIdentity {
        hostname: "SIMULATED-TARGET".to_string(),
        os_version: "Windows 11 (simulated)".to_string(),
        arch: "x86_64".to_string(),
        elevated: true,
        simulated: true,
        agent_version: "sim-0".to_string(),
    }
}

/// Detects the technician host name without a runtime dependency, falling back
/// to a neutral label when detection fails.
fn detect_hostname() -> String {
    std::process::Command::new("hostname")
        .output()
        .ok()
        .and_then(|out| String::from_utf8(out.stdout).ok())
        .map(|name| name.trim().to_string())
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "this-computer".to_string())
}

/// Builds this technician host descriptor from live host inspection. Always
/// present: it seeds both the live-idle console and the developer seed.
fn technician_host() -> TechnicianHost {
    TechnicianHost {
        label: "This computer (technician)".to_string(),
        hostname: detect_hostname(),
        os: format!("{} ({})", std::env::consts::OS, std::env::consts::ARCH),
    }
}

/// Builds the developer console state seeded with simulated targets. Compiled
/// only under `dev-simulate` (ST-M002-009).
#[cfg(feature = "dev-simulate")]
fn seed_state() -> ConsoleState {
    let mut console = ConsoleState::new(technician_host());
    console.upsert_target(DiscoveredTarget::new(
        TargetId::new(),
        "Simulated customer PC",
        "sim://reception-desk",
        DiscoverySource::Simulated,
    ));
    console.upsert_target(DiscoveredTarget::new(
        TargetId::new(),
        "Simulated customer laptop",
        "sim://back-office",
        DiscoverySource::Simulated,
    ));
    console
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(feature = "dev-simulate")]
    #[test]
    fn seeded_state_has_two_simulated_targets_and_no_selection() {
        let console = seed_state();
        assert_eq!(console.targets.len(), 2);
        assert!(console.targets.iter().all(DiscoveredTarget::is_simulated));
        assert!(console.selected.is_none());
        // The technician host is present and labelled, never a target.
        assert!(!console.technician.label.is_empty());
    }

    #[test]
    fn live_idle_is_the_shipped_default_with_no_targets() {
        // The shipped build boots live and idle: technician host present, no
        // targets, ready to open a real listening session (ST-M002-009, AC #4).
        let state = AppState::live_idle();
        let console = state.guard();
        assert!(console.targets.is_empty());
        assert!(console.selected.is_none());
        assert!(matches!(console.phase, SessionPhase::Idle));
        assert!(!console.technician.label.is_empty());
    }

    #[test]
    fn a_fresh_console_hosts_no_service_so_terminals_fail_closed() {
        // No session is under full control at boot, so the terminal service slot
        // is empty and every terminal command refuses before touching the wire
        // (ST-M008-001). The pump only publishes a service at full-control
        // hand-off, and clears it on teardown, so this is the resting state.
        let state = AppState::live_idle();
        assert!(state.service().is_none());
        // The refusal carries the service's own fail-closed kind, so the UI can
        // distinguish "no session" from a transport error.
        assert_eq!(no_session().stage, "no_consented_session");
    }

    #[test]
    fn a_terminal_output_frame_carries_raw_bytes_and_no_end_fields() {
        // Untrusted VT bytes ride through verbatim (the frontend writes them to
        // xterm, never as markup); an output frame has no exit/outcome fields.
        let frame = TerminalFrame::output(7, vec![0x1b, b'[', b'0', b'm']);
        assert_eq!(frame.term, 7);
        assert_eq!(frame.kind, "output");
        assert_eq!(frame.data.as_deref(), Some(&[0x1b, b'[', b'0', b'm'][..]));
        assert!(frame.exit_code.is_none());
        assert!(frame.outcome.is_none());
    }

    #[test]
    fn a_terminal_exit_frame_reports_the_code_and_outcome() {
        // An honest end: the panel gets the exit code and a human outcome label
        // rather than a silent stop (REQ-N-008).
        let frame = TerminalFrame::exited(3, Some(0), JobOutcome::Completed);
        assert_eq!(frame.kind, "exited");
        assert_eq!(frame.exit_code, Some(0));
        assert_eq!(frame.outcome.as_deref(), Some("Completed"));
        assert!(frame.data.is_none());
    }
}
