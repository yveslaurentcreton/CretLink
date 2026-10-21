//! [`IpcTargetSession`]: a [`TargetSession`] that forwards to the console over
//! same-user local IPC (ADR-0004, PART C).
//!
//! The MCP bridge binary backs its [`crate::McpBridge`] with this type. It holds
//! **no** session authority: every call opens a short-lived connection to the
//! console's IPC endpoint, forwards one request, and maps the reply. The console
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
    B64Bytes, CleanupTrigger, JobId, JobSummary, KeyAction, PointerAction, PointerButton,
    RemotePath, SessionId, StagedEntry, TargetIdentity,
};

use crate::ipc::wire::{IpcRequest, IpcResponse, read_message, write_message};
use crate::ipc::{IPC_PROTOCOL, IpcEndpoint};
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

    /// Sends a request and, on a routed reply, refreshes the cached identity so
    /// later synchronous [`Self::session_id`]/[`Self::target`] stay accurate.
    async fn call(&self, request: IpcRequest) -> Result<IpcResponse, TargetError> {
        let response = self.roundtrip(request).await?;
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
    async fn desktop_ack(&self, request: IpcRequest) -> Result<Routed<()>, CallError> {
        match self.call(request).await? {
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

    async fn system_info(&self) -> Result<Routed<SystemInfoReport>, CallError> {
        match self.call(IpcRequest::SystemInfo {}).await? {
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
        params: ExecParams,
        output_cap: usize,
    ) -> Result<Routed<ExecReport>, CallError> {
        let request = IpcRequest::Exec {
            params,
            output_cap: output_cap as u64,
        };
        match self.call(request).await? {
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
        job: JobId,
        data: Vec<u8>,
        sensitive: bool,
    ) -> Result<Routed<()>, CallError> {
        let request = IpcRequest::WriteStdin {
            job,
            data: B64Bytes::new(data),
            sensitive,
        };
        match self.call(request).await? {
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

    async fn cancel(&self, job: JobId) -> Result<Routed<()>, CallError> {
        match self.call(IpcRequest::Cancel { job }).await? {
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

    async fn list_jobs(&self) -> Result<Routed<Vec<JobSummary>>, CallError> {
        match self.call(IpcRequest::ListJobs {}).await? {
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

    async fn fs_stat(&self, path: RemotePath) -> Result<Routed<StatReport>, CallError> {
        match self.call(IpcRequest::FsStat { path }).await? {
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
        path: RemotePath,
        continue_after: Option<String>,
    ) -> Result<Routed<DirPage>, CallError> {
        let request = IpcRequest::FsList {
            path,
            continue_after,
        };
        match self.call(request).await? {
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

    async fn make_dir(&self, path: RemotePath) -> Result<Routed<()>, CallError> {
        match self.call(IpcRequest::MakeDir { path }).await? {
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
        from: RemotePath,
        to: RemotePath,
        overwrite: bool,
    ) -> Result<Routed<()>, CallError> {
        let request = IpcRequest::Move {
            from,
            to,
            overwrite,
        };
        match self.call(request).await? {
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

    async fn fs_delete(&self, path: RemotePath) -> Result<Routed<()>, CallError> {
        match self.call(IpcRequest::Delete { path }).await? {
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

    async fn remove_dir(&self, path: RemotePath) -> Result<Routed<()>, CallError> {
        match self.call(IpcRequest::RemoveDir { path }).await? {
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

    async fn upload(&self, params: UploadParams) -> Result<Routed<UploadReport>, CallError> {
        match self.call(IpcRequest::Upload { params }).await? {
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

    async fn download(&self, params: DownloadParams) -> Result<Routed<DownloadReport>, CallError> {
        match self.call(IpcRequest::Download { params }).await? {
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

    async fn list_staged(&self) -> Result<Routed<Vec<StagedEntry>>, CallError> {
        match self.call(IpcRequest::ListStaged {}).await? {
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

    async fn cleanup(&self, trigger: CleanupTrigger) -> Result<Routed<CleanupSummary>, CallError> {
        match self.call(IpcRequest::Cleanup { trigger }).await? {
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
        params: CaptureParams,
    ) -> Result<Routed<CaptureReport>, CallError> {
        match self.call(IpcRequest::CaptureDesktop { params }).await? {
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
        action: PointerAction,
        button: PointerButton,
        x: i32,
        y: i32,
    ) -> Result<Routed<()>, CallError> {
        self.desktop_ack(IpcRequest::DesktopPointer {
            action,
            button,
            x,
            y,
        })
        .await
    }

    async fn desktop_scroll(
        &self,
        x: i32,
        y: i32,
        dx: i32,
        dy: i32,
    ) -> Result<Routed<()>, CallError> {
        self.desktop_ack(IpcRequest::DesktopScroll { x, y, dx, dy })
            .await
    }

    async fn desktop_key(&self, action: KeyAction, key: String) -> Result<Routed<()>, CallError> {
        self.desktop_ack(IpcRequest::DesktopKey { action, key })
            .await
    }

    async fn desktop_type(&self, text: String) -> Result<Routed<()>, CallError> {
        self.desktop_ack(IpcRequest::DesktopType { text }).await
    }

    async fn desktop_hotkey(&self, keys: Vec<String>) -> Result<Routed<()>, CallError> {
        self.desktop_ack(IpcRequest::DesktopHotkey { keys }).await
    }

    async fn desktop_wait(&self, millis: u64) -> Result<Routed<()>, CallError> {
        self.desktop_ack(IpcRequest::DesktopWait { millis }).await
    }

    async fn clipboard_get(&self) -> Result<Routed<Option<String>>, CallError> {
        match self.call(IpcRequest::ClipboardGet {}).await? {
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

    async fn clipboard_set(&self, text: String) -> Result<Routed<()>, CallError> {
        self.desktop_ack(IpcRequest::ClipboardSet { text }).await
    }
}
