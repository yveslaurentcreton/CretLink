//! Non-privileged simulated CretLink target (ST-M001-003, REQ-N-011).
//!
//! [`Simulator::serve`] speaks the `cretlink-protocol` schema over any
//! framed byte stream (tests use `tokio::io::duplex`) so console, MCP,
//! protocol, and failure behaviour can be exercised deterministically
//! without administrator rights, real process spawning, or a customer
//! computer. Everything runs in tokio userspace.
//!
//! # Identity (REQ-F-016)
//!
//! Every `HelloAck` and `SystemInfo` result carries a [`TargetIdentity`]
//! with hostname `SIMULATED-TARGET` and `simulated = true`, so simulated
//! results can never be mistaken for a real customer computer. The
//! simulator reports `elevated = false` and never asks for elevation.
//!
//! # Scripted commands
//!
//! `Exec` command strings drive deterministic behaviour:
//!
//! | Command | Behaviour |
//! |---|---|
//! | `sim:ok <text>` | One stdout [`EventKind::JobOutput`] chunk `<text>\n`, then `JobExited` with outcome `Completed` and exit code 0. |
//! | `sim:fail <code>` | One stderr chunk, then `JobExited` with outcome `Failed` and exit code `<code>` (a valid `i32`). |
//! | `sim:delay <ms>` | `JobExited` `Completed` after `<ms>` milliseconds of tokio time (tests may pause the clock). |
//! | `sim:hang` | Runs until a `Cancel` arrives, then `JobExited` `Cancelled`. `WriteStdin` to the job echoes the bytes back as stdout, proving the stdin path. |
//! | `sim:timeout` | Honours the request's `timeout_ms` deadline and emits `JobExited` `TimedOut` (deadline 0 when no `timeout_ms` was given). |
//! | `sim:disconnect` | Answers `ExecStarted`, then drops the connection mid-job so peers must report an unknown outcome (REQ-N-008). |
//! | `sim:malformed` | Instead of a response, writes a length-correct frame whose payload is invalid JSON, then closes. |
//! | anything else | A wire `Error` with code [`ErrorCode::Unsupported`]. |
//!
//! The `shell` and `env` fields of `Exec` are accepted but ignored:
//! behaviour is scripted, never executed. `working_dir` is echoed back as
//! the `ExecStarted` effective working directory (or the connection's
//! private VFS root when absent), mirroring the real agent's reporting
//! (ST-M003-003, REQ-F-009). The simulator emits plain text only;
//! VT-sequence pass-through (ADR-0010) concerns real ConPTY output.
//!
//! # Interactive terminal grammar (ADR-0013, ST-M003-009)
//!
//! `TermOpen` registers an interactive terminal job (a terminal is a job),
//! answers `TermOpened`, and emits a fixed plain-text prompt as the first
//! [`EventKind::JobOutput`] chunk. `WriteStdin` to the terminal echoes the bytes
//! back as merged output (the stream path), `TermResize` acknowledges an
//! in-range resize (last-writer-wins), and `TermClose` ends the job with an
//! honest `Cancelled` exit — all deterministic and CI-portable, no real ConPTY.
//!
//! # Scripted misbehaviours
//!
//! For peers' negative-path tests the builder can make negotiation
//! misbehave: [`Simulator::with_wrong_version`] answers `HelloAck` with an
//! incompatible major version, and [`Simulator::with_oversized_frame`]
//! writes a frame prefix exceeding the 1 MiB cap so transports must reject
//! it before allocating.
//!
//! # Bounded state
//!
//! Per-connection state stays bounded, modelling the discipline the real
//! agent must have: at most [`limits::MAX_JOBS_LISTED`] jobs may run
//! concurrently (further `Exec` requests fail with
//! [`ErrorCode::LimitExceeded`]), and only the most recent finished jobs
//! are kept as history — older finished jobs are pruned. `ListJobs` lists
//! running jobs first and fills the remaining space with the most recent
//! finished jobs, so a long history can never push live work out of the
//! listing.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use cretlink_exec::FileStore;
use cretlink_protocol::{
    B64Bytes, ErrorCode, ErrorStage, EventKind, JobId, JobOutcome, JobSummary, Message, OpResult,
    Operation, OutputStream, PROTOCOL_V1, ProtocolVersion, RequestId, Role, SessionId,
    TargetIdentity, limits,
};
use cretlink_transport::{Framed, TransportError};
use thiserror::Error;
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};
use tokio::time::{Duration, Instant, sleep_until};

/// The most finished jobs kept as `ListJobs` history. Older finished jobs
/// are pruned so a long-lived connection's memory stays bounded.
const FINISHED_JOB_HISTORY: usize = 64;

/// The most concurrently running jobs one connection may hold; `Exec`
/// requests beyond it are refused with [`ErrorCode::LimitExceeded`]. This
/// also bounds the timer list (each delay/timeout job arms at most one
/// timer).
const MAX_RUNNING_JOBS: usize = limits::MAX_JOBS_LISTED;

/// The fixed, plain-text prompt a simulated terminal emits as its first
/// merged-output chunk, so an attach test sees deterministic output with no real
/// ConPTY (ADR-0010, ST-M003-009).
const TERMINAL_PROMPT: &[u8] = b"sim-term$ ";

/// A failure while serving the simulated connection.
///
/// Peer-caused protocol problems (bad frames, unknown sessions, unsupported
/// commands) are answered on the wire and are **not** errors here; this
/// type only reports the simulator's own inability to keep serving.
#[derive(Debug, Error)]
pub enum SimulatorError {
    /// Sending or receiving on the framed transport failed.
    #[error("transport failure")]
    Transport(#[from] TransportError),

    /// A raw stream write failed while performing a scripted misbehaviour
    /// or malformed-frame scenario.
    #[error("i/o failure")]
    Io(#[from] std::io::Error),
}

/// Which deliberate negotiation misbehaviour the simulator performs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum Misbehaviour {
    /// Behave correctly.
    #[default]
    None,
    /// Answer `HelloAck` with an incompatible major version.
    WrongVersion,
    /// Write a raw frame prefix exceeding the frame cap instead of a
    /// `HelloAck`.
    OversizedFrame,
}

/// A configurable simulated target; see the crate documentation.
///
/// Build one with [`Simulator::new`] (correct behaviour) plus the optional
/// misbehaviour toggles, then drive one connection with
/// [`Simulator::serve`].
#[derive(Debug, Clone, Default)]
pub struct Simulator {
    misbehaviour: Misbehaviour,
    /// The live pending-reboot state `SystemInfo` reports; shared and mutable
    /// so a test can flip it between calls (see [`Simulator::reboot_flag`]).
    reboot_pending: Arc<AtomicBool>,
}

impl Simulator {
    /// A correctly-behaving simulator.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// A shared handle to the simulator's mutable pending-reboot flag
    /// (initially `false`). Tests flip it between `SystemInfo` calls to prove
    /// a peer reports the **live** per-request value, never a cached one
    /// (ST-M003-003).
    #[must_use]
    pub fn reboot_flag(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.reboot_pending)
    }

    /// Answer negotiation with an incompatible protocol major version, so
    /// peers can prove they reject it (REQ-N-012).
    #[must_use]
    pub fn with_wrong_version(mut self) -> Self {
        self.misbehaviour = Misbehaviour::WrongVersion;
        self
    }

    /// Answer negotiation with a frame prefix exceeding
    /// [`limits::MAX_FRAME_BYTES`], so peers can prove they reject it
    /// before allocating.
    #[must_use]
    pub fn with_oversized_frame(mut self) -> Self {
        self.misbehaviour = Misbehaviour::OversizedFrame;
        self
    }

    /// The identity every simulated session reports (REQ-F-016).
    #[must_use]
    pub fn target_identity() -> TargetIdentity {
        TargetIdentity {
            hostname: "SIMULATED-TARGET".to_string(),
            os_version: "Simulated Windows".to_string(),
            arch: "x86_64".to_string(),
            elevated: false,
            simulated: true,
            agent_version: env!("CARGO_PKG_VERSION").to_string(),
            account: None,
        }
    }

    /// Serves exactly one console connection until the peer disconnects, a
    /// scripted scenario closes the stream, or the transport fails.
    pub async fn serve<T: AsyncRead + AsyncWrite + Unpin>(
        &self,
        io: T,
    ) -> Result<(), SimulatorError> {
        Connection::new(self.misbehaviour, Arc::clone(&self.reboot_pending))
            .run(Framed::new(io))
            .await
    }
}

/// Message-handler verdict for the serve loop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Flow {
    /// Keep serving.
    Continue,
    /// Close the connection (scripted disconnect).
    Close,
    /// Write a length-correct but invalid-JSON frame, then close
    /// (`sim:malformed`).
    WriteMalformedAndClose,
}

/// How a timer-driven job ends.
#[derive(Debug, Clone, Copy)]
enum TimerExit {
    /// `sim:delay` completion: exit code 0, outcome `Completed`.
    Completed,
    /// `sim:timeout` expiry: no exit code, outcome `TimedOut`.
    TimedOut,
}

/// A pending timer-driven job exit.
#[derive(Debug)]
struct Timer {
    at: Instant,
    job: JobId,
    exit: TimerExit,
    duration_ms: u64,
}

/// One tracked simulated job.
#[derive(Debug)]
struct Job {
    preview: String,
    running: bool,
    hang: bool,
    /// Whether this job is an interactive terminal (opened by `TermOpen`); like
    /// a `sim:hang` job it accepts standard input and echoes it back, and it
    /// runs until an explicit `TermClose`/`Cancel` (ST-M003-009).
    terminal: bool,
    started: Instant,
}

/// The parsed scripted command grammar.
#[derive(Debug)]
enum Scripted {
    Ok(String),
    Fail(i32),
    Delay(u64),
    Hang,
    Timeout,
    Disconnect,
    Malformed,
}

/// Per-connection simulator state.
#[derive(Debug)]
struct Connection {
    misbehaviour: Misbehaviour,
    session: SessionId,
    jobs: BTreeMap<JobId, Job>,
    timers: Vec<Timer>,
    next_job: u64,
    /// The live pending-reboot flag, read per `SystemInfo` request.
    reboot_pending: Arc<AtomicBool>,
    /// The real file engine backing this connection's simulated filesystem,
    /// rooted at [`Self::vfs_root`] so nothing leaks outside the temp tree.
    vfs: FileStore,
    /// The private temp directory the VFS is rooted at, removed on drop.
    vfs_root: PathBuf,
}

impl Connection {
    fn new(misbehaviour: Misbehaviour, reboot_pending: Arc<AtomicBool>) -> Self {
        let session = SessionId::new();
        // A unique, per-connection temp root. `with_base` only computes paths —
        // no directory is created until the first write — so this stays cheap for
        // the many connections that never touch the filesystem.
        let vfs_root = std::env::temp_dir().join(format!("cretlink-sim-vfs-{session}"));
        let vfs = FileStore::with_base(session, vfs_root.clone());
        Self {
            misbehaviour,
            session,
            jobs: BTreeMap::new(),
            timers: Vec::new(),
            next_job: 1,
            reboot_pending,
            vfs,
            vfs_root,
        }
    }

    async fn run<T: AsyncRead + AsyncWrite + Unpin>(
        mut self,
        framed: Framed<T>,
    ) -> Result<(), SimulatorError> {
        let Some(mut framed) = self.negotiate(framed).await? else {
            return Ok(());
        };
        loop {
            let deadline = self.next_deadline();
            let armed = deadline.is_some();
            let sleep_at = deadline.unwrap_or_else(|| Instant::now() + Duration::from_secs(86_400));
            tokio::select! {
                received = framed.recv() => match received {
                    Ok(message) => match self.handle_message(&mut framed, message).await? {
                        Flow::Continue => {}
                        Flow::Close => return Ok(()),
                        Flow::WriteMalformedAndClose => {
                            return write_malformed_frame(framed.into_inner()).await;
                        }
                    },
                    Err(error) => return self.close_on_recv_error(&mut framed, error).await,
                },
                () = sleep_until(sleep_at), if armed => {
                    self.fire_due_timers(&mut framed).await?;
                }
            }
        }
    }

    /// Performs `Hello`/`HelloAck` negotiation (or a configured
    /// misbehaviour). Returns the transport when the serve loop should
    /// start, and `None` when the connection is finished.
    async fn negotiate<T: AsyncRead + AsyncWrite + Unpin>(
        &mut self,
        mut framed: Framed<T>,
    ) -> Result<Option<Framed<T>>, SimulatorError> {
        let first = match framed.recv().await {
            Ok(message) => message,
            Err(error) => {
                self.close_on_recv_error(&mut framed, error).await?;
                return Ok(None);
            }
        };
        let Message::Hello {
            protocol,
            role,
            capabilities: _,
        } = first
        else {
            framed
                .send(&negotiation_error(
                    ErrorCode::MalformedMessage,
                    "expected a hello message first",
                ))
                .await?;
            return Ok(None);
        };
        if role != Role::Console {
            framed
                .send(&negotiation_error(
                    ErrorCode::Unsupported,
                    "the simulator only accepts console peers",
                ))
                .await?;
            return Ok(None);
        }
        match self.misbehaviour {
            Misbehaviour::WrongVersion => {
                framed
                    .send(&Message::HelloAck {
                        protocol: ProtocolVersion::new(PROTOCOL_V1.major.wrapping_add(1), 0),
                        session: self.session,
                        target: Simulator::target_identity(),
                    })
                    .await?;
                return Ok(None);
            }
            Misbehaviour::OversizedFrame => {
                let mut io = framed.into_inner();
                let declared = u32::try_from(limits::MAX_FRAME_BYTES + 1).unwrap_or(u32::MAX);
                io.write_all(&declared.to_le_bytes()).await?;
                io.flush().await?;
                return Ok(None);
            }
            Misbehaviour::None => {}
        }
        let negotiated = match PROTOCOL_V1.negotiate(protocol) {
            Ok(version) => version,
            Err(error) => {
                framed
                    .send(&negotiation_error(error.code(), &error.to_string()))
                    .await?;
                return Ok(None);
            }
        };
        framed
            .send(&Message::HelloAck {
                protocol: negotiated,
                session: self.session,
                target: Simulator::target_identity(),
            })
            .await?;
        Ok(Some(framed))
    }

    /// Ends the connection after a receive failure, answering with a wire
    /// `Error` (best effort) where one is meaningful.
    async fn close_on_recv_error<T: AsyncRead + AsyncWrite + Unpin>(
        &self,
        framed: &mut Framed<T>,
        error: TransportError,
    ) -> Result<(), SimulatorError> {
        match error {
            TransportError::Eof => Ok(()),
            TransportError::Decode(protocol_error) => {
                let reply = self.error_message(
                    None,
                    protocol_error.code(),
                    &protocol_error.to_string(),
                    ErrorStage::Validation,
                );
                let _ = framed.send(&reply).await;
                Ok(())
            }
            TransportError::Oversized { len, max } => {
                let reply = self.error_message(
                    None,
                    ErrorCode::OversizedMessage,
                    &format!("frame of {len} bytes exceeds the {max}-byte limit"),
                    ErrorStage::Transport,
                );
                let _ = framed.send(&reply).await;
                Ok(())
            }
            TransportError::Io(io_error) => Err(SimulatorError::Io(io_error)),
        }
    }

    async fn handle_message<T: AsyncRead + AsyncWrite + Unpin>(
        &mut self,
        framed: &mut Framed<T>,
        message: Message,
    ) -> Result<Flow, SimulatorError> {
        match message {
            // The simulated target ignores `origin` (attribution is a
            // console/agent concern); it never changes scripted behaviour.
            Message::Request {
                id, session, op, ..
            } => {
                if session != self.session {
                    framed
                        .send(&self.error_message(
                            Some(id),
                            ErrorCode::UnknownSession,
                            "the request references a session this connection does not own",
                            ErrorStage::Validation,
                        ))
                        .await?;
                    return Ok(Flow::Continue);
                }
                self.handle_request(framed, id, op).await
            }
            Message::Cancel {
                id, session, job, ..
            } => {
                if session != self.session {
                    framed
                        .send(&self.error_message(
                            Some(id),
                            ErrorCode::UnknownSession,
                            "the cancel references a session this connection does not own",
                            ErrorStage::Validation,
                        ))
                        .await?;
                    return Ok(Flow::Continue);
                }
                self.handle_cancel(framed, id, job).await?;
                Ok(Flow::Continue)
            }
            Message::Hello { .. }
            | Message::HelloAck { .. }
            | Message::Response { .. }
            | Message::Event { .. }
            | Message::Error { .. }
            | Message::Pairing { .. }
            // Session-control messages (consent, revocation, heartbeat;
            // ST-M002-005/006) belong to the `cretlink-session` runtime, not to
            // the simulated target, which pre-dates full-control consent. Refuse
            // them like any other unexpected message.
            | Message::Control { .. } => {
                framed
                    .send(&self.error_message(
                        None,
                        ErrorCode::Unsupported,
                        "unexpected message type from a console peer",
                        ErrorStage::Validation,
                    ))
                    .await?;
                Ok(Flow::Continue)
            }
        }
    }

    async fn handle_request<T: AsyncRead + AsyncWrite + Unpin>(
        &mut self,
        framed: &mut Framed<T>,
        id: RequestId,
        op: Operation,
    ) -> Result<Flow, SimulatorError> {
        match op {
            Operation::SystemInfo {} => {
                framed
                    .send(&Message::Response {
                        id,
                        session: self.session,
                        result: OpResult::SystemInfo {
                            target: Simulator::target_identity(),
                            // Read live per request, so peers can prove their
                            // reporting is a roundtrip and never a cache
                            // (ST-M003-003).
                            reboot_pending: self.reboot_pending.load(Ordering::SeqCst),
                        },
                    })
                    .await?;
                Ok(Flow::Continue)
            }
            Operation::ListJobs {} => {
                // Running jobs come first; the remaining space holds the
                // most recent finished jobs, so history never hides live
                // work.
                let mut jobs: Vec<JobSummary> = self
                    .jobs
                    .iter()
                    .filter(|(_, state)| state.running)
                    .take(limits::MAX_JOBS_LISTED)
                    .map(|(job, state)| summarise(*job, state))
                    .collect();
                let room = limits::MAX_JOBS_LISTED - jobs.len();
                let mut finished: Vec<JobSummary> = self
                    .jobs
                    .iter()
                    .rev()
                    .filter(|(_, state)| !state.running)
                    .take(room)
                    .map(|(job, state)| summarise(*job, state))
                    .collect();
                jobs.append(&mut finished);
                jobs.sort_by_key(|summary| summary.job);
                framed
                    .send(&Message::Response {
                        id,
                        session: self.session,
                        result: OpResult::Jobs { jobs },
                    })
                    .await?;
                Ok(Flow::Continue)
            }
            Operation::WriteStdin { job, data } => {
                self.handle_write_stdin(framed, id, job, data).await?;
                Ok(Flow::Continue)
            }
            Operation::Exec {
                command,
                working_dir,
                timeout_ms,
                ..
            } => {
                self.handle_exec(framed, id, &command, working_dir, timeout_ms)
                    .await
            }
            // Interactive terminal (ADR-0013, ST-M003-009): a deterministic,
            // plain-text terminal grammar so an attach/stream/detach round-trip
            // runs on every CI platform without a real ConPTY. The dimensions are
            // already schema-validated in range on decode.
            Operation::TermOpen { cols, rows, .. } => {
                self.handle_term_open(framed, id, cols, rows).await
            }
            Operation::TermResize { job, .. } => {
                self.handle_term_resize(framed, id, job).await?;
                Ok(Flow::Continue)
            }
            Operation::TermClose { job } => {
                self.handle_term_close(framed, id, job).await?;
                Ok(Flow::Continue)
            }
            // Interactive desktop (ADR-0015, CAP_DESKTOP_V1): the simulator
            // advertises no desktop capability and ships no capture-and-input
            // backend, so every desktop operation is refused honestly with
            // `Unsupported` at execution stage — exactly what a production agent
            // without a desktop backend answers (ST-M005-001, REQ-N-008/012).
            Operation::DesktopCapture { .. }
            | Operation::DesktopPointer { .. }
            | Operation::DesktopScroll { .. }
            | Operation::DesktopKey { .. }
            | Operation::DesktopType { .. }
            | Operation::DesktopHotkey { .. }
            | Operation::DesktopWait { .. }
            | Operation::ClipboardGet {}
            | Operation::ClipboardSet { .. } => {
                framed
                    .send(&self.error_message(
                        Some(id),
                        ErrorCode::Unsupported,
                        "the simulator does not advertise the desktop capability",
                        ErrorStage::Execution,
                    ))
                    .await?;
                Ok(Flow::Continue)
            }
            // File and staging operations (ADR-0012, CAP_FS_V1): run the real
            // FileStore engine against this connection's private temp tree, so a
            // simulated target enforces the same staging root, verified-commit,
            // resume, and cleanup discipline as a real agent (ST-M003-004/005).
            fs_op @ (Operation::FsStat { .. }
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
            | Operation::StagingCleanup { .. }) => self.handle_file_op(framed, id, fs_op).await,
        }
    }

    /// Answers a file or staging operation by delegating to the real
    /// [`FileStore`] engine. A success returns the matching [`OpResult`]; a
    /// rejection is surfaced honestly with the engine's [`ErrorCode`] and
    /// message at [`ErrorStage::Execution`] — never dressed up (REQ-N-008).
    async fn handle_file_op<T: AsyncRead + AsyncWrite + Unpin>(
        &mut self,
        framed: &mut Framed<T>,
        id: RequestId,
        op: Operation,
    ) -> Result<Flow, SimulatorError> {
        let message = match self.vfs.perform(op).await {
            Ok(result) => Message::Response {
                id,
                session: self.session,
                result,
            },
            Err(err) => self.error_message(Some(id), err.code, err.message, ErrorStage::Execution),
        };
        framed.send(&message).await?;
        Ok(Flow::Continue)
    }

    async fn handle_exec<T: AsyncRead + AsyncWrite + Unpin>(
        &mut self,
        framed: &mut Framed<T>,
        id: RequestId,
        command: &str,
        working_dir: Option<String>,
        timeout_ms: Option<u64>,
    ) -> Result<Flow, SimulatorError> {
        let Some(script) = parse_scripted(command) else {
            framed
                .send(&self.error_message(
                    Some(id),
                    ErrorCode::Unsupported,
                    "the simulator only runs sim:* scripted commands",
                    ErrorStage::Execution,
                ))
                .await?;
            return Ok(Flow::Continue);
        };
        if matches!(script, Scripted::Malformed) {
            return Ok(Flow::WriteMalformedAndClose);
        }

        let running = self.jobs.values().filter(|state| state.running).count();
        if running >= MAX_RUNNING_JOBS {
            framed
                .send(&self.error_message(
                    Some(id),
                    ErrorCode::LimitExceeded,
                    "too many running jobs; cancel one before starting another",
                    ErrorStage::Execution,
                ))
                .await?;
            return Ok(Flow::Continue);
        }

        let hang = matches!(script, Scripted::Hang);
        let job = self.spawn_job(command, hang);
        // The effective working directory mirrors the real agent's semantics
        // (ST-M003-003, REQ-F-009): the requested directory verbatim, or a
        // plausible default consistent with this connection's private VFS root.
        let working_dir =
            working_dir.unwrap_or_else(|| self.vfs_root.to_string_lossy().into_owned());
        framed
            .send(&Message::Response {
                id,
                session: self.session,
                result: OpResult::ExecStarted { job, working_dir },
            })
            .await?;

        match script {
            Scripted::Ok(text) => {
                let mut data = text.into_bytes();
                data.push(b'\n');
                self.emit_output(framed, job, OutputStream::Stdout, data)
                    .await?;
                self.emit_exit(framed, job, Some(0), 0, JobOutcome::Completed)
                    .await?;
                Ok(Flow::Continue)
            }
            Scripted::Fail(code) => {
                let data = format!("simulated failure with exit code {code}\n").into_bytes();
                self.emit_output(framed, job, OutputStream::Stderr, data)
                    .await?;
                self.emit_exit(framed, job, Some(code), 0, JobOutcome::Failed)
                    .await?;
                Ok(Flow::Continue)
            }
            Scripted::Delay(ms) => {
                let ms = ms.min(limits::MAX_TIMEOUT_MS);
                self.arm_timer(job, ms, TimerExit::Completed);
                Ok(Flow::Continue)
            }
            Scripted::Timeout => {
                let ms = timeout_ms.unwrap_or(0);
                self.arm_timer(job, ms, TimerExit::TimedOut);
                Ok(Flow::Continue)
            }
            Scripted::Hang => Ok(Flow::Continue),
            // The connection drops mid-job: the peer must report the
            // outcome as unknown, never as success (REQ-N-008).
            Scripted::Disconnect => Ok(Flow::Close),
            Scripted::Malformed => Ok(Flow::WriteMalformedAndClose),
        }
    }

    async fn handle_write_stdin<T: AsyncRead + AsyncWrite + Unpin>(
        &mut self,
        framed: &mut Framed<T>,
        id: RequestId,
        job: JobId,
        data: B64Bytes,
    ) -> Result<(), SimulatorError> {
        let accepts_stdin = self
            .jobs
            .get(&job)
            .is_some_and(|state| state.running && (state.hang || state.terminal));
        if !accepts_stdin {
            framed
                .send(&self.error_message(
                    Some(id),
                    ErrorCode::UnknownJob,
                    "no running interactive job with that id",
                    ErrorStage::Execution,
                ))
                .await?;
            return Ok(());
        }
        framed
            .send(&Message::Response {
                id,
                session: self.session,
                result: OpResult::Ack {},
            })
            .await?;
        // Echo the bytes back as stdout to prove the stdin path.
        self.emit_output(framed, job, OutputStream::Stdout, data.into_inner())
            .await
    }

    /// Opens a deterministic simulated terminal: registers an interactive
    /// terminal job, answers [`OpResult::TermOpened`], and emits a fixed
    /// plain-text prompt as the first merged-output chunk (ADR-0010,
    /// ST-M003-009). Written stdin is echoed back by [`Self::handle_write_stdin`].
    async fn handle_term_open<T: AsyncRead + AsyncWrite + Unpin>(
        &mut self,
        framed: &mut Framed<T>,
        id: RequestId,
        cols: u16,
        rows: u16,
    ) -> Result<Flow, SimulatorError> {
        let running = self.jobs.values().filter(|state| state.running).count();
        if running >= MAX_RUNNING_JOBS {
            framed
                .send(&self.error_message(
                    Some(id),
                    ErrorCode::LimitExceeded,
                    "too many running jobs; cancel one before opening a terminal",
                    ErrorStage::Execution,
                ))
                .await?;
            return Ok(Flow::Continue);
        }
        let job = self.insert_job(format!("terminal {cols}x{rows}"), false, true);
        framed
            .send(&Message::Response {
                id,
                session: self.session,
                result: OpResult::TermOpened { job },
            })
            .await?;
        // A fixed prompt so an attach test sees immediate, deterministic output.
        self.emit_output(framed, job, OutputStream::Stdout, TERMINAL_PROMPT.to_vec())
            .await?;
        Ok(Flow::Continue)
    }

    /// Acknowledges a terminal resize (last-writer-wins). The simulator has no
    /// real console, so an in-range resize simply succeeds; an unknown or
    /// non-terminal job is refused honestly.
    async fn handle_term_resize<T: AsyncRead + AsyncWrite + Unpin>(
        &mut self,
        framed: &mut Framed<T>,
        id: RequestId,
        job: JobId,
    ) -> Result<(), SimulatorError> {
        let ok = self
            .jobs
            .get(&job)
            .is_some_and(|state| state.running && state.terminal);
        if !ok {
            framed
                .send(&self.error_message(
                    Some(id),
                    ErrorCode::UnknownJob,
                    "no running terminal with that id",
                    ErrorStage::Execution,
                ))
                .await?;
            return Ok(());
        }
        framed
            .send(&Message::Response {
                id,
                session: self.session,
                result: OpResult::Ack {},
            })
            .await?;
        Ok(())
    }

    /// Closes a terminal: acknowledges, then ends its job with an honest
    /// `Cancelled` outcome — the same teardown as [`Self::handle_cancel`], since
    /// a terminal is a job (ST-M003-009, REQ-N-008).
    async fn handle_term_close<T: AsyncRead + AsyncWrite + Unpin>(
        &mut self,
        framed: &mut Framed<T>,
        id: RequestId,
        job: JobId,
    ) -> Result<(), SimulatorError> {
        let closed = match self.jobs.get_mut(&job) {
            Some(state) if state.running && state.terminal => {
                state.running = false;
                Some(elapsed_ms(state.started))
            }
            _ => None,
        };
        if closed.is_some() {
            self.prune_finished_history();
        }
        let Some(duration_ms) = closed else {
            framed
                .send(&self.error_message(
                    Some(id),
                    ErrorCode::UnknownJob,
                    "no running terminal with that id",
                    ErrorStage::Execution,
                ))
                .await?;
            return Ok(());
        };
        self.timers.retain(|timer| timer.job != job);
        framed
            .send(&Message::Response {
                id,
                session: self.session,
                result: OpResult::Ack {},
            })
            .await?;
        framed
            .send(&Message::Event {
                session: self.session,
                event: EventKind::JobExited {
                    job,
                    exit_code: None,
                    duration_ms,
                    outcome: JobOutcome::Cancelled,
                },
            })
            .await?;
        Ok(())
    }

    async fn handle_cancel<T: AsyncRead + AsyncWrite + Unpin>(
        &mut self,
        framed: &mut Framed<T>,
        id: RequestId,
        job: JobId,
    ) -> Result<(), SimulatorError> {
        let cancelled = match self.jobs.get_mut(&job) {
            Some(state) if state.running => {
                state.running = false;
                Some(elapsed_ms(state.started))
            }
            _ => None,
        };
        if cancelled.is_some() {
            self.prune_finished_history();
        }
        let Some(duration_ms) = cancelled else {
            framed
                .send(&self.error_message(
                    Some(id),
                    ErrorCode::UnknownJob,
                    "no running job with that id",
                    ErrorStage::Execution,
                ))
                .await?;
            return Ok(());
        };
        self.timers.retain(|timer| timer.job != job);
        framed
            .send(&Message::Response {
                id,
                session: self.session,
                result: OpResult::Ack {},
            })
            .await?;
        framed
            .send(&Message::Event {
                session: self.session,
                event: EventKind::JobExited {
                    job,
                    exit_code: None,
                    duration_ms,
                    outcome: JobOutcome::Cancelled,
                },
            })
            .await?;
        Ok(())
    }

    async fn fire_due_timers<T: AsyncRead + AsyncWrite + Unpin>(
        &mut self,
        framed: &mut Framed<T>,
    ) -> Result<(), SimulatorError> {
        let now = Instant::now();
        let mut index = 0;
        while index < self.timers.len() {
            if self.timers[index].at > now {
                index += 1;
                continue;
            }
            let timer = self.timers.swap_remove(index);
            let (exit_code, outcome) = match timer.exit {
                TimerExit::Completed => (Some(0), JobOutcome::Completed),
                TimerExit::TimedOut => (None, JobOutcome::TimedOut),
            };
            self.emit_exit(framed, timer.job, exit_code, timer.duration_ms, outcome)
                .await?;
        }
        Ok(())
    }

    async fn emit_output<T: AsyncRead + AsyncWrite + Unpin>(
        &self,
        framed: &mut Framed<T>,
        job: JobId,
        stream: OutputStream,
        data: Vec<u8>,
    ) -> Result<(), SimulatorError> {
        framed
            .send(&Message::Event {
                session: self.session,
                event: EventKind::JobOutput {
                    job,
                    stream,
                    data: B64Bytes::new(data),
                },
            })
            .await?;
        Ok(())
    }

    async fn emit_exit<T: AsyncRead + AsyncWrite + Unpin>(
        &mut self,
        framed: &mut Framed<T>,
        job: JobId,
        exit_code: Option<i32>,
        duration_ms: u64,
        outcome: JobOutcome,
    ) -> Result<(), SimulatorError> {
        if let Some(state) = self.jobs.get_mut(&job) {
            state.running = false;
        }
        self.prune_finished_history();
        framed
            .send(&Message::Event {
                session: self.session,
                event: EventKind::JobExited {
                    job,
                    exit_code,
                    duration_ms,
                    outcome,
                },
            })
            .await?;
        Ok(())
    }

    /// Prunes the oldest finished jobs beyond [`FINISHED_JOB_HISTORY`] so
    /// per-connection state cannot grow without bound.
    fn prune_finished_history(&mut self) {
        let finished: Vec<JobId> = self
            .jobs
            .iter()
            .filter(|(_, state)| !state.running)
            .map(|(job, _)| *job)
            .collect();
        if finished.len() > FINISHED_JOB_HISTORY {
            for job in finished.iter().take(finished.len() - FINISHED_JOB_HISTORY) {
                self.jobs.remove(job);
            }
        }
    }

    fn spawn_job(&mut self, command: &str, hang: bool) -> JobId {
        self.insert_job(command.chars().take(80).collect(), hang, false)
    }

    /// Inserts a tracked job, returning its id.
    fn insert_job(&mut self, preview: String, hang: bool, terminal: bool) -> JobId {
        let job = JobId(self.next_job);
        self.next_job += 1;
        self.jobs.insert(
            job,
            Job {
                preview,
                running: true,
                hang,
                terminal,
                started: Instant::now(),
            },
        );
        job
    }

    fn arm_timer(&mut self, job: JobId, ms: u64, exit: TimerExit) {
        self.timers.push(Timer {
            at: Instant::now() + Duration::from_millis(ms),
            job,
            exit,
            duration_ms: ms,
        });
    }

    fn next_deadline(&self) -> Option<Instant> {
        self.timers.iter().map(|timer| timer.at).min()
    }

    fn error_message(
        &self,
        id: Option<RequestId>,
        code: ErrorCode,
        text: &str,
        stage: ErrorStage,
    ) -> Message {
        Message::Error {
            id,
            session: Some(self.session),
            code,
            message: truncate_message(text),
            stage,
        }
    }
}

impl Drop for Connection {
    /// Removes the connection's private VFS tree. Best-effort test cleanup: the
    /// staging root is created lazily, so it may not exist, and a failure here is
    /// nothing a simulated target needs to report.
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.vfs_root);
    }
}

/// A negotiation-stage error: no session exists yet, so none is claimed.
fn negotiation_error(code: ErrorCode, text: &str) -> Message {
    Message::Error {
        id: None,
        session: None,
        code,
        message: truncate_message(text),
        stage: ErrorStage::Negotiation,
    }
}

/// Writes a frame that is length-correct but not JSON, then closes.
async fn write_malformed_frame<T: AsyncRead + AsyncWrite + Unpin>(
    mut io: T,
) -> Result<(), SimulatorError> {
    let payload: &[u8] = b"this frame is deliberately not json";
    let declared = u32::try_from(payload.len()).unwrap_or(u32::MAX);
    io.write_all(&declared.to_le_bytes()).await?;
    io.write_all(payload).await?;
    io.flush().await?;
    Ok(())
}

/// Parses the deterministic scripted-command grammar.
fn parse_scripted(command: &str) -> Option<Scripted> {
    if let Some(rest) = command.strip_prefix("sim:ok") {
        if rest.is_empty() {
            return Some(Scripted::Ok(String::new()));
        }
        return rest
            .strip_prefix(' ')
            .map(|text| Scripted::Ok(text.to_string()));
    }
    if let Some(rest) = command.strip_prefix("sim:fail ") {
        return rest.trim().parse::<i32>().ok().map(Scripted::Fail);
    }
    if let Some(rest) = command.strip_prefix("sim:delay ") {
        return rest.trim().parse::<u64>().ok().map(Scripted::Delay);
    }
    match command {
        "sim:hang" => Some(Scripted::Hang),
        "sim:timeout" => Some(Scripted::Timeout),
        "sim:disconnect" => Some(Scripted::Disconnect),
        "sim:malformed" => Some(Scripted::Malformed),
        _ => None,
    }
}

/// Builds the wire summary for one tracked job.
fn summarise(job: JobId, state: &Job) -> JobSummary {
    JobSummary {
        job,
        command_preview: state.preview.clone(),
        running: state.running,
    }
}

/// Milliseconds elapsed since `started`, saturating.
fn elapsed_ms(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)
}

/// Truncates an error message to [`limits::MAX_ERROR_MESSAGE`] bytes on a
/// character boundary.
fn truncate_message(text: &str) -> String {
    let mut out = String::new();
    for character in text.chars() {
        if out.len() + character.len_utf8() > limits::MAX_ERROR_MESSAGE {
            break;
        }
        out.push(character);
    }
    out
}

#[cfg(test)]
mod tests;
