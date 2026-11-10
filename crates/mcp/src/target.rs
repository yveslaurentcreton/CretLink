//! The [`TargetSession`] abstraction the MCP bridge is built on.
//!
//! [`crate::McpBridge`] never talks to the wire protocol directly. It calls an
//! async [`TargetSession`] that stands for one explicitly selected remote
//! target (REQ-F-013, REQ-F-016). The console wires a real session client to
//! this trait; tests provide deterministic fakes. Keeping the bridge behind
//! this seam means the MCP surface can be exercised without a live agent, and
//! every tool result can carry the session identity and [`TargetIdentity`] the
//! trait exposes.

use std::collections::BTreeMap;

use cretlink_protocol::{
    B64Bytes, CleanupFailure, CleanupPolicy, CleanupTrigger, DesktopFrameMeta, ErrorCode,
    ErrorStage, FileEntry, ImageFormat, JobId, JobOutcome, JobSummary, KeyAction, PointerAction,
    PointerButton, Purpose, RemotePath, SessionId, Shell, StagedEntry, TargetIdentity,
};
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Serde helper: raw bytes serialize as a standard base64 string so the
/// bridge-console IPC contract stays compact JSON (a plain `Vec<u8>` would
/// serialize as a numeric array, roughly quadrupling large output). Mirrors
/// [`cretlink_protocol::B64Bytes`] but applies to bare `Vec<u8>` fields.
mod bytes_b64 {
    use base64::Engine as _;
    use base64::engine::general_purpose::STANDARD as BASE64;
    use serde::{Deserialize, Deserializer, Serializer};

    pub(super) fn serialize<S: Serializer>(bytes: &[u8], serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&BASE64.encode(bytes))
    }

    pub(super) fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Vec<u8>, D::Error> {
        let text = String::deserialize(deserializer)?;
        BASE64
            .decode(text.as_bytes())
            .map_err(serde::de::Error::custom)
    }
}

/// Parameters for a single remote command execution.
///
/// Mirrors [`cretlink_protocol::Operation::Exec`] but is decoupled from the
/// wire type so the bridge and its backends share a stable, validated shape.
/// It derives serde so it can travel over the bridge-console IPC contract
/// (ADR-0004).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExecParams {
    /// Which shell runs the command.
    pub shell: Shell,
    /// The command text (already length-checked by the caller).
    pub command: String,
    /// Optional working directory.
    pub working_dir: Option<String>,
    /// Extra environment variables.
    pub env: BTreeMap<String, String>,
    /// Optional deadline in milliseconds; expiry yields
    /// [`JobOutcome::TimedOut`].
    pub timeout_ms: Option<u64>,
    /// Whether the caller marked the command text sensitive (REQ-S-018): the
    /// console then records the command in the audit log as a `[REDACTED]`
    /// marker (no correlation digest) while keeping the action, job id, and
    /// outcome. Additive within the IPC contract major (ADR-0004): absent on
    /// the wire when `false`, so an older peer's `deny_unknown_fields` decode
    /// is unaffected until the flag is actually used.
    #[serde(default, skip_serializing_if = "is_false")]
    pub sensitive: bool,
}

/// Serde helper: `true` when a defaulted `bool` field can be omitted from the
/// wire form (keeps additive flags invisible to older peers until used).
#[allow(clippy::trivially_copy_pass_by_ref)]
pub(crate) fn is_false(value: &bool) -> bool {
    !*value
}

/// The collected result of running an [`ExecParams`] to completion.
///
/// Output is captured up to a caller-supplied byte cap; `truncated` records
/// honestly whether the cap (or a dropped stream) hid later bytes (REQ-N-008).
/// The byte streams serialize as base64 so the report travels compactly over
/// the bridge-console IPC contract (ADR-0004).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExecReport {
    /// The job the agent assigned.
    pub job: JobId,
    /// The effective **remote** working directory the job received, as
    /// reported by the agent's job-start result (ST-M003-003, REQ-F-009).
    pub working_dir: String,
    /// Captured standard output bytes (raw; VT sequences passed through).
    #[serde(with = "bytes_b64")]
    pub stdout: Vec<u8>,
    /// Captured standard error bytes (raw).
    #[serde(with = "bytes_b64")]
    pub stderr: Vec<u8>,
    /// Whether output was truncated at the cap or lost to a dropped stream.
    pub truncated: bool,
    /// The process exit code, when one exists.
    pub exit_code: Option<i32>,
    /// Wall-clock job duration in milliseconds.
    pub duration_ms: u64,
    /// The honest job outcome (REQ-N-008).
    pub outcome: JobOutcome,
}

/// The result of a [`TargetSession::system_info`] call.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SystemInfoReport {
    /// The target's identity (REQ-F-016).
    pub target: TargetIdentity,
    /// Whether the target reports a pending reboot.
    pub reboot_pending: bool,
}

/// The result of a [`TargetSession::fs_stat`] call.
///
/// A thin projection of [`cretlink_protocol::OpResult::PathStat`]: the entry
/// when the path exists, plus the pending part length when a staging write is in
/// progress (used by resume). No file content is ever returned (REQ-N-007).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StatReport {
    /// The entry when the path exists, else `None`.
    pub entry: Option<FileEntry>,
    /// The bytes already written to a pending staging part, when one exists.
    pub partial_len: Option<u64>,
}

/// One page of a directory listing (a projection of
/// [`cretlink_protocol::OpResult::DirEntries`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DirPage {
    /// The entries on this page (bounded by the protocol page size).
    pub entries: Vec<FileEntry>,
    /// Whether this page is the end of the listing; `false` means paginate with
    /// the last entry's name as `continue_after`.
    pub end: bool,
}

/// The result of a [`TargetSession::cleanup`] call: how many staged artifacts
/// were removed and any honest per-artifact failures (REQ-N-008).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CleanupSummary {
    /// Artifacts removed (or already gone) and dropped from the manifest.
    pub removed: u32,
    /// Artifacts that could not be removed; their records are kept for retry.
    pub failed: Vec<CleanupFailure>,
}

/// Parameters for a single desktop capture (ADR-0015, ST-M005-004).
///
/// Mirrors the capture hints of [`cretlink_protocol::Operation::DesktopCapture`]
/// but is decoupled from the wire type so the bridge and its backends share a
/// stable, validated shape. The optional downscale (`max_width`/`max_height`)
/// and `quality` hints are bounds-checked against `cretlink_protocol::limits`
/// before the request reaches the live session, so a malformed request fails on
/// the call and never touches the target (REQ-N-012).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CaptureParams {
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

/// The result of a [`TargetSession::capture_desktop`] call.
///
/// A projection of [`cretlink_protocol::OpResult::DesktopFrame`]: the honest
/// frame [`DesktopFrameMeta`] (live state, geometry, and capture timestamp) is
/// always present, but a pixel payload is present only when the desktop was
/// [`cretlink_protocol::DesktopState::Active`]. Exactly one of [`Self::image`]
/// (inline when it fit the inline bound) or [`Self::staged`] (a staged-artifact
/// reference read back over the ordinary file path) carries it; both are `None`
/// for a locked, switched, disconnected, or secure desktop, which is never
/// dressed up with a stale frame (ADR-0015, REQ-N-008). The frame timestamp and
/// state are what a caller uses to reason about staleness and a changed desktop.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CaptureReport {
    /// The frame's identity, geometry, live state, and capture timestamp.
    pub meta: DesktopFrameMeta,
    /// The inline encoded image (base64 on the wire), when it fit the inline
    /// bound.
    pub image: Option<B64Bytes>,
    /// The staging path of the encoded frame, when it exceeded the inline bound.
    pub staged: Option<RemotePath>,
}

/// The error that ended a failed transfer (secret-free): the agent's report of
/// an operation it refused, or an error on the technician's own file.
///
/// Decoupled from the session crate's transfer error so the bridge, target
/// trait, and IPC contract share one stable shape without the bridge depending
/// on the session runtime.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TransferFailure {
    /// Machine-readable category: the one the agent returned, or the cause of
    /// an error on the technician's own file.
    pub code: ErrorCode,
    /// Human-readable, secret-free description. In the `local` stage it names
    /// the local path and the reason and says that the file is on the
    /// technician computer.
    pub message: String,
    /// The transfer stage the failure occurred in: a stage on the target, or
    /// `local` for an error on the technician's own file (an upload's source, a
    /// download's destination), which is not a lost session and leaves the
    /// session usable (ST-M003-005).
    pub stage: String,
}

/// Parameters for a single file upload into target staging.
///
/// The destination is always a [`RemotePath::Staging`] name so the upload lands
/// in the tracked, cleanup-eligible staging area with a required origin label
/// (REQ-S-017, REQ-F-019). Absolute placement is composed by a later move.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UploadParams {
    /// The local file on the technician machine to read.
    pub local_path: String,
    /// The staging destination on the target.
    pub dest: RemotePath,
    /// The technician-side origin label bound into the staging manifest.
    pub source_label: String,
    /// The per-artifact cleanup policy recorded at commit.
    pub cleanup: CleanupPolicy,
    /// Whether an existing staged artifact of that name may be replaced.
    pub overwrite: bool,
}

/// Parameters for a single file download from the target.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DownloadParams {
    /// The source path on the target to read.
    pub source: RemotePath,
    /// The local path on the technician machine to create.
    pub local_path: String,
}

/// The collected result of an [`TargetSession::upload`] call.
///
/// A completion is structurally verified by the agent's `FsCommit` (length and
/// SHA-256) before the atomic rename, so `Completed` always carries the resolved
/// absolute `remote_path` and the verified `digest`; content is never returned
/// (REQ-N-007, REQ-S-018).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UploadReport {
    /// Terminal outcome: `Completed`, `Failed`, or `Cancelled`.
    pub outcome: JobOutcome,
    /// The agent's resolved absolute committed path on `Completed`.
    pub remote_path: Option<String>,
    /// Total bytes in the artifact.
    pub len: u64,
    /// Bytes confirmed transferred this run (excludes a resumed prefix).
    pub sent: u64,
    /// The verified SHA-256 digest on `Completed`; empty otherwise.
    #[serde(with = "bytes_b64")]
    pub digest: Vec<u8>,
    /// Bytes already present that a verified resume reused.
    pub resumed_from: u64,
    /// Whether a mismatch forced an abort-and-restart from zero.
    pub restarted: bool,
    /// The error that ended a `Failed` transfer, if any.
    pub error: Option<TransferFailure>,
}

/// The collected result of a [`TargetSession::download`] call.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DownloadReport {
    /// Terminal outcome: `Completed`, `Failed`, or `Cancelled`.
    pub outcome: JobOutcome,
    /// The local path the download was written to.
    pub local_path: String,
    /// Total bytes in the artifact.
    pub len: u64,
    /// Bytes confirmed transferred this run (excludes a resumed prefix).
    pub sent: u64,
    /// The verified SHA-256 digest on `Completed`; empty otherwise.
    #[serde(with = "bytes_b64")]
    pub digest: Vec<u8>,
    /// Bytes already present that a verified resume reused.
    pub resumed_from: u64,
    /// Whether a mismatch forced a restart from zero.
    pub restarted: bool,
    /// The error that ended a `Failed` transfer, if any.
    pub error: Option<TransferFailure>,
}

/// A successful [`TargetSession`] result together with the routed identity that
/// produced it.
///
/// The identity is captured from the **same** console reply that produced the
/// result, never from a cache read before the call. The MCP bridge builds every
/// success envelope from this, so a result can never be attributed to a
/// different session or machine — for example a bridge that connected before
/// consent, or one that outlived a console and reconnected to a later session
/// (REQ-F-016).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Routed<T> {
    /// The session the console actually ran the call on.
    pub session: SessionId,
    /// The target identity the console reported for this call.
    pub target: TargetIdentity,
    /// The console-minted, session-scoped monotone `call_id` this routed call
    /// was audited under (ADR-0008 Appendix A). The bridge returns it in the
    /// structured tool result so the AI transcript maps one-to-one onto the
    /// audit trail — the same `call_id` in the record and in the reply
    /// (ST-M004-006). `0` marks a reply from an older console that predates the
    /// field (the audit trail is still authoritative).
    pub call_id: u64,
    /// The operation payload.
    pub value: T,
}

/// A failure surfaced by a [`TargetSession`] operation.
///
/// Distinguishes a remote-reported protocol error from a lost session (whose
/// outcome is unknown) so the bridge can report each honestly rather than
/// collapsing them into a generic failure (REQ-N-008).
#[derive(Debug, Clone, PartialEq, Eq, Error, Serialize, Deserialize)]
#[serde(tag = "error", rename_all = "snake_case", deny_unknown_fields)]
pub enum TargetError {
    /// The remote agent returned a wire `Error` message.
    #[error("remote agent error [{code:?} / {stage:?}]: {message}")]
    Remote {
        /// Machine-readable category.
        code: ErrorCode,
        /// Human-readable description.
        message: String,
        /// Which processing stage failed.
        stage: ErrorStage,
    },
    /// The session ended before the operation completed; the true outcome
    /// could not be determined.
    #[error("session disconnected; remote outcome unknown")]
    Disconnected,
    /// The agent replied with a message that did not fit the request.
    #[error("unexpected response from remote agent")]
    Unexpected,
    /// The console has no active, consented session bound to a remote target,
    /// so the tool call fails closed and nothing runs (REQ-S-009, ST-M004-004).
    #[error("no consented active session; the console has no target under attended full control")]
    NoConsentedSession,
}

impl TargetError {
    /// A short, stable machine token for this error category, embedded in
    /// tool error results so callers can branch without string matching.
    #[must_use]
    pub const fn kind(&self) -> &'static str {
        match self {
            Self::Remote { .. } => "remote",
            Self::Disconnected => "disconnected",
            Self::Unexpected => "unexpected",
            Self::NoConsentedSession => "no_consented_session",
        }
    }
}

/// A [`TargetSession`] failure together with the console-minted `call_id` of the
/// routed call it belongs to, when the console reported one (ST-M004-006).
///
/// This is a **bridge-local** wrapper: unlike [`TargetError`] it never travels
/// the wire, so `call_id` — a console/audit concern — stays out of the
/// wire-serialized error vocabulary. The bridge returns it in the error tool
/// result so a failed or fail-closed call still correlates one-to-one with its
/// audit record. A lost connection or an in-process adapter that mints no id
/// carries `None`.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("{error}")]
pub struct CallError {
    /// The honest remote/session failure.
    pub error: TargetError,
    /// The console-minted `call_id`, when one was reported for this call.
    pub call_id: Option<u64>,
}

impl CallError {
    /// Pairs an error with the routed call's `call_id`.
    #[must_use]
    pub fn new(error: TargetError, call_id: Option<u64>) -> Self {
        Self { error, call_id }
    }

    /// The stable machine token for the underlying error category.
    #[must_use]
    pub const fn kind(&self) -> &'static str {
        self.error.kind()
    }
}

impl From<TargetError> for CallError {
    /// A failure with no associated `call_id` (a lost connection or a local
    /// adapter that routes through no console audit).
    fn from(error: TargetError) -> Self {
        Self {
            error,
            call_id: None,
        }
    }
}

/// One explicitly selected remote target the MCP bridge acts on.
///
/// Every method is bound to the same target for the lifetime of the trait
/// object. Each successful call returns a [`Routed`] carrying the session and
/// [`TargetIdentity`] the console reported **for that call**, so the bridge
/// labels every result with the identity that actually produced it rather than
/// a cached one (REQ-F-016). [`Self::session_id`] and [`Self::target`] expose a
/// best-effort last-known identity used only to label an *error* envelope (a
/// fail-closed or lost-link result that carries no routed identity of its own).
///
/// Every operation takes the AI client's stated [`Purpose`] (REQ-F-026,
/// ST-M009-007): the bridge requires and validates it on every tool call before
/// anything is sent, and an implementation forwards it to the console so it is
/// audited and shown to the customer beside the real operation. It is the
/// caller's claim, never authority.
///
/// Implementations must treat all remote output as untrusted input and must
/// never fabricate a success for a lost session.
#[async_trait::async_trait]
pub trait TargetSession: Send + Sync + 'static {
    /// Records the AI client's self-reported `clientInfo.name` from the MCP
    /// `initialize` request, so the console can label the client's activity
    /// (ST-M009-011). A label only, never authority, and never sent to the
    /// customer computer. The default ignores it.
    fn set_client_name(&self, _name: &str) {}

    /// The last-known session id, for labelling an error envelope only.
    fn session_id(&self) -> SessionId;

    /// The last-known target identity, for labelling an error envelope only.
    fn target(&self) -> TargetIdentity;

    /// Report the target's identity and basic system state.
    async fn system_info(&self, purpose: Purpose) -> Result<Routed<SystemInfoReport>, CallError>;

    /// Run a command to completion, capturing output up to `output_cap`
    /// bytes across both streams.
    async fn exec(
        &self,
        purpose: Purpose,
        params: ExecParams,
        output_cap: usize,
    ) -> Result<Routed<ExecReport>, CallError>;

    /// Feed bytes to a running job's standard input. `sensitive` marks the
    /// input as sensitive for the console's audit trail (REQ-S-018); stdin
    /// content is structurally never logged, so the flag travels for schema
    /// symmetry with `exec` and for any future output-excerpt logging.
    async fn write_stdin(
        &self,
        purpose: Purpose,
        job: JobId,
        data: Vec<u8>,
        sensitive: bool,
    ) -> Result<Routed<()>, CallError>;

    /// Request cancellation of a running job.
    async fn cancel(&self, purpose: Purpose, job: JobId) -> Result<Routed<()>, CallError>;

    /// List the jobs the target currently tracks.
    async fn list_jobs(&self, purpose: Purpose) -> Result<Routed<Vec<JobSummary>>, CallError>;

    /// Stat a path on the target. Returns the entry if it exists and any pending
    /// staging part length; never returns file content (REQ-N-007).
    async fn fs_stat(
        &self,
        purpose: Purpose,
        path: RemotePath,
    ) -> Result<Routed<StatReport>, CallError>;

    /// List one page of a directory on the target, continuing after the given
    /// entry name for pagination.
    async fn fs_list(
        &self,
        purpose: Purpose,
        path: RemotePath,
        continue_after: Option<String>,
    ) -> Result<Routed<DirPage>, CallError>;

    /// Create a directory on the target.
    async fn make_dir(&self, purpose: Purpose, path: RemotePath) -> Result<Routed<()>, CallError>;

    /// Move (rename) a path on the target. Same-volume only; `overwrite` decides
    /// whether an existing destination file may be replaced.
    async fn fs_move(
        &self,
        purpose: Purpose,
        from: RemotePath,
        to: RemotePath,
        overwrite: bool,
    ) -> Result<Routed<()>, CallError>;

    /// Delete a file or reparse point on the target (never a directory).
    async fn fs_delete(&self, purpose: Purpose, path: RemotePath) -> Result<Routed<()>, CallError>;

    /// Remove an empty directory on the target.
    async fn remove_dir(&self, purpose: Purpose, path: RemotePath)
    -> Result<Routed<()>, CallError>;

    /// Upload a local file into target staging and commit it verified, driven by
    /// the console transfer engine over bounded chunk operations. The completion
    /// carries only the canonical path, length, digest, and outcome — never file
    /// content (REQ-N-007, REQ-S-018).
    async fn upload(
        &self,
        purpose: Purpose,
        params: UploadParams,
    ) -> Result<Routed<UploadReport>, CallError>;

    /// Download a target file to a local path, verified by whole-file digest.
    async fn download(
        &self,
        purpose: Purpose,
        params: DownloadParams,
    ) -> Result<Routed<DownloadReport>, CallError>;

    /// List the staged artifacts the target currently tracks (REQ-F-019).
    async fn list_staged(&self, purpose: Purpose) -> Result<Routed<Vec<StagedEntry>>, CallError>;

    /// Run a staging cleanup for a trigger, reporting honest per-artifact
    /// outcomes (REQ-N-008).
    async fn cleanup(
        &self,
        purpose: Purpose,
        trigger: CleanupTrigger,
    ) -> Result<Routed<CleanupSummary>, CallError>;

    // --- Visual desktop surface (ADR-0015, ST-M005-004) ---
    //
    // These generic computer primitives replace task-specific repair tools: the
    // AI observes with `capture_desktop` and acts with the pointer, keyboard, and
    // clipboard calls below. The agent only serves them where it advertised
    // `desktop.v1`; otherwise they surface a mapped `Unsupported` remote error
    // rather than a fabricated frame or a silently dropped input (REQ-N-008).

    /// Capture the target's current interactive desktop, returning an honest
    /// frame — its state and geometry always, with a pixel payload only when the
    /// desktop is active (ADR-0015).
    async fn capture_desktop(
        &self,
        purpose: Purpose,
        params: CaptureParams,
    ) -> Result<Routed<CaptureReport>, CallError>;

    /// Inject a pointer action at an absolute virtual-desktop coordinate.
    async fn desktop_pointer(
        &self,
        purpose: Purpose,
        action: PointerAction,
        button: PointerButton,
        x: i32,
        y: i32,
    ) -> Result<Routed<()>, CallError>;

    /// Inject a scroll (wheel) action at an absolute coordinate.
    async fn desktop_scroll(
        &self,
        purpose: Purpose,
        x: i32,
        y: i32,
        dx: i32,
        dy: i32,
    ) -> Result<Routed<()>, CallError>;

    /// Inject a single key action naming a virtual key. Only *sends* input; no
    /// keylogging primitive exists (REQ-S-013).
    async fn desktop_key(
        &self,
        purpose: Purpose,
        action: KeyAction,
        key: String,
    ) -> Result<Routed<()>, CallError>;

    /// Type a Unicode string as a sequence of key events.
    async fn desktop_type(&self, purpose: Purpose, text: String) -> Result<Routed<()>, CallError>;

    /// Send a chord of named virtual keys as a single hotkey press.
    async fn desktop_hotkey(
        &self,
        purpose: Purpose,
        keys: Vec<String>,
    ) -> Result<Routed<()>, CallError>;

    /// Pace the input pipeline by a bounded number of milliseconds.
    async fn desktop_wait(&self, purpose: Purpose, millis: u64) -> Result<Routed<()>, CallError>;

    /// Read the remote clipboard's text, or `None` when it holds no text. The
    /// clipboard exchange is explicit-only and audited (REQ-S-015).
    async fn clipboard_get(&self, purpose: Purpose) -> Result<Routed<Option<String>>, CallError>;

    /// Replace the remote clipboard's text. CretLink never places session secrets
    /// or provider credentials on the remote clipboard (REQ-F-015, REQ-S-015).
    async fn clipboard_set(&self, purpose: Purpose, text: String) -> Result<Routed<()>, CallError>;
}
