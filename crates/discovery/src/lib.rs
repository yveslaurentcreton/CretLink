//! LAN discovery and the manual connection-code fallback for CretLink
//! (ST-M002-003, ADR-0007).
//!
//! This crate lets the console **advertise** itself and the support agent
//! **browse** for it over standard mDNS/DNS-SD ([`SERVICE_TYPE`]), and it
//! provides the mandatory manual [`ConnectionCode`] fallback for networks
//! where multicast is blocked (REQ-F-004).
//!
//! # Discovery grants no authority (trust boundary)
//!
//! Discovery sits **outside** the trust boundary
//! (the architecture requirements' LAN boundary). Everything this crate
//! surfaces about a peer — its advertised addresses, its TXT `v`/`name`
//! hints, a scanned or typed connection code — is **untrusted network input**.
//! It is an *addressing hint only*. It authenticates nothing and grants no
//! control. A [`DiscoveredConsole`] is deliberately **not** a trusted or
//! paired peer type: trust is established solely by the ST-M002-004 Session
//! Approval Code bound to the pairing transcript (REQ-F-005, REQ-S-006), which
//! fails closed (REQ-S-009). A malicious LAN peer may advertise a fake console
//! or flood announcements; the caller must present candidates for **explicit
//! selection**, never auto-connect and never pre-select one (ADR-0007).
//!
//! # LAN exposure is minimized (REQ-S-010)
//!
//! Advertising and browsing are scoped to **private** interfaces only —
//! RFC1918, link-local, and ULA addresses (see [`is_private_addr`] and
//! [`private_interface_addrs`]) — bound per address, never to `0.0.0.0` or
//! `::`. The advertised SRV port is the ST-M002-004 control-channel TCP
//! listener; discovery itself opens **no internet-reachable listener by
//! default**. Its only network accommodation is multicast UDP 5353, and the
//! manual [`ConnectionCode`] removes even that.
//!
//! A console waits on **one** private address ([`listen_candidates`],
//! [`first_choice`]) and announces that address alone, on its adapter alone
//! (ST-M009-016): a computer on several networks never announces an address
//! that nothing listens on.
//!
//! # No firewall changes
//!
//! This crate performs no OS firewall mutation. Where a host firewall blocks
//! multicast UDP 5353, browsing finds nothing; the manual [`ConnectionCode`]
//! path needs no multicast at all.
//!
//! # Untrusted-input handling
//!
//! Malformed or hostile TXT/SRV records degrade to `None`/empty fields and
//! never panic; connection codes are checksum-validated **before** any
//! connection is attempted, with a distinct
//! [`ManualEntryError::ChecksumMismatch`].

mod code;
mod error;
mod interface;
mod listen;

mod advertiser;
mod browser;

pub use advertiser::Advertiser;
pub use browser::{Browser, DiscoveredConsole};
pub use code::{
    ConnectionCode, ConnectionTarget, ManualEntryError, TargetSource, parse_manual_entry,
};
pub use error::DiscoveryError;
pub use interface::{is_private_addr, private_interface_addrs};
pub use listen::{ListenCandidate, default_route_addr, first_choice, listen_candidates};

/// The DNS-SD service type CretLink consoles advertise and agents browse for
/// (ADR-0007).
///
/// Seeing this service type advertised is an **untrusted hint** that a console
/// *might* be reachable at the advertised address; it conveys no authority.
pub const SERVICE_TYPE: &str = "_cretlink._tcp.local.";

/// TXT key carrying the advertised protocol version as `"major.minor"`.
///
/// An **untrusted hint** used only to pre-filter obviously incompatible peers
/// in the UI; the authoritative compatibility check happens during pairing.
pub const TXT_KEY_VERSION: &str = "v";

/// TXT key carrying a human-readable display name.
///
/// An **untrusted hint** for the selection UI only. It is attacker-controlled
/// and must never be treated as identity.
pub const TXT_KEY_NAME: &str = "name";

#[cfg(test)]
mod tests;
