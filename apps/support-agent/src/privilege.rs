//! Truthful reporting of the agent's effective Windows identity and elevation
//! (REQ-S-011).
//!
//! REQ-S-011 forbids the agent from ever claiming an action ran elevated when
//! it did not. This module therefore queries the *actual* process token on
//! Windows and, whenever a query fails, degrades to an honest `Unknown` /
//! `elevated = false` with a secret-free note rather than guessing. On
//! non-Windows hosts (the macOS and Linux CI matrix) it returns a deterministic
//! "Windows-only" shape so the whole workspace still builds and tests.
//!
//! All `unsafe` FFI is confined to the `windows_token` submodule, which is
//! `#[cfg(windows)]` and annotated `#![allow(unsafe_code)]` per ADR-0001. The
//! RID-to-level mapping ([`integrity_from_rid`]) is a pure function, unit-tested
//! on every platform.

use core::fmt;

use serde::{Deserialize, Serialize};

#[cfg(windows)]
mod windows_token;

/// The Windows mandatory integrity level of the process, derived from the
/// integrity SID's relative identifier (RID).
///
/// `Unknown` is reported only when the level could not actually be observed (a
/// failed query, or a non-Windows host); it is never a real level dressed up as
/// a guess.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IntegrityLevel {
    /// Untrusted integrity.
    Untrusted,
    /// Low integrity.
    Low,
    /// Medium integrity (the default for a standard, non-elevated process).
    Medium,
    /// High integrity (typical of an elevated process).
    High,
    /// System integrity.
    System,
    /// The integrity level could not be determined.
    Unknown,
}

impl IntegrityLevel {
    /// A stable lowercase label, identical to the serde representation.
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Untrusted => "untrusted",
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::System => "system",
            Self::Unknown => "unknown",
        }
    }
}

impl fmt::Display for IntegrityLevel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

// Well-known Windows mandatory-integrity RID thresholds (`winnt.h`
// `SECURITY_MANDATORY_*_RID`). These are stable Windows ABI constants, declared
// here as documented literals rather than pulled from `windows`'s
// `Win32_System_SystemServices` so that [`integrity_from_rid`] stays a pure
// function that compiles and is unit-tested on every host, including the
// non-Windows CI machines.
const RID_LOW: u32 = 0x1000;
const RID_MEDIUM: u32 = 0x2000;
const RID_HIGH: u32 = 0x3000;
const RID_SYSTEM: u32 = 0x4000;

/// Maps a token integrity-level RID to an [`IntegrityLevel`].
///
/// Pure and platform-independent: the Windows token code extracts the RID and
/// calls this. It never returns [`IntegrityLevel::Unknown`] — an out-of-range
/// low RID maps to `Untrusted`, and anything at or above the system RID
/// (including protected-process levels) maps to `System`.
#[must_use]
pub fn integrity_from_rid(rid: u32) -> IntegrityLevel {
    if rid >= RID_SYSTEM {
        IntegrityLevel::System
    } else if rid >= RID_HIGH {
        IntegrityLevel::High
    } else if rid >= RID_MEDIUM {
        IntegrityLevel::Medium
    } else if rid >= RID_LOW {
        IntegrityLevel::Low
    } else {
        IntegrityLevel::Untrusted
    }
}

/// The agent's honestly-reported effective identity (REQ-S-011).
///
/// This is the process's own identity, shown before and during any session. It
/// is never, by itself, a claim of authority: [`elevated`](Self::elevated) is
/// `false` unless the operating-system token reports elevation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EffectiveIdentity {
    /// The account name the process runs as.
    pub username: String,
    /// The account's domain or machine name, when known.
    pub domain: Option<String>,
    /// Whether the process token is elevated. Never `true` unless the token
    /// actually reports elevation (REQ-S-011).
    pub elevated: bool,
    /// The process's mandatory integrity level.
    pub integrity: IntegrityLevel,
    /// Whether the identity was gathered on Windows, where it is meaningful.
    pub is_windows: bool,
    /// A short, secret-free note when something is degraded or unavailable.
    pub note: Option<String>,
}

/// Reports the current process's effective identity.
///
/// On Windows this queries the real process token; any failure degrades to an
/// honest `Unknown` / `elevated = false` with an explanatory note rather than a
/// guess (REQ-S-011). On other platforms it returns the deterministic
/// non-Windows fallback.
#[must_use]
pub fn current_identity() -> EffectiveIdentity {
    #[cfg(windows)]
    {
        windows_token::current_identity()
    }
    #[cfg(not(windows))]
    {
        non_windows_identity()
    }
}

/// The deterministic identity reported off-Windows, where the elevation and
/// integrity model does not apply.
#[cfg(not(windows))]
fn non_windows_identity() -> EffectiveIdentity {
    let username = std::env::var("USER")
        .or_else(|_| std::env::var("USERNAME"))
        .unwrap_or_else(|_| "unknown".to_string());
    EffectiveIdentity {
        username,
        domain: None,
        elevated: false,
        integrity: IntegrityLevel::Unknown,
        is_windows: false,
        note: Some("privilege reporting is Windows-only".to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rid_maps_to_every_integrity_level() {
        assert_eq!(integrity_from_rid(0x0000), IntegrityLevel::Untrusted);
        assert_eq!(integrity_from_rid(0x0FFF), IntegrityLevel::Untrusted);
        assert_eq!(integrity_from_rid(RID_LOW), IntegrityLevel::Low);
        assert_eq!(integrity_from_rid(0x1FFF), IntegrityLevel::Low);
        assert_eq!(integrity_from_rid(RID_MEDIUM), IntegrityLevel::Medium);
        // Medium-plus (0x2100) still buckets as Medium.
        assert_eq!(integrity_from_rid(0x2100), IntegrityLevel::Medium);
        assert_eq!(integrity_from_rid(RID_HIGH), IntegrityLevel::High);
        assert_eq!(integrity_from_rid(RID_SYSTEM), IntegrityLevel::System);
        // Protected-process (0x5000) buckets as System.
        assert_eq!(integrity_from_rid(0x5000), IntegrityLevel::System);
    }

    #[test]
    fn integrity_level_display_matches_serde_round_trip() {
        for level in [
            IntegrityLevel::Untrusted,
            IntegrityLevel::Low,
            IntegrityLevel::Medium,
            IntegrityLevel::High,
            IntegrityLevel::System,
            IntegrityLevel::Unknown,
        ] {
            let json = serde_json::to_string(&level).unwrap();
            // Display and the serde token are identical (bar the JSON quotes).
            assert_eq!(json, format!("\"{level}\""));
            let back: IntegrityLevel = serde_json::from_str(&json).unwrap();
            assert_eq!(level, back);
        }
    }

    #[test]
    fn effective_identity_serialises_round_trip() {
        let identity = current_identity();
        let json = serde_json::to_string(&identity).unwrap();
        let back: EffectiveIdentity = serde_json::from_str(&json).unwrap();
        assert_eq!(identity, back);
    }

    #[test]
    fn current_identity_is_platform_coherent_and_never_empty() {
        let identity = current_identity();
        // The account name is always populated, never an empty string.
        assert!(!identity.username.is_empty());

        #[cfg(windows)]
        assert!(identity.is_windows);

        #[cfg(not(windows))]
        {
            assert!(!identity.is_windows);
            assert!(!identity.elevated);
            assert_eq!(identity.integrity, IntegrityLevel::Unknown);
            assert!(identity.note.is_some());
        }
    }

    #[cfg(not(windows))]
    #[test]
    fn non_windows_fallback_has_the_documented_shape() {
        let identity = non_windows_identity();
        assert!(!identity.is_windows);
        assert!(!identity.elevated);
        assert_eq!(identity.integrity, IntegrityLevel::Unknown);
        assert_eq!(
            identity.note.as_deref(),
            Some("privilege reporting is Windows-only")
        );
    }
}
