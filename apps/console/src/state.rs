//! The technician-side console session-UI state model (ST-M002-002).
//!
//! This is the single source of truth for what the console shows: the
//! technician host, the discovered targets, the one target selected for
//! control, and the lifecycle phase of that target's session. Both the Windows
//! and macOS console builds render this exact model, so the two platforms
//! cannot drift apart (ADR-0005), and the Tauri GUI shell
//! (`apps/app`, Give help) is a thin renderer over these types.
//!
//! # Safety invariants enforced here
//!
//! - **One target at a time (REQ-F-016).** [`ConsoleState::selected`] is a
//!   single optional [`TargetId`]. Switching to a different target while a
//!   session is active is refused ([`StateError::SessionActive`]); the operator
//!   must [`ConsoleState::stop`] first, which is the explicit console action
//!   REQ-F-016 requires.
//! - **No control before consent (REQ-F-006).** [`ControlLevel::FullControl`]
//!   is reachable only through [`ConsoleState::grant_full_control`], which is
//!   only valid from [`SessionPhase::AwaitingConsent`]. Before that,
//!   [`ConsoleState::can_issue_actions`] is `false`.
//! - **Target identity is distinct from the technician host (REQ-F-016).** The
//!   technician host is a separate typed field ([`TechnicianHost`]); a target's
//!   full [`TargetIdentity`] is populated only once the session reaches
//!   full control, so an unconnected target can never be dressed up as the
//!   local machine.
//! - **Fail closed (REQ-F-018, REQ-S-009).** [`ConsoleState::stop`] and
//!   [`ConsoleState::fail`] invalidate the session authority and reject new
//!   actions, and stop is idempotent (REQ-N-004). A selected target that goes
//!   stale during an active session forces the session closed.
//!
//! - **A relay changes where a session travels, never what it may do
//!   (ADR-0016).** [`ConsoleState::relay`] names the relay a session waits on
//!   or runs through, so that every screen can say so. No transition reads
//!   it: the approval code, the customer's consent and the stop are the same
//!   as on one network (REQ-F-022).
//!
//! The transports this model reflects are implemented elsewhere:
//! discovery (ST-M002-003), authenticated pairing and the Session Approval
//! Code (ST-M002-004), consent (ST-M002-005), and revocation/cleanup
//! (ST-M002-006). This module owns the state machine and its invariants, not
//! the wire behaviour.

use cretlink_protocol::{SessionId, TargetIdentity};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use uuid::Uuid;

/// A console-local identifier for a discovered target entry.
///
/// This is distinct from [`SessionId`]: a target has a `TargetId` as soon as it
/// is discovered, long before (and whether or not) a session is negotiated.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct TargetId(pub Uuid);

impl TargetId {
    /// Mints a fresh random target identifier.
    #[must_use]
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
}

impl Default for TargetId {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Display for TargetId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(&self.0, f)
    }
}

/// Identity of the technician's own computer — the machine the console runs
/// on. Kept as its own type so the UI can render it in a distinct region and
/// never confuse it with a remote [`TargetIdentity`] (REQ-F-016).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TechnicianHost {
    /// A plain-language label, for example "This computer (technician)".
    pub label: String,
    /// The technician host's hostname.
    pub hostname: String,
    /// The technician host's operating-system description.
    pub os: String,
    /// The technician's display name as the operating-system account reports
    /// it (full name, else user name; ST-M009-011), or `None` when neither is
    /// available. Sent to the customer side, which shows it as what the
    /// technician's computer says.
    #[serde(default)]
    pub display_name: Option<String>,
}

/// The relay a session waits on or runs through (ST-M007-013, ADR-0016).
///
/// A label for the screens and nothing else: the relay only passes the
/// end-to-end encrypted session on, and no transition of this model depends
/// on it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RelayRoute {
    /// The relay's host name: the name its certificate carries, as both
    /// people are shown it.
    pub host: String,
    /// Whether the relay has taken this console's waiting connection in.
    /// `false` while the relay is still being reached; `true` once it holds
    /// the rendezvous that the customer's relay code leads to.
    pub admitted: bool,
}

/// How a target came to be known to the console.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiscoverySource {
    /// Found through automatic same-LAN discovery (ST-M002-003).
    Discovered,
    /// Entered manually as an address (ST-M002-003 fallback).
    Manual,
    /// A non-privileged simulated target, used to demonstrate the UI before the
    /// real transports land (REQ-N-011). Always rendered as simulated.
    Simulated,
}

/// A target the console currently knows about.
///
/// Before a session reaches full control only the minimum pre-pairing metadata
/// is known (`display_name`, `address`); `identity` stays `None`. The full
/// [`TargetIdentity`] is populated by [`ConsoleState::grant_full_control`], so
/// hostname/OS/architecture/elevation appear only for a connected target.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiscoveredTarget {
    /// Console-local identifier.
    pub id: TargetId,
    /// Minimal human-readable label (for example an mDNS instance name).
    pub display_name: String,
    /// Where to reach the target (host:port, or a manual/simulated marker).
    pub address: String,
    /// How this target was learned.
    pub source: DiscoverySource,
    /// Whether the target's discovery entry has gone stale (expired or no
    /// longer reachable). A stale target cannot be selected or connected, and a
    /// selected target that goes stale during a live session is dropped.
    pub stale: bool,
    /// The full remote identity, known only once the session is under full
    /// control (REQ-F-016).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub identity: Option<TargetIdentity>,
}

impl DiscoveredTarget {
    /// Builds a freshly discovered (non-stale, unidentified) target.
    #[must_use]
    pub fn new(
        id: TargetId,
        display_name: impl Into<String>,
        address: impl Into<String>,
        source: DiscoverySource,
    ) -> Self {
        Self {
            id,
            display_name: display_name.into(),
            address: address.into(),
            source,
            stale: false,
            identity: None,
        }
    }

    /// Whether this target is (or would be) driven by the simulator rather than
    /// a real customer computer.
    #[must_use]
    pub fn is_simulated(&self) -> bool {
        matches!(self.source, DiscoverySource::Simulated)
            || self.identity.as_ref().is_some_and(|id| id.simulated)
    }
}

/// Why a session stopped, recorded on [`SessionPhase::Stopped`] so the UI can
/// explain the transition (REQ-N-004, REQ-N-015).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    /// The technician pressed stop on the console.
    TechnicianRequested,
    /// The customer stopped the session from the support agent.
    CustomerRequested,
    /// The selected target went stale (expired or unreachable) mid-session.
    TargetStale,
    /// The transport disconnected.
    Disconnected,
}

/// The lifecycle phase of the selected target's session.
///
/// The legal progression is `Idle → Listening → Connecting →
/// AwaitingApprovalCode → AwaitingConsent → FullControl → Stopping → Stopped`,
/// with `Failed` reachable from any non-terminal phase. The phases deliberately
/// distinguish plain connection from full control (REQ-N-013).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "phase", rename_all = "snake_case")]
pub enum SessionPhase {
    /// No target is selected.
    Idle,
    /// The console is advertising itself and listening for an incoming customer
    /// connection (ST-M002-012). No target is known yet — the customer appears
    /// on its own once it dials in and the encrypted channel is established. The
    /// console shows the connection `code` and its listening `address` so the
    /// customer can reach it, but neither confers any authority.
    ///
    /// When [`ConsoleState::relay`] is set, the console waits on a relay
    /// instead (ST-M007-013): it listens on no address of its own, `code` is
    /// the relay code that the technician reads aloud, and `address` is where
    /// the relay is reached.
    Listening {
        /// The short human connection code the customer can type as a fallback
        /// to reach this console, or the relay code of a session through a
        /// relay. It is a rendezvous value, not a secret, and carries no key
        /// material.
        code: String,
        /// A human-readable "host:port" the console is listening on, or where
        /// the relay is reached.
        address: String,
    },
    /// A target is selected but no session has been started.
    Discovered,
    /// A session handshake is in progress (no control).
    Connecting,
    /// Both endpoints display a Session Approval Code to compare aloud
    /// (REQ-F-005). Still no control.
    AwaitingApprovalCode {
        /// The short human comparison code shown on both endpoints. It is a
        /// comparison value, not a secret, and carries no key material.
        code: String,
    },
    /// The code matched; awaiting the customer's single full-control consent
    /// (REQ-F-006). Still no control.
    AwaitingConsent,
    /// Attended full control is active (REQ-F-006). Tools are callable.
    FullControl,
    /// A stop is in progress.
    Stopping,
    /// The session has ended; no new actions are accepted.
    Stopped {
        /// Why the session stopped.
        reason: StopReason,
    },
    /// The session failed. The stage and message are actionable and free of
    /// secrets (REQ-N-015).
    Failed {
        /// Which stage failed (for example "connect", "pairing", "consent").
        stage: String,
        /// A plain-language, secret-free description.
        message: String,
    },
}

impl SessionPhase {
    /// Whether this phase represents a live session — one that a target switch
    /// must stop first, and that a stale target must tear down.
    #[must_use]
    fn is_active(&self) -> bool {
        matches!(
            self,
            Self::Connecting
                | Self::AwaitingApprovalCode { .. }
                | Self::AwaitingConsent
                | Self::FullControl
                | Self::Stopping
        )
    }
}

/// The level of authority the console currently presents over the selected
/// target. Used to prove that no control exists before consent (REQ-F-006).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ControlLevel {
    /// No session, or a terminal (stopped/failed) session: nothing is callable.
    None,
    /// Connected/pairing/consenting: identity may be visible but no control.
    Connected,
    /// Full attended control is active.
    FullControl,
}

/// A rejected state transition. Every variant is safe to show to the operator:
/// it names the failed action and a next step, and carries no secrets
/// (REQ-N-015).
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum StateError {
    /// An action needed a selected target but none was selected.
    #[error("no target is selected; select a target first")]
    NoSelection,
    /// The referenced target is not known to the console.
    #[error("target {0} is not known to the console")]
    UnknownTarget(TargetId),
    /// The target is stale and cannot be selected or connected.
    #[error("target {0} is stale; wait for rediscovery or reconnect it")]
    StaleTarget(TargetId),
    /// A different target already has a live session; stop it first.
    #[error("a session is active on the selected target; stop it before switching targets")]
    SessionActive,
    /// The requested transition is not legal from the current phase.
    #[error("cannot {action} while the session is {phase}")]
    InvalidTransition {
        /// The action that was attempted.
        action: &'static str,
        /// The phase it was attempted from.
        phase: &'static str,
    },
}

/// A short static label for a phase, used in [`StateError::InvalidTransition`]
/// so errors read cleanly without leaking phase payloads.
fn phase_label(phase: &SessionPhase) -> &'static str {
    match phase {
        SessionPhase::Idle => "idle",
        SessionPhase::Listening { .. } => "listening",
        SessionPhase::Discovered => "discovered",
        SessionPhase::Connecting => "connecting",
        SessionPhase::AwaitingApprovalCode { .. } => "awaiting the approval code",
        SessionPhase::AwaitingConsent => "awaiting consent",
        SessionPhase::FullControl => "under full control",
        SessionPhase::Stopping => "stopping",
        SessionPhase::Stopped { .. } => "stopped",
        SessionPhase::Failed { .. } => "failed",
    }
}

/// The complete console session-UI state. Serialized whole to the frontend on
/// every change (ADR-0011), so the web layer never holds authoritative state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConsoleState {
    /// The technician's own computer (never a control target).
    pub technician: TechnicianHost,
    /// Every target the console currently knows about.
    pub targets: Vec<DiscoveredTarget>,
    /// The one target selected for control, if any (REQ-F-016).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub selected: Option<TargetId>,
    /// The negotiated session, present only while connected (REQ-F-016).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session: Option<SessionId>,
    /// The relay the session waits on or runs through; `None` for a session
    /// on one network (ST-M007-013). It stays after the session has ended, so
    /// that the end can say which way the session went, and goes when the
    /// console waits again.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub relay: Option<RelayRoute>,
    /// The lifecycle phase of the selected target's session.
    pub phase: SessionPhase,
}

impl ConsoleState {
    /// Creates an empty console bound to the given technician host, with no
    /// targets and nothing selected.
    #[must_use]
    pub fn new(technician: TechnicianHost) -> Self {
        Self {
            technician,
            targets: Vec::new(),
            selected: None,
            session: None,
            relay: None,
            phase: SessionPhase::Idle,
        }
    }

    /// The currently selected target, if any.
    #[must_use]
    pub fn selected_target(&self) -> Option<&DiscoveredTarget> {
        let id = self.selected?;
        self.targets.iter().find(|t| t.id == id)
    }

    /// The authority the console currently presents (REQ-F-006).
    #[must_use]
    pub fn control_level(&self) -> ControlLevel {
        match self.phase {
            SessionPhase::FullControl => ControlLevel::FullControl,
            SessionPhase::Connecting
            | SessionPhase::AwaitingApprovalCode { .. }
            | SessionPhase::AwaitingConsent
            | SessionPhase::Stopping => ControlLevel::Connected,
            SessionPhase::Idle
            | SessionPhase::Listening { .. }
            | SessionPhase::Discovered
            | SessionPhase::Stopped { .. }
            | SessionPhase::Failed { .. } => ControlLevel::None,
        }
    }

    /// Whether tool/command actions may be issued right now: only under active
    /// full control of a selected, non-stale target (REQ-F-006, REQ-F-018).
    #[must_use]
    pub fn can_issue_actions(&self) -> bool {
        matches!(self.phase, SessionPhase::FullControl)
            && self.selected_target().is_some_and(|t| !t.stale)
    }

    /// Inserts a target, or replaces an existing one with the same id
    /// (rediscovery). A rediscovered target is no longer stale.
    pub fn upsert_target(&mut self, target: DiscoveredTarget) {
        if let Some(existing) = self.targets.iter_mut().find(|t| t.id == target.id) {
            *existing = target;
        } else {
            self.targets.push(target);
        }
    }

    /// Marks a target stale or fresh. If the *selected* target goes stale while
    /// its session is live, the session is forced closed
    /// ([`StopReason::TargetStale`]) so control never rests on a target that is
    /// no longer there (REQ-F-018, REQ-S-009).
    pub fn set_stale(&mut self, id: TargetId, stale: bool) {
        let is_selected = self.selected == Some(id);
        if let Some(target) = self.targets.iter_mut().find(|t| t.id == id) {
            target.stale = stale;
        }
        if stale && is_selected && self.phase.is_active() {
            self.session = None;
            self.phase = SessionPhase::Stopped {
                reason: StopReason::TargetStale,
            };
        }
    }

    /// Begins advertising and listening for an incoming customer connection
    /// (ST-M002-012). This is the console's honest starting move: it does not
    /// scan or dial customers — it publishes its presence and waits. Valid only
    /// from a non-live phase (Idle, Listening, Stopped, or Failed) and clears any
    /// prior targets, selection, and session, so the window shows a clean
    /// "waiting for the customer" state with no stale discovery entries and,
    /// crucially, no selectable target list (REQ-F-004).
    ///
    /// # Errors
    ///
    /// [`StateError::InvalidTransition`] if a session is already live.
    pub fn listen(
        &mut self,
        code: impl Into<String>,
        address: impl Into<String>,
    ) -> Result<(), StateError> {
        self.expect_phase(
            "start listening",
            matches!(
                self.phase,
                SessionPhase::Idle
                    | SessionPhase::Listening { .. }
                    | SessionPhase::Stopped { .. }
                    | SessionPhase::Failed { .. }
            ),
        )?;
        self.targets.clear();
        self.selected = None;
        self.session = None;
        self.relay = None;
        self.phase = SessionPhase::Listening {
            code: code.into(),
            address: address.into(),
        };
        Ok(())
    }

    /// Begins waiting for the customer on a relay (ST-M007-013): the console
    /// reaches out to the relay `host` at `address` and keeps the rendezvous
    /// of `code`, the relay code the technician reads aloud, open there. Like
    /// [`Self::listen`] it clears what an earlier session left, shows no
    /// target, and confers no authority: the relay only lets the two
    /// connections find each other (ADR-0016 §2).
    ///
    /// # Errors
    ///
    /// [`StateError::InvalidTransition`] if a session is already live.
    pub fn listen_on_relay(
        &mut self,
        code: impl Into<String>,
        host: impl Into<String>,
        address: impl Into<String>,
    ) -> Result<(), StateError> {
        self.listen(code, address)?;
        self.relay = Some(RelayRoute {
            host: host.into(),
            admitted: false,
        });
        Ok(())
    }

    /// Records that the relay took this console's waiting connection in
    /// (ST-M007-013). Only while the console waits on a relay; anywhere else
    /// it changes nothing. Returns whether the state changed.
    pub fn relay_admitted(&mut self) -> bool {
        if !matches!(self.phase, SessionPhase::Listening { .. }) {
            return false;
        }
        match &mut self.relay {
            Some(relay) if !relay.admitted => {
                relay.admitted = true;
                true
            }
            _ => false,
        }
    }

    /// Records that a customer has connected while the console was listening
    /// (ST-M002-012). The console never fabricates a target up front; it
    /// materialises the incoming target here, timed on the real arrival
    /// (`SessionEvent::Connected`, after the Noise handshake), selects it, and
    /// moves to [`SessionPhase::Connecting`] so the existing pairing and
    /// approval-code flow continues unchanged.
    ///
    /// The customer's own network address is not revealed to the listener, so
    /// the materialised target reuses the console's listening address purely as
    /// an internal label; it is never rendered as the customer's location.
    ///
    /// # Errors
    ///
    /// [`StateError::InvalidTransition`] unless the phase is
    /// [`SessionPhase::Listening`].
    pub fn customer_arrived(
        &mut self,
        id: TargetId,
        display_name: impl Into<String>,
    ) -> Result<(), StateError> {
        let address = match &self.phase {
            SessionPhase::Listening { address, .. } => address.clone(),
            _ => {
                return Err(StateError::InvalidTransition {
                    action: "accept the customer",
                    phase: phase_label(&self.phase),
                });
            }
        };
        self.upsert_target(DiscoveredTarget::new(
            id,
            display_name,
            address,
            DiscoverySource::Manual,
        ));
        self.selected = Some(id);
        self.phase = SessionPhase::Connecting;
        Ok(())
    }

    /// Selects a target for control, enforcing the single-selection invariant
    /// (REQ-F-016).
    ///
    /// Selecting the currently selected target is a no-op. Selecting a
    /// *different* target while a session is active is refused
    /// ([`StateError::SessionActive`]) — the operator must [`Self::stop`] first,
    /// which is the explicit switch action REQ-F-016 requires. Selecting a
    /// target after a terminal session resets it to [`SessionPhase::Discovered`]
    /// so a fresh pairing and consent are required (no silent restore).
    ///
    /// # Errors
    ///
    /// [`StateError::UnknownTarget`] for an unknown id, [`StateError::StaleTarget`]
    /// for a stale one, and [`StateError::SessionActive`] when another target is
    /// mid-session.
    pub fn select(&mut self, id: TargetId) -> Result<(), StateError> {
        let target = self
            .targets
            .iter()
            .find(|t| t.id == id)
            .ok_or(StateError::UnknownTarget(id))?;
        if target.stale {
            return Err(StateError::StaleTarget(id));
        }

        if self.selected == Some(id) {
            // Re-selecting the active target is a no-op; re-selecting a
            // terminal one clears it for a fresh attempt.
            if self.phase.is_active() {
                return Ok(());
            }
            self.session = None;
            self.phase = SessionPhase::Discovered;
            return Ok(());
        }

        if self.phase.is_active() {
            return Err(StateError::SessionActive);
        }

        self.selected = Some(id);
        self.session = None;
        self.phase = SessionPhase::Discovered;
        Ok(())
    }

    /// Clears the current selection.
    ///
    /// # Errors
    ///
    /// [`StateError::SessionActive`] if a session is live; stop it first.
    pub fn clear_selection(&mut self) -> Result<(), StateError> {
        if self.phase.is_active() {
            return Err(StateError::SessionActive);
        }
        self.selected = None;
        self.phase = SessionPhase::Idle;
        Ok(())
    }

    /// Begins connecting to the selected target.
    ///
    /// # Errors
    ///
    /// [`StateError::NoSelection`], [`StateError::StaleTarget`], or
    /// [`StateError::InvalidTransition`] if the phase is not
    /// [`SessionPhase::Discovered`].
    pub fn begin_connect(&mut self) -> Result<(), StateError> {
        self.require_fresh_selection()?;
        self.expect_phase("connect", matches!(self.phase, SessionPhase::Discovered))?;
        self.phase = SessionPhase::Connecting;
        Ok(())
    }

    /// Presents the Session Approval Code for comparison (REQ-F-005).
    ///
    /// # Errors
    ///
    /// [`StateError::InvalidTransition`] unless the phase is
    /// [`SessionPhase::Connecting`].
    pub fn present_approval_code(&mut self, code: impl Into<String>) -> Result<(), StateError> {
        self.expect_phase(
            "show the approval code",
            matches!(self.phase, SessionPhase::Connecting),
        )?;
        self.phase = SessionPhase::AwaitingApprovalCode { code: code.into() };
        Ok(())
    }

    /// Records that the approval code matched, moving to await consent.
    ///
    /// # Errors
    ///
    /// [`StateError::InvalidTransition`] unless the phase is
    /// [`SessionPhase::AwaitingApprovalCode`].
    pub fn confirm_approval_code(&mut self) -> Result<(), StateError> {
        self.expect_phase(
            "confirm the approval code",
            matches!(self.phase, SessionPhase::AwaitingApprovalCode { .. }),
        )?;
        self.phase = SessionPhase::AwaitingConsent;
        Ok(())
    }

    /// Grants attended full control and binds the negotiated session and full
    /// target identity (REQ-F-006, REQ-F-016). This is the only path to
    /// [`ControlLevel::FullControl`].
    ///
    /// # Errors
    ///
    /// [`StateError::NoSelection`] or [`StateError::InvalidTransition`] unless
    /// the phase is [`SessionPhase::AwaitingConsent`].
    pub fn grant_full_control(
        &mut self,
        session: SessionId,
        identity: TargetIdentity,
    ) -> Result<(), StateError> {
        let selected = self.selected.ok_or(StateError::NoSelection)?;
        self.expect_phase(
            "grant full control",
            matches!(self.phase, SessionPhase::AwaitingConsent),
        )?;
        if let Some(target) = self.targets.iter_mut().find(|t| t.id == selected) {
            target.identity = Some(identity);
        }
        self.session = Some(session);
        self.phase = SessionPhase::FullControl;
        Ok(())
    }

    /// Stops the session, invalidating its authority and rejecting new actions
    /// (REQ-F-018). Fail-closed and idempotent (REQ-N-004): stopping an already
    /// terminal or idle session is a successful no-op.
    ///
    /// # Errors
    ///
    /// [`StateError::NoSelection`] only when there is no selected target at all.
    pub fn stop(&mut self, reason: StopReason) -> Result<(), StateError> {
        // Stopping while merely listening ends the advertisement. There is no
        // selected target yet, so this is handled before the selection check so
        // the technician can always abort a listen (REQ-F-018, fail closed).
        if matches!(self.phase, SessionPhase::Listening { .. }) {
            self.session = None;
            self.phase = SessionPhase::Stopped { reason };
            return Ok(());
        }
        if self.selected.is_none() {
            return Err(StateError::NoSelection);
        }
        self.session = None;
        // Idempotent: a terminal or not-yet-started phase stays put but still
        // reports success, so a double-stop never fails and never re-opens.
        if self.phase.is_active() || matches!(self.phase, SessionPhase::Discovered) {
            self.phase = SessionPhase::Stopped { reason };
        }
        Ok(())
    }

    /// Fails the session at a named stage, invalidating its authority
    /// (REQ-N-015, fail closed). The message must be secret-free.
    pub fn fail(&mut self, stage: impl Into<String>, message: impl Into<String>) {
        self.session = None;
        self.phase = SessionPhase::Failed {
            stage: stage.into(),
            message: message.into(),
        };
    }

    /// Confirms a selection exists and its target is not stale.
    fn require_fresh_selection(&self) -> Result<(), StateError> {
        let target = self.selected_target().ok_or(StateError::NoSelection)?;
        if target.stale {
            return Err(StateError::StaleTarget(target.id));
        }
        Ok(())
    }

    /// Turns a phase predicate into an [`StateError::InvalidTransition`] on
    /// failure, labelling the current phase.
    fn expect_phase(&self, action: &'static str, ok: bool) -> Result<(), StateError> {
        if ok {
            Ok(())
        } else {
            Err(StateError::InvalidTransition {
                action,
                phase: phase_label(&self.phase),
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn technician() -> TechnicianHost {
        TechnicianHost {
            label: "This computer (technician)".to_string(),
            hostname: "TECH-MAC".to_string(),
            os: "macOS 15".to_string(),
            display_name: Some("Sam".to_string()),
        }
    }

    fn simulated_identity() -> TargetIdentity {
        TargetIdentity {
            hostname: "SIMULATED-TARGET".to_string(),
            os_version: "Simulated Windows".to_string(),
            arch: "x86_64".to_string(),
            elevated: true,
            simulated: true,
            agent_version: "sim".to_string(),
            account: None,
        }
    }

    /// Builds a console with `n` fresh simulated targets and returns their ids.
    fn console_with(n: usize) -> (ConsoleState, Vec<TargetId>) {
        let mut state = ConsoleState::new(technician());
        let mut ids = Vec::new();
        for i in 0..n {
            let id = TargetId::new();
            ids.push(id);
            state.upsert_target(DiscoveredTarget::new(
                id,
                format!("Target {i}"),
                format!("10.0.0.{i}:7000"),
                DiscoverySource::Simulated,
            ));
        }
        (state, ids)
    }

    /// Drives the selected target all the way to full control.
    fn drive_to_full_control(state: &mut ConsoleState) {
        state.begin_connect().unwrap();
        state.present_approval_code("42-17").unwrap();
        state.confirm_approval_code().unwrap();
        state
            .grant_full_control(SessionId::new(), simulated_identity())
            .unwrap();
    }

    #[test]
    fn only_one_target_is_selected_at_a_time() {
        let (mut state, ids) = console_with(3);
        state.select(ids[0]).unwrap();
        assert_eq!(state.selected, Some(ids[0]));

        state.select(ids[1]).unwrap();
        // Selection is a single Option, so exactly one target is ever selected.
        assert_eq!(state.selected, Some(ids[1]));
        assert_eq!(state.selected_target().map(|t| t.id), Some(ids[1]));
    }

    #[test]
    fn switching_targets_requires_stopping_the_active_session() {
        let (mut state, ids) = console_with(2);
        state.select(ids[0]).unwrap();
        drive_to_full_control(&mut state);
        assert!(state.can_issue_actions());

        // A switch mid-session is refused: control must be explicitly stopped.
        let err = state.select(ids[1]).unwrap_err();
        assert_eq!(err, StateError::SessionActive);
        assert_eq!(state.selected, Some(ids[0]));

        // After an explicit stop, the switch is allowed and lands fresh.
        state.stop(StopReason::TechnicianRequested).unwrap();
        state.select(ids[1]).unwrap();
        assert_eq!(state.selected, Some(ids[1]));
        assert_eq!(state.phase, SessionPhase::Discovered);
        assert!(state.session.is_none());
    }

    #[test]
    fn no_control_before_consent() {
        let (mut state, ids) = console_with(1);
        state.select(ids[0]).unwrap();
        assert_eq!(state.control_level(), ControlLevel::None);

        state.begin_connect().unwrap();
        assert_eq!(state.control_level(), ControlLevel::Connected);
        assert!(!state.can_issue_actions());

        state.present_approval_code("42-17").unwrap();
        assert!(!state.can_issue_actions());
        state.confirm_approval_code().unwrap();
        assert_eq!(state.control_level(), ControlLevel::Connected);
        assert!(!state.can_issue_actions());

        // Only after consent does control exist.
        state
            .grant_full_control(SessionId::new(), simulated_identity())
            .unwrap();
        assert_eq!(state.control_level(), ControlLevel::FullControl);
        assert!(state.can_issue_actions());
    }

    #[test]
    fn granting_full_control_is_rejected_before_consent() {
        let (mut state, ids) = console_with(1);
        state.select(ids[0]).unwrap();
        state.begin_connect().unwrap();
        // Skipping the approval-code and consent steps must fail closed.
        let err = state
            .grant_full_control(SessionId::new(), simulated_identity())
            .unwrap_err();
        assert!(matches!(err, StateError::InvalidTransition { .. }));
        assert_eq!(state.control_level(), ControlLevel::Connected);
    }

    #[test]
    fn target_identity_appears_only_once_connected() {
        let (mut state, ids) = console_with(1);
        state.select(ids[0]).unwrap();
        assert!(state.selected_target().unwrap().identity.is_none());

        drive_to_full_control(&mut state);
        let target = state.selected_target().unwrap();
        assert!(target.identity.is_some());
        // The remote identity is not the technician host.
        assert_ne!(
            target.identity.as_ref().unwrap().hostname,
            state.technician.hostname
        );
    }

    #[test]
    fn stop_is_fail_closed_and_idempotent() {
        let (mut state, ids) = console_with(1);
        state.select(ids[0]).unwrap();
        drive_to_full_control(&mut state);

        state.stop(StopReason::TechnicianRequested).unwrap();
        assert!(!state.can_issue_actions());
        assert!(state.session.is_none());
        assert_eq!(
            state.phase,
            SessionPhase::Stopped {
                reason: StopReason::TechnicianRequested
            }
        );

        // A second stop succeeds and does not re-open or change the outcome.
        state.stop(StopReason::CustomerRequested).unwrap();
        assert_eq!(
            state.phase,
            SessionPhase::Stopped {
                reason: StopReason::TechnicianRequested
            }
        );
        assert!(!state.can_issue_actions());
    }

    #[test]
    fn a_selected_target_going_stale_forces_the_session_closed() {
        let (mut state, ids) = console_with(1);
        state.select(ids[0]).unwrap();
        drive_to_full_control(&mut state);
        assert!(state.can_issue_actions());

        state.set_stale(ids[0], true);
        assert!(!state.can_issue_actions());
        assert!(state.session.is_none());
        assert_eq!(
            state.phase,
            SessionPhase::Stopped {
                reason: StopReason::TargetStale
            }
        );
    }

    #[test]
    fn a_stale_target_cannot_be_selected_or_connected() {
        let (mut state, ids) = console_with(2);
        state.set_stale(ids[0], true);
        let err = state.select(ids[0]).unwrap_err();
        assert_eq!(err, StateError::StaleTarget(ids[0]));

        // Selecting a fresh one, then it going stale, blocks connect.
        state.select(ids[1]).unwrap();
        state.set_stale(ids[1], true);
        let err = state.begin_connect().unwrap_err();
        assert_eq!(err, StateError::StaleTarget(ids[1]));
    }

    #[test]
    fn selecting_an_unknown_target_is_rejected() {
        let (mut state, _ids) = console_with(1);
        let ghost = TargetId::new();
        let err = state.select(ghost).unwrap_err();
        assert_eq!(err, StateError::UnknownTarget(ghost));
    }

    #[test]
    fn reconnect_after_stop_requires_fresh_pairing_and_consent() {
        let (mut state, ids) = console_with(1);
        state.select(ids[0]).unwrap();
        drive_to_full_control(&mut state);
        state.stop(StopReason::CustomerRequested).unwrap();

        // Re-selecting the same target resets it to Discovered — consent is not
        // silently restored (aligns with ST-M002-006).
        state.select(ids[0]).unwrap();
        assert_eq!(state.phase, SessionPhase::Discovered);
        assert_eq!(state.control_level(), ControlLevel::None);
        // Jumping straight back to full control is impossible.
        let err = state
            .grant_full_control(SessionId::new(), simulated_identity())
            .unwrap_err();
        assert!(matches!(err, StateError::InvalidTransition { .. }));
    }

    #[test]
    fn actions_without_a_selection_report_no_selection() {
        let mut state = ConsoleState::new(technician());
        assert_eq!(state.begin_connect().unwrap_err(), StateError::NoSelection);
        assert_eq!(
            state.stop(StopReason::TechnicianRequested).unwrap_err(),
            StateError::NoSelection
        );
    }

    #[test]
    fn listen_clears_targets_and_enters_listening() {
        let (mut state, _ids) = console_with(3);
        state.listen("6RRR-2H8F", "192.168.1.20:7000").unwrap();
        assert_eq!(
            state.phase,
            SessionPhase::Listening {
                code: "6RRR-2H8F".to_string(),
                address: "192.168.1.20:7000".to_string(),
            }
        );
        // Listening shows no discovered targets and holds no selection/session.
        assert!(state.targets.is_empty());
        assert!(state.selected.is_none());
        assert!(state.session.is_none());
        assert_eq!(state.control_level(), ControlLevel::None);
        assert!(!state.can_issue_actions());
    }

    #[test]
    fn customer_arrival_moves_listening_to_connecting_and_can_reach_full_control() {
        let mut state = ConsoleState::new(technician());
        state.listen("6RRR-2H8F", "192.168.1.20:7000").unwrap();

        let id = TargetId::new();
        state
            .customer_arrived(id, "Incoming customer computer")
            .unwrap();
        assert_eq!(state.phase, SessionPhase::Connecting);
        assert_eq!(state.selected, Some(id));
        assert_eq!(state.control_level(), ControlLevel::Connected);
        assert!(!state.can_issue_actions());

        // The materialised target drives the existing pairing/consent flow all
        // the way to full control — no separate select/begin_connect needed.
        state.present_approval_code("42-17").unwrap();
        state.confirm_approval_code().unwrap();
        state
            .grant_full_control(SessionId::new(), simulated_identity())
            .unwrap();
        assert_eq!(state.control_level(), ControlLevel::FullControl);
        assert!(state.can_issue_actions());
    }

    #[test]
    fn customer_arrived_rejected_outside_listening() {
        let mut state = ConsoleState::new(technician());
        // Not listening yet: an arrival is refused and nothing is selected.
        let err = state
            .customer_arrived(TargetId::new(), "Incoming customer computer")
            .unwrap_err();
        assert!(matches!(err, StateError::InvalidTransition { .. }));
        assert!(state.selected.is_none());
        assert_eq!(state.phase, SessionPhase::Idle);
    }

    #[test]
    fn stop_ends_listening() {
        let mut state = ConsoleState::new(technician());
        state.listen("6RRR-2H8F", "192.168.1.20:7000").unwrap();
        // Stop is available while listening even though no target is selected.
        state.stop(StopReason::TechnicianRequested).unwrap();
        assert_eq!(
            state.phase,
            SessionPhase::Stopped {
                reason: StopReason::TechnicianRequested
            }
        );
        assert!(state.session.is_none());
    }

    #[test]
    fn listen_rejected_while_active() {
        let (mut state, ids) = console_with(1);
        state.select(ids[0]).unwrap();
        drive_to_full_control(&mut state);
        // A live session must be explicitly stopped before listening again.
        let err = state.listen("6RRR-2H8F", "192.168.1.20:7000").unwrap_err();
        assert!(matches!(err, StateError::InvalidTransition { .. }));
        assert_eq!(state.control_level(), ControlLevel::FullControl);
    }

    #[test]
    fn state_serializes_for_the_frontend() {
        let (mut state, ids) = console_with(1);
        state.select(ids[0]).unwrap();
        drive_to_full_control(&mut state);
        let json = serde_json::to_string(&state).unwrap();
        // The serialized snapshot names the phase and carries the target,
        // which is what the frontend renders.
        assert!(json.contains("\"phase\":\"full_control\""));
        assert!(json.contains("SIMULATED-TARGET"));
        let round: ConsoleState = serde_json::from_str(&json).unwrap();
        assert_eq!(round, state);
    }

    // --- A relay (ST-M007-013) ---

    #[test]
    fn waiting_on_a_relay_names_the_relay_and_says_when_it_holds_the_rendezvous() {
        let mut state = ConsoleState::new(technician());
        assert_eq!(state.relay, None);
        // Nothing to admit while no relay is waited on.
        assert!(!state.relay_admitted());

        state
            .listen_on_relay("53ST-A71D-JKKV-0VGH", "relay.example.org", "192.0.2.1:443")
            .unwrap();
        assert_eq!(
            state.phase,
            SessionPhase::Listening {
                code: "53ST-A71D-JKKV-0VGH".to_string(),
                address: "192.0.2.1:443".to_string(),
            }
        );
        assert_eq!(
            state.relay,
            Some(RelayRoute {
                host: "relay.example.org".to_string(),
                admitted: false,
            })
        );
        // No authority, and no target that could be selected.
        assert_eq!(state.control_level(), ControlLevel::None);
        assert!(state.targets.is_empty());

        assert!(state.relay_admitted());
        assert!(state.relay.as_ref().unwrap().admitted);
        // Said once.
        assert!(!state.relay_admitted());
    }

    #[test]
    fn a_session_through_a_relay_takes_every_step_of_one_on_a_network() {
        let mut state = ConsoleState::new(technician());
        state
            .listen_on_relay("53ST-A71D-JKKV-0VGH", "relay.example.org", "192.0.2.1:443")
            .unwrap();
        state.relay_admitted();
        let id = TargetId::new();
        state
            .customer_arrived(id, "Incoming customer computer")
            .unwrap();
        // The relay stays named through the session; it grants nothing.
        let named = |state: &ConsoleState| state.relay.as_ref().map(|relay| relay.host.clone());
        assert_eq!(named(&state).as_deref(), Some("relay.example.org"));
        assert!(!state.can_issue_actions());
        state.present_approval_code("482913").unwrap();
        assert_eq!(state.control_level(), ControlLevel::Connected);
        // Consent cannot be skipped because a relay is in between.
        assert!(
            state
                .grant_full_control(SessionId::new(), simulated_identity())
                .is_err()
        );
        state.confirm_approval_code().unwrap();
        state
            .grant_full_control(SessionId::new(), simulated_identity())
            .unwrap();
        assert!(state.can_issue_actions());
        assert_eq!(named(&state).as_deref(), Some("relay.example.org"));
        // Once the customer is there the relay is no longer "admitting".
        assert!(!state.relay_admitted());

        state.stop(StopReason::Disconnected).unwrap();
        assert!(!state.can_issue_actions());
        // The end can still say which way the session went.
        assert_eq!(named(&state).as_deref(), Some("relay.example.org"));
        // The next wait on this network names no relay.
        state.listen("6RRR-2H8F", "10.0.0.42:7000").unwrap();
        assert_eq!(state.relay, None);
    }

    #[test]
    fn a_relay_cannot_be_waited_on_while_a_session_is_live() {
        let (mut state, ids) = console_with(1);
        state.select(ids[0]).unwrap();
        state.begin_connect().unwrap();
        assert!(matches!(
            state.listen_on_relay("53ST-A71D-JKKV-0VGH", "relay.example.org", "192.0.2.1:443"),
            Err(StateError::InvalidTransition { .. })
        ));
        assert_eq!(state.relay, None);
    }

    #[test]
    fn the_relay_is_part_of_the_state_the_window_gets_only_when_there_is_one() {
        let mut state = ConsoleState::new(technician());
        assert!(serde_json::to_value(&state).unwrap().get("relay").is_none());
        state
            .listen_on_relay("53ST-A71D-JKKV-0VGH", "relay.example.org", "192.0.2.1:443")
            .unwrap();
        assert_eq!(
            serde_json::to_value(&state).unwrap()["relay"],
            serde_json::json!({ "host": "relay.example.org", "admitted": false })
        );
        // A state from before this field is read without it.
        let old: ConsoleState = serde_json::from_value(serde_json::json!({
            "technician": { "label": "x", "hostname": "TECH-LAPTOP", "os": "Windows 11" },
            "targets": [],
            "phase": { "phase": "idle" },
        }))
        .unwrap();
        assert_eq!(old.relay, None);
    }
}
