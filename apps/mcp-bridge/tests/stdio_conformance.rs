//! The ST-M004-005 stdio conformance harness, run against the `cretlink-mcp`
//! binary. The same harness runs against `cretlink-cli mcp` in `apps/cli`
//! (ST-M009-002); it lives in [`conformance`].

/// The binary under test.
const BRIDGE_BIN: &str = env!("CARGO_BIN_EXE_cretlink-mcp");
/// The arguments that make it serve MCP over stdio.
const BRIDGE_ARGS: &[&str] = &[];

mod conformance;
