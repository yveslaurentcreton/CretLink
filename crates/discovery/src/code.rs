//! The manual connection code and raw `host:port` fallback (REQ-F-004).
//!
//! When multicast discovery is blocked, the technician reads a short
//! **connection code** off the console screen and types it into the agent, or
//! (as a documented debugging escape hatch) types a raw `host:port` /
//! `[ipv6]:port`. Neither path grants any authority: the result is an
//! addressing hint that must still go through pairing and the Session Approval
//! Code before any control is possible (ADR-0007).
//!
//! # Code format (v1)
//!
//! A 60-bit payload, shown as **12 Crockford Base32 characters** grouped
//! `XXXX-XXXX-XXXX`:
//!
//! | bits | field |
//! |------|-------|
//! | 4    | format version (`1`) |
//! | 32   | IPv4 address (big-endian) |
//! | 16   | TCP port (big-endian) |
//! | 8    | CRC-8 over the first 52 bits |
//!
//! The Crockford Base32 alphabet excludes `I L O U`; decoding is
//! case-insensitive and maps `I`/`L` to `1` and `O` to `0`. Hyphens and ASCII
//! whitespace are ignored on input.
//!
//! ## CRC
//!
//! CRC-8 with polynomial `0x07` (`x^8 + x^2 + x + 1`), initial value `0x00`,
//! no input/output reflection, no final XOR, computed bit-by-bit MSB-first
//! over the 52-bit `[version:4][ipv4:32][port:16]` payload. It catches all
//! single- and double-bit transcription errors over this length. It gives
//! roughly 1-in-256 odds of an *undetected* random error, so it is a
//! transcription check, **not** a guarantee — connecting to a wrong address
//! still grants nothing before the Session Approval Code is compared.
//!
//! v1 encodes IPv4 only; IPv6-only LANs use the raw `[ipv6]:port` path. The
//! version nibble tells the formats of the family apart: version 2 is the
//! sixteen-character relay code ([`crate::RelayCode`], ST-M007-012), and a
//! well-formed twelve-character code of another version is rejected distinctly
//! as [`ManualEntryError::UnsupportedVersion`]. [`parse_entry`] reads either
//! format from one input field.

use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};

use thiserror::Error;

use crate::relay_code::{RelayCode, has_relay_code_length};

/// Crockford Base32 alphabet (excludes `I`, `L`, `O`, `U`).
pub(crate) const ALPHABET: [u8; 32] = *b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";

/// Number of Base32 characters in a connection code (excluding grouping).
const CODE_LEN: usize = 12;

/// How a [`ConnectionTarget`] was obtained. Recorded for logging and UI, and a
/// standing reminder that neither source conveys authority.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TargetSource {
    /// Parsed from a CretLink connection code of the given format version.
    ConnectionCode {
        /// The 4-bit format version encoded in the code (currently always
        /// [`ConnectionCode::FORMAT_VERSION`]).
        format_version: u8,
    },
    /// Parsed from a raw `host:port` / `[ipv6]:port` entry (the debugging
    /// escape hatch).
    HostPort,
}

/// A network address a manual entry resolved to.
///
/// **This is an addressing hint only.** Producing a `ConnectionTarget`
/// performs no I/O, opens no connection, and authenticates nothing; it grants
/// no authority whatsoever. All trust is established later by pairing and the
/// Session Approval Code (REQ-F-005, REQ-S-006).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConnectionTarget {
    addr: SocketAddr,
    source: TargetSource,
}

impl ConnectionTarget {
    /// The socket address to attempt pairing against. Not yet trusted.
    #[must_use]
    pub const fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// How this target was entered.
    #[must_use]
    pub const fn source(&self) -> TargetSource {
        self.source
    }
}

/// Why a manual entry could not be parsed.
///
/// Every variant identifies the failed stage and implies a safe next action
/// without exposing secrets (REQ-N-015). A [`ChecksumMismatch`] is reported
/// **before** any connection attempt.
///
/// [`ChecksumMismatch`]: ManualEntryError::ChecksumMismatch
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ManualEntryError {
    /// The code did not have the length of its format once grouping hyphens
    /// and whitespace are removed: 12 Base32 characters for a connection code
    /// of the LAN, 16 for a relay code.
    #[error("a connection code has 12 characters and a relay code has 16")]
    BadLength,

    /// The code contained a character outside the Crockford Base32 alphabet.
    #[error("connection code contains an invalid character")]
    BadChar,

    /// The code's checksum did not match its contents: a transcription error.
    /// Rejected before any connection is attempted.
    #[error("connection code checksum did not match; re-check for a typo")]
    ChecksumMismatch,

    /// The code is well-formed and its checksum is valid, but it declares a
    /// format version this build does not understand (for example a future v2
    /// IPv6 code). Use a raw `host:port` entry instead.
    #[error("connection code format version is not supported by this build")]
    UnsupportedVersion,

    /// A raw manual address could not be parsed as `host:port` or
    /// `[ipv6]:port`. The host must be an IP literal with an explicit port;
    /// name resolution is out of scope for this offline parser.
    #[error("manual address must be an IP literal 'host:port' or '[ipv6]:port'")]
    BadHostPort,
}

/// The CretLink connection-code codec (see the module documentation for the
/// wire format).
///
/// This is a stateless codec namespace, not a trusted peer: decoding a code
/// yields a [`ConnectionTarget`] addressing hint and nothing more.
#[derive(Debug, Clone, Copy)]
pub struct ConnectionCode;

impl ConnectionCode {
    /// The only format version this build encodes and accepts.
    pub const FORMAT_VERSION: u8 = 1;

    /// Encodes an IPv4 socket address as a grouped 12-character connection
    /// code, `XXXX-XXXX-XXXX`.
    #[must_use]
    pub fn encode(addr: SocketAddrV4) -> String {
        let ipv4 = u32::from(*addr.ip());
        let port = addr.port();
        let payload = payload52(Self::FORMAT_VERSION, ipv4, port);
        let value = (payload << 8) | u64::from(crc8(payload));
        encode_symbols(value)
    }

    /// Parses a connection code (strictly — not a raw `host:port`; use
    /// [`parse_manual_entry`] for that).
    ///
    /// Grouping hyphens and ASCII whitespace are ignored. The checksum is
    /// verified before the version, so a corrupted code is reported as
    /// [`ManualEntryError::ChecksumMismatch`] rather than misread as a
    /// different version.
    ///
    /// # Errors
    ///
    /// See [`ManualEntryError`] (`BadLength`, `BadChar`, `ChecksumMismatch`,
    /// `UnsupportedVersion`).
    pub fn parse(input: &str) -> Result<ConnectionTarget, ManualEntryError> {
        let mut value: u64 = 0;
        let mut count: usize = 0;
        for ch in input.chars() {
            if ch == '-' || ch.is_ascii_whitespace() {
                continue;
            }
            count += 1;
            if count > CODE_LEN {
                return Err(ManualEntryError::BadLength);
            }
            let symbol = decode_symbol(ch).ok_or(ManualEntryError::BadChar)?;
            value = (value << 5) | u64::from(symbol);
        }
        if count != CODE_LEN {
            return Err(ManualEntryError::BadLength);
        }

        // `value` now holds exactly 60 significant bits.
        let payload = value >> 8; // top 52 bits: [version:4][ipv4:32][port:16]
        let stored_crc = (value & 0xff) as u8;
        if crc8(payload) != stored_crc {
            return Err(ManualEntryError::ChecksumMismatch);
        }

        let version = ((value >> 56) & 0x0f) as u8;
        if version != Self::FORMAT_VERSION {
            return Err(ManualEntryError::UnsupportedVersion);
        }

        let ipv4 = ((value >> 24) & 0xffff_ffff) as u32;
        let port = ((value >> 8) & 0xffff) as u16;
        let addr = SocketAddrV4::new(Ipv4Addr::from(ipv4), port);
        Ok(ConnectionTarget {
            addr: SocketAddr::V4(addr),
            source: TargetSource::ConnectionCode {
                format_version: version,
            },
        })
    }
}

/// Parses either a connection code or a raw `host:port` / `[ipv6]:port` entry
/// from a single input field (REQ-F-004).
///
/// The two shapes are distinguished deterministically: any input containing a
/// colon (`:`) is a raw address entry — every valid `host:port` and
/// `[ipv6]:port` contains one and a connection code never does; anything else
/// is parsed as a connection code. Either way the result is an untrusted
/// addressing hint that grants no authority.
///
/// # Errors
///
/// Returns a [`ManualEntryError`]: connection-code variants when the input is
/// treated as a code, or [`ManualEntryError::BadHostPort`] when a colon-shaped
/// input is not a valid `host:port` / `[ipv6]:port` IP literal.
pub fn parse_manual_entry(input: &str) -> Result<ConnectionTarget, ManualEntryError> {
    let trimmed = input.trim();
    if trimmed.contains(':') {
        let addr = trimmed
            .parse::<SocketAddr>()
            .map_err(|_| ManualEntryError::BadHostPort)?;
        Ok(ConnectionTarget {
            addr,
            source: TargetSource::HostPort,
        })
    } else {
        ConnectionCode::parse(trimmed)
    }
}

/// What a typed entry names: a console on the same network, or a rendezvous
/// on the relay (ST-M007-012).
///
/// Either way it is an untrusted addressing hint that grants no authority.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ManualEntry {
    /// A LAN connection code or a raw `host:port`: where the console listens.
    Lan(ConnectionTarget),
    /// A relay code: the rendezvous seed of a session through the relay of
    /// this copy's relay profile.
    Relay(RelayCode),
}

/// Parses what a person typed into the one code field: a LAN connection code,
/// a relay code, or a raw `host:port` / `[ipv6]:port` (REQ-F-004, ST-M007-012).
///
/// The shapes are told apart without guessing. An input with a colon is a raw
/// address. Otherwise sixteen characters (hyphens and whitespace left out) are
/// a relay code, and anything else is read as a LAN connection code, twelve
/// characters. A relay code that carries a typing error is therefore reported
/// as such, and never tried as a LAN code.
///
/// # Errors
///
/// Returns the [`ManualEntryError`] of the format the input was read as.
pub fn parse_entry(input: &str) -> Result<ManualEntry, ManualEntryError> {
    let trimmed = input.trim();
    if !trimmed.contains(':') && has_relay_code_length(trimmed) {
        return RelayCode::parse(trimmed).map(ManualEntry::Relay);
    }
    parse_manual_entry(trimmed).map(ManualEntry::Lan)
}

/// Packs `[version:4][ipv4:32][port:16]` into the low 52 bits of a `u64`.
fn payload52(version: u8, ipv4: u32, port: u16) -> u64 {
    ((u64::from(version) & 0x0f) << 48) | (u64::from(ipv4) << 16) | u64::from(port)
}

/// CRC-8 (poly `0x07`, init `0x00`, no reflection, no final XOR) over the low
/// 52 bits of `payload`, processed MSB-first.
pub(crate) fn crc8(payload: u64) -> u8 {
    let mut crc: u8 = 0x00;
    for shift in (0..52u32).rev() {
        let bit = ((payload >> shift) & 1) as u8;
        let feedback = ((crc >> 7) & 1) ^ bit;
        crc <<= 1;
        if feedback == 1 {
            crc ^= 0x07;
        }
    }
    crc
}

/// Renders a 60-bit `value` as 12 Crockford Base32 chars grouped
/// `XXXX-XXXX-XXXX`.
pub(crate) fn encode_symbols(value: u64) -> String {
    let mut out = String::with_capacity(CODE_LEN + 2);
    for group in 0..CODE_LEN {
        let shift = 55 - 5 * group as u32;
        let index = ((value >> shift) & 0x1f) as usize;
        out.push(ALPHABET[index] as char);
        if group == 3 || group == 7 {
            out.push('-');
        }
    }
    out
}

/// Decodes one Crockford Base32 symbol to its 5-bit value, case-insensitively,
/// mapping `I`/`L` to `1` and `O` to `0`. Returns `None` for any character
/// outside the alphabet (including `U`).
pub(crate) fn decode_symbol(ch: char) -> Option<u8> {
    if !ch.is_ascii() {
        return None;
    }
    let upper = ch.to_ascii_uppercase();
    match upper {
        'I' | 'L' => Some(1),
        'O' => Some(0),
        _ => ALPHABET
            .iter()
            .position(|&b| b == upper as u8)
            .and_then(|p| u8::try_from(p).ok()),
    }
}

#[cfg(test)]
pub(crate) use self::{
    crc8 as crc8_for_tests, decode_symbol as decode_symbol_for_tests,
    encode_symbols as encode_symbols_for_tests,
};
