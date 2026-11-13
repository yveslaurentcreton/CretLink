//! The technician role, Give help (ST-M002-002, ST-M009-005).
//!
//! This module is a thin renderer over the Rust-owned session-UI state model in
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
use std::net::{IpAddr, Ipv4Addr};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use cretlink_console::{
    ChannelAudit, ConsoleSessionService, ConsoleState, SessionPhase, StopReason,
    TechnicianActivity, TechnicianHost, TermAttachment, TermEvent, technician_display_name,
    terminal_closed, terminal_open_started, terminal_opened,
};
// The simulated seed targets are developer-only: the shipped console listens and
// never picks a target, so these compile only under `dev-simulate`.
#[cfg(feature = "dev-simulate")]
use cretlink_console::{DiscoveredTarget, DiscoverySource, TargetId};
use cretlink_discovery::{RelayCode, default_route_addr, listen_candidates};
// The technician terminal (ST-M008-001) addresses a hosted terminal by `JobId`,
// picks a `Shell`, and reports its end via `JobOutcome`; `TargetError` is the
// service's fail-closed error surface, mapped straight to a frontend
// `CommandError` so `no_consented_session` stays legible in the UI.
use cretlink_mcp::TargetError;
use cretlink_protocol::{JobId, JobOutcome, RevokeReason, Shell};
// Only the fabricated-consent developer command binds a synthetic session id and
// simulated identity; they leave the shipped build entirely (ST-M002-009).
#[cfg(feature = "dev-simulate")]
use cretlink_protocol::{SessionId, TargetIdentity};
use cretlink_session::ActivityEntry;
use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager, State};

// The local MCP bridge for the rail (ST-M009-008).
mod bridge;
// The file explorer of the live session (ST-M010-002).
mod explorer;
// The session history and the audit files (ST-M009-012).
mod history;
// This computer's operating system by name (ST-M009-008).
mod host_os;
// The network the technician waits on (ST-M009-016).
mod network;
// Settings → Relay (ST-M007-011).
mod relay_settings;
mod session_driver;
// The task of one session: waiting on a network or on a relay (ST-M007-013).
mod session_task;
// This copy, its AI clients, and run once or install (ST-M009-006); the start
// screen registers part of it too.
pub(crate) mod setup;
use session_driver::{
    ActivitySlot, Intent, IntentSender, Place, ServiceSlot, StateSink, intent_channel, lock,
    lock_activity, lock_service,
};
use session_task::{RelayWait, Run, Wait};

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
    /// The current session's activity feed (ST-M009-011), read by
    /// [`get_activity`]. Kept after the session ends and replaced only when the
    /// next session starts.
    activity: ActivitySlot,
    /// The live session's audit sink while a session runs, so the in-process
    /// terminal commands record their actions as the `terminal` actor
    /// (REQ-S-023, ST-M009-011). Cleared before the log is sealed.
    audit: Arc<Mutex<Option<ChannelAudit>>>,
    /// The session log the audit drain is writing, left out of the history
    /// until it is sealed (ST-M009-012).
    audit_log: history::OpenLog,
    /// The local MCP bridge as the rail shows it: its endpoint and the AI
    /// clients that called during this session (ST-M009-008).
    bridge: bridge::BridgeSlot,
    /// The network adapter the technician chose to wait on (ST-M009-016).
    /// The installed copy keeps it in its settings, so that it holds after
    /// the program is closed (ST-M007-011); a copy that runs once keeps it
    /// in memory only.
    network: network::NetworkChoice,
    /// The certificate authority file the technician chose in Settings →
    /// Relay and has not saved yet (ST-M007-011).
    relay_ca: relay_settings::PendingCa,
    /// The task of the session that is live or was live last, so that a
    /// change of network can wait until the old listener is gone before it
    /// starts the new one (ST-M009-016).
    pump: PumpSlot,
    /// The folder listing the file explorer is waiting for, so that the next
    /// place the technician opens can leave it (ST-M010-002).
    listings: explorer::Listings,
    /// The transfer queue of the file explorer: what the technician sends to
    /// the customer computer and copies from it in this session, and what was
    /// dropped or picked to be sent (ST-M010-002).
    transfers: explorer::Transfers,
    /// The deletes of the file explorer that run, so that the window can
    /// cancel one (ST-M010-002).
    removals: explorer::Removals,
}

/// The slot of [`AppState::pump`].
type PumpSlot = Arc<Mutex<Option<tauri::async_runtime::JoinHandle<()>>>>;

impl AppState {
    /// Builds the shipped initial console: this technician host, live and idle,
    /// with no targets. Real discovery or [`start_live_session`] populate it; the
    /// UI never boots into a simulated driver.
    #[cfg_attr(
        feature = "dev-simulate",
        allow(
            dead_code,
            reason = "the dev-simulate build boots the seeded demo instead"
        )
    )]
    #[must_use]
    pub fn live_idle() -> Self {
        Self {
            inner: Arc::new(Mutex::new(ConsoleState::new(technician_host()))),
            live: Arc::new(Mutex::new(None)),
            service: Arc::new(Mutex::new(None)),
            activity: Arc::new(Mutex::new(TechnicianActivity::new())),
            audit: Arc::new(Mutex::new(None)),
            audit_log: history::OpenLog::default(),
            bridge: bridge::BridgeSlot::default(),
            network: network::NetworkChoice::of_this_copy(),
            relay_ca: relay_settings::PendingCa::default(),
            pump: PumpSlot::default(),
            listings: explorer::Listings::default(),
            transfers: explorer::Transfers::default(),
            removals: explorer::Removals::default(),
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
            activity: Arc::new(Mutex::new(TechnicianActivity::new())),
            audit: Arc::new(Mutex::new(None)),
            audit_log: history::OpenLog::default(),
            bridge: bridge::BridgeSlot::default(),
            network: network::NetworkChoice::of_this_copy(),
            relay_ca: relay_settings::PendingCa::default(),
            pump: PumpSlot::default(),
            listings: explorer::Listings::default(),
            transfers: explorer::Transfers::default(),
            removals: explorer::Removals::default(),
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

    /// The live session's service and its audit sink, for the technician's own
    /// file actions (ST-M010-002): both exist only while a session is under
    /// full control. Without either there is no consented session to act on,
    /// or no trail to record the action in, and the action is refused.
    fn files(&self) -> Option<(ConsoleSessionService, ChannelAudit)> {
        let service = self.service()?;
        let audit = self
            .audit
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()?;
        Some((service, audit))
    }

    /// Prepares the task of the next session: a new intent channel, an empty
    /// feed, an empty bridge status and an empty transfer queue, and the
    /// slots the task shares with the commands. `place` is where that task
    /// writes outside this process.
    fn next_run(&self, sink: Arc<dyn StateSink>, place: Place) -> Run {
        // Open the intent channel so the compare-gate confirm/mismatch
        // buttons and the emergency stop can reach the session's task, which
        // exclusively owns the handle (ST-M002-013). The task takes the
        // receiver; the sender lives in `live` and replaces any stale sender
        // from a prior session.
        let (intent_tx, intents) = intent_channel();
        *self.live.lock().unwrap_or_else(PoisonError::into_inner) = Some(intent_tx);
        // A new session starts a new feed; the previous one was kept until
        // now (ST-M009-011).
        *lock_activity(&self.activity) = TechnicianActivity::new();
        // A new session starts with no bridge endpoint and no AI clients.
        bridge::lock_bridge(&self.bridge).reset();
        // And with an empty transfer queue: what the session before it sent,
        // copied or was offered is dropped (ST-M010-002).
        self.transfers.reset();
        Run {
            shared: self.inner.clone(),
            sink,
            intents,
            // The task publishes the hosted service here at full-control
            // hand-off, making the technician terminal reachable in-process
            // (ST-M008-001). Any stale service from a prior session was
            // cleared on that session's fail-closed teardown.
            service: self.service.clone(),
            activity: self.activity.clone(),
            bridge: self.bridge.clone(),
            audit: self.audit.clone(),
            audit_log: self.audit_log.clone(),
            place,
        }
    }

    /// Sends a technician [`Intent`] to the running pump task, if a session is
    /// live. Returns `false` when there is no live session or the pump has already
    /// ended (a dropped receiver), so callers can decide honestly.
    fn send_intent(&self, intent: Intent) -> bool {
        let live = self.live.lock().unwrap_or_else(PoisonError::into_inner);
        live.as_ref().is_some_and(|tx| tx.send(intent).is_ok())
    }

    /// The current session's activity feed, oldest first (ST-M009-011).
    fn activity_entries(&self) -> Vec<ActivityEntry> {
        lock_activity(&self.activity).entries()
    }

    /// Records one technician-terminal audit entry on the live session's log
    /// (REQ-S-023, ST-M009-011); the pump's live echo then folds it into the
    /// activity feed. Without a live audit sink (no session) there is nothing
    /// to record.
    fn record_terminal(&self, entry: cretlink_console::AuditEntry) {
        let audit = self
            .audit
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        if let Some(audit) = audit {
            audit.record_entry(entry);
        }
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

/// Takes a session's audit sink back from the terminal commands when dropped,
/// so the last sender can go and the drain can seal the log (ST-M003-006).
struct AuditLent(Arc<Mutex<Option<ChannelAudit>>>);

impl Drop for AuditLent {
    fn drop(&mut self) {
        *self.0.lock().unwrap_or_else(PoisonError::into_inner) = None;
    }
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

    fn emit_activity(&self, entry: &ActivityEntry) {
        // Live activity rides a separate event (the frontend's `ActivityEvent`,
        // ST-M009-011) so a burst of actions never forces a whole
        // `console-state` re-render (REQ-N-006); a dropped frontend is likewise
        // not a backend error.
        let _ = self.app.emit("console-activity", entry);
    }

    fn emit_bridge(&self, status: &bridge::BridgeStatus) {
        // The rail's bridge status (ST-M009-008), on its own event.
        let _ = self.app.emit(bridge::BRIDGE_EVENT, status);
    }
}

impl Default for AppState {
    fn default() -> Self {
        default_app_state()
    }
}

/// The event a [`Notice`] is emitted on.
pub(crate) const NOTICE_EVENT: &str = "console-notice";

/// Something the technician should know that is not the failure of a command
/// (`ConsoleNotice` of the contract): what happened and a next step, in plain
/// words (REQ-N-015). The window shows it in its alert.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Notice {
    /// What this is about.
    pub title: String,
    /// What happened.
    pub message: String,
    /// What to do next.
    pub next: String,
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

/// Returns the current session's activity feed, oldest first (ST-M009-011):
/// every item and gap the `"console-activity"` event has carried, so a frontend
/// that (re)loads mid-session or after it ended shows the same feed. Kept until
/// the next session starts.
#[tauri::command]
fn get_activity(state: State<'_, AppState>) -> Vec<ActivityEntry> {
    state.activity_entries()
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
    confirm_now(&state)
}

/// [`confirm_code`], without Tauri.
fn confirm_now(state: &AppState) -> Result<ConsoleState, CommandError> {
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
    stop_now(&state)
}

/// [`stop_session`], without Tauri. While the console waits on a relay there
/// is no wire yet: the session's task then drops its waiting connection, and
/// the relay code leads to no session any more (ST-M007-013).
fn stop_now(state: &AppState) -> Result<ConsoleState, CommandError> {
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

    begin_listening(app, &state, listen_ip(&state.network));
    Ok(state.guard().clone())
}

/// How long a change of network waits for the old listener to go, and then
/// for the new one to report.
const NETWORK_SWITCH_TIMEOUT: Duration = Duration::from_secs(5);

/// Moves the waiting session to another of this computer's networks
/// (ST-M009-016).
///
/// Allowed only while the console waits and nobody is connecting. `address`
/// must be one of the candidates [`network::get_listen_networks`] lists; any
/// other address is refused before anything changes (REQ-S-010). The old
/// listener is stopped and awaited, so that its last events cannot reach the
/// new session's state, and a new listener starts on the chosen address with
/// a new connection code. The adapter is remembered for the later sessions of
/// this run.
///
/// The returned state is the new waiting state, or the honest failure when
/// the chosen address could not be bound (REQ-N-008).
#[tauri::command]
async fn switch_listen_network(
    app: AppHandle,
    state: State<'_, AppState>,
    address: String,
) -> Result<ConsoleState, CommandError> {
    {
        let console = state.guard();
        // A wait on a relay listens on no network of this computer.
        if !matches!(console.phase, SessionPhase::Listening { .. }) || console.relay.is_some() {
            return Err(CommandError::new(
                "network",
                "the network can be changed only while waiting for the customer on a network",
            ));
        }
    }
    let candidates = listen_candidates().map_err(|e| CommandError::new("network", e))?;
    let chosen = network::requested(&candidates, &address)
        .map_err(|message| CommandError::new("network", message))?
        .clone();

    // Stop waiting on the old address, exactly as Cancel does: the wire
    // revoke is best effort and the local stop is authoritative.
    let _ = state.send_intent(Intent::Stop(RevokeReason::TechnicianRequested));
    state
        .guard()
        .stop(StopReason::TechnicianRequested)
        .map_err(|e| CommandError::new("network", e))?;
    let previous = state
        .pump
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .take();
    if let Some(task) = previous {
        // Bounded: a listener that does not let go must not hold the window.
        let _ = tokio::time::timeout(NETWORK_SWITCH_TIMEOUT, task).await;
    }

    if let Err(reason) = state.network.set(&chosen.adapter) {
        // The choice holds for this run; that it will not be there next time
        // is said, not kept quiet (REQ-N-008).
        let _ = app.emit(
            NOTICE_EVENT,
            Notice {
                title: "The network choice could not be saved".to_string(),
                message: reason,
                next: "It holds until CretLink closes. Choose the network again next time, or \
                       check that your settings can be written."
                    .to_string(),
            },
        );
    }
    begin_listening(app, &state, chosen.addr);

    // The new listener reports from its own task: wait for it, so that the
    // answer is the new waiting state and not the stop in between.
    let deadline = tokio::time::Instant::now() + NETWORK_SWITCH_TIMEOUT;
    loop {
        {
            let console = state.guard();
            if !matches!(console.phase, SessionPhase::Stopped { .. }) {
                return Ok(console.clone());
            }
        }
        if tokio::time::Instant::now() >= deadline {
            return Ok(state.guard().clone());
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// The address to wait on: the adapter the technician chose earlier in this
/// run while it still has one, otherwise the first choice among this
/// computer's private addresses (ST-M009-016). Never a public or wildcard
/// address (REQ-S-010): without any private address the listener binds
/// loopback, so a bind always targets a concrete address that is not
/// reachable from outside.
fn listen_ip(choice: &network::NetworkChoice) -> IpAddr {
    let candidates = listen_candidates().unwrap_or_default();
    network::pick(&candidates, choice.get().as_deref(), default_route_addr())
        .map_or(IpAddr::V4(Ipv4Addr::LOCALHOST), |chosen| chosen.addr)
}

/// Starts the listener on `bind_ip` and the task that drives the session it
/// accepts; shared by [`start_live_session`] and [`switch_listen_network`].
fn begin_listening(app: AppHandle, state: &AppState, bind_ip: IpAddr) {
    let sink: Arc<dyn StateSink> = Arc::new(TauriSink { app });
    let task = begin_network_wait(state, sink, Place::of_this_user(), bind_ip);
    *state.pump.lock().unwrap_or_else(PoisonError::into_inner) = Some(task);
}

/// Starts the task of a session that waits for the customer on `bind_ip`.
/// The task enters the listening state itself, once the listener is bound
/// and its connection code is known.
fn begin_network_wait(
    state: &AppState,
    sink: Arc<dyn StateSink>,
    place: Place,
    bind_ip: IpAddr,
) -> tauri::async_runtime::JoinHandle<()> {
    let run = state.next_run(sink, place);
    tauri::async_runtime::spawn(run.drive(Wait::Network(bind_ip)))
}

// --- A session through the relay (ST-M007-013) ---

/// Refuses a relay unless the console waits for the customer on a network:
/// that is the one moment the waiting screen offers it.
fn relay_can_be_chosen(console: &ConsoleState) -> Result<(), CommandError> {
    if matches!(console.phase, SessionPhase::Listening { .. }) && console.relay.is_none() {
        Ok(())
    } else {
        Err(CommandError::new(
            "relay",
            "A relay can be chosen only while waiting for the customer.",
        ))
    }
}

/// The technician chose **Use relay** on the waiting screen: stop waiting on
/// this network, and wait for the customer on the relay of this copy instead
/// (ST-M007-013).
///
/// Allowed only while the console waits on a network and nobody is
/// connecting. The relay is the one of this copy's profile: the file next to
/// the program, or the settings of the installed copy. Without a profile, or
/// with one that cannot be used, nothing changes and the answer says what to
/// do (REQ-N-015). The old listener is stopped and awaited first, as for a
/// change of network, so that its last events cannot reach the new wait.
///
/// A new relay code is drawn for this wait; it is what the technician reads
/// to the customer. The answer is the new waiting state. Reaching the relay
/// happens in the session's own task: what comes of it arrives as
/// `console-state`, first that the relay holds the code, or the failure in
/// words.
///
/// The relay only lets the two copies find each other (ADR-0016): the
/// approval code is compared and the customer consents exactly as on one
/// network.
#[tauri::command]
async fn use_relay(
    app: AppHandle,
    state: State<'_, AppState>,
) -> Result<ConsoleState, CommandError> {
    relay_can_be_chosen(&state.guard())?;
    // Reading the profile touches the disk: not on this task.
    let relay = setup::blocking(relay_settings::relay_in_use).await?;
    let code = RelayCode::generate().map_err(|_| {
        CommandError::new(
            "relay",
            "This computer gave no randomness for a relay code. Try again.",
        )
    })?;

    // Stop waiting on the network, exactly as Cancel does: the wire revoke
    // is best effort and the local stop is authoritative. The customer may
    // have connected while the profile was read, so the check is made again
    // under the same lock that stops the wait.
    {
        let mut console = state.guard();
        relay_can_be_chosen(&console)?;
        let _ = state.send_intent(Intent::Stop(RevokeReason::TechnicianRequested));
        console
            .stop(StopReason::TechnicianRequested)
            .map_err(|e| CommandError::new("relay", e))?;
    }
    let previous = state
        .pump
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .take();
    if let Some(task) = previous {
        // Bounded: a listener that does not let go must not hold the window.
        let _ = tokio::time::timeout(NETWORK_SWITCH_TIMEOUT, task).await;
    }

    let sink: Arc<dyn StateSink> = Arc::new(TauriSink { app });
    let task = begin_relay_wait(
        &state,
        sink,
        Place::of_this_user(),
        RelayWait::new(relay, code),
    );
    *state.pump.lock().unwrap_or_else(PoisonError::into_inner) = Some(task);
    Ok(state.guard().clone())
}

/// Enters the waiting state of a session through a relay and starts the task
/// that keeps the rendezvous open there.
///
/// The state is entered here, before the task runs, so that the answer to
/// [`use_relay`] already shows the relay and the code: the relay code is
/// drawn on this side and needs nothing from the relay. The state says that
/// the relay is still being reached until the task reports that the relay
/// holds the code.
fn begin_relay_wait(
    state: &AppState,
    sink: Arc<dyn StateSink>,
    place: Place,
    wait: RelayWait,
) -> tauri::async_runtime::JoinHandle<()> {
    let run = state.next_run(Arc::clone(&sink), place);
    {
        let mut console = state.guard();
        let technician = console.technician.clone();
        *console = ConsoleState::new(technician);
        // A new console state always takes a wait.
        let _ = console.listen_on_relay(wait.code(), wait.host(), wait.address());
        sink.emit(&console);
    }
    tauri::async_runtime::spawn(run.drive(Wait::Relay(wait)))
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
    let (cols, rows) = (cols.max(1), rows.max(1));
    // The service stamps `Origin::Terminal`, keeping the technician terminal
    // distinct from the AI's MCP bridge (ST-M003-009); the open is recorded as
    // the `terminal` actor before and after it reaches the target (REQ-S-023,
    // ST-M009-011).
    let session = service.session();
    state.record_terminal(terminal_open_started(session, cols, rows));
    let started = std::time::Instant::now();
    let opened = service
        .open_terminal(shell, cols, rows, None, BTreeMap::new())
        .await;
    let elapsed = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    match &opened {
        Ok(term) => state.record_terminal(terminal_opened(session, cols, rows, Ok(*term), elapsed)),
        Err(error) => state.record_terminal(terminal_opened(
            session,
            cols,
            rows,
            Err(&error.to_string()),
            elapsed,
        )),
    }
    let term = opened.map_err(terminal_error)?;
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
    let started = std::time::Instant::now();
    let closed = service.term_close(JobId(term)).await;
    let elapsed = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    // Recorded as the `terminal` actor with its honest outcome (REQ-S-023).
    let reason = closed.as_ref().err().map(ToString::to_string);
    state.record_terminal(terminal_closed(
        service.session(),
        JobId(term),
        reason.as_deref().map_or(Ok(()), Err),
        elapsed,
    ));
    closed.map_err(terminal_error)
}

/// Adds the technician role's state and commands to the application. Only this
/// role's commands are registered in a technician process (ST-M009-005), plus
/// the shared [`crate::start::get_role`].
pub(crate) fn attach(builder: tauri::Builder<tauri::Wry>) -> tauri::Builder<tauri::Wry> {
    let builder = builder.manage(default_app_state());
    // Files and folders dropped on the window are kept on this side, and the
    // window is told their names: it never names a file to send by its path
    // (ST-M010-002).
    let builder = builder.on_window_event(|window, event| {
        if let tauri::WindowEvent::DragDrop(tauri::DragDropEvent::Drop { paths, .. }) = event {
            explorer::commands::dropped(window.app_handle(), paths);
        }
    });

    // The shipped exe exposes only the real-transport command surface. The
    // simulate-* developer commands are added only under `dev-simulate`, so a
    // release build has no invocable fabricated-consent path (ST-M002-009).
    #[cfg(feature = "dev-simulate")]
    let builder = builder.invoke_handler(tauri::generate_handler![
        crate::start::get_role,
        get_state,
        get_activity,
        bridge::get_bridge_status,
        confirm_code,
        report_code_mismatch,
        simulate_consent,
        simulate_target_lost,
        stop_session,
        start_live_session,
        switch_listen_network,
        use_relay,
        network::get_listen_networks,
        open_terminal,
        term_input,
        term_resize,
        term_close,
        explorer::explorer_drives,
        explorer::explorer_open,
        explorer::commands::explorer_pick_upload,
        explorer::commands::explorer_send_check,
        explorer::commands::explorer_send,
        explorer::commands::explorer_choose_folder,
        explorer::commands::explorer_download_check,
        explorer::commands::explorer_download,
        explorer::commands::explorer_transfers,
        explorer::commands::explorer_cancel_transfer,
        explorer::commands::explorer_cancel_transfers,
        explorer::commands::explorer_show_folder,
        explorer::commands::explorer_rename,
        explorer::commands::explorer_delete,
        explorer::commands::explorer_cancel_delete,
        history::list_sessions,
        history::open_audit,
        history::get_session_audit,
        setup::get_app_info,
        setup::ai_clients,
        setup::ai_client_install,
        setup::ai_client_uninstall,
        setup::uninstall_plan,
        setup::uninstall_app,
        relay_settings::get_relay_settings,
        relay_settings::choose_relay_ca,
        relay_settings::save_relay,
        relay_settings::test_relay,
        relay_settings::remove_relay
    ]);
    #[cfg(not(feature = "dev-simulate"))]
    let builder = builder.invoke_handler(tauri::generate_handler![
        crate::start::get_role,
        get_state,
        get_activity,
        bridge::get_bridge_status,
        confirm_code,
        report_code_mismatch,
        stop_session,
        start_live_session,
        switch_listen_network,
        use_relay,
        network::get_listen_networks,
        open_terminal,
        term_input,
        term_resize,
        term_close,
        explorer::explorer_drives,
        explorer::explorer_open,
        explorer::commands::explorer_pick_upload,
        explorer::commands::explorer_send_check,
        explorer::commands::explorer_send,
        explorer::commands::explorer_choose_folder,
        explorer::commands::explorer_download_check,
        explorer::commands::explorer_download,
        explorer::commands::explorer_transfers,
        explorer::commands::explorer_cancel_transfer,
        explorer::commands::explorer_cancel_transfers,
        explorer::commands::explorer_show_folder,
        explorer::commands::explorer_rename,
        explorer::commands::explorer_delete,
        explorer::commands::explorer_cancel_delete,
        history::list_sessions,
        history::open_audit,
        history::get_session_audit,
        setup::get_app_info,
        setup::ai_clients,
        setup::ai_client_install,
        setup::ai_client_uninstall,
        setup::uninstall_plan,
        setup::uninstall_app,
        relay_settings::get_relay_settings,
        relay_settings::choose_relay_ca,
        relay_settings::save_relay,
        relay_settings::test_relay,
        relay_settings::remove_relay
    ]);
    builder
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
        account: None,
    }
}

/// Detects the technician host name without a runtime dependency, falling back
/// to a neutral label when detection fails.
///
/// No program is looked up by name: Windows would search the running
/// program's own folder first. Windows reads `COMPUTERNAME`, Linux the
/// kernel's host name, and macOS runs `/bin/hostname` by its absolute path.
fn detect_hostname() -> String {
    os_hostname()
        .map(|name| name.trim().to_string())
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "this-computer".to_string())
}

#[cfg(windows)]
fn os_hostname() -> Option<String> {
    std::env::var("COMPUTERNAME").ok()
}

#[cfg(target_os = "linux")]
fn os_hostname() -> Option<String> {
    std::fs::read_to_string("/proc/sys/kernel/hostname").ok()
}

#[cfg(all(unix, not(target_os = "linux")))]
fn os_hostname() -> Option<String> {
    std::process::Command::new("/bin/hostname")
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()
        .and_then(|out| String::from_utf8(out.stdout).ok())
}

#[cfg(not(any(windows, unix)))]
fn os_hostname() -> Option<String> {
    None
}

/// The DNS-SD instance name this console advertises: its host name when that
/// is one plain DNS label, else the product's neutral name (ST-M009-009).
fn advertised_instance(hostname: &str) -> String {
    let label = hostname.split('.').next().unwrap_or_default();
    let plain = !label.is_empty()
        && label.len() <= 63
        && label.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
        && !label.starts_with('-')
        && !label.ends_with('-');
    if plain {
        label.to_owned()
    } else {
        "cretlink-console".to_owned()
    }
}

/// Builds this technician host descriptor from live host inspection. Always
/// present: it seeds both the live-idle console and the developer seed.
fn technician_host() -> TechnicianHost {
    TechnicianHost {
        label: "This computer (technician)".to_string(),
        hostname: detect_hostname(),
        // The product name and release, such as "Windows 11 Pro 25H2"
        // (ST-M009-008).
        os: host_os::name(),
        // The account's full name, else its user name (ST-M009-011).
        display_name: technician_display_name().map(|name| name.as_str().to_string()),
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
    fn get_activity_reads_the_shared_feed_the_pump_folds_into() {
        // A fresh console has an empty feed; what the pump folds into the shared
        // slot is exactly what `get_activity` returns (ST-M009-011).
        let state = AppState::live_idle();
        assert!(state.activity_entries().is_empty());
        let mut entry = cretlink_console::AuditEntry::event(
            cretlink_shared::Actor::Mcp,
            None,
            "mcp_remote_list_jobs_start",
            cretlink_shared::Outcome::Ok,
        );
        entry
            .args
            .insert("call_id", cretlink_shared::AuditValue::Int(1));
        let emitted = lock_activity(&state.activity).apply(&entry, 0);
        assert_eq!(
            state.activity_entries(),
            emitted.into_iter().collect::<Vec<_>>()
        );
        // No session runs, so no terminal record can be written.
        state.record_terminal(entry);
        assert_eq!(state.activity_entries().len(), 1);
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

    #[cfg(target_os = "linux")]
    #[test]
    fn the_host_name_comes_from_the_kernel_without_running_a_program() {
        let kernel = std::fs::read_to_string("/proc/sys/kernel/hostname").unwrap_or_default();
        let kernel = kernel.trim();
        if !kernel.is_empty() {
            assert_eq!(detect_hostname(), kernel);
        }
    }

    #[test]
    fn this_computer_is_described_by_its_host_name_and_operating_system_name() {
        // The foot of the navigation shows both (ST-M009-008): the operating
        // system is the name `host_os` reads, for the console state and for
        // this copy's details alike.
        let host = technician_host();
        assert!(!host.hostname.is_empty());
        assert_eq!(host.os, host_os::name());
        assert_eq!(setup::app_info().os, host.os);
    }

    #[test]
    fn a_relay_can_be_chosen_only_while_waiting_on_a_network() {
        let mut console = ConsoleState::new(TechnicianHost {
            label: "This computer (technician)".to_string(),
            hostname: "TECH-LAPTOP".to_string(),
            os: "Windows 11".to_string(),
            display_name: None,
        });
        // Not before a session was started.
        assert_eq!(relay_can_be_chosen(&console).unwrap_err().stage, "relay");
        // While waiting on a network: yes.
        console.listen("6RRR-2H8F", "10.0.0.42:7000").unwrap();
        assert!(relay_can_be_chosen(&console).is_ok());
        // Not once a customer is connecting: their session is not dropped
        // for a relay.
        console
            .customer_arrived(
                cretlink_console::TargetId::new(),
                "Incoming customer computer",
            )
            .unwrap();
        assert!(relay_can_be_chosen(&console).is_err());
        // Not while already waiting on a relay, and not after the end.
        let technician = console.technician.clone();
        let mut console = ConsoleState::new(technician);
        console
            .listen_on_relay(
                "53ST-A71D-JKKV-0VGH",
                "relay.example.org",
                "relay.example.org:443",
            )
            .unwrap();
        assert!(relay_can_be_chosen(&console).is_err());
        console.stop(StopReason::TechnicianRequested).unwrap();
        assert!(relay_can_be_chosen(&console).is_err());
    }

    #[test]
    fn stop_while_waiting_on_a_relay_ends_the_wait_and_tells_its_task() {
        let state = AppState::live_idle();
        let (intents, mut received) = intent_channel();
        *state.live.lock().unwrap_or_else(PoisonError::into_inner) = Some(intents);
        state
            .guard()
            .listen_on_relay(
                "53ST-A71D-JKKV-0VGH",
                "relay.example.org",
                "relay.example.org:443",
            )
            .unwrap_or_default();
        let stopped = stop_now(&state).unwrap_or_else(|error| panic!("{error:?}"));
        assert_eq!(
            stopped.phase,
            SessionPhase::Stopped {
                reason: StopReason::TechnicianRequested
            }
        );
        // The task that waits on the relay is told: it drops the connection.
        assert!(matches!(
            received.try_recv(),
            Ok(Intent::Stop(RevokeReason::TechnicianRequested))
        ));
        // There is no approval code to confirm in a wait.
        assert_eq!(
            confirm_now(&state).map(|_| ()).map_err(|e| e.stage),
            Err("compare".to_owned())
        );
    }

    #[test]
    fn the_advertised_instance_is_the_host_name_when_plain() {
        assert_eq!(advertised_instance("TECH-LAPTOP"), "TECH-LAPTOP");
        assert_eq!(advertised_instance("host-01.example.org"), "host-01");
        for odd in ["", "this computer", "-x", "naïve", &"a".repeat(64)] {
            assert_eq!(advertised_instance(odd), "cretlink-console", "{odd:?}");
        }
    }
}
