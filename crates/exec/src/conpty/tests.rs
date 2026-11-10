//! ConPTY adapter white-box tests (Windows-only: the whole `conpty` module is
//! `cfg(windows)`, so this file compiles only there).
//!
//! These drive the adapter directly — no tokio runtime, since [`spawn`] bridges
//! through blocking OS threads and the channels' `blocking_*` methods work
//! without one. They prove the two properties the broker layer cannot see:
//! Job Object containment of the whole tree (REQ-S-014) and an honest exit code
//! straight from the exit waiter (REQ-N-008).

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::time::{Duration, Instant};

use super::{Conpty, command_line, spawn};
use crate::{JobSpec, Shell, TermSize};

/// Polls `f` every 50ms until it is true or `budget` elapses.
fn wait_until(budget: Duration, mut f: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + budget;
    loop {
        if f() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// A live interactive child appears inside the tracked Job Object, a resize
/// within bounds is applied, and `terminate` reaps the whole tree so no process
/// is left in the job (REQ-S-014). `active_processes` is the honest survivor
/// count that proves it.
#[test]
fn interactive_child_is_contained_and_resizable() {
    let spec = JobSpec::new(Shell::Cmd, "ping -n 30 127.0.0.1 > nul");
    let Conpty {
        output_rx,
        stdin_tx,
        exit_rx,
        control,
    } = spawn(&spec, TermSize::new(80, 24).unwrap()).expect("spawn conpty");

    // Keep the merged stream draining so the reader never blocks on a full
    // channel while we inspect the job.
    let drainer = std::thread::spawn(move || {
        let mut rx = output_rx;
        while rx.blocking_recv().is_some() {}
    });

    assert!(
        wait_until(Duration::from_secs(5), || control.active_processes() >= 1),
        "the interactive child must be live inside the tracked Job Object"
    );

    // A resize within bounds is applied without error while the job runs.
    control
        .resize(TermSize::new(120, 40).unwrap())
        .expect("resize within bounds");

    // Terminating the Job Object must reap the whole tree.
    control.terminate();
    assert!(
        wait_until(Duration::from_secs(10), || control.active_processes() == 0),
        "TerminateJobObject must leave no live process in the job"
    );

    // Deadlock-safe teardown while the drainer is still draining.
    control.shutdown();
    drop(stdin_tx);
    drop(exit_rx);
    let _ = drainer.join();
}

/// The exit waiter reports the child's real exit code, not a fabricated success
/// (REQ-N-008).
#[test]
fn exit_waiter_reports_the_real_exit_code() {
    let spec = JobSpec::new(Shell::Cmd, "exit 7");
    let Conpty {
        output_rx,
        stdin_tx,
        exit_rx,
        control,
    } = spawn(&spec, TermSize::new(80, 24).unwrap()).expect("spawn conpty");

    // Drain concurrently so a full output channel can never wedge the child.
    let drainer = std::thread::spawn(move || {
        let mut rx = output_rx;
        while rx.blocking_recv().is_some() {}
    });

    let code = exit_rx
        .blocking_recv()
        .expect("exit waiter dropped its result");
    assert_eq!(code, Some(7));

    control.shutdown();
    drop(stdin_tx);
    let _ = drainer.join();
}

/// The command line each shell form is started with. A Command Prompt command
/// is appended raw behind `/S /C`, so `cmd.exe` receives it exactly as written:
/// no quote is escaped and no backslash is doubled (ST-M003-001). A PowerShell
/// command keeps the C runtime quoting that `powershell.exe` reads, and the
/// forms without a command stay the shells' own prompts (ST-M003-009).
#[test]
fn command_lines_keep_cmd_raw_and_powershell_quoted() {
    assert_eq!(
        command_line(Shell::Cmd, r#"dir "C:\Program Files\""#),
        r#"cmd.exe /S /C "dir "C:\Program Files\"""#
    );
    assert_eq!(
        command_line(Shell::PowerShell, r#"Write-Output "a b""#),
        r#"powershell.exe -NoProfile -Command "Write-Output \"a b\"""#
    );
    assert_eq!(command_line(Shell::Cmd, ""), "cmd.exe");
    assert_eq!(
        command_line(Shell::PowerShell, ""),
        "powershell.exe -NoProfile -NoLogo"
    );
}
