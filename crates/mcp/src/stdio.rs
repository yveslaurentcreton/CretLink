//! The stdio MCP server entry point shared by `cretlink-cli mcp` and the
//! `cretlink-mcp` binary (ADR-0004, ST-M004-001, ST-M009-002).
//!
//! The bridge holds **no** session authority. It serves the generic
//! remote-computer tools of [`McpBridge`] over the stdio MCP transport, backed
//! by an [`IpcTargetSession`] that forwards every tool call to the running
//! CretLink console over same-user local IPC. The console — the sole routing
//! authority — runs each call on its explicitly selected, consented session, or
//! fails closed when none exists (REQ-S-009). AI credentials never leave the
//! client/technician side (REQ-F-015, REQ-S-015).
//!
//! # stdout is sacred
//!
//! stdout carries the MCP JSON-RPC stream. Every diagnostic — the connection
//! banner and errors — goes to **stderr** only. The caller installs a panic hook
//! that does the same, so a crash cannot corrupt the protocol stream (ADR-0004
//! consequence).
//!
//! # Endpoint selection
//!
//! By default the bridge connects to the current user's console endpoint
//! ([`IpcEndpoint::user_default`]). Setting `CRETLINK_IPC_ENDPOINT` to a
//! non-empty name selects an explicitly named same-user endpoint instead
//! ([`IpcEndpoint::named`]). This never changes the transport or its authority:
//! the value is only a logical endpoint name, resolved to the same same-user
//! named pipe (Windows) or `0700`-directory Unix socket, with no path to TCP or
//! to another user. It exists so the shipped binary can be pointed at an
//! isolated per-test console in the ST-M004-005 conformance harness, and so an
//! advanced technician can run more than one console endpoint for one user.

use std::process::ExitCode;
use std::sync::Arc;

use rmcp::serve_server;
use rmcp::transport::stdio;

use crate::{IpcEndpoint, IpcTargetSession, McpBridge};

/// Serves the MCP bridge over this process's stdin and stdout until the client
/// disconnects. `program` prefixes every stderr diagnostic, so the technician
/// can tell which command wrote it.
pub async fn serve_stdio(program: &str) -> ExitCode {
    let endpoint = match std::env::var("CRETLINK_IPC_ENDPOINT") {
        Ok(name) if !name.trim().is_empty() => IpcEndpoint::named(name),
        _ => IpcEndpoint::user_default(),
    };
    eprintln!(
        "{program}: bridging MCP tool calls to the CretLink console over local IPC endpoint '{}'.",
        endpoint.name()
    );
    eprintln!(
        "{program}: tools act only on the console's selected, consented session; \
         with no console or no consented session they fail closed and nothing runs."
    );

    // Connecting never hard-fails: with no console or no consented session, the
    // tools return a clear fail-closed error rather than crashing the session.
    let target = IpcTargetSession::connect(endpoint).await;
    let bridge = McpBridge::new(Arc::new(target));

    match serve_server(bridge, stdio()).await {
        Ok(running) => match running.waiting().await {
            Ok(_reason) => ExitCode::SUCCESS,
            Err(err) => {
                eprintln!("{program}: MCP session ended with an error: {err}");
                ExitCode::FAILURE
            }
        },
        Err(err) => {
            eprintln!("{program}: failed to start the MCP stdio server: {err}");
            ExitCode::FAILURE
        }
    }
}
