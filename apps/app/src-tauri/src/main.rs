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
    let stem = std::env::current_exe()
        .ok()
        .and_then(|exe| {
            exe.file_stem()
                .map(|stem| stem.to_string_lossy().into_owned())
        })
        .unwrap_or_default();

    match launch::resolve(&args, &stem, CUSTOMER_AVAILABLE) {
        Launch::Subcommand(args) => subcommand(args),
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
        Launch::Start => cretlink_app::run(Screen::Start { declined: false }),
        Launch::Role {
            role: Role::Technician,
            ..
        } => cretlink_app::run(Screen::Role(Role::Technician)),
        Launch::Role {
            role: Role::Customer,
            relaunched,
        } => cretlink_app::run_customer(relaunched),
    }
}

/// Runs a subcommand without a window, exactly as `cretlink-cli` does.
fn subcommand(args: Vec<String>) -> ExitCode {
    // Route panics to stderr only — never stdout, which may be the JSON-RPC
    // stream of `cretlink mcp`.
    std::panic::set_hook(Box::new(|info| {
        eprintln!("cretlink: panic: {info}");
    }));
    match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime.block_on(cretlink_cli::run("cretlink", args)),
        Err(err) => {
            eprintln!("cretlink: cannot start the async runtime: {err}");
            ExitCode::FAILURE
        }
    }
}
