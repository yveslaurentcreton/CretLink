//! The `cretlink-shell` host-terminal binary (ST-M008-002, ADR-0004, ADR-0013).
//!
//! The client itself lives in [`cretlink_shell::cli`], shared with
//! `cretlink-cli shell` (ST-M009-002), which replaces this binary in ST-M009-010.

use std::process::ExitCode;

#[tokio::main]
async fn main() -> ExitCode {
    // Route panics to stderr only — never stdout, which is the terminal stream.
    std::panic::set_hook(Box::new(|info| {
        eprintln!("cretlink-shell: panic: {info}");
    }));
    cretlink_shell::cli::run("cretlink-shell", std::env::args().skip(1)).await
}
