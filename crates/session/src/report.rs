//! Session end reports (ST-M009-012, REQ-F-027, REQ-F-019).
//!
//! Both surfaces end a session with a plain report: who ended it, what
//! happened, and the cleanup result with every failure and residual first.
//! This module holds the vocabulary both share — [`EndedBy`], the cleanup
//! classification ([`CleanupResult`]) — and the customer's report
//! ([`CustomerReport`]), which the customer side builds **in memory** from its
//! activity feed and the agent's own [`CleanupReport`]; nothing is written
//! unless the customer saves a copy ([`html`]). The technician's report is
//! read back from the console's audit file by `cretlink-console`.
//!
//! Every type serializes exactly to its namesake in the application's frontend
//! contract (`apps/app/src/shared/contract.ts`): camelCase fields, snake_case
//! enum values.
//!
//! # The cleanup rule
//!
//! The agent's teardown report is `{ ok, notes }`. The notes the agent writes
//! (see the session runtime's staging sweep) are of three kinds, told apart by
//! a fixed prefix that the agent itself uses ([`RESIDUAL_NOTE_PREFIX`],
//! [`RESIDUALS_NOT_LISTED_PREFIX`], [`TEARDOWN_FAILED_NOTE_PREFIX`]):
//!
//! - a **residual**: one staged artifact that could not be removed
//!   (`staged artifact not removed: <path> (<reason>)`), or a counted note for
//!   residuals that did not fit the report
//!   (`staged artifacts not removed and not listed: <n>`);
//! - a **teardown failure**: the staging sweep itself failed or answered
//!   something unexpected (`staging cleanup failed: <reason>`);
//! - anything else is **informational** (what was torn down).
//!
//! The result ([`CleanupResult::of_report`]) is then:
//!
//! - `failed` when any note is a teardown failure, or when the report is not
//!   ok without naming a residual (something went wrong that it does not
//!   name);
//! - `residuals` (with their count) when residual notes remain;
//! - `cleaned_up` only when the report is ok and no note is a residual or a
//!   failure;
//! - `unknown` when no report arrived at all ([`CleanupResult::unknown`]),
//!   which is never shown as success (REQ-N-015).

use serde::{Deserialize, Serialize};

use crate::activity::{ActivityEntry, ActivityKind, ActivityState};
use crate::{CleanupReport, RevokeReason};

pub mod html;

/// The prefix of a note naming one staged artifact the agent could not remove.
pub const RESIDUAL_NOTE_PREFIX: &str = "staged artifact not removed: ";

/// The prefix of the counted note for residuals that did not fit a bounded
/// report, followed by their number.
pub const RESIDUALS_NOT_LISTED_PREFIX: &str = "staged artifacts not removed and not listed: ";

/// The prefix of a note saying the staging sweep itself failed.
pub const TEARDOWN_FAILED_NOTE_PREFIX: &str = "staging cleanup failed: ";

/// Who ended a session, from the point of view of the surface reporting it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EndedBy {
    /// The technician stopped it (or reported that the codes did not match).
    Technician,
    /// The customer stopped it or declined consent.
    Customer,
    /// The connection was lost.
    ConnectionLost,
    /// A failure ended it: a timeout, a protocol violation, a revocation for an
    /// error or an expiry.
    Failure,
}

impl EndedBy {
    /// Who a revocation reason names. A revocation for an error or an expiry
    /// is a failure, not a person's decision.
    #[must_use]
    pub const fn of_revoke(reason: RevokeReason) -> Self {
        match reason {
            RevokeReason::TechnicianRequested => Self::Technician,
            RevokeReason::CustomerRequested => Self::Customer,
            RevokeReason::Expired | RevokeReason::Error => Self::Failure,
        }
    }

    /// The stable snake_case name, as serialized.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Technician => "technician",
            Self::Customer => "customer",
            Self::ConnectionLost => "connection_lost",
            Self::Failure => "failure",
        }
    }

    /// Parses the snake_case name.
    #[must_use]
    pub fn parse(name: &str) -> Option<Self> {
        [
            Self::Technician,
            Self::Customer,
            Self::ConnectionLost,
            Self::Failure,
        ]
        .into_iter()
        .find(|ended_by| ended_by.as_str() == name)
    }
}

/// What one cleanup note says, by the rule in the module documentation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoteKind {
    /// What was torn down.
    Informational,
    /// This many staged artifacts were left behind.
    Residual(u32),
    /// The staging sweep itself failed.
    TeardownFailed,
}

/// Classifies one cleanup note by its prefix.
#[must_use]
pub fn classify_note(note: &str) -> NoteKind {
    if note.starts_with(RESIDUAL_NOTE_PREFIX) {
        NoteKind::Residual(1)
    } else if let Some(rest) = note.strip_prefix(RESIDUALS_NOT_LISTED_PREFIX) {
        let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
        // An unreadable count still names at least one residual.
        NoteKind::Residual(digits.parse::<u32>().unwrap_or(1).max(1))
    } else if note.starts_with(TEARDOWN_FAILED_NOTE_PREFIX) {
        NoteKind::TeardownFailed
    } else {
        NoteKind::Informational
    }
}

/// The overall cleanup state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CleanupState {
    /// Everything session-scoped was torn down.
    CleanedUp,
    /// Staged artifacts were left behind.
    Residuals,
    /// Teardown failed.
    Failed,
    /// No cleanup report arrived; never shown as success.
    Unknown,
}

/// One cleanup note, marked ok when it is not a failure or a residual.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CleanupNote {
    /// `false` for a residual or a teardown failure.
    pub ok: bool,
    /// The note as the agent wrote it.
    pub text: String,
}

/// The cleanup result of a session, as both surfaces show it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CleanupResult {
    /// The overall state.
    pub state: CleanupState,
    /// How many staged artifacts were left behind.
    pub residuals: u32,
    /// Every note, failures and residuals first.
    pub notes: Vec<CleanupNote>,
}

impl CleanupResult {
    /// Classifies the agent's teardown report by the rule in the module
    /// documentation.
    #[must_use]
    pub fn of_report(report: &CleanupReport) -> Self {
        Self::of_notes(report.ok, &report.notes)
    }

    /// Classifies an `ok` flag and its notes (the form an audit record keeps).
    #[must_use]
    pub fn of_notes(ok: bool, notes: &[String]) -> Self {
        let mut residuals: u32 = 0;
        let mut teardown_failed = false;
        let mut failures = Vec::new();
        let mut rest = Vec::new();
        for note in notes {
            let kind = classify_note(note);
            match kind {
                NoteKind::Residual(count) => residuals = residuals.saturating_add(count),
                NoteKind::TeardownFailed => teardown_failed = true,
                NoteKind::Informational => {}
            }
            let entry = CleanupNote {
                ok: kind == NoteKind::Informational,
                text: note.clone(),
            };
            if entry.ok {
                rest.push(entry);
            } else {
                failures.push(entry);
            }
        }
        // Teardown failures before residuals, then what went well.
        failures.sort_by_key(|note| classify_note(&note.text) != NoteKind::TeardownFailed);
        failures.extend(rest);
        let state = if teardown_failed || (!ok && residuals == 0) {
            CleanupState::Failed
        } else if residuals > 0 {
            CleanupState::Residuals
        } else {
            CleanupState::CleanedUp
        };
        Self {
            state,
            residuals,
            notes: failures,
        }
    }

    /// No report arrived: the state is unknown, with an optional note saying
    /// why.
    #[must_use]
    pub fn unknown(note: Option<String>) -> Self {
        Self {
            state: CleanupState::Unknown,
            residuals: 0,
            notes: note
                .into_iter()
                .map(|text| CleanupNote { ok: false, text })
                .collect(),
        }
    }
}

/// Fits a teardown report to the wire bounds of `CleanupReported`
/// (`MAX_CLEANUP_NOTES`, `MAX_CLEANUP_NOTE_BYTES`, single lines), keeping its
/// meaning: failures and residuals come first, a long note is shortened in the
/// middle so its prefix and its end (the file name and the reason) stay, and
/// notes that do not fit are folded into one counted note, so no residual is
/// dropped silently (REQ-F-019).
#[must_use]
pub fn fit_for_wire(report: &CleanupReport) -> CleanupReport {
    use cretlink_protocol::limits::{MAX_CLEANUP_NOTE_BYTES, MAX_CLEANUP_NOTES};

    let mut notes: Vec<String> = report.notes.iter().map(|note| one_line(note)).collect();
    // Stable: teardown failures, then residuals, then the rest.
    notes.sort_by_key(|note| match classify_note(note) {
        NoteKind::TeardownFailed => 0,
        NoteKind::Residual(_) => 1,
        NoteKind::Informational => 2,
    });
    if notes.len() > MAX_CLEANUP_NOTES {
        let overflow = notes.split_off(MAX_CLEANUP_NOTES - 1);
        let mut residuals: u32 = 0;
        let mut others: usize = 0;
        for note in &overflow {
            match classify_note(note) {
                NoteKind::Residual(count) => residuals = residuals.saturating_add(count),
                NoteKind::TeardownFailed | NoteKind::Informational => others += 1,
            }
        }
        notes.push(if residuals > 0 {
            format!("{RESIDUALS_NOT_LISTED_PREFIX}{residuals} (and {others} other notes not sent)")
        } else {
            format!("{others} more cleanup notes not sent")
        });
    }
    CleanupReport {
        ok: report.ok,
        notes: notes
            .into_iter()
            .map(|note| shorten_middle(&note, MAX_CLEANUP_NOTE_BYTES))
            .collect(),
    }
}

/// Replaces control characters and line separators with spaces.
fn one_line(text: &str) -> String {
    text.chars()
        .map(|c| {
            if c.is_control() || c == '\u{2028}' || c == '\u{2029}' {
                ' '
            } else {
                c
            }
        })
        .collect()
}

/// Shortens `text` to at most `max` bytes by cutting its middle, on char
/// boundaries, keeping the start and the end.
fn shorten_middle(text: &str, max: usize) -> String {
    const MARK: &str = " … ";
    if text.len() <= max {
        return text.to_owned();
    }
    let budget = max.saturating_sub(MARK.len());
    let mut head = budget / 2;
    while head > 0 && !text.is_char_boundary(head) {
        head -= 1;
    }
    let mut tail = text.len() - (budget - head);
    while tail < text.len() && !text.is_char_boundary(tail) {
        tail += 1;
    }
    format!("{}{MARK}{}", &text[..head], &text[tail..])
}

/// What the customer's end report counts.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CustomerCounts {
    /// Commands and terminals run on this computer.
    pub commands: u32,
    /// Of those, how many failed, timed out or ended with an unknown result.
    pub failed_commands: u32,
    /// Files that left this computer (REQ-S-016).
    pub files_sent: u32,
    /// Files that arrived on this computer.
    pub files_received: u32,
    /// Pictures of the screen taken.
    pub screenshots: u32,
}

impl CustomerCounts {
    /// Counts the items of a feed. A transfer counts once it completed; a
    /// command counts whatever its outcome.
    #[must_use]
    pub fn of_feed(entries: &[ActivityEntry]) -> Self {
        let mut counts = Self::default();
        for entry in entries {
            let ActivityEntry::Item { item } = entry else {
                continue;
            };
            let done = matches!(item.state, ActivityState::Done { .. });
            match item.kind {
                ActivityKind::Command => {
                    counts.commands += 1;
                    if matches!(
                        item.state,
                        ActivityState::Failed { .. } | ActivityState::TimedOut
                    ) {
                        counts.failed_commands += 1;
                    }
                }
                ActivityKind::FileSent if done => counts.files_sent += 1,
                ActivityKind::FileReceived if done => counts.files_received += 1,
                ActivityKind::ScreenObserved if done => counts.screenshots += 1,
                _ => {}
            }
        }
        counts
    }
}

/// The customer's end report (`CustomerReport` in the frontend contract).
///
/// Built in memory from what the customer saw — the activity feed, the
/// agent's own teardown report, this computer's hostname and what the
/// technician's computer said its user is called — and never from a session
/// secret: no approval code, no key, no connection code (REQ-S-017). Nothing is
/// written unless the customer saves a copy (REQ-F-027).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CustomerReport {
    /// Who ended the session, from the customer's point of view; `None` when
    /// the session task did not report its end.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ended_by: Option<EndedBy>,
    /// The technician's display name as the technician's computer reported it
    /// (unverified, ST-M009-011).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub technician_name: Option<String>,
    /// This computer's hostname.
    pub hostname: String,
    /// Whether full control was granted at any point.
    pub control_granted: bool,
    /// When the session began (pairing completed), RFC 3339 UTC.
    pub started_at: String,
    /// When it ended, RFC 3339 UTC.
    pub ended_at: String,
    /// How long it lasted.
    pub duration_ms: u64,
    /// What happened.
    pub counts: CustomerCounts,
    /// The cleanup result, failures and residuals first.
    pub cleanup: CleanupResult,
    /// Every activity item the customer saw, oldest first.
    pub activity: Vec<ActivityEntry>,
}

/// The inputs of a [`CustomerReport`], gathered by the customer window during
/// the session.
#[derive(Debug, Clone)]
pub struct CustomerReportInput {
    /// Who ended it, from the session task.
    pub ended_by: Option<EndedBy>,
    /// The technician's display name from the pairing handshake.
    pub technician_name: Option<String>,
    /// This computer's hostname.
    pub hostname: String,
    /// Whether full control was granted.
    pub control_granted: bool,
    /// When pairing completed, Unix milliseconds.
    pub started_unix_millis: i64,
    /// When the session ended, Unix milliseconds.
    pub ended_unix_millis: i64,
    /// The agent's own teardown report; `None` when it never arrived.
    pub cleanup: Option<CleanupReport>,
    /// The activity feed, oldest first.
    pub activity: Vec<ActivityEntry>,
}

impl CustomerReport {
    /// Builds the report from what the customer saw.
    #[must_use]
    pub fn build(input: CustomerReportInput) -> Self {
        let cleanup = match &input.cleanup {
            Some(report) => CleanupResult::of_report(report),
            None => CleanupResult::unknown(Some(
                "CretLink could not confirm the cleanup on this computer".to_owned(),
            )),
        };
        let ended = input.ended_unix_millis.max(input.started_unix_millis);
        Self {
            ended_by: input.ended_by,
            technician_name: input.technician_name,
            hostname: input.hostname,
            control_granted: input.control_granted,
            started_at: cretlink_shared::format_rfc3339_millis(input.started_unix_millis),
            ended_at: cretlink_shared::format_rfc3339_millis(ended),
            duration_ms: u64::try_from(ended - input.started_unix_millis).unwrap_or(0),
            counts: CustomerCounts::of_feed(&input.activity),
            cleanup,
            activity: input.activity,
        }
    }

    /// The suggested file name of a saved copy: `YYYY-MM-DD <hostname>.html`,
    /// dated by the session's start (UTC), with characters that are not valid
    /// in a Windows file name replaced.
    #[must_use]
    pub fn file_name(&self) -> String {
        let date: String = self.started_at.chars().take(10).collect();
        let host: String = self
            .hostname
            .chars()
            .map(|c| {
                if c.is_control()
                    || matches!(c, '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*')
                {
                    '-'
                } else {
                    c
                }
            })
            .collect();
        let host = host.trim().trim_end_matches('.');
        let host = if host.is_empty() {
            "this computer"
        } else {
            host
        };
        format!("{date} {host}.html")
    }
}

#[cfg(test)]
mod tests;
