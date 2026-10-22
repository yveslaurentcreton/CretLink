//! [`IpcTargetSession`]: a [`TargetSession`] that forwards to the console over
//! same-user local IPC (ADR-0004, PART C).
//!
//! The MCP bridge binary backs its [`crate::McpBridge`] with this type. It holds
//! **no** session authority: every call opens a short-lived connection to the
//! console's IPC endpoint, learns the console's contract version with a `Hello`
//! on that connection, forwards one request (wrapped with the stated purpose
//! when the console understands it, ST-M009-007), and maps the reply. The console
//! is the sole routing authority and answers with the routed session identity
//! and target, or a fail-closed [`crate::TargetError::NoConsentedSession`] when
//! no consented active session exists (REQ-S-009). A lost IPC connection is
//! surfaced as [`crate::TargetError::Disconnected`] — an unknown outcome, never
//! a fabricated success (REQ-N-008).
//!
//! Using one connection per call lets the console multiplex concurrent bridge
//! calls against its single active session (ADR-0004) and keeps a long-running
//! `remote_exec` from blocking a concurrent `remote_cancel`.

use std::sync::Mutex;

use async_trait::async_trait;
use cretlink_protocol::{
    B64Bytes, CleanupTrigger, JobId, JobSummary, KeyAction, PointerAction, PointerButton, Purpose,
    RemotePath, SessionId, StagedEntry, TargetIdentity,
};

use crate::ipc::wire::{IpcRequest, IpcResponse, read_message, write_message};
use crate::ipc::{IPC_PROTOCOL, IPC_PURPOSE_MINOR, IpcEndpoint};
use crate::target::{
    CallError, CaptureParams, CaptureReport, CleanupSummary, DirPage, DownloadParams,
    DownloadReport, ExecParams, ExecReport, Routed, StatReport, SystemInfoReport, TargetError,
    TargetSession, UploadParams, UploadReport,
};

/// A [`TargetSession`] backed by the console over local IPC.
#[derive(Debug)]
pub struct IpcTargetSession {
    endpoint: IpcEndpoint,
    /// The last-known routed identity, used **only** to label an error envelope
    /// (a fail-closed or lost-link result that carries no routed identity of its
    /// own). Seeded by a `Hello` at construction and refreshed from every
    /// response that carries one. A *successful* tool result never reads this:
    /// it is labelled from the identity in the same reply that produced it (see
    /// [`Routed`]), so a stale cache can never mis-attribute a success. A stable
    /// placeholder holds until a consented session is observed, so a fail-closed
    /// error still names something deterministic.
    cached: Mutex<Identity>,
}

/// The routed identity the bridge reports, with a stable placeholder for the
/// no-session state.
#[derive(Debug, Clone)]
struct Identity {
    session: SessionId,
    target: TargetIdentity,
}

impl Identity {
    fn placeholder() -> Self {
        Self {
            session: SessionId::new(),
            target: TargetIdentity {
                hostname: "unknown".to_string(),
                os_version: "unknown".to_string(),
                arch: "unknown".to_string(),
                elevated: false,
                simulated: false,
                agent_version: "unknown".to_string(),
            },
        }
    }
}

impl IpcTargetSession {
    /// Connects to `endpoint` and caches the routed identity if a consented
    /// session already exists. Never hard-fails: if the console is down or has
    /// no session, the returned target answers every tool call with an honest
    /// fail-closed error rather than crashing the MCP session.
    pub async fn connect(endpoint: IpcEndpoint) -> Self {
        let session = Self {
            endpoint,
            cached: Mutex::new(Identity::placeholder()),
        };
        // Best-effort handshake to seed the identity; a failure just leaves the
        // placeholder in place.
        if let Ok(IpcResponse::Hello {
            session: Some(id),
            target: Some(target),
            ..
        }) = session
            .roundtrip(IpcRequest::Hello {
                protocol: IPC_PROTOCOL,
            })
            .await
        {
            session.store_identity(id, target);
        }
        session
    }

    /// The endpoint this bridge connects to.
    #[must_use]
    pub fn endpoint(&self) -> &IpcEndpoint {
        &self.endpoint
    }

    /// Opens a fresh connection, sends one request, and reads one response. A
    /// connect or framing failure is a lost IPC link, surfaced as
    /// [`TargetError::Disconnected`] (unknown outcome, never success).
    async fn roundtrip(&self, request: IpcRequest) -> Result<IpcResponse, TargetError> {
        let mut stream = crate::ipc::connect(&self.endpoint)
            .await
            .map_err(|_| TargetError::Disconnected)?;
        write_message(&mut stream, &request)
            .await
            .map_err(|_| TargetError::Disconnected)?;
        read_message(&mut stream)
            .await
            .map_err(|_| TargetError::Disconnected)
    }

    /// Sends one routed tool request with the AI client's stated purpose and,
    /// on a routed reply, refreshes the cached identity so later synchronous
    /// [`Self::session_id`]/[`Self::target`] stay accurate.
    ///
    /// The purpose rides in an [`IpcRequest::WithPurpose`] wrapper only when a
    /// `Hello` on this same connection reports a console contract minor of at
    /// least [`IPC_PURPOSE_MINOR`] (ST-M009-007). An older console would close
    /// the connection on the unknown wrapper, so it receives the bare request
    /// instead: the purpose is dropped, the call still runs attributed to the
    /// AI and audited, and the customer sees it without a stated reason. A
    /// failed handshake is a lost link (unknown outcome), as for any call.
    async fn call(
        &self,
        request: IpcRequest,
        purpose: Purpose,
    ) -> Result<IpcResponse, TargetError> {
        let mut stream = crate::ipc::connect(&self.endpoint)
            .await
            .map_err(|_| TargetError::Disconnected)?;
        write_message(
            &mut stream,
            &IpcRequest::Hello {
                protocol: IPC_PROTOCOL,
            },
        )
        .await
        .map_err(|_| TargetError::Disconnected)?;
        let hello: IpcResponse = read_message(&mut stream)
            .await
            .map_err(|_| TargetError::Disconnected)?;
        let request = if console_accepts_purpose(&hello) {
            IpcRequest::WithPurpose {
                purpose,
                call: Box::new(request),
            }
        } else {
            request
        };
        write_message(&mut stream, &request)
            .await
            .map_err(|_| TargetError::Disconnected)?;
        let response: IpcResponse = read_message(&mut stream)
            .await
            .map_err(|_| TargetError::Disconnected)?;
        if let Some((session, target)) = routed_identity(&response) {
            self.store_identity(session, target.clone());
        }
        Ok(response)
    }

    fn store_identity(&self, session: SessionId, target: TargetIdentity) {
        if let Ok(mut cached) = self.cached.lock() {
            *cached = Identity { session, target };
        }
    }

    fn snapshot(&self) -> Identity {
        self.cached
            .lock()
            .map(|guard| guard.clone())
            .unwrap_or_else(|poison| poison.into_inner().clone())
    }

    /// Sends a desktop input request whose reply is a bare
    /// [`IpcResponse::DesktopAck`] and maps it onto `Routed<()>`. Shared by the
    /// pointer, scroll, key, type, hotkey, wait, and clipboard-set methods, which
    /// carry no payload and so share one ack shape (ST-M005-004).
    async fn desktop_ack(
        &self,
        request: IpcRequest,
        purpose: Purpose,
    ) -> Result<Routed<()>, CallError> {
        match self.call(request, purpose).await? {
            IpcResponse::DesktopAck {
                session,
                target,
                call_id,
            } => Ok(Routed {
                session,
                target,
                call_id,
                value: (),
            }),
            IpcResponse::Error { error, call_id } => {
                Err(CallError::new(error.into_target_error(), Some(call_id)))
            }
            _ => Err(TargetError::Unexpected.into()),
        }
    }
}

/// Whether a `Hello` answer comes from a console that understands
/// [`IpcRequest::WithPurpose`]: a compatible major at minor
/// [`IPC_PURPOSE_MINOR`] or later (ST-M009-007).
fn console_accepts_purpose(hello: &IpcResponse) -> bool {
    match hello {
        IpcResponse::Hello { protocol, .. } => {
            protocol.is_compatible_with(IPC_PROTOCOL) && protocol.minor >= IPC_PURPOSE_MINOR
        }
        _ => false,
    }
}

/// Extracts the routed `(session, target)` from any response that carries one.
fn routed_identity(response: &IpcResponse) -> Option<(SessionId, &TargetIdentity)> {
    match response {
        IpcResponse::Hello {
            session: Some(session),
            target: Some(target),
            ..
        } => Some((*session, target)),
        IpcResponse::SystemInfo {
            session, target, ..
        }
        | IpcResponse::Exec {
            session, target, ..
        }
        | IpcResponse::WriteStdin {
            session, target, ..
        }
        | IpcResponse::Cancel {
            session, target, ..
        }
        | IpcResponse::ListJobs {
            session, target, ..
        }
        | IpcResponse::FsStat {
            session, target, ..
        }
        | IpcResponse::FsList {
            session, target, ..
        }
        | IpcResponse::MakeDir {
            session, target, ..
        }
        | IpcResponse::Move {
            session, target, ..
        }
        | IpcResponse::Delete {
            session, target, ..
        }
        | IpcResponse::RemoveDir {
            session, target, ..
        }
        | IpcResponse::Upload {
            session, target, ..
        }
        | IpcResponse::Download {
            session, target, ..
        }
        | IpcResponse::ListStaged {
            session, target, ..
        }
        | IpcResponse::Cleanup {
            session, target, ..
        }
        | IpcResponse::DesktopFrame {
            session, target, ..
        }
        | IpcResponse::DesktopAck {
            session, target, ..
        }
        | IpcResponse::ClipboardContents {
            session, target, ..
        }
        // The bridge never sends terminal requests, but these responses carry
        // the routed identity like any other, so surface it consistently.
        | IpcResponse::TermOpened {
            session, target, ..
        }
        | IpcResponse::TermAttached {
            session, target, ..
        } => Some((*session, target)),
        IpcResponse::Hello { .. } | IpcResponse::Error { .. } => None,
    }
}

#[async_trait]
impl TargetSession for IpcTargetSession {
    fn session_id(&self) -> SessionId {
        self.snapshot().session
    }

    fn target(&self) -> TargetIdentity {
        self.snapshot().target
    }

    async fn system_info(&self, purpose: Purpose) -> Result<Routed<SystemInfoReport>, CallError> {
        match self.call(IpcRequest::SystemInfo {}, purpose).await? {
            IpcResponse::SystemInfo {
                session,
                target,
                call_id,
                report,
            } => Ok(Routed {
                session,
                target,
                call_id,
                value: report,
            }),
            IpcResponse::Error { error, call_id } => {
                Err(CallError::new(error.into_target_error(), Some(call_id)))
            }
            _ => Err(TargetError::Unexpected.into()),
        }
    }

    async fn exec(
        &self,
        purpose: Purpose,
        params: ExecParams,
        output_cap: usize,
    ) -> Result<Routed<ExecReport>, CallError> {
        let request = IpcRequest::Exec {
            params,
            output_cap: output_cap as u64,
        };
        match self.call(request, purpose).await? {
            IpcResponse::Exec {
                session,
                target,
                call_id,
                report,
            } => Ok(Routed {
                session,
                target,
                call_id,
                value: report,
            }),
            IpcResponse::Error { error, call_id } => {
                Err(CallError::new(error.into_target_error(), Some(call_id)))
            }
            _ => Err(TargetError::Unexpected.into()),
        }
    }

    async fn write_stdin(
        &self,
        purpose: Purpose,
        job: JobId,
        data: Vec<u8>,
        sensitive: bool,
    ) -> Result<Routed<()>, CallError> {
        let request = IpcRequest::WriteStdin {
            job,
            data: B64Bytes::new(data),
            sensitive,
        };
        match self.call(request, purpose).await? {
            IpcResponse::WriteStdin {
                session,
                target,
                call_id,
                ..
            } => Ok(Routed {
                session,
                target,
                call_id,
                value: (),
            }),
            IpcResponse::Error { error, call_id } => {
                Err(CallError::new(error.into_target_error(), Some(call_id)))
            }
            _ => Err(TargetError::Unexpected.into()),
        }
    }

    async fn cancel(&self, purpose: Purpose, job: JobId) -> Result<Routed<()>, CallError> {
        match self.call(IpcRequest::Cancel { job }, purpose).await? {
            IpcResponse::Cancel {
                session,
                target,
                call_id,
            } => Ok(Routed {
                session,
                target,
                call_id,
                value: (),
            }),
            IpcResponse::Error { error, call_id } => {
                Err(CallError::new(error.into_target_error(), Some(call_id)))
            }
            _ => Err(TargetError::Unexpected.into()),
        }
    }

    async fn list_jobs(&self, purpose: Purpose) -> Result<Routed<Vec<JobSummary>>, CallError> {
        match self.call(IpcRequest::ListJobs {}, purpose).await? {
            IpcResponse::ListJobs {
                session,
                target,
                call_id,
                jobs,
            } => Ok(Routed {
                session,
                target,
                call_id,
                value: jobs,
            }),
            IpcResponse::Error { error, call_id } => {
                Err(CallError::new(error.into_target_error(), Some(call_id)))
            }
            _ => Err(TargetError::Unexpected.into()),
        }
    }

    async fn fs_stat(
        &self,
        purpose: Purpose,
        path: RemotePath,
    ) -> Result<Routed<StatReport>, CallError> {
        match self.call(IpcRequest::FsStat { path }, purpose).await? {
            IpcResponse::FsStat {
                session,
                target,
                call_id,
                report,
            } => Ok(Routed {
                session,
                target,
                call_id,
                value: report,
            }),
            IpcResponse::Error { error, call_id } => {
                Err(CallError::new(error.into_target_error(), Some(call_id)))
            }
            _ => Err(TargetError::Unexpected.into()),
        }
    }

    async fn fs_list(
        &self,
        purpose: Purpose,
        path: RemotePath,
        continue_after: Option<String>,
    ) -> Result<Routed<DirPage>, CallError> {
        let request = IpcRequest::FsList {
            path,
            continue_after,
        };
        match self.call(request, purpose).await? {
            IpcResponse::FsList {
                session,
                target,
                call_id,
                page,
            } => Ok(Routed {
                session,
                target,
                call_id,
                value: page,
            }),
            IpcResponse::Error { error, call_id } => {
                Err(CallError::new(error.into_target_error(), Some(call_id)))
            }
            _ => Err(TargetError::Unexpected.into()),
        }
    }

    async fn make_dir(&self, purpose: Purpose, path: RemotePath) -> Result<Routed<()>, CallError> {
        match self.call(IpcRequest::MakeDir { path }, purpose).await? {
            IpcResponse::MakeDir {
                session,
                target,
                call_id,
            } => Ok(Routed {
                session,
                target,
                call_id,
                value: (),
            }),
            IpcResponse::Error { error, call_id } => {
                Err(CallError::new(error.into_target_error(), Some(call_id)))
            }
            _ => Err(TargetError::Unexpected.into()),
        }
    }

    async fn fs_move(
        &self,
        purpose: Purpose,
        from: RemotePath,
        to: RemotePath,
        overwrite: bool,
    ) -> Result<Routed<()>, CallError> {
        let request = IpcRequest::Move {
            from,
            to,
            overwrite,
        };
        match self.call(request, purpose).await? {
            IpcResponse::Move {
                session,
                target,
                call_id,
            } => Ok(Routed {
                session,
                target,
                call_id,
                value: (),
            }),
            IpcResponse::Error { error, call_id } => {
                Err(CallError::new(error.into_target_error(), Some(call_id)))
            }
            _ => Err(TargetError::Unexpected.into()),
        }
    }

    async fn fs_delete(&self, purpose: Purpose, path: RemotePath) -> Result<Routed<()>, CallError> {
        match self.call(IpcRequest::Delete { path }, purpose).await? {
            IpcResponse::Delete {
                session,
                target,
                call_id,
            } => Ok(Routed {
                session,
                target,
                call_id,
                value: (),
            }),
            IpcResponse::Error { error, call_id } => {
                Err(CallError::new(error.into_target_error(), Some(call_id)))
            }
            _ => Err(TargetError::Unexpected.into()),
        }
    }

    async fn remove_dir(
        &self,
        purpose: Purpose,
        path: RemotePath,
    ) -> Result<Routed<()>, CallError> {
        match self.call(IpcRequest::RemoveDir { path }, purpose).await? {
            IpcResponse::RemoveDir {
                session,
                target,
                call_id,
            } => Ok(Routed {
                session,
                target,
                call_id,
                value: (),
            }),
            IpcResponse::Error { error, call_id } => {
                Err(CallError::new(error.into_target_error(), Some(call_id)))
            }
            _ => Err(TargetError::Unexpected.into()),
        }
    }

    async fn upload(
        &self,
        purpose: Purpose,
        params: UploadParams,
    ) -> Result<Routed<UploadReport>, CallError> {
        match self.call(IpcRequest::Upload { params }, purpose).await? {
            IpcResponse::Upload {
                session,
                target,
                call_id,
                report,
            } => Ok(Routed {
                session,
                target,
                call_id,
                value: report,
            }),
            IpcResponse::Error { error, call_id } => {
                Err(CallError::new(error.into_target_error(), Some(call_id)))
            }
            _ => Err(TargetError::Unexpected.into()),
        }
    }

    async fn download(
        &self,
        purpose: Purpose,
        params: DownloadParams,
    ) -> Result<Routed<DownloadReport>, CallError> {
        match self.call(IpcRequest::Download { params }, purpose).await? {
            IpcResponse::Download {
                session,
                target,
                call_id,
                report,
            } => Ok(Routed {
                session,
                target,
                call_id,
                value: report,
            }),
            IpcResponse::Error { error, call_id } => {
                Err(CallError::new(error.into_target_error(), Some(call_id)))
            }
            _ => Err(TargetError::Unexpected.into()),
        }
    }

    async fn list_staged(&self, purpose: Purpose) -> Result<Routed<Vec<StagedEntry>>, CallError> {
        match self.call(IpcRequest::ListStaged {}, purpose).await? {
            IpcResponse::ListStaged {
                session,
                target,
                call_id,
                entries,
            } => Ok(Routed {
                session,
                target,
                call_id,
                value: entries,
            }),
            IpcResponse::Error { error, call_id } => {
                Err(CallError::new(error.into_target_error(), Some(call_id)))
            }
            _ => Err(TargetError::Unexpected.into()),
        }
    }

    async fn cleanup(
        &self,
        purpose: Purpose,
        trigger: CleanupTrigger,
    ) -> Result<Routed<CleanupSummary>, CallError> {
        match self.call(IpcRequest::Cleanup { trigger }, purpose).await? {
            IpcResponse::Cleanup {
                session,
                target,
                call_id,
                report,
            } => Ok(Routed {
                session,
                target,
                call_id,
                value: report,
            }),
            IpcResponse::Error { error, call_id } => {
                Err(CallError::new(error.into_target_error(), Some(call_id)))
            }
            _ => Err(TargetError::Unexpected.into()),
        }
    }

    async fn capture_desktop(
        &self,
        purpose: Purpose,
        params: CaptureParams,
    ) -> Result<Routed<CaptureReport>, CallError> {
        match self
            .call(IpcRequest::CaptureDesktop { params }, purpose)
            .await?
        {
            IpcResponse::DesktopFrame {
                session,
                target,
                call_id,
                report,
            } => Ok(Routed {
                session,
                target,
                call_id,
                value: report,
            }),
            IpcResponse::Error { error, call_id } => {
                Err(CallError::new(error.into_target_error(), Some(call_id)))
            }
            _ => Err(TargetError::Unexpected.into()),
        }
    }

    async fn desktop_pointer(
        &self,
        purpose: Purpose,
        action: PointerAction,
        button: PointerButton,
        x: i32,
        y: i32,
    ) -> Result<Routed<()>, CallError> {
        self.desktop_ack(
            IpcRequest::DesktopPointer {
                action,
                button,
                x,
                y,
            },
            purpose,
        )
        .await
    }

    async fn desktop_scroll(
        &self,
        purpose: Purpose,
        x: i32,
        y: i32,
        dx: i32,
        dy: i32,
    ) -> Result<Routed<()>, CallError> {
        self.desktop_ack(IpcRequest::DesktopScroll { x, y, dx, dy }, purpose)
            .await
    }

    async fn desktop_key(
        &self,
        purpose: Purpose,
        action: KeyAction,
        key: String,
    ) -> Result<Routed<()>, CallError> {
        self.desktop_ack(IpcRequest::DesktopKey { action, key }, purpose)
            .await
    }

    async fn desktop_type(&self, purpose: Purpose, text: String) -> Result<Routed<()>, CallError> {
        self.desktop_ack(IpcRequest::DesktopType { text }, purpose)
            .await
    }

    async fn desktop_hotkey(
        &self,
        purpose: Purpose,
        keys: Vec<String>,
    ) -> Result<Routed<()>, CallError> {
        self.desktop_ack(IpcRequest::DesktopHotkey { keys }, purpose)
            .await
    }

    async fn desktop_wait(&self, purpose: Purpose, millis: u64) -> Result<Routed<()>, CallError> {
        self.desktop_ack(IpcRequest::DesktopWait { millis }, purpose)
            .await
    }

    async fn clipboard_get(&self, purpose: Purpose) -> Result<Routed<Option<String>>, CallError> {
        match self.call(IpcRequest::ClipboardGet {}, purpose).await? {
            IpcResponse::ClipboardContents {
                session,
                target,
                call_id,
                text,
            } => Ok(Routed {
                session,
                target,
                call_id,
                value: text,
            }),
            IpcResponse::Error { error, call_id } => {
                Err(CallError::new(error.into_target_error(), Some(call_id)))
            }
            _ => Err(TargetError::Unexpected.into()),
        }
    }

    async fn clipboard_set(&self, purpose: Purpose, text: String) -> Result<Routed<()>, CallError> {
        self.desktop_ack(IpcRequest::ClipboardSet { text }, purpose)
            .await
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, Ordering};

    use cretlink_protocol::ProtocolVersion;
    use tokio::sync::Mutex as AsyncMutex;

    use super::*;
    use crate::ipc::IpcListener;

    fn unique_endpoint() -> IpcEndpoint {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        IpcEndpoint::named(format!("cretlink-purpose-{}-{n}", std::process::id()))
    }

    fn identity() -> TargetIdentity {
        TargetIdentity {
            hostname: "SIMULATED-TARGET".to_string(),
            os_version: "Simulated".to_string(),
            arch: "x86_64".to_string(),
            elevated: false,
            simulated: true,
            agent_version: "test".to_string(),
        }
    }

    /// A fake console that answers `Hello` with `minor` and records every other
    /// request it receives, answering it as a routed `ListJobs`.
    fn fake_console(minor: u16) -> (IpcEndpoint, Arc<AsyncMutex<Vec<IpcRequest>>>) {
        let endpoint = unique_endpoint();
        let mut listener = IpcListener::bind(&endpoint).unwrap();
        let seen = Arc::new(AsyncMutex::new(Vec::new()));
        let recorded = Arc::clone(&seen);
        tokio::spawn(async move {
            let session = SessionId::new();
            while let Ok(mut stream) = listener.accept().await {
                let recorded = Arc::clone(&recorded);
                tokio::spawn(async move {
                    while let Ok(request) = read_message::<_, IpcRequest>(&mut stream).await {
                        let response = match request {
                            IpcRequest::Hello { .. } => IpcResponse::Hello {
                                protocol: ProtocolVersion::new(IPC_PROTOCOL.major, minor),
                                session: Some(session),
                                target: Some(identity()),
                            },
                            other => {
                                recorded.lock().await.push(other);
                                IpcResponse::ListJobs {
                                    session,
                                    target: identity(),
                                    call_id: 1,
                                    jobs: Vec::new(),
                                }
                            }
                        };
                        if write_message(&mut stream, &response).await.is_err() {
                            return;
                        }
                    }
                });
            }
        });
        (endpoint, seen)
    }

    /// A console on IPC minor 9 or later receives the stated purpose in the
    /// `WithPurpose` wrapper (ST-M009-007).
    #[tokio::test]
    async fn purpose_is_sent_to_a_console_that_understands_it() {
        let (endpoint, seen) = fake_console(IPC_PURPOSE_MINOR);
        let target = IpcTargetSession::connect(endpoint).await;
        let purpose = Purpose::new("Checking running jobs").unwrap();
        target.list_jobs(purpose.clone()).await.unwrap();
        assert_eq!(
            seen.lock().await.as_slice(),
            &[IpcRequest::WithPurpose {
                purpose,
                call: Box::new(IpcRequest::ListJobs {}),
            }]
        );
    }

    /// A console on an older IPC minor would close the connection on the
    /// unknown wrapper, so it receives the bare request: the purpose is dropped
    /// and the call still runs (ST-M009-007, REQ-N-012).
    #[tokio::test]
    async fn purpose_is_dropped_for_an_older_console_and_the_call_still_runs() {
        let (endpoint, seen) = fake_console(IPC_PURPOSE_MINOR - 1);
        let target = IpcTargetSession::connect(endpoint).await;
        let routed = target
            .list_jobs(Purpose::new("Checking running jobs").unwrap())
            .await
            .unwrap();
        assert_eq!(routed.call_id, 1);
        assert_eq!(seen.lock().await.as_slice(), &[IpcRequest::ListJobs {}]);
    }
}
