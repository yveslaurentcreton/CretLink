//! Windows DPAPI at-rest sealer for reboot-resume key material behind
//! localized, reviewed `unsafe` (ADR-0001, ADR-0014 §4).
//!
//! Reboot-resume must persist `K_rr` across a reboot without ever writing it in
//! the clear (ADR-0014). On Windows the established platform primitive for that
//! is the Data Protection API: [`CryptProtectData`] / [`CryptUnprotectData`]
//! seal a blob to the **current user** so only that user on that machine can
//! recover it — inventing no cryptography of our own (REQ-S-005). We pass
//! `CRYPTPROTECT_UI_FORBIDDEN` so sealing never blocks on an interactive prompt,
//! and no optional entropy (the transcript binding already lives inside `K_rr`).
//!
//! This is the only `unsafe` in the reboot-resume path. The workspace denies
//! `unsafe_code`; this module opts back in with the crate-narrow allowance
//! ADR-0001 grants for Microsoft's `windows` FFI, keeps every `unsafe` block
//! small and individually justified with a `SAFETY:` note, and frees the
//! DPAPI-allocated output buffer on every path so no memory leaks. Every backend
//! failure degrades to a coarse [`SealError`] that never distinguishes *why*, so
//! a failed unseal collapses to the same fail-closed reconnect refusal
//! (ADR-0014 §5).
#![allow(unsafe_code)]

use core::ffi::c_void;

use cretlink_protocol::{AtRestSealer, SealError};
use windows::Win32::Foundation::{HLOCAL, LocalFree};
use windows::Win32::Security::Cryptography::{
    CRYPT_INTEGER_BLOB, CRYPTPROTECT_UI_FORBIDDEN, CryptProtectData, CryptUnprotectData,
};
use windows::core::PCWSTR;
use zeroize::Zeroizing;

/// Seals reboot-resume key material to the current Windows user via DPAPI.
///
/// Holds no state: each call is a standalone DPAPI round trip. Cloneable so the
/// agent can hand it to the vault and keep a copy for diagnostics.
#[derive(Debug, Clone, Copy, Default)]
pub struct DpapiSealer;

impl DpapiSealer {
    /// Builds a DPAPI sealer bound to the current user context.
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

/// Copies out the bytes of a DPAPI-produced output blob and frees the buffer
/// DPAPI allocated for it (via `LocalAlloc`), leaving nothing behind.
///
/// # Safety
///
/// `blob` must be an output blob just written by `CryptProtectData` /
/// `CryptUnprotectData`: `pbData` either null (then `cbData` is 0) or a live
/// `LocalAlloc` allocation of `cbData` bytes owned by this call.
unsafe fn take_and_free(blob: &CRYPT_INTEGER_BLOB) -> Vec<u8> {
    let out = if blob.pbData.is_null() || blob.cbData == 0 {
        Vec::new()
    } else {
        // SAFETY: on success DPAPI guarantees `pbData` addresses `cbData`
        // initialized bytes; we copy them out before freeing.
        unsafe { core::slice::from_raw_parts(blob.pbData, blob.cbData as usize) }.to_vec()
    };
    if !blob.pbData.is_null() {
        // SAFETY: `pbData` was allocated by DPAPI with `LocalAlloc`; freeing it
        // exactly once here matches the documented ownership transfer. The
        // returned handle (non-null only on failure) is intentionally ignored.
        let _ = unsafe { LocalFree(Some(HLOCAL(blob.pbData.cast::<c_void>()))) };
    }
    out
}

impl AtRestSealer for DpapiSealer {
    fn seal(&self, plaintext: &[u8]) -> Result<Vec<u8>, SealError> {
        let input = CRYPT_INTEGER_BLOB {
            cbData: u32::try_from(plaintext.len()).map_err(|_| SealError::Backend)?,
            // DPAPI does not mutate the input; the cast to `*mut` is required by
            // the C signature only.
            pbData: plaintext.as_ptr().cast_mut(),
        };
        let mut output = CRYPT_INTEGER_BLOB::default();
        // SAFETY: `input` describes a readable `plaintext.len()`-byte buffer;
        // description/entropy/reserved/prompt pointers are null (no UI, no extra
        // entropy); `output` is a writable out-blob DPAPI fills with a
        // `LocalAlloc` buffer we free via `take_and_free`.
        let result = unsafe {
            CryptProtectData(
                &input,
                PCWSTR::null(),
                None,
                None,
                None,
                CRYPTPROTECT_UI_FORBIDDEN,
                &mut output,
            )
        };
        result.map_err(|_| SealError::Backend)?;
        // SAFETY: on `Ok`, `output` is a DPAPI output blob per `take_and_free`.
        let sealed = unsafe { take_and_free(&output) };
        if sealed.is_empty() {
            // A zero-length seal cannot be a valid DPAPI ciphertext.
            return Err(SealError::Backend);
        }
        Ok(sealed)
    }

    fn unseal(&self, sealed: &[u8]) -> Result<Zeroizing<Vec<u8>>, SealError> {
        let input = CRYPT_INTEGER_BLOB {
            cbData: u32::try_from(sealed.len()).map_err(|_| SealError::Unrecoverable)?,
            pbData: sealed.as_ptr().cast_mut(),
        };
        let mut output = CRYPT_INTEGER_BLOB::default();
        // SAFETY: `input` describes a readable `sealed.len()`-byte buffer; the
        // description out-pointer, entropy, reserved, and prompt are null;
        // `output` is a writable out-blob DPAPI fills with a `LocalAlloc` buffer
        // we free via `take_and_free`. A wrong-user/wrong-machine/tampered blob
        // fails here and maps to `Unrecoverable`.
        let result = unsafe {
            CryptUnprotectData(
                &input,
                None,
                None,
                None,
                None,
                CRYPTPROTECT_UI_FORBIDDEN,
                &mut output,
            )
        };
        result.map_err(|_| SealError::Unrecoverable)?;
        // SAFETY: on `Ok`, `output` is a DPAPI output blob per `take_and_free`.
        let plain = unsafe { take_and_free(&output) };
        Ok(Zeroizing::new(plain))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // These exercise the real DPAPI backend; they run only on Windows, where the
    // agent actually seals. They assert the round trip and tamper-rejection the
    // reboot-resume vault relies on, without ever asserting on cleartext.

    #[test]
    fn seal_then_unseal_round_trips() {
        let sealer = DpapiSealer::new();
        let secret = b"reboot-resume key material \x00\x01\x02\xff";
        let sealed = sealer
            .seal(secret)
            .expect("DPAPI seal succeeds for the current user");
        assert_ne!(
            sealed.as_slice(),
            secret,
            "sealed output is not the plaintext"
        );
        let recovered = sealer.unseal(&sealed).expect("DPAPI unseal round-trips");
        assert_eq!(recovered.as_slice(), secret);
    }

    #[test]
    fn tampered_ciphertext_is_unrecoverable() {
        let sealer = DpapiSealer::new();
        let mut sealed = sealer.seal(b"another secret").expect("seal");
        // Flip a byte in the body so DPAPI's integrity check rejects it.
        let last = sealed.len() - 1;
        sealed[last] ^= 0xff;
        assert_eq!(sealer.unseal(&sealed), Err(SealError::Unrecoverable));
    }

    #[test]
    fn empty_ciphertext_is_unrecoverable() {
        let sealer = DpapiSealer::new();
        assert_eq!(sealer.unseal(&[]), Err(SealError::Unrecoverable));
    }
}
