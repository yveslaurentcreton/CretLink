//! The ST-M004-005 stdio conformance harness, run against `cretlink-cli mcp`
//! (ST-M009-002): the subcommand must behave exactly like the `cretlink-mcp`
//! binary it replaces.

/// The binary under test.
const BRIDGE_BIN: &str = env!("CARGO_BIN_EXE_cretlink-cli");
/// The arguments that make it serve MCP over stdio.
const BRIDGE_ARGS: &[&str] = &["mcp"];

#[path = "../../mcp-bridge/tests/conformance/mod.rs"]
mod conformance;
