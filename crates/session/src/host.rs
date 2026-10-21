//! The console-side session entry point and handle (ST-M002-005/006).

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::sync::Arc;

use cretlink_protocol::{
    B64Bytes, JobId, JobSummary, KeyAction, Message, OpResult, Operation, Origin, PointerAction,
    PointerButton, ProtocolVersion, RequestId, SessionApprovalCode, SessionId, Shell,
    TargetIdentity,
};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpListener;
use tokio::sync::mpsc::{self, UnboundedSender, unbounded_channel};
use tokio::sync::oneshot;
use tokio::task::JoinHandle as SpawnedTask;

use crate::runtime::{CompareDecision, HostCommand, Outbound};
use crate::{
    CleanupReport, ConsoleHints, DesktopCaptureRequest, DesktopFrame, Dial, DisclosureSummary,
    ExecRequest, NoAudit, RevokeReason, SessionAudit, SessionError, SessionEvent, Shared,
    StartedJob, runtime,
};

/// The console side of a session (the Noise **responder**, ADR-0003).
///
/// [`SessionHost::host`] binds a private-interface address, accepts one inbound
/// connection from the agent, pairs, and drives the consent and active phases.
#[derive(Debug, Clone, Copy, Default)]
pub struct SessionHost;

impl SessionHost {
    /// Binds `bind_addr`, then runs one console-side session in the background.
    ///
    /// `bind_addr` must be a private-interface address from
    /// [`private_interface_addrs`](cretlink_discovery::private_interface_addrs);
    /// any non-private address — the unspecified address (`0.0.0.0`/`::`) or a
    /// globally routable one — is refused so the console never opens an
    /// internet-reachable listener (REQ-S-010). The returned [`HostHandle`]
    /// exposes the bound address immediately; pairing completes in the
    /// background and surfaces as [`SessionEvent::Connected`], after which
    /// [`HostHandle::approval_code`] is available.
    ///
    /// # Errors
    ///
    /// [`SessionError::Io`] if the address is not private or the bind fails.
    pub async fn host(
        bind_addr: SocketAddr,
        hints: ConsoleHints,
        version: ProtocolVersion,
        capabilities: Vec<String>,
    ) -> Result<HostHandle, SessionError> {
        Self::host_with_audit(bind_addr, hints, version, capabilities, Arc::new(NoAudit)).await
    }

    /// Like [`SessionHost::host`], but records lifecycle outcomes to `audit`
    /// (REQ-F-017, REQ-S-017).
    ///
    /// # Errors
    ///
    /// [`SessionError::Io`] if the address is not private or the bind fails.
    pub async fn host_with_audit(
        bind_addr: SocketAddr,
        hints: ConsoleHints,
        version: ProtocolVersion,
        capabilities: Vec<String>,
        audit: Arc<dyn SessionAudit>,
    ) -> Result<HostHandle, SessionError> {
        // Never open an internet-reachable listener: refuse the unspecified
        // address and any globally routable one, binding only private/loopback
        // interfaces (REQ-S-010). The caller sources these from
        // `cretlink_discovery::private_interface_addrs`.
        if !cretlink_discovery::is_private_addr(bind_addr.ip()) {
            return Err(SessionError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "refusing to bind a non-private address (REQ-S-010)",
            )));
        }
        let listener = TcpListener::bind(bind_addr)
            .await
            .map_err(SessionError::Io)?;
        let local_addr = listener.local_addr().map_err(SessionError::Io)?;

        let shared = Arc::new(Shared::new());
        // The event stream is bounded so a remote target that floods command
        // output cannot exhaust technician-side memory (REQ-N-006); the session
        // task applies a drop-and-mark policy when it is under pressure. Control
        // channels stay unbounded so revoke/commands never block.
        let (event_tx, event_rx) = mpsc::channel(runtime::SESSION_EVENT_CHANNEL_CAPACITY);
        let (revoke_tx, revoke_rx) = unbounded_channel();
        // Approval-code comparison decision channel (ST-M002-013): the technician
        // releases the humanly-perceptible compare gate over this channel; it stays
        // unbounded so the confirm/mismatch signal never blocks (REQ-S-006).
        let (confirm_tx, confirm_rx) = unbounded_channel();
        let (command_tx, command_rx) = unbounded_channel();

        let task = tokio::spawn(runtime::host_task(
            listener,
            version,
            capabilities,
            Arc::clone(&shared),
            event_tx,
            revoke_rx,
            confirm_rx,
            command_rx,
            audit,
        ));

        Ok(HostHandle {
            shared,
            events: event_rx,
            revoke_tx,
            confirm_tx,
            command_tx,
            local_addr: Some(local_addr),
            hints,
            task,
        })
    }

    /// Runs one console-side session over an already-connected `stream`
    /// (ST-M007-008, ADR-0016 §5) rather than accepting one from a LAN
    /// listener. The relay dialer supplies the stream after the rendezvous
    /// splice; the console then pairs as the Noise responder and drives consent
    /// and the active phase byte-identically to the LAN path — selecting the
    /// relay is a transport choice, not a session-logic change (ADR-0007).
    ///
    /// [`HostHandle::local_addr`] is `None`: no local listener was bound, so
    /// there is no private-interface address to report (the relay path never
    /// opens an inbound port, REQ-S-010).
    ///
    #[must_use]
    pub fn host_over_stream<S>(
        stream: S,
        hints: ConsoleHints,
        version: ProtocolVersion,
        capabilities: Vec<String>,
        audit: Arc<dyn SessionAudit>,
    ) -> HostHandle
    where
        S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
        let shared = Arc::new(Shared::new());
        // Same channel shapes as `host_with_audit`: a bounded event stream so a
        // flooding target cannot exhaust technician memory (REQ-N-006) and
        // unbounded control channels so revoke/commands never block.
        let (event_tx, event_rx) = mpsc::channel(runtime::SESSION_EVENT_CHANNEL_CAPACITY);
        let (revoke_tx, revoke_rx) = unbounded_channel();
        // Approval-code comparison decision channel (ST-M002-013), as in
        // [`SessionHost::host`]; unbounded so the compare-gate release never blocks.
        let (confirm_tx, confirm_rx) = unbounded_channel();
        let (command_tx, command_rx) = unbounded_channel();

        let task = tokio::spawn(runtime::host_task_over_stream(
            stream,
            version,
            capabilities,
            Arc::clone(&shared),
            event_tx,
            revoke_rx,
            confirm_rx,
            command_rx,
            audit,
        ));

        HostHandle {
            shared,
            events: event_rx,
            revoke_tx,
            confirm_tx,
            command_tx,
            local_addr: None,
            hints,
            task,
        }
    }

    /// Obtains a stream from `dialer` and runs one console-side session over it.
    ///
    /// This is the relay-mode console entry (ST-M007-008): the console dials the
    /// relay outbound, writes its rendezvous preamble, and pairs over the spliced
    /// stream. An unreachable relay, a rejected preamble, or a never-matched peer
    /// fails closed as [`SessionError::Io`] and never yields a fabricated session
    /// (REQ-N-008, REQ-N-015).
    ///
    /// # Errors
    ///
    /// [`SessionError::Io`] if the dialer cannot obtain the stream.
    pub async fn host_dialed<D: Dial>(
        dialer: D,
        hints: ConsoleHints,
        version: ProtocolVersion,
        capabilities: Vec<String>,
        audit: Arc<dyn SessionAudit>,
    ) -> Result<HostHandle, SessionError> {
        let stream = dialer.dial().await?;
        Ok(Self::host_over_stream(
            stream,
            hints,
            version,
            capabilities,
            audit,
        ))
    }
}

/// A handle to a running console-side session.
///
/// Dropping the handle tears the session down fail-closed: the background task
/// is aborted, which drops the [`SecureChannel`](cretlink_transport::SecureChannel)
/// (invalidating the session keys) and lets the peer observe the disconnect.
#[derive(Debug)]
pub struct HostHandle {
    shared: Arc<Shared>,
    events: mpsc::Receiver<SessionEvent>,
    revoke_tx: UnboundedSender<RevokeReason>,
    confirm_tx: UnboundedSender<CompareDecision>,
    command_tx: UnboundedSender<HostCommand>,
    local_addr: Option<SocketAddr>,
    hints: ConsoleHints,
    task: SpawnedTask<()>,
}

impl HostHandle {
    /// The address the console actually bound (with the OS-assigned port when
    /// `bind_addr` used port 0), or `None` for a relay-dialed session
    /// ([`SessionHost::host_over_stream`] / [`SessionHost::host_dialed`]) that
    /// bound no local listener (ST-M007-008).
    #[must_use]
    pub fn local_addr(&self) -> Option<SocketAddr> {
        self.local_addr
    }

    /// The console's local identity hints.
    #[must_use]
    pub fn hints(&self) -> &ConsoleHints {
        &self.hints
    }

    /// The six-digit Session Approval Code, available once
    /// [`SessionEvent::Connected`] has been observed (REQ-F-005).
    #[must_use]
    pub fn approval_code(&self) -> Option<SessionApprovalCode> {
        self.shared.approval_code()
    }

    /// Confirms, on the technician's attended gesture, that the Session Approval
    /// Code shown here matches the one on the customer's screen, releasing the
    /// humanly-perceptible compare gate (REQ-F-005, REQ-S-006, ST-M002-013).
    ///
    /// This is meaningful only while the session waits at
    /// [`SessionEvent::AwaitingApprovalCompare`]; the runtime then sends the
    /// agent [`cretlink_protocol::ControlMessage::ApprovalCompareConfirmed`] and
    /// proceeds to [`SessionEvent::AwaitingConsent`]. It confirms the *codes
    /// match only — it never grants control, because no control exists until the
    /// customer consents (REQ-S-001).
    ///
    /// # Errors
    ///
    /// [`SessionError::Disconnected`] if the session already ended, so a
    /// post-teardown confirmation is refused (the channel is terminal).
    pub fn confirm_code(&self) -> Result<(), SessionError> {
        self.confirm_tx
            .send(CompareDecision::Match)
            .map_err(|_| SessionError::Disconnected)
    }

    /// Reports, on the technician's attended gesture, that the Session Approval
    /// Codes did **not** match, failing the session closed and granting nothing
    /// (REQ-S-006, REQ-S-009, ST-M002-013).
    ///
    /// Meaningful only at [`SessionEvent::AwaitingApprovalCompare`]; the runtime
    /// tears the session down with a [`SessionEvent::Failed`] at the `compare`
    /// stage and revokes the peer, so a mismatched pairing can never advance to
    /// consent.
    ///
    /// # Errors
    ///
    /// [`SessionError::Disconnected`] if the session already ended.
    pub fn report_code_mismatch(&self) -> Result<(), SessionError> {
        self.confirm_tx
            .send(CompareDecision::Mismatch)
            .map_err(|_| SessionError::Disconnected)
    }

    /// The full-control disclosure the customer was shown, as received from the
    /// agent over the encrypted channel (REQ-S-003, REQ-S-016).
    ///
    /// `None` until [`SessionEvent::AwaitingConsent`] has been observed; from
    /// then on it carries the exact text the customer saw, so the console can
    /// present verbatim what was consented to rather than reconstructing it.
    #[must_use]
    pub fn received_disclosure(&self) -> Option<DisclosureSummary> {
        self.shared.disclosure()
    }

    /// Whether attended full control is active right now (REQ-S-001). `false`
    /// before consent and after any teardown.
    #[must_use]
    pub fn control_active(&self) -> bool {
        self.shared.control_active()
    }

    /// The number of heartbeats received from the agent so far.
    #[must_use]
    pub fn heartbeats_received(&self) -> u64 {
        self.shared.heartbeats()
    }

    /// The expiry (Unix milliseconds, UTC) of the reboot-resume ticket the agent
    /// has reflected as armed, or `None` when nothing is armed (ADR-0014,
    /// ST-M006-001). Display-only; carries no key material and never implies
    /// control — attended consent still runs after any reconnect (REQ-S-001).
    #[must_use]
    pub fn reboot_resume_armed_until(&self) -> Option<i64> {
        self.shared.armed_not_after()
    }

    /// The honest cleanup report, available once the session has torn down
    /// (REQ-F-019).
    #[must_use]
    pub fn cleanup_report(&self) -> Option<CleanupReport> {
        self.shared.cleanup()
    }

    /// Awaits the next lifecycle event, or `None` once the session task has
    /// ended and its events are drained.
    pub async fn next_event(&mut self) -> Option<SessionEvent> {
        self.events.recv().await
    }

    /// The negotiated session id, available once control is active. Every exec
    /// request is bound to it (REQ-F-016, REQ-S-012).
    #[must_use]
    pub fn session(&self) -> Option<SessionId> {
        self.shared.session()
    }

    /// Runs a command on the remote target and returns the started job — its
    /// agent-assigned id plus the effective remote working directory it
    /// received (ST-M003-003, REQ-F-009). Output and the terminal outcome
    /// arrive as [`SessionEvent::ExecOutput`] / [`SessionEvent::ExecFinished`]
    /// on the event stream (ST-M003-001/002).
    ///
    /// # Errors
    ///
    /// [`SessionError::NotActive`] before the session reaches full control,
    /// [`SessionError::Remote`] if the agent rejects the command, and
    /// [`SessionError::Disconnected`] if the session ends first.
    pub async fn run_command(&self, request: ExecRequest) -> Result<StartedJob, SessionError> {
        self.run_command_as(request, Origin::Console).await
    }

    /// Like [`Self::run_command`], but stamps an explicit [`Origin`] onto the
    /// wire request so the agent — and the attended customer surface — can
    /// attribute the command to the AI or the technician (ST-M004-006,
    /// REQ-F-017). [`Self::run_command`] is the default technician-console path.
    ///
    /// # Errors
    ///
    /// See [`Self::run_command`].
    pub async fn run_command_as(
        &self,
        request: ExecRequest,
        origin: Origin,
    ) -> Result<StartedJob, SessionError> {
        self.exec_op(None, request.into_operation(), "run_command", origin)
            .await
            .and_then(|result| match result {
                OpResult::ExecStarted { job, working_dir } => Ok(StartedJob { job, working_dir }),
                _ => Err(SessionError::Unexpected {
                    context: "run_command",
                }),
            })
    }

    /// Like [`Self::run_command`], but stamps the request with an explicit
    /// session id. Production callers use [`Self::run_command`]; this exists to
    /// verify that the agent rejects a request bound to any other session and
    /// never runs it (REQ-S-012, REQ-S-014).
    #[doc(hidden)]
    pub async fn run_command_with_session(
        &self,
        session: SessionId,
        request: ExecRequest,
    ) -> Result<StartedJob, SessionError> {
        self.exec_op(
            Some(session),
            request.into_operation(),
            "run_command",
            Origin::Console,
        )
        .await
        .and_then(|result| match result {
            OpResult::ExecStarted { job, working_dir } => Ok(StartedJob { job, working_dir }),
            _ => Err(SessionError::Unexpected {
                context: "run_command",
            }),
        })
    }

    /// Reports the target's identity and pending-reboot state, answered live
    /// by the agent for this request — never a console-side cache
    /// (ST-M003-003, REQ-F-016).
    ///
    /// # Errors
    ///
    /// See [`Self::run_command`].
    pub async fn system_info(&self) -> Result<(TargetIdentity, bool), SessionError> {
        self.system_info_as(Origin::Console).await
    }

    /// Like [`Self::system_info`], but stamps an explicit [`Origin`] onto the
    /// wire request (ST-M004-006).
    ///
    /// # Errors
    ///
    /// See [`Self::run_command`].
    pub async fn system_info_as(
        &self,
        origin: Origin,
    ) -> Result<(TargetIdentity, bool), SessionError> {
        match self
            .exec_op(None, Operation::SystemInfo {}, "system_info", origin)
            .await?
        {
            OpResult::SystemInfo {
                target,
                reboot_pending,
            } => Ok((target, reboot_pending)),
            _ => Err(SessionError::Unexpected {
                context: "system_info",
            }),
        }
    }

    /// Feeds bytes to a running job's standard input.
    ///
    /// # Errors
    ///
    /// See [`Self::run_command`] (plus [`SessionError::Remote`] with
    /// [`cretlink_protocol::ErrorCode::UnknownJob`] for an unknown job).
    pub async fn write_stdin(&self, job: JobId, data: Vec<u8>) -> Result<(), SessionError> {
        self.write_stdin_as(job, data, Origin::Console).await
    }

    /// Like [`Self::write_stdin`], but stamps an explicit [`Origin`] onto the
    /// wire request (ST-M004-006).
    ///
    /// # Errors
    ///
    /// See [`Self::write_stdin`].
    pub async fn write_stdin_as(
        &self,
        job: JobId,
        data: Vec<u8>,
        origin: Origin,
    ) -> Result<(), SessionError> {
        match self
            .exec_op(
                None,
                Operation::WriteStdin {
                    job,
                    data: B64Bytes::new(data),
                },
                "write_stdin",
                origin,
            )
            .await?
        {
            OpResult::Ack {} => Ok(()),
            _ => Err(SessionError::Unexpected {
                context: "write_stdin",
            }),
        }
    }

    /// Requests cancellation of a running job and awaits the agent's
    /// acknowledgement, so a refused cancellation surfaces rather than being
    /// swallowed (stop paths fail closed). The job's terminal outcome still
    /// arrives as a [`SessionEvent::ExecFinished`].
    ///
    /// # Errors
    ///
    /// See [`Self::run_command`].
    pub async fn cancel_job(&self, job: JobId) -> Result<(), SessionError> {
        // A `Cancel` is a distinct top-level message with no origin field; the
        // origin passed here is ignored by the outbound loop.
        match self
            .dispatch(None, Outbound::Cancel(job), Origin::Console)
            .await?
        {
            OpResult::Ack {} => Ok(()),
            _ => Err(SessionError::Unexpected { context: "cancel" }),
        }
    }

    /// Lists the jobs the agent currently tracks.
    ///
    /// # Errors
    ///
    /// See [`Self::run_command`].
    pub async fn list_jobs(&self) -> Result<Vec<JobSummary>, SessionError> {
        self.list_jobs_as(Origin::Console).await
    }

    /// Like [`Self::list_jobs`], but stamps an explicit [`Origin`] onto the wire
    /// request (ST-M004-006).
    ///
    /// # Errors
    ///
    /// See [`Self::run_command`].
    pub async fn list_jobs_as(&self, origin: Origin) -> Result<Vec<JobSummary>, SessionError> {
        match self
            .exec_op(None, Operation::ListJobs {}, "list_jobs", origin)
            .await?
        {
            OpResult::Jobs { jobs } => Ok(jobs),
            _ => Err(SessionError::Unexpected {
                context: "list_jobs",
            }),
        }
    }

    /// Opens an interactive terminal (a pseudo-console job) on the target and
    /// returns its job id (ADR-0013, ST-M003-009). **A terminal is a job**: its
    /// merged VT output and exit arrive as [`SessionEvent::ExecOutput`] /
    /// [`SessionEvent::ExecFinished`], and [`Self::write_stdin`],
    /// [`Self::cancel_job`], and [`Self::list_jobs`] all apply to it. Input is
    /// fed with [`Self::write_stdin`]; the terminal is resized with
    /// [`Self::resize_terminal`] and closed with [`Self::close_terminal`].
    ///
    /// The request is stamped [`Origin::Terminal`] so the agent attributes it to
    /// the technician terminal, distinct from the AI bridge (REQ-S-023).
    ///
    /// # Errors
    ///
    /// [`SessionError::Invalid`] with [`cretlink_protocol::ErrorCode::LimitExceeded`]
    /// if `cols`/`rows` are out of bounds or [`cretlink_protocol::ErrorCode::InvalidPath`]
    /// for an adversarial `working_dir` (rejected locally); [`SessionError::Remote`]
    /// with [`cretlink_protocol::ErrorCode::Unsupported`] if the agent does not
    /// advertise the terminal capability or runs off Windows; plus the errors of
    /// [`Self::run_command`].
    pub async fn open_terminal(
        &self,
        shell: Shell,
        cols: u16,
        rows: u16,
        working_dir: Option<String>,
        env: BTreeMap<String, String>,
    ) -> Result<JobId, SessionError> {
        match self
            .exec_op(
                None,
                Operation::TermOpen {
                    shell,
                    cols,
                    rows,
                    working_dir,
                    env,
                },
                "open_terminal",
                Origin::Terminal,
            )
            .await?
        {
            OpResult::TermOpened { job } => Ok(job),
            _ => Err(SessionError::Unexpected {
                context: "open_terminal",
            }),
        }
    }

    /// Resizes a running terminal's pseudo-console (last-writer-wins). An
    /// out-of-bounds size is rejected locally as [`SessionError::Invalid`] and
    /// never sent (ST-M003-009).
    ///
    /// # Errors
    ///
    /// [`SessionError::Invalid`] with [`cretlink_protocol::ErrorCode::LimitExceeded`]
    /// for an out-of-bounds size; [`SessionError::Remote`] with
    /// [`cretlink_protocol::ErrorCode::UnknownJob`] for an unknown terminal; plus
    /// the errors of [`Self::run_command`].
    pub async fn resize_terminal(
        &self,
        job: JobId,
        cols: u16,
        rows: u16,
    ) -> Result<(), SessionError> {
        match self
            .exec_op(
                None,
                Operation::TermResize { job, cols, rows },
                "resize_terminal",
                Origin::Terminal,
            )
            .await?
        {
            OpResult::Ack {} => Ok(()),
            _ => Err(SessionError::Unexpected {
                context: "resize_terminal",
            }),
        }
    }

    /// Closes a terminal: the same teardown as cancelling its job. The
    /// terminal's honest exit still arrives as a [`SessionEvent::ExecFinished`]
    /// (ST-M003-009).
    ///
    /// # Errors
    ///
    /// [`SessionError::Remote`] with [`cretlink_protocol::ErrorCode::UnknownJob`]
    /// for an unknown terminal; plus the errors of [`Self::run_command`].
    pub async fn close_terminal(&self, job: JobId) -> Result<(), SessionError> {
        match self
            .exec_op(
                None,
                Operation::TermClose { job },
                "close_terminal",
                Origin::Terminal,
            )
            .await?
        {
            OpResult::Ack {} => Ok(()),
            _ => Err(SessionError::Unexpected {
                context: "close_terminal",
            }),
        }
    }

    // --- Interactive desktop (ADR-0015, ST-M005-001/002/003) ---
    //
    // Each method gates on the agent advertising `CAP_DESKTOP_V1`: an agent
    // without it answers `Unsupported` before any side effect, and an agent that
    // advertises it but runs on a platform without a capture-and-input backend
    // answers `Unsupported` from the backend rather than fabricating a frame.
    // Capture is a synchronous request→frame, paced and bounded by the agent's
    // capture governor (REQ-N-006); input actions answer `Ack`. Every operation
    // carries the default [`Origin::Console`] technician attribution; the
    // AI-driven variants stamp [`Origin::Mcp`] when the MCP bridge lands
    // (ST-M004-006).

    /// Captures the target's current interactive desktop and returns the honest
    /// frame — its [`DesktopFrameMeta`](crate::DesktopFrameMeta) state and
    /// geometry, plus a pixel payload only when the desktop is
    /// [`DesktopState::Active`](crate::DesktopState::Active) (ADR-0015,
    /// ST-M005-001). A locked, switched, disconnected, or secure desktop returns
    /// state with **no** image rather than a stale or fabricated frame
    /// (REQ-N-008).
    ///
    /// # Errors
    ///
    /// [`SessionError::Invalid`] if a downscale or quality hint is out of bounds
    /// (validated locally); [`SessionError::Remote`] with
    /// [`cretlink_protocol::ErrorCode::Unsupported`] if the agent does not
    /// advertise the desktop capability or has no backend; plus the errors of
    /// [`Self::run_command`].
    pub async fn capture_desktop(
        &self,
        request: DesktopCaptureRequest,
    ) -> Result<DesktopFrame, SessionError> {
        match self
            .exec_op(
                None,
                request.into_operation(),
                "capture_desktop",
                Origin::Console,
            )
            .await?
        {
            OpResult::DesktopFrame {
                meta,
                image,
                staged,
            } => Ok(DesktopFrame {
                meta,
                image: image.map(B64Bytes::into_inner),
                staged,
            }),
            _ => Err(SessionError::Unexpected {
                context: "capture_desktop",
            }),
        }
    }

    /// Injects a pointer action at an absolute virtual-desktop coordinate
    /// (ADR-0015, ST-M005-002). Refused on a non-active desktop and after any
    /// teardown (REQ-S-004, REQ-S-009).
    ///
    /// # Errors
    ///
    /// [`SessionError::Invalid`] if a coordinate exceeds the protocol bound
    /// (validated locally); [`SessionError::Remote`] with
    /// [`cretlink_protocol::ErrorCode::Unsupported`] when the capability or
    /// backend is absent; plus the errors of [`Self::run_command`].
    pub async fn desktop_pointer(
        &self,
        action: PointerAction,
        button: PointerButton,
        x: i32,
        y: i32,
    ) -> Result<(), SessionError> {
        self.desktop_ack(
            Operation::DesktopPointer {
                action,
                button,
                x,
                y,
            },
            "desktop_pointer",
        )
        .await
    }

    /// Injects a scroll of `dx`/`dy` detents at an absolute virtual-desktop
    /// origin (ADR-0015, ST-M005-002).
    ///
    /// # Errors
    ///
    /// See [`Self::desktop_pointer`] (coordinate and delta bounds are validated
    /// locally).
    pub async fn desktop_scroll(
        &self,
        x: i32,
        y: i32,
        dx: i32,
        dy: i32,
    ) -> Result<(), SessionError> {
        self.desktop_ack(Operation::DesktopScroll { x, y, dx, dy }, "desktop_scroll")
            .await
    }

    /// Injects a single-key action naming a virtual key (ADR-0015,
    /// ST-M005-002). This only *sends* input; no keylogging primitive exists
    /// (REQ-S-013).
    ///
    /// # Errors
    ///
    /// See [`Self::desktop_pointer`] (the key length is validated locally).
    pub async fn desktop_key(
        &self,
        action: KeyAction,
        key: impl Into<String>,
    ) -> Result<(), SessionError> {
        self.desktop_ack(
            Operation::DesktopKey {
                action,
                key: key.into(),
            },
            "desktop_key",
        )
        .await
    }

    /// Types a Unicode string as a sequence of key events (ADR-0015,
    /// ST-M005-002).
    ///
    /// # Errors
    ///
    /// See [`Self::desktop_pointer`] (the text length is validated locally).
    pub async fn desktop_type(&self, text: impl Into<String>) -> Result<(), SessionError> {
        self.desktop_ack(Operation::DesktopType { text: text.into() }, "desktop_type")
            .await
    }

    /// Sends a chord of named virtual keys as a single hotkey press (ADR-0015,
    /// ST-M005-002): each element is held in order, then released in reverse.
    ///
    /// # Errors
    ///
    /// See [`Self::desktop_pointer`] (the chord length and each key length are
    /// validated locally).
    pub async fn desktop_hotkey(&self, keys: Vec<String>) -> Result<(), SessionError> {
        self.desktop_ack(Operation::DesktopHotkey { keys }, "desktop_hotkey")
            .await
    }

    /// Pauses the input pipeline for a bounded interval so a scripted action
    /// sequence can pace itself (ADR-0015, ST-M005-002). The wait is serviced by
    /// the agent's session, not a synthesized input, but stays gated on the
    /// capability like every other desktop operation.
    ///
    /// # Errors
    ///
    /// See [`Self::desktop_pointer`] (the interval is validated locally).
    pub async fn desktop_wait(&self, millis: u64) -> Result<(), SessionError> {
        self.desktop_ack(Operation::DesktopWait { millis }, "desktop_wait")
            .await
    }

    /// Reads the remote clipboard's text, or `None` when it holds no text
    /// (ADR-0015, ST-M005-003). Clipboard exchange is explicit-only and audited.
    ///
    /// # Errors
    ///
    /// [`SessionError::Remote`] with
    /// [`cretlink_protocol::ErrorCode::Unsupported`] when the capability or
    /// backend is absent; plus the errors of [`Self::run_command`].
    pub async fn clipboard_get(&self) -> Result<Option<String>, SessionError> {
        match self
            .exec_op(
                None,
                Operation::ClipboardGet {},
                "clipboard_get",
                Origin::Console,
            )
            .await?
        {
            OpResult::ClipboardContents { text } => Ok(text),
            _ => Err(SessionError::Unexpected {
                context: "clipboard_get",
            }),
        }
    }

    /// Replaces the remote clipboard's text (ADR-0015, ST-M005-003). CretLink
    /// never places session secrets or provider credentials on the remote
    /// clipboard (REQ-F-015, REQ-S-015).
    ///
    /// # Errors
    ///
    /// See [`Self::desktop_pointer`] (the text length is validated locally).
    pub async fn clipboard_set(&self, text: impl Into<String>) -> Result<(), SessionError> {
        self.desktop_ack(
            Operation::ClipboardSet { text: text.into() },
            "clipboard_set",
        )
        .await
    }

    /// Dispatches a desktop `Operation` that answers [`OpResult::Ack`], mapping
    /// any other shape to [`SessionError::Unexpected`]. Shared by every
    /// input-injection and clipboard-set method above.
    async fn desktop_ack(&self, op: Operation, context: &'static str) -> Result<(), SessionError> {
        match self.exec_op(None, op, context, Origin::Console).await? {
            OpResult::Ack {} => Ok(()),
            _ => Err(SessionError::Unexpected { context }),
        }
    }

    /// Performs one file or staging operation on the target and returns the
    /// agent's typed result (ST-M003-004/005).
    ///
    /// This is the console's single primitive for the file surface: every
    /// `Operation::Fs*`, `CreateDir`, `RemoveDir`, `ListStaged`, and
    /// `StagingCleanup` travels through it and comes back as the matching
    /// [`OpResult`] variant, so higher layers — the transfer engine and the MCP
    /// bridge — compose it without duplicating the wire loop. The caller matches
    /// the variant it expects; an unexpected shape is [`SessionError::Unexpected`].
    ///
    /// The bytes carried by `FsWrite`/`FsRead` and any path are remote-facing
    /// data; the operation is validated against the protocol limits locally
    /// before it reaches the live session (REQ-N-012).
    ///
    /// # Errors
    ///
    /// [`SessionError::Invalid`] if the operation violates a protocol limit,
    /// [`SessionError::NotActive`] before full control, [`SessionError::Remote`]
    /// with the agent's [`cretlink_protocol::ErrorCode`] if the agent rejects it,
    /// and [`SessionError::Disconnected`] if the session ends first.
    pub async fn file_op(&self, op: Operation) -> Result<OpResult, SessionError> {
        self.file_op_as(op, Origin::Console).await
    }

    /// Like [`Self::file_op`], but stamps an explicit [`Origin`] onto the wire
    /// request so an AI-driven single file operation is attributed to the AI
    /// (ST-M004-006). The transfer engine's own chunk operations keep the
    /// default console origin (see `Self::submit_op`).
    ///
    /// # Errors
    ///
    /// See [`Self::file_op`].
    pub async fn file_op_as(
        &self,
        op: Operation,
        origin: Origin,
    ) -> Result<OpResult, SessionError> {
        self.exec_op(None, op, "file_op", origin).await
    }

    /// Like [`Self::file_op`], but stamps the request with an explicit session
    /// id. Production callers use [`Self::file_op`]; this exists to verify that
    /// the agent rejects a file operation bound to any other session and never
    /// performs it (REQ-S-012, REQ-S-014).
    #[doc(hidden)]
    pub async fn file_op_with_session(
        &self,
        session: SessionId,
        op: Operation,
    ) -> Result<OpResult, SessionError> {
        self.exec_op(Some(session), op, "file_op", Origin::Console)
            .await
    }

    /// Validates one request operation locally, then dispatches it and awaits the
    /// correlated result.
    ///
    /// Validation happens here so a command that violates a protocol limit fails
    /// only this call (with [`SessionError::Invalid`]) and never reaches the live
    /// session (REQ-N-012).
    async fn exec_op(
        &self,
        session_override: Option<SessionId>,
        op: Operation,
        context: &'static str,
        origin: Origin,
    ) -> Result<OpResult, SessionError> {
        let _ = context;
        // Validate against the protocol limits via a throwaway probe (the id and
        // session do not affect field-limit validation).
        Message::Request {
            id: RequestId(0),
            session: SessionId::new(),
            origin,
            op: op.clone(),
        }
        .validate()
        // Keep the typed category from the protocol validation so the front
        // end can report the true error class (ST-M003-003, REQ-N-015).
        .map_err(|error| SessionError::Invalid {
            code: error.code(),
            message: error.to_string(),
        })?;
        self.dispatch(session_override, Outbound::Op(op), origin)
            .await
    }

    /// Queues one outbound command for the session task and awaits its reply.
    /// Refuses before the session is active so a caller never blocks on a reply
    /// that cannot come. `origin` is stamped onto a wire `Request` (ignored for
    /// a `Cancel`, which is a distinct message).
    async fn dispatch(
        &self,
        session_override: Option<SessionId>,
        kind: Outbound,
        origin: Origin,
    ) -> Result<OpResult, SessionError> {
        let reply = self.submit(session_override, kind, origin)?;
        match reply.await {
            Ok(result) => result,
            Err(_) => Err(SessionError::Disconnected),
        }
    }

    /// Queues one file operation for the current session and returns its reply
    /// channel **without** awaiting it, so the transfer engine can keep several
    /// chunk operations in flight at once and await them in its own order.
    ///
    /// The session task correlates every reply to its request by id, so
    /// out-of-order completion is safe; the transfer engine still awaits FIFO to
    /// bound the in-flight window to
    /// [`MAX_INFLIGHT_FILE_OPS`](cretlink_protocol::limits::MAX_INFLIGHT_FILE_OPS),
    /// giving genuine backpressure without unbounded buffering (REQ-N-006).
    pub(crate) fn submit_op(
        &self,
        op: Operation,
    ) -> Result<oneshot::Receiver<Result<OpResult, SessionError>>, SessionError> {
        // File-transfer chunk operations compose the file surface; they carry
        // the default console origin at the wire level. AI attribution for a
        // transfer is captured console-side (audit `call_id`, the in-flight
        // indicator) rather than on each chunk (ST-M004-006).
        self.submit(None, Outbound::Op(op), Origin::Console)
    }

    /// Queues one outbound command and returns the reply channel without awaiting
    /// it. Refuses before the session is active so a caller never holds a reply
    /// that cannot come (fail closed). `origin` is stamped onto a wire `Request`.
    fn submit(
        &self,
        session_override: Option<SessionId>,
        kind: Outbound,
        origin: Origin,
    ) -> Result<oneshot::Receiver<Result<OpResult, SessionError>>, SessionError> {
        if !self.shared.control_active() {
            return Err(SessionError::NotActive);
        }
        let (reply_tx, reply_rx) = oneshot::channel();
        let command = HostCommand {
            session_override,
            kind,
            origin,
            reply: reply_tx,
        };
        if self.command_tx.send(command).is_err() {
            return Err(SessionError::Disconnected);
        }
        Ok(reply_rx)
    }

    /// Revokes the session with an explicit reason, tearing it down fail-closed
    /// (REQ-S-004, REQ-S-009).
    ///
    /// # Errors
    ///
    /// [`SessionError::Disconnected`] if the session has already ended, so a
    /// post-teardown revoke is refused (the channel is terminal).
    pub fn revoke(&self, reason: RevokeReason) -> Result<(), SessionError> {
        self.revoke_tx
            .send(reason)
            .map_err(|_| SessionError::Disconnected)
    }

    /// Technician-initiated stop (REQ-S-004).
    ///
    /// # Errors
    ///
    /// [`SessionError::Disconnected`] if the session has already ended.
    pub fn stop(&self) -> Result<(), SessionError> {
        self.revoke(RevokeReason::TechnicianRequested)
    }
}

impl Drop for HostHandle {
    fn drop(&mut self) {
        // Fail-closed teardown on drop: aborting the task drops the secure
        // channel and closes the stream, so a forgotten handle cannot leave an
        // authorized session running (REQ-S-009).
        self.task.abort();
    }
}
