//! The network the technician waits on (ST-M009-016).
//!
//! A technician's computer is often on several private networks: the local
//! network, a VPN, the virtual switch of a hypervisor. The customer can reach
//! only one of them. The waiting screen therefore names the adapter and the
//! address the session waits on and offers the other candidates
//! ([`get_listen_networks`]); `switch_listen_network` in the parent module
//! moves the listener.
//!
//! Which address is taken without a choice is decided by
//! [`cretlink_discovery::first_choice`]. A choice of the technician holds for
//! the later sessions of this run ([`NetworkChoice`]) and is kept in memory
//! only: nothing is written to disk.

use std::net::IpAddr;
use std::sync::{Arc, Mutex, PoisonError};

use cretlink_discovery::{ListenCandidate, first_choice, listen_candidates};
use serde::Serialize;

/// One network the technician can wait on, as the waiting screen lists it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ListenNetwork {
    /// The private address, without a port.
    pub address: String,
    /// The name of the network adapter as the operating system gives it.
    pub adapter: String,
    /// A point-to-point link, as many VPN tunnels are.
    pub point_to_point: bool,
}

impl From<&ListenCandidate> for ListenNetwork {
    fn from(candidate: &ListenCandidate) -> Self {
        Self {
            address: candidate.addr.to_string(),
            adapter: candidate.adapter.clone(),
            point_to_point: candidate.point_to_point,
        }
    }
}

/// The adapter the technician chose earlier in this run, by name.
#[derive(Debug, Clone, Default)]
pub(crate) struct NetworkChoice(Arc<Mutex<Option<String>>>);

impl NetworkChoice {
    /// The name of the chosen adapter, if the technician chose one.
    pub(crate) fn get(&self) -> Option<String> {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Remembers the adapter for the later sessions of this run.
    pub(crate) fn set(&self, adapter: &str) {
        *self.0.lock().unwrap_or_else(PoisonError::into_inner) = Some(adapter.to_string());
    }
}

/// The candidate to wait on: the adapter chosen earlier in this run while it
/// still has a candidate address, and otherwise the first choice of the
/// discovery crate.
pub(crate) fn pick<'a>(
    candidates: &'a [ListenCandidate],
    remembered: Option<&str>,
    default_route: Option<IpAddr>,
) -> Option<&'a ListenCandidate> {
    remembered
        .and_then(|adapter| candidates.iter().find(|c| c.adapter == adapter))
        .or_else(|| first_choice(candidates, default_route))
}

/// The candidate that has `address`.
///
/// The window can name only an address that this computer offered: anything
/// else is refused, so a request can never make the listener bind a loopback,
/// unspecified or public address (REQ-S-010).
pub(crate) fn requested<'a>(
    candidates: &'a [ListenCandidate],
    address: &str,
) -> Result<&'a ListenCandidate, String> {
    let wanted: IpAddr = address
        .trim()
        .parse()
        .map_err(|_| "that is not a network address".to_string())?;
    candidates.iter().find(|c| c.addr == wanted).ok_or_else(|| {
        "this computer no longer has that address; choose one of the networks listed".to_string()
    })
}

/// The networks this computer can wait on right now, in the order the
/// waiting screen lists them (ST-M009-016). Empty when it has no private
/// address or the adapters cannot be read.
#[tauri::command]
pub(crate) fn get_listen_networks() -> Vec<ListenNetwork> {
    listen_candidates()
        .unwrap_or_default()
        .iter()
        .map(ListenNetwork::from)
        .collect()
}

#[cfg(test)]
mod tests {
    use std::net::Ipv4Addr;

    use super::*;

    fn candidate(addr: [u8; 4], adapter: &str, point_to_point: bool) -> ListenCandidate {
        ListenCandidate {
            addr: IpAddr::V4(Ipv4Addr::from(addr)),
            adapter: adapter.to_string(),
            point_to_point,
        }
    }

    /// A computer on a VPN, a virtual switch and the local network, in the
    /// numeric order the discovery crate returns.
    fn three_networks() -> Vec<ListenCandidate> {
        vec![
            candidate([10, 0, 0, 5], "VPN", false),
            candidate([172, 20, 0, 1], "vEthernet (Default Switch)", false),
            candidate([192, 168, 1, 20], "Wi-Fi", false),
        ]
    }

    fn ip(addr: [u8; 4]) -> Option<IpAddr> {
        Some(IpAddr::V4(Ipv4Addr::from(addr)))
    }

    #[test]
    fn without_a_choice_the_first_choice_of_discovery_is_taken() {
        let networks = three_networks();
        let chosen = pick(&networks, None, ip([192, 168, 1, 20])).unwrap();
        assert_eq!(chosen.adapter, "Wi-Fi");
    }

    #[test]
    fn an_earlier_choice_of_this_run_wins_from_the_default_route() {
        let networks = three_networks();
        // The technician chose the VPN on purpose earlier in this run.
        let chosen = pick(&networks, Some("VPN"), ip([192, 168, 1, 20])).unwrap();
        assert_eq!(chosen.adapter, "VPN");
    }

    #[test]
    fn an_earlier_choice_follows_its_adapter_to_a_new_address() {
        // The same Wi-Fi adapter on another network, with another address.
        let moved = vec![
            candidate([10, 0, 0, 5], "VPN", false),
            candidate([192, 168, 1, 10], "Wi-Fi", false),
        ];
        let chosen = pick(&moved, Some("Wi-Fi"), ip([10, 0, 0, 5])).unwrap();
        assert_eq!(chosen.addr, IpAddr::V4(Ipv4Addr::new(192, 168, 1, 10)));
    }

    #[test]
    fn an_earlier_choice_that_is_gone_falls_back_to_the_first_choice() {
        // The VPN was disconnected since: its adapter has no candidate address.
        let without_vpn = vec![
            candidate([172, 20, 0, 1], "vEthernet (Default Switch)", false),
            candidate([192, 168, 1, 20], "Wi-Fi", false),
        ];
        let chosen = pick(&without_vpn, Some("VPN"), ip([192, 168, 1, 20])).unwrap();
        assert_eq!(chosen.adapter, "Wi-Fi");
        assert!(pick(&[], Some("VPN"), None).is_none());
    }

    #[test]
    fn a_requested_address_must_be_one_this_computer_offered() {
        let networks = three_networks();
        assert_eq!(
            requested(&networks, "192.168.1.20").unwrap().adapter,
            "Wi-Fi"
        );
        assert_eq!(requested(&networks, " 10.0.0.5 ").unwrap().adapter, "VPN");
        // Not a candidate: another private address, loopback, the unspecified
        // address and a public one are all refused (REQ-S-010).
        for other in ["192.168.1.10", "127.0.0.1", "0.0.0.0", "192.0.2.1", "::1"] {
            assert!(
                requested(&networks, other)
                    .unwrap_err()
                    .contains("no longer has that address"),
                "{other}"
            );
        }
        for junk in ["", "Wi-Fi", "192.168.1.20:7000", "192.168.1"] {
            assert_eq!(
                requested(&networks, junk).unwrap_err(),
                "that is not a network address",
                "{junk:?}"
            );
        }
    }

    #[test]
    fn a_network_is_listed_by_adapter_and_address_without_a_port() {
        let listed = ListenNetwork::from(&candidate([192, 168, 1, 20], "Wi-Fi", false));
        assert_eq!(
            serde_json::to_value(&listed).unwrap(),
            serde_json::json!({
                "address": "192.168.1.20",
                "adapter": "Wi-Fi",
                "pointToPoint": false
            })
        );
    }

    #[test]
    fn the_choice_is_remembered_for_this_run() {
        let choice = NetworkChoice::default();
        assert_eq!(choice.get(), None);
        choice.set("Wi-Fi");
        assert_eq!(choice.get().as_deref(), Some("Wi-Fi"));
        // A clone shares the choice, as the application state does.
        choice.clone().set("VPN");
        assert_eq!(choice.get().as_deref(), Some("VPN"));
    }
}
