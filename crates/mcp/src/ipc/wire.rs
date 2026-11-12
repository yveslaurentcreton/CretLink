//! The versioned, length-prefixed bridge-console IPC contract (ADR-0004,
//! REQ-N-012).
//!
//! The MCP bridge (`cretlink-cli mcp`) holds no session authority; it translates
//! MCP tool calls into these [`IpcRequest`]s and forwards them to the console
//! over same-user local IPC ([`super::transport`]). The console — the sole
//! routing authority — answers with an [`IpcResponse`] carrying the routed
//! session identity and target on every result (REQ-F-016), or a fail-closed
//! [`IpcError::NoConsentedSession`] when no consented active session exists
//! (REQ-S-009, ST-M004-004).
//!
//! # Framing and bounds
//!
//! Every message is a 4-byte little-endian `u32` length prefix followed by
//! exactly that many bytes of `serde_json`. The declared length is checked
//! against [`MAX_IPC_FRAME_BYTES`] **before** the payload buffer is allocated,
//! so a hostile or corrupt prefix fails closed without committing memory,
//! mirroring the wire-protocol discipline in
//! [`cretlink_protocol::limits`]. Any decode error, oversize, or EOF fails the
//! connection closed — an in-flight tool call then surfaces as an honest
//! [`crate::TargetError`], never a fabricated success (REQ-N-008).

use std::collections::BTreeMap;

use cretlink_protocol::{
    B64Bytes, CleanupTrigger, JobId, JobOutcome, JobSummary, KeyAction, PROTOCOL_V1, PointerAction,
    PointerButton, ProtocolVersion, Purpose, RemotePath, SessionId, Shell, StagedEntry,
    TargetIdentity,
};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::target::{
    CaptureParams, CaptureReport, CleanupSummary, DirPage, DownloadParams, DownloadReport,
    ExecParams, ExecReport, StatReport, SystemInfoReport, TargetError, UploadParams, UploadReport,
};

/// The IPC contract version. It reuses the wire [`ProtocolVersion`] scheme
/// (equal majors are compatible) so a bridge and console built from different
/// revisions detect an incompatible contract during the handshake instead of
/// misparsing frames (REQ-N-012).
///
/// Minor `3` adds the additive `call_id` on every routed [`IpcResponse`], so
/// the bridge can return the console-minted audit `call_id` in the structured
/// tool result (ST-M004-006). The field is `#[serde(default)]`, so a reply from
/// an older-minor console still decodes.
///
/// Minor `4` adds the additive interactive-terminal message set —
/// [`IpcRequest::TermOpen`]/`TermAttach`/`TermInput`/`TermResize`/`TermDetach`/
/// `TermClose`, the [`IpcResponse::TermOpened`]/`TermAttached` replies, and the
/// server-pushed [`TermFrame`] stream (ST-M003-009, ADR-0013). A successful
/// `TermAttach` switches the connection into a bounded duplex stream mode, a
/// deliberate and well-scoped extension of ADR-0004's request/response
/// turn-taking. It rides the same length-prefix framing and the same
/// major-compatible negotiation, so an older-minor bridge that never sends a
/// terminal message is unaffected (REQ-N-012). Because [`IPC_PROTOCOL`] tracks
/// the wire [`PROTOCOL_V1`], a terminal client should confirm the console's
/// advertised minor is `>= 4` before attaching.
///
/// Minor `6` adds the additive visual-desktop message set —
/// [`IpcRequest::CaptureDesktop`]/`DesktopPointer`/`DesktopScroll`/`DesktopKey`/
/// `DesktopType`/`DesktopHotkey`/`DesktopWait`/`ClipboardGet`/`ClipboardSet` and
/// the [`IpcResponse::DesktopFrame`]/`DesktopAck`/`ClipboardContents` replies
/// (ADR-0015, ST-M005-004). These are ordinary request/response calls carrying
/// the routed identity and `call_id` like every other; a bridge that never
/// issues a desktop request is unaffected. The agent serves them only where it
/// advertised `desktop.v1`, so an unequipped target answers a mapped
/// `Unsupported` remote error rather than a fabricated frame (REQ-N-008).
///
/// Minor `9` adds the additive [`IpcRequest::WithPurpose`] wrapper, which
/// carries the AI client's stated [`Purpose`] beside one routed tool request
/// (REQ-F-026, ST-M009-007). The console records the purpose in the call's
/// audit records, shows it in its activity view, and forwards it on the session
/// request so the attended customer sees it. Because every request rejects
/// unknown variants, a console on an older minor would close the connection on
/// the wrapper, so the bridge sends it only after a `Hello` on the same
/// connection reports a console minor of at least [`IPC_PURPOSE_MINOR`]; to an
/// older console it sends the bare request and the purpose is dropped (the call
/// still runs, attributed to the AI, and is audited without a purpose). A
/// console on minor `9` accepts both forms, so an older bridge keeps working
/// and its calls show no purpose (REQ-N-012).
///
/// Minor `10` adds the additive, optional [`ClientName`] on
/// [`IpcRequest::WithPurpose`]: the AI client's `clientInfo.name` from the MCP
/// `initialize` request, which the console records in the call's audit records
/// and maps to the client label of its activity feed (ST-M009-011). The label
/// stays on the technician computer: the console never puts it on the session
/// protocol, so it never reaches the customer computer. Because the wrapper
/// rejects unknown fields, the bridge sends `client` only after a `Hello` on the
/// same connection reports a console minor of at least [`IPC_CLIENT_MINOR`]; an
/// older console receives the wrapper without it and its feed shows "AI
/// assistant" (REQ-N-012).
///
/// Minor `11` adds the optional `account` of every routed [`TargetIdentity`]
/// (ST-M009-008). [`TargetIdentity`] rejects unknown fields, so a bridge or
/// terminal client on an older minor would fail every answer that carries it:
/// the console sends `account` only on a connection whose `Hello` reported a
/// client minor of at least [`IPC_ACCOUNT_MINOR`], and strips it otherwise —
/// also on a connection that sent no `Hello` ([`IpcResponse::for_peer`],
/// [`identity_for_peer`]).
///
/// Minor `12` adds no IPC message. The number moves because this contract
/// tracks the wire [`PROTOCOL_V1`], which gained the drive list and the removal
/// of a folder with its content for the technician's file explorer
/// (ST-M010-001). Neither is offered over IPC: the tool surface of the bridge
/// is unchanged, so a bridge or terminal client on minor `11` works with a
/// console on minor `12` as before (REQ-N-012).
pub const IPC_PROTOCOL: ProtocolVersion = PROTOCOL_V1;

/// The first IPC contract minor whose clients decode the `account` of a
/// [`TargetIdentity`] (ST-M009-008).
pub const IPC_ACCOUNT_MINOR: u16 = 11;

/// Removes from `target` what a client on `peer_minor` cannot decode: the
/// `account` below [`IPC_ACCOUNT_MINOR`], or when the client's minor is not
/// known (no `Hello` on the connection). Applied by the console to everything
/// it writes (REQ-N-012).
pub fn identity_for_peer(target: &mut TargetIdentity, peer_minor: Option<u16>) {
    if peer_minor.is_none_or(|minor| minor < IPC_ACCOUNT_MINOR) {
        target.account = None;
    }
}

/// The first IPC contract minor whose console understands
/// [`IpcRequest::WithPurpose`] (ST-M009-007).
pub const IPC_PURPOSE_MINOR: u16 = 9;

/// The first IPC contract minor whose console understands the
/// [`ClientName`] on [`IpcRequest::WithPurpose`] (ST-M009-011).
pub const IPC_CLIENT_MINOR: u16 = 10;

/// The longest [`ClientName`], in characters.
pub const MAX_CLIENT_NAME_CHARS: usize = 64;

/// The AI client's self-reported name (`clientInfo.name` from the MCP
/// `initialize` request), bounded to one line of at most
/// [`MAX_CLIENT_NAME_CHARS`] characters (ST-M009-011).
///
/// It is a label the client chose, used only to name the client in the
/// technician's activity feed and audit records; it grants no authority and is
/// never sent to the customer computer. It is validated on decode, so a
/// multi-line, empty, or over-long name fails the frame closed.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct ClientName(String);

impl ClientName {
    /// Validates `raw` strictly: trimmed, 1 to [`MAX_CLIENT_NAME_CHARS`]
    /// characters, no control characters.
    ///
    /// # Errors
    ///
    /// A plain description of why the name was refused.
    pub fn new(raw: &str) -> Result<Self, String> {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            return Err("the client name is empty".to_string());
        }
        if trimmed.chars().any(char::is_control) {
            return Err("the client name must be one line of plain text".to_string());
        }
        if trimmed.chars().count() > MAX_CLIENT_NAME_CHARS {
            return Err(format!(
                "the client name is longer than {MAX_CLIENT_NAME_CHARS} characters"
            ));
        }
        Ok(Self(trimmed.to_owned()))
    }

    /// Builds a name from what an MCP client reported, dropping control
    /// characters and cutting an over-long name; `None` when nothing is left.
    #[must_use]
    pub fn from_client_info(raw: &str) -> Option<Self> {
        let cleaned: String = raw.chars().filter(|c| !c.is_control()).collect();
        let cut: String = cleaned.trim().chars().take(MAX_CLIENT_NAME_CHARS).collect();
        Self::new(&cut).ok()
    }

    /// The name text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for ClientName {
    type Error = String;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::new(&value)
    }
}

impl From<ClientName> for String {
    fn from(value: ClientName) -> Self {
        value.0
    }
}

/// Maximum size of one IPC frame, checked from the length prefix before any
/// allocation. Sized to hold a full [`ExecReport`] whose captured output is at
/// the bridge's cap (16 MiB across both streams), base64-expanded (~4/3) with
/// JSON overhead, plus generous slack. It is far larger than the 1 MiB wire
/// frame cap because a single IPC reply aggregates a whole collected job.
pub const MAX_IPC_FRAME_BYTES: usize = 64 * 1024 * 1024;

/// A request from the bridge to the console.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "request", rename_all = "snake_case", deny_unknown_fields)]
pub enum IpcRequest {
    /// Handshake: negotiate the contract version and learn whether a consented
    /// active session exists (and, if so, its identity). Sent first so the
    /// bridge can answer `session_id()`/`target()` without a live tool call.
    Hello {
        /// The bridge's IPC contract version.
        protocol: ProtocolVersion,
    },
    /// One routed tool request together with the AI client's stated purpose
    /// (IPC minor `9`, REQ-F-026, ST-M009-007). `call` must be one of the
    /// routed tool requests (never a `Hello`, a terminal message, or another
    /// `WithPurpose`); the console refuses anything else with
    /// [`IpcError::Malformed`] and runs nothing. The purpose is the caller's
    /// unverified claim: it is recorded and shown beside the real operation,
    /// never instead of it, and grants no authority.
    WithPurpose {
        /// The validated, single-line stated purpose.
        purpose: Purpose,
        /// The AI client's `clientInfo.name` (IPC minor `10`, ST-M009-011),
        /// sent only to a console that reported at least [`IPC_CLIENT_MINOR`];
        /// omitted when unknown, so the minor-9 wire form is unchanged.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        client: Option<ClientName>,
        /// The routed tool request the purpose belongs to.
        call: Box<IpcRequest>,
    },
    /// Report the routed target's identity and basic system state.
    SystemInfo {},
    /// Run a command to completion, capturing output up to `output_cap` bytes.
    Exec {
        /// The validated command parameters.
        params: ExecParams,
        /// The combined stdout+stderr capture cap in bytes.
        output_cap: u64,
    },
    /// Feed bytes to a running job's standard input.
    WriteStdin {
        /// The job to write to.
        job: JobId,
        /// The bytes to write (base64 on the wire).
        data: B64Bytes,
        /// Whether the caller marked the input sensitive (REQ-S-018). Stdin
        /// content is structurally never audited; the flag travels for schema
        /// symmetry with `Exec`. Additive within the contract major: omitted
        /// when `false`, so an older console's `deny_unknown_fields` decode is
        /// unaffected until the flag is actually used.
        #[serde(default, skip_serializing_if = "crate::target::is_false")]
        sensitive: bool,
    },
    /// Request cancellation of a running job.
    Cancel {
        /// The job to cancel.
        job: JobId,
    },
    /// List the jobs the routed target currently tracks.
    ListJobs {},
    /// Stat a path on the routed target.
    FsStat {
        /// The path to stat.
        path: RemotePath,
    },
    /// List one page of a directory on the routed target.
    FsList {
        /// The directory to list.
        path: RemotePath,
        /// Continue after this entry name for pagination.
        continue_after: Option<String>,
    },
    /// Create a directory on the routed target.
    MakeDir {
        /// The directory to create.
        path: RemotePath,
    },
    /// Move (rename) a path on the routed target.
    Move {
        /// The source path.
        from: RemotePath,
        /// The destination path.
        to: RemotePath,
        /// Whether an existing destination file may be replaced.
        overwrite: bool,
    },
    /// Delete a file or reparse point on the routed target.
    Delete {
        /// The path to delete.
        path: RemotePath,
    },
    /// Remove an empty directory on the routed target.
    RemoveDir {
        /// The directory to remove.
        path: RemotePath,
    },
    /// Upload a local file into target staging and commit it verified.
    Upload {
        /// The validated upload parameters.
        params: UploadParams,
    },
    /// Download a target file to a local path, verified.
    Download {
        /// The validated download parameters.
        params: DownloadParams,
    },
    /// List the staged artifacts the routed target currently tracks.
    ListStaged {},
    /// Run a staging cleanup for a trigger on the routed target.
    Cleanup {
        /// Which staged artifacts the cleanup selects.
        trigger: CleanupTrigger,
    },
    // --- Visual desktop surface (ADR-0015, ST-M005-004, IPC minor 6) ---
    //
    // Ordinary request/response calls that reach the one consented session and
    // are attributed and audited like any other routed call. The agent serves
    // them only where it advertised `desktop.v1`.
    /// Capture the routed target's current interactive desktop. Answered with
    /// [`IpcResponse::DesktopFrame`]; fails closed when no consented session is
    /// bound (REQ-S-009).
    CaptureDesktop {
        /// The validated capture parameters.
        params: CaptureParams,
    },
    /// Inject a pointer action at an absolute virtual-desktop coordinate.
    DesktopPointer {
        /// The pointer action to perform.
        action: PointerAction,
        /// The mouse button the action applies to.
        button: PointerButton,
        /// The absolute virtual-desktop x coordinate.
        x: i32,
        /// The absolute virtual-desktop y coordinate.
        y: i32,
    },
    /// Inject a scroll (wheel) action at an absolute coordinate.
    DesktopScroll {
        /// The absolute virtual-desktop x coordinate.
        x: i32,
        /// The absolute virtual-desktop y coordinate.
        y: i32,
        /// The horizontal scroll delta.
        dx: i32,
        /// The vertical scroll delta.
        dy: i32,
    },
    /// Inject a single key action naming a virtual key.
    DesktopKey {
        /// The key action (down, up, or press).
        action: KeyAction,
        /// The virtual key name.
        key: String,
    },
    /// Type a Unicode string as a sequence of key events.
    DesktopType {
        /// The text to type.
        text: String,
    },
    /// Send a chord of named virtual keys as a single hotkey press.
    DesktopHotkey {
        /// The chord's virtual key names, held in order and released in reverse.
        keys: Vec<String>,
    },
    /// Pace the input pipeline by a bounded number of milliseconds.
    DesktopWait {
        /// The pause in milliseconds.
        millis: u64,
    },
    /// Read the routed target's clipboard text.
    ClipboardGet {},
    /// Replace the routed target's clipboard text.
    ClipboardSet {
        /// The replacement clipboard text.
        text: String,
    },
    // --- Interactive terminal (ADR-0013, ST-M003-009, IPC minor 4) ---
    //
    // A terminal client is a *second class* of same-user IPC consumer, distinct
    // from the MCP bridge: it reaches the same one consented session but is
    // attributed to the `Terminal` audit actor, and attaching grants it no new
    // authority. `TermOpen` and `TermAttach` are ordinary request/response
    // calls; a **successful** `TermAttach` then switches this connection into a
    // bounded duplex stream mode in which the console pushes [`TermFrame`]s
    // while the client keeps sending the messages below.
    /// Open an interactive terminal on the routed session (a terminal is a job).
    /// Answered with [`IpcResponse::TermOpened`]; fails closed when no consented
    /// session is bound (REQ-S-009).
    TermOpen {
        /// Which shell runs in the terminal.
        shell: Shell,
        /// Initial terminal width in columns (validated against the protocol
        /// bounds when it reaches the routed session).
        cols: u16,
        /// Initial terminal height in rows.
        rows: u16,
        /// Optional remote working directory (adversarial-path validated).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        working_dir: Option<String>,
        /// Extra environment overrides (bounded by the protocol env limits).
        #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
        env: BTreeMap<String, String>,
    },
    /// Attach this connection as a local consumer of a terminal. On success the
    /// console answers [`IpcResponse::TermAttached`] and the connection enters
    /// duplex stream mode; attaching to no consented session or to an unknown or
    /// already-exited terminal fails closed.
    TermAttach {
        /// The terminal job to attach to.
        term: JobId,
    },
    /// Feed input bytes to the terminal's single pseudo-console (duplex mode).
    /// Input from every consumer is muxed to the one pty; a raw input frame is
    /// never delivered to another consumer — only the shared merged output
    /// (including the pty echo) is.
    TermInput {
        /// The terminal job.
        term: JobId,
        /// The input bytes (base64 on the wire, bounded by the chunk cap).
        data: B64Bytes,
    },
    /// Resize the terminal's pseudo-console (duplex mode, last-writer-wins). An
    /// out-of-bounds size is rejected by the routed session, never acted on.
    TermResize {
        /// The terminal job.
        term: JobId,
        /// New width in columns.
        cols: u16,
        /// New height in rows.
        rows: u16,
    },
    /// Detach this consumer from the terminal (duplex mode). Only this consumer
    /// is removed; the terminal outlives zero consumers and is closed only by an
    /// explicit `TermClose`/cancel, a revoke, or session end.
    TermDetach {
        /// The terminal job.
        term: JobId,
    },
    /// Close the terminal, ending its job for **every** attached consumer, who
    /// each receive an honest [`TermFrame::Exited`] (duplex mode).
    TermClose {
        /// The terminal job.
        term: JobId,
    },
}

impl IpcResponse {
    /// The routed target identity this answer carries, if any.
    fn target_mut(&mut self) -> Option<&mut TargetIdentity> {
        match self {
            Self::Hello { target, .. } => target.as_mut(),
            Self::SystemInfo { target, .. }
            | Self::Exec { target, .. }
            | Self::WriteStdin { target, .. }
            | Self::Cancel { target, .. }
            | Self::ListJobs { target, .. }
            | Self::FsStat { target, .. }
            | Self::FsList { target, .. }
            | Self::MakeDir { target, .. }
            | Self::Move { target, .. }
            | Self::Delete { target, .. }
            | Self::RemoveDir { target, .. }
            | Self::Upload { target, .. }
            | Self::Download { target, .. }
            | Self::ListStaged { target, .. }
            | Self::Cleanup { target, .. }
            | Self::DesktopFrame { target, .. }
            | Self::DesktopAck { target, .. }
            | Self::ClipboardContents { target, .. }
            | Self::TermOpened { target, .. }
            | Self::TermAttached { target, .. } => Some(target),
            Self::Error { .. } => None,
        }
    }

    /// This answer in the form a client on `peer_minor` decodes
    /// ([`identity_for_peer`]); `None` when the connection sent no `Hello`.
    #[must_use]
    pub fn for_peer(mut self, peer_minor: Option<u16>) -> Self {
        if let Some(target) = self.target_mut() {
            identity_for_peer(target, peer_minor);
        }
        self
    }
}

impl IpcRequest {
    /// Whether this is a routed tool request that may carry a stated purpose
    /// inside [`IpcRequest::WithPurpose`]: one of the MCP bridge's tool calls,
    /// not the authority-free `Hello`, a technician terminal message (those are
    /// labelled by their source instead, REQ-F-026), or a nested wrapper.
    #[must_use]
    pub fn is_tool_call(&self) -> bool {
        !matches!(
            self,
            Self::Hello { .. }
                | Self::WithPurpose { .. }
                | Self::TermOpen { .. }
                | Self::TermAttach { .. }
                | Self::TermInput { .. }
                | Self::TermResize { .. }
                | Self::TermDetach { .. }
                | Self::TermClose { .. }
        )
    }
}

/// A response from the console to the bridge.
///
/// Every non-error variant carries the routed `session` and `target` so a
/// result can never be mistaken for a different session or the local machine
/// (REQ-F-016). Errors are carried in [`IpcResponse::Error`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "response", rename_all = "snake_case", deny_unknown_fields)]
pub enum IpcResponse {
    /// Answer to [`IpcRequest::Hello`]. `session`/`target` are `Some` only when
    /// a consented active session exists; `None` is the fail-closed idle state.
    Hello {
        /// The console's IPC contract version.
        protocol: ProtocolVersion,
        /// The routed session, when one is consented and active.
        session: Option<SessionId>,
        /// The routed target identity, when one is consented and active.
        target: Option<TargetIdentity>,
    },
    /// Answer to [`IpcRequest::SystemInfo`].
    SystemInfo {
        /// The routed session (REQ-F-016).
        session: SessionId,
        /// The routed target identity (REQ-F-016).
        target: TargetIdentity,
        /// The console-minted, session-scoped `call_id` this routed call was
        /// audited under, returned so the AI transcript maps one-to-one onto
        /// the audit trail (ST-M004-006, ADR-0008 Appendix A). Additive at IPC
        /// minor `3`: `#[serde(default)]` so an older console's reply (no
        /// `call_id`) still decodes, to `0`.
        #[serde(default)]
        call_id: u64,
        /// The system-info report.
        report: SystemInfoReport,
    },
    /// Answer to [`IpcRequest::Exec`].
    Exec {
        /// The routed session (REQ-F-016).
        session: SessionId,
        /// The routed target identity (REQ-F-016).
        target: TargetIdentity,
        /// The console-minted, session-scoped `call_id` this routed call was
        /// audited under, returned so the AI transcript maps one-to-one onto
        /// the audit trail (ST-M004-006, ADR-0008 Appendix A). Additive at IPC
        /// minor `3`: `#[serde(default)]` so an older console's reply (no
        /// `call_id`) still decodes, to `0`.
        #[serde(default)]
        call_id: u64,
        /// The collected execution report.
        report: ExecReport,
    },
    /// Answer to [`IpcRequest::WriteStdin`].
    WriteStdin {
        /// The routed session (REQ-F-016).
        session: SessionId,
        /// The routed target identity (REQ-F-016).
        target: TargetIdentity,
        /// The console-minted, session-scoped `call_id` this routed call was
        /// audited under, returned so the AI transcript maps one-to-one onto
        /// the audit trail (ST-M004-006, ADR-0008 Appendix A). Additive at IPC
        /// minor `3`: `#[serde(default)]` so an older console's reply (no
        /// `call_id`) still decodes, to `0`.
        #[serde(default)]
        call_id: u64,
        /// The number of bytes written.
        written: u64,
    },
    /// Answer to [`IpcRequest::Cancel`].
    Cancel {
        /// The routed session (REQ-F-016).
        session: SessionId,
        /// The routed target identity (REQ-F-016).
        target: TargetIdentity,
        /// The console-minted, session-scoped `call_id` this routed call was
        /// audited under, returned so the AI transcript maps one-to-one onto
        /// the audit trail (ST-M004-006, ADR-0008 Appendix A). Additive at IPC
        /// minor `3`: `#[serde(default)]` so an older console's reply (no
        /// `call_id`) still decodes, to `0`.
        #[serde(default)]
        call_id: u64,
    },
    /// Answer to [`IpcRequest::ListJobs`].
    ListJobs {
        /// The routed session (REQ-F-016).
        session: SessionId,
        /// The routed target identity (REQ-F-016).
        target: TargetIdentity,
        /// The console-minted, session-scoped `call_id` this routed call was
        /// audited under, returned so the AI transcript maps one-to-one onto
        /// the audit trail (ST-M004-006, ADR-0008 Appendix A). Additive at IPC
        /// minor `3`: `#[serde(default)]` so an older console's reply (no
        /// `call_id`) still decodes, to `0`.
        #[serde(default)]
        call_id: u64,
        /// The tracked jobs.
        jobs: Vec<JobSummary>,
    },
    /// Answer to [`IpcRequest::FsStat`].
    FsStat {
        /// The routed session (REQ-F-016).
        session: SessionId,
        /// The routed target identity (REQ-F-016).
        target: TargetIdentity,
        /// The console-minted, session-scoped `call_id` this routed call was
        /// audited under, returned so the AI transcript maps one-to-one onto
        /// the audit trail (ST-M004-006, ADR-0008 Appendix A). Additive at IPC
        /// minor `3`: `#[serde(default)]` so an older console's reply (no
        /// `call_id`) still decodes, to `0`.
        #[serde(default)]
        call_id: u64,
        /// The stat report.
        report: StatReport,
    },
    /// Answer to [`IpcRequest::FsList`].
    FsList {
        /// The routed session (REQ-F-016).
        session: SessionId,
        /// The routed target identity (REQ-F-016).
        target: TargetIdentity,
        /// The console-minted, session-scoped `call_id` this routed call was
        /// audited under, returned so the AI transcript maps one-to-one onto
        /// the audit trail (ST-M004-006, ADR-0008 Appendix A). Additive at IPC
        /// minor `3`: `#[serde(default)]` so an older console's reply (no
        /// `call_id`) still decodes, to `0`.
        #[serde(default)]
        call_id: u64,
        /// One page of the listing.
        page: DirPage,
    },
    /// Answer to [`IpcRequest::MakeDir`].
    MakeDir {
        /// The routed session (REQ-F-016).
        session: SessionId,
        /// The routed target identity (REQ-F-016).
        target: TargetIdentity,
        /// The console-minted, session-scoped `call_id` this routed call was
        /// audited under, returned so the AI transcript maps one-to-one onto
        /// the audit trail (ST-M004-006, ADR-0008 Appendix A). Additive at IPC
        /// minor `3`: `#[serde(default)]` so an older console's reply (no
        /// `call_id`) still decodes, to `0`.
        #[serde(default)]
        call_id: u64,
    },
    /// Answer to [`IpcRequest::Move`].
    Move {
        /// The routed session (REQ-F-016).
        session: SessionId,
        /// The routed target identity (REQ-F-016).
        target: TargetIdentity,
        /// The console-minted, session-scoped `call_id` this routed call was
        /// audited under, returned so the AI transcript maps one-to-one onto
        /// the audit trail (ST-M004-006, ADR-0008 Appendix A). Additive at IPC
        /// minor `3`: `#[serde(default)]` so an older console's reply (no
        /// `call_id`) still decodes, to `0`.
        #[serde(default)]
        call_id: u64,
    },
    /// Answer to [`IpcRequest::Delete`].
    Delete {
        /// The routed session (REQ-F-016).
        session: SessionId,
        /// The routed target identity (REQ-F-016).
        target: TargetIdentity,
        /// The console-minted, session-scoped `call_id` this routed call was
        /// audited under, returned so the AI transcript maps one-to-one onto
        /// the audit trail (ST-M004-006, ADR-0008 Appendix A). Additive at IPC
        /// minor `3`: `#[serde(default)]` so an older console's reply (no
        /// `call_id`) still decodes, to `0`.
        #[serde(default)]
        call_id: u64,
    },
    /// Answer to [`IpcRequest::RemoveDir`].
    RemoveDir {
        /// The routed session (REQ-F-016).
        session: SessionId,
        /// The routed target identity (REQ-F-016).
        target: TargetIdentity,
        /// The console-minted, session-scoped `call_id` this routed call was
        /// audited under, returned so the AI transcript maps one-to-one onto
        /// the audit trail (ST-M004-006, ADR-0008 Appendix A). Additive at IPC
        /// minor `3`: `#[serde(default)]` so an older console's reply (no
        /// `call_id`) still decodes, to `0`.
        #[serde(default)]
        call_id: u64,
    },
    /// Answer to [`IpcRequest::Upload`].
    Upload {
        /// The routed session (REQ-F-016).
        session: SessionId,
        /// The routed target identity (REQ-F-016).
        target: TargetIdentity,
        /// The console-minted, session-scoped `call_id` this routed call was
        /// audited under, returned so the AI transcript maps one-to-one onto
        /// the audit trail (ST-M004-006, ADR-0008 Appendix A). Additive at IPC
        /// minor `3`: `#[serde(default)]` so an older console's reply (no
        /// `call_id`) still decodes, to `0`.
        #[serde(default)]
        call_id: u64,
        /// The collected upload report.
        report: UploadReport,
    },
    /// Answer to [`IpcRequest::Download`].
    Download {
        /// The routed session (REQ-F-016).
        session: SessionId,
        /// The routed target identity (REQ-F-016).
        target: TargetIdentity,
        /// The console-minted, session-scoped `call_id` this routed call was
        /// audited under, returned so the AI transcript maps one-to-one onto
        /// the audit trail (ST-M004-006, ADR-0008 Appendix A). Additive at IPC
        /// minor `3`: `#[serde(default)]` so an older console's reply (no
        /// `call_id`) still decodes, to `0`.
        #[serde(default)]
        call_id: u64,
        /// The collected download report.
        report: DownloadReport,
    },
    /// Answer to [`IpcRequest::ListStaged`].
    ListStaged {
        /// The routed session (REQ-F-016).
        session: SessionId,
        /// The routed target identity (REQ-F-016).
        target: TargetIdentity,
        /// The console-minted, session-scoped `call_id` this routed call was
        /// audited under, returned so the AI transcript maps one-to-one onto
        /// the audit trail (ST-M004-006, ADR-0008 Appendix A). Additive at IPC
        /// minor `3`: `#[serde(default)]` so an older console's reply (no
        /// `call_id`) still decodes, to `0`.
        #[serde(default)]
        call_id: u64,
        /// The tracked staged artifacts.
        entries: Vec<StagedEntry>,
    },
    /// Answer to [`IpcRequest::Cleanup`].
    Cleanup {
        /// The routed session (REQ-F-016).
        session: SessionId,
        /// The routed target identity (REQ-F-016).
        target: TargetIdentity,
        /// The console-minted, session-scoped `call_id` this routed call was
        /// audited under, returned so the AI transcript maps one-to-one onto
        /// the audit trail (ST-M004-006, ADR-0008 Appendix A). Additive at IPC
        /// minor `3`: `#[serde(default)]` so an older console's reply (no
        /// `call_id`) still decodes, to `0`.
        #[serde(default)]
        call_id: u64,
        /// The cleanup summary.
        report: CleanupSummary,
    },
    /// Answer to [`IpcRequest::CaptureDesktop`]: one honest desktop frame.
    DesktopFrame {
        /// The routed session (REQ-F-016).
        session: SessionId,
        /// The routed target identity (REQ-F-016).
        target: TargetIdentity,
        /// The console-minted, session-scoped `call_id` this action was audited
        /// under (ST-M004-006). Additive at IPC minor `6`: `#[serde(default)]`.
        #[serde(default)]
        call_id: u64,
        /// The captured frame (metadata always; pixels only when active).
        report: CaptureReport,
    },
    /// Shared answer to the desktop input operations
    /// ([`IpcRequest::DesktopPointer`]/`DesktopScroll`/`DesktopKey`/`DesktopType`/
    /// `DesktopHotkey`/`DesktopWait`) and [`IpcRequest::ClipboardSet`]: the agent
    /// acknowledged the action. These ops carry no payload, so one ack shape
    /// serves them all without loss of meaning.
    DesktopAck {
        /// The routed session (REQ-F-016).
        session: SessionId,
        /// The routed target identity (REQ-F-016).
        target: TargetIdentity,
        /// The console-minted, session-scoped `call_id` this action was audited
        /// under (ST-M004-006). Additive at IPC minor `6`: `#[serde(default)]`.
        #[serde(default)]
        call_id: u64,
    },
    /// Answer to [`IpcRequest::ClipboardGet`]: the clipboard text, or `None` when
    /// it holds no text.
    ClipboardContents {
        /// The routed session (REQ-F-016).
        session: SessionId,
        /// The routed target identity (REQ-F-016).
        target: TargetIdentity,
        /// The console-minted, session-scoped `call_id` this action was audited
        /// under (ST-M004-006). Additive at IPC minor `6`: `#[serde(default)]`.
        #[serde(default)]
        call_id: u64,
        /// The clipboard's text, or `None` when it holds no text.
        text: Option<String>,
    },
    /// Answer to [`IpcRequest::TermOpen`]: the interactive terminal job started.
    /// The `job` is the terminal handle for a following `TermAttach`.
    TermOpened {
        /// The routed session (REQ-F-016).
        session: SessionId,
        /// The routed target identity (REQ-F-016).
        target: TargetIdentity,
        /// The console-minted, session-scoped `call_id` this action was audited
        /// under (ST-M004-006). Additive at IPC minor `4`: `#[serde(default)]`.
        #[serde(default)]
        call_id: u64,
        /// The opened terminal's job id.
        job: JobId,
    },
    /// Answer to a **successful** [`IpcRequest::TermAttach`]: this connection is
    /// now in duplex stream mode, and every subsequent server frame is a
    /// [`TermFrame`] until the stream ends.
    TermAttached {
        /// The routed session (REQ-F-016).
        session: SessionId,
        /// The routed target identity (REQ-F-016).
        target: TargetIdentity,
        /// The console-minted, session-scoped `call_id` this attach was audited
        /// under (ST-M004-006). Additive at IPC minor `4`: `#[serde(default)]`.
        #[serde(default)]
        call_id: u64,
        /// The attached terminal's job id.
        term: JobId,
    },
    /// A failed request.
    Error {
        /// Why the request failed.
        error: IpcError,
        /// The console-minted `call_id` of the routed call that failed, when the
        /// console had already minted one (every routed tool call does, success
        /// or failure — including the fail-closed no-consented-session case), so
        /// even a failed call correlates one-to-one with its audit record
        /// (ST-M004-006). `0` marks an error with no associated call (e.g. a
        /// handshake or contract-version failure).
        #[serde(default)]
        call_id: u64,
    },
}

/// A server-pushed frame in terminal duplex stream mode (ST-M003-009).
///
/// After a successful [`IpcRequest::TermAttach`] the console pushes these frames
/// to the attached consumer while the consumer keeps sending
/// `TermInput`/`TermResize`/`TermDetach`/`TermClose` over the **same**
/// length-prefixed connection. This is the bounded duplex extension of
/// ADR-0004's request/response turn-taking, layered on the existing framing.
///
/// Every frame carries the routed `session` and `target` so a frame can never be
/// mistaken for another session or the local machine (REQ-F-016). Every frame is
/// bounded: an [`TermFrame::Output`] chunk is at most one wire output chunk,
/// far below [`MAX_IPC_FRAME_BYTES`]. Each of the four variants closes the
/// stream honestly except `Output`; none fabricates a clean close (REQ-N-008).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "frame", rename_all = "snake_case", deny_unknown_fields)]
pub enum TermFrame {
    /// A chunk of the terminal's merged output (raw VT pass-through, ADR-0010).
    /// Every attached consumer sees the full shared stream, including the pty
    /// echo of any consumer's input.
    Output {
        /// The routed session (REQ-F-016).
        session: SessionId,
        /// The routed target identity (REQ-F-016).
        target: TargetIdentity,
        /// The terminal job this output belongs to.
        term: JobId,
        /// The raw merged output bytes (base64 on the wire).
        data: B64Bytes,
    },
    /// The terminal (job) exited; carries the honest outcome and exit code, if
    /// any — never a fabricated clean close (REQ-N-008). Ends the stream.
    Exited {
        /// The routed session (REQ-F-016).
        session: SessionId,
        /// The routed target identity (REQ-F-016).
        target: TargetIdentity,
        /// The terminal job that exited.
        term: JobId,
        /// The process exit code, when one exists.
        exit_code: Option<i32>,
        /// The honest terminal outcome.
        outcome: JobOutcome,
    },
    /// The session was revoked or stopped while this consumer was attached: the
    /// stream is torn down fail-closed with this honest frame (REQ-S-004,
    /// REQ-S-009). Ends the stream.
    Revoked {
        /// The routed session (REQ-F-016).
        session: SessionId,
        /// The routed target identity (REQ-F-016).
        target: TargetIdentity,
        /// The terminal job whose stream was torn down.
        term: JobId,
    },
    /// This consumer fell too far behind the VT stream and was disconnected, so
    /// the shared stream is never corrupted by dropping bytes for other
    /// consumers (REQ-N-006). An honest close of *this* consumer only; the
    /// terminal and other consumers are unaffected. Ends the stream.
    Dropped {
        /// The routed session (REQ-F-016).
        session: SessionId,
        /// The routed target identity (REQ-F-016).
        target: TargetIdentity,
        /// The terminal job this consumer was dropped from.
        term: JobId,
    },
}

/// A failure surfaced by the console over the IPC contract.
///
/// [`IpcError::NoConsentedSession`] is the explicit fail-closed error required
/// when no consented active session exists (REQ-S-009); everything else maps a
/// [`TargetError`] faithfully so the bridge reports the honest remote outcome
/// category (REQ-N-008). Every variant is safe to surface: none carries key
/// material, the Session Approval Code, or remote content (REQ-S-018).
#[derive(Debug, Clone, PartialEq, Eq, Error, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum IpcError {
    /// No consented active session is bound; the tool call fails closed and
    /// nothing runs (REQ-S-009, ST-M004-004).
    #[error("no consented active session; nothing ran (fail-closed)")]
    NoConsentedSession,
    /// The bridge and console disagree on the IPC contract major version.
    #[error("incompatible IPC contract version: console {local}, bridge {peer}")]
    IncompatibleVersion {
        /// The console's version.
        local: ProtocolVersion,
        /// The bridge's version.
        peer: ProtocolVersion,
    },
    /// A remote-target error, mapped faithfully from a [`TargetError`].
    #[error(transparent)]
    Target(TargetError),
    /// The request could not be decoded or was not understood.
    #[error("malformed IPC request: {message}")]
    Malformed {
        /// A secret-free description.
        message: String,
    },
}

impl IpcError {
    /// Maps this IPC error onto the bridge-facing [`TargetError`], keeping a
    /// fail-closed no-session distinct from a remote-reported failure and a
    /// lost connection. A version or malformed error becomes
    /// [`TargetError::Unexpected`] (the console answered outside contract).
    #[must_use]
    pub fn into_target_error(self) -> TargetError {
        match self {
            Self::NoConsentedSession => TargetError::NoConsentedSession,
            Self::Target(err) => err,
            Self::IncompatibleVersion { .. } | Self::Malformed { .. } => TargetError::Unexpected,
        }
    }
}

/// A low-level IPC framing or serialization failure.
#[derive(Debug, Error)]
pub enum IpcCodecError {
    /// An I/O failure while reading or writing the stream.
    #[error("ipc i/o error: {0}")]
    Io(#[from] std::io::Error),
    /// The peer closed the connection (cleanly at a boundary or mid-frame).
    #[error("ipc peer closed the connection")]
    Eof,
    /// The declared or offered frame length exceeded [`MAX_IPC_FRAME_BYTES`].
    /// On a read this is detected from the length prefix alone, before
    /// allocation.
    #[error("ipc frame of {len} bytes exceeds the {max}-byte limit")]
    Oversized {
        /// The declared or offered length in bytes.
        len: usize,
        /// The enforced maximum in bytes.
        max: usize,
    },
    /// A payload could not be serialized to or parsed from JSON.
    #[error("ipc payload codec error: {0}")]
    Codec(#[source] serde_json::Error),
}

/// Writes one length-prefixed JSON message, rejecting an over-cap payload
/// before any bytes reach the stream so the peer never sees a partial frame.
pub async fn write_message<W, T>(io: &mut W, message: &T) -> Result<(), IpcCodecError>
where
    W: AsyncWrite + Unpin,
    T: Serialize,
{
    let payload = serde_json::to_vec(message).map_err(IpcCodecError::Codec)?;
    if payload.len() > MAX_IPC_FRAME_BYTES {
        return Err(IpcCodecError::Oversized {
            len: payload.len(),
            max: MAX_IPC_FRAME_BYTES,
        });
    }
    let len = u32::try_from(payload.len()).map_err(|_| IpcCodecError::Oversized {
        len: payload.len(),
        max: MAX_IPC_FRAME_BYTES,
    })?;
    io.write_all(&len.to_le_bytes()).await?;
    io.write_all(&payload).await?;
    io.flush().await?;
    Ok(())
}

/// Reads one length-prefixed JSON message, rejecting an over-cap declared
/// length from the 4-byte prefix **before** allocating the payload buffer.
///
/// This is not cancel-safe: callers drive it with strict request/response
/// turn-taking and close the connection on any error.
pub async fn read_message<R, T>(io: &mut R) -> Result<T, IpcCodecError>
where
    R: AsyncRead + Unpin,
    T: DeserializeOwned,
{
    let mut prefix = [0u8; 4];
    read_exact(io, &mut prefix).await?;
    let len = u32::from_le_bytes(prefix) as usize;
    if len > MAX_IPC_FRAME_BYTES {
        return Err(IpcCodecError::Oversized {
            len,
            max: MAX_IPC_FRAME_BYTES,
        });
    }
    let mut buf = vec![0u8; len];
    read_exact(io, &mut buf).await?;
    serde_json::from_slice(&buf).map_err(IpcCodecError::Codec)
}

/// Fills `buf` completely, mapping a short read (peer closed) to
/// [`IpcCodecError::Eof`].
async fn read_exact<R: AsyncRead + Unpin>(io: &mut R, buf: &mut [u8]) -> Result<(), IpcCodecError> {
    match io.read_exact(buf).await {
        Ok(_) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::UnexpectedEof => Err(IpcCodecError::Eof),
        Err(err) => Err(IpcCodecError::Io(err)),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use cretlink_protocol::JobOutcome;

    #[tokio::test]
    async fn round_trips_a_request_and_response() {
        let (mut a, mut b) = tokio::io::duplex(4096);
        let request = IpcRequest::Exec {
            params: ExecParams {
                shell: cretlink_protocol::Shell::PowerShell,
                command: "echo hi".to_string(),
                working_dir: None,
                env: std::collections::BTreeMap::new(),
                timeout_ms: None,
                sensitive: false,
            },
            output_cap: 1024,
        };
        write_message(&mut a, &request).await.unwrap();
        let decoded: IpcRequest = read_message(&mut b).await.unwrap();
        assert_eq!(decoded, request);

        let response = IpcResponse::Exec {
            session: SessionId::new(),
            target: TargetIdentity {
                hostname: "SIMULATED-TARGET".to_string(),
                os_version: "Simulated".to_string(),
                arch: "x86_64".to_string(),
                elevated: false,
                simulated: true,
                agent_version: "test".to_string(),
                account: None,
            },
            call_id: 7,
            report: ExecReport {
                job: JobId(1),
                working_dir: "C:\\Users\\customer".to_string(),
                stdout: b"hi\n".to_vec(),
                stderr: Vec::new(),
                truncated: false,
                exit_code: Some(0),
                duration_ms: 1,
                outcome: JobOutcome::Completed,
            },
        };
        write_message(&mut b, &response).await.unwrap();
        let decoded: IpcResponse = read_message(&mut a).await.unwrap();
        assert_eq!(decoded, response);
    }

    #[tokio::test]
    async fn oversized_prefix_is_rejected_before_allocation() {
        let (mut a, mut b) = tokio::io::duplex(64);
        let declared = u32::try_from(MAX_IPC_FRAME_BYTES + 1).unwrap();
        a.write_all(&declared.to_le_bytes()).await.unwrap();
        a.flush().await.unwrap();
        // The reader must reject from the prefix alone; no payload is ever sent,
        // so an allocating reader would block here forever.
        let err = read_message::<_, IpcRequest>(&mut b).await.unwrap_err();
        assert!(
            matches!(err, IpcCodecError::Oversized { .. }),
            "got {err:?}"
        );
        drop(a);
    }

    #[tokio::test]
    async fn clean_close_surfaces_as_eof() {
        let (a, mut b) = tokio::io::duplex(64);
        drop(a);
        let err = read_message::<_, IpcRequest>(&mut b).await.unwrap_err();
        assert!(matches!(err, IpcCodecError::Eof), "got {err:?}");
    }

    #[test]
    fn no_consented_session_maps_to_fail_closed_target_error() {
        assert_eq!(
            IpcError::NoConsentedSession.into_target_error(),
            TargetError::NoConsentedSession
        );
    }

    fn sample_target() -> TargetIdentity {
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

    /// Every terminal IPC request round-trips through the length-prefixed codec
    /// unchanged (ST-M003-009, IPC minor 4).
    #[tokio::test]
    async fn terminal_requests_round_trip() {
        let requests = [
            IpcRequest::TermOpen {
                shell: Shell::PowerShell,
                cols: 80,
                rows: 24,
                working_dir: Some("C:\\Users\\customer".to_string()),
                env: BTreeMap::from([("K".to_string(), "V".to_string())]),
            },
            IpcRequest::TermAttach { term: JobId(3) },
            IpcRequest::TermInput {
                term: JobId(3),
                data: B64Bytes::new(b"echo hi\r\n".to_vec()),
            },
            IpcRequest::TermResize {
                term: JobId(3),
                cols: 120,
                rows: 40,
            },
            IpcRequest::TermDetach { term: JobId(3) },
            IpcRequest::TermClose { term: JobId(3) },
        ];
        for request in requests {
            let (mut a, mut b) = tokio::io::duplex(4096);
            write_message(&mut a, &request).await.unwrap();
            let decoded: IpcRequest = read_message(&mut b).await.unwrap();
            assert_eq!(decoded, request);
        }
    }

    /// The terminal responses and every server-pushed [`TermFrame`] round-trip,
    /// each carrying the routed session and target (REQ-F-016).
    #[tokio::test]
    async fn terminal_responses_and_frames_round_trip() {
        let session = SessionId::new();
        let target = sample_target();

        let responses = [
            IpcResponse::TermOpened {
                session,
                target: target.clone(),
                call_id: 5,
                job: JobId(7),
            },
            IpcResponse::TermAttached {
                session,
                target: target.clone(),
                call_id: 6,
                term: JobId(7),
            },
        ];
        for response in responses {
            let (mut a, mut b) = tokio::io::duplex(4096);
            write_message(&mut a, &response).await.unwrap();
            let decoded: IpcResponse = read_message(&mut b).await.unwrap();
            assert_eq!(decoded, response);
        }

        let frames = [
            TermFrame::Output {
                session,
                target: target.clone(),
                term: JobId(7),
                data: B64Bytes::new(b"sim-term$ ".to_vec()),
            },
            TermFrame::Exited {
                session,
                target: target.clone(),
                term: JobId(7),
                exit_code: Some(0),
                outcome: JobOutcome::Completed,
            },
            TermFrame::Revoked {
                session,
                target: target.clone(),
                term: JobId(7),
            },
            TermFrame::Dropped {
                session,
                target: target.clone(),
                term: JobId(7),
            },
        ];
        for frame in frames {
            let (mut a, mut b) = tokio::io::duplex(4096);
            write_message(&mut a, &frame).await.unwrap();
            let decoded: TermFrame = read_message(&mut b).await.unwrap();
            assert_eq!(decoded, frame);
        }
    }

    /// A terminal response missing the additive `call_id` still decodes (an
    /// older-minor console reply), defaulting the field to `0` (REQ-N-012).
    #[test]
    fn terminal_opened_tolerates_absent_call_id() {
        let session = SessionId::new();
        let json = serde_json::json!({
            "response": "term_opened",
            "session": session,
            "target": sample_target(),
            "job": 9,
        });
        let decoded: IpcResponse = serde_json::from_value(json).unwrap();
        assert_eq!(
            decoded,
            IpcResponse::TermOpened {
                session,
                target: sample_target(),
                call_id: 0,
                job: JobId(9),
            }
        );
    }

    /// `TermOpen` omits its default-empty `working_dir`/`env` on the wire, so an
    /// older-minor peer's `deny_unknown_fields` decode is unaffected (REQ-N-012).
    #[test]
    fn term_open_omits_empty_optional_fields() {
        let request = IpcRequest::TermOpen {
            shell: Shell::Cmd,
            cols: 80,
            rows: 24,
            working_dir: None,
            env: BTreeMap::new(),
        };
        let value = serde_json::to_value(&request).unwrap();
        let object = value.as_object().unwrap();
        assert!(
            !object.contains_key("working_dir"),
            "empty working_dir omitted"
        );
        assert!(!object.contains_key("env"), "empty env omitted");
    }

    /// An unknown field on a terminal frame is rejected (`deny_unknown_fields`),
    /// so a schema drift can never be silently accepted.
    #[test]
    fn unknown_field_on_a_terminal_frame_is_rejected() {
        let session = SessionId::new();
        let json = serde_json::json!({
            "frame": "revoked",
            "session": session,
            "target": sample_target(),
            "term": 7,
            "unexpected": true,
        });
        assert!(serde_json::from_value::<TermFrame>(json).is_err());
    }

    /// A stated purpose rides beside one routed tool call in the minor-9
    /// `WithPurpose` wrapper and round-trips unchanged; the bare request keeps
    /// its earlier wire form, so an older console still decodes a call that
    /// carries no purpose (ST-M009-007, REQ-N-012).
    #[tokio::test]
    async fn purpose_wrapper_round_trips_and_bare_requests_are_unchanged() {
        let purpose = Purpose::new("Checking free disk space").unwrap();
        let request = IpcRequest::WithPurpose {
            purpose: purpose.clone(),
            client: None,
            call: Box::new(IpcRequest::ListJobs {}),
        };
        let (mut a, mut b) = tokio::io::duplex(4096);
        write_message(&mut a, &request).await.unwrap();
        let decoded: IpcRequest = read_message(&mut b).await.unwrap();
        assert_eq!(decoded, request);

        let value = serde_json::to_value(&request).unwrap();
        assert_eq!(
            value,
            serde_json::json!({
                "request": "with_purpose",
                "purpose": "Checking free disk space",
                "call": { "request": "list_jobs" },
            })
        );
        assert_eq!(
            serde_json::to_value(IpcRequest::ListJobs {}).unwrap(),
            serde_json::json!({ "request": "list_jobs" })
        );
    }

    /// An invalid purpose inside the wrapper fails the decode, like any other
    /// out-of-contract field (ST-M009-007).
    #[test]
    fn purpose_wrapper_rejects_an_invalid_purpose() {
        for bad in ["", "   ", "two\nlines"] {
            let json = serde_json::json!({
                "request": "with_purpose",
                "purpose": bad,
                "call": { "request": "list_jobs" },
            });
            assert!(
                serde_json::from_value::<IpcRequest>(json).is_err(),
                "{bad:?} must be rejected"
            );
        }
        let json = serde_json::json!({
            "request": "with_purpose",
            "purpose": "a".repeat(cretlink_protocol::limits::MAX_PURPOSE_CHARS + 1),
            "call": { "request": "list_jobs" },
        });
        assert!(serde_json::from_value::<IpcRequest>(json).is_err());
    }

    /// Only routed tool calls may carry a purpose: a handshake, a terminal
    /// message, or a nested wrapper is not a tool call (ST-M009-007).
    #[test]
    fn only_tool_calls_may_carry_a_purpose() {
        assert!(IpcRequest::ListJobs {}.is_tool_call());
        assert!(IpcRequest::DesktopWait { millis: 1 }.is_tool_call());
        assert!(
            !IpcRequest::Hello {
                protocol: IPC_PROTOCOL
            }
            .is_tool_call()
        );
        assert!(!IpcRequest::TermAttach { term: JobId(1) }.is_tool_call());
        assert!(
            !IpcRequest::WithPurpose {
                purpose: Purpose::new("x").unwrap(),
                client: None,
                call: Box::new(IpcRequest::ListJobs {}),
            }
            .is_tool_call()
        );
        const { assert!(IPC_PROTOCOL.minor >= IPC_PURPOSE_MINOR) };
        const { assert!(IPC_PROTOCOL.minor >= IPC_CLIENT_MINOR) };
    }

    /// The account in a routed identity reaches only a client on IPC minor 11
    /// or later; an older or unannounced client gets the minor-10 form
    /// (ST-M009-008, REQ-N-012).
    #[test]
    fn the_account_is_stripped_for_an_older_client() {
        let mut target = sample_target();
        target.account = Some(cretlink_protocol::AccountName::new(r"ACME\ann").unwrap());
        let answer = IpcResponse::Hello {
            protocol: IPC_PROTOCOL,
            session: Some(SessionId::new()),
            target: Some(target.clone()),
        };
        let account = |response: &IpcResponse| match response {
            IpcResponse::Hello { target, .. } => target.as_ref().and_then(|t| t.account.clone()),
            _ => None,
        };
        assert!(account(&answer.clone().for_peer(Some(IPC_ACCOUNT_MINOR))).is_some());
        assert!(account(&answer.clone().for_peer(Some(IPC_ACCOUNT_MINOR - 1))).is_none());
        assert!(account(&answer.for_peer(None)).is_none());
        let routed = IpcResponse::ListJobs {
            session: SessionId::new(),
            target,
            call_id: 1,
            jobs: Vec::new(),
        }
        .for_peer(Some(10));
        let json = serde_json::to_value(&routed).unwrap();
        assert!(json["target"].get("account").is_none(), "{json}");
        const { assert!(IPC_PROTOCOL.minor >= IPC_ACCOUNT_MINOR) };
    }

    /// The client name rides beside the purpose at IPC minor 10, is omitted
    /// when unknown (the minor-9 wire form), and is validated on decode
    /// (ST-M009-011, REQ-N-012).
    #[tokio::test]
    async fn client_name_round_trips_and_is_validated() {
        let request = IpcRequest::WithPurpose {
            purpose: Purpose::new("Checking free disk space").unwrap(),
            client: Some(ClientName::new("claude-code").unwrap()),
            call: Box::new(IpcRequest::ListJobs {}),
        };
        let (mut a, mut b) = tokio::io::duplex(4096);
        write_message(&mut a, &request).await.unwrap();
        let decoded: IpcRequest = read_message(&mut b).await.unwrap();
        assert_eq!(decoded, request);
        assert_eq!(
            serde_json::to_value(&request).unwrap()["client"],
            serde_json::json!("claude-code")
        );
        for bad in [
            serde_json::json!(""),
            serde_json::json!("two\nlines"),
            serde_json::json!("c".repeat(MAX_CLIENT_NAME_CHARS + 1)),
        ] {
            let json = serde_json::json!({
                "request": "with_purpose",
                "purpose": "Checking free disk space",
                "client": bad,
                "call": { "request": "list_jobs" },
            });
            assert!(serde_json::from_value::<IpcRequest>(json).is_err());
        }
        assert_eq!(
            ClientName::from_client_info("  gemini-cli-mcp-client\r\n")
                .unwrap()
                .as_str(),
            "gemini-cli-mcp-client"
        );
        assert!(ClientName::from_client_info("\u{0007}").is_none());
    }

    /// Reboot-resume can be armed **only** by the attended local gesture on the
    /// support-agent window; it is never reachable remotely (ADR-0014,
    /// ST-M006-001, REQ-S-001/003). The MCP bridge reaches the console solely
    /// through [`IpcRequest`], which deliberately carries no arming variant, so a
    /// bridge (or a hostile local peer) that fabricates one is rejected by the
    /// console's `deny_unknown_fields` decoder rather than arming anything. This
    /// guards the "refused from the MCP surface" acceptance criterion against a
    /// future variant being added without a spec review.
    #[test]
    fn the_ipc_surface_cannot_arm_reboot_resume() {
        for tag in [
            "arm_reboot_resume",
            "reboot_resume_arm",
            "arm",
            "reboot_resume",
            "enable_reboot_resume",
        ] {
            let json = serde_json::json!({ "request": tag, "lifetime_ms": 60_000 });
            assert!(
                serde_json::from_value::<IpcRequest>(json).is_err(),
                "the IPC surface must not accept an arming request `{tag}`",
            );
        }
    }
}
