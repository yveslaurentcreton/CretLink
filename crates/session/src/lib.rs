//! Portable attended-LAN session runtime (ST-M002-005, ST-M002-006).
//!
//! This crate turns the lower-level building blocks — LAN discovery
//! (`cretlink-discovery`), the Noise pairing and Session Approval Code
//! (`cretlink-transport`), the versioned control schema (`cretlink-protocol`),
//! and the audit vocabulary (`cretlink-shared`) — into a single in-process
//! session lifecycle that a console and an agent on the same LAN can drive:
//!
//! 1. connect and derive the same six-digit **Session Approval Code**;
//! 2. grant **one** informed full-control consent (REQ-F-006, REQ-S-003);
//! 3. hold an active session ([`SessionEvent::Active`]) in which the console
//!    drives jobs, file operations (transfers, the drive list and the removal
//!    of a folder among them), interactive terminals and the desktop surface,
//!    with liveness heartbeats underneath; and
//! 4. **revoke and clean up** from either side, fail-closed (REQ-F-018,
//!    REQ-F-019, REQ-S-004, REQ-S-009).
//!
//! # Topology (ADR-0003, ADR-0007)
//!
//! The **agent** is the Noise initiator and dials; the **console** listens.
//! [`SessionHost`] is the console side ([`Role::Responder`]); [`SessionJoiner`]
//! is the agent side ([`Role::Initiator`]). The console binds a private-interface
//! address obtained from
//! [`private_interface_addrs`](cretlink_discovery::private_interface_addrs) — it
//! must never bind `0.0.0.0` (REQ-S-010).
//!
//! # No control before consent, fail-closed after (REQ-S-001, REQ-S-009)
//!
//! No control capability exists until the customer calls
//! [`JoinHandle::grant_consent`], which sends the
//! [`ControlMessage::ConsentGranted`](cretlink_protocol::ControlMessage) that
//! binds the honest [`TargetIdentity`]. Any channel fault, decode error, peer
//! disconnect, or revocation **drops the [`SecureChannel`](cretlink_transport::SecureChannel)**
//! — invalidating the ephemeral session keys (REQ-S-007) — and moves the
//! session to a terminal state. A session is never resumed: a reconnect requires
//! a fresh [`SessionHost::host`]/[`SessionJoiner::join`] (no silent restore).
//!
//! [`Role::Responder`]: cretlink_transport::Role::Responder
//! [`Role::Initiator`]: cretlink_transport::Role::Initiator

use core::fmt;
use std::collections::BTreeMap;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64};
use std::time::Duration;

use cretlink_protocol::{DisplayName, ErrorCode, SessionApprovalCode, TargetIdentity};
use cretlink_shared::{Actor, Outcome};
use cretlink_transport::{PairingError, SecureChannelError};
use serde::{Deserialize, Serialize};
use thiserror::Error;

mod action;
pub mod activity;
mod dial;
mod files;
mod host;
mod joiner;
mod reboot;
mod redeem;
pub mod report;
mod runtime;
mod transfer;

pub use action::{
    ActionOutcome, Attribution, MAX_ACTION_DESCRIPTION_CHARS, describe_cancel, describe_operation,
    is_transfer_step,
};
pub use activity::{
    ActivityEntry, ActivityFeed, ActivityItem, ActivityKind, ActivitySource, ActivityState,
    CustomerActivity,
};
pub use dial::{Dial, DirectDial, RelayDial, WssRelayDial};
pub use files::{TreeRemovalError, TreeRemovalProgress, TreeRemovalReport};
// The wss trust material a retrying endpoint builds once and clones per attempt
// (see `WssRelayDial::connector`). Re-exported so a caller can name the connector
// it holds across a reconnect loop without a direct `cretlink-wss` dependency.
pub use cretlink_wss::WssConnector;
pub use host::{HostHandle, SessionHost};
pub use joiner::{JoinHandle, SessionJoiner};
pub use reboot::{
    ArmOutcome, NoRebootResume, RebootResumeArmError, RebootResumeArmer, RebootResumeTicketId,
};
pub use redeem::{
    AnswerOutcome, RedemptionClock, RedemptionOutcome, RedemptionRefusal, agent_answer_challenge,
    console_redeem,
};
pub use report::{
    CleanupNote, CleanupResult, CleanupState, CustomerCounts, CustomerReport, CustomerReportInput,
    EndedBy,
};
pub use transfer::{CancelToken, DownloadRequest, TransferProgress, TransferReport, UploadRequest};

// The revocation-reason vocabulary lives in the protocol crate (it travels on
// the wire); re-export it so runtime callers depend on one name.
pub use cretlink_protocol::RevokeReason;
// Exec-facing wire vocabulary, re-exported so console/agent callers name one
// crate for the whole command surface (ST-M003-001/002). `JobId`/`SessionId`
// appear in the public `HostHandle` exec API, so they are re-exported too.
pub use cretlink_protocol::{
    CAP_DESKTOP_V1, CAP_FS_V2, CAP_REBOOT_RESUME_V1, CAP_TERM_V1, JobId, JobOutcome, JobSummary,
    Origin, OutputStream, Purpose, SessionId, Shell,
};
// File-operation wire vocabulary surfaced by `HostHandle::file_op` and the
// transfer engine, re-exported so callers name one crate (ST-M003-004/005).
pub use cretlink_protocol::{
    B64Bytes, CleanupFailure, CleanupPolicy, CleanupTrigger, DigestAlgo, FileEntry, FileKind,
    OpResult, Operation, RemotePath, StagedEntry,
};
// The drive list and the folder removal of the technician's file explorer,
// surfaced by `HostHandle::list_drives` and `HostHandle::remove_tree`
// (ST-M010-001).
pub use cretlink_protocol::{DriveEntry, DriveKind, TreeFailure, TreeState};
// Interactive-desktop wire vocabulary surfaced by the `HostHandle` desktop API
// (ADR-0015, ST-M005-001/002/003), re-exported so callers name one crate for
// the whole capture-and-input surface.
pub use cretlink_protocol::{
    DesktopAccess, DesktopFrameMeta, DesktopState, ImageFormat, KeyAction, PointerAction,
    PointerButton,
};
// Relay-dial vocabulary surfaced by [`RelayDial`] (ST-M007-008, ADR-0016 §5),
// re-exported so the console and agent name a single crate for the whole
// transport-dial surface: the single-use rendezvous ticket (with its hex
// parse/format and typed error) and the optional admission credential.
pub use cretlink_relay::{AdmissionToken, RendezvousTicket, TicketError};

/// Probes whether the target reports a pending reboot, injected into the agent
/// side of the runtime so `SystemInfo` answers are **live** yet deterministic
/// under test (ST-M003-003): the support agent wires the real Windows registry
/// probe; tests inject a fixed value. It runs on every `SystemInfo` request —
/// never cached — and `false` means "no reboot signal observed" (best-effort,
/// REQ-N-008).
pub type RebootProbe = std::sync::Arc<dyn Fn() -> bool + Send + Sync>;

/// A started remote job as reported by the agent: the assigned job id plus the
/// effective absolute **remote** working directory the job actually received
/// (ST-M003-003, REQ-F-009).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StartedJob {
    /// The identifier the agent assigned.
    pub job: JobId,
    /// The effective remote working directory: the validated requested
    /// directory, or the agent process's directory at spawn time. Empty only
    /// when the agent could not determine it (never fabricated, REQ-N-008).
    pub working_dir: String,
}

/// A console-side desktop-capture request (ADR-0015, ST-M005-001).
///
/// The optional downscale (`max_width`/`max_height`) and `quality` hints are
/// validated against `cretlink_protocol::limits` locally, before the request
/// reaches the live session, so a malformed request fails on the call and never
/// touches the target (REQ-N-012).
#[derive(Debug, Clone)]
pub struct DesktopCaptureRequest {
    /// The zero-based display index to capture within the session.
    pub display: u16,
    /// The desired image encoding.
    pub format: ImageFormat,
    /// An optional maximum output width in pixels; `None` captures at native
    /// width. The agent scales to fit while preserving aspect ratio.
    pub max_width: Option<u16>,
    /// An optional maximum output height in pixels (see `max_width`).
    pub max_height: Option<u16>,
    /// An optional encoder quality on a 1..=100 scale for a lossy `format`;
    /// ignored for [`ImageFormat::Png`]. `None` uses the agent default.
    pub quality: Option<u8>,
}

impl DesktopCaptureRequest {
    /// A native-resolution capture of `display` in `format`, with no downscale
    /// or quality override.
    #[must_use]
    pub fn new(display: u16, format: ImageFormat) -> Self {
        Self {
            display,
            format,
            max_width: None,
            max_height: None,
            quality: None,
        }
    }

    /// Lowers this request into its wire [`Operation`].
    pub(crate) fn into_operation(self) -> Operation {
        Operation::DesktopCapture {
            display: self.display,
            format: self.format,
            max_width: self.max_width,
            max_height: self.max_height,
            quality: self.quality,
        }
    }
}

/// One captured desktop frame as delivered to the console (ADR-0015,
/// ST-M005-001).
///
/// The frame's [`DesktopFrameMeta`] always describes the desktop's honest state
/// at capture time; a pixel payload is present only when
/// [`DesktopFrameMeta::state`] is [`DesktopState::Active`]. Exactly one of
/// [`Self::image`] (inlined when it fits `MAX_DESKTOP_INLINE_BYTES`) or
/// [`Self::staged`] (a staged-artifact reference read back over the ordinary
/// file path) carries it; both are `None` for a locked, switched, disconnected,
/// or secure desktop, which is never dressed up with a stale frame (REQ-N-008).
#[derive(Debug, Clone)]
pub struct DesktopFrame {
    /// The frame's identity, geometry, live state, and capture timestamp.
    pub meta: DesktopFrameMeta,
    /// The inline encoded image, when it fit the inline bound.
    pub image: Option<Vec<u8>>,
    /// The staging path of the encoded frame, when it exceeded the inline bound.
    pub staged: Option<RemotePath>,
}

/// How often the active session exchanges liveness heartbeats. Heartbeats flow
/// even when no other traffic does, so a silently dropped connection becomes an
/// honest disconnect (REQ-N-008) rather than a stall.
pub(crate) const HEARTBEAT_INTERVAL: Duration = Duration::from_millis(150);

/// How long the console waits for the single inbound connection before failing.
pub(crate) const ACCEPT_TIMEOUT: Duration = Duration::from_secs(30);

/// How long either side waits for the Noise pairing to complete.
pub(crate) const PAIR_TIMEOUT: Duration = Duration::from_secs(15);

/// How long each side waits for the next consent-phase control message.
pub(crate) const CONSENT_TIMEOUT: Duration = Duration::from_secs(120);

/// How long a locally-revoking side keeps the connection open, draining, so the
/// peer can read the `Revoke` before the socket closes — preventing an abortive
/// reset from wiping the Revoke out of the peer's receive buffer. Bounded so a
/// silent peer cannot stall teardown.
pub(crate) const TEARDOWN_DRAIN: Duration = Duration::from_secs(2);

/// How long the console keeps draining after its own `Revoke` for the agent's
/// teardown report (protocol minor 11, ST-M009-012). The agent stops its jobs
/// and sweeps its staging area first, so this is longer than
/// [`TEARDOWN_DRAIN`]; the drain ends as soon as the report arrives or the
/// agent closes.
pub(crate) const CLEANUP_REPORT_WAIT: Duration = Duration::from_secs(5);

/// The honest full-control disclosure the customer sees before consenting
/// (REQ-S-003, REQ-S-016).
///
/// The [`Default`] text is the canonical language: it states that the technician
/// can run commands, install software and drivers, change settings and files,
/// and restart the computer, and that command output and files may be sent to
/// the technician and their AI service. It is sent verbatim to the console in a
/// [`ControlMessage::ConsentRequested`](cretlink_protocol::ControlMessage) so
/// both endpoints show the same words.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DisclosureSummary {
    /// The full-control disclosure text shown to the customer.
    pub text: String,
}

impl DisclosureSummary {
    /// The canonical full-control disclosure text (REQ-S-003, REQ-S-016).
    pub const DEFAULT_TEXT: &'static str = "\
Granting full control lets the remote technician operate this computer as if \
seated at it for this session: run commands and scripts, install or remove \
software and drivers, change system settings, read, modify, and delete files, \
and restart the computer. Command output, screen contents, and files may be \
sent to the technician and processed by their AI service. Control lasts only \
until this session ends, and either of us can stop it at any time.";

    /// Builds a summary from explicit text.
    #[must_use]
    pub fn new(text: impl Into<String>) -> Self {
        Self { text: text.into() }
    }

    /// Borrows the disclosure text.
    #[must_use]
    pub fn text(&self) -> &str {
        &self.text
    }
}

impl Default for DisclosureSummary {
    fn default() -> Self {
        Self::new(Self::DEFAULT_TEXT)
    }
}

/// An honest account of session-scoped teardown (REQ-F-019).
///
/// Teardown drops the secure channel and its ephemeral keys; on the agent it
/// also cancels every running job's process tree (commands and terminals) and
/// sweeps the session's staging area. A clean stop reports `ok = true`. A staged
/// artifact that could not be removed flips `ok` to `false` with a note, so the
/// residual is surfaced prominently and audited as a failed cleanup
/// (REQ-S-012). `notes` never carries a secret.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CleanupReport {
    /// Whether every session-scoped resource was torn down. `false` means a
    /// residual remains and must be surfaced prominently.
    pub ok: bool,
    /// Human-readable, secret-free notes about what was torn down or left
    /// behind.
    pub notes: Vec<String>,
}

impl CleanupReport {
    /// A clean teardown with an optional note.
    fn clean(note: impl Into<String>) -> Self {
        Self {
            ok: true,
            notes: vec![note.into()],
        }
    }
}

/// A discrete session lifecycle event, observed identically on both endpoints.
///
/// The stream always begins with [`Self::Connected`] (the Session Approval Code
/// is then available via `approval_code()`), pauses on
/// [`Self::AwaitingApprovalCompare`] until the technician confirms the code
/// matches on both endpoints, progresses to [`Self::AwaitingConsent`], and ends
/// in exactly one terminal event ([`Self::Revoked`] or [`Self::Failed`]).
/// [`Self::Active`] is reached only through granted consent, never before
/// (REQ-S-001).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionEvent {
    /// Pairing completed; the Session Approval Code is ready for comparison.
    Connected,
    /// The secure channel is up and the Session Approval Code is displayed on
    /// both endpoints; the session is paused for a humanly-perceptible window
    /// while the operators compare the code out of band, before any full-control
    /// consent is requested (REQ-S-006, ST-M002-013). Emitted on **both** sides:
    /// the console waits here for the technician to confirm the match (which
    /// sends [`cretlink_protocol::ControlMessage::ApprovalCompareConfirmed`]),
    /// and the agent waits here for that confirmation to arrive. A reported
    /// mismatch, or a revoke, stop, disconnect, or timeout during the window,
    /// fails the session closed and grants no control (REQ-S-009).
    AwaitingApprovalCompare,
    /// The customer is being asked for one informed full-control consent; no
    /// control exists yet.
    AwaitingConsent,
    /// Consent was granted, binding the honest target identity (REQ-F-016).
    ConsentGranted {
        /// The honest identity of the target now under control.
        identity: TargetIdentity,
    },
    /// The session is active under full control (REQ-F-006). Command execution
    /// is available from here (ST-M003-001/002); heartbeats continue underneath.
    Active,
    /// The agent started running a command (ST-M003-001). Emitted on the agent
    /// side so the customer sees exactly what the technician runs — visible
    /// activity, nothing hidden (REQ-F-017, REQ-S-002).
    CommandStarted {
        /// The job the agent assigned.
        job: JobId,
        /// Which shell runs the command.
        shell: Shell,
        /// The command text (as received from the console).
        command: String,
        /// Which local actor drove the command — the AI (via the MCP bridge) or
        /// the technician — as carried on the wire request, so the attended
        /// customer surface can label AI-driven work distinctly (ST-M004-006,
        /// REQ-F-017, REQ-S-002).
        origin: Origin,
        /// The caller's stated purpose of the command, as carried on the wire
        /// request: the AI client's plain-language reason, shown beside the
        /// real command — never instead of it (REQ-F-026, ST-M009-007). `None`
        /// for technician-driven commands, which are labelled by `origin`.
        purpose: Option<Purpose>,
    },
    /// The agent handled one console request other than a command it started
    /// (that one is [`Self::CommandStarted`] and later [`Self::ExecFinished`]).
    /// Emitted on the agent side only, so the attended customer surface shows
    /// every action on their computer with who drove it, the caller's stated
    /// purpose, the real operation, and the honest result (REQ-F-017,
    /// REQ-F-026, ST-M009-007). Internal file-transfer steps (chunk reads and
    /// writes, resume digests, aborts) are not reported one by one; a
    /// transfer's commit is. Delivered under the same lifecycle reserve as
    /// streamed output, so a burst of actions can never crowd out a stop or
    /// revoke event.
    ///
    /// A file leaving the customer computer is reported twice under one `id`:
    /// when sending starts (`outcome` [`ActionOutcome::InProgress`]) and again
    /// when it completes or fails, with the byte count. A received file is
    /// reported at its commit with its byte count (ST-M009-011).
    ///
    /// The removal of a folder with its content takes several requests and is
    /// reported under one `id` too: in progress when its first step leaves
    /// more to do, and once more when it ends — done only when the folder is
    /// gone, cancelled, or failed with the first reason (ST-M010-001).
    RequestHandled {
        /// Stable for the life of one activity item: a later `RequestHandled`
        /// with the same id (a finished file transfer) replaces the earlier one.
        /// Minted per session by the agent, starting at 1.
        id: u64,
        /// Which local actor drove the request (from the wire `origin`).
        origin: Origin,
        /// The caller's stated purpose (from the wire `purpose`): an unverified
        /// claim, shown beside `operation`, never instead of it. `None` for
        /// technician-driven requests.
        purpose: Option<Purpose>,
        /// What the request does, derived by the agent from the request itself
        /// ([`ActivityKind::of`]), ST-M009-011.
        kind: ActivityKind,
        /// The real operation in plain words ([`describe_operation`]), bounded
        /// and free of typed, clipboard, stdin, or file content.
        operation: String,
        /// The remote path the request acts on, when there is one (the source
        /// of a move), bounded like the operation text.
        path: Option<String>,
        /// The byte count, when known: the bytes sent so far or in total for a
        /// file leaving the computer, the committed length of a received file.
        bytes: Option<u64>,
        /// How the agent answered the request.
        outcome: ActionOutcome,
        /// How long the agent took, in milliseconds: from the request (the
        /// first read of a file transfer) to its answer (the last read).
        /// `None` while a transfer is still in progress.
        duration_ms: Option<u64>,
    },
    /// Activity events ([`Self::RequestHandled`], [`Self::CommandStarted`],
    /// [`Self::ExecFinished`]) were shed on the agent side to keep the bounded
    /// event channel's lifecycle reserve free (REQ-N-006). Emitted, at the
    /// place in the stream where they were dropped, as soon as there is room
    /// again, so the customer surface shows that `count` items were not shown
    /// instead of a silent gap (ST-M009-011, REQ-N-008).
    ActivityShed {
        /// How many activity events were dropped since the last marker.
        count: u64,
    },
    /// A chunk of a running job's output (ST-M003-002). Emitted on the console
    /// side as it streams in. Bytes are raw, untrusted remote output.
    ExecOutput {
        /// The job that produced the bytes.
        job: JobId,
        /// Which stream the bytes came from.
        stream: OutputStream,
        /// The raw output bytes.
        bytes: Vec<u8>,
    },
    /// Some of a job's streamed output was **dropped** to keep the console's
    /// bounded event buffer within its documented limit (REQ-N-006): a remote
    /// target that streams faster than the operator consumes cannot exhaust
    /// technician-side memory. Surfaced instead of silently truncating the
    /// stream (REQ-N-008); `bytes` is the number of output bytes shed for `job`
    /// since the previous delivered chunk or drop marker.
    ExecOutputDropped {
        /// The job whose output was shed.
        job: JobId,
        /// How many output bytes were dropped.
        bytes: u64,
    },
    /// A job ended, with its honest outcome (ST-M003-001/002, REQ-N-008).
    ExecFinished {
        /// The job that ended.
        job: JobId,
        /// The process exit code, when one exists.
        exit_code: Option<i32>,
        /// The honest outcome — never a fabricated success.
        outcome: JobOutcome,
    },
    /// The interactive desktop was observed or controlled (ST-M005-005). Emitted
    /// on **both** endpoints: agent-side after a desktop operation is serviced,
    /// so the customer surface can show live observation/control the same way it
    /// shows a running command; and console-side when the agent's
    /// [`cretlink_protocol::EventKind::DesktopActivity`] reflection arrives, so
    /// the technician sees the same state from the one endpoint that knows what
    /// truly happened after pacing or refusal (REQ-F-017 through REQ-F-020). It
    /// carries the interaction class only — never a captured pixel, keystroke, or
    /// clipboard text (REQ-S-016 through REQ-S-018).
    DesktopActivity {
        /// Whether the agent passively observed or actively drove input.
        access: DesktopAccess,
    },
    /// A single-use reboot-resume ticket was armed on the agent and reflected to
    /// both endpoints (ADR-0014, ST-M006-001). It carries **no key material** —
    /// only the clear ticket id and expiry — so an operator surface can show
    /// "armed until" on both sides. Arming grants no access and never restores
    /// control: attended consent still runs after any reconnect (REQ-S-001).
    RebootResumeArmed {
        /// The single-use ticket id (clear mirror).
        ticket_id: RebootResumeTicketId,
        /// Expiry as Unix milliseconds (UTC).
        not_after_unix_millis: i64,
    },
    /// A previously armed reboot-resume ticket was disarmed (arm-then-cancel) and
    /// reflected to both endpoints; nothing survives to a reboot (ADR-0014).
    RebootResumeDisarmed {
        /// The ticket id that was disarmed.
        ticket_id: RebootResumeTicketId,
    },
    /// The session was revoked and torn down fail-closed (REQ-S-004,
    /// REQ-S-009).
    Revoked {
        /// Why the session was revoked.
        reason: RevokeReason,
    },
    /// The session ended without reaching (or while leaving) active control
    /// other than a clean revoke — a denied consent, a pairing failure, or a
    /// timeout. `stage` and `message` are actionable and secret-free
    /// (REQ-N-015).
    Failed {
        /// Which stage produced the outcome (for example `"pairing"`,
        /// `"consent"`).
        stage: String,
        /// A plain-language, secret-free description.
        message: String,
    },
}

/// A single command to run on the remote target over an active session
/// (ST-M003-001). Decoupled from the wire [`Operation`] so callers depend only
/// on this runtime's surface.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecRequest {
    /// Which shell runs the command.
    pub shell: Shell,
    /// The command text (bounded by the protocol on the wire).
    pub command: String,
    /// An explicit remote working directory, or the agent's default when `None`.
    pub working_dir: Option<String>,
    /// Constrained environment overrides layered onto the agent's environment.
    pub env: BTreeMap<String, String>,
    /// A deadline in milliseconds; expiry yields [`JobOutcome::TimedOut`].
    pub timeout_ms: Option<u64>,
}

impl ExecRequest {
    /// Builds a request for `command` in `shell` with no working directory,
    /// environment overrides, or timeout.
    #[must_use]
    pub fn new(shell: Shell, command: impl Into<String>) -> Self {
        Self {
            shell,
            command: command.into(),
            working_dir: None,
            env: BTreeMap::new(),
            timeout_ms: None,
        }
    }

    /// Sets the command deadline in milliseconds.
    #[must_use]
    pub fn with_timeout_ms(mut self, timeout_ms: u64) -> Self {
        self.timeout_ms = Some(timeout_ms);
        self
    }

    pub(crate) fn into_operation(self) -> Operation {
        Operation::Exec {
            shell: self.shell,
            command: self.command,
            working_dir: self.working_dir,
            env: self.env,
            timeout_ms: self.timeout_ms,
        }
    }
}

/// A refused or failed session operation (fail-closed, REQ-S-009).
///
/// Every variant is safe to surface: it names the failure without leaking key
/// material, the Session Approval Code, or remote content (REQ-S-018,
/// REQ-N-015).
#[derive(Debug, Error)]
pub enum SessionError {
    /// The Noise pairing handshake or Session Approval Code round failed.
    #[error("secure pairing failed")]
    Pairing(#[source] PairingError),
    /// The encrypted channel failed (tamper, decode, or transport fault).
    #[error("secure channel failed")]
    Channel(#[source] SecureChannelError),
    /// A session stage exceeded its time budget.
    #[error("the session timed out")]
    Timeout,
    /// The session was revoked on this side; further actions are refused.
    #[error("the session was revoked locally")]
    RevokedLocally,
    /// The peer revoked the session; further actions are refused.
    #[error("the session was revoked by the peer")]
    RevokedByPeer,
    /// The peer disconnected; any in-flight outcome is unknown (REQ-N-008).
    #[error("the peer disconnected")]
    Disconnected,
    /// The customer declined full-control consent; no control was granted.
    #[error("full-control consent was denied")]
    ConsentDenied,
    /// A control message arrived out of order or with an unexpected shape.
    #[error("session protocol violation: {0}")]
    Protocol(String),
    /// The request violated a protocol rule and was rejected locally before
    /// anything reached the wire; the session stays healthy (REQ-N-012).
    #[error("the request is invalid [{code:?}]: {message}")]
    Invalid {
        /// The machine-readable category from the local validation, preserved
        /// so front ends report the true class (for example `invalid_path`
        /// for a rejected path) instead of a collapsed generic one
        /// (ST-M003-003, REQ-N-015).
        code: ErrorCode,
        /// The human-readable, secret-free description.
        message: String,
    },
    /// The agent rejected the operation with a wire error (for example an
    /// [`ErrorCode::UnknownSession`] on a stale-session request, or
    /// [`ErrorCode::UnknownJob`] on a bad job id). Secret-free.
    #[error("agent rejected the operation [{code:?}]: {message}")]
    Remote {
        /// The machine-readable category the agent returned.
        code: ErrorCode,
        /// The agent's human-readable, secret-free description.
        message: String,
    },
    /// The agent answered with a message that did not fit the request.
    #[error("unexpected reply from the agent during {context}")]
    Unexpected {
        /// Where the unexpected reply appeared.
        context: &'static str,
    },
    /// An operation was attempted before the session reached the active phase.
    #[error("no active session")]
    NotActive,
    /// Binding the console listener or dialing the target failed.
    #[error("session transport setup failed")]
    Io(#[source] std::io::Error),
}

/// A sink for session lifecycle audit events (REQ-F-017, REQ-S-017).
///
/// The runtime reports every outcome — pairing, consent, revocation, disconnect,
/// cleanup — through this sink using the shared audit vocabulary
/// ([`Actor`]/[`Outcome`]). It is object-safe so a caller can adapt it to the
/// `cretlink-shared` [`AuditWriter`](cretlink_shared::AuditWriter) (or any other
/// durable sink) without this portable runtime taking a filesystem dependency.
/// `action` is a stable, secret-free label.
pub trait SessionAudit: Send + Sync + core::fmt::Debug {
    /// Records one session lifecycle event.
    fn record(&self, actor: Actor, action: &'static str, outcome: Outcome);

    /// Records the honest identity the customer's consent bound, right after
    /// `consent_granted`, so a later reader knows which computer the session
    /// controlled (ST-M009-012, REQ-F-016). Defaulted to a no-op for sinks that
    /// keep only the lifecycle.
    fn record_target(&self, _identity: &TargetIdentity) {}

    /// Records how the session ended and the peer's teardown report, after the
    /// terminal lifecycle record and before the log is sealed (ST-M009-012).
    /// Defaulted to a no-op for sinks that keep only the lifecycle.
    fn record_end(&self, _end: &SessionEnd) {}
}

/// How a session ended, as the runtime reports it to [`SessionAudit::record_end`]
/// (ST-M009-012).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionEnd {
    /// Who ended it, from this side's point of view.
    pub ended_by: EndedBy,
    /// The terminal lifecycle action (`revoked`, `disconnected`, `error`,
    /// `consent_denied`, `compare_mismatch`).
    pub action: &'static str,
    /// Whether full control was granted before the end. Without it nothing ran
    /// on the customer computer, so there was nothing to clean up there.
    pub control_granted: bool,
    /// On the console side, the teardown report the agent sent
    /// ([`cretlink_protocol::ControlMessage::CleanupReported`]); `None` when
    /// none arrived (a lost connection, an older peer), which is never
    /// success.
    pub peer_cleanup: Option<CleanupReport>,
}

/// An audit sink that discards every event — the default when a caller wires no
/// durable audit. The runtime's honest [`SessionEvent`] stream is unaffected.
#[derive(Debug, Default, Clone, Copy)]
pub struct NoAudit;

impl SessionAudit for NoAudit {
    fn record(&self, _actor: Actor, _action: &'static str, _outcome: Outcome) {}
}

/// Local, secret-free identity hints for the console side.
///
/// In this runtime the **agent** transmits the honest [`TargetIdentity`]; the
/// console does not send an identity. These hints are for the console's own
/// display and audit context only.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ConsoleHints {
    /// A human label for the technician console, for example
    /// `"This computer (technician)"`.
    pub label: String,
    /// The technician's display name as the operating-system account reports
    /// it (ST-M009-011). The console sends it to the agent in the pairing
    /// handshake ([`cretlink_protocol::PairingMessage::Nonce`], protocol minor
    /// 10) so the customer side can show who the technician's computer says is
    /// helping. A label only: it grants no authority. `None` sends no name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<DisplayName>,
}

/// Cross-handle shared state a session task publishes and its handle reads.
///
/// Uses lock-free primitives and write-once cells so neither side blocks and no
/// reachable path can panic (no `unwrap`): the approval code and cleanup report
/// are each set exactly once by the task.
pub(crate) struct Shared {
    approval_code: OnceLock<SessionApprovalCode>,
    disclosure: OnceLock<DisclosureSummary>,
    session: OnceLock<SessionId>,
    control_active: AtomicBool,
    heartbeats: AtomicU64,
    /// Expiry (Unix millis) of the currently-armed reboot-resume ticket, or `0`
    /// when none is armed (ADR-0014). A display-only indicator both endpoints
    /// publish from the arming reflection; it carries no key material and is
    /// never a security decision.
    armed_not_after: AtomicI64,
    cleanup: OnceLock<CleanupReport>,
    /// On the console side, the teardown report the agent sent before the
    /// channel closed (protocol minor 11, ST-M009-012).
    peer_cleanup: OnceLock<CleanupReport>,
    /// Who ended the session, from this side's point of view, set once at
    /// teardown (ST-M009-012).
    ended_by: OnceLock<EndedBy>,
    /// The console's display name received in the pairing handshake, on the
    /// agent side only (ST-M009-011). An unverified label the customer surface
    /// shows as what the technician's computer says.
    peer_display_name: OnceLock<DisplayName>,
    /// The display name this console sends in the pairing handshake, on the
    /// console side only (from [`ConsoleHints::display_name`], ST-M009-011).
    own_display_name: Option<DisplayName>,
}

impl fmt::Debug for Shared {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // The Session Approval Code is a screen-only comparison value and must
        // never reach a log (REQ-S-018): redact it, exposing only whether one
        // has been derived yet.
        f.debug_struct("Shared")
            .field(
                "approval_code",
                &self.approval_code.get().map(|_| "<redacted>"),
            )
            // The disclosure is the fixed, public consent text (not a secret);
            // show only whether one has been received to keep Debug concise.
            .field("disclosure", &self.disclosure.get().map(|_| "<received>"))
            .field("session", &self.session.get())
            .field("control_active", &self.control_active())
            .field("heartbeats", &self.heartbeats())
            .field("armed_not_after", &self.armed_not_after())
            .field("cleanup", &self.cleanup.get())
            .field("peer_cleanup", &self.peer_cleanup.get())
            .field("ended_by", &self.ended_by.get())
            .field("peer_display_name", &self.peer_display_name.get())
            .finish()
    }
}

impl Shared {
    fn new() -> Self {
        Self {
            approval_code: OnceLock::new(),
            disclosure: OnceLock::new(),
            session: OnceLock::new(),
            control_active: AtomicBool::new(false),
            heartbeats: AtomicU64::new(0),
            armed_not_after: AtomicI64::new(0),
            cleanup: OnceLock::new(),
            peer_cleanup: OnceLock::new(),
            ended_by: OnceLock::new(),
            peer_display_name: OnceLock::new(),
            own_display_name: None,
        }
    }

    /// Shared state for a console-side session that sends `display_name` in
    /// the pairing handshake (ST-M009-011).
    fn for_console(display_name: Option<DisplayName>) -> Self {
        Self {
            own_display_name: display_name,
            ..Self::new()
        }
    }

    fn own_display_name(&self) -> Option<DisplayName> {
        self.own_display_name.clone()
    }

    fn set_peer_display_name(&self, name: DisplayName) {
        // Set at most once, at pairing completion; a redundant set is ignored.
        let _ = self.peer_display_name.set(name);
    }

    fn peer_display_name(&self) -> Option<DisplayName> {
        self.peer_display_name.get().cloned()
    }

    fn set_session(&self, session: SessionId) {
        // Set exactly once, when the session id is adopted/minted; a redundant
        // set is ignored (write-once).
        let _ = self.session.set(session);
    }

    fn session(&self) -> Option<SessionId> {
        self.session.get().copied()
    }

    fn set_approval_code(&self, code: SessionApprovalCode) {
        // Set exactly once at pairing completion; a redundant set is ignored.
        let _ = self.approval_code.set(code);
    }

    fn approval_code(&self) -> Option<SessionApprovalCode> {
        self.approval_code.get().copied()
    }

    fn set_disclosure(&self, disclosure: DisclosureSummary) {
        // Set exactly once, when the console receives the agent's
        // `ConsentRequested`; a redundant set is ignored (write-once).
        let _ = self.disclosure.set(disclosure);
    }

    fn disclosure(&self) -> Option<DisclosureSummary> {
        self.disclosure.get().cloned()
    }

    fn set_control_active(&self, active: bool) {
        self.control_active
            .store(active, std::sync::atomic::Ordering::SeqCst);
    }

    fn control_active(&self) -> bool {
        self.control_active
            .load(std::sync::atomic::Ordering::SeqCst)
    }

    fn set_armed_not_after(&self, not_after_unix_millis: i64) {
        // A non-zero expiry marks the ticket armed; both endpoints set this from
        // the arming reflection so each can show "armed until".
        self.armed_not_after
            .store(not_after_unix_millis, std::sync::atomic::Ordering::SeqCst);
    }

    fn clear_armed(&self) {
        self.armed_not_after
            .store(0, std::sync::atomic::Ordering::SeqCst);
    }

    fn armed_not_after(&self) -> Option<i64> {
        // `0` is the frozen "not armed" sentinel; any armed ticket has a real
        // future expiry, so it can never collide with the sentinel in practice.
        match self
            .armed_not_after
            .load(std::sync::atomic::Ordering::SeqCst)
        {
            0 => None,
            not_after => Some(not_after),
        }
    }

    fn record_heartbeat(&self) -> u64 {
        self.heartbeats
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
            + 1
    }

    fn heartbeats(&self) -> u64 {
        self.heartbeats.load(std::sync::atomic::Ordering::SeqCst)
    }

    fn set_cleanup(&self, report: CleanupReport) {
        let _ = self.cleanup.set(report);
    }

    fn cleanup(&self) -> Option<CleanupReport> {
        self.cleanup.get().cloned()
    }

    fn set_peer_cleanup(&self, report: CleanupReport) {
        // The first report counts; a repeated one is ignored (write-once).
        let _ = self.peer_cleanup.set(report);
    }

    fn peer_cleanup(&self) -> Option<CleanupReport> {
        self.peer_cleanup.get().cloned()
    }

    fn set_ended_by(&self, ended_by: EndedBy) {
        let _ = self.ended_by.set(ended_by);
    }

    fn ended_by(&self) -> Option<EndedBy> {
        self.ended_by.get().copied()
    }
}
