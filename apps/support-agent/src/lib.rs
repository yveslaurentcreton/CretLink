//! CretLink Support: the customer-facing attended support agent (Windows).
//!
//! This library is the agent's portable core: honest privilege reporting
//! ([`privilege`]), the customer-side session lifecycle state machine
//! ([`state`]) and the [`driver`] that folds live session events into it, live
//! system inspection ([`sysinfo`]), the reboot-resume armer and ticket store
//! ([`reboot_resume`]), and the owned-resource inventory and recovery verb
//! ([`inventory`]). Command execution, file operations, terminals and the
//! desktop are served by the `cretlink-session` runtime the agent hosts. The
//! headless binary in `main.rs` and the graphical consent window
//! (`apps/agent-gui`) both build on this core; see ADR-0005.
//!
//! # Cross-platform CI
//!
//! The agent is a Windows product, but the workspace test matrix also builds on
//! macOS and Linux. All Windows-only integration — the elevation manifest in
//! `build.rs`, the process-token queries in [`privilege`], and the DPAPI
//! sealer — is therefore `cfg`-gated with deterministic non-Windows fallbacks,
//! so `cargo build`/`test --workspace` stays green on every host. All `unsafe`
//! FFI is confined to two reviewed `cfg(windows)` submodules,
//! `privilege::windows_token` and `dpapi` (ADR-0001).

pub mod driver;
pub mod inventory;
pub mod privilege;
pub mod reboot_resume;
pub mod state;
pub mod sysinfo;

/// The Windows DPAPI at-rest sealer for reboot-resume key material (ADR-0014
/// §4). Windows-only: it is the single `unsafe` FFI adapter in the reboot-resume
/// path, and the portable [`reboot_resume`] armer is generic over the sealer so
/// its logic is exercised on every host.
#[cfg(windows)]
pub mod dpapi;

pub use driver::{DriverError, EventContext, EventOutcome, apply_session_event};
pub use inventory::{
    Inventory, OwnedResource, RecoveryFailure, RecoveryReport, ResourceKind, UnreadableKind,
    recover, recover_at, scan, scan_at,
};
pub use privilege::{EffectiveIdentity, IntegrityLevel, current_identity, integrity_from_rid};
pub use reboot_resume::{
    TicketStore, VaultArmer, default_ticket_store_path, reboot_resume_arming_disclosure,
};
pub use state::{AgentError, AgentPhase, AgentState, StopReason, VisibleStatus};

#[cfg(windows)]
pub use dpapi::DpapiSealer;
