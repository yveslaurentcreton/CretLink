//! The `cretlink` application (ST-M009-005, ADR-0018).
//!
//! Decides what this process does before any window opens: a subcommand of the
//! shared command line (`cretlink mcp`, `cretlink mcp install`, …), the start
//! screen, or one role's window. See [`cretlink_app::launch`].

// The application is a GUI-subsystem program on Windows: no console window on a
// double-click. Subcommands attach to the terminal that started them, and MCP
// clients use redirected stdio (ST-M009-001). Debug builds keep a console for
// logs.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::process::ExitCode;

use cretlink_app::launch::{self, Launch, Role};
use cretlink_app::{CUSTOMER_AVAILABLE, Screen};

/// Exit code for a command line this program refuses.
const EXIT_USAGE: u8 = 2;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let exe = std::env::current_exe().ok();
    let stem = exe
        .as_ref()
        .and_then(|exe| {
            exe.file_stem()
                .map(|stem| stem.to_string_lossy().into_owned())
        })
        .unwrap_or_default();
    // Whether this copy runs from the per-user install location (ST-M009-006).
    let installed = exe.as_deref().is_some_and(|exe| {
        cretlink_cli::install::Layout::current().is_some_and(|layout| layout.is_installed_exe(exe))
    });

    match launch::resolve(&args, &stem, CUSTOMER_AVAILABLE, installed) {
        Launch::Subcommand(args) => subcommand(&launch::program_name(&stem), args),
        Launch::Refuse(message) => {
            if cretlink_cli::process::attach_parent_console()
                == cretlink_cli::process::ConsoleAttach::None
            {
                cretlink_cli::process::alert("CretLink", &message);
            } else {
                eprintln!("cretlink: {message}");
            }
            ExitCode::from(EXIT_USAGE)
        }
        Launch::Start => cretlink_app::run(Screen::Start),
        Launch::Role {
            role: Role::Technician,
            ..
        } => cretlink_app::run(Screen::Role(Role::Technician)),
        Launch::Role {
            role: Role::Customer,
            entry,
        } => cretlink_app::run_customer(entry),
    }
}

/// Runs a subcommand without a window, exactly as `cretlink-cli` does.
/// `program` names the command in its texts: `cretlink`, or the file name of a
/// `CretLink-Support*` copy, whose only subcommands are `recover` (ST-M009-010)
/// and its own `uninstall` (ST-M009-017).
fn subcommand(program: &str, args: Vec<String>) -> ExitCode {
    // Route panics to stderr only — never stdout, which may be the JSON-RPC
    // stream of `cretlink mcp`.
    std::panic::set_hook(Box::new(|info| {
        eprintln!("cretlink: panic: {info}");
    }));
    match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime.block_on(cretlink_cli::run(program, args)),
        Err(err) => {
            eprintln!("cretlink: cannot start the async runtime: {err}");
            ExitCode::FAILURE
        }
    }
}
