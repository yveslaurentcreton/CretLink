//! Errors for interface enumeration and the mDNS advertiser/browser.
//!
//! Manual connection-code parsing has its own, distinct error taxonomy
//! ([`crate::ManualEntryError`]); these variants cover the network and
//! interface side. Messages identify the failed stage and a safe next action
//! and never carry secrets (REQ-N-015).

use thiserror::Error;

/// A discovery-side failure: interface enumeration, interface scoping, or the
/// underlying mDNS daemon.
#[derive(Debug, Error)]
pub enum DiscoveryError {
    /// The host's network interfaces could not be enumerated, so private
    /// interfaces could not be determined.
    #[error("failed to enumerate local network interfaces: {0}")]
    Interfaces(#[source] std::io::Error),

    /// No private (RFC1918, link-local, or ULA) interface address is available
    /// to advertise on or browse from. Advertising or browsing on a public or
    /// unspecified address is refused (REQ-S-010); the caller should fall back
    /// to the manual connection code (REQ-F-004).
    #[error(
        "no private LAN interface (RFC1918, link-local, or ULA) is available; \
         use the manual connection code instead"
    )]
    NoPrivateInterface,

    /// The mDNS daemon reported an error while registering, browsing, or
    /// scoping interfaces. The detail describes a socket or configuration
    /// problem and contains no secret material.
    #[error("mDNS daemon error: {0}")]
    Mdns(String),
}

impl DiscoveryError {
    /// Wraps an mDNS daemon error as a non-secret [`DiscoveryError::Mdns`].
    /// Takes anything `Display` so `error.rs` need not name the mDNS error
    /// type.
    pub(crate) fn mdns(err: impl core::fmt::Display) -> Self {
        Self::Mdns(err.to_string())
    }
}
