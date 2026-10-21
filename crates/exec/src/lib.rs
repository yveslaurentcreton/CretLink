//! CretLink execution broker (M003, ST-M003-001/002, ADR-0006).
//!
//! The [`Broker`] runs shell commands as tracked jobs, streams their separated
//! standard output and standard error incrementally, accepts standard input,
//! cancels and times them out, and reports **honest** completion outcomes. It
//! implements both halves of ADR-0006's dual-mode design: the piped
//! (non-interactive) mode is the default, and the ConPTY interactive mode is an
//! opt-in per job ([`JobMode::Interactive`], ST-M003-008) for programs that
//! require a real console.
//!
//! # Interactive (ConPTY) mode (ST-M003-008, ADR-0006 Mode B)
//!
//! A job spawned with [`JobMode::Interactive`] runs under a Windows pseudo-console
//! (ConPTY): a program that probes for a real terminal behaves correctly, and the
//! console's **merged** output is streamed as a single VT byte stream (delivered
//! as [`OutputStream::Stdout`] — ConPTY does not separate standard error). The
//! terminal can be resized within explicit bounds ([`Broker::resize`],
//! [`TermSize`]). Interactivity changes nothing about containment or honesty: an
//! interactive job is bound to the same tracked Job Object as a piped job, so a
//! cancel, timeout, or revocation terminates its whole process tree
//! (REQ-S-014), and a crash or unknown termination is reported as
//! [`JobOutcome::Unknown`], never a fabricated clean exit (REQ-N-008). The
//! interactive backend is Windows-only; requesting it elsewhere fails with
//! [`ExecError::InteractiveUnsupported`] rather than silently degrading to a
//! piped job.
//!
//! # Streaming and backpressure (REQ-N-006)
//!
//! Every job's output and its terminal [`JobDone`] flow as [`BrokerEvent`]s over
//! one bounded channel returned by [`Broker::new`]. The bound plus each job's
//! fixed read buffer cap in-flight memory; when the **consumer of this channel**
//! is slow, the readers block on the channel, the OS pipe buffer fills, and the
//! child process blocks on write — genuine backpressure to the broker's
//! consumer, with no unbounded buffering inside the broker. (How far that
//! backpressure propagates beyond the broker is the consumer's policy: the
//! session agent forwards promptly and lets TCP flow control carry it, while the
//! console side bounds its own memory with a documented drop-and-mark policy —
//! see `cretlink_session`.)
//!
//! # Process-tree termination (REQ-S-014, REQ-S-009)
//!
//! On Windows the child is created suspended and assigned to CretLink's own
//! kill-on-close Job Object before it runs (see the `platform` module), so
//! cancelling, timing out, or revoking a job terminates the whole process tree
//! through `TerminateJobObject`, not just the direct child. Dropping a job's
//! handle without a kill reaps its tree too, and a hard kill of the agent closes
//! the Job Object handle, which reaps it through kill-on-close — so a detached
//! grandchild is never orphaned (REQ-S-009). Off Windows the broker runs
//! `sh -c` with the child in its own process group
//! (`process_wrap::ProcessGroup`) and signals the whole group (real targets are
//! Windows; this keeps the crate building and tested everywhere).
//!
//! # Honest outcomes (REQ-N-008)
//!
//! A finished job reports [`JobOutcome::Completed`] only on a genuine success
//! exit. A non-zero exit is [`JobOutcome::Failed`] with the real code; a hit
//! deadline is [`JobOutcome::TimedOut`]; a cancel is [`JobOutcome::Cancelled`];
//! and an exit the broker cannot classify (for example a kill by something other
//! than the broker) is [`JobOutcome::Unknown`] — never dressed up as success.
//!
//! # Untrusted output
//!
//! Job output bytes are untrusted remote data (they may carry control sequences
//! or sensitive text). The broker passes them through unmodified and never logs
//! their content.

mod broker;
#[cfg(windows)]
mod conpty;
mod desktop;
mod error;
mod fs;
mod platform;

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

pub use broker::{Broker, JobSummary, SpawnedJob};
pub use desktop::{
    Admission, CaptureGovernor, CaptureLimits, CaptureReject, CaptureRequest, CapturedFrame,
    Desktop, InputAdmission, InputGovernor, InputLimits,
};
pub use error::ExecError;
pub use fs::{
    FileStore, FsError, SessionStaging, StagingCensus, StartupSweepReport, default_staging_base,
};

// Reuse the wire vocabulary so the session layer forwards broker output without
// lossy conversions.
pub use cretlink_protocol::{JobOutcome, OutputStream, Shell};

/// The largest slice the broker reads from a pipe in one step, and therefore the
/// largest [`OutputChunk`] it emits. Kept at or below the protocol chunk ceiling
/// so a chunk always fits the wire schema; the session layer re-chunks further
/// to fit a single encrypted record.
pub const READ_CHUNK_BYTES: usize = 64 * 1024;

// A chunk must never exceed the protocol's per-chunk ceiling.
const _: () = assert!(READ_CHUNK_BYTES <= cretlink_protocol::limits::MAX_CHUNK_BYTES);

/// The maximum number of jobs one broker tracks concurrently; further
/// [`Broker::spawn`] calls fail with [`ExecError::TooManyJobs`] (bounded state,
/// REQ-N-006).
pub const MAX_CONCURRENT_JOBS: usize = cretlink_protocol::limits::MAX_JOBS_LISTED;

/// A broker-local job identifier, assigned by [`Broker::spawn`]. It is distinct
/// from the wire [`cretlink_protocol::JobId`]; the session layer maps between
/// them (one-to-one, sharing the same integer value).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct JobId(pub u64);

impl core::fmt::Display for JobId {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        core::fmt::Display::fmt(&self.0, f)
    }
}

/// The smallest accepted terminal column/row count for an interactive job.
pub const MIN_TERM_COLS: u16 = 1;
/// The smallest accepted terminal row count for an interactive job.
pub const MIN_TERM_ROWS: u16 = 1;
/// The largest accepted terminal column count. A deliberate, explicit ceiling:
/// oversized resize requests are rejected rather than acted on (ST-M003-008),
/// and the value stays well within the `i16` range a `COORD` can carry.
pub const MAX_TERM_COLS: u16 = 1000;
/// The largest accepted terminal row count (see [`MAX_TERM_COLS`]).
pub const MAX_TERM_ROWS: u16 = 1000;

// The wire protocol mirrors these terminal bounds (a size the protocol accepts
// must be one this backend accepts). The protocol cannot depend on this crate,
// so equality is asserted here — the one place that sees both constants
// (ST-M003-009).
const _: () = assert!(MIN_TERM_COLS == cretlink_protocol::limits::MIN_TERM_COLS);
const _: () = assert!(MIN_TERM_ROWS == cretlink_protocol::limits::MIN_TERM_ROWS);
const _: () = assert!(MAX_TERM_COLS == cretlink_protocol::limits::MAX_TERM_COLS);
const _: () = assert!(MAX_TERM_ROWS == cretlink_protocol::limits::MAX_TERM_ROWS);

/// A validated terminal size (columns by rows) for an interactive job.
///
/// Constructed only through [`TermSize::new`], which enforces the
/// [`MIN_TERM_COLS`]..=[`MAX_TERM_COLS`] by [`MIN_TERM_ROWS`]..=[`MAX_TERM_ROWS`]
/// bounds, so a `TermSize` in hand is always in range — an out-of-bounds or
/// malformed size can never reach the pseudo-console (ST-M003-008).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TermSize {
    cols: u16,
    rows: u16,
}

impl TermSize {
    /// Builds a size after bounds-checking `cols` and `rows`.
    ///
    /// # Errors
    ///
    /// [`ExecError::InvalidTermSize`] if either dimension is outside its bound.
    pub fn new(cols: u16, rows: u16) -> Result<Self, ExecError> {
        if !(MIN_TERM_COLS..=MAX_TERM_COLS).contains(&cols)
            || !(MIN_TERM_ROWS..=MAX_TERM_ROWS).contains(&rows)
        {
            return Err(ExecError::InvalidTermSize { cols, rows });
        }
        Ok(Self { cols, rows })
    }

    /// The (in-bounds) column count.
    #[must_use]
    pub fn cols(self) -> u16 {
        self.cols
    }

    /// The (in-bounds) row count.
    #[must_use]
    pub fn rows(self) -> u16 {
        self.rows
    }
}

/// How a job's standard streams and console are set up.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum JobMode {
    /// Piped, non-interactive: separate standard output and standard error over
    /// anonymous pipes (ADR-0006 Mode A). The default.
    #[default]
    Piped,
    /// Interactive: the job runs under a Windows pseudo-console (ConPTY,
    /// ADR-0006 Mode B) sized to `size`, its output merged into one VT byte
    /// stream. Windows-only.
    Interactive {
        /// The initial terminal size.
        size: TermSize,
    },
}

/// A command to run as a job.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobSpec {
    /// Which shell interprets [`Self::command`].
    pub shell: Shell,
    /// The command text.
    pub command: String,
    /// The working directory, or the broker's current directory when `None`.
    pub working_dir: Option<PathBuf>,
    /// Extra environment variables layered onto the broker's environment.
    pub env: BTreeMap<String, String>,
    /// A deadline after which the job is killed and reported
    /// [`JobOutcome::TimedOut`].
    pub timeout: Option<Duration>,
    /// Piped (default) or interactive pseudo-console execution.
    pub mode: JobMode,
}

impl JobSpec {
    /// Builds a piped spec for `command` in `shell` with no working directory,
    /// environment overrides, or timeout.
    #[must_use]
    pub fn new(shell: Shell, command: impl Into<String>) -> Self {
        Self {
            shell,
            command: command.into(),
            working_dir: None,
            env: BTreeMap::new(),
            timeout: None,
            mode: JobMode::Piped,
        }
    }

    /// Sets the job deadline.
    #[must_use]
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = Some(timeout);
        self
    }

    /// Runs the job under an interactive pseudo-console of `size` instead of
    /// piped streams (ADR-0006 Mode B). Windows-only; a broker on another
    /// platform rejects it with [`ExecError::InteractiveUnsupported`].
    #[must_use]
    pub fn interactive(mut self, size: TermSize) -> Self {
        self.mode = JobMode::Interactive { size };
        self
    }
}

/// One incremental chunk of a job's output.
#[derive(Clone, PartialEq, Eq)]
pub struct OutputChunk {
    /// The job that produced the bytes.
    pub job: JobId,
    /// Which stream the bytes came from.
    pub stream: OutputStream,
    /// The raw bytes (at most [`READ_CHUNK_BYTES`]). Untrusted remote data.
    pub bytes: Vec<u8>,
}

// Redact the content in Debug: output bytes are untrusted and may be sensitive.
impl core::fmt::Debug for OutputChunk {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("OutputChunk")
            .field("job", &self.job)
            .field("stream", &self.stream)
            .field("bytes", &format_args!("<{} bytes>", self.bytes.len()))
            .finish()
    }
}

/// A job's terminal report — its honest outcome (REQ-N-008).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobDone {
    /// The job that ended.
    pub job: JobId,
    /// The process exit code, when the platform exposed one.
    pub exit_code: Option<i32>,
    /// Wall-clock duration from spawn to termination.
    pub duration: Duration,
    /// The honest outcome.
    pub outcome: JobOutcome,
}

/// An event streamed from the broker: either a chunk of output or a job's
/// terminal report. For any single job, all of its [`Self::Output`] events
/// precede its [`Self::Done`] event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BrokerEvent {
    /// A chunk of a job's standard output or standard error.
    Output(OutputChunk),
    /// A job ended.
    Done(JobDone),
}
