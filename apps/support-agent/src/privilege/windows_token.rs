//! Windows process-token queries behind localized, reviewed `unsafe` (ADR-0001).
//!
//! This is one of two modules in the support agent that use `unsafe` (the
//! other is the DPAPI sealer, `dpapi`). The
//! workspace denies `unsafe_code` globally (see the workspace `[lints]`); this
//! module opts back in with the crate-narrow, reviewed allowance ADR-0001
//! grants for Microsoft's `windows` FFI bindings, and keeps every `unsafe`
//! block small and individually justified with a `SAFETY:` note.
//!
//! Truthfulness (REQ-S-011): every query that fails degrades to
//! `elevated = false` / [`IntegrityLevel::Unknown`] with a secret-free note, so
//! the agent never reports elevation or an integrity level it could not
//! actually observe. The process-token handle is closed on drop, so no handle
//! leaks even on an early return.
#![allow(unsafe_code)]

use core::ffi::c_void;

use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::Security::Authentication::Identity::{GetUserNameExW, NameSamCompatible};
use windows::Win32::Security::{
    GetSidSubAuthority, GetSidSubAuthorityCount, GetTokenInformation, TOKEN_ELEVATION,
    TOKEN_MANDATORY_LABEL, TOKEN_QUERY, TokenElevation, TokenIntegrityLevel,
};
use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
use windows::Win32::System::WindowsProgramming::GetUserNameW;
use windows::core::PWSTR;

use super::{EffectiveIdentity, IntegrityLevel, integrity_from_rid};

/// Owns a process-token handle and closes it on drop (fail-closed cleanup).
struct TokenHandle(HANDLE);

impl Drop for TokenHandle {
    fn drop(&mut self) {
        // SAFETY: `self.0` came from `OpenProcessToken`, is not the
        // `GetCurrentProcess` pseudo-handle, and is closed exactly once here.
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}

/// Queries the current process token and reports the effective identity.
pub(super) fn current_identity() -> EffectiveIdentity {
    // `DOMAIN\user` (the machine name for a local account), so the technician
    // sees the customer's qualified account (ST-M009-008); the bare user name
    // when the qualified one cannot be read.
    let (domain, username) = match sam_compatible_name() {
        Some((domain, user)) => (Some(domain), user),
        None => (
            None,
            current_username().unwrap_or_else(|| super::UNKNOWN_USERNAME.to_string()),
        ),
    };

    let Some(token) = open_current_process_token() else {
        return EffectiveIdentity {
            username,
            domain,
            elevated: false,
            integrity: IntegrityLevel::Unknown,
            is_windows: true,
            note: Some("could not open the process token; reporting no elevation".to_string()),
        };
    };

    let elevated = query_elevation(&token);
    let integrity = query_integrity_level(&token);

    let note = if elevated.is_none() || integrity == IntegrityLevel::Unknown {
        Some(
            "some privilege details were unavailable; unknown is reported as not elevated"
                .to_string(),
        )
    } else {
        None
    };

    EffectiveIdentity {
        username,
        // From `GetUserNameExW(NameSamCompatible)`; `None` when only the bare
        // user name could be read, never guessed.
        domain,
        elevated: elevated.unwrap_or(false),
        integrity,
        is_windows: true,
        note,
    }
}

/// Opens the current process's token for querying, or `None` on failure.
fn open_current_process_token() -> Option<TokenHandle> {
    let mut handle = HANDLE(core::ptr::null_mut());
    // SAFETY: `GetCurrentProcess` returns a valid pseudo-handle and `handle` is
    // a valid out-pointer; we request only `TOKEN_QUERY`. On success `handle`
    // owns a real token handle, wrapped below in a closing guard.
    let opened = unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut handle) };
    opened.ok().map(|()| TokenHandle(handle))
}

/// Reads the token's elevation flag, or `None` if the query failed.
fn query_elevation(token: &TokenHandle) -> Option<bool> {
    let mut elevation = TOKEN_ELEVATION { TokenIsElevated: 0 };
    let mut returned = 0u32;
    let size = core::mem::size_of::<TOKEN_ELEVATION>() as u32;
    // SAFETY: `token.0` is a valid `TOKEN_QUERY` handle. We pass a correctly
    // sized, writable `TOKEN_ELEVATION` out-buffer matching the `TokenElevation`
    // class, plus a valid `returned` out-pointer.
    let result = unsafe {
        GetTokenInformation(
            token.0,
            TokenElevation,
            Some(core::ptr::from_mut(&mut elevation).cast::<c_void>()),
            size,
            &mut returned,
        )
    };
    result.ok().map(|()| elevation.TokenIsElevated != 0)
}

/// Reads the token's mandatory integrity level, or `Unknown` if it could not be
/// determined.
fn query_integrity_level(token: &TokenHandle) -> IntegrityLevel {
    // First call: ask for the required buffer size.
    let mut needed = 0u32;
    // SAFETY: a null buffer with length 0 asks the API for the required size,
    // which it writes to `needed`; the expected size-probe failure is ignored.
    let _ = unsafe { GetTokenInformation(token.0, TokenIntegrityLevel, None, 0, &mut needed) };
    if (needed as usize) < core::mem::size_of::<TOKEN_MANDATORY_LABEL>() {
        return IntegrityLevel::Unknown;
    }

    let mut buffer = vec![0u8; needed as usize];
    // SAFETY: `buffer` is exactly `needed` bytes, matching the size the API just
    // reported for the `TokenIntegrityLevel` class; `needed` is a valid
    // out-pointer.
    let filled = unsafe {
        GetTokenInformation(
            token.0,
            TokenIntegrityLevel,
            Some(buffer.as_mut_ptr().cast::<c_void>()),
            needed,
            &mut needed,
        )
    };
    if filled.is_err() {
        return IntegrityLevel::Unknown;
    }

    // SAFETY: on success the buffer begins with a `TOKEN_MANDATORY_LABEL` whose
    // `Label.Sid` points within the same, still-live `buffer`. The header is
    // copied out unaligned (the `Vec<u8>` may be under-aligned for the struct),
    // then the SID's last sub-authority — the integrity RID — is read through
    // the documented SID accessors, checking every pointer for null first.
    let rid = unsafe {
        let label = core::ptr::read_unaligned(buffer.as_ptr().cast::<TOKEN_MANDATORY_LABEL>());
        let sid = label.Label.Sid;
        if sid.0.is_null() {
            return IntegrityLevel::Unknown;
        }
        let count_ptr = GetSidSubAuthorityCount(sid);
        if count_ptr.is_null() {
            return IntegrityLevel::Unknown;
        }
        let count = *count_ptr;
        if count == 0 {
            return IntegrityLevel::Unknown;
        }
        let rid_ptr = GetSidSubAuthority(sid, u32::from(count - 1));
        if rid_ptr.is_null() {
            return IntegrityLevel::Unknown;
        }
        *rid_ptr
    };

    integrity_from_rid(rid)
}

/// The longest SAM-compatible name accepted, in UTF-16 units (a domain of 15
/// and a user name of 256 fit with room to spare).
const MAX_SAM_UNITS: u32 = 1024;

/// Reads the current user's SAM-compatible name, `DOMAIN\user`, split into
/// its domain and user name; `None` on failure or an unexpected form.
fn sam_compatible_name() -> Option<(String, String)> {
    let mut len: u32 = 0;
    // SAFETY: a null buffer with a zero size only asks for the required size,
    // written to `len`; nothing else is read or written. The expected
    // size-probe failure is ignored.
    let _ = unsafe { GetUserNameExW(NameSamCompatible, None, &mut len) };
    if len == 0 || len > MAX_SAM_UNITS {
        return None;
    }
    let mut buffer = vec![0u16; len as usize];
    // SAFETY: `buffer` holds `len` UTF-16 units and `len` says so; the call
    // writes at most that many (with the terminator) and sets `len` to the
    // units written without it.
    let ok = unsafe {
        GetUserNameExW(
            NameSamCompatible,
            Some(PWSTR(buffer.as_mut_ptr())),
            &mut len,
        )
    };
    if !ok || len as usize > buffer.len() {
        return None;
    }
    buffer.truncate(len as usize);
    super::split_sam_compatible(&String::from_utf16(&buffer).ok()?)
}

/// Reads the account name of the current user, or `None` on failure.
fn current_username() -> Option<String> {
    // UNLEN (256) + 1 for the terminating NUL is the documented maximum length.
    let mut size = 257u32;
    let mut buffer = vec![0u16; size as usize];
    // SAFETY: `buffer` has `size` UTF-16 slots and `size` reflects that
    // capacity; `GetUserNameW` writes a NUL-terminated name and updates `size`
    // to the count written (including the NUL).
    let result = unsafe { GetUserNameW(Some(PWSTR(buffer.as_mut_ptr())), &mut size) };
    result.ok()?;
    let len = (size as usize).saturating_sub(1).min(buffer.len());
    Some(String::from_utf16_lossy(&buffer[..len]))
}
