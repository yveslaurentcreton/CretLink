//! Broker unit tests.
//!
//! These run on every OS: real commands go through the two Windows shells on
//! Windows and through the `sh -c` fallback elsewhere, so the assertions hold on
//! all three CI platforms. The Job Object tree-kill guarantee (REQ-S-014) is
//! Windows-only and covered by its own `cfg(windows)` test.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::time::Duration;

use tokio::sync::mpsc;
use tokio::time::timeout;

use crate::{
    Broker, BrokerEvent, ExecError, JobDone, JobId, JobOutcome, JobSpec, OutputStream, Shell,
    TermSize,
};

/// A valid interactive terminal size for the ConPTY tests.
fn term_size() -> TermSize {
    TermSize::new(80, 24).unwrap()
}

/// Everything collected for one job, up to and including its terminal report.
struct Collected {
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    chunks: usize,
    done: JobDone,
}

/// Drains the broker's event stream for `job` until its `Done`, failing (rather
/// than hanging) on a stall.
async fn drain_job(rx: &mut mpsc::Receiver<BrokerEvent>, job: JobId) -> Collected {
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    let mut chunks = 0usize;
    loop {
        let event = timeout(Duration::from_secs(20), rx.recv())
            .await
            .expect("broker event stream stalled")
            .expect("broker event stream closed before Done");
        match event {
            BrokerEvent::Output(chunk) if chunk.job == job => {
                chunks += 1;
                match chunk.stream {
                    OutputStream::Stdout => stdout.extend_from_slice(&chunk.bytes),
                    OutputStream::Stderr => stderr.extend_from_slice(&chunk.bytes),
                }
            }
            BrokerEvent::Done(done) if done.job == job => {
                return Collected {
                    stdout,
                    stderr,
                    chunks,
                    done,
                };
            }
            _ => {}
        }
    }
}

/// A command that runs far longer than any test needs (~30s), for timeout and
/// cancel tests. Bounded so a regressed kill fails fast rather than hanging CI.
fn long_running() -> JobSpec {
    #[cfg(windows)]
    {
        JobSpec::new(Shell::Cmd, "ping -n 30 127.0.0.1 > nul")
    }
    #[cfg(not(windows))]
    {
        JobSpec::new(Shell::Cmd, "sleep 30")
    }
}

/// A command that reads one line of standard input, echoes it to standard
/// output (line-buffered, flushed), and exits. Avoids `findstr`/`cat`, which
/// buffer piped stdout and so never surface the echo before EOF.
fn stdin_echo() -> JobSpec {
    #[cfg(windows)]
    {
        JobSpec::new(
            Shell::PowerShell,
            "$l = [Console]::In.ReadLine(); if ($null -ne $l) { [Console]::Out.WriteLine($l) }",
        )
    }
    #[cfg(not(windows))]
    {
        JobSpec::new(Shell::Cmd, "head -n 1")
    }
}

/// A command that writes exactly `n` bytes to standard output.
fn big_output(n: usize) -> JobSpec {
    #[cfg(windows)]
    {
        JobSpec::new(
            Shell::PowerShell,
            format!("[Console]::Out.Write('x' * {n})"),
        )
    }
    #[cfg(not(windows))]
    {
        JobSpec::new(Shell::Cmd, format!("yes x | head -c {n}"))
    }
}

#[tokio::test]
async fn echo_captures_stdout_and_completes() {
    let (broker, mut rx) = Broker::new();
    let job = broker
        .spawn(JobSpec::new(Shell::Cmd, "echo hello"))
        .await
        .unwrap()
        .job;
    let c = drain_job(&mut rx, job).await;
    assert_eq!(c.done.outcome, JobOutcome::Completed);
    assert_eq!(c.done.exit_code, Some(0));
    assert!(
        String::from_utf8_lossy(&c.stdout).contains("hello"),
        "stdout was {:?}",
        String::from_utf8_lossy(&c.stdout)
    );
}

#[tokio::test]
async fn failing_command_reports_the_real_exit_code() {
    let (broker, mut rx) = Broker::new();
    let job = broker
        .spawn(JobSpec::new(Shell::Cmd, "exit 3"))
        .await
        .unwrap()
        .job;
    let c = drain_job(&mut rx, job).await;
    assert_eq!(c.done.outcome, JobOutcome::Failed);
    assert_eq!(c.done.exit_code, Some(3));
}

#[tokio::test]
async fn stderr_is_streamed_separately() {
    let (broker, mut rx) = Broker::new();
    let job = broker
        .spawn(JobSpec::new(Shell::Cmd, "echo boom 1>&2"))
        .await
        .unwrap()
        .job;
    let c = drain_job(&mut rx, job).await;
    assert!(
        String::from_utf8_lossy(&c.stderr).contains("boom"),
        "stderr was {:?}",
        String::from_utf8_lossy(&c.stderr)
    );
    assert!(c.stdout.is_empty(), "stdout should be empty");
}

#[tokio::test]
async fn timeout_kills_the_job_and_reports_timed_out() {
    let (broker, mut rx) = Broker::new();
    let job = broker
        .spawn(long_running().with_timeout(Duration::from_millis(300)))
        .await
        .unwrap()
        .job;
    let c = drain_job(&mut rx, job).await;
    assert_eq!(c.done.outcome, JobOutcome::TimedOut);
    // The command would run for 300s; a timely TimedOut proves it was killed.
    assert!(
        c.done.duration < Duration::from_secs(30),
        "timed-out job should end promptly, took {:?}",
        c.done.duration
    );
}

#[tokio::test]
async fn cancel_terminates_the_job_and_reports_cancelled() {
    let (broker, mut rx) = Broker::new();
    let job = broker.spawn(long_running()).await.unwrap().job;
    // Let the process get going before cancelling.
    tokio::time::sleep(Duration::from_millis(200)).await;
    broker.cancel(job).unwrap();
    let c = drain_job(&mut rx, job).await;
    assert_eq!(c.done.outcome, JobOutcome::Cancelled);
}

#[tokio::test]
async fn cancel_of_unknown_job_is_reported() {
    let (broker, _rx) = Broker::new();
    assert!(matches!(
        broker.cancel(JobId(9999)),
        Err(ExecError::UnknownJob)
    ));
}

#[tokio::test]
async fn stdin_is_delivered_and_echoed() {
    let (broker, mut rx) = Broker::new();
    let job = broker.spawn(stdin_echo()).await.unwrap().job;
    broker.write_stdin(job, b"ping\n".to_vec()).await.unwrap();

    // The echo command reads one line, writes it back, and exits.
    let c = drain_job(&mut rx, job).await;
    assert_eq!(c.done.outcome, JobOutcome::Completed);
    assert!(
        String::from_utf8_lossy(&c.stdout).contains("ping"),
        "stdout was {:?}",
        String::from_utf8_lossy(&c.stdout)
    );
}

#[tokio::test]
async fn write_stdin_to_unknown_job_is_reported() {
    let (broker, _rx) = Broker::new();
    let err = broker.write_stdin(JobId(9999), b"x".to_vec()).await;
    assert!(matches!(err, Err(ExecError::UnknownJob)));
}

#[tokio::test]
async fn high_output_is_chunked_and_bounded() {
    let n = 200_000;
    let (broker, mut rx) = Broker::new();
    let job = broker.spawn(big_output(n)).await.unwrap().job;
    let c = drain_job(&mut rx, job).await;
    assert_eq!(c.done.outcome, JobOutcome::Completed);
    assert!(
        c.stdout.len() >= n,
        "expected at least {n} bytes, got {}",
        c.stdout.len()
    );
    assert!(
        c.chunks >= 2,
        "high output should arrive in multiple bounded chunks, got {}",
        c.chunks
    );
}

#[tokio::test]
async fn list_jobs_shows_running_work() {
    let (broker, mut rx) = Broker::new();
    let job = broker.spawn(long_running()).await.unwrap().job;
    tokio::time::sleep(Duration::from_millis(50)).await;
    let jobs = broker.list_jobs();
    assert!(
        jobs.iter().any(|s| s.job == job && s.running),
        "list_jobs should include the running job"
    );
    // Drain to completion so the kill runs synchronously here rather than
    // racing the runtime shutdown (which could leave the process lingering).
    broker.cancel(job).unwrap();
    let c = drain_job(&mut rx, job).await;
    assert_eq!(c.done.outcome, JobOutcome::Cancelled);
}

/// A finished job stays discoverable as bounded history with `running: false`,
/// mirroring the simulator's `ListJobs` behaviour (ST-M003-003).
#[tokio::test]
async fn list_jobs_keeps_finished_history() {
    let (broker, mut rx) = Broker::new();
    let finished = broker
        .spawn(JobSpec::new(Shell::Cmd, "echo history"))
        .await
        .unwrap()
        .job;
    let c = drain_job(&mut rx, finished).await;
    assert_eq!(c.done.outcome, JobOutcome::Completed);

    let running = broker.spawn(long_running()).await.unwrap().job;
    tokio::time::sleep(Duration::from_millis(50)).await;

    let jobs = broker.list_jobs();
    assert!(
        jobs.iter().any(|s| s.job == finished && !s.running),
        "the finished job must be listed as history with running=false, got {jobs:?}"
    );
    assert!(
        jobs.iter().any(|s| s.job == running && s.running),
        "the running job must still be listed, got {jobs:?}"
    );

    broker.cancel(running).unwrap();
    let c = drain_job(&mut rx, running).await;
    assert_eq!(c.done.outcome, JobOutcome::Cancelled);
}

// --- Finished-history bound and non-displacement (ST-M003-003) ---

/// A synthetic job-table entry with no live process behind it, for exercising
/// the bounded-history bookkeeping without spawning dozens of processes.
fn synthetic_entry(preview: &str) -> super::JobEntry {
    super::JobEntry {
        preview: preview.to_string(),
        stdin_tx: None,
        cancel_tx: None,
        resize_tx: None,
    }
}

/// `retire` keeps at most `FINISHED_JOB_HISTORY` entries and evicts the
/// oldest first, so a long-lived broker's memory stays bounded.
#[test]
fn finished_history_is_bounded_and_evicts_the_oldest() {
    let mut table = super::JobTable::default();
    let total = super::FINISHED_JOB_HISTORY + 5;
    for index in 0..total {
        let job = JobId(index as u64 + 1);
        table
            .running
            .insert(job, synthetic_entry(&format!("job-{}", index + 1)));
        table.retire(job);
    }

    assert_eq!(table.finished.len(), super::FINISHED_JOB_HISTORY);
    let kept: Vec<u64> = table.finished.iter().map(|done| done.job.0).collect();
    // The five oldest were evicted; the newest survives at the back.
    assert_eq!(kept.first().copied(), Some(6));
    assert_eq!(kept.last().copied(), Some(total as u64));
}

/// History can never displace live work in `list_jobs`: with a full running
/// table no finished job is listed, and freed slots are filled only with the
/// most recent finished jobs, keeping the listing at its bound.
#[test]
fn history_never_displaces_running_work_in_list_jobs() {
    let (broker, _rx) = Broker::new();
    {
        let mut table = broker.jobs.lock().unwrap();
        for index in 0..crate::MAX_CONCURRENT_JOBS {
            let job = JobId(1000 + index as u64);
            table
                .running
                .insert(job, synthetic_entry(&format!("running-{job}")));
        }
        for index in 0..super::FINISHED_JOB_HISTORY {
            let job = JobId(index as u64 + 1);
            table.finished.push_back(super::FinishedJob {
                job,
                preview: format!("finished-{job}"),
            });
        }
    }

    // A full running table leaves no room: every listed job is running.
    let jobs = broker.list_jobs();
    assert_eq!(jobs.len(), crate::MAX_CONCURRENT_JOBS);
    assert!(
        jobs.iter().all(|summary| summary.running),
        "history must never displace running work"
    );

    // Free three running slots: exactly the three MOST RECENT finished jobs
    // fill the room, and the listing stays at its bound.
    {
        let mut table = broker.jobs.lock().unwrap();
        for index in 0..3u64 {
            table.running.remove(&JobId(1000 + index));
        }
    }
    let jobs = broker.list_jobs();
    assert_eq!(jobs.len(), crate::MAX_CONCURRENT_JOBS);
    assert_eq!(
        jobs.iter().filter(|summary| summary.running).count(),
        crate::MAX_CONCURRENT_JOBS - 3,
        "every remaining running job is still listed"
    );
    let mut listed_finished: Vec<u64> = jobs
        .iter()
        .filter(|summary| !summary.running)
        .map(|summary| summary.job.0)
        .collect();
    listed_finished.sort_unstable();
    let newest = super::FINISHED_JOB_HISTORY as u64;
    assert_eq!(
        listed_finished,
        vec![newest - 2, newest - 1, newest],
        "only the most recent finished jobs fill the freed room"
    );
}

// --- Effective working directory (ST-M003-003, REQ-F-009) ---

/// A cleaned, test-private directory under the OS temp root.
fn temp_dir_for(test: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("cretlink-broker-{test}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create temp dir");
    dir
}

/// A command that writes a relative file, so where it lands proves the job's
/// actual working directory.
fn relative_write() -> JobSpec {
    #[cfg(windows)]
    {
        JobSpec::new(Shell::Cmd, "echo marker> cwd-proof.txt")
    }
    #[cfg(not(windows))]
    {
        JobSpec::new(Shell::Cmd, "echo marker > cwd-proof.txt")
    }
}

/// A relative write inside the job lands in the requested working directory,
/// and the spawn reports exactly that directory as the effective one.
#[tokio::test]
async fn requested_working_dir_receives_relative_writes() {
    let dir = temp_dir_for("cwd");
    let (broker, mut rx) = Broker::new();
    let mut spec = relative_write();
    spec.working_dir = Some(dir.clone());

    let started = broker.spawn(spec).await.unwrap();
    assert_eq!(
        started.working_dir, dir,
        "the reported effective working directory is the requested one"
    );
    let c = drain_job(&mut rx, started.job).await;
    assert_eq!(c.done.outcome, JobOutcome::Completed);

    let proof = dir.join("cwd-proof.txt");
    let contents = std::fs::read_to_string(&proof)
        .unwrap_or_else(|e| panic!("relative write must land in {}: {e}", proof.display()));
    assert!(contents.contains("marker"));
    let _ = std::fs::remove_dir_all(&dir);
}

/// Without a requested directory the job starts in — and the spawn reports —
/// the system folder on Windows, never the folder this program was started in
/// (ST-M006-020), and the broker process's current directory elsewhere.
#[tokio::test]
async fn default_working_dir_is_the_system_folder_on_windows() {
    let (broker, mut rx) = Broker::new();
    // `cd` alone prints the working directory in Command Prompt.
    let command = if cfg!(windows) {
        "cd"
    } else {
        "echo default-cwd"
    };
    let started = broker
        .spawn(JobSpec::new(Shell::Cmd, command))
        .await
        .unwrap();
    let expected = crate::system::job_folder()
        .unwrap_or_else(|| std::env::current_dir().expect("current dir"));
    assert_eq!(started.working_dir, expected);
    let c = drain_job(&mut rx, started.job).await;
    assert_eq!(c.done.outcome, JobOutcome::Completed);
    if cfg!(windows) {
        // The job says itself where it runs.
        let printed = String::from_utf8_lossy(&c.stdout).trim().to_lowercase();
        assert_eq!(printed, expected.to_string_lossy().to_lowercase());
    }
}

// --- Windows Job Object tree-kill (REQ-S-014) ---

#[cfg(windows)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn windows_job_object_kills_the_whole_tree() {
    // The job (powershell) spawns a detached grandchild (ping) that would run
    // for minutes and records its PID. Cancelling the job must terminate the
    // whole tree via `TerminateJobObject` on the child's Job Object, so the
    // grandchild dies too — something a plain child kill would leave orphaned.
    let (broker, mut rx) = Broker::new();
    let pidfile = std::env::temp_dir().join(format!(
        "cretlink_tree_{}_{}.txt",
        std::process::id(),
        unique_suffix()
    ));
    let _ = std::fs::remove_file(&pidfile);

    let mut spec = JobSpec::new(
        Shell::PowerShell,
        "$p = Start-Process ping -ArgumentList '-n','60','127.0.0.1' -PassThru; \
         Set-Content -LiteralPath $env:CRETLINK_TREE_PIDFILE -Value $p.Id; \
         Start-Sleep -Seconds 60",
    );
    spec.env.insert(
        "CRETLINK_TREE_PIDFILE".to_string(),
        pidfile.display().to_string(),
    );

    let job = broker.spawn(spec).await.unwrap().job;
    let pid = wait_for_pid(&pidfile).await.expect("grandchild PID file");
    assert!(
        pid_running(pid).await,
        "grandchild should be running before cancel"
    );

    broker.cancel(job).unwrap();

    let done = loop {
        let event = timeout(Duration::from_secs(30), rx.recv())
            .await
            .expect("stalled")
            .expect("closed");
        if let BrokerEvent::Done(d) = event
            && d.job == job
        {
            break d;
        }
    };
    assert_eq!(done.outcome, JobOutcome::Cancelled);

    let mut gone = false;
    for _ in 0..50 {
        if !pid_running(pid).await {
            gone = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    let _ = std::fs::remove_file(&pidfile);
    assert!(
        gone,
        "the Job Object must terminate the grandchild process too (REQ-S-014)"
    );
}

// Simulates a broker crash: the `Spawned` is dropped WITHOUT any explicit
// cancel/timeout, so neither a waiter task nor `Broker::drop` runs
// `TerminateJobObject` through the normal path. Dropping the handle must, on its
// own, reap the whole tree — otherwise tokio's `kill_on_drop` would terminate
// only the direct child (powershell), orphaning the detached grandchild (ping).
// This is mechanism 1 in the `platform` module docs: `Spawned`'s `Drop` calls
// `kill_tree`, which runs `TerminateJobObject`. It exercises the graceful
// destructor path; the hard-kill test below covers the case where no destructor
// runs at all. The cancel-path test above would not catch a regression here.
#[cfg(windows)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn windows_dropping_spawned_reaps_the_whole_tree() {
    let pidfile = std::env::temp_dir().join(format!(
        "cretlink_drop_tree_{}_{}.txt",
        std::process::id(),
        unique_suffix()
    ));
    let _ = std::fs::remove_file(&pidfile);

    let mut spec = JobSpec::new(
        Shell::PowerShell,
        "$p = Start-Process ping -ArgumentList '-n','60','127.0.0.1' -PassThru; \
         Set-Content -LiteralPath $env:CRETLINK_TREE_PIDFILE -Value $p.Id; \
         Start-Sleep -Seconds 60",
    );
    spec.env.insert(
        "CRETLINK_TREE_PIDFILE".to_string(),
        pidfile.display().to_string(),
    );

    let spawned = crate::platform::spawn(&spec).expect("spawn");
    let pid = wait_for_pid(&pidfile).await.expect("grandchild PID file");
    assert!(
        pid_running(pid).await,
        "grandchild should be running before drop"
    );

    // Drop WITHOUT cancelling: the drop alone must reap the whole tree — the
    // crash-time fail-closed guarantee (REQ-S-009, REQ-S-014).
    drop(spawned);

    let mut gone = false;
    for _ in 0..50 {
        if !pid_running(pid).await {
            gone = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    let _ = std::fs::remove_file(&pidfile);
    assert!(
        gone,
        "dropping Spawned must reap the whole process tree (REQ-S-009)"
    );
}

// Models a HARD kill of the agent (Task Manager / `taskkill /f`), where a
// `TerminateProcess` runs NO Rust destructor — not `Spawned::drop`, not tokio's
// `kill_on_drop`. The only thing the OS does on the agent's way out is close its
// handles, so the sole reaping mechanism left is the Job Object's
// `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`: closing the last job handle must reap the
// whole tree. `simulate_hard_kill` reproduces exactly that — it forgets the
// child (suppressing `kill_on_drop`) and drops only the job handle — so a passing
// run proves the flag is set and the tree is contained without any destructor
// (ST-M006-007; the automated analogue of manual test MT-3b in
// docs/manual-tests.md).
#[cfg(windows)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn windows_hard_kill_reaps_the_whole_tree_via_kill_on_close() {
    let pidfile = std::env::temp_dir().join(format!(
        "cretlink_hardkill_tree_{}_{}.txt",
        std::process::id(),
        unique_suffix()
    ));
    let _ = std::fs::remove_file(&pidfile);

    let mut spec = JobSpec::new(
        Shell::PowerShell,
        "$p = Start-Process ping -ArgumentList '-n','60','127.0.0.1' -PassThru; \
         Set-Content -LiteralPath $env:CRETLINK_TREE_PIDFILE -Value $p.Id; \
         Start-Sleep -Seconds 60",
    );
    spec.env.insert(
        "CRETLINK_TREE_PIDFILE".to_string(),
        pidfile.display().to_string(),
    );

    let spawned = crate::platform::spawn(&spec).expect("spawn");
    let pid = wait_for_pid(&pidfile).await.expect("grandchild PID file");
    assert!(
        pid_running(pid).await,
        "grandchild should be running before the simulated hard kill"
    );

    // No destructor runs except the OS-equivalent close of the job handle.
    spawned.simulate_hard_kill();

    let mut gone = false;
    for _ in 0..50 {
        if !pid_running(pid).await {
            gone = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    if !gone {
        // The flag failed to reap the tree; do not leave an orphaned grandchild
        // running for a minute on the test host.
        taskkill(pid).await;
    }
    let _ = std::fs::remove_file(&pidfile);
    assert!(
        gone,
        "JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE must reap the whole tree on a hard \
         kill that skips every destructor (ST-M006-007, REQ-S-014, REQ-S-009)"
    );
}

#[cfg(windows)]
fn unique_suffix() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0)
}

#[cfg(windows)]
async fn wait_for_pid(path: &std::path::Path) -> Option<u32> {
    // Generous budget: PowerShell cold-start under ConPTY on a busy CI runner
    // can take well over ten seconds; a hit returns early, so green runs pay
    // nothing for the headroom.
    for _ in 0..300 {
        if let Ok(text) = std::fs::read_to_string(path)
            && let Ok(pid) = text.trim().parse::<u32>()
        {
            return Some(pid);
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    None
}

#[cfg(windows)]
async fn pid_running(pid: u32) -> bool {
    tokio::task::spawn_blocking(move || {
        let output = std::process::Command::new("tasklist")
            .args(["/FI", &format!("PID eq {pid}"), "/NH", "/FO", "CSV"])
            .output();
        match output {
            Ok(out) => {
                let text = String::from_utf8_lossy(&out.stdout);
                text.contains(&format!("\"{pid}\""))
            }
            Err(_) => false,
        }
    })
    .await
    .unwrap_or(false)
}

#[cfg(windows)]
async fn taskkill(pid: u32) {
    let _ = tokio::task::spawn_blocking(move || {
        std::process::Command::new("taskkill")
            .args(["/PID", &pid.to_string(), "/F"])
            .output()
    })
    .await;
}

// --- Interactive (ConPTY) mode (ST-M003-008) ---
//
// The portable tests below run on every OS: `TermSize` bounds and the
// piped/unknown-job resize rejections are platform-independent. The ConPTY jobs
// themselves are Windows-only; off Windows an interactive request is honestly
// refused with `InteractiveUnsupported` rather than degraded to a piped job.

/// `TermSize::new` accepts the inclusive bounds and rejects anything outside
/// them, so an out-of-bounds or malformed size can never reach the
/// pseudo-console (ST-M003-008).
#[test]
fn term_size_enforces_its_bounds() {
    use crate::{MAX_TERM_COLS, MAX_TERM_ROWS, MIN_TERM_COLS, MIN_TERM_ROWS};

    // The corners of the accepted rectangle are valid.
    assert!(TermSize::new(MIN_TERM_COLS, MIN_TERM_ROWS).is_ok());
    assert!(TermSize::new(MAX_TERM_COLS, MAX_TERM_ROWS).is_ok());

    // Zero and oversized dimensions are rejected, and the error reports the
    // offending values without inventing a clamped size.
    for (cols, rows) in [
        (0, 24),
        (80, 0),
        (MAX_TERM_COLS + 1, 24),
        (80, MAX_TERM_ROWS + 1),
    ] {
        match TermSize::new(cols, rows) {
            Err(ExecError::InvalidTermSize { cols: c, rows: r }) => {
                assert_eq!((c, r), (cols, rows));
            }
            other => panic!("expected InvalidTermSize for {cols}x{rows}, got {other:?}"),
        }
    }
}

/// Resizing a piped job is a category error: it has no terminal, so the broker
/// reports `NotInteractive` rather than silently ignoring the request.
#[tokio::test]
async fn resize_of_piped_job_is_not_interactive() {
    let (broker, mut rx) = Broker::new();
    let job = broker.spawn(long_running()).await.unwrap().job;
    tokio::time::sleep(Duration::from_millis(100)).await;

    let err = broker.resize(job, term_size()).await;
    assert!(
        matches!(err, Err(ExecError::NotInteractive)),
        "resizing a piped job should be NotInteractive, got {err:?}"
    );

    broker.cancel(job).unwrap();
    let c = drain_job(&mut rx, job).await;
    assert_eq!(c.done.outcome, JobOutcome::Cancelled);
}

/// Resizing a job the broker does not track is `UnknownJob`.
#[tokio::test]
async fn resize_of_unknown_job_is_reported() {
    let (broker, _rx) = Broker::new();
    let err = broker.resize(JobId(9999), term_size()).await;
    assert!(matches!(err, Err(ExecError::UnknownJob)), "got {err:?}");
}

/// Off Windows an interactive job is refused, not degraded to a piped one.
#[cfg(not(windows))]
#[tokio::test]
async fn interactive_is_unsupported_off_windows() {
    let (broker, _rx) = Broker::new();
    let spec = JobSpec::new(Shell::Cmd, "echo hi").interactive(term_size());
    let err = broker.spawn(spec).await;
    assert!(
        matches!(err, Err(ExecError::InteractiveUnsupported)),
        "got {err:?}"
    );
}

/// An interactive program runs under a real pseudo-console, its merged output is
/// delivered as `Stdout`, and a clean exit is reported `Completed` (REQ-F-008).
#[cfg(windows)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn interactive_program_runs_under_conpty_and_completes() {
    let (broker, mut rx) = Broker::new();
    // ConPTY renders the screen asynchronously, so a program that prints and
    // exits in the same instant can be torn down before its output frame is
    // emitted. A brief pause after the echo gives the render pass a window —
    // exactly how a real interactive program (which does not exit instantly)
    // behaves. The exit code still comes from the process, not the render stream.
    let spec =
        JobSpec::new(Shell::Cmd, "echo hello & ping -n 2 127.0.0.1 > nul").interactive(term_size());
    let job = broker.spawn(spec).await.unwrap().job;
    let c = drain_job(&mut rx, job).await;

    assert_eq!(c.done.outcome, JobOutcome::Completed);
    assert_eq!(c.done.exit_code, Some(0));
    assert!(
        String::from_utf8_lossy(&c.stdout).contains("hello"),
        "merged ConPTY output should contain the program text, was {:?}",
        String::from_utf8_lossy(&c.stdout)
    );
    // ConPTY merges both streams into the single VT stream delivered as Stdout.
    assert!(c.stderr.is_empty(), "ConPTY has no separate stderr");
}

/// A failing interactive program reports its real non-zero exit code, never a
/// fabricated success (REQ-N-008).
#[cfg(windows)]
#[tokio::test]
async fn interactive_failure_reports_the_real_exit_code() {
    let (broker, mut rx) = Broker::new();
    let spec = JobSpec::new(Shell::Cmd, "exit 5").interactive(term_size());
    let job = broker.spawn(spec).await.unwrap().job;
    let c = drain_job(&mut rx, job).await;

    assert_eq!(c.done.outcome, JobOutcome::Failed);
    assert_eq!(c.done.exit_code, Some(5));
}

/// A resize within bounds is accepted on a running interactive job.
#[cfg(windows)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn interactive_resize_within_bounds_is_accepted() {
    let (broker, mut rx) = Broker::new();
    let spec = JobSpec::new(Shell::Cmd, "ping -n 30 127.0.0.1 > nul").interactive(term_size());
    let job = broker.spawn(spec).await.unwrap().job;
    tokio::time::sleep(Duration::from_millis(200)).await;

    broker
        .resize(job, TermSize::new(120, 40).unwrap())
        .await
        .expect("resize within bounds should be accepted");

    broker.cancel(job).unwrap();
    let c = drain_job(&mut rx, job).await;
    assert_eq!(c.done.outcome, JobOutcome::Cancelled);
}

/// Interactive output is delivered incrementally in multiple bounded chunks,
/// exercising the reader→forward backpressure path (REQ-N-005, REQ-N-006).
#[cfg(windows)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn interactive_high_output_is_chunked() {
    let (broker, mut rx) = Broker::new();
    // Emit well over one read buffer's worth of text so it must span chunks.
    let spec = JobSpec::new(
        Shell::Cmd,
        "for /L %i in (1,1,20000) do @echo CRETLINK-LINE-%i",
    )
    .interactive(term_size());
    let job = broker.spawn(spec).await.unwrap().job;
    let c = drain_job(&mut rx, job).await;

    assert_eq!(c.done.outcome, JobOutcome::Completed);
    assert!(!c.stdout.is_empty(), "expected merged output");
    assert!(
        c.chunks >= 2,
        "high interactive output should arrive in multiple bounded chunks, got {}",
        c.chunks
    );
}

/// Revoking every job (`cancel_all`, REQ-S-011/S-004) terminates an interactive
/// job and it reports `Cancelled`.
#[cfg(windows)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn interactive_revocation_cancels_the_job() {
    let (broker, mut rx) = Broker::new();
    let spec = JobSpec::new(Shell::Cmd, "ping -n 30 127.0.0.1 > nul").interactive(term_size());
    let job = broker.spawn(spec).await.unwrap().job;
    tokio::time::sleep(Duration::from_millis(200)).await;

    broker.cancel_all();

    let c = drain_job(&mut rx, job).await;
    assert_eq!(c.done.outcome, JobOutcome::Cancelled);
}

/// Cancelling an interactive job terminates its whole process tree via the same
/// tracked Job Object as a piped job — a detached grandchild dies too
/// (REQ-S-014). This is the interactive twin of
/// `windows_job_object_kills_the_whole_tree`.
#[cfg(windows)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn interactive_cancel_kills_the_whole_tree() {
    let (broker, mut rx) = Broker::new();
    let pidfile = std::env::temp_dir().join(format!(
        "cretlink_conpty_tree_{}_{}.txt",
        std::process::id(),
        unique_suffix()
    ));
    let _ = std::fs::remove_file(&pidfile);

    let mut spec = JobSpec::new(
        Shell::PowerShell,
        "$p = Start-Process ping -ArgumentList '-n','60','127.0.0.1' -PassThru; \
         Set-Content -LiteralPath $env:CRETLINK_TREE_PIDFILE -Value $p.Id; \
         Start-Sleep -Seconds 60",
    )
    .interactive(term_size());
    spec.env.insert(
        "CRETLINK_TREE_PIDFILE".to_string(),
        pidfile.display().to_string(),
    );

    let job = broker.spawn(spec).await.unwrap().job;
    let pid = wait_for_pid(&pidfile).await.expect("grandchild PID file");
    assert!(
        pid_running(pid).await,
        "grandchild should be running before cancel"
    );

    broker.cancel(job).unwrap();

    let c = drain_job(&mut rx, job).await;
    assert_eq!(c.done.outcome, JobOutcome::Cancelled);

    let mut gone = false;
    for _ in 0..50 {
        if !pid_running(pid).await {
            gone = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    let _ = std::fs::remove_file(&pidfile);
    assert!(
        gone,
        "an interactive job's Job Object must terminate the grandchild too (REQ-S-014)"
    );
}

/// A crash/disconnect — the console child killed by something other than the
/// broker — is never reported as a clean exit (REQ-N-008). The job writes its
/// own PID, an external `taskkill /F` ends it, and the outcome must not be
/// `Completed` nor carry exit code 0.
#[cfg(windows)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn interactive_external_kill_is_never_a_clean_exit() {
    let (broker, mut rx) = Broker::new();
    let pidfile = std::env::temp_dir().join(format!(
        "cretlink_conpty_crash_{}_{}.txt",
        std::process::id(),
        unique_suffix()
    ));
    let _ = std::fs::remove_file(&pidfile);

    // `$PID` is this PowerShell process — the ConPTY child the exit waiter holds.
    let mut spec = JobSpec::new(
        Shell::PowerShell,
        "Set-Content -LiteralPath $env:CRETLINK_CRASH_PIDFILE -Value $PID; \
         Start-Sleep -Seconds 60",
    )
    .interactive(term_size());
    spec.env.insert(
        "CRETLINK_CRASH_PIDFILE".to_string(),
        pidfile.display().to_string(),
    );

    let job = broker.spawn(spec).await.unwrap().job;
    let pid = wait_for_pid(&pidfile).await.expect("child PID file");
    assert!(
        pid_running(pid).await,
        "child should be running before kill"
    );

    // Kill the console child out from under the broker.
    taskkill(pid).await;

    let c = drain_job(&mut rx, job).await;
    let _ = std::fs::remove_file(&pidfile);
    assert_ne!(
        c.done.outcome,
        JobOutcome::Completed,
        "an externally killed console child must never be reported Completed"
    );
    assert_ne!(
        c.done.exit_code,
        Some(0),
        "an externally killed console child must never carry a success exit code"
    );
}

// --- A Command Prompt command reaches cmd.exe as written (ST-M003-001) ---
//
// `cmd.exe` does not read the C runtime's quoting, so both backends append the
// command to its command line raw, as `/S /C "<command>"`. The exact tail is
// pinned on every platform in the `platform` tests; these run the real shell
// in both backends. Each checked text is one the old quoting could not produce,
// and none holds a space: a pseudo-console may draw a space as a cursor
// movement, which would break a plain text comparison.

/// `echo` prints its quoted argument with the quotes, and the `&` inside them
/// is text. Quoted by the C runtime rules the command reached `cmd.exe` with
/// `\"` for each quote, and `echo` printed those backslashes too.
#[cfg(windows)]
const CMD_QUOTED_ARGUMENT: (&str, &str) = (r#"echo "a&b""#, r#""a&b""#);

/// A program started by the command receives its quoted argument, spaces and
/// all, as one argument. The printed text occurs nowhere in the command text,
/// so only the program's own output can hold it: with the old quoting
/// PowerShell received a quoted literal and printed the command text instead,
/// and a pseudo-console stream also carries the window title, where `cmd.exe`
/// may name the program it is running.
#[cfg(windows)]
const CMD_NESTED_PROGRAM: (&str, &str) = (
    r#"powershell -NoProfile -NonInteractive -Command "Write-Output ('x' + '-' + 'y')""#,
    "x-y",
);

/// A quoted path with a space in it (`%ProgramFiles%` is `C:\Program Files`) is
/// one path. With the old quoting the path began with a backslash and did not
/// exist, so nothing was printed.
#[cfg(windows)]
const CMD_QUOTED_PATH: (&str, &str) = (
    r#"if exist "%ProgramFiles%" (echo quoted-path-found)"#,
    "quoted-path-found",
);

/// Runs `command` in the Command Prompt to its end and returns what it
/// printed: piped, or under a pseudo-console when `interactive`.
///
/// A pseudo-console renders the screen asynchronously, so a short pause is
/// appended there to give the render pass a window before the job ends (see
/// `interactive_program_runs_under_conpty_and_completes`).
#[cfg(windows)]
async fn cmd_prints(command: &str, interactive: bool) -> String {
    let spec = if interactive {
        JobSpec::new(Shell::Cmd, format!("{command} & ping -n 2 127.0.0.1 > nul"))
            .interactive(term_size())
    } else {
        JobSpec::new(Shell::Cmd, command)
    };
    let (broker, mut rx) = Broker::new();
    let job = broker.spawn(spec).await.unwrap().job;
    let c = drain_job(&mut rx, job).await;
    assert_eq!(
        c.done.outcome,
        JobOutcome::Completed,
        "`{command}` should run to a clean end, stderr was {:?}",
        String::from_utf8_lossy(&c.stderr)
    );
    String::from_utf8_lossy(&c.stdout).into_owned()
}

/// Asserts that `command` prints `expected` in the given backend.
#[cfg(windows)]
async fn assert_cmd_prints((command, expected): (&str, &str), interactive: bool) {
    let printed = cmd_prints(command, interactive).await;
    assert!(
        printed.contains(expected),
        "`{command}` should print {expected:?}, printed {printed:?}"
    );
}

#[cfg(windows)]
#[tokio::test]
async fn piped_cmd_keeps_a_quoted_argument() {
    assert_cmd_prints(CMD_QUOTED_ARGUMENT, false).await;
}

#[cfg(windows)]
#[tokio::test]
async fn piped_cmd_passes_a_quoted_argument_to_a_nested_program() {
    assert_cmd_prints(CMD_NESTED_PROGRAM, false).await;
}

#[cfg(windows)]
#[tokio::test]
async fn piped_cmd_keeps_a_quoted_path_with_a_space() {
    assert_cmd_prints(CMD_QUOTED_PATH, false).await;
}

#[cfg(windows)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn interactive_cmd_keeps_a_quoted_argument() {
    assert_cmd_prints(CMD_QUOTED_ARGUMENT, true).await;
}

#[cfg(windows)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn interactive_cmd_passes_a_quoted_argument_to_a_nested_program() {
    assert_cmd_prints(CMD_NESTED_PROGRAM, true).await;
}

#[cfg(windows)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn interactive_cmd_keeps_a_quoted_path_with_a_space() {
    assert_cmd_prints(CMD_QUOTED_PATH, true).await;
}
