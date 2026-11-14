//! The relay code: version 2 of the connection-code family (ST-M007-012,
//! ADR-0016 §2, ADR-0007).
//!
//! When the two computers are not on the same network, the technician reads
//! one short **relay code** to the customer, as on a LAN, and both copies of
//! CretLink meet on the technician's relay. The code holds a random
//! **rendezvous seed** and nothing else: no address, no key and no authority.
//! Both copies take the relay from their own relay profile and derive the
//! rendezvous ticket from the seed and the relay's host name
//! (`cretlink_relay::RendezvousTicket::derive`), so two copies with different
//! relays never meet.
//!
//! Like the LAN connection code, a relay code only lets two connections find
//! each other. Trust still comes from pairing and the Session Approval Code
//! alone (REQ-F-005, REQ-S-006). Someone who guesses or overhears a code can
//! at most reach the rendezvous first; the approval codes then differ and the
//! session fails closed.
//!
//! # Code format (v2)
//!
//! An 80-bit value, shown as **16 Crockford Base32 characters** grouped
//! `XXXX-XXXX-XXXX-XXXX`:
//!
//! | bits | field |
//! |------|-------|
//! | 4    | format version (`2`) |
//! | 64   | rendezvous seed (random, big-endian) |
//! | 12   | CRC-12 over the first 68 bits |
//!
//! The alphabet and the tolerance on input are those of the LAN code: no
//! `I L O U`, any case, `I`/`L` read as `1` and `O` as `0`, hyphens and ASCII
//! whitespace ignored.
//!
//! ## CRC
//!
//! CRC-12 with polynomial `0x80F` (`x^12 + x^11 + x^3 + x^2 + x + 1`), initial
//! value `0x000`, no reflection, no final XOR, computed bit by bit, most
//! significant bit first, over the 68-bit `[version:4][seed:64]` payload. It
//! catches every mistyped character and every swap of two neighbouring
//! characters (each is a burst of at most ten bits), and lets a random error
//! through with odds of 1 in 4096. It is a typing check and **no secret**:
//! anyone can compute it, and a code with a valid checksum proves nothing.

use core::fmt;

use rand::TryRngCore;
use rand::rngs::OsRng;
use thiserror::Error;

use crate::code::{ALPHABET, ManualEntryError, decode_symbol};

/// The operating system's randomness source failed while a relay code was
/// minted. No code was produced: a session through the relay is then not
/// offered, instead of using a seed that could be guessed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
#[error("the operating system gave no randomness for a relay code")]
pub struct RelayCodeRandomnessError;

/// Number of Base32 characters in a relay code (excluding grouping).
pub(crate) const RELAY_CODE_LEN: usize = 16;

/// Length in bytes of the rendezvous seed a relay code carries (64 bits).
pub const RELAY_CODE_SEED_LEN: usize = 8;

/// A relay code: the rendezvous seed of one session through a relay.
///
/// **Addressing only.** Holding a relay code performs no I/O and grants no
/// authority; it names a rendezvous that both copies then look up on the
/// relay of their profile.
///
/// [`Debug`] is redacted, so a code does not land in a log by accident. The
/// technician's screen shows it on purpose, through [`RelayCode::encode`].
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct RelayCode {
    seed: [u8; RELAY_CODE_SEED_LEN],
}

impl RelayCode {
    /// The format version of a relay code in the connection-code family.
    pub const FORMAT_VERSION: u8 = 2;

    /// Mints a new code with a seed from the operating system's randomness.
    /// The technician's copy mints one for every session.
    ///
    /// # Errors
    ///
    /// Returns [`RelayCodeRandomnessError`] if the operating system's
    /// randomness source fails; no code is produced then.
    pub fn generate() -> Result<Self, RelayCodeRandomnessError> {
        let mut seed = [0u8; RELAY_CODE_SEED_LEN];
        OsRng
            .try_fill_bytes(&mut seed)
            .map_err(|_| RelayCodeRandomnessError)?;
        Ok(Self { seed })
    }

    /// A code with the given seed, for tests and frozen vectors.
    #[must_use]
    pub const fn from_seed(seed: [u8; RELAY_CODE_SEED_LEN]) -> Self {
        Self { seed }
    }

    /// The rendezvous seed. Both copies derive the rendezvous ticket from it
    /// and from the relay's host name; do not log it.
    #[must_use]
    pub const fn seed(&self) -> &[u8; RELAY_CODE_SEED_LEN] {
        &self.seed
    }

    /// Renders the code as 16 characters grouped `XXXX-XXXX-XXXX-XXXX`, for
    /// the technician to read aloud.
    #[must_use]
    pub fn encode(&self) -> String {
        let payload = payload68(Self::FORMAT_VERSION, u64::from_be_bytes(self.seed));
        let value = (payload << 12) | u128::from(crc12(payload));
        let mut out = String::with_capacity(RELAY_CODE_LEN + 3);
        for index in 0..RELAY_CODE_LEN {
            let shift = 75 - 5 * index as u32;
            out.push(ALPHABET[((value >> shift) & 0x1f) as usize] as char);
            if index % 4 == 3 && index + 1 != RELAY_CODE_LEN {
                out.push('-');
            }
        }
        out
    }

    /// Parses a typed relay code. Hyphens and ASCII whitespace are ignored,
    /// and the checksum is verified before the version, so a mistyped code is
    /// reported as [`ManualEntryError::ChecksumMismatch`] and nothing is
    /// dialled.
    ///
    /// # Errors
    ///
    /// [`ManualEntryError::BadLength`] unless the input holds exactly 16
    /// characters of the alphabet, [`ManualEntryError::BadChar`] for a
    /// character outside it, [`ManualEntryError::ChecksumMismatch`] for a
    /// typing error, and [`ManualEntryError::UnsupportedVersion`] for a
    /// well-formed code of another format version.
    pub fn parse(input: &str) -> Result<Self, ManualEntryError> {
        let mut value: u128 = 0;
        let mut count: usize = 0;
        for ch in input.chars() {
            if ch == '-' || ch.is_ascii_whitespace() {
                continue;
            }
            count += 1;
            if count > RELAY_CODE_LEN {
                return Err(ManualEntryError::BadLength);
            }
            let symbol = decode_symbol(ch).ok_or(ManualEntryError::BadChar)?;
            value = (value << 5) | u128::from(symbol);
        }
        if count != RELAY_CODE_LEN {
            return Err(ManualEntryError::BadLength);
        }

        // `value` holds exactly 80 significant bits.
        let payload = value >> 12; // [version:4][seed:64]
        let stored_crc = (value & 0x0fff) as u16;
        if crc12(payload) != stored_crc {
            return Err(ManualEntryError::ChecksumMismatch);
        }
        let version = ((payload >> 64) & 0x0f) as u8;
        if version != Self::FORMAT_VERSION {
            return Err(ManualEntryError::UnsupportedVersion);
        }
        let seed = (payload & u128::from(u64::MAX)) as u64;
        Ok(Self {
            seed: seed.to_be_bytes(),
        })
    }
}

/// Redacted: a relay code names a rendezvous and must not reach a log.
impl fmt::Debug for RelayCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("RelayCode(<redacted>)")
    }
}

/// Packs `[version:4][seed:64]` into the low 68 bits of a `u128`.
fn payload68(version: u8, seed: u64) -> u128 {
    (u128::from(version & 0x0f) << 64) | u128::from(seed)
}

/// CRC-12 (polynomial `0x80F`, initial value `0x000`, no reflection, no final
/// XOR) over the low 68 bits of `payload`, most significant bit first.
pub(crate) fn crc12(payload: u128) -> u16 {
    let mut crc: u16 = 0;
    for shift in (0..68u32).rev() {
        let bit = ((payload >> shift) & 1) as u16;
        let feedback = ((crc >> 11) & 1) ^ bit;
        crc = (crc << 1) & 0x0fff;
        if feedback == 1 {
            crc ^= 0x80f;
        }
    }
    crc
}

/// Whether `input` has the length of a relay code: sixteen characters once
/// hyphens and whitespace are left out. Tells a relay code from a LAN code
/// (twelve characters) before either is parsed.
pub(crate) fn has_relay_code_length(input: &str) -> bool {
    input
        .chars()
        .filter(|ch| *ch != '-' && !ch.is_ascii_whitespace())
        .count()
        == RELAY_CODE_LEN
}

#[cfg(test)]
mod tests;
