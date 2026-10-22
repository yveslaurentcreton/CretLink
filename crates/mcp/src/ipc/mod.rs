//! The same-user local IPC that links the thin MCP bridge to the console
//! (ADR-0004).
//!
//! The bridge holds no authority; it forwards MCP tool calls to the console —
//! the sole routing authority — over this IPC. Two halves make it up:
//!
//! - [`wire`] — the versioned, length-prefixed, `serde`-framed request/response
//!   contract, with bounds checked before allocation (REQ-N-012) and an
//!   explicit fail-closed [`IpcError::NoConsentedSession`] (REQ-S-009);
//! - [`transport`] — the `cfg`-gated same-user carrier (Windows named pipe /
//!   Unix domain socket) behind one OS-neutral surface.

pub mod transport;
pub mod wire;

pub use transport::{IpcEndpoint, IpcListener, IpcStream, connect};
pub use wire::{
    IPC_PROTOCOL, IPC_PURPOSE_MINOR, IpcCodecError, IpcError, IpcRequest, IpcResponse,
    MAX_IPC_FRAME_BYTES, TermFrame, read_message, write_message,
};
