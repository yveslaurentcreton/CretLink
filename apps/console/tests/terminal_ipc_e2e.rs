//! Terminal same-user IPC attach/stream/detach end-to-end tests (ST-M003-009).
//!
//! These drive the **real** [`IpcServer`] over the **real** same-user IPC
//! transport (named pipe / Unix socket) with a raw wire client that speaks the
//! terminal message set directly — the second, distinct class of local IPC
//! consumer beside the MCP bridge. A deterministic [`TerminalTestRoute`] plays
//! the routed session's terminal behaviour (simulator-style: attach registers a
//! consumer, input echoes back as the shared merged output, close exits), so the
//! duplex stream machine, fail-closed routing, multi-consumer fan-out, and the
//! slow-consumer disconnect policy all run on every CI platform without a real
//! ConPTY. A `#[cfg(windows)]` test at the bottom exercises the whole stack —
//! the real [`ConsoleSessionService`] terminal registry — over a genuine ConPTY
//! terminal.
//!
//! The `ST-M008-003` block near the end is the adversarial consolidation suite
//! for the operator terminal surface: concurrent open/attach races,
//! reattach-after-revoke and stale-handle reattach failing closed, an honest
//! `Unknown` outcome on a lost link, and owning-session audit attribution.
//!
//! `unwrap`/`expect`/`panic` are allowed here: this is test-only code.
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use cretlink_console::{
    ChannelAudit, ClosedRoute, ConsoleRoute, IpcAudit, IpcServer, NoIpcAudit, TermAttachment,
    TermEvent,
};
use cretlink_mcp::ipc::connect;
use cretlink_mcp::ipc::wire::{read_message, write_message};
use cretlink_mcp::{
    CaptureParams, CaptureReport, CleanupSummary, DirPage, DownloadParams, DownloadReport,
    ExecParams, ExecReport, IpcEndpoint, IpcListener, IpcRequest, IpcResponse, IpcStream,
    StatReport, SystemInfoReport, TargetError, TermFrame, UploadParams, UploadReport,
};
use cretlink_protocol::{
    B64Bytes, CleanupTrigger, JobId, JobOutcome, JobSummary, KeyAction, PointerAction,
    PointerButton, Purpose, RemotePath, SessionId, Shell, StagedEntry, TargetIdentity,
};
use cretlink_shared::Actor;
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;
use tokio::time::timeout;

const STEP: Duration = Duration::from_secs(5);

/// Per-consumer channel capacity in the mock: small on purpose, so a
/// slow-consumer overflow is easy to trigger deterministically.
const MOCK_CONSUMER_CAP: usize = 8;

static ENDPOINT_COUNTER: AtomicU64 = AtomicU64::new(0);

fn unique_endpoint() -> IpcEndpoint {
    let n = ENDPOINT_COUNTER.fetch_add(1, Ordering::SeqCst);
    IpcEndpoint::named(format!("cretlink-term-{}-{}", std::process::id(), n))
}

fn simulated_identity() -> TargetIdentity {
    TargetIdentity {
        hostname: "SIMULATED-TARGET".to_string(),
        os_version: "Simulated".to_string(),
        arch: "x86_64".to_string(),
        elevated: false,
        simulated: true,
        agent_version: "test".to_string(),
        account: None,
    }
}

/// A single attached consumer inside the mock route.
struct MockConsumer {
    id: u64,
    tx: mpsc::Sender<TermEvent>,
    overrun: Arc<AtomicBool>,
}

#[derive(Default)]
struct MockState {
    terminals: BTreeMap<JobId, Vec<MockConsumer>>,
    next_job: u64,
    next_consumer: u64,
}

/// A deterministic, simulator-style terminal route: open registers a terminal,
/// attach adds a consumer, input echoes back to **every** consumer as the shared
/// merged output (the pty echo), and close exits. Plus test helpers to inject
/// output, revoke, and force-drop a slow consumer.
struct TerminalTestRoute {
    session: SessionId,
    target: TargetIdentity,
    active: AtomicBool,
    state: Mutex<MockState>,
}

impl TerminalTestRoute {
    fn active() -> Arc<Self> {
        Arc::new(Self {
            session: SessionId::new(),
            target: simulated_identity(),
            active: AtomicBool::new(true),
            state: Mutex::new(MockState::default()),
        })
    }

    /// Fans one merged-output chunk out to every consumer of `term`; a full
    /// channel marks the consumer overrun (mirrors the real registry).
    fn fanout(&self, term: JobId, bytes: &[u8]) {
        let mut state = self.state.lock().unwrap();
        if let Some(consumers) = state.terminals.get_mut(&term) {
            consumers.retain(|c| match c.tx.try_send(TermEvent::Output(bytes.to_vec())) {
                Ok(()) => true,
                Err(_) => {
                    c.overrun.store(true, Ordering::SeqCst);
                    false
                }
            });
        }
    }

    /// Test helper: revokes the session, tearing every consumer down with a
    /// `Revoked` frame — fail-closed (REQ-S-004, REQ-S-009).
    fn revoke(&self) {
        self.active.store(false, Ordering::SeqCst);
        let mut state = self.state.lock().unwrap();
        for (_, consumers) in std::mem::take(&mut state.terminals) {
            for c in consumers {
                let _ = c.tx.try_send(TermEvent::Revoked);
            }
        }
    }

    /// Test helper: force-drops every consumer of `term` as if it overran its
    /// bounded window, so the writer emits an honest `Dropped` frame.
    fn force_drop(&self, term: JobId) {
        let mut state = self.state.lock().unwrap();
        if let Some(consumers) = state.terminals.get_mut(&term) {
            for c in consumers.drain(..) {
                c.overrun.store(true, Ordering::SeqCst);
                // Dropping `c` (and its sender) closes the consumer's channel.
            }
        }
    }

    /// Test helper: ends `term` with an **honest unknown** outcome, as if the
    /// agent link dropped mid-terminal and the real exit could not be observed
    /// (REQ-N-008). Every consumer receives an `Exited { Unknown, None }` frame
    /// — never a fabricated clean `Completed` close.
    fn exit_unknown(&self, term: JobId) {
        let mut state = self.state.lock().unwrap();
        if let Some(consumers) = state.terminals.remove(&term) {
            for c in consumers {
                let _ = c.tx.try_send(TermEvent::Exited {
                    exit_code: None,
                    outcome: JobOutcome::Unknown,
                });
            }
        }
    }
}

#[async_trait]
impl ConsoleRoute for TerminalTestRoute {
    fn consented(&self) -> Option<(SessionId, TargetIdentity)> {
        if self.active.load(Ordering::SeqCst) {
            Some((self.session, self.target.clone()))
        } else {
            None
        }
    }

    // --- Non-terminal surface: not exercised by these tests. ---
    async fn system_info(
        &self,
        _purpose: Option<Purpose>,
    ) -> Result<SystemInfoReport, TargetError> {
        Err(TargetError::Unexpected)
    }
    async fn exec(
        &self,
        _purpose: Option<Purpose>,
        _p: ExecParams,
        _c: usize,
    ) -> Result<ExecReport, TargetError> {
        Err(TargetError::Unexpected)
    }
    async fn write_stdin(
        &self,
        _purpose: Option<Purpose>,
        _job: JobId,
        _data: Vec<u8>,
    ) -> Result<(), TargetError> {
        Err(TargetError::Unexpected)
    }
    async fn cancel(&self, _purpose: Option<Purpose>, _job: JobId) -> Result<(), TargetError> {
        Err(TargetError::Unexpected)
    }
    async fn list_jobs(&self, _purpose: Option<Purpose>) -> Result<Vec<JobSummary>, TargetError> {
        Err(TargetError::Unexpected)
    }
    async fn fs_stat(
        &self,
        _purpose: Option<Purpose>,
        _path: RemotePath,
    ) -> Result<StatReport, TargetError> {
        Err(TargetError::Unexpected)
    }
    async fn fs_list(
        &self,
        _purpose: Option<Purpose>,
        _path: RemotePath,
        _continue_after: Option<String>,
    ) -> Result<DirPage, TargetError> {
        Err(TargetError::Unexpected)
    }
    async fn make_dir(
        &self,
        _purpose: Option<Purpose>,
        _path: RemotePath,
    ) -> Result<(), TargetError> {
        Err(TargetError::Unexpected)
    }
    async fn fs_move(
        &self,
        _purpose: Option<Purpose>,
        _from: RemotePath,
        _to: RemotePath,
        _overwrite: bool,
    ) -> Result<(), TargetError> {
        Err(TargetError::Unexpected)
    }
    async fn fs_delete(
        &self,
        _purpose: Option<Purpose>,
        _path: RemotePath,
    ) -> Result<(), TargetError> {
        Err(TargetError::Unexpected)
    }
    async fn remove_dir(
        &self,
        _purpose: Option<Purpose>,
        _path: RemotePath,
    ) -> Result<(), TargetError> {
        Err(TargetError::Unexpected)
    }
    async fn upload(
        &self,
        _purpose: Option<Purpose>,
        _params: UploadParams,
    ) -> Result<UploadReport, TargetError> {
        Err(TargetError::Unexpected)
    }
    async fn download(
        &self,
        _purpose: Option<Purpose>,
        _params: DownloadParams,
    ) -> Result<DownloadReport, TargetError> {
        Err(TargetError::Unexpected)
    }
    async fn list_staged(
        &self,
        _purpose: Option<Purpose>,
    ) -> Result<Vec<StagedEntry>, TargetError> {
        Err(TargetError::Unexpected)
    }
    async fn cleanup(
        &self,
        _purpose: Option<Purpose>,
        _trigger: CleanupTrigger,
    ) -> Result<CleanupSummary, TargetError> {
        Err(TargetError::Unexpected)
    }

    // --- Visual desktop control: not under test here, stubbed fail-closed. ---
    async fn capture_desktop(
        &self,
        _purpose: Option<Purpose>,
        _params: CaptureParams,
    ) -> Result<CaptureReport, TargetError> {
        Err(TargetError::Unexpected)
    }
    async fn desktop_pointer(
        &self,
        _purpose: Option<Purpose>,
        _action: PointerAction,
        _button: PointerButton,
        _x: i32,
        _y: i32,
    ) -> Result<(), TargetError> {
        Err(TargetError::Unexpected)
    }
    async fn desktop_scroll(
        &self,
        _purpose: Option<Purpose>,
        _x: i32,
        _y: i32,
        _dx: i32,
        _dy: i32,
    ) -> Result<(), TargetError> {
        Err(TargetError::Unexpected)
    }
    async fn desktop_key(
        &self,
        _purpose: Option<Purpose>,
        _action: KeyAction,
        _key: String,
    ) -> Result<(), TargetError> {
        Err(TargetError::Unexpected)
    }
    async fn desktop_type(
        &self,
        _purpose: Option<Purpose>,
        _text: String,
    ) -> Result<(), TargetError> {
        Err(TargetError::Unexpected)
    }
    async fn desktop_hotkey(
        &self,
        _purpose: Option<Purpose>,
        _keys: Vec<String>,
    ) -> Result<(), TargetError> {
        Err(TargetError::Unexpected)
    }
    async fn desktop_wait(
        &self,
        _purpose: Option<Purpose>,
        _millis: u64,
    ) -> Result<(), TargetError> {
        Err(TargetError::Unexpected)
    }
    async fn clipboard_get(
        &self,
        _purpose: Option<Purpose>,
    ) -> Result<Option<String>, TargetError> {
        Err(TargetError::Unexpected)
    }
    async fn clipboard_set(
        &self,
        _purpose: Option<Purpose>,
        _text: String,
    ) -> Result<(), TargetError> {
        Err(TargetError::Unexpected)
    }

    // --- Terminal surface (the code under test drives these). ---
    async fn open_terminal(
        &self,
        _shell: Shell,
        _cols: u16,
        _rows: u16,
        _working_dir: Option<String>,
        _env: BTreeMap<String, String>,
    ) -> Result<JobId, TargetError> {
        if !self.active.load(Ordering::SeqCst) {
            return Err(TargetError::NoConsentedSession);
        }
        let mut state = self.state.lock().unwrap();
        state.next_job += 1;
        let job = JobId(state.next_job);
        state.terminals.insert(job, Vec::new());
        Ok(job)
    }

    fn term_attach(&self, term: JobId) -> Result<TermAttachment, TargetError> {
        let (session, target) = self.consented().ok_or(TargetError::NoConsentedSession)?;
        let mut state = self.state.lock().unwrap();
        if !state.terminals.contains_key(&term) {
            return Err(TargetError::Remote {
                code: cretlink_protocol::ErrorCode::UnknownJob,
                message: "no such terminal".to_string(),
                stage: cretlink_protocol::ErrorStage::Validation,
            });
        }
        let id = state.next_consumer;
        state.next_consumer += 1;
        let consumers = state.terminals.get_mut(&term).expect("terminal present");
        let (tx, rx) = mpsc::channel(MOCK_CONSUMER_CAP);
        let overrun = Arc::new(AtomicBool::new(false));
        consumers.push(MockConsumer {
            id,
            tx,
            overrun: Arc::clone(&overrun),
        });
        Ok(TermAttachment {
            session,
            target,
            term,
            consumer: id,
            events: rx,
            overrun,
        })
    }

    async fn term_input(&self, term: JobId, data: Vec<u8>) -> Result<(), TargetError> {
        if !self.active.load(Ordering::SeqCst) {
            return Err(TargetError::NoConsentedSession);
        }
        // Echo the input back as shared merged output to every consumer (the pty
        // echo). Raw input is never delivered to a consumer as a frame.
        self.fanout(term, &data);
        Ok(())
    }

    async fn term_resize(&self, _term: JobId, _cols: u16, _rows: u16) -> Result<(), TargetError> {
        if !self.active.load(Ordering::SeqCst) {
            return Err(TargetError::NoConsentedSession);
        }
        Ok(())
    }

    fn term_detach(&self, term: JobId, consumer: u64) {
        let mut state = self.state.lock().unwrap();
        if let Some(consumers) = state.terminals.get_mut(&term) {
            consumers.retain(|c| c.id != consumer);
        }
    }

    async fn term_close(&self, term: JobId) -> Result<(), TargetError> {
        if !self.active.load(Ordering::SeqCst) {
            return Err(TargetError::NoConsentedSession);
        }
        let mut state = self.state.lock().unwrap();
        if let Some(consumers) = state.terminals.remove(&term) {
            for c in consumers {
                let _ = c.tx.try_send(TermEvent::Exited {
                    exit_code: Some(0),
                    outcome: JobOutcome::Completed,
                });
            }
            Ok(())
        } else {
            Err(TargetError::Remote {
                code: cretlink_protocol::ErrorCode::UnknownJob,
                message: "no such terminal".to_string(),
                stage: cretlink_protocol::ErrorStage::Validation,
            })
        }
    }
}

/// Binds the IPC endpoint and serves `route`, returning the endpoint, the server
/// task, and its shutdown sender (kept alive by the caller).
fn spawn_server(
    route: Arc<dyn ConsoleRoute>,
    audit: Arc<dyn IpcAudit>,
) -> (IpcEndpoint, JoinHandle<()>, oneshot::Sender<()>) {
    let endpoint = unique_endpoint();
    let listener = IpcListener::bind(&endpoint).expect("bind IPC endpoint");
    let in_flight = Arc::new(AtomicU64::new(0));
    let (tx, rx) = oneshot::channel();
    let task = tokio::spawn(IpcServer::serve(listener, route, audit, in_flight, rx));
    (endpoint, task, tx)
}

async fn connect_client(endpoint: &IpcEndpoint) -> IpcStream {
    for _ in 0..100 {
        if let Ok(stream) = connect(endpoint).await {
            return stream;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("could not connect to the IPC endpoint");
}

async fn send(stream: &mut IpcStream, request: &IpcRequest) {
    write_message(stream, request).await.expect("send request");
}

async fn recv_response(stream: &mut IpcStream) -> IpcResponse {
    timeout(STEP, read_message::<_, IpcResponse>(stream))
        .await
        .expect("response within budget")
        .expect("decode response")
}

async fn recv_frame(stream: &mut IpcStream) -> TermFrame {
    timeout(STEP, read_message::<_, TermFrame>(stream))
        .await
        .expect("frame within budget")
        .expect("decode frame")
}

/// Opens a terminal over a fresh connection and returns the connection plus the
/// opened job id.
async fn open_terminal(endpoint: &IpcEndpoint) -> (IpcStream, JobId) {
    let mut stream = connect_client(endpoint).await;
    send(
        &mut stream,
        &IpcRequest::TermOpen {
            shell: Shell::PowerShell,
            cols: 80,
            rows: 24,
            working_dir: None,
            env: BTreeMap::new(),
        },
    )
    .await;
    match recv_response(&mut stream).await {
        IpcResponse::TermOpened { job, .. } => (stream, job),
        other => panic!("expected TermOpened, got {other:?}"),
    }
}

/// Attaches `stream` to `term`, returning the attach response.
async fn attach(stream: &mut IpcStream, term: JobId) -> IpcResponse {
    send(stream, &IpcRequest::TermAttach { term }).await;
    recv_response(stream).await
}

/// No consented session: every terminal call fails closed with the explicit
/// no-consented-session error and nothing runs (REQ-S-009, mirrors the bridge).
#[tokio::test]
async fn no_consented_session_fails_terminal_calls_closed() {
    let route: Arc<dyn ConsoleRoute> = Arc::new(ClosedRoute);
    let (endpoint, server, _shutdown) = spawn_server(route, Arc::new(NoIpcAudit));

    let mut stream = connect_client(&endpoint).await;
    send(
        &mut stream,
        &IpcRequest::TermOpen {
            shell: Shell::Cmd,
            cols: 80,
            rows: 24,
            working_dir: None,
            env: BTreeMap::new(),
        },
    )
    .await;
    match recv_response(&mut stream).await {
        IpcResponse::Error { error, .. } => {
            assert_eq!(error, cretlink_mcp::IpcError::NoConsentedSession);
        }
        other => panic!("TermOpen must fail closed, got {other:?}"),
    }

    send(&mut stream, &IpcRequest::TermAttach { term: JobId(1) }).await;
    match recv_response(&mut stream).await {
        IpcResponse::Error { error, .. } => {
            assert_eq!(error, cretlink_mcp::IpcError::NoConsentedSession);
        }
        other => panic!("TermAttach must fail closed, got {other:?}"),
    }

    server.abort();
}

/// Attaching to a terminal that was never opened (a stale/unknown handle) is
/// refused honestly, never panics, and the connection stays usable.
#[tokio::test]
async fn attach_to_unknown_terminal_is_refused() {
    let route = TerminalTestRoute::active();
    let (endpoint, server, _shutdown) = spawn_server(route, Arc::new(NoIpcAudit));

    let mut stream = connect_client(&endpoint).await;
    send(&mut stream, &IpcRequest::TermAttach { term: JobId(999) }).await;
    match recv_response(&mut stream).await {
        IpcResponse::Error { error, .. } => match error {
            cretlink_mcp::IpcError::Target(TargetError::Remote { code, .. }) => {
                assert_eq!(code, cretlink_protocol::ErrorCode::UnknownJob);
            }
            other => panic!("expected an UnknownJob remote error, got {other:?}"),
        },
        other => panic!("expected an error, got {other:?}"),
    }

    server.abort();
}

/// The full happy path over real IPC: open, attach (enter duplex), type input,
/// see it echoed in the merged output, then close and receive the honest exit
/// frame.
#[tokio::test]
async fn open_attach_input_echo_and_close_end_to_end() {
    let route = TerminalTestRoute::active();
    let (endpoint, server, _shutdown) = spawn_server(route, Arc::new(NoIpcAudit));

    let (mut stream, term) = open_terminal(&endpoint).await;
    match attach(&mut stream, term).await {
        IpcResponse::TermAttached { term: t, .. } => assert_eq!(t, term),
        other => panic!("expected TermAttached, got {other:?}"),
    }

    // Type input; the pty echoes it back as the shared merged output.
    send(
        &mut stream,
        &IpcRequest::TermInput {
            term,
            data: B64Bytes::new(b"echo hi\r\n".to_vec()),
        },
    )
    .await;
    match recv_frame(&mut stream).await {
        TermFrame::Output { data, term: t, .. } => {
            assert_eq!(t, term);
            assert_eq!(data.into_inner(), b"echo hi\r\n");
        }
        other => panic!("expected an Output echo frame, got {other:?}"),
    }

    // Close the terminal; every consumer gets an honest Exited frame.
    send(&mut stream, &IpcRequest::TermClose { term }).await;
    match recv_frame(&mut stream).await {
        TermFrame::Exited {
            outcome, term: t, ..
        } => {
            assert_eq!(t, term);
            assert_eq!(outcome, JobOutcome::Completed);
        }
        other => panic!("expected an Exited frame, got {other:?}"),
    }

    server.abort();
}

/// Detaching removes only this consumer; the terminal outlives it, so a fresh
/// connection can re-attach and keep interacting.
#[tokio::test]
async fn detach_leaves_terminal_alive_for_reattach() {
    let route = TerminalTestRoute::active();
    let (endpoint, server, _shutdown) = spawn_server(route, Arc::new(NoIpcAudit));

    let (mut first, term) = open_terminal(&endpoint).await;
    assert!(matches!(
        attach(&mut first, term).await,
        IpcResponse::TermAttached { .. }
    ));
    // Detach ends this connection's stream.
    send(&mut first, &IpcRequest::TermDetach { term }).await;
    drop(first);

    // The terminal still exists: a new consumer can attach and interact.
    let mut second = connect_client(&endpoint).await;
    assert!(matches!(
        attach(&mut second, term).await,
        IpcResponse::TermAttached { .. }
    ));
    send(
        &mut second,
        &IpcRequest::TermInput {
            term,
            data: B64Bytes::new(b"still-here".to_vec()),
        },
    )
    .await;
    match recv_frame(&mut second).await {
        TermFrame::Output { data, .. } => assert_eq!(data.into_inner(), b"still-here"),
        other => panic!("expected the re-attached consumer to see output, got {other:?}"),
    }

    server.abort();
}

/// Two consumers attached to one terminal both see the full shared merged output
/// (including the echo of the other consumer's input), and neither ever receives
/// the other's raw input as a frame — the tmux model (REQ-N-006, ST-M003-009).
#[tokio::test]
async fn multi_consumer_share_output_without_input_leak() {
    let route = TerminalTestRoute::active();
    let (endpoint, server, _shutdown) = spawn_server(route, Arc::new(NoIpcAudit));

    let (mut a, term) = open_terminal(&endpoint).await;
    assert!(matches!(
        attach(&mut a, term).await,
        IpcResponse::TermAttached { .. }
    ));
    let mut b = connect_client(&endpoint).await;
    assert!(matches!(
        attach(&mut b, term).await,
        IpcResponse::TermAttached { .. }
    ));

    // A types: both A and B see the echoed output; the raw input frame is never
    // forwarded to B (TermFrame carries no input variant at all).
    send(
        &mut a,
        &IpcRequest::TermInput {
            term,
            data: B64Bytes::new(b"from-a".to_vec()),
        },
    )
    .await;
    for consumer in [&mut a, &mut b] {
        match recv_frame(consumer).await {
            TermFrame::Output { data, .. } => assert_eq!(data.into_inner(), b"from-a"),
            other => panic!("both consumers must see A's echo, got {other:?}"),
        }
    }

    // B types: both again see the shared echo.
    send(
        &mut b,
        &IpcRequest::TermInput {
            term,
            data: B64Bytes::new(b"from-b".to_vec()),
        },
    )
    .await;
    for consumer in [&mut a, &mut b] {
        match recv_frame(consumer).await {
            TermFrame::Output { data, .. } => assert_eq!(data.into_inner(), b"from-b"),
            other => panic!("both consumers must see B's echo, got {other:?}"),
        }
    }

    server.abort();
}

/// A revoke while a consumer is attached actively tears its stream down with an
/// honest `Revoked` frame — no silent hang — and every later call fails closed
/// (REQ-S-004, REQ-S-009).
#[tokio::test]
async fn revoke_tears_down_attached_stream_with_honest_frame() {
    let route = TerminalTestRoute::active();
    let (endpoint, server, _shutdown) = spawn_server(route.clone(), Arc::new(NoIpcAudit));

    let (mut stream, term) = open_terminal(&endpoint).await;
    assert!(matches!(
        attach(&mut stream, term).await,
        IpcResponse::TermAttached { .. }
    ));

    // Revoke from the routed session side.
    route.revoke();

    match recv_frame(&mut stream).await {
        TermFrame::Revoked { term: t, .. } => assert_eq!(t, term),
        other => panic!("a revoke must push an honest Revoked frame, got {other:?}"),
    }

    // A new connection now fails closed.
    let mut after = connect_client(&endpoint).await;
    send(
        &mut after,
        &IpcRequest::TermOpen {
            shell: Shell::Cmd,
            cols: 80,
            rows: 24,
            working_dir: None,
            env: BTreeMap::new(),
        },
    )
    .await;
    match recv_response(&mut after).await {
        IpcResponse::Error { error, .. } => {
            assert_eq!(error, cretlink_mcp::IpcError::NoConsentedSession);
        }
        other => panic!("a revoked session must fail closed, got {other:?}"),
    }

    server.abort();
}

/// A consumer that overruns its bounded window is disconnected with an honest
/// `Dropped` frame — never a fabricated clean close (REQ-N-006, REQ-N-008).
#[tokio::test]
async fn slow_consumer_is_dropped_with_an_honest_frame() {
    let route = TerminalTestRoute::active();
    let (endpoint, server, _shutdown) = spawn_server(route.clone(), Arc::new(NoIpcAudit));

    let (mut stream, term) = open_terminal(&endpoint).await;
    assert!(matches!(
        attach(&mut stream, term).await,
        IpcResponse::TermAttached { .. }
    ));

    // Simulate this consumer falling too far behind: it is dropped for overrun.
    route.force_drop(term);

    match recv_frame(&mut stream).await {
        TermFrame::Dropped { term: t, .. } => assert_eq!(t, term),
        other => panic!("a slow consumer must be dropped with a Dropped frame, got {other:?}"),
    }

    server.abort();
}

/// Terminal actions are audited as the technician `Terminal` actor, distinct
/// from the MCP bridge's `Mcp` — attach grants no new authority, it is a
/// separate class of local consumer of the same consented session (REQ-S-023).
#[tokio::test]
async fn terminal_actions_are_attributed_to_the_terminal_actor() {
    let route = TerminalTestRoute::active();
    let (audit, audit_rx) = ChannelAudit::bounded(64);
    let (endpoint, server, _shutdown) = spawn_server(route, Arc::new(audit) as Arc<dyn IpcAudit>);

    let (mut stream, term) = open_terminal(&endpoint).await;
    assert!(matches!(
        attach(&mut stream, term).await,
        IpcResponse::TermAttached { .. }
    ));
    // Detach so the terminal_detach record is emitted, then let the server settle.
    send(&mut stream, &IpcRequest::TermDetach { term }).await;
    drop(stream);
    tokio::time::sleep(Duration::from_millis(100)).await;

    let mut saw_open = false;
    let mut saw_attach = false;
    while let Ok(entry) = audit_rx.try_recv() {
        if entry.action == "terminal_open" {
            assert_eq!(entry.actor, Actor::Terminal);
            saw_open = true;
        }
        if entry.action == "terminal_attach" {
            assert_eq!(entry.actor, Actor::Terminal);
            saw_attach = true;
        }
        // No terminal action must ever be attributed to the AI bridge actor.
        if entry.action.starts_with("terminal_") {
            assert_ne!(entry.actor, Actor::Mcp);
        }
    }
    assert!(saw_open, "the terminal open must be audited as Terminal");
    assert!(
        saw_attach,
        "the terminal attach must be audited as Terminal"
    );

    server.abort();
}

// --- ST-M008-003: adversarial validation (races, reattach, honest outcomes) ---
//
// The tests above establish the happy path and the core fail-closed properties.
// The block below is the consolidation suite for ST-M008-003: it drives the
// operator terminal surface under concurrency and stop/revoke, and pins the
// honest-outcome and owning-session-attribution guarantees the threat model
// relies on.

/// Concurrent opens each get a **distinct** terminal handle: two racing callers
/// can never be handed the same terminal id, so one client can never end up
/// driving another's terminal (REQ-S-023).
#[tokio::test]
async fn concurrent_opens_yield_distinct_terminals() {
    let route = TerminalTestRoute::active();
    let (endpoint, server, _shutdown) = spawn_server(route, Arc::new(NoIpcAudit));

    // Race several independent opens, each on its own connection, at once.
    let mut tasks = Vec::new();
    for _ in 0..8 {
        let endpoint = endpoint.clone();
        tasks.push(tokio::spawn(async move {
            let (_stream, job) = open_terminal(&endpoint).await;
            job
        }));
    }
    let mut jobs = Vec::new();
    for task in tasks {
        jobs.push(task.await.expect("an open task joins"));
    }

    let unique: BTreeSet<JobId> = jobs.iter().copied().collect();
    assert_eq!(
        unique.len(),
        jobs.len(),
        "concurrent opens must never collide on a terminal id"
    );

    server.abort();
}

/// Concurrent attaches to the **same** terminal all register as distinct
/// consumers with none silently lost in the race: after the dust settles, a
/// single fan-out reaches every one of them (REQ-S-023, REQ-N-006).
#[tokio::test]
async fn concurrent_attaches_all_register_and_receive_the_shared_stream() {
    let route = TerminalTestRoute::active();
    let (endpoint, server, _shutdown) = spawn_server(route.clone(), Arc::new(NoIpcAudit));

    let (opener, term) = open_terminal(&endpoint).await;

    // Race N attaches to the one terminal on independent connections.
    const N: usize = 6;
    let mut tasks = Vec::new();
    for _ in 0..N {
        let endpoint = endpoint.clone();
        tasks.push(tokio::spawn(async move {
            let mut stream = connect_client(&endpoint).await;
            match attach(&mut stream, term).await {
                IpcResponse::TermAttached { .. } => stream,
                other => panic!("a raced attach must succeed, got {other:?}"),
            }
        }));
    }
    let mut consumers = Vec::new();
    for task in tasks {
        consumers.push(task.await.expect("an attach task joins"));
    }

    // One shared-output chunk must reach every consumer that won the race: none
    // was dropped or lost while the registry was mutated concurrently.
    route.fanout(term, b"race-echo");
    for stream in &mut consumers {
        match recv_frame(stream).await {
            TermFrame::Output { data, term: t, .. } => {
                assert_eq!(t, term);
                assert_eq!(data.into_inner(), b"race-echo");
            }
            other => {
                panic!("every raced consumer must receive the shared stream, got {other:?}")
            }
        }
    }

    drop(opener);
    server.abort();
}

/// A stop or revoke while a terminal is active **prevents reattachment**: a
/// fresh connection may not reattach to the same handle, and fails closed with
/// the explicit no-consented-session error — not a stale-handle leak, not a
/// silent success (REQ-F-018, REQ-S-004, REQ-S-009).
#[tokio::test]
async fn reattach_after_revoke_fails_closed() {
    let route = TerminalTestRoute::active();
    let (endpoint, server, _shutdown) = spawn_server(route.clone(), Arc::new(NoIpcAudit));

    let (mut stream, term) = open_terminal(&endpoint).await;
    assert!(matches!(
        attach(&mut stream, term).await,
        IpcResponse::TermAttached { .. }
    ));

    // Stop/revoke tears the live stream down honestly...
    route.revoke();
    match recv_frame(&mut stream).await {
        TermFrame::Revoked { term: t, .. } => assert_eq!(t, term),
        other => panic!("a revoke must push an honest Revoked frame, got {other:?}"),
    }

    // ...and reattachment to the very same handle is refused fail-closed.
    let mut after = connect_client(&endpoint).await;
    match attach(&mut after, term).await {
        IpcResponse::Error { error, .. } => {
            assert_eq!(error, cretlink_mcp::IpcError::NoConsentedSession);
        }
        other => panic!("reattach after revoke must fail closed, got {other:?}"),
    }

    server.abort();
}

/// A **stale handle** — a terminal that was valid but has since exited — is
/// refused on reattach even while the session stays consented: an honest
/// `UnknownJob`, never a panic, hang, or silent success (REQ-S-009, REQ-N-008).
#[tokio::test]
async fn reattach_to_a_closed_terminal_is_refused() {
    let route = TerminalTestRoute::active();
    let (endpoint, server, _shutdown) = spawn_server(route, Arc::new(NoIpcAudit));

    let (mut stream, term) = open_terminal(&endpoint).await;
    assert!(matches!(
        attach(&mut stream, term).await,
        IpcResponse::TermAttached { .. }
    ));

    // Close the terminal: it exits honestly and leaves the registry.
    send(&mut stream, &IpcRequest::TermClose { term }).await;
    match recv_frame(&mut stream).await {
        TermFrame::Exited {
            outcome, term: t, ..
        } => {
            assert_eq!(t, term);
            assert_eq!(outcome, JobOutcome::Completed);
        }
        other => panic!("close must end the stream with an honest Exited frame, got {other:?}"),
    }
    drop(stream);

    // The session is still consented, but the handle is stale: a fresh attach to
    // the now-exited terminal is refused honestly.
    let mut after = connect_client(&endpoint).await;
    match attach(&mut after, term).await {
        IpcResponse::Error { error, .. } => match error {
            cretlink_mcp::IpcError::Target(TargetError::Remote { code, .. }) => {
                assert_eq!(code, cretlink_protocol::ErrorCode::UnknownJob);
            }
            other => panic!("expected an UnknownJob remote error, got {other:?}"),
        },
        other => panic!("a stale-handle reattach must be refused, got {other:?}"),
    }

    server.abort();
}

/// A lost link mid-terminal surfaces an **honest `Unknown`** exit to the
/// attached consumer — the real outcome could not be observed, so the stream
/// ends with `Unknown` (and no exit code), never a fabricated clean close
/// (REQ-N-008).
#[tokio::test]
async fn a_lost_link_surfaces_an_honest_unknown_exit() {
    let route = TerminalTestRoute::active();
    let (endpoint, server, _shutdown) = spawn_server(route.clone(), Arc::new(NoIpcAudit));

    let (mut stream, term) = open_terminal(&endpoint).await;
    assert!(matches!(
        attach(&mut stream, term).await,
        IpcResponse::TermAttached { .. }
    ));

    // The agent link drops mid-terminal: the real exit can't be determined.
    route.exit_unknown(term);

    match recv_frame(&mut stream).await {
        TermFrame::Exited {
            outcome,
            exit_code,
            term: t,
            ..
        } => {
            assert_eq!(t, term);
            assert_eq!(
                outcome,
                JobOutcome::Unknown,
                "a lost link must surface an honest Unknown, never a fabricated clean close"
            );
            assert_eq!(exit_code, None);
        }
        other => {
            panic!("a lost link must end the stream with an honest Exited frame, got {other:?}")
        }
    }

    server.abort();
}

/// Every terminal audit record carries the **owning session** identity and the
/// technician `Terminal` actor, so terminal activity is always attributable to
/// the one consented session and never confused with AI/MCP work (REQ-S-017,
/// REQ-F-016, REQ-S-023).
#[tokio::test]
async fn terminal_audit_records_carry_the_owning_session() {
    let route = TerminalTestRoute::active();
    let owning = route.session;
    let (audit, audit_rx) = ChannelAudit::bounded(64);
    let (endpoint, server, _shutdown) =
        spawn_server(route.clone(), Arc::new(audit) as Arc<dyn IpcAudit>);

    let (mut stream, term) = open_terminal(&endpoint).await;
    assert!(matches!(
        attach(&mut stream, term).await,
        IpcResponse::TermAttached { .. }
    ));
    // Detach so the terminal_detach record is emitted, then let the server settle.
    send(&mut stream, &IpcRequest::TermDetach { term }).await;
    drop(stream);
    tokio::time::sleep(Duration::from_millis(100)).await;

    let mut saw_open = false;
    let mut saw_attach = false;
    while let Ok(entry) = audit_rx.try_recv() {
        if entry.action.starts_with("terminal_") {
            assert_eq!(
                entry.actor,
                Actor::Terminal,
                "a terminal record must be attributed to the Terminal actor"
            );
            assert_eq!(
                entry.session,
                Some(owning),
                "a terminal audit record must carry the owning session"
            );
        }
        if entry.action == "terminal_open" {
            saw_open = true;
        }
        if entry.action == "terminal_attach" {
            saw_attach = true;
        }
    }
    assert!(
        saw_open,
        "the terminal open must be audited with the owning session"
    );
    assert!(
        saw_attach,
        "the terminal attach must be audited with the owning session"
    );

    server.abort();
}

// --- Real ConPTY over IPC (Windows only) ---
//
// The cross-platform tests above use a deterministic mock route. This test runs
// the whole stack — the real `ConsoleSessionService` terminal registry driven by
// a genuine ConPTY job over a consented loopback session — behind the real IPC
// transport, so a Windows CI runner proves the registry fan-out end to end. It
// is `cfg(windows)` because ConPTY is Windows-only (the Ubuntu lint gate does
// not compile it).
#[cfg(windows)]
mod real_conpty {
    #![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

    use super::*;
    use cretlink_console::ConsoleSessionService;
    use cretlink_protocol::{CAP_TERM_V1, PROTOCOL_V1};
    use cretlink_session::{
        ConsoleHints, HostHandle, JoinHandle as SessionJoinHandle, SessionEvent, SessionHost,
        SessionJoiner,
    };
    use std::net::{Ipv4Addr, SocketAddr};

    fn loopback() -> SocketAddr {
        SocketAddr::from((Ipv4Addr::LOCALHOST, 0))
    }

    fn caps_with_terminal() -> Vec<String> {
        vec![CAP_TERM_V1.to_string()]
    }

    async fn next_event(handle: &mut HostHandle) -> SessionEvent {
        timeout(STEP, handle.next_event())
            .await
            .expect("event within budget")
            .expect("event stream stays open")
    }

    /// A consented, active loopback session whose agent advertises `CAP_TERM_V1`.
    async fn active_session() -> (HostHandle, SessionJoinHandle, SessionId, TargetIdentity) {
        let hints = ConsoleHints {
            label: "This computer (technician)".to_string(),
            display_name: None,
        };
        let mut host = SessionHost::host(loopback(), hints, PROTOCOL_V1, caps_with_terminal())
            .await
            .expect("bind console");
        let addr = host
            .local_addr()
            .expect("a LAN-bound console reports its local listener address");
        let mut joiner = SessionJoiner::join(
            addr,
            simulated_identity(),
            PROTOCOL_V1,
            caps_with_terminal(),
        )
        .await
        .expect("dial agent");

        assert_eq!(next_event(&mut host).await, SessionEvent::Connected);
        assert_eq!(
            next_event(&mut host).await,
            SessionEvent::AwaitingApprovalCompare
        );
        host.confirm_code().expect("confirm the approval code");
        assert_eq!(next_event(&mut host).await, SessionEvent::AwaitingConsent);
        joiner.grant_consent().expect("grant consent");
        let identity = match next_event(&mut host).await {
            SessionEvent::ConsentGranted { identity } => identity,
            other => panic!("expected ConsentGranted, got {other:?}"),
        };
        assert_eq!(next_event(&mut host).await, SessionEvent::Active);
        let session = host.session().expect("session id once active");
        let _ = timeout(STEP, joiner.next_event()).await;
        (host, joiner, session, identity)
    }

    /// Open a real ConPTY terminal over the IPC surface, drive a typed command
    /// through, see its output in the merged VT stream, then close — asserting an
    /// honest, non-fabricated teardown (a terminal is a job, REQ-N-008).
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn real_conpty_terminal_over_ipc() {
        let (host, joiner, session, identity) = active_session().await;
        let (service, _driver) = ConsoleSessionService::start(host, session, identity);

        let route: Arc<dyn ConsoleRoute> = Arc::new(service.clone());
        let (endpoint, server, _shutdown) = spawn_server(route, Arc::new(NoIpcAudit));

        let mut stream = connect_client(&endpoint).await;
        send(
            &mut stream,
            &IpcRequest::TermOpen {
                shell: Shell::Cmd,
                cols: 80,
                rows: 24,
                working_dir: None,
                env: BTreeMap::new(),
            },
        )
        .await;
        let term = match recv_response(&mut stream).await {
            IpcResponse::TermOpened { job, .. } => job,
            other => panic!("expected TermOpened, got {other:?}"),
        };

        match attach(&mut stream, term).await {
            IpcResponse::TermAttached { .. } => {}
            other => panic!("expected TermAttached, got {other:?}"),
        }

        send(
            &mut stream,
            &IpcRequest::TermInput {
                term,
                data: B64Bytes::new(b"echo cretlink-ipc-marker\r\n".to_vec()),
            },
        )
        .await;

        // Read frames until the typed marker appears in the merged VT stream.
        let mut saw_marker = false;
        for _ in 0..500 {
            match recv_frame(&mut stream).await {
                TermFrame::Output { data, .. } => {
                    if String::from_utf8_lossy(data.as_bytes()).contains("cretlink-ipc-marker") {
                        saw_marker = true;
                        break;
                    }
                }
                TermFrame::Exited { .. }
                | TermFrame::Revoked { .. }
                | TermFrame::Dropped { .. } => {
                    break;
                }
            }
        }
        assert!(
            saw_marker,
            "the typed command must appear in the merged VT stream"
        );

        send(&mut stream, &IpcRequest::TermClose { term }).await;
        // The terminal's honest exit arrives; a closed terminal is never a
        // fabricated clean success.
        let mut exited = false;
        for _ in 0..500 {
            match recv_frame(&mut stream).await {
                TermFrame::Exited { outcome, .. } => {
                    assert_ne!(outcome, JobOutcome::Completed);
                    exited = true;
                    break;
                }
                TermFrame::Revoked { .. } | TermFrame::Dropped { .. } => {
                    exited = true;
                    break;
                }
                TermFrame::Output { .. } => {}
            }
        }
        assert!(
            exited,
            "closing the terminal must yield an honest terminal frame"
        );

        service.stop();
        server.abort();
        drop(joiner);
    }
}
