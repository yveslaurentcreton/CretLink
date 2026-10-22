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
//! - a customer process registers only the [`customer`] commands, runs
//!   elevated after a visible UAC prompt (REQ-F-002), installs nothing and
//!   leaves nothing behind (REQ-F-001);
//! - the start screen registers only [`start::choose_role`], which starts the
//!   chosen role in a new process.
//!
//! The frontend asks [`start::get_role`] which interface to load.

pub mod launch;
mod start;

mod customer;
mod technician;

use std::io;
use std::path::Path;
use std::process::ExitCode;

use launch::Role;
use tauri::{LogicalSize, Manager};

/// The release version this build belongs to (ST-M006-014).
pub use cretlink_cli::RELEASE_VERSION;

/// Whether the customer role exists in this build: on Windows, or for local
/// development under `dev-simulate`.
pub const CUSTOMER_AVAILABLE: bool = cfg!(any(windows, feature = "dev-simulate"));

/// What a window shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Screen {
    /// Get help or Give help; `declined` after a declined UAC prompt.
    Start {
        /// Whether the customer just declined the UAC prompt of Get help.
        declined: bool,
    },
    /// A role's own interface.
    Role(Role),
}

impl Screen {
    /// Window title and size for this screen.
    fn window(self) -> (&'static str, f64, f64) {
        match self {
            Self::Start { .. } => ("CretLink", 760.0, 520.0),
            Self::Role(Role::Technician) => ("CretLink — Give help", 1024.0, 720.0),
            Self::Role(Role::Customer) => ("CretLink — Get help", 640.0, 760.0),
        }
    }
}

/// Opens the window for `screen` and runs until it closes.
#[must_use]
pub fn run(screen: Screen) -> ExitCode {
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
        Screen::Start { .. } => builder.invoke_handler(tauri::generate_handler![
            start::get_role,
            start::choose_role
        ]),
        Screen::Role(Role::Technician) => technician::attach(builder),
        Screen::Role(Role::Customer) => customer::attach(builder),
    };
    let (title, width, height) = screen.window();
    let builder = builder.setup(move |app| {
        if let Some(window) = app.get_webview_window("main") {
            window.set_title(title)?;
            window.set_size(LogicalSize::new(width, height))?;
            window.center()?;
        }
        Ok(())
    });
    match builder.run(tauri::generate_context!()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            cretlink_cli::process::alert("CretLink failed to start", &err.to_string());
            ExitCode::FAILURE
        }
    }
}

/// Opens the customer role. On Windows a process that is not elevated first
/// starts itself again elevated, behind the UAC prompt, and exits; when the
/// customer declines, the start screen explains it. A process the application
/// already relaunched (`relaunched`) never relaunches again: it opens and shows
/// its real, possibly unelevated, identity (REQ-S-011).
#[must_use]
pub fn run_customer(relaunched: bool) -> ExitCode {
    let elevated = cretlink_support_agent::current_identity().elevated;
    if cfg!(windows) && !elevated && !relaunched {
        let args = vec!["--role".to_owned(), Role::Customer.arg().to_owned()];
        match std::env::current_exe()
            .and_then(|exe| start_role_process(&exe, Role::Customer, &args))
        {
            Ok(true) => return ExitCode::SUCCESS,
            Ok(false) => return run(Screen::Start { declined: true }),
            Err(err) => {
                cretlink_cli::process::alert("CretLink could not get permission", &err.to_string());
                return ExitCode::FAILURE;
            }
        }
    }
    run(Screen::Role(Role::Customer))
}

/// Starts `exe` with `args` for `role`: elevated through UAC for the customer
/// role on Windows, as a normal child process otherwise. Returns `Ok(false)`
/// when the customer declined the UAC prompt.
pub(crate) fn start_role_process(exe: &Path, role: Role, args: &[String]) -> io::Result<bool> {
    if role == Role::Customer && cfg!(windows) {
        return cretlink_cli::process::relaunch_elevated(exe, args)
            .map(|outcome| outcome == cretlink_cli::process::Elevation::Started);
    }
    std::process::Command::new(exe)
        .args(args)
        .spawn()
        .map(|_| true)
}
