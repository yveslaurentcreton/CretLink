//! The `cretlink-cli` command (ST-M009-002, ADR-0018): the console build of the
//! `cretlink` application's subcommands, for terminals and interactive use.
//!
//! See [`cretlink_cli`] for the subcommands. `cretlink mcp` serves MCP over
//! stdio, so every diagnostic of this process — including a panic — goes to
//! stderr only.
//!
//! With the `gui-subsystem` feature the Windows build opens no console window
//! (ST-M009-001, ADR-0018); see [`cretlink_cli::process`].

#![cfg_attr(all(windows, feature = "gui-subsystem"), windows_subsystem = "windows")]

use std::process::ExitCode;

#[tokio::main]
async fn main() -> ExitCode {
    // Route panics to stderr only — never stdout, which may be the JSON-RPC
    // stream (`cretlink mcp`) or the remote terminal (`cretlink shell`).
    std::panic::set_hook(Box::new(|info| {
        eprintln!("cretlink-cli: panic: {info}");
    }));
    cretlink_cli::run("cretlink-cli", std::env::args().skip(1)).await
}
