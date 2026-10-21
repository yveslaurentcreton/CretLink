//! Interface address classification and private-interface enumeration
//! (REQ-S-010).
//!
//! CretLink discovery must be limited to the intended local interfaces and
//! must never advertise on or browse from a public or unspecified address.
//! [`is_private_addr`] is the pure predicate; [`private_interface_addrs`]
//! applies it to the host's real interfaces to produce the exact per-address
//! bind set the advertiser and browser use, preferring routable-private
//! addresses over automatically-assigned link-local ones so the deterministic
//! interface choice is also a reachable one (ST-M002-010).

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

use crate::error::DiscoveryError;

/// Where an address sits relative to the internet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Locality {
    /// `127.0.0.0/8` or `::1` — local to the host, never internet-reachable.
    Loopback,
    /// `0.0.0.0` or `::` — the unspecified address; never a real endpoint.
    Unspecified,
    /// IPv4 `169.254.0.0/16` or IPv6 `fe80::/10`.
    LinkLocal,
    /// IPv6 Unique Local Addresses, `fc00::/7`.
    UniqueLocal,
    /// IPv4 RFC1918 private ranges (`10/8`, `172.16/12`, `192.168/16`).
    Rfc1918,
    /// Anything else: a globally routable / public address.
    Global,
}

fn ipv4_locality(ip: Ipv4Addr) -> Locality {
    if ip.is_unspecified() {
        return Locality::Unspecified; // 0.0.0.0
    }
    if ip.is_loopback() {
        return Locality::Loopback; // 127.0.0.0/8
    }
    let o = ip.octets();
    if o[0] == 10 {
        return Locality::Rfc1918; // 10.0.0.0/8
    }
    if o[0] == 172 && (16..=31).contains(&o[1]) {
        return Locality::Rfc1918; // 172.16.0.0/12
    }
    if o[0] == 192 && o[1] == 168 {
        return Locality::Rfc1918; // 192.168.0.0/16
    }
    if o[0] == 169 && o[1] == 254 {
        return Locality::LinkLocal; // 169.254.0.0/16
    }
    Locality::Global
}

fn ipv6_locality(ip: Ipv6Addr) -> Locality {
    if ip.is_unspecified() {
        return Locality::Unspecified; // ::
    }
    if ip.is_loopback() {
        return Locality::Loopback; // ::1
    }
    // fe80::/10 — the first 10 bits are 1111111010.
    if (ip.segments()[0] & 0xffc0) == 0xfe80 {
        return Locality::LinkLocal;
    }
    // fc00::/7 — the first 7 bits are 1111110 (first byte 0xfc or 0xfd).
    if (ip.octets()[0] & 0xfe) == 0xfc {
        return Locality::UniqueLocal;
    }
    Locality::Global
}

fn locality(addr: IpAddr) -> Locality {
    match addr {
        IpAddr::V4(v4) => ipv4_locality(v4),
        IpAddr::V6(v6) => ipv6_locality(v6),
    }
}

/// Returns `true` when `addr` is a private or otherwise non-internet-facing
/// local address: RFC1918, IPv4/IPv6 link-local, IPv6 ULA, **or loopback**.
///
/// # Loopback handling
///
/// Loopback (`127.0.0.0/8`, `::1`) is classified as private: it is the most
/// local address there is and is never internet-reachable, so it can never be
/// a "public" address in the sense of REQ-S-010. Note that
/// [`private_interface_addrs`] nonetheless **excludes** loopback from the
/// advertise/browse bind set, because a loopback interface carries no LAN
/// reachability — advertising there would be pointless. The two functions
/// answer different questions: this predicate asks "is this address
/// non-public?", while [`private_interface_addrs`] asks "which addresses
/// should we actually bind mDNS to?".
///
/// The unspecified address (`0.0.0.0`, `::`) and any globally routable address
/// always return `false`.
#[must_use]
pub fn is_private_addr(addr: IpAddr) -> bool {
    matches!(
        locality(addr),
        Locality::Loopback | Locality::LinkLocal | Locality::UniqueLocal | Locality::Rfc1918
    )
}

/// Returns `true` for the addresses CretLink will bind mDNS to: the private
/// LAN families (RFC1918, link-local, ULA) but **not** loopback and **not**
/// the unspecified address.
pub(crate) fn is_advertisable_addr(addr: IpAddr) -> bool {
    matches!(
        locality(addr),
        Locality::LinkLocal | Locality::UniqueLocal | Locality::Rfc1918
    )
}

/// Returns `true` for an IPv4/IPv6 link-local address (`169.254.0.0/16`,
/// `fe80::/10`). Used to deprioritize automatically-assigned link-local
/// addresses in favour of a routable-private one (ST-M002-010).
fn is_link_local(addr: IpAddr) -> bool {
    matches!(locality(addr), Locality::LinkLocal)
}

/// Orders an advertisable-address set by reachability preference and drops
/// link-local addresses whenever any routable-private address is present.
///
/// The input is already filtered to the advertisable families (RFC1918, ULA,
/// link-local), so "not link-local" means "routable-private". A host that has a
/// routable-private address (RFC1918 or IPv6 ULA) on one interface and a stray
/// link-local — an APIPA `169.254.x` lease, or an `fe80::` from an interface
/// without DHCP — on another **must** advertise and bind the routable one: the
/// customer agent can only reach the console over a routable address, and a raw
/// numeric sort would otherwise pick `169.254` ahead of `192.168` (or `10.x`)
/// (ST-M002-010). Because the
/// advertiser publishes this whole set as the service's address records, a
/// surviving link-local entry would also mislead a browsing agent into dialling
/// an address nothing listens on, so link-local is *removed*, not merely sorted
/// last, when a routable option exists.
///
/// Link-local is kept **only** when it is the sole option — a direct cable or a
/// no-DHCP LAN, where `169.254`/`fe80::` is the genuine and REQ-S-010-permitted
/// reachable address. Within the surviving tier the order is the numeric
/// address order, so binding stays deterministic and reproducible. The explicit
/// `--bind`/manual override remains the escape hatch for the rare host that must
/// serve a link-local interface while a routable one is also present.
pub(crate) fn prefer_routable_private(mut addrs: Vec<IpAddr>) -> Vec<IpAddr> {
    addrs.sort_unstable();
    addrs.dedup();
    if addrs.iter().any(|ip| !is_link_local(*ip)) {
        addrs.retain(|ip| !is_link_local(*ip));
    }
    addrs
}

/// Enumerates the host's interface addresses and returns only those safe to
/// advertise on or browse from under REQ-S-010: RFC1918, link-local, or ULA.
///
/// Loopback, unspecified, and globally routable addresses are always excluded,
/// so the result never contains `0.0.0.0`, `::`, or any public address. The
/// result is preference-ordered by `prefer_routable_private`: routable-private
/// addresses (RFC1918, IPv6 ULA) come first and link-local (`169.254`/`fe80::`)
/// is dropped entirely whenever a routable one exists, kept only as the sole
/// option on a direct-cable / no-DHCP LAN. Within a tier the order is numeric,
/// so binding stays deterministic and reproducible (the acceptance criterion
/// that interface choice is visible or deterministic and documented, refined by
/// ST-M002-010 so the deterministic choice is also the reachable one). An empty
/// result is legitimate — it means the host has no private LAN address — and the
/// advertiser/browser turn that into [`DiscoveryError::NoPrivateInterface`] so
/// the caller can offer the manual connection code instead.
///
/// # Errors
///
/// Returns [`DiscoveryError::Interfaces`] if the OS interface list cannot be
/// read.
pub fn private_interface_addrs() -> Result<Vec<IpAddr>, DiscoveryError> {
    let interfaces = if_addrs::get_if_addrs().map_err(DiscoveryError::Interfaces)?;
    let addrs: Vec<IpAddr> = interfaces
        .into_iter()
        .filter(|iface| !iface.is_loopback())
        .map(|iface| iface.ip())
        .filter(|ip| is_advertisable_addr(*ip))
        .collect();
    Ok(prefer_routable_private(addrs))
}

#[cfg(test)]
pub(crate) use self::is_advertisable_addr as advertisable_for_tests;
#[cfg(test)]
pub(crate) use self::prefer_routable_private as prefer_routable_private_for_tests;
