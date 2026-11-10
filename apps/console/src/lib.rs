//! The technician role of CretLink (Give help), as a library.
//!
//! The console owns technician-side session state, the audit trail, the
//! emergency stop, and hosts the local MCP bridge. The library is shared by the
//! application `cretlink` (`apps/app`, Give help) and the headless
//! `cretlink-console` binary (`main.rs`). That binary is a development and test
//! tool: it is not in the release archives, which ship only `cretlink` and
//! `cretlink-cli` (ST-M009-010, ADR-0018).
//!
//! The library's modules:
//!
//! - [`state`]: the technician-side session-UI state model;
//! - [`driver`]: folds a live session event stream into that model;
//! - [`service`]: the console session service that owns the live session and
//!   serves the interactive CLI, the GUI, and the MCP bridge concurrently;
//! - [`ipc_server`]: the same-user IPC server that routes MCP-bridge calls and
//!   terminal attachments to the one consented session (REQ-F-013, REQ-F-016);
//! - [`audit`]: the unified per-session audit service;
//! - [`activity`]: the technician-side activity fold over the audit records
//!   (ST-M009-011);
//! - [`history`]: the session history read back from the audit folder, with
//!   each file's chain and seal verified (ST-M009-012);
//! - [`audit_entries`]: one session's audit entries for the technician's Audit
//!   tab (ST-M009-008);
//! - [`session`]: the plaintext test-harness client ([`SessionClient`] and its
//!   [`SessionTarget`] MCP adapter) used by tests against the simulator.

pub mod activity;
pub mod audit;
pub mod audit_entries;
pub mod display_name;
pub mod driver;
pub mod history;
pub mod ipc_server;
pub mod service;
pub mod session;
pub mod state;

pub use activity::{TechnicianActivity, UNKNOWN_CLIENT_LABEL, client_label};
pub use audit::{
    AUDIT_CHANNEL_CAPACITY, AuditServiceConfig, CLEANUP_REPORT_ACTION, ManualActivity, PruneReport,
    SESSION_ENDED_ACTION, SessionAuditStart, TARGET_IDENTITY_ACTION, console_cancel,
    console_exec_finished, console_exec_refused, console_exec_started, default_audit_dir,
    open_session_log, prune_audit_dir, report_prune, spawn_audit_service, terminal_closed,
    terminal_open_started, terminal_opened,
};
pub use audit_entries::{AuditActorView, AuditEntryView, session_entries};
pub use display_name::technician_display_name;
pub use driver::{DriverError, EventContext, EventOutcome, apply_session_event};
pub use history::{ActionCounts, Integrity, SessionSummary, list_sessions, read_session};
pub use ipc_server::{
    AuditEntry, AuditReceiver, ChannelAudit, ClosedRoute, ConsoleRoute, IpcAudit, IpcServer,
    LiveEcho, NoIpcAudit, SHUTDOWN_GRACE,
};
pub use service::{
    ConsoleSessionService, MAX_TERM_CONSUMERS, MAX_TERMINALS, TermAttachment, TermEvent,
};
pub use session::{ExecCollection, ExecRequest, SessionClient, SessionError, SessionTarget};
pub use state::{
    ConsoleState, ControlLevel, DiscoveredTarget, DiscoverySource, SessionPhase, StateError,
    StopReason, TargetId, TechnicianHost,
};
