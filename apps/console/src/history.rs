//! The session history: summaries read back from the audit folder
//! (ST-M009-012, REQ-F-027, REQ-S-017).
//!
//! [`list_sessions`] lists the per-session audit files (ADR-0008) newest first
//! and [`read_session`] summarizes one: the target computer, when it started
//! and ended, who ended it, the actions per source, the files sent and
//! received, the failures, and the cleanup result. The technician's end report
//! and the Sessions page read these summaries.
//!
//! # Integrity first
//!
//! Every file's hash chain and seal are verified ([`verify_sealed`]) before it
//! is summarized. A broken file is summarized from its verified prefix only
//! and listed as [`Integrity::Broken`]; an unsealed one (an interrupted
//! session, or one still being written) as [`Integrity::Unsealed`]. Neither is
//! ever skipped, and neither is ever shown as a clean end: without its end
//! records a session's cleanup is unknown (REQ-N-015).
//!
//! # What the summary is built from
//!
//! - the header record (`session_opened`): the session id and the start;
//! - `target_identity`: the hostname and operating system the customer's
//!   consent bound;
//! - the action records, folded exactly like the live feed
//!   ([`TechnicianActivity`]), so the history and the live session count the
//!   same things;
//! - `session_ended`: who ended it and when;
//! - `cleanup_report`: the agent's teardown report, classified by the rule in
//!   [`cretlink_session::report`].
//!
//! Older files that lack the end records still list, with what they hold.

use std::collections::HashSet;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

use cretlink_session::{
    ActivityEntry, ActivityKind, ActivitySource, ActivityState, CleanupNote, CleanupResult,
    CleanupState, EndedBy,
};
use cretlink_shared::{
    AuditFileMeta, AuditRecord, AuditValue, ChainReport, RetentionPolicy, files_to_prune,
    parse_rfc3339_millis, verify_sealed,
};
use serde::{Deserialize, Serialize};

use crate::TechnicianActivity;
use crate::audit::{
    CLEANUP_REPORT_ACTION, SESSION_ENDED_ACTION, TARGET_IDENTITY_ACTION, cleanup_source,
};

/// Whether an audit file's chain and seal verified.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Integrity {
    /// The chain links and the seal counts every record.
    Verified,
    /// The chain links but the seal is missing: the session was interrupted,
    /// or is still being written.
    Unsealed,
    /// The chain or the seal is broken, or the file could not be read.
    Broken,
}

/// The actions of one session per source.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActionCounts {
    /// Through an AI client.
    pub ai: u32,
    /// In the technician's terminal.
    pub terminal: u32,
    /// Other technician actions.
    pub technician: u32,
}

/// One session of the history (`SessionSummary` in the frontend contract).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionSummary {
    /// The session id, as the console state names it.
    pub id: String,
    /// The audit file this summary was read from.
    pub file: String,
    /// Whether the file verified.
    pub integrity: Integrity,
    /// The target hostname, as the agent reported it at consent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hostname: Option<String>,
    /// The target operating system, as the agent reported it at consent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub os: Option<String>,
    /// When the session log was opened, RFC 3339 UTC.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at: Option<String>,
    /// When the session ended, or the last record of an unfinished file.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ended_at: Option<String>,
    /// From start to end.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
    /// Who ended the session, when the end was recorded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ended_by: Option<EndedBy>,
    /// The actions per source.
    pub actions: ActionCounts,
    /// Completed transfers that left the customer computer.
    pub files_sent: u32,
    /// Completed transfers to the customer computer.
    pub files_received: u32,
    /// Actions that failed, timed out, or ended with an unknown result.
    pub failures: u32,
    /// The operation of the first failure.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub first_failure: Option<String>,
    /// The cleanup result; unknown without the agent's report.
    pub cleanup: CleanupResult,
}

/// Lists the sessions in `dir` within `policy`, newest first.
///
/// Only regular `*.jsonl` files are read (a symbolic link is not followed).
/// Files the retention policy would prune are left out, as they are expired
/// (ADR-0008); every other file is listed, whatever its integrity. A missing
/// directory is an empty history.
#[must_use]
pub fn list_sessions(
    dir: &Path,
    policy: &RetentionPolicy,
    now_unix_millis: i64,
) -> Vec<SessionSummary> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut files: Vec<AuditFileMeta> = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if !name.ends_with(".jsonl") {
            continue;
        }
        // `DirEntry::file_type` does not follow symbolic links.
        if !entry.file_type().is_ok_and(|kind| kind.is_file()) {
            continue;
        }
        let Ok(meta) = entry.metadata() else { continue };
        let modified_unix_millis = meta
            .modified()
            .ok()
            .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
            .and_then(|since| i64::try_from(since.as_millis()).ok())
            .unwrap_or(0);
        files.push(AuditFileMeta {
            name,
            size_bytes: meta.len(),
            modified_unix_millis,
        });
    }
    let expired: HashSet<String> = files_to_prune(&files, now_unix_millis, policy)
        .into_iter()
        .collect();
    let mut names: Vec<String> = files
        .into_iter()
        .map(|file| file.name)
        .filter(|name| !expired.contains(name))
        .collect();
    // File names start with the compact UTC start time, so they sort by start.
    names.sort_unstable_by(|a, b| b.cmp(a));
    names
        .into_iter()
        .map(|name| read_session(&dir.join(name)))
        .collect()
}

/// Summarizes one audit file. Never fails: an unreadable or broken file is a
/// [`Integrity::Broken`] summary with what could be verified.
#[must_use]
pub fn read_session(path: &Path) -> SessionSummary {
    let mut fold = SummaryFold::default();
    let integrity = for_each_verified(path, |record| fold.record(record));
    fold.finish(path, integrity)
}

/// Verifies the chain and seal of the audit file at `path`, then hands each
/// record of its verified prefix to `each`, in order, and returns the file's
/// integrity. Only the lines before a break are trusted; an unreadable file
/// is broken and has no records.
pub(crate) fn for_each_verified(path: &Path, mut each: impl FnMut(&AuditRecord)) -> Integrity {
    let report = File::open(path)
        .ok()
        .and_then(|file| verify_sealed(BufReader::new(file)).ok());
    let (integrity, verified_lines) = match report {
        Some(ChainReport::Valid { records }) => (Integrity::Verified, records),
        Some(ChainReport::Unsealed { records }) => (Integrity::Unsealed, records),
        // Only the lines before the break are trusted.
        Some(ChainReport::Broken { line, .. }) => (Integrity::Broken, line),
        None => (Integrity::Broken, 0),
    };
    if verified_lines > 0
        && let Ok(file) = File::open(path)
    {
        let mut reader = BufReader::new(file);
        let mut line = Vec::new();
        for _ in 0..verified_lines {
            line.clear();
            match reader.read_until(b'\n', &mut line) {
                Ok(0) | Err(_) => break,
                Ok(_) => {}
            }
            let bytes = line.strip_suffix(b"\n").unwrap_or(&line);
            match serde_json::from_slice::<AuditRecord>(bytes) {
                Ok(record) => each(&record),
                Err(_) => break,
            }
        }
    }
    integrity
}

/// The running summary over one file's records.
#[derive(Default)]
struct SummaryFold {
    session: Option<String>,
    started: Option<(String, i64)>,
    last: Option<(String, i64)>,
    ended: Option<(String, i64)>,
    ended_by: Option<EndedBy>,
    hostname: Option<String>,
    os: Option<String>,
    cleanup: Option<CleanupResult>,
    activity: TechnicianActivity,
    seen: HashSet<String>,
    actions: ActionCounts,
    files_sent: u32,
    files_received: u32,
    failures: u32,
    first_failure: Option<String>,
}

impl SummaryFold {
    fn record(&mut self, record: &AuditRecord) {
        let at = parse_rfc3339_millis(&record.ts);
        let stamp = at.map(|millis| (record.ts.clone(), millis));
        if record.seq == 0 {
            self.session = Some(record.session.to_string());
            self.started.clone_from(&stamp);
        }
        if stamp.is_some() {
            self.last.clone_from(&stamp);
        }
        match record.action.as_str() {
            TARGET_IDENTITY_ACTION => {
                self.hostname = text(record, "hostname");
                self.os = text(record, "os_version");
            }
            SESSION_ENDED_ACTION => {
                self.ended_by = text(record, "ended_by").and_then(|name| EndedBy::parse(&name));
                self.ended.clone_from(&stamp);
            }
            CLEANUP_REPORT_ACTION => self.cleanup = Some(cleanup_of(record)),
            // The seal ends an unfinished end: its time stands for the end.
            "session_closed" => {
                if self.ended.is_none() {
                    self.ended.clone_from(&stamp);
                }
            }
            _ => {
                let now = at.unwrap_or_else(|| self.last.as_ref().map_or(0, |(_, ms)| *ms));
                if let Some(entry) = self.activity.apply_record(record, now) {
                    self.count(&entry);
                }
            }
        }
    }

    /// Counts an item the first time it is seen, and its outcome once it
    /// finishes (a completion is final).
    fn count(&mut self, entry: &ActivityEntry) {
        let ActivityEntry::Item { item } = entry else {
            return;
        };
        if self.seen.insert(item.id.clone()) {
            let counter = match item.source {
                ActivitySource::Ai => &mut self.actions.ai,
                ActivitySource::Terminal => &mut self.actions.terminal,
                ActivitySource::Technician => &mut self.actions.technician,
            };
            *counter += 1;
        }
        match &item.state {
            ActivityState::Running | ActivityState::Cancelled => {}
            ActivityState::Done { .. } => match item.kind {
                ActivityKind::FileSent => self.files_sent += 1,
                ActivityKind::FileReceived => self.files_received += 1,
                _ => {}
            },
            ActivityState::Failed { .. } | ActivityState::TimedOut => {
                self.failures += 1;
                if self.first_failure.is_none() {
                    self.first_failure = Some(item.operation.clone());
                }
            }
        }
    }

    fn finish(mut self, path: &Path, integrity: Integrity) -> SessionSummary {
        // An action still running when the records stop ended with the
        // session: a failure, never done (REQ-N-008).
        let end_millis = self
            .ended
            .as_ref()
            .or(self.last.as_ref())
            .map_or(0, |(_, ms)| *ms);
        for entry in self.activity.session_ended(end_millis) {
            self.count(&entry);
        }
        let ended = self.ended.clone().or_else(|| self.last.clone());
        let duration_ms = match (&self.started, &ended) {
            (Some((_, start)), Some((_, end))) => u64::try_from(end - start).ok(),
            _ => None,
        };
        let cleanup = self.cleanup.unwrap_or_else(|| {
            CleanupResult::unknown(Some(
                match integrity {
                    Integrity::Verified => "the audit file holds no cleanup report",
                    Integrity::Unsealed => {
                        "the audit file ends before the session's end was recorded"
                    }
                    Integrity::Broken => {
                        "the audit file is damaged; its cleanup record cannot be trusted"
                    }
                }
                .to_owned(),
            ))
        });
        SessionSummary {
            id: self
                .session
                .unwrap_or_else(|| session_of_file_name(path).unwrap_or_default()),
            file: path.display().to_string(),
            integrity,
            hostname: self.hostname,
            os: self.os,
            started_at: self.started.map(|(ts, _)| ts),
            ended_at: ended.map(|(ts, _)| ts),
            duration_ms,
            ended_by: self.ended_by,
            actions: self.actions,
            files_sent: self.files_sent,
            files_received: self.files_received,
            failures: self.failures,
            first_failure: self.first_failure,
            cleanup,
        }
    }
}

/// The cleanup result of a `cleanup_report` record.
fn cleanup_of(record: &AuditRecord) -> CleanupResult {
    let source = text(record, "source").unwrap_or_default();
    match source.as_str() {
        cleanup_source::AGENT => {
            let ok = matches!(record.args.0.get("ok"), Some(AuditValue::Bool(true)));
            let notes: Vec<String> = match record.args.0.get("notes") {
                Some(AuditValue::List(items)) => items.iter().map(ToString::to_string).collect(),
                _ => Vec::new(),
            };
            CleanupResult::of_notes(ok, &notes)
        }
        cleanup_source::NOT_NEEDED => CleanupResult {
            state: CleanupState::CleanedUp,
            residuals: 0,
            notes: text(record, "reason")
                .into_iter()
                .map(|text| CleanupNote { ok: true, text })
                .collect(),
        },
        // `missing`, or a source this console does not know: never success.
        _ => CleanupResult::unknown(text(record, "reason")),
    }
}

/// One argument's display text, on one line.
fn text(record: &AuditRecord, key: &str) -> Option<String> {
    record
        .args
        .0
        .get(key)
        .map(|value| value.to_string().replace(['\r', '\n'], " "))
}

/// The session id in an audit file name (`<UTC-start>-<session>.jsonl`).
fn session_of_file_name(path: &Path) -> Option<String> {
    let stem = path.file_name()?.to_str()?.strip_suffix(".jsonl")?;
    let (_, session) = stem.split_once('-')?;
    Some(session.to_owned())
}

/// The audit file of a listed session, if `id` is one of `summaries`.
#[must_use]
pub fn file_of(summaries: &[SessionSummary], id: &str) -> Option<PathBuf> {
    summaries
        .iter()
        .find(|summary| summary.id == id)
        .map(|summary| PathBuf::from(&summary.file))
}

#[cfg(test)]
mod tests;
