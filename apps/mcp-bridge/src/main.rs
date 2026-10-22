//! The CretLink MCP bridge binary (`cretlink-mcp`), spawned over stdio by an
//! MCP client (ADR-0004, ST-M004-001).
//!
//! The server itself lives in [`cretlink_mcp::serve_stdio`], shared with
//! `cretlink-cli mcp` (ST-M009-002), which replaces this binary in ST-M009-010.
//!
//! # stdout is sacred
//!
//! stdout carries the MCP JSON-RPC stream. Every diagnostic — the connection
//! banner, errors, and a panic — goes to **stderr** only. A panic hook enforces
//! this so a crash cannot corrupt the protocol stream (ADR-0004 consequence).

use std::process::ExitCode;

#[tokio::main]
async fn main() -> ExitCode {
    // Route panics to stderr only — never stdout, which is the JSON-RPC stream.
    std::panic::set_hook(Box::new(|info| {
        eprintln!("cretlink-mcp: panic: {info}");
    }));
    cretlink_mcp::serve_stdio("cretlink-mcp").await
}
