//! The [`McpBridge`] rmcp server exposing generic remote-computer primitives.
//!
//! The bridge is deliberately generic (per the product model): it offers
//! system inspection, arbitrary shell execution with streamed output, stdin,
//! cancellation and job listing, file operations and transfer, staging
//! cleanup, and desktop and clipboard access rather than task-specific repair
//! verbs. Every tool result embeds `{ session_id, target }` so a result can
//! never be mistaken for a different session or a local machine (REQ-F-016),
//! and every input is validated against the same [`limits`] the wire protocol
//! enforces, so a malformed request becomes a tool error rather than a panic.
//!
//! Every tool acts on the customer computer, so every tool requires a stated
//! `purpose`: the AI client's short plain-language reason, validated first
//! (trimmed, one line, 1 to [`limits::MAX_PURPOSE_CHARS`] characters) so a call
//! without a valid one is refused before anything reaches the console. It is
//! forwarded to be audited and shown to the customer beside the real operation;
//! it is the caller's claim, never authority (REQ-F-026, ST-M009-007).

use std::collections::BTreeMap;
use std::sync::Arc;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use cretlink_protocol::{
    CleanupPolicy, CleanupTrigger, ImageFormat, JobId, KeyAction, PointerAction, PointerButton,
    Purpose, RemotePath, SessionId, Shell, TargetIdentity, limits,
};
use rmcp::ServerHandler;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, ContentBlock, ErrorData};
use rmcp::{tool, tool_handler, tool_router};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::target::{
    CallError, CaptureParams, CaptureReport, DownloadParams, ExecParams, TargetError,
    TargetSession, UploadParams,
};

/// The maximum number of output bytes a single `remote_exec` call collects
/// before reporting truncation, capped at 64 chunks (16 MiB).
pub const MAX_EXEC_OUTPUT_BYTES: usize = limits::MAX_CHUNK_BYTES * 64;

/// The longest standard-base64 encoding of a [`limits::MAX_CHUNK_BYTES`]
/// payload: 4 output characters per 3 input bytes, rounded up to a padded
/// quad. Checked against the *encoded* string **before** decoding, so the
/// bound is enforced without first allocating an over-limit buffer; the
/// exact decoded length is re-checked after decoding (padding makes the
/// encoded length coarser by up to two bytes).
pub const MAX_STDIN_BASE64_CHARS: usize = limits::MAX_CHUNK_BYTES.div_ceil(3) * 4;

/// The inclusive minimum signed virtual-desktop coordinate the pointer and
/// scroll tools accept: the negative of [`limits::MAX_DESKTOP_COORD`]. A named
/// const is needed because a `schemars(range(min = ...))` bound must reference a
/// value and the protocol only defines the positive magnitude; the runtime
/// [`check_magnitude`] check remains authoritative for both signs.
const MIN_DESKTOP_COORD: i32 = -limits::MAX_DESKTOP_COORD;

/// The inclusive minimum signed wheel delta the scroll tool accepts: the
/// negative of [`limits::MAX_DESKTOP_SCROLL`] (see [`MIN_DESKTOP_COORD`]).
const MIN_DESKTOP_SCROLL: i32 = -limits::MAX_DESKTOP_SCROLL;

/// The schema description of the required `purpose` argument every tool takes
/// (REQ-F-026, ST-M009-007). It asks the AI client for a reason a
/// non-technical customer understands, because the customer reads it next to
/// the real operation.
const PURPOSE_DESCRIPTION: &str = "Why you are doing this, in one short plain-language sentence \
the customer will read on their screen next to the real operation, for example \
\"Checking free disk space\". Write it for a non-technical person. 1 to 200 \
characters on one line.";

/// An rmcp server that bridges MCP tool calls onto one [`TargetSession`].
#[derive(Clone)]
pub struct McpBridge {
    session: Arc<dyn TargetSession>,
}

impl std::fmt::Debug for McpBridge {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("McpBridge")
            .field("session_id", &self.session.session_id())
            .finish_non_exhaustive()
    }
}

/// Which shell an exec runs in (the MCP-facing mirror of [`Shell`]).
#[derive(Debug, Clone, Copy, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
enum ShellArg {
    /// Windows PowerShell / PowerShell.
    PowerShell,
    /// Command Prompt (`cmd.exe`).
    Cmd,
}

impl From<ShellArg> for Shell {
    fn from(value: ShellArg) -> Self {
        match value {
            ShellArg::PowerShell => Self::PowerShell,
            ShellArg::Cmd => Self::Cmd,
        }
    }
}

/// Arguments for the `remote_exec` tool.
///
/// The schema-level bounds mirror [`limits`]; [`validate_exec`] remains the
/// authoritative runtime check (the env map's entry and length bounds are
/// runtime-only).
#[derive(Debug, Deserialize, JsonSchema)]
struct ExecInput {
    /// The stated purpose (see [`PURPOSE_DESCRIPTION`]).
    #[schemars(
        description = PURPOSE_DESCRIPTION,
        length(min = 1, max = limits::MAX_PURPOSE_CHARS)
    )]
    purpose: String,
    /// The shell that runs the command.
    shell: ShellArg,
    /// The command text (at most 32 KiB).
    #[schemars(length(max = limits::MAX_COMMAND_BYTES))]
    command: String,
    /// Optional working directory (at most 4096 bytes).
    #[serde(default)]
    #[schemars(length(max = limits::MAX_PATH_BYTES))]
    working_dir: Option<String>,
    /// Extra environment variables (at most 32 entries; keys at most 128
    /// bytes, values at most 4096 bytes — enforced at runtime).
    #[serde(default)]
    env: BTreeMap<String, String>,
    /// Optional deadline in milliseconds (at most 24 hours).
    #[serde(default)]
    #[schemars(range(max = limits::MAX_TIMEOUT_MS))]
    timeout_ms: Option<u64>,
    /// Mark the command text sensitive: the technician console then records it
    /// in the audit log as a `[REDACTED]` marker instead of the literal text,
    /// while keeping the action, job id, and outcome. Defaults to false.
    #[serde(default)]
    sensitive: bool,
}

/// Arguments for the `remote_write_stdin` tool.
#[derive(Debug, Deserialize, JsonSchema)]
struct WriteStdinInput {
    /// The stated purpose (see [`PURPOSE_DESCRIPTION`]).
    #[schemars(
        description = PURPOSE_DESCRIPTION,
        length(min = 1, max = limits::MAX_PURPOSE_CHARS)
    )]
    purpose: String,
    /// The job to write to.
    job: u64,
    /// Standard base64 of the raw bytes to write (at most 256 KiB decoded).
    #[schemars(length(max = MAX_STDIN_BASE64_CHARS))]
    data_base64: String,
    /// Mark the input sensitive for the technician console's audit trail.
    /// Stdin content is never written to the audit log in any case; the flag
    /// exists for symmetry with `remote_exec`. Defaults to false.
    #[serde(default)]
    sensitive: bool,
}

/// Arguments for the `remote_cancel` tool.
#[derive(Debug, Deserialize, JsonSchema)]
struct CancelInput {
    /// The stated purpose (see [`PURPOSE_DESCRIPTION`]).
    #[schemars(
        description = PURPOSE_DESCRIPTION,
        length(min = 1, max = limits::MAX_PURPOSE_CHARS)
    )]
    purpose: String,
    /// The job to cancel.
    job: u64,
}

/// A remote path argument, mirroring [`RemotePath`]'s two disjoint authority
/// domains so a tool call can never blur an absolute path with a staging
/// component. Length bounds mirror [`limits`]; the wire protocol performs the
/// authoritative adversarial-path validation (rejecting `..`, UNC, device
/// namespaces, and alternate-data-stream colons) before the agent acts.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum RemotePathArg {
    /// A full path the elevated user can access.
    Absolute {
        /// The path text (at most 4096 bytes).
        #[schemars(length(max = limits::MAX_PATH_BYTES))]
        path: String,
    },
    /// A single filename component resolved under the session staging root.
    Staging {
        /// The artifact filename; one component only (no separators or `..`).
        #[schemars(length(max = limits::MAX_NAME_BYTES))]
        name: String,
    },
}

impl From<RemotePathArg> for RemotePath {
    fn from(value: RemotePathArg) -> Self {
        match value {
            RemotePathArg::Absolute { path } => Self::Absolute { path },
            RemotePathArg::Staging { name } => Self::Staging { name },
        }
    }
}

impl RemotePathArg {
    /// Validates the component lengths against [`limits`] so an over-limit path
    /// becomes a tool error rather than a rejected wire message.
    fn validate(&self, field: &str) -> Result<(), ErrorData> {
        match self {
            Self::Absolute { path } => {
                check(&format!("{field}.path"), path.len(), limits::MAX_PATH_BYTES)
            }
            Self::Staging { name } => {
                check(&format!("{field}.name"), name.len(), limits::MAX_NAME_BYTES)
            }
        }
    }
}

/// The per-artifact cleanup policy (the MCP mirror of [`CleanupPolicy`]).
#[derive(Debug, Clone, Copy, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
enum CleanupPolicyArg {
    /// Remove when the session ends (the default for support artifacts).
    OnSessionEnd,
    /// Remove at the next normal agent shutdown.
    OnShutdown,
    /// Keep until an explicit cleanup with an `all` trigger.
    Keep,
}

impl From<CleanupPolicyArg> for CleanupPolicy {
    fn from(value: CleanupPolicyArg) -> Self {
        match value {
            CleanupPolicyArg::OnSessionEnd => Self::OnSessionEnd,
            CleanupPolicyArg::OnShutdown => Self::OnShutdown,
            CleanupPolicyArg::Keep => Self::Keep,
        }
    }
}

/// Which staged artifacts a cleanup removes (the MCP mirror of
/// [`CleanupTrigger`]).
#[derive(Debug, Clone, Copy, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
enum CleanupTriggerArg {
    /// Remove `on_session_end` artifacts and orphaned part files.
    SessionEnd,
    /// Remove `on_session_end` or `on_shutdown` artifacts and orphaned parts.
    Shutdown,
    /// Purge every staged artifact and part regardless of policy.
    All,
}

impl From<CleanupTriggerArg> for CleanupTrigger {
    fn from(value: CleanupTriggerArg) -> Self {
        match value {
            CleanupTriggerArg::SessionEnd => Self::SessionEnd,
            CleanupTriggerArg::Shutdown => Self::Shutdown,
            CleanupTriggerArg::All => Self::All,
        }
    }
}

/// Arguments for the `remote_stat` tool.
#[derive(Debug, Deserialize, JsonSchema)]
struct StatInput {
    /// The stated purpose (see [`PURPOSE_DESCRIPTION`]).
    #[schemars(
        description = PURPOSE_DESCRIPTION,
        length(min = 1, max = limits::MAX_PURPOSE_CHARS)
    )]
    purpose: String,
    /// The path to stat.
    path: RemotePathArg,
}

/// Arguments for the `remote_list_dir` tool.
#[derive(Debug, Deserialize, JsonSchema)]
struct ListDirInput {
    /// The stated purpose (see [`PURPOSE_DESCRIPTION`]).
    #[schemars(
        description = PURPOSE_DESCRIPTION,
        length(min = 1, max = limits::MAX_PURPOSE_CHARS)
    )]
    purpose: String,
    /// The directory to list.
    path: RemotePathArg,
    /// Resume token: the name of the last entry from the previous page.
    #[serde(default)]
    #[schemars(length(max = limits::MAX_NAME_BYTES))]
    continue_after: Option<String>,
}

/// Arguments for the `remote_make_dir` tool.
#[derive(Debug, Deserialize, JsonSchema)]
struct MakeDirInput {
    /// The stated purpose (see [`PURPOSE_DESCRIPTION`]).
    #[schemars(
        description = PURPOSE_DESCRIPTION,
        length(min = 1, max = limits::MAX_PURPOSE_CHARS)
    )]
    purpose: String,
    /// The directory to create.
    path: RemotePathArg,
}

/// Arguments for the `remote_move` tool.
#[derive(Debug, Deserialize, JsonSchema)]
struct MoveInput {
    /// The stated purpose (see [`PURPOSE_DESCRIPTION`]).
    #[schemars(
        description = PURPOSE_DESCRIPTION,
        length(min = 1, max = limits::MAX_PURPOSE_CHARS)
    )]
    purpose: String,
    /// The source path.
    from: RemotePathArg,
    /// The destination path (same volume).
    to: RemotePathArg,
    /// Whether an existing destination file may be replaced.
    #[serde(default)]
    overwrite: bool,
}

/// Arguments for the `remote_delete` tool.
#[derive(Debug, Deserialize, JsonSchema)]
struct DeleteInput {
    /// The stated purpose (see [`PURPOSE_DESCRIPTION`]).
    #[schemars(
        description = PURPOSE_DESCRIPTION,
        length(min = 1, max = limits::MAX_PURPOSE_CHARS)
    )]
    purpose: String,
    /// The file or reparse point to delete (never a directory).
    path: RemotePathArg,
}

/// Arguments for the `remote_remove_dir` tool.
#[derive(Debug, Deserialize, JsonSchema)]
struct RemoveDirInput {
    /// The stated purpose (see [`PURPOSE_DESCRIPTION`]).
    #[schemars(
        description = PURPOSE_DESCRIPTION,
        length(min = 1, max = limits::MAX_PURPOSE_CHARS)
    )]
    purpose: String,
    /// The empty directory to remove.
    path: RemotePathArg,
}

/// Arguments for the `remote_upload` tool.
#[derive(Debug, Deserialize, JsonSchema)]
struct UploadInput {
    /// The stated purpose (see [`PURPOSE_DESCRIPTION`]).
    #[schemars(
        description = PURPOSE_DESCRIPTION,
        length(min = 1, max = limits::MAX_PURPOSE_CHARS)
    )]
    purpose: String,
    /// The local file on the technician machine to read.
    #[schemars(length(max = limits::MAX_PATH_BYTES))]
    local_path: String,
    /// The staging destination filename on the target (one component).
    #[schemars(length(max = limits::MAX_NAME_BYTES))]
    dest_name: String,
    /// The technician-side origin label bound into the staging manifest.
    #[schemars(length(max = limits::MAX_LABEL_BYTES))]
    source_label: String,
    /// The per-artifact cleanup policy recorded at commit.
    cleanup: CleanupPolicyArg,
    /// Whether an existing staged artifact of that name may be replaced.
    #[serde(default)]
    overwrite: bool,
}

/// Arguments for the `remote_download` tool.
#[derive(Debug, Deserialize, JsonSchema)]
struct DownloadInput {
    /// The stated purpose (see [`PURPOSE_DESCRIPTION`]).
    #[schemars(
        description = PURPOSE_DESCRIPTION,
        length(min = 1, max = limits::MAX_PURPOSE_CHARS)
    )]
    purpose: String,
    /// The source path on the target to read.
    source: RemotePathArg,
    /// The local path on the technician machine to create.
    #[schemars(length(max = limits::MAX_PATH_BYTES))]
    local_path: String,
}

/// Arguments for the `remote_cleanup` tool.
#[derive(Debug, Deserialize, JsonSchema)]
struct CleanupInput {
    /// The stated purpose (see [`PURPOSE_DESCRIPTION`]).
    #[schemars(
        description = PURPOSE_DESCRIPTION,
        length(min = 1, max = limits::MAX_PURPOSE_CHARS)
    )]
    purpose: String,
    /// Which staged artifacts to remove.
    trigger: CleanupTriggerArg,
}

/// The image encoding for `remote_capture_desktop` (the MCP mirror of
/// [`ImageFormat`], ST-M005-004).
#[derive(Debug, Clone, Copy, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
enum ImageFormatArg {
    /// Lossless PNG.
    Png,
    /// Lossy JPEG (honours `quality`).
    Jpeg,
    /// Lossy WebP (honours `quality`).
    Webp,
}

impl From<ImageFormatArg> for ImageFormat {
    fn from(value: ImageFormatArg) -> Self {
        match value {
            ImageFormatArg::Png => Self::Png,
            ImageFormatArg::Jpeg => Self::Jpeg,
            ImageFormatArg::Webp => Self::Webp,
        }
    }
}

/// A pointer action for `remote_desktop_pointer` (the MCP mirror of
/// [`PointerAction`]).
#[derive(Debug, Clone, Copy, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
enum PointerActionArg {
    /// Move the pointer without pressing a button.
    Move,
    /// Press and hold the button.
    Down,
    /// Release the button.
    Up,
    /// A single press-and-release of the button.
    Click,
    /// A double press-and-release of the button.
    DoubleClick,
}

impl From<PointerActionArg> for PointerAction {
    fn from(value: PointerActionArg) -> Self {
        match value {
            PointerActionArg::Move => Self::Move,
            PointerActionArg::Down => Self::Down,
            PointerActionArg::Up => Self::Up,
            PointerActionArg::Click => Self::Click,
            PointerActionArg::DoubleClick => Self::DoubleClick,
        }
    }
}

/// A pointer button for `remote_desktop_pointer` (the MCP mirror of
/// [`PointerButton`]).
#[derive(Debug, Clone, Copy, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
enum PointerButtonArg {
    /// The primary (left) button.
    Left,
    /// The secondary (right) button.
    Right,
    /// The middle (wheel) button.
    Middle,
}

impl From<PointerButtonArg> for PointerButton {
    fn from(value: PointerButtonArg) -> Self {
        match value {
            PointerButtonArg::Left => Self::Left,
            PointerButtonArg::Right => Self::Right,
            PointerButtonArg::Middle => Self::Middle,
        }
    }
}

/// What `remote_desktop_key` does to a single key (the MCP mirror of
/// [`KeyAction`]).
#[derive(Debug, Clone, Copy, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
enum KeyActionArg {
    /// Press and hold the key.
    Down,
    /// Release the key.
    Up,
    /// A single press-and-release of the key.
    Press,
}

impl From<KeyActionArg> for KeyAction {
    fn from(value: KeyActionArg) -> Self {
        match value {
            KeyActionArg::Down => Self::Down,
            KeyActionArg::Up => Self::Up,
            KeyActionArg::Press => Self::Press,
        }
    }
}

/// Arguments for the `remote_capture_desktop` tool.
///
/// The schema-level bounds mirror [`limits`]; the geometry hints are
/// bounds-checked again at runtime before the request reaches the live session,
/// so a malformed hint fails on the call and never touches the target
/// (REQ-N-012).
#[derive(Debug, Deserialize, JsonSchema)]
struct CaptureInput {
    /// The stated purpose (see [`PURPOSE_DESCRIPTION`]).
    #[schemars(
        description = PURPOSE_DESCRIPTION,
        length(min = 1, max = limits::MAX_PURPOSE_CHARS)
    )]
    purpose: String,
    /// The zero-based display index to capture within the session. Defaults to
    /// the primary display (`0`).
    #[serde(default)]
    display: u16,
    /// The desired image encoding.
    format: ImageFormatArg,
    /// Optional maximum output width in pixels; the agent scales the frame to
    /// fit while preserving aspect ratio. `None` captures at native width.
    #[serde(default)]
    #[schemars(range(min = limits::MIN_DESKTOP_DIM, max = limits::MAX_DESKTOP_DIM))]
    max_width: Option<u16>,
    /// Optional maximum output height in pixels (see `max_width`).
    #[serde(default)]
    #[schemars(range(min = limits::MIN_DESKTOP_DIM, max = limits::MAX_DESKTOP_DIM))]
    max_height: Option<u16>,
    /// Optional encoder quality on a 1..=100 scale for a lossy `format`;
    /// ignored for PNG. `None` uses the agent default.
    #[serde(default)]
    #[schemars(range(min = limits::MIN_DESKTOP_QUALITY, max = limits::MAX_DESKTOP_QUALITY))]
    quality: Option<u8>,
}

/// Arguments for the `remote_desktop_pointer` tool.
#[derive(Debug, Deserialize, JsonSchema)]
struct DesktopPointerInput {
    /// The stated purpose (see [`PURPOSE_DESCRIPTION`]).
    #[schemars(
        description = PURPOSE_DESCRIPTION,
        length(min = 1, max = limits::MAX_PURPOSE_CHARS)
    )]
    purpose: String,
    /// What to do at the coordinates.
    action: PointerActionArg,
    /// Which button the action concerns (ignored for `move`).
    button: PointerButtonArg,
    /// The absolute virtual-desktop x coordinate (magnitude at most
    /// [`limits::MAX_DESKTOP_COORD`]).
    #[schemars(range(min = MIN_DESKTOP_COORD, max = limits::MAX_DESKTOP_COORD))]
    x: i32,
    /// The absolute virtual-desktop y coordinate (magnitude at most
    /// [`limits::MAX_DESKTOP_COORD`]).
    #[schemars(range(min = MIN_DESKTOP_COORD, max = limits::MAX_DESKTOP_COORD))]
    y: i32,
}

/// Arguments for the `remote_desktop_scroll` tool.
#[derive(Debug, Deserialize, JsonSchema)]
struct DesktopScrollInput {
    /// The stated purpose (see [`PURPOSE_DESCRIPTION`]).
    #[schemars(
        description = PURPOSE_DESCRIPTION,
        length(min = 1, max = limits::MAX_PURPOSE_CHARS)
    )]
    purpose: String,
    /// The absolute virtual-desktop x origin (magnitude at most
    /// [`limits::MAX_DESKTOP_COORD`]).
    #[schemars(range(min = MIN_DESKTOP_COORD, max = limits::MAX_DESKTOP_COORD))]
    x: i32,
    /// The absolute virtual-desktop y origin (magnitude at most
    /// [`limits::MAX_DESKTOP_COORD`]).
    #[schemars(range(min = MIN_DESKTOP_COORD, max = limits::MAX_DESKTOP_COORD))]
    y: i32,
    /// The horizontal wheel delta in detents (magnitude at most
    /// [`limits::MAX_DESKTOP_SCROLL`]).
    #[schemars(range(min = MIN_DESKTOP_SCROLL, max = limits::MAX_DESKTOP_SCROLL))]
    dx: i32,
    /// The vertical wheel delta in detents (magnitude at most
    /// [`limits::MAX_DESKTOP_SCROLL`]).
    #[schemars(range(min = MIN_DESKTOP_SCROLL, max = limits::MAX_DESKTOP_SCROLL))]
    dy: i32,
}

/// Arguments for the `remote_desktop_key` tool.
#[derive(Debug, Deserialize, JsonSchema)]
struct DesktopKeyInput {
    /// The stated purpose (see [`PURPOSE_DESCRIPTION`]).
    #[schemars(
        description = PURPOSE_DESCRIPTION,
        length(min = 1, max = limits::MAX_PURPOSE_CHARS)
    )]
    purpose: String,
    /// What to do with the key.
    action: KeyActionArg,
    /// The virtual-key token (for example `"enter"`, `"f5"`, `"ctrl"`), at most
    /// [`limits::MAX_DESKTOP_KEY_BYTES`] bytes.
    #[schemars(length(max = limits::MAX_DESKTOP_KEY_BYTES))]
    key: String,
}

/// Arguments for the `remote_desktop_type` tool.
#[derive(Debug, Deserialize, JsonSchema)]
struct DesktopTypeInput {
    /// The stated purpose (see [`PURPOSE_DESCRIPTION`]).
    #[schemars(
        description = PURPOSE_DESCRIPTION,
        length(min = 1, max = limits::MAX_PURPOSE_CHARS)
    )]
    purpose: String,
    /// The text to type, at most [`limits::MAX_DESKTOP_TEXT_BYTES`] bytes.
    #[schemars(length(max = limits::MAX_DESKTOP_TEXT_BYTES))]
    text: String,
}

/// Arguments for the `remote_desktop_hotkey` tool.
#[derive(Debug, Deserialize, JsonSchema)]
struct DesktopHotkeyInput {
    /// The stated purpose (see [`PURPOSE_DESCRIPTION`]).
    #[schemars(
        description = PURPOSE_DESCRIPTION,
        length(min = 1, max = limits::MAX_PURPOSE_CHARS)
    )]
    purpose: String,
    /// The chord elements (for example `["ctrl", "shift", "esc"]`), at most
    /// [`limits::MAX_DESKTOP_HOTKEY_KEYS`] elements, each at most
    /// [`limits::MAX_DESKTOP_KEY_BYTES`] bytes.
    #[schemars(length(max = limits::MAX_DESKTOP_HOTKEY_KEYS))]
    keys: Vec<String>,
}

/// Arguments for the `remote_desktop_wait` tool.
#[derive(Debug, Deserialize, JsonSchema)]
struct DesktopWaitInput {
    /// The stated purpose (see [`PURPOSE_DESCRIPTION`]).
    #[schemars(
        description = PURPOSE_DESCRIPTION,
        length(min = 1, max = limits::MAX_PURPOSE_CHARS)
    )]
    purpose: String,
    /// The pause in milliseconds, at most [`limits::MAX_DESKTOP_WAIT_MS`].
    #[schemars(range(max = limits::MAX_DESKTOP_WAIT_MS))]
    millis: u64,
}

/// Arguments for the `remote_clipboard_set` tool.
#[derive(Debug, Deserialize, JsonSchema)]
struct ClipboardSetInput {
    /// The stated purpose (see [`PURPOSE_DESCRIPTION`]).
    #[schemars(
        description = PURPOSE_DESCRIPTION,
        length(min = 1, max = limits::MAX_PURPOSE_CHARS)
    )]
    purpose: String,
    /// The text to place on the remote clipboard, at most
    /// [`limits::MAX_CLIPBOARD_BYTES`] bytes.
    #[schemars(length(max = limits::MAX_CLIPBOARD_BYTES))]
    text: String,
}

/// Arguments for the tools whose only argument is the stated purpose
/// (`remote_system_info`, `remote_list_jobs`, `remote_list_staged`,
/// `remote_clipboard_get`).
#[derive(Debug, Deserialize, JsonSchema)]
struct PurposeInput {
    /// The stated purpose (see [`PURPOSE_DESCRIPTION`]).
    #[schemars(
        description = PURPOSE_DESCRIPTION,
        length(min = 1, max = limits::MAX_PURPOSE_CHARS)
    )]
    purpose: String,
}

#[tool_router]
impl McpBridge {
    /// Creates a bridge over the given target session.
    #[must_use]
    pub fn new(session: Arc<dyn TargetSession>) -> Self {
        Self { session }
    }

    /// Report the remote target's identity and basic system state.
    #[tool(
        description = "Report the selected remote target's identity (hostname, OS, architecture, \
                       elevation) and whether a reboot is pending. Use this first to confirm which \
                       machine you are acting on."
    )]
    async fn remote_system_info(
        &self,
        Parameters(input): Parameters<PurposeInput>,
    ) -> Result<CallToolResult, ErrorData> {
        // The stated purpose is checked first, so a call without a valid one
        // is refused before anything reaches the console or the target
        // (REQ-F-026, ST-M009-007).
        let purpose = stated_purpose(&input.purpose)?;
        match self.session.system_info(purpose).await {
            Ok(routed) => Ok(ok_envelope(
                routed.session,
                &routed.target,
                routed.call_id,
                json!({
                    "target": to_value(&routed.value.target),
                    "reboot_pending": routed.value.reboot_pending,
                }),
            )),
            Err(err) => Ok(self.err_envelope(&err)),
        }
    }

    /// Run a shell command on the remote target and collect its output.
    #[tool(
        description = "Run a shell command on the selected remote target and return its collected \
                       stdout/stderr (base64), exit code, duration, and honest outcome \
                       (completed, failed, cancelled, timed_out, or unknown on disconnect). \
                       stdout and stderr are captured as two SEPARATE buffers: byte order is \
                       preserved WITHIN each stream, but interleaving order BETWEEN the two \
                       streams is not. Output is single-shot up to a fixed cap: there is no \
                       continuation or resume; when the cap (or a dropped stream) hides later \
                       bytes, `truncated` is set to true rather than the output being continued."
    )]
    async fn remote_exec(
        &self,
        Parameters(input): Parameters<ExecInput>,
    ) -> Result<CallToolResult, ErrorData> {
        // The stated purpose is checked first, so a call without a valid one
        // is refused before anything reaches the console or the target
        // (REQ-F-026, ST-M009-007).
        let purpose = stated_purpose(&input.purpose)?;
        let params = validate_exec(input)?;
        match self
            .session
            .exec(purpose, params, MAX_EXEC_OUTPUT_BYTES)
            .await
        {
            Ok(routed) => {
                let report = routed.value;
                Ok(ok_envelope(
                    routed.session,
                    &routed.target,
                    routed.call_id,
                    json!({
                        "job": report.job.0,
                        "working_dir": report.working_dir,
                        "outcome": to_value(&report.outcome),
                        "exit_code": report.exit_code,
                        "duration_ms": report.duration_ms,
                        "truncated": report.truncated,
                        "stdout_len": report.stdout.len(),
                        "stderr_len": report.stderr.len(),
                        "stdout_base64": BASE64.encode(&report.stdout),
                        "stderr_base64": BASE64.encode(&report.stderr),
                    }),
                ))
            }
            Err(err) => Ok(self.err_envelope(&err)),
        }
    }

    /// Feed bytes to a running job's standard input.
    #[tool(
        description = "Write bytes (given as standard base64) to the standard input of a running \
                       remote job. Use it to answer interactive prompts."
    )]
    async fn remote_write_stdin(
        &self,
        Parameters(input): Parameters<WriteStdinInput>,
    ) -> Result<CallToolResult, ErrorData> {
        // The stated purpose is checked first, so a call without a valid one
        // is refused before anything reaches the console or the target
        // (REQ-F-026, ST-M009-007).
        let purpose = stated_purpose(&input.purpose)?;
        // Bound the *encoded* input first, so an over-limit payload is
        // rejected before any decoded buffer is allocated.
        if input.data_base64.len() > MAX_STDIN_BASE64_CHARS {
            return Err(ErrorData::invalid_params(
                format!(
                    "data_base64 of {} characters exceeds the {}-character limit \
                     ({} bytes decoded)",
                    input.data_base64.len(),
                    MAX_STDIN_BASE64_CHARS,
                    limits::MAX_CHUNK_BYTES
                ),
                None,
            ));
        }
        let data = BASE64.decode(input.data_base64.as_bytes()).map_err(|err| {
            ErrorData::invalid_params(format!("data_base64 is not valid base64: {err}"), None)
        })?;
        // Padding makes the encoded-length check coarser by up to two
        // bytes: re-check the exact decoded length.
        if data.len() > limits::MAX_CHUNK_BYTES {
            return Err(ErrorData::invalid_params(
                format!(
                    "stdin data of {} bytes exceeds the {}-byte limit",
                    data.len(),
                    limits::MAX_CHUNK_BYTES
                ),
                None,
            ));
        }
        let written = data.len();
        match self
            .session
            .write_stdin(purpose, JobId(input.job), data, input.sensitive)
            .await
        {
            Ok(routed) => Ok(ok_envelope(
                routed.session,
                &routed.target,
                routed.call_id,
                json!({ "job": input.job, "written": written }),
            )),
            Err(err) => Ok(self.err_envelope(&err)),
        }
    }

    /// Request cancellation of a running job.
    #[tool(
        description = "Request cancellation of a running remote job by id. Cancellation is \
                       best-effort; confirm the job's terminal outcome via remote_list_jobs or a \
                       follow-up exec."
    )]
    async fn remote_cancel(
        &self,
        Parameters(input): Parameters<CancelInput>,
    ) -> Result<CallToolResult, ErrorData> {
        // The stated purpose is checked first, so a call without a valid one
        // is refused before anything reaches the console or the target
        // (REQ-F-026, ST-M009-007).
        let purpose = stated_purpose(&input.purpose)?;
        match self.session.cancel(purpose, JobId(input.job)).await {
            Ok(routed) => Ok(ok_envelope(
                routed.session,
                &routed.target,
                routed.call_id,
                json!({ "job": input.job, "cancel_requested": true }),
            )),
            Err(err) => Ok(self.err_envelope(&err)),
        }
    }

    /// List the jobs the remote target currently tracks.
    #[tool(
        description = "List the jobs the selected remote target currently tracks, with a command \
                       preview and whether each is still running."
    )]
    async fn remote_list_jobs(
        &self,
        Parameters(input): Parameters<PurposeInput>,
    ) -> Result<CallToolResult, ErrorData> {
        // The stated purpose is checked first, so a call without a valid one
        // is refused before anything reaches the console or the target
        // (REQ-F-026, ST-M009-007).
        let purpose = stated_purpose(&input.purpose)?;
        match self.session.list_jobs(purpose).await {
            Ok(routed) => {
                let jobs: Vec<Value> = routed
                    .value
                    .into_iter()
                    .map(|summary| {
                        json!({
                            "job": summary.job.0,
                            "command_preview": summary.command_preview,
                            "running": summary.running,
                        })
                    })
                    .collect();
                Ok(ok_envelope(
                    routed.session,
                    &routed.target,
                    routed.call_id,
                    json!({ "jobs": jobs }),
                ))
            }
            Err(err) => Ok(self.err_envelope(&err)),
        }
    }

    /// Stat a path on the selected remote target.
    #[tool(
        description = "Stat a path on the selected remote target. Returns the entry (name, kind, size, modified time, read-only) when it exists, or a null entry when it does not. Uses lstat semantics (a reparse point is reported as a symlink, never followed) and never returns file content. `partial_len` reports the bytes already staged for an in-progress upload resume, when one exists."
    )]
    async fn remote_stat(
        &self,
        Parameters(input): Parameters<StatInput>,
    ) -> Result<CallToolResult, ErrorData> {
        // The stated purpose is checked first, so a call without a valid one
        // is refused before anything reaches the console or the target
        // (REQ-F-026, ST-M009-007).
        let purpose = stated_purpose(&input.purpose)?;
        input.path.validate("path")?;
        match self.session.fs_stat(purpose, input.path.into()).await {
            Ok(routed) => Ok(ok_envelope(
                routed.session,
                &routed.target,
                routed.call_id,
                json!({
                    "entry": to_value(&routed.value.entry),
                    "partial_len": routed.value.partial_len,
                }),
            )),
            Err(err) => Ok(self.err_envelope(&err)),
        }
    }

    /// List one page of a directory on the selected remote target.
    #[tool(
        description = "List one page of a directory on the selected remote target (lstat semantics: reparse points are reported as symlinks, never followed). The page size is bounded by the protocol; when `end` is false, fetch the next page by passing the last entry's `name` as `continue_after`."
    )]
    async fn remote_list_dir(
        &self,
        Parameters(input): Parameters<ListDirInput>,
    ) -> Result<CallToolResult, ErrorData> {
        // The stated purpose is checked first, so a call without a valid one
        // is refused before anything reaches the console or the target
        // (REQ-F-026, ST-M009-007).
        let purpose = stated_purpose(&input.purpose)?;
        input.path.validate("path")?;
        if let Some(after) = &input.continue_after {
            check("continue_after", after.len(), limits::MAX_NAME_BYTES)?;
        }
        match self
            .session
            .fs_list(purpose, input.path.into(), input.continue_after)
            .await
        {
            Ok(routed) => Ok(ok_envelope(
                routed.session,
                &routed.target,
                routed.call_id,
                json!({
                    "entries": to_value(&routed.value.entries),
                    "end": routed.value.end,
                }),
            )),
            Err(err) => Ok(self.err_envelope(&err)),
        }
    }

    /// Create a directory on the selected remote target.
    #[tool(description = "Create a directory on the selected remote target.")]
    async fn remote_make_dir(
        &self,
        Parameters(input): Parameters<MakeDirInput>,
    ) -> Result<CallToolResult, ErrorData> {
        // The stated purpose is checked first, so a call without a valid one
        // is refused before anything reaches the console or the target
        // (REQ-F-026, ST-M009-007).
        let purpose = stated_purpose(&input.purpose)?;
        input.path.validate("path")?;
        match self.session.make_dir(purpose, input.path.into()).await {
            Ok(routed) => Ok(ok_envelope(
                routed.session,
                &routed.target,
                routed.call_id,
                json!({ "created": true }),
            )),
            Err(err) => Ok(self.err_envelope(&err)),
        }
    }

    /// Move (rename) a path on the selected remote target.
    #[tool(
        description = "Move (rename) a path on the selected remote target. Same volume only; set `overwrite` to true to replace an existing destination file (an existing destination directory is never replaced)."
    )]
    async fn remote_move(
        &self,
        Parameters(input): Parameters<MoveInput>,
    ) -> Result<CallToolResult, ErrorData> {
        // The stated purpose is checked first, so a call without a valid one
        // is refused before anything reaches the console or the target
        // (REQ-F-026, ST-M009-007).
        let purpose = stated_purpose(&input.purpose)?;
        input.from.validate("from")?;
        input.to.validate("to")?;
        match self
            .session
            .fs_move(purpose, input.from.into(), input.to.into(), input.overwrite)
            .await
        {
            Ok(routed) => Ok(ok_envelope(
                routed.session,
                &routed.target,
                routed.call_id,
                json!({ "moved": true }),
            )),
            Err(err) => Ok(self.err_envelope(&err)),
        }
    }

    /// Delete a file or reparse point on the selected remote target.
    #[tool(
        description = "Delete a file or reparse point on the selected remote target. Never deletes a directory; use remote_remove_dir for an empty directory."
    )]
    async fn remote_delete(
        &self,
        Parameters(input): Parameters<DeleteInput>,
    ) -> Result<CallToolResult, ErrorData> {
        // The stated purpose is checked first, so a call without a valid one
        // is refused before anything reaches the console or the target
        // (REQ-F-026, ST-M009-007).
        let purpose = stated_purpose(&input.purpose)?;
        input.path.validate("path")?;
        match self.session.fs_delete(purpose, input.path.into()).await {
            Ok(routed) => Ok(ok_envelope(
                routed.session,
                &routed.target,
                routed.call_id,
                json!({ "deleted": true }),
            )),
            Err(err) => Ok(self.err_envelope(&err)),
        }
    }

    /// Remove an empty directory on the selected remote target.
    #[tool(
        description = "Remove an empty directory on the selected remote target. Fails if the directory is not empty."
    )]
    async fn remote_remove_dir(
        &self,
        Parameters(input): Parameters<RemoveDirInput>,
    ) -> Result<CallToolResult, ErrorData> {
        // The stated purpose is checked first, so a call without a valid one
        // is refused before anything reaches the console or the target
        // (REQ-F-026, ST-M009-007).
        let purpose = stated_purpose(&input.purpose)?;
        input.path.validate("path")?;
        match self.session.remove_dir(purpose, input.path.into()).await {
            Ok(routed) => Ok(ok_envelope(
                routed.session,
                &routed.target,
                routed.call_id,
                json!({ "removed": true }),
            )),
            Err(err) => Ok(self.err_envelope(&err)),
        }
    }

    /// Upload a local file into target staging and commit it verified.
    #[tool(
        description = "Upload a local technician file into the selected target's tracked staging area and commit it, verified by length and SHA-256 before an atomic rename. The destination is a single staging filename (`dest_name`); compose absolute placement with a later remote_move. Reports the honest terminal outcome, the committed absolute path, total and sent bytes, the verified digest as lowercase hex, and any verified-resume or restart detail. Never returns file content."
    )]
    async fn remote_upload(
        &self,
        Parameters(input): Parameters<UploadInput>,
    ) -> Result<CallToolResult, ErrorData> {
        // The stated purpose is checked first, so a call without a valid one
        // is refused before anything reaches the console or the target
        // (REQ-F-026, ST-M009-007).
        let purpose = stated_purpose(&input.purpose)?;
        check("local_path", input.local_path.len(), limits::MAX_PATH_BYTES)?;
        check("dest_name", input.dest_name.len(), limits::MAX_NAME_BYTES)?;
        check(
            "source_label",
            input.source_label.len(),
            limits::MAX_LABEL_BYTES,
        )?;
        let params = UploadParams {
            local_path: input.local_path,
            dest: RemotePath::Staging {
                name: input.dest_name,
            },
            source_label: input.source_label,
            cleanup: input.cleanup.into(),
            overwrite: input.overwrite,
        };
        match self.session.upload(purpose, params).await {
            Ok(routed) => {
                let report = routed.value;
                Ok(ok_envelope(
                    routed.session,
                    &routed.target,
                    routed.call_id,
                    json!({
                        "outcome": to_value(&report.outcome),
                        "remote_path": report.remote_path,
                        "len": report.len,
                        "sent": report.sent,
                        "digest_hex": hex_lower(&report.digest),
                        "resumed_from": report.resumed_from,
                        "restarted": report.restarted,
                        "error": to_value(&report.error),
                    }),
                ))
            }
            Err(err) => Ok(self.err_envelope(&err)),
        }
    }

    /// Download a target file to a local path, verified by digest.
    #[tool(
        description = "Download a file from the selected remote target to a local technician path, verified by whole-file SHA-256. Reports the honest terminal outcome, the local path written, total and sent bytes, the verified digest as lowercase hex, and any verified-resume or restart detail."
    )]
    async fn remote_download(
        &self,
        Parameters(input): Parameters<DownloadInput>,
    ) -> Result<CallToolResult, ErrorData> {
        // The stated purpose is checked first, so a call without a valid one
        // is refused before anything reaches the console or the target
        // (REQ-F-026, ST-M009-007).
        let purpose = stated_purpose(&input.purpose)?;
        input.source.validate("source")?;
        check("local_path", input.local_path.len(), limits::MAX_PATH_BYTES)?;
        let params = DownloadParams {
            source: input.source.into(),
            local_path: input.local_path,
        };
        match self.session.download(purpose, params).await {
            Ok(routed) => {
                let report = routed.value;
                Ok(ok_envelope(
                    routed.session,
                    &routed.target,
                    routed.call_id,
                    json!({
                        "outcome": to_value(&report.outcome),
                        "local_path": report.local_path,
                        "len": report.len,
                        "sent": report.sent,
                        "digest_hex": hex_lower(&report.digest),
                        "resumed_from": report.resumed_from,
                        "restarted": report.restarted,
                        "error": to_value(&report.error),
                    }),
                ))
            }
            Err(err) => Ok(self.err_envelope(&err)),
        }
    }

    /// List the staged artifacts the selected remote target tracks.
    #[tool(
        description = "List the staged artifacts the selected remote target currently tracks: the committed absolute path, the technician source label bound at commit, size, the verified digest as lowercase hex, the cleanup policy, and creation time."
    )]
    async fn remote_list_staged(
        &self,
        Parameters(input): Parameters<PurposeInput>,
    ) -> Result<CallToolResult, ErrorData> {
        // The stated purpose is checked first, so a call without a valid one
        // is refused before anything reaches the console or the target
        // (REQ-F-026, ST-M009-007).
        let purpose = stated_purpose(&input.purpose)?;
        match self.session.list_staged(purpose).await {
            Ok(routed) => {
                let staged: Vec<Value> = routed
                    .value
                    .into_iter()
                    .map(|entry| {
                        json!({
                            "canonical_path": entry.canonical_path,
                            "source": entry.source,
                            "len": entry.len,
                            "algo": to_value(&entry.algo),
                            "digest_hex": hex_lower(entry.digest.as_bytes()),
                            "cleanup": to_value(&entry.cleanup),
                            "created_ms": entry.created_ms,
                        })
                    })
                    .collect();
                Ok(ok_envelope(
                    routed.session,
                    &routed.target,
                    routed.call_id,
                    json!({ "staged": staged }),
                ))
            }
            Err(err) => Ok(self.err_envelope(&err)),
        }
    }

    /// Run a staging cleanup on the selected remote target.
    #[tool(
        description = "Run a staging cleanup on the selected remote target for a trigger: `session_end` removes on-session-end artifacts and orphaned parts, `shutdown` also removes on-shutdown artifacts, and `all` purges every staged artifact regardless of policy. Reports how many artifacts were removed and any honest per-artifact failures, whose records are kept for retry."
    )]
    async fn remote_cleanup(
        &self,
        Parameters(input): Parameters<CleanupInput>,
    ) -> Result<CallToolResult, ErrorData> {
        // The stated purpose is checked first, so a call without a valid one
        // is refused before anything reaches the console or the target
        // (REQ-F-026, ST-M009-007).
        let purpose = stated_purpose(&input.purpose)?;
        match self.session.cleanup(purpose, input.trigger.into()).await {
            Ok(routed) => Ok(ok_envelope(
                routed.session,
                &routed.target,
                routed.call_id,
                json!({
                    "removed": routed.value.removed,
                    "failed": to_value(&routed.value.failed),
                }),
            )),
            Err(err) => Ok(self.err_envelope(&err)),
        }
    }

    /// Capture one screenshot of the selected remote target's desktop.
    #[tool(
        description = "Capture one screenshot of the selected remote target's interactive desktop. Returns the honest desktop state and frame identity (WTS session, window-station\\desktop, display index, logical and physical geometry, DPI, and capture timestamp in Unix milliseconds). A live image is present only when the state is `active`; a locked, switched, disconnected, secure, or no-input desktop returns metadata with no image rather than a stale or fabricated frame. A small frame is returned inline as MCP image content; a larger frame is staged on the target and referenced by the `staged` path (read it back with `remote_download`)."
    )]
    async fn remote_capture_desktop(
        &self,
        Parameters(input): Parameters<CaptureInput>,
    ) -> Result<CallToolResult, ErrorData> {
        // The stated purpose is checked first, so a call without a valid one
        // is refused before anything reaches the console or the target
        // (REQ-F-026, ST-M009-007).
        let purpose = stated_purpose(&input.purpose)?;
        if let Some(width) = input.max_width {
            check_range(
                "max_width",
                u64::from(width),
                u64::from(limits::MIN_DESKTOP_DIM),
                u64::from(limits::MAX_DESKTOP_DIM),
            )?;
        }
        if let Some(height) = input.max_height {
            check_range(
                "max_height",
                u64::from(height),
                u64::from(limits::MIN_DESKTOP_DIM),
                u64::from(limits::MAX_DESKTOP_DIM),
            )?;
        }
        if let Some(quality) = input.quality {
            check_range(
                "quality",
                u64::from(quality),
                u64::from(limits::MIN_DESKTOP_QUALITY),
                u64::from(limits::MAX_DESKTOP_QUALITY),
            )?;
        }
        let params = CaptureParams {
            display: input.display,
            format: input.format.into(),
            max_width: input.max_width,
            max_height: input.max_height,
            quality: input.quality,
        };
        match self.session.capture_desktop(purpose, params).await {
            Ok(routed) => Ok(capture_envelope(
                routed.session,
                &routed.target,
                routed.call_id,
                routed.value,
            )),
            Err(err) => Ok(self.err_envelope(&err)),
        }
    }

    /// Inject a pointer action on the remote target's desktop.
    #[tool(
        description = "Inject a pointer action (move, button down, button up, click, or double-click) at absolute virtual-desktop coordinates on the selected remote target. The button is ignored for a `move`. Refused unless a consented session is live and the desktop is `active`. Returns the routed target and session identity."
    )]
    async fn remote_desktop_pointer(
        &self,
        Parameters(input): Parameters<DesktopPointerInput>,
    ) -> Result<CallToolResult, ErrorData> {
        // The stated purpose is checked first, so a call without a valid one
        // is refused before anything reaches the console or the target
        // (REQ-F-026, ST-M009-007).
        let purpose = stated_purpose(&input.purpose)?;
        check_magnitude("x", input.x, limits::MAX_DESKTOP_COORD)?;
        check_magnitude("y", input.y, limits::MAX_DESKTOP_COORD)?;
        match self
            .session
            .desktop_pointer(
                purpose,
                input.action.into(),
                input.button.into(),
                input.x,
                input.y,
            )
            .await
        {
            Ok(routed) => Ok(ok_envelope(
                routed.session,
                &routed.target,
                routed.call_id,
                json!({}),
            )),
            Err(err) => Ok(self.err_envelope(&err)),
        }
    }

    /// Inject a scroll action on the remote target's desktop.
    #[tool(
        description = "Inject a scroll action at an absolute virtual-desktop origin on the selected remote target, with horizontal and vertical wheel deltas in detents. Refused unless a consented session is live and the desktop is `active`. Returns the routed target and session identity."
    )]
    async fn remote_desktop_scroll(
        &self,
        Parameters(input): Parameters<DesktopScrollInput>,
    ) -> Result<CallToolResult, ErrorData> {
        // The stated purpose is checked first, so a call without a valid one
        // is refused before anything reaches the console or the target
        // (REQ-F-026, ST-M009-007).
        let purpose = stated_purpose(&input.purpose)?;
        check_magnitude("x", input.x, limits::MAX_DESKTOP_COORD)?;
        check_magnitude("y", input.y, limits::MAX_DESKTOP_COORD)?;
        check_magnitude("dx", input.dx, limits::MAX_DESKTOP_SCROLL)?;
        check_magnitude("dy", input.dy, limits::MAX_DESKTOP_SCROLL)?;
        match self
            .session
            .desktop_scroll(purpose, input.x, input.y, input.dx, input.dy)
            .await
        {
            Ok(routed) => Ok(ok_envelope(
                routed.session,
                &routed.target,
                routed.call_id,
                json!({}),
            )),
            Err(err) => Ok(self.err_envelope(&err)),
        }
    }

    /// Inject a single-key action on the remote target's desktop.
    #[tool(
        description = "Inject a single virtual-key action (down, up, or a press) on the selected remote target's desktop, naming the key (for example \"enter\", \"f5\", \"ctrl\"). This only sends input; it is not a keylogger. Refused unless a consented session is live and the desktop is `active`."
    )]
    async fn remote_desktop_key(
        &self,
        Parameters(input): Parameters<DesktopKeyInput>,
    ) -> Result<CallToolResult, ErrorData> {
        // The stated purpose is checked first, so a call without a valid one
        // is refused before anything reaches the console or the target
        // (REQ-F-026, ST-M009-007).
        let purpose = stated_purpose(&input.purpose)?;
        check("key", input.key.len(), limits::MAX_DESKTOP_KEY_BYTES)?;
        match self
            .session
            .desktop_key(purpose, input.action.into(), input.key)
            .await
        {
            Ok(routed) => Ok(ok_envelope(
                routed.session,
                &routed.target,
                routed.call_id,
                json!({}),
            )),
            Err(err) => Ok(self.err_envelope(&err)),
        }
    }

    /// Type a Unicode string on the remote target's desktop.
    #[tool(
        description = "Type a Unicode string as a sequence of key events on the selected remote target's desktop. Refused unless a consented session is live and the desktop is `active`. Returns the routed target and session identity."
    )]
    async fn remote_desktop_type(
        &self,
        Parameters(input): Parameters<DesktopTypeInput>,
    ) -> Result<CallToolResult, ErrorData> {
        // The stated purpose is checked first, so a call without a valid one
        // is refused before anything reaches the console or the target
        // (REQ-F-026, ST-M009-007).
        let purpose = stated_purpose(&input.purpose)?;
        check("text", input.text.len(), limits::MAX_DESKTOP_TEXT_BYTES)?;
        match self.session.desktop_type(purpose, input.text).await {
            Ok(routed) => Ok(ok_envelope(
                routed.session,
                &routed.target,
                routed.call_id,
                json!({}),
            )),
            Err(err) => Ok(self.err_envelope(&err)),
        }
    }

    /// Send a hotkey chord on the remote target's desktop.
    #[tool(
        description = "Send a hotkey chord (a modifier-plus-key combination, for example [\"ctrl\", \"shift\", \"esc\"]) as a single press on the selected remote target's desktop: each element is held down in order, then released in reverse. Refused unless a consented session is live and the desktop is `active`."
    )]
    async fn remote_desktop_hotkey(
        &self,
        Parameters(input): Parameters<DesktopHotkeyInput>,
    ) -> Result<CallToolResult, ErrorData> {
        // The stated purpose is checked first, so a call without a valid one
        // is refused before anything reaches the console or the target
        // (REQ-F-026, ST-M009-007).
        let purpose = stated_purpose(&input.purpose)?;
        check(
            "keys count",
            input.keys.len(),
            limits::MAX_DESKTOP_HOTKEY_KEYS,
        )?;
        for key in &input.keys {
            check("hotkey key", key.len(), limits::MAX_DESKTOP_KEY_BYTES)?;
        }
        match self.session.desktop_hotkey(purpose, input.keys).await {
            Ok(routed) => Ok(ok_envelope(
                routed.session,
                &routed.target,
                routed.call_id,
                json!({}),
            )),
            Err(err) => Ok(self.err_envelope(&err)),
        }
    }

    /// Pause the remote input pipeline for a bounded interval.
    #[tool(
        description = "Pause the selected remote target's input pipeline for a bounded interval in milliseconds, so a scripted action sequence can pace itself. Refused unless a consented session is live. Returns the routed target and session identity."
    )]
    async fn remote_desktop_wait(
        &self,
        Parameters(input): Parameters<DesktopWaitInput>,
    ) -> Result<CallToolResult, ErrorData> {
        // The stated purpose is checked first, so a call without a valid one
        // is refused before anything reaches the console or the target
        // (REQ-F-026, ST-M009-007).
        let purpose = stated_purpose(&input.purpose)?;
        if input.millis > limits::MAX_DESKTOP_WAIT_MS {
            return Err(ErrorData::invalid_params(
                format!(
                    "millis of {} exceeds the {}-ms limit",
                    input.millis,
                    limits::MAX_DESKTOP_WAIT_MS
                ),
                None,
            ));
        }
        match self.session.desktop_wait(purpose, input.millis).await {
            Ok(routed) => Ok(ok_envelope(
                routed.session,
                &routed.target,
                routed.call_id,
                json!({}),
            )),
            Err(err) => Ok(self.err_envelope(&err)),
        }
    }

    /// Read the remote target's clipboard text.
    #[tool(
        description = "Read the selected remote target's clipboard text. Clipboard exchange is explicit-only and audited. Returns `text` (or null when the clipboard holds no text) alongside the routed target and session identity. Refused unless a consented session is live."
    )]
    async fn remote_clipboard_get(
        &self,
        Parameters(input): Parameters<PurposeInput>,
    ) -> Result<CallToolResult, ErrorData> {
        // The stated purpose is checked first, so a call without a valid one
        // is refused before anything reaches the console or the target
        // (REQ-F-026, ST-M009-007).
        let purpose = stated_purpose(&input.purpose)?;
        match self.session.clipboard_get(purpose).await {
            Ok(routed) => Ok(ok_envelope(
                routed.session,
                &routed.target,
                routed.call_id,
                json!({ "text": routed.value }),
            )),
            Err(err) => Ok(self.err_envelope(&err)),
        }
    }

    /// Set the remote target's clipboard text.
    #[tool(
        description = "Set the selected remote target's clipboard text. Clipboard exchange is explicit-only and audited; CretLink never places session secrets or provider credentials on the remote clipboard. Refused unless a consented session is live."
    )]
    async fn remote_clipboard_set(
        &self,
        Parameters(input): Parameters<ClipboardSetInput>,
    ) -> Result<CallToolResult, ErrorData> {
        // The stated purpose is checked first, so a call without a valid one
        // is refused before anything reaches the console or the target
        // (REQ-F-026, ST-M009-007).
        let purpose = stated_purpose(&input.purpose)?;
        check("text", input.text.len(), limits::MAX_CLIPBOARD_BYTES)?;
        match self.session.clipboard_set(purpose, input.text).await {
            Ok(routed) => Ok(ok_envelope(
                routed.session,
                &routed.target,
                routed.call_id,
                json!({}),
            )),
            Err(err) => Ok(self.err_envelope(&err)),
        }
    }

    /// Builds a tool-level error result labelled with the bridge's best-effort
    /// last-known identity. An error carries no routed identity of its own (a
    /// fail-closed no-session reply, or a lost IPC link), so this is the one
    /// place a cached identity is used — and only ever on an `ok: false` result,
    /// never to attribute a success (REQ-F-016).
    fn err_envelope(&self, err: &CallError) -> CallToolResult {
        err_envelope(
            self.session.session_id(),
            &self.session.target(),
            err.call_id,
            &err.error,
        )
    }
}

#[tool_handler]
impl ServerHandler for McpBridge {}

/// Validates raw tool input against the shared [`limits`] and builds
/// [`ExecParams`]. Every violation becomes an `invalid_params` tool error so
/// the caller sees exactly which bound it broke, and the bridge never panics
/// on hostile input.
fn validate_exec(input: ExecInput) -> Result<ExecParams, ErrorData> {
    check("command", input.command.len(), limits::MAX_COMMAND_BYTES)?;
    if let Some(dir) = &input.working_dir {
        check("working_dir", dir.len(), limits::MAX_PATH_BYTES)?;
        // Mirror the file tools: apply the protocol's adversarial remote-path
        // rules bridge-locally, so a traversal/UNC/device/drive-relative
        // working directory becomes an `invalid_params` tool error before
        // anything crosses the IPC (ST-M003-003, REQ-N-012).
        cretlink_protocol::validate_remote_absolute_path("working_dir", dir)
            .map_err(|error| ErrorData::invalid_params(error.to_string(), None))?;
    }
    check("env entry count", input.env.len(), limits::MAX_ENV_ENTRIES)?;
    for (key, value) in &input.env {
        check("env key", key.len(), limits::MAX_ENV_KEY)?;
        check("env value", value.len(), limits::MAX_ENV_VALUE)?;
    }
    if let Some(timeout) = input.timeout_ms
        && timeout > limits::MAX_TIMEOUT_MS
    {
        return Err(ErrorData::invalid_params(
            format!(
                "timeout_ms of {timeout} exceeds the {}-ms limit",
                limits::MAX_TIMEOUT_MS
            ),
            None,
        ));
    }
    Ok(ExecParams {
        shell: input.shell.into(),
        command: input.command,
        working_dir: input.working_dir,
        env: input.env,
        timeout_ms: input.timeout_ms,
        sensitive: input.sensitive,
    })
}

/// Validates a tool call's stated purpose (REQ-F-026, ST-M009-007): trimmed,
/// one line, 1 to [`limits::MAX_PURPOSE_CHARS`] characters. A violation is an
/// `invalid_params` tool error whose message tells the AI client what to send,
/// raised before the call reaches the console, so nothing runs on the target.
fn stated_purpose(raw: &str) -> Result<Purpose, ErrorData> {
    Purpose::new(raw).map_err(|err| ErrorData::invalid_params(format!("purpose: {err}"), None))
}

fn check(field: &str, len: usize, max: usize) -> Result<(), ErrorData> {
    if len > max {
        Err(ErrorData::invalid_params(
            format!("{field} of {len} bytes exceeds the {max}-byte limit"),
            None,
        ))
    } else {
        Ok(())
    }
}

/// Rejects a value outside an inclusive `min..=max` range as an
/// `invalid_params` tool error, mirroring the protocol's desktop dimension and
/// quality bounds bridge-locally so a malformed capture hint fails on the call
/// (ST-M005-004, REQ-N-012).
fn check_range(field: &str, value: u64, min: u64, max: u64) -> Result<(), ErrorData> {
    if value < min || value > max {
        Err(ErrorData::invalid_params(
            format!("{field} of {value} is outside the {min}..={max} range"),
            None,
        ))
    } else {
        Ok(())
    }
}

/// Rejects a signed coordinate or scroll delta whose **magnitude** exceeds
/// `max` (ST-M005-002). Comparing on magnitude bounds a negative multi-monitor
/// coordinate symmetrically and treats [`i32::MIN`] as over-limit, matching the
/// wire protocol's `check_desktop_magnitude`.
fn check_magnitude(field: &str, value: i32, max: i32) -> Result<(), ErrorData> {
    let magnitude = u64::from(value.unsigned_abs());
    if magnitude > max as u64 {
        Err(ErrorData::invalid_params(
            format!("{field} magnitude of {magnitude} exceeds the {max} limit"),
            None,
        ))
    } else {
        Ok(())
    }
}

/// Maps a captured frame's [`ImageFormat`] to the MIME type carried by the
/// inline MCP image content block, so a client renders the screenshot without
/// guessing the encoding (ST-M005-004).
fn image_mime(format: ImageFormat) -> &'static str {
    match format {
        ImageFormat::Png => "image/png",
        ImageFormat::Jpeg => "image/jpeg",
        ImageFormat::Webp => "image/webp",
    }
}

/// Builds the result for `remote_capture_desktop`. The frame's identity,
/// geometry, honest state, and timestamp are always present; the pixels are
/// present only for an `active` desktop, either inline as a bounded MCP image
/// content block or as a staged-artifact reference above the inline threshold
/// (ST-M005-004, REQ-N-008). Like [`ok_envelope`], the body is returned both as
/// pretty JSON text and as structured content, and it embeds the routed
/// session, target, and audit `call_id`.
fn capture_envelope(
    session: SessionId,
    target: &TargetIdentity,
    call_id: u64,
    report: CaptureReport,
) -> CallToolResult {
    let inline = report
        .image
        .as_ref()
        .map(|bytes| (BASE64.encode(bytes.as_bytes()), bytes.len()));
    let result = json!({
        "meta": to_value(&report.meta),
        "image_inline": inline.is_some(),
        "image_bytes": inline.as_ref().map(|(_, len)| *len),
        "staged": to_value(&report.staged),
    });
    let body = json!({
        "session_id": session.to_string(),
        "target": to_value(target),
        "call_id": call_id,
        "ok": true,
        "result": result,
    });
    let text = serde_json::to_string_pretty(&body).unwrap_or_default();
    let mut content = vec![ContentBlock::text(text)];
    if let Some((encoded, _)) = inline {
        content.push(ContentBlock::image(
            encoded,
            image_mime(report.meta.format).to_string(),
        ));
    }
    let mut out = CallToolResult::success(content);
    out.structured_content = Some(body);
    out
}

/// Serializes a value, degrading to JSON `null` on the impossible error path
/// rather than unwrapping (the input is always a plain serializable type).
fn to_value<T: serde::Serialize>(value: &T) -> Value {
    serde_json::to_value(value).unwrap_or(Value::Null)
}

/// Renders bytes as a lowercase hexadecimal string. File and transfer digests
/// are surfaced as hex (rather than the base64 the wire uses for raw output) so
/// a technician can compare them directly against the usual tooling.
fn hex_lower(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        // Writing to a String is infallible; ignore the formatter Result.
        let _ = write!(out, "{byte:02x}");
    }
    out
}

/// Builds a successful tool result whose body embeds the session identity,
/// [`TargetIdentity`], and the console-minted audit `call_id` alongside the
/// operation payload (REQ-F-016, ST-M004-006). The `call_id` is the same
/// monotone id the console wrote to the audit record for this routed call, so
/// the AI transcript maps one-to-one onto the audit trail. The body is returned
/// both as pretty JSON text and as structured content.
fn ok_envelope(
    session: SessionId,
    target: &TargetIdentity,
    call_id: u64,
    result: Value,
) -> CallToolResult {
    let body = json!({
        "session_id": session.to_string(),
        "target": to_value(target),
        "call_id": call_id,
        "ok": true,
        "result": result,
    });
    let text = serde_json::to_string_pretty(&body).unwrap_or_default();
    let mut out = CallToolResult::success(vec![ContentBlock::text(text)]);
    out.structured_content = Some(body);
    out
}

/// Builds a tool-level error result that still carries session identity and
/// target (REQ-F-016) plus the honest failure category, so a lost or failed
/// session is never silently dropped or mistaken for success. When the console
/// reported a `call_id` for the failed call, it is embedded too, so even a
/// failed call correlates one-to-one with its audit record (ST-M004-006).
fn err_envelope(
    session: SessionId,
    target: &TargetIdentity,
    call_id: Option<u64>,
    err: &TargetError,
) -> CallToolResult {
    let mut body = json!({
        "session_id": session.to_string(),
        "target": to_value(target),
        "ok": false,
        "error": err.to_string(),
        "error_kind": err.kind(),
    });
    if let Some(call_id) = call_id {
        body["call_id"] = json!(call_id);
    }
    // A remote failure additionally carries the agent's machine-readable
    // `ErrorCode` as a structured snake_case token (for example
    // "path_not_found"), so missing paths, denied access, unsupported
    // features, and disconnected targets are distinguishable without string
    // matching (ST-M003-003, REQ-N-015). Other kinds omit the field.
    if let TargetError::Remote { code, .. } = err {
        body["error_code"] = to_value(code);
    }
    let text = serde_json::to_string_pretty(&body).unwrap_or_default();
    let mut out = CallToolResult::error(vec![ContentBlock::text(text)]);
    out.structured_content = Some(body);
    out
}
