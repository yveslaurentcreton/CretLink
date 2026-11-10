//! Platform adapter that spawns a shell command with separated, piped stdio and
//! process-tree containment (ADR-0006).
//!
//! One [`Spawned`] surface over two backends keeps the rest of the broker
//! platform-agnostic, so process-tree termination works the same way everywhere:
//!
//! - **Windows (real targets):** a `tokio::process::Command` created *suspended*,
//!   assigned to CretLink's own kill-on-close Job Object before it can run, then
//!   resumed (see [`win::spawn`]). Explicit cancel, timeout, or session revoke
//!   terminates the **whole process tree** through `TerminateJobObject`, not just
//!   the direct child (REQ-S-014) — see [`Spawned::kill_tree`].
//! - **Everywhere else (tests, simulated hosts):** the same command wrapped by
//!   [`process_wrap`]'s `ProcessGroup`, which puts the child in its own process
//!   group; `kill_tree` then signals the **whole group**, so when the shell forks
//!   its command (rather than `exec`ing it) the children are still reaped. Real
//!   customer targets are Windows; this path exists so the broker's tests build
//!   and run on macOS and Linux, where a direct-child-only kill would orphan
//!   `sh`'s children.
//!
//! Both backends pipe stdin, stdout, and stderr separately (REQ-F-007) and set
//! `kill_on_drop` on the command so the direct child handle is reaped too.
//!
//! # Fail-closed process-tree reaping (REQ-S-009, REQ-S-014)
//!
//! A hard kill of the agent must not orphan an elevated tree. Two independent
//! mechanisms guarantee that on Windows, matching the interactive ConPTY path
//! (see [`crate::conpty`]):
//!
//! 1. **Graceful / drop-time.** When a [`Spawned`] is dropped without an explicit
//!    kill — the broker's waiter task is aborted, panics, or the future is
//!    cancelled — [`Spawned`]'s `Drop` calls [`Spawned::kill_tree`], which
//!    reaps the tree deterministically (`TerminateJobObject` on Windows, a
//!    process-group kill elsewhere). A detached grandchild started via
//!    `Start-Process` is therefore never orphaned.
//! 2. **Hard kill.** A `TerminateProcess` of the agent (Task Manager, `taskkill
//!    /f`) runs **no** Rust destructor, so mechanism 1 never fires. On Windows the
//!    Job Object created here carries `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`, so the
//!    kernel reaps every process in the job the moment the agent's last handle to
//!    it closes — which the OS does as the agent process is torn down
//!    (ST-M006-007). process-wrap 9.1.0 cannot set that flag (its
//!    `CommandWrap::spawn` `mem::take`s the wrapper set before `JobObject`'s hook
//!    runs, so `has_wrap::<KillOnDrop>()` is always false), which is why the
//!    Windows path manages its own Job Object instead of going through
//!    process-wrap.
//!
//! The non-Windows `ProcessGroup` path has no kill-on-close analogue, so it relies
//! on mechanism 1 alone; that is acceptable because customer targets are Windows
//! and the group path is test/simulation scaffolding only.

use std::process::ExitStatus;

use process_wrap::tokio::ChildWrapper;
use tokio::process::{ChildStderr, ChildStdin, ChildStdout};

use crate::JobSpec;
#[cfg(not(windows))]
use crate::Shell;
use crate::error::ExecError;

/// A spawned child with its stdio handles, behind a platform-neutral surface.
///
/// The stdio accessors hand out the tokio handles exactly once; `wait` and
/// `kill_tree` drive completion and termination. The child is always a tokio
/// [`ChildWrapper`] so `wait`/stdio go through one code path; tree termination
/// goes through the Job Object on Windows and the process group elsewhere.
#[derive(Debug)]
pub(crate) struct Spawned {
    child: Box<dyn ChildWrapper>,
    /// The kill-on-close Job Object containing this job's whole tree. Its `Drop`
    /// (a `CloseHandle`) is the fail-closed net for a hard kill that skips
    /// [`Spawned::kill_tree`] (ST-M006-007).
    #[cfg(windows)]
    job: win::Job,
}

impl Spawned {
    /// Takes the child's standard input handle, if not already taken.
    pub(crate) fn take_stdin(&mut self) -> Option<ChildStdin> {
        self.child.stdin().take()
    }

    /// Takes the child's standard output handle, if not already taken.
    pub(crate) fn take_stdout(&mut self) -> Option<ChildStdout> {
        self.child.stdout().take()
    }

    /// Takes the child's standard error handle, if not already taken.
    pub(crate) fn take_stderr(&mut self) -> Option<ChildStderr> {
        self.child.stderr().take()
    }

    /// Awaits the child's exit. Cancel-safe: a dropped `wait` future never loses
    /// the exit status, so the waiter can race it against a timeout or cancel
    /// signal and call `wait` again after killing.
    pub(crate) async fn wait(&mut self) -> std::io::Result<ExitStatus> {
        self.child.wait().await
    }

    /// Terminates the job's whole process tree — `TerminateJobObject` on the
    /// child's Job Object on Windows, a process-group signal elsewhere — reaping
    /// grandchildren, not just the direct child (REQ-S-014). The caller should
    /// `wait` afterwards to reap the child.
    ///
    /// Idempotent: after a natural exit or an earlier call the job or group is
    /// already empty and this is a harmless no-op.
    pub(crate) fn kill_tree(&mut self) -> std::io::Result<()> {
        #[cfg(windows)]
        {
            // Reap the tree through the Job Object rather than the direct child,
            // so grandchildren die too.
            self.job.terminate();
            Ok(())
        }
        #[cfg(not(windows))]
        {
            self.child.start_kill()
        }
    }
}

/// Drop-time fail-closed reaping (REQ-S-009, REQ-S-014).
///
/// If a [`Spawned`] is dropped without an explicit kill — the broker's waiter
/// task is aborted, panics, or the future is otherwise cancelled — the child's
/// **whole process tree** is still terminated (via `TerminateJobObject` on
/// Windows, or a process-group kill elsewhere), so a detached grandchild can
/// never be orphaned. This is mechanism 1 in the module docs; the Windows Job
/// Object's own `Drop` (`CloseHandle` on a kill-on-close job) is the mechanism-2
/// net that also holds when a hard kill skips this destructor entirely.
///
/// The kill is idempotent: after a normal exit or an earlier `kill_tree` the job
/// or group is already empty and the call is a harmless no-op, so its error is
/// discarded.
impl Drop for Spawned {
    fn drop(&mut self) {
        let _ = self.kill_tree();
    }
}

/// Builds the argument vector for a shell invocation off Windows, where both
/// logical shells route through `sh -c` (documented fallback for tests and
/// simulated hosts). Windows starts the two real shells itself (see `win`).
#[cfg(not(windows))]
fn shell_argv(shell: Shell, command: &str) -> (&'static str, Vec<String>) {
    // `shell` is recorded for parity with the wire schema but does not change
    // the portable fallback.
    let _ = shell;
    ("sh", vec!["-c".to_string(), command.to_string()])
}

/// The tail of a `cmd.exe` command line that runs `command`: `/S /C "<command>"`
/// with the command exactly as written (ST-M003-001).
///
/// `cmd.exe` does not read the C runtime's quoting. Handed the command as one
/// argument quoted by those rules, it strips the outer quotes and passes every
/// escaped `\"` on, so a program inside the command receives a quoted literal
/// instead of its argument. With `/S` it strips exactly the first and the last
/// quote of what follows `/C` and runs the rest verbatim. The command therefore
/// needs no escaping at all: a quote, a backslash before a quote, `&`, `|`, `^`
/// and `%` reach `cmd.exe` as the caller wrote them.
///
/// Both Windows backends append this tail to the command line **raw**, never as
/// a quoted argument: the piped backend through `raw_arg`, the pseudo-console
/// backend in the command line it builds itself.
#[cfg_attr(not(windows), allow(dead_code))] // only the Windows backends call it outside tests
pub(crate) fn cmd_raw_tail(command: &str) -> String {
    format!("/S /C \"{command}\"")
}

/// Spawns `spec` with separated piped stdio and process-tree containment.
pub(crate) fn spawn(spec: &JobSpec) -> Result<Spawned, ExecError> {
    #[cfg(windows)]
    {
        // Own kill-on-close Job Object, assigned while the child is suspended so
        // the whole tree is contained before it can run (ST-M006-007).
        let (child, job) = win::spawn(spec)?;
        Ok(Spawned {
            child: Box::new(child),
            job,
        })
    }
    #[cfg(not(windows))]
    {
        use std::process::Stdio;

        use process_wrap::tokio::*;

        let (program, args) = shell_argv(spec.shell, &spec.command);
        let working_dir = spec.working_dir.clone();
        let env = spec.env.clone();

        let mut wrap = CommandWrap::with_new(program, |command| {
            command
                .args(&args)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .kill_on_drop(true);
            if let Some(dir) = &working_dir {
                command.current_dir(dir);
            }
            for (key, value) in &env {
                command.env(key, value);
            }
        });

        // Put the child in its own process group so `kill_tree` signals the whole
        // group: `sh -c` may fork its command rather than `exec` it, and a
        // direct-child-only kill would then orphan the grandchild.
        wrap.wrap(ProcessGroup::leader());

        let child = wrap.spawn().map_err(ExecError::Spawn)?;
        Ok(Spawned { child })
    }
}

/// Windows piped-mode containment: a kill-on-close Job Object owned by CretLink,
/// assigned to the child while it is suspended, then resumed. This replaces
/// process-wrap's `JobObject` on Windows because process-wrap 9.1.0 cannot enable
/// `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` (see the module docs), which is the flag a
/// hard `TerminateProcess` of the agent relies on to reap the tree (ST-M006-007).
#[cfg(windows)]
mod win {
    #![allow(
        unsafe_code,
        reason = "Localized Windows Job Object / Toolhelp FFI for piped-mode \
                  kill-on-close containment (ST-M006-007); ADR-0001 and ADR-0006 \
                  permit reviewed, localized unsafe in one platform adapter."
    )]

    use std::ffi::c_void;
    use std::process::Stdio;

    use tokio::process::{Child, Command};
    use windows::Win32::Foundation::{CloseHandle, HANDLE};
    use windows::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, TH32CS_SNAPTHREAD, THREADENTRY32, Thread32First, Thread32Next,
    };
    use windows::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
        SetInformationJobObject, TerminateJobObject,
    };
    use windows::Win32::System::Threading::{
        CREATE_SUSPENDED, GetProcessId, OpenThread, ResumeThread, THREAD_SUSPEND_RESUME,
    };
    use windows::core::PCWSTR;

    use super::cmd_raw_tail;
    use crate::error::ExecError;
    use crate::{JobSpec, Shell};

    /// A kill-on-close Job Object that contains one piped job's whole process tree.
    ///
    /// Sole owner of the handle. `terminate` reaps the tree explicitly; on drop the
    /// handle is closed, and because the job carries
    /// `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` that close reaps every process still in
    /// the tree even when the drop is the *only* thing that runs — the hard-kill
    /// case that skips [`super::Spawned::kill_tree`] (ST-M006-007). [`HANDLE`] wraps
    /// a raw pointer and is not `Send`; this handle has exactly one owner
    /// ([`super::Spawned`]) and is never aliased, so moving it across threads once
    /// (the broker moves the job into its waiter task) is sound.
    #[derive(Debug)]
    pub(crate) struct Job(HANDLE);
    // SAFETY: single-owner move with no aliasing; see the type docs.
    unsafe impl Send for Job {}

    impl Job {
        /// Terminates the whole job tree (`TerminateJobObject`). Idempotent: after
        /// a natural exit the job is already empty and this is a harmless no-op.
        pub(crate) fn terminate(&self) {
            // SAFETY: `self.0` is a live Job Object handle owned solely by `self`.
            let _ = unsafe { TerminateJobObject(self.0, 1) };
        }
    }

    impl Drop for Job {
        fn drop(&mut self) {
            // Closing a kill-on-close job reaps any process still in the tree — the
            // fail-closed net for a hard kill that skips `Spawned::kill_tree`.
            // SAFETY: `self.0` is a live handle owned solely by `self`, closed once.
            let _ = unsafe { CloseHandle(self.0) };
        }
    }

    /// The shell process that runs `command`, before its stdio and containment
    /// are set up.
    ///
    /// PowerShell takes the command as one argument, quoted by the C runtime
    /// rules that `powershell.exe` reads. `cmd.exe` does not read those rules,
    /// so a Command Prompt command is appended to the command line raw, as
    /// `/S /C "<command>"`, and reaches `cmd.exe` exactly as written
    /// (ST-M003-001; see [`cmd_raw_tail`]).
    fn shell_command(shell: Shell, command: &str) -> Command {
        match shell {
            Shell::PowerShell => {
                let mut process = Command::new("powershell");
                process.args(["-NoProfile", "-NonInteractive", "-Command", command]);
                process
            }
            Shell::Cmd => {
                let mut process = Command::new("cmd");
                process.raw_arg(cmd_raw_tail(command));
                process
            }
        }
    }

    /// Spawns `spec` as a piped-stdio child contained in a kill-on-close Job
    /// Object, returning the tokio [`Child`] (for stdio and `wait`) and the [`Job`].
    ///
    /// The child is created **suspended** so it is assigned to the job before it
    /// can run: there is no window in which it could spawn a grandchild that
    /// escapes the job (REQ-S-014), matching the ConPTY path. Once contained, its
    /// initial thread is resumed.
    pub(crate) fn spawn(spec: &JobSpec) -> Result<(Child, Job), ExecError> {
        let mut command = shell_command(spec.shell, &spec.command);
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .creation_flags(CREATE_SUSPENDED.0);
        if let Some(dir) = &spec.working_dir {
            command.current_dir(dir);
        }
        for (key, value) in &spec.env {
            command.env(key, value);
        }

        let child = command.spawn().map_err(ExecError::Spawn)?;

        // Contain before the child runs. On any failure `child` is dropped here
        // and its `kill_on_drop` terminates the still-suspended direct child.
        let job = contain_suspended(&child)?;
        Ok((child, job))
    }

    /// Creates the kill-on-close job, assigns the suspended `child` to it, then
    /// resumes it. On any failure the job is terminated and closed so nothing is
    /// left contained-but-unstartable.
    fn contain_suspended(child: &Child) -> Result<Job, ExecError> {
        let raw = child.raw_handle().ok_or_else(|| {
            ExecError::Spawn(std::io::Error::other("child exited before containment"))
        })?;
        let process = HANDLE(raw);

        let job = create_job()?;

        // SAFETY: `job` and `process` are live handles; assigning the suspended
        // child to the job before it runs contains its whole future tree.
        if let Err(e) = unsafe { AssignProcessToJobObject(job, process) } {
            // SAFETY: `job` is live and owned here; close it once on the error path.
            let _ = unsafe { CloseHandle(job) };
            return Err(win_spawn(e));
        }

        if let Err(e) = resume_process(process) {
            // Contained but unstartable: reap deterministically, then close.
            // SAFETY: `job` is a live kill-on-close job owned here.
            let _ = unsafe { TerminateJobObject(job, 1) };
            let _ = unsafe { CloseHandle(job) };
            return Err(e);
        }

        Ok(Job(job))
    }

    /// Creates a Job Object limited to kill-on-close (fail closed), no breakaway —
    /// the same shape as the ConPTY job (see [`crate::conpty`]).
    fn create_job() -> Result<HANDLE, ExecError> {
        // SAFETY: null attributes and name request a default, unnamed job.
        let job = unsafe { CreateJobObjectW(None, PCWSTR::null()) }.map_err(win_spawn)?;

        let mut info = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        let len = u32::try_from(size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>()).unwrap_or(0);
        // SAFETY: `job` is live; `info` is a correctly sized, initialized input for
        // the extended-limit class.
        let set = unsafe {
            SetInformationJobObject(
                job,
                JobObjectExtendedLimitInformation,
                std::ptr::from_ref(&info).cast::<c_void>(),
                len,
            )
        };
        if let Err(e) = set {
            // SAFETY: `job` is live and owned here; close it once on the error path.
            let _ = unsafe { CloseHandle(job) };
            return Err(win_spawn(e));
        }
        Ok(job)
    }

    /// Resumes every thread of the suspended child (a `CREATE_SUSPENDED` process
    /// has exactly its initial thread). We only hold the process handle, not the
    /// thread handle, so we enumerate threads via a Toolhelp snapshot and resume
    /// those owned by the child's PID.
    fn resume_process(process: HANDLE) -> Result<(), ExecError> {
        // SAFETY: `process` is a live handle; `GetProcessId` reads its PID. The
        // process is suspended and held by our handle, so the PID is stable.
        let pid = unsafe { GetProcessId(process) };
        if pid == 0 {
            return Err(ExecError::Spawn(std::io::Error::other(
                "could not read the PID of the contained child",
            )));
        }

        // SAFETY: requests a thread-only snapshot; returns an owned snapshot handle.
        let snapshot =
            unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0) }.map_err(win_spawn)?;

        let mut entry = THREADENTRY32 {
            dwSize: u32::try_from(size_of::<THREADENTRY32>()).unwrap_or(0),
            ..Default::default()
        };
        let mut resumed_any = false;
        // SAFETY: `snapshot` is live; `entry` is a correctly sized, writable buffer
        // with `dwSize` set as `Thread32First` requires.
        let mut ok = unsafe { Thread32First(snapshot, &mut entry) }.is_ok();
        while ok {
            if entry.th32OwnerProcessID == pid {
                resume_thread(entry.th32ThreadID, &mut resumed_any);
            }
            // SAFETY: same invariants as `Thread32First`.
            ok = unsafe { Thread32Next(snapshot, &mut entry) }.is_ok();
        }
        // SAFETY: `snapshot` is a live handle owned here, closed exactly once.
        let _ = unsafe { CloseHandle(snapshot) };

        if resumed_any {
            Ok(())
        } else {
            Err(ExecError::Spawn(std::io::Error::other(
                "no suspended thread found to resume the contained child",
            )))
        }
    }

    /// Opens one thread by ID and resumes it, recording success. Threads that
    /// cannot be opened (e.g. one that exited between snapshot and open) are
    /// skipped rather than failing the whole resume.
    fn resume_thread(thread_id: u32, resumed_any: &mut bool) {
        // SAFETY: opens the thread for suspend/resume by ID; returns an owned
        // handle or an error we skip.
        let Ok(handle) = (unsafe { OpenThread(THREAD_SUSPEND_RESUME, false, thread_id) }) else {
            return;
        };
        // SAFETY: `handle` is a live thread handle; `ResumeThread` decrements its
        // suspend count and returns the previous count, or `u32::MAX` on failure.
        let previous = unsafe { ResumeThread(handle) };
        if previous != u32::MAX {
            *resumed_any = true;
        }
        // SAFETY: `handle` is owned here, closed exactly once.
        let _ = unsafe { CloseHandle(handle) };
    }

    /// Maps a Windows error to a spawn failure (mirrors [`crate::conpty`]).
    fn win_spawn(e: windows::core::Error) -> ExecError {
        ExecError::Spawn(std::io::Error::from_raw_os_error(e.code().0))
    }

    /// Test-only: models a hard `TerminateProcess` of the agent, where **no** Rust
    /// destructor runs except the OS-equivalent close of the Job Object handle.
    ///
    /// It moves both fields out of `spawned` without running [`super::Spawned`]'s
    /// `Drop` (which would call `kill_tree`), then forgets the child to suppress
    /// its `kill_on_drop`, and finally drops **only** the [`Job`]. That close is
    /// the sole reaping mechanism a hard kill leaves, so a passing containment
    /// test proves `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` alone reaps the tree
    /// (ST-M006-007; the automated analogue of manual test MT-3b in
    /// docs/manual-tests.md).
    #[cfg(test)]
    pub(crate) fn simulate_hard_kill(spawned: super::Spawned) {
        // `ManuallyDrop` suppresses `Spawned::drop`; we then read each field out
        // exactly once. Because the container is never dropped, no destructor runs
        // twice.
        let spawned = std::mem::ManuallyDrop::new(spawned);
        // SAFETY: `spawned` is a `ManuallyDrop` we never access again after these
        // reads, so moving `child` and `job` out by value gives each a single
        // owner with no double drop.
        let child = unsafe { std::ptr::read(&spawned.child) };
        let job = unsafe { std::ptr::read(&spawned.job) };
        // A hard kill runs no destructor: forgetting the child suppresses its
        // `kill_on_drop`, leaving the kill-on-close job's `Drop` as the only
        // reaper — exactly what the OS provides when it closes the handle.
        std::mem::forget(child);
        drop(job);
    }
}

/// Test-only bridge to [`win::simulate_hard_kill`] for the containment test.
#[cfg(all(test, windows))]
impl Spawned {
    /// Models a hard `TerminateProcess` of the agent (ST-M006-007; manual test
    /// MT-3b in docs/manual-tests.md): no Rust destructor runs except the
    /// OS-equivalent close of the Job Object handle.
    pub(crate) fn simulate_hard_kill(self) {
        win::simulate_hard_kill(self);
    }
}

#[cfg(test)]
mod tests {
    use super::cmd_raw_tail;

    /// A Command Prompt command is wrapped in one pair of quotes behind
    /// `/S /C` and is otherwise passed exactly as written, so `cmd.exe`, which
    /// strips that one pair, runs what the caller wrote (ST-M003-001).
    #[test]
    fn a_cmd_command_is_passed_as_written() {
        for (command, tail) in [
            // A plain command.
            ("echo hello", r#"/S /C "echo hello""#),
            // A quoted path with a space: no quote is escaped.
            (
                r#"dir "C:\Program Files""#,
                r#"/S /C "dir "C:\Program Files"""#,
            ),
            // A program inside the command with a quoted argument of its own.
            (
                r#"powershell -Command "Write-Output 1""#,
                r#"/S /C "powershell -Command "Write-Output 1"""#,
            ),
            // A path that ends in a backslash before the quote: no backslash is
            // doubled.
            (
                r#"dir "C:\Program Files\""#,
                r#"/S /C "dir "C:\Program Files\"""#,
            ),
            // The characters cmd.exe gives a meaning to stay for cmd.exe.
            (
                "echo a & echo b | findstr b ^& echo %PATH%",
                r#"/S /C "echo a & echo b | findstr b ^& echo %PATH%""#,
            ),
        ] {
            assert_eq!(cmd_raw_tail(command), tail, "{command}");
        }
    }
}
