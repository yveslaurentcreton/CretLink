//! The console-side IPC server that routes MCP-bridge tool calls to the one
//! active, consented session (ADR-0004, ST-M004-002, ST-M004-004).
//!
//! The console is the sole routing authority. This server accepts same-user
//! bridge connections on the local IPC endpoint ([`cretlink_mcp::ipc`]) and
//! forwards each request to a [`ConsoleRoute`] — normally the
//! [`crate::ConsoleSessionService`]. Concurrent bridge connections multiplex
//! against the single selected session.
//!
//! # Fail-closed (REQ-S-009)
//!
//! When no consented active session is bound, every routed request returns
//! [`cretlink_mcp::IpcError::NoConsentedSession`] and nothing runs. A revoke or
//! stop flips the route fail-closed at once (see [`crate::ConsoleSessionService`]),
//! so an outstanding or later request can never reach the old session.
//!
//! # Audit (REQ-S-017, ST-M003-006, ST-M004-004)
//!
//! Bridge-connection accept/refuse and each routed tool call are recorded
//! through an [`IpcAudit`] sink with the routed [`SessionId`] and actor
//! [`Actor::Mcp`], using the `cretlink-shared` audit vocabulary. Every routed
//! call writes **two** records sharing a console-minted monotone `call_id`
//! (ADR-0008 Appendix A): a capture-time `<action>_start` record written
//! *before* the call reaches the target — so an action that already ran on the
//! customer machine can never be traceless, even if the console dies mid-call —
//! and a completion record with the honest outcome, duration, and result
//! detail (`job_id`, `exit_code`, paths, digests). Because `AuditWriter` is
//! single-threaded (its clock is not `Send`), [`ChannelAudit`] forwards
//! entries over a bounded channel the owner drains onto a real writer
//! (REQ-N-006).
//!
//! # Audit never blocks the stop path (REQ-S-004)
//!
//! [`ChannelAudit::record_entry`] is **non-blocking** (`try_send`): if the
//! drain's file I/O stalls (a dying disk, an AV lock), audit degrades — the
//! overflowing entries are counted and surfaced honestly, both immediately on
//! stderr and as a final `audit_degraded` record — but no async task is ever
//! pinned on audit, so Ctrl+C, cancel, quit, and revoke stay reachable.
//! Safety beats completeness: the channel bound is generous, so drops happen
//! only under a real drain stall, never in normal operation.
//!
//! Command text is truncated to the audit field bound; a command the caller
//! marked `sensitive` is recorded only as a `[REDACTED]` marker, and
//! environment override *values* are always redacted structurally — the name
//! may appear, the value never does (REQ-S-018).

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use cretlink_mcp::ipc::wire::{IPC_PROTOCOL, read_message, write_message};
use cretlink_mcp::ipc::{IpcListener, IpcStream};
use cretlink_mcp::{
    CaptureParams, CaptureReport, CleanupSummary, DirPage, DownloadParams, DownloadReport,
    ExecParams, ExecReport, IpcError, IpcRequest, IpcResponse, StatReport, SystemInfoReport,
    TargetError, TermFrame, UploadParams, UploadReport,
};
use cretlink_protocol::{
    B64Bytes, CleanupPolicy, CleanupTrigger, JobId, JobOutcome, JobSummary, KeyAction,
    PointerAction, PointerButton, Purpose, RemotePath, SessionId, Shell, StagedEntry,
    TargetIdentity,
};
use cretlink_shared::{Actor, AuditArgs, AuditValue, Outcome, redact_no_digest, truncate_field};

use crate::service::{TermAttachment, TermEvent};

/// One routing target for bridge requests: the console's face on the selected
/// session. Every method fails closed with [`TargetError::NoConsentedSession`]
/// when no consented active session is bound.
#[async_trait]
pub trait ConsoleRoute: Send + Sync + 'static {
    /// The consent-bound identity when active, else `None` (fail-closed).
    fn consented(&self) -> Option<(SessionId, TargetIdentity)>;

    /// Report the routed target's identity and basic system state.
    async fn system_info(&self, purpose: Option<Purpose>) -> Result<SystemInfoReport, TargetError>;

    /// Run a command to completion, collecting output up to `output_cap` bytes.
    async fn exec(
        &self,
        purpose: Option<Purpose>,
        params: ExecParams,
        output_cap: usize,
    ) -> Result<ExecReport, TargetError>;

    /// Feed bytes to a running job's standard input.
    async fn write_stdin(
        &self,
        purpose: Option<Purpose>,
        job: JobId,
        data: Vec<u8>,
    ) -> Result<(), TargetError>;

    /// Request cancellation of a running job.
    async fn cancel(&self, purpose: Option<Purpose>, job: JobId) -> Result<(), TargetError>;

    /// List the jobs the routed target currently tracks.
    async fn list_jobs(&self, purpose: Option<Purpose>) -> Result<Vec<JobSummary>, TargetError>;

    /// Stat a path on the routed target (never returns content).
    async fn fs_stat(
        &self,
        purpose: Option<Purpose>,
        path: RemotePath,
    ) -> Result<StatReport, TargetError>;

    /// List one page of a directory on the routed target.
    async fn fs_list(
        &self,
        purpose: Option<Purpose>,
        path: RemotePath,
        continue_after: Option<String>,
    ) -> Result<DirPage, TargetError>;

    /// Create a directory on the routed target.
    async fn make_dir(&self, purpose: Option<Purpose>, path: RemotePath)
    -> Result<(), TargetError>;

    /// Move (rename) a path on the routed target.
    async fn fs_move(
        &self,
        purpose: Option<Purpose>,
        from: RemotePath,
        to: RemotePath,
        overwrite: bool,
    ) -> Result<(), TargetError>;

    /// Delete a file or reparse point on the routed target (never a directory).
    async fn fs_delete(
        &self,
        purpose: Option<Purpose>,
        path: RemotePath,
    ) -> Result<(), TargetError>;

    /// Remove an empty directory on the routed target.
    async fn remove_dir(
        &self,
        purpose: Option<Purpose>,
        path: RemotePath,
    ) -> Result<(), TargetError>;

    /// Upload a local file into target staging and commit it verified.
    async fn upload(
        &self,
        purpose: Option<Purpose>,
        params: UploadParams,
    ) -> Result<UploadReport, TargetError>;

    /// Download a target file to a local path, verified by whole-file digest.
    async fn download(
        &self,
        purpose: Option<Purpose>,
        params: DownloadParams,
    ) -> Result<DownloadReport, TargetError>;

    /// List the staged artifacts the routed target currently tracks.
    async fn list_staged(&self, purpose: Option<Purpose>) -> Result<Vec<StagedEntry>, TargetError>;

    /// Run a staging cleanup for a trigger, reporting honest per-artifact
    /// outcomes.
    async fn cleanup(
        &self,
        purpose: Option<Purpose>,
        trigger: CleanupTrigger,
    ) -> Result<CleanupSummary, TargetError>;

    // --- Visual desktop control (ADR-0015, ST-M005-004) ---

    /// Capture one desktop frame from the routed session.
    async fn capture_desktop(
        &self,
        purpose: Option<Purpose>,
        params: CaptureParams,
    ) -> Result<CaptureReport, TargetError>;

    /// Inject a pointer action on the routed session's current input desktop.
    async fn desktop_pointer(
        &self,
        purpose: Option<Purpose>,
        action: PointerAction,
        button: PointerButton,
        x: i32,
        y: i32,
    ) -> Result<(), TargetError>;

    /// Inject a scroll action at the given origin on the routed session.
    async fn desktop_scroll(
        &self,
        purpose: Option<Purpose>,
        x: i32,
        y: i32,
        dx: i32,
        dy: i32,
    ) -> Result<(), TargetError>;

    /// Inject a single-key action on the routed session.
    async fn desktop_key(
        &self,
        purpose: Option<Purpose>,
        action: KeyAction,
        key: String,
    ) -> Result<(), TargetError>;

    /// Type a Unicode string on the routed session.
    async fn desktop_type(&self, purpose: Option<Purpose>, text: String)
    -> Result<(), TargetError>;

    /// Send a hotkey chord on the routed session.
    async fn desktop_hotkey(
        &self,
        purpose: Option<Purpose>,
        keys: Vec<String>,
    ) -> Result<(), TargetError>;

    /// Pause the routed session's input pipeline for a bounded interval.
    async fn desktop_wait(&self, purpose: Option<Purpose>, millis: u64) -> Result<(), TargetError>;

    /// Read the routed session's clipboard text (explicit-only, audited).
    async fn clipboard_get(&self, purpose: Option<Purpose>) -> Result<Option<String>, TargetError>;

    /// Set the routed session's clipboard text (explicit-only, audited).
    async fn clipboard_set(
        &self,
        purpose: Option<Purpose>,
        text: String,
    ) -> Result<(), TargetError>;

    // --- Interactive terminal (ST-M003-009) ---

    /// Open an interactive terminal on the routed session (a terminal is a job).
    async fn open_terminal(
        &self,
        shell: Shell,
        cols: u16,
        rows: u16,
        working_dir: Option<String>,
        env: BTreeMap<String, String>,
    ) -> Result<JobId, TargetError>;

    /// Attach a local consumer to an open terminal. A synchronous, local
    /// registry operation: **attaching grants no new authority** and never
    /// reaches the target. Fails closed with [`TargetError::NoConsentedSession`]
    /// when no consented session is bound.
    fn term_attach(&self, term: JobId) -> Result<TermAttachment, TargetError>;

    /// Feed input bytes to a terminal's single pseudo-console (muxed).
    async fn term_input(&self, term: JobId, data: Vec<u8>) -> Result<(), TargetError>;

    /// Resize a terminal's pseudo-console (last-writer-wins; out-of-bounds
    /// rejected).
    async fn term_resize(&self, term: JobId, cols: u16, rows: u16) -> Result<(), TargetError>;

    /// Detach exactly one consumer; the terminal outlives zero consumers.
    fn term_detach(&self, term: JobId, consumer: u64);

    /// Close a terminal, ending its job for every attached consumer.
    async fn term_close(&self, term: JobId) -> Result<(), TargetError>;
}

#[async_trait]
impl ConsoleRoute for crate::ConsoleSessionService {
    fn consented(&self) -> Option<(SessionId, TargetIdentity)> {
        crate::ConsoleSessionService::consented(self)
    }

    async fn system_info(&self, purpose: Option<Purpose>) -> Result<SystemInfoReport, TargetError> {
        crate::ConsoleSessionService::system_info(self, purpose).await
    }

    async fn exec(
        &self,
        purpose: Option<Purpose>,
        params: ExecParams,
        output_cap: usize,
    ) -> Result<ExecReport, TargetError> {
        crate::ConsoleSessionService::exec(self, purpose, params, output_cap).await
    }

    async fn write_stdin(
        &self,
        purpose: Option<Purpose>,
        job: JobId,
        data: Vec<u8>,
    ) -> Result<(), TargetError> {
        crate::ConsoleSessionService::exec_write_stdin(self, purpose, job, data).await
    }

    async fn cancel(&self, purpose: Option<Purpose>, job: JobId) -> Result<(), TargetError> {
        crate::ConsoleSessionService::exec_cancel(self, purpose, job).await
    }

    async fn list_jobs(&self, purpose: Option<Purpose>) -> Result<Vec<JobSummary>, TargetError> {
        crate::ConsoleSessionService::exec_list_jobs(self, purpose).await
    }

    async fn fs_stat(
        &self,
        purpose: Option<Purpose>,
        path: RemotePath,
    ) -> Result<StatReport, TargetError> {
        crate::ConsoleSessionService::fs_stat(self, purpose, path).await
    }

    async fn fs_list(
        &self,
        purpose: Option<Purpose>,
        path: RemotePath,
        continue_after: Option<String>,
    ) -> Result<DirPage, TargetError> {
        crate::ConsoleSessionService::fs_list(self, purpose, path, continue_after).await
    }

    async fn make_dir(
        &self,
        purpose: Option<Purpose>,
        path: RemotePath,
    ) -> Result<(), TargetError> {
        crate::ConsoleSessionService::make_dir(self, purpose, path).await
    }

    async fn fs_move(
        &self,
        purpose: Option<Purpose>,
        from: RemotePath,
        to: RemotePath,
        overwrite: bool,
    ) -> Result<(), TargetError> {
        crate::ConsoleSessionService::fs_move(self, purpose, from, to, overwrite).await
    }

    async fn fs_delete(
        &self,
        purpose: Option<Purpose>,
        path: RemotePath,
    ) -> Result<(), TargetError> {
        crate::ConsoleSessionService::fs_delete(self, purpose, path).await
    }

    async fn remove_dir(
        &self,
        purpose: Option<Purpose>,
        path: RemotePath,
    ) -> Result<(), TargetError> {
        crate::ConsoleSessionService::remove_dir(self, purpose, path).await
    }

    async fn upload(
        &self,
        purpose: Option<Purpose>,
        params: UploadParams,
    ) -> Result<UploadReport, TargetError> {
        crate::ConsoleSessionService::upload(self, purpose, params).await
    }

    async fn download(
        &self,
        purpose: Option<Purpose>,
        params: DownloadParams,
    ) -> Result<DownloadReport, TargetError> {
        crate::ConsoleSessionService::download(self, purpose, params).await
    }

    async fn list_staged(&self, purpose: Option<Purpose>) -> Result<Vec<StagedEntry>, TargetError> {
        crate::ConsoleSessionService::list_staged(self, purpose).await
    }

    async fn cleanup(
        &self,
        purpose: Option<Purpose>,
        trigger: CleanupTrigger,
    ) -> Result<CleanupSummary, TargetError> {
        crate::ConsoleSessionService::cleanup(self, purpose, trigger).await
    }

    async fn capture_desktop(
        &self,
        purpose: Option<Purpose>,
        params: CaptureParams,
    ) -> Result<CaptureReport, TargetError> {
        crate::ConsoleSessionService::capture_desktop(self, purpose, params).await
    }

    async fn desktop_pointer(
        &self,
        purpose: Option<Purpose>,
        action: PointerAction,
        button: PointerButton,
        x: i32,
        y: i32,
    ) -> Result<(), TargetError> {
        crate::ConsoleSessionService::desktop_pointer(self, purpose, action, button, x, y).await
    }

    async fn desktop_scroll(
        &self,
        purpose: Option<Purpose>,
        x: i32,
        y: i32,
        dx: i32,
        dy: i32,
    ) -> Result<(), TargetError> {
        crate::ConsoleSessionService::desktop_scroll(self, purpose, x, y, dx, dy).await
    }

    async fn desktop_key(
        &self,
        purpose: Option<Purpose>,
        action: KeyAction,
        key: String,
    ) -> Result<(), TargetError> {
        crate::ConsoleSessionService::desktop_key(self, purpose, action, key).await
    }

    async fn desktop_type(
        &self,
        purpose: Option<Purpose>,
        text: String,
    ) -> Result<(), TargetError> {
        crate::ConsoleSessionService::desktop_type(self, purpose, text).await
    }

    async fn desktop_hotkey(
        &self,
        purpose: Option<Purpose>,
        keys: Vec<String>,
    ) -> Result<(), TargetError> {
        crate::ConsoleSessionService::desktop_hotkey(self, purpose, keys).await
    }

    async fn desktop_wait(&self, purpose: Option<Purpose>, millis: u64) -> Result<(), TargetError> {
        crate::ConsoleSessionService::desktop_wait(self, purpose, millis).await
    }

    async fn clipboard_get(&self, purpose: Option<Purpose>) -> Result<Option<String>, TargetError> {
        crate::ConsoleSessionService::clipboard_get(self, purpose).await
    }

    async fn clipboard_set(
        &self,
        purpose: Option<Purpose>,
        text: String,
    ) -> Result<(), TargetError> {
        crate::ConsoleSessionService::clipboard_set(self, purpose, text).await
    }

    async fn open_terminal(
        &self,
        shell: Shell,
        cols: u16,
        rows: u16,
        working_dir: Option<String>,
        env: BTreeMap<String, String>,
    ) -> Result<JobId, TargetError> {
        crate::ConsoleSessionService::open_terminal(self, shell, cols, rows, working_dir, env).await
    }

    fn term_attach(&self, term: JobId) -> Result<TermAttachment, TargetError> {
        crate::ConsoleSessionService::term_attach(self, term)
    }

    async fn term_input(&self, term: JobId, data: Vec<u8>) -> Result<(), TargetError> {
        crate::ConsoleSessionService::term_input(self, term, data).await
    }

    async fn term_resize(&self, term: JobId, cols: u16, rows: u16) -> Result<(), TargetError> {
        crate::ConsoleSessionService::term_resize(self, term, cols, rows).await
    }

    fn term_detach(&self, term: JobId, consumer: u64) {
        crate::ConsoleSessionService::term_detach(self, term, consumer);
    }

    async fn term_close(&self, term: JobId) -> Result<(), TargetError> {
        crate::ConsoleSessionService::term_close(self, term).await
    }
}

/// A route that is always fail-closed: no consented session, every call refused.
/// Used to serve the IPC endpoint while the console is idle (REQ-S-009).
#[derive(Debug, Default, Clone, Copy)]
pub struct ClosedRoute;

#[async_trait]
impl ConsoleRoute for ClosedRoute {
    fn consented(&self) -> Option<(SessionId, TargetIdentity)> {
        None
    }

    async fn system_info(
        &self,
        _purpose: Option<Purpose>,
    ) -> Result<SystemInfoReport, TargetError> {
        Err(TargetError::NoConsentedSession)
    }

    async fn exec(
        &self,
        _purpose: Option<Purpose>,
        _params: ExecParams,
        _cap: usize,
    ) -> Result<ExecReport, TargetError> {
        Err(TargetError::NoConsentedSession)
    }

    async fn write_stdin(
        &self,
        _purpose: Option<Purpose>,
        _job: JobId,
        _data: Vec<u8>,
    ) -> Result<(), TargetError> {
        Err(TargetError::NoConsentedSession)
    }

    async fn cancel(&self, _purpose: Option<Purpose>, _job: JobId) -> Result<(), TargetError> {
        Err(TargetError::NoConsentedSession)
    }

    async fn list_jobs(&self, _purpose: Option<Purpose>) -> Result<Vec<JobSummary>, TargetError> {
        Err(TargetError::NoConsentedSession)
    }

    async fn fs_stat(
        &self,
        _purpose: Option<Purpose>,
        _path: RemotePath,
    ) -> Result<StatReport, TargetError> {
        Err(TargetError::NoConsentedSession)
    }

    async fn fs_list(
        &self,
        _purpose: Option<Purpose>,
        _path: RemotePath,
        _continue_after: Option<String>,
    ) -> Result<DirPage, TargetError> {
        Err(TargetError::NoConsentedSession)
    }

    async fn make_dir(
        &self,
        _purpose: Option<Purpose>,
        _path: RemotePath,
    ) -> Result<(), TargetError> {
        Err(TargetError::NoConsentedSession)
    }

    async fn fs_move(
        &self,
        _purpose: Option<Purpose>,
        _from: RemotePath,
        _to: RemotePath,
        _overwrite: bool,
    ) -> Result<(), TargetError> {
        Err(TargetError::NoConsentedSession)
    }

    async fn fs_delete(
        &self,
        _purpose: Option<Purpose>,
        _path: RemotePath,
    ) -> Result<(), TargetError> {
        Err(TargetError::NoConsentedSession)
    }

    async fn remove_dir(
        &self,
        _purpose: Option<Purpose>,
        _path: RemotePath,
    ) -> Result<(), TargetError> {
        Err(TargetError::NoConsentedSession)
    }

    async fn upload(
        &self,
        _purpose: Option<Purpose>,
        _params: UploadParams,
    ) -> Result<UploadReport, TargetError> {
        Err(TargetError::NoConsentedSession)
    }

    async fn download(
        &self,
        _purpose: Option<Purpose>,
        _params: DownloadParams,
    ) -> Result<DownloadReport, TargetError> {
        Err(TargetError::NoConsentedSession)
    }

    async fn list_staged(
        &self,
        _purpose: Option<Purpose>,
    ) -> Result<Vec<StagedEntry>, TargetError> {
        Err(TargetError::NoConsentedSession)
    }

    async fn cleanup(
        &self,
        _purpose: Option<Purpose>,
        _trigger: CleanupTrigger,
    ) -> Result<CleanupSummary, TargetError> {
        Err(TargetError::NoConsentedSession)
    }

    async fn capture_desktop(
        &self,
        _purpose: Option<Purpose>,
        _params: CaptureParams,
    ) -> Result<CaptureReport, TargetError> {
        Err(TargetError::NoConsentedSession)
    }

    async fn desktop_pointer(
        &self,
        _purpose: Option<Purpose>,
        _action: PointerAction,
        _button: PointerButton,
        _x: i32,
        _y: i32,
    ) -> Result<(), TargetError> {
        Err(TargetError::NoConsentedSession)
    }

    async fn desktop_scroll(
        &self,
        _purpose: Option<Purpose>,
        _x: i32,
        _y: i32,
        _dx: i32,
        _dy: i32,
    ) -> Result<(), TargetError> {
        Err(TargetError::NoConsentedSession)
    }

    async fn desktop_key(
        &self,
        _purpose: Option<Purpose>,
        _action: KeyAction,
        _key: String,
    ) -> Result<(), TargetError> {
        Err(TargetError::NoConsentedSession)
    }

    async fn desktop_type(
        &self,
        _purpose: Option<Purpose>,
        _text: String,
    ) -> Result<(), TargetError> {
        Err(TargetError::NoConsentedSession)
    }

    async fn desktop_hotkey(
        &self,
        _purpose: Option<Purpose>,
        _keys: Vec<String>,
    ) -> Result<(), TargetError> {
        Err(TargetError::NoConsentedSession)
    }

    async fn desktop_wait(
        &self,
        _purpose: Option<Purpose>,
        _millis: u64,
    ) -> Result<(), TargetError> {
        Err(TargetError::NoConsentedSession)
    }

    async fn clipboard_get(
        &self,
        _purpose: Option<Purpose>,
    ) -> Result<Option<String>, TargetError> {
        Err(TargetError::NoConsentedSession)
    }

    async fn clipboard_set(
        &self,
        _purpose: Option<Purpose>,
        _text: String,
    ) -> Result<(), TargetError> {
        Err(TargetError::NoConsentedSession)
    }

    async fn open_terminal(
        &self,
        _shell: Shell,
        _cols: u16,
        _rows: u16,
        _working_dir: Option<String>,
        _env: BTreeMap<String, String>,
    ) -> Result<JobId, TargetError> {
        Err(TargetError::NoConsentedSession)
    }

    fn term_attach(&self, _term: JobId) -> Result<TermAttachment, TargetError> {
        Err(TargetError::NoConsentedSession)
    }

    async fn term_input(&self, _term: JobId, _data: Vec<u8>) -> Result<(), TargetError> {
        Err(TargetError::NoConsentedSession)
    }

    async fn term_resize(&self, _term: JobId, _cols: u16, _rows: u16) -> Result<(), TargetError> {
        Err(TargetError::NoConsentedSession)
    }

    fn term_detach(&self, _term: JobId, _consumer: u64) {}

    async fn term_close(&self, _term: JobId) -> Result<(), TargetError> {
        Err(TargetError::NoConsentedSession)
    }
}

/// A sink for console-side audit events (REQ-S-017, ST-M003-006). The IPC
/// server attributes every bridge entry to [`Actor::Mcp`] at the source site;
/// other producers (the session runtime, the interactive CLI) stamp their own
/// actor when they build an [`AuditEntry`].
pub trait IpcAudit: Send + Sync + 'static {
    /// Records one audit entry.
    fn record(&self, entry: AuditEntry);
}

/// An audit sink that discards everything (the default when no durable audit is
/// wired). The honest fail-closed behaviour is unaffected.
#[derive(Debug, Default, Clone, Copy)]
pub struct NoIpcAudit;

impl IpcAudit for NoIpcAudit {
    fn record(&self, _entry: AuditEntry) {}
}

/// One recorded audit entry, forwarded over a bounded channel to the owner that
/// drains it onto a real writer. Carries the `cretlink-shared` vocabulary plus
/// the ADR-0008 Appendix A argument keys.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuditEntry {
    /// Who originated the action ([`Actor::Mcp`] for bridge calls,
    /// [`Actor::Console`] for manual console commands, the runtime's own
    /// attribution for lifecycle events).
    pub actor: Actor,
    /// The routed session, or `None` when no consented session was bound (the
    /// per-session writer stamps its own session id on the record either way).
    pub session: Option<SessionId>,
    /// A stable, secret-free action label.
    pub action: &'static str,
    /// Structured, redaction-aware arguments (never a live secret).
    pub args: AuditArgs,
    /// The outcome of the action.
    pub outcome: Outcome,
    /// Named SHA-256 digests (e.g. a verified transfer digest).
    pub digests: BTreeMap<String, String>,
    /// Wall-clock duration of the action in milliseconds, when measured.
    pub duration_ms: Option<u64>,
}

impl AuditEntry {
    /// A minimal entry with empty args, no digests, and no duration.
    #[must_use]
    pub fn event(
        actor: Actor,
        session: Option<SessionId>,
        action: &'static str,
        outcome: Outcome,
    ) -> Self {
        Self {
            actor,
            session,
            action,
            args: AuditArgs::new(),
            outcome,
            digests: BTreeMap::new(),
            duration_ms: None,
        }
    }
}

/// The receiving half of the audit channel plus the shared drop counter, so
/// the drain can surface how many entries overflowed while it stalled.
#[derive(Debug)]
pub struct AuditReceiver {
    rx: std::sync::mpsc::Receiver<AuditEntry>,
    dropped: Arc<AtomicU64>,
}

impl AuditReceiver {
    /// Blocks until the next entry, or `Err` when every sender is gone.
    ///
    /// # Errors
    ///
    /// [`std::sync::mpsc::RecvError`] once every sender has been dropped.
    pub fn recv(&self) -> Result<AuditEntry, std::sync::mpsc::RecvError> {
        self.rx.recv()
    }

    /// Non-blocking receive (used by tests inspecting the stream directly).
    ///
    /// # Errors
    ///
    /// [`std::sync::mpsc::TryRecvError`] when the channel is empty or closed.
    pub fn try_recv(&self) -> Result<AuditEntry, std::sync::mpsc::TryRecvError> {
        self.rx.try_recv()
    }

    /// How many entries were dropped so far because the channel was full
    /// (i.e. the drain stalled).
    #[must_use]
    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::SeqCst)
    }
}

/// An audit sink that forwards entries over a **bounded** channel (REQ-N-006)
/// without ever blocking the caller (REQ-S-004).
///
/// The owner (the console's audit service, or a test) drains the paired
/// [`AuditReceiver`] and writes each entry to a `cretlink_shared::AuditWriter`;
/// this keeps the single-threaded, non-`Send` writer off the concurrent
/// connection tasks. The send is `try_send`: **audit never blocks the stop
/// path**. A full channel — which only happens when the drain's file I/O has
/// stalled, given the generous bound — increments a shared drop counter and
/// raises a prominent degraded warning; the drain later appends an honest
/// `audit_degraded` record carrying the count. This trades completeness under
/// an I/O stall for a stop/revoke path that can never freeze on audit; drops
/// are always counted and surfaced, never silent.
///
/// An optional **live** sender echoes bridge-originated entries to the
/// interactive CLI so AI activity is visible the moment it starts
/// (REQ-F-017); the echo is bounded and non-blocking (see
/// [`ChannelAudit::set_live_sender`]) and independent of drain health.
#[derive(Debug, Clone)]
pub struct ChannelAudit {
    tx: std::sync::mpsc::SyncSender<AuditEntry>,
    dropped: Arc<AtomicU64>,
    degraded_warned: Arc<AtomicBool>,
    drain_gone_warned: Arc<AtomicBool>,
    live: Arc<OnceLock<tokio::sync::mpsc::Sender<AuditEntry>>>,
}

impl ChannelAudit {
    /// Creates a bounded channel audit sink and its receiver.
    #[must_use]
    pub fn bounded(capacity: usize) -> (Self, AuditReceiver) {
        let (tx, rx) = std::sync::mpsc::sync_channel(capacity);
        let dropped = Arc::new(AtomicU64::new(0));
        (
            Self {
                tx,
                dropped: Arc::clone(&dropped),
                degraded_warned: Arc::new(AtomicBool::new(false)),
                drain_gone_warned: Arc::new(AtomicBool::new(false)),
                live: Arc::new(OnceLock::new()),
            },
            AuditReceiver { rx, dropped },
        )
    }

    /// Attaches the live activity echo consumed by the interactive CLI
    /// (REQ-F-017). Set at most once; later calls are ignored.
    ///
    /// The sender must be the producing half of a **bounded** channel: the
    /// echo is a cosmetic near-real-time view, so [`record_entry`] forwards
    /// with a non-blocking `try_send` and simply drops the line when the
    /// channel is full (REQ-N-006 — the echo can never grow unboundedly if the
    /// CLI consumer is starved by a high-frequency output stream). The durable
    /// audit log remains the complete source of truth; a dropped echo line is
    /// still fully recorded there.
    ///
    /// [`record_entry`]: ChannelAudit::record_entry
    pub fn set_live_sender(&self, tx: tokio::sync::mpsc::Sender<AuditEntry>) {
        let _ = self.live.set(tx);
    }

    /// How many entries were dropped so far because the drain stalled.
    #[must_use]
    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::SeqCst)
    }

    /// Forwards one entry **without ever blocking** (REQ-S-004: the stop path
    /// must stay reachable even when the audit drain's I/O stalls). A full
    /// channel counts the drop and warns prominently once; the drain surfaces
    /// the count again as a final `audit_degraded` record. A gone drain
    /// discards with one warning.
    pub fn record_entry(&self, entry: AuditEntry) {
        // Live echo first: near-real-time visibility of AI activity must not
        // depend on drain health. Bounded + non-blocking — the echo is
        // cosmetic, so a full channel just drops the line (the durable log
        // still records it in full), keeping the echo bounded (REQ-N-006).
        if entry.actor == Actor::Mcp
            && let Some(live) = self.live.get()
        {
            let _ = live.try_send(entry.clone());
        }
        match self.tx.try_send(entry) {
            Ok(()) => {}
            Err(std::sync::mpsc::TrySendError::Full(_)) => {
                self.dropped.fetch_add(1, Ordering::SeqCst);
                if !self.degraded_warned.swap(true, Ordering::SeqCst) {
                    eprintln!(
                        "\n==================== AUDIT DEGRADED ====================\n\
                         WARNING: the audit writer is stalled; audit entries are\n\
                         being DROPPED (counted, surfaced at session end). The\n\
                         session and its stop controls remain fully functional.\n\
                         ========================================================"
                    );
                }
            }
            Err(std::sync::mpsc::TrySendError::Disconnected(_)) => {
                if !self.drain_gone_warned.swap(true, Ordering::SeqCst) {
                    eprintln!(
                        "WARNING: the audit drain has stopped; further session actions are NOT \
                         being recorded (REQ-S-017)"
                    );
                }
            }
        }
    }
}

impl IpcAudit for ChannelAudit {
    fn record(&self, entry: AuditEntry) {
        self.record_entry(entry);
    }
}

/// How long [`IpcServer::serve`] lets in-flight connections finish their
/// current call after a shutdown request before aborting them. Bounded so a
/// quit can never hang on a lingering bridge connection.
pub const SHUTDOWN_GRACE: Duration = Duration::from_secs(3);

/// RAII guard for the routed-call in-flight counter (ST-M004-006). Incrementing
/// on creation and decrementing on drop means an early return, `?`, or panic in
/// the routing path can never leak a count, so the console's "AI active"
/// indicator returns to idle honestly. The counter is a single bounded atomic
/// (REQ-N-006).
struct InFlight(Arc<AtomicU64>);

impl InFlight {
    fn enter(counter: &Arc<AtomicU64>) -> Self {
        counter.fetch_add(1, Ordering::SeqCst);
        Self(Arc::clone(counter))
    }
}

impl Drop for InFlight {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

/// The console-side IPC server.
#[derive(Debug)]
pub struct IpcServer;

impl IpcServer {
    /// Serves `listener`, routing every accepted bridge connection's requests
    /// to `route` and auditing through `audit`, until `shutdown` fires or the
    /// listener fails (`accept` errors).
    ///
    /// Shutdown is **graceful with a bounded deadline**: accepting stops
    /// immediately, but in-flight connections get [`SHUTDOWN_GRACE`] to finish
    /// their current call so completion records (which carry the honest
    /// outcome of an action that already ran on the customer machine) still
    /// reach the audit drain; whatever remains after the deadline is aborted.
    /// Either way every connection's `route` and `audit` handles are released
    /// before this future returns, which lets the audit drain observe
    /// end-of-senders and seal the session log (ST-M003-006). A severed
    /// connection surfaces to the bridge as an honest disconnect, never a
    /// fabricated result.
    pub async fn serve(
        mut listener: IpcListener,
        route: Arc<dyn ConsoleRoute>,
        audit: Arc<dyn IpcAudit>,
        in_flight: Arc<AtomicU64>,
        mut shutdown: tokio::sync::oneshot::Receiver<()>,
    ) {
        // One console-minted, session-scoped monotone id per routed call
        // (ADR-0008 Appendix A `call_id`): shared by every connection this
        // server instance accepts.
        let call_ids = Arc::new(AtomicU64::new(1));
        let mut connections = tokio::task::JoinSet::new();
        loop {
            tokio::select! {
                // A dropped sender counts as a shutdown request: fail closed.
                _ = &mut shutdown => break,
                accepted = listener.accept() => {
                    match accepted {
                        Ok(stream) => {
                            connections.spawn(handle_connection(
                                stream,
                                Arc::clone(&route),
                                Arc::clone(&audit),
                                Arc::clone(&call_ids),
                                Arc::clone(&in_flight),
                            ));
                        }
                        Err(_) => break,
                    }
                }
                // Reap finished connection tasks so the set stays bounded by
                // the number of *live* connections.
                Some(_) = connections.join_next() => {}
            }
        }
        // Graceful drain: no new connections (listener dropped), a bounded
        // window for in-flight calls to record their completion, then abort.
        drop(listener);
        let deadline = tokio::time::sleep(SHUTDOWN_GRACE);
        tokio::pin!(deadline);
        loop {
            tokio::select! {
                () = &mut deadline => {
                    connections.shutdown().await;
                    break;
                }
                next = connections.join_next() => {
                    if next.is_none() {
                        break;
                    }
                }
            }
        }
    }
}

/// Serves one bridge connection: audits the accept, then answers each request
/// until the peer closes or a framing error occurs.
async fn handle_connection(
    mut stream: IpcStream,
    route: Arc<dyn ConsoleRoute>,
    audit: Arc<dyn IpcAudit>,
    call_ids: Arc<AtomicU64>,
    in_flight: Arc<AtomicU64>,
) {
    let routed = route.consented().map(|(session, _)| session);
    // Accept vs refuse: a connection with a consented session is an accept;
    // without one it is a refusal (nothing it asks can run).
    audit.record(AuditEntry::event(
        Actor::Mcp,
        routed,
        "mcp_bridge_connect",
        if routed.is_some() {
            Outcome::Ok
        } else {
            Outcome::Failed
        },
    ));

    loop {
        let request: IpcRequest = match read_message(&mut stream).await {
            Ok(request) => request,
            // EOF or any framing error: close this connection fail-closed.
            Err(_) => return,
        };
        // A successful `TermAttach` switches this connection into terminal duplex
        // stream mode for the rest of its life (ST-M003-009); on refusal it keeps
        // serving request/response. Terminal clients are the second, distinct
        // class of local IPC consumer (attributed to `Actor::Terminal`).
        if let IpcRequest::TermAttach { term } = request {
            match handle_term_attach(&mut stream, route.as_ref(), audit.as_ref(), &call_ids, term)
                .await
            {
                AttachOutcome::Refused => continue,
                AttachOutcome::Stream(attachment, call_id) => {
                    run_terminal_duplex(
                        stream,
                        Arc::clone(&route),
                        Arc::clone(&audit),
                        attachment,
                        call_id,
                    )
                    .await;
                    return;
                }
            }
        }
        // Count only routed AI tool calls as in-flight activity — not the
        // authority-free `Hello` handshake, and not terminal messages (those are
        // the technician `Terminal` actor, never the AI). The RAII guard
        // decrements on every exit path (early return, `?`, panic) so a count can
        // never leak.
        let counts_as_ai = !matches!(
            request,
            IpcRequest::Hello { .. }
                | IpcRequest::TermOpen { .. }
                | IpcRequest::TermInput { .. }
                | IpcRequest::TermResize { .. }
                | IpcRequest::TermDetach { .. }
                | IpcRequest::TermClose { .. }
        );
        let response = {
            let _active = counts_as_ai.then(|| InFlight::enter(&in_flight));
            route_request(route.as_ref(), audit.as_ref(), &call_ids, request).await
        };
        if write_message(&mut stream, &response).await.is_err() {
            return;
        }
    }
}

/// The result of an attach attempt on a connection.
enum AttachOutcome {
    /// The attach was refused (fail-closed) or the client vanished; an error was
    /// answered and the connection keeps serving request/response.
    Refused,
    /// The attach succeeded; the connection is now a terminal consumer and must
    /// run the duplex stream to completion.
    Stream(TermAttachment, u64),
}

/// Handles one `TermAttach` request, auditing it as a `Terminal`-actor action.
///
/// On success it answers [`IpcResponse::TermAttached`] and returns the
/// attachment so the caller runs the duplex stream. On a fail-closed refusal
/// (no consented session, unknown/exited terminal, or the consumer cap) it
/// answers an honest error and returns [`AttachOutcome::Refused`]. Attach grants
/// no new authority: it is a local registry operation on the already-consented
/// session (REQ-S-009, ST-M003-009).
async fn handle_term_attach(
    stream: &mut IpcStream,
    route: &dyn ConsoleRoute,
    audit: &dyn IpcAudit,
    call_ids: &AtomicU64,
    term: JobId,
) -> AttachOutcome {
    let routed = route.consented();
    let call_id = call_ids.fetch_add(1, Ordering::SeqCst);
    let base_args = AuditArgs::new()
        .with("call_id", len_value(call_id))
        .with("job_id", job_id_value(term));
    let mut start = AuditEntry::event(
        Actor::Terminal,
        routed.as_ref().map(|(session, _)| *session),
        "terminal_attach_start",
        Outcome::Ok,
    );
    start.args = base_args.clone();
    audit.record(start);

    match route.term_attach(term) {
        Ok(attachment) => {
            record_terminal_event(
                audit,
                &routed,
                "terminal_attach",
                Outcome::Ok,
                base_args.clone(),
            );
            let response = IpcResponse::TermAttached {
                session: attachment.session,
                target: attachment.target.clone(),
                call_id,
                term,
            };
            if write_message(stream, &response).await.is_err() {
                // The client vanished right after attaching: detach at once so
                // the registry never keeps a dead consumer.
                route.term_detach(term, attachment.consumer);
                return AttachOutcome::Refused;
            }
            AttachOutcome::Stream(attachment, call_id)
        }
        Err(err) => {
            record_terminal_event(
                audit,
                &routed,
                "terminal_attach",
                Outcome::Failed,
                base_args,
            );
            let error = match err {
                TargetError::NoConsentedSession => IpcError::NoConsentedSession,
                other => IpcError::Target(other),
            };
            let _ = write_message(stream, &IpcResponse::Error { error, call_id }).await;
            AttachOutcome::Refused
        }
    }
}

/// Runs the terminal duplex stream for one attached consumer until it ends
/// (ST-M003-009).
///
/// The console pushes [`TermFrame`]s (output/exit/revoked/dropped) while the
/// client keeps sending input/resize/detach/close. The stream is split so the
/// non-cancel-safe [`read_message`] loop runs alone in the reader task and is
/// never cancelled mid-frame; the writer runs inline. Every exit path tears the
/// consumer down: a terminal frame ends the writer, a client detach/disconnect
/// ends the reader, and either aborts the other — never a silent hang, never a
/// fabricated clean close (REQ-N-008).
async fn run_terminal_duplex(
    stream: IpcStream,
    route: Arc<dyn ConsoleRoute>,
    audit: Arc<dyn IpcAudit>,
    attachment: TermAttachment,
    call_id: u64,
) {
    let TermAttachment {
        session,
        target,
        term,
        consumer,
        mut events,
        overrun,
    } = attachment;
    let routed = Some((session, target.clone()));

    let (read_half, write_half) = tokio::io::split(stream);

    // The reader forwards client input/resize/close and signals a detach or a
    // lost link. It owns the read half alone, so its `read_message` is never
    // cancelled mid-frame.
    let detach = Arc::new(tokio::sync::Notify::new());
    let reader = tokio::spawn(terminal_reader(
        read_half,
        Arc::clone(&route),
        term,
        Arc::clone(&detach),
    ));

    // The writer pushes frames until a terminal frame, a detach, or the channel
    // closes. `Output` is the only non-terminal frame.
    let mut write_half = write_half;
    loop {
        let frame = tokio::select! {
            maybe = events.recv() => match maybe {
                Some(TermEvent::Output(bytes)) => TermFrame::Output {
                    session,
                    target: target.clone(),
                    term,
                    data: B64Bytes::new(bytes),
                },
                Some(TermEvent::Exited { exit_code, outcome }) => TermFrame::Exited {
                    session,
                    target: target.clone(),
                    term,
                    exit_code,
                    outcome,
                },
                Some(TermEvent::Revoked) => TermFrame::Revoked {
                    session,
                    target: target.clone(),
                    term,
                },
                None => {
                    // The channel closed. An overrun means this consumer fell
                    // behind and was dropped so the shared stream stayed intact
                    // for the others (REQ-N-006); otherwise the driver ended and
                    // the stream is torn down fail-closed. Neither is a clean
                    // close (REQ-N-008).
                    if overrun.load(Ordering::SeqCst) {
                        TermFrame::Dropped { session, target: target.clone(), term }
                    } else {
                        TermFrame::Revoked { session, target: target.clone(), term }
                    }
                }
            },
            () = detach.notified() => break,
        };
        let is_terminal = !matches!(frame, TermFrame::Output { .. });
        if write_message(&mut write_half, &frame).await.is_err() {
            break;
        }
        if is_terminal {
            break;
        }
    }

    reader.abort();
    route.term_detach(term, consumer);
    record_terminal_event(
        audit.as_ref(),
        &routed,
        "terminal_detach",
        Outcome::Ok,
        AuditArgs::new()
            .with("call_id", len_value(call_id))
            .with("job_id", job_id_value(term)),
    );
}

/// The reader half of a terminal duplex stream: forwards client
/// input/resize/close to the routed session and signals `detach` when the client
/// detaches or the link is lost. `TermClose` closes the terminal for everyone;
/// the writer then delivers the honest exit and ends.
async fn terminal_reader(
    mut read_half: tokio::io::ReadHalf<IpcStream>,
    route: Arc<dyn ConsoleRoute>,
    term: JobId,
    detach: Arc<tokio::sync::Notify>,
) {
    loop {
        match read_message::<_, IpcRequest>(&mut read_half).await {
            Ok(IpcRequest::TermInput { data, .. }) => {
                let _ = route.term_input(term, data.into_inner()).await;
            }
            Ok(IpcRequest::TermResize { cols, rows, .. }) => {
                let _ = route.term_resize(term, cols, rows).await;
            }
            Ok(IpcRequest::TermClose { .. }) => {
                // Close the terminal for every consumer; the writer delivers the
                // honest exit and ends the stream. Keep reading until then.
                let _ = route.term_close(term).await;
            }
            Ok(IpcRequest::TermDetach { .. }) => {
                detach.notify_one();
                return;
            }
            // Any other message in duplex mode — or a decode error / EOF — ends
            // the stream honestly.
            Ok(_) | Err(_) => {
                detach.notify_one();
                return;
            }
        }
    }
}

/// Records one `Terminal`-actor audit event (attach/detach lifecycle).
fn record_terminal_event(
    audit: &dyn IpcAudit,
    routed: &Option<(SessionId, TargetIdentity)>,
    action: &'static str,
    outcome: Outcome,
    args: AuditArgs,
) {
    let mut entry = AuditEntry::event(
        Actor::Terminal,
        routed.as_ref().map(|(session, _)| *session),
        action,
        outcome,
    );
    entry.args = args;
    audit.record(entry);
}

/// Routes one request to the session, auditing each tool call as **two**
/// records that share a console-minted monotone `call_id` (ADR-0008
/// Appendix A, ST-M003-006): a capture-time `<action>_start` record written
/// **before** the call reaches the target — so an action that already ran on
/// the customer machine is never traceless, even if the console dies or the
/// connection is aborted mid-call — and a completion record with the honest
/// outcome, duration, and result detail. Free-text fields go through
/// `truncate_field`; sensitive command text and environment override values
/// are redacted at capture time.
#[allow(clippy::too_many_lines)]
async fn route_request(
    route: &dyn ConsoleRoute,
    audit: &dyn IpcAudit,
    call_ids: &AtomicU64,
    request: IpcRequest,
) -> IpcResponse {
    // A stated purpose arrives wrapped around exactly one routed tool call (IPC
    // minor 9, ST-M009-007). Anything else inside the wrapper — a handshake, a
    // terminal message, or another wrapper — is refused before anything runs.
    // A bare request (an older bridge) carries no purpose and is served as
    // before (REQ-N-012).
    let (request, purpose) = match request {
        IpcRequest::WithPurpose { purpose, call } if call.is_tool_call() => (*call, Some(purpose)),
        IpcRequest::WithPurpose { .. } => {
            return IpcResponse::Error {
                error: IpcError::Malformed {
                    message: "a stated purpose may only wrap a routed tool call".to_string(),
                },
                call_id: 0,
            };
        }
        other => (other, None),
    };
    match request {
        IpcRequest::Hello { protocol } => {
            if !IPC_PROTOCOL.is_compatible_with(protocol) {
                return IpcResponse::Error {
                    error: IpcError::IncompatibleVersion {
                        local: IPC_PROTOCOL,
                        peer: protocol,
                    },
                    // A handshake failure is not tied to a routed call.
                    call_id: 0,
                };
            }
            let (session, target) = match route.consented() {
                Some((session, target)) => (Some(session), Some(target)),
                None => (None, None),
            };
            IpcResponse::Hello {
                protocol: IPC_PROTOCOL,
                session,
                target,
            }
        }
        IpcRequest::SystemInfo {} => {
            let call = CallAudit::begin(
                audit,
                purpose.as_ref(),
                route.consented(),
                call_ids,
                "mcp_remote_system_info_start",
                "mcp_remote_system_info",
                AuditArgs::new(),
            );
            let result = route.system_info(purpose.clone()).await;
            let (routed, call_id, entry) = call.complete(&result);
            audit.record(entry);
            respond(
                call_id,
                routed,
                result,
                |session, target, call_id, report| IpcResponse::SystemInfo {
                    session,
                    target,
                    call_id,
                    report,
                },
            )
        }
        IpcRequest::Exec { params, output_cap } => {
            // Capture-time redaction (REQ-S-018): a sensitive command is only
            // ever a `[REDACTED]` marker; environment override values are
            // always redacted structurally (the name may appear, the value
            // never does).
            let mut args =
                AuditArgs::new().with("command", command_value(&params.command, params.sensitive));
            if !params.env.is_empty() {
                args.insert("env", env_value(&params.env));
            }
            let call = CallAudit::begin(
                audit,
                purpose.as_ref(),
                route.consented(),
                call_ids,
                "mcp_remote_exec_start",
                "mcp_remote_exec",
                args,
            );
            let result = route
                .exec(purpose.clone(), params, output_cap as usize)
                .await;
            let (routed, call_id, mut entry) = call.complete(&result);
            if let Ok(report) = &result {
                // Correlation (ST-M003-006): the job id, honest job outcome,
                // exit code, and the job's own wall-clock duration.
                entry.args.insert("job_id", job_id_value(report.job));
                if let Some(code) = report.exit_code {
                    entry
                        .args
                        .insert("exit_code", AuditValue::Int(i64::from(code)));
                }
                entry.outcome = job_outcome_to_audit(report.outcome);
                entry.duration_ms = Some(report.duration_ms);
            }
            audit.record(entry);
            respond(
                call_id,
                routed,
                result,
                |session, target, call_id, report| IpcResponse::Exec {
                    session,
                    target,
                    call_id,
                    report,
                },
            )
        }
        IpcRequest::WriteStdin {
            job,
            data,
            sensitive,
        } => {
            let written = data.len() as u64;
            // Stdin content is structurally never audited (REQ-S-018): only
            // its byte length is recorded, so the `sensitive` marker needs no
            // record-level effect here.
            let _ = sensitive;
            let args = AuditArgs::new()
                .with("job_id", job_id_value(job))
                .with("input_len", len_value(written));
            let call = CallAudit::begin(
                audit,
                purpose.as_ref(),
                route.consented(),
                call_ids,
                "mcp_remote_write_stdin_start",
                "mcp_remote_write_stdin",
                args,
            );
            let result = route
                .write_stdin(purpose.clone(), job, data.into_inner())
                .await;
            let (routed, call_id, entry) = call.complete(&result);
            audit.record(entry);
            respond(
                call_id,
                routed,
                result.map(|()| written),
                |session, target, call_id, written| IpcResponse::WriteStdin {
                    session,
                    target,
                    call_id,
                    written,
                },
            )
        }
        IpcRequest::Cancel { job } => {
            let call = CallAudit::begin(
                audit,
                purpose.as_ref(),
                route.consented(),
                call_ids,
                "mcp_remote_cancel_start",
                "mcp_remote_cancel",
                AuditArgs::new().with("job_id", job_id_value(job)),
            );
            let result = route.cancel(purpose.clone(), job).await;
            let (routed, call_id, entry) = call.complete(&result);
            audit.record(entry);
            respond(call_id, routed, result, |session, target, call_id, ()| {
                IpcResponse::Cancel {
                    session,
                    target,
                    call_id,
                }
            })
        }
        IpcRequest::ListJobs {} => {
            let call = CallAudit::begin(
                audit,
                purpose.as_ref(),
                route.consented(),
                call_ids,
                "mcp_remote_list_jobs_start",
                "mcp_remote_list_jobs",
                AuditArgs::new(),
            );
            let result = route.list_jobs(purpose.clone()).await;
            let (routed, call_id, entry) = call.complete(&result);
            audit.record(entry);
            respond(call_id, routed, result, |session, target, call_id, jobs| {
                IpcResponse::ListJobs {
                    session,
                    target,
                    call_id,
                    jobs,
                }
            })
        }
        IpcRequest::FsStat { path } => {
            let call = CallAudit::begin(
                audit,
                purpose.as_ref(),
                route.consented(),
                call_ids,
                "mcp_remote_fs_stat_start",
                "mcp_remote_fs_stat",
                AuditArgs::new().with("path", remote_path_value(&path)),
            );
            let result = route.fs_stat(purpose.clone(), path).await;
            let (routed, call_id, entry) = call.complete(&result);
            audit.record(entry);
            respond(
                call_id,
                routed,
                result,
                |session, target, call_id, report| IpcResponse::FsStat {
                    session,
                    target,
                    call_id,
                    report,
                },
            )
        }
        IpcRequest::FsList {
            path,
            continue_after,
        } => {
            let call = CallAudit::begin(
                audit,
                purpose.as_ref(),
                route.consented(),
                call_ids,
                "mcp_remote_fs_list_start",
                "mcp_remote_fs_list",
                AuditArgs::new().with("path", remote_path_value(&path)),
            );
            let result = route.fs_list(purpose.clone(), path, continue_after).await;
            let (routed, call_id, entry) = call.complete(&result);
            audit.record(entry);
            respond(call_id, routed, result, |session, target, call_id, page| {
                IpcResponse::FsList {
                    session,
                    target,
                    call_id,
                    page,
                }
            })
        }
        IpcRequest::MakeDir { path } => {
            let call = CallAudit::begin(
                audit,
                purpose.as_ref(),
                route.consented(),
                call_ids,
                "mcp_remote_make_dir_start",
                "mcp_remote_make_dir",
                AuditArgs::new().with("path", remote_path_value(&path)),
            );
            let result = route.make_dir(purpose.clone(), path).await;
            let (routed, call_id, entry) = call.complete(&result);
            audit.record(entry);
            respond(call_id, routed, result, |session, target, call_id, ()| {
                IpcResponse::MakeDir {
                    session,
                    target,
                    call_id,
                }
            })
        }
        IpcRequest::Move {
            from,
            to,
            overwrite,
        } => {
            // A move affects two canonical paths: `path` is the source, `to`
            // the destination (ADR-0008 Appendix A).
            let call = CallAudit::begin(
                audit,
                purpose.as_ref(),
                route.consented(),
                call_ids,
                "mcp_remote_move_start",
                "mcp_remote_move",
                AuditArgs::new()
                    .with("path", remote_path_value(&from))
                    .with("to", remote_path_value(&to)),
            );
            let result = route.fs_move(purpose.clone(), from, to, overwrite).await;
            let (routed, call_id, entry) = call.complete(&result);
            audit.record(entry);
            respond(call_id, routed, result, |session, target, call_id, ()| {
                IpcResponse::Move {
                    session,
                    target,
                    call_id,
                }
            })
        }
        IpcRequest::Delete { path } => {
            let call = CallAudit::begin(
                audit,
                purpose.as_ref(),
                route.consented(),
                call_ids,
                "mcp_remote_delete_start",
                "mcp_remote_delete",
                AuditArgs::new().with("path", remote_path_value(&path)),
            );
            let result = route.fs_delete(purpose.clone(), path).await;
            let (routed, call_id, entry) = call.complete(&result);
            audit.record(entry);
            respond(call_id, routed, result, |session, target, call_id, ()| {
                IpcResponse::Delete {
                    session,
                    target,
                    call_id,
                }
            })
        }
        IpcRequest::RemoveDir { path } => {
            let call = CallAudit::begin(
                audit,
                purpose.as_ref(),
                route.consented(),
                call_ids,
                "mcp_remote_remove_dir_start",
                "mcp_remote_remove_dir",
                AuditArgs::new().with("path", remote_path_value(&path)),
            );
            let result = route.remove_dir(purpose.clone(), path).await;
            let (routed, call_id, entry) = call.complete(&result);
            audit.record(entry);
            respond(call_id, routed, result, |session, target, call_id, ()| {
                IpcResponse::RemoveDir {
                    session,
                    target,
                    call_id,
                }
            })
        }
        IpcRequest::Upload { params } => {
            let call = CallAudit::begin(
                audit,
                purpose.as_ref(),
                route.consented(),
                call_ids,
                "mcp_remote_upload_start",
                "mcp_remote_upload",
                AuditArgs::new()
                    .with("path", remote_path_value(&params.dest))
                    .with(
                        "policy",
                        AuditValue::String(policy_label(params.cleanup).to_string()),
                    ),
            );
            let result = route.upload(purpose.clone(), params).await;
            let (routed, call_id, mut entry) = call.complete(&result);
            if let Ok(report) = &result {
                transfer_detail(
                    &mut entry,
                    report.remote_path.as_deref(),
                    report.len,
                    report.resumed_from,
                    report.restarted,
                    &report.digest,
                    report.error.as_ref().map(|error| error.message.as_str()),
                    report.outcome,
                );
            }
            audit.record(entry);
            respond(
                call_id,
                routed,
                result,
                |session, target, call_id, report| IpcResponse::Upload {
                    session,
                    target,
                    call_id,
                    report,
                },
            )
        }
        IpcRequest::Download { params } => {
            let call = CallAudit::begin(
                audit,
                purpose.as_ref(),
                route.consented(),
                call_ids,
                "mcp_remote_download_start",
                "mcp_remote_download",
                AuditArgs::new().with("path", remote_path_value(&params.source)),
            );
            let result = route.download(purpose.clone(), params).await;
            let (routed, call_id, mut entry) = call.complete(&result);
            if let Ok(report) = &result {
                transfer_detail(
                    &mut entry,
                    None,
                    report.len,
                    report.resumed_from,
                    report.restarted,
                    &report.digest,
                    report.error.as_ref().map(|error| error.message.as_str()),
                    report.outcome,
                );
            }
            audit.record(entry);
            respond(
                call_id,
                routed,
                result,
                |session, target, call_id, report| IpcResponse::Download {
                    session,
                    target,
                    call_id,
                    report,
                },
            )
        }
        IpcRequest::ListStaged {} => {
            let call = CallAudit::begin(
                audit,
                purpose.as_ref(),
                route.consented(),
                call_ids,
                "mcp_remote_list_staged_start",
                "mcp_remote_list_staged",
                AuditArgs::new(),
            );
            let result = route.list_staged(purpose.clone()).await;
            let (routed, call_id, entry) = call.complete(&result);
            audit.record(entry);
            respond(
                call_id,
                routed,
                result,
                |session, target, call_id, entries| IpcResponse::ListStaged {
                    session,
                    target,
                    call_id,
                    entries,
                },
            )
        }
        IpcRequest::Cleanup { trigger } => {
            let call = CallAudit::begin(
                audit,
                purpose.as_ref(),
                route.consented(),
                call_ids,
                "mcp_remote_cleanup_start",
                "mcp_remote_cleanup",
                AuditArgs::new().with(
                    "trigger",
                    AuditValue::String(trigger_label(trigger).to_string()),
                ),
            );
            let result = route.cleanup(purpose.clone(), trigger).await;
            let (routed, call_id, mut entry) = call.complete(&result);
            if let Ok(summary) = &result {
                entry
                    .args
                    .insert("removed", len_value(u64::from(summary.removed)));
                if !summary.failed.is_empty() {
                    // Cleanup failures are recorded honestly (REQ-F-019): the
                    // call answered, but artifacts remain on the target.
                    entry
                        .args
                        .insert("failed", len_value(summary.failed.len() as u64));
                    entry.outcome = Outcome::Failed;
                }
            }
            audit.record(entry);
            respond(
                call_id,
                routed,
                result,
                |session, target, call_id, report| IpcResponse::Cleanup {
                    session,
                    target,
                    call_id,
                    report,
                },
            )
        }
        // --- Visual desktop control (ADR-0015, ST-M005-004) ---
        // Each desktop op rides the same routed/audited path as any other tool
        // call: two records (`_start` before the target, completion after) under
        // one console-minted `call_id`, fail-closed when no session is consented.
        IpcRequest::CaptureDesktop { params } => {
            let call = CallAudit::begin(
                audit,
                purpose.as_ref(),
                route.consented(),
                call_ids,
                "mcp_remote_capture_desktop_start",
                "mcp_remote_capture_desktop",
                AuditArgs::new()
                    .with("display", len_value(u64::from(params.display)))
                    .with("format", AuditValue::String(format!("{:?}", params.format))),
            );
            let result = route.capture_desktop(purpose.clone(), params).await;
            let (routed, call_id, mut entry) = call.complete(&result);
            if let Ok(report) = &result {
                // Honest frame provenance: the desktop's liveness state, and
                // whether pixels rode inline or were staged above the inline
                // threshold. A locked/secure desktop yields metadata without
                // pixels — never a fabricated frame (ST-M005-004, REQ-N-008).
                entry.args.insert(
                    "state",
                    AuditValue::String(format!("{:?}", report.meta.state)),
                );
                if let Some(image) = &report.image {
                    entry
                        .args
                        .insert("image_bytes", len_value(image.len() as u64));
                }
                if let Some(path) = &report.staged {
                    entry.args.insert("staged", remote_path_value(path));
                }
            }
            audit.record(entry);
            respond(
                call_id,
                routed,
                result,
                |session, target, call_id, report| IpcResponse::DesktopFrame {
                    session,
                    target,
                    call_id,
                    report,
                },
            )
        }
        IpcRequest::DesktopPointer {
            action,
            button,
            x,
            y,
        } => {
            let call = CallAudit::begin(
                audit,
                purpose.as_ref(),
                route.consented(),
                call_ids,
                "mcp_remote_desktop_pointer_start",
                "mcp_remote_desktop_pointer",
                AuditArgs::new()
                    .with("action", AuditValue::String(format!("{action:?}")))
                    .with("button", AuditValue::String(format!("{button:?}")))
                    .with("x", AuditValue::Int(i64::from(x)))
                    .with("y", AuditValue::Int(i64::from(y))),
            );
            let result = route
                .desktop_pointer(purpose.clone(), action, button, x, y)
                .await;
            let (routed, call_id, entry) = call.complete(&result);
            audit.record(entry);
            respond(call_id, routed, result, |session, target, call_id, ()| {
                IpcResponse::DesktopAck {
                    session,
                    target,
                    call_id,
                }
            })
        }
        IpcRequest::DesktopScroll { x, y, dx, dy } => {
            let call = CallAudit::begin(
                audit,
                purpose.as_ref(),
                route.consented(),
                call_ids,
                "mcp_remote_desktop_scroll_start",
                "mcp_remote_desktop_scroll",
                AuditArgs::new()
                    .with("x", AuditValue::Int(i64::from(x)))
                    .with("y", AuditValue::Int(i64::from(y)))
                    .with("dx", AuditValue::Int(i64::from(dx)))
                    .with("dy", AuditValue::Int(i64::from(dy))),
            );
            let result = route.desktop_scroll(purpose.clone(), x, y, dx, dy).await;
            let (routed, call_id, entry) = call.complete(&result);
            audit.record(entry);
            respond(call_id, routed, result, |session, target, call_id, ()| {
                IpcResponse::DesktopAck {
                    session,
                    target,
                    call_id,
                }
            })
        }
        IpcRequest::DesktopKey { action, key } => {
            let call = CallAudit::begin(
                audit,
                purpose.as_ref(),
                route.consented(),
                call_ids,
                "mcp_remote_desktop_key_start",
                "mcp_remote_desktop_key",
                // A virtual-key name (e.g. "Enter", "F4") is an identifier, not
                // content, so it is recorded in the clear.
                AuditArgs::new()
                    .with("action", AuditValue::String(format!("{action:?}")))
                    .with("key", truncate_field(&key)),
            );
            let result = route.desktop_key(purpose.clone(), action, key).await;
            let (routed, call_id, entry) = call.complete(&result);
            audit.record(entry);
            respond(call_id, routed, result, |session, target, call_id, ()| {
                IpcResponse::DesktopAck {
                    session,
                    target,
                    call_id,
                }
            })
        }
        IpcRequest::DesktopType { text } => {
            let call = CallAudit::begin(
                audit,
                purpose.as_ref(),
                route.consented(),
                call_ids,
                "mcp_remote_desktop_type_start",
                "mcp_remote_desktop_type",
                // Typed text may carry secrets (passwords, tokens): only its byte
                // length is recorded, never the content (REQ-S-018).
                AuditArgs::new().with("text_len", len_value(text.len() as u64)),
            );
            let result = route.desktop_type(purpose.clone(), text).await;
            let (routed, call_id, entry) = call.complete(&result);
            audit.record(entry);
            respond(call_id, routed, result, |session, target, call_id, ()| {
                IpcResponse::DesktopAck {
                    session,
                    target,
                    call_id,
                }
            })
        }
        IpcRequest::DesktopHotkey { keys } => {
            let call = CallAudit::begin(
                audit,
                purpose.as_ref(),
                route.consented(),
                call_ids,
                "mcp_remote_desktop_hotkey_start",
                "mcp_remote_desktop_hotkey",
                // The chord is virtual-key names (e.g. "Ctrl+Alt+Delete"), an
                // identifier rather than content, so it is recorded in the clear.
                AuditArgs::new().with("keys", truncate_field(&keys.join("+"))),
            );
            let result = route.desktop_hotkey(purpose.clone(), keys).await;
            let (routed, call_id, entry) = call.complete(&result);
            audit.record(entry);
            respond(call_id, routed, result, |session, target, call_id, ()| {
                IpcResponse::DesktopAck {
                    session,
                    target,
                    call_id,
                }
            })
        }
        IpcRequest::DesktopWait { millis } => {
            let call = CallAudit::begin(
                audit,
                purpose.as_ref(),
                route.consented(),
                call_ids,
                "mcp_remote_desktop_wait_start",
                "mcp_remote_desktop_wait",
                AuditArgs::new().with("millis", len_value(millis)),
            );
            let result = route.desktop_wait(purpose.clone(), millis).await;
            let (routed, call_id, entry) = call.complete(&result);
            audit.record(entry);
            respond(call_id, routed, result, |session, target, call_id, ()| {
                IpcResponse::DesktopAck {
                    session,
                    target,
                    call_id,
                }
            })
        }
        IpcRequest::ClipboardGet {} => {
            let call = CallAudit::begin(
                audit,
                purpose.as_ref(),
                route.consented(),
                call_ids,
                "mcp_remote_clipboard_get_start",
                "mcp_remote_clipboard_get",
                AuditArgs::new(),
            );
            let result = route.clipboard_get(purpose.clone()).await;
            let (routed, call_id, mut entry) = call.complete(&result);
            if let Ok(text) = &result {
                // The clipboard may hold secrets: only the returned length is
                // recorded, never the content (REQ-S-018).
                let len = text.as_ref().map_or(0, String::len) as u64;
                entry.args.insert("text_len", len_value(len));
            }
            audit.record(entry);
            respond(call_id, routed, result, |session, target, call_id, text| {
                IpcResponse::ClipboardContents {
                    session,
                    target,
                    call_id,
                    text,
                }
            })
        }
        IpcRequest::ClipboardSet { text } => {
            let call = CallAudit::begin(
                audit,
                purpose.as_ref(),
                route.consented(),
                call_ids,
                "mcp_remote_clipboard_set_start",
                "mcp_remote_clipboard_set",
                // Replacement text may carry secrets: only its byte length is
                // recorded, never the content (REQ-S-018).
                AuditArgs::new().with("text_len", len_value(text.len() as u64)),
            );
            let result = route.clipboard_set(purpose.clone(), text).await;
            let (routed, call_id, entry) = call.complete(&result);
            audit.record(entry);
            respond(call_id, routed, result, |session, target, call_id, ()| {
                IpcResponse::DesktopAck {
                    session,
                    target,
                    call_id,
                }
            })
        }
        IpcRequest::TermOpen {
            shell,
            cols,
            rows,
            working_dir,
            env,
        } => {
            // Attributed to the technician `Terminal` actor, distinct from the
            // AI bridge's `Mcp` (REQ-S-023, ST-M003-009).
            let call = CallAudit::begin_as(
                Actor::Terminal,
                audit,
                route.consented(),
                call_ids,
                "terminal_open_start",
                "terminal_open",
                AuditArgs::new().with("size", AuditValue::String(format!("{cols}x{rows}"))),
            );
            let result = route
                .open_terminal(shell, cols, rows, working_dir, env)
                .await;
            let (routed, call_id, mut entry) = call.complete(&result);
            if let Ok(job) = &result {
                entry.args.insert("job_id", job_id_value(*job));
            }
            audit.record(entry);
            respond(call_id, routed, result, |session, target, call_id, job| {
                IpcResponse::TermOpened {
                    session,
                    target,
                    call_id,
                    job,
                }
            })
        }
        // These terminal messages are only valid *inside* an attached duplex
        // stream: `TermAttach` is intercepted before routing (it enters the
        // stream), and the input/resize/detach/close messages are handled by the
        // duplex reader. Seeing any of them in plain request/response mode is
        // client misuse — answered with an honest malformed error, never acted
        // on.
        IpcRequest::TermAttach { .. }
        | IpcRequest::TermInput { .. }
        | IpcRequest::TermResize { .. }
        | IpcRequest::TermDetach { .. }
        | IpcRequest::TermClose { .. } => IpcResponse::Error {
            error: IpcError::Malformed {
                message: "terminal stream message sent outside an attached terminal stream"
                    .to_string(),
            },
            call_id: 0,
        },
        // Unwrapped above; a wrapper inside a wrapper is refused there too.
        IpcRequest::WithPurpose { .. } => IpcResponse::Error {
            error: IpcError::Malformed {
                message: "a stated purpose may only wrap a routed tool call".to_string(),
            },
            call_id: 0,
        },
    }
}

/// Builds a routed response, or the fail-closed / mapped error. A successful
/// result with no consented identity is treated fail-closed (the session ended
/// between the snapshot and the reply).
fn respond<T>(
    call_id: u64,
    routed: Option<(SessionId, TargetIdentity)>,
    result: Result<T, TargetError>,
    make: impl FnOnce(SessionId, TargetIdentity, u64, T) -> IpcResponse,
) -> IpcResponse {
    // Every arm below belongs to a routed call for which `begin` already minted
    // and audited `call_id`, so the error response carries it too — a failed or
    // fail-closed call is still correlatable to its audit record (ST-M004-006).
    match result {
        Ok(value) => match routed {
            Some((session, target)) => make(session, target, call_id, value),
            None => IpcResponse::Error {
                error: IpcError::NoConsentedSession,
                call_id,
            },
        },
        Err(TargetError::NoConsentedSession) => IpcResponse::Error {
            error: IpcError::NoConsentedSession,
            call_id,
        },
        Err(err) => IpcResponse::Error {
            error: IpcError::Target(err),
            call_id,
        },
    }
}

/// The two-record audit envelope of one routed tool call (ST-M003-006).
///
/// [`CallAudit::begin`] mints the console-monotone `call_id`, stamps it into
/// the capture-time args, and records the `<action>_start` entry **before**
/// the call reaches the target; [`CallAudit::complete`] builds the completion
/// entry — same `call_id` and base args, honest outcome, measured duration,
/// and the failure reason on an error (secret-free: [`TargetError`]'s display
/// carries no key material or remote content).
struct CallAudit {
    /// Which local actor drove this call: `Mcp` for a bridge tool call,
    /// `Terminal` for a technician terminal action (REQ-S-023).
    actor: Actor,
    routed: Option<(SessionId, TargetIdentity)>,
    action: &'static str,
    args: AuditArgs,
    started: Instant,
    /// The console-minted, session-scoped monotone id shared by this call's
    /// start and completion audit records; also returned to the bridge in the
    /// routed IPC response so the tool result carries the same id (ST-M004-006).
    call_id: u64,
}

impl CallAudit {
    /// Begins an [`Actor::Mcp`]-attributed routed call (the MCP bridge surface).
    ///
    /// The AI client's stated purpose, when it supplied one, is recorded as
    /// the `purpose` argument of both the start and the completion record: it
    /// is the caller's own claim, kept verbatim beside the real arguments it
    /// describes and never in place of them (REQ-F-026, ST-M009-007). It lives
    /// in `args`, the record's caller-supplied argument map, so the ADR-0008
    /// record schema and its hash chain are unchanged.
    fn begin(
        audit: &dyn IpcAudit,
        purpose: Option<&Purpose>,
        routed: Option<(SessionId, TargetIdentity)>,
        call_ids: &AtomicU64,
        action_start: &'static str,
        action: &'static str,
        mut args: AuditArgs,
    ) -> Self {
        if let Some(purpose) = purpose {
            args.insert("purpose", truncate_field(purpose.as_str()));
        }
        Self::begin_as(
            Actor::Mcp,
            audit,
            routed,
            call_ids,
            action_start,
            action,
            args,
        )
    }

    /// Begins a routed call attributed to an explicit `actor`, so a technician
    /// terminal action is recorded as [`Actor::Terminal`], distinct from the AI
    /// bridge's [`Actor::Mcp`] (REQ-S-023, ST-M003-009).
    fn begin_as(
        actor: Actor,
        audit: &dyn IpcAudit,
        routed: Option<(SessionId, TargetIdentity)>,
        call_ids: &AtomicU64,
        action_start: &'static str,
        action: &'static str,
        mut args: AuditArgs,
    ) -> Self {
        let call_id = call_ids.fetch_add(1, Ordering::SeqCst);
        args.insert("call_id", len_value(call_id));
        let mut start = AuditEntry::event(
            actor,
            routed.as_ref().map(|(session, _)| *session),
            action_start,
            Outcome::Ok,
        );
        start.args = args.clone();
        audit.record(start);
        Self {
            actor,
            routed,
            action,
            args,
            started: Instant::now(),
            call_id,
        }
    }

    /// Builds the completion entry and hands back the routed snapshot and the
    /// `call_id` for the response envelope.
    fn complete<T>(
        self,
        result: &Result<T, TargetError>,
    ) -> (Option<(SessionId, TargetIdentity)>, u64, AuditEntry) {
        let outcome = match result {
            Ok(_) => Outcome::Ok,
            Err(TargetError::NoConsentedSession) => Outcome::Failed,
            // A lost session leaves the true outcome unknown — never a
            // fabricated success or failure (REQ-N-008).
            Err(TargetError::Disconnected) => Outcome::Unknown,
            Err(TargetError::Remote { .. } | TargetError::Unexpected) => Outcome::Failed,
        };
        let mut entry = AuditEntry::event(
            self.actor,
            self.routed.as_ref().map(|(session, _)| *session),
            self.action,
            outcome,
        );
        entry.args = self.args;
        entry.duration_ms = Some(elapsed_ms(self.started));
        if let Err(err) = result {
            entry
                .args
                .insert("reason", truncate_field(&err.to_string()));
        }
        (self.routed, self.call_id, entry)
    }
}

/// Fills a transfer entry with the ADR-0008 Appendix A detail from an honest
/// transfer report: canonical committed path (when the agent resolved one),
/// length, verified digest, resume/restart detail, failure reason, and the
/// mapped terminal outcome.
#[allow(clippy::too_many_arguments)]
fn transfer_detail(
    entry: &mut AuditEntry,
    canonical_path: Option<&str>,
    len: u64,
    resumed_from: u64,
    restarted: bool,
    digest: &[u8],
    failure: Option<&str>,
    outcome: JobOutcome,
) {
    if let Some(path) = canonical_path {
        // The agent's resolved canonical path supersedes the requested one.
        entry.args.insert("path", truncate_field(path));
    }
    entry.args.insert("len", len_value(len));
    if resumed_from > 0 {
        entry.args.insert("resumed_from", len_value(resumed_from));
    }
    if restarted {
        entry.args.insert("restarted", AuditValue::Bool(true));
    }
    if !digest.is_empty() {
        entry
            .digests
            .insert("sha256".to_string(), hex_lower(digest));
    }
    if let Some(reason) = failure {
        entry.args.insert("reason", truncate_field(reason));
    }
    entry.outcome = job_outcome_to_audit(outcome);
}

/// The audit value for a command: truncated plain text, or a `[REDACTED]`
/// marker with **no** correlation digest when the caller marked it sensitive
/// (a sensitive command may embed a low-entropy secret whose unsalted digest
/// would be brute-forceable, REQ-S-017/REQ-S-018).
fn command_value(command: &str, sensitive: bool) -> AuditValue {
    if sensitive {
        redact_no_digest("command marked sensitive by the caller", command.len())
    } else {
        truncate_field(command)
    }
}

/// The audit value for environment overrides: names in the clear, every value
/// replaced by a digest-free `[REDACTED]` marker (REQ-S-018 — values are never
/// logged verbatim, and no correlation digest is emitted because an
/// environment value's entropy is unknown).
///
/// The map is bounded against hostile same-user IPC input (REQ-N-006): names
/// are clamped to the protocol's per-key bound and at most
/// [`cretlink_protocol::limits::MAX_ENV_ENTRIES`] entries are recorded — an
/// over-limit request would be rejected by the actual execution anyway. The
/// overflow is marked explicitly under `[elided_env_entries]` so the record
/// stays honest about what it omitted.
fn env_value(env: &BTreeMap<String, String>) -> AuditValue {
    let mut map: BTreeMap<String, AuditValue> = env
        .iter()
        .take(cretlink_protocol::limits::MAX_ENV_ENTRIES)
        .map(|(name, value)| {
            let mut end = name.len().min(cretlink_protocol::limits::MAX_ENV_KEY);
            while end > 0 && !name.is_char_boundary(end) {
                end -= 1;
            }
            (
                name[..end].to_string(),
                redact_no_digest("environment override values are never logged", value.len()),
            )
        })
        .collect();
    let omitted = env
        .len()
        .saturating_sub(cretlink_protocol::limits::MAX_ENV_ENTRIES);
    if omitted > 0 {
        map.insert(
            "[elided_env_entries]".to_string(),
            AuditValue::Int(i64::try_from(omitted).unwrap_or(i64::MAX)),
        );
    }
    AuditValue::Map(map)
}

/// The Appendix A `path` value for a remote path argument. An absolute path is
/// recorded as its validated text; a staging name is prefixed so the two
/// authority domains stay distinguishable in the log.
fn remote_path_value(path: &RemotePath) -> AuditValue {
    match path {
        RemotePath::Absolute { path } => truncate_field(path),
        RemotePath::Staging { name } => truncate_field(&format!("staging:{name}")),
    }
}

/// The Appendix A `job_id` value.
fn job_id_value(job: JobId) -> AuditValue {
    AuditValue::Int(i64::try_from(job.0).unwrap_or(i64::MAX))
}

/// An audit integer for a byte length or count, saturating into `i64`.
fn len_value(len: u64) -> AuditValue {
    AuditValue::Int(i64::try_from(len).unwrap_or(i64::MAX))
}

/// Maps a job/transfer terminal outcome onto the audit vocabulary — honest,
/// never a fabricated success (REQ-N-008).
fn job_outcome_to_audit(outcome: JobOutcome) -> Outcome {
    match outcome {
        JobOutcome::Completed => Outcome::Ok,
        JobOutcome::Failed => Outcome::Failed,
        JobOutcome::Cancelled => Outcome::Cancelled,
        JobOutcome::TimedOut => Outcome::Timeout,
        JobOutcome::Unknown => Outcome::Unknown,
    }
}

/// The stable label for a staged artifact's cleanup policy (Appendix A
/// `policy`), mirroring the wire serialization.
fn policy_label(policy: CleanupPolicy) -> &'static str {
    match policy {
        CleanupPolicy::OnSessionEnd => "on_session_end",
        CleanupPolicy::OnShutdown => "on_shutdown",
        CleanupPolicy::Keep => "keep",
    }
}

/// The stable label for a cleanup trigger (Appendix A `trigger`), mirroring
/// the wire serialization.
fn trigger_label(trigger: CleanupTrigger) -> &'static str {
    match trigger {
        CleanupTrigger::SessionEnd => "session_end",
        CleanupTrigger::Shutdown => "shutdown",
        CleanupTrigger::All => "all",
    }
}

/// Wall-clock milliseconds since `started`, saturating into `u64`.
fn elapsed_ms(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)
}

/// Lowercase hex of raw digest bytes (the record's `digests` map form).
fn hex_lower(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        // Writing to a String is infallible; ignore the formatter Result.
        let _ = write!(out, "{byte:02x}");
    }
    out
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    /// A hostile same-user peer cannot inflate an audit record through the
    /// env map (REQ-N-006): entries are clamped to the protocol bound with an
    /// explicit elision marker, names to the per-key bound, and every value is
    /// only ever a short redaction marker.
    #[test]
    fn env_value_is_bounded_against_hostile_input() {
        let mut env = BTreeMap::new();
        for i in 0..(cretlink_protocol::limits::MAX_ENV_ENTRIES * 100) {
            env.insert(
                format!("NAME_{i:07}_{}", "k".repeat(1024)),
                "v".repeat(64 * 1024),
            );
        }
        let value = env_value(&env);
        let AuditValue::Map(map) = &value else {
            panic!("env audit value must be a map");
        };
        // At most the protocol entry bound plus the elision marker.
        assert!(map.len() <= cretlink_protocol::limits::MAX_ENV_ENTRIES + 1);
        let omitted = map
            .get("[elided_env_entries]")
            .expect("over-limit env must carry the elision marker");
        assert!(matches!(omitted, AuditValue::Int(n) if *n > 0));
        for (name, entry) in map {
            assert!(name.len() <= cretlink_protocol::limits::MAX_ENV_KEY);
            if name != "[elided_env_entries]" {
                assert!(
                    matches!(entry, AuditValue::Redacted { sha256: None, .. }),
                    "env values must be digest-free redaction markers"
                );
            }
        }
        // The whole serialized value stays small no matter the input size.
        let serialized = serde_json::to_string(&value).expect("serializes");
        assert!(
            serialized.len() < 16 * 1024,
            "env audit value must stay bounded, got {} bytes",
            serialized.len()
        );
    }

    /// A stated purpose arriving in the IPC wrapper is recorded verbatim as the
    /// `purpose` argument of both audit records of the call, beside the real
    /// arguments — here a fail-closed call, which is still audited
    /// (ST-M009-007, REQ-F-026).
    #[tokio::test]
    async fn purpose_is_recorded_in_both_audit_records() {
        let (audit, drain_rx) = ChannelAudit::bounded(16);
        let call_ids = AtomicU64::new(1);
        let request = IpcRequest::WithPurpose {
            purpose: Purpose::new("Looking for the printer log").unwrap(),
            call: Box::new(IpcRequest::FsStat {
                path: RemotePath::Absolute {
                    path: "C:\\Logs\\printer.log".to_string(),
                },
            }),
        };
        let response = route_request(&ClosedRoute, &audit, &call_ids, request).await;
        assert!(
            matches!(
                response,
                IpcResponse::Error {
                    error: IpcError::NoConsentedSession,
                    ..
                }
            ),
            "{response:?}"
        );
        for expected in ["mcp_remote_fs_stat_start", "mcp_remote_fs_stat"] {
            let entry = drain_rx.try_recv().unwrap();
            assert_eq!(entry.action, expected);
            assert_eq!(
                entry.args.0.get("purpose"),
                Some(&truncate_field("Looking for the printer log")),
                "{expected} must hold the stated purpose"
            );
            assert!(entry.args.0.contains_key("path"), "the real path stays");
        }
    }

    /// A bare request (an older bridge) carries no purpose and is audited
    /// without one; nothing is invented (ST-M009-007, REQ-N-012).
    #[tokio::test]
    async fn bare_request_is_audited_without_a_purpose() {
        let (audit, drain_rx) = ChannelAudit::bounded(16);
        let call_ids = AtomicU64::new(1);
        let _ = route_request(&ClosedRoute, &audit, &call_ids, IpcRequest::ListJobs {}).await;
        for _ in 0..2 {
            let entry = drain_rx.try_recv().unwrap();
            assert!(!entry.args.0.contains_key("purpose"), "{entry:?}");
        }
    }

    /// The purpose wrapper may only carry a routed tool call: wrapped around a
    /// terminal message, a handshake, or another wrapper it is refused as
    /// malformed, nothing is routed, and nothing is audited (ST-M009-007).
    #[tokio::test]
    async fn purpose_wrapper_around_a_non_tool_request_is_refused() {
        let purpose = Purpose::new("Opening a terminal").unwrap();
        for call in [
            IpcRequest::TermOpen {
                shell: Shell::Cmd,
                cols: 80,
                rows: 24,
                working_dir: None,
                env: BTreeMap::new(),
            },
            IpcRequest::Hello {
                protocol: IPC_PROTOCOL,
            },
            IpcRequest::WithPurpose {
                purpose: purpose.clone(),
                call: Box::new(IpcRequest::ListJobs {}),
            },
        ] {
            let (audit, drain_rx) = ChannelAudit::bounded(16);
            let call_ids = AtomicU64::new(1);
            let request = IpcRequest::WithPurpose {
                purpose: purpose.clone(),
                call: Box::new(call),
            };
            let response = route_request(&ClosedRoute, &audit, &call_ids, request).await;
            assert!(
                matches!(
                    response,
                    IpcResponse::Error {
                        error: IpcError::Malformed { .. },
                        ..
                    }
                ),
                "{response:?}"
            );
            assert!(drain_rx.try_recv().is_err(), "nothing may be audited");
        }
    }

    /// The live activity echo (REQ-F-017) forwards bridge-originated entries
    /// to the CLI renderer and nothing else, without touching the drain
    /// channel's delivery.
    #[tokio::test]
    async fn live_echo_forwards_only_bridge_entries() {
        let (audit, drain_rx) = ChannelAudit::bounded(16);
        let (live_tx, mut live_rx) = tokio::sync::mpsc::channel(16);
        audit.set_live_sender(live_tx);

        audit.record_entry(AuditEntry::event(
            Actor::Mcp,
            None,
            "mcp_remote_exec_start",
            Outcome::Ok,
        ));
        audit.record_entry(AuditEntry::event(
            Actor::Console,
            None,
            "console_exec_start",
            Outcome::Ok,
        ));

        // The drain sees both, in order.
        assert_eq!(drain_rx.try_recv().unwrap().action, "mcp_remote_exec_start");
        assert_eq!(drain_rx.try_recv().unwrap().action, "console_exec_start");
        // The live echo sees only the bridge entry.
        assert_eq!(live_rx.try_recv().unwrap().action, "mcp_remote_exec_start");
        assert!(
            live_rx.try_recv().is_err(),
            "console entries are not echoed"
        );
    }

    /// The live echo is bounded and non-blocking: once its channel is full,
    /// further echo lines are dropped rather than growing the buffer
    /// (REQ-N-006), and the durable drain still receives every entry.
    #[tokio::test]
    async fn live_echo_drops_when_full_but_drain_keeps_all() {
        let (audit, drain_rx) = ChannelAudit::bounded(64);
        // A one-slot echo channel with no active consumer: the second bridge
        // entry cannot be enqueued and must be dropped, never buffered.
        let (live_tx, mut live_rx) = tokio::sync::mpsc::channel(1);
        audit.set_live_sender(live_tx);

        for _ in 0..8 {
            audit.record_entry(AuditEntry::event(
                Actor::Mcp,
                None,
                "mcp_remote_exec_start",
                Outcome::Ok,
            ));
        }

        // The durable drain recorded all 8 (nothing dropped there).
        let mut drained = 0;
        while drain_rx.try_recv().is_ok() {
            drained += 1;
        }
        assert_eq!(drained, 8);
        assert_eq!(audit.dropped(), 0, "the durable channel dropped nothing");

        // The bounded echo holds at most its capacity; the rest were dropped.
        let mut echoed = 0;
        while live_rx.try_recv().is_ok() {
            echoed += 1;
        }
        assert_eq!(echoed, 1, "the echo is capped at its channel capacity");
    }
}
