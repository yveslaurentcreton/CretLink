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

use rand::TryRngCore;
use rand::rngs::OsRng;
use subtle::ConstantTimeEq;
use thiserror::Error;
use zeroize::Zeroize;

/// Length in bytes of the rendezvous ticket (128 bits).
pub const RENDEZVOUS_TICKET_LEN: usize = 16;

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
}
