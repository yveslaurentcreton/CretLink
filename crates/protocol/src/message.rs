//! The top-level wire message and its nested schemas.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::account::AccountName;
use crate::b64::B64Bytes;
use crate::display_name::DisplayName;
use crate::error::ProtocolError;
use crate::ids::{JobId, RequestId, SessionId};
use crate::limits;
use crate::pairing::{SAC_COMMITMENT_LEN, SAC_NONCE_LEN};
use crate::purpose::Purpose;
use crate::version::ProtocolVersion;

/// Which side of the session a `Hello` comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    /// The technician-side console (or the MCP bridge acting through it).
    Console,
    /// The customer-side support agent (real or simulated).
    Agent,
}

/// Which local actor on the console side drives a console→agent
/// [`Message::Request`]: the manual technician console, the AI agent via the
/// local MCP bridge, or (reserved) an interactive technician terminal.
///
/// It is carried so the agent — and thus the attended customer surface — can
/// truthfully attribute each operation to the AI or the technician (REQ-F-017,
/// REQ-S-002). Origin grants **no** authority of its own: routing, consent, and
/// fail-closed behaviour are unchanged; it is attribution only.
///
/// Additive since protocol minor `3`: the field defaults to [`Origin::Console`]
/// and is omitted on the wire at that default, so a same-major peer on an older
/// minor still decodes every console-origin request (REQ-N-012).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Origin {
    /// The manual technician console (the default when the field is absent).
    #[default]
    Console,
    /// The AI agent, driving through the local MCP bridge.
    Mcp,
    /// The technician's interactive terminal (ST-M003-009), distinct from the
    /// AI bridge (REQ-S-023).
    Terminal,
}

impl Origin {
    /// Whether this is the default [`Origin::Console`]. Used to omit the field
    /// on the wire at its default so an older-minor peer still decodes a
    /// console-origin request (REQ-N-012).
    #[must_use]
    pub fn is_console(&self) -> bool {
        matches!(self, Self::Console)
    }
}

/// Identity of the remote target, exposed before tools become callable
/// (REQ-F-016).
///
/// A simulated target **must** set `simulated` to `true` and use the
/// hostname `SIMULATED-TARGET` so simulated results can never be mistaken
/// for a real customer computer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TargetIdentity {
    /// Target hostname.
    pub hostname: String,
    /// Operating system version string.
    pub os_version: String,
    /// CPU architecture, for example `x86_64`.
    pub arch: String,
    /// Whether the agent runs elevated.
    pub elevated: bool,
    /// Whether this target is a simulator rather than a real computer.
    pub simulated: bool,
    /// Version of the agent software on the target.
    pub agent_version: String,
    /// The account the agent's process runs as, `DOMAIN\user`: the customer's
    /// own account name, shown to the technician in the live identity line
    /// (ST-M009-008, REQ-F-016). Additive in minor 11; omitted when unknown and
    /// validated on decode ([`AccountName`]). It is not part of the
    /// reboot-resume [`crate::TargetBinding`], whose pre-image is frozen.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account: Option<AccountName>,
}

/// Which shell executes an `Exec` command.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Shell {
    /// Windows PowerShell / PowerShell.
    PowerShell,
    /// Command Prompt (`cmd.exe`).
    Cmd,
}

// --- File operations and transfer (ADR-0012, ST-M003-004/005) ---

/// A path on the remote target, addressed as one of two disjoint authority
/// domains so an operation can never blur them (ADR-0012).
///
/// [`RemotePath::Absolute`] carries a full target path and receives the full
/// adversarial-path validation ([`Message::validate`] rejects `..`, UNC and
/// device namespaces, reserved device names, and alternate-data-stream colons on
/// **both** ends). [`RemotePath::Staging`] carries a single filename component;
/// the agent resolves it under the session's own staging root, so a staging
/// reference is structurally unable to escape that root.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum RemotePath {
    /// A full path the elevated user can access (at most
    /// [`limits::MAX_PATH_BYTES`] bytes, [`limits::MAX_PATH_DEPTH`] components).
    Absolute {
        /// The path text.
        path: String,
    },
    /// A single filename component (at most [`limits::MAX_NAME_BYTES`] bytes),
    /// resolved by the agent under the session staging root.
    Staging {
        /// The artifact filename; must be one component (no separators, no
        /// `..`, no drive or colon).
        name: String,
    },
}

/// A cryptographic digest algorithm. Additional algorithms are an additive
/// (minor-bump) change.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DigestAlgo {
    /// SHA-256; digests are [`limits::DIGEST_LEN`] bytes.
    Sha256,
}

/// The kind of a filesystem entry, reported by `FsStat`/`FsList` using `lstat`
/// semantics (a reparse point is reported as [`FileKind::Symlink`], never
/// followed).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FileKind {
    /// A regular file.
    File,
    /// A directory.
    Dir,
    /// A symbolic link, junction, or other reparse point.
    Symlink,
    /// Anything else (device, pipe, and so on).
    Other,
}

/// When the agent autonomously removes a staged artifact (REQ-F-019).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CleanupPolicy {
    /// Remove when the session ends (the default for support artifacts).
    OnSessionEnd,
    /// Remove at the next normal agent shutdown.
    OnShutdown,
    /// Keep until an explicit `StagingCleanup` with an `All` trigger.
    Keep,
}

/// Which staged artifacts an `StagingCleanup` request removes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CleanupTrigger {
    /// Remove artifacts marked [`CleanupPolicy::OnSessionEnd`] and orphaned
    /// part files.
    SessionEnd,
    /// Remove artifacts marked [`CleanupPolicy::OnSessionEnd`] or
    /// [`CleanupPolicy::OnShutdown`] and orphaned part files.
    Shutdown,
    /// Remove every staged artifact and part regardless of policy (an explicit
    /// purge, including [`CleanupPolicy::Keep`] artifacts).
    All,
}

/// One entry in an `FsList` page or an `FsStat` result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FileEntry {
    /// The component name (at most [`limits::MAX_NAME_BYTES`] bytes).
    pub name: String,
    /// The entry kind under `lstat` semantics.
    pub kind: FileKind,
    /// Size in bytes (at most [`limits::MAX_FILE_OFFSET`]).
    pub len: u64,
    /// Last-modified time in Unix milliseconds, when the target reports one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub modified_ms: Option<u64>,
    /// Whether the entry is read-only.
    pub readonly: bool,
}

/// One artifact recorded in the agent's persistent staging manifest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StagedEntry {
    /// The resolved absolute path of the committed artifact.
    pub canonical_path: String,
    /// The technician-side source label bound at commit (at most
    /// [`limits::MAX_LABEL_BYTES`] bytes).
    pub source: String,
    /// Committed size in bytes.
    pub len: u64,
    /// The digest algorithm.
    pub algo: DigestAlgo,
    /// The verified digest (exactly [`limits::DIGEST_LEN`] bytes).
    pub digest: B64Bytes,
    /// The per-artifact cleanup policy.
    pub cleanup: CleanupPolicy,
    /// Creation time in Unix milliseconds.
    pub created_ms: u64,
}

/// One artifact a `StagingCleanup` could not remove, reported honestly in a
/// [`OpResult::CleanupReport`] (REQ-N-008).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CleanupFailure {
    /// The artifact that could not be removed.
    pub canonical_path: String,
    /// Why removal failed (at most [`limits::MAX_REASON_BYTES`] bytes).
    pub reason: String,
}

// --- Drives and folder removal (ADR-0012, ST-M010-001, CAP_FS_V2) ---

/// The kind of a drive of the target ([`DriveEntry`]). Further kinds are an
/// additive (minor-bump) change.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DriveKind {
    /// A drive that stays in the computer: a hard disk or a solid-state drive.
    Fixed,
    /// A drive whose medium can be taken out: a USB stick, a card reader.
    Removable,
    /// A drive on another computer, reached over the network.
    Network,
    /// A CD, DVD or Blu-ray drive.
    Optical,
    /// A drive held in memory.
    RamDisk,
    /// The target could not tell what the drive is.
    Unknown,
}

/// One drive of the target, as [`OpResult::Drives`] lists it (ST-M010-001).
///
/// The label and the two sizes are absent when the target cannot read them:
/// a card reader without a card, a drive on the network, a host that reports
/// no sizes. An absent value means "not known", never zero (REQ-N-008). Every
/// text is the target's own and is untrusted input.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DriveEntry {
    /// The drive's root folder, such as `C:\` (at most
    /// [`limits::MAX_DRIVE_ROOT_BYTES`] bytes, one line). On a Windows target
    /// it is a valid [`RemotePath::Absolute`] for the other file operations. A
    /// host without drive letters reports its own root, so the text is
    /// bounded rather than shape-validated.
    pub root: String,
    /// What the drive is.
    pub kind: DriveKind,
    /// The volume label, when the drive has one that can be read (at most
    /// [`limits::MAX_DRIVE_LABEL_BYTES`] bytes, one line).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// The size of the drive in bytes, when known (at most
    /// [`limits::MAX_FILE_OFFSET`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total_bytes: Option<u64>,
    /// The bytes free for the account the agent runs as, when known (never
    /// more than `total_bytes` when both are present).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub free_bytes: Option<u64>,
}

/// Where the removal of a folder stands after one [`Operation::RemoveTree`]
/// step (ST-M010-001).
///
/// Only [`TreeState::Removed`] is a success. Every other state says plainly
/// that the folder is still there, so a partial removal can never be read as a
/// complete one (REQ-N-008).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TreeState {
    /// The folder and everything in it are gone.
    Removed,
    /// The step reached its bound after removing something: the folder is
    /// still there and the next step gets further.
    More,
    /// The whole folder was walked and the entries the step names could not be
    /// removed, so the folder remains with them in it.
    Incomplete,
    /// The step reached its bound without removing anything: the entries it
    /// met could not be removed, and what lies beyond them was not tried.
    Blocked,
    /// The removal was stopped before the folder was gone, on the caller's
    /// request (`stop`): nothing more was removed.
    Stopped,
}

/// One entry a folder removal could not remove, named in an
/// [`OpResult::TreeRemoval`] (ST-M010-001).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TreeFailure {
    /// The path of the entry on the target (at most
    /// [`limits::MAX_TREE_FAILURE_PATH_BYTES`] bytes, one line). A longer path
    /// is cut and ends in `…`. Untrusted input: the names are the target's.
    pub path: String,
    /// Why it could not be removed, in the wire vocabulary: for example
    /// [`ErrorCode::FileInUse`], [`ErrorCode::AccessDenied`], or
    /// [`ErrorCode::InvalidPath`] for an entry that turned out to lie outside
    /// the folder.
    pub code: ErrorCode,
}

/// The encoding of a captured desktop frame image (ADR-0015).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ImageFormat {
    /// Lossless PNG.
    Png,
    /// Lossy JPEG (honours `DesktopCapture::quality`).
    Jpeg,
    /// WebP (lossy, honours `DesktopCapture::quality`).
    Webp,
}

/// The honest state of the interactive desktop at capture time (ADR-0015,
/// ST-M005-001, REQ-N-008).
///
/// Only [`DesktopState::Active`] can accompany a live image. Every other state
/// is reported explicitly with **no** image and never a fabricated or stale
/// frame passed off as live: the console and AI can tell that the customer's
/// screen is not observable right now, and why.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DesktopState {
    /// An interactive user desktop is present and was captured.
    Active,
    /// The workstation is locked; the lock screen is not captured.
    Locked,
    /// A different session holds the console (fast user switch); the requested
    /// session's desktop is not the current input desktop.
    Switched,
    /// The interactive session is disconnected (no attached display).
    Disconnected,
    /// No input desktop is available to the agent (for example a service
    /// window station with no visible desktop).
    NoInputDesktop,
    /// The secure desktop (Winlogon: UAC, the Secure Attention Sequence, the
    /// sign-in screen) is active. It is out of reach of a user-mode agent by
    /// design and is never captured or driven (ADR-0015, REQ-S-013).
    Secure,
}

/// What a [`Operation::DesktopPointer`] action does at its coordinates.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PointerAction {
    /// Move the pointer without pressing a button.
    Move,
    /// Press and hold `button`.
    Down,
    /// Release `button`.
    Up,
    /// A single press-and-release of `button`.
    Click,
    /// A double press-and-release of `button`.
    DoubleClick,
}

/// A pointer button for a [`Operation::DesktopPointer`] action.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PointerButton {
    /// The primary (left) button.
    Left,
    /// The secondary (right) button.
    Right,
    /// The middle (wheel) button.
    Middle,
}

/// What a [`Operation::DesktopKey`] action does to a single key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum KeyAction {
    /// Press and hold the key.
    Down,
    /// Release the key.
    Up,
    /// A single press-and-release of the key.
    Press,
}

/// The identity and geometry of one captured desktop frame (ADR-0015,
/// ST-M005-001, REQ-F-016).
///
/// Every capture result carries this so a frame can never be mistaken for one
/// from another display, desktop, or moment. The target hostname and session
/// id are stamped by the routing envelope at the MCP boundary; this metadata
/// adds the desktop-specific identity only the agent knows (the Windows
/// interactive session, the window-station\desktop, the display and its
/// geometry) plus the capture timestamp that establishes freshness.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DesktopFrameMeta {
    /// The honest desktop state at capture time. An image is present only when
    /// this is [`DesktopState::Active`].
    pub state: DesktopState,
    /// The Windows interactive (WTS) session the frame came from.
    pub wts_session: u32,
    /// The window-station\desktop the capture ran on (for example
    /// `"WinSta0\\Default"`), at most [`limits::MAX_DESKTOP_NAME_BYTES`] bytes.
    pub desktop: String,
    /// The zero-based display index within the session.
    pub display: u16,
    /// The logical (DPI-scaled) frame width in pixels.
    pub width: u32,
    /// The logical (DPI-scaled) frame height in pixels.
    pub height: u32,
    /// The physical frame width in pixels.
    pub physical_width: u32,
    /// The physical frame height in pixels.
    pub physical_height: u32,
    /// The effective dots-per-inch of the captured display.
    pub dpi: u16,
    /// The encoding of the accompanying image (meaningful only when an image
    /// is present).
    pub format: ImageFormat,
    /// The capture time in Unix milliseconds, establishing frame freshness.
    pub captured_unix_ms: u64,
}

/// An operation the console asks the agent to perform.
///
/// Payload-free variants are empty **struct** variants (`SystemInfo {}`):
/// serde ignores `deny_unknown_fields` for unit variants of internally
/// tagged enums, and the unknown-field policy (REQ-N-012) must hold for
/// every variant.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub enum Operation {
    /// Report the target identity and basic system state.
    SystemInfo {},
    /// Start a shell command as a streaming job.
    Exec {
        /// The shell that runs the command.
        shell: Shell,
        /// The command text (at most [`limits::MAX_COMMAND_BYTES`] bytes).
        command: String,
        /// Optional **remote** working directory. Validated with the same
        /// adversarial path rules as an absolute [`RemotePath`] (drive-qualified
        /// absolute only; `..`, UNC shares, device namespaces, reserved device
        /// names, and alternate-data-stream colons are rejected; at most
        /// [`limits::MAX_PATH_BYTES`] bytes) so path semantics stay
        /// unambiguously remote (ST-M003-003, REQ-F-009).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        working_dir: Option<String>,
        /// Extra environment variables (bounded by
        /// [`limits::MAX_ENV_ENTRIES`] / [`limits::MAX_ENV_KEY`] /
        /// [`limits::MAX_ENV_VALUE`]).
        #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
        env: BTreeMap<String, String>,
        /// Optional deadline in milliseconds (at most
        /// [`limits::MAX_TIMEOUT_MS`]); expiry yields
        /// [`JobOutcome::TimedOut`].
        #[serde(default, skip_serializing_if = "Option::is_none")]
        timeout_ms: Option<u64>,
    },
    /// Feed bytes to a running job's standard input.
    WriteStdin {
        /// The job to write to.
        job: JobId,
        /// The bytes to write (at most [`limits::MAX_CHUNK_BYTES`]).
        data: B64Bytes,
    },
    /// List the jobs the agent currently tracks.
    ListJobs {},
    /// Report metadata for one path, using `lstat` semantics (a reparse point
    /// is described, never followed), plus any pending part length so a resume
    /// can probe without failing on an absent final file. Never errors on
    /// absence: an absent path yields `PathStat { entry: None, .. }`.
    FsStat {
        /// The path to describe.
        path: RemotePath,
    },
    /// List a directory, one bounded page at a time.
    FsList {
        /// The directory to list.
        path: RemotePath,
        /// Resume cursor: the name after which to continue (exclusive). `None`
        /// starts at the beginning. At most [`limits::MAX_NAME_BYTES`] bytes.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        continue_after: Option<String>,
    },
    /// Create a directory. Fails `AlreadyExists` if the path is taken.
    CreateDir {
        /// The directory to create.
        path: RemotePath,
    },
    /// Read a bounded range of a file as raw bytes (no text or newline
    /// conversion).
    FsRead {
        /// The file to read.
        path: RemotePath,
        /// The byte offset to read from (at most [`limits::MAX_FILE_OFFSET`]).
        offset: u64,
        /// The number of bytes to read (at most [`limits::MAX_CHUNK_BYTES`]).
        len: u32,
    },
    /// Append raw bytes to the session-namespaced **part** file for `dest`.
    /// `offset` must equal the current part length or the agent answers
    /// `WriteConflict`; there is no raw write to a final path.
    FsWrite {
        /// The eventual destination whose part receives the bytes.
        dest: RemotePath,
        /// The append offset, which must equal the current part length (at most
        /// [`limits::MAX_FILE_OFFSET`]).
        offset: u64,
        /// The bytes to append (at most [`limits::MAX_CHUNK_BYTES`]).
        data: B64Bytes,
    },
    /// Finalize a part into `dest`: fsync, verify length **and** digest, then
    /// atomically rename into place. The only wire path to a final filename, so
    /// an unverified file is unrepresentable (REQ-N-007).
    FsCommit {
        /// The final destination path.
        dest: RemotePath,
        /// The expected total length (at most [`limits::MAX_TRANSFER_BYTES`]).
        len: u64,
        /// The digest algorithm.
        algo: DigestAlgo,
        /// The expected digest (exactly [`limits::DIGEST_LEN`] bytes).
        digest: B64Bytes,
        /// Whether replacing an existing file is permitted; otherwise a present
        /// destination yields `AlreadyExists`.
        overwrite: bool,
        /// Technician-side origin label; **required** for a
        /// [`RemotePath::Staging`] destination so the staging manifest and audit
        /// record where the artifact came from (REQ-S-017). At most
        /// [`limits::MAX_LABEL_BYTES`] bytes.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        source: Option<String>,
        /// The cleanup policy to record for a staged artifact.
        cleanup: CleanupPolicy,
    },
    /// Discard the part file for `dest`. Idempotent: aborting a nonexistent
    /// part succeeds (the desired state is already reached).
    FsAbort {
        /// The destination whose part is discarded.
        dest: RemotePath,
    },
    /// Compute a whole-file digest, or a verified-prefix digest when
    /// `prefix_len` is set (the resume-verification primitive).
    FsDigest {
        /// The file to digest.
        path: RemotePath,
        /// Digest only the first `prefix_len` bytes when set (at most
        /// [`limits::MAX_FILE_OFFSET`]); otherwise digest the whole file.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        prefix_len: Option<u64>,
    },
    /// Rename or move within the same volume (an atomic rename, never a
    /// copy-and-delete). A cross-volume request yields `Unsupported`; the
    /// console composes read + write + delete when it truly wants a copy.
    FsMove {
        /// The source path.
        from: RemotePath,
        /// The destination path.
        to: RemotePath,
        /// Whether replacing an existing destination is permitted.
        overwrite: bool,
    },
    /// Delete one file, using `lstat` semantics (a reparse point is removed, not
    /// followed). A directory yields `WrongPathKind`.
    FsDelete {
        /// The file to delete.
        path: RemotePath,
    },
    /// Remove one empty directory. A non-empty directory yields `DirNotEmpty`;
    /// a directory with its content is removed by [`Operation::RemoveTree`].
    RemoveDir {
        /// The directory to remove.
        path: RemotePath,
    },
    /// List the session's staged artifacts and their cleanup policies.
    ListStaged {},
    /// Remove staged artifacts selected by a cleanup trigger; the agent reports
    /// honest per-file outcomes.
    StagingCleanup {
        /// Which artifacts to remove.
        trigger: CleanupTrigger,
    },
    // --- Drives and folder removal (ADR-0012, ST-M010-001, CAP_FS_V2) ---
    /// List the drives of the target, answered [`OpResult::Drives`] with each
    /// drive's root, kind, label and total and free space (ST-M010-001). It
    /// reads nothing but those facts. Answered [`ErrorCode::Unsupported`] by a
    /// peer that does not advertise [`crate::CAP_FS_V2`] (REQ-N-012).
    FsDrives {},
    /// Remove a folder and everything in it, **one bounded step at a time**,
    /// answered [`OpResult::TreeRemoval`] (ST-M010-001).
    ///
    /// One step settles at most [`limits::MAX_TREE_STEP_ENTRIES`] entries and
    /// answers after about [`limits::MAX_TREE_STEP_MS`] milliseconds; while
    /// the answer is [`TreeState::More`] the caller sends the request again,
    /// with the `folder_id` of that answer. The agent keeps nothing between
    /// two steps, so it stays free to serve a stop, a revocation or any other
    /// request in between, and a caller that wants to cancel simply stops
    /// asking.
    ///
    /// **One removal is about one folder.** The folder is the one that lies
    /// at `path` when the first step opens it: a path that leads to it
    /// through a link (a junction or a symbolic link in a folder above it)
    /// yields `InvalidPath` and nothing is removed. Every later step names
    /// that folder by its `folder_id`, and a step that finds another folder
    /// at the path — one that was put there in the meantime — yields
    /// `InvalidPath` before it removes anything.
    ///
    /// The step never leaves the folder: a symbolic link, a junction or any
    /// other entry that stands for another place is removed as a link and
    /// never walked, and nothing is removed that does not lie in the folder.
    /// The files are deleted, not moved to a recycle bin. A drive root is not
    /// a folder and yields `InvalidPath`; a file yields `WrongPathKind`.
    /// Answered [`ErrorCode::Unsupported`] by a peer that does not advertise
    /// [`crate::CAP_FS_V2`] (REQ-N-012).
    RemoveTree {
        /// The folder to remove.
        path: RemotePath,
        /// Ends a removal the caller began and will not continue, without
        /// removing anything: the answer is [`TreeState::Stopped`]. It exists
        /// so that the agent, and with it the attended customer, learns that
        /// the removal was cancelled rather than left running (REQ-F-017,
        /// REQ-N-008). Omitted when `false`.
        #[serde(default, skip_serializing_if = "is_false")]
        stop: bool,
        /// The folder this removal began with: the `folder_id` of the
        /// previous answer of this removal ([`OpResult::TreeRemoval`]), given
        /// back unchanged. Absent in the first request of a removal, which
        /// takes the folder that lies at `path`. From 1 to
        /// [`limits::MAX_FOLDER_ID_BYTES`] bytes; omitted when absent.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        folder_id: Option<B64Bytes>,
    },
    // --- Interactive terminal (ADR-0013, ST-M003-009, CAP_TERM_V1) ---
    /// Open an interactive pseudo-console terminal. **A terminal is a job**: the
    /// agent spawns an interactive (ConPTY) job and answers
    /// [`OpResult::TermOpened`] with its identifier, so the existing `Cancel`,
    /// `ListJobs`, and job-exit paths apply. Merged output streams back as
    /// [`EventKind::JobOutput`] (raw VT pass-through, ADR-0010) and the terminal
    /// exit as [`EventKind::JobExited`]. Answered [`ErrorCode::Unsupported`] by a
    /// peer that does not advertise [`crate::CAP_TERM_V1`] (REQ-N-012).
    TermOpen {
        /// Which shell runs in the terminal.
        shell: Shell,
        /// Initial terminal width in columns (within
        /// [`limits::MIN_TERM_COLS`]..=[`limits::MAX_TERM_COLS`]).
        cols: u16,
        /// Initial terminal height in rows (within
        /// [`limits::MIN_TERM_ROWS`]..=[`limits::MAX_TERM_ROWS`]).
        rows: u16,
        /// Optional **remote** working directory, validated with the same
        /// adversarial path rules as [`Operation::Exec::working_dir`] (drive
        /// qualified; `..`, UNC, device namespaces, reserved device names, and
        /// alternate-data-stream colons rejected).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        working_dir: Option<String>,
        /// Extra environment variables (bounded by
        /// [`limits::MAX_ENV_ENTRIES`] / [`limits::MAX_ENV_KEY`] /
        /// [`limits::MAX_ENV_VALUE`]).
        #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
        env: BTreeMap<String, String>,
    },
    /// Resize a running terminal's pseudo-console. An out-of-bounds or malformed
    /// size is rejected (`LimitExceeded`), never acted on (ST-M003-009).
    TermResize {
        /// The terminal job to resize.
        job: JobId,
        /// New width in columns (within
        /// [`limits::MIN_TERM_COLS`]..=[`limits::MAX_TERM_COLS`]).
        cols: u16,
        /// New height in rows (within
        /// [`limits::MIN_TERM_ROWS`]..=[`limits::MAX_TERM_ROWS`]).
        rows: u16,
    },
    /// Close a terminal: the same teardown as cancelling its job (the whole
    /// process tree is terminated, REQ-S-014). The terminal's honest exit still
    /// arrives as [`EventKind::JobExited`].
    TermClose {
        /// The terminal job to close.
        job: JobId,
    },
    /// Capture the agent's current interactive desktop and answer with a
    /// [`OpResult::DesktopFrame`] (ADR-0015, ST-M005-001). The result carries
    /// honest [`DesktopState`]; an image is present only when the desktop is
    /// [`DesktopState::Active`]. Answered [`ErrorCode::Unsupported`] when the
    /// peer does not advertise [`crate::CAP_DESKTOP_V1`] (REQ-N-012).
    DesktopCapture {
        /// The zero-based display index to capture within the session.
        display: u16,
        /// The desired image encoding.
        format: ImageFormat,
        /// Optional maximum output width in pixels; the agent scales the frame
        /// to fit within [`limits::MIN_DESKTOP_DIM`]..=[`limits::MAX_DESKTOP_DIM`]
        /// while preserving aspect ratio. `None` captures at native width.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        max_width: Option<u16>,
        /// Optional maximum output height in pixels (see `max_width`).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        max_height: Option<u16>,
        /// Optional encoder quality on a 1..=100 scale for a lossy `format`;
        /// ignored for [`ImageFormat::Png`]. `None` uses the agent default.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        quality: Option<u8>,
    },
    /// Inject a pointer action on the current input desktop (ADR-0015,
    /// ST-M005-002). Answered [`OpResult::Ack`], or [`ErrorCode::Unsupported`]
    /// when [`crate::CAP_DESKTOP_V1`] is not advertised. Refused after a target
    /// switch, revocation, or stop, and on a non-[`DesktopState::Active`]
    /// desktop (REQ-S-004, REQ-S-009).
    DesktopPointer {
        /// What to do at the coordinates.
        action: PointerAction,
        /// Which button the action concerns (ignored for
        /// [`PointerAction::Move`]).
        button: PointerButton,
        /// The absolute virtual-desktop x coordinate (magnitude at most
        /// [`limits::MAX_DESKTOP_COORD`]).
        x: i32,
        /// The absolute virtual-desktop y coordinate (magnitude at most
        /// [`limits::MAX_DESKTOP_COORD`]).
        y: i32,
    },
    /// Inject a scroll action at the given origin (ADR-0015, ST-M005-002).
    DesktopScroll {
        /// The absolute virtual-desktop x origin (magnitude at most
        /// [`limits::MAX_DESKTOP_COORD`]).
        x: i32,
        /// The absolute virtual-desktop y origin (magnitude at most
        /// [`limits::MAX_DESKTOP_COORD`]).
        y: i32,
        /// Horizontal wheel delta in detents (magnitude at most
        /// [`limits::MAX_DESKTOP_SCROLL`]).
        dx: i32,
        /// Vertical wheel delta in detents (magnitude at most
        /// [`limits::MAX_DESKTOP_SCROLL`]).
        dy: i32,
    },
    /// Inject a single-key action naming a virtual key (ADR-0015,
    /// ST-M005-002). No keylogging or credential-capture primitive is exposed
    /// (REQ-S-013): this only *sends* input.
    DesktopKey {
        /// What to do with the key.
        action: KeyAction,
        /// The virtual-key token (for example `"enter"`, `"f5"`, `"ctrl"`), at
        /// most [`limits::MAX_DESKTOP_KEY_BYTES`] bytes.
        key: String,
    },
    /// Type a Unicode string as a sequence of key events (ADR-0015,
    /// ST-M005-002).
    DesktopType {
        /// The text to type, at most [`limits::MAX_DESKTOP_TEXT_BYTES`] bytes.
        text: String,
    },
    /// Send a chord (modifier + key combination) as a single hotkey press
    /// (ADR-0015, ST-M005-002). The chord holds each element down in order,
    /// then releases in reverse.
    DesktopHotkey {
        /// The chord elements (for example `["ctrl", "shift", "esc"]`), at most
        /// [`limits::MAX_DESKTOP_HOTKEY_KEYS`] elements each at most
        /// [`limits::MAX_DESKTOP_KEY_BYTES`] bytes.
        keys: Vec<String>,
    },
    /// Pause the input pipeline for a bounded interval, so a scripted action
    /// sequence can pace itself (ADR-0015, ST-M005-002).
    DesktopWait {
        /// The pause in milliseconds, at most [`limits::MAX_DESKTOP_WAIT_MS`].
        millis: u64,
    },
    /// Read the remote clipboard's text, answered [`OpResult::ClipboardContents`]
    /// (ADR-0015, ST-M005-003). Clipboard exchange is explicit-only and audited.
    ClipboardGet {},
    /// Set the remote clipboard's text (ADR-0015, ST-M005-003). CretLink never
    /// places session secrets or provider credentials on the remote clipboard
    /// (REQ-F-015, REQ-S-015).
    ClipboardSet {
        /// The text to place on the clipboard, at most
        /// [`limits::MAX_CLIPBOARD_BYTES`] bytes.
        text: String,
    },
}

/// The successful result of an [`Operation`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "result", rename_all = "snake_case", deny_unknown_fields)]
pub enum OpResult {
    /// Result of [`Operation::SystemInfo`].
    SystemInfo {
        /// The target's identity (REQ-F-016).
        target: TargetIdentity,
        /// Whether the target reports a pending reboot.
        reboot_pending: bool,
    },
    /// An `Exec` job started; output arrives as [`EventKind::JobOutput`].
    ExecStarted {
        /// The identifier assigned to the new job.
        job: JobId,
        /// The effective absolute **remote** working directory the job actually
        /// received: the validated requested directory, or the agent process's
        /// current directory at spawn time (ST-M003-003, REQ-F-009). Defaults
        /// to empty on decode so a same-major peer on an older minor stays
        /// interoperable (REQ-N-012): empty means "not reported (an
        /// older-minor peer) or the agent could not read its own directory" —
        /// never a fabricated path. At most [`limits::MAX_PATH_BYTES`] bytes.
        #[serde(default)]
        working_dir: String,
    },
    /// The operation completed and has no payload (for example
    /// `WriteStdin`). An empty struct variant for the same
    /// unknown-field-policy reason as [`Operation::SystemInfo`].
    Ack {},
    /// Result of [`Operation::ListJobs`].
    Jobs {
        /// At most [`limits::MAX_JOBS_LISTED`] summaries.
        jobs: Vec<JobSummary>,
    },
    /// Result of [`Operation::FsStat`]. Both fields absent means nothing exists
    /// at the path (`FsStat` never errors on absence).
    PathStat {
        /// The final entry at the path, when one exists.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        entry: Option<FileEntry>,
        /// The length of a pending part for this destination, when one exists
        /// (the resume probe).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        partial_len: Option<u64>,
    },
    /// Result of [`Operation::FsList`]: one bounded page.
    DirEntries {
        /// At most [`limits::MAX_DIR_PAGE`] entries, ordered by name.
        entries: Vec<FileEntry>,
        /// Whether this is the final page (no more entries follow).
        end: bool,
    },
    /// Result of [`Operation::FsRead`]: one raw chunk.
    FileData {
        /// The bytes read (at most [`limits::MAX_CHUNK_BYTES`]).
        data: B64Bytes,
        /// Whether the read reached end of file.
        eof: bool,
        /// The current total file length, so the console can detect a file that
        /// changed length under it mid-transfer.
        file_len: u64,
    },
    /// Result of [`Operation::FsWrite`].
    WriteAccepted {
        /// The part length after the append (the next expected write offset).
        part_len: u64,
    },
    /// Result of [`Operation::FsCommit`].
    Committed {
        /// The resolved absolute path of the committed file.
        canonical_path: String,
        /// The verified length.
        len: u64,
        /// The digest algorithm.
        algo: DigestAlgo,
        /// The verified digest (exactly [`limits::DIGEST_LEN`] bytes).
        digest: B64Bytes,
        /// Whether an existing file was replaced.
        replaced: bool,
    },
    /// Result of [`Operation::FsDigest`].
    FileDigest {
        /// The digest algorithm.
        algo: DigestAlgo,
        /// The digest (exactly [`limits::DIGEST_LEN`] bytes).
        digest: B64Bytes,
        /// The number of bytes digested (whole file, or `prefix_len`).
        len: u64,
    },
    /// Result of [`Operation::ListStaged`].
    StagedList {
        /// At most [`limits::MAX_DIR_PAGE`] staged artifacts.
        entries: Vec<StagedEntry>,
    },
    /// Result of [`Operation::StagingCleanup`].
    CleanupReport {
        /// How many artifacts were removed.
        removed: u32,
        /// Artifacts that could not be removed (at most
        /// [`limits::MAX_DIR_PAGE`]).
        failed: Vec<CleanupFailure>,
    },
    /// Result of [`Operation::FsDrives`] (ST-M010-001).
    Drives {
        /// At most [`limits::MAX_DRIVES`] drives, in the target's own order.
        drives: Vec<DriveEntry>,
    },
    /// Result of one [`Operation::RemoveTree`] step: what the step removed,
    /// what it could not remove, and where the removal stands (ST-M010-001).
    ///
    /// The two counts and the failures are those of this step only; the
    /// caller adds the counts up over the steps. A step that follows another
    /// walks the folder again from its top, so the failures of the last step
    /// are the ones that still hold.
    TreeRemoval {
        /// Where the removal stands. Only [`TreeState::Removed`] is a success.
        state: TreeState,
        /// Files and links this step removed.
        removed_files: u64,
        /// Folders this step removed, the folder itself included once it is
        /// gone.
        removed_dirs: u64,
        /// The first entries this step could not remove, each with its reason
        /// (at most [`limits::MAX_TREE_FAILURES`]). Omitted when empty.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        failed: Vec<TreeFailure>,
        /// How many more entries this step could not remove, beyond `failed`.
        /// Omitted when zero.
        #[serde(default, skip_serializing_if = "is_zero")]
        failed_more: u64,
        /// What identifies the folder being removed, for the next step: the
        /// caller gives it back in [`Operation::RemoveTree`], so that the
        /// removal goes on with this folder and no other. Present **exactly
        /// when** `state` is [`TreeState::More`]; from 1 to
        /// [`limits::MAX_FOLDER_ID_BYTES`] bytes, whose meaning is the
        /// agent's own. Omitted when absent.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        folder_id: Option<B64Bytes>,
    },
    /// Result of [`Operation::TermOpen`]: the interactive terminal job started.
    /// Its merged output arrives as [`EventKind::JobOutput`] and its exit as
    /// [`EventKind::JobExited`] — a terminal is a job (ST-M003-009).
    TermOpened {
        /// The identifier assigned to the new terminal job.
        job: JobId,
    },
    /// Result of [`Operation::DesktopCapture`]: one desktop frame with its
    /// honest identity and geometry (ADR-0015, ST-M005-001).
    ///
    /// Exactly one of `image` or `staged` is set when `meta.state` is
    /// [`DesktopState::Active`]; both are absent for every other state, which
    /// keeps a non-live desktop from ever presenting a fabricated or stale
    /// frame (REQ-N-008).
    DesktopFrame {
        /// The frame's identity, geometry, state, and timestamp.
        meta: DesktopFrameMeta,
        /// The inline encoded image when it fits
        /// [`limits::MAX_DESKTOP_INLINE_BYTES`]; `None` when the frame was
        /// staged (see `staged`) or no live frame exists (see `meta.state`).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        image: Option<B64Bytes>,
        /// The staging path of the encoded frame when it exceeded the inline
        /// bound (the staged-artifact reference, ADR-0015). The console reads it
        /// back through the ordinary [`Operation::FsRead`] path.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        staged: Option<RemotePath>,
    },
    /// Result of [`Operation::ClipboardGet`]: the remote clipboard's text
    /// (ADR-0015, ST-M005-003).
    ClipboardContents {
        /// The clipboard text, or `None` when the clipboard holds no text (or
        /// only an unsupported type). At most [`limits::MAX_CLIPBOARD_BYTES`]
        /// bytes.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        text: Option<String>,
    },
}

/// One entry in a [`OpResult::Jobs`] listing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JobSummary {
    /// The job identifier.
    pub job: JobId,
    /// A preview of the command that started the job.
    pub command_preview: String,
    /// Whether the job is still running.
    pub running: bool,
}

/// Which output stream a [`EventKind::JobOutput`] chunk belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OutputStream {
    /// Standard output.
    Stdout,
    /// Standard error.
    Stderr,
}

/// How a job ended. Honest outcomes are mandatory (REQ-N-008): a network
/// interruption or process failure must never be reported as success.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobOutcome {
    /// The job exited successfully.
    Completed,
    /// The job exited with a failure code.
    Failed,
    /// The job was cancelled on request.
    Cancelled,
    /// The job hit its `timeout_ms` deadline.
    TimedOut,
    /// The real outcome could not be determined (for example the
    /// connection dropped mid-job).
    Unknown,
}

/// Which class of desktop interaction a [`EventKind::DesktopActivity`] signal
/// reports, so both endpoints can visibly distinguish passive screen
/// observation from active input control (REQ-F-017 through REQ-F-020,
/// REQ-N-013).
///
/// The signal carries this class only — never a captured pixel, a keystroke, or
/// clipboard text — so reflecting it can never itself disclose observed content
/// (REQ-S-016 through REQ-S-018).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DesktopAccess {
    /// The agent read the desktop passively (a screen capture or a clipboard
    /// read).
    Observe,
    /// The agent drove input (pointer, scroll, key, type, hotkey, or a
    /// clipboard write).
    Control,
}

/// An unsolicited event streamed from the agent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case", deny_unknown_fields)]
pub enum EventKind {
    /// A chunk of job output.
    ///
    /// The bytes are raw: ConPTY VT escape sequences are passed through
    /// unmodified (ADR-0010); consumers render or strip them. Treat the
    /// content as untrusted remote input.
    JobOutput {
        /// The job that produced the output.
        job: JobId,
        /// Which stream the chunk came from.
        stream: OutputStream,
        /// Raw output bytes (at most [`limits::MAX_CHUNK_BYTES`]).
        data: B64Bytes,
    },
    /// A job ended.
    JobExited {
        /// The job that ended.
        job: JobId,
        /// The process exit code, when one exists.
        exit_code: Option<i32>,
        /// Wall-clock job duration in milliseconds.
        duration_ms: u64,
        /// The honest outcome (REQ-N-008).
        outcome: JobOutcome,
    },
    /// The agent lost track of a job because the session ended or the
    /// connection dropped.
    Disconnected {
        /// The affected job.
        job: JobId,
        /// Whether the remote process may still be running.
        process_continues: bool,
    },
    /// The agent observed or controlled the interactive desktop.
    ///
    /// Emitted agent-side after a desktop operation is actually serviced, so
    /// the console reflects the same observation/control state the customer
    /// sees, from the one endpoint that knows what truly happened after
    /// pacing/refusal (REQ-F-017 through REQ-F-020, ST-M005-005). It carries the
    /// interaction class only — never a captured pixel, keystroke, or clipboard
    /// text (REQ-S-016 through REQ-S-018).
    DesktopActivity {
        /// Whether the agent passively observed or actively drove input.
        access: DesktopAccess,
    },
}

/// A machine-readable error category carried on wire `Error` messages.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    /// The peer's protocol major version differs.
    IncompatibleVersion,
    /// The payload was not a valid message (bad JSON, unknown field,
    /// missing field, bad base64).
    MalformedMessage,
    /// A frame exceeded [`limits::MAX_FRAME_BYTES`].
    OversizedMessage,
    /// A field violated one of the [`limits`] bounds.
    LimitExceeded,
    /// The referenced session is not known to the receiver.
    UnknownSession,
    /// The referenced job is not known to the receiver.
    UnknownJob,
    /// The session was revoked; all further work fails closed.
    SessionRevoked,
    /// The operation or capability is not supported by the receiver (also used
    /// for a cross-volume `FsMove`, for a file operation when the peer did
    /// not advertise [`crate::CAP_FS_V1`], and for `FsDrives` or `RemoveTree`
    /// when it did not advertise [`crate::CAP_FS_V2`]).
    Unsupported,
    /// A path does not exist (for an operation that requires it).
    PathNotFound,
    /// The target denied access to the path.
    AccessDenied,
    /// The path is in use by another process and cannot be opened as requested.
    FileInUse,
    /// The path failed a structural safety check (also produced by
    /// [`crate::Message::validate`] via [`ProtocolError::InvalidPath`]).
    InvalidPath,
    /// The destination already exists and `overwrite` was not set.
    AlreadyExists,
    /// The path is the wrong kind for the operation (a directory where a file
    /// was required, or the reverse).
    WrongPathKind,
    /// A directory removal was attempted on a non-empty directory.
    DirNotEmpty,
    /// An `FsWrite` offset did not match the current part length.
    WriteConflict,
    /// A commit's declared length or digest did not match the part contents, or
    /// a resume prefix digest did not match; the file was not published.
    IntegrityMismatch,
    /// The operation would exceed a staging or storage quota, or the disk it
    /// writes to is full.
    StorageExhausted,
    /// A resolved path escaped the session staging root.
    OutsideStagingRoot,
    /// An internal receiver failure that is none of the above.
    Internal,
}

/// Which processing stage produced a wire `Error`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorStage {
    /// During `Hello`/`HelloAck` version and capability negotiation.
    Negotiation,
    /// While validating a decoded message against schema limits.
    Validation,
    /// While executing an accepted operation.
    Execution,
    /// While framing, reading, or writing on the transport.
    Transport,
}

/// A message in the commit-reveal Session Approval Code round (ADR-0003).
///
/// These three messages run **inside** the already-encrypted Noise transport,
/// after the handshake and before any full-control consent. They carry the
/// commitment and the two 32-byte nonces the endpoints combine (with the Noise
/// handshake hash) into the human-comparable Session Approval Code
/// ([`crate::derive_session_approval_code`]). The 32-byte fields travel as
/// base64 ([`B64Bytes`]); the actual cryptographic checks (constant-time
/// commitment comparison, code derivation) live in `cretlink-transport`.
///
/// Ordering is fixed and enforced by the transport state machine: the
/// initiator's [`PairingMessage::Commit`] must arrive before the responder
/// sends its [`PairingMessage::Nonce`], and the initiator's
/// [`PairingMessage::Reveal`] is checked against the commitment before any
/// code is derived (REQ-S-006).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum PairingMessage {
    /// Initiator -> responder: the commitment `SHA-256(na)` to the initiator's
    /// nonce, sent before either nonce is revealed
    /// ([`crate::SAC_COMMITMENT_LEN`] bytes).
    Commit {
        /// The 32-byte commitment.
        commitment: B64Bytes,
    },
    /// Responder -> initiator: the responder's revealed nonce `nc`, sent only
    /// after the `Commit` was received ([`crate::SAC_NONCE_LEN`] bytes).
    Nonce {
        /// The responder's 32-byte nonce.
        nc: B64Bytes,
        /// The console's display name, as the technician's computer reports it
        /// (ST-M009-011): an unverified label the customer side shows as what
        /// the technician's computer says, never an identity and never
        /// authority. It is not part of the Session Approval Code derivation.
        /// Additive since minor `10`: omitted when `None`, so a nonce without a
        /// name keeps its earlier wire form, and validated on decode through
        /// [`DisplayName::new`] (REQ-N-012).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        display_name: Option<DisplayName>,
    },
    /// Initiator -> responder: the initiator's revealed nonce `na`; the
    /// responder verifies `SHA-256(na)` equals the earlier commitment before
    /// deriving the code ([`crate::SAC_NONCE_LEN`] bytes).
    Reveal {
        /// The initiator's 32-byte nonce.
        na: B64Bytes,
    },
}

/// Why a session is being revoked, carried on a [`ControlMessage::Revoke`]
/// (ST-M002-006). Every reason is a truthful outcome — a revocation is never
/// dressed up as a clean completion (REQ-N-008).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RevokeReason {
    /// The technician revoked the session from the console.
    TechnicianRequested,
    /// The customer revoked the session from the agent.
    CustomerRequested,
    /// The session's time budget elapsed.
    Expired,
    /// The session ended because of an error (for example a dropped
    /// connection); the real outcome is not a success.
    Error,
}

/// A session-control message exchanged over the **already-encrypted** channel
/// after pairing (ST-M002-005, ST-M002-006).
///
/// These messages carry informed full-control consent and revocation so that
/// full control is granted exactly once, visibly on both endpoints, and can be
/// torn down fail-closed from either side (REQ-F-006, REQ-F-018, REQ-S-004,
/// REQ-S-009). They never carry key material or the Session Approval Code
/// (REQ-S-018). No control capability exists before a [`ConsentGranted`] is
/// exchanged (REQ-S-001, REQ-S-003).
///
/// [`ConsentGranted`]: ControlMessage::ConsentGranted
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "control", rename_all = "snake_case", deny_unknown_fields)]
pub enum ControlMessage {
    /// Agent -> console: the customer is being shown the full-control
    /// disclosure and asked to consent. Carries the honest disclosure summary
    /// text (at most [`limits::MAX_DISCLOSURE_BYTES`] bytes) so the console can
    /// present exactly what the customer saw (REQ-S-003, REQ-S-016).
    ConsentRequested {
        /// The full-control disclosure summary shown to the customer.
        disclosure: String,
    },
    /// Agent -> console: the customer granted one attended full-control
    /// consent, binding it to the honest target identity (REQ-F-016,
    /// REQ-S-011). This is the only message that authorizes control.
    ConsentGranted {
        /// The honest identity of the target now under full control.
        target: TargetIdentity,
    },
    /// Agent -> console: the customer declined; no control is granted.
    ConsentDenied {
        /// A secret-free reason (at most [`limits::MAX_REASON_BYTES`] bytes).
        reason: String,
    },
    /// Either side: revoke the session now and tear down fail-closed
    /// (REQ-S-004, REQ-S-009).
    Revoke {
        /// Why the session is being revoked.
        reason: RevokeReason,
    },
    /// Either side: a liveness heartbeat carrying a monotonic sequence number,
    /// so a dropped connection surfaces as an honest disconnect rather than a
    /// silent stall (REQ-N-008).
    Heartbeat {
        /// Monotonic heartbeat sequence number.
        seq: u64,
    },
    /// Console -> agent: the technician has confirmed, during a humanly
    /// perceptible pause, that the Session Approval Code matches on both
    /// endpoints (REQ-S-006, ST-M002-013). Sent after the secure channel is
    /// established and the code is displayed on both sides, and before the
    /// customer is asked for full-control consent, it releases the agent's
    /// compare-wait so the consent decision can proceed. It carries **no key
    /// material** and **no approval code** — the code is compared out of band;
    /// this frame only signals that the human comparison passed. A mismatch,
    /// revoke, stop, or timeout during the compare window is never encoded as a
    /// confirmation: the session fails closed instead (REQ-S-009). Added in
    /// protocol minor 8; an agent on an older minor rejects the unknown variant
    /// and fails closed, so the gate cannot be silently downgraded (REQ-S-008).
    ApprovalCompareConfirmed {},
    /// Agent -> console: a single-use reboot-resume ticket was just armed by the
    /// customer's attended, local gesture (ADR-0014, ST-M006-001). This is a
    /// reflection only, so the console can show that a ticket is armed and until
    /// when; it grants **no control** and carries **no key material** — the
    /// continuity key never leaves the agent (REQ-S-018). The console cannot arm
    /// remotely: there is no console-to-agent arm message by design.
    RebootResumeArmed {
        /// The single-use ticket identifier (16 bytes, base64), for display and
        /// to correlate a later [`ControlMessage::RebootResumeDisarmed`].
        ticket_id: B64Bytes,
        /// The honest identity of the target the ticket is bound to (REQ-F-016,
        /// REQ-S-011). A ticket armed on one machine can never redeem on another.
        target: TargetIdentity,
        /// Absolute expiry as Unix milliseconds. After this instant the ticket
        /// is dead even if never redeemed (REQ-S-004).
        not_after_unix_millis: i64,
    },
    /// Agent -> console: a previously armed reboot-resume ticket was disarmed
    /// (by the customer, or because the session is ending). The console clears
    /// its armed indication (ADR-0014, ST-M006-001).
    RebootResumeDisarmed {
        /// The ticket identifier that was disarmed (16 bytes, base64).
        ticket_id: B64Bytes,
    },
    /// Agent -> console: the first step of the bilateral arming nonce exchange
    /// (ADR-0014 §2, ST-M006-002). The customer's attended local gesture starts
    /// arming; the agent commits to its fresh 32-byte `Na_rr` by sending
    /// `SHA-256(Na_rr)`, mirroring the SAC's commit-reveal discipline so neither
    /// side can grind the shared `K_rr`. Carries **no key material**: only the
    /// commitment.
    RebootResumeArmCommit {
        /// `SHA-256(Na_rr)` — the agent's binding commitment (32 bytes, base64).
        na_commit: B64Bytes,
    },
    /// Console -> agent: the console's contribution to the arming exchange
    /// (ADR-0014 §2, ST-M006-002). After seeing the agent's commitment the
    /// console replies with its own fresh 32-byte `Nc_rr`, so both endpoints
    /// derive `K_rr = HKDF-SHA256(h ‖ Na_rr ‖ Nc_rr)`. This is the console's
    /// entropy contribution only; it is **not** a remote arm grant — arming is
    /// still gated on the customer's attended local gesture, which already
    /// happened before the commit (REQ-S-001 through REQ-S-004).
    RebootResumeArmChallenge {
        /// The console's fresh reboot-resume nonce `Nc_rr` (32 bytes, base64).
        nc_rr: B64Bytes,
    },
    /// Agent -> console: the agent reveals `Na_rr` (ADR-0014 §2, ST-M006-002).
    /// The console checks `SHA-256(Na_rr)` against the earlier commitment, then
    /// both sides derive and persist their own sealed ticket for the same
    /// `ticket_id`. A mismatch aborts arming fail-closed. Still carries no
    /// `K_rr`: the key is derived independently on each end, never transmitted.
    RebootResumeArmReveal {
        /// The revealed reboot-resume nonce `Na_rr` (32 bytes, base64).
        na_rr: B64Bytes,
    },
    /// Console -> agent: the post-reboot continuity challenge (ADR-0014 §3,
    /// ST-M006-002). In the fresh Noise session after a reboot, before any
    /// control is possible, the console sends a fresh challenge nonce `Nc'`. The
    /// agent answers with [`ControlMessage::RebootResumeProof`]. Carries no key
    /// material and grants nothing by itself.
    RebootResumeChallenge {
        /// The console's fresh post-reboot challenge nonce `Nc'` (32 bytes,
        /// base64).
        nc_prime: B64Bytes,
    },
    /// Agent -> console: the continuity proof answering a
    /// [`ControlMessage::RebootResumeChallenge`] (ADR-0014 §3, ST-M006-002). The
    /// agent replies `HMAC-SHA256(K_rr, h' ‖ ticket_id ‖ Nc')`, binding the proof
    /// to the **new** session's handshake hash `h'`. The console verifies it in
    /// constant time and consumes the ticket. A valid proof authenticates the
    /// returning peer; it **never** restores `control_active` — attended
    /// full-control consent still runs (REQ-S-009).
    RebootResumeProof {
        /// The single-use ticket this proof redeems (16 bytes, base64).
        ticket_id: B64Bytes,
        /// The continuity proof tag (32 bytes, base64).
        proof: B64Bytes,
    },
    /// Agent -> console: the honest account of the agent's session teardown,
    /// sent once when an active session ends and the channel is still up —
    /// before the agent's own `Revoke` when the customer stops, or before the
    /// agent closes after the technician's `Revoke` (ST-M009-012, REQ-F-019).
    /// The console records it in the session's audit file with every note.
    /// `ok` is `false` when something was left behind or teardown failed; the
    /// notes say what. It carries no secret (REQ-S-017): only teardown wording,
    /// canonical paths of staged artifacts and their failure reasons. A
    /// disconnect leaves the console without one, which it records as unknown,
    /// never as success. Added in protocol minor 11.
    CleanupReported {
        /// Whether every session-scoped resource was torn down.
        ok: bool,
        /// One line each, at most [`limits::MAX_CLEANUP_NOTES`] notes of at
        /// most [`limits::MAX_CLEANUP_NOTE_BYTES`] bytes, without control
        /// characters.
        notes: Vec<String>,
    },
}

/// The top-level wire message.
///
/// Serialized as an internally tagged JSON object: the `type` field selects
/// the variant and the variant's fields sit beside it. Unknown fields are
/// rejected everywhere (REQ-N-012); see the crate-level compatibility
/// policy.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum Message {
    /// Opens negotiation; the first message on a connection.
    Hello {
        /// The sender's protocol version.
        protocol: ProtocolVersion,
        /// Which side is speaking.
        role: Role,
        /// Capability strings (bounded by
        /// [`limits::MAX_CAPABILITIES`] /
        /// [`limits::MAX_CAPABILITY_LEN`]).
        capabilities: Vec<String>,
    },
    /// Accepts a `Hello`; carries the responder's own version, the minted
    /// session, and the target identity (REQ-F-016).
    HelloAck {
        /// The responder's protocol version (its own minor).
        protocol: ProtocolVersion,
        /// The session this connection now belongs to.
        session: SessionId,
        /// Who the remote target is.
        target: TargetIdentity,
    },
    /// Asks the agent to perform one operation.
    Request {
        /// Correlates the eventual `Response` or `Error`.
        id: RequestId,
        /// The session the request belongs to.
        session: SessionId,
        /// Which local actor drives this request (REQ-F-017). Additive since
        /// minor `3`: defaults to [`Origin::Console`] and is omitted at that
        /// default, so an older-minor peer still decodes a console-origin
        /// request (REQ-N-012). It carries no authority — attribution only.
        #[serde(default, skip_serializing_if = "Origin::is_console")]
        origin: Origin,
        /// The caller's stated purpose of this action (REQ-F-026,
        /// ST-M009-007): the AI client's short plain-language reason, carried
        /// so the attended customer surface shows it beside the real
        /// operation. It is an unverified **claim**, never authority, and never
        /// replaces the operation it describes. `None` for technician-driven
        /// requests, which are labelled by their `origin` instead. Additive
        /// since minor `9`: omitted when `None`, so an older-minor peer still
        /// decodes every request without a purpose (REQ-N-012), and validated
        /// on decode through [`Purpose::new`].
        #[serde(default, skip_serializing_if = "Option::is_none")]
        purpose: Option<Purpose>,
        /// What to do.
        op: Operation,
    },
    /// Answers a `Request` that succeeded.
    Response {
        /// The `Request` this answers.
        id: RequestId,
        /// The session the response belongs to.
        session: SessionId,
        /// The operation result.
        result: OpResult,
    },
    /// An unsolicited event (job output, job exit, disconnect notice).
    Event {
        /// The session the event belongs to.
        session: SessionId,
        /// What happened.
        event: EventKind,
    },
    /// Asks the agent to cancel a running job.
    Cancel {
        /// Correlates the eventual `Response` or `Error`.
        id: RequestId,
        /// The session the cancellation belongs to.
        session: SessionId,
        /// Which local actor asks for the cancellation (REQ-F-017). Additive
        /// since minor `9`, with the same default-and-omit rule as
        /// [`Message::Request`]'s `origin`. Attribution only.
        #[serde(default, skip_serializing_if = "Origin::is_console")]
        origin: Origin,
        /// The caller's stated purpose of the cancellation (REQ-F-026), with
        /// the same rules as [`Message::Request`]'s `purpose`. Additive since
        /// minor `9`; omitted when `None`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        purpose: Option<Purpose>,
        /// The job to cancel.
        job: JobId,
    },
    /// Reports a failure. `id` and `session` are optional because some
    /// failures (negotiation, unparseable frames) have neither.
    Error {
        /// The `Request` that failed, when known.
        id: Option<RequestId>,
        /// The session, when known.
        session: Option<SessionId>,
        /// The machine-readable category.
        code: ErrorCode,
        /// A human-readable description (at most
        /// [`limits::MAX_ERROR_MESSAGE`] bytes).
        message: String,
        /// Which stage failed.
        stage: ErrorStage,
    },
    /// A commit-reveal Session Approval Code message (ADR-0003), exchanged
    /// over the already-encrypted transport after the Noise handshake.
    Pairing {
        /// The commit, nonce, or reveal step of the SAC round.
        pairing: PairingMessage,
    },
    /// A session-control message (informed consent, revocation, heartbeat),
    /// exchanged over the encrypted transport after pairing
    /// (ST-M002-005/006). Full control is authorized only by a
    /// [`ControlMessage::ConsentGranted`] carried here.
    Control {
        /// The session this control message belongs to.
        session: SessionId,
        /// The control action.
        control: ControlMessage,
    },
}

impl Message {
    /// Validates this message and serializes it to a JSON payload.
    ///
    /// Fails with [`ProtocolError::LimitExceeded`] when a field violates
    /// the [`limits`] bounds and with [`ProtocolError::Oversized`] when
    /// the encoded payload would exceed [`limits::MAX_FRAME_BYTES`].
    pub fn encode(&self) -> Result<Vec<u8>, ProtocolError> {
        self.validate()?;
        let bytes = serde_json::to_vec(self).map_err(ProtocolError::Malformed)?;
        if bytes.len() > limits::MAX_FRAME_BYTES {
            return Err(ProtocolError::Oversized {
                len: bytes.len(),
                max: limits::MAX_FRAME_BYTES,
            });
        }
        Ok(bytes)
    }

    /// Parses and validates a JSON payload.
    ///
    /// The size cap is checked **first**, before any parsing, so an
    /// oversized frame is rejected without inspecting its content; the
    /// parsed message is then validated against every [`limits`] bound.
    pub fn decode(bytes: &[u8]) -> Result<Self, ProtocolError> {
        if bytes.len() > limits::MAX_FRAME_BYTES {
            return Err(ProtocolError::Oversized {
                len: bytes.len(),
                max: limits::MAX_FRAME_BYTES,
            });
        }
        let message: Self = serde_json::from_slice(bytes).map_err(ProtocolError::Malformed)?;
        message.validate()?;
        Ok(message)
    }

    /// Checks every bounded field against [`limits`].
    pub fn validate(&self) -> Result<(), ProtocolError> {
        match self {
            Self::Hello { capabilities, .. } => {
                check_limit(
                    "hello capabilities count",
                    capabilities.len() as u64,
                    limits::MAX_CAPABILITIES as u64,
                )?;
                for capability in capabilities {
                    check_limit(
                        "hello capability length",
                        capability.len() as u64,
                        limits::MAX_CAPABILITY_LEN as u64,
                    )?;
                }
                Ok(())
            }
            // A stated `purpose` (on `Request` and `Cancel`) is bounded by
            // construction: `Purpose` can only be built through `Purpose::new`,
            // which serde also uses on decode, so an empty, multi-line, or
            // over-long purpose never reaches here (ST-M009-007).
            Self::HelloAck { .. } | Self::Cancel { .. } => Ok(()),
            Self::Request { op, .. } => validate_operation(op),
            Self::Response { result, .. } => validate_result(result),
            Self::Event { event, .. } => validate_event(event),
            Self::Error { message, .. } => check_limit(
                "error message length",
                message.len() as u64,
                limits::MAX_ERROR_MESSAGE as u64,
            ),
            Self::Pairing { pairing } => validate_pairing(pairing),
            Self::Control { control, .. } => validate_control(control),
        }
    }
}

fn validate_control(control: &ControlMessage) -> Result<(), ProtocolError> {
    match control {
        ControlMessage::ConsentRequested { disclosure } => check_limit(
            "control disclosure length",
            disclosure.len() as u64,
            limits::MAX_DISCLOSURE_BYTES as u64,
        ),
        ControlMessage::ConsentDenied { reason } => check_limit(
            "control reason length",
            reason.len() as u64,
            limits::MAX_REASON_BYTES as u64,
        ),
        // The reboot-resume reflections bound their variable-length field (the
        // ticket id) to the fixed identifier size, mirroring how the pairing
        // messages bound their fixed 32-byte values. The bound target identity
        // is fixed-shape; its coarse size is caught by the frame cap.
        ControlMessage::RebootResumeArmed { ticket_id, .. } => check_limit(
            "reboot-resume ticket id length",
            ticket_id.len() as u64,
            crate::REBOOT_RESUME_TICKET_ID_LEN as u64,
        ),
        ControlMessage::RebootResumeDisarmed { ticket_id } => check_limit(
            "reboot-resume ticket id length",
            ticket_id.len() as u64,
            crate::REBOOT_RESUME_TICKET_ID_LEN as u64,
        ),
        // The bilateral arming exchange and the post-reboot challenge/response
        // carry fixed 32-byte values (a SHA-256 commitment, the reboot-resume
        // nonces, the fresh challenge, and the HMAC proof); bound each so an
        // oversized field is rejected on both send and receive, exactly as the
        // pairing messages bound their fixed 32-byte values (ADR-0014 §2/§3).
        ControlMessage::RebootResumeArmCommit { na_commit } => check_limit(
            "reboot-resume arm commitment length",
            na_commit.len() as u64,
            crate::REBOOT_RESUME_NONCE_LEN as u64,
        ),
        ControlMessage::RebootResumeArmChallenge { nc_rr } => check_limit(
            "reboot-resume arm nonce length",
            nc_rr.len() as u64,
            crate::REBOOT_RESUME_NONCE_LEN as u64,
        ),
        ControlMessage::RebootResumeArmReveal { na_rr } => check_limit(
            "reboot-resume arm reveal length",
            na_rr.len() as u64,
            crate::REBOOT_RESUME_NONCE_LEN as u64,
        ),
        ControlMessage::RebootResumeChallenge { nc_prime } => check_limit(
            "reboot-resume challenge nonce length",
            nc_prime.len() as u64,
            crate::REBOOT_RESUME_NONCE_LEN as u64,
        ),
        ControlMessage::RebootResumeProof { ticket_id, proof } => {
            check_limit(
                "reboot-resume proof ticket id length",
                ticket_id.len() as u64,
                crate::REBOOT_RESUME_TICKET_ID_LEN as u64,
            )?;
            check_limit(
                "reboot-resume proof length",
                proof.len() as u64,
                crate::REBOOT_RESUME_PROOF_LEN as u64,
            )
        }
        // Bounded so the report always fits one record, and one line per note
        // so a peer cannot forge extra lines in a log or a report (ST-M009-012).
        ControlMessage::CleanupReported { notes, .. } => {
            check_limit(
                "cleanup report note count",
                notes.len() as u64,
                limits::MAX_CLEANUP_NOTES as u64,
            )?;
            for note in notes {
                check_limit(
                    "cleanup report note length",
                    note.len() as u64,
                    limits::MAX_CLEANUP_NOTE_BYTES as u64,
                )?;
                require_single_line(
                    note,
                    "a cleanup report note must be one line without control characters",
                )?;
            }
            Ok(())
        }
        // The bound target identity is fixed-shape; its coarse size is caught by
        // the frame cap, consistent with how `HelloAck` treats it.
        ControlMessage::ConsentGranted { .. }
        | ControlMessage::Revoke { .. }
        | ControlMessage::Heartbeat { .. }
        // Fieldless: it carries nothing to bound. Its meaning is enforced by
        // the runtime compare-gate, not by field limits (ST-M002-013).
        | ControlMessage::ApprovalCompareConfirmed {} => Ok(()),
    }
}

fn validate_pairing(pairing: &PairingMessage) -> Result<(), ProtocolError> {
    // The commitment and nonces are fixed 32-byte values; bound them so an
    // oversized field is rejected on both send and receive. The exact-length
    // conversion to `[u8; 32]` and the constant-time comparison happen in
    // `cretlink-transport`.
    match pairing {
        PairingMessage::Commit { commitment } => check_limit(
            "pairing commitment length",
            commitment.len() as u64,
            SAC_COMMITMENT_LEN as u64,
        ),
        // A display name is bounded by construction (`DisplayName::new`).
        PairingMessage::Nonce { nc, .. } => check_limit(
            "pairing nonce length",
            nc.len() as u64,
            SAC_NONCE_LEN as u64,
        ),
        PairingMessage::Reveal { na } => check_limit(
            "pairing reveal length",
            na.len() as u64,
            SAC_NONCE_LEN as u64,
        ),
    }
}

fn validate_operation(op: &Operation) -> Result<(), ProtocolError> {
    match op {
        Operation::SystemInfo {} | Operation::ListJobs {} => Ok(()),
        Operation::Exec {
            command,
            working_dir,
            env,
            timeout_ms,
            ..
        } => {
            check_limit(
                "exec command length",
                command.len() as u64,
                limits::MAX_COMMAND_BYTES as u64,
            )?;
            if let Some(dir) = working_dir {
                // The same adversarial rules as an absolute RemotePath (which
                // include the length bound), so a working directory can never
                // smuggle traversal, UNC, device-namespace, or ADS forms past
                // the file-path validation (ST-M003-003, REQ-F-009).
                validate_absolute_path("working_dir", dir)?;
            }
            validate_env(env)?;
            if let Some(timeout) = timeout_ms {
                check_limit("exec timeout_ms", *timeout, limits::MAX_TIMEOUT_MS)?;
            }
            Ok(())
        }
        Operation::TermOpen {
            cols,
            rows,
            working_dir,
            env,
            ..
        } => {
            validate_term_size(*cols, *rows)?;
            if let Some(dir) = working_dir {
                // The same adversarial path rules as `Exec.working_dir`.
                validate_absolute_path("working_dir", dir)?;
            }
            validate_env(env)
        }
        Operation::TermResize { cols, rows, .. } => validate_term_size(*cols, *rows),
        Operation::TermClose { .. } => Ok(()),
        Operation::WriteStdin { data, .. } => check_limit(
            "write_stdin data length",
            data.len() as u64,
            limits::MAX_CHUNK_BYTES as u64,
        ),
        Operation::ListStaged {} | Operation::StagingCleanup { .. } | Operation::FsDrives {} => {
            Ok(())
        }
        Operation::FsStat { path }
        | Operation::CreateDir { path }
        | Operation::FsDelete { path }
        | Operation::RemoveDir { path } => validate_remote_path("path", path),
        Operation::RemoveTree {
            path, folder_id, ..
        } => {
            validate_remote_path("path", path)?;
            if let Some(id) = folder_id {
                check_folder_id("remove_tree folder_id length", id)?;
            }
            Ok(())
        }
        Operation::FsAbort { dest } => validate_remote_path("dest", dest),
        Operation::FsList {
            path,
            continue_after,
        } => {
            validate_remote_path("path", path)?;
            if let Some(cursor) = continue_after {
                check_limit(
                    "fs_list continue_after length",
                    cursor.len() as u64,
                    limits::MAX_NAME_BYTES as u64,
                )?;
            }
            Ok(())
        }
        Operation::FsRead { path, offset, len } => {
            validate_remote_path("path", path)?;
            check_limit("fs_read offset", *offset, limits::MAX_FILE_OFFSET)?;
            check_limit(
                "fs_read len",
                u64::from(*len),
                limits::MAX_CHUNK_BYTES as u64,
            )
        }
        Operation::FsWrite { dest, offset, data } => {
            validate_remote_path("dest", dest)?;
            check_limit("fs_write offset", *offset, limits::MAX_FILE_OFFSET)?;
            check_limit(
                "fs_write data length",
                data.len() as u64,
                limits::MAX_CHUNK_BYTES as u64,
            )
        }
        Operation::FsCommit {
            dest,
            len,
            digest,
            source,
            ..
        } => {
            validate_remote_path("dest", dest)?;
            check_limit("fs_commit len", *len, limits::MAX_TRANSFER_BYTES)?;
            check_digest_len("fs_commit digest", digest)?;
            if let Some(label) = source {
                check_limit(
                    "fs_commit source length",
                    label.len() as u64,
                    limits::MAX_LABEL_BYTES as u64,
                )?;
            }
            Ok(())
        }
        Operation::FsDigest { path, prefix_len } => {
            validate_remote_path("path", path)?;
            if let Some(prefix) = prefix_len {
                check_limit("fs_digest prefix_len", *prefix, limits::MAX_FILE_OFFSET)?;
            }
            Ok(())
        }
        Operation::FsMove { from, to, .. } => {
            validate_remote_path("from", from)?;
            validate_remote_path("to", to)
        }
        Operation::DesktopCapture {
            max_width,
            max_height,
            quality,
            ..
        } => validate_desktop_capture(*max_width, *max_height, *quality),
        Operation::DesktopPointer { x, y, .. } => {
            check_desktop_magnitude("desktop_pointer x", *x, limits::MAX_DESKTOP_COORD)?;
            check_desktop_magnitude("desktop_pointer y", *y, limits::MAX_DESKTOP_COORD)
        }
        Operation::DesktopScroll { x, y, dx, dy } => {
            check_desktop_magnitude("desktop_scroll x", *x, limits::MAX_DESKTOP_COORD)?;
            check_desktop_magnitude("desktop_scroll y", *y, limits::MAX_DESKTOP_COORD)?;
            check_desktop_magnitude("desktop_scroll dx", *dx, limits::MAX_DESKTOP_SCROLL)?;
            check_desktop_magnitude("desktop_scroll dy", *dy, limits::MAX_DESKTOP_SCROLL)
        }
        Operation::DesktopKey { key, .. } => check_limit(
            "desktop_key key length",
            key.len() as u64,
            limits::MAX_DESKTOP_KEY_BYTES as u64,
        ),
        Operation::DesktopType { text } => check_limit(
            "desktop_type text length",
            text.len() as u64,
            limits::MAX_DESKTOP_TEXT_BYTES as u64,
        ),
        Operation::DesktopHotkey { keys } => {
            check_limit(
                "desktop_hotkey keys count",
                keys.len() as u64,
                limits::MAX_DESKTOP_HOTKEY_KEYS as u64,
            )?;
            for key in keys {
                check_limit(
                    "desktop_hotkey key length",
                    key.len() as u64,
                    limits::MAX_DESKTOP_KEY_BYTES as u64,
                )?;
            }
            Ok(())
        }
        Operation::DesktopWait { millis } => {
            check_limit("desktop_wait millis", *millis, limits::MAX_DESKTOP_WAIT_MS)
        }
        Operation::ClipboardGet {} => Ok(()),
        Operation::ClipboardSet { text } => check_limit(
            "clipboard_set text length",
            text.len() as u64,
            limits::MAX_CLIPBOARD_BYTES as u64,
        ),
    }
}

fn validate_result(result: &OpResult) -> Result<(), ProtocolError> {
    match result {
        // `TermOpened` carries only a job id (a fixed-shape integer), so like
        // `Ack` it has no bounded field to check.
        OpResult::SystemInfo { .. } | OpResult::Ack {} | OpResult::TermOpened { .. } => Ok(()),
        // The effective working directory is an agent-reported fact (it may
        // legitimately be a non-drive path on a simulated or test host), so it
        // is bounded rather than shape-validated.
        OpResult::ExecStarted { working_dir, .. } => check_limit(
            "exec_started working_dir length",
            working_dir.len() as u64,
            limits::MAX_PATH_BYTES as u64,
        ),
        OpResult::Jobs { jobs } => {
            check_limit(
                "jobs listed count",
                jobs.len() as u64,
                limits::MAX_JOBS_LISTED as u64,
            )?;
            for summary in jobs {
                check_limit(
                    "job command_preview length",
                    summary.command_preview.len() as u64,
                    limits::MAX_COMMAND_BYTES as u64,
                )?;
            }
            Ok(())
        }
        OpResult::PathStat { entry, .. } => {
            if let Some(entry) = entry {
                validate_file_entry(entry)?;
            }
            Ok(())
        }
        OpResult::DirEntries { entries, .. } => {
            check_limit(
                "dir_entries count",
                entries.len() as u64,
                limits::MAX_DIR_PAGE as u64,
            )?;
            for entry in entries {
                validate_file_entry(entry)?;
            }
            Ok(())
        }
        OpResult::FileData { data, .. } => check_limit(
            "file_data length",
            data.len() as u64,
            limits::MAX_CHUNK_BYTES as u64,
        ),
        OpResult::WriteAccepted { part_len } => check_limit(
            "write_accepted part_len",
            *part_len,
            limits::MAX_TRANSFER_BYTES,
        ),
        OpResult::Committed {
            canonical_path,
            len,
            digest,
            ..
        } => {
            check_limit(
                "committed canonical_path length",
                canonical_path.len() as u64,
                limits::MAX_PATH_BYTES as u64,
            )?;
            check_limit("committed len", *len, limits::MAX_TRANSFER_BYTES)?;
            check_digest_len("committed digest", digest)
        }
        OpResult::FileDigest { digest, len, .. } => {
            check_digest_len("file_digest digest", digest)?;
            check_limit("file_digest len", *len, limits::MAX_FILE_OFFSET)
        }
        OpResult::StagedList { entries } => {
            check_limit(
                "staged_list count",
                entries.len() as u64,
                limits::MAX_DIR_PAGE as u64,
            )?;
            for entry in entries {
                validate_staged_entry(entry)?;
            }
            Ok(())
        }
        OpResult::CleanupReport { failed, .. } => {
            check_limit(
                "cleanup_report failed count",
                failed.len() as u64,
                limits::MAX_DIR_PAGE as u64,
            )?;
            for failure in failed {
                check_limit(
                    "cleanup failure reason length",
                    failure.reason.len() as u64,
                    limits::MAX_REASON_BYTES as u64,
                )?;
            }
            Ok(())
        }
        OpResult::Drives { drives } => {
            check_limit(
                "drives count",
                drives.len() as u64,
                limits::MAX_DRIVES as u64,
            )?;
            for drive in drives {
                validate_drive_entry(drive)?;
            }
            Ok(())
        }
        OpResult::TreeRemoval {
            state,
            removed_files,
            removed_dirs,
            failed,
            failed_more,
            folder_id,
        } => {
            // A removal that goes on names the folder it goes on with, and
            // only such a removal does: an answer that ends a removal has no
            // next step to bind.
            match (state, folder_id) {
                (TreeState::More, Some(id)) => {
                    check_folder_id("tree_removal folder_id length", id)?;
                }
                (TreeState::More, None) => {
                    return Err(malformed(
                        "a folder removal that continues must name its folder",
                    ));
                }
                (_, Some(_)) => {
                    return Err(malformed(
                        "only a folder removal that continues names its folder",
                    ));
                }
                (_, None) => {}
            }
            // One step settles at most `MAX_TREE_STEP_ENTRIES` entries, so no
            // count of one step can be larger.
            check_limit(
                "tree_removal removed_files",
                *removed_files,
                limits::MAX_TREE_STEP_ENTRIES,
            )?;
            check_limit(
                "tree_removal removed_dirs",
                *removed_dirs,
                limits::MAX_TREE_STEP_ENTRIES,
            )?;
            check_limit(
                "tree_removal failed count",
                failed.len() as u64,
                limits::MAX_TREE_FAILURES as u64,
            )?;
            check_limit(
                "tree_removal failed_more",
                *failed_more,
                limits::MAX_TREE_STEP_ENTRIES,
            )?;
            for failure in failed {
                check_limit(
                    "tree_failure path length",
                    failure.path.len() as u64,
                    limits::MAX_TREE_FAILURE_PATH_BYTES as u64,
                )?;
                require_single_line(&failure.path, "a tree failure path must be one line")?;
            }
            Ok(())
        }
        OpResult::DesktopFrame {
            meta,
            image,
            staged,
        } => {
            check_limit(
                "desktop_frame desktop name length",
                meta.desktop.len() as u64,
                limits::MAX_DESKTOP_NAME_BYTES as u64,
            )?;
            if let Some(image) = image {
                check_limit(
                    "desktop_frame image length",
                    image.len() as u64,
                    limits::MAX_DESKTOP_INLINE_BYTES as u64,
                )?;
            }
            if let Some(path) = staged {
                validate_remote_path("desktop_frame staged", path)?;
            }
            Ok(())
        }
        OpResult::ClipboardContents { text } => {
            if let Some(text) = text {
                check_limit(
                    "clipboard_contents text length",
                    text.len() as u64,
                    limits::MAX_CLIPBOARD_BYTES as u64,
                )?;
            }
            Ok(())
        }
    }
}

fn validate_event(event: &EventKind) -> Result<(), ProtocolError> {
    match event {
        EventKind::JobOutput { data, .. } => check_limit(
            "job_output data length",
            data.len() as u64,
            limits::MAX_CHUNK_BYTES as u64,
        ),
        // `DesktopActivity` carries only the bounded-by-construction access
        // class (no length-bearing field), so there is nothing to check.
        EventKind::JobExited { .. }
        | EventKind::Disconnected { .. }
        | EventKind::DesktopActivity { .. } => Ok(()),
    }
}

/// Whether a flag is at its default `false`. Such a flag is omitted on the
/// wire, so a message without it keeps the form it had before the flag
/// existed (REQ-N-012).
fn is_false(flag: &bool) -> bool {
    !*flag
}

/// Whether a count is zero and therefore omitted on the wire.
fn is_zero(count: &u64) -> bool {
    *count == 0
}

/// Whether `text` is one line of plain text: no control character and no
/// line or paragraph separator.
fn is_single_line(text: &str) -> bool {
    !text
        .chars()
        .any(|c| c.is_control() || c == '\u{2028}' || c == '\u{2029}')
}

const fn check_limit(field: &'static str, len: u64, max: u64) -> Result<(), ProtocolError> {
    if len > max {
        Err(ProtocolError::LimitExceeded { field, len, max })
    } else {
        Ok(())
    }
}

/// Validates a bounded environment-override map, shared by [`Operation::Exec`]
/// and [`Operation::TermOpen`] so both enforce identical entry-count, key, and
/// value limits.
fn validate_env(env: &BTreeMap<String, String>) -> Result<(), ProtocolError> {
    check_limit(
        "env entry count",
        env.len() as u64,
        limits::MAX_ENV_ENTRIES as u64,
    )?;
    for (key, value) in env {
        check_limit(
            "env key length",
            key.len() as u64,
            limits::MAX_ENV_KEY as u64,
        )?;
        check_limit(
            "env value length",
            value.len() as u64,
            limits::MAX_ENV_VALUE as u64,
        )?;
    }
    Ok(())
}

/// Rejects a terminal dimension outside its inclusive bound (an out-of-bounds or
/// malformed `TermOpen`/`TermResize`), mapping to [`ErrorCode::LimitExceeded`]
/// (ST-M003-009). The bounds mirror `cretlink_exec::{MIN,MAX}_TERM_*` (equal by
/// static assertion in the exec crate), so a size the protocol accepts is always
/// one the pseudo-console backend accepts.
///
/// [`ErrorCode::LimitExceeded`]: crate::ErrorCode::LimitExceeded
const fn check_term_dim(
    field: &'static str,
    value: u16,
    min: u16,
    max: u16,
) -> Result<(), ProtocolError> {
    if value < min || value > max {
        return Err(ProtocolError::LimitExceeded {
            field,
            len: value as u64,
            max: max as u64,
        });
    }
    Ok(())
}

/// Validates a terminal `cols` by `rows` size against the mirrored bounds.
fn validate_term_size(cols: u16, rows: u16) -> Result<(), ProtocolError> {
    check_term_dim(
        "terminal cols",
        cols,
        limits::MIN_TERM_COLS,
        limits::MAX_TERM_COLS,
    )?;
    check_term_dim(
        "terminal rows",
        rows,
        limits::MIN_TERM_ROWS,
        limits::MAX_TERM_ROWS,
    )
}

/// Rejects an out-of-bounds desktop capture size hint or encoder quality
/// (ST-M005-001), mapping to [`ErrorCode::LimitExceeded`]. An oversized hint is
/// rejected outright rather than silently clamped, so a caller always learns
/// its request was out of range (REQ-N-008).
///
/// [`ErrorCode::LimitExceeded`]: crate::ErrorCode::LimitExceeded
fn validate_desktop_capture(
    max_width: Option<u16>,
    max_height: Option<u16>,
    quality: Option<u8>,
) -> Result<(), ProtocolError> {
    if let Some(width) = max_width {
        check_desktop_dim("desktop_capture max_width", width)?;
    }
    if let Some(height) = max_height {
        check_desktop_dim("desktop_capture max_height", height)?;
    }
    if let Some(quality) = quality {
        check_desktop_quality(quality)?;
    }
    Ok(())
}

/// Rejects a capture dimension hint outside
/// [`limits::MIN_DESKTOP_DIM`]..=[`limits::MAX_DESKTOP_DIM`].
const fn check_desktop_dim(field: &'static str, value: u16) -> Result<(), ProtocolError> {
    if value < limits::MIN_DESKTOP_DIM || value > limits::MAX_DESKTOP_DIM {
        return Err(ProtocolError::LimitExceeded {
            field,
            len: value as u64,
            max: limits::MAX_DESKTOP_DIM as u64,
        });
    }
    Ok(())
}

/// Rejects an encoder quality outside
/// [`limits::MIN_DESKTOP_QUALITY`]..=[`limits::MAX_DESKTOP_QUALITY`].
const fn check_desktop_quality(value: u8) -> Result<(), ProtocolError> {
    if value < limits::MIN_DESKTOP_QUALITY || value > limits::MAX_DESKTOP_QUALITY {
        return Err(ProtocolError::LimitExceeded {
            field: "desktop_capture quality",
            len: value as u64,
            max: limits::MAX_DESKTOP_QUALITY as u64,
        });
    }
    Ok(())
}

/// Rejects a desktop coordinate or scroll delta whose **magnitude** exceeds
/// `max` (ST-M005-002). Comparing on magnitude bounds a negative multi-monitor
/// coordinate symmetrically, and treats [`i32::MIN`] (whose positive value is
/// unrepresentable) as over-limit.
const fn check_desktop_magnitude(
    field: &'static str,
    value: i32,
    max: i32,
) -> Result<(), ProtocolError> {
    let magnitude = value.unsigned_abs() as u64;
    if magnitude > max as u64 {
        return Err(ProtocolError::LimitExceeded {
            field,
            len: magnitude,
            max: max as u64,
        });
    }
    Ok(())
}

/// Constructs a [`ProtocolError::InvalidPath`] for a rejected path.
const fn invalid_path(field: &'static str, reason: &'static str) -> ProtocolError {
    ProtocolError::InvalidPath { field, reason }
}

/// Validates a drive-qualified rooted remote path with the full adversarial
/// rules (`..`, UNC and device namespaces, reserved device names,
/// alternate-data-stream colons, bare drive designators, length and depth
/// bounds).
///
/// This is the same check [`Message::validate`] applies to absolute
/// [`RemotePath`]s and to `Exec.working_dir`; it is exposed so local front
/// ends (for example the MCP bridge) can reject an invalid path **before** it
/// travels anywhere, keeping the error local to the one bad call
/// (ST-M003-003, REQ-N-012).
///
/// # Errors
///
/// [`ProtocolError::InvalidPath`] or [`ProtocolError::LimitExceeded`] naming
/// `field` when the path breaks a rule.
pub fn validate_remote_absolute_path(field: &'static str, raw: &str) -> Result<(), ProtocolError> {
    validate_absolute_path(field, raw)
}

/// Validates a [`RemotePath`] on both ends before any operation runs (ADR-0012).
///
/// An [`RemotePath::Absolute`] must be a drive-qualified **rooted** path
/// (`X:\...` — a bare `X:` is drive-relative on Windows and rejected) with no
/// traversal, UNC or device namespace, reserved device name, or
/// alternate-data-stream colon; a [`RemotePath::Staging`] must be a single
/// safe component.
fn validate_remote_path(field: &'static str, path: &RemotePath) -> Result<(), ProtocolError> {
    match path {
        RemotePath::Absolute { path } => validate_absolute_path(field, path),
        RemotePath::Staging { name } => validate_staging_name(field, name),
    }
}

/// Validates a drive-qualified absolute path (see [`validate_remote_path`]).
fn validate_absolute_path(field: &'static str, raw: &str) -> Result<(), ProtocolError> {
    if raw.len() > limits::MAX_PATH_BYTES {
        return Err(ProtocolError::LimitExceeded {
            field,
            len: raw.len() as u64,
            max: limits::MAX_PATH_BYTES as u64,
        });
    }
    if raw.is_empty() {
        return Err(invalid_path(field, "empty path"));
    }
    if raw.chars().any(char::is_control) {
        return Err(invalid_path(field, "control character in path"));
    }
    // Analyse both separators uniformly so `C:/x` and `C:\x` are the same path.
    let normalized: String = raw
        .chars()
        .map(|c| if c == '/' { '\\' } else { c })
        .collect();
    // Two leading separators are UNC (`\\server\share`) or a device namespace
    // (`\\?\`, `\\.\`, and GLOBALROOT beneath them); reject the whole class.
    if normalized.starts_with("\\\\") {
        return Err(invalid_path(field, "UNC or device-namespace path"));
    }
    let mut components = normalized.split('\\').filter(|s| !s.is_empty());
    let Some(first) = components.next() else {
        return Err(invalid_path(field, "no path components"));
    };
    if !is_drive_designator(first) {
        return Err(invalid_path(field, "not a drive-qualified absolute path"));
    }
    // A bare drive designator ("C:") is drive-RELATIVE on Windows: Win32
    // resolves it against the acting process's per-drive current directory, so
    // the operation would run somewhere other than the path it names. Require
    // a separator directly after the drive (`X:\` or `X:/`). This deliberately
    // hardens the shared file-operation validator too (ST-M003-003).
    if normalized.as_bytes().get(2) != Some(&b'\\') {
        return Err(invalid_path(
            field,
            "drive-relative path (no separator after the drive designator)",
        ));
    }
    let mut depth = 0usize;
    for component in components {
        depth += 1;
        if depth > limits::MAX_PATH_DEPTH {
            return Err(ProtocolError::LimitExceeded {
                field,
                len: depth as u64,
                max: limits::MAX_PATH_DEPTH as u64,
            });
        }
        check_component(field, component)?;
    }
    Ok(())
}

/// Validates a single staging-namespace name (see [`validate_remote_path`]).
fn validate_staging_name(field: &'static str, name: &str) -> Result<(), ProtocolError> {
    if name.is_empty() {
        return Err(invalid_path(field, "empty staging name"));
    }
    if name.chars().any(char::is_control) {
        return Err(invalid_path(field, "control character in staging name"));
    }
    if name.contains('\\') || name.contains('/') {
        return Err(invalid_path(field, "path separator in staging name"));
    }
    check_component(field, name)
}

/// Rejects an unsafe single path component: traversal, an alternate-data-stream
/// colon, a reserved device name, an over-long name, or a trailing dot or space.
fn check_component(field: &'static str, component: &str) -> Result<(), ProtocolError> {
    if component.len() > limits::MAX_NAME_BYTES {
        return Err(ProtocolError::LimitExceeded {
            field,
            len: component.len() as u64,
            max: limits::MAX_NAME_BYTES as u64,
        });
    }
    if component == "." || component == ".." {
        return Err(invalid_path(field, "path traversal component"));
    }
    if component.contains(':') {
        return Err(invalid_path(field, "alternate-data-stream colon"));
    }
    if is_reserved_device_name(component) {
        return Err(invalid_path(field, "reserved device name"));
    }
    // Win32 strips a trailing dot or space, so `foo.` aliases `foo`.
    if component.ends_with('.') || component.ends_with(' ') {
        return Err(invalid_path(field, "trailing dot or space"));
    }
    Ok(())
}

/// Whether `s` is a two-character drive designator such as `C:`.
fn is_drive_designator(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() == 2 && b[0].is_ascii_alphabetic() && b[1] == b':'
}

/// Whether a component's stem names a reserved DOS device (`CON`, `PRN`, `AUX`,
/// `NUL`, `COM1`-`COM9`, `LPT1`-`LPT9`), which Win32 resolves regardless of the
/// directory or an appended extension.
fn is_reserved_device_name(component: &str) -> bool {
    let stem = component.split('.').next().unwrap_or(component);
    let upper = stem.to_ascii_uppercase();
    matches!(upper.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || is_numbered_device(&upper, "COM")
        || is_numbered_device(&upper, "LPT")
}

/// Whether `upper` is `prefix` followed by a single `1`-`9` digit.
fn is_numbered_device(upper: &str, prefix: &str) -> bool {
    upper
        .strip_prefix(prefix)
        .is_some_and(|rest| rest.len() == 1 && matches!(rest.as_bytes()[0], b'1'..=b'9'))
}

/// Bounds a digest to at most [`limits::DIGEST_LEN`] bytes.
fn check_digest_len(field: &'static str, digest: &B64Bytes) -> Result<(), ProtocolError> {
    check_limit(field, digest.len() as u64, limits::DIGEST_LEN as u64)
}

/// Bounds what identifies a folder between two steps of its removal: from 1
/// to [`limits::MAX_FOLDER_ID_BYTES`] bytes. An empty one identifies nothing.
fn check_folder_id(field: &'static str, id: &B64Bytes) -> Result<(), ProtocolError> {
    if id.is_empty() {
        return Err(malformed("a folder id is never empty"));
    }
    check_limit(field, id.len() as u64, limits::MAX_FOLDER_ID_BYTES as u64)
}

/// A message that breaks a rule of its schema that the types do not express.
fn malformed(rule: &'static str) -> ProtocolError {
    ProtocolError::Malformed(<serde_json::Error as serde::de::Error>::custom(rule))
}

/// Validates a [`FileEntry`] carried in a result.
fn validate_file_entry(entry: &FileEntry) -> Result<(), ProtocolError> {
    check_limit(
        "file_entry name length",
        entry.name.len() as u64,
        limits::MAX_NAME_BYTES as u64,
    )
}

/// Validates a [`DriveEntry`] carried in a result: every text bounded and one
/// line, every size within the JSON-safe range, and never more free space
/// than the drive holds.
fn validate_drive_entry(drive: &DriveEntry) -> Result<(), ProtocolError> {
    check_limit(
        "drive root length",
        drive.root.len() as u64,
        limits::MAX_DRIVE_ROOT_BYTES as u64,
    )?;
    require_single_line(&drive.root, "a drive root must be one line")?;
    if let Some(label) = &drive.label {
        check_limit(
            "drive label length",
            label.len() as u64,
            limits::MAX_DRIVE_LABEL_BYTES as u64,
        )?;
        require_single_line(label, "a drive label must be one line")?;
    }
    if let Some(total) = drive.total_bytes {
        check_limit("drive total_bytes", total, limits::MAX_FILE_OFFSET)?;
    }
    if let Some(free) = drive.free_bytes {
        check_limit(
            "drive free_bytes",
            free,
            drive.total_bytes.unwrap_or(limits::MAX_FILE_OFFSET),
        )?;
    }
    Ok(())
}

/// Rejects a text that is not one line (see [`is_single_line`]), so a peer
/// cannot forge extra lines in a log, a report or a list.
fn require_single_line(text: &str, rule: &'static str) -> Result<(), ProtocolError> {
    if is_single_line(text) {
        Ok(())
    } else {
        Err(malformed(rule))
    }
}

/// Validates a [`StagedEntry`] carried in a result.
fn validate_staged_entry(entry: &StagedEntry) -> Result<(), ProtocolError> {
    check_limit(
        "staged_entry canonical_path length",
        entry.canonical_path.len() as u64,
        limits::MAX_PATH_BYTES as u64,
    )?;
    check_limit(
        "staged_entry source length",
        entry.source.len() as u64,
        limits::MAX_LABEL_BYTES as u64,
    )?;
    check_digest_len("staged_entry digest", &entry.digest)
}
