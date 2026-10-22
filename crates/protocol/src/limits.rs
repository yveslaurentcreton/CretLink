//! Hard size caps for every protocol message field (REQ-N-012, ADR-0010).
//!
//! [`crate::Message::validate`] enforces each limit; the frame cap is also
//! checked by `cretlink-transport` before allocating a receive buffer and by
//! [`crate::Message::decode`] before parsing.

/// Maximum encoded size of one wire frame (JSON payload), in bytes.
pub const MAX_FRAME_BYTES: usize = 1024 * 1024;

/// Maximum byte length of an `Exec` command string.
pub const MAX_COMMAND_BYTES: usize = 32 * 1024;

/// Maximum byte length of a path such as `Exec::working_dir`.
pub const MAX_PATH_BYTES: usize = 4096;

/// Maximum number of environment entries in one `Exec` request.
pub const MAX_ENV_ENTRIES: usize = 32;

/// Maximum byte length of one environment variable name.
pub const MAX_ENV_KEY: usize = 128;

/// Maximum byte length of one environment variable value.
pub const MAX_ENV_VALUE: usize = 4096;

/// Maximum decoded byte length of one binary chunk (`JobOutput` data,
/// `WriteStdin` data).
pub const MAX_CHUNK_BYTES: usize = 256 * 1024;

/// Maximum byte length of an `Error` message string.
pub const MAX_ERROR_MESSAGE: usize = 4096;

/// Maximum number of capability strings in one `Hello`.
pub const MAX_CAPABILITIES: usize = 32;

/// Maximum byte length of one capability string.
pub const MAX_CAPABILITY_LEN: usize = 64;

/// Maximum number of job summaries in one `Jobs` response.
pub const MAX_JOBS_LISTED: usize = 256;

/// Maximum `Exec` timeout, in milliseconds (24 hours).
pub const MAX_TIMEOUT_MS: u64 = 86_400_000;

/// Maximum byte length of the full-control disclosure summary text carried in a
/// `ConsentRequested` control message (ST-M002-005). Comfortably fits the
/// honest, human-readable consent language without allowing an unbounded blob.
pub const MAX_DISCLOSURE_BYTES: usize = 8 * 1024;

/// Maximum byte length of a human-readable reason string on a control message
/// (consent denial, revocation; ST-M002-005/006). Secret-free and bounded.
pub const MAX_REASON_BYTES: usize = 1024;

// --- Stated purpose of AI actions (REQ-F-026, ST-M009-007) ---

/// Maximum length of a stated purpose ([`crate::Purpose`]), in Unicode scalar
/// values (characters) after trimming. A purpose is one short plain-language
/// sentence a non-technical customer reads next to the real operation, so the
/// bound is on characters rather than bytes; at most 4 bytes each, a purpose
/// never exceeds `4 * MAX_PURPOSE_CHARS` bytes on the wire.
pub const MAX_PURPOSE_CHARS: usize = 200;

// --- File operations and transfer (ADR-0012, ST-M003-004/005) ---

/// Maximum byte length of a single path component (a file or directory name).
///
/// NTFS allows 255 UTF-16 code units, which encode to at most 765 UTF-8 bytes,
/// so a 255-byte cap would reject legal filenames. Rounded up to 768.
pub const MAX_NAME_BYTES: usize = 768;

/// Maximum number of path components in a [`crate::RemotePath::Absolute`] path.
///
/// Bounds the work done by path validation and agent-side canonicalization even
/// within the [`MAX_PATH_BYTES`] byte budget, so a deeply nested path cannot
/// force an unbounded component walk.
pub const MAX_PATH_DEPTH: usize = 64;

/// Maximum number of directory entries returned in one `FsList` page.
pub const MAX_DIR_PAGE: usize = 256;

/// Maximum number of file-transfer operations the console keeps in flight at
/// once (ADR-0012, ST-M003-005 AC1).
///
/// The console-side transfer engine pipelines `FsWrite`/`FsRead` chunks up to
/// this window before it must await an acknowledgement, which bounds in-flight
/// memory and the number of unacknowledged wire requests, giving genuine
/// backpressure without unbounded buffering (REQ-N-006). It is a client-side
/// pacing constant, not a validated wire field.
pub const MAX_INFLIGHT_FILE_OPS: usize = 4;

/// Maximum addressable file offset or length, in bytes.
///
/// Kept within JSON's safe-integer range (`2^53 - 1`) so an offset can never
/// lose precision passing through a JSON number.
pub const MAX_FILE_OFFSET: u64 = (1 << 53) - 1;

/// Maximum total size of one committed transfer artifact, in bytes (4 GiB).
pub const MAX_TRANSFER_BYTES: u64 = 4 * 1024 * 1024 * 1024;

/// Maximum total bytes retained in one session's staging root (8 GiB).
pub const MAX_STAGING_BYTES: u64 = 8 * 1024 * 1024 * 1024;

/// Maximum byte length of a technician-side `source` label on a staged
/// artifact (`FsCommit`, `StagedEntry`).
pub const MAX_LABEL_BYTES: usize = 512;

/// Length in bytes of a SHA-256 digest ([`crate::DigestAlgo::Sha256`]).
pub const DIGEST_LEN: usize = 32;

// --- Interactive terminal (ADR-0013, ST-M003-009) ---

/// The smallest accepted terminal column count for a `TermOpen`/`TermResize`.
///
/// The terminal bounds mirror the pseudo-console backend's
/// `cretlink_exec::MIN_TERM_COLS`/`MAX_TERM_COLS` (and rows) so a size that the
/// protocol accepts is always one the ConPTY backend accepts. The protocol
/// cannot depend on the exec crate (the dependency runs the other way), so the
/// values are mirrored here and their equality is asserted **in the exec crate**,
/// which sees both constants (see `cretlink_exec`'s static assertions).
pub const MIN_TERM_COLS: u16 = 1;
/// The smallest accepted terminal row count (see [`MIN_TERM_COLS`]).
pub const MIN_TERM_ROWS: u16 = 1;
/// The largest accepted terminal column count (see [`MIN_TERM_COLS`]). An
/// oversized `TermOpen`/`TermResize` dimension is rejected, never acted on
/// (ST-M003-009).
pub const MAX_TERM_COLS: u16 = 1000;
/// The largest accepted terminal row count (see [`MIN_TERM_COLS`]).
pub const MAX_TERM_ROWS: u16 = 1000;

// --- Interactive desktop control (ADR-0015, ST-M005-001/002/003) ---

/// The maximum decoded byte length of one **inline** captured desktop frame
/// image (`DesktopFrame::image`), in bytes.
///
/// A desktop capture returns its encoded image inline only when it fits this
/// bound; a larger frame is written to the session staging area and returned as
/// a staged-artifact reference the console reads back through the ordinary
/// [`crate::Operation::FsRead`] path (ADR-0015). Kept well under
/// [`MAX_CHUNK_BYTES`] so an inline frame plus its metadata never approaches
/// [`MAX_FRAME_BYTES`] after base64 expansion.
pub const MAX_DESKTOP_INLINE_BYTES: usize = 128 * 1024;

/// The smallest accepted capture dimension hint (`DesktopCapture::max_width`/
/// `max_height`), in pixels.
pub const MIN_DESKTOP_DIM: u16 = 16;

/// The largest accepted capture dimension hint, in pixels. Comfortably covers
/// an 8K-class display's longest edge; an oversized hint is rejected, never
/// clamped silently (REQ-N-008).
pub const MAX_DESKTOP_DIM: u16 = 16_384;

/// The smallest accepted encoder quality (`DesktopCapture::quality`), on a
/// 1..=100 scale.
pub const MIN_DESKTOP_QUALITY: u8 = 1;

/// The largest accepted encoder quality (`DesktopCapture::quality`).
pub const MAX_DESKTOP_QUALITY: u8 = 100;

/// The maximum magnitude of a pointer coordinate (`DesktopPointer`) or a scroll
/// origin (`DesktopScroll`), in virtual-desktop pixels. Coordinates may be
/// negative on a multi-monitor virtual desktop, so the bound is on magnitude.
pub const MAX_DESKTOP_COORD: i32 = 1 << 20;

/// The maximum magnitude of a single scroll delta (`DesktopScroll::dx`/`dy`),
/// in wheel detents.
pub const MAX_DESKTOP_SCROLL: i32 = 1 << 16;

/// The maximum byte length of one key token (`DesktopKey::key`, a
/// `DesktopHotkey` chord element). A token names a single virtual key (for
/// example `"enter"`, `"f5"`, `"ctrl"`), so a small bound suffices.
pub const MAX_DESKTOP_KEY_BYTES: usize = 32;

/// The maximum number of chord elements in one `DesktopHotkey`.
pub const MAX_DESKTOP_HOTKEY_KEYS: usize = 8;

/// The maximum byte length of the Unicode text of one `DesktopType` action.
pub const MAX_DESKTOP_TEXT_BYTES: usize = 4096;

/// The maximum single `DesktopWait` duration, in milliseconds. A pacing pause
/// between actions is bounded so a request cannot park the input pipeline
/// indefinitely.
pub const MAX_DESKTOP_WAIT_MS: u64 = 60_000;

/// The maximum byte length of the window-station\desktop identity string
/// carried in [`crate::DesktopFrameMeta::desktop`].
pub const MAX_DESKTOP_NAME_BYTES: usize = 256;

/// The maximum decoded byte length of clipboard text exchanged by `ClipboardGet`
/// / `ClipboardSet`. Kept within a single wire frame after JSON escaping.
pub const MAX_CLIPBOARD_BYTES: usize = 256 * 1024;

// --- Desktop capture backpressure (REQ-N-006, ST-M005-001) ---
//
// The desktop capture model is on-demand / paced-refresh (ADR-0015), not a
// video codec, so the bound that actively bites under load is the refresh
// *rate*: a capture requested sooner than the interval below is paced (the
// agent waits out the remainder before capturing), coalescing a burst down to
// at most this rate rather than letting a runaway caller drive capture — and
// its CPU, GPU, and bandwidth cost — without bound. The in-flight and memory
// bounds cap the agent's peak capture memory; they hold by construction while
// the agent serves one capture at a time and hands each frame straight to the
// transport, and become the load-bearing runtime guard once staged-frame
// retention (ADR-0015) lets more than one frame be held at once.

/// The minimum wall-clock spacing between two admitted desktop captures, in
/// milliseconds (the paced-refresh ceiling; ~20 captures/second). A capture
/// requested sooner is paced, never dropped; on-demand captures spaced further
/// apart than this are never delayed. Kept small so the pacing delay a capture
/// can add before a revoke is observed is bounded (REQ-N-006, REQ-S-004).
pub const MIN_DESKTOP_CAPTURE_INTERVAL_MS: u64 = 50;

/// The most desktop capture frames that may be in flight — encoded by the
/// backend but not yet released to the transport — at once. An admission beyond
/// this is refused ([`crate::ErrorCode::StorageExhausted`]), never queued
/// without bound (REQ-N-006).
pub const MAX_DESKTOP_IN_FLIGHT: u32 = 3;

/// The most encoded desktop-frame bytes that may be retained in flight across
/// all in-flight captures at once. A capture that would push the held total
/// past this budget is refused rather than admitted, bounding the agent's peak
/// capture memory (REQ-N-006). Sized to hold several inline frames
/// ([`MAX_DESKTOP_INLINE_BYTES`]) plus one larger staged frame.
pub const MAX_DESKTOP_CAPTURE_MEMORY_BYTES: usize = 8 * 1024 * 1024;

// --- Desktop input rate limiting (REQ-N-006, ST-M005-002) ---
//
// Synthesized pointer, scroll, key, type, and hotkey actions are paced the same
// way captures are: a runaway automation loop must not be able to drive
// `SendInput` on the customer's live desktop without bound. Input actions are
// cheap, synchronous, and lossless to defer, so the governor *paces* a burst
// down to a sustained ceiling (it never silently drops a click), tolerating a
// short burst of back-to-back actions first. The bound is deliberately far
// above any human input rate — it caps a synthetic flood, not an operator.

/// The sustained wall-clock floor between two admitted desktop input actions,
/// in milliseconds (~100 actions/second sustained). An action requested sooner
/// than this — after the burst allowance is spent — is paced (the agent waits
/// out the remainder), never dropped. Kept small so the pacing delay an input
/// action can add before a revoke is observed is bounded (REQ-S-004).
pub const MIN_DESKTOP_INPUT_INTERVAL_MS: u64 = 10;

/// The most desktop input actions that may fire back-to-back before pacing
/// engages (the burst tolerance). A normal click-move-type sequence is absorbed
/// without delay; a sustained flood is paced to
/// [`MIN_DESKTOP_INPUT_INTERVAL_MS`]. One `DesktopType` run counts as a single
/// action regardless of how many keystrokes it synthesizes.
pub const MAX_DESKTOP_INPUT_BURST: u32 = 20;
