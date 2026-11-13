//! The console session service: one owner of a live [`HostHandle`] that both
//! the interactive CLI and the MCP bridge drive concurrently (ST-M004-001,
//! ST-M004-002, ST-M004-004).
//!
//! A [`HostHandle`]'s event stream has a single consumer (`&mut self`), yet the
//! console must serve both a human operator (live streamed output) and MCP tool
//! calls (collected [`ExecReport`]s) against the *same* selected session. This
//! service resolves that by owning the handle inside one driver task that:
//!
//! - pumps every [`SessionEvent`] into a bounded broadcast, so any number of
//!   consumers observe the same stream (the CLI renders it live; the bridge
//!   collects a job's output by id); and
//! - executes command requests (`run_command`, `write_stdin`, `cancel_job`,
//!   `list_jobs`, `stop`) on the handle, correlated by one-shot replies.
//!
//! # The technician's file explorer (ST-M010-001, ST-M010-002)
//!
//! Beside the MCP-facing file surface, which attributes every call to the AI,
//! the service offers the operations of the technician's own file explorer,
//! attributed to the technician ([`Origin::Console`]):
//! [`ConsoleSessionService::list_drives`],
//! [`ConsoleSessionService::list_folder`],
//! [`ConsoleSessionService::remove_tree`] and
//! [`ConsoleSessionService::send_to_folder`]. The last two can run long, so
//! the caller hands in a [`CancelToken`] to cancel them and may hand in a
//! [`ProgressSink`]; a revoke or stop cancels them like any transfer. They add
//! no authority: they work only on the consented session and fail closed with
//! it. The MCP tool surface does not offer them.
//!
//! The service also keeps, for its session only, what the target has
//! confirmed about the places the explorer went to (see [`crate::explorer`]).
//!
//! The service does not write the audit trail; whoever drives an action
//! records it. For these that is [`crate::TechnicianFiles`], which a
//! screen calls instead of the service: it records each action before and
//! after it runs.
//!
//! # Fail-closed (REQ-S-009, ST-M004-004)
//!
//! The service is the routing authority's local face. It is created only for an
//! **active, consented** session and flips `active` to `false` the instant a
//! revoke/stop is requested — before the asynchronous teardown completes — and
//! whenever the session ends. Once inactive, every routed call returns
//! [`TargetError::NoConsentedSession`] and nothing runs. New submissions are
//! refused at the entry point, and a command that was already queued when the
//! flip happened is re-checked at dequeue and dropped (its reply is
//! `NotActive`), so a not-yet-started request can never reach a revoked session.
//! A command already mid-execution when the flip lands runs to its terminal
//! event, which `collect` reports honestly (never a fabricated success).

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use cretlink_mcp::{
    CaptureParams, CaptureReport, CleanupSummary, DirPage, DownloadParams, DownloadReport,
    ExecParams, ExecReport, StatReport, SystemInfoReport, TargetError, TransferFailure,
    UploadParams, UploadReport,
};
use cretlink_protocol::{
    ErrorCode, ErrorStage, JobId, JobOutcome, JobSummary, KeyAction, PointerAction, PointerButton,
    SessionId, Shell, TargetIdentity,
};
use cretlink_session::{
    Attribution, CancelToken, CleanupPolicy, CleanupTrigger, DownloadRequest, DriveEntry,
    ExecRequest as HostExecRequest, HostHandle, OpResult, Operation, Origin, OutputStream, Purpose,
    RemotePath, RevokeReason, SessionError as HostSessionError, SessionEvent, StagedEntry,
    StartedJob, TransferProgress, TransferReport, TreeRemovalProgress, TreeRemovalReport,
    UploadRequest,
};
use tokio::sync::mpsc::error::TrySendError;
use tokio::sync::{broadcast, mpsc, oneshot};
use tokio::task::JoinHandle;

use crate::explorer::{Browsed, FolderSource};

/// Capacity of the broadcast event stream. Each `ExecOutput` event carries at
/// most one wire output chunk (~32 KiB), so buffered output is bounded at
/// roughly `EVENT_CAPACITY * 32 KiB`; a consumer that overruns this window is
/// reported honestly as truncation, never a hang (REQ-N-006, REQ-N-008).
const EVENT_CAPACITY: usize = 512;

/// The maximum number of interactive terminals one consented session may hold
/// open at once (ST-M003-009). Enforced fail-closed at open time via a
/// reservation, so concurrent opens can never exceed it. A terminal is a job,
/// so it also counts against the agent's own job bounds; this is the
/// console-side ceiling on the terminal registry.
pub const MAX_TERMINALS: usize = 8;

/// The maximum number of local consumers that may attach to a single terminal
/// (the tmux model, ST-M003-009). Bounds the per-terminal fan-out set; a
/// further attach is refused with an honest error.
pub const MAX_TERM_CONSUMERS: usize = 8;

/// Capacity of one attached terminal consumer's bounded output channel. Each
/// queued item is at most one wire output chunk, so a consumer's buffered VT
/// output is bounded. A consumer that overruns this window is **disconnected**
/// with an honest `Dropped` frame rather than corrupting the shared VT stream
/// by dropping bytes for the other consumers (REQ-N-006).
const TERM_CONSUMER_CAPACITY: usize = 256;

/// Where a long operation reports its progress, when the caller wants to
/// follow it (ST-M010-001). It is called on the service's driver task after
/// each step the target answered, so it must return quickly and never wait
/// for the service. It may cancel the operation's [`CancelToken`] or stop the
/// session: neither waits.
pub type ProgressSink<T> = Box<dyn FnMut(T) + Send>;

/// A file to send from the technician computer into a folder of the target
/// (ST-M010-001).
///
/// The file is committed in that folder under `name` only after its length
/// and SHA-256 are verified, on whatever drive the folder lies. It is the
/// customer's to keep: it is not staged, and no cleanup removes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SendToFolder {
    /// The file on the technician computer to read.
    pub local_path: PathBuf,
    /// The folder on the target that receives the file: an absolute remote
    /// path such as `C:\Users\ann\Documents`, or a drive root.
    pub folder: String,
    /// The name the file gets in that folder: one name, not a path.
    pub name: String,
    /// Whether a file of that name that is already there may be replaced.
    /// Without it such a file fails the transfer and is left as it is.
    pub overwrite: bool,
}

impl SendToFolder {
    /// The remote path the file is committed at: `name` in `folder`.
    ///
    /// # Errors
    ///
    /// An `invalid_path` [`TargetError`] when `folder` is not a rooted path
    /// on a drive, when `name` is not one name — it holds a separator, so it
    /// would reach into another folder — or when the resulting path breaks
    /// the rules for remote paths. Nothing is sent then.
    pub fn dest(&self) -> Result<RemotePath, TargetError> {
        let invalid = |message: String| TargetError::Remote {
            code: ErrorCode::InvalidPath,
            message,
            stage: ErrorStage::Validation,
        };
        // The folder is checked on its own first: joined to a name, a bare
        // drive (`C:`) would read as the root of that drive, which is another
        // folder than the one it names.
        cretlink_protocol::validate_remote_absolute_path("folder", &self.folder)
            .map_err(|error| invalid(error.to_string()))?;
        if self.name.is_empty() || self.name.contains(['\\', '/']) {
            return Err(invalid(
                "the name of the file must be one name, without a folder".to_string(),
            ));
        }
        let folder = self.folder.trim_end_matches(['\\', '/']);
        let path = format!("{folder}\\{}", self.name);
        cretlink_protocol::validate_remote_absolute_path("dest", &path)
            .map_err(|error| invalid(error.to_string()))?;
        Ok(RemotePath::Absolute { path })
    }
}

/// A command the service asks its driver task to run on the owned handle.
enum DriverCommand {
    Run {
        request: HostExecRequest,
        /// Which local actor drives this command (console or the AI via the MCP
        /// bridge) and, for the AI, its stated purpose, stamped onto the wire
        /// request (ST-M004-006, ST-M009-007, REQ-F-017, REQ-F-026).
        attribution: Attribution,
        reply: oneshot::Sender<Result<StartedJob, HostSessionError>>,
    },
    /// A live target/context inspection, answered by the agent for this very
    /// request — never from the consent-time cache (ST-M003-003).
    SystemInfo {
        attribution: Attribution,
        reply: oneshot::Sender<Result<(TargetIdentity, bool), HostSessionError>>,
    },
    WriteStdin {
        job: JobId,
        data: Vec<u8>,
        attribution: Attribution,
        reply: oneshot::Sender<Result<(), HostSessionError>>,
    },
    Cancel {
        job: JobId,
        attribution: Attribution,
        reply: oneshot::Sender<Result<(), HostSessionError>>,
    },
    ListJobs {
        attribution: Attribution,
        reply: oneshot::Sender<Result<Vec<JobSummary>, HostSessionError>>,
    },
    /// A single bounded file operation (stat, list, create/remove dir, move,
    /// delete, list-staged, cleanup) — a fast request/response the driver runs
    /// on the owned handle and answers over `reply` (ST-M004-003).
    FileOp {
        op: Operation,
        attribution: Attribution,
        reply: oneshot::Sender<Result<OpResult, HostSessionError>>,
    },
    /// A resumable, verified upload driven inline by the transfer engine
    /// (ST-M004-003, REQ-N-007). The driver publishes the [`CancelToken`] into
    /// the transfer gate — the caller's own when it handed one in, else a
    /// fresh one — so a concurrent revoke aborts it within one chunk.
    Upload {
        request: UploadRequest,
        attribution: Attribution,
        cancel: Option<CancelToken>,
        progress: Option<ProgressSink<TransferProgress>>,
        reply: oneshot::Sender<Result<TransferReport, HostSessionError>>,
    },
    /// The removal of a folder with its content, driven inline by the removal
    /// engine in bounded steps (ST-M010-001). Its [`CancelToken`] is published
    /// into the transfer gate like a transfer's, so a revoke ends it between
    /// two steps.
    RemoveTree {
        path: RemotePath,
        attribution: Attribution,
        cancel: CancelToken,
        progress: Option<ProgressSink<TreeRemovalProgress>>,
        reply: oneshot::Sender<Result<TreeRemovalReport, HostSessionError>>,
    },
    /// A resumable, verified download driven inline by the transfer engine
    /// (ST-M004-003, REQ-N-007).
    Download {
        request: DownloadRequest,
        attribution: Attribution,
        reply: oneshot::Sender<Result<TransferReport, HostSessionError>>,
    },
    Stop {
        reason: RevokeReason,
    },
    /// Open an interactive terminal (a `JobMode::Interactive` job) on the owned
    /// handle, stamped [`Origin::Terminal`] (ST-M003-009).
    OpenTerminal {
        shell: Shell,
        cols: u16,
        rows: u16,
        working_dir: Option<String>,
        env: BTreeMap<String, String>,
        reply: oneshot::Sender<Result<JobId, HostSessionError>>,
    },
    /// Resize a running terminal's pseudo-console (last-writer-wins). An
    /// out-of-bounds size is rejected locally by the handle, never acted on.
    ResizeTerminal {
        job: JobId,
        cols: u16,
        rows: u16,
        reply: oneshot::Sender<Result<(), HostSessionError>>,
    },
    /// Close a terminal (the same teardown as cancelling its job). Its honest
    /// exit still arrives as a terminal `ExecFinished`.
    CloseTerminal {
        job: JobId,
        reply: oneshot::Sender<Result<(), HostSessionError>>,
    },
}

/// One event delivered to a single attached terminal consumer over its bounded
/// channel (ST-M003-009). The IPC server maps each onto a wire
/// [`cretlink_mcp::TermFrame`]; the console never fabricates a clean close
/// (REQ-N-008).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TermEvent {
    /// A chunk of the terminal's merged output (raw VT pass-through, ADR-0010).
    /// Every attached consumer of the terminal sees the full shared stream,
    /// including the pty echo of any consumer's input.
    Output(Vec<u8>),
    /// The terminal (job) exited with an honest outcome and exit code, if any.
    Exited {
        /// The process exit code, when one exists.
        exit_code: Option<i32>,
        /// The honest terminal outcome (never a fabricated success).
        outcome: JobOutcome,
    },
    /// The session was revoked or stopped while attached: a fail-closed teardown
    /// (REQ-S-004, REQ-S-009).
    Revoked,
}

/// A single consumer's attachment to a terminal (ST-M003-009).
///
/// Returned by [`ConsoleSessionService::term_attach`] and consumed by the IPC
/// server's duplex writer. Attaching grants **no new authority**: it only adds a
/// local consumer of the already-consented session's terminal output; input,
/// resize, and close all still route through the same fail-closed session
/// surface.
#[derive(Debug)]
pub struct TermAttachment {
    /// The routed session (REQ-F-016).
    pub session: SessionId,
    /// The routed target identity (REQ-F-016).
    pub target: TargetIdentity,
    /// The attached terminal's job id.
    pub term: JobId,
    /// This consumer's id, used to detach exactly this consumer.
    pub consumer: u64,
    /// The bounded per-consumer stream of terminal events.
    pub events: mpsc::Receiver<TermEvent>,
    /// Set true when this consumer was disconnected for overrunning its bounded
    /// window (or an upstream drop), so the writer emits an honest `Dropped`
    /// frame rather than a fabricated clean close (REQ-N-006, REQ-N-008).
    pub overrun: Arc<AtomicBool>,
}

/// Why a [`TerminalRegistry::attach`] was refused.
#[derive(Debug)]
enum AttachRejection {
    /// No such open terminal (unknown or already exited).
    Unknown,
    /// The terminal already has [`MAX_TERM_CONSUMERS`] consumers.
    TooMany,
}

/// A single attached consumer's delivery handle inside the registry.
struct Consumer {
    id: u64,
    events: mpsc::Sender<TermEvent>,
    overrun: Arc<AtomicBool>,
}

/// One open terminal and its attached consumers (the tmux fan-out set).
#[derive(Default)]
struct Terminal {
    consumers: Vec<Consumer>,
}

/// The per-session terminal registry: open terminals keyed by their job id, each
/// with its bounded set of attached consumers, plus a reservation count so
/// concurrent opens can never exceed [`MAX_TERMINALS`] (ST-M003-009).
///
/// The VT stream is fanned out over **per-consumer bounded mpsc channels**, not
/// the shared broadcast: a slow consumer is disconnected honestly rather than
/// corrupting the stream for the others via a broadcast lag-drop (REQ-N-006).
#[derive(Default)]
struct TerminalRegistry {
    terminals: BTreeMap<JobId, Terminal>,
    /// Slots reserved by an in-flight open, released when it lands or fails.
    reserved: usize,
    /// Monotone consumer id source (never reused within a session).
    next_consumer: u64,
}

impl TerminalRegistry {
    /// Reserves one terminal slot fail-closed, or `false` when at capacity.
    fn try_reserve(&mut self) -> bool {
        if self.terminals.len() + self.reserved >= MAX_TERMINALS {
            false
        } else {
            self.reserved += 1;
            true
        }
    }

    /// Releases a reservation whose open failed.
    fn release(&mut self) {
        self.reserved = self.reserved.saturating_sub(1);
    }

    /// Records a newly opened terminal, consuming its reservation.
    fn insert(&mut self, term: JobId) {
        self.reserved = self.reserved.saturating_sub(1);
        self.terminals.entry(term).or_default();
    }

    /// Attaches a new consumer to `term`, returning its id, receiver, and
    /// overrun flag, or a rejection when the terminal is unknown or full.
    fn attach(
        &mut self,
        term: JobId,
    ) -> Result<(u64, mpsc::Receiver<TermEvent>, Arc<AtomicBool>), AttachRejection> {
        let terminal = self
            .terminals
            .get_mut(&term)
            .ok_or(AttachRejection::Unknown)?;
        if terminal.consumers.len() >= MAX_TERM_CONSUMERS {
            return Err(AttachRejection::TooMany);
        }
        let id = self.next_consumer;
        self.next_consumer += 1;
        let (events, rx) = mpsc::channel(TERM_CONSUMER_CAPACITY);
        let overrun = Arc::new(AtomicBool::new(false));
        terminal.consumers.push(Consumer {
            id,
            events,
            overrun: Arc::clone(&overrun),
        });
        Ok((id, rx, overrun))
    }

    /// Removes exactly one consumer; the terminal outlives zero consumers.
    fn detach(&mut self, term: JobId, consumer: u64) {
        if let Some(terminal) = self.terminals.get_mut(&term) {
            terminal.consumers.retain(|c| c.id != consumer);
        }
    }

    /// Fans one merged-output chunk out to every consumer of `term`. A consumer
    /// whose bounded window is full is dropped (marked overrun) so the shared
    /// stream is never corrupted for the others (REQ-N-006); the other
    /// consumers' independent channels are untouched.
    fn deliver_output(&mut self, term: JobId, bytes: &[u8]) {
        if let Some(terminal) = self.terminals.get_mut(&term) {
            terminal.consumers.retain(|c| {
                match c.events.try_send(TermEvent::Output(bytes.to_vec())) {
                    Ok(()) => true,
                    Err(TrySendError::Full(_)) => {
                        c.overrun.store(true, Ordering::SeqCst);
                        false
                    }
                    Err(TrySendError::Closed(_)) => false,
                }
            });
        }
    }

    /// Delivers the terminal's honest exit to every consumer, then removes it.
    fn deliver_exit(&mut self, term: JobId, exit_code: Option<i32>, outcome: JobOutcome) {
        if let Some(terminal) = self.terminals.remove(&term) {
            for c in terminal.consumers {
                if c.events
                    .try_send(TermEvent::Exited { exit_code, outcome })
                    .is_err()
                {
                    c.overrun.store(true, Ordering::SeqCst);
                }
            }
        }
    }

    /// Disconnects every consumer of `term` for an upstream (session-level) VT
    /// drop, keeping the terminal open with an empty consumer set so a fresh
    /// attach reconnects to a clean stream (REQ-N-006).
    fn drop_consumers(&mut self, term: JobId) {
        if let Some(terminal) = self.terminals.get_mut(&term) {
            for c in terminal.consumers.drain(..) {
                c.overrun.store(true, Ordering::SeqCst);
            }
        }
    }

    /// Tears every consumer of every terminal down with a `Revoked` frame
    /// (explicit revoke/stop), clearing the registry (REQ-S-004, REQ-S-009).
    fn revoke_all(&mut self) {
        for (_, terminal) in std::mem::take(&mut self.terminals) {
            for c in terminal.consumers {
                if c.events.try_send(TermEvent::Revoked).is_err() {
                    c.overrun.store(true, Ordering::SeqCst);
                }
            }
        }
    }

    /// Tears every remaining consumer down with an honest `Unknown` exit when
    /// the driver ends on a lost link, clearing the registry — never a
    /// fabricated clean close (REQ-N-008).
    fn disconnect_all(&mut self) {
        for (_, terminal) in std::mem::take(&mut self.terminals) {
            for c in terminal.consumers {
                if c.events
                    .try_send(TermEvent::Exited {
                        exit_code: None,
                        outcome: JobOutcome::Unknown,
                    })
                    .is_err()
                {
                    c.overrun.store(true, Ordering::SeqCst);
                }
            }
        }
    }

    /// Whether a terminal with this id is currently open.
    fn contains(&self, term: JobId) -> bool {
        self.terminals.contains_key(&term)
    }
}

/// The single point that serialises transfer-start against revoke.
///
/// A transfer runs inline in the driver and can span many chunks, so a revoke
/// arriving mid-transfer must be able to stop it promptly. Both the driver's
/// transfer-start decision and [`ConsoleSessionService::revoke`] make their full
/// decision while holding the one gate mutex: revoke flips `revoked` and cancels
/// `current`; the driver refuses to start when `revoked` and otherwise publishes
/// its token into `current` so a later revoke can reach it. This closes the
/// start/revoke race in both interleavings (REQ-S-004, REQ-S-009).
#[derive(Default)]
struct TransferGate {
    /// Set once a revoke/stop has been requested; no new transfer may start.
    revoked: bool,
    /// The in-flight transfer's cancel token, so revoke can abort it.
    current: Option<CancelToken>,
}

/// Cross-task shared state a service publishes and its callers read.
struct Inner {
    session: SessionId,
    target: TargetIdentity,
    active: AtomicBool,
    commands: mpsc::UnboundedSender<DriverCommand>,
    events: broadcast::Sender<SessionEvent>,
    /// Serialises transfer-start against revoke so an in-flight transfer is
    /// cancelled promptly and no new transfer starts after revoke.
    transfer_gate: Mutex<TransferGate>,
    /// The open interactive terminals and their attached consumers
    /// (ST-M003-009). The driver fans terminal output out to each consumer's
    /// bounded channel; the IPC server registers/removes consumers here.
    terminals: Mutex<TerminalRegistry>,
    /// The count of MCP tool calls the IPC server currently has in flight
    /// against this session, so the console can distinguish active AI control
    /// from an idle bridge connection (ST-M004-006). Shared with the IPC server,
    /// which increments/decrements it under an RAII guard; the CLI reads it.
    ai_in_flight: Arc<AtomicU64>,
    /// The last id minted for an action of the technician's own that the
    /// console audits as a start record and a completion record
    /// (ST-M010-001). One counter for the session, so two such actions never
    /// share an id whoever records them.
    technician_actions: AtomicU64,
    /// What the target has confirmed about the places the technician's
    /// explorer went to in this session (ST-M010-002). It ends with the
    /// session, so nothing of one customer computer is taken to the next.
    browsed: Mutex<Browsed>,
}

/// One owner of a live console session, shareable across the CLI and the IPC
/// server.
#[derive(Clone)]
pub struct ConsoleSessionService {
    inner: Arc<Inner>,
}

impl std::fmt::Debug for ConsoleSessionService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ConsoleSessionService")
            .field("session", &self.inner.session)
            .field("active", &self.is_active())
            .finish_non_exhaustive()
    }
}

impl ConsoleSessionService {
    /// Takes ownership of an **active, consented** `handle` and starts its driver
    /// task. `session` and `target` are the consent-bound identity captured when
    /// the session reached full control (REQ-F-016).
    ///
    /// The returned [`JoinHandle`] is the driver; drop it (or the last service
    /// clone) to end driving. Dropping every clone closes the command channel,
    /// which ends the driver and drops the owned handle, tearing the session
    /// down fail-closed.
    #[must_use]
    pub fn start(
        handle: HostHandle,
        session: SessionId,
        target: TargetIdentity,
    ) -> (Self, JoinHandle<()>) {
        let (command_tx, command_rx) = mpsc::unbounded_channel();
        let (event_tx, _event_rx) = broadcast::channel(EVENT_CAPACITY);
        let inner = Arc::new(Inner {
            session,
            target,
            active: AtomicBool::new(true),
            commands: command_tx,
            events: event_tx.clone(),
            transfer_gate: Mutex::new(TransferGate::default()),
            terminals: Mutex::new(TerminalRegistry::default()),
            ai_in_flight: Arc::new(AtomicU64::new(0)),
            technician_actions: AtomicU64::new(0),
            browsed: Mutex::new(Browsed::default()),
        });
        let driver = tokio::spawn(drive(handle, command_rx, event_tx, Arc::clone(&inner)));
        (Self { inner }, driver)
    }

    /// The number of MCP tool calls the local IPC server currently has in flight
    /// against this session — non-zero exactly while the AI is actively driving
    /// (ST-M004-006). The CLI shows a concise "AI active (N)" indicator from it.
    #[must_use]
    pub fn ai_in_flight(&self) -> u64 {
        self.inner.ai_in_flight.load(Ordering::SeqCst)
    }

    /// The shared in-flight counter, handed to [`crate::IpcServer::serve`] so the
    /// server increments/decrements the same atomic the CLI reads through
    /// [`Self::ai_in_flight`] (ST-M004-006).
    #[must_use]
    pub fn activity_counter(&self) -> Arc<AtomicU64> {
        Arc::clone(&self.inner.ai_in_flight)
    }

    /// Mints the id that the start record and the completion record of one
    /// technician action share in the audit trail (`action_id`): unique in
    /// this session and rising (ST-M010-001).
    pub(crate) fn next_technician_action(&self) -> u64 {
        self.inner.technician_actions.fetch_add(1, Ordering::SeqCst) + 1
    }

    /// What this session remembers of the places the technician's explorer
    /// went to (ST-M010-002).
    pub(crate) fn browsed(&self) -> &Mutex<Browsed> {
        &self.inner.browsed
    }

    /// The routed session id (REQ-F-016).
    #[must_use]
    pub fn session(&self) -> SessionId {
        self.inner.session
    }

    /// The routed target identity (REQ-F-016).
    #[must_use]
    pub fn target(&self) -> TargetIdentity {
        self.inner.target.clone()
    }

    /// Whether a consented, active session is currently bound. `false` the
    /// instant a revoke/stop is requested and after any teardown.
    #[must_use]
    pub fn is_active(&self) -> bool {
        self.inner.active.load(Ordering::SeqCst)
    }

    /// The consent-bound identity when active, or `None` (fail-closed) otherwise.
    /// Used to answer the IPC handshake and to gate routing.
    #[must_use]
    pub fn consented(&self) -> Option<(SessionId, TargetIdentity)> {
        if self.is_active() {
            Some((self.inner.session, self.inner.target.clone()))
        } else {
            None
        }
    }

    /// Subscribes to the live session-event stream (for the interactive CLI's
    /// live rendering). Only events emitted after this call are delivered.
    #[must_use]
    pub fn subscribe(&self) -> broadcast::Receiver<SessionEvent> {
        self.inner.events.subscribe()
    }

    /// Starts a command and returns the started job (id plus the effective
    /// remote working directory, ST-M003-003) once the agent has started it.
    /// Output arrives on [`Self::subscribe`]. For the interactive CLI's live
    /// path (the bridge uses [`Self::exec`] to collect).
    ///
    /// # Errors
    ///
    /// [`HostSessionError::NotActive`] when no consented session is bound, or
    /// any error the underlying [`HostHandle::run_command`] surfaces.
    pub async fn run_command(
        &self,
        request: HostExecRequest,
    ) -> Result<StartedJob, HostSessionError> {
        // The interactive CLI path: the technician drives it (ST-M004-006).
        self.dispatch_run(request, Origin::Console.into()).await
    }

    /// Starts a command, stamping the given [`Origin`] onto the wire request so
    /// the agent and the attended customer surface can attribute it to the AI or
    /// the technician (ST-M004-006). The CLI uses [`Self::run_command`]
    /// (`console`); the MCP-facing [`Self::exec`] uses `mcp`.
    async fn dispatch_run(
        &self,
        request: HostExecRequest,
        attribution: Attribution,
    ) -> Result<StartedJob, HostSessionError> {
        if !self.is_active() {
            return Err(HostSessionError::NotActive);
        }
        let (reply_tx, reply_rx) = oneshot::channel();
        self.inner
            .commands
            .send(DriverCommand::Run {
                request,
                attribution,
                reply: reply_tx,
            })
            .map_err(|_| HostSessionError::Disconnected)?;
        reply_rx
            .await
            .unwrap_or(Err(HostSessionError::Disconnected))
    }

    /// Feeds bytes to a running job's standard input.
    ///
    /// # Errors
    ///
    /// See [`Self::run_command`].
    pub async fn write_stdin(&self, job: JobId, data: Vec<u8>) -> Result<(), HostSessionError> {
        self.dispatch_write_stdin(job, data, Origin::Console.into())
            .await
    }

    /// Feeds bytes to a job's stdin, stamping the given [`Origin`] (ST-M004-006).
    async fn dispatch_write_stdin(
        &self,
        job: JobId,
        data: Vec<u8>,
        attribution: Attribution,
    ) -> Result<(), HostSessionError> {
        if !self.is_active() {
            return Err(HostSessionError::NotActive);
        }
        let (reply_tx, reply_rx) = oneshot::channel();
        self.inner
            .commands
            .send(DriverCommand::WriteStdin {
                job,
                data,
                attribution,
                reply: reply_tx,
            })
            .map_err(|_| HostSessionError::Disconnected)?;
        reply_rx
            .await
            .unwrap_or(Err(HostSessionError::Disconnected))
    }

    /// Requests cancellation of a running job.
    ///
    /// # Errors
    ///
    /// See [`Self::run_command`].
    pub async fn cancel_job(&self, job: JobId) -> Result<(), HostSessionError> {
        self.dispatch_cancel(job, Origin::Console.into()).await
    }

    /// Requests cancellation, stamping the given [`Attribution`] onto the wire
    /// `Cancel` (ST-M009-007).
    async fn dispatch_cancel(
        &self,
        job: JobId,
        attribution: Attribution,
    ) -> Result<(), HostSessionError> {
        if !self.is_active() {
            return Err(HostSessionError::NotActive);
        }
        let (reply_tx, reply_rx) = oneshot::channel();
        self.inner
            .commands
            .send(DriverCommand::Cancel {
                job,
                attribution,
                reply: reply_tx,
            })
            .map_err(|_| HostSessionError::Disconnected)?;
        reply_rx
            .await
            .unwrap_or(Err(HostSessionError::Disconnected))
    }

    /// Lists the jobs the target currently tracks.
    ///
    /// # Errors
    ///
    /// See [`Self::run_command`].
    pub async fn list_jobs(&self) -> Result<Vec<JobSummary>, HostSessionError> {
        self.dispatch_list_jobs(Origin::Console.into()).await
    }

    /// Lists tracked jobs, stamping the given [`Origin`] (ST-M004-006).
    async fn dispatch_list_jobs(
        &self,
        attribution: Attribution,
    ) -> Result<Vec<JobSummary>, HostSessionError> {
        if !self.is_active() {
            return Err(HostSessionError::NotActive);
        }
        let (reply_tx, reply_rx) = oneshot::channel();
        self.inner
            .commands
            .send(DriverCommand::ListJobs {
                attribution,
                reply: reply_tx,
            })
            .map_err(|_| HostSessionError::Disconnected)?;
        reply_rx
            .await
            .unwrap_or(Err(HostSessionError::Disconnected))
    }

    /// Technician-initiated stop. Flips the service fail-closed **immediately**
    /// (before the asynchronous teardown completes) so no routed call can reach
    /// the session after this returns (REQ-S-004, REQ-S-009).
    pub fn stop(&self) {
        self.revoke(RevokeReason::TechnicianRequested);
    }

    /// Revokes the session with an explicit reason, flipping fail-closed at once.
    ///
    /// Beyond flipping `active`, this cancels any in-flight transfer and bars a
    /// new one from starting: while holding the transfer gate it sets `revoked`
    /// and cancels the current token, so a transfer running inline in the driver
    /// stops issuing chunks within one wire chunk and reports honestly
    /// (REQ-S-004, REQ-S-009).
    pub fn revoke(&self, reason: RevokeReason) {
        self.inner.active.store(false, Ordering::SeqCst);
        if let Ok(mut gate) = self.inner.transfer_gate.lock() {
            gate.revoked = true;
            if let Some(token) = gate.current.as_ref() {
                token.cancel();
            }
        }
        let _ = self.inner.commands.send(DriverCommand::Stop { reason });
    }

    // --- MCP-facing (collected) surface ---

    /// Reports the routed target identity and basic system state, answered
    /// **live** by the agent over the session (ST-M003-003, REQ-F-016): the
    /// console never fabricates system information from its consent-time
    /// cache. A dead or unconsented session fails closed with an explicit
    /// [`TargetError::NoConsentedSession`].
    ///
    /// # Errors
    ///
    /// [`TargetError::NoConsentedSession`] when fail-closed, or a mapped error
    /// the agent or transport surfaced.
    pub async fn system_info(
        &self,
        purpose: Option<Purpose>,
    ) -> Result<SystemInfoReport, TargetError> {
        if !self.is_active() {
            return Err(TargetError::NoConsentedSession);
        }
        let (reply_tx, reply_rx) = oneshot::channel();
        self.inner
            .commands
            .send(DriverCommand::SystemInfo {
                attribution: mcp(purpose),
                reply: reply_tx,
            })
            .map_err(|_| TargetError::Disconnected)?;
        let (target, reboot_pending) = reply_rx
            .await
            .unwrap_or(Err(HostSessionError::Disconnected))
            .map_err(session_to_target)?;
        Ok(SystemInfoReport {
            target,
            reboot_pending,
        })
    }

    /// Runs a command to completion, collecting output up to `output_cap` bytes
    /// across both streams and returning an honest [`ExecReport`] (REQ-N-008).
    ///
    /// Subscribes to the event stream **before** starting the job so no output
    /// is missed. A dropped stream, an overrun of the cap, or a session that
    /// ends before the job finishes is reported honestly (`truncated`, and an
    /// [`JobOutcome::Unknown`] on a lost terminal event) — never a fabricated
    /// success.
    ///
    /// # Errors
    ///
    /// [`TargetError::NoConsentedSession`] when fail-closed, or a mapped
    /// [`TargetError`] if the agent rejects the command or the session ends.
    pub async fn exec(
        &self,
        purpose: Option<Purpose>,
        params: ExecParams,
        output_cap: usize,
    ) -> Result<ExecReport, TargetError> {
        if !self.is_active() {
            return Err(TargetError::NoConsentedSession);
        }
        // Subscribe before starting, so the job's first output cannot slip past.
        let mut events = self.subscribe();
        let request = HostExecRequest {
            shell: params.shell,
            command: params.command,
            working_dir: params.working_dir,
            env: params.env,
            timeout_ms: params.timeout_ms,
        };
        // Measure wall-clock duration from just before the job is submitted
        // until its terminal event, so the reported `duration_ms` is real.
        let started_at = Instant::now();
        // The AI drives this via the MCP bridge (ST-M004-006).
        let started = self
            .dispatch_run(request, mcp(purpose))
            .await
            .map_err(session_to_target)?;
        collect(
            &mut events,
            started.job,
            started.working_dir,
            output_cap,
            started_at,
        )
        .await
    }

    /// Feeds bytes to a running job's standard input (MCP-facing mapping).
    ///
    /// # Errors
    ///
    /// A mapped [`TargetError`]; fail-closed when inactive.
    pub async fn exec_write_stdin(
        &self,
        purpose: Option<Purpose>,
        job: JobId,
        data: Vec<u8>,
    ) -> Result<(), TargetError> {
        if !self.is_active() {
            return Err(TargetError::NoConsentedSession);
        }
        self.dispatch_write_stdin(job, data, mcp(purpose))
            .await
            .map_err(session_to_target)
    }

    /// Requests cancellation of a running job (MCP-facing mapping).
    ///
    /// # Errors
    ///
    /// A mapped [`TargetError`]; fail-closed when inactive.
    pub async fn exec_cancel(
        &self,
        purpose: Option<Purpose>,
        job: JobId,
    ) -> Result<(), TargetError> {
        if !self.is_active() {
            return Err(TargetError::NoConsentedSession);
        }
        self.dispatch_cancel(job, mcp(purpose))
            .await
            .map_err(session_to_target)
    }

    /// Lists tracked jobs (MCP-facing mapping).
    ///
    /// # Errors
    ///
    /// A mapped [`TargetError`]; fail-closed when inactive.
    pub async fn exec_list_jobs(
        &self,
        purpose: Option<Purpose>,
    ) -> Result<Vec<JobSummary>, TargetError> {
        if !self.is_active() {
            return Err(TargetError::NoConsentedSession);
        }
        self.dispatch_list_jobs(mcp(purpose))
            .await
            .map_err(session_to_target)
    }

    // --- MCP-facing file and transfer surface (ST-M004-003) ---

    /// Runs one bounded file operation on the target, gated fail-closed.
    ///
    /// The operation is enqueued on the driver, which runs it on the owned handle
    /// and answers over a one-shot; the reply is mapped onto [`TargetError`].
    ///
    /// # Errors
    ///
    /// [`TargetError::NoConsentedSession`] when fail-closed, or a mapped error
    /// the agent or transport surfaced.
    async fn mcp_file_op(
        &self,
        purpose: Option<Purpose>,
        op: Operation,
    ) -> Result<OpResult, TargetError> {
        // A single AI-driven operation is attributed to the AI, with its
        // stated purpose (ST-M004-006, ST-M009-007).
        self.file_op(mcp(purpose), op).await
    }

    /// Runs one bounded file operation on the target with the given
    /// attribution, gated fail-closed.
    async fn file_op(
        &self,
        attribution: Attribution,
        op: Operation,
    ) -> Result<OpResult, TargetError> {
        if !self.is_active() {
            return Err(TargetError::NoConsentedSession);
        }
        let (reply_tx, reply_rx) = oneshot::channel();
        self.inner
            .commands
            .send(DriverCommand::FileOp {
                op,
                attribution,
                reply: reply_tx,
            })
            .map_err(|_| TargetError::Disconnected)?;
        reply_rx
            .await
            .unwrap_or(Err(HostSessionError::Disconnected))
            .map_err(session_to_target)
    }

    /// Stats a path on the target (existence, kind, and any pending staging part
    /// length); never returns file content (REQ-N-007).
    ///
    /// # Errors
    ///
    /// A mapped [`TargetError`]; fail-closed when inactive.
    pub async fn fs_stat(
        &self,
        purpose: Option<Purpose>,
        path: RemotePath,
    ) -> Result<StatReport, TargetError> {
        match self
            .mcp_file_op(purpose, Operation::FsStat { path })
            .await?
        {
            OpResult::PathStat { entry, partial_len } => Ok(StatReport { entry, partial_len }),
            _ => Err(TargetError::Unexpected),
        }
    }

    /// Lists one bounded page of a directory on the target, continuing after the
    /// given entry name for pagination.
    ///
    /// # Errors
    ///
    /// A mapped [`TargetError`]; fail-closed when inactive.
    pub async fn fs_list(
        &self,
        purpose: Option<Purpose>,
        path: RemotePath,
        continue_after: Option<String>,
    ) -> Result<DirPage, TargetError> {
        match self
            .mcp_file_op(
                purpose,
                Operation::FsList {
                    path,
                    continue_after,
                },
            )
            .await?
        {
            OpResult::DirEntries { entries, end } => Ok(DirPage { entries, end }),
            _ => Err(TargetError::Unexpected),
        }
    }

    /// Creates a directory on the target.
    ///
    /// # Errors
    ///
    /// A mapped [`TargetError`]; fail-closed when inactive.
    pub async fn make_dir(
        &self,
        purpose: Option<Purpose>,
        path: RemotePath,
    ) -> Result<(), TargetError> {
        match self
            .mcp_file_op(purpose, Operation::CreateDir { path })
            .await?
        {
            OpResult::Ack {} => Ok(()),
            _ => Err(TargetError::Unexpected),
        }
    }

    /// Moves (renames) a path on the target; `overwrite` decides whether an
    /// existing destination file may be replaced.
    ///
    /// # Errors
    ///
    /// A mapped [`TargetError`]; fail-closed when inactive.
    pub async fn fs_move(
        &self,
        purpose: Option<Purpose>,
        from: RemotePath,
        to: RemotePath,
        overwrite: bool,
    ) -> Result<(), TargetError> {
        match self
            .mcp_file_op(
                purpose,
                Operation::FsMove {
                    from,
                    to,
                    overwrite,
                },
            )
            .await?
        {
            OpResult::Ack {} => Ok(()),
            _ => Err(TargetError::Unexpected),
        }
    }

    /// Deletes a file or reparse point on the target (never a directory).
    ///
    /// # Errors
    ///
    /// A mapped [`TargetError`]; fail-closed when inactive.
    pub async fn fs_delete(
        &self,
        purpose: Option<Purpose>,
        path: RemotePath,
    ) -> Result<(), TargetError> {
        match self
            .mcp_file_op(purpose, Operation::FsDelete { path })
            .await?
        {
            OpResult::Ack {} => Ok(()),
            _ => Err(TargetError::Unexpected),
        }
    }

    /// Removes an empty directory on the target.
    ///
    /// # Errors
    ///
    /// A mapped [`TargetError`]; fail-closed when inactive.
    pub async fn remove_dir(
        &self,
        purpose: Option<Purpose>,
        path: RemotePath,
    ) -> Result<(), TargetError> {
        match self
            .mcp_file_op(purpose, Operation::RemoveDir { path })
            .await?
        {
            OpResult::Ack {} => Ok(()),
            _ => Err(TargetError::Unexpected),
        }
    }

    /// Lists the session's staged artifacts and their recorded metadata.
    ///
    /// # Errors
    ///
    /// A mapped [`TargetError`]; fail-closed when inactive.
    pub async fn list_staged(
        &self,
        purpose: Option<Purpose>,
    ) -> Result<Vec<StagedEntry>, TargetError> {
        match self.mcp_file_op(purpose, Operation::ListStaged {}).await? {
            OpResult::StagedList { entries } => Ok(entries),
            _ => Err(TargetError::Unexpected),
        }
    }

    /// Runs a staging cleanup for the given trigger, reporting honest
    /// per-artifact outcomes (REQ-N-008).
    ///
    /// # Errors
    ///
    /// A mapped [`TargetError`]; fail-closed when inactive.
    pub async fn cleanup(
        &self,
        purpose: Option<Purpose>,
        trigger: CleanupTrigger,
    ) -> Result<CleanupSummary, TargetError> {
        match self
            .mcp_file_op(purpose, Operation::StagingCleanup { trigger })
            .await?
        {
            OpResult::CleanupReport { removed, failed } => Ok(CleanupSummary { removed, failed }),
            _ => Err(TargetError::Unexpected),
        }
    }

    // --- Visual desktop control (ADR-0015, ST-M005-004) ---
    //
    // Each desktop op rides the same generic `mcp_file_op` path as any other
    // `Operation`: `is_active()`-gated fail-closed, `Origin::Mcp`-attributed,
    // and audited by the caller. A locked or secure desktop answers with honest
    // metadata and no pixels rather than a fabricated frame (REQ-N-008).

    /// Captures the routed target's current interactive desktop.
    ///
    /// # Errors
    ///
    /// A mapped [`TargetError`]; fail-closed when inactive.
    pub async fn capture_desktop(
        &self,
        purpose: Option<Purpose>,
        params: CaptureParams,
    ) -> Result<CaptureReport, TargetError> {
        let CaptureParams {
            display,
            format,
            max_width,
            max_height,
            quality,
        } = params;
        match self
            .mcp_file_op(
                purpose,
                Operation::DesktopCapture {
                    display,
                    format,
                    max_width,
                    max_height,
                    quality,
                },
            )
            .await?
        {
            OpResult::DesktopFrame {
                meta,
                image,
                staged,
            } => Ok(CaptureReport {
                meta,
                image,
                staged,
            }),
            _ => Err(TargetError::Unexpected),
        }
    }

    /// Injects a pointer action at an absolute virtual-desktop coordinate.
    ///
    /// # Errors
    ///
    /// A mapped [`TargetError`]; fail-closed when inactive.
    pub async fn desktop_pointer(
        &self,
        purpose: Option<Purpose>,
        action: PointerAction,
        button: PointerButton,
        x: i32,
        y: i32,
    ) -> Result<(), TargetError> {
        match self
            .mcp_file_op(
                purpose,
                Operation::DesktopPointer {
                    action,
                    button,
                    x,
                    y,
                },
            )
            .await?
        {
            OpResult::Ack {} => Ok(()),
            _ => Err(TargetError::Unexpected),
        }
    }

    /// Injects a scroll (wheel) action at an absolute coordinate.
    ///
    /// # Errors
    ///
    /// A mapped [`TargetError`]; fail-closed when inactive.
    pub async fn desktop_scroll(
        &self,
        purpose: Option<Purpose>,
        x: i32,
        y: i32,
        dx: i32,
        dy: i32,
    ) -> Result<(), TargetError> {
        match self
            .mcp_file_op(purpose, Operation::DesktopScroll { x, y, dx, dy })
            .await?
        {
            OpResult::Ack {} => Ok(()),
            _ => Err(TargetError::Unexpected),
        }
    }

    /// Injects a single key action naming a virtual key.
    ///
    /// # Errors
    ///
    /// A mapped [`TargetError`]; fail-closed when inactive.
    pub async fn desktop_key(
        &self,
        purpose: Option<Purpose>,
        action: KeyAction,
        key: String,
    ) -> Result<(), TargetError> {
        match self
            .mcp_file_op(purpose, Operation::DesktopKey { action, key })
            .await?
        {
            OpResult::Ack {} => Ok(()),
            _ => Err(TargetError::Unexpected),
        }
    }

    /// Types a Unicode string as a sequence of key events.
    ///
    /// # Errors
    ///
    /// A mapped [`TargetError`]; fail-closed when inactive.
    pub async fn desktop_type(
        &self,
        purpose: Option<Purpose>,
        text: String,
    ) -> Result<(), TargetError> {
        match self
            .mcp_file_op(purpose, Operation::DesktopType { text })
            .await?
        {
            OpResult::Ack {} => Ok(()),
            _ => Err(TargetError::Unexpected),
        }
    }

    /// Sends a chord of named virtual keys as a single hotkey press.
    ///
    /// # Errors
    ///
    /// A mapped [`TargetError`]; fail-closed when inactive.
    pub async fn desktop_hotkey(
        &self,
        purpose: Option<Purpose>,
        keys: Vec<String>,
    ) -> Result<(), TargetError> {
        match self
            .mcp_file_op(purpose, Operation::DesktopHotkey { keys })
            .await?
        {
            OpResult::Ack {} => Ok(()),
            _ => Err(TargetError::Unexpected),
        }
    }

    /// Paces the input pipeline for a bounded number of milliseconds.
    ///
    /// # Errors
    ///
    /// A mapped [`TargetError`]; fail-closed when inactive.
    pub async fn desktop_wait(
        &self,
        purpose: Option<Purpose>,
        millis: u64,
    ) -> Result<(), TargetError> {
        match self
            .mcp_file_op(purpose, Operation::DesktopWait { millis })
            .await?
        {
            OpResult::Ack {} => Ok(()),
            _ => Err(TargetError::Unexpected),
        }
    }

    /// Reads the routed target's clipboard text, or `None` when it holds none.
    ///
    /// # Errors
    ///
    /// A mapped [`TargetError`]; fail-closed when inactive.
    pub async fn clipboard_get(
        &self,
        purpose: Option<Purpose>,
    ) -> Result<Option<String>, TargetError> {
        match self
            .mcp_file_op(purpose, Operation::ClipboardGet {})
            .await?
        {
            OpResult::ClipboardContents { text } => Ok(text),
            _ => Err(TargetError::Unexpected),
        }
    }

    /// Replaces the routed target's clipboard text.
    ///
    /// # Errors
    ///
    /// A mapped [`TargetError`]; fail-closed when inactive.
    pub async fn clipboard_set(
        &self,
        purpose: Option<Purpose>,
        text: String,
    ) -> Result<(), TargetError> {
        match self
            .mcp_file_op(purpose, Operation::ClipboardSet { text })
            .await?
        {
            OpResult::Ack {} => Ok(()),
            _ => Err(TargetError::Unexpected),
        }
    }

    /// Uploads a local file into target staging and commits it verified, driven
    /// inline by the console transfer engine over bounded chunk operations. The
    /// result carries the canonical committed path, length, and verified digest;
    /// content is never returned (REQ-N-007, REQ-S-018).
    ///
    /// # Errors
    ///
    /// A mapped [`TargetError`]; fail-closed when inactive or revoked. An error
    /// on the local file is not an error of the call: it is a `Failed` report
    /// in the stage `local`, and the session stays usable (ST-M003-005).
    pub async fn upload(
        &self,
        purpose: Option<Purpose>,
        params: UploadParams,
    ) -> Result<UploadReport, TargetError> {
        if !self.is_active() {
            return Err(TargetError::NoConsentedSession);
        }
        let request = UploadRequest {
            source: PathBuf::from(params.local_path),
            dest: params.dest,
            source_label: params.source_label,
            cleanup: params.cleanup,
            overwrite: params.overwrite,
        };
        self.dispatch_upload(request, mcp(purpose), None, None)
            .await
    }

    /// Queues one upload on the driver and awaits its honest report.
    async fn dispatch_upload(
        &self,
        request: UploadRequest,
        attribution: Attribution,
        cancel: Option<CancelToken>,
        progress: Option<ProgressSink<TransferProgress>>,
    ) -> Result<UploadReport, TargetError> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.inner
            .commands
            .send(DriverCommand::Upload {
                request,
                attribution,
                cancel,
                progress,
                reply: reply_tx,
            })
            .map_err(|_| TargetError::Disconnected)?;
        let report = reply_rx
            .await
            .unwrap_or(Err(HostSessionError::Disconnected))
            .map_err(session_to_target)?;
        Ok(upload_report_from(report))
    }

    // --- The technician's file explorer (ST-M010-001, ST-M010-002) ---
    //
    // These are the technician's own actions: every request they issue
    // is stamped `Origin::Console`, so the agent and the attended customer
    // attribute them to the technician, not to the AI (REQ-F-017, REQ-S-023).
    // `TechnicianFiles` runs them and records them in the audit trail.

    /// Lists the drives of the target: each with its root, kind, label and
    /// total and free space, where a value the target could not read is
    /// absent.
    ///
    /// # Errors
    ///
    /// A mapped [`TargetError`]; fail-closed when inactive. A target that does
    /// not serve the operation answers `Unsupported`.
    pub async fn list_drives(&self) -> Result<Vec<DriveEntry>, TargetError> {
        match self
            .file_op(Origin::Console.into(), Operation::FsDrives {})
            .await?
        {
            OpResult::Drives { drives } => {
                // The roots are what the explorer names a drive by.
                self.inner
                    .browsed
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .set_roots(&drives);
                Ok(drives)
            }
            _ => Err(TargetError::Unexpected),
        }
    }

    /// Lists one page of a folder of the target for the technician: the
    /// entries after the name `continue_after`, ordered by name, and whether
    /// the folder ends with them (ST-M010-002). A page is as long as fits one
    /// record of the secure channel, so pages differ in length
    /// (ST-M010-005).
    ///
    /// This is one request. [`crate::TechnicianFiles::open_folder`] lists a
    /// whole folder with it and records that in the audit trail.
    ///
    /// # Errors
    ///
    /// A mapped [`TargetError`]; fail-closed when inactive.
    pub async fn list_folder(
        &self,
        path: RemotePath,
        continue_after: Option<String>,
    ) -> Result<DirPage, TargetError> {
        match self
            .file_op(
                Origin::Console.into(),
                Operation::FsList {
                    path,
                    continue_after,
                },
            )
            .await?
        {
            OpResult::DirEntries { entries, end } => Ok(DirPage { entries, end }),
            _ => Err(TargetError::Unexpected),
        }
    }

    /// Removes a folder of the target with everything in it and returns the
    /// honest [`TreeRemovalReport`]: completed only when the folder is gone,
    /// failed with the first entries that could not be removed and their
    /// reasons, or cancelled with what was removed until then. The files are
    /// deleted, not moved to a recycle bin.
    ///
    /// Setting `cancel` ends the removal between two of its steps, and so does
    /// a revoke or stop of the session. `progress` receives the counts after
    /// each step.
    ///
    /// # Errors
    ///
    /// A mapped [`TargetError`]; fail-closed when inactive or revoked. A
    /// removal the target refused or could not finish is not an error of the
    /// call: it is a `Failed` report.
    pub async fn remove_tree(
        &self,
        path: RemotePath,
        cancel: CancelToken,
        progress: Option<ProgressSink<TreeRemovalProgress>>,
    ) -> Result<TreeRemovalReport, TargetError> {
        if !self.is_active() {
            return Err(TargetError::NoConsentedSession);
        }
        let (reply_tx, reply_rx) = oneshot::channel();
        self.inner
            .commands
            .send(DriverCommand::RemoveTree {
                path,
                attribution: Origin::Console.into(),
                cancel,
                progress,
                reply: reply_tx,
            })
            .map_err(|_| TargetError::Disconnected)?;
        reply_rx
            .await
            .unwrap_or(Err(HostSessionError::Disconnected))
            .map_err(session_to_target)
    }

    /// Sends a file of the technician computer into a folder of the target,
    /// on any drive, and commits it there under its name once its length and
    /// SHA-256 are verified. A file of that name that is already there is
    /// replaced only when [`SendToFolder::overwrite`] says so.
    ///
    /// Setting `cancel` ends the transfer and discards its unfinished part;
    /// a revoke or stop does the same. `progress` receives the bytes
    /// confirmed so far.
    ///
    /// # Errors
    ///
    /// A mapped [`TargetError`]: `invalid_path` when the folder or the name
    /// is refused (nothing is sent), fail-closed when inactive or revoked. A
    /// transfer that failed on either computer is not an error of the call:
    /// it is a `Failed` report, as for [`Self::upload`].
    pub async fn send_to_folder(
        &self,
        send: SendToFolder,
        cancel: CancelToken,
        progress: Option<ProgressSink<TransferProgress>>,
    ) -> Result<UploadReport, TargetError> {
        if !self.is_active() {
            return Err(TargetError::NoConsentedSession);
        }
        let request = UploadRequest {
            dest: send.dest()?,
            source: send.local_path,
            // A file sent to a folder is not staged: it has no source label
            // in a manifest and no cleanup policy applies to it.
            source_label: String::new(),
            cleanup: CleanupPolicy::Keep,
            overwrite: send.overwrite,
        };
        self.dispatch_upload(request, Origin::Console.into(), Some(cancel), progress)
            .await
    }

    /// Downloads a target file to a local path, driven inline by the console
    /// transfer engine; content is verified before landing and never returned to
    /// the caller (REQ-N-007, REQ-S-018).
    ///
    /// # Errors
    ///
    /// A mapped [`TargetError`]; fail-closed when inactive or revoked. An error
    /// on the local destination is not an error of the call: it is a `Failed`
    /// report in the stage `local`, and the session stays usable (ST-M003-005).
    pub async fn download(
        &self,
        purpose: Option<Purpose>,
        params: DownloadParams,
    ) -> Result<DownloadReport, TargetError> {
        if !self.is_active() {
            return Err(TargetError::NoConsentedSession);
        }
        let local_path = params.local_path.clone();
        let request = DownloadRequest {
            source: params.source,
            dest: PathBuf::from(params.local_path),
        };
        let (reply_tx, reply_rx) = oneshot::channel();
        self.inner
            .commands
            .send(DriverCommand::Download {
                request,
                attribution: mcp(purpose),
                reply: reply_tx,
            })
            .map_err(|_| TargetError::Disconnected)?;
        let report = reply_rx
            .await
            .unwrap_or(Err(HostSessionError::Disconnected))
            .map_err(session_to_target)?;
        Ok(download_report_from(report, local_path))
    }

    // --- Interactive terminal surface (ST-M003-009) ---

    /// Runs `f` against the terminal registry, or `None` on a poisoned lock
    /// (treated fail-closed).
    fn with_terminals<R>(&self, f: impl FnOnce(&mut TerminalRegistry) -> R) -> Option<R> {
        self.inner.terminals.lock().ok().map(|mut reg| f(&mut reg))
    }

    /// Opens an interactive terminal on the routed session and registers it, so
    /// consumers can attach to its merged output (ST-M003-009). A terminal is a
    /// job: its output/exit flow through the same session-event stream and it is
    /// cancelable and listable like any job. The request is stamped
    /// [`Origin::Terminal`], distinct from the AI bridge (REQ-S-023).
    ///
    /// Bounded by [`MAX_TERMINALS`]: a slot is reserved fail-closed **before**
    /// the host round-trip, so concurrent opens can never exceed the ceiling.
    ///
    /// # Errors
    ///
    /// [`TargetError::NoConsentedSession`] when fail-closed, a mapped
    /// [`TargetError`] the agent surfaced (for example `Unsupported` off
    /// Windows), or a `LimitExceeded` remote error when at the terminal cap.
    pub async fn open_terminal(
        &self,
        shell: Shell,
        cols: u16,
        rows: u16,
        working_dir: Option<String>,
        env: BTreeMap<String, String>,
    ) -> Result<JobId, TargetError> {
        if !self.is_active() {
            return Err(TargetError::NoConsentedSession);
        }
        // Reserve a slot before opening so a burst of concurrent opens can never
        // exceed MAX_TERMINALS (the reservation is released on failure below).
        if !self
            .with_terminals(TerminalRegistry::try_reserve)
            .unwrap_or(false)
        {
            return Err(too_many_terminals());
        }
        let (reply_tx, reply_rx) = oneshot::channel();
        if self
            .inner
            .commands
            .send(DriverCommand::OpenTerminal {
                shell,
                cols,
                rows,
                working_dir,
                env,
                reply: reply_tx,
            })
            .is_err()
        {
            self.with_terminals(TerminalRegistry::release);
            return Err(TargetError::Disconnected);
        }
        let result = reply_rx
            .await
            .unwrap_or(Err(HostSessionError::Disconnected));
        match result {
            Ok(job) => {
                self.with_terminals(|reg| reg.insert(job));
                Ok(job)
            }
            Err(err) => {
                self.with_terminals(TerminalRegistry::release);
                Err(session_to_target(err))
            }
        }
    }

    /// Attaches a local consumer to an open terminal, returning its bounded
    /// event stream (ST-M003-009). This is a purely local registry operation —
    /// **attaching grants no new authority**; it never reaches the target.
    ///
    /// Bounded by [`MAX_TERM_CONSUMERS`] per terminal.
    ///
    /// # Errors
    ///
    /// [`TargetError::NoConsentedSession`] when no consented session is bound or
    /// after a revoke/stop; a `UnknownJob` remote error for an unknown or
    /// already-exited terminal; a `LimitExceeded` remote error at the consumer
    /// cap.
    pub fn term_attach(&self, term: JobId) -> Result<TermAttachment, TargetError> {
        // Fail closed exactly like the bridge: no consented session -> refuse.
        let (session, target) = self.consented().ok_or(TargetError::NoConsentedSession)?;
        let mut reg = self
            .inner
            .terminals
            .lock()
            .map_err(|_| TargetError::NoConsentedSession)?;
        match reg.attach(term) {
            Ok((consumer, events, overrun)) => Ok(TermAttachment {
                session,
                target,
                term,
                consumer,
                events,
                overrun,
            }),
            Err(AttachRejection::Unknown) => Err(unknown_terminal()),
            Err(AttachRejection::TooMany) => Err(too_many_consumers()),
        }
    }

    /// Feeds input bytes to a terminal's single pseudo-console, muxed to the one
    /// pty and stamped [`Origin::Terminal`] (ST-M003-009). Raw input is never
    /// delivered to another consumer; the shared echo returns to everyone via
    /// the merged output.
    ///
    /// # Errors
    ///
    /// A mapped [`TargetError`]; fail-closed when inactive.
    pub async fn term_input(&self, term: JobId, data: Vec<u8>) -> Result<(), TargetError> {
        if !self.is_active() {
            return Err(TargetError::NoConsentedSession);
        }
        self.dispatch_write_stdin(term, data, Origin::Terminal.into())
            .await
            .map_err(session_to_target)
    }

    /// Resizes a terminal's pseudo-console (last-writer-wins). An out-of-bounds
    /// size is rejected locally by the handle and never acted on (ST-M003-009).
    ///
    /// # Errors
    ///
    /// A mapped [`TargetError`]; fail-closed when inactive.
    pub async fn term_resize(&self, term: JobId, cols: u16, rows: u16) -> Result<(), TargetError> {
        if !self.is_active() {
            return Err(TargetError::NoConsentedSession);
        }
        let (reply_tx, reply_rx) = oneshot::channel();
        self.inner
            .commands
            .send(DriverCommand::ResizeTerminal {
                job: term,
                cols,
                rows,
                reply: reply_tx,
            })
            .map_err(|_| TargetError::Disconnected)?;
        reply_rx
            .await
            .unwrap_or(Err(HostSessionError::Disconnected))
            .map_err(session_to_target)
    }

    /// Detaches exactly one consumer from a terminal; the terminal outlives zero
    /// consumers (ST-M003-009). A purely local registry operation.
    pub fn term_detach(&self, term: JobId, consumer: u64) {
        self.with_terminals(|reg| reg.detach(term, consumer));
    }

    /// Closes a terminal, ending its job for every attached consumer, who each
    /// receive an honest terminal frame (ST-M003-009).
    ///
    /// # Errors
    ///
    /// A mapped [`TargetError`]; fail-closed when inactive.
    pub async fn term_close(&self, term: JobId) -> Result<(), TargetError> {
        if !self.is_active() {
            return Err(TargetError::NoConsentedSession);
        }
        let (reply_tx, reply_rx) = oneshot::channel();
        self.inner
            .commands
            .send(DriverCommand::CloseTerminal {
                job: term,
                reply: reply_tx,
            })
            .map_err(|_| TargetError::Disconnected)?;
        reply_rx
            .await
            .unwrap_or(Err(HostSessionError::Disconnected))
            .map_err(session_to_target)
    }
}

/// A locally-determined "too many open terminals" fail-closed error.
fn too_many_terminals() -> TargetError {
    TargetError::Remote {
        code: ErrorCode::LimitExceeded,
        message: "too many open terminals; close one before opening another".to_string(),
        stage: ErrorStage::Validation,
    }
}

/// A locally-determined "too many consumers on this terminal" error.
fn too_many_consumers() -> TargetError {
    TargetError::Remote {
        code: ErrorCode::LimitExceeded,
        message: "too many consumers attached to this terminal".to_string(),
        stage: ErrorStage::Validation,
    }
}

/// A locally-determined "no such open terminal" error (stale/unknown handle).
fn unknown_terminal() -> TargetError {
    TargetError::Remote {
        code: ErrorCode::UnknownJob,
        message: "no such open terminal on this session".to_string(),
        stage: ErrorStage::Validation,
    }
}

/// The driver task: sole owner of the handle. Pumps events into the broadcast
/// and executes command requests, flipping the service fail-closed on any
/// terminal event or when the stream ends.
async fn drive(
    mut handle: HostHandle,
    mut command_rx: mpsc::UnboundedReceiver<DriverCommand>,
    event_tx: broadcast::Sender<SessionEvent>,
    inner: Arc<Inner>,
) {
    loop {
        tokio::select! {
            maybe_command = command_rx.recv() => {
                match maybe_command {
                    Some(command) => run_command(&handle, &inner, command).await,
                    // Every service clone was dropped: end driving. Dropping the
                    // handle aborts its session task, tearing down fail-closed.
                    None => break,
                }
            }
            event = handle.next_event() => {
                match event {
                    Some(event) => {
                        if matches!(event, SessionEvent::Revoked { .. } | SessionEvent::Failed { .. }) {
                            inner.active.store(false, Ordering::SeqCst);
                        }
                        // Fan terminal output/exit/revoke out to attached
                        // consumers before broadcasting the event, so a
                        // revoke actively tears their streams down (REQ-S-009).
                        fanout_terminal(&inner, &event);
                        // A gone receiver just means no one is currently listening.
                        let _ = event_tx.send(event);
                    }
                    None => break,
                }
            }
        }
    }
    inner.active.store(false, Ordering::SeqCst);
    // The driver ended (session lost or every clone dropped): tear any remaining
    // attached terminal consumers down with an honest Unknown exit rather than
    // leaving their streams to hang — never a fabricated clean close (REQ-N-008).
    if let Ok(mut reg) = inner.terminals.lock() {
        reg.disconnect_all();
    }
}

/// Executes one driver command inline on the owned handle. The replies for
/// these operations arrive over the handle's command channel, not its event
/// stream, so awaiting them here never starves event pumping.
///
/// A revoke/stop flips `inner.active` to `false` **before** its `Stop` is
/// enqueued, but the command queue is FIFO, so commands submitted just before
/// the revoke may already sit ahead of that `Stop`. Each such command is
/// re-checked against `inner.active` at dequeue time and short-circuited to
/// [`HostSessionError::NotActive`] once a stop has been requested, so a
/// queued-but-not-yet-run command can never reach a revoked session (REQ-S-009,
/// ST-M004-004). A command already executing when the flip happens still runs to
/// its terminal event, which [`collect`] reports honestly.
async fn run_command(handle: &HostHandle, inner: &Inner, command: DriverCommand) {
    let active = inner.active.load(Ordering::SeqCst);
    match command {
        DriverCommand::Run {
            request,
            attribution,
            reply,
        } => {
            if active {
                let _ = reply.send(handle.run_command_as(request, attribution).await);
            } else {
                let _ = reply.send(Err(HostSessionError::NotActive));
            }
        }
        DriverCommand::SystemInfo { attribution, reply } => {
            if active {
                let _ = reply.send(handle.system_info_as(attribution).await);
            } else {
                let _ = reply.send(Err(HostSessionError::NotActive));
            }
        }
        DriverCommand::WriteStdin {
            job,
            data,
            attribution,
            reply,
        } => {
            if active {
                let _ = reply.send(handle.write_stdin_as(job, data, attribution).await);
            } else {
                let _ = reply.send(Err(HostSessionError::NotActive));
            }
        }
        DriverCommand::Cancel {
            job,
            attribution,
            reply,
        } => {
            if active {
                let _ = reply.send(handle.cancel_job_as(job, attribution).await);
            } else {
                let _ = reply.send(Err(HostSessionError::NotActive));
            }
        }
        DriverCommand::ListJobs { attribution, reply } => {
            if active {
                let _ = reply.send(handle.list_jobs_as(attribution).await);
            } else {
                let _ = reply.send(Err(HostSessionError::NotActive));
            }
        }
        DriverCommand::FileOp {
            op,
            attribution,
            reply,
        } => {
            if active {
                let _ = reply.send(handle.file_op_as(op, attribution).await);
            } else {
                let _ = reply.send(Err(HostSessionError::NotActive));
            }
        }
        DriverCommand::Upload {
            request,
            attribution,
            cancel,
            progress,
            reply,
        } => {
            let _ =
                reply.send(run_upload(handle, inner, request, attribution, cancel, progress).await);
        }
        DriverCommand::RemoveTree {
            path,
            attribution,
            cancel,
            progress,
            reply,
        } => {
            let _ = reply
                .send(run_remove_tree(handle, inner, path, attribution, cancel, progress).await);
        }
        DriverCommand::Download {
            request,
            attribution,
            reply,
        } => {
            let _ = reply.send(run_download(handle, inner, request, attribution).await);
        }
        DriverCommand::OpenTerminal {
            shell,
            cols,
            rows,
            working_dir,
            env,
            reply,
        } => {
            if active {
                let _ = reply.send(
                    handle
                        .open_terminal(shell, cols, rows, working_dir, env)
                        .await,
                );
            } else {
                let _ = reply.send(Err(HostSessionError::NotActive));
            }
        }
        DriverCommand::ResizeTerminal {
            job,
            cols,
            rows,
            reply,
        } => {
            if active {
                let _ = reply.send(handle.resize_terminal(job, cols, rows).await);
            } else {
                let _ = reply.send(Err(HostSessionError::NotActive));
            }
        }
        DriverCommand::CloseTerminal { job, reply } => {
            if active {
                let _ = reply.send(handle.close_terminal(job).await);
            } else {
                let _ = reply.send(Err(HostSessionError::NotActive));
            }
        }
        DriverCommand::Stop { reason } => {
            let _ = handle.revoke(reason);
        }
    }
}

/// Fans one session event out to any attached terminal consumers (ST-M003-009).
///
/// This runs inline in the driver's event pump, so it must not block: every
/// delivery is a non-blocking `try_send` on a per-consumer bounded channel, and
/// a consumer that overruns its window is dropped honestly (REQ-N-006). Output
/// and exit events for a terminal job feed its consumers; a session-level output
/// drop for a terminal disconnects its consumers (the shared stream lost bytes
/// upstream); a revoke tears every consumer down fail-closed.
fn fanout_terminal(inner: &Inner, event: &SessionEvent) {
    let Ok(mut reg) = inner.terminals.lock() else {
        return;
    };
    match event {
        SessionEvent::ExecOutput { job, bytes, .. } if reg.contains(*job) => {
            reg.deliver_output(*job, bytes);
        }
        SessionEvent::ExecOutputDropped { job, .. } if reg.contains(*job) => {
            reg.drop_consumers(*job);
        }
        SessionEvent::ExecFinished {
            job,
            exit_code,
            outcome,
        } if reg.contains(*job) => {
            reg.deliver_exit(*job, *exit_code, *outcome);
        }
        SessionEvent::Revoked { .. } | SessionEvent::Failed { .. } => {
            reg.revoke_all();
        }
        _ => {}
    }
}

/// The attribution of an MCP-bridge call: the AI origin plus the AI client's
/// stated purpose when it supplied one (ST-M004-006, ST-M009-007).
fn mcp(purpose: Option<Purpose>) -> Attribution {
    Attribution::new(Origin::Mcp, purpose)
}

/// Runs one upload inline on the owned handle, gated against revoke.
///
/// The cancel token — the caller's own, or a fresh one — is published into the
/// transfer gate under one lock so a concurrent
/// [`ConsoleSessionService::revoke`] either bars the start (`revoked` already
/// set) or reaches the token to cancel it (REQ-S-004, REQ-S-009). The gate is
/// released before the `await` so the driver's own revoke path is never
/// blocked by the running transfer.
async fn run_upload(
    handle: &HostHandle,
    inner: &Inner,
    request: UploadRequest,
    attribution: Attribution,
    cancel: Option<CancelToken>,
    mut progress: Option<ProgressSink<TransferProgress>>,
) -> Result<TransferReport, HostSessionError> {
    let token = match enter_transfer(inner, cancel) {
        Some(token) => token,
        None => return Err(HostSessionError::NotActive),
    };
    let result = handle
        .upload_file_as(request, attribution, &token, |step| {
            if let Some(sink) = progress.as_mut() {
                sink(step);
            }
        })
        .await;
    leave_transfer(inner);
    result
}

/// Runs the removal of one folder inline on the owned handle, gated against
/// revoke exactly like a transfer (see [`run_upload`]): a revoke cancels the
/// token, and the removal ends between two of its steps (ST-M010-001,
/// REQ-S-004).
async fn run_remove_tree(
    handle: &HostHandle,
    inner: &Inner,
    path: RemotePath,
    attribution: Attribution,
    cancel: CancelToken,
    mut progress: Option<ProgressSink<TreeRemovalProgress>>,
) -> Result<TreeRemovalReport, HostSessionError> {
    let token = match enter_transfer(inner, Some(cancel)) {
        Some(token) => token,
        None => return Err(HostSessionError::NotActive),
    };
    let result = handle
        .remove_tree_as(path, attribution, &token, |step| {
            if let Some(sink) = progress.as_mut() {
                sink(step);
            }
        })
        .await;
    leave_transfer(inner);
    result
}

/// Runs one download inline on the owned handle, gated against revoke. See
/// [`run_upload`] for the gate handshake.
async fn run_download(
    handle: &HostHandle,
    inner: &Inner,
    request: DownloadRequest,
    attribution: Attribution,
) -> Result<TransferReport, HostSessionError> {
    let token = match enter_transfer(inner, None) {
        Some(token) => token,
        None => return Err(HostSessionError::NotActive),
    };
    let result = handle
        .download_file_as(request, attribution, &token, |_| {})
        .await;
    leave_transfer(inner);
    result
}

/// Claims the transfer gate for a new transfer or folder removal, returning
/// its cancel token — `own` when the caller handed one in, else a fresh one —
/// or `None` when the session is revoked or inactive (fail-closed). A poisoned
/// gate is treated as revoked.
fn enter_transfer(inner: &Inner, own: Option<CancelToken>) -> Option<CancelToken> {
    let mut gate = inner.transfer_gate.lock().ok()?;
    if gate.revoked || !inner.active.load(Ordering::SeqCst) {
        return None;
    }
    let token = own.unwrap_or_default();
    gate.current = Some(token.clone());
    Some(token)
}

/// Releases the transfer gate after a transfer ends. A poisoned gate is left as
/// is; the session is already tearing down fail-closed.
fn leave_transfer(inner: &Inner) {
    if let Ok(mut gate) = inner.transfer_gate.lock() {
        gate.current = None;
    }
}

/// Collects a job's streamed output from the broadcast until its terminal event,
/// bounded by `cap`. A session that ends first yields an honest
/// [`JobOutcome::Unknown`] report (REQ-N-008).
async fn collect(
    events: &mut broadcast::Receiver<SessionEvent>,
    job: JobId,
    working_dir: String,
    cap: usize,
    started: Instant,
) -> Result<ExecReport, TargetError> {
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    let mut total = 0usize;
    let mut truncated = false;
    loop {
        match events.recv().await {
            Ok(SessionEvent::ExecOutput {
                job: j,
                stream,
                bytes,
            }) if j == job => {
                let buf = match stream {
                    OutputStream::Stdout => &mut stdout,
                    OutputStream::Stderr => &mut stderr,
                };
                append_capped(buf, &bytes, cap, &mut total, &mut truncated);
            }
            Ok(SessionEvent::ExecOutputDropped { job: j, .. }) if j == job => {
                truncated = true;
            }
            Ok(SessionEvent::ExecFinished {
                job: j,
                exit_code,
                outcome,
            }) if j == job => {
                return Ok(ExecReport {
                    job,
                    working_dir,
                    stdout,
                    stderr,
                    truncated,
                    exit_code,
                    duration_ms: elapsed_ms(started),
                    outcome,
                });
            }
            // A session-terminal event before the job's own finish: the outcome
            // can no longer be determined honestly.
            Ok(SessionEvent::Revoked { .. } | SessionEvent::Failed { .. }) => {
                return Ok(unknown_report(
                    job,
                    working_dir,
                    stdout,
                    stderr,
                    true,
                    started,
                ));
            }
            Ok(_) => {}
            Err(broadcast::error::RecvError::Lagged(_)) => {
                // The bounded window overran; bytes were lost and the terminal
                // event may have been among them. Report honestly rather than
                // hang forever.
                truncated = true;
            }
            Err(broadcast::error::RecvError::Closed) => {
                return Ok(unknown_report(
                    job,
                    working_dir,
                    stdout,
                    stderr,
                    truncated,
                    started,
                ));
            }
        }
    }
}

/// Wall-clock milliseconds since `started`, saturating into the report's `u64`.
fn elapsed_ms(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)
}

/// Builds an honest report for a job whose exact outcome could not be
/// determined — never a fabricated success (REQ-N-008).
fn unknown_report(
    job: JobId,
    working_dir: String,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    truncated: bool,
    started: Instant,
) -> ExecReport {
    ExecReport {
        job,
        working_dir,
        stdout,
        stderr,
        truncated,
        exit_code: None,
        duration_ms: elapsed_ms(started),
        outcome: JobOutcome::Unknown,
    }
}

/// Appends `data` to `buf`, stopping at the combined `cap`, recording truncation.
fn append_capped(
    buf: &mut Vec<u8>,
    data: &[u8],
    cap: usize,
    total: &mut usize,
    truncated: &mut bool,
) {
    if *total >= cap {
        *truncated = true;
        return;
    }
    let room = cap - *total;
    if data.len() > room {
        buf.extend_from_slice(&data[..room]);
        *total += room;
        *truncated = true;
    } else {
        buf.extend_from_slice(data);
        *total += data.len();
    }
}

/// Projects the transfer engine's honest [`TransferReport`] onto the MCP-facing
/// [`UploadReport`], carrying the canonical committed path but never content
/// (REQ-N-007, REQ-S-018). The error that ended a failed transfer (whose type is
/// internal to the session crate) is mapped field-by-field into the decoupled
/// [`TransferFailure`], so its category, message and stage — `local` for an
/// error on the technician's own file — reach the caller unchanged
/// (ST-M003-005).
fn upload_report_from(report: TransferReport) -> UploadReport {
    UploadReport {
        outcome: report.outcome,
        remote_path: report.canonical_path,
        len: report.total,
        sent: report.sent,
        digest: report.digest,
        resumed_from: report.resumed_from,
        restarted: report.restarted,
        error: report.error.map(|e| TransferFailure {
            code: e.code,
            message: e.message,
            stage: e.stage.to_string(),
        }),
    }
}

/// Projects the transfer engine's honest [`TransferReport`] onto the MCP-facing
/// [`DownloadReport`]. The local destination is echoed from the request (the
/// engine reports remote-side facts); content is never returned.
fn download_report_from(report: TransferReport, local_path: String) -> DownloadReport {
    DownloadReport {
        outcome: report.outcome,
        local_path,
        len: report.total,
        sent: report.sent,
        digest: report.digest,
        resumed_from: report.resumed_from,
        restarted: report.restarted,
        error: report.error.map(|e| TransferFailure {
            code: e.code,
            message: e.message,
            stage: e.stage.to_string(),
        }),
    }
}

/// The technician's explorer reads the target through the service, so every
/// request it makes is the technician's ([`Origin::Console`]) and fails closed
/// with the session (ST-M010-002).
#[async_trait::async_trait]
impl FolderSource for ConsoleSessionService {
    async fn drives(&self) -> Result<Vec<DriveEntry>, TargetError> {
        self.list_drives().await
    }

    async fn page(&self, folder: &RemotePath, after: Option<&str>) -> Result<DirPage, TargetError> {
        self.list_folder(folder.clone(), after.map(str::to_owned))
            .await
    }
}

/// Maps a session-runtime error onto the honest MCP [`TargetError`], keeping a
/// fail-closed no-session distinct from a remote failure and a lost session.
fn session_to_target(err: HostSessionError) -> TargetError {
    match err {
        HostSessionError::Remote { code, message } => TargetError::Remote {
            code,
            message,
            stage: ErrorStage::Execution,
        },
        // A locally rejected request keeps its typed category (for example
        // `invalid_path` for a rejected path), so the structured error_code a
        // front end reports names the true class, never a collapsed
        // `limit_exceeded` (ST-M003-003, REQ-N-015).
        HostSessionError::Invalid { code, message } => TargetError::Remote {
            code,
            message,
            stage: ErrorStage::Validation,
        },
        HostSessionError::NotActive
        | HostSessionError::RevokedLocally
        | HostSessionError::RevokedByPeer
        | HostSessionError::ConsentDenied => TargetError::NoConsentedSession,
        // `Io` and `Relay` are the connection's (binding, dialing, the relay).
        // An error on the technician's own file never arrives here: the
        // transfer engine reports it as a failed transfer in the stage `local`
        // (ST-M003-005).
        HostSessionError::Disconnected
        | HostSessionError::Io(_)
        | HostSessionError::Relay(_)
        | HostSessionError::Channel(_)
        | HostSessionError::Pairing(_)
        | HostSessionError::Timeout => TargetError::Disconnected,
        HostSessionError::Protocol(_) | HostSessionError::Unexpected { .. } => {
            TargetError::Unexpected
        }
    }
}

#[cfg(test)]
mod terminal_registry_tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    /// Drains all currently-buffered events from a consumer, returning them.
    fn drain(rx: &mut mpsc::Receiver<TermEvent>) -> Vec<TermEvent> {
        let mut out = Vec::new();
        while let Ok(event) = rx.try_recv() {
            out.push(event);
        }
        out
    }

    /// The reservation scheme bounds concurrent opens at `MAX_TERMINALS`: a
    /// reservation is taken before the host round-trip, so a burst of opens can
    /// never exceed the ceiling, and a released reservation frees a slot.
    #[test]
    fn open_bound_is_enforced_by_reservations() {
        let mut reg = TerminalRegistry::default();
        for i in 0..MAX_TERMINALS {
            assert!(
                reg.try_reserve(),
                "reservation {i} within the cap must succeed"
            );
        }
        assert!(
            !reg.try_reserve(),
            "a reservation beyond MAX_TERMINALS must be refused"
        );
        // Land every reservation as a real terminal.
        for i in 0..MAX_TERMINALS {
            reg.insert(JobId(i as u64 + 1));
        }
        assert!(
            !reg.try_reserve(),
            "with MAX_TERMINALS open, no further slot is free"
        );
        // Freeing a terminal frees a slot again.
        reg.deliver_exit(JobId(1), Some(0), JobOutcome::Completed);
        assert!(reg.try_reserve(), "closing a terminal frees a slot");
    }

    /// Attaching to an unknown terminal is a stale-handle rejection; attaching up
    /// to `MAX_TERM_CONSUMERS` succeeds and the next attach is refused.
    #[test]
    fn attach_bound_and_unknown_terminal() {
        let mut reg = TerminalRegistry::default();
        assert!(matches!(
            reg.attach(JobId(42)),
            Err(AttachRejection::Unknown)
        ));

        reg.reserved += 1;
        reg.insert(JobId(1));
        let mut holders = Vec::new();
        for _ in 0..MAX_TERM_CONSUMERS {
            let (_id, rx, overrun) = reg.attach(JobId(1)).expect("attach within the cap");
            holders.push((rx, overrun));
        }
        assert!(
            matches!(reg.attach(JobId(1)), Err(AttachRejection::TooMany)),
            "a consumer beyond MAX_TERM_CONSUMERS must be refused"
        );
    }

    /// Merged output is fanned out to every attached consumer — the shared tmux
    /// stream — so both consumers see the same bytes (including any echo).
    #[test]
    fn output_is_fanned_out_to_every_consumer() {
        let mut reg = TerminalRegistry::default();
        reg.reserved += 1;
        reg.insert(JobId(1));
        let (_a, mut rx_a, _oa) = reg.attach(JobId(1)).unwrap();
        let (_b, mut rx_b, _ob) = reg.attach(JobId(1)).unwrap();

        reg.deliver_output(JobId(1), b"sim-term$ ");
        assert_eq!(
            drain(&mut rx_a),
            vec![TermEvent::Output(b"sim-term$ ".to_vec())]
        );
        assert_eq!(
            drain(&mut rx_b),
            vec![TermEvent::Output(b"sim-term$ ".to_vec())]
        );
    }

    /// A consumer that overruns its bounded window is disconnected (overrun flag
    /// set, removed from the fan-out set) while a consumer that keeps up receives
    /// every byte in order — the slow consumer never corrupts the shared stream
    /// for the other (REQ-N-006).
    #[test]
    fn slow_consumer_is_dropped_without_corrupting_the_others() {
        let mut reg = TerminalRegistry::default();
        reg.reserved += 1;
        reg.insert(JobId(1));
        let (_slow, mut rx_slow, overrun_slow) = reg.attach(JobId(1)).unwrap();
        let (_fast, mut rx_fast, overrun_fast) = reg.attach(JobId(1)).unwrap();

        // Deliver well past the per-consumer capacity. The fast consumer is
        // drained after every delivery so it never fills; the slow consumer is
        // never drained, so it overflows and is dropped.
        let bursts = TERM_CONSUMER_CAPACITY + 50;
        let mut fast_received = 0usize;
        for i in 0..bursts {
            reg.deliver_output(JobId(1), format!("chunk-{i}").as_bytes());
            fast_received += drain(&mut rx_fast).len();
        }

        assert!(
            overrun_slow.load(Ordering::SeqCst),
            "the slow consumer must be marked overrun"
        );
        assert!(
            !overrun_fast.load(Ordering::SeqCst),
            "the fast consumer must never be marked overrun"
        );
        // The fast consumer saw every byte, uncorrupted; the slow one saw at most
        // its bounded window then had its channel closed.
        assert_eq!(fast_received, bursts, "the fast consumer must lose nothing");
        let slow = drain(&mut rx_slow);
        assert!(
            slow.len() <= TERM_CONSUMER_CAPACITY,
            "the slow consumer's buffer stays bounded, got {}",
            slow.len()
        );
        // The slow consumer's channel is closed (its sender was dropped).
        assert!(rx_slow.try_recv().is_err());
        // The fast consumer is still attached and keeps receiving.
        reg.deliver_output(JobId(1), b"after");
        assert_eq!(
            drain(&mut rx_fast),
            vec![TermEvent::Output(b"after".to_vec())]
        );
    }

    /// An exit delivers an honest `Exited` to every consumer and removes the
    /// terminal; a revoke tears every consumer down with `Revoked`; a driver-end
    /// disconnect delivers an honest `Unknown` exit — never a fabricated close.
    #[test]
    fn exit_revoke_and_disconnect_deliver_honest_terminal_frames() {
        // Exit.
        let mut reg = TerminalRegistry::default();
        reg.reserved += 1;
        reg.insert(JobId(1));
        let (_c, mut rx, _o) = reg.attach(JobId(1)).unwrap();
        reg.deliver_exit(JobId(1), Some(3), JobOutcome::Failed);
        assert_eq!(
            drain(&mut rx),
            vec![TermEvent::Exited {
                exit_code: Some(3),
                outcome: JobOutcome::Failed
            }]
        );
        assert!(!reg.contains(JobId(1)), "an exited terminal is removed");

        // Revoke.
        let mut reg = TerminalRegistry::default();
        reg.reserved += 1;
        reg.insert(JobId(2));
        let (_c, mut rx, _o) = reg.attach(JobId(2)).unwrap();
        reg.revoke_all();
        assert_eq!(drain(&mut rx), vec![TermEvent::Revoked]);
        assert!(!reg.contains(JobId(2)));

        // Driver-end disconnect: honest Unknown, never a clean close.
        let mut reg = TerminalRegistry::default();
        reg.reserved += 1;
        reg.insert(JobId(3));
        let (_c, mut rx, _o) = reg.attach(JobId(3)).unwrap();
        reg.disconnect_all();
        assert_eq!(
            drain(&mut rx),
            vec![TermEvent::Exited {
                exit_code: None,
                outcome: JobOutcome::Unknown
            }]
        );
    }

    /// Detaching removes exactly one consumer; the terminal outlives it and the
    /// remaining consumer keeps receiving.
    #[test]
    fn detach_removes_only_that_consumer() {
        let mut reg = TerminalRegistry::default();
        reg.reserved += 1;
        reg.insert(JobId(1));
        let (id_a, mut rx_a, _oa) = reg.attach(JobId(1)).unwrap();
        let (_id_b, mut rx_b, _ob) = reg.attach(JobId(1)).unwrap();

        reg.detach(JobId(1), id_a);
        reg.deliver_output(JobId(1), b"only-b");
        assert!(
            drain(&mut rx_a).is_empty(),
            "the detached consumer gets nothing"
        );
        assert_eq!(
            drain(&mut rx_b),
            vec![TermEvent::Output(b"only-b".to_vec())]
        );
        assert!(
            reg.contains(JobId(1)),
            "the terminal outlives zero-or-more consumers"
        );
    }
}

#[cfg(test)]
mod session_error_mapping_tests {
    use super::*;

    /// An error of the connection is still a lost session, whose remote outcome
    /// is not known (REQ-N-008). Only an error on the technician's own file is
    /// a failed transfer instead, and the transfer engine reports that one in
    /// its own report: it never arrives here as an error (ST-M003-005).
    #[test]
    fn a_connection_error_is_still_a_lost_session() {
        let reset = std::io::Error::from(std::io::ErrorKind::ConnectionReset);
        for error in [
            HostSessionError::Io(reset),
            HostSessionError::Disconnected,
            HostSessionError::Timeout,
        ] {
            assert_eq!(session_to_target(error), TargetError::Disconnected);
        }
    }
}

#[cfg(test)]
mod send_to_folder_tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    fn send(folder: &str, name: &str) -> SendToFolder {
        SendToFolder {
            local_path: PathBuf::from("report.pdf"),
            folder: folder.to_owned(),
            name: name.to_owned(),
            overwrite: false,
        }
    }

    fn dest(folder: &str, name: &str) -> String {
        match send(folder, name).dest() {
            Ok(RemotePath::Absolute { path }) => path,
            other => panic!("expected an absolute destination, got {other:?}"),
        }
    }

    fn refused(folder: &str, name: &str) -> String {
        match send(folder, name).dest() {
            Err(TargetError::Remote {
                code: ErrorCode::InvalidPath,
                message,
                stage: ErrorStage::Validation,
            }) => message,
            other => panic!("expected an invalid path for {folder:?} + {name:?}, got {other:?}"),
        }
    }

    /// The file goes under its name into the folder, on any drive, a drive
    /// root included, and the folder's own trailing separator is not doubled.
    #[test]
    fn the_destination_is_the_name_in_the_folder() {
        assert_eq!(
            dest("C:\\Users\\ann\\Documents", "report.pdf"),
            "C:\\Users\\ann\\Documents\\report.pdf"
        );
        assert_eq!(
            dest("C:\\Users\\ann\\Documents\\", "report.pdf"),
            "C:\\Users\\ann\\Documents\\report.pdf"
        );
        assert_eq!(dest("D:\\", "setup.exe"), "D:\\setup.exe");
        assert_eq!(dest("E:/Tools", "a b.txt"), "E:/Tools\\a b.txt");
    }

    /// A name is one name: it cannot reach into another folder, up or down.
    #[test]
    fn a_name_that_is_a_path_is_refused() {
        for name in [
            "",
            "sub\\report.pdf",
            "sub/report.pdf",
            "..\\report.pdf",
            "\\report.pdf",
            "/report.pdf",
        ] {
            assert!(
                refused("C:\\Users\\ann", name).contains("one name"),
                "{name:?}"
            );
        }
        // One component that still names another place or another file.
        for name in [
            "..",
            ".",
            "report.pdf:hidden",
            "NUL",
            "nul.txt",
            "report.pdf.",
            "report.pdf ",
        ] {
            refused("C:\\Users\\ann", name);
        }
    }

    /// The folder is a rooted path on a drive, by the same rules as every
    /// remote path: no drive-relative, relative, UNC or device path, and no
    /// way up.
    #[test]
    fn a_folder_that_is_not_a_rooted_path_on_a_drive_is_refused() {
        for folder in [
            "",
            "C:",
            "Documents",
            "\\Users\\ann",
            "\\\\server\\share",
            "\\\\?\\C:\\Users",
            "C:\\Users\\..\\Windows",
            "C:\\Users\\ann\n",
        ] {
            refused(folder, "report.pdf");
        }
    }

    /// A destination that would be too long for a remote path is refused
    /// here, before anything is sent.
    #[test]
    fn a_destination_that_is_too_long_is_refused() {
        let name = "n".repeat(cretlink_protocol::limits::MAX_NAME_BYTES + 1);
        refused("C:\\Users\\ann", &name);
    }
}
