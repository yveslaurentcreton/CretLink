//! Broker failure taxonomy.

use thiserror::Error;

/// A failure raised by the [`Broker`](crate::Broker).
///
/// Every variant is honest and secret-free: it names the failure without
/// echoing remote command output or arguments (REQ-N-015).
#[derive(Debug, Error)]
pub enum ExecError {
    /// The command could not be started (bad program, working directory, or
    /// permissions).
    #[error("failed to start the command")]
    Spawn(#[source] std::io::Error),

    /// The referenced job is not tracked by this broker (already finished,
    /// cancelled, or never existed).
    #[error("no such job")]
    UnknownJob,

    /// The broker is already tracking its maximum number of concurrent jobs;
    /// finish or cancel one before starting another (REQ-N-006 bounded state).
    #[error("too many concurrent jobs")]
    TooManyJobs,

    /// An I/O failure while feeding standard input or managing a job.
    #[error("job i/o error")]
    Io(#[source] std::io::Error),

    /// A requested terminal size is outside the accepted bounds
    /// ([`MIN_TERM_COLS`](crate::MIN_TERM_COLS)..=[`MAX_TERM_COLS`](crate::MAX_TERM_COLS)
    /// by
    /// [`MIN_TERM_ROWS`](crate::MIN_TERM_ROWS)..=[`MAX_TERM_ROWS`](crate::MAX_TERM_ROWS)).
    /// Malformed resize requests are rejected rather than acted on
    /// (ST-M003-008).
    #[error("terminal size out of bounds")]
    InvalidTermSize {
        /// The rejected column count.
        cols: u16,
        /// The rejected row count.
        rows: u16,
    },

    /// A resize was requested for a job that is not an interactive
    /// (pseudo-console) job; only interactive jobs have a terminal to resize.
    #[error("job is not interactive")]
    NotInteractive,

    /// An interactive (ConPTY) job was requested on a platform without a
    /// pseudo-console backend. Real targets are Windows; this keeps the crate
    /// honest on the `sh -c` fallback platforms rather than silently degrading
    /// an interactive request to a piped one.
    #[error("interactive jobs are only supported on Windows")]
    InteractiveUnsupported,

    /// A desktop capture or input operation was requested on a platform (or
    /// build) without a capture-and-input backend present. The agent only
    /// advertises `desktop.v1` where the backend exists, so a production peer
    /// never reaches this; it keeps the seam honest rather than fabricating a
    /// frame or silently dropping input (ADR-0015, REQ-S-009, REQ-N-008).
    #[error("desktop capture and input are only supported on Windows")]
    DesktopUnsupported,

    /// A desktop capture or input operation reached the backend but the
    /// operating system refused it (a failed `BitBlt`, `SendInput`, clipboard
    /// open, or encode). The message names the failure without echoing any
    /// captured pixels, clipboard text, or synthesized keystrokes (REQ-N-015).
    #[error("desktop operation failed")]
    DesktopBackend(#[source] std::io::Error),
}
