//! The session task state machines shared by the console and agent sides.
//!
//! Both [`host_task`] and [`join_task`] own their [`SecureChannel`] for the
//! whole session and drive the same phases: pair, exchange informed consent,
//! hold an active session with heartbeats, then tear down fail-closed. Every
//! terminal path routes through [`finish`], which **drops the channel** (so the
//! ephemeral session keys are invalidated, REQ-S-007/009), records an honest
//! [`CleanupReport`], emits the terminal [`SessionEvent`], and audits the
//! outcome.
//!
//! # Cancellation discipline
//!
//! [`SecureChannel::recv`] is **cancel-safe** — its partial-read state lives in
//! the channel (see `cretlink_transport`), so a dropped `recv` future resumes
//! cleanly without losing bytes. The active loops here rely on that: they race
//! `recv` in a `select!` against console commands, streamed broker output,
//! heartbeats, and revocation, dropping the losing `recv` future every
//! iteration. `SecureChannel::send` is **not** cancel-safe, so a `send` is
//! always awaited to completion and only ever abandoned on a teardown path,
//! after which the channel is dropped and never used again. Even the one narrow
//! race — a local revoke landing at the same instant as an inbound frame — fails
//! **closed**: the AEAD record layer rejects any desynchronised read, poisoning
//! the channel rather than misinterpreting bytes.
//!
//! # An answer that cannot be sent (ST-M010-005)
//!
//! The secure channel refuses a frame that breaks a bound of the protocol, or
//! that is larger than one record, before it touches the stream; the channel
//! is then as it was. The agent's loop tells that refusal from a channel that
//! is gone (`Unsent`). An answer to a request that is refused this way fails
//! that one request with the reason (`answer`), and the session goes on: a
//! folder, a file or a clipboard that somebody filled must not be a way to
//! end the session. A frame that answers no request — streamed output, a
//! heartbeat — has no request to fail and ends the session as before.
//!
//! # Console output buffering (REQ-N-006)
//!
//! The console session task publishes lifecycle and streamed-output
//! [`SessionEvent`]s over a **bounded** channel (see
//! [`SESSION_EVENT_CHANNEL_CAPACITY`]). The console loop always drains the
//! socket — so control frames (cancel acks, revoke) stay responsive and
//! teardown never deadlocks — and applies a **drop-and-mark** policy to streamed
//! output: output is enqueued while the channel keeps [`LIFECYCLE_RESERVE`]
//! slots free for lifecycle/terminal events, and under sustained pressure the
//! excess bytes are shed and reported as [`SessionEvent::ExecOutputDropped`]
//! rather than silently lost (REQ-N-008). This bounds technician-side memory
//! against a remote target that floods output, without blocking the socket read
//! (which would stall the half-duplex channel).

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use cretlink_exec::{
    Admission, Broker, BrokerEvent, CaptureReject, CaptureRequest, FileStore, JobSpec,
};
use cretlink_protocol::{
    B64Bytes, CAP_DESKTOP_V1, CAP_FS_V2, CAP_TERM_V1, CleanupTrigger, ControlMessage,
    DesktopAccess, ErrorCode, ErrorStage, EventKind, JobId, JobOutcome, JobSummary, Message,
    OpResult, Operation, OutputStream, ProtocolVersion, RequestId, RevokeReason, SessionId,
    TargetIdentity, TreeState, canonical_capabilities, limits,
};
use cretlink_shared::{Actor, Outcome};
use cretlink_transport::{Pairing, Role, SecureChannel, SecureChannelError};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpListener;
use tokio::sync::mpsc::{self, UnboundedReceiver};
use tokio::sync::oneshot;
use tokio::time::{MissedTickBehavior, timeout};

use crate::reboot::ArmCommand;
use crate::report::{RESIDUAL_NOTE_PREFIX, TEARDOWN_FAILED_NOTE_PREFIX, fit_for_wire};
use crate::{
    ACCEPT_TIMEOUT, ActionOutcome, ActivityKind, Attribution, CLEANUP_REPORT_WAIT, CONSENT_TIMEOUT,
    CleanupReport, DisclosureSummary, EndedBy, HEARTBEAT_INTERVAL, PAIR_TIMEOUT, RebootProbe,
    RebootResumeArmError, RebootResumeArmer, RebootResumeTicketId, SessionAudit, SessionEnd,
    SessionError, SessionEvent, Shared, TEARDOWN_DRAIN,
};

/// What the agent's active loop answers a `SystemInfo` request from: the
/// honest consent-bound identity plus the injected live reboot probe
/// (ST-M003-003, REQ-F-016, REQ-S-011). The probe runs per request, so the
/// answer is never a consent-time cache.
pub(crate) struct SystemInfoSource {
    identity: TargetIdentity,
    reboot_probe: RebootProbe,
}

/// The largest raw output slice carried in one wire [`EventKind::JobOutput`]
/// event. A single Noise record holds at most ~64 KiB of plaintext, and base64
/// expands bytes by 4/3, so the broker's larger read chunks are re-split to this
/// bound before sending: `base64(WIRE_OUTPUT_CHUNK_BYTES)` plus the JSON envelope
/// stays comfortably inside one record (REQ-N-006). This is a session-layer
/// concern the broker delegates here (see `cretlink_exec::READ_CHUNK_BYTES`).
const WIRE_OUTPUT_CHUNK_BYTES: usize = 32 * 1024;

/// Capacity of a session task's bounded [`SessionEvent`] channel. This is the
/// resource limit that bounds buffered remote output on the technician machine
/// (REQ-N-006): each streamed `ExecOutput` carries at most
/// [`WIRE_OUTPUT_CHUNK_BYTES`], so buffered output is capped at roughly
/// `SESSION_EVENT_CHANNEL_CAPACITY * WIRE_OUTPUT_CHUNK_BYTES` (~4 MiB), no matter
/// how fast a remote target floods output. The agent side publishes only
/// low-volume lifecycle events, so the same bound is comfortably slack there.
pub(crate) const SESSION_EVENT_CHANNEL_CAPACITY: usize = 128;

/// Slots kept free in the console event channel for lifecycle and terminal
/// events, so a flood of streamed output can never crowd out a job's
/// `ExecFinished`, a drop marker, or the session's terminal event. Streamed
/// output is shed (drop-and-mark) once fewer than this many slots remain.
pub(crate) const LIFECYCLE_RESERVE: usize = 16;

/// What the console side asks its session task to send over the active channel,
/// with a one-shot reply the task resolves when the correlated `Response`/`Error`
/// arrives. Decoupled from the wire so [`crate::HostHandle`] never touches the
/// channel directly (which the single-owner task alone drives).
#[derive(Debug)]
pub(crate) struct HostCommand {
    /// The session id to stamp on the outbound frame. Production callers leave
    /// this `None` (the task stamps the negotiated session); a test may override
    /// it to prove session-scoping is enforced (REQ-S-012, REQ-S-014).
    pub(crate) session_override: Option<SessionId>,
    /// What to send.
    pub(crate) kind: Outbound,
    /// Which local actor drives this request and, for an AI action, the
    /// caller's stated purpose, stamped onto the outbound wire `Request` or
    /// `Cancel` so the agent (and the attended customer surface) can attribute
    /// it and show why the caller says it is done (ST-M004-006, ST-M009-007,
    /// REQ-F-017, REQ-F-026).
    pub(crate) attribution: Attribution,
    /// Where the correlated result is delivered.
    pub(crate) reply: oneshot::Sender<Result<OpResult, SessionError>>,
}

/// The two outbound console-driven frame shapes: a request operation or a job
/// cancellation (a distinct top-level message).
#[derive(Debug)]
pub(crate) enum Outbound {
    /// An [`Operation`] carried in a `Request`.
    Op(Operation),
    /// A `Cancel` for one job.
    Cancel(JobId),
}

/// The customer's consent decision, delivered to the agent task out of band of
/// the revoke channel so the active loop never has to reason about it.
#[derive(Debug)]
pub(crate) enum ConsentDecision {
    /// Grant one attended full-control consent.
    Grant,
    /// Decline; carry a secret-free reason.
    Deny(String),
}

/// The technician's approval-code comparison decision, delivered to the console
/// session task out of band of the revoke channel so the compare gate never has
/// to reason about revocation (REQ-S-006, ST-M002-013). It carries no approval
/// code: the code is compared out of band, and this only records the human
/// verdict on that comparison.
#[derive(Debug)]
pub(crate) enum CompareDecision {
    /// The Session Approval Code matched on both screens; release the gate so
    /// the customer can be asked to grant control.
    Match,
    /// The codes did not match; fail the session closed and grant nothing.
    Mismatch,
}

/// A classified inbound frame from [`recv_control`].
enum Incoming {
    /// A well-formed control message.
    Control(SessionId, ControlMessage),
    /// The peer closed the connection (EOF or plain I/O loss).
    Disconnected,
    /// The encrypted channel faulted (tamper, decode, oversize, poison).
    ChannelFault,
    /// A decrypted frame that was not a control message — a protocol violation.
    NonControl,
}

/// How a session ended; maps to a terminal [`SessionEvent`], a cleanup note, and
/// an audit outcome in [`describe_ending`].
#[derive(Debug, Clone, Copy)]
enum Ending {
    /// This side revoked/stopped with the given reason.
    LocalRevoke(RevokeReason),
    /// The peer revoked with the given reason.
    PeerRevoke(RevokeReason),
    /// The peer vanished during the active session.
    ActiveDisconnect,
    /// The peer vanished before the session became active, at `stage`.
    EarlyDisconnect(&'static str),
    /// The encrypted channel faulted at `stage`.
    ChannelFault(&'static str),
    /// The customer declined full-control consent.
    ConsentDenied,
    /// A stage exceeded its time budget.
    Timeout(&'static str),
    /// A control message arrived out of order or with an unexpected shape.
    Protocol(&'static str),
    /// The technician declared the Session Approval Codes did not match during
    /// the compare gate, so the session fails closed and grants no control
    /// (REQ-S-006, REQ-S-009, ST-M002-013).
    CompareMismatch,
}

/// One resolved step of a consent-phase wait: an inbound frame, a local revoke,
/// or an exceptional condition.
enum Step {
    /// A well-formed control frame arrived.
    Control(SessionId, ControlMessage),
    /// A local revoke/stop command arrived (carrying its reason).
    LocalRevoke(RevokeReason),
    /// The peer disconnected.
    Disconnected,
    /// The encrypted channel faulted.
    ChannelFault,
    /// A non-control frame arrived (protocol violation).
    NonControl,
    /// The wait exceeded its budget.
    TimedOut,
}

/// Best-effort emit of a lifecycle/terminal event over the bounded event
/// channel. A gone receiver just means the consumer is gone; a momentarily full
/// channel is avoided in practice because streamed output leaves
/// [`LIFECYCLE_RESERVE`] slots free (see [`deliver_output`]).
fn emit(tx: &mpsc::Sender<SessionEvent>, event: SessionEvent) {
    let _ = tx.try_send(event);
}

/// The agent's per-session bookkeeping of customer-visible activity
/// (ST-M009-011): the ids of handled requests, the activity events shed under
/// pressure, and the files that are leaving the computer.
///
/// Activity events ([`SessionEvent::RequestHandled`]) are delivered only while
/// the bounded channel keeps [`LIFECYCLE_RESERVE`] slots free, so a burst of
/// remote actions (for example rapid pointer moves) can never crowd out a stop,
/// revoke, or failure event (REQ-S-004, REQ-N-006). A shed event is never
/// replaced by a fabricated one: it is **counted**, and the count is delivered
/// as one [`SessionEvent::ActivityShed`] at the place in the stream where the
/// events were dropped, as soon as there is room again, so the customer surface
/// shows a gap instead of silently missing items (REQ-N-008). The console's
/// audit still records every routed call in full.
#[derive(Debug, Default)]
struct ActivityTap {
    /// The last minted item id (ids start at 1).
    last_id: u64,
    /// Activity events dropped since the last delivered marker.
    shed: u64,
    /// Files leaving the computer, keyed by the remote path text.
    sending: HashMap<String, Sending>,
    /// Paths whose refused transfer step has been reported since that path's
    /// last reported request, so the in-flight chunks of one refused transfer
    /// yield one item rather than one each.
    refused_steps: HashSet<String>,
    /// Folder removals that have begun and not ended, keyed by the remote
    /// path text (ST-M010-001). Bounded by [`MAX_OPEN_REMOVALS`].
    removing: HashMap<String, Removing>,
}

/// The most folder removals one session keeps open at once in its activity
/// bookkeeping (REQ-N-006). A console removes one folder at a time; one that
/// opened more would have its oldest closed as "result not known".
const MAX_OPEN_REMOVALS: usize = 64;

/// One folder removal that has begun: its first step removed something and
/// said that more remains.
#[derive(Debug)]
struct Removing {
    id: u64,
    attribution: Attribution,
    operation: String,
    path: Option<String>,
    started: Instant,
}

/// What the agent's answer to one step of a folder removal says about the
/// removal (ST-M010-001).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct RemovalStep {
    /// Where the removal stands after the step.
    state: TreeState,
    /// The reason of the first entry the step could not remove.
    first_failure: Option<ErrorCode>,
}

/// One file that has started to leave the customer computer.
#[derive(Debug)]
struct Sending {
    id: u64,
    attribution: Attribution,
    operation: String,
    path: Option<String>,
    started: Instant,
    sent: u64,
}

impl ActivityTap {
    fn mint(&mut self) -> u64 {
        self.last_id += 1;
        self.last_id
    }

    /// Whether a refused transfer step on `path` is the first since that
    /// path's last reported request.
    fn first_refusal(&mut self, path: Option<&str>) -> bool {
        self.refused_steps
            .insert(path.unwrap_or_default().to_owned())
    }

    /// A request on `path` other than a transfer step was reported: a later
    /// refused step on it starts a new item.
    fn settle(&mut self, path: Option<&str>) {
        self.refused_steps.remove(path.unwrap_or_default());
    }

    /// Reports a request the agent refused before serving it (here: one bound
    /// to another session), so the customer sees every refusal, not only the
    /// requests that ran (ST-M009-011, REQ-F-017).
    fn refused(
        &mut self,
        tx: &mpsc::Sender<SessionEvent>,
        attribution: Attribution,
        op: &Operation,
        code: ErrorCode,
    ) {
        self.handled(
            tx,
            attribution,
            ActivityKind::of(op),
            crate::describe_operation(op),
            crate::activity::path_of(op),
            None,
            ActionOutcome::Failed { code },
            Instant::now(),
        );
    }

    /// Delivers the pending shed count as one marker when there is room.
    fn flush(&mut self, tx: &mpsc::Sender<SessionEvent>) {
        if self.shed > 0
            && tx.capacity() > LIFECYCLE_RESERVE
            && tx
                .try_send(SessionEvent::ActivityShed { count: self.shed })
                .is_ok()
        {
            self.shed = 0;
        }
    }

    /// Delivers a pending shed count before the session's terminal event, so
    /// a gap at the end of a session is shown rather than lost with the
    /// session (REQ-N-008). It may use the lifecycle reserve — nothing else is
    /// sent after it — but leaves one slot for the terminal event itself.
    fn flush_final(&mut self, tx: &mpsc::Sender<SessionEvent>) {
        if self.shed > 0
            && tx.capacity() > 1
            && tx
                .try_send(SessionEvent::ActivityShed { count: self.shed })
                .is_ok()
        {
            self.shed = 0;
        }
    }

    /// Emits one activity event while the lifecycle reserve stays free, else
    /// counts it as shed.
    fn emit(&mut self, tx: &mpsc::Sender<SessionEvent>, event: SessionEvent) {
        self.flush(tx);
        if tx.capacity() <= LIFECYCLE_RESERVE || tx.try_send(event).is_err() {
            self.shed += 1;
        }
    }

    /// Emits a job event ([`SessionEvent::CommandStarted`],
    /// [`SessionEvent::ExecFinished`]), which may use the reserve; one that
    /// still cannot be placed is counted as shed rather than lost silently.
    fn emit_job(&mut self, tx: &mpsc::Sender<SessionEvent>, event: SessionEvent) {
        self.flush(tx);
        if tx.try_send(event).is_err() {
            self.shed += 1;
        }
    }

    /// Reports one handled request other than a file read.
    #[allow(clippy::too_many_arguments)]
    fn handled(
        &mut self,
        tx: &mpsc::Sender<SessionEvent>,
        attribution: Attribution,
        kind: ActivityKind,
        operation: String,
        path: Option<String>,
        bytes: Option<u64>,
        outcome: ActionOutcome,
        started: Instant,
    ) {
        let id = self.mint();
        self.emit(
            tx,
            SessionEvent::RequestHandled {
                id,
                origin: attribution.origin,
                purpose: attribution.purpose,
                kind,
                operation,
                path,
                bytes,
                outcome,
                duration_ms: Some(elapsed_ms(started)),
            },
        );
    }

    /// Reports one file read: the first read of a file reports that it starts
    /// to leave the computer (with its size), the read that reaches the end of
    /// the file or fails reports how sending ended (with the bytes sent). The
    /// reads in between are internal transfer steps and are not reported.
    #[allow(clippy::too_many_arguments)]
    fn read(
        &mut self,
        tx: &mpsc::Sender<SessionEvent>,
        attribution: Attribution,
        operation: String,
        path: Option<String>,
        offset: u64,
        outcome: ActionOutcome,
        data: Option<ReadData>,
        started: Instant,
    ) {
        let key = path.clone().unwrap_or_default();
        // A new read from the start while an earlier one is still open means
        // the earlier transfer was abandoned: its end is not known.
        if offset == 0
            && let Some(old) = self.sending.remove(&key)
        {
            self.end_sending(tx, old, ActionOutcome::Unknown);
        }
        match (outcome, data) {
            (ActionOutcome::Succeeded, Some(data)) => {
                if !self.sending.contains_key(&key) {
                    let id = self.mint();
                    self.emit(
                        tx,
                        SessionEvent::RequestHandled {
                            id,
                            origin: attribution.origin,
                            purpose: attribution.purpose.clone(),
                            kind: ActivityKind::FileSent,
                            operation: operation.clone(),
                            path: path.clone(),
                            bytes: Some(data.file_len.saturating_sub(offset)),
                            outcome: ActionOutcome::InProgress,
                            duration_ms: None,
                        },
                    );
                    self.sending.insert(
                        key.clone(),
                        Sending {
                            id,
                            attribution,
                            operation,
                            path,
                            started,
                            sent: 0,
                        },
                    );
                }
                if let Some(open) = self.sending.get_mut(&key) {
                    open.sent = open.sent.saturating_add(data.len);
                }
                if data.eof
                    && let Some(open) = self.sending.remove(&key)
                {
                    self.end_sending(tx, open, ActionOutcome::Succeeded);
                }
            }
            (outcome, _) => match self.sending.remove(&key) {
                Some(open) => self.end_sending(tx, open, outcome),
                // Nothing left the computer: report the refused read once.
                None => self.handled(
                    tx,
                    attribution,
                    ActivityKind::FileSent,
                    operation,
                    path,
                    None,
                    outcome,
                    started,
                ),
            },
        }
    }

    /// Reports one step of a folder removal, and records the removal in the
    /// audit trail (ST-M010-001, REQ-F-017, REQ-S-017).
    ///
    /// A removal is one item for the customer, however many steps it takes:
    /// the first step that leaves more to do opens it as in progress (and
    /// audits `fs_remove_tree_start`), the steps in between are not reported,
    /// and the step that ends it gives the item its final state (and audits
    /// `fs_remove_tree`): done only when the folder is gone, cancelled when
    /// the caller stopped it, failed with the first reason when something
    /// could not be removed or the step was refused. Never done for a folder
    /// that is still there (REQ-N-008).
    #[allow(clippy::too_many_arguments)]
    fn removal(
        &mut self,
        tx: &mpsc::Sender<SessionEvent>,
        audit: &dyn SessionAudit,
        attribution: Attribution,
        operation: String,
        path: Option<String>,
        outcome: ActionOutcome,
        step: Option<RemovalStep>,
        started: Instant,
    ) {
        let key = path.clone().unwrap_or_default();
        let ended = match (outcome, step) {
            (ActionOutcome::Succeeded, Some(step)) => match step.state {
                // More remains and the caller is expected to go on.
                TreeState::More => {
                    if !self.removing.contains_key(&key) {
                        self.begin_removal(tx, audit, key, attribution, operation, path, started);
                    }
                    return;
                }
                TreeState::Removed => ActionOutcome::Succeeded,
                TreeState::Stopped => ActionOutcome::Cancelled,
                TreeState::Incomplete | TreeState::Blocked => ActionOutcome::Failed {
                    code: step.first_failure.unwrap_or(ErrorCode::DirNotEmpty),
                },
            },
            // An answer that is not a removal step says nothing about the
            // folder: the result is not known.
            (ActionOutcome::Succeeded, None) => ActionOutcome::Unknown,
            (other, _) => other,
        };
        audit.record(
            Actor::Console,
            "fs_remove_tree",
            match ended {
                ActionOutcome::Succeeded => Outcome::Ok,
                ActionOutcome::Cancelled => Outcome::Cancelled,
                ActionOutcome::Failed { .. } => Outcome::Failed,
                ActionOutcome::InProgress | ActionOutcome::Unknown => Outcome::Unknown,
            },
        );
        // The step that ends a removal closes the item its first step opened;
        // a removal of one step is opened and closed here.
        let (id, attribution, operation, path, started) = match self.removing.remove(&key) {
            Some(open) => (
                open.id,
                open.attribution,
                open.operation,
                open.path,
                open.started,
            ),
            None => (self.mint(), attribution, operation, path, started),
        };
        self.settle(path.as_deref());
        self.emit(
            tx,
            SessionEvent::RequestHandled {
                id,
                origin: attribution.origin,
                purpose: attribution.purpose,
                kind: ActivityKind::FileOperation,
                operation,
                path,
                bytes: None,
                outcome: ended,
                duration_ms: Some(elapsed_ms(started)),
            },
        );
    }

    /// Opens the item of a folder removal whose first step left more to do.
    #[allow(clippy::too_many_arguments)]
    fn begin_removal(
        &mut self,
        tx: &mpsc::Sender<SessionEvent>,
        audit: &dyn SessionAudit,
        key: String,
        attribution: Attribution,
        operation: String,
        path: Option<String>,
        started: Instant,
    ) {
        // Keep the bookkeeping bounded: the oldest open removal gives way and
        // is closed as "result not known", never as done.
        if self.removing.len() >= MAX_OPEN_REMOVALS
            && let Some(oldest) = self
                .removing
                .iter()
                .min_by_key(|(_, open)| open.started)
                .map(|(key, _)| key.clone())
            && let Some(open) = self.removing.remove(&oldest)
        {
            audit.record(Actor::Console, "fs_remove_tree", Outcome::Unknown);
            self.end_removal(tx, open, ActionOutcome::Unknown);
        }
        let id = self.mint();
        audit.record(Actor::Console, "fs_remove_tree_start", Outcome::Ok);
        self.emit(
            tx,
            SessionEvent::RequestHandled {
                id,
                origin: attribution.origin,
                purpose: attribution.purpose.clone(),
                kind: ActivityKind::FileOperation,
                operation: operation.clone(),
                path: path.clone(),
                bytes: None,
                outcome: ActionOutcome::InProgress,
                duration_ms: None,
            },
        );
        self.removing.insert(
            key,
            Removing {
                id,
                attribution,
                operation,
                path,
                started,
            },
        );
    }

    /// Gives the item of an open folder removal its final state.
    fn end_removal(
        &mut self,
        tx: &mpsc::Sender<SessionEvent>,
        open: Removing,
        outcome: ActionOutcome,
    ) {
        self.emit(
            tx,
            SessionEvent::RequestHandled {
                id: open.id,
                origin: open.attribution.origin,
                purpose: open.attribution.purpose,
                kind: ActivityKind::FileOperation,
                operation: open.operation,
                path: open.path,
                bytes: None,
                outcome,
                duration_ms: Some(elapsed_ms(open.started)),
            },
        );
    }

    /// Records, when the session ends, every folder removal that had begun
    /// and never ended: its result is not known (REQ-N-008). The customer's
    /// own feed marks the item as ended with the session.
    fn close_removals(&mut self, audit: &dyn SessionAudit) {
        for _ in self.removing.drain() {
            audit.record(Actor::Console, "fs_remove_tree", Outcome::Unknown);
        }
    }

    fn end_sending(
        &mut self,
        tx: &mpsc::Sender<SessionEvent>,
        open: Sending,
        outcome: ActionOutcome,
    ) {
        self.emit(
            tx,
            SessionEvent::RequestHandled {
                id: open.id,
                origin: open.attribution.origin,
                purpose: open.attribution.purpose,
                kind: ActivityKind::FileSent,
                operation: open.operation,
                path: open.path,
                bytes: Some(open.sent),
                outcome,
                duration_ms: Some(elapsed_ms(open.started)),
            },
        );
    }
}

/// What one successful file read answered: the bytes read, whether the read
/// reached the end of the file, and the file's length.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ReadData {
    len: u64,
    eof: bool,
    file_len: u64,
}

fn elapsed_ms(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)
}

/// Delivers one streamed output chunk to the console under the bounded event
/// channel's drop-and-mark policy (REQ-N-006).
///
/// The chunk is enqueued only while the channel keeps [`LIFECYCLE_RESERVE`] slots
/// free (so lifecycle/terminal events are never crowded out); under sustained
/// pressure its bytes are shed and counted per job. Nothing here blocks, so the
/// console loop keeps draining the socket and control frames stay responsive. The
/// shed byte count is surfaced as a coalesced [`SessionEvent::ExecOutputDropped`]
/// by [`flush_dropped`] as soon as buffer headroom allows — the operator sees an
/// explicit gap rather than a silent truncation (REQ-N-008).
fn deliver_output(
    tx: &mpsc::Sender<SessionEvent>,
    dropped: &mut BTreeMap<JobId, u64>,
    job: JobId,
    stream: OutputStream,
    bytes: Vec<u8>,
) {
    // Surface any earlier loss for a job before delivering more of its output, so
    // a gap is reported in stream order.
    flush_dropped(tx, dropped);
    let len = bytes.len() as u64;
    if tx.capacity() <= LIFECYCLE_RESERVE
        || tx
            .try_send(SessionEvent::ExecOutput { job, stream, bytes })
            .is_err()
    {
        *dropped.entry(job).or_insert(0) += len;
    }
}

/// Emits a coalesced [`SessionEvent::ExecOutputDropped`] for every job with shed
/// output, clearing each entry once its marker is delivered. A drop marker is
/// itself informational, so it respects [`LIFECYCLE_RESERVE`] exactly as streamed
/// output does — it never spends the headroom kept for terminal/lifecycle events.
/// A marker that cannot be placed yet (no room beyond the reserve, or a gone
/// receiver) is retained and retried on the next call, so the accounting is
/// preserved rather than lost.
fn flush_dropped(tx: &mpsc::Sender<SessionEvent>, dropped: &mut BTreeMap<JobId, u64>) {
    dropped.retain(|job, bytes| {
        if tx.capacity() <= LIFECYCLE_RESERVE {
            return true; // keep: preserve the reserve, retry when room frees
        }
        tx.try_send(SessionEvent::ExecOutputDropped {
            job: *job,
            bytes: *bytes,
        })
        .is_err()
    });
}

/// Sends one control message over the encrypted channel.
async fn send_control<T: AsyncRead + AsyncWrite + Unpin>(
    channel: &mut SecureChannel<T>,
    session: SessionId,
    control: ControlMessage,
) -> Result<(), SecureChannelError> {
    channel.send(&Message::Control { session, control }).await
}

/// One resolved inbound frame on the active channel: a decoded message, a clean
/// disconnect (EOF/I/O), or a channel fault (crypto, decode, oversize, poison).
enum Frame {
    /// A well-formed protocol message.
    Message(Message),
    /// The peer closed the connection.
    Disconnected,
    /// The encrypted channel faulted.
    ChannelFault,
}

/// Receives and classifies one inbound frame on the active channel.
///
/// `SecureChannel::recv` is cancel-safe (its partial-read state lives in the
/// channel), so the active-loop `select!` may race this against other sources and
/// drop the future without losing bytes.
async fn recv_frame<T: AsyncRead + AsyncWrite + Unpin>(channel: &mut SecureChannel<T>) -> Frame {
    match channel.recv().await {
        Ok(message) => Frame::Message(message),
        Err(SecureChannelError::Eof | SecureChannelError::Io(_)) => Frame::Disconnected,
        Err(_) => Frame::ChannelFault,
    }
}

/// Why a frame was not sent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Unsent {
    /// The frame itself cannot be sent: it is larger than one record of the
    /// secure channel, or it breaks a bound of the protocol. It was refused
    /// before anything reached the wire, so the channel is as it was and the
    /// session goes on.
    Refused(Refusal),
    /// The channel failed. It is closed for good.
    Lost,
}

/// What is wrong with a frame that cannot be sent, in the wire vocabulary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Refusal {
    /// The category: [`ErrorCode::OversizedMessage`] for a frame that is too
    /// large, else the category of the bound it breaks.
    code: ErrorCode,
    /// The stage that refused the frame.
    stage: ErrorStage,
}

impl Unsent {
    /// Tells apart why a send on the secure channel failed. The channel checks
    /// a frame against the bounds of the protocol and the size of a record
    /// before it touches the stream, and such a refusal leaves it unpoisoned;
    /// every other failure poisons it for good.
    fn of(error: &SecureChannelError, poisoned: bool) -> Self {
        if poisoned {
            return Self::Lost;
        }
        match error {
            SecureChannelError::Oversized { .. } => Self::Refused(Refusal {
                code: ErrorCode::OversizedMessage,
                stage: ErrorStage::Transport,
            }),
            SecureChannelError::Decode(protocol) => {
                let code = protocol.code();
                let stage = if code == ErrorCode::OversizedMessage {
                    ErrorStage::Transport
                } else {
                    ErrorStage::Validation
                };
                Self::Refused(Refusal { code, stage })
            }
            // A failure on the stream always poisons the channel; one that
            // did not is still not a refusal of the frame.
            SecureChannelError::Io(_)
            | SecureChannelError::Noise(_)
            | SecureChannelError::Eof
            | SecureChannelError::Poisoned => Self::Lost,
        }
    }
}

/// Where the agent writes one outbound frame: the secure channel itself, or a
/// [`ReplyTap`] that also notes how a request was answered (ST-M009-007). Every
/// agent reply helper is generic over it, so the request handler reports each
/// request's honest outcome without threading it through every branch.
trait FrameSink {
    /// Sends one frame, and says whether a frame that was not sent was
    /// refused as it is or lost with the channel.
    async fn send_frame(&mut self, message: &Message) -> Result<(), Unsent>;
}

impl<T: AsyncRead + AsyncWrite + Unpin> FrameSink for SecureChannel<T> {
    async fn send_frame(&mut self, message: &Message) -> Result<(), Unsent> {
        match self.send(message).await {
            Ok(()) => Ok(()),
            Err(error) => Err(Unsent::of(&error, self.is_poisoned())),
        }
    }
}

/// Wraps the agent's channel while one request is serviced and records how the
/// agent answered **that** request — a `Response` (succeeded) or an `Error`
/// (failed, with its code) carrying its id — so the customer surface reports
/// the real result beside the stated purpose (REQ-F-026, REQ-N-008). Other
/// frames (a desktop-activity event) pass through unrecorded. The answer is
/// noted before the send, because the operation already ran even if the reply
/// then fails to reach the console. An answer that could not be sent as it is
/// (ST-M010-005) is followed by the `Error` that fails the request, and that
/// is then the answer noted: it is what the console was told.
struct ReplyTap<'a, C> {
    inner: &'a mut C,
    request: RequestId,
    outcome: Option<ActionOutcome>,
    /// What a successful file read answered, so a file leaving the computer is
    /// reported with its byte count (ST-M009-011).
    read: Option<ReadData>,
    /// What a step of a folder removal answered, so the removal is reported
    /// by where it stands and not by the mere fact that the step was answered
    /// (ST-M010-001).
    removal: Option<RemovalStep>,
}

impl<'a, C: FrameSink> ReplyTap<'a, C> {
    fn new(inner: &'a mut C, request: RequestId) -> Self {
        Self {
            inner,
            request,
            outcome: None,
            read: None,
            removal: None,
        }
    }

    /// The recorded answer, or [`ActionOutcome::Unknown`] when none was sent.
    fn outcome(&self) -> ActionOutcome {
        self.outcome.unwrap_or(ActionOutcome::Unknown)
    }
}

impl<C: FrameSink> FrameSink for ReplyTap<'_, C> {
    async fn send_frame(&mut self, message: &Message) -> Result<(), Unsent> {
        match message {
            Message::Response { id, result, .. } if *id == self.request => {
                self.outcome = Some(ActionOutcome::Succeeded);
                if let OpResult::FileData {
                    data,
                    eof,
                    file_len,
                } = result
                {
                    self.read = Some(ReadData {
                        len: data.len() as u64,
                        eof: *eof,
                        file_len: *file_len,
                    });
                }
                if let OpResult::TreeRemoval { state, failed, .. } = result {
                    self.removal = Some(RemovalStep {
                        state: *state,
                        first_failure: failed.first().map(|failure| failure.code),
                    });
                }
            }
            Message::Error {
                id: Some(id), code, ..
            } if *id == self.request => {
                self.outcome = Some(ActionOutcome::Failed { code: *code });
                // What a response that was not sent said is not what the
                // console was told.
                self.read = None;
                self.removal = None;
            }
            _ => {}
        }
        self.inner.send_frame(message).await
    }
}

/// Sends one already-built message. A caller that sends anything but an answer
/// to a request treats every failure as a terminal disconnect; an answer goes
/// through [`answer`], which tells a refused frame from a lost channel.
async fn send_message<C: FrameSink>(channel: &mut C, message: Message) -> Result<(), Unsent> {
    channel.send_frame(&message).await
}

/// What became of the answer to a request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Answer {
    /// The answer was sent.
    Sent,
    /// The answer could not be sent as it is, and the request was failed with
    /// the reason instead. The session goes on.
    Failed,
    /// The channel is gone.
    Lost,
}

impl Answer {
    /// What the agent's active loop does next.
    fn step(self) -> AgentStep {
        match self {
            Self::Sent | Self::Failed => AgentStep::Continue,
            Self::Lost => AgentStep::Disconnected,
        }
    }
}

/// What the console is told when the answer to its request is larger than one
/// record of the secure channel.
const ANSWER_TOO_LARGE: &str = "the answer is too large to send: it does not fit one message of \
                                the session. The operation itself ran.";

/// What the console is told when the answer to its request breaks a bound of
/// the protocol.
const ANSWER_OUT_OF_BOUNDS: &str = "the answer could not be sent: it breaks a bound of the \
                                    protocol. The operation itself ran.";

/// Answers the request `id` with `result`.
///
/// An answer that cannot be sent never ends the session (ST-M010-005,
/// REQ-N-008). One that is larger than one record of the secure channel, or
/// that breaks a bound of the protocol, is refused before anything of it
/// reaches the wire: the request is then failed with that reason, in fixed
/// words that hold nothing of the answer (REQ-N-015), and the session goes
/// on. Only a channel that is gone is a disconnect.
async fn answer<C: FrameSink>(
    channel: &mut C,
    session: SessionId,
    id: RequestId,
    result: OpResult,
) -> Answer {
    let response = Message::Response {
        id,
        session,
        result,
    };
    let refusal = match send_message(channel, response).await {
        Ok(()) => return Answer::Sent,
        Err(Unsent::Lost) => return Answer::Lost,
        Err(Unsent::Refused(refusal)) => refusal,
    };
    let reason = if refusal.code == ErrorCode::OversizedMessage {
        ANSWER_TOO_LARGE
    } else {
        ANSWER_OUT_OF_BOUNDS
    };
    match send_error(
        channel,
        Some(id),
        Some(session),
        refusal.code,
        reason,
        refusal.stage,
    )
    .await
    {
        Err(Unsent::Lost) => Answer::Lost,
        // The error is a few fixed words: nothing refuses it.
        Ok(()) | Err(Unsent::Refused(_)) => Answer::Failed,
    }
}

/// Sends a secret-free wire `Error` for a rejected request (bad session, unknown
/// job, unsupported op, failed spawn). Never echoes remote command text
/// (REQ-N-015).
async fn send_error<C: FrameSink>(
    channel: &mut C,
    id: Option<RequestId>,
    session: Option<SessionId>,
    code: ErrorCode,
    message: &str,
    stage: ErrorStage,
) -> Result<(), Unsent> {
    send_message(
        channel,
        Message::Error {
            id,
            session,
            code,
            message: message.to_string(),
            stage,
        },
    )
    .await
}

/// Maps an honest [`JobOutcome`] to the audit [`Outcome`] vocabulary. An unknown
/// outcome is audited as [`Outcome::Unknown`], never as success (REQ-N-008).
fn outcome_to_audit(outcome: JobOutcome) -> Outcome {
    match outcome {
        JobOutcome::Completed => Outcome::Ok,
        JobOutcome::Failed => Outcome::Failed,
        JobOutcome::Cancelled => Outcome::Cancelled,
        JobOutcome::TimedOut => Outcome::Timeout,
        JobOutcome::Unknown => Outcome::Unknown,
    }
}

/// Fails every outstanding console request with a fresh error, so a torn-down
/// session never leaves a caller awaiting a reply that cannot come (fail closed).
///
/// Every caller first takes control away from the session
/// (`set_control_active(false)`): a caller whose request fails here may ask
/// again at once, before the teardown has run, and that request must be
/// refused as "not active", never sent into a session that is ending and
/// then lost with it.
fn fail_pending(
    pending: &mut HashMap<RequestId, oneshot::Sender<Result<OpResult, SessionError>>>,
    make: impl Fn() -> SessionError,
) {
    for (_id, reply) in pending.drain() {
        let _ = reply.send(Err(make()));
    }
}

/// Emits an honest terminal [`SessionEvent::ExecFinished`] with an
/// [`JobOutcome::Unknown`] outcome for every job still believed running when the
/// session tears down — never a fabricated success (REQ-N-008).
fn finish_running_unknown(running: &BTreeSet<JobId>, tx: &mpsc::Sender<SessionEvent>) {
    for job in running {
        emit(
            tx,
            SessionEvent::ExecFinished {
                job: *job,
                exit_code: None,
                outcome: JobOutcome::Unknown,
            },
        );
    }
}

/// Receives and classifies one inbound frame.
async fn recv_control<T: AsyncRead + AsyncWrite + Unpin>(
    channel: &mut SecureChannel<T>,
) -> Incoming {
    match channel.recv().await {
        Ok(Message::Control { session, control }) => Incoming::Control(session, control),
        Ok(_) => Incoming::NonControl,
        Err(SecureChannelError::Eof | SecureChannelError::Io(_)) => Incoming::Disconnected,
        Err(_) => Incoming::ChannelFault,
    }
}

/// A file-cleanup contribution folded into a session teardown report. On the
/// way out the agent sweeps its staging area (see [`agent_active_finish`]); a
/// staged artifact that could not be removed turns the teardown report red and
/// is surfaced as a prominent, secret-free note (REQ-S-012, REQ-F-019).
struct FsCleanup {
    /// Whether every selected staged artifact was removed (or was already gone).
    ok: bool,
    /// Secret-free notes naming any residual that must be retried at next start.
    notes: Vec<String>,
}

impl FsCleanup {
    /// The "nothing to fold" contribution used by every non-agent teardown
    /// path, which owns no staging area.
    fn none() -> Self {
        Self {
            ok: true,
            notes: Vec::new(),
        }
    }
}

/// Tears the session down fail-closed: drops the channel (invalidating the
/// ephemeral keys), records an honest cleanup report, emits the terminal event,
/// and audits the outcome (REQ-S-007, REQ-S-009, REQ-F-019).
fn finish<T: AsyncRead + AsyncWrite + Unpin>(
    channel: SecureChannel<T>,
    shared: &Shared,
    tx: &mpsc::Sender<SessionEvent>,
    audit: &dyn SessionAudit,
    actor: Actor,
    ending: Ending,
) {
    finish_inner(channel, shared, tx, audit, actor, ending, FsCleanup::none());
}

/// The teardown body shared by every terminal path. `fs` folds an agent-side
/// staging-cleanup outcome into the report; non-agent paths pass
/// [`FsCleanup::none`]. A file residual flips the report to not-ok so it is
/// surfaced prominently and audited as a failed cleanup (never as success).
fn finish_inner<T: AsyncRead + AsyncWrite + Unpin>(
    channel: SecureChannel<T>,
    shared: &Shared,
    tx: &mpsc::Sender<SessionEvent>,
    audit: &dyn SessionAudit,
    actor: Actor,
    ending: Ending,
    fs: FsCleanup,
) {
    let report = teardown_report(ending, fs);
    finish_with_report(channel, shared, tx, audit, actor, ending, report);
}

/// This side's honest teardown report: the ending's note plus any staging
/// residuals.
fn teardown_report(ending: Ending, fs: FsCleanup) -> CleanupReport {
    let (_, note, _, _) = describe_ending(ending);
    let mut report = CleanupReport::clean(note);
    report.ok &= fs.ok;
    report.notes.extend(fs.notes);
    report
}

/// Drops the channel, publishes the report and who ended the session, emits the
/// terminal event, and audits the outcome followed by the end record
/// (ST-M009-012). Synchronous from the emit on, so the end record is written
/// even when the caller drops the handle the moment it sees the terminal event.
fn finish_with_report<T: AsyncRead + AsyncWrite + Unpin>(
    channel: SecureChannel<T>,
    shared: &Shared,
    tx: &mpsc::Sender<SessionEvent>,
    audit: &dyn SessionAudit,
    actor: Actor,
    ending: Ending,
    report: CleanupReport,
) {
    // Explicit drop: releasing the `SecureChannel` invalidates the Noise
    // session keys and closes the stream. The session is single-use.
    drop(channel);
    let control_granted = shared.control_granted();
    shared.set_control_active(false);
    let ended_by = ended_by(ending);
    shared.set_ended_by(ended_by);

    let (event, _, action, outcome) = describe_ending(ending);
    let cleanup_outcome = if report.ok {
        Outcome::Ok
    } else {
        Outcome::Failed
    };
    shared.set_cleanup(report);
    emit(tx, event);
    audit.record(actor, action, outcome);
    audit.record(Actor::System, "cleanup", cleanup_outcome);
    audit.record_end(&SessionEnd {
        ended_by,
        action,
        control_granted,
        peer_cleanup: shared.peer_cleanup(),
    });
}

/// Who ended the session, from this side's point of view (ST-M009-012). A
/// revocation names its requester; a declined consent is the customer's
/// decision and a code mismatch the technician's; a lost peer is a lost
/// connection; a fault, a timeout or a protocol violation is a failure.
fn ended_by(ending: Ending) -> EndedBy {
    match ending {
        Ending::LocalRevoke(reason) | Ending::PeerRevoke(reason) => EndedBy::of_revoke(reason),
        Ending::ActiveDisconnect | Ending::EarlyDisconnect(_) => EndedBy::ConnectionLost,
        Ending::ConsentDenied => EndedBy::Customer,
        Ending::CompareMismatch => EndedBy::Technician,
        Ending::ChannelFault(_) | Ending::Timeout(_) | Ending::Protocol(_) => EndedBy::Failure,
    }
}

/// The agent-side terminal path. Before finishing, the agent autonomously
/// sweeps its own staging area with the `SessionEnd` trigger so staged
/// artifacts never outlive the session — this runs on every exit reason
/// (disconnect, revoke, protocol fault), so it holds even when the console is
/// already gone (ST-M003-005, REQ-N-008, REQ-F-019). The honest sweep outcome
/// is folded into the teardown report, which the agent then sends to the
/// console while the channel is still up (protocol minor 11, ST-M009-012) so
/// the technician's audit records what was left behind.
#[allow(clippy::too_many_arguments)]
async fn agent_active_finish<T: AsyncRead + AsyncWrite + Unpin>(
    mut channel: SecureChannel<T>,
    session: SessionId,
    file_store: &FileStore,
    shared: &Shared,
    tx: &mpsc::Sender<SessionEvent>,
    activity: &mut ActivityTap,
    audit: &dyn SessionAudit,
    actor: Actor,
    ending: Ending,
) {
    activity.close_removals(audit);
    let fs = sweep_staging_on_exit(file_store, audit).await;
    let report = teardown_report(ending, fs);
    // A lost or faulted channel cannot carry the report; the console then
    // records the cleanup as unknown, never as success.
    if !matches!(ending, Ending::ActiveDisconnect | Ending::ChannelFault(_)) {
        send_cleanup_report(&mut channel, session, &report).await;
    }
    // Activity shed since the last marker is shown before the end.
    activity.flush_final(tx);
    finish_with_report(channel, shared, tx, audit, actor, ending, report);
}

/// Sends the agent's teardown report to the console, fitted to the wire bounds
/// and bounded in time; a failure only means the console will not have it.
async fn send_cleanup_report<T: AsyncRead + AsyncWrite + Unpin>(
    channel: &mut SecureChannel<T>,
    session: SessionId,
    report: &CleanupReport,
) {
    if channel.is_poisoned() {
        return;
    }
    let fitted = fit_for_wire(report);
    let _ = timeout(
        TEARDOWN_DRAIN,
        send_control(
            channel,
            session,
            ControlMessage::CleanupReported {
                ok: fitted.ok,
                notes: fitted.notes,
            },
        ),
    )
    .await;
}

/// Runs the session-end staging sweep and reports an honest, secret-free
/// outcome. A removal that fails is never reported as success: the agent keeps
/// the manifest record for a later retry, surfaces the residual as a report
/// note, and audits an `fs_cleanup` failure (REQ-N-008).
async fn sweep_staging_on_exit(file_store: &FileStore, audit: &dyn SessionAudit) -> FsCleanup {
    match file_store.cleanup(CleanupTrigger::SessionEnd).await {
        Ok(OpResult::CleanupReport { failed, .. }) if failed.is_empty() => {
            audit.record(Actor::System, "fs_cleanup", Outcome::Ok);
            FsCleanup::none()
        }
        Ok(OpResult::CleanupReport { failed, .. }) => {
            audit.record(Actor::System, "fs_cleanup", Outcome::Failed);
            let notes = failed
                .into_iter()
                .map(|f| format!("{RESIDUAL_NOTE_PREFIX}{} ({})", f.canonical_path, f.reason))
                .collect();
            FsCleanup { ok: false, notes }
        }
        // `cleanup` only ever yields a `CleanupReport`; any other shape is a
        // contract violation, treated as an unknown (never-success) outcome.
        Ok(_) => {
            audit.record(Actor::System, "fs_cleanup", Outcome::Unknown);
            FsCleanup {
                ok: false,
                notes: vec![format!(
                    "{TEARDOWN_FAILED_NOTE_PREFIX}the staging cleanup returned an unexpected result"
                )],
            }
        }
        Err(error) => {
            audit.record(Actor::System, "fs_cleanup", Outcome::Failed);
            // A cleanup that fails as a whole has removed the parts of
            // unfinished transfers to a folder first. The ones it could not
            // remove are named beside its failure.
            let mut notes = vec![format!("{TEARDOWN_FAILED_NOTE_PREFIX}{}", error.message)];
            notes.extend(file_store.unfinished_parts().into_iter().map(|part| {
                format!(
                    "{RESIDUAL_NOTE_PREFIX}{part} (the part of an unfinished transfer; not removed)"
                )
            }));
            FsCleanup { ok: false, notes }
        }
    }
}

/// A terminal outcome for a failure that happened before any secure channel
/// existed (an accept or pairing failure).
struct EarlyFailure {
    /// The stage that failed (for example `"connect"`, `"pairing"`).
    stage: &'static str,
    /// A plain-language, secret-free description.
    message: &'static str,
    /// The audit action label.
    action: &'static str,
    /// The audit outcome.
    outcome: Outcome,
}

/// How a failed pairing is reported. Over a relay, a stream that ends before
/// the peer was heard is no failed handshake: nobody joined the rendezvous
/// ([`crate::dial::rendezvous_lapsed`], ST-M007-012). It gets a stage of its
/// own, so that a front end says "nobody is waiting on this code" instead of a
/// pairing failure of unknown cause (REQ-N-015).
fn pairing_failure(error: &cretlink_transport::PairingError) -> EarlyFailure {
    if crate::dial::rendezvous_lapsed(error) {
        EarlyFailure {
            stage: "rendezvous",
            message: "nobody joined the rendezvous on the relay",
            action: "error",
            outcome: Outcome::Timeout,
        }
    } else {
        EarlyFailure {
            stage: "pairing",
            message: "secure pairing failed",
            action: "error",
            outcome: Outcome::Failed,
        }
    }
}

/// Locally revokes the session: sends the `Revoke`, then keeps the connection
/// open and drains until the peer reads it and closes, so the peer reports the
/// honest revoke reason rather than a bare disconnect (a reset would otherwise
/// wipe the Revoke from the peer's receive buffer). Bounded by [`TEARDOWN_DRAIN`].
///
/// On the console side the drain also keeps the agent's teardown report, which
/// the agent sends before it closes (protocol minor 11, ST-M009-012); the
/// console waits up to [`CLEANUP_REPORT_WAIT`] for it, ending as soon as it
/// arrives or the agent closes.
async fn finish_local_revoke<T: AsyncRead + AsyncWrite + Unpin>(
    mut channel: SecureChannel<T>,
    session: SessionId,
    reason: RevokeReason,
    shared: &Shared,
    tx: &mpsc::Sender<SessionEvent>,
    audit: &dyn SessionAudit,
    actor: Actor,
) {
    let _ = send_control(&mut channel, session, ControlMessage::Revoke { reason }).await;
    if actor == Actor::Console {
        let _ = timeout(
            CLEANUP_REPORT_WAIT,
            drain_keeping_cleanup_report(&mut channel, session, shared),
        )
        .await;
    } else {
        let _ = timeout(TEARDOWN_DRAIN, drain_until_closed(&mut channel)).await;
    }
    finish(
        channel,
        shared,
        tx,
        audit,
        actor,
        Ending::LocalRevoke(reason),
    );
}

/// Tears down after the technician declared, at the compare gate, that the
/// Session Approval Codes do not match: revoke the peer over the wire (so the
/// customer's screen also fails closed rather than hanging), drain like the
/// revoke path so the peer reads the `Revoke` before the connection closes, then
/// finish with an honest compare-mismatch failure that grants no control
/// (REQ-S-006, REQ-S-009, ST-M002-013).
async fn finish_compare_mismatch<T: AsyncRead + AsyncWrite + Unpin>(
    mut channel: SecureChannel<T>,
    session: SessionId,
    shared: &Shared,
    tx: &mpsc::Sender<SessionEvent>,
    audit: &dyn SessionAudit,
    actor: Actor,
) {
    let _ = send_control(
        &mut channel,
        session,
        ControlMessage::Revoke {
            reason: RevokeReason::TechnicianRequested,
        },
    )
    .await;
    let _ = timeout(TEARDOWN_DRAIN, drain_until_closed(&mut channel)).await;
    finish(channel, shared, tx, audit, actor, Ending::CompareMismatch);
}

/// Reads and discards frames until the peer closes or the channel faults.
async fn drain_until_closed<T: AsyncRead + AsyncWrite + Unpin>(channel: &mut SecureChannel<T>) {
    while channel.recv().await.is_ok() {}
}

/// Like [`drain_until_closed`], but keeps the agent's teardown report for this
/// session and stops at it: it is the last frame the agent sends before it
/// closes (console side, ST-M009-012).
async fn drain_keeping_cleanup_report<T: AsyncRead + AsyncWrite + Unpin>(
    channel: &mut SecureChannel<T>,
    session: SessionId,
    shared: &Shared,
) {
    while let Ok(message) = channel.recv().await {
        if let Message::Control {
            session: stamped,
            control: ControlMessage::CleanupReported { ok, notes },
        } = message
            && stamped == session
        {
            shared.set_peer_cleanup(CleanupReport { ok, notes });
            return;
        }
    }
}

/// Fails the session before any channel exists (a pairing/accept failure), still
/// producing an honest cleanup report and terminal event.
fn fail_before_channel(
    shared: &Shared,
    tx: &mpsc::Sender<SessionEvent>,
    audit: &dyn SessionAudit,
    actor: Actor,
    failure: EarlyFailure,
) {
    shared.set_control_active(false);
    shared.set_ended_by(EndedBy::Failure);
    shared.set_cleanup(CleanupReport::clean(
        "no secure session was established; nothing to tear down",
    ));
    emit(
        tx,
        SessionEvent::Failed {
            stage: failure.stage.to_string(),
            message: failure.message.to_string(),
        },
    );
    audit.record(actor, failure.action, failure.outcome);
    audit.record(Actor::System, "cleanup", Outcome::Ok);
    audit.record_end(&SessionEnd {
        ended_by: EndedBy::Failure,
        action: failure.action,
        control_granted: false,
        peer_cleanup: None,
    });
}

/// Maps an [`Ending`] to its terminal event, cleanup note, audit action, and
/// outcome. Every mapping is honest: a disconnect or fault is never reported as
/// success (REQ-N-008).
fn describe_ending(ending: Ending) -> (SessionEvent, &'static str, &'static str, Outcome) {
    match ending {
        Ending::LocalRevoke(reason) => (
            SessionEvent::Revoked { reason },
            "session revoked locally; secure channel and session keys dropped",
            "revoked",
            Outcome::Ok,
        ),
        Ending::PeerRevoke(reason) => (
            SessionEvent::Revoked { reason },
            "session revoked by the peer; secure channel and session keys dropped",
            "revoked",
            Outcome::Ok,
        ),
        Ending::ActiveDisconnect => (
            SessionEvent::Revoked {
                reason: RevokeReason::Error,
            },
            "peer disconnected during the active session; outcome unknown",
            "disconnected",
            Outcome::Unknown,
        ),
        Ending::EarlyDisconnect(stage) => (
            SessionEvent::Failed {
                stage: stage.to_string(),
                message: "peer disconnected".to_string(),
            },
            "peer disconnected before the session became active",
            "disconnected",
            Outcome::Unknown,
        ),
        Ending::ChannelFault(stage) => (
            SessionEvent::Failed {
                stage: stage.to_string(),
                message: "secure channel failed".to_string(),
            },
            "secure channel fault; channel and session keys dropped",
            "error",
            Outcome::Failed,
        ),
        Ending::ConsentDenied => (
            SessionEvent::Failed {
                stage: "consent".to_string(),
                message: "full-control consent was denied".to_string(),
            },
            "consent denied; no control was granted",
            "consent_denied",
            Outcome::Ok,
        ),
        Ending::Timeout(stage) => (
            SessionEvent::Failed {
                stage: stage.to_string(),
                message: "the session timed out".to_string(),
            },
            "session stage timed out; channel and session keys dropped",
            "error",
            Outcome::Timeout,
        ),
        Ending::Protocol(stage) => (
            SessionEvent::Failed {
                stage: stage.to_string(),
                message: "unexpected control message".to_string(),
            },
            "protocol violation; channel and session keys dropped",
            "error",
            Outcome::Failed,
        ),
        Ending::CompareMismatch => (
            SessionEvent::Failed {
                stage: "compare".to_string(),
                message: "the Session Approval Codes did not match".to_string(),
            },
            "approval-code comparison failed; channel and session keys dropped",
            "compare_mismatch",
            Outcome::Failed,
        ),
    }
}

/// Awaits one consent-phase step: an inbound frame, a local revoke, or an
/// exceptional condition, whichever comes first.
///
/// Cancellation is safe here: the `recv` future is only dropped when the local
/// revoke wins, after which the caller tears the channel down and never reads
/// it again.
async fn next_step<T: AsyncRead + AsyncWrite + Unpin>(
    channel: &mut SecureChannel<T>,
    revoke_rx: &mut UnboundedReceiver<RevokeReason>,
    default_reason: RevokeReason,
    budget: Duration,
) -> Step {
    tokio::select! {
        biased;
        cmd = revoke_rx.recv() => Step::LocalRevoke(cmd.unwrap_or(default_reason)),
        res = timeout(budget, recv_control(channel)) => match res {
            Ok(Incoming::Control(session, control)) => Step::Control(session, control),
            Ok(Incoming::Disconnected) => Step::Disconnected,
            Ok(Incoming::ChannelFault) => Step::ChannelFault,
            Ok(Incoming::NonControl) => Step::NonControl,
            Err(_elapsed) => Step::TimedOut,
        },
    }
}

/// One resolved step of the console-side compare gate: the technician's verdict
/// on the approval-code comparison, an inbound frame, a local revoke, or an
/// exceptional condition (REQ-S-006, ST-M002-013).
enum CompareStep {
    /// The technician confirmed the codes match on both screens.
    Confirmed,
    /// The technician declared the codes do not match.
    Mismatch,
    /// A local revoke/stop command arrived (carrying its reason).
    LocalRevoke(RevokeReason),
    /// A well-formed control frame arrived (only a peer `Revoke` is in contract
    /// here; anything else is a protocol violation). The session id is not carried
    /// because the console already adopted it from `ConsentRequested` in Phase 1.
    Control(ControlMessage),
    /// The peer disconnected.
    Disconnected,
    /// The encrypted channel faulted.
    ChannelFault,
    /// A non-control frame arrived (protocol violation).
    NonControl,
    /// The compare window exceeded its budget.
    TimedOut,
}

/// Awaits one step of the compare gate, whichever comes first. Biased so a local
/// revoke is honored ahead of any confirmation and the gate always fails closed
/// under contention (REQ-S-009).
///
/// Cancellation is safe here for the same reason as [`next_step`]: the losing
/// `recv` futures are only dropped once a terminal step wins, after which the
/// caller either sends the confirmation and reads on or tears the channel down.
async fn next_compare_step<T: AsyncRead + AsyncWrite + Unpin>(
    channel: &mut SecureChannel<T>,
    confirm_rx: &mut UnboundedReceiver<CompareDecision>,
    revoke_rx: &mut UnboundedReceiver<RevokeReason>,
    budget: Duration,
) -> CompareStep {
    tokio::select! {
        biased;
        cmd = revoke_rx.recv() => {
            CompareStep::LocalRevoke(cmd.unwrap_or(RevokeReason::TechnicianRequested))
        }
        decision = confirm_rx.recv() => match decision {
            Some(CompareDecision::Match) => CompareStep::Confirmed,
            Some(CompareDecision::Mismatch) => CompareStep::Mismatch,
            // The confirm sender was dropped without a verdict: fail closed as a
            // technician-side revoke rather than silently releasing the gate.
            None => CompareStep::LocalRevoke(RevokeReason::TechnicianRequested),
        },
        res = timeout(budget, recv_control(channel)) => match res {
            Ok(Incoming::Control(_session, control)) => CompareStep::Control(control),
            Ok(Incoming::Disconnected) => CompareStep::Disconnected,
            Ok(Incoming::ChannelFault) => CompareStep::ChannelFault,
            Ok(Incoming::NonControl) => CompareStep::NonControl,
            Err(_elapsed) => CompareStep::TimedOut,
        },
    }
}

/// The console-side session task (ST-M002-005/006): accept exactly one
/// connection from a LAN listener, then hand the stream to the stream-generic
/// body [`host_task_over_stream`]. Kept as a thin wrapper so the LAN listener
/// path and the relay-dial path (ST-M007-008, ADR-0016 §5) share one session
/// body verbatim.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn host_task(
    listener: TcpListener,
    version: ProtocolVersion,
    // The console's served capability set — forwarded verbatim to the
    // stream-generic body (see its parameter doc for why it is unused at runtime).
    capabilities: Vec<String>,
    shared: Arc<Shared>,
    tx: mpsc::Sender<SessionEvent>,
    mut revoke_rx: UnboundedReceiver<RevokeReason>,
    confirm_rx: UnboundedReceiver<CompareDecision>,
    command_rx: UnboundedReceiver<HostCommand>,
    audit: Arc<dyn SessionAudit>,
) {
    // Accept exactly one connection (single session), honoring an early stop.
    // `audit.as_ref()` is borrowed inline at each fail site so the owning `Arc`
    // stays free to move into `host_task_over_stream` on the success path.
    let stream = tokio::select! {
        biased;
        _ = revoke_rx.recv() => {
            return fail_before_channel(&shared, &tx, audit.as_ref(), Actor::Console, EarlyFailure {
                stage: "connect",
                message: "session stopped before a connection",
                action: "revoked",
                outcome: Outcome::Cancelled,
            });
        }
        res = timeout(ACCEPT_TIMEOUT, listener.accept()) => match res {
            Ok(Ok((stream, _peer))) => stream,
            Ok(Err(_)) => return fail_before_channel(&shared, &tx, audit.as_ref(), Actor::Console, EarlyFailure {
                stage: "connect",
                message: "failed to accept a connection",
                action: "error",
                outcome: Outcome::Failed,
            }),
            Err(_elapsed) => return fail_before_channel(&shared, &tx, audit.as_ref(), Actor::Console, EarlyFailure {
                stage: "connect",
                message: "timed out waiting for a connection",
                action: "error",
                outcome: Outcome::Timeout,
            }),
        },
    };
    // Only one connection is accepted; free the listener so the port cannot be
    // reused to resume this session.
    drop(listener);

    // Run the rest of the session over the accepted stream.
    host_task_over_stream(
        stream,
        version,
        capabilities,
        shared,
        tx,
        revoke_rx,
        confirm_rx,
        command_rx,
        audit,
    )
    .await;
}

/// The stream-generic console-side session body: pair as the Noise responder
/// over an already-connected `stream`, await the agent's consent exchange, then
/// hold or tear down the active session. `host_task` supplies a stream from a
/// LAN listener; [`SessionHost::host_over_stream`](crate::SessionHost) supplies
/// one from a relay dial (ST-M007-008, ADR-0016 §5) — the session logic below is
/// byte-identical either way. The console never sends an identity — it receives
/// the honest [`TargetIdentity`] in [`ControlMessage::ConsentGranted`].
#[allow(clippy::too_many_arguments)]
pub(crate) async fn host_task_over_stream<S>(
    stream: S,
    version: ProtocolVersion,
    // The console's served capability set. It is intentionally unused at runtime:
    // the console serves no operations, the prologue now binds the
    // version-canonical set (not this one), and per-host availability is enforced
    // by the agent, not advertised to the console. The parameter is kept for
    // symmetry with `join_task`/`SessionJoiner` and so a future console minor that
    // serves operations has a place to thread its set from (ST-M002-008).
    _capabilities: Vec<String>,
    shared: Arc<Shared>,
    tx: mpsc::Sender<SessionEvent>,
    mut revoke_rx: UnboundedReceiver<RevokeReason>,
    mut confirm_rx: UnboundedReceiver<CompareDecision>,
    mut command_rx: UnboundedReceiver<HostCommand>,
    audit: Arc<dyn SessionAudit>,
) where
    // Generic over the transport stream so the console pairs identically whether
    // the peer arrived on the LAN listener or through the relay splice.
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let audit = audit.as_ref();

    // The prologue binds the **version-canonical** capability set, a pure
    // function of `version` that both peers compute identically. The console
    // serves no operations while the agent serves a host-dependent subset;
    // hashing either *served* set here would make two honest peers present
    // different prologues and never pair. Per-host availability is never carried
    // over this channel to the console — the agent enforces it per operation
    // (answering `Unsupported`) from its own served set (ST-M002-008, REQ-S-008).
    let prologue_caps = canonical_capabilities(version);

    // Pair as the Noise responder, but keep watching for a local stop/revoke so
    // it is acknowledged promptly instead of waiting out PAIR_TIMEOUT. Losing the
    // pairing arm to a revoke drops the in-flight handshake (and its stream),
    // tearing down fail-closed before any channel or key exists.
    let pairing_result = tokio::select! {
        biased;
        _ = revoke_rx.recv() => {
            return fail_before_channel(&shared, &tx, audit, Actor::Console, EarlyFailure {
                stage: "pairing",
                message: "session stopped during pairing",
                action: "revoked",
                outcome: Outcome::Cancelled,
            });
        }
        res = timeout(
            PAIR_TIMEOUT,
            Pairing::perform_with_display_name(stream, Role::Responder, version, &prologue_caps, shared.own_display_name()),
        ) => res,
    };
    let mut channel = match pairing_result {
        Ok(Ok(paired)) => {
            shared.set_approval_code(paired.approval_code);
            paired.channel
        }
        Ok(Err(error)) => {
            return fail_before_channel(
                &shared,
                &tx,
                audit,
                Actor::Console,
                pairing_failure(&error),
            );
        }
        Err(_elapsed) => {
            return fail_before_channel(
                &shared,
                &tx,
                audit,
                Actor::Console,
                EarlyFailure {
                    stage: "pairing",
                    message: "pairing timed out",
                    action: "error",
                    outcome: Outcome::Timeout,
                },
            );
        }
    };
    emit(&tx, SessionEvent::Connected);
    audit.record(Actor::Console, "pairing_completed", Outcome::Ok);

    // Phase 1: adopt the session from the agent's ConsentRequested. Any other
    // frame here is terminal and fails closed.
    let session = match next_step(
        &mut channel,
        &mut revoke_rx,
        RevokeReason::TechnicianRequested,
        CONSENT_TIMEOUT,
    )
    .await
    {
        Step::Control(session, ControlMessage::ConsentRequested { disclosure }) => {
            // Keep the exact text the customer was shown so the console can
            // present it verbatim (REQ-S-003, REQ-S-016). Adopt the agent's
            // session id so every later request/cancel — and the compare
            // confirmation below — is bound to it.
            shared.set_session(session);
            shared.set_disclosure(DisclosureSummary::new(disclosure));
            // The Session Approval Code is already displayed on both endpoints
            // after pairing; pause here for the humanly-perceptible compare
            // window rather than jumping straight to the consent prompt
            // (REQ-S-006, ST-M002-013). The customer is not asked to grant
            // anything until the technician confirms the codes match.
            emit(&tx, SessionEvent::AwaitingApprovalCompare);
            audit.record(Actor::Agent, "consent_requested", Outcome::Ok);
            session
        }
        Step::Control(_, ControlMessage::Revoke { reason }) => {
            return finish(
                channel,
                &shared,
                &tx,
                audit,
                Actor::Agent,
                Ending::PeerRevoke(reason),
            );
        }
        Step::Control(_, _) | Step::NonControl => {
            return finish(
                channel,
                &shared,
                &tx,
                audit,
                Actor::Console,
                Ending::Protocol("consent"),
            );
        }
        // No session id is known yet, so no well-formed Revoke can be sent;
        // dropping the channel closes the connection and the peer observes it.
        Step::LocalRevoke(reason) => {
            return finish(
                channel,
                &shared,
                &tx,
                audit,
                Actor::Console,
                Ending::LocalRevoke(reason),
            );
        }
        Step::Disconnected => {
            return finish(
                channel,
                &shared,
                &tx,
                audit,
                Actor::Console,
                Ending::EarlyDisconnect("consent"),
            );
        }
        Step::ChannelFault => {
            return finish(
                channel,
                &shared,
                &tx,
                audit,
                Actor::Console,
                Ending::ChannelFault("consent"),
            );
        }
        Step::TimedOut => {
            return finish(
                channel,
                &shared,
                &tx,
                audit,
                Actor::Console,
                Ending::Timeout("consent"),
            );
        }
    };

    // Phase 1.5: the compare gate. Hold here — with the approval code on screen —
    // until the technician confirms over the wire that it matches the customer's
    // code, before the customer is ever asked to grant control (REQ-S-006,
    // ST-M002-013). Confirming sends `ApprovalCompareConfirmed`, which releases
    // the agent's matching wait; a mismatch, revoke, disconnect, fault, or
    // timeout here grants no control and fails the session closed (REQ-S-009).
    match next_compare_step(
        &mut channel,
        &mut confirm_rx,
        &mut revoke_rx,
        CONSENT_TIMEOUT,
    )
    .await
    {
        CompareStep::Confirmed => {
            if send_control(
                &mut channel,
                session,
                ControlMessage::ApprovalCompareConfirmed {},
            )
            .await
            .is_err()
            {
                return finish(
                    channel,
                    &shared,
                    &tx,
                    audit,
                    Actor::Console,
                    Ending::EarlyDisconnect("compare"),
                );
            }
            // Only now is the customer asked to grant control.
            emit(&tx, SessionEvent::AwaitingConsent);
            audit.record(Actor::Console, "approval_compare_confirmed", Outcome::Ok);
        }
        CompareStep::Mismatch => {
            return finish_compare_mismatch(channel, session, &shared, &tx, audit, Actor::Console)
                .await;
        }
        CompareStep::LocalRevoke(reason) => {
            return finish_local_revoke(
                channel,
                session,
                reason,
                &shared,
                &tx,
                audit,
                Actor::Console,
            )
            .await;
        }
        CompareStep::Control(ControlMessage::Revoke { reason }) => {
            return finish(
                channel,
                &shared,
                &tx,
                audit,
                Actor::Agent,
                Ending::PeerRevoke(reason),
            );
        }
        CompareStep::Control(_) | CompareStep::NonControl => {
            return finish(
                channel,
                &shared,
                &tx,
                audit,
                Actor::Console,
                Ending::Protocol("compare"),
            );
        }
        CompareStep::Disconnected => {
            return finish(
                channel,
                &shared,
                &tx,
                audit,
                Actor::Console,
                Ending::EarlyDisconnect("compare"),
            );
        }
        CompareStep::ChannelFault => {
            return finish(
                channel,
                &shared,
                &tx,
                audit,
                Actor::Console,
                Ending::ChannelFault("compare"),
            );
        }
        CompareStep::TimedOut => {
            return finish(
                channel,
                &shared,
                &tx,
                audit,
                Actor::Console,
                Ending::Timeout("compare"),
            );
        }
    }

    // Phase 2: await the customer's decision, bound to the adopted session. Each
    // outcome is terminal (grant transitions into the active loop).
    match next_step(
        &mut channel,
        &mut revoke_rx,
        RevokeReason::TechnicianRequested,
        CONSENT_TIMEOUT,
    )
    .await
    {
        Step::Control(s, ControlMessage::ConsentGranted { target }) if s == session => {
            shared.set_control_active(true);
            audit.record(Actor::Agent, "consent_granted", Outcome::Ok);
            // The honest identity this session controls, for the end report
            // (ST-M009-012, REQ-F-016).
            audit.record_target(&target);
            emit(&tx, SessionEvent::ConsentGranted { identity: target });
            emit(&tx, SessionEvent::Active);
            host_active(
                channel,
                session,
                &shared,
                &tx,
                &mut revoke_rx,
                &mut command_rx,
                audit,
            )
            .await;
        }
        Step::Control(_, ControlMessage::ConsentDenied { .. }) => {
            finish(
                channel,
                &shared,
                &tx,
                audit,
                Actor::Agent,
                Ending::ConsentDenied,
            );
        }
        Step::Control(_, ControlMessage::Revoke { reason }) => {
            finish(
                channel,
                &shared,
                &tx,
                audit,
                Actor::Agent,
                Ending::PeerRevoke(reason),
            );
        }
        Step::Control(_, _) | Step::NonControl => {
            finish(
                channel,
                &shared,
                &tx,
                audit,
                Actor::Console,
                Ending::Protocol("consent"),
            );
        }
        Step::LocalRevoke(reason) => {
            finish_local_revoke(
                channel,
                session,
                reason,
                &shared,
                &tx,
                audit,
                Actor::Console,
            )
            .await;
        }
        Step::Disconnected => {
            finish(
                channel,
                &shared,
                &tx,
                audit,
                Actor::Console,
                Ending::EarlyDisconnect("consent"),
            );
        }
        Step::ChannelFault => {
            finish(
                channel,
                &shared,
                &tx,
                audit,
                Actor::Console,
                Ending::ChannelFault("consent"),
            );
        }
        Step::TimedOut => {
            finish(
                channel,
                &shared,
                &tx,
                audit,
                Actor::Console,
                Ending::Timeout("consent"),
            );
        }
    }
}

/// The console-side active loop: multiplex operator-driven commands, streamed
/// job output, echoed heartbeats, and revocation over the one encrypted channel
/// (ST-M003-001/002).
///
/// The loop is the sole owner of the channel and always keeps draining inbound
/// frames — so control frames (cancel acks, revoke) stay responsive and teardown
/// never deadlocks — while publishing events over the handle's **bounded** stream
/// under a drop-and-mark policy for streamed output (REQ-N-006, see
/// [`deliver_output`]): technician-side memory is bounded even if a remote target
/// floods output, and any shed bytes are reported honestly, never silently
/// dropped (REQ-N-008). Console requests arrive from [`HostCommand`]s, are
/// stamped with the negotiated session, sent, and correlated back by
/// [`RequestId`]; a `Response`/`Error` resolves the waiting caller's one-shot.
/// Any teardown fails every pending request and emits an honest
/// [`JobOutcome::Unknown`] for jobs whose terminal event never arrived
/// (REQ-N-008), never a silent success.
async fn host_active<T: AsyncRead + AsyncWrite + Unpin>(
    mut channel: SecureChannel<T>,
    session: SessionId,
    shared: &Shared,
    tx: &mpsc::Sender<SessionEvent>,
    revoke_rx: &mut UnboundedReceiver<RevokeReason>,
    command_rx: &mut UnboundedReceiver<HostCommand>,
    audit: &dyn SessionAudit,
) {
    let mut pending: HashMap<RequestId, oneshot::Sender<Result<OpResult, SessionError>>> =
        HashMap::new();
    let mut running: BTreeSet<JobId> = BTreeSet::new();
    // Output bytes shed under backpressure, per job, awaiting an
    // `ExecOutputDropped` marker (REQ-N-006 drop-and-mark).
    let mut dropped: BTreeMap<JobId, u64> = BTreeMap::new();
    let mut next_id: u64 = 0;

    loop {
        tokio::select! {
            biased;
            cmd = revoke_rx.recv() => {
                let reason = cmd.unwrap_or(RevokeReason::TechnicianRequested);
                shared.set_control_active(false);
                fail_pending(&mut pending, || SessionError::RevokedLocally);
                finish_running_unknown(&running, tx);
                return finish_local_revoke(channel, session, reason, shared, tx, audit, Actor::Console).await;
            }
            maybe_cmd = command_rx.recv() => {
                let Some(command) = maybe_cmd else {
                    // The handle was dropped; its Drop aborts this task. Nothing
                    // more to do here.
                    continue;
                };
                let id = RequestId(next_id);
                next_id += 1;
                let stamp = command.session_override.unwrap_or(session);
                let message = match command.kind {
                    Outbound::Op(op) => Message::Request {
                        id,
                        session: stamp,
                        origin: command.attribution.origin,
                        purpose: command.attribution.purpose,
                        op,
                    },
                    Outbound::Cancel(job) => Message::Cancel {
                        id,
                        session: stamp,
                        origin: command.attribution.origin,
                        purpose: command.attribution.purpose,
                        job,
                    },
                };
                match channel.send(&message).await {
                    Ok(()) => {
                        pending.insert(id, command.reply);
                    }
                    // A validation/oversize rejection is caught before any bytes
                    // reach the wire and does not poison the channel: fail only
                    // this call and keep the session healthy (REQ-N-012). The
                    // typed category is preserved where one exists
                    // (ST-M003-003, REQ-N-015).
                    Err(error) if !channel.is_poisoned() => {
                        let code = match &error {
                            SecureChannelError::Decode(protocol) => protocol.code(),
                            SecureChannelError::Oversized { .. } => ErrorCode::OversizedMessage,
                            _ => ErrorCode::Internal,
                        };
                        let _ = command.reply.send(Err(SessionError::Invalid {
                            code,
                            message: error.to_string(),
                        }));
                    }
                    Err(_) => {
                        let _ = command.reply.send(Err(SessionError::Disconnected));
                        shared.set_control_active(false);
                        fail_pending(&mut pending, || SessionError::Disconnected);
                        finish_running_unknown(&running, tx);
                        return finish(channel, shared, tx, audit, Actor::Console, Ending::ActiveDisconnect);
                    }
                }
            }
            frame = recv_frame(&mut channel) => match frame {
                Frame::Message(Message::Response { id, result, .. }) => {
                    // Both a started command and an opened terminal are jobs: track
                    // them so an unfinished one reports an honest Unknown outcome on
                    // teardown, never a fabricated success (ST-M003-009, REQ-N-008).
                    match &result {
                        OpResult::ExecStarted { job, .. } | OpResult::TermOpened { job } => {
                            running.insert(*job);
                        }
                        _ => {}
                    }
                    if let Some(reply) = pending.remove(&id) {
                        let _ = reply.send(Ok(result));
                    }
                }
                Frame::Message(Message::Error { id: Some(id), code, message, .. }) => {
                    if let Some(reply) = pending.remove(&id) {
                        let _ = reply.send(Err(SessionError::Remote { code, message }));
                    }
                }
                Frame::Message(Message::Event { session: stamped, event }) if stamped == session => {
                    match event {
                        EventKind::JobOutput { job, stream, data } => {
                            deliver_output(tx, &mut dropped, job, stream, data.into_inner());
                        }
                        EventKind::JobExited { job, exit_code, outcome, .. } => {
                            running.remove(&job);
                            // Report any bytes shed for this job before its finish.
                            flush_dropped(tx, &mut dropped);
                            emit(tx, SessionEvent::ExecFinished { job, exit_code, outcome });
                        }
                        EventKind::Disconnected { job, .. } => {
                            running.remove(&job);
                            flush_dropped(tx, &mut dropped);
                            emit(tx, SessionEvent::ExecFinished { job, exit_code: None, outcome: JobOutcome::Unknown });
                        }
                        // The agent observed or controlled the desktop; reflect
                        // the same state onto the console feed so the technician
                        // sees what the customer sees (ST-M005-005, REQ-F-017).
                        EventKind::DesktopActivity { access } => {
                            emit(tx, SessionEvent::DesktopActivity { access });
                        }
                    }
                }
                // An event stamped with a session this channel does not own is
                // dropped so results stay attributable to one target (REQ-F-016).
                Frame::Message(Message::Event { .. }) => {}
                Frame::Message(Message::Error { id: None, .. }) => {}
                Frame::Message(Message::Control { control: ControlMessage::Heartbeat { seq }, .. }) => {
                    shared.record_heartbeat();
                    if send_control(&mut channel, session, ControlMessage::Heartbeat { seq }).await.is_err() {
                        shared.set_control_active(false);
                        fail_pending(&mut pending, || SessionError::Disconnected);
                        finish_running_unknown(&running, tx);
                        return finish(channel, shared, tx, audit, Actor::Console, Ending::ActiveDisconnect);
                    }
                }
                Frame::Message(Message::Control { control: ControlMessage::Revoke { reason }, .. }) => {
                    shared.set_control_active(false);
                    fail_pending(&mut pending, || SessionError::RevokedByPeer);
                    finish_running_unknown(&running, tx);
                    return finish(channel, shared, tx, audit, Actor::Agent, Ending::PeerRevoke(reason));
                }
                // The agent's teardown report, sent just before its own
                // `Revoke` when the customer stops (protocol minor 11,
                // ST-M009-012): kept for the end record. A report stamped with
                // another session is not this target's and is ignored.
                Frame::Message(Message::Control { session: stamped, control: ControlMessage::CleanupReported { ok, notes } }) => {
                    if stamped == session {
                        shared.set_peer_cleanup(CleanupReport { ok, notes });
                    }
                }
                // The agent reflected a freshly armed reboot-resume ticket
                // (ADR-0014, ST-M006-001): surface it for **display only**. It
                // carries no key material and never grants control — attended
                // consent still runs after any reconnect (REQ-S-001). A matched
                // agent always advertises CAP_REBOOT_RESUME_V1, so this is in
                // contract; a malformed ticket id is recorded honestly and dropped
                // rather than tearing down a healthy session.
                Frame::Message(Message::Control { control: ControlMessage::RebootResumeArmed { ticket_id, not_after_unix_millis, .. }, .. }) => {
                    match RebootResumeTicketId::try_from(ticket_id.as_bytes()) {
                        Ok(ticket_id) => {
                            shared.set_armed_not_after(not_after_unix_millis);
                            emit(tx, SessionEvent::RebootResumeArmed { ticket_id, not_after_unix_millis });
                            audit.record(Actor::Agent, "reboot_resume_armed", Outcome::Ok);
                        }
                        Err(_) => audit.record(Actor::Agent, "reboot_resume_reflection_malformed", Outcome::Failed),
                    }
                }
                // The agent reflected a disarm (arm-then-cancel): clear the display
                // indicator. Nothing survives to a reboot (ADR-0014).
                Frame::Message(Message::Control { control: ControlMessage::RebootResumeDisarmed { ticket_id }, .. }) => {
                    match RebootResumeTicketId::try_from(ticket_id.as_bytes()) {
                        Ok(ticket_id) => {
                            shared.clear_armed();
                            emit(tx, SessionEvent::RebootResumeDisarmed { ticket_id });
                            audit.record(Actor::Agent, "reboot_resume_disarmed", Outcome::Ok);
                        }
                        Err(_) => audit.record(Actor::Agent, "reboot_resume_reflection_malformed", Outcome::Failed),
                    }
                }
                // Any other message (a stray Request, consent control, Hello) is a
                // protocol violation on the console side and fails closed.
                Frame::Message(_) => {
                    shared.set_control_active(false);
                    fail_pending(&mut pending, || SessionError::Protocol("active".to_string()));
                    finish_running_unknown(&running, tx);
                    return finish(channel, shared, tx, audit, Actor::Console, Ending::Protocol("active"));
                }
                Frame::Disconnected => {
                    shared.set_control_active(false);
                    fail_pending(&mut pending, || SessionError::Disconnected);
                    finish_running_unknown(&running, tx);
                    return finish(channel, shared, tx, audit, Actor::Console, Ending::ActiveDisconnect);
                }
                Frame::ChannelFault => {
                    shared.set_control_active(false);
                    fail_pending(&mut pending, || SessionError::Disconnected);
                    finish_running_unknown(&running, tx);
                    return finish(channel, shared, tx, audit, Actor::Console, Ending::ChannelFault("active"));
                }
            },
        }
    }
}

/// The agent-side session task (ST-M002-005/006): dial and pair as the Noise
/// initiator, announce the informed disclosure, await the customer's single
/// consent decision, then hold or tear down the active session.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn join_task<S>(
    stream: S,
    identity: TargetIdentity,
    reboot_probe: RebootProbe,
    disclosure: String,
    version: ProtocolVersion,
    capabilities: Vec<String>,
    shared: Arc<Shared>,
    tx: mpsc::Sender<SessionEvent>,
    mut consent_rx: UnboundedReceiver<ConsentDecision>,
    mut revoke_rx: UnboundedReceiver<RevokeReason>,
    mut arm_rx: UnboundedReceiver<ArmCommand>,
    armer: Arc<dyn RebootResumeArmer>,
    audit: Arc<dyn SessionAudit>,
) where
    // Generic over the transport stream so the agent pairs identically whether it
    // dialed the console directly (a `TcpStream`) or reached it through the relay
    // (also a `TcpStream`, after the rendezvous preamble) — ST-M007-008,
    // ADR-0016 §5. The session logic below never learns which dialer produced it.
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let audit = audit.as_ref();

    // Whether this agent advertises the interactive-terminal capability. The
    // agent enforces the gate itself: a terminal operation is answered
    // `Unsupported` unless CAP_TERM_V1 is advertised, so a peer on an older minor
    // (or one that simply cannot serve terminals) interoperates (REQ-N-012,
    // ST-M003-009).
    let terminal_enabled = capabilities.iter().any(|c| c == CAP_TERM_V1);

    // Whether this agent advertises the interactive-desktop capability. Enforced
    // the same way: a desktop capture or input operation is answered
    // `Unsupported` unless CAP_DESKTOP_V1 is advertised. A production agent
    // advertises it only where a capture-and-input backend is present, so a peer
    // never sees the capability on a platform that would answer every desktop
    // operation with `Unsupported` (REQ-N-012, REQ-S-009, ST-M005-001).
    let desktop_enabled = capabilities.iter().any(|c| c == CAP_DESKTOP_V1);

    // Whether this agent advertises the explorer file operations (the drive
    // list and the removal of a folder with its content, ST-M010-001).
    // Enforced the same way: both are answered `Unsupported`, before anything
    // is listed or removed, unless CAP_FS_V2 is advertised (REQ-N-012).
    let fs_v2_enabled = capabilities.iter().any(|c| c == CAP_FS_V2);

    // Prologue binds the version-canonical set (see the responder side in
    // `host_task`): both peers must hash an identical set or the downgrade
    // tripwire fires on every honest asymmetric pair. The agent's served
    // `capabilities` (host-detected, possibly a subset) are *not* hashed here;
    // they stay local and gate each operation above, answering `Unsupported` for
    // anything this host does not serve (ST-M002-008, REQ-S-008).
    let prologue_caps = canonical_capabilities(version);

    // Pair as the Noise initiator, but keep watching for a local stop/revoke so
    // it is acknowledged promptly instead of waiting out PAIR_TIMEOUT. Losing the
    // pairing arm to a revoke drops the in-flight handshake (and its stream),
    // tearing down fail-closed before any channel or key exists. A consent
    // decision arriving now stays queued and is honored after pairing.
    let pairing_result = tokio::select! {
        biased;
        _ = revoke_rx.recv() => {
            return fail_before_channel(&shared, &tx, audit, Actor::Agent, EarlyFailure {
                stage: "pairing",
                message: "session stopped during pairing",
                action: "revoked",
                outcome: Outcome::Cancelled,
            });
        }
        res = timeout(
            PAIR_TIMEOUT,
            Pairing::perform(stream, Role::Initiator, version, &prologue_caps),
        ) => res,
    };
    // Bind the Noise transcript hash `h` from a successful pairing. It is kept
    // **local** to this task (never stored on shared state) and handed to the
    // reboot-resume armer only inside the active loop, so key material derived
    // from it has the narrowest possible reach (ADR-0014). It is zeroized when
    // this task returns.
    let (mut channel, handshake_hash) = match pairing_result {
        Ok(Ok(paired)) => {
            shared.set_approval_code(paired.approval_code);
            // The console's display name, as the technician's computer reports
            // it: a label for the customer surface, never authority (ST-M009-011).
            if let Some(name) = paired.peer_display_name {
                shared.set_peer_display_name(name);
            }
            (paired.channel, paired.handshake_hash)
        }
        Ok(Err(error)) => {
            return fail_before_channel(&shared, &tx, audit, Actor::Agent, pairing_failure(&error));
        }
        Err(_elapsed) => {
            return fail_before_channel(
                &shared,
                &tx,
                audit,
                Actor::Agent,
                EarlyFailure {
                    stage: "pairing",
                    message: "pairing timed out",
                    action: "error",
                    outcome: Outcome::Timeout,
                },
            );
        }
    };
    emit(&tx, SessionEvent::Connected);
    audit.record(Actor::Agent, "pairing_completed", Outcome::Ok);

    // The agent mints the session id and hands the console the honest disclosure
    // so both endpoints show it, carried on the same frame that establishes the
    // session. The customer is not yet asked to grant anything: the approval
    // code shown after pairing must first be compared aloud and confirmed.
    let session = SessionId::new();
    shared.set_session(session);
    if send_control(
        &mut channel,
        session,
        ControlMessage::ConsentRequested { disclosure },
    )
    .await
    .is_err()
    {
        return finish(
            channel,
            &shared,
            &tx,
            audit,
            Actor::Agent,
            Ending::EarlyDisconnect("consent"),
        );
    }
    // Pause for the humanly-perceptible compare window and wait for the console's
    // confirmation that the Session Approval Codes match before presenting the
    // customer with the grant decision (REQ-S-006, ST-M002-013). No consent is
    // read here: there is no control before the gate passes (REQ-S-001). A
    // revoke, disconnect, fault, timeout, or any other frame fails closed
    // (REQ-S-009).
    emit(&tx, SessionEvent::AwaitingApprovalCompare);
    audit.record(Actor::Agent, "consent_requested", Outcome::Ok);
    tokio::select! {
        biased;
        cmd = revoke_rx.recv() => {
            let reason = cmd.unwrap_or(RevokeReason::CustomerRequested);
            return finish_local_revoke(channel, session, reason, &shared, &tx, audit, Actor::Agent).await;
        }
        incoming = timeout(CONSENT_TIMEOUT, recv_control(&mut channel)) => match incoming {
            Ok(Incoming::Control(_, ControlMessage::ApprovalCompareConfirmed {})) => {
                // The technician confirmed the match; fall through to the consent
                // decision below.
                audit.record(Actor::Console, "approval_compare_confirmed", Outcome::Ok);
            }
            Ok(Incoming::Control(_, ControlMessage::Revoke { reason })) => {
                return finish(channel, &shared, &tx, audit, Actor::Console, Ending::PeerRevoke(reason));
            }
            Ok(Incoming::Control(_, _)) | Ok(Incoming::NonControl) => {
                return finish(channel, &shared, &tx, audit, Actor::Agent, Ending::Protocol("compare"));
            }
            Ok(Incoming::Disconnected) => {
                return finish(channel, &shared, &tx, audit, Actor::Agent, Ending::EarlyDisconnect("compare"));
            }
            Ok(Incoming::ChannelFault) => {
                return finish(channel, &shared, &tx, audit, Actor::Agent, Ending::ChannelFault("compare"));
            }
            Err(_elapsed) => {
                return finish(channel, &shared, &tx, audit, Actor::Agent, Ending::Timeout("compare"));
            }
        },
    }

    // The compare gate passed. Now ask the customer to grant control and show the
    // decision on both endpoints (REQ-F-006).
    emit(&tx, SessionEvent::AwaitingConsent);
    audit.record(Actor::Agent, "consent_awaited", Outcome::Ok);

    // Await the customer's decision or a local/peer revoke. There is no control
    // before grant_consent (REQ-S-001). Each branch is terminal (grant
    // transitions into the active loop).
    tokio::select! {
        biased;
        decision = consent_rx.recv() => match decision {
            Some(ConsentDecision::Grant) => {
                if send_control(&mut channel, session, ControlMessage::ConsentGranted { target: identity.clone() }).await.is_err() {
                    return finish(channel, &shared, &tx, audit, Actor::Agent, Ending::EarlyDisconnect("consent"));
                }
                shared.set_control_active(true);
                emit(&tx, SessionEvent::ConsentGranted { identity: identity.clone() });
                emit(&tx, SessionEvent::Active);
                audit.record(Actor::Agent, "consent_granted", Outcome::Ok);
                let sysinfo = SystemInfoSource { identity: identity.clone(), reboot_probe };
                join_active(channel, session, &sysinfo, terminal_enabled, desktop_enabled, fs_v2_enabled, &handshake_hash, armer.as_ref(), &shared, &tx, &mut revoke_rx, &mut arm_rx, audit).await;
            }
            Some(ConsentDecision::Deny(reason)) => {
                let _ = send_control(&mut channel, session, ControlMessage::ConsentDenied { reason }).await;
                // Drain like the revoke path so an abortive reset cannot wipe the
                // `ConsentDenied` from the console's receive buffer before it is
                // read; the console then audits the precise consent-denied
                // outcome rather than a bare disconnect. Bounded by TEARDOWN_DRAIN.
                let _ = timeout(TEARDOWN_DRAIN, drain_until_closed(&mut channel)).await;
                finish(channel, &shared, &tx, audit, Actor::Agent, Ending::ConsentDenied);
            }
            None => {
                // The consent sender was dropped without a decision: treat as
                // a customer revoke.
                finish_local_revoke(channel, session, RevokeReason::CustomerRequested, &shared, &tx, audit, Actor::Agent).await;
            }
        },
        cmd = revoke_rx.recv() => {
            let reason = cmd.unwrap_or(RevokeReason::CustomerRequested);
            finish_local_revoke(channel, session, reason, &shared, &tx, audit, Actor::Agent).await;
        }
        incoming = recv_control(&mut channel) => match incoming {
            Incoming::Control(_, ControlMessage::Revoke { reason }) => {
                finish(channel, &shared, &tx, audit, Actor::Console, Ending::PeerRevoke(reason));
            }
            Incoming::Control(_, _) | Incoming::NonControl => {
                finish(channel, &shared, &tx, audit, Actor::Agent, Ending::Protocol("consent"));
            }
            Incoming::Disconnected => {
                finish(channel, &shared, &tx, audit, Actor::Agent, Ending::EarlyDisconnect("consent"));
            }
            Incoming::ChannelFault => {
                finish(channel, &shared, &tx, audit, Actor::Agent, Ending::ChannelFault("consent"));
            }
        },
    }
}

/// One step the agent takes after classifying an inbound frame in its active
/// loop.
enum AgentStep {
    /// Keep serving the session.
    Continue,
    /// The console revoked; tear down reporting the peer's reason.
    PeerRevoke(RevokeReason),
    /// The console sent an out-of-contract frame; fail closed.
    Protocol,
    /// The channel disconnected while handling the frame.
    Disconnected,
}

/// The agent-side active loop: run the execution [`Broker`] at the agent's
/// privilege, serve console `Request`/`Cancel` frames, stream job output back,
/// emit heartbeats on a timer, and tear down fail-closed (ST-M003-001/002).
///
/// The loop is the sole owner of the channel. It drains the broker's **bounded**
/// event stream (REQ-N-006) and forwards output promptly; in-flight output is
/// bounded by that channel, and if the console stalls, TCP flow control carries
/// the backpressure back here (the broker's readers block, the pipe fills, the
/// child blocks). The console bounds its own memory independently with a
/// drop-and-mark policy (see [`host_active`]). Every request is bound to the
/// negotiated session: a frame stamped with any other session is rejected with
/// [`ErrorCode::UnknownSession`] and never run (REQ-S-012, REQ-S-014). On
/// revocation or disconnect every running job's process tree is terminated and
/// its honest outcome is reported (REQ-S-004, REQ-N-008).
#[allow(clippy::too_many_arguments)]
async fn join_active<T: AsyncRead + AsyncWrite + Unpin>(
    mut channel: SecureChannel<T>,
    session: SessionId,
    sysinfo: &SystemInfoSource,
    terminal_enabled: bool,
    desktop_enabled: bool,
    fs_v2_enabled: bool,
    handshake_hash: &[u8; 32],
    armer: &dyn RebootResumeArmer,
    shared: &Shared,
    tx: &mpsc::Sender<SessionEvent>,
    revoke_rx: &mut UnboundedReceiver<RevokeReason>,
    arm_rx: &mut UnboundedReceiver<ArmCommand>,
    audit: &dyn SessionAudit,
) {
    let (broker, mut broker_rx) = Broker::new();
    // The file store serves the file-operation half of the schema for this
    // session against a real filesystem, rooting its staging area under the
    // session id so cleanup is per-session (ADR-0012, REQ-F-019).
    let file_store = FileStore::for_session(session);
    // The desktop backend serves the capture-and-input half of the schema
    // (ADR-0015). It is consulted only when the capability is in force; on a
    // platform without a real backend it fails closed with `DesktopUnsupported`.
    let desktop = cretlink_exec::Desktop::new();
    // The capture-backpressure governor holds this session's paced-refresh clock
    // and in-flight/memory budgets (REQ-N-006, ST-M005-001). Each `DesktopCapture`
    // consults it so a runaway caller is paced or refused rather than driving
    // capture without bound.
    let mut capture_governor =
        cretlink_exec::CaptureGovernor::new(cretlink_exec::CaptureLimits::from_protocol());
    // The input-rate governor paces synthesized pointer, scroll, key, type, and
    // hotkey actions to a sustained ceiling with a short burst tolerance
    // (REQ-N-006, ST-M005-002). Input is lossless to defer, so it only ever
    // paces — it never drops an action.
    let mut input_governor =
        cretlink_exec::InputGovernor::new(cretlink_exec::InputLimits::from_protocol());
    let mut heartbeat = tokio::time::interval(HEARTBEAT_INTERVAL);
    // Coalesce missed ticks so a long output send never triggers a heartbeat
    // burst afterwards.
    heartbeat.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let mut seq: u64 = 0;
    // The customer-visible activity of this session (ST-M009-011).
    let mut activity = ActivityTap::default();

    // Discard any arming gestures that arrived before the session became active.
    // Reboot-resume may be armed only from *within* an already-consented,
    // SAC-verified session, mirroring the initial full-control gesture: a stale
    // pre-consent `arm` is never replayed the instant consent lands (ADR-0014,
    // ST-M006-001, REQ-S-001). `try_recv` drains the buffer without blocking.
    while arm_rx.try_recv().is_ok() {}

    loop {
        tokio::select! {
            biased;
            cmd = revoke_rx.recv() => {
                let reason = cmd.unwrap_or(RevokeReason::CustomerRequested);
                return teardown_agent_revoke(channel, session, &broker, &mut broker_rx, &file_store, reason, shared, tx, &mut activity, audit).await;
            }
            cmd = arm_rx.recv() => {
                // A local reboot-resume arming gesture. `None` means the handle was
                // dropped: ignore it and keep serving. A refused arm keeps the
                // session healthy; only a failed wire send tears it down.
                if let Some(cmd) = cmd
                    && handle_arm_command(&mut channel, session, handshake_hash, armer, &sysinfo.identity, shared, cmd, tx, audit).await.is_err()
                {
                    broker.cancel_all();
                    return agent_active_finish(channel, session, &file_store, shared, tx, &mut activity, audit, Actor::Agent, Ending::ActiveDisconnect).await;
                }
            }
            maybe_event = broker_rx.recv() => {
                if let Some(event) = maybe_event
                    && forward_broker_event(&mut channel, session, event, tx, &mut activity, audit).await.is_err()
                {
                    broker.cancel_all();
                    return agent_active_finish(channel, session, &file_store, shared, tx, &mut activity, audit, Actor::Agent, Ending::ActiveDisconnect).await;
                }
            }
            frame = recv_frame(&mut channel) => {
                let step = match frame {
                    Frame::Message(message) => {
                        handle_agent_frame(&mut channel, session, &broker, &file_store, &desktop, &mut capture_governor, &mut input_governor, sysinfo, terminal_enabled, desktop_enabled, fs_v2_enabled, shared, message, tx, &mut activity, audit).await
                    }
                    Frame::Disconnected => AgentStep::Disconnected,
                    Frame::ChannelFault => {
                        broker.cancel_all();
                        return agent_active_finish(channel, session, &file_store, shared, tx, &mut activity, audit, Actor::Agent, Ending::ChannelFault("active")).await;
                    }
                };
                match step {
                    AgentStep::Continue => {}
                    AgentStep::PeerRevoke(reason) => {
                        broker.cancel_all();
                        return agent_active_finish(channel, session, &file_store, shared, tx, &mut activity, audit, Actor::Console, Ending::PeerRevoke(reason)).await;
                    }
                    AgentStep::Protocol => {
                        broker.cancel_all();
                        return agent_active_finish(channel, session, &file_store, shared, tx, &mut activity, audit, Actor::Agent, Ending::Protocol("active")).await;
                    }
                    AgentStep::Disconnected => {
                        broker.cancel_all();
                        return agent_active_finish(channel, session, &file_store, shared, tx, &mut activity, audit, Actor::Agent, Ending::ActiveDisconnect).await;
                    }
                }
            }
            _ = heartbeat.tick() => {
                seq += 1;
                if send_control(&mut channel, session, ControlMessage::Heartbeat { seq }).await.is_err() {
                    broker.cancel_all();
                    return agent_active_finish(channel, session, &file_store, shared, tx, &mut activity, audit, Actor::Agent, Ending::ActiveDisconnect).await;
                }
            }
        }
    }
}

/// Services one local reboot-resume arming gesture inside the agent's active
/// loop (ADR-0014, ST-M006-001).
///
/// Arming is honored **only** here — inside a live, consented session — and is
/// never reachable from the console or the MCP surface (REQ-S-001, REQ-S-003).
/// On a successful arm/disarm the agent reflects the clear ticket metadata to
/// the console (no key material) so both endpoints can show "armed until", emits
/// the local [`SessionEvent`], and updates the display-only shared indicator. A
/// refused arm is never reflected to the peer — it is recorded honestly in the
/// local audit log and leaves the session healthy.
///
/// Returns `Err(())` only when a wire send failed (the channel disconnected), so
/// the caller tears the active session down fail-closed.
#[allow(clippy::too_many_arguments)]
async fn handle_arm_command<T: AsyncRead + AsyncWrite + Unpin>(
    channel: &mut SecureChannel<T>,
    session: SessionId,
    handshake_hash: &[u8; 32],
    armer: &dyn RebootResumeArmer,
    target: &TargetIdentity,
    shared: &Shared,
    cmd: ArmCommand,
    tx: &mpsc::Sender<SessionEvent>,
    audit: &dyn SessionAudit,
) -> Result<(), ()> {
    match cmd {
        ArmCommand::Arm { lifetime } => match armer.arm(handshake_hash, target, lifetime) {
            Ok(outcome) => {
                // Reflect the armed state before touching local state, so a send
                // failure tears down without leaving a stale "armed" indicator.
                send_control(
                    channel,
                    session,
                    ControlMessage::RebootResumeArmed {
                        ticket_id: B64Bytes::new(outcome.ticket_id.to_vec()),
                        target: target.clone(),
                        not_after_unix_millis: outcome.not_after_unix_millis,
                    },
                )
                .await
                .map_err(|_| ())?;
                shared.set_armed_not_after(outcome.not_after_unix_millis);
                emit(
                    tx,
                    SessionEvent::RebootResumeArmed {
                        ticket_id: outcome.ticket_id,
                        not_after_unix_millis: outcome.not_after_unix_millis,
                    },
                );
                audit.record(Actor::Agent, "reboot_resume_armed", Outcome::Ok);
                Ok(())
            }
            Err(error) => {
                // Off-by-default and backend refusals are honest non-events: record
                // them locally and never leak the reason to the peer (ADR-0014).
                let outcome = match error {
                    RebootResumeArmError::Unavailable => Outcome::Cancelled,
                    RebootResumeArmError::Backend => Outcome::Failed,
                };
                audit.record(Actor::Agent, "reboot_resume_arm_refused", outcome);
                Ok(())
            }
        },
        ArmCommand::Disarm { ticket_id } => {
            let had_record = armer.disarm(&ticket_id);
            // Reflect the disarm regardless of whether a record was present: an
            // idempotent disarm still clears the console's indicator honestly.
            send_control(
                channel,
                session,
                ControlMessage::RebootResumeDisarmed {
                    ticket_id: B64Bytes::new(ticket_id.to_vec()),
                },
            )
            .await
            .map_err(|_| ())?;
            shared.clear_armed();
            emit(tx, SessionEvent::RebootResumeDisarmed { ticket_id });
            audit.record(
                Actor::Agent,
                "reboot_resume_disarmed",
                if had_record {
                    Outcome::Ok
                } else {
                    Outcome::Cancelled
                },
            );
            Ok(())
        }
    }
}

/// Forwards one broker event to the console. Output chunks are re-split to fit a
/// single encrypted record; a job's terminal report becomes a `JobExited` event,
/// is surfaced locally as [`SessionEvent::ExecFinished`] (so the customer sees
/// the outcome — visible activity, REQ-F-017/S-002), and audited honestly.
///
/// An event answers no request, so there is none to fail: an event that is
/// not sent, for whatever reason, ends the session as a disconnect. Output
/// that was dropped unmarked would read as complete (REQ-N-008).
async fn forward_broker_event<T: AsyncRead + AsyncWrite + Unpin>(
    channel: &mut SecureChannel<T>,
    session: SessionId,
    event: BrokerEvent,
    tx: &mpsc::Sender<SessionEvent>,
    activity: &mut ActivityTap,
    audit: &dyn SessionAudit,
) -> Result<(), ()> {
    match event {
        BrokerEvent::Output(chunk) => {
            let job = JobId(chunk.job.0);
            for piece in chunk.bytes.chunks(WIRE_OUTPUT_CHUNK_BYTES) {
                send_message(
                    channel,
                    Message::Event {
                        session,
                        event: EventKind::JobOutput {
                            job,
                            stream: chunk.stream,
                            data: B64Bytes::new(piece.to_vec()),
                        },
                    },
                )
                .await
                .map_err(|_| ())?;
            }
            Ok(())
        }
        BrokerEvent::Done(done) => {
            let job = JobId(done.job.0);
            let duration_ms = u64::try_from(done.duration.as_millis()).unwrap_or(u64::MAX);
            let result = send_message(
                channel,
                Message::Event {
                    session,
                    event: EventKind::JobExited {
                        job,
                        exit_code: done.exit_code,
                        duration_ms,
                        outcome: done.outcome,
                    },
                },
            )
            .await
            .map_err(|_| ());
            activity.emit_job(
                tx,
                SessionEvent::ExecFinished {
                    job,
                    exit_code: done.exit_code,
                    outcome: done.outcome,
                },
            );
            audit.record(Actor::Agent, "exec_finish", outcome_to_audit(done.outcome));
            result
        }
    }
}

/// Signals — on the local render feed and, best-effort, over the wire — that the
/// interactive desktop was just observed or controlled, so the customer surface
/// and the console reflect the same observation/control state from the one
/// endpoint that knows what actually happened after pacing or refusal
/// (ST-M005-005, REQ-F-017 through REQ-F-020, REQ-S-004). It carries the
/// interaction class only, never any captured pixel, keystroke, or clipboard
/// text (REQ-S-016 through REQ-S-018).
///
/// Called after the desktop operation itself has succeeded, mirroring the
/// read-side `clipboard_get` audit: the state is real regardless of whether the
/// operation's own response or ack later reaches the console, so the local emit
/// always happens. The wire push is best-effort — the caller has already sent
/// (or is about to send) that response, so a channel that breaks here is caught
/// on the next frame rather than fabricating a failure for an operation that
/// genuinely ran.
async fn signal_desktop_activity<C: FrameSink>(
    channel: &mut C,
    session: SessionId,
    tx: &mpsc::Sender<SessionEvent>,
    access: DesktopAccess,
) {
    emit(tx, SessionEvent::DesktopActivity { access });
    let _ = send_message(
        channel,
        Message::Event {
            session,
            event: EventKind::DesktopActivity { access },
        },
    )
    .await;
}

/// Classifies and services one inbound frame on the agent side. Requests and
/// cancels are session-scoped: any frame stamped with a session other than the
/// negotiated one is refused with [`ErrorCode::UnknownSession`] and never run.
#[allow(clippy::too_many_arguments)]
async fn handle_agent_frame<T: AsyncRead + AsyncWrite + Unpin>(
    channel: &mut SecureChannel<T>,
    negotiated: SessionId,
    broker: &Broker,
    file_store: &FileStore,
    desktop: &cretlink_exec::Desktop,
    capture_governor: &mut cretlink_exec::CaptureGovernor,
    input_governor: &mut cretlink_exec::InputGovernor,
    sysinfo: &SystemInfoSource,
    terminal_enabled: bool,
    desktop_enabled: bool,
    fs_v2_enabled: bool,
    shared: &Shared,
    message: Message,
    tx: &mpsc::Sender<SessionEvent>,
    activity: &mut ActivityTap,
    audit: &dyn SessionAudit,
) -> AgentStep {
    match message {
        Message::Request {
            id,
            session,
            origin,
            purpose,
            op,
        } => {
            if session != negotiated {
                // Bound to THIS session only: reject and never execute
                // (REQ-S-012, REQ-S-014). The customer still sees the refused
                // request (ST-M009-011).
                let _ = send_error(
                    channel,
                    Some(id),
                    Some(negotiated),
                    ErrorCode::UnknownSession,
                    "request is not bound to this session",
                    ErrorStage::Execution,
                )
                .await;
                activity.refused(
                    tx,
                    Attribution::new(origin, purpose),
                    &op,
                    ErrorCode::UnknownSession,
                );
                return AgentStep::Continue;
            }
            // Describe the real operation before it moves into the handler, so
            // the customer sees what was done beside the caller's stated
            // purpose and the honest result (REQ-F-026, ST-M009-007), with the
            // structured kind, path and byte count (ST-M009-011).
            let operation = crate::describe_operation(&op);
            let is_step = crate::is_transfer_step(&op);
            let kind = ActivityKind::of(&op);
            let path = crate::activity::path_of(&op);
            let bytes = crate::activity::bytes_of(&op);
            let read_offset = match &op {
                Operation::FsRead { offset, .. } => Some(*offset),
                _ => None,
            };
            let is_exec = matches!(op, Operation::Exec { .. });
            // A step of a folder removal this agent serves. One it does not
            // serve is refused like any other request and reported as such.
            let is_removal = fs_v2_enabled && matches!(op, Operation::RemoveTree { .. });
            let started = Instant::now();
            let attribution = Attribution::new(origin, purpose);
            let mut tap = ReplyTap::new(channel, id);
            let step = handle_agent_request(
                &mut tap,
                negotiated,
                broker,
                file_store,
                desktop,
                capture_governor,
                input_governor,
                sysinfo,
                terminal_enabled,
                desktop_enabled,
                fs_v2_enabled,
                id,
                attribution.clone(),
                op,
                tx,
                activity,
                audit,
            )
            .await;
            let outcome = tap.outcome();
            let read = tap.read;
            let removal = tap.removal;
            if is_removal {
                // A folder removal: one item from its first step to its last,
                // with the state the folder is really in (ST-M010-001).
                activity.removal(
                    tx,
                    audit,
                    attribution,
                    operation,
                    path,
                    outcome,
                    removal,
                    started,
                );
            } else if let Some(offset) = read_offset {
                // A file read: reported when the file starts to leave the
                // computer and again when sending ends (ST-M009-011).
                activity.read(
                    tx,
                    attribution,
                    operation,
                    path,
                    offset,
                    outcome,
                    read,
                    started,
                );
            } else if is_step {
                // An internal transfer step is reported only when the agent
                // refused or failed it: once per path until that path's next
                // reported request, so a refused transfer is never silent on
                // the customer side and its in-flight chunks do not flood the
                // feed (ST-M009-011, REQ-F-017). No byte count: nothing of the
                // file is described.
                if outcome != ActionOutcome::Succeeded && activity.first_refusal(path.as_deref()) {
                    activity.handled(
                        tx,
                        attribution,
                        kind,
                        operation,
                        path,
                        None,
                        outcome,
                        started,
                    );
                }
            } else if !(is_exec && outcome == ActionOutcome::Succeeded) {
                // A command that started is reported by `CommandStarted` and
                // later `ExecFinished`; every other handled request that is not
                // an internal transfer step is reported here.
                activity.settle(path.as_deref());
                activity.handled(
                    tx,
                    attribution,
                    kind,
                    operation,
                    path,
                    bytes,
                    outcome,
                    started,
                );
            }
            step
        }
        Message::Cancel {
            id,
            session,
            origin,
            purpose,
            job,
        } => {
            if session != negotiated {
                let _ = send_error(
                    channel,
                    Some(id),
                    Some(negotiated),
                    ErrorCode::UnknownSession,
                    "cancel is not bound to this session",
                    ErrorStage::Execution,
                )
                .await;
                activity.handled(
                    tx,
                    Attribution::new(origin, purpose),
                    ActivityKind::JobControl,
                    crate::describe_cancel(job),
                    None,
                    None,
                    ActionOutcome::Failed {
                        code: ErrorCode::UnknownSession,
                    },
                    Instant::now(),
                );
                return AgentStep::Continue;
            }
            let started = Instant::now();
            let mut tap = ReplyTap::new(channel, id);
            let step = match broker.cancel(cretlink_exec::JobId(job.0)) {
                Ok(()) => reply_ack(&mut tap, negotiated, id).await,
                Err(_) => {
                    let _ = send_error(
                        &mut tap,
                        Some(id),
                        Some(negotiated),
                        ErrorCode::UnknownJob,
                        "no such job",
                        ErrorStage::Execution,
                    )
                    .await;
                    AgentStep::Continue
                }
            };
            let outcome = tap.outcome();
            activity.handled(
                tx,
                Attribution::new(origin, purpose),
                ActivityKind::JobControl,
                crate::describe_cancel(job),
                None,
                None,
                outcome,
                started,
            );
            step
        }
        // The console's heartbeat echo: record liveness (do not echo back).
        Message::Control {
            control: ControlMessage::Heartbeat { .. },
            ..
        } => {
            shared.record_heartbeat();
            AgentStep::Continue
        }
        Message::Control {
            control: ControlMessage::Revoke { reason },
            ..
        } => AgentStep::PeerRevoke(reason),
        // Any other frame (consent control during active, a Response/Event/Error,
        // or another Hello) is out of contract for the console-to-agent direction.
        _ => AgentStep::Protocol,
    }
}

/// Services one accepted, session-scoped console request.
#[allow(clippy::too_many_arguments)]
async fn handle_agent_request<C: FrameSink>(
    channel: &mut C,
    session: SessionId,
    broker: &Broker,
    file_store: &FileStore,
    desktop: &cretlink_exec::Desktop,
    capture_governor: &mut cretlink_exec::CaptureGovernor,
    input_governor: &mut cretlink_exec::InputGovernor,
    sysinfo: &SystemInfoSource,
    terminal_enabled: bool,
    desktop_enabled: bool,
    fs_v2_enabled: bool,
    id: RequestId,
    attribution: Attribution,
    op: Operation,
    tx: &mpsc::Sender<SessionEvent>,
    activity: &mut ActivityTap,
    audit: &dyn SessionAudit,
) -> AgentStep {
    match op {
        Operation::Exec {
            shell,
            command,
            working_dir,
            env,
            timeout_ms,
        } => {
            let spec = JobSpec {
                shell,
                command: command.clone(),
                working_dir: working_dir.map(PathBuf::from),
                env,
                timeout: timeout_ms.map(Duration::from_millis),
                // The wire Exec request drives only piped jobs; interactive
                // pseudo-console jobs get their own message set (ST-M003-009).
                mode: cretlink_exec::JobMode::Piped,
            };
            match broker.spawn(spec).await {
                Ok(started) => {
                    let job = JobId(started.job.0);
                    // Report the effective working directory the job actually
                    // received, so relative-path semantics are unambiguous on
                    // the console/AI side (ST-M003-003, REQ-F-009).
                    let working_dir = started.working_dir.to_string_lossy().into_owned();
                    let answered = answer(
                        channel,
                        session,
                        id,
                        OpResult::ExecStarted { job, working_dir },
                    )
                    .await;
                    if answered != Answer::Sent {
                        // The caller never learned of this job: nobody could
                        // read what it writes or stop it, so it is ended.
                        let _ = broker.cancel(started.job);
                        return answered.step();
                    }
                    // Visible activity: the customer sees exactly what is run
                    // (REQ-F-017, REQ-S-002), and the start is audited.
                    activity.emit_job(
                        tx,
                        SessionEvent::CommandStarted {
                            job,
                            shell,
                            command,
                            origin: attribution.origin,
                            purpose: attribution.purpose,
                        },
                    );
                    audit.record(Actor::Console, "exec_start", Outcome::Ok);
                    AgentStep::Continue
                }
                Err(_) => {
                    let _ = send_error(
                        channel,
                        Some(id),
                        Some(session),
                        ErrorCode::Internal,
                        "failed to start the command",
                        ErrorStage::Execution,
                    )
                    .await;
                    AgentStep::Continue
                }
            }
        }
        Operation::WriteStdin { job, data } => {
            match broker
                .write_stdin(cretlink_exec::JobId(job.0), data.into_inner())
                .await
            {
                Ok(()) => reply_ack(channel, session, id).await,
                Err(_) => {
                    let _ = send_error(
                        channel,
                        Some(id),
                        Some(session),
                        ErrorCode::UnknownJob,
                        "no such job",
                        ErrorStage::Execution,
                    )
                    .await;
                    AgentStep::Continue
                }
            }
        }
        Operation::ListJobs {} => {
            let jobs = broker
                .list_jobs()
                .into_iter()
                .map(|summary| JobSummary {
                    job: JobId(summary.job.0),
                    command_preview: summary.command_preview,
                    running: summary.running,
                })
                .collect();
            answer(channel, session, id, OpResult::Jobs { jobs })
                .await
                .step()
        }
        // --- Interactive terminal (ADR-0013, ST-M003-009, CAP_TERM_V1) ---
        // A terminal is a job: it spawns an interactive (ConPTY) job and reuses
        // the broker's cancel/list/exit paths, so `Cancel`/`ListJobs`/`JobExited`
        // and revoke teardown all apply unchanged.
        Operation::TermOpen {
            shell,
            cols,
            rows,
            working_dir,
            env,
        } => {
            // Capability gate (REQ-N-012): an agent that does not advertise
            // CAP_TERM_V1 answers Unsupported rather than opening a terminal.
            if !terminal_enabled {
                let _ = send_error(
                    channel,
                    Some(id),
                    Some(session),
                    ErrorCode::Unsupported,
                    "terminal capability (term.v1) is not advertised by this agent",
                    ErrorStage::Execution,
                )
                .await;
                return AgentStep::Continue;
            }
            // The dimensions are already schema-validated in range on decode;
            // `TermSize::new` re-checks against the identical backend bounds so a
            // malformed size can never reach the pseudo-console (ST-M003-008).
            let size = match cretlink_exec::TermSize::new(cols, rows) {
                Ok(size) => size,
                Err(_) => {
                    let _ = send_error(
                        channel,
                        Some(id),
                        Some(session),
                        ErrorCode::LimitExceeded,
                        "terminal size is out of bounds",
                        ErrorStage::Execution,
                    )
                    .await;
                    return AgentStep::Continue;
                }
            };
            // A terminal runs an interactive shell (empty command = the shell's
            // own REPL under ConPTY); its merged VT output and exit reuse the
            // existing job-event path (ST-M003-009).
            let mut spec = JobSpec::new(shell, String::new());
            spec.working_dir = working_dir.map(PathBuf::from);
            spec.env = env;
            let spec = spec.interactive(size);
            match broker.spawn(spec).await {
                Ok(started) => {
                    let job = JobId(started.job.0);
                    // Attribute the terminal to the technician-terminal actor,
                    // distinct from the AI bridge (REQ-S-023, frozen by
                    // ST-M003-006). The console stamps Origin::Terminal on the
                    // request.
                    audit.record(Actor::Terminal, "term_open", Outcome::Ok);
                    let answered = answer(channel, session, id, OpResult::TermOpened { job }).await;
                    if answered != Answer::Sent {
                        // As for a command: a terminal nobody learned of is
                        // ended.
                        let _ = broker.cancel(started.job);
                    }
                    answered.step()
                }
                // Off Windows there is no pseudo-console backend: honest
                // Unsupported, never a silently degraded piped job (REQ-N-008).
                Err(cretlink_exec::ExecError::InteractiveUnsupported) => {
                    audit.record(Actor::Terminal, "term_open", Outcome::Failed);
                    let _ = send_error(
                        channel,
                        Some(id),
                        Some(session),
                        ErrorCode::Unsupported,
                        "interactive terminals are only supported on Windows",
                        ErrorStage::Execution,
                    )
                    .await;
                    AgentStep::Continue
                }
                Err(_) => {
                    audit.record(Actor::Terminal, "term_open", Outcome::Failed);
                    let _ = send_error(
                        channel,
                        Some(id),
                        Some(session),
                        ErrorCode::Internal,
                        "failed to open the terminal",
                        ErrorStage::Execution,
                    )
                    .await;
                    AgentStep::Continue
                }
            }
        }
        Operation::TermResize { job, cols, rows } => {
            // Capability gate first (REQ-N-012): every terminal op is answered
            // Unsupported unless CAP_TERM_V1 is advertised, before any side
            // effect — the same gate as TermOpen.
            if !terminal_enabled {
                let _ = send_error(
                    channel,
                    Some(id),
                    Some(session),
                    ErrorCode::Unsupported,
                    "terminal capability (term.v1) is not advertised by this agent",
                    ErrorStage::Execution,
                )
                .await;
                return AgentStep::Continue;
            }
            let size = match cretlink_exec::TermSize::new(cols, rows) {
                Ok(size) => size,
                Err(_) => {
                    let _ = send_error(
                        channel,
                        Some(id),
                        Some(session),
                        ErrorCode::LimitExceeded,
                        "terminal size is out of bounds",
                        ErrorStage::Execution,
                    )
                    .await;
                    return AgentStep::Continue;
                }
            };
            // Last-writer-wins, and terminal-scoped: `resize` itself refuses a
            // piped or unknown job (NotInteractive/UnknownJob), both surfaced
            // honestly as "no such terminal" — never silently applied elsewhere.
            match broker.resize(cretlink_exec::JobId(job.0), size).await {
                Ok(()) => reply_ack(channel, session, id).await,
                Err(_) => {
                    let _ = send_error(
                        channel,
                        Some(id),
                        Some(session),
                        ErrorCode::UnknownJob,
                        "no such terminal",
                        ErrorStage::Execution,
                    )
                    .await;
                    AgentStep::Continue
                }
            }
        }
        Operation::TermClose { job } => {
            // Capability gate first (REQ-N-012), before any side effect or audit.
            if !terminal_enabled {
                let _ = send_error(
                    channel,
                    Some(id),
                    Some(session),
                    ErrorCode::Unsupported,
                    "terminal capability (term.v1) is not advertised by this agent",
                    ErrorStage::Execution,
                )
                .await;
                return AgentStep::Continue;
            }
            let jid = cretlink_exec::JobId(job.0);
            // Terminal-scoped: only a genuine interactive terminal may be closed.
            // A piped Exec job or an unknown id is refused honestly — never a
            // silent cancel of an arbitrary job tree and never a mis-attributed
            // term_close audit record (REQ-S-023, REQ-N-008). Mirrors the
            // simulator's `running && terminal` guard.
            match broker.is_interactive(jid) {
                Ok(true) => match broker.cancel(jid) {
                    Ok(()) => {
                        // Attribute the close to the technician-terminal actor,
                        // only for a real terminal (REQ-S-023, frozen by
                        // ST-M003-006). The console stamps Origin::Terminal.
                        audit.record(Actor::Terminal, "term_close", Outcome::Ok);
                        reply_ack(channel, session, id).await
                    }
                    // Raced to completion between the check and the cancel:
                    // honest UnknownJob, and no term_close audit.
                    Err(_) => {
                        let _ = send_error(
                            channel,
                            Some(id),
                            Some(session),
                            ErrorCode::UnknownJob,
                            "no such terminal",
                            ErrorStage::Execution,
                        )
                        .await;
                        AgentStep::Continue
                    }
                },
                // A piped job (`Ok(false)`) or an unknown id (`Err`): refuse
                // without cancelling and without a term_close audit.
                Ok(false) | Err(_) => {
                    let _ = send_error(
                        channel,
                        Some(id),
                        Some(session),
                        ErrorCode::UnknownJob,
                        "no such terminal",
                        ErrorStage::Execution,
                    )
                    .await;
                    AgentStep::Continue
                }
            }
        }
        // --- Interactive desktop (ADR-0015, ST-M005-001/002/003, CAP_DESKTOP_V1) ---
        // Every desktop operation is gated on the advertised capability first,
        // before any side effect (REQ-N-012). When advertised, capture and input
        // are served by the agent-side backend, which fails closed with
        // `Unsupported` on a platform without a capture-and-input backend rather
        // than fabricating a frame or dropping input (REQ-S-009, REQ-N-008). A
        // production agent advertises the capability only where that backend is
        // present, so it never answers its own desktop request with `Unsupported`.
        Operation::DesktopCapture {
            display,
            format,
            max_width,
            max_height,
            quality,
        } => {
            if !desktop_enabled {
                return desktop_not_advertised(channel, id, session).await;
            }
            let request = CaptureRequest {
                display,
                format,
                max_width,
                max_height,
                quality,
            };
            // Backpressure before any capture side effect (REQ-N-006,
            // ST-M005-001): pace a caller that outpaces the refresh ceiling down
            // to it, or refuse outright when the in-flight or memory budget is
            // exhausted rather than driving capture without bound. A refusal is
            // an honest error, never a fabricated or stale frame (REQ-N-008).
            match capture_governor.admit(Instant::now()) {
                Admission::Admit => {}
                Admission::Pace(wait) => tokio::time::sleep(wait).await,
                Admission::Reject(reason) => {
                    let detail = match reason {
                        CaptureReject::InFlight => {
                            "too many desktop captures are already in flight"
                        }
                        CaptureReject::Memory => "the desktop capture memory budget is exhausted",
                    };
                    let _ = send_error(
                        channel,
                        Some(id),
                        Some(session),
                        ErrorCode::StorageExhausted,
                        detail,
                        ErrorStage::Execution,
                    )
                    .await;
                    return AgentStep::Continue;
                }
            }
            // The capture counts against the in-flight budget from here; the
            // `release` below returns it, and any charged bytes, on every path.
            capture_governor.begin(Instant::now());
            let outcome = desktop.capture(&request);
            let held = outcome
                .as_ref()
                .ok()
                .and_then(|frame| frame.image.as_ref())
                .map_or(0, |bytes| bytes.len());
            capture_governor.charge(held);
            let step = match outcome {
                Ok(frame) => {
                    // Honest state: an image rides the frame only for an Active
                    // desktop, and only when it fits the inline bound. Frame
                    // staging is not implemented; an oversized frame is refused
                    // fail-closed rather than truncated.
                    match frame.image {
                        Some(bytes) if bytes.len() > limits::MAX_DESKTOP_INLINE_BYTES => {
                            let _ = send_error(
                                channel,
                                Some(id),
                                Some(session),
                                ErrorCode::Internal,
                                "captured frame exceeds the inline bound and frame staging is not available in this build",
                                ErrorStage::Execution,
                            )
                            .await;
                            AgentStep::Continue
                        }
                        image_opt => {
                            let image = image_opt.map(B64Bytes::from);
                            let answered = answer(
                                channel,
                                session,
                                id,
                                OpResult::DesktopFrame {
                                    meta: frame.meta,
                                    image,
                                    staged: None,
                                },
                            )
                            .await;
                            if answered == Answer::Sent {
                                // A frame reached the console: screen observation
                                // is active, signalled so both surfaces reflect it
                                // (ST-M005-005). The class is all that crosses —
                                // never the pixels just delivered.
                                signal_desktop_activity(
                                    channel,
                                    session,
                                    tx,
                                    DesktopAccess::Observe,
                                )
                                .await;
                            }
                            answered.step()
                        }
                    }
                }
                Err(err) => desktop_backend_error(channel, id, session, &err).await,
            };
            capture_governor.release(held);
            step
        }
        Operation::DesktopPointer {
            action,
            button,
            x,
            y,
        } => {
            if !desktop_enabled {
                return desktop_not_advertised(channel, id, session).await;
            }
            gate_input(input_governor).await;
            match desktop.pointer(action, button, x, y) {
                Ok(()) => {
                    audit.record(Actor::Console, "desktop_pointer", Outcome::Ok);
                    signal_desktop_activity(channel, session, tx, DesktopAccess::Control).await;
                    reply_ack(channel, session, id).await
                }
                Err(err) => desktop_backend_error(channel, id, session, &err).await,
            }
        }
        Operation::DesktopScroll { x, y, dx, dy } => {
            if !desktop_enabled {
                return desktop_not_advertised(channel, id, session).await;
            }
            gate_input(input_governor).await;
            match desktop.scroll(x, y, dx, dy) {
                Ok(()) => {
                    audit.record(Actor::Console, "desktop_scroll", Outcome::Ok);
                    signal_desktop_activity(channel, session, tx, DesktopAccess::Control).await;
                    reply_ack(channel, session, id).await
                }
                Err(err) => desktop_backend_error(channel, id, session, &err).await,
            }
        }
        Operation::DesktopKey { action, key } => {
            if !desktop_enabled {
                return desktop_not_advertised(channel, id, session).await;
            }
            gate_input(input_governor).await;
            match desktop.key(action, &key) {
                Ok(()) => {
                    audit.record(Actor::Console, "desktop_key", Outcome::Ok);
                    signal_desktop_activity(channel, session, tx, DesktopAccess::Control).await;
                    reply_ack(channel, session, id).await
                }
                Err(err) => desktop_backend_error(channel, id, session, &err).await,
            }
        }
        Operation::DesktopType { text } => {
            if !desktop_enabled {
                return desktop_not_advertised(channel, id, session).await;
            }
            gate_input(input_governor).await;
            match desktop.type_text(&text) {
                Ok(()) => {
                    audit.record(Actor::Console, "desktop_type", Outcome::Ok);
                    signal_desktop_activity(channel, session, tx, DesktopAccess::Control).await;
                    reply_ack(channel, session, id).await
                }
                Err(err) => desktop_backend_error(channel, id, session, &err).await,
            }
        }
        Operation::DesktopHotkey { keys } => {
            if !desktop_enabled {
                return desktop_not_advertised(channel, id, session).await;
            }
            gate_input(input_governor).await;
            match desktop.hotkey(&keys) {
                Ok(()) => {
                    audit.record(Actor::Console, "desktop_hotkey", Outcome::Ok);
                    signal_desktop_activity(channel, session, tx, DesktopAccess::Control).await;
                    reply_ack(channel, session, id).await
                }
                Err(err) => desktop_backend_error(channel, id, session, &err).await,
            }
        }
        Operation::DesktopWait { millis } => {
            if !desktop_enabled {
                return desktop_not_advertised(channel, id, session).await;
            }
            // A pacing pause between input actions. It is a session-local sleep
            // (the backend synthesizes input, not time), bounded by
            // `MAX_DESKTOP_WAIT_MS` on decode. It is not audited: like a stdin
            // write, it has no standalone side effect worth a log line.
            tokio::time::sleep(Duration::from_millis(millis)).await;
            reply_ack(channel, session, id).await
        }
        Operation::ClipboardGet {} => {
            if !desktop_enabled {
                return desktop_not_advertised(channel, id, session).await;
            }
            match desktop.clipboard_get() {
                Ok(text) => {
                    // A clipboard read is a privacy-sensitive access of the
                    // customer's clipboard, so it is audited like the write
                    // (ST-M005-003 AC2). The record is taken here, before the
                    // reply is sent, so the read is logged even if the response
                    // never reaches the console. It names the action only —
                    // never the contents — so the audit trail cannot itself leak
                    // what was on the clipboard (REQ-S-015, REQ-N-015).
                    audit.record(Actor::Console, "clipboard_get", Outcome::Ok);
                    // A clipboard read is passive observation of customer data,
                    // signalled like a screen capture so both surfaces reflect it
                    // (ST-M005-005). Emitted before the reply, so the state is
                    // recorded even if the response never lands.
                    signal_desktop_activity(channel, session, tx, DesktopAccess::Observe).await;
                    answer(
                        channel,
                        session,
                        id,
                        OpResult::ClipboardContents { text: Some(text) },
                    )
                    .await
                    .step()
                }
                Err(err) => desktop_backend_error(channel, id, session, &err).await,
            }
        }
        Operation::ClipboardSet { text } => {
            if !desktop_enabled {
                return desktop_not_advertised(channel, id, session).await;
            }
            match desktop.clipboard_set(&text) {
                Ok(()) => {
                    audit.record(Actor::Console, "clipboard_set", Outcome::Ok);
                    signal_desktop_activity(channel, session, tx, DesktopAccess::Control).await;
                    reply_ack(channel, session, id).await
                }
                Err(err) => desktop_backend_error(channel, id, session, &err).await,
            }
        }
        // Live system inspection (ST-M003-003): the honest consent-bound
        // identity (REQ-F-016, REQ-S-011) plus a reboot probe that runs NOW,
        // per request — never a consent-time cache. `false` means "no reboot
        // signal observed" (documented best-effort).
        Operation::SystemInfo {} => {
            let reboot_pending = (sysinfo.reboot_probe)();
            answer(
                channel,
                session,
                id,
                OpResult::SystemInfo {
                    target: sysinfo.identity.clone(),
                    reboot_pending,
                },
            )
            .await
            .step()
        }
        // File operations (ADR-0012, CAP_FS_V1) run against a real filesystem at
        // the agent's privilege via the session file store. Mutating operations
        // are audited by their honest outcome (REQ-N-008); read-only inspection
        // is not, so a listing-heavy transfer never floods the audit log.
        Operation::FsStat { .. }
        | Operation::FsList { .. }
        | Operation::CreateDir { .. }
        | Operation::FsRead { .. }
        | Operation::FsWrite { .. }
        | Operation::FsCommit { .. }
        | Operation::FsAbort { .. }
        | Operation::FsDigest { .. }
        | Operation::FsMove { .. }
        | Operation::FsDelete { .. }
        | Operation::RemoveDir { .. }
        | Operation::ListStaged {}
        | Operation::StagingCleanup { .. }
        | Operation::FsDrives {}
        | Operation::RemoveTree { .. } => {
            // The drive list and the removal of a folder with its content
            // (ST-M010-001) are served only by an agent that advertises
            // CAP_FS_V2: a peer that never saw the capability is answered
            // `Unsupported` before anything is listed or removed (REQ-N-012).
            if !fs_v2_enabled && matches!(op, Operation::FsDrives {} | Operation::RemoveTree { .. })
            {
                let _ = send_error(
                    channel,
                    Some(id),
                    Some(session),
                    ErrorCode::Unsupported,
                    "explorer file capability (fs.v2) is not advertised by this agent",
                    ErrorStage::Execution,
                )
                .await;
                return AgentStep::Continue;
            }
            let audit_action = fs_audit_action(&op);
            match file_store.perform(op).await {
                Ok(result) => {
                    if let Some(action) = audit_action {
                        audit.record(Actor::Console, action, Outcome::Ok);
                    }
                    answer(channel, session, id, result).await.step()
                }
                Err(fs_err) => {
                    if let Some(action) = audit_action {
                        audit.record(Actor::Console, action, Outcome::Failed);
                    }
                    let _ = send_error(
                        channel,
                        Some(id),
                        Some(session),
                        fs_err.code,
                        fs_err.message,
                        ErrorStage::Execution,
                    )
                    .await;
                    AgentStep::Continue
                }
            }
        }
    }
}

/// The audit action name for a mutating file operation, or `None` for a
/// read-only inspection. Only operations with a visible filesystem side effect
/// (or a staging purge) are audited, mirroring how command starts — not stdin
/// writes — are recorded on the exec path.
///
/// A folder removal ([`Operation::RemoveTree`]) is not in this table: it takes
/// several requests, and one answered request says nothing about the folder.
/// It is audited as a whole, by where it ends, in [`ActivityTap::removal`]
/// (`fs_remove_tree_start` when it begins, `fs_remove_tree` when it ends).
fn fs_audit_action(op: &Operation) -> Option<&'static str> {
    match op {
        Operation::CreateDir { .. } => Some("fs_create_dir"),
        Operation::FsCommit { .. } => Some("fs_commit"),
        Operation::FsMove { .. } => Some("fs_move"),
        Operation::FsDelete { .. } => Some("fs_delete"),
        Operation::RemoveDir { .. } => Some("fs_remove_dir"),
        Operation::StagingCleanup { .. } => Some("fs_cleanup"),
        _ => None,
    }
}

/// Sends a payload-free `Ack` response, mapping a lost channel to a disconnect.
async fn reply_ack<C: FrameSink>(channel: &mut C, session: SessionId, id: RequestId) -> AgentStep {
    answer(channel, session, id, OpResult::Ack {}).await.step()
}

/// Refuses a desktop operation the agent did not advertise support for.
///
/// The capability gate is enforced before any side effect: a peer on an older
/// minor (or one whose platform has no capture-and-input backend) that issues a
/// desktop operation is answered [`ErrorCode::Unsupported`] rather than acted on,
/// so same-major peers interoperate (REQ-N-012, ST-M005-001).
/// Paces one synthesized desktop input action to the session's sustained input
/// ceiling before it is dispatched (REQ-N-006, ST-M005-002).
///
/// Input is cheap and lossless to defer, so a burst past the tolerance waits out
/// the governor's pacing delay rather than being dropped — the action always
/// proceeds afterwards. The delay is bounded to one input interval per action
/// (see `MIN_DESKTOP_INPUT_INTERVAL_MS`), keeping the session responsive to a
/// revoke that lands mid-flood (REQ-S-004). `begin` re-reads the clock after any
/// sleep so the schedule advances from the moment the action actually admits.
async fn gate_input(governor: &mut cretlink_exec::InputGovernor) {
    if let cretlink_exec::InputAdmission::Pace(wait) = governor.admit(Instant::now()) {
        tokio::time::sleep(wait).await;
    }
    governor.begin(Instant::now());
}

async fn desktop_not_advertised<C: FrameSink>(
    channel: &mut C,
    id: RequestId,
    session: SessionId,
) -> AgentStep {
    let _ = send_error(
        channel,
        Some(id),
        Some(session),
        ErrorCode::Unsupported,
        "desktop capability (desktop.v1) not advertised by agent",
        ErrorStage::Execution,
    )
    .await;
    AgentStep::Continue
}

/// Maps a desktop-backend error to an honest wire error. A backend that is not
/// present on this platform answers [`ErrorCode::Unsupported`] (never a
/// fabricated frame or a silently dropped input); anything else is an internal
/// failure (REQ-N-008, REQ-S-009).
async fn desktop_backend_error<C: FrameSink>(
    channel: &mut C,
    id: RequestId,
    session: SessionId,
    err: &cretlink_exec::ExecError,
) -> AgentStep {
    let (code, message) = match err {
        cretlink_exec::ExecError::DesktopUnsupported => (
            ErrorCode::Unsupported,
            "desktop capture and input backend is not available on this platform",
        ),
        _ => (ErrorCode::Internal, "desktop operation failed"),
    };
    let _ = send_error(
        channel,
        Some(id),
        Some(session),
        code,
        message,
        ErrorStage::Execution,
    )
    .await;
    AgentStep::Continue
}

/// Locally revokes the agent session, but first cancels every running job's
/// process tree and forwards their honest terminal outcomes to the console
/// before dropping the channel — so a revoke mid-job never reads as a silent
/// success (REQ-S-004, REQ-S-014, REQ-N-008). Bounded by [`TEARDOWN_DRAIN`].
#[allow(clippy::too_many_arguments)]
async fn teardown_agent_revoke<T: AsyncRead + AsyncWrite + Unpin>(
    mut channel: SecureChannel<T>,
    session: SessionId,
    broker: &Broker,
    broker_rx: &mut mpsc::Receiver<BrokerEvent>,
    file_store: &FileStore,
    reason: RevokeReason,
    shared: &Shared,
    tx: &mpsc::Sender<SessionEvent>,
    activity: &mut ActivityTap,
    audit: &dyn SessionAudit,
) {
    let mut remaining = broker.job_ids().len();
    broker.cancel_all();

    // Drain each cancelled job's terminal report (and any last output) so the
    // console sees the honest Cancelled outcome, bounded so a stuck kill cannot
    // stall teardown.
    let deadline = tokio::time::sleep(TEARDOWN_DRAIN);
    tokio::pin!(deadline);
    while remaining > 0 {
        tokio::select! {
            biased;
            () = &mut deadline => break,
            maybe_event = broker_rx.recv() => match maybe_event {
                Some(event) => {
                    let is_done = matches!(event, BrokerEvent::Done(_));
                    if forward_broker_event(&mut channel, session, event, tx, activity, audit).await.is_err() {
                        break;
                    }
                    if is_done {
                        remaining -= 1;
                    }
                }
                None => break,
            },
        }
    }

    // Sweep and report before the `Revoke`, so the console still reads the
    // report on its way out (protocol minor 11, ST-M009-012).
    let ending = Ending::LocalRevoke(reason);
    activity.close_removals(audit);
    let fs = sweep_staging_on_exit(file_store, audit).await;
    let report = teardown_report(ending, fs);
    send_cleanup_report(&mut channel, session, &report).await;
    let _ = send_control(&mut channel, session, ControlMessage::Revoke { reason }).await;
    let _ = timeout(TEARDOWN_DRAIN, drain_until_closed(&mut channel)).await;
    // Activity shed since the last marker is shown before the end.
    activity.flush_final(tx);
    finish_with_report(channel, shared, tx, audit, Actor::Agent, ending, report);
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    // A sustained output flood must never grow the console buffer past its bound,
    // must keep headroom for lifecycle/terminal events, and must surface every
    // shed byte as an ExecOutputDropped marker rather than silently truncating
    // (REQ-N-006 bounded resource + backpressure, REQ-N-008 honesty).
    #[tokio::test]
    async fn console_output_is_bounded_and_loss_is_marked() {
        let cap = SESSION_EVENT_CHANNEL_CAPACITY;
        let (tx, mut rx) = mpsc::channel::<SessionEvent>(cap);
        let mut dropped: BTreeMap<JobId, u64> = BTreeMap::new();
        let job = JobId(7);

        // Flood far past the buffer without ever draining it.
        for _ in 0..(cap * 4) {
            deliver_output(
                &tx,
                &mut dropped,
                job,
                OutputStream::Stdout,
                vec![b'x'; 512],
            );
        }

        // Hard bound: the buffer never exceeds its capacity, regardless of flood.
        assert!(
            rx.len() <= cap,
            "buffer exceeded its bound: {} > {cap}",
            rx.len()
        );
        // Reserve kept headroom free so lifecycle/terminal events are never
        // crowded out by streamed output.
        assert!(
            tx.capacity() >= 1,
            "no headroom left for lifecycle events (reserve failed)"
        );

        // Drain the buffered events; loss must be surfaced, not silent.
        let mut output_chunks = 0usize;
        let mut marked_bytes = 0u64;
        while let Ok(event) = rx.try_recv() {
            match event {
                SessionEvent::ExecOutput { .. } => output_chunks += 1,
                SessionEvent::ExecOutputDropped { bytes, .. } => marked_bytes += bytes,
                _ => {}
            }
        }
        // Flush any residual accounting now that the channel has room.
        flush_dropped(&tx, &mut dropped);
        while let Ok(event) = rx.try_recv() {
            if let SessionEvent::ExecOutputDropped { bytes, .. } = event {
                marked_bytes += bytes;
            }
        }

        assert!(output_chunks > 0, "some output should have been delivered");
        assert!(marked_bytes > 0, "shed output must be marked (REQ-N-008)");
        assert!(
            dropped.is_empty(),
            "delivered markers clear the accumulator"
        );
    }

    // Activity shed at the very end of a session is shown as a gap before the
    // terminal event rather than lost with the session (ST-M009-011,
    // REQ-N-008), and never takes the terminal event's slot.
    #[tokio::test]
    async fn a_pending_shed_count_is_delivered_before_the_end() {
        let cap = SESSION_EVENT_CHANNEL_CAPACITY;
        let (tx, mut rx) = mpsc::channel::<SessionEvent>(cap);
        // Fill the channel down to the lifecycle reserve.
        while tx.capacity() > LIFECYCLE_RESERVE {
            tx.try_send(SessionEvent::Active).unwrap();
        }
        let mut tap = ActivityTap::default();
        for _ in 0..3 {
            tap.emit(&tx, SessionEvent::ActivityShed { count: 99 });
        }
        assert_eq!(tap.shed, 3, "activity is shed into the reserve");
        tap.flush(&tx);
        assert_eq!(tap.shed, 3, "the ordinary flush keeps the reserve free");
        // The end: the count goes out, using the reserve.
        tap.flush_final(&tx);
        assert_eq!(tap.shed, 0);
        let mut delivered = Vec::new();
        while let Ok(event) = rx.try_recv() {
            if let SessionEvent::ActivityShed { count } = event {
                delivered.push(count);
            }
        }
        assert_eq!(delivered, vec![3]);

        // With one slot left, it stays for the terminal event.
        while tx.capacity() > 1 {
            tx.try_send(SessionEvent::Active).unwrap();
        }
        tap.shed = 2;
        tap.flush_final(&tx);
        assert_eq!(tap.shed, 2);
        assert_eq!(tx.capacity(), 1);
    }

    // Under no pressure, output is delivered verbatim and in order, and no drop
    // markers are produced — the common case must be lossless.
    #[tokio::test]
    async fn console_output_is_lossless_when_drained() {
        let (tx, mut rx) = mpsc::channel::<SessionEvent>(SESSION_EVENT_CHANNEL_CAPACITY);
        let mut dropped: BTreeMap<JobId, u64> = BTreeMap::new();
        let job = JobId(1);

        for i in 0..1000u32 {
            deliver_output(
                &tx,
                &mut dropped,
                job,
                OutputStream::Stdout,
                vec![i as u8; 8],
            );
            // Drain immediately, as a keeping-up consumer would.
            match rx.try_recv() {
                Ok(SessionEvent::ExecOutput { bytes, .. }) => assert_eq!(bytes, vec![i as u8; 8]),
                other => panic!("expected in-order output, got {other:?}"),
            }
        }
        assert!(dropped.is_empty(), "a keeping-up consumer loses nothing");
        assert!(rx.try_recv().is_err(), "no extra events");
    }

    // An audit sink that records the (action, outcome) pairs it is given so a
    // test can assert the honest cleanup audit is emitted on the way out.
    #[derive(Debug, Default)]
    struct RecordingAudit {
        records: std::sync::Mutex<Vec<(&'static str, Outcome)>>,
    }

    impl SessionAudit for RecordingAudit {
        fn record(&self, _actor: Actor, action: &'static str, outcome: Outcome) {
            self.records.lock().unwrap().push((action, outcome));
        }
    }

    /// One answered step of a folder removal, as the reply tap notes it.
    fn step(state: TreeState, first_failure: Option<ErrorCode>) -> Option<RemovalStep> {
        Some(RemovalStep {
            state,
            first_failure,
        })
    }

    /// Reports one removal step on `path` the way the request handler does.
    fn report_step(
        tap: &mut ActivityTap,
        tx: &mpsc::Sender<SessionEvent>,
        audit: &RecordingAudit,
        path: &str,
        outcome: ActionOutcome,
        step: Option<RemovalStep>,
    ) {
        tap.removal(
            tx,
            audit,
            Attribution::default(),
            format!("delete the folder {path} and everything in it"),
            Some(path.to_owned()),
            outcome,
            step,
            Instant::now(),
        );
    }

    /// The `(id, outcome)` of every activity item delivered so far.
    fn handled(rx: &mut mpsc::Receiver<SessionEvent>) -> Vec<(u64, ActionOutcome)> {
        let mut out = Vec::new();
        while let Ok(event) = rx.try_recv() {
            if let SessionEvent::RequestHandled { id, outcome, .. } = event {
                out.push((id, outcome));
            }
        }
        out
    }

    // A folder removal is one item however many steps it takes, and it ends
    // in the state the folder is really in: done only when the folder is gone,
    // failed with the first reason when something was left, cancelled when the
    // caller stopped it (ST-M010-001, REQ-N-008). The audit trail records its
    // start once and its end once.
    #[tokio::test]
    async fn a_folder_removal_is_one_item_that_ends_in_its_honest_state() {
        for (last, ended, audited) in [
            (
                step(TreeState::Removed, None),
                ActionOutcome::Succeeded,
                Outcome::Ok,
            ),
            (
                step(TreeState::Incomplete, Some(ErrorCode::FileInUse)),
                ActionOutcome::Failed {
                    code: ErrorCode::FileInUse,
                },
                Outcome::Failed,
            ),
            (
                step(TreeState::Blocked, Some(ErrorCode::AccessDenied)),
                ActionOutcome::Failed {
                    code: ErrorCode::AccessDenied,
                },
                Outcome::Failed,
            ),
            (
                step(TreeState::Stopped, None),
                ActionOutcome::Cancelled,
                Outcome::Cancelled,
            ),
        ] {
            let (tx, mut rx) = mpsc::channel::<SessionEvent>(SESSION_EVENT_CHANNEL_CAPACITY);
            let audit = RecordingAudit::default();
            let mut tap = ActivityTap::default();
            let more = step(TreeState::More, None);
            // Three steps that leave more to do, then the one that ends it.
            for _ in 0..3 {
                report_step(
                    &mut tap,
                    &tx,
                    &audit,
                    "C:\\old",
                    ActionOutcome::Succeeded,
                    more,
                );
            }
            report_step(
                &mut tap,
                &tx,
                &audit,
                "C:\\old",
                ActionOutcome::Succeeded,
                last,
            );

            assert_eq!(
                handled(&mut rx),
                vec![(1, ActionOutcome::InProgress), (1, ended)],
                "{last:?}"
            );
            assert_eq!(
                *audit.records.lock().unwrap(),
                vec![
                    ("fs_remove_tree_start", Outcome::Ok),
                    ("fs_remove_tree", audited)
                ],
                "{last:?}"
            );
            assert!(tap.removing.is_empty(), "an ended removal is forgotten");
        }
    }

    // A step the agent refused, or one whose answer never left, ends the
    // removal as failed or as not known: never as done.
    #[tokio::test]
    async fn a_refused_or_unanswered_removal_step_never_ends_as_done() {
        for (outcome, audited) in [
            (
                ActionOutcome::Failed {
                    code: ErrorCode::PathNotFound,
                },
                Outcome::Failed,
            ),
            (ActionOutcome::Unknown, Outcome::Unknown),
        ] {
            let (tx, mut rx) = mpsc::channel::<SessionEvent>(SESSION_EVENT_CHANNEL_CAPACITY);
            let audit = RecordingAudit::default();
            let mut tap = ActivityTap::default();
            report_step(
                &mut tap,
                &tx,
                &audit,
                "C:\\old",
                ActionOutcome::Succeeded,
                step(TreeState::More, None),
            );
            report_step(&mut tap, &tx, &audit, "C:\\old", outcome, None);

            assert_eq!(
                handled(&mut rx),
                vec![(1, ActionOutcome::InProgress), (1, outcome)]
            );
            assert_eq!(
                audit.records.lock().unwrap().last(),
                Some(&("fs_remove_tree", audited))
            );
        }

        // An answer that is not a removal step says nothing about the folder.
        let (tx, mut rx) = mpsc::channel::<SessionEvent>(SESSION_EVENT_CHANNEL_CAPACITY);
        let audit = RecordingAudit::default();
        let mut tap = ActivityTap::default();
        report_step(
            &mut tap,
            &tx,
            &audit,
            "C:\\old",
            ActionOutcome::Succeeded,
            None,
        );
        assert_eq!(handled(&mut rx), vec![(1, ActionOutcome::Unknown)]);
    }

    // The removals the agent keeps open are bounded: one more than the bound
    // closes the oldest as "result not known", and a session that ends records
    // every removal still open the same way (REQ-N-006, REQ-N-008).
    #[tokio::test]
    async fn open_folder_removals_are_bounded_and_closed_honestly() {
        // Room for every item: the bound under test is the bookkeeping's.
        let (tx, mut rx) = mpsc::channel::<SessionEvent>(4 * MAX_OPEN_REMOVALS);
        let audit = RecordingAudit::default();
        let mut tap = ActivityTap::default();
        let more = step(TreeState::More, None);
        for n in 0..=MAX_OPEN_REMOVALS {
            report_step(
                &mut tap,
                &tx,
                &audit,
                &format!("C:\\folder-{n}"),
                ActionOutcome::Succeeded,
                more,
            );
        }

        assert_eq!(tap.removing.len(), MAX_OPEN_REMOVALS);
        assert!(
            !tap.removing.contains_key("C:\\folder-0"),
            "the oldest gave way"
        );
        let items = handled(&mut rx);
        assert_eq!(
            items.iter().filter(|(id, _)| *id == 1).collect::<Vec<_>>(),
            vec![
                &(1, ActionOutcome::InProgress),
                &(1, ActionOutcome::Unknown)
            ],
            "it is closed as not known, never as done"
        );

        audit.records.lock().unwrap().clear();
        tap.close_removals(&audit);
        assert!(tap.removing.is_empty());
        let records = audit.records.lock().unwrap();
        assert_eq!(records.len(), MAX_OPEN_REMOVALS);
        assert!(
            records
                .iter()
                .all(|record| *record == ("fs_remove_tree", Outcome::Unknown))
        );
    }

    // --- an answer that cannot be sent (ST-M010-005) ------------------------

    /// A sink that takes a frame as the secure channel does: one that breaks
    /// a bound of the protocol, or that is larger than one record, is refused
    /// before anything is sent, and the sink is as it was.
    #[derive(Debug, Default)]
    struct RecordSink {
        sent: Vec<Message>,
        lost: bool,
    }

    impl FrameSink for RecordSink {
        async fn send_frame(&mut self, message: &Message) -> Result<(), Unsent> {
            if self.lost {
                return Err(Unsent::of(&SecureChannelError::Eof, true));
            }
            let payload = match message.encode() {
                Ok(payload) => payload,
                Err(error) => return Err(Unsent::of(&SecureChannelError::Decode(error), false)),
            };
            if payload.len() > limits::MAX_RECORD_BYTES {
                let oversized = SecureChannelError::Oversized {
                    len: payload.len(),
                    max: limits::MAX_RECORD_BYTES,
                };
                return Err(Unsent::of(&oversized, false));
            }
            self.sent.push(message.clone());
            Ok(())
        }
    }

    /// A file read of `len` bytes, as the agent answers it.
    fn file_data(len: usize) -> OpResult {
        OpResult::FileData {
            data: B64Bytes::new(vec![0x5A; len]),
            eof: true,
            file_len: len as u64,
        }
    }

    /// The one error the sink took, as (request, session, code, message,
    /// stage).
    fn the_error(
        sink: &RecordSink,
    ) -> (
        Option<RequestId>,
        Option<SessionId>,
        ErrorCode,
        String,
        ErrorStage,
    ) {
        match sink.sent.as_slice() {
            [
                Message::Error {
                    id,
                    session,
                    code,
                    message,
                    stage,
                },
            ] => (*id, *session, *code, message.clone(), *stage),
            other => panic!("expected one error and nothing else, got {other:?}"),
        }
    }

    // A frame that the channel refused before it touched the stream is told
    // apart from a channel that is gone: only the first leaves the session
    // usable.
    #[test]
    fn a_frame_refused_before_the_wire_is_told_apart_from_a_lost_channel() {
        let too_large = SecureChannelError::Oversized {
            len: 70_000,
            max: 1,
        };
        assert_eq!(
            Unsent::of(&too_large, false),
            Unsent::Refused(Refusal {
                code: ErrorCode::OversizedMessage,
                stage: ErrorStage::Transport,
            })
        );
        let out_of_bounds =
            SecureChannelError::Decode(cretlink_protocol::ProtocolError::LimitExceeded {
                field: "file_entry name length",
                len: 2,
                max: 1,
            });
        assert_eq!(
            Unsent::of(&out_of_bounds, false),
            Unsent::Refused(Refusal {
                code: ErrorCode::LimitExceeded,
                stage: ErrorStage::Validation,
            })
        );
        let frame_too_large =
            SecureChannelError::Decode(cretlink_protocol::ProtocolError::Oversized {
                len: 2,
                max: 1,
            });
        assert_eq!(
            Unsent::of(&frame_too_large, false),
            Unsent::Refused(Refusal {
                code: ErrorCode::OversizedMessage,
                stage: ErrorStage::Transport,
            })
        );
        // Whatever the error says, a poisoned channel is gone; and so is one
        // whose stream failed.
        for error in [too_large, out_of_bounds, frame_too_large] {
            assert_eq!(Unsent::of(&error, true), Unsent::Lost);
        }
        for error in [SecureChannelError::Eof, SecureChannelError::Poisoned] {
            assert_eq!(Unsent::of(&error, false), Unsent::Lost);
            assert_eq!(Unsent::of(&error, true), Unsent::Lost);
        }
    }

    // An answer that fits is sent as it is.
    #[tokio::test]
    async fn an_answer_that_fits_is_sent() {
        let session = SessionId::new();
        let mut sink = RecordSink::default();
        let answered = answer(&mut sink, session, RequestId(7), file_data(1024)).await;
        assert_eq!(answered, Answer::Sent);
        assert!(matches!(answered.step(), AgentStep::Continue));
        assert_eq!(
            sink.sent,
            vec![Message::Response {
                id: RequestId(7),
                session,
                result: file_data(1024),
            }]
        );
    }

    // An answer that is larger than one record of the secure channel is not
    // sent, and the session does not end over it: the request is failed with
    // the reason, in fixed words, and the agent goes on.
    #[tokio::test]
    async fn an_answer_larger_than_a_record_fails_its_request_and_the_session_goes_on() {
        let session = SessionId::new();
        let mut sink = RecordSink::default();
        // Within every bound of the protocol, and far larger than a record
        // once it is encoded.
        let large = file_data(limits::MAX_CHUNK_BYTES);

        let answered = answer(&mut sink, session, RequestId(7), large).await;

        assert_eq!(answered, Answer::Failed);
        assert!(matches!(answered.step(), AgentStep::Continue));
        let (id, stamped, code, message, stage) = the_error(&sink);
        assert_eq!((id, stamped), (Some(RequestId(7)), Some(session)));
        assert_eq!(code, ErrorCode::OversizedMessage);
        assert_eq!(stage, ErrorStage::Transport);
        assert_eq!(message, ANSWER_TOO_LARGE);
    }

    // The same for an answer that breaks a bound of the protocol: here a name
    // longer than a name may be.
    #[tokio::test]
    async fn an_answer_that_breaks_a_bound_fails_its_request_and_the_session_goes_on() {
        let session = SessionId::new();
        let mut sink = RecordSink::default();
        let out_of_bounds = OpResult::DirEntries {
            entries: vec![cretlink_protocol::FileEntry {
                name: "n".repeat(limits::MAX_NAME_BYTES + 1),
                kind: cretlink_protocol::FileKind::File,
                len: 1,
                modified_ms: None,
                readonly: false,
            }],
            end: true,
        };

        let answered = answer(&mut sink, session, RequestId(9), out_of_bounds).await;

        assert_eq!(answered, Answer::Failed);
        let (id, stamped, code, message, stage) = the_error(&sink);
        assert_eq!((id, stamped), (Some(RequestId(9)), Some(session)));
        assert_eq!(code, ErrorCode::LimitExceeded);
        assert_eq!(stage, ErrorStage::Validation);
        assert_eq!(message, ANSWER_OUT_OF_BOUNDS);
    }

    // The words that fail such a request are themselves an answer that fits,
    // whatever the request identifier.
    #[test]
    fn the_reason_of_an_answer_that_cannot_be_sent_always_fits() {
        for (code, reason, stage) in [
            (
                ErrorCode::OversizedMessage,
                ANSWER_TOO_LARGE,
                ErrorStage::Transport,
            ),
            (
                ErrorCode::LimitExceeded,
                ANSWER_OUT_OF_BOUNDS,
                ErrorStage::Validation,
            ),
        ] {
            let encoded = Message::Error {
                id: Some(RequestId(u64::MAX)),
                session: Some(SessionId::new()),
                code,
                message: reason.to_string(),
                stage,
            }
            .encode()
            .expect("within the bounds of the wire");
            assert!(encoded.len() < 1024, "{} bytes", encoded.len());
            assert!(!reason.contains(['\n', '\r']), "one line: {reason:?}");
        }
    }

    // A channel that is gone is a disconnect, for the answer and for the
    // error that would have failed the request.
    #[tokio::test]
    async fn an_answer_on_a_lost_channel_is_a_disconnect() {
        let mut sink = RecordSink {
            lost: true,
            ..RecordSink::default()
        };
        let answered = answer(&mut sink, SessionId::new(), RequestId(1), file_data(16)).await;
        assert_eq!(answered, Answer::Lost);
        assert!(matches!(answered.step(), AgentStep::Disconnected));
        assert!(sink.sent.is_empty());
    }

    // The customer is shown what the console was told: a request whose answer
    // could not be sent is a failed one, with the reason, and what the unsent
    // answer said about a file leaving the computer is not kept.
    #[tokio::test]
    async fn the_customer_sees_a_request_whose_answer_could_not_be_sent_as_failed() {
        let session = SessionId::new();
        let mut sink = RecordSink::default();
        let mut tap = ReplyTap::new(&mut sink, RequestId(3));

        let answered = answer(
            &mut tap,
            session,
            RequestId(3),
            file_data(limits::MAX_CHUNK_BYTES),
        )
        .await;

        assert_eq!(answered, Answer::Failed);
        assert_eq!(
            tap.outcome(),
            ActionOutcome::Failed {
                code: ErrorCode::OversizedMessage
            }
        );
        assert_eq!(tap.read, None, "nothing of the file left the computer");

        // An answer that was sent is noted as it always was.
        let mut tap = ReplyTap::new(&mut sink, RequestId(4));
        let answered = answer(&mut tap, session, RequestId(4), file_data(64)).await;
        assert_eq!(answered, Answer::Sent);
        assert_eq!(tap.outcome(), ActionOutcome::Succeeded);
        assert_eq!(
            tap.read,
            Some(ReadData {
                len: 64,
                eof: true,
                file_len: 64
            })
        );
    }

    // The agent must autonomously sweep its own staging area when the session
    // ends: an OnSessionEnd artifact never outlives the session, the teardown
    // report stays clean, and the sweep is audited as a successful fs_cleanup.
    // This is exactly what every agent teardown path runs via
    // `agent_active_finish` (ST-M003-005 AC5a, REQ-F-019, REQ-N-008).
    #[tokio::test]
    async fn staging_is_swept_autonomously_on_session_end() {
        use cretlink_exec::FileStore;
        use cretlink_protocol::{CleanupPolicy, DigestAlgo, RemotePath};
        use sha2::{Digest, Sha256};

        let session = SessionId::new();
        // Root the staging tree in a unique per-session temp dir so the test is
        // hermetic and safe to run in parallel.
        let base = std::env::temp_dir().join(format!("cretlink-sweep-{session}"));
        let store = FileStore::with_base(session, base.clone());

        // Stage an OnSessionEnd artifact: append the bytes, then commit with the
        // verified length and SHA-256 digest (the only wire path to a final
        // filename).
        let data = b"driver package payload".to_vec();
        let digest = Sha256::digest(&data).to_vec();
        let name = "driver.bin".to_string();

        store
            .perform(Operation::FsWrite {
                dest: RemotePath::Staging { name: name.clone() },
                offset: 0,
                data: B64Bytes::new(data.clone()),
            })
            .await
            .expect("staged write accepted");
        store
            .perform(Operation::FsCommit {
                dest: RemotePath::Staging { name: name.clone() },
                len: data.len() as u64,
                algo: DigestAlgo::Sha256,
                digest: B64Bytes::new(digest),
                overwrite: false,
                source: Some("tech:test".to_string()),
                cleanup: CleanupPolicy::OnSessionEnd,
            })
            .await
            .expect("staged commit succeeded");

        let committed = store.staging_root().join(&name);
        assert!(committed.exists(), "the artifact exists before the sweep");

        // The autonomous session-end sweep removes it and reports an honest,
        // clean outcome.
        let audit = RecordingAudit::default();
        let fs = sweep_staging_on_exit(&store, &audit).await;

        assert!(fs.ok, "a clean sweep is ok");
        assert!(
            fs.notes.is_empty(),
            "a clean sweep leaves no residual notes"
        );
        assert!(
            !committed.exists(),
            "the staged artifact is gone after the sweep"
        );
        assert!(
            audit
                .records
                .lock()
                .unwrap()
                .iter()
                .any(|&(action, outcome)| action == "fs_cleanup" && outcome == Outcome::Ok),
            "the sweep audits a successful fs_cleanup"
        );

        // Best-effort teardown of the unique temp tree.
        let _ = std::fs::remove_dir_all(&base);
    }
}
