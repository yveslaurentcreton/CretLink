//! The console-side mDNS advertiser (ADR-0007).
//!
//! The console registers [`SERVICE_TYPE`](crate::SERVICE_TYPE) for the one
//! private address its listener is bound to, on the adapter that owns that
//! address, never on `0.0.0.0`/`::` (REQ-S-010, ST-M009-016). The SRV record's
//! port is the ST-M002-004 control-channel TCP listener; the TXT record
//! carries only the untrusted `v` and `name` hints. Discovery itself opens no
//! internet-reachable listener — its only network accommodation is multicast
//! UDP 5353.

use std::fmt;
use std::net::SocketAddr;

use cretlink_protocol::ProtocolVersion;
use mdns_sd::{IfKind, ServiceDaemon, ServiceInfo};

use crate::error::DiscoveryError;
use crate::interface::is_advertisable_addr;
use crate::{SERVICE_TYPE, TXT_KEY_NAME, TXT_KEY_VERSION};

/// A live console advertisement over mDNS.
///
/// While an `Advertiser` is alive the console is discoverable on the LAN.
/// Dropping it best-effort unregisters the service (sending mDNS goodbye
/// packets) and shuts the daemon down, so the advertisement stops with the
/// value.
pub struct Advertiser {
    daemon: ServiceDaemon,
    fullname: String,
}

impl Advertiser {
    /// Registers this console for discovery.
    ///
    /// - `instance_name` is the DNS-SD instance label (should be a stable,
    ///   DNS-safe identifier for the console).
    /// - `listener` is the address and port of the ST-M002-004 control-channel
    ///   TCP listener that agents will dial: the one private address the
    ///   console waits on.
    /// - `protocol_version` and `display_name` become the TXT `v` and `name`
    ///   hints. They are advertised in the clear and are untrusted; never put
    ///   a secret in either (REQ-S-007).
    ///
    /// Only `listener`'s address is announced, and only on the adapter that
    /// owns it (ST-M009-016): a computer on several networks would otherwise
    /// announce addresses that nothing listens on. An address that is not a
    /// private LAN address is refused with [`DiscoveryError::NotPrivate`]
    /// rather than announced (REQ-S-010); the caller should offer the manual
    /// connection code instead.
    ///
    /// # Errors
    ///
    /// Returns [`DiscoveryError`] if the address is not private or the mDNS
    /// daemon rejects the registration.
    pub fn advertise(
        instance_name: &str,
        listener: SocketAddr,
        protocol_version: ProtocolVersion,
        display_name: &str,
    ) -> Result<Self, DiscoveryError> {
        let info = service_info(instance_name, listener, protocol_version, display_name)?;
        let fullname = info.get_fullname().to_owned();

        let daemon = ServiceDaemon::new().map_err(DiscoveryError::mdns)?;

        // Scope to the listener's adapter only: turn everything off, then
        // enable exactly the adapter that owns the vetted private address.
        // `IfKind::from(IpAddr)` identifies it, so mDNS binds only there and
        // never on 0.0.0.0/:: (REQ-S-010).
        daemon
            .disable_interface(IfKind::All)
            .map_err(DiscoveryError::mdns)?;
        daemon
            .enable_interface(IfKind::from(listener.ip()))
            .map_err(DiscoveryError::mdns)?;

        daemon.register(info).map_err(DiscoveryError::mdns)?;

        Ok(Self { daemon, fullname })
    }

    /// The DNS-SD fullname this console registered under (instance + service
    /// type). Useful for logs and audit; it is not a trusted identity.
    #[must_use]
    pub fn fullname(&self) -> &str {
        &self.fullname
    }
}

/// Builds the complete TXT property set advertised for a console: exactly the
/// protocol `version` under [`TXT_KEY_VERSION`] and the `display_name` under
/// [`TXT_KEY_NAME`], and nothing else (AC1, REQ-S-007).
///
/// Both values are advertised in the clear and are non-secret, untrusted hints.
/// Keeping this construction in one small pure function gives AC1 ("discovery
/// reveals only the minimum pre-pairing metadata") a deterministic regression
/// guard: any future TXT key (for example a stable instance UUID, which
/// ADR-0007 contemplates and which must remain non-secret) has to be added
/// here, changing this array and failing the frozen-metadata unit test until a
/// reviewer confirms the addition is intentional and non-secret.
pub(crate) fn advertise_properties<'a>(
    version: &'a str,
    display_name: &'a str,
) -> [(&'a str, &'a str); 2] {
    [(TXT_KEY_VERSION, version), (TXT_KEY_NAME, display_name)]
}

/// Builds the record a console announces: its instance, the one address and
/// port its listener is bound to, and the two untrusted hints.
///
/// Kept apart from the daemon so that "exactly the bound address, and only a
/// private one" has a test that needs no network (ST-M009-016, REQ-S-010).
pub(crate) fn service_info(
    instance_name: &str,
    listener: SocketAddr,
    protocol_version: ProtocolVersion,
    display_name: &str,
) -> Result<ServiceInfo, DiscoveryError> {
    if !is_advertisable_addr(listener.ip()) {
        return Err(DiscoveryError::NotPrivate(listener.ip()));
    }
    let version = protocol_version.to_string();
    let properties = advertise_properties(&version, display_name);
    let host_name = format!("{instance_name}.local.");
    ServiceInfo::new(
        SERVICE_TYPE,
        instance_name,
        &host_name,
        listener.ip(),
        listener.port(),
        &properties[..],
    )
    .map_err(DiscoveryError::mdns)
}

#[cfg(test)]
pub(crate) use self::advertise_properties as advertise_properties_for_tests;
#[cfg(test)]
pub(crate) use self::service_info as service_info_for_tests;

impl Drop for Advertiser {
    fn drop(&mut self) {
        // Best-effort: send goodbye packets and stop the daemon thread. Any
        // failure here is not actionable from a destructor; residual cache
        // entries expire on their own TTL.
        let _ = self.daemon.unregister(&self.fullname);
        let _ = self.daemon.shutdown();
    }
}

impl fmt::Debug for Advertiser {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Advertiser")
            .field("fullname", &self.fullname)
            .finish_non_exhaustive()
    }
}
