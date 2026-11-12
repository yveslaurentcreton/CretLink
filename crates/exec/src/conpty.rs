//! Windows pseudo-console (ConPTY) execution backend — ADR-0006 **Mode B**,
//! ST-M003-008.
//!
//! This is the interactive half of the dual-mode broker: a job runs under a real
//! Windows pseudo-console so programs that probe for a console behave correctly,
//! and the console's **merged** output (ConPTY does not separate standard error)
//! is streamed as one VT byte stream. It is the only `unsafe` code in the crate,
//! deliberately localized to one Windows adapter as
//! `ADR-0001` and
//! `ADR-0006`
//! prescribe; the workspace otherwise denies `unsafe_code`.
//!
//! # Shape
//!
//! [`spawn`] builds, in order:
//!
//! 1. two anonymous pipes (child stdin, child stdout);
//! 2. a pseudo-console over them ([`CreatePseudoConsole`]);
//! 3. a Job Object created with `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` and
//!    **without** `JOB_OBJECT_LIMIT_BREAKAWAY_OK`;
//! 4. the child via [`CreateProcessW`] with a `STARTUPINFOEXW` attribute list
//!    carrying **both** `PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE` and
//!    `PROC_THREAD_ATTRIBUTE_JOB_LIST`, so the child enters the pseudo-console
//!    and the Job Object atomically at creation — no window in which a child
//!    could spawn grandchildren before it is contained (REQ-S-014).
//!
//! Blocking reader and writer OS threads bridge the pipes to tokio through
//! bounded channels (REQ-N-005, REQ-N-006): the reader blocks on a full output
//! channel, which fills the OS pipe and blocks the child — genuine backpressure.
//!
//! # Containment and honesty
//!
//! Cancellation, timeout, and revocation call [`TerminateJobObject`], reaping the
//! whole tree; `KILL_ON_JOB_CLOSE` reaps it on drop even without cooperation
//! (fail closed, REQ-S-009). The child's real exit code is reported truthfully; a
//! kill or an exit code the adapter cannot read is surfaced as
//! [`crate::JobOutcome::Unknown`] by the broker, never a fabricated clean exit
//! (REQ-N-008).
//!
//! # Shutdown ordering (pre-24H2 `ClosePseudoConsole` deadlock)
//!
//! [`ConptyControl::shutdown`] terminates the job, then calls
//! [`ClosePseudoConsole`] **while the reader thread is still draining** the output
//! pipe on another thread, then joins the reader. `ClosePseudoConsole` can block
//! until pending output is drained; draining concurrently from a *different*
//! thread avoids the
//! [documented deadlock](https://learn.microsoft.com/en-us/windows/console/closepseudoconsole).

#![allow(
    unsafe_code,
    reason = "Localized Windows ConPTY/Job Object FFI; ADR-0001 and ADR-0006 \
              permit reviewed, localized unsafe in one platform adapter."
)]

use std::ffi::c_void;
use std::thread::JoinHandle;

use tokio::sync::{mpsc, oneshot};
use windows::Win32::Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0};
use windows::Win32::Storage::FileSystem::{ReadFile, WriteFile};
use windows::Win32::System::Console::{
    COORD, ClosePseudoConsole, CreatePseudoConsole, HPCON, ResizePseudoConsole,
};
use windows::Win32::System::JobObjects::{
    CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
    JobObjectExtendedLimitInformation, SetInformationJobObject, TerminateJobObject,
};
use windows::Win32::System::Pipes::CreatePipe;
use windows::Win32::System::Threading::{
    CREATE_UNICODE_ENVIRONMENT, CreateProcessW, DeleteProcThreadAttributeList,
    EXTENDED_STARTUPINFO_PRESENT, GetExitCodeProcess, INFINITE, InitializeProcThreadAttributeList,
    LPPROC_THREAD_ATTRIBUTE_LIST, PROC_THREAD_ATTRIBUTE_JOB_LIST,
    PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE, PROCESS_INFORMATION, STARTF_USESTDHANDLES, STARTUPINFOEXW,
    UpdateProcThreadAttribute, WaitForSingleObject,
};
use windows::core::{PCWSTR, PWSTR};

use crate::error::ExecError;
use crate::platform::cmd_raw_tail;
use crate::{JobSpec, READ_CHUNK_BYTES, Shell, TermSize};

/// Bounded depth of the merged-output channel from the reader thread to the
/// broker. With [`READ_CHUNK_BYTES`] this caps in-flight output memory and, when
/// the broker's consumer is slow, drives backpressure into the child.
const OUTPUT_CHANNEL_CAPACITY: usize = 64;

/// Bounded depth of the standard-input queue to the writer thread.
const STDIN_CHANNEL_CAPACITY: usize = 16;

/// A raw Windows handle that is safe to move to another thread.
///
/// The `windows` crate's [`HANDLE`] wraps a raw pointer and is therefore not
/// `Send`. Each handle in this module has exactly **one** owning thread (the
/// reader owns the output read end, the writer owns the input write end, the exit
/// waiter owns the process handle, and [`ConptyControl`] owns the job handle), so
/// moving it across the thread boundary once, with no sharing, is sound.
#[derive(Debug)]
struct SendHandle(HANDLE);
// SAFETY: single-owner move with no aliasing; see the type docs.
unsafe impl Send for SendHandle {}

/// A pseudo-console handle that is safe to move to the owning task.
///
/// Like [`SendHandle`]: [`HPCON`] wraps a raw pointer, but the handle has a
/// single owner ([`ConptyControl`]) that alone resizes and closes it.
#[derive(Debug)]
struct SendHpcon(HPCON);
// SAFETY: single-owner move with no aliasing; see the type docs.
unsafe impl Send for SendHpcon {}

/// A spawned interactive (ConPTY) job, decomposed into the parts the broker
/// distributes: the merged-output receiver, the standard-input sender, the exit
/// receiver, and the control handle for resize/terminate/shutdown.
#[derive(Debug)]
pub(crate) struct Conpty {
    /// Merged VT output from the pseudo-console (reader thread → broker).
    pub(crate) output_rx: mpsc::Receiver<Vec<u8>>,
    /// Bytes to the pseudo-console input (broker → writer thread).
    pub(crate) stdin_tx: mpsc::Sender<Vec<u8>>,
    /// The child's exit code once it exits, or `None` when it could not be read.
    pub(crate) exit_rx: oneshot::Receiver<Option<i32>>,
    /// Resize, terminate, and deadlock-safe shutdown for this job.
    pub(crate) control: ConptyControl,
}

/// Owns the pseudo-console and Job Object handles for one interactive job and the
/// reader thread's join handle. A single task owns this; nothing else touches
/// these handles, keeping the `unsafe` reasoning local.
#[derive(Debug)]
pub(crate) struct ConptyControl {
    hpc: SendHpcon,
    job: SendHandle,
    reader: Option<JoinHandle<()>>,
}

impl ConptyControl {
    /// Resizes the pseudo-console. The bounds are already enforced by
    /// [`TermSize`], so this cannot be asked to set a malformed size.
    ///
    /// # Errors
    ///
    /// [`ExecError::Io`] if the platform call fails.
    pub(crate) fn resize(&self, size: TermSize) -> Result<(), ExecError> {
        // SAFETY: `hpc` is a live pseudo-console owned solely by `self`; `coord`
        // is a plain value.
        unsafe { ResizePseudoConsole(self.hpc.0, coord(size)) }.map_err(win_io)
    }

    /// Terminates the whole job tree (`TerminateJobObject`). Idempotent: after a
    /// natural exit the job is already empty and this is a harmless no-op.
    pub(crate) fn terminate(&self) {
        // SAFETY: `job` is a live Job Object handle owned solely by `self`.
        let _ = unsafe { TerminateJobObject(self.job.0, 1) };
    }

    /// The number of live processes still in the job — an honest survivor count
    /// (REQ-F-008, REQ-N-008), not a guess. Used by the containment test.
    #[cfg(test)]
    pub(crate) fn active_processes(&self) -> u32 {
        use windows::Win32::System::JobObjects::{
            JOBOBJECT_BASIC_ACCOUNTING_INFORMATION, JobObjectBasicAccountingInformation,
            QueryInformationJobObject,
        };

        let mut info = JOBOBJECT_BASIC_ACCOUNTING_INFORMATION::default();
        let len = u32::try_from(size_of::<JOBOBJECT_BASIC_ACCOUNTING_INFORMATION>()).unwrap_or(0);
        // SAFETY: `job` is live; `info` is a correctly sized, writable out-buffer
        // matching `JobObjectBasicAccountingInformation`.
        let ok = unsafe {
            QueryInformationJobObject(
                Some(self.job.0),
                JobObjectBasicAccountingInformation,
                std::ptr::from_mut(&mut info).cast::<c_void>(),
                len,
                None,
            )
        };
        if ok.is_ok() { info.ActiveProcesses } else { 0 }
    }

    /// Deadlock-safe teardown: terminate the job, close the pseudo-console while
    /// the reader still drains on its own thread, then join the reader and close
    /// the job handle. Consumes `self`.
    pub(crate) fn shutdown(mut self) {
        // 1. Kill the tree first so the child stops producing (idempotent).
        self.terminate();
        // 2. Close the pseudo-console. This may block until pending output is
        //    drained; the reader thread is still running on another thread and
        //    drains it, so this cannot deadlock (see the module docs).
        // SAFETY: `hpc` is live and owned solely by `self`; closing it exactly
        //   once here consumes that ownership.
        unsafe { ClosePseudoConsole(self.hpc.0) };
        // 3. The pseudo-console's output write end is now closed, so the reader
        //    hits EOF and exits; join it to guarantee all output was forwarded.
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
        // 4. Close the Job Object handle. With KILL_ON_JOB_CLOSE this also reaps
        //    any straggler, a fail-closed net even if terminate missed one.
        // SAFETY: `job` is live and owned solely by `self`; closed exactly once.
        let _ = unsafe { CloseHandle(self.job.0) };
    }
}

/// Spawns `spec` under a fresh pseudo-console of `size`.
///
/// # Errors
///
/// [`ExecError::Spawn`] if any pipe, pseudo-console, Job Object, or process
/// creation step fails. On failure every handle created so far is released.
pub(crate) fn spawn(spec: &JobSpec, size: TermSize) -> Result<Conpty, ExecError> {
    // --- Pipes: (parent write → child stdin) and (child stdout → parent read).
    let (input_read, input_write) = create_pipe()?;
    let (output_read, output_write) = match create_pipe() {
        Ok(pair) => pair,
        Err(e) => {
            close(input_read);
            close(input_write);
            return Err(e);
        }
    };

    // --- Pseudo-console over the child's ends. It duplicates them internally, so
    //     the parent closes its copies of the child-side ends immediately.
    // SAFETY: `input_read`/`output_write` are live pipe ends; `coord` is a value.
    let hpc = match unsafe { CreatePseudoConsole(coord(size), input_read, output_write, 0) } {
        Ok(hpc) => hpc,
        Err(e) => {
            close(input_read);
            close(input_write);
            close(output_read);
            close(output_write);
            return Err(win_spawn(e));
        }
    };
    close(input_read);
    close(output_write);

    // --- Job Object with kill-on-close and no breakaway (REQ-S-014/S-009).
    let job = match create_job() {
        Ok(job) => job,
        Err(e) => {
            close(input_write);
            close(output_read);
            // SAFETY: `hpc` is live and not yet handed to a child; close once.
            unsafe { ClosePseudoConsole(hpc) };
            return Err(e);
        }
    };

    // --- Child via CreateProcessW with the pseudo-console + job attribute list.
    let process = match spawn_child(spec, hpc, job) {
        Ok(pi) => pi,
        Err(e) => {
            close(input_write);
            close(output_read);
            // SAFETY: both handles are live and owned here; close once each.
            let _ = unsafe { CloseHandle(job) };
            unsafe { ClosePseudoConsole(hpc) };
            return Err(e);
        }
    };

    // --- Bridges: reader thread, writer thread + forward task, exit waiter.
    let (output_tx, output_rx) = mpsc::channel::<Vec<u8>>(OUTPUT_CHANNEL_CAPACITY);
    let reader = spawn_reader(SendHandle(output_read), output_tx);

    let (stdin_tx, stdin_rx) = mpsc::channel::<Vec<u8>>(STDIN_CHANNEL_CAPACITY);
    spawn_writer(SendHandle(input_write), stdin_rx);

    let exit_rx = spawn_exit_waiter(SendHandle(process.hProcess));

    Ok(Conpty {
        output_rx,
        stdin_tx,
        exit_rx,
        control: ConptyControl {
            hpc: SendHpcon(hpc),
            job: SendHandle(job),
            reader: Some(reader),
        },
    })
}

/// The blocking reader: reads merged VT output to EOF, forwarding bounded chunks.
/// A full channel blocks the read (backpressure); a gone consumer or a closed
/// pipe ends it. Closes its handle on exit.
fn spawn_reader(handle: SendHandle, tx: mpsc::Sender<Vec<u8>>) -> JoinHandle<()> {
    std::thread::spawn(move || {
        let handle = handle;
        let mut buf = vec![0u8; READ_CHUNK_BYTES];
        loop {
            let mut read: u32 = 0;
            // SAFETY: `handle.0` is the live output read end owned by this
            // thread; `buf` outlives the call; `read` is a valid out-param.
            let result = unsafe { ReadFile(handle.0, Some(&mut buf), Some(&mut read), None) };
            match result {
                Ok(()) if read > 0 => {
                    let chunk = buf[..read as usize].to_vec();
                    if tx.blocking_send(chunk).is_err() {
                        break;
                    }
                }
                // Zero bytes read is EOF; an error is a broken/closed pipe.
                _ => break,
            }
        }
        close(handle.0);
    })
}

/// The blocking writer: drains queued input to the pseudo-console, ending when
/// the queue closes (all senders dropped) or a write fails. Closes its handle on
/// exit, which gives the child EOF on standard input.
fn spawn_writer(handle: SendHandle, mut rx: mpsc::Receiver<Vec<u8>>) {
    std::thread::spawn(move || {
        let handle = handle;
        while let Some(bytes) = rx.blocking_recv() {
            if write_all(handle.0, &bytes).is_err() {
                break;
            }
        }
        close(handle.0);
    });
}

/// The blocking exit waiter: waits for the child, reads its exit code, and
/// reports it once. Closes the process handle on exit. Reports `None` if the wait
/// or the code read fails, so the broker never fabricates a clean exit.
fn spawn_exit_waiter(handle: SendHandle) -> oneshot::Receiver<Option<i32>> {
    let (tx, rx) = oneshot::channel();
    std::thread::spawn(move || {
        let handle = handle;
        // SAFETY: `handle.0` is the live process handle owned by this thread.
        let waited = unsafe { WaitForSingleObject(handle.0, INFINITE) };
        let code = if waited == WAIT_OBJECT_0 {
            let mut code: u32 = 0;
            // SAFETY: process has exited; `code` is a valid out-param.
            if unsafe { GetExitCodeProcess(handle.0, &mut code) }.is_ok() {
                Some(code as i32)
            } else {
                None
            }
        } else {
            None
        };
        let _ = tx.send(code);
        close(handle.0);
    });
    rx
}

/// Creates one anonymous pipe, returning `(read, write)`.
fn create_pipe() -> Result<(HANDLE, HANDLE), ExecError> {
    let mut read = HANDLE::default();
    let mut write = HANDLE::default();
    // SAFETY: both out-params are valid, writable locals.
    unsafe { CreatePipe(&mut read, &mut write, None, 0) }.map_err(win_spawn)?;
    Ok((read, write))
}

/// Creates a Job Object limited to kill-on-close (fail closed), no breakaway.
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

/// Spawns the child through `CreateProcessW` with a `STARTUPINFOEXW` attribute
/// list binding both the pseudo-console and the Job Object at creation. Returns
/// the [`PROCESS_INFORMATION`]; the caller owns `hProcess` (the thread handle is
/// closed here).
fn spawn_child(spec: &JobSpec, hpc: HPCON, job: HANDLE) -> Result<PROCESS_INFORMATION, ExecError> {
    // Attribute list sized for two attributes (pseudo-console + job list).
    let attr_count = 2;
    let mut list_size: usize = 0;
    // SAFETY: the sizing call is expected to fail with the required size written
    // to `list_size`; we ignore its Err and use the size.
    let _ = unsafe {
        InitializeProcThreadAttributeList(
            Some(LPPROC_THREAD_ATTRIBUTE_LIST(std::ptr::null_mut())),
            attr_count,
            None,
            &mut list_size,
        )
    };
    let mut attr_buf = vec![0u8; list_size];
    let attr_list = LPPROC_THREAD_ATTRIBUTE_LIST(attr_buf.as_mut_ptr().cast::<c_void>());
    // SAFETY: `attr_buf` is `list_size` bytes and outlives the list's use below.
    unsafe { InitializeProcThreadAttributeList(Some(attr_list), attr_count, None, &mut list_size) }
        .map_err(win_spawn)?;

    // Pin the job-handle array for the lifetime of `attr_list`.
    let job_handles = [job];

    // SAFETY: `attr_list` is initialized; each value pointer and size matches its
    // attribute; both values outlive `CreateProcessW` below. The pseudo-console
    // attribute takes the HPCON *value itself* as `lpValue` (not its address, as
    // the job list does) — the console setup reads that stored pointer directly as
    // the HPCON. Passing `&hpc` instead yields a child that starts without a
    // console and dies during DLL init (STATUS_DLL_INIT_FAILED).
    let update = unsafe {
        UpdateProcThreadAttribute(
            attr_list,
            0,
            PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE as usize,
            Some(hpc.0 as *const c_void),
            size_of::<HPCON>(),
            None,
            None,
        )
        .and_then(|()| {
            UpdateProcThreadAttribute(
                attr_list,
                0,
                PROC_THREAD_ATTRIBUTE_JOB_LIST as usize,
                Some(job_handles.as_ptr().cast::<c_void>()),
                size_of::<HANDLE>(),
                None,
                None,
            )
        })
    };
    if let Err(e) = update {
        // SAFETY: `attr_list` was initialized; delete it once on the error path.
        unsafe { DeleteProcThreadAttributeList(attr_list) };
        return Err(win_spawn(e));
    }

    let mut startup = STARTUPINFOEXW::default();
    startup.StartupInfo.cb = u32::try_from(size_of::<STARTUPINFOEXW>()).unwrap_or(0);
    startup.lpAttributeList = attr_list;
    // Bind the child to the pseudo-console rather than any inherited standard
    // handles: mark USESTDHANDLES with all three handles null. Without this, when
    // the parent's own stdio is redirected (an elevated agent with no console, or
    // a test harness), the child inherits those handles instead of the console and
    // its output never reaches the pseudo-console (empirically confirmed; the same
    // fix the `conpty` crate carries for redirected/nextest parents).
    startup.StartupInfo.hStdInput = HANDLE::default();
    startup.StartupInfo.hStdOutput = HANDLE::default();
    startup.StartupInfo.hStdError = HANDLE::default();
    startup.StartupInfo.dwFlags |= STARTF_USESTDHANDLES;

    // The shell is named by its full path in the system folder, so that
    // nothing is looked for by name: not in the folder of this program, not
    // in its working directory (ST-M006-020; see `crate::system`).
    let program = to_utf16(
        crate::system::shell_program(spec.shell)
            .map_err(ExecError::Spawn)?
            .as_os_str(),
    );
    let mut command = command_line_utf16(spec);
    let cwd = spec
        .working_dir
        .as_ref()
        .map(|dir| to_utf16(dir.as_os_str()));
    let env = env_block(spec);

    let mut info = PROCESS_INFORMATION::default();
    // SAFETY: `program` is a null-terminated UTF-16 string and `command` a
    // writable, null-terminated UTF-16 buffer; the startup info and its
    // attribute list are valid; `program`/`cwd`/`env` outlive the call; the
    // out-param `info` is writable.
    let created = unsafe {
        CreateProcessW(
            PCWSTR(program.as_ptr()),
            Some(PWSTR(command.as_mut_ptr())),
            None,
            None,
            false,
            EXTENDED_STARTUPINFO_PRESENT | CREATE_UNICODE_ENVIRONMENT,
            env.as_ref().map(|block| block.as_ptr().cast::<c_void>()),
            cwd.as_ref().map_or(PCWSTR::null(), |w| PCWSTR(w.as_ptr())),
            std::ptr::from_ref(&startup.StartupInfo),
            &mut info,
        )
    };

    // The attribute list has done its job whether creation succeeded or not.
    // SAFETY: `attr_list` was initialized; delete it exactly once here.
    unsafe { DeleteProcThreadAttributeList(attr_list) };

    created.map_err(win_spawn)?;

    // The primary thread handle is not needed; close it, keep the process handle.
    // SAFETY: `hThread` is a live handle returned by CreateProcessW; close once.
    let _ = unsafe { CloseHandle(info.hThread) };
    Ok(info)
}

/// Writes all of `bytes` to `handle`, looping over partial writes.
fn write_all(handle: HANDLE, bytes: &[u8]) -> Result<(), ExecError> {
    let mut offset = 0usize;
    while offset < bytes.len() {
        let mut written: u32 = 0;
        // SAFETY: `handle` is the live input write end; the slice is valid for
        // the call and `written` is a valid out-param.
        unsafe { WriteFile(handle, Some(&bytes[offset..]), Some(&mut written), None) }
            .map_err(win_io)?;
        if written == 0 {
            return Err(ExecError::Io(std::io::Error::from(
                std::io::ErrorKind::WriteZero,
            )));
        }
        offset += written as usize;
    }
    Ok(())
}

/// A `COORD` from a validated [`TermSize`]. The bounds guarantee the fit into the
/// `i16` fields, so the conversion cannot overflow.
fn coord(size: TermSize) -> COORD {
    COORD {
        X: i16::try_from(size.cols()).unwrap_or(i16::MAX),
        Y: i16::try_from(size.rows()).unwrap_or(i16::MAX),
    }
}

/// Closes a handle, ignoring the result (used on cleanup paths).
fn close(handle: HANDLE) {
    // SAFETY: called once per handle on a cleanup path; a already-invalid handle
    // makes this a harmless no-op.
    let _ = unsafe { CloseHandle(handle) };
}

/// Maps a Windows error to a spawn failure.
fn win_spawn(e: windows::core::Error) -> ExecError {
    ExecError::Spawn(std::io::Error::from_raw_os_error(e.code().0))
}

/// Maps a Windows error to a job-I/O failure.
fn win_io(e: windows::core::Error) -> ExecError {
    ExecError::Io(std::io::Error::from_raw_os_error(e.code().0))
}

/// Builds the child command line as a mutable, null-terminated UTF-16 buffer.
fn command_line_utf16(spec: &JobSpec) -> Vec<u16> {
    let mut wide: Vec<u16> = command_line(spec.shell, &spec.command)
        .encode_utf16()
        .collect();
    wide.push(0);
    wide
}

/// The command line of a shell invocation, mirroring the piped backend's
/// mapping for a non-empty command.
///
/// Its first word names the shell for the shell itself. Which program is
/// started is not decided here: the child is created from the shell's full
/// path (see `spawn_child`).
///
/// An **empty** command opens the shell's own interactive REPL under the
/// pseudo-console — the human-terminal case (an interactive `TermOpen` carries
/// no command, ST-M003-009). Running `cmd.exe`/`powershell.exe` with no
/// `/C`/`-Command` gives a live prompt that renders and echoes input; the piped
/// `/C ""` form would instead exit immediately. A non-empty command keeps the
/// one-shot `-Command`/`/C` semantics, so this only changes the terminal case.
///
/// A non-empty PowerShell command is one argument, quoted by the MSVC
/// `CommandLineToArgvW` rules that `powershell.exe` reads. `cmd.exe` does not
/// read those rules, so a non-empty Command Prompt command is appended raw, as
/// `/S /C "<command>"`, and reaches `cmd.exe` exactly as written (ST-M003-001;
/// see [`cmd_raw_tail`]).
fn command_line(shell: Shell, command: &str) -> String {
    match (shell, command.is_empty()) {
        (Shell::PowerShell, true) => "powershell.exe -NoProfile -NoLogo".to_string(),
        (Shell::PowerShell, false) => {
            let mut line = "powershell.exe -NoProfile -Command ".to_string();
            append_quoted(command, &mut line);
            line
        }
        (Shell::Cmd, true) => "cmd.exe".to_string(),
        (Shell::Cmd, false) => format!("cmd.exe {}", cmd_raw_tail(command)),
    }
}

/// Appends `arg` to `out`, quoted and escaped per the MSVC command-line rules so
/// `CommandLineToArgvW` recovers it exactly.
fn append_quoted(arg: &str, out: &mut String) {
    let needs_quotes = arg.is_empty()
        || arg
            .chars()
            .any(|c| c == ' ' || c == '\t' || c == '\n' || c == '\u{0b}' || c == '"');
    if !needs_quotes {
        out.push_str(arg);
        return;
    }
    out.push('"');
    let mut backslashes = 0usize;
    for c in arg.chars() {
        match c {
            '\\' => backslashes += 1,
            '"' => {
                for _ in 0..(backslashes * 2 + 1) {
                    out.push('\\');
                }
                backslashes = 0;
                out.push('"');
            }
            _ => {
                for _ in 0..backslashes {
                    out.push('\\');
                }
                backslashes = 0;
                out.push(c);
            }
        }
    }
    for _ in 0..(backslashes * 2) {
        out.push('\\');
    }
    out.push('"');
}

/// A double-null-terminated UTF-16 environment block layering `spec.env` onto the
/// broker's environment, or `None` to inherit unchanged when there are no
/// overrides.
fn env_block(spec: &JobSpec) -> Option<Vec<u16>> {
    if spec.env.is_empty() {
        return None;
    }
    // Case-insensitive merge: start from the broker environment, then apply
    // overrides (later wins), keyed by the upper-cased name.
    let mut merged: std::collections::BTreeMap<String, (String, String)> =
        std::collections::BTreeMap::new();
    for (key, value) in std::env::vars() {
        merged.insert(key.to_uppercase(), (key, value));
    }
    for (key, value) in &spec.env {
        merged.insert(key.to_uppercase(), (key.clone(), value.clone()));
    }

    let mut block: Vec<u16> = Vec::new();
    for (_, (key, value)) in merged {
        block.extend(format!("{key}={value}").encode_utf16());
        block.push(0);
    }
    block.push(0);
    Some(block)
}

/// UTF-16, null-terminated encoding of an OS string, for path arguments.
fn to_utf16(s: &std::ffi::OsStr) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;
    let mut wide: Vec<u16> = s.encode_wide().collect();
    wide.push(0);
    wide
}

#[cfg(test)]
mod tests;
