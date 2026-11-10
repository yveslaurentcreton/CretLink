//! The customer role of CretLink (Get help): the attended support agent on the
//! customer's Windows computer, as a library.
//!
//! This library is the agent's portable core: honest privilege reporting
//! ([`privilege`]), the customer-side session lifecycle state machine
//! ([`state`]) and the [`driver`] that folds live session events into it, live
//! system inspection ([`sysinfo`]), the reboot-resume armer and ticket store
//! ([`reboot_resume`]), the owned-resource inventory and recovery verb
//! ([`inventory`]) and the text every customer-side command line prints for it
//! ([`recovery`]). Command execution, file operations, terminals and the
//! desktop are served by the `cretlink-session` runtime the agent hosts. The
//! application `cretlink` (`apps/app`, Get help) and the headless
//! `cretlink-support-agent` binary (`main.rs`) both build on this core
//! (ADR-0018). That binary is a development and test tool: it is not in the
//! release archives, which ship only `cretlink` and `cretlink-cli`
//! (ST-M009-010).
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
pub mod recovery;
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
pub use recovery::{RecoverRun, run_recover, sweep_stale_staging};
pub use state::{AgentError, AgentPhase, AgentState, StopReason, VisibleStatus};

#[cfg(windows)]
pub use dpapi::DpapiSealer;
