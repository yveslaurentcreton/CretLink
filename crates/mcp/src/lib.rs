//! Local MCP bridge for CretLink.
//!
//! Exposes generic remote-computer primitives — system information, command
//! execution (stdin, cancellation, job listing), file operations and transfer,
//! staging cleanup, and desktop and clipboard access — to MCP-capable clients
//! on the technician computer, bound to one explicitly selected console session
//! (REQ-F-013, REQ-F-016). The MCP-transport decision is recorded in
//! ADR-0004; message framing and the wire encoding are ADR-0010.
//!
//! The bridge is transport-agnostic: it is built on the async
//! [`TargetSession`] trait rather than the wire protocol, so the same MCP
//! surface can be driven by a real console session or by a deterministic test
//! fake. Every tool result embeds the session id and [`cretlink_protocol::TargetIdentity`]
//! so a result can never be mistaken for a different session or a local
//! machine, and every input is validated against the shared
//! [`cretlink_protocol::limits`] before it reaches the target.

mod bridge;
pub mod ipc;
mod ipc_target;
mod stdio;
mod target;

pub use bridge::{MAX_EXEC_OUTPUT_BYTES, MAX_STDIN_BASE64_CHARS, McpBridge};
pub use ipc::{IpcEndpoint, IpcError, IpcListener, IpcRequest, IpcResponse, IpcStream, TermFrame};
pub use ipc_target::IpcTargetSession;
pub use stdio::serve_stdio;
pub use target::{
    CallError, CaptureParams, CaptureReport, CleanupSummary, DirPage, DownloadParams,
    DownloadReport, ExecParams, ExecReport, Routed, StatReport, SystemInfoReport, TargetError,
    TargetSession, TransferFailure, UploadParams, UploadReport,
};
