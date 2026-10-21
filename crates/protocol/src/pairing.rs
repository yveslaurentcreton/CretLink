//! Pairing prologue and Session Approval Code derivation (ADR-0003).
//!
//! This module carries the transcript-binding pieces of the M002 secure
//! channel that both endpoints must compute **identically**:
//!
//! - [`pairing_prologue`] builds the frozen prologue bytes that are fed into
//!   the Noise `XX` handshake ([`Builder::prologue`] on the `snow` side). The
//!   prologue commits both peers to the same protocol version and capability
//!   set, so a silent downgrade (a different version or a trimmed capability
//!   list) changes the prologue on one side and makes the handshake fail
//!   closed — the REQ-S-008 mechanism.
//! - [`sac_commitment`] and [`derive_session_approval_code`] implement the
//!   commit-reveal Session Approval Code round (REQ-S-006). The code binds the
//!   Noise handshake hash `h` together with the two peer nonces, so a
//!   man-in-the-middle who ran two separate handshakes cannot make both
//!   humans see the same six digits.
//!
//! This module invents no cryptography: it composes SHA-256 and HKDF-SHA256
//! from the RustCrypto crates exactly as ADR-0003 specifies. The random nonce
//! generation, the Noise handshake itself, and the constant-time commitment
//! comparison live in `cretlink-transport`.

use core::fmt;

use hkdf::Hkdf;
use sha2::{Digest, Sha256};
use zeroize::Zeroizing;

use crate::limits;
use crate::version::ProtocolVersion;

/// Domain-separation prefix for the pairing prologue (ADR-0003, frozen wire
/// layout). The trailing NUL keeps the fixed prefix unambiguous from the
/// variable-length body that follows.
pub const PAIRING_PROLOGUE_MAGIC: &[u8] = b"cretlink-pairing-v1\0";

/// HKDF salt for Session Approval Code derivation (ADR-0003).
pub const SAC_HKDF_SALT: &[u8] = b"cretlink-sac-v1";

/// Byte length of the commitment (`SHA-256(na)`) and of each pairing nonce.
pub const SAC_NONCE_LEN: usize = 32;

/// Byte length of the commitment value carried in a pairing `Commit`.
pub const SAC_COMMITMENT_LEN: usize = 32;

/// Modulus applied to the derived code so every six-digit value is reachable
/// (modular reduction, **not** bit truncation — ADR-0003).
pub const SAC_MODULUS: u32 = 1_000_000;

/// Number of decimal digits the Session Approval Code is rendered with.
pub const SAC_DIGITS: usize = 6;

/// A human-comparable Session Approval Code (REQ-F-005, REQ-S-006).
///
/// The inner value is already reduced modulo [`SAC_MODULUS`], so it is always
/// in `0..1_000_000`. Its [`fmt::Display`] renders exactly [`SAC_DIGITS`]
/// zero-padded decimal digits; both endpoints display that string for the
/// human comparison that gates full-control consent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SessionApprovalCode(pub u32);

impl fmt::Display for SessionApprovalCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // `SAC_DIGITS` is 6; keep the width literal so the format string is a
        // compile-time constant.
        write!(f, "{:06}", self.0)
    }
}

/// Builds the frozen pairing prologue for a given protocol version and
/// capability set (ADR-0003).
///
/// # Wire layout
///
/// 1. [`PAIRING_PROLOGUE_MAGIC`] (`b"cretlink-pairing-v1\0"`);
/// 2. `version.major` as a big-endian `u16`;
/// 3. `version.minor` as a big-endian `u16`;
/// 4. the capability count as a big-endian `u16`;
/// 5. for each capability, its UTF-8 byte length as a big-endian `u16`
///    followed by the UTF-8 bytes.
///
/// Capabilities are **sorted and deduplicated** before hashing so that both
/// endpoints derive an identical prologue regardless of the order in which
/// they listed their capabilities. The count and per-capability length are
/// bounded by [`limits::MAX_CAPABILITIES`] and [`limits::MAX_CAPABILITY_LEN`]:
/// any capability longer than the per-item limit is dropped and the list is
/// clamped to the maximum count, so the output stays bounded even for hostile
/// input. On the valid path these bounds are never reached because
/// [`crate::Message::validate`] rejects an out-of-bounds `Hello` before the
/// handshake begins.
///
/// Both endpoints feed the returned bytes into the Noise handshake unchanged;
/// any difference (a different version or capability set — that is, a
/// downgrade attempt) yields a different prologue and makes the handshake fail
/// closed (REQ-S-008).
#[must_use]
pub fn pairing_prologue(version: ProtocolVersion, capabilities: &[String]) -> Vec<u8> {
    // Drop over-length entries, then sort and dedupe for an order-independent,
    // bounded canonical form.
    let mut caps: Vec<&str> = capabilities
        .iter()
        .map(String::as_str)
        .filter(|cap| cap.len() <= limits::MAX_CAPABILITY_LEN)
        .collect();
    caps.sort_unstable();
    caps.dedup();
    caps.truncate(limits::MAX_CAPABILITIES);

    let mut out = Vec::with_capacity(PAIRING_PROLOGUE_MAGIC.len() + 6);
    out.extend_from_slice(PAIRING_PROLOGUE_MAGIC);
    out.extend_from_slice(&version.major.to_be_bytes());
    out.extend_from_slice(&version.minor.to_be_bytes());
    // `caps.len()` is clamped to MAX_CAPABILITIES (32), well within u16.
    out.extend_from_slice(&(caps.len() as u16).to_be_bytes());
    for cap in caps {
        // `cap.len()` is clamped to MAX_CAPABILITY_LEN (64), well within u16.
        out.extend_from_slice(&(cap.len() as u16).to_be_bytes());
        out.extend_from_slice(cap.as_bytes());
    }
    out
}

/// Computes the pairing commitment `SHA-256(na)` (ADR-0003).
///
/// The initiator sends this in its `Commit` before either side reveals a
/// nonce; the responder recomputes it from the revealed `na` and compares it
/// against the received commitment (in constant time, in the transport layer)
/// before deriving any code.
#[must_use]
pub fn sac_commitment(na: &[u8; SAC_NONCE_LEN]) -> [u8; SAC_COMMITMENT_LEN] {
    let mut hasher = Sha256::new();
    hasher.update(na);
    hasher.finalize().into()
}

/// Derives the Session Approval Code from the Noise handshake hash and the two
/// peer nonces (ADR-0003).
///
/// `SAC = HKDF-SHA256(salt = "cretlink-sac-v1", ikm = h || na || nc, info =
/// "")`; the first four output bytes are read big-endian as a `u32` and
/// reduced modulo [`SAC_MODULUS`]. Both endpoints call this with the same
/// inputs on an untampered channel and therefore obtain the same code; a
/// man-in-the-middle produces a different `h` (or nonces) on each side and the
/// two humans see different digits (REQ-S-006).
#[must_use]
pub fn derive_session_approval_code(
    h: &[u8; 32],
    na: &[u8; SAC_NONCE_LEN],
    nc: &[u8; SAC_NONCE_LEN],
) -> SessionApprovalCode {
    // The input keying material concatenates secret transcript-bound values;
    // zeroize the transient copy when it leaves scope.
    let mut ikm = Zeroizing::new([0u8; 32 + SAC_NONCE_LEN + SAC_NONCE_LEN]);
    ikm[..32].copy_from_slice(h);
    ikm[32..32 + SAC_NONCE_LEN].copy_from_slice(na);
    ikm[32 + SAC_NONCE_LEN..].copy_from_slice(nc);

    let hk = Hkdf::<Sha256>::new(Some(SAC_HKDF_SALT), ikm.as_ref());
    let mut okm = [0u8; 4];
    // HKDF-Expand only fails when the requested output exceeds 255*HashLen
    // (8160 bytes for SHA-256); a four-byte output can never trigger that, so
    // this Result is always `Ok`. We discard it explicitly rather than unwrap
    // to keep library code free of panics (workspace lints deny them).
    let _ = hk.expand(b"", &mut okm);

    SessionApprovalCode(u32::from_be_bytes(okm) % SAC_MODULUS)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(major: u16, minor: u16) -> ProtocolVersion {
        ProtocolVersion::new(major, minor)
    }

    fn caps(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| (*s).to_owned()).collect()
    }

    // --- prologue: exact wire layout (frozen golden) ---

    #[test]
    fn prologue_wire_layout_is_frozen() {
        let bytes = pairing_prologue(v(1, 0), &caps(&["exec"]));
        let mut expected = Vec::new();
        expected.extend_from_slice(b"cretlink-pairing-v1\0");
        expected.extend_from_slice(&1u16.to_be_bytes()); // major
        expected.extend_from_slice(&0u16.to_be_bytes()); // minor
        expected.extend_from_slice(&1u16.to_be_bytes()); // capability count
        expected.extend_from_slice(&4u16.to_be_bytes()); // len("exec")
        expected.extend_from_slice(b"exec");
        assert_eq!(bytes, expected);
    }

    #[test]
    fn prologue_magic_has_trailing_nul() {
        assert_eq!(PAIRING_PROLOGUE_MAGIC, b"cretlink-pairing-v1\0");
        assert_eq!(*PAIRING_PROLOGUE_MAGIC.last().unwrap(), 0u8);
    }

    // --- prologue: order-independent and dedup ---

    #[test]
    fn prologue_is_order_independent() {
        let a = pairing_prologue(v(1, 0), &caps(&["exec", "jobs", "files"]));
        let b = pairing_prologue(v(1, 0), &caps(&["files", "exec", "jobs"]));
        let c = pairing_prologue(v(1, 0), &caps(&["jobs", "files", "exec"]));
        assert_eq!(a, b);
        assert_eq!(b, c);
    }

    #[test]
    fn prologue_deduplicates_capabilities() {
        let with_dupes = pairing_prologue(v(1, 0), &caps(&["exec", "exec", "jobs", "jobs"]));
        let without = pairing_prologue(v(1, 0), &caps(&["exec", "jobs"]));
        assert_eq!(with_dupes, without);
    }

    // --- prologue: injective enough (different inputs -> different bytes) ---

    #[test]
    fn prologue_differs_on_major_version() {
        assert_ne!(
            pairing_prologue(v(1, 0), &caps(&["exec"])),
            pairing_prologue(v(2, 0), &caps(&["exec"])),
        );
    }

    #[test]
    fn prologue_differs_on_minor_version() {
        assert_ne!(
            pairing_prologue(v(1, 0), &caps(&["exec"])),
            pairing_prologue(v(1, 1), &caps(&["exec"])),
        );
    }

    #[test]
    fn prologue_differs_on_capability_set() {
        // Adding a capability (a downgrade in reverse) must change the bytes.
        assert_ne!(
            pairing_prologue(v(1, 0), &caps(&["exec"])),
            pairing_prologue(v(1, 0), &caps(&["exec", "jobs"])),
        );
        // A different single capability must change the bytes.
        assert_ne!(
            pairing_prologue(v(1, 0), &caps(&["exec"])),
            pairing_prologue(v(1, 0), &caps(&["jobs"])),
        );
    }

    #[test]
    fn prologue_length_prefixing_prevents_ambiguity() {
        // Without per-item length prefixes, {"ab","c"} and {"a","bc"} would
        // collide. The u16 length prefix keeps them distinct.
        assert_ne!(
            pairing_prologue(v(1, 0), &caps(&["ab", "c"])),
            pairing_prologue(v(1, 0), &caps(&["a", "bc"])),
        );
    }

    #[test]
    fn prologue_empty_capabilities_is_valid() {
        let bytes = pairing_prologue(v(1, 0), &[]);
        let mut expected = Vec::new();
        expected.extend_from_slice(PAIRING_PROLOGUE_MAGIC);
        expected.extend_from_slice(&1u16.to_be_bytes());
        expected.extend_from_slice(&0u16.to_be_bytes());
        expected.extend_from_slice(&0u16.to_be_bytes()); // zero capabilities
        assert_eq!(bytes, expected);
    }

    // --- prologue: limit enforcement (bounded output on hostile input) ---

    #[test]
    fn prologue_clamps_capability_count() {
        // More than MAX_CAPABILITIES distinct entries: the encoded count is
        // clamped, so the output stays bounded.
        let many: Vec<String> = (0..(limits::MAX_CAPABILITIES + 10))
            .map(|i| format!("cap{i:03}"))
            .collect();
        let bytes = pairing_prologue(v(1, 0), &many);
        // Count field sits right after magic + 2*u16 version.
        let count_offset = PAIRING_PROLOGUE_MAGIC.len() + 4;
        let count = u16::from_be_bytes([bytes[count_offset], bytes[count_offset + 1]]);
        assert_eq!(count as usize, limits::MAX_CAPABILITIES);
    }

    #[test]
    fn prologue_drops_over_length_capabilities() {
        let too_long = "x".repeat(limits::MAX_CAPABILITY_LEN + 1);
        let with_bad = pairing_prologue(v(1, 0), &caps(&["exec", too_long.as_str()]));
        let without = pairing_prologue(v(1, 0), &caps(&["exec"]));
        assert_eq!(with_bad, without);
    }

    // --- commitment ---

    #[test]
    fn commitment_is_sha256_of_nonce() {
        let na = [7u8; SAC_NONCE_LEN];
        let commitment = sac_commitment(&na);
        // Independent SHA-256 of 32 bytes of 0x07.
        let mut hasher = Sha256::new();
        hasher.update(na);
        let expected: [u8; 32] = hasher.finalize().into();
        assert_eq!(commitment, expected);
        assert_eq!(commitment.len(), SAC_COMMITMENT_LEN);
    }

    #[test]
    fn commitment_differs_for_different_nonces() {
        assert_ne!(
            sac_commitment(&[1u8; SAC_NONCE_LEN]),
            sac_commitment(&[2u8; SAC_NONCE_LEN]),
        );
    }

    // --- Session Approval Code derivation (frozen golden vector) ---

    #[test]
    fn sac_display_zero_pads_to_six_digits() {
        assert_eq!(SessionApprovalCode(0).to_string(), "000000");
        assert_eq!(SessionApprovalCode(42).to_string(), "000042");
        assert_eq!(SessionApprovalCode(123_456).to_string(), "123456");
        assert_eq!(SessionApprovalCode(999_999).to_string(), "999999");
    }

    #[test]
    fn sac_is_always_below_the_modulus() {
        // A range of handshake hashes must all reduce into 0..1_000_000.
        for byte in 0u8..=255 {
            let code = derive_session_approval_code(
                &[byte; 32],
                &[byte; SAC_NONCE_LEN],
                &[byte.wrapping_add(1); SAC_NONCE_LEN],
            );
            assert!(code.0 < SAC_MODULUS, "code {} >= modulus", code.0);
        }
    }

    #[test]
    fn sac_golden_vector_is_frozen() {
        // Fixed inputs -> a known six-digit code. This freezes the derivation
        // (salt, ikm ordering, modular reduction, big-endian read). A change
        // to any of those breaks this test on purpose (ADR-0003 test vectors).
        let h = [0x11u8; 32];
        let na = [0x22u8; SAC_NONCE_LEN];
        let nc = [0x33u8; SAC_NONCE_LEN];
        let code = derive_session_approval_code(&h, &na, &nc);
        assert_eq!(code.to_string(), "791119");
        assert_eq!(code, SessionApprovalCode(791_119));
    }

    #[test]
    fn sac_second_golden_vector_is_frozen() {
        let h = [0u8; 32];
        let na = [0u8; SAC_NONCE_LEN];
        let nc = [0u8; SAC_NONCE_LEN];
        let code = derive_session_approval_code(&h, &na, &nc);
        assert_eq!(code.to_string(), "797405");
    }

    #[test]
    fn sac_changes_when_any_input_changes() {
        let base = derive_session_approval_code(&[1u8; 32], &[2u8; 32], &[3u8; 32]);
        let diff_h = derive_session_approval_code(&[9u8; 32], &[2u8; 32], &[3u8; 32]);
        let diff_na = derive_session_approval_code(&[1u8; 32], &[9u8; 32], &[3u8; 32]);
        let diff_nc = derive_session_approval_code(&[1u8; 32], &[2u8; 32], &[9u8; 32]);
        assert_ne!(base, diff_h);
        assert_ne!(base, diff_na);
        assert_ne!(base, diff_nc);
    }

    #[test]
    fn sac_nonce_order_matters() {
        // Swapping na and nc must (with overwhelming probability) change the
        // code: the ikm ordering h || na || nc is part of the frozen contract.
        let a = derive_session_approval_code(&[5u8; 32], &[1u8; 32], &[2u8; 32]);
        let b = derive_session_approval_code(&[5u8; 32], &[2u8; 32], &[1u8; 32]);
        assert_ne!(a, b);
    }
}
