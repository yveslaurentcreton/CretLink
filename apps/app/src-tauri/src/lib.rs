//! The one CretLink application (ST-M009-005, ADR-0018).
//!
//! One `cretlink` executable holds the technician role (Give help), the
//! customer role (Get help, Windows only) and the command-line subcommands.
//! [`launch::resolve`] decides, before any window opens, what a process does;
//! the role is then fixed for the life of the process:
//!
//! - a technician process registers only the [`technician`] commands, and is
//!   the only one that binds the MCP bridge's IPC endpoint and the session
//!   listener;
//! - a customer process registers only the [`customer`] commands, installs
//!   nothing and leaves nothing behind (REQ-F-001). Unelevated, it first
//!   explains the UAC prompt (`#c-admin`) and relaunches elevated only when the
//!   customer presses **Continue** (REQ-F-002, ST-M009-009);
//! - the start screen registers [`start::choose_role`], which starts the
//!   chosen role in a new process, and the run-once-or-install commands of
//!   ST-M009-006 (this copy's facts, the install options, install itself).
//!
//! The frontend asks [`start::get_role`] which interface to load.

pub mod launch;
mod opener;
// The relay of this copy, for both roles (ST-M007-011).
mod relay;
mod start;

mod customer;
mod technician;

// A session through the relay, with both roles' drivers (ST-M007-013).
#[cfg(test)]
mod relayed_session_tests;

use std::io;
use std::path::Path;
use std::process::ExitCode;

use launch::{Entry, Role};
use tauri::{LogicalSize, Manager, RunEvent};

/// The release version this build belongs to (ST-M006-014).
pub use cretlink_cli::RELEASE_VERSION;

/// Whether the customer role exists in this build: on Windows, or for local
/// development under `dev-simulate`.
pub const CUSTOMER_AVAILABLE: bool = cfg!(any(windows, feature = "dev-simulate"));

/// What a window shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Screen {
    /// Get help or Give help.
    Start,
    /// A role's own interface.
    Role(Role),
}

impl Screen {
    /// Window title and size for this screen.
    fn window(self) -> (&'static str, f64, f64) {
        match self {
            Self::Start => ("CretLink", 760.0, 640.0),
            Self::Role(Role::Technician) => ("CretLink — Give help", 1024.0, 720.0),
            Self::Role(Role::Customer) => ("CretLink — Get help", 580.0, 800.0),
        }
    }
}

/// Opens the window for `screen` and runs until it closes. The customer role
/// opens through [`run_customer`].
#[must_use = "the exit code is what `main` must return"]
pub fn run(screen: Screen) -> ExitCode {
    run_screen(screen, Entry::Relaunched)
}

/// Opens the customer role, started as `entry` (ST-M009-009). It never
/// relaunches before its window opens: an unelevated process shows `#c-admin`
/// first, and only its **Continue** starts this program again elevated, behind
/// the UAC prompt. A process the application already started with `--role`
/// never relaunches again: it opens and shows its real, possibly unelevated,
/// identity (REQ-S-011).
#[must_use = "the exit code is what `main` must return"]
pub fn run_customer(entry: Entry) -> ExitCode {
    run_screen(Screen::Role(Role::Customer), entry)
}

fn run_screen(screen: Screen, entry: Entry) -> ExitCode {
    if let Err(err) = tauri::webview_version() {
        cretlink_cli::process::alert(
            "CretLink cannot open its window",
            &format!(
                "CretLink needs the Microsoft Edge WebView2 Runtime, which this computer does \
                 not have. Nothing was started and nothing was changed.\n\nInstall the \
                 WebView2 Runtime from Microsoft, or ask the person helping you for another \
                 way to connect.\n\n({err})"
            ),
        );
        return ExitCode::FAILURE;
    }

    let builder = tauri::Builder::default().manage(start::ProcessScreen(screen));
    let builder = match screen {
        Screen::Start => builder.invoke_handler(tauri::generate_handler![
            start::get_role,
            start::choose_role,
            technician::setup::get_app_info,
            technician::setup::install_options,
            technician::setup::install_app,
            technician::setup::open_installed
        ]),
        Screen::Role(Role::Technician) => technician::attach(builder),
        Screen::Role(Role::Customer) => customer::attach(builder, entry),
    };
    let (title, width, height) = screen.window();
    let builder = builder.setup(move |app| {
        if let Some(window) = app.get_webview_window("main") {
            window.set_title(title)?;
            window.set_size(LogicalSize::new(width, height))?;
            window.center()?;
        }
        if screen == Screen::Role(Role::Customer) {
            customer::on_window_start(app.handle());
        }
        Ok(())
    });
    let app = match builder.build(tauri::generate_context!()) {
        Ok(app) => app,
        Err(err) => {
            cretlink_cli::process::alert("CretLink failed to start", &err.to_string());
            return ExitCode::FAILURE;
        }
    };
    let code = app.run_return(move |handle, event| {
        // No path ends a customer process during a session without revoking
        // it first (ST-M009-009): the exit is held and the customer is asked.
        if let RunEvent::ExitRequested { api, code, .. } = &event
            && screen == Screen::Role(Role::Customer)
        {
            customer::on_exit_requested(handle, api, *code);
        }
    });
    ExitCode::from(u8::try_from(code).unwrap_or(1))
}

/// Starts `exe` with `args` as a normal child process.
pub(crate) fn start_process(exe: &Path, args: &[String]) -> io::Result<()> {
    std::process::Command::new(exe)
        .args(args)
        .spawn()
        .map(|_| ())
}

/// Starts `exe` with `args` elevated through the UAC prompt on Windows.
/// Returns `Ok(false)` when the customer declined the prompt. Elsewhere — a
/// `dev-simulate` build on macOS or Linux — there is no UAC: the child starts
/// as a normal, unelevated process, which its identity then says (dev only).
pub(crate) fn start_elevated(exe: &Path, args: &[String]) -> io::Result<bool> {
    if cfg!(windows) {
        return cretlink_cli::process::relaunch_elevated(exe, args)
            .map(|outcome| outcome == cretlink_cli::process::Elevation::Started);
    }
    start_process(exe, args).map(|()| true)
}
