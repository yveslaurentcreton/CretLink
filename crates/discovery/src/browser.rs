//! The agent-side mDNS browser (ADR-0007).
//!
//! The agent browses [`SERVICE_TYPE`](crate::SERVICE_TYPE) on private
//! interfaces only and collects [`DiscoveredConsole`] candidates for the UI to
//! present. Selection is **explicit**: this type never dials, never
//! auto-connects, and never pre-selects a candidate. A LAN peer can advertise
//! a fake console or flood announcements, so every field here is untrusted and
//! grants no authority — trust comes only from the later Session Approval Code.

use std::collections::HashMap;
use std::fmt;
use std::net::IpAddr;
use std::time::{Duration, Instant};

use cretlink_protocol::ProtocolVersion;
use mdns_sd::{IfKind, ResolvedService, ServiceDaemon, ServiceEvent};

use crate::error::DiscoveryError;
use crate::interface::private_interface_addrs;
use crate::{SERVICE_TYPE, TXT_KEY_NAME, TXT_KEY_VERSION};

/// A console candidate observed during a browse.
///
/// **Untrusted.** Every field is derived from network announcements a hostile
/// peer can forge or replay. A `DiscoveredConsole` is an addressing hint for
/// the selection UI, not an authenticated or paired peer. Malformed TXT/SRV
/// data degrades to `None`/empty fields rather than being rejected or causing
/// a panic.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscoveredConsole {
    /// The DNS-SD instance label (service type suffix stripped). A display
    /// hint only.
    pub instance_name: String,
    /// The advertised human-readable name (TXT `name`), if present and
    /// non-empty. Attacker-controlled; never treat as identity.
    pub display_name: Option<String>,
    /// The addresses advertised for this console, sorted and de-duplicated.
    /// These are candidate dial targets for *pairing*, not trusted endpoints.
    pub addresses: Vec<IpAddr>,
    /// The advertised SRV port — the console's control-channel TCP listener.
    pub port: u16,
    /// The advertised protocol version (TXT `v`), if it parsed as
    /// `major.minor`. A hint for pre-filtering; the authoritative check
    /// happens during pairing.
    pub protocol_version: Option<ProtocolVersion>,
}

impl DiscoveredConsole {
    /// Builds a candidate from already-extracted, primitive fields.
    ///
    /// This is the pure core of candidate parsing (kept separate from the
    /// mDNS record types so it is directly testable with synthetic input).
    /// Any malformed hint degrades to `None`/empty; it never fails.
    fn from_parts(
        fullname: &str,
        mut addresses: Vec<IpAddr>,
        port: u16,
        txt_version: Option<&str>,
        txt_name: Option<&str>,
    ) -> Self {
        addresses.sort_unstable();
        addresses.dedup();
        Self {
            instance_name: instance_from_fullname(fullname),
            display_name: txt_name.map(str::to_owned).filter(|name| !name.is_empty()),
            addresses,
            port,
            protocol_version: txt_version.and_then(parse_protocol_version),
        }
    }

    /// Folds another observation of the same console into this one: unions
    /// addresses and fills in any field this candidate is still missing.
    fn merge(&mut self, other: DiscoveredConsole) {
        for addr in other.addresses {
            if !self.addresses.contains(&addr) {
                self.addresses.push(addr);
            }
        }
        self.addresses.sort_unstable();
        self.addresses.dedup();
        if self.display_name.is_none() {
            self.display_name = other.display_name;
        }
        if self.protocol_version.is_none() {
            self.protocol_version = other.protocol_version;
        }
    }
}

/// Browses the LAN for CretLink consoles over mDNS.
///
/// Scoped to private interfaces at construction (REQ-S-010). Dropping it shuts
/// the daemon down.
pub struct Browser {
    daemon: ServiceDaemon,
}

impl Browser {
    /// Creates a browser scoped to the host's private interfaces.
    ///
    /// If the host has no private LAN address the call fails with
    /// [`DiscoveryError::NoPrivateInterface`] rather than browsing from a
    /// public or unspecified address (REQ-S-010); the caller should offer the
    /// manual connection code instead.
    ///
    /// # Errors
    ///
    /// Returns [`DiscoveryError`] if interfaces cannot be enumerated, no
    /// private interface exists, or the mDNS daemon cannot be created or
    /// scoped.
    pub fn new() -> Result<Self, DiscoveryError> {
        let addrs = private_interface_addrs()?;
        if addrs.is_empty() {
            return Err(DiscoveryError::NoPrivateInterface);
        }

        let daemon = ServiceDaemon::new().map_err(DiscoveryError::mdns)?;
        daemon
            .disable_interface(IfKind::All)
            .map_err(DiscoveryError::mdns)?;
        let if_kinds: Vec<IfKind> = addrs.iter().copied().map(IfKind::from).collect();
        daemon
            .enable_interface(if_kinds)
            .map_err(DiscoveryError::mdns)?;

        Ok(Self { daemon })
    }

    /// Browses for `timeout`, returning the de-duplicated candidates observed.
    ///
    /// This performs no connection of any kind — it only listens and
    /// aggregates announcements. The returned list is sorted by instance name
    /// purely for stable display; that ordering is **not** a preference or a
    /// pre-selection. The caller must present the candidates for explicit
    /// selection (ADR-0007).
    ///
    /// # The returned list is uncapped — the caller must bound it
    ///
    /// Candidates are de-duplicated by instance name, but the number of
    /// *distinct* instances is **not** capped: a hostile LAN peer can advertise
    /// many distinct instance names to inflate the returned `Vec` for the
    /// duration of `timeout`. This is a LAN-local UI-nuisance / social-
    /// engineering vector, not a control or crash risk (the map is bounded by
    /// the browse window and freed on return, per-record size is bounded by the
    /// mDNS wire format, and no candidate grants authority). Per ADR-0007,
    /// rate-limiting and de-duplication of discovery results are the agent UI's
    /// responsibility (ST-M002-001): the caller **must** cap and/or rate-limit
    /// this list before display and must never pre-select a candidate.
    ///
    /// # Errors
    ///
    /// Returns [`DiscoveryError`] if the mDNS daemon cannot start the browse.
    pub fn browse(&self, timeout: Duration) -> Result<Vec<DiscoveredConsole>, DiscoveryError> {
        let receiver = self
            .daemon
            .browse(SERVICE_TYPE)
            .map_err(DiscoveryError::mdns)?;

        let deadline = Instant::now() + timeout;
        let mut candidates: HashMap<String, DiscoveredConsole> = HashMap::new();

        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                break;
            }
            match receiver.recv_timeout(remaining) {
                Ok(ServiceEvent::ServiceResolved(info)) => {
                    let candidate = resolved_to_console(info.as_ref());
                    candidates
                        .entry(candidate.instance_name.clone())
                        .and_modify(|existing| existing.merge(candidate.clone()))
                        .or_insert(candidate);
                }
                // Other events (SearchStarted, ServiceFound, ServiceRemoved,
                // SearchStopped, and any future non_exhaustive variant) carry
                // no resolved address and are ignored.
                Ok(_) => {}
                // Timeout or channel disconnect ends the browse window.
                Err(_) => break,
            }
        }

        let mut out: Vec<DiscoveredConsole> = candidates.into_values().collect();
        out.sort_by(|a, b| a.instance_name.cmp(&b.instance_name));
        Ok(out)
    }
}

impl Drop for Browser {
    fn drop(&mut self) {
        let _ = self.daemon.shutdown();
    }
}

impl fmt::Debug for Browser {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Browser").finish_non_exhaustive()
    }
}

/// Extracts the candidate fields from a resolved mDNS service. Addresses with
/// interface scope are flattened to plain [`IpAddr`]; the TXT `v`/`name` hints
/// are read best-effort.
fn resolved_to_console(service: &ResolvedService) -> DiscoveredConsole {
    let addresses: Vec<IpAddr> = service
        .get_addresses()
        .iter()
        .map(|scoped| scoped.to_ip_addr())
        .collect();
    DiscoveredConsole::from_parts(
        service.get_fullname(),
        addresses,
        service.get_port(),
        service.get_property_val_str(TXT_KEY_VERSION),
        service.get_property_val_str(TXT_KEY_NAME),
    )
}

/// Strips the `.<SERVICE_TYPE>` suffix from a DNS-SD fullname and unescapes the
/// common DNS-SD label escapes (`\.` and `\\`). Best-effort: an unexpected
/// shape falls back to the raw fullname. Display hint only.
pub(crate) fn instance_from_fullname(fullname: &str) -> String {
    let suffix_stripped = fullname
        .strip_suffix(SERVICE_TYPE)
        .and_then(|head| head.strip_suffix('.'))
        .unwrap_or(fullname);
    unescape_label(suffix_stripped)
}

/// Unescapes DNS-SD `\.` and `\\` sequences. Any other backslash is passed
/// through literally.
fn unescape_label(label: &str) -> String {
    let mut out = String::with_capacity(label.len());
    let mut chars = label.chars();
    while let Some(ch) = chars.next() {
        if ch == '\\' {
            match chars.next() {
                Some(next @ ('.' | '\\')) => out.push(next),
                Some(other) => {
                    out.push('\\');
                    out.push(other);
                }
                None => out.push('\\'),
            }
        } else {
            out.push(ch);
        }
    }
    out
}

/// Parses a `major.minor` protocol-version hint. Returns `None` on any
/// malformed input (never panics — this is untrusted TXT data).
pub(crate) fn parse_protocol_version(text: &str) -> Option<ProtocolVersion> {
    let (major, minor) = text.split_once('.')?;
    let major: u16 = major.parse().ok()?;
    let minor: u16 = minor.parse().ok()?;
    Some(ProtocolVersion::new(major, minor))
}

#[cfg(test)]
pub(crate) use self::{
    instance_from_fullname as instance_from_fullname_for_tests,
    parse_protocol_version as parse_protocol_version_for_tests,
};

#[cfg(test)]
impl DiscoveredConsole {
    /// Test-only accessor for the pure candidate constructor.
    pub(crate) fn from_parts_for_tests(
        fullname: &str,
        addresses: Vec<IpAddr>,
        port: u16,
        txt_version: Option<&str>,
        txt_name: Option<&str>,
    ) -> Self {
        Self::from_parts(fullname, addresses, port, txt_version, txt_name)
    }
}
