//! The technician's display name from the operating-system account
//! (ST-M009-011).
//!
//! The console names its technician by what the signed-in account reports:
//! the account's **full name**, else its **user name**. The name travels in
//! the discovery advertisement and the pairing handshake, and the customer
//! side shows it as what the technician's computer says — a label, never a
//! verified identity.
//!
//! Where the full name comes from, per platform (each step falls back to the
//! next, and nothing is invented):
//!
//! - **Windows**: the directory display name (`GetUserNameExW(NameDisplay)`,
//!   domain accounts), else the local account's full name (`NetUserGetInfo`
//!   level 10, which a Microsoft account also fills), else `%USERNAME%`.
//! - **macOS**: `id -F` (the account's full name), else `$USER`.
//! - **Other Unix**: the GECOS field of `getent passwd $USER` up to the first
//!   comma, else `$USER` / `$LOGNAME`.
//!
//! The result is sanitized to one bounded line ([`DisplayName::sanitized`]);
//! `None` means neither a full name nor a user name was available, and the
//! console then sends no name.

use cretlink_protocol::DisplayName;

/// The technician's display name: the account's full name, else its user
/// name, or `None` when neither is available.
#[must_use]
pub fn technician_display_name() -> Option<DisplayName> {
    platform::full_name()
        .and_then(|name| DisplayName::sanitized(&name))
        .or_else(|| user_name().and_then(|name| DisplayName::sanitized(&name)))
}

/// The signed-in user name from the environment.
fn user_name() -> Option<String> {
    ["USERNAME", "USER", "LOGNAME"]
        .iter()
        .find_map(|key| std::env::var(key).ok())
        .filter(|name| !name.trim().is_empty())
}

/// The full name from one `passwd` line: the GECOS field (the fifth) up to the
/// first comma, or `None` when it is empty.
#[cfg_attr(any(windows, target_os = "macos"), allow(dead_code))]
fn gecos_full_name(passwd_line: &str) -> Option<String> {
    let gecos = passwd_line.trim_end().split(':').nth(4)?;
    let full = gecos.split(',').next()?.trim();
    (!full.is_empty()).then(|| full.to_owned())
}

/// Runs a short read-only command and returns its trimmed standard output.
#[cfg(not(windows))]
fn command_output(program: &str, args: &[&str]) -> Option<String> {
    let output = std::process::Command::new(program)
        .args(args)
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8(output.stdout).ok()?;
    let text = text.trim();
    (!text.is_empty()).then(|| text.to_owned())
}

#[cfg(target_os = "macos")]
mod platform {
    /// The account's full name from `id -F`.
    pub(super) fn full_name() -> Option<String> {
        super::command_output("id", &["-F"])
    }
}

#[cfg(all(unix, not(target_os = "macos")))]
mod platform {
    /// The account's full name from its `passwd` GECOS field.
    pub(super) fn full_name() -> Option<String> {
        let user = super::user_name()?;
        let line = super::command_output("getent", &["passwd", &user])?;
        super::gecos_full_name(&line)
    }
}

#[cfg(windows)]
mod platform {
    //! The Windows account's full name behind localized, reviewed `unsafe`
    //! (ADR-0001), like the support agent's token queries: every block is
    //! small, bounded, and justified with a `SAFETY:` note, and every failure
    //! falls back to the next source rather than inventing a name.
    #![allow(unsafe_code)]

    use core::ffi::c_void;

    use windows::Win32::NetworkManagement::NetManagement::{
        NetApiBufferFree, NetUserGetInfo, USER_INFO_10,
    };
    use windows::Win32::Security::Authentication::Identity::{GetUserNameExW, NameDisplay};
    use windows::core::{HSTRING, PCWSTR, PWSTR};

    /// The longest name buffer accepted, in UTF-16 units.
    const MAX_NAME_UNITS: u32 = 1024;

    pub(super) fn full_name() -> Option<String> {
        directory_display_name()
            .or_else(local_full_name)
            .filter(|name| !name.trim().is_empty())
    }

    /// The directory display name of a domain account.
    fn directory_display_name() -> Option<String> {
        let mut len: u32 = 0;
        // SAFETY: a null buffer with a zero size only asks for the required
        // size, written to `len`; nothing else is read or written.
        let _ = unsafe { GetUserNameExW(NameDisplay, None, &mut len) };
        if len == 0 || len > MAX_NAME_UNITS {
            return None;
        }
        let mut buffer = vec![0u16; len as usize];
        // SAFETY: `buffer` holds `len` UTF-16 units and `len` says so; the call
        // writes at most that many (with the terminator) and sets `len` to the
        // units written without it.
        let ok = unsafe { GetUserNameExW(NameDisplay, Some(PWSTR(buffer.as_mut_ptr())), &mut len) };
        if !ok || len as usize > buffer.len() {
            return None;
        }
        buffer.truncate(len as usize);
        String::from_utf16(&buffer).ok()
    }

    /// The full name of the local (or Microsoft) account.
    fn local_full_name() -> Option<String> {
        let user = std::env::var("USERNAME").ok()?;
        let user = HSTRING::from(user.as_str());
        let mut buffer: *mut u8 = core::ptr::null_mut();
        // SAFETY: a null server name means this computer; `user` is a valid
        // NUL-terminated string for the call; on success `buffer` receives a
        // NetApi-allocated `USER_INFO_10`, freed below.
        let status = unsafe { NetUserGetInfo(PCWSTR::null(), &user, 10, &mut buffer) };
        if buffer.is_null() {
            return None;
        }
        let name = if status == 0 {
            // SAFETY: a successful level-10 call fills `buffer` with one
            // `USER_INFO_10`, valid until it is freed below.
            let info = unsafe { &*(buffer as *const USER_INFO_10) };
            if info.usri10_full_name.is_null() {
                None
            } else {
                // SAFETY: a NUL-terminated string inside the same buffer.
                unsafe { info.usri10_full_name.to_string() }.ok()
            }
        } else {
            None
        };
        // SAFETY: `buffer` was allocated by `NetUserGetInfo` and is freed
        // exactly once; `info` is not used after this.
        let _ = unsafe { NetApiBufferFree(Some(buffer as *const c_void)) };
        name
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_gecos_full_name_stops_at_the_first_comma() {
        assert_eq!(
            gecos_full_name("sam:x:1000:1000:Sam Taylor,,,:/home/sam:/bin/bash\n").as_deref(),
            Some("Sam Taylor")
        );
        assert_eq!(
            gecos_full_name("svc:x:999:999::/srv:/usr/sbin/nologin"),
            None
        );
        assert_eq!(gecos_full_name("broken"), None);
    }

    #[test]
    fn a_display_name_is_one_bounded_line_when_present() {
        // Whatever this machine reports, it is never empty or multi-line.
        if let Some(name) = technician_display_name() {
            assert!(!name.as_str().is_empty());
            assert!(!name.as_str().contains('\n'));
            assert!(
                name.as_str().chars().count() <= cretlink_protocol::limits::MAX_DISPLAY_NAME_CHARS
            );
        }
    }
}
