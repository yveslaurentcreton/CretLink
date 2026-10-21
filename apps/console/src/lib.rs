//! CretLink Console library.
//!
//! The console owns technician-side session state, the audit trail, the
//! emergency stop, and hosts the local MCP bridge. The library is shared by the
//! headless CLI (`main.rs`) and the graphical console (`apps/console-gui`):
//!
//! - [`state`]: the technician-side session-UI state model;
//! - [`driver`]: folds a live session event stream into that model;
//! - [`service`]: the console session service that owns the live session and
//!   serves the interactive CLI, the GUI, and the MCP bridge concurrently;
//! - [`ipc_server`]: the same-user IPC server that routes MCP-bridge calls and
//!   terminal attachments to the one consented session (REQ-F-013, REQ-F-016);
//! - [`audit`]: the unified per-session audit service;
//! - [`session`]: the plaintext test-harness client ([`SessionClient`] and its
//!   [`SessionTarget`] MCP adapter) used by tests against the simulator.

pub mod audit;
pub mod driver;
pub mod ipc_server;
pub mod service;
pub mod session;
pub mod state;

pub use audit::{
    AUDIT_CHANNEL_CAPACITY, AuditServiceConfig, ManualActivity, PruneReport, SessionAuditStart,
    console_cancel, console_exec_finished, console_exec_refused, console_exec_started,
    default_audit_dir, open_session_log, prune_audit_dir, report_prune, spawn_audit_service,
};
pub use driver::{DriverError, EventContext, EventOutcome, apply_session_event};
pub use ipc_server::{
    AuditEntry, AuditReceiver, ChannelAudit, ClosedRoute, ConsoleRoute, IpcAudit, IpcServer,
    NoIpcAudit, SHUTDOWN_GRACE,
};
pub use service::{
    ConsoleSessionService, MAX_TERM_CONSUMERS, MAX_TERMINALS, TermAttachment, TermEvent,
};
pub use session::{ExecCollection, ExecRequest, SessionClient, SessionError, SessionTarget};
pub use state::{
    ConsoleState, ControlLevel, DiscoveredTarget, DiscoverySource, SessionPhase, StateError,
    StopReason, TargetId, TechnicianHost,
};
