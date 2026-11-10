//! Plaintext test-harness session client for the CretLink wire protocol
//! (ST-M001-003).
//!
//! [`SessionClient`] and its [`SessionTarget`] MCP adapter are the plaintext
//! harness client the console and MCP integration tests drive against the
//! simulator over an in-memory duplex; the live console uses the Noise-secured
//! `cretlink-session` runtime instead (see [`crate::ConsoleSessionService`]).
//!
//! [`SessionClient`] drives one connection to a support agent (real or
//! simulated): it performs the `Hello`/`HelloAck` handshake, validates the
//! negotiated version, then multiplexes request/response correlation and an
//! event stream over a single framed byte channel, so callers can issue
//! concurrent requests and observe streamed job output at the same time.
//!
//! # Honest outcomes (REQ-N-008)
//!
//! The client never invents a success. A request that outlives the connection
//! resolves to [`SessionError::Disconnected`]; a job whose stream ends without
//! a terminal `JobExited` collects as [`JobOutcome::Unknown`]; completed,
//! failed, cancelled, and timed-out jobs each surface their true
//! [`JobOutcome`]. Transport EOF becomes a disconnect rather than a silent
//! hang, and a refused cancellation surfaces as an error instead of being
//! silently swallowed (stop paths fail closed).
//!
//! # Transport reuse and concurrency
//!
//! Framing is provided by [`cretlink_transport::Framed`] (ADR-0010) — the
//! console does not reimplement the wire format or its bounds checks. After
//! the handshake the stream is split (`tokio::io::split`) into a dedicated
//! reader task and a dedicated writer task:
//!
//! - the reader task is the only place [`Framed::recv`] is awaited, so its
//!   cancel-safe state machine is never raced against writes and a frame
//!   delivered in arbitrarily small pieces can never desynchronise the
//!   stream;
//! - the writer task drains a queue of pre-encoded frames, so a
//!   back-pressured write never stops inbound frames from being read (no
//!   half-duplex deadlock with a peer that writes before it reads), and a
//!   locally invalid message fails only its own request — it is rejected by
//!   [`Message::encode`] before anything reaches the wire, leaving the
//!   session healthy.
//!
//! Inbound frames whose `session` stamp does not match the negotiated
//! session are dropped so results stay attributable to exactly one target
//! session (REQ-F-016); the simulator enforces the same rule in the other
//! direction.
//!
//! # MCP bridge adapter
//!
//! [`SessionTarget`] wraps a [`SessionClient`] as a [`cretlink_mcp::TargetSession`],
//! so the console can host the MCP bridge over exactly the target the operator
//! selected (REQ-F-013, REQ-F-016).

use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use cretlink_protocol::{
    B64Bytes, CleanupTrigger, ErrorCode, ErrorStage, EventKind, JobId, JobOutcome, JobSummary,
    KeyAction, Message, OpResult, Operation, Origin, OutputStream, PROTOCOL_V1, PointerAction,
    PointerButton, ProtocolError, ProtocolVersion, Purpose, RemotePath, RequestId, Role, SessionId,
    Shell, StagedEntry, TargetIdentity,
};
use cretlink_transport::{Framed, TransportError};
use thiserror::Error;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::{broadcast, mpsc, oneshot};
use tokio::task::JoinHandle;

/// Capacity of the per-client broadcast event stream, sized to bound
/// technician-side memory: the broadcast ring retains up to this many events
/// for the lifetime of the channel, and each `JobOutput` event may legally
/// carry [`cretlink_protocol::limits::MAX_CHUNK_BYTES`] bytes, so the
/// worst-case retained payload is `EVENT_CHANNEL_CAPACITY * MAX_CHUNK_BYTES`
/// (32 MiB at 128 x 256 KiB). Remote output is untrusted input; a consumer
/// that overruns this window is reported honestly as truncation (and, if the
/// terminal event was lost, an [`JobOutcome::Unknown`] outcome), never a
/// hang.
const EVENT_CHANNEL_CAPACITY: usize = 128;

/// A failure surfaced by a [`SessionClient`] operation.
#[derive(Debug, Error)]
pub enum SessionError {
    /// A transport read or write failed.
    #[error("i/o error on the session transport: {0}")]
    Io(#[from] std::io::Error),
    /// A message could not be encoded or decoded.
    #[error("protocol error: {0}")]
    Protocol(#[from] ProtocolError),
    /// A frame's declared length exceeded
    /// [`cretlink_protocol::limits::MAX_FRAME_BYTES`].
    #[error("frame of {len} bytes exceeds the {max}-byte limit")]
    Oversized {
        /// The declared frame length in bytes.
        len: usize,
        /// The enforced maximum in bytes.
        max: usize,
    },
    /// The peer offered a protocol version with a different major.
    #[error("incompatible protocol version: local {local}, peer {peer}")]
    IncompatibleVersion {
        /// This client's version.
        local: ProtocolVersion,
        /// The version the peer offered.
        peer: ProtocolVersion,
    },
    /// The agent returned a wire `Error` message (or a locally detected
    /// framing fault attributed to the pending request).
    #[error("agent error [{code:?} / {stage:?}]: {message}")]
    Remote {
        /// Machine-readable category.
        code: ErrorCode,
        /// Human-readable description.
        message: String,
        /// Which processing stage failed.
        stage: ErrorStage,
    },
    /// The session ended before the operation completed; the true outcome is
    /// unknown (REQ-N-008).
    #[error("session disconnected before the operation completed")]
    Disconnected,
    /// The agent replied with a message that did not fit the request.
    #[error("unexpected message from agent during {context}")]
    Unexpected {
        /// Where the unexpected message appeared.
        context: &'static str,
    },
}

impl From<TransportError> for SessionError {
    fn from(error: TransportError) -> Self {
        match error {
            TransportError::Io(io) => Self::Io(io),
            TransportError::Oversized { len, max } => Self::Oversized { len, max },
            TransportError::Decode(protocol) => Self::Protocol(protocol),
            TransportError::Eof => Self::Disconnected,
        }
    }
}

/// Internal carrier for a resolved request outcome sent from the router
/// tasks back to a waiting caller. Clonable so the same teardown reason can
/// fail every outstanding request.
#[derive(Debug, Clone)]
enum RemoteError {
    Remote {
        code: ErrorCode,
        message: String,
        stage: ErrorStage,
    },
    Disconnected,
}

impl From<RemoteError> for SessionError {
    fn from(value: RemoteError) -> Self {
        match value {
            RemoteError::Remote {
                code,
                message,
                stage,
            } => Self::Remote {
                code,
                message,
                stage,
            },
            RemoteError::Disconnected => Self::Disconnected,
        }
    }
}

/// A pre-encoded frame queued for the writer task, tagged with the request
/// it belongs to so a pre-write failure can fail exactly that request.
struct OutboundFrame {
    id: RequestId,
    payload: Vec<u8>,
}

/// Outstanding request registrations shared between callers, the reader
/// task, and the writer task.
#[derive(Debug)]
enum Pending {
    /// The session is live; requests may register and be resolved.
    Open(HashMap<RequestId, oneshot::Sender<Result<OpResult, RemoteError>>>),
    /// The session is torn down; new registrations fail immediately.
    Closed,
}

/// State shared by [`SessionClient`] and its router tasks.
#[derive(Debug)]
struct Shared {
    /// The negotiated session; inbound frames stamped with any other
    /// session are dropped (REQ-F-016).
    session: SessionId,
    pending: Mutex<Pending>,
}

impl Shared {
    fn new(session: SessionId) -> Self {
        Self {
            session,
            pending: Mutex::new(Pending::Open(HashMap::new())),
        }
    }

    /// Locks the pending map. A poisoned lock is impossible without a panic
    /// (denied by the workspace lints); recover the inner state regardless
    /// rather than propagating poison.
    fn lock(&self) -> MutexGuard<'_, Pending> {
        self.pending.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Registers a pending request; fails once the session is torn down so
    /// callers never wait on a reply that cannot come.
    fn register(
        &self,
        id: RequestId,
        reply: oneshot::Sender<Result<OpResult, RemoteError>>,
    ) -> Result<(), RemoteError> {
        match &mut *self.lock() {
            Pending::Open(map) => {
                map.insert(id, reply);
                Ok(())
            }
            Pending::Closed => Err(RemoteError::Disconnected),
        }
    }

    /// Resolves one pending request; unknown ids are ignored (the request
    /// may already have been failed by a teardown).
    fn resolve(&self, id: RequestId, result: Result<OpResult, RemoteError>) {
        let reply = match &mut *self.lock() {
            Pending::Open(map) => map.remove(&id),
            Pending::Closed => None,
        };
        if let Some(reply) = reply {
            let _ = reply.send(result);
        }
    }

    /// Removes a registration without answering it (used when enqueueing
    /// the frame failed and the caller reports the error itself).
    fn discard(&self, id: RequestId) {
        if let Pending::Open(map) = &mut *self.lock() {
            map.remove(&id);
        }
    }

    /// Fails every outstanding request with `reason` and refuses all future
    /// registrations.
    fn close(&self, reason: &RemoteError) {
        let drained = match std::mem::replace(&mut *self.lock(), Pending::Closed) {
            Pending::Open(map) => map,
            Pending::Closed => HashMap::new(),
        };
        for (_id, reply) in drained {
            let _ = reply.send(Err(reason.clone()));
        }
    }
}

/// A single shell command to run, decoupled from the wire operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecRequest {
    /// Which shell runs the command.
    pub shell: Shell,
    /// The command text.
    pub command: String,
    /// Optional working directory.
    pub working_dir: Option<String>,
    /// Extra environment variables.
    pub env: BTreeMap<String, String>,
    /// Optional deadline in milliseconds.
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

    fn into_operation(self) -> Operation {
        Operation::Exec {
            shell: self.shell,
            command: self.command,
            working_dir: self.working_dir,
            env: self.env,
            timeout_ms: self.timeout_ms,
        }
    }
}

/// The collected result of running one [`ExecRequest`] to completion.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecCollection {
    /// The job the agent assigned.
    pub job: JobId,
    /// The effective remote working directory the job received, as reported by
    /// the agent's `ExecStarted` (ST-M003-003, REQ-F-009).
    pub working_dir: String,
    /// Captured standard output bytes (raw; VT sequences passed through).
    pub stdout: Vec<u8>,
    /// Captured standard error bytes (raw).
    pub stderr: Vec<u8>,
    /// Whether output was capped or lost to a dropped stream.
    pub truncated: bool,
    /// The process exit code, when one exists.
    pub exit_code: Option<i32>,
    /// Wall-clock job duration in milliseconds.
    pub duration_ms: u64,
    /// The honest job outcome (REQ-N-008).
    pub outcome: JobOutcome,
}

/// A connected console session bound to one remote target.
#[derive(Debug)]
pub struct SessionClient {
    session: SessionId,
    target: TargetIdentity,
    next_id: AtomicU64,
    shared: Arc<Shared>,
    outbound: mpsc::UnboundedSender<OutboundFrame>,
    /// A parked receiver used only as an anchor for [`Self::subscribe`]. The
    /// reader task owns the sole sender, so when it ends every live receiver
    /// observes a closed stream — which is how [`Self::exec_collect`] detects a
    /// lost session (REQ-N-008).
    events: broadcast::Receiver<EventKind>,
    _reader: JoinHandle<()>,
    _writer: JoinHandle<()>,
}

impl std::fmt::Debug for OutboundFrame {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OutboundFrame")
            .field("id", &self.id)
            .field("payload_len", &self.payload.len())
            .finish()
    }
}

impl SessionClient {
    /// Performs the `Hello`/`HelloAck` handshake over `io`, validates that the
    /// peer's major version matches, and starts the background reader and
    /// writer tasks.
    ///
    /// # Errors
    ///
    /// Returns [`SessionError::IncompatibleVersion`] on a major mismatch,
    /// [`SessionError::Remote`] if the agent answers `Hello` with an `Error`,
    /// and [`SessionError::Disconnected`] if the peer closes during the
    /// handshake.
    pub async fn connect<T>(io: T, capabilities: Vec<String>) -> Result<Self, SessionError>
    where
        T: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
        let mut framed = Framed::new(io);
        let hello = Message::Hello {
            protocol: PROTOCOL_V1,
            role: Role::Console,
            capabilities,
        };
        framed.send(&hello).await.map_err(SessionError::from)?;

        let ack = framed.recv().await.map_err(SessionError::from)?;
        let (session, target, protocol) = match ack {
            Message::HelloAck {
                protocol,
                session,
                target,
            } => (session, target, protocol),
            Message::Error {
                code,
                message,
                stage,
                ..
            } => {
                return Err(SessionError::Remote {
                    code,
                    message,
                    stage,
                });
            }
            _ => {
                return Err(SessionError::Unexpected {
                    context: "handshake",
                });
            }
        };

        if !PROTOCOL_V1.is_compatible_with(protocol) {
            return Err(SessionError::IncompatibleVersion {
                local: PROTOCOL_V1,
                peer: protocol,
            });
        }

        let (read_half, write_half) = tokio::io::split(framed.into_inner());
        let shared = Arc::new(Shared::new(session));
        let (outbound_tx, outbound_rx) = mpsc::unbounded_channel();
        let (events_tx, events_rx) = broadcast::channel(EVENT_CHANNEL_CAPACITY);
        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        let writer = tokio::spawn(run_writer(
            Framed::new(write_half),
            outbound_rx,
            Arc::clone(&shared),
            shutdown_tx,
        ));
        let reader = tokio::spawn(run_reader(
            Framed::new(read_half),
            Arc::clone(&shared),
            events_tx,
            shutdown_rx,
        ));

        Ok(Self {
            session,
            target,
            next_id: AtomicU64::new(0),
            shared,
            outbound: outbound_tx,
            events: events_rx,
            _reader: reader,
            _writer: writer,
        })
    }

    /// The session this connection belongs to.
    #[must_use]
    pub fn session(&self) -> SessionId {
        self.session
    }

    /// The identity of the remote target (REQ-F-016).
    #[must_use]
    pub fn target(&self) -> &TargetIdentity {
        &self.target
    }

    /// Subscribes to the stream of unsolicited events. Only events emitted
    /// after this call are delivered to the returned receiver, and a slow
    /// consumer can lag out of the bounded window (see
    /// `EVENT_CHANNEL_CAPACITY`).
    #[must_use]
    pub fn subscribe(&self) -> broadcast::Receiver<EventKind> {
        self.events.resubscribe()
    }

    fn next_request_id(&self) -> RequestId {
        RequestId(self.next_id.fetch_add(1, Ordering::Relaxed))
    }

    /// Validates and encodes `message` locally, queues it for the writer
    /// task, and awaits the correlated response.
    ///
    /// A message that violates a protocol limit is rejected here, before
    /// anything reaches the wire: only this call fails and the session stays
    /// healthy.
    async fn send_correlated(
        &self,
        id: RequestId,
        message: &Message,
    ) -> Result<OpResult, SessionError> {
        let payload = message.encode()?;
        let (reply_tx, reply_rx) = oneshot::channel();
        self.shared
            .register(id, reply_tx)
            .map_err(SessionError::from)?;
        if self.outbound.send(OutboundFrame { id, payload }).is_err() {
            self.shared.discard(id);
            return Err(SessionError::Disconnected);
        }
        match reply_rx.await {
            Ok(Ok(result)) => Ok(result),
            Ok(Err(remote)) => Err(remote.into()),
            Err(_) => Err(SessionError::Disconnected),
        }
    }

    /// Sends one operation and awaits its correlated response.
    ///
    /// # Errors
    ///
    /// Surfaces [`SessionError::Protocol`] when the operation violates a
    /// protocol limit (nothing is sent and the session stays healthy),
    /// [`SessionError::Remote`] for an agent error, and
    /// [`SessionError::Disconnected`] if the session ends first.
    pub async fn request(&self, op: Operation) -> Result<OpResult, SessionError> {
        let id = self.next_request_id();
        let message = Message::Request {
            id,
            session: self.session,
            // This legacy console wire client is technician-driven (the MCP
            // path routes through `ConsoleSessionService`); it is not the AI.
            origin: Origin::Console,
            purpose: None,
            op,
        };
        self.send_correlated(id, &message).await
    }

    /// Requests cancellation of a running job and awaits the agent's
    /// acknowledgement, so a refused cancellation is surfaced rather than
    /// swallowed — stop paths fail closed. The job's terminal outcome is
    /// still observed as a `JobExited` event.
    ///
    /// # Errors
    ///
    /// Returns [`SessionError::Remote`] (for example `UnknownJob`) when the
    /// agent rejects the cancellation, and [`SessionError::Disconnected`] if
    /// the session has ended.
    pub async fn cancel(&self, job: JobId) -> Result<(), SessionError> {
        let id = self.next_request_id();
        let message = Message::Cancel {
            id,
            session: self.session,
            // Technician-driven, like every request of this legacy client.
            origin: Origin::Console,
            purpose: None,
            job,
        };
        match self.send_correlated(id, &message).await? {
            OpResult::Ack {} => Ok(()),
            _ => Err(SessionError::Unexpected { context: "cancel" }),
        }
    }

    /// Reports the target identity and pending-reboot state.
    ///
    /// # Errors
    ///
    /// See [`Self::request`].
    pub async fn system_info(&self) -> Result<(TargetIdentity, bool), SessionError> {
        match self.request(Operation::SystemInfo {}).await? {
            OpResult::SystemInfo {
                target,
                reboot_pending,
            } => Ok((target, reboot_pending)),
            _ => Err(SessionError::Unexpected {
                context: "system_info",
            }),
        }
    }

    /// Lists the jobs the agent currently tracks.
    ///
    /// # Errors
    ///
    /// See [`Self::request`].
    pub async fn list_jobs(&self) -> Result<Vec<JobSummary>, SessionError> {
        match self.request(Operation::ListJobs {}).await? {
            OpResult::Jobs { jobs } => Ok(jobs),
            _ => Err(SessionError::Unexpected {
                context: "list_jobs",
            }),
        }
    }

    /// Writes bytes to a running job's standard input.
    ///
    /// # Errors
    ///
    /// See [`Self::request`].
    pub async fn write_stdin(&self, job: JobId, data: Vec<u8>) -> Result<(), SessionError> {
        match self
            .request(Operation::WriteStdin {
                job,
                data: B64Bytes::new(data),
            })
            .await?
        {
            OpResult::Ack {} => Ok(()),
            _ => Err(SessionError::Unexpected {
                context: "write_stdin",
            }),
        }
    }

    /// Runs an exec to completion, capturing output up to `max_output_bytes`
    /// across both streams and returning the honest outcome (REQ-N-008).
    ///
    /// Subscribes to the event stream *before* starting the job so no output
    /// is missed. A dropped stream or an overrun of the byte cap is reported
    /// via [`ExecCollection::truncated`]; a session that ends without a
    /// terminal `JobExited` yields [`JobOutcome::Unknown`]. If the bounded
    /// event window overruns (`Lagged`), the client resynchronises against
    /// the agent's job list: a job that ended inside the lost window forfeits
    /// its exact outcome and is reported honestly as [`JobOutcome::Unknown`]
    /// with `truncated` set, never awaited forever.
    ///
    /// # Errors
    ///
    /// Surfaces a start-time [`SessionError`] (for example the agent replying
    /// with a malformed frame or an `Unsupported` error) before any output is
    /// collected.
    pub async fn exec_collect(
        &self,
        request: ExecRequest,
        max_output_bytes: usize,
    ) -> Result<ExecCollection, SessionError> {
        let mut events = self.subscribe();
        let (job, working_dir) = match self.request(request.into_operation()).await? {
            OpResult::ExecStarted { job, working_dir } => (job, working_dir),
            _ => {
                return Err(SessionError::Unexpected {
                    context: "exec start",
                });
            }
        };

        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let mut total = 0usize;
        let mut truncated = false;

        loop {
            match events.recv().await {
                Ok(EventKind::JobOutput {
                    job: j,
                    stream,
                    data,
                }) if j == job => {
                    let buf = match stream {
                        OutputStream::Stdout => &mut stdout,
                        OutputStream::Stderr => &mut stderr,
                    };
                    append_capped(
                        buf,
                        data.as_bytes(),
                        max_output_bytes,
                        &mut total,
                        &mut truncated,
                    );
                }
                Ok(EventKind::JobExited {
                    job: j,
                    exit_code,
                    duration_ms,
                    outcome,
                }) if j == job => {
                    return Ok(ExecCollection {
                        job,
                        working_dir,
                        stdout,
                        stderr,
                        truncated,
                        exit_code,
                        duration_ms,
                        outcome,
                    });
                }
                Ok(EventKind::Disconnected { job: j, .. }) if j == job => {
                    return Ok(unknown_collection(
                        job,
                        working_dir,
                        stdout,
                        stderr,
                        truncated,
                    ));
                }
                Ok(_) => {}
                Err(broadcast::error::RecvError::Lagged(_)) => {
                    // The bounded ring overran: bytes were lost, and the
                    // lost window may have contained this job's terminal
                    // event. Resynchronise against the agent's job list
                    // instead of waiting for a frame that may never come.
                    truncated = true;
                    match self.list_jobs().await {
                        Ok(jobs)
                            if jobs
                                .iter()
                                .any(|summary| summary.job == job && summary.running) => {}
                        Ok(_) => {
                            return Ok(unknown_collection(job, working_dir, stdout, stderr, true));
                        }
                        Err(_) => {
                            return Ok(unknown_collection(job, working_dir, stdout, stderr, true));
                        }
                    }
                }
                Err(broadcast::error::RecvError::Closed) => {
                    return Ok(unknown_collection(
                        job,
                        working_dir,
                        stdout,
                        stderr,
                        truncated,
                    ));
                }
            }
        }
    }
}

/// The reader task: the only place inbound frames are received. Routes each
/// message to its waiting request or the broadcast event stream; on any
/// terminal condition it fails every outstanding request with the teardown
/// reason and drops the event sender, which subscribers observe as a closed
/// stream.
async fn run_reader<R>(
    mut framed: Framed<R>,
    shared: Arc<Shared>,
    events_tx: broadcast::Sender<EventKind>,
    mut shutdown_rx: oneshot::Receiver<RemoteError>,
) where
    R: AsyncRead + Unpin + Send + 'static,
{
    let reason = loop {
        tokio::select! {
            // `Framed::recv` is documented cancel-safe: a poll interrupted
            // by the shutdown branch keeps its partial frame inside the
            // state machine.
            received = framed.recv() => match received {
                Ok(message) => route(message, &shared, &events_tx),
                Err(error) => break reader_teardown(&error),
            },
            shutdown = &mut shutdown_rx => {
                break shutdown.unwrap_or(RemoteError::Disconnected);
            }
        }
    };
    shared.close(&reason);
}

/// The writer task: drains pre-encoded frames so a back-pressured write
/// never blocks the reader. An error rejected before any bytes were written
/// fails only the offending request; a possibly-partial write tears the
/// session down (the stream can no longer be trusted) via the shutdown
/// signal to the reader.
async fn run_writer<W>(
    mut framed: Framed<W>,
    mut outbound_rx: mpsc::UnboundedReceiver<OutboundFrame>,
    shared: Arc<Shared>,
    shutdown_tx: oneshot::Sender<RemoteError>,
) where
    W: AsyncWrite + Unpin + Send + 'static,
{
    while let Some(frame) = outbound_rx.recv().await {
        match framed.send_encoded(&frame.payload).await {
            Ok(()) => {}
            Err(TransportError::Oversized { len, max }) => {
                // Rejected before any bytes reached the wire (and normally
                // unreachable: `Message::encode` enforces the frame cap):
                // the stream is intact, so only this request fails.
                shared.resolve(
                    frame.id,
                    Err(RemoteError::Remote {
                        code: ErrorCode::OversizedMessage,
                        message: format!("frame of {len} bytes exceeds the {max}-byte limit"),
                        stage: ErrorStage::Transport,
                    }),
                );
            }
            Err(_) => {
                // An i/o failure may have left a partial frame on the
                // stream: fail this request and tear the session down.
                shared.resolve(frame.id, Err(RemoteError::Disconnected));
                let _ = shutdown_tx.send(RemoteError::Disconnected);
                return;
            }
        }
    }
    // The client was dropped (the sole outbound sender is gone): dropping
    // `shutdown_tx` wakes the reader, which tears the session down.
}

/// Routes one inbound message to its waiting request or the event stream.
/// Messages the console never expects from an agent (another `Hello`, a
/// `Request`) and any message stamped with a session this connection does
/// not own are dropped (REQ-F-016), mirroring the agent-side
/// `UnknownSession` check.
fn route(incoming: Message, shared: &Shared, events_tx: &broadcast::Sender<EventKind>) {
    match incoming {
        Message::Response {
            id,
            session,
            result,
        } => {
            if session == shared.session {
                shared.resolve(id, Ok(result));
            }
        }
        Message::Error {
            id: Some(id),
            session,
            code,
            message,
            stage,
        } => {
            // Negotiation-stage errors may legitimately carry no session;
            // a mismatching one is dropped.
            if session.is_none_or(|stamped| stamped == shared.session) {
                shared.resolve(
                    id,
                    Err(RemoteError::Remote {
                        code,
                        message,
                        stage,
                    }),
                );
            }
        }
        Message::Event { session, event } if session == shared.session => {
            let _ = events_tx.send(event);
        }
        _ => {}
    }
}

/// Maps a reader-side transport failure to the teardown reason used to fail
/// outstanding requests: a decodable-but-invalid frame is attributed as the
/// protocol error it is, an oversized prefix as `OversizedMessage`, and EOF
/// or an i/o fault as a disconnect (REQ-N-008).
fn reader_teardown(error: &TransportError) -> RemoteError {
    match error {
        TransportError::Decode(protocol) => RemoteError::Remote {
            code: protocol.code(),
            message: protocol.to_string(),
            stage: ErrorStage::Validation,
        },
        TransportError::Oversized { len, max } => RemoteError::Remote {
            code: ErrorCode::OversizedMessage,
            message: format!("frame of {len} bytes exceeds the {max}-byte limit"),
            stage: ErrorStage::Transport,
        },
        TransportError::Io(_) | TransportError::Eof => RemoteError::Disconnected,
    }
}

/// Builds the honest collection for a job whose exact outcome could not be
/// determined (lost session, dropped stream, or a lagged-out terminal
/// event) — never a fabricated success (REQ-N-008).
fn unknown_collection(
    job: JobId,
    working_dir: String,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    truncated: bool,
) -> ExecCollection {
    ExecCollection {
        job,
        working_dir,
        stdout,
        stderr,
        truncated,
        exit_code: None,
        duration_ms: 0,
        outcome: JobOutcome::Unknown,
    }
}

/// Appends `data` to `buf`, stopping at the combined `cap`; records truncation
/// honestly when the cap trims bytes.
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

/// Adapts a [`SessionClient`] to the MCP bridge's [`cretlink_mcp::TargetSession`],
/// so the console can serve the MCP surface bound to exactly the selected
/// target (REQ-F-013, REQ-F-016).
///
/// This in-process adapter drives the simulator over the legacy wire client,
/// which stamps every request as technician-driven; it does not forward the
/// AI's stated purpose (ST-M009-007). The production path is the IPC route
/// through [`crate::ConsoleSessionService`], which audits, shows, and forwards
/// it.
#[derive(Debug, Clone)]
pub struct SessionTarget {
    client: Arc<SessionClient>,
}

impl SessionTarget {
    /// Wraps a shared session client as an MCP target.
    #[must_use]
    pub fn new(client: Arc<SessionClient>) -> Self {
        Self { client }
    }

    /// Labels a value with the bound session's identity (REQ-F-016). The
    /// identity is fixed for the life of the client, so it faithfully names the
    /// session that produced the result.
    fn routed<T>(&self, value: T) -> cretlink_mcp::Routed<T> {
        cretlink_mcp::Routed {
            session: self.client.session(),
            target: self.client.target().clone(),
            // This in-process adapter does not route through the console IPC
            // server's audit, so it mints no `call_id`; `0` is the sentinel for
            // "no console call id" (ST-M004-006).
            call_id: 0,
            value,
        }
    }
}

#[async_trait::async_trait]
impl cretlink_mcp::TargetSession for SessionTarget {
    fn session_id(&self) -> SessionId {
        self.client.session()
    }

    fn target(&self) -> TargetIdentity {
        self.client.target().clone()
    }

    async fn system_info(
        &self,
        _purpose: Purpose,
    ) -> Result<cretlink_mcp::Routed<cretlink_mcp::SystemInfoReport>, cretlink_mcp::CallError> {
        let (target, reboot_pending) = self.client.system_info().await.map_err(target_error)?;
        Ok(self.routed(cretlink_mcp::SystemInfoReport {
            target,
            reboot_pending,
        }))
    }

    async fn exec(
        &self,
        _purpose: Purpose,
        params: cretlink_mcp::ExecParams,
        output_cap: usize,
    ) -> Result<cretlink_mcp::Routed<cretlink_mcp::ExecReport>, cretlink_mcp::CallError> {
        let request = ExecRequest {
            shell: params.shell,
            command: params.command,
            working_dir: params.working_dir,
            env: params.env,
            timeout_ms: params.timeout_ms,
        };
        let collected = self
            .client
            .exec_collect(request, output_cap)
            .await
            .map_err(target_error)?;
        Ok(self.routed(cretlink_mcp::ExecReport {
            job: collected.job,
            working_dir: collected.working_dir,
            stdout: collected.stdout,
            stderr: collected.stderr,
            truncated: collected.truncated,
            exit_code: collected.exit_code,
            duration_ms: collected.duration_ms,
            outcome: collected.outcome,
        }))
    }

    async fn write_stdin(
        &self,
        _purpose: Purpose,
        job: JobId,
        data: Vec<u8>,
        _sensitive: bool,
    ) -> Result<cretlink_mcp::Routed<()>, cretlink_mcp::CallError> {
        // The sensitive flag is audit-only (REQ-S-018) and stdin content is
        // structurally never audited, so this direct adapter forwards bytes
        // unchanged.
        self.client
            .write_stdin(job, data)
            .await
            .map_err(target_error)?;
        Ok(self.routed(()))
    }

    async fn cancel(
        &self,
        _purpose: Purpose,
        job: JobId,
    ) -> Result<cretlink_mcp::Routed<()>, cretlink_mcp::CallError> {
        self.client.cancel(job).await.map_err(target_error)?;
        Ok(self.routed(()))
    }

    async fn list_jobs(
        &self,
        _purpose: Purpose,
    ) -> Result<cretlink_mcp::Routed<Vec<JobSummary>>, cretlink_mcp::CallError> {
        let jobs = self.client.list_jobs().await.map_err(target_error)?;
        Ok(self.routed(jobs))
    }

    async fn fs_stat(
        &self,
        _purpose: Purpose,
        path: RemotePath,
    ) -> Result<cretlink_mcp::Routed<cretlink_mcp::StatReport>, cretlink_mcp::CallError> {
        match self
            .client
            .request(Operation::FsStat { path })
            .await
            .map_err(target_error)?
        {
            OpResult::PathStat { entry, partial_len } => {
                Ok(self.routed(cretlink_mcp::StatReport { entry, partial_len }))
            }
            _ => Err(cretlink_mcp::TargetError::Unexpected.into()),
        }
    }

    async fn fs_list(
        &self,
        _purpose: Purpose,
        path: RemotePath,
        continue_after: Option<String>,
    ) -> Result<cretlink_mcp::Routed<cretlink_mcp::DirPage>, cretlink_mcp::CallError> {
        match self
            .client
            .request(Operation::FsList {
                path,
                continue_after,
            })
            .await
            .map_err(target_error)?
        {
            OpResult::DirEntries { entries, end } => {
                Ok(self.routed(cretlink_mcp::DirPage { entries, end }))
            }
            _ => Err(cretlink_mcp::TargetError::Unexpected.into()),
        }
    }

    async fn make_dir(
        &self,
        _purpose: Purpose,
        path: RemotePath,
    ) -> Result<cretlink_mcp::Routed<()>, cretlink_mcp::CallError> {
        match self
            .client
            .request(Operation::CreateDir { path })
            .await
            .map_err(target_error)?
        {
            OpResult::Ack {} => Ok(self.routed(())),
            _ => Err(cretlink_mcp::TargetError::Unexpected.into()),
        }
    }

    async fn fs_move(
        &self,
        _purpose: Purpose,
        from: RemotePath,
        to: RemotePath,
        overwrite: bool,
    ) -> Result<cretlink_mcp::Routed<()>, cretlink_mcp::CallError> {
        match self
            .client
            .request(Operation::FsMove {
                from,
                to,
                overwrite,
            })
            .await
            .map_err(target_error)?
        {
            OpResult::Ack {} => Ok(self.routed(())),
            _ => Err(cretlink_mcp::TargetError::Unexpected.into()),
        }
    }

    async fn fs_delete(
        &self,
        _purpose: Purpose,
        path: RemotePath,
    ) -> Result<cretlink_mcp::Routed<()>, cretlink_mcp::CallError> {
        match self
            .client
            .request(Operation::FsDelete { path })
            .await
            .map_err(target_error)?
        {
            OpResult::Ack {} => Ok(self.routed(())),
            _ => Err(cretlink_mcp::TargetError::Unexpected.into()),
        }
    }

    async fn remove_dir(
        &self,
        _purpose: Purpose,
        path: RemotePath,
    ) -> Result<cretlink_mcp::Routed<()>, cretlink_mcp::CallError> {
        match self
            .client
            .request(Operation::RemoveDir { path })
            .await
            .map_err(target_error)?
        {
            OpResult::Ack {} => Ok(self.routed(())),
            _ => Err(cretlink_mcp::TargetError::Unexpected.into()),
        }
    }

    async fn list_staged(
        &self,
        _purpose: Purpose,
    ) -> Result<cretlink_mcp::Routed<Vec<StagedEntry>>, cretlink_mcp::CallError> {
        match self
            .client
            .request(Operation::ListStaged {})
            .await
            .map_err(target_error)?
        {
            OpResult::StagedList { entries } => Ok(self.routed(entries)),
            _ => Err(cretlink_mcp::TargetError::Unexpected.into()),
        }
    }

    async fn cleanup(
        &self,
        _purpose: Purpose,
        trigger: CleanupTrigger,
    ) -> Result<cretlink_mcp::Routed<cretlink_mcp::CleanupSummary>, cretlink_mcp::CallError> {
        match self
            .client
            .request(Operation::StagingCleanup { trigger })
            .await
            .map_err(target_error)?
        {
            OpResult::CleanupReport { removed, failed } => {
                Ok(self.routed(cretlink_mcp::CleanupSummary { removed, failed }))
            }
            _ => Err(cretlink_mcp::TargetError::Unexpected.into()),
        }
    }

    // --- Visual desktop control (ADR-0015, ST-M005-004) ---
    //
    // Unlike upload/download (which need the resumable transfer engine), the
    // desktop ops are ordinary single-shot wire operations, so this raw-client
    // adapter drives them genuinely. A non-live desktop answers with honest
    // metadata and no pixels rather than a fabricated frame (REQ-N-008).

    async fn capture_desktop(
        &self,
        _purpose: Purpose,
        params: cretlink_mcp::CaptureParams,
    ) -> Result<cretlink_mcp::Routed<cretlink_mcp::CaptureReport>, cretlink_mcp::CallError> {
        let cretlink_mcp::CaptureParams {
            display,
            format,
            max_width,
            max_height,
            quality,
        } = params;
        match self
            .client
            .request(Operation::DesktopCapture {
                display,
                format,
                max_width,
                max_height,
                quality,
            })
            .await
            .map_err(target_error)?
        {
            OpResult::DesktopFrame {
                meta,
                image,
                staged,
            } => Ok(self.routed(cretlink_mcp::CaptureReport {
                meta,
                image,
                staged,
            })),
            _ => Err(cretlink_mcp::TargetError::Unexpected.into()),
        }
    }

    async fn desktop_pointer(
        &self,
        _purpose: Purpose,
        action: PointerAction,
        button: PointerButton,
        x: i32,
        y: i32,
    ) -> Result<cretlink_mcp::Routed<()>, cretlink_mcp::CallError> {
        match self
            .client
            .request(Operation::DesktopPointer {
                action,
                button,
                x,
                y,
            })
            .await
            .map_err(target_error)?
        {
            OpResult::Ack {} => Ok(self.routed(())),
            _ => Err(cretlink_mcp::TargetError::Unexpected.into()),
        }
    }

    async fn desktop_scroll(
        &self,
        _purpose: Purpose,
        x: i32,
        y: i32,
        dx: i32,
        dy: i32,
    ) -> Result<cretlink_mcp::Routed<()>, cretlink_mcp::CallError> {
        match self
            .client
            .request(Operation::DesktopScroll { x, y, dx, dy })
            .await
            .map_err(target_error)?
        {
            OpResult::Ack {} => Ok(self.routed(())),
            _ => Err(cretlink_mcp::TargetError::Unexpected.into()),
        }
    }

    async fn desktop_key(
        &self,
        _purpose: Purpose,
        action: KeyAction,
        key: String,
    ) -> Result<cretlink_mcp::Routed<()>, cretlink_mcp::CallError> {
        match self
            .client
            .request(Operation::DesktopKey { action, key })
            .await
            .map_err(target_error)?
        {
            OpResult::Ack {} => Ok(self.routed(())),
            _ => Err(cretlink_mcp::TargetError::Unexpected.into()),
        }
    }

    async fn desktop_type(
        &self,
        _purpose: Purpose,
        text: String,
    ) -> Result<cretlink_mcp::Routed<()>, cretlink_mcp::CallError> {
        match self
            .client
            .request(Operation::DesktopType { text })
            .await
            .map_err(target_error)?
        {
            OpResult::Ack {} => Ok(self.routed(())),
            _ => Err(cretlink_mcp::TargetError::Unexpected.into()),
        }
    }

    async fn desktop_hotkey(
        &self,
        _purpose: Purpose,
        keys: Vec<String>,
    ) -> Result<cretlink_mcp::Routed<()>, cretlink_mcp::CallError> {
        match self
            .client
            .request(Operation::DesktopHotkey { keys })
            .await
            .map_err(target_error)?
        {
            OpResult::Ack {} => Ok(self.routed(())),
            _ => Err(cretlink_mcp::TargetError::Unexpected.into()),
        }
    }

    async fn desktop_wait(
        &self,
        _purpose: Purpose,
        millis: u64,
    ) -> Result<cretlink_mcp::Routed<()>, cretlink_mcp::CallError> {
        match self
            .client
            .request(Operation::DesktopWait { millis })
            .await
            .map_err(target_error)?
        {
            OpResult::Ack {} => Ok(self.routed(())),
            _ => Err(cretlink_mcp::TargetError::Unexpected.into()),
        }
    }

    async fn clipboard_get(
        &self,
        _purpose: Purpose,
    ) -> Result<cretlink_mcp::Routed<Option<String>>, cretlink_mcp::CallError> {
        match self
            .client
            .request(Operation::ClipboardGet {})
            .await
            .map_err(target_error)?
        {
            OpResult::ClipboardContents { text } => Ok(self.routed(text)),
            _ => Err(cretlink_mcp::TargetError::Unexpected.into()),
        }
    }

    async fn clipboard_set(
        &self,
        _purpose: Purpose,
        text: String,
    ) -> Result<cretlink_mcp::Routed<()>, cretlink_mcp::CallError> {
        match self
            .client
            .request(Operation::ClipboardSet { text })
            .await
            .map_err(target_error)?
        {
            OpResult::Ack {} => Ok(self.routed(())),
            _ => Err(cretlink_mcp::TargetError::Unexpected.into()),
        }
    }

    /// Uploads are not wired through the raw-client bridge adapter.
    ///
    /// [`SessionTarget`] wraps a single-shot [`SessionClient`]; the resumable,
    /// verified transfer engine lives on the console runtime
    /// (`cretlink_session::HostHandle`, surfaced to the bridge over IPC by the
    /// console service), not on the raw wire client. The only consumer of this
    /// adapter is the in-process bridge/simulator test, which exercises the
    /// single-shot tools, so this path is never reached. Should it ever be
    /// called it fails closed as [`cretlink_mcp::TargetError::Unexpected`]
    /// rather than fabricating a transfer result (REQ-N-008).
    async fn upload(
        &self,
        _purpose: Purpose,
        _params: cretlink_mcp::UploadParams,
    ) -> Result<cretlink_mcp::Routed<cretlink_mcp::UploadReport>, cretlink_mcp::CallError> {
        Err(cretlink_mcp::TargetError::Unexpected.into())
    }

    /// Downloads are not wired through the raw-client bridge adapter; see
    /// [`upload`](cretlink_mcp::TargetSession::upload). Fails closed rather than fabricating a result.
    async fn download(
        &self,
        _purpose: Purpose,
        _params: cretlink_mcp::DownloadParams,
    ) -> Result<cretlink_mcp::Routed<cretlink_mcp::DownloadReport>, cretlink_mcp::CallError> {
        Err(cretlink_mcp::TargetError::Unexpected.into())
    }
}

/// Maps a [`SessionError`] onto the MCP bridge's honest [`cretlink_mcp::TargetError`],
/// keeping a lost session distinct from a remote-reported failure.
fn target_error(err: SessionError) -> cretlink_mcp::TargetError {
    match err {
        SessionError::Remote {
            code,
            message,
            stage,
        } => cretlink_mcp::TargetError::Remote {
            code,
            message,
            stage,
        },
        SessionError::Protocol(protocol) => cretlink_mcp::TargetError::Remote {
            code: protocol.code(),
            message: protocol.to_string(),
            stage: ErrorStage::Validation,
        },
        SessionError::Oversized { len, max } => cretlink_mcp::TargetError::Remote {
            code: ErrorCode::OversizedMessage,
            message: format!("frame of {len} bytes exceeds the {max}-byte limit"),
            stage: ErrorStage::Transport,
        },
        SessionError::IncompatibleVersion { local, peer } => cretlink_mcp::TargetError::Remote {
            code: ErrorCode::IncompatibleVersion,
            message: format!("incompatible protocol version: local {local}, peer {peer}"),
            stage: ErrorStage::Negotiation,
        },
        SessionError::Io(_) | SessionError::Disconnected => cretlink_mcp::TargetError::Disconnected,
        SessionError::Unexpected { .. } => cretlink_mcp::TargetError::Unexpected,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cretlink_protocol::limits;
    use std::time::Duration;
    use tokio::io::{AsyncWriteExt, DuplexStream};

    fn simulated_identity() -> TargetIdentity {
        TargetIdentity {
            hostname: "SIMULATED-TARGET".to_string(),
            os_version: "Simulated Windows".to_string(),
            arch: "x86_64".to_string(),
            elevated: false,
            simulated: true,
            agent_version: "fake-agent".to_string(),
            account: None,
        }
    }

    /// Encodes `message` into its raw wire bytes (u32 LE prefix + payload),
    /// for tests that write frames in deliberately split pieces.
    fn raw_frame(message: &Message) -> Vec<u8> {
        let payload = message.encode().unwrap();
        let mut raw = u32::try_from(payload.len()).unwrap().to_le_bytes().to_vec();
        raw.extend_from_slice(&payload);
        raw
    }

    /// What the fake agent's request servicing asks the outer loop to do.
    enum AgentFlow {
        Continue,
        /// Write a length-correct frame with invalid JSON, then close.
        MalformedAndClose,
    }

    /// A compact scripted agent that mirrors the simulator's command contract
    /// closely enough to exercise the client end to end. It mints a simulated
    /// target, then services one request at a time. The `protocol_major`
    /// argument lets a test force a version mismatch.
    async fn fake_agent(io: DuplexStream, protocol_major: u16) {
        let mut framed = Framed::new(io);
        if !matches!(framed.recv().await, Ok(Message::Hello { .. })) {
            return;
        }
        let session = SessionId::new();
        let ack = Message::HelloAck {
            protocol: ProtocolVersion::new(protocol_major, 0),
            session,
            target: simulated_identity(),
        };
        if framed.send(&ack).await.is_err() {
            return;
        }
        if protocol_major != PROTOCOL_V1.major {
            return; // the client will reject the version and hang up
        }

        let mut next_job: u64 = 1;
        loop {
            let message = match framed.recv().await {
                Ok(message) => message,
                Err(_) => return,
            };
            match message {
                Message::Request { id, op, .. } => {
                    match service_request(&mut framed, session, id, op, &mut next_job).await {
                        Ok(AgentFlow::Continue) => {}
                        Ok(AgentFlow::MalformedAndClose) => {
                            let mut io = framed.into_inner();
                            let payload: &[u8] = b"{ not valid json ";
                            let len = u32::try_from(payload.len()).unwrap();
                            let _ = io.write_all(&len.to_le_bytes()).await;
                            let _ = io.write_all(payload).await;
                            let _ = io.flush().await;
                            return;
                        }
                        Err(()) => return,
                    }
                }
                Message::Cancel { id, job, .. } => {
                    // Mirror the simulator: acknowledge the cancel, then
                    // report the honest Cancelled outcome as an event.
                    let ack = Message::Response {
                        id,
                        session,
                        result: OpResult::Ack {},
                    };
                    if send_ignoring(&mut framed, &ack).await.is_err() {
                        return;
                    }
                    let exited = Message::Event {
                        session,
                        event: EventKind::JobExited {
                            job,
                            exit_code: None,
                            duration_ms: 0,
                            outcome: JobOutcome::Cancelled,
                        },
                    };
                    if send_ignoring(&mut framed, &exited).await.is_err() {
                        return;
                    }
                }
                _ => {}
            }
        }
    }

    async fn send_ignoring(framed: &mut Framed<DuplexStream>, message: &Message) -> Result<(), ()> {
        framed.send(message).await.map_err(|_| ())
    }

    async fn service_request(
        framed: &mut Framed<DuplexStream>,
        session: SessionId,
        id: RequestId,
        op: Operation,
        next_job: &mut u64,
    ) -> Result<AgentFlow, ()> {
        match op {
            Operation::SystemInfo {} => {
                let response = Message::Response {
                    id,
                    session,
                    result: OpResult::SystemInfo {
                        target: simulated_identity(),
                        reboot_pending: false,
                    },
                };
                send_ignoring(framed, &response).await?;
                Ok(AgentFlow::Continue)
            }
            Operation::ListJobs {} => {
                let response = Message::Response {
                    id,
                    session,
                    result: OpResult::Jobs {
                        jobs: vec![JobSummary {
                            job: JobId(1),
                            command_preview: "hang".to_string(),
                            running: true,
                        }],
                    },
                };
                send_ignoring(framed, &response).await?;
                Ok(AgentFlow::Continue)
            }
            Operation::WriteStdin { job, data } => {
                let ack = Message::Response {
                    id,
                    session,
                    result: OpResult::Ack {},
                };
                send_ignoring(framed, &ack).await?;
                let echo = Message::Event {
                    session,
                    event: EventKind::JobOutput {
                        job,
                        stream: OutputStream::Stdout,
                        data,
                    },
                };
                send_ignoring(framed, &echo).await?;
                Ok(AgentFlow::Continue)
            }
            Operation::Exec { command, .. } => {
                service_exec(framed, session, id, &command, next_job).await
            }
            // The console test double does not implement the file (ADR-0012)
            // or terminal (ADR-0013) surfaces; it answers fail-closed with
            // `Unsupported`.
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
            | Operation::TermOpen { .. }
            | Operation::TermResize { .. }
            | Operation::TermClose { .. }
            // The console test double also has no interactive-desktop backend
            // (ADR-0015); refuse every desktop operation fail-closed on the same
            // Unsupported path a real agent without a desktop backend uses.
            | Operation::DesktopCapture { .. }
            | Operation::DesktopPointer { .. }
            | Operation::DesktopScroll { .. }
            | Operation::DesktopKey { .. }
            | Operation::DesktopType { .. }
            | Operation::DesktopHotkey { .. }
            | Operation::DesktopWait { .. }
            | Operation::ClipboardGet {}
            | Operation::ClipboardSet { .. } => {
                let error = Message::Error {
                    id: Some(id),
                    session: Some(session),
                    code: ErrorCode::Unsupported,
                    message: "the console test agent does not implement file operations"
                        .to_string(),
                    stage: ErrorStage::Validation,
                };
                send_ignoring(framed, &error).await?;
                Ok(AgentFlow::Continue)
            }
        }
    }

    async fn service_exec(
        framed: &mut Framed<DuplexStream>,
        session: SessionId,
        id: RequestId,
        command: &str,
        next_job: &mut u64,
    ) -> Result<AgentFlow, ()> {
        if command == "malformed" {
            return Ok(AgentFlow::MalformedAndClose);
        }

        let job = JobId(*next_job);
        *next_job += 1;
        let started = Message::Response {
            id,
            session,
            result: OpResult::ExecStarted {
                job,
                working_dir: "C:\\fake-agent-cwd".to_string(),
            },
        };
        send_ignoring(framed, &started).await?;

        match command {
            "ok" => {
                send_output(framed, session, job, OutputStream::Stdout, b"ok-output\n").await?;
                send_exited(framed, session, job, Some(0), JobOutcome::Completed).await?;
            }
            "fail" => {
                send_output(framed, session, job, OutputStream::Stderr, b"err-output\n").await?;
                send_exited(framed, session, job, Some(3), JobOutcome::Failed).await?;
            }
            "timeout" => send_exited(framed, session, job, None, JobOutcome::TimedOut).await?,
            "cancel" => send_exited(framed, session, job, None, JobOutcome::Cancelled).await?,
            "foreign-output" => {
                // The only output chunk is stamped with a session this
                // connection does not own: the client must drop it.
                let foreign = Message::Event {
                    session: SessionId::new(),
                    event: EventKind::JobOutput {
                        job,
                        stream: OutputStream::Stdout,
                        data: B64Bytes::from(&b"leaked"[..]),
                    },
                };
                send_ignoring(framed, &foreign).await?;
                send_exited(framed, session, job, Some(0), JobOutcome::Completed).await?;
            }
            "disconnect" => return Err(()), // drop the connection mid-job
            // "hang" and anything else: leave the job running, awaiting a
            // Cancel or WriteStdin driven by the test.
            _ => {}
        }
        Ok(AgentFlow::Continue)
    }

    async fn send_output(
        framed: &mut Framed<DuplexStream>,
        session: SessionId,
        job: JobId,
        stream: OutputStream,
        data: &[u8],
    ) -> Result<(), ()> {
        let event = Message::Event {
            session,
            event: EventKind::JobOutput {
                job,
                stream,
                data: B64Bytes::from(data),
            },
        };
        send_ignoring(framed, &event).await
    }

    async fn send_exited(
        framed: &mut Framed<DuplexStream>,
        session: SessionId,
        job: JobId,
        exit_code: Option<i32>,
        outcome: JobOutcome,
    ) -> Result<(), ()> {
        let event = Message::Event {
            session,
            event: EventKind::JobExited {
                job,
                exit_code,
                duration_ms: 1,
                outcome,
            },
        };
        send_ignoring(framed, &event).await
    }

    async fn connect_to_fake() -> SessionClient {
        let (agent_io, client_io) = tokio::io::duplex(64 * 1024);
        tokio::spawn(fake_agent(agent_io, PROTOCOL_V1.major));
        SessionClient::connect(client_io, vec!["exec".to_string()])
            .await
            .unwrap()
    }

    async fn start_hang(client: &SessionClient) -> JobId {
        let started = client
            .request(Operation::Exec {
                shell: Shell::Cmd,
                command: "hang".to_string(),
                working_dir: None,
                env: BTreeMap::new(),
                timeout_ms: None,
            })
            .await
            .unwrap();
        match started {
            OpResult::ExecStarted { job, .. } => job,
            other => panic!("expected ExecStarted, got {other:?}"),
        }
    }

    const CAP: usize = 64 * 1024;

    #[tokio::test]
    async fn handshake_exposes_simulated_identity() {
        let client = connect_to_fake().await;
        assert!(client.target().simulated);
        assert_eq!(client.target().hostname, "SIMULATED-TARGET");

        let (target, reboot_pending) = client.system_info().await.unwrap();
        assert!(target.simulated);
        assert!(!reboot_pending);
    }

    #[tokio::test]
    async fn exec_ok_collects_completed_output() {
        let client = connect_to_fake().await;
        let result = client
            .exec_collect(ExecRequest::new(Shell::PowerShell, "ok"), CAP)
            .await
            .unwrap();
        assert_eq!(result.outcome, JobOutcome::Completed);
        assert_eq!(result.exit_code, Some(0));
        assert_eq!(result.stdout, b"ok-output\n");
        assert!(!result.truncated);
    }

    #[tokio::test]
    async fn exec_fail_reports_failed_outcome() {
        let client = connect_to_fake().await;
        let result = client
            .exec_collect(ExecRequest::new(Shell::Cmd, "fail"), CAP)
            .await
            .unwrap();
        assert_eq!(result.outcome, JobOutcome::Failed);
        assert_eq!(result.exit_code, Some(3));
        assert_eq!(result.stderr, b"err-output\n");
    }

    #[tokio::test]
    async fn exec_timeout_reports_timed_out() {
        let client = connect_to_fake().await;
        let result = client
            .exec_collect(ExecRequest::new(Shell::Cmd, "timeout"), CAP)
            .await
            .unwrap();
        assert_eq!(result.outcome, JobOutcome::TimedOut);
        assert_eq!(result.exit_code, None);
    }

    #[tokio::test]
    async fn exec_self_cancel_reports_cancelled() {
        let client = connect_to_fake().await;
        let result = client
            .exec_collect(ExecRequest::new(Shell::Cmd, "cancel"), CAP)
            .await
            .unwrap();
        assert_eq!(result.outcome, JobOutcome::Cancelled);
    }

    #[tokio::test]
    async fn exec_disconnect_reports_unknown() {
        let client = connect_to_fake().await;
        let result = client
            .exec_collect(ExecRequest::new(Shell::Cmd, "disconnect"), CAP)
            .await
            .unwrap();
        assert_eq!(result.outcome, JobOutcome::Unknown);
    }

    #[tokio::test]
    async fn exec_malformed_surfaces_protocol_error() {
        let client = connect_to_fake().await;
        let error = client
            .exec_collect(ExecRequest::new(Shell::Cmd, "malformed"), CAP)
            .await
            .unwrap_err();
        assert!(
            matches!(
                error,
                SessionError::Remote {
                    code: ErrorCode::MalformedMessage,
                    ..
                }
            ),
            "expected a malformed-message error, got {error:?}"
        );
    }

    #[tokio::test]
    async fn events_from_a_foreign_session_are_dropped() {
        let client = connect_to_fake().await;
        let result = client
            .exec_collect(ExecRequest::new(Shell::Cmd, "foreign-output"), CAP)
            .await
            .unwrap();
        // The only output chunk was stamped with a session this connection
        // does not own; it must not be attributed to this session's job
        // (REQ-F-016).
        assert_eq!(result.outcome, JobOutcome::Completed);
        assert!(result.stdout.is_empty());
        assert!(!result.truncated);
    }

    #[tokio::test]
    async fn oversized_local_request_fails_only_that_call() {
        let client = connect_to_fake().await;
        let huge = "x".repeat(limits::MAX_COMMAND_BYTES + 1);
        let error = client
            .exec_collect(ExecRequest::new(Shell::Cmd, huge), CAP)
            .await
            .unwrap_err();
        assert!(matches!(error, SessionError::Protocol(_)), "got {error:?}");

        // The invalid request was rejected before anything reached the
        // wire: the session stays healthy for later calls.
        let (target, _) = client.system_info().await.unwrap();
        assert!(target.simulated);
    }

    #[tokio::test]
    async fn split_frame_delivery_survives_a_concurrent_request() {
        let (agent_io, client_io) = tokio::io::duplex(64 * 1024);
        let agent = tokio::spawn(async move {
            let (read_half, mut write_half) = tokio::io::split(agent_io);
            let mut inbound = Framed::new(read_half);

            // Handshake.
            assert!(matches!(inbound.recv().await, Ok(Message::Hello { .. })));
            let session = SessionId::new();
            let ack = Message::HelloAck {
                protocol: PROTOCOL_V1,
                session,
                target: simulated_identity(),
            };
            write_half.write_all(&raw_frame(&ack)).await.unwrap();
            write_half.flush().await.unwrap();

            // Dribble the first half of a large event frame byte by byte,
            // so the client's reader sits mid-frame...
            let event = Message::Event {
                session,
                event: EventKind::JobOutput {
                    job: JobId(1),
                    stream: OutputStream::Stdout,
                    data: B64Bytes::new(vec![0x41; 8 * 1024]),
                },
            };
            let raw = raw_frame(&event);
            let half = raw.len() / 2;
            for byte in &raw[..half] {
                write_half.write_all(&[*byte]).await.unwrap();
                write_half.flush().await.unwrap();
                tokio::task::yield_now().await;
            }

            // ...while it issues a concurrent request. The request arriving
            // here proves the client wrote while its reader was mid-frame.
            let request = inbound.recv().await.unwrap();
            let Message::Request {
                id,
                op: Operation::SystemInfo {},
                ..
            } = request
            else {
                panic!("expected system_info, got {request:?}");
            };

            // Finish the event frame, then answer the request.
            write_half.write_all(&raw[half..]).await.unwrap();
            let response = Message::Response {
                id,
                session,
                result: OpResult::SystemInfo {
                    target: simulated_identity(),
                    reboot_pending: false,
                },
            };
            write_half.write_all(&raw_frame(&response)).await.unwrap();
            write_half.flush().await.unwrap();
        });

        let client = SessionClient::connect(client_io, Vec::new()).await.unwrap();
        let mut events = client.subscribe();

        // The session must survive writing a request while an inbound frame
        // is partially delivered, and both the response and the split-up
        // event must arrive intact.
        let (target, _) = client.system_info().await.unwrap();
        assert!(target.simulated);

        match events.recv().await.unwrap() {
            EventKind::JobOutput { data, .. } => {
                assert_eq!(data.as_bytes(), &vec![0x41; 8 * 1024][..]);
            }
            other => panic!("expected job output, got {other:?}"),
        }
        agent.await.unwrap();
    }

    #[tokio::test]
    async fn concurrent_max_size_stdin_writes_do_not_deadlock() {
        // A small duplex buffer forces genuine back-pressure. The fake
        // agent is half-duplex (it writes each ack and echo before reading
        // again), so this only completes because the client keeps reading
        // while its own writer is blocked — the dedicated reader/writer
        // task split.
        let (agent_io, client_io) = tokio::io::duplex(16 * 1024);
        tokio::spawn(fake_agent(agent_io, PROTOCOL_V1.major));
        let client = SessionClient::connect(client_io, Vec::new()).await.unwrap();

        let job_a = start_hang(&client).await;
        let job_b = start_hang(&client).await;

        let chunk = vec![0x42u8; limits::MAX_CHUNK_BYTES];
        let (a, b) = tokio::time::timeout(Duration::from_secs(60), async {
            tokio::join!(
                client.write_stdin(job_a, chunk.clone()),
                client.write_stdin(job_b, chunk.clone()),
            )
        })
        .await
        .expect("concurrent max-size stdin writes must not deadlock");
        a.unwrap();
        b.unwrap();
    }

    #[tokio::test]
    async fn cancel_primitive_drives_job_to_cancelled() {
        let client = connect_to_fake().await;
        let mut events = client.subscribe();
        let job = start_hang(&client).await;

        client.cancel(job).await.unwrap();

        loop {
            let event = events.recv().await.unwrap();
            if let EventKind::JobExited {
                job: j, outcome, ..
            } = event
                && j == job
            {
                assert_eq!(outcome, JobOutcome::Cancelled);
                break;
            }
        }
    }

    #[tokio::test]
    async fn write_stdin_is_echoed_as_output() {
        let client = connect_to_fake().await;
        let mut events = client.subscribe();
        let job = start_hang(&client).await;

        client.write_stdin(job, b"ping".to_vec()).await.unwrap();

        let echoed = loop {
            let event = events.recv().await.unwrap();
            if let EventKind::JobOutput {
                job: j,
                stream: OutputStream::Stdout,
                data,
            } = event
                && j == job
            {
                break data;
            }
        };
        assert_eq!(echoed.as_bytes(), b"ping");
    }

    #[tokio::test]
    async fn list_jobs_returns_summaries() {
        let client = connect_to_fake().await;
        let jobs = client.list_jobs().await.unwrap();
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].command_preview, "hang");
        assert!(jobs[0].running);
    }

    #[tokio::test]
    async fn request_after_disconnect_is_disconnected() {
        let client = connect_to_fake().await;
        // "disconnect" drops the agent connection after ExecStarted.
        let _ = client
            .exec_collect(ExecRequest::new(Shell::Cmd, "disconnect"), CAP)
            .await
            .unwrap();
        let error = client.system_info().await.unwrap_err();
        assert!(matches!(error, SessionError::Disconnected), "got {error:?}");
    }

    #[tokio::test]
    async fn incompatible_major_version_is_rejected() {
        let (agent_io, client_io) = tokio::io::duplex(64 * 1024);
        tokio::spawn(fake_agent(agent_io, PROTOCOL_V1.major + 1));
        let error = SessionClient::connect(client_io, Vec::new())
            .await
            .unwrap_err();
        assert!(
            matches!(error, SessionError::IncompatibleVersion { .. }),
            "got {error:?}"
        );
    }

    #[tokio::test]
    async fn handshake_error_is_surfaced() {
        let (agent_io, client_io) = tokio::io::duplex(64 * 1024);
        tokio::spawn(async move {
            // Consume Hello, then answer with an Error instead of HelloAck.
            let mut framed = Framed::new(agent_io);
            let _ = framed.recv().await;
            let error = Message::Error {
                id: None,
                session: None,
                code: ErrorCode::IncompatibleVersion,
                message: "no".to_string(),
                stage: ErrorStage::Negotiation,
            };
            let _ = framed.send(&error).await;
        });
        let error = SessionClient::connect(client_io, Vec::new())
            .await
            .unwrap_err();
        assert!(
            matches!(error, SessionError::Remote { .. }),
            "got {error:?}"
        );
    }
}
