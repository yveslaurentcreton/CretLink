//! The addressing-only rendezvous ticket (ST-M007-002, ADR-0016).
//!
//! A ticket names *which* pending pairing two endpoints want to join at the
//! relay. Like the LAN connection code (ADR-0007) it is **addressing only**:
//! it grants no authority and carries no session key. Session authenticity comes
//! entirely from the end-to-end Noise handshake and the human-compared Session
//! Approval Code (ADR-0003); presenting a wrong or guessed ticket only reaches
//! the wrong peer, and the SAC comparison then fails closed (REQ-S-006).
//!
//! It is 128 bits of OS randomness so it is infeasible to guess within its short
//! time-to-live, single-use window. It is compared in constant time and erased
//! on drop, and its [`Debug`] representation is redacted so it never lands in a
//! log (`Keep target and session identity in results and logs` — but not the
//! rendezvous secret itself).
//!

use core::fmt;
use core::hash::{Hash, Hasher};
use std::net::IpAddr;

use hkdf::Hkdf;
use rand::TryRngCore;
use rand::rngs::OsRng;
use sha2::Sha256;
use subtle::ConstantTimeEq;
use thiserror::Error;
use zeroize::Zeroize;

/// Length in bytes of the rendezvous ticket (128 bits).
pub const RENDEZVOUS_TICKET_LEN: usize = 16;

/// Length in bytes of the rendezvous seed that a relay code carries (64 bits,
/// ST-M007-012).
pub const RENDEZVOUS_SEED_LEN: usize = 8;

/// The longest host name a relay may have: the limit of a DNS name.
pub const MAX_RELAY_HOST_LEN: usize = 253;

/// The fixed HKDF salt of [`RendezvousTicket::derive`]. FROZEN: changing it
/// changes every ticket, so two releases would no longer meet.
const DERIVE_SALT: &[u8] = b"CretLink relay rendezvous ticket v1";

/// A single-use, short-TTL rendezvous ticket. See the [module docs](self).
#[derive(Clone)]
pub struct RendezvousTicket([u8; RENDEZVOUS_TICKET_LEN]);

/// A ticket generation or parsing failure.
#[derive(Debug, Error)]
pub enum TicketError {
    /// The operating-system randomness source failed; no ticket was produced.
    #[error("failed to draw ticket randomness from the OS CSPRNG")]
    Rng,
    /// A parsed ticket was not exactly [`RENDEZVOUS_TICKET_LEN`] bytes.
    #[error("rendezvous ticket must be {RENDEZVOUS_TICKET_LEN} bytes")]
    BadLength,
    /// A parsed ticket string contained a non-hex character.
    #[error("rendezvous ticket contained a non-hexadecimal character")]
    BadHex,
    /// The relay's host name cannot be used to derive a ticket: it is empty,
    /// longer than a DNS name may be, or holds something other than letters,
    /// digits, hyphens, underscores and dots (or it is no IP address).
    #[error("the relay's host name is not a plain host name or an IP address")]
    BadHost,
}

impl RendezvousTicket {
    /// Draws a fresh ticket from the OS CSPRNG.
    ///
    /// # Errors
    ///
    /// Returns [`TicketError::Rng`] if the OS randomness source fails.
    pub fn generate() -> Result<Self, TicketError> {
        let mut bytes = [0u8; RENDEZVOUS_TICKET_LEN];
        OsRng
            .try_fill_bytes(&mut bytes)
            .map_err(|_| TicketError::Rng)?;
        Ok(Self(bytes))
    }

    /// Derives the ticket of a session that is found through a relay code
    /// (ST-M007-012, ADR-0016 §2).
    ///
    /// The technician reads the code aloud; both copies hold its `seed` and
    /// name the same relay in their relay profile. Each computes
    ///
    /// ```text
    /// ticket = HKDF-SHA256(salt = "CretLink relay rendezvous ticket v1",
    ///                      ikm  = seed,
    ///                      info = the canonical host name of the relay)[..16]
    /// ```
    ///
    /// and presents the result to the relay. The host name is part of the
    /// derivation so that two copies with different relays never meet, and so
    /// that one seed names unrelated rendezvous on unrelated relays. See
    /// [`canonical_relay_host`] for the form of the host name.
    ///
    /// The ticket stays addressing only: it is derived from what was read
    /// aloud and grants nothing (REQ-S-006).
    ///
    /// # Errors
    ///
    /// Returns [`TicketError::BadHost`] if `relay_host` has no canonical form.
    pub fn derive(seed: &[u8; RENDEZVOUS_SEED_LEN], relay_host: &str) -> Result<Self, TicketError> {
        let host = canonical_relay_host(relay_host)?;
        let mut bytes = [0u8; RENDEZVOUS_TICKET_LEN];
        Hkdf::<Sha256>::new(Some(DERIVE_SALT), seed)
            .expand(host.as_bytes(), &mut bytes)
            // Sixteen bytes are far below the HKDF output limit.
            .map_err(|_| TicketError::BadHost)?;
        Ok(Self(bytes))
    }

    /// Wraps raw ticket bytes (for reconstructing a ticket received on the wire).
    #[must_use]
    pub fn from_bytes(bytes: [u8; RENDEZVOUS_TICKET_LEN]) -> Self {
        Self(bytes)
    }

    /// The raw ticket bytes.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8; RENDEZVOUS_TICKET_LEN] {
        &self.0
    }

    /// Renders the ticket as lowercase hex grouped in four-character blocks, a
    /// stable transport-neutral form. The final human-facing code layout (a
    /// relay variant of the ADR-0007 connection code) is frozen in later
    /// operational work; this is the addressing token it will carry.
    #[must_use]
    pub fn to_hex(&self) -> String {
        let mut out = String::with_capacity(RENDEZVOUS_TICKET_LEN * 2 + RENDEZVOUS_TICKET_LEN / 2);
        for (i, byte) in self.0.iter().enumerate() {
            if i != 0 && i % 2 == 0 {
                out.push('-');
            }
            out.push(nibble_to_hex(byte >> 4));
            out.push(nibble_to_hex(byte & 0x0f));
        }
        out
    }

    /// Parses a ticket rendered by [`to_hex`](Self::to_hex). Group separators
    /// (`-`) and surrounding whitespace are ignored.
    ///
    /// # Errors
    ///
    /// Returns [`TicketError::BadHex`] on a non-hex character or
    /// [`TicketError::BadLength`] if the decoded length is wrong.
    pub fn parse_hex(text: &str) -> Result<Self, TicketError> {
        let mut bytes = [0u8; RENDEZVOUS_TICKET_LEN];
        let mut nibbles = text
            .chars()
            .filter(|c| *c != '-' && !c.is_whitespace())
            .map(hex_to_nibble);
        for slot in &mut bytes {
            let hi = nibbles.next().ok_or(TicketError::BadLength)??;
            let lo = nibbles.next().ok_or(TicketError::BadLength)??;
            *slot = (hi << 4) | lo;
        }
        if nibbles.next().is_some() {
            return Err(TicketError::BadLength);
        }
        Ok(Self(bytes))
    }
}

impl Drop for RendezvousTicket {
    fn drop(&mut self) {
        self.0.zeroize();
    }
}

/// Constant-time equality: no early exit reveals how many leading bytes matched.
impl PartialEq for RendezvousTicket {
    fn eq(&self, other: &Self) -> bool {
        self.0.ct_eq(&other.0).into()
    }
}

impl Eq for RendezvousTicket {}

impl Hash for RendezvousTicket {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.0.hash(state);
    }
}

/// Redacted: the rendezvous secret must never reach a log.
impl fmt::Debug for RendezvousTicket {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("RendezvousTicket(<redacted>)")
    }
}

/// The form of a relay's host name that both copies of a session agree on,
/// whatever the spelling in their relay profile (ST-M007-012).
///
/// - An IP address, with or without the square brackets of an IPv6 literal,
///   becomes its standard text form.
/// - A host name loses surrounding whitespace and one trailing dot and is
///   lowercased. It may hold ASCII letters, digits, hyphens, underscores and
///   dots, in labels that are not empty, up to [`MAX_RELAY_HOST_LEN`] bytes.
///   An internationalised name is written in its ASCII (`xn--`) form.
///
/// The port is not part of it: a relay is named by its host.
///
/// # Errors
///
/// Returns [`TicketError::BadHost`] for anything else.
pub fn canonical_relay_host(host: &str) -> Result<String, TicketError> {
    let trimmed = host.trim();
    let literal = trimmed
        .strip_prefix('[')
        .and_then(|rest| rest.strip_suffix(']'))
        .unwrap_or(trimmed);
    if let Ok(address) = literal.parse::<IpAddr>() {
        return Ok(address.to_string());
    }

    let name = trimmed.strip_suffix('.').unwrap_or(trimmed);
    let well_formed = !name.is_empty()
        && name.len() <= MAX_RELAY_HOST_LEN
        && name.split('.').all(|label| {
            !label.is_empty()
                && label
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
        });
    if well_formed {
        Ok(name.to_ascii_lowercase())
    } else {
        Err(TicketError::BadHost)
    }
}

fn nibble_to_hex(n: u8) -> char {
    match n {
        0..=9 => (b'0' + n) as char,
        _ => (b'a' + (n - 10)) as char,
    }
}

fn hex_to_nibble(c: char) -> Result<u8, TicketError> {
    match c {
        '0'..='9' => Ok(c as u8 - b'0'),
        'a'..='f' => Ok(c as u8 - b'a' + 10),
        'A'..='F' => Ok(c as u8 - b'A' + 10),
        _ => Err(TicketError::BadHex),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    #[test]
    fn generated_tickets_differ() {
        let a = RendezvousTicket::generate().unwrap();
        let b = RendezvousTicket::generate().unwrap();
        assert_ne!(a, b, "128-bit tickets must not collide in practice");
    }

    #[test]
    fn hex_round_trips() {
        let ticket = RendezvousTicket::from_bytes([
            0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xaa, 0xbb, 0xcc, 0xdd,
            0xee, 0xff,
        ]);
        let text = ticket.to_hex();
        assert_eq!(text, "0011-2233-4455-6677-8899-aabb-ccdd-eeff");
        let parsed = RendezvousTicket::parse_hex(&text).unwrap();
        assert_eq!(parsed, ticket);
    }

    #[test]
    fn parse_ignores_whitespace_and_accepts_uppercase() {
        let ticket = RendezvousTicket::from_bytes([0xab; RENDEZVOUS_TICKET_LEN]);
        let parsed = RendezvousTicket::parse_hex("  ABABABAB ABABABAB ABABABAB ABABABAB ").unwrap();
        assert_eq!(parsed, ticket);
    }

    #[test]
    fn parse_rejects_wrong_length() {
        assert!(matches!(
            RendezvousTicket::parse_hex("0011"),
            Err(TicketError::BadLength)
        ));
        assert!(matches!(
            RendezvousTicket::parse_hex("0011-2233-4455-6677-8899-aabb-ccdd-eeff-0000"),
            Err(TicketError::BadLength)
        ));
    }

    #[test]
    fn parse_rejects_non_hex() {
        assert!(matches!(
            RendezvousTicket::parse_hex("zz11-2233-4455-6677-8899-aabb-ccdd-eeff"),
            Err(TicketError::BadHex)
        ));
    }

    #[test]
    fn debug_is_redacted() {
        let ticket = RendezvousTicket::from_bytes([0x42; RENDEZVOUS_TICKET_LEN]);
        assert_eq!(format!("{ticket:?}"), "RendezvousTicket(<redacted>)");
    }

    const SEED: [u8; RENDEZVOUS_SEED_LEN] = [0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07];

    fn hex(ticket: &RendezvousTicket) -> String {
        ticket.to_hex().replace('-', "")
    }

    #[test]
    fn derivation_matches_the_frozen_vectors() {
        // FROZEN: changing one of these means two releases no longer meet on a
        // relay. Derived independently with HMAC-SHA256 (RFC 5869).
        let vectors: [([u8; RENDEZVOUS_SEED_LEN], &str, &str); 5] = [
            (
                SEED,
                "relay.example.net",
                "5efba570f26dae7f22a75969ccf6750f",
            ),
            (
                [0x8f, 0x3a, 0x51, 0xc2, 0xd9, 0x4e, 0x7b, 0x06],
                "relay.example.net",
                "4df77c5bbc267507a67de585da58cf4b",
            ),
            (
                SEED,
                "other.example.net",
                "13c5494836a365f7d1cface153f09c9b",
            ),
            (SEED, "2001:db8::1", "1c4ed9247c9a119ed6982a812f0c20a7"),
            (SEED, "192.0.2.1", "48761826855a1b280a6da518a470f0b3"),
        ];
        for (seed, host, expected) in vectors {
            let ticket = RendezvousTicket::derive(&seed, host).unwrap();
            assert_eq!(hex(&ticket), expected, "{host}");
        }
    }

    #[test]
    fn both_copies_derive_the_same_ticket_from_different_spellings() {
        let expected = RendezvousTicket::derive(&SEED, "relay.example.net").unwrap();
        for spelling in [
            "Relay.Example.NET",
            "relay.example.net.",
            "  relay.example.net  ",
        ] {
            assert_eq!(
                RendezvousTicket::derive(&SEED, spelling).unwrap(),
                expected,
                "{spelling:?}"
            );
        }
        let v6 = RendezvousTicket::derive(&SEED, "2001:db8::1").unwrap();
        for spelling in ["[2001:db8::1]", "2001:DB8::1", "2001:0db8:0:0:0:0:0:1"] {
            assert_eq!(
                RendezvousTicket::derive(&SEED, spelling).unwrap(),
                v6,
                "{spelling:?}"
            );
        }
    }

    #[test]
    fn another_relay_or_another_seed_gives_another_ticket() {
        let ticket = RendezvousTicket::derive(&SEED, "relay.example.net").unwrap();
        // Two copies with different relays never meet.
        assert_ne!(
            ticket,
            RendezvousTicket::derive(&SEED, "other.example.net").unwrap()
        );
        // A sub-domain is another relay.
        assert_ne!(
            ticket,
            RendezvousTicket::derive(&SEED, "eu.relay.example.net").unwrap()
        );
        let mut other_seed = SEED;
        other_seed[7] ^= 1;
        assert_ne!(
            ticket,
            RendezvousTicket::derive(&other_seed, "relay.example.net").unwrap()
        );
    }

    #[test]
    fn a_host_without_a_canonical_form_is_refused() {
        let too_long = format!("{}.example.net", "a".repeat(MAX_RELAY_HOST_LEN));
        for host in [
            "",
            "   ",
            ".",
            "relay..example.net",
            ".relay.example.net",
            "relay example.net",
            "relay.example.net:443",
            "r\u{e9}lay.example.net",
            "relay.example.net/path",
            too_long.as_str(),
        ] {
            assert!(
                matches!(
                    RendezvousTicket::derive(&SEED, host),
                    Err(TicketError::BadHost)
                ),
                "{host:?}"
            );
        }
        // The longest name that is still a DNS name is accepted.
        let longest = format!("{}.{}", "a".repeat(63), "b".repeat(MAX_RELAY_HOST_LEN - 64));
        assert_eq!(longest.len(), MAX_RELAY_HOST_LEN);
        assert!(canonical_relay_host(&longest).is_ok());
    }
}
