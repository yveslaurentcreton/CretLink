//! One session's audit entries, for the technician's Live tab **Audit**
//! (ST-M009-008, REQ-S-017).
//!
//! [`session_entries`] reads a session's audit file — the one being written
//! during a live session, or the sealed one after it — and renders each
//! record as an [`AuditEntryView`]: when, who, the record kind, the outcome,
//! and a one-line summary. Pairing, consent, cleanup and end records are
//! included, not only actions.
//!
//! The summary is built from what the record already holds, and the records
//! hold no secret by construction (REQ-S-017): an action reads as the
//! activity feed words it ([`TechnicianActivity`]), any other record as its
//! arguments rendered with the audit's own
//! [`AuditValue`](cretlink_shared::AuditValue) display, which shows a
//! redaction as `[REDACTED]` and bytes only as a digest. Hash-chain fields are
//! never shown, values are cut to a bounded length, and only the records the
//! chain verified are read.

use std::collections::VecDeque;
use std::path::Path;

use cretlink_session::ActivityEntry;
use cretlink_shared::{Actor, AuditRecord, Outcome, parse_rfc3339_millis};
use serde::Serialize;

use crate::TechnicianActivity;
use crate::history::for_each_verified;

/// The most entries returned: the newest ones.
pub const MAX_AUDIT_ENTRIES: usize = 500;
/// The longest summary, in characters.
pub const MAX_SUMMARY_CHARS: usize = 240;
/// The longest single value in a summary, in characters.
const MAX_VALUE_CHARS: usize = 120;

/// Who an audit record is attributed to, in the frontend's words.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AuditActorView {
    /// An AI client through the MCP bridge.
    Ai,
    /// The technician, in the console.
    Technician,
    /// The technician's interactive terminal.
    Terminal,
    /// The customer's computer (the support agent).
    Agent,
    /// CretLink itself: the log, the session lifecycle.
    System,
}

impl From<Actor> for AuditActorView {
    fn from(actor: Actor) -> Self {
        match actor {
            Actor::Mcp => Self::Ai,
            Actor::Console => Self::Technician,
            Actor::Terminal => Self::Terminal,
            Actor::Agent => Self::Agent,
            Actor::System => Self::System,
        }
    }
}

/// One audit record as the Audit tab shows it (`AuditEntryView` in the
/// frontend contract).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AuditEntryView {
    /// The record's sequence number in its file.
    pub seq: u64,
    /// When it was written, RFC 3339 UTC.
    pub at: String,
    /// Who it is attributed to.
    pub actor: AuditActorView,
    /// The record kind, e.g. `consent_granted` or `mcp_remote_exec`.
    pub action: String,
    /// Its outcome; an unknown one is never shown as a success.
    pub outcome: Outcome,
    /// One secret-free line about it.
    pub summary: String,
}

/// The audit entries of the session file at `path`, oldest first: the newest
/// [`MAX_AUDIT_ENTRIES`] of the records its chain verified. A missing or
/// unreadable file has none.
#[must_use]
pub fn session_entries(path: &Path) -> Vec<AuditEntryView> {
    let mut fold = TechnicianActivity::new();
    let mut entries = VecDeque::new();
    for_each_verified(path, |record| {
        if entries.len() == MAX_AUDIT_ENTRIES {
            entries.pop_front();
        }
        entries.push_back(view(record, &mut fold));
    });
    entries.into()
}

/// Renders one record; `fold` follows the actions so a completion reads like
/// its start.
fn view(record: &AuditRecord, fold: &mut TechnicianActivity) -> AuditEntryView {
    let at = parse_rfc3339_millis(&record.ts).unwrap_or(0);
    let summary = match fold.apply_record(record, at) {
        Some(ActivityEntry::Item { item }) => item.operation,
        _ => record
            .args
            .0
            .iter()
            .map(|(key, value)| format!("{key}: {}", cut(&value.to_string(), MAX_VALUE_CHARS)))
            .collect::<Vec<_>>()
            .join(" · "),
    };
    AuditEntryView {
        seq: record.seq,
        at: record.ts.clone(),
        actor: record.actor.into(),
        action: record.action.clone(),
        outcome: record.outcome,
        summary: cut(&one_line(&summary), MAX_SUMMARY_CHARS),
    }
}

/// Line breaks and other control characters become spaces.
fn one_line(text: &str) -> String {
    text.chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect()
}

/// At most `max` characters, the cut marked with an ellipsis.
fn cut(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_owned();
    }
    let mut kept: String = text.chars().take(max.saturating_sub(1)).collect();
    kept.push('…');
    kept
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use std::io::Write;

    use cretlink_protocol::SessionId;
    use cretlink_shared::{AuditArgs, AuditValue, AuditWriter, FixedClock, redact_no_digest};

    use super::*;

    fn write_log(path: &Path, records: &[(Actor, &str, AuditArgs, Outcome)], seal: bool) {
        let file = std::fs::File::create(path).unwrap();
        let mut writer =
            AuditWriter::new_session(file, SessionId::new(), FixedClock::from_unix_millis(0))
                .unwrap();
        for (actor, action, args, outcome) in records {
            writer
                .append(*actor, *action, args.clone(), *outcome)
                .unwrap();
        }
        if seal {
            writer.close().unwrap();
        }
    }

    fn temp(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "cretlink-audit-entries-{}-{}",
            std::process::id(),
            SessionId::new()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir.join(name)
    }

    #[test]
    fn every_record_is_listed_with_a_secret_free_line() {
        let path = temp("live.jsonl");
        write_log(
            &path,
            &[
                (
                    Actor::Console,
                    "pairing_started",
                    AuditArgs::new().with("code", redact_no_digest("approval code", 6)),
                    Outcome::Ok,
                ),
                (
                    Actor::Agent,
                    "consent_granted",
                    AuditArgs::new(),
                    Outcome::Ok,
                ),
                (
                    Actor::Mcp,
                    "mcp_remote_exec_start",
                    AuditArgs::new()
                        .with("call_id", AuditValue::Int(4))
                        .with("command", AuditValue::text("Get-PSDrive C\nGet-Date")),
                    Outcome::Ok,
                ),
                (
                    Actor::Mcp,
                    "mcp_remote_exec",
                    AuditArgs::new()
                        .with("call_id", AuditValue::Int(4))
                        .with("exit_code", AuditValue::Int(1)),
                    Outcome::Failed,
                ),
                (
                    Actor::System,
                    "cleanup_report",
                    AuditArgs::new().with("notes", AuditValue::text(&"n".repeat(500))),
                    Outcome::Unknown,
                ),
            ],
            true,
        );
        let entries = session_entries(&path);
        // The header and the seal are records too.
        assert_eq!(entries.first().unwrap().action, "session_opened");
        assert_eq!(entries.last().unwrap().action, "session_closed");
        let by_action = |action: &str| entries.iter().find(|e| e.action == action).unwrap();

        let pairing = by_action("pairing_started");
        assert_eq!(pairing.actor, AuditActorView::Technician);
        assert_eq!(pairing.summary, "code: [REDACTED]");

        let consent = by_action("consent_granted");
        assert_eq!(consent.actor, AuditActorView::Agent);

        let done = by_action("mcp_remote_exec");
        assert_eq!(done.actor, AuditActorView::Ai);
        assert_eq!(done.outcome, Outcome::Failed);
        assert!(
            done.summary.contains("Get-PSDrive C Get-Date"),
            "{}",
            done.summary
        );
        assert!(!done.summary.contains('\n'));

        let cleanup = by_action("cleanup_report");
        assert_eq!(cleanup.outcome, Outcome::Unknown);
        assert!(cleanup.summary.chars().count() <= MAX_SUMMARY_CHARS);
        assert!(cleanup.summary.ends_with('…'));

        for entry in &entries {
            assert!(!entry.summary.contains("prev"), "{entry:?}");
        }
        let json = serde_json::to_value(&entries[1]).unwrap();
        assert_eq!(json["actor"], "technician");
        assert_eq!(json["outcome"], "ok");
        assert!(json.get("at").is_some() && json.get("seq").is_some());
    }

    #[test]
    fn a_live_file_lists_its_verified_records_and_only_those() {
        let path = temp("open.jsonl");
        write_log(
            &path,
            &[(
                Actor::Agent,
                "consent_granted",
                AuditArgs::new(),
                Outcome::Ok,
            )],
            false,
        );
        assert_eq!(session_entries(&path).len(), 2);
        // A torn last line (the writer mid-record) is not shown.
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        file.write_all(b"{\"v\":1,\"seq\":2,").unwrap();
        assert_eq!(session_entries(&path).len(), 2);
        assert!(session_entries(&path.with_file_name("missing.jsonl")).is_empty());
    }

    #[test]
    fn only_the_newest_entries_are_returned() {
        let path = temp("long.jsonl");
        let records: Vec<_> = (0..MAX_AUDIT_ENTRIES + 10)
            .map(|_| {
                (
                    Actor::System,
                    "state_changed",
                    AuditArgs::new(),
                    Outcome::Ok,
                )
            })
            .collect();
        write_log(&path, &records, false);
        let entries = session_entries(&path);
        assert_eq!(entries.len(), MAX_AUDIT_ENTRIES);
        assert_eq!(
            entries.last().unwrap().seq,
            u64::try_from(MAX_AUDIT_ENTRIES + 10).unwrap()
        );
    }
}
