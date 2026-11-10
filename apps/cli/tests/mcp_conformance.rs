//! The ST-M004-005 stdio conformance harness, run against `cretlink-cli mcp`
//! (ST-M009-002, ST-M009-010). With the `gui-subsystem` feature the same
//! harness runs against the Windows GUI-subsystem build (ST-M009-001); it lives
//! in [`conformance`].

/// The binary under test.
const BRIDGE_BIN: &str = env!("CARGO_BIN_EXE_cretlink-cli");
/// The arguments that make it serve MCP over stdio.
const BRIDGE_ARGS: &[&str] = &["mcp"];

mod conformance;
