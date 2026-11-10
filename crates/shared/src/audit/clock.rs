//! Injected wall-clock time and RFC 3339 formatting (ADR-0008).
//!
//! ADR-0008 fixes the on-disk timestamp representation (RFC 3339, UTC,
//! millisecond precision) but leaves the timestamp *library* open. To avoid
//! adding a new workspace dependency, this module formats RFC 3339 from Unix
//! milliseconds itself, using Howard Hinnant's well-established
//! `civil_from_days` calendar algorithm rather than inventing anything. Time is
//! injected through the [`Clock`] trait so tests are deterministic.

use core::fmt;
use std::time::{SystemTime, UNIX_EPOCH};

/// A source of wall-clock time, expressed as Unix milliseconds (UTC).
///
/// Injected into [`crate::audit::AuditWriter`] so the audit timestamps are a
/// pure function of the clock: production uses [`SystemClock`]; tests use
/// [`FixedClock`] so the golden hash chain is reproducible.
pub trait Clock: fmt::Debug {
    /// The current time as milliseconds since the Unix epoch (UTC). May be
    /// negative for instants before 1970, which the formatter still handles.
    fn now_unix_millis(&self) -> i64;
}

/// The real system clock. Reads `SystemTime::now()` at each call.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now_unix_millis(&self) -> i64 {
        match SystemTime::now().duration_since(UNIX_EPOCH) {
            // After the epoch: milliseconds since 1970, saturating rather than
            // wrapping if the platform clock is absurdly far in the future.
            Ok(dur) => i64::try_from(dur.as_millis()).unwrap_or(i64::MAX),
            // Before the epoch: `err` is how far `now` sits *before* 1970, so
            // negate it to a negative Unix-millis value.
            Err(err) => i64::try_from(err.duration().as_millis())
                .map(|ms| -ms)
                .unwrap_or(i64::MIN),
        }
    }
}

/// A fixed clock for deterministic tests. Always returns the same instant.
#[derive(Debug, Clone, Copy)]
pub struct FixedClock {
    unix_millis: i64,
}

impl FixedClock {
    /// Creates a clock pinned to `unix_millis` since the Unix epoch (UTC).
    #[must_use]
    pub const fn from_unix_millis(unix_millis: i64) -> Self {
        Self { unix_millis }
    }
}

impl Clock for FixedClock {
    fn now_unix_millis(&self) -> i64 {
        self.unix_millis
    }
}

/// Formats `unix_millis` as an RFC 3339 UTC timestamp with millisecond
/// precision, e.g. `2026-08-07T12:34:56.789Z`.
///
/// This is the exact on-disk representation ADR-0008 fixes for
/// [`crate::audit::AuditRecord::ts`]. The calendar conversion is Howard
/// Hinnant's `civil_from_days`, valid across the full proleptic Gregorian
/// range, so it is correct for any realistic session timestamp.
#[must_use]
pub fn format_rfc3339_millis(unix_millis: i64) -> String {
    // Split into whole seconds and the millisecond remainder using Euclidean
    // division so a negative instant still yields a millisecond in `0..1000`.
    let secs = unix_millis.div_euclid(1000);
    let millis = unix_millis.rem_euclid(1000);

    let days = secs.div_euclid(86_400);
    let secs_of_day = secs.rem_euclid(86_400);

    let hour = secs_of_day / 3600;
    let minute = (secs_of_day % 3600) / 60;
    let second = secs_of_day % 60;

    let (year, month, day) = civil_from_days(days);

    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}.{millis:03}Z")
}

/// Parses the exact form [`format_rfc3339_millis`] writes
/// (`YYYY-MM-DDTHH:MM:SS.mmmZ`) back into Unix milliseconds, or `None` for
/// anything else. Used to read audit timestamps back (ST-M009-012); it is not
/// a general RFC 3339 parser.
#[must_use]
pub fn parse_rfc3339_millis(text: &str) -> Option<i64> {
    let bytes = text.as_bytes();
    if bytes.len() != 24
        || bytes[4] != b'-'
        || bytes[7] != b'-'
        || bytes[10] != b'T'
        || bytes[13] != b':'
        || bytes[16] != b':'
        || bytes[19] != b'.'
        || bytes[23] != b'Z'
    {
        return None;
    }
    let number = |range: std::ops::Range<usize>| -> Option<i64> {
        let part = text.get(range)?;
        if !part.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        part.parse().ok()
    };
    let (year, month, day) = (number(0..4)?, number(5..7)?, number(8..10)?);
    let (hour, minute, second, millis) = (
        number(11..13)?,
        number(14..16)?,
        number(17..19)?,
        number(20..23)?,
    );
    if !(1..=12).contains(&month)
        || !(1..=31).contains(&day)
        || hour > 23
        || minute > 59
        || second > 59
    {
        return None;
    }
    let days = days_from_civil(year, month, day);
    Some(((days * 86_400 + hour * 3600 + minute * 60 + second) * 1000) + millis)
}

/// The inverse of [`civil_from_days`]: Howard Hinnant's public-domain
/// `days_from_civil` algorithm, in safe integer arithmetic.
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let yoe = year - era * 400; // [0, 399]
    let mp = if month > 2 { month - 3 } else { month + 9 }; // Mar=0..Feb=11
    let doy = (153 * mp + 2) / 5 + day - 1; // [0, 365]
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy; // [0, 146096]
    era * 146_097 + doe - 719_468
}

/// Formats `unix_millis` as a compact, filesystem-safe UTC timestamp,
/// e.g. `20260807T123456789Z`.
///
/// Used only for [`crate::audit::session_file_name`]: RFC 3339 contains colons,
/// which are illegal in Windows file names, so the file-name timestamp drops
/// the separators while staying lexically sortable and unambiguously UTC.
#[must_use]
pub(crate) fn format_compact_utc(unix_millis: i64) -> String {
    let secs = unix_millis.div_euclid(1000);
    let millis = unix_millis.rem_euclid(1000);
    let days = secs.div_euclid(86_400);
    let secs_of_day = secs.rem_euclid(86_400);
    let hour = secs_of_day / 3600;
    let minute = (secs_of_day % 3600) / 60;
    let second = secs_of_day % 60;
    let (year, month, day) = civil_from_days(days);
    format!("{year:04}{month:02}{day:02}T{hour:02}{minute:02}{second:02}{millis:03}Z")
}

/// Converts a count of days since 1970-01-01 into a `(year, month, day)`
/// proleptic Gregorian date.
///
/// This is Howard Hinnant's public-domain `civil_from_days` algorithm
/// (<http://howardhinnant.github.io/date_algorithms.html>), reproduced in safe
/// integer arithmetic. It is a standard calendar conversion, not invented
/// cryptography or date math.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    // Shift the epoch to 0000-03-01 so leap days fall at the end of the era.
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097; // day of era, [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // day of year (Mar-based), [0, 365]
    let mp = (5 * doy + 2) / 153; // month, Mar=0..Feb=11
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32; // [1, 12]
    let year = if month <= 2 { year + 1 } else { year };
    (year, month, day)
}
