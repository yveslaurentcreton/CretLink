//! The customer role, Get help (ST-M002-005, ST-M002-009, ST-M009-005).
//!
//! This module is the customer-facing UI half of the *one informed full-control
//! consent* story, the agent-side mirror of the console GUI. It is a thin
//! renderer over the automated, cross-platform state model in
//! [`cretlink_support_agent::state`] (ADR-0011): it holds one [`AgentState`]
//! behind a mutex, exposes a small set of Tauri commands over it, and serialises
//! a plain [`ConsentView`] snapshot back to the plain-TypeScript frontend on
//! every change. The frontend holds no session authority of its own, so no
//! transition can reach full control except through the customer's single
//! explicit [`grant_consent`] action (REQ-S-001, REQ-S-003).
//!
//! The consent language is the canonical
//! [`DisclosureSummary::DEFAULT_TEXT`](cretlink_session::DisclosureSummary) sent
//! verbatim on the wire, so this window shows the customer the exact same words
//! the console shows the technician (REQ-S-016).
//!
//! # Live by default; the demo is a dev-only build
//!
//! The shipped release boots **resting and honest** (ST-M002-009): this
//! machine's real identity, no simulated demo, waiting for the customer to enter
//! the technician's connection code. [`start_live_session`] then dials a real
//! listening technician console (the agent is the Noise **initiator**,
//! ADR-0003), parsing the connection code the technician reads out, announcing
//! this machine's **honest** startup identity (REQ-S-011), and folding the real
//! lifecycle event stream through the shared agent adapter in [`session_driver`].
//! Control never appears locally: it is shown only when the customer's real
//! [`grant_consent`](cretlink_session::JoinHandle::grant_consent) echoes back
//! from the wire and the pump folds it. Everything is fail-closed — a dropped
//! transport ends in an honest terminal state, never a frozen "under control"
//! surface (REQ-S-009).
//!
//! The clearly-labelled **simulated** consent demo — where granting binds a
//! simulated [`SessionId`] and confers no real control — is compiled only under
//! the `dev-simulate` cargo feature. The shipped exe therefore has no way to
//! reach a fabricated-consent path (AC #4).

use std::net::SocketAddr;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use cretlink_discovery::{Browser, DiscoveredConsole, DiscoveryError, parse_manual_entry};
use cretlink_protocol::{CAP_FS_V1, PROTOCOL_V1, RevokeReason, SessionId, TargetIdentity};
use cretlink_session::{DisclosureSummary, SessionJoiner};
use cretlink_support_agent::{AgentPhase, AgentState, VisibleStatus, current_identity, sysinfo};
use serde::Serialize;
use tauri::{AppHandle, Emitter, State};

mod session_driver;
use session_driver::{ActivityDelta, Intent, ViewSink, is_terminal, lock, pump};

/// How long a single LAN browse listens before returning what it has collected.
/// A short, bounded window keeps discovery responsive and denies a hostile peer
/// the ability to hold the browse open (REQ-N-006).
const BROWSE_WINDOW: Duration = Duration::from_secs(2);

/// The most discovered consoles the customer is shown at once. The transport
/// deliberately returns an *uncapped* list — a LAN peer can advertise many
/// distinct instance names — so the UI caps it here before display (ADR-0007).
const MAX_CONSOLES: usize = 16;

/// The consent window's single source of truth, shared across command
/// invocations and the background live-session pump. Both hold the same
/// `Arc<Mutex<..>>`, so the synchronous commands and the async event stream can
/// never disagree about the current state.
#[derive(Debug, Clone)]
pub struct AppState {
    inner: Arc<Mutex<ConsentSession>>,
}

impl AppState {
    /// Builds the shipped initial window: a resting, honest consent window ready
    /// to dial a real technician. No simulated demo exists in this build.
    #[must_use]
    #[cfg_attr(
        feature = "dev-simulate",
        allow(
            dead_code,
            reason = "the dev-simulate build boots the seeded demo instead"
        )
    )]
    pub fn idle() -> Self {
        Self {
            inner: Arc::new(Mutex::new(ConsentSession::idle())),
        }
    }

    /// Builds the window seeded with the clearly-labelled simulated consent demo.
    /// Compiled only under `dev-simulate`, so the shipped exe can never boot into
    /// the demo (ST-M002-009, AC #4).
    #[cfg(feature = "dev-simulate")]
    #[must_use]
    pub fn seeded() -> Self {
        Self {
            inner: Arc::new(Mutex::new(ConsentSession::seeded())),
        }
    }

    fn guard(&self) -> MutexGuard<'_, ConsentSession> {
        lock(&self.inner)
    }
}

/// The initial window for both [`run`] and [`Default`]: the resting live-idle
/// window in the shipped build, and the simulated demo only under `dev-simulate`.
#[cfg(feature = "dev-simulate")]
fn default_app_state() -> AppState {
    AppState::seeded()
}

/// The initial window for both [`run`] and [`Default`]: resting and honest.
#[cfg(not(feature = "dev-simulate"))]
fn default_app_state() -> AppState {
    AppState::idle()
}

impl Default for AppState {
    fn default() -> Self {
        default_app_state()
    }
}

/// Renders consent-view snapshots to the Tauri frontend by emitting an
/// `"agent-view"` event carrying the whole serialized [`ConsentView`]. The
/// frontend holds no session authority of its own; it only reflects what it
/// receives.
#[derive(Debug)]
struct TauriSink {
    app: AppHandle,
}

impl ViewSink for TauriSink {
    fn emit(&self, view: &ConsentView) {
        // A dropped frontend (e.g. window closed) is not an error the backend
        // must act on; the session lifecycle continues to be driven and audited.
        let _ = self.app.emit("agent-view", view);
    }

    fn activity(&self, delta: &ActivityDelta) {
        // The activity mirror rides a separate `"agent-activity"` channel so the
        // higher-frequency command stream never forces a whole `ConsentView`
        // re-render. Like `emit`, a dropped frontend is not a backend error — the
        // feed is non-authoritative and the lifecycle continues underneath.
        let _ = self.app.emit("agent-activity", delta);
    }
}

/// The window's session: the authoritative [`AgentState`] plus the display-only
/// Session Approval Code to compare, and — in a live session — the intent
/// channel to the pump task that owns the wire handle.
#[derive(Debug)]
pub(crate) struct ConsentSession {
    /// The authoritative agent state model (cross-platform, unit-tested).
    pub(crate) agent: AgentState,
    /// The session id the *demo* binds consent to. In a live session the id
    /// comes from the wire via the pump's `EventContext`, so this is unused on
    /// the live path.
    demo_session: SessionId,
    /// The Session Approval Code to compare aloud before consent (REQ-F-005). In
    /// the demo it is derived from `demo_session`; in a live session it is the
    /// real wire code, refreshed on every fold. A display comparison value, not
    /// a secret and not key material. `None` when there is no code to show yet.
    approval_code: Option<String>,
    /// False for a live dialed session, true for the local demo. Drives the
    /// "SIMULATED" banner and the demo-only affordances.
    simulated: bool,
    /// The intent channel to the live pump task, present only during a live
    /// session. Consent/decline/stop route their wire action through this
    /// instead of acting locally, so control is never fabricated on this side.
    live: Option<session_driver::IntentSender>,
}

impl ConsentSession {
    /// A resting, honest consent window for the shipped build: this machine's
    /// real identity, no simulated demo, no live channel, and no code. It waits
    /// for the customer to enter the technician's connection code and dial out
    /// via [`start_live_session`]; control can then appear only when the real
    /// wire consent echoes back (REQ-S-001). Never `simulated`.
    #[cfg_attr(
        feature = "dev-simulate",
        allow(
            dead_code,
            reason = "the dev-simulate build boots the seeded demo instead"
        )
    )]
    fn idle() -> Self {
        Self {
            agent: AgentState::new(current_identity()),
            // Unused on the resting/live path; the live session id comes from the
            // wire. Present only to satisfy the shared shape.
            demo_session: SessionId::new(),
            approval_code: None,
            simulated: false,
            live: None,
        }
    }

    /// A fresh, clearly-labelled simulated consent session, driven to
    /// [`AgentPhase::AwaitingConsent`] so the disclosure and decision can be
    /// previewed immediately. Compiled only under `dev-simulate` (ST-M002-009).
    #[cfg(feature = "dev-simulate")]
    fn seeded() -> Self {
        let demo_session = SessionId::new();
        let approval_code = approval_code(demo_session);
        let mut agent = AgentState::new(current_identity());
        // Drive the honest lifecycle to the consent step — the exact transitions
        // the real runtime performs before asking the customer. These cannot
        // fail from a fresh `Idle` agent, but we degrade by leaving the agent as
        // it is rather than panicking (panic is denied here).
        let _ = agent.announce();
        let _ = agent.begin_pairing();
        let _ = agent.begin_compare();
        let _ = agent.await_consent();
        Self {
            agent,
            demo_session,
            approval_code: Some(approval_code),
            simulated: true,
            live: None,
        }
    }

    /// A live, dialing consent session: an honest agent announced locally, with
    /// its intent channel to the pump. Pairing, the approval code, and consent
    /// then arrive from the wire — no control without the customer's grant
    /// echoing back (REQ-S-001).
    pub(crate) fn live_dialing(intents: session_driver::IntentSender) -> Self {
        let mut agent = AgentState::new(current_identity());
        // Announce locally so the window honestly shows it is reaching out, not
        // idle; the adapter expects an announced agent before folding the wire
        // `Connected`.
        let _ = agent.announce();
        Self {
            agent,
            demo_session: SessionId::new(),
            approval_code: None,
            simulated: false,
            live: Some(intents),
        }
    }

    /// Replaces the candidate Session Approval Code (the live wire code, or
    /// `None`). Whether it is actually shown is gated by phase in
    /// [`visible_approval_code`](Self::visible_approval_code).
    pub(crate) fn set_approval_code(&mut self, code: Option<String>) {
        self.approval_code = code;
    }

    /// The approval code is visible across the whole pre-decision window — while
    /// pairing, while the codes are being compared aloud, and while awaiting
    /// consent — and hidden once control is granted or the session ends. The
    /// live path parks in [`AgentPhase::AwaitingApprovalCompare`] (the wire's
    /// held compare gate), so it MUST be included or the customer sees no code to
    /// compare in a real session (ST-M002-013).
    fn visible_approval_code(&self) -> Option<String> {
        match self.agent.phase() {
            AgentPhase::Pairing
            | AgentPhase::AwaitingApprovalCompare
            | AgentPhase::AwaitingConsent => self.approval_code.clone(),
            _ => None,
        }
    }

    /// Whether the window may start a live dial now: only when no live session
    /// is currently in flight (a finished one may be retried).
    fn can_connect(&self) -> bool {
        self.live.is_none() || is_terminal(self)
    }

    /// The immutable snapshot the consent window renders. It hides nothing: the
    /// visible status (phase, honest identity, whether control is active), the
    /// approval code to compare, the canonical disclosure, and the flags the
    /// frontend uses to show the demo banner and the connect panel.
    pub(crate) fn view(&self) -> ConsentView {
        ConsentView {
            status: self.agent.visible_status(),
            approval_code: self.visible_approval_code(),
            disclosure: DisclosureSummary::DEFAULT_TEXT.to_string(),
            simulated: self.simulated,
            can_connect: self.can_connect(),
        }
    }
}

/// What the consent window renders (see [`ConsentSession::view`]).
#[derive(Debug, Clone, Serialize)]
pub struct ConsentView {
    /// The authoritative agent visible status (REQ-S-002).
    pub status: VisibleStatus,
    /// The Session Approval Code to compare with the technician's console,
    /// present only before the consent decision. Display-only.
    pub approval_code: Option<String>,
    /// The canonical full-control disclosure, identical to the console and CLI
    /// agent (REQ-S-016).
    pub disclosure: String,
    /// Whether this window is the local demo (a live session sets this false);
    /// granting in the demo binds a simulated session and confers no real
    /// control.
    pub simulated: bool,
    /// Whether the connect panel should offer to dial a real technician now.
    pub can_connect: bool,
}

/// One console the customer may pick from the discovered list (ADR-0007).
///
/// **Untrusted.** Every field is derived from mDNS announcements a hostile LAN
/// peer can forge, replay, or flood; a candidate is an addressing hint for the
/// pick-list, never an authenticated peer. Selecting one grants no authority —
/// trust is still established only by the Session Approval Code (REQ-S-006). The
/// frontend must render every field with `textContent` and must never
/// pre-select or auto-dial a candidate.
#[derive(Debug, Clone, Serialize)]
pub struct DiscoveredConsoleView {
    /// The DNS-SD instance label. A display hint only.
    pub instance_name: String,
    /// The advertised human-readable name, if any. Attacker-controlled; shown,
    /// never trusted as identity.
    pub display_name: Option<String>,
    /// The dial target as `ip:port` (the first advertised address), or empty
    /// when the candidate advertised no address.
    pub address: String,
    /// The advertised `major.minor` protocol hint, if it parsed. A pre-filter
    /// hint only; the authoritative check happens during pairing.
    pub protocol_version: Option<String>,
    /// Whether [`address`](Self::address) is a usable dial target. A candidate
    /// with no address is shown but cannot be dialed.
    pub dialable: bool,
}

/// Projects one untrusted [`DiscoveredConsole`] into its display view. The first
/// advertised address (the list is already sorted for stable display) becomes
/// the `ip:port` dial hint; a candidate that advertised no address is still
/// shown but is not dialable — nothing is fabricated to fill the gap. No field
/// is trusted and none pre-selects a candidate (ADR-0007, REQ-S-006).
fn map_console(console: DiscoveredConsole) -> DiscoveredConsoleView {
    let address = console
        .addresses
        .first()
        .map(|ip| SocketAddr::new(*ip, console.port).to_string())
        .unwrap_or_default();
    let dialable = !address.is_empty();
    DiscoveredConsoleView {
        instance_name: console.instance_name,
        display_name: console.display_name,
        address,
        protocol_version: console.protocol_version.map(|v| v.to_string()),
        dialable,
    }
}

/// Caps the raw browse result to [`MAX_CONSOLES`] and projects each candidate.
/// The cap is applied *here*, before display, because the transport returns an
/// uncapped list on purpose (ADR-0007): a hostile LAN peer must not be able to
/// inflate the pick-list without bound.
fn map_consoles(consoles: Vec<DiscoveredConsole>) -> Vec<DiscoveredConsoleView> {
    consoles
        .into_iter()
        .take(MAX_CONSOLES)
        .map(map_console)
        .collect()
}

/// Turns a browse failure into a safe, actionable message (REQ-N-015). A host
/// with no private LAN address is an expected fall-back case, not a fault to
/// bury: its message points the customer at the always-available connection code
/// (REQ-S-010).
fn browse_error_message(error: &DiscoveryError) -> String {
    match error {
        DiscoveryError::NoPrivateInterface => "no private network was found to search; \
             ask the technician to read out the connection code instead"
            .to_string(),
        other => format!("could not search the local network: {other}"),
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

/// Returns the current consent view to the frontend.
#[tauri::command]
fn get_view(state: State<'_, AppState>) -> ConsentView {
    state.guard().view()
}

/// The customer's single informed full-control consent: the only path to
/// control (REQ-S-001, REQ-S-003). Valid only while awaiting consent.
///
/// In a live session this never grants locally — it sends the real wire consent
/// and full control appears only when the wire `ConsentGranted` echoes back and
/// the pump folds it. In the demo it binds the simulated session id directly.
#[tauri::command]
fn grant_consent(state: State<'_, AppState>) -> Result<ConsentView, CommandError> {
    let mut session = state.guard();
    if !matches!(session.agent.phase(), AgentPhase::AwaitingConsent) {
        return Err(CommandError::new(
            "consent",
            "there is no pending consent to grant",
        ));
    }
    if let Some(tx) = session.live.as_ref() {
        tx.send(Intent::Grant)
            .map_err(|_| CommandError::new("consent", "the session is no longer live"))?;
        return Ok(session.view());
    }
    let bound = session.demo_session;
    session
        .agent
        .grant_consent(bound)
        .map_err(|e| CommandError::new("consent", e))?;
    Ok(session.view())
}

/// The customer declines the pending consent; no control is ever granted.
///
/// In a live session this sends the wire decline and also ends the local model
/// immediately (stop only removes authority and is idempotent, so this is
/// fail-closed even if the wire is already gone).
#[tauri::command]
fn decline_consent(state: State<'_, AppState>) -> Result<ConsentView, CommandError> {
    let mut session = state.guard();
    if !matches!(session.agent.phase(), AgentPhase::AwaitingConsent) {
        return Err(CommandError::new(
            "consent",
            "there is no pending consent to decline",
        ));
    }
    if let Some(tx) = session.live.as_ref() {
        // Best-effort wire decline; the local stop below is authoritative.
        let _ = tx.send(Intent::Decline("customer declined".to_string()));
    }
    // Declining is a clean customer-initiated stop before any control: no
    // failure, no control, session over.
    session.agent.stop();
    Ok(session.view())
}

/// The emergency stop, always available while a session is live: revokes control
/// and rejects new actions (REQ-F-018, REQ-S-004). Fail-closed and idempotent.
///
/// In a live session it sends the wire revoke and ends the local model at once,
/// so the surface goes safe immediately even if the wire acknowledgement is
/// delayed or lost.
#[tauri::command]
fn stop_session(state: State<'_, AppState>) -> Result<ConsentView, CommandError> {
    let mut session = state.guard();
    if let Some(tx) = session.live.as_ref() {
        let _ = tx.send(Intent::Stop(RevokeReason::CustomerRequested));
    }
    session.agent.stop();
    Ok(session.view())
}

/// Simulates the technician ending the session from the console, exercising the
/// bilateral stop path (REQ-S-004). Developer-only: compiled out of the shipped
/// exe (ST-M002-009). On the live path the real technician revoke arrives over
/// the wire instead.
#[cfg(feature = "dev-simulate")]
#[tauri::command]
fn simulate_technician_stop(state: State<'_, AppState>) -> Result<ConsentView, CommandError> {
    let mut session = state.guard();
    if session.live.is_some() && !is_terminal(&session) {
        return Err(CommandError::new(
            "session",
            "not available during a live session",
        ));
    }
    session.agent.remote_stop();
    Ok(session.view())
}

/// Re-arms the demo with a fresh simulated session at the consent step, so the
/// disclosure flow can be exercised repeatedly. A reconnect always re-pairs and
/// re-consents — control is never silently restored (REQ-S-009). Refused while a
/// live session is still in progress. Developer-only: compiled out of the
/// shipped exe (ST-M002-009).
#[cfg(feature = "dev-simulate")]
#[tauri::command]
fn reset_demo(state: State<'_, AppState>) -> Result<ConsentView, CommandError> {
    let mut session = state.guard();
    if session.live.is_some() && !is_terminal(&session) {
        return Err(CommandError::new(
            "session",
            "stop the live session before restarting the demo",
        ));
    }
    *session = ConsentSession::seeded();
    Ok(session.view())
}

/// Fails closed if a live session is already in flight, keeping exactly one live
/// endpoint per window. A finished (terminal) session may be retried. Both dial
/// entry points — [`start_live_session`] and [`connect_console`] — gate on this
/// before touching any state, so a second dial can never race a live one.
fn ensure_can_connect(session: &ConsentSession) -> Result<(), CommandError> {
    if session.live.is_some() && !is_terminal(session) {
        return Err(CommandError::new(
            "connect",
            "a session is already in progress; stop it before connecting again",
        ));
    }
    Ok(())
}

/// Begins a live dial to an already-validated address: announces this machine's
/// honest identity locally, shows the "reaching out" state, and spawns the pump
/// that folds the real wire lifecycle.
///
/// This is the **single** dial path. A hand-typed connection code
/// ([`start_live_session`]) and a console picked from discovery
/// ([`connect_console`]) both resolve to a [`SocketAddr`] through the same
/// [`parse_manual_entry`] and then arrive here, so a discovered console is dialed
/// with exactly the authority of a typed address: none until the Session Approval
/// Code (REQ-S-006). Control never appears locally — it shows only when the wire's
/// `ConsentGranted` echoes back through the pump (REQ-S-001).
fn begin_live_dial(app: AppHandle, state: &State<'_, AppState>, addr: SocketAddr) -> ConsentView {
    // The honest, effective-identity-derived identity this agent presents. The
    // wire never overrides it (REQ-S-011, REQ-F-016).
    let identity = honest_target_identity();

    let (tx, rx) = session_driver::intent_channel();
    let sink: Arc<dyn ViewSink> = Arc::new(TauriSink { app });

    {
        let mut session = state.guard();
        *session = ConsentSession::live_dialing(tx);
        // Show the "reaching out" state right away; the pump takes over once the
        // dial resolves.
        sink.emit(&session.view());
    }

    let shared = state.inner.clone();
    tauri::async_runtime::spawn(async move {
        // Dial the listening console. A dial failure is an honest terminal
        // state, never a silent no-op.
        let handle =
            match SessionJoiner::join(addr, identity, PROTOCOL_V1, honest_capabilities()).await {
                Ok(handle) => handle,
                Err(error) => {
                    let mut session = lock(&shared);
                    session.agent.fail(
                        "connect",
                        format!("could not reach the technician: {error}"),
                    );
                    sink.emit(&session.view());
                    return;
                }
            };
        // Fold the live event stream (and route button intents) until it ends.
        pump(handle, shared, sink, rx).await;
    });

    state.guard().view()
}

/// Dials a real listening technician console from a typed connection code and
/// drives the session live (ST-M002-009).
///
/// The agent is the Noise **initiator** (ADR-0003): it parses the connection
/// code the technician reads out, dials the console, and announces this
/// machine's **honest** startup identity (REQ-S-011) — never a value adopted
/// from the wire. Every pairing, consent, and teardown event then flows through
/// the shared agent adapter in [`session_driver`], so the model on screen tracks
/// the real transport with no fabricated control (REQ-S-001).
///
/// Refusing to start while a live session is in progress keeps exactly one live
/// endpoint per window.
#[tauri::command]
fn start_live_session(
    app: AppHandle,
    code: String,
    state: State<'_, AppState>,
) -> Result<ConsentView, CommandError> {
    ensure_can_connect(&state.guard())?;

    // Parse the technician's connection code into a dialable address *before*
    // mutating any state, so a typo leaves the window untouched. This grants no
    // authority; trust is established later by the Session Approval Code.
    let target = parse_manual_entry(code.trim()).map_err(|e| CommandError::new("connect", e))?;

    Ok(begin_live_dial(app, &state, target.addr()))
}

/// Browses the LAN for listening consoles and returns the capped candidate list
/// for the customer to pick from (ADR-0007).
///
/// This performs discovery only: no dial, no pre-selection, no authority. The
/// blocking mDNS browse runs on a blocking thread so the async runtime is never
/// stalled. On a host with no private LAN address it fails with an honest message
/// that points at the connection code, rather than silently returning nothing
/// (REQ-S-010, REQ-N-015). The candidate list is transient and unauthenticated,
/// so it is returned directly and never enters the authoritative session view.
#[tauri::command]
async fn browse_consoles() -> Result<Vec<DiscoveredConsoleView>, CommandError> {
    let consoles = tauri::async_runtime::spawn_blocking(|| {
        let browser = Browser::new()?;
        browser.browse(BROWSE_WINDOW)
    })
    .await
    .map_err(|_| CommandError::new("discover", "the network search could not be started"))?
    .map_err(|error| CommandError::new("discover", browse_error_message(&error)))?;
    Ok(map_consoles(consoles))
}

/// Dials a console the customer picked from the discovered list (ADR-0007).
///
/// The `address` is the untrusted `ip:port` hint carried by a
/// [`DiscoveredConsoleView`]. It is validated through the same
/// [`parse_manual_entry`] a typed code uses and then dialed through the identical
/// [`begin_live_dial`] path, so picking a candidate grants it no authority and
/// takes no privileged shortcut — trust still comes only from the Session Approval
/// Code (REQ-S-006, REQ-S-001).
#[tauri::command]
fn connect_console(
    app: AppHandle,
    address: String,
    state: State<'_, AppState>,
) -> Result<ConsentView, CommandError> {
    ensure_can_connect(&state.guard())?;

    // The discovered address is validated by exactly the parser the typed code
    // uses; a malformed or address-less candidate leaves the window untouched.
    let target = parse_manual_entry(address.trim()).map_err(|e| CommandError::new("connect", e))?;

    Ok(begin_live_dial(app, &state, target.addr()))
}

/// Adds the customer role's state and commands to the application. Only this
/// role's commands are registered in a customer process (ST-M009-005), plus the
/// shared [`crate::start::get_role`]: no technician command, IPC endpoint or
/// listener exists in it.
pub(crate) fn attach(builder: tauri::Builder<tauri::Wry>) -> tauri::Builder<tauri::Wry> {
    let builder = builder.manage(default_app_state());

    // The shipped exe exposes only the real-transport command surface. The
    // demo-only commands are added only under `dev-simulate`, so a release build
    // has no invocable fabricated-consent path (ST-M002-009).
    #[cfg(feature = "dev-simulate")]
    let builder = builder.invoke_handler(tauri::generate_handler![
        crate::start::get_role,
        get_view,
        grant_consent,
        decline_consent,
        stop_session,
        simulate_technician_stop,
        reset_demo,
        start_live_session,
        browse_consoles,
        connect_console
    ]);
    #[cfg(not(feature = "dev-simulate"))]
    let builder = builder.invoke_handler(tauri::generate_handler![
        crate::start::get_role,
        get_view,
        grant_consent,
        decline_consent,
        stop_session,
        start_live_session,
        browse_consoles,
        connect_console
    ]);
    builder
}

/// Builds the honest wire identity this agent presents on a live dial, from live
/// system inspection and the real process token — mirroring the CLI agent's
/// `build_target_identity`. It is never `simulated` and never adopted from the
/// wire (REQ-S-011).
fn honest_target_identity() -> TargetIdentity {
    let identity = current_identity();
    TargetIdentity {
        hostname: detect_hostname(),
        os_version: sysinfo::os_version(),
        arch: sysinfo::machine_arch(),
        elevated: identity.elevated,
        simulated: false,
        agent_version: env!("CARGO_PKG_VERSION").to_string(),
    }
}

/// The honest capability set this consent window advertises on a live dial.
///
/// Mirrors `cretlink_support_agent`'s `build_reboot_resume` capability logic,
/// minus two entries this build genuinely cannot serve:
///
/// - **reboot-resume** — this window dials via plain [`SessionJoiner::join`],
///   which installs a `NoRebootResume` armer, so nothing can be armed.
///   Advertising `reboot_resume.v1` would be a lie the runtime would then let a
///   technician act on (REQ-N-012, REQ-S-011).
/// - **desktop** — this crate has no `cretlink-exec` dependency to back
///   screenshots and computer actions, so it does not offer the capability.
///
/// `fs.v1` is always served by the session runtime; `term.v1` only where the
/// ConPTY backend exists (Windows). The advertised set must match what the
/// runtime actually serves, because the runtime *enforces* the capability
/// gates — the interactive terminal is refused unless `term.v1` is advertised,
/// so the technician console's Terminal panel is reachable against this GUI
/// customer only when we advertise it here (REQ-N-012, REQ-F-023).
fn honest_capabilities() -> Vec<String> {
    #[cfg(windows)]
    {
        use cretlink_protocol::CAP_TERM_V1;
        vec![CAP_FS_V1.to_string(), CAP_TERM_V1.to_string()]
    }
    #[cfg(not(windows))]
    {
        vec![CAP_FS_V1.to_string()]
    }
}

/// Detects this machine's host name without a runtime dependency, falling back
/// to a neutral label when detection fails (never an invented value).
fn detect_hostname() -> String {
    std::env::var("COMPUTERNAME")
        .ok()
        .or_else(|| std::env::var("HOSTNAME").ok())
        .map(|name| name.trim().to_string())
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "this-computer".to_string())
}

/// Derives a short, stable, human-comparable approval code from a session id.
/// This is a display comparison value, not a secret and not key material.
///
/// Developer-only: the demo derives its code locally, while a live session takes
/// the real Session Approval Code from the wire via
/// [`set_approval_code`](ConsentSession::set_approval_code). Compiled out of the
/// shipped exe (ST-M002-009).
#[cfg(feature = "dev-simulate")]
fn approval_code(session: SessionId) -> String {
    let bytes = session.0.as_bytes();
    format!(
        "{:02}-{:02}-{:02}",
        bytes[0] % 100,
        bytes[1] % 100,
        bytes[2] % 100
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(feature = "dev-simulate")]
    #[test]
    fn seeded_state_is_the_awaiting_consent_demo() {
        let session = ConsentSession::seeded();
        assert!(matches!(session.agent.phase(), AgentPhase::AwaitingConsent));
        assert!(!session.agent.control_granted());
        let view = session.view();
        assert!(!view.status.control_active);
        assert!(view.simulated);
        // The demo can always offer to go live.
        assert!(view.can_connect);
        assert_eq!(
            view.approval_code.as_deref(),
            Some(approval_code(session.demo_session).as_str())
        );
        // The disclosure is the canonical wire text, verbatim.
        assert_eq!(view.disclosure, DisclosureSummary::DEFAULT_TEXT);
    }

    #[test]
    fn idle_is_the_shipped_default_honest_and_ready_to_connect() {
        // The shipped build boots resting and honest: real identity, never the
        // demo, no code, no control — and able to dial a technician
        // (ST-M002-009, AC #4). Control appears only when real wire consent
        // echoes back through the pump.
        let session = ConsentSession::idle();
        assert!(!session.simulated);
        assert!(!session.agent.control_granted());
        let view = session.view();
        assert!(!view.simulated);
        assert!(!view.status.control_active);
        assert!(view.approval_code.is_none());
        assert!(view.can_connect);
        // The disclosure is always the canonical wire text, verbatim.
        assert_eq!(view.disclosure, DisclosureSummary::DEFAULT_TEXT);
    }

    #[cfg(feature = "dev-simulate")]
    #[test]
    fn approval_code_is_stable_and_formatted() {
        let session = SessionId::new();
        assert_eq!(approval_code(session), approval_code(session));
        let code = approval_code(session);
        assert_eq!(code.len(), 8); // NN-NN-NN
        assert!(code.chars().all(|c| c.is_ascii_digit() || c == '-'));
    }

    #[cfg(feature = "dev-simulate")]
    #[test]
    fn granting_consent_is_the_only_path_to_control_and_hides_the_code() {
        let mut session = ConsentSession::seeded();
        let bound = session.demo_session;
        assert!(session.agent.grant_consent(bound).is_ok());
        let view = session.view();
        assert!(view.status.control_active);
        // Once under control the pre-decision approval code is no longer shown.
        assert!(view.approval_code.is_none());
    }

    #[cfg(feature = "dev-simulate")]
    #[test]
    fn declining_leaves_no_control_and_is_terminal() {
        let mut session = ConsentSession::seeded();
        session.agent.stop(); // decline path
        assert!(!session.agent.control_granted());
        // A later grant cannot re-open control from a terminal phase.
        assert!(session.agent.grant_consent(session.demo_session).is_err());
        let view = session.view();
        assert!(!view.status.control_active);
        assert!(view.approval_code.is_none());
    }

    #[test]
    fn live_dialing_starts_announced_honest_and_without_a_code() {
        let (tx, _rx) = session_driver::intent_channel();
        let session = ConsentSession::live_dialing(tx);
        assert!(matches!(session.agent.phase(), AgentPhase::Announced));
        assert!(!session.agent.control_granted());
        let view = session.view();
        // A live session is never the demo, shows no pre-wire code, and while it
        // is in flight cannot start a second dial.
        assert!(!view.simulated);
        assert!(view.approval_code.is_none());
        assert!(!view.can_connect);
    }

    #[test]
    fn honest_identity_is_never_simulated() {
        let identity = honest_target_identity();
        assert!(!identity.simulated);
        assert!(!identity.hostname.is_empty());
        assert_eq!(identity.agent_version, env!("CARGO_PKG_VERSION"));
    }

    #[test]
    fn honest_capabilities_advertise_only_what_this_build_serves() {
        let caps = honest_capabilities();
        // `fs.v1` is always served by the session runtime.
        assert!(caps.iter().any(|c| c == CAP_FS_V1));
        // The interactive terminal is gated on `term.v1`; it is advertised
        // exactly where the ConPTY backend exists, so the technician console's
        // Terminal panel is reachable against this GUI customer on Windows.
        #[cfg(windows)]
        assert!(caps.iter().any(|c| c == cretlink_protocol::CAP_TERM_V1));
        #[cfg(not(windows))]
        assert_eq!(caps.len(), 1);
        // We never advertise what this window cannot honestly back: no
        // reboot-resume (plain `join` installs `NoRebootResume`) and no desktop
        // (no `cretlink-exec` dependency here). Guard against a
        // future edit silently over-advertising (REQ-N-012, REQ-S-011).
        assert!(!caps.iter().any(|c| c.contains("reboot")));
        assert!(!caps.iter().any(|c| c.contains("desktop")));
    }

    #[test]
    fn map_consoles_caps_and_maps() {
        use std::net::{IpAddr, Ipv4Addr};

        use cretlink_protocol::ProtocolVersion;

        // A hostile LAN can advertise more candidates than the display cap.
        let raw: Vec<DiscoveredConsole> = (0..(MAX_CONSOLES as u16 + 5))
            .map(|i| DiscoveredConsole {
                instance_name: format!("console-{i:02}"),
                display_name: Some(format!("Console {i}")),
                addresses: vec![IpAddr::V4(Ipv4Addr::new(10, 0, 0, (i + 1) as u8))],
                port: 7000 + i,
                protocol_version: Some(ProtocolVersion::new(1, 0)),
            })
            .collect();

        let views = map_consoles(raw);
        // Capped for display (ADR-0007), never silently unbounded.
        assert_eq!(views.len(), MAX_CONSOLES);
        // The surviving candidates are fully mapped and dialable.
        let first = &views[0];
        assert_eq!(first.instance_name, "console-00");
        assert_eq!(first.address, "10.0.0.1:7000");
        assert_eq!(first.protocol_version.as_deref(), Some("1.0"));
        assert!(first.dialable);
    }

    #[test]
    fn console_without_address_is_shown_but_not_dialable() {
        // A candidate that advertised no address must still appear (the customer
        // sees it), but nothing is fabricated to make it dialable.
        let view = map_console(DiscoveredConsole {
            instance_name: "no-addr".to_string(),
            display_name: None,
            addresses: Vec::new(),
            port: 7000,
            protocol_version: None,
        });
        assert_eq!(view.instance_name, "no-addr");
        assert!(view.address.is_empty());
        assert!(!view.dialable);
    }

    #[test]
    fn browse_error_no_private_interface_offers_the_code() {
        // No LAN address is a fall-back signal, not a dead end: the message must
        // point the customer at the always-available connection code.
        let message = browse_error_message(&DiscoveryError::NoPrivateInterface);
        assert!(message.contains("connection code"));
    }

    #[test]
    fn connect_console_and_start_live_session_share_the_dial_path() {
        use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};

        use cretlink_discovery::ConnectionCode;

        // The single dial target both entry points must resolve to.
        let addr = SocketAddrV4::new(Ipv4Addr::new(192, 168, 1, 50), 7000);

        // `start_live_session` receives a typed connection code; `connect_console`
        // receives the discovered "ip:port" string. Both are validated by the
        // *same* `parse_manual_entry` that `begin_live_dial` then consumes, so a
        // discovered console is dialed by exactly the code path — no shortcut.
        let from_code = parse_manual_entry(&ConnectionCode::encode(addr))
            .expect("a freshly encoded code parses")
            .addr();
        let from_pick = parse_manual_entry(&SocketAddr::V4(addr).to_string())
            .expect("a discovered ip:port parses")
            .addr();

        assert_eq!(from_code, SocketAddr::V4(addr));
        assert_eq!(from_code, from_pick);
    }
}
