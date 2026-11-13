//! The customer role, Get help (ST-M002-005, ST-M002-009, ST-M009-005,
//! ST-M009-009).
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
//! reach a fabricated-consent path (AC #4). Both builds boot resting; the
//! `dev-simulate` build offers the demo from the technician list
//! ([`reset_demo`]).
//!
//! # A session through a relay (ST-M007-013)
//!
//! The code field takes a LAN connection code, a raw address and a relay code
//! and tells them apart ([`typed_target`]). A relay code joins the rendezvous
//! on the relay of this copy's profile ([`relay`]), over `wss` with the
//! relay's certificate checked, and gives the technician ten seconds to be
//! heard ([`JoiningDial`]). The window says that the connection runs through
//! the relay, by its host name ([`ConsentView::relay`]). The relay is
//! addressing only (ADR-0016): the approval code is compared and the customer
//! decides exactly as on one network, and a relay that cannot be used ends
//! the dial in plain words with a next step ([`ConsentView::failure_next`]),
//! never as a session that was lost for no known reason.
//!
//! # Before the session, and the two windows (ST-M009-009)
//!
//! An unelevated window first explains the UAC prompt ([`admin`]). During a
//! session the main window or the compact bar is always visible, and no path
//! closes the process without revoking the session ([`windows`]). **Stop** and
//! `Escape` act only while a session exists ([`session_exists`]); on the
//! resting window a stop is refused, never turned into a "stopped" session.

use std::net::SocketAddr;
use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use cretlink_discovery::{
    Browser, DiscoveredConsole, DiscoveryError, ManualEntry, RelayCode, parse_entry,
    parse_manual_entry,
};
use cretlink_protocol::{
    CAP_FS_V1, CAP_FS_V2, PROTOCOL_V1, RevokeReason, SessionId, TargetIdentity,
};
use cretlink_session::{
    ActivityEntry, CustomerActivity, DEFAULT_JOIN_LIMIT, DisclosureSummary, JoinHandle,
    JoiningDial, NoAudit, NoRebootResume, RelayFailure, RendezvousTicket, SessionError,
    SessionJoiner,
};
use cretlink_support_agent::{AgentPhase, AgentState, VisibleStatus, current_identity, sysinfo};
use serde::Serialize;
use tauri::{AppHandle, Emitter, State};
use tokio::sync::watch;

use crate::launch::Entry;
use crate::relay;

// Before the UAC prompt (ST-M009-009).
mod admin;
// The end report and its saved copy (ST-M009-012).
mod report;
mod session_driver;
// The startup sweep of stale staging trees (ST-M009-010).
mod startup;
pub(crate) use startup::StartupCleanupView;
// The compact bar and closing (ST-M009-009).
mod windows;
use session_driver::{Intent, ViewSink, is_terminal, lock, pump};
pub(crate) use windows::on_exit_requested;

/// The event every rendered [`ConsentView`] is emitted on, to both windows.
pub(crate) const AGENT_VIEW_EVENT: &str = "agent-view";

/// The event every changed [`ActivityEntry`] is emitted on (ST-M009-011).
pub(crate) const AGENT_ACTIVITY_EVENT: &str = "agent-activity";

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
    /// The report copies this process saved, the only files **Open** opens
    /// (ST-M009-012).
    saved_reports: report::SavedReports,
    /// This process's startup sweep, which a dial waits for (ST-M009-010).
    pub(crate) startup: startup::StartupSweep,
    /// What the profile file next to this program says about a relay
    /// (ST-M007-011), read once when the window starts.
    relay: Arc<relay::Lookup>,
}

impl AppState {
    /// Builds the initial window of both builds: a resting, honest consent
    /// window ready to dial a real technician. It knows no relay.
    #[must_use]
    pub fn idle() -> Self {
        Self::with_relay(relay::Lookup::None)
    }

    /// The initial window of a copy whose profile file says `relay`.
    fn with_relay(relay: relay::Lookup) -> Self {
        Self {
            inner: Arc::new(Mutex::new(ConsentSession::resting(&relay))),
            saved_reports: report::SavedReports::default(),
            startup: startup::StartupSweep::default(),
            relay: Arc::new(relay),
        }
    }

    fn guard(&self) -> MutexGuard<'_, ConsentSession> {
        lock(&self.inner)
    }
}

impl Default for AppState {
    /// Resting and honest, in both builds: the `dev-simulate` demo starts only
    /// when asked for ([`reset_demo`]), so a development build can also dial a
    /// real technician (ST-M009-009).
    fn default() -> Self {
        Self::idle()
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
        let _ = self.app.emit(AGENT_VIEW_EVENT, view);
        // Once the session is over, the compact bar goes and the main window
        // shows the end (REQ-S-002).
        windows::sync_windows(&self.app, view.session_active);
    }

    fn activity(&self, entry: &ActivityEntry) {
        // The activity feed rides a separate `"agent-activity"` channel (the
        // frontend's `ActivityEvent`, ST-M009-011) so the higher-frequency
        // action stream never forces a whole `ConsentView` re-render. Like
        // `emit`, a dropped frontend is not a backend error — the feed is
        // non-authoritative and the lifecycle continues underneath.
        let _ = self.app.emit(AGENT_ACTIVITY_EVENT, entry);
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
    /// This session's activity feed (ST-M009-011), folded from the agent's
    /// events and read by [`get_activity`]. It lives as long as this session
    /// state: a new dial starts a new one.
    pub(crate) activity: CustomerActivity,
    /// The technician's display name from the pairing handshake: what the
    /// technician's computer says, never a verified identity (ST-M009-011).
    technician_name: Option<String>,
    /// What this session's end report is built from, and the report once the
    /// session has ended (ST-M009-012). In memory only.
    pub(crate) report: report::ReportState,
    /// When full control was granted, in Unix milliseconds (ST-M009-009).
    control_since: Option<i64>,
    /// When the screen was last observed and last controlled, from the
    /// agent's `DesktopActivity` signals (ST-M009-009, REQ-F-020).
    screen: ScreenUseView,
    /// Becomes `true` once a live session's task has finished, its cleanup
    /// report included: closing waits for it (ST-M009-009).
    pub(crate) settled: Option<watch::Receiver<bool>>,
    /// Files from an earlier session the startup sweep could not remove
    /// (ST-M009-010). It belongs to the process, not to one session, so it is
    /// carried into every new session state ([`Self::replace_with`]).
    pub(crate) startup_cleanup: Option<StartupCleanupView>,
    /// Which dial this state belongs to: every live dial gets a new one, the
    /// resting window and the demo `0`. A dial's pump, teardown and failure
    /// path write the shared state only while it still carries their
    /// generation, so a finished dial never writes into the next session
    /// ([`session_driver::lock_current`]).
    pub(crate) generation: u64,
    /// The relay this session runs through; `None` for a session on one
    /// network (ST-M007-013). It stays after the session has ended, so that
    /// the end can say which way the session went, and goes with the next
    /// dial.
    relay: Option<RelayUseView>,
    /// What the customer can do about the failure this dial ended with, when
    /// the failure has a next step of its own (ST-M007-013, REQ-N-015).
    failure_next: Option<String>,
    /// The relay this copy is set up for, by its host name: what the profile
    /// file next to the program says (ST-M007-011). The window names it
    /// beside the code field, so that the customer sees through which relay
    /// a relay code would connect before typing one. It belongs to the
    /// process, not to one session, so it is carried into every new session
    /// state ([`Self::replace_with`]).
    copy_relay: Option<RelayUseView>,
}

/// The generation of the next live dial (see [`ConsentSession::generation`]).
static NEXT_GENERATION: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

fn next_generation() -> u64 {
    NEXT_GENERATION.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

impl ConsentSession {
    /// A resting, honest consent window for the shipped build: this machine's
    /// real identity, no simulated demo, no live channel, and no code. It waits
    /// for the customer to enter the technician's connection code and dial out
    /// via [`start_live_session`]; control can then appear only when the real
    /// wire consent echoes back (REQ-S-001). Never `simulated`.
    fn idle() -> Self {
        Self {
            agent: AgentState::new(current_identity()),
            // Unused on the resting/live path; the live session id comes from the
            // wire. Present only to satisfy the shared shape.
            demo_session: SessionId::new(),
            approval_code: None,
            simulated: false,
            live: None,
            activity: CustomerActivity::new(),
            technician_name: None,
            report: report::ReportState::default(),
            control_since: None,
            screen: ScreenUseView::default(),
            settled: None,
            startup_cleanup: None,
            generation: 0,
            relay: None,
            failure_next: None,
            copy_relay: None,
        }
    }

    /// The resting window of a copy whose profile file says `relay`: with a
    /// profile that can be used, the window names the relay this copy is set
    /// up for (ST-M007-011). Nothing is dialled.
    pub(crate) fn resting(relay: &relay::Lookup) -> Self {
        let mut session = Self::idle();
        session.copy_relay = relay.relay().map(RelayUseView::of);
        session
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
            activity: CustomerActivity::new(),
            technician_name: None,
            report: report::ReportState::default(),
            control_since: None,
            screen: ScreenUseView::default(),
            settled: None,
            startup_cleanup: None,
            generation: 0,
            relay: None,
            failure_next: None,
            copy_relay: None,
        }
    }

    /// A live, dialing consent session: an honest agent announced locally, with
    /// its intent channel to the pump. Pairing, the approval code, and consent
    /// then arrive from the wire — no control without the customer's grant
    /// echoing back (REQ-S-001).
    pub(crate) fn live_dialing(
        intents: session_driver::IntentSender,
        settled: Option<watch::Receiver<bool>>,
    ) -> Self {
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
            activity: CustomerActivity::new(),
            technician_name: None,
            report: report::ReportState::default(),
            control_since: None,
            screen: ScreenUseView::default(),
            settled,
            startup_cleanup: None,
            generation: next_generation(),
            relay: None,
            failure_next: None,
            copy_relay: None,
        }
    }

    /// The relay this session runs through, if it runs through one, as a
    /// sentence names it: its name, with where this copy connects when that
    /// is another host ([`relay::named`]).
    pub(crate) fn relay_named(&self) -> Option<String> {
        self.relay
            .as_ref()
            .map(|relay| relay::named(&relay.host, &relay.address))
    }

    /// Says that this session runs through `relay` (ST-M007-013).
    fn through_relay(mut self, relay: &relay::Relay) -> Self {
        self.relay = Some(RelayUseView::of(relay));
        self
    }

    /// Ends this dial as failed before a session existed: what happened, and
    /// what the customer can do next when the failure has a step of its own.
    fn fail_dial(&mut self, failure: DialFailure) {
        self.agent.fail(failure.stage, failure.message);
        self.failure_next = failure.next;
    }

    /// Replaces this state with `next`, a new session's, keeping what belongs
    /// to the process: the startup sweep's notice, and the relay this copy
    /// is set up for.
    pub(crate) fn replace_with(&mut self, mut next: Self) {
        next.startup_cleanup = self.startup_cleanup.take();
        next.copy_relay = self.copy_relay.take();
        *self = next;
    }

    /// Replaces the candidate Session Approval Code (the live wire code, or
    /// `None`). Whether it is actually shown is gated by phase in
    /// [`visible_approval_code`](Self::visible_approval_code).
    pub(crate) fn set_approval_code(&mut self, code: Option<String>) {
        self.approval_code = code;
    }

    /// Notes what the window shows beside the agent model: when full control
    /// began, and when the screen was last observed or controlled.
    pub(crate) fn observe(&mut self, event: &cretlink_session::SessionEvent, now: i64) {
        use cretlink_protocol::DesktopAccess;
        use cretlink_session::SessionEvent;
        match event {
            SessionEvent::ConsentGranted { .. } => {
                self.control_since.get_or_insert(now);
            }
            SessionEvent::DesktopActivity { access } => match access {
                DesktopAccess::Observe => self.screen.observed_at_ms = Some(now),
                DesktopAccess::Control => self.screen.controlled_at_ms = Some(now),
            },
            _ => {}
        }
    }

    /// Records the technician's display name the console sent in the pairing
    /// handshake (`None` when it sent none).
    pub(crate) fn set_technician_name(&mut self, name: Option<String>) {
        self.technician_name = name;
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
            hostname: detect_hostname(),
            session_active: session_exists(self),
            control_since_ms: self.control_since,
            screen: self.screen,
            approval_code: self.visible_approval_code(),
            disclosure: DisclosureSummary::DEFAULT_TEXT.to_string(),
            simulated: self.simulated,
            can_connect: self.can_connect(),
            technician: self
                .technician_name
                .clone()
                .map(|display_name| TechnicianNameView {
                    display_name,
                    provenance: TECHNICIAN_NAME_PROVENANCE,
                }),
            startup_cleanup: self.startup_cleanup.clone(),
            relay: self.relay.clone(),
            failure_next: self.failure_next.clone(),
            relay_of_copy: self.copy_relay.clone(),
        }
    }
}

/// Whether a session exists: a live dial that has not ended, or the running
/// `dev-simulate` demo. **Stop**, `Escape`, the compact bar and the close
/// question exist only then — never on the resting window (ST-M009-009).
pub(crate) fn session_exists(session: &ConsentSession) -> bool {
    if is_terminal(session) {
        return false;
    }
    session.live.is_some()
        || (session.simulated && !matches!(session.agent.phase(), AgentPhase::Idle))
}

/// When the screen was last observed and last controlled, in Unix
/// milliseconds, from the agent's `DesktopActivity` signals (ST-M009-009,
/// REQ-F-020). The signals carry the class of access only, never what was seen
/// or typed.
#[derive(Debug, Clone, Copy, Default, Serialize, PartialEq, Eq)]
pub struct ScreenUseView {
    /// The last screen observation (a screenshot).
    pub observed_at_ms: Option<i64>,
    /// The last time the mouse or keyboard was driven.
    pub controlled_at_ms: Option<i64>,
}

/// How the customer surface labels the technician's display name: as what the
/// technician's computer says, never as a verified identity (ST-M009-011).
pub const TECHNICIAN_NAME_PROVENANCE: &str = "as the technician's computer reports it";

/// The technician's display name in the [`ConsentView`] (ST-M009-011).
///
/// **Unverified.** It is the label the technician's computer chose (its
/// account's full name, else user name) and sent in the pairing handshake;
/// trust in the peer still comes only from the Session Approval Code
/// (REQ-S-006). The frontend writes it with `textContent` beside
/// [`provenance`](Self::provenance).
#[derive(Debug, Clone, Serialize)]
pub struct TechnicianNameView {
    /// The name, one bounded line.
    pub display_name: String,
    /// The label to show with it ([`TECHNICIAN_NAME_PROVENANCE`]).
    pub provenance: &'static str,
}

/// The relay of a session, as the customer's window shows it (ST-M007-013).
///
/// A label and nothing else: the relay passes the end-to-end encrypted
/// session on, cannot look inside and grants nothing (ADR-0016). It holds the
/// relay's name, where this copy connects to reach it and what its
/// certificate is checked against, never the admission key of this copy or
/// the relay code.
///
/// The name alone is not enough. The profile file next to this program can
/// give the relay a name that differs from the host it connects to, with a
/// certificate file of its own to vouch for that name; whoever wrote the
/// file chose both. The window therefore shows the address beside the name
/// when its host is another one, and says when the certificate is checked
/// against a file of the profile and not against this computer's
/// authorities.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct RelayUseView {
    /// The relay's host name: the name its certificate carries.
    pub host: String,
    /// Where this copy connects to reach the relay: `host:port`.
    pub address: String,
    /// What the relay's certificate is checked against.
    pub trust: cretlink_console::RelayTrust,
}

impl RelayUseView {
    /// The relay `relay` as the window names it.
    fn of(relay: &relay::Relay) -> Self {
        let route = relay.route();
        Self {
            host: route.host,
            address: route.address,
            trust: route.trust,
        }
    }
}

/// What the consent window renders (see [`ConsentSession::view`]).
#[derive(Debug, Clone, Serialize)]
pub struct ConsentView {
    /// The authoritative agent visible status (REQ-S-002).
    pub status: VisibleStatus,
    /// This computer's host name, as the technician will see it.
    pub hostname: String,
    /// Whether a session exists ([`session_exists`]): Stop, `Escape`, the
    /// compact bar and the close question exist only then (ST-M009-009).
    pub session_active: bool,
    /// When full control was granted, Unix milliseconds.
    pub control_since_ms: Option<i64>,
    /// When the screen was last observed or controlled.
    pub screen: ScreenUseView,
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
    /// The technician's display name from the pairing handshake, once
    /// connected, labelled as what the technician's computer says
    /// (ST-M009-011). `None` before pairing or when the console sent none.
    pub technician: Option<TechnicianNameView>,
    /// Files from an earlier session that the startup sweep could not remove,
    /// with the recovery command (ST-M009-010). `None` when nothing was kept
    /// or the sweep has not finished.
    pub startup_cleanup: Option<StartupCleanupView>,
    /// The relay this session runs through: its name, where this copy
    /// connects and what its certificate is checked against; `None` for a
    /// session on one network (ST-M007-013).
    pub relay: Option<RelayUseView>,
    /// What to do next about the failure this dial ended with, when the
    /// failure has a next step of its own: a relay that cannot be reached is
    /// put right in another way than a key that the relay refuses
    /// (ST-M007-013, REQ-N-015). `None` otherwise.
    pub failure_next: Option<String>,
    /// The relay this copy is set up for; `None` for a copy without a relay
    /// profile that can be used (ST-M007-011). The window shows it beside
    /// the code field: the customer sees the relay's name, where this copy
    /// would connect and what vouches for the name before they connect
    /// through it.
    pub relay_of_copy: Option<RelayUseView>,
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

/// Returns this session's activity feed, oldest first (ST-M009-011): every
/// item and gap the `"agent-activity"` event has carried, so a frontend that
/// (re)loads shows the same feed. It lasts as long as this session window's
/// session; a new dial starts a new feed.
#[tauri::command]
fn get_activity(state: State<'_, AppState>) -> Vec<ActivityEntry> {
    state.guard().activity.entries()
}

/// The customer's single informed full-control consent: the only path to
/// control (REQ-S-001, REQ-S-003). Valid only while awaiting consent.
///
/// In a live session this never grants locally — it sends the real wire consent
/// and full control appears only when the wire `ConsentGranted` echoes back and
/// the pump folds it. In the demo it binds the simulated session id directly.
#[tauri::command]
fn grant_consent(state: State<'_, AppState>) -> Result<ConsentView, CommandError> {
    grant_now(&mut state.guard())
}

/// [`grant_consent`], without Tauri.
fn grant_now(session: &mut ConsentSession) -> Result<ConsentView, CommandError> {
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
    session.control_since = Some(session_driver::now_unix_millis());
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

/// The emergency stop, always available while a session exists: revokes control
/// and rejects new actions (REQ-F-018, REQ-S-004). Fail-closed.
///
/// In a live session it sends the wire revoke and ends the local model at once,
/// so the surface goes safe immediately even if the wire acknowledgement is
/// delayed or lost. Without a session — the resting window, or a session that
/// already ended — there is nothing to stop: it is refused and changes nothing,
/// so a stray `Escape` never shows a "stopped" session that never existed
/// (ST-M009-009).
#[tauri::command]
fn stop_session(state: State<'_, AppState>) -> Result<ConsentView, CommandError> {
    let mut session = state.guard();
    stop_now(&mut session)?;
    Ok(session.view())
}

/// The stop of [`stop_session`], without Tauri.
fn stop_now(session: &mut ConsentSession) -> Result<(), CommandError> {
    if !session_exists(session) {
        return Err(CommandError::new("stop", "there is no session to stop"));
    }
    if let Some(tx) = session.live.as_ref() {
        let _ = tx.send(Intent::Stop(RevokeReason::CustomerRequested));
    }
    session.agent.stop();
    Ok(())
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

/// Starts (or re-arms) the demo with a fresh simulated session at the consent
/// step, so the disclosure flow can be exercised without a technician. A
/// reconnect always re-pairs and re-consents — control is never silently
/// restored (REQ-S-009). Refused while a session is in progress.
/// Developer-only: compiled out of the shipped exe (ST-M002-009).
#[cfg(feature = "dev-simulate")]
#[tauri::command]
fn reset_demo(state: State<'_, AppState>) -> Result<ConsentView, CommandError> {
    let mut session = state.guard();
    if session_exists(&session) {
        return Err(CommandError::new(
            "session",
            "stop the live session before restarting the demo",
        ));
    }
    session.replace_with(ConsentSession::seeded());
    Ok(session.view())
}

/// Fails closed if a live session is already in flight, keeping exactly one live
/// endpoint per window. A finished (terminal) session may be retried. Both dial
/// entry points — [`start_live_session`] and [`connect_console`] — gate on this
/// before touching any state, so a second dial can never race a live one.
fn ensure_can_connect(session: &ConsentSession, ready: bool) -> Result<(), CommandError> {
    if !ready {
        // Nothing happens before the customer has passed the permission step
        // (`#c-admin`, ST-M009-009).
        return Err(CommandError::new(
            "connect",
            "answer the Windows permission question first",
        ));
    }
    if session_exists(session) {
        return Err(CommandError::new(
            "connect",
            "a session is already in progress; stop it before connecting again",
        ));
    }
    Ok(())
}

/// What the customer typed or picked, resolved to where the technician is
/// reached. Holding one performs no I/O and grants nothing.
#[derive(Debug)]
enum Typed {
    /// A technician on this network: the address to dial.
    Network(SocketAddr),
    /// A technician who waits on the relay of this copy.
    Relay(Box<RelayJoin>),
}

/// A join through the relay of this copy (ST-M007-013): the relay of the
/// profile, the code the technician read out, and how long the technician is
/// given to be heard.
#[derive(Debug)]
struct RelayJoin {
    relay: relay::Relay,
    code: RelayCode,
    limit: Duration,
}

/// Why a dial ended before a session existed, for the window: the stage, what
/// happened, and what to do next when the failure has a step of its own.
#[derive(Debug, Clone, PartialEq, Eq)]
struct DialFailure {
    stage: String,
    message: String,
    next: Option<String>,
}

impl DialFailure {
    /// A relay that could not be used, in the words for a customer
    /// ([`relay::failure_words`]). The stage names the failure:
    /// `relay_unreachable`, `relay_untrusted`, `relay_key_refused`, and so on.
    fn of_relay(failure: &RelayFailure, relay: &relay::Relay) -> Self {
        let words = relay::failure_words(failure, &relay.view(), relay::Audience::Customer);
        Self {
            stage: format!("relay_{}", words.kind),
            message: words.message,
            next: Some(words.next),
        }
    }
}

/// Reaches the technician as `target` says and answers the handle of the
/// session that begins. Nothing here grants anything: pairing, the approval
/// code and the customer's consent follow on the handle (REQ-S-001).
async fn dial(target: Typed, identity: TargetIdentity) -> Result<JoinHandle, DialFailure> {
    match target {
        Typed::Network(addr) => {
            SessionJoiner::join(addr, identity, PROTOCOL_V1, honest_capabilities())
                .await
                .map_err(|error| DialFailure {
                    stage: "connect".to_owned(),
                    message: format!("could not reach the technician: {error}"),
                    next: None,
                })
        }
        Typed::Relay(join) => {
            let RelayJoin { relay, code, limit } = *join;
            let failed = |failure: &RelayFailure| DialFailure::of_relay(failure, &relay);
            // The TLS settings first: a profile without usable authorities
            // is said before anything is dialled.
            let connector = relay
                .connector()
                .await
                .map_err(|failure| failed(&failure))?;
            // Both copies derive the ticket from the code that was read out
            // and from the relay's host name (ST-M007-012). The code itself
            // never reaches the relay.
            let ticket = RendezvousTicket::derive(code.seed(), relay.host()).map_err(|_| {
                failed(&RelayFailure::Settings(
                    "the name of the relay cannot be used for a rendezvous".to_owned(),
                ))
            })?;
            // The joining side gives the technician a limit to be heard, so
            // that a code nobody waits on is said, and soon (ST-M007-012).
            // Reboot-resume stays off, as for a dial on one network.
            SessionJoiner::join_with_reboot_resume_over(
                JoiningDial::with_limit(relay.dial(connector, ticket), limit),
                identity,
                PROTOCOL_V1,
                honest_capabilities(),
                Arc::new(NoAudit),
                Arc::new(|| false),
                Arc::new(NoRebootResume),
            )
            .await
            .map_err(|error| match error {
                SessionError::Relay(failure) => failed(&failure),
                other => failed(&RelayFailure::Settings(other.to_string())),
            })
        }
    }
}

/// Enters the "reaching out" state of a new dial on `shared`, and answers the
/// future that dials `target` and drives the session to its end.
///
/// This is the **single** dial path. A hand-typed connection code, a console
/// picked from discovery and a relay code all arrive here, so each is dialed
/// with exactly the authority of a typed address: none until the Session
/// Approval Code (REQ-S-006). Control never appears locally: it shows only
/// when the wire's `ConsentGranted` echoes back through the pump (REQ-S-001).
fn prepare_dial(
    shared: &Arc<Mutex<ConsentSession>>,
    sink: Arc<dyn ViewSink>,
    target: Typed,
) -> impl Future<Output = ()> + Send + 'static + use<> {
    // The honest, effective-identity-derived identity this agent presents. The
    // wire never overrides it (REQ-S-011, REQ-F-016).
    let identity = honest_target_identity();

    let (tx, rx) = session_driver::intent_channel();
    let (settled_tx, settled_rx) = watch::channel(false);
    let generation = {
        let mut session = lock(shared);
        let mut next = ConsentSession::live_dialing(tx, Some(settled_rx));
        if let Typed::Relay(join) = &target {
            // Said from the first moment: this connection goes through the
            // relay, by its name and where this copy connects.
            next = next.through_relay(&join.relay);
        }
        session.replace_with(next);
        // Show the "reaching out" state right away; the pump takes over once
        // the dial resolves.
        sink.emit(&session.view());
        session.generation
    };

    let shared = Arc::clone(shared);
    async move {
        // A dial failure is an honest terminal state, never a silent no-op.
        match dial(target, identity).await {
            // Fold the live event stream (and route button intents) until it
            // ends, the session task's teardown included.
            Ok(handle) => pump(handle, shared, generation, sink, rx).await,
            Err(failure) => {
                // Only this dial's state: a stopped dial may already have
                // been replaced by the next one.
                if let Some(mut session) = session_driver::lock_current(&shared, generation) {
                    // A dial the customer cancelled meanwhile stays stopped.
                    if !is_terminal(&session) {
                        session.fail_dial(failure);
                    }
                    sink.emit(&session.view());
                }
            }
        }
        let _ = settled_tx.send(true);
    }
}

/// Begins a live dial of `target` from the window: shows the "reaching out"
/// state, and spawns the task that dials and then folds the real wire
/// lifecycle ([`prepare_dial`]).
fn begin_live_dial(app: AppHandle, state: &State<'_, AppState>, target: Typed) -> ConsentView {
    let sink: Arc<dyn ViewSink> = Arc::new(TauriSink { app: app.clone() });
    let session = prepare_dial(&state.inner, sink, target);
    // Never connect before the startup sweep has finished (ST-M009-010); it is
    // normally long done, having started with the window.
    state.startup.start(&app, state);
    let startup = state.startup.clone();
    tauri::async_runtime::spawn(async move {
        startup.finished().await;
        session.await;
    });

    state.guard().view()
}

/// Dials a real listening technician console from a typed connection code and
/// drives the session live (ST-M002-009). A relay code reaches the technician
/// through the relay of this copy instead (ST-M007-013).
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
    admin: State<'_, admin::AdminState>,
) -> Result<ConsentView, CommandError> {
    ensure_can_connect(&state.guard(), admin.ready())?;

    // Parse the technician's connection code into a dialable address *before*
    // mutating any state, so a typo leaves the window untouched. This grants no
    // authority; trust is established later by the Session Approval Code.
    let target = typed_target(&code, &state.relay)?;

    Ok(begin_live_dial(app, &state, target))
}

/// What a relay code gets in a copy that is not set up for a relay
/// (ST-M007-012). The code is well formed, so calling it wrong would send the
/// customer back to type it again; the copy is what does not fit.
const RELAY_CODE_WITHOUT_RELAY: &str = "This is a relay code, for a technician who is not on \
    your network. This copy of CretLink is not set up for a relay, so it connects only on the \
    same network.";

/// What the profile file next to the program `exe` says about a relay
/// (ST-M007-011). It is the one place the customer role reads: no settings of
/// an installed copy, and nothing is written.
fn relay_next_to(exe: Option<&Path>) -> relay::Lookup {
    exe.map_or(relay::Lookup::None, relay::next_to_program)
}

/// What a relay code gets in a copy whose relay profile cannot be used
/// (ST-M007-011): the file and what is wrong with it, in plain words, before
/// anything is dialled (REQ-N-015). The customer passes it on.
fn unusable_profile_message(error: &cretlink_cli::relay_profile::ProfileError) -> String {
    let problem = relay::ProblemView::of(error);
    format!(
        "This is a relay code, but the relay settings of this copy of CretLink cannot be used. \
         {} The file is {}.",
        problem.message, problem.file
    )
}

/// Resolves what the customer typed into the code field to where the
/// technician is reached (REQ-F-004, ST-M007-012, ST-M007-013): a LAN
/// connection code or a raw address is an address on this network; a relay
/// code is a rendezvous on the relay of this copy.
///
/// A relay code is told apart from a mistyped code. In a copy without a relay
/// profile it is answered in plain words with the stage `relay`, and in a copy
/// whose profile cannot be used with `relay_profile`, so the window can say
/// what to do next. Nothing is dialled here.
fn typed_target(code: &str, relay: &relay::Lookup) -> Result<Typed, CommandError> {
    match parse_entry(code.trim()).map_err(|e| CommandError::new("connect", e))? {
        ManualEntry::Lan(target) => Ok(Typed::Network(target.addr())),
        ManualEntry::Relay(code) => match relay {
            relay::Lookup::None => Err(CommandError::new("relay", RELAY_CODE_WITHOUT_RELAY)),
            relay::Lookup::Unusable(error) => Err(CommandError::new(
                "relay_profile",
                unusable_profile_message(error),
            )),
            relay::Lookup::Relay(relay) => Ok(Typed::Relay(Box::new(RelayJoin {
                relay: (**relay).clone(),
                code,
                limit: DEFAULT_JOIN_LIMIT,
            }))),
        },
    }
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
    admin: State<'_, admin::AdminState>,
) -> Result<ConsentView, CommandError> {
    ensure_can_connect(&state.guard(), admin.ready())?;

    // The discovered address is validated by exactly the parser the typed code
    // uses; a malformed or address-less candidate leaves the window untouched.
    let target = parse_manual_entry(address.trim()).map_err(|e| CommandError::new("connect", e))?;

    Ok(begin_live_dial(app, &state, Typed::Network(target.addr())))
}

/// Adds the customer role's state and commands to the application. Only this
/// role's commands are registered in a customer process (ST-M009-005), plus the
/// shared [`crate::start::get_role`]: no technician command, IPC endpoint or
/// listener exists in it. Both customer windows — the main window and the
/// compact bar — use this one command set.
pub(crate) fn attach(
    builder: tauri::Builder<tauri::Wry>,
    entry: Entry,
) -> tauri::Builder<tauri::Wry> {
    let elevated = current_identity().elevated;
    // The relay of this copy: the profile file next to this program, read
    // once and never written (ST-M007-011).
    let relay = relay_next_to(std::env::current_exe().ok().as_deref());
    let builder = builder
        .manage(AppState::with_relay(relay))
        .manage(admin::AdminState::new(entry, elevated))
        .on_window_event(windows::on_window_event);

    // The shipped exe exposes only the real-transport command surface. The
    // demo-only commands are added only under `dev-simulate`, so a release build
    // has no invocable fabricated-consent path (ST-M002-009).
    #[cfg(feature = "dev-simulate")]
    let builder = builder.invoke_handler(tauri::generate_handler![
        crate::start::get_role,
        get_view,
        get_activity,
        grant_consent,
        decline_consent,
        stop_session,
        simulate_technician_stop,
        reset_demo,
        start_live_session,
        browse_consoles,
        connect_console,
        report::get_end_report,
        report::save_report,
        report::open_saved_report,
        admin::get_launch,
        admin::relaunch_elevated,
        admin::continue_without_admin,
        admin::go_back,
        windows::make_small,
        windows::open_main,
        windows::drag_window,
        windows::stop_and_close
    ]);
    #[cfg(not(feature = "dev-simulate"))]
    let builder = builder.invoke_handler(tauri::generate_handler![
        crate::start::get_role,
        get_view,
        get_activity,
        grant_consent,
        decline_consent,
        stop_session,
        start_live_session,
        browse_consoles,
        connect_console,
        report::get_end_report,
        report::save_report,
        report::open_saved_report,
        admin::get_launch,
        admin::relaunch_elevated,
        admin::continue_without_admin,
        admin::go_back,
        windows::make_small,
        windows::open_main,
        windows::drag_window,
        windows::stop_and_close
    ]);
    builder
}

/// Called once the customer window exists: a process already past the
/// permission step starts its startup sweep now (ST-M009-010). One still
/// explaining the UAC prompt starts it when the customer goes on without admin
/// rights ([`admin::continue_without_admin`]), or never, when it hands over to
/// the elevated child.
pub(crate) fn on_window_start(app: &AppHandle) {
    use tauri::Manager;
    let (Some(admin), Some(state)) = (
        app.try_state::<admin::AdminState>(),
        app.try_state::<AppState>(),
    ) else {
        return;
    };
    if admin.ready() {
        state.startup.start(app, &state);
    }
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
        // The customer's own account, shown in the technician's identity
        // line (ST-M009-008); omitted when it could not be read.
        account: identity.account(),
    }
}

/// The honest capability set this consent window advertises on a live dial.
///
/// Mirrors `cretlink_support_agent`'s `build_reboot_resume` capability logic,
/// minus two entries this build genuinely cannot serve:
///
/// - **reboot-resume** — this window dials via plain [`SessionJoiner::join`],
///   which installs a `NoRebootResume` armer, and through a relay with that
///   same armer ([`dial`]), so nothing can be armed. Advertising
///   `reboot_resume.v1` would be a lie the runtime would then let a
///   technician act on (REQ-N-012, REQ-S-011).
/// - **desktop** — this crate has no `cretlink-exec` dependency to back
///   screenshots and computer actions, so it does not offer the capability.
///
/// `fs.v1` and `fs.v2` are always served by the session runtime, which owns
/// the file store: `fs.v2` is the drive list and the removal of a folder with
/// its content that the technician's file explorer uses (ST-M010-001).
/// `term.v1` is served only where the ConPTY backend exists (Windows). The
/// advertised set must match what the runtime actually serves, because the
/// runtime *enforces* the capability gates — the interactive terminal is
/// refused unless `term.v1` is advertised, and the explorer operations are
/// answered `Unsupported` unless `fs.v2` is — so the technician console's
/// Terminal panel and file explorer are reachable against this GUI customer
/// only when we advertise them here (REQ-N-012, REQ-F-023).
fn honest_capabilities() -> Vec<String> {
    #[cfg(windows)]
    {
        use cretlink_protocol::CAP_TERM_V1;
        vec![
            CAP_FS_V1.to_string(),
            CAP_FS_V2.to_string(),
            CAP_TERM_V1.to_string(),
        ]
    }
    #[cfg(not(windows))]
    {
        vec![CAP_FS_V1.to_string(), CAP_FS_V2.to_string()]
    }
}

/// Detects this machine's host name without a runtime dependency, falling back
/// to a neutral label when detection fails (never an invented value).
pub(crate) fn detect_hostname() -> String {
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
pub(crate) mod testing;

#[cfg(test)]
mod tests {
    use super::*;

    /// The address a typed code or address names on this network.
    fn network(typed: Result<Typed, CommandError>) -> SocketAddr {
        match typed {
            Ok(Typed::Network(addr)) => addr,
            other => panic!("expected an address on this network, got {other:?}"),
        }
    }

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
        let session = ConsentSession::live_dialing(tx, None);
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
    fn stop_is_refused_on_the_resting_window_and_changes_nothing() {
        // ST-M009-009: Stop and Escape act only while a session exists. The
        // resting window has none, so a stop is an honest refusal, not a
        // "stopped" session that never existed.
        let mut session = ConsentSession::idle();
        assert!(!session_exists(&session));
        assert!(!session.view().session_active);
        let refused = stop_now(&mut session);
        assert!(refused.is_err());
        assert!(matches!(session.agent.phase(), AgentPhase::Idle));
        // The window can still dial afterwards.
        assert!(session.view().can_connect);
    }

    #[test]
    fn stop_acts_on_a_live_session_and_then_there_is_none() {
        let (tx, mut rx) = session_driver::intent_channel();
        let mut session = ConsentSession::live_dialing(tx, None);
        assert!(session_exists(&session));
        assert!(session.view().session_active);
        assert!(stop_now(&mut session).is_ok());
        // The wire revoke went to the pump, and the model is safe at once.
        assert!(matches!(
            rx.try_recv(),
            Ok(Intent::Stop(RevokeReason::CustomerRequested))
        ));
        assert!(matches!(session.agent.phase(), AgentPhase::Stopped { .. }));
        assert!(!session_exists(&session));
        // A second stop has nothing left to stop.
        assert!(stop_now(&mut session).is_err());
    }

    #[test]
    fn the_startup_notice_belongs_to_the_process_not_the_session() {
        let mut session = ConsentSession::idle();
        assert!(session.view().startup_cleanup.is_none());
        let kept = StartupCleanupView {
            failed: 3,
            command: "& 'C:\\x\\cretlink.exe' recover --remove | Out-Host".to_owned(),
            administrator: true,
        };
        session.startup_cleanup = Some(kept.clone());
        let (tx, _rx) = session_driver::intent_channel();
        session.replace_with(ConsentSession::live_dialing(tx, None));
        assert_eq!(session.view().startup_cleanup, Some(kept));
        let json = serde_json::to_value(session.view()).unwrap_or_default();
        assert_eq!(json["startup_cleanup"]["failed"], 3);
        assert_eq!(
            json["startup_cleanup"]["command"],
            "& 'C:\\x\\cretlink.exe' recover --remove | Out-Host"
        );
    }

    #[test]
    fn no_dial_before_the_permission_step() {
        let session = ConsentSession::idle();
        assert!(ensure_can_connect(&session, false).is_err());
        assert!(ensure_can_connect(&session, true).is_ok());
        let (tx, _rx) = session_driver::intent_channel();
        let live = ConsentSession::live_dialing(tx, None);
        assert!(ensure_can_connect(&live, true).is_err());
    }

    #[test]
    fn the_view_tracks_control_and_the_screen() {
        use cretlink_protocol::DesktopAccess;
        use cretlink_session::SessionEvent;

        let (tx, _rx) = session_driver::intent_channel();
        let mut session = ConsentSession::live_dialing(tx, None);
        let view = session.view();
        assert_eq!(view.control_since_ms, None);
        assert_eq!(view.screen, ScreenUseView::default());
        assert!(!view.hostname.is_empty());
        session.observe(
            &SessionEvent::DesktopActivity {
                access: DesktopAccess::Observe,
            },
            1_000,
        );
        session.observe(
            &SessionEvent::DesktopActivity {
                access: DesktopAccess::Control,
            },
            2_000,
        );
        let view = session.view();
        assert_eq!(view.screen.observed_at_ms, Some(1_000));
        assert_eq!(view.screen.controlled_at_ms, Some(2_000));
        let json = serde_json::to_value(&view).unwrap_or_default();
        assert_eq!(json["screen"]["controlled_at_ms"], 2_000);
        assert_eq!(json["session_active"], true);
        assert!(json.get("hostname").is_some());
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
        // `fs.v1` is always served by the session runtime, and so is `fs.v2`,
        // the explorer's drive list and folder removal (ST-M010-001).
        assert!(caps.iter().any(|c| c == CAP_FS_V1));
        assert!(caps.iter().any(|c| c == CAP_FS_V2));
        // The interactive terminal is gated on `term.v1`; it is advertised
        // exactly where the ConPTY backend exists, so the technician console's
        // Terminal panel is reachable against this GUI customer on Windows.
        #[cfg(windows)]
        assert!(caps.iter().any(|c| c == cretlink_protocol::CAP_TERM_V1));
        #[cfg(not(windows))]
        assert_eq!(caps.len(), 2);
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
        // receives the discovered "ip:port" string. Both end in the same parser
        // of the discovery crate and the same `begin_live_dial`, so a discovered
        // console is dialed by exactly the code path — no shortcut.
        let from_code = network(typed_target(
            &ConnectionCode::encode(addr),
            &relay::Lookup::None,
        ));
        let from_pick = parse_manual_entry(&SocketAddr::V4(addr).to_string())
            .expect("a discovered ip:port parses")
            .addr();

        assert_eq!(from_code, SocketAddr::V4(addr));
        assert_eq!(from_code, from_pick);
        // The field also takes a raw address, typed loosely.
        let typed = format!("  {} ", SocketAddr::V4(addr));
        assert_eq!(
            network(typed_target(&typed, &relay::Lookup::None)),
            SocketAddr::V4(addr)
        );
    }

    #[test]
    fn a_relay_code_in_a_copy_without_a_relay_is_answered_in_plain_words() {
        // A well-formed relay code (ST-M007-012), typed loosely.
        let none = relay::Lookup::None;
        let error = typed_target("53st a71d jkkv 0vgh", &none).expect_err("this copy has no relay");
        assert_eq!(error.stage, "relay");
        assert!(error.message.contains("relay code"), "{}", error.message);
        assert!(
            error.message.contains("not set up for a relay"),
            "{}",
            error.message
        );
        // The message is whole sentences without the gaps of a wrapped literal.
        assert!(!error.message.contains("  "), "{:?}", error.message);

        // A relay code with a typing error is a mistyped code, not a relay
        // matter: the customer checks it with the technician.
        let error = typed_target("53ST-A71D-JKKV-0VGJ", &none).expect_err("a typing error");
        assert_eq!(error.stage, "connect");
        assert!(error.message.contains("checksum"), "{}", error.message);
    }

    /// A key as the relay's operator gives a customer copy.
    const KEY: &str = "9a41c7b35f2c9d0e7a1b4c3d8e9f0a1b2c3d4e5f60718293a4b5c6d7e8f9a0b1";

    #[test]
    fn the_customer_role_reads_the_profile_next_to_its_program_and_writes_nothing() {
        use cretlink_cli::relay_profile::RelayProfile;
        use cretlink_cli::settings::{self, Settings};

        use crate::relay::testing::Home;

        // A technician's installed copy on the same computer keeps a relay
        // and a network in its settings.
        let home = Home::new("customer-reads");
        settings::write(
            &home.layout,
            &Settings {
                relay: Some(
                    RelayProfile::from_fields("relay.example.org:443", Some(KEY), None, None)
                        .expect("a profile"),
                ),
                network_adapter: Some("Wi-Fi".to_owned()),
            },
        )
        .expect("write the settings");
        let program = home.folder.path("usb/CretLink-Support.exe");
        home.folder.write("usb/CretLink-Support.exe", "program");
        let before = home.folder.snapshot();

        // No file next to the program: no relay, whatever those settings say,
        // and also when the installed program itself runs the customer role.
        assert!(matches!(relay_next_to(Some(&program)), relay::Lookup::None));
        assert!(matches!(
            relay_next_to(Some(&home.layout.exe)),
            relay::Lookup::None
        ));
        assert!(matches!(relay_next_to(None), relay::Lookup::None));
        assert_eq!(home.folder.snapshot(), before, "looking wrote something");

        // The file next to the program is the relay of this copy.
        home.folder.write(
            "usb/cretlink.toml",
            format!("[relay]\naddress = \"host-01.example.org:9970\"\nkey = \"{KEY}\"\n"),
        );
        let before = home.folder.snapshot();
        let found = relay_next_to(Some(&program));
        assert_eq!(
            found.relay().map(relay::Relay::host),
            Some("host-01.example.org")
        );
        // The window that starts with it, and every code typed into it, read
        // and never write: not the profile, no settings, nothing.
        let state = AppState::with_relay(found);
        for code in [
            "53st a71d jkkv 0vgh",
            "6RRR-2H8F-0000",
            "192.168.1.20:7000",
            "",
        ] {
            let _ = typed_target(code, &state.relay);
        }
        let _ = state.guard().view();
        assert_eq!(
            home.folder.snapshot(),
            before,
            "the customer role wrote something"
        );

        // A file that cannot be used stays as it is, too.
        home.folder.write(
            "usb/cretlink.toml",
            "[relay]\naddress = \"host-01.example.org\"\n",
        );
        let before = home.folder.snapshot();
        let broken = relay_next_to(Some(&program));
        assert!(matches!(broken, relay::Lookup::Unusable(_)));
        let _ = typed_target("53st a71d jkkv 0vgh", &broken);
        assert_eq!(home.folder.snapshot(), before);
    }

    #[test]
    fn a_relay_code_in_a_copy_whose_profile_cannot_be_used_says_what_is_wrong_with_it() {
        use crate::relay::testing::Folder;

        let folder = Folder::new("customer-unusable");
        let file = folder.write(
            "cretlink.toml",
            format!("[relay]\naddress = \"relay.example.org:443\"\nkey = {KEY}\n"),
        );
        let relay = relay_next_to(Some(&folder.path("CretLink-Support.exe")));
        let error = typed_target("53ST-A71D-JKKV-0VGH", &relay).expect_err("no usable profile");
        assert_eq!(error.stage, "relay_profile");
        assert_eq!(
            error.message,
            format!(
                "This is a relay code, but the relay settings of this copy of CretLink cannot \
                 be used. Line 3: the value of `key` must be text in double quotes. The file is \
                 {}.",
                file.display()
            )
        );
        // The key that stands in the file the wrong way is not repeated.
        assert!(!error.message.contains(KEY));
        // A code of the same network needs no relay: it is read as before.
        assert!(typed_target("192.168.1.20:7000", &relay).is_ok());
    }

    #[test]
    fn a_relay_code_in_a_copy_with_a_profile_is_a_join_on_the_relay_of_that_profile() {
        use crate::relay::testing::Folder;

        let folder = Folder::new("customer-profile");
        folder.write(
            "cretlink.toml",
            format!("[relay]\naddress = \"relay.example.org:443\"\nkey = \"{KEY}\"\n"),
        );
        let relay = relay_next_to(Some(&folder.path("CretLink-Support.exe")));
        // Typed loosely, as people type what is read to them.
        let typed = typed_target(" 53st a71d jkkv 0vgh ", &relay);
        let Ok(Typed::Relay(join)) = typed else {
            panic!("expected a join on the relay, got {typed:?}");
        };
        assert_eq!(join.relay.host(), "relay.example.org");
        assert_eq!(join.code.encode(), "53ST-A71D-JKKV-0VGH");
        // The technician is given ten seconds to be heard (ST-M007-012).
        assert_eq!(join.limit, Duration::from_secs(10));
        // What is printed of it holds neither the key nor the code.
        let printed = format!("{join:?}");
        assert!(!printed.contains(KEY), "{printed}");
        assert!(!printed.contains("53ST"), "{printed}");

        // The same field takes a code or an address of this network, and
        // then nothing about the relay is used: LAN first.
        assert_eq!(
            network(typed_target("192.168.1.20:7000", &relay)),
            "192.168.1.20:7000".parse::<SocketAddr>().unwrap()
        );
        // A connection code of this network is taken as what it is, to the
        // address it says, also in a copy that has a relay profile. It is
        // typed as people type what is read to them.
        let lan = std::net::SocketAddrV4::new(std::net::Ipv4Addr::new(192, 168, 1, 20), 7000);
        let lan_code = cretlink_discovery::ConnectionCode::encode(lan).to_string();
        for typed in [
            lan_code.clone(),
            lan_code.to_lowercase().replace('-', " "),
            format!("  {lan_code} "),
        ] {
            let found = typed_target(&typed, &relay);
            let Ok(Typed::Network(address)) = found else {
                panic!("a connection code of this network was not taken: {typed:?}: {found:?}");
            };
            assert_eq!(address, SocketAddr::V4(lan), "{typed:?}");
        }
        // A code with a typing error is a mistyped code, as in a copy without
        // a profile: it is not read as a relay code.
        let mistyped = typed_target("6RRR-2H8F-0000", &relay).expect_err("a typing error");
        assert_eq!(mistyped.stage, "connect");
    }

    #[test]
    fn a_relay_that_cannot_be_used_ends_the_dial_in_words_with_a_next_step() {
        use cretlink_cli::relay_profile::RelayProfile;

        let profile = RelayProfile::from_fields("relay.example.org:443", Some(KEY), None, None)
            .expect("a profile");
        let relay = relay::Relay::with_ca(
            relay::Source::File(std::path::PathBuf::from("cretlink.toml")),
            profile,
            None,
        )
        .expect("a relay");
        let (tx, _rx) = session_driver::intent_channel();
        let mut session = ConsentSession::live_dialing(tx, None).through_relay(&relay);
        // While it connects, the window says which relay it goes through.
        assert_eq!(
            session.view().relay,
            Some(RelayUseView {
                host: "relay.example.org".to_owned(),
                address: "relay.example.org:443".to_owned(),
                trust: cretlink_console::RelayTrust::SystemAuthorities,
            })
        );
        assert_eq!(session.view().failure_next, None);

        let refused = RelayFailure::Refused(cretlink_relay::RelayRefusal::CredentialRejected);
        session.fail_dial(DialFailure::of_relay(&refused, &relay));
        let view = session.view();
        let AgentPhase::Failed { stage, message } = session.agent.phase() else {
            panic!("the dial did not fail: {:?}", session.agent.phase());
        };
        assert_eq!(stage, "relay_key_refused");
        assert!(message.contains("relay.example.org"), "{message}");
        // The next step is one a customer can take, and it is the window's.
        let next = view.failure_next.as_deref().expect("a next step");
        assert!(next.contains("technician"), "{next}");
        // The relay stays named, and no session exists.
        assert_eq!(
            view.relay.map(|relay| relay.host),
            Some("relay.example.org".to_owned())
        );
        assert!(!view.session_active);
        assert!(!view.status.control_active);
        // Nothing of it holds the key.
        let shown = serde_json::to_string(&session.view()).unwrap();
        assert!(!shown.contains(KEY), "{shown}");

        // The next dial starts without the relay and without that next step.
        let (tx, _rx) = session_driver::intent_channel();
        session.replace_with(ConsentSession::live_dialing(tx, None));
        assert_eq!(session.view().relay, None);
        assert_eq!(session.view().failure_next, None);
    }

    #[test]
    fn a_dial_on_one_network_names_no_relay() {
        let (tx, _rx) = session_driver::intent_channel();
        let session = ConsentSession::live_dialing(tx, None);
        let json = serde_json::to_value(session.view()).unwrap();
        assert!(json["relay"].is_null());
        assert!(json["failure_next"].is_null());
        assert!(json["relay_of_copy"].is_null());
    }

    #[test]
    fn the_resting_window_names_the_relay_this_copy_is_set_up_for() {
        use crate::relay::testing::Folder;

        // Without a profile, and with one that cannot be used, the window
        // names no relay: nothing is offered that would not work.
        assert_eq!(
            ConsentSession::resting(&relay::Lookup::None)
                .view()
                .relay_of_copy,
            None
        );
        let folder = Folder::new("customer-resting");
        folder.write(
            "cretlink.toml",
            "[relay]\naddress = \"relay.example.org\"\n",
        );
        let unusable = relay_next_to(Some(&folder.path("CretLink-Support.exe")));
        assert!(matches!(unusable, relay::Lookup::Unusable(_)));
        assert_eq!(
            ConsentSession::resting(&unusable).view().relay_of_copy,
            None
        );

        // With a profile, the customer sees the relay's host name before
        // typing a code, and nothing is dialled for it.
        folder.write(
            "cretlink.toml",
            format!("[relay]\naddress = \"relay.example.org:443\"\nkey = \"{KEY}\"\n"),
        );
        let lookup = relay_next_to(Some(&folder.path("CretLink-Support.exe")));
        let mut session = ConsentSession::resting(&lookup);
        let view = session.view();
        assert_eq!(
            view.relay_of_copy,
            Some(RelayUseView {
                host: "relay.example.org".to_owned(),
                address: "relay.example.org:443".to_owned(),
                trust: cretlink_console::RelayTrust::SystemAuthorities,
            })
        );
        // It says what this copy could use, not that a session uses it.
        assert_eq!(view.relay, None);
        assert!(!view.session_active);
        assert!(matches!(session.agent.phase(), AgentPhase::Idle));
        assert!(!serde_json::to_string(&view).unwrap().contains(KEY));

        // It belongs to the process: a dial on one network keeps it, and
        // names no relay for that session.
        let (tx, _rx) = session_driver::intent_channel();
        session.replace_with(ConsentSession::live_dialing(tx, None));
        let dialing = session.view();
        assert_eq!(dialing.relay, None);
        assert_eq!(
            dialing.relay_of_copy.map(|relay| relay.host),
            Some("relay.example.org".to_owned())
        );
        // The state that was replaced gave it away: the window has one.
        let (tx, _rx) = session_driver::intent_channel();
        session.replace_with(ConsentSession::live_dialing(tx, None));
        assert!(session.view().relay_of_copy.is_some());
    }
}
