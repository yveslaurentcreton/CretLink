//! `cretlink-shell`: a standalone same-user terminal client for CretLink
//! (ST-M008-002, ADR-0004, ADR-0013).
//!
//! This is the **third** class of same-user local IPC consumer, beside the MCP
//! bridge ([`cretlink_mcp::McpBridge`]) and the console's own terminal panel: a
//! command-line client a technician runs inside a host terminal (Windows
//! Terminal, WezTerm, …) to work an interactive shell on the console's one
//! explicitly selected, consented session. It gives an SSH-like experience
//! **without** SSH — no key, no listener, and nothing installed on the customer
//! computer (REQ-F-023, REQ-S-015).
//!
//! # Authority model
//!
//! The client holds **no** session authority. It connects to the running
//! console over the same kernel-enforced same-user local IPC as the bridge
//! ([`cretlink_mcp::ipc::connect`]) and drives the existing, versioned
//! interactive-terminal wire ops ([`cretlink_mcp::IpcRequest::TermOpen`] /
//! `TermAttach` / `TermInput` / `TermResize` / `TermDetach` / `TermClose`, and
//! the server-pushed [`cretlink_mcp::TermFrame`] duplex). It adds no new
//! listener, endpoint, protocol variant, or trust anchor (REQ-S-023,
//! REQ-N-012). The console remains the sole routing and consent authority: with
//! no consented session, or after a revoke or stop, every call fails closed and
//! nothing runs (REQ-S-009), and the stream is torn down with an honest frame
//! (REQ-N-008).
//!
//! # Layering
//!
//! - [`engine`] — the transport-abstract terminal engine: the handshake, the
//!   fail-closed open/attach, and the bounded duplex pump. It is written against
//!   the async [`tokio::io`] traits rather than a real terminal, so the whole
//!   security-relevant path (fail-closed routing, honest stream end, and the
//!   guarantee that only caller-supplied input is ever forwarded) is exercised
//!   headlessly on every platform.
//! - [`tty`] — the thin real-terminal host used by the binary: local raw-mode
//!   setup, keystroke capture, and output rendering. It is the untested I/O edge
//!   (validated on real terminals as a maintainer hardware task); it holds no
//!   protocol logic.

pub mod engine;
pub mod tty;
