//! The [`Broker`]: job table, spawning, streaming, stdin, cancel, and honest
//! completion.

use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::process::ExitStatus;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::ChildStdin;
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;

use crate::error::ExecError;
use crate::platform;
use crate::{
    BrokerEvent, JobDone, JobId, JobMode, JobOutcome, JobSpec, MAX_CONCURRENT_JOBS, OutputChunk,
    OutputStream, READ_CHUNK_BYTES, TermSize,
};

/// Default capacity of the broker's bounded event channel. Together with
/// [`READ_CHUNK_BYTES`] this caps in-flight output memory and drives
/// backpressure (REQ-N-006).
pub const EVENT_CHANNEL_CAPACITY: usize = 64;

/// Bounded standard-input queue depth per job.
const STDIN_CHANNEL_CAPACITY: usize = 16;

/// Bounded depth of an interactive job's resize-request queue.
#[cfg(windows)]
const RESIZE_CHANNEL_CAPACITY: usize = 16;

/// Longest command text kept as a [`JobSummary`] preview.
const PREVIEW_MAX_BYTES: usize = 200;

/// The most finished jobs kept as `ListJobs` history. Older finished jobs
/// are pruned so a long-lived broker's memory stays bounded. Mirrors the
/// simulator's `FINISHED_JOB_HISTORY` (same name and value) so a real agent
/// and a simulated target report the same discoverable state (ST-M003-003).
const FINISHED_JOB_HISTORY: usize = 64;

/// One entry in the broker's job table.
#[derive(Debug)]
struct JobEntry {
    preview: String,
    /// Sends bytes to the job's standard-input writer task; `None` if the job
    /// has no writable standard input.
    stdin_tx: Option<mpsc::Sender<Vec<u8>>>,
    /// Fires the job's cancel signal exactly once.
    cancel_tx: Option<oneshot::Sender<()>>,
    /// Sends resize requests to an interactive job's waiter; `None` for piped
    /// jobs, which have no terminal to resize.
    resize_tx: Option<mpsc::Sender<TermSize>>,
}

/// One recently finished job retained for [`Broker::list_jobs`] history.
#[derive(Debug)]
struct FinishedJob {
    job: JobId,
    preview: String,
}

/// The broker's job state under one lock: running jobs plus a bounded history
/// of recently finished ones (oldest first).
#[derive(Debug, Default)]
struct JobTable {
    running: HashMap<JobId, JobEntry>,
    finished: VecDeque<FinishedJob>,
}

impl JobTable {
    /// Moves `job` from the running table into the bounded finished history.
    fn retire(&mut self, job: JobId) {
        if let Some(entry) = self.running.remove(&job) {
            self.finished.push_back(FinishedJob {
                job,
                preview: entry.preview,
            });
            while self.finished.len() > FINISHED_JOB_HISTORY {
                self.finished.pop_front();
            }
        }
    }
}

/// A summary of one tracked job, for [`Broker::list_jobs`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobSummary {
    /// The job identifier.
    pub job: JobId,
    /// A bounded preview of the command that started the job.
    pub command_preview: String,
    /// Whether the job is still running (`false` for a job in the bounded
    /// finished history).
    pub running: bool,
}

/// A successfully started job: its identifier plus the effective working
/// directory the process actually received — the requested directory, or the
/// broker process's current directory at spawn time (ST-M003-003, REQ-F-009).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpawnedJob {
    /// The identifier assigned to the new job.
    pub job: JobId,
    /// The effective working directory. Empty only when the current directory
    /// could not be determined — never a fabricated path (REQ-N-008).
    pub working_dir: PathBuf,
}

/// Runs shell commands as tracked, streamed, cancellable jobs.
///
/// Create one with [`Broker::new`], which also yields the bounded
/// [`BrokerEvent`] receiver the caller drains. See the crate docs for the
/// streaming, backpressure, tree-termination, and honest-outcome contracts.
#[derive(Debug)]
pub struct Broker {
    jobs: Arc<Mutex<JobTable>>,
    events_tx: mpsc::Sender<BrokerEvent>,
    next_id: AtomicU64,
    max_jobs: usize,
}

impl Broker {
    /// Creates a broker and its bounded event receiver.
    #[must_use]
    pub fn new() -> (Self, mpsc::Receiver<BrokerEvent>) {
        Self::with_capacity(EVENT_CHANNEL_CAPACITY)
    }

    /// Creates a broker whose event channel holds at most `capacity` events.
    #[must_use]
    pub fn with_capacity(capacity: usize) -> (Self, mpsc::Receiver<BrokerEvent>) {
        let (events_tx, events_rx) = mpsc::channel(capacity.max(1));
        (
            Self {
                jobs: Arc::new(Mutex::new(JobTable::default())),
                events_tx,
                next_id: AtomicU64::new(1),
                max_jobs: MAX_CONCURRENT_JOBS,
            },
            events_rx,
        )
    }

    /// Locks the job table, recovering the inner state if a holder panicked (no
    /// reachable path panics while holding it; the workspace lints forbid it).
    fn lock(&self) -> MutexGuard<'_, JobTable> {
        self.jobs.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Spawns `spec` as a new job and returns its identifier together with the
    /// effective working directory the process received (ST-M003-003,
    /// REQ-F-009). Output and the terminal report stream over the
    /// [`BrokerEvent`] receiver.
    ///
    /// A piped job ([`JobMode::Piped`], the default) streams separate standard
    /// output and error; an interactive job ([`JobMode::Interactive`]) runs under
    /// a Windows pseudo-console and streams one merged VT stream (ST-M003-008).
    ///
    /// # Errors
    ///
    /// [`ExecError::TooManyJobs`] when at the concurrency cap,
    /// [`ExecError::Spawn`] when the process cannot start, or
    /// [`ExecError::InteractiveUnsupported`] for an interactive job off Windows.
    pub async fn spawn(&self, spec: JobSpec) -> Result<SpawnedJob, ExecError> {
        if self.lock().running.len() >= self.max_jobs {
            return Err(ExecError::TooManyJobs);
        }
        // Resolved at spawn time so the report is the path the child actually
        // receives: the requested directory (the spawn fails outright when it
        // does not exist), or this process's current directory.
        let working_dir = effective_working_dir(&spec);
        let job = match spec.mode {
            JobMode::Piped => self.spawn_piped(spec).await,
            JobMode::Interactive { size } => {
                #[cfg(windows)]
                {
                    self.spawn_interactive(spec, size).await
                }
                #[cfg(not(windows))]
                {
                    let _ = (spec, size);
                    Err(ExecError::InteractiveUnsupported)
                }
            }
        }?;
        Ok(SpawnedJob { job, working_dir })
    }

    /// Spawns a piped (non-interactive) job over separate stdout/stderr pipes
    /// (ADR-0006 Mode A).
    async fn spawn_piped(&self, spec: JobSpec) -> Result<JobId, ExecError> {
        let start = Instant::now();
        let id = JobId(self.next_id.fetch_add(1, Ordering::Relaxed));
        let preview = command_preview(&spec.command);
        let timeout = spec.timeout;

        let mut spawned = platform::spawn(&spec)?;
        let stdout = spawned.take_stdout();
        let stderr = spawned.take_stderr();
        let stdin = spawned.take_stdin();

        let mut readers: Vec<JoinHandle<()>> = Vec::new();
        if let Some(out) = stdout {
            readers.push(tokio::spawn(pump(
                out,
                id,
                OutputStream::Stdout,
                self.events_tx.clone(),
            )));
        }
        if let Some(err) = stderr {
            readers.push(tokio::spawn(pump(
                err,
                id,
                OutputStream::Stderr,
                self.events_tx.clone(),
            )));
        }

        let stdin_tx = stdin.map(|handle| {
            let (tx, rx) = mpsc::channel::<Vec<u8>>(STDIN_CHANNEL_CAPACITY);
            tokio::spawn(feed_stdin(handle, rx));
            tx
        });

        let (cancel_tx, cancel_rx) = oneshot::channel();

        // Insert before spawning the waiter so the waiter's self-removal can
        // never race ahead of the insert (which would leak a stale entry).
        self.lock().running.insert(
            id,
            JobEntry {
                preview,
                stdin_tx,
                cancel_tx: Some(cancel_tx),
                resize_tx: None,
            },
        );

        tokio::spawn(wait_job(
            spawned,
            timeout,
            cancel_rx,
            readers,
            Arc::clone(&self.jobs),
            id,
            self.events_tx.clone(),
            start,
        ));

        Ok(id)
    }

    /// Spawns an interactive pseudo-console (ConPTY) job (ADR-0006 Mode B,
    /// ST-M003-008). The merged VT output is forwarded as [`OutputStream::Stdout`];
    /// the job shares the same tracked Job Object containment as a piped job.
    #[cfg(windows)]
    async fn spawn_interactive(&self, spec: JobSpec, size: TermSize) -> Result<JobId, ExecError> {
        let start = Instant::now();
        let id = JobId(self.next_id.fetch_add(1, Ordering::Relaxed));
        let preview = command_preview(&spec.command);
        let timeout = spec.timeout;

        let crate::conpty::Conpty {
            output_rx,
            stdin_tx,
            exit_rx,
            control,
        } = crate::conpty::spawn(&spec, size)?;

        // Forward the merged VT output as Stdout chunks (ConPTY has no separate
        // stderr). The task drains until the reader closes the channel at
        // shutdown, so every Output precedes the Done on the ordered channel.
        let forward = tokio::spawn(forward_conpty_output(output_rx, id, self.events_tx.clone()));

        let (resize_tx, resize_rx) = mpsc::channel::<TermSize>(RESIZE_CHANNEL_CAPACITY);
        let (cancel_tx, cancel_rx) = oneshot::channel();

        // Insert before spawning the waiter (see `spawn_piped`).
        self.lock().running.insert(
            id,
            JobEntry {
                preview,
                stdin_tx: Some(stdin_tx),
                cancel_tx: Some(cancel_tx),
                resize_tx: Some(resize_tx),
            },
        );

        tokio::spawn(wait_job_interactive(
            control,
            exit_rx,
            timeout,
            cancel_rx,
            resize_rx,
            forward,
            Arc::clone(&self.jobs),
            id,
            self.events_tx.clone(),
            start,
        ));

        Ok(id)
    }

    /// Resizes a running interactive job's pseudo-console.
    ///
    /// The size is already validated by [`TermSize`], so an out-of-bounds or
    /// malformed size cannot be requested here (ST-M003-008).
    ///
    /// # Errors
    ///
    /// [`ExecError::UnknownJob`] if the job is not tracked (or already ended),
    /// or [`ExecError::NotInteractive`] if it is a piped job with no terminal.
    pub async fn resize(&self, job: JobId, size: TermSize) -> Result<(), ExecError> {
        // Clone the sender and release the lock before awaiting the send.
        let sender = match self.lock().running.get(&job) {
            Some(entry) => entry.resize_tx.clone(),
            None => return Err(ExecError::UnknownJob),
        };
        match sender {
            Some(tx) => tx.send(size).await.map_err(|_| ExecError::UnknownJob),
            None => Err(ExecError::NotInteractive),
        }
    }

    /// Whether `job` is a running **interactive** (pseudo-console) job — the only
    /// kind a terminal resize or close may target. An interactive job is exactly
    /// one that carries a resize channel, so this distinguishes it from a piped
    /// job without exposing the [`JobMode`]. A terminal client uses it to refuse a
    /// `TermClose`/`TermResize` aimed at a plain piped job (ST-M003-009).
    ///
    /// # Errors
    ///
    /// [`ExecError::UnknownJob`] if the job is not tracked (finished or never
    /// existed).
    pub fn is_interactive(&self, job: JobId) -> Result<bool, ExecError> {
        match self.lock().running.get(&job) {
            Some(entry) => Ok(entry.resize_tx.is_some()),
            None => Err(ExecError::UnknownJob),
        }
    }

    /// Feeds bytes to a running job's standard input.
    ///
    /// # Errors
    ///
    /// [`ExecError::UnknownJob`] if the job is not tracked or has no writable
    /// standard input (for example it already ended).
    pub async fn write_stdin(&self, job: JobId, bytes: Vec<u8>) -> Result<(), ExecError> {
        // Clone the sender and release the lock before awaiting the send, so a
        // slow stdin consumer never blocks the whole job table.
        let sender = match self.lock().running.get(&job) {
            Some(entry) => entry.stdin_tx.clone(),
            None => return Err(ExecError::UnknownJob),
        };
        match sender {
            Some(tx) => tx.send(bytes).await.map_err(|_| ExecError::UnknownJob),
            None => Err(ExecError::UnknownJob),
        }
    }

    /// Requests cancellation of one job: its process **tree** is terminated and
    /// it reports [`JobOutcome::Cancelled`]. Idempotent.
    ///
    /// # Errors
    ///
    /// [`ExecError::UnknownJob`] if the job is not tracked.
    pub fn cancel(&self, job: JobId) -> Result<(), ExecError> {
        match self.lock().running.get_mut(&job) {
            Some(entry) => {
                if let Some(tx) = entry.cancel_tx.take() {
                    let _ = tx.send(());
                }
                Ok(())
            }
            None => Err(ExecError::UnknownJob),
        }
    }

    /// Requests cancellation of every tracked job (revocation/teardown, fail
    /// closed — REQ-S-004). Each job's terminal [`BrokerEvent::Done`] still
    /// arrives on the event receiver.
    pub fn cancel_all(&self) {
        for entry in self.lock().running.values_mut() {
            if let Some(tx) = entry.cancel_tx.take() {
                let _ = tx.send(());
            }
        }
    }

    /// The identifiers of every currently running job.
    #[must_use]
    pub fn job_ids(&self) -> Vec<JobId> {
        let mut ids: Vec<JobId> = self.lock().running.keys().copied().collect();
        ids.sort_unstable();
        ids
    }

    /// Summaries of the jobs the broker tracks: running jobs first, then the
    /// remaining space holds the most recent finished jobs (`running: false`),
    /// so history can never push live work out of the bounded listing —
    /// mirroring the simulator's `ListJobs` behaviour (ST-M003-003).
    #[must_use]
    pub fn list_jobs(&self) -> Vec<JobSummary> {
        let table = self.lock();
        let mut jobs: Vec<JobSummary> = table
            .running
            .iter()
            .take(self.max_jobs)
            .map(|(id, entry)| JobSummary {
                job: *id,
                command_preview: entry.preview.clone(),
                running: true,
            })
            .collect();
        let room = self.max_jobs.saturating_sub(jobs.len());
        let mut finished: Vec<JobSummary> = table
            .finished
            .iter()
            .rev()
            .take(room)
            .map(|done| JobSummary {
                job: done.job,
                command_preview: done.preview.clone(),
                running: false,
            })
            .collect();
        drop(table);
        jobs.append(&mut finished);
        jobs.sort_by_key(|summary| summary.job);
        jobs
    }
}

impl Drop for Broker {
    fn drop(&mut self) {
        // Fail-closed safety net: signal every still-tracked job to cancel so a
        // dropped broker never silently leaves a process tree running. Waiter
        // tasks (which hold their own table handle) then kill and reap.
        self.cancel_all();
    }
}

/// Reads one stream to EOF, emitting bounded output chunks. Backpressure comes
/// from the bounded event channel: a full channel blocks the read, which fills
/// the OS pipe and blocks the child (REQ-N-006). Stops on EOF, a read error, or
/// a gone consumer.
async fn pump<R>(mut reader: R, job: JobId, stream: OutputStream, tx: mpsc::Sender<BrokerEvent>)
where
    R: tokio::io::AsyncRead + Unpin,
{
    let mut buf = vec![0u8; READ_CHUNK_BYTES];
    loop {
        match reader.read(&mut buf).await {
            Ok(0) => break,
            Ok(n) => {
                let chunk = OutputChunk {
                    job,
                    stream,
                    bytes: buf[..n].to_vec(),
                };
                if tx.send(BrokerEvent::Output(chunk)).await.is_err() {
                    break;
                }
            }
            Err(_) => break,
        }
    }
}

/// Owns a job's standard input, writing queued bytes until the queue closes or a
/// write fails, then dropping the handle (which closes stdin, giving the child
/// EOF).
async fn feed_stdin(mut stdin: ChildStdin, mut rx: mpsc::Receiver<Vec<u8>>) {
    while let Some(bytes) = rx.recv().await {
        if stdin.write_all(&bytes).await.is_err() {
            break;
        }
        if stdin.flush().await.is_err() {
            break;
        }
    }
}

/// What ended a job.
enum How {
    Exited(std::io::Result<ExitStatus>),
    TimedOut,
    Cancelled,
}

/// Awaits a job's completion, deadline, or cancel; kills the tree when needed;
/// drains output; then reports the honest terminal outcome (REQ-N-008).
#[allow(clippy::too_many_arguments)]
async fn wait_job(
    mut spawned: platform::Spawned,
    timeout: Option<Duration>,
    cancel_rx: oneshot::Receiver<()>,
    readers: Vec<JoinHandle<()>>,
    jobs: Arc<Mutex<JobTable>>,
    job: JobId,
    tx: mpsc::Sender<BrokerEvent>,
    start: Instant,
) {
    let how = tokio::select! {
        status = spawned.wait() => How::Exited(status),
        () = sleep_opt(timeout) => How::TimedOut,
        _ = cancel_rx => How::Cancelled,
    };

    let (exit_code, outcome) = match how {
        How::Exited(Ok(status)) => classify_exit(status),
        How::Exited(Err(_)) => (None, JobOutcome::Unknown),
        How::TimedOut => {
            let _ = spawned.kill_tree();
            let _ = spawned.wait().await;
            (None, JobOutcome::TimedOut)
        }
        How::Cancelled => {
            let _ = spawned.kill_tree();
            let _ = spawned.wait().await;
            (None, JobOutcome::Cancelled)
        }
    };

    // Drain all output before the terminal event: the readers end when their
    // pipes hit EOF (after the process dies), so joining them guarantees every
    // Output for this job precedes its Done on the ordered channel.
    for reader in readers {
        let _ = reader.await;
    }

    let duration = start.elapsed();
    // Retire rather than forget: the job stays discoverable as bounded
    // finished history (ST-M003-003).
    jobs.lock()
        .unwrap_or_else(PoisonError::into_inner)
        .retire(job);

    let _ = tx
        .send(BrokerEvent::Done(JobDone {
            job,
            exit_code,
            duration,
            outcome,
        }))
        .await;
}

/// Forwards an interactive job's merged pseudo-console output as
/// [`OutputStream::Stdout`] chunks, with the same bounded backpressure as
/// [`pump`]: a full event channel blocks the forward, which blocks the reader
/// thread, fills the OS pipe, and blocks the child (REQ-N-005, REQ-N-006).
#[cfg(windows)]
async fn forward_conpty_output(
    mut rx: mpsc::Receiver<Vec<u8>>,
    job: JobId,
    tx: mpsc::Sender<BrokerEvent>,
) {
    while let Some(bytes) = rx.recv().await {
        let chunk = OutputChunk {
            job,
            stream: OutputStream::Stdout,
            bytes,
        };
        if tx.send(BrokerEvent::Output(chunk)).await.is_err() {
            break;
        }
    }
}

/// Awaits an interactive job's exit, deadline, or cancel while servicing resize
/// requests; terminates the tree when needed; tears down the pseudo-console
/// deadlock-safely; then reports the honest outcome (REQ-N-008). A kill or an
/// unreadable exit code becomes [`JobOutcome::Unknown`], never a clean exit.
#[cfg(windows)]
#[allow(clippy::too_many_arguments)]
async fn wait_job_interactive(
    control: crate::conpty::ConptyControl,
    mut exit_rx: oneshot::Receiver<Option<i32>>,
    timeout: Option<Duration>,
    mut cancel_rx: oneshot::Receiver<()>,
    mut resize_rx: mpsc::Receiver<TermSize>,
    forward: JoinHandle<()>,
    jobs: Arc<Mutex<JobTable>>,
    job: JobId,
    tx: mpsc::Sender<BrokerEvent>,
    start: Instant,
) {
    /// What ended an interactive job.
    enum End {
        Exited(Option<i32>),
        TimedOut,
        Cancelled,
    }

    let sleep = sleep_opt(timeout);
    tokio::pin!(sleep);
    let mut resize_open = true;

    let end = loop {
        tokio::select! {
            code = &mut exit_rx => break End::Exited(code.ok().flatten()),
            () = &mut sleep => break End::TimedOut,
            _ = &mut cancel_rx => break End::Cancelled,
            maybe = resize_rx.recv(), if resize_open => match maybe {
                // Resize requests never end the job; apply and keep waiting.
                Some(size) => {
                    let _ = control.resize(size);
                }
                None => resize_open = false,
            },
        }
    };

    let (exit_code, outcome) = match end {
        End::Exited(Some(0)) => (Some(0), JobOutcome::Completed),
        End::Exited(Some(code)) => (Some(code), JobOutcome::Failed),
        End::Exited(None) => (None, JobOutcome::Unknown),
        End::TimedOut => {
            control.terminate();
            (None, JobOutcome::TimedOut)
        }
        End::Cancelled => {
            control.terminate();
            (None, JobOutcome::Cancelled)
        }
    };

    // Tear down the pseudo-console on a blocking thread while `forward` keeps
    // draining output, so `ClosePseudoConsole` cannot deadlock (see conpty docs).
    let _ = tokio::task::spawn_blocking(move || control.shutdown()).await;

    // The reader is joined inside shutdown, closing the output channel, so
    // awaiting the forwarder guarantees every Output precedes the Done.
    let _ = forward.await;

    let duration = start.elapsed();
    // Retire rather than forget (see `wait_job`).
    jobs.lock()
        .unwrap_or_else(PoisonError::into_inner)
        .retire(job);

    let _ = tx
        .send(BrokerEvent::Done(JobDone {
            job,
            exit_code,
            duration,
            outcome,
        }))
        .await;
}

/// The effective working directory a job receives: the requested directory
/// verbatim; otherwise the system folder on Windows, and this process's
/// current directory at spawn time elsewhere. On Windows a job never starts in
/// the working directory of this program, which whoever started the program
/// chose (ST-M006-020; see `crate::system`). A current directory that cannot
/// be read yields an empty path — reported honestly rather than invented
/// (REQ-N-008).
fn effective_working_dir(spec: &JobSpec) -> PathBuf {
    crate::system::working_dir_of(spec)
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_default())
}

/// Sleeps for `d`, or never resolves when there is no deadline.
async fn sleep_opt(d: Option<Duration>) {
    match d {
        Some(d) => tokio::time::sleep(d).await,
        None => std::future::pending::<()>().await,
    }
}

/// Maps a natural process exit to an honest outcome and exit code.
fn classify_exit(status: ExitStatus) -> (Option<i32>, JobOutcome) {
    if status.success() {
        (status.code(), JobOutcome::Completed)
    } else if let Some(code) = status.code() {
        (Some(code), JobOutcome::Failed)
    } else {
        // No exit code and not a broker-driven kill: killed by something else
        // (for example a Unix signal). Never claim success (REQ-N-008).
        (None, JobOutcome::Unknown)
    }
}

/// Truncates a command to a bounded, char-boundary-safe preview.
fn command_preview(command: &str) -> String {
    let mut out = String::new();
    for ch in command.chars() {
        if out.len() + ch.len_utf8() > PREVIEW_MAX_BYTES {
            break;
        }
        out.push(ch);
    }
    out
}

#[cfg(test)]
mod tests;
