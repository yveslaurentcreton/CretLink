//! Per-session file naming and whole-file retention (REQ-N-006, ADR-0008).
//!
//! Retention is deliberately *whole-file*: a session file is either kept intact
//! or deleted in its entirety. A file is never partially edited, because
//! rewriting lines would break the hash chain and defeat the append-only
//! honesty property. [`files_to_prune`] is a pure function so the pruning
//! decision is unit-testable without touching the filesystem; the caller
//! performs the deletions and must never pass the active session file.
//!
//! # Scope of the ADR-0008 bounds
//!
//! ADR-0008 (lines 57-59) sets three resource bounds for REQ-N-006. Two live
//! here: the 32 KiB per-field cap ([`crate::audit::MAX_FIELD_BYTES`], applied at
//! capture time by [`crate::audit::truncate_field`]) and the 1 GiB audit-
//! directory cap ([`DEFAULT_MAX_TOTAL_BYTES`], applied by [`files_to_prune`]).
//! The third — the [`DEFAULT_SESSION_SOFT_CAP_BYTES`] per-session soft cap, at
//! which the console warns once and elides payload bodies (an explicit
//! `elided` args marker, ADR-0008 Appendix A) while still recording every
//! action — is enforced by the console's audit drain (ST-M003-006), which owns
//! the live writer; only the constant lives in this pure library slice.

use cretlink_protocol::SessionId;

use crate::audit::clock::format_compact_utc;

/// Milliseconds in a day.
const MILLIS_PER_DAY: i64 = 86_400_000;

/// Default retention age (ADR-0008): session files are pruned 90 days after
/// they were last written.
pub const DEFAULT_RETENTION_DAYS: u32 = 90;

/// Default audit-directory size cap (ADR-0008): 1 GiB. When the total exceeds
/// this, the oldest closed sessions are pruned first.
pub const DEFAULT_MAX_TOTAL_BYTES: u64 = 1024 * 1024 * 1024;

/// Default per-session soft cap (ADR-0008): 256 MiB. Above this, the console's
/// audit drain warns once and replaces payload arguments with an explicit
/// `elided` marker while still recording every action (REQ-N-006, REQ-S-017:
/// actions are never dropped, only their payload detail is elided).
pub const DEFAULT_SESSION_SOFT_CAP_BYTES: u64 = 256 * 1024 * 1024;

/// Retention policy for the audit directory.
///
/// A file is pruned if it is older than `max_age_days` *or* if the directory
/// total exceeds `max_total_bytes` (oldest first). `max_age_days == 0` means
/// keep forever (age-based pruning disabled); `max_total_bytes == 0` disables
/// the size cap.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetentionPolicy {
    /// Maximum age in days before a file is pruned. `0` = keep forever.
    pub max_age_days: u32,
    /// Maximum total size of the audit directory in bytes. `0` = no cap.
    pub max_total_bytes: u64,
}

impl Default for RetentionPolicy {
    fn default() -> Self {
        Self {
            max_age_days: DEFAULT_RETENTION_DAYS,
            max_total_bytes: DEFAULT_MAX_TOTAL_BYTES,
        }
    }
}

/// Metadata about one closed session file, as seen by the pruner.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuditFileMeta {
    /// File name (not a full path), e.g. `20260807T120000000Z-<uuid>.jsonl`.
    pub name: String,
    /// File size in bytes.
    pub size_bytes: u64,
    /// Last-modified time as Unix milliseconds (UTC).
    pub modified_unix_millis: i64,
}

/// Builds the per-session file name `<UTC-timestamp>-<session-id>.jsonl`
/// (ADR-0008).
///
/// `start_unix_millis` is the session-start instant. The timestamp is rendered
/// in a compact, colon-free UTC form (`YYYYMMDDTHHMMSSmmmZ`) so the name is
/// valid on Windows — RFC 3339's colons are illegal in Windows file names —
/// while remaining lexically sortable by start time.
#[must_use]
pub fn session_file_name(start_unix_millis: i64, session: SessionId) -> String {
    format!(
        "{}-{}.jsonl",
        format_compact_utc(start_unix_millis),
        session
    )
}

/// Selects whole session files to delete, oldest-first, to satisfy `policy`
/// (REQ-N-006).
///
/// Pure and side-effect-free: it returns the file *names* to delete and leaves
/// the deletion to the caller. Age pruning selects every file older than the
/// age budget; size pruning then removes the oldest remaining files until the
/// directory total is within `max_total_bytes`. The active session file must be
/// excluded by the caller (an active session is never pruned).
#[must_use]
pub fn files_to_prune(
    existing: &[AuditFileMeta],
    now_unix_millis: i64,
    policy: &RetentionPolicy,
) -> Vec<String> {
    // Oldest first; break ties by name so the result is deterministic.
    let mut ordered: Vec<&AuditFileMeta> = existing.iter().collect();
    ordered.sort_by(|a, b| {
        a.modified_unix_millis
            .cmp(&b.modified_unix_millis)
            .then_with(|| a.name.cmp(&b.name))
    });

    let mut prune: Vec<String> = Vec::new();

    // Age-based pruning.
    if policy.max_age_days > 0 {
        let cutoff = now_unix_millis - i64::from(policy.max_age_days) * MILLIS_PER_DAY;
        for file in &ordered {
            if file.modified_unix_millis < cutoff {
                prune.push(file.name.clone());
            }
        }
    }

    // Size-based pruning over what age pruning left behind.
    if policy.max_total_bytes > 0 {
        let mut total: u64 = ordered
            .iter()
            .filter(|file| !prune.contains(&file.name))
            .map(|file| file.size_bytes)
            .sum();

        if total > policy.max_total_bytes {
            for file in &ordered {
                if total <= policy.max_total_bytes {
                    break;
                }
                if prune.contains(&file.name) {
                    continue;
                }
                prune.push(file.name.clone());
                total = total.saturating_sub(file.size_bytes);
            }
        }
    }

    prune
}
