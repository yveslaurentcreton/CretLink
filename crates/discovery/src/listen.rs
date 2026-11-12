//! The addresses a console can wait on, and which one it takes first
//! (ST-M009-016, REQ-S-010).
//!
//! A computer is often on several private networks at once: the local
//! network, a VPN, the virtual switch of a hypervisor. The customer can reach
//! only one of them, so the console must wait on that one.
//! [`listen_candidates`] lists the private addresses with the name of their
//! network adapter, so that the technician can see and change the choice, and
//! [`first_choice`] says which one to take when nothing was chosen.
//!
//! The candidates follow the rule of ST-M002-010: routable private addresses,
//! and link-local ones only when nothing else exists. Loopback, unspecified
//! and public addresses are never candidates.

use std::net::{IpAddr, Ipv4Addr, UdpSocket};

use if_addrs::IfOperStatus;

use crate::error::DiscoveryError;
use crate::interface::{is_advertisable_addr, is_link_local};

/// One private address this computer could wait on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListenCandidate {
    /// The address: RFC 1918, a unique local address or, only when nothing
    /// else exists, a link-local one.
    pub addr: IpAddr,
    /// The name of the network adapter as the operating system gives it, such
    /// as `Wi-Fi` or `en0`. It is shown to the technician; it is no identity.
    pub adapter: String,
    /// The adapter is a point-to-point link, as many VPN tunnels are.
    pub point_to_point: bool,
}

/// Lists the private addresses of this computer's network adapters that are
/// up, in the order of ST-M002-010.
///
/// An empty list is legitimate: the computer has no private LAN address, and
/// the caller says so instead of waiting on a public or unspecified address
/// (REQ-S-010).
///
/// # Errors
///
/// Returns [`DiscoveryError::Interfaces`] if the operating system's list of
/// adapters cannot be read.
pub fn listen_candidates() -> Result<Vec<ListenCandidate>, DiscoveryError> {
    let interfaces = if_addrs::get_if_addrs().map_err(DiscoveryError::Interfaces)?;
    Ok(order_candidates(
        interfaces
            .into_iter()
            .filter(|iface| !iface.is_loopback() && may_be_up(&iface.oper_status))
            .map(|iface| ListenCandidate {
                addr: iface.ip(),
                point_to_point: iface.is_p2p,
                adapter: iface.name,
            })
            .filter(|candidate| is_advertisable_addr(candidate.addr))
            .collect(),
    ))
}

/// An adapter that is up, or whose state the operating system does not tell.
/// One that is down, absent or waiting cannot carry a session.
fn may_be_up(status: &IfOperStatus) -> bool {
    matches!(status, IfOperStatus::Up | IfOperStatus::Unknown)
}

/// Puts candidates in the order of ST-M002-010: numerically, one entry for
/// each address, and link-local addresses only when nothing else exists.
pub(crate) fn order_candidates(mut candidates: Vec<ListenCandidate>) -> Vec<ListenCandidate> {
    candidates.sort_by(|a, b| a.addr.cmp(&b.addr).then_with(|| a.adapter.cmp(&b.adapter)));
    candidates.dedup_by(|a, b| a.addr == b.addr);
    if candidates.iter().any(|c| !is_link_local(c.addr)) {
        candidates.retain(|c| !is_link_local(c.addr));
    }
    candidates
}

/// The candidate to wait on when the technician has not chosen
/// (ST-M009-016).
///
/// The numeric order alone puts a VPN adapter (`10.x`) or a virtual switch
/// (`172.16.x`) ahead of the local network (`192.168.x`), which the customer
/// is on. The first choice is therefore the address this computer uses to
/// reach other networks (`default_route`, see [`default_route_addr`]), unless
/// that adapter is a point-to-point link. Otherwise it is the first candidate
/// that is not a point-to-point link, and otherwise the first candidate.
/// The result depends only on the arguments, so the choice is deterministic.
#[must_use]
pub fn first_choice(
    candidates: &[ListenCandidate],
    default_route: Option<IpAddr>,
) -> Option<&ListenCandidate> {
    default_route
        .and_then(|route| candidates.iter().find(|c| c.addr == route))
        .filter(|c| !c.point_to_point)
        .or_else(|| candidates.iter().find(|c| !c.point_to_point))
        .or_else(|| candidates.first())
}

/// The local address this computer uses to reach other networks: the source
/// address of its default route, when it has one.
///
/// Connecting a datagram socket to an address only asks the operating system
/// which route and local address it would use. Nothing is sent, the socket is
/// never read and it is closed at once, so this is no listener (REQ-S-010).
/// The address asked for is one reserved for documentation (RFC 5737), which
/// belongs to no network.
#[must_use]
pub fn default_route_addr() -> Option<IpAddr> {
    const ELSEWHERE: (Ipv4Addr, u16) = (Ipv4Addr::new(192, 0, 2, 1), 9);
    let socket = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).ok()?;
    socket.connect(ELSEWHERE).ok()?;
    let local = socket.local_addr().ok()?.ip();
    (!local.is_unspecified()).then_some(local)
}

#[cfg(test)]
pub(crate) use self::order_candidates as order_candidates_for_tests;
